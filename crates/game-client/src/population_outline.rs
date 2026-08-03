use std::{
    any::TypeId,
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
};

use bevy::{
    asset::RenderAssetUsages,
    camera::{
        CameraUpdateSystems,
        visibility::{NoFrustumCulling, VisibilitySystems, VisibleEntities},
    },
    light::{NotShadowCaster, NotShadowReceiver},
    mesh::Indices,
    prelude::*,
    render::render_resource::PrimitiveTopology,
};
use hex_core::Axial;

use crate::{
    camera::{CameraRig, GameCamera},
    geometry::{corner, edge_index_for_direction, world_center},
    map_view::{MapViewMode, presentation_input_signature, projected_hex_spacing},
    model::{CellView, MatchView},
    terrain::TerrainChunk,
};

const MIN_OUTLINE_SPACING_PX: f32 = 8.0;
const OUTLINE_SCREEN_MARGIN_PX: f32 = 12.0;
const MAX_VISIBLE_CELL_SCAN: usize = 16_384;
const MAX_BOUNDARY_EDGES: usize = 8_192;
const OUTLINE_SURFACE_OFFSET: f32 = 0.068;
const OUTLINE_INSET: f32 = 0.044;
const OUTLINE_OUTSET: f32 = 0.008;
const OUTLINE_WIDTH: f32 = OUTLINE_INSET + OUTLINE_OUTSET;

const OUTLINE_COLOR: Color = Color::srgb(1.0, 0.84, 0.43);

#[derive(Component)]
struct PopulationOutlineBatch;

#[derive(Resource, Debug)]
struct PopulationOutlineBatchAssets {
    mesh: Handle<Mesh>,
    material: Handle<StandardMaterial>,
    input_signature: Option<u64>,
    signature: u64,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
struct PopulationBoundaryEdge {
    coordinate: Axial,
    elevation: i16,
    direction: u8,
}

#[derive(Default)]
struct OutlineMeshBuilder {
    positions: Vec<[f32; 3]>,
    normals: Vec<[f32; 3]>,
    indices: Vec<u32>,
}

impl OutlineMeshBuilder {
    fn boundary_edge(&mut self, edge: PopulationBoundaryEdge) {
        let center =
            world_center(edge.coordinate, edge.elevation, false) + Vec3::Y * OUTLINE_SURFACE_OFFSET;
        let edge_index = edge_index_for_direction(usize::from(edge.direction));
        let mut start = corner(center, edge_index, center.y);
        let mut end = corner(center, (edge_index + 1) % 6, center.y);
        let tangent = (end - start).normalize_or_zero();
        let midpoint = (start + end) * 0.5;
        let outward =
            Vec3::new(midpoint.x - center.x, 0.0, midpoint.z - center.z).normalize_or_zero();

        // Slight endpoint overlap closes the small wedges where two exposed
        // edges meet around an outer corner.
        start -= tangent * (OUTLINE_WIDTH * 0.5);
        end += tangent * (OUTLINE_WIDTH * 0.5);
        // Keep most of the strip on the populated hex top. Only a narrow lip
        // reaches into the visual gap, so a taller neighbor cannot occlude the
        // complete population boundary.
        let inner_start = start - outward * OUTLINE_INSET;
        let inner_end = end - outward * OUTLINE_INSET;
        let outer_start = start + outward * OUTLINE_OUTSET;
        let outer_end = end + outward * OUTLINE_OUTSET;
        let base = self.positions.len() as u32;
        self.positions.extend([
            inner_start.to_array(),
            inner_end.to_array(),
            outer_end.to_array(),
            outer_start.to_array(),
        ]);
        self.normals.extend([[0.0, 1.0, 0.0]; 4]);
        self.indices
            .extend_from_slice(&[base, base + 2, base + 1, base, base + 3, base + 2]);
    }

    fn finish(self) -> Mesh {
        Mesh::new(
            PrimitiveTopology::TriangleList,
            RenderAssetUsages::MAIN_WORLD | RenderAssetUsages::RENDER_WORLD,
        )
        .with_inserted_attribute(Mesh::ATTRIBUTE_POSITION, self.positions)
        .with_inserted_attribute(Mesh::ATTRIBUTE_NORMAL, self.normals)
        .with_inserted_indices(Indices::U32(self.indices))
    }
}

pub(crate) struct PopulationOutlinePlugin;

impl Plugin for PopulationOutlinePlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Startup, spawn_population_outline_batch)
            .add_systems(
                PostUpdate,
                update_population_outline_batch
                    .after(CameraUpdateSystems)
                    .after(VisibilitySystems::CheckVisibility),
            );
    }
}

fn spawn_population_outline_batch(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    let material = materials.add(StandardMaterial {
        base_color: OUTLINE_COLOR.with_alpha(0.0),
        alpha_mode: AlphaMode::Mask(0.1),
        double_sided: true,
        cull_mode: None,
        unlit: true,
        ..default()
    });
    // Keep a valid hidden mesh. Bevy's mesh allocator cannot upload a
    // zero-vertex replacement after this asset has entered the render world.
    let placeholder = [PopulationBoundaryEdge {
        coordinate: Axial::ZERO,
        elevation: 0,
        direction: 0,
    }];
    let mesh = meshes.add(build_outline_mesh(&placeholder));
    commands.spawn((
        Name::new("Population outline batch"),
        PopulationOutlineBatch,
        Mesh3d(mesh.clone()),
        MeshMaterial3d(material.clone()),
        NoFrustumCulling,
        NotShadowCaster,
        NotShadowReceiver,
        Pickable::IGNORE,
    ));
    commands.insert_resource(PopulationOutlineBatchAssets {
        mesh,
        material,
        input_signature: None,
        signature: 0,
    });
}

#[allow(clippy::too_many_arguments)]
fn update_population_outline_batch(
    camera: Single<(&Camera, &GlobalTransform, &CameraRig, &VisibleEntities), With<GameCamera>>,
    window: Single<&Window>,
    view: Res<MatchView>,
    mode: Res<MapViewMode>,
    chunks: Query<&TerrainChunk>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut batch: ResMut<PopulationOutlineBatchAssets>,
) {
    if *mode != MapViewMode::Civilians {
        batch.input_signature = None;
        hide_batch_if_needed(&mut materials, &mut batch, 1);
        return;
    }

    let (camera, camera_transform, camera_rig, visible) = *camera;
    let mut visible_chunks = visible
        .iter(TypeId::of::<Mesh3d>())
        .filter(|entity| chunks.contains(**entity))
        .copied()
        .collect::<Vec<_>>();
    visible_chunks.sort_unstable();
    let input_signature = presentation_input_signature(
        0x52,
        *mode,
        camera,
        camera_transform,
        camera_rig,
        &window,
        &view,
        &visible_chunks,
    );
    if batch.input_signature == Some(input_signature) {
        return;
    }
    batch.input_signature = Some(input_signature);

    let spacing = projected_hex_spacing(camera, camera_transform, camera_rig.focus);
    if spacing < MIN_OUTLINE_SPACING_PX {
        hide_batch_if_needed(&mut materials, &mut batch, 2);
        return;
    }

    let viewport = Vec2::new(window.width(), window.height());
    // Include one projected cell radius beyond the viewport so a cluster edge
    // remains complete while its cell center is just off-screen.
    let screen_margin = OUTLINE_SCREEN_MARGIN_PX.max(spacing * 0.58);
    let mut visible_coordinates = Vec::new();
    let mut scanned_cells = 0_usize;
    for entity in visible_chunks {
        let Ok(chunk) = chunks.get(entity) else {
            continue;
        };
        scanned_cells = scanned_cells.saturating_add(chunk.cells.len());
        if scanned_cells > MAX_VISIBLE_CELL_SCAN {
            hide_batch_if_needed(&mut materials, &mut batch, 3);
            return;
        }
        for coordinate in &chunk.cells {
            let Some(cell) = view.cell(*coordinate) else {
                continue;
            };
            let center = world_center(cell.coordinate, cell.elevation, cell.is_water());
            let Ok(screen) = camera.world_to_viewport(camera_transform, center) else {
                continue;
            };
            if inside_viewport(screen, viewport, screen_margin) {
                visible_coordinates.push(*coordinate);
            }
        }
    }

    let Some(edges) =
        collect_population_boundary_edges(&view, visible_coordinates, MAX_BOUNDARY_EDGES)
    else {
        hide_batch_if_needed(&mut materials, &mut batch, 4);
        return;
    };
    if edges.is_empty() {
        hide_batch_if_needed(&mut materials, &mut batch, 5);
        return;
    }

    let signature = boundary_signature(&edges);
    if signature == batch.signature {
        return;
    }
    if let Some(mut material) = materials.get_mut(&batch.material) {
        material.base_color = OUTLINE_COLOR.with_alpha(1.0);
    }
    if let Some(mut mesh) = meshes.get_mut(&batch.mesh) {
        *mesh = build_outline_mesh(&edges);
    }
    batch.signature = signature;
}

fn collect_population_boundary_edges(
    view: &MatchView,
    coordinates: impl IntoIterator<Item = Axial>,
    edge_cap: usize,
) -> Option<Vec<PopulationBoundaryEdge>> {
    let mut edges = Vec::new();
    for coordinate in coordinates {
        let Some(cell) = view.cell(coordinate).filter(|cell| has_population(cell)) else {
            continue;
        };
        for (direction, neighbor) in coordinate.neighbors().into_iter().enumerate() {
            if view.cell(neighbor).is_some_and(has_population) {
                continue;
            }
            if edges.len() == edge_cap {
                return None;
            }
            edges.push(PopulationBoundaryEdge {
                coordinate,
                elevation: cell.elevation,
                direction: u8::try_from(direction).expect("six hex directions fit in u8"),
            });
        }
    }
    edges.sort_unstable();
    edges.dedup();
    Some(edges)
}

fn has_population(cell: &CellView) -> bool {
    cell.is_land() && cell.civilians > 0
}

fn build_outline_mesh(edges: &[PopulationBoundaryEdge]) -> Mesh {
    let mut builder = OutlineMeshBuilder::default();
    for edge in edges {
        builder.boundary_edge(*edge);
    }
    builder.finish()
}

fn boundary_signature(edges: &[PopulationBoundaryEdge]) -> u64 {
    let mut hasher = DefaultHasher::new();
    0xB7_u8.hash(&mut hasher);
    edges.hash(&mut hasher);
    hasher.finish()
}

fn hide_batch_if_needed(
    materials: &mut Assets<StandardMaterial>,
    batch: &mut PopulationOutlineBatchAssets,
    reason: u8,
) {
    let mut hasher = DefaultHasher::new();
    0xC7_u8.hash(&mut hasher);
    reason.hash(&mut hasher);
    let signature = hasher.finish();
    if signature == batch.signature {
        return;
    }
    if let Some(mut material) = materials.get_mut(&batch.material) {
        material.base_color = OUTLINE_COLOR.with_alpha(0.0);
    }
    batch.signature = signature;
}

fn inside_viewport(point: Vec2, viewport: Vec2, margin: f32) -> bool {
    point.x >= -margin
        && point.y >= -margin
        && point.x <= viewport.x + margin
        && point.y <= viewport.y + margin
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::mesh::VertexAttributeValues;
    use hex_core::TerrainKind;

    fn populated_cell(coordinate: Axial, owner: Option<u32>) -> CellView {
        CellView {
            coordinate,
            terrain: TerrainKind::Plains,
            elevation: 1,
            owner,
            civilians: 20,
            infantry: 0,
            military_capacity: 100,
            blocked: false,
        }
    }

    fn view_with_cells(cells: impl IntoIterator<Item = CellView>) -> MatchView {
        let mut view = MatchView::connecting(1);
        for cell in cells {
            view.cells.insert(cell.coordinate, cell);
        }
        view.rebuild_chunk_index();
        view
    }

    #[test]
    fn isolated_population_cell_has_six_exposed_edges() {
        let coordinate = Axial::ZERO;
        let view = view_with_cells([populated_cell(coordinate, Some(1))]);
        let edges = collect_population_boundary_edges(&view, [coordinate], usize::MAX).unwrap();

        assert_eq!(edges.len(), 6);
    }

    #[test]
    fn adjacent_population_pair_has_ten_exposed_edges() {
        let left = Axial::ZERO;
        let right = Axial::DIRECTIONS[0];
        let view = view_with_cells([
            populated_cell(left, Some(1)),
            populated_cell(right, Some(2)),
        ]);
        let edges = collect_population_boundary_edges(&view, [right, left], usize::MAX).unwrap();

        assert_eq!(edges.len(), 10);
        assert_eq!(
            edges,
            collect_population_boundary_edges(&view, [left, right], usize::MAX).unwrap()
        );
    }

    #[test]
    fn ownership_does_not_split_a_population_cluster() {
        let left = Axial::ZERO;
        let right = Axial::DIRECTIONS[0];
        let view = view_with_cells([
            populated_cell(left, Some(1)),
            populated_cell(right, Some(2)),
        ]);
        let edges = collect_population_boundary_edges(&view, [left, right], usize::MAX).unwrap();

        assert!(!edges.iter().any(|edge| {
            (edge.coordinate == left && usize::from(edge.direction) == 0)
                || (edge.coordinate == right && usize::from(edge.direction) == 3)
        }));
    }

    #[test]
    fn zero_population_and_water_never_join_a_cluster() {
        let empty = Axial::ZERO;
        let water = Axial::DIRECTIONS[0];
        let mut empty_cell = populated_cell(empty, None);
        empty_cell.civilians = 0;
        let mut water_cell = populated_cell(water, None);
        water_cell.terrain = TerrainKind::Water;
        let view = view_with_cells([empty_cell, water_cell]);

        assert!(
            collect_population_boundary_edges(&view, [empty, water], usize::MAX)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn adjacency_across_a_render_chunk_seam_has_no_internal_edges() {
        let left = Axial::new(7, 0);
        let right = Axial::new(8, 0);
        let view = view_with_cells([
            populated_cell(left, Some(1)),
            populated_cell(right, Some(1)),
        ]);

        assert_ne!(
            crate::geometry::chunk_of(left),
            crate::geometry::chunk_of(right)
        );
        assert_eq!(
            collect_population_boundary_edges(&view, [left, right], usize::MAX)
                .unwrap()
                .len(),
            10
        );
    }

    #[test]
    fn boundary_cap_hides_the_whole_result_instead_of_sampling() {
        let coordinate = Axial::ZERO;
        let view = view_with_cells([populated_cell(coordinate, None)]);

        assert!(collect_population_boundary_edges(&view, [coordinate], 5).is_none());
        assert_eq!(
            collect_population_boundary_edges(&view, [coordinate], 6)
                .unwrap()
                .len(),
            6
        );
    }

    #[test]
    fn one_quad_is_generated_for_each_boundary_edge() {
        let edges = [PopulationBoundaryEdge {
            coordinate: Axial::ZERO,
            elevation: 1,
            direction: 0,
        }];
        let mesh = build_outline_mesh(&edges);
        let positions = mesh
            .attribute(Mesh::ATTRIBUTE_POSITION)
            .and_then(VertexAttributeValues::as_float3)
            .expect("outline positions");

        assert_eq!(positions.len(), 4);
        assert_eq!(mesh.indices().expect("outline indices").len(), 6);
    }

    #[test]
    fn boundary_strip_is_mostly_inside_the_populated_hex() {
        let edge = PopulationBoundaryEdge {
            coordinate: Axial::ZERO,
            elevation: 1,
            direction: 0,
        };
        let mesh = build_outline_mesh(&[edge]);
        let positions = mesh
            .attribute(Mesh::ATTRIBUTE_POSITION)
            .and_then(VertexAttributeValues::as_float3)
            .expect("outline positions");
        let center =
            world_center(edge.coordinate, edge.elevation, false) + Vec3::Y * OUTLINE_SURFACE_OFFSET;
        let edge_index = edge_index_for_direction(usize::from(edge.direction));
        let boundary_midpoint = (corner(center, edge_index, center.y)
            + corner(center, (edge_index + 1) % 6, center.y))
            * 0.5;
        let outward = (boundary_midpoint - center).normalize_or_zero();
        let radial_offsets = positions
            .iter()
            .map(|position| (Vec3::from_array(*position) - boundary_midpoint).dot(outward))
            .collect::<Vec<_>>();
        let inside = radial_offsets.iter().copied().fold(f32::INFINITY, f32::min);
        let outside = radial_offsets
            .iter()
            .copied()
            .fold(f32::NEG_INFINITY, f32::max);

        assert!((inside + OUTLINE_INSET).abs() < 0.0001);
        assert!((outside - OUTLINE_OUTSET).abs() < 0.0001);
        assert!(-inside > outside * 4.0);
    }

    #[test]
    fn stable_signature_follows_sorted_boundary_edges() {
        let left = PopulationBoundaryEdge {
            coordinate: Axial::ZERO,
            elevation: 1,
            direction: 0,
        };
        let right = PopulationBoundaryEdge {
            coordinate: Axial::DIRECTIONS[0],
            elevation: 1,
            direction: 3,
        };

        assert_eq!(
            boundary_signature(&[left, right]),
            boundary_signature(&[left, right])
        );
        assert_ne!(boundary_signature(&[left]), boundary_signature(&[right]));
    }
}
