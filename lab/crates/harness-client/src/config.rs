//! Where the daemon is and the token it wants.
//!
//! The daemon writes both to `~/.cordata/daemon.json` (mode 0600): `token`, `port` (the HTTP /
//! WebSocket surface) and `grpcPort` (this client's). `CORDATA_DAEMON_CONFIG` points at another
//! file, `CORDATA_HOME` at another directory; `CORDATA_GRPC_ADDR` / `CORDATA_DAEMON_TOKEN`
//! override the pair outright (for a daemon on a non-default port or a test). The listener is
//! loopback-only, so the address is always `127.0.0.1:<port>`.

use std::fmt;
use std::path::{Path, PathBuf};

use serde::Deserialize;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read the daemon config {path}: {source}")]
    Read { path: PathBuf, source: std::io::Error },
    #[error("the daemon config {path} is not valid JSON: {source}")]
    Parse { path: PathBuf, source: serde_json::Error },
    #[error("{path} has no `grpcPort`: the daemon is not running its gRPC bridge (start it with `make daemon`)")]
    NoGrpcPort { path: PathBuf },
    #[error("{path} has no `token`")]
    NoToken { path: PathBuf },
}

/// Address and token of the daemon's gRPC bridge.
#[derive(Clone, PartialEq, Eq)]
pub struct DaemonConfig {
    /// `host:port`, loopback.
    pub addr: String,
    pub token: String,
}

// The token is the one secret this process holds; it must not reach a log through `{:?}`.
impl fmt::Debug for DaemonConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DaemonConfig").field("addr", &self.addr).field("token", &"<redacted>").finish()
    }
}

#[derive(Deserialize)]
struct File {
    token: Option<String>,
    #[serde(rename = "grpcPort")]
    grpc_port: Option<u16>,
}

impl DaemonConfig {
    pub fn new(addr: impl Into<String>, token: impl Into<String>) -> Self {
        Self { addr: addr.into(), token: token.into() }
    }

    /// `http://127.0.0.1:8766` — what tonic's `Endpoint` wants.
    pub fn uri(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub fn config_path() -> PathBuf {
        if let Some(path) = std::env::var_os("CORDATA_DAEMON_CONFIG") {
            return PathBuf::from(path);
        }
        let home = std::env::var_os("CORDATA_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cordata")))
            .unwrap_or_else(|| PathBuf::from(".cordata"));
        home.join("daemon.json")
    }

    pub fn discover() -> Result<Self, ConfigError> {
        let env_addr = std::env::var("CORDATA_GRPC_ADDR").ok().filter(|a| !a.is_empty());
        let env_token = std::env::var("CORDATA_DAEMON_TOKEN").ok().filter(|t| !t.is_empty());
        if let (Some(addr), Some(token)) = (env_addr.clone(), env_token.clone()) {
            return Ok(Self { addr, token });
        }
        let mut found = Self::from_file(&Self::config_path());
        // An override replaces its half of a file that has the other.
        if let Ok(config) = found.as_mut() {
            if let Some(addr) = env_addr {
                config.addr = addr;
            }
            if let Some(token) = env_token {
                config.token = token;
            }
        }
        found
    }

    pub fn from_file(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read { path: path.to_owned(), source })?;
        let file: File = serde_json::from_str(&text).map_err(|source| ConfigError::Parse { path: path.to_owned(), source })?;
        let token = file.token.filter(|t| !t.is_empty()).ok_or_else(|| ConfigError::NoToken { path: path.to_owned() })?;
        let port = file.grpc_port.ok_or_else(|| ConfigError::NoGrpcPort { path: path.to_owned() })?;
        Ok(Self { addr: format!("127.0.0.1:{port}"), token })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_daemon_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.json");
        std::fs::write(&path, r#"{"token": "abc", "port": 8765, "grpcPort": 9001}"#).unwrap();
        let config = DaemonConfig::from_file(&path).unwrap();
        assert_eq!(config.addr, "127.0.0.1:9001");
        assert_eq!(config.token, "abc");
        assert_eq!(config.uri(), "http://127.0.0.1:9001");
    }

    #[test]
    fn a_daemon_without_a_bridge_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.json");
        std::fs::write(&path, r#"{"token": "abc", "port": 8765}"#).unwrap();
        let error = DaemonConfig::from_file(&path).unwrap_err().to_string();
        assert!(error.contains("grpcPort") && error.contains("make daemon"), "{error}");
    }

    #[test]
    fn the_token_never_prints() {
        let shown = format!("{:?}", DaemonConfig::new("127.0.0.1:1", "super-secret-token"));
        assert!(!shown.contains("super-secret-token"), "{shown}");
    }

    #[test]
    fn a_missing_file_is_a_readable_error() {
        let error = DaemonConfig::from_file(Path::new("/nonexistent/daemon.json")).unwrap_err().to_string();
        assert!(error.contains("cannot read the daemon config"), "{error}");
    }
}
