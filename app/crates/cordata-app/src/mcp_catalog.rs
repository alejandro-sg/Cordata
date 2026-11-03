//! Curated MCP Marketplace catalog, local discovery, and `.mcp.json` configuration.

use std::fs;
use std::path::{Path, PathBuf};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerEntry {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub source: McpSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum McpSource {
    Project,
    User,
    Claude,
    Antigravity,
}

impl McpSource {
    pub fn label(self) -> &'static str {
        match self {
            McpSource::Project => ".mcp.json",
            McpSource::User => "~/.cordata/mcp.json",
            McpSource::Claude => "Claude Code",
            McpSource::Antigravity => "Antigravity",
        }
    }
}

#[derive(Debug, Clone)]
pub struct MarketplaceItem {
    pub id: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    pub command: &'static str,
    pub default_args: &'static [&'static str],
    pub icon_glyph: &'static str,
    pub category: &'static str,
}

pub const MARKETPLACE_ITEMS: &[MarketplaceItem] = &[
    MarketplaceItem {
        id: "rea",
        name: "Rea Reverse Engineering & Binary Audit",
        description: "Verify arm64 Mach-O binaries, load commands, code signing & entitlements (Rule 5).",
        command: "rea",
        default_args: &[],
        icon_glyph: "⚙",
        category: "Native Dev",
    },
    MarketplaceItem {
        id: "github",
        name: "Official GitHub MCP Server",
        description: "Pull requests, issues, git commits, repos, and code search directly via GitHub API.",
        command: "npx",
        default_args: &["-y", "@modelcontextprotocol/server-github"],
        icon_glyph: "🐙",
        category: "Developer Tools",
    },
    MarketplaceItem {
        id: "brave-search",
        name: "Brave Web Search",
        description: "Real-time web search and fact verification for up-to-date documentation and APIs.",
        command: "npx",
        default_args: &["-y", "@modelcontextprotocol/server-brave-search"],
        icon_glyph: "🔍",
        category: "Web & Search",
    },
    MarketplaceItem {
        id: "postgres",
        name: "PostgreSQL Database Inspector",
        description: "Read schemas, run read-only queries, and inspect relations in PostgreSQL databases.",
        command: "npx",
        default_args: &["-y", "@modelcontextprotocol/server-postgres"],
        icon_glyph: "🐘",
        category: "Databases",
    },
    MarketplaceItem {
        id: "sqlite",
        name: "SQLite Database Inspector",
        description: "Query and inspect local SQLite databases, sqlite3 WAL ledgers, and state tables.",
        command: "uvx",
        default_args: &["mcp-server-sqlite"],
        icon_glyph: "🗄",
        category: "Databases",
    },
    MarketplaceItem {
        id: "fetch",
        name: "Fetch & Web Markdown",
        description: "Fetch web pages and convert HTML into clean LLM-friendly markdown text.",
        command: "uvx",
        default_args: &["mcp-server-fetch"],
        icon_glyph: "🌐",
        category: "Web & Search",
    },
    MarketplaceItem {
        id: "memory",
        name: "Knowledge Graph Long-Term Memory",
        description: "Persistent cross-session knowledge graph storing user preferences and project facts.",
        command: "npx",
        default_args: &["-y", "@modelcontextprotocol/server-memory"],
        icon_glyph: "🧠",
        category: "Productivity",
    },
    MarketplaceItem {
        id: "puppeteer",
        name: "Puppeteer Browser Automation",
        description: "Headless Chrome automation, taking screenshots and inspecting UI elements.",
        command: "npx",
        default_args: &["-y", "@modelcontextprotocol/server-puppeteer"],
        icon_glyph: "🎭",
        category: "Testing & UI",
    },
];

/// Discovers all configured and detected MCP servers across project, user, Claude, and Antigravity.
pub fn discover_servers(cwd: &Path) -> Vec<McpServerEntry> {
    let mut entries = Vec::new();

    // 1. Project .mcp.json
    let project_mcp = cwd.join(".mcp.json");
    if let Ok(content) = fs::read_to_string(&project_mcp) {
        if let Ok(val) = serde_json::from_str::<Value>(&content) {
            parse_mcp_servers(&val, McpSource::Project, &mut entries);
        }
    }

    // 2. User ~/.cordata/mcp.json
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        let user_mcp = home.join(".cordata/mcp.json");
        if let Ok(content) = fs::read_to_string(&user_mcp) {
            if let Ok(val) = serde_json::from_str::<Value>(&content) {
                parse_mcp_servers(&val, McpSource::User, &mut entries);
            }
        }

        // 3. Auto-discover from Claude Code ~/.claude.json
        let claude_json = home.join(".claude.json");
        if let Ok(content) = fs::read_to_string(&claude_json) {
            if let Ok(val) = serde_json::from_str::<Value>(&content) {
                parse_mcp_servers(&val, McpSource::Claude, &mut entries);
            }
        }

        // 4. Auto-discover Antigravity MCP servers (~/.gemini/antigravity/mcp/<name>)
        let agy_mcp = home.join(".gemini/antigravity/mcp");
        if agy_mcp.is_dir() {
            if let Ok(dirs) = fs::read_dir(&agy_mcp) {
                for entry in dirs.flatten() {
                    let path = entry.path();
                    if path.is_dir() {
                        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                        if !entries.iter().any(|e| e.name == name) {
                            entries.push(McpServerEntry {
                                name: name.clone(),
                                command: name,
                                args: Vec::new(),
                                source: McpSource::Antigravity,
                            });
                        }
                    }
                }
            }
        }
    }

    entries
}

fn parse_mcp_servers(val: &Value, source: McpSource, entries: &mut Vec<McpServerEntry>) {
    if let Some(servers) = val.get("mcpServers").and_then(Value::as_object) {
        for (name, spec) in servers {
            if entries.iter().any(|e| &e.name == name) {
                continue;
            }
            let command = spec.get("command").and_then(Value::as_str).unwrap_or("").to_owned();
            let args = spec
                .get("args")
                .and_then(Value::as_array)
                .map(|arr| arr.iter().filter_map(Value::as_str).map(|s| s.to_owned()).collect())
                .unwrap_or_default();
            if !command.is_empty() {
                entries.push(McpServerEntry {
                    name: name.clone(),
                    command,
                    args,
                    source,
                });
            }
        }
    }
}

/// Adds or updates an MCP server in <project>/.mcp.json.
pub fn install_server_to_project(cwd: &Path, id: &str, command: &str, args: &[&str]) -> Result<(), String> {
    let project_mcp = cwd.join(".mcp.json");
    let mut root: Value = if project_mcp.exists() {
        let content = fs::read_to_string(&project_mcp).map_err(|e| e.to_string())?;
        serde_json::from_str(&content).unwrap_or_else(|_| json!({}))
    } else {
        json!({})
    };

    if !root.is_object() {
        root = json!({});
    }

    if let Some(root_obj) = root.as_object_mut() {
        let servers = root_obj.entry("mcpServers").or_insert_with(|| json!({}));
        if let Some(obj) = servers.as_object_mut() {
            obj.insert(
                id.to_owned(),
                json!({
                    "command": command,
                    "args": args,
                }),
            );
        }
    }

    let json_text = serde_json::to_string_pretty(&root).map_err(|e| e.to_string())?;
    fs::write(&project_mcp, json_text).map_err(|e| e.to_string())?;
    Ok(())
}
