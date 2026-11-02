//! Small pieces of presentation *logic* every front end needs and none should re-derive: which kind of block a message
//! is. No drawing here.

use serde_json::Value;

use crate::app::Message;

/// A transcript block's kind (spacing, colour and when it may be committed all key on it).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    User,
    Agent,
    Thought,
    Tool,
    Divider,
    Info,
    Diff,
    Error,
    Help,
    Welcome,
}

impl Message {
    pub fn kind(&self) -> Kind {
        match self {
            Message::User(_) => Kind::User,
            Message::Agent(_) => Kind::Agent,
            Message::Thought(_) => Kind::Thought,
            Message::Tool(_) => Kind::Tool,
            Message::Divider(_) => Kind::Divider,
            Message::Info(_) => Kind::Info,
            Message::Diff(_) => Kind::Diff,
            Message::Error(_) => Kind::Error,
            Message::Help => Kind::Help,
            Message::Welcome(_) => Kind::Welcome,
        }
    }
}

/// One line saying what a tool call does: `$ cmd` for a command, the path or pattern for a read, search or edit, the raw
/// JSON otherwise. While a call streams in, its input is the JSON text so far. Sanitized (no escapes).
pub fn summarize_input(value: &Value) -> Option<String> {
    let text = match value {
        Value::Null => return None,
        Value::String(raw) if raw.is_empty() => return None,
        Value::String(raw) => match serde_json::from_str::<Value>(raw) {
            Ok(parsed @ Value::Object(_)) => return summarize_input(&parsed),
            _ => raw.clone(),
        },
        Value::Object(map) => {
            let field = |key: &str| map.get(key).and_then(Value::as_str);
            match (field("cmd"), field("pattern"), field("path")) {
                (Some(cmd), _, _) => format!("$ {cmd}"),
                (_, Some(pattern), Some(path)) => format!("{pattern}  in {path}"),
                (_, Some(pattern), None) => pattern.to_owned(),
                (_, _, Some(path)) => path.to_owned(),
                _ => value.to_string(),
            }
        }
        other => other.to_string(),
    };
    Some(crate::app::sanitize(&text))
}
