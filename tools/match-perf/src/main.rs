//! Live-match load driver: grows both players with full-share expansion, then
//! switches on long-route Center policies, then attacks. Continuously samples
//! the authoritative logical-step counter and emits a CSV timeline so reducer
//! dilation (gaps above the 250 ms cadence) is directly visible.

#![forbid(unsafe_code)]

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::Parser;
use match_bindings::{
    CellStateTableAccess, CellTerrainTableAccess, ClusterPolicyKind, DbConnection,
    MatchConfigTableAccess, MatchPhase, MatchStateTableAccess, OrderStatus, TerrainClass,
    TransferOrderTableAccess, TransitPacketTableAccess, issue_attack_clusters as _,
    issue_expand_clusters as _, join_match as _, set_cluster_policy as _,
    set_mobilization_target as _,
};
use spacetimedb_sdk::{DbContext, Table};

const SINGLETON_ID: u8 = 0;

#[derive(Debug, Parser)]
struct Args {
    #[arg(long, default_value = "http://127.0.0.1:3000")]
    host: String,
    #[arg(long, default_value = "of-match-perf")]
    database: String,
    #[arg(long, default_value = ".match-perf-tokens")]
    token_dir: PathBuf,
    /// Seconds of full-share expansion before policies switch on.
    #[arg(long, default_value_t = 150)]
    expand_secs: u64,
    /// Seconds of Center-policy redistribution measurement.
    #[arg(long, default_value_t = 180)]
    policy_secs: u64,
    /// Seconds of mutual attack measurement (0 skips the phase).
    #[arg(long, default_value_t = 90)]
    attack_secs: u64,
    /// Re-issue expansion waves at this interval while expanding.
    #[arg(long, default_value_t = 45)]
    reexpand_secs: u64,
}

enum LifecycleEvent {
    Connected { token: String },
    Subscribed,
    Failed(String),
    Disconnected(Option<String>),
}

struct Client {
    label: &'static str,
    conn: DbConnection,
    _events: Receiver<LifecycleEvent>,
    _pump: JoinHandle<()>,
}

impl Client {
    fn connect(label: &'static str, token_path: &Path, args: &Args) -> Result<Self> {
        let existing = fs::read_to_string(token_path).ok();
        let (tx, rx) = mpsc::channel();
        let conn = DbConnection::builder()
            .with_uri(&args.host)
            .with_database_name(&args.database)
            .with_token(existing)
            .on_connect({
                let tx = tx.clone();
                move |ctx, _identity, token| {
                    let _ = tx.send(LifecycleEvent::Connected {
                        token: token.to_owned(),
                    });
                    ctx.subscription_builder()
                        .on_applied({
                            let tx = tx.clone();
                            move |_| {
                                let _ = tx.send(LifecycleEvent::Subscribed);
                            }
                        })
                        .on_error({
                            let tx = tx.clone();
                            move |_, error| {
                                let _ = tx.send(LifecycleEvent::Failed(format!(
                                    "subscription failed: {error}"
                                )));
                            }
                        })
                        .subscribe_to_all_tables();
                }
            })
            .on_connect_error({
                let tx = tx.clone();
                move |_, error| {
                    let _ = tx.send(LifecycleEvent::Failed(format!(
                        "connection establishment failed: {error}"
                    )));
                }
            })
            .on_disconnect({
                let tx = tx.clone();
                move |_, error| {
                    let _ = tx.send(LifecycleEvent::Disconnected(
                        error.map(|value| value.to_string()),
                    ));
                }
            })
            .build()
            .with_context(|| format!("build {label} connection"))?;
        let pump = conn.run_threaded();

        let deadline = Instant::now() + Duration::from_secs(30);
        let mut subscribed = false;
        let mut token = None;
        while !subscribed || token.is_none() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                bail!("{label} connection readiness timed out");
            }
            match rx.recv_timeout(remaining)? {
                LifecycleEvent::Connected { token: new } => token = Some(new),
                LifecycleEvent::Subscribed => subscribed = true,
                LifecycleEvent::Failed(message) => bail!("{label}: {message}"),
                LifecycleEvent::Disconnected(error) => {
                    bail!("{label} disconnected early: {error:?}")
                }
            }
        }
        if let Some(parent) = token_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(token_path, token.context("missing token")?)?;
        Ok(Self {
            label,
            conn,
            _events: rx,
            _pump: pump,
        })
    }

    fn call<O>(
        _name: &str,
        register: impl FnOnce(mpsc::Sender<Result<(), String>>) -> Result<O>,
    ) -> Result<(Duration, Result<(), String>)> {
        let (tx, rx) = mpsc::channel();
        let started = Instant::now();
        register(tx)?;
        let result = rx.recv_timeout(Duration::from_mins(2))?;
        Ok((started.elapsed(), result))
    }

    fn join(&self, player: u8) -> Result<Duration> {
        let (rtt, result) = Self::call("join_match", |tx| {
            self.conn
                .reducers
                .join_match_then(player, self.label.to_owned(), move |_, result| {
                    let _ = tx.send(
                        result
                            .map_err(|error| error.to_string())
                            .and_then(|inner| inner.map_err(|error| error.clone())),
                    );
                })
                .map_err(|error| anyhow::anyhow!(error.to_string()))
        })?;
        result.map_err(anyhow::Error::msg)?;
        Ok(rtt)
    }

    fn expand(
        &self,
        command: u64,
        seeds: Vec<u32>,
        focus: u32,
    ) -> Result<(Duration, Result<(), String>)> {
        Self::call("issue_expand_clusters", |tx| {
            self.conn
                .reducers
                .issue_expand_clusters_then(command, seeds, focus, 10_000, move |_, result| {
                    let _ = tx.send(
                        result
                            .map_err(|error| error.to_string())
                            .and_then(|inner| inner.map_err(|error| error.clone())),
                    );
                })
                .map_err(|error| anyhow::anyhow!(error.to_string()))
        })
    }

    fn policy(
        &self,
        command: u64,
        seeds: Vec<u32>,
        kind: ClusterPolicyKind,
    ) -> Result<(Duration, Result<(), String>)> {
        Self::call("set_cluster_policy", |tx| {
            self.conn
                .reducers
                .set_cluster_policy_then(command, seeds, kind, 0, 0, move |_, result| {
                    let _ = tx.send(
                        result
                            .map_err(|error| error.to_string())
                            .and_then(|inner| inner.map_err(|error| error.clone())),
                    );
                })
                .map_err(|error| anyhow::anyhow!(error.to_string()))
        })
    }

    fn attack(
        &self,
        command: u64,
        sources: Vec<u32>,
        targets: Vec<u32>,
    ) -> Result<(Duration, Result<(), String>)> {
        Self::call("issue_attack_clusters", |tx| {
            self.conn
                .reducers
                .issue_attack_clusters_then(command, sources, targets, 10_000, move |_, result| {
                    let _ = tx.send(
                        result
                            .map_err(|error| error.to_string())
                            .and_then(|inner| inner.map_err(|error| error.clone())),
                    );
                })
                .map_err(|error| anyhow::anyhow!(error.to_string()))
        })
    }

    fn mobilization(&self, command: u64, target_bps: u32) -> Result<()> {
        let (tx, rx) = mpsc::channel();
        self.conn
            .reducers
            .set_mobilization_target_then(command, target_bps, move |_, result| {
                let _ = tx.send(
                    result
                        .map_err(|error| error.to_string())
                        .and_then(|inner| inner.map_err(|error| error.clone())),
                );
            })
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        rx.recv_timeout(Duration::from_secs(30))?
            .map_err(anyhow::Error::msg)?;
        Ok(())
    }
}

struct Sample {
    elapsed: f64,
    step: u64,
    gap_ms: f64,
    owned_one: u64,
    owned_two: u64,
    packets: usize,
    active_orders: usize,
    phase: &'static str,
}

#[allow(clippy::too_many_arguments)]
fn record(
    out: &mut impl Write,
    phase: &'static str,
    started: Instant,
    step: u64,
    gap_ms: f64,
    one: &Client,
    two: &Client,
) -> Result<()> {
    let state = one
        .conn
        .db
        .match_state()
        .singleton_id()
        .find(&SINGLETON_ID)
        .context("match state missing")?;
    let packets = one.conn.db.transit_packet().iter().count();
    let active_orders = one
        .conn
        .db
        .transfer_order()
        .iter()
        .filter(|order| order.status == OrderStatus::Active)
        .count();
    let sample = Sample {
        elapsed: started.elapsed().as_secs_f64(),
        step,
        gap_ms,
        owned_one: state.player_one_controlled,
        owned_two: state.player_two_controlled,
        packets,
        active_orders,
        phase,
    };
    let _ = (one, two);
    writeln!(
        out,
        "{:.3},{},{:.1},{},{},{},{},{}",
        sample.elapsed,
        sample.step,
        sample.gap_ms,
        sample.owned_one,
        sample.owned_two,
        sample.packets,
        sample.active_orders,
        sample.phase,
    )?;
    out.flush()?;
    Ok(())
}

fn current_step(client: &Client) -> Result<u64> {
    Ok(client
        .conn
        .db
        .match_state()
        .singleton_id()
        .find(&SINGLETON_ID)
        .context("match state missing")?
        .logical_step)
}

fn phase_of(client: &Client) -> Result<MatchPhase> {
    Ok(client
        .conn
        .db
        .match_state()
        .singleton_id()
        .find(&SINGLETON_ID)
        .context("match state missing")?
        .phase)
}

/// Finds a neutral passable capturable cell near the opposing spawn to aim a
/// wave at, maximizing sustained wave length for the load test.
fn neutral_focus(client: &Client, near_cell: u32) -> Option<u32> {
    let terrain = client.conn.db.cell_terrain().cell_id().find(&near_cell)?;
    let mut best = None::<(i64, u32)>;
    for row in client.conn.db.cell_terrain().iter() {
        if !row.passable || !row.capturable || matches!(row.terrain, TerrainClass::Water) {
            continue;
        }
        let owned = client
            .conn
            .db
            .cell_state()
            .cell_id()
            .find(&row.cell_id)
            .is_some_and(|state| state.owner_player_id != 0);
        if owned {
            continue;
        }
        let distance = i64::from((row.q - terrain.q).abs()) + i64::from((row.r - terrain.r).abs());
        let score = -distance;
        if best.is_none_or(|(current, _)| score > current) {
            best = Some((score, row.cell_id));
        }
    }
    best.map(|(_, cell)| cell)
}

#[allow(clippy::too_many_lines)]
fn main() -> Result<()> {
    env_logger_init();
    let args = Args::parse();
    let out_path = PathBuf::from(format!("perf-{}.csv", args.database));
    let mut out = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&out_path)?;
    writeln!(
        out,
        "elapsed_s,logical_step,gap_ms,owned_one,owned_two,packets,active_orders,phase"
    )?;

    let one = Client::connect("player-one", &args.token_dir.join("one.token"), &args)?;
    let two = Client::connect("player-two", &args.token_dir.join("two.token"), &args)?;
    println!("connected both clients");

    let rtt_one = one.join(1)?;
    println!("player one joined in {rtt_one:.2?}");
    let rtt_two = two.join(2)?;
    println!("player two joined in {rtt_two:.2?}");

    // Wait for the running phase and the first scheduled tick.
    let wait_started = Instant::now();
    while phase_of(&one)? != MatchPhase::Running {
        if wait_started.elapsed() > Duration::from_secs(20) {
            bail!("match never entered the running phase");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    one.mobilization(11, 6_000)?;
    two.mobilization(12, 6_000)?;
    let config = one
        .conn
        .db
        .match_config()
        .singleton_id()
        .find(&SINGLETON_ID)
        .context("config missing")?;
    let (spawn_one, spawn_two) = (config.spawn_one_cell, config.spawn_two_cell);
    println!("match running; spawns {spawn_one} / {spawn_two}");

    let started = Instant::now();
    let mut last_step = current_step(&one)?;
    let mut last_change = Instant::now();
    let mut last_expand = Instant::now()
        .checked_sub(Duration::from_secs(args.reexpand_secs))
        .unwrap_or_else(Instant::now);
    let mut policies_set = false;
    let mut attack_issued = false;
    let mut command: u64 = 1_000;
    let mut phase: &'static str = "expand";

    loop {
        let elapsed_duration = started.elapsed();
        let elapsed = elapsed_duration.as_secs_f64();
        if phase == "expand" && elapsed_duration >= Duration::from_secs(args.expand_secs) {
            phase = "policy";
        } else if phase == "policy"
            && elapsed_duration >= Duration::from_secs(args.expand_secs + args.policy_secs)
        {
            phase = if args.attack_secs > 0 {
                "attack"
            } else {
                "done"
            };
        } else if phase == "attack"
            && elapsed_duration
                >= Duration::from_secs(args.expand_secs + args.policy_secs + args.attack_secs)
        {
            phase = "done";
        }
        if phase == "done" {
            break;
        }

        if phase == "expand" && last_expand.elapsed() >= Duration::from_secs(args.reexpand_secs) {
            last_expand = Instant::now();
            command += 1;
            if let Some(focus) = neutral_focus(&one, spawn_two) {
                let (rtt, result) = one.expand(command, vec![spawn_one], focus)?;
                println!(
                    "[{elapsed:7.1}s] p1 expand -> {} rtt={rtt:.2?}",
                    show(&result)
                );
            }
            command += 1;
            if let Some(focus) = neutral_focus(&two, spawn_one) {
                let (rtt, result) = two.expand(command, vec![spawn_two], focus)?;
                println!(
                    "[{elapsed:7.1}s] p2 expand -> {} rtt={rtt:.2?}",
                    show(&result)
                );
            }
        }
        if phase == "policy" && !policies_set {
            policies_set = true;
            command += 1;
            let (rtt, result) = one.policy(command, vec![spawn_one], ClusterPolicyKind::Center)?;
            println!(
                "[{elapsed:7.1}s] p1 Center policy -> {} rtt={rtt:.2?}",
                show(&result)
            );
            command += 1;
            let (rtt, result) = two.policy(command, vec![spawn_two], ClusterPolicyKind::Center)?;
            println!(
                "[{elapsed:7.1}s] p2 Center policy -> {} rtt={rtt:.2?}",
                show(&result)
            );
        }
        if phase == "attack" && !attack_issued {
            attack_issued = true;
            command += 1;
            let (rtt, result) = one.attack(command, vec![spawn_one], vec![spawn_two])?;
            println!(
                "[{elapsed:7.1}s] p1 attack -> {} rtt={rtt:.2?}",
                show(&result)
            );
            command += 1;
            let (rtt, result) = two.attack(command, vec![spawn_two], vec![spawn_one])?;
            println!(
                "[{elapsed:7.1}s] p2 attack -> {} rtt={rtt:.2?}",
                show(&result)
            );
        }

        let step = current_step(&one)?;
        if step != last_step {
            let gap = last_change.elapsed().as_secs_f64() * 1_000.0;
            record(&mut out, phase, started, step, gap, &one, &two)?;
            last_step = step;
            last_change = Instant::now();
        }
        std::thread::sleep(Duration::from_millis(15));
    }
    println!("scenario complete; CSV at {}", out_path.display());
    Ok(())
}

fn env_logger_init() {
    // Keep dependency surface minimal: no env_logger; placeholder for symmetry.
}

fn show(result: &Result<(), String>) -> String {
    match result {
        Ok(()) => "ok".to_owned(),
        Err(error) => format!("rejected: {error}"),
    }
}
