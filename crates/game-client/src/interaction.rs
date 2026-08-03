use std::collections::{BTreeMap, BTreeSet, VecDeque};

use bevy::{picking::pointer::PointerInteraction, prelude::*};
use hex_core::Axial;

use crate::{
    camera::GameCamera,
    geometry::axial_to_plane,
    model::{
        CellView, MatchView, ToastKind, reachability_from_sources, reachability_to_destinations,
    },
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
    Submitting { _label: &'static str },
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

const MAX_BRUSH_SIZE: u8 = 31;

/// A map-aligned, odd-sized rectangular paint footprint.
///
/// Axial coordinates are converted through odd-q offset rows so `width` and
/// `height` look like horizontal columns and vertical rows instead of a skewed
/// axial parallelogram.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SelectionBrush {
    width: u8,
    height: u8,
}

impl Default for SelectionBrush {
    fn default() -> Self {
        Self {
            width: 1,
            height: 1,
        }
    }
}

impl SelectionBrush {
    pub const fn width(self) -> u8 {
        self.width
    }

    pub const fn height(self) -> u8 {
        self.height
    }

    pub fn cells(self, center: Axial) -> Vec<Axial> {
        let width = self.width();
        let height = self.height();
        let half_width = i32::from(width / 2);
        let half_height = i32::from(height / 2);
        let center_row = axial_to_offset_row(center);
        let mut cells = Vec::with_capacity(usize::from(width) * usize::from(height));

        for q in (center.q - half_width)..=(center.q + half_width) {
            for row in (center_row - half_height)..=(center_row + half_height) {
                cells.push(offset_to_axial(q, row));
            }
        }
        cells
    }

    fn resize(&mut self, axis: BrushAxis, grow: bool) {
        let resize_dimension = |dimension: &mut u8| {
            *dimension = if grow {
                dimension.saturating_add(2).min(MAX_BRUSH_SIZE)
            } else {
                dimension.saturating_sub(2).max(1)
            };
        };
        match axis {
            BrushAxis::Width => resize_dimension(&mut self.width),
            BrushAxis::Height => resize_dimension(&mut self.height),
            BrushAxis::Both => {
                resize_dimension(&mut self.width);
                resize_dimension(&mut self.height);
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BrushAxis {
    Width,
    Height,
    Both,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SelectionCombine {
    Replace,
    Add,
    Remove,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PreviewModeKey {
    Idle,
    Transfer,
    Balance,
    FrontLoad { direction: Option<(u32, u32)> },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct OrderPreviewKey {
    source_revision: u64,
    destination_revision: u64,
    amount_percent: u8,
    mode: PreviewModeKey,
    cell_state_revision: u64,
    topology_revision: u64,
}

fn axial_to_offset_row(coordinate: Axial) -> i32 {
    coordinate.r + (coordinate.q - coordinate.q.rem_euclid(2)) / 2
}

fn offset_to_axial(q: i32, row: i32) -> Axial {
    Axial::new(q, row - (q - q.rem_euclid(2)) / 2)
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
    pub brush: SelectionBrush,
    pub source_revision: u64,
    pub destination_revision: u64,
    stroke: Option<PaintStroke>,
    return_after_rejection: Option<OrderMode>,
    submitting_command_id: Option<u64>,
    preview_key: Option<OrderPreviewKey>,
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
            brush: SelectionBrush::default(),
            source_revision: 0,
            destination_revision: 0,
            stroke: None,
            return_after_rejection: None,
            submitting_command_id: None,
            preview_key: None,
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

    fn clear_destinations(&mut self) {
        if !self.destinations.is_empty() {
            self.destinations.clear();
            self.destination_revision = self.destination_revision.wrapping_add(1);
        }
    }

    fn insert_destination(&mut self, coordinate: Axial) {
        if self.destinations.insert(coordinate) {
            self.destination_revision = self.destination_revision.wrapping_add(1);
        }
    }

    fn remove_destination(&mut self, coordinate: Axial) {
        if self.destinations.remove(&coordinate) {
            self.destination_revision = self.destination_revision.wrapping_add(1);
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

    if matches!(interaction.mode, OrderMode::Idle) {
        if command_modifier_pressed(&keyboard) && keyboard.just_pressed(KeyCode::KeyA) {
            let selected = all_local_owned_cells(&view);
            if interaction.sources != selected {
                interaction.sources = selected;
                interaction.source_revision = interaction.source_revision.wrapping_add(1);
            }
        } else if keyboard.just_pressed(KeyCode::KeyC)
            && let Some(seed) = interaction.hovered
        {
            let cluster = local_owned_cluster(&view, seed);
            if !cluster.is_empty()
                && combine_selection(
                    &mut interaction.sources,
                    cluster,
                    selection_combine(&keyboard),
                )
            {
                interaction.source_revision = interaction.source_revision.wrapping_add(1);
            }
        }
    }

    let brush_resize = requested_brush_resize(&keyboard, &interaction.mode);
    if let Some((axis, grow)) = brush_resize {
        interaction.brush.resize(axis, grow);
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
    if keyboard.just_pressed(KeyCode::BracketLeft) && brush_resize.is_none() {
        requested_actions.push(UiAction::AmountDown);
    }
    if keyboard.just_pressed(KeyCode::BracketRight) && brush_resize.is_none() {
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

fn shift_pressed(keyboard: &ButtonInput<KeyCode>) -> bool {
    keyboard.any_pressed([KeyCode::ShiftLeft, KeyCode::ShiftRight])
}

fn control_pressed(keyboard: &ButtonInput<KeyCode>) -> bool {
    keyboard.any_pressed([KeyCode::ControlLeft, KeyCode::ControlRight])
}

fn command_modifier_pressed(keyboard: &ButtonInput<KeyCode>) -> bool {
    control_pressed(keyboard) || keyboard.any_pressed([KeyCode::SuperLeft, KeyCode::SuperRight])
}

fn selection_combine(keyboard: &ButtonInput<KeyCode>) -> SelectionCombine {
    if control_pressed(keyboard) {
        SelectionCombine::Remove
    } else if shift_pressed(keyboard) {
        SelectionCombine::Add
    } else {
        SelectionCombine::Replace
    }
}

fn requested_brush_resize(
    keyboard: &ButtonInput<KeyCode>,
    mode: &OrderMode,
) -> Option<(BrushAxis, bool)> {
    let grow = if keyboard.just_pressed(KeyCode::BracketRight) {
        true
    } else if keyboard.just_pressed(KeyCode::BracketLeft) {
        false
    } else {
        return None;
    };
    let shift = shift_pressed(keyboard);
    let control = control_pressed(keyboard);
    let axis = match (shift, control) {
        (true, true) => BrushAxis::Both,
        (true, false) => BrushAxis::Width,
        (false, true) => BrushAxis::Height,
        (false, false) if matches!(mode, OrderMode::Idle) => BrushAxis::Both,
        (false, false) => return None,
    };
    matches!(mode, OrderMode::Idle | OrderMode::Transfer).then_some((axis, grow))
}

fn all_local_owned_cells(view: &MatchView) -> BTreeSet<Axial> {
    view.cells
        .iter()
        .filter_map(|(coordinate, cell)| {
            (cell.owner == Some(view.local_player)).then_some(*coordinate)
        })
        .collect()
}

fn local_owned_cluster(view: &MatchView, seed: Axial) -> BTreeSet<Axial> {
    if !view.is_local_owned(seed) {
        return BTreeSet::new();
    }

    let mut cluster = BTreeSet::from([seed]);
    let mut frontier = VecDeque::from([seed]);
    while let Some(coordinate) = frontier.pop_front() {
        for neighbor in coordinate.neighbors() {
            if view.is_local_owned(neighbor) && cluster.insert(neighbor) {
                frontier.push_back(neighbor);
            }
        }
    }
    cluster
}

fn combine_selection(
    selection: &mut BTreeSet<Axial>,
    cells: BTreeSet<Axial>,
    combine: SelectionCombine,
) -> bool {
    let previous_len = selection.len();
    match combine {
        SelectionCombine::Replace => {
            if *selection == cells {
                return false;
            }
            *selection = cells;
            return true;
        }
        SelectionCombine::Add => selection.extend(cells),
        SelectionCombine::Remove => selection.retain(|coordinate| !cells.contains(coordinate)),
    }
    selection.len() != previous_len
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
                interaction.clear_destinations();
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
                if !interaction.sources.is_empty() {
                    interaction.sources.clear();
                    interaction.source_revision = interaction.source_revision.wrapping_add(1);
                }
            } else if !matches!(interaction.mode, OrderMode::Submitting { .. }) {
                interaction.clear_destinations();
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
    interaction.mode = OrderMode::Submitting { _label: label };
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
        let combine = selection_combine(&keyboard);
        if combine == SelectionCombine::Replace {
            if matches!(interaction.mode, OrderMode::Transfer) {
                interaction.clear_destinations();
            } else if !interaction.sources.is_empty() {
                interaction.sources.clear();
                interaction.source_revision = interaction.source_revision.wrapping_add(1);
            }
        }
        interaction.stroke = Some(PaintStroke {
            operation: if combine == SelectionCombine::Remove {
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
        let footprint = interaction.brush.cells(coordinate);

        if matches!(interaction.mode, OrderMode::Transfer) {
            for coordinate in footprint
                .into_iter()
                .filter(|coordinate| view.cell(*coordinate).is_some_and(CellView::is_land))
            {
                match stroke.operation {
                    PaintOperation::Add => {
                        interaction.insert_destination(coordinate);
                    }
                    PaintOperation::Remove => {
                        interaction.remove_destination(coordinate);
                    }
                }
            }
        } else {
            for coordinate in footprint
                .into_iter()
                .filter(|coordinate| view.is_local_owned(*coordinate))
            {
                match stroke.operation {
                    PaintOperation::Add => {
                        if interaction.sources.insert(coordinate) {
                            interaction.source_revision =
                                interaction.source_revision.wrapping_add(1);
                        }
                    }
                    PaintOperation::Remove => {
                        if interaction.sources.remove(&coordinate) {
                            interaction.source_revision =
                                interaction.source_revision.wrapping_add(1);
                        }
                    }
                }
            }
        }
    }

    if mouse.just_released(MouseButton::Left) {
        interaction.stroke = None;
    }
}

fn order_preview_key(view: &MatchView, interaction: &InteractionState) -> Option<OrderPreviewKey> {
    let mode = match &interaction.mode {
        OrderMode::Idle => PreviewModeKey::Idle,
        OrderMode::Transfer => PreviewModeKey::Transfer,
        OrderMode::BalancePreview => PreviewModeKey::Balance,
        OrderMode::FrontLoadOrient { .. } | OrderMode::FrontLoadPreview { .. } => {
            PreviewModeKey::FrontLoad {
                direction: interaction
                    .frontload_direction()
                    .map(|value| (value.x.to_bits(), value.y.to_bits())),
            }
        }
        OrderMode::Submitting { .. } => return None,
    };
    Some(OrderPreviewKey {
        source_revision: interaction.source_revision,
        destination_revision: interaction.destination_revision,
        amount_percent: interaction.amount_percent,
        mode,
        cell_state_revision: view.cell_state_revision,
        topology_revision: view.chunk_index_revision,
    })
}

fn update_order_preview(view: Res<MatchView>, mut interaction: ResMut<InteractionState>) {
    let Some(key) = order_preview_key(&view, &interaction) else {
        // Keep the accepted preview stable while an authoritative submission is
        // pending. Rejection restores its prior mode and key inputs.
        return;
    };
    if interaction.preview_key == Some(key) {
        return;
    }

    let mut preview = OrderPreview::default();
    match &interaction.mode {
        OrderMode::Transfer => {
            let sources = interaction
                .sources
                .iter()
                .filter(|coordinate| view.is_local_owned(**coordinate))
                .copied()
                .collect::<BTreeSet<_>>();
            let destinations = interaction
                .destinations
                .iter()
                .filter(|coordinate| {
                    !sources.contains(coordinate)
                        && view
                            .cell(**coordinate)
                            .is_some_and(|cell| cell.is_land() && !cell.blocked)
                })
                .copied()
                .collect::<BTreeSet<_>>();

            if sources.is_empty() || destinations.is_empty() {
                preview
                    .excluded
                    .extend(interaction.destinations.iter().copied());
            } else {
                let forward = reachability_from_sources(&view, &sources);
                preview.excluded.extend(
                    interaction
                        .destinations
                        .iter()
                        .filter(|coordinate| {
                            !destinations.contains(coordinate) || !forward.contains(**coordinate)
                        })
                        .copied(),
                );
                let reachable_destinations = destinations
                    .iter()
                    .filter(|coordinate| forward.contains(**coordinate))
                    .copied()
                    .collect::<BTreeSet<_>>();
                let reverse = reachability_to_destinations(&view, &reachable_destinations);
                let reachable_sources = reverse.reachable_sources(&sources);

                preview.route = forward
                    .route_to_any(&reachable_destinations)
                    .unwrap_or_default();
                preview.requested_strength = reachable_sources
                    .iter()
                    .filter_map(|coordinate| view.cell(*coordinate))
                    .map(|cell| cell.infantry * u64::from(interaction.amount_percent) / 100)
                    .sum();
                preview.destination_capacity = reachable_destinations
                    .iter()
                    .filter_map(|coordinate| view.cell(*coordinate))
                    .map(CellView::free_capacity)
                    .sum();
                if preview.route.len() >= 2 {
                    preview.bottleneck = preview
                        .route
                        .windows(2)
                        .min_by_key(|edge| {
                            view.cell(edge[1]).map_or(0, |cell| cell.military_capacity)
                        })
                        .map(|edge| (edge[0], edge[1]));
                    let congestion = preview
                        .requested_strength
                        .saturating_sub(preview.destination_capacity)
                        / 20;
                    preview.eta_seconds = (preview.route.len() as u64 * 2 + congestion)
                        .min(u64::from(u32::MAX)) as u32;
                }
            }
        }
        OrderMode::BalancePreview => {
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
        OrderMode::FrontLoadOrient { .. } | OrderMode::FrontLoadPreview { .. } => {
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
        OrderMode::Idle => {}
        OrderMode::Submitting { .. } => unreachable!("submitting previews return before rebuild"),
    }
    interaction.preview = preview;
    interaction.preview_key = Some(key);
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
                interaction.clear_destinations();
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
    use hex_core::TerrainKind;

    fn preview_cell(coordinate: Axial, infantry: u64) -> CellView {
        CellView {
            coordinate,
            terrain: TerrainKind::Plains,
            elevation: 0,
            owner: Some(1),
            civilians: 0,
            infantry,
            military_capacity: 100,
            blocked: false,
        }
    }

    fn disconnected_preview_view() -> MatchView {
        let mut view = MatchView::connecting(1);
        for cell in [
            preview_cell(Axial::ZERO, 10),
            preview_cell(Axial::new(10, 0), 20),
            preview_cell(Axial::new(11, 0), 0),
            preview_cell(Axial::new(20, 0), 0),
        ] {
            view.cells.insert(cell.coordinate, cell);
        }
        view.rebuild_chunk_index();
        view
    }

    fn pressed(keys: impl IntoIterator<Item = KeyCode>) -> ButtonInput<KeyCode> {
        let mut input = ButtonInput::default();
        for key in keys {
            input.press(key);
        }
        input
    }

    #[test]
    fn brush_uses_odd_q_columns_and_rows_for_visual_dimensions() {
        let brush = SelectionBrush {
            width: 3,
            height: 5,
        };
        let center = Axial::new(-4, 7);
        let cells = brush.cells(center).into_iter().collect::<BTreeSet<_>>();
        let columns = cells.iter().map(|coordinate| coordinate.q);
        let rows = cells
            .iter()
            .map(|coordinate| axial_to_offset_row(*coordinate));

        assert_eq!(cells.len(), 15);
        assert_eq!(columns.clone().min(), Some(center.q - 1));
        assert_eq!(columns.max(), Some(center.q + 1));
        assert_eq!(rows.clone().min(), Some(axial_to_offset_row(center) - 2));
        assert_eq!(rows.max(), Some(axial_to_offset_row(center) + 2));
        assert!(cells.contains(&center));
    }

    #[test]
    fn brush_resize_is_odd_bounded_and_modifier_aware() {
        let idle = OrderMode::Idle;
        let transfer = OrderMode::Transfer;

        assert_eq!(
            requested_brush_resize(&pressed([KeyCode::BracketRight]), &idle),
            Some((BrushAxis::Both, true))
        );
        assert_eq!(
            requested_brush_resize(&pressed([KeyCode::ShiftLeft, KeyCode::BracketRight]), &idle),
            Some((BrushAxis::Width, true))
        );
        assert_eq!(
            requested_brush_resize(
                &pressed([KeyCode::ControlLeft, KeyCode::BracketLeft]),
                &transfer
            ),
            Some((BrushAxis::Height, false))
        );
        assert_eq!(
            requested_brush_resize(&pressed([KeyCode::BracketRight]), &transfer),
            None
        );

        let mut brush = SelectionBrush::default();
        for _ in 0..20 {
            brush.resize(BrushAxis::Both, true);
        }
        assert_eq!((brush.width(), brush.height()), (31, 31));
        for _ in 0..20 {
            brush.resize(BrushAxis::Both, false);
        }
        assert_eq!((brush.width(), brush.height()), (1, 1));
    }

    #[test]
    fn cluster_selection_follows_six_connected_local_ownership() {
        let mut view = MatchView::offline_fixture();
        let seed = Axial::ZERO;
        let connected = Axial::new(1, 0);
        let disconnected = Axial::new(3, 0);
        for coordinate in [seed, connected, disconnected] {
            view.cell_mut(coordinate).expect("fixture cell").owner = Some(view.local_player);
        }

        let cluster = local_owned_cluster(&view, seed);
        assert_eq!(cluster, BTreeSet::from([seed, connected]));
        assert!(!cluster.contains(&disconnected));
    }

    #[test]
    fn select_all_contains_only_local_owned_cells() {
        let view = MatchView::offline_fixture();
        let selected = all_local_owned_cells(&view);

        assert!(!selected.is_empty());
        assert!(
            selected
                .iter()
                .all(|coordinate| view.is_local_owned(*coordinate))
        );
        assert_eq!(
            selected.len(),
            view.cells
                .values()
                .filter(|cell| cell.owner == Some(view.local_player))
                .count()
        );
    }

    #[test]
    fn destination_mutations_advance_the_preview_revision_only_when_state_changes() {
        let mut interaction = InteractionState::default();
        let destination = Axial::new(2, -1);

        interaction.insert_destination(destination);
        assert_eq!(interaction.destination_revision, 1);
        interaction.insert_destination(destination);
        assert_eq!(interaction.destination_revision, 1);

        interaction.remove_destination(Axial::new(9, 9));
        assert_eq!(interaction.destination_revision, 1);
        interaction.remove_destination(destination);
        assert_eq!(interaction.destination_revision, 2);

        interaction.insert_destination(destination);
        interaction.clear_destinations();
        assert_eq!(interaction.destination_revision, 4);
        interaction.clear_destinations();
        assert_eq!(interaction.destination_revision, 4);
    }

    #[test]
    fn preview_cache_key_tracks_revisions_amount_mode_and_direction() {
        let mut view = MatchView::connecting(1);
        let mut interaction = InteractionState {
            mode: OrderMode::Transfer,
            ..Default::default()
        };
        let mut prior = order_preview_key(&view, &interaction).expect("transfer key");

        interaction.source_revision = interaction.source_revision.wrapping_add(1);
        let current = order_preview_key(&view, &interaction).expect("source key");
        assert_ne!(current, prior);
        prior = current;

        interaction.destination_revision = interaction.destination_revision.wrapping_add(1);
        let current = order_preview_key(&view, &interaction).expect("destination key");
        assert_ne!(current, prior);
        prior = current;

        interaction.amount_percent = 70;
        let current = order_preview_key(&view, &interaction).expect("amount key");
        assert_ne!(current, prior);
        prior = current;

        interaction.mode = OrderMode::FrontLoadPreview { direction: Vec2::X };
        let current = order_preview_key(&view, &interaction).expect("mode key");
        assert_ne!(current, prior);
        prior = current;

        interaction.mode = OrderMode::FrontLoadPreview { direction: Vec2::Y };
        let current = order_preview_key(&view, &interaction).expect("direction key");
        assert_ne!(current, prior);
        prior = current;

        view.cell_state_revision = view.cell_state_revision.wrapping_add(1);
        let current = order_preview_key(&view, &interaction).expect("cell-state key");
        assert_ne!(current, prior);
        prior = current;

        view.chunk_index_revision = view.chunk_index_revision.wrapping_add(1);
        assert_ne!(
            order_preview_key(&view, &interaction).expect("topology key"),
            prior
        );
    }

    #[test]
    fn transfer_preview_is_cached_and_counts_only_sources_reaching_a_destination() {
        let isolated_source = Axial::ZERO;
        let reachable_source = Axial::new(10, 0);
        let reachable_destination = Axial::new(11, 0);
        let excluded_destination = Axial::new(20, 0);
        let mut interaction = InteractionState {
            sources: BTreeSet::from([isolated_source, reachable_source]),
            mode: OrderMode::Transfer,
            amount_percent: 100,
            source_revision: 1,
            ..Default::default()
        };
        interaction.insert_destination(reachable_destination);
        interaction.insert_destination(excluded_destination);

        let mut app = App::new();
        app.insert_resource(disconnected_preview_view())
            .insert_resource(interaction)
            .add_systems(Update, update_order_preview);
        app.update();

        let preview = &app.world().resource::<InteractionState>().preview;
        assert_eq!(preview.route, vec![reachable_source, reachable_destination]);
        assert_eq!(preview.requested_strength, 20);
        assert_eq!(preview.destination_capacity, 100);
        assert_eq!(preview.excluded, BTreeSet::from([excluded_destination]));

        app.world_mut()
            .resource_mut::<InteractionState>()
            .preview
            .requested_strength = 999;
        app.update();
        assert_eq!(
            app.world()
                .resource::<InteractionState>()
                .preview
                .requested_strength,
            999,
            "an unchanged cache key must skip the full preview rebuild"
        );

        app.world_mut()
            .resource_mut::<InteractionState>()
            .amount_percent = 50;
        app.update();
        assert_eq!(
            app.world()
                .resource::<InteractionState>()
                .preview
                .requested_strength,
            10
        );
    }

    #[test]
    fn submission_feedback_requires_the_active_online_command() {
        let interaction = InteractionState {
            mode: OrderMode::Submitting { _label: "TRANSFER" },
            submitting_command_id: Some(41),
            ..Default::default()
        };

        assert!(submission_matches(&interaction, Some(41)));
        assert!(!submission_matches(&interaction, Some(42)));
        assert!(submission_matches(&interaction, None));
    }
}
