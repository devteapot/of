use std::time::Duration;

use bevy::{
    diagnostic::{DiagnosticsStore, EntityCountDiagnosticsPlugin, FrameTimeDiagnosticsPlugin},
    prelude::*,
    ui::UiRect,
};

use crate::{model::MatchView, terrain::TerrainChunk};

const REFRESH_INTERVAL: Duration = Duration::from_millis(250);
const PANEL: Color = Color::srgba(0.025, 0.038, 0.047, 0.94);
const LINE: Color = Color::srgba(0.42, 0.58, 0.65, 0.48);
const TEXT: Color = Color::srgb(0.72, 0.91, 0.93);

#[derive(Component)]
struct PerformancePanel;

#[derive(Component)]
struct PerformanceText;

#[derive(Resource)]
struct PerformanceOverlayState {
    visible: bool,
    refresh: Timer,
    refresh_now: bool,
}

impl Default for PerformanceOverlayState {
    fn default() -> Self {
        Self {
            visible: false,
            refresh: Timer::new(REFRESH_INTERVAL, TimerMode::Repeating),
            refresh_now: false,
        }
    }
}

pub struct PerformanceOverlayPlugin;

impl Plugin for PerformanceOverlayPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins((
            FrameTimeDiagnosticsPlugin::default(),
            EntityCountDiagnosticsPlugin::default(),
        ))
        .init_resource::<PerformanceOverlayState>()
        .add_systems(Startup, spawn_performance_overlay)
        .add_systems(
            Update,
            (toggle_performance_overlay, update_performance_overlay).chain(),
        );
    }
}

fn spawn_performance_overlay(mut commands: Commands) {
    commands
        .spawn((
            Name::new("Performance overlay"),
            PerformancePanel,
            Node {
                display: Display::None,
                position_type: PositionType::Absolute,
                left: px(14),
                top: px(70),
                min_width: px(236),
                padding: UiRect::all(px(10)),
                border: UiRect::all(px(1)),
                ..default()
            },
            BackgroundColor(PANEL),
            BorderColor::all(LINE),
            GlobalZIndex(30),
            Pickable::IGNORE,
        ))
        .with_child((
            PerformanceText,
            Text::new("PERFORMANCE  //  F3\nCollecting diagnostics…"),
            TextFont::from_font_size(11.0),
            TextColor(TEXT),
            Pickable::IGNORE,
        ));
}

fn toggle_performance_overlay(
    keyboard: Res<ButtonInput<KeyCode>>,
    mut state: ResMut<PerformanceOverlayState>,
    panel: Single<&mut Node, With<PerformancePanel>>,
) {
    if keyboard.just_pressed(KeyCode::F3) {
        let mut panel = panel.into_inner();
        state.visible = !state.visible;
        panel.display = if state.visible {
            Display::Flex
        } else {
            Display::None
        };
        state.refresh_now = state.visible;
    }
}

fn update_performance_overlay(
    time: Res<Time>,
    diagnostics: Res<DiagnosticsStore>,
    view: Res<MatchView>,
    chunks: Query<(), With<TerrainChunk>>,
    mut state: ResMut<PerformanceOverlayState>,
    text: Single<&mut Text, With<PerformanceText>>,
) {
    if !state.visible {
        return;
    }

    let refresh_due = state.refresh.tick(time.delta()).just_finished();
    if !state.refresh_now && !refresh_due {
        return;
    }
    state.refresh_now = false;

    let fps = diagnostic_smoothed(&diagnostics, &FrameTimeDiagnosticsPlugin::FPS);
    let frame_time = diagnostic_smoothed(&diagnostics, &FrameTimeDiagnosticsPlugin::FRAME_TIME);
    let entities = diagnostics
        .get(&EntityCountDiagnosticsPlugin::ENTITY_COUNT)
        .and_then(bevy::diagnostic::Diagnostic::value)
        .unwrap_or(0.0);
    let mut text = text.into_inner();
    let value = format!(
        "PERFORMANCE  //  F3\nFPS {:>6}  ·  FRAME {:>7}\nENTITIES {:>6.0}  ·  CHUNKS {:>4}\nCELLS {:>9}  ·  DIRTY {:>5}\nORDERS {:>8}  ·  FLOWS {:>5}  ·  FRONTS {:>4}",
        format_metric(fps, 1, ""),
        format_metric(frame_time, 2, " ms"),
        entities,
        chunks.iter().count(),
        view.cells.len(),
        view.dirty_chunks.len(),
        view.active_orders,
        view.active_flows.len(),
        view.active_fronts.len(),
    );
    if **text != value {
        **text = value;
    }
}

fn diagnostic_smoothed(
    diagnostics: &DiagnosticsStore,
    path: &bevy::diagnostic::DiagnosticPath,
) -> Option<f64> {
    diagnostics
        .get(path)
        .and_then(|diagnostic| diagnostic.smoothed().or_else(|| diagnostic.value()))
}

fn format_metric(value: Option<f64>, precision: usize, suffix: &str) -> String {
    value.map_or_else(
        || "     --".to_owned(),
        |value| format!("{value:>6.precision$}{suffix}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn f3_toggles_the_panel_without_enabling_it_by_default() {
        let mut app = App::new();
        app.init_resource::<PerformanceOverlayState>()
            .insert_resource(ButtonInput::<KeyCode>::default())
            .add_systems(Update, toggle_performance_overlay);
        let panel = app
            .world_mut()
            .spawn((
                PerformancePanel,
                Node {
                    display: Display::None,
                    ..default()
                },
            ))
            .id();

        assert!(!app.world().resource::<PerformanceOverlayState>().visible);
        assert_eq!(
            app.world().entity(panel).get::<Node>().unwrap().display,
            Display::None
        );

        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .press(KeyCode::F3);
        app.update();

        assert!(app.world().resource::<PerformanceOverlayState>().visible);
        assert_eq!(
            app.world().entity(panel).get::<Node>().unwrap().display,
            Display::Flex
        );

        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .reset(KeyCode::F3);
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .press(KeyCode::F3);
        app.update();

        assert!(!app.world().resource::<PerformanceOverlayState>().visible);
        assert_eq!(
            app.world().entity(panel).get::<Node>().unwrap().display,
            Display::None
        );
    }

    #[test]
    fn missing_metrics_have_a_stable_placeholder() {
        assert_eq!(format_metric(None, 2, " ms"), "     --");
        assert_eq!(format_metric(Some(16.625), 2, " ms"), " 16.62 ms");
    }
}
