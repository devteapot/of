# Performance profiling

Status: distributed client-load harness for scale and step-rate measurement

This document describes how to profile live match step dilation and client load
from 2 through 500 players. Authority remains **one scheduled reducer** inside a
single match database: client connections and command submission can be
distributed across processes and hosts, but simulation movement and combat are
not sharded.

## What is measured

`match-perf` drives a fresh one-shot match through expansion, optional
front-rebalance (`--rebalance-steps` phase), and optional attack phases.
The coordinator samples the authoritative `logical_step` counter and records:

| Artifact | Contents |
| --- | --- |
| `timeline.csv` | elapsed time, logical step/delta, client-observed gap and ms/step, phase, packet/order/front counts, controlled-cell min/p50/p95/max/sum |
| `players.csv` | long-form `(logical_step, player_id, controlled_cells)` snapshots |
| `worker-<first>-<last>.jsonl` | per-shard join/command receipt status and latency, including expansion attempted/accepted/retried/skipped and front-rebalance attempted/accepted/skipped counts |
| `metadata.json` | full scenario, host/db, map size/hash, shard layout, git HEAD + dirty flag, timing caveat |
| `summary.json` | observed steps (`sum(step_delta)`), weighted p50/p95/p99/max ms/step, max packets/orders/fronts, failures, early completion |

Timing is **client-observed wall-clock time between subscribed logical-step
changes**. It is not server-side reducer fuel. Gaps above the nominal 250 ms
cadence indicate dilation under load.

Controlled-cell columns are aggregate statistics only. There are no fixed
`controlled_p1..p8` fields; use `players.csv` for per-seat series through 500.

## Prerequisites

1. Local SpacetimeDB 2.7.1 running (`./scripts/start-local-server.sh`).
2. A **fresh** match database. `match-perf coordinator` calls one-shot
   `configure_match` and will fail on an already-locked lobby. Never point the
   harness at an interactive `of-match-dev` session you care about.
3. OS connection limits sized for the target player count. Each worker opens one
   shared map/phase observer plus one command connection per owned seat
   (receipt-filtered). At 500 players, budget roughly 500 command connections
   plus a handful of observers. Raise `ulimit -n` and any kernel
   `somaxconn`/ephemeral-port settings before large runs.

Example publish of an isolated perf database:

```bash
spacetime publish --server local --module-path modules/match \
  --delete-data=always --yes of-match-perf
```

## Local multi-process run

The old single-process CLI is replaced by three subcommands. For one host:

```bash
cargo run -p match-perf -- run-local \
  --database of-match-perf \
  --preset playtest \
  --players 32 \
  --shard-size 8 \
  --expand-steps 40 \
  --rebalance-steps 40 \
  --attack-steps 0
```

`run-local` spawns:

1. one `coordinator` (configure + full telemetry observer + readiness marker);
2. enough `worker` subprocesses to cover seats in contiguous `--shard-size`
   ranges.

Artifacts land in a new non-overwriting directory under `artifacts/performance/runs/`
(or `--output-dir`). Existing paths are refused.

### Phase duration options

Prefer **logical-step** durations so distributed workers share one clock:

| Flag | Meaning |
| --- | --- |
| `--expand-steps` | expansion phase length in authoritative steps |
| `--rebalance-steps` | front-rebalance measurement length (issues `issue_front_rebalance`) |
| `--attack-steps` | attack measurement length (`0` skips) |
| `--reexpand-steps` | re-issue expansion waves every N steps while expanding |
| `--warmup-steps` | shared absolute warmup before phase progress (default **120**) |
| `--subscription-mode` | `full-client` (default) or `command-only` |
| `--command-spread` | deterministic player stagger modulus for phase waves (`<=` wave/phase duration) |

Wall-second aliases (`--expand-secs`, `--rebalance-secs`, `--attack-secs`,
`--reexpand-secs`) convert at the nominal 250 ms cadence when step flags are
omitted. Workers and the coordinator synchronize phase from
`logical_step - warmup_steps` on the shared DB clock, not from wall time or
per-process Running snapshots. The default warmup (120 steps ≈ 30s at 250 ms)
gives multi-host joins and per-seat setup a realistic window; raise it further
for slow remote workers.

## Distributed multi-host run

Coordinator and workers are independent processes and may run on different
machines against one published database and reachable host URI.

Terminal A (or orchestration host):

```bash
cargo run -p match-perf -- coordinator \
  --host http://match-host:3000 \
  --database of-match-perf-500 \
  --preset validation \
  --players 500 \
  --shard-size 50 \
  --output-dir /data/runs/val-500 \
  --expand-steps 80 \
  --rebalance-steps 80 \
  --attack-steps 0
```

Workers prefer polling the authoritative locked `match_config` over a shared
`ready.marker`, so they can run on different hosts without a shared filesystem.
The optional marker remains a local convenience. Start workers with disjoint
ranges once configuration is locked (or after the marker appears on shared FS):

```bash
cargo run -p match-perf -- worker \
  --host http://match-host:3000 \
  --database of-match-perf-500 \
  --first-player 1 --player-count 50 --match-players 500 \
  --output-dir /data/runs/val-500 \
  --expand-steps 80 --rebalance-steps 80 --attack-steps 0

cargo run -p match-perf -- worker \
  --host http://match-host:3000 \
  --database of-match-perf-500 \
  --first-player 51 --player-count 50 --match-players 500 \
  --output-dir /data/runs/val-500 \
  --expand-steps 80 --rebalance-steps 80 --attack-steps 0
# ... through player 500
```

Worker connections:

- one shared observer subscribed to map/phase/player tables only (scenario
  derivation; not a per-seat tactical flood);
- one command connection per seat. Default `--subscription-mode full-client`
  mirrors the game client: at `match_players <= 8` full `cell_state` / combat /
  tactical tables; above that, local-owned + spatial cell interest + filtered
  tactical rows. Packet/route tables follow the same debug raw vs release
  visible-view split as the game client. The exact mode is recorded as
  `full-client-low-scale` / `full-client-high-scale` (or `command-only`).
  `command-only` keeps receipt-only queries for a lighter command path.

Command IDs are deterministic and spread by player ID so concurrent workers never
collide. Expansion is derived from each player's current owned component and
neutral traversable perimeter. When a concurrent scheduled tick invalidates
that snapshot, the worker retries with a fresh command ID; players with no
remaining frontier are explicitly skipped. `--command-spread` must not exceed
the relevant expand-wave /
rebalance / attack duration; workers keep pending player sets and dispatch due
residues across subsequent logical steps so every seat is **accepted or
explicitly skipped** once per expansion wave and once for rebalance/attack
(default spread 1 = concurrent fanout of the full due batch). Phase progress
uses one shared absolute epoch:
`phase_progress = logical_step - warmup_steps` (default warmup 120). Workers fan
out per-seat reducer submissions before awaiting callbacks so shard load is
concurrent. When the run directory is shared, workers emit atomic
`worker-<first>-<last>.status.json` (`ready` / `complete` / `failure`). With
`--wait-for-worker-status` the coordinator polls those files during
lobby/warmup/phases and always writes a terminal `summary.json` (with failure
count) before returning failure. `run-local` gives the coordinator a bounded
grace window after a worker exit so that summary can finalize before remaining
children are killed. Remote/no-shared-FS runs should omit
`--wait-for-worker-status`; the phase clock remains database-based either way.

The `--rebalance-steps` phase drives **front rebalance**. Each worker observer derives, when
possible, one complete owned traversable component and two distinct strategic
front seeds (`hex_core::strategic_fronts`), then seats issue
`issue_front_rebalance` with the configured `--command-share-bps` and an empty
supersede list. Component cell IDs are exact and deterministic, the source must
have movable troops outside the target arc, and the target must have physical
military headroom. Players whose current topology or resources lack a usable
pair are **skipped and reported** (worker JSONL + console summary counts:
attempted/accepted/skipped) rather than sent an invalid command. If troop supply
or target capacity changes between the observer snapshot and receipt, that
narrow resource-exhaustion result is also an accounted skip; every other issued
command rejection still fails the run.

Attack commands are optional. When enabled, each seat must have a real
traversable adjacent owned→enemy front; otherwise the worker fails closed.

## Matrix script

`scripts/run-match-perf-matrix.sh` walks the default scale matrix **headless**
by default:

- players: `2 8 32 128 500`
- presets: `dev playtest validation`

It requires an explicit destructive confirmation flag, publishes a **unique**
fresh database per cell with `--delete-data`, runs `run-local`, traps/cleans
child processes, preserves non-overwriting run directories, and appends
`matrix.csv` from each `summary.json`. Matrix rows also aggregate expansion
attempted/accepted/retried/skipped counts plus `front_rebalance_attempted`,
`front_rebalance_accepted`, and
`front_rebalance_skipped` from worker logs so a fast run cannot hide a topology
that exercised no rebalance commands.

```bash
# Headless matrix (default): no Bevy window, CSV/JSON only.
./scripts/run-match-perf-matrix.sh --confirm-destructive-matrix

# Optional Bevy viewer attached as player 1 (reuses the worker seat token).
./scripts/run-match-perf-matrix.sh --confirm-destructive-matrix --viewer
# or: OF_PERF_VIEWER=1 ./scripts/run-match-perf-matrix.sh --confirm-destructive-matrix
```

`--viewer` / `OF_PERF_VIEWER` does not change headless load generation or
artifact layout. The script copies `player-1.token` into a unique
`.spacetime-data/client-perf-viewer-…` profile path so the game client reuses
the worker identity, and tears the viewer + token down between cells / on exit.

Useful environment overrides:

| Variable | Default | Purpose |
| --- | --- | --- |
| `OF_PERF_PLAYERS` | `2 8 32 128 500` | player counts |
| `OF_PERF_PRESETS` | `dev playtest validation` | map presets |
| `OF_PERF_SHARD_SIZE` | `32` | worker shard size |
| `OF_PERF_EXPAND_STEPS` / `POLICY` / `ATTACK` / `REEXPAND` | short smoke defaults | phase lengths (`POLICY` = front-rebalance phase) |
| `OF_PERF_WARMUP_STEPS` | `120` | shared warmup before phase progress |
| `OF_PERF_OUT_ROOT` | `artifacts/performance/matrix-<ts>` | artifact root |
| `OF_PERF_TIMEOUT_SECS` | `3600` | per-cell timeout |
| `OF_PERF_HOST` | `http://127.0.0.1:3000` | client SpacetimeDB URI |
| `OF_PERF_SERVER` | `local` | explicit `spacetime publish --server` target |
| `OF_PERF_BIN` | `cargo run -p match-perf --` | optional prebuilt binary |
| `OF_PERF_VIEWER` | `0` | `1`/`true`/`yes`/`on` launches a Bevy viewer as player 1 |

A full 500 × validation cell is a long run. Start with tiny step counts and a
reduced player list when validating the harness itself.

## Client subscription model (high scale)

Game clients bootstrap with **immutable full terrain + match/player metadata
only** (no `cell_state` / combat / tactical flood). After the authoritative
player count and local seat are known they issue a one-time tactical
subscription, plus (at high scale) a **separate moving spatial `CellState`
handle**:

- `player_count <= 8`: full `cell_state` and `combat_front` plus full tactical
  rows on the tactical handle;
- `player_count > 8`: tactical handle keeps all local-owned `CellState` globally,
  local attacker/defender combat fronts, and local tactical rows; a separate
  spatial handle covers a chunk-radius square around the camera focus (spawn-
  centered until camera state is available) and resubscribes when the focus
  crosses server chunk boundaries. Old spatial handles are retired so
  subscriptions never accumulate; cells leaving interest project to
  neutral/default.

This is **bandwidth interest** (all local-owned cells + moving viewport remote
state). It is not a security boundary. Missing remote state rows render as
neutral/default; local ownership remains complete as territory expands. The
tactical handle never repeats the bootstrap query set. Commands stay blocked
until bootstrap + tactical have applied. See `crates/game-client/src/online.rs`.

Authority remains **one scheduled reducer and one atomic simulation tick**.
`PacketTickState` still processes the complete active packet set; sources are
loaded via `source_by_order` for the union of active packet order IDs **and**
active transfer order IDs (queued sources on active orders with no packet yet).

## Recorded baselines (2026-08-07)

Budgets from [technical architecture](./technical-architecture.md): cadence
250 ms; nominal active-step processing p95 **&lt; 62.5 ms**; stretch **&lt; 125 ms**.
`match-perf` reports client-observed wall-clock ms/step (includes cadence).
Processing dilation ≈ `max(0, observed_p95 − 250)`.

| Scenario | Preset / seats | observed p50 / p95 ms/step | dilation p95 | vs budget | Artifact dir |
| --- | --- | ---: | ---: | --- | --- |
| Nominal-ish 128 | `playtest` / 128 | 251.0 / 268.3 | **18.3 ms** | **PASS** (&lt; 62.5) | `artifacts/performance/runs/playtest128-20260807T181657Z` |
| Stretch map 192 | `validation` / 128 | 250.0 / 269.1 | **19.1 ms** | **PASS** (&lt; 62.5 / &lt; 125) | `artifacts/performance/runs/validation-nominal-20260807T181753Z` |

Notes: both 128-seat runs failures=0. The playtest run hit max packets 1813 /
fronts 1502; validation hit max packets 1817 / fronts 1407. p99/max show rare
spikes while p95 stays near cadence. A 500-seat validation attempt on this host
failed during lobby joins with WebSocket handshake errors (connection limits);
raise `ulimit -n` / ephemeral ports before treating that as a module regression.
True architecture stretch (256×256) remains an unbuilt map preset.

`match-perf` calls `start_match` from player 1 after all seats are claimed
(interactive lobby cutover removed auto-start on join).

## Authority boundary and caveats

- **One match database, one scheduled simulation.** Distributing `match-perf`
  workers distributes client websocket/reducer submission load only.
- **Do not overwrite artifacts.** Run directories and CSV/JSON outputs use
  create-new semantics.
- **Fresh DB required.** Lobby configuration is one-shot.
- **OS limits dominate at 500 connections.** Exhausted file descriptors or
  ephemeral ports look like flaky joins; raise limits before blaming the module.
- **Step timing is observational.** Pair `summary.json` percentiles with module
  instrumentation (for example simulation phase timings) when diagnosing reducer
  hot paths.
- **Ad-hoc `perf-*.csv` logs are local artifacts.** Keep them under the ignored
  `artifacts/performance/` directory. New structured runs are created beneath
  `artifacts/performance/runs/` and must not clobber existing outputs.

## Related docs

- [Technical architecture](./technical-architecture.md) — subscriptions, cadence, scale bands
- [Browser release gates](./browser-gates.md) — Wasm download, WebGPU 128/192, reconnect soak
- [Implementation guide](./implementation.md) — authority and table layout
- [README](../README.md) — toolchain and local multiplayer quick start
