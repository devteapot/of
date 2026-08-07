use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU32, Ordering},
    },
};
#[cfg(not(target_arch = "wasm32"))]
use std::{
    fs, io,
    path::Path,
    process,
    time::{SystemTime, UNIX_EPOCH},
};

use bevy::prelude::*;
use hex_core::{Axial, TerrainKind};
use match_bindings::{
    CellState, CellStateTableAccess, CellTerrain, CellTerrainTableAccess, CombatFront,
    CombatFrontTableAccess, CommandReceipt, CommandReceiptTableAccess, DbConnection, MatchConfig,
    MatchConfigTableAccess, MatchPhase as RemoteMatchPhase, MatchState, MatchStateTableAccess,
    MobilizationPolicy, MobilizationPolicyTableAccess, OrderStatus, PlayerSlot,
    PlayerSlotTableAccess, PlayerState, PlayerStateTableAccess, ReceiptStatus, SubscriptionHandle,
    TransferDestination, TransferDestinationTableAccess, TransferOrder, TransferOrderTableAccess,
    TransferSource, TransferSourceTableAccess, TransitPacket, TransitRoute, cancel_orders,
    issue_attack_clusters, issue_expand_all, issue_expand_clusters, issue_front_rebalance,
    issue_push_front, issue_reshape, join_match, set_mobilization_target,
};
use match_bindings::{TransitPacketTableAccess, TransitRouteTableAccess};
use spacetimedb_sdk::__codegen::InternalError;
use spacetimedb_sdk::{DbContext, SubscriptionHandle as _, Table, TableWithPrimaryKey};

use crate::{
    camera::{CameraRig, GameCamera},
    config::ClientConfig,
    geometry::{axial_to_plane, chunk_of, plane_to_axial},
    map_view::MapViewMode,
    model::{
        ActiveFlow, ActiveFront, AuthorityState, CellView, ConnectionState, ContestedCellView,
        MatchPhase, MatchView, RetaskProjection, ToastKind,
    },
    network::{ClientIntent, NetworkSet, ServerUpdate},
};

const TERRAIN_DIRTY: u32 = 1 << 0;
const CELLS_DIRTY: u32 = 1 << 1;
const MATCH_DIRTY: u32 = 1 << 2;
const PLAYERS_DIRTY: u32 = 1 << 3;
const MOBILIZATION_DIRTY: u32 = 1 << 4;
const RECEIPTS_DIRTY: u32 = 1 << 5;
const FLOWS_DIRTY: u32 = 1 << 6;
const FRONTS_DIRTY: u32 = 1 << 7;
const ORDERS_DIRTY: u32 = 1 << 8;
const ALL_DIRTY: u32 = (1 << 9) - 1;

const PACKET_TABLE: &str = "transit_packet";
const ROUTE_TABLE: &str = "transit_route";

/// Immutable terrain + match/player metadata only. Bootstrap must not flood the
/// client with `cell_state` / combat / tactical rows before the local seat is known.
const BOOTSTRAP_CLIENT_SUBSCRIPTIONS: [&str; 5] = [
    "SELECT * FROM cell_terrain",
    "SELECT * FROM match_config",
    "SELECT * FROM match_state",
    "SELECT * FROM player_slot",
    "SELECT * FROM player_state",
];

const HIGH_SCALE_PLAYER_THRESHOLD: u16 = 8;
/// Moving viewport interest radius in cell-state chunk units (server `chunk_size`,
/// typically 16). Bandwidth interest only — not a security boundary. Local-owned
/// cells stay on the one-time tactical subscription globally; this radius only
/// bounds the separate spatial `CellState` handle around the camera focus.
pub const HIGH_SCALE_INTEREST_CHUNK_RADIUS: i16 = 2;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SpatialInterest {
    /// Center chunk of the spatial `CellState` subscription (spawn until the
    /// camera is available, then camera focus chunk).
    focus_chunk_q: i16,
    focus_chunk_r: i16,
    radius: i16,
}

impl SpatialInterest {
    const fn center(self) -> (i16, i16) {
        (self.focus_chunk_q, self.focus_chunk_r)
    }
}

/// Bootstrap / lobby projections only.
fn bootstrap_subscription_queries() -> Vec<String> {
    BOOTSTRAP_CLIENT_SUBSCRIPTIONS
        .iter()
        .map(|query| (*query).to_owned())
        .collect()
}

fn packet_query(player_filter: Option<u16>) -> String {
    match player_filter {
        Some(player) => format!("SELECT * FROM {PACKET_TABLE} WHERE owner_player_id = {player}"),
        None => format!("SELECT * FROM {PACKET_TABLE}"),
    }
}

fn route_query(player_filter: Option<u16>) -> String {
    match player_filter {
        Some(player) => format!("SELECT * FROM {ROUTE_TABLE} WHERE player_id = {player}"),
        None => format!("SELECT * FROM {ROUTE_TABLE}"),
    }
}

/// Tactical-only queries for a bound seat (one-time handle).
///
/// - `player_count <= 8`: full `cell_state` + `combat_front` + full tactical rows.
/// - `player_count > 8`: all local-owned `CellState` globally, local attacker/
///   defender combat fronts, and local tactical rows. Spatial remote `CellState`
///   around the camera lives on a **separate** moving subscription handle.
///
/// Intentionally excludes bootstrap globals so the tactical subscription does not
/// duplicate them. Missing remote state rows render as neutral defaults.
fn tactical_subscription_queries(player_count: u16, local_player: u16) -> Vec<String> {
    if player_count <= HIGH_SCALE_PLAYER_THRESHOLD {
        return vec![
            "SELECT * FROM cell_state".to_owned(),
            "SELECT * FROM combat_front".to_owned(),
            "SELECT * FROM command_receipt".to_owned(),
            "SELECT * FROM mobilization_policy".to_owned(),
            "SELECT * FROM transfer_destination".to_owned(),
            "SELECT * FROM transfer_order".to_owned(),
            "SELECT * FROM transfer_source".to_owned(),
            route_query(None),
            packet_query(None),
        ];
    }
    vec![
        format!("SELECT * FROM cell_state WHERE owner_player_id = {local_player}"),
        format!("SELECT * FROM combat_front WHERE attacker_player_id = {local_player}"),
        format!("SELECT * FROM combat_front WHERE defender_player_id = {local_player}"),
        format!("SELECT * FROM command_receipt WHERE player_id = {local_player}"),
        format!("SELECT * FROM mobilization_policy WHERE player_id = {local_player}"),
        format!("SELECT * FROM transfer_destination WHERE player_id = {local_player}"),
        format!("SELECT * FROM transfer_order WHERE player_id = {local_player}"),
        format!("SELECT * FROM transfer_source WHERE player_id = {local_player}"),
        route_query(Some(local_player)),
        packet_query(Some(local_player)),
    ]
}

/// Separate high-scale spatial `CellState` interest around a focus chunk.
/// Bandwidth only — not auth. Does not include local-owned (that stays tactical).
fn spatial_cell_state_queries(interest: SpatialInterest) -> Vec<String> {
    let qmin = interest.focus_chunk_q.saturating_sub(interest.radius);
    let qmax = interest.focus_chunk_q.saturating_add(interest.radius);
    let rmin = interest.focus_chunk_r.saturating_sub(interest.radius);
    let rmax = interest.focus_chunk_r.saturating_add(interest.radius);
    vec![format!(
        "SELECT * FROM cell_state WHERE chunk_q >= {qmin} AND chunk_q <= {qmax} AND chunk_r >= {rmin} AND chunk_r <= {rmax}"
    )]
}

fn chunk_coords_for_cell_id(cell_id: u32, map_width: u16, chunk_size: u16) -> (i16, i16) {
    let width = u32::from(map_width.max(1));
    let size = i32::from(chunk_size.max(1));
    let column = i32::try_from(cell_id % width).unwrap_or(0);
    let row = i32::try_from(cell_id / width).unwrap_or(0);
    (
        i16::try_from(column.div_euclid(size)).unwrap_or(0),
        i16::try_from(row.div_euclid(size)).unwrap_or(0),
    )
}

fn chunk_coords_for_axial(
    coordinate: Axial,
    map_origin_q: i32,
    map_origin_r: i32,
    chunk_size: u16,
) -> (i16, i16) {
    let size = i32::from(chunk_size.max(1));
    let column = coordinate.q - map_origin_q;
    let row = coordinate.r - map_origin_r;
    (
        i16::try_from(column.div_euclid(size)).unwrap_or(0),
        i16::try_from(row.div_euclid(size)).unwrap_or(0),
    )
}

/// Tracks bootstrap vs tactical readiness so commands cannot fire after the
/// lobby snapshot alone. Reconnect clears both flags.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct SubscriptionLifecycle {
    bootstrap_ready: bool,
    tactical_ready: bool,
}

impl SubscriptionLifecycle {
    const fn commands_ready(self) -> bool {
        self.bootstrap_ready && self.tactical_ready
    }

    const fn reset() -> Self {
        Self {
            bootstrap_ready: false,
            tactical_ready: false,
        }
    }

    const fn on_bootstrap_applied(self) -> Self {
        Self {
            bootstrap_ready: true,
            tactical_ready: self.tactical_ready,
        }
    }

    const fn on_tactical_start(self) -> Self {
        Self {
            bootstrap_ready: self.bootstrap_ready,
            tactical_ready: false,
        }
    }

    const fn on_tactical_applied(self) -> Self {
        Self {
            bootstrap_ready: self.bootstrap_ready,
            tactical_ready: true,
        }
    }
}

#[derive(SystemSet, Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct OnlineSyncSet;

#[derive(Clone, Debug)]
enum LifecycleEvent {
    Connected { generation: u64 },
    BootstrapSubscribed { generation: u64 },
    TacticalSubscribed { generation: u64 },
    JoinFailed { generation: u64, reason: String },
    ConnectionFailed { generation: u64, reason: String },
    Disconnected { generation: u64, reason: String },
    CommandFailed { command_id: u64, reason: String },
    TokenWarning { generation: u64, message: String },
}

#[derive(Default)]
struct SharedSignals {
    dirty: AtomicU32,
    cell_changes: Mutex<BTreeMap<u32, CellState>>,
    /// Cells removed from the client cache (interest leave / map replace).
    /// Projected to neutral/default without requiring a full terrain rebuild.
    cell_absences: Mutex<BTreeSet<u32>>,
    packet_changes: Mutex<BTreeMap<u64, Option<TransitPacket>>>,
    route_changes: Mutex<BTreeMap<u64, Option<TransitRoute>>>,
    front_changes: Mutex<BTreeMap<String, Option<CombatFront>>>,
    order_changes: Mutex<BTreeMap<u64, Option<TransferOrder>>>,
    source_changes: Mutex<BTreeMap<u128, Option<TransferSource>>>,
    destination_changes: Mutex<BTreeMap<u128, Option<TransferDestination>>>,
    events: Mutex<VecDeque<LifecycleEvent>>,
}

impl SharedSignals {
    fn mark(&self, bits: u32) {
        self.dirty.fetch_or(bits, Ordering::Release);
    }

    fn take_dirty(&self) -> u32 {
        self.dirty.swap(0, Ordering::AcqRel)
    }

    /// Drop every pending row delta and dirty bit. Used on reconnect so a new
    /// generation cannot apply stale callbacks from the previous connection.
    fn clear_pending_deltas(&self) {
        self.dirty.store(0, Ordering::Release);
        if let Ok(mut cells) = self.cell_changes.lock() {
            cells.clear();
        }
        if let Ok(mut absences) = self.cell_absences.lock() {
            absences.clear();
        }
        if let Ok(mut packets) = self.packet_changes.lock() {
            packets.clear();
        }
        if let Ok(mut routes) = self.route_changes.lock() {
            routes.clear();
        }
        if let Ok(mut fronts) = self.front_changes.lock() {
            fronts.clear();
        }
        if let Ok(mut orders) = self.order_changes.lock() {
            orders.clear();
        }
        if let Ok(mut sources) = self.source_changes.lock() {
            sources.clear();
        }
        if let Ok(mut destinations) = self.destination_changes.lock() {
            destinations.clear();
        }
    }

    fn record_cell(&self, cell: &CellState) {
        if let Ok(mut absences) = self.cell_absences.lock() {
            absences.remove(&cell.cell_id);
        }
        if let Ok(mut changes) = self.cell_changes.lock() {
            changes.insert(cell.cell_id, cell.clone());
        }
        self.mark(CELLS_DIRTY);
    }

    fn record_cell_absence(&self, cell_id: u32) {
        if let Ok(mut changes) = self.cell_changes.lock() {
            changes.remove(&cell_id);
        }
        if let Ok(mut absences) = self.cell_absences.lock() {
            absences.insert(cell_id);
        }
        self.mark(CELLS_DIRTY);
    }

    fn take_cell_changes(&self) -> Vec<CellState> {
        self.cell_changes.lock().map_or_else(
            |_| Vec::new(),
            |mut changes| std::mem::take(&mut *changes).into_values().collect(),
        )
    }

    fn take_cell_absences(&self) -> BTreeSet<u32> {
        self.cell_absences.lock().map_or_else(
            |_| BTreeSet::new(),
            |mut absences| std::mem::take(&mut *absences),
        )
    }

    fn take_tactical_changes(&self) -> TacticalChanges {
        TacticalChanges {
            packets: take_changes(&self.packet_changes),
            routes: take_changes(&self.route_changes),
            fronts: take_changes(&self.front_changes),
            orders: take_changes(&self.order_changes),
            sources: take_changes(&self.source_changes),
            destinations: take_changes(&self.destination_changes),
        }
    }

    fn push(&self, event: LifecycleEvent) {
        if let Ok(mut events) = self.events.lock() {
            events.push_back(event);
        }
    }

    fn drain(&self) -> Vec<LifecycleEvent> {
        self.events
            .lock()
            .map_or_else(|_| Vec::new(), |mut events| events.drain(..).collect())
    }
}

fn take_changes<K: Ord, V>(changes: &Mutex<BTreeMap<K, Option<V>>>) -> BTreeMap<K, Option<V>> {
    changes.lock().map_or_else(
        |_| BTreeMap::new(),
        |mut changes| std::mem::take(&mut *changes),
    )
}

fn record_change<K: Ord, V>(changes: &Mutex<BTreeMap<K, Option<V>>>, key: K, value: Option<V>) {
    if let Ok(mut changes) = changes.lock() {
        changes.insert(key, value);
    }
}

#[derive(Default)]
struct TacticalChanges {
    packets: BTreeMap<u64, Option<TransitPacket>>,
    routes: BTreeMap<u64, Option<TransitRoute>>,
    fronts: BTreeMap<String, Option<CombatFront>>,
    orders: BTreeMap<u64, Option<TransferOrder>>,
    sources: BTreeMap<u128, Option<TransferSource>>,
    destinations: BTreeMap<u128, Option<TransferDestination>>,
}

#[derive(Default)]
struct TacticalCache {
    packets: BTreeMap<u64, TransitPacket>,
    packet_ids_by_order: BTreeMap<u64, BTreeSet<u64>>,
    packet_ids_by_route: BTreeMap<u64, BTreeSet<u64>>,
    routes: BTreeMap<u64, TransitRoute>,
    fronts: BTreeMap<String, CombatFront>,
    orders: BTreeMap<u64, TransferOrder>,
    sources: BTreeMap<u128, TransferSource>,
    source_keys_by_order: BTreeMap<u64, BTreeSet<u128>>,
    destinations: BTreeMap<u128, TransferDestination>,
    destination_keys_by_order: BTreeMap<u64, BTreeSet<u128>>,
}

impl TacticalCache {
    fn capture(connection: &DbConnection) -> Self {
        let mut cache = Self {
            packets: subscribed_packets(connection)
                .into_iter()
                .map(|packet| (packet.packet_key, packet))
                .collect(),
            routes: subscribed_routes(connection)
                .into_iter()
                .map(|route| (route.route_id, route))
                .collect(),
            fronts: connection
                .db
                .combat_front()
                .iter()
                .map(|front| (front.front_key.clone(), front))
                .collect(),
            orders: connection
                .db
                .transfer_order()
                .iter()
                .filter(|order| order.status == OrderStatus::Active)
                .map(|order| (order.order_id, order))
                .collect(),
            sources: connection
                .db
                .transfer_source()
                .iter()
                .map(|source| (source.source_key, source))
                .collect(),
            destinations: connection
                .db
                .transfer_destination()
                .iter()
                .map(|destination| (destination.destination_key, destination))
                .collect(),
            ..Default::default()
        };
        cache.rebuild_indexes();
        cache
    }

    fn apply(&mut self, changes: TacticalChanges) {
        for (key, value) in changes.packets {
            if let Some(previous) = self.packets.remove(&key) {
                remove_index_value(&mut self.packet_ids_by_order, previous.order_id, key);
                if previous.route_id != 0 {
                    remove_index_value(&mut self.packet_ids_by_route, previous.route_id, key);
                }
            }
            if let Some(value) = value {
                self.packet_ids_by_order
                    .entry(value.order_id)
                    .or_default()
                    .insert(key);
                if value.route_id != 0 {
                    self.packet_ids_by_route
                        .entry(value.route_id)
                        .or_default()
                        .insert(key);
                }
                self.packets.insert(key, value);
            }
        }
        apply_changes(&mut self.routes, changes.routes);
        apply_changes(&mut self.fronts, changes.fronts);
        apply_changes(&mut self.orders, changes.orders);
        for (key, value) in changes.sources {
            if let Some(previous) = self.sources.remove(&key) {
                remove_index_value(&mut self.source_keys_by_order, previous.order_id, key);
            }
            if let Some(value) = value {
                self.source_keys_by_order
                    .entry(value.order_id)
                    .or_default()
                    .insert(key);
                self.sources.insert(key, value);
            }
        }
        for (key, value) in changes.destinations {
            if let Some(previous) = self.destinations.remove(&key) {
                remove_index_value(&mut self.destination_keys_by_order, previous.order_id, key);
            }
            if let Some(value) = value {
                self.destination_keys_by_order
                    .entry(value.order_id)
                    .or_default()
                    .insert(key);
                self.destinations.insert(key, value);
            }
        }
    }

    fn rebuild_indexes(&mut self) {
        self.packet_ids_by_order.clear();
        self.packet_ids_by_route.clear();
        self.source_keys_by_order.clear();
        self.destination_keys_by_order.clear();
        for packet in self.packets.values() {
            self.packet_ids_by_order
                .entry(packet.order_id)
                .or_default()
                .insert(packet.packet_key);
            if packet.route_id != 0 {
                self.packet_ids_by_route
                    .entry(packet.route_id)
                    .or_default()
                    .insert(packet.packet_key);
            }
        }
        for source in self.sources.values() {
            self.source_keys_by_order
                .entry(source.order_id)
                .or_default()
                .insert(source.source_key);
        }
        for destination in self.destinations.values() {
            self.destination_keys_by_order
                .entry(destination.order_id)
                .or_default()
                .insert(destination.destination_key);
        }
    }
}

fn remove_index_value<K: Ord + Copy, V: Ord + Copy>(
    index: &mut BTreeMap<K, BTreeSet<V>>,
    key: K,
    value: V,
) {
    let remove_entry = index.get_mut(&key).is_some_and(|values| {
        values.remove(&value);
        values.is_empty()
    });
    if remove_entry {
        index.remove(&key);
    }
}

fn apply_changes<K: Ord, V>(rows: &mut BTreeMap<K, V>, changes: BTreeMap<K, Option<V>>) {
    for (key, value) in changes {
        if let Some(value) = value {
            rows.insert(key, value);
        } else {
            rows.remove(&key);
        }
    }
}

#[derive(Clone, Debug)]
enum PendingCommand {
    ExpandClusters,
    AttackClusters,
    PushFront,
    FrontRebalance,
    ExpandAll,
    Reshape,
    CancelOrders,
    Mobilization { target: f32 },
}

impl PendingCommand {
    const fn label(&self) -> &'static str {
        match self {
            Self::ExpandClusters => "Expand Clusters",
            Self::AttackClusters => "Attack Clusters",
            Self::PushFront => "Push Front",
            Self::FrontRebalance => "Front Rebalance",
            Self::ExpandAll => "Expand Perimeter",
            Self::Reshape => "Reshape",
            Self::CancelOrders => "Stop Orders",
            Self::Mobilization { .. } => "Mobilization",
        }
    }

    const fn receipt_name(&self) -> &'static str {
        match self {
            Self::ExpandClusters => "issue_expand_clusters",
            Self::AttackClusters => "issue_attack_clusters",
            Self::PushFront => "issue_push_front",
            Self::FrontRebalance => "issue_front_rebalance",
            Self::ExpandAll => "issue_expand_all",
            Self::Reshape => "issue_reshape",
            Self::CancelOrders => "cancel_orders",
            Self::Mobilization { .. } => "set_mobilization_target",
        }
    }

    const fn is_modal(&self) -> bool {
        !matches!(self, Self::Mobilization { .. })
    }
}

#[derive(Resource)]
struct OnlineTransport {
    connection: Option<DbConnection>,
    #[cfg(target_arch = "wasm32")]
    pending_connection: Option<PendingConnection>,
    signals: Arc<SharedSignals>,
    config: ClientConfig,
    coordinate_to_id: BTreeMap<Axial, u32>,
    id_to_coordinate: BTreeMap<u32, Axial>,
    tactical: TacticalCache,
    retask_source_counts: BTreeMap<(u64, Axial), u32>,
    retask_destination_claim_counts: BTreeMap<(u64, Axial), u32>,
    retask_edge_counts: BTreeMap<((u32, u32), u64), u32>,
    retask_orders_by_edge: BTreeMap<(u32, u32), BTreeSet<u64>>,
    pending: BTreeMap<u64, PendingCommand>,
    processed_receipts: BTreeSet<u128>,
    terminal_command_ids: BTreeSet<u64>,
    next_command_id: u64,
    bound_player: Option<u16>,
    /// Bootstrap (globals) and tactical readiness are tracked separately so
    /// lobby snapshots never unlock commands before the seat-scoped tactical
    /// subscription has applied.
    subscription_lifecycle: SubscriptionLifecycle,
    /// Player id used for the single post-bind tactical subscription.
    tactical_subscription_player: Option<u16>,
    /// One-time tactical handle (local-owned + combat + tactical tables).
    tactical_subscription_handle: Option<SubscriptionHandle>,
    /// Separate moving spatial `CellState` interest handle (high-scale only).
    spatial_cell_handle: Option<SubscriptionHandle>,
    /// Currently subscribed spatial interest center/radius, if any.
    spatial_interest: Option<SpatialInterest>,
    /// Pending old spatial handle awaiting drop after the replacement applies.
    retiring_spatial_handle: Option<SubscriptionHandle>,
    command_ids_ready: bool,
    active_generation: u64,
    failed_generation: Option<u64>,
    reconnect_attempt: u32,
    reconnect_delay_seconds: f32,
    connection_disabled: bool,
}

impl OnlineTransport {
    fn new(config: ClientConfig) -> Self {
        Self {
            connection: None,
            #[cfg(target_arch = "wasm32")]
            pending_connection: None,
            signals: Arc::default(),
            config,
            coordinate_to_id: BTreeMap::new(),
            id_to_coordinate: BTreeMap::new(),
            tactical: TacticalCache::default(),
            retask_source_counts: BTreeMap::new(),
            retask_destination_claim_counts: BTreeMap::new(),
            retask_edge_counts: BTreeMap::new(),
            retask_orders_by_edge: BTreeMap::new(),
            pending: BTreeMap::new(),
            processed_receipts: BTreeSet::new(),
            terminal_command_ids: BTreeSet::new(),
            next_command_id: session_command_floor(),
            bound_player: None,
            subscription_lifecycle: SubscriptionLifecycle::default(),
            tactical_subscription_player: None,
            tactical_subscription_handle: None,
            spatial_cell_handle: None,
            spatial_interest: None,
            retiring_spatial_handle: None,
            command_ids_ready: false,
            active_generation: 0,
            failed_generation: None,
            reconnect_attempt: 0,
            reconnect_delay_seconds: 0.0,
            connection_disabled: false,
        }
    }

    fn clear_subscription_handles(&mut self) {
        if let Some(handle) = self.tactical_subscription_handle.take() {
            let _ = handle.unsubscribe();
        }
        if let Some(handle) = self.spatial_cell_handle.take() {
            let _ = handle.unsubscribe();
        }
        if let Some(handle) = self.retiring_spatial_handle.take() {
            let _ = handle.unsubscribe();
        }
        self.spatial_interest = None;
        self.tactical_subscription_player = None;
    }

    fn allocate_command_id(&mut self) -> Option<u64> {
        let command_id = self.next_command_id;
        self.next_command_id = command_id.checked_add(1)?;
        Some(command_id)
    }

    fn observe_command_id(&mut self, command_id: u64) {
        let Some(next) = command_id.checked_add(1) else {
            self.next_command_id = u64::MAX;
            self.command_ids_ready = false;
            return;
        };
        self.next_command_id = self.next_command_id.max(next);
    }

    fn schedule_reconnect(&mut self) {
        const INITIAL_DELAY_SECONDS: f32 = 0.5;
        const MAX_EXPONENT: u32 = 4;

        let exponent = self.reconnect_attempt.min(MAX_EXPONENT);
        self.reconnect_delay_seconds = INITIAL_DELAY_SECONDS * 2_u32.pow(exponent) as f32;
        self.reconnect_attempt = self.reconnect_attempt.saturating_add(1);
    }
}

fn session_command_floor() -> u64 {
    const PROCESS_BITS: u32 = 20;
    const PROCESS_MASK: u64 = (1_u64 << PROCESS_BITS) - 1;

    #[cfg(not(target_arch = "wasm32"))]
    let milliseconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX >> PROCESS_BITS)) as u64;
    #[cfg(target_arch = "wasm32")]
    let milliseconds = (js_sys::Date::now() as u64).min(u64::MAX >> PROCESS_BITS);

    #[cfg(not(target_arch = "wasm32"))]
    let session_nonce = u64::from(process::id()) & PROCESS_MASK;
    #[cfg(target_arch = "wasm32")]
    let session_nonce = (js_sys::Math::random() * PROCESS_MASK as f64) as u64;

    (milliseconds << PROCESS_BITS) | session_nonce
}

#[cfg(target_arch = "wasm32")]
type PendingConnection = Arc<Mutex<Option<Result<DbConnection, String>>>>;

pub struct OnlineTransportPlugin;

impl Plugin for OnlineTransportPlugin {
    fn build(&self, app: &mut App) {
        let config = app.world().resource::<ClientConfig>().clone();
        app.insert_resource(OnlineTransport::new(config))
            .add_systems(
                Update,
                (
                    maintain_connection,
                    maintain_moving_viewport_interest,
                    send_online_intents,
                    frame_tick,
                )
                    .chain()
                    .in_set(NetworkSet::Transport),
            )
            .add_systems(
                Update,
                synchronize_authoritative_view
                    .in_set(NetworkSet::Apply)
                    .in_set(OnlineSyncSet),
            );
    }
}

fn maintain_connection(
    time: Res<Time>,
    mut transport: ResMut<OnlineTransport>,
    mut view: ResMut<MatchView>,
) {
    if transport.connection.is_some() || transport.connection_disabled {
        return;
    }
    #[cfg(target_arch = "wasm32")]
    if transport.pending_connection.is_some() {
        poll_pending_connection(&mut transport, &mut view);
        return;
    }
    transport.reconnect_delay_seconds =
        (transport.reconnect_delay_seconds - time.delta_secs()).max(0.0);
    if transport.reconnect_delay_seconds > 0.0 {
        return;
    }
    connect_to_spacetimedb(&mut transport, &mut view);
}

fn connect_to_spacetimedb(transport: &mut OnlineTransport, view: &mut MatchView) {
    let host = match transport
        .config
        .host
        .parse::<spacetimedb_sdk::__codegen::http::Uri>()
    {
        Ok(host)
            if matches!(host.scheme_str(), Some("http" | "https" | "ws" | "wss"))
                && host.authority().is_some() =>
        {
            host
        }
        Ok(_) => {
            disable_invalid_host(
                transport,
                view,
                "expected an absolute http, https, ws, or wss URI",
            );
            return;
        }
        Err(error) => {
            disable_invalid_host(transport, view, &error.to_string());
            return;
        }
    };

    transport.active_generation = transport.active_generation.wrapping_add(1).max(1);
    let generation = transport.active_generation;
    transport.failed_generation = None;
    transport.subscription_lifecycle = SubscriptionLifecycle::reset();
    transport.clear_subscription_handles();
    transport.command_ids_ready = false;
    transport.signals.clear_pending_deltas();
    transport.tactical = TacticalCache::default();
    clear_tactical_presentation(view);
    view.authority = AuthorityState::Connecting;

    let token = match load_token(&transport.config) {
        Ok(token) => token,
        Err(error) => {
            view.push_log(format!(
                "Could not read auth token from {}: {error}",
                credential_store_label(&transport.config)
            ));
            None
        }
    };

    let signals = Arc::clone(&transport.signals);
    let connect_signals = Arc::clone(&signals);
    let connect_error_signals = Arc::clone(&signals);
    let disconnect_signals = Arc::clone(&signals);
    let credential_config = transport.config.clone();
    let preferred_player = transport.config.preferred_player;
    let display_name = transport.config.display_name.clone();

    let connection = DbConnection::builder()
        .with_uri(host)
        .with_database_name(transport.config.database.clone())
        .with_token(token)
        .on_connect(move |connection, _identity, private_token| {
            if let Err(error) = save_token(&credential_config, private_token) {
                connect_signals.push(LifecycleEvent::TokenWarning {
                    generation,
                    message: format!("Could not persist auth token: {error}"),
                });
            }
            connect_signals.push(LifecycleEvent::Connected { generation });

            let applied_signals = Arc::clone(&connect_signals);
            let subscription_error_signals = Arc::clone(&connect_signals);
            connection
                .subscription_builder()
                .on_applied(move |context| {
                    applied_signals.mark(ALL_DIRTY);
                    applied_signals.push(LifecycleEvent::BootstrapSubscribed { generation });
                    let join_signals = Arc::clone(&applied_signals);
                    if let Err(error) = context.reducers.join_match_then(
                        preferred_player,
                        display_name,
                        move |_context, result| {
                            if let Some(reason) = reducer_failure(result) {
                                join_signals
                                    .push(LifecycleEvent::JoinFailed { generation, reason });
                            }
                        },
                    ) {
                        applied_signals.push(LifecycleEvent::JoinFailed {
                            generation,
                            reason: error.to_string(),
                        });
                    }
                })
                .on_error(move |_context, error| {
                    subscription_error_signals.push(LifecycleEvent::ConnectionFailed {
                        generation,
                        reason: format!("bootstrap subscription failed: {error}"),
                    });
                })
                .subscribe(bootstrap_subscription_queries());
        })
        .on_connect_error(move |_context, error| {
            connect_error_signals.push(LifecycleEvent::ConnectionFailed {
                generation,
                reason: error.to_string(),
            });
        })
        .on_disconnect(move |_context, error| {
            let reason =
                error.map_or_else(|| "connection closed".to_owned(), |error| error.to_string());
            disconnect_signals.push(LifecycleEvent::Disconnected { generation, reason });
        });

    #[cfg(not(target_arch = "wasm32"))]
    finish_connection_setup(
        transport,
        view,
        connection.build().map_err(|error| error.to_string()),
    );

    #[cfg(target_arch = "wasm32")]
    {
        let pending: PendingConnection = Arc::new(Mutex::new(None));
        let task_result = Arc::clone(&pending);
        wasm_bindgen_futures::spawn_local(async move {
            let result = connection.build().await.map_err(|error| error.to_string());
            if let Ok(mut slot) = task_result.lock() {
                *slot = Some(result);
            }
        });
        transport.pending_connection = Some(pending);
        set_connecting_status(transport, view);
    }
}

#[cfg(target_arch = "wasm32")]
fn poll_pending_connection(transport: &mut OnlineTransport, view: &mut MatchView) {
    let Some(pending) = transport.pending_connection.clone() else {
        return;
    };
    let result = pending.lock().ok().and_then(|mut slot| slot.take());
    if let Some(result) = result {
        transport.pending_connection = None;
        finish_connection_setup(transport, view, result);
    }
}

fn finish_connection_setup(
    transport: &mut OnlineTransport,
    view: &mut MatchView,
    result: Result<DbConnection, String>,
) {
    match result {
        Ok(connection) => {
            register_table_watchers(&connection, &transport.signals);
            transport.connection = Some(connection);
            set_connecting_status(transport, view);
        }
        Err(error) => {
            transport.schedule_reconnect();
            view.push_log(format!(
                "Connection setup failed: {error} · retrying in {:.1}s",
                transport.reconnect_delay_seconds
            ));
            view.show_toast("SpacetimeDB connection failed", ToastKind::Rejection);
        }
    }
}

fn set_connecting_status(transport: &OnlineTransport, view: &mut MatchView) {
    view.latest_result = format!(
        "Connecting to {} / {} as {}…",
        transport.config.host, transport.config.database, transport.config.display_name
    );
}

fn disable_invalid_host(transport: &mut OnlineTransport, view: &mut MatchView, reason: &str) {
    transport.connection_disabled = true;
    transport.subscription_lifecycle = SubscriptionLifecycle::default();
    transport.clear_subscription_handles();
    transport.command_ids_ready = false;
    clear_tactical_presentation(view);
    view.authority = AuthorityState::ConnectionUnavailable {
        reason: format!("invalid host: {reason}"),
    };
    "Invalid SpacetimeDB host · restart with a valid --host".clone_into(&mut view.latest_result);
    view.push_log(format!(
        "Invalid SpacetimeDB host {:?}: {reason}",
        transport.config.host
    ));
    view.show_toast("Invalid SpacetimeDB host", ToastKind::Rejection);
}

fn register_table_watchers(connection: &DbConnection, signals: &Arc<SharedSignals>) {
    macro_rules! watch_table {
        ($table:expr, $bits:expr) => {{
            let insert_signals = Arc::clone(signals);
            $table.on_insert(move |_context, _row| insert_signals.mark($bits));
            let delete_signals = Arc::clone(signals);
            $table.on_delete(move |_context, _row| delete_signals.mark($bits));
            let update_signals = Arc::clone(signals);
            $table.on_update(move |_context, _old, _new| update_signals.mark($bits));
        }};
    }

    watch_table!(connection.db.cell_terrain(), TERRAIN_DIRTY | CELLS_DIRTY);
    let cell_insert_signals = Arc::clone(signals);
    connection
        .db
        .cell_state()
        .on_insert(move |_context, row| cell_insert_signals.record_cell(row));
    let cell_update_signals = Arc::clone(signals);
    connection
        .db
        .cell_state()
        .on_update(move |_context, _old, new| cell_update_signals.record_cell(new));
    let cell_delete_signals = Arc::clone(signals);
    connection.db.cell_state().on_delete(move |_context, row| {
        // Interest leave (moving viewport) and lobby map replacement both delete
        // rows from the client cache. Project absence to neutral/default so stale
        // remote state cannot linger when the spatial handle moves.
        cell_delete_signals.record_cell_absence(row.cell_id);
    });
    watch_table!(connection.db.match_config(), MATCH_DIRTY);
    watch_table!(connection.db.match_state(), MATCH_DIRTY);
    watch_table!(connection.db.player_slot(), PLAYERS_DIRTY);
    watch_table!(connection.db.player_state(), MATCH_DIRTY);
    watch_table!(connection.db.mobilization_policy(), MOBILIZATION_DIRTY);
    watch_table!(connection.db.command_receipt(), RECEIPTS_DIRTY);
    {
        let packet_insert_signals = Arc::clone(signals);
        connection
            .db
            .transit_packet()
            .on_insert(move |_context, row| {
                record_change(
                    &packet_insert_signals.packet_changes,
                    row.packet_key,
                    Some(row.clone()),
                );
                packet_insert_signals.mark(FLOWS_DIRTY);
            });
        let packet_delete_signals = Arc::clone(signals);
        connection
            .db
            .transit_packet()
            .on_delete(move |_context, row| {
                record_change(&packet_delete_signals.packet_changes, row.packet_key, None);
                packet_delete_signals.mark(FLOWS_DIRTY);
            });
        let packet_update_signals = Arc::clone(signals);
        connection
            .db
            .transit_packet()
            .on_update(move |_context, _old, new| {
                record_change(
                    &packet_update_signals.packet_changes,
                    new.packet_key,
                    Some(new.clone()),
                );
                packet_update_signals.mark(FLOWS_DIRTY);
            });
    }
    {
        let route_insert_signals = Arc::clone(signals);
        connection
            .db
            .transit_route()
            .on_insert(move |_context, row| {
                record_change(
                    &route_insert_signals.route_changes,
                    row.route_id,
                    Some(row.clone()),
                );
                route_insert_signals.mark(FLOWS_DIRTY);
            });
        let route_delete_signals = Arc::clone(signals);
        connection
            .db
            .transit_route()
            .on_delete(move |_context, row| {
                record_change(&route_delete_signals.route_changes, row.route_id, None);
                route_delete_signals.mark(FLOWS_DIRTY);
            });
        let route_update_signals = Arc::clone(signals);
        connection
            .db
            .transit_route()
            .on_update(move |_context, _old, new| {
                record_change(
                    &route_update_signals.route_changes,
                    new.route_id,
                    Some(new.clone()),
                );
                route_update_signals.mark(FLOWS_DIRTY);
            });
    }
    let front_insert_signals = Arc::clone(signals);
    connection
        .db
        .combat_front()
        .on_insert(move |_context, row| {
            record_change(
                &front_insert_signals.front_changes,
                row.front_key.clone(),
                Some(row.clone()),
            );
            front_insert_signals.mark(FRONTS_DIRTY);
        });
    let front_delete_signals = Arc::clone(signals);
    connection
        .db
        .combat_front()
        .on_delete(move |_context, row| {
            record_change(
                &front_delete_signals.front_changes,
                row.front_key.clone(),
                None,
            );
            front_delete_signals.mark(FRONTS_DIRTY);
        });
    let front_update_signals = Arc::clone(signals);
    connection
        .db
        .combat_front()
        .on_update(move |_context, _old, new| {
            record_change(
                &front_update_signals.front_changes,
                new.front_key.clone(),
                Some(new.clone()),
            );
            front_update_signals.mark(FRONTS_DIRTY);
        });

    let order_insert_signals = Arc::clone(signals);
    connection
        .db
        .transfer_order()
        .on_insert(move |_context, row| {
            let value = (row.status == OrderStatus::Active).then(|| row.clone());
            record_change(&order_insert_signals.order_changes, row.order_id, value);
            order_insert_signals.mark(ORDERS_DIRTY);
        });
    let order_delete_signals = Arc::clone(signals);
    connection
        .db
        .transfer_order()
        .on_delete(move |_context, row| {
            record_change(&order_delete_signals.order_changes, row.order_id, None);
            order_delete_signals.mark(ORDERS_DIRTY);
        });
    let order_update_signals = Arc::clone(signals);
    connection
        .db
        .transfer_order()
        .on_update(move |_context, _old, new| {
            let value = (new.status == OrderStatus::Active).then(|| new.clone());
            record_change(&order_update_signals.order_changes, new.order_id, value);
            order_update_signals.mark(ORDERS_DIRTY);
        });

    let source_insert_signals = Arc::clone(signals);
    connection
        .db
        .transfer_source()
        .on_insert(move |_context, row| {
            record_change(
                &source_insert_signals.source_changes,
                row.source_key,
                Some(row.clone()),
            );
            source_insert_signals.mark(ORDERS_DIRTY);
        });
    let source_delete_signals = Arc::clone(signals);
    connection
        .db
        .transfer_source()
        .on_delete(move |_context, row| {
            record_change(&source_delete_signals.source_changes, row.source_key, None);
            source_delete_signals.mark(ORDERS_DIRTY);
        });
    let source_update_signals = Arc::clone(signals);
    connection
        .db
        .transfer_source()
        .on_update(move |_context, _old, new| {
            record_change(
                &source_update_signals.source_changes,
                new.source_key,
                Some(new.clone()),
            );
            source_update_signals.mark(ORDERS_DIRTY);
        });

    let destination_insert_signals = Arc::clone(signals);
    connection
        .db
        .transfer_destination()
        .on_insert(move |_context, row| {
            record_change(
                &destination_insert_signals.destination_changes,
                row.destination_key,
                Some(row.clone()),
            );
            destination_insert_signals.mark(ORDERS_DIRTY);
        });
    let destination_delete_signals = Arc::clone(signals);
    connection
        .db
        .transfer_destination()
        .on_delete(move |_context, row| {
            record_change(
                &destination_delete_signals.destination_changes,
                row.destination_key,
                None,
            );
            destination_delete_signals.mark(ORDERS_DIRTY);
        });
    let destination_update_signals = Arc::clone(signals);
    connection
        .db
        .transfer_destination()
        .on_update(move |_context, _old, new| {
            record_change(
                &destination_update_signals.destination_changes,
                new.destination_key,
                Some(new.clone()),
            );
            destination_update_signals.mark(ORDERS_DIRTY);
        });
}

fn frame_tick(transport: Res<OnlineTransport>) {
    let Some(connection) = &transport.connection else {
        return;
    };
    if let Err(error) = connection.frame_tick() {
        transport.signals.push(LifecycleEvent::Disconnected {
            generation: transport.active_generation,
            reason: error.to_string(),
        });
    }
}

fn send_online_intents(
    mut intents: MessageReader<ClientIntent>,
    view: Res<MatchView>,
    mut transport: ResMut<OnlineTransport>,
    mut updates: MessageWriter<ServerUpdate>,
) {
    for intent in intents.read() {
        if !transport.subscription_lifecycle.commands_ready() || !transport.command_ids_ready {
            updates.write(ServerUpdate::Rejected {
                command_id: None,
                reason: view.authority.command_block_reason(),
                relevant_cell: None,
            });
            continue;
        }
        let Some(command_id) = transport.allocate_command_id() else {
            updates.write(ServerUpdate::Rejected {
                command_id: None,
                reason: "Client command ID space is exhausted".to_owned(),
                relevant_cell: None,
            });
            continue;
        };
        if !matches!(intent, ClientIntent::SetMobilization { .. }) {
            updates.write(ServerUpdate::SubmissionStarted { command_id });
        }

        let result = invoke_intent(&transport, &view, command_id, intent);
        match result {
            Ok(kind) => {
                transport.pending.insert(command_id, kind);
            }
            Err(reason) => {
                transport.terminal_command_ids.insert(command_id);
                updates.write(ServerUpdate::Rejected {
                    command_id: Some(command_id),
                    reason,
                    relevant_cell: None,
                });
            }
        }
    }
}

fn invoke_intent(
    transport: &OnlineTransport,
    _view: &MatchView,
    command_id: u64,
    intent: &ClientIntent,
) -> Result<PendingCommand, String> {
    let connection = transport
        .connection
        .as_ref()
        .ok_or_else(|| "SpacetimeDB connection is unavailable".to_owned())?;
    let signals = Arc::clone(&transport.signals);
    let callback = move |_context: &match_bindings::ReducerEventContext, result| {
        if let Some(reason) = reducer_failure(result) {
            signals.push(LifecycleEvent::CommandFailed { command_id, reason });
        }
    };

    match intent {
        ClientIntent::ExpandClusters {
            sources,
            focus,
            commitment_percent,
        } => {
            connection
                .reducers
                .issue_expand_clusters_then(
                    command_id,
                    ids_for_selection(transport, sources)?,
                    id_for_coordinate(transport, *focus)?,
                    u32::from((*commitment_percent).clamp(1, 100)) * 100,
                    callback,
                )
                .map_err(|error| error.to_string())?;
            Ok(PendingCommand::ExpandClusters)
        }
        ClientIntent::AttackClusters {
            sources,
            targets,
            commitment_percent,
        } => {
            connection
                .reducers
                .issue_attack_clusters_then(
                    command_id,
                    ids_for_selection(transport, sources)?,
                    ids_for_selection(transport, targets)?,
                    u32::from((*commitment_percent).clamp(1, 100)) * 100,
                    callback,
                )
                .map_err(|error| error.to_string())?;
            Ok(PendingCommand::AttackClusters)
        }
        ClientIntent::PushFront {
            sources,
            supersede_order_ids,
            direction,
            commitment_percent,
        } => {
            connection
                .reducers
                .issue_push_front_then(
                    command_id,
                    ids_for_selection(transport, sources)?,
                    direction.q,
                    direction.r,
                    u32::from((*commitment_percent).clamp(1, 100)) * 100,
                    supersede_order_ids.iter().copied().collect(),
                    callback,
                )
                .map_err(|error| error.to_string())?;
            Ok(PendingCommand::PushFront)
        }
        ClientIntent::FrontRebalance {
            source_component_cells,
            source_front_seed,
            target_front_seed,
            commitment_percent,
            supersede_order_ids,
        } => {
            connection
                .reducers
                .issue_front_rebalance_then(
                    command_id,
                    ids_for_selection(transport, source_component_cells)?,
                    id_for_coordinate(transport, *source_front_seed)?,
                    id_for_coordinate(transport, *target_front_seed)?,
                    u32::from((*commitment_percent).clamp(1, 100)) * 100,
                    supersede_order_ids.iter().copied().collect(),
                    callback,
                )
                .map_err(|error| error.to_string())?;
            Ok(PendingCommand::FrontRebalance)
        }
        ClientIntent::ExpandAll {
            sources,
            supersede_order_ids,
            commitment_percent,
        } => {
            connection
                .reducers
                .issue_expand_all_then(
                    command_id,
                    ids_for_selection(transport, sources)?,
                    u32::from((*commitment_percent).clamp(1, 100)) * 100,
                    supersede_order_ids.iter().copied().collect(),
                    callback,
                )
                .map_err(|error| error.to_string())?;
            Ok(PendingCommand::ExpandAll)
        }
        ClientIntent::Reshape {
            sources,
            targets,
            supersede_order_ids,
        } => {
            connection
                .reducers
                .issue_reshape_then(
                    command_id,
                    ids_for_selection(transport, sources)?,
                    ids_for_selection(transport, targets)?,
                    supersede_order_ids.iter().copied().collect(),
                    callback,
                )
                .map_err(|error| error.to_string())?;
            Ok(PendingCommand::Reshape)
        }
        ClientIntent::CancelOrders { order_ids } => {
            connection
                .reducers
                .cancel_orders_then(command_id, order_ids.iter().copied().collect(), callback)
                .map_err(|error| error.to_string())?;
            Ok(PendingCommand::CancelOrders)
        }
        ClientIntent::SetMobilization { target } => {
            let target = target.clamp(0.0, 1.0);
            let target_bps = (target * 10_000.0).round() as u32;
            connection
                .reducers
                .set_mobilization_target_then(command_id, target_bps, callback)
                .map_err(|error| error.to_string())?;
            Ok(PendingCommand::Mobilization { target })
        }
    }
}

fn ids_for_selection(
    transport: &OnlineTransport,
    selection: &BTreeSet<Axial>,
) -> Result<Vec<u32>, String> {
    selection
        .iter()
        .map(|coordinate| {
            transport
                .coordinate_to_id
                .get(coordinate)
                .copied()
                .ok_or_else(|| {
                    format!(
                        "hex {},{} is not present in the authoritative map",
                        coordinate.q, coordinate.r
                    )
                })
        })
        .collect()
}

fn id_for_coordinate(transport: &OnlineTransport, coordinate: Axial) -> Result<u32, String> {
    transport
        .coordinate_to_id
        .get(&coordinate)
        .copied()
        .ok_or_else(|| {
            format!(
                "hex {},{} is not present in the authoritative map",
                coordinate.q, coordinate.r
            )
        })
}

struct AuthoritySnapshot {
    identity: Option<spacetimedb_sdk::Identity>,
    terrain: Option<Vec<CellTerrain>>,
    cells: Option<Vec<CellState>>,
    config: Option<MatchConfig>,
    match_state: Option<MatchState>,
    player_states: Option<Vec<PlayerState>>,
    players: Option<Vec<PlayerSlot>>,
    mobilization: Option<Vec<MobilizationPolicy>>,
    receipts: Option<Vec<CommandReceipt>>,
    tactical: Option<TacticalCache>,
}

impl AuthoritySnapshot {
    fn capture(connection: &DbConnection, dirty: u32, changed_cells: Vec<CellState>) -> Self {
        let terrain_changed = dirty & TERRAIN_DIRTY != 0;
        Self {
            identity: connection.try_identity(),
            terrain: terrain_changed.then(|| connection.db.cell_terrain().iter().collect()),
            cells: if terrain_changed {
                Some(connection.db.cell_state().iter().collect())
            } else {
                (dirty & CELLS_DIRTY != 0).then_some(changed_cells)
            },
            config: (dirty & MATCH_DIRTY != 0)
                .then(|| connection.db.match_config().iter().next())
                .flatten(),
            match_state: (dirty & MATCH_DIRTY != 0)
                .then(|| connection.db.match_state().iter().next())
                .flatten(),
            player_states: (dirty & MATCH_DIRTY != 0)
                .then(|| connection.db.player_state().iter().collect()),
            players: (dirty & PLAYERS_DIRTY != 0)
                .then(|| connection.db.player_slot().iter().collect()),
            mobilization: (dirty & MOBILIZATION_DIRTY != 0)
                .then(|| connection.db.mobilization_policy().iter().collect()),
            receipts: (dirty & (RECEIPTS_DIRTY | PLAYERS_DIRTY) != 0)
                .then(|| connection.db.command_receipt().iter().collect()),
            // A topology replacement is the only full tactical-table capture.
            // Ordinary row callbacks feed the persistent cache incrementally.
            tactical: terrain_changed.then(|| TacticalCache::capture(connection)),
        }
    }
}

fn subscribed_packets(connection: &DbConnection) -> Vec<TransitPacket> {
    connection.db.transit_packet().iter().collect()
}

fn subscribed_routes(connection: &DbConnection) -> Vec<TransitRoute> {
    connection.db.transit_route().iter().collect()
}

#[derive(Default)]
struct ProjectionImpact {
    flow_packet_ids: BTreeSet<u64>,
    retask_packet_ids: BTreeSet<u64>,
    retask_source_keys: BTreeSet<u128>,
    retask_destination_keys: BTreeSet<u128>,
    route_ids: BTreeSet<u64>,
    structural_order_ids: BTreeSet<u64>,
    fronts_changed: bool,
}

impl ProjectionImpact {
    fn before(cache: &TacticalCache, changes: &TacticalChanges) -> Self {
        let mut impact = Self {
            flow_packet_ids: changes.packets.keys().copied().collect(),
            retask_packet_ids: changes.packets.keys().copied().collect(),
            retask_source_keys: changes.sources.keys().copied().collect(),
            retask_destination_keys: changes.destinations.keys().copied().collect(),
            route_ids: changes.routes.keys().copied().collect(),
            fronts_changed: !changes.fronts.is_empty(),
            ..Default::default()
        };
        for route_id in &impact.route_ids {
            if let Some(packet_ids) = cache.packet_ids_by_route.get(route_id) {
                impact.flow_packet_ids.extend(packet_ids);
                impact.retask_packet_ids.extend(packet_ids);
            }
        }
        for (order_id, next) in &changes.orders {
            let previous = cache.orders.get(order_id);
            if flow_order_projection_changed(previous, next.as_ref())
                && let Some(packet_ids) = cache.packet_ids_by_order.get(order_id)
            {
                impact.flow_packet_ids.extend(packet_ids);
            }
            if retask_order_projection_changed(previous, next.as_ref()) {
                impact.structural_order_ids.insert(*order_id);
                impact.extend_order_rows(cache, *order_id);
            }
        }
        impact
    }

    fn after(&mut self, cache: &TacticalCache) {
        for route_id in &self.route_ids {
            if let Some(packet_ids) = cache.packet_ids_by_route.get(route_id) {
                self.flow_packet_ids.extend(packet_ids);
                self.retask_packet_ids.extend(packet_ids);
            }
        }
        for order_id in self.structural_order_ids.clone() {
            if let Some(packet_ids) = cache.packet_ids_by_order.get(&order_id) {
                self.flow_packet_ids.extend(packet_ids);
            }
            self.extend_order_rows(cache, order_id);
        }
    }

    fn extend_order_rows(&mut self, cache: &TacticalCache, order_id: u64) {
        if let Some(packet_ids) = cache.packet_ids_by_order.get(&order_id) {
            self.retask_packet_ids.extend(packet_ids);
        }
        if let Some(source_keys) = cache.source_keys_by_order.get(&order_id) {
            self.retask_source_keys.extend(source_keys);
        }
        if let Some(destination_keys) = cache.destination_keys_by_order.get(&order_id) {
            self.retask_destination_keys.extend(destination_keys);
        }
    }

    fn retask_changed(&self) -> bool {
        self.fronts_changed
            || !self.structural_order_ids.is_empty()
            || !self.retask_packet_ids.is_empty()
            || !self.retask_source_keys.is_empty()
            || !self.retask_destination_keys.is_empty()
    }
}

fn flow_order_projection_changed(
    previous: Option<&TransferOrder>,
    next: Option<&TransferOrder>,
) -> bool {
    match (previous, next) {
        (Some(previous), Some(next)) => {
            previous.status != next.status || previous.kind != next.kind
        }
        (None, None) => false,
        _ => true,
    }
}

fn retask_order_projection_changed(
    previous: Option<&TransferOrder>,
    next: Option<&TransferOrder>,
) -> bool {
    match (previous, next) {
        (Some(previous), Some(next)) => {
            previous.status != next.status
                || previous.player_id != next.player_id
                || previous.kind != next.kind
                || previous.client_command_id != next.client_command_id
        }
        (None, None) => false,
        _ => true,
    }
}

fn synchronize_authoritative_view(
    mut transport: ResMut<OnlineTransport>,
    mut view: ResMut<MatchView>,
    mode: Res<MapViewMode>,
    mut updates: MessageWriter<ServerUpdate>,
) {
    let previous_local_player = view.local_player;
    for event in transport.signals.drain() {
        apply_lifecycle_event(&mut transport, &mut view, &mut updates, event);
    }

    let dirty = transport.signals.take_dirty();
    if dirty == 0 {
        return;
    }
    let cell_absences = transport.signals.take_cell_absences();
    let snapshot = {
        let Some(connection) = &transport.connection else {
            return;
        };
        AuthoritySnapshot::capture(connection, dirty, transport.signals.take_cell_changes())
    };
    let tactical_changes = transport.signals.take_tactical_changes();
    let full_tactical_rebuild = snapshot.tactical.is_some();
    let mut projection_impact = ProjectionImpact::before(&transport.tactical, &tactical_changes);
    if !full_tactical_rebuild {
        apply_retask_row_changes(&mut transport, &mut view, &projection_impact, false);
    }
    if let Some(tactical) = snapshot.tactical {
        transport.tactical = tactical;
    }
    // Deltas may arrive between the full capture and this drain. Applying
    // them unconditionally closes that race without another table scan.
    transport.tactical.apply(tactical_changes);
    projection_impact.after(&transport.tactical);

    if let Some(terrain) = snapshot.terrain {
        rebuild_cells(
            &mut transport,
            &mut view,
            terrain,
            snapshot.cells.as_deref(),
        );
    } else {
        if let Some(cells) = snapshot.cells {
            update_cells(&transport, &mut view, &cells, *mode);
        }
        if !cell_absences.is_empty() {
            neutralize_absent_cells(&transport, &mut view, &cell_absences, *mode);
        }
    }

    if let Some(config) = snapshot.config {
        view.player_count = config.player_count;
        view.connection
            .resize(usize::from(config.player_count), ConnectionState::Syncing);
        view.conquest_threshold_bps = config.conquest_threshold_bps;
        view.max_elevation_step = u16::from(config.max_elevation_step);
        // Config may arrive after the local seat bind in a later dirty batch;
        // issue the single tactical subscription only once both are known.
        if let Some(local_player) = transport.bound_player {
            ensure_tactical_subscriptions(&mut transport, config.player_count, local_player);
        }
    }
    if let Some(state) = snapshot.match_state {
        view.phase = match state.phase {
            RemoteMatchPhase::Lobby => MatchPhase::Lobby,
            RemoteMatchPhase::Running => MatchPhase::Running,
            RemoteMatchPhase::Completed => MatchPhase::Victory(u32::from(state.winner_player_id)),
        };
        view.logical_step = state.logical_step;
        view.capturable_cells = state.capturable_cells;
        view.required_control = state.required_control;
        view.claimed_players = state.claimed_players;
    }
    if let Some(player_states) = snapshot.player_states {
        view.authoritative_control = Some(
            player_states
                .into_iter()
                .map(|state| (u32::from(state.player_id), state.controlled_cells))
                .collect(),
        );
    }
    if let Some(players) = snapshot.players {
        update_players(&mut transport, &mut view, snapshot.identity, &players);
    }
    let full_retask_rebuild = full_tactical_rebuild || view.local_player != previous_local_player;
    if let Some(policies) = snapshot.mobilization
        && let Some(policy) = policies
            .iter()
            .find(|policy| u32::from(policy.player_id) == view.local_player)
    {
        view.mobilization_target = policy.target_bps as f32 / 10_000.0;
    }

    if !transport.command_ids_ready
        && transport.subscription_lifecycle.commands_ready()
        && snapshot.identity.is_some()
        && let Some(receipts) = snapshot.receipts.as_deref()
        && transport.bound_player.is_some()
    {
        seed_command_ids(&mut transport, view.local_player, receipts);
    }
    if let Some(receipts) = snapshot.receipts {
        process_receipts(&mut transport, &view, receipts, &mut updates);
    }
    if full_tactical_rebuild {
        replace_authoritative_flows(
            &transport,
            &mut view,
            transport.tactical.packets.values(),
            &transport.tactical.routes,
            transport.tactical.orders.values(),
        );
    } else if !projection_impact.flow_packet_ids.is_empty() {
        update_authoritative_flows(&transport, &mut view, &projection_impact.flow_packet_ids);
    }
    if dirty & (TERRAIN_DIRTY | FRONTS_DIRTY) != 0 {
        let (active_fronts, contested_cells) =
            fronts_to_overlays(&transport, &view, transport.tactical.fronts.values());
        view.active_fronts = active_fronts;
        view.set_contested_cells(contested_cells);
    }
    if full_retask_rebuild {
        rebuild_retask_indexes(&mut transport, &mut view);
    } else if projection_impact.retask_changed() {
        apply_retask_row_changes(&mut transport, &mut view, &projection_impact, true);
        refresh_retask_order_kinds(&transport, &mut view);
        rebuild_retask_handles(&transport, &mut view);
        view.retask_revision = view.retask_revision.wrapping_add(1);
    }
    if dirty & (TERRAIN_DIRTY | ORDERS_DIRTY) != 0 {
        view.active_orders = transport.tactical.orders.len();
        view.queued_infantry = transport
            .tactical
            .sources
            .values()
            .map(|source| source.queued_infantry)
            .sum();
    }
}

fn apply_lifecycle_event(
    transport: &mut OnlineTransport,
    view: &mut MatchView,
    updates: &mut MessageWriter<ServerUpdate>,
    event: LifecycleEvent,
) {
    match event {
        LifecycleEvent::Connected { generation } if lifecycle_is_current(transport, generation) => {
            view.authority = AuthorityState::Connecting;
            "Connected · subscribing to authoritative tables…".clone_into(&mut view.latest_result);
        }
        LifecycleEvent::BootstrapSubscribed { generation }
            if lifecycle_is_current(transport, generation) =>
        {
            transport.reconnect_attempt = 0;
            transport.reconnect_delay_seconds = 0.0;
            transport.subscription_lifecycle =
                transport.subscription_lifecycle.on_bootstrap_applied();
            view.authority = AuthorityState::Connecting;
            "Authoritative lobby snapshot applied · joining match…"
                .clone_into(&mut view.latest_result);
        }
        LifecycleEvent::TacticalSubscribed { generation }
            if lifecycle_is_current(transport, generation) =>
        {
            transport.subscription_lifecycle =
                transport.subscription_lifecycle.on_tactical_applied();
            if transport.bound_player.is_some() {
                view.authority = AuthorityState::Ready;
                "Authoritative controls ready · tactical subscription applied"
                    .clone_into(&mut view.latest_result);
            }
        }
        LifecycleEvent::JoinFailed { generation, reason }
            if lifecycle_is_current(transport, generation) =>
        {
            mark_join_failed(transport, view, reason);
        }
        LifecycleEvent::ConnectionFailed { generation, reason } => {
            handle_connection_loss(
                transport,
                view,
                updates,
                generation,
                format!("Connection failed: {reason}"),
            );
        }
        LifecycleEvent::Disconnected { generation, reason } => {
            handle_connection_loss(
                transport,
                view,
                updates,
                generation,
                format!("Disconnected: {reason}"),
            );
        }
        LifecycleEvent::CommandFailed { command_id, reason } => {
            if transport.terminal_command_ids.insert(command_id) {
                let pending = transport.pending.remove(&command_id);
                if pending.is_some() {
                    updates.write(ServerUpdate::Rejected {
                        command_id: Some(command_id),
                        reason,
                        relevant_cell: None,
                    });
                }
            }
        }
        LifecycleEvent::TokenWarning {
            generation,
            message,
        } if generation == transport.active_generation => view.push_log(message),
        LifecycleEvent::Connected { .. }
        | LifecycleEvent::BootstrapSubscribed { .. }
        | LifecycleEvent::TacticalSubscribed { .. }
        | LifecycleEvent::JoinFailed { .. }
        | LifecycleEvent::TokenWarning { .. } => {}
    }
}

fn mark_join_failed(transport: &mut OnlineTransport, view: &mut MatchView, reason: String) {
    transport.bound_player = None;
    transport.clear_subscription_handles();
    // Keep bootstrap readiness; the lobby snapshot is still valid. Clear only
    // tactical readiness so a later successful seat claim re-subscribes.
    transport.subscription_lifecycle.tactical_ready = false;
    transport.command_ids_ready = false;
    clear_tactical_presentation(view);
    view.authority = AuthorityState::SlotUnavailable {
        reason: reason.clone(),
    };
    let authority_label = view.authority.label();
    view.latest_result = format!("{authority_label} · {reason}");
    view.push_log(view.latest_result.clone());
    view.show_toast(
        format!("Player slot unavailable: {reason}"),
        ToastKind::Rejection,
    );
}

fn lifecycle_is_current(transport: &OnlineTransport, generation: u64) -> bool {
    generation == transport.active_generation && transport.failed_generation != Some(generation)
}

fn handle_connection_loss(
    transport: &mut OnlineTransport,
    view: &mut MatchView,
    updates: &mut MessageWriter<ServerUpdate>,
    generation: u64,
    reason: String,
) {
    if generation != transport.active_generation
        || transport.failed_generation.replace(generation) == Some(generation)
    {
        return;
    }

    transport.subscription_lifecycle = SubscriptionLifecycle::reset();
    transport.command_ids_ready = false;
    transport.bound_player = None;
    transport.clear_subscription_handles();
    transport.signals.clear_pending_deltas();
    transport.tactical = TacticalCache::default();
    clear_tactical_presentation(view);
    view.authority = AuthorityState::Connecting;
    if let Some(connection) = transport.connection.take() {
        let _ = connection.disconnect();
    }
    for (command_id, pending) in std::mem::take(&mut transport.pending) {
        transport.terminal_command_ids.insert(command_id);
        if pending.is_modal() {
            updates.write(ServerUpdate::Rejected {
                command_id: Some(command_id),
                reason: format!(
                    "{} outcome is unknown after connection loss; retry after reconnect",
                    pending.label()
                ),
                relevant_cell: None,
            });
        }
    }

    transport.schedule_reconnect();
    view.connection = vec![ConnectionState::Syncing; usize::from(view.player_count)];
    view.latest_result = format!(
        "Connection lost · retrying in {:.1}s",
        transport.reconnect_delay_seconds
    );
    view.push_log(reason);
    view.show_toast("Disconnected · reconnecting", ToastKind::Rejection);
}

fn rebuild_cells(
    transport: &mut OnlineTransport,
    view: &mut MatchView,
    terrain_rows: Vec<CellTerrain>,
    state_rows: Option<&[CellState]>,
) {
    let states: BTreeMap<_, _> = state_rows
        .unwrap_or_default()
        .iter()
        .map(|state| (state.cell_id, state))
        .collect();
    view.dirty_chunks
        .extend(view.cells_by_chunk.keys().copied());
    view.cells.clear();
    view.non_capturable_cells.clear();
    transport.coordinate_to_id.clear();
    transport.id_to_coordinate.clear();
    for terrain in terrain_rows {
        let coordinate = Axial::new(terrain.q, terrain.r);
        if !terrain.capturable {
            view.non_capturable_cells.insert(coordinate);
        }
        let state = states.get(&terrain.cell_id).copied();
        transport
            .coordinate_to_id
            .insert(coordinate, terrain.cell_id);
        transport
            .id_to_coordinate
            .insert(terrain.cell_id, coordinate);
        view.cells
            .insert(coordinate, cell_view_from_rows(coordinate, &terrain, state));
        view.dirty_chunks.insert(chunk_of(coordinate));
    }
    view.rebuild_chunk_index();
}

fn update_cells(
    transport: &OnlineTransport,
    view: &mut MatchView,
    states: &[CellState],
    mode: MapViewMode,
) {
    if !states.is_empty() {
        view.mark_cell_state_changed();
    }
    let mut planning_changed = false;
    let mut ownership_changed = false;
    for state in states {
        let Some(coordinate) = transport.id_to_coordinate.get(&state.cell_id).copied() else {
            continue;
        };
        let Some(cell) = view.cell(coordinate) else {
            continue;
        };
        let owner_changed = cell.owner != owner(state.owner_player_id);
        ownership_changed |= owner_changed;
        planning_changed |= owner_changed
            || cell.infantry != state.infantry
            || cell.military_capacity != state.military_capacity;
    }
    if planning_changed {
        view.mark_planning_changed();
    }
    if ownership_changed {
        view.mark_ownership_changed();
    }
    for state in states {
        let Some(coordinate) = transport.id_to_coordinate.get(&state.cell_id).copied() else {
            continue;
        };
        let next_owner = owner(state.owner_player_id);
        let rendering_changed = view.cell(coordinate).is_some_and(|cell| {
            cell.owner != next_owner
                || (mode == MapViewMode::Civilians && cell.civilians != state.civilians)
                || (mode == MapViewMode::Soldiers && cell.infantry != state.infantry)
        });
        if !rendering_changed {
            if let Some(cell) = view.cells.get_mut(&coordinate) {
                cell.owner = next_owner;
                cell.civilians = state.civilians;
                cell.infantry = state.infantry;
                cell.military_capacity = state.military_capacity;
            }
            continue;
        }
        view.dirty_chunks.insert(chunk_of(coordinate));
        let Some(cell) = view.cells.get_mut(&coordinate) else {
            continue;
        };
        cell.owner = next_owner;
        cell.civilians = state.civilians;
        cell.infantry = state.infantry;
        cell.military_capacity = state.military_capacity;
    }
}

fn cell_view_from_rows(
    coordinate: Axial,
    terrain: &CellTerrain,
    state: Option<&CellState>,
) -> CellView {
    CellView {
        coordinate,
        terrain: match terrain.terrain {
            match_bindings::TerrainClass::Water => TerrainKind::Water,
            match_bindings::TerrainClass::Plains => TerrainKind::Plains,
            match_bindings::TerrainClass::Hills => TerrainKind::Hills,
            match_bindings::TerrainClass::Mountain => TerrainKind::Mountain,
        },
        elevation: terrain.elevation,
        owner: state.and_then(|state| owner(state.owner_player_id)),
        civilians: state.map_or(0, |state| state.civilians),
        infantry: state.map_or(0, |state| state.infantry),
        military_capacity: state.map_or(0, |state| state.military_capacity),
        blocked: !matches!(terrain.terrain, match_bindings::TerrainClass::Water)
            && !terrain.passable,
    }
}

const fn owner(player_id: u16) -> Option<u32> {
    if player_id == 0 {
        None
    } else {
        Some(player_id as u32)
    }
}

fn update_players(
    transport: &mut OnlineTransport,
    view: &mut MatchView,
    identity: Option<spacetimedb_sdk::Identity>,
    players: &[PlayerSlot],
) {
    view.connection
        .resize(usize::from(view.player_count), ConnectionState::Syncing);
    // Single linear pass over the snapshot; index by player_id instead of a
    // nested scan per configured seat. Missing seats stay Syncing until the
    // authority projects them.
    for state in &mut view.connection {
        *state = ConnectionState::Syncing;
    }
    let mut local_slot = None;
    for slot in players {
        let Some(index) = (slot.player_id as usize).checked_sub(1) else {
            continue;
        };
        if index >= view.connection.len() {
            continue;
        }
        view.connection[index] = if slot.identity.is_none() {
            ConnectionState::Open
        } else if slot.connected {
            ConnectionState::Connected
        } else {
            ConnectionState::ClaimedOffline
        };
        if identity.is_some() && slot.identity.as_ref() == identity.as_ref() {
            local_slot = Some(slot);
        }
    }
    if let Some(slot) = local_slot {
        let local_player = u32::from(slot.player_id);
        if view.local_player != local_player {
            view.local_player = local_player;
            view.mark_planning_changed();
            view.mark_ownership_changed();
        }
        if transport.bound_player != Some(slot.player_id) {
            transport.bound_player = Some(slot.player_id);
            transport.command_ids_ready = false;
            view.authority = AuthorityState::Connecting;
            view.latest_result = format!(
                "Bound to Player {} · subscribing to tactical tables…",
                slot.player_id
            );
            ensure_tactical_subscriptions(transport, view.player_count, slot.player_id);
        }
    }
}

fn ensure_tactical_subscriptions(
    transport: &mut OnlineTransport,
    player_count: u16,
    local_player: u16,
) {
    // Exactly one tactical subscription after the seat and count are known.
    // Bootstrap is metadata/terrain only, so this second handle stacks safely
    // without replacing or duplicating those projections. High-scale spatial
    // CellState interest is a third, movable handle installed separately.
    if transport.tactical_subscription_player == Some(local_player) {
        return;
    }
    let Some(connection) = transport.connection.as_ref() else {
        return;
    };
    let queries = tactical_subscription_queries(player_count, local_player);
    let applied_signals = Arc::clone(&transport.signals);
    let error_signals = Arc::clone(&transport.signals);
    let generation = transport.active_generation;
    let handle = connection
        .subscription_builder()
        .on_applied(move |_| {
            applied_signals.mark(ALL_DIRTY);
            applied_signals.push(LifecycleEvent::TacticalSubscribed { generation });
        })
        .on_error(move |_, error| {
            error_signals.push(LifecycleEvent::ConnectionFailed {
                generation,
                reason: format!("tactical subscription failed: {error}"),
            });
        })
        .subscribe(queries);
    if let Some(previous) = transport.tactical_subscription_handle.replace(handle) {
        let _ = previous.unsubscribe();
    }
    transport.tactical_subscription_player = Some(local_player);
    transport.subscription_lifecycle = transport.subscription_lifecycle.on_tactical_start();

    // Spawn-centered initial spatial interest before camera state is available.
    if player_count > HIGH_SCALE_PLAYER_THRESHOLD {
        let config = connection.db.match_config().iter().next();
        let spawn_cell = connection
            .db
            .player_state()
            .iter()
            .find(|state| state.player_id == local_player)
            .map_or(0, |state| state.spawn_cell_id);
        let (chunk_q, chunk_r) = config.map_or((0, 0), |config| {
            chunk_coords_for_cell_id(spawn_cell, config.map_width, config.chunk_size)
        });
        let interest = SpatialInterest {
            focus_chunk_q: chunk_q,
            focus_chunk_r: chunk_r,
            radius: HIGH_SCALE_INTEREST_CHUNK_RADIUS,
        };
        install_spatial_cell_subscription(transport, interest);
    }
}

fn install_spatial_cell_subscription(transport: &mut OnlineTransport, interest: SpatialInterest) {
    if transport.spatial_interest == Some(interest) {
        return;
    }
    let Some(connection) = transport.connection.as_ref() else {
        return;
    };
    let queries = spatial_cell_state_queries(interest);
    let applied_signals = Arc::clone(&transport.signals);
    let error_signals = Arc::clone(&transport.signals);
    let generation = transport.active_generation;
    let handle = connection
        .subscription_builder()
        .on_applied(move |_| {
            // Spatial interest only carries CellState bandwidth rows.
            applied_signals.mark(CELLS_DIRTY);
        })
        .on_error(move |_, error| {
            error_signals.push(LifecycleEvent::ConnectionFailed {
                generation,
                reason: format!("spatial cell interest subscription failed: {error}"),
            });
        })
        .subscribe(queries);

    // Keep at most one live + one retiring spatial handle. Drop any already-
    // retiring handle immediately so subscriptions never accumulate.
    if let Some(previous) = transport.spatial_cell_handle.replace(handle)
        && let Some(stale) = transport.retiring_spatial_handle.replace(previous)
    {
        let _ = stale.unsubscribe();
    }
    transport.spatial_interest = Some(interest);
}

fn finish_retiring_spatial_handle(transport: &mut OnlineTransport) {
    let ready = transport
        .spatial_cell_handle
        .as_ref()
        .is_some_and(SubscriptionHandle::is_active);
    if ready && let Some(old) = transport.retiring_spatial_handle.take() {
        let _ = old.unsubscribe();
    }
}

/// High-scale bandwidth interest: resubscribe the spatial `CellState` handle when
/// the camera focus crosses a server chunk boundary. Local-owned cells remain on
/// the one-time tactical subscription.
fn maintain_moving_viewport_interest(
    mut transport: ResMut<OnlineTransport>,
    view: Res<MatchView>,
    camera: Option<Single<&CameraRig, With<GameCamera>>>,
) {
    finish_retiring_spatial_handle(&mut transport);
    if view.player_count <= HIGH_SCALE_PLAYER_THRESHOLD {
        return;
    }
    if transport.bound_player.is_none() || transport.connection.is_none() {
        return;
    }
    if !transport.subscription_lifecycle.tactical_ready {
        return;
    }
    let Some(rig) = camera else {
        return;
    };
    let Some(interest) = camera_focus_spatial_interest(&transport, &rig) else {
        return;
    };
    if transport
        .spatial_interest
        .is_some_and(|current| current.center() == interest.center())
    {
        return;
    }
    install_spatial_cell_subscription(&mut transport, interest);
}

fn camera_focus_spatial_interest(
    transport: &OnlineTransport,
    rig: &CameraRig,
) -> Option<SpatialInterest> {
    let connection = transport.connection.as_ref()?;
    let config = connection.db.match_config().iter().next()?;
    let plane = Vec2::new(rig.focus.x, rig.focus.z);
    let focus = plane_to_axial(plane);
    let (chunk_q, chunk_r) = if transport.coordinate_to_id.contains_key(&focus) {
        chunk_coords_for_axial(focus, config.map_q_min, config.map_r_min, config.chunk_size)
    } else if let Some((&coordinate, _)) =
        transport
            .coordinate_to_id
            .iter()
            .min_by_key(|(coordinate, _)| {
                let center = axial_to_plane(**coordinate);
                ordered_float_key(center.distance_squared(plane))
            })
    {
        chunk_coords_for_axial(
            coordinate,
            config.map_q_min,
            config.map_r_min,
            config.chunk_size,
        )
    } else {
        chunk_coords_for_axial(focus, config.map_q_min, config.map_r_min, config.chunk_size)
    };
    Some(SpatialInterest {
        focus_chunk_q: chunk_q,
        focus_chunk_r: chunk_r,
        radius: HIGH_SCALE_INTEREST_CHUNK_RADIUS,
    })
}

fn ordered_float_key(value: f32) -> u32 {
    // distance_squared is non-negative; keep a total order for BTree min_by_key.
    value.to_bits()
}

fn clear_tactical_presentation(view: &mut MatchView) {
    view.clear_authoritative_flows();
    view.active_flows.clear();
    view.active_fronts.clear();
    view.set_contested_cells(BTreeMap::new());
    if view.retask_projection != RetaskProjection::default() {
        view.retask_projection = RetaskProjection::default();
        view.retask_revision = view.retask_revision.wrapping_add(1);
    }
}

fn neutralize_absent_cells(
    transport: &OnlineTransport,
    view: &mut MatchView,
    absences: &BTreeSet<u32>,
    mode: MapViewMode,
) {
    let mut planning_changed = false;
    let mut ownership_changed = false;
    for &cell_id in absences {
        let Some(coordinate) = transport.id_to_coordinate.get(&cell_id).copied() else {
            continue;
        };
        let Some(cell) = view.cells.get_mut(&coordinate) else {
            continue;
        };
        let rendering_changed = cell.owner.is_some()
            || cell.infantry != 0
            || (mode == MapViewMode::Civilians && cell.civilians != 0);
        ownership_changed |= cell.owner.is_some();
        planning_changed |=
            cell.owner.is_some() || cell.infantry != 0 || cell.military_capacity != 0;
        if rendering_changed {
            view.dirty_chunks.insert(chunk_of(coordinate));
        }
        // Project subscription absence to neutral/default. Terrain stays;
        // remote dynamic state is unknown outside interest.
        cell.owner = None;
        cell.civilians = 0;
        cell.infantry = 0;
        cell.military_capacity = 0;
    }
    if planning_changed {
        view.mark_planning_changed();
    }
    if ownership_changed {
        view.mark_ownership_changed();
    }
    if !absences.is_empty() {
        view.mark_cell_state_changed();
    }
}

fn seed_command_ids(
    transport: &mut OnlineTransport,
    local_player: u32,
    receipts: &[CommandReceipt],
) {
    for receipt in receipts
        .iter()
        .filter(|receipt| u32::from(receipt.player_id) == local_player)
    {
        transport.observe_command_id(receipt.client_command_id);
        if !transport.pending.contains_key(&receipt.client_command_id) {
            transport.processed_receipts.insert(receipt.receipt_key);
            transport
                .terminal_command_ids
                .insert(receipt.client_command_id);
        }
    }
    if let Some(command_id) = transport.pending.keys().next_back().copied() {
        transport.observe_command_id(command_id);
    }
    if let Some(command_id) = transport.terminal_command_ids.iter().next_back().copied() {
        transport.observe_command_id(command_id);
    }
    transport.command_ids_ready = transport.next_command_id != u64::MAX;
}

fn process_receipts(
    transport: &mut OnlineTransport,
    view: &MatchView,
    receipts: Vec<CommandReceipt>,
    updates: &mut MessageWriter<ServerUpdate>,
) {
    for receipt in receipts {
        if u32::from(receipt.player_id) != view.local_player {
            continue;
        }
        transport.observe_command_id(receipt.client_command_id);
        if !transport.processed_receipts.insert(receipt.receipt_key) {
            continue;
        }

        let Some(pending) = transport.pending.remove(&receipt.client_command_id) else {
            transport
                .terminal_command_ids
                .insert(receipt.client_command_id);
            continue;
        };
        if !transport
            .terminal_command_ids
            .insert(receipt.client_command_id)
        {
            updates.write(ServerUpdate::Rejected {
                command_id: Some(receipt.client_command_id),
                reason: "Command ID collided with an earlier terminal command; retry".to_owned(),
                relevant_cell: None,
            });
            continue;
        }
        if receipt.command_name != pending.receipt_name() {
            updates.write(ServerUpdate::Rejected {
                command_id: Some(receipt.client_command_id),
                reason: format!(
                    "Command ID collision: expected {}, received {}; retry",
                    pending.receipt_name(),
                    receipt.command_name
                ),
                relevant_cell: None,
            });
            continue;
        }

        match receipt.status {
            ReceiptStatus::Accepted => {
                if let PendingCommand::Mobilization { target } = pending {
                    updates.write(ServerUpdate::MobilizationChanged {
                        command_id: Some(receipt.client_command_id),
                        target,
                    });
                } else {
                    let label = pending.label();
                    let order = if receipt.order_id != 0 {
                        format!(" · order #{}", receipt.order_id)
                    } else {
                        String::new()
                    };
                    updates.write(ServerUpdate::Accepted {
                        command_id: Some(receipt.client_command_id),
                        summary: format!("{label} accepted{order}"),
                        patches: Vec::new(),
                        flow: None,
                        front: None,
                    });
                }
            }
            ReceiptStatus::Rejected => {
                updates.write(ServerUpdate::Rejected {
                    command_id: Some(receipt.client_command_id),
                    reason: receipt.message,
                    relevant_cell: None,
                });
            }
        }
    }
}

fn packet_to_flow(
    transport: &OnlineTransport,
    view: &MatchView,
    packet: &TransitPacket,
    routes: &BTreeMap<u64, TransitRoute>,
    order: Option<&TransferOrder>,
) -> Option<ActiveFlow> {
    let _order = order?;
    let resolved_route = resolved_packet_route(packet, routes)?;
    let route_index = packet.route_index as usize;
    let route = resolved_route
        .get(route_index..)
        .unwrap_or_default()
        .iter()
        .filter_map(|cell_id| transport.id_to_coordinate.get(cell_id).copied())
        .collect::<Vec<_>>();
    if route.len() < 2 {
        return None;
    }
    let attacking = transport
        .id_to_coordinate
        .get(&packet.destination_cell)
        .and_then(|coordinate| view.cell(*coordinate))
        .and_then(|cell| cell.owner)
        .is_some_and(|owner| owner != u32::from(packet.owner_player_id));
    Some(ActiveFlow {
        route,
        strength: packet.infantry,
        attacking,
        age: 0.0,
        lifetime: 60.0,
    })
}

fn replace_authoritative_flows<'a>(
    transport: &OnlineTransport,
    view: &mut MatchView,
    packets: impl IntoIterator<Item = &'a TransitPacket>,
    routes: &BTreeMap<u64, TransitRoute>,
    orders: impl IntoIterator<Item = &'a TransferOrder>,
) {
    let orders_by_id = orders
        .into_iter()
        .map(|order| (order.order_id, order))
        .collect::<BTreeMap<_, _>>();
    let flows = packets
        .into_iter()
        .filter_map(|packet| {
            packet_to_flow(
                transport,
                view,
                packet,
                routes,
                orders_by_id.get(&packet.order_id).copied(),
            )
            .map(|flow| (packet.packet_key, flow))
        })
        .collect::<Vec<_>>();
    view.clear_authoritative_flows();
    for (packet_id, flow) in flows {
        view.set_authoritative_flow(packet_id, Some(flow));
    }
}

fn update_authoritative_flows(
    transport: &OnlineTransport,
    view: &mut MatchView,
    packet_ids: &BTreeSet<u64>,
) {
    for packet_id in packet_ids {
        let flow = transport
            .tactical
            .packets
            .get(packet_id)
            .and_then(|packet| {
                packet_to_flow(
                    transport,
                    view,
                    packet,
                    &transport.tactical.routes,
                    transport.tactical.orders.get(&packet.order_id),
                )
            });
        view.set_authoritative_flow(*packet_id, flow);
    }
}

fn resolved_packet_route(
    packet: &TransitPacket,
    routes: &BTreeMap<u64, TransitRoute>,
) -> Option<Vec<u32>> {
    if packet.route_id == 0 {
        return Some(if packet.current_cell == packet.destination_cell {
            vec![packet.current_cell]
        } else {
            vec![packet.current_cell, packet.destination_cell]
        });
    }
    routes
        .get(&packet.route_id)
        .map(|route| route.cells.clone())
}

fn fronts_to_overlays<'a>(
    transport: &OnlineTransport,
    view: &MatchView,
    fronts: impl IntoIterator<Item = &'a CombatFront>,
) -> (Vec<ActiveFront>, BTreeMap<Axial, ContestedCellView>) {
    let mut overlays = Vec::new();
    let mut pressure_by_target = BTreeMap::<Axial, BTreeMap<u32, u64>>::new();
    for front in fronts {
        let Some(from) = transport.id_to_coordinate.get(&front.from_cell).copied() else {
            continue;
        };
        let Some(to) = transport.id_to_coordinate.get(&front.to_cell).copied() else {
            continue;
        };
        let attacker_player = u32::from(front.attacker_player_id);
        let defender_player = u32::from(front.defender_player_id);
        let (friendly, hostile) = if attacker_player == view.local_player {
            (from, to)
        } else if defender_player == view.local_player {
            (to, from)
        } else {
            (from, to)
        };
        let engaged = front.attacker_engaged.max(front.defender_engaged);
        let intensity = if front.frontage == 0 {
            0.25
        } else {
            (engaged as f32 / front.frontage as f32).clamp(0.15, 1.0)
        };
        overlays.push(ActiveFront {
            friendly,
            hostile,
            intensity,
            age: 0.0,
        });

        let attacker_pressure = front
            .queued_infantry
            .saturating_add(front.attacker_engaged)
            .saturating_sub(front.attacker_casualties);
        let player_pressure = pressure_by_target
            .entry(to)
            .or_default()
            .entry(attacker_player)
            .or_default();
        *player_pressure = player_pressure.saturating_add(attacker_pressure);
    }

    let contested = pressure_by_target
        .into_iter()
        .filter_map(|(coordinate, pressure_by_player)| {
            let (attacker_player, attacker_pressure) = pressure_by_player.into_iter().max_by(
                |(left_player, left_pressure), (right_player, right_pressure)| {
                    left_pressure
                        .cmp(right_pressure)
                        .then_with(|| right_player.cmp(left_player))
                },
            )?;
            let cell = view.cell(coordinate)?;
            if cell.owner == Some(attacker_player) {
                return None;
            }
            let defender_pressure = cell.infantry;
            let total = attacker_pressure.saturating_add(defender_pressure);
            (total > 0).then_some((
                coordinate,
                ContestedCellView {
                    controller_player: cell.owner.unwrap_or(0),
                    attacker_player,
                    attacker_strength: attacker_pressure,
                    attacker_share: attacker_pressure as f32 / total as f32,
                },
            ))
        })
        .collect();
    (overlays, contested)
}

fn active_local_order<'a>(
    transport: &'a OnlineTransport,
    view: &MatchView,
    order_id: u64,
) -> Option<&'a TransferOrder> {
    transport.tactical.orders.get(&order_id).filter(|order| {
        order.status == OrderStatus::Active && u32::from(order.player_id) == view.local_player
    })
}

fn apply_retask_row_changes(
    transport: &mut OnlineTransport,
    view: &mut MatchView,
    impact: &ProjectionImpact,
    adding: bool,
) {
    let packets = impact
        .retask_packet_ids
        .iter()
        .filter_map(|key| transport.tactical.packets.get(key).cloned())
        .collect::<Vec<_>>();
    let sources = impact
        .retask_source_keys
        .iter()
        .filter_map(|key| transport.tactical.sources.get(key).cloned())
        .collect::<Vec<_>>();
    let destinations = impact
        .retask_destination_keys
        .iter()
        .filter_map(|key| transport.tactical.destinations.get(key).cloned())
        .collect::<Vec<_>>();
    for packet in &packets {
        adjust_retask_packet(transport, view, packet, adding);
    }
    for source in &sources {
        adjust_retask_source(transport, view, source, adding);
    }
    for destination in &destinations {
        adjust_retask_destination(transport, view, destination, adding);
    }
}

fn adjust_retask_packet(
    transport: &mut OnlineTransport,
    view: &mut MatchView,
    packet: &TransitPacket,
    adding: bool,
) {
    if u32::from(packet.owner_player_id) != view.local_player
        || active_local_order(transport, view, packet.order_id).is_none()
    {
        return;
    }
    let Some(current) = transport
        .id_to_coordinate
        .get(&packet.current_cell)
        .copied()
    else {
        return;
    };
    adjust_strength(
        &mut view.retask_projection.active_strength_by_cell,
        current,
        packet.infantry,
        adding,
    );
    adjust_order_strength(
        &mut view.retask_projection.order_strength_by_cell,
        packet.order_id,
        current,
        packet.infantry,
        adding,
    );
    if let Some(destination) = transport
        .id_to_coordinate
        .get(&packet.destination_cell)
        .copied()
    {
        adjust_destination_claim(
            transport,
            &mut view.retask_projection,
            packet.order_id,
            destination,
            adding,
        );
    }
    let next_index = packet.route_index as usize + 1;
    if let Some(next) = resolved_packet_route(packet, &transport.tactical.routes)
        .and_then(|route| route.get(next_index).copied())
    {
        adjust_retask_edge(
            transport,
            (packet.current_cell, next),
            packet.order_id,
            adding,
        );
    }
}

fn adjust_retask_source(
    transport: &mut OnlineTransport,
    view: &mut MatchView,
    source: &TransferSource,
    adding: bool,
) {
    if active_local_order(transport, view, source.order_id).is_none() {
        return;
    }
    let Some(coordinate) = transport.id_to_coordinate.get(&source.cell_id).copied() else {
        return;
    };
    let key = (source.order_id, coordinate);
    adjust_counted_set(
        &mut transport.retask_source_counts,
        key,
        &mut view.retask_projection.order_source_cells,
        source.order_id,
        coordinate,
        adding,
    );
}

fn adjust_retask_destination(
    transport: &mut OnlineTransport,
    view: &mut MatchView,
    destination: &TransferDestination,
    adding: bool,
) {
    let Some(order) = active_local_order(transport, view, destination.order_id) else {
        return;
    };
    let outstanding = destination
        .target_infantry
        .saturating_sub(destination.received_infantry);
    if outstanding == 0 {
        return;
    }
    let is_internal = matches!(
        order.kind,
        match_bindings::OrderKind::Reshape | match_bindings::OrderKind::FrontRebalance
    );
    let Some(coordinate) = transport
        .id_to_coordinate
        .get(&destination.cell_id)
        .copied()
    else {
        return;
    };
    adjust_destination_claim(
        transport,
        &mut view.retask_projection,
        destination.order_id,
        coordinate,
        adding,
    );
    if is_internal {
        adjust_order_strength(
            &mut view.retask_projection.destination_reservations_by_order,
            destination.order_id,
            coordinate,
            outstanding,
            adding,
        );
    }
}

fn adjust_strength(
    strengths: &mut BTreeMap<Axial, u64>,
    coordinate: Axial,
    amount: u64,
    adding: bool,
) {
    if adding {
        let strength = strengths.entry(coordinate).or_default();
        *strength = strength.saturating_add(amount);
    } else if let Some(strength) = strengths.get_mut(&coordinate) {
        *strength = strength.saturating_sub(amount);
        if *strength == 0 {
            strengths.remove(&coordinate);
        }
    }
}

fn adjust_order_strength(
    strengths: &mut BTreeMap<u64, BTreeMap<Axial, u64>>,
    order_id: u64,
    coordinate: Axial,
    amount: u64,
    adding: bool,
) {
    if adding {
        let strength = strengths
            .entry(order_id)
            .or_default()
            .entry(coordinate)
            .or_default();
        *strength = strength.saturating_add(amount);
        return;
    }
    let remove_order = strengths.get_mut(&order_id).is_some_and(|by_cell| {
        if let Some(strength) = by_cell.get_mut(&coordinate) {
            *strength = strength.saturating_sub(amount);
            if *strength == 0 {
                by_cell.remove(&coordinate);
            }
        }
        by_cell.is_empty()
    });
    if remove_order {
        strengths.remove(&order_id);
    }
}

fn adjust_destination_claim(
    transport: &mut OnlineTransport,
    projection: &mut RetaskProjection,
    order_id: u64,
    coordinate: Axial,
    adding: bool,
) {
    adjust_counted_set(
        &mut transport.retask_destination_claim_counts,
        (order_id, coordinate),
        &mut projection.destination_claims_by_order,
        order_id,
        coordinate,
        adding,
    );
}

fn adjust_counted_set(
    counts: &mut BTreeMap<(u64, Axial), u32>,
    count_key: (u64, Axial),
    sets: &mut BTreeMap<u64, BTreeSet<Axial>>,
    order_id: u64,
    coordinate: Axial,
    adding: bool,
) {
    if adding {
        *counts.entry(count_key).or_default() += 1;
        sets.entry(order_id).or_default().insert(coordinate);
        return;
    }
    let remove_coordinate = counts.get_mut(&count_key).is_some_and(|count| {
        *count = count.saturating_sub(1);
        *count == 0
    });
    if !remove_coordinate {
        return;
    }
    counts.remove(&count_key);
    let remove_order = sets.get_mut(&order_id).is_some_and(|coordinates| {
        coordinates.remove(&coordinate);
        coordinates.is_empty()
    });
    if remove_order {
        sets.remove(&order_id);
    }
}

fn adjust_retask_edge(
    transport: &mut OnlineTransport,
    edge: (u32, u32),
    order_id: u64,
    adding: bool,
) {
    let key = (edge, order_id);
    if adding {
        *transport.retask_edge_counts.entry(key).or_default() += 1;
        transport
            .retask_orders_by_edge
            .entry(edge)
            .or_default()
            .insert(order_id);
        return;
    }
    let remove_order = transport
        .retask_edge_counts
        .get_mut(&key)
        .is_some_and(|count| {
            *count = count.saturating_sub(1);
            *count == 0
        });
    if !remove_order {
        return;
    }
    transport.retask_edge_counts.remove(&key);
    let remove_edge = transport
        .retask_orders_by_edge
        .get_mut(&edge)
        .is_some_and(|orders| {
            orders.remove(&order_id);
            orders.is_empty()
        });
    if remove_edge {
        transport.retask_orders_by_edge.remove(&edge);
    }
}

fn refresh_retask_order_kinds(_transport: &OnlineTransport, view: &mut MatchView) {
    view.retask_projection.active_order_ids = view
        .retask_projection
        .order_strength_by_cell
        .keys()
        .copied()
        .collect();
}

fn rebuild_retask_handles(transport: &OnlineTransport, view: &mut MatchView) {
    view.retask_projection.handle_orders.clear();
    for front in transport.tactical.fronts.values().filter(|front| {
        u32::from(front.attacker_player_id) == view.local_player
            && front
                .queued_infantry
                .saturating_add(front.attacker_engaged)
                .saturating_sub(front.attacker_casualties)
                > 0
    }) {
        let Some(handle) = transport.id_to_coordinate.get(&front.to_cell).copied() else {
            continue;
        };
        if !view.is_local_retask_handle(handle) {
            continue;
        }
        let Some(order_ids) = transport
            .retask_orders_by_edge
            .get(&(front.from_cell, front.to_cell))
        else {
            continue;
        };
        view.retask_projection
            .handle_orders
            .entry(handle)
            .or_default()
            .extend(order_ids);
    }
}

fn rebuild_retask_indexes(transport: &mut OnlineTransport, view: &mut MatchView) {
    let previous = std::mem::take(&mut view.retask_projection);
    transport.retask_source_counts.clear();
    transport.retask_destination_claim_counts.clear();
    transport.retask_edge_counts.clear();
    transport.retask_orders_by_edge.clear();
    let impact = ProjectionImpact {
        retask_packet_ids: transport.tactical.packets.keys().copied().collect(),
        retask_source_keys: transport.tactical.sources.keys().copied().collect(),
        retask_destination_keys: transport.tactical.destinations.keys().copied().collect(),
        ..Default::default()
    };
    apply_retask_row_changes(transport, view, &impact, true);
    refresh_retask_order_kinds(transport, view);
    rebuild_retask_handles(transport, view);
    if view.retask_projection != previous {
        view.retask_revision = view.retask_revision.wrapping_add(1);
    }
}

fn reducer_failure(result: Result<Result<(), String>, InternalError>) -> Option<String> {
    match result {
        Ok(Ok(())) => None,
        Ok(Err(reason)) => Some(reason),
        Err(error) => Some(error.to_string()),
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn load_token(config: &ClientConfig) -> Result<Option<String>, String> {
    let path = config.token_path();
    match fs::read_to_string(path) {
        Ok(token) => {
            let token = token.trim().to_owned();
            Ok((!token.is_empty()).then_some(token))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.to_string()),
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn save_token(config: &ClientConfig, token: &str) -> Result<(), String> {
    save_token_file(&config.token_path(), token).map_err(|error| error.to_string())
}

#[cfg(not(target_arch = "wasm32"))]
fn save_token_file(path: &Path, token: &str) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    #[cfg(unix)]
    {
        use std::{
            fs::{File, OpenOptions, Permissions},
            io::Write,
            os::unix::fs::{OpenOptionsExt, PermissionsExt},
        };

        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let filename = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("client-token");
        let temporary = path.with_file_name(format!(".{filename}.{}.{}.tmp", process::id(), nonce));
        let write_result: io::Result<()> = (|| {
            let mut file = OpenOptions::new()
                .create_new(true)
                .write(true)
                .mode(0o600)
                .open(&temporary)?;
            file.write_all(token.as_bytes())?;
            file.sync_all()?;
            fs::rename(&temporary, path)?;
            fs::set_permissions(path, Permissions::from_mode(0o600))?;
            if let Some(parent) = path.parent() {
                File::open(parent)?.sync_all()?;
            }
            Ok(())
        })();
        if write_result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        write_result?;
    }
    #[cfg(not(unix))]
    fs::write(path, token)?;
    Ok(())
}

#[cfg(target_arch = "wasm32")]
fn load_token(config: &ClientConfig) -> Result<Option<String>, String> {
    let token = browser_storage()?
        .get_item(&config.token_storage_key())
        .map_err(|error| format!("{error:?}"))?;
    Ok(token.filter(|token| !token.trim().is_empty()))
}

#[cfg(target_arch = "wasm32")]
fn save_token(config: &ClientConfig, token: &str) -> Result<(), String> {
    browser_storage()?
        .set_item(&config.token_storage_key(), token)
        .map_err(|error| format!("{error:?}"))
}

#[cfg(target_arch = "wasm32")]
fn browser_storage() -> Result<web_sys::Storage, String> {
    web_sys::window()
        .ok_or_else(|| "browser window is unavailable".to_owned())?
        .local_storage()
        .map_err(|error| format!("{error:?}"))?
        .ok_or_else(|| "browser localStorage is unavailable".to_owned())
}

#[cfg(not(target_arch = "wasm32"))]
fn credential_store_label(config: &ClientConfig) -> String {
    config.token_path().display().to_string()
}

#[cfg(target_arch = "wasm32")]
fn credential_store_label(config: &ClientConfig) -> String {
    format!("browser localStorage ({})", config.token_storage_key())
}
