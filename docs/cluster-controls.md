# Cluster-first troop controls

Status: implemented V1 interaction contract

The main troop-control unit is an owned traversable cluster, not a painted
sub-region. A cluster is the complete connected set of owned, passable hexes;
blocked terrain and impassable elevation edges split it. Empty owned hexes still
connect the cluster.

## Selection

- `C` replaces the selection with the owned cluster under the cursor.
- Shift+`C` adds that cluster; Control+`C` removes it.
- Control/Command+`A` selects every owned traversable cluster, including empty
  controlled cells.
- Selection contains no implicit retask handles and never cancels or adopts a
  live order. When ownership changes, surviving selected cells expand back to
  their complete current clusters. Growth and merges are absorbed; both sides
  of a split remain selected when they are still owned.

## Contextual map clicks

With at least one source cluster selected, the owner of the clicked hex chooses
the command:

- Clicking unclaimed capturable ground dispatches **Expand Clusters**. Every
  selected source cluster with a reachable neutral perimeter expands on all of
  its perimeter. The clicked hex is a focus, not a destination: branches which
  move closer to it receive weight 3, equal-distance branches weight 2, and
  branches moving away weight 1. The allocator gives every branch a positive
  baseline when the committed integer strength makes that possible.
- Clicking an enemy hex dispatches **Attack Clusters** against that complete
  enemy traversable cluster. Every shared passable front between the selected
  sources and selected targets participates. Shift-click stages or toggles
  several complete enemy clusters without dispatching each one separately;
  Control-click removes a staged target, a plain enemy click adds it and submits
  the union, and `Enter` submits the staged union.

Attack does not retain one global direction. The authoritative order snapshots
the target-cluster mask and branches from all initially shared fronts. Captured
cells expose the next cells in that mask, so the wave can turn around a corner,
split, and merge as the front changes. It never enters a cell outside the
accepted mask. Terrain, elevation, edge throughput, destination capacity,
garrison cost, and enemy infantry are rechecked during execution.

## Force Share

One persisted Share percentage applies to Expand Clusters, Attack Clusters,
and Front Rebalance. Each participating source cell contributes that percentage of its
action-available infantry exactly once: stationary free strength plus
yieldable background-policy strength physically inside the source, excluding
troops committed to another explicit action. A selected source cluster with no
eligible shared front contributes nothing. Strength is then conserved through
branching and merging; selecting several targets never reapplies Share to the
same source.

Infantry in a live action packet is unavailable to a later action unless that
order is explicitly stopped. Merely reselecting its cluster does not supersede
it.

Repeating the exact same contextual click while its earlier command is still
in flight queues another independent command immediately. Each command gets a
distinct authority ID and recomputes Share from the action-available pool left
by commands accepted before it. For example, two 10% expansions from an
otherwise unchanged pool of 100 commit 10 and then 9, for 19 total; the second
click neither replaces nor retasks the first. A click with different sources,
targets, focus, or Share waits until the current rapid-repeat group settles so
it cannot be mistaken for a replay.

## Strategic fronts and explicit rebalance

A strategic front is a set of deployable directed edges leaving one complete
owned cluster. Hostile runs are grouped by opponent. A neutral gap between two
runs against the same opponent keeps them in one hostile front; another opponent
splits the hostile frontage. Neutral-facing edges are grouped by the hostile
fronts that bound them around the perimeter: interruptions by the same hostile
front remain one neutral front, while the neutral sections on opposite sides of
two different hostile fronts remain independent. Neutral bridge edges remain in
their neutral front, so an edge or owned boundary hex may correctly belong to
more than one front. Off-map,
impassable, uncapturable, and cliff-blocked edges are ignored as non-deployable
markers and never appear in a front.

Select exactly one complete cluster, press `B`, and drag from an owned cell on
the source front to an owned cell on another front. Authority re-derives both
fronts from current topology, snapshots Share once from movable source-front
troops, and computes terrain-aware aggregate routes to the destination. Troops
then move physically through the normal packet pipeline and remain subject to
throughput, capacity, route invalidation, combat, and future interception.

Fronts have equal strategic importance by default. Their total-perimeter size
does not silently alter the player's cross-front allocation. Once a target
front is chosen, exposed edge count and available capacity weight placement
inside that front. This gives longer fronts useful frontage without reintroducing
constant global balancing.

There is no scheduled background redistribution or persistent troop-density
policy. Redistribution occurs only through explicit Front Rebalance or Reshape
commands.

## Reshape and stop

Reshape is available only when exactly one owned cluster is selected. `T`
enables the brush; left-drag draws the desired owned troop footprint. The brush
has independent width and height controls plus symmetric ring growth, and its
complete unavailable/out-of-world footprint remains visible. On release the
client previews a best-effort transition using the whole currently available
pool, never Share.

Reachable drawn cells fill first. A fitting shape drains movable strength from
source cells outside the drawing; an undersized shape saturates and leaves exact
conserved overflow outside it. Active unrelated allocations remain fixed, and
disconnected source parts with no reachable target stay unchanged.

`X` snapshots and previews the exact live explicit dispatches intersecting the
selected clusters. Confirming stops only that explicit snapshot. `Escape`
cancels a staged target, front rebalance, reshape, or stop preview; in idle mode
it clears the cluster selection.

## Deliberate V1 boundary

The older sub-cluster Push Front, one-shot formation, and retask-handle grammar
remain implementation history rather than the primary controls. The cluster
attack wave already produces different local push vectors along different
front arcs, so no separate point-side seventh/eighth direction is required for
the main interaction. Direct sub-cluster surgery can return later as an advanced
tool after the cluster loop is proven.
