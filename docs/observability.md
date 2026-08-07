# Observability

Status: focused structured events for client + module debugging  
Last updated: 2026-08-07

This is a thin instrumentation layer for reconnect, lobby/provision, command
paths, subscription sync, and frame spikes — not a full telemetry platform.

Related: [Browser release gates](./browser-gates.md), [Performance](./performance.md).

## What already existed

| Surface | What it gives you |
| --- | --- |
| **F3** performance overlay | FPS, frame ms, entities, chunks, cells, orders/flows/fronts |
| HUD order log / toasts | Last few command results (human-readable, not greppable) |
| Match `LogStopwatch` | Per-tick phase timings in SpacetimeDB logs (`simulation_*`) |
| `match-perf` | Distributed step-dilation CSV/JSONL under load |
| Reconnect soak / e2e | Pass/fail harnesses, not live event streams |

## What this adds

### Client (`of.observe`)

Stable `category.action` keys, an in-memory ring (last 64), optional console
emission, and an **F4** overlay.

| Key | When |
| --- | --- |
| `net.connect_begin` / `net.reconnect` | Connection attempt starts |
| `net.connected` | SDK connect callback |
| `net.bootstrap` / `net.tactical` | Subscription applied |
| `net.disconnect` / `net.connect_fail` / `net.join_fail` | Loss / setup / seat failure |
| `lobby.action` | Create / join / start reducer outcome |
| `cmd.submit` / `cmd.accept` / `cmd.reject` / `cmd.fail` | Control path |
| `sync.apply` | Authoritative dirty batch applied (debug level) |
| `perf.frame_spike` | Frame time ≥ 33 ms (rate-limited) |
| `auth.token_warn` | Token persist failure |

### Module logs (`target: "of"`)

Published match/lobby modules emit the same style of `event=…` lines into
SpacetimeDB logs:

| Event | Module |
| --- | --- |
| `match.join` (`mode=claim\|reconnect`) | match |
| `match.start` / `match.connected` / `match.disconnected` | match |
| `cmd.accept` / `cmd.reject` | match (`write_receipt`) |
| `sim.heartbeat` | match (every 40 logical steps) |
| `lobby.create` / `join` / `leave` | lobby |
| `lobby.provision_begin` / `complete` / `fail` | lobby |

Phase stopwatches remain available as before.

## How to enable / read

### Native client

```bash
OF_OBSERVE=1 cargo run -p game-client -- --player 1 --name "P1"
# or: cargo run -p game-client -- --observe --player 1
```

Console lines look like:

```text
[of.observe] INFO net.bootstrap gen=1 auto_join=true
```

Press **F4** for the on-screen ring (works even when console emit is off).
**F3** remains the performance panel.

### Browser client

Open with `?observe=1`, e.g.:

```text
http://127.0.0.1:8080/?player=1&name=P1&profile=browser-p1&observe=1
```

Filter DevTools console for `of.observe`. Use F3 + F4 together during WebGPU
frame-cost qualification.

On wasm, the F3 sampler also mirrors smoothed FPS / frame ms into
`window.__ofPerf` (~250 ms cadence) so CDP/`Runtime.evaluate` automation can
record gate samples without OCR. Do not use `std::time::Instant` in
wasm-shared client code — it panics with `time not implemented on this
platform`; prefer Bevy `Time`.

### SpacetimeDB module logs

```bash
spacetime logs --server local of-match-dev
spacetime logs --server local of-lobby-dev   # name may differ in your deploy
```

Filter for `event=` / `target=of`. Republish modules after pulling these changes
so server-side events appear.

## Suggested follow-ups

1. Optional JSONL dump of the client ring to `artifacts/observe/` for soak runs.
2. Correlate client `command_id` with module `cmd.accept` in reconnect soak reports.
3. Bevy system timings / render pass marks once WebGPU frame gates need deeper drill-down.
4. Quiet `sim.heartbeat` / `cmd.accept` behind a module log level when noise hurts fuel.
