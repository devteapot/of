use std::{any::TypeId, collections::BTreeSet};

use bevy::{
    camera::visibility::{VisibilitySystems, VisibleEntities},
    prelude::*,
};
use hex_core::Axial;

use crate::{
    camera::GameCamera,
    geometry::{corner, edge_index_for_direction, world_center},
    interaction::{InteractionState, OrderMode},
    model::{CellView, MatchView},
    terrain::TerrainChunk,
};

const SOURCE: Color = Color::srgb(0.44, 0.90, 0.94);
const FRIENDLY: Color = Color::srgb(0.26, 0.78, 0.91);
const HOSTILE: Color = Color::srgb(1.0, 0.39, 0.30);
const AMBER: Color = Color::srgb(1.0, 0.69, 0.25);
const BLOCKED: Color = Color::srgba(0.83, 0.35, 0.24, 0.72);

#[derive(Clone, Copy)]
struct PerimeterStyle {
    lift: f32,
    scale: f32,
    split_destination_classes: bool,
}

pub struct OverlayPlugin;

impl Plugin for OverlayPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(
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
    mut gizmos: Gizmos,
) {
    let needs_visible_cells = !interaction.sources.is_empty()
        || !interaction.destinations.is_empty()
        || !interaction.preview.heatmap.is_empty()
        || !interaction.preview.excluded.is_empty();
    let visible_cells = if needs_visible_cells {
        visible_cell_coordinates(&visible, &chunks)
    } else {
        Vec::new()
    };
    draw_blocked_cells(&view, &visible, &chunks, &mut gizmos);
    draw_selection(&view, &interaction, &visible_cells, &mut gizmos);
    draw_preview(&view, &interaction, &visible_cells, &mut gizmos);
    draw_committed_orders(&view, time.elapsed_secs(), &mut gizmos);
}

fn visible_cell_coordinates(
    visible: &VisibleEntities,
    chunks: &Query<&TerrainChunk>,
) -> Vec<Axial> {
    visible
        .iter(TypeId::of::<Mesh3d>())
        .filter_map(|entity| chunks.get(*entity).ok())
        .flat_map(|chunk| chunk.cells.iter().copied())
        .collect()
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
            split_destination_classes: false,
        },
        |_| SOURCE,
        gizmos,
    );

    draw_region_perimeter(
        view,
        &interaction.destinations,
        visible_cells,
        PerimeterStyle {
            lift: 0.105,
            scale: 0.74,
            split_destination_classes: true,
        },
        |cell| {
            if cell.owner == Some(view.local_player) {
                FRIENDLY
            } else {
                HOSTILE
            }
        },
        gizmos,
    );

    let hovered_footprint = interaction.hovered.map_or_else(BTreeSet::new, |hovered| {
        interaction
            .brush
            .cells(hovered)
            .into_iter()
            .filter(|coordinate| match interaction.mode {
                OrderMode::Idle => view.is_local_owned(*coordinate),
                OrderMode::Transfer => view.cell(*coordinate).is_some_and(CellView::is_land),
                _ => false,
            })
            .collect()
    });
    let hovered_cells = hovered_footprint.iter().copied().collect::<Vec<_>>();
    draw_region_perimeter(
        view,
        &hovered_footprint,
        &hovered_cells,
        PerimeterStyle {
            lift: 0.13,
            scale: 1.01,
            split_destination_classes: false,
        },
        |_| Color::srgba(0.96, 0.98, 1.0, 0.92),
        gizmos,
    );
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
            if perimeter_edge_is_exposed(
                view,
                selection,
                cell,
                neighbor,
                style.split_destination_classes,
            ) {
                draw_hex_edge(gizmos, center, direction, style.scale, color);
            }
        }
    }
}

fn perimeter_edge_is_exposed(
    view: &MatchView,
    selection: &BTreeSet<Axial>,
    cell: &CellView,
    neighbor: Axial,
    split_destination_classes: bool,
) -> bool {
    !selection.contains(&neighbor)
        || (split_destination_classes
            && view.cell(neighbor).is_some_and(|neighbor| {
                (neighbor.owner == Some(view.local_player))
                    != (cell.owner == Some(view.local_player))
            }))
}

fn draw_preview(
    view: &MatchView,
    interaction: &InteractionState,
    visible_cells: &[Axial],
    gizmos: &mut Gizmos,
) {
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

    let route_points = interaction
        .preview
        .route
        .iter()
        .filter_map(|coordinate| view.cell(*coordinate))
        .map(|cell| point(cell, 0.19))
        .collect::<Vec<_>>();
    draw_dashed_route(gizmos, &route_points, AMBER);

    if let Some((from, to)) = interaction.preview.bottleneck
        && let (Some(from), Some(to)) = (view.cell(from), view.cell(to))
    {
        let a = point(from, 0.205);
        let b = point(to, 0.205);
        let direction = (b - a).normalize_or_zero();
        let side = Vec3::new(-direction.z, 0.0, direction.x) * 0.055;
        gizmos.line(a + side, b + side, HOSTILE);
        gizmos.line(a - side, b - side, HOSTILE);
    }

    if matches!(
        interaction.mode,
        OrderMode::FrontLoadOrient { .. } | OrderMode::FrontLoadPreview { .. }
    ) && let Some(direction) = interaction.frontload_direction()
        && let Some(center) = selection_center(view, &interaction.sources)
    {
        let direction = Vec3::new(direction.x, 0.0, direction.y);
        gizmos
            .arrow(center - direction * 1.2, center + direction * 1.7, SOURCE)
            .with_tip_length(0.34);
    }
}

fn draw_committed_orders(view: &MatchView, elapsed: f32, gizmos: &mut Gizmos) {
    for flow in &view.active_flows {
        let points = flow
            .route
            .iter()
            .filter_map(|coordinate| view.cell(*coordinate))
            .map(|cell| point(cell, 0.16))
            .collect::<Vec<_>>();
        if points.len() < 2 {
            continue;
        }
        let color = if flow.attacking { HOSTILE } else { FRIENDLY };
        for pair in points.windows(2) {
            gizmos.line(pair[0], pair[1], color);
        }
        let travel = ((flow.age * 0.68).fract() * (points.len() - 1) as f32)
            .clamp(0.0, (points.len() - 1) as f32);
        let index = travel.floor() as usize;
        let next = (index + 1).min(points.len() - 1);
        let moving = points[index].lerp(points[next], travel.fract()) + Vec3::Y * 0.04;
        let marker_radius = 0.075 + (flow.strength as f32 / 700.0).clamp(0.0, 0.075);
        gizmos.sphere(moving, marker_radius, color).resolution(10);
    }

    for front in &view.active_fronts {
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

    fn cell(coordinate: Axial, owner: u32) -> CellView {
        CellView {
            coordinate,
            terrain: TerrainKind::Plains,
            elevation: 0,
            owner: Some(owner),
            civilians: 0,
            infantry: 0,
            military_capacity: 100,
            blocked: false,
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
    fn destination_perimeter_exposes_friendly_hostile_transitions() {
        let friendly = Axial::ZERO;
        let hostile = Axial::new(1, 0);
        let mut view = MatchView::connecting(1);
        view.cells.insert(friendly, cell(friendly, 1));
        view.cells.insert(hostile, cell(hostile, 2));
        let selection = BTreeSet::from([friendly, hostile]);

        assert!(!perimeter_edge_is_exposed(
            &view,
            &selection,
            view.cell(friendly).expect("friendly cell"),
            hostile,
            false,
        ));
        assert!(perimeter_edge_is_exposed(
            &view,
            &selection,
            view.cell(friendly).expect("friendly cell"),
            hostile,
            true,
        ));
        assert!(perimeter_edge_is_exposed(
            &view,
            &selection,
            view.cell(hostile).expect("hostile cell"),
            friendly,
            true,
        ));
    }
}
