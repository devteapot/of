# V1 Graybox UI Direction

Status: implementation baseline

Provenance: derived from `docs/v1-ui-brief.md`, reviewed headlessly with Grok CLI 0.2.118 using `grok-4.5` on 2026-08-02. The brief remains provider-neutral and this document records the decisions adopted by the implementation.

## Interaction model

The client has one modal order state machine:

```text
Idle -> Transfer -> Transfer preview -> Submit -> Idle
Idle -> Redistribute Balance -> Preview -> Submit -> Idle
Idle -> Redistribute Front-load -> Orient -> Preview -> Submit -> Idle
```

The same owned-region selection feeds transfers and both redistribution presets. Server intentions remain independent of input gestures:

- `Transfer { sources, destinations, amount }`
- `Redistribute { cells, preset, direction }`
- `SetMobilization { target }`

The next gameplay experiment adds `Expand { commitment }`: a neutral-only,
all-front pulse whose committed infantry remains local and whose expansion
speed is constrained by terrain and edge limits. It is intentionally separate
from targeted transfers and hostile attacks. Its final gesture is not locked;
the current left-drag selection must remain available while the prototype is
evaluated.

The corresponding focused command is `PushFront { edges, direction,
commitment }`. `X` activates Expand All without changing the cell selection;
`P` highlights neutral-facing border segments. Clicking chooses a natural
segment, dragging along the perimeter chooses a contiguous subsection, and an
outward drag supplies a six-direction continuation heading. The client submits
exact directed edge keys rather than a visual segment ID, so a changing border
cannot silently retarget the command.

### Camera

- Middle-mouse drag or `Space + left drag`: pan.
- `Q` / `E`: rotate around the current focus.
- Mouse wheel: orthographic zoom.
- `Home`: frame the island.

### Selection and transfer

- Left click or drag paints owned source hexes.
- `Shift + left`: add; `Control + left`: subtract.
- `T` locks the source selection and enters destination painting.
- Left click or drag paints destination land; friendly destinations mean arrival, while neutral/enemy destinations mean staging and attack.
- `[` / `]` changes requested strength by ten percentage points; the provisional default is 50%.
- `Enter` confirms; `Escape` cancels the preview while retaining the source selection.

### Redistribution

- `B` previews Balance over the owned selection and `Enter` submits it.
- Hold `F`, drag an arrow over the map plane, and release to preview Front-load in that orientation. A zero-length direction is invalid.
- The heatmap represents proposed target density, not strength that has already moved.
- `Enter` submits and `Escape` cancels while retaining the region selection.

### Mobilization

The global mobilization target remains visible in the bottom bar. The caption says that it affects future recruitment and does not demobilize existing troops. It can be adjusted through the pointer slider or with `M` plus the arrow keys.

## HUD hierarchy

- Top strip: player identity/slot, both connection states, both conquest percentages, and match phase.
- Small upper-right inspector: coordinate, terrain, elevation, owner, civilians, infantry, military capacity, and occupancy.
- Contextual right-side order panel: mode, selection totals, amount, reachable/excluded counts, estimated ETA, bottleneck, Confirm, and Cancel.
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

- hover: thin white ring;
- source: solid outer outline with corner ticks;
- friendly destination: dashed inner outline with inward chevrons;
- hostile/neutral destination: dashed outline with attack ticks;
- preview route: desaturated dashed arrows;
- committed flow: solid animated arrows, thickness proportional to flow;
- excluded/blocked: diagonal cross or hatch;
- bottleneck: thick edge marker and source-side queue wedge;
- combat: bold edge with opposing chevrons;
- redistribution target: monochrome selection-only heatmap plus orientation arrow for Front-load.

Color is never the only signal for source, destination, block, or combat. Multi-source routes should render as aggregated corridors, not every source/destination pair.

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
LMB paint owned hexes · T move · B balance · F drag front-load · Esc cancel · ? help
```

Contextual first-use hints explain that enemy destinations stage an attack rather than teleporting, Front-load still moves troops physically, and lowering mobilization does not send soldiers home.

## First playtest risks

Test these before adding visual polish:

1. ownership remains readable under density shading at far zoom;
2. adjacent source and destination regions remain distinct;
3. attack staging cannot be mistaken for entering an enemy cell;
4. route aggregation avoids unreadable arrow clutter;
5. live density and proposed redistribution heatmaps look different;
6. height-aware picking selects the visible column at cliffs and camera pitch extremes;
7. the two local client windows make their player slot unmistakable;
8. ETA is labeled as an estimate and visibly corrects to authoritative state.
