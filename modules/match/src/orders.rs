use std::collections::{BTreeMap, BTreeSet, VecDeque};

use hex_core::{
    Axial, FrontSelectionError, HexMap, MovementConfig, StrategicExterior, StrategicFront,
    UNIFORM_ALLOCATION_WEIGHT, ground_traversal, redistribution_targets_dense_with_weights,
    redistribution_targets_with_fallback_constraints, selected_all_front_edges,
    selected_directional_routes, selected_front_edges, selected_local_front_routes, shortest_path,
    strategic_front_index_for_seed, strategic_fronts,
};
use spacetimedb::{ReducerContext, Table};

use crate::rules::{
    MAX_SELECTION_CELLS, allocated_infantry_at_cell, cell_state, command_was_seen, config,
    coordinate_for_cell, core_cell, edge_runtime_limits, order_cell_key, require_running_player,
    state, terrain, write_receipt,
};
use crate::schema::{
    EXPANSION_AGGREGATE_ORIGIN, ExpansionWave, NEUTRAL_PLAYER, OrderKind, OrderStatus,
    ReceiptStatus, RetreatAbandonment, TransferDestination, TransferOrder, TransferSource,
    TransitPacket, TransitRoute,
};
use crate::schema::{
    cell_state as _, expansion_wave, mobilization_policy, retreat_abandonment,
    transfer_destination, transfer_order, transfer_source, transit_packet, transit_route,
};

#[derive(Clone)]
struct PlannedLeg {
    source: u32,
    destination: u32,
    amount: u64,
    route: Vec<u32>,
}

struct PlannedDistribution {
    /// The component snapshot used to calculate the targets is also the
    /// authoritative routing graph for this plan. Keeping it here avoids
    /// rebuilding the same cells through database probes for every leg.
    map: HexMap,
    cell_ids_by_coordinate: BTreeMap<Axial, u32>,
    coordinates_by_cell_id: BTreeMap<u32, Axial>,
    /// Maximum affected strength each source may contribute while preserving
    /// allocations that do not belong to this command.
    source_limits: BTreeMap<u32, u64>,
    demands: BTreeMap<u32, u64>,
    amount: u64,
}

struct PreparedOrderPersistence {
    logical_step: u64,
    requested: u64,
    committed: u64,
    legs: Vec<PlannedLeg>,
    source_totals: BTreeMap<u32, u64>,
    destination_totals: BTreeMap<u32, u64>,
}

struct PlannedExpansion {
    kind: OrderKind,
    selected_cells: Vec<u32>,
    outside_depths: Vec<u16>,
    focus_cell_id: Option<u32>,
    target_cells: Vec<u32>,
    commitments: BTreeMap<u32, u64>,
    requested: u64,
}

/// Read-only command view used to replace active orders without releasing
/// their packets until the replacement has been fully planned.
#[derive(Clone, Debug, Eq, PartialEq)]
struct RetaskSelection {
    source_cells: BTreeSet<u32>,
    superseded_order_ids: BTreeSet<u64>,
    released_by_cell: BTreeMap<u32, u64>,
}

impl RetaskSelection {
    fn released_at(&self, cell_id: u32) -> u64 {
        self.released_by_cell.get(&cell_id).copied().unwrap_or(0)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RetaskPacketSnapshot {
    order_id: u64,
    current_cell: u32,
    infantry: u64,
    current_cell_owned: bool,
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
    player_id: u16,
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

/// Commits every selected owned region toward its eligible fronts.
///
/// A non-zero orientation selects one exact global axial direction. The zero
/// orientation is the local-arc sentinel: only hostile contact edges
/// participate, and every reachable source is assigned to one nearby edge so
/// different fronts may advance along different local normals.
#[spacetimedb::reducer]
pub fn issue_push_front(
    ctx: &ReducerContext,
    client_command_id: u64,
    source_cells: Vec<u32>,
    direction_q: i32,
    direction_r: i32,
    commitment_bps: u32,
    supersede_order_ids: Vec<u64>,
) -> Result<(), String> {
    let player_id = require_running_player(ctx)?;
    if command_was_seen(ctx, player_id, client_command_id) {
        return Ok(());
    }
    let direction = Axial::new(direction_q, direction_r);
    let prepared = resolve_retask_selection(
        ctx,
        player_id,
        &source_cells,
        &supersede_order_ids,
        "push source",
    )
    .and_then(|selection| {
        plan_push_front(ctx, player_id, &selection, direction, commitment_bps)
            .map(|(requested, legs, abandonments)| (selection, requested, legs, abandonments))
    });
    let (selection, requested, legs, abandonments) = match prepared {
        Ok(prepared) => prepared,
        Err(message) => {
            return receipt_result(
                ctx,
                player_id,
                client_command_id,
                "issue_push_front",
                Err(message),
            );
        }
    };
    cancel_superseded_orders(ctx, player_id, &selection.superseded_order_ids)?;
    let order_id = persist_order(
        ctx,
        player_id,
        client_command_id,
        OrderKind::PushFront,
        requested,
        direction,
        legs,
    )?;
    persist_retreat_abandonments(ctx, order_id, &abandonments);
    receipt_result(
        ctx,
        player_id,
        client_command_id,
        "issue_push_front",
        Ok(Some(order_id)),
    )
}

/// Commits one fixed share of currently unallocated infantry already stationed
/// on each eligible neutral-facing perimeter cell. Interior troops remain in
/// place until the player explicitly rebalances them to a front.
#[spacetimedb::reducer]
pub fn issue_expand_all(
    ctx: &ReducerContext,
    client_command_id: u64,
    source_cells: Vec<u32>,
    commitment_bps: u32,
    supersede_order_ids: Vec<u64>,
) -> Result<(), String> {
    let player_id = require_running_player(ctx)?;
    if command_was_seen(ctx, player_id, client_command_id) {
        return Ok(());
    }
    let prepared = resolve_retask_selection(
        ctx,
        player_id,
        &source_cells,
        &supersede_order_ids,
        "expand source",
    )
    .and_then(|selection| {
        plan_expand_all(ctx, player_id, &selection, commitment_bps).map(|plan| (selection, plan))
    });
    let (selection, plan) = match prepared {
        Ok(prepared) => prepared,
        Err(message) => {
            return receipt_result(
                ctx,
                player_id,
                client_command_id,
                "issue_expand_all",
                Err(message),
            );
        }
    };
    cancel_superseded_orders(ctx, player_id, &selection.superseded_order_ids)?;
    let order_id = persist_expand_order(ctx, player_id, client_command_id, plan)?;
    receipt_result(
        ctx,
        player_id,
        client_command_id,
        "issue_expand_all",
        Ok(Some(order_id)),
    )
}

/// Expands every complete owned cluster touched by `source_seed_cells` across
/// its full neutral perimeter. `focus_cell_id` biases, but never suppresses,
/// branches toward the clicked neutral hex.
#[spacetimedb::reducer]
pub fn issue_expand_clusters(
    ctx: &ReducerContext,
    client_command_id: u64,
    source_seed_cells: Vec<u32>,
    focus_cell_id: u32,
    commitment_bps: u32,
) -> Result<(), String> {
    let player_id = require_running_player(ctx)?;
    if command_was_seen(ctx, player_id, client_command_id) {
        return Ok(());
    }
    let prepared = complete_owned_component_selection(
        ctx,
        player_id,
        &source_seed_cells,
        "cluster expand source",
    )
    .and_then(|selection| {
        plan_expand_clusters(ctx, player_id, &selection, focus_cell_id, commitment_bps)
            .map(|plan| (selection, plan))
    });
    let (selection, plan) = match prepared {
        Ok(prepared) => prepared,
        Err(message) => {
            return receipt_result(
                ctx,
                player_id,
                client_command_id,
                "issue_expand_clusters",
                Err(message),
            );
        }
    };
    cancel_superseded_orders(ctx, player_id, &selection.superseded_order_ids)?;
    let order_id = persist_expand_order(ctx, player_id, client_command_id, plan)?;
    receipt_result(
        ctx,
        player_id,
        client_command_id,
        "issue_expand_clusters",
        Ok(Some(order_id)),
    )
}

/// Attacks the immutable union of complete enemy clusters touched by
/// `target_seed_cells`, starting every front shared with the selected source
/// clusters. The wave may turn and branch as captures reveal new masked edges,
/// but can never leave the snapshotted target footprint.
#[spacetimedb::reducer]
pub fn issue_attack_clusters(
    ctx: &ReducerContext,
    client_command_id: u64,
    source_seed_cells: Vec<u32>,
    target_seed_cells: Vec<u32>,
    commitment_bps: u32,
) -> Result<(), String> {
    let player_id = require_running_player(ctx)?;
    if command_was_seen(ctx, player_id, client_command_id) {
        return Ok(());
    }
    let prepared = complete_owned_component_selection(
        ctx,
        player_id,
        &source_seed_cells,
        "cluster attack source",
    )
    .and_then(|selection| {
        complete_enemy_component_selection(
            ctx,
            player_id,
            &target_seed_cells,
            "cluster attack target",
        )
        .and_then(|target_cells| {
            plan_attack_clusters(ctx, player_id, &selection, &target_cells, commitment_bps)
                .map(|plan| (selection, plan))
        })
    });
    let (selection, plan) = match prepared {
        Ok(prepared) => prepared,
        Err(message) => {
            return receipt_result(
                ctx,
                player_id,
                client_command_id,
                "issue_attack_clusters",
                Err(message),
            );
        }
    };
    cancel_superseded_orders(ctx, player_id, &selection.superseded_order_ids)?;
    let order_id = persist_expand_order(ctx, player_id, client_command_id, plan)?;
    receipt_result(
        ctx,
        player_id,
        client_command_id,
        "issue_attack_clusters",
        Ok(Some(order_id)),
    )
}

#[spacetimedb::reducer]
pub fn issue_reshape(
    ctx: &ReducerContext,
    client_command_id: u64,
    source_cells: Vec<u32>,
    target_cells: Vec<u32>,
    supersede_order_ids: Vec<u64>,
) -> Result<(), String> {
    let player_id = require_running_player(ctx)?;
    if command_was_seen(ctx, player_id, client_command_id) {
        return Ok(());
    }
    let prepared = resolve_single_cluster_retask_selection(
        ctx,
        player_id,
        &source_cells,
        &supersede_order_ids,
        "reshape source",
    )
    .and_then(|(cluster, selection)| {
        let targets = unique_selection(&target_cells, "reshape destination")?;
        validate_owned_passable_cells(ctx, player_id, &targets, "reshape destination")?;
        validate_cells_within_cluster(&targets, &cluster, "reshape destination")?;
        let reservations =
            active_destination_reservations(ctx, player_id, &selection.superseded_order_ids);
        reject_active_internal_destination_overlap("reshape destination", &targets, &reservations)?;
        let target_cells = targets.into_iter().collect::<Vec<_>>();
        let plan = shape_distribution_plan(ctx, player_id, &selection, &target_cells)?;
        if plan.amount == 0 {
            return Err("reshape destination does not move any selected troops".into());
        }
        let requested = plan.amount;
        let legs = plan_distribution_legs(ctx, player_id, &selection, plan, None)?;
        if legs.is_empty() {
            return Err("reshape destination does not move any selected troops".into());
        }
        Ok((selection, requested, legs))
    });
    let (selection, requested, legs) = match prepared {
        Ok(prepared) => prepared,
        Err(message) => {
            return receipt_result(
                ctx,
                player_id,
                client_command_id,
                "issue_reshape",
                Err(message),
            );
        }
    };
    cancel_superseded_orders(ctx, player_id, &selection.superseded_order_ids)?;
    let order_id = persist_order(
        ctx,
        player_id,
        client_command_id,
        OrderKind::Reshape,
        requested,
        Axial::ZERO,
        legs,
    )?;
    receipt_result(
        ctx,
        player_id,
        client_command_id,
        "issue_reshape",
        Ok(Some(order_id)),
    )
}

/// Moves a Share of movable troops from one strategic front arc onto another
/// front of the same owned traversable component.
///
/// `source_component_cells` must be owned seeds resolving to exactly one
/// complete component; authority expands them to the current component.
/// `source_front_seed` / `target_front_seed` are owned boundary cell IDs that
/// identify the two fronts (any source cell of a front edge works). Cross-front
/// share is the caller's `commitment_bps` applied once to movable source-front
/// troops; target placement inside the destination front is proportional to
/// exposed edge count, capacity-safe. Routing uses one pass of current terrain
/// costs over the component graph.
///
/// # Limitation
/// Front identity uses the pure [`strategic_fronts`] derivation (neutral gaps
/// bridge same-opponent hostile runs). Durable front IDs are not persisted.
#[spacetimedb::reducer]
pub fn issue_front_rebalance(
    ctx: &ReducerContext,
    client_command_id: u64,
    source_component_cells: Vec<u32>,
    source_front_seed: u32,
    target_front_seed: u32,
    commitment_bps: u32,
    supersede_order_ids: Vec<u64>,
) -> Result<(), String> {
    let player_id = require_running_player(ctx)?;
    if command_was_seen(ctx, player_id, client_command_id) {
        return Ok(());
    }
    let prepared = resolve_single_cluster_retask_selection(
        ctx,
        player_id,
        &source_component_cells,
        &supersede_order_ids,
        "front rebalance component",
    )
    .and_then(|(cluster, selection)| {
        plan_front_rebalance(
            ctx,
            player_id,
            &cluster,
            &selection,
            source_front_seed,
            target_front_seed,
            commitment_bps,
        )
        .map(|(requested, legs)| (selection, requested, legs))
    });
    let (selection, requested, legs) = match prepared {
        Ok(prepared) => prepared,
        Err(message) => {
            return receipt_result(
                ctx,
                player_id,
                client_command_id,
                "issue_front_rebalance",
                Err(message),
            );
        }
    };
    cancel_superseded_orders(ctx, player_id, &selection.superseded_order_ids)?;
    let order_id = persist_order(
        ctx,
        player_id,
        client_command_id,
        OrderKind::FrontRebalance,
        requested,
        Axial::ZERO,
        legs,
    )?;
    receipt_result(
        ctx,
        player_id,
        client_command_id,
        "issue_front_rebalance",
        Ok(Some(order_id)),
    )
}

#[allow(clippy::too_many_arguments)]
#[spacetimedb::reducer]
pub fn cancel_orders(
    ctx: &ReducerContext,
    client_command_id: u64,
    order_ids: Vec<u64>,
) -> Result<(), String> {
    let player_id = require_running_player(ctx)?;
    if command_was_seen(ctx, player_id, client_command_id) {
        return Ok(());
    }
    let result = exact_order_ids(&order_ids).and_then(|order_ids| {
        for &order_id in &order_ids {
            preflight_cancel_order(ctx, player_id, order_id)?;
        }
        for &order_id in &order_ids {
            cancel_order(ctx, player_id, order_id)?;
        }
        Ok((order_ids.len() == 1).then(|| *order_ids.first().expect("one order ID")))
    });
    receipt_result(ctx, player_id, client_command_id, "cancel_orders", result)
}

fn resolve_retask_selection(
    ctx: &ReducerContext,
    player_id: u16,
    source_cells: &[u32],
    supersede_order_ids: &[u64],
    label: &str,
) -> Result<RetaskSelection, String> {
    if source_cells.is_empty() && supersede_order_ids.is_empty() {
        return Err(format!("{label} selection is empty"));
    }
    if source_cells.len() > MAX_SELECTION_CELLS {
        return Err(format!(
            "{label} selection exceeds the {MAX_SELECTION_CELLS}-cell command limit"
        ));
    }
    if supersede_order_ids.len() > MAX_SELECTION_CELLS {
        return Err(format!(
            "superseded order selection exceeds the {MAX_SELECTION_CELLS}-order command limit"
        ));
    }
    let owned_cells = source_cells.iter().copied().collect::<BTreeSet<_>>();
    for &cell_id in &owned_cells {
        if cell_state(ctx, cell_id)?.owner_player_id != player_id {
            return Err(format!("{label} source cell {cell_id} is not owned"));
        }
    }

    let requested_order_ids = supersede_order_ids.iter().copied().collect::<BTreeSet<_>>();
    let mut active_orders = BTreeMap::new();
    let mut packets = Vec::new();
    for &order_id in &requested_order_ids {
        let order = ctx
            .db
            .transfer_order()
            .order_id()
            .find(order_id)
            .ok_or_else(|| format!("unknown superseded order {order_id}"))?;
        let order_packets = ctx
            .db
            .transit_packet()
            .packet_by_order()
            .filter(order_id)
            .collect::<Vec<_>>();
        validate_superseded_order_claim(
            order_id,
            player_id,
            order.player_id,
            order.status,
            !order_packets.is_empty(),
        )?;
        for packet in order_packets {
            if packet.owner_player_id != player_id {
                return Err(format!(
                    "superseded order {order_id} has a packet owned by another player"
                ));
            }
            packets.push(RetaskPacketSnapshot {
                order_id,
                current_cell: packet.current_cell,
                infantry: packet.infantry,
                current_cell_owned: cell_state(ctx, packet.current_cell)?.owner_player_id
                    == player_id,
            });
        }
        active_orders.insert(order_id, order);
    }
    let selection = resolve_retask_snapshot(owned_cells, &requested_order_ids, &packets)?;

    // Preflight every old order before any reducer mutation. `cancel_order`
    // performs the same accounting check when the prepared replacement is
    // committed, so a bad survivor ledger cannot partially cancel a group.
    for order_id in &selection.superseded_order_ids {
        let order = active_orders
            .get(order_id)
            .ok_or_else(|| format!("retask order {order_id} is no longer active"))?;
        let released = packets
            .iter()
            .filter(|packet| packet.order_id == *order_id)
            .try_fold(0_u64, |total, packet| {
                total
                    .checked_add(packet.infantry)
                    .ok_or_else(|| "retasked order strength overflow".to_string())
            })?;
        cancelled_settled_strength(
            order.committed_infantry,
            order.delivered_infantry,
            order.casualty_infantry,
            released,
        )?;
    }
    Ok(selection)
}

/// Resolves stale client source cells against current topology before any
/// legacy retask payload is considered. The returned source is the complete
/// current cluster, including cells gained or merged since the UI snapshot.
fn resolve_single_cluster_retask_selection(
    ctx: &ReducerContext,
    player_id: u16,
    source_seed_cells: &[u32],
    supersede_order_ids: &[u64],
    label: &str,
) -> Result<(BTreeSet<u32>, RetaskSelection), String> {
    let seeds = unique_selection(source_seed_cells, label)?;
    validate_owned_passable_cells(ctx, player_id, &seeds, label)?;
    let cluster =
        single_complete_seed_component(&seeds, &owned_components(ctx, player_id)?, label)?;
    if cluster.len() > MAX_SELECTION_CELLS {
        return Err(format!(
            "{label} cluster exceeds the {MAX_SELECTION_CELLS}-cell command limit"
        ));
    }
    let current_cluster_cells = cluster.iter().copied().collect::<Vec<_>>();
    let selection = resolve_retask_selection(
        ctx,
        player_id,
        &current_cluster_cells,
        supersede_order_ids,
        label,
    )?;
    validate_retask_within_cluster(&selection, &cluster, label)?;
    Ok((cluster, selection))
}

fn single_complete_seed_component(
    seeds: &BTreeSet<u32>,
    current_components: &[BTreeSet<u32>],
    label: &str,
) -> Result<BTreeSet<u32>, String> {
    if seeds.is_empty() {
        return Err(format!("{label} selection is empty"));
    }
    let touched = current_components
        .iter()
        .filter(|component| !component.is_disjoint(seeds))
        .collect::<Vec<_>>();
    if touched.len() != 1 || !seeds.is_subset(touched[0]) {
        return Err(format!(
            "{label} must resolve to exactly one current owned traversable cluster"
        ));
    }
    Ok(touched[0].clone())
}

fn validate_retask_within_cluster(
    selection: &RetaskSelection,
    cluster: &BTreeSet<u32>,
    label: &str,
) -> Result<(), String> {
    if let Some(cell_id) = selection.source_cells.difference(cluster).next() {
        return Err(format!(
            "{label} superseded order has surviving strength outside the current cluster at cell {cell_id}"
        ));
    }
    if let Some(cell_id) = selection
        .released_by_cell
        .keys()
        .find(|cell_id| !cluster.contains(cell_id))
    {
        return Err(format!(
            "{label} superseded release is outside the current cluster at cell {cell_id}"
        ));
    }
    Ok(())
}

fn validate_cells_within_cluster(
    cells: &BTreeSet<u32>,
    cluster: &BTreeSet<u32>,
    label: &str,
) -> Result<(), String> {
    if let Some(cell_id) = cells.difference(cluster).next() {
        Err(format!(
            "{label} cell {cell_id} is outside the selected current cluster"
        ))
    } else {
        Ok(())
    }
}

fn validate_owned_passable_cells(
    ctx: &ReducerContext,
    player_id: u16,
    cells: &BTreeSet<u32>,
    label: &str,
) -> Result<(), String> {
    for &cell_id in cells {
        let terrain = terrain(ctx, cell_id)?;
        let state = cell_state(ctx, cell_id)?;
        if !terrain.passable || state.owner_player_id != player_id {
            return Err(format!(
                "{label} cell {cell_id} is not owned passable ground"
            ));
        }
    }
    Ok(())
}

/// Complete current owned components under the same passability and elevation
/// rules used by internal routes and client cluster selection.
fn owned_components(ctx: &ReducerContext, player_id: u16) -> Result<Vec<BTreeSet<u32>>, String> {
    let max_elevation_step = u32::from(config(ctx)?.max_elevation_step);
    let mut owned = BTreeMap::<Axial, (u32, i16)>::new();
    for state in ctx
        .db
        .cell_state()
        .iter()
        .filter(|cell| cell.owner_player_id == player_id)
    {
        let terrain = terrain(ctx, state.cell_id)?;
        if terrain.passable {
            owned.insert(
                Axial::new(terrain.q, terrain.r),
                (state.cell_id, terrain.elevation),
            );
        }
    }
    let mut remaining = owned.keys().copied().collect::<BTreeSet<_>>();
    let mut result = Vec::new();
    while let Some(seed) = remaining.pop_first() {
        let mut pending = VecDeque::from([seed]);
        let mut component = BTreeSet::new();
        while let Some(current) = pending.pop_front() {
            component.insert(owned[&current].0);
            let current_elevation = owned[&current].1;
            for neighbor in current.neighbors() {
                let Some((_, neighbor_elevation)) = owned.get(&neighbor) else {
                    continue;
                };
                let elevation_delta =
                    (i32::from(current_elevation) - i32::from(*neighbor_elevation)).unsigned_abs();
                if elevation_delta <= max_elevation_step && remaining.remove(&neighbor) {
                    pending.push_back(neighbor);
                }
            }
        }
        result.push(component);
    }
    Ok(result)
}

/// orders remain allocated and are never implicitly retasked.
fn complete_owned_component_selection(
    ctx: &ReducerContext,
    player_id: u16,
    seed_cells: &[u32],
    label: &str,
) -> Result<RetaskSelection, String> {
    let source_cells = complete_components_for_seeds(ctx, player_id, seed_cells, label)?;
    Ok(RetaskSelection {
        source_cells,
        superseded_order_ids: BTreeSet::new(),
        released_by_cell: BTreeMap::new(),
    })
}

fn complete_enemy_component_selection(
    ctx: &ReducerContext,
    player_id: u16,
    seed_cells: &[u32],
    label: &str,
) -> Result<BTreeSet<u32>, String> {
    let seeds = unique_selection(seed_cells, label)?;
    let first_seed = *seeds
        .first()
        .ok_or_else(|| format!("{label} selection is empty"))?;
    let enemy_player_id = cell_state(ctx, first_seed)?.owner_player_id;
    if enemy_player_id == NEUTRAL_PLAYER || enemy_player_id == player_id {
        return Err(format!("{label} must identify enemy-owned ground"));
    }
    for &cell_id in &seeds {
        if cell_state(ctx, cell_id)?.owner_player_id != enemy_player_id {
            return Err(format!(
                "{label} cells must all belong to the same enemy player"
            ));
        }
    }
    complete_components_for_seed_set(ctx, enemy_player_id, &seeds, label)
}

fn complete_components_for_seeds(
    ctx: &ReducerContext,
    owner_player_id: u16,
    seed_cells: &[u32],
    label: &str,
) -> Result<BTreeSet<u32>, String> {
    let seeds = unique_selection(seed_cells, label)?;
    complete_components_for_seed_set(ctx, owner_player_id, &seeds, label)
}

fn complete_components_for_seed_set(
    ctx: &ReducerContext,
    owner_player_id: u16,
    seeds: &BTreeSet<u32>,
    label: &str,
) -> Result<BTreeSet<u32>, String> {
    validate_owned_passable_cells(ctx, owner_player_id, seeds, label)?;
    let selected = owned_components(ctx, owner_player_id)?
        .into_iter()
        .filter(|component| !component.is_disjoint(seeds))
        .flat_map(|component| component.into_iter())
        .collect::<BTreeSet<_>>();
    if !seeds.is_subset(&selected) {
        return Err(format!("{label} could not resolve every seed component"));
    }
    if selected.len() > MAX_SELECTION_CELLS {
        return Err(format!(
            "{label} components exceed the {MAX_SELECTION_CELLS}-cell command limit"
        ));
    }
    Ok(selected)
}

fn validate_superseded_order_claim(
    order_id: u64,
    requester_player_id: u16,
    order_player_id: u16,
    status: OrderStatus,
    has_surviving_packets: bool,
) -> Result<(), String> {
    if order_player_id != requester_player_id {
        return Err(format!(
            "superseded order {order_id} belongs to the other player"
        ));
    }
    if status != OrderStatus::Active {
        return Err(format!("superseded order {order_id} is no longer active"));
    }
    if !has_surviving_packets {
        return Err(format!(
            "superseded order {order_id} has no surviving packets"
        ));
    }
    Ok(())
}

fn resolve_retask_snapshot(
    mut owned_cells: BTreeSet<u32>,
    requested_order_ids: &BTreeSet<u64>,
    packets: &[RetaskPacketSnapshot],
) -> Result<RetaskSelection, String> {
    for &order_id in requested_order_ids {
        if !packets.iter().any(|packet| packet.order_id == order_id) {
            return Err(format!(
                "superseded order {order_id} has no surviving packets"
            ));
        }
    }

    let mut released_by_cell = BTreeMap::<u32, u64>::new();
    for packet in packets
        .iter()
        .filter(|packet| requested_order_ids.contains(&packet.order_id))
    {
        if !packet.current_cell_owned {
            return Err(format!(
                "retask order {} has surviving strength on non-owned cell {}",
                packet.order_id, packet.current_cell
            ));
        }
        owned_cells.insert(packet.current_cell);
        let released = released_by_cell.entry(packet.current_cell).or_default();
        *released = released
            .checked_add(packet.infantry)
            .ok_or_else(|| "retasked cell strength overflow".to_string())?;
    }
    if owned_cells.len() > MAX_SELECTION_CELLS {
        return Err(format!(
            "retask physical selection exceeds the {MAX_SELECTION_CELLS}-cell command limit"
        ));
    }
    Ok(RetaskSelection {
        source_cells: owned_cells,
        superseded_order_ids: requested_order_ids.clone(),
        released_by_cell,
    })
}

fn available_after_retask_release(
    total_infantry: u64,
    allocated_infantry: u64,
    released_infantry: u64,
) -> Result<u64, String> {
    let unaffected =
        unaffected_after_retask_release(total_infantry, allocated_infantry, released_infantry)?;
    Ok(total_infantry - unaffected)
}

fn unaffected_after_retask_release(
    current_infantry: u64,
    allocated_infantry: u64,
    released_infantry: u64,
) -> Result<u64, String> {
    if released_infantry > allocated_infantry {
        return Err("retask released strength exceeds allocated strength at its cell".into());
    }
    let physically_allocated = allocated_infantry.min(current_infantry);
    let physically_released = released_infantry.min(physically_allocated);
    Ok(physically_allocated - physically_released)
}

fn plan_push_front(
    ctx: &ReducerContext,
    player_id: u16,
    selection: &RetaskSelection,
    direction: Axial,
    commitment_bps: u32,
) -> Result<(u64, Vec<PlannedLeg>, BTreeSet<u32>), String> {
    validate_basis_points(commitment_bps, "push commitment")?;
    if direction == Axial::ZERO {
        return plan_local_arc_push(ctx, player_id, selection, commitment_bps);
    }
    if !Axial::DIRECTIONS.contains(&direction) {
        return Err("push direction must be one of the six adjacent hex directions".into());
    }

    let mut selected_map = HexMap::new();
    let mut coordinate_to_id = BTreeMap::new();
    for &cell_id in &selection.source_cells {
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
    let mut target_by_boundary = BTreeMap::<Axial, (u32, bool)>::new();
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
        if !initial_push_target_is_eligible(
            player_id,
            target_state.owner_player_id,
            target_terrain.passable,
            target_terrain.capturable,
            edge_runtime_limits(ctx, source_id, target_id)?.is_some(),
        ) {
            continue;
        }
        target_by_boundary.insert(
            source_coordinate,
            (target_id, target_state.owner_player_id == player_id),
        );
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
    let routes = front_route_tree(&selected_map, &boundary_sources, direction, &movement);

    let mut requested = 0_u64;
    let mut legs = Vec::new();
    let mut abandonments = BTreeSet::new();
    for (&source_coordinate, &source_id) in &coordinate_to_id {
        let Some((boundary, route_coordinates)) = routes.route_to_boundary(source_coordinate)
        else {
            continue;
        };
        let &(target_id, friendly_boundary) = target_by_boundary
            .get(&boundary)
            .ok_or_else(|| "push route ended at an unknown boundary".to_string())?;
        let source = cell_state(ctx, source_id)?;
        let allocated = allocated_infantry_at_cell(ctx, player_id, source_id);
        let available = available_after_retask_release(
            source.infantry,
            allocated,
            selection.released_at(source_id),
        )?;
        let commitment = basis_point_share(available, commitment_bps);
        requested = requested
            .checked_add(commitment)
            .ok_or_else(|| "push requested infantry overflow".to_string())?;
        if commitment == 0 {
            continue;
        }

        let (destination, route) = if friendly_boundary {
            let (destination, route, abandon_source) = retreat_translation_leg(
                source_coordinate,
                direction,
                &selected_coordinates,
                &coordinate_to_id,
                target_id,
            )
            .ok_or_else(|| "retreat source is missing from its selected column".to_string())?;
            if edge_runtime_limits(ctx, source_id, destination)?.is_none() {
                continue;
            }
            if abandon_source && commitment == source.infantry {
                abandonments.insert(source_id);
            }
            (destination, route)
        } else {
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
            (target_id, route)
        };
        legs.push(PlannedLeg {
            source: source_id,
            destination,
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
    Ok((requested, legs, abandonments))
}

/// Plans one hostile-contact Push whose lanes each keep the normal of their
/// assigned local front edge. The shared pure planner assigns at most one
/// route to each source, so the source's Share is computed exactly once even
/// when the selected region touches several hostile arcs.
fn plan_local_arc_push(
    ctx: &ReducerContext,
    player_id: u16,
    selection: &RetaskSelection,
    commitment_bps: u32,
) -> Result<(u64, Vec<PlannedLeg>, BTreeSet<u32>), String> {
    let mut selected_map = HexMap::new();
    let mut coordinate_to_id = BTreeMap::new();
    for &cell_id in &selection.source_cells {
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

    let mut target_by_edge = BTreeMap::<(Axial, Axial), u32>::new();
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
            if local_arc_target_is_eligible(
                player_id,
                target_state.owner_player_id,
                target_terrain.passable,
                target_terrain.capturable,
                edge_runtime_limits(ctx, source_id, target_id)?.is_some(),
            ) {
                target_by_edge.insert((source_coordinate, target_coordinate), target_id);
            }
        }
    }

    let edges = selected_all_front_edges(&selected_coordinates, |source, target| {
        target_by_edge.contains_key(&(source, target))
    })
    .map_err(local_arc_selection_message)?;

    let movement = MovementConfig {
        max_elevation_step: u16::from(match_config.max_elevation_step),
        level_cost: 10,
        uphill_cost: 15,
        downhill_cost: 10,
    };
    let mut traversal_costs = BTreeMap::<(Axial, Axial), u64>::new();
    for (&from_coordinate, &from_id) in &coordinate_to_id {
        for to_coordinate in from_coordinate.neighbors() {
            let Some(&to_id) = coordinate_to_id.get(&to_coordinate) else {
                continue;
            };
            if edge_runtime_limits(ctx, from_id, to_id)?.is_none() {
                continue;
            }
            let Some(traversal) = selected_map
                .get(from_coordinate)
                .zip(selected_map.get(to_coordinate))
                .and_then(|(from, to)| ground_traversal(from, to, &movement))
            else {
                continue;
            };
            traversal_costs.insert((from_coordinate, to_coordinate), u64::from(traversal.cost));
        }
    }
    let routes = selected_local_front_routes(&selected_coordinates, &edges, |from, to| {
        traversal_costs.get(&(from, to)).copied()
    });

    let mut requested = 0_u64;
    let mut legs = Vec::new();
    for (source_coordinate, local_route) in routes {
        let source_id = coordinate_to_id[&source_coordinate];
        let source = cell_state(ctx, source_id)?;
        let allocated = allocated_infantry_at_cell(ctx, player_id, source_id);
        let available = available_after_retask_release(
            source.infantry,
            allocated,
            selection.released_at(source_id),
        )?;
        let commitment = basis_point_share(available, commitment_bps);
        requested = requested
            .checked_add(commitment)
            .ok_or_else(|| "push requested infantry overflow".to_string())?;
        if commitment == 0 {
            continue;
        }

        let (&target_coordinate, interior) = local_route
            .cells
            .split_last()
            .ok_or_else(|| "local push route is empty".to_string())?;
        if target_coordinate != local_route.edge.target {
            return Err("local push route ended at the wrong hostile target".into());
        }
        let mut route = interior
            .iter()
            .map(|coordinate| {
                coordinate_to_id
                    .get(coordinate)
                    .copied()
                    .ok_or_else(|| "local push route escaped the selected region".to_string())
            })
            .collect::<Result<Vec<_>, _>>()?;
        let target_id = target_by_edge
            .get(&(local_route.edge.source, local_route.edge.target))
            .copied()
            .ok_or_else(|| "local push route ended at an unknown hostile edge".to_string())?;
        route.push(target_id);
        legs.push(PlannedLeg {
            source: source_id,
            destination: target_id,
            amount: commitment,
            route,
        });
    }

    if requested == 0 {
        return Err(
            "the hostile contact arcs have no uncommitted infantry at this commitment".into(),
        );
    }
    if legs.is_empty() {
        return Err("the hostile contact arcs have no committed infantry".into());
    }
    Ok((requested, legs, BTreeSet::new()))
}

fn plan_expand_all(
    ctx: &ReducerContext,
    player_id: u16,
    selection: &RetaskSelection,
    commitment_bps: u32,
) -> Result<PlannedExpansion, String> {
    plan_neutral_expansion(
        ctx,
        player_id,
        selection,
        commitment_bps,
        OrderKind::ExpandAll,
        None,
    )
}

fn plan_expand_clusters(
    ctx: &ReducerContext,
    player_id: u16,
    selection: &RetaskSelection,
    focus_cell_id: u32,
    commitment_bps: u32,
) -> Result<PlannedExpansion, String> {
    let focus_terrain = terrain(ctx, focus_cell_id)?;
    let focus_state = cell_state(ctx, focus_cell_id)?;
    if focus_state.owner_player_id != NEUTRAL_PLAYER
        || !focus_terrain.passable
        || !focus_terrain.capturable
    {
        return Err("cluster expand focus must be unclaimed passable ground".into());
    }
    plan_neutral_expansion(
        ctx,
        player_id,
        selection,
        commitment_bps,
        OrderKind::ExpandClusters,
        Some(focus_cell_id),
    )
}

fn plan_neutral_expansion(
    ctx: &ReducerContext,
    player_id: u16,
    selection: &RetaskSelection,
    commitment_bps: u32,
    kind: OrderKind,
    focus_cell_id: Option<u32>,
) -> Result<PlannedExpansion, String> {
    if !matches!(kind, OrderKind::ExpandAll | OrderKind::ExpandClusters) {
        return Err("neutral expansion received an invalid order kind".into());
    }
    validate_basis_points(commitment_bps, "expand commitment")?;

    let selected_ids = &selection.source_cells;
    let mut coordinate_to_id = BTreeMap::new();
    for cell_id in selected_ids.iter().copied() {
        let terrain_row = terrain(ctx, cell_id)?;
        let cell = core_cell(ctx, cell_id)?;
        if !terrain_row.passable || cell.owner != Some(u32::from(player_id)) {
            return Err(format!(
                "expand source cell {cell_id} is not owned passable ground"
            ));
        }
        coordinate_to_id.insert(cell.coordinate, cell_id);
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
    let cell_count = usize::from(match_config.map_width)
        .checked_mul(usize::from(match_config.map_height))
        .ok_or_else(|| "map cell count overflow".to_string())?;
    let outside_depths = outside_wave_depths(
        ctx,
        &match_config,
        player_id,
        selected_ids,
        &first_ring,
        cell_count,
    )?;

    let selected_cells = boundary_cells.iter().copied().collect::<Vec<_>>();
    let commitments =
        perimeter_commitments(ctx, player_id, selection, &boundary_cells, commitment_bps)?;
    let requested = commitments.values().try_fold(0_u64, |total, commitment| {
        total
            .checked_add(*commitment)
            .ok_or_else(|| "expand requested infantry overflow".to_string())
    })?;
    if requested == 0 {
        return Err("the eligible neutral perimeters have no uncommitted infantry".into());
    }
    Ok(PlannedExpansion {
        kind,
        selected_cells,
        outside_depths,
        focus_cell_id,
        target_cells: Vec::new(),
        commitments,
        requested,
    })
}

fn plan_attack_clusters(
    ctx: &ReducerContext,
    player_id: u16,
    selection: &RetaskSelection,
    target_cells: &BTreeSet<u32>,
    commitment_bps: u32,
) -> Result<PlannedExpansion, String> {
    validate_basis_points(commitment_bps, "attack commitment")?;
    if target_cells.is_empty() {
        return Err("cluster attack target selection is empty".into());
    }

    let selected_ids = &selection.source_cells;
    let mut coordinate_to_id = BTreeMap::new();
    for cell_id in selected_ids.iter().copied() {
        let terrain_row = terrain(ctx, cell_id)?;
        let cell = core_cell(ctx, cell_id)?;
        if !terrain_row.passable || cell.owner != Some(u32::from(player_id)) {
            return Err(format!(
                "cluster attack source cell {cell_id} is not owned passable ground"
            ));
        }
        coordinate_to_id.insert(cell.coordinate, cell_id);
    }
    let selected_coordinates = coordinate_to_id.keys().copied().collect::<BTreeSet<_>>();
    let match_config = config(ctx)?;

    let mut eligible_targets = BTreeMap::<(Axial, Axial), u32>::new();
    for (&source_coordinate, &source_id) in &coordinate_to_id {
        for direction in Axial::DIRECTIONS {
            let target_coordinate = source_coordinate + direction;
            let Some(target_id) =
                crate::rules::cell_id_for_coordinate(&match_config, target_coordinate)
            else {
                continue;
            };
            if !target_cells.contains(&target_id) {
                continue;
            }
            let target_terrain = terrain(ctx, target_id)?;
            if target_terrain.passable
                && target_terrain.capturable
                && edge_runtime_limits(ctx, source_id, target_id)?.is_some()
            {
                eligible_targets.insert((source_coordinate, target_coordinate), target_id);
            }
        }
    }

    let edges = selected_all_front_edges(&selected_coordinates, |source, target| {
        eligible_targets.contains_key(&(source, target))
    })
    .map_err(attack_cluster_selection_message)?;
    let boundary_cells = edges
        .iter()
        .map(|edge| coordinate_to_id[&edge.source])
        .collect::<BTreeSet<_>>();
    let first_ring = edges
        .iter()
        .map(|edge| eligible_targets[&(edge.source, edge.target)])
        .collect::<BTreeSet<_>>();
    let cell_count = usize::from(match_config.map_width)
        .checked_mul(usize::from(match_config.map_height))
        .ok_or_else(|| "map cell count overflow".to_string())?;
    let outside_depths =
        masked_wave_depths(ctx, &match_config, target_cells, &first_ring, cell_count)?;

    let selected_cells = boundary_cells.iter().copied().collect::<Vec<_>>();
    let commitments =
        perimeter_commitments(ctx, player_id, selection, &boundary_cells, commitment_bps)?;
    let requested = commitments.values().try_fold(0_u64, |total, commitment| {
        total
            .checked_add(*commitment)
            .ok_or_else(|| "attack requested infantry overflow".to_string())
    })?;
    if requested == 0 {
        return Err("the shared attack fronts have no uncommitted infantry".into());
    }
    Ok(PlannedExpansion {
        kind: OrderKind::AttackClusters,
        selected_cells,
        outside_depths,
        focus_cell_id: None,
        target_cells: target_cells.iter().copied().collect(),
        commitments,
        requested,
    })
}

fn perimeter_commitments(
    ctx: &ReducerContext,
    player_id: u16,
    selection: &RetaskSelection,
    perimeter_cells: &BTreeSet<u32>,
    commitment_bps: u32,
) -> Result<BTreeMap<u32, u64>, String> {
    perimeter_cells
        .iter()
        .copied()
        .map(|cell_id| {
            let cell = core_cell(ctx, cell_id)?;
            let allocated = allocated_infantry_at_cell(ctx, player_id, cell_id);
            let available = available_after_retask_release(
                cell.force(),
                allocated,
                selection.released_at(cell_id),
            )?;
            Ok((cell_id, basis_point_share(available, commitment_bps)))
        })
        .collect()
}

fn outside_wave_depths(
    ctx: &ReducerContext,
    match_config: &crate::schema::MatchConfig,
    player_id: u16,
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

/// Static multi-source distances constrained to the snapshotted enemy mask.
/// Ownership is intentionally not consulted after validation: captures may
/// turn cells friendly while the same wave continues through the target.
fn masked_wave_depths(
    ctx: &ReducerContext,
    match_config: &crate::schema::MatchConfig,
    target_cells: &BTreeSet<u32>,
    first_ring: &BTreeSet<u32>,
    cell_count: usize,
) -> Result<Vec<u16>, String> {
    let mut depths = vec![u16::MAX; cell_count];
    let mut pending = VecDeque::new();
    for &cell_id in first_ring {
        if !target_cells.contains(&cell_id) {
            return Err(format!(
                "attack first-ring cell {cell_id} is outside the target mask"
            ));
        }
        let index =
            usize::try_from(cell_id).map_err(|_| "cell id does not fit usize".to_string())?;
        let depth = depths
            .get_mut(index)
            .ok_or_else(|| format!("attack first-ring cell {cell_id} is outside the map"))?;
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
            if !target_cells.contains(&neighbor_id) || depths[neighbor_id as usize] != u16::MAX {
                continue;
            }
            let target = terrain(ctx, neighbor_id)?;
            if !target.passable
                || !target.capturable
                || edge_runtime_limits(ctx, current_id, neighbor_id)?.is_none()
            {
                continue;
            }
            let depth = current_depth
                .checked_add(1)
                .filter(|depth| *depth != u16::MAX)
                .ok_or_else(|| "attack target depth overflow".to_string())?;
            depths[neighbor_id as usize] = depth;
            pending.push_back(neighbor_id);
        }
    }
    validate_attack_target_reachability(target_cells, &depths)?;
    Ok(depths)
}

fn validate_attack_target_reachability(
    target_cells: &BTreeSet<u32>,
    depths: &[u16],
) -> Result<(), String> {
    let unreachable = target_cells.iter().copied().find(|cell_id| {
        usize::try_from(*cell_id)
            .ok()
            .and_then(|index| depths.get(index))
            .is_none_or(|depth| *depth == u16::MAX)
    });
    if let Some(cell_id) = unreachable {
        return Err(format!(
            "every targeted enemy cluster must share a passable front with the selected source clusters; target cell {cell_id} is unreachable"
        ));
    }
    Ok(())
}

fn expand_target_is_eligible(
    target_owner: u16,
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

#[cfg(test)]
fn aggregate_lane_allocations_rotated(
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

#[cfg(test)]
type LaneAllocations = Vec<(usize, usize, u64)>;

fn expand_selection_message(error: FrontSelectionError) -> String {
    match error {
        FrontSelectionError::EmptySelection => "expand selection is empty",
        FrontSelectionError::NoEligibleFront => {
            "the selected regions have no adjacent neutral passable ground"
        }
        FrontSelectionError::InvalidDirection => "invalid all-front expansion boundary",
    }
    .into()
}

fn attack_cluster_selection_message(error: FrontSelectionError) -> String {
    match error {
        FrontSelectionError::EmptySelection => "cluster attack source selection is empty",
        FrontSelectionError::NoEligibleFront => {
            "the selected source and enemy clusters have no shared passable front"
        }
        FrontSelectionError::InvalidDirection => "invalid cluster attack boundary",
    }
    .into()
}

fn front_selection_message(error: FrontSelectionError) -> String {
    match error {
        FrontSelectionError::EmptySelection => "push selection is empty",
        FrontSelectionError::InvalidDirection => {
            "push direction must be one of the six adjacent hex directions"
        }
        FrontSelectionError::NoEligibleFront => {
            "the selected regions have no passable lane in that direction"
        }
    }
    .into()
}

fn local_arc_selection_message(error: FrontSelectionError) -> String {
    match error {
        FrontSelectionError::EmptySelection => "push selection is empty",
        FrontSelectionError::NoEligibleFront => {
            "the selected regions have no adjacent hostile passable front"
        }
        FrontSelectionError::InvalidDirection => "invalid local hostile front boundary",
    }
    .into()
}

/// A Push may cross its selected boundary into either friendly territory or a
/// capturable non-friendly cell. Friendly ground is an internal endpoint, so
/// its capturability flag is irrelevant; passability and the runtime edge
/// constraint still apply to both cases.
fn initial_push_target_is_eligible(
    player_id: u16,
    target_owner: u16,
    passable: bool,
    capturable: bool,
    traversable: bool,
) -> bool {
    passable && traversable && (target_owner == player_id || capturable)
}

/// Automatic local-arc Push is deliberately contact-only. Neutral expansion
/// remains Shift+P, while friendly-facing movement remains the explicit
/// directional retreat gesture.
fn local_arc_target_is_eligible(
    player_id: u16,
    target_owner: u16,
    passable: bool,
    capturable: bool,
    traversable: bool,
) -> bool {
    target_owner != NEUTRAL_PLAYER
        && target_owner != player_id
        && passable
        && capturable
        && traversable
}

fn retreat_translation_leg(
    source: Axial,
    direction: Axial,
    selected: &BTreeSet<Axial>,
    coordinate_to_id: &BTreeMap<Axial, u32>,
    outside_target: u32,
) -> Option<(u32, Vec<u32>, bool)> {
    let source_id = coordinate_to_id.get(&source).copied()?;
    let destination = coordinate_to_id
        .get(&(source + direction))
        .copied()
        .unwrap_or(outside_target);
    let abandon_source = !selected.contains(&(source - direction));
    Some((destination, vec![source_id, destination], abandon_source))
}

fn front_route_tree(
    selected_map: &HexMap,
    boundary_sources: &BTreeSet<Axial>,
    direction: Axial,
    movement: &MovementConfig,
) -> FrontRouteTree {
    let sources = selected_map.coordinates().collect::<BTreeSet<_>>();
    let routes = selected_directional_routes(&sources, direction, boundary_sources, |from, to| {
        selected_map
            .get(from)
            .zip(selected_map.get(to))
            .is_some_and(|(from, to)| ground_traversal(from, to, movement).is_some())
    });
    let mut labels = BTreeMap::<Axial, (u64, Axial, Axial)>::new();
    for route in routes.values() {
        let Some(&boundary) = route.last() else {
            continue;
        };
        for (index, &coordinate) in route.iter().enumerate() {
            let cost = u64::try_from(route.len().saturating_sub(index + 1)).unwrap_or(u64::MAX);
            let next = route.get(index + 1).copied().unwrap_or(coordinate);
            labels.entry(coordinate).or_insert((cost, boundary, next));
        }
    }
    FrontRouteTree { labels }
}

fn plan_front_rebalance(
    ctx: &ReducerContext,
    player_id: u16,
    cluster: &BTreeSet<u32>,
    selection: &RetaskSelection,
    source_front_seed: u32,
    target_front_seed: u32,
    commitment_bps: u32,
) -> Result<(u64, Vec<PlannedLeg>), String> {
    if !(1..=10_000).contains(&commitment_bps) {
        return Err("front rebalance share must be between 1 and 10000 bps".into());
    }
    if source_front_seed == target_front_seed {
        return Err("front rebalance source and target seeds must differ".into());
    }
    if !cluster.contains(&source_front_seed) || !cluster.contains(&target_front_seed) {
        return Err("front rebalance seeds must lie in the selected component".into());
    }
    if cluster.len() > MAX_SELECTION_CELLS {
        return Err(format!(
            "front rebalance component exceeds the {MAX_SELECTION_CELLS}-cell command limit"
        ));
    }

    let mut coordinates_by_cell_id = BTreeMap::new();
    let mut cell_ids_by_coordinate = BTreeMap::new();
    let mut exterior_by_edge = BTreeMap::new();
    let mut component_coordinates = BTreeSet::new();
    for &cell_id in cluster {
        let terrain_row = terrain(ctx, cell_id)?;
        let state = cell_state(ctx, cell_id)?;
        if !terrain_row.passable || state.owner_player_id != player_id {
            return Err(format!(
                "front rebalance cell {cell_id} is not owned passable ground"
            ));
        }
        let coordinate = Axial::new(terrain_row.q, terrain_row.r);
        coordinates_by_cell_id.insert(cell_id, coordinate);
        cell_ids_by_coordinate.insert(coordinate, cell_id);
        component_coordinates.insert(coordinate);
    }

    // Classify every directed exterior edge. Off-map, blocked, uncapturable,
    // same-owner, and terrain-disconnected neighbors are not deployable fronts.
    let match_config = config(ctx)?;
    for &source in &component_coordinates {
        let source_id = cell_ids_by_coordinate[&source];
        for target in source.neighbors() {
            if component_coordinates.contains(&target) {
                continue;
            }
            let exterior = if let Some(target_id) =
                crate::rules::cell_id_for_coordinate(&match_config, target)
            {
                let target_terrain = terrain(ctx, target_id)?;
                let target_state = cell_state(ctx, target_id)?;
                if !target_terrain.passable
                    || !target_terrain.capturable
                    || edge_runtime_limits(ctx, source_id, target_id)?.is_none()
                    || target_state.owner_player_id == player_id
                {
                    StrategicExterior::Ignored
                } else if target_state.owner_player_id == NEUTRAL_PLAYER {
                    StrategicExterior::Neutral
                } else {
                    StrategicExterior::Opponent(u32::from(target_state.owner_player_id))
                }
            } else {
                StrategicExterior::Ignored
            };
            exterior_by_edge.insert((source, target), exterior);
        }
    }

    let fronts = strategic_fronts(component_coordinates.iter().copied(), |source, target| {
        exterior_by_edge
            .get(&(source, target))
            .copied()
            .unwrap_or(StrategicExterior::Ignored)
    })
    .map_err(|_| "front rebalance component has no boundary".to_string())?;
    if fronts.len() < 2 {
        return Err("front rebalance needs at least two strategic fronts on the component".into());
    }

    let source_seed_coord = coordinates_by_cell_id[&source_front_seed];
    let target_seed_coord = coordinates_by_cell_id[&target_front_seed];
    let source_index =
        strategic_front_index_for_seed(&fronts, source_seed_coord).ok_or_else(|| {
            "source front seed is not on a strategic front boundary of the component".to_string()
        })?;
    let target_index =
        strategic_front_index_for_seed(&fronts, target_seed_coord).ok_or_else(|| {
            "target front seed is not on a strategic front boundary of the component".to_string()
        })?;
    if source_index == target_index {
        return Err("source and target seeds resolve to the same strategic front".into());
    }

    let source_front = &fronts[source_index];
    let target_front = &fronts[target_index];
    let mut source_front_cells = front_cell_ids(source_front, &cell_ids_by_coordinate)?;
    let target_front_cells = front_cell_ids(target_front, &cell_ids_by_coordinate)?;
    // A corner cell may expose edges belonging to both arcs. It is already at
    // the target front, so keep it stationary instead of rejecting the whole
    // command or creating an overlapping source/destination leg.
    source_front_cells.retain(|cell_id| !target_front_cells.contains(cell_id));
    if source_front_cells.is_empty() {
        return Err("source front has no cells outside the target front".into());
    }

    let reservations =
        active_destination_reservations(ctx, player_id, &selection.superseded_order_ids);
    reject_active_internal_destination_overlap(
        "front rebalance destination",
        &target_front_cells,
        &reservations,
    )?;

    let edge_counts = target_front.edge_count_by_source();
    let mut map = HexMap::new();
    let mut source_limits = BTreeMap::new();
    let mut total_supply = 0_u64;
    let mut physical_headroom_by_cell = BTreeMap::new();

    for &cell_id in cluster {
        let mut cell = core_cell(ctx, cell_id)?;
        let current = cell.force();
        let allocated = allocated_infantry_at_cell(ctx, player_id, cell_id);
        let projected = redistribution_cell_projection(
            current,
            cell.military_capacity,
            allocated,
            selection.released_at(cell_id),
        )?;
        physical_headroom_by_cell.insert(cell_id, cell.military_capacity - current);
        if source_front_cells.contains(&cell_id) {
            let share = basis_point_share(projected.affected, commitment_bps);
            if share > 0 {
                source_limits.insert(cell_id, share);
                total_supply = total_supply
                    .checked_add(share)
                    .ok_or_else(|| "front rebalance supply overflow".to_string())?;
            }
        }
        cell.forces.infantry = 0;
        cell.military_capacity = projected.residual_capacity.max(1);
        map.insert(cell);
    }
    if total_supply == 0 {
        return Err("source front has no movable troops for the requested share".into());
    }

    // Cross-front strategy is explicit: this command moves Share to one chosen
    // front. Inside that front, exposed edge count and physical headroom weight
    // placement. Existing troops still occupy capacity and are never overbooked.
    let mut target_ids = Vec::new();
    let mut target_coordinates = Vec::new();
    let mut target_capacities = Vec::new();
    let mut target_weights = Vec::new();
    for &cell_id in &target_front_cells {
        let coordinate = coordinates_by_cell_id[&cell_id];
        let headroom = physical_headroom_by_cell[&cell_id]
            .saturating_sub(reservations.get(&cell_id).copied().unwrap_or(0));
        if headroom == 0 {
            continue;
        }
        target_ids.push(cell_id);
        target_coordinates.push(coordinate);
        target_capacities.push(headroom);
        target_weights.push(edge_counts.get(&coordinate).copied().unwrap_or(0).max(1));
    }
    let total_headroom = target_capacities
        .iter()
        .try_fold(0_u64, |total, capacity| {
            total
                .checked_add(*capacity)
                .ok_or_else(|| "front rebalance capacity overflow".to_string())
        })?;
    let deliverable = total_supply.min(total_headroom);
    if deliverable == 0 {
        return Err("target front cannot accept any of the requested share".into());
    }
    let target_distribution = redistribution_targets_dense_with_weights(
        &target_coordinates,
        &target_capacities,
        deliverable,
        target_weights,
    )
    .map_err(|error| format!("invalid front target distribution: {error:?}"))?;
    let demands = target_ids
        .into_iter()
        .zip(target_distribution.targets)
        .filter(|(_, amount)| *amount > 0)
        .collect::<BTreeMap<_, _>>();

    // If the target saturates, reduce every source proportionally with the same
    // deterministic largest-remainder allocator rather than privileging a cell.
    if deliverable < total_supply {
        let source_entries = source_limits.into_iter().collect::<Vec<_>>();
        let source_coordinates = source_entries
            .iter()
            .map(|(cell_id, _)| coordinates_by_cell_id[cell_id])
            .collect::<Vec<_>>();
        let source_capacities = source_entries
            .iter()
            .map(|(_, amount)| *amount)
            .collect::<Vec<_>>();
        let source_distribution = redistribution_targets_dense_with_weights(
            &source_coordinates,
            &source_capacities,
            deliverable,
            vec![UNIFORM_ALLOCATION_WEIGHT; source_entries.len()],
        )
        .map_err(|error| format!("invalid front source distribution: {error:?}"))?;
        source_limits = source_entries
            .into_iter()
            .map(|(cell_id, _)| cell_id)
            .zip(source_distribution.targets)
            .filter(|(_, amount)| *amount > 0)
            .collect();
    }

    let plan = PlannedDistribution {
        map,
        cell_ids_by_coordinate,
        coordinates_by_cell_id,
        source_limits,
        demands,
        amount: deliverable,
    };
    let legs = plan_distribution_legs(ctx, player_id, selection, plan, None)?;
    if legs.is_empty() {
        return Err("front rebalance could not route any troops".into());
    }
    let routed = legs.iter().try_fold(0_u64, |total, leg| {
        total
            .checked_add(leg.amount)
            .ok_or_else(|| "front rebalance route overflow".to_string())
    })?;
    if routed == 0 {
        return Err("front rebalance could not route any troops".into());
    }
    Ok((routed, legs))
}

fn front_cell_ids(
    front: &StrategicFront,
    cell_ids_by_coordinate: &BTreeMap<Axial, u32>,
) -> Result<BTreeSet<u32>, String> {
    front
        .source_cells()
        .into_iter()
        .map(|coordinate| {
            cell_ids_by_coordinate
                .get(&coordinate)
                .copied()
                .ok_or_else(|| {
                    format!("front cell {coordinate:?} is missing from the component map")
                })
        })
        .collect()
}

fn shape_distribution_plan(
    ctx: &ReducerContext,
    player_id: u16,
    selection: &RetaskSelection,
    target_cells: &[u32],
) -> Result<PlannedDistribution, String> {
    // The reducer validates the global destination before partitioning. A
    // particular owned source component may legitimately have no reachable
    // destination; its projected affected strength then remains unchanged.
    let targets = target_cells.iter().copied().collect::<BTreeSet<_>>();
    let all_cells = selection
        .source_cells
        .union(&targets)
        .copied()
        .collect::<BTreeSet<_>>();
    if all_cells.len() > MAX_SELECTION_CELLS {
        return Err(format!(
            "reshape source and destination exceed the {MAX_SELECTION_CELLS}-cell command limit"
        ));
    }

    let mut map = HexMap::new();
    let mut by_coordinate = BTreeMap::new();
    let mut fixed_by_cell = BTreeMap::new();
    let mut target_membership = BTreeMap::new();
    let mut total = 0_u64;
    for cell_id in all_cells {
        let terrain_row = terrain(ctx, cell_id)?;
        let mut cell = core_cell(ctx, cell_id)?;
        if !terrain_row.passable || cell.owner != Some(u32::from(player_id)) {
            return Err(format!(
                "reshape cell {cell_id} is not owned passable ground"
            ));
        }
        let is_source = selection.source_cells.contains(&cell_id);
        let (affected, fixed, residual_capacity) = if is_source {
            let allocated = allocated_infantry_at_cell(ctx, player_id, cell_id);
            let projected = redistribution_cell_projection(
                cell.force(),
                cell.military_capacity,
                allocated,
                selection.released_at(cell_id),
            )?;
            (
                projected.affected,
                projected.unaffected,
                projected.residual_capacity,
            )
        } else {
            (
                0,
                cell.force(),
                cell.military_capacity.saturating_sub(cell.force()),
            )
        };
        total = total
            .checked_add(affected)
            .ok_or_else(|| "reshape strength overflow".to_string())?;
        let coordinate = cell.coordinate;
        cell.forces.infantry = affected;
        cell.military_capacity = residual_capacity;
        by_coordinate.insert(coordinate, cell_id);
        fixed_by_cell.insert(cell_id, fixed);
        target_membership.insert(coordinate, targets.contains(&cell_id));
        map.insert(cell);
    }
    let shape_targets = best_effort_shape_targets(&map, player_id, &target_membership, total)?;

    let reservations =
        active_destination_reservations(ctx, player_id, &selection.superseded_order_ids);
    let mut source_limits = BTreeMap::new();
    let mut demands = BTreeMap::new();
    let mut total_demand = 0_u64;
    for (coordinate, affected_target) in shape_targets {
        let cell_id = by_coordinate[&coordinate];
        let current = cell_state(ctx, cell_id)?.infantry;
        let target = fixed_by_cell[&cell_id]
            .checked_add(affected_target)
            .ok_or_else(|| "reshape target overflow".to_string())?;
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
        map,
        cell_ids_by_coordinate: by_coordinate.clone(),
        coordinates_by_cell_id: by_coordinate
            .into_iter()
            .map(|(coordinate, cell_id)| (cell_id, coordinate))
            .collect(),
        source_limits,
        demands,
        amount: total_demand,
    })
}

/// Fills the drawn shape with every affected unit that fits. Non-target source
/// cells have zero preference and retain only overflow, clamped to their
/// current affected strength. With enough drawn capacity this remains an exact
/// shape and drains excluded sources; a component without a target is a no-op.
fn best_effort_shape_targets(
    map: &HexMap,
    player_id: u16,
    target_membership: &BTreeMap<Axial, bool>,
    total: u64,
) -> Result<BTreeMap<Axial, u64>, String> {
    let weights = target_membership
        .iter()
        .map(|(&coordinate, &is_target)| {
            (
                coordinate,
                if is_target {
                    UNIFORM_ALLOCATION_WEIGHT
                } else {
                    0
                },
            )
        })
        .collect::<BTreeMap<_, _>>();
    let lower_bounds = weights
        .keys()
        .copied()
        .map(|coordinate| (coordinate, 0))
        .collect::<BTreeMap<_, _>>();
    let distribution = redistribution_targets_with_fallback_constraints(
        map,
        u32::from(player_id),
        weights,
        lower_bounds,
        total,
    )
    .map_err(|error| format!("invalid reshape: {error:?}"))?;
    debug_assert_eq!(distribution.unassigned, 0);
    Ok(distribution.targets)
}

fn active_destination_reservations(
    ctx: &ReducerContext,
    player_id: u16,
    excluded_order_ids: &BTreeSet<u64>,
) -> BTreeMap<u32, u64> {
    let mut reservations = BTreeMap::<u32, u64>::new();
    for destination in ctx.db.transfer_destination().iter() {
        let Some(order) = ctx
            .db
            .transfer_order()
            .order_id()
            .find(destination.order_id)
        else {
            continue;
        };
        if active_internal_reservation_is_relevant(&order, player_id, excluded_order_ids) {
            let remaining = destination
                .target_infantry
                .saturating_sub(destination.received_infantry);
            if remaining > 0 {
                let reservation = reservations.entry(destination.cell_id).or_default();
                *reservation = reservation.saturating_add(remaining);
            }
        }
    }
    reservations
}

fn active_internal_reservation_is_relevant(
    order: &TransferOrder,
    player_id: u16,
    excluded_order_ids: &BTreeSet<u64>,
) -> bool {
    order.status == OrderStatus::Active
        && order.player_id == player_id
        && !excluded_order_ids.contains(&order.order_id)
        && matches!(order.kind, OrderKind::Reshape | OrderKind::FrontRebalance)
}

fn reject_active_internal_destination_overlap(
    label: &str,
    command_cells: &BTreeSet<u32>,
    reservations: &BTreeMap<u32, u64>,
) -> Result<(), String> {
    let overlap = command_cells.iter().find_map(|cell_id| {
        reservations
            .get(cell_id)
            .copied()
            .filter(|amount| *amount > 0)
            .map(|amount| (*cell_id, amount))
    });
    let Some((cell_id, amount)) = overlap else {
        return Ok(());
    };
    Err(format!(
        "{label} overlaps cell {cell_id}, which still has {amount} infantry reserved by another active internal order; stop that order or wait for it to finish"
    ))
}

fn basis_point_share(value: u64, basis_points: u32) -> u64 {
    (u128::from(value) * u128::from(basis_points) / 10_000) as u64
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RedistributionCellProjection {
    affected: u64,
    unaffected: u64,
    residual_capacity: u64,
}

fn redistribution_cell_projection(
    current: u64,
    military_capacity: u64,
    allocated: u64,
    superseded: u64,
) -> Result<RedistributionCellProjection, String> {
    if current > military_capacity {
        return Err("redistribution cell strength exceeds military capacity".into());
    }
    let unaffected = unaffected_after_retask_release(current, allocated, superseded)?;
    Ok(RedistributionCellProjection {
        affected: current - unaffected,
        unaffected,
        residual_capacity: military_capacity - unaffected,
    })
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

fn exact_order_ids(order_ids: &[u64]) -> Result<BTreeSet<u64>, String> {
    if order_ids.is_empty() {
        return Err("order ID selection is empty".into());
    }
    if order_ids.len() > MAX_SELECTION_CELLS {
        return Err(format!(
            "order ID selection exceeds the {MAX_SELECTION_CELLS}-order command limit"
        ));
    }
    let unique = order_ids.iter().copied().collect::<BTreeSet<_>>();
    if unique.len() != order_ids.len() {
        return Err("order ID selection contains duplicates".into());
    }
    Ok(unique)
}

fn plan_distribution_legs(
    ctx: &ReducerContext,
    player_id: u16,
    selection: &RetaskSelection,
    plan: PlannedDistribution,
    max_legs: Option<usize>,
) -> Result<Vec<PlannedLeg>, String> {
    let PlannedDistribution {
        map,
        cell_ids_by_coordinate,
        coordinates_by_cell_id,
        source_limits,
        demands: mut destination_demands,
        amount: requested,
    } = plan;
    if source_limits.is_empty() {
        return Err("source selection is empty".into());
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
        let available_after_release = available_after_retask_release(
            cell.infantry,
            allocated,
            selection.released_at(source),
        )?;
        let available = source_limit.min(available_after_release);
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
        .map(|cell_id| {
            coordinates_by_cell_id
                .get(&cell_id)
                .copied()
                .map(|coordinate| (cell_id, coordinate))
                .ok_or_else(|| format!("distribution cell {cell_id} is missing from its map"))
        })
        .collect::<Result<BTreeMap<_, _>, String>>()?;
    let match_config = config(ctx)?;
    let movement = MovementConfig {
        max_elevation_step: u16::from(match_config.max_elevation_step),
        level_cost: 10,
        uphill_cost: 15,
        downhill_cost: 10,
    };
    let mut legs = Vec::new();
    let mut remaining = requested;

    let mut capped = false;
    'sources: for (&source, source_available) in &mut available_by_source {
        if remaining == 0 {
            break;
        }
        let source_coordinate = coordinates_by_cell_id
            .get(&source)
            .copied()
            .ok_or_else(|| format!("distribution cell {source} is missing from its map"))?;
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
            if max_legs.is_some_and(|limit| legs.len() >= limit) {
                capped = true;
                break 'sources;
            }
            if *source_available == 0 || remaining == 0 {
                break;
            }
            let demand = destination_demands.get(&destination).copied().unwrap_or(0);
            if demand == 0 {
                continue;
            }
            let Some(path) = shortest_path(
                &map,
                source_coordinate,
                destination_coordinates[&destination],
                &movement,
                |cell| cell.owner == Some(u32::from(player_id)),
            ) else {
                continue;
            };
            let route = path
                .cells
                .into_iter()
                .map(|coordinate| {
                    cell_ids_by_coordinate
                        .get(&coordinate)
                        .copied()
                        .ok_or_else(|| {
                            format!("route coordinate {coordinate:?} is missing from its map")
                        })
                })
                .collect::<Result<Vec<_>, String>>()?;
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
    if remaining != 0 && !capped {
        return Err("not all redistribution demand can be routed".into());
    }
    Ok(legs)
}

#[allow(clippy::too_many_arguments)]
fn persist_order(
    ctx: &ReducerContext,
    player_id: u16,
    client_command_id: u64,
    kind: OrderKind,
    requested: u64,
    orientation: Axial,
    legs: Vec<PlannedLeg>,
) -> Result<u64, String> {
    let prepared = prepare_order_persistence(ctx, requested, legs)?;
    Ok(persist_prepared_order(
        ctx,
        player_id,
        client_command_id,
        kind,
        orientation,
        prepared,
    ))
}

fn prepare_order_persistence(
    ctx: &ReducerContext,
    requested: u64,
    legs: Vec<PlannedLeg>,
) -> Result<PreparedOrderPersistence, String> {
    let legs = coalesced_order_legs(legs)?;
    let committed = legs.iter().try_fold(0_u64, |total, leg| {
        total
            .checked_add(leg.amount)
            .ok_or_else(|| "order committed infantry overflow".to_string())
    })?;
    if committed == 0 {
        return Err("order has no committed infantry".into());
    }

    let logical_step = state(ctx)?.logical_step;
    let mut source_totals = BTreeMap::<u32, u64>::new();
    let mut destination_totals = BTreeMap::<u32, u64>::new();
    for leg in &legs {
        let source_total = source_totals.entry(leg.source).or_default();
        *source_total = source_total
            .checked_add(leg.amount)
            .ok_or_else(|| "order source infantry overflow".to_string())?;
        let destination_total = destination_totals.entry(leg.destination).or_default();
        *destination_total = destination_total
            .checked_add(leg.amount)
            .ok_or_else(|| "order destination infantry overflow".to_string())?;
    }
    Ok(PreparedOrderPersistence {
        logical_step,
        requested,
        committed,
        legs,
        source_totals,
        destination_totals,
    })
}

fn coalesced_order_legs(legs: Vec<PlannedLeg>) -> Result<Vec<PlannedLeg>, String> {
    let mut coalesced = BTreeMap::<(u32, u32), PlannedLeg>::new();
    for leg in legs {
        let key = (leg.source, leg.destination);
        if let Some(existing) = coalesced.get_mut(&key) {
            if existing.route != leg.route {
                return Err(format!(
                    "order has conflicting routes from {} to {}",
                    leg.source, leg.destination
                ));
            }
            existing.amount = existing
                .amount
                .checked_add(leg.amount)
                .ok_or_else(|| "coalesced order leg overflow".to_string())?;
        } else {
            coalesced.insert(key, leg);
        }
    }
    Ok(coalesced.into_values().collect())
}

/// Persists values that have already passed every recoverable check. Database
/// inserts are infallible inside the surrounding reducer transaction, so this
/// stage is deliberately non-`Result`; callers may safely cancel a superseded
/// order immediately before committing the prepared replacement.
fn persist_prepared_order(
    ctx: &ReducerContext,
    player_id: u16,
    client_command_id: u64,
    kind: OrderKind,
    orientation: Axial,
    prepared: PreparedOrderPersistence,
) -> u64 {
    let PreparedOrderPersistence {
        logical_step,
        requested,
        committed,
        legs,
        source_totals,
        destination_totals,
    } = prepared;
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

    for leg in legs {
        let route = ctx.db.transit_route().insert(TransitRoute {
            route_id: 0,
            order_id: order.order_id,
            player_id,
            cells: leg.route,
        });
        ctx.db.transit_packet().insert(TransitPacket {
            packet_key: 0,
            order_id: order.order_id,
            owner_player_id: player_id,
            origin_cell: leg.source,
            current_cell: leg.source,
            destination_cell: leg.destination,
            infantry: leg.amount,
            pending_source_infantry: leg.amount,
            route_id: route.route_id,
            route_index: 0,
            updated_step: logical_step,
        });
    }
    for (cell_id, infantry) in source_totals {
        ctx.db.transfer_source().insert(TransferSource {
            source_key: order_cell_key(order.order_id, cell_id),
            order_id: order.order_id,
            player_id,
            cell_id,
            committed_infantry: infantry,
            queued_infantry: infantry,
        });
    }
    for (cell_id, infantry) in destination_totals {
        ctx.db.transfer_destination().insert(TransferDestination {
            destination_key: order_cell_key(order.order_id, cell_id),
            order_id: order.order_id,
            player_id,
            cell_id,
            target_infantry: infantry,
            received_infantry: 0,
        });
    }
    order.order_id
}

fn persist_retreat_abandonments(ctx: &ReducerContext, order_id: u64, abandonments: &BTreeSet<u32>) {
    for &cell_id in abandonments {
        ctx.db.retreat_abandonment().insert(RetreatAbandonment {
            abandonment_key: order_cell_key(order_id, cell_id),
            order_id,
            cell_id,
        });
    }
}

fn persist_expand_order(
    ctx: &ReducerContext,
    player_id: u16,
    client_command_id: u64,
    plan: PlannedExpansion,
) -> Result<u64, String> {
    if plan.requested == 0 {
        return Err("expand order has no committed infantry".into());
    }
    if !matches!(
        plan.kind,
        OrderKind::ExpandAll | OrderKind::ExpandClusters | OrderKind::AttackClusters
    ) {
        return Err("expand topology received an invalid order kind".into());
    }
    if plan.selected_cells.contains(&EXPANSION_AGGREGATE_ORIGIN) {
        return Err("map cell id collides with the expansion aggregate sentinel".into());
    }

    let logical_step = state(ctx)?.logical_step;
    let order = ctx.db.transfer_order().insert(TransferOrder {
        order_id: 0,
        player_id,
        client_command_id,
        kind: plan.kind,
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
            source_key: order_cell_key(order.order_id, cell_id),
            order_id: order.order_id,
            player_id,
            cell_id,
            committed_infantry: infantry,
            queued_infantry: infantry,
        });
        if infantry == 0 {
            continue;
        }
        ctx.db.transit_packet().insert(TransitPacket {
            packet_key: 0,
            order_id: order.order_id,
            owner_player_id: player_id,
            origin_cell: EXPANSION_AGGREGATE_ORIGIN,
            current_cell: cell_id,
            destination_cell: cell_id,
            infantry,
            pending_source_infantry: infantry,
            route_id: 0,
            route_index: 0,
            updated_step: logical_step,
        });
    }
    ctx.db.expansion_wave().insert(ExpansionWave {
        order_id: order.order_id,
        selected_cells: plan.selected_cells,
        split_cursors: vec![0; plan.outside_depths.len()],
        outside_depths: plan.outside_depths,
        focus_cell_id: plan.focus_cell_id,
        target_cells: plan.target_cells,
    });
    Ok(order.order_id)
}

fn cancel_superseded_orders(
    ctx: &ReducerContext,
    player_id: u16,
    order_ids: &BTreeSet<u64>,
) -> Result<(), String> {
    for &order_id in order_ids {
        cancel_order(ctx, player_id, order_id)?;
    }
    Ok(())
}

fn preflight_cancel_order(
    ctx: &ReducerContext,
    player_id: u16,
    order_id: u64,
) -> Result<(), String> {
    let order = ctx
        .db
        .transfer_order()
        .order_id()
        .find(order_id)
        .ok_or_else(|| format!("unknown order {order_id}"))?;
    validate_cancel_claim(order_id, player_id, order.player_id, order.status)?;
    let released = ctx
        .db
        .transit_packet()
        .packet_by_order()
        .filter(order_id)
        .try_fold(0_u64, |total, packet| {
            total
                .checked_add(packet.infantry)
                .ok_or_else(|| "cancelled order strength overflow".to_string())
        })?;
    cancelled_settled_strength(
        order.committed_infantry,
        order.delivered_infantry,
        order.casualty_infantry,
        released,
    )?;
    Ok(())
}

fn validate_cancel_claim(
    order_id: u64,
    requester_player_id: u16,
    order_player_id: u16,
    status: OrderStatus,
) -> Result<(), String> {
    if order_player_id != requester_player_id {
        return Err(format!("order {order_id} belongs to the other player"));
    }
    if status != OrderStatus::Active {
        return Err(format!("order {order_id} is no longer active"));
    }
    Ok(())
}

fn cancel_order(ctx: &ReducerContext, player_id: u16, order_id: u64) -> Result<(), String> {
    preflight_cancel_order(ctx, player_id, order_id)?;
    let mut order = ctx
        .db
        .transfer_order()
        .order_id()
        .find(order_id)
        .ok_or_else(|| format!("unknown order {order_id}"))?;
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
            .delete(packet.packet_key);
    }
    let route_ids = ctx
        .db
        .transit_route()
        .route_by_order()
        .filter(order_id)
        .map(|route| route.route_id)
        .collect::<Vec<_>>();
    for route_id in route_ids {
        ctx.db.transit_route().route_id().delete(route_id);
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
        if let Some(mut source) = ctx.db.transfer_source().source_key().find(source_key) {
            source.queued_infantry = 0;
            ctx.db.transfer_source().source_key().update(source);
        }
    }
    ctx.db.expansion_wave().order_id().delete(order_id);
    let abandonment_keys = ctx
        .db
        .retreat_abandonment()
        .abandonment_by_order()
        .filter(order_id)
        .map(|abandonment| abandonment.abandonment_key)
        .collect::<Vec<_>>();
    for key in abandonment_keys {
        ctx.db.retreat_abandonment().abandonment_key().delete(key);
    }
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

    fn retask_packet(order_id: u64, current_cell: u32, infantry: u64) -> RetaskPacketSnapshot {
        RetaskPacketSnapshot {
            order_id,
            current_cell,
            infantry,
            current_cell_owned: true,
        }
    }

    fn reservation_order(
        order_id: u64,
        player_id: u16,
        kind: OrderKind,
        status: OrderStatus,
    ) -> TransferOrder {
        TransferOrder {
            order_id,
            player_id,
            client_command_id: order_id,
            kind,
            status,
            requested_infantry: 10,
            committed_infantry: 10,
            in_transit_infantry: 10,
            delivered_infantry: 0,
            casualty_infantry: 0,
            orientation_q: 0,
            orientation_r: 0,
            created_step: 0,
            updated_step: 0,
        }
    }

    #[test]
    fn whole_order_retask_deduplicates_ids_and_credits_every_physical_origin() {
        let requested_ids = [7, 7, 8].into_iter().collect::<BTreeSet<_>>();
        let packets = vec![
            retask_packet(7, 10, 30),
            retask_packet(7, 11, 20),
            retask_packet(8, 11, 5),
            retask_packet(8, 12, 15),
            retask_packet(9, 99, 100),
        ];
        let selection = resolve_retask_snapshot(BTreeSet::from([1]), &requested_ids, &packets)
            .expect("both requested orders have owned surviving packets");

        assert_eq!(selection.superseded_order_ids, BTreeSet::from([7, 8]));
        assert_eq!(selection.source_cells, BTreeSet::from([1, 10, 11, 12]));
        assert_eq!(
            selection.released_by_cell,
            BTreeMap::from([(10, 30), (11, 25), (12, 15)])
        );
        assert_eq!(selection.released_by_cell.values().sum::<u64>(), 70);

        // Cell 11 also has 15 strength reserved by an unrelated order. The
        // superseded 25 is released before applying 50%, so 32 of the now-65
        // available strength is committed without touching that reservation.
        let available = available_after_retask_release(80, 40, selection.released_at(11)).unwrap();
        let replacement = basis_point_share(available, 5_000);
        assert_eq!((available, replacement), (65, 32));
        let allocated_after_replacement = 40 - selection.released_at(11) + replacement;
        assert_eq!(allocated_after_replacement, 47);
        assert!(allocated_after_replacement <= 80);
    }

    #[test]
    fn failed_retask_preparation_leaves_the_old_packet_snapshot_unchanged() {
        let requested_ids = BTreeSet::from([7]);
        let packets = vec![retask_packet(7, 10, 30), retask_packet(7, 11, 20)];
        let old_packets = packets.clone();

        let prepared = resolve_retask_snapshot(BTreeSet::new(), &requested_ids, &packets).and_then(
            |selection| {
                validate_basis_points(0, "replacement commitment")?;
                Ok(selection)
            },
        );

        assert!(prepared.is_err());
        assert_eq!(packets, old_packets);
    }

    #[test]
    fn retask_claim_rejects_foreign_inactive_and_empty_orders() {
        assert!(validate_superseded_order_claim(7, 1, 2, OrderStatus::Active, true).is_err());
        assert!(validate_superseded_order_claim(7, 1, 1, OrderStatus::Completed, true).is_err());
        assert!(validate_superseded_order_claim(7, 1, 1, OrderStatus::Cancelled, true).is_err());
        assert!(validate_superseded_order_claim(7, 1, 1, OrderStatus::Active, false).is_err());
        assert!(validate_superseded_order_claim(7, 1, 1, OrderStatus::Active, true).is_ok());

        assert!(
            resolve_retask_snapshot(
                BTreeSet::new(),
                &BTreeSet::from([7, 8]),
                &[retask_packet(7, 10, 30)],
            )
            .is_err()
        );
    }

    #[test]
    fn initial_push_accepts_friendly_non_capturable_ground_but_keeps_hard_constraints() {
        assert!(initial_push_target_is_eligible(1, 1, true, false, true));
        assert!(initial_push_target_is_eligible(1, 0, true, true, true));
        assert!(initial_push_target_is_eligible(1, 2, true, true, true));

        assert!(!initial_push_target_is_eligible(1, 0, true, false, true));
        assert!(!initial_push_target_is_eligible(1, 2, true, false, true));
        assert!(!initial_push_target_is_eligible(1, 1, false, false, true));
        assert!(!initial_push_target_is_eligible(1, 1, true, false, false));
    }

    #[test]
    fn local_arc_push_targets_only_hostile_contact() {
        assert!(local_arc_target_is_eligible(1, 2, true, true, true));

        assert!(!local_arc_target_is_eligible(
            1,
            NEUTRAL_PLAYER,
            true,
            true,
            true,
        ));
        assert!(!local_arc_target_is_eligible(1, 1, true, true, true));
        assert!(!local_arc_target_is_eligible(1, 2, false, true, true));
        assert!(!local_arc_target_is_eligible(1, 2, true, false, true));
        assert!(!local_arc_target_is_eligible(1, 2, true, true, false));
    }

    #[test]
    fn local_arc_error_does_not_describe_neutral_expansion_or_a_global_direction() {
        assert_eq!(
            local_arc_selection_message(FrontSelectionError::NoEligibleFront),
            "the selected regions have no adjacent hostile passable front"
        );
    }

    #[test]
    fn friendly_push_boundary_is_valid_on_all_six_hex_axes() {
        let source = Axial::ZERO;
        let sources = BTreeSet::from([source]);

        for direction in Axial::DIRECTIONS {
            let edges = selected_front_edges(&sources, direction, |_, _| {
                initial_push_target_is_eligible(1, 1, true, false, true)
            })
            .expect("friendly ground is a legal boundary in every axial direction");
            assert_eq!(
                edges,
                vec![hex_core::DirectedFrontEdge {
                    source,
                    target: source + direction,
                }]
            );
        }
    }

    #[test]
    fn friendly_push_translates_every_selected_column_one_hex_on_all_six_axes() {
        for direction in Axial::DIRECTIONS {
            let trailing = Axial::ZERO;
            let middle = trailing + direction;
            let boundary = middle + direction;
            let selected = BTreeSet::from([trailing, middle, boundary]);
            let ids = BTreeMap::from([(trailing, 10), (middle, 11), (boundary, 12)]);

            assert_eq!(
                retreat_translation_leg(trailing, direction, &selected, &ids, 13),
                Some((11, vec![10, 11], true))
            );
            assert_eq!(
                retreat_translation_leg(middle, direction, &selected, &ids, 13),
                Some((12, vec![11, 12], false))
            );
            assert_eq!(
                retreat_translation_leg(boundary, direction, &selected, &ids, 13),
                Some((13, vec![12, 13], false))
            );
        }
    }

    #[test]
    fn one_directional_push_can_mix_friendly_and_non_friendly_boundary_lanes() {
        let upper = Axial::new(0, -1);
        let lower = Axial::new(0, 1);
        let sources = BTreeSet::from([upper, lower]);
        let direction = Axial::new(1, 0);
        let edges = selected_front_edges(&sources, direction, |source, _| {
            let owner = if source == upper { 1 } else { 2 };
            initial_push_target_is_eligible(1, owner, true, owner != 1, true)
        })
        .expect("friendly and hostile lanes are both legal Push boundaries");

        assert_eq!(edges.len(), 2);
        assert_eq!(edges[0].target, upper + direction);
        assert_eq!(edges[1].target, lower + direction);
    }

    #[test]
    fn no_front_error_describes_a_blocked_lane_without_assuming_ownership() {
        assert_eq!(
            front_selection_message(FrontSelectionError::NoEligibleFront),
            "the selected regions have no passable lane in that direction"
        );
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
            Axial::new(1, 0),
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
    fn front_route_tree_keeps_parallel_lanes_separate() {
        let lower_rear = Axial::new(0, 0);
        let lower_front = Axial::new(2, 0);
        let upper_rear = Axial::new(0, 1);
        let upper_front = Axial::new(2, 1);
        let mut map = HexMap::new();
        for r in 0..=1 {
            for q in 0..=2 {
                map.insert(selected_cell(Axial::new(q, r), 0));
            }
        }
        let routes = front_route_tree(
            &map,
            &BTreeSet::from([lower_front, upper_front]),
            Axial::new(1, 0),
            &MovementConfig::default(),
        );

        assert_eq!(
            routes.route_to_boundary(lower_rear),
            Some((lower_front, vec![lower_rear, Axial::new(1, 0), lower_front]))
        );
        assert_eq!(
            routes.route_to_boundary(upper_rear),
            Some((upper_front, vec![upper_rear, Axial::new(1, 1), upper_front]))
        );
    }

    #[test]
    fn separated_directional_arcs_do_not_pull_sideways_sources() {
        let coordinates = (-2..=2).map(|r| Axial::new(0, r)).collect::<BTreeSet<_>>();
        let direction = Axial::new(1, 0);
        let edges = selected_front_edges(&coordinates, direction, |source, _| {
            source.r.unsigned_abs() == 2
        })
        .expect("separated eligible arcs are one directional push");
        let front_seeds = edges
            .iter()
            .map(|edge| edge.source)
            .collect::<BTreeSet<_>>();

        let mut map = HexMap::new();
        for coordinate in &coordinates {
            map.insert(selected_cell(*coordinate, 0));
        }
        let routes = front_route_tree(&map, &front_seeds, direction, &MovementConfig::default());

        assert_eq!(
            front_seeds,
            BTreeSet::from([Axial::new(0, -2), Axial::new(0, 2)])
        );
        assert_eq!(routes.labels.len(), front_seeds.len());
        assert!(routes.route_to_boundary(Axial::new(0, 0)).is_none());
    }

    #[test]
    fn separated_front_seeds_cover_every_component_split_by_an_internal_cliff() {
        let lower_rear = Axial::new(0, 0);
        let lower_front = Axial::new(1, 0);
        let upper_rear = Axial::new(0, 2);
        let upper_front = Axial::new(1, 2);
        let coordinates = BTreeSet::from([lower_rear, lower_front, upper_rear, upper_front]);
        let direction = Axial::new(1, 0);
        let edges = selected_front_edges(&coordinates, direction, |source, _| {
            source == lower_front || source == upper_front
        })
        .expect("each cliff-separated component has an eligible directional arc");
        let front_seeds = edges
            .iter()
            .map(|edge| edge.source)
            .collect::<BTreeSet<_>>();

        let mut map = HexMap::new();
        map.insert(selected_cell(lower_rear, 0));
        map.insert(selected_cell(lower_front, 0));
        map.insert(selected_cell(upper_rear, 2));
        map.insert(selected_cell(upper_front, 2));
        let routes = front_route_tree(&map, &front_seeds, direction, &MovementConfig::default());

        assert_eq!(routes.labels.len(), coordinates.len());
        assert_eq!(
            routes.route_to_boundary(lower_rear),
            Some((lower_front, vec![lower_rear, lower_front]))
        );
        assert_eq!(
            routes.route_to_boundary(lower_front),
            Some((lower_front, vec![lower_front]))
        );
        assert_eq!(
            routes.route_to_boundary(upper_rear),
            Some((upper_front, vec![upper_rear, upper_front]))
        );
        assert_eq!(
            routes.route_to_boundary(upper_front),
            Some((upper_front, vec![upper_front]))
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
            Axial::new(1, 0),
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
    fn exact_cell_payload_limit_covers_the_largest_current_map_preset() {
        const LARGEST_PRESET_CAPTURABLE_CELLS: usize = 21_484;
        assert_eq!(MAX_SELECTION_CELLS, 32_768);

        let current_map = (0..LARGEST_PRESET_CAPTURABLE_CELLS as u32).collect::<Vec<_>>();
        assert_eq!(
            unique_selection(&current_map, "select all")
                .expect("the largest current map fits the exact payload")
                .len(),
            LARGEST_PRESET_CAPTURABLE_CELLS
        );

        let oversized = vec![0; MAX_SELECTION_CELLS + 1];
        assert_eq!(
            unique_selection(&oversized, "select all"),
            Err(format!(
                "select all selection exceeds the {MAX_SELECTION_CELLS}-cell command limit"
            ))
        );
    }

    #[test]
    fn redistribution_projection_releases_only_the_superseded_allocation() {
        assert_eq!(
            redistribution_cell_projection(80, 100, 40, 25),
            Ok(RedistributionCellProjection {
                affected: 65,
                unaffected: 15,
                residual_capacity: 85,
            })
        );
    }

    #[test]
    fn full_strength_reshape_can_contract_outside_the_source_footprint() {
        let source_a = Axial::new(-1, 0);
        let source_b = Axial::ZERO;
        let source_c = Axial::new(0, 1);
        let outside_target = Axial::new(1, 0);
        let mut map = HexMap::new();
        for (coordinate, affected, residual_capacity) in [
            (source_a, 30, 93),
            (source_b, 30, 96),
            (source_c, 30, 100),
            (outside_target, 0, 100),
        ] {
            let mut cell = selected_cell(coordinate, 0);
            cell.forces.infantry = affected;
            cell.military_capacity = residual_capacity;
            map.insert(cell);
        }
        let membership = map
            .coordinates()
            .map(|coordinate| (coordinate, coordinate == outside_target))
            .collect::<BTreeMap<_, _>>();

        let targets = best_effort_shape_targets(&map, 1, &membership, 90).unwrap();

        assert_eq!(targets[&outside_target], 90);
        assert_eq!(targets[&source_a], 0);
        assert_eq!(targets[&source_b], 0);
        assert_eq!(targets[&source_c], 0);
        // These fixed allocations are excluded from the projected map by the
        // reducer and are therefore the only final source occupancy.
        let fixed = BTreeMap::from([(source_a, 7), (source_b, 4), (source_c, 0)]);
        for source in [source_a, source_b, source_c] {
            assert_eq!(fixed[&source] + targets[&source], fixed[&source]);
        }
    }

    #[test]
    fn reshape_source_seeds_close_over_current_cluster_growth_and_merges() {
        // Cells 2 and 8 may have represented separate stale UI footprints;
        // current authority sees the bridge and newly grown perimeter cells.
        let current = BTreeSet::from([1, 2, 3, 4, 5, 6, 7, 8, 9]);
        let resolved = single_complete_seed_component(
            &BTreeSet::from([2, 8]),
            std::slice::from_ref(&current),
            "reshape source",
        )
        .unwrap();
        assert_eq!(resolved, current);
    }

    #[test]
    fn reshape_rejects_stale_sources_split_across_current_clusters() {
        let components = [BTreeSet::from([1, 2, 3]), BTreeSet::from([7, 8, 9])];
        assert!(
            single_complete_seed_component(&BTreeSet::from([2, 8]), &components, "reshape source",)
                .is_err()
        );
        assert!(
            single_complete_seed_component(
                &BTreeSet::from([2, 99]),
                &components,
                "reshape source",
            )
            .is_err(),
            "a lost or otherwise non-current seed must not be silently dropped"
        );
    }

    #[test]
    fn reshape_rejects_targets_and_legacy_retasks_outside_resolved_cluster() {
        let cluster = BTreeSet::from([1, 2, 3, 4]);
        assert!(
            validate_cells_within_cluster(&BTreeSet::from([2, 4]), &cluster, "reshape destination")
                .is_ok()
        );
        assert!(
            validate_cells_within_cluster(&BTreeSet::from([2, 5]), &cluster, "reshape destination")
                .is_err()
        );

        let confined = RetaskSelection {
            source_cells: cluster.clone(),
            superseded_order_ids: BTreeSet::from([7]),
            released_by_cell: BTreeMap::from([(3, 20)]),
        };
        assert!(validate_retask_within_cluster(&confined, &cluster, "reshape source").is_ok());

        let escaped = RetaskSelection {
            source_cells: cluster.union(&BTreeSet::from([9])).copied().collect(),
            superseded_order_ids: BTreeSet::from([7]),
            released_by_cell: BTreeMap::from([(9, 20)]),
        };
        assert!(validate_retask_within_cluster(&escaped, &cluster, "reshape source").is_err());
    }

    #[test]
    fn reshape_uses_all_free_strength_but_keeps_unretasked_action_packets_fixed() {
        let projected = redistribution_cell_projection(100, 100, 40, 0).unwrap();
        assert_eq!(projected.affected, 60);
        assert_eq!(projected.unaffected, 40);
        assert_eq!(projected.residual_capacity, 60);

        let legacy_retask = redistribution_cell_projection(100, 100, 40, 15).unwrap();
        assert_eq!(legacy_retask.affected, 75);
        assert_eq!(legacy_retask.unaffected, 25);
        assert_eq!(legacy_retask.residual_capacity, 75);
    }

    #[test]
    fn full_strength_reshape_can_expand_beyond_the_source_footprint() {
        let source = Axial::ZERO;
        let outside_targets = [Axial::new(1, 0), Axial::new(1, -1), Axial::new(0, -1)];
        let mut map = HexMap::new();
        let mut source_cell = selected_cell(source, 0);
        source_cell.forces.infantry = 90;
        source_cell.military_capacity = 95;
        map.insert(source_cell);
        for coordinate in outside_targets {
            map.insert(selected_cell(coordinate, 0));
        }
        let membership = map
            .coordinates()
            .map(|coordinate| (coordinate, outside_targets.contains(&coordinate)))
            .collect::<BTreeMap<_, _>>();

        let targets = best_effort_shape_targets(&map, 1, &membership, 90).unwrap();

        assert_eq!(targets[&source], 0);
        for target in outside_targets {
            assert_eq!(targets[&target], 30);
        }
        // The source had five infantry reserved by an unrelated allocation;
        // the reducer keeps that fixed pool and moves every affected unit.
        let fixed_source = 5;
        assert_eq!(fixed_source + targets[&source], fixed_source);
    }

    #[test]
    fn best_effort_reshape_saturates_a_small_shape_and_retains_exact_overflow() {
        let source_a = Axial::new(0, 0);
        let source_b = Axial::new(1, 0);
        let target = Axial::new(2, 0);
        let mut map = HexMap::new();
        for (coordinate, affected, residual_capacity) in
            [(source_a, 80, 100), (source_b, 40, 60), (target, 0, 50)]
        {
            let mut cell = selected_cell(coordinate, 0);
            cell.forces.infantry = affected;
            cell.military_capacity = residual_capacity;
            map.insert(cell);
        }
        let membership = map
            .coordinates()
            .map(|coordinate| (coordinate, coordinate == target))
            .collect::<BTreeMap<_, _>>();

        let targets = best_effort_shape_targets(&map, 1, &membership, 120).unwrap();

        assert_eq!(targets[&target], 50);
        assert_eq!(targets[&source_a], 47);
        assert_eq!(targets[&source_b], 23);
        assert_eq!(targets.values().sum::<u64>(), 120);
        assert!(targets[&source_a] <= 80);
        assert!(targets[&source_b] <= 40);
    }

    #[test]
    fn best_effort_reshape_uses_only_residual_target_capacity() {
        let source = Axial::ZERO;
        let target = Axial::new(1, 0);
        let mut map = HexMap::new();
        let mut source_cell = selected_cell(source, 0);
        source_cell.forces.infantry = 70;
        map.insert(source_cell);
        let mut target_cell = selected_cell(target, 0);
        target_cell.forces.infantry = 0;
        // The reducer projected 85 fixed infantry out of this cell, leaving
        // only 15 capacity for affected Reshape strength.
        target_cell.military_capacity = 15;
        map.insert(target_cell);
        let membership = BTreeMap::from([(source, false), (target, true)]);

        let targets = best_effort_shape_targets(&map, 1, &membership, 70).unwrap();

        assert_eq!(targets[&target], 15);
        assert_eq!(targets[&source], 55);
        assert_eq!(targets.values().sum::<u64>(), 70);
        assert_eq!(85 + targets[&target], 100);
    }

    #[test]
    fn disconnected_source_component_without_a_target_stays_unchanged() {
        let stranded_a = Axial::new(-20, 0);
        let stranded_b = Axial::new(-19, 0);
        let mut stranded_map = HexMap::new();
        for (coordinate, affected) in [(stranded_a, 31), (stranded_b, 59)] {
            let mut cell = selected_cell(coordinate, 0);
            cell.forces.infantry = affected;
            stranded_map.insert(cell);
        }
        let stranded_membership = BTreeMap::from([(stranded_a, false), (stranded_b, false)]);

        let unchanged =
            best_effort_shape_targets(&stranded_map, 1, &stranded_membership, 90).unwrap();

        assert_eq!(
            unchanged,
            BTreeMap::from([(stranded_a, 31), (stranded_b, 59)])
        );

        // A connected shape with a reachable target still redistributes all
        // affected strength normally.
        let moving_source = Axial::ZERO;
        let moving_target = Axial::new(1, 0);
        let mut moving_map = HexMap::new();
        let mut source_cell = selected_cell(moving_source, 0);
        source_cell.forces.infantry = 40;
        moving_map.insert(source_cell);
        moving_map.insert(selected_cell(moving_target, 0));
        let moved = best_effort_shape_targets(
            &moving_map,
            1,
            &BTreeMap::from([(moving_source, false), (moving_target, true)]),
            40,
        )
        .unwrap();
        assert_eq!(
            moved,
            BTreeMap::from([(moving_source, 0), (moving_target, 40)])
        );
    }

    #[test]
    fn cancelling_releases_the_fixed_pool_in_place_without_losing_strength() {
        assert_eq!(cancelled_settled_strength(100, 15, 25, 60), Ok(75));
        assert!(cancelled_settled_strength(100, 15, 25, 59).is_err());
        assert!(cancelled_settled_strength(100, u64::MAX, 0, 1).is_err());
    }

    #[test]
    fn exact_cancellation_rejects_empty_and_duplicate_ids() {
        assert!(exact_order_ids(&[]).is_err());
        assert!(exact_order_ids(&[7, 7]).is_err());
        assert_eq!(exact_order_ids(&[7, 8]), Ok(BTreeSet::from([7, 8])));
    }

    #[test]
    fn cancellation_preflight_requires_each_exact_local_active_order() {
        assert!(validate_cancel_claim(7, 1, 2, OrderStatus::Active).is_err());
        assert!(validate_cancel_claim(7, 1, 1, OrderStatus::Completed).is_err());
        assert!(validate_cancel_claim(7, 1, 1, OrderStatus::Cancelled).is_err());
        assert!(validate_cancel_claim(7, 1, 1, OrderStatus::Active).is_ok());
    }

    #[test]
    fn active_internal_destination_overlap_is_rejected_but_non_overlap_is_allowed() {
        let reservations = BTreeMap::from([(7, 30), (9, 0)]);
        let error = reject_active_internal_destination_overlap(
            "redistribution",
            &BTreeSet::from([6, 7]),
            &reservations,
        )
        .unwrap_err();

        assert!(error.starts_with("redistribution overlaps"));
        assert!(error.contains("cell 7"));
        assert!(error.contains("stop that order or wait"));
        assert!(
            reject_active_internal_destination_overlap(
                "redistribution",
                &BTreeSet::from([5, 6]),
                &reservations,
            )
            .is_ok()
        );
        assert!(
            reject_active_internal_destination_overlap(
                "reshape destination",
                &BTreeSet::from([9]),
                &reservations,
            )
            .is_ok(),
            "a fully received destination has no remaining reservation"
        );
    }

    #[test]
    fn superseded_foreign_inactive_and_combat_orders_do_not_reserve_internal_destinations() {
        let excluded = BTreeSet::from([7]);
        for kind in [OrderKind::Reshape, OrderKind::FrontRebalance] {
            let active = reservation_order(8, 1, kind, OrderStatus::Active);
            assert!(active_internal_reservation_is_relevant(
                &active, 1, &excluded
            ));

            let superseded = reservation_order(7, 1, kind, OrderStatus::Active);
            assert!(!active_internal_reservation_is_relevant(
                &superseded,
                1,
                &excluded
            ));
        }

        assert!(!active_internal_reservation_is_relevant(
            &reservation_order(8, 2, OrderKind::Reshape, OrderStatus::Active),
            1,
            &excluded,
        ));
        assert!(!active_internal_reservation_is_relevant(
            &reservation_order(8, 1, OrderKind::Reshape, OrderStatus::Completed),
            1,
            &excluded,
        ));
        for kind in [
            OrderKind::PushFront,
            OrderKind::ExpandAll,
            OrderKind::ExpandClusters,
            OrderKind::AttackClusters,
        ] {
            assert!(!active_internal_reservation_is_relevant(
                &reservation_order(8, 1, kind, OrderStatus::Active),
                1,
                &excluded,
            ));
        }
    }

    #[test]
    fn cluster_actions_coexist_with_existing_allocations_without_retasking_them() {
        let total = 100;
        let existing_action_allocation = 40;
        let available =
            available_after_retask_release(total, existing_action_allocation, 0).unwrap();
        assert_eq!(available, 60);
        assert_eq!(basis_point_share(available, 10_000), 60);
        assert_eq!(total - available, existing_action_allocation);
    }

    #[test]
    fn cluster_attack_rejects_an_adjacent_and_a_disconnected_target_component() {
        // Cells 1-2 are one enemy component reached from a shared source
        // front. Cells 8-9 are another complete selected enemy component with
        // no shared front, so neither receives a finite masked-wave depth.
        let target_cells = BTreeSet::from([1, 2, 8, 9]);
        let mut depths = vec![u16::MAX; 10];
        depths[1] = 1;
        depths[2] = 2;

        let error = validate_attack_target_reachability(&target_cells, &depths)
            .expect_err("one reachable component must not hide a disconnected target component");

        assert!(error.contains("every targeted enemy cluster"));
        assert!(error.contains("target cell 8 is unreachable"));
    }

    #[test]
    fn cluster_attack_accepts_multiple_target_components_when_each_has_contact() {
        // Both complete components have their own shared source front and may
        // therefore advance as independent branches of one attack order.
        let target_cells = BTreeSet::from([1, 2, 8, 9]);
        let mut depths = vec![u16::MAX; 10];
        depths[1] = 1;
        depths[2] = 2;
        depths[8] = 1;
        depths[9] = 2;

        assert!(validate_attack_target_reachability(&target_cells, &depths).is_ok());
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
