//! Output from the download engine goes through this module, so the CLI can print it
//! with colors and the GUI can keep it in a log tab. Without a sink set, lines are
//! printed as plain text.

use std::sync::RwLock;

/// What a line is for. Sinks decide how to show each kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A section heading.
    Title,
    /// A step that worked.
    Ok,
    /// Neutral progress information.
    Info,
    /// Something went wrong but the run continues.
    Warn,
    /// A failure.
    Fail,
    /// Extra detail under the line before it.
    Detail,
    /// A suggested fix.
    Hint,
    /// Ordinary engine output.
    Plain,
}

/// Receives every line. Text may contain several lines.
pub type Sink = Box<dyn Fn(Kind, &str) + Send + Sync>;

static SINK: RwLock<Option<Sink>> = RwLock::new(None);

/// Replaces the current sink. The CLI and the GUI each install one at startup.
pub fn set_sink(sink: Sink) {
    let mut guard = SINK.write().unwrap_or_else(|e| e.into_inner());
    *guard = Some(sink);
}

/// Sends one message to the current sink, or prints it when none is set.
pub fn emit(kind: Kind, text: impl AsRef<str>) {
    let text = text.as_ref();
    let guard = SINK.read().unwrap_or_else(|e| e.into_inner());
    match guard.as_ref() {
        Some(sink) => sink(kind, text),
        None => println!("{text}"),
    }
}

/// `say!("...", args)`: an ordinary line from the engine.
#[macro_export]
macro_rules! say {
    ($($arg:tt)*) => {
        $crate::log::emit($crate::log::Kind::Plain, format!($($arg)*))
    };
}
