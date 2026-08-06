//! Layered, chunk-addressable deterministic world generation.
//!
//! Version one remains the compatibility generator used by existing matches.
//! This module provides the version-two contract: independent physical,
//! hydrology, biome, edge, and gameplay layers composed by typed passes.

// WorldSpec validates dimensions and total cell count before any generator
// pass runs. Keeping cell IDs as u32 and coordinates as i32 is the serialized
// contract, so conversions inside that validated boundary are intentional.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]

mod model;
mod noise;
mod passes;
mod pipeline;
mod validation;

pub use model::{
    Biome, CellLayers, ChunkCell, Crossing, EdgeLayers, GameplayCell, Landform, LayeredWorld,
    PassProvenance, RiverCell, Surface, TerrainChunk, TerrainTags, V2Manifest, WorldLayer,
    WorldParameters, WorldSpec,
};
pub use passes::{
    BiomePass, CoastConnectivityPass, ContinentPass, GameplayPass, HydrologyPass, LakeBasinPass,
    LandformPass, MountainPass, SpawnPass,
};
pub use pipeline::{ElevationWrite, PassReport, WorldPass, WorldPatch, WorldPipeline, generate};
pub use validation::{V2ValidationReport, validate};

/// Layered map format and default pipeline version.
pub const GENERATOR_VERSION: u16 = 2;

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use hex_core::{HexDirection, HexEdge};

    use super::*;

    fn spec(size: u32, seed: u64) -> WorldSpec {
        let mut spec = WorldSpec::new(format!("layered-{size}"), size, size, seed);
        spec.player_count = 8;
        spec.chunk_size = 32;
        spec
    }

    #[test]
    fn layered_generation_is_deterministic_composed_and_valid() {
        let spec = spec(96, 42);
        let first = generate(&spec).expect("layered map should generate");
        let second = generate(&spec).expect("layered map should repeat");
        assert_eq!(first, second);
        let report = validate(&first).expect("layered map should validate");
        assert_eq!(report.total_cells, 96 * 96);
        assert!(report.land_cells > 0);
        assert_eq!(first.manifest.generator_version, GENERATOR_VERSION);
        assert_eq!(first.manifest.pipeline.len(), 9);
        assert_eq!(first.manifest.pipeline[0].name, "continent");
        assert!(
            first.manifest.pipeline[0]
                .writes
                .contains(&WorldLayer::Elevation)
        );
        assert!(
            first
                .cells()
                .iter()
                .any(|cell| cell.surface == Surface::Land && cell.river.is_some())
        );
        assert!(first.cells().iter().any(|cell| {
            cell.surface == Surface::Land
                && cell.river.is_some()
                && matches!(
                    cell.landform,
                    Landform::Plain | Landform::Valley | Landform::Hill
                )
        }));
    }

    #[test]
    fn lakes_are_connected_and_have_stable_water_body_ids() {
        let mut spec = spec(128, 7);
        spec.parameters.lake_depth_threshold = 6;
        let world = generate(&spec).expect("lake-rich map should generate");
        let ids = world
            .cells()
            .iter()
            .filter(|cell| cell.surface == Surface::Lake)
            .map(|cell| cell.water_body_id.expect("lake has water-body id"))
            .collect::<BTreeSet<_>>();
        assert!(!ids.is_empty());
        assert_eq!(
            validate(&world)
                .expect("lakes should validate")
                .water_bodies,
            ids.len()
        );
    }

    #[test]
    fn river_connections_remain_consistent_across_chunk_boundaries() {
        let world = generate(&spec(192, 99)).expect("layered map should generate");
        let mut crossed_boundary = false;
        for (cell_id, cell) in world.cells().iter().enumerate() {
            let Some(river) = cell.river else {
                continue;
            };
            let next = world
                .neighbor_id(cell_id as u32, river.outflow)
                .expect("validated outflow");
            let first = world.coordinate(cell_id as u32).expect("cell coordinate");
            let second = world.coordinate(next).expect("next coordinate");
            let size = u32::from(world.manifest.chunk_size);
            let chunk_for = |coordinate: hex_core::Axial| hex_core::ChunkCoord {
                q: (coordinate.q - world.manifest.q_min).div_euclid(size as i32),
                r: (coordinate.r - world.manifest.r_min).div_euclid(size as i32),
            };
            if chunk_for(first) != chunk_for(second) {
                crossed_boundary = true;
                let first_chunk = chunk_for(first);
                let extracted = world.chunk(first_chunk).expect("source chunk");
                assert!(
                    extracted
                        .cells
                        .iter()
                        .any(|cell| cell.cell_id == cell_id as u32)
                );
            }
        }
        assert!(
            crossed_boundary,
            "fixture should exercise a cross-chunk river"
        );
    }

    struct RoadPass;

    impl WorldPass for RoadPass {
        fn name(&self) -> &'static str {
            "test-roads"
        }

        fn run(&self, world: &LayeredWorld, _seed: u64) -> Result<WorldPatch, String> {
            let first = world.manifest.spawn_cells[0];
            let second = HexDirection::ALL
                .into_iter()
                .map(|direction| first.neighbor(direction))
                .find(|neighbor| {
                    world
                        .cell(*neighbor)
                        .is_some_and(|cell| cell.gameplay.passable)
                })
                .ok_or_else(|| "spawn has no road neighbor".to_owned())?;
            Ok(WorldPatch {
                edges: vec![(
                    HexEdge::new(first, second).expect("adjacent road"),
                    EdgeLayers {
                        road_level: 1,
                        ..EdgeLayers::default()
                    },
                )],
                ..WorldPatch::default()
            })
        }
    }

    #[test]
    fn independent_edge_pass_composes_without_replacing_cell_layers() {
        let spec = spec(96, 123);
        let base = generate(&spec).expect("base map");
        let (composed, _) = WorldPipeline::default_v2()
            .with_pass(RoadPass)
            .run(&spec)
            .expect("road pass");
        assert_eq!(base.cells(), composed.cells());
        assert_eq!(composed.edges.len(), 1);
        let json = serde_json::to_vec(&composed).expect("edge map should serialize as an array");
        let decoded: LayeredWorld =
            serde_json::from_slice(&json).expect("edge map should deserialize");
        assert_eq!(decoded, composed);
    }

    #[test]
    fn scale_fixture_256_square_is_chunk_complete() {
        let world = generate(&spec(256, 2026)).expect("scale fixture should generate");
        let report = validate(&world).expect("scale fixture should validate");
        assert_eq!(report.total_cells, 256 * 256);
        assert_eq!(report.chunks, 64);
    }
}
