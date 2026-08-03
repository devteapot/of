# Hex RTS V1

A native two-player RTS prototype about moving conserved aggregate forces across
a stepped 2.5D hex world. Players paint source and destination regions instead
of selecting individual soldiers. Terrain height, military capacity, edge
throughput, combat frontage, and travel time determine where pressure can be
applied.

The project intentionally has no settled title, fiction, or production art yet.
V1 is a graybox built to test the troop-flow and redistribution loop.

## What V1 includes

- Two human players in one authoritative match; no bots or NPC factions.
- Conquest victory at 80% of a fixed capturable-land denominator.
- Deterministic 64×64, 128×128, and 192×192 stepped-island map presets.
- Per-hex civilians, infantry, civilian capacity, and military capacity.
- A global mobilization target that converts population locally over time.
- Source-to-destination transfers with spatial conservation and congestion.
- One-shot Balance and oriented Front-load redistribution.
- Height-aware movement, impassable cliffs, uphill combat penalties, edge
  frontage, casualties, capture, and disconnected pockets.
- A native Bevy client with chunked 3D terrain, density shading, selection,
  route/front overlays, HUD, inspector, previews, rejections, and reconnectable
  SpacetimeDB profiles.
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

   For later module updates that must preserve an in-progress match, use
   `./scripts/publish-local.sh` without arguments instead.

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
| `T` | Enter destination painting for a transfer |
| `[` / `]` | Lower or raise transfer percentage |
| `B` | Preview Balance over the selected region |
| Hold `F`, drag, release | Orient and preview Front-load |
| Enter | Confirm the current preview |
| Escape | Cancel the current mode; in idle, clear selection |
| Middle drag or Space + left drag | Pan |
| `W` `A` `S` `D` | Pan |
| `Q` / `E` | Rotate |
| Mouse wheel | Zoom |
| Home | Frame the map |
| `M` + arrow keys | Adjust mobilization target |
| `?` | Toggle help |
| `F3` | Toggle the performance overlay |

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

- `crates/hex-core` — deterministic coordinates, traversal, routing, movement,
  combat, connectivity, redistribution, and Conquest rules.
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
