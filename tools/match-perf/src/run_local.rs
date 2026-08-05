//! Local distributed-process launcher: coordinator + worker shards.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::Parser;

use crate::common::{PhaseSchedule, PresetArg, SubscriptionMode, default_run_dir, player_shards};
use crate::coordinator::{CoordinatorArgs, resolve_schedule};

/// Bounded window for the coordinator to poll worker status files and write
/// `summary.json` after a worker fails, before remaining children are killed.
const COORDINATOR_SUMMARY_GRACE: Duration = Duration::from_secs(45);

#[derive(Debug, Parser)]
pub struct RunLocalArgs {
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
    #[arg(long)]
    pub output_dir: Option<PathBuf>,
    /// Players per worker subprocess.
    #[arg(long, default_value_t = 32)]
    pub shard_size: u16,
    #[arg(long)]
    pub expand_steps: Option<u64>,
    #[arg(long)]
    pub policy_steps: Option<u64>,
    #[arg(long)]
    pub attack_steps: Option<u64>,
    #[arg(long)]
    pub reexpand_steps: Option<u64>,
    #[arg(long)]
    pub expand_secs: Option<u64>,
    #[arg(long)]
    pub policy_secs: Option<u64>,
    #[arg(long)]
    pub attack_secs: Option<u64>,
    #[arg(long)]
    pub reexpand_secs: Option<u64>,
    /// Shared absolute warmup before phase progress. Default 120 matches the
    /// coordinator/worker default so local multi-process joins finish setup.
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
    #[arg(long, default_value_t = 1)]
    pub players_snapshot_every_steps: u64,
    /// Overall wall-clock timeout for the local process tree.
    #[arg(long, default_value_t = 3_600)]
    pub timeout_secs: u64,
    /// Path to the match-perf binary. Defaults to the current executable.
    #[arg(long)]
    pub bin: Option<PathBuf>,
}

/// RAII guard: every early return / spawn / poll error kills already-started children.
struct ChildGuard {
    children: Vec<TrackedChild>,
}

struct TrackedChild {
    label: String,
    child: Child,
}

impl ChildGuard {
    fn new() -> Self {
        Self {
            children: Vec::new(),
        }
    }

    fn push(&mut self, label: impl Into<String>, child: Child) {
        self.children.push(TrackedChild {
            label: label.into(),
            child,
        });
    }

    fn is_empty(&self) -> bool {
        self.children.is_empty()
    }

    fn has_label(&self, label: &str) -> bool {
        self.children.iter().any(|child| child.label == label)
    }

    fn kill_all(&mut self) {
        for tracked in &mut self.children {
            let _ = tracked.child.kill();
            let _ = tracked.child.wait();
        }
        self.children.clear();
    }

    fn poll_exits(&mut self) -> Result<Vec<String>> {
        let mut failures = Vec::new();
        let mut idx = 0;
        while idx < self.children.len() {
            match self.children[idx].child.try_wait() {
                Ok(Some(status)) => {
                    let tracked = self.children.remove(idx);
                    if status.success() {
                        println!("{} exited successfully", tracked.label);
                    } else {
                        failures.push(format!("{} -> {status}", tracked.label));
                    }
                }
                Ok(None) => idx += 1,
                Err(error) => {
                    self.kill_all();
                    return Err(error).context("wait for child");
                }
            }
        }
        Ok(failures)
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        self.kill_all();
    }
}

#[allow(clippy::too_many_lines, clippy::needless_pass_by_value)]
pub fn run(args: RunLocalArgs) -> Result<()> {
    let schedule = resolve_schedule(&CoordinatorArgs {
        host: args.host.clone(),
        database: args.database.clone(),
        token_dir: args.token_dir.clone(),
        players: args.players,
        preset: args.preset,
        output_dir: args.output_dir.clone(),
        expand_steps: args.expand_steps,
        policy_steps: args.policy_steps,
        attack_steps: args.attack_steps,
        reexpand_steps: args.reexpand_steps,
        expand_secs: args.expand_secs,
        policy_secs: args.policy_secs,
        attack_secs: args.attack_secs,
        reexpand_secs: args.reexpand_secs,
        warmup_steps: args.warmup_steps,
        subscription_mode: args.subscription_mode,
        command_spread: args.command_spread,
        mobilization_bps: args.mobilization_bps,
        command_share_bps: args.command_share_bps,
        shard_size: args.shard_size,
        players_snapshot_every_steps: args.players_snapshot_every_steps,
        connect_timeout_secs: 120,
        join_timeout_secs: 600,
        idle_timeout_secs: 120,
        wait_for_worker_status: true,
    })?;

    let run_dir = args
        .output_dir
        .clone()
        .unwrap_or_else(|| default_run_dir(&args.database, args.preset, args.players));
    if run_dir.exists() {
        bail!(
            "run directory {} already exists; refusing to overwrite",
            run_dir.display()
        );
    }

    let bin = args
        .bin
        .clone()
        .or_else(|| std::env::current_exe().ok())
        .context("unable to locate match-perf binary")?;

    let shards = player_shards(args.players, args.shard_size)?;
    println!(
        "run-local: {} players across {} shards into {} (mode={}, warmup={})",
        args.players,
        shards.len(),
        run_dir.display(),
        args.subscription_mode.label(),
        args.warmup_steps
    );

    let mut guard = ChildGuard::new();

    let mut coordinator_cmd = base_command(&bin, "coordinator", &args, &schedule);
    coordinator_cmd
        .arg("--output-dir")
        .arg(&run_dir)
        .arg("--players")
        .arg(args.players.to_string())
        .arg("--shard-size")
        .arg(args.shard_size.to_string())
        .arg("--players-snapshot-every-steps")
        .arg(args.players_snapshot_every_steps.to_string())
        .arg("--wait-for-worker-status");

    let coordinator = coordinator_cmd
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .context("spawn coordinator")?;
    guard.push("coordinator", coordinator);

    // Workers poll locked match_config; still stagger spawn slightly so the
    // coordinator's configure_match wins the lobby race.
    std::thread::sleep(Duration::from_millis(200));

    for shard in &shards {
        let mut cmd = base_command(&bin, "worker", &args, &schedule);
        cmd.arg("--output-dir")
            .arg(&run_dir)
            .arg("--first-player")
            .arg(shard.first_player.to_string())
            .arg("--player-count")
            .arg(shard.player_count.to_string())
            .arg("--match-players")
            .arg(args.players.to_string());
        let child = match cmd
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()
        {
            Ok(child) => child,
            Err(error) => {
                guard.kill_all();
                return Err(error).with_context(|| {
                    format!(
                        "spawn worker {}-{}",
                        shard.first_player,
                        shard.last_player()
                    )
                });
            }
        };
        guard.push(
            format!("worker-{}-{}", shard.first_player, shard.last_player()),
            child,
        );
    }

    let deadline = Instant::now() + Duration::from_secs(args.timeout_secs);
    let mut failures = Vec::new();
    while !guard.is_empty() {
        if Instant::now() >= deadline {
            guard.kill_all();
            bail!(
                "run-local timed out after {}s; killed remaining processes",
                args.timeout_secs
            );
        }
        let exited = guard.poll_exits()?;
        if !exited.is_empty() {
            failures.extend(exited);
            // Prefer letting the coordinator finalize summary.json when it is
            // still alive so local worker failures always leave a terminal summary.
            if guard.has_label("coordinator") {
                let grace_deadline = Instant::now() + COORDINATOR_SUMMARY_GRACE;
                while Instant::now() < grace_deadline {
                    let more = guard.poll_exits()?;
                    failures.extend(more);
                    if !guard.has_label("coordinator") || run_dir.join("summary.json").exists() {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
            guard.kill_all();
            bail!("subprocess failures: {}", failures.join(", "));
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    println!("run-local complete; artifacts in {}", run_dir.display());
    Ok(())
}

fn base_command(
    bin: &Path,
    subcommand: &str,
    args: &RunLocalArgs,
    schedule: &PhaseSchedule,
) -> Command {
    let mut cmd = Command::new(bin);
    cmd.arg(subcommand)
        .arg("--host")
        .arg(&args.host)
        .arg("--database")
        .arg(&args.database)
        .arg("--token-dir")
        .arg(&args.token_dir)
        .arg("--preset")
        .arg(match args.preset {
            PresetArg::Dev => "dev",
            PresetArg::Playtest => "playtest",
            PresetArg::Validation => "validation",
        })
        .arg("--expand-steps")
        .arg(schedule.expand_steps.to_string())
        .arg("--policy-steps")
        .arg(schedule.policy_steps.to_string())
        .arg("--attack-steps")
        .arg(schedule.attack_steps.to_string())
        .arg("--reexpand-steps")
        .arg(schedule.reexpand_steps.to_string())
        .arg("--warmup-steps")
        .arg(args.warmup_steps.to_string())
        .arg("--subscription-mode")
        .arg(args.subscription_mode.label())
        .arg("--command-spread")
        .arg(args.command_spread.to_string())
        .arg("--mobilization-bps")
        .arg(args.mobilization_bps.to_string())
        .arg("--command-share-bps")
        .arg(args.command_share_bps.to_string());
    cmd
}
