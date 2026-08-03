# V1 Game Design

Status: implemented V1 gameplay baseline, ready for playtesting. Values explicitly marked **provisional** are expected to change through playtesting.

## Vision

Build a native two-player RTS on a stepped 2.5D hex world. The player controls territory and the movement and distribution of aggregate forces, not individual foot soldiers.

The defining mechanic is **spatially conserved troop flow**:

`local population -> local mobilization -> aggregate troop movement -> local front strength -> territorial advance`

Army strength cannot be reassigned instantly from one border to another. Troops have a location, routes take time, destinations have finite capacity, and terrain creates bottlenecks. This should preserve high-level RTS control while making geography and logistics matter.

V1 exists to answer one question: is selecting regions, moving aggregate strength, redistributing it, and fighting over a height-aware hex map understandable and fun?

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

For a simple transfer step:

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
- transfers cannot cross the lost connection;
- the pocket can defend, attack outward, and attempt to reconnect;
- reconnection permits movement again but does not instantly equalize strength.

There is no separate out-of-supply damage, disappearance, morale debuff, or HQ-connectivity penalty in V1. Isolation emerges from the spatial population and troop model.

## Player Orders and UX Experiments

The underlying order semantics are locked; the final gesture and presentation are not.

### Source-to-destination transfer

The first interaction to test is:

1. Select or paint a set of owned source hexes.
2. Inspect their available movable strength.
3. Select or paint destination hexes.
4. Choose an amount or percentage to move.
5. Preview routes, destination capacity, congestion, excluded cells, and ETA.
6. Confirm the aggregate transfer.

The server distributes the requested strength across reachable destinations up to their capacities. Unreachable sources or destinations remain excluded and visibly explained. A neutral or hostile destination causes forces to stage at and feed the relevant contested border rather than travel through unowned territory.

The exact selection gestures, amount controls, path presentation, and confirmation flow are provisional and should be iterated in playtests.

### Next experiment: all-front neutral expansion

The target-based transfer is intentionally precise, but it is probably too
deliberate to be the only way to cross the large amount of unclaimed land at
the beginning of a match. The next gameplay experiment should add a fast
**Expand** action alongside transfers rather than replacing them.

An Expand activation is a one-shot pulse over every traversable edge from the
player's territory into neutral, unoccupied land:

- it never attacks or crosses into enemy-owned territory;
- every disconnected component expands using only the infantry physically
  present on its own frontier;
- each frontier hex commits a player-selected share of its movable infantry,
  while retaining a provisional local reserve;
- a source facing several neutral edges divides its commitment instead of
  duplicating it;
- cliffs, water, full destinations, and other impassable edges are excluded;
- issuing another pulse while one is active should adjust or renew the
  commitment, not stack click-speed bonuses.

The pulse creates expansion fronts, not instant ownership changes. Each front
accumulates capture progress from its committed local infantry. Effective
progress is bounded by edge throughput/frontage and modified by terrain and
elevation, so additional infantry creates more momentum only until the local
edge is saturated. When a neutral cell is captured, occupying infantry must be
able to enter it and a small garrison is left behind. Remaining committed
strength may continue into the next neutral edge. The wave therefore slows as
it widens, crosses difficult terrain, fills capacity, or spends strength
holding newly captured ground.

This keeps spatial conservation intact: "momentum" is an observable consequence
of local committed troops and geography, not a second army value or a global
expansion currency. If both players reach the same neutral cell, the first
front to complete capture progress claims it. An exact same-step tie leaves the
cell neutral and stops both pulses there until a targeted order resolves the
contest, avoiding an arbitrary player-slot priority. Any later arrival that
finds enemy ownership also stops as an expansion pulse and leaves explicit
attack rules to resolve the conflict.

The first UX prototype should make Expand a single action with an adjustable
commitment percentage and an immediate frontier/momentum visualization. Whether
that action ultimately uses a keyboard shortcut, a HUD button, or a contextual
left-click is deliberately open because ordinary left-drag currently owns
region selection.

This produces three useful levels of control:

1. **Expand** for quick, neutral-only growth across all local fronts.
2. **Transfer and redistribution** for moving real strength between regions.
3. **Targeted attack** for deliberately crossing an enemy border.

The experiment belongs close to V1 because it changes the cadence of the core
loop. Its exact capture threshold, reserve, continuation, and input gesture are
playtest values rather than locked rules.

### One-shot redistribution

A redistribution order applies a target density pattern to selected owned hexes. Surplus strength moves toward deficits through the ordinary route, throughput, and capacity rules; redistribution is never instantaneous.

V1 should test at least:

- **Balance**: equalize occupancy ratio (`strength / capacity`) over the selection.
- **Front-load**: bias target density along a player-specified direction.

For a directional preset, the player selects a region and drags an orientation arrow. Each selected hex receives a weight based on its projection along that direction; the preview shows the resulting target-density heatmap. The direction is captured when the one-shot order is issued and does not rotate automatically as the border changes.

Possible future presets include rear reserve, center concentration, flank concentration, fill-to-percentage, and corridor weighting. Persistent policies such as “maintain 300 strength here” are deferred until one-shot orders are understood; they must not be necessary for V1.

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

Exact lethality, neutral resistance, retreat behavior, capture timing, multi-edge allocation, and elevation coefficients are provisional. The first model should be deterministic, inspectable, and easy to tune rather than feature-rich.

## Presentation

V1 uses intentionally simple graybox graphics:

- generated stepped hex meshes;
- flat terrain and ownership colors;
- simple lighting;
- clear borders and selection highlights;
- route arrows, transfer direction, queues, and ETA;
- occupancy-versus-capacity heatmaps;
- force-density shading.

Density shading is a core strategic view, not just a debugging aid. At medium and far zoom it should communicate where force is concentrated even before representative 3D assets exist.

Later, the Bevy client may render a small deterministic sample of representative infantry, tank, or artillery models based on scalar composition. Those models are presentation only and are never authoritative individual units. Close, medium, and far zoom levels may use different representations without changing simulation state.

## Explicit V1 Non-goals

- Individual foot-soldier or squad selection and simulation.
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
- source/destination and redistribution UX details.

## V1 Acceptance Criteria

The vertical slice is ready for gameplay evaluation when all of the following are true:

1. Two human players can join and finish a Conquest match on a curated stepped island map.
2. The match ends when one player reaches 80% of the fixed capturable-land denominator; there is no HQ elimination.
3. The same rules work on variable map sizes, with a 128 x 128 playtest map and a representative 192 x 192 validation map.
4. Terrain elevation visibly affects traversal and uphill combat, and cliffs block ordinary ground movement.
5. Civilian population grows locally and a global mobilization target converts it into local infantry strength over time.
6. Lowering the mobilization target does not instantly demobilize existing force.
7. Players can issue a source-region-to-destination-region transfer and inspect route, ETA, destination capacity, and congestion before confirmation.
8. Transfers obey spatial conservation, capacity, and edge throughput; no command can teleport or duplicate strength.
9. Cutting a corridor creates genuinely independent connected components whose existing population and forces remain usable locally but cannot transfer across the cut.
10. Players can apply one-shot Balance and oriented Front-load redistribution orders, preview their target densities, and watch force physically redistribute.
11. Combat is resolved across contested edges using frontage, elevation, capacity, and casualties, including attacks from more than one edge without double-counting defenders.
12. Ownership changes through combat and expansion, and all authoritative state is fully visible to both players.
13. Graybox overlays make ownership, force density, occupancy/capacity, transfers, queues, fronts, and blocked orders understandable without production assets.
14. Core tuning values are configurable so playtests can adjust the model without changing its data or interaction foundations.

## Questions for Playtesting, Not Pre-production Blockers

- Does direct source-to-destination movement feel natural, or should persistent target-density orders eventually become primary?
- Does an all-front Expand pulse make the opening flow naturally without
  removing the value of spatial logistics or rewarding click spam?
- Which selection gestures make irregular source and destination regions easy to express?
- Is the local civilian-to-military mobilization model understandable before an explicit economy is introduced?
- Do capacity, throughput, and frontage create clear bottlenecks rather than frustrating queues?
- Does Balance solve post-push cleanup, and does an oriented density preset provide enough control?
- What map density and match duration best expose troop travel time without creating long periods of inactivity?
- Is combat readable and sufficiently predictable while still rewarding elevation and multi-edge attacks?
