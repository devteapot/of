# Cluster controls V1 notes — 2026-08-26

## Session

- Method: implementation + automated client tests; offline fixture screenshots
  when the native client can render.
- Focus: hover-before-click expand/attack preview and a mode-switched command
  strip. No new mechanic, no sub-cluster surgery, no retask.

## Presentation contract

| Surface | Expected |
| --- | --- |
| Expand hover | Participating perimeter highlighted, 11/10/9 on those branches, committed Share labeled, inland dimmed (contributes 0) |
| Attack hover | Full target mask plus the shared fronts that will fire |
| Idle HUD | `C` + click only |
| Share | Valid expand/attack hover (and existing ready rebalance/expand/attack modes) |
| `T` | Exactly one complete cluster with inland free infantry |
| `X` | Live explicit orders intersecting the selection |
| `B` | One complete cluster with two strategic fronts |

Playtest risk 1 (focus-as-destination) remains the gate: the hover must make
all-perimeter expansion unmissable *before* dispatch. Automated tests cover the
preview payload and HUD copy; participant observation still belongs on the
checklist in [`cluster-controls-v1.md`](./cluster-controls-v1.md).
