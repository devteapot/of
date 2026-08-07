# Cluster controls V1 results — 2026-08-07

## Session

- Method: desk review + code inspection; no GUI session was run.
- Git SHA: `8037c75` (working tree contains unrelated WIP).
- Intended local endpoint: `http://127.0.0.1:3000`.
- Blocker: the checked-in local data directory contains `spacetime.pid` owned
  by PID 41307, but the endpoint was not reachable. Starting another server
  safely failed on that lock, so this review did not remove the stale-looking
  lock or alter the existing local data.
- Evidence inspected:
  - [`hud.rs`](../../crates/game-client/src/hud.rs), field manual and
    contextual HUD copy;
  - [`interaction.rs`](../../crates/game-client/src/interaction.rs), input,
    full-component selection, front validation, Reshape preview, and Stop
    snapshot construction;
  - [`overlays.rs`](../../crates/game-client/src/overlays.rs), staged-target,
    Reshape, Stop, and active-front overlays;
  - [`v1-ui-direction.md`](../v1-ui-direction.md), intended interaction
    contract and first-playtest risks.

These are implementation-readiness verdicts, not participant-validation
results. Repeat every row with the procedure in
[the checklist](./cluster-controls-v1.md) once a clean local or browser
two-player session is available.

| Risk | Verdict | Desk-review observation | Follow-up |
| --- | --- | --- | --- |
| Focus-as-destination | **INCONCLUSIVE — implementation evidence supports it** | The field manual says expansion pressures all sides with a mild click bias (`hud.rs` lines 28–29), and the neutral click path submits `ExpandClusters` with `focus: clicked` (`interaction.rs` lines 799–822). This does not establish that a player reads the click as a focus. | Test participant prediction before dispatch on a multi-exit cluster. |
| Enemy mask vs active fronts | **INCONCLUSIVE — implementation evidence supports it** | Enemy clicks expand a complete connected enemy cluster before submission (`interaction.rs` lines 835–876); staged targets and `view.active_fronts` have separate overlay paths in `overlays.rs`. The manual explicitly says the wave never leaves the selected enemy mask (`hud.rs` lines 30–31). | Observe an attack that turns/splits and ask the player to identify mask versus live combat. |
| Front ambiguity at corners | **INCONCLUSIVE — implementation evidence supports it** | Front Rebalance requires a one-component selection and validates source/target seeds before dispatch (`interaction.rs` lines 893–970); the HUD/manual states drag is from one strategic-front boundary to another (`hud.rs` lines 32–33). Code validation is not evidence that corner highlighting is legible. | Exercise two corner-adjacent source seeds in a live match. |
| Whole-cluster multi-select coarseness | **INCONCLUSIVE — implementation evidence supports it** | Selection expands to full owned components and supports add/remove modifiers (`interaction.rs` lines 1199–1223); the HUD describes complete-cluster selection (`hud.rs` line 27). Whether that is tactically too coarse needs player behavior, not code inspection. | Log every requested partial-component action with map context and desired outcome. |
| Reshape overflow visibility | **INCONCLUSIVE — implementation evidence supports it** | The manual promises full unavailable/off-world brush visibility and conserved overflow (`hud.rs` lines 34–35). The ready HUD explicitly reports “STAY OUTSIDE” when overflow is nonzero (`hud.rs` lines 629–649), backed by `reshape_outside_strength` in `interaction.rs`. | Draw an undersized and boundary-crossing footprint; verify participant predicts overflow and unavailable cells. |
| Stop discoverability | **INCONCLUSIVE — implementation evidence supports it** | The persistent idle controls include `X stop` (`hud.rs` lines 555–565); the field manual explains its exact-snapshot semantics (`hud.rs` lines 36–37), and `interaction.rs` freezes order IDs in `StopPreview`. Discovery without coaching cannot be inferred. | Ask a participant to stop active orders without naming the key, then record whether they find the visible hint or `?` manual. |

## Overall verdict

The implementation and copy cover each documented risk, but all six remain
**INCONCLUSIVE** pending participant observation in a clean two-player session.
