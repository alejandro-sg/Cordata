//! Settings (⌘, or ⚙): a 960×680 modal with a navigation column and a section on the right. Changes apply at once and
//! persist: the window's own preferences in `prefs.toml`, the harness's in its settings files through
//! `_cordata/settings/*`, trust through `_cordata/trust/*`, the API key in the Keychain.

use cordata_theme::tokens as t;
use cordata_view::shown;
use gpui::prelude::FluentBuilder as _;
use gpui::{AnyElement, Context, FontWeight, InteractiveElement as _, IntoElement, KeyDownEvent, ParentElement, SharedString, StatefulInteractiveElement as _, Styled, Window, anchored, deferred, div, px, relative};
use gpui_component::input::Input;

use super::{Confirm, Overlay, Workspace};
use crate::theme::{c, line, mono, tk};
use crate::ui::{self, Dot, Kind};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Section {
    #[default]
    General,
    Appearance,
    Shortcuts,
    Account,
    Agents,
    Mcp,
    Advanced,
}

impl Section {
    pub const ALL: [Section; 7] = [
        Section::General,
        Section::Appearance,
        Section::Shortcuts,
        Section::Account,
        Section::Agents,
        Section::Mcp,
        Section::Advanced,
    ];

    fn title(self) -> &'static str {
        match self {
            Section::General => "General",
            Section::Appearance => "Appearance",
            Section::Shortcuts => "Shortcuts",
            Section::Account => "Account",
            Section::Agents => "Agents & Delegation",
            Section::Mcp => "MCP & Marketplace",
            Section::Advanced => "Advanced",
        }
    }

    /// Words the settings search matches beyond the title and description: the rows each section holds.
    fn keywords(self) -> &'static str {
        match self {
            Section::General => "launch startup quick ask hotkey send enter cost notifications sound meter context tokens latency instruments",
            Section::Appearance => "theme dark light ink paper density font size",
            Section::Shortcuts => "keys keyboard bindings hotkeys change sheet triage rack gate",
            Section::Account => "api key anthropic sign out usage keychain",
            Section::Agents => "agents claude codex gemini cursor delegation quota workers assistants",
            Section::Mcp => "mcp tools servers model context protocol catalog approve rea github git fetch memory",
            Section::Advanced => "logs diagnostics reset clear history",
        }
    }

    /// Whether every word of `query` appears in this section's title, description or keywords.
    pub fn matches(self, query: &str) -> bool {
        let haystack = format!("{} {} {}", self.title(), self.description(), self.keywords()).to_lowercase();
        query.split_whitespace().all(|word| haystack.contains(&word.to_lowercase()))
    }

    fn description(self) -> &'static str {
        match self {
            Section::General => "Launch, sending and notifications.",
            Section::Appearance => "Theme and density.",
            Section::Shortcuts => "Every key the window answers to.",
            Section::Account => "Your key and usage.",
            Section::Agents => "Installed coding assistants, delegation and usage quotas.",
            Section::Mcp => "Model Context Protocol tools, discovery and official marketplace.",
            Section::Advanced => "Logs, diagnostics and resets.",
        }
    }
}

/// The selects in the modal (one open at a time).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Select {
    OpenOnLaunch,
    CodeFont,
    QuickAskKey,
}

/// Its keys are the person's choice, shown live rather than from this table.
const QUICK_ASK_SHORTCUT: &str = "Quick ask, from any app";

/// The read-only shortcut reference.
pub const SHORTCUTS: [(&str, &str); 19] = [
    ("Command palette", "⌘K"),
    ("Switch project", "⌘O"),
    ("New conversation", "⌘N"),
    ("Switch conversation", "⌘1–9"),
    ("Multi-agent triage rack", "⌘0"),
    ("Change sheet / Transcript", "⌘D"),
    ("Toggle project sidebar", "⌘B"),
    ("Toggle side panel", "⌘\\"),
    ("Terminal", "⌃`"),
    ("Settings", "⌘,"),
    ("Send / approve routine card", "↵"),
    ("Confirm high-risk gate", "⌘Y"),
    ("New line", "⇧↵"),
    ("Deny / close overlay", "esc"),
    ("Stop the run", "esc esc"),
    (QUICK_ASK_SHORTCUT, "⌃⌥Space"),
    ("Find a file", "⌘P"),
    ("Save the open file", "⌘S"),
    ("Attach files or images", "⇧⌘A"),
];

impl Workspace {
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

    /// A new search: when the open section no longer matches, show the first one that does.
    pub(super) fn on_settings_search(&mut self, query: &str, cx: &mut Context<Self>) {
        if !self.settings_section.matches(query)
            && let Some(&first) = Section::ALL.iter().find(|s| s.matches(query))
        {
            self.settings_section = first;
            self.open_select = None;
            self.on_settings_section(cx);
        }
        cx.notify();
    }

    pub(super) fn settings_modal(&self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let tk = tk(cx);
        let size = window.viewport_size();
        let w = t::SETTINGS_W.min(size.width.as_f32() - 48.);
        let h = t::SETTINGS_H.min(size.height.as_f32() - 48.);
        let query = self.settings_search.read(cx).value().to_string();
        let nav: Vec<AnyElement> = Section::ALL.iter().filter(|s| s.matches(&query)).map(|&section| {
            let active = self.settings_section == section;
            let (hover, selected) = (line(cx, t::HOVER), line(cx, t::HOVER_STRONG));
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
                .font_weight(if active { FontWeight::MEDIUM } else { FontWeight::NORMAL })
                .when(active, move |r| r.bg(selected).text_color(c(tk.text_primary)))
                .when(!active, move |r| r.text_color(c(tk.text_secondary)).hover(move |s| s.bg(hover)))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.settings_section = section;
                    this.open_select = None;
                    this.on_settings_section(cx);
                    cx.notify();
                }))
                .child(section.title())
                .into_any_element()
        }).collect();
        let section = self.settings_section;
        let body: Vec<AnyElement> = match section {
            Section::General => self.general_rows(cx),
            Section::Appearance => self.appearance_rows(cx),
            Section::Shortcuts => {
                let quick_ask = self.quick_ask_combo().label;
                SHORTCUTS
                    .iter()
                    .map(|(label, keys)| {
                        let keys: SharedString = if *label == QUICK_ASK_SHORTCUT { quick_ask.clone().into() } else { (*keys).into() };
                        setting_row(label, "", ui::meta(keys, cx).into_any_element(), cx)
                    })
                    .collect()
            }
            Section::Account => self.account_rows(cx),
            Section::Agents => vec![div().pt(px(12.)).child(self.agents_panel(cx)).into_any_element()],
            Section::Mcp => self.mcp_rows(cx),
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
                    .rounded(px(t::RADIUS_POPOVER))
                    .border_1()
                    .border_color(line(cx, t::LINE_CONTROL_STRONG))
                    .bg(crate::theme::glass_modal_bg(cx))
                    .shadow(crate::theme::modal_shadow(cx))
                    .child(
                        div()
                            .w(px(t::SETTINGS_NAV_W))
                            .flex_shrink_0()
                            .h_full()
                            .flex()
                            .flex_col()
                            .bg(crate::theme::glass_panel_bg(cx))
                            .border_r_1()
                            .border_color(line(cx, t::LINE_DEFAULT))
                            .child(
                                div()
                                    .h(px(t::SETTINGS_HEADER_H))
                                    .flex()
                                    .items_center()
                                    .gap(px(8.))
                                    .px(px(16.))
                                    .child(ui::cordata_logo(20., cx))
                                    .child(div().text_size(px(t::TEXT_TITLE)).font_weight(FontWeight::SEMIBOLD).text_color(c(tk.text_primary)).child("Settings")),
                            )
                            .child(
                                div()
                                    .mx(px(8.))
                                    .mb(px(8.))
                                    .h(px(28.))
                                    .flex()
                                    .items_center()
                                    .gap(px(6.))
                                    .px(px(8.))
                                    .rounded(px(t::RADIUS_CONTROL))
                                    .bg(c(tk.bg_input))
                                    .border_1()
                                    .border_color(line(cx, t::LINE_CONTROL))
                                    .text_size(px(t::TEXT_UI))
                                    .child(div().flex_1().min_w_0().child(Input::new(&self.settings_search).appearance(false).aria_label("Search settings"))),
                            )
                            .when(nav.is_empty(), |col| col.child(div().px(px(16.)).py(px(6.)).child(ui::meta("No settings match.", cx))))
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
                                        ui::icon_action("settings-close", gpui_kit_assets::IconName::X, cx)
                                            .aria_label("Close settings (esc)")
                                            .tooltip(ui::tip("Close settings  esc"))
                                            .on_click(cx.listener(|this, _, window, cx| this.close_overlay(Overlay::Settings, window, cx))),
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
        if self.settings_section == Section::Account {
            self.api_key_masked = crate::keychain::masked();
        }
        if self.settings_section == Section::Agents && self.agents.found.is_none() {
            self.load_agents(cx);
        }
        if self.settings_section == Section::Mcp {
            self.mcp = crate::mcp_catalog::snapshot(&super::cordata_home(), &self.cwd);
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
            Select::CodeFont => format!("{value} px"),
            Select::QuickAskKey => crate::platform::Combo::parse(value).map(|c| c.label).unwrap_or_else(|_| value.into()),
        }
    }

    fn pick(&mut self, which: Select, value: String, _window: &mut Window, cx: &mut Context<Self>) {
        match which {
            Select::OpenOnLaunch => {
                self.prefs.open_on_launch = value;
                self.save_prefs(cx);
            }
            Select::CodeFont => {
                self.prefs.code_font_size = value.parse().unwrap_or(13.0);
                self.save_prefs(cx);
            }
            Select::QuickAskKey => {
                let before = std::mem::replace(&mut self.prefs.quick_ask_key, value);
                if self.apply_hotkey() {
                    self.save_prefs(cx);
                } else {
                    let taken = self.quick_ask_combo().label;
                    self.prefs.quick_ask_key = before;
                    self.settings_view.set_notice(format!("{taken} is taken by another app; kept {}.", self.quick_ask_combo().label), true);
                }
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
        let mode = self.segmented("default-workspace-mode", &[("canvas", "Canvas"), ("transcript", "Transcript"), ("sheet", "Sheet")], &self.prefs.default_mode.clone(), |this, v, cx| {
            this.prefs.default_mode = v.into();
            this.save_prefs(cx);
            this.desk_mode = match v {
                "canvas" => super::DeskMode::Rack,
                "sheet" => super::DeskMode::ChangeSheet,
                _ => super::DeskMode::Transcript,
            };
            cx.notify();
        }, cx);
        let restore = self.toggle("restore-threads", self.prefs.restore_threads, "Reopen threads on launch", |this, on, cx| {
            this.prefs.restore_threads = on;
            this.save_prefs(cx);
            if on {
                this.persist_threads(cx);
            } else {
                this.threads.clear();
            }
        }, cx);
        let send = self.segmented("send-with", &[("enter", "↵"), ("cmd-enter", "⌘↵")], &self.prefs.send_with.clone(), |this, v, cx| {
            this.prefs.send_with = v.into();
            this.save_prefs(cx);
            this.apply_send_with(cx);
        }, cx);
        let notify = self.toggle("notify-approval", self.prefs.notify_approval, "Notify when approval is needed", |this, on, cx| {
            this.prefs.notify_approval = on;
            this.save_prefs(cx);
        }, cx);
        let notify_done = self.toggle("notify-done", self.prefs.notify_done, "Notify when a thread finishes in the background", |this, on, cx| {
            this.prefs.notify_done = on;
            this.save_prefs(cx);
        }, cx);
        let menu_bar = self.toggle("menu-bar", self.prefs.menu_bar, "Show in the menu bar", |this, on, cx| {
            this.prefs.menu_bar = on;
            this.save_prefs(cx);
        }, cx);
        let sound = self.toggle("sound-done", self.prefs.sound_on_completion, "Sounds", |this, on, cx| {
            this.prefs.sound_on_completion = on;
            this.save_prefs(cx);
        }, cx);
        let hotkey = self.toggle("quick-ask", self.prefs.quick_ask_hotkey, "Quick ask hotkey", |this, on, cx| {
            this.prefs.quick_ask_hotkey = on;
            this.save_prefs(cx);
            if !this.apply_hotkey() {
                this.settings_view.set_notice(format!("{} is taken by another app.", this.quick_ask_combo().label), true);
            }
        }, cx);
        let meter = self.toggle("show-meter", self.prefs.show_meter, "Turn meter", |this, on, cx| {
            this.prefs.show_meter = on;
            this.save_prefs(cx);
        }, cx);
        let context = self.toggle("show-context", self.prefs.show_context, "Context gauge", |this, on, cx| {
            this.prefs.show_context = on;
            this.save_prefs(cx);
        }, cx);
        let combo = self.quick_ask_combo();
        let mut keys: Vec<(String, String)> = crate::platform::COMBO_PRESETS.iter().filter_map(|p| crate::platform::Combo::parse(p).ok().map(|c| ((*p).to_owned(), c.label))).collect();
        if !crate::platform::COMBO_PRESETS.contains(&self.prefs.quick_ask_key.as_str()) {
            keys.insert(0, (self.prefs.quick_ask_key.clone(), combo.label.clone()));
        }
        let key = self.select(Select::QuickAskKey, &self.prefs.quick_ask_key.clone(), keys, cx);
        let quick_ask = div().flex().items_center().gap(px(8.)).child(key).child(hotkey).into_any_element();
        vec![
            setting_row("Open on launch", "What the window shows when Cordata starts.", launch, cx),
            setting_row("Default workspace", "Which environment Cordata opens into (Canvas, Transcript, or Sheet).", mode, cx),
            setting_row("Reopen threads on launch", "Put back the threads that were open, with the one you were in. Off: every launch starts one new thread and your conversations stay under Earlier. Only one window keeps the list; a second copy of the app starts clean.", restore, cx),
            setting_row("Quick ask", "From any app: bring Cordata forward with the composer ready. Any other combination can go in prefs.toml as quick_ask_key (e.g. \"cmd-alt-j\").", quick_ask, cx),
            setting_row("Send message with", "The other key adds a new line.", send, cx),
            setting_row("Show in the menu bar", "Cordata stays up when you close the window (⌘Q quits). The mark shows when something needs you, and a command that is safe to answer can be answered from there without opening the window.", menu_bar, cx),
            sub_heading("notifications", cx),
            setting_row("Notify when approval is needed", "A system notification when a command waits for you and the window is in the back.", notify, cx),
            setting_row("Notify when a thread finishes", "A system notification when a thread finishes while the window is in the back; it stays marked Done until you look (⌘J goes there).", notify_done, cx),
            setting_row("Sounds", "Two soft cues, only for what you cannot see: a rising one when something needs you, a falling one when a thread finishes elsewhere.", sound, cx),
            sub_heading("instruments", cx),
            setting_row("Turn meter", "Under the composer: time to the first word, the whole turn, output speed. Click it for the last turns, medians, tokens and cache.", meter, cx),
            setting_row("Context gauge", "How full the model's context window is; it turns ochre past 80%.", context, cx),
        ]
    }

    fn appearance_rows(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let theme = self.segmented("theme", &[("system", "system"), ("light", "paper"), ("2049", "ink"), ("graphite", "graphite"), ("dim", "dim")], &self.prefs.theme.clone(), |this, v, cx| {
            this.prefs.theme = v.into();
            this.save_prefs(cx);
            crate::theme::set_theme(v, cx);
        }, cx);
        let density = self.segmented("density", &[("compact", "compact"), ("comfortable", "comfortable")], &self.prefs.density.clone(), |this, v, cx| {
            this.prefs.density = v.into();
            this.save_prefs(cx);
        }, cx);
        let size = format!("{:.0}", self.prefs.code_font_size);
        let font = self.select(Select::CodeFont, &size, ["11", "12", "13", "14", "15", "16"].iter().map(|v| ((*v).to_owned(), format!("{v} px"))).collect(), cx);
        vec![
            setting_row("Theme", "system follows macOS Light/Dark live. Paper is the light theme; ink, graphite and dim are dark.", theme, cx),
            setting_row("Density", "Spacing in lists and the transcript.", density, cx),
            setting_row("Code font size", "The editor and code blocks.", font, cx),
        ]
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
            setting_row_owned("API key".into(), format!("Stored in the login Keychain only ({}). Read at the next request.{}", crate::keychain::SERVICE, self.api_key_masked.as_ref().map(|m| format!(" Current: {m}")).unwrap_or_default()), false, key, cx),
            setting_row("Usage this session", "Monthly usage is not tracked by the harness; these are this conversation's tokens.", mono_value(&format!("{in_t} in · {out_t} out"), cx), cx),
            setting_row("Sign out", "Removes the key from the Keychain.", ui::button("sign-out", "Sign out", Kind::Danger, cx).on_click(cx.listener(|this, _, window, cx| this.ask_confirm(Confirm::SignOut, window, cx))).into_any_element(), cx),
        ]
    }

    pub(super) fn mcp_rows(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let tk = tk(cx);
        let snap = &self.mcp;
        let mut rows = Vec::new();

        rows.push(sub_heading("servers a new chat starts", cx));
        if snap.servers.is_empty() {
            rows.push(setting_row("None", "Add one below, or list servers in ~/.cordata/mcp.json.", status_control(Dot::Muted, "none", cx), cx));
        }
        for server in &snap.servers {
            let desc = format!("{} · {} {}", server.source.label(), server.command, server.args.join(" "));
            rows.push(setting_row_owned(server.name.clone(), desc, false, status_control(Dot::Ok, "starts with each chat", cx), cx));
        }
        if !snap.held.is_empty() {
            rows.push(sub_heading("waiting for you", cx));
        }
        for (ix, held) in snap.held.iter().enumerate() {
            let approve_this = held.clone();
            let button = ui::button(("mcp-approve", ix), if held.changed { "Approve change" } else { "Approve" }, Kind::Tint, cx)
                .on_click(cx.listener(move |this, _, _, cx| {
                    match crate::mcp_catalog::approve(&cordata_view::settings::Store::discover(&this.cwd), &approve_this) {
                        Ok(()) => this.settings_view.set_notice(format!("Approved {}: it starts with the next chat", approve_this.name), false),
                        Err(e) => this.settings_view.set_notice(format!("Could not approve {}: {e}", approve_this.name), true),
                    }
                    this.mcp = crate::mcp_catalog::snapshot(&super::cordata_home(), &this.cwd);
                    cx.notify();
                }))
                .into_any_element();
            let what = if held.changed {
                format!("Changed since you approved it. This repository's .mcp.json now runs: {}", held.command_line())
            } else {
                format!("This repository's .mcp.json would run on your Mac: {}", held.command_line())
            };
            rows.push(setting_row_owned(held.name.clone(), what, false, button, cx));
        }

        rows.push(sub_heading("marketplace & catalog", cx));
        rows.push(div().text_size(px(t::TEXT_SMALL)).text_color(c(tk.text_muted)).pb(px(8.)).child("Search and add verified MCP servers to ~/.cordata/mcp.json. Every call asks first.").into_any_element());

        let search_box = div()
            .h(px(28.))
            .flex()
            .items_center()
            .gap(px(6.))
            .px(px(8.))
            .rounded(px(t::RADIUS_CONTROL))
            .bg(c(tk.bg_input))
            .border_1()
            .border_color(line(cx, t::LINE_CONTROL))
            .text_size(px(t::TEXT_UI))
            .child(div().flex_1().min_w_0().child(Input::new(&self.mcp_search).appearance(false).aria_label("Search MCP catalog")));
        rows.push(setting_row("Search", "Filter by server name, command, or capability.", search_box.into_any_element(), cx));

        let category_pills: Vec<AnyElement> = crate::mcp_catalog::CATEGORIES
            .iter()
            .enumerate()
            .map(|(index, &cat)| {
                let selected = self.mcp_category == cat;
                ui::segment(("mcp-cat", index), cat, selected, cx)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.mcp_category = cat;
                        cx.notify();
                    }))
                    .into_any_element()
            })
            .collect();
        rows.push(setting_row("Category", "Filter servers by domain.", ui::segmented_row(cx).children(category_pills).into_any_element(), cx));

        let query = self.mcp_search.read(cx).value().to_string();
        let items = crate::mcp_catalog::search_marketplace(&query, Some(self.mcp_category));

        if items.is_empty() {
            rows.push(setting_row("No servers found", "No MCP servers matched your search query.", status_control(Dot::Muted, "none", cx), cx));
        }

        for (ix, item) in items.iter().enumerate() {
            let installed = snap.servers.iter().any(|s| s.name == item.id);
            let (item_id, item_cmd, item_args) = (item.id, item.command, item.default_args);
            let action: AnyElement = if installed {
                status_control(Dot::Ok, "added", cx)
            } else {
                ui::button(("mcp-install", ix), "Add", Kind::Tint, cx)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        match crate::mcp_catalog::install_server(&super::cordata_home(), item_id, item_cmd, item_args) {
                            Ok(()) => this.settings_view.set_notice(format!("Added {item_id}: it starts with the next chat"), false),
                            Err(e) => this.settings_view.set_notice(format!("Could not add {item_id}: {e}"), true),
                        }
                        this.mcp = crate::mcp_catalog::snapshot(&super::cordata_home(), &this.cwd);
                        cx.notify();
                    }))
                    .into_any_element()
            };
            let mut desc = format!("{} · {}", item.category, item.description);
            if !item.required_env.is_empty() {
                desc.push_str(&format!(" · Env: {}", item.required_env.join(", ")));
            }
            rows.push(setting_row_owned(item.name.into(), desc, false, action, cx));
        }

        let user_file = super::cordata_home().join("mcp.json");
        let reveal = ui::button("reveal-mcp-json", "Reveal mcp.json", Kind::Outline, cx).on_click(cx.listener(move |_, _, _, cx| cx.reveal_path(&user_file))).into_any_element();
        rows.push(sub_heading("configuration", cx));
        rows.push(setting_row("Your servers", "~/.cordata/mcp.json applies in every folder; a project's servers only as you approved them — a change asks again.", reveal, cx));
        rows
    }

    fn advanced_rows(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        vec![
            setting_row("Logs folder", "The harness's state, trajectories and logs (~/.cordata).", ui::button("open-logs", "Reveal in Finder", Kind::Outline, cx).on_click(cx.listener(|_, _, _, cx| cx.reveal_path(&super::cordata_home()))).into_any_element(), cx),
            setting_row("Reset all settings", "Restores this window's preferences (theme, layout, recents). Harness settings files are untouched.", ui::button("reset-prefs", "Reset…", Kind::Danger, cx).on_click(cx.listener(|this, _, window, cx| this.ask_confirm(Confirm::ResetPrefs, window, cx))).into_any_element(), cx),
        ]
    }

    // ------------------------------------------------------------------------------------------------ actions

    pub(super) fn save_api_key(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let key = self.api_key_input.read(cx).value().to_string();
        // The field is cleared whatever happens: the key must not sit in the window.
        self.api_key_input.update(cx, |s, cx| s.set_value("", window, cx));
        match crate::keychain::store(&key) {
            Ok(()) => {
                self.api_key_masked = crate::keychain::masked();
                self.settings_view.set_notice("Key saved to the Keychain. The next message uses it.".into(), false);
            }
            Err(error) => self.settings_view.set_notice(error, true),
        }
        cx.notify();
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
    div().pt(px(22.)).pb(px(4.)).child(ui::quiet_label(text, cx)).into_any_element()
}

fn status_control(kind: Dot, text: &str, cx: &mut Context<Workspace>) -> AnyElement {
    div().flex().items_center().gap(px(8.)).child(ui::dot(kind, cx)).child(div().whitespace_nowrap().font_family(mono(cx)).text_size(px(t::TEXT_SMALL)).text_color(c(tk(cx).text_secondary)).child(SharedString::from(text.to_owned()))).into_any_element()
}

fn mono_value(text: &str, cx: &mut Context<Workspace>) -> AnyElement {
    div().max_w(px(320.)).truncate().font_family(mono(cx)).text_size(px(t::TEXT_SMALL)).text_color(c(tk(cx).text_secondary)).child(SharedString::from(text.to_owned())).into_any_element()
}

