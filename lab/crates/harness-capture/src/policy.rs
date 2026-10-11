//! Who may be read, and by whose decision.
//!
//! Two lists, with opposite characters:
//!
//! * The **never-read list** is a constant. It is not in a settings file, it is not editable from the
//!   app, and it is not a default that a local file can override — a cloned repository must not be
//!   able to grant itself a password manager's window. It lives in `harness_client::policy` and is
//!   mirrored verbatim in the daemon (`backend/os_context.py`), because the Rust client decides what is *captured* while the daemon
//!   decides what reaches a model, and only one of those is under this process's control. A test in
//!   the Python suite holds the two lists to each other, the way `MODES` is held to `acp_server`.
//! * The **allowlist** is the person's. Empty means *deny everything*: a capture that has to be
//!   switched on for a named app is one a person meant to switch on.

/// Apps whose content is never context, whatever any allowlist says. One list for the whole client —
/// [`harness_client::policy`], the copy the Python suite holds `backend/os_context.py` to — so the
/// list that decides what is captured and the list the daemon re-checks cannot drift apart.
pub use harness_client::policy::ALWAYS_DENY_BUNDLE_IDS;

/// The apps the person has said may be read. Empty is the default and it means nothing may be read.
///
/// A second, smaller list rides along: apps the person has specifically denied. It cannot contradict
/// the never-read constant — that one is asked first and answers for itself — but it lets someone say
/// "not this app" without having to deny everything else, which is how a person withdraws consent
/// from one app without turning the feature off.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Allowlist {
    bundle_ids: Vec<String>,
    denied: Vec<String>,
}

impl Allowlist {
    /// An allowlist with nothing on it: the state a fresh install is in, and the state it stays in
    /// until a person adds an app by name.
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn of(bundle_ids: impl IntoIterator<Item = impl Into<String>>) -> Self {
        let mut list: Vec<String> = bundle_ids.into_iter().map(Into::into).collect();
        list.sort();
        list.dedup();
        Self { bundle_ids: list, denied: Vec::new() }
    }

    /// The apps the person denied. Read from the user's own settings file, never from a project's.
    pub fn denied(&self) -> &[String] {
        &self.denied
    }

    /// Deny one app. Returns whether it was not already denied. An app that is denied is also dropped
    /// from the allowlist, so the two lists cannot disagree about it.
    pub fn deny(&mut self, bundle_id: &str) -> bool {
        if !acceptable(bundle_id) || self.denies(bundle_id) {
            return false;
        }
        self.denied.push(bundle_id.to_owned());
        self.denied.sort();
        self.bundle_ids.retain(|allowed| allowed != bundle_id);
        true
    }

    pub fn denies(&self, bundle_id: &str) -> bool {
        self.denied.iter().any(|denied| denied == bundle_id)
    }

    pub fn is_empty(&self) -> bool {
        self.bundle_ids.is_empty()
    }

    pub fn contains(&self, bundle_id: &str) -> bool {
        self.bundle_ids.iter().any(|allowed| allowed == bundle_id)
    }

    /// Add an app, returning whether it was not already there. The "always allow" button in the deck
    /// is this call.
    pub fn allow(&mut self, bundle_id: &str) -> bool {
        if !acceptable(bundle_id) || self.contains(bundle_id) {
            return false;
        }
        self.bundle_ids.push(bundle_id.to_owned());
        // Allowing an app the person had denied is a change of mind, and the denial goes with it —
        // otherwise the two lists would say opposite things and the deny would win silently.
        self.denied.retain(|denied| denied != bundle_id);
        true
    }

    pub fn remove(&mut self, bundle_id: &str) -> bool {
        let before = self.bundle_ids.len();
        self.bundle_ids.retain(|allowed| allowed != bundle_id);
        self.bundle_ids.len() != before
    }

    pub fn bundle_ids(&self) -> &[String] {
        &self.bundle_ids
    }
}

/// May this value be an app in the allowlist at all?
///
/// Bundle-id shaped, and nothing else: reverse-DNS — at least two dot-separated labels, each a letter
/// or digit followed by letters, digits, dashes or dots. The check exists so no free-form string can
/// be smuggled into a list that will be matched against a bundle id — a path, a shell fragment, a
/// wildcard, or an empty string that would match everything.
pub fn acceptable(bundle_id: &str) -> bool {
    let labels: Vec<&str> = bundle_id.split('.').collect();
    labels.len() >= 2
        && labels.iter().all(|label| {
            !label.is_empty()
                && label.chars().next().is_some_and(|first| first.is_ascii_alphanumeric())
                && label.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
        })
}

/// The answer for one app.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Allow,
    /// Why not, in a sentence a person reads on the deck rather than in a log.
    Refuse(String),
}

impl Verdict {
    pub fn is_allowed(&self) -> bool {
        matches!(self, Verdict::Allow)
    }
}

/// May this app's context be read? Checked in this order, and the order is the policy: the never-read
/// list is *never* overridable, so it is asked first and nothing the person sets can answer for it.
pub fn verdict(bundle_id: &str, allowlist: &Allowlist) -> Verdict {
    if bundle_id.is_empty() {
        return Verdict::Refuse("the frontmost app has no bundle id, so it cannot be identified and is never read".into());
    }
    if is_never_read(bundle_id) {
        return Verdict::Refuse(format!("{bundle_id} is a secret-bearing app and is never read, whatever the allowlist says"));
    }
    if allowlist.denies(bundle_id) {
        return Verdict::Refuse(format!("you denied {bundle_id} — allow it again from the deck to read it"));
    }
    if allowlist.contains(bundle_id) {
        return Verdict::Allow;
    }
    if allowlist.is_empty() {
        return Verdict::Refuse(format!("nothing is allowed to be read yet — {bundle_id} can be allowed from the deck"));
    }
    Verdict::Refuse(format!("{bundle_id} is not on the list of apps you allowed"))
}

/// Is this app on the never-read list? The daemon asks the same question of the same names, by the
/// same rule (case-insensitive, helpers under an entry included).
pub fn is_never_read(bundle_id: &str) -> bool {
    harness_client::policy::is_denied(bundle_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_allowlist_reads_nothing() {
        let allowlist = Allowlist::empty();
        let verdict = verdict("com.apple.Safari", &allowlist);
        assert!(!verdict.is_allowed(), "deny by default is the whole design");
        assert!(matches!(&verdict, Verdict::Refuse(reason) if reason.contains("nothing is allowed")), "and it says so in words: {verdict:?}");
    }

    #[test]
    fn the_never_read_list_wins_over_anything_the_person_allows() {
        // Even if a person adds it — through a settings file, a stale copy, or a bug — it is refused.
        let allowlist = Allowlist::of(["com.1password.1password"]);
        let verdict = verdict("com.1password.1password", &allowlist);
        assert!(!verdict.is_allowed());
        assert!(matches!(&verdict, Verdict::Refuse(reason) if reason.contains("never read")), "the reason names the rule, not the person: {verdict:?}");
    }

    #[test]
    fn a_denied_app_is_refused_and_allowing_it_again_lifts_the_denial() {
        // The per-app "no" a person can say without turning the whole feature off.
        let mut allowlist = Allowlist::of(["com.apple.Safari", "com.apple.mail"]);
        assert!(allowlist.deny("com.apple.mail"));
        assert!(!verdict("com.apple.mail", &allowlist).is_allowed(), "the denial is what answers");
        assert!(verdict("com.apple.Safari", &allowlist).is_allowed(), "and it takes only its own app with it");
        assert!(!allowlist.contains("com.apple.mail"), "the two lists cannot disagree about one app");
        assert!(allowlist.allow("com.apple.mail"), "changing your mind is one call");
        assert!(verdict("com.apple.mail", &allowlist).is_allowed());
        assert!(!allowlist.denies("com.apple.mail"), "and the denial went with it");
    }

    #[test]
    fn the_never_read_list_is_asked_before_the_denials_are() {
        // Both refuse, but only one of them is a rule rather than a preference, and the sentence a
        // person reads must be the rule.
        let mut allowlist = Allowlist::of(["com.apple.Safari"]);
        allowlist.deny("com.1password.1password");
        let verdict = verdict("com.1password.1password", &allowlist);
        assert!(matches!(&verdict, Verdict::Refuse(reason) if reason.contains("never read")), "{verdict:?}");
    }

    #[test]
    fn a_denial_only_accepts_bundle_id_shaped_values() {
        let mut allowlist = Allowlist::empty();
        assert!(!allowlist.deny(""), "an empty denial would match everything");
        assert!(!allowlist.deny("not a bundle id"));
        assert!(allowlist.denied().is_empty());
    }

    #[test]
    fn a_named_app_is_allowed_and_an_unnamed_one_is_not() {
        let allowlist = Allowlist::of(["com.apple.Safari", "com.microsoft.VSCode"]);
        assert!(verdict("com.apple.Safari", &allowlist).is_allowed());
        assert!(verdict("com.microsoft.VSCode", &allowlist).is_allowed());
        assert!(!verdict("com.apple.mail", &allowlist).is_allowed(), "the list is the list");
    }

    #[test]
    fn an_app_with_no_bundle_id_is_never_read() {
        // Some processes have none. Nothing to ask the allowlist about, so it is refused rather than
        // falling through to a name-only capture.
        assert!(!verdict("", &Allowlist::of(["com.apple.Safari"])).is_allowed());
    }

    #[test]
    fn the_allowlist_only_accepts_bundle_id_shaped_values() {
        let mut allowlist = Allowlist::empty();
        for nonsense in ["", "Safari", "/Applications/Safari.app", "com.*", "com.apple.Safari && rm -rf /", "com..Safari", ".com.apple"] {
            assert!(!allowlist.allow(nonsense), "{nonsense:?} must not become an entry");
        }
        assert!(allowlist.is_empty(), "and nothing was added behind the refusal");
        assert!(allowlist.allow("com.apple.Safari"));
        assert!(!allowlist.allow("com.apple.Safari"), "adding it twice changes nothing");
        assert_eq!(allowlist.bundle_ids(), ["com.apple.Safari".to_owned()]);
        assert!(allowlist.remove("com.apple.Safari"));
        assert!(allowlist.is_empty());
        assert!(!allowlist.remove("com.apple.Safari"), "removing what is not there is not a change");
    }

    #[test]
    fn the_allowlist_is_case_sensitive_because_bundle_ids_are() {
        // A case-insensitive match would let `com.apple.passwords` reach `com.apple.Passwords`.
        let allowlist = Allowlist::of(["com.apple.Safari"]);
        assert!(!verdict("com.apple.safari", &allowlist).is_allowed());
    }

    #[test]
    fn the_mirrored_list_has_no_duplicates_and_no_blanks() {
        // The Python suite holds its copy to this list; a duplicate here would be a difference there.
        let mut seen: Vec<&str> = Vec::new();
        for bundle_id in ALWAYS_DENY_BUNDLE_IDS {
            assert!(acceptable(bundle_id), "{bundle_id} is not bundle-id shaped");
            assert!(!seen.contains(bundle_id), "{bundle_id} appears twice");
            seen.push(bundle_id);
        }
    }
}
