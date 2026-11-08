//! Messages for the user. Each helper sends one line of a given kind through
//! `log::emit`. `cli_sink` draws those kinds with colors for the terminal.

use crate::log::{self, Kind};
use anstyle::{AnsiColor, Color, Style};
use std::fmt::Display;

const CYAN: Style = Style::new().fg_color(Some(Color::Ansi(AnsiColor::Cyan)));
const GREEN: Style = Style::new().fg_color(Some(Color::Ansi(AnsiColor::Green)));
const YELLOW: Style = Style::new().fg_color(Some(Color::Ansi(AnsiColor::Yellow)));
const RED: Style = Style::new().bold().fg_color(Some(Color::Ansi(AnsiColor::Red)));
const TITLE: Style = Style::new().bold().fg_color(Some(Color::Ansi(AnsiColor::Cyan)));
const DIM: Style = Style::new().dimmed();

/// Installs the terminal printer. The CLI calls this once at startup.
pub fn install_cli_sink() {
    log::set_sink(Box::new(|kind, text| {
        for line in text.lines() {
            print_styled(kind, line);
        }
    }));
}

fn print_styled(kind: Kind, text: &str) {
    match kind {
        Kind::Title => anstream::println!("\n{TITLE}{text}{TITLE:#}"),
        Kind::Ok => anstream::println!("{GREEN}✔{GREEN:#} {text}"),
        Kind::Info => anstream::println!("{CYAN}›{CYAN:#} {text}"),
        Kind::Warn => anstream::println!("{YELLOW}⚠ {text}{YELLOW:#}"),
        Kind::Fail => anstream::println!("{RED}✖ {text}{RED:#}"),
        Kind::Detail => anstream::println!("  {DIM}{text}{DIM:#}"),
        Kind::Hint => anstream::println!("  {CYAN}hint:{CYAN:#} {text}"),
        Kind::Plain => anstream::println!("{text}"),
    }
}

/// A section heading with a blank line above it.
pub fn title(text: impl Display) {
    log::emit(Kind::Title, text.to_string());
}

/// A successful step.
pub fn ok(text: impl Display) {
    log::emit(Kind::Ok, text.to_string());
}

/// Neutral progress information.
pub fn info(text: impl Display) {
    log::emit(Kind::Info, text.to_string());
}

/// Something went wrong but the run continues.
pub fn warn(text: impl Display) {
    log::emit(Kind::Warn, text.to_string());
}

/// A failed step.
pub fn fail(text: impl Display) {
    log::emit(Kind::Fail, text.to_string());
}

/// Extra detail under a line, one entry per line of text.
pub fn detail(text: impl Display) {
    log::emit(Kind::Detail, text.to_string());
}

/// A suggested fix.
pub fn hint(text: impl Display) {
    log::emit(Kind::Hint, text.to_string());
}

/// A complete error: what failed, why, and what to do about it.
pub fn error_block(what: impl Display, why: impl Display, fix: Option<&str>) {
    fail(what);
    detail(why);
    if let Some(f) = fix {
        hint(f);
    }
}
