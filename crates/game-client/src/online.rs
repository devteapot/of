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
use match_bindings::{
    CellState, CellStateTableAccess, CellTerrain, CellTerrainTableAccess, CombatFront,
    CombatFrontTableAccess, CommandReceipt, CommandReceiptTableAccess, DbConnection, MatchConfig,
    MatchConfigTableAccess, MatchPhase as RemoteMatchPhase, MatchState, MatchStateTableAccess,
    MobilizationPolicy, MobilizationPolicyTableAccess, OrderStatus, PlayerSlot,
    PlayerSlotTableAccess, ReceiptStatus, TransferDestinationTableAccess, TransferOrder,
    TransferOrderTableAccess, TransferSource, TransferSourceTableAccess, TransitPacket,
    TransitPacketTableAccess, issue_balance, issue_front_load, issue_push_front, issue_transfer,
    join_match, set_mobilization_target,
};
use spacetimedb_sdk::__codegen::InternalError;
use spacetimedb_sdk::{DbContext, Table, TableWithPrimaryKey};

use crate::{
    config::ClientConfig,
    geometry::{HEX_RADIUS, chunk_of},
    map_view::MapViewMode,
    model::{
        ActiveFlow, ActiveFront, AuthorityState, CellView, ConnectionState, MatchPhase, MatchView,
        ToastKind,
    },
    network::{ClientIntent, NetworkSet, RedistributionPreset, ServerUpdate},
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
    events: Mutex<VecDeque<LifecycleEvent>>,
}

impl SharedSignals {
    fn mark(&self, bits: u32) {
        self.dirty.fetch_or(bits, Ordering::Release);
    }

    fn take_dirty(&self) -> u32 {
        self.dirty.swap(0, Ordering::AcqRel)
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

#[derive(Clone, Debug)]
enum PendingCommand {
    PushFront,
    Transfer,
    Balance,
    FrontLoad,
    Mobilization { target: f32 },
}

impl PendingCommand {
    const fn label(&self) -> &'static str {
        match self {
            Self::PushFront => "Push Front",
            Self::Transfer => "Transfer",
            Self::Balance => "Balance",
            Self::FrontLoad => "Front-load",
            Self::Mobilization { .. } => "Mobilization",
        }
    }

    const fn receipt_name(&self) -> &'static str {
        match self {
            Self::PushFront => "issue_push_front",
            Self::Transfer => "issue_transfer",
            Self::Balance => "issue_balance",
            Self::FrontLoad => "issue_front_load",
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
                .subscribe_to_all_tables();
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
    watch_table!(connection.db.cell_state(), CELLS_DIRTY);
    watch_table!(connection.db.match_config(), MATCH_DIRTY);
    watch_table!(connection.db.match_state(), MATCH_DIRTY);
    watch_table!(connection.db.player_slot(), PLAYERS_DIRTY);
    watch_table!(connection.db.mobilization_policy(), MOBILIZATION_DIRTY);
    watch_table!(connection.db.command_receipt(), RECEIPTS_DIRTY);
    watch_table!(connection.db.transit_packet(), FLOWS_DIRTY);
    watch_table!(connection.db.combat_front(), FRONTS_DIRTY);
    watch_table!(connection.db.transfer_order(), ORDERS_DIRTY);
    watch_table!(connection.db.transfer_source(), ORDERS_DIRTY);
    watch_table!(connection.db.transfer_destination(), ORDERS_DIRTY);
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
    view: &MatchView,
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
        ClientIntent::PushFront {
            sources,
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
                    callback,
                )
                .map_err(|error| error.to_string())?;
            Ok(PendingCommand::PushFront)
        }
        ClientIntent::Transfer {
            sources,
            destinations,
            amount_percent,
        } => {
            let source_ids = ids_for_selection(transport, sources)?;
            let destination_ids = ids_for_selection(transport, destinations)?;
            let percentage = u64::from((*amount_percent).clamp(10, 100));
            let infantry = sources
                .iter()
                .filter_map(|coordinate| view.cell(*coordinate))
                .map(|cell| cell.infantry.saturating_mul(percentage) / 100)
                .sum();
            connection
                .reducers
                .issue_transfer_then(command_id, source_ids, destination_ids, infantry, callback)
                .map_err(|error| error.to_string())?;
            Ok(PendingCommand::Transfer)
        }
        ClientIntent::Redistribute {
            cells,
            preset: RedistributionPreset::Balance,
            ..
        } => {
            connection
                .reducers
                .issue_balance_then(command_id, ids_for_selection(transport, cells)?, callback)
                .map_err(|error| error.to_string())?;
            Ok(PendingCommand::Balance)
        }
        ClientIntent::Redistribute {
            cells,
            preset: RedistributionPreset::FrontLoad,
            direction,
        } => {
            let (orientation_q, orientation_r) = direction
                .and_then(world_direction_to_axial)
                .ok_or_else(|| "Front-load direction is too short".to_owned())?;
            connection
                .reducers
                .issue_front_load_then(
                    command_id,
                    ids_for_selection(transport, cells)?,
                    orientation_q,
                    orientation_r,
                    callback,
                )
                .map_err(|error| error.to_string())?;
            Ok(PendingCommand::FrontLoad)
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

fn world_direction_to_axial(direction: Vec2) -> Option<(i32, i32)> {
    if direction.length_squared() < 0.0001 {
        return None;
    }
    let q = direction.x / (1.5 * HEX_RADIUS);
    let r = direction.y / (3.0_f32.sqrt() * HEX_RADIUS) - q * 0.5;
    let scale = 1_024.0 / q.abs().max(r.abs()).max(0.0001);
    let q = (q * scale).round() as i32;
    let r = (r * scale).round() as i32;
    (q != 0 || r != 0).then_some((q, r))
}

struct AuthoritySnapshot {
    identity: Option<spacetimedb_sdk::Identity>,
    terrain: Option<Vec<CellTerrain>>,
    cells: Option<Vec<CellState>>,
    config: Option<MatchConfig>,
    match_state: Option<MatchState>,
    players: Option<Vec<PlayerSlot>>,
    mobilization: Option<Vec<MobilizationPolicy>>,
    receipts: Option<Vec<CommandReceipt>>,
    packets: Option<Vec<TransitPacket>>,
    fronts: Option<Vec<CombatFront>>,
    orders: Option<Vec<TransferOrder>>,
    sources: Option<Vec<TransferSource>>,
}

impl AuthoritySnapshot {
    fn capture(connection: &DbConnection, dirty: u32) -> Self {
        let terrain_changed = dirty & TERRAIN_DIRTY != 0;
        Self {
            identity: connection.try_identity(),
            terrain: terrain_changed.then(|| connection.db.cell_terrain().iter().collect()),
            cells: (terrain_changed || dirty & CELLS_DIRTY != 0)
                .then(|| connection.db.cell_state().iter().collect()),
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
            packets: (dirty & (FLOWS_DIRTY | TERRAIN_DIRTY) != 0)
                .then(|| connection.db.transit_packet().iter().collect()),
            fronts: (dirty & (FRONTS_DIRTY | TERRAIN_DIRTY) != 0)
                .then(|| connection.db.combat_front().iter().collect()),
            orders: (dirty & ORDERS_DIRTY != 0)
                .then(|| connection.db.transfer_order().iter().collect()),
            sources: (dirty & ORDERS_DIRTY != 0)
                .then(|| connection.db.transfer_source().iter().collect()),
        }
    }
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
        AuthoritySnapshot::capture(connection, dirty)
    };

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
    if let Some(packets) = snapshot.packets {
        view.active_flows = packets_to_flows(&transport, &view, packets);
    }
    if let Some(fronts) = snapshot.fronts {
        view.active_fronts = fronts_to_overlays(&transport, &view, fronts);
    }
    if let (Some(orders), Some(sources)) = (snapshot.orders, snapshot.sources) {
        view.active_orders = orders
            .iter()
            .filter(|order| order.status == OrderStatus::Active)
            .count();
        view.queued_infantry = sources.iter().map(|source| source.queued_infantry).sum();
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
    transport.coordinate_to_id.clear();
    transport.id_to_coordinate.clear();
    for terrain in terrain_rows {
        let coordinate = Axial::new(terrain.q, terrain.r);
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
        let Some(cell) = view.cell_mut(coordinate) else {
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
        view.local_player = u32::from(slot.player_id);
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

fn packets_to_flows(
    transport: &OnlineTransport,
    view: &MatchView,
    packets: Vec<TransitPacket>,
) -> Vec<ActiveFlow> {
    packets
        .into_iter()
        .filter_map(|packet| {
            let route_index = packet.route_index as usize;
            let route = packet
                .route
                .get(route_index..)
                .unwrap_or_default()
                .iter()
                .filter_map(|cell_id| transport.id_to_coordinate.get(cell_id).copied())
                .collect::<Vec<_>>();
            if route.is_empty() {
                return None;
            }
            let attacking = transport
                .id_to_coordinate
                .get(&packet.destination_cell)
                .and_then(|coordinate| view.cell(*coordinate))
                .is_some_and(|cell| cell.owner != Some(u32::from(packet.owner_player_id)));
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

fn fronts_to_overlays(
    transport: &OnlineTransport,
    view: &MatchView,
    fronts: Vec<CombatFront>,
) -> Vec<ActiveFront> {
    fronts
        .into_iter()
        .filter_map(|front| {
            let from = transport.id_to_coordinate.get(&front.from_cell).copied()?;
            let to = transport.id_to_coordinate.get(&front.to_cell).copied()?;
            let (friendly, hostile) = if u32::from(front.attacker_player_id) == view.local_player {
                (from, to)
            } else if u32::from(front.defender_player_id) == view.local_player {
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
            Some(ActiveFront {
                friendly,
                hostile,
                intensity,
                age: 0.0,
            })
        })
        .collect()
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
        }
    }

    #[test]
    fn front_load_direction_preserves_axis_orientation() {
        let east = world_direction_to_axial(Vec2::X).expect("east direction");
        assert!(east.0 > 0);
        let north = world_direction_to_axial(Vec2::Y).expect("north direction");
        assert!(north.1 > 0);
    }

    #[test]
    fn empty_front_load_direction_is_rejected() {
        assert_eq!(world_direction_to_axial(Vec2::ZERO), None);
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
            }],
            MapViewMode::Civilians,
        );

        assert_eq!(
            view.cell(coordinate).unwrap().civilians,
            original.civilians + 1
        );
        assert_eq!(view.dirty_chunks, BTreeSet::from([chunk_of(coordinate)]));
    }

    #[test]
    fn civilian_only_updates_do_not_recolor_the_soldier_view() {
        let mut transport = OnlineTransport::new(test_config());
        let mut view = MatchView::offline_fixture();
        let coordinate = *view.cells.keys().next().expect("fixture cell");
        let original = view.cell(coordinate).expect("fixture cell").clone();
        transport.id_to_coordinate.insert(43, coordinate);
        view.dirty_chunks.clear();

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
            }],
            MapViewMode::Soldiers,
        );

        assert_eq!(
            view.cell(coordinate).unwrap().civilians,
            original.civilians + 1
        );
        assert!(view.dirty_chunks.is_empty());
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
