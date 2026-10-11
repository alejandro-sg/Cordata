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

use std::fmt;
use std::path::Path;

use agent_client_protocol::{ConnectTo, Lines, Role};
use futures::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;

use crate::config::DaemonConfig;
use crate::error::Result;

/// Where to find the agent. Chosen once per session, at construction.
///
/// `Debug` is written by hand: the `Ws` URL carries the daemon's token as a
/// query parameter (a WebSocket handshake has no header to put it in), and a
/// derived `Debug` would print it into any log line or panic message that
/// formats a `SessionOptions`.
#[derive(Clone)]
pub enum BackendTransport {
    /// Spawn `backend/acp_server.py` and speak over its stdio. The default, and
    /// the only option that works without the daemon running.
    Stdio,
    /// Attach to a session the daemon already owns, or ask it to start one in
    /// this directory. `url` carries the token and the cwd (see
    /// [`DaemonConfig::socket_url`]).
    Ws { url: String },
    /// Join a conversation the daemon holds — shared with Discord, the menu bar and the web app.
    /// `conversation` is a handle, `latest` (this folder's newest) or `new`; it is resolved when the
    /// session starts ([`crate::hub::open`]), then attached by handle over the same socket.
    Hub { daemon: DaemonConfig, conversation: String },
}

impl fmt::Debug for BackendTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Stdio => f.write_str("Stdio"),
            Self::Ws { url } => f
                .debug_struct("Ws")
                .field("url", &redact_token(url))
                .finish(),
            Self::Hub { conversation, .. } => f.debug_struct("Hub").field("conversation", conversation).finish_non_exhaustive(),
        }
    }
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
            Self::Hub { .. } => "hub",
        }
    }
}

/// The WebSocket transport itself.
///
/// Kept separate from [`BackendTransport`] because `ConnectTo` is consumed by
/// value, and the enum is what the UI clones and prints.
pub struct WsTransport {
    url: String,
}

impl fmt::Debug for WsTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WsTransport")
            .field("url", &redact_token(&self.url))
            .finish()
    }
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

        // A close frame ends the stream — left in, it would be filtered out and
        // the reader would wait forever on a socket that is gone. A close the
        // daemon *meant* (the session exited, the token was refused) is surfaced
        // as an error first, so the user is told why instead of seeing the
        // connection simply stop.
        let incoming = stream
            .scan(false, |closed, item| {
                let next = if *closed {
                    None
                } else {
                    match item {
                        Ok(Message::Text(text)) => Some(Some(Ok(text.to_string()))),
                        Ok(Message::Close(frame)) => {
                            *closed = true;
                            frame
                                .as_ref()
                                .and_then(close_error)
                                .map(|error| Some(Err(error)))
                        }
                        // Binary is not ACP. Ping/Pong are handled by
                        // tungstenite itself on the read side, and never reach
                        // here carrying protocol content.
                        Ok(_) => Some(None),
                        Err(error) => Some(Some(Err(std::io::Error::other(error.to_string())))),
                    }
                };
                futures::future::ready(next)
            })
            .filter_map(futures::future::ready);

        ConnectTo::<Counterpart>::connect_to(Lines::new(outgoing, incoming), client).await
    }
}

/// The URL with its token masked. The token is a query parameter (a browser
/// WebSocket cannot set headers, and this client mirrors that), so any message
/// that quotes the URL — a connect error, a log line — would otherwise put the
/// daemon's credential into scrollback, a pasted bug report or a screenshot.
fn redact_token(url: &str) -> String {
    let Some((head, query)) = url.split_once('?') else {
        return url.to_owned();
    };
    let query = query
        .split('&')
        .map(|pair| match pair.split_once('=') {
            Some(("token", _)) => "token=REDACTED".to_owned(),
            _ => pair.to_owned(),
        })
        .collect::<Vec<_>>()
        .join("&");
    format!("{head}?{query}")
}

/// An error for a close frame the daemon sent on purpose, or `None` for an
/// ordinary goodbye. Its codes are the daemon's own (`backend/daemon/server.py`):
/// 4401 bad token, 4403 browser origin, 4404 no such session, 4409 already
/// attached, 4410 session exited.
fn close_error(frame: &CloseFrame) -> Option<std::io::Error> {
    match frame.code {
        CloseCode::Normal | CloseCode::Away => None,
        code => {
            let reason = frame.reason.to_string();
            Some(std::io::Error::other(format!(
                "the daemon closed the connection ({}){}",
                u16::from(code),
                if reason.is_empty() {
                    String::new()
                } else {
                    format!(": {reason}")
                }
            )))
        }
    }
}

/// Tungstenite's own Display for a handshake failure says only "HTTP error",
/// which is useless when what actually happened is that the daemon is not
/// running. Say what to check instead.
///
/// The daemon refuses a socket *before* accepting it, which a client sees as a
/// bare HTTP 403 whatever the reason, so all the causes are named: a stale
/// token is only one, and "another client is already attached to this
/// directory's session" is the one a second terminal hits.
fn describe_connect_error(url: &str, error: &tokio_tungstenite::tungstenite::Error) -> String {
    let url = redact_token(url);
    let detail = error.to_string();
    if matches!(error, tokio_tungstenite::tungstenite::Error::Http(_)) {
        return format!(
            "{url} refused the connection — the daemon may not be running \
             (make daemon), the token in ~/.sierralab/daemon.json may be stale, \
             or another client is already attached to this directory's session"
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
    fn the_token_never_appears_in_a_message_or_in_debug_output() {
        let url = "ws://127.0.0.1:8765/ws?token=SECRET-abc_123&cwd=%2Ftmp%2Fp";
        let refused =
            std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "connection refused");
        let text = describe_connect_error(url, &WsError::Io(refused));
        assert!(!text.contains("SECRET"), "{text}");
        assert!(
            text.contains("cwd=%2Ftmp%2Fp"),
            "the rest of the URL stays readable: {text}"
        );

        let transport = BackendTransport::Ws { url: url.into() };
        assert!(!format!("{transport:?}").contains("SECRET"));
        assert!(!format!("{:?}", WsTransport::new(url)).contains("SECRET"));
        assert_eq!(
            redact_token("ws://x/ws"),
            "ws://x/ws",
            "no query, nothing to mask"
        );
    }

    #[test]
    fn a_deliberate_close_becomes_an_error_and_a_goodbye_does_not() {
        let frame = |code: CloseCode, reason: &str| CloseFrame {
            code,
            reason: reason.into(),
        };
        let exited = close_error(&frame(CloseCode::Library(4410), "session has exited"))
            .expect("4410 is the daemon telling us the session is gone");
        let text = exited.to_string();
        assert!(
            text.contains("4410") && text.contains("session has exited"),
            "{text}"
        );
        assert!(close_error(&frame(CloseCode::Normal, "")).is_none());
        assert!(close_error(&frame(CloseCode::Away, "going away")).is_none());
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
