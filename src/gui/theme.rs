//! Material 3–style dark theme in Crunchyroll orange. Surfaces are near-neutral dark
//! steps above a black background, and the accent is Crunchyroll's orange (#F47521).

use iced::font::{Family, Weight};
use iced::theme::Palette;
use iced::widget::{button, checkbox, container, progress_bar, scrollable, text_input, toggler};
use iced::{Background, Border, Color, Font, Shadow, Theme, Vector};

/// Material Symbols Rounded: the icon font. Codepoints are the standard Material ones.
pub const ICON_FONT: Font = Font::with_name("Material Symbols Rounded");
pub const HOME: char = '\u{e88a}';
pub const SETTINGS: char = '\u{e8b8}';
pub const DESCRIPTION: char = '\u{e873}';
pub const LOCK: char = '\u{e897}';
pub const LOCK_OPEN: char = '\u{e898}';
pub const STOP: char = '\u{e047}';
pub const FOLDER: char = '\u{e2c7}';
pub const DOWNLOAD: char = '\u{e2c4}';
pub const DELETE: char = '\u{e872}';
pub const VISIBILITY: char = '\u{e8f4}';
pub const VISIBILITY_OFF: char = '\u{e8f5}';
pub const EXPAND_MORE: char = '\u{e5cf}';
pub const EXPAND_LESS: char = '\u{e5ce}';
pub const INFO: char = '\u{e88e}';
pub const CLOSE: char = '\u{e5cd}';
pub const LANGUAGE: char = '\u{e894}';
pub const TUNE: char = '\u{e429}';
pub const SEARCH: char = '\u{e8b6}';
pub const CHECK: char = '\u{e5ca}';
pub const EXPLORE: char = '\u{e87a}';
pub const ARROW_BACK: char = '\u{e5c4}';
pub const MOVIE: char = '\u{e02c}';
pub const LOGIN: char = '\u{ea77}';
pub const LOGOUT: char = '\u{e9ba}';
pub const HOURGLASS: char = '\u{e88b}';
pub const UPDATE: char = '\u{e923}';

/// An icon glyph as a text widget. Set the color with `.color(..)`.
pub fn icon<'a>(glyph: char, size: f32) -> iced::widget::Text<'a> {
    iced::widget::text(glyph.to_string()).font(ICON_FONT).size(size)
}

/// Roboto Bold, for headings.
pub fn bold() -> Font {
    Font { weight: Weight::Bold, ..Font::with_name("Roboto") }
}

/// Roboto Medium, for labels and buttons.
pub fn medium() -> Font {
    Font { weight: Weight::Medium, family: Family::Name("Roboto"), ..Font::DEFAULT }
}

pub fn background() -> Color {
    Color::from_rgb8(0x0F, 0x0F, 0x12)
}
/// Navigation rail and the lowest surface.
pub fn rail_color() -> Color {
    Color::from_rgb8(0x09, 0x09, 0x0C)
}
/// Cards.
pub fn surface() -> Color {
    Color::from_rgb8(0x1A, 0x1A, 0x1F)
}
/// Inputs and nested panels.
pub fn surface_high() -> Color {
    Color::from_rgb8(0x24, 0x24, 0x2B)
}
/// Buttons that sit on a card.
pub fn surface_highest() -> Color {
    Color::from_rgb8(0x30, 0x30, 0x39)
}
pub fn outline() -> Color {
    Color::from_rgb8(0x45, 0x45, 0x50)
}
/// Crunchyroll orange.
pub fn accent() -> Color {
    Color::from_rgb8(0xF4, 0x75, 0x21)
}
/// Near-black, for text and icons that sit on the orange accent.
pub fn on_accent() -> Color {
    Color::from_rgb8(0x24, 0x11, 0x00)
}
/// A deep, muted orange for selected chips and the nav pill.
pub fn accent_container() -> Color {
    Color::from_rgb8(0x4A, 0x2A, 0x10)
}
pub fn on_accent_container() -> Color {
    Color::from_rgb8(0xFF, 0xD8, 0xB4)
}
pub fn text() -> Color {
    Color::from_rgb8(0xED, 0xEA, 0xE6)
}
pub fn muted() -> Color {
    Color::from_rgb8(0xA6, 0xA2, 0x9C)
}
pub fn success() -> Color {
    Color::from_rgb8(0x9A, 0xE0, 0xA8)
}
pub fn warning() -> Color {
    Color::from_rgb8(0xF5, 0xC9, 0x7A)
}
pub fn danger() -> Color {
    Color::from_rgb8(0xFF, 0xB0, 0x9C)
}
pub fn danger_container() -> Color {
    Color::from_rgb8(0x5C, 0x24, 0x1A)
}

/// The application theme.
pub fn theme() -> Theme {
    Theme::custom(
        "Crunchyroll Orange".to_string(),
        Palette {
            background: background(),
            text: text(),
            primary: accent(),
            success: success(),
            warning: warning(),
            danger: danger(),
        },
    )
}

fn mix(c: Color, toward: Color, t: f32) -> Color {
    Color {
        r: c.r + (toward.r - c.r) * t,
        g: c.g + (toward.g - c.g) * t,
        b: c.b + (toward.b - c.b) * t,
        a: c.a,
    }
}

fn button_style(bg: Color, fg: Color, status: button::Status, radius: f32) -> button::Style {
    let (bg, fg) = match status {
        button::Status::Disabled => (Color { a: 0.12, ..bg }, Color { a: 0.38, ..fg }),
        button::Status::Hovered => (mix(bg, Color::WHITE, 0.08), fg),
        button::Status::Pressed => (mix(bg, Color::WHITE, 0.16), fg),
        button::Status::Active => (bg, fg),
    };
    button::Style {
        background: Some(Background::Color(bg)),
        text_color: fg,
        border: Border { radius: radius.into(), ..Default::default() },
        shadow: Shadow::default(),
        ..Default::default()
    }
}

/// Main call to action: a filled, pill-shaped button.
pub fn filled(_: &Theme, status: button::Status) -> button::Style {
    button_style(accent(), on_accent(), status, 22.0)
}

/// Secondary action: a tonal button.
pub fn tonal(_: &Theme, status: button::Status) -> button::Style {
    button_style(surface_highest(), text(), status, 22.0)
}

/// Destructive action: a tonal red button.
pub fn danger_tonal(_: &Theme, status: button::Status) -> button::Style {
    button_style(danger_container(), danger(), status, 22.0)
}

/// Text-only button.
pub fn ghost(_: &Theme, status: button::Status) -> button::Style {
    let bg = match status {
        button::Status::Hovered => Color { a: 0.08, ..accent() },
        _ => Color::TRANSPARENT,
    };
    button_style(bg, accent(), status, 22.0)
}

/// A Browse poster card: no fill at rest, a soft surface lift on hover so the grid stays
/// calm but clearly clickable.
pub fn poster_card(_: &Theme, status: button::Status) -> button::Style {
    let bg = match status {
        button::Status::Hovered => surface_high(),
        button::Status::Pressed => surface_highest(),
        _ => Color::TRANSPARENT,
    };
    button::Style {
        background: Some(Background::Color(bg)),
        text_color: text(),
        border: Border { radius: 14.0.into(), ..Default::default() },
        ..Default::default()
    }
}

/// The grey box shown while a poster loads, or when it has none.
pub fn poster_placeholder(_: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(surface_high())),
        border: Border { radius: 10.0.into(), ..Default::default() },
        ..Default::default()
    }
}

/// A sort/filter choice: filled orange when selected, a quiet chip otherwise.
pub fn choice(selected: bool) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |theme, status| if selected { filled(theme, status) } else { tonal(theme, status) }
}

/// Small selectable chip, for chosen languages.
pub fn chip(_: &Theme, status: button::Status) -> button::Style {
    button_style(accent_container(), on_accent_container(), status, 16.0)
}

/// The field that opens a dropdown: a rounded tonal button with an accent outline while open.
pub fn select_field(open: bool) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_, status| {
        let lifted = open || matches!(status, button::Status::Hovered | button::Status::Pressed);
        button::Style {
            background: Some(Background::Color(if lifted { surface_highest() } else { surface_high() })),
            text_color: text(),
            border: Border {
                radius: 18.0.into(),
                width: if open { 2.0 } else { 1.0 },
                color: if open { accent() } else { outline() },
            },
            shadow: Shadow::default(),
            ..Default::default()
        }
    }
}

/// One row of an open dropdown. The chosen value is filled with the accent container. The row
/// the keyboard is on (`highlighted`) gets the same tone as a hovered row.
pub fn option_row(selected: bool, highlighted: bool) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_, status| {
        let bg = match (selected, status) {
            (true, _) => accent_container(),
            (false, button::Status::Pressed) => mix(surface_highest(), Color::WHITE, 0.08),
            (false, button::Status::Hovered) => surface_highest(),
            (false, _) if highlighted => surface_highest(),
            _ => Color::TRANSPARENT,
        };
        button::Style {
            background: Some(Background::Color(bg)),
            text_color: if selected { on_accent_container() } else { text() },
            border: Border { radius: 14.0.into(), ..Default::default() },
            shadow: Shadow::default(),
            ..Default::default()
        }
    }
}

/// Background of an open dropdown list. `alpha` fades it in and out with the animation.
pub fn menu_surface(alpha: f32) -> impl Fn(&Theme) -> container::Style {
    move |_| container::Style {
        background: Some(Background::Color(Color { a: alpha, ..surface_highest() })),
        border: Border {
            radius: 18.0.into(),
            width: 1.0,
            color: Color { a: alpha, ..outline() },
        },
        shadow: Shadow {
            color: Color { a: 0.4 * alpha, ..Color::BLACK },
            offset: Vector { x: 0.0, y: 6.0 },
            blur_radius: 16.0,
        },
        ..Default::default()
    }
}

/// Scrollbars: a thin rounded thumb that brightens while it is hovered or dragged.
pub fn scroll(_: &Theme, status: scrollable::Status) -> scrollable::Style {
    let hot = matches!(
        status,
        scrollable::Status::Hovered { is_vertical_scrollbar_hovered: true, .. } | scrollable::Status::Dragged { .. }
    );
    let thumb = Color { a: if hot { 0.75 } else { 0.38 }, ..muted() };
    let rail = scrollable::Rail {
        background: None,
        border: Border { radius: 3.0.into(), ..Default::default() },
        scroller: scrollable::Scroller {
            background: Background::Color(thumb),
            border: Border { radius: 3.0.into(), ..Default::default() },
        },
    };
    scrollable::Style {
        container: container::Style::default(),
        vertical_rail: rail,
        horizontal_rail: rail,
        gap: None,
        auto_scroll: scrollable::AutoScroll {
            background: Background::Color(surface_high()),
            border: Border { radius: 99.0.into(), width: 1.0, color: outline() },
            shadow: Shadow::default(),
            icon: muted(),
        },
    }
}

/// A slim vertical scrollbar that sits close to the content.
pub fn scrollbar() -> scrollable::Direction {
    scrollable::Direction::Vertical(scrollable::Scrollbar::new().width(6).scroller_width(6).margin(2))
}

/// A scrollbar that takes no space and draws nothing. Used while a list animates, so no stub shows.
pub fn scrollbar_hidden() -> scrollable::Direction {
    scrollable::Direction::Vertical(scrollable::Scrollbar::new().width(0).scroller_width(0).margin(0))
}

/// Toggle switch: a filled accent track with a dark knob when on, an outlined track with a
/// muted knob when off.
pub fn toggle(_: &Theme, status: toggler::Status) -> toggler::Style {
    let (on, hovered) = match status {
        toggler::Status::Active { is_toggled } => (is_toggled, false),
        toggler::Status::Hovered { is_toggled } => (is_toggled, true),
        toggler::Status::Disabled { is_toggled } => (is_toggled, false),
    };
    let track = if on {
        accent()
    } else if hovered {
        mix(surface_highest(), Color::WHITE, 0.08)
    } else {
        surface_highest()
    };
    toggler::Style {
        background: Background::Color(track),
        background_border_width: if on { 0.0 } else { 2.0 },
        background_border_color: outline(),
        foreground: Background::Color(if on { on_accent() } else { muted() }),
        foreground_border_width: 0.0,
        foreground_border_color: Color::TRANSPARENT,
        text_color: Some(text()),
        border_radius: None,
        padding_ratio: 0.16,
    }
}

/// A thin horizontal divider between flat list rows (e.g. log lines).
pub fn divider(_: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(Color { a: 0.14, ..outline() })),
        ..Default::default()
    }
}

/// The sliding pill behind the active navigation item.
pub fn pill(_: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(accent_container())),
        border: Border { radius: 32.0.into(), ..Default::default() },
        ..Default::default()
    }
}

/// Navigation rail item: transparent, with a faint state layer on hover.
pub fn nav_item(active: bool) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_, status| {
        // The active item sits on the pill, which already has its own color.
        let overlay = match (active, status) {
            (true, _) => Color::TRANSPARENT,
            (false, button::Status::Hovered) => Color { a: 0.08, ..on_accent_container() },
            (false, button::Status::Pressed) => Color { a: 0.14, ..on_accent_container() },
            _ => Color::TRANSPARENT,
        };
        button::Style {
            background: Some(Background::Color(overlay)),
            text_color: if active { on_accent_container() } else { muted() },
            border: Border { radius: 32.0.into(), ..Default::default() },
            ..Default::default()
        }
    }
}

/// Text input: a filled field with an outline that turns accent on focus.
pub fn field(_: &Theme, status: text_input::Status) -> text_input::Style {
    let focused = matches!(status, text_input::Status::Focused { .. });
    text_input::Style {
        background: Background::Color(surface_high()),
        border: Border {
            radius: 14.0.into(),
            width: if focused { 2.0 } else { 1.0 },
            color: if focused { accent() } else { outline() },
        },
        icon: muted(),
        placeholder: muted(),
        value: text(),
        selection: Color { a: 0.35, ..accent() },
    }
}

/// A rounded tonal card with a soft shadow, so it lifts off the background.
pub fn card(_: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(surface())),
        border: Border { radius: 28.0.into(), ..Default::default() },
        shadow: Shadow {
            color: Color { a: 0.32, ..Color::BLACK },
            offset: Vector { x: 0.0, y: 4.0 },
            blur_radius: 18.0,
        },
        ..Default::default()
    }
}

/// Linear progress: a tonal track with an accent fill.
pub fn progress(_: &Theme) -> progress_bar::Style {
    progress_bar::Style {
        background: Background::Color(surface_highest()),
        bar: Background::Color(accent()),
        border: Border { radius: 8.0.into(), ..Default::default() },
    }
}

/// The navigation rail on the left.
pub fn rail(_: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(rail_color())),
        ..Default::default()
    }
}

/// Small colored label, for log levels and status.
pub fn badge(color: Color) -> impl Fn(&Theme) -> container::Style {
    move |_| container::Style {
        background: Some(Background::Color(Color { a: 0.18, ..color })),
        border: Border { radius: 99.0.into(), ..Default::default() },
        text_color: Some(color),
        ..Default::default()
    }
}

/// A notice banner in the given color.
pub fn banner(color: Color) -> impl Fn(&Theme) -> container::Style {
    move |_| container::Style {
        background: Some(Background::Color(Color { a: 0.16, ..color })),
        border: Border { radius: 18.0.into(), color, width: 1.0 },
        ..Default::default()
    }
}

/// Checkbox: a rounded square that fills with the accent when checked.
pub fn check(_: &Theme, status: checkbox::Status) -> checkbox::Style {
    let (checked, hovered) = match status {
        checkbox::Status::Active { is_checked } => (is_checked, false),
        checkbox::Status::Hovered { is_checked } => (is_checked, true),
        checkbox::Status::Disabled { is_checked } => (is_checked, false),
    };
    checkbox::Style {
        background: Background::Color(if checked { accent() } else { Color::TRANSPARENT }),
        icon_color: on_accent(),
        border: Border {
            radius: 8.0.into(),
            width: 2.0,
            color: if checked {
                accent()
            } else if hovered {
                text()
            } else {
                muted()
            },
        },
        text_color: Some(text()),
    }
}
