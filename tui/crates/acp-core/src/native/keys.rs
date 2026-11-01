//! API keys, from the login Keychain first, then the provider's usual environment variable.
//! Same items the Python backend and `cordata-app` read (`scripts/store-key.sh`); nothing is ever
//! written here, and the key never touches argv beyond the lookup's service name.

use std::process::Command;

/// Keychain service for a provider's key.
pub(crate) fn keychain_service(provider: &str) -> Option<&'static str> {
    match provider {
        "anthropic" => Some("cordata-anthropic-api-key"),
        "ollama" | "ollama_cloud" => Some("cordata-ollama-api-key"),
        _ => None,
    }
}

/// The environment variable `genai` itself would read for this provider.
pub(crate) fn env_var(provider: &str) -> Option<&'static str> {
    match provider {
        "anthropic" => Some("ANTHROPIC_API_KEY"),
        "openai" => Some("OPENAI_API_KEY"),
        "gemini" => Some("GEMINI_API_KEY"),
        "ollama_cloud" => Some("OLLAMA_API_KEY"),
        _ => None,
    }
}

/// Environment first (an explicit export is a deliberate override), then the Keychain.
pub(crate) fn resolve(provider: &str) -> Option<String> {
    if let Some(name) = env_var(provider) {
        if let Ok(value) = std::env::var(name) {
            if !value.trim().is_empty() {
                return Some(value);
            }
        }
    }
    keychain(keychain_service(provider)?)
}

#[cfg(target_os = "macos")]
fn keychain(service: &str) -> Option<String> {
    let account = std::env::var("USER").ok()?;
    let out = Command::new("/usr/bin/security")
        .args(["find-generic-password", "-a", &account, "-s", service, "-w"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let key = String::from_utf8(out.stdout).ok()?.trim().to_owned();
    (!key.is_empty()).then_some(key)
}

#[cfg(not(target_os = "macos"))]
fn keychain(_service: &str) -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn services_match_backend_items() {
        assert_eq!(keychain_service("anthropic"), Some("cordata-anthropic-api-key"));
        assert_eq!(keychain_service("ollama"), Some("cordata-ollama-api-key"));
        assert_eq!(keychain_service("openai"), None);
    }

    #[test]
    fn local_ollama_needs_no_key() {
        assert_eq!(env_var("ollama"), None);
    }
}
