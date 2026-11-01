//! The API key, in the login Keychain and nowhere else (Settings → Account). The item is the one the backend already
//! reads at start, so a key saved here
//! is used by the next backend that starts — no file, no environment variable, no argv.
//!
//! The window never shows the key: only whether one is stored and its last four characters.

pub const SERVICE: &str = "cordata-anthropic-api-key";

fn account() -> String {
    std::env::var("USER").unwrap_or_else(|_| "cordata".into())
}

/// `sk-…abcd`, or `None` when no key is stored.
pub fn masked() -> Option<String> {
    let key = read()?;
    let tail: String = key.chars().rev().take(4).collect::<Vec<_>>().into_iter().rev().collect();
    let head: String = key.chars().take(3).collect();
    Some(format!("{head}…{tail}"))
}

#[cfg(target_os = "macos")]
fn read() -> Option<String> {
    let bytes = security_framework::passwords::get_generic_password(SERVICE, &account()).ok()?;
    let text = String::from_utf8(bytes).ok()?;
    (!text.trim().is_empty()).then_some(text)
}

#[cfg(target_os = "macos")]
pub fn store(key: &str) -> Result<(), String> {
    let key = key.trim();
    if key.is_empty() || key.len() > 512 || key.chars().any(char::is_control) {
        return Err("that does not look like an API key".into());
    }
    security_framework::passwords::set_generic_password(SERVICE, &account(), key.as_bytes()).map_err(|e| e.to_string())
}

#[cfg(target_os = "macos")]
pub fn forget() -> Result<(), String> {
    match security_framework::passwords::delete_generic_password(SERVICE, &account()) {
        Ok(()) => Ok(()),
        // Nothing stored is the state asked for.
        Err(error) if error.code() == -25300 => Ok(()),
        Err(error) => Err(error.to_string()),
    }
}

#[cfg(not(target_os = "macos"))]
fn read() -> Option<String> {
    None
}

#[cfg(not(target_os = "macos"))]
pub fn store(_key: &str) -> Result<(), String> {
    Err("the Keychain is macOS-only".into())
}

#[cfg(not(target_os = "macos"))]
pub fn forget() -> Result<(), String> {
    Ok(())
}
