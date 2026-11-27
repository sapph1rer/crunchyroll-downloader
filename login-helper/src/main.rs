// GUI subsystem on Windows: no console window flashes when the app launches this helper.
#![cfg_attr(windows, windows_subsystem = "windows")]

//! cr-login-helper — a standalone WebView2 login window for Crunchyroll.
//!
//! It opens the Crunchyroll login page, waits for the user to sign in, captures the
//! `etp_rt` cookie, prints it to stdout as a single line `ETP_RT=<value>`, then wipes its
//! ephemeral browser profile and exits.
//!
//! Why a separate process: embedding a webview inside the iced main window means two event
//! loops fighting in one process. A tiny helper keeps that isolated — and if the webview
//! crashes, the app survives.
//!
//! Why it can read etp_rt: `etp_rt` is an HttpOnly cookie, so page JavaScript can't see it.
//! The native WebView2 cookie store can, and wry exposes it via `cookies_for_url`, which
//! returns HttpOnly and secure cookies too.
//!
//! Security: the browser profile (the WebView2 user-data folder) holds the full login
//! session and is managed/encrypted by WebView2 itself, outside our vault. So we make it
//! EPHEMERAL — a fresh temp dir that is deleted on exit. Only the extracted etp_rt (the
//! long-lived refresh token, which is all the downloader needs) leaves this process, and
//! it is handed back over stdout, never written to disk here and never logged.
//!
//! Exit codes: 0 captured · 2 window closed before login · 3 timed out · 1 internal error.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use tao::event::{Event, StartCause, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoop};
use tao::platform::run_return::EventLoopExtRunReturn;
use tao::window::WindowBuilder;
use wry::{WebContext, WebViewBuilder};
use zeroize::Zeroizing;

const LOGIN_URL: &str = "https://www.crunchyroll.com/login";
const COOKIE_URL: &str = "https://www.crunchyroll.com";
const COOKIE_NAME: &str = "etp_rt";
const POLL: Duration = Duration::from_millis(1000);

fn arg_value(flag: &str) -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    args.iter().position(|a| a.as_str() == flag).and_then(|i| args.get(i + 1).cloned())
}

fn main() {
    let timeout = Duration::from_secs(
        arg_value("--timeout-secs").and_then(|s| s.parse().ok()).unwrap_or(300),
    );
    let url = arg_value("--url").unwrap_or_else(|| LOGIN_URL.to_string());

    // A fresh, private profile we own and will delete. Keyed by PID so parallel runs
    // never collide.
    let udf = std::env::temp_dir().join(format!("cr-login-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&udf);

    let mut event_loop = EventLoop::new();
    let window = match WindowBuilder::new()
        .with_title("Log in to Crunchyroll")
        .with_inner_size(tao::dpi::LogicalSize::new(520.0, 760.0))
        .build(&event_loop)
    {
        Ok(w) => w,
        Err(e) => {
            eprintln!("could not create the window: {e}");
            cleanup(&udf);
            std::process::exit(1);
        }
    };

    // WebView2 already presents itself as desktop Edge, so Crunchyroll's email/password
    // login page works as-is; no user-agent override or popup handling is needed.
    let mut web_context = WebContext::new(Some(udf.clone()));
    let webview = match WebViewBuilder::new_with_web_context(&mut web_context)
        .with_url(url.as_str())
        .build(&window)
    {
        Ok(w) => w,
        Err(e) => {
            eprintln!("could not create the webview (is the WebView2 Runtime installed?): {e}");
            cleanup(&udf);
            std::process::exit(1);
        }
    };

    let start = Instant::now();
    let mut captured: Option<Zeroizing<String>> = None;

    let code = event_loop.run_return(|event, _, control_flow| {
        *control_flow = ControlFlow::WaitUntil(Instant::now() + POLL);
        match event {
            Event::NewEvents(StartCause::Init) => {
                *control_flow = ControlFlow::WaitUntil(Instant::now() + POLL);
            }
            Event::NewEvents(StartCause::ResumeTimeReached { .. }) => {
                // Poll the native cookie store for a non-empty etp_rt.
                if let Ok(cookies) = webview.cookies_for_url(COOKIE_URL) {
                    if let Some(c) = cookies.iter().find(|c| c.name() == COOKIE_NAME) {
                        let value = c.value().to_string();
                        if !value.is_empty() {
                            captured = Some(Zeroizing::new(value));
                            *control_flow = ControlFlow::Exit;
                            return;
                        }
                    }
                }
                if start.elapsed() >= timeout {
                    *control_flow = ControlFlow::ExitWithCode(3);
                    return;
                }
                *control_flow = ControlFlow::WaitUntil(Instant::now() + POLL);
            }
            Event::WindowEvent { event: WindowEvent::CloseRequested, .. } => {
                *control_flow = ControlFlow::ExitWithCode(2);
            }
            _ => {}
        }
    });

    // Release the webview (and its file locks on the profile) before wiping it.
    drop(webview);
    drop(web_context);

    let exit = if let Some(value) = captured {
        use std::io::Write;
        let mut stdout = std::io::stdout();
        let _ = writeln!(stdout, "ETP_RT={}", &*value);
        let _ = stdout.flush();
        // `value` is zeroized on drop here.
        0
    } else {
        code
    };

    cleanup(&udf);
    std::process::exit(exit);
}

/// Best-effort wipe of the ephemeral profile. WebView2 can keep file handles open for a
/// brief moment after the window closes, so retry a few times.
fn cleanup(udf: &PathBuf) {
    for _ in 0..10 {
        if std::fs::remove_dir_all(udf).is_ok() || !udf.exists() {
            return;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}
