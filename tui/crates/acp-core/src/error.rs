//! One error type for everything `acp-core` can fail at, with actionable
//! messages for the setup mistakes people actually hit.

use std::path::PathBuf;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("backend python not found at {0} — run scripts/setup.sh first")]
    PythonNotFound(PathBuf),

    #[error("backend ACP server not found at {0}")]
    ServerNotFound(PathBuf),

    #[error(
        "could not locate backend/acp_server.py (searched from {0}); \
         pass --backend-root or set ACP_TUI_BACKEND_ROOT"
    )]
    RepoRootNotFound(PathBuf),

    #[error("ACP error: {0}")]
    Protocol(String),

    #[error("session is closed")]
    SessionClosed,

    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl From<agent_client_protocol::Error> for Error {
    fn from(error: agent_client_protocol::Error) -> Self {
        Error::Protocol(describe_acp_error(&error))
    }
}

/// `agent_client_protocol::Error`'s own Display drops the `data` payload,
/// which is where the Python server (and the SDK, for a dead child process)
/// put the real reason — so surface it, minus source-location noise.
pub fn describe_acp_error(error: &agent_client_protocol::Error) -> String {
    let code = i32::from(error.code);
    match error
        .data
        .as_ref()
        .map(data_text)
        .filter(|text| !text.is_empty())
    {
        Some(data) => format!("{} (code {code}): {data}", error.message),
        None => format!("{} (code {code})", error.message),
    }
}

fn data_text(data: &serde_json::Value) -> String {
    use serde_json::Value;
    match data {
        Value::String(text) => text.clone(),
        Value::Object(map) => {
            for key in ["data", "details", "message"] {
                if let Some(inner) = map.get(key) {
                    return data_text(inner);
                }
            }
            let mut rest = map.clone();
            rest.remove("spawned_at");
            if rest.is_empty() {
                String::new()
            } else {
                Value::Object(rest).to_string()
            }
        }
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_sdk_data_is_unwrapped_without_source_locations() {
        let error = agent_client_protocol::Error::internal_error().data(serde_json::json!({
            "spawned_at": "/x/jsonrpc.rs:1:1",
            "data": "Process exited with exit status: 3",
        }));
        assert_eq!(
            describe_acp_error(&error),
            "Internal error (code -32603): Process exited with exit status: 3"
        );
    }

    #[test]
    fn backend_details_payload_is_surfaced() {
        let error = agent_client_protocol::Error::internal_error()
            .data(serde_json::json!({"details": "Connection error."}));
        assert!(describe_acp_error(&error).ends_with(": Connection error."));
    }
}
