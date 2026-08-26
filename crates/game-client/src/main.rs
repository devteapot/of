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
mod lobby;
mod map_view;
mod model;
mod network;
mod observe;
mod online;
mod overlays;
mod performance;
mod population_outline;
#[cfg(not(target_arch = "wasm32"))]
mod screenshot;
mod terrain;

use bevy::{prelude::*, window::WindowResolution};

use camera::{camera_controls, spawn_camera_and_light};
use config::ClientConfig;
use hud::HudPlugin;
use interaction::GameInteractionPlugin;
use lobby::LobbyPlugin;
use map_view::MapViewPlugin;
use model::{MatchView, update_transient_state};
use network::{NetworkBoundaryPlugin, NetworkSet, OfflineTransportPlugin, apply_server_updates};
use observe::ObservePlugin;
use online::{OnlineSyncSet, OnlineTransportPlugin};
use overlays::OverlayPlugin;
use performance::PerformanceOverlayPlugin;
use population_outline::PopulationOutlinePlugin;
use terrain::{spawn_terrain, sync_terrain_chunks};
use worldgen::v2::{WorldSpec, generate as generate_v2};

fn main() {
    let config = ClientConfig::from_process();
    let match_view = if let Some(options) = &config.layered_world {
        eprintln!(
            "Generating layered V2 viewer map {}x{} · {} players · seed {}…",
            options.width, options.height, options.players, options.seed
        );
        let mut spec = WorldSpec::new(
            format!("viewer-v2-{}x{}", options.width, options.height),
            options.width,
            options.height,
            options.seed,
        );
        spec.player_count = options.players;
        let world = generate_v2(&spec).unwrap_or_else(|error| {
            eprintln!("failed to generate layered V2 viewer map: {error}");
            std::process::exit(2);
        });
        eprintln!(
            "Generated {:016x} · {} land · {} lake · {} river cells",
            world.manifest.content_hash,
            world.manifest.land_cells,
            world.manifest.lake_cells,
            world.manifest.river_cells,
        );
        MatchView::offline_layered_world(&world, config.preferred_player)
    } else if config.offline {
        MatchView::offline_fixture()
    } else {
        MatchView::connecting(config.preferred_player)
    };
    let window_title = if config.layered_world.is_some() {
        "OnlyFronts · Layered V2 Viewer".to_owned()
    } else {
        format!("OnlyFronts · V1 {}", config.mode_label())
    };
    let mut app = App::new();
    app.insert_resource(config.clone())
        .insert_resource(match_view)
        .add_plugins(DefaultPlugins.set(WindowPlugin {
            primary_window: Some(Window {
                title: window_title,
                resolution: WindowResolution::new(1440, 900),
                resizable: true,
                canvas: Some("#game-canvas".to_owned()),
                fit_canvas_to_parent: true,
                ..default()
            }),
            ..default()
        }))
        .add_plugins((
            NetworkBoundaryPlugin,
            GameInteractionPlugin,
            MapViewPlugin,
            PopulationOutlinePlugin,
            OverlayPlugin,
            LobbyPlugin,
            HudPlugin,
            PerformanceOverlayPlugin,
            ObservePlugin {
                console_enabled: config.observe,
            },
        ))
        .insert_resource(ClearColor(Color::srgb(0.28, 0.62, 0.86)))
        .insert_resource(GlobalAmbientLight {
            color: Color::srgb(0.92, 0.78, 1.0),
            brightness: 220.0,
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
    #[cfg(not(target_arch = "wasm32"))]
    if config.screenshot_path.is_some() {
        app.add_plugins(screenshot::OfflineScreenshotPlugin);
    }
    app.run();
}
