//! How the client reaches an agent: a spawned child over stdio, or the
//! always-on daemon over a WebSocket.
//!
//! Both are ACP, byte for byte. `backend/daemon/frames.py` proxies each frame
//! verbatim rather than parsing and rebuilding it, precisely so that everything
//! above this module — `connection.rs`, `events.rs`, the whole UI — cannot tell
//! which transport is in use. This file exists to keep that promise mechanical:
//! it produces a `ConnectTo<Client>` and hands it to the same
//! `Client.builder().connect_with(..)` call either way.
//!
//! The one asymmetry worth knowing about: stdio is a *stream* of newline-
//! delimited JSON, while a WebSocket is already *framed* — one text message per
//! ACP line. `Lines` handles both, because the JSON-RPC layer downstream reads
//! one message per stream item and does not care whether that item arrived
//! with a trailing newline.

use std::path::Path;

use agent_client_protocol::{ConnectTo, Lines, Role};
use futures::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;

use crate::config::DaemonConfig;
use crate::error::Result;

/// Where to find the agent. Chosen once per session, at construction.
#[derive(Debug, Clone)]
pub enum BackendTransport {
    /// Spawn `backend/acp_server.py` and speak over its stdio. The default, and
    /// the only option that works without the daemon running.
    Stdio,
    /// Attach to a session the daemon already owns, or ask it to start one in
    /// this directory. `url` carries the token and the cwd (see
    /// [`DaemonConfig::socket_url`]).
    Ws { url: String },
}

impl BackendTransport {
    /// The daemon transport for `cwd`, reading the token and port from the
    /// daemon's own config file.
    pub fn daemon(cwd: &Path) -> Result<Self> {
        let daemon = DaemonConfig::discover()?;
        Ok(Self::Ws {
            url: daemon.socket_url(cwd),
        })
    }

    /// A short label for the connect banner and error messages.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Stdio => "stdio",
            Self::Ws { .. } => "ws",
        }
    }
}

/// The WebSocket transport itself.
///
/// Kept separate from [`BackendTransport`] because `ConnectTo` is consumed by
/// value, and the enum is what the UI clones and prints.
#[derive(Debug)]
pub struct WsTransport {
    url: String,
}

impl WsTransport {
    pub fn new(url: impl Into<String>) -> Self {
        Self { url: url.into() }
    }
}

impl<Counterpart: Role> ConnectTo<Counterpart> for WsTransport {
    async fn connect_to(
        self,
        client: impl ConnectTo<Counterpart::Counterpart>,
    ) -> Result<(), agent_client_protocol::Error> {
        let (socket, _response) =
            tokio_tungstenite::connect_async(&self.url)
                .await
                .map_err(|error| {
                    agent_client_protocol::util::internal_error(describe_connect_error(
                        &self.url, &error,
                    ))
                })?;
        let (sink, stream) = socket.split();

        // One ACP line per text message, in both directions. The error type is
        // `io::Error` rather than tungstenite's so `Lines` accepts it — the
        // same narrowing `Stdio` does, and the reason a dropped socket surfaces
        // as an ordinary I/O failure rather than a transport-specific one.
        let outgoing = futures::sink::unfold(sink, async |mut sink, line: String| {
            sink.send(Message::text(line))
                .await
                .map_err(|error| std::io::Error::other(error.to_string()))?;
            Ok::<_, std::io::Error>(sink)
        });

        let incoming = stream
            // A close frame ends the stream. Left in, it would be filtered out
            // below and the reader would wait forever on a socket that is gone.
            .take_while(|item| futures::future::ready(!matches!(item, Ok(Message::Close(_)))))
            .filter_map(|item| async move {
                match item {
                    Ok(Message::Text(text)) => Some(Ok(text.to_string())),
                    // Binary is not ACP. Ping/Pong are handled by tungstenite
                    // itself on the read side, and never reach here carrying
                    // protocol content.
                    Ok(_) => None,
                    Err(error) => Some(Err(std::io::Error::other(error.to_string()))),
                }
            });

        ConnectTo::<Counterpart>::connect_to(Lines::new(outgoing, incoming), client).await
    }
}

/// Tungstenite's own Display for a handshake failure says only "HTTP error",
/// which is useless when what actually happened is that the daemon is not
/// running. Say what to check instead.
fn describe_connect_error(url: &str, error: &tokio_tungstenite::tungstenite::Error) -> String {
    let detail = error.to_string();
    if matches!(error, tokio_tungstenite::tungstenite::Error::Http(_)) {
        return format!(
            "{url} refused the connection — the daemon may not be running \
             (make daemon), or the token in ~/.sierralab/daemon.json is stale"
        );
    }
    format!("{url}: {detail}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_tungstenite::tungstenite::Error as WsError;

    #[test]
    fn a_rejected_handshake_says_what_to_check() {
        // A 403 is what the daemon's own close codes turn into at the
        // handshake level — the case a user hits with a stale token, and the
        // one where tungstenite's own message is just "HTTP error".
        let response = tokio_tungstenite::tungstenite::http::Response::builder()
            .status(403)
            .body(None)
            .expect("a well-formed response");
        let text =
            describe_connect_error("ws://127.0.0.1:8765/ws", &WsError::Http(Box::new(response)));
        assert!(text.contains("make daemon"), "{text}");
        assert!(text.contains("~/.sierralab/daemon.json"), "{text}");
    }

    #[test]
    fn other_failures_keep_the_transport_detail() {
        let refused =
            std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "connection refused");
        let text = describe_connect_error("ws://127.0.0.1:8765/ws", &WsError::Io(refused));
        assert!(text.starts_with("ws://127.0.0.1:8765/ws: "), "{text}");
        assert!(text.contains("connection refused"), "{text}");
    }

    #[test]
    fn the_label_names_the_transport_for_the_banner() {
        assert_eq!(BackendTransport::Stdio.label(), "stdio");
        assert_eq!(
            BackendTransport::Ws {
                url: "ws://x".into()
            }
            .label(),
            "ws"
        );
    }
}
