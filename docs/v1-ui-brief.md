# V1 graybox UI brief

Status: canonical cluster-first design input

## Objective

Design a readable native-desktop graybox interface for a 2–500 player 2.5D hex
RTS. The interface tests aggregate troop logistics through complete territorial
clusters, not individual units or painted sub-cluster fronts.

The detailed behavior is fixed by
[Cluster-first troop controls](./cluster-controls.md). Visual exploration may
improve legibility but must not change its authority or accounting rules.

## Player mental model

1. Hover owned territory and press `C` to select its complete passable cluster.
2. Add or remove clusters with Shift/Control+`C`, or select all with
   Control/Command+`A`.
3. Click neutral territory to expand every reachable selected perimeter,
   weighted somewhat toward the click.
4. Click one or more complete enemy clusters to attack every shared front.
5. Use Share to choose how much free infantry expansion, attack, or Front
   Rebalance commits.
6. Press `B` and drag between strategic fronts to move troops explicitly.
7. Use one-cluster Reshape for a best-effort drawn troop footprint and `X` for
   exact Stop.

Selection never means retask. Explicit actions remain allocated until they
settle, complete, or are explicitly stopped.

## Required V1 interactions

1. Show stepped terrain, ownership, absolute infantry density, civilians, and
   exact cell totals at readable zoom.
2. Make complete selected clusters legible, including empty owned connectors
   and multiple disconnected selections.
3. Keep selection coherent when clusters grow, merge, or split.
4. Show neutral clicks as a focus on all-perimeter expansion, not a precise
   destination. Hover-before-click must make participating perimeters, 11/10/9
   branch weights, committed Share, and inland-zero unmistakable. Communicate
   stronger toward/equal/away branch weighting without hiding weak-side
   expansion.
5. Highlight the entire enemy cluster under the pointer and every shared source
   front before the attack click.
6. Support Shift staging/toggling and Control removal of several complete enemy
   target clusters.
7. Display one persisted Share for expansion, attack, and Front Rebalance;
   never show Reshape as percentage-limited.
8. Highlight complete source and target strategic-front arcs during a rebalance
   gesture.
9. Distinguish free infantry from troops committed to explicit actions.
10. Enable Reshape only for one selected cluster. Show the complete brush with
    independent width, independent height, symmetric ring growth, unavailable
    in-map cells, and out-of-world positions.
11. Preview best-effort Reshape fit and conserved outside overflow using the
    whole available pool.
12. Preview `X` as an exact frozen order set and explain that stopped troops
    remain at their current physical cells.
13. Show terrain, capacity, throughput, frontage, garrisons, congestion,
    blocked paths, and contested pressure without implying dual cell ownership.
14. Adjust the global mobilization target separately from Share. Lowering it
    stops future conversion but does not demobilize existing infantry.
15. Use a compact keybind-first contextual strip and a `?` field manual, not a
    persistent button for every command. Idle shows `C` and click; Share, `B`,
    `T`, and `X` appear only in the states where they apply.

## Context states

The control strip must have distinct copy for:

- idle selection and contextual click (`C` + click only until a hover or live
  context makes Share / `B` / `T` / `X` relevant);
- hover-before-click expand (perimeters, 11/10/9, Share, inland dim) and attack
  (full target mask plus firing fronts);
- staged enemy targets;
- Front Rebalance source/target gesture;
- Reshape drawing and ready preview;
- exact Stop preview;
- invalid action with a specific reason;
- locked submission awaiting an authoritative receipt.

Source selection should survive successful commands and ordinary rejections.
`Escape` backs out of staged modes; idle Escape clears selection.

## Presentation constraints

- Preserve map readability at 1280 x 720.
- Keep the top status strip, right inspector/order summary, bottom contextual
  strip, and mobilization control compact. For `player_count <= 8` the status
  strip may list every seat; above eight it must stay aggregate (configured /
  claimed / connected / open, plus leader and local status) rather than 500
  inline entries.
- Do not obscure the map with a command grid or long permanent help copy.
- Use text labels and line/heatmap overlays before adding bespoke iconography.
- Keep overlay categories composable and color-blind distinguishable through
  line weight, pattern, or luminance as well as hue.
- Use one authoritative controller color per cell; blend active attacker pressure
  only as presentation.
- Keep unavailable and out-of-world brush cells visually distinct from valid
  owned targets.

## Technical constraints

- Combined chunk meshes and batched overlays; no entity/material/UI node per
  hex.
- Viewport-bounded outlines and labels.
- Deterministic axial coordinates and height-aware picking.
- Generated authoritative reducer bindings; previews never become authority.
- Complete-component selection and target scope must be revalidated server-side.
- Materialized selection is capped for V1; later world-scale selection should
  become symbolic without changing the UX contract.

## Requested output

A code-oriented graybox direction that can be implemented with Bevy primitives,
text, line geometry, vertex-color updates, and allocation heatmaps. Prioritize
clarity of complete-cluster scope, contextual click result, Share accounting,
strategic fronts, and exact Stop over decorative style.
