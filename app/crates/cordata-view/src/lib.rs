//! Cordata's front-end state, shared by the terminal (`tui/crates/acp-tui`) and the native app (`app/crates/cordata-app`).
//!
//! Everything here is plain state and pure-ish functions over it: the conversation reducer ([`app::App::on_event`] turns
//! acp-core's events into messages, tool cards, pending approvals, gauges), the slash commands, the composer's input
//! model, menus, `@` mentions, `!` shell, custom commands, the settings schema and its editor and the run
//! board. Nothing draws: no ratatui, no GPUI, no terminal. Keys and pointer events arrive as [`key`]'s neutral types;
//! the full-screen overlays are the front end's, behind [`overlay::View`].
//!
//! Moved here from acp-tui unchanged in behaviour (docs/app.md §3 D3); acp-tui re-exports each module under its old
//! path, so its renderers and tests did not change.

pub mod app;
pub mod assistants;
pub mod commands;
pub mod config_ui;
pub mod custom;
pub mod extras;
pub mod input;
pub mod key;
pub mod mentions;
pub mod menu;
pub mod models;
pub mod overlay;
pub mod redact;
pub mod review;
pub mod settings;
pub mod shell;
pub mod telemetry;
pub mod tree;
pub mod view;
pub mod work;

/// Text as a front end may draw it: terminal escapes and control characters stripped, credential-shaped spans masked.
/// The backend masks before it sends and the daemon masks again; this runs last, on everything shown, so the rule
/// "a key is never drawn" does not depend on the other two (THREAT_MODEL.md, invariant "scrub before draw").
///
/// A frame redraws the same transcript many times a second (streaming, scrolling, hover), so a long text is cleaned
/// once and the result kept, keyed by the exact text: the rule still holds for every draw, the regexes run once.
pub fn shown(text: &str) -> String {
    if text.len() < SHOWN_CACHE_MIN {
        return clean(text);
    }
    SHOWN.with_borrow_mut(|cache| {
        let key = cache.key(text);
        if let Some(hit) = cache.map.get(&key) {
            return hit.clone();
        }
        let out = clean(text);
        if cache.bytes + out.len() > SHOWN_CACHE_BYTES {
            cache.map.clear();
            cache.bytes = 0;
        }
        cache.bytes += out.len();
        cache.map.insert(key, out.clone());
        out
    })
}

fn clean(text: &str) -> String {
    redact::scrub(&app::sanitize(text)).0
}

/// Shorter texts clean faster than they hash.
const SHOWN_CACHE_MIN: usize = 256;
/// Cleaned outputs held; a streaming reply leaves one entry per redraw, so the cache is dropped whole when full.
const SHOWN_CACHE_BYTES: usize = 16 << 20;

/// Keyed by a per-process SipHash of the text and its length — never the raw text, so no unmasked copy lingers here.
/// The random key makes a crafted collision impractical.
#[derive(Default)]
struct ShownCache {
    seed: std::hash::RandomState,
    map: std::collections::HashMap<(u64, usize), String>,
    bytes: usize,
}

impl ShownCache {
    fn key(&self, text: &str) -> (u64, usize) {
        use std::hash::BuildHasher as _;
        (self.seed.hash_one(text), text.len())
    }
}

thread_local! {
    static SHOWN: std::cell::RefCell<ShownCache> = std::cell::RefCell::default();
}

#[cfg(test)]
mod shown_tests {
    #[test]
    fn a_cached_text_is_still_masked() {
        let key = format!("sk-ant-api03-{}", "Q7zR9kMp3XwB".repeat(4));
        let long = format!("{}\nexport ANTHROPIC_API_KEY={key}\n", "fn main() {}\n".repeat(40));
        for _ in 0..3 {
            let out = super::shown(&long);
            assert!(!out.contains(&key) && out.contains("[REDACTED]"));
        }
        assert_eq!(super::shown("\x1b[31mred\x1b[0m"), "red");
    }

    /// One frame redraws every visible message: 20 replies of 5KB each, as streaming or scrolling redraws them.
    #[test]
    #[ignore]
    fn bench_frame_of_transcript() {
        let unit = "let token_count = request.auth_header.len(); // key lookup in cache\n";
        let msgs: Vec<String> = (0..20).map(|i| format!("reply {i}\n{}", unit.repeat(5 * 1024 / unit.len()))).collect();
        let time = |f: &dyn Fn()| {
            let mut t: Vec<f64> = (0..31).map(|_| { let s = std::time::Instant::now(); f(); s.elapsed().as_secs_f64() * 1e3 }).collect();
            t.sort_by(f64::total_cmp);
            t[15]
        };
        let uncached = time(&|| for m in &msgs { std::hint::black_box(super::clean(m)); });
        let cached = time(&|| for m in &msgs { std::hint::black_box(super::shown(m)); });
        println!("BENCH frame (20 x 5KB messages): scrub every draw {uncached:.3}ms | memoized {cached:.3}ms | 120Hz budget 8.333ms");
    }
}
