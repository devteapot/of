use std::collections::BTreeMap;

use hex_core::{Axial, ChunkCoord, HexDirection, HexEdge};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};

use super::GENERATOR_VERSION;

pub const DEFAULT_CHUNK_SIZE: u16 = 64;
pub const DEFAULT_MACRO_CELL_SIZE: u16 = 32;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct WorldParameters {
    pub mountain_density_bps: u16,
    pub lake_depth_threshold: i16,
    pub river_threshold: u32,
}

impl Default for WorldParameters {
    fn default() -> Self {
        Self {
            mountain_density_bps: 3_000,
            lake_depth_threshold: 18,
            river_threshold: 0,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct WorldSpec {
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub seed: u64,
    pub player_count: u16,
    pub chunk_size: u16,
    pub macro_cell_size: u16,
    pub parameters: WorldParameters,
}

impl WorldSpec {
    #[must_use]
    pub fn new(name: impl Into<String>, width: u32, height: u32, seed: u64) -> Self {
        Self {
            name: name.into(),
            width,
            height,
            seed,
            player_count: 2,
            chunk_size: DEFAULT_CHUNK_SIZE,
            macro_cell_size: DEFAULT_MACRO_CELL_SIZE,
            parameters: WorldParameters::default(),
        }
    }

    /// Checks dimensions, IDs, player count, and pass configuration.
    ///
    /// # Errors
    ///
    /// Returns a descriptive error when the specification cannot be represented
    /// by the layered map contract.
    pub fn validate(&self) -> Result<(), String> {
        if self.width < 24 || self.height < 24 {
            return Err("layered maps must be at least 24 by 24".to_owned());
        }
        if !(2..=500).contains(&self.player_count) {
            return Err("player count must be between 2 and 500".to_owned());
        }
        if self.chunk_size == 0 || self.macro_cell_size == 0 {
            return Err("chunk and macro-cell sizes must be nonzero".to_owned());
        }
        if self.parameters.mountain_density_bps > 10_000 {
            return Err("mountain density must not exceed 10,000 basis points".to_owned());
        }
        if self.parameters.lake_depth_threshold <= 0 {
            return Err("lake depth threshold must be positive".to_owned());
        }
        if self.width > i32::MAX as u32 || self.height > i32::MAX as u32 {
            return Err("map dimensions exceed axial coordinate storage".to_owned());
        }
        let count = u64::from(self.width) * u64::from(self.height);
        if count >= u64::from(u32::MAX) {
            return Err("layered maps must contain fewer than u32::MAX cells".to_owned());
        }
        Ok(())
    }

    #[must_use]
    pub const fn q_min(&self) -> i32 {
        -(self.width as i32 / 2)
    }

    #[must_use]
    pub const fn r_min(&self) -> i32 {
        -(self.height as i32 / 2)
    }

    #[must_use]
    pub const fn cell_count(&self) -> usize {
        self.width as usize * self.height as usize
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[repr(u8)]
pub enum Surface {
    #[default]
    Land,
    Ocean,
    Lake,
}

#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[repr(u8)]
pub enum Landform {
    #[default]
    Plain,
    Hill,
    Mountain,
    Valley,
    Plateau,
}

#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[repr(u8)]
pub enum Biome {
    #[default]
    TemperateGrassland,
    Forest,
    Wetland,
    Dryland,
    Alpine,
    Tundra,
}

#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TerrainTags(pub u16);

impl TerrainTags {
    pub const COAST: u16 = 1 << 0;
    pub const RIVERBANK: u16 = 1 << 1;
    pub const SOURCE: u16 = 1 << 2;
    pub const OUTLET: u16 = 1 << 3;

    pub fn insert(&mut self, flag: u16) {
        self.0 |= flag;
    }

    #[must_use]
    pub const fn contains(self, flag: u16) -> bool {
        self.0 & flag != 0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RiverCell {
    /// Direction toward the next river, lake, or ocean cell.
    pub outflow: HexDirection,
    /// Bit `HexDirection::index()` is set for every upstream river neighbor.
    pub inflow_mask: u8,
    pub order: u8,
    pub discharge: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct GameplayCell {
    pub passable: bool,
    pub capturable: bool,
    pub habitable: bool,
    pub movement_cost: u16,
    pub military_capacity: u16,
    pub civilian_capacity: u16,
}

impl Default for GameplayCell {
    fn default() -> Self {
        Self {
            passable: true,
            capturable: true,
            habitable: true,
            movement_cost: 10,
            military_capacity: 100,
            civilian_capacity: 100,
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct CellLayers {
    pub elevation: i16,
    pub surface: Surface,
    pub landform: Landform,
    pub biome: Biome,
    pub moisture: u8,
    pub fertility: u8,
    pub water_body_id: Option<u32>,
    pub river: Option<RiverCell>,
    pub tags: TerrainTags,
    pub gameplay: GameplayCell,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[repr(u8)]
pub enum Crossing {
    #[default]
    None,
    Ford,
    Bridge,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct EdgeLayers {
    pub road_level: u8,
    pub crossing: Crossing,
    pub movement_modifier_bps: i16,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub enum WorldLayer {
    Elevation,
    Surface,
    Landform,
    Biome,
    Moisture,
    Fertility,
    Hydrology,
    Tags,
    Gameplay,
    Edges,
    Spawns,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PassProvenance {
    pub name: String,
    pub seed: u64,
    pub reads: Vec<WorldLayer>,
    pub writes: Vec<WorldLayer>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct V2Manifest {
    pub name: String,
    pub generator_version: u16,
    pub width: u32,
    pub height: u32,
    pub q_min: i32,
    pub r_min: i32,
    pub seed: u64,
    pub player_count: u16,
    pub chunk_size: u16,
    pub macro_cell_size: u16,
    pub parameters: WorldParameters,
    pub pipeline: Vec<PassProvenance>,
    pub content_hash: u64,
    pub land_cells: u32,
    pub lake_cells: u32,
    pub river_cells: u32,
    pub spawn_cells: Vec<Axial>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LayeredWorld {
    pub manifest: V2Manifest,
    cells: Vec<CellLayers>,
    pub edges: BTreeMap<HexEdge, EdgeLayers>,
}

impl Serialize for LayeredWorld {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        #[derive(Serialize)]
        struct WireWorld<'a> {
            manifest: &'a V2Manifest,
            cells: &'a [CellLayers],
            edges: Vec<(&'a HexEdge, &'a EdgeLayers)>,
        }

        WireWorld {
            manifest: &self.manifest,
            cells: &self.cells,
            edges: self.edges.iter().collect(),
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for LayeredWorld {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct WireWorld {
            manifest: V2Manifest,
            cells: Vec<CellLayers>,
            edges: Vec<(HexEdge, EdgeLayers)>,
        }

        let wire = WireWorld::deserialize(deserializer)?;
        let mut edges = BTreeMap::new();
        for (edge, layers) in wire.edges {
            if edges.insert(edge, layers).is_some() {
                return Err(D::Error::custom(format!(
                    "duplicate layered edge {:?}->{:?}",
                    edge.a, edge.b
                )));
            }
        }
        Ok(Self {
            manifest: wire.manifest,
            cells: wire.cells,
            edges,
        })
    }
}

impl LayeredWorld {
    pub(crate) fn empty(spec: &WorldSpec) -> Self {
        Self {
            manifest: V2Manifest {
                name: spec.name.clone(),
                generator_version: GENERATOR_VERSION,
                width: spec.width,
                height: spec.height,
                q_min: spec.q_min(),
                r_min: spec.r_min(),
                seed: spec.seed,
                player_count: spec.player_count,
                chunk_size: spec.chunk_size,
                macro_cell_size: spec.macro_cell_size,
                parameters: spec.parameters.clone(),
                pipeline: Vec::new(),
                content_hash: 0,
                land_cells: 0,
                lake_cells: 0,
                river_cells: 0,
                spawn_cells: Vec::new(),
            },
            cells: vec![CellLayers::default(); spec.cell_count()],
            edges: BTreeMap::new(),
        }
    }

    #[must_use]
    pub const fn width(&self) -> u32 {
        self.manifest.width
    }

    #[must_use]
    pub const fn height(&self) -> u32 {
        self.manifest.height
    }

    #[must_use]
    pub fn cells(&self) -> &[CellLayers] {
        &self.cells
    }

    #[must_use]
    pub fn cells_mut(&mut self) -> &mut [CellLayers] {
        &mut self.cells
    }

    #[must_use]
    pub fn cell_id(&self, coordinate: Axial) -> Option<u32> {
        let column = coordinate.q.checked_sub(self.manifest.q_min)?;
        let row = coordinate.r.checked_sub(self.manifest.r_min)?;
        if column < 0
            || row < 0
            || column >= self.manifest.width as i32
            || row >= self.manifest.height as i32
        {
            return None;
        }
        Some(row as u32 * self.manifest.width + column as u32)
    }

    #[must_use]
    pub fn coordinate(&self, cell_id: u32) -> Option<Axial> {
        if cell_id as usize >= self.cells.len() {
            return None;
        }
        let column = cell_id % self.manifest.width;
        let row = cell_id / self.manifest.width;
        Some(Axial::new(
            self.manifest.q_min + column as i32,
            self.manifest.r_min + row as i32,
        ))
    }

    #[must_use]
    pub fn cell(&self, coordinate: Axial) -> Option<&CellLayers> {
        self.cell_id(coordinate)
            .and_then(|cell_id| self.cells.get(cell_id as usize))
    }

    pub fn cell_mut(&mut self, coordinate: Axial) -> Option<&mut CellLayers> {
        let cell_id = self.cell_id(coordinate)?;
        self.cells.get_mut(cell_id as usize)
    }

    #[must_use]
    pub fn neighbor_id(&self, cell_id: u32, direction: HexDirection) -> Option<u32> {
        let coordinate = self.coordinate(cell_id)?;
        self.cell_id(coordinate.neighbor(direction))
    }

    #[must_use]
    pub fn chunks_wide(&self) -> u32 {
        self.width().div_ceil(u32::from(self.manifest.chunk_size))
    }

    #[must_use]
    pub fn chunks_high(&self) -> u32 {
        self.height().div_ceil(u32::from(self.manifest.chunk_size))
    }

    /// Extracts one zero-based storage chunk, including sparse edges touching it.
    #[must_use]
    pub fn chunk(&self, coordinate: ChunkCoord) -> Option<TerrainChunk> {
        if coordinate.q < 0 || coordinate.r < 0 {
            return None;
        }
        let size = u32::from(self.manifest.chunk_size);
        let column_start = u32::try_from(coordinate.q).ok()?.checked_mul(size)?;
        let row_start = u32::try_from(coordinate.r).ok()?.checked_mul(size)?;
        if column_start >= self.width() || row_start >= self.height() {
            return None;
        }
        let width = size.min(self.width() - column_start);
        let height = size.min(self.height() - row_start);
        let mut cells = Vec::with_capacity(width as usize * height as usize);
        for local_row in 0..height {
            for local_column in 0..width {
                let cell_id = (row_start + local_row) * self.width() + column_start + local_column;
                let coordinate = self.coordinate(cell_id)?;
                cells.push(ChunkCell {
                    cell_id,
                    coordinate,
                    layers: self.cells[cell_id as usize].clone(),
                });
            }
        }
        let q_min = self.manifest.q_min + column_start as i32;
        let r_min = self.manifest.r_min + row_start as i32;
        let q_max = q_min + width as i32;
        let r_max = r_min + height as i32;
        let edges = self
            .edges
            .iter()
            .filter(|(edge, _)| {
                let contains = |cell: Axial| {
                    cell.q >= q_min && cell.q < q_max && cell.r >= r_min && cell.r < r_max
                };
                contains(edge.a) || contains(edge.b)
            })
            .map(|(edge, layers)| (*edge, layers.clone()))
            .collect();
        Some(TerrainChunk {
            coordinate,
            width,
            height,
            cells,
            edges,
        })
    }

    pub(crate) fn refresh_manifest_counts(&mut self) {
        self.manifest.land_cells = self
            .cells
            .iter()
            .filter(|cell| cell.surface == Surface::Land)
            .count() as u32;
        self.manifest.lake_cells = self
            .cells
            .iter()
            .filter(|cell| cell.surface == Surface::Lake)
            .count() as u32;
        self.manifest.river_cells = self
            .cells
            .iter()
            .filter(|cell| cell.river.is_some())
            .count() as u32;
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ChunkCell {
    pub cell_id: u32,
    pub coordinate: Axial,
    pub layers: CellLayers,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TerrainChunk {
    pub coordinate: ChunkCoord,
    pub width: u32,
    pub height: u32,
    pub cells: Vec<ChunkCell>,
    pub edges: Vec<(HexEdge, EdgeLayers)>,
}
