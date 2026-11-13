//! Token economizer: an in-process, offline engine that keeps the tokens of a native agent session down.
//!
//! * [`toolprune`]   sends only the tool schemas a prompt is likely to need.
//! * [`microskills`] remembers per-repository quirks across sessions and injects a few of them.
//! * [`loopguard`]   nudges, then refuses, repeated identical tool calls and edit reversions.
//! * [`cache_align`] lays the prompt out so the prefix stays byte-identical between rounds.
//! * [`compactor`]   keeps the head and tail of long outputs and ages old ones into digests.
//!
//! Nothing here touches the network or a model; everything is a pure function of its inputs plus the
//! small JSON files under `~/.cordata/economizer/`.

pub mod cache_align;
pub mod compactor;
pub mod loopguard;
pub mod microskills;
pub mod profile;
pub mod toolprune;

pub use profile::{Profile, ProviderKind};

/// Rough token count (4 characters per token), enough to compare payload sizes.
pub fn estimate_tokens(text: &str) -> usize {
    text.chars().count().div_ceil(4)
}

/// `~/.cordata/economizer`, or `None` without a `HOME`.
pub fn default_state_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME").map(|home| std::path::PathBuf::from(home).join(".cordata/economizer"))
}

/// Writes `bytes` to `path` through a sibling temp file, so a crash never leaves a half-written store.
pub(crate) fn write_atomic(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

/// First 16 hex characters of the SHA-1 of `text`: a stable id for repository paths and offloaded files.
pub(crate) fn short_id(text: &str) -> String {
    use sha1::{Digest, Sha1};
    let digest = Sha1::digest(text.as_bytes());
    digest.iter().take(8).map(|b| format!("{b:02x}")).collect()
}
