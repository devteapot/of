# Hex RTS V1

A native two-player RTS prototype about moving conserved aggregate forces across
a stepped 2.5D hex world. Players select a connected owned region, then either
push its eligible boundary arcs in one chosen direction or expand across every
neutral boundary instead of selecting individual soldiers or painting
destination cells. Terrain
height, military capacity, edge throughput, combat frontage, garrisons,
resistance, and travel time determine how far each Push lane or wave branch
advances.

The project intentionally has no settled title, fiction, or production art yet.
V1 is a graybox built to test the Push Front, troop-flow, and redistribution
loop.

## What V1 includes

- Two human players in one authoritative match; no bots or NPC factions.
- Conquest victory at 80% of a fixed capturable-land denominator.
- Deterministic 64×64, 128×128, and 192×192 stepped-island map presets.
- Per-hex civilians, infantry, civilian capacity, and military capacity.
- A global mobilization target that converts population locally over time.
- Sustained directional Push Front orders with spatial conservation,
  lane-by-lane resistance, congestion, and manual cancellation.
- Neutral-only Expand All orders that dispatch one selected percentage, route
  strength outward through a branching selected-region flow, merge converging
  contributions, and continue as an independently advancing perimeter wave.
- Percentage-aware one-shot Balance, oriented Front-load, Core-load, and
  Perimeter-load redistribution.
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

## Local two-player match

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
   sustained-Push, Expand All, and redistribution API cutover changes the
   persisted schema and reducer surface, so an older local database must be
   recreated with the fresh command above.

3. Before either player joins, optionally choose a map preset. The development
   map is already the default:

   ```bash
   spacetime call --server local of-match-dev configure_map '{"playtest128":{}}'
   ```

   Other values are `dev64` and `validation192`.

4. Start player one and player two with distinct persistent profiles:

   ```bash
   OF_PLAYER=1 OF_NAME="Player One" OF_PROFILE=player-one \
     ./scripts/run-client.sh
   ```

   ```bash
   OF_PLAYER=2 OF_NAME="Player Two" OF_PROFILE=player-two \
     ./scripts/run-client.sh
   ```

The match begins when both slots are claimed. Profile tokens are stored below
the ignored `.spacetime-data/` directory; reuse the same profile to reclaim a
slot after reconnecting. If a different identity tries to enter a fully claimed
match, the client remains an unbound observer and reports the exact slot error;
start a fresh match with the reset command above rather than stealing a
disconnected identity. The defaults are `http://127.0.0.1:3000` and
`of-match-dev` and can be overridden with `OF_HOST` and `OF_DATABASE` (or the
`--host` and `--database` arguments).

## Controls

| Input | Action |
| --- | --- |
| Left drag | Paint cells |
| Shift + left drag | Add to the current region |
| Control + left drag | Remove from the current region |
| `[` / `]` while selecting | Remove or add one complete hex ring around the brush |
| Shift + `[` / `]` | Change brush width only |
| Control + `[` / `]` | Change brush height only |
| `C` | Select the connected owned cluster under the cursor |
| Shift / Control + `C` | Add or remove that cluster |
| Control/Command + `A` | Select all locally owned hexes |
| Hold `P`, drag outward, release | Preview a sustained push from the selected front in one exact hex direction |
| Click `P PUSH FRONT`, then click outward on the map | Mouse-only alternative for orienting the same Push Front preview |
| Shift + `P` or click `EXPAND ALL` | Preview every eligible neutral edge around the selected region |
| `[` / `]` during an order preview | Lower or raise the troops dispatched/participating in that order; this is separate from mobilization |
| `B` | Preview Balance over the selected region |
| Hold `F`, drag, release | Orient and preview Front-load |
| `G` | Preview Core-load toward the selected region's center |
| `R` | Preview Perimeter-load toward the selected region's outside rings |
| `X` during a Push Front or Expand All preview | Cancel matching active operations from that selection |
| Enter | Confirm the current preview |
| Escape | Cancel the current mode; in idle, clear selection |
| Middle drag or Space + left drag | Pan |
| `W` `A` `S` `D` | Pan |
| `Q` / `E` | Rotate |
| Mouse wheel | Zoom |
| Home | Frame the map |
| `M` + arrow keys | Adjust mobilization target |
| `1` / `2` / `3` | Show Overview, Soldiers, or Civilians map view |
| `V` | Cycle map views |
| `?` | Toggle help |
| `F3` | Toggle the performance overlay |

After a Push direction is chosen, selected cells that face neutral or enemy
territory in that direction are the front. Selection geometry or target
eligibility may split that front into several disconnected arcs; every arc is
an independent outward seed. The other selected cells are their reinforcement
corridor, and every selected cell must reach at least one front cell through
traversable selected-only edges. The
committed percentage becomes a fixed pool that feeds independent straight
lanes; terrain, elevation, throughput, frontage, resistance, and terrain-scaled
garrisons determine how far each lane travels. Contested cells keep one
authoritative controller while their terrain color blends controller and
attacker pressure for readability.

Expand All uses the same connected selection without an orientation. Every
selected cell contributes the chosen dispatch percentage of its currently
unallocated soldiers once. Inside the selection, each local pool splits evenly
among every traversable neighbor one depth closer to an eligible neutral
boundary; contributions merge wherever those branches meet. Boundary pools
then split across their neutral exits, and repeat the same split-and-merge rule
through successive outward perimeter layers. Local branches advance
independently, so terrain, throughput, capacity, and garrison costs may make the
wave bulge rather than form a globally even ring.
Expand All never attacks: a branch stops before enemy territory. Use directional
Push Front when enemy contact or a deliberate direction is intended. The
mobilization slider controls future civilian recruitment and does not change
the percentage dispatched by an order.

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

## Repository map

- `crates/hex-core` — deterministic coordinates, directional-front selection,
  traversal, routing, movement, combat, connectivity, redistribution, and
  Conquest rules.
- `crates/worldgen` — deterministic map generation and validation.
- `crates/match-bindings` — generated SpacetimeDB Rust wire contract.
- `crates/game-client` — native Bevy rendering, input, UI, and transport.
- `modules/match` — authoritative SpacetimeDB schema, reducers, and scheduler.
- `tools/mapgen` — curated map generator/validator CLI.
- `tools/match-e2e` — real-server two-client acceptance smoke test.
- `docs` — the game design, architecture, UI direction, implementation notes,
  and deliberately deferred ideas.

Start with [the V1 game design](docs/v1-game-design.md) and
[the implementation guide](docs/implementation.md). Deferred mechanics such as
roads, cities and economy, fog of war, armor, naval/air play, mutable terrain,
diplomacy, and demobilization are preserved in
[the future backlog](docs/future-ideas.md).
