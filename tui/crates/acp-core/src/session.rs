//! `SessionHandle`: the one-process, one-session unit both binaries drive.
//!
//! One handle == one native session (`crate::native`).

use std::path::PathBuf;

use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::error::{Error, Result};
use crate::events::AppEvent;
use crate::policy::Mode;
use crate::transport::BackendTransport;

#[derive(Debug, Clone)]
pub struct SessionOptions {
    /// Project directory the agent works in (sent as the session's `cwd`).
    pub cwd: PathBuf,
    /// How to reach the agent.
    pub transport: BackendTransport,
    /// Append diagnostics here. Never stdout/stdin — the TUI owns those.
    pub debug_log: Option<PathBuf>,
    /// The model to start with; `None` follows `CORDATA_NATIVE_MODEL`, then whichever provider key exists.
    pub model: Option<String>,
}

/// Something sent with a prompt: an image (a pasted screenshot, a dropped picture) or a file's text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Attachment {
    /// Base64-encoded image bytes and their MIME type.
    Image { mime: String, data_base64: String },
    /// A text file, with its `file://` URI.
    Text { uri: String, text: String },
}

/// What the front end asks of a session.
#[derive(Debug)]
pub(crate) enum Command {
    Prompt(String, Vec<Attachment>),
    /// Load a saved session (`last`, an id or a prefix) in place of this one's conversation.
    Resume(String),
    /// Delete this project's saved sessions and stop saving this one.
    Forget,
    /// Answer with this model from the next request on (between turns).
    SetModel(String),
    /// Who answers approvals from now on: the person (`Ask`) or `auto::judge` first (`Auto`).
    SetMode(Mode),
    Cancel,
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

    /// Continues a saved session: `last`, an id, or a unique prefix of one (`acp_core::history::list`). Between turns
    /// only; the result is `SessionRestored` or a `Notice`.
    pub fn resume(&self, wanted: impl Into<String>) -> Result<()> {
        self.send(Command::Resume(wanted.into()))
    }

    /// Deletes every saved session of this project, and keeps this one from being saved. Answered with a `Notice`.
    pub fn forget(&self) -> Result<()> {
        self.send(Command::Forget)
    }

    /// Switches the model (a genai name, as `CORDATA_NATIVE_MODEL` takes) between turns, keeping the conversation.
    /// Answered with `ModelChanged`, or a `Notice` saying why not.
    pub fn set_model(&self, model: impl Into<String>) -> Result<()> {
        self.send(Command::SetModel(model.into()))
    }

    /// Switches who answers approvals (`ask` or `auto`) for the rest of this session. Answered with `ModeChanged`.
    pub fn set_mode(&self, mode: Mode) -> Result<()> {
        self.send(Command::SetMode(mode))
    }

    /// Stops a running turn, between streamed events. Inert while an approval is unanswered: answer it first.
    pub fn cancel(&self) -> Result<()> {
        self.send(Command::Cancel)
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
