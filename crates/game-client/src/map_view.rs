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
    image::ImageSampler,
    mesh::Indices,
    prelude::*,
    render::render_resource::{Extent3d, PrimitiveTopology, TextureDimension, TextureFormat},
};
use hex_core::Axial;

use crate::{
    camera::{CameraRig, GameCamera},
    geometry::world_center,
    model::{CellView, MatchView},
    terrain::TerrainChunk,
};

const SOLDIER_REFERENCE: f32 = 100.0;
const CIVILIAN_REFERENCE: f32 = 240.0;

// Exact values are a close-zoom layer. The cap bounds candidate scanning,
// glyph geometry, and GPU uploads without allocating a Bevy UI node per cell.
const MAX_VALUE_LABELS: usize = 2_048;
const MIN_LABEL_SPACING_PX: f32 = 28.0;
const LABEL_PACKING_FACTOR: f32 = 0.68;
const LABEL_SCREEN_MARGIN_PX: f32 = 10.0;
const GLYPH_WORLD_WIDTH: f32 = 0.19;
const GLYPH_WORLD_HEIGHT: f32 = 0.30;
const GLYPH_WORLD_GAP: f32 = 0.018;
const LABEL_SURFACE_OFFSET: f32 = 0.055;

const ATLAS_TILE_WIDTH: u32 = 9;
const ATLAS_TILE_HEIGHT: u32 = 11;
const GLYPHS: [char; 13] = [
    '0', '1', '2', '3', '4', '5', '6', '7', '8', '9', '.', 'k', 'M',
];

const SOLDIER_TEXT: Color = Color::srgb(0.72, 0.96, 1.0);
const CIVILIAN_TEXT: Color = Color::srgb(1.0, 0.86, 0.56);

#[derive(Resource, Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub(crate) enum MapViewMode {
    Overview,
    #[default]
    Soldiers,
    Civilians,
}

impl MapViewMode {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Overview => "OVERVIEW",
            Self::Soldiers => "SOLDIERS",
            Self::Civilians => "CIVILIANS",
        }
    }

    pub(crate) const fn value(self, cell: &CellView) -> Option<u64> {
        match self {
            Self::Overview => None,
            Self::Soldiers => Some(cell.infantry),
            Self::Civilians => Some(cell.civilians),
        }
    }

    const fn legend(self) -> &'static str {
        match self {
            Self::Overview => "OWNER + TERRAIN",
            Self::Soldiers => "ABSOLUTE STRENGTH / CONTEST PRESSURE 0–100+",
            Self::Civilians => "ABSOLUTE POPULATION 0–240+",
        }
    }

    const fn next(self) -> Self {
        match self {
            Self::Overview => Self::Soldiers,
            Self::Soldiers => Self::Civilians,
            Self::Civilians => Self::Overview,
        }
    }

    const fn text_color(self) -> Color {
        match self {
            Self::Overview => Color::srgb(0.82, 0.88, 0.90),
            Self::Soldiers => SOLDIER_TEXT,
            Self::Civilians => CIVILIAN_TEXT,
        }
    }
}

/// Returns a stable, presentation-only intensity for the selected map view.
///
/// The references deliberately do not follow the live map maximum: a battle or
/// population update must not change the apparent value of unrelated cells.
/// Soldier shading compares absolute strength rather than occupancy, while the
/// civilian logarithm leaves contrast for future dense settlements.
pub(crate) fn normalized_cell_value(mode: MapViewMode, cell: &CellView) -> Option<f32> {
    match mode {
        MapViewMode::Overview => None,
        MapViewMode::Soldiers => Some(normalized_soldier_strength(cell.infantry)),
        MapViewMode::Civilians => {
            Some(((cell.civilians as f32).ln_1p() / CIVILIAN_REFERENCE.ln_1p()).clamp(0.0, 1.0))
        }
    }
}

pub(crate) fn normalized_soldier_strength(strength: u64) -> f32 {
    ((strength as f32) / SOLDIER_REFERENCE)
        .sqrt()
        .clamp(0.0, 1.0)
}

#[derive(Component)]
pub(crate) struct MapViewStatus;

#[derive(Component)]
struct MapValueBatch;

#[derive(Resource, Debug)]
struct MapValueBatchAssets {
    mesh: Handle<Mesh>,
    material: Handle<StandardMaterial>,
    input_signature: Option<u64>,
    signature: u64,
}

#[derive(Clone, Copy)]
struct PresentationInput<'a> {
    layer_tag: u8,
    mode: MapViewMode,
    world_from_view: Mat4,
    clip_from_view: Mat4,
    focus: Vec3,
    window_logical_size: Vec2,
    window_physical_size: UVec2,
    window_scale_factor: f32,
    logical_viewport: Option<Rect>,
    cell_state_revision: u64,
    contest_revision: u64,
    chunk_index_revision: u64,
    visible_chunks: &'a [Entity],
}

#[derive(Resource, Debug, Default)]
pub(crate) struct MapViewDiagnostics {
    pub active_labels: usize,
    pub visible_chunks: usize,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
struct LabelCandidate {
    coordinate: Axial,
    elevation: i16,
    value: u64,
}

#[derive(Default)]
struct LabelMeshBuilder {
    positions: Vec<[f32; 3]>,
    normals: Vec<[f32; 3]>,
    uvs: Vec<[f32; 2]>,
    indices: Vec<u32>,
}

impl LabelMeshBuilder {
    fn glyph(&mut self, center: Vec3, right: Vec3, up: Vec3, glyph: char) {
        let Some(glyph_index) = glyph_index(glyph) else {
            return;
        };
        let half_width = GLYPH_WORLD_WIDTH * 0.5;
        let half_height = GLYPH_WORLD_HEIGHT * 0.5;
        let bottom_left = center - right * half_width - up * half_height;
        let bottom_right = center + right * half_width - up * half_height;
        let top_right = center + right * half_width + up * half_height;
        let top_left = center - right * half_width + up * half_height;
        let base = self.positions.len() as u32;
        self.positions.extend([
            bottom_left.to_array(),
            bottom_right.to_array(),
            top_right.to_array(),
            top_left.to_array(),
        ]);
        self.normals.extend([[0.0, 1.0, 0.0]; 4]);

        let atlas_width = ATLAS_TILE_WIDTH * GLYPHS.len() as u32;
        let u0 = glyph_index as f32 * ATLAS_TILE_WIDTH as f32 / atlas_width as f32;
        let u1 = (glyph_index as f32 + 1.0) * ATLAS_TILE_WIDTH as f32 / atlas_width as f32;
        self.uvs
            .extend([[u0, 1.0], [u1, 1.0], [u1, 0.0], [u0, 0.0]]);
        // Winding faces +Y for a label printed just above the hex top.
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
        .with_inserted_attribute(Mesh::ATTRIBUTE_UV_0, self.uvs)
        .with_inserted_indices(Indices::U32(self.indices))
    }
}

pub(crate) struct MapViewPlugin;

impl Plugin for MapViewPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<MapViewMode>()
            .init_resource::<MapViewDiagnostics>()
            .add_systems(Startup, spawn_map_value_batch)
            .add_systems(Update, (switch_map_view, update_map_view_status).chain())
            .add_systems(
                PostUpdate,
                update_map_value_batch
                    .after(CameraUpdateSystems)
                    .after(VisibilitySystems::CheckVisibility),
            );
    }
}

pub(crate) fn map_view_status_bundle() -> impl Bundle {
    (
        Name::new("Map view status"),
        MapViewStatus,
        Text::new(
            "MAP VIEW  //  SOLDIERS\nABSOLUTE STRENGTH / CONTEST PRESSURE 0-100+ | 1/2/3 SELECT | V CYCLE",
        ),
        TextFont::from_font_size(10.5),
        TextColor(SOLDIER_TEXT),
        Pickable::IGNORE,
    )
}

fn spawn_map_value_batch(
    mut commands: Commands,
    mut images: ResMut<Assets<Image>>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    let atlas = images.add(build_glyph_atlas());
    let material = materials.add(StandardMaterial {
        base_color: SOLDIER_TEXT.with_alpha(0.0),
        base_color_texture: Some(atlas),
        alpha_mode: AlphaMode::Mask(0.1),
        double_sided: true,
        cull_mode: None,
        unlit: true,
        ..default()
    });
    // Bevy 0.19's mesh slab allocator cannot upload a modified zero-vertex
    // mesh. Keep a valid hidden placeholder and hide the batch through the
    // material alpha when exact values are outside their LOD.
    let mesh = meshes.add(build_label_mesh(
        &[LabelCandidate {
            coordinate: Axial::ZERO,
            elevation: 0,
            value: 0,
        }],
        Vec3::X,
        Vec3::Z,
    ));
    commands.spawn((
        Name::new("Map value texture batch"),
        MapValueBatch,
        Mesh3d(mesh.clone()),
        MeshMaterial3d(material.clone()),
        NoFrustumCulling,
        Pickable::IGNORE,
    ));
    commands.insert_resource(MapValueBatchAssets {
        mesh,
        material,
        input_signature: None,
        signature: 0,
    });
}

fn switch_map_view(keyboard: Res<ButtonInput<KeyCode>>, mut mode: ResMut<MapViewMode>) {
    let requested = if keyboard.just_pressed(KeyCode::Digit1) {
        Some(MapViewMode::Overview)
    } else if keyboard.just_pressed(KeyCode::Digit2) {
        Some(MapViewMode::Soldiers)
    } else if keyboard.just_pressed(KeyCode::Digit3) {
        Some(MapViewMode::Civilians)
    } else if keyboard.just_pressed(KeyCode::KeyV) {
        Some(mode.next())
    } else {
        None
    };

    if let Some(requested) = requested
        && *mode != requested
    {
        *mode = requested;
    }
}

fn update_map_view_status(
    mode: Res<MapViewMode>,
    status: Single<(&mut Text, &mut TextColor), With<MapViewStatus>>,
) {
    if !mode.is_changed() {
        return;
    }
    let (mut text, mut text_color) = status.into_inner();
    **text = format!(
        "MAP VIEW  //  {}\n{} | 1/2/3 SELECT | V CYCLE",
        mode.label(),
        mode.legend()
    );
    text_color.0 = mode.text_color();
}

#[allow(clippy::too_many_arguments)]
fn update_map_value_batch(
    camera: Single<(&Camera, &GlobalTransform, &CameraRig, &VisibleEntities), With<GameCamera>>,
    window: Single<&Window>,
    view: Res<MatchView>,
    mode: Res<MapViewMode>,
    chunks: Query<&TerrainChunk>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut batch: ResMut<MapValueBatchAssets>,
    mut diagnostics: ResMut<MapViewDiagnostics>,
) {
    let (camera, camera_transform, camera_rig, visible) = *camera;
    let mut visible_chunks = visible
        .iter(TypeId::of::<Mesh3d>())
        .filter(|entity| chunks.contains(**entity))
        .copied()
        .collect::<Vec<_>>();
    visible_chunks.sort_unstable();
    diagnostics.visible_chunks = visible_chunks.len();

    let input_signature = presentation_input_signature(
        0x51,
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

    let viewport = Vec2::new(window.width(), window.height());
    let spacing = projected_hex_spacing(camera, camera_transform, camera_rig.focus);
    let required_spacing = minimum_complete_label_spacing(viewport);
    if *mode == MapViewMode::Overview || spacing < required_spacing {
        diagnostics.active_labels = 0;
        clear_batch_if_needed(&mut materials, &mut batch, *mode, 1);
        return;
    }

    let mut candidates = Vec::new();
    for entity in visible_chunks {
        let Ok(chunk) = chunks.get(entity) else {
            continue;
        };
        for coordinate in &chunk.cells {
            let Some(cell) = view.cell(*coordinate).filter(|cell| cell.is_land()) else {
                continue;
            };
            let point = world_center(cell.coordinate, cell.elevation, cell.is_water())
                + Vec3::Y * LABEL_SURFACE_OFFSET;
            let Ok(projected) = camera.world_to_viewport(camera_transform, point) else {
                continue;
            };
            if !inside_viewport(projected, viewport, LABEL_SCREEN_MARGIN_PX) {
                continue;
            }
            let Some(mut value) = mode.value(cell) else {
                continue;
            };
            if *mode == MapViewMode::Soldiers
                && let Some(contest) = view.contested_cells.get(coordinate)
            {
                value = value.saturating_add(contest.attacker_strength);
            }
            candidates.push(LabelCandidate {
                coordinate: *coordinate,
                elevation: cell.elevation,
                value,
            });
        }
    }
    candidates.sort_unstable();
    candidates.dedup_by_key(|candidate| candidate.coordinate);

    // Never sample "important" cells. If the complete readable visible set is
    // larger than the batch budget, the exact-value layer waits for more zoom.
    if candidates.len() > MAX_VALUE_LABELS {
        diagnostics.active_labels = 0;
        clear_batch_if_needed(&mut materials, &mut batch, *mode, 2);
        return;
    }
    if candidates.is_empty() {
        diagnostics.active_labels = 0;
        clear_batch_if_needed(&mut materials, &mut batch, *mode, 3);
        return;
    }

    diagnostics.active_labels = candidates.len();
    let right = planar_axis(camera_transform.right().as_vec3());
    let up = planar_axis(camera_transform.up().as_vec3());
    let signature = label_signature(*mode, &candidates, right, up);
    if signature == batch.signature {
        return;
    }

    if let Some(mut material) = materials.get_mut(&batch.material) {
        material.base_color = mode.text_color().with_alpha(1.0);
    }
    if let Some(mut mesh) = meshes.get_mut(&batch.mesh) {
        *mesh = build_label_mesh(&candidates, right, up);
    }
    batch.signature = signature;
}

fn clear_batch_if_needed(
    materials: &mut Assets<StandardMaterial>,
    batch: &mut MapValueBatchAssets,
    mode: MapViewMode,
    reason: u8,
) {
    let mut hasher = DefaultHasher::new();
    0xC1_u8.hash(&mut hasher);
    mode.hash(&mut hasher);
    reason.hash(&mut hasher);
    let signature = hasher.finish();
    if signature == batch.signature {
        return;
    }
    if let Some(mut material) = materials.get_mut(&batch.material) {
        material.base_color = mode.text_color().with_alpha(0.0);
    }
    batch.signature = signature;
}

fn build_label_mesh(candidates: &[LabelCandidate], right: Vec3, up: Vec3) -> Mesh {
    let mut builder = LabelMeshBuilder::default();
    for candidate in candidates {
        let text = compact_value(candidate.value);
        let glyph_count = text.chars().count() as f32;
        let width =
            glyph_count * GLYPH_WORLD_WIDTH + (glyph_count - 1.0).max(0.0) * GLYPH_WORLD_GAP;
        let first_center = -width * 0.5 + GLYPH_WORLD_WIDTH * 0.5;
        let center = world_center(candidate.coordinate, candidate.elevation, false)
            + Vec3::Y * LABEL_SURFACE_OFFSET;
        for (index, glyph) in text.chars().enumerate() {
            let offset = first_center + index as f32 * (GLYPH_WORLD_WIDTH + GLYPH_WORLD_GAP);
            builder.glyph(center + right * offset, right, up, glyph);
        }
    }
    builder.finish()
}

fn label_signature(mode: MapViewMode, candidates: &[LabelCandidate], right: Vec3, up: Vec3) -> u64 {
    let mut hasher = DefaultHasher::new();
    0xA1_u8.hash(&mut hasher);
    mode.hash(&mut hasher);
    for value in [right.x, right.z, up.x, up.z] {
        // Camera yaw changes continuously; rounding avoids mesh uploads from
        // insignificant floating-point noise while keeping labels upright.
        ((value * 10_000.0).round() as i32).hash(&mut hasher);
    }
    candidates.hash(&mut hasher);
    hasher.finish()
}

pub(crate) fn projected_hex_spacing(
    camera: &Camera,
    camera_transform: &GlobalTransform,
    focus: Vec3,
) -> f32 {
    let Ok(projected_focus) = camera.world_to_viewport(camera_transform, focus) else {
        return 0.0;
    };
    let origin = world_center(Axial::ZERO, 0, false);
    let spacing = Axial::DIRECTIONS
        .into_iter()
        .filter_map(|coordinate| {
            let neighbor_offset = world_center(coordinate, 0, false) - origin;
            camera
                .world_to_viewport(camera_transform, focus + neighbor_offset)
                .ok()
                .map(|neighbor| projected_focus.distance(neighbor))
        })
        .fold(f32::INFINITY, f32::min);
    if spacing.is_finite() { spacing } else { 0.0 }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn presentation_input_signature(
    layer_tag: u8,
    mode: MapViewMode,
    camera: &Camera,
    camera_transform: &GlobalTransform,
    camera_rig: &CameraRig,
    window: &Window,
    view: &MatchView,
    visible_chunks: &[Entity],
) -> u64 {
    presentation_input_signature_from_parts(PresentationInput {
        layer_tag,
        mode,
        world_from_view: camera_transform.to_matrix(),
        clip_from_view: camera.clip_from_view(),
        focus: camera_rig.focus,
        window_logical_size: Vec2::new(window.width(), window.height()),
        window_physical_size: UVec2::new(window.physical_width(), window.physical_height()),
        window_scale_factor: window.scale_factor(),
        logical_viewport: camera.logical_viewport_rect(),
        cell_state_revision: view.cell_state_revision,
        contest_revision: view.contest_revision,
        chunk_index_revision: view.chunk_index_revision,
        visible_chunks,
    })
}

fn presentation_input_signature_from_parts(input: PresentationInput<'_>) -> u64 {
    let mut hasher = DefaultHasher::new();
    input.layer_tag.hash(&mut hasher);
    input.mode.hash(&mut hasher);
    for value in input
        .world_from_view
        .to_cols_array()
        .into_iter()
        .chain(input.clip_from_view.to_cols_array())
        .chain(input.focus.to_array())
        .chain(input.window_logical_size.to_array())
    {
        value.to_bits().hash(&mut hasher);
    }
    input.window_physical_size.to_array().hash(&mut hasher);
    input.window_scale_factor.to_bits().hash(&mut hasher);
    input
        .logical_viewport
        .map(|viewport| {
            [
                viewport.min.x.to_bits(),
                viewport.min.y.to_bits(),
                viewport.max.x.to_bits(),
                viewport.max.y.to_bits(),
            ]
        })
        .hash(&mut hasher);
    input.cell_state_revision.hash(&mut hasher);
    input.contest_revision.hash(&mut hasher);
    input.chunk_index_revision.hash(&mut hasher);
    input.visible_chunks.hash(&mut hasher);
    hasher.finish()
}

fn minimum_complete_label_spacing(viewport: Vec2) -> f32 {
    let area_limited =
        (viewport.x * viewport.y / (MAX_VALUE_LABELS as f32 * LABEL_PACKING_FACTOR)).sqrt();
    area_limited.max(MIN_LABEL_SPACING_PX)
}

fn inside_viewport(point: Vec2, viewport: Vec2, margin: f32) -> bool {
    point.x >= margin
        && point.y >= margin
        && point.x <= viewport.x - margin
        && point.y <= viewport.y - margin
}

fn planar_axis(axis: Vec3) -> Vec3 {
    Vec3::new(axis.x, 0.0, axis.z).normalize_or_zero()
}

fn compact_value(value: u64) -> String {
    match value {
        0..=999 => value.to_string(),
        1_000..=999_999 => compact_scaled(value, 1_000, 'k'),
        _ => compact_scaled(value, 1_000_000, 'M'),
    }
}

fn compact_scaled(value: u64, divisor: u64, suffix: char) -> String {
    let whole = value / divisor;
    let tenth = value % divisor * 10 / divisor;
    if whole < 10 && tenth > 0 {
        format!("{whole}.{tenth}{suffix}")
    } else {
        format!("{whole}{suffix}")
    }
}

fn glyph_index(glyph: char) -> Option<usize> {
    GLYPHS.iter().position(|candidate| *candidate == glyph)
}

fn build_glyph_atlas() -> Image {
    let glyph_count = u32::try_from(GLYPHS.len()).expect("glyph count fits in u32");
    let width = ATLAS_TILE_WIDTH * glyph_count;
    let height = ATLAS_TILE_HEIGHT;
    let mut data = vec![
        0_u8;
        usize::try_from(width).expect("atlas width fits in usize")
            * usize::try_from(height).expect("atlas height fits in usize")
            * 4
    ];

    for (glyph_index, glyph) in GLYPHS.into_iter().enumerate() {
        let glyph_index = i32::try_from(glyph_index).expect("glyph index fits in i32");
        let tile_width = i32::try_from(ATLAS_TILE_WIDTH).expect("tile width fits in i32");
        let rows = glyph_rows(glyph);
        for (row, bits) in rows.into_iter().enumerate() {
            let row = i32::try_from(row).expect("glyph row fits in i32");
            for column in 0..5 {
                if bits & (1 << (4 - column)) == 0 {
                    continue;
                }
                let x = glyph_index * tile_width + column + 2;
                let y = row + 2;
                for offset_y in -1..=1 {
                    for offset_x in -1..=1 {
                        set_atlas_pixel(
                            &mut data,
                            width,
                            x + offset_x,
                            y + offset_y,
                            [5, 9, 12, 238],
                            false,
                        );
                    }
                }
                set_atlas_pixel(&mut data, width, x, y, [255, 255, 255, 255], true);
            }
        }
    }

    let mut image = Image::new(
        Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        TextureDimension::D2,
        data,
        TextureFormat::Rgba8UnormSrgb,
        RenderAssetUsages::MAIN_WORLD | RenderAssetUsages::RENDER_WORLD,
    );
    image.sampler = ImageSampler::nearest();
    image
}

fn set_atlas_pixel(data: &mut [u8], width: u32, x: i32, y: i32, color: [u8; 4], overwrite: bool) {
    let (Ok(x), Ok(y)) = (u32::try_from(x), u32::try_from(y)) else {
        return;
    };
    if x >= width || y >= ATLAS_TILE_HEIGHT {
        return;
    }
    let index = (usize::try_from(y).expect("atlas y fits in usize")
        * usize::try_from(width).expect("atlas width fits in usize")
        + usize::try_from(x).expect("atlas x fits in usize"))
        * 4;
    if overwrite || data[index + 3] == 0 {
        data[index..index + 4].copy_from_slice(&color);
    }
}

const fn glyph_rows(glyph: char) -> [u8; 7] {
    match glyph {
        '0' => [
            0b01110, 0b10001, 0b10011, 0b10101, 0b11001, 0b10001, 0b01110,
        ],
        '1' => [
            0b00100, 0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110,
        ],
        '2' => [
            0b01110, 0b10001, 0b00001, 0b00010, 0b00100, 0b01000, 0b11111,
        ],
        '3' => [
            0b11110, 0b00001, 0b00001, 0b01110, 0b00001, 0b00001, 0b11110,
        ],
        '4' => [
            0b00010, 0b00110, 0b01010, 0b10010, 0b11111, 0b00010, 0b00010,
        ],
        '5' => [
            0b11111, 0b10000, 0b10000, 0b11110, 0b00001, 0b00001, 0b11110,
        ],
        '6' => [
            0b01110, 0b10000, 0b10000, 0b11110, 0b10001, 0b10001, 0b01110,
        ],
        '7' => [
            0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b01000, 0b01000,
        ],
        '8' => [
            0b01110, 0b10001, 0b10001, 0b01110, 0b10001, 0b10001, 0b01110,
        ],
        '9' => [
            0b01110, 0b10001, 0b10001, 0b01111, 0b00001, 0b00001, 0b01110,
        ],
        '.' => [0, 0, 0, 0, 0, 0, 0b00100],
        'k' => [
            0b10000, 0b10000, 0b10010, 0b10100, 0b11000, 0b10100, 0b10010,
        ],
        'M' => [
            0b10001, 0b11011, 0b10101, 0b10101, 0b10001, 0b10001, 0b10001,
        ],
        _ => [0; 7],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::mesh::VertexAttributeValues;
    use hex_core::TerrainKind;

    fn cell(infantry: u64, civilians: u64, military_capacity: u64) -> CellView {
        CellView {
            coordinate: Axial::ZERO,
            terrain: TerrainKind::Plains,
            elevation: 0,
            owner: Some(1),
            civilians,
            infantry,
            military_capacity,
            blocked: false,
        }
    }

    #[test]
    fn soldiers_use_absolute_strength_instead_of_occupancy() {
        let low_capacity = cell(50, 0, 50);
        let high_capacity = cell(50, 0, 100);
        assert_eq!(
            normalized_cell_value(MapViewMode::Soldiers, &low_capacity),
            normalized_cell_value(MapViewMode::Soldiers, &high_capacity)
        );
        assert!(
            normalized_cell_value(MapViewMode::Soldiers, &cell(100, 0, 100))
                > normalized_cell_value(MapViewMode::Soldiers, &low_capacity)
        );
    }

    #[test]
    fn stable_reference_curves_are_clamped_and_monotonic() {
        assert_eq!(
            normalized_cell_value(MapViewMode::Overview, &cell(100, 240, 100)),
            None
        );
        assert_eq!(
            normalized_cell_value(MapViewMode::Soldiers, &cell(0, 0, 100)),
            Some(0.0)
        );
        assert_eq!(
            normalized_cell_value(MapViewMode::Soldiers, &cell(10_000, 0, 100)),
            Some(1.0)
        );
        let sparse = normalized_cell_value(MapViewMode::Civilians, &cell(0, 24, 0)).unwrap();
        let dense = normalized_cell_value(MapViewMode::Civilians, &cell(0, 120, 0)).unwrap();
        assert!(sparse < dense);
        assert_eq!(
            normalized_cell_value(MapViewMode::Civilians, &cell(0, 24_000, 0)),
            Some(1.0)
        );
    }

    #[test]
    fn compact_values_have_stable_boundaries() {
        assert_eq!(compact_value(0), "0");
        assert_eq!(compact_value(999), "999");
        assert_eq!(compact_value(1_000), "1k");
        assert_eq!(compact_value(1_250), "1.2k");
        assert_eq!(compact_value(12_999), "12k");
        assert_eq!(compact_value(999_999), "999k");
        assert_eq!(compact_value(1_000_000), "1M");
        assert_eq!(compact_value(2_500_000), "2.5M");
    }

    #[test]
    fn glyph_atlas_contains_opaque_strokes_and_dark_outline() {
        let atlas = build_glyph_atlas();
        assert_eq!(
            atlas.texture_descriptor.size,
            Extent3d {
                width: ATLAS_TILE_WIDTH * GLYPHS.len() as u32,
                height: ATLAS_TILE_HEIGHT,
                depth_or_array_layers: 1,
            }
        );
        let data = atlas.data.expect("runtime atlas has CPU pixel data");
        assert!(
            data.chunks_exact(4)
                .any(|pixel| pixel == [255, 255, 255, 255])
        );
        assert!(
            data.chunks_exact(4)
                .any(|pixel| pixel[0] < 16 && pixel[3] > 200)
        );
    }

    #[test]
    fn one_batch_mesh_contains_one_quad_per_compact_glyph() {
        let candidates = [LabelCandidate {
            coordinate: Axial::ZERO,
            elevation: 0,
            value: 1_250,
        }];
        let mesh = build_label_mesh(&candidates, Vec3::X, Vec3::Z);
        let positions = mesh
            .attribute(Mesh::ATTRIBUTE_POSITION)
            .and_then(VertexAttributeValues::as_float3)
            .expect("label positions");
        assert_eq!(positions.len(), compact_value(1_250).chars().count() * 4);
        assert_eq!(mesh.indices().expect("label indices").len(), 4 * 6);
    }

    #[test]
    fn label_lod_threshold_grows_with_viewport_area() {
        let small = minimum_complete_label_spacing(Vec2::new(1_280.0, 720.0));
        let large = minimum_complete_label_spacing(Vec2::new(3_840.0, 2_160.0));
        assert!(small >= MIN_LABEL_SPACING_PX);
        assert!(large > small);
    }

    #[test]
    fn presentation_cache_tracks_every_pre_scan_input() {
        let chunks = [
            Entity::from_raw_u32(7).expect("valid test entity"),
            Entity::from_raw_u32(9).expect("valid test entity"),
        ];
        let base = PresentationInput {
            layer_tag: 0x51,
            mode: MapViewMode::Civilians,
            world_from_view: Mat4::IDENTITY,
            clip_from_view: Mat4::orthographic_rh(-4.0, 4.0, -3.0, 3.0, 0.1, 100.0),
            focus: Vec3::new(12.0, 0.45, -8.0),
            window_logical_size: Vec2::new(1_280.0, 720.0),
            window_physical_size: UVec2::new(2_560, 1_440),
            window_scale_factor: 2.0,
            logical_viewport: Some(Rect::from_corners(Vec2::ZERO, Vec2::new(1_280.0, 720.0))),
            cell_state_revision: 17,
            contest_revision: 5,
            chunk_index_revision: 3,
            visible_chunks: &chunks,
        };
        let signature = presentation_input_signature_from_parts(base);
        assert_eq!(
            signature,
            presentation_input_signature_from_parts(base),
            "a stationary frame must hit the pre-scan cache"
        );

        let mut changed = base;
        changed.world_from_view = Mat4::from_translation(Vec3::X);
        assert_ne!(signature, presentation_input_signature_from_parts(changed));

        changed = base;
        changed.clip_from_view = Mat4::orthographic_rh(-8.0, 8.0, -6.0, 6.0, 0.1, 100.0);
        assert_ne!(signature, presentation_input_signature_from_parts(changed));

        changed = base;
        changed.focus.x += 1.0;
        assert_ne!(signature, presentation_input_signature_from_parts(changed));

        changed = base;
        changed.window_logical_size.x += 1.0;
        assert_ne!(signature, presentation_input_signature_from_parts(changed));

        changed = base;
        changed.window_physical_size.x += 1;
        assert_ne!(signature, presentation_input_signature_from_parts(changed));

        changed = base;
        changed.window_scale_factor = 1.5;
        assert_ne!(signature, presentation_input_signature_from_parts(changed));

        changed = base;
        changed.logical_viewport = None;
        assert_ne!(signature, presentation_input_signature_from_parts(changed));

        changed = base;
        changed.cell_state_revision += 1;
        assert_ne!(signature, presentation_input_signature_from_parts(changed));

        changed = base;
        changed.contest_revision += 1;
        assert_ne!(signature, presentation_input_signature_from_parts(changed));

        changed = base;
        changed.chunk_index_revision += 1;
        assert_ne!(signature, presentation_input_signature_from_parts(changed));

        let fewer_chunks = &chunks[..1];
        changed = base;
        changed.visible_chunks = fewer_chunks;
        assert_ne!(signature, presentation_input_signature_from_parts(changed));

        changed = base;
        changed.layer_tag = 0x52;
        assert_ne!(signature, presentation_input_signature_from_parts(changed));
    }

    #[test]
    fn direct_keys_and_cycle_change_only_the_view_resource() {
        fn resulting_mode(initial: MapViewMode, key: KeyCode) -> MapViewMode {
            let mut input = ButtonInput::<KeyCode>::default();
            input.press(key);
            let mut app = App::new();
            app.insert_resource(input)
                .insert_resource(initial)
                .add_systems(Update, switch_map_view);
            app.update();
            *app.world().resource::<MapViewMode>()
        }

        assert_eq!(
            resulting_mode(MapViewMode::Soldiers, KeyCode::Digit1),
            MapViewMode::Overview
        );
        assert_eq!(
            resulting_mode(MapViewMode::Overview, KeyCode::Digit2),
            MapViewMode::Soldiers
        );
        assert_eq!(
            resulting_mode(MapViewMode::Overview, KeyCode::Digit3),
            MapViewMode::Civilians
        );
        assert_eq!(
            resulting_mode(MapViewMode::Soldiers, KeyCode::KeyV),
            MapViewMode::Civilians
        );
    }
}
