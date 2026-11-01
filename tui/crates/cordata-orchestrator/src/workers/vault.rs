//! Egress for the coding CLIs (`backend/vault_proxy.py`, the part workers use): through the Agent Vault proxy, or not at
//! all. A CLI never gets a direct route out and never a provider key. Fail closed.

use std::collections::HashMap;
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use serde_json::Value;

pub const DEFAULT_ADDR: &str = "http://127.0.0.1:14322";
const LOCAL_NO_PROXY: &str = "127.0.0.1,localhost,::1";
const HEALTH_TIMEOUT: Duration = Duration::from_millis(500);

pub const REFUSED_DIRECT: &str = "Refused: coding CLIs never run with direct network access. Turn on vault.enabled and start the Agent Vault proxy.";

/// `vault.*` settings, however they were loaded (see `cordata-doctor`'s layered loader).
pub trait Prefs {
    fn get(&self, dotted: &str) -> Option<Value>;
}

/// Preferences as one merged JSON object: `{"vault": {"enabled": true}}` or flat `{"vault.enabled": true}`.
pub struct JsonPrefs(pub Value);

impl Prefs for JsonPrefs {
    fn get(&self, dotted: &str) -> Option<Value> {
        if let Some(v) = self.0.get(dotted) {
            return Some(v.clone());
        }
        dotted.split('.').try_fold(&self.0, |v, part| v.get(part)).cloned()
    }
}

/// `("127.0.0.1", 14322)` from `http://127.0.0.1:14322`, `127.0.0.1:14322` or `https://vault.internal`.
pub fn split_addr(addr: &str) -> Option<(String, u16)> {
    let text = addr.trim();
    if text.is_empty() {
        return None;
    }
    let (scheme, rest) = text.split_once("//").map_or(("", text), |(s, r)| (s, r));
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let authority = authority.rsplit_once('@').map_or(authority, |(_, a)| a);
    let default = if scheme.starts_with("https") { 443 } else { 80 };
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) if !h.ends_with(']') || h.starts_with('[') => (h.trim_matches(['[', ']']), p.parse::<u16>().ok()?),
        _ => (authority.trim_matches(['[', ']']), default),
    };
    (!host.is_empty()).then(|| (host.to_owned(), port))
}

/// Whether something is accepting connections at the vault's proxy address (a TCP connect, not an HTTP request).
pub fn check_health(addr: &str) -> bool {
    let Some((host, port)) = split_addr(addr) else { return false };
    (host.as_str(), port).to_socket_addrs().ok().into_iter().flatten().any(|a| TcpStream::connect_timeout(&a, HEALTH_TIMEOUT).is_ok())
}

fn merged_no_proxy(environ: &HashMap<String, String>) -> String {
    let mut entries: Vec<String> = Vec::new();
    for raw in [environ.get("NO_PROXY").map(String::as_str), environ.get("no_proxy").map(String::as_str), Some(LOCAL_NO_PROXY)].into_iter().flatten() {
        for item in raw.replace(' ', ",").split(',') {
            if !item.is_empty() && !entries.iter().any(|e| e == item) {
                entries.push(item.to_owned());
            }
        }
    }
    entries.join(",")
}

/// The environment a subprocess needs to reach the network through the vault (no credential placeholders: a coding
/// CLI holds none).
pub fn build_proxy_env(addr: &str, environ: &HashMap<String, String>) -> Vec<(String, String)> {
    let target = addr.trim_end_matches('/').to_owned();
    let no_proxy = merged_no_proxy(environ);
    let mut out: Vec<(String, String)> = ["HTTP_PROXY", "HTTPS_PROXY", "http_proxy", "https_proxy"].iter().map(|k| ((*k).to_owned(), target.clone())).collect();
    out.extend([("NO_PROXY".into(), no_proxy.clone()), ("no_proxy".into(), no_proxy), ("NODE_USE_ENV_PROXY".into(), "1".into())]);
    if let Some(bundle) = environ.get("CORDATA_VAULT_CA").map(|s| s.trim()).filter(|s| !s.is_empty()) {
        for key in ["SSL_CERT_FILE", "REQUESTS_CA_BUNDLE", "CURL_CA_BUNDLE", "GIT_SSL_CAINFO", "NODE_EXTRA_CA_CERTS"] {
            out.push((key.into(), bundle.to_owned()));
        }
    }
    out
}

/// What a worker may do about the network: the extra environment to give the child, or why it must not run.
pub trait Egress: Send + Sync {
    fn env(&self) -> Result<Vec<(String, String)>, String>;
}

/// The Python rule, from preferences and an environment snapshot. `health` is injectable so tests need no socket.
pub struct VaultEgress {
    pub prefs: Box<dyn Prefs + Send + Sync>,
    pub environ: HashMap<String, String>,
    pub health: fn(&str) -> bool,
}

impl VaultEgress {
    pub fn new(prefs: impl Prefs + Send + Sync + 'static, environ: HashMap<String, String>) -> Self {
        Self { prefs: Box::new(prefs), environ, health: check_health }
    }
}

impl Egress for VaultEgress {
    fn env(&self) -> Result<Vec<(String, String)>, String> {
        if matches!(self.environ.get("CORDATA_VAULT_DIRECT").map(String::as_str), Some("1" | "true" | "yes")) {
            return Ok(Vec::new());
        }
        let truthy = |key: &str| self.prefs.get(key).and_then(|v| v.as_bool()).unwrap_or(false);
        if truthy("vault.allowDirect") || truthy("vault.allow_direct") {
            return Ok(Vec::new());
        }
        if !truthy("vault.enabled") {
            return Err(REFUSED_DIRECT.into());
        }
        let addr = self.prefs.get("vault.addr").and_then(|v| v.as_str().map(|s| s.trim().to_owned())).filter(|s| !s.is_empty()).unwrap_or_else(|| DEFAULT_ADDR.into());
        if (self.health)(&addr) {
            return Ok(build_proxy_env(&addr, &self.environ));
        }
        // Unreachable: `deny` (the default) refuses; `proxy` still routes through the address, without credentials.
        match self.prefs.get("vault.egressPolicy").and_then(|v| v.as_str().map(str::to_owned)).as_deref() {
            Some("proxy") => Ok(build_proxy_env(&addr, &self.environ)),
            _ => Err(format!("Refused: the vault at {addr} is unreachable and vault.egressPolicy is 'deny'")),
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn egress(prefs: Value, env: &[(&str, &str)], healthy: bool) -> Result<Vec<(String, String)>, String> {
        let mut e = VaultEgress::new(JsonPrefs(prefs), env.iter().map(|(k, v)| ((*k).into(), (*v).into())).collect());
        e.health = if healthy { |_| true } else { |_| false };
        e.env()
    }

    #[test]
    fn address_forms() {
        assert_eq!(split_addr("http://127.0.0.1:14322"), Some(("127.0.0.1".into(), 14322)));
        assert_eq!(split_addr("127.0.0.1:9"), Some(("127.0.0.1".into(), 9)));
        assert_eq!(split_addr("https://vault.internal"), Some(("vault.internal".into(), 443)));
        assert_eq!(split_addr("http://v.internal/x"), Some(("v.internal".into(), 80)));
        assert_eq!(split_addr("host:notaport"), None);
        assert_eq!(split_addr(""), None);
    }

    #[test]
    fn fail_closed_unless_explicitly_direct_or_vault_healthy() {
        assert_eq!(egress(json!({}), &[], true), Err(REFUSED_DIRECT.into()));
        assert_eq!(egress(json!({}), &[("CORDATA_VAULT_DIRECT", "1")], true), Ok(vec![]));
        assert_eq!(egress(json!({"vault": {"allowDirect": true}}), &[], false), Ok(vec![]));
        let on = json!({"vault": {"enabled": true}});
        assert!(egress(on.clone(), &[], false).unwrap_err().contains("unreachable and vault.egressPolicy is 'deny'"));
        let env = egress(on, &[("NO_PROXY", "corp.internal")], true).unwrap();
        assert!(env.contains(&("HTTPS_PROXY".into(), DEFAULT_ADDR.into())));
        assert!(env.contains(&("NO_PROXY".into(), "corp.internal,127.0.0.1,localhost,::1".into())), "the user's exclusions stay: {env:?}");
        let routed = egress(json!({"vault": {"enabled": true, "egressPolicy": "proxy", "addr": "http://v:1/"}}), &[], false).unwrap();
        assert!(routed.contains(&("HTTP_PROXY".into(), "http://v:1".into())));
        let ca = egress(json!({"vault.enabled": true}), &[("CORDATA_VAULT_CA", "/ca.pem")], true).unwrap();
        assert!(ca.contains(&("NODE_EXTRA_CA_CERTS".into(), "/ca.pem".into())));
    }
}
