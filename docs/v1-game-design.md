# V1 Game Design

Status: implemented V1 gameplay baseline, ready for playtesting. Values explicitly marked **provisional** are expected to change through playtesting.

## Vision

Build a native two-player RTS on a stepped 2.5D hex world. The player controls territory and the movement and distribution of aggregate forces, not individual foot soldiers.

The defining mechanic is **spatially conserved troop flow**:

`local population -> local mobilization -> aggregate troop movement -> local front strength -> territorial advance`

Army strength cannot be reassigned instantly from one border to another. Troops have a location, routes take time, destinations have finite capacity, and terrain creates bottlenecks. This should preserve high-level RTS control while making geography and logistics matter.

V1 exists to answer one question: is selecting regions, pushing connected front
sections, redistributing aggregate strength, and fighting over a height-aware
hex map understandable and fun?

## Locked V1 Scope

### Match and victory

- A match has exactly two human players.
- There are no bots, NPC factions, nations, or tribes.
- The initial game mode is **Conquest**.
- The first player to control 80% of capturable land wins.
- The conquest denominator is fixed when the match starts. It excludes water and any permanently inaccessible or decorative cells.
- A time limit is optional and disabled by default. If enabled later, its tiebreak or draw rule belongs to the game-mode configuration.
- There is no defeat-critical HQ in V1. Capturing a particular starting structure does not immediately eliminate a player.

### World and map

- One visible terrain hex is one authoritative gameplay hex. There is no hidden finer grid and no larger territory grid.
- A hex is the smallest address for elevation, terrain, ownership, population, force occupancy, movement, and combat.
- Chunks and connected regions are derived groupings for simulation, rendering, and tools; they are not additional gameplay grids.
- Maps may have different dimensions and land densities. Systems must not assume one fixed width or height.
- V1 maps are selected from seeded, procedurally generated candidates and baked for playtesting. Match-time terrain generation is not required.
- The initial map library may include islands, mountains, sea, rivers, and lakes. Maps must provide ground crossings wherever required to keep Conquest achievable without naval units.
- Terrain is immutable during a V1 match.
- The map is fully visible. There is no fog of war.

### Elevation and traversal

- Elevation is discrete and visually represented by stepped hex columns.
- A normal ground force can traverse a level edge or a one-level slope.
- Moving uphill by one level is slower and penalizes the attacker in combat.
- An elevation change greater than one level is a cliff and is impassable to normal ground forces.
- Traversal is evaluated per edge so later mobility profiles can support armor, bridges, climbing, tunnels, naval movement, or air movement without replacing the grid model.

The total number of elevation levels and the precise movement/combat modifiers are provisional.

## Population and Forces

### Per-hex state

Each owned, habitable hex can hold scalar state:

- civilian population;
- civilian population capacity;
- force composition;
- military capacity.

V1 may use only infantry strength, but force state must be composition-shaped so future types such as armor or artillery can use different movement, capacity, upkeep, and combat weights. These values represent aggregate strength, not individually simulated people or vehicles.

### Local mobilization

- New troops originate where civilian population is located; they do not appear in a global reserve or teleport from an HQ.
- A player controls one global target mobilization percentage.
- Raising the target gradually converts civilian population into military strength locally, subject to local military capacity and the mobilization rate.
- Lowering the target slows or stops further recruitment. It does not instantly convert existing soldiers back into civilians.
- Civilian population recovers slowly toward local population capacity.
- A disconnected region retains its local civilians and can continue local recruitment if its population and military capacity permit it.

### Economic consequence is deferred

V1 models the conversion between civilian population and military strength, but it does not yet model civilian economic output or military upkeep. A mobilized citizen is no longer part of the civilian population, which preserves the state needed for the later economy without adding an economic resource to the first gameplay test.

After V1, civilians should produce economic value while soldiers both remove civilian labor and impose ongoing upkeep. This is intended to make mobilization a careful balance between economic capacity and military manpower, not merely a choice between two display numbers. Explicit demobilization can later return surviving military population to the civilian economy.

## Density, Movement, and Congestion

### Three related limits

The simulation distinguishes:

- **Hex capacity (`C`)**: how much weighted military strength may occupy a hex.
- **Edge throughput (`Q`)**: how much weighted strength may cross an edge per unit of time.
- **Combat frontage (`W`)**: how much weighted strength may actively fight across an edge at once.

They use the same abstract force scale but are not interchangeable. This separation allows a dense city behind a narrow gate, or a fast road that improves logistics without automatically increasing combat power.

For a simple movement step:

`moved <= min(source strength, destination free capacity, edge throughput * elapsed time)`

Baseline edge values may be derived from the adjacent hex capacities and then modified by terrain and elevation:

`Q_base = min(C_from, C_to) * movement rate`

`W_base = min(C_from, C_to) * frontage ratio`

- A destination can never exceed its military capacity.
- Force that cannot pass immediately queues on the source side of the bottleneck.
- Roads, paths, bridges, cities, and forts are future content, but the capacity/throughput/frontage model must be able to express their effects.
- A force type may consume weighted capacity and throughput differently in the future.

### Spatial conservation

Every unit of military strength is always in exactly one state:

- present in a hex;
- in transit on a route; or
- removed as a casualty.

Orders never teleport strength or create hidden global army pools. Switching pressure between distant fronts must incur route and travel time.

If territory is cut into disconnected components:

- each component keeps the civilians and forces physically inside it;
- forces may redistribute within that component;
- troop movement cannot cross the lost connection;
- the pocket can defend, attack outward, and attempt to reconnect;
- reconnection permits movement again but does not instantly equalize strength.

There is no separate out-of-supply damage, disappearance, morale debuff, or HQ-connectivity penalty in V1. Isolation emerges from the spatial population and troop model.

## Player Orders and UX Experiments

### Push Front: the primary conquest interaction

V1 does not expose destination-cell painting as its conquest loop. The player
selects the territory and troops that may participate, then chooses exactly one
of the six hex directions in which its boundary should advance:

1. Paint one connected owned source region. Include the intended border section
   and continue backward into owned territory to include its reinforcement
   corridor and troop pool.
2. Hold `P`, drag outward, and release. The client quantizes the gesture to one
   exact axial direction.
3. Preview the exact initial active edges, selected routes, participating
   strength, commitment, capacity, congestion, and any invalid condition.
4. Use `[` / `]` to change commitment, `Enter` to submit, or `Escape` to
   cancel while retaining the selected region.

For every selected cell, only its neighbor in the chosen direction is
considered. A selected cell whose outward neighbor is neutral or enemy
territory is part of the **front**, and the corresponding initial active edge
is exactly `(selected source, source + direction)`. Side and rear edges never
join the command implicitly. The remaining selected cells are the
**reinforcement corridor**: they must connect behind the front through the
six-connected selected region, but they do not open outward lanes of their own.
The front cells must form one connected section. Initial targets must be
capturable and traversable. The same command expands into neutral land or
attacks an enemy-held cell; ownership determines the resolution, not a separate
targeting mode.

Cells behind the boundary contribute only through routes that stay inside the
submitted selection until they reach an initial front cell. A cell outside the
selection cannot become a convenient shortcut into the front, and an
unselected friendly stack cannot be debited. Internal cliffs or other blocked
edges that split the selected reinforcement corridor make the command invalid
rather than silently changing its meaning.

After crossing an initial front edge, the committed force continues
automatically through successive hex layers along that lane's exact axial ray.
It does not spread sideways, bend toward an easier target, or absorb newly
adjacent friendly stacks. Each lane has a fixed committed strength pool and
resolves independently. Terrain and elevation affect travel, throughput and
combat; frontage limits engagement; neutral or enemy forces resist; and every
captured cell retains a terrain-scaled garrison that consumes momentum. A lane
stops when its mobile pool is exhausted, it is blocked or defeated, it reaches
the map edge, or the player manually cancels it. This creates momentum from the
troops physically committed to the selected corridor rather than click-speed
bonuses or a separate expansion resource.

The authoritative `issue_push_front` reducer receives a stable command ID, the
exact sorted selected cell IDs, one axial direction, and commitment in basis
points. It revalidates ownership, source connectivity, active-front
connectivity, traversal, available unallocated infantry, target capacity, and
selected-only routes in one transaction. Accepted strength becomes ordinary
aggregate transit packets; rejected commands create a receipt without partial
gameplay mutation.

Commitment is a one-time share of the currently unallocated strength in each
selected source when the command is accepted. Existing allocations reduce the
available base; the percentage is never multiplied by the number of front
edges. Submitting another command is a new allocation decision and applies to
what remains unallocated at that later moment. Destination reservations are
scoped to the issuing player; another player's pending attack cannot pre-claim
neutral capacity before the forces physically meet.

With the same front selection and direction previewed, `X` cancels matching
active Push Front orders. Cancellation releases their remaining allocations at
the cells where they currently exist; it does not rewind captures, return force
to its original source, or erase casualties.

### Expand All: neutral opening expansion

Expand All applies the same spatial commitment model to every eligible neutral
boundary around one connected owned selection. Use Shift+`P` or the HUD button
to preview it, plain `[` / `]` to set the dispatch percentage, `Enter` to start,
and `X` from the same preview to stop matching operations. It requires no
orientation and never includes an enemy-held target; directional Push Front
remains the command for attacks.

Each selected cell snapshots the chosen share of its currently unallocated
infantry exactly once. It is not multiplied by the number of adjacent edges.
Rear cells route within the selected region to their nearest eligible neutral
boundary. Where one boundary cell has several outward exits, their combined
local pool is divided as evenly as integer strength permits before source
contributions fill those lane quotas. This makes each fork use the same effort
without letting rounding repeatedly favor one exit.

Every unique first target becomes a stable lane anchor. Concave boundaries that
reach the same outside target are deterministically deduplicated, then each
lane continues along its own initial axial direction under the same capacity,
throughput, elevation, and terrain-scaled garrison rules as Push Front. A lane
may cross already-friendly traversable cells without recapturing or
regarrisoning them, which prevents intersecting independent rays from blocking
one another. It stops independently when its mobile pool is exhausted, blocked,
reaches the map edge, is cancelled, or would enter enemy territory. Ownership
is rechecked during execution, so a neutral target that becomes hostile is not
attacked by an already-issued Expand All operation.

The dispatch percentage belongs to the current order. It is deliberately
separate from the global mobilization target, which governs future conversion
of civilians into soldiers. This distinction is shown in the order panel and
help text.

The generic aggregate transfer machinery remains a useful implementation
substrate for routes, queues, congestion, and delivery, but V1 has no public
precise-infantry-transfer command. Exact cell targeting is reserved for possible
future discrete units such as tanks or boats.

### One-shot redistribution

A redistribution order applies a target density pattern to selected owned
hexes. Each order also has a participation percentage. For every selected cell,
the unparticipating share of its current stack is frozen as a per-cell lower
bound; only the participating share joins the redistribution pool. Surplus
strength moves toward deficits through the ordinary route, throughput, and
capacity rules, so redistribution is never instantaneous.

V1 includes four presets:

- **Balance**: equalize occupancy ratio (`strength / capacity`) over the selection.
- **Front-load**: bias target density along a player-specified direction.
- **Core-load**: bias target density inward toward the selected region's
  geometric center.
- **Perimeter-load**: bias target density outward toward the selected region's
  outer rings.

For Front-load, the player selects a region and drags an orientation arrow.
Each selected hex receives a weight based on its projection along that
direction; the preview shows the resulting target-density heatmap. The
direction is captured when the one-shot order is issued and does not rotate
automatically as the border changes. Core-load and Perimeter-load use distance
from the selected cells' geometric center, so they need no orientation. Plain
`[` / `]` adjusts participation for every preset before submission.

Possible future presets include rear reserve, flank concentration,
fill-to-percentage, and corridor weighting. Persistent policies such as
“maintain 300 strength here” are deferred until one-shot orders are understood;
they must not be necessary for V1.

## Combat Placeholder

V1 combat is aggregate and edge-based:

- An attack occurs when force is directed through a traversable edge into neutral or enemy territory.
- Only strength within the edge's combat frontage can participate at once.
- Remaining attackers wait behind the active frontage and continue feeding the battle subject to throughput.
- Defending force is local to its hex. When attacked through multiple edges, the same defenders cannot be counted at full strength against every edge.
- Uphill attackers receive a clear penalty.
- Casualties remove force from the spatial-conservation total.
- Ownership changes only after local resistance is overcome and occupying force can enter the destination within its capacity.
- Multiple attack edges should make encirclement valuable by creating additional frontage, without duplicating defending strength.
- A cell always has one authoritative controller and one authoritative local
  infantry stack. Opposing forces remain on hostile edges until capture; V1
  does not introduce dual occupancy or fractional ownership.

Exact lethality, neutral resistance, retreat behavior, capture timing, multi-edge allocation, and elevation coefficients are provisional. The first model should be deterministic, inspectable, and easy to tune rather than feature-rich.

## Presentation

V1 uses intentionally simple graybox graphics:

- generated stepped hex meshes;
- flat terrain and ownership colors;
- simple lighting;
- clear borders and selection highlights;
- selected-only push routes, exact active front edges, queues, and ETA;
- redistribution target-density heatmaps;
- pressure-blended contested-cell colors derived from active combat fronts;
- absolute force-strength shading, with alternate civilian and ownership
  overview modes.

Force shading is a core strategic view, not just a debugging aid. It represents
the absolute scalar strength held in each hex rather than the percentage of
that hex's capacity. At medium and far zoom it should communicate where force
is concentrated even before representative 3D assets exist; close zoom may add
exact compact totals without changing the authoritative model.

Later, the Bevy client may render a small deterministic sample of representative infantry, tank, or artillery models based on scalar composition. Those models are presentation only and are never authoritative individual units. Close, medium, and far zoom levels may use different representations without changing simulation state.

## Explicit V1 Non-goals

- Individual foot-soldier or squad selection and simulation.
- Precise cell-to-cell commands for aggregate infantry; exact movement may be
  revisited for future discrete vehicles or vessels.
- Bots or NPC AI.
- More than two players, teams, diplomacy, alliances, or betrayal.
- Fog of war.
- A defeat-critical HQ or capital-capture victory condition.
- Game modes other than Conquest.
- Runtime terrain mutation, terraforming, or destruction.
- Roads, paths, buildable cities, depots, forts, bridges, or other infrastructure gameplay.
- Tanks, discrete multi-hex vehicles, artillery, naval combat, or air combat.
- Technology trees.
- Full economic resources, production chains, trade, migration, jobs, happiness, training, evacuation, or demobilization.
- Persistent target-density policies or automatic front management.
- Browser delivery, matchmaking, progression, or production operations.
- Production art, asset generation, or a settled fiction/theme.

These are scope exclusions, not permanent rejections. In particular, infrastructure, fog of war, mutable terrain, armor, naval and air forces, diplomacy, population logistics, economic depth, and demobilization are recorded as post-V1 directions.

## Provisional Starting Values

These values exist only to make the first implementation executable and must live in configuration:

| Parameter | Initial experiment |
| --- | ---: |
| Baseline hex military capacity | 100 strength |
| Baseline edge throughput | 20 strength/second |
| Baseline edge combat frontage | 25 strength |
| Fast developer map | 64 x 64 cells |
| First small-island playtest | 128 x 128 cells |
| Representative acceptance map | 192 x 192 cells |
| Stretch/load map | 256 x 256 cells |

Also provisional:

- elevation level count;
- terrain distributions and map dimensions;
- initial ownership, population, force, and spawn distributions;
- population capacity, growth, and recruitment rates;
- mobilization response speed;
- terrain capacity and throughput modifiers;
- combat lethality, frontage ratio, neutral resistance, and uphill penalty;
- the timer's resolution rule if a timer is enabled;
- Push Front commitment and redistribution UX details.

## V1 Acceptance Criteria

The vertical slice is ready for gameplay evaluation when all of the following are true:

1. Two human players can join and finish a Conquest match on a curated stepped island map.
2. The match ends when one player reaches 80% of the fixed capturable-land denominator; there is no HQ elimination.
3. The same rules work on variable map sizes, with a 128 x 128 playtest map and a representative 192 x 192 validation map.
4. Terrain elevation visibly affects traversal and uphill combat, and cliffs block ordinary ground movement.
5. Civilian population grows locally and a global mobilization target converts it into local infantry strength over time.
6. Lowering the mobilization target does not instantly demobilize existing force.
7. Players can select one connected owned region, hold-drag-release `P` to
   choose one exact direction, and preview one connected active front before
   confirmation.
8. Push Front routes remain inside the exact selection until they feed an
   initial front cell, then advance lane-by-lane along the chosen axial
   direction using one fixed committed pool. Terrain-scaled garrisons consume
   momentum, lanes stop independently, and no command can teleport or duplicate
   strength.
9. Players can preview Expand All over a connected owned selection, choose an
   order dispatch percentage independently of mobilization, and advance every
   eligible neutral boundary with conserved, locally split, independent lanes
   that stop before enemy territory.
10. Cutting a corridor creates genuinely independent connected components whose existing population and forces remain usable locally but cannot transfer across the cut.
11. Players can apply percentage-aware one-shot Balance, oriented Front-load,
    Core-load, and Perimeter-load orders, preview their target densities, and
    watch the participating force physically redistribute while each cell's
    unparticipating share remains in place.
12. Combat is resolved across contested edges using frontage, elevation, capacity, and casualties, including attacks from more than one edge without double-counting defenders.
13. Ownership changes through combat and expansion, and all authoritative state is fully visible to both players.
14. Graybox overlays make ownership, force density, occupancy/capacity, Push
    Front flows, queues, active edges, blocked orders, and contested pressure
    understandable without implying dual occupancy or requiring production
    assets.
15. Core tuning values are configurable so playtests can adjust the model without changing its data or interaction foundations.

## Questions for Playtesting, Not Pre-production Blockers

- Does selecting a front plus its backward reinforcement corridor feel direct,
  and does fixed-pool sustained advancement create understandable momentum
  without excessive input or opaque automation?
- Does Expand All make neutral opening expansion faster without obscuring which
  local troops feed each independent lane or encouraging repeated click spam?
- Is nearest-boundary routing with even local fork splitting understandable,
  or does the preview need stronger lane-allocation feedback?
- Which selection gestures make irregular connected source regions easy to express?
- Is the local civilian-to-military mobilization model understandable before an explicit economy is introduced?
- Do capacity, throughput, and frontage create clear bottlenecks rather than frustrating queues?
- Do Balance, Front-load, Core-load, and Perimeter-load plus participation
  percentage provide enough post-push control without precise infantry micro?
- What map density and match duration best expose troop travel time without creating long periods of inactivity?
- Is combat readable and sufficiently predictable while still rewarding elevation and multi-edge attacks?
