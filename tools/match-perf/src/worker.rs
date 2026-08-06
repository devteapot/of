//! Worker: owns a contiguous player range with configurable subscription load.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use clap::Parser;
use match_bindings::{
    MatchPhase, issue_attack_clusters as _, issue_expand_clusters as _, issue_front_rebalance as _,
    set_mobilization_target as _,
};

use crate::client::{Client, require_reducer_success};
use crate::common::{
    CommandKind, DEFAULT_LOGICAL_STEP_MS, PhaseSchedule, PlayerRange, PresetArg, ScenarioPhase,
    SubscriptionMode, WorkerStatus, WorkerStatusKind, deterministic_command_id,
    due_players_from_pending, validate_command_spread_for_schedule, worker_status_path,
    write_worker_status,
};
use crate::front_rebalance::FrontRebalancePlan;
use crate::output::WorkerLog;
use crate::queries::{subscription_mode_detail, worker_command_queries, worker_observer_queries};

/// Matches game-client high-scale interest radius (server chunk units).
const INTEREST_CHUNK_RADIUS: i16 = 2;

#[derive(Debug, Parser)]
pub struct WorkerArgs {
    #[arg(long, default_value = "http://127.0.0.1:3000")]
    pub host: String,
    #[arg(long, default_value = "of-match-perf")]
    pub database: String,
    #[arg(long, default_value = ".match-perf-tokens")]
    pub token_dir: PathBuf,
    /// First player id in this worker's contiguous range (1-based).
    #[arg(long, value_parser = clap::value_parser!(u16).range(1..=500))]
    pub first_player: u16,
    /// Number of players owned by this worker.
    #[arg(long, value_parser = clap::value_parser!(u16).range(1..=500))]
    pub player_count: u16,
    /// Total configured match players (for attack-front discovery).
    #[arg(long, value_parser = clap::value_parser!(u16).range(2..=500))]
    pub match_players: u16,
    #[arg(long)]
    pub output_dir: PathBuf,
    #[arg(long, value_enum, default_value_t = PresetArg::Dev)]
    pub preset: PresetArg,
    #[arg(long)]
    pub expand_steps: Option<u64>,
    #[arg(long)]
    pub rebalance_steps: Option<u64>,
    #[arg(long)]
    pub attack_steps: Option<u64>,
    #[arg(long)]
    pub reexpand_steps: Option<u64>,
    #[arg(long)]
    pub expand_secs: Option<u64>,
    #[arg(long)]
    pub rebalance_secs: Option<u64>,
    #[arg(long)]
    pub attack_secs: Option<u64>,
    #[arg(long)]
    pub reexpand_secs: Option<u64>,
    /// Shared absolute warmup before phase progress starts at `logical_step` 0.
    /// Default is intentionally longer than a smoke test so remote multi-host
    /// joins/setup can finish before phase progress begins.
    #[arg(long, default_value_t = 120)]
    pub warmup_steps: u64,
    #[arg(long, value_enum, default_value_t = SubscriptionMode::FullClient)]
    pub subscription_mode: SubscriptionMode,
    /// Deterministic stagger modulus for phase command waves (`player_id % N`).
    #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u32).range(1..=500))]
    pub command_spread: u32,
    #[arg(long, default_value_t = 6_000, value_parser = clap::value_parser!(u32).range(0..=10_000))]
    pub mobilization_bps: u32,
    #[arg(long, default_value_t = 10_000, value_parser = clap::value_parser!(u32).range(1..=10_000))]
    pub command_share_bps: u32,
    #[arg(long, default_value_t = 300)]
    pub ready_timeout_secs: u64,
    #[arg(long, default_value_t = 120)]
    pub connect_timeout_secs: u64,
    #[arg(long, default_value_t = 60)]
    pub command_timeout_secs: u64,
    /// When set, require a shared ready.marker (legacy single-host FS). Default
    /// polls authoritative locked `match_config` so workers can run on other hosts.
    #[arg(long, default_value_t = false)]
    pub require_shared_ready_marker: bool,
}

fn resolve_schedule(args: &WorkerArgs) -> Result<PhaseSchedule> {
    let has_steps = args.expand_steps.is_some()
        || args.rebalance_steps.is_some()
        || args.attack_steps.is_some()
        || args.reexpand_steps.is_some();
    let has_secs = args.expand_secs.is_some()
        || args.rebalance_secs.is_some()
        || args.attack_secs.is_some()
        || args.reexpand_secs.is_some();
    if has_steps {
        PhaseSchedule::from_steps(
            args.expand_steps.unwrap_or(600),
            args.rebalance_steps.unwrap_or(720),
            args.attack_steps.unwrap_or(360),
            args.reexpand_steps.unwrap_or(180),
        )
    } else if has_secs {
        PhaseSchedule::from_secs(
            args.expand_secs.unwrap_or(150),
            args.rebalance_secs.unwrap_or(180),
            args.attack_secs.unwrap_or(90),
            args.reexpand_secs.unwrap_or(45),
            DEFAULT_LOGICAL_STEP_MS,
        )
    } else {
        PhaseSchedule::from_steps(600, 720, 360, 180)
    }
}

fn now_unix_s() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn emit_status(
    args: &WorkerArgs,
    range: PlayerRange,
    kind: WorkerStatusKind,
    message: impl Into<String>,
) -> Result<()> {
    let path = worker_status_path(&args.output_dir, range.first_player, range.last_player());
    write_worker_status(
        &path,
        &WorkerStatus {
            status: kind,
            first_player: range.first_player,
            last_player: range.last_player(),
            player_count: range.player_count,
            message: message.into(),
            updated_at_unix_s: now_unix_s(),
        },
    )
}

struct PendingReceipt {
    player_id: u16,
    command_id: u64,
    action: String,
    receipt_name: &'static str,
    rtt: Duration,
    result: Result<(), String>,
}

/// Fan-out: register every reducer callback first, then await all receipts so
/// 500-player load is concurrent across seats/shards rather than sequential.
fn fanout_then_await<I, F>(items: I, register: F, timeout: Duration) -> Result<Vec<PendingReceipt>>
where
    I: IntoIterator,
    F: Fn(
        I::Item,
        mpsc::Sender<(u16, u64, String, &'static str, Instant, Result<(), String>)>,
    ) -> Result<()>,
{
    let (tx, rx) = mpsc::channel();
    let mut expected = 0_usize;
    for item in items {
        register(item, tx.clone())?;
        expected += 1;
    }
    drop(tx);
    let deadline = Instant::now() + timeout;
    let mut out = Vec::with_capacity(expected);
    while out.len() < expected {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            bail!(
                "timed out waiting for {} concurrent reducer callbacks (got {})",
                expected,
                out.len()
            );
        }
        let (player_id, command_id, action, receipt_name, started, result) = rx
            .recv_timeout(remaining)
            .context("recv concurrent reducer callback")?;
        out.push(PendingReceipt {
            player_id,
            command_id,
            action,
            receipt_name,
            rtt: started.elapsed(),
            result,
        });
    }
    Ok(out)
}

#[allow(clippy::too_many_lines, clippy::needless_pass_by_value)]
pub fn run(args: WorkerArgs) -> Result<()> {
    let range = PlayerRange::new(args.first_player, args.player_count)?;
    if range.last_player() > args.match_players {
        bail!(
            "worker range {}-{} exceeds match_players {}",
            range.first_player,
            range.last_player(),
            args.match_players
        );
    }
    let schedule = resolve_schedule(&args)?;
    std::fs::create_dir_all(&args.output_dir)
        .with_context(|| format!("create output dir {}", args.output_dir.display()))?;

    let fail = |args: &WorkerArgs, range: PlayerRange, error: anyhow::Error| -> Result<()> {
        let _ = emit_status(args, range, WorkerStatusKind::Failure, error.to_string());
        Err(error)
    };
    if let Err(error) = validate_command_spread_for_schedule(args.command_spread, schedule) {
        return fail(&args, range, error);
    }

    let connect_timeout = Duration::from_secs(args.connect_timeout_secs);
    let command_timeout = Duration::from_secs(args.command_timeout_secs);
    let ready_deadline = Instant::now() + Duration::from_secs(args.ready_timeout_secs);

    let observer = match Client::connect(
        format!(
            "worker-observer-{}-{}",
            range.first_player,
            range.last_player()
        ),
        &args.host,
        &args.database,
        &args.token_dir.join(format!(
            "worker-observer-{}-{}.token",
            range.first_player,
            range.last_player()
        )),
        &worker_observer_queries(),
        connect_timeout,
    ) {
        Ok(client) => client,
        Err(error) => return fail(&args, range, error),
    };

    // Prefer authoritative locked config over a shared ready.marker so workers
    // can run on different hosts. Optional marker remains a local convenience.
    while Instant::now() < ready_deadline {
        if args.require_shared_ready_marker {
            let marker = crate::common::readiness_marker_path(&args.output_dir);
            if !marker.exists() {
                std::thread::sleep(Duration::from_millis(50));
                continue;
            }
        }
        match observer.config() {
            Ok(config)
                if config.lobby_configuration_locked
                    && config.player_count == args.match_players =>
            {
                break;
            }
            Ok(_) | Err(_) => {
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
    let config = match observer.config() {
        Ok(config)
            if config.lobby_configuration_locked && config.player_count == args.match_players =>
        {
            config
        }
        Ok(config) => {
            return fail(
                &args,
                range,
                anyhow::anyhow!(
                    "timed out waiting for locked match_config (locked={}, players={})",
                    config.lobby_configuration_locked,
                    config.player_count
                ),
            );
        }
        Err(error) => return fail(&args, range, error),
    };

    let mut log = match WorkerLog::create(&args.output_dir, range.first_player, range.last_player())
    {
        Ok(log) => log,
        Err(error) => return fail(&args, range, error),
    };

    let mut commanders = BTreeMap::new();
    for player_id in range.iter() {
        let spawn_cell = match observer.spawn_cell(player_id) {
            Ok(cell) => cell,
            Err(error) => return fail(&args, range, error),
        };
        let width = config.map_width.max(1);
        let chunk_size = config.chunk_size.max(1);
        let column = i32::try_from(spawn_cell % u32::from(width)).unwrap_or(0);
        let row = i32::try_from(spawn_cell / u32::from(width)).unwrap_or(0);
        let size = i32::from(chunk_size);
        let spawn_chunk_q = i16::try_from(column.div_euclid(size)).unwrap_or(0);
        let spawn_chunk_r = i16::try_from(row.div_euclid(size)).unwrap_or(0);
        let queries = worker_command_queries(
            player_id,
            args.subscription_mode,
            args.match_players,
            spawn_chunk_q,
            spawn_chunk_r,
            INTEREST_CHUNK_RADIUS,
        );
        let mode_detail = subscription_mode_detail(args.subscription_mode, args.match_players);
        let client = match Client::connect(
            format!("player-{player_id}"),
            &args.host,
            &args.database,
            &args.token_dir.join(format!("player-{player_id}.token")),
            &queries,
            connect_timeout,
        ) {
            Ok(client) => client,
            Err(error) => return fail(&args, range, error),
        };
        let join_rtt = match client.join(player_id, command_timeout) {
            Ok(rtt) => rtt,
            Err(error) => return fail(&args, range, error),
        };
        let _ = log.write_event(&serde_json::json!({
            "event": "join",
            "player_id": player_id,
            "ok": true,
            "rtt_ms": join_rtt.as_secs_f64() * 1_000.0,
            "subscription_mode": args.subscription_mode.label(),
            "subscription_mode_detail": mode_detail,
        }));
        println!(
            "worker {}-{}: player {player_id} joined in {join_rtt:.2?}",
            range.first_player,
            range.last_player()
        );
        commanders.insert(player_id, client);
    }

    if let Err(error) = emit_status(
        &args,
        range,
        WorkerStatusKind::Ready,
        format!(
            "joined {} players; mode={}",
            range.player_count,
            args.subscription_mode.label()
        ),
    ) {
        return fail(&args, range, error);
    }

    let join_deadline = Instant::now() + Duration::from_secs(args.ready_timeout_secs);
    while match observer.phase() {
        Ok(phase) => phase != MatchPhase::Running,
        Err(error) => return fail(&args, range, error),
    } {
        if Instant::now() >= join_deadline {
            return fail(
                &args,
                range,
                anyhow::anyhow!("match never entered running phase"),
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    // Setup after Running; must finish before the shared warmup epoch ends.
    let setup_deadline_step = args.warmup_steps;
    let mob_timeout = command_timeout.saturating_mul(range.player_count.max(1).into());
    let pending = match fanout_then_await(
        range.iter().collect::<Vec<_>>(),
        |player_id, tx| {
            let command = deterministic_command_id(player_id, CommandKind::Mobilization, 1);
            let client = commanders.get(&player_id).context("missing commander")?;
            let started = Instant::now();
            client
                .conn
                .reducers
                .set_mobilization_target_then(command, args.mobilization_bps, move |_, result| {
                    let mapped = result
                        .map_err(|error| error.to_string())
                        .and_then(|inner| inner.map_err(|error| error.clone()));
                    let _ = tx.send((
                        player_id,
                        command,
                        format!("mobilization for player {player_id}"),
                        "set_mobilization_target",
                        started,
                        mapped,
                    ));
                })
                .map_err(|error| anyhow::anyhow!(error.to_string()))?;
            Ok(())
        },
        mob_timeout,
    ) {
        Ok(pending) => pending,
        Err(error) => return fail(&args, range, error),
    };
    for item in pending {
        if let Err(error) = item.result {
            return fail(&args, range, anyhow::anyhow!(error));
        }
        let Some(client) = commanders.get(&item.player_id) else {
            return fail(&args, range, anyhow::anyhow!("missing commander"));
        };
        if let Err(error) = client.require_receipt(
            item.player_id,
            &item.action,
            item.command_id,
            item.receipt_name,
            command_timeout,
        ) {
            return fail(&args, range, error);
        }
        let _ = log.write_event(&serde_json::json!({
            "event": "mobilization",
            "player_id": item.player_id,
            "command_id": item.command_id,
            "ok": true,
            "rtt_ms": item.rtt.as_secs_f64() * 1_000.0,
        }));
    }
    let setup_step = match observer.current_step() {
        Ok(step) => step,
        Err(error) => return fail(&args, range, error),
    };
    if setup_step >= setup_deadline_step && setup_deadline_step > 0 {
        return fail(
            &args,
            range,
            anyhow::anyhow!(
                "setup missed shared warmup epoch (step {setup_step} >= warmup {})",
                args.warmup_steps
            ),
        );
    }

    let spawns = match range
        .iter()
        .map(|player_id| observer.spawn_cell(player_id).map(|cell| (player_id, cell)))
        .collect::<Result<BTreeMap<_, _>>>()
    {
        Ok(spawns) => spawns,
        Err(error) => return fail(&args, range, error),
    };

    let mut last_expand_wave = u64::MAX;
    let mut pending_expand: BTreeSet<u16> = BTreeSet::new();
    let mut pending_rebalance: Option<BTreeSet<u16>> = None;
    let mut pending_attack: Option<BTreeSet<u16>> = None;
    let mut front_rebalance_attempted = 0_u64;
    let mut front_rebalance_accepted = 0_u64;
    let mut front_rebalance_skipped = 0_u64;
    let mut expand_seq = 1_u32;
    let started = Instant::now();
    let budget = Duration::from_millis(
        args.warmup_steps
            .saturating_add(schedule.total_steps())
            .saturating_mul(u64::from(config.logical_step_ms.max(1)))
            .saturating_mul(4)
            .saturating_add(120_000),
    );

    loop {
        if started.elapsed() > budget {
            return fail(
                &args,
                range,
                anyhow::anyhow!("worker exceeded phase budget"),
            );
        }
        let step = match observer.current_step() {
            Ok(step) => step,
            Err(error) => return fail(&args, range, error),
        };
        let progress = PhaseSchedule::phase_progress(step, args.warmup_steps);
        let phase = if PhaseSchedule::in_warmup(step, args.warmup_steps) {
            ScenarioPhase::Expand
        } else {
            schedule.phase_at(progress)
        };
        if !PhaseSchedule::in_warmup(step, args.warmup_steps) {
            let completed = match observer.phase() {
                Ok(MatchPhase::Completed) => true,
                Ok(_) => false,
                Err(error) => return fail(&args, range, error),
            };
            if phase == ScenarioPhase::Done || completed {
                break;
            }
        }
        if PhaseSchedule::in_warmup(step, args.warmup_steps) {
            std::thread::sleep(Duration::from_millis(15));
            continue;
        }

        if phase == ScenarioPhase::Expand {
            let wave = progress / schedule.reexpand_steps;
            if wave != last_expand_wave {
                // Prior wave must have drained every seat exactly once.
                if last_expand_wave != u64::MAX && !pending_expand.is_empty() {
                    return fail(
                        &args,
                        range,
                        anyhow::anyhow!(
                            "expand wave {last_expand_wave} ended with pending players {pending_expand:?}"
                        ),
                    );
                }
                last_expand_wave = wave;
                pending_expand = range.iter().collect();
                expand_seq = expand_seq.saturating_add(1);
            }
            let players = due_players_from_pending(&pending_expand, step, args.command_spread);
            if !players.is_empty() {
                let seq = expand_seq;
                let batch_timeout = command_timeout
                    .saturating_mul(u32::try_from(players.len().max(1)).unwrap_or(u32::MAX));
                let pending = match fanout_then_await(
                    players.clone(),
                    |player_id, tx| {
                        let command = deterministic_command_id(player_id, CommandKind::Expand, seq);
                        let spawn = *spawns.get(&player_id).context("spawn missing")?;
                        let opposing_index = (u32::from(player_id) - 1
                            + u32::from(args.match_players) / 2)
                            % u32::from(args.match_players)
                            + 1;
                        let opposing_spawn = observer
                            .spawn_cell(u16::try_from(opposing_index).context("opposing id")?)
                            .unwrap_or(spawn);
                        let focus = observer.neutral_focus(opposing_spawn).with_context(|| {
                            format!("player {player_id} has no neutral expansion focus")
                        })?;
                        let client = commanders.get(&player_id).context("commander")?;
                        let started = Instant::now();
                        client
                            .conn
                            .reducers
                            .issue_expand_clusters_then(
                                command,
                                vec![spawn],
                                focus,
                                args.command_share_bps,
                                move |_, result| {
                                    let mapped = result
                                        .map_err(|error| error.to_string())
                                        .and_then(|inner| inner.map_err(|error| error.clone()));
                                    let _ = tx.send((
                                        player_id,
                                        command,
                                        format!("expansion for player {player_id}"),
                                        "issue_expand_clusters",
                                        started,
                                        mapped,
                                    ));
                                },
                            )
                            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
                        Ok(())
                    },
                    batch_timeout,
                ) {
                    Ok(pending) => pending,
                    Err(error) => return fail(&args, range, error),
                };
                for item in pending {
                    if let Err(error) = require_reducer_success(&item.action, item.result) {
                        let _ = log.write_event(&serde_json::json!({
                            "event": "expand",
                            "player_id": item.player_id,
                            "command_id": item.command_id,
                            "ok": false,
                            "error": error.to_string(),
                            "rtt_ms": item.rtt.as_secs_f64() * 1_000.0,
                        }));
                        return fail(&args, range, error);
                    }
                    let client = commanders.get(&item.player_id).context("commander")?;
                    if let Err(error) = client.require_receipt(
                        item.player_id,
                        &item.action,
                        item.command_id,
                        item.receipt_name,
                        command_timeout,
                    ) {
                        return fail(&args, range, error);
                    }
                    pending_expand.remove(&item.player_id);
                    let _ = log.write_event(&serde_json::json!({
                        "event": "expand",
                        "player_id": item.player_id,
                        "command_id": item.command_id,
                        "ok": true,
                        "rtt_ms": item.rtt.as_secs_f64() * 1_000.0,
                    }));
                }
            }
        }

        if phase == ScenarioPhase::Rebalance {
            if pending_rebalance.is_none() {
                pending_rebalance = Some(range.iter().collect());
            }
            if let Some(pending_set) = pending_rebalance.as_mut() {
                let players = due_players_from_pending(pending_set, step, args.command_spread);
                if !players.is_empty() {
                    let max_elevation_step = config.max_elevation_step;
                    // One observer snapshot for the whole due batch keeps derivation
                    // deterministic across seats without re-walking tables per player.
                    let map_cells = observer.map_cells();
                    // Partition due seats into skippable topology vs issuable commands.
                    let mut ready = Vec::new();
                    for player_id in players {
                        match crate::front_rebalance::plan_front_rebalance_for_player(
                            &map_cells,
                            player_id,
                            max_elevation_step,
                        ) {
                            FrontRebalancePlan::Skipped(reason) => {
                                pending_set.remove(&player_id);
                                front_rebalance_skipped = front_rebalance_skipped.saturating_add(1);
                                let _ = log.write_event(&serde_json::json!({
                                    "event": "front_rebalance",
                                    "player_id": player_id,
                                    "ok": true,
                                    "skipped": true,
                                    "reason": reason,
                                }));
                                println!(
                                    "worker {}-{}: skip front_rebalance player {player_id}: {reason}",
                                    range.first_player,
                                    range.last_player()
                                );
                            }
                            FrontRebalancePlan::Ready(command) => {
                                ready.push((player_id, command));
                            }
                        }
                    }
                    if !ready.is_empty() {
                        let batch_timeout = command_timeout
                            .saturating_mul(u32::try_from(ready.len().max(1)).unwrap_or(u32::MAX));
                        let pending = match fanout_then_await(
                            ready,
                            |(player_id, plan), tx| {
                                let command =
                                    deterministic_command_id(player_id, CommandKind::Rebalance, 1);
                                let client = commanders.get(&player_id).context("commander")?;
                                let started = Instant::now();
                                let component_cells = plan.component_cells.clone();
                                let source_front_seed = plan.source_front_seed;
                                let target_front_seed = plan.target_front_seed;
                                client
                                    .conn
                                    .reducers
                                    .issue_front_rebalance_then(
                                        command,
                                        component_cells,
                                        source_front_seed,
                                        target_front_seed,
                                        args.command_share_bps,
                                        Vec::new(),
                                        move |_, result| {
                                            let mapped =
                                                result.map_err(|error| error.to_string()).and_then(
                                                    |inner| inner.map_err(|error| error.clone()),
                                                );
                                            let _ = tx.send((
                                                player_id,
                                                command,
                                                format!("front rebalance for player {player_id}"),
                                                "issue_front_rebalance",
                                                started,
                                                mapped,
                                            ));
                                        },
                                    )
                                    .map_err(|error| anyhow::anyhow!(error.to_string()))?;
                                Ok(())
                            },
                            batch_timeout,
                        ) {
                            Ok(pending) => pending,
                            Err(error) => return fail(&args, range, error),
                        };
                        for item in pending {
                            front_rebalance_attempted = front_rebalance_attempted.saturating_add(1);
                            if let Err(error) = require_reducer_success(&item.action, item.result) {
                                let _ = log.write_event(&serde_json::json!({
                                    "event": "front_rebalance",
                                    "player_id": item.player_id,
                                    "command_id": item.command_id,
                                    "ok": false,
                                    "skipped": false,
                                    "error": error.to_string(),
                                    "rtt_ms": item.rtt.as_secs_f64() * 1_000.0,
                                }));
                                return fail(&args, range, error);
                            }
                            let client = commanders.get(&item.player_id).context("commander")?;
                            if let Err(error) = client.require_receipt(
                                item.player_id,
                                &item.action,
                                item.command_id,
                                item.receipt_name,
                                command_timeout,
                            ) {
                                return fail(&args, range, error);
                            }
                            pending_set.remove(&item.player_id);
                            front_rebalance_accepted = front_rebalance_accepted.saturating_add(1);
                            let _ = log.write_event(&serde_json::json!({
                                "event": "front_rebalance",
                                "player_id": item.player_id,
                                "command_id": item.command_id,
                                "ok": true,
                                "skipped": false,
                                "rtt_ms": item.rtt.as_secs_f64() * 1_000.0,
                            }));
                        }
                    }
                }
            }
        }

        if phase == ScenarioPhase::Attack {
            if pending_attack.is_none() {
                pending_attack = Some(range.iter().collect());
            }
            if let Some(pending_set) = pending_attack.as_mut() {
                let players = due_players_from_pending(pending_set, step, args.command_spread);
                if !players.is_empty() {
                    let fronts = match observer
                        .attack_fronts(args.match_players, config.max_elevation_step)
                    {
                        Ok(fronts) => fronts,
                        Err(error) => return fail(&args, range, error),
                    };
                    let batch_timeout = command_timeout
                        .saturating_mul(u32::try_from(players.len().max(1)).unwrap_or(u32::MAX));
                    let pending = match fanout_then_await(
                        players,
                        |player_id, tx| {
                            let front =
                                fronts.get(usize::from(player_id - 1)).with_context(|| {
                                    format!("missing attack front for player {player_id}")
                                })?;
                            let command =
                                deterministic_command_id(player_id, CommandKind::Attack, 1);
                            let client = commanders.get(&player_id).context("commander")?;
                            let started = Instant::now();
                            let source = front.source;
                            let target = front.target;
                            client
                                .conn
                                .reducers
                                .issue_attack_clusters_then(
                                    command,
                                    vec![source],
                                    vec![target],
                                    args.command_share_bps,
                                    move |_, result| {
                                        let mapped = result
                                            .map_err(|error| error.to_string())
                                            .and_then(|inner| inner.map_err(|error| error.clone()));
                                        let _ = tx.send((
                                            player_id,
                                            command,
                                            format!("attack for player {player_id}"),
                                            "issue_attack_clusters",
                                            started,
                                            mapped,
                                        ));
                                    },
                                )
                                .map_err(|error| anyhow::anyhow!(error.to_string()))?;
                            Ok(())
                        },
                        batch_timeout,
                    ) {
                        Ok(pending) => pending,
                        Err(error) => return fail(&args, range, error),
                    };
                    for item in pending {
                        if let Err(error) = require_reducer_success(&item.action, item.result) {
                            return fail(&args, range, error);
                        }
                        let client = commanders.get(&item.player_id).context("commander")?;
                        if let Err(error) = client.require_receipt(
                            item.player_id,
                            &item.action,
                            item.command_id,
                            item.receipt_name,
                            command_timeout,
                        ) {
                            return fail(&args, range, error);
                        }
                        pending_set.remove(&item.player_id);
                        let _ = log.write_event(&serde_json::json!({
                            "event": "attack",
                            "player_id": item.player_id,
                            "command_id": item.command_id,
                            "ok": true,
                            "rtt_ms": item.rtt.as_secs_f64() * 1_000.0,
                        }));
                    }
                }
            }
        }

        std::thread::sleep(Duration::from_millis(15));
    }

    if !pending_expand.is_empty() {
        return fail(
            &args,
            range,
            anyhow::anyhow!("expand wave left pending players {pending_expand:?}"),
        );
    }
    if pending_rebalance
        .as_ref()
        .is_some_and(|pending| !pending.is_empty())
    {
        return fail(
            &args,
            range,
            anyhow::anyhow!("front-rebalance phase left pending players {pending_rebalance:?}"),
        );
    }
    if pending_attack
        .as_ref()
        .is_some_and(|pending| !pending.is_empty())
    {
        return fail(
            &args,
            range,
            anyhow::anyhow!("attack phase left pending players {pending_attack:?}"),
        );
    }

    let _ = log.write_event(&serde_json::json!({
        "event": "front_rebalance_summary",
        "first_player": range.first_player,
        "last_player": range.last_player(),
        "attempted": front_rebalance_attempted,
        "accepted": front_rebalance_accepted,
        "skipped": front_rebalance_skipped,
    }));
    println!(
        "worker {}-{} front_rebalance attempted={} accepted={} skipped={}",
        range.first_player,
        range.last_player(),
        front_rebalance_attempted,
        front_rebalance_accepted,
        front_rebalance_skipped
    );

    let _ = log.write_event(&serde_json::json!({
        "event": "complete",
        "first_player": range.first_player,
        "last_player": range.last_player(),
        "ok": true,
        "front_rebalance_attempted": front_rebalance_attempted,
        "front_rebalance_accepted": front_rebalance_accepted,
        "front_rebalance_skipped": front_rebalance_skipped,
    }));
    emit_status(&args, range, WorkerStatusKind::Complete, "worker complete")?;
    println!(
        "worker {}-{} complete; log {}",
        range.first_player,
        range.last_player(),
        log.path().display()
    );
    Ok(())
}
