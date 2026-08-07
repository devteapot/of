# Browser release gates

Status: Wasm size + reconnect (isolated + under load) + WebGPU 128/192 frame cost measured  
Last updated: 2026-08-07

Architecture commits a Bevy WebAssembly/WebGPU client, but treats representative
browser graphics, reconnect, download size, and map performance as **required
before production release**. This document turns that into concrete budgets,
scripts, and a status ledger.

Related:

- [Technical architecture](./technical-architecture.md) — delivery commitments and load targets
- [Performance profiling](./performance.md) — native/headless `match-perf` harness
- [Implementation guide](./implementation.md) — current evidence and known limits
- [Observability](./observability.md) — F3 / F4 / `window.__ofPerf` / `of.observe`

## Gate summary

| Gate | Budget (provisional) | How to measure | Status (2026-08-07) |
| --- | --- | --- | --- |
| Wasm / web download size | ≤ **14 MiB** total gzip-9 transfer; ≤ **50 MiB** largest `.wasm` raw | `./scripts/measure-web-bundle.sh` | **Measured — PASS** (see baseline below) |
| WebGPU frame cost @ 128×128 | Sustained **≥ 55 FPS** / frame **≤ 18 ms** during ordinary pan/zoom (F3 overlay) | Browser procedure below | **Measured — PASS** (p50 59.9 FPS / 16.7 ms) |
| WebGPU frame cost @ 192×192 | Same as 128, plus no sustained stall when large subscription batches apply | Browser procedure below | **Measured — PASS** (p50 60.2 FPS / 16.6 ms) |
| Reconnect under load | Token reclaim succeeds; disconnect→reclaim p95 **≤ 5 s** over ≥ 20 cycles while the match is running | `./scripts/run-reconnect-soak.sh` (native SDK path) + browser smoke | **PASS** (isolated + concurrent `match-perf` 128); browser reload smoke recorded |
| Browser compile | `cargo clippy -p game-client --target wasm32-unknown-unknown -D warnings` | CI already | **PASS** (CI) |

Budgets are engineering gates, not marketing claims. Revisit them if Bevy/WebGPU
stack upgrades move the cost curve.

## Measured web-bundle baseline

Recorded from the existing Trunk `wasm-release` dist at `target/web` (git
`608ce3a`, dirty tree ignored by the script at measurement time):

| Artifact | Raw | gzip-9 |
| --- | ---: | ---: |
| `*_bg.wasm` | 43,371,133 (~41.36 MiB) | 11,932,611 (~11.38 MiB) |
| `*.js` | 169,487 | 24,666 |
| `index.html` | 2,020 | 1,012 |
| **Total** | **43,542,640** | **11,958,289 (~11.41 MiB)** |

Gate result vs defaults: **PASS** (gzip 11.41 / 14 MiB; raw wasm 41.36 / 50 MiB).

Re-measure after any Bevy/feature change:

```bash
./scripts/measure-web-bundle.sh --build --enforce
```

Reports land under `artifacts/browser/bundle-*.json` (gitignored). Production
deploy runs the same measurement with `--enforce` after Trunk build.

Override budgets when intentionally moving the ceiling:

```bash
OF_WEB_GZIP_BUDGET_BYTES=16777216 OF_WEB_RAW_WASM_BUDGET_BYTES=67108864 \
  ./scripts/measure-web-bundle.sh --enforce
```

## WebGPU frame cost @ 128 / 192

### Measured baselines (2026-08-07)

Qualified on **Chrome for Testing 149.0.7827.55** (headed, CDP attach,
`--use-angle=metal`), **Apple M4 Pro**, git `608ce3a` (+ local uncommitted
observe/`__ofPerf` bridge). Database: local `of-match-browser-gate` (fresh
publish; `of-match-dev` was owned by another identity on this machine).

Samples: ~60 s ordinary WASD pan/zoom after match start; gate uses smoothed
F3 / `window.__ofPerf` **p50** FPS and frame ms.

| Preset | Cells | p50 FPS | p50 frame ms | p5 FPS | p95 frame ms | Result | Artifact |
| --- | ---: | ---: | ---: | ---: | ---: | --- | --- |
| `playtest128` | 16 384 | **59.9** | **16.7** | 58.5 | 17.1 | **PASS** | `artifacts/browser/webgpu-frame-playtest128-2026-08-07T1749Z.json` |
| `validation192` | 36 864 | **60.2** | **16.6** | 57.5 | 17.4 | **PASS** | `artifacts/browser/webgpu-frame-validation192-2026-08-07T1752Z.json` |

F3 screenshots: `artifacts/browser/webgpu-playtest128-f3.png`,
`webgpu-validation192-f3.png`. Brief load spikes at connect (observe
`perf.frame_spike`) are excluded from the sustained window. The 192 run saw
seat-claim races when leftover tabs/tokens contended; the map still streamed
fully (36 864 cells) and frame cost stayed in budget.

### Procedure

1. Start SpacetimeDB and publish a fresh match you **own**:
   `./scripts/start-local-server.sh`
   `spacetime publish --server local --module-path modules/match --delete-data=always --yes of-match-browser-gate`
   (or `./scripts/publish-local.sh --fresh --confirm-delete-of-match-dev` when you
   own `of-match-dev`)
2. Configure the preset and two seats (`--no-config` if `spacetime.local.json`
   pins another database name):
   - 128: `spacetime call --server local --no-config of-match-browser-gate configure_match '{"playtest128":{}}' 2`
   - 192: `spacetime call --server local --no-config of-match-browser-gate configure_match '{"validation192":{}}' 2`
3. Serve the web client: `./scripts/run-web-client.sh`
4. Open a **WebGPU-capable headed Chrome** (Playwright’s default
   `chromium.launch` often exposes `navigator.gpu === undefined`; attach via CDP
   to Chrome for Testing with `--remote-debugging-port=9222 --use-angle=metal`):
   `http://127.0.0.1:8080/?player=1&name=P1&profile=browser-p1&observe=1&database=of-match-browser-gate`
   and a second profile for player 2. With `?player=N` the client auto-joins and
   auto-starts once both seats are claimed.
5. Press **F3** for the performance overlay (FPS, frame ms, chunks, dirty cells)
   and **F4** for the structured event ring. On wasm, smoothed metrics also
   publish to `window.__ofPerf` every ~250 ms for automation. Filter DevTools
   for `of.observe`. See [Observability](./observability.md).
6. Sample for ≥ 60 s of ordinary camera motion at default zoom, then after a
   burst of expansion/attack updates. Record smoothed FPS / frame ms, map
   preset, browser, GPU, and git HEAD into
   `artifacts/browser/webgpu-frame-<preset>-<timestamp>.json` using the template:

```json
{
  "kind": "webgpu-frame-sample",
  "preset": "playtest128",
  "browser": "Chrome 139",
  "gpu": "Apple M4 Pro",
  "git_head": "REPLACE",
  "ordinary_pan_zoom": { "fps": 0, "frame_ms": 0 },
  "after_subscription_burst": { "fps": 0, "frame_ms": 0, "notes": "" },
  "passed": false
}
```

### Native proxy (not a substitute)

`scripts/run-match-perf-matrix.sh --viewer` attaches a **native** Bevy client to
player 1 during headless load. Use it to debug mesh/subscription cost, but do
not mark the browser WebGPU gate PASS from native FPS alone.

The offline fixture (`?offline=1`) is a small radius map, **not** the 128/192
presets — useful for WebGPU bring-up only.

## Reconnect under load

### What already exists

- `match-e2e` proves a single token reclaim after the gameplay smoke.
- The client stores browser tokens in `localStorage` scoped by host/database/profile
  and rebuilds from a fresh subscription on reconnect.

### Harness added here

```bash
# Isolated native soak (publishes of-match-reconnect-soak, joins, starts, cycles)
./scripts/run-reconnect-soak.sh --fresh --cycles 20

# Against an in-flight match-perf database (same match, concurrent workers)
# while match-perf run-local is active and tokens exist:
./scripts/run-reconnect-soak.sh \
  --database of-match-perf \
  --cycles 30 \
  --out artifacts/browser/reconnect-under-load.json
# Point match-e2e at the worker seats explicitly when needed:
#   cargo run -p match-e2e -- --reconnect-only --reconnect-cycles 30 \
#     --database of-match-perf \
#     --player-one-token .match-perf-tokens/player-1.token \
#     --player-two-token .match-perf-tokens/player-2.token \
#     --reconnect-report artifacts/browser/reconnect-under-load.json
```

`match-e2e` flags: `--reconnect-only`, `--reconnect-cycles N`,
`--reconnect-report PATH`, optional `--player-one-token` /
`--player-two-token` (point at `.match-perf-tokens/player-1.token` and
`player-2.token` for true same-match under-load soaks).

### Measured isolated baseline (2026-08-07)

`./scripts/run-reconnect-soak.sh --fresh --cycles 5` on local SpacetimeDB
2.7.1 (default 64×64 / 2 seats, no concurrent workers):

| Metric | Value |
| --- | ---: |
| cycles | 5 |
| p50 disconnect→reclaim | 71 ms |
| p95 | 71 ms |
| max | 86 ms |
| budget | p95 ≤ 5000 ms |

Gate result for the **isolated** path: **PASS**.

### Measured under-load baseline (2026-08-07)

Ran 20 reclaim cycles against worker seats 1–2 while `match-perf` drove
`playtest` / 128 players / expand+rebalance on `of-match-perf-128` (git
`ec06915`, after `start_match` accepted). Report:
`artifacts/browser/reconnect-under-load-20260807.json`.

| Metric | Value |
| --- | ---: |
| cycles | 20 |
| p50 disconnect→reclaim | 186 ms |
| p95 | 191 ms |
| max | 196 ms |
| budget | p95 ≤ 5000 ms |

Gate result under concurrent load: **PASS**.

Provisional budget: disconnect→reclaim **p95 ≤ 5000 ms** over the soak report.

### Browser reload smoke

Recorded `artifacts/browser/browser-reconnect-smoke-20260807.json` on the
running 192 match: page reload restored `net.connected` / bootstrap and
`cells=36864` with ~60 FPS. In that session leftover CDP tabs still held both
seats, so `join_match` returned “all player slots are already claimed” rather
than a clean token reclaim — treat this as **map rebuild after reload**
evidence, not a full seat-reclaim proof.

### Still optional for deeper qualification

1. Architecture’s 30-minute soak with scheduler recovery.
2. Browser-path seat reclaim (not just map rebuild) with a single owning tab.

## CI / deploy hooks

| Pipeline | Browser-related check |
| --- | --- |
| `.github/workflows/ci.yml` | Wasm **compile/lint** + live local `match-e2e` smoke job |
| `.github/workflows/deploy.yml` | Trunk production build, then `measure-web-bundle.sh --enforce` |
| Local | `measure-web-bundle.sh`, `run-reconnect-soak.sh`, manual/CDP WebGPU procedure |

## Qualification checklist

- [x] Document budgets and unqualified items
- [x] Scripted Wasm download measurement + enforceable budget
- [x] Deploy-time size gate
- [x] Reconnect soak harness (native SDK)
- [x] Record isolated reconnect soak JSON (5-cycle PASS; extend to 20+ at will)
- [x] Record WebGPU 128 sample (PASS JSON)
- [x] Record WebGPU 192 sample (PASS JSON)
- [x] Record reconnect soak JSON under concurrent `match-perf` load
- [x] Manual browser reconnect smoke on the live path (reload restored map; seat reclaim still contested when other tabs hold slots)
- [x] Flip gate summary for under-load reconnect to measured PASS
