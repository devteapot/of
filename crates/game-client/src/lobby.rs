use bevy::{
    input::{ButtonState, keyboard::KeyboardInput},
    prelude::*,
    ui::UiRect,
};

use crate::{
    config::ClientConfig,
    model::{MatchPhase, MatchView},
};

const SCRIM: Color = Color::srgba(0.008, 0.014, 0.018, 0.88);
const CARD: Color = Color::srgba(0.035, 0.052, 0.064, 0.985);
const FIELD: Color = Color::srgb(0.055, 0.078, 0.092);
const LINE: Color = Color::srgba(0.42, 0.58, 0.65, 0.55);
const TEXT: Color = Color::srgb(0.88, 0.93, 0.95);
const MUTED: Color = Color::srgb(0.57, 0.68, 0.72);
const CYAN: Color = Color::srgb(0.40, 0.87, 0.91);
const ACTIVE: Color = Color::srgb(0.10, 0.31, 0.35);
const DISABLED: Color = Color::srgb(0.075, 0.085, 0.09);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LobbyMapPreset {
    Small,
    Medium,
    Large,
}

#[derive(Message, Clone, Debug)]
pub enum LobbyIntent {
    Create {
        preset: LobbyMapPreset,
        player_count: u16,
        display_name: String,
    },
    Join {
        display_name: String,
    },
    Start,
}

#[derive(Resource)]
struct LobbyForm {
    interactive: bool,
    display_name: String,
    preset: LobbyMapPreset,
    player_count: u16,
    name_focused: bool,
}

#[derive(Component)]
struct LobbyRoot;
#[derive(Component)]
struct LobbyStatus;
#[derive(Component)]
struct LobbyRoster;
#[derive(Component)]
struct NameValue;
#[derive(Component)]
struct PlayerCountValue;
#[derive(Component)]
struct NameField;
#[derive(Component)]
struct CreateButton;
#[derive(Component)]
struct JoinButton;
#[derive(Component)]
struct StartButton;
#[derive(Component)]
struct DecrementPlayers;
#[derive(Component)]
struct IncrementPlayers;
#[derive(Component, Clone, Copy)]
struct PresetButton(LobbyMapPreset);

pub struct LobbyPlugin;

impl Plugin for LobbyPlugin {
    fn build(&self, app: &mut App) {
        let config = app.world().resource::<ClientConfig>();
        app.insert_resource(LobbyForm {
            interactive: !config.auto_join,
            display_name: config.display_name.chars().take(32).collect(),
            preset: LobbyMapPreset::Small,
            player_count: 2,
            name_focused: true,
        })
        .add_message::<LobbyIntent>()
        .add_systems(Startup, spawn_lobby)
        .add_systems(
            Update,
            (
                edit_player_name,
                handle_lobby_buttons,
                update_lobby.after(handle_lobby_buttons),
            ),
        );
    }
}

fn spawn_lobby(mut commands: Commands) {
    commands
        .spawn((
            LobbyRoot,
            Name::new("Lobby screen"),
            Button,
            Node {
                position_type: PositionType::Absolute,
                width: percent(100),
                height: percent(100),
                justify_content: JustifyContent::Center,
                align_items: AlignItems::Center,
                ..default()
            },
            GlobalZIndex(100),
            BackgroundColor(SCRIM),
        ))
        .with_children(|root| {
            root.spawn((
                Node {
                    width: px(720),
                    max_width: percent(92),
                    padding: UiRect::all(px(28)),
                    border: UiRect::all(px(1)),
                    flex_direction: FlexDirection::Column,
                    row_gap: px(18),
                    ..default()
                },
                BackgroundColor(CARD),
                BorderColor::all(LINE),
            ))
            .with_children(|card| {
                card.spawn((
                    Text::new("MATCH LOBBY"),
                    TextFont::from_font_size(26.0),
                    TextColor(CYAN),
                ));
                card.spawn((
                    LobbyStatus,
                    Text::new("Connecting to lobby authority…"),
                    TextFont::from_font_size(13.0),
                    TextColor(MUTED),
                ));
                card.spawn(label("PLAYER NAME"));
                card.spawn((
                    NameField,
                    Button,
                    Node {
                        height: px(46),
                        padding: UiRect::axes(px(13), px(0)),
                        border: UiRect::all(px(1)),
                        align_items: AlignItems::Center,
                        ..default()
                    },
                    BackgroundColor(FIELD),
                    BorderColor::all(CYAN),
                ))
                .with_children(|field| {
                    field.spawn((
                        NameValue,
                        Text::new(""),
                        TextFont::from_font_size(16.0),
                        TextColor(TEXT),
                    ));
                });

                card.spawn(label("CREATE A LOBBY"));
                card.spawn((Node {
                    flex_direction: FlexDirection::Row,
                    column_gap: px(10),
                    flex_wrap: FlexWrap::Wrap,
                    ..default()
                },))
                    .with_children(|row| {
                        spawn_button(row, "SMALL · 64×64", PresetButton(LobbyMapPreset::Small));
                        spawn_button(
                            row,
                            "MEDIUM · 128×128",
                            PresetButton(LobbyMapPreset::Medium),
                        );
                        spawn_button(row, "LARGE · 192×192", PresetButton(LobbyMapPreset::Large));
                    });
                card.spawn((Node {
                    flex_direction: FlexDirection::Row,
                    align_items: AlignItems::Center,
                    column_gap: px(10),
                    ..default()
                },))
                    .with_children(|row| {
                        spawn_button(row, "−", DecrementPlayers);
                        row.spawn((
                            PlayerCountValue,
                            Text::new("2 PLAYERS"),
                            TextFont::from_font_size(14.0),
                            TextColor(TEXT),
                            Node {
                                width: px(110),
                                justify_content: JustifyContent::Center,
                                ..default()
                            },
                        ));
                        spawn_button(row, "+", IncrementPlayers);
                        spawn_button(row, "CREATE & JOIN", CreateButton);
                    });

                card.spawn((
                    Node {
                        height: px(1),
                        ..default()
                    },
                    BackgroundColor(LINE),
                ));
                card.spawn((
                    LobbyRoster,
                    Text::new("No lobby snapshot yet"),
                    TextFont::from_font_size(13.0),
                    TextColor(TEXT),
                ));
                card.spawn((Node {
                    flex_direction: FlexDirection::Row,
                    column_gap: px(10),
                    ..default()
                },))
                    .with_children(|row| {
                        spawn_button(row, "JOIN LOBBY", JoinButton);
                        spawn_button(row, "START GAME", StartButton);
                    });
            });
        });
}

fn label(value: &'static str) -> impl Bundle {
    (
        Text::new(value),
        TextFont::from_font_size(11.0),
        TextColor(MUTED),
    )
}

fn spawn_button<M: Component>(parent: &mut ChildSpawnerCommands, text: &'static str, marker: M) {
    parent
        .spawn((
            marker,
            Button,
            Node {
                min_width: px(46),
                height: px(40),
                padding: UiRect::axes(px(14), px(0)),
                border: UiRect::all(px(1)),
                justify_content: JustifyContent::Center,
                align_items: AlignItems::Center,
                ..default()
            },
            BackgroundColor(FIELD),
            BorderColor::all(LINE),
        ))
        .with_children(|button| {
            button.spawn((
                Text::new(text),
                TextFont::from_font_size(12.0),
                TextColor(TEXT),
            ));
        });
}

fn edit_player_name(
    mut events: MessageReader<KeyboardInput>,
    view: Res<MatchView>,
    mut form: ResMut<LobbyForm>,
) {
    if !form.interactive
        || view.phase != MatchPhase::Lobby
        || view.lobby.local_player.is_some()
        || !form.name_focused
    {
        return;
    }
    for event in events.read() {
        if event.state != ButtonState::Pressed {
            continue;
        }
        match event.key_code {
            KeyCode::Backspace => {
                form.display_name.pop();
            }
            _ => {
                if let Some(text) = &event.text {
                    for character in text.chars().filter(|character| !character.is_control()) {
                        if form.display_name.chars().count() < 32 {
                            form.display_name.push(character);
                        }
                    }
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_lobby_buttons(
    mut form: ResMut<LobbyForm>,
    view: Res<MatchView>,
    name_field: Query<&Interaction, (Changed<Interaction>, With<NameField>)>,
    presets: Query<(&Interaction, &PresetButton), Changed<Interaction>>,
    decrement: Query<&Interaction, (Changed<Interaction>, With<DecrementPlayers>)>,
    increment: Query<&Interaction, (Changed<Interaction>, With<IncrementPlayers>)>,
    create: Query<&Interaction, (Changed<Interaction>, With<CreateButton>)>,
    join: Query<&Interaction, (Changed<Interaction>, With<JoinButton>)>,
    start: Query<&Interaction, (Changed<Interaction>, With<StartButton>)>,
    mut intents: MessageWriter<LobbyIntent>,
) {
    if !form.interactive || view.phase != MatchPhase::Lobby {
        return;
    }
    if name_field
        .iter()
        .any(|interaction| *interaction == Interaction::Pressed)
    {
        form.name_focused = true;
    }
    for (interaction, preset) in &presets {
        if *interaction == Interaction::Pressed && view.lobby.local_player.is_none() {
            form.preset = preset.0;
        }
    }
    if decrement
        .iter()
        .any(|interaction| *interaction == Interaction::Pressed)
    {
        form.player_count = form.player_count.saturating_sub(1).max(2);
    }
    if increment
        .iter()
        .any(|interaction| *interaction == Interaction::Pressed)
    {
        form.player_count = form.player_count.saturating_add(1).min(500);
    }

    let valid_name = !form.display_name.trim().is_empty();
    let create_allowed = view.lobby.available
        && !view.lobby.action_pending
        && view.lobby.local_player.is_none()
        && view.claimed_players == 0
        && !view.lobby.configuration_locked
        && valid_name;
    if create_allowed
        && create
            .iter()
            .any(|interaction| *interaction == Interaction::Pressed)
    {
        intents.write(LobbyIntent::Create {
            preset: form.preset,
            player_count: form.player_count,
            display_name: form.display_name.trim().to_owned(),
        });
    }
    let join_allowed = view.lobby.available
        && !view.lobby.action_pending
        && view.lobby.local_player.is_none()
        && view.claimed_players < view.player_count
        && valid_name;
    if join_allowed
        && join
            .iter()
            .any(|interaction| *interaction == Interaction::Pressed)
    {
        intents.write(LobbyIntent::Join {
            display_name: form.display_name.trim().to_owned(),
        });
    }
    let start_allowed = view.lobby.available
        && !view.lobby.action_pending
        && view.lobby.local_player.is_some()
        && view.claimed_players == view.player_count;
    if start_allowed
        && start
            .iter()
            .any(|interaction| *interaction == Interaction::Pressed)
    {
        intents.write(LobbyIntent::Start);
    }
}

#[allow(clippy::too_many_arguments)]
fn update_lobby(
    view: Res<MatchView>,
    form: Res<LobbyForm>,
    mut root: Single<&mut Node, With<LobbyRoot>>,
    mut status: Single<&mut Text, With<LobbyStatus>>,
    mut roster: Single<&mut Text, With<LobbyRoster>>,
    mut name: Single<&mut Text, With<NameValue>>,
    mut count: Single<&mut Text, With<PlayerCountValue>>,
    mut preset_buttons: Query<(&PresetButton, &mut BackgroundColor)>,
    mut actions: ParamSet<(
        Single<&mut BackgroundColor, With<CreateButton>>,
        Single<&mut BackgroundColor, With<JoinButton>>,
        Single<&mut BackgroundColor, With<StartButton>>,
    )>,
) {
    root.display = if form.interactive && view.phase == MatchPhase::Lobby {
        Display::Flex
    } else {
        Display::None
    };
    if root.display == Display::None {
        return;
    }

    ***name = if form.display_name.is_empty() {
        "Type your name…|".to_owned()
    } else if form.name_focused && view.lobby.local_player.is_none() {
        format!("{}|", form.display_name)
    } else {
        form.display_name.clone()
    };
    ***count = format!("{} PLAYERS", form.player_count);
    ***status = if !view.lobby.available {
        "Connecting to lobby authority…".to_owned()
    } else if view.lobby.action_pending {
        "Applying lobby action…".to_owned()
    } else if view.lobby.local_player.is_some() && view.claimed_players == view.player_count {
        "All players are here. The match is ready to start.".to_owned()
    } else if view.lobby.local_player.is_some() {
        format!(
            "Waiting for players · {}/{} joined",
            view.claimed_players, view.player_count
        )
    } else {
        "Create a new lobby or join the lobby shown below.".to_owned()
    };

    let mut lines = vec![format!(
        "CURRENT LOBBY  ·  {}×{} MAP  ·  {}/{} PLAYERS",
        view.lobby.map_size, view.lobby.map_size, view.claimed_players, view.player_count
    )];
    for seat in 0..usize::from(view.player_count.min(12)) {
        let player = seat + 1;
        let value = view.lobby.player_names.get(seat).map_or("", String::as_str);
        let connection = view
            .connection
            .get(seat)
            .map_or("SYNCING", |state| state.label());
        if value.is_empty() {
            lines.push(format!("P{player:<3} — open seat"));
        } else {
            lines.push(format!("P{player:<3} {value}  ·  {connection}"));
        }
    }
    if view.player_count > 12 {
        lines.push(format!("…and {} more seats", view.player_count - 12));
    }
    ***roster = lines.join("\n");

    for (preset, mut color) in &mut preset_buttons {
        *color = BackgroundColor(if preset.0 == form.preset {
            ACTIVE
        } else {
            FIELD
        });
    }
    let valid_name = !form.display_name.trim().is_empty();
    let create_enabled = view.lobby.available
        && !view.lobby.action_pending
        && view.lobby.local_player.is_none()
        && view.claimed_players == 0
        && !view.lobby.configuration_locked
        && valid_name;
    let join_enabled = view.lobby.available
        && !view.lobby.action_pending
        && view.lobby.local_player.is_none()
        && view.claimed_players < view.player_count
        && valid_name;
    let start_enabled = view.lobby.available
        && !view.lobby.action_pending
        && view.lobby.local_player.is_some()
        && view.claimed_players == view.player_count;
    **actions.p0() = BackgroundColor(if create_enabled { ACTIVE } else { DISABLED });
    **actions.p1() = BackgroundColor(if join_enabled { ACTIVE } else { DISABLED });
    **actions.p2() = BackgroundColor(if start_enabled { ACTIVE } else { DISABLED });
}
