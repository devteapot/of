use bevy::{
    app::AppExit,
    picking::hover::Hovered,
    prelude::*,
    ui::UiRect,
    ui_widgets::{
        Slider, SliderDragState, SliderRange, SliderThumb, SliderValue, TrackClick, ValueChange,
    },
};

use crate::{
    interaction::{InteractionState, OrderMode},
    map_view::map_view_status_bundle,
    model::{MatchPhase, MatchView, ToastKind},
    network::{ClientIntent, NetworkSet},
};

const PANEL: Color = Color::srgba(0.035, 0.052, 0.064, 0.96);
const PANEL_SOFT: Color = Color::srgba(0.055, 0.078, 0.092, 0.94);
const LINE: Color = Color::srgba(0.42, 0.58, 0.65, 0.48);
const TEXT: Color = Color::srgb(0.88, 0.93, 0.95);
const MUTED: Color = Color::srgb(0.57, 0.68, 0.72);
const CYAN: Color = Color::srgb(0.40, 0.87, 0.91);
const CORAL: Color = Color::srgb(1.0, 0.40, 0.32);

const FIELD_MANUAL: &str = concat!(
    "CLUSTER SELECTION\n",
    "C selects the complete owned traversable cluster under the cursor. Shift+C adds; Control+C removes; Control/Command+A selects all owned clusters. Empty owned cells connect a cluster, while blocked terrain and cliffs split it. Escape clears an idle selection.\n\n",
    "LMB NEUTRAL  EXPAND CLUSTERS\n",
    "Click unclaimed passable ground to dispatch Share from troops already stationed on every eligible selected perimeter. Use Reshape to deploy inland reserves; Front Rebalance shifts troops between existing fronts. Expansion still pressures all sides, with a mild bias toward the click. Repeat the exact click while it is in flight to layer another independent command from the action-available perimeter troops remaining after earlier commands. [ / ] changes Share.\n\n",
    "LMB ENEMY  ATTACK CLUSTERS\n",
    "Click an enemy cluster to attack every shared front using troops already stationed there. Use Reshape to deploy inland reserves; Front Rebalance shifts troops between existing fronts. Shift+LMB stages/toggles several complete enemy clusters, Control+LMB removes one, and plain LMB or Enter dispatches the union once. Repeat the exact plain click while it is in flight to layer another independent Share from the remaining action-available front troops. The wave turns and branches as fronts change but never leaves the selected enemy mask.\n\n",
    "B  FRONT REBALANCE\n",
    "Select exactly one complete cluster, press B, then drag from one owned front boundary to another. The current Share moves once from the source front to the target front along terrain-aware routes. Fronts have equal strategic importance by default; exposed edge count only spreads the chosen target allocation within that front. [ / ] changes Share.\n\n",
    "T  RESHAPE ONE CLUSTER\n",
    "Select exactly one cluster, press T, and draw its desired owned troop footprint. [ / ] grows a symmetric ring; Shift+[ / ] changes width and Control+[ / ] changes height. The full unavailable/off-world brush remains visible. Reshape uses all free troops, fills the drawing best-effort, and leaves conserved overflow outside when it cannot fit. Release previews; LMB/Enter applies; T redraws.\n\n",
    "X  STOP\n",
    "X snapshots live orders intersecting the selected clusters. LMB/Enter stops only that exact snapshot. Selecting a cluster never retasks or cancels its active troops.\n\n",
    "MAP / CAMERA\n",
    "1 overview · 2 soldiers · 3 civilians · V cycle. MMB or Space+LMB pan · WASD pan · Q/E rotate · wheel zoom · Home frame. Bottom slider or M+Arrows changes future recruitment. ? closes this guide.",
);

#[derive(Component)]
struct TopStatus;
#[derive(Component)]
struct InspectorText;
#[derive(Component)]
struct OrderText;
#[derive(Component)]
struct BottomStatus;
#[derive(Component)]
struct ToastRoot;
#[derive(Component)]
struct ToastText;
#[derive(Component)]
struct HelpPanel;
#[derive(Component)]
struct MobilizationLabel;
#[derive(Component)]
struct MobilizationSlider;
#[derive(Component)]
struct MobilizationThumb;

#[derive(Component)]
struct ResultOverlay;
#[derive(Component)]
struct ResultTitle;
#[derive(Component)]
struct ResultDetails;

#[derive(Component)]
struct CommandContextTitle;
#[derive(Component)]
struct CommandContextSummary;
#[derive(Component)]
struct CommandKeyHints;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReadyCommand {
    Push,
    ContactPush,
    FrontRebalance,
    Expand,
}

/// Small UI-facing projection of the reducer state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HudContext {
    Idle,
    AttackTargets,
    Orient,
    Ready(ReadyCommand),
    ReshapeDrawing,
    ReshapeReady,
    Stop,
    Submitting,
}

fn hud_context(mode: &OrderMode) -> HudContext {
    match mode {
        OrderMode::Idle => HudContext::Idle,
        OrderMode::AttackClustersPreview => HudContext::AttackTargets,
        OrderMode::PushFrontOrient { .. } => HudContext::Orient,
        OrderMode::PushFrontPreview { .. } => HudContext::Ready(ReadyCommand::Push),
        OrderMode::PushFrontArcPreview => HudContext::Ready(ReadyCommand::ContactPush),
        OrderMode::FrontRebalanceSelectSource | OrderMode::FrontRebalanceDrag { .. } => {
            HudContext::Ready(ReadyCommand::FrontRebalance)
        }
        OrderMode::ExpandAllPreview => HudContext::Ready(ReadyCommand::Expand),
        OrderMode::ReshapeDrawing => HudContext::ReshapeDrawing,
        OrderMode::ReshapePreview => HudContext::ReshapeReady,
        OrderMode::StopPreview { .. } => HudContext::Stop,
        OrderMode::Submitting { .. } => HudContext::Submitting,
    }
}

#[derive(Default)]
struct SelectionTotalsCache {
    initialized: bool,
    source_revision: u64,
    logical_step: u64,
    chunk_index_revision: u64,
    cell_state_revision: u64,
    retask_revision: u64,
    totals: (u64, u64, u64),
    active_strength: u64,
}

pub struct HudPlugin;

impl Plugin for HudPlugin {
    fn build(&self, app: &mut App) {
        app.add_observer(on_mobilization_change)
            .add_systems(Startup, spawn_hud)
            .add_systems(
                Update,
                (
                    update_hud.after(NetworkSet::Apply),
                    update_command_bar.after(update_hud),
                    update_slider_visuals.after(update_hud),
                    leave_match_after_victory,
                ),
            );
    }
}

fn spawn_hud(mut commands: Commands, view: Res<MatchView>) {
    commands
        .spawn((
            Name::new("HUD root"),
            Node {
                width: percent(100),
                height: percent(100),
                position_type: PositionType::Absolute,
                ..default()
            },
            GlobalZIndex(20),
            Pickable::IGNORE,
        ))
        .with_children(|root| {
            spawn_top_bar(root);
            spawn_right_panel(root);
            spawn_command_bar(root);
            spawn_bottom_bar(root, view.mobilization_target);
            spawn_toast(root);
            spawn_help(root);
            spawn_result_overlay(root);
        });
}

fn spawn_top_bar(root: &mut ChildSpawnerCommands) {
    root.spawn((
        Name::new("Top status strip"),
        Node {
            position_type: PositionType::Absolute,
            left: px(14),
            right: px(14),
            top: px(12),
            height: px(48),
            padding: UiRect::axes(px(17), px(0)),
            border: UiRect::all(px(1)),
            align_items: AlignItems::Center,
            ..default()
        },
        BackgroundColor(PANEL),
        BorderColor::all(LINE),
    ))
    .with_children(|bar| {
        bar.spawn((
            TopStatus,
            Text::new("Loading match state..."),
            TextFont::from_font_size(13.0),
            TextColor(TEXT),
            Pickable::IGNORE,
        ));
    });
}

fn spawn_right_panel(root: &mut ChildSpawnerCommands) {
    root.spawn((
        Name::new("Compact tactical summary"),
        Node {
            position_type: PositionType::Absolute,
            right: px(14),
            top: px(70),
            width: px(286),
            padding: UiRect::all(px(12)),
            border: UiRect::all(px(1)),
            flex_direction: FlexDirection::Column,
            row_gap: px(8),
            ..default()
        },
        BackgroundColor(PANEL),
        BorderColor::all(LINE),
    ))
    .with_children(|panel| {
        panel.spawn(section_title("TACTICAL SUMMARY"));
        panel.spawn(map_view_status_bundle());
        panel.spawn(divider());
        panel.spawn((
            InspectorText,
            Text::new("INSPECTOR\nHover a visible hex"),
            TextFont::from_font_size(12.5),
            TextColor(TEXT),
            Pickable::IGNORE,
        ));
        panel.spawn(divider());
        panel.spawn((
            OrderText,
            Text::new("ORDER\nC selects a cluster · LMB issues contextual orders"),
            TextFont::from_font_size(12.5),
            TextColor(TEXT),
            Pickable::IGNORE,
        ));
    });
}

fn spawn_command_bar(root: &mut ChildSpawnerCommands) {
    root.spawn((
        Name::new("Compact contextual key hints"),
        Node {
            position_type: PositionType::Absolute,
            left: px(14),
            right: px(14),
            bottom: px(91),
            height: px(52),
            padding: UiRect::axes(px(12), px(7)),
            border: UiRect::all(px(1)),
            flex_direction: FlexDirection::Row,
            align_items: AlignItems::Center,
            column_gap: px(12),
            ..default()
        },
        BackgroundColor(PANEL),
        BorderColor::all(LINE),
    ))
    .with_children(|bar| {
        bar.spawn((
            Node {
                width: px(220),
                flex_shrink: 0.0,
                flex_direction: FlexDirection::Column,
                row_gap: px(2),
                ..default()
            },
            Pickable::IGNORE,
        ))
        .with_children(|context| {
            context.spawn((
                CommandContextTitle,
                Text::new("SELECTION  //  EMPTY"),
                TextFont::from_font_size(10.5),
                TextColor(CYAN),
                Pickable::IGNORE,
            ));
            context.spawn((
                CommandContextSummary,
                Text::new("LMB neutral expand  ·  LMB enemy attack  ·  ? manual"),
                TextFont::from_font_size(9.5),
                TextColor(MUTED),
                Pickable::IGNORE,
            ));
        });

        bar.spawn((
            CommandKeyHints,
            Text::new(
                "C cluster  ·  Shift/Ctrl+C multi  ·  Ctrl+A all  ·  B rebalance fronts  ·  [ / ] Share  ·  T reshape  ·  X stop",
            ),
            TextFont::from_font_size(10.0),
            TextColor(TEXT),
            Node {
                flex_grow: 1.0,
                min_width: px(0),
                ..default()
            },
            Pickable::IGNORE,
        ));
    });
}

fn spawn_bottom_bar(root: &mut ChildSpawnerCommands, mobilization: f32) {
    root.spawn((
        Name::new("Bottom status strip"),
        Node {
            position_type: PositionType::Absolute,
            left: px(14),
            right: px(14),
            bottom: px(12),
            height: px(70),
            padding: UiRect::axes(px(17), px(10)),
            border: UiRect::all(px(1)),
            flex_direction: FlexDirection::Row,
            align_items: AlignItems::Center,
            column_gap: px(18),
            ..default()
        },
        BackgroundColor(PANEL),
        BorderColor::all(LINE),
    ))
    .with_children(|bar| {
        bar.spawn((
            Node {
                width: px(365),
                flex_direction: FlexDirection::Column,
                row_gap: px(5),
                ..default()
            },
            Pickable::IGNORE,
        ))
        .with_children(|group| {
            group.spawn((
                MobilizationLabel,
                Text::new("MOBILIZATION TARGET  55%"),
                TextFont::from_font_size(12.0),
                TextColor(CYAN),
                Pickable::IGNORE,
            ));
            group.spawn((
                Text::new("future recruitment / existing soldiers remain mobilized"),
                TextFont::from_font_size(10.0),
                TextColor(MUTED),
                Pickable::IGNORE,
            ));
        });

        bar.spawn((
            Name::new("Mobilization slider"),
            MobilizationSlider,
            Slider {
                track_click: TrackClick::Snap,
                ..default()
            },
            SliderValue(mobilization),
            SliderRange::new(0.0, 1.0),
            Hovered::default(),
            Node {
                width: px(220),
                height: px(20),
                justify_content: JustifyContent::Center,
                align_items: AlignItems::Center,
                ..default()
            },
        ))
        .with_children(|slider| {
            slider.spawn((
                Node {
                    width: percent(100),
                    height: px(5),
                    border_radius: BorderRadius::MAX,
                    ..default()
                },
                BackgroundColor(Color::srgb(0.10, 0.16, 0.18)),
                Pickable::IGNORE,
            ));
            slider
                .spawn((
                    Node {
                        position_type: PositionType::Absolute,
                        left: px(0),
                        right: px(14),
                        top: px(0),
                        bottom: px(0),
                        ..default()
                    },
                    Pickable::IGNORE,
                ))
                .with_children(|track| {
                    track.spawn((
                        MobilizationThumb,
                        SliderThumb,
                        Node {
                            position_type: PositionType::Absolute,
                            left: percent(mobilization * 100.0),
                            top: px(3),
                            width: px(14),
                            height: px(14),
                            border_radius: BorderRadius::MAX,
                            border: UiRect::all(px(2)),
                            ..default()
                        },
                        BackgroundColor(CYAN),
                        BorderColor::all(Color::srgb(0.84, 0.98, 1.0)),
                        Pickable::IGNORE,
                    ));
                });
        });

        bar.spawn((
            BottomStatus,
            Text::new("No committed flows"),
            TextFont::from_font_size(11.0),
            TextColor(TEXT),
            Node {
                flex_grow: 1.0,
                ..default()
            },
            Pickable::IGNORE,
        ));
    });
}

fn spawn_toast(root: &mut ChildSpawnerCommands) {
    root.spawn((
        ToastRoot,
        Node {
            display: Display::None,
            position_type: PositionType::Absolute,
            left: percent(50),
            bottom: px(155),
            padding: UiRect::axes(px(17), px(9)),
            border: UiRect::all(px(1)),
            ..default()
        },
        UiTransform::from_translation(Val2::percent(-50.0, 0.0)),
        BackgroundColor(PANEL_SOFT),
        BorderColor::all(LINE),
        Pickable::IGNORE,
    ))
    .with_children(|toast| {
        toast.spawn((
            ToastText,
            Text::new(""),
            TextFont::from_font_size(12.5),
            TextColor(TEXT),
            Pickable::IGNORE,
        ));
    });
}

fn spawn_help(root: &mut ChildSpawnerCommands) {
    root.spawn((
        HelpPanel,
        Node {
            display: Display::None,
            position_type: PositionType::Absolute,
            left: px(56),
            top: px(74),
            width: px(760),
            padding: UiRect::all(px(14)),
            border: UiRect::all(px(1)),
            flex_direction: FlexDirection::Column,
            row_gap: px(6),
            ..default()
        },
        GlobalZIndex(40),
        BackgroundColor(Color::srgba(0.025, 0.039, 0.049, 0.985)),
        BorderColor::all(CYAN),
    ))
    .with_children(|help| {
        help.spawn(section_title("FIELD MANUAL  //  ? TO CLOSE"));
        help.spawn((
            Text::new(FIELD_MANUAL),
            TextFont::from_font_size(9.5),
            TextColor(TEXT),
            Pickable::IGNORE,
        ));
    });
}

fn spawn_result_overlay(root: &mut ChildSpawnerCommands) {
    root.spawn((
        Name::new("Match result overlay"),
        ResultOverlay,
        Node {
            display: Display::None,
            position_type: PositionType::Absolute,
            left: percent(50),
            top: percent(50),
            width: px(480),
            padding: UiRect::all(px(24)),
            border: UiRect::all(px(2)),
            flex_direction: FlexDirection::Column,
            align_items: AlignItems::Center,
            row_gap: px(12),
            ..default()
        },
        UiTransform::from_translation(Val2::percent(-50.0, -50.0)),
        GlobalZIndex(60),
        BackgroundColor(Color::srgba(0.025, 0.039, 0.049, 0.985)),
        BorderColor::all(CYAN),
        Pickable::IGNORE,
    ))
    .with_children(|overlay| {
        overlay.spawn((
            ResultTitle,
            Text::new("MATCH COMPLETE"),
            TextFont::from_font_size(24.0),
            TextColor(CYAN),
            Pickable::IGNORE,
        ));
        overlay.spawn((
            ResultDetails,
            Text::new(""),
            TextFont::from_font_size(13.0),
            TextColor(TEXT),
            Node {
                align_self: AlignSelf::Stretch,
                ..default()
            },
            Pickable::IGNORE,
        ));
    });
}

fn section_title(value: &'static str) -> impl Bundle {
    (
        Text::new(value),
        TextFont::from_font_size(11.0),
        TextColor(CYAN),
        Pickable::IGNORE,
    )
}

fn divider() -> impl Bundle {
    (
        Node {
            width: percent(100),
            height: px(1),
            ..default()
        },
        BackgroundColor(LINE),
        Pickable::IGNORE,
    )
}

fn on_mobilization_change(
    change: On<ValueChange<f32>>,
    sliders: Query<(), With<MobilizationSlider>>,
    mut intents: MessageWriter<ClientIntent>,
) {
    if sliders.contains(change.event_target()) {
        intents.write(ClientIntent::SetMobilization {
            target: change.value.clamp(0.0, 1.0),
        });
    }
}

fn update_command_bar(
    interaction: Res<InteractionState>,
    mut copy: ParamSet<(
        Single<&mut Text, With<CommandContextTitle>>,
        Single<&mut Text, With<CommandContextSummary>>,
        Single<&mut Text, With<CommandKeyHints>>,
    )>,
) {
    let context = hud_context(&interaction.mode);
    let (title, summary, hints) = command_bar_copy(&interaction, context);
    set_text(&mut copy.p0(), title);
    set_text(&mut copy.p1(), summary);
    set_text(&mut copy.p2(), hints);
}

fn command_bar_copy(
    interaction: &InteractionState,
    context: HudContext,
) -> (String, String, String) {
    let invalid = interaction.preview.invalid_reason;
    let contextual_status =
        interaction
            .contextual_in_flight_label()
            .map_or_else(String::new, |label| {
                format!(
                    "  ·  {} {} IN FLIGHT  ·  REPEAT SAME CLICK TO LAYER",
                    interaction.contextual_in_flight_count(),
                    label,
                )
            });
    let projection = |include_share: bool| {
        if let Some(reason) = invalid {
            format!("INVALID  //  {reason}")
        } else if include_share {
            format!(
                "SHARE {:>3}%  ·  {:>5} INF  ·  {} OUT  ·  ETA ~{}s",
                interaction.amount_percent,
                interaction.preview.strength_upper_bound,
                interaction.preview.excluded.len(),
                interaction.preview.eta_seconds,
            )
        } else {
            format!(
                "WHOLE SELECTION  ·  {:>5} INF  ·  {} OUT  ·  ETA ~{}s",
                interaction.preview.strength_upper_bound,
                interaction.preview.excluded.len(),
                interaction.preview.eta_seconds,
            )
        }
    };
    match context {
        HudContext::Idle => (
            format!(
                "SELECTED CLUSTERS  //  {} CELL{}",
                interaction.sources.len(),
                plural(interaction.sources.len())
            ),
            format!(
                "SHARE {:>3}%  ·  LMB neutral expand  ·  LMB enemy attack{contextual_status}  ·  ? manual",
                interaction.amount_percent,
            ),
            "C cluster  ·  Shift/Ctrl+C multi  ·  Ctrl+A all  ·  B rebalance fronts  ·  [ / ] Share  ·  T reshape  ·  X stop".to_owned(),
        ),
        HudContext::AttackTargets => (
            "ATTACK CLUSTERS  //  TARGETS".to_owned(),
            format!(
                "{} TARGET HEX{}  ·  SHARE {:>3}%{contextual_status}",
                interaction.attack_targets.len(),
                plural(interaction.attack_targets.len()),
                interaction.amount_percent,
            ),
            "Shift+LMB toggle cluster  ·  Ctrl+LMB remove  ·  [ / ] Share  ·  LMB/Enter dispatch union  ·  Esc cancel".to_owned(),
        ),
        HudContext::Orient => {
            let direction = interaction.push_direction();
            (
                "PUSH  //  CHOOSE DIRECTION".to_owned(),
                direction.map_or_else(
                    || format!("SHARE {:>3}%  ·  tap for contacts or point for direction", interaction.amount_percent),
                    |direction| format!("HEX {:+},{:+}  ·  SHARE {:>3}%", direction.q, direction.r, interaction.amount_percent),
                ),
                "Tap P contact arcs  ·  hold+point global  ·  [ / ] share  ·  Alt+release cast".to_owned(),
            )
        }
        HudContext::Ready(command) => (
            format!(
                "{}  //  READY",
                match command {
                    ReadyCommand::Push => "PUSH",
                    ReadyCommand::ContactPush => "CONTACT FRONTS",
                    ReadyCommand::FrontRebalance => "FRONT REBALANCE",
                    ReadyCommand::Expand => "EXPAND PERIMETER",
                }
            ),
            projection(matches!(
                command,
                ReadyCommand::Push
                    | ReadyCommand::ContactPush
                    | ReadyCommand::FrontRebalance
                    | ReadyCommand::Expand
            )),
            if matches!(
                command,
                ReadyCommand::Push
                    | ReadyCommand::ContactPush
                    | ReadyCommand::FrontRebalance
                    | ReadyCommand::Expand
            ) {
                "[ / ] share  ·  LMB/Enter dispatch  ·  Esc back".to_owned()
            } else {
                "Apply to free troops only  ·  active action troops stay allocated  ·  Esc back".to_owned()
            },
        ),
        HudContext::ReshapeDrawing => (
            "RESHAPE  //  DRAW".to_owned(),
            format!(
                "{} DESTINATION HEX{}  ·  BRUSH {}x{}+{}  ·  FREE TROOPS  ·  ONE CLUSTER",
                interaction.shape_targets.len(),
                plural(interaction.shape_targets.len()),
                interaction.brush.width(),
                interaction.brush.height(),
                interaction.brush.rings(),
            ),
            "LMB draw  ·  [ / ] ring  ·  Shift+[ / ] width  ·  Ctrl+[ / ] height  ·  release previews".to_owned(),
        ),
        HudContext::ReshapeReady => (
            "RESHAPE  //  READY".to_owned(),
            if let Some(reason) = invalid {
                format!("INVALID  //  {reason}")
            } else if interaction.preview.reshape_outside_strength > 0 {
                format!(
                    "FIT {} / CAP {}  ·  {} STAY OUTSIDE  ·  BEST EFFORT",
                    interaction.preview.reshape_destination_strength,
                    interaction.preview.destination_capacity,
                    interaction.preview.reshape_outside_strength,
                )
            } else {
                format!(
                    "{} DESTINATION HEX{}  ·  FIT {} / CAP {}  ·  EXACT",
                    interaction.shape_targets.len(),
                    plural(interaction.shape_targets.len()),
                    interaction.preview.reshape_destination_strength,
                    interaction.preview.destination_capacity,
                )
            },
            "T redraw  ·  LMB/Enter apply  ·  Esc back".to_owned(),
        ),
        HudContext::Stop => {
            let count = match &interaction.mode {
                OrderMode::StopPreview { order_ids } => order_ids.len(),
                _ => 0,
            };
            (
                "STOP  //  EXACT SNAPSHOT".to_owned(),
                format!(
                    "{count} exact order{} highlighted  ·  STOP affects only this snapshot",
                    plural(count)
                ),
                "LMB/Enter stop highlighted orders  ·  Esc back".to_owned(),
            )
        }
        HudContext::Submitting => (
            "SUBMITTING COMMAND".to_owned(),
            "WAITING FOR AUTHORITY  ·  troop controls locked".to_owned(),
            "The selection and pending command remain stable until the response arrives".to_owned(),
        ),
    }
}

#[allow(clippy::too_many_arguments)]
fn update_hud(
    mut commands: Commands,
    view: Res<MatchView>,
    interaction: Res<InteractionState>,
    mut texts: ParamSet<(
        Single<&mut Text, With<TopStatus>>,
        Single<&mut Text, With<InspectorText>>,
        Single<&mut Text, With<OrderText>>,
        Single<&mut Text, With<BottomStatus>>,
        Single<&mut Text, With<MobilizationLabel>>,
        Single<&mut Text, With<ToastText>>,
        Single<&mut Text, With<ResultTitle>>,
        Single<&mut Text, With<ResultDetails>>,
    )>,
    mut panels: ParamSet<(
        Single<(&mut Node, &mut BackgroundColor, &mut BorderColor), With<ToastRoot>>,
        Single<&mut Node, With<HelpPanel>>,
        Single<(&mut Node, &mut BorderColor), With<ResultOverlay>>,
    )>,
    slider: Single<(Entity, &SliderValue), With<MobilizationSlider>>,
    mut selection_totals: Local<SelectionTotalsCache>,
) {
    let player_status = player_status_summary(&view);
    {
        let mut top = texts.p0();
        set_text(
            &mut top,
            format!(
                "LOCAL P{}  /  AUTH {}     {}     {}",
                view.local_player,
                view.authority.label(),
                view.phase.label(view.conquest_threshold_bps),
                player_status,
            ),
        );
    }

    let inspector_value = interaction.hovered.and_then(|coordinate| {
        view.cell(coordinate).map(|cell| {
            let owner = cell
                .owner
                .map_or_else(|| "UNCLAIMED".to_owned(), |owner| format!("PLAYER {owner}"));
            let contest = view.contested_cells.get(&coordinate).map_or_else(
                String::new,
                |contest| {
                    format!(
                        "\nCONTEST P{} {:>4}  /  SHARE {:>3.0}%  /  TOTAL {:>4}",
                        contest.attacker_player,
                        contest.attacker_strength,
                        contest.attacker_share * 100.0,
                        cell.infantry.saturating_add(contest.attacker_strength),
                    )
                },
            );
            format!(
                "INSPECTOR\nHEX {:+03},{:+03}  /  {:?}\nELEVATION {:02}  /  OWNER {}\nCIVILIANS {:>4}  /  CONTROL INF {:>4}\nCAPACITY {:>4}  /  OCCUPANCY {:>3.0}%{}{}",
                coordinate.q,
                coordinate.r,
                cell.terrain,
                cell.elevation,
                owner,
                cell.civilians,
                cell.infantry,
                cell.military_capacity,
                cell.density() * 100.0,
                if cell.blocked { "  /  X BLOCKED" } else { "" },
                contest,
            )
        })
    });
    {
        let mut inspector = texts.p1();
        set_text(
            &mut inspector,
            inspector_value.unwrap_or_else(|| {
                "INSPECTOR\nHover a visible hex\nHeight-aware picking reports the topmost cell"
                    .to_owned()
            }),
        );
    }

    if !selection_totals.initialized
        || selection_totals.source_revision != interaction.source_revision
        || selection_totals.logical_step != view.logical_step
        || selection_totals.chunk_index_revision != view.chunk_index_revision
        || selection_totals.cell_state_revision != view.cell_state_revision
        || selection_totals.retask_revision != view.retask_revision
    {
        selection_totals.totals = view.selected_totals(&interaction.sources);
        selection_totals.active_strength = interaction
            .sources
            .iter()
            .map(|coordinate| {
                let infantry = view.cell(*coordinate).map_or(0, |cell| cell.infantry);
                view.retask_projection
                    .active_strength_by_cell
                    .get(coordinate)
                    .copied()
                    .unwrap_or(0)
                    .min(infantry)
            })
            .sum();
        selection_totals.source_revision = interaction.source_revision;
        selection_totals.logical_step = view.logical_step;
        selection_totals.chunk_index_revision = view.chunk_index_revision;
        selection_totals.cell_state_revision = view.cell_state_revision;
        selection_totals.retask_revision = view.retask_revision;
        selection_totals.initialized = true;
    }
    let (raw_strength, raw_capacity, civilians) = selection_totals.totals;
    let active_strength = selection_totals.active_strength;
    let free_strength = raw_strength.saturating_sub(active_strength);
    let occupancy = if raw_capacity == 0 {
        0.0
    } else {
        raw_strength as f32 * 100.0 / raw_capacity as f32
    };
    let state_line = interaction.preview.invalid_reason.map_or_else(
        || match interaction.mode {
            OrderMode::Idle if interaction.contextual_in_flight_count() > 0 => format!(
                "{} {} IN FLIGHT / REPEAT SAME CLICK TO LAYER",
                interaction.contextual_in_flight_count(),
                interaction
                    .contextual_in_flight_label()
                    .unwrap_or("COMMAND"),
            ),
            OrderMode::Idle => "LMB NEUTRAL EXPAND / ENEMY ATTACK".to_owned(),
            OrderMode::AttackClustersPreview => "STAGE TARGET CLUSTERS OR DISPATCH".to_owned(),
            OrderMode::ReshapeDrawing | OrderMode::ReshapePreview => {
                "BEST-EFFORT SINGLE-CLUSTER TRANSITION".to_owned()
            }
            OrderMode::StopPreview { .. } => "STOP AFFECTS ONLY THIS SNAPSHOT".to_owned(),
            OrderMode::Submitting { .. } => "WAITING FOR AUTHORITY".to_owned(),
            _ => "READY FOR CONTEXTUAL COMMAND".to_owned(),
        },
        |reason| format!("INVALID / {reason}"),
    );
    let allocation = match interaction.mode {
        OrderMode::PushFrontOrient { .. }
        | OrderMode::PushFrontPreview { .. }
        | OrderMode::PushFrontArcPreview
        | OrderMode::FrontRebalanceSelectSource
        | OrderMode::FrontRebalanceDrag { .. }
        | OrderMode::ExpandAllPreview
        | OrderMode::AttackClustersPreview => {
            format!("SHARE {:>3}%", interaction.amount_percent)
        }
        OrderMode::ReshapeDrawing | OrderMode::ReshapePreview => {
            format!("AVAILABLE FREE INF {free_strength}")
        }
        OrderMode::StopPreview { .. } => "SCOPE EXACT".to_owned(),
        OrderMode::Idle => "SELECT CLUSTER".to_owned(),
        OrderMode::Submitting { .. } => "SCOPE LOCKED".to_owned(),
    };
    {
        let mut order = texts.p2();
        set_text(
            &mut order,
            format!(
                "ORDER  //  {}\nSOURCE {:>3} CELLS\nFREE {:>5}  /  ACTIVE {:>5}  /  CAP {:>5}\nCIV {:>5}  /  TOTAL DENSITY {:>3.0}%\n{}\n{}",
                interaction.mode.label(),
                interaction.sources.len(),
                free_strength,
                active_strength,
                raw_capacity,
                civilians,
                occupancy,
                allocation,
                state_line,
            ),
        );
    }

    {
        let mut bottom = texts.p3();
        set_text(
            &mut bottom,
            format!(
                "STEP {:>7}  /  {} ORDER{}  /  {} FLOW{}  /  {} FRONT{}  /  {:>4} QUEUED\nLATEST  {}",
                view.logical_step,
                view.active_orders,
                plural(view.active_orders),
                view.flow_count(),
                plural(view.flow_count()),
                view.active_fronts.len(),
                plural(view.active_fronts.len()),
                view.queued_infantry,
                view.latest_result,
            ),
        );
    }
    {
        let mut mobilization_label = texts.p4();
        set_text(
            &mut mobilization_label,
            format!(
                "MOBILIZATION TARGET  {:>3.0}%",
                view.mobilization_target * 100.0
            ),
        );
    }
    if (slider.1.0 - view.mobilization_target).abs() > 0.001 {
        commands
            .entity(slider.0)
            .insert(SliderValue(view.mobilization_target));
    }

    {
        let mut help_panel = panels.p1();
        help_panel.display = if interaction.show_help {
            Display::Flex
        } else {
            Display::None
        };
    }

    if let Some(toast) = &view.toast {
        {
            let mut toast_root = panels.p0();
            toast_root.0.display = Display::Flex;
            let accent = match toast.kind {
                ToastKind::Info => CYAN,
                ToastKind::Success => Color::srgb(0.41, 0.86, 0.58),
                ToastKind::Rejection => CORAL,
            };
            toast_root.1.0 = Color::srgba(0.035, 0.052, 0.064, 0.98);
            toast_root.2.set_all(accent);
        }
        let mut toast_text = texts.p5();
        set_text(&mut toast_text, toast.text.clone());
    } else {
        let mut toast_root = panels.p0();
        toast_root.0.display = Display::None;
    }

    {
        let mut overlay = panels.p2();
        if let MatchPhase::Victory(winner) = view.phase {
            let local_won = winner == view.local_player;
            overlay.0.display = Display::Flex;
            overlay.1.set_all(if local_won { CYAN } else { CORAL });

            let mut title = texts.p6();
            set_text(
                &mut title,
                if local_won {
                    "VICTORY CONFIRMED".to_owned()
                } else {
                    "MATCH COMPLETE".to_owned()
                },
            );

            let mut details = texts.p7();
            set_text(
                &mut details,
                format!(
                    "WINNER       PLAYER {winner}\nLOCAL SEAT   PLAYER {}  //  {}\nCONQUEST    RESOLVED AT LOGICAL STEP {}\n\nESC  //  RETURN TO LOBBY DIRECTORY\nFROM THE DIRECTORY, SELECT LEAVE TO RETIRE THE LOBBY.",
                    view.local_player,
                    if local_won { "VICTORY" } else { "DEFEAT" },
                    view.logical_step,
                ),
            );
        } else {
            overlay.0.display = Display::None;
        }
    }
}

fn leave_match_after_victory(
    keyboard: Res<ButtonInput<KeyCode>>,
    view: Res<MatchView>,
    mut app_exit: MessageWriter<AppExit>,
) {
    if !matches!(view.phase, MatchPhase::Victory(_)) || !keyboard.just_pressed(KeyCode::Escape) {
        return;
    }

    #[cfg(target_arch = "wasm32")]
    {
        if let Some(window) = web_sys::window() {
            let _ = window.location().set_href("/");
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    app_exit.write(AppExit::Success);
}

fn update_slider_visuals(
    slider: Single<
        (&SliderValue, &SliderRange, &Hovered, &SliderDragState),
        With<MobilizationSlider>,
    >,
    mut thumb: Single<(&mut Node, &mut BackgroundColor), With<MobilizationThumb>>,
) {
    thumb.0.left = percent(slider.1.thumb_position(slider.0.0) * 100.0);
    thumb.1.0 = if slider.2.get() || slider.3.dragging {
        Color::srgb(0.66, 0.98, 1.0)
    } else {
        CYAN
    };
}

fn player_status_summary(view: &MatchView) -> String {
    if view.player_count <= 8 {
        return (1..=view.player_count)
            .map(|player_id| {
                let connection = view
                    .connection
                    .get(usize::from(player_id - 1))
                    .copied()
                    .unwrap_or(crate::model::ConnectionState::Syncing);
                format!(
                    "P{player_id} {:>5.1}% {}",
                    view.conquest_percent(u32::from(player_id)),
                    connection.label()
                )
            })
            .collect::<Vec<_>>()
            .join("  ·  ");
    }

    let mut connected = 0_u16;
    let mut open = 0_u16;
    for state in &view.connection {
        match state {
            crate::model::ConnectionState::Open => open += 1,
            crate::model::ConnectionState::Connected => connected += 1,
            crate::model::ConnectionState::ClaimedOffline
            | crate::model::ConnectionState::Syncing
            | crate::model::ConnectionState::Offline => {}
        }
    }
    // Prefer the authoritative MatchState counter; Syncing seats are not claims.
    let claimed = view.claimed_players;
    let leader = (1..=view.player_count)
        .max_by(|left, right| {
            view.conquest_percent(u32::from(*left))
                .partial_cmp(&view.conquest_percent(u32::from(*right)))
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(left.cmp(right))
        })
        .unwrap_or(1);
    let local_connection = view
        .connection
        .get(usize::from(view.local_player.saturating_sub(1) as u16))
        .copied()
        .unwrap_or(crate::model::ConnectionState::Syncing);
    format!(
        "{}p cfg · {} claimed · {} conn · {} open · lead P{} {:>5.1}% · local P{} {} {:>5.1}%",
        view.player_count,
        claimed,
        connected,
        open,
        leader,
        view.conquest_percent(u32::from(leader)),
        view.local_player,
        local_connection.label(),
        view.conquest_percent(view.local_player),
    )
}

fn set_text(text: &mut Text, value: String) {
    if **text != value {
        **text = value;
    }
}

const fn plural(count: usize) -> &'static str {
    if count == 1 { "" } else { "S" }
}
