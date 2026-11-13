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
        let mut sock = tokio::net::TcpStream::connect(&self.addr).await.map_err(unreachable)?;
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
            match sock.read(&mut chunk).await.map_err(unreachable)? {
                0 => return Err("ollama: the connection closed before a reply".into()),
                n => raw.extend_from_slice(&chunk[..n]),
            }
        };
        let head = String::from_utf8_lossy(&raw[..head_end]).to_ascii_lowercase();
        let status = head.split_whitespace().nth(1).unwrap_or("").to_owned();
        let chunked = head.contains("transfer-encoding: chunked");
        let state = Reader { sock, buf: raw.split_off(head_end), chunked, chunk_left: 0, text: String::new(), calls: 0, eof: false };
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
    text: String,
    calls: usize,
    eof: bool,
}

impl Reader {
    /// Moves what `buf` holds of the body into `text`, stopping at an incomplete chunk header.
    fn decode(&mut self) {
        if !self.chunked {
            self.text.push_str(&String::from_utf8_lossy(&std::mem::take(&mut self.buf)));
            return;
        }
        loop {
            if self.chunk_left > 0 {
                let n = self.chunk_left.min(self.buf.len());
                self.text.push_str(&String::from_utf8_lossy(&self.buf.drain(..n).collect::<Vec<_>>()));
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
            let size = usize::from_str_radix(String::from_utf8_lossy(&self.buf[..eol]).split(';').next().unwrap_or("").trim(), 16).unwrap_or(0);
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
                if let Some(end) = r.text.find('\n') {
                    let line: String = r.text.drain(..=end).collect();
                    if line.trim().is_empty() {
                        continue;
                    }
                    let items = deltas(&line, &mut r.calls);
                    return Some((stream::iter(items), r));
                }
                if r.eof {
                    let rest = std::mem::take(&mut r.text);
                    return (!rest.trim().is_empty()).then(|| (stream::iter(deltas(&rest, &mut r.calls)), r));
                }
                let mut chunk = [0u8; 8192];
                match r.sock.read(&mut chunk).await {
                    Ok(0) => r.eof = true,
                    Ok(n) => r.buf.extend_from_slice(&chunk[..n]),
                    Err(e) => {
                        r.eof = true;
                        return Some((stream::iter(vec![Err(format!("ollama: {e}"))]), r));
                    }
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

    #[tokio::test]
    async fn a_streamed_reply_is_read_across_split_chunks() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 65_536];
            let n = sock.read(&mut buf).await.unwrap();
            // Chunked, with a line split across two chunks, the way Ollama streams.
            let (a, b) = ("{\"message\":{\"content\":\"he\"},\"done\":false}\n{\"message\":{\"con", "tent\":\"llo\"},\"done\":true,\"eval_count\":2}\n");
            let reply = format!("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n{a}\r\n{:x}\r\n{b}\r\n0\r\n\r\n", a.len(), b.len());
            sock.write_all(reply.as_bytes()).await.unwrap();
            String::from_utf8_lossy(&buf[..n]).into_owned()
        });
        let chat = OllamaChat::at(&addr.to_string(), 65_536);
        let stream = chat.stream(chat.body("llama3", "s", &[Msg::User { text: "hi".into() }], &[])).await.unwrap();
        let got: Vec<_> = stream.collect().await;
        let text: String = got.iter().filter_map(|d| if let Ok(Delta::Text(t)) = d { Some(t.as_str()) } else { None }).collect();
        assert_eq!(text, "hello");
        assert!(seen.await.unwrap().contains("\"num_ctx\":65536"));
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
