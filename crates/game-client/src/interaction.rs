use std::collections::{BTreeMap, BTreeSet, VecDeque};

use bevy::{picking::pointer::PointerInteraction, prelude::*};
use hex_core::{
    Axial, DirectedFrontEdge, FrontSelectionError, StrategicExterior, selected_directional_routes,
    selected_front_edges, strategic_front_index_for_seed, strategic_fronts,
};

use crate::{
    camera::GameCamera,
    geometry::axial_to_plane,
    model::{MatchView, OrderSelectionProjectionError, ProjectedOrderSelection, ToastKind},
    network::{
        ClientIntent, ExpandWaveError, MAX_WAVE_PREVIEW_RINGS, NetworkSet, ServerUpdate,
        arc_push_routes, forecast_attack_wave, forecast_expand_wave, projected_shape_distribution,
        push_edge_is_eligible, resolve_projected_push_front,
    },
    terrain::TerrainChunk,
};

#[derive(Clone, Debug, Default)]
pub struct OrderPreview {
    /// One aggregated reinforcement corridor for every independently
    /// traversable Push component. Rendering one representative route per
    /// component keeps multi-region previews legible without hiding a region.
    pub component_routes: Vec<Vec<Axial>>,
    pub front_edges: Vec<DirectedFrontEdge>,
    pub excluded: BTreeSet<Axial>,
    pub heatmap: BTreeMap<Axial, f32>,
    /// Signed change from the currently visible cell strength to the proposed
    /// final strength. Positive values are gains and negative values are
    /// losses; zero-delta cells are omitted.
    pub delta_by_cell: BTreeMap<Axial, i128>,
    pub projected_sources: BTreeSet<Axial>,
    pub stop_order_ids: BTreeSet<u64>,
    pub wave_depth: BTreeMap<Axial, u16>,
    pub wave_truncated: bool,
    pub projected_source_count: usize,
    pub retask_handle_count: usize,
    pub retask_order_count: usize,
    pub retask_strength: u64,
    pub projected_strength: u64,
    pub projected_capacity: u64,
    pub eta_seconds: u32,
    /// Upper-bound estimate after excluding unrelated active packet
    /// allocations. A newer authoritative snapshot or destination reservation
    /// can still reduce what the server accepts.
    pub strength_upper_bound: u64,
    pub destination_capacity: u64,
    /// Affected strength projected onto reachable drawn Reshape targets.
    pub reshape_destination_strength: u64,
    /// Affected strength which best-effort Reshape leaves on source cells
    /// outside the drawn footprint, including unreachable source components.
    pub reshape_outside_strength: u64,
    pub component_bottlenecks: Vec<(Axial, Axial)>,
    pub invalid_reason: Option<&'static str>,
}

#[derive(Clone, Debug)]
#[allow(dead_code)] // Retained as a non-primary compatibility path for focused engine previews.
pub enum OrderMode {
    Idle,
    AttackClustersPreview,
    PushFrontOrient {
        start: Vec3,
        current: Vec3,
    },
    PushFrontPreview {
        direction: Axial,
    },
    PushFrontArcPreview,
    FrontRebalanceSelectSource,
    FrontRebalanceDrag {
        source_front_seed: Axial,
        target_front_seed: Option<Axial>,
    },
    ExpandAllPreview,
    ReshapeDrawing,
    ReshapePreview,
    StopPreview {
        order_ids: BTreeSet<u64>,
    },
    Submitting {
        _label: &'static str,
    },
}

impl OrderMode {
    pub const fn label(&self) -> &'static str {
        match self {
            Self::Idle => "CLUSTER SELECTION",
            Self::AttackClustersPreview => "ATTACK CLUSTERS / TARGETS",
            Self::PushFrontOrient { .. } => "PUSH FRONT / ORIENT",
            Self::PushFrontPreview { .. } => "PUSH FRONT / READY",
            Self::PushFrontArcPreview => "CONTACT FRONTS / READY",
            Self::FrontRebalanceSelectSource => "FRONT REBALANCE / PICK SOURCE",
            Self::FrontRebalanceDrag { .. } => "FRONT REBALANCE / PICK TARGET",
            Self::ExpandAllPreview => "EXPAND PERIMETER / READY",
            Self::ReshapeDrawing => "RESHAPE / DRAW DESTINATION",
            Self::ReshapePreview => "RESHAPE / READY",
            Self::StopPreview { .. } => "STOP ORDERS / READY",
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
const MAX_COMMAND_SELECTION_CELLS: usize = 32_768;
const MAX_COMMAND_SUPERSEDE_ORDERS: usize = 32_768;
const MAX_CONTEXTUAL_COMMANDS_IN_FLIGHT: usize = 32;
const PUSH_DRAG_THRESHOLD_PIXELS: f32 = 10.0;

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
        match axis {
            BrushAxis::Width => self.resize_width(grow),
            BrushAxis::Height => self.resize_height(grow),
            BrushAxis::Both => {
                if grow {
                    if self.width() < MAX_BRUSH_SIZE && self.height() < MAX_BRUSH_SIZE {
                        self.rings += 1;
                    }
                } else if self.rings > 0 {
                    self.rings -= 1;
                } else {
                    self.core_width = self.core_width.saturating_sub(2).max(1);
                    self.core_height = self.core_height.saturating_sub(2).max(1);
                }
            }
        }
    }

    fn resize_width(&mut self, grow: bool) {
        if grow {
            let max_core_dimension = MAX_BRUSH_SIZE.saturating_sub(self.rings * 2);
            self.core_width = self.core_width.saturating_add(2).min(max_core_dimension);
        } else if self.core_width > 1 {
            self.core_width -= 2;
        } else if self.rings > 0 {
            // Consume one symmetric ring into the untouched axis. This keeps
            // the visible height stable while reducing visible width by two,
            // even when ring growth owns all of the horizontal extent.
            self.rings -= 1;
            self.core_height = self.core_height.saturating_add(2);
        }
    }

    fn resize_height(&mut self, grow: bool) {
        if grow {
            let max_core_dimension = MAX_BRUSH_SIZE.saturating_sub(self.rings * 2);
            self.core_height = self.core_height.saturating_add(2).min(max_core_dimension);
        } else if self.core_height > 1 {
            self.core_height -= 2;
        } else if self.rings > 0 {
            // Mirror the width conversion: preserve visible width while the
            // vertical axis sheds one cell on each side.
            self.rings -= 1;
            self.core_width = self.core_width.saturating_add(2);
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
    AttackClusters,
    PushFront {
        direction: Option<Axial>,
    },
    FrontRebalance {
        source_front_seed: Option<Axial>,
        target_front_seed: Option<Axial>,
    },
    ExpandAll,
    Reshape,
    Stop,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct OrderPreviewKey {
    source_revision: u64,
    share_percent: Option<u8>,
    mode: PreviewModeKey,
    shape_revision: u64,
    attack_revision: u64,
    state_revision: u64,
    topology_revision: u64,
    retask_revision: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ContextualGesture {
    Expand {
        sources: BTreeSet<Axial>,
        focus: Axial,
        commitment_percent: u8,
    },
    Attack {
        sources: BTreeSet<Axial>,
        targets: BTreeSet<Axial>,
        commitment_percent: u8,
    },
}

impl ContextualGesture {
    fn from_intent(intent: &ClientIntent) -> Option<Self> {
        match intent {
            ClientIntent::ExpandClusters {
                sources,
                focus,
                commitment_percent,
            } => Some(Self::Expand {
                sources: sources.clone(),
                focus: *focus,
                commitment_percent: *commitment_percent,
            }),
            ClientIntent::AttackClusters {
                sources,
                targets,
                commitment_percent,
            } => Some(Self::Attack {
                sources: sources.clone(),
                targets: targets.clone(),
                commitment_percent: *commitment_percent,
            }),
            _ => None,
        }
    }

    const fn label(&self) -> &'static str {
        match self {
            Self::Expand { .. } => "EXPAND",
            Self::Attack { .. } => "ATTACK",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ContextualSubmissionGroup {
    gesture: ContextualGesture,
    /// Online commands receive IDs in FIFO `SubmissionStarted` order. Offline
    /// fixture responses have no ID and consume this queue from the front.
    command_ids: VecDeque<Option<u64>>,
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
    if direction.length_squared() <= f32::EPSILON {
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

fn visible_push_drag(start: Option<Vec2>, current: Option<Vec2>) -> bool {
    start
        .zip(current)
        .is_some_and(|(start, current)| start.distance(current) >= PUSH_DRAG_THRESHOLD_PIXELS)
}

#[derive(Resource, Debug)]
pub struct InteractionState {
    pub hovered: Option<Axial>,
    /// Last cell hit on the map, retained while the pointer crosses the HUD so
    /// hover-relative keys such as Cluster still have a map-space seed.
    pub last_map_hovered: Option<Axial>,
    pub cursor_world: Option<Vec3>,
    pub cursor_screen: Option<Vec2>,
    pub sources: BTreeSet<Axial>,
    /// Snapshotted union of complete enemy traversable clusters staged for a
    /// single contextual attack. Share is applied once to this whole union.
    pub attack_targets: BTreeSet<Axial>,
    /// Enemy contested cells selected as stable handles. The associated order
    /// IDs are snapshotted when painted and never rebound implicitly.
    pub retask_handles: BTreeMap<Axial, BTreeSet<u64>>,
    pub shape_targets: BTreeSet<Axial>,
    pub mode: OrderMode,
    pub amount_percent: u8,
    pub preview: OrderPreview,
    pub show_help: bool,
    pub brush: SelectionBrush,
    pub source_revision: u64,
    pub shape_revision: u64,
    pub attack_revision: u64,
    stroke: Option<PaintStroke>,
    return_after_rejection: Option<OrderMode>,
    submitting_command_id: Option<u64>,
    /// Rapid repeats of one exact contextual gesture may be in flight. A
    /// different click is held until the group settles, preventing unrelated
    /// map input from becoming an accidental duplicate action.
    contextual_submissions: Option<ContextualSubmissionGroup>,
    preview_key: Option<OrderPreviewKey>,
    push_start_screen: Option<Vec2>,
}

impl Default for InteractionState {
    fn default() -> Self {
        Self {
            hovered: None,
            last_map_hovered: None,
            cursor_world: None,
            cursor_screen: None,
            sources: BTreeSet::new(),
            attack_targets: BTreeSet::new(),
            retask_handles: BTreeMap::new(),
            shape_targets: BTreeSet::new(),
            mode: OrderMode::Idle,
            amount_percent: 50,
            preview: OrderPreview::default(),
            show_help: false,
            brush: SelectionBrush::default(),
            source_revision: 0,
            shape_revision: 0,
            attack_revision: 0,
            stroke: None,
            return_after_rejection: None,
            submitting_command_id: None,
            contextual_submissions: None,
            preview_key: None,
            push_start_screen: None,
        }
    }
}

impl InteractionState {
    fn has_pending_submission(&self) -> bool {
        self.return_after_rejection.is_some()
            || self.submitting_command_id.is_some()
            || matches!(self.mode, OrderMode::Submitting { .. })
    }

    pub fn contextual_in_flight_count(&self) -> usize {
        self.contextual_submissions
            .as_ref()
            .map_or(0, |submissions| submissions.command_ids.len())
    }

    pub fn contextual_in_flight_label(&self) -> Option<&'static str> {
        self.contextual_submissions
            .as_ref()
            .map(|submissions| submissions.gesture.label())
    }

    pub fn supersede_order_ids(&self) -> BTreeSet<u64> {
        self.retask_handles.values().flatten().copied().collect()
    }

    pub fn has_selection(&self) -> bool {
        !self.sources.is_empty() || !self.retask_handles.is_empty()
    }

    pub fn push_direction(&self) -> Option<Axial> {
        match self.mode {
            OrderMode::PushFrontOrient { start, current } => {
                visible_push_drag(self.push_start_screen, self.cursor_screen).then(|| {
                    quantize_world_direction(Vec2::new(current.x - start.x, current.z - start.z))
                })?
            }
            OrderMode::PushFrontPreview { direction } => Some(direction),
            _ => None,
        }
    }
}

#[derive(Message, Clone, Copy, Debug)]
#[allow(dead_code)] // Legacy UI messages remain accepted while the HUD exposes cluster-first controls.
pub enum UiAction {
    PushFront,
    FrontRebalance,
    ExpandAll,
    Reshape,
    StopOrders,
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
                    reconcile_selection,
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
        // Keep the last map-space position while the pointer crosses the HUD so
        // key-launched orientation gestures never invent screen/world math.
        return;
    }

    let (camera, camera_transform) = *camera;
    let Some(cursor) = window.cursor_position() else {
        interaction.hovered = None;
        interaction.cursor_world = None;
        interaction.cursor_screen = None;
        return;
    };
    interaction.cursor_screen = Some(cursor);
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
    if interaction.hovered.is_some() {
        interaction.last_map_hovered = interaction.hovered;
    }
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

    if interaction.has_pending_submission() {
        for _ in actions.read() {}
        return;
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
            select_all_clusters(&mut view, &mut interaction);
        } else if keyboard.just_pressed(KeyCode::KeyC)
            && let Some(seed) = interaction.hovered.or(interaction.last_map_hovered)
        {
            let combine = selection_combine(&keyboard);
            select_cluster(&mut view, &mut interaction, seed, combine);
        }
    }

    let brush_resize = requested_brush_resize(&keyboard, &interaction.mode);
    if let Some((axis, grow)) = brush_resize {
        interaction.brush.resize(axis, grow);
    }

    let mut requested_actions = Vec::new();
    if keyboard.just_pressed(KeyCode::KeyT) {
        requested_actions.push(UiAction::Reshape);
    }
    if keyboard.just_pressed(KeyCode::KeyB) {
        requested_actions.push(UiAction::FrontRebalance);
    }
    if keyboard.just_pressed(KeyCode::KeyX) {
        requested_actions.push(UiAction::StopOrders);
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

    let mode_before_actions = interaction.mode.clone();
    let click_confirms_preview = mouse.just_pressed(MouseButton::Left)
        && pointer_is_preview(&mode_before_actions)
        && !matches!(mode_before_actions, OrderMode::AttackClustersPreview)
        && interaction.hovered.is_some()
        && !keyboard.pressed(KeyCode::Space);
    for action in requested_actions {
        handle_action(action, &mut interaction, &mut view, &mut intents);
    }
    let contextual_click_consumed =
        handle_contextual_map_click(&keyboard, &mouse, &mut interaction, &mut view, &mut intents);
    handle_front_rebalance_drag(&keyboard, &mouse, &mut interaction, &mut view, &mut intents);
    if !contextual_click_consumed
        && click_confirms_preview
        && same_preview_mode(&mode_before_actions, &interaction.mode)
    {
        submit_current(&mut interaction, &mut view, &mut intents);
    }

    let cursor_world = interaction.cursor_world;
    let cursor_screen = interaction.cursor_screen;
    let push_start_screen = interaction.push_start_screen;
    if let OrderMode::PushFrontOrient { start, current } = &mut interaction.mode {
        if let Some(cursor) = cursor_world {
            *current = cursor;
        }
        if keyboard.just_released(KeyCode::KeyP) {
            let direction = Vec2::new(current.x - start.x, current.z - start.z);
            if !interaction.shape_targets.is_empty() {
                interaction.shape_targets.clear();
                interaction.shape_revision = interaction.shape_revision.wrapping_add(1);
            }
            if visible_push_drag(push_start_screen, cursor_screen)
                && let Some(direction) = quantize_world_direction(direction)
            {
                interaction.mode = OrderMode::PushFrontPreview { direction };
            } else {
                interaction.mode = OrderMode::PushFrontArcPreview;
                view.show_toast(
                    "Contact fronts use local normals · drag P for one global direction",
                    ToastKind::Info,
                );
            }
            interaction.push_start_screen = None;
        }
    }
}

fn same_preview_mode(before: &OrderMode, after: &OrderMode) -> bool {
    pointer_is_preview(before)
        && pointer_is_preview(after)
        && std::mem::discriminant(before) == std::mem::discriminant(after)
}

fn pointer_is_preview(mode: &OrderMode) -> bool {
    matches!(
        mode,
        OrderMode::AttackClustersPreview
            | OrderMode::PushFrontPreview { .. }
            | OrderMode::PushFrontArcPreview
            | OrderMode::ExpandAllPreview
            | OrderMode::ReshapePreview
            | OrderMode::StopPreview { .. }
    )
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
        (false, false) if matches!(mode, OrderMode::ReshapeDrawing) => BrushAxis::Both,
        (false, false) => return None,
    };
    matches!(mode, OrderMode::ReshapeDrawing).then_some((axis, grow))
}

fn handle_contextual_map_click(
    keyboard: &ButtonInput<KeyCode>,
    mouse: &ButtonInput<MouseButton>,
    interaction: &mut InteractionState,
    view: &mut MatchView,
    intents: &mut MessageWriter<ClientIntent>,
) -> bool {
    if !mouse.just_pressed(MouseButton::Left)
        || keyboard.pressed(KeyCode::Space)
        || !matches!(
            interaction.mode,
            OrderMode::Idle | OrderMode::AttackClustersPreview
        )
    {
        return false;
    }
    let Some(clicked) = interaction.hovered else {
        return false;
    };
    let Some(cell) = view.cell(clicked) else {
        return false;
    };
    let shift = shift_pressed(keyboard);
    let control = control_pressed(keyboard);

    if cell.owner.is_none() && view.is_capturable(clicked) {
        if !matches!(interaction.mode, OrderMode::Idle) || shift || control {
            return false;
        }
        if interaction.sources.is_empty() {
            view.show_toast(
                "Select one or more owned clusters first",
                ToastKind::Rejection,
            );
            return true;
        }
        let sources = interaction.sources.clone();
        queue_contextual_submission(
            interaction,
            view,
            intents,
            ClientIntent::ExpandClusters {
                sources,
                focus: clicked,
                commitment_percent: interaction.amount_percent,
            },
            "EXPAND CLUSTERS",
        );
        return true;
    }

    if cell.owner.is_none_or(|owner| owner == view.local_player) || !view.is_capturable(clicked) {
        return false;
    }
    if interaction.sources.is_empty() {
        view.show_toast(
            "Select one or more owned clusters first",
            ToastKind::Rejection,
        );
        return true;
    }
    let cluster = enemy_owned_cluster(view, clicked);
    if cluster.is_empty() {
        return false;
    }

    if control && matches!(interaction.mode, OrderMode::AttackClustersPreview) {
        edit_attack_targets(interaction, view, &cluster, AttackTargetEdit::Remove);
        return true;
    }
    if shift {
        edit_attack_targets(interaction, view, &cluster, AttackTargetEdit::Toggle);
        return true;
    }
    if control {
        return true;
    }

    if !edit_attack_targets(interaction, view, &cluster, AttackTargetEdit::Add) {
        return true;
    }
    rebuild_order_preview(view, interaction);
    if let Some(reason) = interaction.preview.invalid_reason {
        view.show_toast(reason, ToastKind::Rejection);
        return true;
    }
    let sources = interaction.sources.clone();
    let targets = interaction.attack_targets.clone();
    let queued = queue_contextual_submission(
        interaction,
        view,
        intents,
        ClientIntent::AttackClusters {
            sources,
            targets,
            commitment_percent: interaction.amount_percent,
        },
        "ATTACK CLUSTERS",
    );
    if queued {
        interaction.attack_targets.clear();
        interaction.attack_revision = interaction.attack_revision.wrapping_add(1);
        interaction.mode = OrderMode::Idle;
        interaction.preview = OrderPreview::default();
        interaction.preview_key = None;
    }
    true
}

fn handle_front_rebalance_drag(
    keyboard: &ButtonInput<KeyCode>,
    mouse: &ButtonInput<MouseButton>,
    interaction: &mut InteractionState,
    view: &mut MatchView,
    intents: &mut MessageWriter<ClientIntent>,
) {
    if keyboard.pressed(KeyCode::Space) {
        return;
    }
    if matches!(interaction.mode, OrderMode::FrontRebalanceSelectSource)
        && mouse.just_pressed(MouseButton::Left)
    {
        let Some(seed) = interaction.hovered else {
            view.show_toast(
                "Point at an owned strategic-front boundary cell",
                ToastKind::Rejection,
            );
            return;
        };
        let Some(component) = selected_complete_component(view, interaction) else {
            view.show_toast(
                "Front Rebalance selection is no longer one complete component",
                ToastKind::Rejection,
            );
            interaction.mode = OrderMode::Idle;
            return;
        };
        if let Err(reason) = front_rebalance_seed_error(view, &component, seed, None) {
            view.show_toast(reason, ToastKind::Rejection);
            return;
        }
        interaction.mode = OrderMode::FrontRebalanceDrag {
            source_front_seed: seed,
            target_front_seed: None,
        };
        interaction.preview_key = None;
    }

    if let OrderMode::FrontRebalanceDrag {
        target_front_seed, ..
    } = &mut interaction.mode
        && mouse.pressed(MouseButton::Left)
    {
        *target_front_seed = interaction.hovered;
        interaction.preview_key = None;
    }

    if !mouse.just_released(MouseButton::Left) {
        return;
    }
    let (source_front_seed, target_front_seed) = match &interaction.mode {
        OrderMode::FrontRebalanceDrag {
            source_front_seed,
            target_front_seed: Some(target_front_seed),
        } => (*source_front_seed, *target_front_seed),
        _ => return,
    };
    let Some(component) = selected_complete_component(view, interaction) else {
        view.show_toast(
            "Front Rebalance selection is no longer one complete component",
            ToastKind::Rejection,
        );
        interaction.mode = OrderMode::Idle;
        return;
    };
    if let Err(reason) =
        front_rebalance_seed_error(view, &component, source_front_seed, Some(target_front_seed))
    {
        view.show_toast(reason, ToastKind::Rejection);
        return;
    }
    begin_submission(
        interaction,
        intents,
        ClientIntent::FrontRebalance {
            source_component_cells: component,
            source_front_seed,
            target_front_seed,
            commitment_percent: interaction.amount_percent,
            supersede_order_ids: interaction.supersede_order_ids(),
        },
        "FRONT REBALANCE",
        OrderMode::FrontRebalanceDrag {
            source_front_seed,
            target_front_seed: Some(target_front_seed),
        },
    );
}

fn selected_complete_component(
    view: &MatchView,
    interaction: &InteractionState,
) -> Option<BTreeSet<Axial>> {
    let seed = *interaction.sources.first()?;
    let component = local_owned_cluster(view, seed);
    (component == interaction.sources).then_some(component)
}

fn front_rebalance_component_error(
    view: &MatchView,
    component: &BTreeSet<Axial>,
) -> Result<(), &'static str> {
    if component.len() > MAX_COMMAND_SELECTION_CELLS {
        return Err("Front Rebalance component exceeds the 32,768-cell command limit");
    }
    let fronts = strategic_fronts(component.iter().copied(), |_, target| {
        strategic_exterior_for_view(view, target)
    })
    .map_err(|_| "Front Rebalance component has no boundary")?;
    if fronts.len() < 2 {
        return Err("Front Rebalance needs at least two strategic fronts");
    }
    Ok(())
}

fn strategic_exterior_for_view(view: &MatchView, target: Axial) -> StrategicExterior {
    let Some(cell) = view.cell(target) else {
        return StrategicExterior::Ignored;
    };
    if !cell.is_land() || cell.blocked {
        return StrategicExterior::Ignored;
    }
    match cell.owner {
        None => StrategicExterior::Neutral,
        Some(owner) if owner == view.local_player => StrategicExterior::Ignored,
        Some(owner) => StrategicExterior::Opponent(owner),
    }
}

fn front_rebalance_seed_error(
    view: &MatchView,
    component: &BTreeSet<Axial>,
    source_front_seed: Axial,
    target_front_seed: Option<Axial>,
) -> Result<(), &'static str> {
    front_rebalance_component_error(view, component)?;
    if !component.contains(&source_front_seed) {
        return Err("Source seed must be an owned boundary cell in the selected component");
    }
    let fronts = strategic_fronts(component.iter().copied(), |_, target| {
        strategic_exterior_for_view(view, target)
    })
    .map_err(|_| "Front Rebalance component has no boundary")?;
    let source_index = strategic_front_index_for_seed(&fronts, source_front_seed)
        .ok_or("Source seed is not on a strategic-front boundary")?;
    let Some(target_front_seed) = target_front_seed else {
        return Ok(());
    };
    if !component.contains(&target_front_seed) {
        return Err("Target seed must be an owned boundary cell in the selected component");
    }
    let target_index = strategic_front_index_for_seed(&fronts, target_front_seed)
        .ok_or("Target seed is not on a strategic-front boundary")?;
    if source_index == target_index {
        return Err("Source and target must be on different strategic fronts");
    }
    // Corner cells may expose edges on both fronts. Authority keeps such cells
    // stationary and routes only from the source-only part of the arc.
    Ok(())
}

#[derive(Clone, Copy)]
enum AttackTargetEdit {
    Add,
    Remove,
    Toggle,
}

fn edit_attack_targets(
    interaction: &mut InteractionState,
    view: &mut MatchView,
    cluster: &BTreeSet<Axial>,
    edit: AttackTargetEdit,
) -> bool {
    let mut candidate = interaction.attack_targets.clone();
    match edit {
        AttackTargetEdit::Remove => candidate.retain(|coordinate| !cluster.contains(coordinate)),
        AttackTargetEdit::Toggle if cluster.is_subset(&candidate) => {
            candidate.retain(|coordinate| !cluster.contains(coordinate));
        }
        AttackTargetEdit::Add | AttackTargetEdit::Toggle => candidate.extend(cluster),
    }
    if candidate.len() > MAX_COMMAND_SELECTION_CELLS {
        view.show_toast(
            "Enemy targets exceed the 32,768-cell command limit",
            ToastKind::Rejection,
        );
        return false;
    }
    if candidate != interaction.attack_targets {
        interaction.attack_targets = candidate;
        interaction.attack_revision = interaction.attack_revision.wrapping_add(1);
        interaction.preview_key = None;
    }
    interaction.mode = if interaction.attack_targets.is_empty() {
        OrderMode::Idle
    } else {
        OrderMode::AttackClustersPreview
    };
    true
}

fn enemy_owned_cluster(view: &MatchView, seed: Axial) -> BTreeSet<Axial> {
    let Some(owner) = view.cell(seed).and_then(|cell| cell.owner) else {
        return BTreeSet::new();
    };
    if owner == view.local_player || !enemy_cluster_cell(view, seed, owner) {
        return BTreeSet::new();
    }

    let mut cluster = BTreeSet::from([seed]);
    let mut frontier = VecDeque::from([seed]);
    while let Some(coordinate) = frontier.pop_front() {
        for neighbor in coordinate.neighbors() {
            if enemy_cluster_edge(view, coordinate, neighbor, owner) && cluster.insert(neighbor) {
                frontier.push_back(neighbor);
            }
        }
    }
    cluster
}

fn enemy_cluster_cell(view: &MatchView, coordinate: Axial, owner: u32) -> bool {
    view.cell(coordinate)
        .is_some_and(|cell| cell.owner == Some(owner) && view.is_capturable(coordinate))
}

fn enemy_cluster_edge(view: &MatchView, from: Axial, to: Axial, owner: u32) -> bool {
    if from.distance(to) != 1
        || !enemy_cluster_cell(view, from, owner)
        || !enemy_cluster_cell(view, to, owner)
    {
        return false;
    }
    view.cell(from)
        .zip(view.cell(to))
        .is_some_and(|(from, to)| {
            (i32::from(from.elevation) - i32::from(to.elevation)).unsigned_abs()
                <= u32::from(view.max_elevation_step)
        })
}

fn local_owned_cluster(view: &MatchView, seed: Axial) -> BTreeSet<Axial> {
    if !view.is_local_owned_passable(seed) {
        return BTreeSet::new();
    }

    let mut cluster = BTreeSet::from([seed]);
    let mut frontier = VecDeque::from([seed]);
    while let Some(coordinate) = frontier.pop_front() {
        for neighbor in coordinate.neighbors() {
            if view.is_local_traversable_edge(coordinate, neighbor) && cluster.insert(neighbor) {
                frontier.push_back(neighbor);
            }
        }
    }
    cluster
}

fn all_owned_passable_cells(view: &MatchView) -> BTreeSet<Axial> {
    view.cells
        .keys()
        .filter(|coordinate| view.is_local_owned_passable(**coordinate))
        .copied()
        .collect()
}

fn selected_order_ids(handles: &BTreeMap<Axial, BTreeSet<u64>>) -> BTreeSet<u64> {
    handles.values().flatten().copied().collect()
}

fn commit_selection_candidate(
    view: &mut MatchView,
    interaction: &mut InteractionState,
    sources: BTreeSet<Axial>,
    retask_handles: BTreeMap<Axial, BTreeSet<u64>>,
) -> bool {
    let order_ids = selected_order_ids(&retask_handles);
    if order_ids.len() > MAX_COMMAND_SUPERSEDE_ORDERS {
        view.show_toast(
            "Selection would exceed the 32,768-order command limit",
            ToastKind::Rejection,
        );
        return false;
    }
    if sources.len() > MAX_COMMAND_SELECTION_CELLS {
        view.show_toast(
            "Selection would exceed the 32,768-cell command limit",
            ToastKind::Rejection,
        );
        return false;
    }
    let projection = match view.project_order_selection(&sources, &order_ids) {
        Ok(projection) => projection,
        Err(error) => {
            view.show_toast(projection_error_text(error), ToastKind::Rejection);
            return false;
        }
    };
    if projection.cells.len() > MAX_COMMAND_SELECTION_CELLS {
        view.show_toast(
            "Selection would exceed the 32,768-cell command limit",
            ToastKind::Rejection,
        );
        return false;
    }
    if interaction.sources == sources && interaction.retask_handles == retask_handles {
        return false;
    }
    interaction.sources = sources;
    interaction.retask_handles = retask_handles;
    interaction.source_revision = interaction.source_revision.wrapping_add(1);
    true
}

fn select_cluster(
    view: &mut MatchView,
    interaction: &mut InteractionState,
    seed: Axial,
    combine: SelectionCombine,
) -> bool {
    let cluster = local_owned_cluster(view, seed);
    if cluster.is_empty() {
        view.show_toast(
            "Hovered hex has no owned traversable cluster",
            ToastKind::Rejection,
        );
        return false;
    }
    let mut sources = interaction.sources.clone();
    match combine {
        SelectionCombine::Replace => sources = cluster,
        SelectionCombine::Add => {
            sources.extend(cluster);
        }
        SelectionCombine::Remove => {
            sources.retain(|coordinate| !cluster.contains(coordinate));
        }
    }
    commit_selection_candidate(view, interaction, sources, BTreeMap::new())
}

fn select_all_clusters(view: &mut MatchView, interaction: &mut InteractionState) -> bool {
    let sources = all_owned_passable_cells(view);
    commit_selection_candidate(view, interaction, sources, BTreeMap::new())
}

#[derive(Default)]
struct SelectionReconcileCache {
    initialized: bool,
    ownership_revision: u64,
    retask_revision: u64,
    source_revision: u64,
}

impl SelectionReconcileCache {
    fn is_current(&self, view: &MatchView, interaction: &InteractionState) -> bool {
        self.initialized
            && self.ownership_revision == view.ownership_revision
            && self.retask_revision == view.retask_revision
            && self.source_revision == interaction.source_revision
    }

    fn record(&mut self, view: &MatchView, interaction: &InteractionState) {
        self.initialized = true;
        self.ownership_revision = view.ownership_revision;
        self.retask_revision = view.retask_revision;
        self.source_revision = interaction.source_revision;
    }
}

fn reconcile_selection(
    view: Res<MatchView>,
    mut interaction: ResMut<InteractionState>,
    mut cache: Local<SelectionReconcileCache>,
) {
    if cache.is_current(&view, &interaction) {
        return;
    }
    let changed = {
        let interaction = &mut *interaction;
        reconcile_selection_sets(
            &view,
            &mut interaction.sources,
            &mut interaction.retask_handles,
        )
    };
    if changed {
        interaction.source_revision = interaction.source_revision.wrapping_add(1);
    }
    cache.record(&view, &interaction);
}

fn reconcile_selection_sets(
    view: &MatchView,
    sources: &mut BTreeSet<Axial>,
    retask_handles: &mut BTreeMap<Axial, BTreeSet<u64>>,
) -> bool {
    let previous_sources = sources.clone();
    let previous_handles = retask_handles.clone();
    let mut surviving = sources
        .iter()
        .filter(|coordinate| view.is_local_owned_passable(**coordinate))
        .copied()
        .collect::<BTreeSet<_>>();
    let mut closed = BTreeSet::new();
    while let Some(seed) = surviving.pop_first() {
        let cluster = local_owned_cluster(view, seed);
        surviving.retain(|coordinate| !cluster.contains(coordinate));
        closed.extend(cluster);
    }
    *sources = closed;
    for order_ids in retask_handles.values_mut() {
        order_ids.retain(|order_id| view.retask_projection.active_order_ids.contains(order_id));
    }
    retask_handles.retain(|_, order_ids| !order_ids.is_empty());
    *sources != previous_sources || *retask_handles != previous_handles
}

fn selected_owned_cluster_count(view: &MatchView, sources: &BTreeSet<Axial>) -> usize {
    let mut remaining = sources
        .iter()
        .filter(|coordinate| view.is_local_owned_passable(**coordinate))
        .copied()
        .collect::<BTreeSet<_>>();
    let mut count = 0;
    while let Some(seed) = remaining.pop_first() {
        let cluster = local_owned_cluster(view, seed);
        remaining.retain(|coordinate| !cluster.contains(coordinate));
        count += 1;
    }
    count
}

fn handle_action(
    action: UiAction,
    interaction: &mut InteractionState,
    view: &mut MatchView,
    intents: &mut MessageWriter<ClientIntent>,
) {
    // Modal submissions own the interaction state until their authoritative
    // response arrives. In particular, formation shortcuts must not orphan the
    // pending command by replacing `Submitting` with another preview.
    if interaction.has_pending_submission() {
        return;
    }

    match action {
        UiAction::PushFront => {
            if !interaction.has_selection() {
                view.show_toast(
                    "Paint sources or active-front handles first",
                    ToastKind::Rejection,
                );
            } else if matches!(
                interaction.mode,
                OrderMode::Idle
                    | OrderMode::PushFrontPreview { .. }
                    | OrderMode::PushFrontArcPreview
            ) {
                if let Some(start) = interaction.cursor_world {
                    interaction.push_start_screen = interaction.cursor_screen;
                    interaction.mode = OrderMode::PushFrontOrient {
                        start,
                        current: start,
                    };
                    view.show_toast(
                        "Tap P for hostile contact arcs · drag for one of six directions",
                        ToastKind::Info,
                    );
                } else {
                    view.show_toast(
                        "Point at the map before orienting Push Front",
                        ToastKind::Rejection,
                    );
                }
            }
        }
        UiAction::FrontRebalance => {
            let Some(component) = selected_complete_component(view, interaction) else {
                view.show_toast(
                    "Front Rebalance needs exactly one complete owned component",
                    ToastKind::Rejection,
                );
                return;
            };
            if let Err(reason) = front_rebalance_component_error(view, &component) {
                view.show_toast(reason, ToastKind::Rejection);
            } else if matches!(interaction.mode, OrderMode::Idle) {
                interaction.mode = OrderMode::FrontRebalanceSelectSource;
                interaction.preview_key = None;
                view.show_toast(
                    "Drag from one owned strategic-front boundary cell to another",
                    ToastKind::Info,
                );
            }
        }
        UiAction::ExpandAll => {
            if !interaction.has_selection() {
                view.show_toast(
                    "Paint sources or active-front handles first",
                    ToastKind::Rejection,
                );
            } else if matches!(interaction.mode, OrderMode::Idle) {
                interaction.mode = OrderMode::ExpandAllPreview;
                view.show_toast(
                    "Expand Perimeter previews every selected region · click map to dispatch",
                    ToastKind::Info,
                );
            }
        }
        UiAction::Reshape => {
            let selected_cluster_count = selected_owned_cluster_count(view, &interaction.sources);
            if selected_cluster_count == 0 {
                view.show_toast("Reshape needs one selected cluster", ToastKind::Rejection);
            } else if selected_cluster_count > 1 {
                view.show_toast(
                    "Reshape works with exactly one selected cluster",
                    ToastKind::Rejection,
                );
            } else {
                interaction.shape_targets.clear();
                interaction.shape_revision = interaction.shape_revision.wrapping_add(1);
                interaction.mode = OrderMode::ReshapeDrawing;
                view.show_toast(
                    "Draw the owned destination shape · release to preview",
                    ToastKind::Info,
                );
            }
        }
        UiAction::StopOrders => begin_stop_preview(interaction, view),
        UiAction::Confirm => submit_current(interaction, view, intents),
        UiAction::Cancel => {
            // A held paint button must not carry an in-progress source or
            // Reshape stroke across the cancellation boundary.
            interaction.stroke = None;
            interaction.push_start_screen = None;
            if matches!(interaction.mode, OrderMode::Idle) {
                if interaction.has_selection() {
                    interaction.sources.clear();
                    interaction.retask_handles.clear();
                    interaction.source_revision = interaction.source_revision.wrapping_add(1);
                }
            } else if !matches!(interaction.mode, OrderMode::Submitting { .. }) {
                if !interaction.attack_targets.is_empty() {
                    interaction.attack_targets.clear();
                    interaction.attack_revision = interaction.attack_revision.wrapping_add(1);
                }
                interaction.shape_targets.clear();
                interaction.shape_revision = interaction.shape_revision.wrapping_add(1);
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
        OrderMode::Idle
            | OrderMode::AttackClustersPreview
            | OrderMode::PushFrontOrient { .. }
            | OrderMode::PushFrontPreview { .. }
            | OrderMode::PushFrontArcPreview
            | OrderMode::FrontRebalanceSelectSource
            | OrderMode::FrontRebalanceDrag { .. }
            | OrderMode::ExpandAllPreview
    )
}

fn stop_order_ids(view: &MatchView, interaction: &InteractionState) -> BTreeSet<u64> {
    let is_stoppable = |order_id: &u64| view.retask_projection.active_order_ids.contains(order_id);
    let mut order_ids = interaction
        .supersede_order_ids()
        .into_iter()
        .filter(is_stoppable)
        .collect::<BTreeSet<_>>();
    for (&order_id, strength_by_cell) in &view.retask_projection.order_strength_by_cell {
        if is_stoppable(&order_id)
            && strength_by_cell
                .keys()
                .any(|coordinate| interaction.sources.contains(coordinate))
        {
            order_ids.insert(order_id);
        }
    }
    for (&order_id, source_cells) in &view.retask_projection.order_source_cells {
        if is_stoppable(&order_id) && !source_cells.is_disjoint(&interaction.sources) {
            order_ids.insert(order_id);
        }
    }
    order_ids
}

fn begin_stop_preview(interaction: &mut InteractionState, view: &mut MatchView) {
    if !matches!(interaction.mode, OrderMode::Idle) && !pointer_is_preview(&interaction.mode) {
        return;
    }
    let order_ids = stop_order_ids(view, interaction);
    if order_ids.is_empty() {
        view.show_toast(
            "Select cells intersecting active troops or active-front handles",
            ToastKind::Rejection,
        );
        return;
    }
    interaction.mode = OrderMode::StopPreview { order_ids };
    interaction.preview_key = None;
}

fn submit_current(
    interaction: &mut InteractionState,
    view: &mut MatchView,
    intents: &mut MessageWriter<ClientIntent>,
) {
    if pointer_is_preview(&interaction.mode)
        && interaction.preview_key != order_preview_key(view, interaction)
    {
        view.show_toast(
            "Preview changed · inspect it and confirm again",
            ToastKind::Info,
        );
        return;
    }
    if let Some(reason) = interaction.preview.invalid_reason {
        view.show_toast(reason, ToastKind::Rejection);
        return;
    }
    let Some((intent, label, return_mode)) = submission_request(interaction) else {
        if matches!(
            interaction.mode,
            OrderMode::PushFrontPreview { .. }
                | OrderMode::PushFrontArcPreview
                | OrderMode::ExpandAllPreview
        ) {
            let fallback = match &interaction.mode {
                OrderMode::PushFrontPreview { .. } => "The selection has no valid passable lane",
                OrderMode::PushFrontArcPreview => {
                    "No hostile contact fronts · drag P for a directional Push"
                }
                _ => "The selection has no passable neutral frontier",
            };
            view.show_toast(
                interaction.preview.invalid_reason.unwrap_or(fallback),
                ToastKind::Rejection,
            );
        }
        return;
    };
    interaction.return_after_rejection = Some(return_mode);
    interaction.submitting_command_id = None;
    interaction.mode = OrderMode::Submitting { _label: label };
    intents.write(intent);
}

fn begin_submission(
    interaction: &mut InteractionState,
    intents: &mut MessageWriter<ClientIntent>,
    intent: ClientIntent,
    label: &'static str,
    return_mode: OrderMode,
) {
    interaction.return_after_rejection = Some(return_mode);
    interaction.submitting_command_id = None;
    interaction.mode = OrderMode::Submitting { _label: label };
    intents.write(intent);
}

fn queue_contextual_submission(
    interaction: &mut InteractionState,
    view: &mut MatchView,
    intents: &mut MessageWriter<ClientIntent>,
    intent: ClientIntent,
    label: &'static str,
) -> bool {
    let Some(gesture) = ContextualGesture::from_intent(&intent) else {
        debug_assert!(
            false,
            "only contextual cluster commands may use the rapid queue"
        );
        return false;
    };
    match &mut interaction.contextual_submissions {
        Some(submissions) if submissions.gesture != gesture => {
            view.show_toast(
                "A different contextual command is still in flight · repeat the same click or wait",
                ToastKind::Info,
            );
            return false;
        }
        Some(submissions) if submissions.command_ids.len() >= MAX_CONTEXTUAL_COMMANDS_IN_FLIGHT => {
            view.show_toast(
                "Contextual command queue is full · wait for authority",
                ToastKind::Rejection,
            );
            return false;
        }
        Some(submissions) => submissions.command_ids.push_back(None),
        None => {
            interaction.contextual_submissions = Some(ContextualSubmissionGroup {
                gesture,
                command_ids: VecDeque::from([None]),
            });
        }
    }
    intents.write(intent);
    let count = interaction.contextual_in_flight_count();
    view.show_toast(
        format!(
            "{label} dispatched · {count} in flight · repeat the same click to layer another Share"
        ),
        ToastKind::Info,
    );
    true
}

fn submission_request(
    interaction: &InteractionState,
) -> Option<(ClientIntent, &'static str, OrderMode)> {
    let supersede_order_ids = interaction.supersede_order_ids();
    match &interaction.mode {
        OrderMode::AttackClustersPreview if !interaction.attack_targets.is_empty() => Some((
            ClientIntent::AttackClusters {
                sources: interaction.sources.clone(),
                targets: interaction.attack_targets.clone(),
                commitment_percent: interaction.amount_percent,
            },
            "ATTACK CLUSTERS",
            OrderMode::AttackClustersPreview,
        )),
        OrderMode::PushFrontPreview { direction }
            if !interaction.preview.front_edges.is_empty()
                && interaction.preview.invalid_reason.is_none() =>
        {
            Some((
                ClientIntent::PushFront {
                    sources: interaction.sources.clone(),
                    supersede_order_ids: supersede_order_ids.clone(),
                    direction: *direction,
                    commitment_percent: interaction.amount_percent,
                },
                "PUSH FRONT",
                OrderMode::PushFrontPreview {
                    direction: *direction,
                },
            ))
        }
        OrderMode::PushFrontArcPreview
            if !interaction.preview.front_edges.is_empty()
                && interaction.preview.invalid_reason.is_none() =>
        {
            Some((
                ClientIntent::PushFront {
                    sources: interaction.sources.clone(),
                    supersede_order_ids: supersede_order_ids.clone(),
                    direction: Axial::ZERO,
                    commitment_percent: interaction.amount_percent,
                },
                "CONTACT FRONTS",
                OrderMode::PushFrontArcPreview,
            ))
        }
        OrderMode::FrontRebalanceDrag {
            source_front_seed,
            target_front_seed: Some(target_front_seed),
        } if interaction.preview.invalid_reason.is_none() => Some((
            ClientIntent::FrontRebalance {
                source_component_cells: interaction.sources.clone(),
                source_front_seed: *source_front_seed,
                target_front_seed: *target_front_seed,
                commitment_percent: interaction.amount_percent,
                supersede_order_ids,
            },
            "FRONT REBALANCE",
            interaction.mode.clone(),
        )),
        OrderMode::ExpandAllPreview
            if !interaction.preview.front_edges.is_empty()
                && interaction.preview.invalid_reason.is_none() =>
        {
            Some((
                ClientIntent::ExpandAll {
                    sources: interaction.sources.clone(),
                    supersede_order_ids: supersede_order_ids.clone(),
                    commitment_percent: interaction.amount_percent,
                },
                "EXPAND PERIMETER",
                OrderMode::ExpandAllPreview,
            ))
        }
        OrderMode::ReshapePreview
            if !interaction.shape_targets.is_empty()
                && interaction.preview.invalid_reason.is_none() =>
        {
            Some((
                ClientIntent::Reshape {
                    sources: interaction.sources.clone(),
                    targets: interaction.shape_targets.clone(),
                    supersede_order_ids,
                },
                "RESHAPE",
                OrderMode::ReshapePreview,
            ))
        }
        OrderMode::StopPreview { order_ids } if !order_ids.is_empty() => Some((
            ClientIntent::CancelOrders {
                order_ids: order_ids.clone(),
            },
            "STOP ORDERS",
            OrderMode::StopPreview {
                order_ids: order_ids.clone(),
            },
        )),
        _ => None,
    }
}

fn paint_regions(
    keyboard: Res<ButtonInput<KeyCode>>,
    mouse: Res<ButtonInput<MouseButton>>,
    view: Res<MatchView>,
    mut interaction: ResMut<InteractionState>,
) {
    let drawing_shape = matches!(interaction.mode, OrderMode::ReshapeDrawing);
    let can_paint =
        drawing_shape && !keyboard.pressed(KeyCode::Space) && !mouse.pressed(MouseButton::Middle);
    if !can_paint {
        interaction.stroke = None;
        return;
    }

    if mouse.just_pressed(MouseButton::Left) && interaction.hovered.is_some() {
        let combine = selection_combine(&keyboard);
        if combine == SelectionCombine::Replace && !interaction.shape_targets.is_empty() {
            interaction.shape_targets.clear();
            interaction.shape_revision = interaction.shape_revision.wrapping_add(1);
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
        let footprint = centers
            .into_iter()
            .flat_map(|center| interaction.brush.cells(center))
            .collect::<BTreeSet<_>>();
        let mut changed = false;
        for coordinate in footprint {
            if !view.is_local_owned_passable(coordinate) {
                continue;
            }
            changed |= match stroke.operation {
                PaintOperation::Add => interaction.shape_targets.insert(coordinate),
                PaintOperation::Remove => interaction.shape_targets.remove(&coordinate),
            };
        }
        if changed {
            interaction.shape_revision = interaction.shape_revision.wrapping_add(1);
        }
    }

    if mouse.just_released(MouseButton::Left) {
        interaction.stroke = None;
        if !interaction.shape_targets.is_empty() {
            interaction.mode = OrderMode::ReshapePreview;
        }
    }
}

fn order_preview_key(view: &MatchView, interaction: &InteractionState) -> Option<OrderPreviewKey> {
    let mode = match &interaction.mode {
        OrderMode::Idle => PreviewModeKey::Idle,
        OrderMode::AttackClustersPreview => PreviewModeKey::AttackClusters,
        OrderMode::PushFrontOrient { .. } | OrderMode::PushFrontPreview { .. } => {
            PreviewModeKey::PushFront {
                direction: interaction.push_direction(),
            }
        }
        OrderMode::PushFrontArcPreview => PreviewModeKey::PushFront {
            direction: Some(Axial::ZERO),
        },
        OrderMode::FrontRebalanceSelectSource => PreviewModeKey::FrontRebalance {
            source_front_seed: None,
            target_front_seed: None,
        },
        OrderMode::FrontRebalanceDrag {
            source_front_seed,
            target_front_seed,
        } => PreviewModeKey::FrontRebalance {
            source_front_seed: Some(*source_front_seed),
            target_front_seed: *target_front_seed,
        },
        OrderMode::ExpandAllPreview => PreviewModeKey::ExpandAll,
        OrderMode::ReshapeDrawing | OrderMode::ReshapePreview => PreviewModeKey::Reshape,
        OrderMode::StopPreview { .. } => PreviewModeKey::Stop,
        OrderMode::Submitting { .. } => return None,
    };
    Some(OrderPreviewKey {
        source_revision: interaction.source_revision,
        share_percent: is_percentage_preview(&interaction.mode)
            .then_some(interaction.amount_percent),
        mode,
        shape_revision: interaction.shape_revision,
        attack_revision: interaction.attack_revision,
        state_revision: if matches!(interaction.mode, OrderMode::Idle) {
            view.ownership_revision
        } else {
            view.planning_revision
        },
        topology_revision: view.chunk_index_revision,
        retask_revision: view.retask_revision,
    })
}

#[cfg(test)]
fn build_push_front_preview(
    view: &MatchView,
    selected: &BTreeSet<Axial>,
    direction: Axial,
    commitment_percent: u8,
    preview: &mut OrderPreview,
) {
    let Ok(projection) = view.project_cluster_action_selection(selected, &BTreeSet::new()) else {
        preview.invalid_reason = Some("Push sources are no longer available");
        return;
    };
    build_projected_push_front_preview(view, &projection, direction, commitment_percent, preview);
}

fn build_projected_push_front_preview(
    view: &MatchView,
    projection: &ProjectedOrderSelection,
    direction: Axial,
    commitment_percent: u8,
    preview: &mut OrderPreview,
) {
    let selected = &projection.cells;
    if selected.len() > MAX_COMMAND_SELECTION_CELLS {
        preview.invalid_reason = Some("Push selection exceeds the 32,768-cell command limit");
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
        push_edge_is_eligible(view, source, target)
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
    let (mut routes, mut distance) =
        selected_reachability_to_front(view, &sources, &front_sources, direction);
    let target_by_boundary = edges
        .iter()
        .map(|edge| (edge.source, edge.target))
        .collect::<BTreeMap<_, _>>();
    for (&source, route) in &mut routes {
        let Some(&boundary) = route.last() else {
            continue;
        };
        if target_by_boundary
            .get(&boundary)
            .is_some_and(|target| view.is_local_owned(*target))
        {
            *route = vec![source, source + direction];
            distance.insert(source, 1);
        }
    }
    preview.excluded.extend(
        sources
            .iter()
            .filter(|source| !routes.contains_key(source))
            .copied(),
    );
    let reachable_sources = routes.keys().copied().collect::<BTreeSet<_>>();
    if reachable_sources.is_empty() {
        preview.invalid_reason = Some("No selected troops can reach an eligible front");
        return;
    }

    preview.front_edges = edges;
    let percentage = u64::from(commitment_percent.clamp(10, 100));
    preview.strength_upper_bound = reachable_sources
        .iter()
        .map(|coordinate| {
            projection
                .affected_strength_by_cell
                .get(coordinate)
                .copied()
                .unwrap_or(0)
                .saturating_mul(percentage)
                / 100
        })
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

    preview.component_routes = representative_component_routes(
        view,
        &reachable_sources,
        &routes,
        &preview.front_edges,
        &projection.affected_strength_by_cell,
    );
    preview.component_bottlenecks = preview
        .component_routes
        .iter()
        .filter_map(|route| {
            route
                .windows(2)
                .min_by_key(|edge| view.cell(edge[1]).map_or(0, |cell| cell.military_capacity))
                .map(|edge| (edge[0], edge[1]))
        })
        .collect();
    let max_distance = distance.values().copied().max().unwrap_or(0);
    let congestion = preview
        .strength_upper_bound
        .saturating_sub(preview.destination_capacity)
        / 20;
    preview.eta_seconds = (u64::from(max_distance.saturating_add(1)) * 2 + congestion)
        .min(u64::from(u32::MAX)) as u32;

    if let ServerUpdate::Accepted { patches, .. } =
        resolve_projected_push_front(view, projection, direction, commitment_percent)
    {
        for patch in patches {
            let before = view.cell(patch.coordinate).map_or(0, |cell| cell.infantry);
            let delta = i128::from(patch.infantry) - i128::from(before);
            if delta != 0 {
                preview.delta_by_cell.insert(patch.coordinate, delta);
            }
        }
    }
}

fn build_projected_arc_push_preview(
    view: &MatchView,
    projection: &ProjectedOrderSelection,
    commitment_percent: u8,
    preview: &mut OrderPreview,
) {
    let selected = &projection.cells;
    if selected.len() > MAX_COMMAND_SELECTION_CELLS {
        preview.invalid_reason = Some("Push selection exceeds the 32,768-cell command limit");
        return;
    }
    if let Some(invalid) = selected.iter().find(|coordinate| {
        view.cell(**coordinate).is_none_or(|cell| {
            !view.is_local_owned(**coordinate) || !cell.is_land() || cell.blocked
        })
    }) {
        preview.excluded.insert(*invalid);
        preview.invalid_reason = Some("Every selected source must be owned passable ground");
        return;
    }

    let routes = match arc_push_routes(view, selected) {
        Ok(routes) => routes,
        Err(reason) => {
            preview.invalid_reason = Some(reason);
            return;
        }
    };
    preview.excluded.extend(
        selected
            .iter()
            .filter(|source| !routes.contains_key(source))
            .copied(),
    );

    let percentage = u64::from(commitment_percent.clamp(10, 100));
    let requested_by_source = routes
        .keys()
        .map(|coordinate| {
            let amount = projection
                .affected_strength_by_cell
                .get(coordinate)
                .copied()
                .unwrap_or(0)
                .saturating_mul(percentage)
                / 100;
            (*coordinate, amount)
        })
        .collect::<BTreeMap<_, _>>();
    preview.strength_upper_bound = requested_by_source.values().copied().sum();
    if preview.strength_upper_bound == 0 {
        preview.invalid_reason =
            Some("Selected contact sources have no visible infantry to request");
        return;
    }
    preview.excluded.extend(
        requested_by_source
            .iter()
            .filter_map(|(&source, &amount)| (amount == 0).then_some(source)),
    );

    let mut front_edges = routes
        .iter()
        .filter(|(source, _)| requested_by_source[source] > 0)
        .map(|(_, route)| route.edge)
        .collect::<Vec<_>>();
    front_edges.sort_unstable_by_key(|edge| (edge.source, edge.target));
    front_edges.dedup_by_key(|edge| (edge.source, edge.target));
    let targets = front_edges
        .iter()
        .map(|edge| edge.target)
        .collect::<BTreeSet<_>>();
    preview.destination_capacity = targets
        .iter()
        .filter_map(|coordinate| view.cell(*coordinate))
        .map(|cell| cell.military_capacity)
        .fold(0_u64, u64::saturating_add);
    if preview.destination_capacity == 0 {
        preview.invalid_reason = Some("The hostile contact fronts have no military capacity");
        return;
    }
    preview.front_edges = front_edges;

    let reachable_sources = routes
        .keys()
        .filter(|source| requested_by_source[source] > 0)
        .copied()
        .collect::<BTreeSet<_>>();
    let route_cells = routes
        .iter()
        .filter(|(source, _)| requested_by_source[source] > 0)
        .map(|(&source, route)| (source, route.cells.clone()))
        .collect::<BTreeMap<_, _>>();
    preview.component_routes = representative_local_component_routes(
        view,
        &reachable_sources,
        &route_cells,
        &projection.affected_strength_by_cell,
    );
    preview.component_bottlenecks = preview
        .component_routes
        .iter()
        .filter_map(|route| {
            route
                .windows(2)
                .min_by_key(|edge| view.cell(edge[1]).map_or(0, |cell| cell.military_capacity))
                .map(|edge| (edge[0], edge[1]))
        })
        .collect();
    let max_distance = routes
        .iter()
        .filter(|(source, _)| requested_by_source[source] > 0)
        .map(|(_, route)| route)
        .map(|route| route.cells.len().saturating_sub(1))
        .max()
        .unwrap_or(0);
    let congestion = preview
        .strength_upper_bound
        .saturating_sub(preview.destination_capacity)
        / 20;
    preview.eta_seconds = u64::try_from(max_distance)
        .unwrap_or(u64::MAX)
        .saturating_mul(2)
        .saturating_add(congestion)
        .min(u64::from(u32::MAX)) as u32;

    if let ServerUpdate::Accepted { patches, .. } =
        resolve_projected_push_front(view, projection, Axial::ZERO, commitment_percent)
    {
        for patch in patches {
            let before = view.cell(patch.coordinate).map_or(0, |cell| cell.infantry);
            let delta = i128::from(patch.infantry) - i128::from(before);
            if delta != 0 {
                preview.delta_by_cell.insert(patch.coordinate, delta);
            }
        }
    }
}

#[cfg(test)]
fn build_expand_all_preview(
    view: &MatchView,
    selected: &BTreeSet<Axial>,
    commitment_percent: u8,
    preview: &mut OrderPreview,
) {
    let Ok(projection) = view.project_cluster_action_selection(selected, &BTreeSet::new()) else {
        preview.invalid_reason = Some("Expand Perimeter sources are no longer available");
        return;
    };
    build_projected_expand_all_preview(view, &projection, commitment_percent, preview);
}

fn build_projected_expand_all_preview(
    view: &MatchView,
    projection: &ProjectedOrderSelection,
    commitment_percent: u8,
    preview: &mut OrderPreview,
) {
    let selected = &projection.cells;
    if selected.len() > MAX_COMMAND_SELECTION_CELLS {
        preview.invalid_reason =
            Some("Expand Perimeter selection exceeds the 32,768-cell command limit");
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

    let forecast = match forecast_expand_wave(
        view,
        &sources,
        &projection.affected_strength_by_cell,
        commitment_percent,
        MAX_WAVE_PREVIEW_RINGS,
    ) {
        Ok(forecast) => forecast,
        Err(ExpandWaveError::Front(FrontSelectionError::EmptySelection)) => {
            preview.invalid_reason = Some("Select owned cells before expanding");
            return;
        }
        Err(ExpandWaveError::Front(FrontSelectionError::NoEligibleFront)) => {
            preview.invalid_reason = Some("The selection has no passable neutral frontier");
            return;
        }
        Err(ExpandWaveError::Front(FrontSelectionError::InvalidDirection)) => {
            preview.invalid_reason = Some("Expand Perimeter frontier is invalid");
            return;
        }
    };
    preview.front_edges = forecast.initial_edges;
    preview.wave_depth = forecast.reached_depth;
    preview.wave_truncated = forecast.truncated;
    preview.strength_upper_bound = forecast.strength_upper_bound;
    if preview.strength_upper_bound == 0 {
        preview.invalid_reason =
            Some("Eligible perimeter cells have no visible infantry to request");
        return;
    }
    preview.destination_capacity = forecast.first_ring_capacity;
    if preview.destination_capacity == 0 {
        preview.invalid_reason = Some("The neutral frontier has no military capacity");
        return;
    }
    let outside_depth = preview.wave_depth.values().copied().max().unwrap_or(0);
    preview.eta_seconds =
        u32::try_from(u64::from(outside_depth).saturating_mul(2)).unwrap_or(u32::MAX);
}

fn selected_reachability_to_front(
    view: &MatchView,
    sources: &BTreeSet<Axial>,
    front_sources: &BTreeSet<Axial>,
    direction: Axial,
) -> (BTreeMap<Axial, Vec<Axial>>, BTreeMap<Axial, u32>) {
    let routes = selected_directional_routes(sources, direction, front_sources, |from, to| {
        view.cell(from)
            .zip(view.cell(to))
            .is_some_and(|(from, to)| {
                from.is_land()
                    && to.is_land()
                    && !from.blocked
                    && !to.blocked
                    && (i32::from(from.elevation) - i32::from(to.elevation)).unsigned_abs()
                        <= u32::from(view.max_elevation_step)
            })
    });
    let mut distance = BTreeMap::new();
    for route in routes.values() {
        let route_distance = u32::try_from(route.len().saturating_sub(1)).unwrap_or(u32::MAX);
        for (index, coordinate) in route.iter().enumerate() {
            let remaining = route_distance.saturating_sub(index as u32);
            distance
                .entry(*coordinate)
                .and_modify(|known: &mut u32| *known = (*known).max(remaining))
                .or_insert(remaining);
        }
    }
    (routes, distance)
}

fn representative_component_routes(
    view: &MatchView,
    reachable_sources: &BTreeSet<Axial>,
    routes: &BTreeMap<Axial, Vec<Axial>>,
    front_edges: &[DirectedFrontEdge],
    affected_strength_by_cell: &BTreeMap<Axial, u64>,
) -> Vec<Vec<Axial>> {
    let front_target_by_source = front_edges
        .iter()
        .map(|edge| (edge.source, edge.target))
        .collect::<BTreeMap<_, _>>();
    let mut remaining = reachable_sources.clone();
    let mut representatives = Vec::new();

    while let Some(seed) = remaining.pop_first() {
        let mut pending = VecDeque::from([seed]);
        let mut component = BTreeSet::from([seed]);
        while let Some(current) = pending.pop_front() {
            for neighbor in current.neighbors() {
                if remaining.contains(&neighbor)
                    && view.is_local_traversable_edge(current, neighbor)
                {
                    remaining.remove(&neighbor);
                    component.insert(neighbor);
                    pending.push_back(neighbor);
                }
            }
        }

        let Some((_, route)) = component
            .iter()
            .filter(|source| affected_strength_by_cell.get(source).copied().unwrap_or(0) > 0)
            .filter_map(|source| routes.get(source).map(|route| (*source, route)))
            .max_by_key(|(source, route)| (route.len(), *source))
        else {
            continue;
        };
        let mut representative = route.clone();
        if let Some(target) = representative
            .last()
            .and_then(|source| front_target_by_source.get(source))
        {
            representative.push(*target);
        }
        representatives.push(representative);
    }

    representatives
}

fn representative_local_component_routes(
    view: &MatchView,
    reachable_sources: &BTreeSet<Axial>,
    routes: &BTreeMap<Axial, Vec<Axial>>,
    affected_strength_by_cell: &BTreeMap<Axial, u64>,
) -> Vec<Vec<Axial>> {
    let mut remaining = reachable_sources.clone();
    let mut representatives = Vec::new();
    while let Some(seed) = remaining.pop_first() {
        let mut pending = VecDeque::from([seed]);
        let mut component = BTreeSet::from([seed]);
        while let Some(current) = pending.pop_front() {
            for neighbor in current.neighbors() {
                if remaining.contains(&neighbor)
                    && view.is_local_traversable_edge(current, neighbor)
                {
                    remaining.remove(&neighbor);
                    component.insert(neighbor);
                    pending.push_back(neighbor);
                }
            }
        }
        if let Some(route) = component
            .iter()
            .filter(|source| affected_strength_by_cell.get(source).copied().unwrap_or(0) > 0)
            .filter_map(|source| routes.get(source).map(|route| (*source, route)))
            .max_by_key(|(source, route)| (route.len(), *source))
            .map(|(_, route)| route.clone())
        {
            representatives.push(route);
        }
    }
    representatives
}

const fn front_selection_error_text(error: FrontSelectionError) -> &'static str {
    match error {
        FrontSelectionError::EmptySelection => "Select owned cells before pushing",
        FrontSelectionError::InvalidDirection => "Push direction must match one hex direction",
        FrontSelectionError::NoEligibleFront => "No passable lane faces that direction",
    }
}

fn update_order_preview(view: Res<MatchView>, mut interaction: ResMut<InteractionState>) {
    rebuild_order_preview(&view, &mut interaction);
}

fn rebuild_order_preview(view: &MatchView, interaction: &mut InteractionState) {
    let Some(key) = order_preview_key(view, interaction) else {
        // Keep the accepted preview stable while an authoritative submission is
        // pending. Rejection restores its prior mode and key inputs.
        return;
    };
    if interaction.preview_key == Some(key) {
        return;
    }

    let mut preview = OrderPreview::default();
    if interaction.sources.len() > MAX_COMMAND_SELECTION_CELLS {
        preview.invalid_reason = Some("Selection exceeds the 32,768-cell command limit");
        interaction.preview = preview;
        interaction.preview_key = Some(key);
        return;
    }
    let supersede_order_ids = interaction.supersede_order_ids();
    if supersede_order_ids.len() > MAX_COMMAND_SUPERSEDE_ORDERS {
        preview.invalid_reason = Some("Retask selection exceeds the 32,768-order command limit");
        interaction.preview = preview;
        interaction.preview_key = Some(key);
        return;
    }
    let projection_result =
        view.project_order_selection(&interaction.sources, &supersede_order_ids);
    let projection = match projection_result {
        Ok(projection) => projection,
        Err(error) => {
            preview.invalid_reason = Some(projection_error_text(error));
            interaction.preview = preview;
            interaction.preview_key = Some(key);
            return;
        }
    };
    preview.projected_source_count = projection.cells.len();
    preview.projected_sources.clone_from(&projection.cells);
    preview.retask_handle_count = interaction.retask_handles.len();
    preview.retask_order_count = projection.superseded_order_count;
    preview.retask_strength = projection.superseded_strength;
    preview.projected_strength = projection.affected_strength_by_cell.values().copied().sum();
    preview.projected_capacity = projection
        .cells
        .iter()
        .map(|coordinate| {
            let capacity = view
                .cell(*coordinate)
                .map_or(0, |cell| cell.military_capacity);
            capacity.saturating_sub(
                projection
                    .unaffected_strength_by_cell
                    .get(coordinate)
                    .copied()
                    .unwrap_or(0),
            )
        })
        .sum();
    if projection.cells.len() > MAX_COMMAND_SELECTION_CELLS {
        preview.invalid_reason = Some("Selection exceeds the 32,768-cell command limit");
        interaction.preview = preview;
        interaction.preview_key = Some(key);
        return;
    }
    match &interaction.mode {
        OrderMode::AttackClustersPreview => build_attack_clusters_preview(
            view,
            &projection,
            &interaction.attack_targets,
            interaction.amount_percent,
            &mut preview,
        ),
        OrderMode::PushFrontOrient { .. } | OrderMode::PushFrontPreview { .. } => {
            if let Some(direction) = interaction.push_direction() {
                build_projected_push_front_preview(
                    view,
                    &projection,
                    direction,
                    interaction.amount_percent,
                    &mut preview,
                );
            } else {
                preview.invalid_reason = Some("Drag farther to choose one of six directions");
            }
        }
        OrderMode::PushFrontArcPreview => build_projected_arc_push_preview(
            view,
            &projection,
            interaction.amount_percent,
            &mut preview,
        ),
        OrderMode::FrontRebalanceSelectSource => {
            preview.projected_sources.clone_from(&projection.cells);
        }
        OrderMode::FrontRebalanceDrag {
            source_front_seed,
            target_front_seed,
        } => build_front_rebalance_preview(
            view,
            &projection.cells,
            *source_front_seed,
            *target_front_seed,
            interaction.amount_percent,
            &mut preview,
        ),
        OrderMode::ExpandAllPreview => build_projected_expand_all_preview(
            view,
            &projection,
            interaction.amount_percent,
            &mut preview,
        ),
        OrderMode::ReshapeDrawing | OrderMode::ReshapePreview => {
            build_projected_shape_preview(
                view,
                &projection,
                &interaction.shape_targets,
                &mut preview,
            );
        }
        OrderMode::StopPreview { order_ids } => build_stop_preview(view, order_ids, &mut preview),
        OrderMode::Idle => {}
        OrderMode::Submitting { .. } => unreachable!("submitting previews return before rebuild"),
    }
    interaction.preview = preview;
    interaction.preview_key = Some(key);
}

fn build_attack_clusters_preview(
    view: &MatchView,
    projection: &ProjectedOrderSelection,
    targets: &BTreeSet<Axial>,
    commitment_percent: u8,
    preview: &mut OrderPreview,
) {
    if targets.is_empty() {
        preview.invalid_reason = Some("Stage at least one enemy cluster");
        return;
    }
    let forecast = match forecast_attack_wave(view, &projection.cells, targets) {
        Ok(forecast) => forecast,
        Err(reason) => {
            preview.invalid_reason = Some(reason);
            return;
        }
    };
    preview.front_edges = forecast.initial_edges;
    preview.wave_depth = forecast.reached_depth;
    preview.strength_upper_bound = forecast
        .participating_sources
        .iter()
        .filter_map(|source| projection.affected_strength_by_cell.get(source).copied())
        .map(|strength| strength.saturating_mul(u64::from(commitment_percent.clamp(10, 100))) / 100)
        .fold(0_u64, u64::saturating_add);
    preview.destination_capacity = preview
        .front_edges
        .iter()
        .filter_map(|edge| view.cell(edge.target))
        .map(|cell| cell.military_capacity)
        .fold(0_u64, u64::saturating_add);
}

fn build_front_rebalance_preview(
    view: &MatchView,
    component: &BTreeSet<Axial>,
    source_front_seed: Axial,
    target_front_seed: Option<Axial>,
    commitment_percent: u8,
    preview: &mut OrderPreview,
) {
    preview.projected_sources.clone_from(component);
    preview.strength_upper_bound = component
        .iter()
        .filter_map(|coordinate| view.cell(*coordinate))
        .map(|cell| cell.infantry)
        .sum::<u64>()
        .saturating_mul(u64::from(commitment_percent.clamp(10, 100)))
        / 100;
    preview.eta_seconds = component.len() as u32 / 3 + 2;
    if let Ok(fronts) = strategic_fronts(component.iter().copied(), |_, target| {
        strategic_exterior_for_view(view, target)
    }) {
        if let Some(source_index) = strategic_front_index_for_seed(&fronts, source_front_seed) {
            for cell in fronts[source_index].source_cells() {
                preview.heatmap.insert(cell, 0.15);
            }
        }
        if let Some(target_front_seed) = target_front_seed
            && let Some(target_index) = strategic_front_index_for_seed(&fronts, target_front_seed)
        {
            for cell in fronts[target_index].source_cells() {
                preview.heatmap.insert(cell, 1.0);
            }
        }
    }
    if let Err(reason) =
        front_rebalance_seed_error(view, component, source_front_seed, target_front_seed)
    {
        preview.invalid_reason = Some(reason);
    }
}

fn build_stop_preview(view: &MatchView, order_ids: &BTreeSet<u64>, preview: &mut OrderPreview) {
    preview.stop_order_ids.clone_from(order_ids);
    preview.projected_sources.clear();
    for order_id in order_ids {
        if let Some(strength_by_cell) = view.retask_projection.order_strength_by_cell.get(order_id)
        {
            preview
                .projected_sources
                .extend(strength_by_cell.keys().copied());
        }
    }
    preview.projected_source_count = preview.projected_sources.len();
}

const fn projection_error_text(error: OrderSelectionProjectionError) -> &'static str {
    match error {
        OrderSelectionProjectionError::InvalidSource(_) => {
            "A selected source is no longer owned passable ground"
        }
        OrderSelectionProjectionError::StaleOrder(_) => {
            "A retask handle no longer references an active local order"
        }
        OrderSelectionProjectionError::UnknownPacketCell(_) => {
            "A retasked order references an unknown map cell"
        }
    }
}

fn build_projected_shape_preview(
    view: &MatchView,
    projection: &ProjectedOrderSelection,
    targets: &BTreeSet<Axial>,
    preview: &mut OrderPreview,
) {
    let shape = match projected_shape_distribution(view, projection, targets) {
        Ok(shape) => shape,
        Err(reason) => {
            preview.invalid_reason = Some(reason);
            return;
        }
    };
    preview.strength_upper_bound = shape.participating_strength;
    preview.destination_capacity = shape.destination_capacity;
    preview.reshape_destination_strength = shape.destination_strength;
    preview.reshape_outside_strength = shape.outside_strength;
    preview.excluded.extend(shape.excluded);
    for (coordinate, target) in shape.final_strength_by_cell {
        record_projected_strength(view, preview, coordinate, target);
    }
}

fn record_projected_strength(
    view: &MatchView,
    preview: &mut OrderPreview,
    coordinate: Axial,
    target: u64,
) {
    let (current, capacity) = view
        .cell(coordinate)
        .map_or((0, 0), |cell| (cell.infantry, cell.military_capacity));
    let delta = i128::from(target) - i128::from(current);
    if delta != 0 {
        preview.delta_by_cell.insert(coordinate, delta);
    }
    preview.heatmap.insert(
        coordinate,
        if capacity == 0 {
            0.0
        } else {
            target as f32 / capacity as f32
        },
    );
}
fn finish_submission(
    mut updates: MessageReader<ServerUpdate>,
    mut interaction: ResMut<InteractionState>,
) {
    for update in updates.read() {
        match update {
            ServerUpdate::SubmissionStarted { command_id } => {
                if assign_contextual_command_id(&mut interaction, *command_id) {
                    continue;
                }
                // `return_after_rejection` is the durable marker for a modal
                // command. Keep consuming its matching response even if a
                // presentation mode is corrupted, so it cannot be orphaned.
                if interaction.has_pending_submission()
                    && interaction.submitting_command_id.is_none()
                {
                    interaction.submitting_command_id = Some(*command_id);
                }
            }
            ServerUpdate::Accepted { command_id, .. }
                if finish_contextual_command(&mut interaction, *command_id) => {}
            ServerUpdate::Accepted { command_id, .. }
                if interaction.has_pending_submission()
                    && submission_matches(&interaction, *command_id) =>
            {
                interaction.mode = OrderMode::Idle;
                interaction.preview = OrderPreview::default();
                interaction.return_after_rejection = None;
                interaction.shape_targets.clear();
                interaction.shape_revision = interaction.shape_revision.wrapping_add(1);
                if !interaction.attack_targets.is_empty() {
                    interaction.attack_targets.clear();
                    interaction.attack_revision = interaction.attack_revision.wrapping_add(1);
                }
                interaction.submitting_command_id = None;
            }
            ServerUpdate::Rejected { command_id, .. }
                if finish_contextual_command(&mut interaction, *command_id) => {}
            ServerUpdate::Rejected { command_id, .. }
                if interaction.has_pending_submission()
                    && submission_matches(&interaction, *command_id) =>
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

fn assign_contextual_command_id(interaction: &mut InteractionState, command_id: u64) -> bool {
    let Some(command) = interaction
        .contextual_submissions
        .as_mut()
        .and_then(|submissions| {
            submissions
                .command_ids
                .iter_mut()
                .find(|command_id| command_id.is_none())
        })
    else {
        return false;
    };
    *command = Some(command_id);
    true
}

fn finish_contextual_command(interaction: &mut InteractionState, command_id: Option<u64>) -> bool {
    let Some(submissions) = interaction.contextual_submissions.as_mut() else {
        return false;
    };
    let index = command_id.map_or_else(
        || (!submissions.command_ids.is_empty()).then_some(0),
        |command_id| {
            submissions
                .command_ids
                .iter()
                .position(|pending| *pending == Some(command_id))
        },
    );
    let Some(index) = index else {
        return false;
    };
    submissions.command_ids.remove(index);
    if submissions.command_ids.is_empty() {
        interaction.contextual_submissions = None;
    }
    true
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
    use crate::model::{CellView, ContestedCellView, RetaskProjection};
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

    fn retask_preview_view() -> (MatchView, Axial, Axial, Axial) {
        let rear = Axial::ZERO;
        let front = Axial::new(1, 0);
        let handle = Axial::new(2, 0);
        let mut view = MatchView::connecting(1);
        for cell in [
            preview_cell(rear, Some(1), 100, 0),
            preview_cell(front, Some(1), 80, 0),
            preview_cell(handle, Some(2), 40, 0),
        ] {
            view.cells.insert(cell.coordinate, cell);
        }
        view.set_contested_cells(BTreeMap::from([(
            handle,
            ContestedCellView {
                controller_player: 2,
                attacker_player: 1,
                attacker_strength: 50,
                attacker_share: 50.0 / 90.0,
            },
        )]));
        view.set_retask_projection(RetaskProjection {
            handle_orders: BTreeMap::from([(handle, BTreeSet::from([7]))]),
            active_order_ids: BTreeSet::from([7, 8]),
            order_source_cells: BTreeMap::new(),
            order_strength_by_cell: BTreeMap::from([
                (7, BTreeMap::from([(rear, 30), (front, 20)])),
                (8, BTreeMap::from([(rear, 20)])),
            ]),
            active_strength_by_cell: BTreeMap::from([(rear, 50), (front, 20)]),
            destination_reservations_by_order: BTreeMap::new(),
            destination_claims_by_order: BTreeMap::new(),
        });
        view.rebuild_chunk_index();
        (view, rear, front, handle)
    }

    fn pressed(keys: impl IntoIterator<Item = KeyCode>) -> ButtonInput<KeyCode> {
        let mut input = ButtonInput::default();
        for key in keys {
            input.press(key);
        }
        input
    }

    fn order_input_app(view: MatchView, interaction: InteractionState) -> App {
        let mut app = App::new();
        app.add_message::<UiAction>()
            .add_message::<ClientIntent>()
            .insert_resource(ButtonInput::<KeyCode>::default())
            .insert_resource(ButtonInput::<MouseButton>::default())
            .insert_resource(view)
            .insert_resource(interaction)
            .add_systems(Update, process_order_input);
        app
    }

    fn mark_preview_current(view: &MatchView, interaction: &mut InteractionState) {
        interaction.preview_key = order_preview_key(view, interaction);
    }

    #[derive(Resource, Default)]
    struct CapturedIntents(Vec<ClientIntent>);

    fn capture_intents(
        mut intents: MessageReader<ClientIntent>,
        mut captured: ResMut<CapturedIntents>,
    ) {
        captured.0.extend(intents.read().cloned());
    }

    fn hex_disk(radius: i32) -> Vec<Axial> {
        (-radius..=radius)
            .flat_map(|q| (-radius..=radius).map(move |r| Axial::new(q, r)))
            .filter(|coordinate| coordinate.distance(Axial::ZERO) <= radius as u64)
            .collect()
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
    fn reshape_drawing_accepts_owned_targets_outside_the_source_footprint() {
        let source = Axial::ZERO;
        let corridor = Axial::new(1, 0);
        let target = Axial::new(2, 0);
        let mut view = MatchView::connecting(1);
        for coordinate in [source, corridor, target] {
            view.cells
                .insert(coordinate, preview_cell(coordinate, Some(1), 0, 0));
        }
        view.cell_mut(source).expect("source").infantry = 40;
        view.rebuild_chunk_index();
        let interaction = InteractionState {
            hovered: Some(target),
            sources: BTreeSet::from([source]),
            mode: OrderMode::ReshapeDrawing,
            ..Default::default()
        };
        let mut app = App::new();
        app.insert_resource(ButtonInput::<KeyCode>::default())
            .insert_resource(ButtonInput::<MouseButton>::default())
            .insert_resource(view)
            .insert_resource(interaction)
            .add_systems(Update, paint_regions);

        app.world_mut()
            .resource_mut::<ButtonInput<MouseButton>>()
            .press(MouseButton::Left);
        app.update();
        assert_eq!(
            app.world().resource::<InteractionState>().shape_targets,
            BTreeSet::from([target])
        );

        let mouse = &mut *app.world_mut().resource_mut::<ButtonInput<MouseButton>>();
        mouse.clear_just_pressed(MouseButton::Left);
        mouse.release(MouseButton::Left);
        app.update();
        assert!(matches!(
            app.world().resource::<InteractionState>().mode,
            OrderMode::ReshapePreview
        ));
    }

    #[test]
    fn idle_left_click_never_paints_a_subcluster_selection() {
        let selected = Axial::ZERO;
        let hovered = Axial::new(1, 0);
        let mut view = MatchView::connecting(1);
        for coordinate in [selected, hovered] {
            view.cells
                .insert(coordinate, preview_cell(coordinate, Some(1), 20, 0));
        }
        let interaction = InteractionState {
            hovered: Some(hovered),
            sources: BTreeSet::from([selected]),
            ..Default::default()
        };
        let mut app = App::new();
        app.insert_resource(ButtonInput::<KeyCode>::default())
            .insert_resource(ButtonInput::<MouseButton>::default())
            .insert_resource(view)
            .insert_resource(interaction)
            .add_systems(Update, paint_regions);
        app.world_mut()
            .resource_mut::<ButtonInput<MouseButton>>()
            .press(MouseButton::Left);

        app.update();

        let interaction = app.world().resource::<InteractionState>();
        assert_eq!(interaction.sources, BTreeSet::from([selected]));
        assert!(interaction.stroke.is_none());
        assert!(interaction.shape_targets.is_empty());
    }

    #[test]
    fn escape_during_a_held_reshape_stroke_does_not_repaint_the_source_selection() {
        let source = Axial::ZERO;
        let prior_target = Axial::new(1, 0);
        let hovered = Axial::new(2, 0);
        let mut view = MatchView::connecting(1);
        for coordinate in [source, prior_target, hovered] {
            view.cells
                .insert(coordinate, preview_cell(coordinate, Some(1), 20, 0));
        }
        let interaction = InteractionState {
            hovered: Some(hovered),
            sources: BTreeSet::from([source]),
            shape_targets: BTreeSet::from([prior_target]),
            mode: OrderMode::ReshapeDrawing,
            stroke: Some(PaintStroke {
                operation: PaintOperation::Add,
                last: Some(prior_target),
            }),
            ..Default::default()
        };
        let mut app = order_input_app(view, interaction);
        app.add_systems(Update, paint_regions.after(process_order_input));
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .press(KeyCode::Escape);
        let mouse = &mut *app.world_mut().resource_mut::<ButtonInput<MouseButton>>();
        mouse.press(MouseButton::Left);
        mouse.clear_just_pressed(MouseButton::Left);

        app.update();

        let interaction = app.world().resource::<InteractionState>();
        assert!(matches!(interaction.mode, OrderMode::Idle));
        assert_eq!(interaction.sources, BTreeSet::from([source]));
        assert!(interaction.shape_targets.is_empty());
        assert!(interaction.stroke.is_none());
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
        let reshape = OrderMode::ReshapeDrawing;
        let push = OrderMode::PushFrontPreview {
            direction: Axial::new(1, 0),
        };

        assert_eq!(
            requested_brush_resize(&pressed([KeyCode::BracketRight]), &idle),
            None,
            "plain brackets adjust Share while no reshape brush is active"
        );
        assert_eq!(
            requested_brush_resize(&pressed([KeyCode::ShiftLeft, KeyCode::BracketRight]), &idle),
            None
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
        assert_eq!(
            requested_brush_resize(&pressed([KeyCode::BracketRight]), &reshape),
            Some((BrushAxis::Both, true))
        );
        assert_eq!(
            requested_brush_resize(
                &pressed([KeyCode::ControlLeft, KeyCode::BracketLeft]),
                &reshape
            ),
            Some((BrushAxis::Height, false))
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

        let mut ring = SelectionBrush::default();
        ring.resize(BrushAxis::Both, true);
        ring.resize(BrushAxis::Width, false);
        assert_eq!(
            (ring.width(), ring.height(), ring.rings()),
            (1, 3, 0),
            "width shrink must remain effective after symmetric ring growth"
        );

        let mut ring = SelectionBrush::default();
        ring.resize(BrushAxis::Both, true);
        ring.resize(BrushAxis::Height, false);
        assert_eq!(
            (ring.width(), ring.height(), ring.rings()),
            (3, 1, 0),
            "height shrink must remain effective after symmetric ring growth"
        );

        let mut two_rings = SelectionBrush::default();
        two_rings.resize(BrushAxis::Both, true);
        two_rings.resize(BrushAxis::Both, true);
        two_rings.resize(BrushAxis::Width, false);
        assert_eq!(
            (two_rings.width(), two_rings.height(), two_rings.rings()),
            (3, 5, 1),
            "axis shrink must preserve the other visible extent"
        );
    }

    #[test]
    fn cluster_selection_crosses_empty_owned_corridors() {
        let seed = Axial::ZERO;
        let empty_corridor = Axial::new(1, 0);
        let far_troops = Axial::new(2, 0);
        let mut view = MatchView::connecting(1);
        view.cells.insert(seed, preview_cell(seed, Some(1), 20, 0));
        view.cells
            .insert(empty_corridor, preview_cell(empty_corridor, Some(1), 0, 0));
        view.cells
            .insert(far_troops, preview_cell(far_troops, Some(1), 30, 0));

        let cluster = local_owned_cluster(&view, seed);
        assert_eq!(cluster, BTreeSet::from([seed, empty_corridor, far_troops]));
    }

    #[test]
    fn cluster_selection_splits_at_cliffs_blocked_ground_and_water() {
        let seed = Axial::ZERO;
        let cliff = Axial::new(1, 0);
        let blocked = Axial::new(0, 1);
        let water = Axial::new(-1, 1);
        let mut view = MatchView::connecting(1);
        view.cells.insert(seed, preview_cell(seed, Some(1), 20, 0));
        view.cells
            .insert(cliff, preview_cell(cliff, Some(1), 20, 3));
        let mut blocked_cell = preview_cell(blocked, Some(1), 20, 0);
        blocked_cell.blocked = true;
        view.cells.insert(blocked, blocked_cell);
        let mut water_cell = preview_cell(water, Some(1), 20, 0);
        water_cell.terrain = TerrainKind::Water;
        view.cells.insert(water, water_cell);

        assert_eq!(local_owned_cluster(&view, seed), BTreeSet::from([seed]));
    }

    #[test]
    fn cluster_key_uses_the_last_map_hover_after_pointer_leaves_map() {
        let seed = Axial::ZERO;
        let connected = Axial::new(1, 0);
        let disconnected = Axial::new(3, 0);
        let mut view = MatchView::connecting(1);
        for coordinate in [seed, connected, disconnected] {
            view.cells
                .insert(coordinate, preview_cell(coordinate, Some(1), 20, 0));
        }
        view.rebuild_chunk_index();
        let interaction = InteractionState {
            hovered: None,
            last_map_hovered: Some(seed),
            ..Default::default()
        };
        let mut app = order_input_app(view, interaction);
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .press(KeyCode::KeyC);

        app.update();

        assert_eq!(
            app.world().resource::<InteractionState>().sources,
            BTreeSet::from([seed, connected])
        );
    }

    #[test]
    fn reconciliation_prunes_finished_retask_handles_without_rebinding_them() {
        let (mut view, _rear, _front, handle) = retask_preview_view();
        let mut sources = BTreeSet::new();
        let mut handles = BTreeMap::from([(handle, BTreeSet::from([7]))]);

        let mut refreshed = view.retask_projection.clone();
        refreshed.handle_orders.insert(handle, BTreeSet::from([8]));
        refreshed.active_order_ids = BTreeSet::from([7, 8]);
        view.set_retask_projection(refreshed);
        assert!(!reconcile_selection_sets(&view, &mut sources, &mut handles));
        assert_eq!(handles[&handle], BTreeSet::from([7]));

        let mut finished = view.retask_projection.clone();
        finished.active_order_ids = BTreeSet::from([8]);
        view.set_retask_projection(finished);
        assert!(reconcile_selection_sets(&view, &mut sources, &mut handles));
        assert!(handles.is_empty());
        assert!(!sources.contains(&handle));
    }

    #[test]
    fn captured_retask_handle_stays_a_ghost_until_its_snapshotted_order_finishes() {
        let (mut view, _rear, _front, handle) = retask_preview_view();
        let mut sources = BTreeSet::new();
        let mut handles = BTreeMap::from([(handle, BTreeSet::from([7]))]);
        view.cell_mut(handle).expect("handle cell").owner = Some(1);
        view.contested_cells.remove(&handle);

        assert!(!reconcile_selection_sets(&view, &mut sources, &mut handles));
        assert_eq!(handles[&handle], BTreeSet::from([7]));
        assert!(!sources.contains(&handle));
    }

    #[test]
    fn select_all_includes_every_owned_passable_cluster_even_when_empty() {
        let troops = Axial::ZERO;
        let empty_corridor = Axial::new(1, 0);
        let empty_component = Axial::new(5, 0);
        let mut view = MatchView::connecting(1);
        view.cells
            .insert(troops, preview_cell(troops, Some(1), 20, 0));
        view.cells
            .insert(empty_corridor, preview_cell(empty_corridor, Some(1), 0, 0));
        view.cells.insert(
            empty_component,
            preview_cell(empty_component, Some(1), 0, 0),
        );

        assert_eq!(
            all_owned_passable_cells(&view),
            BTreeSet::from([troops, empty_corridor, empty_component])
        );
    }

    #[test]
    fn select_all_does_not_select_or_supersede_active_internal_orders() {
        let source = Axial::ZERO;
        let packet = Axial::new(1, 0);
        let mut view = MatchView::connecting(1);
        view.cells
            .insert(source, preview_cell(source, Some(1), 20, 0));
        view.cells
            .insert(packet, preview_cell(packet, Some(1), 15, 0));
        view.set_retask_projection(RetaskProjection {
            active_order_ids: BTreeSet::from([77]),
            order_strength_by_cell: BTreeMap::from([(77, BTreeMap::from([(packet, 15)]))]),
            active_strength_by_cell: BTreeMap::from([(packet, 15)]),
            ..Default::default()
        });
        assert!(view.retask_projection.handle_orders.is_empty());
        let mut interaction = InteractionState {
            retask_handles: BTreeMap::from([(packet, BTreeSet::from([77]))]),
            ..Default::default()
        };

        assert!(select_all_clusters(&mut view, &mut interaction));

        assert_eq!(interaction.sources, BTreeSet::from([source, packet]));
        assert!(interaction.retask_handles.is_empty());
        assert!(interaction.supersede_order_ids().is_empty());
    }

    #[test]
    fn cluster_selection_does_not_select_or_supersede_intersecting_orders() {
        let seed = Axial::ZERO;
        let local_packet = Axial::new(1, 0);
        let remote_packet = Axial::new(5, 0);
        let mut view = MatchView::connecting(1);
        for (coordinate, infantry) in [(seed, 20), (local_packet, 10), (remote_packet, 12)] {
            view.cells
                .insert(coordinate, preview_cell(coordinate, Some(1), infantry, 0));
        }
        view.set_retask_projection(RetaskProjection {
            active_order_ids: BTreeSet::from([7]),
            order_strength_by_cell: BTreeMap::from([(
                7,
                BTreeMap::from([(local_packet, 10), (remote_packet, 12)]),
            )]),
            active_strength_by_cell: BTreeMap::from([(local_packet, 10), (remote_packet, 12)]),
            ..Default::default()
        });
        let mut interaction = InteractionState {
            retask_handles: BTreeMap::from([(local_packet, BTreeSet::from([7]))]),
            ..Default::default()
        };

        assert!(select_cluster(
            &mut view,
            &mut interaction,
            seed,
            SelectionCombine::Replace,
        ));

        assert_eq!(interaction.sources, BTreeSet::from([seed, local_packet]));
        assert!(interaction.retask_handles.is_empty());
        assert!(interaction.supersede_order_ids().is_empty());
        let projection = view
            .project_order_selection(&interaction.sources, &interaction.supersede_order_ids())
            .expect("cluster selection projects around active allocations");
        assert!(!projection.cells.contains(&remote_packet));
        assert_eq!(projection.affected_strength_by_cell[&local_packet], 0);
    }

    #[test]
    fn cluster_combine_modes_replace_add_and_remove_whole_components() {
        let first = Axial::ZERO;
        let first_neighbor = Axial::new(1, 0);
        let second = Axial::new(5, 0);
        let mut view = MatchView::connecting(1);
        for coordinate in [first, first_neighbor, second] {
            view.cells
                .insert(coordinate, preview_cell(coordinate, Some(1), 10, 0));
        }
        let mut interaction = InteractionState::default();

        assert!(select_cluster(
            &mut view,
            &mut interaction,
            first,
            SelectionCombine::Replace,
        ));
        assert_eq!(interaction.sources, BTreeSet::from([first, first_neighbor]));
        assert!(interaction.retask_handles.is_empty());

        assert!(select_cluster(
            &mut view,
            &mut interaction,
            second,
            SelectionCombine::Add,
        ));
        assert_eq!(
            interaction.sources,
            BTreeSet::from([first, first_neighbor, second])
        );
        assert!(interaction.retask_handles.is_empty());

        assert!(select_cluster(
            &mut view,
            &mut interaction,
            first,
            SelectionCombine::Remove,
        ));
        assert_eq!(interaction.sources, BTreeSet::from([second]));
        assert!(interaction.retask_handles.is_empty());
    }

    #[test]
    fn reshape_requires_exactly_one_current_owned_cluster() {
        let first = Axial::ZERO;
        let second = Axial::new(5, 0);
        let view = || {
            let mut view = MatchView::connecting(1);
            for coordinate in [first, second] {
                view.cells
                    .insert(coordinate, preview_cell(coordinate, Some(1), 10, 0));
            }
            view
        };

        let mut multiple = order_input_app(
            view(),
            InteractionState {
                sources: BTreeSet::from([first, second]),
                ..Default::default()
            },
        );
        multiple
            .world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .press(KeyCode::KeyT);
        multiple.update();
        assert!(matches!(
            multiple.world().resource::<InteractionState>().mode,
            OrderMode::Idle
        ));
        assert_eq!(
            multiple
                .world()
                .resource::<MatchView>()
                .toast
                .as_ref()
                .map(|toast| toast.text.as_str()),
            Some("Reshape works with exactly one selected cluster")
        );

        let mut single = order_input_app(
            view(),
            InteractionState {
                sources: BTreeSet::from([first]),
                ..Default::default()
            },
        );
        single
            .world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .press(KeyCode::KeyT);
        single.update();
        assert!(matches!(
            single.world().resource::<InteractionState>().mode,
            OrderMode::ReshapeDrawing
        ));
    }

    #[test]
    fn reconciliation_absorbs_growth_and_merges_while_retaining_split_children() {
        let west = Axial::ZERO;
        let bridge = Axial::new(1, 0);
        let east = Axial::new(2, 0);
        let mut view = MatchView::connecting(1);
        for coordinate in [west, bridge, east] {
            view.cells
                .insert(coordinate, preview_cell(coordinate, Some(1), 0, 0));
        }
        let mut sources = BTreeSet::from([west]);
        let mut handles = BTreeMap::new();

        assert!(reconcile_selection_sets(&view, &mut sources, &mut handles));
        assert_eq!(sources, BTreeSet::from([west, bridge, east]));

        view.cell_mut(bridge).expect("bridge").owner = None;
        assert!(reconcile_selection_sets(&view, &mut sources, &mut handles));
        assert_eq!(sources, BTreeSet::from([west, east]));
    }

    #[test]
    fn oversized_select_all_is_atomic() {
        let mut view = MatchView::connecting(1);
        for index in 0..=MAX_COMMAND_SELECTION_CELLS {
            let coordinate = Axial::new(i32::try_from(index).expect("test index fits i32"), 0);
            view.cells.insert(
                coordinate,
                preview_cell(coordinate, Some(1), u64::from(index == 0), 0),
            );
        }
        let original = BTreeSet::from([Axial::ZERO]);
        let mut interaction = InteractionState {
            sources: original.clone(),
            ..Default::default()
        };

        assert!(!select_all_clusters(&mut view, &mut interaction));
        assert_eq!(interaction.sources, original);
        assert!(interaction.retask_handles.is_empty());
        assert_eq!(
            view.toast.as_ref().map(|toast| toast.text.as_str()),
            Some("Selection would exceed the 32,768-cell command limit")
        );
    }

    #[test]
    fn oversized_order_snapshot_is_atomic() {
        let source = Axial::ZERO;
        let mut view = MatchView::connecting(1);
        view.cells
            .insert(source, preview_cell(source, Some(1), 10, 0));
        let mut interaction = InteractionState {
            sources: BTreeSet::from([source]),
            ..Default::default()
        };
        let handles = BTreeMap::from([(
            source,
            (0..=u64::try_from(MAX_COMMAND_SUPERSEDE_ORDERS).expect("limit fits u64")).collect(),
        )]);

        assert!(!commit_selection_candidate(
            &mut view,
            &mut interaction,
            BTreeSet::from([source]),
            handles,
        ));
        assert_eq!(interaction.sources, BTreeSet::from([source]));
        assert!(interaction.retask_handles.is_empty());
        assert_eq!(
            view.toast.as_ref().map(|toast| toast.text.as_str()),
            Some("Selection would exceed the 32,768-order command limit")
        );
    }

    #[test]
    fn largest_supported_selection_preset_fits_the_command_cap() {
        let largest_preset_cells = std::hint::black_box(21_484_usize);
        assert_eq!(MAX_COMMAND_SELECTION_CELLS, 32_768);
        assert!(largest_preset_cells <= MAX_COMMAND_SELECTION_CELLS);
    }

    #[test]
    fn push_direction_quantizes_to_each_exact_hex_axis() {
        for expected in Axial::DIRECTIONS {
            let plane = axial_to_plane(expected);
            assert_eq!(quantize_world_direction(plane * 3.0), Some(expected));
        }
        assert_eq!(quantize_world_direction(Vec2::ZERO), None);

        let mut interaction = InteractionState {
            cursor_screen: Some(Vec2::new(PUSH_DRAG_THRESHOLD_PIXELS - 1.0, 0.0)),
            push_start_screen: Some(Vec2::ZERO),
            mode: OrderMode::PushFrontOrient {
                start: Vec3::ZERO,
                current: Vec3::new(30.0, 0.0, 0.0),
            },
            ..Default::default()
        };
        assert_eq!(
            interaction.push_direction(),
            None,
            "large world-space jitter remains a tap below the pixel threshold"
        );
        interaction.cursor_screen = Some(Vec2::new(PUSH_DRAG_THRESHOLD_PIXELS + 1.0, 0.0));
        let east = axial_to_plane(Axial::new(1, 0)).normalize() * 0.01;
        interaction.mode = OrderMode::PushFrontOrient {
            start: Vec3::ZERO,
            current: Vec3::new(east.x, 0.0, east.y),
        };
        assert_eq!(
            interaction.push_direction(),
            Some(Axial::new(1, 0)),
            "a visible pixel drag resolves even when zoom makes its world delta small"
        );
    }

    #[test]
    fn plain_neutral_and_enemy_clicks_dispatch_contextual_cluster_commands() {
        let source = Axial::ZERO;
        let neutral = Axial::new(-1, 0);
        let enemy_a = Axial::new(1, 0);
        let enemy_b = Axial::new(2, 0);
        let mut view = MatchView::connecting(1);
        view.cells
            .insert(source, preview_cell(source, Some(1), 40, 0));
        view.cells
            .insert(neutral, preview_cell(neutral, None, 0, 0));
        view.cells
            .insert(enemy_a, preview_cell(enemy_a, Some(2), 20, 0));
        view.cells
            .insert(enemy_b, preview_cell(enemy_b, Some(2), 20, 0));

        let mut expand_app = order_input_app(
            view,
            InteractionState {
                hovered: Some(neutral),
                sources: BTreeSet::from([source]),
                amount_percent: 70,
                ..Default::default()
            },
        );
        expand_app
            .init_resource::<CapturedIntents>()
            .add_systems(Update, capture_intents.after(process_order_input));
        expand_app
            .world_mut()
            .resource_mut::<ButtonInput<MouseButton>>()
            .press(MouseButton::Left);
        expand_app.update();
        assert!(matches!(
            expand_app.world().resource::<CapturedIntents>().0.as_slice(),
            [ClientIntent::ExpandClusters {
                sources,
                focus,
                commitment_percent: 70,
            }] if sources == &BTreeSet::from([source]) && *focus == neutral
        ));

        let view = expand_app
            .world_mut()
            .remove_resource::<MatchView>()
            .unwrap();
        let mut attack_app = order_input_app(
            view,
            InteractionState {
                hovered: Some(enemy_a),
                sources: BTreeSet::from([source]),
                amount_percent: 60,
                ..Default::default()
            },
        );
        attack_app
            .init_resource::<CapturedIntents>()
            .add_systems(Update, capture_intents.after(process_order_input));
        attack_app
            .world_mut()
            .resource_mut::<ButtonInput<MouseButton>>()
            .press(MouseButton::Left);
        attack_app.update();
        assert!(matches!(
            attack_app.world().resource::<CapturedIntents>().0.as_slice(),
            [ClientIntent::AttackClusters {
                sources,
                targets,
                commitment_percent: 60,
            }] if sources == &BTreeSet::from([source])
                && targets == &BTreeSet::from([enemy_a, enemy_b])
        ));
    }

    #[test]
    fn rapid_exact_contextual_clicks_queue_independent_expand_and_attack_intents() {
        let source = Axial::ZERO;
        let neutral = Axial::new(-1, 0);
        let other_neutral = Axial::new(0, -1);
        let enemy = Axial::new(1, 0);
        let enemy_tail = Axial::new(2, 0);
        let view = || {
            let mut view = MatchView::connecting(1);
            for cell in [
                preview_cell(source, Some(1), 100, 0),
                preview_cell(neutral, None, 0, 0),
                preview_cell(other_neutral, None, 0, 0),
                preview_cell(enemy, Some(2), 20, 0),
                preview_cell(enemy_tail, Some(2), 20, 0),
            ] {
                view.cells.insert(cell.coordinate, cell);
            }
            view
        };

        let mut expand = order_input_app(
            view(),
            InteractionState {
                hovered: Some(neutral),
                sources: BTreeSet::from([source]),
                amount_percent: 10,
                ..Default::default()
            },
        );
        expand
            .init_resource::<CapturedIntents>()
            .add_systems(Update, capture_intents.after(process_order_input));
        for _ in 0..2 {
            *expand
                .world_mut()
                .resource_mut::<ButtonInput<MouseButton>>() = ButtonInput::default();
            expand
                .world_mut()
                .resource_mut::<ButtonInput<MouseButton>>()
                .press(MouseButton::Left);
            expand.update();
        }
        assert_eq!(
            expand
                .world()
                .resource::<InteractionState>()
                .contextual_in_flight_count(),
            2
        );
        assert!(matches!(
            expand.world().resource::<CapturedIntents>().0.as_slice(),
            [
                ClientIntent::ExpandClusters {
                    sources: first_sources,
                    focus: first_focus,
                    commitment_percent: 10,
                },
                ClientIntent::ExpandClusters {
                    sources: second_sources,
                    focus: second_focus,
                    commitment_percent: 10,
                },
            ] if first_sources == second_sources
                && first_sources == &BTreeSet::from([source])
                && *first_focus == neutral
                && *second_focus == neutral
        ));

        expand
            .world_mut()
            .resource_mut::<InteractionState>()
            .hovered = Some(other_neutral);
        *expand
            .world_mut()
            .resource_mut::<ButtonInput<MouseButton>>() = ButtonInput::default();
        expand
            .world_mut()
            .resource_mut::<ButtonInput<MouseButton>>()
            .press(MouseButton::Left);
        expand.update();
        assert_eq!(
            expand.world().resource::<CapturedIntents>().0.len(),
            2,
            "a different contextual gesture must not become an accidental replay"
        );
        assert!(
            expand
                .world()
                .resource::<MatchView>()
                .toast
                .as_ref()
                .is_some_and(|toast| toast.text.contains("different contextual command"))
        );

        let mut attack = order_input_app(
            view(),
            InteractionState {
                hovered: Some(enemy),
                sources: BTreeSet::from([source]),
                amount_percent: 10,
                ..Default::default()
            },
        );
        attack
            .init_resource::<CapturedIntents>()
            .add_systems(Update, capture_intents.after(process_order_input));
        for _ in 0..2 {
            *attack
                .world_mut()
                .resource_mut::<ButtonInput<MouseButton>>() = ButtonInput::default();
            attack
                .world_mut()
                .resource_mut::<ButtonInput<MouseButton>>()
                .press(MouseButton::Left);
            attack.update();
        }
        assert_eq!(
            attack
                .world()
                .resource::<InteractionState>()
                .contextual_in_flight_count(),
            2
        );
        assert!(matches!(
            attack.world().resource::<CapturedIntents>().0.as_slice(),
            [
                ClientIntent::AttackClusters {
                    sources: first_sources,
                    targets: first_targets,
                    commitment_percent: 10,
                },
                ClientIntent::AttackClusters {
                    sources: second_sources,
                    targets: second_targets,
                    commitment_percent: 10,
                },
            ] if first_sources == second_sources
                && first_sources == &BTreeSet::from([source])
                && first_targets == second_targets
                && first_targets == &BTreeSet::from([enemy, enemy_tail])
        ));
    }

    #[test]
    fn contextual_receipts_track_distinct_ids_out_of_order_without_clearing_selection() {
        let source = Axial::ZERO;
        let gesture = ContextualGesture::Expand {
            sources: BTreeSet::from([source]),
            focus: Axial::new(1, 0),
            commitment_percent: 10,
        };
        let interaction = InteractionState {
            sources: BTreeSet::from([source]),
            contextual_submissions: Some(ContextualSubmissionGroup {
                gesture,
                command_ids: VecDeque::from([None, None]),
            }),
            ..Default::default()
        };
        let mut app = App::new();
        app.add_message::<ServerUpdate>()
            .insert_resource(interaction)
            .add_systems(Update, finish_submission);
        app.world_mut()
            .write_message(ServerUpdate::SubmissionStarted { command_id: 41 });
        app.world_mut()
            .write_message(ServerUpdate::SubmissionStarted { command_id: 42 });
        app.update();

        assert_eq!(
            app.world()
                .resource::<InteractionState>()
                .contextual_submissions
                .as_ref()
                .map(|submissions| submissions.command_ids.clone()),
            Some(VecDeque::from([Some(41), Some(42)]))
        );

        app.world_mut().write_message(ServerUpdate::Rejected {
            command_id: Some(42),
            reason: "second rejected".to_owned(),
            relevant_cell: None,
        });
        app.world_mut().write_message(ServerUpdate::Accepted {
            command_id: Some(41),
            summary: "first accepted".to_owned(),
            patches: Vec::new(),
            flow: None,
            front: None,
        });
        app.update();

        let interaction = app.world().resource::<InteractionState>();
        assert_eq!(interaction.contextual_in_flight_count(), 0);
        assert_eq!(interaction.sources, BTreeSet::from([source]));
        assert!(matches!(interaction.mode, OrderMode::Idle));
    }

    #[test]
    fn staged_multiple_contacted_enemy_clusters_toggle_and_submit_as_one_union() {
        let source = Axial::ZERO;
        let second_source = Axial::new(0, 2);
        let first = Axial::new(1, 0);
        let first_tail = Axial::new(2, 0);
        let second = Axial::new(0, 3);
        let mut view = MatchView::connecting(1);
        for source in [source, second_source] {
            view.cells
                .insert(source, preview_cell(source, Some(1), 40, 0));
        }
        for target in [first, first_tail, second] {
            view.cells
                .insert(target, preview_cell(target, Some(2), 20, 0));
        }
        let mut app = order_input_app(
            view,
            InteractionState {
                hovered: Some(first),
                sources: BTreeSet::from([source, second_source]),
                amount_percent: 80,
                ..Default::default()
            },
        );
        app.init_resource::<CapturedIntents>()
            .add_systems(Update, capture_intents.after(process_order_input));

        for target in [first, second] {
            app.world_mut().resource_mut::<InteractionState>().hovered = Some(target);
            *app.world_mut().resource_mut::<ButtonInput<KeyCode>>() = pressed([KeyCode::ShiftLeft]);
            *app.world_mut().resource_mut::<ButtonInput<MouseButton>>() = ButtonInput::default();
            app.world_mut()
                .resource_mut::<ButtonInput<MouseButton>>()
                .press(MouseButton::Left);
            app.update();
        }
        assert!(app.world().resource::<CapturedIntents>().0.is_empty());
        assert_eq!(
            app.world().resource::<InteractionState>().attack_targets,
            BTreeSet::from([first, first_tail, second])
        );

        app.world_mut().resource_mut::<InteractionState>().hovered = Some(first);
        *app.world_mut().resource_mut::<ButtonInput<KeyCode>>() = pressed([KeyCode::ControlLeft]);
        *app.world_mut().resource_mut::<ButtonInput<MouseButton>>() = ButtonInput::default();
        app.world_mut()
            .resource_mut::<ButtonInput<MouseButton>>()
            .press(MouseButton::Left);
        app.update();
        assert_eq!(
            app.world().resource::<InteractionState>().attack_targets,
            BTreeSet::from([second])
        );

        app.world_mut().resource_mut::<InteractionState>().hovered = Some(first);
        *app.world_mut().resource_mut::<ButtonInput<KeyCode>>() = ButtonInput::default();
        *app.world_mut().resource_mut::<ButtonInput<MouseButton>>() = ButtonInput::default();
        app.world_mut()
            .resource_mut::<ButtonInput<MouseButton>>()
            .press(MouseButton::Left);
        app.update();

        assert!(matches!(
            app.world().resource::<InteractionState>().mode,
            OrderMode::Idle
        ));
        assert_eq!(
            app.world()
                .resource::<InteractionState>()
                .contextual_in_flight_count(),
            1
        );
        assert!(
            app.world()
                .resource::<InteractionState>()
                .attack_targets
                .is_empty()
        );
        assert!(matches!(
            app.world().resource::<CapturedIntents>().0.as_slice(),
            [ClientIntent::AttackClusters {
                sources,
                targets,
                commitment_percent: 80,
            }] if sources == &BTreeSet::from([source, second_source])
                && targets == &BTreeSet::from([first, first_tail, second])
        ));
    }

    #[test]
    fn staged_attack_rejects_a_contacted_and_remote_target_union_before_emitting() {
        let source = Axial::ZERO;
        let contacted = Axial::new(1, 0);
        let remote = Axial::new(4, 0);
        let mut view = MatchView::connecting(1);
        view.cells
            .insert(source, preview_cell(source, Some(1), 40, 0));
        for target in [contacted, remote] {
            view.cells
                .insert(target, preview_cell(target, Some(2), 20, 0));
        }
        let mut app = order_input_app(
            view,
            InteractionState {
                sources: BTreeSet::from([source]),
                amount_percent: 80,
                ..Default::default()
            },
        );
        app.init_resource::<CapturedIntents>()
            .add_systems(Update, update_order_preview.after(process_order_input))
            .add_systems(Update, capture_intents.after(process_order_input));

        for target in [contacted, remote] {
            app.world_mut().resource_mut::<InteractionState>().hovered = Some(target);
            *app.world_mut().resource_mut::<ButtonInput<KeyCode>>() = pressed([KeyCode::ShiftLeft]);
            *app.world_mut().resource_mut::<ButtonInput<MouseButton>>() = ButtonInput::default();
            app.world_mut()
                .resource_mut::<ButtonInput<MouseButton>>()
                .press(MouseButton::Left);
            app.update();
        }

        let interaction = app.world().resource::<InteractionState>();
        assert_eq!(
            interaction.preview.invalid_reason,
            Some("Every targeted enemy cluster must share a passable front with the selection")
        );
        assert!(matches!(interaction.mode, OrderMode::AttackClustersPreview));
        assert!(app.world().resource::<CapturedIntents>().0.is_empty());

        *app.world_mut().resource_mut::<ButtonInput<KeyCode>>() = pressed([KeyCode::Enter]);
        *app.world_mut().resource_mut::<ButtonInput<MouseButton>>() = ButtonInput::default();
        app.update();

        assert!(app.world().resource::<CapturedIntents>().0.is_empty());
        assert!(matches!(
            app.world().resource::<InteractionState>().mode,
            OrderMode::AttackClustersPreview
        ));
    }

    #[test]
    fn plain_remote_enemy_click_is_rejected_locally_without_losing_quick_dispatch() {
        let source = Axial::ZERO;
        let remote = Axial::new(4, 0);
        let mut view = MatchView::connecting(1);
        view.cells
            .insert(source, preview_cell(source, Some(1), 40, 0));
        view.cells
            .insert(remote, preview_cell(remote, Some(2), 20, 0));
        let mut app = order_input_app(
            view,
            InteractionState {
                hovered: Some(remote),
                sources: BTreeSet::from([source]),
                amount_percent: 60,
                ..Default::default()
            },
        );
        app.init_resource::<CapturedIntents>()
            .add_systems(Update, capture_intents.after(process_order_input));
        app.world_mut()
            .resource_mut::<ButtonInput<MouseButton>>()
            .press(MouseButton::Left);

        app.update();

        assert!(app.world().resource::<CapturedIntents>().0.is_empty());
        let interaction = app.world().resource::<InteractionState>();
        assert_eq!(
            interaction.preview.invalid_reason,
            Some("Selected source and enemy clusters share no passable front")
        );
        assert!(matches!(interaction.mode, OrderMode::AttackClustersPreview));
    }

    #[test]
    fn staged_attack_enter_submits_and_escape_cancels_without_losing_sources() {
        let source = Axial::ZERO;
        let target = Axial::new(1, 0);
        let view = || {
            let mut view = MatchView::connecting(1);
            view.cells
                .insert(source, preview_cell(source, Some(1), 40, 0));
            view.cells
                .insert(target, preview_cell(target, Some(2), 20, 0));
            view
        };
        let staged = || InteractionState {
            sources: BTreeSet::from([source]),
            attack_targets: BTreeSet::from([target]),
            mode: OrderMode::AttackClustersPreview,
            amount_percent: 90,
            ..Default::default()
        };

        let mut submit = order_input_app(view(), staged());
        submit
            .init_resource::<CapturedIntents>()
            .add_systems(Update, update_order_preview.after(process_order_input))
            .add_systems(Update, capture_intents.after(process_order_input));
        submit.update();
        submit
            .world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .press(KeyCode::Enter);
        submit.update();
        assert!(matches!(
            submit.world().resource::<CapturedIntents>().0.as_slice(),
            [ClientIntent::AttackClusters {
                sources,
                targets,
                commitment_percent: 90,
            }] if sources == &BTreeSet::from([source]) && targets == &BTreeSet::from([target])
        ));

        let mut cancel = order_input_app(view(), staged());
        cancel
            .world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .press(KeyCode::Escape);
        cancel.update();
        let interaction = cancel.world().resource::<InteractionState>();
        assert!(matches!(interaction.mode, OrderMode::Idle));
        assert_eq!(interaction.sources, BTreeSet::from([source]));
        assert!(interaction.attack_targets.is_empty());
    }

    #[test]
    fn staged_attack_waits_for_its_current_preview_key_before_submitting() {
        let source = Axial::ZERO;
        let target = Axial::new(1, 0);
        let mut view = MatchView::connecting(1);
        view.cells
            .insert(source, preview_cell(source, Some(1), 40, 0));
        view.cells
            .insert(target, preview_cell(target, Some(2), 20, 0));
        let mut app = order_input_app(
            view,
            InteractionState {
                sources: BTreeSet::from([source]),
                attack_targets: BTreeSet::from([target]),
                mode: OrderMode::AttackClustersPreview,
                amount_percent: 90,
                // Deliberately leave `preview_key` stale. The generic preview
                // freshness gate must apply to Attack as it does to every
                // other previewed command.
                ..Default::default()
            },
        );
        app.init_resource::<CapturedIntents>()
            .add_systems(Update, capture_intents.after(process_order_input));
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .press(KeyCode::Enter);

        app.update();

        assert!(app.world().resource::<CapturedIntents>().0.is_empty());
        assert!(matches!(
            app.world().resource::<InteractionState>().mode,
            OrderMode::AttackClustersPreview
        ));
    }

    #[test]
    fn arc_push_submission_uses_the_zero_direction_sentinel() {
        let source = Axial::ZERO;
        let target = Axial::new(1, 0);
        let interaction = InteractionState {
            sources: BTreeSet::from([source]),
            mode: OrderMode::PushFrontArcPreview,
            preview: OrderPreview {
                front_edges: vec![DirectedFrontEdge { source, target }],
                ..Default::default()
            },
            ..Default::default()
        };

        let (intent, label, return_mode) = submission_request(&interaction).expect("arc request");
        assert_eq!(label, "CONTACT FRONTS");
        assert!(matches!(return_mode, OrderMode::PushFrontArcPreview));
        assert!(matches!(
            intent,
            ClientIntent::PushFront { direction, .. } if direction == Axial::ZERO
        ));
    }

    #[test]
    fn idle_preview_and_selection_reconcile_ignore_infantry_only_changes() {
        let mut view = MatchView::connecting(1);
        let interaction = InteractionState::default();
        let idle_key = order_preview_key(&view, &interaction).expect("idle key");
        let mut reconcile_cache = SelectionReconcileCache::default();
        reconcile_cache.record(&view, &interaction);

        view.planning_revision = view.planning_revision.wrapping_add(1);
        assert_eq!(
            order_preview_key(&view, &interaction).expect("infantry-only idle key"),
            idle_key
        );
        assert!(reconcile_cache.is_current(&view, &interaction));

        view.ownership_revision = view.ownership_revision.wrapping_add(1);
        assert_ne!(
            order_preview_key(&view, &interaction).expect("ownership idle key"),
            idle_key
        );
        assert!(!reconcile_cache.is_current(&view, &interaction));
    }

    #[test]
    fn contact_preview_converges_around_a_hostile_pocket_without_touching_neutral_exterior() {
        let pocket = Axial::ZERO;
        let sources = pocket.neighbors().into_iter().collect::<BTreeSet<_>>();
        let exterior = Axial::new(2, 0);
        let mut view = MatchView::connecting(1);
        for source in &sources {
            view.cells
                .insert(*source, preview_cell(*source, Some(1), 10, 0));
        }
        view.cells
            .insert(pocket, preview_cell(pocket, Some(2), 20, 0));
        view.cells
            .insert(exterior, preview_cell(exterior, None, 0, 0));
        let projection = view
            .project_order_selection(&sources, &BTreeSet::new())
            .expect("owned pocket ring");
        let mut preview = OrderPreview::default();

        build_projected_arc_push_preview(&view, &projection, 50, &mut preview);

        assert_eq!(preview.invalid_reason, None);
        assert_eq!(preview.strength_upper_bound, 30);
        assert_eq!(preview.front_edges.len(), 6);
        assert!(preview.front_edges.iter().all(|edge| edge.target == pocket));
        assert!(
            !preview
                .front_edges
                .iter()
                .any(|edge| edge.target == exterior)
        );
        assert_eq!(
            preview
                .front_edges
                .iter()
                .map(|edge| edge.target - edge.source)
                .collect::<BTreeSet<_>>(),
            Axial::DIRECTIONS.into_iter().collect()
        );
    }

    #[test]
    fn contact_preview_omits_edges_whose_source_share_rounds_to_zero() {
        let zero_source = Axial::ZERO;
        let active_source = Axial::new(0, 1);
        let zero_target = Axial::new(-1, 0);
        let active_target = Axial::new(1, 1);
        let sources = BTreeSet::from([zero_source, active_source]);
        let mut view = MatchView::connecting(1);
        view.cells
            .insert(zero_source, preview_cell(zero_source, Some(1), 1, 0));
        view.cells
            .insert(active_source, preview_cell(active_source, Some(1), 20, 0));
        view.cells
            .insert(zero_target, preview_cell(zero_target, Some(2), 10, 0));
        view.cells
            .insert(active_target, preview_cell(active_target, Some(2), 10, 0));
        let projection = view
            .project_order_selection(&sources, &BTreeSet::new())
            .expect("owned contact sources");
        let mut preview = OrderPreview::default();

        build_projected_arc_push_preview(&view, &projection, 50, &mut preview);

        assert_eq!(preview.invalid_reason, None);
        assert_eq!(preview.strength_upper_bound, 10);
        assert_eq!(
            preview.front_edges,
            vec![DirectedFrontEdge {
                source: active_source,
                target: active_target,
            }]
        );
        assert_eq!(preview.destination_capacity, 100);
        assert!(preview.excluded.contains(&zero_source));
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
            preview.component_routes,
            vec![vec![
                Axial::ZERO,
                Axial::new(1, 0),
                Axial::new(2, 0),
                Axial::new(3, 0),
            ]]
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
    fn push_preview_does_not_route_a_blocked_lane_sideways() {
        let direction = Axial::new(1, 0);
        let sources = BTreeSet::from([Axial::new(0, -1), Axial::ZERO, Axial::new(0, 1)]);
        let upper_target = Axial::new(1, -1);
        let blocked_gap = Axial::new(1, 0);
        let lower_target = Axial::new(1, 1);
        let mut view = MatchView::connecting(1);
        for &source in &sources {
            view.cells
                .insert(source, preview_cell(source, Some(1), 20, 0));
        }
        view.cells
            .insert(upper_target, preview_cell(upper_target, None, 0, 0));
        let mut blocked_target = preview_cell(blocked_gap, Some(1), 0, 0);
        blocked_target.blocked = true;
        view.cells.insert(blocked_gap, blocked_target);
        view.cells
            .insert(lower_target, preview_cell(lower_target, None, 0, 0));
        view.rebuild_chunk_index();

        let mut preview = OrderPreview::default();
        build_push_front_preview(&view, &sources, direction, 50, &mut preview);

        assert_eq!(preview.invalid_reason, None);
        assert_eq!(
            preview.front_edges,
            vec![
                DirectedFrontEdge {
                    source: Axial::new(0, -1),
                    target: upper_target,
                },
                DirectedFrontEdge {
                    source: Axial::new(0, 1),
                    target: lower_target,
                },
            ]
        );
        assert_eq!(preview.strength_upper_bound, 20);
        assert_eq!(preview.destination_capacity, 200);
        assert_eq!(preview.excluded, BTreeSet::from([Axial::ZERO]));
    }

    #[test]
    fn push_preview_draws_one_route_per_disconnected_component() {
        let direction = Axial::new(1, 0);
        let sources = BTreeSet::from([
            Axial::new(0, 0),
            Axial::new(1, 0),
            Axial::new(0, 3),
            Axial::new(1, 3),
        ]);
        let mut view = MatchView::connecting(1);
        for source in &sources {
            view.cells
                .insert(*source, preview_cell(*source, Some(1), 20, 0));
        }
        for target in [Axial::new(2, 0), Axial::new(2, 3)] {
            view.cells.insert(target, preview_cell(target, None, 0, 0));
        }
        view.rebuild_chunk_index();

        let mut preview = OrderPreview::default();
        build_push_front_preview(&view, &sources, direction, 50, &mut preview);

        assert_eq!(preview.invalid_reason, None);
        assert_eq!(
            preview.component_routes,
            vec![
                vec![Axial::new(0, 0), Axial::new(1, 0), Axial::new(2, 0)],
                vec![Axial::new(0, 3), Axial::new(1, 3), Axial::new(2, 3)],
            ]
        );
        assert_eq!(preview.component_bottlenecks.len(), 2);
    }

    #[test]
    fn push_preview_accepts_non_capturable_friendly_endpoints_on_all_six_axes() {
        for direction in Axial::DIRECTIONS {
            let source = Axial::ZERO;
            let target = source + direction;
            let mut view = MatchView::connecting(1);
            view.cells
                .insert(source, preview_cell(source, Some(1), 40, 0));
            view.cells
                .insert(target, preview_cell(target, Some(1), 10, 0));
            view.non_capturable_cells.insert(target);
            view.rebuild_chunk_index();

            let mut preview = OrderPreview::default();
            build_push_front_preview(
                &view,
                &BTreeSet::from([source]),
                direction,
                50,
                &mut preview,
            );

            assert_eq!(preview.invalid_reason, None, "axis {direction:?}");
            assert_eq!(
                preview.front_edges,
                vec![DirectedFrontEdge { source, target }]
            );
            assert_eq!(preview.component_routes, vec![vec![source, target]]);
            assert_eq!(preview.strength_upper_bound, 20);
        }
    }

    #[test]
    fn push_preview_reports_a_generic_error_when_every_facing_lane_is_blocked() {
        let source = Axial::ZERO;
        let target = Axial::new(1, 0);
        let mut view = MatchView::connecting(1);
        view.cells
            .insert(source, preview_cell(source, Some(1), 40, 0));
        let mut blocked = preview_cell(target, Some(1), 0, 0);
        blocked.blocked = true;
        view.cells.insert(target, blocked);
        view.rebuild_chunk_index();

        let mut preview = OrderPreview::default();
        build_push_front_preview(
            &view,
            &BTreeSet::from([source]),
            Axial::new(1, 0),
            50,
            &mut preview,
        );

        assert_eq!(
            preview.invalid_reason,
            Some("No passable lane faces that direction")
        );
    }

    #[test]
    fn push_preview_excludes_sources_split_from_the_front_by_a_cliff() {
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

        assert_eq!(preview.invalid_reason, None);
        assert_eq!(
            preview.front_edges,
            vec![DirectedFrontEdge {
                source: Axial::new(2, 0),
                target: Axial::new(3, 0),
            }]
        );
        assert_eq!(preview.strength_upper_bound, 15);
        assert_eq!(
            preview.excluded,
            BTreeSet::from([Axial::ZERO, Axial::new(1, 0)])
        );
    }

    #[test]
    fn push_preview_uses_authoritative_slope_and_capturability_constraints() {
        let source = Axial::ZERO;
        let target = Axial::new(1, 0);
        let sources = BTreeSet::from([source]);
        let mut view = MatchView::connecting(1);
        view.cells
            .insert(source, preview_cell(source, Some(1), 40, 0));
        view.cells.insert(target, preview_cell(target, None, 0, 2));
        view.max_elevation_step = 2;
        view.rebuild_chunk_index();

        let mut permitted = OrderPreview::default();
        build_push_front_preview(&view, &sources, Axial::new(1, 0), 50, &mut permitted);
        assert_eq!(permitted.invalid_reason, None);
        assert_eq!(permitted.front_edges.len(), 1);

        view.max_elevation_step = 1;
        let mut cliff = OrderPreview::default();
        build_push_front_preview(&view, &sources, Axial::new(1, 0), 50, &mut cliff);
        assert!(cliff.front_edges.is_empty());
        assert!(cliff.invalid_reason.is_some());

        view.max_elevation_step = 2;
        view.non_capturable_cells.insert(target);
        let mut protected = OrderPreview::default();
        build_push_front_preview(&view, &sources, Axial::new(1, 0), 50, &mut protected);
        assert!(protected.front_edges.is_empty());
        assert!(protected.invalid_reason.is_some());
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
        assert_eq!(preview.wave_depth.len(), 5);
        assert!(preview.wave_depth.values().all(|depth| *depth == 1));
        assert!(preview.component_routes.is_empty());
    }

    #[test]
    fn expand_all_preview_merges_shared_targets_into_one_wave_cell() {
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
        assert_eq!(preview.front_edges.len(), 2);
        assert!(
            preview
                .front_edges
                .iter()
                .all(|edge| edge.target == shared_target)
        );
        assert_eq!(preview.wave_depth, BTreeMap::from([(shared_target, 1)]));
        assert_eq!(preview.strength_upper_bound, 20);
    }

    #[test]
    fn expand_all_preview_draws_offset_rings_instead_of_a_route_spoke() {
        let source = Axial::ZERO;
        let mut view = MatchView::connecting(1);
        view.cells
            .insert(source, preview_cell(source, Some(1), 100, 0));
        for coordinate in hex_disk(2).into_iter().filter(|cell| *cell != source) {
            view.cells
                .insert(coordinate, preview_cell(coordinate, None, 0, 0));
        }
        view.rebuild_chunk_index();

        let mut preview = OrderPreview::default();
        build_expand_all_preview(&view, &BTreeSet::from([source]), 100, &mut preview);

        assert_eq!(preview.invalid_reason, None);
        assert_eq!(
            preview
                .wave_depth
                .values()
                .filter(|depth| **depth == 1)
                .count(),
            6
        );
        assert_eq!(
            preview
                .wave_depth
                .values()
                .filter(|depth| **depth == 2)
                .count(),
            12
        );
        assert!(preview.component_routes.is_empty());
        assert!(preview.component_bottlenecks.is_empty());
    }

    #[test]
    fn expand_all_preview_keeps_the_first_perimeter_continuous_at_low_strength() {
        let source = Axial::ZERO;
        let mut view = MatchView::connecting(1);
        view.cells
            .insert(source, preview_cell(source, Some(1), 10, 0));
        for direction in Axial::DIRECTIONS {
            let coordinate = source + direction;
            view.cells
                .insert(coordinate, preview_cell(coordinate, None, 0, 0));
        }
        view.rebuild_chunk_index();

        let mut preview = OrderPreview::default();
        build_expand_all_preview(&view, &BTreeSet::from([source]), 10, &mut preview);

        assert_eq!(preview.strength_upper_bound, 1);
        assert_eq!(preview.wave_depth.len(), 6);
        assert!(preview.wave_depth.values().all(|depth| *depth == 1));
    }

    #[test]
    fn expand_all_preview_does_not_pull_strength_from_a_deep_seed() {
        let selected = hex_disk(2).into_iter().collect::<BTreeSet<_>>();
        let mut view = MatchView::connecting(1);
        for coordinate in hex_disk(3) {
            let owner = selected.contains(&coordinate).then_some(1);
            let infantry = u64::from(coordinate == Axial::ZERO) * 180;
            view.cells
                .insert(coordinate, preview_cell(coordinate, owner, infantry, 0));
        }
        view.rebuild_chunk_index();

        let mut preview = OrderPreview::default();
        build_expand_all_preview(&view, &selected, 100, &mut preview);

        assert_eq!(
            preview.invalid_reason,
            Some("Eligible perimeter cells have no visible infantry to request")
        );
        assert!(preview.wave_depth.is_empty());
        assert_eq!(preview.eta_seconds, 0);
    }

    #[test]
    fn expand_all_preview_supports_disconnected_source_regions() {
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

        assert_eq!(preview.invalid_reason, None);
        assert_eq!(preview.front_edges.len(), 2);
        assert_eq!(preview.strength_upper_bound, 20);
        assert!(preview.excluded.is_empty());
    }

    #[test]
    fn map_click_waits_when_share_changes_after_the_displayed_preview() {
        let source = Axial::ZERO;
        let target = Axial::new(1, 0);
        let mut view = MatchView::connecting(1);
        view.cells
            .insert(source, preview_cell(source, Some(1), 100, 0));
        view.cells.insert(target, preview_cell(target, None, 0, 0));
        let mut interaction = InteractionState {
            hovered: Some(target),
            sources: BTreeSet::from([source]),
            mode: OrderMode::PushFrontPreview {
                direction: Axial::new(1, 0),
            },
            preview: OrderPreview {
                front_edges: vec![DirectedFrontEdge { source, target }],
                ..Default::default()
            },
            ..Default::default()
        };
        mark_preview_current(&view, &mut interaction);
        let mut app = order_input_app(view, interaction);
        app.init_resource::<CapturedIntents>()
            .add_systems(Update, update_order_preview.after(process_order_input))
            .add_systems(Update, capture_intents.after(process_order_input));
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .press(KeyCode::BracketRight);
        app.world_mut()
            .resource_mut::<ButtonInput<MouseButton>>()
            .press(MouseButton::Left);

        app.update();

        let interaction = app.world().resource::<InteractionState>();
        assert_eq!(interaction.amount_percent, 60);
        assert!(matches!(
            interaction.mode,
            OrderMode::PushFrontPreview { .. }
        ));
        assert!(app.world().resource::<CapturedIntents>().0.is_empty());

        *app.world_mut().resource_mut::<ButtonInput<KeyCode>>() = ButtonInput::default();
        *app.world_mut().resource_mut::<ButtonInput<MouseButton>>() = ButtonInput::default();
        app.world_mut()
            .resource_mut::<ButtonInput<MouseButton>>()
            .press(MouseButton::Left);
        app.update();

        assert!(matches!(
            app.world().resource::<InteractionState>().mode,
            OrderMode::Submitting { .. }
        ));
        let captured = &app.world().resource::<CapturedIntents>().0;
        assert_eq!(captured.len(), 1);
        assert!(matches!(
            &captured[0],
            ClientIntent::PushFront {
                commitment_percent: 60,
                ..
            }
        ));
    }

    #[test]
    fn reshape_preview_shows_target_gain_and_outside_remainder_at_capacity() {
        let source = Axial::ZERO;
        let target = Axial::new(1, 0);
        let mut view = MatchView::connecting(1);
        view.cells
            .insert(source, preview_cell(source, Some(1), 80, 0));
        let mut constrained_target = preview_cell(target, Some(1), 0, 0);
        constrained_target.military_capacity = 10;
        view.cells.insert(target, constrained_target);
        view.rebuild_chunk_index();
        let projection = view
            .project_order_selection(&BTreeSet::from([source]), &BTreeSet::new())
            .expect("owned source");
        let mut preview = OrderPreview::default();

        build_projected_shape_preview(&view, &projection, &BTreeSet::from([target]), &mut preview);

        assert_eq!(preview.invalid_reason, None);
        assert_eq!(preview.reshape_destination_strength, 10);
        assert_eq!(preview.reshape_outside_strength, 70);
        assert_eq!(preview.destination_capacity, 10);
        assert_eq!(
            preview.delta_by_cell,
            BTreeMap::from([(source, -10), (target, 10)])
        );
        assert!(preview.heatmap.contains_key(&source));
        assert!(preview.heatmap.contains_key(&target));
    }

    #[test]
    fn reshape_preview_contracts_a_large_selection_best_effort() {
        let sources = (0..12).map(|q| Axial::new(q, 0)).collect::<BTreeSet<_>>();
        let targets = BTreeSet::from([Axial::new(12, 0), Axial::new(13, 0)]);
        let mut view = MatchView::connecting(1);
        for &source in &sources {
            view.cells
                .insert(source, preview_cell(source, Some(1), 40, 0));
        }
        for &target in &targets {
            view.cells
                .insert(target, preview_cell(target, Some(1), 0, 0));
        }
        view.rebuild_chunk_index();
        let projection = view
            .project_order_selection(&sources, &BTreeSet::new())
            .expect("owned screenshot-scale source section");
        let mut preview = OrderPreview::default();

        build_projected_shape_preview(&view, &projection, &targets, &mut preview);

        assert_eq!(preview.invalid_reason, None);
        assert_eq!(preview.strength_upper_bound, 480);
        assert_eq!(preview.reshape_destination_strength, 200);
        assert_eq!(preview.reshape_outside_strength, 280);
        assert_eq!(preview.destination_capacity, 200);
        assert!(
            targets
                .iter()
                .all(|target| preview.delta_by_cell.get(target) == Some(&100))
        );
        assert!(sources.iter().any(|source| {
            preview
                .delta_by_cell
                .get(source)
                .is_some_and(|delta| *delta < 0)
        }));
        assert!(
            sources
                .iter()
                .all(|source| preview.heatmap.contains_key(source))
        );
    }

    #[test]
    fn reshape_preview_skips_unreachable_source_components_without_vetoing_moves() {
        let local_source = Axial::ZERO;
        let local_target = Axial::new(1, 0);
        let stranded_source = Axial::new(8, 0);
        let mut view = MatchView::connecting(1);
        view.cells
            .insert(local_source, preview_cell(local_source, Some(1), 40, 0));
        view.cells
            .insert(local_target, preview_cell(local_target, Some(1), 0, 0));
        view.cells.insert(
            stranded_source,
            preview_cell(stranded_source, Some(1), 30, 0),
        );
        view.rebuild_chunk_index();
        let projection = view
            .project_order_selection(
                &BTreeSet::from([local_source, stranded_source]),
                &BTreeSet::new(),
            )
            .expect("owned disconnected sources");
        let mut preview = OrderPreview::default();

        build_projected_shape_preview(
            &view,
            &projection,
            &BTreeSet::from([local_target]),
            &mut preview,
        );

        assert_eq!(preview.invalid_reason, None);
        assert_eq!(preview.strength_upper_bound, 70);
        assert_eq!(preview.reshape_destination_strength, 40);
        assert_eq!(preview.reshape_outside_strength, 30);
        assert_eq!(
            preview.delta_by_cell,
            BTreeMap::from([(local_source, -40), (local_target, 40)])
        );
        assert_eq!(preview.excluded, BTreeSet::from([stranded_source]));
        assert!(!preview.heatmap.contains_key(&stranded_source));
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
    fn stop_preview_freezes_current_packet_intersection_ids() {
        let (mut view, rear, front, handle) = retask_preview_view();
        let mut interaction = InteractionState {
            sources: BTreeSet::from([front]),
            retask_handles: BTreeMap::from([(handle, BTreeSet::from([7]))]),
            ..Default::default()
        };
        begin_stop_preview(&mut interaction, &mut view);

        let OrderMode::StopPreview { order_ids } = &interaction.mode else {
            panic!("X should enter StopPreview");
        };
        assert_eq!(order_ids, &BTreeSet::from([7]));
        view.retask_projection.active_order_ids.insert(9);
        view.retask_projection
            .order_strength_by_cell
            .insert(9, BTreeMap::from([(front, 1)]));

        let (intent, _, _) = submission_request(&interaction).expect("stop request");
        assert!(matches!(
            intent,
            ClientIntent::CancelOrders { order_ids } if order_ids == BTreeSet::from([7])
        ));
        assert_eq!(
            stop_order_ids(
                &view,
                &InteractionState {
                    sources: BTreeSet::from([rear]),
                    ..Default::default()
                }
            ),
            BTreeSet::from([7, 8])
        );
    }

    #[test]
    fn stop_preview_finds_an_action_by_its_launch_cluster_after_packets_leave() {
        let source = Axial::ZERO;
        let advanced_packet = Axial::new(4, 0);
        let mut view = MatchView::connecting(1);
        view.cells
            .insert(source, preview_cell(source, Some(1), 20, 0));
        view.cells.insert(
            advanced_packet,
            preview_cell(advanced_packet, Some(2), 15, 0),
        );
        view.set_retask_projection(RetaskProjection {
            active_order_ids: BTreeSet::from([77]),
            order_source_cells: BTreeMap::from([(77, BTreeSet::from([source]))]),
            order_strength_by_cell: BTreeMap::from([(77, BTreeMap::from([(advanced_packet, 15)]))]),
            active_strength_by_cell: BTreeMap::from([(advanced_packet, 15)]),
            ..Default::default()
        });
        let mut interaction = InteractionState {
            sources: BTreeSet::from([source]),
            ..Default::default()
        };

        begin_stop_preview(&mut interaction, &mut view);

        assert!(matches!(
            interaction.mode,
            OrderMode::StopPreview { ref order_ids } if order_ids == &BTreeSet::from([77])
        ));
    }

    #[test]
    fn rejected_commands_restore_expand_and_arc_previews() {
        let interaction = InteractionState {
            mode: OrderMode::Submitting {
                _label: "STOP EXPAND PERIMETER",
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

        let interaction = InteractionState {
            mode: OrderMode::Submitting {
                _label: "CONTACT FRONTS",
            },
            return_after_rejection: Some(OrderMode::PushFrontArcPreview),
            submitting_command_id: Some(42),
            ..Default::default()
        };
        let mut app = App::new();
        app.add_message::<ServerUpdate>()
            .insert_resource(interaction)
            .add_systems(Update, finish_submission);
        app.world_mut().write_message(ServerUpdate::Rejected {
            command_id: Some(42),
            reason: "contact changed".to_owned(),
            relevant_cell: None,
        });
        app.update();

        let interaction = app.world().resource::<InteractionState>();
        assert!(matches!(interaction.mode, OrderMode::PushFrontArcPreview));
        assert_eq!(interaction.submitting_command_id, None);
    }
}
