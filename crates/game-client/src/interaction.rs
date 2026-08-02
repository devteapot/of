use std::collections::{BTreeMap, BTreeSet};

use bevy::{picking::pointer::PointerInteraction, prelude::*};
use hex_core::Axial;

use crate::{
    camera::GameCamera,
    geometry::axial_to_plane,
    model::{CellView, MatchView, ToastKind, find_route},
    network::{ClientIntent, NetworkSet, RedistributionPreset, ServerUpdate},
    terrain::TerrainChunk,
};

#[derive(Clone, Debug, Default)]
pub struct OrderPreview {
    pub route: Vec<Axial>,
    pub excluded: BTreeSet<Axial>,
    pub heatmap: BTreeMap<Axial, f32>,
    pub eta_seconds: u32,
    pub requested_strength: u64,
    pub destination_capacity: u64,
    pub bottleneck: Option<(Axial, Axial)>,
}

#[derive(Clone, Debug)]
pub enum OrderMode {
    Idle,
    Transfer,
    BalancePreview,
    FrontLoadOrient { start: Vec3, current: Vec3 },
    FrontLoadPreview { direction: Vec2 },
    Submitting { label: &'static str },
}

impl OrderMode {
    pub const fn label(&self) -> &'static str {
        match self {
            Self::Idle => "SOURCE SELECTION",
            Self::Transfer => "TRANSFER PREVIEW",
            Self::BalancePreview => "BALANCE PREVIEW",
            Self::FrontLoadOrient { .. } => "FRONT-LOAD · ORIENT",
            Self::FrontLoadPreview { .. } => "FRONT-LOAD PREVIEW",
            Self::Submitting { .. } => "SUBMITTING",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PaintOperation {
    Add,
    Remove,
}

#[derive(Clone, Copy, Debug)]
struct PaintStroke {
    operation: PaintOperation,
    last: Option<Axial>,
}

#[derive(Resource, Debug)]
pub struct InteractionState {
    pub hovered: Option<Axial>,
    pub cursor_world: Option<Vec3>,
    pub sources: BTreeSet<Axial>,
    pub destinations: BTreeSet<Axial>,
    pub mode: OrderMode,
    pub amount_percent: u8,
    pub preview: OrderPreview,
    pub show_help: bool,
    stroke: Option<PaintStroke>,
    return_after_rejection: Option<OrderMode>,
    submitting_command_id: Option<u64>,
}

impl Default for InteractionState {
    fn default() -> Self {
        Self {
            hovered: None,
            cursor_world: None,
            sources: BTreeSet::new(),
            destinations: BTreeSet::new(),
            mode: OrderMode::Idle,
            amount_percent: 50,
            preview: OrderPreview::default(),
            show_help: false,
            stroke: None,
            return_after_rejection: None,
            submitting_command_id: None,
        }
    }
}

impl InteractionState {
    pub fn frontload_direction(&self) -> Option<Vec2> {
        match self.mode {
            OrderMode::FrontLoadOrient { start, current } => {
                let value = Vec2::new(current.x - start.x, current.z - start.z);
                (value.length_squared() > 0.01).then(|| value.normalize())
            }
            OrderMode::FrontLoadPreview { direction } => Some(direction),
            _ => None,
        }
    }
}

#[derive(Message, Clone, Copy, Debug)]
pub enum UiAction {
    Transfer,
    Balance,
    FrontLoad,
    Confirm,
    Cancel,
    AmountDown,
    AmountUp,
}

pub struct GameInteractionPlugin;

impl Plugin for GameInteractionPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<InteractionState>()
            .add_message::<UiAction>()
            .add_systems(
                Update,
                (
                    update_hovered_cell,
                    process_order_input,
                    paint_regions,
                    update_order_preview,
                    finish_submission,
                )
                    .chain()
                    .before(NetworkSet::Transport),
            );
    }
}

#[allow(clippy::too_many_arguments)]
fn update_hovered_cell(
    camera: Single<(&Camera, &GlobalTransform), With<GameCamera>>,
    window: Single<&Window>,
    pointers: Query<&PointerInteraction>,
    ui_nodes: Query<(), With<Node>>,
    chunks: Query<&TerrainChunk>,
    mut ray_cast: MeshRayCast,
    mut interaction: ResMut<InteractionState>,
) {
    let pointer_over_ui = pointers.iter().any(|pointer| {
        pointer
            .get_nearest_hit()
            .is_some_and(|(entity, _)| ui_nodes.contains(*entity))
    });
    if pointer_over_ui {
        interaction.hovered = None;
        // Keep the last map-space position so a HUD action such as Front-load
        // can seed its orientation gesture without inventing screen/world math.
        return;
    }

    let (camera, camera_transform) = *camera;
    let Some(cursor) = window.cursor_position() else {
        interaction.hovered = None;
        interaction.cursor_world = None;
        return;
    };
    let Ok(ray) = camera.viewport_to_world(camera_transform, cursor) else {
        return;
    };
    let filter = |entity| chunks.contains(entity);
    let settings = MeshRayCastSettings::default()
        .with_filter(&filter)
        .always_early_exit();
    let Some((chunk_entity, hit)) = ray_cast.cast_ray(ray, &settings).first() else {
        interaction.hovered = None;
        interaction.cursor_world = None;
        return;
    };
    let Some(triangle_index) = hit.triangle_index else {
        return;
    };
    let Ok(chunk) = chunks.get(*chunk_entity) else {
        return;
    };
    interaction.hovered = chunk.triangle_to_cell.get(triangle_index).copied();
    interaction.cursor_world = Some(hit.point);
}

#[allow(clippy::too_many_arguments)]
fn process_order_input(
    keyboard: Res<ButtonInput<KeyCode>>,
    mut actions: MessageReader<UiAction>,
    mut interaction: ResMut<InteractionState>,
    mut view: ResMut<MatchView>,
    mut intents: MessageWriter<ClientIntent>,
) {
    if keyboard.just_pressed(KeyCode::Slash) {
        interaction.show_help = !interaction.show_help;
    }

    if keyboard.pressed(KeyCode::KeyM) {
        let increase =
            keyboard.just_pressed(KeyCode::ArrowRight) || keyboard.just_pressed(KeyCode::ArrowUp);
        let decrease =
            keyboard.just_pressed(KeyCode::ArrowLeft) || keyboard.just_pressed(KeyCode::ArrowDown);
        if increase || decrease {
            let delta = if increase { 0.05 } else { -0.05 };
            intents.write(ClientIntent::SetMobilization {
                target: (view.mobilization_target + delta).clamp(0.0, 1.0),
            });
        }
    }

    let mut requested_actions = Vec::new();
    if keyboard.just_pressed(KeyCode::KeyT) {
        requested_actions.push(UiAction::Transfer);
    }
    if keyboard.just_pressed(KeyCode::KeyB) {
        requested_actions.push(UiAction::Balance);
    }
    if keyboard.just_pressed(KeyCode::Enter) {
        requested_actions.push(UiAction::Confirm);
    }
    if keyboard.just_pressed(KeyCode::Escape) {
        requested_actions.push(UiAction::Cancel);
    }
    if keyboard.just_pressed(KeyCode::BracketLeft) {
        requested_actions.push(UiAction::AmountDown);
    }
    if keyboard.just_pressed(KeyCode::BracketRight) {
        requested_actions.push(UiAction::AmountUp);
    }
    requested_actions.extend(actions.read().copied());

    if keyboard.just_pressed(KeyCode::KeyF) {
        requested_actions.push(UiAction::FrontLoad);
    }

    for action in requested_actions {
        handle_action(action, &mut interaction, &mut view, &mut intents);
    }

    let cursor_world = interaction.cursor_world;
    if let OrderMode::FrontLoadOrient { start, current } = &mut interaction.mode {
        if let Some(cursor) = cursor_world {
            *current = cursor;
        }
        if keyboard.just_released(KeyCode::KeyF) {
            let direction = Vec2::new(current.x - start.x, current.z - start.z);
            if direction.length() < 0.35 {
                interaction.mode = OrderMode::Idle;
                view.show_toast(
                    "Front-load needs a visible drag direction",
                    ToastKind::Rejection,
                );
            } else {
                interaction.mode = OrderMode::FrontLoadPreview {
                    direction: direction.normalize(),
                };
            }
        }
    }
}

fn handle_action(
    action: UiAction,
    interaction: &mut InteractionState,
    view: &mut MatchView,
    intents: &mut MessageWriter<ClientIntent>,
) {
    match action {
        UiAction::Transfer => {
            if interaction.sources.is_empty() {
                view.show_toast("Paint owned source hexes first", ToastKind::Rejection);
            } else if matches!(interaction.mode, OrderMode::Idle) {
                interaction.destinations.clear();
                interaction.mode = OrderMode::Transfer;
                view.show_toast("Paint friendly or hostile destinations", ToastKind::Info);
            }
        }
        UiAction::Balance => {
            if interaction.sources.len() < 2 {
                view.show_toast(
                    "Balance needs at least two owned hexes",
                    ToastKind::Rejection,
                );
            } else if matches!(interaction.mode, OrderMode::Idle) {
                interaction.mode = OrderMode::BalancePreview;
            }
        }
        UiAction::FrontLoad => {
            if interaction.sources.len() < 2 {
                view.show_toast(
                    "Front-load needs at least two owned hexes",
                    ToastKind::Rejection,
                );
            } else if matches!(interaction.mode, OrderMode::Idle)
                && let Some(start) = interaction.cursor_world
            {
                interaction.mode = OrderMode::FrontLoadOrient {
                    start,
                    current: start,
                };
                view.show_toast("Hold F and move the pointer to orient", ToastKind::Info);
            }
        }
        UiAction::Confirm => submit_current(interaction, view, intents),
        UiAction::Cancel => {
            if matches!(interaction.mode, OrderMode::Idle) {
                interaction.sources.clear();
            } else if !matches!(interaction.mode, OrderMode::Submitting { .. }) {
                interaction.destinations.clear();
                interaction.preview = OrderPreview::default();
                interaction.mode = OrderMode::Idle;
            }
        }
        UiAction::AmountDown => {
            if matches!(interaction.mode, OrderMode::Transfer) {
                interaction.amount_percent = interaction.amount_percent.saturating_sub(10).max(10);
            }
        }
        UiAction::AmountUp => {
            if matches!(interaction.mode, OrderMode::Transfer) {
                interaction.amount_percent = interaction.amount_percent.saturating_add(10).min(100);
            }
        }
    }
}

fn submit_current(
    interaction: &mut InteractionState,
    view: &mut MatchView,
    intents: &mut MessageWriter<ClientIntent>,
) {
    let (intent, label, return_mode) = match &interaction.mode {
        OrderMode::Transfer if !interaction.destinations.is_empty() => (
            ClientIntent::Transfer {
                sources: interaction.sources.clone(),
                destinations: interaction.destinations.clone(),
                amount_percent: interaction.amount_percent,
            },
            "TRANSFER",
            OrderMode::Transfer,
        ),
        OrderMode::BalancePreview => (
            ClientIntent::Redistribute {
                cells: interaction.sources.clone(),
                preset: RedistributionPreset::Balance,
                direction: None,
            },
            "BALANCE",
            OrderMode::BalancePreview,
        ),
        OrderMode::FrontLoadPreview { direction } => (
            ClientIntent::Redistribute {
                cells: interaction.sources.clone(),
                preset: RedistributionPreset::FrontLoad,
                direction: Some(*direction),
            },
            "FRONT-LOAD",
            OrderMode::FrontLoadPreview {
                direction: *direction,
            },
        ),
        OrderMode::Transfer => {
            view.show_toast("Paint at least one destination", ToastKind::Rejection);
            return;
        }
        _ => return,
    };
    interaction.return_after_rejection = Some(return_mode);
    interaction.submitting_command_id = None;
    interaction.mode = OrderMode::Submitting { label };
    intents.write(intent);
}

fn paint_regions(
    keyboard: Res<ButtonInput<KeyCode>>,
    mouse: Res<ButtonInput<MouseButton>>,
    view: Res<MatchView>,
    mut interaction: ResMut<InteractionState>,
) {
    let can_paint = matches!(interaction.mode, OrderMode::Idle | OrderMode::Transfer)
        && !keyboard.pressed(KeyCode::Space)
        && !mouse.pressed(MouseButton::Middle);
    if !can_paint {
        interaction.stroke = None;
        return;
    }

    if mouse.just_pressed(MouseButton::Left) {
        let removing =
            keyboard.pressed(KeyCode::ControlLeft) || keyboard.pressed(KeyCode::ControlRight);
        let additive =
            keyboard.pressed(KeyCode::ShiftLeft) || keyboard.pressed(KeyCode::ShiftRight);
        if !removing && !additive {
            if matches!(interaction.mode, OrderMode::Transfer) {
                interaction.destinations.clear();
            } else {
                interaction.sources.clear();
            }
        }
        interaction.stroke = Some(PaintStroke {
            operation: if removing {
                PaintOperation::Remove
            } else {
                PaintOperation::Add
            },
            last: None,
        });
    }

    if mouse.pressed(MouseButton::Left) {
        let hovered = interaction.hovered;
        let Some(mut stroke) = interaction.stroke else {
            return;
        };
        if hovered == stroke.last {
            return;
        }
        stroke.last = hovered;
        interaction.stroke = Some(stroke);
        let Some(coordinate) = hovered else {
            return;
        };

        if matches!(interaction.mode, OrderMode::Transfer) {
            let valid = view.cell(coordinate).is_some_and(CellView::is_land);
            if valid {
                match stroke.operation {
                    PaintOperation::Add => {
                        interaction.destinations.insert(coordinate);
                    }
                    PaintOperation::Remove => {
                        interaction.destinations.remove(&coordinate);
                    }
                }
            }
        } else if view.is_local_owned(coordinate) {
            match stroke.operation {
                PaintOperation::Add => {
                    interaction.sources.insert(coordinate);
                }
                PaintOperation::Remove => {
                    interaction.sources.remove(&coordinate);
                }
            }
        }
    }

    if mouse.just_released(MouseButton::Left) {
        interaction.stroke = None;
    }
}

fn update_order_preview(view: Res<MatchView>, mut interaction: ResMut<InteractionState>) {
    let mut preview = OrderPreview::default();
    match &interaction.mode {
        OrderMode::Transfer | OrderMode::Submitting { label: "TRANSFER" } => {
            preview.route = find_route(&view, &interaction.sources, &interaction.destinations)
                .unwrap_or_default();
            preview.requested_strength = interaction
                .sources
                .iter()
                .filter_map(|coordinate| view.cell(*coordinate))
                .map(|cell| cell.infantry * u64::from(interaction.amount_percent) / 100)
                .sum();
            preview.destination_capacity = interaction
                .destinations
                .iter()
                .filter_map(|coordinate| view.cell(*coordinate))
                .map(CellView::free_capacity)
                .sum();
            for destination in &interaction.destinations {
                let singleton = BTreeSet::from([*destination]);
                if find_route(&view, &interaction.sources, &singleton).is_none() {
                    preview.excluded.insert(*destination);
                }
            }
            if preview.route.len() >= 2 {
                preview.bottleneck = preview
                    .route
                    .windows(2)
                    .min_by_key(|edge| view.cell(edge[1]).map_or(0, |cell| cell.military_capacity))
                    .map(|edge| (edge[0], edge[1]));
                let congestion = preview
                    .requested_strength
                    .saturating_sub(preview.destination_capacity)
                    / 20;
                preview.eta_seconds =
                    (preview.route.len() as u64 * 2 + congestion).min(u64::from(u32::MAX)) as u32;
            }
        }
        OrderMode::BalancePreview | OrderMode::Submitting { label: "BALANCE" } => {
            let (strength, capacity, _) = view.selected_totals(&interaction.sources);
            let target = if capacity == 0 {
                0.0
            } else {
                strength as f32 / capacity as f32
            };
            preview.heatmap.extend(
                interaction
                    .sources
                    .iter()
                    .map(|coordinate| (*coordinate, target)),
            );
            preview.requested_strength = strength;
            preview.eta_seconds = interaction.sources.len() as u32 / 3 + 3;
        }
        OrderMode::FrontLoadOrient { .. }
        | OrderMode::FrontLoadPreview { .. }
        | OrderMode::Submitting {
            label: "FRONT-LOAD",
        } => {
            if let Some(direction) = interaction.frontload_direction() {
                let projections: Vec<_> = interaction
                    .sources
                    .iter()
                    .map(|coordinate| (*coordinate, axial_to_plane(*coordinate).dot(direction)))
                    .collect();
                let min = projections
                    .iter()
                    .map(|(_, value)| *value)
                    .fold(f32::INFINITY, f32::min);
                let max = projections
                    .iter()
                    .map(|(_, value)| *value)
                    .fold(f32::NEG_INFINITY, f32::max);
                let span = (max - min).max(0.001);
                preview.heatmap.extend(
                    projections.into_iter().map(|(coordinate, value)| {
                        (coordinate, 0.18 + 0.78 * (value - min) / span)
                    }),
                );
                preview.requested_strength = view.selected_totals(&interaction.sources).0;
                preview.eta_seconds = interaction.sources.len() as u32 / 2 + 4;
            }
        }
        _ => {}
    }
    interaction.preview = preview;
}

fn finish_submission(
    mut updates: MessageReader<ServerUpdate>,
    mut interaction: ResMut<InteractionState>,
) {
    for update in updates.read() {
        if !matches!(interaction.mode, OrderMode::Submitting { .. }) {
            continue;
        }
        match update {
            ServerUpdate::SubmissionStarted { command_id } => {
                if interaction.submitting_command_id.is_none() {
                    interaction.submitting_command_id = Some(*command_id);
                }
            }
            ServerUpdate::Accepted { command_id, .. }
                if submission_matches(&interaction, *command_id) =>
            {
                interaction.mode = OrderMode::Idle;
                interaction.destinations.clear();
                interaction.preview = OrderPreview::default();
                interaction.return_after_rejection = None;
                interaction.submitting_command_id = None;
            }
            ServerUpdate::Rejected { command_id, .. }
                if submission_matches(&interaction, *command_id) =>
            {
                interaction.mode = interaction
                    .return_after_rejection
                    .take()
                    .unwrap_or(OrderMode::Idle);
                interaction.submitting_command_id = None;
            }
            ServerUpdate::Accepted { .. }
            | ServerUpdate::Rejected { .. }
            | ServerUpdate::MobilizationChanged { .. } => {}
        }
    }
}

fn submission_matches(interaction: &InteractionState, command_id: Option<u64>) -> bool {
    match command_id {
        None => true,
        Some(command_id) => interaction.submitting_command_id == Some(command_id),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn submission_feedback_requires_the_active_online_command() {
        let interaction = InteractionState {
            mode: OrderMode::Submitting { label: "TRANSFER" },
            submitting_command_id: Some(41),
            ..Default::default()
        };

        assert!(submission_matches(&interaction, Some(41)));
        assert!(!submission_matches(&interaction, Some(42)));
        assert!(submission_matches(&interaction, None));
    }
}
