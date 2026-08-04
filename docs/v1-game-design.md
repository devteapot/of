# V1 Game Design

Status: implemented V1 gameplay baseline, ready for playtesting. Values explicitly marked **provisional** are expected to change through playtesting.

## Vision

Build a native two-player RTS on a stepped 2.5D hex world. The player controls territory and the movement and distribution of aggregate forces, not individual foot soldiers.

The defining mechanic is **spatially conserved troop flow**:

`local population -> local mobilization -> aggregate troop movement -> local front strength -> territorial advance`

Army strength cannot be reassigned instantly from one border to another. Troops have a location, routes take time, destinations have finite capacity, and terrain creates bottlenecks. This should preserve high-level RTS control while making geography and logistics matter.

V1 exists to answer one question: is selecting complete territorial clusters,
contextually expanding or attacking with them, choosing persistent density
policies, and fighting over a height-aware hex map understandable and fun?

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
- Raising the target gradually converts civilian population into military strength locally, subject to the mobilization rate and local military capacity not already reserved for an active internal movement destination.
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

## Player Orders and Cluster-first UX

The main control unit is a complete owned traversable cluster, not a painted
sub-region. This keeps the primary decision strategic: choose which territorial
forces participate, which territory to pressure, and how much free infantry to
commit. Direct sub-cluster front surgery remains outside the V1 interaction.

The HUD is keybind-first. A compact strip reports selection, current policy,
Share when relevant, staged targets, preview validity, and submission state.
It does not repeat every command as a button grid. `?` opens the complete field
manual.

### Cluster selection

A cluster is the complete connected set of owned passable cells. Empty owned
cells may connect troop-bearing regions; blocked terrain and impassable
elevation edges split them.

- `C` replaces the selection with the cluster under the cursor.
- Shift+`C` adds a cluster and Control+`C` removes one.
- Control/Command+`A` selects every owned traversable cluster, including empty
  controlled cells.

Selection has no implicit order ownership. It never adopts a live packet,
snapshots a contested retask handle, or cancels an existing action. As
authoritative ownership changes, selected growth and merges are absorbed.
When a selected cluster splits, each still-owned child remains selected.

### Contextual expansion and attack

With one or more source clusters selected, left-clicking the map chooses the
action from the clicked cell's owner.

Clicking unclaimed capturable ground issues **Expand Clusters**. Every selected
source cluster with a reachable neutral perimeter participates across all of
that perimeter. The click is a focus, not a destination. Branches that reduce
distance to it receive weight 3, equal-distance branches weight 2, and branches
moving away weight 1. When the committed integer strength is sufficient, each
eligible branch receives a positive baseline before the weighted remainder.
Terrain, capacity, throughput, elevation, and terrain-scaled occupation
garrisons can make the resulting outline bulge or stall. Expansion never enters
enemy territory.

Clicking an enemy hex issues **Attack Clusters** against that complete enemy
cluster. Every passable front shared by the selected sources and accepted
targets starts simultaneously. Shift-click stages or toggles more complete
enemy clusters, Control-click removes a staged cluster, a plain enemy click adds
its cluster and submits the union, and `Enter` submits an already staged union.

The authority snapshots the complete enemy target union when accepting the
attack. Captures expose the next masked cells, so each local front can turn,
split, merge, stall, or be defeated as geometry changes. There is no global push
vector and no assumption that one direction fits every front arc. A branch never
leaves the accepted target mask or silently attacks a newly adjacent cluster.
Enemy infantry, frontage, terrain, elevation, throughput, capacity, and
garrisons are evaluated authoritatively during progress.

### Force Share and live allocations

One persisted **Share** percentage applies only to Expand Clusters and Attack
Clusters. `[` and `]` adjust it. It is independent of the mobilization
target, which controls future civilian conversion.

On acceptance, each participating source cell contributes Share of its
action-available infantry exactly once: stationary free strength plus yieldable
background-policy strength physically inside the source, excluding troops
committed to another explicit action. A source with no eligible neutral route
or shared enemy front contributes nothing. Multiple exits, shared fronts, or
staged target clusters never multiply the source base. Strength remains
conserved as contributions split and merge.

Infantry committed to any live action packet is unavailable to a later action.
Selecting the same cluster again does not retask, supersede, or double-allocate
it. The player must explicitly stop that order if its surviving strength should
become free.

### Persistent cluster policy

Every cluster has one density policy:

- **Balanced** evens the free pool across residual military capacity.
- **Perimeter** weights free infantry toward the current boundary.
- **Center** weights it toward increasing exact boundary depth.
- **Directional** weights it toward one exact fixed-point axial facing.

`R` cycles Balanced, Perimeter, and Center on all selected clusters.
Holding `F`, dragging, and releasing sets Directional policy. Policies are
persistent metadata rather than one-shot formation commands. As the cluster
grows, contracts, or changes shape, the authority can redistribute its free
troops toward the current policy.

Policy computation deliberately excludes infantry in live expansion, attack,
Push, or Reshape packets from the target population. The same packets still
consume capacity in the cells they physically occupy. Therefore policy neither
moves active action troops nor counterbalances its free distribution against
them, but it also cannot overfill around them. Settled, completed, or cancelled
strength rejoins the free pool.

Background policy movement yields atomically when an accepted explicit command
intersects it. A rejected command leaves maintenance untouched, and unrelated
explicit actions remain fixed. Yielding does not clear the policy metadata; the
authority resumes maintenance on a later pass when troops and capacity are free.
Capacity-blocked policy strength remains queued rather than being finalized at
an intermediate cell. Reconciliation replans from current physical positions
and can relay resident strength through a saturated connector while incoming
strength replaces it. Policy always considers the whole free pool and never the
player's Share; Share remains exclusive to expansion and attack.

Policy lineage is stored on owned cells. Both children of a split inherit their
existing lineage. Captured cells inherit the connected cluster policy. When
clusters merge, the policy with the newest explicit player revision wins across
the merged component; the player can immediately set another one.

### Single-cluster Reshape

Reshape is a best-effort internal movement tool, not an alternative attack.
It is available only when exactly one cluster is selected. `T` enables the
brush and left-drag draws the desired owned, passable troop footprint.

The brush exposes independent width and height plus symmetric ring growth.
Its overlay shows the complete intended footprint even at an edge: usable
cells, unavailable in-map cells, and out-of-world positions have distinct
treatments. The drawn footprint may be smaller or larger than the current troop
footprint, including owned cells outside the previous occupied bounds.

Reshape uses the whole currently available pool and never Share. Reachable
drawn cells fill first by residual capacity. If the drawing can hold the pool,
movable source strength outside it drains; if it is undersized, targets saturate
and exact conserved overflow remains outside. Live unrelated allocations remain
fixed and reserve capacity. A disconnected source part without a reachable
target stays unchanged. Internal routes never cross non-friendly ground.

### Exact Stop and cancellation

`X` snapshots the exact live explicit-order IDs whose current allocations
intersect the selected clusters. Background policy maintenance is excluded.
Confirming Stop cancels only that frozen dispatch set and releases its surviving
strength at current physical cells. It does not rewind captures, restore
casualties, clear policy metadata, or cancel a newly arriving unrelated order.

`Escape` cancels a staged enemy union, Reshape, or Stop preview. In idle mode
it clears source selection. Successful contextual actions retain source
selection for follow-up commands.

### Deliberate V1 boundary

The previous painted sub-cluster Push Front, one-shot Formation/Bias, and
contested retask-handle grammar remain useful implementation history, not the
primary V1 interaction. Cluster attacks already compute distinct local
progression along changing front arcs. Point-side seventh/eighth global
directions and direct sub-cluster surgery should return only if playtests reveal
a tactical need that the cluster loop cannot express.

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

Exact lethality, neutral resistance, capture timing, multi-edge allocation, and
elevation coefficients are provisional. The first model should be deterministic,
inspectable, and easy to tune rather than feature-rich.

## Presentation

V1 uses intentionally simple graybox graphics:

- generated stepped hex meshes;
- flat terrain and ownership colors;
- simple lighting;
- clear borders and selection highlights;
- selected-cluster and enemy-target perimeters, exact active front edges,
  queues, and ETA;
- policy and Reshape target-density heatmaps;
- a 52-pixel, text-only contextual key-hint strip showing the current mode,
  projection, invalid/submitting state, and exact next keys, plus a complete
  `?` field manual and compact side inspector/order summary;
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
- Per-cell target-density scripting, policy priorities, conditional automation,
  or automatic enemy-target selection beyond the four cluster policies.
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
- contextual expansion/attack weighting and cluster-policy tuning details.

## V1 Acceptance Criteria

The vertical slice is ready for gameplay evaluation when all of the following
are true:

1. Two human players can join and finish Conquest on a curated stepped island;
   victory occurs at 80% of the fixed capturable-land denominator.
2. The same rules operate on the 128 x 128 playtest and representative 192 x 192
   validation maps.
3. Elevation visibly affects traversal and uphill combat, and cliffs block
   ordinary ground movement.
4. Civilian population grows locally; the global mobilization target converts
   it into local infantry over time; lowering the target does not instantly
   demobilize existing force.
5. `C`, its add/remove modifiers, and Select All operate on complete owned
   passable clusters, including empty owned connectors. Selection remains
   coherent through authoritative growth, merge, and split changes without
   adopting or cancelling active orders.
6. Clicking neutral ground expands every reachable selected perimeter with a
   mild focus bias and positive all-side participation when strength permits.
   The conserved wave respects terrain, capacity, throughput, garrisons, and
   hostile exclusion.
7. Clicking one or more complete enemy clusters attacks every shared front.
   Fronts can turn, split, and merge as captures expose the immutable target
   mask, but cannot escape it or duplicate source commitments.
8. One persisted Share applies exactly once per participating free source cell
   and only to expansion and attack. Mobilization, policy, and Reshape remain
   independent of it.
9. Balanced, Perimeter, Center, and Directional persist on clusters and adapt to
   geometry. Policy targets exclude live action troops while reserving their
   occupied capacity. Maintenance yields atomically to intersecting accepted
   commands, queues at capacity bottlenecks, and replans relay movement from
   current positions; explicit allocations and split, capture, and
   newest-revision merge inheritance remain preserved.
10. One selected cluster can best-effort Reshape into a smaller or larger owned,
    passable troop footprint using its whole available pool. Exact fits drain
    movable strength outside; undersized shapes saturate and conserve overflow;
    unrelated allocations remain fixed.
11. `X` cancels only the exact explicit-dispatch snapshot intersecting selected
    clusters; background policy maintenance is excluded. Normal selection and
    contextual commands never implicitly retask another explicit order.
12. Cutting a corridor creates genuinely independent components whose existing
    population and forces remain usable locally but cannot transfer across it.
13. Combat resolves across contested edges using frontage, elevation, capacity,
    and casualties, including attacks from several edges without double-counting
    defenders.
14. Graybox overlays make source clusters, staged enemy targets, focused
    expansion, policy/Reshape targets, active flows, blocked orders, and
    contested pressure understandable without production assets.
15. Core tuning values are configurable so playtests can adjust the model
    without changing its data or interaction foundations.

## Questions for Playtesting, Not Pre-production Blockers

- Is complete-cluster selection fast enough for ordinary play, including
  multi-select and Select All, without making players miss sub-cluster control?
- Does neutral clicking communicate “all sides, weighted toward this focus”
  clearly enough, or does it need stronger branch-allocation preview?
- Does attacking complete enemy clusters from every shared front produce
  understandable momentum as the masked fronts turn, split, and merge?
- Is one Share value sufficient for both contextual actions, and is it always
  clear that policies and Reshape ignore it?
- Do persistent Balanced, Perimeter, Center, and Directional policies reduce
  repetitive housekeeping without creating surprising troop motion?
- Is excluding live action troops from policy targets intuitive once their
  occupied capacity is still visible and reserved?
- Does the one-cluster Reshape brush make contraction, enlargement, saturation,
  and conserved overflow legible?
- Is exact Stop discoverable and precise enough without normal selection ever
  functioning as a retask gesture?
- Is the local civilian-to-military mobilization model understandable before an
  explicit economy is introduced?
- Do capacity, throughput, frontage, terrain, and garrisons create readable
  bottlenecks rather than frustrating queues?
- What map density and match duration best expose travel time without long
  periods of inactivity?
