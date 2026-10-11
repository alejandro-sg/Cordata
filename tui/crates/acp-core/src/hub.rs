//! The daemon's conversation hub, from a terminal.
//!
//! One conversation is shared by every front door — this terminal, Discord, the menu bar, the web
//! app — and the daemon holds it (`backend/daemon/chat.py`). A terminal joins one in two steps:
//! ask the daemon to open it (`POST /conversations`: a handle, `latest` in this folder, or `new`),
//! which starts its agent and answers with the ACP session id; then attach over the socket by handle
//! and `session/load` that id, which replays the history. From then on, turns from any door stream
//! into this terminal, and this terminal's turns stream into theirs.
//!
//! The HTTP here is a few lines of HTTP/1.1 over a loopback socket rather than a client crate: two
//! JSON calls to `127.0.0.1`, with the token in a header (never the URL, which a debug log prints).

use std::path::Path;
use std::time::Duration;

use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::config::DaemonConfig;
use crate::error::{Error, Result};

/// Opening a conversation may start its agent, which imports the whole backend: generous, like the
/// daemon's own handshake timeout.
const OPEN_TIMEOUT: Duration = Duration::from_secs(150);
const LIST_TIMEOUT: Duration = Duration::from_secs(10);

/// One conversation as the daemon lists it.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Conversation {
    pub handle: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub cwd: String,
    /// The door that opened it: `terminal`, `discord`, `menubar`, `web`.
    #[serde(default)]
    pub origin: String,
    #[serde(default)]
    pub last_used: f64,
    #[serde(default)]
    pub turns: u64,
    #[serde(default)]
    pub live: bool,
    #[serde(default)]
    pub busy: bool,
    /// Whose turn is running, when one is.
    #[serde(default)]
    pub turn_origin: String,
    #[serde(default)]
    pub acp_session_id: String,
}

impl Conversation {
    /// One line for `/conversations`: handle, title, size, where it came from, and whether it is busy.
    pub fn summary(&self, now: f64) -> String {
        let title = if self.title.is_empty() { "(untitled)" } else { self.title.as_str() };
        let age = describe_age(now - self.last_used);
        let busy = if self.busy { format!(" · running a turn from {}", door(&self.turn_origin)) } else { String::new() };
        format!("{}  {title} — {} turn{}, {age}, opened from {}{busy}", self.handle, self.turns, if self.turns == 1 { "" } else { "s" }, door(&self.origin))
    }
}

/// A door's name as a person reads it.
pub fn door(origin: &str) -> &str {
    match origin {
        "discord" => "Discord",
        "terminal" => "a terminal",
        "menubar" => "the menu bar",
        "web" => "the web app",
        "" => "somewhere",
        other => other,
    }
}

fn describe_age(seconds: f64) -> String {
    let seconds = seconds.max(0.0) as u64;
    match seconds {
        0..=59 => "just now".into(),
        60..=3599 => format!("{}m ago", seconds / 60),
        3600..=86_399 => format!("{}h ago", seconds / 3600),
        _ => format!("{}d ago", seconds / 86_400),
    }
}

/// Open (or resolve) a conversation and make sure its agent is up. `reference` is a handle,
/// `latest` (this folder's newest, else a new one) or `new`.
pub async fn open(config: &DaemonConfig, reference: &str, cwd: &Path) -> Result<Conversation> {
    let body = serde_json::json!({ "conversation": reference, "cwd": cwd.display().to_string(), "origin": "terminal" });
    let value = request(config, "POST", "/conversations", Some(&body), OPEN_TIMEOUT).await?;
    serde_json::from_value(value).map_err(|error| Error::DaemonUnreachable(format!("the daemon's answer was not a conversation: {error}")))
}

/// Conversations, newest first — in `cwd` when given.
pub async fn list(config: &DaemonConfig, cwd: Option<&Path>) -> Result<Vec<Conversation>> {
    let path = match cwd {
        Some(cwd) => format!("/conversations?cwd={}", crate::config::percent_encode(&cwd.display().to_string())),
        None => "/conversations".to_owned(),
    };
    let value = request(config, "GET", &path, None, LIST_TIMEOUT).await?;
    let rows = value.get("conversations").cloned().unwrap_or_default();
    serde_json::from_value(rows).map_err(|error| Error::DaemonUnreachable(format!("the daemon's list was not understood: {error}")))
}

/// Is a daemon answering on its port? A connect, not a request: what `--transport auto` asks before
/// it decides between the hub and a terminal-only backend.
pub async fn reachable(config: &DaemonConfig) -> bool {
    matches!(tokio::time::timeout(Duration::from_millis(400), TcpStream::connect(("127.0.0.1", config.port))).await, Ok(Ok(_)))
}

async fn request(config: &DaemonConfig, method: &str, path: &str, body: Option<&serde_json::Value>, timeout: Duration) -> Result<serde_json::Value> {
    let exchange = async {
        let mut stream = TcpStream::connect(("127.0.0.1", config.port))
            .await
            .map_err(|error| Error::DaemonUnreachable(format!("cannot reach the daemon on port {}: {error}", config.port)))?;
        let body = body.map(|value| value.to_string()).unwrap_or_default();
        let head = format!(
            "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nX-Daemon-Token: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            config.token,
            body.len()
        );
        stream.write_all(head.as_bytes()).await?;
        stream.write_all(body.as_bytes()).await?;
        let mut raw = Vec::new();
        stream.read_to_end(&mut raw).await?;
        parse_response(&raw)
    };
    tokio::time::timeout(timeout, exchange)
        .await
        .map_err(|_| Error::DaemonUnreachable(format!("the daemon did not answer {method} {path} in time")))?
}

/// Status line, headers, body — `Content-Length` or chunked, which is all uvicorn sends.
fn parse_response(raw: &[u8]) -> Result<serde_json::Value> {
    let split = raw.windows(4).position(|window| window == b"\r\n\r\n").ok_or_else(|| Error::DaemonUnreachable("the daemon closed the connection mid-answer".into()))?;
    let head = String::from_utf8_lossy(&raw[..split]).to_string();
    let mut body = raw[split + 4..].to_vec();
    let status: u16 = head.split_whitespace().nth(1).and_then(|code| code.parse().ok()).unwrap_or(0);
    if head.to_ascii_lowercase().contains("transfer-encoding: chunked") {
        body = dechunk(&body);
    }
    let value: serde_json::Value = serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
    if status != 200 {
        let detail = value.get("detail").and_then(|detail| detail.as_str()).map(str::to_owned).unwrap_or_else(|| format!("HTTP {status}"));
        return Err(Error::DaemonUnreachable(detail));
    }
    Ok(value)
}

fn dechunk(mut body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    while let Some(line_end) = body.windows(2).position(|window| window == b"\r\n") {
        let size = usize::from_str_radix(String::from_utf8_lossy(&body[..line_end]).trim(), 16).unwrap_or(0);
        if size == 0 {
            break;
        }
        let start = line_end + 2;
        let end = (start + size).min(body.len());
        out.extend_from_slice(&body[start..end]);
        body = &body[(end + 2).min(body.len())..];
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_answer_is_read_whether_sized_or_chunked() {
        let sized = b"HTTP/1.1 200 OK\r\ncontent-length: 17\r\n\r\n{\"handle\":\"c1\"}  ";
        assert_eq!(parse_response(sized).unwrap()["handle"], "c1");
        let chunked = b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n6\r\n{\"a\":1\r\n1\r\n}\r\n0\r\n\r\n";
        assert_eq!(parse_response(chunked).unwrap()["a"], 1);
    }

    #[test]
    fn a_refusal_carries_the_daemons_sentence() {
        let refused = b"HTTP/1.1 400 Bad Request\r\ncontent-length: 41\r\n\r\n{\"detail\":\"no conversation named 'cx'\"}  ";
        let error = parse_response(refused).unwrap_err().to_string();
        assert!(error.contains("no conversation named"), "{error}");
    }

    #[test]
    fn a_row_reads_as_a_line_a_person_can_act_on() {
        let row = Conversation {
            handle: "c3f9a1e".into(),
            title: "fix the flaky test".into(),
            cwd: "/p".into(),
            origin: "discord".into(),
            last_used: 1000.0,
            turns: 3,
            live: true,
            busy: true,
            turn_origin: "menubar".into(),
            acp_session_id: "s".into(),
        };
        let line = row.summary(1000.0 + 7200.0);
        assert!(line.starts_with("c3f9a1e  fix the flaky test"), "{line}");
        assert!(line.contains("3 turns, 2h ago, opened from Discord · running a turn from the menu bar"), "{line}");
    }
}
