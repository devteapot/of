use std::{collections::BTreeSet, path::PathBuf};

use bevy::{
    prelude::*,
    render::view::screenshot::{Screenshot, save_to_disk},
};
use hex_core::Axial;

use crate::{
    config::{ClientConfig, ScreenshotScene},
    interaction::InteractionState,
    model::{MatchView, PLAYER_ONE, PLAYER_TWO},
};

const WARMUP: u32 = 40;
const SETTLE: u32 = 18;

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
) {
    plan.frames = plan.frames.saturating_add(1);
    if !plan.staged && plan.frames >= WARMUP {
        stage_scene(&mut view, &mut interaction, plan.scene);
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

fn stage_scene(view: &mut MatchView, interaction: &mut InteractionState, scene: ScreenshotScene) {
    match scene {
        ScreenshotScene::Idle => {
            interaction.hovered = None;
            interaction.sources.clear();
            interaction.source_revision = interaction.source_revision.wrapping_add(1);
            interaction.invalidate_preview();
        }
        ScreenshotScene::ExpandHover => {
            let seed = Axial::new(-8, 0);
            let cluster = owned_cluster(view, seed);
            let focus = cluster
                .iter()
                .flat_map(|coordinate| coordinate.neighbors())
                .find(|neighbor| {
                    view.cell(*neighbor)
                        .is_some_and(|cell| cell.owner.is_none() && view.is_capturable(*neighbor))
                })
                .unwrap_or(Axial::new(-4, 0));
            interaction.sources = cluster;
            interaction.hovered = Some(focus);
            interaction.last_map_hovered = Some(focus);
            interaction.source_revision = interaction.source_revision.wrapping_add(1);
            interaction.invalidate_preview();
        }
        ScreenshotScene::AttackHover => {
            let contact = Axial::new(5, 0);
            let enemy = Axial::new(6, 0);
            if let Some(cell) = view.cells.get_mut(&contact) {
                cell.owner = Some(PLAYER_ONE);
                cell.infantry = cell.military_capacity.min(40);
            }
            if let Some(cell) = view.cells.get_mut(&enemy) {
                cell.owner = Some(PLAYER_TWO);
            }
            view.ownership_revision = view.ownership_revision.wrapping_add(1);
            view.planning_revision = view.planning_revision.wrapping_add(1);
            view.cell_state_revision = view.cell_state_revision.wrapping_add(1);
            interaction.sources = BTreeSet::from([contact]);
            interaction.hovered = Some(enemy);
            interaction.last_map_hovered = Some(enemy);
            interaction.source_revision = interaction.source_revision.wrapping_add(1);
            interaction.invalidate_preview();
        }
    }
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
