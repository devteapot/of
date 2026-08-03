// Bevy ECS system parameters must be owned wrapper values. The remaining cast
// and shape allowances are limited to bounded presentation-space conversion
// and declarative UI/system composition in this native client crate.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::needless_pass_by_value,
    clippy::too_many_lines,
    clippy::type_complexity
)]

mod camera;
mod config;
mod geometry;
mod hud;
mod interaction;
mod model;
mod network;
mod online;
mod overlays;
mod performance;
mod terrain;

use bevy::{prelude::*, window::WindowResolution};

use camera::{camera_controls, spawn_camera_and_light};
use config::ClientConfig;
use hud::HudPlugin;
use interaction::GameInteractionPlugin;
use model::{MatchView, update_transient_state};
use network::{NetworkBoundaryPlugin, NetworkSet, OfflineTransportPlugin, apply_server_updates};
use online::{OnlineSyncSet, OnlineTransportPlugin};
use overlays::OverlayPlugin;
use performance::PerformanceOverlayPlugin;
use terrain::{spawn_terrain, sync_terrain_chunks};

fn main() {
    let config = ClientConfig::from_process();
    let match_view = if config.offline {
        MatchView::offline_fixture()
    } else {
        MatchView::connecting(config.preferred_player)
    };
    let mut app = App::new();
    app.insert_resource(config.clone())
        .insert_resource(match_view)
        .add_plugins(DefaultPlugins.set(WindowPlugin {
            primary_window: Some(Window {
                title: format!("Hex RTS · V1 {}", config.mode_label()),
                resolution: WindowResolution::new(1440, 900),
                resizable: true,
                ..default()
            }),
            ..default()
        }))
        .add_plugins((
            NetworkBoundaryPlugin,
            GameInteractionPlugin,
            OverlayPlugin,
            HudPlugin,
            PerformanceOverlayPlugin,
        ))
        .insert_resource(ClearColor(Color::srgb(0.018, 0.025, 0.031)))
        .insert_resource(GlobalAmbientLight {
            color: Color::srgb(0.47, 0.56, 0.66),
            brightness: 340.0,
            ..default()
        })
        .add_systems(Startup, (spawn_camera_and_light, spawn_terrain).chain())
        .add_systems(
            Update,
            (
                camera_controls,
                update_transient_state,
                apply_server_updates
                    .in_set(NetworkSet::Apply)
                    .after(OnlineSyncSet),
                sync_terrain_chunks.after(NetworkSet::Apply),
            ),
        );
    if config.offline {
        app.add_plugins(OfflineTransportPlugin);
    } else {
        app.add_plugins(OnlineTransportPlugin);
    }
    app.run();
}
