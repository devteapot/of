# Project design documentation

This directory is the source of truth for the implemented V1 design and its
deliberately deferred ideas, so later possibilities do not silently expand the
current interaction or simulation contract.

## Documents

- [Cluster-first troop controls](./cluster-controls.md) — canonical V1 input,
  action, Share, strategic-front, Reshape, and Stop contract.
- [V1 game design](./v1-game-design.md) — player experience, rules, interaction experiments, provisional tuning, and acceptance criteria.
- [Technical architecture](./technical-architecture.md) — Bevy and SpacetimeDB boundaries, simulation and data flow, scalability, testing, and the implementation sequence.
- [V1 implementation guide](./implementation.md) — executable components, runtime topology, operational flow, evidence, and known limits.
- [Graybox UI direction](./v1-ui-direction.md) — implemented interaction states, controls, HUD hierarchy, overlays, and playtest risks.
- [Graybox UI brief](./v1-ui-brief.md) — concise requirements for visual and
  interaction exploration within the canonical control contract.
- [Future ideas](./future-ideas.md) — explicitly deferred mechanics and research topics that should not be forgotten or treated as V1 commitments.

## Document status

These are living design documents. A statement marked **Locked** is the current implementation target. A statement marked **Provisional** is expected to change through playtesting. Anything in the future-ideas document is out of V1 unless it is deliberately promoted into the V1 design.

When a troop-control decision changes, update `cluster-controls.md` first and
then align the design, architecture, implementation, and UI documents in the
same change. Do not rely on conversation history as the only record.
