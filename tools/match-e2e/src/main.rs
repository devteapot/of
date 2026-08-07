#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
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
use hex_core::{
    Axial, Cell as CoreCell, ForceComposition, HexMap, TerrainKind,
    redistribution_targets_with_fallback_constraints,
};
use match_bindings::{
    CellState, CellStateTableAccess, CellTerrain, CellTerrainTableAccess, CommandReceipt,
    CommandReceiptTableAccess, DbConnection, MatchConfigTableAccess, MatchPhase,
    MatchStateTableAccess, MobilizationPolicyTableAccess, OrderKind, OrderStatus,
    PlayerSlotTableAccess, ReceiptStatus, TerrainClass, TransferDestinationTableAccess,
    TransferOrder, TransferOrderTableAccess, TransferSourceTableAccess, TransitPacket,
    TransitPacketTableAccess, TransitRouteTableAccess, cancel_orders as _,
    issue_attack_clusters as _, issue_expand_all as _, issue_expand_clusters as _,
    issue_push_front as _, issue_reshape as _, join_match as _, set_mobilization_target as _,
    start_match as _,
};
use spacetimedb_sdk::{DbContext, Identity, Table};

const PLAYER_ONE: u16 = 1;
const PLAYER_TWO: u16 = 2;
const SINGLETON_ID: u8 = 0;
const COMMAND_ID_FLOOR: u64 = 9_000_000_000;
const PUSH_COMMITMENT_BPS: u32 = 5_000;
const EXPAND_COMMITMENT_BPS: u32 = 10_000;
const CLUSTER_ACTION_COMMITMENT_BPS: u32 = 10_000;
const CONTACT_EXPAND_COMMITMENT_BPS: u32 = 10_000;
const MAX_PUSH_CORRIDOR_CELLS: usize = 5;
const REQUIRED_LANE_CELLS: usize = 4;
const OBSERVED_CAPTURE_LAYERS: usize = 2;
const POST_CANCEL_STEPS: u64 = 2;
const EXPANSION_AGGREGATE_ORIGIN: u32 = u32::MAX;

const fn receipt_key(player_id: u16, command_id: u64) -> u128 {
    (player_id as u128) << 64 | command_id as u128
}

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

    /// Optional override for player one's token file (for example a match-perf
    /// `player-1.token` during reconnect-under-load soaks).
    #[arg(long)]
    player_one_token: Option<PathBuf>,

    /// Optional override for player two's token file / reconnect observer.
    #[arg(long)]
    player_two_token: Option<PathBuf>,

    /// Maximum time allowed for each asynchronous phase.
    #[arg(long, default_value_t = 60)]
    timeout_secs: u64,

    /// Client-cache polling interval.
    #[arg(long, default_value_t = 20)]
    poll_ms: u64,

    /// Skip the gameplay smoke and only exercise reconnect reclaim cycles.
    /// Requires a published database; joins seats and calls `start_match` when
    /// the match is still in Lobby.
    #[arg(long, default_value_t = false)]
    reconnect_only: bool,

    /// Extra disconnect/reconnect cycles after the functional reclaim proof
    /// (or the sole workload when `--reconnect-only` is set).
    #[arg(long, default_value_t = 0)]
    reconnect_cycles: u32,

    /// Optional JSON report path for reconnect soak timings.
    #[arg(long)]
    reconnect_report: Option<PathBuf>,
}

#[derive(Debug, serde::Serialize)]
struct ReconnectCycleReport {
    cycle: u32,
    disconnect_to_reclaim_ms: u128,
    reconnect_count_after: u32,
}

#[derive(Debug, serde::Serialize)]
struct ReconnectSoakReport {
    kind: &'static str,
    host: String,
    database: String,
    cycles_requested: u32,
    cycles_completed: u32,
    p50_ms: u128,
    p95_ms: u128,
    max_ms: u128,
    cycles: Vec<ReconnectCycleReport>,
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

    fn join_match(&self, player_id: u16, display_name: &str, timeout: Duration) -> Result<()> {
        let (tx, rx) = mpsc::channel();
        self.conn
            .reducers
            .join_match_then(player_id, display_name.to_owned(), move |_, result| {
                let _ = tx.send(flatten_reducer_result(result));
            })
            .with_context(|| format!("send join_match for {}", self.label))?;
        wait_for_reducer(&rx, timeout, &format!("{} join_match", self.label))
    }

    fn start_match(&self, timeout: Duration) -> Result<()> {
        let (tx, rx) = mpsc::channel();
        self.conn
            .reducers
            .start_match_then(move |_, result| {
                let _ = tx.send(flatten_reducer_result(result));
            })
            .with_context(|| format!("send start_match for {}", self.label))?;
        wait_for_reducer(&rx, timeout, &format!("{} start_match", self.label))
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
        supersede_order_ids: &[u64],
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
                supersede_order_ids.to_vec(),
                move |_, result| {
                    let _ = tx.send(flatten_reducer_result(result));
                },
            )
            .context("send issue_push_front")?;
        wait_for_reducer(&rx, timeout, "issue_push_front")
    }

    fn cancel_orders(
        &self,
        command_id: u64,
        selected_order_ids: &[u64],
        timeout: Duration,
    ) -> Result<()> {
        let (tx, rx) = mpsc::channel();
        self.conn
            .reducers
            .cancel_orders_then(command_id, selected_order_ids.to_vec(), move |_, result| {
                let _ = tx.send(flatten_reducer_result(result));
            })
            .context("send cancel_orders")?;
        wait_for_reducer(&rx, timeout, "cancel_orders")
    }

    fn issue_expand_all(
        &self,
        command_id: u64,
        selected_cells: &[u32],
        commitment_bps: u32,
        supersede_order_ids: &[u64],
        timeout: Duration,
    ) -> Result<()> {
        let (tx, rx) = mpsc::channel();
        self.conn
            .reducers
            .issue_expand_all_then(
                command_id,
                selected_cells.to_vec(),
                commitment_bps,
                supersede_order_ids.to_vec(),
                move |_, result| {
                    let _ = tx.send(flatten_reducer_result(result));
                },
            )
            .context("send issue_expand_all")?;
        wait_for_reducer(&rx, timeout, "issue_expand_all")
    }

    fn issue_expand_clusters(
        &self,
        command_id: u64,
        source_seed_cells: &[u32],
        focus_cell_id: u32,
        commitment_bps: u32,
        timeout: Duration,
    ) -> Result<()> {
        let (tx, rx) = mpsc::channel();
        self.conn
            .reducers
            .issue_expand_clusters_then(
                command_id,
                source_seed_cells.to_vec(),
                focus_cell_id,
                commitment_bps,
                move |_, result| {
                    let _ = tx.send(flatten_reducer_result(result));
                },
            )
            .context("send issue_expand_clusters")?;
        wait_for_reducer(&rx, timeout, "issue_expand_clusters")
    }

    fn issue_attack_clusters(
        &self,
        command_id: u64,
        source_seed_cells: &[u32],
        target_seed_cells: &[u32],
        commitment_bps: u32,
        timeout: Duration,
    ) -> Result<()> {
        let (tx, rx) = mpsc::channel();
        self.conn
            .reducers
            .issue_attack_clusters_then(
                command_id,
                source_seed_cells.to_vec(),
                target_seed_cells.to_vec(),
                commitment_bps,
                move |_, result| {
                    let _ = tx.send(flatten_reducer_result(result));
                },
            )
            .context("send issue_attack_clusters")?;
        wait_for_reducer(&rx, timeout, "issue_attack_clusters")
    }

    fn issue_reshape(
        &self,
        command_id: u64,
        source_cells: &[u32],
        target_cells: &[u32],
        supersede_order_ids: &[u64],
        timeout: Duration,
    ) -> Result<()> {
        let (tx, rx) = mpsc::channel();
        self.conn
            .reducers
            .issue_reshape_then(
                command_id,
                source_cells.to_vec(),
                target_cells.to_vec(),
                supersede_order_ids.to_vec(),
                move |_, result| {
                    let _ = tx.send(flatten_reducer_result(result));
                },
            )
            .context("send issue_reshape")?;
        wait_for_reducer(&rx, timeout, "issue_reshape")
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
    perimeter_sources: BTreeSet<u32>,
    outside_depths: HashMap<u32, u16>,
    children: HashMap<u32, Vec<u32>>,
    first_ring: HashSet<u32>,
    turning_second_ring: HashSet<u32>,
}

#[derive(Clone, Debug)]
struct FocusedClusterExpandCandidate {
    source_seed: u32,
    source_component: BTreeSet<u32>,
    approach_source: u32,
    focus_cell: u32,
    focus_distance: u32,
    expected_source_commitments: HashMap<u32, u64>,
    expected_requested: u64,
}

#[derive(Clone, Debug)]
struct ClusterAttackCandidate {
    source_seed: u32,
    source_component: BTreeSet<u32>,
    target_seed: u32,
    target_component: BTreeSet<u32>,
    shared_front_targets: BTreeSet<u32>,
    outside_guard_cells: BTreeSet<u32>,
    expected_source_commitments: HashMap<u32, u64>,
    expected_requested: u64,
}

#[derive(Clone, Debug, PartialEq)]
struct ActionOrderSnapshot {
    logical_step: u64,
    status: OrderStatus,
    requested_infantry: u64,
    committed_infantry: u64,
    in_transit_infantry: u64,
    delivered_infantry: u64,
    casualty_infantry: u64,
    updated_step: u64,
    packets: Vec<TransitPacket>,
}

#[derive(Clone, Debug)]
struct InternalPlan {
    command_name: &'static str,
    kind: OrderKind,
    orientation: Axial,
    expected_requested: u64,
    expected_strength_by_cell: HashMap<u32, u64>,
}

#[derive(Clone, Debug)]
struct ClusterReshapeCandidate {
    source_component: BTreeSet<u32>,
    source_seed: u32,
    target_cell: u32,
    target_capacity: u64,
    expected_overflow: u64,
    plan: InternalPlan,
    invalid_target: u32,
}

#[allow(clippy::too_many_lines)]
fn main() -> Result<()> {
    let args = Args::parse();
    ensure!(args.timeout_secs > 0, "--timeout-secs must be positive");
    ensure!(args.poll_ms > 0, "--poll-ms must be positive");
    if args.reconnect_only {
        ensure!(
            args.reconnect_cycles > 0,
            "--reconnect-only requires --reconnect-cycles > 0"
        );
    }
    let timeout = Duration::from_secs(args.timeout_secs);
    let poll = Duration::from_millis(args.poll_ms);

    let player_one_token = args
        .player_one_token
        .clone()
        .unwrap_or_else(|| args.token_dir.join("player-one.token"));
    let player_two_token = args
        .player_two_token
        .clone()
        .unwrap_or_else(|| args.token_dir.join("player-two.token"));

    println!("[1/10] connecting two persistent identity profiles");
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

    println!("[2/10] claiming player slots and waiting for a running match");
    player_one.join_match(PLAYER_ONE, "E2E Player 1", timeout)?;
    player_two.join_match(PLAYER_TWO, "E2E Player 2", timeout)?;
    wait_for_slot(&player_one, PLAYER_ONE, player_one.identity, timeout, poll)?;
    wait_for_slot(&player_one, PLAYER_TWO, player_two.identity, timeout, poll)?;
    ensure_match_running(&player_one, timeout, poll)?;
    let running_step = wait_until("match phase Running", timeout, poll, || {
        let state = player_one
            .conn
            .db
            .match_state()
            .singleton_id()
            .find(&SINGLETON_ID);
        Ok(state.and_then(|row| (row.phase == MatchPhase::Running).then_some(row.logical_step)))
    })?;

    if args.reconnect_only {
        println!(
            "reconnect-only: skipping gameplay smoke; running {} reclaim cycles",
            args.reconnect_cycles
        );
        let report = run_reconnect_soak(
            &mut player_one,
            &player_two,
            &player_one_token,
            &args.host,
            &args.database,
            args.reconnect_cycles,
            timeout,
            poll,
        )?;
        write_reconnect_report(args.reconnect_report.as_deref(), &report)?;
        player_one.disconnect(timeout)?;
        player_two.disconnect(timeout)?;
        println!(
            "PASS: reconnect soak completed {} cycles (p50={}ms p95={}ms max={}ms)",
            report.cycles_completed, report.p50_ms, report.p95_ms, report.max_ms
        );
        return Ok(());
    }

    println!("[3/10] verifying idempotent mobilization and its receipt");
    let mobilization_id = unused_command_id(&player_one.conn, PLAYER_ONE, COMMAND_ID_FLOOR)?;
    // Keep recruitment stopped after proving the mobilization command. That makes
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

    println!("[4/10] exercising whole-cluster best-effort Reshape and atomic rejection");
    let internal_control_id = exercise_internal_controls(
        &player_one,
        mobilization_id
            .checked_add(1)
            .context("mobilization command ID overflow")?,
        timeout,
        poll,
    )?;

    println!("[5/10] issuing an authoritative directional front push");
    let candidate = select_push_front_candidate(&player_one.conn, PLAYER_ONE)?;
    let push_id = unused_command_id(
        &player_one.conn,
        PLAYER_ONE,
        internal_control_id
            .checked_add(1)
            .context("whole-cluster Reshape command ID overflow")?,
    )?;
    player_one.issue_push_front(
        push_id,
        &candidate.selected_cells,
        candidate.direction,
        candidate.commitment_bps,
        &[],
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
            "front-push order did not persist its commanded front target"
        );
        Ok(Some(()))
    })?;

    player_one.issue_push_front(
        push_id,
        &candidate.selected_cells,
        candidate.direction,
        candidate.commitment_bps,
        &[],
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

    let rejected_retask_id = unused_command_id(
        &player_one.conn,
        PLAYER_ONE,
        push_id.checked_add(1).context("push command ID overflow")?,
    )?;
    player_one.issue_push_front(
        rejected_retask_id,
        &[],
        Axial::new(2, 0),
        candidate.commitment_bps,
        &[push_receipt.order_id],
        timeout,
    )?;
    let rejected_retask = wait_for_rejected_receipt(
        &player_one,
        PLAYER_ONE,
        rejected_retask_id,
        "issue_push_front",
        timeout,
        poll,
    )?;
    ensure!(
        rejected_retask.message.contains("direction"),
        "invalid replacement was rejected for an unexpected reason: {}",
        rejected_retask.message
    );
    let preserved_order = player_one
        .conn
        .db
        .transfer_order()
        .order_id()
        .find(&push_receipt.order_id)
        .context("failed replacement removed the original push")?;
    ensure!(
        preserved_order.status == OrderStatus::Active && preserved_order.in_transit_infantry > 0,
        "failed replacement did not preserve an active original push with surviving packets"
    );
    assert_order_conservation(&preserved_order)?;
    let preserved_packet_total = player_one
        .conn
        .db
        .transit_packet()
        .iter()
        .filter(|packet| packet.order_id == push_receipt.order_id)
        .try_fold(0_u64, |total, packet| {
            total
                .checked_add(packet.infantry)
                .context("preserved packet strength overflow")
        })?;
    ensure!(
        preserved_packet_total == preserved_order.in_transit_infantry,
        "failed replacement left {} packet infantry for an order reporting {} in transit",
        preserved_packet_total,
        preserved_order.in_transit_infantry
    );

    let retask_id = unused_command_id(
        &player_one.conn,
        PLAYER_ONE,
        rejected_retask_id
            .checked_add(1)
            .context("rejected retask command ID overflow")?,
    )?;
    player_one.issue_push_front(
        retask_id,
        &[],
        candidate.direction,
        candidate.commitment_bps,
        &[push_receipt.order_id],
        timeout,
    )?;
    let retask_receipt = wait_for_receipt(
        &player_one,
        PLAYER_ONE,
        retask_id,
        "issue_push_front",
        timeout,
        poll,
    )?;
    ensure!(
        retask_receipt.order_id != 0 && retask_receipt.order_id != push_receipt.order_id,
        "retask did not create a distinct replacement order"
    );
    let retasked_old_order = wait_until("atomic front-push replacement", timeout, poll, || {
        let Some(old_order) = player_one
            .conn
            .db
            .transfer_order()
            .order_id()
            .find(&push_receipt.order_id)
        else {
            return Ok(None);
        };
        let Some(replacement) = player_one
            .conn
            .db
            .transfer_order()
            .order_id()
            .find(&retask_receipt.order_id)
        else {
            return Ok(None);
        };
        if old_order.status != OrderStatus::Cancelled
            || replacement.status != OrderStatus::Active
            || replacement.in_transit_infantry == 0
        {
            return Ok(None);
        }
        assert_order_conservation(&old_order)?;
        assert_order_conservation(&replacement)?;
        ensure!(
            !player_one
                .conn
                .db
                .transit_packet()
                .iter()
                .any(|packet| packet.order_id == old_order.order_id),
            "superseded push retained transit packets"
        );
        Ok(Some(old_order))
    })?;
    ensure!(
        retasked_old_order.delivered_infantry > 0,
        "retasking did not settle any surviving strength on the old order"
    );
    let uncaptured_after_retask = candidate
        .lane_cells
        .iter()
        .copied()
        .filter(|cell_id| {
            player_one
                .conn
                .db
                .cell_state()
                .cell_id()
                .find(cell_id)
                .is_some_and(|cell| cell.owner_player_id != PLAYER_ONE)
        })
        .collect::<Vec<_>>();
    ensure!(
        uncaptured_after_retask.len() >= OBSERVED_CAPTURE_LAYERS,
        "only {} lane cell(s) remained uncaptured after retasking; the live fixture advanced too far to prove replacement progression",
        uncaptured_after_retask.len()
    );
    let replacement_capture_targets = uncaptured_after_retask[..OBSERVED_CAPTURE_LAYERS].to_vec();

    println!("[6/10] observing retasked progression, then cancelling the replacement push");
    let mut observed_packet_progress = false;
    let active_order = wait_until("two successive front-push captures", timeout, poll, || {
        let Some(order) = player_one
            .conn
            .db
            .transfer_order()
            .order_id()
            .find(&retask_receipt.order_id)
        else {
            return Ok(None);
        };
        ensure!(
            order.player_id == PLAYER_ONE
                && order.client_command_id == retask_id
                && order.kind == OrderKind::PushFront
                && (order.orientation_q, order.orientation_r)
                    == (candidate.direction.q, candidate.direction.r),
            "replacement receipt referenced an invalid Push Front order"
        );
        assert_order_conservation(&order)?;
        let packets = player_one
            .conn
            .db
            .transit_packet()
            .iter()
            .filter(|packet| packet.order_id == order.order_id)
            .collect::<Vec<_>>();
        observed_packet_progress |= packets
            .iter()
            .any(|packet| packet.route_index > 0 || packet.updated_step > order.created_step);
        let captured_layers = replacement_capture_targets
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
        "replacement front-push order predates the observed running match"
    );

    let unknown_order_id = unused_order_id(&player_one.conn)?;
    let rejected_cancel_id = unused_command_id(
        &player_one.conn,
        PLAYER_ONE,
        retask_id
            .checked_add(1)
            .context("retask command ID overflow")?,
    )?;
    let order_before_rejected_cancel = player_one
        .conn
        .db
        .transfer_order()
        .order_id()
        .find(&active_order.order_id)
        .context("active replacement push disappeared before rejected cancellation")?;
    player_one.cancel_orders(
        rejected_cancel_id,
        &[active_order.order_id, unknown_order_id],
        timeout,
    )?;
    wait_for_rejected_receipt(
        &player_one,
        PLAYER_ONE,
        rejected_cancel_id,
        "cancel_orders",
        timeout,
        poll,
    )?;
    let order_after_rejected_cancel = player_one
        .conn
        .db
        .transfer_order()
        .order_id()
        .find(&active_order.order_id)
        .context("rejected cancellation removed the active replacement push")?;
    assert_order_conservation(&order_after_rejected_cancel)?;
    ensure!(
        order_after_rejected_cancel.status == OrderStatus::Active
            && order_after_rejected_cancel.in_transit_infantry > 0
            && order_after_rejected_cancel.committed_infantry
                == order_before_rejected_cancel.committed_infantry
            && player_one
                .conn
                .db
                .transit_packet()
                .iter()
                .any(|packet| packet.order_id == active_order.order_id),
        "rejected exact cancellation did not preserve the valid active order"
    );
    let cancel_id = unused_command_id(
        &player_one.conn,
        PLAYER_ONE,
        rejected_cancel_id
            .checked_add(1)
            .context("rejected cancellation command ID overflow")?,
    )?;
    let replacement_sources = player_one
        .conn
        .db
        .transfer_source()
        .iter()
        .filter(|source| source.order_id == retask_receipt.order_id)
        .map(|source| source.cell_id)
        .collect::<Vec<_>>();
    ensure!(
        !replacement_sources.is_empty(),
        "replacement push persisted no physical source cells"
    );
    player_one.cancel_orders(cancel_id, &[retask_receipt.order_id], timeout)?;
    let cancel_receipt = wait_for_receipt(
        &player_one,
        PLAYER_ONE,
        cancel_id,
        "cancel_orders",
        timeout,
        poll,
    )?;
    ensure!(
        cancel_receipt.order_id == retask_receipt.order_id,
        "cancellation receipt referenced order {} instead of the replacement push {}; use a fresh database if older active pushes overlap this selection",
        cancel_receipt.order_id,
        retask_receipt.order_id
    );

    let (cancelled_order, owners_at_cancel) =
        wait_until("front-push cancellation", timeout, poll, || {
            let Some(order) = player_one
                .conn
                .db
                .transfer_order()
                .order_id()
                .find(&retask_receipt.order_id)
            else {
                return Ok(None);
            };
            if order.status != OrderStatus::Cancelled {
                return Ok(None);
            }
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
                sources.len() == replacement_sources.len()
                    && sources.iter().all(|source| source.queued_infantry == 0),
                "cancellation did not release every replacement source allocation"
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
            .find(&retask_receipt.order_id)
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

    println!("[7/10] issuing one fixed-percentage neutral expansion across all fronts");
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
        &[],
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
        &[],
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

    println!("[8/10] cancelling the perimeter wave and proving it remains stopped");
    let expand_cancel_id = unused_command_id(
        &player_one.conn,
        PLAYER_ONE,
        expand_id
            .checked_add(1)
            .context("all-front command ID overflow")?,
    )?;
    player_one.cancel_orders(expand_cancel_id, &[expand_receipt.order_id], timeout)?;
    let expand_cancel_receipt = wait_for_receipt(
        &player_one,
        PLAYER_ONE,
        expand_cancel_id,
        "cancel_orders",
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

    println!("[9/10] proving cluster-first expansion and attack controls");
    let _cluster_control_id = exercise_cluster_first_controls(
        &player_two,
        expand_cancel_id
            .checked_add(1)
            .context("all-front cancellation command ID overflow")?,
        timeout,
        poll,
    )?;
    println!("[10/10] reconnecting player one with its persisted token");
    let functional = run_reconnect_soak(
        &mut player_one,
        &player_two,
        &player_one_token,
        &args.host,
        &args.database,
        1,
        timeout,
        poll,
    )?;
    ensure!(
        functional.cycles_completed == 1,
        "functional reconnect reclaim did not complete"
    );

    if args.reconnect_cycles > 0 {
        println!(
            "running {} additional reconnect soak cycles",
            args.reconnect_cycles
        );
        let soak = run_reconnect_soak(
            &mut player_one,
            &player_two,
            &player_one_token,
            &args.host,
            &args.database,
            args.reconnect_cycles,
            timeout,
            poll,
        )?;
        write_reconnect_report(args.reconnect_report.as_deref(), &soak)?;
        println!(
            "reconnect soak: {} cycles p50={}ms p95={}ms max={}ms",
            soak.cycles_completed, soak.p50_ms, soak.p95_ms, soak.max_ms
        );
    } else if let Some(path) = args.reconnect_report.as_deref() {
        write_reconnect_report(Some(path), &functional)?;
    }

    player_one.disconnect(timeout)?;
    player_two.disconnect(timeout)?;
    println!(
        "PASS: receipts, sparse-seed whole-cluster best-effort Reshape, atomic invalid-shape rejection, directional Push and retasking, neutral perimeter expansion, cluster-first expansion/attack, conservation/cancellation, and token reuse verified"
    );
    Ok(())
}

fn ensure_match_running(client: &Client, timeout: Duration, poll: Duration) -> Result<()> {
    let phase = client
        .conn
        .db
        .match_state()
        .singleton_id()
        .find(&SINGLETON_ID)
        .map(|state| state.phase);
    match phase {
        Some(MatchPhase::Running) => Ok(()),
        Some(MatchPhase::Lobby) => {
            match client.start_match(timeout) {
                Ok(()) => {}
                Err(error) => {
                    let message = error.to_string();
                    let already_running = client
                        .conn
                        .db
                        .match_state()
                        .singleton_id()
                        .find(&SINGLETON_ID)
                        .is_some_and(|state| state.phase == MatchPhase::Running);
                    if !already_running {
                        return Err(error).context(format!(
                            "start_match failed while match remained non-running: {message}"
                        ));
                    }
                }
            }
            wait_until("match phase Running after start_match", timeout, poll, || {
                let state = client
                    .conn
                    .db
                    .match_state()
                    .singleton_id()
                    .find(&SINGLETON_ID);
                Ok(state.and_then(|row| (row.phase == MatchPhase::Running).then_some(())))
            })?;
            Ok(())
        }
        Some(other) => bail!("match is in unexpected phase {other:?}; expected Lobby or Running"),
        None => bail!("match state is missing before start/reconnect"),
    }
}

#[allow(clippy::too_many_arguments)]
fn run_reconnect_soak(
    player_one: &mut Client,
    observer: &Client,
    token_path: &Path,
    host: &str,
    database: &str,
    cycles: u32,
    timeout: Duration,
    poll: Duration,
) -> Result<ReconnectSoakReport> {
    ensure!(cycles > 0, "reconnect soak requires at least one cycle");
    let original_identity = player_one.identity;
    let mut cycle_reports = Vec::with_capacity(cycles as usize);

    for cycle in 1..=cycles {
        let reconnect_count_before = observer
            .conn
            .db
            .player_slot()
            .player_id()
            .find(&PLAYER_ONE)
            .context("player one slot disappeared before reconnect soak")?
            .reconnect_count;
        let started = Instant::now();
        player_one.disconnect(timeout)?;
        wait_until(
            &format!("player one disconnect visibility (cycle {cycle})"),
            timeout,
            poll,
            || {
                Ok(observer
                    .conn
                    .db
                    .player_slot()
                    .player_id()
                    .find(&PLAYER_ONE)
                    .and_then(|slot| (!slot.connected).then_some(())))
            },
        )?;
        let reconnected = Client::connect(
            "player one reconnect",
            token_path,
            host,
            database,
            timeout,
        )?;
        ensure!(
            reconnected.identity == original_identity,
            "persisted player-one token resolved to a different identity on cycle {cycle}"
        );
        reconnected.join_match(PLAYER_ONE, "E2E Player 1", timeout)?;
        let reconnect_count_after = wait_until(
            &format!("player one reconnect visibility (cycle {cycle})"),
            timeout,
            poll,
            || {
                let Some(slot) = observer
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
                    "reconnected slot identity changed on cycle {cycle}"
                );
                Ok((slot.connected
                    && slot.has_reconnected
                    && slot.reconnect_count > reconnect_count_before)
                    .then_some(slot.reconnect_count))
            },
        )?;
        cycle_reports.push(ReconnectCycleReport {
            cycle,
            disconnect_to_reclaim_ms: started.elapsed().as_millis(),
            reconnect_count_after,
        });
        *player_one = reconnected;
    }

    let mut sorted: Vec<u128> = cycle_reports
        .iter()
        .map(|cycle| cycle.disconnect_to_reclaim_ms)
        .collect();
    sorted.sort_unstable();
    let p50_ms = percentile_sorted(&sorted, 50);
    let p95_ms = percentile_sorted(&sorted, 95);
    let max_ms = *sorted.last().context("reconnect soak produced no timings")?;

    Ok(ReconnectSoakReport {
        kind: "reconnect-soak",
        host: host.to_owned(),
        database: database.to_owned(),
        cycles_requested: cycles,
        cycles_completed: cycle_reports.len() as u32,
        p50_ms,
        p95_ms,
        max_ms,
        cycles: cycle_reports,
    })
}

fn percentile_sorted(sorted: &[u128], percentile: u8) -> u128 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = (u128::from(percentile) * (sorted.len() as u128 - 1)) / 100;
    sorted[rank as usize]
}

fn write_reconnect_report(path: Option<&Path>, report: &ReconnectSoakReport) -> Result<()> {
    let Some(path) = path else {
        return Ok(());
    };
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create reconnect report directory {}", parent.display()))?;
    }
    let payload = serde_json::to_vec_pretty(report).context("serialize reconnect soak report")?;
    fs::write(path, payload)
        .with_context(|| format!("write reconnect soak report {}", path.display()))?;
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn exercise_cluster_first_controls(
    client: &Client,
    command_id_floor: u64,
    timeout: Duration,
    poll: Duration,
) -> Result<u64> {
    let expand_candidate = select_focused_cluster_expand_candidate(
        &client.conn,
        PLAYER_TWO,
        CLUSTER_ACTION_COMMITMENT_BPS,
    )?;
    let owners_before_expand = owner_snapshot(&client.conn);
    let expand_id = unused_command_id(&client.conn, PLAYER_TWO, command_id_floor)?;
    client.issue_expand_clusters(
        expand_id,
        &[expand_candidate.source_seed],
        expand_candidate.focus_cell,
        CLUSTER_ACTION_COMMITMENT_BPS,
        timeout,
    )?;
    let expand_receipt = wait_for_receipt(
        client,
        PLAYER_TWO,
        expand_id,
        "issue_expand_clusters",
        timeout,
        poll,
    )?;
    ensure!(
        expand_receipt.order_id != 0,
        "focused cluster expansion did not persist an order"
    );
    wait_until(
        "focused cluster expansion persistence",
        timeout,
        poll,
        || {
            let Some(order) = client
                .conn
                .db
                .transfer_order()
                .order_id()
                .find(&expand_receipt.order_id)
            else {
                return Ok(None);
            };
            let visible_packet_total = client
                .conn
                .db
                .transit_packet()
                .iter()
                .filter(|packet| packet.order_id == order.order_id)
                .map(|packet| packet.infantry)
                .sum::<u64>();
            if visible_packet_total != order.in_transit_infantry {
                // Table callbacks from one transaction may reach the SDK
                // cache in different turns. Wait for the coherent snapshot;
                // a durable mismatch still times out and fails this phase.
                return Ok(None);
            }
            assert_cluster_action_order(
                &client.conn,
                &order,
                PLAYER_TWO,
                expand_id,
                OrderKind::ExpandClusters,
                expand_candidate.expected_requested,
                &expand_candidate.source_component,
                &expand_candidate.expected_source_commitments,
                true,
            )?;
            ensure!(
                order.status == OrderStatus::Active && order.in_transit_infantry > 0,
                "focused cluster expansion exhausted before its front-local accounting check"
            );
            Ok(Some(order))
        },
    )?;

    let focused_progress = wait_until(
        "clicked focus participation in the persisted cluster expansion",
        timeout,
        poll,
        || {
            let order = client
                .conn
                .db
                .transfer_order()
                .order_id()
                .find(&expand_receipt.order_id)
                .context("focused cluster expansion disappeared while observing its click focus")?;
            assert_order_conservation(&order)?;
            ensure!(
                owner_changes_for_player(&client.conn, PLAYER_TWO, &owners_before_expand,)
                    .iter()
                    .all(|cell_id| { owners_before_expand.get(cell_id).copied() == Some(0) }),
                "neutral-only focused expansion captured non-neutral territory"
            );
            let focus_owned = client
                .conn
                .db
                .cell_state()
                .cell_id()
                .find(&expand_candidate.focus_cell)
                .is_some_and(|cell| cell.owner_player_id == PLAYER_TWO);
            let focus_in_public_packet = client
                .conn
                .db
                .transit_packet()
                .iter()
                .filter(|packet| packet.order_id == order.order_id)
                .any(|packet| {
                    packet.current_cell == expand_candidate.focus_cell
                        || packet.destination_cell == expand_candidate.focus_cell
                });
            if !focus_owned && !focus_in_public_packet {
                ensure!(
                    order.status == OrderStatus::Active,
                    "focused expansion terminated without ever exposing activity at clicked cell {}",
                    expand_candidate.focus_cell
                );
                return Ok(None);
            }
            Ok(Some(order))
        },
    )?;

    let last_expand_command_id = if focused_progress.status == OrderStatus::Active {
        let expand_cancel_id = unused_command_id(
            &client.conn,
            PLAYER_TWO,
            expand_id
                .checked_add(1)
                .context("focused expansion command ID overflow")?,
        )?;
        client.cancel_orders(expand_cancel_id, &[expand_receipt.order_id], timeout)?;
        let cancel_receipt = wait_for_receipt(
            client,
            PLAYER_TWO,
            expand_cancel_id,
            "cancel_orders",
            timeout,
            poll,
        )?;
        ensure!(
            cancel_receipt.order_id == expand_receipt.order_id,
            "focused expansion cancellation referenced order {} instead of {}",
            cancel_receipt.order_id,
            expand_receipt.order_id
        );
        wait_until(
            "focused cluster expansion cancellation",
            timeout,
            poll,
            || {
                let Some(order) = client
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
                assert_order_conservation(&order)?;
                ensure!(
                    order.in_transit_infantry == 0
                        && !client
                            .conn
                            .db
                            .transit_packet()
                            .iter()
                            .any(|packet| packet.order_id == order.order_id),
                    "cancelled focused expansion retained live packet strength"
                );
                Ok(Some(()))
            },
        )?;
        expand_cancel_id
    } else {
        ensure!(
            focused_progress.status == OrderStatus::Completed,
            "focused expansion entered unexpected status {:?}",
            focused_progress.status
        );
        expand_id
    };

    let mut contact_setup_command_id = establish_cluster_contact_with_expansions(
        client,
        PLAYER_TWO,
        PLAYER_ONE,
        last_expand_command_id,
        timeout,
        poll,
    )?;

    let setup_attack_candidate = select_cluster_attack_candidate(
        &client.conn,
        PLAYER_TWO,
        PLAYER_ONE,
        CLUSTER_ACTION_COMMITMENT_BPS,
    )?;
    if component_target_needs_reshape(
        &client.conn,
        &setup_attack_candidate.source_component,
        setup_attack_candidate.source_seed,
    )? {
        let reshape_id = unused_command_id(
            &client.conn,
            PLAYER_TWO,
            contact_setup_command_id
                .checked_add(1)
                .context("attack-front Reshape command ID overflow")?,
        )?;
        client.issue_reshape(
            reshape_id,
            &[setup_attack_candidate.source_seed],
            &[setup_attack_candidate.source_seed],
            &[],
            timeout,
        )?;
        let receipt = wait_for_receipt(
            client,
            PLAYER_TWO,
            reshape_id,
            "issue_reshape",
            timeout,
            poll,
        )?;
        wait_until(
            "explicit attack-front Reshape completion",
            timeout,
            poll,
            || {
                let Some(order) = client
                    .conn
                    .db
                    .transfer_order()
                    .order_id()
                    .find(&receipt.order_id)
                else {
                    return Ok(None);
                };
                assert_order_conservation(&order)?;
                Ok(
                    (order.status == OrderStatus::Completed && order.in_transit_infantry == 0)
                        .then_some(()),
                )
            },
        )?;
        contact_setup_command_id = reshape_id;
    }
    let attack_candidate = select_cluster_attack_candidate(
        &client.conn,
        PLAYER_TWO,
        PLAYER_ONE,
        CLUSTER_ACTION_COMMITMENT_BPS,
    )?;
    let owners_before_attack = owner_snapshot(&client.conn);
    let attack_id = unused_command_id(
        &client.conn,
        PLAYER_TWO,
        contact_setup_command_id
            .checked_add(1)
            .context("contact-building expansion command ID overflow")?,
    )?;
    client.issue_attack_clusters(
        attack_id,
        &[attack_candidate.source_seed],
        &[attack_candidate.target_seed],
        CLUSTER_ACTION_COMMITMENT_BPS,
        timeout,
    )?;
    let attack_receipt = wait_for_receipt(
        client,
        PLAYER_TWO,
        attack_id,
        "issue_attack_clusters",
        timeout,
        poll,
    )?;
    ensure!(
        attack_receipt.order_id != 0,
        "cluster attack did not persist an order"
    );
    let observed_attack = wait_until(
        "masked enemy-cluster attack activity",
        timeout,
        poll,
        || {
            let Some(order) = client
                .conn
                .db
                .transfer_order()
                .order_id()
                .find(&attack_receipt.order_id)
            else {
                return Ok(None);
            };
            assert_cluster_action_order(
                &client.conn,
                &order,
                PLAYER_TWO,
                attack_id,
                OrderKind::AttackClusters,
                attack_candidate.expected_requested,
                &attack_candidate.source_component,
                &attack_candidate.expected_source_commitments,
                false,
            )?;
            let mask_activity = assert_attack_stays_in_target_mask(
                &client.conn,
                &order,
                PLAYER_TWO,
                &attack_candidate.source_component,
                &attack_candidate.target_component,
                &attack_candidate.outside_guard_cells,
                &owners_before_attack,
            )?;
            if !mask_activity {
                ensure!(
                    order.status == OrderStatus::Active,
                    "AttackClusters terminated without reaching any of its {} shared-front targets",
                    attack_candidate.shared_front_targets.len()
                );
                return Ok(None);
            }
            Ok(Some(order))
        },
    )?;

    let final_command_id = if observed_attack.status == OrderStatus::Active {
        let attack_cancel_id = unused_command_id(
            &client.conn,
            PLAYER_TWO,
            attack_id
                .checked_add(1)
                .context("cluster attack command ID overflow")?,
        )?;
        client.cancel_orders(attack_cancel_id, &[attack_receipt.order_id], timeout)?;
        wait_for_receipt(
            client,
            PLAYER_TWO,
            attack_cancel_id,
            "cancel_orders",
            timeout,
            poll,
        )?;
        wait_until("enemy-cluster attack cancellation", timeout, poll, || {
            let Some(order) = client
                .conn
                .db
                .transfer_order()
                .order_id()
                .find(&attack_receipt.order_id)
            else {
                return Ok(None);
            };
            if order.status != OrderStatus::Cancelled {
                return Ok(None);
            }
            assert_order_conservation(&order)?;
            ensure!(
                order.in_transit_infantry == 0
                    && !client
                        .conn
                        .db
                        .transit_packet()
                        .iter()
                        .any(|packet| packet.order_id == order.order_id),
                "cancelled enemy-cluster attack retained live packet strength"
            );
            assert_attack_stays_in_target_mask(
                &client.conn,
                &order,
                PLAYER_TWO,
                &attack_candidate.source_component,
                &attack_candidate.target_component,
                &attack_candidate.outside_guard_cells,
                &owners_before_attack,
            )?;
            Ok(Some(()))
        })?;
        attack_cancel_id
    } else {
        attack_id
    };

    let ownership_at_attack_stop = owner_snapshot(&client.conn);
    let stopped_step = client
        .conn
        .db
        .transfer_order()
        .order_id()
        .find(&attack_receipt.order_id)
        .context("cluster attack disappeared after it stopped")?
        .updated_step;
    wait_until("post cluster-attack mask stability", timeout, poll, || {
        let Some(state) = client
            .conn
            .db
            .match_state()
            .singleton_id()
            .find(&SINGLETON_ID)
        else {
            return Ok(None);
        };
        if state.logical_step < stopped_step.saturating_add(POST_CANCEL_STEPS) {
            return Ok(None);
        }
        ensure!(
            owner_changes_for_player(&client.conn, PLAYER_TWO, &owners_before_attack)
                .is_subset(&attack_candidate.target_component),
            "player two acquired territory outside the immutable enemy target component after AttackClusters stopped"
        );
        for &guard_cell in &attack_candidate.outside_guard_cells {
            ensure!(
                ownership_at_attack_stop.get(&guard_cell)
                    == owner_snapshot(&client.conn).get(&guard_cell),
                "outside-mask guard cell {guard_cell} changed owner after the cluster attack stopped"
            );
        }
        Ok(Some(()))
    })?;
    Ok(final_command_id)
}

#[allow(clippy::too_many_lines)]
fn establish_cluster_contact_with_expansions(
    client: &Client,
    player_id: u16,
    enemy_player_id: u16,
    previous_command_id: u64,
    timeout: Duration,
    poll: Duration,
) -> Result<u64> {
    if players_share_traversable_front(&client.conn, player_id, enemy_player_id)? {
        return Ok(previous_command_id);
    }

    let initial_contact_distance = select_contact_cluster_expand_candidate(
        &client.conn,
        player_id,
        enemy_player_id,
        CONTACT_EXPAND_COMMITMENT_BPS,
    )?
    .focus_distance;
    let max_contact_expansion_attempts =
        usize::try_from(initial_contact_distance.saturating_add(8))
            .context("neutral contact attempt bound does not fit this platform")?;
    ensure!(
        max_contact_expansion_attempts > 0,
        "non-contacting clusters produced a zero neutral contact distance"
    );

    let mut last_command_id = previous_command_id;
    for attempt in 1..=max_contact_expansion_attempts {
        if players_share_traversable_front(&client.conn, player_id, enemy_player_id)? {
            return Ok(last_command_id);
        }

        let setup_candidate = select_contact_cluster_expand_candidate(
            &client.conn,
            player_id,
            enemy_player_id,
            CONTACT_EXPAND_COMMITMENT_BPS,
        )?;
        if contact_approach_needs_reshape(&client.conn, &setup_candidate)? {
            let reshape_id = unused_command_id(
                &client.conn,
                player_id,
                last_command_id
                    .checked_add(1)
                    .context("contact-setup Reshape command ID overflow")?,
            )?;
            client.issue_reshape(
                reshape_id,
                &[setup_candidate.source_seed],
                &[setup_candidate.approach_source],
                &[],
                timeout,
            )?;
            let reshape_receipt = wait_for_receipt(
                client,
                player_id,
                reshape_id,
                "issue_reshape",
                timeout,
                poll,
            )?;
            wait_until(
                "explicit contact-front Reshape completion",
                timeout,
                poll,
                || {
                    let Some(order) = client
                        .conn
                        .db
                        .transfer_order()
                        .order_id()
                        .find(&reshape_receipt.order_id)
                    else {
                        return Ok(None);
                    };
                    assert_order_conservation(&order)?;
                    Ok(
                        (order.status == OrderStatus::Completed && order.in_transit_infantry == 0)
                            .then_some(()),
                    )
                },
            )?;
            last_command_id = reshape_id;
        }

        // Re-read after the explicit distribution command. Expansion itself
        // may commit only the infantry now present on its eligible perimeter.
        let candidate = select_contact_cluster_expand_candidate(
            &client.conn,
            player_id,
            enemy_player_id,
            CONTACT_EXPAND_COMMITMENT_BPS,
        )?;
        let owners_before = owner_snapshot(&client.conn);
        let expand_id = unused_command_id(
            &client.conn,
            player_id,
            last_command_id
                .checked_add(1)
                .context("contact-building command ID overflow")?,
        )?;
        client.issue_expand_clusters(
            expand_id,
            &[candidate.source_seed],
            candidate.focus_cell,
            CONTACT_EXPAND_COMMITMENT_BPS,
            timeout,
        )?;
        let receipt = wait_for_receipt(
            client,
            player_id,
            expand_id,
            "issue_expand_clusters",
            timeout,
            poll,
        )?;
        ensure!(
            receipt.order_id != 0,
            "contact-building cluster expansion did not persist an order"
        );
        last_command_id = expand_id;

        let (observed, has_contact) = wait_until(
            &format!(
                "contact-building cluster expansion {attempt}/{max_contact_expansion_attempts}"
            ),
            timeout,
            poll,
            || {
                let Some(order) = client
                    .conn
                    .db
                    .transfer_order()
                    .order_id()
                    .find(&receipt.order_id)
                else {
                    return Ok(None);
                };
                assert_cluster_action_order(
                    &client.conn,
                    &order,
                    player_id,
                    expand_id,
                    OrderKind::ExpandClusters,
                    candidate.expected_requested,
                    &candidate.source_component,
                    &candidate.expected_source_commitments,
                    false,
                )?;
                assert_expansion_claimed_only_neutral(&client.conn, player_id, &owners_before)?;
                let has_contact =
                    players_share_traversable_front(&client.conn, player_id, enemy_player_id)?;
                if has_contact {
                    return Ok(Some((order, true)));
                }
                if order.status == OrderStatus::Active {
                    return Ok(None);
                }
                ensure!(
                    order.status == OrderStatus::Completed
                        && order.in_transit_infantry == 0
                        && !client
                            .conn
                            .db
                            .transit_packet()
                            .iter()
                            .any(|packet| packet.order_id == order.order_id)
                        && client
                            .conn
                            .db
                            .transfer_source()
                            .iter()
                            .filter(|source| source.order_id == order.order_id)
                            .all(|source| source.queued_infantry == 0),
                    "contact-building expansion ended without contact in invalid terminal state {:?}",
                    order.status
                );
                Ok(Some((order, false)))
            },
        )?;
        assert_order_conservation(&observed)?;
        if !has_contact {
            let claimed = owner_changes_for_player(&client.conn, player_id, &owners_before);
            ensure!(
                !claimed.is_empty(),
                "contact-building expansion {attempt} completed without contact or territorial progress"
            );
            continue;
        }

        if observed.status == OrderStatus::Active {
            let cancel_id = unused_command_id(
                &client.conn,
                player_id,
                last_command_id
                    .checked_add(1)
                    .context("contact-building expansion cancellation ID overflow")?,
            )?;
            client.cancel_orders(cancel_id, &[receipt.order_id], timeout)?;
            let cancel_receipt =
                wait_for_receipt(client, player_id, cancel_id, "cancel_orders", timeout, poll)?;
            ensure!(
                cancel_receipt.order_id == receipt.order_id,
                "contact-building cancellation referenced order {} instead of {}",
                cancel_receipt.order_id,
                receipt.order_id
            );
            wait_until(
                "contact-building cluster expansion cancellation",
                timeout,
                poll,
                || {
                    let Some(order) = client
                        .conn
                        .db
                        .transfer_order()
                        .order_id()
                        .find(&receipt.order_id)
                    else {
                        return Ok(None);
                    };
                    if order.status != OrderStatus::Cancelled {
                        return Ok(None);
                    }
                    assert_cluster_action_order(
                        &client.conn,
                        &order,
                        player_id,
                        expand_id,
                        OrderKind::ExpandClusters,
                        candidate.expected_requested,
                        &candidate.source_component,
                        &candidate.expected_source_commitments,
                        false,
                    )?;
                    ensure!(
                        order.in_transit_infantry == 0
                            && !client
                                .conn
                                .db
                                .transit_packet()
                                .iter()
                                .any(|packet| packet.order_id == order.order_id)
                            && client
                                .conn
                                .db
                                .transfer_source()
                                .iter()
                                .filter(|source| source.order_id == order.order_id)
                                .all(|source| source.queued_infantry == 0),
                        "cancelled contact-building expansion retained live packet strength"
                    );
                    assert_expansion_claimed_only_neutral(&client.conn, player_id, &owners_before)?;
                    Ok(Some(()))
                },
            )?;
            last_command_id = cancel_id;
        } else {
            ensure!(
                observed.status == OrderStatus::Completed,
                "contact-building expansion reached contact in unexpected state {:?}",
                observed.status
            );
        }
        ensure!(
            players_share_traversable_front(&client.conn, player_id, enemy_player_id)?,
            "enemy contact disappeared when the neutral-only expansion stopped"
        );
        return Ok(last_command_id);
    }

    bail!(
        "failed to establish enemy contact after {max_contact_expansion_attempts} explicitly supplied front-local cluster expansions across an initial {initial_contact_distance}-cell neutral contact path"
    )
}

fn contact_approach_needs_reshape(
    conn: &DbConnection,
    candidate: &FocusedClusterExpandCandidate,
) -> Result<bool> {
    component_target_needs_reshape(conn, &candidate.source_component, candidate.approach_source)
}

fn component_target_needs_reshape(
    conn: &DbConnection,
    source_component: &BTreeSet<u32>,
    target_cell: u32,
) -> Result<bool> {
    let target = conn
        .db
        .cell_state()
        .cell_id()
        .find(&target_cell)
        .context("explicit distribution target disappeared")?;
    if target.infantry >= target.military_capacity {
        return Ok(false);
    }
    Ok(source_component.iter().any(|cell_id| {
        *cell_id != target_cell
            && conn
                .db
                .cell_state()
                .cell_id()
                .find(cell_id)
                .is_some_and(|cell| cell.infantry > 0)
    }))
}

fn assert_expansion_claimed_only_neutral(
    conn: &DbConnection,
    player_id: u16,
    owners_before: &HashMap<u32, u16>,
) -> Result<()> {
    for cell_id in owner_changes_for_player(conn, player_id, owners_before) {
        ensure!(
            owners_before.get(&cell_id).copied() == Some(0),
            "neutral-only cluster expansion captured non-neutral cell {cell_id}"
        );
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn exercise_internal_controls(
    client: &Client,
    command_id_floor: u64,
    timeout: Duration,
    poll: Duration,
) -> Result<u64> {
    wait_for_order_quiescence(client, PLAYER_ONE, timeout, poll)?;
    let candidate = select_cluster_reshape_candidate(&client.conn, PLAYER_ONE)?;
    ensure!(
        candidate.source_component.len() > 1
            && candidate.source_component.contains(&candidate.source_seed)
            && candidate.source_seed == candidate.target_cell
            && candidate.expected_overflow > 0
            && candidate.plan.expected_strength_by_cell[&candidate.target_cell]
                == candidate.target_capacity,
        "whole-cluster Reshape fixture does not describe a smaller singleton shape with deterministic overflow"
    );

    // The source payload is deliberately only the target cell. Treating it as
    // a literal sub-cluster would be a no-op; the non-zero order therefore
    // proves authority closed the sparse seed over its complete current cluster.
    let reshape_id = unused_command_id(&client.conn, PLAYER_ONE, command_id_floor)?;
    client.issue_reshape(
        reshape_id,
        &[candidate.source_seed],
        &[candidate.target_cell],
        &[],
        timeout,
    )?;
    let reshape_receipt = wait_for_receipt(
        client,
        PLAYER_ONE,
        reshape_id,
        "issue_reshape",
        timeout,
        poll,
    )?;
    ensure!(
        reshape_receipt.order_id != 0,
        "whole-cluster best-effort Reshape was accepted as a no-op"
    );

    let reshape = wait_until("whole-cluster Reshape persistence", timeout, poll, || {
        let Some(order) = client
            .conn
            .db
            .transfer_order()
            .order_id()
            .find(&reshape_receipt.order_id)
        else {
            return Ok(None);
        };
        assert_internal_order(&client.conn, &order, reshape_id, &candidate.plan)?;
        let source_cells = client
            .conn
            .db
            .transfer_source()
            .iter()
            .filter(|source| source.order_id == order.order_id)
            .map(|source| source.cell_id)
            .collect::<BTreeSet<_>>();
        let destination_cells = client
            .conn
            .db
            .transfer_destination()
            .iter()
            .filter(|destination| destination.order_id == order.order_id)
            .map(|destination| destination.cell_id)
            .collect::<BTreeSet<_>>();
        ensure!(
            source_cells.is_subset(&candidate.source_component)
                && destination_cells.is_subset(&candidate.source_component)
                && source_cells
                    .iter()
                    .any(|cell_id| *cell_id != candidate.source_seed)
                && destination_cells.contains(&candidate.target_cell),
            "sparse-seed Reshape did not resolve and route within exactly its complete current cluster"
        );
        Ok(Some(order))
    })?;

    let invalid_reshape_id = unused_command_id(
        &client.conn,
        PLAYER_ONE,
        reshape_id
            .checked_add(1)
            .context("whole-cluster Reshape command ID overflow")?,
    )?;
    client.issue_reshape(
        invalid_reshape_id,
        &[candidate.source_seed],
        &[candidate.invalid_target],
        &[],
        timeout,
    )?;
    let rejected = wait_for_rejected_receipt(
        client,
        PLAYER_ONE,
        invalid_reshape_id,
        "issue_reshape",
        timeout,
        poll,
    )?;
    ensure!(
        rejected.order_id == 0 && rejected.message.contains("not owned passable ground"),
        "invalid whole-cluster Reshape produced an unexpected receipt: order {}, message '{}'",
        rejected.order_id,
        rejected.message
    );
    ensure!(
        !client.conn.db.transfer_order().iter().any(|order| {
            order.player_id == PLAYER_ONE && order.client_command_id == invalid_reshape_id
        }),
        "invalid rejected Reshape persisted a replacement order"
    );
    let preserved = client
        .conn
        .db
        .transfer_order()
        .order_id()
        .find(&reshape_receipt.order_id)
        .context("invalid Reshape removed the accepted whole-cluster order")?;
    ensure!(
        preserved.status != OrderStatus::Cancelled
            && preserved.requested_infantry == reshape.requested_infantry
            && preserved.committed_infantry == reshape.committed_infantry,
        "invalid Reshape mutated or cancelled the accepted whole-cluster order"
    );
    assert_order_conservation(&preserved)?;

    wait_for_internal_order_completion(
        client,
        reshape_receipt.order_id,
        reshape_id,
        &candidate.plan,
        timeout,
        poll,
    )?;
    wait_for_order_quiescence(client, PLAYER_ONE, timeout, poll)?;
    Ok(invalid_reshape_id)
}

fn wait_for_internal_order_completion(
    client: &Client,
    order_id: u64,
    command_id: u64,
    plan: &InternalPlan,
    timeout: Duration,
    poll: Duration,
) -> Result<TransferOrder> {
    wait_until(
        &format!("{} physical completion", plan.command_name),
        timeout,
        poll,
        || {
            let Some(order) = client.conn.db.transfer_order().order_id().find(&order_id) else {
                return Ok(None);
            };
            assert_internal_order(&client.conn, &order, command_id, plan)?;
            if order.status == OrderStatus::Active {
                return Ok(None);
            }
            ensure!(
                order.status == OrderStatus::Completed,
                "{} order {} ended with {:?} instead of completing",
                plan.command_name,
                order.order_id,
                order.status
            );
            ensure!(
                order.in_transit_infantry == 0
                    && order.delivered_infantry == order.committed_infantry
                    && order.casualty_infantry == 0,
                "{} completed with invalid terminal accounting",
                plan.command_name
            );
            ensure!(
                !client
                    .conn
                    .db
                    .transit_packet()
                    .iter()
                    .any(|packet| packet.order_id == order.order_id)
                    && client
                        .conn
                        .db
                        .transfer_source()
                        .iter()
                        .filter(|source| source.order_id == order.order_id)
                        .all(|source| source.queued_infantry == 0)
                    && client
                        .conn
                        .db
                        .transfer_destination()
                        .iter()
                        .filter(|destination| destination.order_id == order.order_id)
                        .all(|destination| {
                            destination.received_infantry == destination.target_infantry
                        }),
                "{} completed without draining every source, packet, and destination ledger",
                plan.command_name
            );
            Ok(Some(order))
        },
    )
}

fn wait_for_order_quiescence(
    client: &Client,
    player_id: u16,
    timeout: Duration,
    poll: Duration,
) -> Result<()> {
    let population_interval = u64::from(
        client
            .conn
            .db
            .match_config()
            .singleton_id()
            .find(&SINGLETON_ID)
            .context("match config disappeared while waiting for order quiescence")?
            .population_step_interval
            .max(1),
    );
    let mut quiet_since = None;
    wait_until("persistent order quiescence", timeout, poll, || {
        let logical_step = client
            .conn
            .db
            .match_state()
            .singleton_id()
            .find(&SINGLETON_ID)
            .context("match state disappeared while waiting for order quiescence")?
            .logical_step;
        let has_active = client
            .conn
            .db
            .transfer_order()
            .iter()
            .any(|order| order.player_id == player_id && order.status == OrderStatus::Active);
        if has_active {
            quiet_since = None;
            return Ok(None);
        }
        let first_quiet_step = *quiet_since.get_or_insert(logical_step);
        Ok(
            (logical_step >= first_quiet_step.saturating_add(population_interval + 1))
                .then_some(()),
        )
    })
}

fn assert_internal_order(
    conn: &DbConnection,
    order: &TransferOrder,
    command_id: u64,
    plan: &InternalPlan,
) -> Result<()> {
    ensure!(
        order.player_id == PLAYER_ONE
            && order.client_command_id == command_id
            && order.kind == plan.kind,
        "{} receipt referenced the wrong order identity or kind",
        plan.command_name
    );
    ensure!(
        (order.orientation_q, order.orientation_r) == (plan.orientation.q, plan.orientation.r),
        "{} persisted orientation ({}, {}) instead of ({}, {})",
        plan.command_name,
        order.orientation_q,
        order.orientation_r,
        plan.orientation.q,
        plan.orientation.r
    );
    ensure!(
        order.requested_infantry == plan.expected_requested
            && order.committed_infantry == plan.expected_requested,
        "{} requested/committed {}/{} infantry instead of predicted {}",
        plan.command_name,
        order.requested_infantry,
        order.committed_infantry,
        plan.expected_requested
    );
    ensure!(
        order.casualty_infantry == 0,
        "{} internal movement recorded casualties",
        plan.command_name
    );
    assert_order_conservation(order)?;

    let packet_total = conn
        .db
        .transit_packet()
        .iter()
        .filter(|packet| packet.order_id == order.order_id)
        .try_fold(0_u64, |total, packet| {
            ensure!(
                packet.owner_player_id == PLAYER_ONE,
                "{} packet belongs to another player",
                plan.command_name
            );
            total
                .checked_add(packet.infantry)
                .context("internal packet total overflow")
        })?;
    ensure!(
        packet_total == order.in_transit_infantry,
        "{} has {packet_total} packet infantry but reports {} in transit",
        plan.command_name,
        order.in_transit_infantry
    );

    let sources = conn
        .db
        .transfer_source()
        .iter()
        .filter(|source| source.order_id == order.order_id)
        .collect::<Vec<_>>();
    let destinations = conn
        .db
        .transfer_destination()
        .iter()
        .filter(|destination| destination.order_id == order.order_id)
        .collect::<Vec<_>>();
    ensure!(
        !sources.is_empty() && !destinations.is_empty(),
        "{} did not persist both source and destination accounting",
        plan.command_name
    );
    let source_committed = sources.iter().try_fold(0_u64, |total, source| {
        ensure!(
            source.queued_infantry <= source.committed_infantry,
            "{} source {} is over-queued",
            plan.command_name,
            source.cell_id
        );
        total
            .checked_add(source.committed_infantry)
            .context("internal source total overflow")
    })?;
    let destination_target = destinations.iter().try_fold(0_u64, |total, destination| {
        ensure!(
            destination.received_infantry <= destination.target_infantry,
            "{} destination {} received beyond its target",
            plan.command_name,
            destination.cell_id
        );
        total
            .checked_add(destination.target_infantry)
            .context("internal destination total overflow")
    })?;
    ensure!(
        source_committed == order.committed_infantry
            && destination_target == order.committed_infantry,
        "{} table totals do not match its committed infantry",
        plan.command_name
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
    player_id: u16,
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
    player_id: u16,
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

fn unused_command_id(conn: &DbConnection, player_id: u16, start: u64) -> Result<u64> {
    let mut candidate = start;
    loop {
        let key = receipt_key(player_id, candidate);
        if conn.db.command_receipt().receipt_key().find(&key).is_none() {
            return Ok(candidate);
        }
        candidate = candidate
            .checked_add(1)
            .context("exhausted client command ID range")?;
    }
}

fn unused_order_id(conn: &DbConnection) -> Result<u64> {
    let mut candidate = u64::MAX;
    loop {
        if conn
            .db
            .transfer_order()
            .order_id()
            .find(&candidate)
            .is_none()
        {
            return Ok(candidate);
        }
        candidate = candidate
            .checked_sub(1)
            .context("exhausted order ID range while selecting an unknown ID")?;
    }
}

fn wait_for_receipt(
    client: &Client,
    player_id: u16,
    command_id: u64,
    command_name: &str,
    timeout: Duration,
    poll: Duration,
) -> Result<CommandReceipt> {
    let key = receipt_key(player_id, command_id);
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

fn wait_for_rejected_receipt(
    client: &Client,
    player_id: u16,
    command_id: u64,
    command_name: &str,
    timeout: Duration,
    poll: Duration,
) -> Result<CommandReceipt> {
    let key = receipt_key(player_id, command_id);
    let receipt = wait_until(
        &format!("rejected {command_name} command receipt"),
        timeout,
        poll,
        || Ok(client.conn.db.command_receipt().receipt_key().find(&key)),
    )?;
    ensure!(
        receipt.player_id == player_id
            && receipt.client_command_id == command_id
            && receipt.command_name == command_name,
        "rejected receipt {key} did not identify the expected command"
    );
    ensure!(
        receipt.status == ReceiptStatus::Rejected,
        "{command_name} unexpectedly succeeded while testing atomic rejection"
    );
    Ok(receipt)
}

#[allow(clippy::too_many_lines)]
fn select_cluster_reshape_candidate(
    conn: &DbConnection,
    player_id: u16,
) -> Result<ClusterReshapeCandidate> {
    let terrain_by_id = conn
        .db
        .cell_terrain()
        .iter()
        .map(|terrain| (terrain.cell_id, terrain))
        .collect::<HashMap<_, _>>();
    let state_by_id = conn
        .db
        .cell_state()
        .iter()
        .map(|state| (state.cell_id, state))
        .collect::<HashMap<_, _>>();
    let invalid_target = terrain_by_id
        .values()
        .filter(|terrain| {
            state_by_id
                .get(&terrain.cell_id)
                .is_some_and(|state| state.owner_player_id != player_id || !terrain.passable)
        })
        .map(|terrain| terrain.cell_id)
        .min()
        .context("whole-cluster Reshape coverage needs a foreign, neutral, or impassable target")?;
    ensure!(
        allocated_infantry_by_cell(conn, player_id)?.is_empty(),
        "whole-cluster Reshape candidate must be selected after active orders are quiescent"
    );

    let mut candidates = Vec::new();
    for source_component in owned_traversable_components(conn, player_id)? {
        if source_component.len() <= 1 {
            continue;
        }
        let mut map = HexMap::new();
        let mut id_by_coordinate = BTreeMap::new();
        for &cell_id in &source_component {
            let terrain = &terrain_by_id[&cell_id];
            let state = &state_by_id[&cell_id];
            let coordinate = Axial::new(terrain.q, terrain.r);
            id_by_coordinate.insert(coordinate, cell_id);
            map.insert(CoreCell {
                coordinate,
                terrain: core_terrain(terrain.terrain),
                elevation: terrain.elevation,
                capturable: terrain.capturable,
                habitable: terrain.habitable,
                owner: Some(u32::from(player_id)),
                civilian_population: state.civilians,
                civilian_capacity: state.civilian_capacity,
                forces: ForceComposition::infantry(state.infantry),
                military_capacity: state.military_capacity,
            });
        }
        let source_coordinates = id_by_coordinate.keys().copied().collect::<BTreeSet<_>>();
        let source_total = source_component.iter().try_fold(0_u64, |total, cell_id| {
            total
                .checked_add(state_by_id[cell_id].infantry)
                .context("whole-cluster Reshape source-strength overflow")
        })?;
        for &target_cell in &source_component {
            let target_terrain = &terrain_by_id[&target_cell];
            let target_coordinate = Axial::new(target_terrain.q, target_terrain.r);
            let target_coordinates = BTreeSet::from([target_coordinate]);
            let Some((plan, peak_destination_demand)) = projected_reshape_plan(
                &map,
                &id_by_coordinate,
                player_id,
                &source_coordinates,
                &target_coordinates,
            ) else {
                continue;
            };
            let target_capacity = state_by_id[&target_cell].military_capacity;
            let expected_target = plan.expected_strength_by_cell[&target_cell];
            let expected_overflow = plan
                .expected_strength_by_cell
                .iter()
                .filter(|(cell_id, _)| **cell_id != target_cell)
                .try_fold(0_u64, |total, (_, strength)| total.checked_add(*strength));
            let Some(expected_overflow) = expected_overflow else {
                continue;
            };
            if expected_target != target_capacity
                || expected_overflow == 0
                || expected_target.checked_add(expected_overflow) != Some(source_total)
            {
                continue;
            }
            candidates.push((
                peak_destination_demand,
                ClusterReshapeCandidate {
                    source_component: source_component.clone(),
                    source_seed: target_cell,
                    target_cell,
                    target_capacity,
                    expected_overflow,
                    plan,
                    invalid_target,
                },
            ));
        }
    }
    candidates.sort_unstable_by_key(|(peak, candidate)| {
        (
            std::cmp::Reverse(*peak),
            std::cmp::Reverse(candidate.source_component.len()),
            candidate.source_seed,
        )
    });
    candidates
        .into_iter()
        .next()
        .map(|(_, candidate)| candidate)
        .context(
            "no complete owned cluster can produce a non-zero singleton best-effort Reshape with capacity overflow; run against a fresh default fixture",
        )
}

fn projected_reshape_plan(
    map: &HexMap,
    id_by_coordinate: &BTreeMap<Axial, u32>,
    player_id: u16,
    source_coordinates: &BTreeSet<Axial>,
    target_coordinates: &BTreeSet<Axial>,
) -> Option<(InternalPlan, u64)> {
    if source_coordinates.is_empty() || target_coordinates.is_empty() {
        return None;
    }
    let relevant = source_coordinates
        .union(target_coordinates)
        .copied()
        .collect::<BTreeSet<_>>();
    let mut projected = HexMap::new();
    let mut fixed_by_coordinate = BTreeMap::new();
    let mut total_affected = 0_u64;
    for &coordinate in &relevant {
        let original = map.get(coordinate)?;
        let mut cell = original.clone();
        let (affected, fixed, residual_capacity) = if source_coordinates.contains(&coordinate) {
            (original.force(), 0, original.military_capacity)
        } else {
            (
                0,
                original.force(),
                original.military_capacity.saturating_sub(original.force()),
            )
        };
        total_affected = total_affected.checked_add(affected)?;
        cell.forces = ForceComposition::infantry(affected);
        cell.military_capacity = residual_capacity;
        fixed_by_coordinate.insert(coordinate, fixed);
        projected.insert(cell);
    }
    let weights = relevant
        .iter()
        .map(|coordinate| {
            (
                *coordinate,
                if target_coordinates.contains(coordinate) {
                    10_000
                } else {
                    0
                },
            )
        })
        .collect::<BTreeMap<_, _>>();
    let lower_bounds = relevant
        .iter()
        .map(|coordinate| (*coordinate, 0_u64))
        .collect::<BTreeMap<_, _>>();
    let distribution = redistribution_targets_with_fallback_constraints(
        &projected,
        u32::from(player_id),
        weights,
        lower_bounds,
        total_affected,
    )
    .ok()?;

    let mut expected_requested = 0_u64;
    let mut peak_destination_demand = 0_u64;
    let mut expected_strength_by_cell = HashMap::new();
    for (&coordinate, &affected_target) in &distribution.targets {
        let expected = fixed_by_coordinate[&coordinate].checked_add(affected_target)?;
        let current = map.get(coordinate)?.force();
        let demand = expected.saturating_sub(current);
        expected_requested = expected_requested.checked_add(demand)?;
        peak_destination_demand = peak_destination_demand.max(demand);
        expected_strength_by_cell.insert(id_by_coordinate[&coordinate], expected);
    }
    if expected_requested == 0 {
        return None;
    }
    Some((
        InternalPlan {
            command_name: "issue_reshape",
            kind: OrderKind::Reshape,
            orientation: Axial::ZERO,
            expected_requested,
            expected_strength_by_cell,
        },
        peak_destination_demand,
    ))
}

const fn core_terrain(terrain: TerrainClass) -> TerrainKind {
    match terrain {
        TerrainClass::Plains => TerrainKind::Plains,
        TerrainClass::Hills => TerrainKind::Hills,
        TerrainClass::Mountain => TerrainKind::Mountain,
        TerrainClass::Water => TerrainKind::Water,
    }
}

#[allow(clippy::too_many_lines)]
fn select_push_front_candidate(conn: &DbConnection, player_id: u16) -> Result<PushFrontCandidate> {
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

    // Only
    // other explicit actions stay unavailable when predicting the commitment.
    let allocated_by_source = allocated_infantry_by_cell(conn, player_id)?;

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
fn select_expand_all_candidate(conn: &DbConnection, player_id: u16) -> Result<ExpandAllCandidate> {
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
    // Existing explicit action packets remain fixed and reduce availability.
    let allocated_by_cell = allocated_infantry_by_cell(conn, player_id)?;

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

        let expected_source_commitments = expected_component_shares(
            &boundary,
            &cell_by_id,
            &allocated_by_cell,
            EXPAND_COMMITMENT_BPS,
        )?;
        let expected_requested =
            expected_source_commitments
                .values()
                .try_fold(0_u64, |total, &committed| {
                    total
                        .checked_add(committed)
                        .context("all-front candidate commitment overflow")
                })?;
        if expected_requested == 0 {
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
            &boundary,
            &outside_depths,
            &terrain_by_id,
            &cell_by_coordinate,
        );
        if !children.values().any(|targets| targets.len() >= 2) {
            continue;
        }

        let turning_second_ring =
            turning_second_ring_cells(&boundary, &outside_depths, &children, &terrain_by_id);
        let reached = forecast_wave_reach(
            &expected_source_commitments,
            &boundary,
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
            perimeter_sources: boundary,
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

fn select_focused_cluster_expand_candidate(
    conn: &DbConnection,
    player_id: u16,
    commitment_bps: u32,
) -> Result<FocusedClusterExpandCandidate> {
    let terrain_by_id = conn
        .db
        .cell_terrain()
        .iter()
        .map(|terrain| (terrain.cell_id, terrain))
        .collect::<HashMap<_, _>>();
    let cell_by_id = conn
        .db
        .cell_state()
        .iter()
        .map(|cell| (cell.cell_id, cell))
        .collect::<HashMap<_, _>>();
    let cell_by_coordinate = terrain_by_id
        .values()
        .map(|terrain| (Axial::new(terrain.q, terrain.r), terrain.cell_id))
        .collect::<HashMap<_, _>>();
    let max_elevation_step = current_max_elevation_step(conn)?;
    let allocated_by_cell = allocated_infantry_by_cell(conn, player_id)?;
    let mut candidates = Vec::new();
    for source_component in owned_traversable_components(conn, player_id)? {
        let mut first_ring = BTreeSet::new();
        let mut approach_by_target = BTreeMap::new();
        let mut perimeter_sources = BTreeSet::new();
        for &source_cell in &source_component {
            let source_terrain = &terrain_by_id[&source_cell];
            for coordinate in Axial::new(source_terrain.q, source_terrain.r).neighbors() {
                let Some(&target_cell) = cell_by_coordinate.get(&coordinate) else {
                    continue;
                };
                let target_state = &cell_by_id[&target_cell];
                let target_terrain = &terrain_by_id[&target_cell];
                if target_state.owner_player_id == 0
                    && target_terrain.passable
                    && target_terrain.capturable
                    && terrain_edge_is_traversable_with_limit(
                        &terrain_by_id,
                        source_cell,
                        target_cell,
                        max_elevation_step,
                    )
                {
                    perimeter_sources.insert(source_cell);
                    first_ring.insert(target_cell);
                    approach_by_target.entry(target_cell).or_insert(source_cell);
                }
            }
        }
        let Some(&focus_cell) = first_ring.first() else {
            continue;
        };
        let approach_source = approach_by_target[&focus_cell];
        let expected_source_commitments = expected_component_shares(
            &perimeter_sources,
            &cell_by_id,
            &allocated_by_cell,
            commitment_bps,
        )?;
        let expected_requested =
            expected_source_commitments
                .values()
                .try_fold(0_u64, |total, &commitment| {
                    total
                        .checked_add(commitment)
                        .context("focused cluster expansion commitment overflow")
                })?;
        if expected_requested == 0 {
            continue;
        }
        candidates.push(FocusedClusterExpandCandidate {
            source_seed: *source_component
                .first()
                .context("owned component unexpectedly empty")?,
            source_component,
            approach_source,
            focus_cell,
            focus_distance: 1,
            expected_source_commitments,
            expected_requested,
        });
    }
    candidates.sort_unstable_by_key(|candidate| {
        (
            std::cmp::Reverse(candidate.expected_requested),
            std::cmp::Reverse(candidate.source_component.len()),
            candidate.source_seed,
            candidate.focus_cell,
        )
    });
    candidates.into_iter().next().context(
        "no owned traversable cluster has both unallocated infantry and an unclaimed passable perimeter for focused expansion; run against a fresh default fixture",
    )
}

fn select_contact_cluster_expand_candidate(
    conn: &DbConnection,
    player_id: u16,
    enemy_player_id: u16,
    commitment_bps: u32,
) -> Result<FocusedClusterExpandCandidate> {
    let terrain_by_id = conn
        .db
        .cell_terrain()
        .iter()
        .map(|terrain| (terrain.cell_id, terrain))
        .collect::<HashMap<_, _>>();
    let cell_by_id = conn
        .db
        .cell_state()
        .iter()
        .map(|cell| (cell.cell_id, cell))
        .collect::<HashMap<_, _>>();
    let cell_by_coordinate = terrain_by_id
        .values()
        .map(|terrain| (Axial::new(terrain.q, terrain.r), terrain.cell_id))
        .collect::<HashMap<_, _>>();
    let max_elevation_step = current_max_elevation_step(conn)?;
    let allocated_by_cell = allocated_infantry_by_cell(conn, player_id)?;
    let mut candidates = Vec::new();
    for source_component in owned_traversable_components(conn, player_id)? {
        let Some((distance, focus_cell, approach_source)) = nearest_neutral_focus_toward_enemy(
            &source_component,
            enemy_player_id,
            &terrain_by_id,
            &cell_by_id,
            &cell_by_coordinate,
            max_elevation_step,
        ) else {
            continue;
        };
        let perimeter_sources = neutral_perimeter_sources(
            &source_component,
            &terrain_by_id,
            &cell_by_id,
            &cell_by_coordinate,
            max_elevation_step,
        );
        let expected_source_commitments = expected_component_shares(
            &perimeter_sources,
            &cell_by_id,
            &allocated_by_cell,
            commitment_bps,
        )?;
        let expected_requested =
            expected_source_commitments
                .values()
                .try_fold(0_u64, |total, &commitment| {
                    total
                        .checked_add(commitment)
                        .context("contact-building expansion commitment overflow")
                })?;
        if expected_requested == 0 {
            continue;
        }
        let source_seed = *source_component
            .first()
            .context("contact-building source component unexpectedly empty")?;
        candidates.push((
            distance,
            FocusedClusterExpandCandidate {
                source_seed,
                source_component,
                approach_source,
                focus_cell,
                focus_distance: distance,
                expected_source_commitments,
                expected_requested,
            },
        ));
    }
    candidates.sort_unstable_by_key(|(distance, candidate)| {
        (
            *distance,
            std::cmp::Reverse(candidate.expected_requested),
            std::cmp::Reverse(candidate.source_component.len()),
            candidate.source_seed,
            candidate.focus_cell,
        )
    });
    candidates
        .into_iter()
        .next()
        .map(|(_, candidate)| candidate)
        .context(
            "no owned cluster with available infantry has a traversable neutral route toward enemy contact",
        )
}

fn nearest_neutral_focus_toward_enemy(
    source_component: &BTreeSet<u32>,
    enemy_player_id: u16,
    terrain_by_id: &HashMap<u32, CellTerrain>,
    cell_by_id: &HashMap<u32, CellState>,
    cell_by_coordinate: &HashMap<Axial, u32>,
    max_elevation_step: u16,
) -> Option<(u32, u32, u32)> {
    let mut reached = BTreeSet::new();
    let mut pending = VecDeque::new();
    for &source_cell in source_component {
        let source = terrain_by_id.get(&source_cell)?;
        for coordinate in Axial::new(source.q, source.r).neighbors() {
            let Some(&neighbor) = cell_by_coordinate.get(&coordinate) else {
                continue;
            };
            let Some(state) = cell_by_id.get(&neighbor) else {
                continue;
            };
            let Some(terrain) = terrain_by_id.get(&neighbor) else {
                continue;
            };
            if state.owner_player_id == 0
                && terrain.passable
                && terrain.capturable
                && terrain_edge_is_traversable_with_limit(
                    terrain_by_id,
                    source_cell,
                    neighbor,
                    max_elevation_step,
                )
                && reached.insert(neighbor)
            {
                pending.push_back((neighbor, 1_u32, source_cell));
            }
        }
    }

    while let Some((current, distance, approach_source)) = pending.pop_front() {
        let terrain = terrain_by_id.get(&current)?;
        let neighbors = Axial::new(terrain.q, terrain.r)
            .neighbors()
            .into_iter()
            .filter_map(|coordinate| cell_by_coordinate.get(&coordinate).copied())
            .collect::<Vec<_>>();
        if neighbors.iter().any(|neighbor| {
            cell_by_id
                .get(neighbor)
                .is_some_and(|state| state.owner_player_id == enemy_player_id)
                && terrain_edge_is_traversable_with_limit(
                    terrain_by_id,
                    current,
                    *neighbor,
                    max_elevation_step,
                )
        }) {
            return Some((distance, current, approach_source));
        }
        for neighbor in neighbors {
            let Some(state) = cell_by_id.get(&neighbor) else {
                continue;
            };
            let Some(terrain) = terrain_by_id.get(&neighbor) else {
                continue;
            };
            if state.owner_player_id == 0
                && terrain.passable
                && terrain.capturable
                && terrain_edge_is_traversable_with_limit(
                    terrain_by_id,
                    current,
                    neighbor,
                    max_elevation_step,
                )
                && reached.insert(neighbor)
            {
                pending.push_back((neighbor, distance.saturating_add(1), approach_source));
            }
        }
    }
    None
}

#[allow(clippy::too_many_lines)]
fn select_cluster_attack_candidate(
    conn: &DbConnection,
    player_id: u16,
    enemy_player_id: u16,
    commitment_bps: u32,
) -> Result<ClusterAttackCandidate> {
    let terrain_by_id = conn
        .db
        .cell_terrain()
        .iter()
        .map(|terrain| (terrain.cell_id, terrain))
        .collect::<HashMap<_, _>>();
    let cell_by_id = conn
        .db
        .cell_state()
        .iter()
        .map(|cell| (cell.cell_id, cell))
        .collect::<HashMap<_, _>>();
    let cell_by_coordinate = terrain_by_id
        .values()
        .map(|terrain| (Axial::new(terrain.q, terrain.r), terrain.cell_id))
        .collect::<HashMap<_, _>>();
    let max_elevation_step = current_max_elevation_step(conn)?;
    let allocated_by_cell = allocated_infantry_by_cell(conn, player_id)?;
    let source_components = owned_traversable_components(conn, player_id)?;
    let target_components = owned_traversable_components(conn, enemy_player_id)?;
    let source_sizes = source_components
        .iter()
        .map(BTreeSet::len)
        .collect::<Vec<_>>();
    let target_sizes = target_components
        .iter()
        .map(BTreeSet::len)
        .collect::<Vec<_>>();
    let mut shared_component_pairs = 0_usize;
    let mut pairs_with_available_share = 0_usize;
    let mut pairs_with_outside_guard = 0_usize;
    let mut candidates = Vec::new();
    for source_component in &source_components {
        for target_component in &target_components {
            let mut shared_front_sources = BTreeSet::new();
            let mut shared_front_targets = BTreeSet::new();
            for &source_cell in source_component {
                let source_terrain = &terrain_by_id[&source_cell];
                for coordinate in Axial::new(source_terrain.q, source_terrain.r).neighbors() {
                    let Some(&target_cell) = cell_by_coordinate.get(&coordinate) else {
                        continue;
                    };
                    if target_component.contains(&target_cell)
                        && terrain_edge_is_traversable_with_limit(
                            &terrain_by_id,
                            source_cell,
                            target_cell,
                            max_elevation_step,
                        )
                    {
                        shared_front_sources.insert(source_cell);
                        shared_front_targets.insert(target_cell);
                    }
                }
            }
            if shared_front_targets.is_empty() {
                continue;
            }
            shared_component_pairs += 1;
            let expected_source_commitments = expected_component_shares(
                &shared_front_sources,
                &cell_by_id,
                &allocated_by_cell,
                commitment_bps,
            )?;
            let expected_requested =
                expected_source_commitments
                    .values()
                    .try_fold(0_u64, |total, &commitment| {
                        total
                            .checked_add(commitment)
                            .context("enemy-cluster attack commitment overflow")
                    })?;
            if expected_requested == 0 {
                continue;
            }
            pairs_with_available_share += 1;

            let mut outside_guard_cells = BTreeSet::new();
            for &target_cell in target_component {
                let target_terrain = &terrain_by_id[&target_cell];
                for coordinate in Axial::new(target_terrain.q, target_terrain.r).neighbors() {
                    let Some(&outside_cell) = cell_by_coordinate.get(&coordinate) else {
                        continue;
                    };
                    if target_component.contains(&outside_cell) {
                        continue;
                    }
                    let outside_state = &cell_by_id[&outside_cell];
                    let outside_terrain = &terrain_by_id[&outside_cell];
                    if outside_state.owner_player_id == 0
                        && outside_terrain.passable
                        && outside_terrain.capturable
                        && terrain_edge_is_traversable_with_limit(
                            &terrain_by_id,
                            target_cell,
                            outside_cell,
                            max_elevation_step,
                        )
                    {
                        outside_guard_cells.insert(outside_cell);
                    }
                }
            }
            if outside_guard_cells.is_empty() {
                continue;
            }
            pairs_with_outside_guard += 1;
            candidates.push(ClusterAttackCandidate {
                source_seed: *shared_front_sources
                    .first()
                    .context("shared front had no source cell")?,
                source_component: source_component.clone(),
                target_seed: *shared_front_targets
                    .first()
                    .context("shared front had no target cell")?,
                target_component: target_component.clone(),
                shared_front_targets,
                outside_guard_cells,
                expected_source_commitments,
                expected_requested,
            });
        }
    }
    candidates.sort_unstable_by_key(|candidate| {
        (
            std::cmp::Reverse(candidate.expected_requested),
            std::cmp::Reverse(candidate.shared_front_targets.len()),
            std::cmp::Reverse(candidate.outside_guard_cells.len()),
            candidate.source_seed,
            candidate.target_seed,
        )
    });
    let Some(candidate) = candidates.into_iter().next() else {
        bail!(
            "no safe shared enemy-front candidate exists: attacker=P{player_id} component_sizes={source_sizes:?}, enemy=P{enemy_player_id} component_sizes={target_sizes:?}, shared_component_pairs={shared_component_pairs}, pairs_with_available_share={pairs_with_available_share}, pairs_with_traversable_neutral_outside_guard={pairs_with_outside_guard}; the target-mask scenario requires a fresh fixture with an observable spill boundary"
        );
    };
    Ok(candidate)
}

fn current_max_elevation_step(conn: &DbConnection) -> Result<u16> {
    Ok(u16::from(
        conn.db
            .match_config()
            .singleton_id()
            .find(&SINGLETON_ID)
            .context("match config is absent while resolving cluster topology")?
            .max_elevation_step,
    ))
}

fn players_share_traversable_front(
    conn: &DbConnection,
    player_id: u16,
    enemy_player_id: u16,
) -> Result<bool> {
    let terrain_by_id = conn
        .db
        .cell_terrain()
        .iter()
        .map(|terrain| (terrain.cell_id, terrain))
        .collect::<HashMap<_, _>>();
    let cell_by_id = conn
        .db
        .cell_state()
        .iter()
        .map(|cell| (cell.cell_id, cell))
        .collect::<HashMap<_, _>>();
    let cell_by_coordinate = terrain_by_id
        .values()
        .map(|terrain| (Axial::new(terrain.q, terrain.r), terrain.cell_id))
        .collect::<HashMap<_, _>>();
    let max_elevation_step = current_max_elevation_step(conn)?;
    for source in cell_by_id
        .values()
        .filter(|cell| cell.owner_player_id == player_id)
    {
        let Some(source_terrain) = terrain_by_id.get(&source.cell_id) else {
            continue;
        };
        for coordinate in Axial::new(source_terrain.q, source_terrain.r).neighbors() {
            let Some(&target_cell) = cell_by_coordinate.get(&coordinate) else {
                continue;
            };
            if cell_by_id
                .get(&target_cell)
                .is_some_and(|cell| cell.owner_player_id == enemy_player_id)
                && terrain_edge_is_traversable_with_limit(
                    &terrain_by_id,
                    source.cell_id,
                    target_cell,
                    max_elevation_step,
                )
            {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

fn owned_traversable_components(conn: &DbConnection, player_id: u16) -> Result<Vec<BTreeSet<u32>>> {
    let terrain_by_id = conn
        .db
        .cell_terrain()
        .iter()
        .map(|terrain| (terrain.cell_id, terrain))
        .collect::<HashMap<_, _>>();
    let cell_by_coordinate = terrain_by_id
        .values()
        .map(|terrain| (Axial::new(terrain.q, terrain.r), terrain.cell_id))
        .collect::<HashMap<_, _>>();
    let max_elevation_step = current_max_elevation_step(conn)?;
    let mut unvisited = conn
        .db
        .cell_state()
        .iter()
        .filter_map(|cell| {
            let terrain = terrain_by_id.get(&cell.cell_id)?;
            (cell.owner_player_id == player_id && terrain.passable).then_some(cell.cell_id)
        })
        .collect::<BTreeSet<_>>();
    let mut components = Vec::new();
    while let Some(seed) = unvisited.first().copied() {
        unvisited.remove(&seed);
        let mut component = BTreeSet::from([seed]);
        let mut pending = VecDeque::from([seed]);
        while let Some(current) = pending.pop_front() {
            let current_terrain = &terrain_by_id[&current];
            for coordinate in Axial::new(current_terrain.q, current_terrain.r).neighbors() {
                let Some(&neighbor) = cell_by_coordinate.get(&coordinate) else {
                    continue;
                };
                if !unvisited.contains(&neighbor)
                    || !terrain_edge_is_traversable_with_limit(
                        &terrain_by_id,
                        current,
                        neighbor,
                        max_elevation_step,
                    )
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
    components.sort_unstable_by_key(|component| component.first().copied().unwrap_or(u32::MAX));
    Ok(components)
}

fn allocated_infantry_by_cell(conn: &DbConnection, player_id: u16) -> Result<HashMap<u32, u64>> {
    let mut allocated = HashMap::<u32, u64>::new();
    for packet in conn
        .db
        .transit_packet()
        .iter()
        .filter(|packet| packet.owner_player_id == player_id)
    {
        let current = allocated.entry(packet.current_cell).or_default();
        *current = current
            .checked_add(packet.infantry)
            .context("allocated cluster-action infantry overflow")?;
    }
    Ok(allocated)
}

fn expected_component_shares(
    component: &BTreeSet<u32>,
    cell_by_id: &HashMap<u32, CellState>,
    allocated_by_cell: &HashMap<u32, u64>,
    commitment_bps: u32,
) -> Result<HashMap<u32, u64>> {
    component
        .iter()
        .map(|&cell_id| {
            let infantry = cell_by_id
                .get(&cell_id)
                .with_context(|| format!("cluster source cell {cell_id} is absent"))?
                .infantry;
            let allocated = allocated_by_cell.get(&cell_id).copied().unwrap_or(0);
            let available = infantry.checked_sub(allocated).with_context(|| {
                format!(
                    "cluster source {cell_id} has {allocated} allocated infantry but only {infantry} physically present"
                )
            })?;
            Ok((cell_id, basis_point_share(available, commitment_bps)))
        })
        .collect()
}

fn neutral_perimeter_sources(
    component: &BTreeSet<u32>,
    terrain_by_id: &HashMap<u32, CellTerrain>,
    cell_by_id: &HashMap<u32, CellState>,
    cell_by_coordinate: &HashMap<Axial, u32>,
    max_elevation_step: u16,
) -> BTreeSet<u32> {
    component
        .iter()
        .copied()
        .filter(|source_cell| {
            let source = &terrain_by_id[source_cell];
            Axial::new(source.q, source.r)
                .neighbors()
                .into_iter()
                .any(|coordinate| {
                    let Some(&target_cell) = cell_by_coordinate.get(&coordinate) else {
                        return false;
                    };
                    let target_state = &cell_by_id[&target_cell];
                    let target_terrain = &terrain_by_id[&target_cell];
                    target_state.owner_player_id == 0
                        && target_terrain.passable
                        && target_terrain.capturable
                        && terrain_edge_is_traversable_with_limit(
                            terrain_by_id,
                            *source_cell,
                            target_cell,
                            max_elevation_step,
                        )
                })
        })
        .collect()
}

fn terrain_edge_is_traversable_with_limit(
    terrain_by_id: &HashMap<u32, CellTerrain>,
    from_cell: u32,
    to_cell: u32,
    max_elevation_step: u16,
) -> bool {
    terrain_by_id
        .get(&from_cell)
        .zip(terrain_by_id.get(&to_cell))
        .is_some_and(|(from, to)| {
            from.passable
                && to.passable
                && from.elevation.abs_diff(to.elevation) <= max_elevation_step
        })
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

fn wave_outside_depths(
    player_id: u16,
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
    perimeter_sources: &BTreeSet<u32>,
    outside_depths: &HashMap<u32, u16>,
    terrain_by_id: &HashMap<u32, CellTerrain>,
    cell_by_coordinate: &HashMap<Axial, u32>,
) -> HashMap<u32, Vec<u32>> {
    let mut result = HashMap::new();
    for &cell_id in perimeter_sources {
        let current = &terrain_by_id[&cell_id];
        let mut targets = Axial::new(current.q, current.r)
            .neighbors()
            .into_iter()
            .filter_map(|coordinate| cell_by_coordinate.get(&coordinate).copied())
            .filter(|target| terrain_edge_is_traversable(terrain_by_id, cell_id, *target))
            .filter(|target| outside_depths.get(target) == Some(&1))
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
    perimeter_sources: &BTreeSet<u32>,
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
                        perimeter_sources.contains(&boundary)
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
    perimeter_sources: &BTreeSet<u32>,
    outside_depths: &HashMap<u32, u16>,
    children: &HashMap<u32, Vec<u32>>,
    terrain_by_id: &HashMap<u32, CellTerrain>,
    cell_by_id: &HashMap<u32, CellState>,
    max_depth: u16,
) -> HashSet<u32> {
    let mut incoming = HashMap::new();
    for &cell_id in perimeter_sources {
        let amount = commitments.get(&cell_id).copied().unwrap_or(0);
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

fn lane_owners(conn: &DbConnection, lane_cells: &[u32]) -> Result<Vec<u16>> {
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

fn transit_packet_route(conn: &DbConnection, packet: &TransitPacket) -> Result<Vec<u32>> {
    if packet.route_id == 0 {
        return Ok(if packet.current_cell == packet.destination_cell {
            vec![packet.current_cell]
        } else {
            vec![packet.current_cell, packet.destination_cell]
        });
    }
    conn.db
        .transit_route()
        .route_id()
        .find(&packet.route_id)
        .map(|route| route.cells)
        .with_context(|| {
            format!(
                "packet {} references missing route {}",
                packet.packet_key, packet.route_id
            )
        })
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
        let route = transit_packet_route(conn, packet)?;
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
            "front-push packet did not retain its commanded front target"
        );
        ensure!(
            route.first() == Some(&packet.origin_cell),
            "front-push packet route did not begin at its selected origin"
        );
        let route_index = usize::try_from(packet.route_index).context("route index overflow")?;
        ensure!(
            route.get(route_index) == Some(&packet.current_cell),
            "front-push packet current cell does not match its route index"
        );
        let anchor_index = route
            .iter()
            .position(|cell_id| *cell_id == candidate.lane_cells[0])
            .context("front-push route omitted its commanded front target")?;
        ensure!(
            anchor_index > 0
                && route[..anchor_index]
                    .iter()
                    .all(|cell_id| selected.contains(cell_id)),
            "front-push packet escaped the submitted corridor before entering its lane"
        );
        ensure!(
            route.get(anchor_index - 1) == Some(&candidate.front_cell),
            "front-push packet did not leave through the selected front cell"
        );
        let lane_suffix = &route[anchor_index..];
        ensure!(
            lane_suffix.len() <= candidate.lane_cells.len()
                && lane_suffix
                    .iter()
                    .zip(&candidate.lane_cells)
                    .all(|(actual, expected)| actual == expected),
            "front-push packet route did not extend along the submitted axial ray"
        );
        for cells in route.windows(2) {
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
        for cells in route[anchor_index - 1..].windows(2) {
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
    let mut pending_source_total = 0_u64;
    for packet in packets {
        let route = transit_packet_route(conn, &packet)?;
        ensure!(
            packet.owner_player_id == PLAYER_ONE,
            "all-front packet belongs to another player"
        );
        ensure!(
            packet.route_index == 0 && route.first() == Some(&packet.current_cell),
            "all-front packet is not positioned at the start of its local route"
        );
        let resting = route.as_slice() == [packet.current_cell]
            && packet.destination_cell == packet.current_cell;
        let crossing = route.len() == 2
            && route[1] == packet.destination_cell
            && candidate
                .children
                .get(&packet.current_cell)
                .is_some_and(|children| children.contains(&packet.destination_cell));
        ensure!(
            resting || crossing,
            "all-front packet must be one resting node or one monotonic wave edge"
        );
        ensure!(
            candidate.perimeter_sources.contains(&packet.current_cell)
                || candidate.outside_depths.contains_key(&packet.current_cell),
            "all-front packet rests outside its accepted perimeter/wave topology"
        );
        ensure!(
            packet.origin_cell == EXPANSION_AGGREGATE_ORIGIN,
            "all-front packet retained per-origin accounting"
        );
        pending_source_total = pending_source_total
            .checked_add(packet.pending_source_infantry)
            .context("all-front pending-source accounting overflow")?;
        packet_total = packet_total
            .checked_add(packet.infantry)
            .context("all-front packet accounting overflow")?;
    }
    ensure!(
        packet_total == order.in_transit_infantry,
        "all-front packet total {packet_total} differs from order in-transit infantry {}",
        order.in_transit_infantry
    );
    let queued_source_total = conn
        .db
        .transfer_source()
        .iter()
        .filter(|source| source.order_id == order.order_id)
        .map(|source| source.queued_infantry)
        .sum::<u64>();
    ensure!(
        queued_source_total == pending_source_total,
        "all-front sources report {queued_source_total} queued but packets retain {pending_source_total} pending"
    );
    Ok(())
}

fn stable_action_order_snapshot(
    conn: &DbConnection,
    order: &TransferOrder,
) -> Result<ActionOrderSnapshot> {
    for _ in 0..8 {
        let step_before = conn
            .db
            .match_state()
            .singleton_id()
            .find(&SINGLETON_ID)
            .context("match state disappeared before action snapshot")?
            .logical_step;
        let current = conn
            .db
            .transfer_order()
            .order_id()
            .find(&order.order_id)
            .with_context(|| format!("action order {} disappeared", order.order_id))?;
        let mut packets = conn
            .db
            .transit_packet()
            .iter()
            .filter(|packet| packet.order_id == order.order_id)
            .collect::<Vec<_>>();
        packets.sort_unstable_by_key(|packet| packet.packet_key);
        let step_after = conn
            .db
            .match_state()
            .singleton_id()
            .find(&SINGLETON_ID)
            .context("match state disappeared after action snapshot")?
            .logical_step;
        if step_before != step_after {
            continue;
        }
        return Ok(ActionOrderSnapshot {
            logical_step: step_after,
            status: current.status,
            requested_infantry: current.requested_infantry,
            committed_infantry: current.committed_infantry,
            in_transit_infantry: current.in_transit_infantry,
            delivered_infantry: current.delivered_infantry,
            casualty_infantry: current.casualty_infantry,
            updated_step: current.updated_step,
            packets,
        });
    }
    bail!(
        "simulation advanced across eight consecutive reads of action order {}",
        order.order_id
    )
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn assert_cluster_action_order(
    conn: &DbConnection,
    order: &TransferOrder,
    player_id: u16,
    command_id: u64,
    kind: OrderKind,
    expected_requested: u64,
    expected_source_component: &BTreeSet<u32>,
    expected_source_commitments: &HashMap<u32, u64>,
    require_exact_source_snapshot: bool,
) -> Result<()> {
    let snapshot = stable_action_order_snapshot(conn, order)?;
    // Recruitment and concurrent explicit packets can change per-cell
    // availability between the client snapshot and reducer acceptance.
    // Dynamic actions therefore prove the authoritative source ledger and
    // conservation; stable fixtures retain the exact snapshot assertion.
    let commitment_matches_snapshot =
        !require_exact_source_snapshot || snapshot.committed_infantry == expected_requested;
    ensure!(
        order.player_id == player_id
            && order.client_command_id == command_id
            && order.kind == kind
            && snapshot.requested_infantry == snapshot.committed_infantry
            && commitment_matches_snapshot
            && (order.orientation_q, order.orientation_r) == (0, 0),
        "cluster action order {} persisted invalid identity or counters: player={}, command={}, kind={:?}, requested={}, committed={}, orientation=({}, {}), expected player={}, command={}, kind={kind:?}, pre-command one-share total={expected_requested}, exact snapshot required={require_exact_source_snapshot}",
        order.order_id,
        order.player_id,
        order.client_command_id,
        order.kind,
        snapshot.requested_infantry,
        snapshot.committed_infantry,
        order.orientation_q,
        order.orientation_r,
        player_id,
        command_id,
    );
    let accounted = snapshot
        .in_transit_infantry
        .checked_add(snapshot.delivered_infantry)
        .and_then(|value| value.checked_add(snapshot.casualty_infantry))
        .context("cluster action accounting overflow")?;
    ensure!(
        snapshot.committed_infantry == accounted,
        "cluster action order {} violates conservation at stable logical step {}: committed={}, in_transit={}, delivered={}, casualties={}",
        order.order_id,
        snapshot.logical_step,
        snapshot.committed_infantry,
        snapshot.in_transit_infantry,
        snapshot.delivered_infantry,
        snapshot.casualty_infantry,
    );

    let sources = conn
        .db
        .transfer_source()
        .iter()
        .filter(|source| source.order_id == order.order_id)
        .collect::<Vec<_>>();
    let actual_source_cells = sources
        .iter()
        .map(|source| source.cell_id)
        .collect::<BTreeSet<_>>();
    let expected_snapshot_cells = expected_source_commitments
        .keys()
        .copied()
        .collect::<BTreeSet<_>>();
    let expected_snapshot_total =
        expected_source_commitments
            .values()
            .try_fold(0_u64, |total, &commitment| {
                total
                    .checked_add(commitment)
                    .context("pre-command cluster source commitment overflow")
            })?;
    ensure!(
        expected_snapshot_cells.is_subset(expected_source_component)
            && expected_snapshot_total == expected_requested,
        "cluster action test fixture has sources outside its component or an inconsistent commitment total"
    );
    ensure!(
        actual_source_cells == expected_snapshot_cells,
        "authority did not restrict order {} to its participating perimeter/front: expected {} cells {:?} inside component {:?}, got {} cells {:?}",
        order.order_id,
        expected_snapshot_cells.len(),
        expected_snapshot_cells,
        expected_source_component,
        actual_source_cells.len(),
        actual_source_cells,
    );
    let mut persisted_total = 0_u64;
    for source in sources {
        if require_exact_source_snapshot {
            let expected = expected_source_commitments
                .get(&source.cell_id)
                .copied()
                .with_context(|| {
                    format!(
                        "authority persisted unexpected cluster source {}",
                        source.cell_id
                    )
                })?;
            ensure!(
                source.committed_infantry == expected,
                "cluster source {} committed {} infantry instead of its exact {expected}-infantry acceptance snapshot",
                source.cell_id,
                source.committed_infantry
            );
        }
        persisted_total = persisted_total
            .checked_add(source.committed_infantry)
            .context("persisted cluster source commitment overflow")?;
    }
    ensure!(
        persisted_total == snapshot.committed_infantry,
        "cluster source commitments sum to {persisted_total}, but order {} committed {}",
        order.order_id,
        snapshot.committed_infantry
    );
    let packet_total = snapshot.packets.iter().try_fold(0_u64, |total, packet| {
        total
            .checked_add(packet.infantry)
            .context("cluster action packet strength overflow")
    })?;
    ensure!(
        packet_total == snapshot.in_transit_infantry,
        "cluster action order {} reports {} in transit but has {packet_total} public packet infantry",
        order.order_id,
        snapshot.in_transit_infantry
    );
    Ok(())
}

fn assert_attack_stays_in_target_mask(
    conn: &DbConnection,
    order: &TransferOrder,
    player_id: u16,
    source_component: &BTreeSet<u32>,
    target_component: &BTreeSet<u32>,
    outside_guard_cells: &BTreeSet<u32>,
    owners_before: &HashMap<u32, u16>,
) -> Result<bool> {
    assert_order_conservation(order)?;
    let packets = conn
        .db
        .transit_packet()
        .iter()
        .filter(|packet| packet.order_id == order.order_id)
        .collect::<Vec<_>>();
    let mut packet_total = 0_u64;
    let mut mask_activity = false;
    for packet in packets {
        let route = transit_packet_route(conn, &packet)?;
        packet_total = packet_total
            .checked_add(packet.infantry)
            .context("masked cluster-attack packet strength overflow")?;
        ensure!(
            packet.origin_cell == EXPANSION_AGGREGATE_ORIGIN
                || source_component.contains(&packet.origin_cell),
            "AttackClusters packet {} has origin {} outside its authority-expanded source component",
            packet.packet_key,
            packet.origin_cell
        );
        for cell_id in std::iter::once(packet.current_cell)
            .chain(std::iter::once(packet.destination_cell))
            .chain(route.iter().copied())
        {
            ensure!(
                source_component.contains(&cell_id) || target_component.contains(&cell_id),
                "AttackClusters packet {} escaped its source/target mask through cell {cell_id}",
                packet.packet_key
            );
            mask_activity |= target_component.contains(&cell_id);
        }
    }
    ensure!(
        packet_total == order.in_transit_infantry,
        "AttackClusters order {} reports {} in transit but exposes {packet_total} packet infantry",
        order.order_id,
        order.in_transit_infantry
    );
    let captures = owner_changes_for_player(conn, player_id, owners_before);
    ensure!(
        captures.is_subset(target_component),
        "AttackClusters acquired cells outside its immutable target component: captures={captures:?}, target_mask={target_component:?}"
    );
    mask_activity |= !captures.is_empty() || order.casualty_infantry > 0;
    for &guard_cell in outside_guard_cells {
        let owner = conn
            .db
            .cell_state()
            .cell_id()
            .find(&guard_cell)
            .with_context(|| format!("outside-mask guard cell {guard_cell} disappeared"))?
            .owner_player_id;
        ensure!(
            owner != player_id,
            "AttackClusters captured traversable outside-mask guard cell {guard_cell}"
        );
    }
    Ok(mask_activity)
}

fn owner_changes_for_player(
    conn: &DbConnection,
    player_id: u16,
    before: &HashMap<u32, u16>,
) -> BTreeSet<u32> {
    conn.db
        .cell_state()
        .iter()
        .filter_map(|cell| {
            (cell.owner_player_id == player_id
                && before.get(&cell.cell_id).copied() != Some(player_id))
            .then_some(cell.cell_id)
        })
        .collect()
}

fn owner_snapshot(conn: &DbConnection) -> HashMap<u32, u16> {
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
