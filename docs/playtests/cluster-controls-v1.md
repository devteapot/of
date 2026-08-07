# Cluster controls V1 playtest

Use this checklist to test the cluster-first controls described in
[V1 UI direction](../v1-ui-direction.md). Save completed results as
`docs/playtests/cluster-controls-v1-results-YYYY-MM-DD.md`. Runtime artifacts
(screenshots, recordings, and logs) belong in the gitignored
`artifacts/playtests/` directory.

## Session setup

1. Start a fresh two-seat match, either through the production browser lobby
   directory or a local match database. For local play:
   `./scripts/start-local-server.sh`, publish `modules/match`, configure two
   seats, and connect two native clients or two browser profiles.
2. Start the match and use enough mobilization/actions to create neutral
   perimeters, an enemy contact, active orders, and an undersized owned area.
3. Record client target (native/browser), database, map preset, participants,
   git SHA, and any accessibility or input-device constraint.

For each item, mark **PASS**, **FAIL**, or **INCONCLUSIVE**, describe what the
participant did and understood, and attach relevant evidence.

## Checklist

### 1. Focus is a destination, not an exact order

| Field | Check |
| --- | --- |
| Procedure | Select an owned cluster, hover then click neutral terrain from a cluster with multiple eligible exits. Ask the player what will happen before and after dispatch. |
| Expected observation | The player identifies the clicked hex as a weighted focus and expects expansion from all eligible selected perimeters, rather than a single-cell movement destination. |
| Pass | The participant explains or acts on the all-perimeter behavior without corrective coaching. |
| Fail | The participant expects only the clicked cell to receive troops or regards outcomes on other branches as a bug. |

### 2. Enemy mask versus active fronts

| Field | Check |
| --- | --- |
| Procedure | Attack a complete enemy cluster, then observe the preview and several simulation updates as fronts turn, split, or disappear. |
| Expected observation | The complete selected enemy component remains understandable as the target mask, while active front overlays communicate the currently contested subset. |
| Pass | The participant can distinguish “accepted target territory” from “where troops are fighting now.” |
| Fail | The participant confuses the mask with live fronts or cannot identify either state. |

### 3. Front ambiguity at corners

| Field | Check |
| --- | --- |
| Procedure | Select one owned cluster, press `B`, choose a source boundary near a corner/shared-cell junction, and drag to another front. Repeat with a nearby alternative seed. |
| Expected observation | Source and target strategic arcs make the chosen fronts explicit; invalid or same-front gestures explain the correction. |
| Pass | The participant can predict the source/target fronts before release and recover from an invalid choice. |
| Fail | Corner-adjacent selections make the intended arc unclear or create surprising transfers. |

### 4. Whole-cluster multi-select coarseness

| Field | Check |
| --- | --- |
| Procedure | Select a cluster with `C`, add another with Shift+`C`, remove one with Control+`C`, then ask the participant to perform a tactical action affecting only part of a selected component. |
| Expected observation | The participant understands that selection is whole connected owned components; any desire for sub-cluster control is recorded with its tactical context. |
| Pass | No blocking tactical case is found, or requested granularity is satisfiable through a documented cluster action. |
| Fail | A recurring, concrete tactical case cannot be expressed without sub-cluster selection. |

### 5. Reshape overflow visibility

| Field | Check |
| --- | --- |
| Procedure | Select exactly one cluster, press `T`, draw an undersized owned/passable footprint, and inspect the ready preview before confirming. Also draw across a world edge or unavailable cells. |
| Expected observation | Available, unavailable, and off-world footprint portions remain visible; the preview reports strength that will stay outside and does not imply troop loss. |
| Pass | The participant correctly predicts conserved overflow and can see why excluded cells are unavailable. |
| Fail | Overflow appears lost/failed or part of the intended footprint is invisible. |

### 6. Stop discoverability

| Field | Check |
| --- | --- |
| Procedure | Create active orders, select their cluster, and ask the participant to stop them without naming a key. Then press `X`, inspect the preview, and confirm with LMB/Enter. |
| Expected observation | The participant finds or learns the `X` hint/manual; the preview communicates that it freezes an exact order snapshot and does not retask troops. |
| Pass | The participant discovers the control from visible guidance or uses it correctly after the field manual. |
| Fail | The participant expects a new action or selection to retask/cancel orders, or cannot locate Stop from provided guidance. |

## Results template

```md
# Cluster controls V1 results — YYYY-MM-DD

## Session
- Method: native / browser / desk review + code inspection
- Git SHA:
- Match/database and preset:
- Participants:
- Evidence:

| Risk | Verdict | Observation | Follow-up |
| --- | --- | --- | --- |
| Focus-as-destination |  |  |  |
| Enemy mask vs active fronts |  |  |  |
| Front ambiguity at corners |  |  |  |
| Whole-cluster multi-select coarseness |  |  |  |
| Reshape overflow visibility |  |  |  |
| Stop discoverability |  |  |  |
```
