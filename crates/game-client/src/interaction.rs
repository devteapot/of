use std::collections::{BTreeMap, BTreeSet, VecDeque};

use bevy::{picking::pointer::PointerInteraction, prelude::*};
use hex_core::{
    Axial, Cell, DirectedFrontEdge, DistributionPreset as CoreDistributionPreset, ForceComposition,
    FrontSelectionError, HexMap, redistribution_targets_with_commitment, selected_front_edges,
};

use crate::{
    camera::GameCamera,
    geometry::axial_to_plane,
    model::{MatchView, ToastKind},
    network::{
        ClientIntent, NetworkSet, RedistributionPreset, ServerUpdate, expand_all_front_edges,
    },
    terrain::TerrainChunk,
};

#[derive(Clone, Debug, Default)]
pub struct OrderPreview {
    pub route: Vec<Axial>,
    pub front_edges: Vec<DirectedFrontEdge>,
    pub excluded: BTreeSet<Axial>,
    pub heatmap: BTreeMap<Axial, f32>,
    pub eta_seconds: u32,
    /// Upper-bound estimate from visible infantry. Active authoritative packet
    /// allocations are not represented in `MatchView`, so the accepted order
    /// may dispatch less than this value.
    pub strength_upper_bound: u64,
    pub destination_capacity: u64,
    pub bottleneck: Option<(Axial, Axial)>,
    pub invalid_reason: Option<&'static str>,
}

#[derive(Clone, Debug)]
pub enum OrderMode {
    Idle,
    PushFrontOrient { start: Vec3, current: Vec3 },
    PushFrontPreview { direction: Axial },
    ExpandAllPreview,
    BalancePreview,
    FrontLoadOrient { start: Vec3, current: Vec3 },
    FrontLoadPreview { direction: Vec2 },
    CoreLoadPreview,
    PerimeterLoadPreview,
    Submitting { _label: &'static str },
}

impl OrderMode {
    pub const fn label(&self) -> &'static str {
        match self {
            Self::Idle => "SOURCE SELECTION",
            Self::PushFrontOrient { .. } => "PUSH FRONT · ORIENT",
            Self::PushFrontPreview { .. } => "PUSH FRONT PREVIEW",
            Self::ExpandAllPreview => "EXPAND ALL PREVIEW",
            Self::BalancePreview => "BALANCE PREVIEW",
            Self::FrontLoadOrient { .. } => "FRONT-LOAD · ORIENT",
            Self::FrontLoadPreview { .. } => "FRONT-LOAD PREVIEW",
            Self::CoreLoadPreview => "CORE-LOAD PREVIEW",
            Self::PerimeterLoadPreview => "PERIMETER-LOAD PREVIEW",
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
const MAX_COMMAND_SELECTION_CELLS: usize = 4_096;

/// A map-aligned paint footprint with independent rectangular extents and hex rings.
///
/// Axial coordinates are converted through odd-q offset rows so the rectangular
/// core looks like horizontal columns and vertical rows instead of a skewed
/// axial parallelogram. Plain bracket resizing dilates that core with complete
/// six-neighbor rings, so growing both axes adds exactly one cell around the
/// entire perimeter instead of producing a larger offset-coordinate rectangle.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SelectionBrush {
    core_width: u8,
    core_height: u8,
    rings: u8,
}

impl Default for SelectionBrush {
    fn default() -> Self {
        Self {
            core_width: 1,
            core_height: 1,
            rings: 0,
        }
    }
}

impl SelectionBrush {
    pub const fn width(self) -> u8 {
        self.core_width + self.rings * 2
    }

    pub const fn height(self) -> u8 {
        self.core_height + self.rings * 2
    }

    pub const fn rings(self) -> u8 {
        self.rings
    }

    pub fn cells(self, center: Axial) -> Vec<Axial> {
        let width = self.core_width;
        let height = self.core_height;
        let half_width = i32::from(width / 2);
        let half_height = i32::from(height / 2);
        let center_row = axial_to_offset_row(center);
        let mut cells = BTreeSet::new();

        for q in (center.q - half_width)..=(center.q + half_width) {
            for row in (center_row - half_height)..=(center_row + half_height) {
                cells.insert(offset_to_axial(q, row));
            }
        }

        for _ in 0..self.rings {
            let perimeter = cells
                .iter()
                .flat_map(|coordinate| coordinate.neighbors())
                .collect::<Vec<_>>();
            cells.extend(perimeter);
        }

        cells.into_iter().collect()
    }

    fn resize(&mut self, axis: BrushAxis, grow: bool) {
        let max_core_dimension = MAX_BRUSH_SIZE.saturating_sub(self.rings * 2);
        let resize_dimension = |dimension: &mut u8| {
            *dimension = if grow {
                dimension.saturating_add(2).min(max_core_dimension)
            } else {
                dimension.saturating_sub(2).max(1)
            };
        };
        match axis {
            BrushAxis::Width => resize_dimension(&mut self.core_width),
            BrushAxis::Height => resize_dimension(&mut self.core_height),
            BrushAxis::Both => {
                if grow {
                    if self.width() < MAX_BRUSH_SIZE && self.height() < MAX_BRUSH_SIZE {
                        self.rings += 1;
                    }
                } else if self.rings > 0 {
                    self.rings -= 1;
                } else {
                    resize_dimension(&mut self.core_width);
                    resize_dimension(&mut self.core_height);
                }
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
    PushFront { direction: Option<Axial> },
    ExpandAll,
    Balance,
    FrontLoad { direction: Option<(u32, u32)> },
    CoreLoad,
    PerimeterLoad,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct OrderPreviewKey {
    source_revision: u64,
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

fn hex_line_between(start: Axial, end: Axial) -> Vec<Axial> {
    let steps = start.distance(end);
    if steps == 0 {
        return vec![start];
    }
    let start = start.cube();
    let end = end.cube();
    (0..=steps)
        .map(|step| {
            let t = step as f64 / steps as f64;
            let x = start.x as f64 + (end.x - start.x) as f64 * t;
            let y = start.y as f64 + (end.y - start.y) as f64 * t;
            let z = start.z as f64 + (end.z - start.z) as f64 * t;
            let mut rounded_x = x.round();
            let mut rounded_y = y.round();
            let mut rounded_z = z.round();
            let x_error = (rounded_x - x).abs();
            let y_error = (rounded_y - y).abs();
            let z_error = (rounded_z - z).abs();
            if x_error > y_error && x_error > z_error {
                rounded_x = -rounded_y - rounded_z;
            } else if y_error > z_error {
                rounded_y = -rounded_x - rounded_z;
            } else {
                rounded_z = -rounded_x - rounded_y;
            }
            debug_assert!((rounded_x + rounded_y + rounded_z).abs() < f64::EPSILON);
            Axial::new(rounded_x as i32, rounded_z as i32)
        })
        .collect()
}

fn quantize_world_direction(direction: Vec2) -> Option<Axial> {
    if direction.length() < 0.35 {
        return None;
    }
    let direction = direction.normalize();
    Axial::DIRECTIONS.into_iter().max_by(|left, right| {
        axial_to_plane(*left)
            .normalize()
            .dot(direction)
            .total_cmp(&axial_to_plane(*right).normalize().dot(direction))
    })
}

#[derive(Resource, Debug)]
pub struct InteractionState {
    pub hovered: Option<Axial>,
    pub cursor_world: Option<Vec3>,
    pub sources: BTreeSet<Axial>,
    pub mode: OrderMode,
    pub amount_percent: u8,
    pub preview: OrderPreview,
    pub show_help: bool,
    pub brush: SelectionBrush,
    pub source_revision: u64,
    stroke: Option<PaintStroke>,
    orientation_uses_click: bool,
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
            mode: OrderMode::Idle,
            amount_percent: 50,
            preview: OrderPreview::default(),
            show_help: false,
            brush: SelectionBrush::default(),
            source_revision: 0,
            stroke: None,
            orientation_uses_click: false,
            return_after_rejection: None,
            submitting_command_id: None,
            preview_key: None,
        }
    }
}

impl InteractionState {
    pub fn push_direction(&self) -> Option<Axial> {
        match self.mode {
            OrderMode::PushFrontOrient { start, current } => {
                quantize_world_direction(Vec2::new(current.x - start.x, current.z - start.z))
            }
            OrderMode::PushFrontPreview { direction } => Some(direction),
            _ => None,
        }
    }

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
    PushFront,
    PushFrontKey,
    ExpandAll,
    Balance,
    FrontLoad,
    FrontLoadKey,
    CoreLoad,
    PerimeterLoad,
    CancelPush,
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
    mouse: Res<ButtonInput<MouseButton>>,
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
    if keyboard.just_pressed(KeyCode::KeyP) {
        requested_actions.push(if shift_pressed(&keyboard) {
            UiAction::ExpandAll
        } else {
            UiAction::PushFrontKey
        });
    }
    if keyboard.just_pressed(KeyCode::KeyB) {
        requested_actions.push(UiAction::Balance);
    }
    if keyboard.just_pressed(KeyCode::KeyG) {
        requested_actions.push(UiAction::CoreLoad);
    }
    if keyboard.just_pressed(KeyCode::KeyR) {
        requested_actions.push(UiAction::PerimeterLoad);
    }
    if keyboard.just_pressed(KeyCode::KeyX) {
        requested_actions.push(UiAction::CancelPush);
    }
    if keyboard.just_pressed(KeyCode::Enter) {
        requested_actions.push(UiAction::Confirm);
    }
    if keyboard.just_pressed(KeyCode::Escape) {
        requested_actions.push(UiAction::Cancel);
    }
    let plain_brackets = !shift_pressed(&keyboard) && !control_pressed(&keyboard);
    if keyboard.just_pressed(KeyCode::BracketLeft) && brush_resize.is_none() && plain_brackets {
        requested_actions.push(UiAction::AmountDown);
    }
    if keyboard.just_pressed(KeyCode::BracketRight) && brush_resize.is_none() && plain_brackets {
        requested_actions.push(UiAction::AmountUp);
    }
    requested_actions.extend(actions.read().copied());

    if keyboard.just_pressed(KeyCode::KeyF) {
        requested_actions.push(UiAction::FrontLoadKey);
    }

    for action in requested_actions {
        handle_action(action, &mut interaction, &mut view, &mut intents);
    }

    let cursor_world = interaction.cursor_world;
    let orientation_uses_click = interaction.orientation_uses_click;
    let pointer_is_over_map = interaction.hovered.is_some();
    match &mut interaction.mode {
        OrderMode::PushFrontOrient { start, current } => {
            if let Some(cursor) = cursor_world {
                *current = cursor;
            }
            let finish_with_pointer = orientation_uses_click
                && mouse.just_pressed(MouseButton::Left)
                && pointer_is_over_map;
            let finish_with_key = !orientation_uses_click && keyboard.just_released(KeyCode::KeyP);
            if finish_with_pointer || finish_with_key {
                let direction = Vec2::new(current.x - start.x, current.z - start.z);
                if let Some(direction) = quantize_world_direction(direction) {
                    interaction.mode = OrderMode::PushFrontPreview { direction };
                } else {
                    interaction.mode = OrderMode::Idle;
                    view.show_toast(
                        "Push Front needs a visible outward drag",
                        ToastKind::Rejection,
                    );
                }
            }
        }
        OrderMode::FrontLoadOrient { start, current } => {
            if let Some(cursor) = cursor_world {
                *current = cursor;
            }
            let finish_with_pointer = orientation_uses_click
                && mouse.just_pressed(MouseButton::Left)
                && pointer_is_over_map;
            let finish_with_key = !orientation_uses_click && keyboard.just_released(KeyCode::KeyF);
            if finish_with_pointer || finish_with_key {
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
        _ => {}
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
    matches!(mode, OrderMode::Idle).then_some((axis, grow))
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
        action @ (UiAction::PushFront | UiAction::PushFrontKey) => {
            if interaction.sources.is_empty() {
                view.show_toast("Paint owned source hexes first", ToastKind::Rejection);
            } else if matches!(interaction.mode, OrderMode::Idle) {
                if let Some(start) = interaction.cursor_world {
                    interaction.orientation_uses_click = matches!(action, UiAction::PushFront);
                    interaction.mode = OrderMode::PushFrontOrient {
                        start,
                        current: start,
                    };
                    let hint = if interaction.orientation_uses_click {
                        "Move onto the map and click outward from the selected front"
                    } else {
                        "Keep P held and drag outward from the selected front"
                    };
                    view.show_toast(hint, ToastKind::Info);
                } else {
                    view.show_toast(
                        "Point at the map before orienting Push Front",
                        ToastKind::Rejection,
                    );
                }
            }
        }
        UiAction::ExpandAll => {
            if interaction.sources.is_empty() {
                view.show_toast("Paint owned source hexes first", ToastKind::Rejection);
            } else if matches!(interaction.mode, OrderMode::Idle) {
                interaction.mode = OrderMode::ExpandAllPreview;
                view.show_toast(
                    "Expand All previews every neutral frontier · [/] adjusts dispatch request",
                    ToastKind::Info,
                );
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
        UiAction::CoreLoad => {
            if interaction.sources.len() < 2 {
                view.show_toast(
                    "Core-load needs at least two owned hexes",
                    ToastKind::Rejection,
                );
            } else if matches!(interaction.mode, OrderMode::Idle) {
                interaction.mode = OrderMode::CoreLoadPreview;
            }
        }
        UiAction::PerimeterLoad => {
            if interaction.sources.len() < 2 {
                view.show_toast(
                    "Perimeter-load needs at least two owned hexes",
                    ToastKind::Rejection,
                );
            } else if matches!(interaction.mode, OrderMode::Idle) {
                interaction.mode = OrderMode::PerimeterLoadPreview;
            }
        }
        action @ (UiAction::FrontLoad | UiAction::FrontLoadKey) => {
            if interaction.sources.len() < 2 {
                view.show_toast(
                    "Front-load needs at least two owned hexes",
                    ToastKind::Rejection,
                );
            } else if matches!(interaction.mode, OrderMode::Idle) {
                if let Some(start) = interaction.cursor_world {
                    interaction.orientation_uses_click = matches!(action, UiAction::FrontLoad);
                    interaction.mode = OrderMode::FrontLoadOrient {
                        start,
                        current: start,
                    };
                    let hint = if interaction.orientation_uses_click {
                        "Move onto the map and click to orient Front-load"
                    } else {
                        "Hold F and move the pointer to orient"
                    };
                    view.show_toast(hint, ToastKind::Info);
                } else {
                    view.show_toast(
                        "Point at the map before orienting Front-load",
                        ToastKind::Rejection,
                    );
                }
            }
        }
        UiAction::Confirm => submit_current(interaction, view, intents),
        UiAction::CancelPush => cancel_matching_push(interaction, view, intents),
        UiAction::Cancel => {
            if matches!(interaction.mode, OrderMode::Idle) {
                if !interaction.sources.is_empty() {
                    interaction.sources.clear();
                    interaction.source_revision = interaction.source_revision.wrapping_add(1);
                }
            } else if !matches!(interaction.mode, OrderMode::Submitting { .. }) {
                interaction.preview = OrderPreview::default();
                interaction.mode = OrderMode::Idle;
            }
        }
        UiAction::AmountDown => {
            if is_percentage_preview(&interaction.mode) {
                interaction.amount_percent = interaction.amount_percent.saturating_sub(10).max(10);
            }
        }
        UiAction::AmountUp => {
            if is_percentage_preview(&interaction.mode) {
                interaction.amount_percent = interaction.amount_percent.saturating_add(10).min(100);
            }
        }
    }
}

fn is_percentage_preview(mode: &OrderMode) -> bool {
    matches!(
        mode,
        OrderMode::PushFrontPreview { .. }
            | OrderMode::ExpandAllPreview
            | OrderMode::BalancePreview
            | OrderMode::FrontLoadPreview { .. }
            | OrderMode::CoreLoadPreview
            | OrderMode::PerimeterLoadPreview
    )
}

fn cancel_matching_push(
    interaction: &mut InteractionState,
    view: &mut MatchView,
    intents: &mut MessageWriter<ClientIntent>,
) {
    if interaction.sources.is_empty() {
        view.show_toast(
            "Select the expansion's source region first",
            ToastKind::Rejection,
        );
        return;
    }
    let Some((intent, label, return_mode)) = cancellation_request(interaction) else {
        view.show_toast(
            "Preview a directional Push or Expand All first, then X stops matching operations",
            ToastKind::Info,
        );
        return;
    };
    interaction.return_after_rejection = Some(return_mode);
    interaction.submitting_command_id = None;
    interaction.mode = OrderMode::Submitting { _label: label };
    intents.write(intent);
}

fn cancellation_request(
    interaction: &InteractionState,
) -> Option<(ClientIntent, &'static str, OrderMode)> {
    match interaction.mode {
        OrderMode::PushFrontPreview { direction } => Some((
            ClientIntent::CancelPush {
                sources: interaction.sources.clone(),
                direction,
            },
            "STOP PUSH",
            OrderMode::PushFrontPreview { direction },
        )),
        OrderMode::ExpandAllPreview => Some((
            ClientIntent::CancelExpandAll {
                sources: interaction.sources.clone(),
            },
            "STOP EXPAND ALL",
            OrderMode::ExpandAllPreview,
        )),
        _ => None,
    }
}

fn submit_current(
    interaction: &mut InteractionState,
    view: &mut MatchView,
    intents: &mut MessageWriter<ClientIntent>,
) {
    if let Some(reason) = interaction.preview.invalid_reason {
        view.show_toast(reason, ToastKind::Rejection);
        return;
    }
    let (intent, label, return_mode) = match &interaction.mode {
        OrderMode::PushFrontPreview { direction }
            if !interaction.preview.front_edges.is_empty()
                && interaction.preview.invalid_reason.is_none() =>
        {
            (
                ClientIntent::PushFront {
                    sources: interaction.sources.clone(),
                    direction: *direction,
                    commitment_percent: interaction.amount_percent,
                },
                "PUSH FRONT",
                OrderMode::PushFrontPreview {
                    direction: *direction,
                },
            )
        }
        OrderMode::ExpandAllPreview
            if !interaction.preview.front_edges.is_empty()
                && interaction.preview.invalid_reason.is_none() =>
        {
            (
                ClientIntent::ExpandAll {
                    sources: interaction.sources.clone(),
                    commitment_percent: interaction.amount_percent,
                },
                "EXPAND ALL",
                OrderMode::ExpandAllPreview,
            )
        }
        OrderMode::BalancePreview => (
            ClientIntent::Redistribute {
                cells: interaction.sources.clone(),
                preset: RedistributionPreset::Balance,
                direction: None,
                amount_percent: interaction.amount_percent,
            },
            "BALANCE",
            OrderMode::BalancePreview,
        ),
        OrderMode::FrontLoadPreview { direction } => (
            ClientIntent::Redistribute {
                cells: interaction.sources.clone(),
                preset: RedistributionPreset::FrontLoad,
                direction: Some(*direction),
                amount_percent: interaction.amount_percent,
            },
            "FRONT-LOAD",
            OrderMode::FrontLoadPreview {
                direction: *direction,
            },
        ),
        OrderMode::CoreLoadPreview => (
            ClientIntent::Redistribute {
                cells: interaction.sources.clone(),
                preset: RedistributionPreset::CoreLoad,
                direction: None,
                amount_percent: interaction.amount_percent,
            },
            "CORE-LOAD",
            OrderMode::CoreLoadPreview,
        ),
        OrderMode::PerimeterLoadPreview => (
            ClientIntent::Redistribute {
                cells: interaction.sources.clone(),
                preset: RedistributionPreset::PerimeterLoad,
                direction: None,
                amount_percent: interaction.amount_percent,
            },
            "PERIMETER-LOAD",
            OrderMode::PerimeterLoadPreview,
        ),
        OrderMode::PushFrontPreview { .. } | OrderMode::ExpandAllPreview => {
            view.show_toast(
                interaction
                    .preview
                    .invalid_reason
                    .unwrap_or("The selection has no valid outward front"),
                ToastKind::Rejection,
            );
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
    let can_paint = matches!(interaction.mode, OrderMode::Idle)
        && !keyboard.pressed(KeyCode::Space)
        && !mouse.pressed(MouseButton::Middle);
    if !can_paint {
        interaction.stroke = None;
        return;
    }

    if mouse.just_pressed(MouseButton::Left) && interaction.hovered.is_some() {
        let combine = selection_combine(&keyboard);
        if combine == SelectionCombine::Replace && !interaction.sources.is_empty() {
            interaction.sources.clear();
            interaction.source_revision = interaction.source_revision.wrapping_add(1);
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
        let Some(mut stroke) = interaction.stroke else {
            return;
        };
        let Some(hovered) = interaction.hovered else {
            return;
        };
        if Some(hovered) == stroke.last {
            return;
        }
        let centers = stroke
            .last
            .map_or_else(|| vec![hovered], |last| hex_line_between(last, hovered));
        stroke.last = Some(hovered);
        interaction.stroke = Some(stroke);
        let mut changed = false;
        for center in centers {
            for coordinate in interaction
                .brush
                .cells(center)
                .into_iter()
                .filter(|coordinate| view.is_local_owned(*coordinate))
            {
                match stroke.operation {
                    PaintOperation::Add => {
                        changed |= interaction.sources.insert(coordinate);
                    }
                    PaintOperation::Remove => {
                        changed |= interaction.sources.remove(&coordinate);
                    }
                }
            }
        }
        if changed {
            interaction.source_revision = interaction.source_revision.wrapping_add(1);
        }
    }

    if mouse.just_released(MouseButton::Left) {
        interaction.stroke = None;
    }
}

fn order_preview_key(view: &MatchView, interaction: &InteractionState) -> Option<OrderPreviewKey> {
    let mode = match &interaction.mode {
        OrderMode::Idle => PreviewModeKey::Idle,
        OrderMode::PushFrontOrient { .. } | OrderMode::PushFrontPreview { .. } => {
            PreviewModeKey::PushFront {
                direction: interaction.push_direction(),
            }
        }
        OrderMode::ExpandAllPreview => PreviewModeKey::ExpandAll,
        OrderMode::BalancePreview => PreviewModeKey::Balance,
        OrderMode::FrontLoadOrient { .. } | OrderMode::FrontLoadPreview { .. } => {
            PreviewModeKey::FrontLoad {
                direction: interaction
                    .frontload_direction()
                    .map(|value| (value.x.to_bits(), value.y.to_bits())),
            }
        }
        OrderMode::CoreLoadPreview => PreviewModeKey::CoreLoad,
        OrderMode::PerimeterLoadPreview => PreviewModeKey::PerimeterLoad,
        OrderMode::Submitting { .. } => return None,
    };
    Some(OrderPreviewKey {
        source_revision: interaction.source_revision,
        amount_percent: interaction.amount_percent,
        mode,
        cell_state_revision: view.cell_state_revision,
        topology_revision: view.chunk_index_revision,
    })
}

fn build_push_front_preview(
    view: &MatchView,
    selected: &BTreeSet<Axial>,
    direction: Axial,
    commitment_percent: u8,
    preview: &mut OrderPreview,
) {
    if selected.len() > MAX_COMMAND_SELECTION_CELLS {
        preview.invalid_reason = Some("Push selection exceeds the 4096-cell V1 command limit");
        return;
    }
    let sources = selected
        .iter()
        .filter(|coordinate| {
            view.cell(**coordinate)
                .is_some_and(|cell| view.is_local_owned(**coordinate) && !cell.blocked)
        })
        .copied()
        .collect::<BTreeSet<_>>();
    if sources.len() != selected.len() {
        preview
            .excluded
            .extend(selected.difference(&sources).copied());
        preview.invalid_reason = Some("Every selected source must be owned passable ground");
        return;
    }

    let edges = match selected_front_edges(&sources, direction, |source, target| {
        let Some(source) = view.cell(source) else {
            return false;
        };
        view.cell(target).is_some_and(|target| {
            target.is_land()
                && !target.blocked
                && target.owner != Some(view.local_player)
                && (i32::from(source.elevation) - i32::from(target.elevation)).unsigned_abs() <= 1
        })
    }) {
        Ok(edges) => edges,
        Err(error) => {
            preview.invalid_reason = Some(front_selection_error_text(error));
            return;
        }
    };

    let front_sources = edges
        .iter()
        .map(|edge| edge.source)
        .collect::<BTreeSet<_>>();
    let (next, distance) = selected_reachability_to_front(view, &sources, &front_sources);
    if next.len() != sources.len() {
        preview.excluded.extend(
            sources
                .iter()
                .filter(|source| !next.contains_key(source))
                .copied(),
        );
        preview.invalid_reason = Some("Selected troops must reach the front inside the selection");
        return;
    }

    preview.front_edges = edges;
    let percentage = u64::from(commitment_percent.clamp(10, 100));
    preview.strength_upper_bound = sources
        .iter()
        .filter_map(|coordinate| view.cell(*coordinate))
        .map(|cell| cell.infantry.saturating_mul(percentage) / 100)
        .fold(0_u64, u64::saturating_add);
    if preview.strength_upper_bound == 0 {
        preview.invalid_reason = Some("Selected sources have no visible infantry to request");
        return;
    }
    let targets = preview
        .front_edges
        .iter()
        .map(|edge| edge.target)
        .collect::<BTreeSet<_>>();
    preview.destination_capacity = targets
        .iter()
        .filter_map(|coordinate| view.cell(*coordinate))
        .map(|cell| cell.military_capacity)
        .fold(0_u64, u64::saturating_add);
    if preview.destination_capacity == 0 {
        preview.invalid_reason = Some("The selected front has no military capacity");
        return;
    }

    let route_start = sources
        .iter()
        .filter(|source| view.cell(**source).is_some_and(|cell| cell.infantry > 0))
        .max_by_key(|source| (distance.get(source).copied().unwrap_or(0), **source))
        .copied();
    if let Some(mut current) = route_start {
        preview.route.push(current);
        while let Some(next_cell) = next.get(&current).copied() {
            if next_cell == current {
                break;
            }
            current = next_cell;
            preview.route.push(current);
        }
        if let Some(edge) = preview
            .front_edges
            .iter()
            .find(|edge| edge.source == current)
        {
            preview.route.push(edge.target);
        }
    }
    if preview.route.len() >= 2 {
        preview.bottleneck = preview
            .route
            .windows(2)
            .min_by_key(|edge| view.cell(edge[1]).map_or(0, |cell| cell.military_capacity))
            .map(|edge| (edge[0], edge[1]));
    }
    let max_distance = distance.values().copied().max().unwrap_or(0);
    let congestion = preview
        .strength_upper_bound
        .saturating_sub(preview.destination_capacity)
        / 20;
    preview.eta_seconds = (u64::from(max_distance.saturating_add(1)) * 2 + congestion)
        .min(u64::from(u32::MAX)) as u32;
}

fn build_expand_all_preview(
    view: &MatchView,
    selected: &BTreeSet<Axial>,
    commitment_percent: u8,
    preview: &mut OrderPreview,
) {
    if selected.len() > MAX_COMMAND_SELECTION_CELLS {
        preview.invalid_reason = Some("Expand All selection exceeds the 4096-cell V1 limit");
        return;
    }
    if selected.is_empty() {
        preview.invalid_reason = Some("Select owned cells before expanding");
        return;
    }
    let sources = selected
        .iter()
        .filter(|coordinate| {
            view.cell(**coordinate).is_some_and(|cell| {
                view.is_local_owned(**coordinate) && cell.is_land() && !cell.blocked
            })
        })
        .copied()
        .collect::<BTreeSet<_>>();
    if sources.len() != selected.len() {
        preview
            .excluded
            .extend(selected.difference(&sources).copied());
        preview.invalid_reason = Some("Every selected source must be owned passable ground");
        return;
    }

    let edges = match expand_all_front_edges(view, &sources) {
        Ok(edges) => edges,
        Err(FrontSelectionError::EmptySelection) => {
            preview.invalid_reason = Some("Select owned cells before expanding");
            return;
        }
        Err(FrontSelectionError::DisconnectedSelection) => {
            preview.invalid_reason = Some("Expand All selection must be one connected region");
            return;
        }
        Err(FrontSelectionError::NoEligibleFront) => {
            preview.invalid_reason = Some("The selection has no passable neutral frontier");
            return;
        }
        Err(FrontSelectionError::InvalidDirection | FrontSelectionError::DisconnectedFront) => {
            preview.invalid_reason = Some("Expand All frontier is invalid");
            return;
        }
    };
    let front_sources = edges
        .iter()
        .map(|edge| edge.source)
        .collect::<BTreeSet<_>>();
    let (next, distance) = selected_reachability_to_front(view, &sources, &front_sources);
    if next.len() != sources.len() {
        preview.excluded.extend(
            sources
                .iter()
                .filter(|source| !next.contains_key(source))
                .copied(),
        );
        preview.invalid_reason =
            Some("Every selected source must reach a neutral frontier inside the selection");
        return;
    }

    preview.front_edges = edges;
    let percentage = u64::from(commitment_percent.clamp(10, 100));
    preview.strength_upper_bound = sources
        .iter()
        .filter_map(|coordinate| view.cell(*coordinate))
        .map(|cell| cell.infantry.saturating_mul(percentage) / 100)
        .fold(0_u64, u64::saturating_add);
    if preview.strength_upper_bound == 0 {
        preview.invalid_reason = Some("Selected sources have no visible infantry to request");
        return;
    }
    let targets = preview
        .front_edges
        .iter()
        .map(|edge| edge.target)
        .collect::<BTreeSet<_>>();
    preview.destination_capacity = targets
        .iter()
        .filter_map(|coordinate| view.cell(*coordinate))
        .map(|cell| cell.military_capacity)
        .fold(0_u64, u64::saturating_add);
    if preview.destination_capacity == 0 {
        preview.invalid_reason = Some("The neutral frontier has no military capacity");
        return;
    }

    // Show one representative rear-to-front route. Every edge is still drawn,
    // while this route communicates that the rear selection feeds its nearest
    // boundary rather than cloning troops into each outward direction.
    let route_start = sources
        .iter()
        .filter(|source| view.cell(**source).is_some_and(|cell| cell.infantry > 0))
        .max_by_key(|source| (distance.get(source).copied().unwrap_or(0), **source))
        .copied();
    if let Some(mut current) = route_start {
        preview.route.push(current);
        while let Some(next_cell) = next.get(&current).copied() {
            if next_cell == current {
                break;
            }
            current = next_cell;
            preview.route.push(current);
        }
        if let Some(edge) = preview
            .front_edges
            .iter()
            .find(|edge| edge.source == current)
        {
            preview.route.push(edge.target);
        }
    }
    if preview.route.len() >= 2 {
        preview.bottleneck = preview
            .route
            .windows(2)
            .min_by_key(|edge| view.cell(edge[1]).map_or(0, |cell| cell.military_capacity))
            .map(|edge| (edge[0], edge[1]));
    }
    let max_distance = distance.values().copied().max().unwrap_or(0);
    let congestion = preview
        .strength_upper_bound
        .saturating_sub(preview.destination_capacity)
        / 20;
    preview.eta_seconds = (u64::from(max_distance.saturating_add(1)) * 2 + congestion)
        .min(u64::from(u32::MAX)) as u32;
}

fn selected_reachability_to_front(
    view: &MatchView,
    sources: &BTreeSet<Axial>,
    front_sources: &BTreeSet<Axial>,
) -> (BTreeMap<Axial, Axial>, BTreeMap<Axial, u32>) {
    let mut next = BTreeMap::new();
    let mut distance = BTreeMap::new();
    let mut pending = VecDeque::new();
    for source in front_sources {
        next.insert(*source, *source);
        distance.insert(*source, 0_u32);
        pending.push_back(*source);
    }
    while let Some(current) = pending.pop_front() {
        let current_distance = distance[&current];
        for neighbor in current.neighbors() {
            if !sources.contains(&neighbor) || next.contains_key(&neighbor) {
                continue;
            }
            let traversable =
                view.cell(current)
                    .zip(view.cell(neighbor))
                    .is_some_and(|(current, neighbor)| {
                        current.is_land()
                            && neighbor.is_land()
                            && !current.blocked
                            && !neighbor.blocked
                            && (i32::from(current.elevation) - i32::from(neighbor.elevation))
                                .unsigned_abs()
                                <= 1
                    });
            if traversable {
                next.insert(neighbor, current);
                distance.insert(neighbor, current_distance.saturating_add(1));
                pending.push_back(neighbor);
            }
        }
    }
    (next, distance)
}

const fn front_selection_error_text(error: FrontSelectionError) -> &'static str {
    match error {
        FrontSelectionError::EmptySelection => "Select owned cells before pushing",
        FrontSelectionError::DisconnectedSelection => "Push selection must be one connected region",
        FrontSelectionError::InvalidDirection => "Push direction must match one hex direction",
        FrontSelectionError::NoEligibleFront => "No non-owned passable front faces that direction",
        FrontSelectionError::DisconnectedFront => {
            "The chosen boundary contains separate front arcs"
        }
    }
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
    if interaction.sources.len() > MAX_COMMAND_SELECTION_CELLS {
        preview.invalid_reason = Some("Selection exceeds the 4096-cell V1 command limit");
        interaction.preview = preview;
        interaction.preview_key = Some(key);
        return;
    }
    match &interaction.mode {
        OrderMode::PushFrontOrient { .. } | OrderMode::PushFrontPreview { .. } => {
            if let Some(direction) = interaction.push_direction() {
                build_push_front_preview(
                    &view,
                    &interaction.sources,
                    direction,
                    interaction.amount_percent,
                    &mut preview,
                );
            } else {
                preview.invalid_reason = Some("Drag farther to choose one of six directions");
            }
        }
        OrderMode::ExpandAllPreview => build_expand_all_preview(
            &view,
            &interaction.sources,
            interaction.amount_percent,
            &mut preview,
        ),
        OrderMode::BalancePreview => build_redistribution_preview(
            &view,
            &interaction.sources,
            CoreDistributionPreset::Balance,
            interaction.amount_percent,
            &mut preview,
        ),
        OrderMode::FrontLoadOrient { .. } | OrderMode::FrontLoadPreview { .. } => {
            if let Some(direction) = interaction.frontload_direction() {
                if let Some(direction) = quantize_world_direction(direction) {
                    build_redistribution_preview(
                        &view,
                        &interaction.sources,
                        CoreDistributionPreset::front_load(direction),
                        interaction.amount_percent,
                        &mut preview,
                    );
                } else {
                    preview.invalid_reason = Some("Front-load direction is too short");
                }
            }
        }
        OrderMode::CoreLoadPreview => build_redistribution_preview(
            &view,
            &interaction.sources,
            CoreDistributionPreset::CoreLoad,
            interaction.amount_percent,
            &mut preview,
        ),
        OrderMode::PerimeterLoadPreview => build_redistribution_preview(
            &view,
            &interaction.sources,
            CoreDistributionPreset::PerimeterLoad,
            interaction.amount_percent,
            &mut preview,
        ),
        OrderMode::Idle => {}
        OrderMode::Submitting { .. } => unreachable!("submitting previews return before rebuild"),
    }
    interaction.preview = preview;
    interaction.preview_key = Some(key);
}

fn build_redistribution_preview(
    view: &MatchView,
    selected: &BTreeSet<Axial>,
    preset: CoreDistributionPreset,
    amount_percent: u8,
    preview: &mut OrderPreview,
) {
    if selected.len() < 2 {
        preview.invalid_reason = Some("Redistribution needs at least two owned hexes");
        return;
    }

    let mut map = HexMap::new();
    let mut total = 0_u64;
    for &coordinate in selected {
        let Some(cell) = view.cell(coordinate) else {
            preview.invalid_reason = Some("Redistribution contains an unknown hex");
            return;
        };
        if cell.owner != Some(view.local_player) || !cell.is_land() || cell.blocked {
            preview.invalid_reason = Some("Redistribution needs owned passable ground");
            return;
        }
        total = total.saturating_add(cell.infantry);
        map.insert(Cell {
            coordinate,
            terrain: cell.terrain,
            elevation: cell.elevation,
            capturable: true,
            habitable: true,
            owner: cell.owner,
            civilian_population: cell.civilians,
            civilian_capacity: cell.civilians,
            forces: ForceComposition::infantry(cell.infantry),
            military_capacity: cell.military_capacity,
        });
    }

    let amount_bps = u32::from(amount_percent.clamp(10, 100)) * 100;
    let Ok(distribution) = redistribution_targets_with_commitment(
        &map,
        view.local_player,
        selected.iter().copied(),
        total,
        preset,
        amount_bps,
    ) else {
        preview.invalid_reason = Some("This redistribution cannot be resolved");
        return;
    };

    for (&coordinate, &target) in &distribution.targets {
        let capacity = view
            .cell(coordinate)
            .map_or(0, |cell| cell.military_capacity);
        preview.heatmap.insert(
            coordinate,
            if capacity == 0 {
                0.0
            } else {
                target as f32 / capacity as f32
            },
        );
    }
    preview.strength_upper_bound = selected
        .iter()
        .filter_map(|coordinate| view.cell(*coordinate))
        .map(|cell| cell.infantry.saturating_mul(u64::from(amount_bps)) / 10_000)
        .sum();
    preview.destination_capacity = selected
        .iter()
        .filter_map(|coordinate| view.cell(*coordinate))
        .map(|cell| cell.military_capacity)
        .sum();
    preview.eta_seconds = selected.len() as u32 / 3 + 3;
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
    use crate::model::CellView;
    use hex_core::TerrainKind;

    fn preview_cell(
        coordinate: Axial,
        owner: Option<u32>,
        infantry: u64,
        elevation: i16,
    ) -> CellView {
        CellView {
            coordinate,
            terrain: TerrainKind::Plains,
            elevation,
            owner,
            civilians: 0,
            infantry,
            military_capacity: 100,
            blocked: false,
        }
    }

    fn push_preview_view() -> MatchView {
        let mut view = MatchView::connecting(1);
        for cell in [
            preview_cell(Axial::ZERO, Some(1), 10, 0),
            preview_cell(Axial::new(1, 0), Some(1), 20, 0),
            preview_cell(Axial::new(2, 0), Some(1), 30, 0),
            preview_cell(Axial::new(3, 0), None, 0, 0),
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
            core_width: 3,
            core_height: 5,
            rings: 0,
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
    fn combined_brush_growth_adds_complete_hex_perimeter_rings() {
        let center = Axial::new(-4, 7);
        let mut brush = SelectionBrush::default();

        brush.resize(BrushAxis::Both, true);
        let first_ring = brush.cells(center).into_iter().collect::<BTreeSet<_>>();
        let expected_first_ring = center
            .neighbors()
            .into_iter()
            .chain([center])
            .collect::<BTreeSet<_>>();
        assert_eq!(first_ring, expected_first_ring);
        assert_eq!((brush.width(), brush.height(), brush.rings()), (3, 3, 1));

        brush.resize(BrushAxis::Both, true);
        let second_ring = brush.cells(center).into_iter().collect::<BTreeSet<_>>();
        assert_eq!(second_ring.len(), 19);
        assert!(
            second_ring
                .iter()
                .all(|coordinate| center.distance(*coordinate) <= 2)
        );

        brush.resize(BrushAxis::Both, false);
        assert_eq!(
            brush.cells(center).into_iter().collect::<BTreeSet<_>>(),
            expected_first_ring
        );
        brush.resize(BrushAxis::Both, false);
        assert_eq!(brush.cells(center), vec![center]);
    }

    #[test]
    fn drag_rasterization_fills_every_hex_between_pointer_samples() {
        let start = Axial::new(-4, 2);
        let end = Axial::new(5, -3);
        let line = hex_line_between(start, end);

        assert_eq!(line.first(), Some(&start));
        assert_eq!(line.last(), Some(&end));
        assert_eq!(line.len() as u64, start.distance(end) + 1);
        assert!(line.windows(2).all(|pair| pair[0].distance(pair[1]) == 1));
    }

    #[test]
    fn brush_resize_is_bounded_and_modifier_aware() {
        let idle = OrderMode::Idle;
        let push = OrderMode::PushFrontPreview {
            direction: Axial::new(1, 0),
        };

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
                &push
            ),
            None
        );
        assert_eq!(
            requested_brush_resize(&pressed([KeyCode::BracketRight]), &push),
            None
        );

        let mut brush = SelectionBrush::default();
        for _ in 0..20 {
            brush.resize(BrushAxis::Both, true);
        }
        assert_eq!((brush.width(), brush.height(), brush.rings()), (31, 31, 15));
        for _ in 0..20 {
            brush.resize(BrushAxis::Both, false);
        }
        assert_eq!((brush.width(), brush.height(), brush.rings()), (1, 1, 0));

        brush.resize(BrushAxis::Width, true);
        brush.resize(BrushAxis::Height, true);
        assert_eq!((brush.width(), brush.height(), brush.rings()), (3, 3, 0));
        brush.resize(BrushAxis::Both, true);
        assert_eq!((brush.width(), brush.height(), brush.rings()), (5, 5, 1));
        brush.resize(BrushAxis::Width, true);
        assert_eq!((brush.width(), brush.height(), brush.rings()), (7, 5, 1));
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
    fn push_direction_quantizes_to_each_exact_hex_axis() {
        for expected in Axial::DIRECTIONS {
            let plane = axial_to_plane(expected);
            assert_eq!(quantize_world_direction(plane * 3.0), Some(expected));
        }
        assert_eq!(quantize_world_direction(Vec2::splat(0.01)), None);
    }

    #[test]
    fn preview_cache_key_tracks_revisions_amount_mode_and_direction() {
        let mut view = MatchView::connecting(1);
        let mut interaction = InteractionState {
            mode: OrderMode::PushFrontPreview {
                direction: Axial::new(1, 0),
            },
            ..Default::default()
        };
        let mut prior = order_preview_key(&view, &interaction).expect("push key");

        interaction.source_revision = interaction.source_revision.wrapping_add(1);
        let current = order_preview_key(&view, &interaction).expect("source key");
        assert_ne!(current, prior);
        prior = current;

        interaction.amount_percent = 70;
        let current = order_preview_key(&view, &interaction).expect("amount key");
        assert_ne!(current, prior);
        prior = current;

        interaction.mode = OrderMode::PushFrontPreview {
            direction: Axial::new(0, 1),
        };
        let current = order_preview_key(&view, &interaction).expect("direction key");
        assert_ne!(current, prior);
        prior = current;

        interaction.mode = OrderMode::FrontLoadPreview { direction: Vec2::Y };
        let current = order_preview_key(&view, &interaction).expect("mode key");
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
    fn push_preview_uses_the_front_edge_and_caches_unchanged_inputs() {
        let sources = BTreeSet::from([Axial::ZERO, Axial::new(1, 0), Axial::new(2, 0)]);
        let interaction = InteractionState {
            sources,
            mode: OrderMode::PushFrontPreview {
                direction: Axial::new(1, 0),
            },
            amount_percent: 100,
            source_revision: 1,
            ..Default::default()
        };

        let mut app = App::new();
        app.insert_resource(push_preview_view())
            .insert_resource(interaction)
            .add_systems(Update, update_order_preview);
        app.update();

        let preview = &app.world().resource::<InteractionState>().preview;
        assert_eq!(
            preview.front_edges,
            vec![DirectedFrontEdge {
                source: Axial::new(2, 0),
                target: Axial::new(3, 0),
            }]
        );
        assert_eq!(
            preview.route,
            vec![
                Axial::ZERO,
                Axial::new(1, 0),
                Axial::new(2, 0),
                Axial::new(3, 0),
            ]
        );
        assert_eq!(preview.strength_upper_bound, 60);
        assert_eq!(preview.destination_capacity, 100);
        assert!(preview.excluded.is_empty());
        assert_eq!(preview.invalid_reason, None);

        app.world_mut()
            .resource_mut::<InteractionState>()
            .preview
            .strength_upper_bound = 999;
        app.update();
        assert_eq!(
            app.world()
                .resource::<InteractionState>()
                .preview
                .strength_upper_bound,
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
                .strength_upper_bound,
            30
        );
    }

    #[test]
    fn push_preview_rejects_a_selected_corridor_split_by_a_cliff() {
        let mut view = push_preview_view();
        view.cell_mut(Axial::new(1, 0))
            .expect("middle source")
            .elevation = 3;
        let mut preview = OrderPreview::default();
        build_push_front_preview(
            &view,
            &BTreeSet::from([Axial::ZERO, Axial::new(1, 0), Axial::new(2, 0)]),
            Axial::new(1, 0),
            50,
            &mut preview,
        );

        assert_eq!(
            preview.invalid_reason,
            Some("Selected troops must reach the front inside the selection")
        );
        assert!(preview.front_edges.is_empty());
        assert_eq!(
            preview.excluded,
            BTreeSet::from([Axial::ZERO, Axial::new(1, 0)])
        );
    }

    #[test]
    fn push_preview_rejects_zero_committable_infantry() {
        let mut view = push_preview_view();
        for coordinate in [Axial::ZERO, Axial::new(1, 0), Axial::new(2, 0)] {
            view.cell_mut(coordinate).expect("owned source").infantry = 0;
        }
        let mut preview = OrderPreview::default();
        build_push_front_preview(
            &view,
            &BTreeSet::from([Axial::ZERO, Axial::new(1, 0), Axial::new(2, 0)]),
            Axial::new(1, 0),
            50,
            &mut preview,
        );

        assert_eq!(
            preview.invalid_reason,
            Some("Selected sources have no visible infantry to request")
        );
    }

    #[test]
    fn expand_all_preview_finds_every_neutral_direction_and_excludes_enemy_edges() {
        let source = Axial::ZERO;
        let enemy_target = source + Axial::DIRECTIONS[2];
        let mut view = MatchView::connecting(1);
        view.cells
            .insert(source, preview_cell(source, Some(1), 60, 0));
        for direction in Axial::DIRECTIONS {
            let coordinate = source + direction;
            let owner = (coordinate == enemy_target).then_some(2);
            view.cells
                .insert(coordinate, preview_cell(coordinate, owner, 0, 0));
        }
        view.rebuild_chunk_index();

        let mut preview = OrderPreview::default();
        build_expand_all_preview(&view, &BTreeSet::from([source]), 25, &mut preview);

        assert_eq!(preview.invalid_reason, None);
        assert_eq!(preview.front_edges.len(), 5);
        assert!(
            preview
                .front_edges
                .iter()
                .all(|edge| edge.target != enemy_target)
        );
        assert_eq!(preview.strength_upper_bound, 15);
        assert_eq!(preview.destination_capacity, 500);
    }

    #[test]
    fn expand_all_preview_deduplicates_a_shared_concave_target() {
        let left = Axial::ZERO;
        let right = Axial::new(1, 0);
        let shared_target = Axial::new(0, 1);
        let mut view = MatchView::connecting(1);
        for coordinate in [left, right] {
            view.cells
                .insert(coordinate, preview_cell(coordinate, Some(1), 20, 0));
        }
        view.cells
            .insert(shared_target, preview_cell(shared_target, None, 0, 0));
        view.rebuild_chunk_index();

        let mut preview = OrderPreview::default();
        build_expand_all_preview(&view, &BTreeSet::from([left, right]), 50, &mut preview);

        assert_eq!(preview.invalid_reason, None);
        assert_eq!(
            preview.front_edges,
            vec![DirectedFrontEdge {
                source: left,
                target: shared_target,
            }]
        );
        assert_eq!(preview.strength_upper_bound, 20);
    }

    #[test]
    fn expand_all_preview_rejects_disconnected_source_regions() {
        let left = Axial::ZERO;
        let right = Axial::new(3, 0);
        let mut view = MatchView::connecting(1);
        for source in [left, right] {
            view.cells
                .insert(source, preview_cell(source, Some(1), 20, 0));
            let target = source + Axial::new(0, 1);
            view.cells.insert(target, preview_cell(target, None, 0, 0));
        }
        view.rebuild_chunk_index();

        let mut preview = OrderPreview::default();
        build_expand_all_preview(&view, &BTreeSet::from([left, right]), 50, &mut preview);

        assert_eq!(
            preview.invalid_reason,
            Some("Expand All selection must be one connected region")
        );
        assert!(preview.front_edges.is_empty());
    }

    #[test]
    fn every_order_preview_rejects_an_oversized_selection_before_building() {
        let sources = (0..=MAX_COMMAND_SELECTION_CELLS)
            .map(|index| Axial::new(i32::try_from(index).expect("test index fits i32"), 0))
            .collect();
        let interaction = InteractionState {
            sources,
            mode: OrderMode::BalancePreview,
            source_revision: 1,
            ..Default::default()
        };
        let mut app = App::new();
        app.insert_resource(MatchView::connecting(1))
            .insert_resource(interaction)
            .add_systems(Update, update_order_preview);
        app.update();

        let preview = &app.world().resource::<InteractionState>().preview;
        assert_eq!(
            preview.invalid_reason,
            Some("Selection exceeds the 4096-cell V1 command limit")
        );
        assert!(preview.heatmap.is_empty());
    }

    #[test]
    fn submission_feedback_requires_the_active_online_command() {
        let interaction = InteractionState {
            mode: OrderMode::Submitting {
                _label: "PUSH FRONT",
            },
            submitting_command_id: Some(41),
            ..Default::default()
        };

        assert!(submission_matches(&interaction, Some(41)));
        assert!(!submission_matches(&interaction, Some(42)));
        assert!(submission_matches(&interaction, None));
    }

    #[test]
    fn expand_all_cancellation_emits_the_selected_source_region() {
        let sources = BTreeSet::from([Axial::ZERO, Axial::new(1, 0)]);
        let interaction = InteractionState {
            sources: sources.clone(),
            mode: OrderMode::ExpandAllPreview,
            ..Default::default()
        };

        let (intent, label, return_mode) =
            cancellation_request(&interaction).expect("Expand All can be cancelled");
        let ClientIntent::CancelExpandAll {
            sources: cancelled_sources,
        } = intent
        else {
            panic!("Expand All preview must emit its matching cancellation intent");
        };
        assert_eq!(cancelled_sources, sources);
        assert_eq!(label, "STOP EXPAND ALL");
        assert!(matches!(return_mode, OrderMode::ExpandAllPreview));
    }

    #[test]
    fn rejected_expand_all_cancellation_restores_its_preview() {
        let interaction = InteractionState {
            mode: OrderMode::Submitting {
                _label: "STOP EXPAND ALL",
            },
            return_after_rejection: Some(OrderMode::ExpandAllPreview),
            submitting_command_id: Some(41),
            ..Default::default()
        };
        let mut app = App::new();
        app.add_message::<ServerUpdate>()
            .insert_resource(interaction)
            .add_systems(Update, finish_submission);
        app.world_mut().write_message(ServerUpdate::Rejected {
            command_id: Some(41),
            reason: "nothing matched".to_owned(),
            relevant_cell: None,
        });
        app.update();

        let interaction = app.world().resource::<InteractionState>();
        assert!(matches!(interaction.mode, OrderMode::ExpandAllPreview));
        assert_eq!(interaction.submitting_command_id, None);
    }
}
