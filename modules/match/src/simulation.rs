use std::{
    collections::{BTreeMap, BTreeSet},
    rc::Rc,
};

use hex_core::{
    AttackFront, Axial, CombatConfig, EdgeLimits, HexMap, LogisticsConfig, MovementConfig,
    MovementIntent, MovementLimit, focus_branch_weight, movement_step, resolve_edge_combat,
    weighted_branch_allocations_rotated,
};
use spacetimedb::{ReducerContext, Table};

use crate::orders::{is_background_policy_order, maintain_cluster_policies};
use crate::rules::{
    allocated_infantry_at_cell, cell_id_for_coordinate, cell_state, config, coordinate_for_cell,
    core_cell, edge_runtime_limits, packet_key, state, terrain,
};
use crate::schema::{
    CellState, CombatFront, EXPANSION_AGGREGATE_ORIGIN, ExpansionGarrisonDebt, ExpansionWave,
    MatchPhase, NEUTRAL_PLAYER, OrderKind, OrderStatus, PLAYER_ONE, PLAYER_TWO, TerrainClass,
    TransferOrder, TransitPacket,
};
use crate::schema::{
    cell_state as cell_state_table, combat_front, expansion_garrison_debt, expansion_wave,
    match_state, mobilization_policy, retreat_abandonment, transfer_destination, transfer_order,
    transfer_source, transit_packet,
};

/// Transaction-local packet index for one simulation step.
///
/// SpacetimeDB row iteration decodes the complete row, including `route`, on
/// every scan. The simulation has several ordered packet phases, so keeping a
/// synchronized in-memory index avoids decoding the same active set for trim,
/// branching, movement, combat, and finalization while preserving their exact
/// mutation order. Writes still reach the database immediately so existing
/// indexed queries outside this module observe the current transaction state.
#[derive(Clone)]
struct TickPacket {
    packet_key: String,
    order_id: u64,
    owner_player_id: u8,
    origin_cell: u32,
    current_cell: u32,
    destination_cell: u32,
    infantry: u64,
    route_index: u32,
    route: Rc<[u32]>,
    updated_step: u64,
}

impl From<TransitPacket> for TickPacket {
    fn from(packet: TransitPacket) -> Self {
        Self {
            packet_key: packet.packet_key,
            order_id: packet.order_id,
            owner_player_id: packet.owner_player_id,
            origin_cell: packet.origin_cell,
            current_cell: packet.current_cell,
            destination_cell: packet.destination_cell,
            infantry: packet.infantry,
            route_index: packet.route_index,
            route: Rc::from(packet.route),
            updated_step: packet.updated_step,
        }
    }
}

impl TickPacket {
    fn to_row(&self) -> TransitPacket {
        TransitPacket {
            packet_key: self.packet_key.clone(),
            order_id: self.order_id,
            owner_player_id: self.owner_player_id,
            origin_cell: self.origin_cell,
            current_cell: self.current_cell,
            destination_cell: self.destination_cell,
            infantry: self.infantry,
            route_index: self.route_index,
            route: self.route.to_vec(),
            updated_step: self.updated_step,
        }
    }
}

#[derive(Default)]
struct PacketTickState {
    rows: BTreeMap<Rc<str>, TickPacket>,
    by_order: BTreeMap<u64, BTreeSet<Rc<str>>>,
    by_cell: BTreeMap<u32, BTreeSet<Rc<str>>>,
    by_order_destination: BTreeMap<(u64, u32), BTreeSet<Rc<str>>>,
}

impl PacketTickState {
    fn load(ctx: &ReducerContext) -> Self {
        let mut state = Self::default();
        for packet in ctx.db.transit_packet().iter() {
            state.track(packet.into());
        }
        state
    }

    fn iter(&self) -> impl Iterator<Item = &TickPacket> {
        self.rows.values()
    }

    fn find(&self, packet_key: &str) -> Option<TickPacket> {
        self.rows.get(packet_key).cloned()
    }

    fn by_order(&self, order_id: u64) -> impl Iterator<Item = &TickPacket> {
        self.by_order
            .get(&order_id)
            .into_iter()
            .flatten()
            .filter_map(|key| self.rows.get(key))
    }

    fn by_cell(&self, cell_id: u32) -> impl Iterator<Item = &TickPacket> {
        self.by_cell
            .get(&cell_id)
            .into_iter()
            .flatten()
            .filter_map(|key| self.rows.get(key))
    }

    fn by_order_destination(
        &self,
        order_id: u64,
        destination_cell: u32,
    ) -> impl Iterator<Item = &TickPacket> {
        self.by_order_destination
            .get(&(order_id, destination_cell))
            .into_iter()
            .flatten()
            .filter_map(|key| self.rows.get(key))
    }

    fn insert(&mut self, ctx: &ReducerContext, packet: TickPacket) {
        ctx.db.transit_packet().insert(packet.to_row());
        self.track(packet);
    }

    fn update(&mut self, ctx: &ReducerContext, packet: TickPacket) {
        ctx.db.transit_packet().packet_key().update(packet.to_row());
        self.untrack(&packet.packet_key);
        self.track(packet);
    }

    fn delete(&mut self, ctx: &ReducerContext, packet_key: &str) {
        if let Some(packet) = self.rows.get(packet_key) {
            ctx.db
                .transit_packet()
                .packet_key()
                .delete(&packet.packet_key);
        }
        self.untrack(packet_key);
    }

    fn track(&mut self, packet: TickPacket) {
        let key = Rc::<str>::from(packet.packet_key.as_str());
        self.by_order
            .entry(packet.order_id)
            .or_default()
            .insert(key.clone());
        self.by_cell
            .entry(packet.current_cell)
            .or_default()
            .insert(key.clone());
        self.by_order_destination
            .entry((packet.order_id, packet.destination_cell))
            .or_default()
            .insert(key.clone());
        self.rows.insert(key, packet);
    }

    fn untrack(&mut self, packet_key: &str) {
        let Some(packet) = self.rows.remove(packet_key) else {
            return;
        };
        if let Some(keys) = self.by_order.get_mut(&packet.order_id) {
            keys.remove(packet_key);
        }
        if let Some(keys) = self.by_cell.get_mut(&packet.current_cell) {
            keys.remove(packet_key);
        }
        if let Some(keys) = self
            .by_order_destination
            .get_mut(&(packet.order_id, packet.destination_cell))
        {
            keys.remove(packet_key);
        }
    }
}

pub fn advance_simulation(ctx: &ReducerContext) -> Result<bool, String> {
    let mut match_state = state(ctx)?;
    if match_state.phase != MatchPhase::Running {
        return Ok(false);
    }
    match_state.logical_step = match_state
        .logical_step
        .checked_add(1)
        .ok_or_else(|| "logical step overflow".to_string())?;
    let logical_step = match_state.logical_step;
    ctx.db.match_state().singleton_id().update(match_state);

    let mut packets = PacketTickState::load(ctx);
    trim_all_overallocated_packets(ctx, &mut packets, logical_step)?;
    branch_expand_waves(ctx, &mut packets, logical_step)?;
    stop_blocked_expand_edges(ctx, &mut packets, logical_step)?;
    move_friendly_packets(ctx, &mut packets, logical_step)?;
    stop_blocked_internal_edges(ctx, &mut packets, logical_step)?;
    resolve_combats(ctx, &mut packets, logical_step)?;
    clear_stale_combat_fronts(ctx, logical_step);
    finalize_orders(ctx, &packets, logical_step)?;

    let config = config(ctx)?;
    let population_interval = u64::from(config.population_step_interval.max(1));
    if logical_step.is_multiple_of(population_interval) {
        population_step(ctx, logical_step)?;
    }
    maintain_cluster_policies(ctx, logical_step)?;
    Ok(state(ctx)?.phase == MatchPhase::Running)
}

fn is_expansion_wave_order(kind: OrderKind) -> bool {
    matches!(
        kind,
        OrderKind::ExpandAll | OrderKind::ExpandClusters | OrderKind::AttackClusters
    )
}

/// Neutral waves stop at later enemy ownership. Attack waves are constrained
/// instead by their immutable source and target masks, so captures can open
/// deeper fronts without ever leaking into an unselected cluster.
fn stop_blocked_expand_edges(
    ctx: &ReducerContext,
    packets: &mut PacketTickState,
    logical_step: u64,
) -> Result<(), String> {
    let expand_orders = ctx
        .db
        .transfer_order()
        .order_by_status()
        .filter(OrderStatus::Active)
        .filter(|order| is_expansion_wave_order(order.kind))
        .collect::<Vec<_>>();
    if expand_orders.is_empty() {
        return Ok(());
    }

    let mut blocked_packets = Vec::new();
    for order in expand_orders {
        let wave = ctx
            .db
            .expansion_wave()
            .order_id()
            .find(order.order_id)
            .ok_or_else(|| format!("wave order {} has no topology", order.order_id))?;
        for packet in packets.by_order(order.order_id) {
            let next_index = packet.route_index as usize + 1;
            let Some(&next_cell) = packet.route.get(next_index) else {
                continue;
            };
            if !expansion_edge_is_available(ctx, &order, &wave, packet.current_cell, next_cell)? {
                blocked_packets.push(packet.clone());
            }
        }
    }
    blocked_packets.sort_unstable_by(|left, right| left.packet_key.cmp(&right.packet_key));
    for packet in blocked_packets {
        station_packet_allocation(ctx, packets, &packet, packet.infantry, logical_step)?;
    }
    Ok(())
}

/// Formation and reshape routes are logistics-only. If ownership changes
/// after an order is accepted, its allocation is retired in its current cell
/// before the generic combat pass can treat the stale route as an attack.
/// Running this after friendly movement also catches a packet that advanced
/// next to the newly non-friendly cell during the same simulation step.
fn stop_blocked_internal_edges(
    ctx: &ReducerContext,
    packets: &mut PacketTickState,
    logical_step: u64,
) -> Result<(), String> {
    let internal_orders = ctx
        .db
        .transfer_order()
        .order_by_status()
        .filter(OrderStatus::Active)
        .filter(|order| internal_order_requires_friendly_route(order.kind))
        .map(|order| (order.order_id, order.player_id, order.kind))
        .collect::<Vec<_>>();
    if internal_orders.is_empty() {
        return Ok(());
    }

    let mut blocked_packets = Vec::new();
    for (order_id, player_id, kind) in internal_orders {
        for packet in packets.by_order(order_id) {
            let Some(&next_cell) = packet.route.get(packet.route_index as usize + 1) else {
                continue;
            };
            let next_owner = cell_state(ctx, next_cell)?.owner_player_id;
            if internal_next_owner_is_blocked(kind, player_id, next_owner) {
                blocked_packets.push(packet.clone());
            }
        }
    }
    blocked_packets.sort_unstable_by(|left, right| left.packet_key.cmp(&right.packet_key));
    for packet in blocked_packets {
        station_packet_allocation(ctx, packets, &packet, packet.infantry, logical_step)?;
    }
    Ok(())
}

fn internal_order_requires_friendly_route(kind: OrderKind) -> bool {
    matches!(
        kind,
        OrderKind::Balance
            | OrderKind::FrontLoad
            | OrderKind::CoreLoad
            | OrderKind::PerimeterLoad
            | OrderKind::Reshape
    )
}

fn internal_next_owner_is_blocked(kind: OrderKind, player_id: u8, next_owner: u8) -> bool {
    internal_order_requires_friendly_route(kind) && next_owner != player_id
}

fn branch_expand_waves(
    ctx: &ReducerContext,
    packets: &mut PacketTickState,
    logical_step: u64,
) -> Result<(), String> {
    let orders = ctx
        .db
        .transfer_order()
        .order_by_status()
        .filter(OrderStatus::Active)
        .filter(|order| is_expansion_wave_order(order.kind))
        .collect::<Vec<_>>();
    for order in orders {
        let mut wave = ctx
            .db
            .expansion_wave()
            .order_id()
            .find(order.order_id)
            .ok_or_else(|| format!("expand order {} has no topology", order.order_id))?;
        let mut resting_by_cell = BTreeMap::<u32, Vec<TickPacket>>::new();
        for packet in packets.by_order(order.order_id) {
            if expansion_packet_is_resting(packet) {
                resting_by_cell
                    .entry(packet.current_cell)
                    .or_default()
                    .push(packet.clone());
            }
        }
        let mut topology_changed = false;
        for (cell_id, mut contributions) in resting_by_cell {
            contributions.sort_unstable_by(|left, right| left.packet_key.cmp(&right.packet_key));
            topology_changed |= branch_expand_node(
                ctx,
                packets,
                &order,
                &mut wave,
                cell_id,
                &contributions,
                logical_step,
            )?;
        }
        if topology_changed {
            ctx.db.expansion_wave().order_id().update(wave);
        }
    }
    Ok(())
}

fn expansion_packet_is_resting(packet: &TickPacket) -> bool {
    packet.route_index == 0
        && packet.route.as_ref() == [packet.current_cell]
        && packet.destination_cell == packet.current_cell
}

fn branch_expand_node(
    ctx: &ReducerContext,
    packets: &mut PacketTickState,
    order: &TransferOrder,
    wave: &mut ExpansionWave,
    cell_id: u32,
    contributions: &[TickPacket],
    logical_step: u64,
) -> Result<bool, String> {
    let contributions =
        pay_expansion_garrison_debt(ctx, packets, order, cell_id, contributions, logical_step)?;
    if contributions.is_empty() {
        return Ok(false);
    }

    let children = expansion_children(ctx, wave, cell_id)?;
    if children.is_empty() {
        for contribution in &contributions {
            station_packet_allocation(
                ctx,
                packets,
                contribution,
                contribution.infantry,
                logical_step,
            )?;
        }
        return Ok(false);
    }

    let amounts = contributions
        .iter()
        .map(|packet| packet.infantry)
        .collect::<Vec<_>>();
    let cursor = wave
        .split_cursors
        .get(cell_id as usize)
        .copied()
        .ok_or_else(|| format!("expand split cursor is missing cell {cell_id}"))?;
    let child_weights = expansion_child_weights(ctx, wave, cell_id, &children)?;
    let weighted =
        weighted_branch_allocations_rotated(&amounts, &child_weights, usize::from(cursor))
            .map_err(|error| format!("invalid expansion branch allocation: {error:?}"))?;
    let allocations = weighted.allocations;
    let next_cursor = weighted.next_cursor;
    let next_cursor =
        u8::try_from(next_cursor).map_err(|_| "expand child cursor exceeds u8".to_string())?;
    let topology_changed = next_cursor != cursor;
    if topology_changed {
        wave.split_cursors[cell_id as usize] = next_cursor;
    }
    let mut stationed_by_contribution = vec![0_u64; contributions.len()];
    let mut outgoing = Vec::new();
    for allocation in allocations {
        let child = children[allocation.child_index];
        if expansion_edge_is_available(ctx, order, wave, cell_id, child)? {
            outgoing.push((allocation.contribution_index, child, allocation.amount));
        } else {
            stationed_by_contribution[allocation.contribution_index] = stationed_by_contribution
                [allocation.contribution_index]
                .checked_add(allocation.amount)
                .ok_or_else(|| "expand stationed strength overflow".to_string())?;
        }
    }

    for (index, contribution) in contributions.iter().enumerate() {
        let stationed = stationed_by_contribution[index];
        if stationed > 0 {
            station_packet_allocation(ctx, packets, contribution, stationed, logical_step)?;
        }
        if packets.find(&contribution.packet_key).is_some() {
            packets.delete(ctx, &contribution.packet_key);
        }
    }
    for (contribution_index, child, amount) in outgoing {
        insert_expand_edge_packet(
            ctx,
            packets,
            order,
            &contributions[contribution_index],
            cell_id,
            child,
            amount,
            logical_step,
        )?;
    }
    Ok(topology_changed)
}

/// Pays only capture-scoped debt and only from this expansion's resting
/// allocations. Infantry stays in the cell; stationing merely retires the
/// allocation and accounts it as delivered before any surplus can branch.
fn pay_expansion_garrison_debt(
    ctx: &ReducerContext,
    packets: &mut PacketTickState,
    order: &TransferOrder,
    cell_id: u32,
    contributions: &[TickPacket],
    logical_step: u64,
) -> Result<Vec<TickPacket>, String> {
    let Some(mut debt) = ctx.db.expansion_garrison_debt().cell_id().find(cell_id) else {
        return Ok(contributions.to_vec());
    };
    let cell = cell_state(ctx, cell_id)?;
    if !expansion_debt_applies(debt.owner_player_id, cell.owner_player_id, order.player_id) {
        ctx.db.expansion_garrison_debt().cell_id().delete(cell_id);
        return Ok(contributions.to_vec());
    }

    let required = occupation_garrison(cell.military_capacity, terrain(ctx, cell_id)?.terrain);
    let allocated = allocated_infantry_at_cell(ctx, order.player_id, cell_id);
    let currently_missing = additional_garrison_required(required, cell.infantry, allocated);
    let mut remaining_debt = debt.remaining_infantry.min(currently_missing);

    for contribution in contributions {
        if remaining_debt == 0 {
            break;
        }
        let Some(current) = packets.find(&contribution.packet_key) else {
            continue;
        };
        if current.order_id != order.order_id
            || current.owner_player_id != order.player_id
            || current.current_cell != cell_id
            || !expansion_packet_is_resting(&current)
        {
            return Err("expand garrison debt received an invalid resting contribution".into());
        }
        let (stationed, next_debt, _) = garrison_debt_partition(remaining_debt, current.infantry);
        station_packet_allocation(ctx, packets, &current, stationed, logical_step)?;
        remaining_debt = next_debt;
    }

    if remaining_debt == 0 {
        ctx.db.expansion_garrison_debt().cell_id().delete(cell_id);
    } else if remaining_debt != debt.remaining_infantry {
        debt.remaining_infantry = remaining_debt;
        ctx.db.expansion_garrison_debt().cell_id().update(debt);
    }

    Ok(contributions
        .iter()
        .filter_map(|contribution| packets.find(&contribution.packet_key))
        .collect())
}

fn expansion_debt_applies(debt_owner: u8, cell_owner: u8, order_owner: u8) -> bool {
    debt_owner == cell_owner && cell_owner == order_owner
}

/// Returns `(stationed, remaining debt, continuing surplus)`.
fn garrison_debt_partition(debt: u64, arrival: u64) -> (u64, u64, u64) {
    let stationed = debt.min(arrival);
    (stationed, debt - stationed, arrival - stationed)
}

fn expansion_children(
    ctx: &ReducerContext,
    wave: &ExpansionWave,
    cell_id: u32,
) -> Result<Vec<u32>, String> {
    if wave.selected_cells.len() != wave.seed_depths.len() {
        return Err("expand topology has mismatched seed vectors".into());
    }
    let parent_depth = wave_node_depth(wave, cell_id)
        .ok_or_else(|| format!("expand resting cell {cell_id} is outside its topology"))?;

    let match_config = config(ctx)?;
    let coordinate = coordinate_for_cell(ctx, cell_id)?;
    let mut children = Vec::new();
    for neighbor in coordinate.neighbors() {
        let Some(neighbor_id) = cell_id_for_coordinate(&match_config, neighbor) else {
            continue;
        };
        let matches_depth = wave_node_depth(wave, neighbor_id)
            .is_some_and(|child_depth| wave_depth_allows_child(parent_depth, child_depth));
        if matches_depth && edge_runtime_limits(ctx, cell_id, neighbor_id)?.is_some() {
            children.push(neighbor_id);
        }
    }
    children.sort_unstable();
    children.dedup();
    Ok(children)
}

fn expansion_child_weights(
    ctx: &ReducerContext,
    wave: &ExpansionWave,
    parent_cell: u32,
    children: &[u32],
) -> Result<Vec<u8>, String> {
    let Some(focus_cell) = wave.focus_cell_id else {
        return Ok(vec![1; children.len()]);
    };
    let parent = coordinate_for_cell(ctx, parent_cell)?;
    let focus = coordinate_for_cell(ctx, focus_cell)?;
    let child_coordinates = children
        .iter()
        .map(|child| coordinate_for_cell(ctx, *child))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(focus_weights_for_coordinates(
        parent,
        &child_coordinates,
        focus,
    ))
}

fn focus_weights_for_coordinates(parent: Axial, children: &[Axial], focus: Axial) -> Vec<u8> {
    children
        .iter()
        .map(|child| focus_branch_weight(parent, *child, focus))
        .collect()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WaveNodeDepth {
    Seed(u16),
    Outside(u16),
}

fn wave_node_depth(wave: &ExpansionWave, cell_id: u32) -> Option<WaveNodeDepth> {
    if let Ok(index) = wave.selected_cells.binary_search(&cell_id) {
        return wave
            .seed_depths
            .get(index)
            .copied()
            .map(WaveNodeDepth::Seed);
    }
    wave.outside_depths
        .get(cell_id as usize)
        .copied()
        .filter(|depth| *depth != u16::MAX)
        .map(WaveNodeDepth::Outside)
}

fn wave_depth_allows_child(parent: WaveNodeDepth, child: WaveNodeDepth) -> bool {
    match (parent, child) {
        (WaveNodeDepth::Seed(parent), WaveNodeDepth::Seed(child)) => {
            parent > 0 && child.checked_add(1) == Some(parent)
        }
        (WaveNodeDepth::Seed(0), WaveNodeDepth::Outside(1)) => true,
        (WaveNodeDepth::Outside(parent), WaveNodeDepth::Outside(child)) => {
            parent.checked_add(1) == Some(child)
        }
        _ => false,
    }
}

fn expansion_edge_is_available(
    ctx: &ReducerContext,
    order: &TransferOrder,
    wave: &ExpansionWave,
    from_cell: u32,
    to_cell: u32,
) -> Result<bool, String> {
    let target_terrain = terrain(ctx, to_cell)?;
    let target_owner = cell_state(ctx, to_cell)?.owner_player_id;
    Ok(wave_scope_allows_cell(
        order.kind,
        order.player_id,
        to_cell,
        target_owner,
        &wave.selected_cells,
        &wave.target_cells,
    ) && target_terrain.passable
        && target_terrain.capturable
        && edge_runtime_limits(ctx, from_cell, to_cell)?.is_some())
}

fn wave_scope_allows_cell(
    kind: OrderKind,
    player_id: u8,
    cell_id: u32,
    owner_player_id: u8,
    selected_cells: &[u32],
    target_cells: &[u32],
) -> bool {
    match kind {
        OrderKind::ExpandAll | OrderKind::ExpandClusters => {
            owner_player_id == NEUTRAL_PLAYER || owner_player_id == player_id
        }
        OrderKind::AttackClusters => {
            target_cells.binary_search(&cell_id).is_ok()
                || (owner_player_id == player_id && selected_cells.binary_search(&cell_id).is_ok())
        }
        _ => false,
    }
}

#[allow(clippy::too_many_arguments)]
fn insert_expand_edge_packet(
    ctx: &ReducerContext,
    packets: &mut PacketTickState,
    order: &TransferOrder,
    contribution: &TickPacket,
    from_cell: u32,
    to_cell: u32,
    amount: u64,
    logical_step: u64,
) -> Result<(), String> {
    if amount == 0 {
        return Ok(());
    }
    let key = packet_key(
        order.order_id,
        contribution.origin_cell,
        to_cell,
        from_cell,
        0,
    );
    if let Some(mut existing) = packets.find(&key) {
        existing.infantry = merged_expand_strength(existing.infantry, amount)?;
        existing.updated_step = logical_step;
        packets.update(ctx, existing);
    } else {
        packets.insert(
            ctx,
            TickPacket {
                packet_key: key,
                order_id: order.order_id,
                owner_player_id: order.player_id,
                origin_cell: contribution.origin_cell,
                current_cell: from_cell,
                destination_cell: to_cell,
                infantry: amount,
                route_index: 0,
                route: Rc::from([from_cell, to_cell]),
                updated_step: logical_step,
            },
        );
    }
    Ok(())
}

fn merged_expand_strength(existing: u64, incoming: u64) -> Result<u64, String> {
    existing
        .checked_add(incoming)
        .ok_or_else(|| "expand packet strength overflow".to_string())
}

fn clear_stale_combat_fronts(ctx: &ReducerContext, logical_step: u64) {
    let keys: Vec<_> = ctx
        .db
        .combat_front()
        .iter()
        .filter(|front| front.logical_step != logical_step)
        .map(|front| front.front_key)
        .collect();
    for key in keys {
        ctx.db.combat_front().front_key().delete(key);
    }
}

fn population_step(ctx: &ReducerContext, logical_step: u64) -> Result<(), String> {
    let config = config(ctx)?;
    let destination_reservations = active_internal_destination_reservations(ctx)?;
    let retreating_edge_cells = active_retreat_abandonment_cells(ctx);
    let policies: BTreeMap<_, _> = ctx
        .db
        .mobilization_policy()
        .iter()
        .map(|policy| (policy.player_id, policy.target_bps))
        .collect();
    let cells: Vec<_> = ctx.db.cell_state().iter().collect();
    for mut cell in cells {
        let Some(&target_bps) = policies.get(&cell.owner_player_id) else {
            continue;
        };
        if cell.civilian_capacity == 0 {
            continue;
        }
        let previous_civilians = cell.civilians;
        let previous_infantry = cell.infantry;
        let missing = cell.civilian_capacity.saturating_sub(cell.civilians);
        if missing > 0 {
            let growth =
                ((u128::from(missing) * u128::from(config.civilian_growth_bps)) / 10_000) as u64;
            cell.civilians = cell.civilians.saturating_add(growth.max(1).min(missing));
        }

        let local_population = cell.civilians.saturating_add(cell.infantry);
        let desired_infantry =
            ((u128::from(local_population) * u128::from(target_bps)) / 10_000) as u64;
        if cell.infantry < desired_infantry && !retreating_edge_cells.contains(&cell.cell_id) {
            let reserved_capacity = reserved_recruitment_capacity(
                &destination_reservations,
                cell.owner_player_id,
                cell.cell_id,
            );
            let recruit = desired_infantry
                .saturating_sub(cell.infantry)
                .min(config.mobilization_per_population_step)
                .min(cell.civilians)
                .min(recruitment_headroom(
                    cell.military_capacity,
                    cell.infantry,
                    reserved_capacity,
                ));
            cell.civilians -= recruit;
            cell.infantry += recruit;
        }
        if cell.civilians != previous_civilians || cell.infantry != previous_infantry {
            cell.last_changed_step = logical_step;
            if cell.infantry != previous_infantry {
                cell.last_policy_changed_step = logical_step;
            }
            ctx.db.cell_state().cell_id().update(cell);
        }
    }
    Ok(())
}

fn active_retreat_abandonment_cells(ctx: &ReducerContext) -> BTreeSet<u32> {
    let active_orders = ctx
        .db
        .transfer_order()
        .order_by_status()
        .filter(OrderStatus::Active)
        .map(|order| order.order_id)
        .collect::<BTreeSet<_>>();
    ctx.db
        .retreat_abandonment()
        .iter()
        .filter(|candidate| active_orders.contains(&candidate.order_id))
        .map(|candidate| candidate.cell_id)
        .collect()
}

/// Capacity promised to active internal movement remains unavailable to local
/// recruitment until the destination receives it. Without this reservation, a
/// long Formation or Reshape route can be accepted capacity-safely and then be
/// blocked forever because later mobilization fills its destination first.
fn active_internal_destination_reservations(
    ctx: &ReducerContext,
) -> Result<BTreeMap<(u8, u32), u64>, String> {
    let mut reservations = BTreeMap::<(u8, u32), u64>::new();
    for destination in ctx.db.transfer_destination().iter() {
        let Some(order) = ctx
            .db
            .transfer_order()
            .order_id()
            .find(destination.order_id)
        else {
            continue;
        };
        add_internal_destination_reservation(
            &mut reservations,
            &order,
            destination.cell_id,
            destination.target_infantry,
            destination.received_infantry,
        )?;
    }
    Ok(reservations)
}

fn order_reserves_recruitment_capacity(order: &TransferOrder) -> bool {
    order.status == OrderStatus::Active && internal_order_requires_friendly_route(order.kind)
}

fn add_internal_destination_reservation(
    reservations: &mut BTreeMap<(u8, u32), u64>,
    order: &TransferOrder,
    cell_id: u32,
    target_infantry: u64,
    received_infantry: u64,
) -> Result<(), String> {
    if !order_reserves_recruitment_capacity(order) {
        return Ok(());
    }
    let remaining = target_infantry.saturating_sub(received_infantry);
    if remaining == 0 {
        return Ok(());
    }
    let reserved = reservations.entry((order.player_id, cell_id)).or_default();
    *reserved = reserved
        .checked_add(remaining)
        .ok_or_else(|| "internal destination reservation overflow".to_string())?;
    Ok(())
}

fn reserved_recruitment_capacity(
    reservations: &BTreeMap<(u8, u32), u64>,
    owner_player_id: u8,
    cell_id: u32,
) -> u64 {
    reservations
        .get(&(owner_player_id, cell_id))
        .copied()
        .unwrap_or(0)
}

fn recruitment_headroom(capacity: u64, current: u64, reserved: u64) -> u64 {
    capacity.saturating_sub(current).saturating_sub(reserved)
}

#[derive(Clone)]
struct FriendlyIntent {
    id: u64,
    packet: TickPacket,
    next_cell: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PacketArrival {
    Friendly,
    Capture,
}

impl PacketArrival {
    const fn may_extend_sustained_push(self) -> bool {
        matches!(self, Self::Capture)
    }
}

fn should_settle_capacity_blocked_friendly_lane(
    kind: OrderKind,
    route_index: u32,
    route_len: usize,
    limits: &BTreeSet<MovementLimit>,
) -> bool {
    kind == OrderKind::PushFront
        && usize::try_from(route_index)
            .ok()
            .and_then(|index| index.checked_add(2))
            == Some(route_len)
        && limits.contains(&MovementLimit::DestinationCapacity)
}

fn should_station_capacity_blocked_packet(
    order: &TransferOrder,
    limits: &BTreeSet<MovementLimit>,
) -> bool {
    // Policy maintenance is replanned from the packets' current physical
    // positions at each policy checkpoint. Keep capacity-blocked packets
    // queued until then: prematurely stationing them marks undelivered demand
    // as completed and is the source of large-cluster redistribution churn.
    // Explicit one-shot logistics and bounded expansion waves retain their
    // best-effort stop-in-place semantics.
    !is_background_policy_order(order)
        && (internal_order_requires_friendly_route(order.kind)
            || is_expansion_wave_order(order.kind))
        && limits.contains(&MovementLimit::DestinationCapacity)
}

fn move_friendly_packets(
    ctx: &ReducerContext,
    packets: &mut PacketTickState,
    logical_step: u64,
) -> Result<(), String> {
    let mut intents = Vec::new();
    for packet in packets.iter() {
        let Some(&next_cell) = packet.route.get(packet.route_index as usize + 1) else {
            continue;
        };
        let next = cell_state(ctx, next_cell)?;
        if next.owner_player_id == packet.owner_player_id {
            intents.push(FriendlyIntent {
                id: intents.len() as u64 + 1,
                packet: packet.clone(),
                next_cell,
            });
        }
    }
    if intents.is_empty() {
        return Ok(());
    }

    let config = config(ctx)?;
    let movement = MovementConfig {
        max_elevation_step: u16::from(config.max_elevation_step),
        ..MovementConfig::default()
    };
    let logistics = LogisticsConfig {
        default_military_capacity: config.base_military_capacity,
        default_edge_throughput: 0,
        default_combat_frontage: config.base_combat_frontage,
    };
    let mut map = HexMap::new();
    let mut participating = BTreeSet::new();
    for intent in &intents {
        participating.insert(intent.packet.current_cell);
        participating.insert(intent.next_cell);
    }
    for cell_id in participating.iter().copied() {
        map.insert(core_cell(ctx, cell_id)?);
    }
    for intent in &intents {
        let limits = edge_runtime_limits(ctx, intent.packet.current_cell, intent.next_cell)?
            .ok_or_else(|| "active route contains an impassable edge".to_string())?;
        map.set_edge_limits(
            coordinate_for_cell(ctx, intent.packet.current_cell)?,
            coordinate_for_cell(ctx, intent.next_cell)?,
            EdgeLimits {
                throughput: limits.throughput_per_step,
                frontage: limits.frontage,
            },
        );
    }
    let movement_intents: Vec<_> = intents
        .iter()
        .map(|intent| {
            Ok(MovementIntent {
                id: intent.id,
                priority: 0,
                owner: u32::from(intent.packet.owner_player_id),
                from: coordinate_for_cell(ctx, intent.packet.current_cell)?,
                to: coordinate_for_cell(ctx, intent.next_cell)?,
                requested: intent.packet.infantry,
            })
        })
        .collect::<Result<_, String>>()?;
    let result = movement_step(&mut map, &movement_intents, &movement, &logistics)
        .map_err(|error| format!("friendly movement failed: {error:?}"))?;

    // Throughput is a per-step throttle, so constrained packets remain queued.
    // Capacity is a hard stop: Push releases the whole directional lane, while
    // an internal logistics or Expand packet retires its blocked remainder in
    // place. This guarantees neither a Formation/Reshape nor a bounded wave can
    // remain active forever behind a full friendly cell.
    let mut capacity_stopped_lanes = BTreeSet::new();
    let mut capacity_stopped_packets = BTreeMap::new();
    for intent in &intents {
        let Some(outcome) = result.outcomes.get(&intent.id) else {
            continue;
        };
        let order = ctx
            .db
            .transfer_order()
            .order_id()
            .find(intent.packet.order_id)
            .ok_or_else(|| "friendly packet order is missing".to_string())?;
        if should_settle_capacity_blocked_friendly_lane(
            order.kind,
            intent.packet.route_index,
            intent.packet.route.len(),
            &outcome.limits,
        ) {
            let direction = push_lane_direction(ctx, &order, &intent.packet)?;
            capacity_stopped_lanes.insert((
                intent.packet.order_id,
                intent.packet.destination_cell,
                direction,
            ));
        } else if should_station_capacity_blocked_packet(&order, &outcome.limits) {
            capacity_stopped_packets
                .insert(intent.packet.packet_key.clone(), intent.packet.clone());
        }
    }

    for cell_id in participating {
        let coordinate = coordinate_for_cell(ctx, cell_id)?;
        let infantry = map
            .get(coordinate)
            .ok_or_else(|| "movement result omitted a participating cell".to_string())?
            .force();
        let mut row = cell_state(ctx, cell_id)?;
        if row.infantry != infantry {
            row.last_policy_changed_step = logical_step;
        }
        row.infantry = infantry;
        row.last_changed_step = logical_step;
        ctx.db.cell_state().cell_id().update(row);
    }
    // Commit downstream metadata first. An upstream packet may flow into and
    // merge with the downstream packet's key in the same logical step; doing
    // the downstream update first prevents a stale snapshot from deleting the
    // newly merged strength.
    intents.sort_unstable_by(|left, right| {
        right
            .packet
            .route_index
            .cmp(&left.packet.route_index)
            .then_with(|| left.packet.packet_key.cmp(&right.packet.packet_key))
    });
    for intent in intents {
        let approved = result
            .outcomes
            .get(&intent.id)
            .map(|outcome| outcome.approved)
            .unwrap_or(0);
        if approved > 0 {
            advance_packet(
                ctx,
                packets,
                &intent.packet,
                approved,
                logical_step,
                PacketArrival::Friendly,
            )?;
        }
    }
    for (order_id, lane_anchor, direction) in capacity_stopped_lanes {
        settle_stopped_sustained_lane(
            ctx,
            packets,
            order_id,
            lane_anchor,
            direction,
            logical_step,
        )?;
    }
    for packet in capacity_stopped_packets.into_values() {
        // An upstream packet can merge into this key during the same pipeline
        // step. Retire the complete post-movement allocation at the blocked
        // choke, not only the amount present in the pre-step snapshot.
        station_packet_allocation(ctx, packets, &packet, u64::MAX, logical_step)?;
    }
    Ok(())
}

#[derive(Clone)]
struct FrontPackets {
    attacker: u8,
    from_cell: u32,
    to_cell: u32,
    packets: Vec<TickPacket>,
}

fn resolve_combats(
    ctx: &ReducerContext,
    packets: &mut PacketTickState,
    logical_step: u64,
) -> Result<(), String> {
    let mut targets = BTreeMap::<u32, BTreeMap<u8, BTreeMap<u32, Vec<TickPacket>>>>::new();
    for packet in packets.iter() {
        let Some(&next_cell) = packet.route.get(packet.route_index as usize + 1) else {
            continue;
        };
        if cell_state(ctx, next_cell)?.owner_player_id == packet.owner_player_id {
            continue;
        }
        targets
            .entry(next_cell)
            .or_default()
            .entry(packet.owner_player_id)
            .or_default()
            .entry(packet.current_cell)
            .or_default()
            .push(packet.clone());
    }

    for (target_cell, attackers) in targets {
        if state(ctx)?.phase != MatchPhase::Running {
            break;
        }
        let attackers = refresh_target_attackers(ctx, packets, target_cell, attackers)?;
        if attackers.is_empty() {
            continue;
        }
        let defender = cell_state(ctx, target_cell)?;
        let selected_attacker = if defender.owner_player_id == NEUTRAL_PLAYER {
            attackers
                .iter()
                .max_by(|(left_player, left_fronts), (right_player, right_fronts)| {
                    let left_strength = left_fronts
                        .values()
                        .flatten()
                        .map(|packet| packet.infantry)
                        .sum::<u64>();
                    let right_strength = right_fronts
                        .values()
                        .flatten()
                        .map(|packet| packet.infantry)
                        .sum::<u64>();
                    left_strength
                        .cmp(&right_strength)
                        .then_with(|| right_player.cmp(left_player))
                })
                .map(|(&player, _)| player)
        } else {
            attackers
                .keys()
                .copied()
                .find(|attacker| *attacker != defender.owner_player_id)
        };
        let Some(attacker) = selected_attacker else {
            continue;
        };
        let Some(front_map) = attackers.get(&attacker) else {
            continue;
        };
        let fronts: Vec<_> = front_map
            .iter()
            .map(|(&from_cell, packets)| FrontPackets {
                attacker,
                from_cell,
                to_cell: target_cell,
                packets: packets.clone(),
            })
            .collect();
        resolve_target_combat(ctx, packets, defender, fronts, logical_step)?;
    }
    Ok(())
}

fn refresh_target_attackers(
    ctx: &ReducerContext,
    packet_state: &PacketTickState,
    target_cell: u32,
    candidates: BTreeMap<u8, BTreeMap<u32, Vec<TickPacket>>>,
) -> Result<BTreeMap<u8, BTreeMap<u32, Vec<TickPacket>>>, String> {
    let mut current = BTreeMap::<u8, BTreeMap<u32, Vec<TickPacket>>>::new();
    for (player, fronts) in candidates {
        for (from_cell, packets) in fronts {
            for snapshot in packets {
                let Some(packet) = packet_state.find(&snapshot.packet_key) else {
                    continue;
                };
                let next_cell = packet.route.get(packet.route_index as usize + 1).copied();
                if packet.owner_player_id != player
                    || packet.current_cell != from_cell
                    || next_cell != Some(target_cell)
                    || cell_state(ctx, from_cell)?.owner_player_id != player
                {
                    continue;
                }
                current
                    .entry(player)
                    .or_default()
                    .entry(from_cell)
                    .or_default()
                    .push(packet);
            }
        }
    }
    Ok(current)
}

fn resolve_target_combat(
    ctx: &ReducerContext,
    packets: &mut PacketTickState,
    mut defender: CellState,
    fronts: Vec<FrontPackets>,
    logical_step: u64,
) -> Result<(), String> {
    let config = config(ctx)?;
    let target_coordinate = coordinate_for_cell(ctx, defender.cell_id)?;
    let target_terrain = terrain(ctx, defender.cell_id)?;
    let mut attacks = Vec::new();
    for front in &fronts {
        let limits = edge_runtime_limits(ctx, front.from_cell, front.to_cell)?
            .ok_or_else(|| "combat route contains an impassable edge".to_string())?;
        attacks.push(AttackFront {
            id: u64::from(front.from_cell),
            attacker: u32::from(front.attacker),
            from: coordinate_for_cell(ctx, front.from_cell)?,
            from_elevation: terrain(ctx, front.from_cell)?.elevation,
            offered: front.packets.iter().map(|packet| packet.infantry).sum(),
            frontage: limits.frontage,
        });
    }
    let combat_config = CombatConfig {
        max_elevation_step: u16::from(config.max_elevation_step),
        uphill_attack_bps: config.uphill_attack_bps,
        attacker_damage_bps: config.combat_lethality_bps,
        defender_damage_bps: config.combat_lethality_bps,
    };
    let resolution = resolve_edge_combat(
        target_coordinate,
        defender.infantry,
        target_terrain.elevation,
        &attacks,
        &combat_config,
    )
    .map_err(|error| format!("combat resolution failed: {error:?}"))?;

    let total_engaged: u64 = resolution
        .attacks
        .values()
        .map(|outcome| outcome.engaged)
        .sum();
    let extra_defender_casualty = u64::from(
        defender.infantry > 0 && total_engaged > 0 && resolution.defender_casualties == 0,
    );
    let defender_casualties = resolution
        .defender_casualties
        .saturating_add(extra_defender_casualty)
        .min(defender.infantry);

    let mut surviving_by_front = BTreeMap::new();
    for front in &fronts {
        let outcome = resolution
            .attacks
            .get(&u64::from(front.from_cell))
            .ok_or_else(|| "combat omitted an attack front".to_string())?;
        let extra_attacker = u64::from(
            outcome.engaged > 0
                && outcome.defense_allocated > 0
                && outcome.attacker_casualties == 0,
        );
        let attacker_casualties = outcome
            .attacker_casualties
            .saturating_add(extra_attacker)
            .min(outcome.offered);
        apply_attacker_casualties(
            ctx,
            packets,
            &front.packets,
            attacker_casualties,
            logical_step,
        )?;
        surviving_by_front.insert(
            front.from_cell,
            outcome.offered.saturating_sub(attacker_casualties),
        );
        let limits = edge_runtime_limits(ctx, front.from_cell, front.to_cell)?
            .ok_or_else(|| "combat route became impassable".to_string())?;
        let front_defender_casualties = outcome.defender_casualties
            + u64::from(extra_defender_casualty > 0 && front.from_cell == fronts[0].from_cell);
        let front_key = format!("{}:{}:{}", front.attacker, front.from_cell, front.to_cell);
        let next_front = CombatFront {
            front_key: front_key.clone(),
            attacker_player_id: front.attacker,
            defender_player_id: defender.owner_player_id,
            from_cell: front.from_cell,
            to_cell: front.to_cell,
            queued_infantry: outcome.waiting,
            attacker_engaged: outcome.engaged,
            defender_engaged: outcome.defense_allocated,
            attacker_casualties,
            defender_casualties: front_defender_casualties,
            frontage: limits.frontage,
            uphill: limits.uphill,
            logical_step,
        };
        if ctx.db.combat_front().front_key().find(&front_key).is_some() {
            ctx.db.combat_front().front_key().update(next_front);
        } else {
            ctx.db.combat_front().insert(next_front);
        }
    }

    defender.infantry = defender.infantry.saturating_sub(defender_casualties);
    defender.last_changed_step = logical_step;
    if defender_casualties > 0 {
        defender.last_policy_changed_step = logical_step;
    }
    ctx.db.cell_state().cell_id().update(defender.clone());
    trim_packets_at_cell(
        ctx,
        packets,
        defender.cell_id,
        defender.owner_player_id,
        logical_step,
    )?;

    if defender.infantry == 0 {
        let capturing_front = surviving_by_front
            .iter()
            .filter(|(_, strength)| **strength > 0)
            .max_by(|(left_cell, left_strength), (right_cell, right_strength)| {
                left_strength
                    .cmp(right_strength)
                    .then_with(|| right_cell.cmp(left_cell))
            })
            .map(|(&cell, _)| cell);
        if let Some(from_cell) = capturing_front {
            let front = fronts
                .iter()
                .find(|front| front.from_cell == from_cell)
                .ok_or_else(|| "capturing front is missing".to_string())?;
            occupy_after_combat(ctx, packets, front, defender, logical_step)?;
        }
    }
    Ok(())
}

fn apply_attacker_casualties(
    ctx: &ReducerContext,
    packet_state: &mut PacketTickState,
    packets: &[TickPacket],
    mut casualties: u64,
    logical_step: u64,
) -> Result<(), String> {
    let mut sorted = packets.to_vec();
    sorted.sort_unstable_by(|left, right| left.packet_key.cmp(&right.packet_key));
    for packet in sorted {
        if casualties == 0 {
            break;
        }
        let Some(current) = packet_state.find(&packet.packet_key) else {
            continue;
        };
        let lost = casualties.min(current.infantry);
        let mut source_state = cell_state(ctx, current.current_cell)?;
        source_state.infantry = source_state.infantry.saturating_sub(lost);
        source_state.last_changed_step = logical_step;
        source_state.last_policy_changed_step = logical_step;
        ctx.db.cell_state().cell_id().update(source_state);
        reduce_packet_metadata(ctx, packet_state, current, lost, logical_step, true)?;
        casualties -= lost;
    }
    Ok(())
}

fn occupy_after_combat(
    ctx: &ReducerContext,
    packet_state: &mut PacketTickState,
    front: &FrontPackets,
    mut target: CellState,
    logical_step: u64,
) -> Result<(), String> {
    let limits = edge_runtime_limits(ctx, front.from_cell, front.to_cell)?
        .ok_or_else(|| "capturing edge is impassable".to_string())?;
    let mut packets: Vec<_> = front
        .packets
        .iter()
        .filter_map(|packet| packet_state.find(&packet.packet_key))
        .collect();
    packets.sort_unstable_by(|left, right| left.packet_key.cmp(&right.packet_key));
    let offered: u64 = packets.iter().map(|packet| packet.infantry).sum();
    let occupancy = offered
        .min(limits.throughput_per_step)
        .min(target.military_capacity.saturating_sub(target.infantry));
    if occupancy == 0 {
        return Ok(());
    }

    let old_owner = target.owner_player_id;
    target.owner_player_id = front.attacker;
    target.infantry = target.infantry.saturating_add(occupancy);
    target.last_changed_step = logical_step;
    target.last_policy_changed_step = logical_step;
    ctx.db.cell_state().cell_id().update(target.clone());

    let mut remaining = occupancy;
    let mut capturing_expand_order = None::<u64>;
    for packet in packets {
        if remaining == 0 {
            break;
        }
        let moved = remaining.min(packet.infantry);
        if moved > 0
            && ctx
                .db
                .transfer_order()
                .order_id()
                .find(packet.order_id)
                .is_some_and(|order| is_expansion_wave_order(order.kind))
        {
            capturing_expand_order = Some(
                capturing_expand_order
                    .map_or(packet.order_id, |current| current.min(packet.order_id)),
            );
        }
        let mut source = cell_state(ctx, packet.current_cell)?;
        source.infantry = source.infantry.saturating_sub(moved);
        source.last_changed_step = logical_step;
        source.last_policy_changed_step = logical_step;
        ctx.db.cell_state().cell_id().update(source);
        advance_packet(
            ctx,
            packet_state,
            &packet,
            moved,
            logical_step,
            PacketArrival::Capture,
        )?;
        remaining -= moved;
    }
    station_capture_garrison(
        ctx,
        packet_state,
        target.cell_id,
        front.attacker,
        logical_step,
    )?;
    record_capture(ctx, target.cell_id, old_owner, front.attacker)?;
    if let Some(order_id) = capturing_expand_order {
        record_expand_garrison_debt(ctx, order_id, target.cell_id, front.attacker)?;
    }
    Ok(())
}

fn record_expand_garrison_debt(
    ctx: &ReducerContext,
    order_id: u64,
    cell_id: u32,
    owner_player_id: u8,
) -> Result<(), String> {
    let wave = ctx
        .db
        .expansion_wave()
        .order_id()
        .find(order_id)
        .ok_or_else(|| format!("capturing expand order {order_id} has no topology"))?;
    if wave.selected_cells.binary_search(&cell_id).is_ok() {
        return Err("expand capture debt cannot be recorded inside its seed".into());
    }
    if wave
        .outside_depths
        .get(cell_id as usize)
        .is_none_or(|depth| *depth == u16::MAX)
    {
        return Err("expand captured a cell outside its wave topology".into());
    }
    let cell = cell_state(ctx, cell_id)?;
    if cell.owner_player_id != owner_player_id {
        return Err("expand capture owner changed before garrison debt was recorded".into());
    }
    let required = occupation_garrison(cell.military_capacity, terrain(ctx, cell_id)?.terrain);
    let allocated = allocated_infantry_at_cell(ctx, owner_player_id, cell_id);
    let unallocated = cell.infantry.saturating_sub(allocated);
    let debt = required.saturating_sub(unallocated);
    if debt > 0 {
        ctx.db
            .expansion_garrison_debt()
            .insert(ExpansionGarrisonDebt {
                cell_id,
                owner_player_id,
                remaining_infantry: debt,
            });
    }
    Ok(())
}

fn record_capture(
    ctx: &ReducerContext,
    cell_id: u32,
    old_owner: u8,
    new_owner: u8,
) -> Result<(), String> {
    if old_owner == new_owner || !terrain(ctx, cell_id)?.capturable {
        return Ok(());
    }
    // Ownership changes eagerly invalidate the prior owner's capture-scoped
    // debt, even when this capture was performed by a different order kind.
    ctx.db.expansion_garrison_debt().cell_id().delete(cell_id);
    let mut match_state = state(ctx)?;
    match_state.ownership_revision = match_state
        .ownership_revision
        .checked_add(1)
        .ok_or_else(|| "ownership revision overflow".to_string())?;
    match old_owner {
        PLAYER_ONE => {
            match_state.player_one_controlled = match_state.player_one_controlled.saturating_sub(1);
        }
        PLAYER_TWO => {
            match_state.player_two_controlled = match_state.player_two_controlled.saturating_sub(1);
        }
        _ => {}
    }
    match new_owner {
        PLAYER_ONE => match_state.player_one_controlled += 1,
        PLAYER_TWO => match_state.player_two_controlled += 1,
        _ => {}
    }
    let controlled = controlled_cells_for_owner(
        new_owner,
        match_state.player_one_controlled,
        match_state.player_two_controlled,
    );
    if controlled.is_some_and(|controlled| controlled >= match_state.required_control) {
        match_state.phase = MatchPhase::Completed;
        match_state.winner_player_id = new_owner;
        match_state.completed_at_us = crate::timestamp_us(ctx);
    }
    ctx.db.match_state().singleton_id().update(match_state);
    Ok(())
}

fn controlled_cells_for_owner(owner: u8, player_one: u64, player_two: u64) -> Option<u64> {
    match owner {
        PLAYER_ONE => Some(player_one),
        PLAYER_TWO => Some(player_two),
        _ => None,
    }
}

fn advance_packet(
    ctx: &ReducerContext,
    packets: &mut PacketTickState,
    packet: &TickPacket,
    moved: u64,
    logical_step: u64,
    arrival: PacketArrival,
) -> Result<(), String> {
    let Some(mut packet) = packets.find(&packet.packet_key) else {
        return Ok(());
    };
    if moved == 0 || moved > packet.infantry {
        return Err("invalid packet movement amount".into());
    }
    let order = ctx
        .db
        .transfer_order()
        .order_id()
        .find(packet.order_id)
        .ok_or_else(|| "packet order is missing".to_string())?;
    if is_expansion_wave_order(order.kind) {
        return advance_expand_packet(ctx, packets, &packet, moved, logical_step);
    }
    let next_index = packet.route_index + 1;
    let next_cell = *packet
        .route
        .get(next_index as usize)
        .ok_or_else(|| "packet route ended before movement".to_string())?;

    if next_index as usize + 1 == packet.route.len() && arrival.may_extend_sustained_push() {
        extend_sustained_lane(ctx, packets, &packet, next_cell, logical_step)?;
        packet = packets
            .find(&packet.packet_key)
            .ok_or_else(|| "push packet disappeared while extending its lane".to_string())?;
    }

    if moved == packet.infantry {
        packets.delete(ctx, &packet.packet_key);
    } else {
        let mut remainder = packet.clone();
        remainder.infantry -= moved;
        remainder.updated_step = logical_step;
        packets.update(ctx, remainder);
    }
    if packet_has_pending_source(&packet) {
        decrement_source_queue(ctx, packet.order_id, packet.origin_cell, moved)?;
    }

    if next_index as usize + 1 == packet.route.len() {
        increment_destination_received(ctx, packet.order_id, packet.destination_cell, moved)?;
        if arrival.may_extend_sustained_push() && order.kind == OrderKind::PushFront {
            let direction = push_lane_direction(ctx, &order, &packet)?;
            settle_stopped_sustained_lane(
                ctx,
                packets,
                packet.order_id,
                packet.destination_cell,
                direction,
                logical_step,
            )?;
        }
        return Ok(());
    }

    let child_key = packet_key(
        packet.order_id,
        packet.origin_cell,
        packet.destination_cell,
        next_cell,
        next_index,
    );
    if let Some(mut existing) = packets.find(&child_key) {
        existing.infantry = existing
            .infantry
            .checked_add(moved)
            .ok_or_else(|| "packet strength overflow".to_string())?;
        existing.updated_step = logical_step;
        packets.update(ctx, existing);
    } else {
        packets.insert(
            ctx,
            TickPacket {
                packet_key: child_key,
                order_id: packet.order_id,
                owner_player_id: packet.owner_player_id,
                origin_cell: packet.origin_cell,
                current_cell: next_cell,
                destination_cell: packet.destination_cell,
                infantry: moved,
                route_index: next_index,
                route: packet.route.clone(),
                updated_step: logical_step,
            },
        );
    }
    Ok(())
}

fn advance_expand_packet(
    ctx: &ReducerContext,
    packets: &mut PacketTickState,
    packet: &TickPacket,
    moved: u64,
    logical_step: u64,
) -> Result<(), String> {
    if packet.route_index != 0 || packet.route.len() != 2 || packet.route[0] != packet.current_cell
    {
        return Err("expand edge packet has an invalid one-edge route".into());
    }
    let next_cell = packet.route[1];
    if moved == packet.infantry {
        packets.delete(ctx, &packet.packet_key);
    } else {
        let mut remainder = packet.clone();
        remainder.infantry -= moved;
        remainder.updated_step = logical_step;
        packets.update(ctx, remainder);
    }
    if packet_has_pending_source(packet) {
        decrement_source_queue(ctx, packet.order_id, packet.origin_cell, moved)?;
    }

    let rest_key = packet_key(
        packet.order_id,
        EXPANSION_AGGREGATE_ORIGIN,
        next_cell,
        next_cell,
        0,
    );
    if let Some(mut resting) = packets.find(&rest_key) {
        resting.infantry = merged_expand_strength(resting.infantry, moved)?;
        resting.updated_step = logical_step;
        packets.update(ctx, resting);
    } else {
        packets.insert(
            ctx,
            TickPacket {
                packet_key: rest_key,
                order_id: packet.order_id,
                owner_player_id: packet.owner_player_id,
                origin_cell: EXPANSION_AGGREGATE_ORIGIN,
                current_cell: next_cell,
                destination_cell: next_cell,
                infantry: moved,
                route_index: 0,
                route: Rc::from([next_cell]),
                updated_step: logical_step,
            },
        );
    }
    Ok(())
}

fn packet_has_pending_source(packet: &TickPacket) -> bool {
    packet.route_index == 0 && packet.origin_cell != EXPANSION_AGGREGATE_ORIGIN
}

/// Resolves the immutable local normal of one Push lane from its persisted
/// route. `destination_cell` is the first cell beyond the original selection,
/// and the preceding route cell is therefore the selected boundary source.
/// Later sustained layers append after this pair without changing it.
fn push_lane_direction(
    ctx: &ReducerContext,
    order: &TransferOrder,
    packet: &TickPacket,
) -> Result<Axial, String> {
    if order.kind != OrderKind::PushFront || packet.order_id != order.order_id {
        return Err("lane direction requires a packet from its Push order".into());
    }
    let direction = lane_direction_from_route(&packet.route, packet.destination_cell, |cell_id| {
        coordinate_for_cell(ctx, cell_id)
    })?;
    let stored = Axial::new(order.orientation_q, order.orientation_r);
    if stored != Axial::ZERO && stored != direction {
        return Err("directional push lane disagrees with its stored orientation".into());
    }
    Ok(direction)
}

fn lane_direction_from_route(
    route: &[u32],
    destination_cell: u32,
    mut coordinate: impl FnMut(u32) -> Result<Axial, String>,
) -> Result<Axial, String> {
    let destination_index = route
        .iter()
        .position(|cell_id| *cell_id == destination_cell)
        .ok_or_else(|| "push route does not contain its stable lane destination".to_string())?;
    let boundary_index = destination_index
        .checked_sub(1)
        .ok_or_else(|| "push lane destination has no preceding boundary cell".to_string())?;
    let boundary = coordinate(route[boundary_index])?;
    let destination = coordinate(destination_cell)?;
    let direction = destination - boundary;
    if !Axial::DIRECTIONS.contains(&direction) {
        return Err("push lane route has an invalid local direction".into());
    }
    Ok(direction)
}

/// Extends every packet in one straight push lane by one outward cell.
///
/// `destination_cell` remains the stable first-layer anchor. Local-arc orders
/// may approach one anchor from several directions, so only packets sharing
/// both the anchor and the route-derived direction may share later layers.
fn extend_sustained_lane(
    ctx: &ReducerContext,
    packets: &mut PacketTickState,
    packet: &TickPacket,
    reached_cell: u32,
    logical_step: u64,
) -> Result<bool, String> {
    let Some(order) = ctx.db.transfer_order().order_id().find(packet.order_id) else {
        return Err("push order is missing while extending a lane".into());
    };
    if order.status != OrderStatus::Active || order.kind != OrderKind::PushFront {
        return Ok(false);
    }

    let current = packets
        .find(&packet.packet_key)
        .ok_or_else(|| "push packet is missing while extending a lane".to_string())?;
    if current.route.last().copied() != Some(reached_cell) {
        return Ok(true);
    }

    let direction = push_lane_direction(ctx, &order, &current)?;
    let match_config = config(ctx)?;
    let next_coordinate = coordinate_for_cell(ctx, reached_cell)? + direction;
    let Some(next_cell) = cell_id_for_coordinate(&match_config, next_coordinate) else {
        return Ok(false);
    };
    let next_terrain = terrain(ctx, next_cell)?;
    let next_state = cell_state(ctx, next_cell)?;
    let traversable = edge_runtime_limits(ctx, reached_cell, next_cell)?.is_some();
    let eligible = sustained_push_target_is_eligible(
        order.player_id,
        next_state.owner_player_id,
        next_terrain.passable,
        next_terrain.capturable,
        traversable,
    );
    if !eligible {
        return Ok(false);
    }

    let candidates = packets
        .by_order_destination(order.order_id, packet.destination_cell)
        .cloned()
        .collect::<Vec<_>>();
    let mut extended = false;
    for mut candidate in candidates {
        if push_lane_direction(ctx, &order, &candidate)? != direction {
            continue;
        }
        let mut route = candidate.route.to_vec();
        if !append_lane_layer(&mut route, reached_cell, next_cell) {
            continue;
        }
        candidate.route = Rc::from(route);
        candidate.updated_step = logical_step;
        packets.update(ctx, candidate);
        extended = true;
    }
    Ok(extended)
}

fn sustained_push_target_is_eligible(
    player_id: u8,
    target_owner: u8,
    passable: bool,
    capturable: bool,
    traversable: bool,
) -> bool {
    passable && capturable && traversable && target_owner != player_id
}

fn append_lane_layer(route: &mut Vec<u32>, reached_cell: u32, next_cell: u32) -> bool {
    if route.last().copied() != Some(reached_cell) {
        return false;
    }
    route.push(next_cell);
    true
}

fn decrement_source_queue(
    ctx: &ReducerContext,
    order_id: u64,
    origin_cell: u32,
    amount: u64,
) -> Result<(), String> {
    let key = format!("{order_id}:{origin_cell}");
    let mut source = ctx
        .db
        .transfer_source()
        .source_key()
        .find(&key)
        .ok_or_else(|| "transfer source row is missing".to_string())?;
    source.queued_infantry = source.queued_infantry.saturating_sub(amount);
    ctx.db.transfer_source().source_key().update(source);
    Ok(())
}

fn increment_destination_received(
    ctx: &ReducerContext,
    order_id: u64,
    destination_cell: u32,
    amount: u64,
) -> Result<(), String> {
    let key = format!("{order_id}:{destination_cell}");
    let mut destination = ctx
        .db
        .transfer_destination()
        .destination_key()
        .find(&key)
        .ok_or_else(|| "transfer destination row is missing".to_string())?;
    destination.received_infantry = destination
        .received_infantry
        .saturating_add(amount)
        .min(destination.target_infantry);
    ctx.db
        .transfer_destination()
        .destination_key()
        .update(destination);
    let mut order = ctx
        .db
        .transfer_order()
        .order_id()
        .find(order_id)
        .ok_or_else(|| "transfer order is missing".to_string())?;
    order.delivered_infantry = order.delivered_infantry.saturating_add(amount);
    ctx.db.transfer_order().order_id().update(order);
    Ok(())
}

fn station_capture_garrison(
    ctx: &ReducerContext,
    packets: &mut PacketTickState,
    cell_id: u32,
    owner_player_id: u8,
    logical_step: u64,
) -> Result<(), String> {
    let cell = cell_state(ctx, cell_id)?;
    let terrain_class = terrain(ctx, cell_id)?.terrain;
    let required = occupation_garrison(cell.military_capacity, terrain_class).min(cell.infantry);
    let allocated = allocated_infantry_at_cell(ctx, owner_player_id, cell_id);
    let mut remaining = additional_garrison_required(required, cell.infantry, allocated);
    if remaining == 0 {
        return Ok(());
    }

    let mut candidates = packets
        .by_cell(cell_id)
        .filter(|packet| packet.owner_player_id == owner_player_id)
        .filter(|packet| {
            ctx.db
                .transfer_order()
                .order_id()
                .find(packet.order_id)
                .is_some_and(|order| order.status == OrderStatus::Active)
        })
        .cloned()
        .collect::<Vec<_>>();
    candidates.sort_unstable_by(|left, right| left.packet_key.cmp(&right.packet_key));
    for packet in candidates {
        if remaining == 0 {
            break;
        }
        let stationed = remaining.min(packet.infantry);
        station_packet_allocation(ctx, packets, &packet, stationed, logical_step)?;
        remaining -= stationed;
    }
    Ok(())
}

fn additional_garrison_required(required: u64, total: u64, allocated: u64) -> u64 {
    required.saturating_sub(total.saturating_sub(allocated))
}

fn occupation_garrison(military_capacity: u64, terrain: TerrainClass) -> u64 {
    if military_capacity == 0 || terrain == TerrainClass::Water {
        return 0;
    }
    let base = military_capacity.div_ceil(20).max(1);
    let multiplier = match terrain {
        TerrainClass::Plains => 1,
        TerrainClass::Hills => 2,
        TerrainClass::Mountain => 3,
        TerrainClass::Water => 0,
    };
    base.saturating_mul(multiplier).min(military_capacity)
}

/// Releases every still-allocated survivor in a lane where the directional
/// ray reached friendly territory, the map boundary, or an impassable edge.
fn settle_stopped_sustained_lane(
    ctx: &ReducerContext,
    packets: &mut PacketTickState,
    order_id: u64,
    lane_anchor: u32,
    direction: Axial,
    logical_step: u64,
) -> Result<(), String> {
    let Some(order) = ctx.db.transfer_order().order_id().find(order_id) else {
        return Err("order is missing while settling a stopped push lane".into());
    };
    if order.kind != OrderKind::PushFront {
        return Ok(());
    }
    let mut candidates = packets
        .by_order_destination(order_id, lane_anchor)
        .cloned()
        .collect::<Vec<_>>();
    candidates.sort_unstable_by(|left, right| left.packet_key.cmp(&right.packet_key));
    for packet in candidates {
        if push_lane_direction(ctx, &order, &packet)? != direction {
            continue;
        }
        station_packet_allocation(ctx, packets, &packet, packet.infantry, logical_step)?;
    }
    Ok(())
}

/// Retires an allocation without removing infantry from the cell. The public
/// `delivered_infantry` counter therefore means all surviving strength that
/// has left this operation: endpoint arrivals, occupation garrisons, and
/// release-in-place at an automatic stop.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct StationAccounting {
    stationed: u64,
    packet_remaining: u64,
    delivered_after: u64,
}

fn station_accounting(
    packet_infantry: u64,
    amount: u64,
    delivered_before: u64,
) -> Result<StationAccounting, String> {
    let stationed = amount.min(packet_infantry);
    let delivered_after = delivered_before
        .checked_add(stationed)
        .ok_or_else(|| "stationed infantry overflow".to_string())?;
    Ok(StationAccounting {
        stationed,
        packet_remaining: packet_infantry - stationed,
        delivered_after,
    })
}

fn station_packet_allocation(
    ctx: &ReducerContext,
    packets: &mut PacketTickState,
    packet: &TickPacket,
    amount: u64,
    logical_step: u64,
) -> Result<(), String> {
    let Some(mut current) = packets.find(&packet.packet_key) else {
        return Ok(());
    };
    if amount == 0 || current.infantry == 0 {
        return Ok(());
    }
    let mut order = ctx
        .db
        .transfer_order()
        .order_id()
        .find(current.order_id)
        .ok_or_else(|| "order is missing while stationing infantry".to_string())?;
    let accounting = station_accounting(current.infantry, amount, order.delivered_infantry)?;
    if accounting.packet_remaining == 0 {
        packets.delete(ctx, &current.packet_key);
    } else {
        current.infantry = accounting.packet_remaining;
        current.updated_step = logical_step;
        packets.update(ctx, current.clone());
    }
    if packet_has_pending_source(&current) {
        decrement_source_queue(
            ctx,
            current.order_id,
            current.origin_cell,
            accounting.stationed,
        )?;
    }
    order.delivered_infantry = accounting.delivered_after;
    order.updated_step = logical_step;
    ctx.db.transfer_order().order_id().update(order);
    Ok(())
}

fn reduce_packet_metadata(
    ctx: &ReducerContext,
    packets: &mut PacketTickState,
    mut packet: TickPacket,
    amount: u64,
    logical_step: u64,
    count_casualty: bool,
) -> Result<(), String> {
    let lost = amount.min(packet.infantry);
    if lost == packet.infantry {
        packets.delete(ctx, &packet.packet_key);
    } else {
        packet.infantry -= lost;
        packet.updated_step = logical_step;
        packets.update(ctx, packet.clone());
    }
    if packet_has_pending_source(&packet) {
        decrement_source_queue(ctx, packet.order_id, packet.origin_cell, lost)?;
    }
    if count_casualty {
        let mut order = ctx
            .db
            .transfer_order()
            .order_id()
            .find(packet.order_id)
            .ok_or_else(|| "casualty order is missing".to_string())?;
        order.casualty_infantry = order.casualty_infantry.saturating_add(lost);
        order.updated_step = logical_step;
        ctx.db.transfer_order().order_id().update(order);
    }
    Ok(())
}

fn trim_packets_at_cell(
    ctx: &ReducerContext,
    packet_state: &mut PacketTickState,
    cell_id: u32,
    owner_player_id: u8,
    logical_step: u64,
) -> Result<(), String> {
    let cell = cell_state(ctx, cell_id)?;
    let mut packets: Vec<_> = packet_state
        .by_cell(cell_id)
        .filter(|packet| packet.owner_player_id == owner_player_id)
        .cloned()
        .collect();
    packets.sort_unstable_by(|left, right| right.packet_key.cmp(&left.packet_key));
    let allocated: u64 = packets.iter().map(|packet| packet.infantry).sum();
    let mut trim = packet_trim_required(
        cell.owner_player_id,
        owner_player_id,
        cell.infantry,
        allocated,
    );
    for packet in packets {
        if trim == 0 {
            break;
        }
        let lost = trim.min(packet.infantry);
        reduce_packet_metadata(ctx, packet_state, packet, lost, logical_step, true)?;
        trim -= lost;
    }
    Ok(())
}

/// Returns how much allocation metadata is no longer backed by infantry at a
/// cell. Strength in a captured cell belongs exclusively to its current owner;
/// it must never keep a displaced owner's packets alive.
fn packet_trim_required(
    cell_owner: u8,
    packet_owner: u8,
    cell_infantry: u64,
    allocated: u64,
) -> u64 {
    let backing = if cell_owner == packet_owner {
        cell_infantry
    } else {
        0
    };
    allocated.saturating_sub(backing)
}

fn trim_all_overallocated_packets(
    ctx: &ReducerContext,
    packet_state: &mut PacketTickState,
    logical_step: u64,
) -> Result<(), String> {
    let mut packets_by_location = BTreeMap::<(u32, u8), Vec<TickPacket>>::new();
    for packet in packet_state.iter() {
        packets_by_location
            .entry((packet.current_cell, packet.owner_player_id))
            .or_default()
            .push(packet.clone());
    }
    for ((cell_id, owner), mut packets) in packets_by_location {
        let cell = cell_state(ctx, cell_id)?;
        packets.sort_unstable_by(|left, right| right.packet_key.cmp(&left.packet_key));
        let allocated = packets.iter().map(|packet| packet.infantry).sum();
        let mut trim = packet_trim_required(cell.owner_player_id, owner, cell.infantry, allocated);
        for packet in packets {
            if trim == 0 {
                break;
            }
            let lost = trim.min(packet.infantry);
            reduce_packet_metadata(ctx, packet_state, packet, lost, logical_step, true)?;
            trim -= lost;
        }
    }
    Ok(())
}

fn finalize_orders(
    ctx: &ReducerContext,
    packets: &PacketTickState,
    logical_step: u64,
) -> Result<(), String> {
    let mut active_strength = BTreeMap::<u64, u64>::new();
    for packet in packets.iter() {
        *active_strength.entry(packet.order_id).or_default() += packet.infantry;
    }
    let orders: Vec<TransferOrder> = ctx
        .db
        .transfer_order()
        .order_by_status()
        .filter(OrderStatus::Active)
        .collect();
    for mut order in orders {
        let in_transit = active_strength.get(&order.order_id).copied().unwrap_or(0);
        let status = finalized_order_status(
            order.committed_infantry,
            in_transit,
            order.delivered_infantry,
            order.casualty_infantry,
        )
        .map_err(|error| format!("order {} {error}", order.order_id))?;
        let changed = order.in_transit_infantry != in_transit || order.status != status;
        if status == OrderStatus::Completed {
            order.in_transit_infantry = in_transit;
            order.status = status;
            complete_retreat_abandonments(ctx, packets, &order, logical_step)?;
            ctx.db.expansion_wave().order_id().delete(order.order_id);
        }
        if changed {
            order.in_transit_infantry = in_transit;
            order.status = status;
            order.updated_step = logical_step;
            ctx.db.transfer_order().order_id().update(order);
        }
    }
    Ok(())
}

fn complete_retreat_abandonments(
    ctx: &ReducerContext,
    packets: &PacketTickState,
    order: &TransferOrder,
    logical_step: u64,
) -> Result<(), String> {
    let candidates = ctx
        .db
        .retreat_abandonment()
        .abandonment_by_order()
        .filter(order.order_id)
        .collect::<Vec<_>>();
    for candidate in candidates {
        let mut cell = cell_state(ctx, candidate.cell_id)?;
        let ground = terrain(ctx, candidate.cell_id)?;
        let has_live_packet_claim = packets.iter().any(|packet| {
            packet.owner_player_id == order.player_id
                && (packet.current_cell == candidate.cell_id
                    || packet.destination_cell == candidate.cell_id)
        });
        let has_active_destination_reservation =
            ctx.db.transfer_destination().iter().any(|target| {
                target.cell_id == candidate.cell_id
                    && target.order_id != order.order_id
                    && target.target_infantry > target.received_infantry
                    && ctx
                        .db
                        .transfer_order()
                        .order_id()
                        .find(target.order_id)
                        .is_some_and(|other| {
                            other.player_id == order.player_id
                                && other.status == OrderStatus::Active
                        })
            });
        let may_abandon = retreat_abandonment_is_safe(
            cell.owner_player_id == order.player_id,
            ground.passable,
            ground.capturable,
            cell.infantry,
            allocated_infantry_at_cell(ctx, order.player_id, candidate.cell_id),
            has_live_packet_claim,
            has_active_destination_reservation,
        );
        if may_abandon {
            let old_owner = cell.owner_player_id;
            cell.owner_player_id = NEUTRAL_PLAYER;
            cell.last_changed_step = logical_step;
            cell.last_policy_changed_step = logical_step;
            ctx.db.cell_state().cell_id().update(cell);
            record_capture(ctx, candidate.cell_id, old_owner, NEUTRAL_PLAYER)?;
        }
        ctx.db
            .retreat_abandonment()
            .abandonment_key()
            .delete(&candidate.abandonment_key);
    }
    Ok(())
}

fn retreat_abandonment_is_safe(
    still_owned: bool,
    passable: bool,
    capturable: bool,
    infantry: u64,
    allocated: u64,
    has_live_packet_claim: bool,
    has_active_destination_reservation: bool,
) -> bool {
    still_owned
        && passable
        && capturable
        && infantry == 0
        && allocated == 0
        && !has_live_packet_claim
        && !has_active_destination_reservation
}

fn finalized_order_status(
    committed: u64,
    in_transit: u64,
    delivered: u64,
    casualties: u64,
) -> Result<OrderStatus, String> {
    let accounted = in_transit
        .checked_add(delivered)
        .and_then(|total| total.checked_add(casualties))
        .ok_or_else(|| "has overflowing infantry accounting".to_string())?;
    if accounted != committed {
        return Err(format!(
            "violates infantry conservation: committed {committed}, accounted {accounted}"
        ));
    }
    Ok(if in_transit == 0 {
        OrderStatus::Completed
    } else {
        OrderStatus::Active
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone)]
    struct FunnelPacket {
        route: Vec<Axial>,
        route_index: usize,
        infantry: u64,
    }

    fn run_policy_pipeline(
        mut map: HexMap,
        mut packets: Vec<FunnelPacket>,
        station_blocked_remainders: bool,
        max_passes: usize,
    ) -> (HexMap, bool, usize) {
        let movement = MovementConfig::default();
        let logistics = LogisticsConfig {
            default_military_capacity: 100,
            default_edge_throughput: 1_000,
            default_combat_frontage: 100,
        };
        let mut saw_capacity_backpressure = false;
        let mut passes = 0;
        while !packets.is_empty() && passes < max_passes {
            passes += 1;
            let intents = packets
                .iter()
                .enumerate()
                .map(|(index, packet)| MovementIntent {
                    id: index as u64 + 1,
                    priority: 0,
                    owner: 1,
                    from: packet.route[packet.route_index],
                    to: packet.route[packet.route_index + 1],
                    requested: packet.infantry,
                })
                .collect::<Vec<_>>();
            let step = movement_step(&mut map, &intents, &movement, &logistics)
                .expect("capacity-safe friendly policy pipeline");
            let mut next_packets = Vec::new();
            for (index, packet) in packets.into_iter().enumerate() {
                let outcome = &step.outcomes[&(index as u64 + 1)];
                let capacity_blocked = outcome.limits.contains(&MovementLimit::DestinationCapacity);
                saw_capacity_backpressure |= capacity_blocked;
                let remainder = packet.infantry - outcome.approved;
                if remainder > 0 && !(station_blocked_remainders && capacity_blocked) {
                    next_packets.push(FunnelPacket {
                        infantry: remainder,
                        ..packet.clone()
                    });
                }
                if outcome.approved > 0 && packet.route_index + 2 < packet.route.len() {
                    next_packets.push(FunnelPacket {
                        route_index: packet.route_index + 1,
                        infantry: outcome.approved,
                        ..packet
                    });
                }
            }
            packets = next_packets;
        }
        assert!(packets.is_empty(), "the queued policy pipeline must drain");
        (map, saw_capacity_backpressure, passes)
    }

    fn run_capacity_funnel(station_blocked_remainders: bool) -> (HexMap, bool, usize) {
        let center = Axial::ZERO;
        let source_a = Axial::new(-1, 0);
        let source_b = Axial::new(0, -1);
        let source_c = Axial::new(-1, 1);
        let destination_a = Axial::new(1, 0);
        let destination_b = Axial::new(0, 1);
        let destination_c = Axial::new(1, -1);
        let mut map = HexMap::new();
        for (coordinate, infantry) in [
            (source_a, 100),
            (source_b, 100),
            (source_c, 100),
            (center, 100),
            (destination_a, 0),
            (destination_b, 0),
            (destination_c, 0),
        ] {
            let mut cell = hex_core::Cell::ground(coordinate, 0, Some(1), 100);
            cell.forces.infantry = infantry;
            map.insert(cell);
        }

        // This is a balanced target for 400 infantry over seven equal cells:
        // 57 everywhere with the deterministic remainder at source C. Every
        // source-to-destination route crosses the already-full center, which
        // reproduces the large-cluster funnel that used to strand most of a
        // policy order at intermediate capacity stops.
        let packets = vec![
            FunnelPacket {
                route: vec![center, destination_a],
                route_index: 0,
                infantry: 43,
            },
            FunnelPacket {
                route: vec![source_a, center, destination_a],
                route_index: 0,
                infantry: 14,
            },
            FunnelPacket {
                route: vec![source_a, center, destination_b],
                route_index: 0,
                infantry: 29,
            },
            FunnelPacket {
                route: vec![source_b, center, destination_b],
                route_index: 0,
                infantry: 28,
            },
            FunnelPacket {
                route: vec![source_b, center, destination_c],
                route_index: 0,
                infantry: 15,
            },
            FunnelPacket {
                route: vec![source_c, center, destination_c],
                route_index: 0,
                infantry: 42,
            },
        ];
        run_policy_pipeline(map, packets, station_blocked_remainders, 16)
    }

    fn test_order(kind: OrderKind, status: OrderStatus) -> TransferOrder {
        TransferOrder {
            order_id: 1,
            player_id: 1,
            client_command_id: 1,
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

    fn test_packet(origin: u32, current: u32, destination: u32, route: Vec<u32>) -> TickPacket {
        TickPacket {
            packet_key: "test".into(),
            order_id: 1,
            owner_player_id: 1,
            origin_cell: origin,
            current_cell: current,
            destination_cell: destination,
            infantry: 10,
            route_index: 0,
            route: Rc::from(route),
            updated_step: 0,
        }
    }

    #[test]
    fn packet_tick_indexes_track_insertions_and_removals() {
        let mut state = PacketTickState::default();
        let mut first = test_packet(10, 10, 11, vec![10, 11]);
        first.packet_key = "1:10:11".into();
        let mut second = test_packet(20, 20, 21, vec![20, 21]);
        second.packet_key = "2:20:21".into();
        second.order_id = 2;
        state.track(first.clone());
        state.track(second.clone());

        assert_eq!(state.iter().count(), 2);
        assert_eq!(state.by_order(1).count(), 1);
        assert_eq!(state.by_order(2).count(), 1);
        assert_eq!(state.by_cell(10).count(), 1);
        assert_eq!(state.by_order_destination(2, 21).count(), 1);

        state.untrack(&first.packet_key);
        assert!(state.find(&first.packet_key).is_none());
        assert_eq!(state.by_order(1).count(), 0);
        assert_eq!(state.by_cell(10).count(), 0);
        assert_eq!(state.iter().count(), 1);
    }

    #[test]
    fn expand_resting_nodes_and_one_edge_packets_are_unambiguous() {
        let resting = test_packet(5, 5, 5, vec![5]);
        let edge = test_packet(5, 5, 6, vec![5, 6]);
        assert!(expansion_packet_is_resting(&resting));
        assert!(!expansion_packet_is_resting(&edge));
        assert!(packet_has_pending_source(&resting));

        let aggregate = test_packet(EXPANSION_AGGREGATE_ORIGIN, 8, 8, vec![8]);
        assert!(expansion_packet_is_resting(&aggregate));
        assert!(!packet_has_pending_source(&aggregate));
    }

    #[test]
    fn wave_depth_transitions_are_monotonic_and_cannot_form_cycles_or_rays() {
        assert!(wave_depth_allows_child(
            WaveNodeDepth::Seed(2),
            WaveNodeDepth::Seed(1)
        ));
        assert!(wave_depth_allows_child(
            WaveNodeDepth::Seed(0),
            WaveNodeDepth::Outside(1)
        ));
        assert!(wave_depth_allows_child(
            WaveNodeDepth::Outside(3),
            WaveNodeDepth::Outside(4)
        ));
        assert!(!wave_depth_allows_child(
            WaveNodeDepth::Seed(1),
            WaveNodeDepth::Seed(1)
        ));
        assert!(!wave_depth_allows_child(
            WaveNodeDepth::Outside(3),
            WaveNodeDepth::Outside(5)
        ));
        assert!(!wave_depth_allows_child(
            WaveNodeDepth::Outside(3),
            WaveNodeDepth::Outside(2)
        ));

        let wave = ExpansionWave {
            order_id: 1,
            selected_cells: vec![2, 4],
            seed_depths: vec![1, 0],
            outside_depths: vec![u16::MAX, 1, u16::MAX, 2, u16::MAX],
            split_cursors: vec![0; 5],
            focus_cell_id: None,
            target_cells: Vec::new(),
        };
        assert_eq!(wave_node_depth(&wave, 2), Some(WaveNodeDepth::Seed(1)));
        assert_eq!(wave_node_depth(&wave, 4), Some(WaveNodeDepth::Seed(0)));
        assert_eq!(wave_node_depth(&wave, 1), Some(WaveNodeDepth::Outside(1)));
        assert_eq!(wave_node_depth(&wave, 0), None);
    }

    #[test]
    fn shared_child_arrivals_merge_with_exact_checked_conservation() {
        assert_eq!(merged_expand_strength(17, 25), Ok(42));
        assert!(merged_expand_strength(u64::MAX, 1).is_err());
    }

    #[test]
    fn focused_cluster_expansion_biases_without_zeroing_the_rear() {
        let parent = Axial::ZERO;
        let children = [Axial::new(1, 0), Axial::new(1, -1), Axial::new(-1, 0)];
        let weights = focus_weights_for_coordinates(parent, &children, Axial::new(3, 0));
        assert_eq!(weights, vec![3, 2, 1]);

        let split = weighted_branch_allocations_rotated(&[12], &weights, 0).unwrap();
        let by_child = split.allocations.into_iter().fold(
            vec![0_u64; children.len()],
            |mut totals, allocation| {
                totals[allocation.child_index] += allocation.amount;
                totals
            },
        );
        assert_eq!(by_child, vec![6, 4, 2]);
        assert!(by_child[2] > 0, "the rear perimeter must remain active");
    }

    #[test]
    fn focused_cluster_expansion_recomputes_opposed_front_vectors_locally() {
        let focus = Axial::ZERO;
        let west_parent = Axial::new(-1, 0);
        let east_parent = Axial::new(1, 0);

        let west_children = [focus, Axial::new(-2, 0)];
        let east_children = [focus, Axial::new(2, 0)];
        let west_weights = focus_weights_for_coordinates(west_parent, &west_children, focus);
        let east_weights = focus_weights_for_coordinates(east_parent, &east_children, focus);

        assert_eq!(west_weights, vec![3, 1]);
        assert_eq!(east_weights, vec![3, 1]);

        let split_totals = |weights: &[u8]| {
            weighted_branch_allocations_rotated(&[20], weights, 0)
                .unwrap()
                .allocations
                .into_iter()
                .fold(vec![0_u64; weights.len()], |mut totals, allocation| {
                    totals[allocation.child_index] += allocation.amount;
                    totals
                })
        };
        let west_split = split_totals(&west_weights);
        let east_split = split_totals(&east_weights);

        assert_eq!(west_split, vec![15, 5]);
        assert_eq!(east_split, vec![15, 5]);
        assert_eq!(west_split.iter().chain(&east_split).sum::<u64>(), 40);
        assert!(west_split[1] > 0 && east_split[1] > 0);
    }

    #[test]
    fn attack_wave_can_branch_and_turn_on_successive_mask_depths() {
        let parent_coordinate = Axial::new(1, 0);
        let straight_child = Axial::new(2, 0);
        let turning_child = Axial::new(1, 1);
        assert_eq!(parent_coordinate.distance(straight_child), 1);
        assert_eq!(parent_coordinate.distance(turning_child), 1);
        assert_ne!(
            straight_child - parent_coordinate,
            turning_child - parent_coordinate
        );
        assert!(wave_depth_allows_child(
            WaveNodeDepth::Outside(1),
            WaveNodeDepth::Outside(2)
        ));

        let split = weighted_branch_allocations_rotated(&[8], &[1, 1], 0).unwrap();
        let reached = split
            .allocations
            .iter()
            .map(|allocation| allocation.child_index)
            .collect::<BTreeSet<_>>();
        assert_eq!(reached, BTreeSet::from([0, 1]));
    }

    #[test]
    fn one_lane_extends_without_touching_its_parallel_neighbor() {
        let mut first_lane = vec![1, 2, 3];
        let mut second_lane = vec![10, 11];

        assert!(append_lane_layer(&mut first_lane, 3, 4));
        assert!(append_lane_layer(&mut first_lane, 4, 5));
        assert!(!append_lane_layer(&mut second_lane, 3, 4));
        assert_eq!(first_lane, vec![1, 2, 3, 4, 5]);
        assert_eq!(second_lane, vec![10, 11]);
    }

    #[test]
    fn shared_destination_lanes_retain_distinct_route_derived_directions() {
        let west_origin = 1;
        let west_boundary = 2;
        let east_origin = 3;
        let east_boundary = 4;
        let shared_target = 5;
        let later_layer = 6;
        let coordinates = BTreeMap::from([
            (west_origin, Axial::new(-2, 0)),
            (west_boundary, Axial::new(-1, 0)),
            (east_origin, Axial::new(2, 0)),
            (east_boundary, Axial::new(1, 0)),
            (shared_target, Axial::ZERO),
            (later_layer, Axial::new(1, 0)),
        ]);
        let lookup = |cell_id| {
            coordinates
                .get(&cell_id)
                .copied()
                .ok_or_else(|| "unknown test cell".to_string())
        };

        let eastward = lane_direction_from_route(
            &[west_origin, west_boundary, shared_target, later_layer],
            shared_target,
            lookup,
        )
        .unwrap();
        let westward = lane_direction_from_route(
            &[east_origin, east_boundary, shared_target],
            shared_target,
            lookup,
        )
        .unwrap();

        assert_eq!(eastward, Axial::new(1, 0));
        assert_eq!(westward, Axial::new(-1, 0));
        assert_ne!(eastward, westward);
    }

    #[test]
    fn lane_direction_requires_a_preceding_adjacent_boundary() {
        let coordinates = BTreeMap::from([
            (1, Axial::ZERO),
            (2, Axial::new(2, 0)),
            (3, Axial::new(1, 0)),
        ]);
        let lookup = |cell_id| {
            coordinates
                .get(&cell_id)
                .copied()
                .ok_or_else(|| "unknown test cell".to_string())
        };

        assert!(lane_direction_from_route(&[2], 2, lookup).is_err());
        assert!(lane_direction_from_route(&[1, 2], 2, lookup).is_err());
        assert!(lane_direction_from_route(&[1, 3], 2, lookup).is_err());
    }

    #[test]
    fn partial_frontage_keeps_the_extended_route_for_all_lane_packets() {
        let mut leading_route = vec![1, 2];
        let mut queued_remainder_route = vec![1, 2];
        assert!(append_lane_layer(&mut leading_route, 2, 3));
        assert!(append_lane_layer(&mut queued_remainder_route, 2, 3));

        let offered = 100_u64;
        let throughput = 17_u64;
        assert_eq!((offered.min(throughput), offered - throughput), (17, 83));
        assert_eq!(leading_route, vec![1, 2, 3]);
        assert_eq!(queued_remainder_route, leading_route);
    }

    #[test]
    fn only_a_capture_may_extend_a_sustained_push_lane() {
        assert!(!PacketArrival::Friendly.may_extend_sustained_push());
        assert!(PacketArrival::Capture.may_extend_sustained_push());
    }

    #[test]
    fn friendly_push_endpoint_settles_capacity_backpressure_but_not_throughput() {
        let capacity = BTreeSet::from([MovementLimit::DestinationCapacity]);
        let throughput = BTreeSet::from([MovementLimit::EdgeThroughput]);
        let both = BTreeSet::from([
            MovementLimit::DestinationCapacity,
            MovementLimit::EdgeThroughput,
        ]);

        assert!(should_settle_capacity_blocked_friendly_lane(
            OrderKind::PushFront,
            1,
            3,
            &capacity,
        ));
        assert!(should_settle_capacity_blocked_friendly_lane(
            OrderKind::PushFront,
            1,
            3,
            &both,
        ));
        assert!(!should_settle_capacity_blocked_friendly_lane(
            OrderKind::PushFront,
            1,
            3,
            &throughput,
        ));
        assert!(!should_settle_capacity_blocked_friendly_lane(
            OrderKind::PushFront,
            0,
            3,
            &capacity,
        ));
        assert!(!should_settle_capacity_blocked_friendly_lane(
            OrderKind::Balance,
            1,
            3,
            &capacity,
        ));
    }

    #[test]
    fn partial_inward_capacity_move_releases_the_exact_blocked_remainder() {
        let committed = 50_u64;
        let moved = 15_u64;
        let queued_after_move = committed - moved;
        let stopped = station_accounting(queued_after_move, queued_after_move, moved).unwrap();

        assert_eq!(stopped.stationed, 35);
        assert_eq!(stopped.packet_remaining, 0);
        assert_eq!(stopped.delivered_after, committed);
        assert_eq!(
            finalized_order_status(committed, 0, stopped.delivered_after, 0),
            Ok(OrderStatus::Completed)
        );
    }

    #[test]
    fn explicit_logistics_and_expand_capacity_backpressure_station_but_throughput_queues() {
        let capacity = BTreeSet::from([MovementLimit::DestinationCapacity]);
        let throughput = BTreeSet::from([MovementLimit::EdgeThroughput]);
        for kind in [
            OrderKind::Balance,
            OrderKind::FrontLoad,
            OrderKind::CoreLoad,
            OrderKind::PerimeterLoad,
            OrderKind::Reshape,
        ] {
            let order = test_order(kind, OrderStatus::Active);
            assert!(should_station_capacity_blocked_packet(&order, &capacity));
            assert!(!should_station_capacity_blocked_packet(&order, &throughput));
        }
        for kind in [
            OrderKind::ExpandAll,
            OrderKind::ExpandClusters,
            OrderKind::AttackClusters,
        ] {
            let order = test_order(kind, OrderStatus::Active);
            assert!(should_station_capacity_blocked_packet(&order, &capacity));
            assert!(!should_station_capacity_blocked_packet(&order, &throughput));
        }
        let push = test_order(OrderKind::PushFront, OrderStatus::Active);
        assert!(!should_station_capacity_blocked_packet(&push, &capacity));

        let committed = 50;
        let moved = 15;
        let stopped = station_accounting(committed - moved, committed - moved, moved).unwrap();
        assert_eq!(stopped.packet_remaining, 0);
        assert_eq!(stopped.delivered_after, committed);
        assert_eq!(
            finalized_order_status(committed, 0, stopped.delivered_after, 0),
            Ok(OrderStatus::Completed)
        );
    }

    #[test]
    fn persistent_policy_capacity_backpressure_waits_for_the_next_replan() {
        let capacity = BTreeSet::from([MovementLimit::DestinationCapacity]);
        let throughput = BTreeSet::from([MovementLimit::EdgeThroughput]);
        for kind in [
            OrderKind::Balance,
            OrderKind::FrontLoad,
            OrderKind::CoreLoad,
            OrderKind::PerimeterLoad,
        ] {
            let mut policy = test_order(kind, OrderStatus::Active);
            policy.client_command_id = 0;
            assert!(!should_station_capacity_blocked_packet(&policy, &capacity));
            assert!(!should_station_capacity_blocked_packet(
                &policy,
                &throughput
            ));
        }

        // Command ID zero is not enough on its own: only the strict set of
        // persistent-policy order kinds receives receding-horizon behavior.
        let mut reshape = test_order(OrderKind::Reshape, OrderStatus::Active);
        reshape.client_command_id = 0;
        assert!(should_station_capacity_blocked_packet(&reshape, &capacity));
    }

    #[test]
    fn queued_policy_funnel_converges_while_stationing_reproduces_the_pockets() {
        let (queued, saw_backpressure, passes) = run_capacity_funnel(false);
        assert!(saw_backpressure);
        assert!(passes > 1, "the regression must exercise a real pipeline");
        assert_eq!(queued.total_force(), 400);
        for (coordinate, expected) in [
            (Axial::new(-1, 0), 57),
            (Axial::new(0, -1), 57),
            (Axial::new(-1, 1), 58),
            (Axial::ZERO, 57),
            (Axial::new(1, 0), 57),
            (Axial::new(0, 1), 57),
            (Axial::new(1, -1), 57),
        ] {
            assert_eq!(queued.get(coordinate).unwrap().force(), expected);
        }

        let (stationed, saw_backpressure, _) = run_capacity_funnel(true);
        assert!(saw_backpressure);
        assert_eq!(stationed.total_force(), 400);
        assert!(
            [Axial::new(1, 0), Axial::new(0, 1), Axial::new(1, -1)]
                .into_iter()
                .any(|coordinate| stationed.get(coordinate).unwrap().force() < 57),
            "old stop-in-place behavior must leave declared policy demand undelivered"
        );
        assert!(
            [
                Axial::new(-1, 0),
                Axial::new(0, -1),
                Axial::new(-1, 1),
                Axial::ZERO,
            ]
            .into_iter()
            .any(|coordinate| stationed.get(coordinate).unwrap().force() > 58),
            "undelivered strength must remain as a high pocket"
        );
    }

    #[test]
    fn capacity_stopped_expand_conserves_strength_without_consuming_garrison_debt() {
        let committed = 50;
        let delivered_before = 15;
        let blocked_packet = committed - delivered_before;
        let stopped = station_accounting(blocked_packet, u64::MAX, delivered_before).unwrap();
        let existing_debt = 4;
        let (_, debt_after_stop, continuing) = garrison_debt_partition(existing_debt, 0);

        assert_eq!(stopped.stationed, blocked_packet);
        assert_eq!(stopped.packet_remaining, 0);
        assert_eq!(stopped.delivered_after, committed);
        assert_eq!((debt_after_stop, continuing), (existing_debt, 0));
        assert_eq!(
            finalized_order_status(committed, 0, stopped.delivered_after, 0),
            Ok(OrderStatus::Completed)
        );
    }

    #[test]
    fn captured_cell_strength_never_backs_displaced_owner_packets() {
        // Regression: the old implementation compared player one's 30
        // allocated troops with player two's newly arrived 100 infantry and
        // therefore retired nothing after capture.
        assert_eq!(packet_trim_required(2, 1, 100, 30), 30);
        assert_eq!(packet_trim_required(1, 2, u64::MAX, 7), 7);
    }

    #[test]
    fn packet_backing_is_computed_independently_for_every_owner() {
        let cell_owner = 2;
        let cell_infantry = 50;
        let allocations = [(1, 30), (2, 70), (3, 5)];
        let trims = allocations.map(|(packet_owner, allocated)| {
            packet_trim_required(cell_owner, packet_owner, cell_infantry, allocated)
        });

        assert_eq!(trims, [30, 20, 5]);
        assert_eq!(packet_trim_required(2, 2, 50, 40), 0);
    }

    #[test]
    fn sustained_push_stops_before_friendly_impassable_or_uncapturable_ground() {
        assert!(sustained_push_target_is_eligible(1, 0, true, true, true));
        assert!(sustained_push_target_is_eligible(1, 2, true, true, true));
        assert!(!sustained_push_target_is_eligible(1, 1, true, true, true));
        assert!(!sustained_push_target_is_eligible(1, 0, false, true, true));
        assert!(!sustained_push_target_is_eligible(1, 0, true, false, true));
        assert!(!sustained_push_target_is_eligible(1, 0, true, true, false));
    }

    #[test]
    fn wave_scope_keeps_attack_inside_its_snapshot_and_neutral_expand_out_of_enemy_ground() {
        let selected = [2, 3];
        let target = [7, 8];
        assert!(wave_scope_allows_cell(
            OrderKind::ExpandClusters,
            1,
            99,
            NEUTRAL_PLAYER,
            &selected,
            &[],
        ));
        assert!(wave_scope_allows_cell(
            OrderKind::ExpandClusters,
            1,
            99,
            1,
            &selected,
            &[],
        ));
        assert!(!wave_scope_allows_cell(
            OrderKind::ExpandClusters,
            1,
            99,
            2,
            &selected,
            &[],
        ));

        for owner in [NEUTRAL_PLAYER, 1, 2] {
            assert!(wave_scope_allows_cell(
                OrderKind::AttackClusters,
                1,
                7,
                owner,
                &selected,
                &target,
            ));
        }
        assert!(wave_scope_allows_cell(
            OrderKind::AttackClusters,
            1,
            2,
            1,
            &selected,
            &target,
        ));
        assert!(!wave_scope_allows_cell(
            OrderKind::AttackClusters,
            1,
            2,
            2,
            &selected,
            &target,
        ));
        assert!(!wave_scope_allows_cell(
            OrderKind::AttackClusters,
            1,
            99,
            1,
            &selected,
            &target,
        ));
    }

    #[test]
    fn internal_orders_stop_before_every_non_friendly_route_cell() {
        for kind in [
            OrderKind::Balance,
            OrderKind::FrontLoad,
            OrderKind::CoreLoad,
            OrderKind::PerimeterLoad,
            OrderKind::Reshape,
        ] {
            assert!(!internal_next_owner_is_blocked(kind, 1, 1));
            assert!(internal_next_owner_is_blocked(kind, 1, NEUTRAL_PLAYER));
            assert!(internal_next_owner_is_blocked(kind, 1, 2));
        }
    }

    #[test]
    fn recruitment_leaves_headroom_for_outstanding_internal_destinations() {
        assert_eq!(recruitment_headroom(100, 70, 25), 5);
        assert_eq!(recruitment_headroom(100, 70, 30), 0);
        assert_eq!(recruitment_headroom(100, 70, 40), 0);
        assert_eq!(recruitment_headroom(100, 100, 10), 0);
        assert_eq!(recruitment_headroom(90, 100, 10), 0);
    }

    #[test]
    fn only_active_internal_orders_reserve_recruitment_capacity() {
        for kind in [
            OrderKind::Balance,
            OrderKind::FrontLoad,
            OrderKind::CoreLoad,
            OrderKind::PerimeterLoad,
            OrderKind::Reshape,
        ] {
            let order = test_order(kind, OrderStatus::Active);
            assert!(order_reserves_recruitment_capacity(&order));
        }
        for kind in [
            OrderKind::PushFront,
            OrderKind::ExpandAll,
            OrderKind::ExpandClusters,
            OrderKind::AttackClusters,
        ] {
            let order = test_order(kind, OrderStatus::Active);
            assert!(!order_reserves_recruitment_capacity(&order));
        }
        for status in [OrderStatus::Completed, OrderStatus::Cancelled] {
            let order = test_order(OrderKind::Reshape, status);
            assert!(!order_reserves_recruitment_capacity(&order));
        }

        let reservations = BTreeMap::from([((2, 7), 30)]);
        assert_eq!(reserved_recruitment_capacity(&reservations, 1, 7), 0);
        assert_eq!(reserved_recruitment_capacity(&reservations, 2, 7), 30);
    }

    #[test]
    fn internal_destination_reservation_tracks_only_the_unreceived_remainder() {
        let mut reservations = BTreeMap::new();
        let balance = test_order(OrderKind::Balance, OrderStatus::Active);
        add_internal_destination_reservation(&mut reservations, &balance, 7, 50, 20).unwrap();
        assert_eq!(reserved_recruitment_capacity(&reservations, 1, 7), 30);

        let reshape = test_order(OrderKind::Reshape, OrderStatus::Active);
        add_internal_destination_reservation(&mut reservations, &reshape, 7, 10, 5).unwrap();
        assert_eq!(reserved_recruitment_capacity(&reservations, 1, 7), 35);

        let push = test_order(OrderKind::PushFront, OrderStatus::Active);
        add_internal_destination_reservation(&mut reservations, &push, 7, 100, 0).unwrap();
        let completed = test_order(OrderKind::CoreLoad, OrderStatus::Completed);
        add_internal_destination_reservation(&mut reservations, &completed, 7, 100, 0).unwrap();
        assert_eq!(reserved_recruitment_capacity(&reservations, 1, 7), 35);

        let mut foreign = test_order(OrderKind::PerimeterLoad, OrderStatus::Active);
        foreign.player_id = 2;
        add_internal_destination_reservation(&mut reservations, &foreign, 7, 40, 10).unwrap();
        assert_eq!(reserved_recruitment_capacity(&reservations, 1, 7), 35);
        assert_eq!(reserved_recruitment_capacity(&reservations, 2, 7), 30);

        add_internal_destination_reservation(&mut reservations, &reshape, 8, 10, 15).unwrap();
        assert_eq!(reserved_recruitment_capacity(&reservations, 1, 8), 0);
    }

    #[test]
    fn combat_capable_orders_are_not_stopped_by_the_internal_route_guard() {
        for kind in [
            OrderKind::PushFront,
            OrderKind::ExpandAll,
            OrderKind::ExpandClusters,
            OrderKind::AttackClusters,
        ] {
            assert!(!internal_next_owner_is_blocked(kind, 1, 1));
            assert!(!internal_next_owner_is_blocked(kind, 1, NEUTRAL_PLAYER));
            assert!(!internal_next_owner_is_blocked(kind, 1, 2));
        }
    }

    #[test]
    fn blocked_internal_packets_station_in_place_and_finalize_without_combat() {
        for kind in [
            OrderKind::Balance,
            OrderKind::FrontLoad,
            OrderKind::CoreLoad,
            OrderKind::PerimeterLoad,
            OrderKind::Reshape,
        ] {
            let cell_infantry_before = 55_u64;
            let packet_infantry = 30_u64;
            let delivered_before = 10_u64;
            let committed = 40_u64;

            assert!(internal_next_owner_is_blocked(kind, 1, 2));
            let station =
                station_accounting(packet_infantry, packet_infantry, delivered_before).unwrap();

            // The production transition only retires allocation metadata; the
            // backing infantry remains stationed in its current cell.
            let cell_infantry_after = cell_infantry_before;
            let in_transit_after = station.packet_remaining;
            let combat_offered_after = station.packet_remaining;
            let status =
                finalized_order_status(committed, in_transit_after, station.delivered_after, 0)
                    .unwrap();

            assert_eq!(station.stationed, 30);
            assert_eq!(cell_infantry_after, cell_infantry_before);
            assert_eq!(station.delivered_after, committed);
            assert_eq!(in_transit_after, 0);
            assert_eq!(combat_offered_after, 0);
            assert_eq!(status, OrderStatus::Completed);
        }
    }

    #[test]
    fn occupation_garrison_is_terrain_scaled_and_capacity_bounded() {
        assert_eq!(occupation_garrison(100, TerrainClass::Plains), 5);
        assert_eq!(occupation_garrison(80, TerrainClass::Hills), 8);
        assert_eq!(occupation_garrison(60, TerrainClass::Mountain), 9);
        assert_eq!(occupation_garrison(1, TerrainClass::Mountain), 1);
        assert_eq!(occupation_garrison(0, TerrainClass::Plains), 0);
        assert_eq!(occupation_garrison(100, TerrainClass::Water), 0);
    }

    #[test]
    fn garrison_and_surplus_exactly_partition_a_captured_wave() {
        for occupancy in 0..=100_u64 {
            let stationed = occupancy.min(occupation_garrison(100, TerrainClass::Plains));
            let continuing = occupancy - stationed;
            assert_eq!(stationed + continuing, occupancy);
            assert!(stationed <= 5);
        }
    }

    #[test]
    fn existing_unallocated_strength_counts_toward_a_capture_garrison() {
        assert_eq!(additional_garrison_required(10, 30, 30), 10);
        assert_eq!(additional_garrison_required(10, 30, 25), 5);
        assert_eq!(additional_garrison_required(10, 30, 20), 0);
        assert_eq!(additional_garrison_required(10, 30, 0), 0);
    }

    #[test]
    fn a_partial_expand_capture_is_topped_up_before_surplus_continues() {
        let required = occupation_garrison(100, TerrainClass::Plains);
        let first_capture = 1;
        let debt = required - first_capture;
        let (stationed, remaining_debt, continuing) = garrison_debt_partition(debt, 99);

        assert_eq!(required, 5);
        assert_eq!((stationed, remaining_debt, continuing), (4, 0, 95));
        assert_eq!(first_capture + stationed, required);
    }

    #[test]
    fn a_later_overlapping_order_can_pay_debt_after_the_capturing_order_ends() {
        let debt_after_capture = 4;
        // The capturing order has no later arrival and may complete. Because
        // debt is cell-keyed, that lifecycle does not consume or erase it.
        let (_, debt_after_first_order, _) = garrison_debt_partition(debt_after_capture, 0);
        let (stationed_by_overlap, remaining_debt, continuing) =
            garrison_debt_partition(debt_after_first_order, 99);

        assert_eq!(debt_after_first_order, 4);
        assert_eq!(
            (stationed_by_overlap, remaining_debt, continuing),
            (4, 0, 95)
        );
    }

    #[test]
    fn stale_debt_never_charges_a_different_owner() {
        assert!(expansion_debt_applies(1, 1, 1));
        assert!(!expansion_debt_applies(1, 2, 2));
        assert!(!expansion_debt_applies(1, 1, 2));
        assert!(!expansion_debt_applies(2, 1, 1));
    }

    #[test]
    fn retreat_abandonment_requires_an_unclaimed_empty_owned_cell() {
        let safe = || retreat_abandonment_is_safe(true, true, true, 0, 0, false, false);
        assert!(safe());
        assert!(!retreat_abandonment_is_safe(
            true, true, true, 0, 0, true, false
        ));
        assert!(!retreat_abandonment_is_safe(
            true, true, true, 0, 0, false, true
        ));
        assert!(!retreat_abandonment_is_safe(
            true, true, true, 1, 0, false, false
        ));
        assert!(!retreat_abandonment_is_safe(
            true, true, true, 0, 1, false, false
        ));
        assert!(!retreat_abandonment_is_safe(
            false, true, true, 0, 0, false, false
        ));
    }

    #[test]
    fn neutral_ownership_changes_can_never_name_a_winner() {
        assert_eq!(controlled_cells_for_owner(PLAYER_ONE, 80, 70), Some(80));
        assert_eq!(controlled_cells_for_owner(PLAYER_TWO, 80, 70), Some(70));
        assert_eq!(controlled_cells_for_owner(NEUTRAL_PLAYER, 80, 70), None);
    }
}
