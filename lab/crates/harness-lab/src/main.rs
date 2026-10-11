//! The Experimental Lab, started from a terminal.
//!
//! The window itself lives in the `harness-lab` library (see [`harness_lab::app`]); this binary
//! only decides where it goes and what workspace it watches. Starting with no daemon is a supported
//! state, not a crash: the window opens, says what is missing, keeps looking — the daemon's address
//! and token are re-read on every attempt — and starts working the moment a daemon answers,
//! including one started *after* the window.
//!
//! Usage: `harness-lab [--watch <dir>]`, where `--watch` is the workspace the proactive observer
//! watches (default: `SIERRALAB_LAB_WATCH`, else the current directory).

use gpui::{App, AppContext as _, Application, Bounds, SharedString, TitlebarOptions, WindowBounds, WindowOptions, point, px, size};

use harness_lab::app::LabApp;
use harness_lab::workspace_root;

fn main() {
    let root = workspace_root(std::env::args().skip(1));

    Application::new().run(move |cx: &mut App| {
        let _window = cx
            .open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds::new(point(px(140.), px(90.)), size(px(1440.), px(920.))))),
                    window_min_size: Some(size(px(980.), px(640.))),
                    // A transparent titlebar: the header row is the titlebar, and leaves room for the traffic lights.
                    titlebar: Some(TitlebarOptions {
                        title: Some(SharedString::from("sierralab · Experimental Lab")),
                        appears_transparent: true,
                        traffic_light_position: Some(point(px(16.), px(16.))),
                    }),
                    ..Default::default()
                },
                |_window, cx| {
                    let root = root.clone();
                    cx.new(|cx| LabApp::discovering(root, cx))
                },
            )
            .expect("the Lab window could not be opened");
        cx.activate(true);
    });
}

