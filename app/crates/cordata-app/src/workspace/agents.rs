//! The Agents tab: every coding assistant installed on this Mac, which of them Cordata may delegate to, and how much
//! of each one's quota is left and when it resets. Delegation is `orchestrator.workers` in the user's settings; the
//! quota is the orchestrator's ledger (`cordata-run status --json`: metered use, and the windows the providers report
//! back). Assistants the orchestrator cannot drive yet open in the dock terminal, where their state is followed.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use cordata_theme::tokens as t;
use cordata_view::assistants::{self, Found, KNOWN};
use cordata_view::settings::Store;
use gpui::prelude::FluentBuilder as _;
use gpui::{AnyElement, Context, FontWeight, InteractiveElement as _, IntoElement, ParentElement, SharedString, StatefulInteractiveElement as _, Styled, Window, div, px};
use serde_json::Value;

use super::{DockTab, Overlay, Workspace};
use crate::theme::{c, line, mono, tk};
use crate::ui::{self, Kind};

/// One quota pool, as the ledger reports it.
#[derive(Debug, Clone, PartialEq)]
pub struct Pool {
    pub pool_id: String,
    pub family: String,
    pub pct: f64,
    pub level: String,
    pub window_seconds: i64,
    pub resets_at: Option<f64>,
    pub limited_until: Option<f64>,
    pub source: String,
}

impl Pool {
    fn from_json(v: &Value) -> Option<Self> {
        Some(Self {
            pool_id: v.get("pool_id")?.as_str()?.to_owned(),
            family: v.get("family")?.as_str()?.to_owned(),
            pct: v.get("pct").and_then(Value::as_f64).unwrap_or(0.0),
            level: v.get("level").and_then(Value::as_str).unwrap_or("ok").to_owned(),
            window_seconds: v.get("window_seconds").and_then(Value::as_i64).unwrap_or(0),
            resets_at: v.get("resets_at").and_then(Value::as_f64),
            limited_until: v.get("limited_until").and_then(Value::as_f64),
            source: v.get("source").and_then(Value::as_str).unwrap_or("meter").to_owned(),
        })
    }
}

#[derive(Default)]
pub struct Agents {
    pub found: Option<Vec<Found>>,
    pub pools: Vec<Pool>,
    pub enabled: Vec<String>,
    pub loading: bool,
    /// Why the quota could not be read, or a delegation change that failed.
    pub note: Option<String>,
}

/// `cordata-run`: `$CORDATA_RUN_BIN`, the usual install folders, or this checkout's release build.
pub(super) fn cordata_run() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("CORDATA_RUN_BIN").map(PathBuf::from).filter(|p| p.is_file()) {
        return Some(path);
    }
    let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    let on_path = assistants::search_dirs(std::env::var("PATH").ok().as_deref(), &home).into_iter().map(|d| d.join("cordata-run")).find(|p| p.is_file());
    on_path.or_else(|| {
        let exe = std::env::current_exe().ok()?;
        exe.ancestors().map(|dir| dir.join("tui/target/release/cordata-run")).find(|p| p.is_file())
    })
}

/// The ledger's pools; an empty list with a reason when `cordata-run` is missing or fails.
fn read_pools(cwd: &Path) -> (Vec<Pool>, Option<String>) {
    let Some(bin) = cordata_run() else { return (Vec::new(), Some("quota needs cordata-run — make tui-build".into())) };
    match std::process::Command::new(bin).args(["status", "--json"]).current_dir(cwd).stdin(std::process::Stdio::null()).output() {
        Ok(out) if out.status.success() => match serde_json::from_slice::<Value>(&out.stdout) {
            Ok(data) => (data.get("pools").and_then(Value::as_array).map(|p| p.iter().filter_map(Pool::from_json).collect()).unwrap_or_default(), None),
            Err(e) => (Vec::new(), Some(format!("could not read the ledger: {e}"))),
        },
        Ok(out) => (Vec::new(), Some(format!("cordata-run status failed: {}", String::from_utf8_lossy(&out.stderr).lines().next().unwrap_or("")))),
        Err(e) => (Vec::new(), Some(format!("could not run cordata-run: {e}"))),
    }
}

fn now() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64())
}

/// `in 1h 12m`, `in 4m`, `now`.
pub fn until(at: f64, now: f64) -> String {
    let s = (at - now).max(0.0) as u64;
    match s {
        0..60 => "now".into(),
        60..3600 => format!("in {}m", s / 60),
        3600..86_400 => format!("in {}h {:02}m", s / 3600, s % 3600 / 60),
        _ => format!("in {}d {}h", s / 86_400, s % 86_400 / 3600),
    }
}

fn window_name(seconds: i64) -> String {
    match seconds {
        0 => "window".into(),
        s if s % 86_400 == 0 => format!("{}d window", s / 86_400),
        s if s % 3600 == 0 => format!("{}h window", s / 3600),
        s => format!("{}m window", s / 60),
    }
}

/// What a pool line says: `38% of the 5h window · resets in 1h 12m`, or when it comes back.
pub fn pool_line(pool: &Pool, now: f64) -> String {
    if let Some(until_ts) = pool.limited_until.filter(|u| *u > now) {
        return format!("limited · back {}", until(until_ts, now));
    }
    let share = format!("{:.0}% of the {}", pool.pct * 100.0, window_name(pool.window_seconds));
    let source = if pool.source == "provider" { " (provider)" } else { "" };
    match pool.resets_at.filter(|r| *r > now) {
        Some(reset) if pool.pct > 0.0 => format!("{share}{source} · resets {}", until(reset, now)),
        _ if pool.pct <= 0.0 => "nothing used in this window".into(),
        _ => format!("{share}{source}"),
    }
}

impl Workspace {
    /// Detect the installed CLIs and read the ledger, off the UI thread.
    pub(super) fn load_agents(&mut self, cx: &mut Context<Self>) {
        let Some(runtime) = cx.try_global::<crate::session::Runtime>().map(|r| r.0.clone()) else { return };
        if self.agents.loading {
            return;
        }
        self.agents.loading = true;
        let store = Store::discover(&self.cwd);
        self.agents.enabled = assistants::enabled(&store.load());
        let cwd = self.cwd.clone();
        cx.spawn(async move |this, cx| {
            let (found, (pools, note)) = runtime.spawn_blocking(move || (assistants::detect(), read_pools(&cwd))).await.unwrap_or_default();
            let _ = this.update(cx, |this, cx| {
                this.agents.found = Some(found);
                this.agents.pools = pools;
                this.agents.note = note;
                this.agents.loading = false;
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn set_delegation(&mut self, family: &'static str, on: bool, cx: &mut Context<Self>) {
        match assistants::set_enabled(&Store::discover(&self.cwd), family, on) {
            Ok(families) => {
                self.agents.enabled = families;
                self.agents.note = None;
            }
            Err(error) => self.agents.note = Some(format!("could not save: {error}")),
        }
        cx.notify();
    }

    /// Start an assistant in the dock terminal (where its state is followed like any terminal agent).
    fn open_in_terminal(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        self.overlays.retain(|o| *o != Overlay::Settings);
        self.switch_dock(DockTab::Terminal, cx);
        self.ensure_terminal(cx);
        let focus = self.terminal.focus.clone();
        window.focus(&focus, cx);
        let quoted = format!("'{}'", path.display().to_string().replace('\'', r"'\''"));
        if !self.terminal_type(&format!("{quoted}\r")) {
            self.agents.note = Some("the terminal is not running".into());
        }
        cx.notify();
    }

    pub(super) fn agents_panel(&self, cx: &mut Context<Self>) -> AnyElement {
        let tk = tk(cx);
        let now = now();
        let found = self.agents.found.clone().unwrap_or_default();
        let label = |text: &'static str| div().px(px(14.)).pt(px(16.)).pb(px(6.)).text_size(px(t::TEXT_LABEL)).text_color(c(tk.text_faint)).child(text);
        let mut delegate: Vec<AnyElement> = Vec::new();
        for (ix, known) in KNOWN.iter().filter(|k| k.family.is_some()).enumerate() {
            let family = known.family.expect("filtered");
            let install = found.iter().find(|f| f.known.id == known.id);
            let on = self.agents.enabled.iter().any(|f| f == family);
            let pools: Vec<&Pool> = self.agents.pools.iter().filter(|p| p.family == family).collect();
            let detail = match install {
                Some(f) => f.version.as_deref().map(tidy_version).unwrap_or_else(|| f.path.display().to_string()),
                None if self.agents.found.is_none() => "looking…".into(),
                None => "not installed".into(),
            };
            let usable = install.is_some();
            delegate.push(
                div()
                    .id(("delegate", ix))
                    .px(px(14.))
                    .py(px(10.))
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .border_b_1()
                    .border_color(line(cx, t::LINE_SUBTLE))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(10.))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .flex()
                                    .flex_col()
                                    .gap(px(2.))
                                    .child(div().text_size(px(t::TEXT_SMALL)).font_weight(FontWeight::MEDIUM).text_color(c(if usable { tk.text_primary } else { tk.text_muted })).child(known.name))
                                    .child(div().truncate().font_family(mono(cx)).text_size(px(t::TEXT_META)).text_color(c(tk.text_faint)).child(SharedString::from(detail))),
                            )
                            .child(
                                ui::switch(("delegate-switch", ix), on, cx)
                                    .aria_label(SharedString::from(format!("Delegate to {}", known.name)))
                                    .tooltip(ui::tip(if on { "Cordata may delegate plan steps to it" } else { "Off: the orchestrator leaves it out" }))
                                    .on_click(cx.listener(move |this, _, _, cx| this.set_delegation(family, !on, cx))),
                            ),
                    )
                    .children(pools.into_iter().map(|pool| quota_row(pool, now, cx)))
                    .into_any_element(),
            );
        }
        let others: Vec<AnyElement> = found
            .iter()
            .filter(|f| f.known.family.is_none())
            .enumerate()
            .map(|(ix, f)| {
                let path = f.path.clone();
                div()
                    .px(px(14.))
                    .py(px(8.))
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap(px(2.))
                            .child(div().text_size(px(t::TEXT_SMALL)).text_color(c(tk.text_primary)).child(SharedString::from(if f.known.vendor.eq_ignore_ascii_case(f.known.name) { f.known.name.to_owned() } else { format!("{}  ·  {}", f.known.name, f.known.vendor) })))
                            .child(div().truncate().font_family(mono(cx)).text_size(px(t::TEXT_META)).text_color(c(tk.text_faint)).child(SharedString::from(f.version.as_deref().map(tidy_version).unwrap_or_else(|| f.path.display().to_string())))),
                    )
                    .child(
                        ui::button(("open-agent", ix), "Open", Kind::Ghost, cx)
                            .h(px(24.))
                            .px(px(8.))
                            .text_size(px(t::TEXT_SMALL))
                            .tooltip(ui::tip("Run it in the dock terminal"))
                            .on_click(cx.listener(move |this, _, window, cx| this.open_in_terminal(path.clone(), window, cx))),
                    )
                    .into_any_element()
            })
            .collect();
        let missing: Vec<&str> = match &self.agents.found {
            Some(found) => KNOWN.iter().filter(|k| k.family.is_none() && !found.iter().any(|f| f.known.id == k.id)).map(|k| k.name).collect(),
            None => Vec::new(),
        };
        let refresh_hover = c(tk.text_primary);
        div()
            .id("agents-panel")
            .w_full()
            .flex()
            .flex_col()
            .child(
                div()
                    .px(px(14.))
                    .pt(px(12.))
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(div().text_size(px(t::TEXT_SMALL)).text_color(c(tk.text_secondary)).child("Coding assistants on this Mac"))
                    .child(
                        div()
                            .id("agents-refresh")
                            .role(gpui::Role::Button)
                            .cursor_pointer()
                            .text_size(px(t::TEXT_META))
                            .text_color(c(tk.text_faint))
                            .hover(move |s| s.text_color(refresh_hover))
                            .child(if self.agents.loading { "checking…" } else { "refresh" })
                            .on_click(cx.listener(|this, _, _, cx| this.load_agents(cx))),
                    ),
            )
            .when_some(self.agents.note.clone(), |el, note| el.child(div().px(px(14.)).pt(px(8.)).text_size(px(t::TEXT_META)).text_color(c(tk.attention)).child(SharedString::from(note))))
            .child(label("DELEGATE — the orchestrator may hand plan steps to these"))
            .children(delegate)
            .when(!others.is_empty(), |el| el.child(label("ALSO INSTALLED — run in the dock terminal")).children(others))
            .when(!missing.is_empty(), |el| {
                el.child(label("KNOWN, NOT FOUND")).child(div().px(px(14.)).pb(px(16.)).text_size(px(t::TEXT_META)).text_color(c(tk.text_faint)).child(SharedString::from(missing.join(" · "))))
            })
            .into_any_element()
    }
}

fn quota_row(pool: &Pool, now: f64, cx: &mut Context<Workspace>) -> AnyElement {
    let tk = tk(cx);
    let colour = match pool.level.as_str() {
        "ok" => c(tk.amber),
        "soft" | "hard" => c(tk.attention),
        _ => c(tk.danger),
    };
    let share = pool.pct.clamp(0.0, 1.0) as f32;
    div()
        .flex()
        .flex_col()
        .gap(px(4.))
        .child(
            div()
                .flex()
                .justify_between()
                .gap(px(8.))
                .font_family(mono(cx))
                .text_size(px(t::TEXT_META))
                .child(div().text_color(c(tk.text_muted)).child(SharedString::from(pool.pool_id.clone())))
                .child(div().truncate().text_color(c(if pool.level == "ok" { tk.text_secondary } else { tk.attention })).child(SharedString::from(pool_line(pool, now)))),
        )
        .child(div().w_full().h(px(3.)).rounded_full().bg(line(cx, 0.12)).child(div().h_full().rounded_full().w(gpui::relative(share)).bg(colour)))
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool(pct: f64, resets_at: Option<f64>, limited_until: Option<f64>) -> Pool {
        Pool { pool_id: "claude_code".into(), family: "claude_code".into(), pct, level: "ok".into(), window_seconds: 5 * 3600, resets_at, limited_until, source: "meter".into() }
    }

    #[test]
    fn pool_lines_say_what_is_left_and_when_it_resets() {
        assert_eq!(pool_line(&pool(0.384, Some(1000.0 + 4320.0), None), 1000.0), "38% of the 5h window · resets in 1h 12m");
        assert_eq!(pool_line(&pool(1.0, None, Some(1300.0)), 1000.0), "limited · back in 5m");
        assert_eq!(pool_line(&pool(0.0, None, None), 1000.0), "nothing used in this window");
    }

    #[test]
    fn ledger_json_parses() {
        let v = serde_json::json!({"pool_id":"agy","family":"antigravity","pct":0.5,"level":"soft","window_seconds":18000,"resets_at":null,"limited_until":null,"source":"provider"});
        let p = Pool::from_json(&v).unwrap();
        assert_eq!((p.family.as_str(), p.level.as_str(), p.resets_at), ("antigravity", "soft", None));
    }
}

/// A CLI's `--version` line as a version: `ollama version is 0.40.13` and `2.1.317 (Claude Code)` both read
/// as the number. A line with no number in it is shown as it came.
fn tidy_version(line: &str) -> String {
    line.split_whitespace()
        .find(|word| word.trim_start_matches('v').chars().next().is_some_and(|c| c.is_ascii_digit()) && word.contains('.'))
        .map_or_else(|| line.trim().to_owned(), str::to_owned)
}

#[cfg(test)]
mod version_tests {
    use super::tidy_version;

    #[test]
    fn a_version_line_reads_as_the_number() {
        assert_eq!(tidy_version("ollama version is 0.40.13"), "0.40.13");
        assert_eq!(tidy_version("2.1.317 (Claude Code)"), "2.1.317");
        assert_eq!(tidy_version("aider 0.86.2"), "0.86.2");
        assert_eq!(tidy_version("v1.4.12"), "v1.4.12");
        assert_eq!(tidy_version("unknown version"), "unknown version");
    }
}
