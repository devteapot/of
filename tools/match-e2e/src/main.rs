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
    MobilizationPolicyTableAccess, OrderStatus, PlayerSlotTableAccess, ReceiptStatus,
    TransferDestinationTableAccess, TransferOrder, TransferOrderTableAccess,
    TransitPacketTableAccess, issue_transfer as _, join_match as _, set_mobilization_target as _,
};
use spacetimedb_sdk::{DbContext, Identity, Table};

const PLAYER_ONE: u8 = 1;
const PLAYER_TWO: u8 = 2;
const SINGLETON_ID: u8 = 0;
const COMMAND_ID_FLOOR: u64 = 9_000_000_000;

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

    fn issue_transfer(
        &self,
        command_id: u64,
        source_cell: u32,
        destination_cell: u32,
        infantry: u64,
        timeout: Duration,
    ) -> Result<()> {
        let (tx, rx) = mpsc::channel();
        self.conn
            .reducers
            .issue_transfer_then(
                command_id,
                vec![source_cell],
                vec![destination_cell],
                infantry,
                move |_, result| {
                    let _ = tx.send(flatten_reducer_result(result));
                },
            )
            .context("send issue_transfer")?;
        wait_for_reducer(&rx, timeout, "issue_transfer")
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

#[derive(Clone, Copy, Debug)]
struct TransferCandidate {
    source_cell: u32,
    destination_cell: u32,
    infantry: u64,
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

    println!("[4/6] issuing a nearby friendly transfer");
    let candidate = select_transfer_candidate(&player_one.conn, PLAYER_ONE)?;
    let destination_before = player_one
        .conn
        .db
        .cell_state()
        .cell_id()
        .find(&candidate.destination_cell)
        .context("selected destination state disappeared")?
        .infantry;
    let transfer_id = unused_command_id(
        &player_one.conn,
        PLAYER_ONE,
        mobilization_id
            .checked_add(1)
            .context("mobilization command ID overflow")?,
    )?;
    player_one.issue_transfer(
        transfer_id,
        candidate.source_cell,
        candidate.destination_cell,
        candidate.infantry,
        timeout,
    )?;
    let transfer_receipt = wait_for_receipt(
        &player_one,
        PLAYER_ONE,
        transfer_id,
        "issue_transfer",
        timeout,
        poll,
    )?;
    ensure!(
        transfer_receipt.order_id != 0,
        "accepted transfer receipt did not reference an order"
    );

    println!("[5/6] observing transfer progression and strength conservation");
    let completed_order = wait_until("nearby transfer completion", timeout, poll, || {
        let Some(order) = player_one
            .conn
            .db
            .transfer_order()
            .order_id()
            .find(&transfer_receipt.order_id)
        else {
            return Ok(None);
        };
        assert_order_conservation(&order)?;
        Ok((order.status == OrderStatus::Completed).then_some(order))
    })?;
    ensure!(
        completed_order.committed_infantry > 0,
        "transfer completed without committing infantry"
    );
    ensure!(
        completed_order.delivered_infantry == completed_order.committed_infantry,
        "friendly transfer delivered {} of {} committed infantry",
        completed_order.delivered_infantry,
        completed_order.committed_infantry
    );
    ensure!(
        completed_order.casualty_infantry == 0,
        "friendly transfer unexpectedly recorded {} casualties",
        completed_order.casualty_infantry
    );
    ensure!(
        completed_order.updated_step > completed_order.created_step,
        "transfer never progressed beyond creation step {}",
        completed_order.created_step
    );
    ensure!(
        completed_order.created_step >= running_step,
        "transfer order predates the observed running match"
    );
    let destination_after = player_one
        .conn
        .db
        .cell_state()
        .cell_id()
        .find(&candidate.destination_cell)
        .context("selected destination state disappeared after transfer")?
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
        .context("match state disappeared after transfer")?
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
        "PASS: receipts, idempotency, transfer conservation/progression, and token reuse verified"
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
fn select_transfer_candidate(conn: &DbConnection, player_id: u8) -> Result<TransferCandidate> {
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

    let mut candidates = Vec::new();
    for source in cell_by_id
        .values()
        .filter(|cell| cell.owner_player_id == player_id)
    {
        let Some(source_terrain) = terrain_by_id.get(&source.cell_id) else {
            continue;
        };
        if !source_terrain.passable || !source_terrain.capturable {
            continue;
        }
        let available = source.infantry.saturating_sub(
            allocated_by_source
                .get(&source.cell_id)
                .copied()
                .unwrap_or(0),
        );
        if available == 0 {
            continue;
        }
        let source_coordinate = Axial::new(source_terrain.q, source_terrain.r);
        for destination_coordinate in source_coordinate.neighbors() {
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
            if destination.owner_player_id != player_id
                || !destination_terrain.passable
                || !destination_terrain.capturable
                || source_terrain
                    .elevation
                    .abs_diff(destination_terrain.elevation)
                    > 1
            {
                continue;
            }
            let free_capacity = destination
                .military_capacity
                .saturating_sub(destination.infantry)
                .saturating_sub(
                    reserved_by_destination
                        .get(&destination_id)
                        .copied()
                        .unwrap_or(0),
                );
            let infantry = available.min(free_capacity).min(5);
            if infantry > 0 {
                candidates.push(TransferCandidate {
                    source_cell: source.cell_id,
                    destination_cell: destination_id,
                    infantry,
                });
            }
        }
    }
    candidates.sort_unstable_by_key(|candidate| {
        (
            std::cmp::Reverse(candidate.infantry),
            candidate.source_cell,
            candidate.destination_cell,
        )
    });
    candidates.into_iter().next().context(
        "no adjacent owned cells have both uncommitted source infantry and destination capacity",
    )
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
