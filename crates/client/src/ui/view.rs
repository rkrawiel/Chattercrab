//! View functions for the three screens. Plain widgets, no animations.

use chattercrab_media::{PEER_VOLUME_MAX, PeerConnectionState, ScreenContent, ScreenScale};
use iced::widget::image::FilterMethod;
use iced::widget::{
    Column, button, checkbox, column, container, image, pick_list, row, scrollable, slider, space,
    text, text_input, toggler,
};
use iced::{Alignment, Background, Border, Color, ContentFit, Element, Length, Theme};

use super::room_model::MemberView;
use super::{App, Lobby, Message, RoomScreen, SYSTEM_DEFAULT, Screen, SharePicker, selectable};
use crate::config::ThemeChoice;

const SPACING: u32 = 8;
const PADDING: u16 = 12;
const CONTENT_CHOICES: [ScreenContent; 2] = [ScreenContent::Desktop, ScreenContent::Game];
const FPS_CHOICES: [u32; 6] = [5, 10, 15, 20, 30, 60];
const FPS_CHOICES_HQ: [u32; 7] = [5, 10, 15, 20, 30, 60, 120];
const SCALE_CHOICES: [ScreenScale; 4] = [
    ScreenScale::P4K,
    ScreenScale::P2K,
    ScreenScale::P1440,
    ScreenScale::P1080,
];

pub(super) fn view(app: &App) -> Element<'_, Message> {
    let body = match &app.state.screen {
        Screen::Connect { connecting } => connect_view(app, *connecting),
        Screen::Lobby(lobby) => lobby_view(lobby),
        Screen::Room(room) => room_view(
            room,
            app.config.noise_suppression,
            app.config.input_gain,
            app.share_fullscreen,
        ),
    };
    let mut page = Column::new().push(container(body).height(Length::Fill));
    if let Some(notice) = &app.state.notice {
        page = page.push(
            container(
                row![
                    text(notice).width(Length::Fill),
                    button("Dismiss")
                        .style(button::text)
                        .on_press(Message::DismissNotice),
                ]
                .spacing(SPACING)
                .align_y(Alignment::Center),
            )
            .padding([4, PADDING])
            .style(container::bordered_box),
        );
    }
    page.into()
}

fn device_options(names: &[String]) -> Vec<String> {
    std::iter::once(SYSTEM_DEFAULT.to_owned())
        .chain(names.iter().cloned())
        .collect()
}

fn connect_view(app: &App, connecting: bool) -> Element<'_, Message> {
    let config = &app.config;
    let inputs = device_options(&app.state.inputs);
    let outputs = device_options(&app.state.outputs);
    let selected_input = config
        .input_device
        .clone()
        .unwrap_or_else(|| SYSTEM_DEFAULT.to_owned());
    let selected_output = config
        .output_device
        .clone()
        .unwrap_or_else(|| SYSTEM_DEFAULT.to_owned());

    let connect = button(if connecting {
        "Connecting..."
    } else {
        "Connect"
    })
    .on_press_maybe((!connecting).then_some(Message::Connect));

    let form = column![
        text("Chattercrab").size(28),
        labeled(
            "Server",
            text_input("https://chattercrab.qcodefactory.com", &config.server_url)
                .on_input(Message::ServerUrlChanged)
                .on_submit(Message::Connect)
                .into(),
        ),
        labeled(
            "User",
            text_input("Username", &config.auth_user)
                .on_input(Message::AuthUserChanged)
                .on_submit(Message::Connect)
                .into(),
        ),
        labeled(
            "Password",
            text_input("Shared password", &config.password)
                .secure(true)
                .on_input(Message::PasswordChanged)
                .on_submit(Message::Connect)
                .into(),
        ),
        labeled(
            "Display name",
            text_input("Your name (1-32 characters)", &config.display_name)
                .on_input(Message::DisplayNameChanged)
                .on_submit(Message::Connect)
                .into(),
        ),
        labeled(
            "Microphone",
            pick_list(inputs, Some(selected_input), Message::InputSelected)
                .width(Length::Fill)
                .into(),
        ),
        labeled("Mic volume", mic_gain_control(config.input_gain)),
        labeled(
            "Speakers",
            pick_list(outputs, Some(selected_output), Message::OutputSelected)
                .width(Length::Fill)
                .into(),
        ),
        checkbox(config.noise_suppression)
            .label("Noise suppression (removes background noise, keeps voice)")
            .on_toggle(Message::NoiseSuppression),
        row![
            toggler(config.theme == ThemeChoice::Dark)
                .label("Dark theme")
                .on_toggle(Message::DarkTheme),
            space().width(Length::Fill),
            button("Refresh devices")
                .style(button::secondary)
                .on_press(Message::RefreshDevices),
            connect,
        ]
        .spacing(SPACING)
        .align_y(Alignment::Center),
    ]
    .spacing(SPACING * 2)
    .max_width(480);

    container(form).center(Length::Fill).padding(PADDING).into()
}

fn mic_gain_control(gain: f32) -> Element<'static, Message> {
    row![
        slider(0.0..=5.0, gain, Message::InputGainChanged)
            .step(0.05_f32)
            .on_release(Message::InputGainReleased)
            .width(Length::Fill),
        text(format!("{:>3}%", (gain * 100.0).round() as u32)).size(14),
    ]
    .spacing(SPACING)
    .align_y(Alignment::Center)
    .into()
}

fn labeled<'a>(label: &'a str, field: Element<'a, Message>) -> Element<'a, Message> {
    column![text(label).size(14), field].spacing(4).into()
}

fn lobby_view(lobby: &Lobby) -> Element<'_, Message> {
    let header = row![
        text("Rooms").size(24).width(Length::Fill),
        button("Refresh")
            .style(button::secondary)
            .on_press(Message::RefreshRooms),
        button("Disconnect")
            .style(button::secondary)
            .on_press(Message::Disconnect),
    ]
    .spacing(SPACING)
    .align_y(Alignment::Center);

    let mut list = Column::new().spacing(4);
    if lobby.rooms.is_empty() {
        list = list.push(text("No rooms yet. Create one below."));
    }
    for room in &lobby.rooms {
        list = list.push(
            container(
                row![
                    text(room.name.as_str()).width(Length::Fill),
                    text(format!(
                        "{} member{}",
                        room.member_count,
                        if room.member_count == 1 { "" } else { "s" }
                    )),
                    button("Join").on_press(Message::JoinRoom(room.id.clone(), room.name.clone())),
                ]
                .spacing(SPACING)
                .align_y(Alignment::Center),
            )
            .padding([6, PADDING])
            .style(container::bordered_box),
        );
    }

    let create = row![
        text_input("New room name", &lobby.new_room)
            .on_input(Message::NewRoomChanged)
            .on_submit(Message::CreateRoom),
        button("Create and join").on_press(Message::CreateRoom),
    ]
    .spacing(SPACING);

    column![header, scrollable(list).height(Length::Fill), create]
        .spacing(SPACING * 2)
        .padding(PADDING)
        .into()
}

fn dot(on: bool) -> Element<'static, Message> {
    container(space().width(10).height(10))
        .style(move |theme: &Theme| {
            let palette = theme.palette();
            let color = if on {
                palette.success
            } else {
                Color {
                    a: 0.25,
                    ..palette.text
                }
            };
            container::Style::default()
                .background(Background::Color(color))
                .border(Border::default().rounded(5))
        })
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

fn member_row(member: &MemberView) -> Element<'_, Message> {
    let id = member.info.id;
    column![
        row![
            dot(member.speaking() && !member.muted),
            text(member.info.display_name.as_str()).width(Length::Fill),
            text(state_label(member.state)).size(12),
        ]
        .spacing(SPACING)
        .align_y(Alignment::Center),
        row![
            slider(0.0..=PEER_VOLUME_MAX, member.volume, move |v| {
                Message::PeerVolumeChanged(id, v)
            })
            .step(0.05_f32)
            .on_release(Message::PeerVolumeCommitted(id))
            .width(Length::Fill),
            text(format!("{:>3}%", (member.volume * 100.0).round() as u32)).size(12),
            checkbox(member.muted)
                .label("Mute")
                .text_size(12)
                .on_toggle(move |m| Message::PeerMuteToggled(id, m)),
        ]
        .spacing(SPACING)
        .align_y(Alignment::Center),
    ]
    .spacing(4)
    .into()
}

fn members_panel(room: &RoomScreen) -> Element<'_, Message> {
    let model = &room.model;
    let mut list = Column::new().spacing(SPACING * 2);
    let me_label = if model.muted {
        format!("{} (you, muted)", model.my_name)
    } else {
        format!("{} (you)", model.my_name)
    };
    list = list.push(
        row![
            dot(!model.muted && model.local_level >= super::room_model::SPEAKING_THRESHOLD),
            text(me_label)
        ]
        .spacing(SPACING)
        .align_y(Alignment::Center),
    );
    for member in &model.members {
        list = list.push(member_row(member));
    }
    container(scrollable(list).height(Length::Fill))
        .width(240)
        .height(Length::Fill)
        .padding(PADDING)
        .style(container::bordered_box)
        .into()
}

fn chat_panel(room: &RoomScreen) -> Element<'_, Message> {
    let model = &room.model;
    let mut log = Column::new().spacing(4).width(Length::Fill);
    for msg in &model.chat {
        log = log.push(selectable::message(
            msg.id,
            msg.display_name.as_str(),
            msg.text.as_str(),
        ));
    }
    let input = text_input("Message (Enter to send)", &room.chat_input)
        .on_input(Message::ChatChanged)
        .on_submit(Message::SendChat);
    column![
        scrollable(log)
            .anchor_bottom()
            .height(Length::Fill)
            .width(Length::Fill),
        input
    ]
    .spacing(SPACING)
    .height(Length::Fill)
    .into()
}

fn share_picker(picker: &SharePicker) -> Element<'_, Message> {
    let sources: Element<'_, Message> = if picker.sources.is_empty() {
        text("Looking for screens and windows...").into()
    } else {
        pick_list(
            picker.sources.as_slice(),
            picker.selected.clone(),
            Message::ShareSourceSelected,
        )
        .width(Length::Fill)
        .into()
    };
    let fps_choices: Vec<u32> = if picker.high_quality {
        FPS_CHOICES_HQ.to_vec()
    } else {
        FPS_CHOICES.to_vec()
    };
    let controls = row![
        sources,
        text("mode"),
        pick_list(
            CONTENT_CHOICES,
            Some(picker.content),
            Message::ShareContentSelected,
        ),
        text("fps"),
        pick_list(fps_choices, Some(picker.fps), Message::ShareFpsSelected),
        text("size"),
        pick_list(
            SCALE_CHOICES,
            Some(picker.scale),
            Message::ShareScaleSelected,
        ),
        button("Start sharing")
            .on_press_maybe(picker.selected.is_some().then_some(Message::StartShare)),
        button("Cancel")
            .style(button::secondary)
            .on_press(Message::CancelShare),
    ]
    .spacing(SPACING)
    .align_y(Alignment::Center);
    let mut picker_column = column![
        controls,
        row![
            checkbox(picker.high_quality)
                .label("High quality")
                .on_toggle(Message::ShareHighQualityToggled),
            checkbox(picker.include_audio)
                .label("Share audio from other apps (this call is not included)")
                .on_toggle(Message::ShareAudioToggled),
        ]
        .spacing(SPACING)
        .align_y(Alignment::Center),
    ]
    .spacing(SPACING);
    let mode_hint = match picker.content {
        ScreenContent::Desktop => {
            "Crisp fonts and UI. Resolution and frame rate stay as you set them."
        }
        ScreenContent::Game => {
            "Less blur on moving images. Resolution and frame rate stay as you set them."
        }
    };
    picker_column = picker_column.push(text(mode_hint).size(14));
    if picker.high_quality {
        picker_column = picker_column.push(text("Higher bitrate and low delay.").size(14));
    }
    picker_column.into()
}

fn bottom_bar(room: &RoomScreen, noise_suppression: bool, input_gain: f32) -> Element<'_, Message> {
    if let Some(picker) = &room.picker {
        return share_picker(picker);
    }
    let model = &room.model;
    let share = if model.local_sharing {
        button("Stop sharing").on_press(Message::StopShare)
    } else {
        // Only one publisher per room; the button is disabled while someone shares.
        let free = model.screen_publisher.is_none() && model.me.is_some();
        button("Share screen").on_press_maybe(free.then_some(Message::OpenSharePicker))
    };
    let toggle_style = |active: bool| {
        if active {
            button::danger
        } else {
            button::secondary
        }
    };
    let controls = row![
        button(if model.muted { "Unmute" } else { "Mute" })
            .style(toggle_style(model.muted))
            .on_press(Message::ToggleMute),
        button(if model.deafened { "Undeafen" } else { "Deafen" })
            .style(toggle_style(model.deafened))
            .on_press(Message::ToggleDeafen),
        share,
        checkbox(noise_suppression)
            .label("Noise suppression")
            .on_toggle(Message::NoiseSuppression),
        space().width(Length::Fill),
        button("Leave")
            .style(button::danger)
            .on_press(Message::Leave),
    ]
    .spacing(SPACING)
    .align_y(Alignment::Center);
    column![
        controls,
        labeled("Mic volume", mic_gain_control(input_gain))
    ]
    .spacing(SPACING)
    .into()
}

fn share_stage(room: &RoomScreen) -> Element<'_, Message> {
    match &room.frame {
        Some(handle) => image(handle.clone())
            .filter_method(FilterMethod::Nearest)
            .content_fit(ContentFit::Contain)
            .width(Length::Fill)
            .height(Length::Fill)
            .into(),
        None => container(text("Waiting for the first frame..."))
            .center(Length::Fill)
            .into(),
    }
}

fn room_view(
    room: &RoomScreen,
    noise_suppression: bool,
    input_gain: f32,
    fullscreen: bool,
) -> Element<'_, Message> {
    let model = &room.model;
    if fullscreen && model.remote_share_active() {
        return column![
            container(share_stage(room))
                .width(Length::Fill)
                .height(Length::Fill),
            row![
                space().width(Length::Fill),
                button("Exit fullscreen").on_press(Message::ExitShareFullscreen),
            ]
            .spacing(SPACING)
            .align_y(Alignment::Center),
        ]
        .spacing(SPACING)
        .padding(PADDING)
        .height(Length::Fill)
        .into();
    }

    let mut header = row![text(model.name.as_str()).size(22).width(Length::Fill)]
        .spacing(SPACING)
        .align_y(Alignment::Center);
    if model.me.is_none() && model.reconnecting.is_none() {
        header = header.push(text("Joining..."));
    }
    if let Some(status) = &model.reconnecting {
        header = header.push(text(status.as_str()));
    }
    if let Some(publisher) = model.screen_publisher {
        let who = model.name_of(publisher).unwrap_or("someone");
        header = header.push(text(format!("{who} is sharing")).size(14));
    }
    if model.remote_share_active() {
        header = header.push(button("Fullscreen").on_press(Message::ToggleShareFullscreen));
    }

    let center: Element<'_, Message> = if model.remote_share_active() {
        row![
            container(share_stage(room))
                .width(Length::FillPortion(3))
                .height(Length::Fill),
            container(chat_panel(room)).width(Length::Fixed(300.0)),
        ]
        .spacing(SPACING)
        .into()
    } else {
        chat_panel(room)
    };

    column![
        header,
        row![members_panel(room), center]
            .spacing(SPACING)
            .height(Length::Fill),
        bottom_bar(room, noise_suppression, input_gain),
    ]
    .spacing(SPACING)
    .padding(PADDING)
    .into()
}
