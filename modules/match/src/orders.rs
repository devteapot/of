use std::{
    cmp::Reverse,
    collections::{BTreeMap, BTreeSet, BinaryHeap, VecDeque},
};

use hex_core::{
    Axial, DistributionPreset, FrontSelectionError, HexMap, MovementConfig, ground_traversal,
    redistribution_targets_with_commitment, selected_all_front_edges, selected_front_edges,
};
use spacetimedb::{ReducerContext, Table};

use crate::rules::{
    MAX_SELECTION_CELLS, allocated_infantry_at_cell, cell_state, command_was_seen, config,
    coordinate_for_cell, core_cell, edge_runtime_limits, packet_key, require_running_player,
    route_to, state, terrain, write_receipt,
};
use crate::schema::{
    EXPANSION_AGGREGATE_ORIGIN, ExpansionWave, NEUTRAL_PLAYER, OrderKind, OrderStatus,
    ReceiptStatus, TransferDestination, TransferOrder, TransferSource, TransitPacket,
};
use crate::schema::{
    expansion_wave, mobilization_policy, transfer_destination, transfer_order, transfer_source,
    transit_packet,
};

#[derive(Clone)]
struct PlannedLeg {
    source: u32,
    destination: u32,
    amount: u64,
    route: Vec<u32>,
}

struct PlannedDistribution {
    /// Maximum strength each source may contribute without crossing the
    /// percentage-aware target's frozen per-cell lower bound.
    source_limits: BTreeMap<u32, u64>,
    demands: BTreeMap<u32, u64>,
    amount: u64,
}

struct PlannedExpansion {
    selected_cells: Vec<u32>,
    seed_depths: Vec<u16>,
    outside_depths: Vec<u16>,
    commitments: BTreeMap<u32, u64>,
    requested: u64,
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

/// Commits one fixed share of every selected cell's currently unallocated
/// infantry to a branching perimeter wave around the selected region.
#[spacetimedb::reducer]
pub fn issue_expand_all(
    ctx: &ReducerContext,
    client_command_id: u64,
    selected_cells: Vec<u32>,
    commitment_bps: u32,
) -> Result<(), String> {
    let player_id = require_running_player(ctx)?;
    if command_was_seen(ctx, player_id, client_command_id) {
        return Ok(());
    }
    let result = plan_expand_all(ctx, player_id, &selected_cells, commitment_bps)
        .and_then(|plan| persist_expand_order(ctx, player_id, client_command_id, plan).map(Some));
    receipt_result(
        ctx,
        player_id,
        client_command_id,
        "issue_expand_all",
        result,
    )
}

#[spacetimedb::reducer]
pub fn issue_balance(
    ctx: &ReducerContext,
    client_command_id: u64,
    selected_cells: Vec<u32>,
    amount_bps: u32,
) -> Result<(), String> {
    issue_distribution(
        ctx,
        client_command_id,
        selected_cells,
        amount_bps,
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
    amount_bps: u32,
) -> Result<(), String> {
    let direction = Axial::new(orientation_q, orientation_r);
    issue_distribution(
        ctx,
        client_command_id,
        selected_cells,
        amount_bps,
        OrderKind::FrontLoad,
        DistributionPreset::front_load(direction),
        direction,
        "issue_front_load",
    )
}

#[spacetimedb::reducer]
pub fn issue_core_load(
    ctx: &ReducerContext,
    client_command_id: u64,
    selected_cells: Vec<u32>,
    amount_bps: u32,
) -> Result<(), String> {
    issue_distribution(
        ctx,
        client_command_id,
        selected_cells,
        amount_bps,
        OrderKind::CoreLoad,
        DistributionPreset::CoreLoad,
        Axial::ZERO,
        "issue_core_load",
    )
}

#[spacetimedb::reducer]
pub fn issue_perimeter_load(
    ctx: &ReducerContext,
    client_command_id: u64,
    selected_cells: Vec<u32>,
    amount_bps: u32,
) -> Result<(), String> {
    issue_distribution(
        ctx,
        client_command_id,
        selected_cells,
        amount_bps,
        OrderKind::PerimeterLoad,
        DistributionPreset::PerimeterLoad,
        Axial::ZERO,
        "issue_perimeter_load",
    )
}

#[allow(clippy::too_many_arguments)]
fn issue_distribution(
    ctx: &ReducerContext,
    client_command_id: u64,
    selected_cells: Vec<u32>,
    amount_bps: u32,
    kind: OrderKind,
    preset: DistributionPreset,
    orientation: Axial,
    command_name: &str,
) -> Result<(), String> {
    let player_id = require_running_player(ctx)?;
    if command_was_seen(ctx, player_id, client_command_id) {
        return Ok(());
    }
    let result =
        distribution_plan(ctx, player_id, &selected_cells, preset, amount_bps).and_then(|plan| {
            if plan.amount == 0 {
                Ok(None)
            } else {
                create_order(
                    ctx,
                    player_id,
                    client_command_id,
                    kind,
                    plan.source_limits,
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
pub fn cancel_push_fronts(
    ctx: &ReducerContext,
    client_command_id: u64,
    selected_cells: Vec<u32>,
    direction_q: i32,
    direction_r: i32,
) -> Result<(), String> {
    let player_id = require_running_player(ctx)?;
    if command_was_seen(ctx, player_id, client_command_id) {
        return Ok(());
    }
    let direction = Axial::new(direction_q, direction_r);
    let result = cancel_matching_pushes(ctx, player_id, &selected_cells, direction);
    receipt_result(
        ctx,
        player_id,
        client_command_id,
        "cancel_push_fronts",
        result,
    )
}

#[spacetimedb::reducer]
pub fn cancel_expand_all(
    ctx: &ReducerContext,
    client_command_id: u64,
    selected_cells: Vec<u32>,
) -> Result<(), String> {
    let player_id = require_running_player(ctx)?;
    if command_was_seen(ctx, player_id, client_command_id) {
        return Ok(());
    }
    let result = cancel_matching_expand_all(ctx, player_id, &selected_cells);
    receipt_result(
        ctx,
        player_id,
        client_command_id,
        "cancel_expand_all",
        result,
    )
}

fn plan_push_front(
    ctx: &ReducerContext,
    player_id: u8,
    selected_cells: &[u32],
    direction: Axial,
    commitment_bps: u32,
) -> Result<(u64, Vec<PlannedLeg>), String> {
    validate_basis_points(commitment_bps, "push commitment")?;
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

    let mut requested = 0_u64;
    let mut legs = Vec::new();
    for (&source_coordinate, &source_id) in &coordinate_to_id {
        let source = cell_state(ctx, source_id)?;
        let (boundary, route_coordinates) = routes
            .route_to_boundary(source_coordinate)
            .ok_or_else(|| format!("push source cell {source_id} cannot reach the front"))?;
        let target_id = *target_by_boundary
            .get(&boundary)
            .ok_or_else(|| "push route ended at an unknown boundary".to_string())?;
        let allocated = allocated_infantry_at_cell(ctx, player_id, source_id);
        let available = source.infantry.saturating_sub(allocated);
        let commitment = basis_point_share(available, commitment_bps);
        requested = requested
            .checked_add(commitment)
            .ok_or_else(|| "push requested infantry overflow".to_string())?;
        if commitment == 0 {
            continue;
        }

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
            amount: commitment,
            route,
        });
    }

    if requested == 0 {
        return Err("the push selection has no uncommitted infantry at this commitment".into());
    }
    if legs.is_empty() {
        return Err("the push front has no committed infantry".into());
    }
    Ok((requested, legs))
}

fn plan_expand_all(
    ctx: &ReducerContext,
    player_id: u8,
    selected_cells: &[u32],
    commitment_bps: u32,
) -> Result<PlannedExpansion, String> {
    validate_basis_points(commitment_bps, "expand commitment")?;

    let selected_ids = unique_selection(selected_cells, "expand source")?;
    let mut coordinate_to_id = BTreeMap::new();
    let mut commitments = BTreeMap::new();
    let mut requested = 0_u64;
    for cell_id in selected_ids.iter().copied() {
        let terrain_row = terrain(ctx, cell_id)?;
        let cell = core_cell(ctx, cell_id)?;
        if !terrain_row.passable || cell.owner != Some(u32::from(player_id)) {
            return Err(format!(
                "expand source cell {cell_id} is not owned passable ground"
            ));
        }
        coordinate_to_id.insert(cell.coordinate, cell_id);
        let allocated = allocated_infantry_at_cell(ctx, player_id, cell_id);
        let commitment = basis_point_share(cell.force().saturating_sub(allocated), commitment_bps);
        requested = requested
            .checked_add(commitment)
            .ok_or_else(|| "expand requested infantry overflow".to_string())?;
        commitments.insert(cell_id, commitment);
    }
    let selected_coordinates = coordinate_to_id.keys().copied().collect::<BTreeSet<_>>();
    let match_config = config(ctx)?;

    let mut eligible_targets = BTreeMap::<(Axial, Axial), u32>::new();
    for (&source_coordinate, &source_id) in &coordinate_to_id {
        for direction in Axial::DIRECTIONS {
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
            if expand_target_is_eligible(
                target_state.owner_player_id,
                target_terrain.passable,
                target_terrain.capturable,
                edge_runtime_limits(ctx, source_id, target_id)?.is_some(),
            ) {
                eligible_targets.insert((source_coordinate, target_coordinate), target_id);
            }
        }
    }

    let edges = selected_all_front_edges(&selected_coordinates, |source, target| {
        eligible_targets.contains_key(&(source, target))
    })
    .map_err(expand_selection_message)?;
    let boundary_cells = edges
        .iter()
        .map(|edge| coordinate_to_id[&edge.source])
        .collect::<BTreeSet<_>>();
    let first_ring = edges
        .iter()
        .map(|edge| eligible_targets[&(edge.source, edge.target)])
        .collect::<BTreeSet<_>>();
    let seed_depth_by_id =
        seed_inward_depths(ctx, &coordinate_to_id, &boundary_cells, &selected_ids)?;

    let cell_count = usize::from(match_config.map_width)
        .checked_mul(usize::from(match_config.map_height))
        .ok_or_else(|| "map cell count overflow".to_string())?;
    let outside_depths = outside_wave_depths(
        ctx,
        &match_config,
        player_id,
        &selected_ids,
        &first_ring,
        cell_count,
    )?;

    if requested == 0 {
        return Err("the expand selection has no uncommitted infantry at this commitment".into());
    }
    let selected_cells = selected_ids.into_iter().collect::<Vec<_>>();
    let seed_depths = selected_cells
        .iter()
        .map(|cell_id| seed_depth_by_id[cell_id])
        .collect();
    Ok(PlannedExpansion {
        selected_cells,
        seed_depths,
        outside_depths,
        commitments,
        requested,
    })
}

fn seed_inward_depths(
    ctx: &ReducerContext,
    coordinate_to_id: &BTreeMap<Axial, u32>,
    boundary_cells: &BTreeSet<u32>,
    selected_ids: &BTreeSet<u32>,
) -> Result<BTreeMap<u32, u16>, String> {
    let mut depths = BTreeMap::new();
    let mut pending = VecDeque::new();
    for &cell_id in boundary_cells {
        depths.insert(cell_id, 0_u16);
        pending.push_back(cell_id);
    }
    while let Some(current_id) = pending.pop_front() {
        let current_depth = depths[&current_id];
        let current_coordinate = coordinate_for_cell(ctx, current_id)?;
        let mut neighbors = current_coordinate.neighbors();
        neighbors.sort_unstable();
        for neighbor_coordinate in neighbors {
            let Some(&neighbor_id) = coordinate_to_id.get(&neighbor_coordinate) else {
                continue;
            };
            if depths.contains_key(&neighbor_id)
                || edge_runtime_limits(ctx, neighbor_id, current_id)?.is_none()
            {
                continue;
            }
            let depth = current_depth
                .checked_add(1)
                .ok_or_else(|| "expand seed depth overflow".to_string())?;
            depths.insert(neighbor_id, depth);
            pending.push_back(neighbor_id);
        }
    }
    if depths.len() != selected_ids.len() {
        return Err("expand selection is split from its perimeter by an internal cliff".into());
    }
    Ok(depths)
}

fn outside_wave_depths(
    ctx: &ReducerContext,
    match_config: &crate::schema::MatchConfig,
    player_id: u8,
    selected_ids: &BTreeSet<u32>,
    first_ring: &BTreeSet<u32>,
    cell_count: usize,
) -> Result<Vec<u16>, String> {
    let mut depths = vec![u16::MAX; cell_count];
    let mut pending = VecDeque::new();
    for &cell_id in first_ring {
        let index =
            usize::try_from(cell_id).map_err(|_| "cell id does not fit usize".to_string())?;
        let depth = depths
            .get_mut(index)
            .ok_or_else(|| format!("first-ring cell {cell_id} is outside the map"))?;
        *depth = 1;
        pending.push_back(cell_id);
    }
    while let Some(current_id) = pending.pop_front() {
        let current_depth = depths[current_id as usize];
        let current_coordinate = coordinate_for_cell(ctx, current_id)?;
        let mut neighbors = current_coordinate.neighbors();
        neighbors.sort_unstable();
        for neighbor_coordinate in neighbors {
            let Some(neighbor_id) =
                crate::rules::cell_id_for_coordinate(match_config, neighbor_coordinate)
            else {
                continue;
            };
            if selected_ids.contains(&neighbor_id) || depths[neighbor_id as usize] != u16::MAX {
                continue;
            }
            let target = terrain(ctx, neighbor_id)?;
            let target_owner = cell_state(ctx, neighbor_id)?.owner_player_id;
            if !target.passable
                || !target.capturable
                || !matches!(target_owner, NEUTRAL_PLAYER) && target_owner != player_id
                || edge_runtime_limits(ctx, current_id, neighbor_id)?.is_none()
            {
                continue;
            }
            let depth = current_depth
                .checked_add(1)
                .filter(|depth| *depth != u16::MAX)
                .ok_or_else(|| "expand outside depth overflow".to_string())?;
            depths[neighbor_id as usize] = depth;
            pending.push_back(neighbor_id);
        }
    }
    Ok(depths)
}

fn expand_target_is_eligible(
    target_owner: u8,
    passable: bool,
    capturable: bool,
    traversable: bool,
) -> bool {
    target_owner == NEUTRAL_PLAYER && passable && capturable && traversable
}

#[cfg(test)]
fn even_partition_at(total: u64, parts: usize, index: usize) -> u64 {
    debug_assert!(parts > 0);
    debug_assert!(index < parts);
    let parts_u64 = parts as u64;
    total / parts_u64 + u64::from((index as u64) < total % parts_u64)
}

/// Splits one aggregate wave-node pool into even child quotas, then consumes
/// sorted contributions into those quotas. The persisted cursor rotates the
/// integer remainder so asynchronous small arrivals remain unbiased over time.
#[cfg(test)]
fn aggregate_lane_allocations(
    contribution_amounts: &[u64],
    lane_count: usize,
) -> Result<Vec<(usize, usize, u64)>, String> {
    aggregate_lane_allocations_rotated(contribution_amounts, lane_count, 0)
        .map(|(allocations, _)| allocations)
}

pub(crate) fn aggregate_lane_allocations_rotated(
    contribution_amounts: &[u64],
    lane_count: usize,
    start_cursor: usize,
) -> Result<(LaneAllocations, usize), String> {
    if lane_count == 0 {
        return Err("expand boundary has no outgoing lanes".into());
    }
    let total = contribution_amounts.iter().try_fold(0_u64, |sum, amount| {
        sum.checked_add(*amount)
            .ok_or_else(|| "expand boundary pool overflow".to_string())
    })?;
    let base = total / lane_count as u64;
    let remainder = (total % lane_count as u64) as usize;
    let cursor = start_cursor % lane_count;
    let mut lane_remaining = vec![base; lane_count];
    for offset in 0..remainder {
        lane_remaining[(cursor + offset) % lane_count] += 1;
    }
    let mut allocations = Vec::new();
    let mut lane_index = 0;
    for (contribution_index, &amount) in contribution_amounts.iter().enumerate() {
        let mut remaining = amount;
        while remaining > 0 {
            while lane_index < lane_count && lane_remaining[lane_index] == 0 {
                lane_index += 1;
            }
            if lane_index == lane_count {
                return Err("expand lane quotas did not conserve source strength".into());
            }
            let assigned = remaining.min(lane_remaining[lane_index]);
            allocations.push((contribution_index, lane_index, assigned));
            remaining -= assigned;
            lane_remaining[lane_index] -= assigned;
        }
    }
    if lane_remaining.iter().any(|remaining| *remaining != 0) {
        return Err("expand lane quotas left committed strength unassigned".into());
    }
    Ok((allocations, (cursor + remainder) % lane_count))
}

pub(crate) type LaneAllocations = Vec<(usize, usize, u64)>;

fn expand_selection_message(error: FrontSelectionError) -> String {
    match error {
        FrontSelectionError::EmptySelection => "expand selection is empty",
        FrontSelectionError::DisconnectedSelection => {
            "expand selection must be one six-connected region"
        }
        FrontSelectionError::NoEligibleFront => {
            "the selected region has no adjacent neutral passable ground"
        }
        FrontSelectionError::InvalidDirection | FrontSelectionError::DisconnectedFront => {
            "invalid all-front expansion boundary"
        }
    }
    .into()
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
    amount_bps: u32,
) -> Result<PlannedDistribution, String> {
    validate_basis_points(amount_bps, "redistribution amount")?;
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
    let targets = redistribution_targets_with_commitment(
        &map,
        u32::from(player_id),
        map.coordinates(),
        total,
        preset,
        amount_bps,
    )
    .map_err(|error| format!("invalid redistribution: {error:?}"))?;

    let mut source_limits = BTreeMap::new();
    let mut demands = BTreeMap::new();
    let mut total_demand = 0_u64;
    let reservations = active_destination_reservations(ctx, player_id);
    for (coordinate, target) in targets.targets {
        let cell_id = by_coordinate[&coordinate];
        let current = cell_state(ctx, cell_id)?.infantry;
        if current > target {
            source_limits.insert(cell_id, current - target);
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
        source_limits,
        demands,
        amount: total_demand,
    })
}

fn active_destination_reservations(ctx: &ReducerContext, player_id: u8) -> BTreeMap<u32, u64> {
    let mut reservations = BTreeMap::<u32, u64>::new();
    for destination in ctx.db.transfer_destination().iter() {
        let active = ctx
            .db
            .transfer_order()
            .order_id()
            .find(destination.order_id)
            .is_some_and(|order| {
                order.status == OrderStatus::Active
                    && order.player_id == player_id
                    && !matches!(order.kind, OrderKind::PushFront | OrderKind::ExpandAll)
            });
        if active {
            *reservations.entry(destination.cell_id).or_default() += destination
                .target_infantry
                .saturating_sub(destination.received_infantry);
        }
    }
    reservations
}

fn basis_point_share(value: u64, basis_points: u32) -> u64 {
    (u128::from(value) * u128::from(basis_points) / 10_000) as u64
}

fn movable_redistribution_surplus(current: u64, allocated: u64, target_surplus: u64) -> u64 {
    target_surplus.min(current).saturating_sub(allocated)
}

fn validate_basis_points(value: u32, label: &str) -> Result<(), String> {
    if (1..=10_000).contains(&value) {
        Ok(())
    } else {
        Err(format!("{label} must be between 1 and 10000 basis points"))
    }
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
    source_limits: BTreeMap<u32, u64>,
    mut destination_demands: BTreeMap<u32, u64>,
    requested: u64,
    orientation: Axial,
) -> Result<u64, String> {
    if source_limits.is_empty() {
        return Err("source selection is empty".into());
    }
    if source_limits.len() > MAX_SELECTION_CELLS {
        return Err(format!(
            "source selection exceeds the {MAX_SELECTION_CELLS}-cell command limit"
        ));
    }
    if destination_demands.is_empty() {
        return Err("destination selection is empty".into());
    }
    if source_limits
        .keys()
        .any(|source| destination_demands.contains_key(source))
    {
        return Err("source and destination selections may not overlap".into());
    }

    let mut available_by_source = BTreeMap::new();
    for (source, source_limit) in source_limits {
        let terrain_row = terrain(ctx, source)?;
        let cell = cell_state(ctx, source)?;
        if !terrain_row.passable || cell.owner_player_id != player_id {
            return Err(format!("source cell {source} is not owned passable ground"));
        }
        let allocated = allocated_infantry_at_cell(ctx, player_id, source);
        let available = movable_redistribution_surplus(cell.infantry, allocated, source_limit);
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

fn persist_expand_order(
    ctx: &ReducerContext,
    player_id: u8,
    client_command_id: u64,
    plan: PlannedExpansion,
) -> Result<u64, String> {
    if plan.requested == 0 {
        return Err("expand order has no committed infantry".into());
    }
    if plan.selected_cells.len() != plan.seed_depths.len() {
        return Err("expand topology has mismatched seed vectors".into());
    }
    if plan.selected_cells.contains(&EXPANSION_AGGREGATE_ORIGIN) {
        return Err("map cell id collides with the expansion aggregate sentinel".into());
    }

    let logical_step = state(ctx)?.logical_step;
    let order = ctx.db.transfer_order().insert(TransferOrder {
        order_id: 0,
        player_id,
        client_command_id,
        kind: OrderKind::ExpandAll,
        status: OrderStatus::Active,
        requested_infantry: plan.requested,
        committed_infantry: plan.requested,
        in_transit_infantry: plan.requested,
        delivered_infantry: 0,
        casualty_infantry: 0,
        orientation_q: 0,
        orientation_r: 0,
        created_step: logical_step,
        updated_step: logical_step,
    });

    for &cell_id in &plan.selected_cells {
        let infantry = plan.commitments.get(&cell_id).copied().unwrap_or(0);
        ctx.db.transfer_source().insert(TransferSource {
            source_key: format!("{}:{cell_id}", order.order_id),
            order_id: order.order_id,
            cell_id,
            committed_infantry: infantry,
            queued_infantry: infantry,
        });
        if infantry == 0 {
            continue;
        }
        let key = packet_key(order.order_id, cell_id, cell_id, cell_id, 0);
        ctx.db.transit_packet().insert(TransitPacket {
            packet_key: key,
            order_id: order.order_id,
            owner_player_id: player_id,
            origin_cell: cell_id,
            current_cell: cell_id,
            destination_cell: cell_id,
            infantry,
            route_index: 0,
            route: vec![cell_id],
            updated_step: logical_step,
        });
    }
    ctx.db.expansion_wave().insert(ExpansionWave {
        order_id: order.order_id,
        selected_cells: plan.selected_cells,
        seed_depths: plan.seed_depths,
        split_cursors: vec![0; plan.outside_depths.len()],
        outside_depths: plan.outside_depths,
    });
    Ok(order.order_id)
}

fn cancel_matching_pushes(
    ctx: &ReducerContext,
    player_id: u8,
    selected_cells: &[u32],
    direction: Axial,
) -> Result<Option<u64>, String> {
    if !Axial::DIRECTIONS.contains(&direction) {
        return Err("push direction must be one of the six adjacent hex directions".into());
    }
    let selected = unique_selection(selected_cells, "push cancellation")?;
    let mut matching = ctx
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
        .filter_map(|order| {
            let sources = ctx
                .db
                .transfer_source()
                .source_by_order()
                .filter(order.order_id)
                .map(|source| source.cell_id)
                .collect::<BTreeSet<_>>();
            (!sources.is_empty() && sources.is_subset(&selected)).then_some(order.order_id)
        })
        .collect::<Vec<_>>();
    matching.sort_unstable();
    if matching.is_empty() {
        return Err("no active push matches that selected source region and direction".into());
    }
    for order_id in matching.iter().copied() {
        cancel_order(ctx, player_id, order_id)?;
    }
    Ok((matching.len() == 1).then_some(matching[0]))
}

fn cancel_matching_expand_all(
    ctx: &ReducerContext,
    player_id: u8,
    selected_cells: &[u32],
) -> Result<Option<u64>, String> {
    let selected = unique_selection(selected_cells, "expand cancellation")?;
    let mut matching = ctx
        .db
        .transfer_order()
        .iter()
        .filter(|order| {
            order.player_id == player_id
                && order.status == OrderStatus::Active
                && order.kind == OrderKind::ExpandAll
        })
        .filter_map(|order| {
            let sources = ctx
                .db
                .transfer_source()
                .source_by_order()
                .filter(order.order_id)
                .map(|source| source.cell_id)
                .collect::<BTreeSet<_>>();
            (!sources.is_empty() && sources.is_subset(&selected)).then_some(order.order_id)
        })
        .collect::<Vec<_>>();
    matching.sort_unstable();
    if matching.is_empty() {
        return Err("no active all-front expansion matches that selected source region".into());
    }
    for order_id in matching.iter().copied() {
        cancel_order(ctx, player_id, order_id)?;
    }
    Ok((matching.len() == 1).then_some(matching[0]))
}

fn cancel_order(ctx: &ReducerContext, player_id: u8, order_id: u64) -> Result<(), String> {
    let mut order = ctx
        .db
        .transfer_order()
        .order_id()
        .find(order_id)
        .ok_or_else(|| format!("unknown order {order_id}"))?;
    if order.player_id != player_id {
        return Err("the order belongs to the other player".into());
    }
    if order.status != OrderStatus::Active {
        return Err("the order is no longer active".into());
    }
    let packets: Vec<_> = ctx
        .db
        .transit_packet()
        .packet_by_order()
        .filter(order_id)
        .collect();
    let released = packets.iter().try_fold(0_u64, |total, packet| {
        total
            .checked_add(packet.infantry)
            .ok_or_else(|| "cancelled push strength overflow".to_string())
    })?;
    let settled = cancelled_settled_strength(
        order.committed_infantry,
        order.delivered_infantry,
        order.casualty_infantry,
        released,
    )?;
    for packet in packets {
        ctx.db
            .transit_packet()
            .packet_key()
            .delete(&packet.packet_key);
    }
    order.status = OrderStatus::Cancelled;
    order.in_transit_infantry = 0;
    order.delivered_infantry = settled;
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
    ctx.db.expansion_wave().order_id().delete(order_id);
    Ok(())
}

fn cancelled_settled_strength(
    committed: u64,
    already_settled: u64,
    casualties: u64,
    released: u64,
) -> Result<u64, String> {
    let settled = already_settled
        .checked_add(released)
        .ok_or_else(|| "cancelled push settled strength overflow".to_string())?;
    let accounted = settled
        .checked_add(casualties)
        .ok_or_else(|| "cancelled push accounting overflow".to_string())?;
    if accounted != committed {
        return Err(format!(
            "cancelled push violates infantry conservation: committed {committed}, accounted {accounted}"
        ));
    }
    Ok(settled)
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
    fn push_commitment_is_a_snapshot_share_of_unallocated_strength() {
        assert_eq!(basis_point_share(100, 5_000), 50);
        assert_eq!(basis_point_share(30, 5_000), 15);
        assert_eq!(basis_point_share(1, 5_000), 0);
        assert_eq!(basis_point_share(u64::MAX, 10_000), u64::MAX);
    }

    #[test]
    fn command_percentages_reject_zero_and_out_of_range_values() {
        assert!(validate_basis_points(1, "amount").is_ok());
        assert!(validate_basis_points(10_000, "amount").is_ok());
        assert_eq!(
            validate_basis_points(0, "amount"),
            Err("amount must be between 1 and 10000 basis points".into())
        );
        assert_eq!(
            validate_basis_points(10_001, "amount"),
            Err("amount must be between 1 and 10000 basis points".into())
        );
    }

    #[test]
    fn redistribution_never_moves_below_a_percentage_frozen_target() {
        // A 25% solve may ask 35 -> 27 and 65 -> 49. Even when the first
        // source is routed first, its contribution is capped at its own eight
        // soldier surplus instead of the destination's full demand.
        assert_eq!(movable_redistribution_surplus(35, 0, 35 - 27), 8);
        assert_eq!(movable_redistribution_surplus(65, 0, 65 - 49), 16);
        assert_eq!(movable_redistribution_surplus(35, 5, 35 - 27), 3);
        assert_eq!(movable_redistribution_surplus(35, 30, 35 - 27), 0);
    }

    #[test]
    fn cancelling_releases_the_fixed_pool_in_place_without_losing_strength() {
        assert_eq!(cancelled_settled_strength(100, 15, 25, 60), Ok(75));
        assert!(cancelled_settled_strength(100, 15, 25, 59).is_err());
        assert!(cancelled_settled_strength(100, u64::MAX, 0, 1).is_err());
    }

    #[test]
    fn all_front_forks_split_exactly_and_deterministically() {
        assert_eq!(
            (0..3)
                .map(|index| even_partition_at(10, 3, index))
                .collect::<Vec<_>>(),
            vec![4, 3, 3]
        );
        for total in 0..=100_u64 {
            for parts in 1..=6 {
                let split = (0..parts)
                    .map(|index| even_partition_at(total, parts, index))
                    .collect::<Vec<_>>();
                assert_eq!(split.iter().sum::<u64>(), total);
                assert!(split.windows(2).all(|pair| pair[0] >= pair[1]));
                assert!(split.windows(2).all(|pair| pair[0] - pair[1] <= 1));
            }
        }
    }

    #[test]
    fn all_front_expansion_only_accepts_neutral_traversable_ground() {
        assert!(expand_target_is_eligible(0, true, true, true));
        assert!(!expand_target_is_eligible(1, true, true, true));
        assert!(!expand_target_is_eligible(2, true, true, true));
        assert!(!expand_target_is_eligible(0, false, true, true));
        assert!(!expand_target_is_eligible(0, true, false, true));
        assert!(!expand_target_is_eligible(0, true, true, false));
    }

    #[test]
    fn commitment_is_taken_once_before_a_boundary_fork() {
        let available = 101;
        let committed = basis_point_share(available, 2_500);
        let forked = (0..4)
            .map(|index| even_partition_at(committed, 4, index))
            .sum::<u64>();

        assert_eq!(committed, 25);
        assert_eq!(forked, committed);
    }

    #[test]
    fn many_small_sources_split_evenly_as_one_boundary_pool() {
        let allocations = aggregate_lane_allocations(&vec![1; 101], 3).unwrap();
        let mut lane_totals = [0_u64; 3];
        let mut source_totals = vec![0_u64; 101];
        for (source, lane, amount) in allocations {
            source_totals[source] += amount;
            lane_totals[lane] += amount;
        }

        assert_eq!(lane_totals, [34, 34, 33]);
        assert!(source_totals.iter().all(|amount| *amount == 1));
    }

    #[test]
    fn central_wave_node_splits_equally_across_all_six_neighbors() {
        let allocations = aggregate_lane_allocations(&[600], 6).unwrap();
        let mut totals = [0_u64; 6];
        for (_, lane, amount) in allocations {
            totals[lane] += amount;
        }
        assert_eq!(totals, [100; 6]);
    }

    #[test]
    fn asynchronous_single_strength_arrivals_rotate_across_every_child() {
        let mut cursor = 0;
        let mut totals = [0_u64; 6];
        for _ in 0..6 {
            let (allocations, next_cursor) =
                aggregate_lane_allocations_rotated(&[1], 6, cursor).unwrap();
            for (_, lane, amount) in allocations {
                totals[lane] += amount;
            }
            cursor = next_cursor;
        }
        assert_eq!(totals, [1; 6]);
        assert_eq!(cursor, 0);
    }

    #[test]
    fn perfectly_divisible_split_does_not_dirty_the_remainder_cursor() {
        let (_, next_cursor) = aggregate_lane_allocations_rotated(&[12], 6, 4).unwrap();
        assert_eq!(next_cursor, 4);
    }
}
