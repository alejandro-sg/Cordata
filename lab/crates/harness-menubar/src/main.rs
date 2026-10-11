//! The menu-bar door into the harness.
//!
//! The Lab is a window you go to; this is a status item that is always there. It owns the same
//! client and the same model, but at **app scope**, so it keeps its connection with no window open
//! at all — the Lab's model dies with its window, and a menu bar that went dark when a window
//! closed would be worse than no menu bar.
//!
//! What it does not do: decide anything. The daemon decides routing, gating and memory exactly as it
//! does for the Lab; this process reads the OS context the *person* allowed it to read, sends it
//! with a turn, and draws what came back.
//!
//! Usage: `harness-menubar [--tray-selftest | --deck-selftest]`. `--tray-selftest` cycles the three
//! tray states every two seconds with no daemon at all — a ten-second eyeball check that the icons
//! read right. `--deck-selftest` opens the deck itself, prints where it landed, closes it, and exits:
//! a popover window with a transparent background is the one thing here that no headless test can
//! create, so it is checked against the real window server or not at all.

use std::time::Duration;

use gpui::{App, AppContext as _, Application, Entity};
use harness_capture::CaptureSource as _;
use harness_client::deck::TrayState;
use harness_menubar::app::{self, MenubarApp};
use harness_menubar::deck::DeckView;
use harness_menubar::deck_window::{self as window, DeckSession, Frame};
use harness_menubar::hotkey::{Hotkey, Registrar};
use harness_menubar::tray::{Tray, TrayHandle};
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let tray_selftest = args.iter().any(|arg| arg == "--tray-selftest");
    let deck_selftest = args.iter().any(|arg| arg == "--deck-selftest");

    Application::new().run(move |cx: &mut App| {
        accessory(cx);

        if tray_selftest {
            run_selftest(cx);
            return;
        }

        if deck_selftest {
            run_deck_selftest(cx);
            return;
        }

        let app = app::install(cx);
        install_hotkey(cx, &app);
        cx.set_global(DeckSession::default());
        install_tray(cx, app);
    });
}

/// Take the app out of the Dock. gpui sets `Regular` before it invokes this closure — the policy a
/// normal windowed app wants — and a menu-bar app is exactly the case where that is wrong: it has
/// no Dock icon and no window to show. `cargo run` has no bundle, so this call, not `Info.plist`, is
/// what removes the Dock icon.
fn accessory(_cx: &mut App) {
    let Some(mtm) = objc2::MainThreadMarker::new() else {
        eprintln!("harness-menubar must start on the main thread");
        return;
    };
    let ns_app = NSApplication::sharedApplication(mtm);
    if !ns_app.setActivationPolicy(NSApplicationActivationPolicy::Accessory) {
        eprintln!("harness-menubar could not become an accessory app: the Dock icon stays");
    }
}

/// Claim the summon hotkey and keep it in a `Global`, so any window (or no window) can ask whether
/// it was pressed.
///
/// A hotkey that cannot be claimed is said out loud and then forgotten about: the tray works, and a
/// person pressing a dead keystroke with no explanation would conclude the app is broken.
fn install_hotkey(cx: &mut App, app: &Entity<MenubarApp>) {
    let hotkey = match harness_menubar::hotkey::CarbonRegistrar::new() {
        Ok(registrar) => Hotkey::install(&registrar as &dyn Registrar),
        Err(error) => Hotkey::unavailable(error),
    };
    if let Some(error) = &hotkey.error {
        // Said once, on stderr, at the moment it is discovered — and again in the deck, where a
        // person who never reads a terminal will see it.
        eprintln!("harness-menubar: {error}");
    }
    app.update(cx, |app, cx| {
        app.hotkey_binding = hotkey.binding.clone();
        app.hotkey_error = hotkey.error.clone();
        // No "press … to summon" notice: the only place a person would read it is the deck they have
        // already summoned. The deck's own hint says it when there is nothing else to say.
        cx.notify();
    });
    cx.set_global(HotkeyHandle(hotkey));
}

/// The summon hotkey, kept for the life of the app. It has to be registered on the main thread and
/// held: dropping it unregisters the keystroke.
struct HotkeyHandle(pub Hotkey);

impl gpui::Global for HotkeyHandle {}

/// Create the status item and keep it up to date from the app-scoped model.
///
/// The tray lives in a `Global` rather than inside this task, for two reasons: the task would
/// otherwise have to move it, and a global is what the deck's own window will reach for later. A
/// `Global` is the one place GPUI keeps something no window owns.
fn install_tray(cx: &mut App, app: gpui::Entity<MenubarApp>) {
    let tray = match Tray::install() {
        Ok(tray) => tray,
        Err(error) => {
            // A tray that cannot appear is not a crash: the app stays alive so the reason can be
            // read, rather than vanishing with no explanation.
            eprintln!("harness-menubar: {error}");
            return;
        }
    };
    cx.set_global(TrayHandle(tray));

    cx.spawn(async move |cx| {
        let mut last_waiting = 0usize;
        loop {
            // Fast enough that the hotkey feels instant (it is polled here, not callbacked), and the
            // work per wake is a drain and a comparison — the icon is only rebuilt on a change.
            cx.background_executor().timer(Duration::from_millis(50)).await;
            // One hop to the foreground: drain the model, then act on what it says. The model is
            // still the only thing that changed; the tray, the hotkey and the deck only ever read
            // it.
            let Ok(tick) = cx.update(|cx| {
                let (state, waiting, auto_open, open) = app.update(cx, |app, cx| {
                    app.drain(cx);
                    // Tiering: only a request that *interrupts* pulls the deck open by itself, and
                    // only while it is not already up. The rule lives in `app::open_id`, so it holds
                    // with no window at all.
                    let auto_open = !app.deck_open && app.auto_open_id().is_some();
                    (app.tray_state(), app.waiting(), auto_open, app.deck_open)
                });
                let tray = cx.global_mut::<TrayHandle>();
                // Redrawn only when the state or the count changes: a menu-bar icon rebuilt five
                // times a second is a flicker, not a status.
                let redrawn = Some(state) != tray.0.shown_state() || waiting != last_waiting;
                if redrawn && let Err(error) = tray.0.show(state, waiting) {
                    eprintln!("harness-menubar: {error}");
                }
                // Clicks are drained every tick, whether or not the icon changed, so none is lost.
                let clicks = tray.0.clicks();
                // The click's own rect is the anchor; before any click, the status item's own rect
                // answers for the hotkey.
                let icon = clicks.last().map(|click| click.rect).or_else(|| tray.0.icon_rect());
                let hotkey = cx.try_global::<HotkeyHandle>().map(|handle| handle.0.pressed()).unwrap_or(false);
                Tick {
                    waiting,
                    redrawn,
                    toggles: clicks.len() + usize::from(hotkey),
                    auto_open,
                    open,
                    icon: icon.map(|rect| Frame { x: rect.position.x as f32, y: rect.position.y as f32, width: rect.size.width as f32, height: rect.size.height as f32 }),
                }
            }) else { return };
            if tick.redrawn {
                last_waiting = tick.waiting;
            }
            // A second hop for the parts that need `&mut App` — opening a window is an app-level
            // act, and there is no reason to invent a second path for it.
            let acted = cx.update(|cx| {
                // The deck closed itself (Esc removes its own window): forget the window, or the next
                // summon would find a deck "already open" and do nothing — and hand the keyboard back,
                // because Esc is the person's own act.
                if !tick.open && cx.try_global::<DeckSession>().is_some_and(|session| session.is_open()) {
                    cx.global_mut::<DeckSession>().forget();
                    cx.hide();
                }
                if tick.toggles > 0 {
                    // One toggle for any number of clicks in this tick: a double click must not open
                    // and immediately close.
                    set_deck(cx, &app, !tick.open, tick.icon, true);
                } else if tick.auto_open {
                    // Opened *for* the person, not *by* them: it appears, but does not take the
                    // keyboard out of whatever they are typing into.
                    set_deck(cx, &app, true, tick.icon, false);
                } else if tick.open {
                    // It had the keyboard and something else took it: the outside click. Esc and the
                    // hotkey are the other two ways out.
                    let lost = cx.has_global::<DeckSession>() && cx.global_mut::<DeckSession>().lost_key();
                    if lost {
                        set_deck(cx, &app, false, None, false);
                    }
                }
            });
            if acted.is_err() {
                return;
            }
        }
    })
    .detach();
}

/// What one wake decided, carried out of the foreground hop.
struct Tick {
    waiting: usize,
    redrawn: bool,
    /// Hotkey presses plus tray clicks: anything that asks for the deck's presence to flip.
    toggles: usize,
    auto_open: bool,
    open: bool,
    icon: Option<Frame>,
}

/// Open or close the deck to match `open`, if it is not already there. `summoned` is a person's own
/// click or keystroke: only then does the deck take the keyboard, and only then is the keyboard handed
/// back when it closes.
fn set_deck(cx: &mut App, app: &Entity<MenubarApp>, open: bool, icon: Option<Frame>, summoned: bool) {
    let up = cx.try_global::<DeckSession>().map(|session| session.is_open()).unwrap_or(false);
    if open == up {
        return;
    }
    if open {
        open_deck(cx, app, icon, summoned);
    } else {
        close_deck(cx, app, summoned);
    }
}

/// Open the deck under the icon. A window that cannot be created is said out loud: silently doing
/// nothing on a hotkey press is what makes a person stop trusting the key.
fn open_deck(cx: &mut App, app: &Entity<MenubarApp>, icon: Option<Frame>, summoned: bool) {
    // Who the person was looking at, read *before* the deck takes the keyboard — afterwards the deck
    // is the frontmost app. The app's identity costs no permission; nothing in its windows is read.
    #[cfg(target_os = "macos")]
    {
        let frontmost = harness_capture::MacCaptureSource::new().frontmost();
        app.update(cx, |app, cx| {
            app.note_target(frontmost);
            cx.notify();
        });
    }
    // The conversations as they are now — a terminal or Discord may have started one since the deck
    // last opened, and the newest is where the deck picks up.
    app.update(cx, |app, cx| app.refresh_conversations(cx));
    let bounds = window::bounds_for(cx, icon);
    let root = app.clone();
    // The view's window is only reachable inside the build closure; the pointer comes out of it
    // through this cell rather than through a second lookup.
    let view_ptr = std::cell::Cell::new(None);
    let backdrop = window::Backdrop::detect();
    let opened = {
        let view_ptr = &view_ptr;
        cx.open_window(window::options(bounds, backdrop), move |window, cx| {
            view_ptr.set(window::view_ptr_of(window));
            window::dress(window);
            let glass = backdrop == window::Backdrop::Glass && window::install_glass(window);
            // Said once per run, so "why does it not look like glass" has an answer in the log.
            static SAID: std::sync::Once = std::sync::Once::new();
            SAID.call_once(|| eprintln!("harness-menubar: deck backdrop: {}", if glass { "Liquid Glass" } else { "blur (no NSGlassEffectView on this macOS)" }));
            cx.new(|cx| DeckView::new(root, cx))
        })
    };
    let handle = match opened {
        Ok(handle) => handle,
        Err(error) => {
            eprintln!("harness-menubar: the deck could not open: {error}");
            return;
        }
    };
    cx.global_mut::<DeckSession>().opened(handle.into(), view_ptr.get());
    if summoned {
        // An accessory app is never active by itself, and a window of an inactive app cannot be key:
        // without this the deck would sit behind the person's typing and the outside-click check
        // would have nothing to compare against. "Regardless of what app the user is in" is this call.
        cx.activate(true);
        let _ = handle.update(cx, |_view, window, _cx| window.activate_window());
    }
    app.update(cx, |app, cx| {
        app.deck_open = true;
        cx.notify();
    });
}

/// Close the deck and say so in the model, so the tray and the next tick agree it is gone. When the
/// person closed it themselves, the keyboard goes back to the app they summoned it over — an accessory
/// app left active with no window would otherwise swallow their next keystroke.
fn close_deck(cx: &mut App, app: &Entity<MenubarApp>, hand_back: bool) {
    // The handle is taken out before it is used: closing the window needs `&mut App`, and holding a
    // borrow of the global across that call would be two mutable borrows of one `App`.
    let handle = cx.try_global::<DeckSession>().and_then(|session| session.handle);
    cx.global_mut::<DeckSession>().forget();
    if let Some(handle) = handle {
        // A window already closing by its own hand fails here; that is not worth saying.
        let _ = handle.update(cx, |_view, window, _cx| window.remove_window());
    }
    if hand_back {
        cx.hide();
    }
    app.update(cx, |app, cx| {
        app.deck_open = false;
        cx.notify();
    });
}

/// `--deck-selftest`: open the deck, say where it landed, close it, quit. This is the only way to
/// find out whether the popover window — `WindowKind::PopUp`, a transparent background, an
/// accessory app with no Dock icon — actually comes up on this machine, since a headless test cannot
/// create a window at all.
fn run_deck_selftest(cx: &mut App) {
    let app = app::install(cx);
    cx.set_global(DeckSession::default());
    cx.spawn(async move |cx| {
        cx.background_executor().timer(Duration::from_millis(500)).await;
        let Ok(bounds) = cx.update(|cx| {
            let bounds = window::bounds_for(cx, None);
            eprintln!("deck-selftest: opening at ({:?}, {:?}) {:?}", bounds.origin.x, bounds.origin.y, bounds.size);
            open_deck(cx, &app, None, true);
            bounds
        }) else { return };
        let _ = bounds;
        cx.background_executor().timer(Duration::from_millis(800)).await;
        // What a summon has to achieve, read back from AppKit: the deck is the key window of the
        // active app, and the app the person was in is remembered as the capture target.
        let _ = cx.update(|cx| {
            let key = cx.try_global::<DeckSession>().is_some_and(|session| session.still_key());
            let active = objc2::MainThreadMarker::new().is_some_and(|mtm| NSApplication::sharedApplication(mtm).isActive());
            let target = app.read(cx).target.clone().map(|t| format!("{} ({})", t.name, t.bundle_id)).unwrap_or_else(|| "none".into());
            eprintln!("deck-selftest: key={key} active={active} target={target}");
            if let Some(chrome) = cx.try_global::<DeckSession>().and_then(|session| session.chrome()) {
                eprintln!("deck-selftest: {chrome}");
            }
        });
        cx.background_executor().timer(Duration::from_secs(2)).await;
        let _ = cx.update(|cx| close_deck(cx, &app, true));
        cx.background_executor().timer(Duration::from_millis(500)).await;
        let frontmost = harness_capture::MacCaptureSource::new().frontmost().map(|f| f.name).unwrap_or_default();
        eprintln!("deck-selftest: closed; frontmost is now {frontmost}");
        let _ = cx.update(|cx| cx.quit());
    })
    .detach();
}

/// `--tray-selftest`: cycle the three states with no daemon at all, so the icons can be looked at.
/// A ten-second eyeball check that the glyphs read right — the honest substitute for a test, since
/// real status-item pixels are not observable.
fn run_selftest(cx: &mut App) {
    let tray = match Tray::install() {
        Ok(tray) => tray,
        Err(error) => {
            eprintln!("harness-menubar: {error}");
            return;
        }
    };
    cx.set_global(TrayHandle(tray));
    cx.spawn(async move |cx| {
        let states = [TrayState::Idle, TrayState::Executing, TrayState::ActionRequired];
        let mut step = 0usize;
        loop {
            let state = states[step % states.len()];
            let waiting = if state == TrayState::ActionRequired { 2 } else { 0 };
            let shown = cx.update(|cx| cx.global_mut::<TrayHandle>().0.show(state, waiting));
            match shown {
                Ok(Ok(_)) => {
                    // Where AppKit placed the item: a frame in the menu bar is the evidence the icon is
                    // really there, since the window server's own window list does not report status
                    // items to an unprivileged process on recent macOS.
                    let rect = cx.update(|cx| cx.global::<TrayHandle>().0.icon_rect()).ok().flatten();
                    match rect {
                        Some(rect) => eprintln!("tray-selftest: {state:?} at ({:.0}, {:.0}) {:.0}x{:.0}", rect.position.x, rect.position.y, rect.size.width, rect.size.height),
                        None => eprintln!("tray-selftest: {state:?} (not yet placed in the menu bar)"),
                    }
                }
                Ok(Err(error)) => {
                    eprintln!("harness-menubar: {error}");
                    return;
                }
                Err(_) => return,
            }
            step += 1;
            cx.background_executor().timer(Duration::from_secs(2)).await;
        }
    })
    .detach();
}
