//! The Models & Voice pane in the dock (docs/app.md §2, §4, phase P6):
//! Zero-download Ollama `:cloud` fleet, Auto Mode multi-model routing, background shell tasks,
//! unified memory budget, and offline macOS voice integration.

use cordata_theme::Role;
use gpui::{
    AnyElement, Context, InteractiveElement as _, IntoElement, ParentElement, SharedString,
    StatefulInteractiveElement as _, Styled, Window, div, px,
};
use gpui_component::button::{Button, ButtonVariants as _};
use serde_json::{Value, json};

use super::Workspace;
use crate::hud;
use crate::theme::color;
use crate::ui::{self, Kind};

pub const OLLAMA_STATUS_TAG: &str = "gui-ollama-status";
pub const OLLAMA_STOP_TAG: &str = "gui-ollama-stop";
pub const TASKS_LIST_TAG: &str = "gui-bg-tasks-list";
pub const TASKS_KILL_TAG: &str = "gui-bg-tasks-kill";

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ModelRow {
    pub name: String,
    pub size: String,
    pub modified: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct CloudModelRow {
    pub alias: String,
    pub wire_id: String,
    pub tier: String,
    pub context_window: u64,
    pub summary: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct BgTaskRow {
    pub task_id: String,
    pub command: String,
    pub state: String,
    pub elapsed_s: f64,
    pub exit_code: Option<i64>,
}

pub struct ModelsView {
    pub cloud_fleet: Vec<CloudModelRow>,
    pub local_installed: Vec<String>,
    pub loaded_models: Vec<ModelRow>,
    pub bg_tasks: Vec<BgTaskRow>,
    pub daemon_ok: Option<bool>,
    pub daemon_note: Option<String>,
    pub dispatch: String,
    pub total_memory_gb: f32,
    pub used_memory_gb: f32,
    pub ollama_resident_gb: f32,
}

fn default_cloud_fleet() -> Vec<CloudModelRow> {
    vec![
        CloudModelRow {
            alias: "mistral-large-4".into(),
            wire_id: "mistral-large-4:cloud".into(),
            tier: "strong".into(),
            context_window: 1_048_576,
            summary: "Mistral Large 4 — 1.05T MoE · lead architect, deep research & cross-family harden".into(),
        },
        CloudModelRow {
            alias: "minimax-m3.1-flash".into(),
            wire_id: "minimax-m3.1-flash:cloud".into(),
            tier: "fast".into(),
            context_window: 1_000_000,
            summary: "MiniMax M3.1 Flash — 428B MoE · fast repo scout, syntax edits & observer".into(),
        },
        CloudModelRow {
            alias: "gemma4:31b".into(),
            wire_id: "gemma4:31b:cloud".into(),
            tier: "standard".into(),
            context_window: 262_144,
            summary: "Gemma 4 31B — dense · grounded synthesis, structured JSON plans & conformance".into(),
        },
        CloudModelRow {
            alias: "kimi-k2.7-code".into(),
            wire_id: "kimi-k2.7-code:cloud".into(),
            tier: "standard".into(),
            context_window: 262_144,
            summary: "Kimi K2.7 Code — 1.04T MoE · long-horizon coding & debugging".into(),
        },
        CloudModelRow {
            alias: "deepseek-v4.1-flash".into(),
            wire_id: "deepseek-v4.1-flash:cloud".into(),
            tier: "fast".into(),
            context_window: 1_048_576,
            summary: "DeepSeek V4.1 Flash — 763B MoE · fast reasoning & coding".into(),
        },
        CloudModelRow {
            alias: "glm-5.3-flash".into(),
            wire_id: "glm-5.3-flash:cloud".into(),
            tier: "fast".into(),
            context_window: 1_048_576,
            summary: "GLM 5.3 Flash — 321B MoE · fast general fallback".into(),
        },
    ]
}

fn fmt_window(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.0}M ctx", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{}k ctx", tokens / 1_024)
    } else {
        format!("{tokens} ctx")
    }
}

impl ModelsView {
    pub fn new(_window: &mut Window, _cx: &mut Context<Workspace>) -> Self {
        Self {
            cloud_fleet: default_cloud_fleet(),
            local_installed: Vec::new(),
            loaded_models: Vec::new(),
            bg_tasks: Vec::new(),
            daemon_ok: None,
            daemon_note: None,
            dispatch: "shadow".into(),
            total_memory_gb: 48.0,
            used_memory_gb: 0.0,
            ollama_resident_gb: 0.0,
        }
    }

    pub fn take_status(&mut self, value: &Value) {
        let Some(content) = value.get("content").and_then(Value::as_array) else { return };
        for item in content {
            let Some(json) = item.get("json") else { continue };
            if let Some(total) = json
                .get("total_gb")
                .or_else(|| json.get("ceiling_gb"))
                .or_else(|| json.get("budget_gb"))
                .and_then(Value::as_f64)
            {
                self.total_memory_gb = total as f32;
            }
            if let Some(used) = json.get("used_gb").and_then(Value::as_f64) {
                self.used_memory_gb = used as f32;
            }
            if let Some(residents) = json.get("residents").and_then(Value::as_array) {
                let mut ollama_gb = 0.0f32;
                let mut rows = Vec::new();
                for r in residents {
                    let name = r.get("name").and_then(Value::as_str).unwrap_or_default().to_owned();
                    let runtime = r.get("runtime").and_then(Value::as_str).unwrap_or("ollama");
                    let size_gb = r.get("size_gb").and_then(Value::as_f64).unwrap_or(0.0) as f32;
                    if runtime == "ollama" {
                        ollama_gb += size_gb;
                    }
                    if !name.is_empty() {
                        rows.push(ModelRow {
                            name,
                            size: format!("{size_gb:.1} GB"),
                            modified: runtime.to_owned(),
                        });
                    }
                }
                self.ollama_resident_gb = ollama_gb;
                self.loaded_models = rows;
            }
            if let Some(daemon) = json.get("daemon") {
                self.daemon_ok = daemon.get("ok").and_then(Value::as_bool);
                self.daemon_note = daemon.get("problem").and_then(Value::as_str).map(str::to_owned);
            }
            if let Some(local) = json.get("local_installed").and_then(Value::as_array) {
                self.local_installed = local.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect();
            }
            if let Some(dispatch) = json.get("dispatch").and_then(Value::as_str) {
                self.dispatch = dispatch.to_owned();
            }
            if let Some(fleet) = json.get("cloud_fleet").and_then(Value::as_array) {
                let parsed: Vec<CloudModelRow> = fleet
                    .iter()
                    .filter_map(|m| {
                        Some(CloudModelRow {
                            alias: m.get("alias")?.as_str()?.to_owned(),
                            wire_id: m.get("wireId").and_then(Value::as_str).unwrap_or_default().to_owned(),
                            tier: m.get("tier").and_then(Value::as_str).unwrap_or("standard").to_owned(),
                            context_window: m.get("contextWindow").and_then(Value::as_u64).unwrap_or(128_000),
                            summary: m.get("summary").and_then(Value::as_str).unwrap_or_default().to_owned(),
                        })
                    })
                    .collect();
                if !parsed.is_empty() {
                    self.cloud_fleet = parsed;
                }
            }
        }
    }

    pub fn take_tasks(&mut self, value: &Value) {
        let Some(tasks) = value.get("tasks").and_then(Value::as_array) else { return };
        self.bg_tasks = tasks
            .iter()
            .filter_map(|t| {
                Some(BgTaskRow {
                    task_id: t.get("taskId")?.as_str()?.to_owned(),
                    command: t.get("command").and_then(Value::as_str).unwrap_or_default().to_owned(),
                    state: t.get("state").and_then(Value::as_str).unwrap_or("running").to_owned(),
                    elapsed_s: t.get("elapsedS").and_then(Value::as_f64).unwrap_or(0.0),
                    exit_code: t.get("exitCode").and_then(Value::as_i64),
                })
            })
            .collect();
    }
}

impl Workspace {
    pub(super) fn refresh_models_and_tasks(&mut self, cx: &mut Context<Self>) {
        let session = self.session.read(cx);
        session.request("_cordata/ollama/status", json!({}), OLLAMA_STATUS_TAG);
        session.request("_cordata/tasks/list", json!({}), TASKS_LIST_TAG);
    }

    pub(super) fn models_panel(&self, cx: &mut Context<Self>) -> AnyElement {
        let models = &self.models_view;
        let active_model = self
            .session
            .read(cx)
            .view
            .config("model")
            .map(|o| o.current_value.clone())
            .unwrap_or_default();

        // 1. Auto Mode Multi-Model Orchestrator Card
        let current_dispatch = self
            .settings_view
            .values
            .get("orchestrator.dispatch")
            .and_then(Value::as_str)
            .unwrap_or(models.dispatch.as_str())
            .to_owned();
        let dispatch_segments: Vec<AnyElement> = [("auto", "auto"), ("shadow", "shadow"), ("off", "static")]
            .iter()
            .enumerate()
            .map(|(ix, &(val, label))| {
                ui::segment(("dock-dispatch", ix), label, current_dispatch == val, cx)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.models_view.dispatch = val.to_owned();
                        this.set_user_setting("orchestrator.dispatch", json!(val), cx);
                    }))
                    .into_any_element()
            })
            .collect();
        let router_card = div()
            .p(px(12.))
            .bg(crate::theme::line(cx, cordata_theme::tokens::FILL_FAINT))
            .rounded(px(cordata_theme::tokens::RADIUS_CARD))
            .border_1()
            .border_color(crate::theme::line(cx, cordata_theme::tokens::LINE_DEFAULT))
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(ui::heading("auto mode routing", cx))
                    .child(ui::segmented_row(cx).children(dispatch_segments)),
            )
            .child(
                div()
                    .text_xs()
                    .line_height(gpui::relative(1.4))
                    .text_color(color(cx, Role::TextSecondary))
                    .child("Routes sub-tasks across Claude, Antigravity and Ollama :cloud by API cost, context window, capabilities and per-step RLAIF."),
            );

        // 2. Zero-Download Ollama Cloud Fleet Card
        let (daemon_label, daemon_role) = match models.daemon_ok {
            Some(true) => (
                format!("● Cloud proxy ready · {} local weights", models.local_installed.len()),
                Role::Ok,
            ),
            Some(false) => ("○ Daemon offline (run `ollama serve`)".to_owned(), Role::Attention),
            None => ("◌ Checking cloud proxy…".to_owned(), Role::TextDim),
        };
        let fleet_rows = models.cloud_fleet.iter().enumerate().map(|(ix, m)| {
            let alias = m.alias.clone();
            let is_active = active_model == m.alias || active_model == m.wire_id;
            let tier_role = match m.tier.as_str() {
                "strong" => Role::Accent,
                "fast" => Role::Ok,
                _ => Role::Agent,
            };
            div()
                .id(("cloud-model", ix))
                .flex()
                .flex_col()
                .gap(px(3.))
                .p(px(8.))
                .rounded(px(cordata_theme::tokens::RADIUS_CARD))
                .bg(if is_active { crate::theme::amber(cx, cordata_theme::tokens::AMBER_FILL) } else { color(cx, Role::Ground) })
                .border_1()
                .border_color(if is_active { crate::theme::amber(cx, cordata_theme::tokens::AMBER_BORDER) } else { crate::theme::line(cx, cordata_theme::tokens::LINE_DEFAULT) })
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap(px(6.))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(6.))
                                .min_w_0()
                                .child(
                                    div()
                                        .text_xs()
                                        .font_weight(gpui::FontWeight::BOLD)
                                        .text_color(color(cx, Role::Text))
                                        .truncate()
                                        .child(SharedString::from(m.alias.clone())),
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .font_family(crate::theme::mono(cx))
                                        .text_color(color(cx, tier_role))
                                        .child(SharedString::from(format!(
                                            "{} · {}",
                                            fmt_window(m.context_window),
                                            m.tier
                                        ))),
                                ),
                        )
                        .child(if is_active {
                            ui::meta("active", cx).into_any_element()
                        } else {
                            ui::button(("use-cloud-model", ix), "Use", Kind::Outline, cx)
                                .h(px(22.))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.session.update(cx, |s, cx| s.set_config("model", &alias, cx));
                                }))
                                .into_any_element()
                        }),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(color(cx, Role::TextDim))
                        .truncate()
                        .child(SharedString::from(m.summary.clone())),
                )
        });
        let fleet_body = div()
            .flex()
            .flex_col()
            .gap(px(6.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_xs()
                            .font_family(crate::theme::mono(cx))
                            .text_color(color(cx, daemon_role))
                            .child(SharedString::from(daemon_label)),
                    )
                    .child(
                        Button::new("refresh-fleet-btn")
                            .label("Refresh")
                            .ghost()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.refresh_models_and_tasks(cx);
                            })),
                    ),
            )
            .children(models.daemon_note.as_ref().map(|note| {
                div()
                    .text_xs()
                    .text_color(color(cx, Role::Attention))
                    .child(SharedString::from(note.clone()))
            }))
            .child(div().flex().flex_col().gap(px(4.)).children(fleet_rows));

        // 3. Background Shell Tasks (`_cordata/tasks/{list,kill}`)
        let tasks_body = if models.bg_tasks.is_empty() {
            div()
                .p(px(6.))
                .text_xs()
                .text_color(color(cx, Role::TextDim))
                .child("No background shell tasks in this session.")
        } else {
            let rows = models.bg_tasks.iter().enumerate().map(|(ix, t)| {
                let tid = t.task_id.clone();
                let running = t.state == "running";
                let badge_role = match t.state.as_str() {
                    "running" => Role::Accent,
                    "exited" if t.exit_code == Some(0) => Role::Ok,
                    "exited" | "failed" | "timed_out" => Role::Danger,
                    _ => Role::TextDim,
                };
                div()
                    .id(("bg-task", ix))
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap(px(6.))
                    .p(px(4.))
                    .rounded(px(2.))
                    .bg(color(cx, Role::Ground))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap(px(2.))
                            .child(
                                div()
                                    .text_xs()
                                    .font_family(crate::theme::mono(cx))
                                    .text_color(color(cx, Role::Text))
                                    .truncate()
                                    .child(SharedString::from(t.command.clone())),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(color(cx, badge_role))
                                    .child(SharedString::from(format!(
                                        "{} · {} · {:.1}s",
                                        t.task_id, t.state, t.elapsed_s
                                    ))),
                            ),
                    )
                    .child(if running {
                        ui::button(("kill-bg-task", ix), "Kill", Kind::Danger, cx)
                            .h(px(22.))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.kill_background_task(&tid, cx);
                            }))
                            .into_any_element()
                    } else {
                        ui::meta(SharedString::from(t.state.clone()), cx).into_any_element()
                    })
            });
            div().flex().flex_col().gap(px(3.)).children(rows)
        };

        // 4. Unified Memory Budget (zero local downloads by default)
        let mem_fraction = (models.used_memory_gb / models.total_memory_gb.max(1.0)).clamp(0.0, 1.0);
        let mem_gauge = div()
            .p(px(12.))
            .bg(crate::theme::line(cx, cordata_theme::tokens::FILL_FAINT))
            .rounded(px(cordata_theme::tokens::RADIUS_CARD))
            .border_1()
            .border_color(crate::theme::line(cx, cordata_theme::tokens::LINE_DEFAULT))
            .flex()
            .flex_col()
            .gap(px(6.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(ui::heading("memory budget", cx))
                    .child(
                        div()
                            .text_xs()
                            .font_family(crate::theme::mono(cx))
                            .text_color(color(cx, Role::TextSecondary))
                            .child(SharedString::from(format!(
                                "{:.1} GB / {:.1} GB ({:.0}%)",
                                models.used_memory_gb,
                                models.total_memory_gb,
                                mem_fraction * 100.0
                            ))),
                    ),
            )
            .child(
                div()
                    .w_full()
                    .h(px(5.))
                    .bg(color(cx, Role::Ground))
                    .rounded(px(3.))
                    .child(
                        div()
                            .h_full()
                            .w(gpui::relative(mem_fraction))
                            .bg(color(cx, if mem_fraction > 0.85 { Role::Attention } else { Role::Accent }))
                            .rounded(px(3.)),
                    ),
            )
            .child(
                div()
                    .text_xs()
                    .font_family(crate::theme::mono(cx))
                    .text_color(color(cx, Role::TextDim))
                    .child(SharedString::from(format!(
                        "Ollama resident: {:.1} GB · zero-download :cloud mode",
                        models.ollama_resident_gb
                    ))),
            );

        let loaded_section = (!models.loaded_models.is_empty()).then(|| {
            let rows = models.loaded_models.iter().enumerate().map(|(ix, m)| {
                let name = m.name.clone();
                div()
                    .id(("loaded-model", ix))
                    .flex()
                    .items_center()
                    .justify_between()
                    .p(px(4.))
                    .rounded(px(2.))
                    .bg(color(cx, Role::Ground))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .child(div().text_xs().text_color(color(cx, Role::Ok)).child("●"))
                            .child(
                                div()
                                    .text_xs()
                                    .font_weight(gpui::FontWeight::BOLD)
                                    .text_color(color(cx, Role::Text))
                                    .child(SharedString::from(m.name.clone())),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .font_family(crate::theme::mono(cx))
                                    .text_color(color(cx, Role::TextDim))
                                    .child(SharedString::from(m.size.clone())),
                            ),
                    )
                    .child(
                        Button::new(("unload-btn", ix))
                            .label("Unload")
                            .ghost()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.unload_ollama_model(&name, cx);
                            })),
                    )
            });
            hud::frame(
                "resident local weights",
                div().flex().flex_col().gap(px(2.)).children(rows),
                cx,
            )
        });

        // 5. Voice Section (macOS offline STT/TTS)
        let voice_card = div()
            .p(px(12.))
            .bg(crate::theme::line(cx, cordata_theme::tokens::FILL_FAINT))
            .rounded(px(cordata_theme::tokens::RADIUS_CARD))
            .border_1()
            .border_color(crate::theme::line(cx, cordata_theme::tokens::LINE_DEFAULT))
            .flex()
            .flex_col()
            .gap(px(6.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(ui::heading("voice & speech", cx))
                    .child(ui::badge("● macOS Native", ui::Tone::Teal, cx)),
            )
            .child(
                div()
                    .text_xs()
                    .line_height(gpui::relative(1.4))
                    .text_color(color(cx, Role::TextSecondary))
                    .child("Zero cloud latency: speech recognition via Apple Dictation and speech synthesis via NSSpeechSynthesizer."),
            )
            .child(
                div()
                    .flex()
                    .justify_end()
                    .pt(px(2.))
                    .child(
                        ui::button("voice-test-btn", "Test voice output ♫", Kind::Outline, cx)
                            .h(px(26.))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.speak_text("Cordata systems operational.", cx);
                            })),
                    ),
            );

        div()
            .id("models-dock-pane")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap(px(10.))
            .child(router_card)
            .child(hud::frame("ollama :cloud fleet (zero download)", fleet_body, cx))
            .child(hud::frame("background shell tasks", tasks_body, cx))
            .child(mem_gauge)
            .children(loaded_section)
            .child(voice_card)
            .into_any_element()
    }

    pub(super) fn kill_background_task(&mut self, task_id: &str, cx: &mut Context<Self>) {
        self.session.read(cx).request(
            "_cordata/tasks/kill",
            json!({ "taskId": task_id }),
            TASKS_KILL_TAG,
        );
    }

    pub(super) fn unload_ollama_model(&mut self, model: &str, cx: &mut Context<Self>) {
        self.session.read(cx).request(
            "_cordata/ollama/stop",
            json!({ "model": model }),
            OLLAMA_STOP_TAG,
        );
        self.models_view.loaded_models.retain(|m| m.name != model);
        cx.notify();
    }

    pub(super) fn speak_text(&mut self, text: &str, cx: &mut Context<Self>) {
        let text_owned = text.to_owned();
        cx.background_executor()
            .spawn(async move {
                let _ = std::process::Command::new("say")
                    .arg("-v")
                    .arg("Samantha")
                    .arg(&text_owned)
                    .output();
            })
            .detach();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_fleet_is_zero_download_cloud_only() {
        let fleet = default_cloud_fleet();
        assert_eq!(fleet.len(), 6);
        assert!(fleet.iter().all(|m| m.wire_id.ends_with(":cloud")));
        assert_eq!(fmt_window(1_048_576), "1M ctx");
        assert_eq!(fmt_window(262_144), "256k ctx");
    }

    #[test]
    fn take_status_and_tasks_parse_live_payloads() {
        let mut view = ModelsView {
            cloud_fleet: default_cloud_fleet(),
            local_installed: Vec::new(),
            loaded_models: Vec::new(),
            bg_tasks: Vec::new(),
            daemon_ok: None,
            daemon_note: None,
            dispatch: "shadow".into(),
            total_memory_gb: 48.0,
            used_memory_gb: 0.0,
            ollama_resident_gb: 0.0,
        };
        view.take_status(&json!({
            "content": [{
                "json": {
                    "ceiling_gb": 64.0,
                    "used_gb": 0.0,
                    "residents": [],
                    "daemon": {"ok": true, "problem": null},
                    "local_installed": [],
                    "dispatch": "auto",
                    "cloud_fleet": [
                        {"alias": "mistral-large-4", "wireId": "mistral-large-4:cloud", "tier": "strong", "contextWindow": 1048576, "summary": "Lead architect"}
                    ]
                }
            }]
        }));
        assert_eq!(view.daemon_ok, Some(true));
        assert_eq!(view.dispatch, "auto");
        assert_eq!(view.total_memory_gb, 64.0);
        assert_eq!(view.cloud_fleet.len(), 1);
        view.take_tasks(&json!({
            "tasks": [
                {"taskId": "0123456789ab", "command": "cargo test --workspace", "state": "running", "elapsedS": 4.2, "exitCode": null}
            ]
        }));
        assert_eq!(view.bg_tasks.len(), 1);
        assert_eq!(view.bg_tasks[0].task_id, "0123456789ab");
    }
}
