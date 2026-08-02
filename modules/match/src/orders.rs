use std::collections::{BTreeMap, BTreeSet};

use hex_core::{Axial, DistributionPreset, HexMap, redistribution_targets};
use spacetimedb::{ReducerContext, Table};

use crate::rules::{
    MAX_SELECTION_CELLS, allocated_infantry_at_cell, cell_state, command_was_seen,
    coordinate_for_cell, core_cell, packet_key, require_running_player, route_to, state, terrain,
    write_receipt,
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
    let reservations = active_destination_reservations(ctx);

    let mut remaining = requested;
    let mut demands = BTreeMap::new();
    for destination in destinations {
        let terrain_row = terrain(ctx, destination)?;
        if !terrain_row.passable || !terrain_row.capturable {
            return Err(format!(
                "destination cell {destination} is not capturable ground"
            ));
        }
        let cell = cell_state(ctx, destination)?;
        let capacity_before_reservations = if cell.owner_player_id == player_id {
            cell.military_capacity.saturating_sub(cell.infantry)
        } else {
            cell.military_capacity
        };
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
    let reservations = active_destination_reservations(ctx);
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

fn active_destination_reservations(ctx: &ReducerContext) -> BTreeMap<u32, u64> {
    let mut reservations = BTreeMap::<u32, u64>::new();
    for destination in ctx.db.transfer_destination().iter() {
        let active = ctx
            .db
            .transfer_order()
            .order_id()
            .find(destination.order_id)
            .is_some_and(|order| order.status == OrderStatus::Active);
        if active {
            *reservations.entry(destination.cell_id).or_default() += destination
                .target_infantry
                .saturating_sub(destination.received_infantry);
        }
    }
    reservations
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
    let committed = requested - remaining;
    if committed == 0 {
        return Err("no selected source can route to a selected destination".into());
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
