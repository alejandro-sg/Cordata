//! Refuses to let a real credential into git: provider-shaped tokens (Anthropic, OpenAI-style `sk-`, GitHub, AWS, Google,
//! Slack, Hugging Face, JWTs, private-key blocks). Tests and redaction fixtures contain deliberately fake ones, so a match
//! is let through when it is plainly synthetic: an alphabet run, a repeated character, or a word like "fake".
//! Findings carry file and line, never the value.

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use regex::Regex;

const PATTERNS: &[(&str, &str)] = &[
    ("Anthropic key", r"sk-ant-[A-Za-z0-9_\-]{10,}"),
    ("OpenAI-style key", r"\bsk-[A-Za-z0-9_\-]{20,}"),
    ("GitHub token", r"\b(?:gh[pousr]_[A-Za-z0-9_]{20,}|github_pat_[A-Za-z0-9_]{20,})"),
    ("AWS access key id", r"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b"),
    ("Google API key", r"\bAIza[0-9A-Za-z_\-]{35}\b"),
    ("Slack token", r"\bxox[abprs]-[A-Za-z0-9\-]{10,}"),
    ("Hugging Face token", r"\bhf_[A-Za-z0-9]{30,}\b"),
    ("JWT", r"\beyJ[A-Za-z0-9_\-]{10,}\.eyJ[A-Za-z0-9_\-]{10,}\.[A-Za-z0-9_\-]{10,}"),
    ("credentials in a URL", r"://([^/\s:@]+):([^/\s:@]{8,})@([A-Za-z0-9.\-]+)"),
];

static COMPILED: LazyLock<Vec<(&'static str, Regex)>> =
    LazyLock::new(|| PATTERNS.iter().map(|(name, rx)| (*name, Regex::new(rx).expect("pattern"))).collect());
static FAKE_WORDS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(example|sample|placeholder|dummy|fake|test|planted|leak|secret|should|redact|canary|your[_-]|changeme|xxxx|aaaa|1234|abcd|0000|<[^>]+>|\$\{|\.\.\.)")
        .expect("fake words")
});
static PREFIX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(sk-ant-|sk-|gh[pousr]_|github_pat_|xox[abprs]-|hf_|AKIA|ASIA|AIza|eyJ)").expect("prefix"));
static PEM: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"-----BEGIN [A-Z ]*PRIVATE KEY-----(?:\\n|\\r|\s)*([A-Za-z0-9+/=]{40,})").expect("pem")
});
static SKIP_PATH: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(^|/)(Cargo\.lock|package-lock\.json|uv\.lock|.*\.min\.js)$").expect("skip"));
static CREDENTIAL_FILE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(^|/)\.env($|\.[^/]*$)|\.(pem|p12|pfx|key)$|id_(rsa|ed25519)").expect("credential file")
});

pub fn skip_path(path: &str) -> bool {
    SKIP_PATH.is_match(path)
}

/// `.env` and `.env.local`, not `.env.example`; key and certificate files.
pub fn credential_file(path: &str) -> bool {
    CREDENTIAL_FILE.is_match(path) && !path.ends_with(".env.example")
}

fn entropy(text: &str) -> f64 {
    let mut counts: HashMap<char, usize> = HashMap::new();
    let mut total = 0usize;
    for c in text.chars() {
        *counts.entry(c).or_default() += 1;
        total += 1;
    }
    -counts.values().map(|&n| { let p = n as f64 / total as f64; p * p.log2() }).sum::<f64>()
}

/// True for the fakes tests use: they spell the alphabet, repeat a character, or say what they are.
fn looks_synthetic(token: &str) -> bool {
    if FAKE_WORDS.is_match(token) {
        return true;
    }
    let body = PREFIX.replace(token, "").into_owned();
    let chars: Vec<char> = body.chars().collect();
    if chars.len() < 8 {
        return true;
    }
    if token.starts_with("sk-ant-") && chars.len() < 40 {
        return true; // real Anthropic keys run past 90 characters; a short one was typed by hand for a test
    }
    if token.starts_with("eyJ") && token.contains(".eyJzdWIiOiIxMjM0NTY3ODkwIn") {
        return true; // the jwt.io sample, in every JWT tutorial
    }
    let pairs = chars.windows(2).count().max(1);
    let steps = chars.windows(2).filter(|w| w[1] as i64 - w[0] as i64 == 1).count();
    if steps as f64 / pairs as f64 >= 0.5 {
        return true; // abcdefghij…, 0123456789
    }
    chars.iter().collect::<HashSet<_>>().len() <= 6 || entropy(&body) < 3.0
}

/// The URL pattern needs "not a local host", which the regex crate cannot say as a lookahead.
fn url_credentials_are_remote(caps: &regex::Captures) -> bool {
    let host = &caps[3];
    !(host.starts_with("localhost") || host.starts_with("127.0.0.1"))
}

/// `(line number, kind)` for each credential-shaped match in `text`.
pub fn findings_in(text: &str) -> Vec<(usize, &'static str)> {
    let mut found = Vec::new();
    for m in PEM.captures_iter(text) {
        if !looks_synthetic(&m[1]) {
            let start = m.get(0).map_or(0, |g| g.start());
            found.push((text[..start].matches('\n').count() + 1, "private key"));
        }
    }
    for (index, line) in text.lines().enumerate() {
        if line.len() > 4000 {
            continue;
        }
        for (name, rx) in COMPILED.iter() {
            for caps in rx.captures_iter(line) {
                if *name == "credentials in a URL" && !url_credentials_are_remote(&caps) {
                    continue;
                }
                if !looks_synthetic(&caps[0]) {
                    found.push((index + 1, *name));
                }
            }
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn real_looking(prefix: &str, n: usize) -> String {
        // Varied enough to defeat the synthetic heuristics, without being any provider's actual key.
        let pool = "q7Zr9KmP3xW5vB8nL2cT6yH4jD1sF0gR";
        format!("{prefix}{}", pool.chars().cycle().take(n).enumerate().map(|(i, c)| if i % 3 == 0 { c.to_ascii_uppercase() } else { c }).collect::<String>())
    }

    #[test]
    fn a_realistic_token_is_found_and_a_labelled_fake_is_not() {
        let line = format!("key = \"{}\"", real_looking("sk-ant-", 95));
        assert!(findings_in(&line).contains(&(1, "Anthropic key")));
        assert!(findings_in("key = \"sk-ant-api03-fakefakefakefakefakefakefakefakefakefakefakefake\"").is_empty());
        assert!(findings_in("sk-abcdefghijklmnopqrstuvwxyz").is_empty());
    }

    #[test]
    fn local_urls_pass_and_remote_ones_with_passwords_fail() {
        assert!(findings_in(&format!("postgres://user:{}@localhost:5432/db", "Zk3p9QxW7mTn")).is_empty());
        assert_eq!(findings_in(&format!("postgres://user:{}@db.prod.net/db", "Zk3p9QxW7mTn")), vec![(1, "credentials in a URL")]);
    }

    #[test]
    fn a_private_key_header_alone_is_prose() {
        assert!(findings_in("-----BEGIN PRIVATE KEY----- is how it starts").is_empty());
    }

    #[test]
    fn credential_files_but_not_examples() {
        assert!(credential_file(".env") && credential_file("a/.env.local") && credential_file("k.pem"));
        assert!(!credential_file(".env.example") && !credential_file("src/main.rs"));
    }
}
