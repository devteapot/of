# V1 Graybox UI Direction

Status: implementation baseline

Provenance: derived from `docs/v1-ui-brief.md`, reviewed headlessly with Grok CLI 0.2.118 using `grok-4.5` on 2026-08-02. The brief remains provider-neutral and this document records the decisions adopted by the implementation.

## Interaction model

The client has one modal order state machine:

```text
Idle -> Push Front orient -> Push Front preview -> Submit -> Idle
Idle -> Redistribute Balance -> Preview -> Submit -> Idle
Idle -> Redistribute Front-load -> Orient -> Preview -> Submit -> Idle
```

The same owned-region selection feeds Push Front and both redistribution presets. Server intentions remain independent of input gestures:

- `PushFront { selected_cells, direction, commitment }`
- `Redistribute { cells, preset, direction }`
- `SetMobilization { target }`

Push Front is the player-facing V1 conquest command. It uses the ordinary
connected owned-cell selection: cells on its boundary in one exact direction
define one contiguous active front, and the selection may be painted backward
into owned territory to include its reinforcement pool and routes. Hold `P`,
drag outward, and release to choose one of six directions. The client derives
the exact final edge for each participating boundary cell and submits the
selected cells, direction, and commitment. The server independently derives
and validates the same edges; visual segment IDs are never authoritative.

### Camera

- Middle-mouse drag or `Space + left drag`: pan.
- `Q` / `E`: rotate around the current focus.
- Mouse wheel: orthographic zoom.
- `Home`: frame the island.

### Selection and Push Front

- Left click or drag paints owned source hexes.
- `Shift + left`: add; `Control + left`: subtract.
- In source-selection mode, `[` / `]` remove or add one complete six-neighbor
  ring around the brush. `Shift` modifies only width, and `Control` modifies
  only height. The rectangular core uses odd dimensions and the ring expansion
  remains centered on the hovered hex.
- `C` selects the hovered six-connected owned cluster; Shift adds that cluster
  and Control removes it. `Control/Command + A` selects every owned hex.
- Hold `P`, drag outward, and release to quantize one exact hex direction. The
  selected region must be connected and its active directional boundary must
  be one connected front section.
- The preview highlights only edges from a selected source to its immediate
  non-owned neighbor in that direction. Side edges are never inferred.
- Routes from rear cells remain entirely inside the selection until the exact
  final frontier edge. The command advances only one cell deep; repeat it from
  the resulting border to continue the push.
- `[` / `]` changes commitment by ten percentage points; the provisional
  default is 50%.
- `Enter` confirms; `Escape` cancels the preview while retaining the source
  selection.

Painted destination transfer remains an internal aggregate-flow primitive and
a possible future precision-logistics tool. It is not exposed in the V1 input
loop.

### Redistribution

- `B` previews Balance over the owned selection and `Enter` submits it.
- Hold `F`, drag an arrow over the map plane, and release to preview Front-load in that orientation. A zero-length direction is invalid.
- The heatmap represents proposed target density, not strength that has already moved.
- `Enter` submits and `Escape` cancels while retaining the region selection.

### Mobilization

The global mobilization target remains visible in the bottom bar. The caption says that it affects future recruitment and does not demobilize existing troops. It can be adjusted through the pointer slider or with `M` plus the arrow keys.

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
- active Push Front: bold exact directional boundary edges, with hostile ticks
  when they lead into enemy ownership;
- populated land: one batched external cluster perimeter in Civilians view;
- selected-only push route: desaturated dashed arrows terminating at the exact
  frontier edge;
- committed flow: solid animated arrows, thickness proportional to flow;
- excluded/blocked: diagonal cross or hatch;
- bottleneck: thick edge marker and source-side queue wedge;
- combat: bold edge with opposing chevrons;
- redistribution target: monochrome selection-only heatmap plus orientation arrow for Front-load.

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
LMB paint owned hexes · hold P + drag push · B balance · F drag front-load · Esc cancel · ? help
```

Contextual first-use hints explain that Push Front uses only selected cells and
crosses one exact edge, Front-load still moves troops physically, and lowering
mobilization does not send soldiers home.

## First playtest risks

Test these before adding visual polish:

1. ownership remains readable under density shading at far zoom;
2. the selected reinforcement corridor and active front remain distinct;
3. the exact one-cell push cannot be mistaken for an automatic expansion wave;
4. route aggregation avoids unreadable arrow clutter;
5. live density and proposed redistribution heatmaps look different;
6. height-aware picking selects the visible column at cliffs and camera pitch extremes;
7. the two local client windows make their player slot unmistakable;
8. ETA is labeled as an estimate and visibly corrects to authoritative state.
