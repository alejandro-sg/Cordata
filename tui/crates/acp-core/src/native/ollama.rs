//! Local Ollama models, called on Ollama's own `/api/chat` so each request carries an explicit `options.num_ctx`.
//! Without it Ollama runs a model at its server default context (a few thousand tokens) and silently drops the start
//! of a long conversation. genai's Ollama adapter cannot send the option, so this one request shape is ours, spoken as
//! plain HTTP/1.1 to the local server over tokio (no new dependency). Ollama Cloud (`ollama_cloud::`) still goes
//! through genai.

use futures::StreamExt;
use futures::stream::{self, BoxStream};
use genai::adapter::AdapterKind;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use serde_json::{Value, json};

use super::provider::{Call, Delta, Msg};
use super::tools::ToolSpec;

/// The context window asked for when `chat.ollamaNumCtx` is not set.
pub(crate) const DEFAULT_NUM_CTX: u64 = 65_536;
/// How long to wait for the server to accept, and for any next byte of a reply (a cold model load can be slow).
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
const IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

pub(crate) struct OllamaChat {
    /// `host:port` of the local server.
    addr: String,
    num_ctx: u64,
}

impl OllamaChat {
    /// `Some` for a model genai would send to local Ollama (an unknown or `ollama::` name), `None` for anything else.
    pub(crate) fn for_model(model: &str) -> Option<Self> {
        (AdapterKind::from_model(model).ok()? == AdapterKind::Ollama).then(|| Self::at("127.0.0.1:11434", num_ctx_setting()))
    }

    pub(crate) fn at(addr: &str, num_ctx: u64) -> Self {
        Self { addr: addr.to_owned(), num_ctx }
    }

    pub(crate) fn body(&self, model: &str, system: &str, history: &[Msg], tools: &[ToolSpec]) -> Value {
        let mut messages = vec![json!({"role": "system", "content": system})];
        messages.extend(history.iter().map(|m| match m {
            Msg::User { text } => json!({"role": "user", "content": text}),
            Msg::Assistant { text, calls } => json!({"role": "assistant", "content": text,
                "tool_calls": calls.iter().map(|c| json!({"function": {"name": c.name, "arguments": c.args}})).collect::<Vec<_>>()}),
            Msg::ToolResult { output, .. } => json!({"role": "tool", "content": output}),
        }));
        let tools: Vec<Value> = tools.iter().map(|t| json!({"type": "function", "function": {"name": t.name, "description": t.description, "parameters": t.schema}})).collect();
        let name = model.strip_prefix("ollama::").unwrap_or(model);
        json!({"model": name, "stream": true, "messages": messages, "tools": tools, "options": {"num_ctx": self.num_ctx}})
    }

    pub(crate) async fn stream(&self, body: Value) -> Result<BoxStream<'static, Result<Delta, String>>, String> {
        let unreachable = |e: std::io::Error| format!("ollama at {}: {e} (is `ollama serve` running?)", self.addr);
        let mut sock = match tokio::time::timeout(CONNECT_TIMEOUT, tokio::net::TcpStream::connect(&self.addr)).await {
            Ok(connected) => connected.map_err(unreachable)?,
            Err(_) => return Err(format!("ollama at {}: no answer in {}s", self.addr, CONNECT_TIMEOUT.as_secs())),
        };
        let payload = body.to_string();
        let request = format!("POST /api/chat HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}", self.addr, payload.len());
        sock.write_all(request.as_bytes()).await.map_err(unreachable)?;
        // Read up to the end of the headers; whatever follows is the start of the body.
        let mut raw = Vec::new();
        let head_end = loop {
            if let Some(i) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                break i + 4;
            }
            let mut chunk = [0u8; 8192];
            let read = tokio::time::timeout(IDLE_TIMEOUT, sock.read(&mut chunk)).await.map_err(|_| format!("ollama: no reply in {}s", IDLE_TIMEOUT.as_secs()))?;
            match read.map_err(unreachable)? {
                0 => return Err("ollama: the connection closed before a reply".into()),
                n => raw.extend_from_slice(&chunk[..n]),
            }
        };
        let head = String::from_utf8_lossy(&raw[..head_end]).to_ascii_lowercase();
        let status = head.split_whitespace().nth(1).unwrap_or("").to_owned();
        let chunked = head.contains("transfer-encoding: chunked");
        let state = Reader { sock, buf: raw.split_off(head_end), chunked, chunk_left: 0, text: Vec::new(), calls: 0, eof: false, done: false, failed: None };
        if status != "200" {
            let body: Vec<_> = state.into_stream().collect().await;
            let detail = body.into_iter().find_map(Result::err).unwrap_or_default();
            return Err(format!("ollama: HTTP {status} {detail}"));
        }
        Ok(state.into_stream())
    }
}

/// The reply body as it arrives: de-chunked, split into NDJSON lines, each turned into deltas.
struct Reader {
    sock: tokio::net::TcpStream,
    buf: Vec<u8>,
    chunked: bool,
    /// Bytes still due in the current chunk (chunked replies only).
    chunk_left: usize,
    /// Decoded body bytes not yet split into lines (kept as bytes so a character split across reads survives).
    text: Vec<u8>,
    calls: usize,
    eof: bool,
    /// Ollama said `done`; a body that ends without it was cut off.
    done: bool,
    failed: Option<String>,
}

impl Reader {
    /// Moves what `buf` holds of the body into `text`, stopping at an incomplete chunk header.
    fn decode(&mut self) {
        if !self.chunked {
            self.text.append(&mut self.buf);
            return;
        }
        loop {
            if self.chunk_left > 0 {
                let n = self.chunk_left.min(self.buf.len());
                self.text.extend(self.buf.drain(..n));
                self.chunk_left -= n;
                if self.chunk_left > 0 {
                    return;
                }
                continue;
            }
            // Between chunks: CRLF, then `<hex size>\r\n`; size 0 ends the body.
            while self.buf.starts_with(b"\r\n") {
                self.buf.drain(..2);
            }
            let Some(eol) = self.buf.windows(2).position(|w| w == b"\r\n") else { return };
            let Ok(size) = usize::from_str_radix(String::from_utf8_lossy(&self.buf[..eol]).split(';').next().unwrap_or("").trim(), 16) else {
                self.failed = Some("ollama: a malformed chunk in the reply".into());
                self.buf.clear();
                self.eof = true;
                return;
            };
            self.buf.drain(..eol + 2);
            if size == 0 {
                self.eof = true;
                return;
            }
            self.chunk_left = size;
        }
    }

    fn into_stream(self) -> BoxStream<'static, Result<Delta, String>> {
        stream::unfold(self, |mut r| async move {
            loop {
                r.decode();
                if let Some(end) = r.text.iter().position(|b| *b == b'\n') {
                    let line: Vec<u8> = r.text.drain(..=end).collect();
                    let line = String::from_utf8_lossy(&line);
                    if line.trim().is_empty() {
                        continue;
                    }
                    let items = deltas(&line, &mut r.calls);
                    r.done |= items.iter().any(|d| matches!(d, Ok(Delta::Usage(_))));
                    return Some((stream::iter(items), r));
                }
                if r.eof {
                    let rest = String::from_utf8_lossy(&std::mem::take(&mut r.text)).into_owned();
                    let mut items = if rest.trim().is_empty() { Vec::new() } else { deltas(&rest, &mut r.calls) };
                    r.done |= items.iter().any(|d| matches!(d, Ok(Delta::Usage(_))));
                    if let Some(why) = r.failed.take() {
                        items.push(Err(why));
                    } else if !r.done && !items.iter().any(Result::is_err) {
                        items.push(Err("ollama: the reply ended before the model finished".into()));
                    }
                    r.done = true;
                    return (!items.is_empty()).then(|| (stream::iter(items), r));
                }
                let mut chunk = [0u8; 8192];
                match tokio::time::timeout(IDLE_TIMEOUT, r.sock.read(&mut chunk)).await {
                    Ok(Ok(0)) => r.eof = true,
                    Ok(Ok(n)) => r.buf.extend_from_slice(&chunk[..n]),
                    Ok(Err(e)) => (r.failed, r.eof) = (Some(format!("ollama: {e}")), true),
                    Err(_) => (r.failed, r.eof) = (Some(format!("ollama: no more reply for {}s", IDLE_TIMEOUT.as_secs())), true),
                }
            }
        })
        .flatten()
        .boxed()
    }
}

/// One NDJSON line of `/api/chat` as the engine's deltas.
pub(crate) fn deltas(line: &str, calls: &mut usize) -> Vec<Result<Delta, String>> {
    let v: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => return vec![Err(format!("ollama: unreadable reply line ({e})"))],
    };
    if let Some(err) = v.get("error").and_then(Value::as_str) {
        return vec![Err(format!("ollama: {err}"))];
    }
    let text = |k: &str| v.pointer(&format!("/message/{k}")).and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_owned);
    let mut out: Vec<Result<Delta, String>> = Vec::new();
    out.extend(text("thinking").map(|t| Ok(Delta::Thought(t))));
    out.extend(text("content").map(|t| Ok(Delta::Text(t))));
    for call in v.pointer("/message/tool_calls").and_then(Value::as_array).into_iter().flatten() {
        *calls += 1;
        let name = call.pointer("/function/name").and_then(Value::as_str).unwrap_or_default().to_owned();
        let args = call.pointer("/function/arguments").cloned().unwrap_or(json!({}));
        let id = call.get("id").and_then(Value::as_str).map_or_else(|| format!("call_{calls}"), str::to_owned);
        out.push(Ok(Delta::Call(Call { id, name, args, thought_signatures: None })));
    }
    if v.get("done").and_then(Value::as_bool) == Some(true) {
        let n = |k: &str| v.get(k).and_then(Value::as_u64).unwrap_or(0);
        out.push(Ok(Delta::Usage(crate::events::TokenUsage { input: n("prompt_eval_count"), output: n("eval_count"), cached: 0 })));
    }
    out
}

/// `chat.ollamaNumCtx` from `$CORDATA_HOME/settings.json`, else [`DEFAULT_NUM_CTX`].
fn num_ctx_setting() -> u64 {
    let read = || -> Option<u64> {
        let text = std::fs::read_to_string(crate::policy::cordata_home()?.join("settings.json")).ok()?;
        serde_json::from_str::<Value>(&text).ok()?.pointer("/chat/ollamaNumCtx")?.as_u64().filter(|n| *n >= 2_048)
    };
    read().unwrap_or(DEFAULT_NUM_CTX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_local_ollama_models_take_this_path() {
        assert!(OllamaChat::for_model("llama3.1:8b").is_some());
        assert!(OllamaChat::for_model("qwen3-coder").is_some());
        for hosted in ["claude-sonnet-5-5", "gemini-3-pro", "gpt-5", "ollama_cloud::kimi-k3"] {
            assert!(OllamaChat::for_model(hosted).is_none(), "{hosted}");
        }
    }

    #[test]
    fn every_request_names_its_context_window() {
        let chat = OllamaChat::at("127.0.0.1:1", 70_000);
        let history = vec![Msg::User { text: "hi".into() }, Msg::Assistant { text: String::new(), calls: vec![Call { id: "call_1".into(), name: "read_file".into(), args: json!({"path": "a"}), thought_signatures: None }] }, Msg::ToolResult { id: "call_1".into(), output: "x".into() }];
        let body = chat.body("ollama::llama3.1:8b", "sys", &history, &[]);
        assert_eq!((body["model"].clone(), body["options"]["num_ctx"].clone()), (json!("llama3.1:8b"), json!(70_000)));
        assert_eq!(body["messages"][2]["tool_calls"][0]["function"]["name"], "read_file");
        assert_eq!(body["messages"][3]["role"], "tool");
    }

    #[test]
    fn reply_lines_become_text_calls_and_usage() {
        let mut n = 0;
        let got = deltas(r#"{"message":{"content":"hel","thinking":"hmm"},"done":false}"#, &mut n);
        assert_eq!(got, vec![Ok(Delta::Thought("hmm".into())), Ok(Delta::Text("hel".into()))]);
        let got = deltas(r#"{"message":{"content":"","tool_calls":[{"function":{"name":"read_file","arguments":{"path":"a"}}}]},"done":true,"prompt_eval_count":12,"eval_count":3}"#, &mut n);
        assert!(matches!(&got[0], Ok(Delta::Call(c)) if c.id == "call_1" && c.args == json!({"path": "a"})));
        assert_eq!(got[1], Ok(Delta::Usage(crate::events::TokenUsage { input: 12, output: 3, cached: 0 })));
        assert!(deltas(r#"{"error":"model not found"}"#, &mut n)[0].as_ref().unwrap_err().contains("model not found"));
    }

    /// A one-shot local server that answers with `reply` and hands back what it was sent.
    async fn serve(reply: Vec<u8>) -> (String, tokio::task::JoinHandle<String>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let task = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 65_536];
            let n = sock.read(&mut buf).await.unwrap();
            sock.write_all(&reply).await.unwrap();
            String::from_utf8_lossy(&buf[..n]).into_owned()
        });
        (addr, task)
    }

    async fn reply_of(reply: Vec<u8>) -> (Vec<Result<Delta, String>>, String) {
        let (addr, seen) = serve(reply).await;
        let chat = OllamaChat::at(&addr, 65_536);
        let got = chat.stream(chat.body("llama3", "s", &[Msg::User { text: "hi".into() }], &[])).await.unwrap().collect().await;
        (got, seen.await.unwrap())
    }

    fn text_of(got: &[Result<Delta, String>]) -> String {
        got.iter().filter_map(|d| if let Ok(Delta::Text(t)) = d { Some(t.as_str()) } else { None }).collect()
    }

    #[tokio::test]
    async fn a_streamed_reply_is_read_across_split_chunks() {
        // Chunked the way Ollama streams, with a line split across chunks and the two bytes of "é" split too.
        let a: &[u8] = b"{\"message\":{\"content\":\"he\"},\"done\":false}\n{\"message\":{\"content\":\"ll\xC3";
        let b: &[u8] = b"\xA9\"},\"done\":true,\"eval_count\":2}\n";
        let mut reply = format!("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n", a.len()).into_bytes();
        reply.extend_from_slice(a);
        reply.extend_from_slice(format!("\r\n{:x}\r\n", b.len()).as_bytes());
        reply.extend_from_slice(b);
        reply.extend_from_slice(b"\r\n0\r\n\r\n");
        let (got, seen) = reply_of(reply).await;
        assert_eq!(text_of(&got), "hellé");
        assert!(got.iter().all(Result::is_ok), "{got:?}");
        assert!(seen.contains("\"num_ctx\":65536"));
    }

    #[tokio::test]
    async fn a_reply_cut_off_before_done_is_an_error() {
        let body = "{\"message\":{\"content\":\"half\"},\"done\":false}\n";
        let (got, _) = reply_of(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}", body.len()).into_bytes()).await;
        assert_eq!(text_of(&got), "half");
        assert!(got.last().unwrap().as_ref().unwrap_err().contains("ended before the model finished"), "{got:?}");
        let (got, _) = reply_of(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nzz\r\nxx\r\n".to_vec()).await;
        assert!(got.last().unwrap().as_ref().unwrap_err().contains("malformed chunk"), "{got:?}");
    }

    /// Against a real local server: `CORDATA_LIVE_OLLAMA=1 cargo test -p acp-core live_ollama -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn live_ollama_reports_a_missing_model_as_an_error() {
        let chat = OllamaChat::at("127.0.0.1:11434", 65_536);
        let err = chat.stream(chat.body("cordata-no-such-model", "s", &[Msg::User { text: "hi".into() }], &[])).await.err().unwrap();
        assert!(err.contains("HTTP 404") && err.contains("not found"), "{err}");
    }
}
