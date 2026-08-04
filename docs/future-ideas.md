# Future ideas: deliberately after V1

This is the parking lot for ideas we want to preserve without allowing them to blur the first playable version. None of these items is promised, scheduled, or part of V1. An idea should move out of this file only after the spatial troop-and-front loop is fun, legible, and technically healthy.

The intended extension point is the same throughout: troops, resources, and infrastructure should remain spatial. Future systems should deepen the consequences of distance and terrain, not recreate a global pool that can appear anywhere instantly.

## Logistics and infrastructure

### Precise movement for future discrete units

Aggregate infantry intentionally has no exact-strength cell-to-cell command.
Contextual cluster expansion/attack, persistent density policy, and
single-cluster Reshape should first prove that aggregate infantry logistics can
remain expressive without destination micromanagement. Reshape draws a target
footprint, but the deterministic allocator—not the player—chooses capacity-safe
cell strengths. Exact targeting may return for genuinely discrete units such as
a tank formation, boat, transport, or specialist whose position and route are
individually meaningful. Such movement must still preserve spatial
conservation, expose route and ETA, and respect clearance, congestion, and
interception.

**Depends on:** a discrete-unit model that earns direct control, validated
aggregate-infantry interactions, route readability, order priorities, and
controls that remain usable on large maps.

### Advanced sub-cluster and point-side control

The cluster-first attack wave needs no global direction: it progresses through
the selected enemy mask from every shared front and can turn as that front
changes. Playtests may nevertheless reveal a need for optional sub-cluster
front surgery. If that advanced tool returns, ordinary axial movement exposes
the six true hex-neighbor directions.

Two explicit screen-space headings toward opposite point pairs would not be
additional hex edges: they require alternating between adjacent axial steps
(`a, b, a, b...`). That creates parity-dependent lanes, ambiguous frontage, and
visually different routes after camera rotation. Keep them out of the global
control unless cluster-first playtests show a real tactical gap and an advanced
directional tool is deliberately restored. If added, model them as an explicit
zig-zag route policy with deterministic parity and a full preview, not as fake
seventh and eighth hex directions.

**Depends on:** evidence that whole-cluster actions are tactically insufficient,
stable alternating-lane frontage and collision rules, rotation-proof input
labeling, and readable route previews.

### Roads and paths

Roads and paths could reduce travel time and increase troop-flow capacity along particular edges. This would make infrastructure a strategic investment and make redeployment routes visible targets rather than passive bonuses.

**Depends on:** stable edge-based movement costs, flow capacity, route previews, and travel-time tuning in the core logistics model.

### Rivers, bridges, and crossings

Bridges could enable or accelerate crossings at specific river edges, while fords provide weaker natural alternatives. Their scarcity could create meaningful chokepoints without adding individual-unit micro.

**Depends on:** rivers represented as edge data, reliable multi-route pathfinding, map validation that prevents impossible victory states, and clear crossing/combat-width rules.

### Infrastructure damage and repair

Roads and bridges could be damaged, disabled, repaired, or deliberately sabotaged. Damage should affect future movement through ordinary edge modifiers so it composes with the existing logistics model.

**Depends on:** infrastructure ownership, targeting and repair interactions, rerouting behavior, attribution, and safeguards against cheap or unreadable permanent denial. Damage is not useful until roads and alternate routes are already strategically legible.

## Cities, population, and economy

### Cities and spatial population

Cities could become spatial sources of population, economic output, and recruitment instead of merely granting a global modifier. Their location would make encirclement, evacuation, and transport matter.

**Depends on:** stable territory components, spatial resource origins, local storage, and a UI that can explain flows without becoming a city-management game by accident.

### Civilian economy and the burden of mobilization

The intended post-V1 economy must make soldiers an active weight on economic
capacity, not merely civilians who no longer produce. Recruitment removes labor
from the civilian economy, while maintaining armies should consume a legible
combination of food, wages, equipment, or productive capacity. The exact
resource model remains open, but the strategic requirement is fixed: choosing
more manpower now must carry an ongoing economic burden and trade against
long-term growth. That burden should be understandable and gradual, not a
hidden punishment for using the primary game mechanic.

**Depends on:** a validated match economy, population accounting, explicit soldier upkeep, and balance tools that prevent an early mobilization advantage from becoming irreversible.

### Migration

Population could move toward safety, employment, or better-governed regions and away from combat or shortages. Migration should be a slow spatial flow with causes the player can inspect, not background randomness.

**Depends on:** cities, regional conditions, civilian pathing or aggregate flow, housing/capacity rules, and careful performance budgeting for large maps.

### Depots and local reserves

Depots could store equipment or supplies closer to a front, shortening reinforcement time while creating capturable or destructible logistical targets.

**Depends on:** spatial resources, transport capacity, structures and capture rules, and a clear distinction between manpower, equipment, and any later supply resource.

### Training, evacuation, and demobilization

- **Training** could turn civilians or recruits into effective troop strength over time at particular facilities.
- **Evacuation** could deliberately move civilians, recruits, or strategic stockpiles away from threatened areas.
- **Demobilization** could return surviving troop capacity to the civilian economy, with travel time and perhaps a recovery delay.

These mechanics would make the transition between civilian and military power reversible and spatial.

**Depends on:** population, cities, recruitment sources, transport, and a clear lifecycle for troop strength. They should arrive together or in a deliberately coherent subset so population cannot disappear into unexplained state changes.

### Regional policies and persistent distributions

Players could assign persistent policies to regions: recruitment intensity, reserve targets, tax or production emphasis, evacuation thresholds, and preferred distribution between nearby fronts. Policies should express long-term intent while actual people and troops still travel physically.

**Depends on:** stable or player-defined regions, actual-versus-target distribution UI, deterministic logistics priorities, and good behavior when regions split, merge, or are captured.

## Information and world state

### Fog of war

Fog of war could make scouting, deception, terrain, and force concentration more important. It should conceal information consistently rather than merely darken the map.

**Depends on:** authoritative visibility, sensor and last-known-state rules, secure subscription filtering, recon tools, and a UI that clearly distinguishes unseen, stale, and currently observed information.

### Mutable terrain

Terraforming, excavation, destruction, flooding, or constructed elevation could eventually let players reshape routes and chokepoints. This is intentionally deferred because terrain edits touch pathfinding, connectivity, chunk meshing, collision, persistence, networking, map fairness, and every system that caches traversal data.

**Depends on:** a proven immutable terrain pipeline, deterministic edit transactions, incremental remeshing and connectivity updates, save/version migration, and strict limits on where and how terrain can change.

## Future combat domains

### Tanks and armor

Two different models remain open and should be prototyped rather than blended prematurely:

1. **Weighted scalar armor.** Armor is attached to a front or troop allocation and changes attack, defense, speed, terrain suitability, and upkeep. It preserves the no-unit-micro model and is the lower-complexity extension.
2. **Discrete 2x2 polyhex vehicles.** A vehicle occupies a four-hex footprint, has an orientation, follows clearance-aware paths, and may rotate in place only when the required footprint or swept cells are free. A stopped vehicle can intentionally block a narrow road, pass, or bridge. This provides stronger physical tactics but introduces rotation rules, occupancy conflicts, deadlocks, pathfinding complexity, and potentially unit-level micro.

The choice depends on what the validated core is missing. A visual tank does not require a discrete simulation footprint; a one-hex marker could also represent an armored formation.

**Depends on:** terrain scale, combat width, occupancy and footprint rules, vehicle-friendly route generation, and a firm decision about how much direct unit control belongs in the game.

### Naval forces

Naval play could make seas, islands, ports, blockades, amphibious landings, and overseas logistics meaningful. It should be designed as a domain with its own movement and projection rules, not just ground troops that happen to traverse water.

**Depends on:** water topology, ports and embarkation, cross-domain combat, map objectives that justify naval investment, and ways to avoid stranded or unreachable victory territory before naval play is enabled.

### Air forces

Air power could provide reconnaissance, transport, interception, or time-bounded strikes. A sortie, mission, or regional-allocation model may fit the strategic control style better than individually flown aircraft.

**Depends on:** fog of war, bases, range and readiness, interception, counterplay, and a readable way to show effects over a 2.5D map.

## Diplomacy

Alliances, access agreements, shared vision, trade, ceasefires, and betrayal could support matches with more than two players. Diplomacy should have explicit rules for troops in allied territory, shared infrastructure, victory, and what happens when a relationship changes.

**Depends on:** multiplayer beyond the initial two-player session, player relationships separate from ownership, robust disconnect and surrender handling, and victory modes designed to resist kingmaking or indefinite stalemates.

## Objectives, modes, and match clocks

Conquest begins with the simple V1 rule: control 80% of capturable land. Later modes could use HQ capture, control points, resource or population objectives, escort/evacuation, survival, asymmetric scenarios, or team goals.

Match timers should remain a mode option rather than a universal rule. Timed modes need explicit tie-breaking and overtime; untimed modes need reachable objectives and anti-stalemate pressure designed into the mode itself.

**Depends on:** a mode-owned victory contract, map metadata and validation per mode, spectator/end-state handling, and enough data from Conquest matches to understand where stalemates actually arise.

## Technology tree: explicitly undecided

A technology tree is not an assumed destination. It should be added only if research choices create meaningful strategic divergence that cannot be expressed more cleanly through geography, infrastructure, buildings, force composition, or scenario rules. Content volume alone is not a reason to add one.

**Depends on:** a mature economy and multiple viable strategic paths. The decision remains open.

## Larger and world-scale maps

Larger maps, long-running worlds, or multiple linked theaters could amplify logistics, regional policy, migration, and diplomacy. They could also turn useful travel time into inactivity and greatly increase state, subscription, pathfinding, and persistence costs.

**Depends on:** benchmarks at several map sizes, chunked storage and rendering, interest management, hierarchical routing, regional aggregation, persistence/versioning, and a game loop that remains active while distant forces travel. Scale should grow because it improves decisions, not because the renderer can display more hexes.

Before increasing the V1 32,768-cell current-world command cap, measure the
private cluster-wave topology, policy redistribution routes, packet counts, and
subscription churn. Coalesce shared topology where those measurements justify
it. Track active packet count through the F3 `FLOWS` metric while profiling
representative wide fronts.

## Browser target

A browser build remains a desirable later target while native desktop is the V1 focus. Core simulation and data formats should avoid unnecessary platform coupling, but browser-specific compromises should not constrain the first implementation before its mechanics are proven.

**Depends on:** Bevy target compatibility at the chosen pinned version, graphics/shader portability, asset size and loading, memory limits, input differences, networking behavior, and profiling representative maps on actual browsers.

## Visual and asset pipeline

V1 graphics should remain simple graybox or low-complexity assets until scale, readability, and interaction are stable. Later visual work can include environment kits, buildings, vehicles, effects, UI decoration, and biome-specific map presentation.

The preferred generation workflow is:

1. Write a versioned brief with style, scale, palette, camera, budget, filenames, and acceptance criteria.
2. Use Grok 4.5 through its headless CLI for bounded concept, raster, UI-reference, and Blender-script tasks when available.
3. Use reviewed, deterministic Blender scripts for procedural 3D exports rather than giving an agent unrestricted shell control.
4. Render previews and validate dimensions, transforms, materials, polygon counts, readability, and Bevy loading.
5. Preserve prompts, source files, model/tool versions, licenses or reference provenance, output hashes, previews, and known defects in an asset manifest.
6. If Grok is unavailable, unsuitable, or out of credits, continue through ordinary Codex subagents using GPT-5.6-sol.

Exact UI text and layout should be implemented in Bevy/code; generated images are better suited to textless concepts, decoration, icons, and references. Any generated output still requires human review for consistency, readability, licensing risk, and technical correctness.

**Depends on:** an established art direction, asset scale conventions, naming/import rules, repeatable validation, and enough stable gameplay to know what the art must communicate.

## Promotion rule

Before implementing any item here, write down:

- which observed V1 problem or opportunity it addresses;
- the smallest testable version;
- the systems it depends on and invalidates;
- how the player will understand and control it;
- its simulation, networking, persistence, and content cost;
- what result would cause us to remove it again.

Until then, these ideas are not forgotten; they are intentionally not V1.
