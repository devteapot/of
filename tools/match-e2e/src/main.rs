#![forbid(unsafe_code)]

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
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
    TerrainClass, TransferDestinationTableAccess, TransferOrder, TransferOrderTableAccess,
    TransferSourceTableAccess, TransitPacket, TransitPacketTableAccess, cancel_expand_all as _,
    cancel_push_fronts as _, issue_expand_all as _, issue_push_front as _, join_match as _,
    set_mobilization_target as _,
};
use spacetimedb_sdk::{DbContext, Identity, Table};

const PLAYER_ONE: u8 = 1;
const PLAYER_TWO: u8 = 2;
const SINGLETON_ID: u8 = 0;
const COMMAND_ID_FLOOR: u64 = 9_000_000_000;
const PUSH_COMMITMENT_BPS: u32 = 5_000;
const EXPAND_COMMITMENT_BPS: u32 = 10_000;
const MAX_PUSH_CORRIDOR_CELLS: usize = 5;
const REQUIRED_LANE_CELLS: usize = 4;
const OBSERVED_CAPTURE_LAYERS: usize = 2;
const POST_CANCEL_STEPS: u64 = 2;
const EXPANSION_AGGREGATE_ORIGIN: u32 = u32::MAX;

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

    fn cancel_push_fronts(
        &self,
        command_id: u64,
        selected_cells: &[u32],
        direction: Axial,
        timeout: Duration,
    ) -> Result<()> {
        let (tx, rx) = mpsc::channel();
        self.conn
            .reducers
            .cancel_push_fronts_then(
                command_id,
                selected_cells.to_vec(),
                direction.q,
                direction.r,
                move |_, result| {
                    let _ = tx.send(flatten_reducer_result(result));
                },
            )
            .context("send cancel_push_fronts")?;
        wait_for_reducer(&rx, timeout, "cancel_push_fronts")
    }

    fn issue_expand_all(
        &self,
        command_id: u64,
        selected_cells: &[u32],
        commitment_bps: u32,
        timeout: Duration,
    ) -> Result<()> {
        let (tx, rx) = mpsc::channel();
        self.conn
            .reducers
            .issue_expand_all_then(
                command_id,
                selected_cells.to_vec(),
                commitment_bps,
                move |_, result| {
                    let _ = tx.send(flatten_reducer_result(result));
                },
            )
            .context("send issue_expand_all")?;
        wait_for_reducer(&rx, timeout, "issue_expand_all")
    }

    fn cancel_expand_all(
        &self,
        command_id: u64,
        selected_cells: &[u32],
        timeout: Duration,
    ) -> Result<()> {
        let (tx, rx) = mpsc::channel();
        self.conn
            .reducers
            .cancel_expand_all_then(command_id, selected_cells.to_vec(), move |_, result| {
                let _ = tx.send(flatten_reducer_result(result));
            })
            .context("send cancel_expand_all")?;
        wait_for_reducer(&rx, timeout, "cancel_expand_all")
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
    lane_cells: Vec<u32>,
    direction: Axial,
    commitment_bps: u32,
    expected_requested: u64,
}

#[derive(Clone, Debug)]
struct ExpandAllCandidate {
    selected_cells: Vec<u32>,
    commitment_bps: u32,
    expected_requested: u64,
    expected_source_commitments: HashMap<u32, u64>,
    seed_depths: HashMap<u32, u16>,
    outside_depths: HashMap<u32, u16>,
    children: HashMap<u32, Vec<u32>>,
    first_ring: HashSet<u32>,
    turning_second_ring: HashSet<u32>,
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

    println!("[1/8] connecting two persistent identity profiles");
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

    println!("[2/8] claiming player slots and waiting for a running match");
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

    println!("[3/8] verifying idempotent mobilization and its receipt");
    let mobilization_id = unused_command_id(&player_one.conn, PLAYER_ONE, COMMAND_ID_FLOOR)?;
    // Keep recruitment stopped after proving the policy command. That makes
    // the exact percentage snapshots below stable across simulation ticks.
    let target_bps = 0;
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

    println!("[4/8] issuing an authoritative directional front push");
    let candidate = select_push_front_candidate(&player_one.conn, PLAYER_ONE)?;
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
                && destinations[0].cell_id == candidate.lane_cells[0]
                && destinations[0].target_infantry == order.committed_infantry,
            "front-push order did not persist its stable first-layer lane anchor"
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

    println!("[5/8] observing sustained progression, then cancelling the active push");
    let mut observed_packet_progress = false;
    let active_order = wait_until("two successive front-push captures", timeout, poll, || {
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
        observed_packet_progress |= packets
            .iter()
            .any(|packet| packet.route_index > 0 || packet.updated_step > order.created_step);
        if !packets.is_empty() {
            assert_push_routes(&player_one.conn, &candidate, &order, &packets)?;
        }

        let captured_layers = candidate.lane_cells[..OBSERVED_CAPTURE_LAYERS]
            .iter()
            .filter(|cell_id| {
                player_one
                    .conn
                    .db
                    .cell_state()
                    .cell_id()
                    .find(cell_id)
                    .is_some_and(|cell| cell.owner_player_id == PLAYER_ONE && cell.infantry > 0)
            })
            .count();
        if captured_layers < OBSERVED_CAPTURE_LAYERS {
            ensure!(
                order.status == OrderStatus::Active,
                "front push stopped after {captured_layers} layer(s), before proving sustained progression; use a fresh default fixture with a clear neutral lane"
            );
            return Ok(None);
        }
        ensure!(
            order.status == OrderStatus::Active && order.in_transit_infantry > 0,
            "front push exhausted at the second layer, leaving no active operation to cancel; use a fixture with a longer lane and more source infantry"
        );
        ensure!(
            observed_packet_progress && order.updated_step > order.created_step,
            "two captured layers were not accompanied by observable packet progression"
        );
        Ok(Some(order))
    })?;
    ensure!(
        active_order.casualty_infantry == 0,
        "neutral front push unexpectedly recorded {} casualties",
        active_order.casualty_infantry
    );
    ensure!(
        active_order.created_step >= running_step,
        "front-push order predates the observed running match"
    );

    let cancel_id = unused_command_id(
        &player_one.conn,
        PLAYER_ONE,
        push_id.checked_add(1).context("push command ID overflow")?,
    )?;
    player_one.cancel_push_fronts(
        cancel_id,
        &candidate.selected_cells,
        candidate.direction,
        timeout,
    )?;
    let cancel_receipt = wait_for_receipt(
        &player_one,
        PLAYER_ONE,
        cancel_id,
        "cancel_push_fronts",
        timeout,
        poll,
    )?;
    ensure!(
        cancel_receipt.order_id == push_receipt.order_id,
        "cancellation receipt referenced order {} instead of the unique sustained push {}; use a fresh database if older active pushes overlap this selection",
        cancel_receipt.order_id,
        push_receipt.order_id
    );

    let (cancelled_order, owners_at_cancel) =
        wait_until("front-push cancellation", timeout, poll, || {
            let Some(order) = player_one
                .conn
                .db
                .transfer_order()
                .order_id()
                .find(&push_receipt.order_id)
            else {
                return Ok(None);
            };
            if order.status != OrderStatus::Cancelled {
                return Ok(None);
            }
            assert_push_order(&order, &candidate, push_id)?;
            assert_order_conservation(&order)?;
            ensure!(
                order.in_transit_infantry == 0,
                "cancelled push retained {} in-transit infantry",
                order.in_transit_infantry
            );
            ensure!(
                !player_one
                    .conn
                    .db
                    .transit_packet()
                    .iter()
                    .any(|packet| packet.order_id == order.order_id),
                "cancelled push retained transit packets"
            );
            let sources: Vec<_> = player_one
                .conn
                .db
                .transfer_source()
                .iter()
                .filter(|source| source.order_id == order.order_id)
                .collect();
            ensure!(
                sources.len() == candidate.selected_cells.len()
                    && sources.iter().all(|source| source.queued_infantry == 0),
                "cancellation did not release every selected source allocation"
            );
            let owners = lane_owners(&player_one.conn, &candidate.lane_cells)?;
            Ok(Some((order, owners)))
        })?;
    ensure!(
        cancelled_order.delivered_infantry > active_order.delivered_infantry,
        "cancellation did not settle any of the {} infantry that were still in transit",
        active_order.in_transit_infantry
    );
    ensure!(
        cancelled_order.casualty_infantry == 0,
        "neutral sustained push unexpectedly recorded casualties before cancellation"
    );

    wait_until("post-cancellation simulation steps", timeout, poll, || {
        let Some(state) = player_one
            .conn
            .db
            .match_state()
            .singleton_id()
            .find(&SINGLETON_ID)
        else {
            return Ok(None);
        };
        if state.logical_step
            < cancelled_order
                .updated_step
                .saturating_add(POST_CANCEL_STEPS)
        {
            return Ok(None);
        }
        let order = player_one
            .conn
            .db
            .transfer_order()
            .order_id()
            .find(&push_receipt.order_id)
            .context("cancelled push order disappeared")?;
        ensure!(
            order.status == OrderStatus::Cancelled,
            "cancelled push changed status after later simulation steps"
        );
        assert_order_conservation(&order)?;
        ensure!(
            !player_one
                .conn
                .db
                .transit_packet()
                .iter()
                .any(|packet| packet.order_id == order.order_id),
            "cancelled push emitted a later transit packet"
        );
        ensure!(
            lane_owners(&player_one.conn, &candidate.lane_cells)? == owners_at_cancel,
            "the cancelled operation continued acquiring cells along its original lane"
        );
        Ok(Some(()))
    })?;

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

    println!("[6/8] issuing one fixed-percentage neutral expansion across all fronts");
    let expand_candidate = select_expand_all_candidate(&player_one.conn, PLAYER_ONE)?;
    let expand_owners_before = owner_snapshot(&player_one.conn);
    let expand_id = unused_command_id(
        &player_one.conn,
        PLAYER_ONE,
        cancel_id
            .checked_add(1)
            .context("push-cancellation command ID overflow")?,
    )?;
    player_one.issue_expand_all(
        expand_id,
        &expand_candidate.selected_cells,
        expand_candidate.commitment_bps,
        timeout,
    )?;
    let expand_receipt = wait_for_receipt(
        &player_one,
        PLAYER_ONE,
        expand_id,
        "issue_expand_all",
        timeout,
        poll,
    )?;
    ensure!(
        expand_receipt.order_id != 0,
        "accepted all-front expansion receipt did not reference an order"
    );

    wait_until("all-front expansion persistence", timeout, poll, || {
        let Some(order) = player_one
            .conn
            .db
            .transfer_order()
            .order_id()
            .find(&expand_receipt.order_id)
        else {
            return Ok(None);
        };
        assert_expand_order(&order, &expand_candidate, expand_id)?;
        assert_order_conservation(&order)?;
        ensure!(
            order.status == OrderStatus::Active && order.in_transit_infantry > 0,
            "all-front expansion exhausted before cancellation coverage; use a fresh default fixture with a larger connected source pool"
        );
        assert_expand_persistence(&player_one.conn, &expand_candidate, &order)?;
        Ok(Some(order))
    })?;

    player_one.issue_expand_all(
        expand_id,
        &expand_candidate.selected_cells,
        expand_candidate.commitment_bps,
        timeout,
    )?;
    let matching_receipts = player_one
        .conn
        .db
        .command_receipt()
        .iter()
        .filter(|receipt| receipt.player_id == PLAYER_ONE && receipt.client_command_id == expand_id)
        .count();
    let matching_orders = player_one
        .conn
        .db
        .transfer_order()
        .iter()
        .filter(|order| order.player_id == PLAYER_ONE && order.client_command_id == expand_id)
        .count();
    ensure!(
        matching_receipts == 1 && matching_orders == 1,
        "idempotent all-front retry produced {matching_receipts} receipts and {matching_orders} orders"
    );

    let active_expand = wait_until(
        "branching all-front perimeter-wave progression",
        timeout,
        poll,
        || {
            let order = player_one
                .conn
                .db
                .transfer_order()
                .order_id()
                .find(&expand_receipt.order_id)
                .context("all-front order disappeared while observing lane progression")?;
            assert_expand_order(&order, &expand_candidate, expand_id)?;
            assert_order_conservation(&order)?;
            ensure!(
                order.status == OrderStatus::Active && order.in_transit_infantry > 0,
                "all-front expansion exhausted before its branching perimeter wave progressed; use a fresh default fixture with a larger connected source pool"
            );
            let current_owners = player_one
                .conn
                .db
                .cell_state()
                .iter()
                .map(|cell| (cell.cell_id, cell.owner_player_id))
                .collect::<HashMap<_, _>>();
            let captured = current_owners
                .iter()
                .filter_map(|(&cell_id, &owner)| {
                    (owner == PLAYER_ONE
                        && expand_owners_before.get(&cell_id).copied() != Some(PLAYER_ONE))
                    .then_some(cell_id)
                })
                .collect::<HashSet<_>>();
            ensure!(
                !captured.iter().any(|cell_id| {
                    expand_owners_before.get(cell_id).copied() == Some(PLAYER_TWO)
                }),
                "neutral-only all-front wave captured enemy territory"
            );
            let first_ring_captures = captured.intersection(&expand_candidate.first_ring).count();
            let turning_captures = captured
                .intersection(&expand_candidate.turning_second_ring)
                .count();
            if first_ring_captures < 2 || turning_captures == 0 {
                return Ok(None);
            }
            for &cell_id in &captured {
                let Some(&depth) = expand_candidate.outside_depths.get(&cell_id) else {
                    continue;
                };
                if depth <= 1 {
                    continue;
                }
                let has_owned_parent =
                    expand_candidate.children.iter().any(|(&parent, children)| {
                        children.contains(&cell_id)
                            && current_owners.get(&parent).copied() == Some(PLAYER_ONE)
                    });
                ensure!(
                    has_owned_parent,
                    "all-front wave captured outside depth {depth} cell {cell_id} without owning a depth {} parent",
                    depth - 1
                );
            }
            assert_expand_persistence(&player_one.conn, &expand_candidate, &order)?;
            Ok(Some(order))
        },
    )?;

    println!("[7/8] cancelling the perimeter wave and proving it remains stopped");
    let expand_cancel_id = unused_command_id(
        &player_one.conn,
        PLAYER_ONE,
        expand_id
            .checked_add(1)
            .context("all-front command ID overflow")?,
    )?;
    player_one.cancel_expand_all(expand_cancel_id, &expand_candidate.selected_cells, timeout)?;
    let expand_cancel_receipt = wait_for_receipt(
        &player_one,
        PLAYER_ONE,
        expand_cancel_id,
        "cancel_expand_all",
        timeout,
        poll,
    )?;
    ensure!(
        expand_cancel_receipt.order_id == expand_receipt.order_id,
        "all-front cancellation receipt referenced order {} instead of {}; use a fresh database if older expansions overlap this selection",
        expand_cancel_receipt.order_id,
        expand_receipt.order_id
    );

    let (cancelled_expand, owners_at_expand_cancel) =
        wait_until("all-front expansion cancellation", timeout, poll, || {
            let Some(order) = player_one
                .conn
                .db
                .transfer_order()
                .order_id()
                .find(&expand_receipt.order_id)
            else {
                return Ok(None);
            };
            if order.status != OrderStatus::Cancelled {
                return Ok(None);
            }
            assert_expand_order(&order, &expand_candidate, expand_id)?;
            assert_order_conservation(&order)?;
            ensure!(
                order.in_transit_infantry == 0,
                "cancelled all-front expansion retained {} in-transit infantry",
                order.in_transit_infantry
            );
            ensure!(
                !player_one
                    .conn
                    .db
                    .transit_packet()
                    .iter()
                    .any(|packet| packet.order_id == order.order_id),
                "cancelled all-front expansion retained transit packets"
            );
            assert_expand_sources(&player_one.conn, &expand_candidate, order.order_id, true)?;
            Ok(Some((order.clone(), owner_snapshot(&player_one.conn))))
        })?;
    ensure!(
        cancelled_expand.delivered_infantry > active_expand.delivered_infantry,
        "all-front cancellation did not release any of the {} infantry still in transit",
        active_expand.in_transit_infantry
    );
    ensure!(
        cancelled_expand.casualty_infantry == 0,
        "neutral-only expansion unexpectedly recorded {} casualties",
        cancelled_expand.casualty_infantry
    );

    wait_until(
        "post all-front cancellation simulation steps",
        timeout,
        poll,
        || {
            let Some(state) = player_one
                .conn
                .db
                .match_state()
                .singleton_id()
                .find(&SINGLETON_ID)
            else {
                return Ok(None);
            };
            if state.logical_step
                < cancelled_expand
                    .updated_step
                    .saturating_add(POST_CANCEL_STEPS)
            {
                return Ok(None);
            }
            let order = player_one
                .conn
                .db
                .transfer_order()
                .order_id()
                .find(&expand_receipt.order_id)
                .context("cancelled all-front order disappeared")?;
            ensure!(
                order.status == OrderStatus::Cancelled,
                "cancelled all-front expansion changed status after later simulation steps"
            );
            assert_order_conservation(&order)?;
            ensure!(
                order == cancelled_expand,
                "cancelled all-front order counters changed after later simulation steps"
            );
            ensure!(
                !player_one
                    .conn
                    .db
                    .transit_packet()
                    .iter()
                    .any(|packet| packet.order_id == order.order_id),
                "cancelled all-front expansion emitted a later transit packet"
            );
            ensure!(
                owner_snapshot(&player_one.conn) == owners_at_expand_cancel,
                "cell ownership changed after all-front cancellation"
            );
            Ok(Some(()))
        },
    )?;

    println!("[8/8] reconnecting player one with its persisted token");
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
        "PASS: receipts, idempotency, directional push, neutral all-front expansion, conservation/cancellation, and token reuse verified"
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
            let mut lane_cells = Vec::new();
            let mut previous_terrain = front_terrain;
            let mut next_coordinate = front_coordinate + direction;
            while let Some(next_id) = cell_by_coordinate.get(&next_coordinate).copied() {
                let Some(next_state) = cell_by_id.get(&next_id) else {
                    break;
                };
                let Some(next_terrain) = terrain_by_id.get(&next_id) else {
                    break;
                };
                if next_state.owner_player_id != 0
                    || next_state.infantry != 0
                    || !next_terrain.passable
                    || !next_terrain.capturable
                    || previous_terrain.elevation.abs_diff(next_terrain.elevation) > 1
                {
                    break;
                }
                lane_cells.push(next_id);
                previous_terrain = next_terrain;
                next_coordinate = next_coordinate + direction;
            }
            if lane_cells.len() < REQUIRED_LANE_CELLS {
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
                let garrison_buffer = lane_cells[..REQUIRED_LANE_CELLS - 1]
                    .iter()
                    .map(|cell_id| {
                        expected_occupation_garrison(
                            terrain_by_id
                                .get(cell_id)
                                .expect("lane terrain was validated above"),
                            cell_by_id
                                .get(cell_id)
                                .expect("lane state was validated above"),
                        )
                    })
                    .try_fold(0_u64, u64::checked_add);
                if garrison_buffer.is_none_or(|minimum| expected_requested <= minimum) {
                    continue;
                }
                candidates.push(PushFrontCandidate {
                    selected_cells,
                    front_cell: front_id,
                    lane_cells: lane_cells.clone(),
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
            std::cmp::Reverse(candidate.lane_cells.len()),
            std::cmp::Reverse(candidate.expected_requested),
            candidate.front_cell,
            candidate.lane_cells[0],
            candidate.direction,
        )
    });
    candidates.into_iter().next().context(
        "no connected owned corridor has enough unallocated infantry and four traversable neutral cells in one exact direction; run against a fresh default fixture",
    )
}

#[allow(clippy::too_many_lines)]
fn select_expand_all_candidate(conn: &DbConnection, player_id: u8) -> Result<ExpandAllCandidate> {
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
    let mut allocated_by_cell = HashMap::<u32, u64>::new();
    for packet in conn
        .db
        .transit_packet()
        .iter()
        .filter(|packet| packet.owner_player_id == player_id)
    {
        *allocated_by_cell.entry(packet.current_cell).or_default() += packet.infantry;
    }

    let owned_ground = cell_by_id
        .values()
        .filter_map(|cell| {
            let terrain = terrain_by_id.get(&cell.cell_id)?;
            (cell.owner_player_id == player_id && terrain.passable)
                .then_some(Axial::new(terrain.q, terrain.r))
        })
        .collect::<BTreeSet<_>>();
    let mut unvisited = owned_ground.clone();
    let mut components = Vec::<BTreeSet<Axial>>::new();
    while let Some(seed) = unvisited.first().copied() {
        let mut component = BTreeSet::from([seed]);
        let mut pending = VecDeque::from([seed]);
        unvisited.remove(&seed);
        while let Some(current) = pending.pop_front() {
            let current_id = cell_by_coordinate[&current];
            let current_terrain = &terrain_by_id[&current_id];
            for neighbor in current.neighbors() {
                if !unvisited.contains(&neighbor) {
                    continue;
                }
                let neighbor_id = cell_by_coordinate[&neighbor];
                let neighbor_terrain = &terrain_by_id[&neighbor_id];
                if current_terrain
                    .elevation
                    .abs_diff(neighbor_terrain.elevation)
                    > 1
                {
                    continue;
                }
                unvisited.remove(&neighbor);
                component.insert(neighbor);
                pending.push_back(neighbor);
            }
        }
        components.push(component);
    }

    let mut candidates = Vec::new();
    for component in components {
        let selected_cells = component
            .iter()
            .map(|coordinate| cell_by_coordinate[coordinate])
            .collect::<Vec<_>>();
        let selected_ids = selected_cells.iter().copied().collect::<BTreeSet<_>>();
        let mut expected_source_commitments = HashMap::new();
        let mut expected_requested = 0_u64;
        for &cell_id in &selected_cells {
            let state = &cell_by_id[&cell_id];
            let available = state
                .infantry
                .saturating_sub(allocated_by_cell.get(&cell_id).copied().unwrap_or(0));
            let committed = basis_point_share(available, EXPAND_COMMITMENT_BPS);
            expected_source_commitments.insert(cell_id, committed);
            expected_requested = expected_requested
                .checked_add(committed)
                .context("all-front candidate commitment overflow")?;
        }
        if expected_requested == 0 {
            continue;
        }

        let mut boundary = BTreeSet::new();
        let mut first_ring = BTreeSet::new();
        for &source_coordinate in &component {
            let source_id = cell_by_coordinate[&source_coordinate];
            for target_coordinate in source_coordinate.neighbors() {
                if component.contains(&target_coordinate) {
                    continue;
                }
                let Some(&target_id) = cell_by_coordinate.get(&target_coordinate) else {
                    continue;
                };
                let target_state = &cell_by_id[&target_id];
                let target_terrain = &terrain_by_id[&target_id];
                if target_state.owner_player_id == 0
                    && target_terrain.passable
                    && target_terrain.capturable
                    && terrain_edge_is_traversable(&terrain_by_id, source_id, target_id)
                {
                    boundary.insert(source_id);
                    first_ring.insert(target_id);
                }
            }
        }
        if first_ring.len() < 2 {
            continue;
        }

        let seed_depths = wave_seed_depths(
            &selected_ids,
            &boundary,
            &terrain_by_id,
            &cell_by_coordinate,
        );
        if seed_depths.len() != selected_cells.len() {
            continue;
        }
        let outside_depths = wave_outside_depths(
            player_id,
            &selected_ids,
            &first_ring,
            &terrain_by_id,
            &cell_by_id,
            &cell_by_coordinate,
        );
        let children = wave_children(
            &seed_depths,
            &outside_depths,
            &terrain_by_id,
            &cell_by_coordinate,
        );
        if !children.values().any(|targets| targets.len() >= 2) {
            continue;
        }

        let turning_second_ring =
            turning_second_ring_cells(&seed_depths, &outside_depths, &children, &terrain_by_id);
        let reached = forecast_wave_reach(
            &expected_source_commitments,
            &seed_depths,
            &outside_depths,
            &children,
            &terrain_by_id,
            &cell_by_id,
            u16::try_from(OBSERVED_CAPTURE_LAYERS).expect("observed capture layer count fits u16"),
        );
        let reached_first_ring = first_ring
            .iter()
            .filter(|cell_id| reached.contains(cell_id))
            .count();
        let reached_turns = turning_second_ring.intersection(&reached).count();
        if reached_first_ring < 2 || reached_turns == 0 {
            continue;
        }

        candidates.push(ExpandAllCandidate {
            selected_cells,
            commitment_bps: EXPAND_COMMITMENT_BPS,
            expected_requested,
            expected_source_commitments,
            seed_depths,
            outside_depths,
            children,
            first_ring: first_ring.into_iter().collect(),
            turning_second_ring,
        });
    }
    candidates.sort_unstable_by_key(|candidate| {
        (
            std::cmp::Reverse(candidate.expected_requested),
            std::cmp::Reverse(candidate.turning_second_ring.len()),
            std::cmp::Reverse(candidate.first_ring.len()),
            std::cmp::Reverse(candidate.selected_cells.len()),
        )
    });
    candidates.into_iter().next().context(
        "no traversably connected owned region can sustain a multi-branch, direction-changing neutral perimeter wave; run against a fresh default fixture",
    )
}

fn terrain_edge_is_traversable(
    terrain_by_id: &HashMap<u32, CellTerrain>,
    from_cell: u32,
    to_cell: u32,
) -> bool {
    terrain_by_id
        .get(&from_cell)
        .zip(terrain_by_id.get(&to_cell))
        .is_some_and(|(from, to)| {
            from.passable && to.passable && from.elevation.abs_diff(to.elevation) <= 1
        })
}

fn wave_seed_depths(
    selected: &BTreeSet<u32>,
    boundary: &BTreeSet<u32>,
    terrain_by_id: &HashMap<u32, CellTerrain>,
    cell_by_coordinate: &HashMap<Axial, u32>,
) -> HashMap<u32, u16> {
    let mut depths = HashMap::new();
    let mut pending = VecDeque::new();
    for &cell_id in boundary {
        depths.insert(cell_id, 0_u16);
        pending.push_back(cell_id);
    }
    while let Some(current_id) = pending.pop_front() {
        let depth = depths[&current_id];
        let current = &terrain_by_id[&current_id];
        for neighbor in Axial::new(current.q, current.r).neighbors() {
            let Some(&neighbor_id) = cell_by_coordinate.get(&neighbor) else {
                continue;
            };
            if !selected.contains(&neighbor_id)
                || depths.contains_key(&neighbor_id)
                || !terrain_edge_is_traversable(terrain_by_id, current_id, neighbor_id)
            {
                continue;
            }
            depths.insert(neighbor_id, depth.saturating_add(1));
            pending.push_back(neighbor_id);
        }
    }
    depths
}

fn wave_outside_depths(
    player_id: u8,
    selected: &BTreeSet<u32>,
    first_ring: &BTreeSet<u32>,
    terrain_by_id: &HashMap<u32, CellTerrain>,
    cell_by_id: &HashMap<u32, CellState>,
    cell_by_coordinate: &HashMap<Axial, u32>,
) -> HashMap<u32, u16> {
    let mut depths = HashMap::new();
    let mut pending = VecDeque::new();
    for &cell_id in first_ring {
        depths.insert(cell_id, 1_u16);
        pending.push_back(cell_id);
    }
    while let Some(current_id) = pending.pop_front() {
        let depth = depths[&current_id];
        let current = &terrain_by_id[&current_id];
        for neighbor in Axial::new(current.q, current.r).neighbors() {
            let Some(&neighbor_id) = cell_by_coordinate.get(&neighbor) else {
                continue;
            };
            if selected.contains(&neighbor_id) || depths.contains_key(&neighbor_id) {
                continue;
            }
            let terrain = &terrain_by_id[&neighbor_id];
            let owner = cell_by_id[&neighbor_id].owner_player_id;
            if !terrain.capturable
                || !matches!(owner, 0) && owner != player_id
                || !terrain_edge_is_traversable(terrain_by_id, current_id, neighbor_id)
            {
                continue;
            }
            depths.insert(neighbor_id, depth.saturating_add(1));
            pending.push_back(neighbor_id);
        }
    }
    depths
}

fn wave_children(
    seed_depths: &HashMap<u32, u16>,
    outside_depths: &HashMap<u32, u16>,
    terrain_by_id: &HashMap<u32, CellTerrain>,
    cell_by_coordinate: &HashMap<Axial, u32>,
) -> HashMap<u32, Vec<u32>> {
    let mut result = HashMap::new();
    for (&cell_id, &depth) in seed_depths {
        let wanted_seed = depth.checked_sub(1);
        let current = &terrain_by_id[&cell_id];
        let mut targets = Axial::new(current.q, current.r)
            .neighbors()
            .into_iter()
            .filter_map(|coordinate| cell_by_coordinate.get(&coordinate).copied())
            .filter(|target| terrain_edge_is_traversable(terrain_by_id, cell_id, *target))
            .filter(|target| {
                wanted_seed.map_or_else(
                    || outside_depths.get(target) == Some(&1),
                    |wanted| seed_depths.get(target) == Some(&wanted),
                )
            })
            .collect::<Vec<_>>();
        targets.sort_unstable();
        targets.dedup();
        result.insert(cell_id, targets);
    }
    for (&cell_id, &depth) in outside_depths {
        let current = &terrain_by_id[&cell_id];
        let mut targets = Axial::new(current.q, current.r)
            .neighbors()
            .into_iter()
            .filter_map(|coordinate| cell_by_coordinate.get(&coordinate).copied())
            .filter(|target| terrain_edge_is_traversable(terrain_by_id, cell_id, *target))
            .filter(|target| outside_depths.get(target) == Some(&depth.saturating_add(1)))
            .collect::<Vec<_>>();
        targets.sort_unstable();
        targets.dedup();
        result.insert(cell_id, targets);
    }
    result
}

fn turning_second_ring_cells(
    seed_depths: &HashMap<u32, u16>,
    outside_depths: &HashMap<u32, u16>,
    children: &HashMap<u32, Vec<u32>>,
    terrain_by_id: &HashMap<u32, CellTerrain>,
) -> HashSet<u32> {
    outside_depths
        .iter()
        .filter_map(|(&target, &depth)| (depth == 2).then_some(target))
        .filter(|target| {
            let target_coordinate = axial_for_cell(terrain_by_id, *target);
            children
                .iter()
                .filter(|(parent, targets)| {
                    outside_depths.get(parent) == Some(&1) && targets.contains(target)
                })
                .any(|(&parent, _)| {
                    let parent_coordinate = axial_for_cell(terrain_by_id, parent);
                    let outward = target_coordinate - parent_coordinate;
                    children.iter().any(|(&boundary, targets)| {
                        seed_depths.get(&boundary) == Some(&0)
                            && targets.contains(&parent)
                            && parent_coordinate - axial_for_cell(terrain_by_id, boundary)
                                != outward
                    })
                })
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn forecast_wave_reach(
    commitments: &HashMap<u32, u64>,
    seed_depths: &HashMap<u32, u16>,
    outside_depths: &HashMap<u32, u16>,
    children: &HashMap<u32, Vec<u32>>,
    terrain_by_id: &HashMap<u32, CellTerrain>,
    cell_by_id: &HashMap<u32, CellState>,
    max_depth: u16,
) -> HashSet<u32> {
    let mut pools = commitments.clone();
    let max_seed_depth = seed_depths.values().copied().max().unwrap_or(0);
    for depth in (1..=max_seed_depth).rev() {
        let cells = seed_depths
            .iter()
            .filter_map(|(&cell_id, &candidate)| (candidate == depth).then_some(cell_id))
            .collect::<Vec<_>>();
        for cell_id in cells {
            let amount = pools.remove(&cell_id).unwrap_or(0);
            distribute_wave_pool(amount, children.get(&cell_id), &mut pools);
        }
    }
    let boundary = seed_depths
        .iter()
        .filter_map(|(&cell_id, &depth)| (depth == 0).then_some(cell_id))
        .collect::<Vec<_>>();
    let mut incoming = HashMap::new();
    for cell_id in boundary {
        let amount = pools.remove(&cell_id).unwrap_or(0);
        distribute_wave_pool(amount, children.get(&cell_id), &mut incoming);
    }

    let mut reached = HashSet::new();
    for depth in 1..=max_depth {
        let current = std::mem::take(&mut incoming);
        for (cell_id, amount) in current {
            if amount == 0 || outside_depths.get(&cell_id) != Some(&depth) {
                continue;
            }
            reached.insert(cell_id);
            let state = &cell_by_id[&cell_id];
            let mobile = if state.owner_player_id == 0 {
                amount.saturating_sub(expected_occupation_garrison(
                    &terrain_by_id[&cell_id],
                    state,
                ))
            } else {
                amount
            };
            distribute_wave_pool(mobile, children.get(&cell_id), &mut incoming);
        }
    }
    reached
}

fn distribute_wave_pool(total: u64, targets: Option<&Vec<u32>>, incoming: &mut HashMap<u32, u64>) {
    let Some(targets) = targets.filter(|targets| !targets.is_empty()) else {
        return;
    };
    let parts = targets.len() as u64;
    for (index, &target) in targets.iter().enumerate() {
        let amount = total / parts + u64::from((index as u64) < total % parts);
        *incoming.entry(target).or_default() += amount;
    }
}

fn axial_for_cell(terrain_by_id: &HashMap<u32, CellTerrain>, cell_id: u32) -> Axial {
    let terrain = &terrain_by_id[&cell_id];
    Axial::new(terrain.q, terrain.r)
}

fn basis_point_share(value: u64, basis_points: u32) -> u64 {
    u64::try_from(u128::from(value) * u128::from(basis_points) / 10_000)
        .expect("a basis-point share cannot exceed its input")
}

fn expected_occupation_garrison(terrain: &CellTerrain, cell: &CellState) -> u64 {
    if cell.military_capacity == 0 || terrain.terrain == TerrainClass::Water {
        return 0;
    }
    let multiplier = match terrain.terrain {
        TerrainClass::Plains => 1,
        TerrainClass::Hills => 2,
        TerrainClass::Mountain => 3,
        TerrainClass::Water => 0,
    };
    cell.military_capacity
        .div_ceil(20)
        .max(1)
        .saturating_mul(multiplier)
        .min(cell.military_capacity)
}

fn lane_owners(conn: &DbConnection, lane_cells: &[u32]) -> Result<Vec<u8>> {
    lane_cells
        .iter()
        .map(|cell_id| {
            conn.db
                .cell_state()
                .cell_id()
                .find(cell_id)
                .map(|cell| cell.owner_player_id)
                .with_context(|| format!("lane cell {cell_id} disappeared"))
        })
        .collect()
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
            packet.destination_cell == candidate.lane_cells[0],
            "front-push packet did not retain the first layer as its stable lane anchor"
        );
        ensure!(
            packet.route.first() == Some(&packet.origin_cell),
            "front-push packet route did not begin at its selected origin"
        );
        let route_index = usize::try_from(packet.route_index).context("route index overflow")?;
        ensure!(
            packet.route.get(route_index) == Some(&packet.current_cell),
            "front-push packet current cell does not match its route index"
        );
        let anchor_index = packet
            .route
            .iter()
            .position(|cell_id| *cell_id == candidate.lane_cells[0])
            .context("front-push route omitted its stable first-layer lane anchor")?;
        ensure!(
            anchor_index > 0
                && packet.route[..anchor_index]
                    .iter()
                    .all(|cell_id| selected.contains(cell_id)),
            "front-push packet escaped the submitted corridor before entering its lane"
        );
        ensure!(
            packet.route.get(anchor_index - 1) == Some(&candidate.front_cell),
            "front-push packet did not leave through the selected front cell"
        );
        let lane_suffix = &packet.route[anchor_index..];
        ensure!(
            lane_suffix.len() <= candidate.lane_cells.len()
                && lane_suffix
                    .iter()
                    .zip(&candidate.lane_cells)
                    .all(|(actual, expected)| actual == expected),
            "front-push packet route did not extend along the submitted axial ray"
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
        for cells in packet.route[anchor_index - 1..].windows(2) {
            let from = terrain_by_id
                .get(&cells[0])
                .with_context(|| format!("route terrain {} disappeared", cells[0]))?;
            let to = terrain_by_id
                .get(&cells[1])
                .with_context(|| format!("route terrain {} disappeared", cells[1]))?;
            ensure!(
                Axial::new(to.q, to.r) - Axial::new(from.q, from.r) == candidate.direction,
                "front-push lane changed direction after leaving the selected corridor"
            );
        }
        has_rear_corridor_route |= packet.origin_cell != candidate.front_cell && anchor_index >= 2;
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

fn assert_expand_order(
    order: &TransferOrder,
    candidate: &ExpandAllCandidate,
    command_id: u64,
) -> Result<()> {
    ensure!(
        order.player_id == PLAYER_ONE && order.client_command_id == command_id,
        "all-front receipt referenced an order owned by another command"
    );
    ensure!(
        order.kind == OrderKind::ExpandAll,
        "all-front receipt referenced {:?} order {}",
        order.kind,
        order.order_id
    );
    ensure!(
        (order.orientation_q, order.orientation_r) == (0, 0),
        "unoriented all-front order persisted orientation ({}, {})",
        order.orientation_q,
        order.orientation_r
    );
    ensure!(
        order.requested_infantry == candidate.expected_requested
            && order.committed_infantry == candidate.expected_requested,
        "{} bps all-front order requested/committed {}/{} infantry instead of the one-time expected {}",
        candidate.commitment_bps,
        order.requested_infantry,
        order.committed_infantry,
        candidate.expected_requested
    );
    Ok(())
}

fn assert_expand_sources(
    conn: &DbConnection,
    candidate: &ExpandAllCandidate,
    order_id: u64,
    require_released: bool,
) -> Result<()> {
    let actual = conn
        .db
        .transfer_source()
        .iter()
        .filter(|source| source.order_id == order_id)
        .map(|source| {
            (
                source.cell_id,
                (source.committed_infantry, source.queued_infantry),
            )
        })
        .collect::<HashMap<_, _>>();
    ensure!(
        actual.len() == candidate.expected_source_commitments.len(),
        "all-front order persisted {} positive sources instead of {}",
        actual.len(),
        candidate.expected_source_commitments.len()
    );
    for (&cell_id, &expected) in &candidate.expected_source_commitments {
        let (committed, queued) = actual
            .get(&cell_id)
            .copied()
            .with_context(|| format!("all-front source cell {cell_id} was not persisted"))?;
        ensure!(
            committed == expected,
            "all-front source {cell_id} committed {committed} infantry instead of one {} bps share {expected}",
            candidate.commitment_bps
        );
        ensure!(
            queued <= committed,
            "all-front source {cell_id} queued {queued} infantry beyond its {committed} commitment"
        );
        if require_released {
            ensure!(
                queued == 0,
                "cancelled all-front source {cell_id} retained {queued} queued infantry"
            );
        }
    }
    Ok(())
}

fn assert_expand_persistence(
    conn: &DbConnection,
    candidate: &ExpandAllCandidate,
    order: &TransferOrder,
) -> Result<()> {
    assert_expand_sources(conn, candidate, order.order_id, false)?;
    let selected = candidate
        .selected_cells
        .iter()
        .copied()
        .collect::<HashSet<_>>();
    ensure!(
        !conn
            .db
            .transfer_destination()
            .iter()
            .any(|destination| destination.order_id == order.order_id),
        "branching all-front wave unexpectedly exposed stable destination anchors"
    );

    let packets = conn
        .db
        .transit_packet()
        .iter()
        .filter(|packet| packet.order_id == order.order_id)
        .collect::<Vec<_>>();
    ensure!(
        !packets.is_empty(),
        "active all-front order has no transit packets"
    );
    let mut packet_total = 0_u64;
    let mut queued_by_source = HashMap::<u32, u64>::new();
    for packet in packets {
        ensure!(
            packet.owner_player_id == PLAYER_ONE,
            "all-front packet belongs to another player"
        );
        ensure!(
            packet.route_index == 0 && packet.route.first() == Some(&packet.current_cell),
            "all-front packet is not positioned at the start of its local route"
        );
        let resting = packet.route.as_slice() == [packet.current_cell]
            && packet.destination_cell == packet.current_cell;
        let crossing = packet.route.len() == 2
            && packet.route[1] == packet.destination_cell
            && candidate
                .children
                .get(&packet.current_cell)
                .is_some_and(|children| children.contains(&packet.destination_cell));
        ensure!(
            resting || crossing,
            "all-front packet must be one resting node or one monotonic wave edge"
        );
        ensure!(
            candidate.seed_depths.contains_key(&packet.current_cell)
                || candidate.outside_depths.contains_key(&packet.current_cell),
            "all-front packet rests outside its accepted seed/wave topology"
        );
        if packet.origin_cell != EXPANSION_AGGREGATE_ORIGIN {
            ensure!(
                selected.contains(&packet.origin_cell) && packet.current_cell == packet.origin_cell,
                "unmerged all-front packet left or misidentified its selected source"
            );
            *queued_by_source.entry(packet.origin_cell).or_default() += packet.infantry;
        }
        packet_total = packet_total
            .checked_add(packet.infantry)
            .context("all-front packet accounting overflow")?;
    }
    ensure!(
        packet_total == order.in_transit_infantry,
        "all-front packet total {packet_total} differs from order in-transit infantry {}",
        order.in_transit_infantry
    );
    for source in conn
        .db
        .transfer_source()
        .iter()
        .filter(|source| source.order_id == order.order_id)
    {
        ensure!(
            source.queued_infantry == queued_by_source.get(&source.cell_id).copied().unwrap_or(0),
            "all-front source {} reports {} queued but has {} source-backed packet strength",
            source.cell_id,
            source.queued_infantry,
            queued_by_source.get(&source.cell_id).copied().unwrap_or(0)
        );
    }
    Ok(())
}

fn owner_snapshot(conn: &DbConnection) -> HashMap<u32, u8> {
    conn.db
        .cell_state()
        .iter()
        .map(|cell| (cell.cell_id, cell.owner_player_id))
        .collect()
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
