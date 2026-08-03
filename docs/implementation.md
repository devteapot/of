# V1 implementation guide

Status: playable implementation and engineering handoff

This document maps the agreed V1 design onto the code that implements it. The
game rules remain in [the V1 design](./v1-game-design.md); this file describes
the executable boundaries, operational flow, and intentional simplifications.

## Runtime topology

```mermaid
flowchart LR
    A["Bevy client · player 1"] -->|"reducers"| DB["SpacetimeDB match database"]
    B["Bevy client · player 2"] -->|"reducers"| DB
    DB -->|"typed subscriptions"| A
    DB -->|"typed subscriptions"| B
    HC["hex-core"] --> A
    HC --> B
    HC --> DB
    WG["worldgen"] --> DB
    GEN["SpacetimeDB codegen"] --> BIND["match-bindings"]
    BIND --> A
    BIND --> B
```

One SpacetimeDB host can run many databases. V1 manually provisions one logical
database per match; it does not require a host process per match. A future lobby
or orchestrator can create databases and assign players without changing the
match module's authority boundary.

## Authority and cadence

The Bevy client sends intentions only. The match module owns identity slots,
terrain, population, mobilization, ownership, force state, orders, routes,
movement, congestion, combat, capture, victory, receipts, and persistence.
Generated bindings are the typed wire contract and must not be edited manually.

The match starts after both identity-bound player slots are claimed. A private
scheduled table advances the simulation every 250 milliseconds. Movement and
combat operate on active packets/fronts/orders; the provisional one-second
population cadence scans habitable state. Client commands carry stable IDs and
produce public receipts, making retries idempotent. A reconnecting client reuses
its stored SpacetimeDB token, subscribes to a fresh snapshot, reclaims its slot,
and continues above the highest observed command ID.

Every committed infantry point is represented by its current cell plus order
allocation metadata. A logical step must satisfy:

```text
committed = in transit + delivered + casualties
```

Movement also enforces per-cell military capacity, per-edge throughput, and
passability. Combat separately enforces frontage and the uphill modifier.

## Authoritative tables

Public state is split by read/update pattern:

- `player_slot`, `match_config`, `match_state`, and `mobilization_policy` hold
  match-wide state;
- `cell_terrain` is immutable after lobby configuration;
- `cell_state` holds mutable ownership, population, infantry, and capacities;
- `command_receipt` records accepted and rejected idempotent commands;
- `transfer_order`, `transfer_source`, `transfer_destination`, and
  `transit_packet` expose generic aggregate-flow progress and congestion;
- `combat_front` exposes the current contested edges and casualties.

`simulation_schedule` is private. Clients cannot call the scheduled reducer as
a player identity.

## Public reducer surface

- `configure_map` — select Dev64, Playtest128, or Validation192 before joining.
- `join_match` — claim or reclaim one of the two player slots.
- `set_mobilization_target` — change the global target in basis points.
- `issue_push_front` — commit a percentage of one connected selected region
  through its one connected directional front, using routes that stay selected
  until the exact final edge.
- `issue_transfer` — lower-level friendly-territory-only aggregate transfer
  primitive retained for compatibility, testing, and future precision
  logistics; the V1 client does not expose destination painting and the
  reducer rejects unowned destinations.
- `issue_balance` — create a physical one-shot density equalization order.
- `issue_front_load` — create a physical one-shot directional density order.
- `cancel_transfer_order` — release the remaining allocation of an active
  order.

Rejected gameplay commands still create an authoritative receipt with a reason;
they do not partially mutate match state.

## Map contract

The shared generator currently pins three deterministic stepped-island maps:

| Preset | Dimensions | Capturable | Content hash |
| --- | ---: | ---: | --- |
| Dev | 64×64 | 2,395 | `3b9b9767ada36223` |
| Playtest | 128×128 | 9,657 | `894c50e9e590ddb9` |
| Validation | 192×192 | 21,484 | `40a19e2ad4608010` |

Generation validates bounds, completeness, capacity, spawns, land connectivity,
slopes, cliffs, and hash stability. JSON export uses an ordered cell array so it
round-trips without non-string map keys. V1 has no per-edge map overrides;
roads, rivers, bridges, and crossings require an explicit versioned edge array
later.

## Client structure

The native client renders combined chunk meshes rather than one entity per hex.
Each triangle retains its authoritative axial cell so ray picking selects the
visible stepped surface. Ownership hue and the active map-view intensity are
encoded in vertex colors; separate overlays communicate selection, target
density, route, bottlenecks, active flow, and combat fronts.

The default Soldiers view shades cells by absolute infantry strength rather
than capacity occupancy. Overview removes force-dependent luminance, and the
Civilians view shades by absolute civilian population. `1`, `2`, and `3`
select those views directly; `V` cycles them. Stable presentation reference
values keep an unrelated cell's color from changing when the map-wide maximum
changes. At readable zoom levels, one texture-atlas-backed world-space mesh
batches compact authoritative totals directly over their hex tops; there is no
UI node or draw call per cell. Farther out, the vertex-color heatmap carries the
information without trying to draw text for every cell. The label LOD is
viewport-derived and displays the complete readable visible set or hides it,
rather than silently sampling cells when a fixed budget is exceeded.
In Civilians mode, a second single-mesh batch draws thin quads across exposed
edges of six-connected populated land, with most of each strip kept on the
populated hex top. Its scan is limited to visible render chunks and its edge
budget is likewise complete-or-hidden; it does not create a UI element,
material, entity, or gizmo for every populated cell. Camera, projection,
visible-chunk, and cell-state signatures skip both value and perimeter scans on
unchanged frames.

Rendering work is organized by an 8 x 8 client-side spatial index. This is
deliberately independent of the module's 16 x 16 storage/subscription chunks so
the renderer can tune culling granularity without changing authoritative cell
identity. Initial render-chunk creation is amortized, state changes replace
only the affected vertex-color attributes, visible dirty chunks are
prioritized, and hidden chunks converge under a bounded per-frame budget.
The value batch, population outline, blocked overlay, and selected-region
perimeters inspect visible render-chunk cells rather than scanning the whole
map. Selection painting supports a centered odd-sized rectangular core with
complete hex-ring dilation up to 31 x 31 cells, connected local-owned
components, and all-local-owned selection. Width and height remain independently
adjustable while combined resizing adds or removes a true one-cell perimeter.
Large selections are materialized once in V1, but their per-frame outline work
is viewport-bounded and emits only exposed edges. Order previews are keyed by
selection and cell-state revisions. Push Front uses shared deterministic rules
to derive only the exact chosen-direction boundary, then checks reachability
backward through the selected region; it does not perform a route search for
every possible map destination. The retained lower-level transfer preview uses
constant-count multi-source graph traversals. This keeps presentation work tied
primarily to the viewport and real state changes. World-scale maps will still
require symbolic region selections plus chunk-interest subscriptions or
regional summaries so the client does not retain or transmit every
authoritative cell.

Input produces `ClientIntent`. The online transport maps coordinates to
authoritative cell IDs, invokes reducers, pumps SpacetimeDB frames, and rebuilds
`MatchView` from subscribed tables. The offline transport implements the same
message boundary for UI development but is not a rules-equivalent substitute
for the server.

The V1 conquest interaction is Push Front: paint one connected owned region,
hold `P`, drag outward, and release to preview one exact direction. Plain
brackets adjust commitment and `Enter` submits. The server routes only through
the submitted cells before crossing the exact final edge, and each command is
one cell deep so continuing momentum requires repeating the high-level push.
Balance and Front-load remain modal redistribution previews. Exact gestures and
presentation remain playtest material, but painted destination transfer is not
part of the exposed V1 loop.

## Tests and current evidence

- `hex-core` covers coordinates, chunks, traversal, routing, connectivity,
  selected directional-front derivation, capacity, throughput, backpressure,
  conservation, multi-edge combat, redistribution, and exact 80% victory math.
- `worldgen` pins all curated maps, round-trips serialization, rejects invalid
  metadata/duplicates, and sweeps supported custom seeds.
- The module's native tests pin preset metadata; module compilation validates
  the SpacetimeDB schema and WASM target.
- The headless real-server smoke test uses two identities to cover slot claims,
  match start, subscriptions, idempotent receipts, the lower-level aggregate
  flow pipeline, progression, and token-based reconnect.
- The Bevy client has coordinate/fixture/route tests and a native launch smoke.
- CI repeats formatting, workspace tests/lints, module tests/lints/build, and
  generated-binding drift detection.

## Known V1 limits

- Infantry is the only force composition serialized by the module.
- The second join auto-starts the match; there is no lobby UI or ready toggle.
- Routes are deterministic and fixed when an order is accepted; active orders
  do not dynamically replan around later ownership changes.
- Push Front converts the chosen commitment percentage to authoritative basis
  points. Accepted routes are fixed and there are no order priorities.
- Population/mobilization uses a provisional full-state cadence. Movement and
  combat use active sets, but nominal/stress performance gates still need
  representative playtest measurement.
- There is no retreat, morale, explicit supply penalty, HQ defeat, time limit,
  economy/upkeep, demobilization, infrastructure, fog, armor, naval/air force,
  diplomacy, AI, matchmaking, or production art.
- Mobilization already removes people from the civilian population, but V1 has
  no economic output to reduce and no army upkeep to pay. The recorded post-V1
  requirement is that soldiers impose both lost civilian labor and an explicit,
  ongoing economic burden.
- Native desktop is the supported V1 target. Bevy can target the web, but the
  filesystem-backed credential adapter and representative browser performance
  have not been implemented or qualified.

These are bounded omissions, not hidden placeholders. Candidate extensions and
their dependencies are recorded in [future ideas](./future-ideas.md).
