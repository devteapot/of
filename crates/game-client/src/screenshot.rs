use std::{collections::BTreeSet, path::PathBuf};

use bevy::{
    prelude::*,
    render::view::screenshot::{Screenshot, save_to_disk},
};
use hex_core::Axial;

use crate::{
    camera::{CameraRig, GameCamera, look_at_world},
    config::{ClientConfig, ScreenshotScene},
    geometry::axial_to_plane,
    interaction::{ForcedMapHover, InteractionState},
    model::{MatchView, PLAYER_ONE, PLAYER_TWO},
};

const WARMUP: u32 = 48;
const SETTLE: u32 = 24;

#[derive(Resource, Debug)]
struct CapturePlan {
    path: PathBuf,
    scene: ScreenshotScene,
    frames: u32,
    staged: bool,
    requested: bool,
}

pub struct OfflineScreenshotPlugin;

impl Plugin for OfflineScreenshotPlugin {
    fn build(&self, app: &mut App) {
        let config = app.world().resource::<ClientConfig>();
        let Some(path) = config.screenshot_path.clone() else {
            return;
        };
        app.insert_resource(CapturePlan {
            path,
            scene: config.screenshot_scene,
            frames: 0,
            staged: false,
            requested: false,
        })
        .add_systems(Update, (stage_and_capture, drain_exit));
    }
}

fn stage_and_capture(
    mut commands: Commands,
    mut plan: ResMut<CapturePlan>,
    mut interaction: ResMut<InteractionState>,
    mut view: ResMut<MatchView>,
    camera: Option<Single<(&mut CameraRig, &mut Transform, &mut Projection), With<GameCamera>>>,
) {
    plan.frames = plan.frames.saturating_add(1);
    if !plan.staged && plan.frames >= WARMUP {
        let focus = stage_scene(&mut commands, &mut view, &mut interaction, plan.scene);
        if let Some(camera) = camera {
            let (mut rig, mut transform, mut projection) = camera.into_inner();
            look_at_world(&mut rig, &mut transform, focus);
            if !matches!(plan.scene, ScreenshotScene::Idle)
                && let Projection::Orthographic(orthographic) = &mut *projection
            {
                orthographic.scale = 0.52;
            }
        }
        plan.staged = true;
    }
    if plan.staged && !plan.requested && plan.frames >= WARMUP + SETTLE {
        commands
            .spawn(Screenshot::primary_window())
            .observe(save_to_disk(plan.path.clone()));
        plan.requested = true;
        commands.spawn(DelayedExit::default());
    }
}

#[derive(Component, Default)]
struct DelayedExit {
    frames: u32,
}

fn drain_exit(
    mut commands: Commands,
    mut pending: Query<(Entity, &mut DelayedExit)>,
    mut exit: MessageWriter<AppExit>,
) {
    for (entity, mut delay) in &mut pending {
        delay.frames = delay.frames.saturating_add(1);
        if delay.frames >= 24 {
            commands.entity(entity).despawn();
            exit.write(AppExit::Success);
        }
    }
}

fn stage_scene(
    commands: &mut Commands,
    view: &mut MatchView,
    interaction: &mut InteractionState,
    scene: ScreenshotScene,
) -> Vec3 {
    match scene {
        ScreenshotScene::Idle => {
            interaction.sources.clear();
            interaction.source_revision = interaction.source_revision.wrapping_add(1);
            interaction.invalidate_preview();
            commands.insert_resource(ForcedMapHover(None));
            Vec3::new(0.0, 0.45, 0.0)
        }
        ScreenshotScene::ExpandHover => {
            let focus = Axial::new(-4, 0);
            let seed = Axial::new(-5, 0);
            interaction.sources = owned_cluster(view, seed);
            interaction.source_revision = interaction.source_revision.wrapping_add(1);
            interaction.invalidate_preview();
            commands.insert_resource(ForcedMapHover(Some(focus)));
            world_focus(view, focus)
        }
        ScreenshotScene::AttackHover => {
            let inland = Axial::new(4, 0);
            let contact = Axial::new(5, 0);
            let enemy = Axial::new(6, 0);
            claim_screenshot_cell(view, inland, PLAYER_ONE, 40);
            claim_screenshot_cell(view, contact, PLAYER_ONE, 40);
            if let Some(cell) = view.cell_mut(enemy) {
                cell.owner = Some(PLAYER_TWO);
            }
            view.mark_ownership_changed();
            interaction.sources = BTreeSet::from([inland, contact]);
            interaction.source_revision = interaction.source_revision.wrapping_add(1);
            interaction.invalidate_preview();
            commands.insert_resource(ForcedMapHover(Some(enemy)));
            world_focus(view, contact)
        }
    }
}

fn claim_screenshot_cell(view: &mut MatchView, coordinate: Axial, owner: u32, infantry: u64) {
    if let Some(cell) = view.cell_mut(coordinate) {
        cell.owner = Some(owner);
        cell.infantry = cell.military_capacity.min(infantry);
    }
}

fn world_focus(view: &MatchView, coordinate: Axial) -> Vec3 {
    let plane = axial_to_plane(coordinate);
    let elevation = view.cell(coordinate).map_or(1, |cell| cell.elevation);
    Vec3::new(plane.x, 0.45 + f32::from(elevation) * 0.18, plane.y)
}

fn owned_cluster(view: &MatchView, seed: Axial) -> BTreeSet<Axial> {
    if !view.is_local_owned_passable(seed) {
        return view
            .cells
            .values()
            .find(|cell| {
                cell.owner == Some(PLAYER_ONE) && view.is_local_owned_passable(cell.coordinate)
            })
            .map(|cell| flood(view, cell.coordinate))
            .unwrap_or_default();
    }
    flood(view, seed)
}

fn flood(view: &MatchView, seed: Axial) -> BTreeSet<Axial> {
    let mut cluster = BTreeSet::from([seed]);
    let mut frontier = vec![seed];
    while let Some(coordinate) = frontier.pop() {
        for neighbor in coordinate.neighbors() {
            if view.is_local_traversable_edge(coordinate, neighbor) && cluster.insert(neighbor) {
                frontier.push(neighbor);
            }
        }
    }
    cluster
}
