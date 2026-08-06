use hex_core::{Axial, HexEdge};
use serde::{Deserialize, Serialize};

use super::{
    Biome, CellLayers, EdgeLayers, GameplayCell, Landform, LayeredWorld, PassProvenance, RiverCell,
    Surface, TerrainTags, WorldLayer, WorldSpec,
    passes::{
        BiomePass, CoastConnectivityPass, ContinentPass, GameplayPass, HydrologyPass,
        LakeBasinPass, LandformPass, MountainPass, SpawnPass,
    },
};

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct PassReport {
    pub name: String,
    pub changed_cells: usize,
    pub changed_edges: usize,
    pub notes: Vec<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ElevationWrite {
    Set(i16),
    Add(i16),
}

#[derive(Clone, Debug, Default)]
pub struct WorldPatch {
    pub elevation: Vec<(u32, ElevationWrite)>,
    pub surfaces: Vec<(u32, Surface)>,
    pub landforms: Vec<(u32, Landform)>,
    pub biomes: Vec<(u32, Biome)>,
    pub moisture: Vec<(u32, u8)>,
    pub fertility: Vec<(u32, u8)>,
    pub water_bodies: Vec<(u32, Option<u32>)>,
    pub rivers: Vec<(u32, Option<RiverCell>)>,
    pub tags: Vec<(u32, TerrainTags)>,
    pub gameplay: Vec<(u32, GameplayCell)>,
    pub edges: Vec<(HexEdge, EdgeLayers)>,
    pub spawns: Option<Vec<Axial>>,
    pub report: PassReport,
}

pub trait WorldPass {
    fn name(&self) -> &'static str;

    fn reads(&self) -> &'static [WorldLayer] {
        &[]
    }

    fn writes(&self) -> &'static [WorldLayer] {
        &[]
    }

    /// Computes typed writes without mutating the input world.
    ///
    /// # Errors
    ///
    /// Returns an error when the pass cannot satisfy its layer contract.
    fn run(&self, world: &LayeredWorld, pass_seed: u64) -> Result<WorldPatch, String>;
}

pub struct WorldPipeline {
    passes: Vec<Box<dyn WorldPass>>,
}

impl Default for WorldPipeline {
    fn default() -> Self {
        Self::new()
    }
}

impl WorldPipeline {
    #[must_use]
    pub const fn new() -> Self {
        Self { passes: Vec::new() }
    }

    #[must_use]
    pub fn default_v2() -> Self {
        Self {
            passes: vec![
                Box::new(ContinentPass),
                Box::new(CoastConnectivityPass),
                Box::new(MountainPass),
                Box::new(LakeBasinPass),
                Box::new(HydrologyPass),
                Box::new(LandformPass),
                Box::new(BiomePass),
                Box::new(GameplayPass),
                Box::new(SpawnPass),
            ],
        }
    }

    #[must_use]
    pub fn with_pass(mut self, pass: impl WorldPass + 'static) -> Self {
        self.passes.push(Box::new(pass));
        self
    }

    pub fn push_pass(&mut self, pass: impl WorldPass + 'static) {
        self.passes.push(Box::new(pass));
    }

    /// Executes every pass in order and records pass provenance in the manifest.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid specification, a failed pass, or an
    /// out-of-bounds patch write.
    pub fn run(&self, spec: &WorldSpec) -> Result<(LayeredWorld, Vec<PassReport>), String> {
        spec.validate()?;
        let mut world = LayeredWorld::empty(spec);
        let mut reports = Vec::with_capacity(self.passes.len());
        for pass in &self.passes {
            let seed = super::noise::pass_seed(spec.seed, pass.name());
            let mut patch = pass.run(&world, seed)?;
            pass.name().clone_into(&mut patch.report.name);
            let report = patch.report.clone();
            apply_patch(&mut world, patch)?;
            world.manifest.pipeline.push(PassProvenance {
                name: pass.name().to_owned(),
                seed,
                reads: pass.reads().to_vec(),
                writes: pass.writes().to_vec(),
            });
            reports.push(report);
        }
        world.refresh_manifest_counts();
        world.manifest.content_hash = content_hash(&world);
        Ok((world, reports))
    }
}

/// Generates a layered map with the standard version-two pass sequence.
///
/// # Errors
///
/// Returns a pass or validation error when a valid layered map cannot be built.
pub fn generate(spec: &WorldSpec) -> Result<LayeredWorld, String> {
    let (world, _) = WorldPipeline::default_v2().run(spec)?;
    super::validation::validate(&world)?;
    Ok(world)
}

fn apply_patch(world: &mut LayeredWorld, patch: WorldPatch) -> Result<(), String> {
    {
        let mut write_cell = |cell_id: u32, update: &mut dyn FnMut(&mut CellLayers)| {
            let cell = world
                .cells_mut()
                .get_mut(cell_id as usize)
                .ok_or_else(|| format!("pass wrote out-of-bounds cell {cell_id}"))?;
            update(cell);
            Ok::<_, String>(())
        };
        for (cell_id, write) in patch.elevation {
            write_cell(cell_id, &mut |cell| match write {
                ElevationWrite::Set(value) => cell.elevation = value,
                ElevationWrite::Add(value) => cell.elevation = cell.elevation.saturating_add(value),
            })?;
        }
        for (cell_id, value) in patch.surfaces {
            write_cell(cell_id, &mut |cell| cell.surface = value)?;
        }
        for (cell_id, value) in patch.landforms {
            write_cell(cell_id, &mut |cell| cell.landform = value)?;
        }
        for (cell_id, value) in patch.biomes {
            write_cell(cell_id, &mut |cell| cell.biome = value)?;
        }
        for (cell_id, value) in patch.moisture {
            write_cell(cell_id, &mut |cell| cell.moisture = value)?;
        }
        for (cell_id, value) in patch.fertility {
            write_cell(cell_id, &mut |cell| cell.fertility = value)?;
        }
        for (cell_id, value) in patch.water_bodies {
            write_cell(cell_id, &mut |cell| cell.water_body_id = value)?;
        }
        for (cell_id, value) in patch.rivers {
            write_cell(cell_id, &mut |cell| cell.river = value)?;
        }
        for (cell_id, value) in patch.tags {
            write_cell(cell_id, &mut |cell| cell.tags.0 |= value.0)?;
        }
        for (cell_id, value) in patch.gameplay {
            write_cell(cell_id, &mut |cell| cell.gameplay = value)?;
        }
    }
    for (edge, value) in patch.edges {
        world.edges.insert(edge, value);
    }
    if let Some(spawns) = patch.spawns {
        world.manifest.spawn_cells = spawns;
    }
    Ok(())
}

pub(crate) fn content_hash(world: &LayeredWorld) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    let mut value = |number: u64| {
        for byte in number.to_le_bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01B3);
        }
    };
    value(u64::from(world.width()));
    value(u64::from(world.height()));
    value(world.manifest.seed);
    value(u64::from(world.manifest.player_count));
    for cell in world.cells() {
        value(u64::from(u16::from_ne_bytes(cell.elevation.to_ne_bytes())));
        value(cell.surface as u64);
        value(cell.landform as u64);
        value(cell.biome as u64);
        value(u64::from(cell.moisture));
        value(u64::from(cell.fertility));
        value(u64::from(cell.water_body_id.unwrap_or_default()));
        value(u64::from(cell.tags.0));
        if let Some(river) = cell.river {
            value(1);
            value(river.outflow as u64);
            value(u64::from(river.inflow_mask));
            value(u64::from(river.order));
            value(u64::from(river.discharge));
        } else {
            value(0);
        }
        value(u64::from(cell.gameplay.passable));
        value(u64::from(cell.gameplay.capturable));
        value(u64::from(cell.gameplay.habitable));
        value(u64::from(cell.gameplay.movement_cost));
        value(u64::from(cell.gameplay.military_capacity));
        value(u64::from(cell.gameplay.civilian_capacity));
    }
    for (edge, layers) in &world.edges {
        value(u64::from(u32::from_ne_bytes(edge.a.q.to_ne_bytes())));
        value(u64::from(u32::from_ne_bytes(edge.a.r.to_ne_bytes())));
        value(u64::from(u32::from_ne_bytes(edge.b.q.to_ne_bytes())));
        value(u64::from(u32::from_ne_bytes(edge.b.r.to_ne_bytes())));
        value(u64::from(layers.road_level));
        value(layers.crossing as u64);
        value(u64::from(u16::from_ne_bytes(
            layers.movement_modifier_bps.to_ne_bytes(),
        )));
    }
    for spawn in &world.manifest.spawn_cells {
        value(u64::from(u32::from_ne_bytes(spawn.q.to_ne_bytes())));
        value(u64::from(u32::from_ne_bytes(spawn.r.to_ne_bytes())));
    }
    hash
}
