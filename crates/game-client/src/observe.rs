//! Lightweight structured observability for the Bevy client.
//!
//! Events use stable `category.action` keys so browser `DevTools`, native stderr,
//! and the F4 overlay stay greppable. Console emission is gated by
//! `OF_OBSERVE=1` / `--observe` (native) or `?observe=1` (wasm); the in-memory
//! ring always records recent events so F4 works without a restart.

use std::collections::VecDeque;
use std::fmt::Write as _;

use bevy::{
    diagnostic::{DiagnosticsStore, FrameTimeDiagnosticsPlugin},
    prelude::*,
    ui::UiRect,
};

const RING_CAPACITY: usize = 64;
const FRAME_SPIKE_MS: f64 = 33.0;
/// Prefer Bevy `Time` over `std::time::Instant` — Instant panics on wasm.
const FRAME_SPIKE_COOLDOWN_SECS: f64 = 2.0;
const PANEL: Color = Color::srgba(0.025, 0.038, 0.047, 0.94);
const LINE: Color = Color::srgba(0.42, 0.58, 0.65, 0.48);
const TEXT: Color = Color::srgb(0.72, 0.91, 0.93);

/// Stable event keys. Prefer extending this list over inventing ad-hoc strings.
pub mod keys {
    pub const NET_CONNECT_BEGIN: &str = "net.connect_begin";
    pub const NET_CONNECTED: &str = "net.connected";
    pub const NET_BOOTSTRAP: &str = "net.bootstrap";
    pub const NET_TACTICAL: &str = "net.tactical";
    pub const NET_DISCONNECT: &str = "net.disconnect";
    pub const NET_RECONNECT: &str = "net.reconnect";
    pub const NET_CONNECT_FAIL: &str = "net.connect_fail";
    pub const NET_JOIN_FAIL: &str = "net.join_fail";
    pub const LOBBY_ACTION: &str = "lobby.action";
    pub const CMD_SUBMIT: &str = "cmd.submit";
    pub const CMD_ACCEPT: &str = "cmd.accept";
    pub const CMD_REJECT: &str = "cmd.reject";
    pub const CMD_FAIL: &str = "cmd.fail";
    pub const SYNC_APPLY: &str = "sync.apply";
    pub const PERF_FRAME_SPIKE: &str = "perf.frame_spike";
    pub const TOKEN_WARN: &str = "auth.token_warn";
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, PartialOrd, Ord)]
pub enum ObserveLevel {
    Debug,
    Info,
    Warn,
    Error,
}

impl ObserveLevel {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Debug => "DEBUG",
            Self::Info => "INFO",
            Self::Warn => "WARN",
            Self::Error => "ERROR",
        }
    }
}

#[derive(Clone, Debug)]
struct ObserveRecord {
    seq: u64,
    level: ObserveLevel,
    key: &'static str,
    detail: String,
}

#[derive(Resource)]
pub struct ObserveState {
    /// When true, events also go to Bevy's log (stderr / browser console).
    pub console_enabled: bool,
    pub overlay_visible: bool,
    next_seq: u64,
    events: VecDeque<ObserveRecord>,
    last_frame_spike_at_secs: Option<f64>,
    refresh_overlay: bool,
}

impl ObserveState {
    pub fn new(console_enabled: bool) -> Self {
        Self {
            console_enabled,
            overlay_visible: false,
            next_seq: 1,
            events: VecDeque::with_capacity(RING_CAPACITY),
            last_frame_spike_at_secs: None,
            refresh_overlay: false,
        }
    }

    pub fn emit(&mut self, level: ObserveLevel, key: &'static str, detail: impl Into<String>) {
        let detail = detail.into();
        let seq = self.next_seq;
        self.next_seq = self.next_seq.saturating_add(1);
        if self.console_enabled {
            emit_console(level, key, &detail);
        }
        self.events.push_back(ObserveRecord {
            seq,
            level,
            key,
            detail,
        });
        while self.events.len() > RING_CAPACITY {
            self.events.pop_front();
        }
        self.refresh_overlay = true;
    }

    fn note_frame_spike(&mut self, now_secs: f64, frame_ms: f64, fps: Option<f64>) {
        if self
            .last_frame_spike_at_secs
            .is_some_and(|previous| now_secs - previous < FRAME_SPIKE_COOLDOWN_SECS)
        {
            return;
        }
        self.last_frame_spike_at_secs = Some(now_secs);
        let fps_part = fps.map_or_else(|| "fps=--".to_owned(), |value| format!("fps={value:.1}"));
        self.emit(
            ObserveLevel::Warn,
            keys::PERF_FRAME_SPIKE,
            format!("frame_ms={frame_ms:.2} {fps_part} threshold_ms={FRAME_SPIKE_MS}"),
        );
    }
}

fn emit_console(level: ObserveLevel, key: &str, detail: &str) {
    let message = format!("[of.observe] {} {key} {detail}", level.as_str());
    match level {
        ObserveLevel::Debug => bevy::log::debug!(target: "of.observe", "{message}"),
        ObserveLevel::Info => bevy::log::info!(target: "of.observe", "{message}"),
        ObserveLevel::Warn => bevy::log::warn!(target: "of.observe", "{message}"),
        ObserveLevel::Error => bevy::log::error!(target: "of.observe", "{message}"),
    }
}

#[derive(Component)]
struct ObservePanel;

#[derive(Component)]
struct ObserveText;

pub struct ObservePlugin {
    pub console_enabled: bool,
}

impl Plugin for ObservePlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(ObserveState::new(self.console_enabled))
            .add_systems(Startup, spawn_observe_overlay)
            .add_systems(
                Update,
                (
                    toggle_observe_overlay,
                    detect_frame_spikes,
                    update_observe_overlay,
                )
                    .chain(),
            );
    }
}

fn spawn_observe_overlay(mut commands: Commands) {
    commands
        .spawn((
            Name::new("Observe overlay"),
            ObservePanel,
            Node {
                display: Display::None,
                position_type: PositionType::Absolute,
                left: px(72),
                top: px(286),
                width: px(620),
                max_height: px(320),
                padding: UiRect::all(px(10)),
                border: UiRect::all(px(1)),
                flex_direction: FlexDirection::Column,
                row_gap: px(4),
                overflow: Overflow::clip_y(),
                ..default()
            },
            BackgroundColor(PANEL),
            BorderColor::all(LINE),
            GlobalZIndex(31),
            Pickable::IGNORE,
        ))
        .with_child((
            ObserveText,
            Text::new("OBSERVE  //  F4\nWaiting for events…"),
            TextFont::from_font_size(11.0),
            TextColor(TEXT),
            Pickable::IGNORE,
        ));
}

fn toggle_observe_overlay(
    keyboard: Res<ButtonInput<KeyCode>>,
    mut state: ResMut<ObserveState>,
    panel: Single<&mut Node, With<ObservePanel>>,
) {
    if !keyboard.just_pressed(KeyCode::F4) {
        return;
    }
    let mut panel = panel.into_inner();
    state.overlay_visible = !state.overlay_visible;
    panel.display = if state.overlay_visible {
        Display::Flex
    } else {
        Display::None
    };
    state.refresh_overlay = state.overlay_visible;
}

fn detect_frame_spikes(
    time: Res<Time>,
    diagnostics: Res<DiagnosticsStore>,
    mut state: ResMut<ObserveState>,
) {
    let Some(frame_ms) = diagnostics
        .get(&FrameTimeDiagnosticsPlugin::FRAME_TIME)
        .and_then(|diagnostic| diagnostic.value().or_else(|| diagnostic.smoothed()))
    else {
        return;
    };
    if frame_ms < FRAME_SPIKE_MS {
        return;
    }
    let fps = diagnostics
        .get(&FrameTimeDiagnosticsPlugin::FPS)
        .and_then(|diagnostic| diagnostic.smoothed().or_else(|| diagnostic.value()));
    state.note_frame_spike(time.elapsed_secs_f64(), frame_ms, fps);
}

fn update_observe_overlay(
    mut state: ResMut<ObserveState>,
    text: Single<&mut Text, With<ObserveText>>,
) {
    if !state.overlay_visible || !state.refresh_overlay {
        return;
    }
    state.refresh_overlay = false;
    let console = if state.console_enabled {
        "console ON"
    } else {
        "console OFF · OF_OBSERVE / ?observe=1"
    };
    let mut body = format!("OBSERVE  //  F4  ·  {console}\n");
    if state.events.is_empty() {
        body.push_str("Waiting for events…");
    } else {
        for record in state.events.iter().rev().take(18) {
            let _ = writeln!(
                body,
                "{:>4} {:>5} {} {}",
                record.seq,
                record.level.as_str(),
                record.key,
                record.detail
            );
        }
    }
    let mut text = text.into_inner();
    if **text != body {
        **text = body;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_retains_latest_events_and_assigns_stable_keys() {
        let mut state = ObserveState::new(false);
        for index in 0..RING_CAPACITY + 5 {
            state.emit(
                ObserveLevel::Info,
                keys::NET_CONNECTED,
                format!("gen={index}"),
            );
        }
        assert_eq!(state.events.len(), RING_CAPACITY);
        assert_eq!(state.events.front().unwrap().detail, "gen=5");
        assert_eq!(
            state.events.back().unwrap().detail,
            format!("gen={}", RING_CAPACITY + 4)
        );
        assert!(
            state
                .events
                .iter()
                .all(|record| record.key == keys::NET_CONNECTED)
        );
    }

    #[test]
    fn frame_spike_is_rate_limited() {
        let mut state = ObserveState::new(false);
        state.note_frame_spike(40.0, 40.0, Some(25.0));
        state.note_frame_spike(50.0, 50.0, Some(20.0));
        assert_eq!(state.events.len(), 1);
        assert_eq!(state.events[0].key, keys::PERF_FRAME_SPIKE);
    }
}
