# V1 UI direction

Status: implemented cluster-first interaction baseline

This document describes the presentation of the canonical
[cluster-first troop controls](./cluster-controls.md). Painted source regions,
manual retask handles, and `P`-driven Push are not the primary V1 workflow.

## Interaction model

The ordinary loop is intentionally short:

```text
hover owned cluster -> C -> optionally add clusters -> click neutral or enemy
```

The map itself chooses the contextual action:

- neutral click: expand every reachable selected perimeter, mildly weighted
  toward the click;
- enemy click: attack that complete enemy cluster from every shared front;
- own, blocked, or otherwise invalid click: no implicit troop action.

Source clusters stay selected after a successful action. Normal selection never
cancels or adopts a live order.

### Controls

| Input | UI behavior |
| --- | --- |
| `C` | Replace selection with the complete owned cluster under the cursor |
| Shift + `C` | Add the hovered cluster |
| Control + `C` | Remove the hovered cluster |
| Control/Command + `A` | Select every owned traversable cluster |
| LMB neutral | Submit focused all-perimeter expansion |
| LMB enemy | Add the complete enemy cluster and submit the target union |
| Shift + LMB enemy | Stage or toggle an additional enemy cluster |
| Control + LMB enemy | Remove a staged enemy cluster |
| `Enter` | Submit a staged enemy target union or ready Stop/Reshape preview |
| `[` / `]` | Change Share for expansion, attack, and Front Rebalance |
| `B`, then LMB drag | Move Share from one strategic front to another |
| `T`, then LMB drag | Draw one-cluster best-effort Reshape |
| `X` | Preview exact Stop for explicit dispatches intersecting selected clusters |
| Escape | Back out of staged targets/Reshape/Stop; idle clears selection |
| `?` | Toggle the field manual |

Camera, map-view, mobilization, framing, and diagnostics retain their existing
bindings.

### Selection state

A selected cluster perimeter uses the local-player highlight with a solid
exposed edge. Multi-selection shows every complete component identically; there
is no “primary” sub-region. Empty owned cells remain visibly selected because
they are real connectivity.

Selection reconciliation should not flash or shrink when authoritative growth
arrives. Merges absorb the newly connected owned cells. If ownership splits a
selected component, each surviving selected child keeps its outline.

The HUD selection summary reports cluster count, selected hex count, free
infantry, and action-committed infantry.

### Neutral expansion feedback

Hovering or clicking valid neutral ground shows it as a focus, not a single-cell
destination. The preview emphasizes all reachable neutral perimeter exits while
using stronger arrows or intensity for branches approaching the focus and
lighter treatment for equal/away branches. Branch weights `11` / `10` / `9` sit
on those exits, committed Share is labeled, and inland cells that contribute 0
are dimmed before the click.

The presentation must avoid implying that the clicked hex alone will be filled.
A short label such as “ALL PERIMETERS · FOCUS +q,-r · SHARE 40%” carries the
right mental model. Ineligible source clusters and blocked exits are visibly
dimmed rather than making a valid multi-cluster action look globally invalid.

### Enemy-cluster target staging

The complete enemy component under the pointer receives a hostile target
perimeter. Shift-staged components remain highlighted, including disconnected
targets. Control-removal immediately removes the complete component.

Attack preview marks every shared source/target front. It does not draw one
global direction arrow. As the attack runs, subscribed active fronts replace
the initial preview so turning, splitting, and merging remain legible without
claiming that later enemy-cluster growth joined the immutable mask.

The HUD shows staged cluster count, target hex count, participating source
strength, and Share. Selecting several targets must never display Share as
multiplied per target.

### Strategic Front Rebalance

`B` enters a modal front gesture for exactly one complete selected cluster.
Pressing the source boundary identifies and highlights its whole strategic arc;
dragging to another front highlights the target arc. The compact HUD displays
Share, projected movable strength, route validity, and estimated travel time.

The preview must distinguish source and target fronts and never imply that a
larger perimeter automatically owns more cluster-wide troops. Exposed edge
count only influences placement within the chosen target front. A stale topology
or same-front gesture is rejected without moving troops.

### Single-cluster Reshape

`T` is enabled only with exactly one selected cluster. Entering it activates
the brush; idle dragging never paints source selection.

The brush has:

- symmetric ring growth with `[` / `]`;
- independent width with Shift + brackets;
- independent height with Control + brackets;
- combined width/height with Shift + Control + brackets.

Its full intended footprint remains visible in three categories: available
owned/passable cells, unavailable in-map cells, and out-of-world positions.
This lets a player understand the shape even when part of the brush cannot be a
target.

The preview heatmap shows best-effort results, including strength that must
remain outside an undersized drawing. Copy should say **Available pool**, not
Share. A disconnected source part with no reachable drawn target stays unchanged
and should be called out rather than reported as lost.

### Exact Stop

`X` freezes the live explicit-order IDs currently intersecting selected
clusters. The preview reports exact order and packet counts. Confirming cancels
that frozen dispatch set only; new unrelated orders do not join it.

Stop feedback says that surviving troops settle at their current cells. It
never promises a rewind, restored territory, or recovered casualties.

## Contextual state machine

The compact command strip switches between these states:

1. **Idle / clusters selected** — `C` and the contextual click. Share appears
   only on a valid expand/attack hover; `B` only with two fronts; `T` only when
   one cluster has inland free infantry; `X` only with live orders.
2. **Attack targets staged** — target count, all shared fronts, LMB/Enter submit,
   Shift toggle, Control remove, Escape back.
3. **Front Rebalance gesture** — source/target strategic fronts and release-to-preview.
4. **Reshape drawing** — brush dimensions, available pool, capacity, and drawing
   controls.
5. **Reshape ready** — projected fit/overflow and LMB/Enter confirmation.
6. **Stop ready** — exact frozen order count and LMB/Enter confirmation.
7. **Invalid** — precise reason and the smallest corrective hint.
8. **Submitting** — input lock until a matching receipt or disconnect outcome.

No state renders a permanent command-button grid.

## HUD hierarchy

The top strip remains match-level: identity, connection, conquest progress,
match phase, and opponent status.

The right panel remains inspection-level:

- active map view;
- hovered cell coordinate, terrain, elevation, owner, capacity, and occupants;
- selected cluster/source summary;
- active order/front counts and latest receipt.

The bottom contextual strip is action-level. It shows the current state and only
the relevant keys. Share appears for neutral expansion, enemy attack, and Front
Rebalance. Reshape instead shows free/available strength and reserved active
occupancy. `?` contains explanations rather than forcing long copy into the map view.

## Map encodings

Use a small, composable overlay vocabulary:

- hover: thin high-contrast top outline;
- source cluster: solid local-player exposed perimeter;
- staged enemy cluster: solid hostile perimeter with shared fronts emphasized;
- expansion focus: target marker plus weighted all-perimeter branches;
- active wave: current packet/front segments, congestion, and blocked edges;
- front-rebalance/Reshape: target heatmap;
- unavailable brush: distinct warning tint;
- out-of-world brush: translucent third tint that preserves intended geometry;
- contested cell: controller/attacker pressure blend while ownership stays
  singular.

Keep overlays viewport-bounded and avoid one UI entity per hex.

## Submission and rejection

Immediate contextual clicks still enter a locked submitting state until their
receipt arrives. Staged attacks, Reshape, and Stop use LMB on the map or
`Enter` to confirm. `Escape` never mutates authoritative state.

A rejected command restores the prior interaction state where useful, preserves
source selection and staged geometry, and displays the exact server reason.
Receipts are more important than optimistic animation; speculative preview must
yield cleanly to the authoritative snapshot.

## Minimal onboarding

The first-run hint can fit in four lines:

```text
C selects a whole owned cluster · click after selecting
Hover unclaimed: all perimeters, 11/10/9, Share · hover enemy: mask + fronts
B then drag rebalances fronts when two exist · T reshape inland · X stop live orders
? opens the full field manual
```

## First playtest risks

1. Players may read a neutral focus as an exact destination instead of weighted
   all-perimeter expansion.
2. Complete enemy target masks may be hard to distinguish from currently active
   fronts as the attack progresses.
3. Front boundaries may be ambiguous at shared corner cells; source and target
   arc highlighting must make the chosen fronts explicit.
4. Multi-selecting complete clusters may be too coarse in specific tactical
   situations; record those cases before restoring sub-cluster controls.
5. An undersized Reshape may look like failure unless conserved outside overflow
   is included in the preview.
6. Exact Stop may be overlooked if players expect selecting a cluster and
   issuing another action to retask it.
