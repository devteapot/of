# Hex RTS V1

A native 2–500 player RTS prototype about moving conserved aggregate forces across
a stepped 2.5D hex world. The primary interaction is cluster-first: select one
or more complete owned traversable clusters, then click neutral ground to expand
their full perimeters toward that focus or click complete enemy clusters to
attack every shared front. The authoritative wave may split, merge, and turn as
the front changes. Terrain height, capacity, throughput, frontage, garrisons,
resistance, and travel time determine how far each branch advances.

The project intentionally has no settled title, fiction, or production art yet.
V1 is a graybox built to test cluster conquest and explicit troop logistics.
The exact control contract lives in
[Cluster-first troop controls](docs/cluster-controls.md).

## What V1 includes

- Two to 500 human players in one authoritative match (default two), with contiguous u16 IDs starting at 1 and neutral ownership encoded as 0; no bots or NPC factions.
- Conquest victory at 80% of a fixed capturable-land denominator.
- Deterministic 64×64, 128×128, and 192×192 stepped-island map presets.
- Per-hex civilians, infantry, civilian capacity, and military capacity.
- A global mobilization target that converts population locally over time.
- Contextual neutral expansion from every selected perimeter, mildly weighted
  toward the clicked focus and powered only by troops already on that perimeter.
- Target-mask enemy attacks from every shared front. Branches dynamically turn
  through the selected enemy clusters and never escape into an unselected one;
  only troops already stationed on a shared hostile front participate.
- One persisted Force Share setting for expansion, attack, and explicit
  front-to-front rebalancing, independent of mobilization and applied once per
  participating free source cell.
- Deterministic strategic front arcs with terrain-aware, one-shot troop routing.
- Single-cluster, player-drawn best-effort Reshape using the whole available
  pool, plus exact selected-order cancellation.
- Height-aware movement, impassable cliffs, uphill combat penalties, edge
  frontage, casualties, capture, and disconnected pockets.
- A native Bevy client with chunked 3D terrain, switchable soldier/civilian
  shading and readable close-zoom totals, selection, route/front overlays, HUD,
  inspector, previews, rejections, pressure-blended contested cells, and
  reconnectable SpacetimeDB profiles.
- An explicit offline fixture for fast interaction and rendering work.

## Pinned toolchain

- Rust 1.95.0
- Bevy 0.19.0
- SpacetimeDB 2.7.1

Rust is selected automatically by `rust-toolchain.toml`. Install SpacetimeDB,
then select the matching release:

```bash
curl -sSf https://install.spacetimedb.com | sh
spacetime version install 2.7.1 --use --yes
```

`./scripts/check-toolchain.sh` verifies both versions.

## Fast offline start

The offline fixture needs no database and is useful for learning the controls:

```bash
./scripts/run-client.sh --offline
```

Offline commands resolve locally for presentation testing. Multiplayer,
authoritative timing, combat, and persistence must be evaluated online.

## Local multiplayer match

Use separate terminals from the repository root.

1. Start the local SpacetimeDB host:

   ```bash
   ./scripts/start-local-server.sh
   ```

2. Build and publish a fresh development match, then regenerate its typed
   client bindings. This command permanently deletes and recreates only the
   local `of-match-dev` database; the explicit confirmation argument is
   required:

   ```bash
   ./scripts/publish-local.sh --fresh --confirm-delete-of-match-dev
   ```

   For later schema-compatible module updates that must preserve an in-progress
   match, use `./scripts/publish-local.sh` without arguments instead. The
   strategic-front, Reshape, and cancellation cutover changes the persisted
   schema and reducer surface, so an older local database must be
   recreated with the fresh command above.

3. Before any player joins, optionally choose both map and player scale. The
   default is `dev64` with two players:

   ```bash
   spacetime call --server local of-match-dev configure_match '{"playtest128":{}}' 4
   ```

   Player counts from 2 through 500 are supported. Counts above eight use one-cell high-scale spawns, a compact HUD summary, and selective local tactical subscriptions. Other presets are `dev64` and
   `validation192`. Lobby configuration is one-shot: the first successful
   `configure_match` or `configure_map` locks it against further regeneration
   without claiming a player slot. Every configured slot remains open for a
   normal `join_match` call. The compatibility reducer `configure_map` changes
   only the preset while retaining the currently configured player count:

   ```bash
   spacetime call --server local of-match-dev configure_map '{"playtest128":{}}'
   ```

4. Start one client per configured slot with distinct persistent profiles. For
   the default two-player match:

   ```bash
   OF_PLAYER=1 OF_NAME="Player One" OF_PROFILE=player-one \
     ./scripts/run-client.sh
   ```

   ```bash
   OF_PLAYER=2 OF_NAME="Player Two" OF_PROFILE=player-two \
     ./scripts/run-client.sh
   ```

The match begins when every configured slot is claimed. Use `OF_PLAYER=3`
through `OF_PLAYER=8` for larger matches. Profile tokens are stored below
the ignored `.spacetime-data/` directory; reuse the same profile to reclaim a
slot after reconnecting. If a different identity tries to enter a fully claimed
match, the client remains an unbound observer and reports the exact slot error;
start a fresh match with the reset command above rather than stealing a
disconnected identity. The defaults are `http://127.0.0.1:3000` and
`of-match-dev` and can be overridden with `OF_HOST` and `OF_DATABASE` (or the
`--host` and `--database` arguments).

## Controls

The V1 interaction unit is a complete owned traversable cluster. Selection does
not adopt, cancel, or retask live orders. After selecting one or more source
clusters, the owner of the clicked map cell determines the action: neutral
ground expands every reachable source perimeter with a mild bias toward the
click, while enemy ground attacks the clicked enemy cluster from every shared
front. See [Cluster-first troop controls](docs/cluster-controls.md) for the
complete authority and accounting contract.

The HUD is keybind-first. Its compact strip shows only the current state and
relevant keys; `?` opens the field manual.

| Input | Action |
| --- | --- |
| `C` | Replace the selection with the complete owned cluster under the cursor |
| Shift + `C` | Add the hovered owned cluster |
| Control + `C` | Remove the hovered owned cluster |
| Control/Command + `A` | Select every owned traversable cluster, including empty controlled cells |
| Left click neutral ground | Expand every reachable selected perimeter, weighted toward the click |
| Left click an enemy hex | Attack that complete enemy cluster from every shared front |
| Shift + left click enemy | Stage or toggle another complete enemy target cluster |
| Control + left click enemy | Remove that staged enemy cluster |
| `Enter` | Submit the staged enemy-cluster union or another ready preview |
| `[` / `]` | Lower or raise the persisted Share used by expansion, attack, and front rebalancing |
| `B`, then left drag | For one selected cluster, move Share once from the source strategic front to the target front |
| `T`, then left drag | For one selected cluster, draw and preview a best-effort troop footprint |
| `[` / `]` while reshaping | Remove or add one symmetric brush ring |
| Shift + `[` / `]` while reshaping | Change brush width only |
| Control + `[` / `]` while reshaping | Change brush height only |
| Shift + Control + `[` / `]` while reshaping | Change brush width and height together |
| `X` | Preview the exact explicit-dispatch snapshot intersecting the selected clusters |
| Escape | Cancel staged targets, Reshape, or Stop; while idle, clear selection |
| Middle drag or Space + left drag | Pan |
| `W` `A` `S` `D` | Pan |
| `Q` / `E` | Rotate |
| Mouse wheel | Zoom |
| Home | Frame the map |
| `M` + arrow keys | Adjust mobilization target |
| `1` / `2` / `3` | Show Overview, Soldiers, or Civilians map view |
| `V` | Cycle map views |
| `?` | Toggle the field manual |
| `F3` | Toggle the performance overlay |

A cluster is the full connected set of owned passable cells. Empty owned cells
can connect troop-bearing areas; blocked terrain and impassable elevation edges
split it. Selection reconciles with authoritative ownership: growth and merges
are absorbed, while both surviving children of a split remain selected.

Expansion, attack, and Front Rebalance use Share. Each participating source
cell contributes that percentage of its action-available infantry once:
stationary free strength, but never troops committed to another explicit action.
For expansion and attack, participating sources are only the perimeter cells
that already touch an eligible neutral edge or accepted hostile target. Interior
troops do not move automatically; use Reshape to deploy inland reserves, while
Front Rebalance shifts strength between existing fronts. This
happens once regardless of how many edges or target clusters are involved.
Repeating the exact same contextual click while it is in flight immediately
queues another independent order against the remaining action-available pool;
two 10% clicks on an unchanged pool of 100 therefore commit 10 and then 9.
Expansion allocates a positive all-side baseline when integer strength permits
and applies a small 11/10/9 closer/equal/farther focus weight. Attack
snapshots the accepted enemy-cluster mask and advances through it from all shared fronts;
local branches may turn, split, merge, stall, or be defeated as terrain,
capacity, throughput, garrisons, and defenders are re-evaluated. An attack
never leaves its target mask.

A strategic front is a deterministic arc of deployable boundary edges on one
owned cluster. Hostile runs are separated by opponent; neutral gaps between two
runs against the same opponent keep the arc continuous. Impassable and off-map
edges are not fronts. Press `B`, then drag from an owned source-front cell to a
different target-front cell. The command snapshots Share of movable troops on
the source arc, computes terrain-aware routes once, and persists aggregate
packets that physically traverse those routes. There is no periodic background
rebalance. Fronts have equal strategic importance by default; a larger front
does not silently take a larger global share. Within the chosen target front,
exposed edge count and available capacity distribute the arriving troops, so a
longer arc can use more frontage without changing the player's cross-front
choice. Fronts are currently derived from live topology rather than assigned
durable IDs; a topology change can therefore invalidate an in-progress gesture,
while already accepted packets remain physically conserved.

Reshape is intentionally narrower: exactly one cluster may be selected, then
`T` enables the resizable brush. The complete brush footprint distinguishes
available cells, unavailable in-map cells, and positions outside the world.
Reshape uses the whole available pool, never Share, and makes a best-effort
transition through owned passable cells. A fitting drawing can drain movable
strength outside it; an undersized drawing saturates and leaves conserved
overflow outside. Unrelated live allocations remain fixed.

`X` freezes the exact explicit dispatches intersecting the selected clusters.
Confirming Stop releases only those surviving explicit allocations at their
current physical cells. It does not rewind captures or casualties. Normal
cluster selection never implicitly retasks an existing explicit order.

The older painted sub-cluster Push Front, one-shot Formation/Bias, and contested
retask-handle grammar are retained only as implementation history. They are not
the primary V1 controls.

## Verification

Run the complete local check set with:

```bash
cargo fmt --all -- --check
cargo test --workspace --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --manifest-path modules/match/Cargo.toml
cargo clippy --manifest-path modules/match/Cargo.toml --all-targets -- -D warnings
spacetime build --module-path modules/match
```

The headless smoke tool defaults to the isolated `of-match-e2e` database so it
cannot claim the interactive `of-match-dev` player slots. Freshly publish that
test database before running it; this deletes only prior E2E state:

```bash
spacetime publish --server local --module-path modules/match \
  --delete-data=always --yes of-match-e2e
cargo run -p match-e2e
```

The live smoke exercises the public cluster-action reducer surface, conserved
branching movement, capacity-safe best-effort Reshape, exact Stop, and
identity-token reuse. Deterministic module and client tests cover complete
cluster selection, focused all-side expansion, immutable enemy target masks,
multi-front attack progression, Share-once accounting, strategic-front
rebalancing, explicit-allocation isolation, and exact Stop.

Generate inspectable maps at a chosen player scale with, for example,
`cargo run -p mapgen -- --preset playtest --players 8`. For a short live scaling
profile, freshly publish an isolated database and run the distributed-capable
`match-perf` harness (it never overwrites an existing run directory):

```bash
spacetime publish --server local --module-path modules/match \
  --delete-data=always --yes of-match-perf
cargo run -p match-perf -- run-local --database of-match-perf --preset playtest \
  --players 4 --shard-size 2 --expand-steps 20 --rebalance-steps 20 --attack-steps 0
```

`run-local` spawns one coordinator telemetry observer plus worker process shards.
`coordinator` and `worker` can also run on different hosts. Outputs land in a new
`match-perf-runs/…` directory: `timeline.csv`, long-form `players.csv`, per-shard
`worker-*.jsonl`, `metadata.json`, and `summary.json`. Prefer logical-step phase
durations for distributed synchronization; wall-second aliases remain available.
Required expansion commands fail the run if rejected. The `--rebalance-steps`
phase issues `issue_front_rebalance` (exact owned component + two strategic
front seeds); seats without two usable fronts are skipped and counted rather than
sent invalid commands. An optional attack phase fails unless each player has a
real adjacent owned/enemy front. Do not point the profiler at an in-progress
database: the coordinator calls one-shot `configure_match` before workers join.
The destructive matrix script stays headless by default
(`./scripts/run-match-perf-matrix.sh --confirm-destructive-matrix`); pass
`--viewer` (or `OF_PERF_VIEWER=1`) to attach one Bevy client as player 1. See
[Performance profiling](docs/performance.md) for multi-host usage, matrix
env vars, OS connection limits at 500 clients, and the authority caveat (client
load is distributed; simulation is not).

## Repository map

- `crates/hex-core` — deterministic coordinates, traversal, routing, branching,
  movement, combat, connectivity, redistribution, and Conquest rules.
- `crates/worldgen` — deterministic map generation and validation.
- `crates/match-bindings` — generated SpacetimeDB Rust wire contract.
- `crates/game-client` — native Bevy rendering, input, UI, and transport.
- `modules/match` — authoritative SpacetimeDB schema, reducers, and scheduler.
- `tools/mapgen` — curated map generator/validator CLI.
- `tools/match-e2e` — real-server two-client acceptance smoke test.
- `tools/match-perf` — distributed-capable live-match load driver and step-rate profiler.
- `docs` — the game design, architecture, UI direction, implementation notes,
  performance profiling, and deliberately deferred ideas.

Start with [the V1 game design](docs/v1-game-design.md) and
[the implementation guide](docs/implementation.md). Live scale measurement is
covered in [Performance profiling](docs/performance.md). Deferred mechanics such as
roads, cities and economy, fog of war, armor, naval/air play, mutable terrain,
diplomacy, and demobilization are preserved in
[the future backlog](docs/future-ideas.md).
