//! `SessionHandle`: the one-process, one-session unit both binaries drive.
//!
//! One handle == one spawned `acp_server.py` == one session. That is a hard
//! requirement, not a convenience: the backend's `_build_session` does a
//! process-wide `os.chdir(cwd)`, so two sessions sharing one server process
//! could have their working directory changed under each other.

use std::path::PathBuf;

use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::config::BackendConfig;
use crate::connection;
use crate::error::{Error, Result};
use crate::events::AppEvent;
use crate::transport::BackendTransport;

#[derive(Debug, Clone)]
pub struct SessionOptions {
    pub backend: BackendConfig,
    /// Project directory the agent works in (sent as the session's `cwd`).
    pub cwd: PathBuf,
    /// How to reach the agent. `Stdio` spawns `backend/acp_server.py` here;
    /// `Ws` attaches to the always-on daemon instead, which is what lets a
    /// session outlive the terminal that started it.
    pub transport: BackendTransport,
    /// Resume an existing session id via `session/load` instead of creating one.
    pub resume: Option<String>,
    /// Append raw JSON-RPC lines + server stderr here. Never stdout/stdin —
    /// the TUI owns those. With `--transport ws` this records only what the
    /// client sent and received; the child's stderr stays on the daemon's.
    pub debug_log: Option<PathBuf>,
    /// With no `resume`, ask the backend (`session/list`) for the newest session
    /// in `cwd` and load that one; a fresh session when there is none.
    pub continue_latest: bool,
    /// The hub conversation this session joined, once resolved (set by the connection, not callers).
    pub hub_handle: Option<String>,
}

#[derive(Debug)]
pub(crate) enum Command {
    Prompt(String),
    Cancel,
    SetMode(String),
    SetConfigOption {
        id: String,
        value: String,
    },
    /// An extension request (`_sierralab/...`, or a standard method the typed
    /// API doesn't cover); the answer arrives as `AppEvent::ExtResult { tag }`.
    Ext {
        method: String,
        params: serde_json::Value,
        tag: String,
    },
    Shutdown,
}

pub struct SessionHandle {
    commands: mpsc::UnboundedSender<Command>,
    task: Option<JoinHandle<()>>,
}

impl SessionHandle {
    /// Spawns the backend and starts the session task. Setup progress and
    /// failures arrive on the returned receiver (`Connected`, or
    /// `ConnectionError` + `Closed`).
    pub fn spawn(options: SessionOptions) -> (Self, mpsc::UnboundedReceiver<AppEvent>) {
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(connection::run(options, cmd_rx, event_tx));
        (
            Self {
                commands: cmd_tx,
                task: Some(task),
            },
            event_rx,
        )
    }

    fn send(&self, command: Command) -> Result<()> {
        self.commands
            .send(command)
            .map_err(|_| Error::SessionClosed)
    }

    pub fn send_prompt(&self, text: impl Into<String>) -> Result<()> {
        self.send(Command::Prompt(text.into()))
    }

    /// `session/cancel`. Only stops a running turn, and only between streamed
    /// events — it is inert while the server is blocked on an unanswered
    /// permission request (answer the request first).
    pub fn cancel(&self) -> Result<()> {
        self.send(Command::Cancel)
    }

    pub fn set_mode(&self, mode_id: impl Into<String>) -> Result<()> {
        self.send(Command::SetMode(mode_id.into()))
    }

    pub fn set_config_option(&self, id: impl Into<String>, value: impl Into<String>) -> Result<()> {
        self.send(Command::SetConfigOption {
            id: id.into(),
            value: value.into(),
        })
    }

    /// Send an extension request; the reply comes back as `AppEvent::ExtResult`
    /// carrying `tag`, so the caller knows which request it answers.
    pub fn ext(
        &self,
        method: impl Into<String>,
        params: serde_json::Value,
        tag: impl Into<String>,
    ) -> Result<()> {
        self.send(Command::Ext {
            method: method.into(),
            params,
            tag: tag.into(),
        })
    }

    /// Stops the session and waits until the backend process is gone.
    pub async fn shutdown(mut self) {
        let _ = self.commands.send(Command::Shutdown);
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

impl Drop for SessionHandle {
    fn drop(&mut self) {
        // Best effort when the owner didn't call shutdown(): the task exits,
        // dropping the connection, whose guard kills the backend's process group.
        let _ = self.commands.send(Command::Shutdown);
    }
}
