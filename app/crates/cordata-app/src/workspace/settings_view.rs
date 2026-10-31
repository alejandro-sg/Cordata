//! The Settings & Trust pane in the dock (docs/app.md §6.4, phase B4):
//! - Trust status: gated keys, ignored settings, blocked plugins/MCP servers with exact commands,
//!   and explicit user grant (`_cordata/trust/grant` with `confirmed: true`).
//! - Settings: schema validation, user vs project scopes, `app.*` preferences, live updates via `_cordata/settings/*`.
//! - Rules: allow/ask/deny permissions list with Add and Remove.
//! - Doctor: system health checks via `_cordata/doctor`.
//! - Auto Readiness: pre-flight check via `_cordata/auto/readiness`.

use std::collections::HashMap;

use cordata_theme::Role;
use cordata_view::shown;
use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, AppContext as _, Context, Entity, InteractiveElement as _, IntoElement,
    ParentElement, SharedString, StatefulInteractiveElement as _, Styled, Window, div, px,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::{Input, InputState};
use serde_json::Value;

use crate::hud;
use crate::theme::{color, tint};
use super::Workspace;

pub const SETTINGS_GET_TAG: &str = "gui-settings-get";
pub const SETTINGS_SCHEMA_TAG: &str = "gui-settings-schema";
pub const SETTINGS_SET_TAG: &str = "gui-settings-set";
pub const SETTINGS_RULE_ADD_TAG: &str = "gui-settings-rule-add";
pub const SETTINGS_RULE_REMOVE_TAG: &str = "gui-settings-rule-remove";
pub const TRUST_STATUS_TAG: &str = "gui-trust-status";
pub const TRUST_GRANT_TAG: &str = "gui-trust-grant";
pub const DOCTOR_TAG: &str = "gui-doctor";
pub const READINESS_TAG: &str = "gui-auto-readiness";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SettingsSubTab {
    #[default]
    Trust,
    Config,
    Doctor,
}

#[derive(Clone, Debug, Default)]
pub struct RuleRow {
    pub kind: String,
    pub text: String,
    pub scope: String,
    pub active: bool,
}

#[derive(Clone, Debug, Default)]
pub struct BlockedPlugin {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub env_keys: Vec<String>,
    pub reason: String,
}

#[derive(Clone, Debug, Default)]
pub struct DoctorCheck {
    pub name: String,
    pub level: String,
    pub message: String,
}

#[derive(Clone, Debug)]
#[allow(dead_code)]
pub struct SchemaKey {
    pub key: String,
    pub kind: String,
    pub args: Vec<String>,
    pub trust_gated: bool,
    pub user_only: bool,
}

pub struct SettingsView {
    pub sub_tab: SettingsSubTab,
    // Trust
    pub trusted: bool,
    pub ignored_warnings: Vec<String>,
    pub blocked_plugins: Vec<BlockedPlugin>,
    pub allow_rules: Vec<String>,
    pub gated_keys: Vec<String>,
    pub project_settings: HashMap<String, Value>,
    // Readiness
    pub readiness: Option<Value>,
    // Settings & Schema
    pub values: HashMap<String, Value>,
    pub sources: HashMap<String, String>,
    pub warnings: Vec<String>,
    pub schema_keys: Vec<SchemaKey>,
    // Rules
    pub rules: Vec<RuleRow>,
    pub rule_input: Entity<InputState>,
    pub rule_kind: String,
    pub rule_scope: String,
    // Doctor
    pub doctor_checks: Vec<DoctorCheck>,
    pub doctor_loading: bool,
    // Feedback
    pub notice: Option<String>,
    pub notice_error: bool,
}

impl SettingsView {
    pub fn new(window: &mut Window, cx: &mut Context<Workspace>) -> Self {
        let rule_input = cx.new(|cx| InputState::new(window, cx).placeholder("e.g. shell(cargo test) or file_write(src/*)"));
        Self {
            sub_tab: SettingsSubTab::Trust,
            trusted: false,
            ignored_warnings: Vec::new(),
            blocked_plugins: Vec::new(),
            allow_rules: Vec::new(),
            gated_keys: Vec::new(),
            project_settings: HashMap::new(),
            readiness: None,
            values: HashMap::new(),
            sources: HashMap::new(),
            warnings: Vec::new(),
            schema_keys: Vec::new(),
            rules: Vec::new(),
            rule_input,
            rule_kind: "allow".into(),
            rule_scope: "user".into(),
            doctor_checks: Vec::new(),
            doctor_loading: false,
            notice: None,
            notice_error: false,
        }
    }

    pub fn take_settings_get(&mut self, value: &Value) {
        if let Some(trusted) = value.get("trusted").and_then(Value::as_bool) {
            self.trusted = trusted;
        }
        if let Some(obj) = value.get("values").and_then(Value::as_object) {
            self.values = obj.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        }
        if let Some(obj) = value.get("sources").and_then(Value::as_object) {
            self.sources = obj
                .iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_owned())))
                .collect();
        }
        if let Some(warnings) = value.get("warnings").and_then(Value::as_array) {
            self.warnings = warnings.iter().filter_map(|w| w.as_str().map(str::to_owned)).collect();
        }
        if let Some(rules) = value.get("rules").and_then(Value::as_array) {
            self.rules = rules
                .iter()
                .map(|r| RuleRow {
                    kind: r.get("kind").and_then(Value::as_str).unwrap_or("").to_owned(),
                    text: r.get("text").and_then(Value::as_str).unwrap_or("").to_owned(),
                    scope: r.get("scope").and_then(Value::as_str).unwrap_or("user").to_owned(),
                    active: r.get("active").and_then(Value::as_bool).unwrap_or(true),
                })
                .collect();
        }
    }

    pub fn take_schema(&mut self, value: &Value) {
        if let Some(keys) = value.get("keys").and_then(Value::as_array) {
            self.schema_keys = keys
                .iter()
                .map(|k| SchemaKey {
                    key: k.get("key").and_then(Value::as_str).unwrap_or("").to_owned(),
                    kind: k.get("kind").and_then(Value::as_str).unwrap_or("").to_owned(),
                    args: k
                        .get("args")
                        .and_then(Value::as_array)
                        .map(|arr| arr.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect())
                        .unwrap_or_default(),
                    trust_gated: k.get("trustGated").and_then(Value::as_bool).unwrap_or(false),
                    user_only: k.get("userOnly").and_then(Value::as_bool).unwrap_or(false),
                })
                .collect();
        }
    }

    pub fn take_trust_status(&mut self, value: &Value) {
        if let Some(trusted) = value.get("trusted").and_then(Value::as_bool) {
            self.trusted = trusted;
        }
        if let Some(arr) = value.get("ignored").and_then(Value::as_array) {
            self.ignored_warnings = arr.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect();
        }
        if let Some(arr) = value.get("allowRules").and_then(Value::as_array) {
            self.allow_rules = arr.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect();
        }
        if let Some(arr) = value.get("gatedKeys").and_then(Value::as_array) {
            self.gated_keys = arr.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect();
        }
        if let Some(obj) = value.get("projectSettings").and_then(Value::as_object) {
            self.project_settings = obj.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        }
        if let Some(plugins) = value.get("blockedPlugins").and_then(Value::as_array) {
            self.blocked_plugins = plugins
                .iter()
                .map(|p| BlockedPlugin {
                    name: p.get("name").and_then(Value::as_str).unwrap_or("").to_owned(),
                    command: p.get("command").and_then(Value::as_str).unwrap_or("").to_owned(),
                    args: p
                        .get("args")
                        .and_then(Value::as_array)
                        .map(|arr| arr.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect())
                        .unwrap_or_default(),
                    env_keys: p
                        .get("envKeys")
                        .and_then(Value::as_array)
                        .map(|arr| arr.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect())
                        .unwrap_or_default(),
                    reason: p.get("reason").and_then(Value::as_str).unwrap_or("").to_owned(),
                })
                .collect();
        }
    }

    pub fn take_doctor(&mut self, value: &Value) {
        self.doctor_loading = false;
        if let Some(checks) = value.get("checks").and_then(Value::as_array) {
            self.doctor_checks = checks
                .iter()
                .map(|c| DoctorCheck {
                    name: c.get("name").and_then(Value::as_str).unwrap_or("").to_owned(),
                    level: c.get("level").and_then(Value::as_str).unwrap_or("ok").to_owned(),
                    message: c
                        .get("message")
                        .or_else(|| c.get("summary"))
                        .or_else(|| c.get("details"))
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_owned(),
                })
                .collect();
        }
    }

    pub fn take_readiness(&mut self, value: &Value) {
        self.readiness = Some(value.clone());
    }

    pub fn set_notice(&mut self, text: String, error: bool) {
        self.notice = Some(text);
        self.notice_error = error;
    }
}

impl Workspace {
    pub(super) fn settings_panel(&self, cx: &mut Context<Self>) -> AnyElement {
        let view = &self.settings_view;
        const SUB_TABS: [SettingsSubTab; 3] = [SettingsSubTab::Trust, SettingsSubTab::Config, SettingsSubTab::Doctor];

        let sub_nav = div()
            .flex()
            .items_center()
            .gap(px(6.))
            .border_b_1()
            .border_color(color(cx, Role::Hairline))
            .pb(px(6.))
            .children(SUB_TABS.iter().map(|&tab| {
                let selected = view.sub_tab == tab;
                let label = match tab {
                    SettingsSubTab::Trust => {
                        if view.trusted {
                            "Trust & Rules ✓"
                        } else {
                            "Trust & Rules ⚠"
                        }
                    }
                    SettingsSubTab::Config => "Settings",
                    SettingsSubTab::Doctor => "Doctor",
                };
                div()
                    .id(("settings-subtab", tab as usize))
                    .px(px(6.))
                    .py(px(2.))
                    .rounded(px(2.))
                    .cursor_pointer()
                    .text_xs()
                    .font_family(crate::theme::mono(cx))
                    .text_color(color(cx, if selected { Role::Accent } else { Role::TextSecondary }))
                    .when(selected, |el| el.bg(tint(cx, Role::Accent, 0.1)))
                    .hover(|el| el.text_color(color(cx, Role::Text)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.settings_view.sub_tab = tab;
                        if tab == SettingsSubTab::Doctor && this.settings_view.doctor_checks.is_empty() {
                            this.run_doctor(cx);
                        }
                        cx.notify();
                    }))
                    .child(SharedString::from(label))
            }));

        let notice_element = view.notice.as_ref().map(|text| {
            let role = if view.notice_error { Role::Danger } else { Role::Ok };
            div()
                .px(px(8.))
                .py(px(4.))
                .rounded(px(2.))
                .bg(tint(cx, role, 0.12))
                .border_1()
                .border_color(tint(cx, role, 0.3))
                .text_xs()
                .text_color(color(cx, role))
                .child(SharedString::from(text.clone()))
        });

        let body = match view.sub_tab {
            SettingsSubTab::Trust => self.trust_subpanel(cx),
            SettingsSubTab::Config => self.config_subpanel(cx),
            SettingsSubTab::Doctor => self.doctor_subpanel(cx),
        };

        div()
            .id("settings-scroll")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap(px(10.))
            .pb(px(16.))
            .child(sub_nav)
            .when_some(notice_element, |this, el| this.child(el))
            .child(body)
            .into_any_element()
    }

    // --------------------------------------------------------------------------------- Trust & Rules
    fn trust_subpanel(&self, cx: &mut Context<Self>) -> AnyElement {
        let view = &self.settings_view;

        // Trust status card
        let status_header = if view.trusted {
            div()
                .p(px(8.))
                .rounded(px(3.))
                .bg(tint(cx, Role::Ok, 0.08))
                .border_1()
                .border_color(tint(cx, Role::Ok, 0.25))
                .flex()
                .flex_col()
                .gap(px(4.))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.))
                        .text_xs()
                        .font_weight(gpui::FontWeight::BOLD)
                        .text_color(color(cx, Role::Ok))
                        .child("✓ FOLDER IS TRUSTED"),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(color(cx, Role::TextSecondary))
                        .child("Project allow rules, MCP servers, and gated preferences are active."),
                )
        } else {
            div()
                .p(px(8.))
                .rounded(px(3.))
                .bg(tint(cx, Role::Attention, 0.08))
                .border_1()
                .border_color(tint(cx, Role::Attention, 0.35))
                .flex()
                .flex_col()
                .gap(px(6.))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.))
                        .text_xs()
                        .font_weight(gpui::FontWeight::BOLD)
                        .text_color(color(cx, Role::Attention))
                        .child("⚠ FOLDER IS NOT TRUSTED"),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(color(cx, Role::TextSecondary))
                        .child("To protect against arbitrary execution, project-scoped settings, allow rules, and MCP servers are held back until you grant trust."),
                )
                .child(
                    div()
                        .flex()
                        .justify_end()
                        .pt(px(4.))
                        .child(
                            Button::new("grant-trust-btn")
                                .label("Grant Trust to this Folder")
                                .primary()
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.grant_trust(cx);
                                })),
                        ),
                )
        };

        // Blocked MCP servers / plugins
        let blocked_section = (!view.blocked_plugins.is_empty()).then(|| {
            let items = view.blocked_plugins.iter().enumerate().map(|(ix, p)| {
                div()
                    .id(("blocked-plugin", ix))
                    .p(px(6.))
                    .rounded(px(2.))
                    .bg(color(cx, Role::Surface))
                    .border_1()
                    .border_color(color(cx, Role::Hairline))
                    .flex()
                    .flex_col()
                    .gap(px(2.))
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
                                    .child(SharedString::from(p.name.clone())),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(color(cx, Role::Danger))
                                    .child(SharedString::from(p.reason.clone())),
                            ),
                    )
                    .child(
                        div()
                            .text_xs()
                            .font_family(crate::theme::mono(cx))
                            .text_color(color(cx, Role::TextDim))
                            .child(SharedString::from(format!("{} {}", p.command, p.args.join(" ")))),
                    )
                    .when(!p.env_keys.is_empty(), |el| {
                        el.child(
                            div()
                                .text_xs()
                                .text_color(color(cx, Role::TextDim))
                                .child(SharedString::from(format!("env: {}", p.env_keys.join(", ")))),
                        )
                    })
            });

            hud::frame(
                "blocked mcp servers (held back)",
                div().flex().flex_col().gap(px(4.)).children(items),
                cx,
            )
        });

        // Ignored project settings
        let ignored_section = (!view.ignored_warnings.is_empty()).then(|| {
            let rows = view.ignored_warnings.iter().enumerate().map(|(ix, w)| {
                div()
                    .id(("ignored-warning", ix))
                    .text_xs()
                    .text_color(color(cx, Role::Attention))
                    .child(SharedString::from(format!("• {w}")))
            });
            hud::frame("ignored settings", div().flex().flex_col().gap(px(3.)).children(rows), cx)
        });

        // Rules list
        let rules_list = {
            let rows = view.rules.iter().enumerate().map(|(ix, r)| {
                let role = match r.kind.as_str() {
                    "allow" => Role::Ok,
                    "deny" => Role::Danger,
                    _ => Role::Attention,
                };
                let rule_text = r.text.clone();
                div()
                    .id(("rule-row", ix))
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
                                    .px(px(4.))
                                    .py(px(1.))
                                    .rounded(px(2.))
                                    .bg(tint(cx, role, 0.15))
                                    .text_xs()
                                    .font_weight(gpui::FontWeight::BOLD)
                                    .text_color(color(cx, role))
                                    .child(SharedString::from(r.kind.to_uppercase())),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .font_family(crate::theme::mono(cx))
                                    .text_color(color(cx, if r.active { Role::Text } else { Role::TextDim }))
                                    .child(SharedString::from(shown(&r.text))),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(color(cx, Role::TextDim))
                                    .child(SharedString::from(format!("[{}]", r.scope))),
                            )
                            .when(!r.active, |el| {
                                el.child(
                                    div()
                                        .text_xs()
                                        .text_color(color(cx, Role::Attention))
                                        .child("(inactive)"),
                                )
                            }),
                    )
                    .child(
                        Button::new(("remove-rule", ix))
                            .label("Remove")
                            .ghost()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.remove_rule(&rule_text, cx);
                            })),
                    )
            });

            hud::frame(
                "permission rules (allow · ask · deny)",
                div().flex().flex_col().gap(px(2.)).children(rows),
                cx,
            )
        };

        // Add Rule Form
        let add_rule_card = {
            let kinds = ["allow", "ask", "deny"];
            let kind_buttons = div().flex().gap(px(4.)).children(kinds.into_iter().map(|k| {
                let selected = view.rule_kind == k;
                let role = match k {
                    "allow" => Role::Ok,
                    "deny" => Role::Danger,
                    _ => Role::Attention,
                };
                div()
                    .id(SharedString::from(format!("kind-select-{k}")))
                    .px(px(6.))
                    .py(px(2.))
                    .rounded(px(2.))
                    .cursor_pointer()
                    .text_xs()
                    .font_weight(gpui::FontWeight::BOLD)
                    .text_color(color(cx, if selected { role } else { Role::TextDim }))
                    .when(selected, |el| el.bg(tint(cx, role, 0.2)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.settings_view.rule_kind = k.into();
                        cx.notify();
                    }))
                    .child(k.to_uppercase())
            }));

            let scopes = ["user", "project"];
            let scope_buttons = div().flex().gap(px(4.)).children(scopes.into_iter().map(|s| {
                let selected = view.rule_scope == s;
                div()
                    .id(SharedString::from(format!("scope-select-{s}")))
                    .px(px(6.))
                    .py(px(2.))
                    .rounded(px(2.))
                    .cursor_pointer()
                    .text_xs()
                    .text_color(color(cx, if selected { Role::Accent } else { Role::TextDim }))
                    .when(selected, |el| el.bg(tint(cx, Role::Accent, 0.15)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.settings_view.rule_scope = s.into();
                        cx.notify();
                    }))
                    .child(s)
            }));

            let input_el = Input::new(&view.rule_input);

            hud::frame(
                "add rule",
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(kind_buttons)
                            .child(scope_buttons),
                    )
                    .child(input_el)
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .child(
                                Button::new("submit-rule-btn")
                                    .label("+ Add Rule")
                                    .primary()
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.submit_rule(cx);
                                    })),
                            ),
                    ),
                cx,
            )
        };

        // Auto mode readiness
        let readiness_section = view.readiness.as_ref().map(|ready| {
            let checkpoints_ok = ready
                .get("checkpoints")
                .and_then(|c| c.get("enabled"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let viable = ready
                .get("checkpoints")
                .and_then(|c| c.get("viable"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let deny_count = ready.get("denyRules").and_then(Value::as_array).map_or(0, |a| a.len());
            let asks = ready
                .get("stillAsks")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", "))
                .unwrap_or_default();

            hud::frame(
                "auto mode readiness",
                div()
                    .flex()
                    .flex_col()
                    .gap(px(3.))
                    .text_xs()
                    .child(
                        div()
                            .text_color(color(cx, if checkpoints_ok { Role::Ok } else { Role::Attention }))
                            .child(SharedString::from(format!(
                                "• Checkpoints: {} (viable: {})",
                                if checkpoints_ok { "enabled" } else { "disabled" },
                                viable
                            ))),
                    )
                    .child(
                        div()
                            .text_color(color(cx, Role::TextSecondary))
                            .child(SharedString::from(format!("• Deny rules protecting run: {deny_count}"))),
                    )
                    .when(!asks.is_empty(), |el| {
                        el.child(
                            div()
                                .text_color(color(cx, Role::TextDim))
                                .child(SharedString::from(format!("• Still asks for: {asks}"))),
                        )
                    }),
                cx,
            )
        });

        div()
            .flex()
            .flex_col()
            .gap(px(10.))
            .child(status_header)
            .when_some(blocked_section, |this, el| this.child(el))
            .when_some(ignored_section, |this, el| this.child(el))
            .child(rules_list)
            .child(add_rule_card)
            .when_some(readiness_section, |this, el| this.child(el))
            .into_any_element()
    }

    // --------------------------------------------------------------------------------- Settings Editor
    fn config_subpanel(&self, cx: &mut Context<Self>) -> AnyElement {
        let view = &self.settings_view;

        // Group known configurable settings
        let target_keys = [
            ("app.theme", "Theme (System / Light / 2049 / Graphite / Dim)"),
            ("app.density", "Density (Comfortable / Compact)"),
            ("temperature", "Model Temperature (Precise / Balanced / Creative)"),
            ("tui.thinking", "Thought Bubbles"),
            ("tui.toolOutput", "Tool Outputs"),
            ("tui.showLaya", "Show Laya Classifier"),
            ("vault.enabled", "Credentials Vault (Fail-Closed)"),
            ("vault.addr", "Vault Server Address"),
            ("memory.traces", "Episodic Trace Storage"),
            ("laya.threshold", "Laya Routing Threshold"),
        ];

        let rows = target_keys.into_iter().enumerate().map(|(ix, (key, label))| {
            let current = view.values.get(key);
            let source = view.sources.get(key).map(|s| s.as_str()).unwrap_or("default");
            let key_owned = key.to_owned();

            let val_display = match current {
                Some(Value::Bool(b)) => if *b { "true" } else { "false" }.to_owned(),
                Some(Value::String(s)) => s.clone(),
                Some(Value::Number(n)) => n.to_string(),
                _ => "—".to_owned(),
            };

            let is_bool = matches!(current, Some(Value::Bool(_)));
            let is_enum = key == "app.theme" || key == "app.density" || key == "temperature" || key.starts_with("tui.");

            div()
                .id(("config-row", ix))
                .flex()
                .items_center()
                .justify_between()
                .p(px(6.))
                .rounded(px(2.))
                .hover(|el| el.bg(color(cx, Role::Surface)))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(2.))
                        .child(
                            div()
                                .text_xs()
                                .font_weight(gpui::FontWeight::BOLD)
                                .text_color(color(cx, Role::Text))
                                .child(label),
                        )
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(4.))
                                .child(
                                    div()
                                        .text_xs()
                                        .font_family(crate::theme::mono(cx))
                                        .text_color(color(cx, Role::TextDim))
                                        .child(key),
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(color(cx, Role::Accent))
                                        .child(SharedString::from(format!("[{source}]"))),
                                ),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.))
                        .child(
                            div()
                                .text_xs()
                                .font_family(crate::theme::mono(cx))
                                .text_color(color(cx, Role::Accent))
                                .child(SharedString::from(val_display)),
                        )
                        .when(is_bool, |el| {
                            let curr_b = matches!(current, Some(Value::Bool(true)));
                            let key_bool = key_owned.clone();
                            el.child(
                                Button::new(("toggle-btn", ix))
                                    .label(if curr_b { "Turn Off" } else { "Turn On" })
                                    .ghost()
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.set_setting("user", &key_bool, Value::Bool(!curr_b), cx);
                                    })),
                            )
                        })
                        .when(is_enum && !is_bool, |el| {
                            let key_enum = key_owned.clone();
                            let next_val = match (key, current.and_then(Value::as_str).unwrap_or("")) {
                                ("app.theme", "system") => "light",
                                ("app.theme", "light") => "2049",
                                ("app.theme", "2049" | "night") => "graphite",
                                ("app.theme", "graphite") => "dim",
                                ("app.theme", _) => "system",
                                ("app.density", "comfortable") => "compact",
                                ("app.density", _) => "comfortable",
                                ("temperature", "precise") => "balanced",
                                ("temperature", "balanced") => "creative",
                                ("temperature", _) => "precise",
                                ("tui.thinking" | "tui.toolOutput", "collapsed") => "expanded",
                                ("tui.thinking" | "tui.toolOutput", _) => "collapsed",
                                _ => "",
                            };
                            if next_val.is_empty() {
                                el
                            } else {
                                el.child(
                                    Button::new(("cycle-btn", ix))
                                        .label("Cycle")
                                        .ghost()
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.set_setting("user", &key_enum, Value::String(next_val.into()), cx);
                                        })),
                                )
                            }
                        }),
                )
        });

        hud::frame(
            "preferences & app settings",
            div().flex().flex_col().gap(px(2.)).children(rows),
            cx,
        )
        .into_any_element()
    }

    // --------------------------------------------------------------------------------- Doctor
    fn doctor_subpanel(&self, cx: &mut Context<Self>) -> AnyElement {
        let view = &self.settings_view;

        let action_bar = div()
            .flex()
            .items_center()
            .justify_between()
            .child(
                div()
                    .text_xs()
                    .text_color(color(cx, Role::TextSecondary))
                    .child("Diagnose tools, models, vault, and environment health."),
            )
            .child(
                Button::new("run-doctor-btn")
                    .label(if view.doctor_loading { "Running…" } else { "↻ Run Health Checks" })
                    .primary()
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.run_doctor(cx);
                    })),
            );

        let checks_list = if view.doctor_checks.is_empty() {
            div()
                .p(px(12.))
                .text_xs()
                .text_color(color(cx, Role::TextDim))
                .child("Click 'Run Health Checks' to evaluate environment health.")
        } else {
            let items = view.doctor_checks.iter().enumerate().map(|(ix, check)| {
                let (glyph, role) = match check.level.as_str() {
                    "ok" => ("✓", Role::Ok),
                    "warn" => ("⚠", Role::Attention),
                    _ => ("✗", Role::Danger),
                };
                div()
                    .id(("doctor-check", ix))
                    .p(px(6.))
                    .rounded(px(2.))
                    .bg(color(cx, Role::Surface))
                    .border_1()
                    .border_color(color(cx, Role::Hairline))
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .child(
                                div()
                                    .text_xs()
                                    .font_weight(gpui::FontWeight::BOLD)
                                    .text_color(color(cx, role))
                                    .child(glyph),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .font_weight(gpui::FontWeight::BOLD)
                                    .text_color(color(cx, Role::Text))
                                    .child(SharedString::from(check.name.clone())),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(color(cx, role))
                                    .child(SharedString::from(format!("[{}]", check.level.to_uppercase()))),
                            ),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(color(cx, Role::TextSecondary))
                            .child(SharedString::from(check.message.clone())),
                    )
            });

            div().flex().flex_col().gap(px(4.)).children(items)
        };

        div()
            .flex()
            .flex_col()
            .gap(px(10.))
            .child(action_bar)
            .child(hud::frame("health checks", checks_list, cx))
            .into_any_element()
    }

    // --------------------------------------------------------------------------------- Actions & Requests
    pub(super) fn load_settings_data(&mut self, cx: &mut Context<Self>) {
        let session = self.session.read(cx);
        session.request("_cordata/trust/status", serde_json::json!({}), TRUST_STATUS_TAG);
        session.request("_cordata/settings/get", serde_json::json!({}), SETTINGS_GET_TAG);
        session.request("_cordata/settings/schema", serde_json::json!({}), SETTINGS_SCHEMA_TAG);
        session.request("_cordata/auto/readiness", serde_json::json!({}), READINESS_TAG);
    }

    pub(super) fn grant_trust(&mut self, cx: &mut Context<Self>) {
        self.session
            .read(cx)
            .request("_cordata/trust/grant", serde_json::json!({ "confirmed": true }), TRUST_GRANT_TAG);
    }

    pub(super) fn remove_rule(&mut self, rule: &str, cx: &mut Context<Self>) {
        self.session
            .read(cx)
            .request("_cordata/settings/rules/remove", serde_json::json!({ "rule": rule }), SETTINGS_RULE_REMOVE_TAG);
    }

    pub(super) fn submit_rule(&mut self, cx: &mut Context<Self>) {
        let rule_text = self.settings_view.rule_input.read(cx).value().to_string();
        let rule_trimmed = rule_text.trim();
        if rule_trimmed.is_empty() {
            self.settings_view.set_notice("Rule cannot be empty".into(), true);
            cx.notify();
            return;
        }
        let kind = self.settings_view.rule_kind.clone();
        let scope = self.settings_view.rule_scope.clone();
        self.session.read(cx).request(
            "_cordata/settings/rules/add",
            serde_json::json!({ "kind": kind, "rule": rule_trimmed, "scope": scope }),
            SETTINGS_RULE_ADD_TAG,
        );
    }

    pub(super) fn set_setting(&mut self, scope: &str, key: &str, value: Value, cx: &mut Context<Self>) {
        self.session.read(cx).request(
            "_cordata/settings/set",
            serde_json::json!({ "scope": scope, "key": key, "value": value }),
            SETTINGS_SET_TAG,
        );
    }

    pub(super) fn run_doctor(&mut self, cx: &mut Context<Self>) {
        self.settings_view.doctor_loading = true;
        self.session.read(cx).request("_cordata/doctor", serde_json::json!({}), DOCTOR_TAG);
        cx.notify();
    }
}
