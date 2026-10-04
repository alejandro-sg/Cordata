//! Locates the backend (`backend/.venv/bin/python` + `backend/acp_server.py`)
//! as absolute paths, so the TUI works from any launch directory. Also reads
//! the daemon's own config file for the WebSocket transport.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::{Error, Result};

pub const BACKEND_ROOT_ENV: &str = "ACP_TUI_BACKEND_ROOT";

/// Where `backend/daemon/server.py` writes its token and port. Same variable
/// name it reads, so a test or a second daemon can point both ends at one file.
pub const DAEMON_CONFIG_ENV: &str = "SIERRALAB_DAEMON_CONFIG";

#[derive(Debug, Clone)]
pub struct BackendConfig {
    pub backend_dir: PathBuf,
    pub python: PathBuf,
    pub server: PathBuf,
}

impl BackendConfig {
    /// Resolution order: explicit override, `ACP_TUI_BACKEND_ROOT`, walking
    /// up from the running binary, walking up from the current directory,
    /// then the source checkout this crate was compiled from. An override
    /// may name either the repo root or the `backend/` directory itself.
    pub fn discover(override_root: Option<&Path>) -> Result<Self> {
        let mut searched = Vec::new();

        let explicit = override_root
            .map(Path::to_path_buf)
            .or_else(|| std::env::var_os(BACKEND_ROOT_ENV).map(PathBuf::from));
        if let Some(root) = explicit {
            // An explicit choice never silently falls through to another checkout.
            return Self::from_root(&root).ok_or(Error::RepoRootNotFound(root));
        }

        let mut starts: Vec<PathBuf> = Vec::new();
        if let Ok(exe) = std::env::current_exe() {
            starts.extend(exe.parent().map(Path::to_path_buf));
        }
        starts.extend(std::env::current_dir().ok());
        starts.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")));

        for start in starts {
            for dir in start.ancestors() {
                if let Some(found) = Self::from_root(dir) {
                    return Ok(found);
                }
            }
            searched.push(start);
        }
        Err(Error::RepoRootNotFound(
            searched.into_iter().next().unwrap_or_default(),
        ))
    }

    fn from_root(root: &Path) -> Option<Self> {
        let backend_dir = [root.join("backend"), root.to_path_buf()]
            .into_iter()
            .find(|dir| dir.join("acp_server.py").is_file())?;
        let backend_dir = backend_dir.canonicalize().ok()?;
        Some(Self {
            python: backend_dir.join(".venv/bin/python"),
            server: backend_dir.join("acp_server.py"),
            backend_dir,
        })
    }

    /// Point at a different ACP server script (still run with the backend's
    /// venv python) — for testing the clients against a scripted agent.
    pub fn with_server_script(mut self, script: impl Into<PathBuf>) -> Self {
        self.server = script.into();
        self
    }

    /// Fails with an actionable message if either path is missing.
    pub fn validate(&self) -> Result<()> {
        if !self.python.is_file() {
            return Err(Error::PythonNotFound(self.python.clone()));
        }
        if !self.server.is_file() {
            return Err(Error::ServerNotFound(self.server.clone()));
        }
        Ok(())
    }
}

/// What the daemon wrote when it started: `{"token": "…", "port": 8765}`, mode
/// 0600 in the user's private harness directory.
///
/// The client reads the token from the same file the daemon created it in,
/// rather than taking it on the command line, because a token in `argv` is
/// visible to every process on the machine (`ps`). File permissions are the
/// authorization boundary here; loopback is not one.
#[derive(Clone, Deserialize)]
pub struct DaemonConfig {
    pub token: String,
    #[serde(default = "default_port")]
    pub port: u16,
    /// The file this was read from — named in errors, so a stale config is
    /// diagnosable without guessing which path was in play.
    #[serde(skip)]
    pub path: PathBuf,
}

fn default_port() -> u16 {
    8765
}

// By hand, so the token cannot reach a log line through `{:?}`.
impl std::fmt::Debug for DaemonConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DaemonConfig")
            .field("token", &"REDACTED")
            .field("port", &self.port)
            .field("path", &self.path)
            .finish()
    }
}

impl DaemonConfig {
    /// The daemon config file: `SIERRALAB_DAEMON_CONFIG`, else
    /// `~/.sierralab/daemon.json`.
    pub fn path() -> Result<PathBuf> {
        if let Some(override_path) = std::env::var_os(DAEMON_CONFIG_ENV) {
            return Ok(PathBuf::from(override_path));
        }
        let home = std::env::var_os("HOME")
            .ok_or_else(|| Error::DaemonConfigNotFound(PathBuf::from("$HOME is not set")))?;
        Ok(PathBuf::from(home).join(".sierralab").join("daemon.json"))
    }

    pub fn discover() -> Result<Self> {
        let path = Self::path()?;
        let text = std::fs::read_to_string(&path)
            .map_err(|_| Error::DaemonConfigNotFound(path.clone()))?;
        let mut config: Self = serde_json::from_str(&text).map_err(|error| {
            Error::DaemonUnreachable(format!(
                "{} is not valid daemon config: {error}",
                path.display()
            ))
        })?;
        config.path = path;
        Ok(config)
    }

    /// The socket URL for a session in `cwd`.
    ///
    /// The daemon takes `cwd` rather than a session id on purpose: the id is
    /// the daemon's to mint, and a client that has just started does not know
    /// it. Attaching to a *running* session in the same directory is exactly
    /// what someone reconnecting after a dropped terminal wants.
    pub fn socket_url(&self, cwd: &Path) -> String {
        format!(
            "ws://127.0.0.1:{}/ws?token={}&cwd={}",
            self.port,
            percent_encode(&self.token),
            percent_encode(&cwd.display().to_string()),
        )
    }
}

/// Percent-encoding for a query value, RFC 3986 unreserved set kept literal.
///
/// Hand-rolled to avoid a dependency for twenty lines: a `cwd` is a path, so
/// it routinely contains `/` and spaces, and neither survives a query string
/// unescaped — the daemon would resolve the wrong directory, or none.
fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(*byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolved_backend_paths_exist_and_are_absolute() {
        let config =
            BackendConfig::discover(None).expect("backend discoverable from this checkout");
        assert!(config.python.is_absolute() && config.server.is_absolute());
        config
            .validate()
            .expect("python and acp_server.py exist on disk");
    }

    #[test]
    fn override_accepts_repo_root_or_backend_dir() {
        let discovered = BackendConfig::discover(None).unwrap();
        let from_backend = BackendConfig::discover(Some(&discovered.backend_dir)).unwrap();
        let from_root =
            BackendConfig::discover(Some(discovered.backend_dir.parent().unwrap())).unwrap();
        assert_eq!(from_backend.server, from_root.server);
    }

    #[test]
    fn bad_override_does_not_fall_back() {
        let error = BackendConfig::discover(Some(Path::new("/nonexistent/nowhere"))).unwrap_err();
        assert!(matches!(error, Error::RepoRootNotFound(_)));
    }

    fn daemon(port: u16, token: &str) -> DaemonConfig {
        DaemonConfig {
            token: token.to_owned(),
            port,
            path: PathBuf::from("/tmp/daemon.json"),
        }
    }

    #[test]
    fn socket_url_carries_the_token_the_port_and_an_escaped_cwd() {
        let url = daemon(8765, "abc-123_XYZ").socket_url(Path::new("/Users/me/My Project"));
        assert_eq!(
            url,
            "ws://127.0.0.1:8765/ws?token=abc-123_XYZ&cwd=%2FUsers%2Fme%2FMy%20Project"
        );
    }

    #[test]
    fn percent_encoding_leaves_the_unreserved_set_alone() {
        assert_eq!(percent_encode("azAZ09-._~"), "azAZ09-._~");
        // A `/` kept literal would split the query value; a `&` would start a
        // new parameter. Both are escapes a path routinely contains.
        assert_eq!(percent_encode("a/b&c=d?e"), "a%2Fb%26c%3Dd%3Fe");
        assert_eq!(percent_encode("é"), "%C3%A9");
    }

    #[test]
    fn a_missing_daemon_config_names_the_path_it_looked_for() {
        // The override is what `DaemonConfig::path` reads, so a test can point
        // it at a file that is not there without touching the real one.
        unsafe { std::env::set_var(DAEMON_CONFIG_ENV, "/nonexistent/daemon.json") };
        let error = DaemonConfig::discover().unwrap_err();
        unsafe { std::env::remove_var(DAEMON_CONFIG_ENV) };
        assert!(
            matches!(error, Error::DaemonConfigNotFound(path) if path.ends_with("daemon.json"))
        );
    }
}
