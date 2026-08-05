use std::{
    collections::{BTreeMap, BTreeSet, VecDeque, hash_map::DefaultHasher},
    hash::{Hash, Hasher},
};

use bevy::{
    asset::RenderAssetUsages,
    color::{ColorToComponents, LinearRgba},
    mesh::Indices,
    prelude::*,
    render::render_resource::PrimitiveTopology,
};
use hex_core::{Axial, ChunkCoord, TerrainKind};

use crate::{
    geometry::{COLUMN_FLOOR, cell_top, corner, edge_index_for_direction, world_center},
    map_view::{MapViewMode, normalized_cell_value, normalized_soldier_strength},
    model::{CellView, ContestedCellView, MatchView},
};

/// Mesh creation is deliberately amortized: a future world-scale map may
/// contain hundreds or thousands of render chunks, but only nearby chunks
/// need to appear in the first frame.
const INITIAL_CHUNK_SPAWN_BUDGET: usize = 32;
const CHUNK_SPAWN_BUDGET_PER_FRAME: usize = 24;
/// Visible chunks get most of the update budget so changing a map view feels
/// immediate. Hidden chunks converge in the background without a frame spike.
const VISIBLE_COLOR_UPDATES_PER_FRAME: usize = 48;
const HIDDEN_COLOR_UPDATES_PER_FRAME: usize = 12;
/// Classification is also bounded. This prevents a map-view switch or a large
/// authoritative update from repartitioning every dirty chunk each frame.
const UPDATE_CLASSIFICATION_BUDGET_PER_FRAME: usize = 192;

#[derive(Component, Debug)]
pub struct TerrainChunk {
    pub coordinate: ChunkCoord,
    /// Unique, deterministic coordinates in this chunk. Overlays and the
    /// batched value layer use this instead of scanning every map cell.
    pub cells: Vec<Axial>,
    pub triangle_to_cell: Vec<Axial>,
    vertex_to_cell: Vec<Axial>,
    vertex_shades: Vec<f32>,
    geometry_fingerprint: u64,
}

#[derive(Resource)]
pub struct TerrainMaterial(Handle<StandardMaterial>);

/// Persistent bookkeeping for render chunks.
///
/// The registry is reconciled with `MatchView::cells_by_chunk` only when that
/// index's revision changes. Ordinary frames use direct entity lookups and
/// bounded queues instead of rebuilding desired/existing chunk sets.
#[derive(Resource, Debug)]
pub(crate) struct TerrainChunkRegistry {
    topology_revision: u64,
    rendered_mode: MapViewMode,
    ordered_coordinates: Vec<ChunkCoord>,
    entities: BTreeMap<ChunkCoord, Entity>,
    pending_spawns: VecDeque<ChunkCoord>,
    pending_spawn_set: BTreeSet<ChunkCoord>,
    visible_updates: VecDeque<ChunkCoord>,
    hidden_updates: VecDeque<ChunkCoord>,
    scheduled_updates: BTreeSet<ChunkCoord>,
    mode_sweep_cursor: Option<usize>,
}

impl TerrainChunkRegistry {
    fn new(
        topology_revision: u64,
        rendered_mode: MapViewMode,
        coordinates: impl IntoIterator<Item = ChunkCoord>,
    ) -> Self {
        let mut ordered_coordinates = coordinates.into_iter().collect::<Vec<_>>();
        sort_chunks_near_origin(&mut ordered_coordinates);
        let pending_spawns = ordered_coordinates.iter().copied().collect::<VecDeque<_>>();
        let pending_spawn_set = ordered_coordinates.iter().copied().collect();
        Self {
            topology_revision,
            rendered_mode,
            ordered_coordinates,
            entities: BTreeMap::new(),
            pending_spawns,
            pending_spawn_set,
            visible_updates: VecDeque::new(),
            hidden_updates: VecDeque::new(),
            scheduled_updates: BTreeSet::new(),
            mode_sweep_cursor: None,
        }
    }

    /// Returns removed entities. The caller owns their deferred despawn.
    fn reconcile(
        &mut self,
        topology_revision: u64,
        coordinates: impl IntoIterator<Item = ChunkCoord>,
    ) -> Vec<Entity> {
        debug_assert_ne!(self.topology_revision, topology_revision);
        let mut ordered_coordinates = coordinates.into_iter().collect::<Vec<_>>();
        sort_chunks_near_origin(&mut ordered_coordinates);
        let desired = ordered_coordinates.iter().copied().collect::<BTreeSet<_>>();
        let removed_coordinates = self
            .entities
            .keys()
            .filter(|coordinate| !desired.contains(coordinate))
            .copied()
            .collect::<Vec<_>>();
        let removed = removed_coordinates
            .into_iter()
            .filter_map(|coordinate| self.entities.remove(&coordinate))
            .collect();

        self.pending_spawns = ordered_coordinates
            .iter()
            .filter(|coordinate| !self.entities.contains_key(coordinate))
            .copied()
            .collect();
        self.pending_spawn_set = self.pending_spawns.iter().copied().collect();
        self.ordered_coordinates = ordered_coordinates;
        self.visible_updates.clear();
        self.hidden_updates.clear();
        self.scheduled_updates.clear();
        // Retained chunks may need a topology rebuild even when their chunk
        // coordinate survived the map replacement.
        self.mode_sweep_cursor = Some(0);
        self.topology_revision = topology_revision;
        removed
    }

    fn take_pending_spawns(&mut self, budget: usize) -> Vec<ChunkCoord> {
        let mut coordinates = Vec::with_capacity(budget.min(self.pending_spawns.len()));
        for _ in 0..budget {
            let Some(coordinate) = self.pending_spawns.pop_front() else {
                break;
            };
            self.pending_spawn_set.remove(&coordinate);
            coordinates.push(coordinate);
        }
        coordinates
    }

    fn register(&mut self, coordinate: ChunkCoord, entity: Entity) {
        self.entities.insert(coordinate, entity);
        self.pending_spawn_set.remove(&coordinate);
    }

    fn mark_missing(&mut self, coordinate: ChunkCoord) {
        self.entities.remove(&coordinate);
        self.scheduled_updates.remove(&coordinate);
        // This is only called for an entity already registered after the most
        // recent topology reconciliation, so the coordinate is still desired.
        if self.pending_spawn_set.insert(coordinate) {
            self.pending_spawns.push_back(coordinate);
        }
    }

    fn begin_mode_sweep(&mut self) {
        self.mode_sweep_cursor = Some(0);
    }

    fn take_mode_sweep(&mut self, budget: usize) -> Vec<ChunkCoord> {
        let Some(start) = self.mode_sweep_cursor else {
            return Vec::new();
        };
        let end = start
            .saturating_add(budget)
            .min(self.ordered_coordinates.len());
        let batch = self.ordered_coordinates[start..end].to_vec();
        self.mode_sweep_cursor = (end < self.ordered_coordinates.len()).then_some(end);
        batch
    }

    fn queue_update(&mut self, coordinate: ChunkCoord, visible: bool) {
        if !self.entities.contains_key(&coordinate) || !self.scheduled_updates.insert(coordinate) {
            return;
        }
        if visible {
            self.visible_updates.push_back(coordinate);
        } else {
            self.hidden_updates.push_back(coordinate);
        }
    }

    fn finish_update(&mut self, coordinate: ChunkCoord) {
        self.scheduled_updates.remove(&coordinate);
    }
}

#[derive(Default)]
struct ChunkMeshBuilder {
    positions: Vec<[f32; 3]>,
    normals: Vec<[f32; 3]>,
    colors: Vec<[f32; 4]>,
    indices: Vec<u32>,
    triangle_to_cell: Vec<Axial>,
    vertex_to_cell: Vec<Axial>,
    vertex_shades: Vec<f32>,
}

struct BuiltChunk {
    mesh: Mesh,
    cells: Vec<Axial>,
    triangle_to_cell: Vec<Axial>,
    vertex_to_cell: Vec<Axial>,
    vertex_shades: Vec<f32>,
    geometry_fingerprint: u64,
}

impl ChunkMeshBuilder {
    fn vertex(
        &mut self,
        cell: Axial,
        position: Vec3,
        normal: Vec3,
        color: [f32; 4],
        color_shade: f32,
    ) -> u32 {
        let index = self.positions.len() as u32;
        self.positions.push(position.to_array());
        self.normals.push(normal.to_array());
        self.colors.push(color);
        self.vertex_to_cell.push(cell);
        self.vertex_shades.push(color_shade);
        index
    }

    fn triangle(&mut self, cell: Axial, a: u32, b: u32, c: u32) {
        self.indices.extend_from_slice(&[a, b, c]);
        self.triangle_to_cell.push(cell);
    }

    fn finish(self, cells: Vec<Axial>) -> BuiltChunk {
        let mesh = Mesh::new(
            PrimitiveTopology::TriangleList,
            RenderAssetUsages::MAIN_WORLD | RenderAssetUsages::RENDER_WORLD,
        )
        .with_inserted_attribute(Mesh::ATTRIBUTE_POSITION, self.positions)
        .with_inserted_attribute(Mesh::ATTRIBUTE_NORMAL, self.normals)
        .with_inserted_attribute(Mesh::ATTRIBUTE_COLOR, self.colors)
        .with_inserted_indices(Indices::U32(self.indices));
        BuiltChunk {
            mesh,
            cells,
            triangle_to_cell: self.triangle_to_cell,
            vertex_to_cell: self.vertex_to_cell,
            vertex_shades: self.vertex_shades,
            geometry_fingerprint: 0,
        }
    }
}

pub fn spawn_terrain(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut view: ResMut<MatchView>,
    mode: Res<MapViewMode>,
) {
    let material = materials.add(StandardMaterial {
        base_color: Color::WHITE,
        perceptual_roughness: 0.96,
        metallic: 0.0,
        ..default()
    });
    commands.insert_resource(TerrainMaterial(material.clone()));
    let mut registry = TerrainChunkRegistry::new(
        view.chunk_index_revision,
        *mode,
        view.cells_by_chunk.keys().copied(),
    );
    for coordinate in registry.take_pending_spawns(INITIAL_CHUNK_SPAWN_BUDGET) {
        let entity = spawn_chunk(
            &mut commands,
            &mut meshes,
            &material,
            &view,
            coordinate,
            *mode,
        );
        registry.register(coordinate, entity);
    }
    // Both the initial meshes and pending spawns consume the current view, so
    // no initial recolor work is necessary.
    view.dirty_chunks.clear();
    commands.insert_resource(registry);
}

pub fn sync_terrain_chunks(
    mut commands: Commands,
    mut view: ResMut<MatchView>,
    mut chunks: Query<(Entity, &mut TerrainChunk, &Mesh3d, Option<&ViewVisibility>)>,
    mut meshes: ResMut<Assets<Mesh>>,
    material: Res<TerrainMaterial>,
    mut registry: ResMut<TerrainChunkRegistry>,
    mode: Res<MapViewMode>,
) {
    if registry.topology_revision != view.chunk_index_revision {
        let removed = registry.reconcile(
            view.chunk_index_revision,
            view.cells_by_chunk.keys().copied(),
        );
        for entity in removed {
            commands.entity(entity).despawn();
        }
        // Reconciliation's bounded sweep covers all retained chunks, while
        // missing chunks will be built from current state when spawned.
        view.dirty_chunks.clear();
    }

    if registry.rendered_mode != *mode {
        registry.begin_mode_sweep();
        registry.rendered_mode = *mode;
    }

    // Pull a bounded amount of newly dirty work into persistent visibility
    // queues. A large server update remains spread over subsequent frames.
    let mut classification = Vec::with_capacity(UPDATE_CLASSIFICATION_BUDGET_PER_FRAME * 2);
    for _ in 0..UPDATE_CLASSIFICATION_BUDGET_PER_FRAME {
        let Some(coordinate) = view.dirty_chunks.pop_first() else {
            break;
        };
        classification.push(coordinate);
    }
    classification.extend(registry.take_mode_sweep(UPDATE_CLASSIFICATION_BUDGET_PER_FRAME));
    for coordinate in classification {
        let Some(entity) = registry.entities.get(&coordinate).copied() else {
            // A pending spawn always builds the latest state directly.
            continue;
        };
        match chunks.get_mut(entity) {
            Ok((_, _, _, visibility)) => {
                registry.queue_update(coordinate, visibility.is_none_or(|value| value.get()));
            }
            Err(_) => registry.mark_missing(coordinate),
        }
    }

    // Bound attempts, not only successful recolors: after a large camera move,
    // an entire queued set may need visibility reclassification.
    for _ in 0..VISIBLE_COLOR_UPDATES_PER_FRAME {
        let Some(coordinate) = registry.visible_updates.pop_front() else {
            break;
        };
        let Some(entity) = registry.entities.get(&coordinate).copied() else {
            registry.finish_update(coordinate);
            continue;
        };
        match refresh_chunk(entity, true, &mut chunks, &mut meshes, &view, *mode) {
            RefreshResult::Updated => registry.finish_update(coordinate),
            RefreshResult::Reclassify => registry.hidden_updates.push_back(coordinate),
            RefreshResult::Missing => registry.mark_missing(coordinate),
        }
    }

    for _ in 0..HIDDEN_COLOR_UPDATES_PER_FRAME {
        let Some(coordinate) = registry.hidden_updates.pop_front() else {
            break;
        };
        let Some(entity) = registry.entities.get(&coordinate).copied() else {
            registry.finish_update(coordinate);
            continue;
        };
        match refresh_chunk(entity, false, &mut chunks, &mut meshes, &view, *mode) {
            RefreshResult::Updated => registry.finish_update(coordinate),
            RefreshResult::Reclassify => registry.visible_updates.push_back(coordinate),
            RefreshResult::Missing => registry.mark_missing(coordinate),
        }
    }

    // Spawn last: command-buffer entities are not queryable until deferred
    // commands apply, and fresh meshes already reflect the latest map mode.
    for coordinate in registry.take_pending_spawns(CHUNK_SPAWN_BUDGET_PER_FRAME) {
        let entity = spawn_chunk(
            &mut commands,
            &mut meshes,
            &material.0,
            &view,
            coordinate,
            *mode,
        );
        registry.register(coordinate, entity);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RefreshResult {
    Updated,
    Reclassify,
    Missing,
}

fn refresh_chunk(
    entity: Entity,
    expected_visible: bool,
    chunks: &mut Query<(Entity, &mut TerrainChunk, &Mesh3d, Option<&ViewVisibility>)>,
    meshes: &mut Assets<Mesh>,
    view: &MatchView,
    mode: MapViewMode,
) -> RefreshResult {
    let Ok((_, mut chunk, mesh_handle, visibility)) = chunks.get_mut(entity) else {
        return RefreshResult::Missing;
    };
    if visibility.is_none_or(|value| value.get()) != expected_visible {
        return RefreshResult::Reclassify;
    }
    let updated_colors = meshes
        .get_mut(mesh_handle)
        .is_some_and(|mut mesh| recolor_chunk_mesh(&mut mesh, &chunk, view, mode));
    if updated_colors {
        return RefreshResult::Updated;
    }

    // Defensive fallback for topology changes or malformed mesh metadata.
    let replacement = build_chunk_mesh(view, chunk.coordinate, mode);
    if let Some(mut mesh) = meshes.get_mut(mesh_handle) {
        *mesh = replacement.mesh;
    }
    chunk.cells = replacement.cells;
    chunk.triangle_to_cell = replacement.triangle_to_cell;
    chunk.vertex_to_cell = replacement.vertex_to_cell;
    chunk.vertex_shades = replacement.vertex_shades;
    chunk.geometry_fingerprint = replacement.geometry_fingerprint;
    RefreshResult::Updated
}

fn spawn_chunk(
    commands: &mut Commands,
    meshes: &mut Assets<Mesh>,
    material: &Handle<StandardMaterial>,
    view: &MatchView,
    coordinate: ChunkCoord,
    mode: MapViewMode,
) -> Entity {
    let built = build_chunk_mesh(view, coordinate, mode);
    commands
        .spawn((
            Name::new(format!("Terrain chunk {},{}", coordinate.q, coordinate.r)),
            TerrainChunk {
                coordinate,
                cells: built.cells,
                triangle_to_cell: built.triangle_to_cell,
                vertex_to_cell: built.vertex_to_cell,
                vertex_shades: built.vertex_shades,
                geometry_fingerprint: built.geometry_fingerprint,
            },
            Mesh3d(meshes.add(built.mesh)),
            MeshMaterial3d(material.clone()),
        ))
        .id()
}

fn build_chunk_mesh(view: &MatchView, chunk: ChunkCoord, mode: MapViewMode) -> BuiltChunk {
    let mut builder = ChunkMeshBuilder::default();
    let cells = view.cells_in_chunk(chunk).to_vec();
    for coordinate in &cells {
        if let Some(cell) = view.cell(*coordinate) {
            push_cell(&mut builder, view, cell, mode);
        }
    }
    let mut built = builder.finish(cells);
    built.geometry_fingerprint = chunk_geometry_fingerprint(view, chunk);
    built
}

fn push_cell(builder: &mut ChunkMeshBuilder, view: &MatchView, cell: &CellView, mode: MapViewMode) {
    let top_y = cell_top(cell.elevation, cell.is_water());
    let center = world_center(cell.coordinate, cell.elevation, cell.is_water());
    let top_color = cell_color(cell, view.contested_cells.get(&cell.coordinate), mode);

    let center_index = builder.vertex(cell.coordinate, center, Vec3::Y, top_color, 1.0);
    let top_corners = std::array::from_fn::<_, 6, _>(|index| {
        builder.vertex(
            cell.coordinate,
            corner(center, index, top_y),
            Vec3::Y,
            top_color,
            1.0,
        )
    });
    for index in 0..6 {
        let next = (index + 1) % 6;
        // Clockwise winding in XZ points the normal toward +Y.
        builder.triangle(
            cell.coordinate,
            center_index,
            top_corners[next],
            top_corners[index],
        );
    }

    for (direction, neighbor_coord) in cell.coordinate.neighbors().into_iter().enumerate() {
        let neighbor_top = view.cell(neighbor_coord).map_or(COLUMN_FLOOR, |neighbor| {
            cell_top(neighbor.elevation, neighbor.is_water())
        });
        let bottom_y = if neighbor_top + 0.015 < top_y {
            neighbor_top.max(COLUMN_FLOOR)
        } else {
            (top_y - 0.065).max(COLUMN_FLOOR)
        };
        if bottom_y >= top_y {
            continue;
        }

        let edge = edge_index_for_direction(direction);
        let next = (edge + 1) % 6;
        let top_a = corner(center, edge, top_y);
        let top_b = corner(center, next, top_y);
        let bottom_a = corner(center, edge, bottom_y);
        let bottom_b = corner(center, next, bottom_y);
        let normal = Vec3::new(
            (top_a.x + top_b.x) * 0.5 - center.x,
            0.0,
            (top_a.z + top_b.z) * 0.5 - center.z,
        )
        .normalize();
        let depth = ((top_y - bottom_y) / 2.4).clamp(0.0, 1.0);
        let side_shade = 0.52 - depth * 0.12;
        let side_color = shade(top_color, side_shade);
        let a = builder.vertex(cell.coordinate, bottom_a, normal, side_color, side_shade);
        let b = builder.vertex(cell.coordinate, bottom_b, normal, side_color, side_shade);
        let c = builder.vertex(cell.coordinate, top_b, normal, shade(top_color, 0.68), 0.68);
        let d = builder.vertex(cell.coordinate, top_a, normal, shade(top_color, 0.68), 0.68);
        builder.triangle(cell.coordinate, a, c, b);
        builder.triangle(cell.coordinate, a, d, c);
    }
}

fn recolor_chunk_mesh(
    mesh: &mut Mesh,
    chunk: &TerrainChunk,
    view: &MatchView,
    mode: MapViewMode,
) -> bool {
    if chunk.geometry_fingerprint != chunk_geometry_fingerprint(view, chunk.coordinate)
        || chunk.vertex_to_cell.len() != chunk.vertex_shades.len()
    {
        return false;
    }
    let colors = chunk
        .vertex_to_cell
        .iter()
        .zip(&chunk.vertex_shades)
        .map(|(coordinate, factor)| {
            view.cell(*coordinate).map(|cell| {
                shade(
                    cell_color(cell, view.contested_cells.get(coordinate), mode),
                    *factor,
                )
            })
        })
        .collect::<Option<Vec<_>>>();
    let Some(colors) = colors else {
        return false;
    };
    mesh.insert_attribute(Mesh::ATTRIBUTE_COLOR, colors);
    true
}

fn cell_color(cell: &CellView, contest: Option<&ContestedCellView>, mode: MapViewMode) -> [f32; 4] {
    let base = if cell.is_water() {
        Color::srgb(0.055, 0.16, 0.21)
    } else {
        match cell.owner {
            Some(player) => player_color(player).unwrap_or_else(|| terrain_color(cell.terrain)),
            None => terrain_color(cell.terrain),
        }
    };
    let mut linear = contest.filter(|_| cell.is_land()).map_or_else(
        || LinearRgba::from(base),
        |contest| {
            let controller = player_color(contest.controller_player).unwrap_or(base);
            let attacker = player_color(contest.attacker_player).unwrap_or(base);
            mix_linear_colors(
                controller,
                attacker,
                normalized_share(contest.attacker_share),
            )
        },
    );
    let intensity = match mode {
        MapViewMode::Overview => 0.42,
        MapViewMode::Soldiers => normalized_soldier_strength(
            cell.infantry
                .saturating_add(contest.map_or(0, |contest| contest.attacker_strength)),
        ),
        MapViewMode::Civilians => normalized_cell_value(mode, cell).unwrap_or(0.0),
    };
    let terrain_light = match cell.terrain {
        TerrainKind::Plains => 1.0,
        TerrainKind::Hills => 0.92,
        TerrainKind::Mountain => 0.80,
        TerrainKind::Water => 0.82,
    };
    let ownership_readability = 0.58 + intensity * 0.68;
    linear.red = (linear.red * ownership_readability * terrain_light + intensity * 0.035).min(1.0);
    linear.green =
        (linear.green * ownership_readability * terrain_light + intensity * 0.045).min(1.0);
    linear.blue = (linear.blue * ownership_readability * terrain_light + intensity * 0.05).min(1.0);
    linear.to_f32_array()
}

const PLAYER_PALETTE: [(f32, f32, f32); 8] = [
    (0.06, 0.48, 0.58),
    (0.76, 0.24, 0.16),
    (0.50, 0.32, 0.78),
    (0.75, 0.62, 0.12),
    (0.20, 0.62, 0.30),
    (0.86, 0.34, 0.62),
    (0.25, 0.43, 0.86),
    (0.72, 0.43, 0.18),
];

fn player_color(player: u32) -> Option<Color> {
    if player == 0 || player > 500 {
        return None;
    }
    if let Some((r, g, b)) = PLAYER_PALETTE.get((player - 1) as usize) {
        return Some(Color::srgb(*r, *g, *b));
    }
    // Deterministic generated colors for IDs above the curated eight-color set.
    // Golden-ratio hue walk keeps neighbors visually distinct without a table.
    let index = player - 1;
    let hue = ((index as f32) * 0.618_034).fract();
    let saturation = 0.55 + 0.25 * (((index * 3) % 5) as f32 / 4.0);
    let lightness = 0.42 + 0.16 * (((index * 5) % 4) as f32 / 3.0);
    Some(hsl_color(hue, saturation, lightness))
}

fn hsl_color(hue: f32, saturation: f32, lightness: f32) -> Color {
    let c = (1.0 - (2.0 * lightness - 1.0).abs()) * saturation;
    let h = hue * 6.0;
    let x = c * (1.0 - (h % 2.0 - 1.0).abs());
    let (r1, g1, b1) = match h as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = lightness - c * 0.5;
    Color::srgb(r1 + m, g1 + m, b1 + m)
}

fn terrain_color(terrain: TerrainKind) -> Color {
    match terrain {
        TerrainKind::Plains => Color::srgb(0.39, 0.43, 0.30),
        TerrainKind::Hills => Color::srgb(0.42, 0.38, 0.25),
        TerrainKind::Mountain => Color::srgb(0.36, 0.35, 0.31),
        TerrainKind::Water => Color::srgb(0.055, 0.16, 0.21),
    }
}

fn normalized_share(share: f32) -> f32 {
    if share.is_finite() {
        share.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

fn mix_linear_colors(controller: Color, attacker: Color, attacker_share: f32) -> LinearRgba {
    let controller = LinearRgba::from(controller);
    let attacker = LinearRgba::from(attacker);
    let controller_share = 1.0 - attacker_share;
    LinearRgba::new(
        controller.red * controller_share + attacker.red * attacker_share,
        controller.green * controller_share + attacker.green * attacker_share,
        controller.blue * controller_share + attacker.blue * attacker_share,
        controller.alpha * controller_share + attacker.alpha * attacker_share,
    )
}

fn sort_chunks_near_origin(chunks: &mut [ChunkCoord]) {
    chunks.sort_by_key(|chunk| {
        let q = i64::from(chunk.q);
        let r = i64::from(chunk.r);
        q.pow(2) + r.pow(2) + (q + r).pow(2)
    });
}

fn chunk_geometry_fingerprint(view: &MatchView, chunk: ChunkCoord) -> u64 {
    let mut hasher = DefaultHasher::new();
    for coordinate in view.cells_in_chunk(chunk) {
        coordinate.hash(&mut hasher);
        if let Some(cell) = view.cell(*coordinate) {
            cell.elevation.hash(&mut hasher);
            cell.is_water().hash(&mut hasher);
            for neighbor in coordinate.neighbors() {
                match view.cell(neighbor) {
                    Some(neighbor) => {
                        1_u8.hash(&mut hasher);
                        neighbor.elevation.hash(&mut hasher);
                        neighbor.is_water().hash(&mut hasher);
                    }
                    None => 0_u8.hash(&mut hasher),
                }
            }
        }
    }
    hasher.finish()
}

fn shade(color: [f32; 4], factor: f32) -> [f32; 4] {
    [
        color[0] * factor,
        color[1] * factor,
        color[2] * factor,
        color[3],
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{PLAYER_ONE, PLAYER_TWO};

    const fn test_chunk(q: i32, r: i32) -> ChunkCoord {
        ChunkCoord { q, r }
    }

    fn test_cell(infantry: u64, civilians: u64, military_capacity: u64) -> CellView {
        CellView {
            coordinate: Axial::ZERO,
            terrain: TerrainKind::Plains,
            elevation: 1,
            owner: Some(PLAYER_ONE),
            civilians,
            infantry,
            military_capacity,
            blocked: false,
        }
    }

    #[test]
    fn every_supported_player_has_a_distinct_color() {
        let colors = (1..=8)
            .map(|player| player_color(player).expect("supported player color"))
            .collect::<Vec<_>>();
        for (index, color) in colors.iter().enumerate() {
            assert!(colors.iter().skip(index + 1).all(|other| other != color));
        }
        assert!(player_color(0).is_none());
        assert!(player_color(9).is_some());
        assert!(player_color(500).is_some());
        assert!(player_color(501).is_none());
        // Curated first-eight palette stays pinned.
        assert_eq!(player_color(1), Some(Color::srgb(0.06, 0.48, 0.58)));
        assert_eq!(player_color(2), Some(Color::srgb(0.76, 0.24, 0.16)));
    }

    #[test]
    fn player_colors_cover_one_through_five_hundred_deterministically() {
        let mut seen = std::collections::BTreeSet::new();
        for player in 1_u32..=500 {
            let color = player_color(player).expect("supported player color");
            let components = LinearRgba::from(color).to_f32_array();
            // Neutral/default black is forbidden; perfect uniqueness is not required.
            assert!(
                components[0] + components[1] + components[2] > 0.05,
                "player {player} color is too near neutral"
            );
            assert_eq!(player_color(player), Some(color));
            seen.insert(format!(
                "{:.4}/{:.4}/{:.4}",
                components[0], components[1], components[2]
            ));
        }
        // Deterministic non-collapse: far more distinct samples than the curated eight.
        assert!(seen.len() >= 64, "only {} distinct colors", seen.len());
        assert!(player_color(0).is_none());
        assert!(player_color(501).is_none());
    }

    #[test]
    fn soldier_shading_uses_absolute_strength_not_capacity_ratio() {
        let compact = test_cell(50, 0, 100);
        let spacious = test_cell(50, 0, 1_000);

        let compact = cell_color(&compact, None, MapViewMode::Soldiers);
        let spacious = cell_color(&spacious, None, MapViewMode::Soldiers);
        assert!(
            compact
                .iter()
                .zip(spacious)
                .all(|(left, right)| (*left - right).abs() < f32::EPSILON)
        );
    }

    #[test]
    fn civilian_shading_becomes_brighter_as_population_increases() {
        let sparse = cell_color(&test_cell(0, 10, 100), None, MapViewMode::Civilians);
        let dense = cell_color(&test_cell(0, 220, 100), None, MapViewMode::Civilians);

        assert!(dense[..3].iter().sum::<f32>() > sparse[..3].iter().sum::<f32>());
    }

    #[test]
    fn overview_brightness_does_not_follow_force_composition() {
        let empty = cell_color(&test_cell(0, 0, 100), None, MapViewMode::Overview);
        let crowded = cell_color(&test_cell(100, 500, 100), None, MapViewMode::Overview);

        assert!(
            empty
                .iter()
                .zip(crowded)
                .all(|(left, right)| (*left - right).abs() < f32::EPSILON)
        );
    }

    #[test]
    fn contested_color_tracks_attacker_share_between_player_colors() {
        let controller_cell = test_cell(50, 0, 100);
        let mut attacker_cell = controller_cell.clone();
        attacker_cell.owner = Some(PLAYER_TWO);
        let controller = cell_color(&controller_cell, None, MapViewMode::Overview);
        let attacker = cell_color(&attacker_cell, None, MapViewMode::Overview);

        let contest = |attacker_share| ContestedCellView {
            controller_player: PLAYER_ONE,
            attacker_player: PLAYER_TWO,
            attacker_strength: 0,
            attacker_share,
        };
        let zero = contest(0.0);
        let half = contest(0.5);
        let full = contest(1.0);
        let zero = cell_color(&controller_cell, Some(&zero), MapViewMode::Overview);
        let half = cell_color(&controller_cell, Some(&half), MapViewMode::Overview);
        let full = cell_color(&controller_cell, Some(&full), MapViewMode::Overview);

        assert_colors_close(zero, controller);
        assert_colors_close(full, attacker);
        for index in 0..4 {
            assert!((half[index] - (controller[index] + attacker[index]) * 0.5).abs() < 1e-6);
        }
    }

    #[test]
    fn contested_color_clamps_invalid_network_shares() {
        let cell = test_cell(50, 0, 100);
        let controller = cell_color(&cell, None, MapViewMode::Overview);
        let invalid = ContestedCellView {
            controller_player: PLAYER_ONE,
            attacker_player: PLAYER_TWO,
            attacker_strength: 0,
            attacker_share: f32::NAN,
        };
        let overfull = ContestedCellView {
            attacker_share: 2.0,
            ..invalid
        };

        assert_colors_close(
            cell_color(&cell, Some(&invalid), MapViewMode::Overview),
            controller,
        );
        let mut attacker_cell = cell.clone();
        attacker_cell.owner = Some(PLAYER_TWO);
        assert_colors_close(
            cell_color(&cell, Some(&overfull), MapViewMode::Overview),
            cell_color(&attacker_cell, None, MapViewMode::Overview),
        );
    }

    #[test]
    fn contested_soldier_shading_includes_attacker_pressure() {
        let cell = test_cell(25, 0, 100);
        let uncontested = cell_color(&cell, None, MapViewMode::Soldiers);
        let contest = ContestedCellView {
            controller_player: PLAYER_ONE,
            attacker_player: PLAYER_TWO,
            attacker_strength: 75,
            attacker_share: 0.75,
        };
        let contested = cell_color(&cell, Some(&contest), MapViewMode::Soldiers);

        assert!(contested[..3].iter().sum::<f32>() > uncontested[..3].iter().sum::<f32>());
    }

    #[test]
    fn built_chunk_exposes_the_indexed_cells_once_and_in_order() {
        let view = MatchView::offline_fixture();
        let coordinate = *view.cells_by_chunk.keys().next().expect("fixture chunk");
        let built = build_chunk_mesh(&view, coordinate, MapViewMode::Overview);

        assert_eq!(built.cells, view.cells_in_chunk(coordinate));
        assert!(built.cells.windows(2).all(|pair| pair[0] < pair[1]));
        assert_eq!(
            built.cells.iter().copied().collect::<BTreeSet<_>>().len(),
            built.cells.len()
        );
    }

    #[test]
    fn geometry_fingerprint_detects_height_changes() {
        let mut view = MatchView::offline_fixture();
        let coordinate = *view.cells_by_chunk.keys().next().expect("fixture chunk");
        let before = chunk_geometry_fingerprint(&view, coordinate);
        let cell = *view.cells_in_chunk(coordinate).first().expect("chunk cell");
        view.cell_mut(cell).expect("fixture cell").elevation += 1;

        assert_ne!(before, chunk_geometry_fingerprint(&view, coordinate));
    }

    #[test]
    fn force_recolor_preserves_geometry_and_triangle_picking_metadata() {
        let mut view = MatchView::offline_fixture();
        let cell_coordinate = view
            .cells
            .values()
            .find(|cell| cell.owner.is_some() && cell.is_land())
            .expect("fixture owned land")
            .coordinate;
        let chunk_coordinate = crate::geometry::chunk_of(cell_coordinate);
        let built = build_chunk_mesh(&view, chunk_coordinate, MapViewMode::Soldiers);
        let BuiltChunk {
            mut mesh,
            cells,
            triangle_to_cell,
            vertex_to_cell,
            vertex_shades,
            geometry_fingerprint,
        } = built;
        let chunk = TerrainChunk {
            coordinate: chunk_coordinate,
            cells,
            triangle_to_cell,
            vertex_to_cell,
            vertex_shades,
            geometry_fingerprint,
        };
        let positions_before = mesh
            .attribute(Mesh::ATTRIBUTE_POSITION)
            .expect("chunk positions")
            .clone();
        let indices_before = mesh.indices().expect("chunk indices").clone();
        let colors_before = mesh
            .attribute(Mesh::ATTRIBUTE_COLOR)
            .expect("chunk colors")
            .clone();
        let picking_before = chunk.triangle_to_cell.clone();

        let cell = view.cell_mut(cell_coordinate).expect("fixture cell");
        cell.infantry = u64::from(cell.infantry == 0) * 100;

        assert!(recolor_chunk_mesh(
            &mut mesh,
            &chunk,
            &view,
            MapViewMode::Soldiers
        ));
        assert_eq!(
            mesh.attribute(Mesh::ATTRIBUTE_POSITION),
            Some(&positions_before)
        );
        assert_eq!(mesh.indices(), Some(&indices_before));
        assert_ne!(mesh.attribute(Mesh::ATTRIBUTE_COLOR), Some(&colors_before));
        assert_eq!(chunk.triangle_to_cell, picking_before);
    }

    #[test]
    fn contest_recolor_changes_only_vertex_colors() {
        let mut view = MatchView::offline_fixture();
        let cell_coordinate = view
            .cells
            .values()
            .find(|cell| cell.owner == Some(PLAYER_ONE) && cell.is_land())
            .expect("fixture player one land")
            .coordinate;
        let chunk_coordinate = crate::geometry::chunk_of(cell_coordinate);
        let built = build_chunk_mesh(&view, chunk_coordinate, MapViewMode::Overview);
        let BuiltChunk {
            mut mesh,
            cells,
            triangle_to_cell,
            vertex_to_cell,
            vertex_shades,
            geometry_fingerprint,
        } = built;
        let chunk = TerrainChunk {
            coordinate: chunk_coordinate,
            cells,
            triangle_to_cell,
            vertex_to_cell,
            vertex_shades,
            geometry_fingerprint,
        };
        let positions_before = mesh
            .attribute(Mesh::ATTRIBUTE_POSITION)
            .expect("chunk positions")
            .clone();
        let indices_before = mesh.indices().expect("chunk indices").clone();
        let colors_before = mesh
            .attribute(Mesh::ATTRIBUTE_COLOR)
            .expect("chunk colors")
            .clone();

        view.contested_cells.insert(
            cell_coordinate,
            ContestedCellView {
                controller_player: PLAYER_ONE,
                attacker_player: PLAYER_TWO,
                attacker_strength: 50,
                attacker_share: 0.5,
            },
        );

        assert!(recolor_chunk_mesh(
            &mut mesh,
            &chunk,
            &view,
            MapViewMode::Overview
        ));
        assert_eq!(
            mesh.attribute(Mesh::ATTRIBUTE_POSITION),
            Some(&positions_before)
        );
        assert_eq!(mesh.indices(), Some(&indices_before));
        assert_ne!(mesh.attribute(Mesh::ATTRIBUTE_COLOR), Some(&colors_before));
    }

    fn assert_colors_close(left: [f32; 4], right: [f32; 4]) {
        assert!(
            left.iter()
                .zip(right)
                .all(|(left, right)| (*left - right).abs() < 1e-6),
            "left={left:?}, right={right:?}"
        );
    }

    #[test]
    fn registry_reconciles_only_missing_and_removed_chunks() {
        let a = test_chunk(0, 0);
        let b = test_chunk(1, 0);
        let c = test_chunk(2, 0);
        let d = test_chunk(3, 0);
        let mut registry = TerrainChunkRegistry::new(1, MapViewMode::Overview, [a, b, c]);
        assert_eq!(registry.take_pending_spawns(usize::MAX).len(), 3);

        let entity_a = Entity::from_raw_u32(1).expect("test entity");
        let entity_b = Entity::from_raw_u32(2).expect("test entity");
        let entity_c = Entity::from_raw_u32(3).expect("test entity");
        registry.register(a, entity_a);
        registry.register(b, entity_b);
        registry.register(c, entity_c);

        let removed = registry.reconcile(2, [b, c, d]);

        assert_eq!(removed, vec![entity_a]);
        assert_eq!(registry.entities.get(&b), Some(&entity_b));
        assert_eq!(registry.entities.get(&c), Some(&entity_c));
        assert!(!registry.entities.contains_key(&a));
        assert_eq!(registry.take_pending_spawns(8), vec![d]);
        assert_eq!(registry.topology_revision, 2);
    }

    #[test]
    fn registry_work_queues_respect_requested_budgets() {
        let coordinates = (0..20).map(|q| test_chunk(q, 0)).collect::<Vec<_>>();
        let mut registry = TerrainChunkRegistry::new(4, MapViewMode::Overview, coordinates.clone());

        let first_spawns = registry.take_pending_spawns(5);
        let second_spawns = registry.take_pending_spawns(5);
        assert_eq!(first_spawns.len(), 5);
        assert_eq!(second_spawns.len(), 5);
        assert_eq!(registry.pending_spawns.len(), 10);

        registry.begin_mode_sweep();
        let first_sweep = registry.take_mode_sweep(7);
        let second_sweep = registry.take_mode_sweep(7);
        let third_sweep = registry.take_mode_sweep(7);
        assert_eq!(first_sweep.len(), 7);
        assert_eq!(second_sweep.len(), 7);
        assert_eq!(third_sweep.len(), 6);
        assert!(registry.take_mode_sweep(7).is_empty());
        assert_eq!(
            first_sweep
                .into_iter()
                .chain(second_sweep)
                .chain(third_sweep)
                .collect::<BTreeSet<_>>(),
            coordinates.into_iter().collect()
        );
    }
}
