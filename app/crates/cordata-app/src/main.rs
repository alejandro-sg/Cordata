//! Cordata.app — the native macOS workspace for the Cordata harness (docs/app.md).
//!
//!     cordata-app [--cwd DIR] [--conversation latest|new|HANDLE] [--transport hub|stdio] [--selftest]
//!
//! The window is a front door like the terminal: it speaks ACP to the daemon's hub (the default — a conversation
//! shared with the terminal, Discord and the menu bar), or to a private `acp_server.py` child when run from a
//! checkout with `--transport stdio`. The UI thread only draws; I/O runs on a tokio runtime (`session.rs`).

mod hud;
mod mission;
mod platform;
mod session;
mod theme;
mod transcript;
mod workspace;

use std::path::PathBuf;
use std::time::Duration;

use gpui::{App, AppContext as _, Bounds, SharedString, TitlebarOptions, WindowBackgroundAppearance, WindowBounds, WindowOptions, point, px, size};
use gpui_component::Root;

use crate::platform::Display;
use crate::session::{Connection, Prefer, Runtime};
use crate::workspace::Workspace;

struct Args {
    cwd: PathBuf,
    conversation: String,
    prefer: Prefer,
    /// `--prompt TEXT`: sent once the session connects (scripted runs, `cordata ask`).
    prompt: Option<String>,
    /// `--run GOAL`: start an orchestrator run once connected, and follow it in Mission Control.
    run: Option<String>,
    selftest: bool,
}

fn parse_args() -> Args {
    let mut args = Args { cwd: default_cwd(), conversation: "latest".into(), prefer: Prefer::Hub, prompt: None, run: None, selftest: false };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--cwd" => {
                if let Some(dir) = it.next() {
                    args.cwd = std::fs::canonicalize(&dir).unwrap_or_else(|_| PathBuf::from(dir));
                }
            }
            "--conversation" => args.conversation = it.next().unwrap_or_else(|| "latest".into()),
            "--transport" => args.prefer = if it.next().as_deref() == Some("stdio") { Prefer::Stdio } else { Prefer::Hub },
            "--prompt" => args.prompt = it.next(),
            "--run" => args.run = it.next(),
            "--selftest" => args.selftest = true,
            // A bundle opened by `open -a Cordata dir` (or `cordata .`) gets the folder as its one argument.
            other if !other.starts_with('-') => args.cwd = std::fs::canonicalize(other).unwrap_or_else(|_| PathBuf::from(other)),
            _ => {}
        }
    }
    args
}

/// Launched from Finder the working directory is `/`: the person's home is the honest default there.
fn default_cwd() -> PathBuf {
    match std::env::current_dir() {
        Ok(dir) if dir != std::path::Path::new("/") => dir,
        _ => std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/")),
    }
}

fn main() {
    let args = parse_args();
    let runtime = match Runtime::new() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("cordata: cannot start the I/O runtime: {error}");
            std::process::exit(1);
        }
    };
    let display = Display::read();
    let connection = Connection::discover(args.prefer, &args.conversation);

    gpui_platform::application().run(move |cx: &mut App| {
        cx.set_global(runtime);
        cx.set_global(display);
        gpui_component::init(cx);
        theme::install(display.increase_contrast, cx);
        workspace::bind_keys(cx);

        let glass_wanted = !display.reduce_transparency;
        let bounds = Bounds::centered(None, size(px(1440.), px(900.)), cx);
        let window = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                window_min_size: Some(size(px(980.), px(600.))),
                titlebar: Some(TitlebarOptions { title: Some(SharedString::from("Cordata")), appears_transparent: true, traffic_light_position: Some(point(px(16.), px(16.))) }),
                window_background: if glass_wanted { WindowBackgroundAppearance::Transparent } else { WindowBackgroundAppearance::Opaque },
                app_id: Some("dev.cordata.app".into()),
                ..Default::default()
            },
            |window, cx| {
                let glass = glass_wanted && platform::install_glass(window);
                let view = cx.new(|cx| Workspace::new(
                    connection.clone(),
                    args.cwd.clone(),
                    workspace::Launch { prompt: args.prompt.clone(), run: args.run.clone(), glass, selftest: args.selftest && (args.prompt.is_some() || args.run.is_some()) },
                    window,
                    cx,
                ));
                cx.new(|cx| Root::new(view, window, cx))
            },
        );
        let window = match window {
            Ok(window) => window,
            Err(error) => {
                eprintln!("cordata: the window could not be opened: {error}");
                cx.quit();
                return;
            }
        };
        cx.activate(true);

        if args.selftest {
            // `--selftest`: a packaged bundle proves it starts and draws, then quits. With `--prompt`, the workspace
            // also runs that turn to its end (approving whatever it asks) and prints what happened; this timer is the
            // watchdog for both. Used by `scripts/package-app.sh` and the CI bundle check.
            let started = std::time::Instant::now();
            let limit = if args.run.is_some() { 180 } else if args.prompt.is_some() { 60 } else { 3 };
            cx.spawn(async move |cx| {
                cx.background_executor().timer(Duration::from_secs(limit)).await;
                let _ = window.update(cx, |_, _, cx| {
                    println!("SELFTEST window=ok elapsed_ms={}{}", started.elapsed().as_millis(), if limit > 3 { " timeout=true" } else { "" });
                    cx.quit();
                });
            })
            .detach();
        }
    });
}
