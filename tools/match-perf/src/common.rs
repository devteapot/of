//! Shared CLI types, phase schedule, command IDs, and git metadata helpers.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use clap::ValueEnum;
use match_bindings::MapPreset;
use serde::{Deserialize, Serialize};

pub const SINGLETON_ID: u8 = 0;
pub const DEFAULT_LOGICAL_STEP_MS: u32 = 250;

#[derive(Clone, Copy, Debug, ValueEnum, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PresetArg {
    Dev,
    Playtest,
    Validation,
}

impl PresetArg {
    pub const fn remote(self) -> MapPreset {
        match self {
            Self::Dev => MapPreset::Dev64,
            Self::Playtest => MapPreset::Playtest128,
            Self::Validation => MapPreset::Validation192,
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Dev => "dev64",
            Self::Playtest => "playtest128",
            Self::Validation => "validation192",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScenarioPhase {
    Expand,
    Policy,
    Attack,
    Done,
}

impl ScenarioPhase {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Expand => "expand",
            Self::Policy => "policy",
            Self::Attack => "attack",
            Self::Done => "done",
        }
    }
}

/// Logical-step phase durations used for distributed synchronization.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[allow(clippy::struct_field_names)]
pub struct PhaseSchedule {
    pub expand_steps: u64,
    pub policy_steps: u64,
    pub attack_steps: u64,
    pub reexpand_steps: u64,
}

impl PhaseSchedule {
    pub fn from_steps(
        expand_steps: u64,
        policy_steps: u64,
        attack_steps: u64,
        reexpand_steps: u64,
    ) -> Result<Self> {
        if reexpand_steps == 0 {
            bail!("reexpand_steps must be >= 1");
        }
        Ok(Self {
            expand_steps,
            policy_steps,
            attack_steps,
            reexpand_steps,
        })
    }

    /// Convert wall-second aliases into logical steps at the nominal cadence.
    pub fn from_secs(
        expand_secs: u64,
        policy_secs: u64,
        attack_secs: u64,
        reexpand_secs: u64,
        logical_step_ms: u32,
    ) -> Result<Self> {
        let step_ms = u64::from(logical_step_ms.max(1));
        let to_steps = |secs: u64| secs.saturating_mul(1_000) / step_ms;
        Self::from_steps(
            to_steps(expand_secs),
            to_steps(policy_secs),
            to_steps(attack_secs),
            to_steps(reexpand_secs).max(1),
        )
    }

    pub fn total_steps(self) -> u64 {
        self.expand_steps
            .saturating_add(self.policy_steps)
            .saturating_add(self.attack_steps)
    }

    /// Phase progress uses one shared absolute epoch: `logical_step` itself starts
    /// at zero when the match enters Running, so every process uses
    /// `phase_progress = step.saturating_sub(warmup_steps)` rather than a
    /// per-process snapshot of the step at which it observed Running.
    pub fn phase_at(self, phase_progress: u64) -> ScenarioPhase {
        if phase_progress < self.expand_steps {
            return ScenarioPhase::Expand;
        }
        let after_expand = phase_progress - self.expand_steps;
        if after_expand < self.policy_steps {
            return ScenarioPhase::Policy;
        }
        let after_policy = after_expand - self.policy_steps;
        if self.attack_steps > 0 && after_policy < self.attack_steps {
            return ScenarioPhase::Attack;
        }
        ScenarioPhase::Done
    }

    pub fn phase_progress(logical_step: u64, warmup_steps: u64) -> u64 {
        logical_step.saturating_sub(warmup_steps)
    }

    pub fn in_warmup(logical_step: u64, warmup_steps: u64) -> bool {
        logical_step < warmup_steps
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommandKind {
    Mobilization = 1,
    Expand = 2,
    Policy = 3,
    Attack = 4,
}

/// Deterministic, player-spread command IDs so concurrent workers never collide.
pub const fn deterministic_command_id(player_id: u16, kind: CommandKind, sequence: u32) -> u64 {
    ((player_id as u64) << 48) | ((kind as u64) << 32) | (sequence as u64)
}

/// Contiguous inclusive player ranges covering `1..=total_players`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PlayerRange {
    pub first_player: u16,
    pub player_count: u16,
}

impl PlayerRange {
    pub fn new(first_player: u16, player_count: u16) -> Result<Self> {
        if first_player == 0 {
            bail!("first_player must be >= 1");
        }
        if player_count == 0 {
            bail!("player_count must be >= 1");
        }
        let last = first_player
            .checked_add(player_count)
            .and_then(|value| value.checked_sub(1))
            .context("player range overflow")?;
        if last < first_player {
            bail!("invalid player range");
        }
        Ok(Self {
            first_player,
            player_count,
        })
    }

    pub fn last_player(self) -> u16 {
        self.first_player + self.player_count - 1
    }

    pub fn iter(self) -> impl Iterator<Item = u16> {
        self.first_player..=self.last_player()
    }
}

/// Split `1..=total_players` into contiguous shards of at most `shard_size`.
pub fn player_shards(total_players: u16, shard_size: u16) -> Result<Vec<PlayerRange>> {
    if total_players < 2 {
        bail!("total_players must be >= 2");
    }
    if shard_size == 0 {
        bail!("shard_size must be >= 1");
    }
    let mut shards = Vec::new();
    let mut first = 1_u16;
    while first <= total_players {
        let remaining = total_players - first + 1;
        let count = remaining.min(shard_size);
        shards.push(PlayerRange::new(first, count)?);
        first = first.saturating_add(count);
        if count == 0 {
            bail!("failed to advance player shard");
        }
    }
    Ok(shards)
}

pub fn default_run_dir(database: &str, preset: PresetArg, players: u16) -> PathBuf {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    PathBuf::from(format!(
        "match-perf-runs/{}-{}-{}p-{timestamp}",
        database,
        preset.label(),
        players
    ))
}

pub fn create_run_dir(path: &Path) -> Result<()> {
    if path.exists() {
        bail!(
            "run directory {} already exists; refusing to overwrite",
            path.display()
        );
    }
    std::fs::create_dir_all(path)
        .with_context(|| format!("create run directory {}", path.display()))?;
    Ok(())
}

/// Optional local convenience marker. Workers prefer polling authoritative
/// locked `match_config` so different hosts do not need a shared filesystem.
pub fn readiness_marker_path(run_dir: &Path) -> PathBuf {
    run_dir.join("ready.marker")
}

#[derive(Clone, Copy, Debug, ValueEnum, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum SubscriptionMode {
    /// Receipt-only per-seat queries (minimal command path).
    CommandOnly,
    /// Bootstrap globals + player-filtered/spatial state/tactical rows so each
    /// seat reproduces game-client subscription load.
    #[default]
    FullClient,
}

impl SubscriptionMode {
    pub const fn label(self) -> &'static str {
        match self {
            Self::CommandOnly => "command-only",
            Self::FullClient => "full-client",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkerStatusKind {
    Ready,
    Complete,
    Failure,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkerStatus {
    pub status: WorkerStatusKind,
    pub first_player: u16,
    pub last_player: u16,
    pub player_count: u16,
    pub message: String,
    pub updated_at_unix_s: u64,
}

pub fn worker_status_path(run_dir: &Path, first: u16, last: u16) -> PathBuf {
    run_dir.join(format!("worker-{first}-{last}.status.json"))
}

/// Atomically write a worker status JSON document (temp file + rename).
pub fn write_worker_status(path: &Path, status: &WorkerStatus) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create status parent {}", parent.display()))?;
    }
    let tmp = path.with_extension("status.json.tmp");
    {
        let file = std::fs::File::create(&tmp)
            .with_context(|| format!("create temp status {}", tmp.display()))?;
        serde_json::to_writer_pretty(file, status)
            .with_context(|| format!("serialize status {}", tmp.display()))?;
    }
    std::fs::rename(&tmp, path).with_context(|| format!("publish status {}", path.display()))?;
    Ok(())
}

pub fn read_worker_status(path: &Path) -> Result<Option<WorkerStatus>> {
    if !path.exists() {
        return Ok(None);
    }
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("read status {}", path.display()))?;
    let status =
        serde_json::from_str(&raw).with_context(|| format!("parse status {}", path.display()))?;
    Ok(Some(status))
}

/// Deterministic command-spread stagger: player executes on ticks where
/// `(logical_step + player_id) % spread == 0`. Spread 1 means every player
/// every eligible wave (concurrent fanout of the due batch).
pub fn player_due_on_step(player_id: u16, logical_step: u64, spread: u32) -> bool {
    let spread = u64::from(spread.max(1));
    (logical_step.wrapping_add(u64::from(player_id))) % spread == 0
}

/// Players still pending that are due on this logical step, in ascending id order.
pub fn due_players_from_pending(
    pending: &std::collections::BTreeSet<u16>,
    logical_step: u64,
    spread: u32,
) -> Vec<u16> {
    pending
        .iter()
        .copied()
        .filter(|player_id| player_due_on_step(*player_id, logical_step, spread))
        .collect()
}

/// Fail closed when spread cannot cover every player within the available steps
/// of a wave/phase (`spread` must be `<=` the relevant duration).
pub fn validate_command_spread(spread: u32, available_steps: u64, label: &str) -> Result<()> {
    let spread = u64::from(spread.max(1));
    if available_steps == 0 {
        // Zero-length phases are skipped; nothing to validate.
        return Ok(());
    }
    if spread > available_steps {
        bail!(
            "command_spread {spread} exceeds {label} duration {available_steps}; \
             every player must execute exactly once within the phase/wave"
        );
    }
    Ok(())
}

/// Validate spread against expand wave cadence and non-zero policy/attack phases.
pub fn validate_command_spread_for_schedule(spread: u32, schedule: PhaseSchedule) -> Result<()> {
    validate_command_spread(spread, schedule.reexpand_steps, "reexpand wave")?;
    validate_command_spread(spread, schedule.policy_steps, "policy phase")?;
    validate_command_spread(spread, schedule.attack_steps, "attack phase")?;
    Ok(())
}

pub fn git_revision() -> String {
    Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|revision| revision.trim().to_owned())
        .filter(|revision| !revision.is_empty())
        .unwrap_or_else(|| "unknown".to_owned())
}

pub fn git_dirty() -> bool {
    Command::new("git")
        .args(["status", "--porcelain"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .is_some_and(|status| !status.trim().is_empty())
}

pub fn hostname() -> String {
    Command::new("hostname")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "unknown".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn player_shards_cover_ranges_through_500() {
        for total in [2_u16, 8, 32, 128, 500] {
            for shard_size in [1_u16, 7, 8, 32, 64, 500] {
                let shards = player_shards(total, shard_size).expect("shards");
                assert!(!shards.is_empty());
                assert_eq!(shards[0].first_player, 1);
                assert_eq!(shards.last().expect("last").last_player(), total);
                let mut covered = 0_u16;
                let mut previous_last = 0_u16;
                for shard in &shards {
                    assert!(shard.player_count <= shard_size);
                    assert_eq!(shard.first_player, previous_last + 1);
                    covered += shard.player_count;
                    previous_last = shard.last_player();
                }
                assert_eq!(covered, total);
            }
        }
    }

    #[test]
    fn phase_schedule_advances_by_logical_steps() {
        let schedule = PhaseSchedule::from_steps(10, 20, 5, 4).expect("schedule");
        assert_eq!(schedule.phase_at(0), ScenarioPhase::Expand);
        assert_eq!(schedule.phase_at(9), ScenarioPhase::Expand);
        assert_eq!(schedule.phase_at(10), ScenarioPhase::Policy);
        assert_eq!(schedule.phase_at(29), ScenarioPhase::Policy);
        assert_eq!(schedule.phase_at(30), ScenarioPhase::Attack);
        assert_eq!(schedule.phase_at(34), ScenarioPhase::Attack);
        assert_eq!(schedule.phase_at(35), ScenarioPhase::Done);
        assert_eq!(schedule.total_steps(), 35);
    }

    #[test]
    fn phase_schedule_skips_attack_when_zero() {
        let schedule = PhaseSchedule::from_steps(3, 4, 0, 1).expect("schedule");
        assert_eq!(schedule.phase_at(6), ScenarioPhase::Policy);
        assert_eq!(schedule.phase_at(7), ScenarioPhase::Done);
    }

    #[test]
    fn wall_second_aliases_use_nominal_cadence() {
        let schedule = PhaseSchedule::from_secs(10, 20, 5, 2, 250).expect("schedule");
        assert_eq!(schedule.expand_steps, 40);
        assert_eq!(schedule.policy_steps, 80);
        assert_eq!(schedule.attack_steps, 20);
        assert_eq!(schedule.reexpand_steps, 8);
    }

    #[test]
    fn command_ids_spread_by_player() {
        let left = deterministic_command_id(1, CommandKind::Expand, 1);
        let right = deterministic_command_id(2, CommandKind::Expand, 1);
        assert_ne!(left, right);
        assert_eq!(
            deterministic_command_id(500, CommandKind::Attack, 9),
            deterministic_command_id(500, CommandKind::Attack, 9)
        );
    }

    #[test]
    fn phase_progress_uses_shared_warmup_epoch() {
        let schedule = PhaseSchedule::from_steps(10, 5, 0, 2).expect("schedule");
        assert!(PhaseSchedule::in_warmup(3, 8));
        assert!(!PhaseSchedule::in_warmup(8, 8));
        assert_eq!(PhaseSchedule::phase_progress(8, 8), 0);
        assert_eq!(
            schedule.phase_at(PhaseSchedule::phase_progress(12, 8)),
            ScenarioPhase::Expand
        );
        assert_eq!(
            schedule.phase_at(PhaseSchedule::phase_progress(18, 8)),
            ScenarioPhase::Policy
        );
        assert_eq!(
            schedule.phase_at(PhaseSchedule::phase_progress(23, 8)),
            ScenarioPhase::Done
        );
    }

    #[test]
    fn command_spread_covers_all_players_across_modulus() {
        let spread = 4_u32;
        for player in 1_u16..=16 {
            let hits = (0..20)
                .filter(|step| player_due_on_step(player, *step, spread))
                .count();
            assert!(hits >= 4, "player {player} underserved");
        }
    }

    #[test]
    fn command_spread_pending_dispatch_covers_one_through_five_hundred() {
        use std::collections::BTreeSet;

        // Representative spread/reexpand pairs: default concurrent (1), modest
        // stagger, and large modulus still within a wave/phase budget.
        let cases = [
            (1_u32, 1_u64),
            (1, 180),
            (4, 20),
            (8, 40),
            (16, 32),
            (32, 64),
            (50, 100),
            (100, 100),
            (250, 250),
            (500, 500),
        ];
        for (spread, available_steps) in cases {
            validate_command_spread(spread, available_steps, "test").expect("spread ok");
            let mut pending: BTreeSet<u16> = (1_u16..=500).collect();
            let mut executed = BTreeSet::new();
            for step in 0..available_steps {
                let due = due_players_from_pending(&pending, step, spread);
                for player in due {
                    assert!(pending.remove(&player), "player {player} double-dispatched");
                    assert!(executed.insert(player), "player {player} executed twice");
                }
            }
            assert_eq!(
                executed.len(),
                500,
                "spread={spread} steps={available_steps} missed players"
            );
            assert!(pending.is_empty());
            assert_eq!(
                executed.iter().copied().collect::<Vec<_>>(),
                (1..=500).collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn command_spread_rejects_values_exceeding_phase_budget() {
        assert!(validate_command_spread(5, 4, "wave").is_err());
        assert!(validate_command_spread(1, 0, "skipped").is_ok());
        let schedule = PhaseSchedule::from_steps(10, 3, 0, 2).expect("schedule");
        assert!(validate_command_spread_for_schedule(4, schedule).is_err());
        assert!(validate_command_spread_for_schedule(2, schedule).is_ok());
    }
}
