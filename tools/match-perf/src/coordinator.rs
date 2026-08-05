//! Coordinator: configure match, full telemetry observer, timeline + summary.

use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use clap::Parser;
use match_bindings::MatchPhase;

use crate::client::Client;
use crate::common::{
    DEFAULT_LOGICAL_STEP_MS, PhaseSchedule, PresetArg, ScenarioPhase, SubscriptionMode,
    WorkerStatusKind, create_run_dir, default_run_dir, player_shards, read_worker_status,
    readiness_marker_path, worker_status_path,
};
use crate::output::{
    PlayersWriter, ShardMeta, TimelineWriter, build_metadata, write_metadata, write_summary,
};
use crate::queries::coordinator_observer_queries;
use crate::stats::ControlStats;

#[derive(Debug, Parser)]
pub struct CoordinatorArgs {
    #[arg(long, default_value = "http://127.0.0.1:3000")]
    pub host: String,
    #[arg(long, default_value = "of-match-perf")]
    pub database: String,
    #[arg(long, default_value = ".match-perf-tokens")]
    pub token_dir: PathBuf,
    #[arg(long, default_value_t = 2, value_parser = clap::value_parser!(u16).range(2..=500))]
    pub players: u16,
    #[arg(long, value_enum, default_value_t = PresetArg::Dev)]
    pub preset: PresetArg,
    /// Non-overwriting run directory for all artifacts.
    #[arg(long)]
    pub output_dir: Option<PathBuf>,
    /// Logical steps of full-share expansion.
    #[arg(long)]
    pub expand_steps: Option<u64>,
    /// Logical steps of Center-policy measurement.
    #[arg(long)]
    pub policy_steps: Option<u64>,
    /// Logical steps of mutual attack measurement (0 skips).
    #[arg(long)]
    pub attack_steps: Option<u64>,
    /// Re-issue expansion waves every N logical steps while expanding.
    #[arg(long)]
    pub reexpand_steps: Option<u64>,
    /// Optional wall-second alias for `expand_steps` at nominal 250 ms cadence.
    #[arg(long)]
    pub expand_secs: Option<u64>,
    #[arg(long)]
    pub policy_secs: Option<u64>,
    #[arg(long)]
    pub attack_secs: Option<u64>,
    #[arg(long)]
    pub reexpand_secs: Option<u64>,
    /// Shared absolute warmup steps before phase progress begins at step 0.
    /// Default 120 gives multi-host joins/setup a realistic window; remote users
    /// can raise it further when workers start slowly.
    #[arg(long, default_value_t = 120)]
    pub warmup_steps: u64,
    #[arg(long, value_enum, default_value_t = SubscriptionMode::FullClient)]
    pub subscription_mode: SubscriptionMode,
    #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u32).range(1..=500))]
    pub command_spread: u32,
    #[arg(long, default_value_t = 6_000, value_parser = clap::value_parser!(u32).range(0..=10_000))]
    pub mobilization_bps: u32,
    #[arg(long, default_value_t = 10_000, value_parser = clap::value_parser!(u32).range(1..=10_000))]
    pub command_share_bps: u32,
    /// Shard size recorded in metadata (actual workers may run remotely).
    #[arg(long, default_value_t = 32)]
    pub shard_size: u16,
    /// Emit players.csv snapshots every N observed logical steps (1 = every sample).
    #[arg(long, default_value_t = 1)]
    pub players_snapshot_every_steps: u64,
    #[arg(long, default_value_t = 120)]
    pub connect_timeout_secs: u64,
    #[arg(long, default_value_t = 600)]
    pub join_timeout_secs: u64,
    /// Extra wall seconds after the scheduled phase budget before failing.
    #[arg(long, default_value_t = 120)]
    pub idle_timeout_secs: u64,
    /// When set, wait for worker status JSON files in the run directory before
    /// writing the final summary. Requires a shared filesystem with workers.
    /// Remote/no-shared-FS runs should leave this off; phase clock remains DB-based.
    #[arg(long, default_value_t = false)]
    pub wait_for_worker_status: bool,
}

pub fn resolve_schedule(args: &CoordinatorArgs) -> Result<PhaseSchedule> {
    let has_steps = args.expand_steps.is_some()
        || args.policy_steps.is_some()
        || args.attack_steps.is_some()
        || args.reexpand_steps.is_some();
    let has_secs = args.expand_secs.is_some()
        || args.policy_secs.is_some()
        || args.attack_secs.is_some()
        || args.reexpand_secs.is_some();
    if has_steps {
        PhaseSchedule::from_steps(
            args.expand_steps.unwrap_or(600),
            args.policy_steps.unwrap_or(720),
            args.attack_steps.unwrap_or(360),
            args.reexpand_steps.unwrap_or(180),
        )
    } else if has_secs {
        PhaseSchedule::from_secs(
            args.expand_secs.unwrap_or(150),
            args.policy_secs.unwrap_or(180),
            args.attack_secs.unwrap_or(90),
            args.reexpand_secs.unwrap_or(45),
            DEFAULT_LOGICAL_STEP_MS,
        )
    } else {
        // Defaults prefer logical steps (≈150/180/90s at 250 ms).
        PhaseSchedule::from_steps(600, 720, 360, 180)
    }
}

fn collect_worker_failures(
    run_dir: &std::path::Path,
    shards: &[ShardMeta],
    require_terminal: bool,
) -> Result<(u64, Vec<String>)> {
    let mut failures = 0_u64;
    let mut messages = Vec::new();
    for shard in shards {
        let path = worker_status_path(run_dir, shard.first_player, shard.last_player);
        if let Some(status) = read_worker_status(&path)? {
            match status.status {
                WorkerStatusKind::Complete => {}
                WorkerStatusKind::Failure => {
                    failures += 1;
                    messages.push(format!(
                        "worker {}-{}: {}",
                        status.first_player, status.last_player, status.message
                    ));
                }
                WorkerStatusKind::Ready => {
                    if require_terminal {
                        failures += 1;
                        messages.push(format!(
                            "worker {}-{} still ready (did not complete)",
                            status.first_player, status.last_player
                        ));
                    }
                }
            }
        } else if require_terminal {
            failures += 1;
            messages.push(format!(
                "worker {}-{} missing status file",
                shard.first_player, shard.last_player
            ));
        }
    }
    Ok((failures, messages))
}

fn poll_worker_failures_so_far(
    run_dir: &std::path::Path,
    shards: &[ShardMeta],
) -> Result<(u64, Vec<String>)> {
    collect_worker_failures(run_dir, shards, false)
}

fn write_terminal_summary(
    timeline: TimelineWriter,
    summary_path: &std::path::Path,
    failures: u64,
    early_completion: bool,
) -> Result<crate::output::RunSummary> {
    let summary = timeline.into_summary(failures, early_completion);
    write_summary(summary_path, &summary)?;
    Ok(summary)
}

#[allow(clippy::too_many_lines, clippy::needless_pass_by_value)]
pub fn run(args: CoordinatorArgs) -> Result<()> {
    let schedule = resolve_schedule(&args)?;
    let run_dir = args
        .output_dir
        .clone()
        .unwrap_or_else(|| default_run_dir(&args.database, args.preset, args.players));
    create_run_dir(&run_dir)?;

    let timeline_path = run_dir.join("timeline.csv");
    let players_path = run_dir.join("players.csv");
    let metadata_path = run_dir.join("metadata.json");
    let summary_path = run_dir.join("summary.json");
    let marker_path = readiness_marker_path(&run_dir);

    let mut timeline = TimelineWriter::create(&timeline_path)?;
    let mut players_out = PlayersWriter::create(&players_path)?;

    let timeout = Duration::from_secs(args.connect_timeout_secs);
    let observer = Client::connect(
        "coordinator-observer",
        &args.host,
        &args.database,
        &args.token_dir.join("coordinator-observer.token"),
        &coordinator_observer_queries(),
        timeout,
    )?;
    println!("coordinator connected to {} / {}", args.host, args.database);

    let configure_rtt = observer.configure(args.preset.remote(), args.players, timeout)?;
    println!(
        "configured {} for {} players in {configure_rtt:.2?}",
        args.preset.label(),
        args.players
    );

    // Optional local convenience marker. Workers prefer locked match_config.
    fs::write(
        &marker_path,
        format!(
            "ready\nhost={}\ndatabase={}\nplayers={}\npreset={}\nwarmup_steps={}\nsubscription_mode={}\n",
            args.host,
            args.database,
            args.players,
            args.preset.label(),
            args.warmup_steps,
            args.subscription_mode.label(),
        ),
    )
    .with_context(|| format!("write readiness marker {}", marker_path.display()))?;
    println!("readiness marker written at {}", marker_path.display());

    let shards = player_shards(args.players, args.shard_size)?
        .into_iter()
        .map(|range| ShardMeta {
            first_player: range.first_player,
            last_player: range.last_player(),
            player_count: range.player_count,
        })
        .collect::<Vec<_>>();

    let join_deadline = Instant::now() + Duration::from_secs(args.join_timeout_secs);
    while observer.phase()? == MatchPhase::Lobby {
        if args.wait_for_worker_status {
            let (count, messages) = poll_worker_failures_so_far(&run_dir, &shards)?;
            if count > 0 {
                for message in &messages {
                    eprintln!("coordinator: {message}");
                }
                let summary = write_terminal_summary(timeline, &summary_path, count, false)?;
                println!(
                    "coordinator abort during lobby; summary={}, failures={}",
                    summary_path.display(),
                    summary.failures
                );
                bail!("coordinator finished with {count} worker failure(s) during lobby");
            }
        }
        if Instant::now() >= join_deadline {
            let failures = if args.wait_for_worker_status {
                let (count, messages) = collect_worker_failures(&run_dir, &shards, true)?;
                for message in messages {
                    eprintln!("coordinator: {message}");
                }
                count.max(1)
            } else {
                1
            };
            let _ = write_terminal_summary(timeline, &summary_path, failures, false);
            bail!(
                "timed out waiting for workers to fill {} slots (claimed {})",
                args.players,
                observer.claimed_players()
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    while observer.phase()? != MatchPhase::Running {
        if args.wait_for_worker_status {
            let (count, messages) = poll_worker_failures_so_far(&run_dir, &shards)?;
            if count > 0 {
                for message in &messages {
                    eprintln!("coordinator: {message}");
                }
                let _ = write_terminal_summary(timeline, &summary_path, count, false);
                bail!("coordinator finished with {count} worker failure(s) before Running");
            }
        }
        if Instant::now() >= join_deadline {
            let _ = write_terminal_summary(timeline, &summary_path, 1, false);
            bail!("match never entered the running phase");
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    let config = observer.config()?;
    let generated_at_unix_s = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let metadata = build_metadata(
        &args.host,
        &args.database,
        args.preset,
        args.players,
        config.map_width,
        config.map_height,
        config.map_hash,
        config.logical_step_ms,
        schedule,
        args.warmup_steps,
        args.subscription_mode.label(),
        args.command_spread,
        args.mobilization_bps,
        args.command_share_bps,
        &shards,
        args.players_snapshot_every_steps.max(1),
        generated_at_unix_s,
    );
    write_metadata(&metadata_path, &metadata)?;
    println!(
        "match running; sampling timeline into {} (warmup_steps={})",
        timeline_path.display(),
        args.warmup_steps
    );

    let started = Instant::now();
    // Shared absolute epoch: logical_step starts at 0 when Running. Do not
    // snapshot a per-process start step.
    let mut last_step = observer.current_step()?;
    let mut last_change = Instant::now();
    let mut samples_since_players = 0_u64;
    let mut early_completion = false;
    let idle_limit = Duration::from_secs(args.idle_timeout_secs);
    let phase_budget = Duration::from_millis(
        args.warmup_steps
            .saturating_add(schedule.total_steps())
            .saturating_mul(u64::from(config.logical_step_ms.max(1)))
            .saturating_mul(4)
            .saturating_add(args.idle_timeout_secs.saturating_mul(1_000)),
    );

    let mut aborted_for_worker_failure = false;
    loop {
        if args.wait_for_worker_status {
            let (count, messages) = poll_worker_failures_so_far(&run_dir, &shards)?;
            if count > 0 {
                for message in &messages {
                    eprintln!("coordinator: {message}");
                }
                aborted_for_worker_failure = true;
                break;
            }
        }
        if started.elapsed() > phase_budget {
            let _ = write_terminal_summary(timeline, &summary_path, 1, early_completion);
            bail!(
                "coordinator idle/phase budget exceeded after {:?}",
                started.elapsed()
            );
        }
        let step = observer.current_step()?;
        let warmup = PhaseSchedule::in_warmup(step, args.warmup_steps);
        let progress = PhaseSchedule::phase_progress(step, args.warmup_steps);
        let phase = if warmup {
            ScenarioPhase::Expand
        } else {
            schedule.phase_at(progress)
        };
        if !warmup && phase == ScenarioPhase::Done {
            break;
        }
        if matches!(observer.phase()?, MatchPhase::Completed) {
            early_completion = true;
        }

        if step != last_step {
            let gap = last_change.elapsed().as_secs_f64() * 1_000.0;
            let step_delta = step
                .checked_sub(last_step)
                .context("authoritative logical step moved backwards")?;
            let controls = observer.controlled_counts();
            let stats =
                ControlStats::from_counts(controls.iter().map(|(_, count)| *count).collect());
            let labeled_phase = if warmup { ScenarioPhase::Expand } else { phase };
            timeline.write_row(
                started.elapsed().as_secs_f64(),
                step,
                step_delta,
                gap,
                labeled_phase,
                warmup,
                observer.packet_count(),
                observer.active_order_count(),
                observer.front_count(),
                stats,
            )?;
            samples_since_players += 1;
            if samples_since_players >= args.players_snapshot_every_steps.max(1) {
                players_out.write_snapshot(step, &controls)?;
                samples_since_players = 0;
            }
            last_step = step;
            last_change = Instant::now();
            if early_completion && !warmup {
                break;
            }
        } else if last_change.elapsed() > idle_limit {
            let _ = write_terminal_summary(timeline, &summary_path, 1, early_completion);
            bail!("no logical-step advance for {idle_limit:?} (last step {last_step})");
        }
        std::thread::sleep(Duration::from_millis(15));
    }

    // Terminal Done/Completed sample before exit.
    {
        let step = observer.current_step().unwrap_or(last_step);
        let warmup = PhaseSchedule::in_warmup(step, args.warmup_steps);
        let progress = PhaseSchedule::phase_progress(step, args.warmup_steps);
        let phase = if early_completion {
            ScenarioPhase::Done
        } else if warmup {
            ScenarioPhase::Expand
        } else {
            schedule.phase_at(progress)
        };
        let controls = observer.controlled_counts();
        let stats = ControlStats::from_counts(controls.iter().map(|(_, count)| *count).collect());
        timeline.record_terminal_sample_if_needed(
            started.elapsed().as_secs_f64(),
            step,
            if phase == ScenarioPhase::Done || early_completion {
                ScenarioPhase::Done
            } else {
                phase
            },
            warmup,
            observer.packet_count(),
            observer.active_order_count(),
            observer.front_count(),
            stats,
        )?;
    }

    let mut failures = 0_u64;
    if args.wait_for_worker_status {
        let status_deadline = Instant::now() + Duration::from_secs(args.idle_timeout_secs.max(30));
        // When a worker already failed mid-run, still give peers a short window
        // to publish terminal status so the summary reflects the full tree.
        loop {
            let pending = shards.iter().any(|shard| {
                read_worker_status(&worker_status_path(
                    &run_dir,
                    shard.first_player,
                    shard.last_player,
                ))
                .ok()
                .flatten()
                .is_none_or(|status| matches!(status.status, WorkerStatusKind::Ready))
            });
            if !pending || Instant::now() >= status_deadline {
                break;
            }
            // If any failure is already visible and nothing is still Ready-only,
            // finish promptly so run-local's grace window can observe the summary.
            if aborted_for_worker_failure {
                let (count, _) = poll_worker_failures_so_far(&run_dir, &shards)?;
                if count > 0 && !pending {
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let (count, messages) = collect_worker_failures(&run_dir, &shards, true)?;
        failures = count;
        for message in messages {
            eprintln!("coordinator: {message}");
        }
    }

    let summary = write_terminal_summary(timeline, &summary_path, failures, early_completion)?;
    println!(
        "coordinator complete; timeline={}, summary={}, failures={}",
        timeline_path.display(),
        summary_path.display(),
        summary.failures
    );
    if summary.failures > 0 {
        bail!(
            "coordinator finished with {} worker failure(s)",
            summary.failures
        );
    }
    Ok(())
}
