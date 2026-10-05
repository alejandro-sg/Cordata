//! Mask what a credential looks like, before the GUI draws it.
//!
//! The third layer of three (the agent masks registered secrets and credential-shaped spans in
//! `intervention_view.py`; the daemon masks again in `daemon/telemetry.py`). This one exists
//! because the rule "the GUI never shows a key" should not depend on the other two being right:
//! whatever arrives, this runs on it last.
//!
//! The same table as `backend/secret_patterns.py`, held to the same fixture
//! (`proto/fixtures/redact_cases.json`) by the test below. Rust's `regex` has no look-around, so
//! the two conditions Python expresses as look-aheads — "the value contains a digit, and a
//! letter" — are checked in code on the match. The effect is identical: `max_tokens = 4096` and
//! `token_count = compute_budget` stay readable, `password: hunter2hunter2hunter2` does not.

use std::sync::LazyLock;

use regex::{Captures, Regex};

pub const REDACTED: &str = "[REDACTED]";

struct Table {
    pem: Regex,
    authorization: Regex,
    bearer: Regex,
    assignment: Regex,
    jwt: Regex,
    formats: Regex,
}

static TABLE: LazyLock<Table> = LazyLock::new(|| Table {
    pem: Regex::new(r"(?s)-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----.*?(?:-----END [A-Z0-9 ]*PRIVATE KEY-----|\z)").unwrap(),
    authorization: Regex::new(r"(?i)\b(authorization\s*[:=]\s*(?:bearer|basic|token)\s+)[A-Za-z0-9._~+/=-]{8,}").unwrap(),
    bearer: Regex::new(r"(?i)\b(bearer\s+)([A-Za-z0-9._~+/=-]{16,})").unwrap(),
    assignment: Regex::new(
        r#"(?i)\b([A-Za-z0-9_.-]*(?:api[_-]?key|apikey|token|secret|passw(?:or)?d|credential|private[_-]?key|auth)[A-Za-z0-9_.-]*)(\s*[=:]\s*)(['"]?)([A-Za-z0-9/+_=.-]{16,})(['"]?)"#,
    )
    .unwrap(),
    jwt: Regex::new(r"\beyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\b").unwrap(),
    formats: Regex::new(
        r"\b(?:sk-ant-[A-Za-z0-9_-]{16,}|sk-[A-Za-z0-9_-]{20,}|ghp_[A-Za-z0-9]{20,}|gho_[A-Za-z0-9]{20,}|ghs_[A-Za-z0-9]{20,}|github_pat_[A-Za-z0-9_]{20,}|xox[abprs]-[A-Za-z0-9-]{10,}|AKIA[0-9A-Z]{16}|AIza[0-9A-Za-z_-]{30,}|hf_[A-Za-z0-9]{20,})\b",
    )
    .unwrap(),
});

fn has_digit_and_letter(value: &str) -> bool {
    value.bytes().any(|b| b.is_ascii_digit()) && value.bytes().any(|b| b.is_ascii_alphabetic())
}

fn replace_counting(regex: &Regex, text: &str, mut replace: impl FnMut(&Captures) -> Option<String>) -> (String, usize) {
    let mut count = 0;
    let out = regex.replace_all(text, |caps: &Captures| {
        match replace(caps) {
            Some(masked) => {
                count += 1;
                masked
            }
            None => caps[0].to_owned(),
        }
    });
    (out.into_owned(), count)
}

/// `text` with every credential-shaped span masked, and how many spans that was.
pub fn scrub(text: &str) -> (String, usize) {
    if text.is_empty() {
        return (String::new(), 0);
    }
    let table = &*TABLE;
    let mut total = 0;
    let mut out = text.to_owned();

    let (next, n) = replace_counting(&table.pem, &out, |_| Some("[REDACTED PRIVATE KEY]".to_owned()));
    out = next;
    total += n;
    let (next, n) = replace_counting(&table.authorization, &out, |c| Some(format!("{}{REDACTED}", &c[1])));
    out = next;
    total += n;
    let (next, n) = replace_counting(&table.bearer, &out, |c| has_digit(&c[2]).then(|| format!("{}{REDACTED}", &c[1])));
    out = next;
    total += n;
    let (next, n) = replace_counting(&table.assignment, &out, |c| {
        let (open, value, close) = (&c[3], &c[4], &c[5]);
        (open == close && has_digit_and_letter(value)).then(|| format!("{}{}{}{REDACTED}{}", &c[1], &c[2], open, close))
    });
    out = next;
    total += n;
    let (next, n) = replace_counting(&table.jwt, &out, |_| Some(REDACTED.to_owned()));
    out = next;
    total += n;
    let (next, n) = replace_counting(&table.formats, &out, |_| Some(REDACTED.to_owned()));
    out = next;
    total += n;
    (out, total)
}

fn has_digit(value: &str) -> bool {
    value.bytes().any(|b| b.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    const FIXTURE: &str = include_str!("../../../../proto/fixtures/redact_cases.json");

    #[test]
    fn masks_what_the_shared_fixture_says_to_mask() {
        let cases: Value = serde_json::from_str(FIXTURE).unwrap();
        for case in cases["masks"].as_array().unwrap() {
            let input = case["input"].as_str().unwrap();
            let (out, n) = scrub(input);
            for gone in case["gone"].as_array().unwrap() {
                assert!(!out.contains(gone.as_str().unwrap()), "{}: {gone} survived in {out:?}", case["name"]);
            }
            assert!(out.contains("[REDACTED") && n >= 1, "{}: nothing was masked ({out:?})", case["name"]);
        }
    }

    #[test]
    fn leaves_what_the_shared_fixture_says_to_leave() {
        let cases: Value = serde_json::from_str(FIXTURE).unwrap();
        for case in cases["keeps"].as_array().unwrap() {
            let input = case["input"].as_str().unwrap();
            let (out, n) = scrub(input);
            assert_eq!((out.as_str(), n), (input, 0), "{} was altered", case["name"]);
        }
    }

    #[test]
    fn counts_every_span_and_survives_empty_input() {
        let (out, n) = scrub("a=sk-ABCDEF0123456789ABCDEF0123456789 b=ghp_abcdefghijklmnopqrstuvwxyz0123456789");
        assert_eq!(n, 2, "{out}");
        assert_eq!(scrub(""), (String::new(), 0));
    }
}
