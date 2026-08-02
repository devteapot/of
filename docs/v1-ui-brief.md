# V1 Graybox UI Brief

## Objective

Design a readable native-desktop graybox interface for a two-player 2.5D hex RTS. The interface exists to test aggregate troop logistics, not individual-unit control or visual theme.

## Player mental model

Troops are scalar strength physically located on hexes. A player selects owned source hexes, selects destination hexes, previews an aggregate route and ETA, then confirms. Strength travels through the map subject to hex capacity, edge throughput, elevation, cliffs, and combat frontage. It never teleports.

## Required V1 interactions

1. Join one of two human player slots and see connection/match status.
2. Pan, rotate, and zoom an orthographic 3D camera over a stepped hex island.
3. Hover a hex and inspect coordinate, terrain, elevation, owner, civilians, infantry, capacity, and occupancy percentage.
4. Paint or toggle a set of owned source hexes.
5. Paint or toggle destination hexes, including friendly staging areas and neutral/enemy attack goals.
6. Preview reachable routes, excluded cells, estimated arrival time, bottlenecks, destination capacity, and requested troop amount.
7. Confirm or cancel a source-to-destination transfer.
8. Select an owned region and issue one-shot Balance redistribution.
9. Select an owned region, drag an orientation arrow, preview a directional target-density heatmap, and issue Front-load redistribution.
10. Adjust a global mobilization target. Lowering it stops future conversion but does not demobilize existing troops.
11. Read active flows, congestion, combat fronts, ownership changes, casualties, conquest percentage, and the 80% victory result.

## Presentation constraints

- No production assets, fiction, unit portraits, minimap, fog of war, technology tree, or build menu.
- The map is the primary surface and remains visible behind compact overlays.
- Ownership and troop-density shading must remain distinguishable at far zoom.
- Color cannot be the only signal for source, destination, blocked path, or combat.
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
- exact source/destination/redistribution gestures;
- compact HUD regions and their contents;
- overlay encodings that work together;
- state transitions for transfer and oriented Front-load previews;
- rejection/error feedback;
- the smallest viable onboarding hints;
- major ambiguity or readability risks to test first.

Keep this to V1 graybox mechanics. Do not invent setting, production art, additional resources, units, buildings, or game modes.
