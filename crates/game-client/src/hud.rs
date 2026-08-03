use bevy::{
    picking::hover::Hovered,
    prelude::*,
    ui::{Pressed, UiRect},
    ui_widgets::{
        Activate, Button, Slider, SliderDragState, SliderRange, SliderThumb, SliderValue,
        TrackClick, ValueChange,
    },
};

use crate::{
    interaction::{InteractionState, OrderMode, UiAction},
    map_view::map_view_status_bundle,
    model::{MatchView, ToastKind},
    network::{ClientIntent, NetworkSet},
};

const PANEL: Color = Color::srgba(0.035, 0.052, 0.064, 0.96);
const PANEL_SOFT: Color = Color::srgba(0.055, 0.078, 0.092, 0.94);
const LINE: Color = Color::srgba(0.42, 0.58, 0.65, 0.48);
const TEXT: Color = Color::srgb(0.88, 0.93, 0.95);
const MUTED: Color = Color::srgb(0.57, 0.68, 0.72);
const CYAN: Color = Color::srgb(0.40, 0.87, 0.91);
const CORAL: Color = Color::srgb(1.0, 0.40, 0.32);

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
struct HudActionButton(UiAction);

#[derive(Default)]
struct SelectionTotalsCache {
    initialized: bool,
    source_revision: u64,
    logical_step: u64,
    chunk_index_revision: u64,
    cell_state_revision: u64,
    totals: (u64, u64, u64),
}

pub struct HudPlugin;

impl Plugin for HudPlugin {
    fn build(&self, app: &mut App) {
        app.add_observer(on_hud_action)
            .add_observer(on_mobilization_change)
            .add_systems(Startup, spawn_hud)
            .add_systems(
                Update,
                (
                    update_hud.after(NetworkSet::Apply),
                    update_button_style,
                    update_slider_visuals.after(update_hud),
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
            spawn_bottom_bar(root, view.mobilization_target);
            spawn_onboarding(root);
            spawn_toast(root);
            spawn_help(root);
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
            Text::new("Loading match state…"),
            TextFont::from_font_size(13.0),
            TextColor(TEXT),
            Pickable::IGNORE,
        ));
    });
}

fn spawn_right_panel(root: &mut ChildSpawnerCommands) {
    root.spawn((
        Name::new("Tactical control panel"),
        Node {
            position_type: PositionType::Absolute,
            right: px(14),
            top: px(70),
            bottom: px(94),
            width: px(326),
            padding: UiRect::all(px(15)),
            border: UiRect::all(px(1)),
            flex_direction: FlexDirection::Column,
            row_gap: px(10),
            overflow: Overflow::clip_y(),
            ..default()
        },
        BackgroundColor(PANEL),
        BorderColor::all(LINE),
    ))
    .with_children(|panel| {
        panel.spawn(section_title("TACTICAL CONTROL  //  AUTHORITY"));
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
            Text::new("ORDER\nPaint an owned region to begin"),
            TextFont::from_font_size(12.5),
            TextColor(TEXT),
            Pickable::IGNORE,
        ));
        panel
            .spawn(Node {
                width: percent(100),
                flex_direction: FlexDirection::Row,
                column_gap: px(6),
                flex_wrap: FlexWrap::Wrap,
                row_gap: px(6),
                ..default()
            })
            .with_children(|buttons| {
                spawn_button(buttons, "P  PUSH FRONT", UiAction::PushFront, false);
                spawn_button(buttons, "B  BALANCE", UiAction::Balance, false);
                spawn_button(buttons, "F  FRONT-LOAD", UiAction::FrontLoad, false);
                spawn_button(buttons, "[  −10%", UiAction::AmountDown, true);
                spawn_button(buttons, "]  +10%", UiAction::AmountUp, true);
            });
        panel.spawn(divider());
        panel
            .spawn(Node {
                width: percent(100),
                flex_direction: FlexDirection::Row,
                column_gap: px(7),
                ..default()
            })
            .with_children(|buttons| {
                spawn_button(buttons, "ENTER  CONFIRM", UiAction::Confirm, false);
                spawn_button(buttons, "ESC  CANCEL", UiAction::Cancel, false);
            });
        panel.spawn((
            Text::new(
                "MAP KEY\nouter perimeter     selected region\namber/red edge      push front\n× marker            blocked\nopposing chevrons   combat",
            ),
            TextFont::from_font_size(10.5),
            TextColor(MUTED),
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
                Text::new("future recruitment · existing soldiers remain mobilized"),
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

fn spawn_onboarding(root: &mut ChildSpawnerCommands) {
    root.spawn((
        Node {
            position_type: PositionType::Absolute,
            left: percent(50),
            bottom: px(91),
            padding: UiRect::axes(px(12), px(6)),
            ..default()
        },
        UiTransform::from_translation(Val2::percent(-50.0, 0.0)),
        BackgroundColor(PANEL_SOFT),
        Pickable::IGNORE,
    ))
    .with_children(|hint| {
        hint.spawn((
            Text::new(
                "LMB paint  ·  [ / ] brush  ·  C cluster  ·  Ctrl/Cmd+A all  ·  P push  ·  B balance  ·  F front-load  ·  ? help",
            ),
            TextFont::from_font_size(11.0),
            TextColor(MUTED),
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
            bottom: px(145),
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
            left: px(72),
            top: px(82),
            width: px(450),
            padding: UiRect::all(px(19)),
            border: UiRect::all(px(1)),
            flex_direction: FlexDirection::Column,
            row_gap: px(8),
            ..default()
        },
        GlobalZIndex(40),
        BackgroundColor(Color::srgba(0.025, 0.039, 0.049, 0.985)),
        BorderColor::all(CYAN),
    ))
    .with_children(|help| {
        help.spawn(section_title("FIELD MANUAL  //  ? TO CLOSE"));
        help.spawn((
            Text::new(
                "SELECT\nLMB drag paints owned source hexes. Shift adds; Control subtracts. In source mode, [ / ] removes or adds one complete hex ring around the brush, Shift+[ / ] changes width, and Control+[ / ] changes height. C selects the connected owned cluster under the cursor; Shift adds it and Control removes it. Ctrl/Cmd+A selects all owned hexes.\n\nPUSH FRONT\nSelect one connected border section and continue painting backward to include its reinforcement corridor. Hold P, drag outward, and release to choose one of six directions; after clicking the HUD button, click outward on the map instead. Plain [ / ] changes commitment; Enter confirms. The server keeps routes inside the selection until troops cross the displayed front edge.\n\nMAP VIEWS\n1 shows ownership overview, 2 shows absolute soldier strength, and 3 shows civilians. V cycles views. Exact values appear when the camera is close enough to read them; Civilians also outlines populated clusters.\n\nREDISTRIBUTE\nB previews an even target density. Hold F and drag over the map to orient front-load. The pale nested outlines are proposed density, not troops that already moved.\n\nCAMERA\nMMB or Space+LMB pan · WASD pan · Q/E rotate · wheel zoom · Home frame.\n\nDIAGNOSTICS\nF3 toggles the performance overlay. It reports FPS, frame time, entity and gameplay counts.\n\nMOBILIZATION\nUse the bottom slider or M + arrows. It affects future recruitment only; lowering it does not demobilize existing soldiers.",
            ),
            TextFont::from_font_size(12.0),
            TextColor(TEXT),
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

fn spawn_button(
    parent: &mut ChildSpawnerCommands,
    label: &'static str,
    action: UiAction,
    compact: bool,
) {
    parent
        .spawn((
            HudActionButton(action),
            Button,
            Hovered::default(),
            Node {
                min_width: if compact { px(70) } else { px(132) },
                height: px(31),
                padding: UiRect::axes(px(9), px(0)),
                border: UiRect::all(px(1)),
                justify_content: JustifyContent::Center,
                align_items: AlignItems::Center,
                flex_grow: f32::from(!compact),
                ..default()
            },
            BackgroundColor(Color::srgb(0.08, 0.12, 0.14)),
            BorderColor::all(LINE),
        ))
        .with_children(|button| {
            button.spawn((
                Text::new(label),
                TextFont::from_font_size(10.0),
                TextColor(TEXT),
                Pickable::IGNORE,
            ));
        });
}

fn on_hud_action(
    activate: On<Activate>,
    buttons: Query<&HudActionButton>,
    mut actions: MessageWriter<UiAction>,
) {
    if let Ok(action) = buttons.get(activate.event_target()) {
        actions.write(action.0);
    }
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
    )>,
    mut panels: ParamSet<(
        Single<(&mut Node, &mut BackgroundColor, &mut BorderColor), With<ToastRoot>>,
        Single<&mut Node, With<HelpPanel>>,
    )>,
    slider: Single<(Entity, &SliderValue), With<MobilizationSlider>>,
    mut selection_totals: Local<SelectionTotalsCache>,
) {
    let p1 = view.conquest_percent(1);
    let p2 = view.conquest_percent(2);
    {
        let mut top = texts.p0();
        set_text(
            &mut top,
            format!(
                "LOCAL P{}  ·  AUTH {:<22}     ◆ P1 {:<15}     P1  {:>5.1}%     {}     P2  {:>5.1}%     ◇ P2 {:<15}",
                view.local_player,
                view.authority.label(),
                view.connection[0].label(),
                p1,
                view.phase.label(view.conquest_threshold_bps),
                p2,
                view.connection[1].label(),
            ),
        );
    }

    let inspector_value = interaction.hovered.and_then(|coordinate| {
        view.cell(coordinate).map(|cell| {
            let owner = cell
                .owner
                .map_or_else(|| "UNCLAIMED".to_owned(), |owner| format!("PLAYER {owner}"));
            format!(
                "INSPECTOR\nHEX {:+03},{:+03}  ·  {:?}\nELEVATION {:02}  ·  OWNER {}\nCIVILIANS {:>4}  ·  INFANTRY {:>4}\nCAPACITY {:>4}  ·  OCCUPANCY {:>3.0}%{}",
                coordinate.q,
                coordinate.r,
                cell.terrain,
                cell.elevation,
                owner,
                cell.civilians,
                cell.infantry,
                cell.military_capacity,
                cell.density() * 100.0,
                if cell.blocked { "  ·  × BLOCKED" } else { "" },
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
    {
        selection_totals.totals = view.selected_totals(&interaction.sources);
        selection_totals.source_revision = interaction.source_revision;
        selection_totals.logical_step = view.logical_step;
        selection_totals.chunk_index_revision = view.chunk_index_revision;
        selection_totals.cell_state_revision = view.cell_state_revision;
        selection_totals.initialized = true;
    }
    let (strength, capacity, civilians) = selection_totals.totals;
    let occupancy = if capacity == 0 {
        0.0
    } else {
        strength as f32 * 100.0 / capacity as f32
    };
    let bottleneck = interaction.preview.bottleneck.map_or_else(
        || "none".to_owned(),
        |(from, to)| format!("{},{} → {},{}", from.q, from.r, to.q, to.r),
    );
    let context_hint = match interaction.mode {
        OrderMode::Idle => format!(
            "BRUSH {}x{} · RING {} · [/] perimeter · Shift width · Ctrl height",
            interaction.brush.width(),
            interaction.brush.height(),
            interaction.brush.rings()
        ),
        OrderMode::PushFrontOrient { .. } => {
            "Choose outward · release P or click map to quantize".to_owned()
        }
        OrderMode::PushFrontPreview { .. } => interaction.preview.invalid_reason.map_or_else(
            || "Up to shown strength · queued flows may reduce it · Enter confirms".to_owned(),
            |reason| format!("INVALID · {reason}"),
        ),
        OrderMode::BalancePreview => "Nested outlines show target density".to_owned(),
        OrderMode::FrontLoadOrient { .. } => {
            "Choose direction · release F or click map to preview".to_owned()
        }
        OrderMode::FrontLoadPreview { .. } => {
            "Arrow shows orientation · Enter to confirm".to_owned()
        }
        OrderMode::Submitting { .. } => "Waiting for authoritative response…".to_owned(),
    };
    {
        let mut order = texts.p2();
        set_text(
            &mut order,
            format!(
                "ORDER  //  {}\nSOURCE {:>3} HEXES  ·  INF {:>5} / {:>5}\nCIVILIANS {:>5}  ·  DENSITY {:>3.0}%\nFRONT {:>3} EDGES  ·  UP TO {:>5}  ·  COMMIT {:>3}%\nROUTE {:>3} HEXES  ·  EXCLUDED {:>2}\nETA ≈ {:>3}s  ·  BOTTLENECK {}\n\n{}",
                interaction.mode.label(),
                interaction.sources.len(),
                strength,
                capacity,
                civilians,
                occupancy,
                interaction.preview.front_edges.len(),
                interaction.preview.requested_strength,
                interaction.amount_percent,
                interaction.preview.route.len(),
                interaction.preview.excluded.len(),
                interaction.preview.eta_seconds,
                bottleneck,
                context_hint,
            ),
        );
    }

    {
        let mut bottom = texts.p3();
        set_text(
            &mut bottom,
            format!(
                "STEP {:>7}  ·  {} ORDER{}  ·  {} FLOW{}  ·  {} FRONT{}  ·  {:>4} QUEUED\nLATEST  {}",
                view.logical_step,
                view.active_orders,
                plural(view.active_orders),
                view.active_flows.len(),
                plural(view.active_flows.len()),
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
}

fn update_button_style(
    mut buttons: Query<
        (
            &Hovered,
            Has<Pressed>,
            &mut BackgroundColor,
            &mut BorderColor,
        ),
        With<HudActionButton>,
    >,
) {
    for (hovered, pressed, mut background, mut border) in &mut buttons {
        let (fill, line) = if pressed {
            (Color::srgb(0.16, 0.35, 0.39), CYAN)
        } else if hovered.get() {
            (Color::srgb(0.11, 0.20, 0.23), CYAN)
        } else {
            (Color::srgb(0.08, 0.12, 0.14), LINE)
        };
        background.0 = fill;
        border.set_all(line);
    }
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

fn set_text(text: &mut Text, value: String) {
    if **text != value {
        **text = value;
    }
}

const fn plural(count: usize) -> &'static str {
    if count == 1 { "" } else { "S" }
}
