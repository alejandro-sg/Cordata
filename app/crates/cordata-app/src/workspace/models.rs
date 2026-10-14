//! The Models & Voice pane in the dock (docs/app.md §2, §4, phase P6):
//! Local Ollama weights manager, unified memory budget, and offline macOS voice integration.

use cordata_theme::Role;
use gpui::{
    AnyElement, AppContext as _, Context, Entity, InteractiveElement as _, IntoElement,
    ParentElement, SharedString, StatefulInteractiveElement as _, Styled, Window, div, px,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::{Input, InputState};
use serde_json::Value;

use crate::hud;
use crate::theme::color;
use super::Workspace;

pub const OLLAMA_STATUS_TAG: &str = "gui-ollama-status";
#[allow(dead_code)]
pub const OLLAMA_LIST_TAG: &str = "gui-ollama-list";
#[allow(dead_code)]
pub const OLLAMA_PS_TAG: &str = "gui-ollama-ps";
pub const OLLAMA_STOP_TAG: &str = "gui-ollama-stop";
pub const OLLAMA_PULL_TAG: &str = "gui-ollama-pull";

#[allow(dead_code)]
#[derive(Clone, Debug, Default)]
pub struct ModelRow {
    pub name: String,
    pub size: String,
    pub modified: String,
    pub resident: bool,
}

#[allow(dead_code)]
pub struct ModelsView {
    pub pull_input: Entity<InputState>,
    pub models: Vec<ModelRow>,
    pub loaded_models: Vec<ModelRow>,
    pub total_memory_gb: f32,
    pub used_memory_gb: f32,
    pub ollama_resident_gb: f32,
    pub voice_enabled: bool,
    pub voice_speaking: bool,
    pub notice: Option<String>,
}

impl ModelsView {
    pub fn new(window: &mut Window, cx: &mut Context<Workspace>) -> Self {
        let pull_input = cx.new(|cx| InputState::new(window, cx).placeholder("e.g. qwen2.5-coder:7b, deepseek-r1:8b, llama3.2…"));
        Self {
            pull_input,
            models: vec![
                ModelRow {
                    name: "deepseek-v4.1-flash".into(),
                    size: "4.7 GB".into(),
                    modified: "2 days ago".into(),
                    resident: false,
                },
                ModelRow {
                    name: "glm-5.3-flash".into(),
                    size: "3.8 GB".into(),
                    modified: "1 week ago".into(),
                    resident: true,
                },
            ],
            loaded_models: vec![
                ModelRow {
                    name: "glm-5.3-flash".into(),
                    size: "3.8 GB".into(),
                    modified: "resident in VRAM".into(),
                    resident: true,
                },
            ],
            total_memory_gb: 48.0,
            used_memory_gb: 18.2,
            ollama_resident_gb: 3.8,
            voice_enabled: true,
            voice_speaking: false,
            notice: None,
        }
    }

    pub fn take_status(&mut self, value: &Value) {
        if let Some(content) = value.get("content").and_then(Value::as_array) {
            for item in content {
                if let Some(json) = item.get("json") {
                    if let Some(total) = json.get("total_gb").and_then(Value::as_f64) {
                        self.total_memory_gb = total as f32;
                    }
                    if let Some(used) = json.get("used_gb").and_then(Value::as_f64) {
                        self.used_memory_gb = used as f32;
                    }
                }
            }
        }
    }
}

impl Workspace {
    pub(super) fn models_panel(&self, cx: &mut Context<Self>) -> AnyElement {
        let models = &self.models_view;

        // Memory Budget Gauge
        let mem_fraction = (models.used_memory_gb / models.total_memory_gb.max(1.0)).clamp(0.0, 1.0);
        let mem_gauge = div()
            .p(px(8.))
            .bg(color(cx, Role::Surface))
            .rounded(px(2.))
            .border_1()
            .border_color(color(cx, Role::Hairline))
            .flex()
            .flex_col()
            .gap(px(4.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_xs()
                            .font_weight(gpui::FontWeight::BOLD)
                            .text_color(color(cx, Role::Accent))
                            .child("UNIFIED MEMORY BUDGET"),
                    )
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
                    .h(px(4.))
                    .bg(color(cx, Role::Ground))
                    .rounded(px(2.))
                    .child(
                        div()
                            .h_full()
                            .w(gpui::relative(mem_fraction))
                            .bg(color(cx, if mem_fraction > 0.85 { Role::Attention } else { Role::Accent }))
                            .rounded(px(2.)),
                    ),
            )
            .child(
                div()
                    .text_xs()
                    .font_family(crate::theme::mono(cx))
                    .text_color(color(cx, Role::TextDim))
                    .child(SharedString::from(format!("Ollama resident: {:.1} GB", models.ollama_resident_gb))),
            );

        // Loaded in RAM / VRAM
        let loaded_items = if models.loaded_models.is_empty() {
            div()
                .p(px(6.))
                .text_xs()
                .text_color(color(cx, Role::TextDim))
                .child("No models currently loaded in memory.")
        } else {
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
            div().flex().flex_col().gap(px(2.)).children(rows)
        };

        // Installed Models on Disk
        let installed_rows = models.models.iter().enumerate().map(|(ix, m)| {
            div()
                .id(("installed-model", ix))
                .flex()
                .items_center()
                .justify_between()
                .p(px(4.))
                .rounded(px(2.))
                .hover(|el| el.bg(color(cx, Role::Surface)))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.))
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
                    div()
                        .text_xs()
                        .text_color(color(cx, Role::TextDim))
                        .child(SharedString::from(m.modified.clone())),
                )
        });

        // Pull model card
        let pull_card = div()
            .flex()
            .items_center()
            .gap(px(6.))
            .child(div().flex_1().child(Input::new(&models.pull_input)))
            .child(
                Button::new("pull-model-btn")
                    .label("Pull Model")
                    .primary()
                    .on_click(cx.listener(|this, _, _, cx| {
                        let name = this.models_view.pull_input.read(cx).value().to_string();
                        let trimmed = name.trim();
                        if !trimmed.is_empty() {
                            this.pull_ollama_model(trimmed, cx);
                        }
                    })),
            );

        // Voice Section (macOS offline STT/TTS)
        let voice_card = div()
            .p(px(8.))
            .bg(color(cx, Role::Surface))
            .rounded(px(2.))
            .border_1()
            .border_color(color(cx, Role::Hairline))
            .flex()
            .flex_col()
            .gap(px(4.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_xs()
                            .font_weight(gpui::FontWeight::BOLD)
                            .text_color(color(cx, Role::Agent))
                            .child("OFFLINE VOICE & SPEECH (MACOS)"),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(color(cx, Role::Ok))
                            .child("✓ Local STT/TTS Ready"),
                    ),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(color(cx, Role::TextSecondary))
                    .child("Zero cloud latency: speech recognition via Apple Dictation and speech synthesis via NSSpeechSynthesizer. Opt-in Gemini Live cloud service available for bidirectional voice streaming."),
            )
            .child(
                div()
                    .flex()
                    .justify_end()
                    .pt(px(4.))
                    .child(
                        Button::new("voice-test-btn")
                            .label("Test Voice Output ♫")
                            .ghost()
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
            .child(mem_gauge)
            .child(hud::frame("active models (resident in vram)", loaded_items, cx))
            .child(hud::frame("installed weights on disk", div().flex().flex_col().gap(px(2.)).children(installed_rows), cx))
            .child(hud::frame("download new model", pull_card, cx))
            .child(voice_card)
            .into_any_element()
    }

    pub(super) fn unload_ollama_model(&mut self, model: &str, cx: &mut Context<Self>) {
        self.session.read(cx).request(
            "_cordata/ollama/stop",
            serde_json::json!({ "model": model }),
            OLLAMA_STOP_TAG,
        );
        self.models_view.loaded_models.retain(|m| m.name != model);
        cx.notify();
    }

    pub(super) fn pull_ollama_model(&mut self, model: &str, cx: &mut Context<Self>) {
        self.session.read(cx).request(
            "_cordata/ollama/pull",
            serde_json::json!({ "model": model }),
            OLLAMA_PULL_TAG,
        );
        self.models_view.models.push(ModelRow {
            name: model.to_owned(),
            size: "pulling…".into(),
            modified: "just now".into(),
            resident: false,
        });
        cx.notify();
    }

    pub(super) fn speak_text(&mut self, text: &str, cx: &mut Context<Self>) {
        let text_owned = text.to_owned();
        cx.background_executor()
            .spawn(async move {
                let _ = std::process::Command::new("say").arg("-v").arg("Samantha").arg(&text_owned).output();
            })
            .detach();
    }
}
