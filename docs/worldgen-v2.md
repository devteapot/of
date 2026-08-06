# Layered world generator V2

Status: implemented offline generator contract; authoritative match integration
is deliberately separate from the pinned V1 map format.

## Purpose

V2 separates geography into composable layers so one cell can be a fertile
temperate plain, carry a river, and participate in an independently defined
road/crossing edge. It also replaces the generator's ordered per-cell tree with
dense row-major storage and chunk extraction suitable for maps far larger than
the 192 x 192 V1 validation preset.

V1 remains unchanged and continues to produce its pinned hashes. V2 uses
generator version `2` and has its own manifest, content hash, pass provenance,
and validation report.

## Layer contract

Each cell carries independent fields for:

- elevation;
- surface (`Land`, `Ocean`, or `Lake`);
- landform (`Plain`, `Hill`, `Mountain`, `Valley`, or `Plateau`);
- biome, moisture, and fertility;
- optional water-body and river metadata;
- composable tags;
- derived gameplay properties.

Sparse canonical edges independently carry roads, crossings, and movement
modifiers. Rivers are centerline hydrology overlays with upstream masks and an
outflow direction; they do not replace the underlying landform or biome.

Every built-in pass declares the layers it reads and writes. The generated
manifest records the pass name, independently derived seed, read set, and write
set. Elevation writes explicitly choose set or additive semantics, surface and
classification writes replace only their own field, and tags merge by union.
Custom passes implement `WorldPass` and return a `WorldPatch`.

## Default pipeline

1. `continent` creates a global elevation field and coherent interpolated coast.
2. `coast-connectivity` bridges nearby fragments and removes remote specks.
3. `mountains` adds area-scaled regional ridges.
4. `lake-basins` carves deterministic inland basins.
5. `hydrology` priority-fills depressions, labels lakes, calculates flow and
   upstream discharge, and retains river networks that terminate in water.
6. `landforms` classifies the final relief without touching hydrology.
7. `biomes` derives climate, biome, and fertility.
8. `gameplay` derives passability, capacity, and the main connected play area.
9. `spawns` uses bounded-candidate farthest-point sampling rather than scanning
   every cell once per player.

Each pass receives `hash(world_seed, pass_name)`. Adding a later independent
pass therefore does not perturb the random choices of existing passes.

## CLI

The default remains V1:

```bash
cargo run -p mapgen -- --preset validation
```

Generate and validate a custom layered map:

```bash
cargo run -p mapgen --release -- \
  --generator v2 --width 1024 --height 1024 --players 500 --seed 42
```

Tune geographic components:

```bash
cargo run -p mapgen --release -- \
  --generator v2 --width 512 --height 512 \
  --mountain-density-bps 3500 \
  --lake-depth-threshold 20 \
  --river-threshold 0
```

`--river-threshold 0` selects the area-scaled default.

Export a layer as a binary portable graymap:

```bash
cargo run -p mapgen -- --generator v2 --width 512 --height 512 \
  --inspect-layer rivers --inspect-output /tmp/rivers.pgm
```

Supported inspection layers are elevation, surface, landform, biome, moisture,
fertility, rivers, and gameplay.

Export a manifest and deterministic JSON chunks:

```bash
cargo run -p mapgen --release -- --generator v2 \
  --width 1024 --height 1024 --chunk-size 64 \
  --chunks-dir /tmp/layered-world
```

Chunk coordinates are zero-based storage coordinates. Each chunk includes its
cells and any sparse edge records touching it. JSON is intended for inspection;
a production client/server handoff should add a packed compressed encoding
without changing the layered model or content identity.

## Validation and scale

Validation checks manifest/hash consistency, layer compatibility, connected
water-body IDs, river reciprocity/discharge/termination/cycle freedom, spawn
uniqueness and suitability, connected playable land, and exact chunk coverage.
Tests cover V1 compatibility, deterministic layer composition, plains carrying
rivers, independent edge-pass composition, lakes, rivers crossing chunk
boundaries, and a 256 x 256 scale fixture.

On the development machine, the release generator produced and validated a
1024 x 1024 / 500-player fixture in approximately 0.75 seconds with about 92 MB
peak resident memory. This measures the offline generator only; the current
SpacetimeDB schema and Bevy client still materialize full per-cell/per-edge
state and must not switch to V2 at that size without runtime terrain streaming,
sparse wave topology, and map-size-based subscription/simulation policies.

## Runtime integration boundary

V2 is intentionally not selected by the match reducer yet. Safe integration
requires a versioned terrain adapter and generated bindings, followed by:

- packed immutable chunk storage or deterministic client-side regeneration;
- spatial terrain interest and a resident-set renderer;
- dynamic state only for playable land;
- compact static topology instead of one database row per adjacent edge;
- scale decisions based on map cells/chunks as well as player count;
- sparse expansion-wave topology and symbolic large-component commands.

Keeping this boundary explicit prevents an offline million-cell success from
being mistaken for a playable million-cell authoritative match.
