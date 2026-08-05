//! Distributed-capable live-match load driver and step-rate profiler.
//!
//! Subcommands:
//! - `coordinator` — configure a fresh one-shot match, observe full telemetry
//! - `worker` — drive a contiguous player ID range with minimal subscriptions
//! - `run-local` — spawn coordinator + worker process shards on one host
//!
//! Coordinator and worker can also run on different hosts against one database.
//! Prefer logical-step phase durations for distributed synchronization; wall-
//! second aliases remain available.

#![forbid(unsafe_code)]

mod attack;
mod client;
mod common;
mod coordinator;
mod output;
mod queries;
mod run_local;
mod stats;
mod worker;

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::coordinator::CoordinatorArgs;
use crate::run_local::RunLocalArgs;
use crate::worker::WorkerArgs;

#[derive(Debug, Parser)]
#[command(
    name = "match-perf",
    about = "Distributed-capable live-match load driver and step-rate profiler",
    long_about = "Replaces the older single-process match-perf CLI.\n\n\
Use `run-local` for a one-host multi-process load generation, or run \
`coordinator` and `worker` independently on different hosts.\n\n\
Example (local 32-player playtest):\n  \
cargo run -p match-perf -- run-local --database of-match-perf --preset playtest \
--players 32 --shard-size 8 --expand-steps 20 --policy-steps 20 --attack-steps 0"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Configure a fresh match, publish readiness, sample timeline, write summary.
    Coordinator(CoordinatorArgs),
    /// Own a contiguous player range; issue deterministic commands.
    Worker(WorkerArgs),
    /// Spawn coordinator + worker shards locally and aggregate exit status.
    RunLocal(RunLocalArgs),
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Commands::Coordinator(args) => coordinator::run(args),
        Commands::Worker(args) => worker::run(args),
        Commands::RunLocal(args) => run_local::run(args),
    }
}
