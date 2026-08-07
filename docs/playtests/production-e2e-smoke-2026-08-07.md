# Production E2E smoke — 2026-08-07

Target: `https://of.carlid.dev` + Maincloud `of-lobby` / provisioned match DB.

## Procedure

1. `GET /api/lobbies` — empty directory.
2. Create two anonymous sessions via `POST /api/session`.
3. Create lobby `fbd65f9a` (small / 2 players) as smoke-p1 → provisioned
   `of-match-fbd65f9a`, status `open`.
4. Join as smoke-p2 → status `full`, memberCount 2.
5. Confirm lobby listed; no Pending/Provisioning orphans.
6. `match-e2e` against Maincloud with session tokens:
   - Full gameplay smoke joined and started, then failed a map-geometry
     front-push progression assert (not a provisioning/start failure).
   - `--reconnect-only --reconnect-cycles 5` **PASS** (p50 1278 ms, p95 1328 ms,
     max 1336 ms; budget ≤ 5000 ms).
7. Leave p2 then p1 → lobby row deleted; directory empty again.
8. Best-effort `spacetime delete --server maincloud of-match-fbd65f9a`.

## Result

**PASS** for create/join/start/reconnect/leave cleanup of lobby rows.
No orphan Pending lobbies remained.

Raw JSON (gitignored): `artifacts/browser/production-e2e-smoke-2026-08-07.json`,
`artifacts/browser/production-reconnect-smoke.json`.

## Gaps

- Browser WebGPU play on production was not exercised in this pass (API + SDK).
- Match-database auto-delete on leave requires the Vercel cleanup deploy from
  this branch; smoke used CLI delete as belt-and-suspenders.
- Full gameplay e2e geometry assert is flaky on provisioned small maps; prefer
  reconnect-only + selective reducer checks for production smokes.
