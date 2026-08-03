#![forbid(unsafe_code)]

use std::collections::{HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use clap::Parser;
use hex_core::Axial;
use match_bindings::{
    CellState, CellStateTableAccess, CellTerrain, CellTerrainTableAccess, CommandReceipt,
    CommandReceiptTableAccess, DbConnection, MatchPhase, MatchStateTableAccess,
    MobilizationPolicyTableAccess, OrderKind, OrderStatus, PlayerSlotTableAccess, ReceiptStatus,
    TransferDestinationTableAccess, TransferOrder, TransferOrderTableAccess,
    TransferSourceTableAccess, TransitPacket, TransitPacketTableAccess, issue_push_front as _,
    join_match as _, set_mobilization_target as _,
};
use spacetimedb_sdk::{DbContext, Identity, Table};

const PLAYER_ONE: u8 = 1;
const PLAYER_TWO: u8 = 2;
const SINGLETON_ID: u8 = 0;
const COMMAND_ID_FLOOR: u64 = 9_000_000_000;
const PUSH_COMMITMENT_BPS: u32 = 1_000;
const MAX_PUSH_CORRIDOR_CELLS: usize = 5;

#[derive(Debug, Parser)]
#[command(about = "Exercise a live V1 match with two persistent anonymous identities")]
struct Args {
    /// `SpacetimeDB` host URI.
    #[arg(long, default_value = "http://127.0.0.1:3000")]
    host: String,

    /// Published match database name or identity. The default is deliberately
    /// isolated from the interactive development match.
    #[arg(long, default_value = "of-match-e2e")]
    database: String,

    /// Directory for the two ignored identity token profiles.
    #[arg(long, default_value = ".match-e2e-tokens")]
    token_dir: PathBuf,

    /// Maximum time allowed for each asynchronous phase.
    #[arg(long, default_value_t = 30)]
    timeout_secs: u64,

    /// Client-cache polling interval.
    #[arg(long, default_value_t = 20)]
    poll_ms: u64,
}

enum LifecycleEvent {
    Connected { identity: Identity, token: String },
    Subscribed,
    Failed(String),
    Disconnected(Option<String>),
}

struct Client {
    label: &'static str,
    conn: DbConnection,
    identity: Identity,
    events: Receiver<LifecycleEvent>,
    pump: Option<JoinHandle<()>>,
    stopped: bool,
}

impl Client {
    fn connect(
        label: &'static str,
        token_path: &Path,
        host: &str,
        database: &str,
        timeout: Duration,
    ) -> Result<Self> {
        let existing_token = read_token(token_path)?;
        let (event_tx, event_rx) = mpsc::channel();
        let connected_tx = event_tx.clone();
        let connect_error_tx = event_tx.clone();
        let disconnect_tx = event_tx;

        let conn = DbConnection::builder()
            .with_uri(host)
            .with_database_name(database)
            .with_token(existing_token)
            .on_connect(move |ctx, identity, token| {
                let _ = connected_tx.send(LifecycleEvent::Connected {
                    identity,
                    token: token.to_owned(),
                });
                let applied_tx = connected_tx.clone();
                let subscription_error_tx = connected_tx.clone();
                ctx.subscription_builder()
                    .on_applied(move |_| {
                        let _ = applied_tx.send(LifecycleEvent::Subscribed);
                    })
                    .on_error(move |_, error| {
                        let _ = subscription_error_tx.send(LifecycleEvent::Failed(format!(
                            "subscription failed: {error}"
                        )));
                    })
                    .subscribe_to_all_tables();
            })
            .on_connect_error(move |_, error| {
                let _ = connect_error_tx.send(LifecycleEvent::Failed(format!(
                    "connection establishment failed: {error}"
                )));
            })
            .on_disconnect(move |_, error| {
                let _ = disconnect_tx.send(LifecycleEvent::Disconnected(
                    error.map(|value| value.to_string()),
                ));
            })
            .build()
            .with_context(|| format!("build {label} connection to {host}/{database}"))?;
        let pump = conn.run_threaded();

        let deadline = Instant::now() + timeout;
        let mut connected = None;
        let mut subscribed = false;
        while connected.is_none() || !subscribed {
            match receive_before(
                &event_rx,
                deadline,
                &format!("{label} connection readiness"),
            )? {
                LifecycleEvent::Connected { identity, token } => {
                    connected = Some((identity, token));
                }
                LifecycleEvent::Subscribed => subscribed = true,
                LifecycleEvent::Failed(message) => bail!("{label}: {message}"),
                LifecycleEvent::Disconnected(error) => {
                    bail!(
                        "{label} disconnected before its subscription was ready: {}",
                        error.as_deref().unwrap_or("no server error")
                    );
                }
            }
        }

        let (identity, token) = connected.context("connection callback omitted identity")?;
        write_token(token_path, &token).with_context(|| {
            format!("persist {label} identity token at {}", token_path.display())
        })?;
        Ok(Self {
            label,
            conn,
            identity,
            events: event_rx,
            pump: Some(pump),
            stopped: false,
        })
    }

    fn join_match(&self, player_id: u8, display_name: &str, timeout: Duration) -> Result<()> {
        let (tx, rx) = mpsc::channel();
        self.conn
            .reducers
            .join_match_then(player_id, display_name.to_owned(), move |_, result| {
                let _ = tx.send(flatten_reducer_result(result));
            })
            .with_context(|| format!("send join_match for {}", self.label))?;
        wait_for_reducer(&rx, timeout, &format!("{} join_match", self.label))
    }

    fn set_mobilization_target(
        &self,
        command_id: u64,
        target_bps: u32,
        timeout: Duration,
    ) -> Result<()> {
        let (tx, rx) = mpsc::channel();
        self.conn
            .reducers
            .set_mobilization_target_then(command_id, target_bps, move |_, result| {
                let _ = tx.send(flatten_reducer_result(result));
            })
            .context("send set_mobilization_target")?;
        wait_for_reducer(&rx, timeout, "set_mobilization_target")
    }

    fn issue_push_front(
        &self,
        command_id: u64,
        selected_cells: &[u32],
        direction: Axial,
        commitment_bps: u32,
        timeout: Duration,
    ) -> Result<()> {
        let (tx, rx) = mpsc::channel();
        self.conn
            .reducers
            .issue_push_front_then(
                command_id,
                selected_cells.to_vec(),
                direction.q,
                direction.r,
                commitment_bps,
                move |_, result| {
                    let _ = tx.send(flatten_reducer_result(result));
                },
            )
            .context("send issue_push_front")?;
        wait_for_reducer(&rx, timeout, "issue_push_front")
    }

    fn disconnect(&mut self, timeout: Duration) -> Result<()> {
        if self.stopped {
            return Ok(());
        }
        self.conn
            .disconnect()
            .with_context(|| format!("request {} disconnect", self.label))?;
        let deadline = Instant::now() + timeout;
        loop {
            match receive_before(
                &self.events,
                deadline,
                &format!("{} disconnect callback", self.label),
            )? {
                LifecycleEvent::Disconnected(error) => {
                    if let Some(error) = error {
                        bail!("{} disconnected with an error: {error}", self.label);
                    }
                    break;
                }
                LifecycleEvent::Failed(message) => bail!("{}: {message}", self.label),
                LifecycleEvent::Connected { .. } | LifecycleEvent::Subscribed => {}
            }
        }
        self.stopped = true;
        self.finish_pump(timeout);
        Ok(())
    }

    fn finish_pump(&mut self, timeout: Duration) {
        let Some(pump) = self.pump.take() else {
            return;
        };
        let deadline = Instant::now() + timeout;
        while !pump.is_finished() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        if pump.is_finished() {
            let _ = pump.join();
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        if !self.stopped {
            let _ = self.conn.disconnect();
        }
    }
}

#[derive(Clone, Debug)]
struct PushFrontCandidate {
    selected_cells: Vec<u32>,
    front_cell: u32,
    destination_cell: u32,
    direction: Axial,
    commitment_bps: u32,
    expected_requested: u64,
}

#[allow(clippy::too_many_lines)]
fn main() -> Result<()> {
    let args = Args::parse();
    ensure!(args.timeout_secs > 0, "--timeout-secs must be positive");
    ensure!(args.poll_ms > 0, "--poll-ms must be positive");
    let timeout = Duration::from_secs(args.timeout_secs);
    let poll = Duration::from_millis(args.poll_ms);

    let player_one_token = args.token_dir.join("player-one.token");
    let player_two_token = args.token_dir.join("player-two.token");

    println!("[1/6] connecting two persistent identity profiles");
    let mut player_one = Client::connect(
        "player one",
        &player_one_token,
        &args.host,
        &args.database,
        timeout,
    )?;
    let mut player_two = Client::connect(
        "player two",
        &player_two_token,
        &args.host,
        &args.database,
        timeout,
    )?;
    ensure!(
        player_one.identity != player_two.identity,
        "the two token profiles resolved to the same identity; remove one token profile and retry"
    );

    assert_slot_available_or_owned(&player_one, PLAYER_ONE, player_one.identity)?;
    assert_slot_available_or_owned(&player_one, PLAYER_TWO, player_two.identity)?;

    println!("[2/6] claiming player slots and waiting for a running match");
    player_one.join_match(PLAYER_ONE, "E2E Player 1", timeout)?;
    player_two.join_match(PLAYER_TWO, "E2E Player 2", timeout)?;
    wait_for_slot(&player_one, PLAYER_ONE, player_one.identity, timeout, poll)?;
    wait_for_slot(&player_one, PLAYER_TWO, player_two.identity, timeout, poll)?;
    let running_step = wait_until("match phase Running", timeout, poll, || {
        let state = player_one
            .conn
            .db
            .match_state()
            .singleton_id()
            .find(&SINGLETON_ID);
        Ok(state.and_then(|row| (row.phase == MatchPhase::Running).then_some(row.logical_step)))
    })?;

    println!("[3/6] verifying idempotent mobilization and its receipt");
    let mobilization_id = unused_command_id(&player_one.conn, PLAYER_ONE, COMMAND_ID_FLOOR)?;
    let current_target = player_one
        .conn
        .db
        .mobilization_policy()
        .player_id()
        .find(&PLAYER_ONE)
        .context("player one mobilization policy is absent")?
        .target_bps;
    let target_bps = if current_target == 0 { 100 } else { 0 };
    player_one.set_mobilization_target(mobilization_id, target_bps, timeout)?;
    let mobilization_receipt = wait_for_receipt(
        &player_one,
        PLAYER_ONE,
        mobilization_id,
        "set_mobilization_target",
        timeout,
        poll,
    )?;
    ensure!(
        mobilization_receipt.order_id == 0,
        "mobilization receipt unexpectedly referenced order {}",
        mobilization_receipt.order_id
    );
    player_one.set_mobilization_target(mobilization_id, target_bps, timeout)?;
    let matching_receipts = player_one
        .conn
        .db
        .command_receipt()
        .iter()
        .filter(|receipt| {
            receipt.player_id == PLAYER_ONE && receipt.client_command_id == mobilization_id
        })
        .count();
    ensure!(
        matching_receipts == 1,
        "idempotent mobilization retry produced {matching_receipts} receipts instead of one"
    );
    wait_until("mobilization policy update", timeout, poll, || {
        Ok(player_one
            .conn
            .db
            .mobilization_policy()
            .player_id()
            .find(&PLAYER_ONE)
            .and_then(|policy| (policy.target_bps == target_bps).then_some(())))
    })?;

    println!("[4/6] issuing an authoritative directional front push");
    let candidate = select_push_front_candidate(&player_one.conn, PLAYER_ONE)?;
    let destination_before = player_one
        .conn
        .db
        .cell_state()
        .cell_id()
        .find(&candidate.destination_cell)
        .context("selected destination state disappeared")?
        .infantry;
    let push_id = unused_command_id(
        &player_one.conn,
        PLAYER_ONE,
        mobilization_id
            .checked_add(1)
            .context("mobilization command ID overflow")?,
    )?;
    player_one.issue_push_front(
        push_id,
        &candidate.selected_cells,
        candidate.direction,
        candidate.commitment_bps,
        timeout,
    )?;
    let push_receipt = wait_for_receipt(
        &player_one,
        PLAYER_ONE,
        push_id,
        "issue_push_front",
        timeout,
        poll,
    )?;
    ensure!(
        push_receipt.order_id != 0,
        "accepted front-push receipt did not reference an order"
    );

    wait_until("front-push route persistence", timeout, poll, || {
        let Some(order) = player_one
            .conn
            .db
            .transfer_order()
            .order_id()
            .find(&push_receipt.order_id)
        else {
            return Ok(None);
        };
        assert_push_order(&order, &candidate, push_id)?;
        assert_order_conservation(&order)?;
        let packets: Vec<_> = player_one
            .conn
            .db
            .transit_packet()
            .iter()
            .filter(|packet| packet.order_id == order.order_id)
            .collect();
        if packets.is_empty() {
            return Ok(None);
        }
        assert_push_routes(&player_one.conn, &candidate, &order, &packets)?;

        let source_cells: HashSet<_> = player_one
            .conn
            .db
            .transfer_source()
            .iter()
            .filter(|source| source.order_id == order.order_id)
            .map(|source| source.cell_id)
            .collect();
        let expected_sources: HashSet<_> = candidate.selected_cells.iter().copied().collect();
        ensure!(
            source_cells == expected_sources,
            "front-push order did not persist every selected corridor cell as a source"
        );
        let destinations: Vec<_> = player_one
            .conn
            .db
            .transfer_destination()
            .iter()
            .filter(|destination| destination.order_id == order.order_id)
            .collect();
        ensure!(
            destinations.len() == 1
                && destinations[0].cell_id == candidate.destination_cell
                && destinations[0].target_infantry == order.committed_infantry,
            "front-push order did not persist its exact directional destination"
        );
        Ok(Some(()))
    })?;

    player_one.issue_push_front(
        push_id,
        &candidate.selected_cells,
        candidate.direction,
        candidate.commitment_bps,
        timeout,
    )?;
    let matching_receipts = player_one
        .conn
        .db
        .command_receipt()
        .iter()
        .filter(|receipt| receipt.player_id == PLAYER_ONE && receipt.client_command_id == push_id)
        .count();
    let matching_orders = player_one
        .conn
        .db
        .transfer_order()
        .iter()
        .filter(|order| order.player_id == PLAYER_ONE && order.client_command_id == push_id)
        .count();
    ensure!(
        matching_receipts == 1 && matching_orders == 1,
        "idempotent front-push retry produced {matching_receipts} receipts and {matching_orders} orders"
    );

    println!("[5/6] observing front-push progression and strength conservation");
    let mut observed_packet_progress = false;
    let completed_order = wait_until("front-push completion", timeout, poll, || {
        let Some(order) = player_one
            .conn
            .db
            .transfer_order()
            .order_id()
            .find(&push_receipt.order_id)
        else {
            return Ok(None);
        };
        assert_push_order(&order, &candidate, push_id)?;
        assert_order_conservation(&order)?;
        observed_packet_progress |= player_one
            .conn
            .db
            .transit_packet()
            .iter()
            .filter(|packet| packet.order_id == order.order_id)
            .any(|packet| packet.route_index > 0 || packet.updated_step > order.created_step);
        if order.status != OrderStatus::Completed {
            return Ok(None);
        }
        ensure!(
            observed_packet_progress || order.updated_step > order.created_step,
            "front-push order completed without observable route progression"
        );
        Ok(Some(order))
    })?;
    ensure!(
        completed_order.committed_infantry > 0,
        "front push completed without committing infantry"
    );
    ensure!(
        completed_order.delivered_infantry == completed_order.committed_infantry,
        "neutral front push delivered {} of {} committed infantry",
        completed_order.delivered_infantry,
        completed_order.committed_infantry
    );
    ensure!(
        completed_order.casualty_infantry == 0,
        "neutral front push unexpectedly recorded {} casualties",
        completed_order.casualty_infantry
    );
    ensure!(
        completed_order.updated_step > completed_order.created_step,
        "front push never progressed beyond creation step {}",
        completed_order.created_step
    );
    ensure!(
        completed_order.created_step >= running_step,
        "front-push order predates the observed running match"
    );
    let destination_after = player_one
        .conn
        .db
        .cell_state()
        .cell_id()
        .find(&candidate.destination_cell)
        .context("selected destination state disappeared after front push")?
        .infantry;
    ensure!(
        destination_after >= destination_before.saturating_add(completed_order.delivered_infantry),
        "destination infantry did not reflect delivery: before={destination_before}, after={destination_after}, delivered={}",
        completed_order.delivered_infantry
    );
    let progressed_step = player_one
        .conn
        .db
        .match_state()
        .singleton_id()
        .find(&SINGLETON_ID)
        .context("match state disappeared after front push")?
        .logical_step;
    ensure!(
        progressed_step > running_step,
        "logical simulation step did not progress beyond {running_step}"
    );

    println!("[6/6] reconnecting player one with its persisted token");
    let reconnect_count_before = player_two
        .conn
        .db
        .player_slot()
        .player_id()
        .find(&PLAYER_ONE)
        .context("player one slot disappeared before reconnect test")?
        .reconnect_count;
    let original_identity = player_one.identity;
    player_one.disconnect(timeout)?;
    wait_until("player one disconnect visibility", timeout, poll, || {
        Ok(player_two
            .conn
            .db
            .player_slot()
            .player_id()
            .find(&PLAYER_ONE)
            .and_then(|slot| (!slot.connected).then_some(())))
    })?;
    let mut reconnected = Client::connect(
        "player one reconnect",
        &player_one_token,
        &args.host,
        &args.database,
        timeout,
    )?;
    ensure!(
        reconnected.identity == original_identity,
        "persisted player-one token resolved to a different identity"
    );
    reconnected.join_match(PLAYER_ONE, "E2E Player 1", timeout)?;
    wait_until("player one reconnect visibility", timeout, poll, || {
        let Some(slot) = player_two
            .conn
            .db
            .player_slot()
            .player_id()
            .find(&PLAYER_ONE)
        else {
            return Ok(None);
        };
        ensure!(
            slot.identity.as_ref() == Some(&original_identity),
            "reconnected slot identity changed"
        );
        Ok((slot.connected
            && slot.has_reconnected
            && slot.reconnect_count > reconnect_count_before)
            .then_some(()))
    })?;

    reconnected.disconnect(timeout)?;
    player_two.disconnect(timeout)?;
    println!(
        "PASS: receipts, idempotency, front-push direction/routing/conservation, and token reuse verified"
    );
    Ok(())
}

fn flatten_reducer_result<E: std::fmt::Debug>(
    result: std::result::Result<std::result::Result<(), String>, E>,
) -> std::result::Result<(), String> {
    match result {
        Ok(Ok(())) => Ok(()),
        Ok(Err(message)) => Err(message),
        Err(error) => Err(format!("SDK reducer callback failed: {error:?}")),
    }
}

fn wait_for_reducer(
    receiver: &Receiver<std::result::Result<(), String>>,
    timeout: Duration,
    label: &str,
) -> Result<()> {
    match receiver.recv_timeout(timeout) {
        Ok(Ok(())) => Ok(()),
        Ok(Err(message)) => bail!("{label} was rejected by the reducer: {message}"),
        Err(RecvTimeoutError::Timeout) => bail!("timed out after {timeout:?} waiting for {label}"),
        Err(RecvTimeoutError::Disconnected) => {
            bail!("callback channel closed while waiting for {label}")
        }
    }
}

fn receive_before(
    receiver: &Receiver<LifecycleEvent>,
    deadline: Instant,
    label: &str,
) -> Result<LifecycleEvent> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        bail!("timed out waiting for {label}");
    }
    match receiver.recv_timeout(remaining) {
        Ok(event) => Ok(event),
        Err(RecvTimeoutError::Timeout) => bail!("timed out waiting for {label}"),
        Err(RecvTimeoutError::Disconnected) => {
            bail!("lifecycle channel closed while waiting for {label}")
        }
    }
}

fn wait_until<T>(
    label: &str,
    timeout: Duration,
    poll: Duration,
    mut inspect: impl FnMut() -> Result<Option<T>>,
) -> Result<T> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(value) = inspect().with_context(|| format!("while waiting for {label}"))? {
            return Ok(value);
        }
        if Instant::now() >= deadline {
            bail!("timed out after {timeout:?} waiting for {label}");
        }
        thread::sleep(poll.min(deadline.saturating_duration_since(Instant::now())));
    }
}

fn read_token(path: &Path) -> Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(contents) => {
            let token = contents.trim();
            ensure!(!token.is_empty(), "token file {} is empty", path.display());
            Ok(Some(token.to_owned()))
        }
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("read token file {}", path.display())),
    }
}

fn write_token(path: &Path, token: &str) -> Result<()> {
    ensure!(
        !token.trim().is_empty(),
        "server returned an empty identity token"
    );
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create token directory {}", parent.display()))?;
    }
    let mut options = OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options
        .open(path)
        .with_context(|| format!("open token file {}", path.display()))?;
    file.write_all(token.as_bytes())
        .with_context(|| format!("write token file {}", path.display()))?;
    file.write_all(b"\n")
        .with_context(|| format!("finish token file {}", path.display()))?;
    Ok(())
}

fn assert_slot_available_or_owned(
    client: &Client,
    player_id: u8,
    expected_identity: Identity,
) -> Result<()> {
    let slot = client
        .conn
        .db
        .player_slot()
        .player_id()
        .find(&player_id)
        .with_context(|| format!("player slot {player_id} is absent"))?;
    if let Some(identity) = slot.identity {
        ensure!(
            identity == expected_identity,
            "player slot {player_id} is already owned by another identity; use a fresh database or the original token profiles"
        );
    }
    Ok(())
}

fn wait_for_slot(
    client: &Client,
    player_id: u8,
    identity: Identity,
    timeout: Duration,
    poll: Duration,
) -> Result<()> {
    wait_until(
        &format!("player slot {player_id} claim"),
        timeout,
        poll,
        || {
            let Some(slot) = client.conn.db.player_slot().player_id().find(&player_id) else {
                return Ok(None);
            };
            if let Some(actual) = slot.identity {
                ensure!(
                    actual == identity,
                    "player slot {player_id} was claimed by an unexpected identity"
                );
            }
            Ok((slot.identity == Some(identity) && slot.connected && slot.ready).then_some(()))
        },
    )
}

fn unused_command_id(conn: &DbConnection, player_id: u8, start: u64) -> Result<u64> {
    let mut candidate = start;
    loop {
        let key = format!("{player_id}:{candidate}");
        if conn.db.command_receipt().receipt_key().find(&key).is_none() {
            return Ok(candidate);
        }
        candidate = candidate
            .checked_add(1)
            .context("exhausted client command ID range")?;
    }
}

fn wait_for_receipt(
    client: &Client,
    player_id: u8,
    command_id: u64,
    command_name: &str,
    timeout: Duration,
    poll: Duration,
) -> Result<CommandReceipt> {
    let key = format!("{player_id}:{command_id}");
    let receipt = wait_until(
        &format!("{command_name} command receipt"),
        timeout,
        poll,
        || Ok(client.conn.db.command_receipt().receipt_key().find(&key)),
    )?;
    ensure!(
        receipt.player_id == player_id && receipt.client_command_id == command_id,
        "receipt key {key} resolved to the wrong command"
    );
    ensure!(
        receipt.command_name == command_name,
        "receipt named command '{}' instead of '{command_name}'",
        receipt.command_name
    );
    ensure!(
        receipt.status == ReceiptStatus::Accepted,
        "{command_name} receipt was rejected: {}",
        receipt.message
    );
    Ok(receipt)
}

#[allow(clippy::too_many_lines)]
fn select_push_front_candidate(conn: &DbConnection, player_id: u8) -> Result<PushFrontCandidate> {
    let terrain_by_id: HashMap<u32, CellTerrain> = conn
        .db
        .cell_terrain()
        .iter()
        .map(|terrain| (terrain.cell_id, terrain))
        .collect();
    let cell_by_id: HashMap<u32, CellState> = conn
        .db
        .cell_state()
        .iter()
        .map(|cell| (cell.cell_id, cell))
        .collect();
    let cell_by_coordinate: HashMap<Axial, u32> = terrain_by_id
        .values()
        .map(|terrain| (Axial::new(terrain.q, terrain.r), terrain.cell_id))
        .collect();

    let active_orders: HashSet<u64> = conn
        .db
        .transfer_order()
        .iter()
        .filter(|order| order.status == OrderStatus::Active)
        .map(|order| order.order_id)
        .collect();
    let mut reserved_by_destination = HashMap::<u32, u64>::new();
    for destination in conn.db.transfer_destination().iter() {
        if active_orders.contains(&destination.order_id) {
            let outstanding = destination
                .target_infantry
                .saturating_sub(destination.received_infantry);
            *reserved_by_destination
                .entry(destination.cell_id)
                .or_default() += outstanding;
        }
    }
    let mut allocated_by_source = HashMap::<u32, u64>::new();
    for packet in conn
        .db
        .transit_packet()
        .iter()
        .filter(|packet| packet.owner_player_id == player_id)
    {
        *allocated_by_source.entry(packet.current_cell).or_default() += packet.infantry;
    }

    let mut owned_coordinates: Vec<_> = cell_by_coordinate
        .iter()
        .filter_map(|(&coordinate, &cell_id)| {
            (cell_by_id.get(&cell_id)?.owner_player_id == player_id)
                .then_some((coordinate, cell_id))
        })
        .collect();
    owned_coordinates.sort_unstable();

    let mut candidates = Vec::new();
    for (front_coordinate, front_id) in owned_coordinates {
        let Some(front_terrain) = terrain_by_id.get(&front_id) else {
            continue;
        };
        if !front_terrain.passable || !front_terrain.capturable {
            continue;
        }
        for direction in Axial::DIRECTIONS {
            let destination_coordinate = front_coordinate + direction;
            let Some(destination_id) = cell_by_coordinate.get(&destination_coordinate).copied()
            else {
                continue;
            };
            let Some(destination) = cell_by_id.get(&destination_id) else {
                continue;
            };
            let Some(destination_terrain) = terrain_by_id.get(&destination_id) else {
                continue;
            };
            if destination.owner_player_id != 0
                || destination.infantry != 0
                || !destination_terrain.passable
                || !destination_terrain.capturable
                || front_terrain
                    .elevation
                    .abs_diff(destination_terrain.elevation)
                    > 1
            {
                continue;
            }
            let free_capacity = destination.military_capacity.saturating_sub(
                reserved_by_destination
                    .get(&destination_id)
                    .copied()
                    .unwrap_or(0),
            );
            if free_capacity == 0 {
                continue;
            }

            // Walk straight backward from the exposed boundary. This creates
            // the smallest useful representation of the actual interaction:
            // a front cell plus connected owned cells that feed it without
            // exposing any additional directional boundary edges.
            let mut corridor = vec![(front_coordinate, front_id)];
            let mut previous_terrain = front_terrain;
            let mut next_coordinate = front_coordinate - direction;
            while corridor.len() < MAX_PUSH_CORRIDOR_CELLS {
                let Some(next_id) = cell_by_coordinate.get(&next_coordinate).copied() else {
                    break;
                };
                let Some(next_state) = cell_by_id.get(&next_id) else {
                    break;
                };
                let Some(next_terrain) = terrain_by_id.get(&next_id) else {
                    break;
                };
                if next_state.owner_player_id != player_id
                    || !next_terrain.passable
                    || !next_terrain.capturable
                    || previous_terrain.elevation.abs_diff(next_terrain.elevation) > 1
                {
                    break;
                }
                corridor.push((next_coordinate, next_id));
                previous_terrain = next_terrain;
                next_coordinate = next_coordinate - direction;
            }

            for corridor_len in (2..=corridor.len()).rev() {
                let selected_cells: Vec<_> = corridor[..corridor_len]
                    .iter()
                    .map(|(_, cell_id)| *cell_id)
                    .collect();
                let commitments = selected_cells
                    .iter()
                    .map(|cell_id| {
                        let state = cell_by_id
                            .get(cell_id)
                            .expect("corridor state was validated above");
                        let available = state
                            .infantry
                            .saturating_sub(allocated_by_source.get(cell_id).copied().unwrap_or(0));
                        u64::try_from(
                            u128::from(available) * u128::from(PUSH_COMMITMENT_BPS) / 10_000,
                        )
                        .expect("basis-point commitment cannot exceed available infantry")
                    })
                    .collect::<Vec<_>>();
                if commitments.contains(&0) {
                    continue;
                }
                let expected_requested = commitments
                    .iter()
                    .try_fold(0_u64, |total, amount| total.checked_add(*amount));
                let Some(expected_requested) = expected_requested else {
                    continue;
                };
                if expected_requested > free_capacity {
                    continue;
                }
                candidates.push(PushFrontCandidate {
                    selected_cells,
                    front_cell: front_id,
                    destination_cell: destination_id,
                    direction,
                    commitment_bps: PUSH_COMMITMENT_BPS,
                    expected_requested,
                });
                break;
            }
        }
    }
    candidates.sort_unstable_by_key(|candidate| {
        (
            std::cmp::Reverse(candidate.selected_cells.len()),
            std::cmp::Reverse(candidate.expected_requested),
            candidate.front_cell,
            candidate.destination_cell,
            candidate.direction,
        )
    });
    candidates.into_iter().next().context(
        "no connected owned corridor has infantry and an empty neutral front in one exact direction",
    )
}

fn assert_push_order(
    order: &TransferOrder,
    candidate: &PushFrontCandidate,
    command_id: u64,
) -> Result<()> {
    ensure!(
        order.player_id == PLAYER_ONE && order.client_command_id == command_id,
        "front-push receipt referenced an order owned by another command"
    );
    ensure!(
        order.kind == OrderKind::PushFront,
        "front-push receipt referenced {:?} order {}",
        order.kind,
        order.order_id
    );
    ensure!(
        (order.orientation_q, order.orientation_r)
            == (candidate.direction.q, candidate.direction.r),
        "front-push orientation ({}, {}) did not preserve submitted direction ({}, {})",
        order.orientation_q,
        order.orientation_r,
        candidate.direction.q,
        candidate.direction.r
    );
    ensure!(
        order.requested_infantry == candidate.expected_requested
            && order.committed_infantry == candidate.expected_requested,
        "{} bps push requested/committed {}/{} infantry instead of the expected {}",
        candidate.commitment_bps,
        order.requested_infantry,
        order.committed_infantry,
        candidate.expected_requested
    );
    Ok(())
}

fn assert_push_routes(
    conn: &DbConnection,
    candidate: &PushFrontCandidate,
    order: &TransferOrder,
    packets: &[TransitPacket],
) -> Result<()> {
    let terrain_by_id: HashMap<_, _> = conn
        .db
        .cell_terrain()
        .iter()
        .map(|terrain| (terrain.cell_id, terrain))
        .collect();
    let selected: HashSet<_> = candidate.selected_cells.iter().copied().collect();
    let mut packet_total = 0_u64;
    let mut has_rear_corridor_route = false;
    for packet in packets {
        ensure!(
            packet.order_id == order.order_id && packet.owner_player_id == PLAYER_ONE,
            "front-push route packet belongs to another order or player"
        );
        ensure!(
            selected.contains(&packet.origin_cell),
            "front-push packet originated outside the submitted corridor"
        );
        ensure!(
            packet.route.first() == Some(&packet.origin_cell)
                && packet.route.last() == Some(&candidate.destination_cell),
            "front-push packet route did not connect its selected origin to the directional target"
        );
        let route_index = usize::try_from(packet.route_index).context("route index overflow")?;
        ensure!(
            packet.route.get(route_index) == Some(&packet.current_cell),
            "front-push packet current cell does not match its route index"
        );
        ensure!(
            packet.route[..packet.route.len() - 1]
                .iter()
                .all(|cell_id| selected.contains(cell_id)),
            "front-push packet escaped the submitted corridor before its final edge"
        );
        ensure!(
            packet.route.get(packet.route.len() - 2) == Some(&candidate.front_cell),
            "front-push packet did not leave through the selected front cell"
        );
        for cells in packet.route.windows(2) {
            let from = terrain_by_id
                .get(&cells[0])
                .with_context(|| format!("route terrain {} disappeared", cells[0]))?;
            let to = terrain_by_id
                .get(&cells[1])
                .with_context(|| format!("route terrain {} disappeared", cells[1]))?;
            ensure!(
                Axial::new(from.q, from.r).distance(Axial::new(to.q, to.r)) == 1,
                "front-push route contains a non-adjacent step"
            );
        }
        let boundary = terrain_by_id
            .get(&candidate.front_cell)
            .context("front terrain disappeared")?;
        let destination = terrain_by_id
            .get(&candidate.destination_cell)
            .context("destination terrain disappeared")?;
        ensure!(
            Axial::new(destination.q, destination.r) - Axial::new(boundary.q, boundary.r)
                == candidate.direction,
            "front-push route's final edge does not match the submitted direction"
        );
        has_rear_corridor_route |=
            packet.origin_cell != candidate.front_cell && packet.route.len() >= 3;
        packet_total = packet_total
            .checked_add(packet.infantry)
            .context("front-push packet accounting overflow")?;
    }
    ensure!(
        has_rear_corridor_route,
        "front-push order did not persist a route from behind the boundary"
    );
    ensure!(
        packet_total == order.in_transit_infantry,
        "front-push packet total {packet_total} differs from order in-transit infantry {}",
        order.in_transit_infantry
    );
    Ok(())
}

fn assert_order_conservation(order: &TransferOrder) -> Result<()> {
    let accounted = order
        .in_transit_infantry
        .checked_add(order.delivered_infantry)
        .and_then(|value| value.checked_add(order.casualty_infantry))
        .context("transfer accounting overflow")?;
    ensure!(
        order.committed_infantry == accounted,
        "order {} violates conservation: committed={}, in_transit={}, delivered={}, casualties={}",
        order.order_id,
        order.committed_infantry,
        order.in_transit_infantry,
        order.delivered_infantry,
        order.casualty_infantry
    );
    Ok(())
}
