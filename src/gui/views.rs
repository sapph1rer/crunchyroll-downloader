//! Screens: the password gate, the navigation rail with a sliding pill, and the
//! Download, Settings and Logs pages.

use crate::app::{
    page_scroll_id, password_input_id, tally, App, DropdownId, Poster, FfmpegDlEvent, FfmpegState, LangField, LoginPhase, Message,
    QueueItem, Screen, Tab, NAV_GAP, NAV_H,
};
use crate::dropdown::Dropdown;
use crate::engine;
use crate::{logs, theme};
use crunchyroll_downloader::api::SeasonEpisode;
use crunchyroll_downloader::log::Kind;
use crunchyroll_downloader::progress::{Bar, EpisodeState};
use crunchyroll_downloader::util::{parse_langs, track_title};
use crunchyroll_downloader::vault::{self, Settings};
use iced::font::Font;
use iced::widget::scrollable::AbsoluteOffset;
use iced::widget::{button, checkbox, column, container, progress_bar, row, scrollable, stack, text, text_input, toggler, Id, Space};
use iced::{Alignment, Color, Element, Length, Padding, Task};
use crate::update;
use theme::icon;

/// Locales offered in the language pickers, in display order.
const LANG_CODES: &[&str] = &[
    "ja-JP", "en-US", "en-IN", "id-ID", "ms-MY", "ca-ES", "de-DE", "es-419", "es-ES", "fr-FR", "it-IT", "pl-PL",
    "pt-BR", "pt-PT", "vi-VN", "tr-TR", "ru-RU", "ar-SA", "hi-IN", "ta-IN", "te-IN", "zh-CN", "zh-HK", "zh-TW",
    "ko-KR", "th-TH",
];

/// Log lines drawn at once. The export always holds every line.
const LOG_VIEW: usize = 300;
/// Distance between two rows of a dropdown list (row height plus spacing).
const ROW_STEP: f32 = 42.0;
/// The tallest a dropdown list gets before it scrolls.
const MENU_MAX_H: f32 = 276.0;
/// Empty space between a dropdown's field and its open list.
const LIST_GAP: f32 = 10.0;

/// One entry in a dropdown: the stored value and the text shown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Opt {
    pub value: String,
    pub label: String,
}

fn opt(value: &str, label: &str) -> Opt {
    Opt { value: value.into(), label: label.into() }
}

fn video_options() -> Vec<Opt> {
    vec![
        opt("best", "Best available"),
        opt("1080p", "1080p Full HD"),
        opt("720p", "720p HD"),
        opt("480p", "480p"),
        opt("360p", "360p"),
    ]
}

fn audio_options() -> Vec<Opt> {
    vec![
        opt("best", "Best available"),
        opt("192k", "192 kbps"),
        opt("128k", "128 kbps"),
        opt("96k", "96 kbps"),
    ]
}

fn lang_options() -> Vec<Opt> {
    let mut v = vec![opt("ALL", "ALL (every language)")];
    v.extend(LANG_CODES.iter().map(|c| opt(c, &format!("{}  ·  {c}", track_title(c)))));
    v
}

fn autolock_options() -> Vec<Opt> {
    vec![
        opt("5", "5 minutes"),
        opt("10", "10 minutes"),
        opt("20", "20 minutes"),
        opt("30", "30 minutes"),
        opt("0", "Never"),
    ]
}

fn settings_of(app: &App) -> Option<&Settings> {
    app.vault.as_ref().map(|v| &v.contents.settings)
}

fn lang_value(st: &Settings, field: LangField) -> &str {
    match field {
        LangField::Audio => &st.audio_langs,
        LangField::Subs => &st.subs_langs,
        LangField::Cc => &st.cc_langs,
    }
}

/// The options a dropdown offers right now. The list and the keyboard both use this, so
/// they always agree. A language already chosen is not offered again.
pub fn dropdown_options(app: &App, id: DropdownId) -> Vec<Opt> {
    match id {
        DropdownId::Video => video_options(),
        DropdownId::Audio => audio_options(),
        DropdownId::Lang(field) => {
            let chosen = settings_of(app).map(|st| parse_langs(lang_value(st, field))).unwrap_or_default();
            let query = app.dropdown_filter.trim().to_lowercase();
            lang_options()
                .into_iter()
                .filter(|o| !chosen.iter().any(|c| c == &o.value))
                .filter(|o| {
                    query.is_empty()
                        || o.label.to_lowercase().contains(&query)
                        || o.value.to_lowercase().contains(&query)
                })
                .collect()
        }
        DropdownId::AutoLock => autolock_options(),
        DropdownId::Genre => std::iter::once(opt("", "All genres"))
            .chain(app.browse.genres.iter().map(|g| Opt { value: g.id.clone(), label: g.title.clone() }))
            .collect(),
    }
}

/// The option the keyboard starts on when a list opens: the current value, or the first option.
pub fn highlight_for_open(app: &App, id: DropdownId) -> usize {
    if id == DropdownId::Genre {
        let current = app.browse.genre.as_deref().unwrap_or("");
        return dropdown_options(app, id).iter().position(|o| o.value == current).unwrap_or(0);
    }
    let current = match (id, settings_of(app)) {
        (DropdownId::Video, Some(st)) => st.video_quality.as_str(),
        (DropdownId::Audio, Some(st)) => st.audio_quality.as_str(),
        (DropdownId::AutoLock, Some(st)) => st.auto_lock_minutes.as_str(),
        _ => return 0,
    };
    dropdown_options(app, id)
        .iter()
        .position(|o| o.value.eq_ignore_ascii_case(current))
        .unwrap_or(0)
}

/// Id of the open list's scrollable, so the keyboard can scroll it.
fn dropdown_list_id() -> Id {
    Id::new("dropdown-list")
}

/// Scrolls the open list so the option at `index` sits in the middle of it.
pub fn scroll_to_highlight(index: usize) -> Task<Message> {
    let target = (index as f32 * ROW_STEP + ROW_STEP / 2.0 - MENU_MAX_H / 2.0).max(0.0);
    iced::widget::operation::scroll_to(dropdown_list_id(), AbsoluteOffset { x: None, y: Some(target) })
}

fn kind_color(kind: Kind) -> Color {
    match kind {
        Kind::Ok | Kind::Title => theme::success(),
        Kind::Warn | Kind::Hint => theme::warning(),
        Kind::Fail => theme::danger(),
        _ => theme::accent(),
    }
}

fn heading(title: &str, size: u32) -> Element<'static, Message> {
    text(title.to_string())
        .size(size)
        .font(theme::bold())
        .color(theme::text())
        .into()
}

/// A dropdown. The field shows the current value. The list is drawn as an overlay below
/// it, so opening it never moves the content under it. It fades and grows while it opens
/// and closes, and scrolls once it is long.
fn select<'a>(app: &App, id: DropdownId, shown: String, selected: &str, options: Vec<Opt>) -> Element<'a, Message> {
    let open = app.dropdown == Some(id);
    let t = app.dropdown_t();
    // The list is drawn while it opens and while it closes, and is gone once it has closed.
    let showing = app.dropdown_last == Some(id) && t > 0.01;

    let field = button(
        row![
            text(shown).size(16).color(theme::text()).width(Length::Fill),
            icon(if open { theme::EXPAND_LESS } else { theme::EXPAND_MORE }, 22.0).color(theme::muted()),
        ]
        .spacing(8)
        .align_y(Alignment::Center),
    )
    .on_press(Message::ToggleDropdown(id))
    .style(theme::select_field(open))
    .padding(Padding { top: 14.0, right: 16.0, bottom: 14.0, left: 18.0 })
    .width(Length::Fill);

    let panel: Element<'a, Message> = if showing {
        let natural = (options.len().max(1) as f32 * ROW_STEP + 12.0).min(MENU_MAX_H);
        let highlight = app.highlight;
        let items = column(options.into_iter().enumerate().map(|(index, o)| {
            let is_selected = o.value == selected;
            let is_highlighted = open && index == highlight;
            // A list that is closing ignores clicks, so it cannot change a setting.
            let press = open.then(|| Message::Choose(id, o.value.clone()));
            button(
                row![
                    text(o.label)
                        .size(15)
                        .font(if is_selected { theme::medium() } else { Font::DEFAULT })
                        .width(Length::Fill),
                    icon(if is_selected { theme::CHECK } else { ' ' }, 18.0).color(theme::on_accent_container()),
                ]
                .spacing(8)
                .align_y(Alignment::Center),
            )
            .on_press_maybe(press)
            .style(theme::option_row(is_selected, is_highlighted))
            .padding([10, 14])
            .width(Length::Fill)
            .into()
        }))
        .spacing(4)
        .padding(6);
        // The height follows the animation, so the list grows and shrinks smoothly.
        // The scrollbar appears only once the list is fully open, so it never shows as a stub.
        let bar = if t >= 0.999 { theme::scrollbar() } else { theme::scrollbar_hidden() };
        let list = scrollable(items)
            .id(dropdown_list_id())
            .direction(bar)
            .style(theme::scroll)
            .width(Length::Fill)
            .height(Length::Fixed(natural * t));
        // Language lists get a search box at the top — there are too many languages to scan.
        // It appears only once the list is fully open, so it doesn't flicker during the animation.
        let is_lang = matches!(id, DropdownId::Lang(_));
        if is_lang && t >= 0.999 {
            let search = text_input("Search language…", &app.dropdown_filter)
                .on_input(Message::DropdownFilter)
                .style(theme::field)
                .size(14)
                .padding(10);
            let content = column![search, list].spacing(6).padding(6);
            container(content).width(Length::Fill).style(theme::menu_surface(t)).into()
        } else {
            container(list).width(Length::Fill).style(theme::menu_surface(t)).into()
        }
    } else {
        Space::new().into()
    };

    Dropdown::new(field, panel, showing, LIST_GAP * t, Message::CloseDropdown).into()
}

pub fn root(app: &App) -> Element<'_, Message> {
    match app.screen {
        Screen::Ready => shell(app),
        _ => gate(app),
    }
}

/// Text input for a password or cookie, with a Show / Hide switch beside it. `id` lets the
/// app focus the field from code.
fn secret_field<'a>(
    placeholder: &'a str,
    value: &'a str,
    show: bool,
    on_input: fn(String) -> Message,
    on_submit: Message,
    id: Option<Id>,
) -> Element<'a, Message> {
    let mut input = text_input(placeholder, value)
        .secure(!show)
        .on_input(on_input)
        .on_submit(on_submit)
        .style(theme::field)
        .padding(14)
        .width(Length::Fill);
    if let Some(id) = id {
        input = input.id(id);
    }
    let glyph = if show { theme::VISIBILITY_OFF } else { theme::VISIBILITY };
    let label = if show { "Hide" } else { "Show" };
    row![
        input,
        button(
            row![icon(glyph, 18.0), text(label).size(14).font(theme::medium())]
                .spacing(6)
                .align_y(Alignment::Center),
        )
        .on_press(Message::ToggleSecret)
        .style(theme::tonal)
        .padding([12, 16]),
    ]
    .spacing(8)
    .align_y(Alignment::Center)
    .into()
}

/// Password screen for creating or unlocking the vault. The password field has focus when
/// the screen appears, and Enter sends the form.
fn gate(app: &App) -> Element<'_, Message> {
    let create = app.screen == Screen::Create;
    let (title, submit) = if create {
        ("Create your vault", Message::CreateVault)
    } else {
        ("Unlock Crunchyroll Downloader", Message::UnlockVault)
    };
    let badge = container(icon(theme::LOCK, 36.0).color(theme::on_accent_container()))
        .padding(22)
        .style(|_| container::Style {
            background: Some(iced::Background::Color(theme::accent_container())),
            border: iced::Border { radius: 40.0.into(), ..Default::default() },
            ..Default::default()
        });
    let mut col = column![
        badge,
        heading(title, 28),
        text(
            "Your settings and etp_rt cookie are kept in an encrypted file protected by this password. \
             Without the password the data cannot be opened, and there is no recovery."
        )
        .size(14)
        .color(theme::muted())
        .align_x(Alignment::Center),
        secret_field(
            "Password",
            &app.pw,
            app.show_secret,
            Message::PwInput,
            submit.clone(),
            Some(password_input_id()),
        ),
    ]
    .spacing(16)
    .max_width(480)
    .align_x(Alignment::Center);
    if create {
        col = col.push(secret_field(
            "Repeat the password",
            &app.pw2,
            app.show_secret,
            Message::Pw2Input,
            submit.clone(),
            None,
        ));
        col = col.push(text("At least 8 characters. Write it down somewhere safe.").size(13).color(theme::muted()));
    }
    col = col.push(
        button(text(if create { "Create vault" } else { "Unlock" }).size(16).font(theme::medium()))
            .on_press_maybe((!app.busy).then_some(submit))
            .style(theme::filled)
            .padding([14, 24])
            .width(Length::Fill),
    );
    if app.busy {
        col = col.push(text("Working. Key derivation takes a moment...").size(13).color(theme::muted()));
    }
    if let Some(n) = notice(app) {
        col = col.push(n);
    }
    container(col.padding(32)).center(Length::Fill).into()
}

/// Shows the message in a banner above the content, when there is one.
fn notice(app: &App) -> Option<Element<'_, Message>> {
    let (kind, msg) = app.notice.as_ref()?;
    let color = kind_color(*kind);
    Some(
        container(
            row![
                text(msg.as_str()).size(15).color(theme::text()).width(Length::Fill),
                button(theme::icon(theme::CLOSE, 18.0).color(color))
                    .on_press(Message::DismissNotice)
                    .style(theme::ghost)
                    .padding(6),
            ]
            .spacing(8)
            .align_y(Alignment::Center),
        )
        .padding([14, 18])
        .width(Length::Fill)
        .style(theme::banner(color))
        .into(),
    )
}

/// One item of the navigation rail: an icon over a label, sitting on the sliding pill.
fn nav<'a>(app: &App, tab: Tab, glyph: char, label: &'a str) -> Element<'a, Message> {
    let active = app.tab == tab;
    let color = if active { theme::on_accent_container() } else { theme::muted() };
    button(
        column![icon(glyph, 24.0).color(color), text(label).size(12).color(color).font(theme::medium())]
            .spacing(2)
            .width(Length::Fill)
            .align_x(Alignment::Center),
    )
    .on_press(Message::Tab(tab))
    .style(theme::nav_item(active))
    .padding([6, 4])
    .height(Length::Fixed(NAV_H))
    .width(Length::Fill)
    .into()
}

/// The four navigation items, with the pill drawn behind them at the animated position.
fn nav_rail(app: &App) -> Element<'_, Message> {
    let y = app.nav_y(app.now);
    // Three items plus the gaps between them. The stack needs an explicit height,
    // otherwise it is sized by its first layer and clips the other items.
    let total = 4.0 * NAV_H + 3.0 * NAV_GAP;
    let pill = column![
        Space::new().height(Length::Fixed(y)),
        container(Space::new())
            .width(Length::Fill)
            .height(Length::Fixed(NAV_H))
            .style(theme::pill),
    ]
    .width(Length::Fill)
    .height(Length::Fixed(total));
    let items = column![
        nav(app, Tab::Main, theme::HOME, "Download"),
        nav(app, Tab::Browse, theme::EXPLORE, "Browse"),
        nav(app, Tab::Settings, theme::SETTINGS, "Settings"),
        nav(app, Tab::Logs, theme::DESCRIPTION, "Logs"),
    ]
    .spacing(NAV_GAP)
    .width(Length::Fill)
    .height(Length::Fixed(total));
    stack![pill, items]
        .width(Length::Fill)
        .height(Length::Fixed(total))
        .into()
}

/// Navigation rail, page header and the active page.
fn shell(app: &App) -> Element<'_, Message> {
    let lock = button(
        column![icon(theme::LOCK_OPEN, 24.0).color(theme::muted()), text("Lock").size(12).color(theme::muted()).font(theme::medium())]
            .spacing(2)
            .width(Length::Fill)
            .align_x(Alignment::Center),
    )
    .on_press(Message::Lock)
    .style(theme::nav_item(false))
    .padding([6, 4])
    .height(Length::Fixed(NAV_H))
    .width(Length::Fill);

    let rail = column![nav_rail(app), Space::new().height(Length::Fill), lock]
        .width(108)
        .height(Length::Fill)
        .padding([16, 10]);

    let (title, subtitle) = match app.tab {
        Tab::Main => ("Download", "Fetch the episode list, pick what you want, then download"),
        Tab::Browse => ("Browse", "Find a series, open it, and download episodes, seasons or everything"),
        Tab::Settings => ("Settings", "Saved encrypted on this computer"),
        Tab::Logs => ("Logs", "Every step the downloader takes"),
    };
    let (status_text, status_color) = if app.running {
        ("Downloading", theme::warning())
    } else {
        ("Ready", theme::success())
    };
    let header = row![
        column![heading(title, 24), text(subtitle).size(13).color(theme::muted())]
            .spacing(2)
            .width(Length::Fill),
        container(text(status_text).size(13).color(status_color).font(theme::medium()))
            .padding([6, 14])
            .style(theme::badge(status_color)),
    ]
    .align_y(Alignment::Center);

    let body = match app.tab {
        Tab::Main => main_tab(app),
        Tab::Browse => browse_tab(app),
        Tab::Settings => settings_tab(app),
        Tab::Logs => logs_tab(app),
    };

    // The page rises a little after a tab change.
    let slide = app.tab_anim.interpolate_with(|v| 16.0 * (1.0 - v), app.now);
    let mut content = column![header]
        .spacing(20)
        .padding(Padding { top: 26.0 + slide, right: 32.0, bottom: 28.0, left: 32.0 })
        .width(Length::Fill);
    if let Some(n) = notice(app) {
        content = content.push(n);
    }
    content = content.push(body);

    row![
        container(rail).style(theme::rail).height(Length::Fill),
        scrollable(content)
            .id(page_scroll_id())
            .on_scroll(Message::PageScrolled)
            .direction(theme::scrollbar())
            .style(theme::scroll)
            .width(Length::Fill)
            .height(Length::Fill),
    ]
    .into()
}

fn card<'a>(content: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    container(content).padding(24).width(Length::Fill).style(theme::card).into()
}

/// A rounded tile holding an icon, at the head of a card.
fn icon_tile(glyph: char, size: f32) -> Element<'static, Message> {
    container(icon(glyph, size).color(theme::on_accent_container()))
        .padding(12)
        .style(|_| container::Style {
            background: Some(iced::Background::Color(theme::accent_container())),
            border: iced::Border { radius: 18.0.into(), ..Default::default() },
            ..Default::default()
        })
        .into()
}

/// Icon in a tonal tile, title and a one-line description. Heads every card.
fn section_head(glyph: char, title: impl Into<String>, subtitle: &'static str) -> Element<'static, Message> {
    row![
        icon_tile(glyph, 22.0),
        column![
            text(title.into()).size(18).font(theme::bold()).color(theme::text()),
            text(subtitle).size(13).color(theme::muted()),
        ]
        .spacing(2),
    ]
    .spacing(14)
    .align_y(Alignment::Center)
    .into()
}

/// A settings card: a header, then its controls.
fn settings_card<'a>(head: Element<'a, Message>, body: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    card(column![head, body.into()].spacing(20))
}

/// A label and description on the left, a toggle on the right.
fn toggle_row<'a>(label: &'a str, description: &'a str, value: bool, on: fn(bool) -> Message) -> Element<'a, Message> {
    row![
        column![
            text(label).size(15).color(theme::text()).font(theme::medium()),
            text(description).size(13).color(theme::muted()),
        ]
        .spacing(2)
        .width(Length::Fill),
        toggler(value).on_toggle(on).size(26.0).style(theme::toggle),
    ]
    .align_y(Alignment::Center)
    .into()
}

/// A labelled text field for a number or short value. The hint shows while it is empty.
fn number_field<'a>(title: &'a str, hint: &'a str, value: &'a str, on_input: fn(String) -> Message) -> Element<'a, Message> {
    column![
        text(title).size(14).color(theme::muted()).font(theme::medium()),
        text_input(hint, value).on_input(on_input).style(theme::field).padding(14),
    ]
    .spacing(8)
    .width(Length::Fill)
    .into()
}

/// A status chip: a label and a colored value.
fn status_pill(label: &str, value: &str, ok: bool) -> Element<'static, Message> {
    let color = if ok { theme::success() } else { theme::warning() };
    row![
        text(label.to_string()).size(14).color(theme::muted()),
        container(text(value.to_string()).size(13).color(color).font(theme::medium()))
            .padding([5, 12])
            .style(theme::badge(color)),
    ]
    .spacing(10)
    .align_y(Alignment::Center)
    .into()
}

/// What a setup row is doing right now: finished, waiting on something in progress, or
/// needing the user to act.
#[derive(Clone, Copy, PartialEq, Eq)]
enum RowState {
    Done,
    Busy,
    Todo,
}

/// One row of the setup check: a status mark, the item and what it needs, an optional
/// widget under the text (a progress bar), and up to two actions — the main one filled in
/// orange so the next step is always obvious, a secondary one as a quiet text button.
fn setup_row<'a>(
    state: RowState,
    title: &'static str,
    detail: String,
    below: Option<Element<'a, Message>>,
    secondary: Option<(&'static str, Message)>,
    primary: Option<(&'static str, char, Message)>,
) -> Element<'a, Message> {
    let (glyph, color) = match state {
        RowState::Done => (theme::CHECK, theme::success()),
        RowState::Busy => (theme::HOURGLASS, theme::accent()),
        RowState::Todo => (theme::INFO, theme::warning()),
    };
    let mut info = column![
        text(title).size(15).font(theme::medium()).color(theme::text()),
        text(detail).size(13).color(theme::muted()),
    ]
    .spacing(3)
    .width(Length::Fill);
    if let Some(extra) = below {
        info = info.push(container(extra).padding(Padding { top: 6.0, ..Padding::ZERO }));
    }
    let mut line = row![icon(glyph, 22.0).color(color), info].spacing(14).align_y(Alignment::Center);
    if let Some((label, message)) = secondary {
        line = line.push(
            button(text(label).size(14).font(theme::medium()))
                .on_press(message)
                .style(theme::ghost)
                .padding([10, 14]),
        );
    }
    if let Some((label, glyph, message)) = primary {
        line = line.push(
            button(row![icon(glyph, 18.0), text(label).size(14).font(theme::medium())].spacing(8).align_y(Alignment::Center))
                .on_press(message)
                .style(theme::filled)
                .padding([10, 18]),
        );
    }
    line.into()
}

/// A labelled progress bar for the ffmpeg download, used on both the Download page and in
/// Settings so the two always look the same.
fn ffmpeg_progress<'a>(event: &FfmpegDlEvent) -> Option<Element<'a, Message>> {
    match event {
        FfmpegDlEvent::Progress(p) => Some(progress_bar(0.0..=1.0, p.max(0.0)).girth(6).style(theme::progress).into()),
        FfmpegDlEvent::Extracting => Some(progress_bar(0.0..=1.0, 1.0).girth(6).style(theme::progress).into()),
        FfmpegDlEvent::Done(_) => None,
    }
}

/// Guidance for what a download still needs: a Crunchyroll login, the Widevine device and
/// ffmpeg. Each row offers the one action that fixes it. Shown until all three are ready.
fn setup_check(app: &App) -> Option<Element<'_, Message>> {
    let secrets = &app.vault.as_ref()?.contents.secrets;
    let signed_in = !secrets.etp_rt.is_empty() && !app.login_expired;
    let device = secrets.has_device();
    let ffmpeg_ok = matches!(app.ffmpeg, FfmpegState::Ready(_));
    if signed_in && device && ffmpeg_ok && app.login_busy.is_none() {
        return None;
    }
    let ready = [signed_in, device, ffmpeg_ok].iter().filter(|b| **b).count();

    // Crunchyroll account.
    let account = match app.login_busy {
        Some(LoginPhase::Waiting) => setup_row(
            RowState::Busy,
            "Crunchyroll account",
            "Sign in with your email and password in the login window. It closes by itself.".into(),
            None,
            None,
            None,
        ),
        Some(LoginPhase::Verifying) => setup_row(
            RowState::Busy,
            "Crunchyroll account",
            "Checking your login with Crunchyroll…".into(),
            None,
            None,
            None,
        ),
        None if signed_in => setup_row(RowState::Done, "Crunchyroll account", "Signed in".into(), None, None, None),
        None if app.login_expired => setup_row(
            RowState::Todo,
            "Crunchyroll account",
            "Your login expired. Sign in again to keep downloading.".into(),
            None,
            None,
            Some(("Log in", theme::LOGIN, Message::BrowserLogin)),
        ),
        None => setup_row(
            RowState::Todo,
            "Crunchyroll account",
            "Sign in once with your Crunchyroll email and password. The login is saved encrypted.".into(),
            None,
            Some(("Paste cookie", Message::GoSetup)),
            Some(("Log in", theme::LOGIN, Message::BrowserLogin)),
        ),
    };

    // Widevine device.
    let widevine = if device {
        setup_row(RowState::Done, "Widevine device", "Added".into(), None, None, None)
    } else {
        setup_row(
            RowState::Todo,
            "Widevine device",
            "Unlocks the video. Add client_id.bin + private_key.pem, or one .wvd file.".into(),
            None,
            None,
            Some(("Add device", theme::FOLDER, Message::GoSetup)),
        )
    };

    // ffmpeg: one click downloads and installs it, with live progress right here.
    let ffmpeg_row = match (&app.ffmpeg_dl, &app.ffmpeg) {
        (_, FfmpegState::Ready(path)) => {
            setup_row(RowState::Done, "ffmpeg", format!("Ready · {}", path.display()), None, None, None)
        }
        (Some(ev @ FfmpegDlEvent::Progress(p)), _) => setup_row(
            RowState::Busy,
            "ffmpeg",
            if *p >= 0.0 {
                format!("Downloading… {}%", (p * 100.0).round() as i32)
            } else {
                "Downloading…".to_string()
            },
            ffmpeg_progress(ev),
            None,
            None,
        ),
        (Some(ev @ FfmpegDlEvent::Extracting), _) => {
            setup_row(RowState::Busy, "ffmpeg", "Installing…".into(), ffmpeg_progress(ev), None, None)
        }
        (Some(FfmpegDlEvent::Done(Err(e))), _) => setup_row(
            RowState::Todo,
            "ffmpeg",
            format!("Download failed: {e}"),
            None,
            Some(("Choose file", Message::PickFfmpeg)),
            Some(("Try again", theme::DOWNLOAD, Message::DownloadFfmpeg)),
        ),
        (_, FfmpegState::Checking) => setup_row(RowState::Busy, "ffmpeg", "Checking…".into(), None, None, None),
        _ => setup_row(
            RowState::Todo,
            "ffmpeg",
            "Joins video and audio into the finished file. One click downloads and installs it.".into(),
            None,
            Some(("Choose file", Message::PickFfmpeg)),
            Some(("Download", theme::DOWNLOAD, Message::DownloadFfmpeg)),
        ),
    };

    let head = row![
        container(section_head(theme::INFO, "Before your first download", "Everything is stored encrypted on this PC"))
            .width(Length::Fill),
        container(text(format!("{ready} of 3 ready")).size(13).font(theme::medium()).color(theme::accent()))
            .padding([5, 12])
            .style(theme::badge(theme::accent())),
    ]
    .align_y(Alignment::Center);

    Some(card(column![head, column![account, widevine, ffmpeg_row].spacing(16)].spacing(20)))
}

/// One season: a checkbox that selects all of its episodes, the count picked, and an
/// expander that shows the episodes.
fn season_block<'a>(app: &'a App, g: &'a engine::SeasonGroup) -> Element<'a, Message> {
    let number = g.number;
    // A movie listing's films arrive as one group; label it as films, not "Season 0".
    let films = !g.episodes.is_empty() && g.episodes.iter().all(|e| e.is_movie);
    let total = g.episodes.len();
    let picked = g.episodes.iter().filter(|e| app.picked.contains(&e.id)).count();
    let all = total > 0 && picked == total;
    let open = app.expanded.contains(&number);
    let head = row![
        checkbox(all)
            .on_toggle(move |b| Message::ToggleSeason(number, b))
            .style(theme::check),
        button(
            column![
                text(if films { (if total == 1 { "Film" } else { "Films" }).to_string() } else { format!("Season {number}") })
                    .size(16)
                    .font(theme::medium())
                    .color(theme::text()),
                text(format!("{picked} of {total} {} selected", if films { "film(s)" } else { "episodes" }))
                    .size(13)
                    .color(theme::muted()),
            ]
            .spacing(2)
            .width(Length::Fill),
        )
        .on_press(Message::ToggleExpand(number))
        .style(theme::ghost)
        .padding([8, 10])
        .width(Length::Fill),
        button(
            row![icon(theme::DOWNLOAD, 16.0), text(if films { "All" } else { "Season" }).size(13).font(theme::medium())]
                .spacing(6)
                .align_y(Alignment::Center),
        )
        .on_press_maybe((!app.running && !app.fetching).then_some(Message::DownloadSeason(number)))
        .style(theme::tonal)
        .padding([8, 12]),
        button(icon(if open { theme::EXPAND_LESS } else { theme::EXPAND_MORE }, 22.0).color(theme::muted()))
            .on_press(Message::ToggleExpand(number))
            .style(theme::ghost)
            .padding(8),
    ]
    .spacing(8)
    .align_y(Alignment::Center);

    let mut block = column![head].spacing(4);
    if open {
        block = block.push(column(g.episodes.iter().map(|ep| episode_row(app, ep))).spacing(2).padding([0, 8]));
    }
    container(block)
        .padding(8)
        .width(Length::Fill)
        .style(|_| container::Style {
            background: Some(iced::Background::Color(theme::surface_high())),
            border: iced::Border { radius: 20.0.into(), ..Default::default() },
            ..Default::default()
        })
        .into()
}

/// One episode row with its checkbox.
fn episode_row<'a>(app: &'a App, ep: &'a SeasonEpisode) -> Element<'a, Message> {
    let on = app.picked.contains(&ep.id);
    let have = app.downloaded.contains(&ep.id);
    let id = ep.id.clone();
    let mut line = row![
        checkbox(on)
            .on_toggle(move |b| Message::ToggleEpisode(id.clone(), b))
            .style(theme::check),
        text(if ep.is_movie { "Film".to_string() } else { format!("E{:02}", ep.episode_number) })
            .size(14)
            .color(theme::muted())
            .width(48),
        text(ep.title.as_str())
            .size(15)
            .color(if have { theme::muted() } else { theme::text() })
            .width(Length::Fill),
    ]
    .spacing(12)
    .align_y(Alignment::Center)
    .padding([10, 12]);
    if have {
        // Already in the output folder at this quality; downloading again would be skipped.
        line = line.push(
            container(
                row![icon(theme::CHECK, 14.0).color(theme::success()), text("Downloaded").size(12).font(theme::medium())]
                    .spacing(4)
                    .align_y(Alignment::Center),
            )
            .padding([3, 10])
            .style(theme::badge(theme::success())),
        );
    }
    // One click downloads just this episode.
    line = line.push(
        button(icon(theme::DOWNLOAD, 18.0))
            .on_press_maybe((!app.running && !app.fetching).then_some(Message::DownloadOne(ep.id.clone())))
            .style(theme::ghost)
            .padding(6),
    );
    line.into()
}

/// One row of the queue: the episode, its state, and the action that state allows.
fn queue_row(item: &QueueItem, running: bool) -> Element<'_, Message> {
    let (label, color, detail) = match &item.state {
        EpisodeState::Waiting => ("Waiting", theme::muted(), String::new()),
        EpisodeState::Downloading => ("Downloading", theme::accent(), String::new()),
        EpisodeState::Done => ("Done", theme::success(), String::new()),
        EpisodeState::Skipped(why) => ("Skipped", theme::muted(), why.clone()),
        EpisodeState::Failed(why) => ("Failed", theme::danger(), why.clone()),
        EpisodeState::Cancelled => ("Cancelled", theme::muted(), String::new()),
    };
    let id = item.ep.id.clone();
    let action: Element<'_, Message> = match &item.state {
        EpisodeState::Waiting | EpisodeState::Downloading => button(text("Cancel").size(13).font(theme::medium()))
            .on_press(Message::CancelEpisode(id))
            .style(theme::ghost)
            .padding([8, 12])
            .into(),
        EpisodeState::Failed(_) | EpisodeState::Cancelled => button(text("Retry").size(13).font(theme::medium()))
            .on_press_maybe((!running).then_some(Message::RetryEpisode(id)))
            .style(theme::tonal)
            .padding([8, 12])
            .into(),
        _ => Space::new().into(),
    };
    let mut info = column![text(item.label.as_str()).size(15).color(theme::text())]
        .spacing(2)
        .width(Length::Fill);
    if !detail.is_empty() {
        info = info.push(text(detail).size(12).color(theme::muted()));
    }
    container(
        row![
            container(text(label).size(11).font(theme::bold()))
                .padding([4, 10])
                .style(theme::badge(color)),
            info,
            action,
        ]
        .spacing(12)
        .align_y(Alignment::Center),
    )
    .padding([10, 14])
    .width(Length::Fill)
    .style(|_| container::Style {
        background: Some(iced::Background::Color(theme::surface_high())),
        border: iced::Border { radius: 16.0.into(), ..Default::default() },
        ..Default::default()
    })
    .into()
}

/// The episodes of the current or last download, each with its state. Failed or cancelled
/// episodes can be retried, and the folder can be opened.
fn queue_card(app: &App) -> Element<'_, Message> {
    let running = app.running;
    let (done, failed, skipped) = tally(&app.queue);
    let retryable = app
        .queue
        .iter()
        .any(|q| matches!(q.state, EpisodeState::Failed(_) | EpisodeState::Cancelled));
    let summary = format!("{done} downloaded · {failed} failed · {skipped} skipped or cancelled");

    let retry = button(
        row![icon(theme::DOWNLOAD, 18.0), text("Retry failed").size(14).font(theme::medium())]
            .spacing(8)
            .align_y(Alignment::Center),
    )
    .on_press_maybe((!running && retryable).then_some(Message::RetryFailed))
    .style(theme::tonal)
    .padding([10, 16]);
    let open = button(
        row![icon(theme::FOLDER, 18.0), text("Open folder").size(14).font(theme::medium())]
            .spacing(8)
            .align_y(Alignment::Center),
    )
    .on_press(Message::OpenOutput)
    .style(theme::tonal)
    .padding([10, 16]);

    card(
        column![
            row![
                icon_tile(theme::DESCRIPTION, 22.0),
                column![
                    text("Queue").size(18).font(theme::bold()).color(theme::text()),
                    text(summary).size(13).color(theme::muted()),
                ]
                .spacing(2)
                .width(Length::Fill),
                open,
                retry,
            ]
            .spacing(10)
            .align_y(Alignment::Center),
            column(app.queue.iter().map(|item| queue_row(item, running))).spacing(8),
        ]
        .spacing(18),
    )
}

/// Capitalizes the first letter of a track name, for example "video" → "Video".
fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// One track of the running episode: its name, a thin bar, the detail, the speed and the percentage.
fn track_row(bar: &Bar) -> Element<'static, Message> {
    let ratio = if bar.total > 0 { (bar.current as f32 / bar.total as f32).clamp(0.0, 1.0) } else { 0.0 };
    let name = capitalize(bar.title.trim_start_matches("Downloading "));
    let detail = if bar.total > 0 && !bar.label.is_empty() {
        format!("{} of {} {}", bar.current, bar.total, bar.label)
    } else {
        String::new()
    };
    let rate = bar.rate_mbps();
    let speed = if rate > 0.0 { format!("{rate:.1} Mbps") } else { String::new() };
    column![
        row![
            text(name).size(14).font(theme::medium()).color(theme::text()).width(Length::Fill),
            text(detail).size(12).color(theme::muted()),
            text(speed).size(12).color(theme::muted()),
            text(format!("{:.0}%", ratio * 100.0))
                .size(14)
                .font(theme::medium())
                .color(theme::accent())
                .width(52)
                .align_x(Alignment::End),
        ]
        .spacing(12)
        .align_y(Alignment::Center),
        progress_bar(0.0..=1.0, ratio).girth(5).style(theme::progress),
    ]
    .spacing(7)
    .into()
}

/// The running download: overall progress, each track's progress, the current step and Stop.
fn progress_card(app: &App) -> Element<'_, Message> {
    let shown = app.progress_shown.clamp(0.0, 1.0);
    let percent = (shown * 100.0).round() as u32;
    let o = &app.overall;
    let caption = match o.total {
        0 => "Preparing".to_string(),
        _ if o.index >= o.total => "Finishing up".to_string(),
        _ => format!("Episode {} of {}  ·  {}", o.index + 1, o.total, o.label),
    };
    let tracks: Element<'_, Message> = if app.bars.is_empty() {
        text("Waiting for the next track...").size(13).color(theme::muted()).into()
    } else {
        column(app.bars.iter().map(track_row)).spacing(14).into()
    };
    let stop = button(row![icon(theme::STOP, 20.0), text("Stop").size(15).font(theme::medium())].spacing(10).align_y(Alignment::Center))
        .on_press(Message::Stop)
        .style(theme::danger_tonal)
        .padding([12, 22]);

    card(
        column![
            row![
                icon_tile(theme::DOWNLOAD, 22.0),
                column![
                    text("Downloading").size(18).font(theme::bold()).color(theme::text()),
                    text(caption).size(13).color(theme::muted()),
                ]
                .spacing(2)
                .width(Length::Fill),
                text(format!("{percent}%")).size(30).font(theme::bold()).color(theme::accent()),
            ]
            .spacing(14)
            .align_y(Alignment::Center),
            progress_bar(0.0..=1.0, shown).girth(8).style(theme::progress),
            tracks,
            text(app.current.as_str()).size(13).color(theme::muted()),
            row![Space::new().width(Length::Fill), stop].align_y(Alignment::Center),
        ]
        .spacing(18),
    )
}

fn main_tab(app: &App) -> Element<'_, Message> {
    let busy = app.running;
    let fetching = app.fetching;
    let mut page = column![].spacing(20);

    if let Some(setup) = setup_check(app) {
        page = page.push(setup);
    }

    // The live progress comes first, so it is the first thing seen while a download runs.
    if busy {
        page = page.push(progress_card(app));
    } else if fetching {
        page = page.push(card(
            row![
                icon_tile(theme::SEARCH, 22.0),
                column![
                    text("Fetching the episode list").size(18).font(theme::bold()).color(theme::text()),
                    text(app.current.as_str()).size(13).color(theme::muted()),
                ]
                .spacing(2),
            ]
            .spacing(14)
            .align_y(Alignment::Center),
        ));
    }
    if !app.queue.is_empty() {
        page = page.push(queue_card(app));
    }

    let url = text_input("Paste an episode, series or movie link", &app.url)
        .on_input(Message::UrlInput)
        .on_submit(Message::FetchPressed)
        .style(theme::field)
        .padding(16)
        .size(16);
    let fetch = button(
        row![icon(theme::SEARCH, 20.0), text(if fetching { "Fetching..." } else { "Fetch" }).size(16).font(theme::medium())]
            .spacing(10)
            .align_y(Alignment::Center),
    )
    .on_press_maybe((!busy && !fetching).then_some(Message::FetchPressed))
    .style(theme::filled)
    .padding([15, 26]);
    page = page.push(card(
        column![
            section_head(theme::SEARCH, "Find episodes", "Paste a link and press Fetch — or find titles in the Browse tab"),
            row![url, fetch].spacing(12).align_y(Alignment::Center),
        ]
        .spacing(20),
    ));

    if let Some(list) = catalog_card(app) {
        page = page.push(list);
    }
    page.into()
}

/// The loaded seasons and episodes with every way to download: one episode (its download
/// button), a whole season (the season's button), ticked episodes, or everything (Select
/// all). Used by the Download tab and the Browse detail page.
fn catalog_card(app: &App) -> Option<Element<'_, Message>> {
    let catalog = app.catalog.as_ref()?;
    let busy = app.running;
    let fetching = app.fetching;
    let total: usize = catalog.seasons.iter().map(|g| g.episodes.len()).sum();
    let picked = app.picked.len();
    // "All" means every episode not already on disk (select-all leaves those unticked).
    let wanted = total.saturating_sub(app.downloaded.len());
    let all_on = wanted > 0 && app.picked.iter().filter(|id| !app.downloaded.contains(*id)).count() == wanted;
    let toolbar = row![
        checkbox(all_on)
            .label("Select all")
            .on_toggle(Message::SelectAll)
            .style(theme::check)
            .text_size(15),
        Space::new().width(Length::Fill),
        text(format!("{picked} of {total} selected")).size(14).color(theme::muted()),
    ]
    .spacing(10)
    .align_y(Alignment::Center);

    let download = button(
        row![icon(theme::DOWNLOAD, 20.0), text(format!("Download {picked}")).size(16).font(theme::medium())]
            .spacing(10)
            .align_y(Alignment::Center),
    )
    .on_press_maybe((picked > 0 && !busy && !fetching).then_some(Message::DownloadSelected))
    .style(theme::filled)
    .padding([15, 28]);

    let list = column(catalog.seasons.iter().map(|g| season_block(app, g))).spacing(12);
    Some(card(
        column![
            section_head(
                theme::DESCRIPTION,
                &catalog.title,
                "Download one episode, a whole season, or tick several and press Download"
            ),
            toolbar,
            list,
            row![Space::new().width(Length::Fill), download].align_y(Alignment::Center),
        ]
        .spacing(18),
    ))
}

/// A poster image, or a placeholder box while it loads (or when there is none).
fn poster_view<'a>(app: &'a App, url: Option<&String>, width: f32, height: f32) -> Element<'a, Message> {
    let handle = url.and_then(|u| match app.browse.posters.get(u) {
        Some(Poster::Ready(h)) => Some(h.clone()),
        _ => None,
    });
    match handle {
        Some(h) => iced::widget::image(h)
            .width(Length::Fixed(width))
            .height(Length::Fixed(height))
            .content_fit(iced::ContentFit::Cover)
            .border_radius(10.0)
            .into(),
        None => container(icon(theme::MOVIE, 32.0).color(theme::outline()))
            .width(Length::Fixed(width))
            .height(Length::Fixed(height))
            .center_x(Length::Fixed(width))
            .center_y(Length::Fixed(height))
            .style(theme::poster_placeholder)
            .into(),
    }
}

/// Small facts under a title: seasons, episodes, year, sub/dub.
fn series_facts(item: &engine::CatalogItem) -> String {
    let mut parts = Vec::new();
    if item.kind == engine::Kind::Movie {
        parts.push("Movie".to_string());
    }
    if let Some(n) = item.season_count {
        parts.push(format!("{n} season{}", if n == 1 { "" } else { "s" }));
    }
    if let Some(n) = item.episode_count {
        parts.push(format!("{n} ep"));
    }
    if let Some(y) = item.year {
        parts.push(y.to_string());
    }
    match (item.subbed, item.dubbed) {
        (true, true) => parts.push("Sub | Dub".into()),
        (true, false) => parts.push("Sub".into()),
        (false, true) => parts.push("Dub".into()),
        _ => {}
    }
    parts.join(" · ")
}

const CARD_W: f32 = 168.0;
const CARD_H: f32 = 252.0;

/// One show in the Browse grid: poster, title, facts. Click opens its detail page.
fn series_card<'a>(app: &'a App, item: &'a engine::CatalogItem) -> Element<'a, Message> {
    button(
        column![
            poster_view(app, item.poster.as_ref(), CARD_W, CARD_H),
            // Two lines at most; a longer title is clipped instead of running into the facts.
            container(text(item.title.as_str()).size(14).font(theme::medium()).color(theme::text()))
                .width(Length::Fixed(CARD_W))
                .height(Length::Fixed(38.0))
                .clip(true),
            text(series_facts(item)).size(12).color(theme::muted()).width(Length::Fixed(CARD_W)),
        ]
        .spacing(6),
    )
    .on_press(Message::OpenSeries(item.clone()))
    .style(theme::poster_card)
    .padding(8)
    .into()
}

/// The Browse tab: search and sort on top, a poster grid below, or a series' detail page.
fn browse_tab(app: &App) -> Element<'_, Message> {
    let signed_in = app.vault.as_ref().is_some_and(|v| !v.contents.secrets.etp_rt.is_empty());
    if !signed_in || (app.login_expired && app.browse.items.is_empty()) {
        return card(
            column![
                section_head(theme::EXPLORE, "Browse Crunchyroll", "Sign in to see the catalog and search it"),
                button(row![icon(theme::LOGIN, 18.0), text("Log in").size(15).font(theme::medium())].spacing(8).align_y(Alignment::Center))
                    .on_press_maybe(app.login_busy.is_none().then_some(Message::BrowserLogin))
                    .style(theme::filled)
                    .padding([12, 22]),
            ]
            .spacing(18),
        );
    }
    if let Some(selected) = app.browse.selected.as_ref() {
        return series_detail(app, selected);
    }

    let b = &app.browse;
    let mut search = row![
        text_input("Search anime by title…", &b.query)
            .on_input(Message::BrowseQueryInput)
            .on_submit(Message::BrowseSubmit)
            .style(theme::field)
            .padding(14)
            .size(16)
            .width(Length::Fill),
    ]
    .spacing(8)
    .align_y(Alignment::Center);
    if !b.query.is_empty() {
        search = search.push(
            button(icon(theme::CLOSE, 20.0))
                .on_press(Message::BrowseClearSearch)
                .style(theme::ghost)
                .padding(12),
        );
    }

    // Kind, order and genre only apply to the catalog; a search covers series and movies.
    let searching = !b.query.trim().is_empty();
    let mut sorts = row![].spacing(8).align_y(Alignment::Center);
    if searching {
        sorts = sorts.push(text(format!("Results for \"{}\" — series and movies", b.query.trim())).size(14).color(theme::muted()));
    } else {
        for show in engine::Show::ALL {
            sorts = sorts.push(
                button(text(show.label()).size(14).font(theme::medium()))
                    .on_press(Message::BrowseShow(show))
                    .style(theme::choice(b.show == show))
                    .padding([8, 16]),
            );
        }
        // A 1px rule between the groups (padding on the styled box would paint a block).
        sorts = sorts.push(container(container(Space::new().width(1).height(22)).style(theme::divider)).padding([0, 6]));
        for sort in engine::Sort::ALL {
            sorts = sorts.push(
                button(text(sort.label()).size(14).font(theme::medium()))
                    .on_press(Message::BrowseSort(sort))
                    .style(theme::choice(b.sort == sort))
                    .padding([8, 16]),
            );
        }
    }
    sorts = sorts.push(Space::new().width(Length::Fill));
    if b.loading {
        sorts = sorts.push(text("Loading…").size(13).color(theme::muted()));
    }
    // The genre filter sits at the end of the same row once the catalog's genres are known.
    if !searching && !b.genres.is_empty() {
        let current = b.genre.clone().unwrap_or_default();
        let options = dropdown_options(app, DropdownId::Genre);
        let shown = options.iter().find(|o| o.value == current).map_or_else(|| "All genres".to_string(), |o| o.label.clone());
        sorts = sorts.push(container(select(app, DropdownId::Genre, shown, &current, options)).width(Length::Fixed(240.0)));
    }

    let mut page = column![search, sorts].spacing(14);

    if let Some(err) = &b.error {
        page = page.push(card(
            column![
                text("Couldn't load the catalog").size(16).font(theme::medium()).color(theme::text()),
                text(err.as_str()).size(13).color(theme::muted()),
                row![
                    button(text("Try again").size(14).font(theme::medium()))
                        .on_press(Message::BrowseRetry)
                        .style(theme::tonal)
                        .padding([10, 16]),
                ],
            ]
            .spacing(10),
        ));
    } else if b.items.is_empty() && !b.loading {
        let msg = if searching { "Nothing matches that search." } else { "Nothing to show here." };
        page = page.push(text(msg).size(14).color(theme::muted()));
    }

    // Cards flow into as many columns as the window fits.
    let grid = row(b.items.iter().map(|item| series_card(app, item))).spacing(8).wrap().vertical_spacing(12);
    page = page.push(grid);

    // More catalog pages load on their own near the bottom; the button is a fallback.
    if b.has_more() {
        page = page.push(
            container(
                button(text(if b.loading { "Loading…" } else { "Load more" }).size(15).font(theme::medium()))
                    .on_press_maybe((!b.loading).then_some(Message::BrowseMore))
                    .style(theme::tonal)
                    .padding([12, 28]),
            )
            .center_x(Length::Fill),
        );
    }
    page.into()
}

/// A series' page: poster, title, description, then its seasons and episodes with every
/// download option. Live progress shows here too, so the user never has to switch tabs.
fn series_detail<'a>(app: &'a App, item: &'a engine::CatalogItem) -> Element<'a, Message> {
    let back = button(
        row![icon(theme::ARROW_BACK, 18.0), text("Back to Browse").size(14).font(theme::medium())]
            .spacing(8)
            .align_y(Alignment::Center),
    )
    .on_press(Message::BrowseBack)
    .style(theme::ghost)
    .padding([8, 12]);

    let poster_url = item.poster_large.as_ref().or(item.poster.as_ref());
    let description = if item.description.is_empty() { "No description." } else { item.description.as_str() };
    let mut info = column![
        text(item.title.as_str()).size(26).font(theme::bold()).color(theme::text()),
        text(series_facts(item)).size(14).color(theme::accent()).font(theme::medium()),
        text(description).size(14).color(theme::muted()),
    ]
    .spacing(10)
    .width(Length::Fill);
    // Which dubs and subtitles exist, by name (e.g. "Japanese, English").
    for (label, locales) in [("Audio", &item.audio_locales), ("Subtitles", &item.subtitle_locales)] {
        if !locales.is_empty() {
            let names = locales.iter().map(|l| track_title(l)).collect::<Vec<_>>().join(", ");
            info = info.push(
                row![
                    text(label).size(14).font(theme::medium()).color(theme::text()).width(Length::Fixed(80.0)),
                    text(names).size(14).color(theme::muted()).width(Length::Fill),
                ]
                .spacing(8),
            );
        }
    }
    let header = card(row![poster_view(app, poster_url, 200.0, 300.0), info].spacing(24));

    let mut page = column![back, header].spacing(16);
    if app.running {
        page = page.push(progress_card(app));
    }
    if !app.queue.is_empty() {
        page = page.push(queue_card(app));
    }

    let movie = item.kind == engine::Kind::Movie;
    let loaded_here = app.catalog_series.as_deref() == Some(item.id.as_str());
    if app.fetching {
        page = page.push(card(
            row![
                icon_tile(theme::SEARCH, 22.0),
                text(if movie { "Loading the film…" } else { "Loading seasons and episodes…" })
                    .size(16)
                    .font(theme::medium())
                    .color(theme::text()),
            ]
            .spacing(14)
            .align_y(Alignment::Center),
        ));
    } else if loaded_here {
        if let Some(list) = catalog_card(app) {
            page = page.push(list);
        }
    } else if app.running {
        page = page.push(card(
            text("The list loads when the current download finishes (or press Stop).")
                .size(14)
                .color(theme::muted()),
        ));
    } else {
        page = page.push(card(
            button(text(if movie { "Load film" } else { "Load episodes" }).size(15).font(theme::medium()))
                .on_press(Message::OpenSeries(item.clone()))
                .style(theme::filled)
                .padding([12, 22]),
        ));
    }
    page.into()
}

/// Chips for the chosen locales, plus a dropdown to add more.
fn lang_picker<'a>(app: &App, title: &'a str, field: LangField, value: &str) -> Element<'a, Message> {
    let chosen = parse_langs(value);
    let chips: Element<'a, Message> = if chosen.is_empty() {
        text("None selected").size(14).color(theme::muted()).into()
    } else {
        row(chosen.iter().map(|code| {
            let label = if code.eq_ignore_ascii_case("all") {
                "ALL".to_string()
            } else {
                format!("{}  {code}", track_title(code))
            };
            button(
                row![text(label).size(14).font(theme::medium()), icon(theme::CLOSE, 16.0)]
                    .spacing(8)
                    .align_y(Alignment::Center),
            )
            .on_press(Message::RemoveLang(field, code.clone()))
            .style(theme::chip)
            .padding([8, 12])
            .into()
        }))
        .spacing(8)
        .into()
    };
    let id = DropdownId::Lang(field);
    column![
        text(title).size(14).color(theme::muted()).font(theme::medium()),
        chips,
        select(app, id, "Add a language".to_string(), "", dropdown_options(app, id)),
    ]
    .spacing(8)
    .width(Length::Fill)
    .into()
}

/// Dropdown for a quality setting. The field shows the option's label for the current value.
fn quality_pick<'a>(app: &App, id: DropdownId, title: &'a str, current: &str) -> Element<'a, Message> {
    let options = dropdown_options(app, id);
    let shown = options
        .iter()
        .find(|o| o.value.eq_ignore_ascii_case(current))
        .map_or_else(|| current.to_string(), |o| o.label.clone());
    column![
        text(title).size(14).color(theme::muted()).font(theme::medium()),
        select(app, id, shown, current, options),
    ]
    .spacing(8)
    .width(Length::Fill)
    .into()
}

fn settings_tab(app: &App) -> Element<'_, Message> {
    let Some(v) = app.vault.as_ref() else {
        return text("The vault is locked.").into();
    };
    let st = &v.contents.settings;
    let sec = &v.contents.secrets;

    let out_dir = crate::app::resolve_output(st);
    let default_dir = vault::default_output_dir();
    let downloads = settings_card(
        section_head(theme::FOLDER, "Downloads", "Where files go, and the pause between episodes"),
        column![
            row![
                text_input(&default_dir.display().to_string(), &st.output_dir)
                    .on_input(Message::SetOutputDir)
                    .style(theme::field)
                    .padding(14)
                    .width(Length::Fill),
                button(row![icon(theme::FOLDER, 18.0), text("Browse").size(14).font(theme::medium())].spacing(8).align_y(Alignment::Center))
                    .on_press(Message::BrowseOutput)
                    .style(theme::tonal)
                    .padding([13, 18]),
                button(text("Open").size(14).font(theme::medium()))
                    .on_press(Message::OpenOutput)
                    .style(theme::ghost)
                    .padding([13, 14]),
            ]
            .spacing(8)
            .align_y(Alignment::Center),
            text(format!(
                "Each series gets its own folder: {}\\<Series>\\<Series> S01E01 - <Title> [quality].mkv",
                out_dir.display()
            ))
            .size(12)
            .color(theme::muted()),
            text("Delay between episodes (for example 20s or 2m)").size(13).color(theme::muted()),
            text_input("0s", &st.download_delay)
                .on_input(Message::SetDelay)
                .style(theme::field)
                .padding(14),
            toggle_row(
                "Notify me when downloads finish",
                "A desktop notification when the whole queue is done",
                st.notify_done,
                Message::SetNotify,
            ),
        ]
        .spacing(10),
    );

    let quality = settings_card(
        section_head(theme::TUNE, "Quality", "Best picks the highest version each episode offers"),
        row![
            quality_pick(app, DropdownId::Video, "Video", &st.video_quality),
            quality_pick(app, DropdownId::Audio, "Audio", &st.audio_quality),
        ]
        .spacing(24),
    );

    let speed = settings_card(
        section_head(theme::DOWNLOAD, "Speed", "How many pieces download at the same time"),
        column![
            number_field(
                "Segments at once (per track)",
                "1 to 16, default 20",
                &st.segment_workers,
                Message::SetSegmentWorkers,
            ),
            number_field("Audio dubs at once", "1 to 4, default 2", &st.track_workers, Message::SetTrackWorkers),
            {
                let proxy_hint = if st.proxy_url.trim().is_empty() {
                    "Direct connection (no proxy)"
                } else {
                    "API calls routed through proxy · CDN downloads always go direct"
                };
                column![
                    number_field(
                        "Proxy URL (API only, leave blank to disable)",
                        "socks5://127.0.0.1:1080  or  http://user:pass@host:8080",
                        &st.proxy_url,
                        Message::SetProxyUrl,
                    ),
                    text(proxy_hint).size(12).color(theme::muted()),
                ]
                .spacing(4)
            },
        ]
        .spacing(18),
    );

    let (ff_value, ff_ok) = match &app.ffmpeg {
        FfmpegState::Ready(_) => ("ready", true),
        FfmpegState::Checking => ("checking", false),
        FfmpegState::Unknown => ("not checked", false),
        FfmpegState::Broken(_) => ("not working", false),
    };
    let ff_detail = match &app.ffmpeg {
        FfmpegState::Ready(path) => format!("Using {}", path.display()),
        FfmpegState::Broken(why) => why.clone(),
        _ => "ffmpeg is checked when the vault opens".to_string(),
    };
    let ff_source = if st.ffmpeg_path.trim().is_empty() {
        "Automatic search: next to this program, then PATH".to_string()
    } else {
        format!("Chosen by you: {}", st.ffmpeg_path)
    };
    // While a download runs (or just failed), show its status and a progress bar in place of
    // the download button.
    let downloading = matches!(app.ffmpeg_dl, Some(FfmpegDlEvent::Progress(_)) | Some(FfmpegDlEvent::Extracting));
    let dl_ui: Element<'_, Message> = match &app.ffmpeg_dl {
        Some(FfmpegDlEvent::Progress(p)) => {
            let known = *p >= 0.0;
            let label = if known {
                format!("Downloading ffmpeg… {}%", (p * 100.0).round() as i32)
            } else {
                "Downloading ffmpeg…".to_string()
            };
            column![
                text(label).size(13).color(theme::muted()),
                progress_bar(0.0..=1.0, if known { *p } else { 0.0 }).girth(8).style(theme::progress),
            ]
            .spacing(6)
            .into()
        }
        Some(FfmpegDlEvent::Extracting) => column![
            text("Unpacking ffmpeg…").size(13).color(theme::muted()),
            progress_bar(0.0..=1.0, 1.0).girth(8).style(theme::progress),
        ]
        .spacing(6)
        .into(),
        Some(FfmpegDlEvent::Done(Err(e))) => {
            text(format!("Download failed: {e}")).size(13).color(theme::danger()).into()
        }
        _ => Space::new().into(),
    };

    // Offer the automatic download whenever ffmpeg isn't ready and nothing is downloading.
    let download_btn: Element<'_, Message> = if !ff_ok && !downloading {
        button(
            row![icon(theme::DOWNLOAD, 18.0), text("Download ffmpeg").size(14).font(theme::medium())]
                .spacing(8)
                .align_y(Alignment::Center),
        )
        .on_press(Message::DownloadFfmpeg)
        .style(theme::filled)
        .padding([12, 18])
        .into()
    } else {
        Space::new().into()
    };

    let ffmpeg_card = settings_card(
        section_head(theme::SEARCH, "ffmpeg", "Merges video and audio. Download it automatically, or choose the file"),
        column![
            status_pill("Status", ff_value, ff_ok),
            text(ff_detail).size(13).color(theme::muted()),
            text(ff_source).size(13).color(theme::muted()),
            download_btn,
            dl_ui,
            row![
                button(row![icon(theme::FOLDER, 18.0), text("Choose ffmpeg").size(14).font(theme::medium())].spacing(8).align_y(Alignment::Center))
                    .on_press(Message::PickFfmpeg)
                    .style(theme::tonal)
                    .padding([12, 16]),
                button(text("Use automatic search").size(14).font(theme::medium()))
                    .on_press(Message::UseAutoFfmpeg)
                    .style(theme::ghost)
                    .padding([12, 16]),
            ]
            .spacing(8),
        ]
        .spacing(12),
    );

    let portable = vault::is_portable();
    let ffmpeg_place = match &app.ffmpeg {
        FfmpegState::Ready(p) => p.display().to_string(),
        _ => "not installed yet".to_string(),
    };
    let storage = column![
        text("Where things are stored").size(14).font(theme::medium()).color(theme::text()),
        text(format!("Vault (login, device, settings — encrypted): {}", app.vault_path.display()))
            .size(12)
            .color(theme::muted()),
        text(format!("Videos: {}", out_dir.display())).size(12).color(theme::muted()),
        text(format!("ffmpeg: {ffmpeg_place}")).size(12).color(theme::muted()),
        text("Temporary pieces go to your Windows temp folder and are removed when each episode finishes (leftovers are cleaned at startup).")
            .size(12)
            .color(theme::muted()),
    ]
    .spacing(4);
    let advanced = settings_card(
        section_head(theme::INFO, "Security and advanced", "Auto-lock, portable mode and troubleshooting"),
        column![
            quality_pick(app, DropdownId::AutoLock, "Sleep (lock) after inactivity", &st.auto_lock_minutes),
            toggle_row(
                "Portable mode",
                "Keep the vault (and a downloaded ffmpeg) next to the program, so the folder works from a USB drive",
                portable,
                Message::SetPortable,
            ),
            toggle_row(
                "Debug output",
                "Print raw playback JSON and manifests in the Logs tab",
                st.debug,
                Message::SetDebug,
            ),
            storage,
        ]
        .spacing(18),
    );

    let languages = settings_card(
        section_head(theme::LANGUAGE, "Languages", "Pick several. ALL downloads every language offered"),
        column![
            lang_picker(app, "Audio (dubs)", LangField::Audio, &st.audio_langs),
            lang_picker(app, "Subtitles", LangField::Subs, &st.subs_langs),
            lang_picker(app, "Closed captions", LangField::Cc, &st.cc_langs),
        ]
        .spacing(20),
    );

    let (etp_value, etp_ok) = if sec.etp_rt.is_empty() { ("not set", false) } else { ("saved", true) };
    let (dev_value, dev_ok) = if sec.has_device() { ("saved", true) } else { ("not set", false) };
    let account_body = column![
        row![
            status_pill("etp_rt cookie", etp_value, etp_ok),
            status_pill("Widevine device", dev_value, dev_ok),
        ]
        .spacing(20)
        .align_y(Alignment::Center),
        button(row![icon(theme::LOCK, 18.0), text("Set up").size(15).font(theme::medium())].spacing(8).align_y(Alignment::Center))
            .on_press(Message::OpenSetup)
            .style(theme::filled)
            .padding([12, 22]),
    ]
    .spacing(16);
    let mut account_items = column![account_body].spacing(16);
    if app.setup_open || app.setup_anim.is_animating(app.now) {
        account_items = account_items.push(setup_panel(app));
    }
    let account = settings_card(
        section_head(theme::LOCK, "Account and device", "Stored encrypted. Values are never shown again"),
        account_items,
    );

    let password = settings_card(
        section_head(theme::LOCK_OPEN, "Vault password", "Changing it re-encrypts everything"),
        column![
            secret_field("Current password", &app.old_pw, app.show_secret, Message::OldPwInput, Message::ChangePassword, None),
            secret_field(
                "New password (8 or more characters)",
                &app.new_pw,
                app.show_secret,
                Message::NewPwInput,
                Message::ChangePassword,
                None,
            ),
            secret_field(
                "Repeat the new password",
                &app.new_pw2,
                app.show_secret,
                Message::NewPw2Input,
                Message::ChangePassword,
                None,
            ),
            button(text("Change password").size(15).font(theme::medium()))
                .on_press(Message::ChangePassword)
                .style(theme::tonal)
                .padding([12, 20]),
        ]
        .spacing(12),
    );

    let left = column![languages, account, speed, updates_card(app)].spacing(20).width(Length::Fill);
    let right = column![downloads, quality, ffmpeg_card, advanced].spacing(20).width(Length::Fill);
    column![row![left, right].spacing(20), password].spacing(20).into()
}

/// Settings › Updates: this version, Check now, and installing a newer release.
fn updates_card(app: &App) -> Element<'_, Message> {
    let u = &app.update;
    let check = button(
        row![icon(theme::UPDATE, 18.0), text(if u.checking { "Checking…" } else { "Check now" }).size(14).font(theme::medium())]
            .spacing(8)
            .align_y(Alignment::Center),
    )
    .on_press_maybe((!u.checking && u.progress.is_none()).then_some(Message::CheckUpdate(true)))
    .style(theme::tonal)
    .padding([12, 16]);

    let mut body = column![row![
        text(format!("You have version {}", update::VERSION)).size(14).color(theme::text()).width(Length::Fill),
        check,
    ]
    .align_y(Alignment::Center)]
    .spacing(12);

    match &u.latest {
        Some(r) if r.is_newer() => {
            body = body.push(text(format!("Version {} is available", r.tag)).size(15).font(theme::medium()).color(theme::accent()));
            // What changed: the first lines of the release notes.
            let notes: Vec<&str> = r.notes.lines().map(str::trim).filter(|l| !l.is_empty()).take(6).collect();
            if !notes.is_empty() {
                body = body.push(text(notes.join("\n")).size(13).color(theme::muted()));
            }
            if let Some(p) = u.progress {
                body = body.push(text("Downloading the update…").size(13).color(theme::muted()));
                body = body.push(progress_bar(0.0..=1.0, p.max(0.0)).girth(8).style(theme::progress));
            } else if u.installed.is_some() {
                body = body.push(
                    row![
                        text("Installed — restart to use it.").size(14).color(theme::success()).width(Length::Fill),
                        button(text("Restart now").size(14).font(theme::medium()))
                            .on_press_maybe((!app.running).then_some(Message::RestartUpdate))
                            .style(theme::filled)
                            .padding([12, 18]),
                    ]
                    .align_y(Alignment::Center),
                );
            } else if update::dev_build() {
                body = body.push(
                    text(format!("This copy runs from cargo's target folder — update it with git, or download it from {}", r.page))
                        .size(13)
                        .color(theme::muted()),
                );
            } else {
                body = body.push(
                    button(
                        row![icon(theme::DOWNLOAD, 18.0), text(format!("Download and install {}", r.tag)).size(14).font(theme::medium())]
                            .spacing(8)
                            .align_y(Alignment::Center),
                    )
                    .on_press_maybe((!app.running).then_some(Message::InstallUpdate))
                    .style(theme::filled)
                    .padding([12, 18]),
                );
                if app.running {
                    body = body.push(text("Available when the current download finishes.").size(13).color(theme::muted()));
                }
            }
        }
        Some(r) => body = body.push(text(format!("Up to date (newest: {})", r.tag)).size(13).color(theme::success())),
        None => {}
    }
    if let Some(e) = &u.error {
        body = body.push(text(e.as_str()).size(13).color(theme::danger()));
    }
    settings_card(section_head(theme::UPDATE, "Updates", "New versions come from the project's GitHub releases"), body)
}

/// The setup panel. It fades in and slides up while it opens or closes. It says where each
/// value comes from, so a first-time user knows what to look for.
fn setup_panel(app: &App) -> Element<'_, Message> {
    let fade = app.setup_anim.interpolate_with(|v| v, app.now);
    let slide = app.setup_anim.interpolate_with(|v| 14.0 * (1.0 - v), app.now);
    let has_login = app.vault.as_ref().is_some_and(|v| !v.contents.secrets.etp_rt.is_empty());
    let busy = app.login_busy;
    let etp: Element<'_, Message> = if has_login && !app.login_expired && busy.is_none() {
        // Signed in: say so plainly, and offer the two things a user might want next.
        column![
            row![
                icon(theme::CHECK, 20.0).color(theme::success()),
                text("Signed in to Crunchyroll").size(15).font(theme::medium()).color(theme::text()),
            ]
            .spacing(10)
            .align_y(Alignment::Center),
            text("Your login is stored encrypted in this vault. Only the login cookie is kept — never your password.")
                .size(13)
                .color(theme::muted()),
            row![
                button(row![icon(theme::LOGIN, 18.0), text("Switch account").size(14).font(theme::medium())].spacing(8).align_y(Alignment::Center))
                    .on_press(Message::BrowserLogin)
                    .style(theme::tonal)
                    .padding([10, 16]),
                button(row![icon(theme::LOGOUT, 18.0), text("Sign out").size(14).font(theme::medium())].spacing(8).align_y(Alignment::Center))
                    .on_press(Message::SignOut)
                    .style(theme::danger_tonal)
                    .padding([10, 16]),
            ]
            .spacing(8),
        ]
        .spacing(10)
        .into()
    } else {
        let (label, press) = match busy {
            Some(LoginPhase::Waiting) => ("Waiting for you to sign in…", None),
            Some(LoginPhase::Verifying) => ("Checking your login…", None),
            None => ("Log in to Crunchyroll", Some(Message::BrowserLogin)),
        };
        let intro = if app.login_expired {
            "Your login expired. Sign in again with your Crunchyroll email and password."
        } else {
            "Sign in with your Crunchyroll email and password in a secure login window. \
             Only the login cookie is saved (encrypted) — never your password."
        };
        column![
            text(intro).size(13).color(theme::muted()),
            button(
                row![
                    icon(if busy.is_some() { theme::HOURGLASS } else { theme::LOGIN }, 18.0),
                    text(label).size(15).font(theme::medium())
                ]
                .spacing(8)
                .align_y(Alignment::Center)
            )
            .on_press_maybe(press)
            .style(theme::filled)
            .padding([12, 22]),
            text("Advanced: paste the etp_rt cookie yourself (F12 → Application → Cookies → crunchyroll.com). It is checked before it is saved.")
                .size(12)
                .color(theme::muted()),
            row![
                secret_field("etp_rt value", &app.etp_draft, app.show_secret, Message::EtpDraftInput, Message::SaveEtp, None),
                button(text("Check & save").size(14).font(theme::medium()))
                    .on_press_maybe(busy.is_none().then_some(Message::SaveEtp))
                    .style(theme::tonal)
                    .padding([12, 18]),
            ]
            .spacing(8)
            .align_y(Alignment::Center),
        ]
        .spacing(10)
        .into()
    };
    let device = column![
        text("Widevine device: either client_id.bin plus private_key.pem, or one .wvd file (a .wvd takes priority). \
              Both are stored encrypted.")
            .size(13)
            .color(theme::muted()),
        row![
            button(text("Choose client_id.bin").size(14)).on_press(Message::PickClientId).style(theme::tonal).padding([10, 14]),
            button(text("Choose private_key.pem").size(14)).on_press(Message::PickPem).style(theme::tonal).padding([10, 14]),
            button(text("Choose .wvd").size(14)).on_press(Message::PickWvd).style(theme::tonal).padding([10, 14]),
            button(row![icon(theme::DELETE, 16.0), text("Remove device").size(14)].spacing(8).align_y(Alignment::Center))
                .on_press(Message::ClearDevice)
                .style(theme::danger_tonal)
                .padding([10, 14]),
        ]
        .spacing(8),
    ]
    .spacing(10);
    container(
        column![
            text("etp_rt cookie").size(16).color(theme::text()).font(theme::medium()),
            etp,
            text("Widevine device").size(16).color(theme::text()).font(theme::medium()),
            device,
            row![
                Space::new().width(Length::Fill),
                button(text("Close").size(14)).on_press(Message::CloseSetup).style(theme::ghost).padding([10, 14]),
            ],
        ]
        .spacing(14),
    )
    .padding(Padding { top: 20.0 + slide, right: 20.0, bottom: 20.0, left: 20.0 })
    .width(Length::Fill)
    .style(move |_| container::Style {
        background: Some(iced::Background::Color(Color { a: fade, ..theme::surface_high() })),
        border: iced::Border { radius: 24.0.into(), ..Default::default() },
        ..Default::default()
    })
    .into()
}

/// One log line: a level pill, the time, then the message, on a soft rounded row.
fn log_line(e: &logs::Entry) -> Element<'_, Message> {
    let color = kind_color(e.kind);
    // Flat row: a small level tag, a timestamp, then the message. No box, no fill — rows are
    // separated by thin dividers instead, so the log reads as one open list.
    row![
        container(text(logs::label(e.kind)).size(10).font(theme::bold()))
            .padding([2, 0])
            .center_x(Length::Fixed(52.0))
            .style(theme::badge(color)),
        text(e.time.as_str()).size(12).color(theme::muted()).width(56),
        text(e.text.as_str()).size(13).color(theme::text()).width(Length::Fill),
    ]
    .spacing(12)
    .padding([7, 4])
    .align_y(Alignment::Center)
    .into()
}

/// A hairline divider between flat log rows.
fn log_divider<'a>() -> Element<'a, Message> {
    container(Space::new().width(Length::Fill).height(Length::Fixed(1.0)))
        .width(Length::Fill)
        .style(theme::divider)
        .into()
}

fn logs_tab(app: &App) -> Element<'_, Message> {
    let count = app.logs.len();
    let toolbar = row![
        text(format!("{count} lines")).size(13).color(theme::muted()).width(Length::Fill),
        button(row![icon(theme::DOWNLOAD, 16.0), text("Export").size(13).font(theme::medium())].spacing(6).align_y(Alignment::Center))
            .on_press(Message::ExportLogs)
            .style(theme::tonal)
            .padding([8, 14]),
        button(row![icon(theme::DELETE, 16.0), text("Clear").size(13).font(theme::medium())].spacing(6).align_y(Alignment::Center))
            .on_press(Message::ClearLogs)
            .style(theme::ghost)
            .padding([8, 14]),
    ]
    .spacing(8)
    .align_y(Alignment::Center);

    if app.logs.is_empty() {
        let empty = column![
            icon(theme::DESCRIPTION, 40.0).color(theme::muted()),
            text("No log yet").size(18).color(theme::text()).font(theme::medium()),
            text("Start a download on the Download page and every step will be listed here, with the reason for each decision and any fix to try.")
                .size(14)
                .color(theme::muted()),
        ]
        .spacing(10)
        .align_x(Alignment::Center)
        .width(Length::Fill);
        return column![toolbar, container(empty).padding([48, 12]).width(Length::Fill).center_x(Length::Fill)]
            .spacing(12)
            .into();
    }

    // Only the newest lines are drawn, which keeps redraws light. The export has them all.
    let hidden = count.saturating_sub(LOG_VIEW);
    let shown = count - hidden;
    let mut body = column![].spacing(0);
    if hidden > 0 {
        body = body.push(
            text(format!("Showing the last {LOG_VIEW} of {count} lines. Export saves all of them."))
                .size(12)
                .color(theme::muted()),
        );
    }
    let mut list = column![].spacing(0);
    for (i, e) in app.logs.iter().skip(hidden).enumerate() {
        list = list.push(log_line(e));
        if i + 1 < shown {
            list = list.push(log_divider());
        }
    }
    column![toolbar, body.push(list)].spacing(10).into()
}
