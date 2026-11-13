//! Cordata.app — the native macOS workspace for the Cordata harness (docs/app.md).
//!
//!     cordata-app [--cwd DIR] [--prompt TEXT] [--selftest]
//!     cordata-app status | ls | do <command> [arg]     (drive the open window: `control.rs`)
//!
//! The window runs the agent in this process (acp-core's native engine). The UI thread only draws; I/O runs on a
//! tokio runtime (`session.rs`).

mod control;
mod detect;
mod hud;
mod icons;
mod keychain;
pub mod mcp_catalog;
mod motion;
mod platform;
mod prefs;
mod session;
mod sound;
mod theme;
mod threads;
mod transcript;
mod tray;
mod ui;
mod vt;
mod workspace;

use std::path::PathBuf;
use std::time::Duration;

use gpui::{App, AppContext as _, Bounds, SharedString, TitlebarOptions, WindowBackgroundAppearance, WindowBounds, WindowOptions, point, px, size};
use gpui_component::Root;

use crate::platform::Display;
use crate::session::{Connection, Runtime};
use crate::workspace::Workspace;

struct Args {
    cwd: PathBuf,
    /// `--prompt TEXT`: sent once the session connects (scripted runs, `cordata ask`).
    prompt: Option<String>,
    selftest: bool,
    /// A folder was named (`--cwd`, a path argument, `open -a Cordata dir`).
    cwd_given: bool,
}

fn parse_args() -> Args {
    let mut args = Args { cwd: default_cwd(), prompt: None, selftest: false, cwd_given: false };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--cwd" => {
                if let Some(dir) = it.next() {
                    args.cwd = std::fs::canonicalize(&dir).unwrap_or_else(|_| PathBuf::from(dir));
                    args.cwd_given = true;
                }
            }
            "--prompt" => args.prompt = it.next(),
            "--selftest" => args.selftest = true,
            other if other.starts_with("cordata://") => parse_cordata_url(other, &mut args),
            // A bundle opened by `open -a Cordata dir` (or `cordata .`) gets the folder as its one argument.
            other if !other.starts_with('-') => {
                args.cwd = std::fs::canonicalize(other).unwrap_or_else(|_| PathBuf::from(other));
                args.cwd_given = true;
            }
            _ => {}
        }
    }
    // Started from a shell inside a folder (not Finder's `/`, not home): that folder is the project.
    if !args.cwd_given
        && let Ok(dir) = std::env::current_dir()
        && dir != std::path::Path::new("/")
        && std::env::var_os("HOME").is_none_or(|home| dir != std::path::Path::new(&home))
    {
        args.cwd_given = true;
    }
    args
}

fn parse_cordata_url(url: &str, args: &mut Args) {
    let stripped = url.trim_start_matches("cordata://");
    if let Some(query) = stripped.strip_prefix("ask?") {
        for param in query.split('&') {
            if let Some((k, v)) = param.split_once('=')
                && k == "prompt"
            {
                args.prompt = Some(v.replace("%20", " ").replace('+', " "));
            }
        }
    }
}

/// Launched from Finder the working directory is `/`: the person's home is the honest default there.
fn default_cwd() -> PathBuf {
    match std::env::current_dir() {
        Ok(dir) if dir != std::path::Path::new("/") => dir,
        _ => std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/")),
    }
}

fn bootstrap_path() {
    let home = std::env::var("HOME").unwrap_or_default();
    let common = [
        format!("{home}/.local/bin"),
        format!("{home}/.cargo/bin"),
        "/opt/homebrew/bin".to_string(),
        "/opt/homebrew/sbin".to_string(),
        "/usr/local/bin".to_string(),
    ];
    let current = std::env::var("PATH").unwrap_or_default();
    let mut parts: Vec<String> = common
        .into_iter()
        .filter(|p| !p.is_empty() && std::path::Path::new(p).exists())
        .collect();
    for existing in current.split(':') {
        if !existing.is_empty() && !parts.iter().any(|p| p == existing) {
            parts.push(existing.to_string());
        }
    }
    unsafe {
        std::env::set_var("PATH", parts.join(":"));
    }
}

fn main() {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    if matches!(raw.first().map(String::as_str), Some("status" | "ls" | "read" | "do")) {
        std::process::exit(control::client(&raw));
    }
    bootstrap_path();
    let mut args = parse_args();
    let runtime = match Runtime::new() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("cordata: cannot start the I/O runtime: {error}");
            std::process::exit(1);
        }
    };
    let display = Display::read();
    let prefs = prefs::Prefs::load();
    // No folder on the command line: the preference decides (the last project, home, or the picker over home).
    let mut open_picker = false;
    if !args.cwd_given {
        match prefs.open_on_launch.as_str() {
            "last" => {
                if let Some(last) = prefs.last_project().filter(|p| p.is_dir()) {
                    args.cwd = last.to_path_buf();
                }
            }
            "picker" => open_picker = true,
            _ => {}
        }
    }
    // Threads the last run left open come back (Settings → Reopen threads on launch), unless this launch was asked for
    // something else: a prompt, a self-test or the picker. A folder named on the command line is shown on top of them.
    let restore_wanted = prefs.restore_threads && args.prompt.is_none() && !args.selftest && !open_picker;
    let connection = Connection::discover();

    gpui_platform::application().with_assets(icons::Icons).run(move |cx: &mut App| {
        cx.set_global(runtime);
        cx.set_global(display);
        cx.set_global(prefs.clone());
        gpui_component::init(cx);
        theme::install(display.increase_contrast, &prefs.theme, cx);
        workspace::bind_keys(cx);
        workspace::install_menus(cx);

        // `CORDATA_APP_WINDOW=1280x800`: the size to open at (layout checks at fixed widths).
        let (w, h) = std::env::var("CORDATA_APP_WINDOW")
            .ok()
            .and_then(|v| v.split_once('x').and_then(|(w, h)| Some((w.trim().parse::<f32>().ok()?, h.trim().parse::<f32>().ok()?))))
            .unwrap_or((1440., 900.));
        let bounds = Bounds::centered(None, size(px(w), px(h)), cx);
        let window = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                window_min_size: Some(size(px(cordata_theme::tokens::MIN_WINDOW_W), px(cordata_theme::tokens::MIN_WINDOW_H))),
                titlebar: Some(TitlebarOptions { title: Some(SharedString::from("Cordata")), appears_transparent: true, traffic_light_position: Some(point(px(18.), px(19.))) }),
                // Flat, opaque grounds: no vibrancy behind the chrome.
                window_background: WindowBackgroundAppearance::Opaque,
                app_id: Some("dev.cordata.app".into()),
                ..Default::default()
            },
            |window, cx| {
                // `system` theme: follow macOS Light/Dark while the app runs (a no-op for any other choice).
                window
                    .observe_window_appearance(|window, cx| {
                        theme::appearance_changed(window.appearance(), cx);
                    })
                    .detach();
                let mut store = threads::Store::open(prefs::dir());
                let restore = if restore_wanted { store.load() } else { threads::Layout::default() };
                let view = cx.new(|cx| Workspace::new(
                    connection.clone(),
                    args.cwd.clone(),
                    workspace::Launch { prompt: args.prompt.clone(), selftest: args.selftest && args.prompt.is_some(), picker: open_picker, show: std::env::var("CORDATA_APP_SHOW").ok(), control: control::enabled(), shell: true, restore, store, open_folder: args.cwd_given },
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
            let limit = if args.prompt.is_some() { 60 } else { 3 };
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cordata_deep_links() {
        let mut args = Args {
            cwd: PathBuf::from("/"),
           
            prompt: None,
            selftest: false,
            cwd_given: false,
        };

        parse_cordata_url("cordata://ask?prompt=review%20this%20code", &mut args);
        assert_eq!(args.prompt.as_deref(), Some("review this code"));
    }
}

/// A folder for one test, under this test run's own `cordata-app-tests-<pid>` in the temp dir. Tests leave their
/// folders behind (some are shared across tests), so the first call also removes earlier runs' parents that have sat
/// untouched for an hour: leftovers stay bounded instead of piling up run after run.
#[cfg(test)]
pub(crate) fn test_dir(name: &str) -> std::path::PathBuf {
    static SWEPT: std::sync::Once = std::sync::Once::new();
    let tmp = std::env::temp_dir();
    SWEPT.call_once(|| {
        let mine = format!("cordata-app-tests-{}", std::process::id());
        for entry in std::fs::read_dir(&tmp).into_iter().flatten().flatten() {
            let file = entry.file_name().to_string_lossy().into_owned();
            let stale = entry.metadata().and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).is_some_and(|age| age.as_secs() > 3600);
            if file.starts_with("cordata-app-tests-") && file != mine && stale {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
    });
    let parent = tmp.join(format!("cordata-app-tests-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&parent);
    parent.join(name)
}
