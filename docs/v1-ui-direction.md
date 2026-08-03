# V1 Graybox UI Direction

Status: implementation baseline

Provenance: derived from `docs/v1-ui-brief.md`, reviewed headlessly with Grok CLI 0.2.118 using `grok-4.5` on 2026-08-02. The brief remains provider-neutral and this document records the decisions adopted by the implementation.

## Interaction model

The client has one modal order state machine:

```text
Idle -> Push Front orient -> Push Front preview -> Submit -> Idle
Idle -> Expand All preview -> Submit -> Idle
Idle -> Redistribute Balance -> Preview -> Submit -> Idle
Idle -> Redistribute Front-load -> Orient -> Preview -> Submit -> Idle
Idle -> Redistribute Core-load -> Preview -> Submit -> Idle
Idle -> Redistribute Perimeter-load -> Preview -> Submit -> Idle
```

The same owned-region selection feeds Push Front, Expand All, and all four
redistribution presets. Server intentions remain independent of input gestures:

- `PushFront { selected_cells, direction, commitment }`
- `CancelPushFront { selected_cells, direction }`
- `ExpandAll { selected_cells, dispatch }`
- `CancelExpandAll { selected_cells }`
- `Redistribute { cells, preset, direction?, participation }`
- `SetMobilization { target }`

Push Front is the player-facing V1 conquest command. It uses the ordinary
connected owned-cell selection. After one exact direction is chosen, selected
cells whose outward neighbor is neutral or enemy territory define the active
front. Selection geometry or target eligibility may divide it into several
disconnected arcs; every arc seeds independent outward lanes. The remaining
selected cells behind those arcs are their reinforcement corridor and troop
pool. Every selected cell must reach at least one arc across traversable
selected-only edges. Hold `P`, drag outward, and release to choose one of six
directions. The client derives every initial outward edge and submits the
selected cells, direction, and commitment. The server independently derives
and validates the same front; visual segment IDs are never authoritative.

### Camera

- Middle-mouse drag or `Space + left drag`: pan.
- `Q` / `E`: rotate around the current focus.
- Mouse wheel: orthographic zoom.
- `Home`: frame the island.

### Selection and Push Front

- Left click or drag paints owned source hexes.
- A locally attacked enemy-controlled contested hex may instead be painted as
  a tagged order handle. The gesture snapshots the local active order IDs
  pressing that edge; the hex is not inserted into route geometry as though it
  were owned.
- `Shift + left`: add; `Control + left`: subtract.
- In source-selection mode, `[` / `]` remove or add one complete six-neighbor
  ring around the brush. `Shift` modifies only width, and `Control` modifies
  only height. The rectangular core uses odd dimensions and the ring expansion
  remains centered on the hovered hex.
- `C` selects the hovered six-connected owned cluster; Shift adds that cluster
  and Control removes it. `Control/Command + A` selects every owned hex.
- Cluster and all-owned selection remain physical snapshots: they prune lost
  or impassable cells, never auto-add later captures, and never flood through a
  contested handle. Press `C` again to refresh the current owned component.
- A tagged handle follows its snapshotted order IDs while their packet
  locations update. It does not acquire newly pressing orders or silently
  become a physical source after capture. The preview distinguishes the handle
  from the derived current source cells and reports replaced order count and
  surviving strength.
- Hold `P`, drag outward, and release to quantize one exact hex direction. The
  selected region must be connected. Its active directional boundary may
  contain multiple disconnected arcs.
- The preview highlights only edges from a selected source to its immediate
  non-owned neighbor in that direction. Side edges are never inferred.
- Those outward-facing selected cells are the front. Every other selected cell
  acts as the reinforcement corridor; it does not create an outward lane of its
  own. Every selected cell must reach at least one active arc across traversable
  selected-only edges; cliffs may divide sources between arcs, but cannot leave
  a source with no reachable arc.
- Routes from corridor cells remain entirely inside the selection until they
  feed an initial front cell. From there, each lane advances automatically
  through successive cells along the chosen axial direction. It never bends,
  widens, or retargets to an adjacent lane.
- `[` / `]` changes commitment by ten percentage points; the provisional
  default is 50%.
- `Enter` confirms; `Escape` cancels the preview while retaining the source
  selection.
- While the Push Front direction is previewed, `X` cancels matching active
  pushes launched from the selected front in that direction. Cancellation
  stops future advancement; surviving troops remain where they physically are.

The committed strength is a fixed pool. Terrain, elevation, throughput,
frontage, neutral or enemy resistance, and a terrain-scaled occupying garrison
consume or delay that pool. Lanes advance, stall, exhaust, lose, become blocked,
reach the map edge, or are manually cancelled independently. Precise infantry
transfer is not part of the V1 command surface; exact cell movement is reserved
for possible future discrete units such as tanks or boats.

### Expand All

- Shift+`P` or the HUD `EXPAND ALL` button previews every passable
  selected-to-neutral edge around the connected owned selection. No orientation
  gesture is required; disconnected boundary arcs may coexist and a shared
  outside target may receive strength through more than one edge.
- Selected strength moves only toward decreasing internal depth until it reaches
  an eligible boundary. At every cell, the combined local and incoming pool is
  divided evenly among all traversable children one depth closer to that
  boundary; contributions merge before the next split. Every
  movement-isolated part of the selection must expose at least one eligible
  neutral boundary of its own.
- `[` / `]` changes dispatch by ten percentage points. The preview shows an
  **up to** amount from visible local strength; the authority applies that share
  once to each selected cell's currently unallocated soldiers at submission.
  It is separate from mobilization.
- Each boundary's combined pool is divided evenly among its local outward exits.
  Beyond them, surplus repeats the split-and-merge rule from perimeter depth
  `d` to `d + 1`; it does not preserve the first edge's axial direction. Amber
  edges and the forecast wave communicate neutral expansion, while red remains
  reserved for a directional Push Front that can attack.
- `Enter` starts independently progressing wave branches. Terrain, throughput,
  capacity, and garrison costs can make one side bulge while another stalls. A
  branch cannot cycle, skip a perimeter layer, or tunnel through an uncleared
  cell; it stops before enemy territory, when exhausted or blocked, at the map
  edge, or when cancelled. It may cross friendly ground without paying another
  occupation garrison.
- From the same selected-region preview, `X` cancels matching active Expand All
  operations and releases survivors where they currently are.

### Redistribution

- `B` previews Balance over the owned selection and `Enter` submits it.
- Hold `F`, drag an arrow over the map plane, and release to preview Front-load in that orientation. A zero-length direction is invalid.
- `G` previews Core-load, which favors cells nearest the selected region's
  geometric center.
- `R` previews Perimeter-load, which favors the selected region's outer rings.
- `[` / `]` changes the participating percentage for every redistribution
  preset. The unparticipating share of every cell's current stack is frozen in
  that cell; only the selected share joins the redistributed pool.
- The heatmap represents proposed target density, not strength that has already moved.
- Tagged contested handles may participate in every preset. Their whole
  snapshotted orders are replaced atomically and the heatmap is calculated from
  the packets' current physical cells plus explicitly selected owned cells.
- `Enter` submits and `Escape` cancels while retaining the region selection.

### Mobilization

The global mobilization target remains visible in the bottom bar. The caption
says that it affects future recruitment and does not demobilize existing
troops. It is explicitly distinct from the order-panel Dispatch/Participate
percentage and can be adjusted through the pointer slider or with `M` plus the
arrow keys.

## HUD hierarchy

- Top strip: player identity/slot, both connection states, both conquest percentages, and match phase.
- Upper-right tactical panel: compact map-view status followed by coordinate,
  terrain, elevation, owner, civilians, infantry, military capacity, and
  occupancy.
- Contextual right-side order panel: mode, selection totals, commitment, active
  edge count or invalid reason, estimated ETA, bottleneck, Confirm, and Cancel.
- Bottom strip: mobilization target, active flow/front counts, and latest command result.
- Transient bottom-center toast: authoritative rejection or important match event.

At 1280 x 720, the inspector and order details share one stacked side panel so they do not cover the map.

## Map encodings

Base chunk vertex colors combine ownership hue with the current map-view
luminance: absolute soldier strength by default, absolute civilian population
in Civilians, and force-neutral ownership readability in Overview. `1`, `2`,
and `3` select those views, while `V` cycles them. Exact compact totals appear
only when projected hex spacing is readable. They are glyphs from one small
texture atlas, batched into one world-space mesh over the visible hex tops—not
individual Bevy UI elements. Height and shaded column sides convey elevation.
These overlays remain separate:

- hover: thin white perimeter around the current brush footprint;
- source: solid exposed perimeter around each selected component;
- active Push Front: bold exact directional boundary edges across every
  eligible arc, with hostile ticks when they lead into enemy ownership;
- Expand All preview: amber entry edges plus a bounded translucent forecast of
  successive split-and-merge perimeter depths; disconnected arcs and
  independently progressing branches are allowed, and forecast truncation is
  explicit;
- populated land: one batched external cluster perimeter in Civilians view;
- selected-only push route: desaturated dashed arrows terminating at the exact
  frontier edge;
- committed flow: solid animated arrows, thickness proportional to flow;
- excluded/blocked: diagonal cross or hatch;
- bottleneck: thick edge marker and source-side queue wedge;
- combat: bold edge with opposing chevrons;
- contested cell: one authoritative controller remains, while the chunk's
  vertex colors blend the controller and attacker colors according to pressure
  derived from subscribed `CombatFront` state;
- sustained push preview: initial edges plus a representative selected
  reinforcement route; later layers remain outcome-dependent until simulated;
- redistribution target: monochrome selection-only heatmap plus an orientation
  arrow for Front-load and center/perimeter emphasis for radial presets.

Color is never the only signal for source, active front, block, or combat.
Multi-source routes should render as aggregated corridors, not every
source/front-edge pair.

## Submission and rejection behavior

Confirm enters a temporary submitting state and cannot be pressed twice. Accepted commands become committed overlays. Rejected commands return to their preview with selections intact.

Every hard rejection uses all three channels:

1. a short toast from the server reason;
2. a marker on the relevant cell or edge when available;
3. an entry in the compact order log.

Congestion is accepted state, not a rejection. It increases estimated arrival time and displays queues/bottlenecks.

## Minimal onboarding

The first-session hint is limited to:

```text
LMB paint · P push · Shift+P expand all · B/F/G/R redistribute · [ ] order % · X stop · ? help
```

Contextual first-use hints explain that Push Front uses only selected cells and
continues in one exact direction with a fixed force pool; Expand All uses the
same region but forms a branching neutral-only perimeter wave with no retained
heading; every redistribution preset still moves troops physically; and the
order percentage does not change mobilization or send soldiers home.

## First playtest risks

Test these before adding visual polish:

1. ownership remains readable under density shading at far zoom;
2. the selected reinforcement corridor and active front remain distinct;
3. each sustained Push lane's remaining momentum, resistance, and stop reason
   are understandable without implying lateral retargeting;
4. route aggregation avoids unreadable arrow clutter;
5. live density and proposed redistribution heatmaps look different;
6. height-aware picking selects the visible column at cliffs and camera pitch extremes;
7. the two local client windows make their player slot unmistakable;
8. ETA is labeled as an estimate and visibly corrects to authoritative state;
9. contested color blending communicates pressure without implying that both
   players authoritatively occupy the cell.
10. the Expand All forecast explains local splits, merges, and independently
    bulging branches without promising a globally even ring.
