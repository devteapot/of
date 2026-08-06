use std::{any::TypeId, collections::BTreeSet};

use bevy::{
    camera::visibility::{VisibilitySystems, VisibleEntities},
    prelude::*,
};
use hex_core::{Axial, ChunkCoord};

use crate::{
    camera::GameCamera,
    geometry::{chunk_of, corner, edge_index_for_direction, world_center},
    interaction::{InteractionState, OrderMode},
    model::{CellView, MatchView},
    terrain::TerrainChunk,
};

const SOURCE: Color = Color::srgb(0.44, 0.90, 0.94);
const FRIENDLY: Color = Color::srgb(0.26, 0.78, 0.91);
const HOSTILE: Color = Color::srgb(1.0, 0.39, 0.30);
const AMBER: Color = Color::srgb(1.0, 0.69, 0.25);
const RETASK: Color = Color::srgb(0.91, 0.54, 1.0);
const BLOCKED: Color = Color::srgba(0.83, 0.35, 0.24, 0.72);
const BRUSH_VALID: Color = Color::srgba(0.96, 0.98, 1.0, 0.92);
const BRUSH_SELECTED: Color = Color::srgba(0.44, 0.90, 0.94, 0.92);
const BRUSH_RETASK: Color = Color::srgba(0.91, 0.54, 1.0, 0.92);
const BRUSH_BLOCKED: Color = Color::srgba(1.0, 0.69, 0.25, 0.88);
const BRUSH_FOREIGN: Color = Color::srgba(1.0, 0.34, 0.24, 0.88);
const BRUSH_OFF_MAP: Color = Color::srgba(0.48, 0.57, 0.61, 0.78);
const SHAPE_TARGET: Color = Color::srgb(0.54, 0.94, 0.56);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BrushCategory {
    Selectable,
    AlreadySelected,
    RetaskHandle,
    ShapeTarget,
    Blocked,
    Foreign,
    OffMap,
}

#[derive(Clone, Copy)]
struct PerimeterStyle {
    lift: f32,
    scale: f32,
}

pub struct OverlayPlugin;

#[derive(Resource, Default)]
struct OverlayScratch {
    cells: Vec<Axial>,
    chunks: BTreeSet<ChunkCoord>,
    flow_ids: BTreeSet<u64>,
}

impl Plugin for OverlayPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<OverlayScratch>().add_systems(
            PostUpdate,
            draw_world_overlays.after(VisibilitySystems::CheckVisibility),
        );
    }
}

fn draw_world_overlays(
    time: Res<Time>,
    view: Res<MatchView>,
    interaction: Res<InteractionState>,
    visible: Single<&VisibleEntities, With<GameCamera>>,
    chunks: Query<&TerrainChunk>,
    mut scratch: ResMut<OverlayScratch>,
    mut gizmos: Gizmos,
) {
    let needs_visible_cells = interaction.has_selection()
        || !interaction.attack_targets.is_empty()
        || !interaction.preview.front_edges.is_empty()
        || !interaction.preview.wave_depth.is_empty()
        || !interaction.preview.heatmap.is_empty()
        || !interaction.preview.delta_by_cell.is_empty()
        || !interaction.preview.component_routes.is_empty()
        || !interaction.preview.projected_sources.is_empty()
        || !interaction.preview.excluded.is_empty();
    scratch.cells.clear();
    if needs_visible_cells {
        append_visible_cell_coordinates(&visible, &chunks, &mut scratch.cells);
    }
    scratch.chunks.clear();
    if !(view.active_flows.is_empty()
        && view.authoritative_flows.is_empty()
        && view.active_fronts.is_empty()
        && interaction.preview.front_edges.is_empty())
    {
        append_visible_chunk_coordinates(&visible, &chunks, &mut scratch.chunks);
    }
    scratch.flow_ids.clear();
    let OverlayScratch {
        chunks: visible_chunks,
        flow_ids: visible_flow_ids,
        ..
    } = &mut *scratch;
    for chunk in visible_chunks.iter() {
        if let Some(packet_ids) = view.authoritative_flows_by_chunk.get(chunk) {
            visible_flow_ids.extend(packet_ids);
        }
    }
    draw_blocked_cells(&view, &visible, &chunks, &mut gizmos);
    draw_selection(&view, &interaction, &scratch.cells, &mut gizmos);
    draw_preview(
        &view,
        &interaction,
        &scratch.cells,
        &scratch.chunks,
        &mut gizmos,
    );
    draw_committed_orders(
        &view,
        time.elapsed_secs(),
        &scratch.chunks,
        &scratch.flow_ids,
        &mut gizmos,
    );
}

fn append_visible_cell_coordinates(
    visible: &VisibleEntities,
    chunks: &Query<&TerrainChunk>,
    coordinates: &mut Vec<Axial>,
) {
    for chunk in visible
        .iter(TypeId::of::<Mesh3d>())
        .filter_map(|entity| chunks.get(*entity).ok())
    {
        coordinates.extend_from_slice(&chunk.cells);
    }
}

fn append_visible_chunk_coordinates(
    visible: &VisibleEntities,
    chunks: &Query<&TerrainChunk>,
    coordinates: &mut BTreeSet<ChunkCoord>,
) {
    coordinates.extend(
        visible
            .iter(TypeId::of::<Mesh3d>())
            .filter_map(|entity| chunks.get(*entity).ok())
            .map(|chunk| chunk.coordinate),
    );
}

fn draw_blocked_cells(
    view: &MatchView,
    visible: &VisibleEntities,
    chunks: &Query<&TerrainChunk>,
    gizmos: &mut Gizmos,
) {
    for entity in visible.iter(TypeId::of::<Mesh3d>()) {
        let Ok(chunk) = chunks.get(*entity) else {
            continue;
        };
        for cell in chunk
            .cells
            .iter()
            .filter_map(|coordinate| view.cell(*coordinate))
            .filter(|cell| cell.blocked)
        {
            let center = point(cell, 0.075);
            let arm = 0.18;
            gizmos.line(
                center + Vec3::new(-arm, 0.0, -arm),
                center + Vec3::new(arm, 0.0, arm),
                BLOCKED,
            );
            gizmos.line(
                center + Vec3::new(-arm, 0.0, arm),
                center + Vec3::new(arm, 0.0, -arm),
                BLOCKED,
            );
        }
    }
}

fn draw_selection(
    view: &MatchView,
    interaction: &InteractionState,
    visible_cells: &[Axial],
    gizmos: &mut Gizmos,
) {
    draw_region_perimeter(
        view,
        &interaction.sources,
        visible_cells,
        PerimeterStyle {
            lift: 0.09,
            scale: 0.96,
        },
        |_| SOURCE,
        gizmos,
    );

    for coordinate in visible_cells
        .iter()
        .filter(|coordinate| interaction.retask_handles.contains_key(coordinate))
    {
        let Some(cell) = view.cell(*coordinate) else {
            continue;
        };
        let center = point(cell, 0.135);
        for direction in [0, 2, 4] {
            draw_hex_edge(gizmos, center, direction, 0.91, RETASK);
        }
    }

    if matches!(interaction.mode, OrderMode::StopPreview { .. }) {
        draw_region_perimeter(
            view,
            &interaction.preview.projected_sources,
            visible_cells,
            PerimeterStyle {
                lift: 0.155,
                scale: 0.82,
            },
            |_| RETASK,
            gizmos,
        );
    } else if interaction.preview.retask_order_count > 0 {
        for coordinate in visible_cells.iter().filter(|coordinate| {
            interaction.preview.projected_sources.contains(coordinate)
                && !interaction.sources.contains(coordinate)
        }) {
            let Some(cell) = view.cell(*coordinate) else {
                continue;
            };
            let center = point(cell, 0.125);
            for direction in [1, 3, 5] {
                draw_hex_edge(gizmos, center, direction, 0.88, FRIENDLY);
            }
        }
    }

    draw_region_perimeter(
        view,
        &interaction.shape_targets,
        visible_cells,
        PerimeterStyle {
            lift: 0.145,
            scale: 0.88,
        },
        |_| SHAPE_TARGET,
        gizmos,
    );

    draw_region_perimeter(
        view,
        &interaction.attack_targets,
        visible_cells,
        PerimeterStyle {
            lift: 0.155,
            scale: 0.86,
        },
        |_| HOSTILE,
        gizmos,
    );

    if matches!(interaction.mode, OrderMode::ReshapeDrawing)
        && let Some(hovered) = interaction.hovered
    {
        let reference_surface = view
            .cell(hovered)
            .map_or((0, false), |cell| (cell.elevation, cell.is_water()));
        for coordinate in interaction.brush.cells(hovered) {
            let cell = view.cell(coordinate);
            let center = cell.map_or_else(
                || {
                    world_center(coordinate, reference_surface.0, reference_surface.1)
                        + Vec3::Y * 0.13
                },
                |cell| point(cell, 0.13),
            );
            draw_brush_cell(
                gizmos,
                center,
                brush_category(view, interaction, coordinate, true),
            );
        }
    }
}

fn brush_category(
    view: &MatchView,
    interaction: &InteractionState,
    coordinate: Axial,
    drawing_shape: bool,
) -> BrushCategory {
    let Some(cell) = view.cell(coordinate) else {
        return BrushCategory::OffMap;
    };
    if !cell.is_land() || cell.blocked {
        return BrushCategory::Blocked;
    }
    if drawing_shape {
        if interaction.shape_targets.contains(&coordinate) {
            BrushCategory::ShapeTarget
        } else if view.is_local_owned_passable(coordinate) {
            BrushCategory::Selectable
        } else {
            BrushCategory::Foreign
        }
    } else if interaction.sources.contains(&coordinate) {
        BrushCategory::AlreadySelected
    } else if interaction.retask_handles.contains_key(&coordinate)
        || view.is_local_retask_handle(coordinate)
    {
        BrushCategory::RetaskHandle
    } else if view.is_local_owned_passable(coordinate) {
        BrushCategory::Selectable
    } else {
        BrushCategory::Foreign
    }
}

fn draw_brush_cell(gizmos: &mut Gizmos, center: Vec3, category: BrushCategory) {
    match category {
        BrushCategory::Selectable => draw_hex(gizmos, center, 1.01, BRUSH_VALID),
        BrushCategory::AlreadySelected => {
            draw_hex(gizmos, center, 1.01, BRUSH_SELECTED);
            draw_hex(gizmos, center, 0.76, BRUSH_SELECTED);
        }
        BrushCategory::RetaskHandle => {
            for direction in [0, 2, 4] {
                draw_hex_edge(gizmos, center, direction, 1.01, BRUSH_RETASK);
            }
        }
        BrushCategory::ShapeTarget => {
            draw_hex(gizmos, center, 1.01, SHAPE_TARGET);
            draw_hex(gizmos, center, 0.76, SHAPE_TARGET);
        }
        BrushCategory::Blocked => {
            for direction in [0, 2, 4] {
                draw_hex_edge(gizmos, center, direction, 1.01, BRUSH_BLOCKED);
            }
            let arm = 0.22;
            gizmos.line(
                center + Vec3::new(-arm, 0.0, -arm),
                center + Vec3::new(arm, 0.0, arm),
                BRUSH_BLOCKED,
            );
            gizmos.line(
                center + Vec3::new(-arm, 0.0, arm),
                center + Vec3::new(arm, 0.0, -arm),
                BRUSH_BLOCKED,
            );
        }
        BrushCategory::Foreign => draw_segmented_hex(gizmos, center, 1.01, BRUSH_FOREIGN, 0.48),
        BrushCategory::OffMap => draw_segmented_hex(gizmos, center, 1.01, BRUSH_OFF_MAP, 0.20),
    }
}

fn draw_region_perimeter(
    view: &MatchView,
    selection: &BTreeSet<Axial>,
    visible_cells: &[Axial],
    style: PerimeterStyle,
    color_for: impl Fn(&CellView) -> Color,
    gizmos: &mut Gizmos,
) {
    if selection.is_empty() {
        return;
    }
    // Iterate the visible map rather than the selection so symbolic or
    // select-all-sized regions remain bounded by the current viewport.
    for coordinate in visible_cells {
        if !selection.contains(coordinate) {
            continue;
        }
        let Some(cell) = view.cell(*coordinate) else {
            continue;
        };
        let center = point(cell, style.lift);
        let color = color_for(cell);
        for (direction, neighbor) in coordinate.neighbors().into_iter().enumerate() {
            if perimeter_edge_is_exposed(selection, neighbor) {
                draw_hex_edge(gizmos, center, direction, style.scale, color);
            }
        }
    }
}

fn perimeter_edge_is_exposed(selection: &BTreeSet<Axial>, neighbor: Axial) -> bool {
    !selection.contains(&neighbor)
}

fn draw_preview(
    view: &MatchView,
    interaction: &InteractionState,
    visible_cells: &[Axial],
    visible_chunks: &BTreeSet<ChunkCoord>,
    gizmos: &mut Gizmos,
) {
    draw_push_front_edges(view, interaction, visible_chunks, gizmos);
    if matches!(interaction.mode, OrderMode::ExpandAllPreview) {
        draw_expand_wave(view, interaction, visible_cells, gizmos);
    }

    if !interaction.preview.heatmap.is_empty() {
        for coordinate in visible_cells {
            let Some(density) = interaction.preview.heatmap.get(coordinate) else {
                continue;
            };
            let Some(cell) = view.cell(*coordinate) else {
                continue;
            };
            let density = density.clamp(0.0, 1.0);
            let color = Color::srgba(
                0.22 + density * 0.36,
                0.58 + density * 0.32,
                0.69 + density * 0.27,
                0.45 + density * 0.45,
            );
            draw_hex(gizmos, point(cell, 0.115), 0.47 + density * 0.29, color);
        }
    }

    for coordinate in visible_cells {
        let Some(&delta) = interaction.preview.delta_by_cell.get(coordinate) else {
            continue;
        };
        let Some(cell) = view.cell(*coordinate) else {
            continue;
        };
        draw_redistribution_delta(gizmos, point(cell, 0.205), delta);
    }

    if !interaction.preview.excluded.is_empty() {
        for coordinate in visible_cells {
            if !interaction.preview.excluded.contains(coordinate) {
                continue;
            }
            let Some(cell) = view.cell(*coordinate) else {
                continue;
            };
            let center = point(cell, 0.15);
            gizmos.line(
                center + Vec3::new(-0.30, 0.0, -0.30),
                center + Vec3::new(0.30, 0.0, 0.30),
                HOSTILE,
            );
            gizmos.line(
                center + Vec3::new(-0.30, 0.0, 0.30),
                center + Vec3::new(0.30, 0.0, -0.30),
                HOSTILE,
            );
        }
    }

    if !matches!(interaction.mode, OrderMode::ExpandAllPreview) {
        for route in &interaction.preview.component_routes {
            let route_points = route
                .iter()
                .filter_map(|coordinate| view.cell(*coordinate))
                .map(|cell| point(cell, 0.19))
                .collect::<Vec<_>>();
            draw_dashed_route(gizmos, &route_points, AMBER);
        }
    }

    for &(from, to) in &interaction.preview.component_bottlenecks {
        if let (Some(from), Some(to)) = (view.cell(from), view.cell(to)) {
            let a = point(from, 0.205);
            let b = point(to, 0.205);
            let direction = (b - a).normalize_or_zero();
            let side = Vec3::new(-direction.z, 0.0, direction.x) * 0.055;
            gizmos.line(a + side, b + side, HOSTILE);
            gizmos.line(a - side, b - side, HOSTILE);
        }
    }

    if matches!(
        interaction.mode,
        OrderMode::PushFrontOrient { .. } | OrderMode::PushFrontPreview { .. }
    ) && let Some(direction) = interaction.push_direction()
        && let Some(center) = projected_selection_center(view, interaction)
    {
        let direction = axial_to_world_direction(direction);
        gizmos
            .arrow(center - direction * 0.65, center + direction * 1.8, AMBER)
            .with_tip_length(0.38);
    }
}

fn projected_selection_center(view: &MatchView, interaction: &InteractionState) -> Option<Vec3> {
    let order_ids = interaction.supersede_order_ids();
    view.project_order_selection(&interaction.sources, &order_ids)
        .ok()
        .and_then(|projection| selection_center(view, &projection.cells))
}

fn draw_expand_wave(
    view: &MatchView,
    interaction: &InteractionState,
    visible_cells: &[Axial],
    gizmos: &mut Gizmos,
) {
    let max_depth = interaction
        .preview
        .wave_depth
        .values()
        .copied()
        .max()
        .unwrap_or(1);
    for depth in 1..=max_depth {
        let ring = interaction
            .preview
            .wave_depth
            .iter()
            .filter_map(|(&coordinate, &coordinate_depth)| {
                (coordinate_depth == depth).then_some(coordinate)
            })
            .collect::<BTreeSet<_>>();
        if ring.is_empty() {
            continue;
        }
        let progress = if max_depth <= 1 {
            0.0
        } else {
            f32::from(depth - 1) / f32::from(max_depth - 1)
        };
        let alpha = 0.94 - progress * 0.58;
        let color = Color::srgba(
            1.0 - progress * 0.22,
            0.69 + progress * 0.14,
            0.25 + progress * 0.48,
            alpha,
        );
        draw_region_perimeter(
            view,
            &ring,
            visible_cells,
            PerimeterStyle {
                lift: 0.15 + f32::from(depth) * 0.002,
                scale: 0.91,
            },
            |_| color,
            gizmos,
        );
    }
}

fn draw_push_front_edges(
    view: &MatchView,
    interaction: &InteractionState,
    visible_chunks: &BTreeSet<ChunkCoord>,
    gizmos: &mut Gizmos,
) {
    for edge in &interaction.preview.front_edges {
        if !edge_touches_visible_chunk(visible_chunks, edge.source, edge.target) {
            continue;
        }
        let (Some(source), Some(target)) = (view.cell(edge.source), view.cell(edge.target)) else {
            continue;
        };
        let Some(direction) = edge
            .source
            .neighbors()
            .iter()
            .position(|neighbor| *neighbor == edge.target)
        else {
            continue;
        };
        let color = push_front_edge_color(view.local_player, target.owner);
        let source_point = point(source, 0.155);
        let target_point = point(target, 0.155);
        draw_hex_edge(gizmos, source_point, direction, 1.025, color);
        gizmos
            .arrow(
                source_point.lerp(target_point, 0.24),
                source_point.lerp(target_point, 0.76),
                color,
            )
            .with_tip_length(0.17);
    }
}

fn edge_touches_visible_chunk(
    visible_chunks: &BTreeSet<ChunkCoord>,
    source: Axial,
    target: Axial,
) -> bool {
    visible_chunks.contains(&chunk_of(source)) || visible_chunks.contains(&chunk_of(target))
}

fn push_front_edge_color(local_player: u32, target_owner: Option<u32>) -> Color {
    if target_owner == Some(local_player) {
        FRIENDLY
    } else if target_owner.is_some() {
        HOSTILE
    } else {
        AMBER
    }
}

fn axial_to_world_direction(direction: Axial) -> Vec3 {
    let plane = crate::geometry::axial_to_plane(direction).normalize_or_zero();
    Vec3::new(plane.x, 0.0, plane.y)
}

fn draw_committed_orders(
    view: &MatchView,
    elapsed: f32,
    visible_chunks: &BTreeSet<ChunkCoord>,
    visible_flow_ids: &BTreeSet<u64>,
    gizmos: &mut Gizmos,
) {
    for flow in &view.active_flows {
        if flow.route.len() < 2
            || !flow
                .route
                .iter()
                .any(|coordinate| visible_chunks.contains(&chunk_of(*coordinate)))
        {
            continue;
        }
        draw_flow(view, flow, visible_chunks, gizmos);
    }
    for packet_id in visible_flow_ids {
        if let Some(flow) = view.authoritative_flows.get(packet_id) {
            draw_flow(view, flow, visible_chunks, gizmos);
        }
    }

    for front in &view.active_fronts {
        if !visible_chunks.contains(&chunk_of(front.friendly))
            && !visible_chunks.contains(&chunk_of(front.hostile))
        {
            continue;
        }
        let (Some(friendly), Some(hostile)) = (view.cell(front.friendly), view.cell(front.hostile))
        else {
            continue;
        };
        let friendly = point(friendly, 0.22);
        let hostile = point(hostile, 0.22);
        let middle = friendly.lerp(hostile, 0.5);
        let direction = (hostile - friendly).normalize_or_zero();
        let side = Vec3::new(-direction.z, 0.0, direction.x);
        let pulse = 0.82
            + (elapsed * 5.0 + front.age).sin().abs() * 0.18
            + front.intensity.clamp(0.0, 1.0) * 0.10;
        for offset in [-0.16, 0.0, 0.16] {
            let lane = side * offset * pulse;
            gizmos
                .arrow(friendly + lane, middle - direction * 0.06 + lane, FRIENDLY)
                .with_tip_length(0.16);
            gizmos
                .arrow(hostile + lane, middle + direction * 0.06 + lane, HOSTILE)
                .with_tip_length(0.16);
        }
    }
}

fn draw_flow(
    view: &MatchView,
    flow: &crate::model::ActiveFlow,
    visible_chunks: &BTreeSet<ChunkCoord>,
    gizmos: &mut Gizmos,
) {
    if flow.route.len() < 2 {
        return;
    }
    let color = if flow.attacking { HOSTILE } else { FRIENDLY };
    for pair in flow.route.windows(2).filter(|pair| {
        visible_chunks.contains(&chunk_of(pair[0])) || visible_chunks.contains(&chunk_of(pair[1]))
    }) {
        let (Some(from), Some(to)) = (view.cell(pair[0]), view.cell(pair[1])) else {
            continue;
        };
        gizmos.line(point(from, 0.16), point(to, 0.16), color);
    }

    let travel = ((flow.age * 0.68).fract() * (flow.route.len() - 1) as f32)
        .clamp(0.0, (flow.route.len() - 1) as f32);
    let index = travel.floor() as usize;
    let next = (index + 1).min(flow.route.len() - 1);
    if !visible_chunks.contains(&chunk_of(flow.route[index]))
        && !visible_chunks.contains(&chunk_of(flow.route[next]))
    {
        return;
    }
    let (Some(from), Some(to)) = (view.cell(flow.route[index]), view.cell(flow.route[next])) else {
        return;
    };
    let moving = point(from, 0.16).lerp(point(to, 0.16), travel.fract()) + Vec3::Y * 0.04;
    let marker_radius = 0.075 + (flow.strength as f32 / 700.0).clamp(0.0, 0.075);
    gizmos.sphere(moving, marker_radius, color).resolution(4);
}

fn point(cell: &CellView, lift: f32) -> Vec3 {
    world_center(cell.coordinate, cell.elevation, cell.is_water()) + Vec3::Y * lift
}

fn draw_hex(gizmos: &mut Gizmos, center: Vec3, scale: f32, color: Color) {
    let points = (0..6).map(|index| {
        let full = corner(center, index, center.y);
        center + (full - center) * scale
    });
    gizmos.lineloop(points, color);
}

fn draw_hex_edge(gizmos: &mut Gizmos, center: Vec3, direction: usize, scale: f32, color: Color) {
    let start_corner = edge_index_for_direction(direction);
    let end_corner = (start_corner + 1) % 6;
    let start = center + (corner(center, start_corner, center.y) - center) * scale;
    let end = center + (corner(center, end_corner, center.y) - center) * scale;
    gizmos.line(start, end, color);
}

fn draw_segmented_hex(
    gizmos: &mut Gizmos,
    center: Vec3,
    scale: f32,
    color: Color,
    segment_fraction: f32,
) {
    for direction in 0..6 {
        let start_corner = edge_index_for_direction(direction);
        let end_corner = (start_corner + 1) % 6;
        let start = center + (corner(center, start_corner, center.y) - center) * scale;
        let end = center + (corner(center, end_corner, center.y) - center) * scale;
        let middle = start.lerp(end, 0.5);
        let half = segment_fraction.clamp(0.05, 0.95) * 0.5;
        gizmos.line(start.lerp(middle, 1.0 - half), middle, color);
        gizmos.line(middle, end.lerp(middle, 1.0 - half), color);
    }
}

#[cfg(test)]
fn perimeter_edge_count(selection: &BTreeSet<Axial>) -> usize {
    selection
        .iter()
        .map(|coordinate| {
            coordinate
                .neighbors()
                .into_iter()
                .filter(|neighbor| !selection.contains(neighbor))
                .count()
        })
        .sum()
}

fn draw_dashed_route(gizmos: &mut Gizmos, points: &[Vec3], color: Color) {
    for pair in points.windows(2) {
        let segment = pair[1] - pair[0];
        for index in 0..3 {
            let start = index as f32 / 3.0 + 0.04;
            let end = (start + 0.20).min(1.0);
            gizmos.line(pair[0] + segment * start, pair[0] + segment * end, color);
        }
        gizmos
            .arrow(pair[0] + segment * 0.68, pair[0] + segment * 0.88, color)
            .with_tip_length(0.13);
    }
}

fn redistribution_delta_scale(delta: i128) -> f32 {
    let normalized = delta.unsigned_abs().min(10_000) as f32 / 10_000.0;
    0.10 + normalized.sqrt() * 0.16
}

fn draw_redistribution_delta(gizmos: &mut Gizmos, center: Vec3, delta: i128) {
    let arm = redistribution_delta_scale(delta);
    let horizontal = Vec3::new(arm, 0.0, 0.0);
    gizmos.line(
        center - horizontal,
        center + horizontal,
        if delta > 0 { SHAPE_TARGET } else { HOSTILE },
    );
    if delta > 0 {
        let vertical = Vec3::new(0.0, 0.0, arm);
        gizmos.line(center - vertical, center + vertical, SHAPE_TARGET);
    }
}

fn selection_center(
    view: &MatchView,
    selection: &std::collections::BTreeSet<Axial>,
) -> Option<Vec3> {
    let (sum, count) = selection
        .iter()
        .filter_map(|coordinate| view.cell(*coordinate))
        .map(|cell| point(cell, 0.24))
        .fold((Vec3::ZERO, 0_u32), |(sum, count), point| {
            (sum + point, count + 1)
        });
    (count > 0).then(|| sum / count as f32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hex_core::TerrainKind;

    fn cell(coordinate: Axial, owner: Option<u32>, blocked: bool) -> CellView {
        CellView {
            coordinate,
            terrain: TerrainKind::Plains,
            elevation: 0,
            owner,
            civilians: 0,
            infantry: 0,
            military_capacity: 100,
            blocked,
        }
    }

    #[test]
    fn perimeter_counts_only_exposed_edges() {
        assert_eq!(perimeter_edge_count(&BTreeSet::new()), 0);
        assert_eq!(perimeter_edge_count(&BTreeSet::from([Axial::ZERO])), 6);
        assert_eq!(
            perimeter_edge_count(&BTreeSet::from([Axial::ZERO, Axial::new(1, 0)])),
            10
        );

        let center_and_neighbors = Axial::ZERO
            .neighbors()
            .into_iter()
            .chain([Axial::ZERO])
            .collect();
        assert_eq!(perimeter_edge_count(&center_and_neighbors), 18);
    }

    #[test]
    fn push_edge_visibility_uses_chunks_instead_of_cell_ordering() {
        let visible = BTreeSet::from([chunk_of(Axial::ZERO)]);
        assert!(edge_touches_visible_chunk(
            &visible,
            Axial::ZERO,
            Axial::new(20, 0),
        ));
        assert!(edge_touches_visible_chunk(
            &visible,
            Axial::new(20, 0),
            Axial::new(1, 0),
        ));
        assert!(!edge_touches_visible_chunk(
            &visible,
            Axial::new(20, 0),
            Axial::new(21, 0),
        ));
    }

    #[test]
    fn brush_categories_keep_the_full_footprint_legible() {
        let selected = Axial::ZERO;
        let available = Axial::new(1, 0);
        let blocked = Axial::new(0, 1);
        let foreign = Axial::new(-1, 0);
        let off_map = Axial::new(8, 8);
        let mut view = MatchView::connecting(1);
        view.cells.insert(selected, cell(selected, Some(1), false));
        view.cells
            .insert(available, cell(available, Some(1), false));
        view.cells.insert(blocked, cell(blocked, Some(1), true));
        view.cells.insert(foreign, cell(foreign, Some(2), false));
        let mut interaction = InteractionState::default();
        interaction.sources.insert(selected);

        assert_eq!(
            brush_category(&view, &interaction, selected, false),
            BrushCategory::AlreadySelected
        );
        assert_eq!(
            brush_category(&view, &interaction, available, false),
            BrushCategory::Selectable
        );
        assert_eq!(
            brush_category(&view, &interaction, blocked, false),
            BrushCategory::Blocked
        );
        assert_eq!(
            brush_category(&view, &interaction, foreign, false),
            BrushCategory::Foreign
        );
        assert_eq!(
            brush_category(&view, &interaction, off_map, false),
            BrushCategory::OffMap
        );
    }

    #[test]
    fn push_edges_distinguish_inward_neutral_and_hostile_targets() {
        assert_eq!(push_front_edge_color(1, Some(1)), FRIENDLY);
        assert_eq!(push_front_edge_color(1, None), AMBER);
        assert_eq!(push_front_edge_color(1, Some(2)), HOSTILE);
    }

    #[test]
    fn redistribution_delta_glyphs_scale_by_magnitude_not_sign() {
        assert!(
            (redistribution_delta_scale(25) - redistribution_delta_scale(-25)).abs() < f32::EPSILON
        );
        assert!(redistribution_delta_scale(250) > redistribution_delta_scale(25));
        assert!(redistribution_delta_scale(25_000) <= 0.26);
    }
}
