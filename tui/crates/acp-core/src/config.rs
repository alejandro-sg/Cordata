//! Locates the backend (`backend/.venv/bin/python` + `backend/acp_server.py`)
//! as absolute paths, so the TUI works from any launch directory.

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

pub const BACKEND_ROOT_ENV: &str = "ACP_TUI_BACKEND_ROOT";

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
}
