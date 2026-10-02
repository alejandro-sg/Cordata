mod app;
mod commands;
mod input;
mod ui;

use std::path::PathBuf;
use std::time::Duration;

use acp_core::{BackendConfig, SessionHandle, SessionOptions};
use anyhow::{Context, Result};
use clap::Parser;
use crossterm::event::{Event, EventStream};
use futures::StreamExt;
use ratatui::DefaultTerminal;

use crate::app::{App, Effect, Message};

/// Interactive terminal client for the sierralab-agents ACP backend.
#[derive(Parser)]
#[command(version)]
struct Args {
    /// Resume an existing session id (session/load). The backend replays text
    /// only, and always comes back in build mode.
    #[arg(long, value_name = "SESSION_ID")]
    resume: Option<String>,

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
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    let mut backend = BackendConfig::discover(args.backend_root.as_deref())?;
    if let Some(script) = args.server_script {
        backend = backend.with_server_script(script);
    }
    backend.validate()?;

    let cwd = match args.cwd {
        Some(cwd) => cwd
            .canonicalize()
            .with_context(|| format!("--cwd {}", cwd.display()))?,
        None => std::env::current_dir()?,
    };

    let (session, events) = SessionHandle::spawn(SessionOptions {
        backend,
        cwd,
        resume: args.resume,
        debug_log: args.debug_log,
    });

    let mut terminal = ratatui::init();
    let result = run(&mut terminal, &session, events).await;
    ratatui::restore();
    session.shutdown().await;
    result
}

async fn run(
    terminal: &mut DefaultTerminal,
    session: &SessionHandle,
    mut events: tokio::sync::mpsc::UnboundedReceiver<acp_core::AppEvent>,
) -> Result<()> {
    let mut app = App::new();
    let mut keys = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(100));

    loop {
        terminal.draw(|frame| ui::draw(frame, &mut app))?;

        let mut effects = Vec::new();
        tokio::select! {
            Some(event) = keys.next() => {
                if let Event::Key(key) = event? {
                    effects.extend(app.on_key(key));
                }
            }
            Some(event) = events.recv() => {
                app.on_event(event);
                // Drain the burst so a fast stream costs one redraw, not one per chunk.
                while let Ok(event) = events.try_recv() {
                    app.on_event(event);
                }
            }
            _ = tick.tick() => app.tick(),
        }

        for effect in effects {
            let outcome = match effect {
                Effect::Quit => return Ok(()),
                Effect::SendPrompt(text) => session.send_prompt(text),
                Effect::Cancel => session.cancel(),
                Effect::SetMode(mode) => session.set_mode(mode),
                Effect::SetConfig { id, value } => session.set_config_option(id, value),
            };
            if let Err(error) = outcome {
                app.messages
                    .push(Message::Error(format!("session unavailable: {error}")));
            }
        }
    }
}
