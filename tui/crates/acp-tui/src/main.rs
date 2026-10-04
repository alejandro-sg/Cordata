mod app;
mod commands;
mod config_ui;
mod input;
mod settings;
mod term;
mod ui;

use std::io;
use std::panic;
use std::path::PathBuf;
use std::time::Duration;

use acp_core::{AppEvent, BackendConfig, BackendTransport, SessionHandle, SessionOptions};
use anyhow::{Context, Result};
use clap::Parser;
use crossterm::cursor::Show;
use crossterm::event::{DisableBracketedPaste, EnableBracketedPaste, Event, EventStream};
use crossterm::{execute, terminal};
use futures::StreamExt;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use tokio::sync::mpsc;

use crate::app::{App, Effect, Message, SwitchTarget, TabBadge};
use crate::term::InlineTerm;
use crate::ui::transcript;

/// Interactive terminal client for the sierralab-agents ACP backend.
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

    /// How to reach the agent: `stdio` spawns a backend process for this
    /// terminal; `ws` attaches to the always-on daemon, which keeps the
    /// session alive across a dropped terminal and lets several clients
    /// reattach to it.
    #[arg(long, value_enum, default_value_t = Transport::Stdio)]
    transport: Transport,

    /// Daemon URL, overriding the one built from ~/.sierralab/daemon.json
    /// (which supplies the port and the token). Only meaningful with
    /// `--transport ws`.
    #[arg(long, value_name = "URL")]
    daemon_url: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum Transport {
    Stdio,
    Ws,
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
}

impl Workspace {
    fn spawn(&mut self, resume: Option<String>, continue_latest: bool) -> usize {
        let (handle, mut rx) = SessionHandle::spawn(SessionOptions {
            backend: self.backend.clone(),
            cwd: self.cwd.clone(),
            transport: self.transport.clone(),
            resume,
            debug_log: self.debug_log.clone(),
            continue_latest,
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
        let mut app = App::new();
        app.attach_store(settings::Store::discover(&self.cwd));
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

    let mut backend = BackendConfig::discover(args.backend_root.as_deref())?;
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

    let transport = match (args.transport, args.daemon_url) {
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
    let mut workspace = Workspace {
        slots: Vec::new(),
        active: 0,
        next_number: 1,
        backend,
        cwd,
        transport,
        debug_log: args.debug_log,
        events: tx,
    };
    workspace.spawn(args.resume, args.continue_latest);

    install_panic_hook();
    let guard = TerminalGuard::enter()?;
    let mut screen = InlineTerm::new(io::stdout());
    let result = run(&mut workspace, &mut screen, rx).await;
    let _ = screen.clear_live();
    drop(guard);
    for slot in &workspace.slots {
        if let Some(info) = &slot.app.session {
            println!("resume this session:  acp-tui --resume {}", info.session_id);
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

fn restore_terminal() {
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

async fn run(
    workspace: &mut Workspace,
    screen: &mut InlineTerm<io::Stdout>,
    mut events: mpsc::UnboundedReceiver<(usize, AppEvent)>,
) -> Result<()> {
    let mut keys = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    let mut last = None;
    let mut banner: Option<Line<'static>> = None;

    loop {
        workspace.update_tabs();
        draw(workspace.app(), screen, &mut last, &mut banner)?;

        let mut effects = Vec::new();
        tokio::select! {
            Some(event) = keys.next() => match event? {
                Event::Key(key) => effects.extend(workspace.app().on_key(key)),
                Event::Paste(text) => workspace.app().on_paste(&text),
                // The terminal re-wrapped what was on screen; draw the region afresh.
                Event::Resize(..) => last = None,
                _ => {}
            },
            Some((number, event)) = events.recv() => {
                workspace.route(number, event);
                // Drain the burst so a fast stream costs one redraw, not one per chunk.
                while let Ok((number, event)) = events.try_recv() {
                    workspace.route(number, event);
                }
            }
            _ = tick.tick() => {
                for slot in &mut workspace.slots {
                    slot.app.tick();
                }
            }
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
                    if matches!(workspace.transport, BackendTransport::Ws { .. }) {
                        // The daemon hands out one session per directory and
                        // refuses a second client on it (4409), so a second slot
                        // here could only fail to connect — say why instead.
                        workspace.app().messages.push(Message::Info(
                            "/new needs --transport stdio: the daemon keeps one session per \
                             directory — run another terminal in a different folder"
                                .into(),
                        ));
                    } else {
                        workspace.active = workspace.spawn(None, false);
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
