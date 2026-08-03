use std::{
    cmp::Reverse,
    collections::{BTreeMap, BTreeSet, BinaryHeap},
};

use hex_core::{
    Axial, DistributionPreset, FrontSelectionError, HexMap, MovementConfig, ground_traversal,
    redistribution_targets, selected_front_edges,
};
use spacetimedb::{ReducerContext, Table};

use crate::rules::{
    MAX_SELECTION_CELLS, allocated_infantry_at_cell, cell_state, command_was_seen, config,
    coordinate_for_cell, core_cell, edge_runtime_limits, packet_key, require_running_player,
    route_to, state, terrain, write_receipt,
};
use crate::schema::{
    OrderKind, OrderStatus, ReceiptStatus, TransferDestination, TransferOrder, TransferSource,
    TransitPacket,
};
use crate::schema::{
    mobilization_policy, transfer_destination, transfer_order, transfer_source, transit_packet,
};

#[derive(Clone)]
struct PlannedLeg {
    source: u32,
    destination: u32,
    amount: u64,
    route: Vec<u32>,
}

struct PlannedDistribution {
    sources: Vec<u32>,
    demands: BTreeMap<u32, u64>,
    amount: u64,
}

#[derive(Clone, Debug)]
struct FrontRouteTree {
    /// Coordinate -> `(cost to front, assigned boundary coordinate, next coordinate)`.
    labels: BTreeMap<Axial, (u64, Axial, Axial)>,
}

impl FrontRouteTree {
    fn route_to_boundary(&self, source: Axial) -> Option<(Axial, Vec<Axial>)> {
        let &(_, boundary, _) = self.labels.get(&source)?;
        let mut route = vec![source];
        let mut current = source;
        while current != boundary {
            let &(_, _, next) = self.labels.get(&current)?;
            if next == current {
                return None;
            }
            current = next;
            route.push(current);
            if route.len() > self.labels.len() {
                return None;
            }
        }
        Some((boundary, route))
    }
}

fn receipt_result(
    ctx: &ReducerContext,
    player_id: u8,
    client_command_id: u64,
    command_name: &str,
    result: Result<Option<u64>, String>,
) -> Result<(), String> {
    match result {
        Ok(order_id) => write_receipt(
            ctx,
            player_id,
            client_command_id,
            command_name,
            ReceiptStatus::Accepted,
            order_id.unwrap_or(0),
            "command accepted",
        ),
        Err(message) => write_receipt(
            ctx,
            player_id,
            client_command_id,
            command_name,
            ReceiptStatus::Rejected,
            0,
            message,
        ),
    }
}

#[spacetimedb::reducer]
pub fn set_mobilization_target(
    ctx: &ReducerContext,
    client_command_id: u64,
    target_bps: u32,
) -> Result<(), String> {
    let player_id = require_running_player(ctx)?;
    if command_was_seen(ctx, player_id, client_command_id) {
        return Ok(());
    }
    let result = if target_bps > 10_000 {
        Err("mobilization target must be between 0 and 10000 basis points".into())
    } else {
        let mut policy = ctx
            .db
            .mobilization_policy()
            .player_id()
            .find(player_id)
            .ok_or_else(|| "mobilization policy is missing".to_string())?;
        policy.target_bps = target_bps;
        ctx.db.mobilization_policy().player_id().update(policy);
        Ok(None)
    };
    receipt_result(
        ctx,
        player_id,
        client_command_id,
        "set_mobilization_target",
        result,
    )
}

#[spacetimedb::reducer]
pub fn issue_transfer(
    ctx: &ReducerContext,
    client_command_id: u64,
    source_cells: Vec<u32>,
    destination_cells: Vec<u32>,
    infantry: u64,
) -> Result<(), String> {
    let player_id = require_running_player(ctx)?;
    if command_was_seen(ctx, player_id, client_command_id) {
        return Ok(());
    }
    let result = direct_destination_demands(ctx, player_id, &destination_cells, infantry).and_then(
        |demands| {
            create_order(
                ctx,
                player_id,
                client_command_id,
                OrderKind::Transfer,
                source_cells,
                demands,
                infantry,
                Axial::ZERO,
            )
            .map(Some)
        },
    );
    receipt_result(ctx, player_id, client_command_id, "issue_transfer", result)
}

/// Commits a selected, connected owned region toward its exact directional
/// boundary. Cells behind the boundary contribute infantry through routes that
/// remain inside the submitted selection until their final frontier edge.
#[spacetimedb::reducer]
pub fn issue_push_front(
    ctx: &ReducerContext,
    client_command_id: u64,
    selected_cells: Vec<u32>,
    direction_q: i32,
    direction_r: i32,
    commitment_bps: u32,
) -> Result<(), String> {
    let player_id = require_running_player(ctx)?;
    if command_was_seen(ctx, player_id, client_command_id) {
        return Ok(());
    }
    let direction = Axial::new(direction_q, direction_r);
    let result = plan_push_front(ctx, player_id, &selected_cells, direction, commitment_bps)
        .and_then(|(requested, legs)| {
            persist_order(
                ctx,
                player_id,
                client_command_id,
                OrderKind::PushFront,
                requested,
                direction,
                legs,
            )
            .map(Some)
        });
    receipt_result(
        ctx,
        player_id,
        client_command_id,
        "issue_push_front",
        result,
    )
}

#[spacetimedb::reducer]
pub fn issue_balance(
    ctx: &ReducerContext,
    client_command_id: u64,
    selected_cells: Vec<u32>,
) -> Result<(), String> {
    issue_distribution(
        ctx,
        client_command_id,
        selected_cells,
        OrderKind::Balance,
        DistributionPreset::Balance,
        Axial::ZERO,
        "issue_balance",
    )
}

#[spacetimedb::reducer]
pub fn issue_front_load(
    ctx: &ReducerContext,
    client_command_id: u64,
    selected_cells: Vec<u32>,
    orientation_q: i32,
    orientation_r: i32,
) -> Result<(), String> {
    let direction = Axial::new(orientation_q, orientation_r);
    issue_distribution(
        ctx,
        client_command_id,
        selected_cells,
        OrderKind::FrontLoad,
        DistributionPreset::front_load(direction),
        direction,
        "issue_front_load",
    )
}

fn issue_distribution(
    ctx: &ReducerContext,
    client_command_id: u64,
    selected_cells: Vec<u32>,
    kind: OrderKind,
    preset: DistributionPreset,
    orientation: Axial,
    command_name: &str,
) -> Result<(), String> {
    let player_id = require_running_player(ctx)?;
    if command_was_seen(ctx, player_id, client_command_id) {
        return Ok(());
    }
    let result = distribution_plan(ctx, player_id, &selected_cells, preset).and_then(|plan| {
        if plan.amount == 0 {
            Ok(None)
        } else {
            create_order(
                ctx,
                player_id,
                client_command_id,
                kind,
                plan.sources,
                plan.demands,
                plan.amount,
                orientation,
            )
            .map(Some)
        }
    });
    receipt_result(ctx, player_id, client_command_id, command_name, result)
}

#[spacetimedb::reducer]
pub fn cancel_transfer_order(
    ctx: &ReducerContext,
    client_command_id: u64,
    order_id: u64,
) -> Result<(), String> {
    let player_id = require_running_player(ctx)?;
    if command_was_seen(ctx, player_id, client_command_id) {
        return Ok(());
    }
    let result = cancel_order(ctx, player_id, order_id).map(|()| Some(order_id));
    receipt_result(
        ctx,
        player_id,
        client_command_id,
        "cancel_transfer_order",
        result,
    )
}

fn direct_destination_demands(
    ctx: &ReducerContext,
    player_id: u8,
    destinations: &[u32],
    requested: u64,
) -> Result<BTreeMap<u32, u64>, String> {
    if requested == 0 {
        return Err("transfer infantry must be greater than zero".into());
    }
    let destinations = unique_selection(destinations, "destination")?;
    let reservations = active_destination_reservations(ctx, player_id);

    let mut remaining = requested;
    let mut demands = BTreeMap::new();
    for destination in destinations {
        let terrain_row = terrain(ctx, destination)?;
        if !terrain_row.passable || !terrain_row.capturable {
            return Err(format!(
                "destination cell {destination} is not passable ground"
            ));
        }
        let cell = cell_state(ctx, destination)?;
        if cell.owner_player_id != player_id {
            return Err(format!(
                "destination cell {destination} is not owned by the player; use Push Front for conquest"
            ));
        }
        let capacity_before_reservations = cell.military_capacity.saturating_sub(cell.infantry);
        let capacity = capacity_before_reservations
            .saturating_sub(reservations.get(&destination).copied().unwrap_or(0));
        let demand = remaining.min(capacity);
        if demand > 0 {
            demands.insert(destination, demand);
            remaining -= demand;
        }
        if remaining == 0 {
            break;
        }
    }
    if demands.is_empty() {
        return Err("the selected destinations have no available military capacity".into());
    }
    Ok(demands)
}

fn plan_push_front(
    ctx: &ReducerContext,
    player_id: u8,
    selected_cells: &[u32],
    direction: Axial,
    commitment_bps: u32,
) -> Result<(u64, Vec<PlannedLeg>), String> {
    if !(1..=10_000).contains(&commitment_bps) {
        return Err("push commitment must be between 1 and 10000 basis points".into());
    }
    if !Axial::DIRECTIONS.contains(&direction) {
        return Err("push direction must be one of the six adjacent hex directions".into());
    }

    let selected_ids = unique_selection(selected_cells, "push source")?;
    let mut selected_map = HexMap::new();
    let mut coordinate_to_id = BTreeMap::new();
    for cell_id in selected_ids {
        let terrain_row = terrain(ctx, cell_id)?;
        let cell = core_cell(ctx, cell_id)?;
        if !terrain_row.passable || cell.owner != Some(u32::from(player_id)) {
            return Err(format!(
                "push source cell {cell_id} is not owned passable ground"
            ));
        }
        coordinate_to_id.insert(cell.coordinate, cell_id);
        selected_map.insert(cell);
    }
    let selected_coordinates = coordinate_to_id.keys().copied().collect::<BTreeSet<_>>();

    let match_config = config(ctx)?;
    let mut target_by_boundary = BTreeMap::<Axial, u32>::new();
    for (&source_coordinate, &source_id) in &coordinate_to_id {
        let target_coordinate = source_coordinate + direction;
        if selected_coordinates.contains(&target_coordinate) {
            continue;
        }
        let Some(target_id) =
            crate::rules::cell_id_for_coordinate(&match_config, target_coordinate)
        else {
            continue;
        };
        let target_terrain = terrain(ctx, target_id)?;
        let target_state = cell_state(ctx, target_id)?;
        if !target_terrain.passable
            || !target_terrain.capturable
            || target_state.owner_player_id == player_id
            || edge_runtime_limits(ctx, source_id, target_id)?.is_none()
        {
            continue;
        }
        target_by_boundary.insert(source_coordinate, target_id);
    }

    let edges = selected_front_edges(&selected_coordinates, direction, |source, _target| {
        target_by_boundary.contains_key(&source)
    })
    .map_err(front_selection_message)?;
    debug_assert_eq!(edges.len(), target_by_boundary.len());

    let movement = MovementConfig {
        max_elevation_step: u16::from(match_config.max_elevation_step),
        level_cost: 10,
        uphill_cost: 15,
        downhill_cost: 10,
    };
    let boundary_sources = edges
        .iter()
        .map(|edge| edge.source)
        .collect::<BTreeSet<_>>();
    let routes = front_route_tree(&selected_map, &boundary_sources, &movement);
    if routes.labels.len() != selected_coordinates.len() {
        return Err(
            "push selection is split by a cliff or another impassable internal edge".into(),
        );
    }

    let reservations = active_destination_reservations(ctx, player_id);
    let mut remaining_capacity = BTreeMap::<u32, u64>::new();
    for target_id in target_by_boundary.values().copied() {
        let target = cell_state(ctx, target_id)?;
        let capacity = target
            .military_capacity
            .saturating_sub(reservations.get(&target_id).copied().unwrap_or(0));
        remaining_capacity.insert(target_id, capacity);
    }

    let mut requested = 0_u64;
    let mut legs = Vec::new();
    let matching_push_allocations = active_push_allocations(ctx, player_id, direction);
    for (&source_coordinate, &source_id) in &coordinate_to_id {
        let source = cell_state(ctx, source_id)?;
        let (boundary, route_coordinates) = routes
            .route_to_boundary(source_coordinate)
            .ok_or_else(|| format!("push source cell {source_id} cannot reach the front"))?;
        let target_id = *target_by_boundary
            .get(&boundary)
            .ok_or_else(|| "push route ended at an unknown boundary".to_string())?;
        let allocated = allocated_infantry_at_cell(ctx, player_id, source_id);
        let matching_allocated = matching_push_allocations
            .get(&(source_id, target_id))
            .copied()
            .unwrap_or(0);
        let commitment = additional_commitment(
            source.infantry,
            allocated,
            matching_allocated,
            commitment_bps,
        );
        requested = requested
            .checked_add(commitment)
            .ok_or_else(|| "push requested infantry overflow".to_string())?;
        if commitment == 0 {
            continue;
        }

        let available_capacity = remaining_capacity
            .get_mut(&target_id)
            .ok_or_else(|| "push target capacity is missing".to_string())?;
        let amount = commitment.min(*available_capacity);
        if amount == 0 {
            continue;
        }
        *available_capacity -= amount;
        let mut route = route_coordinates
            .into_iter()
            .map(|coordinate| {
                coordinate_to_id
                    .get(&coordinate)
                    .copied()
                    .ok_or_else(|| "push route escaped the selected region".to_string())
            })
            .collect::<Result<Vec<_>, _>>()?;
        route.push(target_id);
        legs.push(PlannedLeg {
            source: source_id,
            destination: target_id,
            amount,
            route,
        });
    }

    if requested == 0 {
        return Err("the push selection has no uncommitted infantry at this commitment".into());
    }
    if legs.is_empty() {
        return Err("the push front has no unreserved destination capacity".into());
    }
    Ok((requested, legs))
}

fn front_selection_message(error: FrontSelectionError) -> String {
    match error {
        FrontSelectionError::EmptySelection => "push selection is empty",
        FrontSelectionError::DisconnectedSelection => "push selection must be six-connected",
        FrontSelectionError::InvalidDirection => {
            "push direction must be one of the six adjacent hex directions"
        }
        FrontSelectionError::NoEligibleFront => {
            "the selected region has no non-owned passable front in that direction"
        }
        FrontSelectionError::DisconnectedFront => {
            "the selected directional front is split into disconnected sections"
        }
    }
    .into()
}

fn front_route_tree(
    selected_map: &HexMap,
    boundary_sources: &BTreeSet<Axial>,
    movement: &MovementConfig,
) -> FrontRouteTree {
    let mut labels = BTreeMap::<Axial, (u64, Axial, Axial)>::new();
    let mut pending = BinaryHeap::<Reverse<(u64, Axial, Axial)>>::new();
    for &boundary in boundary_sources {
        if !selected_map.contains(boundary) {
            continue;
        }
        labels.insert(boundary, (0, boundary, boundary));
        pending.push(Reverse((0, boundary, boundary)));
    }

    while let Some(Reverse((cost, boundary, current))) = pending.pop() {
        if labels
            .get(&current)
            .is_none_or(|&(best_cost, best_boundary, _)| {
                (best_cost, best_boundary) != (cost, boundary)
            })
        {
            continue;
        }
        let Some(current_cell) = selected_map.get(current) else {
            continue;
        };
        let mut neighbors = current.neighbors();
        neighbors.sort_unstable();
        for neighbor in neighbors {
            let Some(neighbor_cell) = selected_map.get(neighbor) else {
                continue;
            };
            // Search backward from the front. The cost must therefore describe
            // the eventual forward step `neighbor -> current`.
            let Some(step) = ground_traversal(neighbor_cell, current_cell, movement) else {
                continue;
            };
            let candidate = (cost.saturating_add(u64::from(step.cost)), boundary, current);
            if labels
                .get(&neighbor)
                .is_none_or(|existing| candidate < *existing)
            {
                labels.insert(neighbor, candidate);
                pending.push(Reverse((candidate.0, candidate.1, neighbor)));
            }
        }
    }
    FrontRouteTree { labels }
}

fn distribution_plan(
    ctx: &ReducerContext,
    player_id: u8,
    selected_cells: &[u32],
    preset: DistributionPreset,
) -> Result<PlannedDistribution, String> {
    let selected = unique_selection(selected_cells, "redistribution")?;
    let mut map = HexMap::new();
    let mut by_coordinate = BTreeMap::new();
    let mut total = 0_u64;
    for cell_id in selected {
        let cell = core_cell(ctx, cell_id)?;
        if cell.owner != Some(u32::from(player_id)) {
            return Err(format!("redistribution cell {cell_id} is not owned"));
        }
        total = total
            .checked_add(cell.force())
            .ok_or_else(|| "redistribution strength overflow".to_string())?;
        by_coordinate.insert(cell.coordinate, cell_id);
        map.insert(cell);
    }
    let targets =
        redistribution_targets(&map, u32::from(player_id), map.coordinates(), total, preset)
            .map_err(|error| format!("invalid redistribution: {error:?}"))?;

    let mut sources = Vec::new();
    let mut demands = BTreeMap::new();
    let mut total_demand = 0_u64;
    let reservations = active_destination_reservations(ctx, player_id);
    for (coordinate, target) in targets.targets {
        let cell_id = by_coordinate[&coordinate];
        let current = cell_state(ctx, cell_id)?.infantry;
        if current > target {
            sources.push(cell_id);
        } else if target > current {
            let demand =
                (target - current).saturating_sub(reservations.get(&cell_id).copied().unwrap_or(0));
            if demand > 0 {
                demands.insert(cell_id, demand);
                total_demand = total_demand.saturating_add(demand);
            }
        }
    }
    Ok(PlannedDistribution {
        sources,
        demands,
        amount: total_demand,
    })
}

fn active_destination_reservations(
    ctx: &ReducerContext,
    player_id: u8,
) -> BTreeMap<u32, u64> {
    let mut reservations = BTreeMap::<u32, u64>::new();
    for destination in ctx.db.transfer_destination().iter() {
        let active = ctx
            .db
            .transfer_order()
            .order_id()
            .find(destination.order_id)
            .is_some_and(|order| {
                order.status == OrderStatus::Active && order.player_id == player_id
            });
        if active {
            *reservations.entry(destination.cell_id).or_default() += destination
                .target_infantry
                .saturating_sub(destination.received_infantry);
        }
    }
    reservations
}

fn active_push_allocations(
    ctx: &ReducerContext,
    player_id: u8,
    direction: Axial,
) -> BTreeMap<(u32, u32), u64> {
    let matching_orders = ctx
        .db
        .transfer_order()
        .iter()
        .filter(|order| {
            order.player_id == player_id
                && order.status == OrderStatus::Active
                && order.kind == OrderKind::PushFront
                && order.orientation_q == direction.q
                && order.orientation_r == direction.r
        })
        .map(|order| order.order_id)
        .collect::<BTreeSet<_>>();
    let mut allocations = BTreeMap::<(u32, u32), u64>::new();
    for packet in ctx.db.transit_packet().iter().filter(|packet| {
        packet.owner_player_id == player_id && matching_orders.contains(&packet.order_id)
    }) {
        *allocations
            .entry((packet.origin_cell, packet.destination_cell))
            .or_default() += packet.infantry;
    }
    allocations
}

fn additional_commitment(
    infantry: u64,
    total_allocated: u64,
    matching_allocated: u64,
    commitment_bps: u32,
) -> u64 {
    let desired = (u128::from(infantry) * u128::from(commitment_bps) / 10_000) as u64;
    desired
        .saturating_sub(matching_allocated)
        .min(infantry.saturating_sub(total_allocated))
}

fn unique_selection(cells: &[u32], label: &str) -> Result<BTreeSet<u32>, String> {
    if cells.is_empty() {
        return Err(format!("{label} selection is empty"));
    }
    if cells.len() > MAX_SELECTION_CELLS {
        return Err(format!(
            "{label} selection exceeds the {MAX_SELECTION_CELLS}-cell command limit"
        ));
    }
    Ok(cells.iter().copied().collect())
}

#[allow(clippy::too_many_arguments)]
fn create_order(
    ctx: &ReducerContext,
    player_id: u8,
    client_command_id: u64,
    kind: OrderKind,
    source_cells: Vec<u32>,
    mut destination_demands: BTreeMap<u32, u64>,
    requested: u64,
    orientation: Axial,
) -> Result<u64, String> {
    let sources = unique_selection(&source_cells, "source")?;
    if destination_demands.is_empty() {
        return Err("destination selection is empty".into());
    }
    if sources
        .iter()
        .any(|source| destination_demands.contains_key(source))
    {
        return Err("source and destination selections may not overlap".into());
    }

    let mut available_by_source = BTreeMap::new();
    for source in sources {
        let terrain_row = terrain(ctx, source)?;
        let cell = cell_state(ctx, source)?;
        if !terrain_row.passable || cell.owner_player_id != player_id {
            return Err(format!("source cell {source} is not owned passable ground"));
        }
        let allocated = allocated_infantry_at_cell(ctx, player_id, source);
        let available = cell.infantry.saturating_sub(allocated);
        if available > 0 {
            available_by_source.insert(source, available);
        }
    }
    if available_by_source.is_empty() {
        return Err("the selected sources have no uncommitted infantry".into());
    }

    let destination_coordinates = destination_demands
        .keys()
        .copied()
        .map(|cell_id| Ok((cell_id, coordinate_for_cell(ctx, cell_id)?)))
        .collect::<Result<BTreeMap<_, _>, String>>()?;
    let mut legs = Vec::new();
    let mut remaining = requested;

    for (&source, source_available) in &mut available_by_source {
        if remaining == 0 {
            break;
        }
        let source_coordinate = coordinate_for_cell(ctx, source)?;
        let mut candidates: Vec<_> = destination_coordinates
            .iter()
            .filter(|(destination, _)| {
                destination_demands.get(destination).copied().unwrap_or(0) > 0
            })
            .map(|(&destination, &coordinate)| {
                (source_coordinate.distance(coordinate), destination)
            })
            .collect();
        candidates.sort_unstable();

        for (_, destination) in candidates {
            if *source_available == 0 || remaining == 0 {
                break;
            }
            let demand = destination_demands.get(&destination).copied().unwrap_or(0);
            if demand == 0 {
                continue;
            }
            let Some((route, _cost)) = route_to(ctx, player_id, source, destination)? else {
                continue;
            };
            if route.len() < 2 {
                continue;
            }
            let amount = (*source_available).min(demand).min(remaining);
            *source_available -= amount;
            *destination_demands
                .get_mut(&destination)
                .expect("candidate destination exists") -= amount;
            remaining -= amount;
            legs.push(PlannedLeg {
                source,
                destination,
                amount,
                route,
            });
        }
    }
    if requested == remaining {
        return Err("no selected source can route to a selected destination".into());
    }

    persist_order(
        ctx,
        player_id,
        client_command_id,
        kind,
        requested,
        orientation,
        legs,
    )
}

#[allow(clippy::too_many_arguments)]
fn persist_order(
    ctx: &ReducerContext,
    player_id: u8,
    client_command_id: u64,
    kind: OrderKind,
    requested: u64,
    orientation: Axial,
    legs: Vec<PlannedLeg>,
) -> Result<u64, String> {
    let committed = legs.iter().try_fold(0_u64, |total, leg| {
        total
            .checked_add(leg.amount)
            .ok_or_else(|| "order committed infantry overflow".to_string())
    })?;
    if committed == 0 {
        return Err("order has no committed infantry".into());
    }

    let logical_step = state(ctx)?.logical_step;
    let order = ctx.db.transfer_order().insert(TransferOrder {
        order_id: 0,
        player_id,
        client_command_id,
        kind,
        status: OrderStatus::Active,
        requested_infantry: requested,
        committed_infantry: committed,
        in_transit_infantry: committed,
        delivered_infantry: 0,
        casualty_infantry: 0,
        orientation_q: orientation.q,
        orientation_r: orientation.r,
        created_step: logical_step,
        updated_step: logical_step,
    });

    let mut source_totals = BTreeMap::<u32, u64>::new();
    let mut destination_totals = BTreeMap::<u32, u64>::new();
    for leg in legs {
        *source_totals.entry(leg.source).or_default() += leg.amount;
        *destination_totals.entry(leg.destination).or_default() += leg.amount;
        let key = packet_key(order.order_id, leg.source, leg.destination, leg.source, 0);
        ctx.db.transit_packet().insert(TransitPacket {
            packet_key: key,
            order_id: order.order_id,
            owner_player_id: player_id,
            origin_cell: leg.source,
            current_cell: leg.source,
            destination_cell: leg.destination,
            infantry: leg.amount,
            route_index: 0,
            route: leg.route,
            updated_step: logical_step,
        });
    }
    for (cell_id, infantry) in source_totals {
        ctx.db.transfer_source().insert(TransferSource {
            source_key: format!("{}:{cell_id}", order.order_id),
            order_id: order.order_id,
            cell_id,
            committed_infantry: infantry,
            queued_infantry: infantry,
        });
    }
    for (cell_id, infantry) in destination_totals {
        ctx.db.transfer_destination().insert(TransferDestination {
            destination_key: format!("{}:{cell_id}", order.order_id),
            order_id: order.order_id,
            cell_id,
            target_infantry: infantry,
            received_infantry: 0,
        });
    }
    Ok(order.order_id)
}

fn cancel_order(ctx: &ReducerContext, player_id: u8, order_id: u64) -> Result<(), String> {
    let mut order = ctx
        .db
        .transfer_order()
        .order_id()
        .find(order_id)
        .ok_or_else(|| format!("unknown transfer order {order_id}"))?;
    if order.player_id != player_id {
        return Err("the transfer order belongs to the other player".into());
    }
    if order.status != OrderStatus::Active {
        return Err("the transfer order is no longer active".into());
    }
    let packet_keys: Vec<_> = ctx
        .db
        .transit_packet()
        .packet_by_order()
        .filter(order_id)
        .map(|packet| packet.packet_key)
        .collect();
    for key in packet_keys {
        ctx.db.transit_packet().packet_key().delete(key);
    }
    order.status = OrderStatus::Cancelled;
    order.in_transit_infantry = 0;
    order.updated_step = state(ctx)?.logical_step;
    ctx.db.transfer_order().order_id().update(order);
    let source_keys: Vec<_> = ctx
        .db
        .transfer_source()
        .source_by_order()
        .filter(order_id)
        .map(|source| source.source_key)
        .collect();
    for source_key in source_keys {
        if let Some(mut source) = ctx.db.transfer_source().source_key().find(&source_key) {
            source.queued_infantry = 0;
            ctx.db.transfer_source().source_key().update(source);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use hex_core::Cell;

    fn selected_cell(coordinate: Axial, elevation: i16) -> Cell {
        Cell::ground(coordinate, elevation, Some(1), 100)
    }

    #[test]
    fn front_route_tree_stays_inside_selection_until_the_boundary() {
        let mut map = HexMap::new();
        for q in 0..=3 {
            map.insert(selected_cell(Axial::new(q, 0), 0));
        }
        let boundary = Axial::new(3, 0);
        let routes = front_route_tree(
            &map,
            &BTreeSet::from([boundary]),
            &MovementConfig::default(),
        );

        assert_eq!(
            routes.route_to_boundary(Axial::ZERO),
            Some((
                boundary,
                vec![
                    Axial::new(0, 0),
                    Axial::new(1, 0),
                    Axial::new(2, 0),
                    Axial::new(3, 0),
                ],
            ))
        );
    }

    #[test]
    fn front_route_tree_uses_stable_boundary_ties() {
        let origin = Axial::ZERO;
        let first = Axial::new(0, 1);
        let second = Axial::new(1, 0);
        let mut map = HexMap::new();
        for coordinate in [origin, first, second] {
            map.insert(selected_cell(coordinate, 0));
        }
        let routes = front_route_tree(
            &map,
            &BTreeSet::from([second, first]),
            &MovementConfig::default(),
        );

        assert_eq!(
            routes.route_to_boundary(origin),
            Some((first, vec![origin, first]))
        );
    }

    #[test]
    fn front_route_tree_does_not_cross_an_internal_cliff() {
        let rear = Axial::ZERO;
        let boundary = Axial::new(1, 0);
        let mut map = HexMap::new();
        map.insert(selected_cell(rear, 0));
        map.insert(selected_cell(boundary, 2));
        let routes = front_route_tree(
            &map,
            &BTreeSet::from([boundary]),
            &MovementConfig::default(),
        );

        assert!(routes.route_to_boundary(rear).is_none());
        assert_eq!(
            routes.route_to_boundary(boundary),
            Some((boundary, vec![boundary]))
        );
    }

    #[test]
    fn repeated_push_only_fills_the_remaining_commitment_target() {
        assert_eq!(additional_commitment(100, 0, 0, 5_000), 50);
        assert_eq!(additional_commitment(100, 20, 20, 5_000), 30);
        assert_eq!(additional_commitment(100, 50, 50, 5_000), 0);
        assert_eq!(additional_commitment(100, 80, 50, 5_000), 0);
    }

    #[test]
    fn unrelated_allocations_limit_supply_without_satisfying_the_push_target() {
        assert_eq!(additional_commitment(100, 50, 0, 5_000), 50);
        assert_eq!(additional_commitment(100, 70, 0, 5_000), 30);
        assert_eq!(additional_commitment(100, 70, 20, 5_000), 30);
    }
}
