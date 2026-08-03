# V1 Graybox UI Brief

## Objective

Design a readable native-desktop graybox interface for a two-player 2.5D hex RTS. The interface exists to test aggregate troop logistics, not individual-unit control or visual theme.

## Player mental model

Troops are scalar strength physically located on hexes. A player selects one
connected owned region, holds `P`, drags toward one of six directions, and
previews the initial edges of a sustained front before confirming. Selected
cells facing neutral or enemy territory in that direction form the front; the
other connected selected cells behind it form the reinforcement corridor. Rear
troops route only through that corridor before feeding fixed-direction lanes.
Strength moves subject to hex capacity, edge throughput, elevation, cliffs,
combat frontage, resistance, and terrain-scaled garrisons. It never teleports.

## Required V1 interactions

1. Join one of two human player slots and see connection/match status.
2. Pan, rotate, and zoom an orthographic 3D camera over a stepped hex island.
3. Hover a hex and inspect coordinate, terrain, elevation, owner, civilians, infantry, capacity, and occupancy percentage.
4. Paint one connected owned source region, including a border section and any
   reinforcement corridor extending backward into owned territory.
5. Hold `P`, drag outward, and release to choose one exact hex direction.
6. Preview one connected active front, its exact initial edges, selected-only
   corridor routes, estimated arrival time, bottlenecks, resistance, garrison
   cost, and committed troop amount.
7. Confirm the sustained Push Front command or stop a matching active push.
   Each axial lane continues independently until its committed pool is
   exhausted, blocked, defeated, reaches the map edge, or is manually cancelled.
8. From the same connected selection, preview and confirm a neutral-only Expand
   All operation. The chosen dispatch share is taken once from each selected
   unallocated stack. Combined strength splits evenly at each outward local
   branch, merges at shared children, and continues through successive perimeter
   layers. Each branch advances independently until it stops before an enemy,
   exhausts, blocks, reaches the edge, or is cancelled.
9. Select an owned region and issue percentage-aware one-shot Balance,
   Core-load, or Perimeter-load redistribution.
10. Select an owned region, drag an orientation arrow, preview a directional
   target-density heatmap, and issue percentage-aware Front-load
   redistribution. The unparticipating share remains frozen per source cell.
11. Distinguish a contested cell by a controller/attacker pressure blend without
    interpreting it as authoritative dual occupancy.
12. Adjust a global mobilization target. Lowering it stops future conversion but does not demobilize existing troops; this future-recruitment target remains visibly separate from each order's dispatch/participation percentage.
13. Read active flows, congestion, combat fronts, ownership changes, casualties, conquest percentage, and the 80% victory result.

## Presentation constraints

- No production assets, fiction, unit portraits, minimap, fog of war, technology tree, or build menu.
- The map is the primary surface and remains visible behind compact overlays.
- Ownership and troop-density shading must remain distinguishable at far zoom.
- Color cannot be the only signal for the selected region, active front,
  blocked path, or combat.
- The interface must explain authoritative rejection and congestion rather than silently doing nothing.
- Prefer direct manipulation and a short, discoverable keyboard vocabulary.
- Support two separate client windows on one machine during development.

## Technical constraints

- Bevy 0.19 native UI and GPU rendering.
- Orthographic 3D camera.
- Combined chunk meshes with per-vertex colors; avoid one UI/material/collider per hex.
- Deterministic height-aware ray-to-hex picking.
- SpacetimeDB is authoritative. Client route/ETA/heatmap previews are replaceable predictions.
- Initial viewport target is 1440 x 900, but layout must remain usable at 1280 x 720.

## Requested output

Provide an implementation-oriented critique and one recommended layout/input model. Specify:

- information hierarchy;
- exact Push Front selection/orientation, Expand All, and redistribution gestures;
- compact HUD regions and their contents;
- overlay encodings that work together;
- state transitions for Push Front, Expand All, cancellation, and all four
  redistribution previews;
- rejection/error feedback;
- the smallest viable onboarding hints;
- major ambiguity or readability risks to test first.

Keep this to V1 graybox mechanics. Do not invent setting, production art, additional resources, units, buildings, or game modes.
