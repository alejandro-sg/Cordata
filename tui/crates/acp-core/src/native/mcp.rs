//! Model Context Protocol (MCP) stdio client and tool dispatcher for Cordata.
//! Discovers servers configured in `<cwd>/.mcp.json` and `~/.cordata/mcp.json`,
//! negotiates `initialize` and `tools/list` via JSON-RPC 2.0, and executes tool calls.

use std::borrow::Cow;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;
use tokio::time::timeout;

use super::tools::ToolSpec;

const MCP_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerConfig {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
}

struct ActiveProcess {
    stdin: ChildStdin,
    reader: BufReader<ChildStdout>,
    _child: Child,
    next_id: u64,
}

impl ActiveProcess {
    async fn spawn(config: &McpServerConfig, cwd: &Path) -> Result<Self, String> {
        let mut cmd = Command::new(&config.command);
        cmd.args(&config.args);
        cmd.current_dir(cwd);
        for (k, v) in &config.env {
            cmd.env(k, v);
        }
        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::null());

        let mut child = cmd.spawn().map_err(|e| format!("failed to spawn MCP server `{}`: {e}", config.command))?;
        let stdin = child.stdin.take().ok_or_else(|| "cannot capture stdin of MCP server".to_string())?;
        let stdout = child.stdout.take().ok_or_else(|| "cannot capture stdout of MCP server".to_string())?;
        let reader = BufReader::new(stdout);

        let mut proc = Self {
            stdin,
            reader,
            _child: child,
            next_id: 1,
        };

        // Handshake: initialize
        let init_params = json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {
                "name": "cordata",
                "version": env!("CARGO_PKG_VERSION")
            }
        });
        let _ = proc.request("initialize", init_params).await?;

        // Handshake: initialized notification
        let notify = json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        });
        let mut line = serde_json::to_string(&notify).map_err(|e| e.to_string())?;
        line.push('\n');
        let _ = proc.stdin.write_all(line.as_bytes()).await;
        let _ = proc.stdin.flush().await;

        Ok(proc)
    }

    async fn request(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_id;
        self.next_id += 1;
        let req = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        });
        let mut line = serde_json::to_string(&req).map_err(|e| e.to_string())?;
        line.push('\n');

        self.stdin.write_all(line.as_bytes()).await.map_err(|e| format!("MCP write failed: {e}"))?;
        self.stdin.flush().await.map_err(|e| format!("MCP flush failed: {e}"))?;

        let mut reply_line = String::new();
        timeout(MCP_TIMEOUT, self.reader.read_line(&mut reply_line))
            .await
            .map_err(|_| "MCP request timed out (30s)".to_string())?
            .map_err(|e| format!("MCP read failed: {e}"))?;

        if reply_line.trim().is_empty() {
            return Err("MCP server closed connection without response".into());
        }

        let resp: Value = serde_json::from_str(&reply_line).map_err(|e| format!("invalid JSON from MCP server: {e}"))?;
        if let Some(err) = resp.get("error") {
            let msg = err.get("message").and_then(Value::as_str).unwrap_or("unknown error");
            return Err(format!("MCP error: {msg}"));
        }

        resp.get("result").cloned().ok_or_else(|| "MCP response missing result field".into())
    }
}

pub struct McpManager {
    servers: HashMap<String, McpServerConfig>,
    processes: Mutex<HashMap<String, Arc<Mutex<ActiveProcess>>>>,
    tool_map: HashMap<String, (String, String)>, // alias/call_name -> (server_name, original_tool_name)
    tools: Vec<ToolSpec>,
}

impl McpManager {
    pub async fn load(cwd: &Path) -> Self {
        let mut configs = HashMap::new();

        // 1. Project-level .mcp.json
        let project_mcp = cwd.join(".mcp.json");
        if let Ok(content) = std::fs::read_to_string(&project_mcp) {
            parse_configs(&content, &mut configs);
        }

        // 2. User-level ~/.cordata/mcp.json
        if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
            let user_mcp = home.join(".cordata/mcp.json");
            if let Ok(content) = std::fs::read_to_string(&user_mcp) {
                parse_configs(&content, &mut configs);
            }
        }

        let mut manager = Self {
            servers: configs,
            processes: Mutex::new(HashMap::new()),
            tool_map: HashMap::new(),
            tools: Vec::new(),
        };

        manager.discover_tools(cwd).await;
        manager
    }

    async fn discover_tools(&mut self, cwd: &Path) {
        let builtin_names: [&str; 5] = ["read_file", "edit_file", "run_command", "code_search", "code_outline"];

        for (server_name, config) in &self.servers {
            match ActiveProcess::spawn(config, cwd).await {
                Ok(mut proc) => {
                    if let Ok(result) = proc.request("tools/list", json!({})).await {
                        if let Some(tools_arr) = result.get("tools").and_then(Value::as_array) {
                            for tool_val in tools_arr {
                                let Some(orig_name) = tool_val.get("name").and_then(Value::as_str) else { continue };
                                let description = tool_val.get("description").and_then(Value::as_str).unwrap_or("MCP tool");
                                let schema = tool_val.get("inputSchema").cloned().unwrap_or_else(|| json!({"type": "object"}));

                                let prefixed = format!("mcp__{server_name}__{orig_name}");
                                self.tool_map.insert(prefixed.clone(), (server_name.clone(), orig_name.to_owned()));

                                self.tools.push(ToolSpec {
                                    name: Cow::Owned(prefixed.clone()),
                                    description: Cow::Owned(format!("[{server_name}] {description}")),
                                    schema: schema.clone(),
                                });

                                // Also allow unprefixed name if no conflict
                                if !builtin_names.contains(&orig_name) && !self.tool_map.contains_key(orig_name) {
                                    self.tool_map.insert(orig_name.to_owned(), (server_name.clone(), orig_name.to_owned()));
                                    self.tools.push(ToolSpec {
                                        name: Cow::Owned(orig_name.to_owned()),
                                        description: Cow::Owned(format!("[{server_name}] {description}")),
                                        schema,
                                    });
                                }
                            }
                        }
                    }
                    self.processes.lock().await.insert(server_name.clone(), Arc::new(Mutex::new(proc)));
                }
                Err(_err) => {
                    // Silently ignore unreachable servers (e.g. CLI missing or offline test)
                }
            }
        }
    }

    pub fn tools(&self) -> &[ToolSpec] {
        &self.tools
    }

    pub fn has_tool(&self, name: &str) -> bool {
        self.tool_map.contains_key(name)
    }

    pub async fn call_tool(&self, name: &str, args: &Value) -> Result<String, String> {
        let (server_name, orig_name) = self.tool_map.get(name).ok_or_else(|| format!("unknown MCP tool `{name}`"))?;

        let proc_arc = {
            let procs = self.processes.lock().await;
            procs.get(server_name).cloned().ok_or_else(|| format!("MCP server `{server_name}` is not connected"))?
        };

        let mut proc = proc_arc.lock().await;
        let call_params = json!({
            "name": orig_name,
            "arguments": args
        });

        let result = proc.request("tools/call", call_params).await?;

        // Handle MCP result format: {"content": [{"type": "text", "text": "..."}], "isError": false}
        let is_error = result.get("isError").and_then(Value::as_bool).unwrap_or(false);
        let mut text_parts = Vec::new();

        if let Some(content_arr) = result.get("content").and_then(Value::as_array) {
            for item in content_arr {
                if let Some(t) = item.get("text").and_then(Value::as_str) {
                    text_parts.push(t);
                }
            }
        }

        let output = if text_parts.is_empty() {
            result.to_string()
        } else {
            text_parts.join("\n")
        };

        if is_error {
            Err(output)
        } else {
            Ok(output)
        }
    }
}

fn parse_configs(content: &str, map: &mut HashMap<String, McpServerConfig>) {
    let Ok(val) = serde_json::from_str::<Value>(content) else { return };

    let servers = if let Some(mcp_servers) = val.get("mcpServers").and_then(Value::as_object) {
        mcp_servers
    } else if let Some(obj) = val.as_object() {
        obj
    } else {
        return;
    };

    for (name, spec) in servers {
        if name == "mcpServers" {
            continue;
        }
        if let Ok(config) = serde_json::from_value::<McpServerConfig>(spec.clone()) {
            map.insert(name.clone(), config);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_mcp_configs() {
        let json_text = r#"{
            "mcpServers": {
                "rea": {
                    "command": "rea",
                    "args": ["serve"]
                },
                "github": {
                    "command": "npx",
                    "args": ["-y", "@modelcontextprotocol/server-github"],
                    "env": {"GITHUB_TOKEN": "test"}
                }
            }
        }"#;
        let mut map = HashMap::new();
        parse_configs(json_text, &mut map);
        assert_eq!(map.len(), 2);
        assert_eq!(map["rea"].command, "rea");
        assert_eq!(map["rea"].args, vec!["serve"]);
        assert_eq!(map["github"].env.get("GITHUB_TOKEN").unwrap(), "test");
    }

    #[tokio::test]
    async fn test_load_empty_dir() {
        let temp = tempfile::tempdir().unwrap();
        let manager = McpManager::load(temp.path()).await;
        assert!(manager.tools().is_empty());
        assert!(!manager.has_tool("any_tool"));
    }
}
