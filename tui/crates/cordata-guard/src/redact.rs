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

/// Every pattern above needs one of these literals, so text without any cannot match: one pass of the regex crate's
/// SIMD literal search (Teddy) instead of the eight full scans. Keep in step with `NAME` and `formats`.
static MAY_HOLD_SECRET: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i:bearer|auth|key|token|secret|passw|credential)|sk-|ghp_|gho_|ghs_|github_pat_|xox|AKIA|AIza|hf_|eyJ|-----BEGIN").expect("static")
});

fn may_contain_secrets(text: &str) -> bool {
    MAY_HOLD_SECRET.is_match(text)
}

/// `text` with every credential-shaped span masked, and how many were masked.
pub fn scrub(text: &str) -> (String, usize) {
    if text.is_empty() || !may_contain_secrets(text) {
        return (text.to_owned(), 0);
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
    fn the_prefilter_admits_every_shape_the_patterns_mask() {
        for text in ["APIKEY=x", "PassWd", "Credential", "x-auth", "BEARER", "sk-1", "ghp_", "gho_", "ghs_", "github_pat_", "xoxb-", "AKIA", "AIza", "hf_", "eyJ", "-----BEGIN"] {
            assert!(may_contain_secrets(text), "{text}");
        }
        assert!(!may_contain_secrets("The quick brown fox jumps over the lazy dog."));
    }

    #[test]
    fn leaves_code_readable() {
        for keep in ["max_tokens = 4096", "token_count = compute_budget", "bearer lowercaseonlytokenvalue", "secret: onlyletterslongvalueabcdef", "plain text"] {
            assert_eq!(scrub(keep), (keep.to_owned(), 0), "{keep}");
        }
    }
}

/// `cargo test --release -p cordata-guard bench_ -- --ignored --nocapture` (scripts/bench.sh).
#[cfg(test)]
mod bench {
    use std::time::Instant;

    /// Code as a transcript carries it: identifiers named token/key/auth on most lines, no actual secret.
    pub(crate) fn code(bytes: usize) -> String {
        let unit = "let token_count = request.auth_header.len(); // key lookup in cache\nfn main() { println!(\"hello world\"); }\n";
        unit.repeat(bytes / unit.len() + 1)[..bytes].to_owned()
    }

    fn median_us(runs: usize, mut f: impl FnMut()) -> f64 {
        let mut t: Vec<f64> = (0..runs).map(|_| {
            let start = Instant::now();
            f();
            start.elapsed().as_secs_f64() * 1e6
        }).collect();
        t.sort_by(f64::total_cmp);
        t[runs / 2]
    }

    #[test]
    #[ignore]
    fn bench_scrub() {
        for kb in [1, 10, 50, 100] {
            let text = code(kb * 1024);
            let prose = "The quick brown fox jumps over the lazy dog. ".repeat(kb * 1024 / 45);
            let c = median_us(21, || drop(std::hint::black_box(super::scrub(&text))));
            let p = median_us(21, || drop(std::hint::black_box(super::scrub(&prose))));
            println!("BENCH scrub code {kb:>3}KB {c:>9.1}us | prose (pre-filter skips) {p:>7.1}us");
        }
    }
}
