use std::time::Duration;

use bevy::{
    diagnostic::{DiagnosticsStore, EntityCountDiagnosticsPlugin, FrameTimeDiagnosticsPlugin},
    prelude::*,
    ui::UiRect,
};

use crate::{map_view::MapViewDiagnostics, model::MatchView, terrain::TerrainChunk};

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
                // The help panel occupies x=72..522 at the same vertical
                // band; keep diagnostics in the adjacent HUD slot so both
                // toggles remain readable when enabled together.
                left: px(536),
                top: px(70),
                width: px(390),
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
    map_view: Res<MapViewDiagnostics>,
    chunks: Query<(), With<TerrainChunk>>,
    mut state: ResMut<PerformanceOverlayState>,
    text: Single<&mut Text, With<PerformanceText>>,
) {
    let refresh_due = state.refresh.tick(time.delta()).just_finished();
    // Always sample on the cadence so wasm automation can read `window.__ofPerf`
    // without requiring the F3 panel to be open.
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
    let chunk_count = chunks.iter().count();
    #[cfg(target_arch = "wasm32")]
    publish_browser_perf(
        fps,
        frame_time,
        entities,
        chunk_count,
        map_view.visible_chunks,
        view.cells.len(),
        view.dirty_chunks.len(),
    );

    if !state.visible {
        return;
    }

    let mut text = text.into_inner();
    let value = format!(
        "PERFORMANCE  //  F3\nFPS {:>6}  ·  FRAME {:>7}\nENTITIES {:>6.0}  ·  CHUNKS {:>4} / {:>4} VISIBLE\nCELLS {:>9}  ·  LABELS {:>4}  ·  DIRTY {:>5}\nORDERS {:>8}  ·  FLOWS {:>5}  ·  FRONTS {:>4}",
        format_metric(fps, 1, ""),
        format_metric(frame_time, 2, " ms"),
        entities,
        chunk_count,
        map_view.visible_chunks,
        view.cells.len(),
        map_view.active_labels,
        view.dirty_chunks.len(),
        view.active_orders,
        view.flow_count(),
        view.active_fronts.len(),
    );
    if **text != value {
        **text = value;
    }
}

/// Exposes smoothed F3 diagnostics for browser gate automation (`Runtime.evaluate`).
#[cfg(target_arch = "wasm32")]
fn publish_browser_perf(
    fps: Option<f64>,
    frame_ms: Option<f64>,
    entities: f64,
    chunks: usize,
    visible_chunks: usize,
    cells: usize,
    dirty: usize,
) {
    use js_sys::{Date, Object, Reflect};
    use wasm_bindgen::JsValue;

    let Some(window) = web_sys::window() else {
        return;
    };
    let perf = Object::new();
    let set = |key: &str, value: JsValue| {
        let _ = Reflect::set(&perf, &JsValue::from_str(key), &value);
    };
    match fps {
        Some(value) => set("fps", JsValue::from_f64(value)),
        None => set("fps", JsValue::NULL),
    }
    match frame_ms {
        Some(value) => set("frame_ms", JsValue::from_f64(value)),
        None => set("frame_ms", JsValue::NULL),
    }
    set("entities", JsValue::from_f64(entities));
    set("chunks", JsValue::from_f64(chunks as f64));
    set("visible_chunks", JsValue::from_f64(visible_chunks as f64));
    set("cells", JsValue::from_f64(cells as f64));
    set("dirty", JsValue::from_f64(dirty as f64));
    set("updated_at_ms", JsValue::from_f64(Date::now()));
    let _ = Reflect::set(&window, &JsValue::from_str("__ofPerf"), &perf);
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
