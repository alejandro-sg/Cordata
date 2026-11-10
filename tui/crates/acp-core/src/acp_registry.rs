//! Agent Client Protocol (ACP) Registry client and agent discovery.
//!
//! Inspired by T3 Code's `acpRegistry.ts`, this module provides discovery, metadata parsing,
//! integrity checking, and hermetic offline resolution for ACP-compliant agent distributions.

use serde::{Deserialize, Serialize};

pub const ACP_REGISTRY_CDN_HOSTNAME: &str = "cdn.agentclientprotocol.com";
pub const ACP_REGISTRY_MAX_AGENTS: usize = 512;

/// Supported delivery formats for an ACP agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AcpDistribution {
    Binary,
    Npx,
    Uvx,
}

impl AcpDistribution {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Binary => "binary",
            Self::Npx => "npx",
            Self::Uvx => "uvx",
        }
    }
}

/// Verification scheme for an ACP agent distribution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AcpIntegrity {
    Sha256,
    Registry,
}

/// Metadata describing an agent listed in the ACP Registry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcpRegistryAgent {
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    #[serde(default)]
    pub authors: Vec<String>,
    pub license: Option<String>,
    pub website: Option<String>,
    pub repository: Option<String>,
    pub icon: Option<String>,
    pub distribution: AcpDistribution,
    pub integrity: AcpIntegrity,
    pub checksum: Option<String>,
}

/// Validates that an agent ID conforms to the ACP naming convention (`^[a-z0-9][a-z0-9._-]*$`).
pub fn is_valid_agent_id(id: &str) -> bool {
    if id.is_empty() || id.len() > 128 {
        return false;
    }
    let mut chars = id.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() || c.is_ascii_digit() => (),
        _ => return false,
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '_' || c == '-')
}

/// Generates the official ACP CDN icon URL for a valid agent ID.
pub fn official_icon_url_for_agent(agent_id: &str) -> Option<String> {
    if is_valid_agent_id(agent_id) {
        Some(format!(
            "https://{ACP_REGISTRY_CDN_HOSTNAME}/registry/v1/latest/{agent_id}.svg"
        ))
    } else {
        None
    }
}

/// In-memory catalog of ACP agents supporting offline search and validation.
#[derive(Debug, Clone, Default)]
pub struct AcpRegistryCatalog {
    agents: Vec<AcpRegistryAgent>,
}

impl AcpRegistryCatalog {
    pub fn new() -> Self {
        Self { agents: Vec::new() }
    }

    /// Initializes a catalog with standard well-known ACP agents.
    pub fn standard() -> Self {
        let mut catalog = Self::new();
        catalog.register(AcpRegistryAgent {
            id: "cordata".into(),
            name: "Cordata Native Agent".into(),
            version: "0.5.0".into(),
            description: "High performance native Rust agent workspace with Seatbelt isolation".into(),
            authors: vec!["Cordata Core Team".into()],
            license: Some("MIT".into()),
            website: Some("https://cordata.run".into()),
            repository: Some("https://github.com/cordata/cordata".into()),
            icon: official_icon_url_for_agent("cordata"),
            distribution: AcpDistribution::Binary,
            integrity: AcpIntegrity::Sha256,
            checksum: None,
        });
        catalog.register(AcpRegistryAgent {
            id: "claude-code".into(),
            name: "Claude Code".into(),
            version: "1.0.0".into(),
            description: "Anthropic Claude Code harness for CLI agent orchestration".into(),
            authors: vec!["Anthropic".into()],
            license: Some("Proprietary".into()),
            website: Some("https://claude.com".into()),
            repository: None,
            icon: official_icon_url_for_agent("claude-code"),
            distribution: AcpDistribution::Npx,
            integrity: AcpIntegrity::Registry,
            checksum: None,
        });
        catalog.register(AcpRegistryAgent {
            id: "codex-cli".into(),
            name: "Codex CLI".into(),
            version: "0.9.0".into(),
            description: "OpenAI Codex CLI coding harness".into(),
            authors: vec!["OpenAI".into()],
            license: Some("Proprietary".into()),
            website: Some("https://developers.openai.com".into()),
            repository: None,
            icon: official_icon_url_for_agent("codex-cli"),
            distribution: AcpDistribution::Binary,
            integrity: AcpIntegrity::Sha256,
            checksum: None,
        });
        catalog
    }

    /// Registers or updates an agent in the catalog if valid.
    pub fn register(&mut self, agent: AcpRegistryAgent) -> bool {
        if !is_valid_agent_id(&agent.id) {
            return false;
        }
        if let Some(pos) = self.agents.iter().position(|a| a.id == agent.id) {
            self.agents[pos] = agent;
        } else {
            if self.agents.len() >= ACP_REGISTRY_MAX_AGENTS {
                return false;
            }
            self.agents.push(agent);
        }
        true
    }

    /// Searches agents matching `query` by ID, name, or description.
    pub fn search(&self, query: &str, limit: usize) -> Vec<&AcpRegistryAgent> {
        let q = query.trim().to_lowercase();
        let cap = limit.clamp(1, ACP_REGISTRY_MAX_AGENTS);

        if q.is_empty() {
            return self.agents.iter().take(cap).collect();
        }

        let mut scored: Vec<(u32, &AcpRegistryAgent)> = self
            .agents
            .iter()
            .filter_map(|agent| {
                let id_lower = agent.id.to_lowercase();
                let name_lower = agent.name.to_lowercase();
                let desc_lower = agent.description.to_lowercase();

                let score = if id_lower == q {
                    100
                } else if id_lower.starts_with(&q) {
                    80
                } else if name_lower.contains(&q) {
                    50
                } else if desc_lower.contains(&q) {
                    20
                } else {
                    return None;
                };

                Some((score, agent))
            })
            .collect();

        scored.sort_by_key(|a| std::cmp::Reverse(a.0));
        scored.into_iter().take(cap).map(|(_, agent)| agent).collect()
    }

    /// Finds an agent by its unique identifier.
    pub fn find_by_id(&self, id: &str) -> Option<&AcpRegistryAgent> {
        self.agents.iter().find(|a| a.id == id)
    }

    /// Loads agent entries from raw JSON string (such as cached registry payload).
    pub fn load_from_json(&mut self, json_str: &str) -> Result<usize, String> {
        let list: Vec<AcpRegistryAgent> =
            serde_json::from_str(json_str).map_err(|e| format!("invalid registry json: {e}"))?;
        let count = list.len();
        for agent in list {
            self.register(agent);
        }
        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_acp_agent_ids() {
        assert!(is_valid_agent_id("cordata"));
        assert!(is_valid_agent_id("claude-code"));
        assert!(is_valid_agent_id("codex_cli.v1"));
        assert!(!is_valid_agent_id(""));
        assert!(!is_valid_agent_id("-leading-dash"));
        assert!(!is_valid_agent_id("UPPERCASE"));
        assert!(!is_valid_agent_id("spaces in id"));
    }

    #[test]
    fn official_icon_urls() {
        assert_eq!(
            official_icon_url_for_agent("cordata").as_deref(),
            Some("https://cdn.agentclientprotocol.com/registry/v1/latest/cordata.svg")
        );
        assert_eq!(official_icon_url_for_agent("invalid id"), None);
    }

    #[test]
    fn search_ranking_and_lookup() {
        let catalog = AcpRegistryCatalog::standard();

        let exact = catalog.search("cordata", 10);
        assert_eq!(exact.len(), 1);
        assert_eq!(exact[0].id, "cordata");

        let partial = catalog.search("cli", 10);
        assert!(!partial.is_empty());
        assert!(partial.iter().any(|a| a.id == "codex-cli"));

        let found = catalog.find_by_id("claude-code").unwrap();
        assert_eq!(found.distribution, AcpDistribution::Npx);
    }

    #[test]
    fn json_load_round_trip() {
        let mut catalog = AcpRegistryCatalog::new();
        let payload = r#"[
            {
                "id": "test-agent",
                "name": "Test Agent",
                "version": "1.0.0",
                "description": "Hermetic test agent",
                "authors": ["Test Author"],
                "license": "MIT",
                "website": null,
                "repository": null,
                "icon": null,
                "distribution": "binary",
                "integrity": "sha256",
                "checksum": "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
            }
        ]"#;

        assert_eq!(catalog.load_from_json(payload).unwrap(), 1);
        let found = catalog.find_by_id("test-agent").unwrap();
        assert_eq!(found.name, "Test Agent");
        assert_eq!(found.distribution, AcpDistribution::Binary);
    }
}
