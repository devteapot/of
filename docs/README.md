# Project design documentation

This directory is the source of truth for the game decisions made before implementation. The documents deliberately separate the first playable version from later ideas so that promising extensions do not silently expand V1.

## Documents

- [V1 game design](./v1-game-design.md) — player experience, rules, interaction experiments, provisional tuning, and acceptance criteria.
- [Technical architecture](./technical-architecture.md) — Bevy and SpacetimeDB boundaries, simulation and data flow, scalability, testing, and the implementation sequence.
- [V1 implementation guide](./implementation.md) — executable components, runtime topology, operational flow, evidence, and known limits.
- [Graybox UI direction](./v1-ui-direction.md) — implemented interaction states, controls, HUD hierarchy, overlays, and playtest risks.
- [Future ideas](./future-ideas.md) — explicitly deferred mechanics and research topics that should not be forgotten or treated as V1 commitments.

## Document status

These are living design documents. A statement marked **Locked** is the current implementation target. A statement marked **Provisional** is expected to change through playtesting. Anything in the future-ideas document is out of V1 unless it is deliberately promoted into the V1 design.

When a decision changes, update the relevant document in the same change as the implementation. Do not rely on conversation history as the only record.
