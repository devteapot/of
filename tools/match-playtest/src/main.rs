//! Fully automated no-human live playtest for the cluster-first control
//! surface (docs/playtests/cluster-controls-v1.md), run against a real local
//! SpacetimeDB match through the public reducer/table surface only.
//!
//! One-command entrypoint: `./scripts/run-automated-playtest.sh`
//! (publishes a fresh isolated database, runs this binary, and leaves the
//! artifacts + results document behind). Direct use:
//!
//! ```text
//! cargo run -p match-playtest -- --database of-match-e2e-auto
//! ```

mod client;
mod monitor;
mod report;
mod scenarios;
mod world;

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::Result;
use clap::Parser;
use match_bindings::{MapPreset, MatchPhase, MatchStateTableAccess};

use client::Client;
use monitor::{Mode, Monitor};
use report::{RunReport, ScenarioResult, Verdict, unix_ms, utc_date_label};
use scenarios::{PLAYER_ONE, PLAYER_TWO, Session};
use world::SINGLETON_ID;

#[derive(Debug, Parser)]
#[command(about = "Automated no-human live playtest for the cluster control surface")]
struct Args {
    /// `SpacetimeDB` host URI.
    #[arg(long, default_value = "http://127.0.0.1:3000")]
    host: String,

    /// Freshly published isolated database (never the dev database).
    #[arg(long, default_value = "of-match-e2e-auto")]
    database: String,

    /// Directory for the ignored identity token profiles.
    #[arg(long, default_value = ".match-playtest-tokens")]
    token_dir: PathBuf,

    /// Per-operation timeout.
    #[arg(long, default_value_t = 30)]
    timeout_secs: u64,

    /// Wall-clock budget for the expansion-to-contact staging phase.
    #[arg(long, default_value_t = 240)]
    contact_budget_secs: u64,

    /// Structured JSON artifact path (gitignored).
    #[arg(long)]
    artifact: Option<PathBuf>,

    /// Machine-generated results markdown path.
    #[arg(long)]
    results_doc: Option<PathBuf>,
}

fn main() -> ExitCode {
    match run() {
        Ok(passed) => {
            if passed {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(2)
            }
        }
        Err(error) => {
            eprintln!("FATAL: {error:#}");
            ExitCode::FAILURE
        }
    }
}

#[allow(clippy::too_many_lines)]
fn run() -> Result<bool> {
    let args = Args::parse();
    let timeout = Duration::from_secs(args.timeout_secs);
    let date = utc_date_label();
    let started_unix_ms = unix_ms();

    println!(
        "[1/8] connecting observer and two players to {}/{}",
        args.host, args.database
    );
    let observer = Client::connect(
        "observer",
        &args.token_dir.join("observer.token"),
        &args.host,
        &args.database,
        timeout,
    )?;
    let p1 = Client::connect(
        "player-one",
        &args.token_dir.join("player-1.token"),
        &args.host,
        &args.database,
        timeout,
    )?;
    let p2 = Client::connect(
        "player-two",
        &args.token_dir.join("player-2.token"),
        &args.host,
        &args.database,
        timeout,
    )?;

    println!("[2/8] configuring, joining, and starting a fresh two-player match");
    let phase = p1
        .conn
        .db
        .match_state()
        .singleton_id()
        .find(&SINGLETON_ID)
        .map(|state| state.phase);
    anyhow::ensure!(
        phase == Some(MatchPhase::Lobby),
        "expected a freshly published database in Lobby phase, found {phase:?}; \
         publish with --delete-data=always first"
    );
    p1.configure_match(MapPreset::Dev64, 2, timeout)?;
    p1.join_match(PLAYER_ONE, "auto-p1", timeout)?;
    p2.join_match(PLAYER_TWO, "auto-p2", timeout)?;
    p1.start_match(timeout)?;

    let step_ms = {
        let snapshot = world::WorldSnapshot::capture(&p1.conn)?;
        snapshot.config.logical_step_ms
    };
    let step = Duration::from_millis(u64::from(step_ms.max(1)));
    let poll = (step / 4).max(Duration::from_millis(20));

    // Both players fight with their spawn armies only until a scenario needs
    // growth: exact conservation is the default accounting regime.
    let monitor = Monitor::start(observer, poll);
    let session = Session {
        p1: &p1,
        p2: &p2,
        monitor: &monitor,
        step,
        poll,
        timeout,
    };
    client::wait_until("match running", timeout, poll, || {
        let state = p1.conn.db.match_state().singleton_id().find(&SINGLETON_ID);
        Ok(state.and_then(|row| (row.phase == MatchPhase::Running).then_some(())))
    })?;
    for player in [PLAYER_ONE, PLAYER_TWO] {
        let command_id = session.command_id(player)?;
        session
            .client(player)
            .set_mobilization_target(command_id, 0, timeout)?;
        session.accepted_receipt(player, command_id)?;
    }
    let (map_preset, map_seed, _) = scenarios::map_summary(&session)?;
    println!(
        "    map {map_preset} seed {map_seed:#x}, logical step {step_ms} ms; monitor sampling every {poll:?}"
    );

    let mut results: Vec<ScenarioResult> = Vec::new();
    println!("[3/8] strict idle conservation baseline");
    scenarios::strict_idle_window(&session, 10)?;

    println!("[4/8] S1 focus-as-destination + S4 share-once (pre-contact, spawn armies)");
    results.push(run_scenario("S1", scenarios::s1_focus_weighting(&session)));
    session.quiesce()?;
    results.push(run_scenario("S4", scenarios::s4_share_once(&session)));
    session.quiesce()?;

    println!(
        "[5/8] expanding both players toward each other (mobilization on, budget {}s)",
        args.contact_budget_secs
    );
    let contact =
        scenarios::establish_contact(&session, Duration::from_secs(args.contact_budget_secs))?;
    println!("    hostile contact established: {contact}");

    println!("[6/8] S5 reshape + S3 front rebalance + S6 exact stop (strict windows)");
    results.push(run_scenario("S5", scenarios::s5_reshape(&session)));
    session.quiesce()?;
    scenarios::strict_idle_window(&session, 4)?;
    results.push(run_scenario("S3", scenarios::s3_front_rebalance(&session)));
    session.quiesce()?;
    results.push(run_scenario("S6", scenarios::s6_exact_stop(&session)));
    session.quiesce()?;

    println!("[7/8] S2 attack mask (combat window)");
    results.push(run_scenario("S2", scenarios::s2_attack_mask(&session)));
    session.quiesce()?;
    scenarios::strict_idle_window(&session, 8)?;

    println!("[8/8] finishing instrumentation and writing artifacts");
    monitor.set_mode(Mode::Strict);
    let (monitor_report, mut observer) = monitor.finish();
    let violation_count = monitor_report.violations.len();

    results.sort_by_key(|scenario| scenario.risk.clone());
    let all_pass = results
        .iter()
        .all(|scenario| scenario.verdict != Verdict::Fail)
        && violation_count == 0;
    let git_sha = git_sha();
    let finished_unix_ms = unix_ms();
    let run_report = RunReport {
        kind: "cluster-controls-v1-automated",
        host: args.host.clone(),
        database: args.database.clone(),
        map_preset,
        map_seed,
        git_sha,
        started_unix_ms,
        finished_unix_ms,
        logical_step_ms: step_ms,
        scenarios: results,
        monitor: monitor_report,
        passed: all_pass,
    };

    let artifact = args.artifact.unwrap_or_else(|| {
        PathBuf::from(format!(
            "artifacts/playtests/cluster-controls-v1-automated-{date}.json"
        ))
    });
    report::write_json(&artifact, &run_report)?;
    let doc = args.results_doc.unwrap_or_else(|| {
        PathBuf::from(format!(
            "docs/playtests/cluster-controls-v1-automated-{date}.md"
        ))
    });
    report::write_markdown(&doc, &run_report, &date)?;

    println!();
    for scenario in &run_report.scenarios {
        println!(
            "  {}: {} — {}",
            scenario.risk,
            scenario.verdict.label(),
            scenario.title
        );
    }
    println!(
        "  invariants: {} violations across {} samples (steps {}..{})",
        violation_count,
        run_report.monitor.samples,
        run_report.monitor.first_step,
        run_report.monitor.last_step
    );
    println!("  artifact: {}", artifact.display());
    println!("  results doc: {}", doc.display());
    println!(
        "{}",
        if all_pass {
            "PASS: all automated behavioral checks and global invariants held"
        } else {
            "FAIL: at least one behavioral check or invariant failed (see results doc)"
        }
    );

    let _ = observer.disconnect(timeout);
    let mut p1 = p1;
    let mut p2 = p2;
    let _ = p1.disconnect(timeout);
    let _ = p2.disconnect(timeout);
    Ok(all_pass)
}

fn run_scenario(label: &str, outcome: Result<ScenarioResult>) -> ScenarioResult {
    match outcome {
        Ok(result) => {
            println!("    {label}: {}", result.verdict.label());
            result
        }
        Err(error) => {
            println!("    {label}: FAIL ({error:#})");
            let titles = [
                (
                    "S1",
                    "Focus-as-destination: 11/10/9-weighted branches, none suppressed",
                ),
                (
                    "S2",
                    "Attack mask: captures never leave the accepted target footprint; fronts stay on it",
                ),
                (
                    "S3",
                    "Front rebalance: Share-once snapshot, physical traversal, conservation",
                ),
                (
                    "S4",
                    "Whole-cluster multi-select: Share once per source, then share-of-remainder",
                ),
                (
                    "S5",
                    "Reshape: undersized footprint saturates + conserves overflow; oversized drains",
                ),
                (
                    "S6",
                    "Exact Stop: only the frozen order set is released, at current physical cells",
                ),
            ];
            let title = titles
                .iter()
                .find(|(risk, _)| *risk == label)
                .map(|(_, title)| *title)
                .unwrap_or(label);
            let mut result = ScenarioResult::new(label, title);
            result.fail(format!("scenario aborted: {error:#}"));
            result
        }
    }
}

fn git_sha() -> String {
    std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .unwrap_or_else(|| "unknown".to_owned())
}
