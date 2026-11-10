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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarketplaceItem {
    pub id: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    pub command: &'static str,
    pub default_args: &'static [&'static str],
    pub category: &'static str,
    pub required_env: &'static [&'static str],
}

pub const CATEGORIES: &[&str] = &[
    "All",
    "Developer",
    "Database",
    "Web",
    "Productivity",
    "Cloud",
    "Native",
];

/// Maintained servers catalog across categories. Each runs on the person's machine with their rights;
/// every call still asks first.
pub const MARKETPLACE_ITEMS: &[MarketplaceItem] = &[
    MarketplaceItem {
        id: "rea",
        name: "Rea reverse engineering",
        description: "Mach-O load commands, code signatures and entitlements of native binaries.",
        command: "rea",
        default_args: &[],
        category: "Native",
        required_env: &[],
    },
    MarketplaceItem {
        id: "github",
        name: "GitHub",
        description: "Issues, pull requests, commits and repository code search.",
        command: "github-mcp-server",
        default_args: &["stdio"],
        category: "Developer",
        required_env: &["GITHUB_PERSONAL_ACCESS_TOKEN"],
    },
    MarketplaceItem {
        id: "git",
        name: "Git",
        description: "Read, diff and search local repository history (uvx mcp-server-git).",
        command: "uvx",
        default_args: &["mcp-server-git"],
        category: "Developer",
        required_env: &[],
    },
    MarketplaceItem {
        id: "fetch",
        name: "Fetch",
        description: "Fetch web pages as clean markdown over HTTP/HTTPS (uvx mcp-server-fetch).",
        command: "uvx",
        default_args: &["mcp-server-fetch"],
        category: "Web",
        required_env: &[],
    },
    MarketplaceItem {
        id: "brave-search",
        name: "Brave Search",
        description: "Live internet web search and query index.",
        command: "npx",
        default_args: &["-y", "@modelcontextprotocol/server-brave-search"],
        category: "Web",
        required_env: &["BRAVE_API_KEY"],
    },
    MarketplaceItem {
        id: "puppeteer",
        name: "Puppeteer",
        description: "Headless Chrome web automation, scraping, and visual screenshot capture.",
        command: "npx",
        default_args: &["-y", "@modelcontextprotocol/server-puppeteer"],
        category: "Web",
        required_env: &[],
    },
    MarketplaceItem {
        id: "memory",
        name: "Memory",
        description: "A local knowledge graph that persists facts across sessions.",
        command: "npx",
        default_args: &["-y", "@modelcontextprotocol/server-memory"],
        category: "Productivity",
        required_env: &[],
    },
    MarketplaceItem {
        id: "sequential-thinking",
        name: "Sequential Thinking",
        description: "Structured step-by-step problem-solving and dynamic plan refinement.",
        command: "npx",
        default_args: &["-y", "@modelcontextprotocol/server-sequential-thinking"],
        category: "Productivity",
        required_env: &[],
    },
    MarketplaceItem {
        id: "postgres",
        name: "PostgreSQL",
        description: "Inspect schema, view tables, and execute analytical database queries.",
        command: "npx",
        default_args: &["-y", "@modelcontextprotocol/server-postgres"],
        category: "Database",
        required_env: &["DATABASE_URL"],
    },
    MarketplaceItem {
        id: "sqlite",
        name: "SQLite",
        description: "Read schema and run queries against local SQLite database files.",
        command: "uvx",
        default_args: &["mcp-server-sqlite"],
        category: "Database",
        required_env: &[],
    },
    MarketplaceItem {
        id: "redis",
        name: "Redis",
        description: "Key-value inspection, cache queries, and TTL monitoring.",
        command: "npx",
        default_args: &["-y", "@modelcontextprotocol/server-redis"],
        category: "Database",
        required_env: &["REDIS_URL"],
    },
    MarketplaceItem {
        id: "supabase",
        name: "Supabase",
        description: "Inspect databases, migrations, edge functions, and project schemas.",
        command: "npx",
        default_args: &["-y", "@supabase/mcp-server"],
        category: "Database",
        required_env: &["SUPABASE_ACCESS_TOKEN"],
    },
    MarketplaceItem {
        id: "filesystem",
        name: "Filesystem",
        description: "Direct secure access to designated local directories.",
        command: "npx",
        default_args: &["-y", "@modelcontextprotocol/server-filesystem"],
        category: "Developer",
        required_env: &[],
    },
    MarketplaceItem {
        id: "docker",
        name: "Docker",
        description: "Inspect containers, image registries, compose setups, and container logs.",
        command: "uvx",
        default_args: &["mcp-server-docker"],
        category: "Developer",
        required_env: &[],
    },
    MarketplaceItem {
        id: "kubernetes",
        name: "Kubernetes",
        description: "Cluster exploration, pods, deployments, services, and namespace monitoring.",
        command: "npx",
        default_args: &["-y", "@modelcontextprotocol/server-kubernetes"],
        category: "Cloud",
        required_env: &[],
    },
    MarketplaceItem {
        id: "cloudflare",
        name: "Cloudflare",
        description: "Manage Workers, KV namespaces, DNS records, and Pages deployments.",
        command: "npx",
        default_args: &["-y", "@cloudflare/mcp-server-cloudflare"],
        category: "Cloud",
        required_env: &["CLOUDFLARE_API_TOKEN"],
    },
    MarketplaceItem {
        id: "sentry",
        name: "Sentry",
        description: "Search issues, investigate stack traces, and review crash telemetry.",
        command: "uvx",
        default_args: &["mcp-server-sentry"],
        category: "Developer",
        required_env: &["SENTRY_AUTH_TOKEN"],
    },
    MarketplaceItem {
        id: "linear",
        name: "Linear",
        description: "Search and triage issues, sprints, cycles, and project milestones.",
        command: "npx",
        default_args: &["-y", "@modelcontextprotocol/server-linear"],
        category: "Productivity",
        required_env: &["LINEAR_API_KEY"],
    },
    MarketplaceItem {
        id: "slack",
        name: "Slack",
        description: "List channels, search message history, and post notifications.",
        command: "npx",
        default_args: &["-y", "@modelcontextprotocol/server-slack"],
        category: "Productivity",
        required_env: &["SLACK_BOT_TOKEN"],
    },
    MarketplaceItem {
        id: "everything",
        name: "Everything Reference",
        description: "Reference implementation of all MCP features (tools, prompts, sampling).",
        command: "npx",
        default_args: &["-y", "@modelcontextprotocol/server-everything"],
        category: "Developer",
        required_env: &[],
    },
];

/// Searches marketplace items by query text and optional category filter.
pub fn search_marketplace(query: &str, category: Option<&str>) -> Vec<&'static MarketplaceItem> {
    let q = query.trim().to_lowercase();
    let cat = category.filter(|c| *c != "All" && !c.is_empty());
    MARKETPLACE_ITEMS
        .iter()
        .filter(|item| {
            if let Some(c) = cat
                && !item.category.eq_ignore_ascii_case(c)
            {
                return false;
            }
            if q.is_empty() {
                return true;
            }
            item.id.to_lowercase().contains(&q)
                || item.name.to_lowercase().contains(&q)
                || item.description.to_lowercase().contains(&q)
                || item.category.to_lowercase().contains(&q)
                || item.command.to_lowercase().contains(&q)
        })
        .collect()
}

/// Whether `command` can start: an absolute path that exists, or a name found on `PATH`.
pub fn command_on_path(command: &str) -> bool {
    use std::os::unix::fs::PermissionsExt;
    let runs = |p: &Path| p.metadata().is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0);
    if command.contains('/') {
        return runs(Path::new(command));
    }
    std::env::var_os("PATH").is_some_and(|paths| std::env::split_paths(&paths).any(|dir| runs(&dir.join(command))))
}

/// What stands between a catalog entry and a server that starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Readiness {
    Ready,
    /// The program is not installed (`uvx`, `npx`, ...).
    MissingCommand,
    /// The server needs these variables. The engine starts servers with a cleared environment, so they must be in the
    /// server's own `env` block; Add cannot invent them, and a secret is not written for the person.
    NeedsEnv(&'static [&'static str]),
}

pub fn readiness(item: &MarketplaceItem) -> Readiness {
    if !item.required_env.is_empty() {
        Readiness::NeedsEnv(item.required_env)
    } else if !command_on_path(item.command) {
        Readiness::MissingCommand
    } else {
        Readiness::Ready
    }
}

/// The `mcp.json` entry for `item`, with a placeholder for each variable it needs: copied for the person to fill in
/// (the file is private to them), never written with a made-up value.
pub fn setup_snippet(item: &MarketplaceItem) -> String {
    let env: Map<String, Value> = item.required_env.iter().map(|k| ((*k).to_owned(), Value::String(format!("<your {k}>")))).collect();
    let mut entry = json!({"command": item.command, "args": item.default_args});
    if !env.is_empty() {
        entry.as_object_mut().expect("an object").insert("env".into(), Value::Object(env));
    }
    serde_json::to_string_pretty(&json!({"mcpServers": {item.id: entry}})).expect("serialises") + "\n"
}

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

/// Adds (or replaces) a server in `<home>/mcp.json` with optional environment variables.
pub fn install_server_with_env(home: &Path, id: &str, command: &str, args: &[&str], env: &[(&str, &str)]) -> Result<(), String> {
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
    let mut entry = json!({"command": command, "args": args});
    if !env.is_empty() {
        let mut env_map = Map::new();
        for (k, v) in env {
            env_map.insert((*k).to_owned(), Value::String((*v).to_owned()));
        }
        entry.as_object_mut().unwrap().insert("env".into(), Value::Object(env_map));
    }
    servers.as_object_mut().expect("just made an object").insert(id.to_owned(), entry);
    fs::create_dir_all(home).map_err(|e| e.to_string())?;
    let tmp = home.join(format!(".mcp.{}.tmp", std::process::id()));
    fs::write(&tmp, serde_json::to_string_pretty(&Value::Object(root)).map_err(|e| e.to_string())? + "\n").map_err(|e| e.to_string())?;
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600));
    }
    fs::rename(&tmp, &path).map_err(|e| e.to_string())
}

/// Adds (or replaces) a server in `<home>/mcp.json`, the person's own file: it applies in every folder and no
/// repository can change it. Written atomically, private.
pub fn install_server(home: &Path, id: &str, command: &str, args: &[&str]) -> Result<(), String> {
    install_server_with_env(home, id, command, args, &[])
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

    #[test]
    fn search_marketplace_filters_by_query_and_category() {
        // Query search
        let sqlite = search_marketplace("sqlite", None);
        assert_eq!(sqlite.len(), 1);
        assert_eq!(sqlite[0].id, "sqlite");

        // Category filter
        let db = search_marketplace("", Some("Database"));
        assert!(db.iter().any(|i| i.id == "postgres"));
        assert!(db.iter().any(|i| i.id == "sqlite"));
        assert!(!db.iter().any(|i| i.id == "github"));

        // Combined query + category
        let search_db = search_marketplace("post", Some("Database"));
        assert_eq!(search_db.len(), 1);
        assert_eq!(search_db[0].id, "postgres");

        // Non-existent search
        let none = search_marketplace("nonexistent-xyz-tool", None);
        assert!(none.is_empty());
    }

    #[test]
    fn install_server_with_env_persists_environment_variables() {
        let home = scratch("mcp-env");
        install_server_with_env(home.as_path(), "gh", "github-mcp-server", &["stdio"], &[("GITHUB_PERSONAL_ACCESS_TOKEN", "test_tok")]).unwrap();
        let text = std::fs::read_to_string(home.as_path().join("mcp.json")).unwrap();
        let val: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(val["mcpServers"]["gh"]["command"], "github-mcp-server");
        assert_eq!(val["mcpServers"]["gh"]["env"]["GITHUB_PERSONAL_ACCESS_TOKEN"], "test_tok");
    }

    #[test]
    fn the_catalog_is_well_formed() {
        let mut seen = std::collections::HashSet::new();
        for item in MARKETPLACE_ITEMS {
            assert!(seen.insert(item.id), "{}: duplicate id", item.id);
            assert!(!item.command.is_empty() && !item.name.is_empty() && !item.description.is_empty(), "{}: empty field", item.id);
            assert!(CATEGORIES.contains(&item.category) && item.category != "All", "{}: category {}", item.id, item.category);
            for var in item.required_env {
                assert!(var.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_'), "{}: env name {var}", item.id);
            }
            // Every entry is found by its own id and by its own name.
            assert!(search_marketplace(item.id, None).iter().any(|i| i.id == item.id), "{}: not found by id", item.id);
            assert!(search_marketplace(item.name, Some(item.category)).iter().any(|i| i.id == item.id), "{}: not found by name in its category", item.id);
        }
        assert!(MARKETPLACE_ITEMS.len() >= 10);
        // Each category on the filter holds something, so no pill leads to an empty list.
        for category in CATEGORIES.iter().filter(|c| **c != "All") {
            assert!(!search_marketplace("", Some(category)).is_empty(), "{category} is empty");
        }
    }

    #[test]
    fn readiness_says_what_is_missing_and_never_invents_a_secret() {
        for item in MARKETPLACE_ITEMS {
            match readiness(item) {
                Readiness::NeedsEnv(vars) => assert_eq!(vars, item.required_env, "{}", item.id),
                other => assert!(item.required_env.is_empty(), "{}: {other:?}", item.id),
            }
            let snippet: Value = serde_json::from_str(&setup_snippet(item)).expect("the snippet is JSON");
            let entry = &snippet["mcpServers"][item.id];
            assert_eq!(entry["command"], item.command, "{}", item.id);
            for var in item.required_env {
                let placeholder = entry["env"][*var].as_str().unwrap_or_default();
                assert!(placeholder.starts_with("<your "), "{}: {var} must be a placeholder, not a value", item.id);
            }
            assert_eq!(entry.get("env").is_some(), !item.required_env.is_empty(), "{}", item.id);
        }
        assert!(command_on_path("sh") && command_on_path("/bin/sh"));
        assert!(!command_on_path("cordata-no-such-program-xyz") && !command_on_path("/nonexistent/x"));
    }

    #[test]
    fn adding_every_server_keeps_the_file_valid_and_each_shows_as_the_users() {
        let home = scratch("market-all");
        let cwd = scratch("market-cwd");
        for item in MARKETPLACE_ITEMS {
            install_server(&home, item.id, item.command, item.default_args).unwrap_or_else(|e| panic!("{}: {e}", item.id));
        }
        // Adding one twice replaces it; it does not duplicate.
        let first = &MARKETPLACE_ITEMS[0];
        install_server(&home, first.id, first.command, first.default_args).unwrap();
        let file: Value = serde_json::from_str(&std::fs::read_to_string(home.join("mcp.json")).unwrap()).unwrap();
        assert_eq!(file["mcpServers"].as_object().unwrap().len(), MARKETPLACE_ITEMS.len());
        let snap = snapshot(&home, &cwd);
        assert_eq!(snap.servers.len(), MARKETPLACE_ITEMS.len());
        assert!(snap.servers.iter().all(|s| s.source == McpSource::User) && snap.held.is_empty());
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(home.join("mcp.json")).unwrap().permissions().mode() & 0o777, 0o600, "private to the person");
    }
}
