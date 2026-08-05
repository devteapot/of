use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fs, io,
    path::Path,
    process,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU32, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use bevy::prelude::*;
use hex_core::{Axial, TerrainKind};
#[cfg(debug_assertions)]
use match_bindings::TransitPacketTableAccess;
#[cfg(not(debug_assertions))]
use match_bindings::VisiblePacketsTableAccess;
use match_bindings::{
    CellState, CellStateTableAccess, CellTerrain, CellTerrainTableAccess, ClusterPolicyAssignment,
    ClusterPolicyAssignmentTableAccess, ClusterPolicyKind as RemoteClusterPolicyKind, CombatFront,
    CombatFrontTableAccess, CommandReceipt, CommandReceiptTableAccess, DbConnection, MatchConfig,
    MatchConfigTableAccess, MatchPhase as RemoteMatchPhase, MatchState, MatchStateTableAccess,
    MobilizationPolicy, MobilizationPolicyTableAccess, OrderStatus, PlayerSlot,
    PlayerSlotTableAccess, ReceiptStatus, TransferDestination, TransferDestinationTableAccess,
    TransferOrder, TransferOrderTableAccess, TransferSource, TransferSourceTableAccess,
    TransitPacket, cancel_orders, issue_attack_clusters, issue_balance, issue_core_load,
    issue_expand_all, issue_expand_clusters, issue_front_load, issue_perimeter_load,
    issue_push_front, issue_reshape, join_match, set_cluster_policy, set_mobilization_target,
};
use spacetimedb_sdk::__codegen::InternalError;
use spacetimedb_sdk::{DbContext, Table, TableWithPrimaryKey};

use crate::{
    config::ClientConfig,
    geometry::chunk_of,
    map_view::MapViewMode,
    model::{
        ActiveFlow, ActiveFront, AuthorityState, CellView, ClusterPolicyView, ConnectionState,
        ContestedCellView, MatchPhase, MatchView, RetaskProjection, ToastKind,
    },
    network::{ClientIntent, ClusterPolicy, NetworkSet, RedistributionPreset, ServerUpdate},
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
const POLICIES_DIRTY: u32 = 1 << 9;
const ALL_DIRTY: u32 = (1 << 10) - 1;

/// Debug builds retain the raw packet stream for the F4 policy-route
/// diagnostic. Release builds use the server-maintained tactical view, so
/// background rebalancing packets never cross the network.
#[cfg(debug_assertions)]
const PACKET_STREAM_QUERY: &str = "SELECT * FROM transit_packet";
#[cfg(not(debug_assertions))]
const PACKET_STREAM_QUERY: &str = "SELECT * FROM visible_packets";

/// Subscribe only to the public state used by the game client.
const CLIENT_SUBSCRIPTIONS: [&str; 13] = [
    "SELECT * FROM cell_state",
    "SELECT * FROM cell_terrain",
    "SELECT * FROM cluster_policy_assignment",
    "SELECT * FROM combat_front",
    "SELECT * FROM command_receipt",
    "SELECT * FROM match_config",
    "SELECT * FROM match_state",
    "SELECT * FROM mobilization_policy",
    "SELECT * FROM player_slot",
    "SELECT * FROM transfer_destination",
    "SELECT * FROM transfer_order",
    "SELECT * FROM transfer_source",
    PACKET_STREAM_QUERY,
];

#[cfg(debug_assertions)]
const POLICY_FLOW_DEBUG_KEY: KeyCode = KeyCode::F4;

#[derive(SystemSet, Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct OnlineSyncSet;

#[derive(Clone, Debug)]
enum LifecycleEvent {
    Connected { generation: u64 },
    Subscribed { generation: u64 },
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
    packet_changes: Mutex<BTreeMap<String, Option<TransitPacket>>>,
    front_changes: Mutex<BTreeMap<String, Option<CombatFront>>>,
    order_changes: Mutex<BTreeMap<u64, Option<TransferOrder>>>,
    source_changes: Mutex<BTreeMap<String, Option<TransferSource>>>,
    destination_changes: Mutex<BTreeMap<String, Option<TransferDestination>>>,
    events: Mutex<VecDeque<LifecycleEvent>>,
}

impl SharedSignals {
    fn mark(&self, bits: u32) {
        self.dirty.fetch_or(bits, Ordering::Release);
    }

    fn take_dirty(&self) -> u32 {
        self.dirty.swap(0, Ordering::AcqRel)
    }

    fn record_cell(&self, cell: &CellState) {
        if let Ok(mut changes) = self.cell_changes.lock() {
            changes.insert(cell.cell_id, cell.clone());
        }
        self.mark(CELLS_DIRTY);
    }

    fn take_cell_changes(&self) -> Vec<CellState> {
        self.cell_changes.lock().map_or_else(
            |_| Vec::new(),
            |mut changes| std::mem::take(&mut *changes).into_values().collect(),
        )
    }

    fn take_tactical_changes(&self) -> TacticalChanges {
        TacticalChanges {
            packets: take_changes(&self.packet_changes),
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
    packets: BTreeMap<String, Option<TransitPacket>>,
    fronts: BTreeMap<String, Option<CombatFront>>,
    orders: BTreeMap<u64, Option<TransferOrder>>,
    sources: BTreeMap<String, Option<TransferSource>>,
    destinations: BTreeMap<String, Option<TransferDestination>>,
}

#[derive(Default)]
struct TacticalCache {
    packets: BTreeMap<String, TransitPacket>,
    fronts: BTreeMap<String, CombatFront>,
    orders: BTreeMap<u64, TransferOrder>,
    sources: BTreeMap<String, TransferSource>,
    destinations: BTreeMap<String, TransferDestination>,
}

impl TacticalCache {
    fn capture(connection: &DbConnection) -> Self {
        Self {
            packets: subscribed_packets(connection)
                .into_iter()
                .map(|packet| (packet.packet_key.clone(), packet))
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
                .map(|source| (source.source_key.clone(), source))
                .collect(),
            destinations: connection
                .db
                .transfer_destination()
                .iter()
                .map(|destination| (destination.destination_key.clone(), destination))
                .collect(),
        }
    }

    fn apply(&mut self, changes: TacticalChanges) {
        apply_changes(&mut self.packets, changes.packets);
        apply_changes(&mut self.fronts, changes.fronts);
        apply_changes(&mut self.orders, changes.orders);
        apply_changes(&mut self.sources, changes.sources);
        apply_changes(&mut self.destinations, changes.destinations);
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
    ClusterPolicy(ClusterPolicy),
    PushFront,
    ExpandAll,
    Reshape,
    CancelOrders,
    Balance,
    FrontLoad,
    CoreLoad,
    PerimeterLoad,
    Mobilization { target: f32 },
}

impl PendingCommand {
    const fn label(&self) -> &'static str {
        match self {
            Self::ExpandClusters => "Expand Clusters",
            Self::AttackClusters => "Attack Clusters",
            Self::ClusterPolicy(policy) => policy.label(),
            Self::PushFront => "Push Front",
            Self::ExpandAll => "Expand Perimeter",
            Self::Reshape => "Reshape",
            Self::CancelOrders => "Stop Orders",
            Self::Balance => "Formation · Balanced",
            Self::FrontLoad => "Directional Bias",
            Self::CoreLoad => "Formation · Center",
            Self::PerimeterLoad => "Formation · Perimeter",
            Self::Mobilization { .. } => "Mobilization",
        }
    }

    const fn receipt_name(&self) -> &'static str {
        match self {
            Self::ExpandClusters => "issue_expand_clusters",
            Self::AttackClusters => "issue_attack_clusters",
            Self::ClusterPolicy(_) => "set_cluster_policy",
            Self::PushFront => "issue_push_front",
            Self::ExpandAll => "issue_expand_all",
            Self::Reshape => "issue_reshape",
            Self::CancelOrders => "cancel_orders",
            Self::Balance => "issue_balance",
            Self::FrontLoad => "issue_front_load",
            Self::CoreLoad => "issue_core_load",
            Self::PerimeterLoad => "issue_perimeter_load",
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
    signals: Arc<SharedSignals>,
    config: ClientConfig,
    coordinate_to_id: BTreeMap<Axial, u32>,
    id_to_coordinate: BTreeMap<u32, Axial>,
    tactical: TacticalCache,
    pending: BTreeMap<u64, PendingCommand>,
    processed_receipts: BTreeSet<String>,
    terminal_command_ids: BTreeSet<u64>,
    next_command_id: u64,
    bound_player: Option<u8>,
    subscription_ready: bool,
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
            signals: Arc::default(),
            config,
            coordinate_to_id: BTreeMap::new(),
            id_to_coordinate: BTreeMap::new(),
            tactical: TacticalCache::default(),
            pending: BTreeMap::new(),
            processed_receipts: BTreeSet::new(),
            terminal_command_ids: BTreeSet::new(),
            next_command_id: session_command_floor(),
            bound_player: None,
            subscription_ready: false,
            command_ids_ready: false,
            active_generation: 0,
            failed_generation: None,
            reconnect_attempt: 0,
            reconnect_delay_seconds: 0.0,
            connection_disabled: false,
        }
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

    let milliseconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX >> PROCESS_BITS)) as u64;
    (milliseconds << PROCESS_BITS) | (u64::from(process::id()) & PROCESS_MASK)
}

pub struct OnlineTransportPlugin;

impl Plugin for OnlineTransportPlugin {
    fn build(&self, app: &mut App) {
        let config = app.world().resource::<ClientConfig>().clone();
        app.insert_resource(OnlineTransport::new(config))
            .add_systems(
                Update,
                (maintain_connection, send_online_intents, frame_tick)
                    .chain()
                    .in_set(NetworkSet::Transport),
            )
            .add_systems(
                Update,
                synchronize_authoritative_view
                    .in_set(NetworkSet::Apply)
                    .in_set(OnlineSyncSet),
            );

        #[cfg(debug_assertions)]
        app.add_systems(
            Update,
            toggle_policy_flow_debug.in_set(NetworkSet::Transport),
        );
    }
}

/// Development-only presentation switch for background policy logistics.
///
/// Packet visibility depends on both packet and order provenance. Force both
/// caches through the authoritative projection so enabling is immediate and
/// disabling cannot leave a previously rendered route behind.
#[cfg(debug_assertions)]
fn toggle_policy_flow_debug(
    keyboard: Res<ButtonInput<KeyCode>>,
    mut transport: ResMut<OnlineTransport>,
    mut view: ResMut<MatchView>,
) {
    if !keyboard.just_pressed(POLICY_FLOW_DEBUG_KEY) {
        return;
    }

    transport.config.debug_policy_flows = !transport.config.debug_policy_flows;
    transport.signals.mark(FLOWS_DIRTY | ORDERS_DIRTY);
    if !transport.config.debug_policy_flows {
        // A connected client restores explicit routes from authority later in
        // this frame. Clear first so stale policy trails also disappear when
        // the toggle is used during a reconnect and no snapshot is available.
        view.active_flows.clear();
    }
    let status = if transport.config.debug_policy_flows {
        "ON · F4 to hide"
    } else {
        "OFF · F4 to show"
    };
    view.show_toast(format!("DEBUG · policy routes {status}"), ToastKind::Info);
}

fn maintain_connection(
    time: Res<Time>,
    mut transport: ResMut<OnlineTransport>,
    mut view: ResMut<MatchView>,
) {
    if transport.connection.is_some() || transport.connection_disabled {
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
    transport.subscription_ready = false;
    transport.command_ids_ready = false;
    view.authority = AuthorityState::Connecting;

    let token_path = transport.config.token_path();
    let token = match load_token(&token_path) {
        Ok(token) => token,
        Err(error) => {
            view.push_log(format!(
                "Could not read auth token {}: {error}",
                token_path.display()
            ));
            None
        }
    };

    let signals = Arc::clone(&transport.signals);
    let connect_signals = Arc::clone(&signals);
    let connect_error_signals = Arc::clone(&signals);
    let disconnect_signals = Arc::clone(&signals);
    let token_path_for_callback = token_path.clone();
    let preferred_player = transport.config.preferred_player;
    let display_name = transport.config.display_name.clone();

    let connection = DbConnection::builder()
        .with_uri(host)
        .with_database_name(transport.config.database.clone())
        .with_token(token)
        .on_connect(move |connection, _identity, private_token| {
            if let Err(error) = save_token(&token_path_for_callback, private_token) {
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
                    applied_signals.push(LifecycleEvent::Subscribed { generation });
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
                        reason: format!("subscription failed: {error}"),
                    });
                })
                .subscribe(CLIENT_SUBSCRIPTIONS);
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
        })
        .build();

    match connection {
        Ok(connection) => {
            register_table_watchers(&connection, &signals);
            transport.connection = Some(connection);
            view.latest_result = format!(
                "Connecting to {} / {} as {}…",
                transport.config.host, transport.config.database, transport.config.display_name
            );
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

fn disable_invalid_host(transport: &mut OnlineTransport, view: &mut MatchView, reason: &str) {
    transport.connection_disabled = true;
    transport.subscription_ready = false;
    transport.command_ids_ready = false;
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
    connection.db.cell_state().on_delete(move |_context, _row| {
        // Cell deletion only occurs during lobby map replacement. Force a
        // terrain-backed rebuild so removed cells cannot remain in the view.
        cell_delete_signals.mark(TERRAIN_DIRTY | CELLS_DIRTY);
    });
    watch_table!(connection.db.cluster_policy_assignment(), POLICIES_DIRTY);
    watch_table!(connection.db.match_config(), MATCH_DIRTY);
    watch_table!(connection.db.match_state(), MATCH_DIRTY);
    watch_table!(connection.db.player_slot(), PLAYERS_DIRTY);
    watch_table!(connection.db.mobilization_policy(), MOBILIZATION_DIRTY);
    watch_table!(connection.db.command_receipt(), RECEIPTS_DIRTY);
    #[cfg(debug_assertions)]
    {
        let packet_insert_signals = Arc::clone(signals);
        connection
            .db
            .transit_packet()
            .on_insert(move |_context, row| {
                record_change(
                    &packet_insert_signals.packet_changes,
                    row.packet_key.clone(),
                    Some(row.clone()),
                );
                packet_insert_signals.mark(FLOWS_DIRTY);
            });
        let packet_delete_signals = Arc::clone(signals);
        connection
            .db
            .transit_packet()
            .on_delete(move |_context, row| {
                record_change(
                    &packet_delete_signals.packet_changes,
                    row.packet_key.clone(),
                    None,
                );
                packet_delete_signals.mark(FLOWS_DIRTY);
            });
        let packet_update_signals = Arc::clone(signals);
        connection
            .db
            .transit_packet()
            .on_update(move |_context, _old, new| {
                record_change(
                    &packet_update_signals.packet_changes,
                    new.packet_key.clone(),
                    Some(new.clone()),
                );
                packet_update_signals.mark(FLOWS_DIRTY);
            });
    }
    #[cfg(not(debug_assertions))]
    {
        let packet_insert_signals = Arc::clone(signals);
        connection
            .db
            .visible_packets()
            .on_insert(move |_context, row| {
                record_change(
                    &packet_insert_signals.packet_changes,
                    row.packet_key.clone(),
                    Some(row.clone()),
                );
                packet_insert_signals.mark(FLOWS_DIRTY);
            });
        let packet_delete_signals = Arc::clone(signals);
        connection
            .db
            .visible_packets()
            .on_delete(move |_context, row| {
                record_change(
                    &packet_delete_signals.packet_changes,
                    row.packet_key.clone(),
                    None,
                );
                packet_delete_signals.mark(FLOWS_DIRTY);
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
                row.source_key.clone(),
                Some(row.clone()),
            );
            source_insert_signals.mark(ORDERS_DIRTY);
        });
    let source_delete_signals = Arc::clone(signals);
    connection
        .db
        .transfer_source()
        .on_delete(move |_context, row| {
            record_change(
                &source_delete_signals.source_changes,
                row.source_key.clone(),
                None,
            );
            source_delete_signals.mark(ORDERS_DIRTY);
        });
    let source_update_signals = Arc::clone(signals);
    connection
        .db
        .transfer_source()
        .on_update(move |_context, _old, new| {
            record_change(
                &source_update_signals.source_changes,
                new.source_key.clone(),
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
                row.destination_key.clone(),
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
                row.destination_key.clone(),
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
                new.destination_key.clone(),
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
        if !transport.subscription_ready || !transport.command_ids_ready {
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
        ClientIntent::SetClusterPolicy {
            sources,
            policy,
            direction,
        } => {
            let (kind, orientation) = remote_cluster_policy(*policy, *direction)?;
            connection
                .reducers
                .set_cluster_policy_then(
                    command_id,
                    ids_for_selection(transport, sources)?,
                    kind,
                    orientation.q,
                    orientation.r,
                    callback,
                )
                .map_err(|error| error.to_string())?;
            Ok(PendingCommand::ClusterPolicy(*policy))
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
        ClientIntent::Redistribute {
            cells,
            supersede_order_ids,
            preset: RedistributionPreset::Balance,
            ..
        } => {
            connection
                .reducers
                .issue_balance_then(
                    command_id,
                    ids_for_selection(transport, cells)?,
                    supersede_order_ids.iter().copied().collect(),
                    callback,
                )
                .map_err(|error| error.to_string())?;
            Ok(PendingCommand::Balance)
        }
        ClientIntent::Redistribute {
            cells,
            supersede_order_ids,
            preset: RedistributionPreset::FrontLoad,
            direction,
        } => {
            let orientation = validated_front_load_orientation(*direction)
                .ok_or_else(|| "Directional Bias direction is too short".to_owned())?;
            connection
                .reducers
                .issue_front_load_then(
                    command_id,
                    ids_for_selection(transport, cells)?,
                    orientation.q,
                    orientation.r,
                    supersede_order_ids.iter().copied().collect(),
                    callback,
                )
                .map_err(|error| error.to_string())?;
            Ok(PendingCommand::FrontLoad)
        }
        ClientIntent::Redistribute {
            cells,
            supersede_order_ids,
            preset: RedistributionPreset::CoreLoad,
            ..
        } => {
            connection
                .reducers
                .issue_core_load_then(
                    command_id,
                    ids_for_selection(transport, cells)?,
                    supersede_order_ids.iter().copied().collect(),
                    callback,
                )
                .map_err(|error| error.to_string())?;
            Ok(PendingCommand::CoreLoad)
        }
        ClientIntent::Redistribute {
            cells,
            supersede_order_ids,
            preset: RedistributionPreset::PerimeterLoad,
            ..
        } => {
            connection
                .reducers
                .issue_perimeter_load_then(
                    command_id,
                    ids_for_selection(transport, cells)?,
                    supersede_order_ids.iter().copied().collect(),
                    callback,
                )
                .map_err(|error| error.to_string())?;
            Ok(PendingCommand::PerimeterLoad)
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

fn remote_cluster_policy(
    policy: ClusterPolicy,
    direction: Option<Axial>,
) -> Result<(RemoteClusterPolicyKind, Axial), String> {
    let (kind, orientation) = match policy {
        ClusterPolicy::Balanced => (RemoteClusterPolicyKind::Balanced, Axial::ZERO),
        ClusterPolicy::Center => (RemoteClusterPolicyKind::Center, Axial::ZERO),
        ClusterPolicy::Perimeter => (RemoteClusterPolicyKind::Perimeter, Axial::ZERO),
        ClusterPolicy::Directional => (
            RemoteClusterPolicyKind::Directional,
            direction
                .filter(|orientation| *orientation != Axial::ZERO)
                .ok_or_else(|| {
                    "Directional cluster policy needs a visible orientation".to_owned()
                })?,
        ),
    };
    if policy != ClusterPolicy::Directional && direction.is_some() {
        return Err("Only the directional cluster policy accepts an orientation".to_owned());
    }
    Ok((kind, orientation))
}

fn validated_front_load_orientation(direction: Option<Axial>) -> Option<Axial> {
    direction.filter(|orientation| *orientation != Axial::ZERO)
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
    cluster_policies: Option<Vec<ClusterPolicyAssignment>>,
    config: Option<MatchConfig>,
    match_state: Option<MatchState>,
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
            cluster_policies: (dirty & POLICIES_DIRTY != 0)
                .then(|| connection.db.cluster_policy_assignment().iter().collect()),
            config: (dirty & MATCH_DIRTY != 0)
                .then(|| connection.db.match_config().iter().next())
                .flatten(),
            match_state: (dirty & MATCH_DIRTY != 0)
                .then(|| connection.db.match_state().iter().next())
                .flatten(),
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

#[cfg(debug_assertions)]
fn subscribed_packets(connection: &DbConnection) -> Vec<TransitPacket> {
    connection.db.transit_packet().iter().collect()
}

#[cfg(not(debug_assertions))]
fn subscribed_packets(connection: &DbConnection) -> Vec<TransitPacket> {
    connection.db.visible_packets().iter().collect()
}

fn synchronize_authoritative_view(
    mut transport: ResMut<OnlineTransport>,
    mut view: ResMut<MatchView>,
    mode: Res<MapViewMode>,
    mut updates: MessageWriter<ServerUpdate>,
) {
    for event in transport.signals.drain() {
        apply_lifecycle_event(&mut transport, &mut view, &mut updates, event);
    }

    let dirty = transport.signals.take_dirty();
    if dirty == 0 {
        return;
    }
    let snapshot = {
        let Some(connection) = &transport.connection else {
            return;
        };
        AuthoritySnapshot::capture(connection, dirty, transport.signals.take_cell_changes())
    };
    let tactical_changes = transport.signals.take_tactical_changes();
    if let Some(tactical) = snapshot.tactical {
        transport.tactical = tactical;
    }
    // Deltas may arrive between the full capture and this drain. Applying
    // them unconditionally closes that race without another table scan.
    transport.tactical.apply(tactical_changes);

    if let Some(terrain) = snapshot.terrain {
        rebuild_cells(
            &mut transport,
            &mut view,
            terrain,
            snapshot.cells.as_deref(),
        );
    } else if let Some(cells) = snapshot.cells {
        update_cells(&transport, &mut view, &cells, *mode);
    }

    if let Some(config) = snapshot.config {
        view.conquest_threshold_bps = config.conquest_threshold_bps;
        view.max_elevation_step = u16::from(config.max_elevation_step);
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
        view.authoritative_control =
            Some([state.player_one_controlled, state.player_two_controlled]);
    }
    if let Some(players) = snapshot.players {
        update_players(&mut transport, &mut view, snapshot.identity, &players);
    }
    if let Some(policies) = snapshot.cluster_policies {
        update_cluster_policies(&transport, &mut view, &policies);
    }
    if let Some(policies) = snapshot.mobilization
        && let Some(policy) = policies
            .iter()
            .find(|policy| u32::from(policy.player_id) == view.local_player)
    {
        view.mobilization_target = policy.target_bps as f32 / 10_000.0;
    }

    if !transport.command_ids_ready
        && transport.subscription_ready
        && snapshot.identity.is_some()
        && let Some(receipts) = snapshot.receipts.as_deref()
        && transport.bound_player.is_some()
    {
        seed_command_ids(&mut transport, view.local_player, receipts);
    }
    if let Some(receipts) = snapshot.receipts {
        process_receipts(&mut transport, &view, receipts, &mut updates);
    }
    if dirty & (TERRAIN_DIRTY | FLOWS_DIRTY | ORDERS_DIRTY) != 0 {
        replace_authoritative_flows(
            &transport,
            &mut view,
            transport.tactical.packets.values(),
            transport.tactical.orders.values(),
            transport.config.debug_policy_flows,
        );
    }
    if dirty & (TERRAIN_DIRTY | FRONTS_DIRTY) != 0 {
        let (active_fronts, contested_cells) =
            fronts_to_overlays(&transport, &view, transport.tactical.fronts.values());
        view.active_fronts = active_fronts;
        view.set_contested_cells(contested_cells);
    }
    if dirty & (TERRAIN_DIRTY | FLOWS_DIRTY | FRONTS_DIRTY | ORDERS_DIRTY) != 0 {
        let projection = retask_projection_from_authority(
            &transport,
            &view,
            transport.tactical.orders.values(),
            transport.tactical.packets.values(),
            transport.tactical.fronts.values(),
            transport.tactical.sources.values(),
            transport.tactical.destinations.values(),
        );
        view.set_retask_projection(projection);
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
        LifecycleEvent::Subscribed { generation }
            if lifecycle_is_current(transport, generation) =>
        {
            transport.reconnect_attempt = 0;
            transport.reconnect_delay_seconds = 0.0;
            transport.subscription_ready = true;
            view.authority = AuthorityState::Connecting;
            "Authoritative snapshot applied · joining match…".clone_into(&mut view.latest_result);
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
        | LifecycleEvent::Subscribed { .. }
        | LifecycleEvent::JoinFailed { .. }
        | LifecycleEvent::TokenWarning { .. } => {}
    }
}

fn mark_join_failed(transport: &mut OnlineTransport, view: &mut MatchView, reason: String) {
    transport.bound_player = None;
    transport.command_ids_ready = false;
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

    transport.subscription_ready = false;
    transport.command_ids_ready = false;
    transport.bound_player = None;
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
    view.connection = [ConnectionState::Syncing, ConnectionState::Syncing];
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

const fn owner(player_id: u8) -> Option<u32> {
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
    for player_id in [1_u8, 2] {
        view.connection[usize::from(player_id - 1)] = players
            .iter()
            .find(|slot| slot.player_id == player_id)
            .map_or(ConnectionState::Syncing, |slot| {
                if slot.identity.is_none() {
                    ConnectionState::Open
                } else if slot.connected {
                    ConnectionState::Connected
                } else {
                    ConnectionState::ClaimedOffline
                }
            });
    }
    if let Some(identity) = identity
        && let Some(slot) = players
            .iter()
            .find(|slot| slot.identity.as_ref() == Some(&identity))
    {
        let local_player = u32::from(slot.player_id);
        if view.local_player != local_player {
            view.local_player = local_player;
            view.mark_planning_changed();
            view.mark_ownership_changed();
        }
        if transport.bound_player != Some(slot.player_id) {
            transport.bound_player = Some(slot.player_id);
            transport.command_ids_ready = false;
            view.authority = AuthorityState::Ready;
            view.latest_result = format!(
                "Authoritative controls ready · bound to Player {}",
                slot.player_id
            );
        }
    }
}

fn update_cluster_policies(
    transport: &OnlineTransport,
    view: &mut MatchView,
    assignments: &[ClusterPolicyAssignment],
) {
    view.cluster_policies.clear();
    for assignment in assignments
        .iter()
        .filter(|assignment| u32::from(assignment.owner_player_id) == view.local_player)
    {
        let Some(&coordinate) = transport.id_to_coordinate.get(&assignment.cell_id) else {
            continue;
        };
        let kind = match assignment.kind {
            RemoteClusterPolicyKind::Balanced => ClusterPolicy::Balanced,
            RemoteClusterPolicyKind::Center => ClusterPolicy::Center,
            RemoteClusterPolicyKind::Perimeter => ClusterPolicy::Perimeter,
            RemoteClusterPolicyKind::Directional => ClusterPolicy::Directional,
        };
        view.cluster_policies.insert(
            coordinate,
            ClusterPolicyView {
                kind,
                orientation: Axial::new(assignment.orientation_q, assignment.orientation_r),
                revision: assignment.revision,
            },
        );
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
            transport
                .processed_receipts
                .insert(receipt.receipt_key.clone());
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
        if !transport
            .processed_receipts
            .insert(receipt.receipt_key.clone())
        {
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

fn packets_to_flows<'a>(
    transport: &OnlineTransport,
    view: &MatchView,
    packets: impl IntoIterator<Item = &'a TransitPacket>,
    orders: impl IntoIterator<Item = &'a TransferOrder>,
    show_policy_flows: bool,
) -> Vec<ActiveFlow> {
    // Packet and order callbacks are independent. On a busy policy tick the
    // packet can therefore be visible in the SDK cache one frame before its
    // order row. Unknown packets must fail closed: otherwise every newly
    // spawned background redistribution flashes as a cyan action route before
    // the next order-table callback supplies the information needed to hide it.
    let orders_by_id = orders
        .into_iter()
        .map(|order| (order.order_id, order))
        .collect::<BTreeMap<_, _>>();
    packets
        .into_iter()
        .filter_map(|packet| {
            let order = orders_by_id.get(&packet.order_id)?;
            if !order_flow_is_visible(order, show_policy_flows) {
                return None;
            }
            let route_index = packet.route_index as usize;
            let route = packet
                .route
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
        })
        .collect()
}

fn replace_authoritative_flows<'a>(
    transport: &OnlineTransport,
    view: &mut MatchView,
    packets: impl IntoIterator<Item = &'a TransitPacket>,
    orders: impl IntoIterator<Item = &'a TransferOrder>,
    show_policy_flows: bool,
) {
    view.active_flows = packets_to_flows(transport, view, packets, orders, show_policy_flows);
}

const fn order_flow_is_visible(order: &TransferOrder, show_policy_flows: bool) -> bool {
    match order.kind {
        match_bindings::OrderKind::Balance
        | match_bindings::OrderKind::FrontLoad
        | match_bindings::OrderKind::CoreLoad
        | match_bindings::OrderKind::PerimeterLoad => show_policy_flows,
        match_bindings::OrderKind::Reshape
        | match_bindings::OrderKind::PushFront
        | match_bindings::OrderKind::ExpandAll
        | match_bindings::OrderKind::ExpandClusters
        | match_bindings::OrderKind::AttackClusters => true,
    }
}

fn is_internal_distribution_order(order: &TransferOrder) -> bool {
    matches!(
        order.kind,
        match_bindings::OrderKind::Balance
            | match_bindings::OrderKind::FrontLoad
            | match_bindings::OrderKind::CoreLoad
            | match_bindings::OrderKind::PerimeterLoad
    )
}

fn is_policy_maintenance_order(order: &TransferOrder) -> bool {
    order.client_command_id == 0 && is_internal_distribution_order(order)
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

fn retask_projection_from_authority<'a, O, P, F, S, D>(
    transport: &OnlineTransport,
    view: &MatchView,
    orders: O,
    packets: P,
    fronts: F,
    sources: S,
    destinations: D,
) -> RetaskProjection
where
    O: Clone + IntoIterator<Item = &'a TransferOrder>,
    P: IntoIterator<Item = &'a TransitPacket>,
    F: IntoIterator<Item = &'a CombatFront>,
    S: IntoIterator<Item = &'a TransferSource>,
    D: IntoIterator<Item = &'a TransferDestination>,
{
    let active_local_orders = orders
        .clone()
        .into_iter()
        .filter(|order| {
            order.status == OrderStatus::Active && u32::from(order.player_id) == view.local_player
        })
        .map(|order| order.order_id)
        .collect::<BTreeSet<_>>();
    let mut projection = RetaskProjection {
        background_policy_order_ids: orders
            .clone()
            .into_iter()
            .filter(|order| {
                order.status == OrderStatus::Active
                    && u32::from(order.player_id) == view.local_player
                    && is_policy_maintenance_order(order)
            })
            .map(|order| order.order_id)
            .collect(),
        ..Default::default()
    };
    let mut orders_by_edge = BTreeMap::<(u32, u32), BTreeSet<u64>>::new();

    for source in sources
        .into_iter()
        .filter(|source| active_local_orders.contains(&source.order_id))
    {
        let Some(coordinate) = transport.id_to_coordinate.get(&source.cell_id).copied() else {
            continue;
        };
        projection
            .order_source_cells
            .entry(source.order_id)
            .or_default()
            .insert(coordinate);
    }

    for packet in packets.into_iter().filter(|packet| {
        u32::from(packet.owner_player_id) == view.local_player
            && active_local_orders.contains(&packet.order_id)
    }) {
        let Some(current) = transport
            .id_to_coordinate
            .get(&packet.current_cell)
            .copied()
        else {
            continue;
        };
        let active_cell = projection
            .active_strength_by_cell
            .entry(current)
            .or_default();
        *active_cell = active_cell.saturating_add(packet.infantry);
        let order_cell = projection
            .order_strength_by_cell
            .entry(packet.order_id)
            .or_default()
            .entry(current)
            .or_default();
        *order_cell = order_cell.saturating_add(packet.infantry);

        if let Some(destination) = transport
            .id_to_coordinate
            .get(&packet.destination_cell)
            .copied()
        {
            projection
                .destination_claims_by_order
                .entry(packet.order_id)
                .or_default()
                .insert(destination);
        }

        let next_index = packet.route_index as usize + 1;
        if let Some(&next) = packet.route.get(next_index) {
            orders_by_edge
                .entry((packet.current_cell, next))
                .or_default()
                .insert(packet.order_id);
        }
    }

    let active_local_internal_orders = orders
        .into_iter()
        .filter(|order| {
            order.status == OrderStatus::Active
                && u32::from(order.player_id) == view.local_player
                && matches!(
                    order.kind,
                    match_bindings::OrderKind::Balance
                        | match_bindings::OrderKind::FrontLoad
                        | match_bindings::OrderKind::CoreLoad
                        | match_bindings::OrderKind::PerimeterLoad
                        | match_bindings::OrderKind::Reshape
                )
        })
        .map(|order| order.order_id)
        .collect::<BTreeSet<_>>();
    for destination in destinations.into_iter().filter(|destination| {
        active_local_orders.contains(&destination.order_id)
            && destination.target_infantry > destination.received_infantry
    }) {
        let Some(coordinate) = transport
            .id_to_coordinate
            .get(&destination.cell_id)
            .copied()
        else {
            continue;
        };
        projection
            .destination_claims_by_order
            .entry(destination.order_id)
            .or_default()
            .insert(coordinate);
        if !active_local_internal_orders.contains(&destination.order_id) {
            continue;
        }
        let reserved = projection
            .destination_reservations_by_order
            .entry(destination.order_id)
            .or_default()
            .entry(coordinate)
            .or_default();
        *reserved = reserved.saturating_add(
            destination
                .target_infantry
                .saturating_sub(destination.received_infantry),
        );
    }

    for front in fronts.into_iter().filter(|front| {
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
        let Some(order_ids) = orders_by_edge.get(&(front.from_cell, front.to_cell)) else {
            continue;
        };
        projection
            .handle_orders
            .entry(handle)
            .or_default()
            .extend(order_ids);
    }

    projection.active_order_ids = projection.order_strength_by_cell.keys().copied().collect();
    projection
}

fn reducer_failure(result: Result<Result<(), String>, InternalError>) -> Option<String> {
    match result {
        Ok(Ok(())) => None,
        Ok(Err(reason)) => Some(reason),
        Err(error) => Some(error.to_string()),
    }
}

fn load_token(path: &Path) -> io::Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(token) => {
            let token = token.trim().to_owned();
            Ok((!token.is_empty()).then_some(token))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn save_token(path: &Path, token: &str) -> io::Result<()> {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> ClientConfig {
        ClientConfig {
            offline: false,
            host: "http://127.0.0.1:3000".to_owned(),
            database: "test".to_owned(),
            preferred_player: 1,
            display_name: "Test".to_owned(),
            profile: "test".to_owned(),
            debug_policy_flows: false,
        }
    }

    fn test_order(
        order_id: u64,
        client_command_id: u64,
        kind: match_bindings::OrderKind,
    ) -> TransferOrder {
        TransferOrder {
            order_id,
            player_id: 1,
            client_command_id,
            kind,
            status: OrderStatus::Active,
            requested_infantry: 20,
            committed_infantry: 20,
            in_transit_infantry: 20,
            delivered_infantry: 0,
            casualty_infantry: 0,
            orientation_q: 0,
            orientation_r: 0,
            created_step: 1,
            updated_step: 1,
        }
    }

    #[test]
    fn tactical_row_changes_coalesce_by_primary_key() {
        let signals = SharedSignals::default();
        let mut cache = TacticalCache::default();
        let mut packet = TransitPacket {
            packet_key: "7:10:11".to_owned(),
            order_id: 7,
            owner_player_id: 1,
            origin_cell: 10,
            current_cell: 10,
            destination_cell: 11,
            infantry: 20,
            route_index: 0,
            route: vec![10, 11],
            updated_step: 1,
        };
        record_change(
            &signals.packet_changes,
            packet.packet_key.clone(),
            Some(packet.clone()),
        );
        packet.infantry = 12;
        record_change(
            &signals.packet_changes,
            packet.packet_key.clone(),
            Some(packet.clone()),
        );

        cache.apply(signals.take_tactical_changes());
        assert_eq!(cache.packets.len(), 1);
        assert_eq!(cache.packets[&packet.packet_key].infantry, 12);

        record_change(&signals.packet_changes, packet.packet_key.clone(), None);
        cache.apply(signals.take_tactical_changes());
        assert!(cache.packets.is_empty());
    }

    #[test]
    fn front_load_transport_preserves_exact_fixed_point_orientation() {
        let continuous_heading = Axial::new(1_024, 375);
        assert_eq!(
            validated_front_load_orientation(Some(continuous_heading)),
            Some(continuous_heading)
        );
    }

    #[test]
    fn empty_front_load_direction_is_rejected() {
        assert_eq!(validated_front_load_orientation(Some(Axial::ZERO)), None);
        assert_eq!(validated_front_load_orientation(None), None);
    }

    #[test]
    fn expand_wave_edge_packets_project_while_resting_packets_are_omitted() {
        let mut transport = OnlineTransport::new(test_config());
        transport.id_to_coordinate.insert(10, Axial::ZERO);
        transport.id_to_coordinate.insert(11, Axial::new(1, 0));
        transport.id_to_coordinate.insert(12, Axial::new(0, 1));

        let mut view = MatchView::connecting(1);
        for (coordinate, owner) in [(Axial::new(1, 0), None), (Axial::new(0, 1), Some(2))] {
            view.cells.insert(
                coordinate,
                CellView {
                    coordinate,
                    terrain: TerrainKind::Plains,
                    elevation: 0,
                    owner,
                    civilians: 0,
                    infantry: 0,
                    military_capacity: 100,
                    blocked: false,
                },
            );
        }

        let edge = TransitPacket {
            packet_key: "7:10:11".to_owned(),
            order_id: 7,
            owner_player_id: 1,
            origin_cell: u32::MAX,
            current_cell: 10,
            destination_cell: 11,
            infantry: 7,
            route_index: 0,
            route: vec![10, 11],
            updated_step: 1,
        };
        let hostile_edge = TransitPacket {
            packet_key: "8:10:12".to_owned(),
            order_id: 8,
            owner_player_id: 1,
            origin_cell: 10,
            current_cell: 10,
            destination_cell: 12,
            infantry: 9,
            route_index: 0,
            route: vec![10, 12],
            updated_step: 1,
        };
        let resting = TransitPacket {
            packet_key: "7:11:11".to_owned(),
            order_id: 7,
            owner_player_id: 1,
            origin_cell: u32::MAX,
            current_cell: 11,
            destination_cell: 11,
            infantry: 8,
            route_index: 0,
            route: vec![11],
            updated_step: 1,
        };

        let orders = [
            test_order(7, 7, match_bindings::OrderKind::ExpandClusters),
            test_order(8, 8, match_bindings::OrderKind::AttackClusters),
        ];
        let flows = packets_to_flows(
            &transport,
            &view,
            &[edge, hostile_edge, resting],
            &orders,
            false,
        );
        assert_eq!(flows.len(), 2);
        assert_eq!(flows[0].route, vec![Axial::ZERO, Axial::new(1, 0)]);
        assert!(!flows[0].attacking, "neutral expansion is not hostile");
        assert!(flows[1].attacking, "enemy-targeted movement stays hostile");
    }

    #[test]
    fn internal_distribution_flows_are_debug_only_regardless_of_command_origin() {
        let mut transport = OnlineTransport::new(test_config());
        transport.id_to_coordinate.insert(10, Axial::ZERO);
        transport.id_to_coordinate.insert(11, Axial::new(1, 0));
        let view = MatchView::connecting(1);
        let packet = |order_id, infantry| TransitPacket {
            packet_key: format!("{order_id}:10:11"),
            order_id,
            owner_player_id: 1,
            origin_cell: 10,
            current_cell: 10,
            destination_cell: 11,
            infantry,
            route_index: 0,
            route: vec![10, 11],
            updated_step: 1,
        };
        let packets = [
            packet(20, 20),
            packet(21, 21),
            packet(22, 22),
            // Packet callbacks can precede their order callback on a busy
            // frame. Unknown packet provenance is never player-visible.
            packet(23, 23),
        ];
        let orders = [
            test_order(20, 0, match_bindings::OrderKind::PerimeterLoad),
            // A receipt-bearing legacy redistribution still represents the
            // same noisy internal logistics and stays behind the diagnostic.
            test_order(21, 21, match_bindings::OrderKind::PerimeterLoad),
            test_order(22, 22, match_bindings::OrderKind::ExpandClusters),
        ];

        let normal = packets_to_flows(&transport, &view, &packets, &orders, false);
        assert_eq!(
            normal.iter().map(|flow| flow.strength).collect::<Vec<_>>(),
            vec![22]
        );

        let debug = packets_to_flows(&transport, &view, &packets, &orders, true);
        assert_eq!(
            debug.iter().map(|flow| flow.strength).collect::<Vec<_>>(),
            vec![20, 21, 22]
        );
    }

    #[cfg(debug_assertions)]
    #[test]
    fn f4_toggles_policy_routes_and_requests_an_immediate_reprojection() {
        let transport = OnlineTransport::new(test_config());
        let signals = Arc::clone(&transport.signals);
        let mut app = App::new();
        app.insert_resource(ButtonInput::<KeyCode>::default())
            .insert_resource(transport)
            .insert_resource(MatchView::connecting(1))
            .add_systems(Update, toggle_policy_flow_debug);

        assert!(
            !app.world()
                .resource::<OnlineTransport>()
                .config
                .debug_policy_flows,
            "policy routes must start hidden"
        );
        assert_eq!(signals.take_dirty(), 0);

        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .press(POLICY_FLOW_DEBUG_KEY);
        app.update();

        assert!(
            app.world()
                .resource::<OnlineTransport>()
                .config
                .debug_policy_flows
        );
        assert_eq!(signals.take_dirty(), FLOWS_DIRTY | ORDERS_DIRTY);
        assert_eq!(
            app.world()
                .resource::<MatchView>()
                .toast
                .as_ref()
                .map(|toast| toast.text.as_str()),
            Some("DEBUG · policy routes ON · F4 to hide")
        );

        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .clear_just_pressed(POLICY_FLOW_DEBUG_KEY);
        app.update();
        assert!(
            app.world()
                .resource::<OnlineTransport>()
                .config
                .debug_policy_flows,
            "holding F4 must not retrigger the toggle"
        );
        assert_eq!(signals.take_dirty(), 0);

        app.world_mut()
            .resource_mut::<MatchView>()
            .active_flows
            .push(ActiveFlow {
                route: vec![Axial::ZERO, Axial::new(1, 0)],
                strength: 20,
                attacking: false,
                age: 0.0,
                lifetime: 60.0,
            });
        {
            let mut keyboard = app.world_mut().resource_mut::<ButtonInput<KeyCode>>();
            keyboard.reset(POLICY_FLOW_DEBUG_KEY);
            keyboard.press(POLICY_FLOW_DEBUG_KEY);
        }
        app.update();

        assert!(
            !app.world()
                .resource::<OnlineTransport>()
                .config
                .debug_policy_flows
        );
        assert!(
            app.world().resource::<MatchView>().active_flows.is_empty(),
            "turning the diagnostic off must clear stale trails without authority"
        );
        assert_eq!(signals.take_dirty(), FLOWS_DIRTY | ORDERS_DIRTY);
        assert_eq!(
            app.world()
                .resource::<MatchView>()
                .toast
                .as_ref()
                .map(|toast| toast.text.as_str()),
            Some("DEBUG · policy routes OFF · F4 to show")
        );
    }

    #[test]
    fn authoritative_flow_sync_clears_stale_and_unknown_policy_routes() {
        let mut transport = OnlineTransport::new(test_config());
        transport.id_to_coordinate.insert(10, Axial::ZERO);
        transport.id_to_coordinate.insert(11, Axial::new(1, 0));
        let mut view = MatchView::connecting(1);
        view.active_flows.push(ActiveFlow {
            route: vec![Axial::ZERO, Axial::new(1, 0)],
            strength: 99,
            attacking: false,
            age: 8.0,
            lifetime: 60.0,
        });
        let packet = TransitPacket {
            packet_key: "24:10:11".to_owned(),
            order_id: 24,
            owner_player_id: 1,
            origin_cell: 10,
            current_cell: 10,
            destination_cell: 11,
            infantry: 24,
            route_index: 0,
            route: vec![10, 11],
            updated_step: 1,
        };

        // This models the packet-first callback window: replacement must
        // clear the old projection rather than retaining or exposing either
        // route while the matching order row is unavailable.
        replace_authoritative_flows(&transport, &mut view, &[packet], &[], false);
        assert!(view.active_flows.is_empty());
    }

    #[test]
    fn combat_fronts_project_a_percentage_contested_cell() {
        let mut transport = OnlineTransport::new(test_config());
        let from = Axial::ZERO;
        let to = Axial::new(1, 0);
        transport.id_to_coordinate.insert(10, from);
        transport.id_to_coordinate.insert(12, Axial::new(0, 1));
        transport.id_to_coordinate.insert(11, to);
        let mut view = MatchView::connecting(1);
        view.cells.insert(
            to,
            CellView {
                coordinate: to,
                terrain: TerrainKind::Plains,
                elevation: 0,
                owner: Some(2),
                civilians: 0,
                infantry: 65,
                military_capacity: 100,
                blocked: false,
            },
        );

        let first = CombatFront {
            front_key: "10:11:1".to_owned(),
            attacker_player_id: 1,
            defender_player_id: 2,
            from_cell: 10,
            to_cell: 11,
            queued_infantry: 30,
            attacker_engaged: 10,
            defender_engaged: 10,
            attacker_casualties: 5,
            defender_casualties: 4,
            frontage: 25,
            uphill: false,
            logical_step: 7,
        };
        let second = CombatFront {
            front_key: "12:11:1".to_owned(),
            from_cell: 12,
            queued_infantry: 15,
            attacker_engaged: 0,
            attacker_casualties: 0,
            ..first.clone()
        };
        let (overlays, contested) = fronts_to_overlays(&transport, &view, &[first, second]);

        assert_eq!(overlays.len(), 2);
        let contest = contested.get(&to).expect("target should be contested");
        assert_eq!(contest.controller_player, 2);
        assert_eq!(contest.attacker_player, 1);
        assert_eq!(contest.attacker_strength, 50);
        assert!((contest.attacker_share - (50.0 / 115.0)).abs() < f32::EPSILON);
    }

    #[test]
    fn cluster_policy_transport_preserves_facing_and_rejects_invalid_combinations() {
        assert_eq!(
            remote_cluster_policy(ClusterPolicy::Balanced, None),
            Ok((RemoteClusterPolicyKind::Balanced, Axial::ZERO))
        );
        assert_eq!(
            remote_cluster_policy(ClusterPolicy::Directional, Some(Axial::new(512, -341))),
            Ok((RemoteClusterPolicyKind::Directional, Axial::new(512, -341)))
        );
        assert!(remote_cluster_policy(ClusterPolicy::Directional, None).is_err());
        assert!(remote_cluster_policy(ClusterPolicy::Center, Some(Axial::new(1, 0))).is_err());
    }

    #[test]
    fn policy_snapshot_maps_only_the_bound_players_authoritative_rows() {
        let mut transport = OnlineTransport::new(test_config());
        let local = Axial::new(2, -1);
        let foreign = Axial::new(3, -1);
        transport.id_to_coordinate.insert(20, local);
        transport.id_to_coordinate.insert(21, foreign);
        let mut view = MatchView::connecting(1);

        update_cluster_policies(
            &transport,
            &mut view,
            &[
                ClusterPolicyAssignment {
                    cell_id: 20,
                    owner_player_id: 1,
                    kind: RemoteClusterPolicyKind::Directional,
                    orientation_q: 700,
                    orientation_r: -250,
                    revision: 8,
                },
                ClusterPolicyAssignment {
                    cell_id: 21,
                    owner_player_id: 2,
                    kind: RemoteClusterPolicyKind::Perimeter,
                    orientation_q: 0,
                    orientation_r: 0,
                    revision: 9,
                },
            ],
        );

        assert_eq!(
            view.cluster_policies.get(&local),
            Some(&ClusterPolicyView {
                kind: ClusterPolicy::Directional,
                orientation: Axial::new(700, -250),
                revision: 8,
            })
        );
        assert!(!view.cluster_policies.contains_key(&foreign));
    }

    #[test]
    fn retask_projection_maps_a_contested_handle_to_all_current_order_cells() {
        let mut transport = OnlineTransport::new(test_config());
        let rear = Axial::ZERO;
        let front = Axial::new(1, 0);
        let handle = Axial::new(2, 0);
        for (cell_id, coordinate) in [(10, rear), (11, front), (12, handle)] {
            transport.id_to_coordinate.insert(cell_id, coordinate);
        }

        let mut view = MatchView::connecting(1);
        for (coordinate, owner, infantry) in [
            (rear, Some(1), 30),
            (front, Some(1), 20),
            (handle, Some(2), 40),
        ] {
            view.cells.insert(
                coordinate,
                CellView {
                    coordinate,
                    terrain: TerrainKind::Plains,
                    elevation: 0,
                    owner,
                    civilians: 0,
                    infantry,
                    military_capacity: 100,
                    blocked: false,
                },
            );
        }

        let combat = CombatFront {
            front_key: "11:12:1".to_owned(),
            attacker_player_id: 1,
            defender_player_id: 2,
            from_cell: 11,
            to_cell: 12,
            queued_infantry: 12,
            attacker_engaged: 5,
            defender_engaged: 5,
            attacker_casualties: 0,
            defender_casualties: 0,
            frontage: 25,
            uphill: false,
            logical_step: 7,
        };
        let (_, contested) = fronts_to_overlays(&transport, &view, std::slice::from_ref(&combat));
        view.set_contested_cells(contested);

        let order = |order_id, player_id, status, kind| TransferOrder {
            order_id,
            player_id,
            client_command_id: order_id,
            kind,
            status,
            requested_infantry: 20,
            committed_infantry: 20,
            in_transit_infantry: 20,
            delivered_infantry: 0,
            casualty_infantry: 0,
            orientation_q: 1,
            orientation_r: 0,
            created_step: 1,
            updated_step: 7,
        };
        let packet = |packet_key: &str,
                      order_id,
                      current_cell,
                      destination_cell,
                      infantry,
                      route: Vec<u32>| TransitPacket {
            packet_key: packet_key.to_owned(),
            order_id,
            owner_player_id: 1,
            origin_cell: 10,
            current_cell,
            destination_cell,
            infantry,
            route_index: 0,
            route,
            updated_step: 7,
        };
        let orders = [
            order(
                7,
                1,
                OrderStatus::Active,
                match_bindings::OrderKind::PushFront,
            ),
            order(
                8,
                1,
                OrderStatus::Active,
                match_bindings::OrderKind::PushFront,
            ),
            order(
                9,
                1,
                OrderStatus::Completed,
                match_bindings::OrderKind::Balance,
            ),
            order(
                10,
                1,
                OrderStatus::Active,
                match_bindings::OrderKind::Balance,
            ),
            order(
                11,
                2,
                OrderStatus::Active,
                match_bindings::OrderKind::Reshape,
            ),
        ];
        let packets = [
            packet("7:11:12", 7, 11, 12, 12, vec![11, 12]),
            packet("7:10:10", 7, 10, 10, 8, vec![10]),
            packet("8:10:10", 8, 10, 10, 5, vec![10]),
            packet("9:10:10", 9, 10, 10, 99, vec![10]),
        ];
        let destinations = [
            TransferDestination {
                destination_key: "7:12".to_owned(),
                order_id: 7,
                cell_id: 12,
                target_infantry: 20,
                received_infantry: 0,
            },
            TransferDestination {
                destination_key: "9:11".to_owned(),
                order_id: 9,
                cell_id: 11,
                target_infantry: 20,
                received_infantry: 0,
            },
            TransferDestination {
                destination_key: "10:11".to_owned(),
                order_id: 10,
                cell_id: 11,
                target_infantry: 30,
                received_infantry: 12,
            },
            TransferDestination {
                destination_key: "11:10".to_owned(),
                order_id: 11,
                cell_id: 10,
                target_infantry: 20,
                received_infantry: 0,
            },
        ];
        let sources = [
            TransferSource {
                source_key: "7:10".to_owned(),
                order_id: 7,
                cell_id: 10,
                committed_infantry: 20,
                queued_infantry: 0,
            },
            TransferSource {
                source_key: "8:10".to_owned(),
                order_id: 8,
                cell_id: 10,
                committed_infantry: 5,
                queued_infantry: 0,
            },
        ];

        let projection = retask_projection_from_authority(
            &transport,
            &view,
            &orders,
            &packets,
            &[combat],
            &sources,
            &destinations,
        );

        assert_eq!(
            projection.handle_orders,
            BTreeMap::from([(handle, BTreeSet::from([7]))])
        );
        assert_eq!(projection.active_order_ids, BTreeSet::from([7, 8]));
        assert_eq!(
            projection.order_source_cells,
            BTreeMap::from([(7, BTreeSet::from([rear])), (8, BTreeSet::from([rear])),])
        );
        assert_eq!(
            projection.order_strength_by_cell[&7],
            BTreeMap::from([(rear, 8), (front, 12)])
        );
        assert_eq!(
            projection.active_strength_by_cell,
            BTreeMap::from([(rear, 13), (front, 12)])
        );
        assert_eq!(
            projection.destination_reservations_by_order,
            BTreeMap::from([(10, BTreeMap::from([(front, 18)]))])
        );
        assert_eq!(
            projection.destination_claims_by_order,
            BTreeMap::from([
                (7, BTreeSet::from([rear, handle])),
                (8, BTreeSet::from([rear])),
                (10, BTreeSet::from([front])),
            ])
        );
    }

    #[test]
    fn retask_projection_classifies_only_internal_active_policy_orders() {
        let transport = OnlineTransport::new(test_config());
        let view = MatchView::connecting(1);
        let order = |order_id, player_id, client_command_id, status, kind| TransferOrder {
            order_id,
            player_id,
            client_command_id,
            kind,
            status,
            requested_infantry: 20,
            committed_infantry: 20,
            in_transit_infantry: 20,
            delivered_infantry: 0,
            casualty_infantry: 0,
            orientation_q: 0,
            orientation_r: 0,
            created_step: 1,
            updated_step: 1,
        };
        let orders = [
            order(
                1,
                1,
                0,
                OrderStatus::Active,
                match_bindings::OrderKind::Balance,
            ),
            order(
                2,
                1,
                2,
                OrderStatus::Active,
                match_bindings::OrderKind::FrontLoad,
            ),
            order(
                3,
                1,
                0,
                OrderStatus::Completed,
                match_bindings::OrderKind::CoreLoad,
            ),
            order(
                4,
                2,
                0,
                OrderStatus::Active,
                match_bindings::OrderKind::PerimeterLoad,
            ),
            order(
                5,
                1,
                0,
                OrderStatus::Active,
                match_bindings::OrderKind::PushFront,
            ),
        ];

        let projection =
            retask_projection_from_authority(&transport, &view, &orders, &[], &[], &[], &[]);

        assert_eq!(projection.background_policy_order_ids, BTreeSet::from([1]));
    }

    #[test]
    fn delayed_receipt_advances_command_allocator_before_deduplication() {
        let mut transport = OnlineTransport::new(test_config());
        transport.next_command_id = 40;
        transport.command_ids_ready = true;
        transport.terminal_command_ids.insert(72);

        transport.observe_command_id(72);
        assert_eq!(transport.next_command_id, 73);
        assert!(transport.command_ids_ready);

        transport.observe_command_id(12);
        assert_eq!(transport.next_command_id, 73);

        transport.observe_command_id(u64::MAX);
        assert!(!transport.command_ids_ready);
        assert_eq!(transport.allocate_command_id(), None);
    }

    #[test]
    fn invalid_host_disables_connection_without_entering_sdk_builder() {
        let mut config = test_config();
        config.host = "http://[broken".to_owned();
        let mut transport = OnlineTransport::new(config);
        let mut view = MatchView::connecting(1);

        connect_to_spacetimedb(&mut transport, &mut view);

        assert!(transport.connection_disabled);
        assert!(transport.connection.is_none());
        assert!(!transport.subscription_ready);
        assert!(matches!(
            view.authority,
            AuthorityState::ConnectionUnavailable { .. }
        ));
    }

    #[test]
    fn relative_host_is_rejected_before_entering_sdk_builder() {
        let mut config = test_config();
        config.host = "localhost".to_owned();
        let mut transport = OnlineTransport::new(config);
        let mut view = MatchView::connecting(1);

        connect_to_spacetimedb(&mut transport, &mut view);

        assert!(transport.connection_disabled);
        assert!(transport.connection.is_none());
    }

    #[test]
    fn join_failure_is_terminal_and_preserves_the_authoritative_reason() {
        let mut transport = OnlineTransport::new(test_config());
        transport.subscription_ready = true;
        transport.command_ids_ready = true;
        transport.bound_player = Some(1);
        let mut view = MatchView::connecting(1);

        mark_join_failed(
            &mut transport,
            &mut view,
            "both player slots are already claimed".to_owned(),
        );

        assert!(transport.subscription_ready);
        assert!(!transport.command_ids_ready);
        assert_eq!(transport.bound_player, None);
        assert_eq!(
            view.authority,
            AuthorityState::SlotUnavailable {
                reason: "both player slots are already claimed".to_owned()
            }
        );
        assert_eq!(
            view.authority.command_block_reason(),
            "Player slot unavailable: both player slots are already claimed"
        );
        assert_eq!(
            view.latest_result,
            "SLOT UNAVAILABLE · both player slots are already claimed"
        );
    }

    #[test]
    fn slot_status_distinguishes_unknown_open_and_claimed_offline() {
        let mut transport = OnlineTransport::new(test_config());
        let mut view = MatchView::connecting(1);

        update_players(&mut transport, &mut view, None, &[]);
        assert_eq!(view.connection, [ConnectionState::Syncing; 2]);

        let players = [
            PlayerSlot {
                player_id: 1,
                identity: None,
                display_name: String::new(),
                connected: false,
                has_reconnected: false,
                reconnect_count: 0,
                ready: false,
                joined_at_us: 0,
                last_seen_at_us: 0,
            },
            PlayerSlot {
                player_id: 2,
                identity: Some(spacetimedb_sdk::Identity::ZERO),
                display_name: "Previous player".to_owned(),
                connected: false,
                has_reconnected: true,
                reconnect_count: 1,
                ready: true,
                joined_at_us: 1,
                last_seen_at_us: 2,
            },
        ];
        update_players(&mut transport, &mut view, None, &players);

        assert_eq!(view.connection[0], ConnectionState::Open);
        assert_eq!(view.connection[1], ConnectionState::ClaimedOffline);
        assert_eq!(view.connection[0].label(), "OPEN SLOT");
        assert_eq!(view.connection[1].label(), "CLAIMED OFFLINE");
    }

    #[test]
    fn impassable_water_does_not_receive_a_blocked_land_overlay() {
        let terrain = CellTerrain {
            cell_id: 1,
            q: 0,
            r: 0,
            chunk_q: 0,
            chunk_r: 0,
            terrain: match_bindings::TerrainClass::Water,
            elevation: 0,
            passable: false,
            capturable: false,
            habitable: false,
        };

        assert!(!cell_view_from_rows(Axial::ZERO, &terrain, None).blocked);
    }

    #[test]
    fn terrain_rebuild_retains_authoritative_capturability() {
        let mut transport = OnlineTransport::new(test_config());
        let mut view = MatchView::connecting(1);
        let mut terrain = CellTerrain {
            cell_id: 1,
            q: 0,
            r: 0,
            chunk_q: 0,
            chunk_r: 0,
            terrain: match_bindings::TerrainClass::Plains,
            elevation: 0,
            passable: true,
            capturable: false,
            habitable: true,
        };

        rebuild_cells(&mut transport, &mut view, vec![terrain.clone()], None);
        assert!(view.cell(Axial::ZERO).is_some_and(|cell| !cell.blocked));
        assert!(!view.is_capturable(Axial::ZERO));

        terrain.capturable = true;
        rebuild_cells(&mut transport, &mut view, vec![terrain], None);
        assert!(view.is_capturable(Axial::ZERO));
    }

    #[test]
    fn civilian_only_updates_invalidate_the_render_chunk() {
        let mut transport = OnlineTransport::new(test_config());
        let mut view = MatchView::offline_fixture();
        let coordinate = *view
            .cells
            .keys()
            .find(|coordinate| view.cell(**coordinate).is_some_and(CellView::is_land))
            .expect("fixture land cell");
        let original = view.cell(coordinate).expect("indexed fixture cell").clone();
        transport.id_to_coordinate.insert(42, coordinate);
        view.dirty_chunks.clear();
        let planning_revision = view.planning_revision;

        update_cells(
            &transport,
            &mut view,
            &[CellState {
                cell_id: 42,
                owner_player_id: original.owner.unwrap_or_default() as u8,
                civilians: original.civilians + 1,
                civilian_capacity: original.civilians + 100,
                infantry: original.infantry,
                military_capacity: original.military_capacity,
                last_changed_step: 1,
                last_policy_changed_step: 1,
            }],
            MapViewMode::Civilians,
        );

        assert_eq!(
            view.cell(coordinate).unwrap().civilians,
            original.civilians + 1
        );
        assert_eq!(view.dirty_chunks, BTreeSet::from([chunk_of(coordinate)]));
        assert_eq!(view.planning_revision, planning_revision);
    }

    #[test]
    fn civilian_only_updates_do_not_recolor_the_soldier_view() {
        let mut transport = OnlineTransport::new(test_config());
        let mut view = MatchView::offline_fixture();
        let coordinate = *view.cells.keys().next().expect("fixture cell");
        let original = view.cell(coordinate).expect("fixture cell").clone();
        transport.id_to_coordinate.insert(43, coordinate);
        view.dirty_chunks.clear();
        let planning_revision = view.planning_revision;

        update_cells(
            &transport,
            &mut view,
            &[CellState {
                cell_id: 43,
                owner_player_id: original.owner.unwrap_or_default() as u8,
                civilians: original.civilians + 1,
                civilian_capacity: original.civilians + 100,
                infantry: original.infantry,
                military_capacity: original.military_capacity,
                last_changed_step: 1,
                last_policy_changed_step: 1,
            }],
            MapViewMode::Soldiers,
        );

        assert_eq!(
            view.cell(coordinate).unwrap().civilians,
            original.civilians + 1
        );
        assert!(view.dirty_chunks.is_empty());
        assert_eq!(view.planning_revision, planning_revision);
    }

    #[test]
    fn infantry_updates_preserve_ownership_revision_until_control_changes() {
        let mut transport = OnlineTransport::new(test_config());
        let mut view = MatchView::offline_fixture();
        let coordinate = *view
            .cells
            .keys()
            .find(|coordinate| view.is_local_owned(**coordinate))
            .expect("fixture owned cell");
        let original = view.cell(coordinate).expect("fixture cell").clone();
        transport.id_to_coordinate.insert(44, coordinate);
        let ownership_revision = view.ownership_revision;
        let planning_revision = view.planning_revision;

        let state = |owner_player_id, infantry| CellState {
            cell_id: 44,
            owner_player_id,
            civilians: original.civilians,
            civilian_capacity: original.civilians,
            infantry,
            military_capacity: original.military_capacity,
            last_changed_step: 1,
            last_policy_changed_step: 1,
        };
        update_cells(
            &transport,
            &mut view,
            &[state(1, original.infantry.saturating_add(1))],
            MapViewMode::Soldiers,
        );
        assert_eq!(view.ownership_revision, ownership_revision);
        assert_eq!(view.planning_revision, planning_revision.wrapping_add(1));

        update_cells(
            &transport,
            &mut view,
            &[state(2, original.infantry.saturating_add(1))],
            MapViewMode::Soldiers,
        );
        assert_eq!(view.ownership_revision, ownership_revision.wrapping_add(1));
    }

    #[cfg(unix)]
    #[test]
    fn token_save_is_atomic_and_repairs_private_permissions() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let directory = std::env::temp_dir().join(format!(
            "of-token-test-{}-{}",
            process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        fs::create_dir(&directory).expect("create token test directory");
        let path = directory.join("client.token");
        fs::write(&path, "old").expect("seed existing token");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644))
            .expect("make existing token permissive");

        save_token(&path, "replacement").expect("save replacement token");

        assert_eq!(
            fs::read_to_string(&path).expect("read token"),
            "replacement"
        );
        assert_eq!(
            fs::metadata(&path).expect("token metadata").mode() & 0o777,
            0o600
        );
        fs::remove_file(&path).expect("remove test token");
        fs::remove_dir(&directory).expect("remove token test directory");
    }
}
