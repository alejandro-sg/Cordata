//! Which applications the agent may never read, mirrored with the daemon.
//!
//! The capture policy is enforced twice. `harness-capture` decides what is *captured* — a denylisted
//! app never has its window read, so the pixels and the title never exist anywhere. The daemon decides
//! what is *forwarded to a model*, because a gRPC client other than this one could skip ours: a
//! scripted caller could send a password manager's window by never consulting this list at all. The
//! daemon only ever tightens, so this list is the floor, not the ceiling — a person's own allowlist
//! can never re-admit one of these.
//!
//! Kept equal to `backend/os_context.py::ALWAYS_DENY_BUNDLE_IDS` by `_os_context_smoke_test.py`, which
//! parses this file and compares, the way `MODES` is held to `acp_server._MODE_IDS`.

/// Matched on bundle id, never on the app's name: a name is localized and a person can rename a
/// bundle, and "1Password" typed into a field is not the app.
///
/// Ids checked against the apps' own `Info.plist`s where they could be (Keychain Access, Passwords on
/// this machine; KeePassXC, NordPass, Enpass from their published bundles). An entry also covers its
/// helpers — see [`is_denied`] — so `me.proton.pass` reaches the Electron app's `me.proton.pass.electron`.
pub const ALWAYS_DENY_BUNDLE_IDS: &[&str] = &[
    "com.1password.1password",
    "com.agilebits.onepassword7",
    "com.bitwarden.desktop",
    "com.dashlane.Dashlane",
    "com.lastpass.LastPass",
    "com.apple.keychainaccess",
    "com.apple.Passwords",
    "org.keepassxc.keepassxc",
    "com.nordsec.nordpass",
    "in.sinew.Enpass-Desktop",
    "me.proton.pass",
];

/// Is this an app whose content is never read, whatever any allowlist says?
///
/// Looser than the allowlist on purpose, because a miss here is the expensive mistake: the match
/// ignores ASCII case (a vendor that recases its id between releases stays denied) and covers every
/// id *under* an entry at a label boundary (`com.apple.Passwords.MenuBarExtra`, a 1Password helper),
/// but never a sibling that merely shares a prefix (`com.apple.PasswordsX` is not `com.apple.Passwords`).
/// `backend/os_context.py::is_denied` applies the same rule.
pub fn is_denied(bundle_id: &str) -> bool {
    !bundle_id.is_empty()
        && ALWAYS_DENY_BUNDLE_IDS.iter().any(|entry| {
            bundle_id.eq_ignore_ascii_case(entry)
                || (bundle_id.len() > entry.len()
                    && bundle_id.as_bytes()[entry.len()] == b'.'
                    && bundle_id.is_char_boundary(entry.len())
                    && bundle_id[..entry.len()].eq_ignore_ascii_case(entry))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn denies_by_bundle_id_not_by_name() {
        assert!(is_denied("com.1password.1password"));
        assert!(is_denied("com.apple.keychainaccess"));
        assert!(is_denied("com.bitwarden.desktop"));
    }

    #[test]
    fn empty_and_ordinary_apps_are_not_denied() {
        // No bundle id at all is "unknown", not "allowed": the caller treats an empty id as no capture.
        assert!(!is_denied(""));
        assert!(!is_denied("com.apple.Safari"));
        assert!(!is_denied("com.google.Chrome"));
        assert!(!is_denied("1Password"), "the app's name is not its bundle id");
    }

    #[test]
    fn a_recased_id_or_a_helper_is_still_denied_and_a_sibling_is_not() {
        assert!(is_denied("com.apple.passwords"), "case is not a way around the list");
        assert!(is_denied("ORG.KEEPASSXC.KEEPASSXC"));
        assert!(is_denied("me.proton.pass.electron"), "the Electron build under the vendor's id");
        assert!(is_denied("com.apple.Passwords.MenuBarExtra"), "a helper under a denied id");
        assert!(!is_denied("com.apple.PasswordsX"), "a sibling that only shares a prefix is not denied");
        assert!(!is_denied("me.proton.mail"), "nor is another app from the same vendor");
    }
}
