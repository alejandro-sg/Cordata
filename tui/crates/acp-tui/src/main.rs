mod overlay;
mod term;
mod theme;
mod ui;

// The state half moved to cordata-view (shared with the native app); kept at its old paths so `crate::app::App` and
// friends still name it everywhere here.
use cordata_view::{app, commands, config_ui, extras, input, menu, settings, shell};

use std::io;
use std::panic;
use std::path::PathBuf;
use std::time::Duration;

use acp_core::{AppEvent, BackendConfig, BackendTransport, DaemonConfig, SessionHandle, SessionOptions, hub};
use anyhow::{Context, Result};
use clap::Parser;
use std::sync::atomic::{AtomicBool, Ordering};

use crossterm::cursor::{Hide, Show};
use crossterm::event::{DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event, EventStream};
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::{execute, terminal};
use futures::StreamExt;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use tokio::sync::mpsc;

use crate::app::{App, Effect, LocalEvent, Message, SwitchTarget, TabBadge};
use crate::term::InlineTerm;
use crate::ui::transcript;

/// Interactive terminal client for the Cordata ACP backend.
#[derive(Parser)]
#[command(version)]
struct Args {
    /// Resume an existing session id (session/load). The backend replays text
    /// only, and always comes back in build mode.
    #[arg(long, value_name = "SESSION_ID")]
    resume: Option<String>,

    /// Resume the newest saved session for this folder (session/list).
    #[arg(short = 'c', long = "continue", conflicts_with = "resume")]
    continue_latest: bool,

    /// Project directory the agent works in [default: current directory].
    #[arg(long)]
    cwd: Option<PathBuf>,

    /// Repo root or backend/ dir [env: ACP_TUI_BACKEND_ROOT; default: auto-discover].
    #[arg(long)]
    backend_root: Option<PathBuf>,

    /// Append raw JSON-RPC traffic and server stderr to this file.
    #[arg(long, value_name = "FILE")]
    debug_log: Option<PathBuf>,

    /// Run a different ACP server script with the backend's python (testing).
    #[arg(long, hide = true)]
    server_script: Option<PathBuf>,

    /// How to reach the agent: `hub` joins a conversation the daemon holds —
    /// the same one Discord, the menu bar and the web app continue; `stdio`
    /// spawns a backend process for this terminal only; `ws` attaches to the
    /// daemon's per-folder session; `auto` (the default) is `hub` when the
    /// daemon is running and `stdio` when it is not.
    #[arg(long, value_enum, default_value_t = Transport::Auto)]
    transport: Transport,

    /// The hub conversation to join: a handle (`c3f9a1e`), `latest` (this
    /// folder's newest — the default, so a terminal picks up where any door
    /// left off) or `new`.
    #[arg(long, value_name = "HANDLE")]
    conversation: Option<String>,

    /// Daemon URL, overriding the one built from ~/.cordata/daemon.json
    /// (which supplies the port and the token). Only meaningful with
    /// `--transport ws`.
    #[arg(long, value_name = "URL")]
    daemon_url: Option<String>,

    /// Initial prompt to send once connected (like Claude Code / agy "prompt").
    #[arg(value_name = "PROMPT")]
    prompt: Option<String>,

    /// Start in full-screen split-pane workspace mode.
    #[arg(long)]
    panes: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum Transport {
    Auto,
    Hub,
    Stdio,
    Ws,
    /// The in-process Rust agent (prototype: chat only, no tools yet).
    Native,
}

/// One session the user can switch to: its backend process and its UI state.
struct Slot {
    /// Stable label (1, 2, …) — numbers aren't reused after a close.
    number: usize,
    handle: Option<SessionHandle>,
    app: App,
}

/// Every open session in this project. One backend process per session (the
/// backend chdirs per session, so sessions must not share a process); their
/// events are tagged with the slot number and merged into one channel.
struct Workspace {
    slots: Vec<Slot>,
    active: usize,
    next_number: usize,
    backend: BackendConfig,
    cwd: PathBuf,
    transport: BackendTransport,
    debug_log: Option<PathBuf>,
    events: mpsc::UnboundedSender<(usize, AppEvent)>,
    /// Results of work done on this side of the wire (`!commands`), tagged with the slot that asked.
    local: mpsc::UnboundedSender<(usize, LocalEvent)>,
}

impl Workspace {
    fn spawn(&mut self, resume: Option<String>, continue_latest: bool, initial_prompt: Option<String>) -> usize {
        self.spawn_in(resume, continue_latest, None, initial_prompt)
    }

    /// A slot on a particular hub conversation (`new`, a handle) instead of the workspace's own.
    fn spawn_in(
        &mut self,
        resume: Option<String>,
        continue_latest: bool,
        conversation: Option<String>,
        initial_prompt: Option<String>,
    ) -> usize {
        let transport = match (&self.transport, conversation) {
            (BackendTransport::Hub { daemon, .. }, Some(conversation)) => BackendTransport::Hub { daemon: daemon.clone(), conversation },
            (transport, _) => transport.clone(),
        };
        let (handle, mut rx) = SessionHandle::spawn(SessionOptions {
            backend: self.backend.clone(),
            cwd: self.cwd.clone(),
            transport,
            resume,
            debug_log: self.debug_log.clone(),
            continue_latest,
            hub_handle: None,
        });
        let number = self.next_number;
        self.next_number += 1;
        let tx = self.events.clone();
        tokio::spawn(async move {
            while let Some(event) = rx.recv().await {
                if tx.send((number, event)).is_err() {
                    break;
                }
            }
        });
        let mut app = crate::overlay::new_app();
        app.attach_store(settings::Store::discover(&self.cwd));
        app.initial_prompt = initial_prompt;
        self.slots.push(Slot {
            number,
            handle: Some(handle),
            app,
        });
        self.slots.len() - 1
    }

    fn app(&mut self) -> &mut App {
        &mut self.slots[self.active].app
    }

    fn handle(&self) -> Option<&SessionHandle> {
        self.slots[self.active].handle.as_ref()
    }

    fn route(&mut self, number: usize, event: AppEvent) {
        if let Some(slot) = self.slots.iter_mut().find(|slot| slot.number == number) {
            slot.app.on_event(event);
        }
    }

    /// Refresh every app's tab strip (only the shown one is drawn).
    fn update_tabs(&mut self) {
        let tabs: Vec<TabBadge> = self
            .slots
            .iter()
            .enumerate()
            .map(|(index, slot)| TabBadge {
                number: slot.number,
                active: index == self.active,
                state: slot.app.tab_state(),
            })
            .collect();
        for slot in &mut self.slots {
            slot.app.tabs.clone_from(&tabs);
        }
    }

    /// A divider written into scrollback when the shown session changes.
    fn banner(&self) -> Line<'static> {
        let slot = &self.slots[self.active];
        let title = slot.app.title.clone().unwrap_or_else(|| {
            slot.app
                .session
                .as_ref()
                .map(|s| s.session_id.chars().take(8).collect())
                .unwrap_or_else(|| "connecting…".to_owned())
        });
        Line::styled(
            format!(
                "━━ session {} of {} · {title} ━━",
                slot.number,
                self.slots.len()
            ),
            Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        )
    }

    fn switch(&mut self, target: SwitchTarget) -> bool {
        let len = self.slots.len();
        let next = match target {
            SwitchTarget::Next => (self.active + 1) % len,
            SwitchTarget::Prev => (self.active + len - 1) % len,
            SwitchTarget::Slot(number) => {
                match self.slots.iter().position(|s| s.number == number) {
                    Some(index) => index,
                    None => {
                        self.app().messages.push(Message::Info(format!(
                            "no session {number} — /sessions lists them"
                        )));
                        return false;
                    }
                }
            }
        };
        let changed = next != self.active;
        self.active = next;
        changed
    }

    fn list(&mut self) {
        let rows: Vec<String> = self
            .slots
            .iter()
            .enumerate()
            .map(|(index, slot)| {
                let id = slot
                    .app
                    .session
                    .as_ref()
                    .map(|s| s.session_id.chars().take(8).collect::<String>())
                    .unwrap_or_else(|| "…".into());
                format!(
                    "  {}{} {id}  {:?}  {}",
                    if index == self.active { "▸" } else { " " },
                    slot.number,
                    slot.app.tab_state(),
                    slot.app.title.clone().unwrap_or_default()
                )
            })
            .collect();
        let text = format!(
            "open sessions (Ctrl+←/→ or Alt+N to switch, Ctrl+N for another):\n{}",
            rows.join("\n")
        );
        self.app().messages.push(Message::Info(text));
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    // The native agent has no Python backend, so a missing checkout is not an error for it.
    let mut backend = match BackendConfig::discover(args.backend_root.as_deref()) {
        Ok(backend) => backend,
        Err(_) if args.transport == Transport::Native => {
            BackendConfig { backend_dir: PathBuf::new(), python: PathBuf::new(), server: PathBuf::new() }
        }
        Err(error) => return Err(error.into()),
    };
    if let Some(script) = args.server_script {
        backend = backend.with_server_script(script);
    }
    // With `--transport ws` the daemon spawns the child, not this process, so a
    // missing local backend is the daemon's problem to report — and it does, as
    // a `ConnectionError` naming the directory it tried.
    if args.transport == Transport::Stdio {
        backend.validate()?;
    }

    let cwd = match args.cwd {
        Some(cwd) => cwd
            .canonicalize()
            .with_context(|| format!("--cwd {}", cwd.display()))?,
        None => std::env::current_dir()?,
    };

    let hub_for = |conversation: Option<String>| -> Result<BackendTransport> {
        Ok(BackendTransport::Hub { daemon: DaemonConfig::discover()?, conversation: conversation.unwrap_or_else(|| "latest".into()) })
    };
    let transport = match (args.transport, args.daemon_url) {
        (Transport::Hub, _) => hub_for(args.conversation.clone())?,
        // `--resume` / `--continue` name a terminal-only session, so they keep the stdio backend.
        (Transport::Auto, _) if args.resume.is_some() || args.continue_latest => BackendTransport::Stdio,
        (Transport::Auto, _) => match DaemonConfig::discover() {
            Ok(daemon) if hub::reachable(&daemon).await => BackendTransport::Hub { daemon, conversation: args.conversation.clone().unwrap_or_else(|| "latest".into()) },
            _ if args.conversation.is_some() => anyhow::bail!("--conversation needs the daemon — start it with `make daemon`"),
            _ => BackendTransport::Stdio,
        },
        (Transport::Native, None) => BackendTransport::Native,
        (Transport::Native, Some(_)) => anyhow::bail!("--daemon-url needs --transport ws"),
        (Transport::Stdio, None) => BackendTransport::Stdio,
        // A flag that is accepted and then ignored is worse than one that
        // complains: `--daemon-url` with stdio means the user thinks they are
        // talking to the daemon.
        (Transport::Stdio, Some(_)) => anyhow::bail!("--daemon-url needs --transport ws"),
        (Transport::Ws, Some(url)) => BackendTransport::Ws { url },
        // Built here rather than in the session task: the token file is read
        // once, and a missing daemon config should fail before the terminal is
        // taken over, not inside the alternate screen where the message would
        // be hard to see.
        (Transport::Ws, None) => BackendTransport::daemon(&cwd)?,
    };

    let (tx, rx) = mpsc::unbounded_channel();
    let (local_tx, local_rx) = mpsc::unbounded_channel();
    let mut workspace = Workspace {
        slots: Vec::new(),
        active: 0,
        next_number: 1,
        backend,
        cwd,
        transport,
        debug_log: args.debug_log,
        events: tx,
        local: local_tx,
    };
    workspace.spawn(args.resume, args.continue_latest, args.prompt);
    if args.panes || workspace.app().settings.get("ui.panes").and_then(serde_json::Value::as_bool).unwrap_or(false) {
        workspace.app().open_overlay(crate::overlay::Kind::Panes);
    }

    install_panic_hook();
    let guard = TerminalGuard::enter()?;
    let mut screen = InlineTerm::new(io::stdout());
    let result = run(&mut workspace, &mut screen, rx, local_rx).await;
    let _ = screen.clear_live();
    drop(guard);
    for slot in &workspace.slots {
        if let Some(info) = &slot.app.session {
            match info.transport.strip_prefix("hub · ") {
                // A hub conversation is continued by handle, from here or any other door.
                Some(handle) => println!("continue this conversation anywhere:  cordata --conversation {handle}  (or /open {handle})"),
                None => println!("resume this session:  cordata --resume {}", info.session_id),
            }
        }
    }
    for slot in &mut workspace.slots {
        if let Some(handle) = slot.handle.take() {
            handle.shutdown().await;
        }
    }
    result
}

/// Raw mode + bracketed paste for as long as the UI runs. There is no alternate
/// screen and no mouse capture: output lives in the terminal's own scrollback, so
/// the wheel, trackpad, selection and copy all behave as they do in any shell.
struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> Result<Self> {
        terminal::enable_raw_mode()?;
        execute!(io::stdout(), EnableBracketedPaste)?;
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_terminal();
    }
}

/// Whether an overlay currently has the alternate screen and the mouse (so a crash can give them back).
static OVERLAY_SCREEN: AtomicBool = AtomicBool::new(false);

/// Take the alternate screen and the mouse for an overlay. The inline conversation is untouched underneath: leaving
/// puts the terminal back exactly as it was, scrollback and live region included.
fn enter_overlay_screen() -> io::Result<()> {
    execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture, Hide)?;
    OVERLAY_SCREEN.store(true, Ordering::SeqCst);
    Ok(())
}

fn leave_overlay_screen() {
    if OVERLAY_SCREEN.swap(false, Ordering::SeqCst) {
        let _ = execute!(io::stdout(), DisableMouseCapture, LeaveAlternateScreen, Show);
    }
}

fn restore_terminal() {
    leave_overlay_screen();
    let _ = execute!(io::stdout(), DisableBracketedPaste, Show);
    let _ = terminal::disable_raw_mode();
}

fn install_panic_hook() {
    let default = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        restore_terminal();
        default(info);
    }));
}

/// What one frame put on screen, to skip redrawing when nothing changed.
#[derive(PartialEq)]
struct Frame {
    rows: Vec<String>,
    cursor: Option<(u16, u16)>,
}

fn draw(
    app: &mut App,
    screen: &mut InlineTerm<io::Stdout>,
    last: &mut Option<Frame>,
    banner: &mut Option<Line<'static>>,
) -> io::Result<()> {
    let (width, height) = terminal::size()?;
    let mut commits = Vec::new();
    if let Some(line) = banner.take() {
        commits.push(Line::raw(""));
        commits.push(line);
    }
    commits.extend(transcript::take_commits(app, width));
    let live = ui::render_live(app, width, height);
    let frame = Frame {
        rows: term::buffer_to_ansi_rows(&live.buffer),
        cursor: live.cursor,
    };
    if commits.is_empty() && last.as_ref() == Some(&frame) {
        return Ok(());
    }
    screen.draw(
        &term::lines_to_ansi(&commits, width),
        &frame.rows,
        frame.cursor,
    )?;
    *last = Some(frame);
    Ok(())
}

/// One full-screen frame of the open overlay, written in place (no scrolling, no trailing newline).
fn draw_overlay(app: &mut App, last: &mut Option<Frame>) -> io::Result<()> {
    use std::io::Write;
    let (width, height) = terminal::size()?;
    let Some(mut boxed) = app.overlay.take() else {
        return Ok(());
    };
    let Some(overlay) = overlay::concrete(&mut boxed) else {
        app.overlay = Some(boxed);
        return Ok(());
    };
    let buffer = overlay.render(app, width, height);
    let cursor = overlay.cursor();
    app.overlay = Some(boxed);
    let rows = term::buffer_to_ansi_rows(&buffer);
    let frame = Frame {
        rows,
        cursor,
    };
    if last.as_ref() == Some(&frame) {
        return Ok(());
    }
    let mut out = String::from("\x1b[?2026h");
    for (y, row) in frame.rows.iter().enumerate() {
        out.push_str(&format!("\x1b[{};1H\x1b[2K{row}", y + 1));
    }
    match frame.cursor {
        Some((cx, cy)) => {
            out.push_str(&format!("\x1b[{};{}H\x1b[?25h", cy + 1, cx + 1));
        }
        None => {
            out.push_str("\x1b[?25l");
        }
    }
    out.push_str("\x1b[?2026l");
    let mut stdout = io::stdout();
    stdout.write_all(out.as_bytes())?;
    stdout.flush()?;
    *last = Some(frame);
    Ok(())
}

async fn run(
    workspace: &mut Workspace,
    screen: &mut InlineTerm<io::Stdout>,
    mut events: mpsc::UnboundedReceiver<(usize, AppEvent)>,
    mut local: mpsc::UnboundedReceiver<(usize, LocalEvent)>,
) -> Result<()> {
    let mut keys = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    let mut last = None;
    let mut banner: Option<Line<'static>> = None;
    let mut overlay_on = false;
    let mut last_overlay: Option<Frame> = None;

    loop {
        workspace.update_tabs();
        let want_overlay = workspace.app().overlay.is_some();
        if want_overlay != overlay_on {
            if want_overlay {
                enter_overlay_screen()?;
            } else {
                leave_overlay_screen();
                last = None; // the inline region is redrawn over what the terminal gave back
            }
            overlay_on = want_overlay;
            last_overlay = None;
        }
        if overlay_on {
            draw_overlay(workspace.app(), &mut last_overlay)?;
        } else {
            draw(workspace.app(), screen, &mut last, &mut banner)?;
        }

        let mut effects = Vec::new();
        tokio::select! {
            Some(event) = keys.next() => match event? {
                Event::Key(key) => effects.extend(workspace.app().on_key(key.into())),
                Event::Paste(text) => workspace.app().on_paste(&text),
                Event::Mouse(mouse) => effects.extend(workspace.app().on_mouse(mouse.into())),
                // The terminal re-wrapped what was on screen; draw the region afresh.
                Event::Resize(..) => {
                    last = None;
                    last_overlay = None;
                }
                _ => {}
            },
            Some((number, event)) = events.recv() => {
                workspace.route(number, event);
                // Drain the burst so a fast stream costs one redraw, not one per chunk.
                while let Ok((number, event)) = events.try_recv() {
                    workspace.route(number, event);
                }
            }
            Some((number, event)) = local.recv() => {
                if let Some(slot) = workspace.slots.iter_mut().find(|slot| slot.number == number) {
                    slot.app.on_local(event);
                }
            }
            _ = tick.tick() => {
                for slot in &mut workspace.slots {
                    slot.app.tick();
                }
            }
        }

        if let Some(effect) = workspace.app().take_auto_resume() {
            effects.push(effect);
        }
        if let Some(effect) = workspace.app().take_initial_prompt() {
            effects.push(effect);
        }
        if let Some(effect) = workspace.app().take_queued() {
            effects.push(effect);
        }
        if std::mem::take(&mut workspace.app().attention) && std::env::var_os("CORDATA_NOTIFY").is_none_or(|v| v != "off") {
            // A terminal bell: the dock icon bounces / the tab lights up, in the terminals that do either.
            let mut out = io::stdout();
            let _ = io::Write::write_all(&mut out, b"\x07");
            let _ = io::Write::flush(&mut out);
        }
        for effect in effects {
            let outcome = match effect {
                Effect::Quit => return Ok(()),
                Effect::ClearScreen => {
                    screen.clear_screen()?;
                    last = None;
                    Ok(())
                }
                Effect::NewSession => {
                    if matches!(workspace.transport, BackendTransport::Hub { .. }) {
                        // Another conversation on the hub: visible to every door from its first turn.
                        workspace.active = workspace.spawn_in(None, false, Some("new".into()), None);
                        banner = Some(workspace.banner());
                        last = None;
                    } else if matches!(workspace.transport, BackendTransport::Ws { .. }) {
                        // The daemon hands out one session per directory and
                        // refuses a second client on it (4409), so a second slot
                        // here could only fail to connect — say why instead.
                        workspace.app().messages.push(Message::Info(
                            "/new needs --transport stdio: the daemon keeps one session per \
                             directory — run another terminal in a different folder"
                                .into(),
                        ));
                    } else {
                        workspace.active = workspace.spawn(None, false, None);
                        banner = Some(workspace.banner());
                        last = None;
                    }
                    Ok(())
                }
                Effect::SwitchSession(target) => {
                    if workspace.switch(target) {
                        banner = Some(workspace.banner());
                        last = None;
                    }
                    Ok(())
                }
                Effect::CloseSession => {
                    let slot = workspace.slots.remove(workspace.active);
                    if let Some(handle) = slot.handle {
                        tokio::spawn(handle.shutdown());
                    }
                    if workspace.slots.is_empty() {
                        return Ok(());
                    }
                    workspace.active = workspace.active.min(workspace.slots.len() - 1);
                    banner = Some(workspace.banner());
                    last = None;
                    Ok(())
                }
                Effect::ListSessions => {
                    workspace.list();
                    let cwd = workspace.cwd.display().to_string();
                    match workspace.handle() {
                        Some(handle) => handle.ext(
                            "session/list",
                            serde_json::json!({ "cwd": cwd }),
                            "sessions",
                        ),
                        None => Ok(()),
                    }
                }
                Effect::ListConversations => {
                    match &workspace.transport {
                        BackendTransport::Hub { daemon, .. } => match hub::list(daemon, None).await {
                            Ok(rows) if rows.is_empty() => workspace.app().messages.push(Message::Info("no conversations yet".into())),
                            Ok(rows) => {
                                let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or_default();
                                let mut text = String::from("conversations (shared with Discord, the menu bar and the web app — /open <handle>):");
                                for row in rows.iter().take(15) {
                                    text.push_str("\n  ");
                                    text.push_str(&row.summary(now));
                                }
                                workspace.app().messages.push(Message::Info(text));
                            }
                            Err(error) => workspace.app().messages.push(Message::Error(format!("could not list conversations: {error}"))),
                        },
                        _ => workspace.app().messages.push(Message::Info("/conversations needs the daemon's hub — start it (`make daemon`) and run acp-tui again".into())),
                    }
                    Ok(())
                }
                Effect::OpenConversation(handle) => {
                    if matches!(workspace.transport, BackendTransport::Hub { .. }) {
                        workspace.active = workspace.spawn_in(None, false, Some(handle), None);
                        banner = Some(workspace.banner());
                        last = None;
                    } else {
                        workspace.app().messages.push(Message::Info("/open needs the daemon's hub — start it (`make daemon`) and run acp-tui again".into()));
                    }
                    Ok(())
                }
                Effect::Shell { command, share, as_diff } => {
                    let slot = &workspace.slots[workspace.active];
                    let (number, cwd, tx) = (slot.number, slot.app.project_dir(), workspace.local.clone());
                    tokio::task::spawn_blocking(move || {
                        let outcome = shell::run(&command, &cwd, shell::TIMEOUT);
                        let _ = tx.send((number, LocalEvent::ShellDone { command, outcome, share, as_diff }));
                    });
                    Ok(())
                }
                Effect::Review => {
                    let slot = &workspace.slots[workspace.active];
                    let (number, cwd, tx) = (slot.number, slot.app.project_dir(), workspace.local.clone());
                    tokio::task::spawn_blocking(move || {
                        let diff = "(git --no-pager diff HEAD --unified=3 --no-color || git --no-pager diff --unified=3 --no-color); git ls-files --others --exclude-standard -z | while IFS= read -r -d '' f; do [ -f \"$f\" ] && git --no-pager diff --no-index --unified=3 --no-color -- /dev/null \"$f\" || true; done; true";
                        let outcome = shell::run_keeping(diff, &cwd, shell::TIMEOUT, 400 * 1024);
                        let _ = tx.send((number, LocalEvent::ReviewDone(outcome)));
                    });
                    Ok(())
                }
                Effect::Copy(text) => {
                    copy_to_clipboard(&text);
                    Ok(())
                }
                Effect::SendPrompt(text) => with_handle(workspace, |h| h.send_prompt(text)),
                Effect::Cancel => with_handle(workspace, SessionHandle::cancel),
                Effect::SetMode(mode) => with_handle(workspace, |h| h.set_mode(mode)),
                Effect::SetConfig { id, value } => {
                    with_handle(workspace, |h| h.set_config_option(id, value))
                }
                Effect::Ext {
                    method,
                    params,
                    tag,
                } => with_handle(workspace, |h| h.ext(method, params, tag)),
            };
            if let Err(error) = outcome {
                workspace
                    .app()
                    .messages
                    .push(Message::Error(format!("session unavailable: {error}")));
            }
        }
    }
}

fn with_handle(
    workspace: &Workspace,
    f: impl FnOnce(&SessionHandle) -> acp_core::Result<()>,
) -> acp_core::Result<()> {
    match workspace.handle() {
        Some(handle) => f(handle),
        None => Err(acp_core::Error::SessionClosed),
    }
}

/// The system clipboard: `pbcopy` on macOS, `wl-copy` / `xclip` elsewhere, and the terminal's own OSC 52 when none of
/// those exist (works over SSH in most modern terminals).
fn copy_to_clipboard(text: &str) {
    use std::io::Write;
    use std::process::{Command, Stdio};
    for (program, args) in [("pbcopy", vec![]), ("wl-copy", vec![]), ("xclip", vec!["-selection", "clipboard"])] {
        if let Ok(mut child) = Command::new(program).args(&args).stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null()).spawn() {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(text.as_bytes());
            }
            let _ = child.wait();
            return;
        }
    }
    let encoded = base64_encode(text.as_bytes());
    let mut out = io::stdout();
    let _ = write!(out, "\x1b]52;c;{encoded}\x07");
    let _ = out.flush();
}

fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = (u32::from(chunk[0]) << 16) | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8) | u32::from(*chunk.get(2).unwrap_or(&0));
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { TABLE[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { TABLE[n as usize & 63] as char } else { '=' });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::base64_encode;

    #[test]
    fn base64_matches_the_standard_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }
}
