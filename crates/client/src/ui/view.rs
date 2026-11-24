//! View functions for the three screens. Plain widgets, no animations.

use chattercrab_media::{PEER_VOLUME_MAX, PeerConnectionState, ScreenContent, ScreenScale};
use iced::widget::shader::Shader;
use iced::widget::{
    Button, Column, TextInput, button, checkbox, column, container, mouse_area, pick_list, row,
    scrollable, slider, space, stack, text, text_input, tooltip,
};
use iced::{Alignment, Element, Length, Padding, Theme};
use std::collections::BTreeMap;

use super::room_model::{MemberView, RoomModel};
use super::share_view::ShareView;
use super::{
    App, Lobby, Message, RoomScreen, SYSTEM_DEFAULT, Screen, SharePicker, emoji, meter, selectable,
    style,
};
use crate::config::{Config, ThemeChoice};

const SPACING: u32 = 8;
const PADDING: u16 = 16;
const CONTENT_CHOICES: [ScreenContent; 2] = [ScreenContent::Desktop, ScreenContent::Game];
const FPS_CHOICES: [u32; 6] = [5, 10, 15, 20, 30, 60];
const FPS_CHOICES_HQ: [u32; 7] = [5, 10, 15, 20, 30, 60, 120];
const SCALE_CHOICES: [ScreenScale; 4] = [
    ScreenScale::P4K,
    ScreenScale::P2K,
    ScreenScale::P1440,
    ScreenScale::P1080,
];
/// Field padding shared by text inputs and pick lists.
const FIELD_PADDING: [u16; 2] = [9, 12];
/// Width of the percentage next to a volume slider.
const PERCENT_WIDTH: u32 = 36;

type ButtonStyle = fn(&Theme, button::Status) -> button::Style;

pub(super) fn view(app: &App) -> Element<'_, Message> {
    let body = match &app.state.screen {
        Screen::Connect { connecting } => connect_view(app, *connecting),
        Screen::Lobby(lobby) => lobby_view(lobby, &app.config),
        Screen::Room(room) => room_view(app, room),
    };
    let mut page = Column::new().push(container(body).height(Length::Fill));
    if let Some(notice) = &app.state.notice {
        page = page.push(
            container(
                container(
                    row![
                        text(notice).size(14).width(Length::Fill),
                        action("Dismiss", style::ghost_button).on_press(Message::DismissNotice),
                    ]
                    .spacing(SPACING)
                    .align_y(Alignment::Center),
                )
                .padding(Padding::new(6.0).left(f32::from(PADDING)))
                .style(style::floating),
            )
            .padding(Padding::new(f32::from(PADDING)).top(0)),
        );
    }
    page.into()
}

/// A button with the shared padding and text size.
fn action(label: &str, style: ButtonStyle) -> Button<'_, Message> {
    button(text(label).size(14)).padding([8, 14]).style(style)
}

fn input<'a>(placeholder: &str, value: &str) -> TextInput<'a, Message> {
    text_input(placeholder, value)
        .padding(FIELD_PADDING)
        .style(style::input)
}

/// Theme (Auto, Light, Dark) and interface size controls, with text labels.
fn display_controls(config: &Config) -> Element<'static, Message> {
    let (label, tip) = if config.follow_system_theme {
        ("Auto", "Following the system theme. Click for light")
    } else if config.theme == ThemeChoice::Light {
        ("Light", "Light theme. Click for dark")
    } else {
        ("Dark", "Dark theme. Click for auto")
    };
    let tipped = |content: Element<'static, Message>, tip: &'static str| {
        tooltip(
            content,
            container(text(tip).size(12))
                .padding([4, 8])
                .style(style::floating),
            tooltip::Position::Bottom,
        )
    };
    row![
        tipped(
            button(text("A-").size(13))
                .padding([6, 8])
                .style(style::icon_button)
                .on_press(Message::UiScaleStep(-1))
                .into(),
            "Smaller interface (Ctrl+-)",
        ),
        tipped(
            button(text("A+").size(13))
                .padding([6, 8])
                .style(style::icon_button)
                .on_press(Message::UiScaleStep(1))
                .into(),
            "Larger interface (Ctrl+=, Ctrl+0 resets)",
        ),
        tipped(
            button(text(label).size(13))
                .padding([6, 10])
                .style(style::icon_button)
                .on_press(Message::ToggleTheme)
                .into(),
            tip,
        ),
    ]
    .spacing(4)
    .align_y(Alignment::Center)
    .into()
}

/// Round avatar with the name's initial; `radius` below half the size gives a tile.
fn avatar(name: &str, size: f32, radius: f32, speaking: bool) -> Element<'static, Message> {
    let fill = style::avatar_color(name);
    let initial: String = name
        .chars()
        .next()
        .into_iter()
        .flat_map(char::to_uppercase)
        .collect();
    container(text(initial).size(size * 0.42).font(style::SEMIBOLD))
        .center(size)
        .style(move |theme: &Theme| style::avatar(theme, fill, radius, speaking))
        .into()
}

fn device_options(names: &[String]) -> Vec<String> {
    std::iter::once(SYSTEM_DEFAULT.to_owned())
        .chain(names.iter().cloned())
        .collect()
}

/// Pick list options and the label of the chosen device (`None` is the system default).
/// A device missing from a listed set is labelled as not found; the label is not an
/// option, so it cannot be picked again.
fn device_choice(names: &[String], selected: Option<&str>) -> (Vec<String>, String) {
    let label = match selected {
        None => SYSTEM_DEFAULT.to_owned(),
        Some(name) if names.is_empty() || names.iter().any(|n| n == name) => name.to_owned(),
        Some(name) => format!("{name} (not found)"),
    };
    (device_options(names), label)
}

fn connect_view(app: &App, connecting: bool) -> Element<'_, Message> {
    let config = &app.config;
    let (inputs, selected_input) = device_choice(&app.state.inputs, config.input_device.as_deref());
    let (outputs, selected_output) =
        device_choice(&app.state.outputs, config.output_device.as_deref());

    let connect = action(
        if connecting {
            "Connecting..."
        } else {
            "Connect"
        },
        style::primary_button,
    )
    .padding([9, 22])
    .on_press_maybe((!connecting).then_some(Message::Connect));

    let title = column![
        text("Chattercrab").size(26).font(style::SEMIBOLD),
        text("Voice, chat and screen sharing")
            .size(14)
            .style(style::muted),
    ]
    .spacing(2);

    let account = section(
        "ACCOUNT",
        column![
            labeled(
                "Server",
                input("https://chattercrab.qcodefactory.com", &config.server_url)
                    .on_input(Message::ServerUrlChanged)
                    .on_submit(Message::Connect)
                    .into(),
            ),
            row![
                labeled(
                    "User",
                    input("Username", &config.auth_user)
                        .on_input(Message::AuthUserChanged)
                        .on_submit(Message::Connect)
                        .into(),
                ),
                labeled(
                    "Password",
                    input("Shared password", &config.password)
                        .secure(true)
                        .on_input(Message::PasswordChanged)
                        .on_submit(Message::Connect)
                        .into(),
                ),
            ]
            .spacing(SPACING * 2),
            labeled(
                "Display name",
                input("Your name (1-32 characters)", &config.display_name)
                    .on_input(Message::DisplayNameChanged)
                    .on_submit(Message::Connect)
                    .into(),
            ),
        ],
    );

    let audio = section(
        "AUDIO",
        column![
            labeled(
                "Microphone",
                pick_list(inputs, Some(selected_input), Message::InputSelected)
                    .width(Length::Fill)
                    .padding(FIELD_PADDING)
                    .style(style::pick)
                    .menu_style(style::menu)
                    .into(),
            ),
            row![
                small_label("Mic volume"),
                mic_gain_control(config.input_gain)
            ]
            .spacing(SPACING * 2)
            .align_y(Alignment::Center),
            labeled(
                "Speakers",
                pick_list(outputs, Some(selected_output), Message::OutputSelected)
                    .width(Length::Fill)
                    .padding(FIELD_PADDING)
                    .style(style::pick)
                    .menu_style(style::menu)
                    .into(),
            ),
            checkbox(config.noise_suppression)
                .label("Noise suppression (removes background noise, keeps voice)")
                .size(16)
                .text_size(14)
                .style(style::check)
                .on_toggle(Message::NoiseSuppression),
        ],
    );

    let footer = row![
        action("Refresh devices", style::ghost_button).on_press(Message::RefreshDevices),
        space().width(Length::Fill),
        connect,
    ]
    .spacing(SPACING)
    .align_y(Alignment::Center);

    let card = container(
        column![title, account, audio, footer]
            .spacing(SPACING * 2 + 4)
            .width(Length::Fill),
    )
    .padding(24)
    .width(Length::Fill)
    .max_width(480)
    .style(style::card);

    stack![
        container(
            scrollable(container(card).center_x(Length::Fill).padding(PADDING))
                .width(Length::Fill)
                .style(style::scroll),
        )
        .center(Length::Fill),
        container(display_controls(config))
            .align_right(Length::Fill)
            .padding(PADDING),
    ]
    .into()
}

/// A titled group of form fields.
fn section<'a>(title: &'a str, fields: Column<'a, Message>) -> Element<'a, Message> {
    column![
        text(title)
            .size(12)
            .font(style::SEMIBOLD)
            .style(style::muted),
        fields.spacing(SPACING + 2),
    ]
    .spacing(SPACING)
    .into()
}

fn mic_gain_control(gain: f32) -> Element<'static, Message> {
    row![
        slider(0.0..=5.0, gain, Message::InputGainChanged)
            .step(0.05_f32)
            .on_release(Message::InputGainReleased)
            .style(style::volume)
            .width(Length::Fill),
        percent(gain),
    ]
    .spacing(SPACING)
    .align_y(Alignment::Center)
    .into()
}

fn percent(value: f32) -> Element<'static, Message> {
    text(format!("{}%", (value * 100.0).round() as u32))
        .size(12)
        .width(PERCENT_WIDTH)
        .align_x(Alignment::End)
        .style(style::muted)
        .into()
}

fn labeled<'a>(label: &'a str, field: Element<'a, Message>) -> Element<'a, Message> {
    column![text(label).size(13).style(style::muted), field]
        .spacing(4)
        .into()
}

fn lobby_view<'a>(lobby: &'a Lobby, config: &Config) -> Element<'a, Message> {
    let count = match lobby.rooms.len() {
        0 => "No rooms open".to_owned(),
        1 => "1 room open".to_owned(),
        n => format!("{n} rooms open"),
    };
    let header = row![
        column![
            text("Rooms").size(24).font(style::SEMIBOLD),
            text(count).size(13).style(style::muted),
        ]
        .spacing(2)
        .width(Length::Fill),
        action("Refresh", style::ghost_button).on_press(Message::RefreshRooms),
        action("Disconnect", style::ghost_button).on_press(Message::Disconnect),
        display_controls(config),
    ]
    .spacing(SPACING)
    .align_y(Alignment::Center);

    let create = container(
        row![
            input("New room name", &lobby.new_room)
                .on_input(Message::NewRoomChanged)
                .on_submit(Message::CreateRoom),
            action("Create and join", style::primary_button).on_press(Message::CreateRoom),
        ]
        .spacing(SPACING)
        .align_y(Alignment::Center),
    )
    .padding(10)
    .style(style::card);

    let mut list = Column::new().spacing(SPACING);
    if lobby.rooms.is_empty() {
        list = list.push(
            container(
                text("No rooms yet. Create one above.")
                    .size(14)
                    .style(style::muted),
            )
            .center_x(Length::Fill)
            .padding(32),
        );
    }
    for room in &lobby.rooms {
        list = list.push(
            container(
                row![
                    avatar(room.name.as_str(), 36.0, 10.0, false),
                    column![
                        text(room.name.as_str()).size(15).font(style::SEMIBOLD),
                        text(format!(
                            "{} member{}",
                            room.member_count,
                            if room.member_count == 1 { "" } else { "s" }
                        ))
                        .size(12)
                        .style(style::muted),
                    ]
                    .spacing(2)
                    .width(Length::Fill),
                    action("Join", style::secondary_button)
                        .on_press(Message::JoinRoom(room.id.clone(), room.name.clone())),
                ]
                .spacing(SPACING + 4)
                .align_y(Alignment::Center),
            )
            .padding(12)
            .style(style::card),
        );
    }

    container(
        column![
            header,
            create,
            scrollable(list)
                .height(Length::Fill)
                .spacing(4)
                .style(style::scroll),
        ]
        .spacing(SPACING * 2)
        .max_width(720),
    )
    .center_x(Length::Fill)
    .padding(PADDING)
    .into()
}

fn state_label(state: Option<PeerConnectionState>) -> &'static str {
    match state {
        None | Some(PeerConnectionState::Connecting) => "connecting",
        Some(PeerConnectionState::Connected) => "",
        Some(PeerConnectionState::Disconnected) => "unstable",
        Some(PeerConnectionState::Failed) => "unreachable",
        Some(PeerConnectionState::Closed) => "closed",
    }
}

/// Avatar, name and an optional status line.
fn identity<'a>(name: &'a str, status: String, speaking: bool) -> Element<'a, Message> {
    let mut info = column![text(name).size(14).font(style::SEMIBOLD)].width(Length::Fill);
    if !status.is_empty() {
        info = info.push(text(status).size(12).style(style::muted));
    }
    row![avatar(name, 32.0, 16.0, speaking), info]
        .spacing(SPACING + 2)
        .align_y(Alignment::Center)
        .into()
}

fn member_row(member: &MemberView) -> Element<'_, Message> {
    let id = member.info.id;
    let mute = button(text(if member.muted { "Muted" } else { "Mute" }).size(12))
        .padding([3, 8])
        .style(if member.muted {
            style::active_toggle_button
        } else {
            style::ghost_button
        })
        .on_press(Message::PeerMuteToggled(id, !member.muted));
    let good = match member.state {
        Some(PeerConnectionState::Connected) => Some(true),
        Some(PeerConnectionState::Failed | PeerConnectionState::Closed) => Some(false),
        _ => None,
    };
    let dot = container(space())
        .width(8)
        .height(8)
        .style(move |t: &Theme| style::dot(style::connection_color(t, good)));
    column![
        row![
            identity(
                member.info.display_name.as_str(),
                state_label(member.state).to_owned(),
                member.speaking() && !member.muted,
            ),
            dot,
            mute,
        ]
        .spacing(SPACING)
        .align_y(Alignment::Center),
        row![
            slider(0.0..=PEER_VOLUME_MAX, member.volume, move |v| {
                Message::PeerVolumeChanged(id, v)
            })
            .step(0.05_f32)
            .on_release(Message::PeerVolumeCommitted(id))
            .style(style::volume)
            .width(Length::Fill),
            percent(member.volume),
        ]
        .spacing(SPACING)
        .align_y(Alignment::Center)
        .padding(Padding::ZERO.left(42)),
    ]
    .spacing(4)
    .into()
}

fn members_panel(room: &RoomScreen) -> Element<'_, Message> {
    let model = &room.model;
    let mut list = Column::new().spacing(SPACING * 2);
    list = list.push(
        text(format!("IN ROOM · {}", model.members.len() + 1))
            .size(12)
            .font(style::SEMIBOLD)
            .style(style::muted),
    );
    let me_status = if model.muted { "You · muted" } else { "You" };
    list = list.push(identity(
        &model.my_name,
        me_status.to_owned(),
        !model.muted && model.local_level >= super::room_model::SPEAKING_THRESHOLD,
    ));
    for member in &model.members {
        list = list.push(member_row(member));
    }
    container(
        scrollable(list)
            .height(Length::Fill)
            .spacing(4)
            .style(style::scroll),
    )
    .width(260)
    .height(Length::Fill)
    .padding(14)
    .style(style::card)
    .into()
}

fn chat_panel<'a>(
    room: &'a RoomScreen,
    dark: bool,
    usage: &BTreeMap<String, u32>,
) -> Element<'a, Message> {
    let model = &room.model;
    let log: Element<'_, Message> = if model.chat.is_empty() {
        container(
            text("No messages yet. Say hi!")
                .size(14)
                .style(style::muted),
        )
        .center(Length::Fill)
        .into()
    } else {
        // Positional on purpose: iced 0.14's keyed column re-matches every existing row
        // when a line is appended (see `CHAT_TRIM_BATCH` for the trim side).
        let mut log = Column::new().spacing(SPACING + 4).width(Length::Fill);
        let link_color = if dark { &style::DARK } else { &style::LIGHT }
            .extended_palette()
            .primary
            .base
            .color;
        for msg in &model.chat {
            log = log.push(selectable::message(
                msg.id,
                msg.display_name.as_str(),
                msg.text.as_str(),
                link_color,
                Message::OpenLink,
            ));
        }
        scrollable(log)
            .anchor_bottom()
            .spacing(4)
            .style(style::scroll)
            .height(Length::Fill)
            .width(Length::Fill)
            .into()
    };
    let input = input("Message (Enter to send)", &room.chat_input)
        .id(super::CHAT_INPUT_ID)
        .on_input(Message::ChatChanged)
        .on_submit(Message::SendChat);
    let emoji_toggle = tooltip(
        button(text("😀").size(16).center())
            .width(36)
            .height(36)
            .padding(0)
            .style(style::icon_button)
            .on_press(Message::ToggleEmojiPicker),
        container(text("Emoji").size(12))
            .padding([4, 8])
            .style(style::floating),
        tooltip::Position::Top,
    );
    let send_button = tooltip(
        button(text("Send").size(14).center())
            .width(56)
            .height(36)
            .padding(0)
            .style(style::primary_button)
            .on_press_maybe((!room.chat_input.trim().is_empty()).then_some(Message::SendChat)),
        container(text("Send").size(12))
            .padding([4, 8])
            .style(style::floating),
        tooltip::Position::Top,
    );
    let input_row = row![input, emoji_toggle, send_button]
        .spacing(SPACING)
        .align_y(Alignment::Center);
    // The popup slot is always present so the input row keeps its position in the widget
    // tree; inserting a child before it would hand it the popup's state and drop its focus.
    let gap = (SPACING + 4) as f32;
    let popup: Element<'_, Message> = if room.emoji_open {
        container(emoji_picker(&room.emoji_query, usage))
            .width(Length::Fill)
            .align_x(Alignment::End)
            .padding(Padding::ZERO.bottom(gap))
            .into()
    } else if !room.suggestions().is_empty() {
        container(emoji_suggestions(room))
            .width(Length::Fill)
            .padding(Padding::ZERO.bottom(gap))
            .into()
    } else {
        space().into()
    };
    let panel = column![
        container(mouse_area(log).on_press(Message::CloseEmojiPicker))
            .height(Length::Fill)
            .padding(Padding::ZERO.bottom(gap)),
        popup,
        input_row,
    ]
    .height(Length::Fill);
    container(panel)
        .padding(14)
        .height(Length::Fill)
        .style(style::card)
        .into()
}

/// Number of emoji per row in the picker.
const EMOJI_PER_ROW: usize = 8;

/// Width of the emoji picker and suggestion popups.
const EMOJI_PICKER_WIDTH: f32 = 360.0;

fn emoji_grid<'a>(
    mut list: Column<'a, Message>,
    emoji: impl Iterator<Item = &'static str>,
) -> Column<'a, Message> {
    let emoji: Vec<&'static str> = emoji.collect();
    for chunk in emoji.chunks(EMOJI_PER_ROW) {
        list = list.push(
            row(chunk.iter().map(|&e| {
                button(text(e).size(20).center())
                    .width(38)
                    .height(38)
                    .padding(0)
                    .style(style::icon_button)
                    .on_press(Message::InsertEmoji(e))
                    .into()
            }))
            .spacing(2),
        );
    }
    list
}

fn emoji_picker<'a>(query: &str, usage: &BTreeMap<String, u32>) -> Element<'a, Message> {
    let search = input("Search emoji", query)
        .id(super::EMOJI_SEARCH_ID)
        .on_input(Message::EmojiSearchChanged);
    let mut list = Column::new().spacing(4).padding([0, 8]);
    if query.trim().is_empty() {
        let top = emoji::top(usage);
        let mut sections: Vec<(String, Vec<&'static str>)> = Vec::new();
        if !top.is_empty() {
            sections.push(("🕘 Most used".to_owned(), top));
        }
        sections.extend(
            emoji::CATEGORIES
                .iter()
                .map(|c| (format!("{} {}", c.icon, c.name), c.emoji.to_vec())),
        );
        for (i, (title, emoji)) in sections.into_iter().enumerate() {
            if i > 0 {
                list = list.push(
                    container(space())
                        .width(Length::Fill)
                        .height(1)
                        .style(style::divider),
                );
            }
            list = list.push(text(title).size(12).style(style::muted));
            list = emoji_grid(list, emoji.into_iter());
        }
    } else {
        let hits = emoji::search(query, emoji::MAX_RESULTS);
        if hits.is_empty() {
            list = list.push(text("No emoji found").size(13).style(style::muted));
        } else {
            list = emoji_grid(list, hits.into_iter().map(|e| e.as_str()));
        }
    }
    let body = column![
        search,
        scrollable(list)
            .style(style::scroll)
            .width(Length::Fill)
            .height(Length::Fixed(220.0))
    ]
    .spacing(SPACING);
    container(body)
        .padding(10)
        .width(Length::Fixed(EMOJI_PICKER_WIDTH))
        .style(style::popup)
        .into()
}

/// `:word` suggestions above the chat input; Up, Down, Tab and Enter drive them.
fn emoji_suggestions(room: &RoomScreen) -> Element<'_, Message> {
    let rows = Column::with_children(room.suggestions().into_iter().enumerate().map(|(i, e)| {
        let label = format!("{}  :{}:", e.as_str(), e.shortcode().unwrap_or(e.name()));
        button(text(label).size(15))
            .width(Length::Fill)
            .padding([4, 8])
            .style(style::suggestion(i == room.suggest_index))
            .on_press(Message::CompleteEmoji(e.as_str()))
            .into()
    }))
    .push(
        text("Up/Down to choose, Tab or Enter to insert, Esc to dismiss")
            .size(11)
            .style(style::muted),
    )
    .spacing(2);
    container(rows)
        .padding(6)
        .width(Length::Fixed(EMOJI_PICKER_WIDTH))
        .style(style::popup)
        .into()
}

fn small_label(label: &str) -> Element<'_, Message> {
    text(label).size(13).style(style::muted).into()
}

fn share_picker(picker: &SharePicker) -> Element<'_, Message> {
    let sources: Element<'_, Message> = if picker.sources.is_empty() {
        small_label("Looking for screens and windows...")
    } else {
        pick_list(
            picker.sources.as_slice(),
            picker.selected.clone(),
            Message::ShareSourceSelected,
        )
        .width(Length::Fill)
        .padding(FIELD_PADDING)
        .style(style::pick)
        .menu_style(style::menu)
        .into()
    };
    let fps_choices: Vec<u32> = if picker.high_quality {
        FPS_CHOICES_HQ.to_vec()
    } else {
        FPS_CHOICES.to_vec()
    };
    let title = row![
        text("Share your screen")
            .size(15)
            .font(style::SEMIBOLD)
            .width(Length::Fill),
        action("Cancel", style::ghost_button).on_press(Message::CancelShare),
        action("Start sharing", style::primary_button)
            .on_press_maybe(picker.selected.is_some().then_some(Message::StartShare)),
    ]
    .spacing(SPACING)
    .align_y(Alignment::Center);
    let controls = row![
        sources,
        small_label("Mode"),
        pick_list(
            CONTENT_CHOICES,
            Some(picker.content),
            Message::ShareContentSelected,
        )
        .padding(FIELD_PADDING)
        .style(style::pick)
        .menu_style(style::menu),
        small_label("FPS"),
        pick_list(fps_choices, Some(picker.fps), Message::ShareFpsSelected)
            .padding(FIELD_PADDING)
            .style(style::pick)
            .menu_style(style::menu),
        small_label("Size"),
        pick_list(
            SCALE_CHOICES,
            Some(picker.scale),
            Message::ShareScaleSelected,
        )
        .padding(FIELD_PADDING)
        .style(style::pick)
        .menu_style(style::menu),
    ]
    .spacing(SPACING)
    .align_y(Alignment::Center);
    let mode_hint = match picker.content {
        ScreenContent::Desktop => {
            "Crisp fonts and UI. Resolution and frame rate stay as you set them."
        }
        ScreenContent::Game => {
            "Less blur on moving images. Resolution and frame rate stay as you set them."
        }
    };
    let mut picker_column = column![
        title,
        controls,
        row![
            checkbox(picker.high_quality)
                .label("High quality")
                .size(16)
                .text_size(14)
                .style(style::check)
                .on_toggle(Message::ShareHighQualityToggled),
            checkbox(picker.include_audio)
                .label("Share audio from other apps (this call is not included)")
                .size(16)
                .text_size(14)
                .style(style::check)
                .on_toggle(Message::ShareAudioToggled),
        ]
        .spacing(SPACING * 3)
        .align_y(Alignment::Center),
        small_label(mode_hint),
    ]
    .spacing(SPACING + 4);
    if picker.high_quality {
        picker_column = picker_column.push(small_label("Higher bitrate and low delay."));
    }
    container(picker_column)
        .padding(14)
        .width(Length::Fill)
        .style(style::card)
        .into()
}

/// Our microphone level for the meter; empty while muted.
fn mic_level(model: &RoomModel) -> f32 {
    if model.muted { 0.0 } else { model.local_level }
}

fn device_pick(
    options: Vec<String>,
    selected: String,
    on_select: fn(String) -> Message,
) -> Element<'static, Message> {
    pick_list(options, Some(selected), on_select)
        .width(Length::Fill)
        .padding(FIELD_PADDING)
        .style(style::pick)
        .menu_style(style::menu)
        .into()
}

/// Replaces the bottom bar like the share picker: devices, mic volume and level.
fn audio_panel<'a>(app: &'a App, room: &'a RoomScreen) -> Element<'a, Message> {
    let config = &app.config;
    let (inputs, input) = device_choice(&app.state.inputs, config.input_device.as_deref());
    let (outputs, output) = device_choice(&app.state.outputs, config.output_device.as_deref());
    let title = row![
        text("Audio settings")
            .size(15)
            .font(style::SEMIBOLD)
            .width(Length::Fill),
        action("Refresh devices", style::ghost_button).on_press(Message::RefreshDevices),
        action("Close", style::secondary_button).on_press(Message::CloseAudioPanel),
    ]
    .spacing(SPACING)
    .align_y(Alignment::Center);
    let devices = row![
        labeled(
            "Microphone",
            device_pick(inputs, input, Message::InputSelected)
        ),
        labeled(
            "Speakers",
            device_pick(outputs, output, Message::OutputSelected)
        ),
    ]
    .spacing(SPACING * 2);
    let mic = row![
        labeled("Mic volume", mic_gain_control(config.input_gain)),
        labeled(
            "Mic level",
            container(meter::level_meter(mic_level(&room.model), 10.0, 3))
                .padding([4, 0])
                .into(),
        ),
    ]
    .spacing(SPACING * 2);
    let ptt = row![
        checkbox(config.push_to_talk)
            .label("Push to talk (hold the key to speak; works while Chattercrab is focused)")
            .size(16)
            .text_size(14)
            .style(style::check)
            .on_toggle(Message::PushToTalk),
        space().width(Length::Fill),
        pick_list(
            &crate::config::PTT_KEYS[..],
            Some(config.ptt_key.as_str()),
            |key: &str| Message::PttKeySelected(key.to_owned()),
        )
        .padding(FIELD_PADDING)
        .style(style::pick)
        .menu_style(style::menu),
    ]
    .spacing(SPACING)
    .align_y(Alignment::Center);
    container(
        column![
            title,
            devices,
            mic,
            checkbox(config.noise_suppression)
                .label("Noise suppression (removes background noise, keeps voice)")
                .size(16)
                .text_size(14)
                .style(style::check)
                .on_toggle(Message::NoiseSuppression),
            ptt,
        ]
        .spacing(SPACING + 4),
    )
    .padding(14)
    .width(Length::Fill)
    .style(style::card)
    .into()
}

fn bottom_bar<'a>(app: &'a App, room: &'a RoomScreen) -> Element<'a, Message> {
    if let Some(picker) = &room.picker {
        return share_picker(picker);
    }
    if room.audio_panel {
        return audio_panel(app, room);
    }
    let model = &room.model;
    let share = if model.local_sharing {
        action("Stop sharing", style::active_toggle_button).on_press(Message::StopShare)
    } else {
        // Only one publisher per room; the button is disabled while someone shares.
        let free = model.screen_publisher.is_none() && model.me.is_some();
        action("Share screen", style::secondary_button)
            .on_press_maybe(free.then_some(Message::OpenSharePicker))
    };
    let toggle_style = |active: bool| -> ButtonStyle {
        if active {
            style::active_toggle_button
        } else {
            style::secondary_button
        }
    };
    let mute: Element<'_, Message> = if app.config.push_to_talk {
        let label = if model.muted {
            format!("Hold {} to talk", app.config.ptt_key)
        } else {
            "Talking".to_owned()
        };
        button(text(label).size(14))
            .padding([8, 14])
            .style(toggle_style(!model.muted))
            .into()
    } else {
        action(
            if model.muted { "Unmute" } else { "Mute" },
            toggle_style(model.muted),
        )
        .on_press(Message::ToggleMute)
        .into()
    };
    let controls = row![
        mute,
        action(
            if model.deafened { "Undeafen" } else { "Deafen" },
            toggle_style(model.deafened),
        )
        .on_press(Message::ToggleDeafen),
        share,
        space().width(SPACING),
        small_label("Mic"),
        // The level meter sits under the slider rail, clear of the percentage.
        container(
            column![
                mic_gain_control(app.config.input_gain),
                container(meter::level_meter(mic_level(model), 5.0, 2))
                    .padding(Padding::ZERO.right(PERCENT_WIDTH + SPACING)),
            ]
            .spacing(4)
        )
        .max_width(200),
        space().width(SPACING),
        checkbox(app.config.noise_suppression)
            .label("Noise suppression")
            .size(16)
            .text_size(14)
            .style(style::check)
            .on_toggle(Message::NoiseSuppression),
        space().width(Length::Fill),
        action("Audio", style::secondary_button).on_press(Message::OpenAudioPanel),
        action("Leave", style::danger_button).on_press(Message::Leave),
    ]
    .spacing(SPACING)
    .align_y(Alignment::Center);
    container(controls)
        .padding(10)
        .width(Length::Fill)
        .style(style::card)
        .into()
}

fn share_stage(room: &RoomScreen) -> Element<'_, Message> {
    if !room.share_frame.has_frame() {
        return container(
            text("Waiting for the first frame...")
                .size(14)
                .style(style::muted),
        )
        .center(Length::Fill)
        .style(style::card)
        .into();
    }
    Shader::new(ShareView {
        latest: room.share_frame.clone(),
    })
    .width(Length::Fill)
    .height(Length::Fill)
    .into()
}

fn room_view<'a>(app: &'a App, room: &'a RoomScreen) -> Element<'a, Message> {
    let model = &room.model;
    if app.share_fullscreen && model.remote_share_active() {
        return column![
            container(share_stage(room))
                .width(Length::Fill)
                .height(Length::Fill),
            row![
                space().width(Length::Fill),
                action("Exit fullscreen", style::secondary_button)
                    .on_press(Message::ExitShareFullscreen),
            ]
            .spacing(SPACING)
            .align_y(Alignment::Center),
        ]
        .spacing(SPACING)
        .padding(PADDING)
        .height(Length::Fill)
        .into();
    }

    let mut status = Vec::new();
    if let Some(reconnecting) = &model.reconnecting {
        status.push(reconnecting.clone());
    } else if model.me.is_none() {
        status.push("Joining...".to_owned());
    } else {
        status.push(format!("{} in room", model.members.len() + 1));
    }
    if let Some(publisher) = model.screen_publisher {
        let who = model.name_of(publisher).unwrap_or("someone");
        status.push(format!("{who} is sharing"));
    }
    let mut header = row![
        column![
            text(model.name.as_str()).size(20).font(style::SEMIBOLD),
            text(status.join(" · ")).size(13).style(style::muted),
        ]
        .spacing(2)
        .width(Length::Fill)
    ]
    .spacing(SPACING)
    .align_y(Alignment::Center);
    if model.remote_share_active() {
        header = header.push(
            action("Fullscreen", style::ghost_button).on_press(Message::ToggleShareFullscreen),
        );
    }
    header = header.push(display_controls(&app.config));

    let center: Element<'_, Message> = if model.remote_share_active() {
        row![
            container(share_stage(room))
                .width(Length::FillPortion(3))
                .height(Length::Fill),
            container(chat_panel(room, app.is_dark(), &app.config.emoji_usage))
                .width(Length::Fixed(300.0)),
        ]
        .spacing(SPACING + 4)
        .into()
    } else {
        chat_panel(room, app.is_dark(), &app.config.emoji_usage)
    };

    let sharing_banner: Option<Element<'_, Message>> = model.local_sharing.then(|| {
        container(
            row![
                text("You are sharing your screen")
                    .size(14)
                    .width(Length::Fill),
                action("Stop sharing", style::danger_button).on_press(Message::StopShare),
            ]
            .spacing(SPACING)
            .align_y(Alignment::Center),
        )
        .padding([6, 12])
        .width(Length::Fill)
        .style(style::sharing_banner)
        .into()
    });

    column![header]
        .extend(sharing_banner)
        .push(
            row![members_panel(room), center]
                .spacing(SPACING + 4)
                .height(Length::Fill),
        )
        .push(bottom_bar(app, room))
        .spacing(SPACING + 4)
        .padding(PADDING)
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_device_stays_visible_but_is_not_offered() {
        let names = vec!["Mic A".to_owned()];
        let (options, label) = device_choice(&names, Some("Mic A"));
        assert_eq!(options, [SYSTEM_DEFAULT, "Mic A"]);
        assert_eq!(label, "Mic A");
        assert_eq!(device_choice(&names, None).1, SYSTEM_DEFAULT);
        let (options, label) = device_choice(&names, Some("Mic B"));
        assert_eq!(label, "Mic B (not found)");
        assert!(!options.contains(&label));
        // Not listed yet: no claim that it is missing.
        assert_eq!(device_choice(&[], Some("Mic B")).1, "Mic B");
    }
}
