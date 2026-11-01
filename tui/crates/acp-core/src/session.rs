//! `SessionHandle`: the one-process, one-session unit both binaries drive.
//!
//! One handle == one native session (`crate::native`).

use std::path::PathBuf;

use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::error::{Error, Result};
use crate::events::AppEvent;
use crate::transport::BackendTransport;

#[derive(Debug, Clone)]
pub struct SessionOptions {
    /// Project directory the agent works in (sent as the session's `cwd`).
    pub cwd: PathBuf,
    /// How to reach the agent.
    pub transport: BackendTransport,
    /// Append diagnostics here. Never stdout/stdin — the TUI owns those.
    pub debug_log: Option<PathBuf>,
}

/// Something sent with a prompt: an image (a pasted screenshot, a dropped picture) or a file's text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Attachment {
    /// Base64-encoded image bytes and their MIME type.
    Image { mime: String, data_base64: String },
    /// A text file, embedded as an ACP resource (`file://` URI).
    Text { uri: String, text: String },
}

/// What the front end asks of a session. The native engine answers mode, config and extension requests with a notice;
/// their payloads are kept so a front end can still send them.
#[allow(dead_code)]
#[derive(Debug)]
pub(crate) enum Command {
    Prompt(String, Vec<Attachment>),
    Cancel,
    SetMode(String),
    SetConfigOption {
        id: String,
        value: String,
    },
    /// An extension request (`_cordata/...`, or a standard method the typed
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
    /// Starts the session task. Setup progress and
    /// failures arrive on the returned receiver (`Connected`, or
    /// `ConnectionError` + `Closed`).
    pub fn spawn(options: SessionOptions) -> (Self, mpsc::UnboundedReceiver<AppEvent>) {
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(crate::native::run(options, cmd_rx, event_tx));
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
        self.send(Command::Prompt(text.into(), Vec::new()))
    }

    /// A prompt with images and files: the text first, then one content block per attachment.
    pub fn send_prompt_with(&self, text: impl Into<String>, attachments: Vec<Attachment>) -> Result<()> {
        self.send(Command::Prompt(text.into(), attachments))
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

    /// Stops the session and waits until its task has ended.
    pub async fn shutdown(mut self) {
        let _ = self.commands.send(Command::Shutdown);
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

impl Drop for SessionHandle {
    fn drop(&mut self) {
        // Best effort when the owner didn't call shutdown(): the task exits.
        let _ = self.commands.send(Command::Shutdown);
    }
}
