//! The MCP servers a chat in this folder runs — read with the engine's own rules (`acp_core::native::mcp`), so what
//! the settings pane shows is what starts — and a short curated catalog the person can add to `~/.cordata/mcp.json`.
//! Read once when the pane opens, never per frame.

use std::fs;
use std::path::Path;

use acp_core::native::mcp::{self, Held, McpServerConfig};
use serde_json::{Map, Value, json};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpSource {
    User,
    Project,
}

impl McpSource {
    pub fn label(self) -> &'static str {
        match self {
            McpSource::User => "~/.cordata/mcp.json",
            McpSource::Project => ".mcp.json",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServerEntry {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub source: McpSource,
}

/// What the pane draws: the servers that will start, and the project's that wait for the person's approval.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub servers: Vec<McpServerEntry>,
    pub held: Vec<Held>,
}

#[derive(Debug, Clone)]
pub struct MarketplaceItem {
    pub id: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    pub command: &'static str,
    pub default_args: &'static [&'static str],
    pub category: &'static str,
}

/// Maintained servers only (the archived reference servers — brave-search, postgres, puppeteer, the old npm GitHub
/// server — are gone). Each runs on the person's machine with their rights; every call still asks first.
pub const MARKETPLACE_ITEMS: &[MarketplaceItem] = &[
    MarketplaceItem {
        id: "rea",
        name: "Rea reverse engineering",
        description: "Mach-O load commands, code signatures and entitlements of native binaries.",
        command: "rea",
        default_args: &[],
        category: "Native",
    },
    MarketplaceItem {
        id: "github",
        name: "GitHub",
        description: "Issues, pull requests and code search. Needs the github-mcp-server binary and GITHUB_PERSONAL_ACCESS_TOKEN in its env block.",
        command: "github-mcp-server",
        default_args: &["stdio"],
        category: "Developer",
    },
    MarketplaceItem {
        id: "git",
        name: "Git",
        description: "Read and search repository history (uvx mcp-server-git).",
        command: "uvx",
        default_args: &["mcp-server-git"],
        category: "Developer",
    },
    MarketplaceItem {
        id: "fetch",
        name: "Fetch",
        description: "Fetch a web page as markdown (uvx mcp-server-fetch). Reaches the network.",
        command: "uvx",
        default_args: &["mcp-server-fetch"],
        category: "Web",
    },
    MarketplaceItem {
        id: "memory",
        name: "Memory",
        description: "A local knowledge graph that persists across sessions (npx @modelcontextprotocol/server-memory).",
        command: "npx",
        default_args: &["-y", "@modelcontextprotocol/server-memory"],
        category: "Productivity",
    },
];

/// The servers a chat started in `cwd` would run, by the engine's rules.
pub fn snapshot(home: &Path, cwd: &Path) -> Snapshot {
    let (user, _) = mcp::configured(Some(home), Path::new("/nonexistent"), &[]);
    let (all, held) = mcp::configured(Some(home), cwd, &mcp::approved(Some(home)));
    let servers = all
        .into_iter()
        .map(|(name, McpServerConfig { command, args, .. })| {
            let source = if user.iter().any(|(n, c)| *n == name && c.command == command && c.args == args) { McpSource::User } else { McpSource::Project };
            McpServerEntry { name, command, args, source }
        })
        .collect();
    Snapshot { servers, held }
}

/// Adds (or replaces) a server in `<home>/mcp.json`, the person's own file: it applies in every folder and no
/// repository can change it. Written atomically, private.
pub fn install_server(home: &Path, id: &str, command: &str, args: &[&str]) -> Result<(), String> {
    let path = home.join("mcp.json");
    let mut root: Map<String, Value> = match fs::read_to_string(&path) {
        Ok(text) => match serde_json::from_str::<Value>(&text) {
            Ok(Value::Object(map)) => map,
            _ => return Err(format!("not overwriting {}: it does not parse", path.display())),
        },
        Err(_) => Map::new(),
    };
    let servers = root.entry("mcpServers").or_insert_with(|| json!({}));
    if !servers.is_object() {
        *servers = json!({});
    }
    servers.as_object_mut().expect("just made an object").insert(id.to_owned(), json!({"command": command, "args": args}));
    fs::create_dir_all(home).map_err(|e| e.to_string())?;
    let tmp = home.join(format!(".mcp.{}.tmp", std::process::id()));
    fs::write(&tmp, serde_json::to_string_pretty(&Value::Object(root)).map_err(|e| e.to_string())? + "\n").map_err(|e| e.to_string())?;
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600));
    }
    fs::rename(&tmp, &path).map_err(|e| e.to_string())
}

/// Approves one held-back server as it is now: its entry replaces any earlier one for the same folder and name, so a
/// changed server is re-approved rather than approved twice.
pub fn approve(store: &cordata_view::settings::Store, held: &Held) -> Result<(), String> {
    let prefix = held.approval.rsplit_once('|').map(|(p, _)| format!("{p}|")).unwrap_or_default();
    let mut list: Vec<String> = store.load().get("mcp.approved").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(str::to_owned).collect()).unwrap_or_default();
    list.retain(|a| !a.starts_with(&prefix));
    list.push(held.approval.clone());
    store.set(cordata_view::settings::Scope::User, "mcp.approved", json!(list))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh, empty folder under the system temp dir (the app links no `tempfile`).
    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("cordata-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::canonicalize(dir).unwrap()
    }

    #[test]
    fn shows_what_the_engine_starts_and_holds_back_an_unapproved_server() {
        let (home, cwd) = (scratch("mcp-home"), scratch("mcp-cwd"));
        install_server(home.as_path(), "rea", "rea", &[]).unwrap();
        std::fs::write(cwd.join(".mcp.json"), r#"{"mcpServers": {"evil": {"command": "curl"}}}"#).unwrap();
        let snap = snapshot(home.as_path(), cwd.as_path());
        assert_eq!(snap.servers.iter().map(|s| (s.name.as_str(), s.source)).collect::<Vec<_>>(), [("rea", McpSource::User)]);
        assert_eq!(snap.held.iter().map(|h| h.name.as_str()).collect::<Vec<_>>(), ["evil"]);
        std::fs::write(home.join("settings.json"), serde_json::json!({"mcp": {"approved": [snap.held[0].approval]}}).to_string()).unwrap();
        let snap = snapshot(home.as_path(), cwd.as_path());
        assert!(snap.held.is_empty());
        assert!(snap.servers.iter().any(|s| s.name == "evil" && s.source == McpSource::Project));
    }

    #[test]
    fn approving_a_changed_server_replaces_its_old_approval() {
        let (home, cwd) = (scratch("mcp-approve-home"), scratch("mcp-approve-cwd"));
        let store = cordata_view::settings::Store { user_dir: home.clone(), cwd: cwd.clone() };
        std::fs::write(cwd.join(".mcp.json"), r#"{"mcpServers": {"tool": {"command": "node"}}}"#).unwrap();
        approve(&store, &snapshot(&home, &cwd).held[0]).unwrap();
        assert!(snapshot(&home, &cwd).held.is_empty(), "approved through the settings store, read by the engine's rules");
        std::fs::write(cwd.join(".mcp.json"), r#"{"mcpServers": {"tool": {"command": "curl"}}}"#).unwrap();
        let snap = snapshot(&home, &cwd);
        assert!(snap.held[0].changed);
        approve(&store, &snap.held[0]).unwrap();
        assert!(snapshot(&home, &cwd).held.is_empty());
        assert_eq!(acp_core::native::mcp::approved(Some(&home)).len(), 1, "the old approval is gone, not kept beside the new");
    }

    #[test]
    fn install_refuses_to_clobber_a_broken_file() {
        let home = scratch("mcp-broken");
        std::fs::write(home.as_path().join("mcp.json"), "{ not json").unwrap();
        assert!(install_server(home.as_path(), "x", "x", &[]).is_err());
    }
}
