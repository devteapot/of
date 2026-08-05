//! Non-overwriting run-directory writers for timeline, players, metadata, summary.

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::common::{PhaseSchedule, PresetArg, ScenarioPhase, git_dirty, git_revision, hostname};
use crate::stats::{ControlStats, quantiles_ms};

pub const TIMELINE_HEADER: &str = "elapsed_s,logical_step,step_delta,client_observed_gap_ms,observed_ms_per_step,scenario_phase,warmup,packets,active_orders,fronts,controlled_min,controlled_p50,controlled_p95,controlled_max,controlled_sum";
pub const PLAYERS_HEADER: &str = "logical_step,player_id,controlled_cells";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RunMetadata {
    pub generated_at_unix_s: u64,
    pub host: String,
    pub hostname: String,
    pub database: String,
    pub git_revision: String,
    pub git_dirty: bool,
    pub preset: String,
    pub player_count: u16,
    pub map_width: u16,
    pub map_height: u16,
    pub map_hash: String,
    pub logical_step_ms: u32,
    pub phase_schedule: PhaseSchedule,
    pub warmup_steps: u64,
    pub subscription_mode: String,
    pub command_spread: u32,
    pub mobilization_bps: u32,
    pub command_share_bps: u32,
    pub shard_layout: Vec<ShardMeta>,
    pub players_snapshot_every_steps: u64,
    pub timing_caveat: String,
    pub authority_note: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ShardMeta {
    pub first_player: u16,
    pub last_player: u16,
    pub player_count: u16,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RunSummary {
    pub observed_steps: u64,
    pub observed_ms_per_step_p50: f64,
    pub observed_ms_per_step_p95: f64,
    pub observed_ms_per_step_p99: f64,
    pub observed_ms_per_step_max: f64,
    pub max_packets: u64,
    pub max_active_orders: u64,
    pub max_fronts: u64,
    pub failures: u64,
    pub early_completion: bool,
    pub final_phase: String,
    pub final_logical_step: u64,
}

pub struct TimelineWriter {
    out: BufWriter<File>,
    /// One sample per observed step; multi-step gaps contribute `step_delta`
    /// weighted copies of the per-step rate so skipped steps still affect quantiles.
    samples_ms_per_step: Vec<f64>,
    max_packets: u64,
    max_orders: u64,
    max_fronts: u64,
    observed_steps: u64,
    last_step: Option<u64>,
    last_phase: ScenarioPhase,
    last_row: Option<LastTimelineRow>,
}

#[derive(Clone, Copy, Debug)]
struct LastTimelineRow {
    elapsed_s: f64,
}

impl TimelineWriter {
    pub fn create(path: &Path) -> Result<Self> {
        let file = create_new_file(path)?;
        let mut out = BufWriter::new(file);
        writeln!(out, "{TIMELINE_HEADER}")?;
        out.flush()?;
        Ok(Self {
            out,
            samples_ms_per_step: Vec::new(),
            max_packets: 0,
            max_orders: 0,
            max_fronts: 0,
            observed_steps: 0,
            last_step: None,
            last_phase: ScenarioPhase::Expand,
            last_row: None,
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn write_row(
        &mut self,
        elapsed_s: f64,
        logical_step: u64,
        step_delta: u64,
        gap_ms: f64,
        phase: ScenarioPhase,
        warmup: bool,
        packets: u64,
        active_orders: u64,
        fronts: u64,
        controls: ControlStats,
    ) -> Result<()> {
        #[allow(clippy::cast_precision_loss)]
        let observed_ms_per_step = if step_delta == 0 {
            0.0
        } else {
            gap_ms / step_delta as f64
        };
        writeln!(
            self.out,
            "{elapsed_s:.3},{logical_step},{step_delta},{gap_ms:.1},{observed_ms_per_step:.3},{},{},{packets},{active_orders},{fronts},{},{},{},{},{}",
            phase.label(),
            u8::from(warmup),
            controls.min,
            controls.p50,
            controls.p95,
            controls.max,
            controls.sum,
        )?;
        self.out.flush()?;
        if step_delta > 0 {
            // Weight multi-step gaps so skipped-step timing enters quantiles.
            for _ in 0..step_delta {
                self.samples_ms_per_step.push(observed_ms_per_step);
            }
            self.observed_steps = self.observed_steps.saturating_add(step_delta);
        }
        self.max_packets = self.max_packets.max(packets);
        self.max_orders = self.max_orders.max(active_orders);
        self.max_fronts = self.max_fronts.max(fronts);
        self.last_step = Some(logical_step);
        self.last_phase = phase;
        self.last_row = Some(LastTimelineRow { elapsed_s });
        Ok(())
    }

    /// Re-record the terminal Done/Completed sample before exit so summaries
    /// always include the final observed state even when the loop breaks on phase.
    #[allow(clippy::too_many_arguments)]
    pub fn record_terminal_sample_if_needed(
        &mut self,
        elapsed_s: f64,
        logical_step: u64,
        phase: ScenarioPhase,
        warmup: bool,
        packets: u64,
        active_orders: u64,
        fronts: u64,
        controls: ControlStats,
    ) -> Result<()> {
        if self.last_step == Some(logical_step) && self.last_phase == phase {
            return Ok(());
        }
        let step_delta = self
            .last_step
            .map_or(0, |previous| logical_step.saturating_sub(previous));
        let gap_ms = self
            .last_row
            .map_or(0.0, |row| (elapsed_s - row.elapsed_s).max(0.0) * 1_000.0);
        self.write_row(
            elapsed_s,
            logical_step,
            step_delta,
            gap_ms,
            phase,
            warmup,
            packets,
            active_orders,
            fronts,
            controls,
        )
    }

    pub fn into_summary(self, failures: u64, early_completion: bool) -> RunSummary {
        let (p50, p95, p99, max) = quantiles_ms(&self.samples_ms_per_step);
        RunSummary {
            observed_steps: self.observed_steps,
            observed_ms_per_step_p50: p50,
            observed_ms_per_step_p95: p95,
            observed_ms_per_step_p99: p99,
            observed_ms_per_step_max: max,
            max_packets: self.max_packets,
            max_active_orders: self.max_orders,
            max_fronts: self.max_fronts,
            failures,
            early_completion,
            final_phase: self.last_phase.label().to_owned(),
            final_logical_step: self.last_step.unwrap_or(0),
        }
    }
}

pub struct PlayersWriter {
    out: BufWriter<File>,
}

impl PlayersWriter {
    pub fn create(path: &Path) -> Result<Self> {
        let file = create_new_file(path)?;
        let mut out = BufWriter::new(file);
        writeln!(out, "{PLAYERS_HEADER}")?;
        out.flush()?;
        Ok(Self { out })
    }

    pub fn write_snapshot(&mut self, logical_step: u64, controls: &[(u16, u64)]) -> Result<()> {
        for (player_id, controlled) in controls {
            writeln!(self.out, "{logical_step},{player_id},{controlled}")?;
        }
        self.out.flush()?;
        Ok(())
    }
}

pub struct WorkerLog {
    path: PathBuf,
    out: BufWriter<File>,
}

impl WorkerLog {
    pub fn create(run_dir: &Path, first: u16, last: u16) -> Result<Self> {
        let path = run_dir.join(format!("worker-{first}-{last}.jsonl"));
        let file = create_new_file(&path)?;
        Ok(Self {
            path,
            out: BufWriter::new(file),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn write_event(&mut self, event: &serde_json::Value) -> Result<()> {
        serde_json::to_writer(&mut self.out, event)?;
        self.out.write_all(b"\n")?;
        self.out.flush()?;
        Ok(())
    }
}

pub fn write_metadata(path: &Path, metadata: &RunMetadata) -> Result<()> {
    let file = create_new_file(path)?;
    serde_json::to_writer_pretty(file, metadata)?;
    Ok(())
}

pub fn write_summary(path: &Path, summary: &RunSummary) -> Result<()> {
    let file = create_new_file(path)?;
    serde_json::to_writer_pretty(file, summary)?;
    Ok(())
}

pub fn create_new_file(path: &Path) -> Result<File> {
    OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
        .with_context(|| format!("create new file {}", path.display()))
}

#[allow(clippy::too_many_arguments)]
pub fn build_metadata(
    host: &str,
    database: &str,
    preset: PresetArg,
    player_count: u16,
    map_width: u16,
    map_height: u16,
    map_hash: u64,
    logical_step_ms: u32,
    schedule: PhaseSchedule,
    warmup_steps: u64,
    subscription_mode: &str,
    command_spread: u32,
    mobilization_bps: u32,
    command_share_bps: u32,
    shards: &[ShardMeta],
    players_snapshot_every_steps: u64,
    generated_at_unix_s: u64,
) -> RunMetadata {
    RunMetadata {
        generated_at_unix_s,
        host: host.to_owned(),
        hostname: hostname(),
        database: database.to_owned(),
        git_revision: git_revision(),
        git_dirty: git_dirty(),
        preset: preset.label().to_owned(),
        player_count,
        map_width,
        map_height,
        map_hash: format!("{map_hash:016x}"),
        logical_step_ms,
        phase_schedule: schedule,
        warmup_steps,
        subscription_mode: subscription_mode.to_owned(),
        command_spread,
        mobilization_bps,
        command_share_bps,
        shard_layout: shards.to_vec(),
        players_snapshot_every_steps,
        timing_caveat: "client-observed wall-clock time between subscribed logical-step changes; not server-side reducer fuel. observed_steps sums step_delta; multi-step gaps weight quantiles".to_owned(),
        authority_note: "authority remains one scheduled reducer and one atomic simulation tick; client load is distributed across coordinator/worker processes, simulation movement/combat is not sharded".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeline_header_has_no_fixed_player_columns() {
        assert!(!TIMELINE_HEADER.contains("controlled_p1"));
        assert!(!TIMELINE_HEADER.contains("controlled_p8"));
        assert!(TIMELINE_HEADER.contains("controlled_min"));
        assert!(TIMELINE_HEADER.contains("controlled_p50"));
        assert!(TIMELINE_HEADER.contains("controlled_p95"));
        assert!(TIMELINE_HEADER.contains("controlled_max"));
        assert!(TIMELINE_HEADER.contains("controlled_sum"));
        assert!(TIMELINE_HEADER.contains("fronts"));
        assert!(TIMELINE_HEADER.contains("observed_ms_per_step"));
    }

    #[test]
    fn players_header_is_long_form() {
        assert_eq!(PLAYERS_HEADER, "logical_step,player_id,controlled_cells");
    }
}
