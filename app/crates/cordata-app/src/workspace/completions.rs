//! Ghost-text completions in the editor from a local Ollama model (Copilot's inline suggestions, Zed's edit
//! prediction): after a pause in typing, the code before and after the cursor goes to the model's fill-in-the-middle
//! endpoint and the answer shows as grey text; Tab accepts it, typing on dismisses it.
//!
//! Local only: the request goes to Ollama on a loopback address over a plain socket — code never leaves the Mac, and
//! no network client is added to the app. Off until a model is chosen (Settings → Models → Inline completions).

use std::io::{Read as _, Write as _};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use gpui::{App, Task, Window};
use gpui_component::input::{CompletionProvider, Rope};
use lsp_types::{CompletionContext, CompletionResponse, InlineCompletionContext, InlineCompletionItem, InlineCompletionResponse};

/// Code before the cursor sent to the model, and after it.
const PREFIX_BYTES: usize = 6_000;
const SUFFIX_BYTES: usize = 2_000;
/// The longest suggestion asked for, in tokens.
const MAX_TOKENS: u32 = 96;
const TIMEOUT: Duration = Duration::from_secs(6);

/// Where Ollama listens, from `OLLAMA_HOST` when it names a loopback address, else `127.0.0.1:11434`. A remote host
/// is never used: completions would carry the file to it.
pub fn ollama_addr() -> SocketAddr {
    let fallback: SocketAddr = ([127, 0, 0, 1], 11434).into();
    let Ok(raw) = std::env::var("OLLAMA_HOST") else { return fallback };
    let host = raw.trim().trim_start_matches("http://").trim_end_matches('/');
    let with_port = if host.contains(':') { host.to_owned() } else { format!("{host}:11434") };
    match with_port.parse::<SocketAddr>() {
        Ok(addr) if addr.ip().is_loopback() => addr,
        _ => fallback,
    }
}

/// `text` cut to at most `max` bytes on a character boundary, keeping its end (`keep_end`) or its start.
fn cut(text: &str, max: usize, keep_end: bool) -> &str {
    if text.len() <= max {
        return text;
    }
    if keep_end {
        let mut start = text.len() - max;
        while !text.is_char_boundary(start) {
            start += 1;
        }
        &text[start..]
    } else {
        let mut end = max;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        &text[..end]
    }
}

/// A plain HTTP/1.1 exchange with Ollama (Content-Length or chunked bodies).
fn post_json(addr: SocketAddr, path: &str, body: &serde_json::Value) -> Result<serde_json::Value, String> {
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_millis(500)).map_err(|e| format!("Ollama is not running ({e})"))?;
    stream.set_read_timeout(Some(TIMEOUT)).ok();
    stream.set_write_timeout(Some(TIMEOUT)).ok();
    let body = body.to_string();
    let head = format!("POST {path} HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
    stream.write_all(head.as_bytes()).and_then(|_| stream.write_all(body.as_bytes())).map_err(|e| e.to_string())?;
    let mut raw = Vec::new();
    stream.take(4 * 1024 * 1024).read_to_end(&mut raw).map_err(|e| e.to_string())?;
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n").ok_or("a malformed answer")?;
    let (head, rest) = (String::from_utf8_lossy(&raw[..split]).to_ascii_lowercase(), &raw[split + 4..]);
    if !head.starts_with("http/1.1 200") && !head.starts_with("http/1.0 200") {
        return Err(head.lines().next().unwrap_or("an error").to_owned());
    }
    let body = if head.contains("transfer-encoding: chunked") { dechunk(rest) } else { rest.to_vec() };
    serde_json::from_slice(&body).map_err(|e| e.to_string())
}

fn dechunk(mut data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    while let Some(line_end) = data.windows(2).position(|w| w == b"\r\n") {
        let size = usize::from_str_radix(String::from_utf8_lossy(&data[..line_end]).trim(), 16).unwrap_or(0);
        if size == 0 {
            break;
        }
        let start = line_end + 2;
        let end = (start + size).min(data.len());
        out.extend_from_slice(&data[start..end]);
        data = &data[(end + 2).min(data.len())..];
    }
    out
}

/// The suggestion for `prefix`│`suffix`, or None. The reply is trimmed to what reads as one continuation: it stops at
/// a blank line and never repeats the start of the suffix.
pub fn suggest(addr: SocketAddr, model: &str, prefix: &str, suffix: &str) -> Result<Option<String>, String> {
    let body = serde_json::json!({
        "model": model,
        "prompt": prefix,
        "suffix": suffix,
        "stream": false,
        "options": { "num_predict": MAX_TOKENS, "temperature": 0.1, "stop": ["\n\n\n"] },
    });
    let value = post_json(addr, "/api/generate", &body)?;
    let text = value.get("response").and_then(|r| r.as_str()).unwrap_or_default();
    Ok(trim_suggestion(text, suffix))
}

pub fn trim_suggestion(text: &str, suffix: &str) -> Option<String> {
    let mut text = text.split("\n\n").next().unwrap_or_default().trim_end().to_owned();
    // Stop where the suggestion starts a line the code after the cursor already has.
    if let Some(first) = suffix.trim_start().lines().next().map(str::trim).filter(|l| !l.is_empty())
        && let Some(at) = text.lines().scan(0usize, |pos, line| {
            let here = *pos;
            *pos += line.len() + 1;
            Some((here, line))
        }).skip(1).find(|(_, line)| line.trim() == first).map(|(at, _)| at)
    {
        text.truncate(at);
        text = text.trim_end().to_owned();
    }
    (!text.trim().is_empty()).then_some(text)
}

/// The editor's completion provider: no menu completions (the language server's), ghost text from Ollama.
pub struct OllamaInline {
    pub model: String,
}

impl CompletionProvider for OllamaInline {
    fn completions(&self, _: &Rope, _: usize, _: CompletionContext, _: &mut Window, _: &mut App) -> Task<anyhow::Result<CompletionResponse>> {
        Task::ready(Ok(CompletionResponse::Array(vec![])))
    }

    fn inline_completion(&self, rope: &Rope, offset: usize, _: InlineCompletionContext, _: &mut Window, cx: &mut App) -> Task<anyhow::Result<InlineCompletionResponse>> {
        let text = rope.to_string();
        let offset = offset.min(text.len());
        if !text.is_char_boundary(offset) {
            return Task::ready(Ok(InlineCompletionResponse::Array(vec![])));
        }
        let prefix = cut(&text[..offset], PREFIX_BYTES, true).to_owned();
        let suffix = cut(&text[offset..], SUFFIX_BYTES, false).to_owned();
        // Nothing to go on, or the cursor mid-word: no suggestion (it would fight the typing).
        if prefix.trim().is_empty() || suffix.chars().next().is_some_and(|c| c.is_alphanumeric() || c == '_') {
            return Task::ready(Ok(InlineCompletionResponse::Array(vec![])));
        }
        let model = self.model.clone();
        cx.background_executor().spawn(async move {
            let items = match suggest(ollama_addr(), &model, &prefix, &suffix) {
                Ok(Some(text)) => vec![InlineCompletionItem { insert_text: text, filter_text: None, range: None, command: None, insert_text_format: None }],
                _ => vec![],
            };
            Ok(InlineCompletionResponse::Array(items))
        })
    }

    fn inline_completion_debounce(&self) -> Duration {
        Duration::from_millis(450)
    }

    fn is_completion_trigger(&self, _: usize, _: &str, _: &mut App) -> bool {
        false
    }
}

/// The installed Ollama models (`/api/tags`), for Settings. Empty when Ollama is not running.
pub fn installed_models() -> Vec<String> {
    let Ok(mut stream) = TcpStream::connect_timeout(&ollama_addr(), Duration::from_millis(400)) else { return Vec::new() };
    stream.set_read_timeout(Some(Duration::from_secs(3))).ok();
    let head = format!("GET /api/tags HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n", ollama_addr());
    if stream.write_all(head.as_bytes()).is_err() {
        return Vec::new();
    }
    let mut raw = Vec::new();
    let _ = stream.take(2 * 1024 * 1024).read_to_end(&mut raw);
    let Some(split) = raw.windows(4).position(|w| w == b"\r\n\r\n") else { return Vec::new() };
    let head = String::from_utf8_lossy(&raw[..split]).to_ascii_lowercase();
    let body = if head.contains("transfer-encoding: chunked") { dechunk(&raw[split + 4..]) } else { raw[split + 4..].to_vec() };
    serde_json::from_slice::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| v.get("models").and_then(|m| m.as_array()).map(|a| a.iter().filter_map(|m| m.get("name").and_then(|n| n.as_str()).map(str::to_owned)).collect()))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn only_loopback_hosts_are_used() {
        // SAFETY: test-only env change before any reader in this test.
        unsafe { std::env::set_var("OLLAMA_HOST", "10.0.0.5:11434") };
        assert!(ollama_addr().ip().is_loopback(), "a remote OLLAMA_HOST is ignored");
        unsafe { std::env::set_var("OLLAMA_HOST", "127.0.0.1:9999") };
        assert_eq!(ollama_addr().port(), 9999);
        unsafe { std::env::remove_var("OLLAMA_HOST") };
    }

    #[test]
    fn suggestions_are_trimmed_to_one_continuation() {
        assert_eq!(trim_suggestion("a + b\n\nfn other() {}", ""), Some("a + b".into()));
        assert_eq!(trim_suggestion("x = 1;\n}", "}\n"), Some("x = 1;".into()), "stops before a line the suffix already has");
        assert_eq!(trim_suggestion("   ", ""), None);
        assert_eq!(cut("héllo", 3, true), "llo");
        assert_eq!(cut("héllo", 2, false), "h");
    }

    #[test]
    fn a_fill_in_the_middle_exchange_with_a_fake_ollama() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            // Read the whole request: headers, then Content-Length bytes of body.
            let mut raw = Vec::new();
            let mut buf = [0u8; 8192];
            loop {
                let n = socket.read(&mut buf).unwrap();
                raw.extend_from_slice(&buf[..n]);
                if let Some(split) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&raw[..split]).to_ascii_lowercase();
                    let len = head.lines().find_map(|l| l.strip_prefix("content-length:")).and_then(|v| v.trim().parse::<usize>().ok()).unwrap_or(0);
                    if raw.len() >= split + 4 + len || n == 0 {
                        break;
                    }
                }
            }
            let request = String::from_utf8_lossy(&raw).into_owned();
            let body = r#"{"response":"a + b\n"}"#;
            let chunked = format!("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n{body}\r\n0\r\n\r\n", body.len());
            socket.write_all(chunked.as_bytes()).unwrap();
            request
        });
        let got = suggest(addr, "qwen2.5-coder", "fn add(a: i32, b: i32) -> i32 {\n    ", "\n}").unwrap();
        let request = server.join().unwrap();
        assert_eq!(got.as_deref(), Some("a + b"));
        assert!(request.contains("\"suffix\":\"\\n}\"") && request.starts_with("POST /api/generate"), "fill-in-the-middle with the suffix");
    }
}
