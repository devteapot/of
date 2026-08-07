use std::{
    cmp::Reverse,
    collections::{BTreeMap, BTreeSet},
    rc::Rc,
};

use hex_core::{
    AttackFront, Axial, CombatConfig, EdgeLimits, HexMap, LogisticsConfig, MovementConfig,
    MovementIntent, MovementLimit, focus_branch_weight, movement_step, resolve_edge_combat,
    select_capture, weighted_branch_allocations_rotated,
};
use spacetimedb::{ReducerContext, Table, log_stopwatch::LogStopwatch};

use crate::rules::{
    allocated_infantry_at_cell, cell_id_for_coordinate, cell_state, config, coordinate_for_cell,
    core_cell, edge_runtime_limits, order_cell_key, state, terrain,
};
use crate::schema::{
    CellState, CombatFront, EXPANSION_AGGREGATE_ORIGIN, ExpansionGarrisonDebt,
    ExpansionSplitCursor, ExpansionWave, MatchPhase, NEUTRAL_PLAYER, OrderKind, OrderStatus,
    TerrainClass, TransferOrder, TransferSource, TransitPacket,
};
use crate::schema::{
    cell_state as cell_state_table, combat_front, expansion_garrison_debt, expansion_split_cursor,
    expansion_wave, match_state, mobilization_policy, player_state, retreat_abandonment,
    transfer_destination, transfer_order, transfer_source, transit_packet, transit_route,
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
    packet_key: u64,
    order_id: u64,
    owner_player_id: u16,
    origin_cell: u32,
    current_cell: u32,
    destination_cell: u32,
    infantry: u64,
    pending_source_infantry: u64,
    route_id: u64,
    route_index: u32,
    route: Rc<[u32]>,
    updated_step: u64,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct PacketMergeKey {
    order_id: u64,
    origin_cell: u32,
    destination_cell: u32,
    current_cell: u32,
    route_index: u32,
}

impl From<&TickPacket> for PacketMergeKey {
    fn from(packet: &TickPacket) -> Self {
        Self {
            order_id: packet.order_id,
            origin_cell: packet.origin_cell,
            destination_cell: packet.destination_cell,
            current_cell: packet.current_cell,
            route_index: packet.route_index,
        }
    }
}

impl TickPacket {
    fn from_row(packet: TransitPacket, route: Rc<[u32]>) -> Self {
        Self {
            packet_key: packet.packet_key,
            order_id: packet.order_id,
            owner_player_id: packet.owner_player_id,
            origin_cell: packet.origin_cell,
            current_cell: packet.current_cell,
            destination_cell: packet.destination_cell,
            infantry: packet.infantry,
            pending_source_infantry: packet.pending_source_infantry,
            route_id: packet.route_id,
            route_index: packet.route_index,
            route,
            updated_step: packet.updated_step,
        }
    }

    fn to_row(&self) -> TransitPacket {
        TransitPacket {
            packet_key: self.packet_key,
            order_id: self.order_id,
            owner_player_id: self.owner_player_id,
            origin_cell: self.origin_cell,
            current_cell: self.current_cell,
            destination_cell: self.destination_cell,
            infantry: self.infantry,
            pending_source_infantry: self.pending_source_infantry,
            route_id: self.route_id,
            route_index: self.route_index,
            updated_step: self.updated_step,
        }
    }
}

#[derive(Default)]
struct PacketTickState {
    rows: BTreeMap<u64, TickPacket>,
    by_order: BTreeMap<u64, BTreeSet<u64>>,
    by_cell: BTreeMap<u32, BTreeSet<u64>>,
    by_order_destination: BTreeMap<(u64, u32), BTreeSet<u64>>,
    by_merge_key: BTreeMap<PacketMergeKey, u64>,
    source_rows: BTreeMap<(u64, u32), TransferSource>,
    sources_by_order: BTreeMap<u64, Vec<u32>>,
    source_cursor_by_order: BTreeMap<u64, usize>,
    dirty_sources: BTreeSet<(u64, u32)>,
}

impl PacketTickState {
    /// Load the complete active packet set for this atomic simulation tick.
    ///
    /// Authority remains one scheduled reducer: every active packet is still
    /// processed. To reduce global table scans we only materialize:
    /// - routes referenced by those packets (direct `route_id` lookup)
    /// - transfer sources belonging to order IDs present on active packets **or**
    ///   active transfer orders (via `source_by_order`), so queued sources on
    ///   active orders with no packet yet still spawn this tick
    ///
    /// Combat fronts stay on their own shared path and are intentionally not
    /// sharded here — front resolution still sees the full contested set.
    fn load(ctx: &ReducerContext) -> Result<Self, String> {
        let mut state = Self::default();
        let packets: Vec<_> = ctx.db.transit_packet().iter().collect();
        let mut route_ids = BTreeSet::new();
        let mut order_ids = BTreeSet::new();
        for packet in &packets {
            order_ids.insert(packet.order_id);
            if packet.route_id != 0 {
                route_ids.insert(packet.route_id);
            }
        }
        for order in ctx
            .db
            .transfer_order()
            .order_by_status()
            .filter(OrderStatus::Active)
        {
            order_ids.insert(order.order_id);
        }
        let mut routes = BTreeMap::new();
        for route_id in route_ids {
            let route = ctx
                .db
                .transit_route()
                .route_id()
                .find(route_id)
                .ok_or_else(|| format!("active packet references missing route {route_id}"))?;
            routes.insert(route_id, Rc::<[u32]>::from(route.cells));
        }
        for order_id in order_ids {
            for source in ctx.db.transfer_source().source_by_order().filter(order_id) {
                state
                    .sources_by_order
                    .entry(source.order_id)
                    .or_default()
                    .push(source.cell_id);
                state
                    .source_rows
                    .insert((source.order_id, source.cell_id), source);
            }
        }
        for cells in state.sources_by_order.values_mut() {
            cells.sort_unstable();
        }
        state.source_cursor_by_order = state
            .sources_by_order
            .keys()
            .copied()
            .map(|order_id| (order_id, 0))
            .collect();
        for packet in packets {
            let route = if packet.route_id == 0 {
                if packet.current_cell == packet.destination_cell {
                    Rc::from([packet.current_cell])
                } else {
                    Rc::from([packet.current_cell, packet.destination_cell])
                }
            } else {
                routes.get(&packet.route_id).cloned().ok_or_else(|| {
                    format!(
                        "packet {} references missing route {}",
                        packet.packet_key, packet.route_id
                    )
                })?
            };
            state.track(TickPacket::from_row(packet, route));
        }
        Ok(state)
    }

    fn iter(&self) -> impl Iterator<Item = &TickPacket> {
        self.rows.values()
    }

    fn find(&self, packet_key: &u64) -> Option<TickPacket> {
        self.rows.get(packet_key).cloned()
    }

    fn find_merge(&self, merge_key: PacketMergeKey) -> Option<TickPacket> {
        self.by_merge_key
            .get(&merge_key)
            .and_then(|packet_key| self.rows.get(packet_key))
            .cloned()
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
        let inserted = ctx.db.transit_packet().insert(packet.to_row());
        let mut packet = packet;
        packet.packet_key = inserted.packet_key;
        self.track(packet);
    }

    fn update(&mut self, ctx: &ReducerContext, packet: TickPacket) {
        ctx.db.transit_packet().packet_key().update(packet.to_row());
        self.untrack(&packet.packet_key);
        self.track(packet);
    }

    fn delete(&mut self, ctx: &ReducerContext, packet_key: &u64) {
        if let Some(packet) = self.rows.get(packet_key) {
            ctx.db
                .transit_packet()
                .packet_key()
                .delete(packet.packet_key);
        }
        self.untrack(packet_key);
    }

    fn track(&mut self, packet: TickPacket) {
        let key = packet.packet_key;
        self.by_merge_key.insert((&packet).into(), key);
        self.by_order
            .entry(packet.order_id)
            .or_default()
            .insert(key);
        self.by_cell
            .entry(packet.current_cell)
            .or_default()
            .insert(key);
        self.by_order_destination
            .entry((packet.order_id, packet.destination_cell))
            .or_default()
            .insert(key);
        self.rows.insert(key, packet);
    }

    fn untrack(&mut self, packet_key: &u64) {
        let Some(packet) = self.rows.remove(packet_key) else {
            return;
        };
        self.by_merge_key.remove(&PacketMergeKey::from(&packet));
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

    fn replace_route(
        &mut self,
        ctx: &ReducerContext,
        route_id: u64,
        route: Vec<u32>,
    ) -> Result<(), String> {
        if route_id == 0 {
            return Err("inline expansion route cannot be extended".into());
        }
        let mut stored = ctx
            .db
            .transit_route()
            .route_id()
            .find(route_id)
            .ok_or_else(|| format!("missing transit route {route_id}"))?;
        stored.cells = route;
        let shared = Rc::<[u32]>::from(stored.cells.clone());
        ctx.db.transit_route().route_id().update(stored);
        for packet in self.rows.values_mut() {
            if packet.route_id == route_id {
                packet.route = shared.clone();
            }
        }
        Ok(())
    }

    fn decrement_source_queue(&mut self, packet: &TickPacket, amount: u64) -> Result<(), String> {
        if amount == 0 {
            return Ok(());
        }
        if packet.origin_cell != EXPANSION_AGGREGATE_ORIGIN {
            return self.decrement_source_row(packet.order_id, packet.origin_cell, amount);
        }

        let mut remaining = amount;
        let mut cursor = self
            .source_cursor_by_order
            .get(&packet.order_id)
            .copied()
            .unwrap_or(0);
        let source_count = self
            .sources_by_order
            .get(&packet.order_id)
            .map_or(0, Vec::len);
        while remaining > 0 && cursor < source_count {
            let cell_id = self.sources_by_order[&packet.order_id][cursor];
            let queued = self
                .source_rows
                .get(&(packet.order_id, cell_id))
                .map_or(0, |source| source.queued_infantry);
            let consumed = remaining.min(queued);
            self.decrement_source_row(packet.order_id, cell_id, consumed)?;
            remaining -= consumed;
            if consumed == queued {
                cursor += 1;
            }
        }
        self.source_cursor_by_order.insert(packet.order_id, cursor);
        if remaining > 0 {
            return Err(format!(
                "order {} source queue is short by {remaining} infantry",
                packet.order_id
            ));
        }
        Ok(())
    }

    fn decrement_source_row(
        &mut self,
        order_id: u64,
        cell_id: u32,
        amount: u64,
    ) -> Result<(), String> {
        if amount == 0 {
            return Ok(());
        }
        let source = self
            .source_rows
            .get_mut(&(order_id, cell_id))
            .ok_or_else(|| format!("transfer source {order_id}:{cell_id} is missing"))?;
        if amount > source.queued_infantry {
            return Err(format!(
                "transfer source {order_id}:{cell_id} queue underflow"
            ));
        }
        source.queued_infantry -= amount;
        self.dirty_sources.insert((order_id, cell_id));
        Ok(())
    }

    fn flush_source_queues(&mut self, ctx: &ReducerContext) {
        for key in std::mem::take(&mut self.dirty_sources) {
            if let Some(source) = self.source_rows.get(&key) {
                ctx.db.transfer_source().source_key().update(source.clone());
            }
        }
    }
}

pub fn advance_simulation(ctx: &ReducerContext) -> Result<bool, String> {
    let _tick_stopwatch = LogStopwatch::new("simulation_tick_total");
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

    let mut packets = {
        let _phase_stopwatch = LogStopwatch::new("simulation_packet_load");
        PacketTickState::load(ctx)?
    };
    {
        let _phase_stopwatch = LogStopwatch::new("simulation_trim");
        trim_all_overallocated_packets(ctx, &mut packets, logical_step)?;
    }
    {
        let _phase_stopwatch = LogStopwatch::new("simulation_branch");
        branch_expand_waves(ctx, &mut packets, logical_step)?;
        stop_blocked_expand_edges(ctx, &mut packets, logical_step)?;
    }
    {
        let _phase_stopwatch = LogStopwatch::new("simulation_move");
        move_friendly_packets(ctx, &mut packets, logical_step)?;
        stop_blocked_internal_edges(ctx, &mut packets, logical_step)?;
    }
    {
        let _phase_stopwatch = LogStopwatch::new("simulation_combat");
        resolve_combats(ctx, &mut packets, logical_step)?;
        clear_stale_combat_fronts(ctx, logical_step);
    }
    {
        let _phase_stopwatch = LogStopwatch::new("simulation_finalize");
        finalize_orders(ctx, &mut packets, logical_step)?;
        packets.flush_source_queues(ctx);
    }

    let config = config(ctx)?;
    let population_interval = u64::from(config.population_step_interval.max(1));
    let high_scale = config.player_count > crate::schema::HIGH_SCALE_PLAYER_THRESHOLD;
    // Low-scale keeps the historical cadence: full population every interval
    // steps. High-scale shards population so each cell still updates once per
    // interval while work stays bounded.
    let run_population = if high_scale {
        true
    } else {
        logical_step.is_multiple_of(population_interval)
    };
    if run_population {
        let _phase_stopwatch = LogStopwatch::new("simulation_population");
        population_step(ctx, logical_step, high_scale)?;
    }
    if logical_step.is_multiple_of(ORDER_PRUNE_INTERVAL_STEPS) {
        let _phase_stopwatch = LogStopwatch::new("simulation_prune");
        prune_order_history(ctx, logical_step);
    }
    if logical_step.is_multiple_of(40) {
        log::info!(
            target: "of",
            "event=sim.heartbeat step={logical_step} packets={} high_scale={}",
            packets.rows.len(),
            u8::from(high_scale)
        );
    }
    Ok(state(ctx)?.phase == MatchPhase::Running)
}

/// How long terminal Completed/Cancelled orders (and their source and
/// destination rows) remain visible for client feedback: 2,400 steps is ten
/// minutes at the default 250 ms step. Quarantined orders are exempt — they
/// are the operator-visible record of an invariant violation and are rare by
/// construction.
const ORDER_RETENTION_STEPS: u64 = 2_400;
const ORDER_PRUNE_INTERVAL_STEPS: u64 = 40;

fn prune_order_history(ctx: &ReducerContext, logical_step: u64) {
    for status in [OrderStatus::Completed, OrderStatus::Cancelled] {
        let stale: Vec<u64> = ctx
            .db
            .transfer_order()
            .order_by_status()
            .filter(status)
            .filter(|order| order_history_is_prunable(order.updated_step, logical_step))
            .map(|order| order.order_id)
            .collect();
        for order_id in stale {
            let source_keys: Vec<_> = ctx
                .db
                .transfer_source()
                .source_by_order()
                .filter(order_id)
                .map(|source| source.source_key)
                .collect();
            for key in source_keys {
                ctx.db.transfer_source().source_key().delete(key);
            }
            let destination_keys: Vec<_> = ctx
                .db
                .transfer_destination()
                .destination_by_order()
                .filter(order_id)
                .map(|destination| destination.destination_key)
                .collect();
            for key in destination_keys {
                ctx.db.transfer_destination().destination_key().delete(key);
            }
            ctx.db.transfer_order().order_id().delete(order_id);
        }
    }
}

fn order_history_is_prunable(updated_step: u64, logical_step: u64) -> bool {
    updated_step.saturating_add(ORDER_RETENTION_STEPS) < logical_step
}

fn is_expansion_wave_order(kind: OrderKind) -> bool {
    matches!(
        kind,
        OrderKind::ExpandAll | OrderKind::ExpandClusters | OrderKind::AttackClusters
    )
}

/// Permanently parks an order after an attributable invariant violation so a
/// deterministic per-order failure cannot re-fail every scheduled tick and
/// freeze the match.
///
/// Strength is conserved by construction: infantry always lives in
/// `CellState` rows, and packets/sources are allocation metadata only.
/// Deleting the order's packets releases its strength at the current physical
/// cells; zeroing the source queues releases the not-yet-departed remainder
/// at its origins. The order row is kept with `OrderStatus::Quarantined` (its
/// last-known counters frozen) as the operator/player-visible record, and the
/// failure is logged loudly.
fn quarantine_order(
    ctx: &ReducerContext,
    packets: &mut PacketTickState,
    order_id: u64,
    reason: &str,
    logical_step: u64,
) {
    log::error!(
        target: "of",
        "event=order.quarantine order_id={order_id} step={logical_step} reason={reason}"
    );
    let packet_keys: Vec<u64> = packets
        .by_order(order_id)
        .map(|packet| packet.packet_key)
        .collect();
    for packet_key in packet_keys {
        packets.delete(ctx, &packet_key);
    }
    let source_cells = packets
        .sources_by_order
        .get(&order_id)
        .cloned()
        .unwrap_or_default();
    for cell_id in source_cells {
        if let Some(source) = packets.source_rows.get_mut(&(order_id, cell_id))
            && source.queued_infantry != 0
        {
            source.queued_infantry = 0;
            packets.dirty_sources.insert((order_id, cell_id));
        }
    }
    if let Some(mut order) = ctx.db.transfer_order().order_id().find(order_id) {
        order.status = OrderStatus::Quarantined;
        order.in_transit_infantry = 0;
        order.updated_step = logical_step;
        ctx.db.transfer_order().order_id().update(order);
    }
    ctx.db.expansion_wave().order_id().delete(order_id);
    clear_expansion_split_cursors(ctx, order_id);
    let route_ids: Vec<_> = ctx
        .db
        .transit_route()
        .route_by_order()
        .filter(order_id)
        .map(|route| route.route_id)
        .collect();
    for route_id in route_ids {
        ctx.db.transit_route().route_id().delete(route_id);
    }
    let abandonment_keys: Vec<_> = ctx
        .db
        .retreat_abandonment()
        .abandonment_by_order()
        .filter(order_id)
        .map(|abandonment| abandonment.abandonment_key)
        .collect();
    for key in abandonment_keys {
        ctx.db.retreat_abandonment().abandonment_key().delete(key);
    }
}

fn expansion_split_cursor_value(ctx: &ReducerContext, order_id: u64, cell_id: u32) -> u8 {
    ctx.db
        .expansion_split_cursor()
        .cursor_key()
        .find(order_cell_key(order_id, cell_id))
        .map_or(0, |row| row.cursor)
}

fn set_expansion_split_cursor(ctx: &ReducerContext, order_id: u64, cell_id: u32, cursor: u8) {
    let cursor_key = order_cell_key(order_id, cell_id);
    if let Some(mut row) = ctx
        .db
        .expansion_split_cursor()
        .cursor_key()
        .find(cursor_key)
    {
        row.cursor = cursor;
        ctx.db.expansion_split_cursor().cursor_key().update(row);
    } else {
        ctx.db
            .expansion_split_cursor()
            .insert(ExpansionSplitCursor {
                cursor_key,
                order_id,
                cell_id,
                cursor,
            });
    }
}

pub(crate) fn clear_expansion_split_cursors(ctx: &ReducerContext, order_id: u64) {
    let keys: Vec<_> = ctx
        .db
        .expansion_split_cursor()
        .cursor_by_order()
        .filter(order_id)
        .map(|row| row.cursor_key)
        .collect();
    for key in keys {
        ctx.db.expansion_split_cursor().cursor_key().delete(key);
    }
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
        match blocked_expand_packets_for_order(ctx, packets, &order) {
            Ok(mut blocked) => blocked_packets.append(&mut blocked),
            Err(error) => quarantine_order(ctx, packets, order.order_id, &error, logical_step),
        }
    }
    blocked_packets.sort_unstable_by_key(|packet| packet.packet_key);
    for packet in blocked_packets {
        if let Err(error) =
            station_packet_allocation(ctx, packets, &packet, packet.infantry, logical_step)
        {
            quarantine_order(ctx, packets, packet.order_id, &error, logical_step);
        }
    }
    Ok(())
}

fn blocked_expand_packets_for_order(
    ctx: &ReducerContext,
    packets: &PacketTickState,
    order: &TransferOrder,
) -> Result<Vec<TickPacket>, String> {
    let wave = ctx
        .db
        .expansion_wave()
        .order_id()
        .find(order.order_id)
        .ok_or_else(|| format!("wave order {} has no topology", order.order_id))?;
    let mut blocked = Vec::new();
    for packet in packets.by_order(order.order_id) {
        let next_index = packet.route_index as usize + 1;
        let Some(&next_cell) = packet.route.get(next_index) else {
            continue;
        };
        if !expansion_edge_is_available(ctx, order, &wave, packet.current_cell, next_cell)? {
            blocked.push(packet.clone());
        }
    }
    Ok(blocked)
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
    let mut broken_orders = Vec::new();
    'orders: for (order_id, player_id, kind) in internal_orders {
        for packet in packets.by_order(order_id) {
            let Some(&next_cell) = packet.route.get(packet.route_index as usize + 1) else {
                continue;
            };
            let next_owner = match cell_state(ctx, next_cell) {
                Ok(next) => next.owner_player_id,
                Err(error) => {
                    broken_orders.push((order_id, error));
                    continue 'orders;
                }
            };
            if internal_next_owner_is_blocked(kind, player_id, next_owner) {
                blocked_packets.push(packet.clone());
            }
        }
    }
    for (order_id, error) in broken_orders {
        quarantine_order(ctx, packets, order_id, &error, logical_step);
    }
    blocked_packets.sort_unstable_by_key(|packet| packet.packet_key);
    for packet in blocked_packets {
        if let Err(error) =
            station_packet_allocation(ctx, packets, &packet, packet.infantry, logical_step)
        {
            quarantine_order(ctx, packets, packet.order_id, &error, logical_step);
        }
    }
    Ok(())
}

fn internal_order_requires_friendly_route(kind: OrderKind) -> bool {
    matches!(kind, OrderKind::Reshape | OrderKind::FrontRebalance)
}

fn internal_next_owner_is_blocked(kind: OrderKind, player_id: u16, next_owner: u16) -> bool {
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
        if let Err(error) = branch_expand_wave_order(ctx, packets, &order, logical_step) {
            quarantine_order(ctx, packets, order.order_id, &error, logical_step);
        }
    }
    Ok(())
}

fn branch_expand_wave_order(
    ctx: &ReducerContext,
    packets: &mut PacketTickState,
    order: &TransferOrder,
    logical_step: u64,
) -> Result<(), String> {
    let wave = ctx
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
    for (cell_id, mut contributions) in resting_by_cell {
        contributions.sort_unstable_by_key(|packet| packet.packet_key);
        branch_expand_node(
            ctx,
            packets,
            order,
            &wave,
            cell_id,
            &contributions,
            logical_step,
        )?;
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
    wave: &ExpansionWave,
    cell_id: u32,
    contributions: &[TickPacket],
    logical_step: u64,
) -> Result<(), String> {
    let contributions =
        pay_expansion_garrison_debt(ctx, packets, order, cell_id, contributions, logical_step)?;
    if contributions.is_empty() {
        return Ok(());
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
        return Ok(());
    }

    let amounts = contributions
        .iter()
        .map(|packet| packet.infantry)
        .collect::<Vec<_>>();
    let cursor = expansion_split_cursor_value(ctx, order.order_id, cell_id);
    let child_weights = expansion_child_weights(ctx, wave, cell_id, &children)?;
    let weighted =
        weighted_branch_allocations_rotated(&amounts, &child_weights, usize::from(cursor))
            .map_err(|error| format!("invalid expansion branch allocation: {error:?}"))?;
    let allocations = weighted.allocations;
    let next_cursor = weighted.next_cursor;
    let next_cursor =
        u8::try_from(next_cursor).map_err(|_| "expand child cursor exceeds u8".to_string())?;
    if next_cursor != cursor {
        set_expansion_split_cursor(ctx, order.order_id, cell_id, next_cursor);
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
        let contribution = &contributions[contribution_index];
        let pending_source_infantry = match contribution.pending_source_infantry {
            0 => 0,
            pending if pending == contribution.infantry => amount,
            _ => return Err("expand resting packet has partial source accounting".into()),
        };
        insert_expand_edge_packet(
            ctx,
            packets,
            order,
            cell_id,
            child,
            amount,
            pending_source_infantry,
            logical_step,
        )?;
    }
    Ok(())
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

fn expansion_debt_applies(debt_owner: u16, cell_owner: u16, order_owner: u16) -> bool {
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
    Source,
    Outside(u16),
}

fn wave_node_depth(wave: &ExpansionWave, cell_id: u32) -> Option<WaveNodeDepth> {
    if wave.selected_cells.binary_search(&cell_id).is_ok() {
        return Some(WaveNodeDepth::Source);
    }
    wave.outside_depths
        .get(cell_id as usize)
        .copied()
        .filter(|depth| *depth != u16::MAX)
        .map(WaveNodeDepth::Outside)
}

fn wave_depth_allows_child(parent: WaveNodeDepth, child: WaveNodeDepth) -> bool {
    match (parent, child) {
        (WaveNodeDepth::Source, WaveNodeDepth::Outside(1)) => true,
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
    player_id: u16,
    cell_id: u32,
    owner_player_id: u16,
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
    from_cell: u32,
    to_cell: u32,
    amount: u64,
    pending_source_infantry: u64,
    logical_step: u64,
) -> Result<(), String> {
    if amount == 0 {
        return Ok(());
    }
    let merge_key = PacketMergeKey {
        order_id: order.order_id,
        origin_cell: EXPANSION_AGGREGATE_ORIGIN,
        destination_cell: to_cell,
        current_cell: from_cell,
        route_index: 0,
    };
    if let Some(mut existing) = packets.find_merge(merge_key) {
        existing.infantry = merged_expand_strength(existing.infantry, amount)?;
        existing.pending_source_infantry = existing
            .pending_source_infantry
            .checked_add(pending_source_infantry)
            .ok_or_else(|| "expand pending-source strength overflow".to_string())?;
        existing.updated_step = logical_step;
        packets.update(ctx, existing);
    } else {
        packets.insert(
            ctx,
            TickPacket {
                packet_key: 0,
                order_id: order.order_id,
                owner_player_id: order.player_id,
                origin_cell: EXPANSION_AGGREGATE_ORIGIN,
                current_cell: from_cell,
                destination_cell: to_cell,
                infantry: amount,
                pending_source_infantry,
                route_id: 0,
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

fn population_step(
    ctx: &ReducerContext,
    logical_step: u64,
    high_scale: bool,
) -> Result<(), String> {
    let config = config(ctx)?;
    let destination_reservations = active_internal_destination_reservations(ctx)?;
    let retreating_edge_cells = active_retreat_abandonment_cells(ctx);
    let policies: BTreeMap<_, _> = ctx
        .db
        .mobilization_policy()
        .iter()
        .map(|policy| (policy.player_id, policy.target_bps))
        .collect();
    let interval = config.population_step_interval.max(1);
    if interval > u32::from(u16::MAX) {
        return Err(format!(
            "population_step_interval {interval} exceeds u16 shard storage"
        ));
    }
    let active_shard = u16::try_from(logical_step % u64::from(interval))
        .map_err(|_| "active population shard overflow".to_owned())?;
    let cells: Vec<_> = if high_scale {
        ctx.db
            .cell_state()
            .state_by_population_shard()
            .filter(active_shard)
            .collect()
    } else {
        ctx.db.cell_state().iter().collect()
    };
    for mut cell in cells {
        if cell.owner_player_id == NEUTRAL_PLAYER {
            continue;
        }
        let Some(&target_bps) = policies.get(&cell.owner_player_id) else {
            continue;
        };
        if cell.civilian_capacity == 0 {
            continue;
        }
        let reserved_capacity = reserved_recruitment_capacity(
            &destination_reservations,
            cell.owner_player_id,
            cell.cell_id,
        );
        let next = population_cell_transition(
            cell.civilians,
            cell.civilian_capacity,
            cell.infantry,
            cell.military_capacity,
            config.civilian_growth_bps,
            target_bps,
            config.mobilization_per_population_step,
            reserved_capacity,
            retreating_edge_cells.contains(&cell.cell_id),
        );
        if next.civilians != cell.civilians || next.infantry != cell.infantry {
            cell.civilians = next.civilians;
            cell.infantry = next.infantry;
            cell.last_changed_step = logical_step;
            ctx.db.cell_state().cell_id().update(cell);
        }
    }
    Ok(())
}

/// One owned cell's population transition for one population interval.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PopulationTransition {
    civilians: u64,
    infantry: u64,
}

/// Pure population formula: civilians regrow toward capacity (basis-point
/// share of the missing amount, minimum one), then mobilization converts
/// civilians to infantry toward the target share, bounded by the per-step
/// mobilization budget, available civilians, and unreserved military
/// capacity headroom. Lowering the target never demobilizes. The transition
/// conserves total population except for the explicit regrowth amount.
#[allow(clippy::too_many_arguments)]
fn population_cell_transition(
    civilians: u64,
    civilian_capacity: u64,
    infantry: u64,
    military_capacity: u64,
    growth_bps: u32,
    target_bps: u32,
    mobilization_per_step: u64,
    reserved_capacity: u64,
    retreating: bool,
) -> PopulationTransition {
    let mut civilians = civilians;
    let mut infantry = infantry;
    let missing = civilian_capacity.saturating_sub(civilians);
    if missing > 0 {
        let growth = ((u128::from(missing) * u128::from(growth_bps)) / 10_000) as u64;
        civilians = civilians.saturating_add(growth.max(1).min(missing));
    }

    let local_population = civilians.saturating_add(infantry);
    let desired_infantry =
        ((u128::from(local_population) * u128::from(target_bps)) / 10_000) as u64;
    if infantry < desired_infantry && !retreating {
        let recruit = desired_infantry
            .saturating_sub(infantry)
            .min(mobilization_per_step)
            .min(civilians)
            .min(recruitment_headroom(
                military_capacity,
                infantry,
                reserved_capacity,
            ));
        civilians -= recruit;
        infantry += recruit;
    }
    PopulationTransition {
        civilians,
        infantry,
    }
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
) -> Result<BTreeMap<(u16, u32), u64>, String> {
    let mut reservations = BTreeMap::<(u16, u32), u64>::new();
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
    reservations: &mut BTreeMap<(u16, u32), u64>,
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
    reservations: &BTreeMap<(u16, u32), u64>,
    owner_player_id: u16,
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
    // Explicit one-shot logistics and bounded expansion waves use best-effort
    // stop-in-place when destination capacity is exhausted.
    (internal_order_requires_friendly_route(order.kind) || is_expansion_wave_order(order.kind))
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
            capacity_stopped_packets.insert(intent.packet.packet_key, intent.packet.clone());
        }
    }

    for cell_id in participating {
        let coordinate = coordinate_for_cell(ctx, cell_id)?;
        let infantry = map
            .get(coordinate)
            .ok_or_else(|| "movement result omitted a participating cell".to_string())?
            .force();
        let mut row = cell_state(ctx, cell_id)?;
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
        if let Err(error) = settle_stopped_sustained_lane(
            ctx,
            packets,
            order_id,
            lane_anchor,
            direction,
            logical_step,
        ) {
            quarantine_order(ctx, packets, order_id, &error, logical_step);
        }
    }
    for packet in capacity_stopped_packets.into_values() {
        // An upstream packet can merge into this key during the same pipeline
        // step. Retire the complete post-movement allocation at the blocked
        // choke, not only the amount present in the pre-step snapshot.
        if let Err(error) = station_packet_allocation(ctx, packets, &packet, u64::MAX, logical_step)
        {
            quarantine_order(ctx, packets, packet.order_id, &error, logical_step);
        }
    }
    Ok(())
}

#[derive(Clone)]
struct FrontPackets {
    attacker: u16,
    from_cell: u32,
    to_cell: u32,
    packets: Vec<TickPacket>,
}

fn resolve_combats(
    ctx: &ReducerContext,
    packets: &mut PacketTickState,
    logical_step: u64,
) -> Result<(), String> {
    let mut targets = BTreeMap::<u32, BTreeMap<u16, BTreeMap<u32, Vec<TickPacket>>>>::new();
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
        // Every hostile owner engages simultaneously. The kernel allocates the
        // defenders proportionally across all valid fronts and applies the
        // documented multi-attacker capture rule; an owner who captured this
        // cell earlier in the same pass is filtered out by the refresh above
        // or by the defender-ownership check here.
        let mut fronts = Vec::new();
        for (&attacker, front_map) in &attackers {
            if attacker == defender.owner_player_id {
                continue;
            }
            for (&from_cell, front_packets) in front_map {
                fronts.push(FrontPackets {
                    attacker,
                    from_cell,
                    to_cell: target_cell,
                    packets: front_packets.clone(),
                });
            }
        }
        if fronts.is_empty() {
            continue;
        }
        // A cell has exactly one owner, so `from_cell` is unique across owners
        // and doubles as the deterministic kernel attack ID.
        fronts.sort_unstable_by_key(|front| front.from_cell);
        if let Err(error) = resolve_target_combat(ctx, packets, defender, &fronts, logical_step) {
            // The failure is attributable to this contested cell: quarantine
            // every order that contributed a front so the remaining targets
            // (and future ticks) keep resolving.
            let order_ids: BTreeSet<u64> = fronts
                .iter()
                .flat_map(|front| front.packets.iter().map(|packet| packet.order_id))
                .collect();
            for order_id in order_ids {
                quarantine_order(ctx, packets, order_id, &error, logical_step);
            }
        }
    }
    Ok(())
}

fn refresh_target_attackers(
    ctx: &ReducerContext,
    packet_state: &PacketTickState,
    target_cell: u32,
    candidates: BTreeMap<u16, BTreeMap<u32, Vec<TickPacket>>>,
) -> Result<BTreeMap<u16, BTreeMap<u32, Vec<TickPacket>>>, String> {
    let mut current = BTreeMap::<u16, BTreeMap<u32, Vec<TickPacket>>>::new();
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

/// Resolves one contested cell against every hostile front simultaneously.
///
/// Casualty allocation and the capture rule are owned by the kernel
/// ([`resolve_edge_combat`]): defenders split proportionally over all valid
/// fronts regardless of owner, and when the defender is eliminated the owner
/// with the largest surviving committed strength captures (ties break toward
/// the smaller owner ID, then the smaller origin cell ID). The module applies
/// its minimum-one-casualty adjustment before re-running the capture
/// selection over the adjusted survivors, so displayed numbers and the
/// capture pick always agree.
///
/// Fronts rejected by the kernel (broken geometry) quarantine their
/// contributing orders while the remaining valid fronts still resolve.
fn resolve_target_combat(
    ctx: &ReducerContext,
    packets: &mut PacketTickState,
    mut defender: CellState,
    fronts: &[FrontPackets],
    logical_step: u64,
) -> Result<(), String> {
    let config = config(ctx)?;
    let target_coordinate = coordinate_for_cell(ctx, defender.cell_id)?;
    let target_terrain = terrain(ctx, defender.cell_id)?;
    let mut attacks = Vec::new();
    for front in fronts {
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

    // A rejected front means this order's persisted geometry violates the
    // combat contract (non-adjacent origin, cliff, duplicated origin). That is
    // attributable: park those orders and let the valid fronts resolve.
    if !resolution.rejected.is_empty() {
        let rejected_ids: BTreeMap<u64, _> = resolution
            .rejected
            .iter()
            .map(|rejection| (rejection.id, rejection.reason))
            .collect();
        let mut rejected_orders = BTreeMap::new();
        for front in fronts {
            if let Some(reason) = rejected_ids.get(&u64::from(front.from_cell)) {
                for packet in &front.packets {
                    rejected_orders.insert(
                        packet.order_id,
                        format!(
                            "combat front {}->{} rejected: {reason:?}",
                            front.from_cell, front.to_cell
                        ),
                    );
                }
            }
        }
        for (order_id, reason) in rejected_orders {
            quarantine_order(ctx, packets, order_id, &reason, logical_step);
        }
    }
    let valid_fronts: Vec<&FrontPackets> = fronts
        .iter()
        .filter(|front| resolution.attacks.contains_key(&u64::from(front.from_cell)))
        .collect();
    if valid_fronts.is_empty() {
        return Ok(());
    }

    let total_engaged: u64 = resolution
        .attacks
        .values()
        .map(|outcome| outcome.engaged)
        .sum();
    let defender_casualties = minimum_casualty(
        total_engaged > 0,
        resolution.defender_casualties,
        defender.infantry,
    );
    let extra_defender_casualty =
        defender_casualties.saturating_sub(resolution.defender_casualties);

    let mut adjusted_outcomes = resolution.attacks.clone();
    for front in &valid_fronts {
        let outcome = resolution
            .attacks
            .get(&u64::from(front.from_cell))
            .ok_or_else(|| "combat omitted an attack front".to_string())?;
        let attacker_casualties = minimum_casualty(
            outcome.engaged > 0 && outcome.defense_allocated > 0,
            outcome.attacker_casualties,
            outcome.offered,
        );
        apply_attacker_casualties(
            ctx,
            packets,
            &front.packets,
            attacker_casualties,
            logical_step,
        )?;
        if let Some(adjusted) = adjusted_outcomes.get_mut(&u64::from(front.from_cell)) {
            adjusted.attacker_remaining = outcome.offered.saturating_sub(attacker_casualties);
        }
        let limits = edge_runtime_limits(ctx, front.from_cell, front.to_cell)?
            .ok_or_else(|| "combat route became impassable".to_string())?;
        let front_defender_casualties = outcome.defender_casualties
            + u64::from(
                extra_defender_casualty > 0 && front.from_cell == valid_fronts[0].from_cell,
            );
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
    ctx.db.cell_state().cell_id().update(defender.clone());
    trim_packets_at_cell(
        ctx,
        packets,
        defender.cell_id,
        defender.owner_player_id,
        logical_step,
    )?;

    if defender.infantry == 0 {
        let (capturing_owner, capturing_front) = select_capture(&adjusted_outcomes);
        if let (Some(owner), Some(front_id)) = (capturing_owner, capturing_front) {
            let front = valid_fronts
                .iter()
                .find(|front| u64::from(front.from_cell) == front_id)
                .ok_or_else(|| "capturing front is missing".to_string())?;
            if u32::from(front.attacker) != owner {
                return Err("capture selection disagrees with its front owner".into());
            }
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
    sorted.sort_unstable_by_key(|packet| packet.packet_key);
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
    packets.sort_unstable_by_key(|packet| packet.packet_key);
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
    owner_player_id: u16,
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

/// Pure capture bookkeeping: the counter updates and the victory decision for
/// one ownership change, independent of any database state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CaptureAccounting {
    /// Loser's counter after the change; `None` when the loser is neutral.
    old_controlled: Option<u64>,
    /// Winner's counter after the change; `None` when the winner is neutral.
    new_controlled: Option<u64>,
    /// True exactly when a non-neutral winner reaches `required_control`.
    victory: bool,
}

fn capture_accounting(
    old_owner: u16,
    new_owner: u16,
    old_controlled: u64,
    new_controlled: u64,
    required_control: u64,
) -> Result<CaptureAccounting, String> {
    let old_after = if old_owner == NEUTRAL_PLAYER {
        None
    } else {
        Some(controlled_after_loss(old_controlled)?)
    };
    let new_after = if new_owner == NEUTRAL_PLAYER {
        None
    } else {
        Some(
            new_controlled
                .checked_add(1)
                .ok_or_else(|| "controlled-cell count overflow".to_string())?,
        )
    };
    Ok(CaptureAccounting {
        old_controlled: old_after,
        new_controlled: new_after,
        victory: new_after.is_some_and(|controlled| controlled >= required_control),
    })
}

fn record_capture(
    ctx: &ReducerContext,
    cell_id: u32,
    old_owner: u16,
    new_owner: u16,
) -> Result<(), String> {
    let player_count = config(ctx)?.player_count;
    if !valid_owner(old_owner, player_count) || !valid_owner(new_owner, player_count) {
        return Err("capture named a player outside the configured range".into());
    }
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
    let old_controlled = if old_owner == NEUTRAL_PLAYER {
        0
    } else {
        ctx.db
            .player_state()
            .player_id()
            .find(old_owner)
            .ok_or("captured player's state is missing")?
            .controlled_cells
    };
    let new_controlled = if new_owner == NEUTRAL_PLAYER {
        0
    } else {
        ctx.db
            .player_state()
            .player_id()
            .find(new_owner)
            .ok_or("capturing player's state is missing")?
            .controlled_cells
    };
    let accounting = capture_accounting(
        old_owner,
        new_owner,
        old_controlled,
        new_controlled,
        match_state.required_control,
    )?;
    if let Some(controlled) = accounting.old_controlled {
        let mut old_state = ctx
            .db
            .player_state()
            .player_id()
            .find(old_owner)
            .ok_or("captured player's state is missing")?;
        old_state.controlled_cells = controlled;
        ctx.db.player_state().player_id().update(old_state);
    }
    if let Some(controlled) = accounting.new_controlled {
        let mut new_state = ctx
            .db
            .player_state()
            .player_id()
            .find(new_owner)
            .ok_or("capturing player's state is missing")?;
        new_state.controlled_cells = controlled;
        ctx.db.player_state().player_id().update(new_state);
    }
    if accounting.victory {
        match_state.phase = MatchPhase::Completed;
        match_state.winner_player_id = new_owner;
        match_state.completed_at_us = crate::timestamp_us(ctx);
    }
    ctx.db.match_state().singleton_id().update(match_state);

    // Guard against the incremental counter drifting from real ownership.
    // Callers update `CellState.owner_player_id` before recording the
    // capture, and players can only own capturable cells, so an indexed
    // recount of the winner's rows must equal the incremental counter.
    #[cfg(debug_assertions)]
    if let Some(controlled) = accounting.new_controlled {
        let recounted = ctx
            .db
            .cell_state()
            .state_by_owner()
            .filter(new_owner)
            .count() as u64;
        debug_assert_eq!(
            recounted, controlled,
            "controlled_cells counter for player {new_owner} drifted from ownership recount"
        );
    }
    Ok(())
}

fn controlled_after_loss(controlled_cells: u64) -> Result<u64, String> {
    controlled_cells
        .checked_sub(1)
        .ok_or_else(|| "controlled-cell count underflow".to_owned())
}

/// Pure victory glue: when capture accounting reports victory, the match
/// completes with `new_owner` as winner; otherwise the phase is unchanged.
#[cfg(test)]
fn match_state_after_capture(
    phase: MatchPhase,
    winner_player_id: u16,
    new_owner: u16,
    victory: bool,
) -> (MatchPhase, u16) {
    if victory {
        (MatchPhase::Completed, new_owner)
    } else {
        (phase, winner_player_id)
    }
}

const fn valid_owner(owner: u16, player_count: u16) -> bool {
    owner == NEUTRAL_PLAYER || (owner >= 1 && owner <= player_count)
}

/// Module min-casualty adjustment applied after the kernel resolution so a
/// contested edge always shows at least one casualty on each engaged side.
fn minimum_casualty(engaged: bool, casualties: u64, available: u64) -> u64 {
    let extra = u64::from(engaged && casualties == 0 && available > 0);
    casualties.saturating_add(extra).min(available)
}

/// Orders whose fronts the kernel rejected are attributable failures: park
/// them while sibling valid fronts continue to resolve.
#[cfg(test)]
fn orders_for_rejected_fronts(
    fronts: &[(u32, Vec<u64>)],
    rejected_front_ids: &BTreeSet<u64>,
) -> BTreeSet<u64> {
    let mut orders = BTreeSet::new();
    for &(from_cell, ref packet_orders) in fronts {
        if rejected_front_ids.contains(&u64::from(from_cell)) {
            orders.extend(packet_orders.iter().copied());
        }
    }
    orders
}

/// Active orders participate in the tick; quarantined rows are parked and
/// never re-enter movement/combat/finalize until an operator intervenes.
#[cfg(test)]
const fn order_participates_in_tick(status: OrderStatus) -> bool {
    matches!(status, OrderStatus::Active)
}

/// Quarantined orders are exempt from the Completed/Cancelled retention prune
/// so the operator-visible failure record survives.
#[cfg(test)]
const fn order_status_is_prunable_history(status: OrderStatus) -> bool {
    matches!(status, OrderStatus::Completed | OrderStatus::Cancelled)
}

/// Named tick phases in the order `advance_simulation` executes them. Mid-tick
/// quarantine of one order must not skip later phases for the remaining set.
#[cfg(test)]
const TICK_PHASES: &[&str] = &[
    "packet_load",
    "trim",
    "branch",
    "move",
    "combat",
    "finalize",
    "population",
    "prune",
];

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
        extend_sustained_lane(ctx, packets, &packet, next_cell)?;
        packet = packets
            .find(&packet.packet_key)
            .ok_or_else(|| "push packet disappeared while extending its lane".to_string())?;
    }

    let source_debit = moved.min(packet.pending_source_infantry);
    if moved == packet.infantry {
        packets.delete(ctx, &packet.packet_key);
    } else {
        let mut remainder = packet.clone();
        remainder.infantry -= moved;
        remainder.pending_source_infantry -= source_debit;
        remainder.updated_step = logical_step;
        packets.update(ctx, remainder);
    }
    packets.decrement_source_queue(&packet, source_debit)?;

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

    let child_merge_key = PacketMergeKey {
        order_id: packet.order_id,
        origin_cell: packet.origin_cell,
        destination_cell: packet.destination_cell,
        current_cell: next_cell,
        route_index: next_index,
    };
    if let Some(mut existing) = packets.find_merge(child_merge_key) {
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
                packet_key: 0,
                order_id: packet.order_id,
                owner_player_id: packet.owner_player_id,
                origin_cell: packet.origin_cell,
                current_cell: next_cell,
                destination_cell: packet.destination_cell,
                infantry: moved,
                pending_source_infantry: 0,
                route_id: packet.route_id,
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
    let source_debit = moved.min(packet.pending_source_infantry);
    if moved == packet.infantry {
        packets.delete(ctx, &packet.packet_key);
    } else {
        let mut remainder = packet.clone();
        remainder.infantry -= moved;
        remainder.pending_source_infantry -= source_debit;
        remainder.updated_step = logical_step;
        packets.update(ctx, remainder);
    }
    packets.decrement_source_queue(packet, source_debit)?;

    let rest_merge_key = PacketMergeKey {
        order_id: packet.order_id,
        origin_cell: EXPANSION_AGGREGATE_ORIGIN,
        destination_cell: next_cell,
        current_cell: next_cell,
        route_index: 0,
    };
    if let Some(mut resting) = packets.find_merge(rest_merge_key) {
        resting.infantry = merged_expand_strength(resting.infantry, moved)?;
        resting.updated_step = logical_step;
        packets.update(ctx, resting);
    } else {
        packets.insert(
            ctx,
            TickPacket {
                packet_key: 0,
                order_id: packet.order_id,
                owner_player_id: packet.owner_player_id,
                origin_cell: EXPANSION_AGGREGATE_ORIGIN,
                current_cell: next_cell,
                destination_cell: next_cell,
                infantry: moved,
                pending_source_infantry: 0,
                route_id: 0,
                route_index: 0,
                route: Rc::from([next_cell]),
                updated_step: logical_step,
            },
        );
    }
    Ok(())
}

#[cfg(test)]
fn packet_has_pending_source(packet: &TickPacket) -> bool {
    packet.pending_source_infantry > 0
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
    let mut extended_routes = BTreeSet::new();
    for candidate in candidates {
        if push_lane_direction(ctx, &order, &candidate)? != direction {
            continue;
        }
        if !extended_routes.insert(candidate.route_id) {
            continue;
        }
        let mut route = candidate.route.to_vec();
        if !append_lane_layer(&mut route, reached_cell, next_cell) {
            continue;
        }
        packets.replace_route(ctx, candidate.route_id, route)?;
        extended = true;
    }
    Ok(extended)
}

fn sustained_push_target_is_eligible(
    player_id: u16,
    target_owner: u16,
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

fn increment_destination_received(
    ctx: &ReducerContext,
    order_id: u64,
    destination_cell: u32,
    amount: u64,
) -> Result<(), String> {
    let key = order_cell_key(order_id, destination_cell);
    let mut destination = ctx
        .db
        .transfer_destination()
        .destination_key()
        .find(key)
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
    owner_player_id: u16,
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
    candidates.sort_unstable_by_key(|packet| packet.packet_key);
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
    candidates.sort_unstable_by_key(|packet| packet.packet_key);
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
    let source_debit = accounting.stationed.min(current.pending_source_infantry);
    if accounting.packet_remaining == 0 {
        packets.delete(ctx, &current.packet_key);
    } else {
        current.infantry = accounting.packet_remaining;
        current.pending_source_infantry -= source_debit;
        current.updated_step = logical_step;
        packets.update(ctx, current.clone());
    }
    packets.decrement_source_queue(&current, source_debit)?;
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
    let source_debit = lost.min(packet.pending_source_infantry);
    if lost == packet.infantry {
        packets.delete(ctx, &packet.packet_key);
    } else {
        packet.infantry -= lost;
        packet.pending_source_infantry -= source_debit;
        packet.updated_step = logical_step;
        packets.update(ctx, packet.clone());
    }
    packets.decrement_source_queue(&packet, source_debit)?;
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
    owner_player_id: u16,
    logical_step: u64,
) -> Result<(), String> {
    let cell = cell_state(ctx, cell_id)?;
    let mut packets: Vec<_> = packet_state
        .by_cell(cell_id)
        .filter(|packet| packet.owner_player_id == owner_player_id)
        .cloned()
        .collect();
    packets.sort_unstable_by_key(|packet| Reverse(packet.packet_key));
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
    cell_owner: u16,
    packet_owner: u16,
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
    let mut packets_by_location = BTreeMap::<(u32, u16), Vec<TickPacket>>::new();
    for packet in packet_state.iter() {
        packets_by_location
            .entry((packet.current_cell, packet.owner_player_id))
            .or_default()
            .push(packet.clone());
    }
    for ((cell_id, owner), mut packets) in packets_by_location {
        let cell = cell_state(ctx, cell_id)?;
        packets.sort_unstable_by_key(|packet| Reverse(packet.packet_key));
        let allocated = packets.iter().map(|packet| packet.infantry).sum();
        let mut trim = packet_trim_required(cell.owner_player_id, owner, cell.infantry, allocated);
        for packet in packets {
            if trim == 0 {
                break;
            }
            let order_id = packet.order_id;
            let lost = trim.min(packet.infantry);
            if let Err(error) =
                reduce_packet_metadata(ctx, packet_state, packet, lost, logical_step, true)
            {
                // Any residual over-allocation at this location is retried by
                // the next tick's trim pass over post-quarantine state.
                quarantine_order(ctx, packet_state, order_id, &error, logical_step);
                break;
            }
            trim -= lost;
        }
    }
    Ok(())
}

fn finalize_orders(
    ctx: &ReducerContext,
    packets: &mut PacketTickState,
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
        let status = match finalized_order_status(
            order.committed_infantry,
            in_transit,
            order.delivered_infantry,
            order.casualty_infantry,
        ) {
            Ok(status) => status,
            Err(error) => {
                // A per-order conservation violation is exactly the class of
                // failure that used to freeze the match forever: the reducer
                // rolled back and the scheduler re-ran the identical state.
                // Park the offending order and let everything else continue.
                let reason = format!("order {} {error}", order.order_id);
                quarantine_order(ctx, packets, order.order_id, &reason, logical_step);
                continue;
            }
        };
        let changed = order.in_transit_infantry != in_transit || order.status != status;
        if status == OrderStatus::Completed {
            order.in_transit_infantry = in_transit;
            order.status = status;
            complete_retreat_abandonments(ctx, packets, &order, logical_step)?;
            ctx.db.expansion_wave().order_id().delete(order.order_id);
            clear_expansion_split_cursors(ctx, order.order_id);
            let route_ids = ctx
                .db
                .transit_route()
                .route_by_order()
                .filter(order.order_id)
                .map(|route| route.route_id)
                .collect::<Vec<_>>();
            for route_id in route_ids {
                ctx.db.transit_route().route_id().delete(route_id);
            }
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
            ctx.db.cell_state().cell_id().update(cell);
            record_capture(ctx, candidate.cell_id, old_owner, NEUTRAL_PLAYER)?;
        }
        ctx.db
            .retreat_abandonment()
            .abandonment_key()
            .delete(candidate.abandonment_key);
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

    fn run_internal_pipeline(
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
                .expect("capacity-safe friendly internal pipeline");
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
        assert!(
            packets.is_empty(),
            "the queued internal pipeline must drain"
        );
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
        // internal order at intermediate capacity stops.
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
        run_internal_pipeline(map, packets, station_blocked_remainders, 16)
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
            packet_key: 1,
            order_id: 1,
            owner_player_id: 1,
            origin_cell: origin,
            current_cell: current,
            destination_cell: destination,
            infantry: 10,
            pending_source_infantry: 10,
            route_id: 1,
            route_index: 0,
            route: Rc::from(route),
            updated_step: 0,
        }
    }

    #[test]
    fn packet_tick_indexes_track_insertions_and_removals() {
        let mut state = PacketTickState::default();
        let mut first = test_packet(10, 10, 11, vec![10, 11]);
        first.packet_key = 1;
        let mut second = test_packet(20, 20, 21, vec![20, 21]);
        second.packet_key = 2;
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
    fn aggregated_expansion_debits_sources_deterministically_and_coalesces_writes() {
        let mut state = PacketTickState::default();
        for (cell_id, queued_infantry) in [(10, 7), (20, 8)] {
            let source = TransferSource {
                source_key: order_cell_key(1, cell_id),
                order_id: 1,
                player_id: 1,
                cell_id,
                committed_infantry: queued_infantry,
                queued_infantry,
            };
            state.source_rows.insert((1, cell_id), source);
            state.sources_by_order.entry(1).or_default().push(cell_id);
        }
        let aggregate = test_packet(EXPANSION_AGGREGATE_ORIGIN, 10, 11, vec![10, 11]);
        state.decrement_source_queue(&aggregate, 12).unwrap();
        assert_eq!(state.source_rows[&(1, 10)].queued_infantry, 0);
        assert_eq!(state.source_rows[&(1, 20)].queued_infantry, 3);
        assert_eq!(state.dirty_sources, BTreeSet::from([(1, 10), (1, 20)]));

        let exact = test_packet(20, 20, 21, vec![20, 21]);
        state.decrement_source_queue(&exact, 2).unwrap();
        assert_eq!(state.source_rows[&(1, 20)].queued_infantry, 1);
        assert_eq!(state.dirty_sources.len(), 2);
        assert!(state.decrement_source_queue(&aggregate, 2).is_err());
    }

    #[test]
    fn expand_resting_nodes_and_one_edge_packets_are_unambiguous() {
        let resting = test_packet(5, 5, 5, vec![5]);
        let edge = test_packet(5, 5, 6, vec![5, 6]);
        assert!(expansion_packet_is_resting(&resting));
        assert!(!expansion_packet_is_resting(&edge));
        assert!(packet_has_pending_source(&resting));

        let mut aggregate = test_packet(EXPANSION_AGGREGATE_ORIGIN, 8, 8, vec![8]);
        aggregate.pending_source_infantry = 0;
        assert!(expansion_packet_is_resting(&aggregate));
        assert!(!packet_has_pending_source(&aggregate));
    }

    #[test]
    fn wave_depth_transitions_start_at_the_perimeter_and_cannot_form_cycles_or_rays() {
        assert!(wave_depth_allows_child(
            WaveNodeDepth::Source,
            WaveNodeDepth::Outside(1)
        ));
        assert!(wave_depth_allows_child(
            WaveNodeDepth::Outside(3),
            WaveNodeDepth::Outside(4)
        ));
        assert!(!wave_depth_allows_child(
            WaveNodeDepth::Source,
            WaveNodeDepth::Source
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
            outside_depths: vec![u16::MAX, 1, u16::MAX, 2, u16::MAX],
            focus_cell_id: None,
            target_cells: Vec::new(),
        };
        assert_eq!(wave_node_depth(&wave, 2), Some(WaveNodeDepth::Source));
        assert_eq!(wave_node_depth(&wave, 4), Some(WaveNodeDepth::Source));
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
        assert_eq!(weights, vec![11, 10, 9]);

        let split = weighted_branch_allocations_rotated(&[33], &weights, 0).unwrap();
        let by_child = split.allocations.into_iter().fold(
            vec![0_u64; children.len()],
            |mut totals, allocation| {
                totals[allocation.child_index] += allocation.amount;
                totals
            },
        );
        assert_eq!(by_child, vec![12, 11, 10]);
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

        assert_eq!(west_weights, vec![11, 9]);
        assert_eq!(east_weights, vec![11, 9]);

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

        assert_eq!(west_split, vec![11, 9]);
        assert_eq!(east_split, vec![11, 9]);
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
            OrderKind::Reshape,
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
        for kind in [OrderKind::Reshape, OrderKind::FrontRebalance] {
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
    fn queued_internal_funnel_converges_while_stationing_reproduces_the_pockets() {
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
            "old stop-in-place behavior must leave declared internal demand undelivered"
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
        for kind in [OrderKind::Reshape, OrderKind::FrontRebalance] {
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
        for kind in [OrderKind::Reshape, OrderKind::FrontRebalance] {
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
        let front_rebalance = test_order(OrderKind::FrontRebalance, OrderStatus::Active);
        add_internal_destination_reservation(&mut reservations, &front_rebalance, 7, 50, 20)
            .unwrap();
        assert_eq!(reserved_recruitment_capacity(&reservations, 1, 7), 30);

        let reshape = test_order(OrderKind::Reshape, OrderStatus::Active);
        add_internal_destination_reservation(&mut reservations, &reshape, 7, 10, 5).unwrap();
        assert_eq!(reserved_recruitment_capacity(&reservations, 1, 7), 35);

        let push = test_order(OrderKind::PushFront, OrderStatus::Active);
        add_internal_destination_reservation(&mut reservations, &push, 7, 100, 0).unwrap();
        let completed = test_order(OrderKind::Reshape, OrderStatus::Completed);
        add_internal_destination_reservation(&mut reservations, &completed, 7, 100, 0).unwrap();
        assert_eq!(reserved_recruitment_capacity(&reservations, 1, 7), 35);

        let mut foreign = test_order(OrderKind::FrontRebalance, OrderStatus::Active);
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
        for kind in [OrderKind::Reshape, OrderKind::FrontRebalance] {
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
    fn owner_validation_accepts_neutral_and_every_configured_player() {
        for owner in 0..=8 {
            assert!(valid_owner(owner, 8));
        }
        assert!(valid_owner(500, 500));
        assert!(!valid_owner(9, 8));
        assert!(!valid_owner(501, 500));
        assert!(!valid_owner(3, 2));
    }

    #[test]
    fn high_scale_population_shard_preserves_interval_frequency() {
        let interval = 4_u32;
        let step = 9_u64;
        let active_shard = u16::try_from(step % u64::from(interval)).unwrap();
        assert_eq!(active_shard, 1);
        for cell_id in [1_u32, 5, 9, 13] {
            assert_eq!(u16::try_from(cell_id % interval).unwrap(), active_shard);
        }
        for cell_id in [2_u32, 3, 4, 6] {
            assert_ne!(u16::try_from(cell_id % interval).unwrap(), active_shard);
        }
        // Wider type must preserve intervals above 255 without truncation.
        let wide_interval = 300_u32;
        let wide_cell = 301_u32;
        let wide_shard = u16::try_from(wide_cell % wide_interval).unwrap();
        assert_eq!(wide_shard, 1);
        let truncated_interval = u32::from(wide_interval as u8); // 300u8 == 44
        assert_eq!(truncated_interval, 44);
        assert_ne!(
            wide_cell % wide_interval,
            wide_cell % truncated_interval,
            "u8 interval truncation would change the shard cycle"
        );
    }

    #[test]
    fn packet_tick_load_scopes_routes_and_sources_to_active_orders() {
        // Documented contract for PacketTickState::load: full active packet set,
        // routes only for referenced route_ids, sources via source_by_order for
        // the union of packet order IDs and ACTIVE transfer order IDs (covers
        // queued sources on active orders with no packet yet). Combat remains
        // unsharded/shared.
        let packet_order_ids = [3_u64, 9];
        let active_transfer_order_ids = [3_u64, 9, 5]; // 5 has sources but no packet
        let mut order_ids: BTreeSet<u64> = packet_order_ids.into_iter().collect();
        order_ids.extend(active_transfer_order_ids);
        let all_sources = [(1_u64, 10_u32), (3, 11), (3, 12), (9, 13), (4, 14), (5, 15)];
        let scoped: Vec<_> = all_sources
            .into_iter()
            .filter(|(order_id, _)| order_ids.contains(order_id))
            .collect();
        assert_eq!(scoped, vec![(3, 11), (3, 12), (9, 13), (5, 15)]);
        assert!(
            !scoped
                .iter()
                .any(|(order_id, _)| *order_id == 1 || *order_id == 4)
        );
        let referenced_routes = [7_u64, 8, 7];
        let unique_routes: BTreeSet<_> = referenced_routes.into_iter().collect();
        assert_eq!(unique_routes.len(), 2);
    }

    #[test]
    fn controlled_cell_loss_rejects_counter_underflow() {
        assert_eq!(controlled_after_loss(2), Ok(1));
        assert_eq!(
            controlled_after_loss(0),
            Err("controlled-cell count underflow".to_owned())
        );
    }

    #[test]
    fn capture_accounting_increments_and_decrements_both_counters() {
        let taken_from_player = capture_accounting(2, 1, 10, 4, 100).unwrap();
        assert_eq!(
            taken_from_player,
            CaptureAccounting {
                old_controlled: Some(9),
                new_controlled: Some(5),
                victory: false,
            }
        );

        let taken_from_neutral = capture_accounting(NEUTRAL_PLAYER, 1, 0, 4, 100).unwrap();
        assert_eq!(taken_from_neutral.old_controlled, None);
        assert_eq!(taken_from_neutral.new_controlled, Some(5));

        let relinquished = capture_accounting(1, NEUTRAL_PLAYER, 4, 0, 3).unwrap();
        assert_eq!(relinquished.old_controlled, Some(3));
        assert_eq!(relinquished.new_controlled, None);
    }

    #[test]
    fn victory_triggers_exactly_at_the_required_control_threshold() {
        let below = capture_accounting(NEUTRAL_PLAYER, 1, 0, 98, 100).unwrap();
        assert_eq!(below.new_controlled, Some(99));
        assert!(!below.victory);

        let exactly = capture_accounting(NEUTRAL_PLAYER, 1, 0, 99, 100).unwrap();
        assert_eq!(exactly.new_controlled, Some(100));
        assert!(exactly.victory);

        let above = capture_accounting(2, 1, 5, 100, 100).unwrap();
        assert_eq!(above.new_controlled, Some(101));
        assert!(above.victory);
    }

    #[test]
    fn neutral_captures_never_win_a_match_and_underflow_is_rejected() {
        // Relinquishing to neutral can never produce a winner even when the
        // "required control" is trivially low.
        let to_neutral = capture_accounting(1, NEUTRAL_PLAYER, 4, u64::MAX, 0).unwrap();
        assert!(!to_neutral.victory);

        assert!(capture_accounting(1, 2, 0, 5, 100).is_err());
        assert!(capture_accounting(1, 2, 5, u64::MAX, u64::MAX).is_err());
    }

    #[test]
    fn civilians_regrow_toward_capacity_with_a_minimum_of_one() {
        // 200 bps of 50 missing civilians = 1 per interval.
        let next = population_cell_transition(50, 100, 0, 100, 200, 0, 10, 0, false);
        assert_eq!(
            next,
            PopulationTransition {
                civilians: 51,
                infantry: 0
            }
        );

        // Tiny deficits still regrow by at least one, and never overshoot.
        let almost_full = population_cell_transition(99, 100, 0, 100, 200, 0, 10, 0, false);
        assert_eq!(almost_full.civilians, 100);
        let full = population_cell_transition(100, 100, 0, 100, 200, 0, 10, 0, false);
        assert_eq!(full.civilians, 100);
    }

    #[test]
    fn mobilization_conserves_population_and_respects_every_bound() {
        // 50% target of 100 population wants 50 infantry; the per-step budget
        // caps conversion at 10 and total population is conserved (no growth:
        // civilians already at capacity).
        let next = population_cell_transition(100, 100, 0, 200, 200, 5_000, 10, 0, false);
        assert_eq!(
            next,
            PopulationTransition {
                civilians: 90,
                infantry: 10
            }
        );
        assert_eq!(next.civilians + next.infantry, 100);

        // Military capacity headroom (including reservations) is respected.
        let capped = population_cell_transition(100, 100, 47, 50, 200, 10_000, 100, 2, false);
        assert_eq!(capped.infantry, 48);
        assert_eq!(capped.civilians + capped.infantry, 147);

        // Retreating edge cells never recruit.
        let retreating = population_cell_transition(100, 100, 0, 200, 200, 5_000, 10, 0, true);
        assert_eq!(retreating.infantry, 0);

        // Rounding neither creates nor destroys population across a long run:
        // each step's total change equals exactly the regrowth amount, which
        // is bounded by the remaining civilian deficit.
        let mut civilians = 73_u64;
        let mut infantry = 9_u64;
        for _ in 0..500 {
            let before_total = civilians + infantry;
            let deficit = 100 - civilians;
            let next =
                population_cell_transition(civilians, 100, infantry, 80, 150, 3_333, 7, 0, false);
            let grown = next.civilians + next.infantry - before_total;
            assert!(grown <= deficit, "growth is bounded by the deficit");
            civilians = next.civilians;
            infantry = next.infantry;
            assert!(infantry <= 80, "military capacity is never exceeded");
        }
        // Deterministic fixed point: full civilian capacity plus the largest
        // infantry count where the 33.33% target is already satisfied.
        assert_eq!((civilians, infantry), (100, 49));
    }

    #[test]
    fn lowering_the_mobilization_target_never_demobilizes() {
        let mobilized = population_cell_transition(50, 100, 50, 200, 200, 0, 100, 0, false);
        assert_eq!(mobilized.infantry, 50);
        assert!(mobilized.civilians >= 50);

        // Target of 10% wants 10 infantry but 50 are already mobilized: no
        // conversion in either direction beyond regular regrowth.
        let lowered = population_cell_transition(50, 100, 50, 200, 200, 1_000, 100, 0, false);
        assert_eq!(lowered.infantry, 50);
    }

    #[test]
    fn per_order_conservation_violations_map_to_quarantine_not_fail_stop() {
        // The exact class of failure that used to freeze the match: an
        // order's accounting no longer sums to its commitment. The finalize
        // phase must classify this as attributable (quarantine) rather than
        // propagate it out of the scheduled reducer.
        let broken = finalized_order_status(100, 10, 50, 30);
        assert!(broken.is_err(), "90 accounted of 100 committed must fail");
        let overflow = finalized_order_status(100, u64::MAX, 1, 0);
        assert!(overflow.is_err());

        // Healthy orders keep their previous lifecycle transitions.
        assert_eq!(
            finalized_order_status(100, 0, 70, 30),
            Ok(OrderStatus::Completed)
        );
        assert_eq!(
            finalized_order_status(100, 10, 60, 30),
            Ok(OrderStatus::Active)
        );
    }

    #[test]
    fn quarantined_orders_leave_the_tick_and_survive_history_prune() {
        // Packet load and finalize only pull Active rows; a quarantined order
        // therefore cannot re-enter movement/combat and re-fail the tick.
        assert!(order_participates_in_tick(OrderStatus::Active));
        assert!(!order_participates_in_tick(OrderStatus::Quarantined));
        assert!(!order_participates_in_tick(OrderStatus::Completed));
        assert!(!order_participates_in_tick(OrderStatus::Cancelled));

        // Completed/Cancelled feedback rows age out; quarantined records are
        // the operator-visible invariant failure and are never pruned by the
        // retention pass.
        assert!(order_status_is_prunable_history(OrderStatus::Completed));
        assert!(order_status_is_prunable_history(OrderStatus::Cancelled));
        assert!(!order_status_is_prunable_history(OrderStatus::Quarantined));
        assert!(!order_status_is_prunable_history(OrderStatus::Active));
        assert!(!order_history_is_prunable(100, 100 + ORDER_RETENTION_STEPS));
        assert!(order_history_is_prunable(
            100,
            100 + ORDER_RETENTION_STEPS + 1
        ));
    }

    #[test]
    fn tick_phases_keep_finalize_population_and_prune_after_combat() {
        // Mid-tick quarantine of one order must not skip later phases: the
        // remaining orders still finalize, population still runs on cadence,
        // and history prune still runs on its interval.
        assert_eq!(
            TICK_PHASES,
            &[
                "packet_load",
                "trim",
                "branch",
                "move",
                "combat",
                "finalize",
                "population",
                "prune",
            ]
        );
        let combat = TICK_PHASES
            .iter()
            .position(|phase| *phase == "combat")
            .unwrap();
        let finalize = TICK_PHASES
            .iter()
            .position(|phase| *phase == "finalize")
            .unwrap();
        let population = TICK_PHASES
            .iter()
            .position(|phase| *phase == "population")
            .unwrap();
        let prune = TICK_PHASES
            .iter()
            .position(|phase| *phase == "prune")
            .unwrap();
        assert!(combat < finalize && finalize < population && population < prune);
    }

    #[test]
    fn rejected_fronts_quarantine_only_their_contributing_orders() {
        let fronts = [(10_u32, vec![1_u64, 2]), (20, vec![3]), (30, vec![4, 5])];
        let rejected = BTreeSet::from([10_u64, 30]);
        let quarantined = orders_for_rejected_fronts(&fronts, &rejected);
        assert_eq!(quarantined, BTreeSet::from([1, 2, 4, 5]));
        // The sibling valid front's order is untouched and continues resolving.
        assert!(!quarantined.contains(&3));
    }

    #[test]
    fn module_min_casualty_adjustment_forces_one_on_engaged_sides() {
        assert_eq!(minimum_casualty(true, 0, 50), 1);
        assert_eq!(minimum_casualty(true, 7, 50), 7);
        assert_eq!(minimum_casualty(false, 0, 50), 0);
        assert_eq!(minimum_casualty(true, 0, 0), 0);
        assert_eq!(minimum_casualty(true, 9, 5), 5);
    }

    #[test]
    fn mixed_owner_kernel_capture_survives_module_min_casualty_adjustment() {
        // Two owners attack one defender; after the module bumps sub-lethal
        // attacker casualties by one, re-running select_capture must still
        // pick the stronger surviving owner (mirrors resolve_target_combat).
        let attacks = [
            AttackFront {
                id: 10,
                attacker: 1,
                from: Axial::new(1, 0),
                from_elevation: 0,
                offered: 20,
                frontage: 25,
            },
            AttackFront {
                id: 20,
                attacker: 2,
                from: Axial::new(0, 1),
                from_elevation: 0,
                offered: 40,
                frontage: 25,
            },
        ];
        let resolution =
            resolve_edge_combat(Axial::ZERO, 30, 0, &attacks, &CombatConfig::default()).unwrap();
        assert!(resolution.rejected.is_empty());
        assert_eq!(resolution.defender_remaining, 0);
        let mut adjusted = resolution.attacks.clone();
        for outcome in adjusted.values_mut() {
            let casualties = minimum_casualty(
                outcome.engaged > 0 && outcome.defense_allocated > 0,
                outcome.attacker_casualties,
                outcome.offered,
            );
            outcome.attacker_remaining = outcome.offered.saturating_sub(casualties);
        }
        let (owner, front) = select_capture(&adjusted);
        assert_eq!(owner, Some(2));
        assert_eq!(front, Some(20));
        // Default lethality already produces the same capture; the module
        // re-run exists so a forced +1 casualty cannot flip the winner.
        assert_eq!(owner, resolution.capturing_owner);
        assert_eq!(front, resolution.capturing_front);
    }

    #[test]
    fn victory_glue_completes_the_match_for_the_capturing_owner() {
        let accounting = capture_accounting(NEUTRAL_PLAYER, 3, 0, 99, 100).unwrap();
        assert!(accounting.victory);
        assert_eq!(
            match_state_after_capture(MatchPhase::Running, 0, 3, accounting.victory),
            (MatchPhase::Completed, 3)
        );

        let short = capture_accounting(NEUTRAL_PLAYER, 3, 0, 50, 100).unwrap();
        assert!(!short.victory);
        assert_eq!(
            match_state_after_capture(MatchPhase::Running, 0, 3, short.victory),
            (MatchPhase::Running, 0)
        );

        // Relinquishing to neutral never completes the match.
        let to_neutral = capture_accounting(1, NEUTRAL_PLAYER, 4, 0, 1).unwrap();
        assert!(!to_neutral.victory);
        assert_eq!(
            match_state_after_capture(MatchPhase::Running, 0, NEUTRAL_PLAYER, to_neutral.victory),
            (MatchPhase::Running, 0)
        );
    }
}
