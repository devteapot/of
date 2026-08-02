use bevy::prelude::*;
use hex_core::Axial;

use crate::{
    geometry::{corner, world_center},
    interaction::{InteractionState, OrderMode},
    model::{CellView, MatchView},
};

const SOURCE: Color = Color::srgb(0.44, 0.90, 0.94);
const FRIENDLY: Color = Color::srgb(0.26, 0.78, 0.91);
const HOSTILE: Color = Color::srgb(1.0, 0.39, 0.30);
const AMBER: Color = Color::srgb(1.0, 0.69, 0.25);
const BLOCKED: Color = Color::srgba(0.83, 0.35, 0.24, 0.72);

pub struct OverlayPlugin;

impl Plugin for OverlayPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Update, draw_world_overlays);
    }
}

fn draw_world_overlays(
    time: Res<Time>,
    view: Res<MatchView>,
    interaction: Res<InteractionState>,
    mut gizmos: Gizmos,
) {
    draw_blocked_cells(&view, &mut gizmos);
    draw_selection(&view, &interaction, &mut gizmos);
    draw_preview(&view, &interaction, &mut gizmos);
    draw_committed_orders(&view, time.elapsed_secs(), &mut gizmos);
}

fn draw_blocked_cells(view: &MatchView, gizmos: &mut Gizmos) {
    for cell in view.cells.values().filter(|cell| cell.blocked) {
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

fn draw_selection(view: &MatchView, interaction: &InteractionState, gizmos: &mut Gizmos) {
    for coordinate in &interaction.sources {
        let Some(cell) = view.cell(*coordinate) else {
            continue;
        };
        draw_hex(gizmos, point(cell, 0.09), 0.96, SOURCE);
        draw_corner_ticks(gizmos, point(cell, 0.095), SOURCE);
    }

    for coordinate in &interaction.destinations {
        let Some(cell) = view.cell(*coordinate) else {
            continue;
        };
        let color = if cell.owner == Some(view.local_player) {
            FRIENDLY
        } else {
            HOSTILE
        };
        draw_hex(gizmos, point(cell, 0.105), 0.74, color);
    }

    if let Some(hovered) = interaction.hovered
        && let Some(cell) = view.cell(hovered)
    {
        draw_hex(
            gizmos,
            point(cell, 0.13),
            1.01,
            Color::srgba(0.96, 0.98, 1.0, 0.92),
        );
    }
}

fn draw_preview(view: &MatchView, interaction: &InteractionState, gizmos: &mut Gizmos) {
    for (coordinate, density) in &interaction.preview.heatmap {
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

    for coordinate in &interaction.preview.excluded {
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

fn draw_corner_ticks(gizmos: &mut Gizmos, center: Vec3, color: Color) {
    for index in 0..6 {
        let point = corner(center, index, center.y);
        let inward = (center - point).normalize_or_zero();
        gizmos.line(point, point + inward * 0.17, color);
    }
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
