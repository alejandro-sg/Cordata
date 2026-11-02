//! Pattern-based secret masking: what a credential *looks like*, with no registry of known secrets. Conservative about
//! false positives on code, complete on the formats that actually leak. Used for orchestrator audit output and for the
//! chat sessions Cordata keeps on disk.
//!
//! Rust's `regex` has neither look-ahead nor back-references, so the two patterns that use them are expressed as a
//! match plus a check on the captured run, and the optional-quote form is three patterns (`"v"`, `'v'`, bare).

use std::sync::LazyLock;

use regex::{Captures, Regex};

pub const REDACTED: &str = "[REDACTED]";

const NAME: &str = r"[A-Za-z0-9_.-]*(?:api[_-]?key|apikey|token|secret|passw(?:or)?d|credential|private[_-]?key|auth)[A-Za-z0-9_.-]*";

struct Patterns {
    private_key: Regex,
    authorization: Regex,
    bearer: Regex,
    named: [(Regex, &'static str); 3],
    jwt: Regex,
    formats: Regex,
}

static PATTERNS: LazyLock<Patterns> = LazyLock::new(|| {
    let named = |quote: &'static str| {
        let q = if quote.is_empty() { String::new() } else { quote.to_owned() };
        // `["']?` lets a quoted key through: `"api_key": "…"`, `{'password':'…'}`.
        (Regex::new(&format!(r#"(?i)\b({NAME})(["']?\s*[=:]\s*){q}([A-Za-z0-9/+_=.-]{{16,}}){q}"#)).expect("static regex"), quote)
    };
    Patterns {
        private_key: Regex::new(r"(?s)-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----.*?(?:-----END [A-Z0-9 ]*PRIVATE KEY-----|\z)").expect("static"),
        authorization: Regex::new(r"(?i)\b(authorization\s*[:=]\s*(?:bearer|basic|token)\s+)[A-Za-z0-9._~+/=-]{8,}").expect("static"),
        bearer: Regex::new(r"(?i)\b(bearer\s+)([A-Za-z0-9._~+/=-]{16,})").expect("static"),
        named: [named("\""), named("'"), named("")],
        jwt: Regex::new(r"\beyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\b").expect("static"),
        formats: Regex::new(concat!(
            r"\b(?:sk-ant-[A-Za-z0-9_-]{16,}|sk-[A-Za-z0-9_-]{20,}|ghp_[A-Za-z0-9]{20,}|gho_[A-Za-z0-9]{20,}|ghs_[A-Za-z0-9]{20,}|github_pat_[A-Za-z0-9_]{20,}",
            r"|xox[abprs]-[A-Za-z0-9-]{10,}|AKIA[0-9A-Z]{16}|AIza[0-9A-Za-z_-]{30,}|hf_[A-Za-z0-9]{20,})\b"
        ))
        .expect("static"),
    }
});

/// Replaces matches with `f(captures)` when it returns `Some`, else leaves them; counts replacements.
fn replace_where(re: &Regex, text: &str, count: &mut usize, f: impl Fn(&Captures) -> Option<String>) -> String {
    re.replace_all(text, |caps: &Captures| match f(caps) {
        Some(replacement) => {
            *count += 1;
            replacement
        }
        None => caps[0].to_owned(),
    })
    .into_owned()
}

/// `text` with every credential-shaped span masked, and how many were masked.
pub fn scrub(text: &str) -> (String, usize) {
    if text.is_empty() {
        return (String::new(), 0);
    }
    let p = &*PATTERNS;
    let mut count = 0;
    let mut out = replace_where(&p.private_key, text, &mut count, |_| Some("[REDACTED PRIVATE KEY]".into()));
    out = replace_where(&p.authorization, &out, &mut count, |c| Some(format!("{}{REDACTED}", &c[1])));
    // The look-ahead `(?=[A-Za-z0-9._~+/=-]*\d)` scans the same maximal run the token takes.
    out = replace_where(&p.bearer, &out, &mut count, |c| c[2].chars().any(|ch| ch.is_ascii_digit()).then(|| format!("{}{REDACTED}", &c[1])));
    for (re, quote) in &p.named {
        out = replace_where(re, &out, &mut count, |c| {
            let value = &c[3];
            (value.chars().any(|ch| ch.is_ascii_digit()) && value.chars().any(|ch| ch.is_ascii_alphabetic())).then(|| format!("{}{}{quote}{REDACTED}{quote}", &c[1], &c[2]))
        });
    }
    out = replace_where(&p.jwt, &out, &mut count, |_| Some(REDACTED.into()));
    out = replace_where(&p.formats, &out, &mut count, |_| Some(REDACTED.into()));
    (out, count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_known_formats_and_labelled_values() {
        let key = format!("sk-ant-api03-{}", "Z".repeat(40));
        let (out, n) = scrub(&format!("key {key} ok"));
        assert_eq!((out.as_str(), n), ("key [REDACTED] ok", 1));
        assert_eq!(scrub("Authorization: Bearer abcdefgh12345678").0, "Authorization: Bearer [REDACTED]");
        assert_eq!(scrub("API_KEY=\"abcd1234efgh5678ijkl\"").0, "API_KEY=\"[REDACTED]\"");
        assert_eq!(scrub("password: 'abcd1234efgh5678'").0, "password: '[REDACTED]'");
        assert_eq!(scrub("token = abcd1234efgh5678").0, "token = [REDACTED]");
        assert_eq!(scrub(r#"{"api_key": "abcd1234efgh5678ijkl"}"#).0, r#"{"api_key": "[REDACTED]"}"#);
        assert_eq!(scrub(r#"{"password":"abcd1234efgh5678"}"#).0, r#"{"password":"[REDACTED]"}"#);
        let pem = "-----BEGIN RSA PRIVATE KEY-----\nMIIB\n-----END RSA PRIVATE KEY-----";
        assert_eq!(scrub(pem).0, "[REDACTED PRIVATE KEY]");
        assert_eq!(scrub("-----BEGIN PRIVATE KEY-----\ncut off").0, "[REDACTED PRIVATE KEY]");
    }

    #[test]
    fn leaves_code_readable() {
        for keep in ["max_tokens = 4096", "token_count = compute_budget", "bearer lowercaseonlytokenvalue", "secret: onlyletterslongvalueabcdef", "plain text"] {
            assert_eq!(scrub(keep), (keep.to_owned(), 0), "{keep}");
        }
    }
}
