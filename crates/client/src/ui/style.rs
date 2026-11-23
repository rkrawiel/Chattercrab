//! Themes and widget styles: two custom palettes, rounded surfaces, quiet controls.

use std::sync::LazyLock;

use iced::theme::Palette;
use iced::widget::{
    button, checkbox, container, overlay, pick_list, scrollable, slider, text, text_input,
};
use iced::{Background, Border, Color, Font, Shadow, Theme, Vector, border, color, font};

/// Corner radius of panels and cards.
pub const RADIUS_LG: f32 = 12.0;
/// Corner radius of buttons and fields.
pub const RADIUS: f32 = 8.0;

/// Default font at semibold weight, for titles and names.
pub const SEMIBOLD: Font = Font {
    weight: font::Weight::Semibold,
    ..Font::DEFAULT
};

/// Built once; [`Theme::custom`] allocates and derives the extended palette.
pub static DARK: LazyLock<Theme> = LazyLock::new(|| {
    Theme::custom(
        "Chattercrab Dark",
        Palette {
            background: color!(0x111318),
            text: color!(0xe7e9ee),
            primary: color!(0x6d7cff),
            success: color!(0x3ecf8e),
            warning: color!(0xf5b544),
            danger: color!(0xf0565c),
        },
    )
});

/// Built once; see [`DARK`].
pub static LIGHT: LazyLock<Theme> = LazyLock::new(|| {
    Theme::custom(
        "Chattercrab Light",
        Palette {
            background: color!(0xf3f4f7),
            text: color!(0x16181d),
            primary: color!(0x4f5bd5),
            success: color!(0x1f9d63),
            warning: color!(0xc98a0b),
            danger: color!(0xd93a42),
        },
    )
});

fn alpha(color: Color, a: f32) -> Color {
    Color { a, ..color }
}

fn is_dark(theme: &Theme) -> bool {
    theme.extended_palette().is_dark
}

/// Raised surface color: lighter than the window in dark mode, white in light mode.
fn surface(theme: &Theme) -> Color {
    if is_dark(theme) {
        color!(0x1a1d24)
    } else {
        Color::WHITE
    }
}

/// Field and inset color, one step off the surface.
fn inset(theme: &Theme) -> Color {
    if is_dark(theme) {
        color!(0x222630)
    } else {
        color!(0xeef0f4)
    }
}

/// Hairline border color.
fn hairline(theme: &Theme) -> Color {
    alpha(
        theme.palette().text,
        if is_dark(theme) { 0.08 } else { 0.10 },
    )
}

/// Secondary text color.
pub fn muted_color(theme: &Theme) -> Color {
    alpha(theme.palette().text, 0.6)
}

/// Secondary text: labels, hints, counts.
pub fn muted(theme: &Theme) -> text::Style {
    text::Style {
        color: Some(muted_color(theme)),
    }
}

/// Panels and cards.
pub fn card(theme: &Theme) -> container::Style {
    container::Style::default()
        .background(surface(theme))
        .border(border::rounded(RADIUS_LG).width(1).color(hairline(theme)))
}

/// The notice bar and tooltips: a card with a soft shadow.
pub fn floating(theme: &Theme) -> container::Style {
    container::Style {
        shadow: Shadow {
            color: alpha(Color::BLACK, if is_dark(theme) { 0.4 } else { 0.12 }),
            offset: Vector::new(0.0, 4.0),
            blur_radius: 16.0,
        },
        ..card(theme)
    }
}

/// A menu floating over a card (emoji picker, suggestions): one step off the card, with a
/// stronger border and a shadow so it stands out.
pub fn popup(theme: &Theme) -> container::Style {
    container::Style {
        shadow: Shadow {
            color: alpha(Color::BLACK, if is_dark(theme) { 0.5 } else { 0.16 }),
            offset: Vector::new(0.0, 4.0),
            blur_radius: 16.0,
        },
        ..container::Style::default().background(inset(theme)).border(
            border::rounded(RADIUS_LG)
                .width(1)
                .color(alpha(theme.palette().text, 0.22)),
        )
    }
}

/// A one pixel separator line.
pub fn divider(theme: &Theme) -> container::Style {
    container::Style::default().background(alpha(theme.palette().text, 0.16))
}

fn filled(background: Color, text_color: Color, status: button::Status) -> button::Style {
    let background = match status {
        button::Status::Hovered => Color {
            a: background.a * 0.88,
            ..background
        },
        button::Status::Pressed => Color {
            a: background.a * 0.76,
            ..background
        },
        _ => background,
    };
    let style = button::Style {
        background: Some(Background::Color(background)),
        text_color,
        border: border::rounded(RADIUS),
        ..button::Style::default()
    };
    if status == button::Status::Disabled {
        button::Style {
            background: style.background.map(|b| b.scale_alpha(0.4)),
            text_color: alpha(text_color, 0.5),
            ..style
        }
    } else {
        style
    }
}

/// Main action.
pub fn primary_button(theme: &Theme, status: button::Status) -> button::Style {
    filled(theme.palette().primary, Color::WHITE, status)
}

/// Destructive action.
pub fn danger_button(theme: &Theme, status: button::Status) -> button::Style {
    filled(theme.palette().danger, Color::WHITE, status)
}

/// Neutral action on a quiet inset background.
pub fn secondary_button(theme: &Theme, status: button::Status) -> button::Style {
    let text_color = theme.palette().text;
    let style = filled(inset(theme), text_color, status);
    match status {
        button::Status::Hovered | button::Status::Pressed => button::Style {
            background: Some(Background::Color(alpha(text_color, 0.12))),
            ..style
        },
        _ => style,
    }
}

/// A switched-on toggle like Mute or Deafen: tinted with the danger color.
pub fn active_toggle_button(theme: &Theme, status: button::Status) -> button::Style {
    let danger = theme.palette().danger;
    let a = match status {
        button::Status::Hovered | button::Status::Pressed => 0.24,
        _ => 0.16,
    };
    filled(alpha(danger, a), danger, status)
}

/// Transparent until hovered; for low-emphasis actions and icon buttons.
pub fn ghost_button(theme: &Theme, status: button::Status) -> button::Style {
    let text_color = theme.palette().text;
    let background = match status {
        button::Status::Hovered => alpha(text_color, 0.08),
        button::Status::Pressed => alpha(text_color, 0.14),
        _ => Color::TRANSPARENT,
    };
    filled(background, text_color, status)
}

/// Round icon button.
pub fn icon_button(theme: &Theme, status: button::Status) -> button::Style {
    button::Style {
        border: border::rounded(u32::MAX),
        ..ghost_button(theme, status)
    }
}

/// Text fields.
pub fn input(theme: &Theme, status: text_input::Status) -> text_input::Style {
    let palette = theme.palette();
    let border_color = match status {
        text_input::Status::Focused { .. } => palette.primary,
        text_input::Status::Hovered => alpha(palette.text, 0.2),
        _ => Color::TRANSPARENT,
    };
    text_input::Style {
        background: Background::Color(inset(theme)),
        border: border::rounded(RADIUS).width(1).color(border_color),
        icon: muted_color(theme),
        placeholder: alpha(palette.text, 0.4),
        value: palette.text,
        selection: alpha(palette.primary, 0.35),
    }
}

/// Checkboxes: a visible outline when off, filled with the primary color when on.
pub fn check(theme: &Theme, status: checkbox::Status) -> checkbox::Style {
    let palette = theme.palette();
    let (is_checked, hovered) = match status {
        checkbox::Status::Active { is_checked } | checkbox::Status::Disabled { is_checked } => {
            (is_checked, false)
        }
        checkbox::Status::Hovered { is_checked } => (is_checked, true),
    };
    let (background, border_color) = if is_checked {
        (palette.primary, palette.primary)
    } else {
        (
            inset(theme),
            alpha(palette.text, if hovered { 0.5 } else { 0.3 }),
        )
    };
    checkbox::Style {
        background: Background::Color(background),
        icon_color: Color::WHITE,
        border: border::rounded(4).width(1).color(border_color),
        text_color: None,
    }
}

/// Drop-down fields.
pub fn pick(theme: &Theme, status: pick_list::Status) -> pick_list::Style {
    let palette = theme.palette();
    let border_color = match status {
        pick_list::Status::Opened { .. } => palette.primary,
        pick_list::Status::Hovered => alpha(palette.text, 0.2),
        pick_list::Status::Active => Color::TRANSPARENT,
    };
    pick_list::Style {
        text_color: palette.text,
        placeholder_color: alpha(palette.text, 0.4),
        handle_color: muted_color(theme),
        background: Background::Color(inset(theme)),
        border: border::rounded(RADIUS).width(1).color(border_color),
    }
}

/// Drop-down menus.
pub fn menu(theme: &Theme) -> overlay::menu::Style {
    let palette = theme.palette();
    overlay::menu::Style {
        background: Background::Color(surface(theme)),
        border: border::rounded(RADIUS).width(1).color(hairline(theme)),
        text_color: palette.text,
        selected_text_color: palette.text,
        selected_background: Background::Color(alpha(palette.primary, 0.22)),
        shadow: Shadow {
            color: alpha(Color::BLACK, 0.25),
            offset: Vector::new(0.0, 4.0),
            blur_radius: 12.0,
        },
    }
}

/// Volume sliders: thin rail, round handle.
pub fn volume(theme: &Theme, status: slider::Status) -> slider::Style {
    let palette = theme.palette();
    let handle = match status {
        slider::Status::Active => palette.primary,
        slider::Status::Hovered | slider::Status::Dragged => {
            theme.extended_palette().primary.strong.color
        }
    };
    slider::Style {
        rail: slider::Rail {
            backgrounds: (palette.primary.into(), alpha(palette.text, 0.12).into()),
            width: 4.0,
            border: border::rounded(2),
        },
        handle: slider::Handle {
            shape: slider::HandleShape::Circle { radius: 7.0 },
            background: handle.into(),
            border_width: 2.0,
            border_color: surface(theme),
        },
    }
}

/// Thin scrollbars that only show a scroller.
pub fn scroll(theme: &Theme, status: scrollable::Status) -> scrollable::Style {
    let mut style = scrollable::default(theme, status);
    let hovered = matches!(
        status,
        scrollable::Status::Hovered {
            is_vertical_scrollbar_hovered: true,
            ..
        } | scrollable::Status::Dragged {
            is_vertical_scrollbar_dragged: true,
            ..
        }
    );
    let rail = scrollable::Rail {
        background: None,
        border: Border::default(),
        scroller: scrollable::Scroller {
            background: alpha(theme.palette().text, if hovered { 0.35 } else { 0.18 }).into(),
            border: border::rounded(u32::MAX),
        },
    };
    style.vertical_rail = rail;
    style.horizontal_rail = rail;
    style
}

/// Avatar colors, picked by name.
const AVATAR_COLORS: [Color; 8] = [
    color!(0x6d7cff),
    color!(0xe0679b),
    color!(0x2fb3a0),
    color!(0xf08a4b),
    color!(0x9b6dff),
    color!(0x3d9be9),
    color!(0xd4a72c),
    color!(0x5fb35f),
];

/// A stable color for a display name.
pub fn avatar_color(name: &str) -> Color {
    let hash = name
        .bytes()
        .fold(0_u32, |h, b| h.wrapping_mul(31).wrapping_add(u32::from(b)));
    AVATAR_COLORS[hash as usize % AVATAR_COLORS.len()]
}

/// An avatar or room tile; a green ring shows that the member is speaking.
pub fn avatar(theme: &Theme, fill: Color, radius: f32, speaking: bool) -> container::Style {
    let ring = if speaking {
        theme.palette().success
    } else {
        Color::TRANSPARENT
    };
    container::Style::default()
        .background(fill)
        .color(Color::WHITE)
        .border(border::rounded(radius).width(2).color(ring))
}

/// Loudness band of a level meter segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MeterZone {
    /// Normal speech.
    Normal,
    /// Loud.
    Loud,
    /// Close to clipping.
    Hot,
}

/// One segment of the microphone level meter: green, yellow or red when lit, a dim
/// rail color when not.
pub fn meter_segment(theme: &Theme, zone: MeterZone, lit: bool) -> container::Style {
    let palette = theme.palette();
    let fill = match (lit, zone) {
        (false, _) => alpha(palette.text, 0.12),
        (true, MeterZone::Normal) => palette.success,
        (true, MeterZone::Loud) => palette.warning,
        (true, MeterZone::Hot) => palette.danger,
    };
    container::Style::default()
        .background(fill)
        .border(border::rounded(2))
}
