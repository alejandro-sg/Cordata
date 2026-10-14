//! Settings (⌘, or ⚙): a 960×680 modal with a navigation column and a section on the right. Changes apply at once and
//! persist: the window's own preferences in `prefs.toml`, the harness's in its settings files through
//! `_cordata/settings/*`, trust through `_cordata/trust/*`, the API key in the Keychain.

use cordata_theme::tokens as t;
use cordata_view::shown;
use gpui::prelude::FluentBuilder as _;
use gpui::{AnyElement, Context, FontWeight, InteractiveElement as _, IntoElement, KeyDownEvent, ParentElement, SharedString, StatefulInteractiveElement as _, Styled, Window, anchored, deferred, div, px, relative};
use gpui_component::input::Input;
use serde_json::{Value, json};

use super::{Confirm, Overlay, Workspace, conversation::always_prefix, tilde};
use crate::theme::{amber, c, inset_ring, line, mono, tk};
use crate::ui::{self, Dot, Kind};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Section {
    #[default]
    General,
    Models,
    Permissions,
    Customize,
    Automations,
    Appearance,
    Shortcuts,
    Connections,
    Account,
    Advanced,
}

impl Section {
    pub const ALL: [Section; 10] = [Section::General, Section::Models, Section::Permissions, Section::Customize, Section::Automations, Section::Appearance, Section::Shortcuts, Section::Connections, Section::Account, Section::Advanced];

    fn icon(self) -> &'static str {
        match self {
            Section::General => "◎",
            Section::Models => "◇",
            Section::Permissions => "⛨",
            Section::Customize => "✎",
            Section::Automations => "⏱",
            Section::Appearance => "◐",
            Section::Shortcuts => "⌘",
            Section::Connections => "⇄",
            Section::Account => "◉",
            Section::Advanced => "⚑",
        }
    }

    fn title(self) -> &'static str {
        match self {
            Section::General => "General",
            Section::Models => "Models",
            Section::Permissions => "Permissions",
            Section::Customize => "Customize",
            Section::Automations => "Automations",
            Section::Appearance => "Appearance",
            Section::Shortcuts => "Shortcuts",
            Section::Connections => "Connections",
            Section::Account => "Account",
            Section::Advanced => "Advanced",
        }
    }

    fn description(self) -> &'static str {
        match self {
            Section::General => "Launch, sending and notifications.",
            Section::Models => "Which models answer, and how much they think.",
            Section::Permissions => "What Cordata may do without asking.",
            Section::Customize => "Instructions, prompt commands and learned skills.",
            Section::Automations => "Prompts the daemon runs on a schedule.",
            Section::Appearance => "Theme, haze and density.",
            Section::Shortcuts => "Every key the window answers to.",
            Section::Connections => "The hub and the MCP servers this project uses.",
            Section::Account => "Your key and usage.",
            Section::Advanced => "Logs, diagnostics and resets.",
        }
    }
}

/// The selects in the modal (one open at a time).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Select {
    OpenOnLaunch,
    DefaultModel,
    FastModel,
    MaxTokens,
    CodeFont,
    CompletionModel,
}

/// The read-only shortcut reference.
pub const SHORTCUTS: [(&str, &str); 16] = [
    ("Command palette", "⌘K"),
    ("Switch project", "⌘O"),
    ("New conversation", "⌘N"),
    ("Switch conversation", "⌘1–9"),
    ("Toggle side panel", "⌘\\"),
    ("Terminal", "⌃`"),
    ("Settings", "⌘,"),
    ("Send / approve the card", "↵"),
    ("New line", "⇧↵"),
    ("Deny / close overlay", "esc"),
    ("Stop the run", "esc esc"),
    ("Cycle permission mode", "⇧Tab"),
    ("Start mission run", "⌘↵"),
    ("Quick ask, from any app", "⌃⌥Space"),
    ("Find a file", "⌘P"),
    ("Save the open file", "⌘S"),
];

impl Workspace {
    fn value(&self, key: &str) -> Option<&Value> {
        self.settings_view.values.get(key)
    }

    fn value_str(&self, key: &str) -> String {
        self.value(key).map(|v| v.as_str().map(str::to_owned).unwrap_or_else(|| v.to_string())).unwrap_or_default()
    }

    /// A harness setting, written to the person's own settings file.
    pub(super) fn set_user_setting(&mut self, key: &str, value: Value, cx: &mut Context<Self>) {
        self.settings_view.values.insert(key.to_owned(), value.clone());
        self.set_setting("user", key, value, cx);
        cx.notify();
    }

    fn save_prefs(&mut self, cx: &mut Context<Self>) {
        self.prefs.save();
        cx.set_global(self.prefs.clone());
        cx.notify();
    }

    pub(super) fn settings_keys(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if event.keystroke.key == "escape" {
            if self.open_select.is_some() {
                self.open_select = None;
                cx.notify();
            } else {
                self.close_overlay(Overlay::Settings, window, cx);
            }
            cx.stop_propagation();
        }
    }

    pub(super) fn settings_modal(&self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let tk = tk(cx);
        let size = window.viewport_size();
        let w = t::SETTINGS_W.min(size.width.as_f32() - 48.);
        let h = t::SETTINGS_H.min(size.height.as_f32() - 48.);
        let nav: Vec<AnyElement> = Section::ALL.iter().map(|&section| {
            let active = self.settings_section == section;
            let hover = line(cx, t::HOVER);
            div()
                .id(("settings-nav", section as usize))
                .role(gpui::Role::Tab)
                .aria_selected(active)
                .flex()
                .items_center()
                .gap(px(10.))
                .h(px(32.))
                .px(px(10.))
                .rounded(px(t::RADIUS_ROW))
                .cursor_pointer()
                .whitespace_nowrap()
                .text_size(px(t::TEXT_UI))
                .when(active, |r| r.bg(amber(cx, t::AMBER_FILL_MID)).shadow(inset_ring(amber(cx, t::AMBER_BORDER))).text_color(c(tk.text_primary)))
                .when(!active, move |r| r.text_color(c(tk.text_secondary)).hover(move |s| s.bg(hover)))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.settings_section = section;
                    this.open_select = None;
                    this.on_settings_section(cx);
                    cx.notify();
                }))
                .child(div().w(px(16.)).text_color(c(if active { tk.amber } else { tk.text_muted })).child(section.icon()))
                .child(section.title())
                .into_any_element()
        }).collect();
        let section = self.settings_section;
        let body: Vec<AnyElement> = match section {
            Section::General => self.general_rows(cx),
            Section::Models => self.model_rows(cx),
            Section::Permissions => self.permission_rows(cx),
            Section::Customize => self.customize_rows(cx),
            Section::Automations => self.automation_rows(cx),
            Section::Appearance => self.appearance_rows(cx),
            Section::Shortcuts => SHORTCUTS.iter().map(|(label, keys)| setting_row(label, "", ui::keycap(*keys, cx).into_any_element(), cx)).collect(),
            Section::Connections => self.connection_rows(cx),
            Section::Account => self.account_rows(cx),
            Section::Advanced => self.advanced_rows(cx),
        };
        div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .child(
                div()
                    .id("settings")
                    .role(gpui::Role::Dialog)
                    .aria_label("Settings")
                    .key_context("Settings")
                    .track_focus(&self.settings_focus)
                    .capture_key_down(cx.listener(Self::settings_keys))
                    .w(px(w))
                    .h(px(h))
                    .flex()
                    .overflow_hidden()
                    .rounded(px(t::RADIUS_MODAL))
                    .border_1()
                    .border_color(line(cx, t::LINE_CONTROL_STRONG))
                    .bg(c(tk.bg_panel))
                    .shadow(crate::theme::modal_shadow(cx))
                    .child(
                        div()
                            .w(px(t::SETTINGS_NAV_W))
                            .flex_shrink_0()
                            .h_full()
                            .flex()
                            .flex_col()
                            .bg(c(tk.bg_nav_deep))
                            .border_r_1()
                            .border_color(line(cx, t::LINE_DEFAULT))
                            .child(div().h(px(t::SETTINGS_HEADER_H)).flex().items_center().px(px(16.)).child(ui::heading("settings", cx)))
                            .child(div().id("settings-nav-list").role(gpui::Role::TabList).flex().flex_col().gap(px(2.)).px(px(8.)).children(nav))
                            .child(div().flex_1())
                            .child(div().p(px(16.)).flex().flex_col().gap(px(2.)).child(ui::meta(SharedString::from(format!("Cordata v{}", env!("CARGO_PKG_VERSION"))), cx)).child(ui::meta("Changes save automatically", cx))),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .h(px(t::SETTINGS_HEADER_H))
                                    .flex_shrink_0()
                                    .flex()
                                    .items_center()
                                    .gap(px(12.))
                                    .px(px(24.))
                                    .border_b_1()
                                    .border_color(line(cx, t::LINE_DEFAULT))
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .flex()
                                            .flex_col()
                                            .child(div().text_size(px(t::TEXT_SETTINGS_TITLE)).font_weight(FontWeight::MEDIUM).text_color(c(tk.text_primary)).child(section.title()))
                                            .child(div().truncate().text_size(px(t::TEXT_SMALL)).text_color(c(tk.text_muted)).child(section.description())),
                                    )
                                    .child(
                                        div()
                                            .id("settings-close")
                                            .role(gpui::Role::Button)
                                            .aria_label("Close settings (esc)")
                                            .flex()
                                            .items_center()
                                            .gap(px(6.))
                                            .cursor_pointer()
                                            .on_click(cx.listener(|this, _, window, cx| this.close_overlay(Overlay::Settings, window, cx)))
                                            .child(ui::keycap("esc", cx))
                                            .child(div().text_color(c(tk.text_muted)).child("✕")),
                                    ),
                            )
                            .child(
                                div()
                                    .id("settings-body")
                                    .flex_1()
                                    .min_h_0()
                                    .overflow_y_scroll()
                                    .px(px(24.))
                                    .pb(px(24.))
                                    .when_some(self.settings_view.notice.clone(), |b, notice| {
                                        b.child(div().pt(px(12.)).text_size(px(t::TEXT_SMALL)).text_color(c(if self.settings_view.notice_error { tk.danger } else { tk.teal })).child(SharedString::from(shown(&notice))))
                                    })
                                    .children(body),
                            ),
                    ),
            )
            .into_any_element()
    }

    /// Loads what a section shows when it opens.
    pub(super) fn on_settings_section(&mut self, cx: &mut Context<Self>) {
        match self.settings_section {
            Section::Connections => self.load_plugins(cx),
            Section::Permissions => self.load_trusted_projects(cx),
            Section::Customize => self.load_skills(cx),
            Section::Automations => self.load_automations(cx),
            Section::Account => self.api_key_masked = crate::keychain::masked(),
            Section::Models => {
                let job = cx.background_executor().spawn(async { super::completions::installed_models() });
                cx.spawn(async move |this, cx| {
                    let models = job.await;
                    let _ = this.update(cx, |this, cx| {
                        this.ollama_models = models;
                        cx.notify();
                    });
                })
                .detach();
            }
            _ => {}
        }
    }

    // ------------------------------------------------------------------------------------------------ controls

    fn select(&self, which: Select, value: &str, options: Vec<(String, String)>, cx: &mut Context<Self>) -> AnyElement {
        let tk = tk(cx);
        let open = self.open_select == Some(which);
        let menu = open.then(|| {
            let rows = options.into_iter().enumerate().map(|(index, (option, label))| {
                let current = option == value;
                ui::row(("select-option", index), current, cx)
                    .h(px(30.))
                    .font_family(mono(cx))
                    .text_size(px(t::TEXT_SMALL))
                    .text_color(c(if current { tk.amber } else { tk.text_primary }))
                    .child(SharedString::from(label))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_select = None;
                        this.pick(which, option.clone(), window, cx);
                    }))
            });
            deferred(anchored().snap_to_window_with_margin(px(8.)).child(ui::popover(cx).id("select-menu").role(gpui::Role::ListBox).mt(px(4.)).min_w(px(t::SELECT_MIN_W)).max_h(px(280.)).overflow_y_scroll().p(px(4.)).children(rows).on_mouse_down_out(cx.listener(|this, _, _, cx| {
                this.open_select = None;
                cx.notify();
            }))))
            .with_priority(2)
        });
        let label = self.select_label(which, value);
        div()
            .flex()
            .flex_col()
            .items_end()
            .child(ui::select_trigger(("select", which as usize), &label, cx).aria_expanded(open).on_click(cx.listener(move |this, _, _, cx| {
                this.open_select = if this.open_select == Some(which) { None } else { Some(which) };
                cx.notify();
            })))
            .children(menu)
            .into_any_element()
    }

    fn select_label(&self, which: Select, value: &str) -> String {
        match which {
            Select::OpenOnLaunch => match value {
                "home" => "home folder",
                "picker" => "project picker",
                _ => "last project",
            }
            .into(),
            Select::FastModel if value.is_empty() => "same as main".into(),
            Select::MaxTokens if value.is_empty() => "16384".into(),
            Select::CodeFont => format!("{value} px"),
            Select::CompletionModel if value.is_empty() => "off".into(),
            _ if value.is_empty() => "—".into(),
            _ => value.into(),
        }
    }

    fn pick(&mut self, which: Select, value: String, _window: &mut Window, cx: &mut Context<Self>) {
        match which {
            Select::OpenOnLaunch => {
                self.prefs.open_on_launch = value;
                self.save_prefs(cx);
            }
            Select::DefaultModel => {
                self.set_user_setting("model", json!(value), cx);
                self.session.update(cx, |s, cx| s.set_config("model", &value, cx));
            }
            Select::FastModel => self.set_user_setting("tiers.fast", json!(value), cx),
            Select::MaxTokens => self.set_user_setting("model.maxTokens", json!(value.parse::<f64>().unwrap_or(16384.0)), cx),
            Select::CodeFont => {
                self.prefs.code_font_size = value.parse().unwrap_or(13.0);
                self.save_prefs(cx);
            }
            Select::CompletionModel => {
                self.prefs.completion_model = value;
                self.save_prefs(cx);
                self.apply_completions_to_all(cx);
            }
        }
    }

    fn segmented(&self, id: &'static str, options: &[(&'static str, &'static str)], selected: &str, on_pick: fn(&mut Workspace, &'static str, &mut Context<Workspace>), cx: &mut Context<Self>) -> AnyElement {
        let segments: Vec<AnyElement> = options
            .iter()
            .enumerate()
            .map(|(index, &(value, label))| {
                ui::segment((id, index), label, value == selected, cx)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        on_pick(this, value, cx);
                        cx.notify();
                    }))
                    .into_any_element()
            })
            .collect();
        ui::segmented_row(cx).children(segments).into_any_element()
    }

    fn toggle(&self, id: &'static str, on: bool, label: &'static str, on_flip: fn(&mut Workspace, bool, &mut Context<Workspace>), cx: &mut Context<Self>) -> AnyElement {
        ui::switch(id, on, cx).aria_label(label).on_click(cx.listener(move |this, _, _, cx| on_flip(this, !on, cx))).into_any_element()
    }

    // ------------------------------------------------------------------------------------------------ sections

    fn general_rows(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let launch = self.select(Select::OpenOnLaunch, &self.prefs.open_on_launch.clone(), vec![("last".into(), "last project".into()), ("home".into(), "home folder".into()), ("picker".into(), "project picker".into())], cx);
        let send = self.segmented("send-with", &[("enter", "↵"), ("cmd-enter", "⌘↵")], &self.prefs.send_with.clone(), |this, v, cx| {
            this.prefs.send_with = v.into();
            this.save_prefs(cx);
            this.apply_send_with(cx);
        }, cx);
        let notify = self.toggle("notify-approval", self.prefs.notify_approval, "Notify when approval is needed", |this, on, cx| {
            this.prefs.notify_approval = on;
            this.save_prefs(cx);
        }, cx);
        let sound = self.toggle("sound-done", self.prefs.sound_on_completion, "Sound on completion", |this, on, cx| {
            this.prefs.sound_on_completion = on;
            this.save_prefs(cx);
        }, cx);
        let hotkey = self.toggle("quick-ask", self.prefs.quick_ask_hotkey, "Quick ask hotkey", |this, on, cx| {
            this.prefs.quick_ask_hotkey = on;
            this.save_prefs(cx);
            if !crate::platform::set_hotkey(on) && on {
                this.settings_view.set_notice("⌃⌥Space is taken by another app.".into(), true);
            }
        }, cx);
        vec![
            setting_row("Open on launch", "What the window shows when Cordata starts.", launch, cx),
            setting_row("Quick ask (⌃⌥Space)", "From any app: bring Cordata forward with the composer ready.", hotkey, cx),
            setting_row("Send message with", "The other key adds a new line.", send, cx),
            sub_heading("notifications", cx),
            setting_row("Notify when approval is needed", "A system notification when a command waits for you and the window is in the back.", notify, cx),
            setting_row("Sound on completion", "A short sound when a turn finishes.", sound, cx),
        ]
    }

    fn model_choices(&self, cx: &Context<Self>) -> Vec<(String, String)> {
        self.session.read(cx).view.config("model").map(|o| o.choices.iter().map(|c| (c.value.clone(), shown(&c.name))).collect()).unwrap_or_default()
    }

    fn model_rows(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let choices = self.model_choices(cx);
        let current = {
            let set = self.value_str("model");
            if set.is_empty() { self.session.read(cx).view.config("model").map(|o| o.current_value.clone()).unwrap_or_default() } else { set }
        };
        let default_model = if choices.is_empty() { note_control("Connect to choose a model.", cx) } else { self.select(Select::DefaultModel, &current, choices.clone(), cx) };
        let mut fast_options = vec![(String::new(), "same as main".to_owned())];
        fast_options.extend(choices);
        let fast = self.select(Select::FastModel, &self.value_str("tiers.fast"), fast_options, cx);
        let thinking = self.value_str("model.thinking");
        let thinking = self.segmented("thinking", &[("off", "off"), ("auto", "auto"), ("always", "always")], if thinking.is_empty() { "off" } else { &thinking }, |this, v, cx| this.set_user_setting("model.thinking", json!(v), cx), cx);
        let max_tokens = self.value("model.maxTokens").and_then(Value::as_f64).map(|v| format!("{v:.0}")).unwrap_or_default();
        let max_tokens = self.select(Select::MaxTokens, &max_tokens, ["4096", "8192", "16384", "32000", "64000"].iter().map(|v| ((*v).to_owned(), (*v).to_owned())).collect(), cx);
        let compact = self.value_str("context.autoCompact");
        let compact = self.segmented("auto-compact", &[("80", "80%"), ("90", "90%"), ("off", "off")], &compact, |this, v, cx| this.set_user_setting("context.autoCompact", json!(v), cx), cx);
        let mut local = vec![(String::new(), "off".to_owned())];
        local.extend(self.ollama_models.iter().map(|m| (m.clone(), m.clone())));
        let completion = self.select(Select::CompletionModel, &self.prefs.completion_model.clone(), local, cx);
        vec![
            setting_row("Default model", "Used by new conversations; the header's model chip changes the current one.", default_model, cx),
            setting_row("Inline completions", "Ghost text in the editor from a local Ollama model (a coder model with fill-in-the-middle, e.g. qwen2.5-coder). Code stays on this Mac. Tab accepts.", completion, cx),
            setting_row("Fast model", "Titles, summaries and compaction.", fast, cx),
            setting_row("Extended thinking", "Anthropic models only. Auto: adaptive thinking (the model decides). Always: adaptive at high effort every turn.", thinking, cx),
            setting_row("Max output tokens", "The longest single reply.", max_tokens, cx),
            setting_row("Auto-compact at", "Summarise older turns when the context window is this full (unset: 70%).", compact, cx),
        ]
    }

    fn permission_rows(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let tk = tk(cx);
        let mode = self.value_str("permissions.defaultMode");
        let mode = self.segmented("default-mode", &[("build", "ask first"), ("edit", "auto-edit"), ("plan", "plan only")], if mode.is_empty() { "build" } else { &mode }, |this, v, cx| this.set_user_setting("permissions.defaultMode", json!(v), cx), cx);
        let allowed: Vec<AnyElement> = self
            .settings_view
            .rules
            .iter()
            .filter(|r| r.kind == "allow" && r.scope == "user")
            .enumerate()
            .map(|(index, rule)| {
                let text = rule.text.clone();
                removable_chip(("allow-chip", index), &always_prefix(&rule.text), cx).on_click(cx.listener(move |this, _, _, cx| this.remove_rule(&text.clone(), cx))).into_any_element()
            })
            .collect();
        let adding = self.adding_allow;
        let add: AnyElement = if adding {
            div()
                .flex()
                .items_center()
                .gap(px(6.))
                .child(div().w(px(180.)).h(px(26.)).flex().items_center().px(px(8.)).rounded(px(t::RADIUS_CONTROL)).border_1().border_color(amber(cx, t::AMBER_BORDER_STRONG)).font_family(mono(cx)).text_size(px(t::TEXT_SMALL)).child(Input::new(&self.allow_input).appearance(false)))
                .child(ui::button("allow-save", "Add", Kind::Tint, cx).h(px(26.)).on_click(cx.listener(|this, _, window, cx| this.add_allowed_command(window, cx))))
                .into_any_element()
        } else {
            ui::button("allow-add", "+ add", Kind::Ghost, cx).h(px(26.)).on_click(cx.listener(|this, _, window, cx| {
                this.adding_allow = true;
                this.allow_input.update(cx, |s, cx| s.focus(window, cx));
                cx.notify();
            })).into_any_element()
        };
        let chips = div().flex().flex_wrap().justify_end().gap(px(6.)).max_w(px(420.)).children(allowed).child(add).into_any_element();
        let trusted: Vec<AnyElement> = self
            .trusted_projects
            .iter()
            .enumerate()
            .map(|(index, path)| {
                let target = path.clone();
                removable_chip(("trusted-chip", index), &tilde(std::path::Path::new(path)), cx).on_click(cx.listener(move |this, _, _, cx| this.revoke_trust(Some(target.clone()), cx))).into_any_element()
            })
            .collect();
        let trusted_el = if trusted.is_empty() { note_control("No trusted projects.", cx) } else { div().flex().flex_wrap().justify_end().gap(px(6.)).max_w(px(420.)).children(trusted).into_any_element() };
        vec![
            setting_row("Default mode for new projects", "How much a new conversation may do before it asks.", mode, cx),
            setting_row("Always-allowed commands", "Shell commands that never ask. \"Always allow\" on an approval adds one here.", chips, cx),
            setting_row("Ask before network access", "Enforced by the harness's tool policy (THREAT_MODEL.md); not a per-window switch yet.", status_control(Dot::Ok, "always on", cx), cx),
            setting_row("Sandbox shell commands", "Self-heal verification runs in the sandbox; other shell commands follow the approval rules above.", status_control(Dot::Muted, "per approval", cx), cx),
            setting_row("Trusted projects", "Removing one revokes its trust: its settings, rules and MCP servers stop applying.", trusted_el, cx),
            div().pt(px(6.)).text_size(px(t::TEXT_SMALL)).text_color(c(tk.text_muted)).child("Raw rules, gated keys and the doctor are under Advanced → Harness.").into_any_element(),
        ]
    }

    fn appearance_rows(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let theme = self.segmented("theme", &[("2049", "2049"), ("graphite", "graphite"), ("dim", "dim")], &self.prefs.theme.clone(), |this, v, cx| {
            this.prefs.theme = v.into();
            this.save_prefs(cx);
            crate::theme::set_theme(v, cx);
        }, cx);
        let haze = self.toggle("haze", self.prefs.haze, "Ambient haze", |this, on, cx| {
            this.prefs.haze = on;
            this.save_prefs(cx);
        }, cx);
        let density = self.segmented("density", &[("compact", "compact"), ("comfortable", "comfortable")], &self.prefs.density.clone(), |this, v, cx| {
            this.prefs.density = v.into();
            this.save_prefs(cx);
        }, cx);
        let size = format!("{:.0}", self.prefs.code_font_size);
        let font = self.select(Select::CodeFont, &size, ["11", "12", "13", "14", "15", "16"].iter().map(|v| ((*v).to_owned(), format!("{v} px"))).collect(), cx);
        vec![
            setting_row("Theme", "2049 is the reference look; graphite is neutral; dim lifts the ground for bright rooms.", theme, cx),
            setting_row("Ambient haze", "The amber and teal glow behind the conversation. Off with Reduce Motion or Reduce Transparency.", haze, cx),
            setting_row("Density", "Spacing in lists and the transcript.", density, cx),
            setting_row("Code font size", "The editor and code blocks.", font, cx),
        ]
    }

    fn connection_rows(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let tk = tk(cx);
        let (kind, health) = self.hub_health(cx);
        let mut rows = vec![
            setting_row("Hub endpoint", "Where this window's conversations run. Set by the daemon (CORDATA_HOME); --transport stdio runs a private backend.", mono_value(&self.endpoint(), cx), cx),
            setting_row("Hub status", "", status_control(kind, &health, cx), cx),
            sub_heading("mcp servers", cx),
        ];
        if self.plugins.is_empty() {
            rows.push(div().py(px(12.)).text_size(px(t::TEXT_SMALL)).text_color(c(tk.text_muted)).child("No MCP servers configured for this project.").into_any_element());
        }
        for (index, plugin) in self.plugins.iter().enumerate() {
            let name = plugin.get("name").and_then(Value::as_str).unwrap_or_default().to_owned();
            let status = plugin.get("status").and_then(Value::as_str).unwrap_or_default().to_owned();
            let error = plugin.get("error").and_then(Value::as_str).filter(|e| !e.is_empty()).map(str::to_owned);
            let tools = plugin.get("tools").and_then(Value::as_array).map_or(0, Vec::len);
            let transport = plugin.get("transport").and_then(Value::as_str).unwrap_or_default();
            let source = plugin.get("source").and_then(Value::as_str).unwrap_or_default();
            let on = matches!(status.as_str(), "running" | "starting" | "enabled");
            let desc = match &error {
                Some(e) => shown(e),
                None => format!("{transport} · {source} · {tools} tool{}", if tools == 1 { "" } else { "s" }),
            };
            let target = name.clone();
            let switch = ui::switch(("mcp-switch", index), on, cx).aria_label(SharedString::from(format!("{name} enabled"))).on_click(cx.listener(move |this, _, _, cx| this.toggle_plugin(&target, !on, cx))).into_any_element();
            rows.push(setting_row_owned(name, desc, error.is_some(), switch, cx));
        }
        rows.push(
            div()
                .pt(px(10.))
                .child(ui::button("mcp-add", "+ Add server", Kind::Outline, cx).tooltip(ui::tip("Opens the project .mcp.json in the editor")).on_click(cx.listener(|this, _, window, cx| this.add_mcp_server(window, cx))))
                .into_any_element(),
        );
        rows
    }

    fn account_rows(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let signed_in = self.api_key_masked.is_some();
        let key: AnyElement = div()
            .flex()
            .items_center()
            .gap(px(6.))
            .child(div().w(px(200.)).h(px(28.)).flex().items_center().px(px(8.)).rounded(px(t::RADIUS_CONTROL)).bg(c(tk(cx).bg_input)).border_1().border_color(line(cx, t::LINE_CONTROL_STRONG)).font_family(mono(cx)).text_size(px(t::TEXT_SMALL)).child(Input::new(&self.api_key_input).appearance(false)))
            .child(ui::button("api-key-save", "Save", Kind::Tint, cx).on_click(cx.listener(|this, _, window, cx| this.save_api_key(window, cx))))
            .into_any_element();
        let (in_t, out_t) = self.session.read(cx).view.session_tokens;
        vec![
            setting_row("Signed in", "An Anthropic API key in the Keychain is what signs Cordata in.", status_control(if signed_in { Dot::Ok } else { Dot::Muted }, if signed_in { "key stored" } else { "no key" }, cx), cx),
            setting_row_owned("API key".into(), format!("Stored in the login Keychain only ({}). Used by the next backend that starts.{}", crate::keychain::SERVICE, self.api_key_masked.as_ref().map(|m| format!(" Current: {m}")).unwrap_or_default()), false, key, cx),
            setting_row("Usage this session", "Monthly usage is not tracked by the harness; these are this conversation's tokens.", mono_value(&format!("{in_t} in · {out_t} out"), cx), cx),
            setting_row("Sign out", "Removes the key from the Keychain.", ui::button("sign-out", "Sign out", Kind::Danger, cx).on_click(cx.listener(|this, _, window, cx| this.ask_confirm(Confirm::SignOut, window, cx))).into_any_element(), cx),
        ]
    }

    fn advanced_rows(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let diag = self.toggle("diagnostics", self.prefs.share_diagnostics, "Share anonymous diagnostics", |this, on, cx| {
            this.prefs.share_diagnostics = on;
            this.save_prefs(cx);
        }, cx);
        vec![
            setting_row("Logs folder", "The harness's state, trajectories and logs (~/.cordata).", ui::button("open-logs", "Reveal in Finder", Kind::Outline, cx).on_click(cx.listener(|_, _, _, cx| cx.reveal_path(&super::cordata_home()))).into_any_element(), cx),
            setting_row("Share anonymous diagnostics", "Nothing is sent today; this records your choice for when it exists.", diag, cx),
            setting_row("Clear conversation history", "Not available yet: the hub has no delete hook. Remove ~/.cordata/sessions by hand if you must.", ui::button("clear-history", "Clear…", Kind::Danger, cx).opacity(0.5).cursor_default().into_any_element(), cx),
            setting_row("Reset all settings", "Restores this window's preferences (theme, layout, recents). Harness settings files are untouched.", ui::button("reset-prefs", "Reset…", Kind::Danger, cx).on_click(cx.listener(|this, _, window, cx| this.ask_confirm(Confirm::ResetPrefs, window, cx))).into_any_element(), cx),
            sub_heading("harness", cx),
            div().pt(px(8.)).h(px(520.)).child(self.settings_panel(cx)).into_any_element(),
        ]
    }

    // ------------------------------------------------------------------------------------------------ actions

    pub(super) fn add_allowed_command(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.allow_input.read(cx).value().trim().to_owned();
        self.adding_allow = false;
        self.allow_input.update(cx, |s, cx| s.set_value("", window, cx));
        if text.is_empty() {
            cx.notify();
            return;
        }
        let rule = if text.contains('(') { text } else { format!("shell({text})") };
        self.session.read(cx).request("_cordata/settings/rules/add", json!({ "kind": "allow", "rule": rule, "scope": "user" }), super::settings_view::SETTINGS_RULE_ADD_TAG);
        cx.notify();
    }

    pub(super) fn save_api_key(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let key = self.api_key_input.read(cx).value().to_string();
        // The field is cleared whatever happens: the key must not sit in the window.
        self.api_key_input.update(cx, |s, cx| s.set_value("", window, cx));
        match crate::keychain::store(&key) {
            Ok(()) => {
                self.api_key_masked = crate::keychain::masked();
                self.settings_view.set_notice("Key saved to the Keychain. Restart the backend to use it.".into(), false);
            }
            Err(error) => self.settings_view.set_notice(error, true),
        }
        cx.notify();
    }

    fn toggle_plugin(&mut self, name: &str, on: bool, cx: &mut Context<Self>) {
        let method = if on { "_cordata/plugins/enable" } else { "_cordata/plugins/disable" };
        self.session.read(cx).request(method, json!({ "name": name, "persist": true }), super::PLUGIN_TOGGLE_TAG);
        if let Some(row) = self.plugins.iter_mut().find(|p| p.get("name").and_then(Value::as_str) == Some(name))
            && let Some(obj) = row.as_object_mut()
        {
            obj.insert("status".into(), json!(if on { "starting" } else { "disabled" }));
        }
        cx.notify();
    }

    /// Opens the project's `.mcp.json` (created empty when missing) in the editor; project servers start once the
    /// project is trusted.
    fn add_mcp_server(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let path = self.cwd.join(".mcp.json");
        if !path.exists()
            && let Err(error) = std::fs::write(&path, "{\n  \"mcpServers\": {}\n}\n")
        {
            self.settings_view.set_notice(format!("Could not create .mcp.json: {error}"), true);
            cx.notify();
            return;
        }
        self.close_overlay(Overlay::Settings, window, cx);
        self.set_panel(true, cx);
        self.open_file(".mcp.json", None, window, cx);
    }
}

/// One settings row: the label and its description on the left, the control on the right.
fn setting_row(label: &str, description: &str, control: AnyElement, cx: &mut Context<Workspace>) -> AnyElement {
    setting_row_owned(label.to_owned(), description.to_owned(), false, control, cx)
}

fn setting_row_owned(label: String, description: String, error: bool, control: AnyElement, cx: &mut Context<Workspace>) -> AnyElement {
    let tk = tk(cx);
    div()
        .flex()
        .items_center()
        .gap(px(24.))
        .py(px(t::ROW_PAD_Y))
        .border_b_1()
        .border_color(line(cx, t::LINE_SUBTLE))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(px(3.))
                .child(div().text_size(px(t::TEXT_TITLE)).text_color(c(tk.text_primary)).child(SharedString::from(label)))
                .when(!description.is_empty(), |d| d.child(div().text_size(px(t::TEXT_SMALL)).line_height(relative(1.45)).text_color(c(if error { tk.danger } else { tk.text_muted })).child(SharedString::from(description)))),
        )
        .child(div().flex_shrink_0().child(control))
        .into_any_element()
}

fn sub_heading(text: &str, cx: &mut Context<Workspace>) -> AnyElement {
    div().pt(px(22.)).pb(px(4.)).child(ui::section_label(text, cx)).into_any_element()
}

fn status_control(kind: Dot, text: &str, cx: &mut Context<Workspace>) -> AnyElement {
    div().flex().items_center().gap(px(8.)).child(ui::dot(kind, cx)).child(div().whitespace_nowrap().font_family(mono(cx)).text_size(px(t::TEXT_SMALL)).text_color(c(tk(cx).text_secondary)).child(SharedString::from(text.to_owned()))).into_any_element()
}

fn mono_value(text: &str, cx: &mut Context<Workspace>) -> AnyElement {
    div().max_w(px(320.)).truncate().font_family(mono(cx)).text_size(px(t::TEXT_SMALL)).text_color(c(tk(cx).text_secondary)).child(SharedString::from(text.to_owned())).into_any_element()
}

fn note_control(text: &str, cx: &mut Context<Workspace>) -> AnyElement {
    div().text_size(px(t::TEXT_SMALL)).text_color(c(tk(cx).text_muted)).child(SharedString::from(text.to_owned())).into_any_element()
}

fn removable_chip(id: impl Into<gpui::ElementId>, text: &str, cx: &mut Context<Workspace>) -> gpui::Stateful<gpui::Div> {
    let tk = tk(cx);
    let hover = c(tk.danger);
    div()
        .id(id)
        .role(gpui::Role::Button)
        .aria_label(SharedString::from(format!("Remove {text}")))
        .flex()
        .items_center()
        .gap(px(6.))
        .h(px(26.))
        .px(px(8.))
        .rounded(px(t::RADIUS_CONTROL))
        .border_1()
        .border_color(line(cx, t::LINE_CONTROL_STRONG))
        .whitespace_nowrap()
        .cursor_pointer()
        .font_family(mono(cx))
        .text_size(px(t::TEXT_SMALL))
        .text_color(c(tk.text_secondary))
        .child(div().max_w(px(220.)).truncate().child(SharedString::from(text.to_owned())))
        .child(div().text_color(c(tk.text_faint)).hover(move |s| s.text_color(hover)).child("✕"))
}
