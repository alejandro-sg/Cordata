//! Settings → Customize (Copilot's customizations editor, Claude's skills and commands) and Settings → Automations
//! (scheduled prompts, run by the daemon).
//!
//! Customize: the instruction files the agent reads (open, create, or generate with /init), the prompt commands
//! (`.cordata/commands/*.md`, run as `/name`), the skills the harness has learned (retire one), and the registry
//! (`backend/registry.py`): MCP servers and skills installed with one click from a catalog, or from one token typed in
//! (`@scope/pkg`, `uvx:pkg`, `https://…`, a skill folder or git URL). Installs go through the backend, which writes the
//! user's own files, never trusts or pre-approves anything, and hot-registers into the running session.
//! Everything is a file the person owns or a backend method; nothing here runs code.

use std::path::{Path, PathBuf};

use cordata_theme::tokens as t;
use cordata_view::shown;
use gpui::prelude::FluentBuilder as _;
use gpui::{AnyElement, AppContext as _, Context, Entity, IntoElement, ParentElement, SharedString, StatefulInteractiveElement as _, Styled, Window, div, px};
use gpui_component::input::{Input, InputState, Textarea, TextareaState};
use serde_json::{Value, json};

use super::{Overlay, Workspace, tilde};
use crate::theme::{c, line, mono, tk};
use crate::ui::{self, Dot, Kind};

pub const SKILLS_TAG: &str = "gui-skills";
pub const SKILL_RETIRE_TAG: &str = "gui-skill-retire";
pub const MCP_CATALOG_TAG: &str = "gui-mcp-catalog";
pub const MCP_INSTALL_TAG: &str = "gui-mcp-install";
pub const SKILL_CATALOG_TAG: &str = "gui-skill-catalog";
pub const SKILL_INSTALL_TAG: &str = "gui-skill-install";
/// Every tag `on_customize_data` answers (the workspace routes these here).
pub const TAGS: [&str; 6] = [SKILLS_TAG, SKILL_RETIRE_TAG, MCP_CATALOG_TAG, MCP_INSTALL_TAG, SKILL_CATALOG_TAG, SKILL_INSTALL_TAG];

pub struct Customize {
    pub skills: Vec<Value>,
    pub command_name: Entity<InputState>,
    pub note: Option<(String, bool)>,
    // Automations
    pub automations: Vec<Value>,
    pub auto_name: Entity<InputState>,
    pub auto_prompt: Entity<TextareaState>,
    pub auto_time: Entity<InputState>,
    pub auto_kind: &'static str,
    pub auto_day: &'static str,
    pub auto_minutes: u32,
    pub auto_note: Option<(String, bool)>,
    // The registry
    pub mcp_catalog: Vec<Value>,
    pub mcp_installed: Vec<Value>,
    pub skill_catalog: Vec<Value>,
    pub skills_installed: Vec<Value>,
    pub mcp_quick: Entity<InputState>,
    pub skill_quick: Entity<InputState>,
    pub registry_note: Option<(String, bool)>,
}

impl Customize {
    pub fn new(window: &mut Window, cx: &mut Context<Workspace>) -> Self {
        Self {
            skills: Vec::new(),
            command_name: cx.new(|cx| InputState::new(window, cx).placeholder("review-security")),
            note: None,
            automations: Vec::new(),
            auto_name: cx.new(|cx| InputState::new(window, cx).placeholder("Morning summary")),
            auto_prompt: cx.new(|cx| TextareaState::new(window, cx).rows(3).placeholder("What it should do each time — e.g. summarise yesterday's commits and open issues")),
            auto_time: cx.new(|cx| InputState::new(window, cx).placeholder("09:00")),
            auto_kind: "daily",
            auto_day: "mon",
            auto_minutes: 60,
            auto_note: None,
            mcp_catalog: Vec::new(),
            mcp_installed: Vec::new(),
            skill_catalog: Vec::new(),
            skills_installed: Vec::new(),
            mcp_quick: cx.new(|cx| InputState::new(window, cx).placeholder("@scope/pkg · uvx:pkg · https://host/mcp")),
            skill_quick: cx.new(|cx| InputState::new(window, cx).placeholder("~/skills/my-skill · https://github.com/o/r.git#path")),
            registry_note: None,
        }
    }
}

/// The instruction files the agent reads, in the order the backend looks for them.
pub fn instruction_files(cwd: &Path) -> Vec<(&'static str, PathBuf)> {
    let home = super::cordata_home();
    vec![("yours, every project", home.join("CORDATA.md")), ("this project", cwd.join("CORDATA.md")), ("this project (shared)", cwd.join("AGENTS.md")), ("this project (Claude)", cwd.join("CLAUDE.md"))]
}

/// A prompt command's file name: letters, digits, `-` and `_`, at most 40.
pub fn valid_command(name: &str) -> bool {
    !name.is_empty() && name.len() <= 40 && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') && !name.starts_with('-')
}

const COMMAND_TEMPLATE: &str = "---\ndescription: What this command does\nargument-hint: [what to focus on]\n---\nDescribe the task for the agent here. $ARGUMENTS is replaced by what follows the command.\n";
const INSTRUCTIONS_TEMPLATE: &str = "# Instructions for Cordata\n\n- How to build and test this project:\n- Conventions to follow:\n- Things never to do:\n";

impl Workspace {
    pub(super) fn load_skills(&mut self, cx: &mut Context<Self>) {
        let session = self.session.read(cx);
        session.request("_cordata/skills/list", json!({}), SKILLS_TAG);
        session.request("_cordata/plugins/catalog", json!({}), MCP_CATALOG_TAG);
        session.request("_cordata/skills/catalog", json!({}), SKILL_CATALOG_TAG);
    }

    fn install_mcp(&mut self, spec: String, cx: &mut Context<Self>) {
        if spec.trim().is_empty() {
            return;
        }
        self.customize.registry_note = Some((format!("Adding {spec}…"), false));
        self.session.read(cx).request("_cordata/plugins/install", json!({ "spec": spec }), MCP_INSTALL_TAG);
        cx.notify();
    }

    fn install_skill(&mut self, source: String, cx: &mut Context<Self>) {
        if source.trim().is_empty() {
            return;
        }
        self.customize.registry_note = Some((format!("Installing {source}…"), false));
        self.session.read(cx).request("_cordata/skills/install", json!({ "source": source }), SKILL_INSTALL_TAG);
        cx.notify();
    }

    pub(super) fn on_customize_data(&mut self, tag: &str, value: &Value, cx: &mut Context<Self>) {
        match tag {
            SKILLS_TAG => self.customize.skills = value.get("skills").and_then(Value::as_array).cloned().unwrap_or_default(),
            SKILL_RETIRE_TAG => {
                let ok = value.get("ok").and_then(Value::as_bool).unwrap_or(true);
                self.customize.note = Some(if ok { ("Retired.".into(), false) } else { (value.get("reason").and_then(Value::as_str).unwrap_or("not retired").to_owned(), true) });
                self.load_skills(cx);
            }
            MCP_CATALOG_TAG => {
                self.customize.mcp_catalog = value.get("catalog").and_then(Value::as_array).cloned().unwrap_or_default();
                self.customize.mcp_installed = value.get("installed").and_then(Value::as_array).cloned().unwrap_or_default();
            }
            SKILL_CATALOG_TAG => {
                self.customize.skill_catalog = value.get("catalog").and_then(Value::as_array).cloned().unwrap_or_default();
                self.customize.skills_installed = value.get("installed").and_then(Value::as_array).cloned().unwrap_or_default();
            }
            MCP_INSTALL_TAG | SKILL_INSTALL_TAG => {
                let ok = value.get("ok").and_then(Value::as_bool).unwrap_or(false);
                let name = value.get("name").and_then(Value::as_str).unwrap_or("it");
                let warnings: Vec<&str> = value.get("warnings").and_then(Value::as_array).map(|w| w.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
                let note = if ok {
                    let live = if tag == MCP_INSTALL_TAG {
                        match value.get("status").and_then(|s| s.get("status")).and_then(Value::as_str) {
                            Some("running") => " — running in this session now".to_owned(),
                            Some(other) => format!(" — {other}"),
                            None => String::new(),
                        }
                    } else {
                        " — available to the agent now".to_owned()
                    };
                    (format!("Added {name}{live}.{}", if warnings.is_empty() { String::new() } else { format!(" {}", warnings.join(" ")) }), false)
                } else {
                    (value.get("reason").and_then(Value::as_str).unwrap_or("not added").to_owned(), true)
                };
                self.customize.registry_note = Some(note);
                if tag == MCP_INSTALL_TAG {
                    self.session.read(cx).request("_cordata/plugins/list", json!({}), super::PLUGINS_TAG);
                }
                self.load_skills(cx);
            }
            _ => {}
        }
        cx.notify();
    }

    /// Open an instruction file: in the editor when it is in the project, with the system's editor when it is yours.
    fn open_instructions(&mut self, path: PathBuf, create: bool, window: &mut Window, cx: &mut Context<Self>) {
        if create && !path.exists() {
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            if let Err(error) = std::fs::write(&path, INSTRUCTIONS_TEMPLATE) {
                self.customize.note = Some((format!("Could not create {}: {error}", tilde(&path)), true));
                cx.notify();
                return;
            }
        }
        match path.strip_prefix(&self.cwd) {
            Ok(rel) => {
                let rel = rel.display().to_string();
                self.close_overlay(Overlay::Settings, window, cx);
                self.set_panel(true, cx);
                self.open_file(&rel, None, window, cx);
            }
            Err(_) => cx.open_with_system(&path),
        }
    }

    fn new_command(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.customize.command_name.read(cx).value().trim().to_owned();
        if !valid_command(&name) {
            self.customize.note = Some(("A command name is letters, digits, - and _ (it becomes /name).".into(), true));
            cx.notify();
            return;
        }
        let rel = format!(".cordata/commands/{name}.md");
        let path = self.cwd.join(&rel);
        if path.exists() {
            self.customize.note = Some((format!("/{name} exists — opening it."), false));
        } else {
            let _ = std::fs::create_dir_all(path.parent().unwrap_or(&self.cwd));
            if let Err(error) = std::fs::write(&path, COMMAND_TEMPLATE) {
                self.customize.note = Some((format!("Could not create {rel}: {error}"), true));
                cx.notify();
                return;
            }
        }
        self.customize.command_name.update(cx, |s, cx| s.set_value("", window, cx));
        self.close_overlay(Overlay::Settings, window, cx);
        self.set_panel(true, cx);
        self.open_file(&rel, None, window, cx);
    }

    pub(super) fn customize_rows(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let tk = tk(cx);
        let mut rows: Vec<AnyElement> = Vec::new();
        if let Some((note, error)) = self.customize.note.clone() {
            rows.push(div().pt(px(12.)).text_size(px(t::TEXT_SMALL)).text_color(c(if error { tk.danger } else { tk.teal })).child(SharedString::from(note)).into_any_element());
        }
        rows.push(sub(cx, "instructions"));
        for (index, (label, path)) in instruction_files(&self.cwd).into_iter().enumerate() {
            let exists = path.exists();
            let open_path = path.clone();
            let control = div()
                .flex()
                .gap(px(6.))
                .child(ui::button(("instr-open", index), if exists { "Open" } else { "Create" }, if exists { Kind::Outline } else { Kind::Ghost }, cx).h(px(26.)).on_click(cx.listener(move |this, _, window, cx| this.open_instructions(open_path.clone(), true, window, cx))))
                .into_any_element();
            rows.push(row(SharedString::from(tilde(&path)), SharedString::from(format!("{label}{}", if exists { "" } else { " — not created" })), control, exists, cx));
        }
        rows.push(
            div()
                .pt(px(8.))
                .child(ui::button("instr-init", "Generate project instructions (/init)", Kind::Ghost, cx).h(px(26.)).on_click(cx.listener(|this, _, window, cx| {
                    this.close_overlay(Overlay::Settings, window, cx);
                    this.session.update(cx, |s, cx| s.submit("/init", cx));
                })))
                .into_any_element(),
        );
        rows.push(sub(cx, "prompt commands"));
        let custom = self.session.read(cx).view.custom.clone();
        if custom.is_empty() {
            rows.push(div().py(px(10.)).text_size(px(t::TEXT_SMALL)).text_color(c(tk.text_muted)).child("None yet. A command is a markdown prompt you run as /name.").into_any_element());
        }
        for (index, cmd) in custom.iter().enumerate().take(40) {
            let name = cmd.name.clone();
            let control = ui::button(("cmd-run", index), "Run", Kind::Ghost, cx)
                .h(px(24.))
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.close_overlay(Overlay::Settings, window, cx);
                    let text = format!("/{name} ");
                    this.composer.update(cx, |s, cx| {
                        s.set_value(text, window, cx);
                        s.focus(window, cx);
                    });
                }))
                .into_any_element();
            rows.push(row(SharedString::from(format!("/{}", cmd.name)), SharedString::from(format!("{} · {}", shown(&cmd.description), cmd.scope)), control, true, cx));
        }
        rows.push(
            div()
                .flex()
                .items_center()
                .gap(px(6.))
                .pt(px(8.))
                .child(div().font_family(mono(cx)).text_size(px(t::TEXT_SMALL)).text_color(c(tk.text_muted)).child("/"))
                .child(field(&self.customize.command_name, 180., cx))
                .child(ui::button("cmd-new", "New command", Kind::Outline, cx).h(px(26.)).on_click(cx.listener(|this, _, window, cx| this.new_command(window, cx))))
                .into_any_element(),
        );
        rows.push(sub(cx, "learned skills"));
        if self.customize.skills.is_empty() {
            rows.push(div().py(px(10.)).text_size(px(t::TEXT_SMALL)).text_color(c(tk.text_muted)).child("Nothing learned yet. Lessons are drafted after costly turns and promoted only when they beat the baseline.").into_any_element());
        }
        for (index, skill) in self.customize.skills.iter().enumerate() {
            let name = skill.get("name").and_then(Value::as_str).unwrap_or_default().to_owned();
            let status = skill.get("status").and_then(Value::as_str).unwrap_or_default().to_owned();
            let description = skill.get("description").and_then(Value::as_str).unwrap_or_default();
            let target = name.clone();
            let control = if status == "retired" {
                ui::meta("retired", cx).into_any_element()
            } else {
                ui::button(("skill-retire", index), "Retire", Kind::Ghost, cx)
                    .h(px(24.))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.session.read(cx).request("_cordata/skills/retire", json!({ "name": target }), SKILL_RETIRE_TAG);
                    }))
                    .into_any_element()
            };
            rows.push(row(SharedString::from(name), SharedString::from(format!("{status} · {}", shown(description))), control, status == "live", cx));
        }
        rows.extend(self.registry_rows(cx));
        rows.push(sub(cx, "agents & hooks"));
        rows.push(div().py(px(10.)).text_size(px(t::TEXT_SMALL)).text_color(c(tk.text_muted)).child("Lifecycle hooks are the hooks.preToolUse / hooks.postToolUse / hooks.stop settings (Advanced → Harness; trust-gated). The specialist agents are set in code.").into_any_element());
        rows
    }

    // ------------------------------------------------------------------------------------------------ automations

    pub(super) fn load_automations(&mut self, cx: &mut Context<Self>) {
        let acp_core::BackendTransport::Hub { daemon, .. } = &self.connection.transport else {
            self.customize.auto_note = Some(("Automations run in the daemon: start Cordata with the hub (the default).".into(), true));
            return;
        };
        let daemon = daemon.clone();
        let job = cx.global::<crate::session::Runtime>().0.spawn(async move { acp_core::hub::automations(&daemon).await });
        cx.spawn(async move |this, cx| {
            let result = job.await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(Ok(rows)) => this.customize.automations = rows,
                    Ok(Err(error)) => this.customize.auto_note = Some((error.to_string(), true)),
                    Err(_) => {}
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn automation_call(&mut self, call: AutomationCall, cx: &mut Context<Self>) {
        let acp_core::BackendTransport::Hub { daemon, .. } = &self.connection.transport else { return };
        let daemon = daemon.clone();
        let job = cx.global::<crate::session::Runtime>().0.spawn(async move {
            match call {
                AutomationCall::Save(body) => acp_core::hub::save_automation(&daemon, &body).await.map(|_| "Saved.".to_owned()),
                AutomationCall::Delete(id) => acp_core::hub::delete_automation(&daemon, &id).await.map(|_| "Deleted.".to_owned()),
                AutomationCall::Run(id) => acp_core::hub::run_automation(&daemon, &id).await.map(|_| "Started — it appears as a ⏱ conversation.".to_owned()),
            }
        });
        cx.spawn(async move |this, cx| {
            let result = job.await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(Ok(note)) => this.customize.auto_note = Some((note, false)),
                    Ok(Err(error)) => this.customize.auto_note = Some((error.to_string(), true)),
                    Err(_) => {}
                }
                this.load_automations(cx);
                this.refresh_listing(cx);
            });
        })
        .detach();
    }

    fn save_new_automation(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let c = &self.customize;
        let at = c.auto_time.read(cx).value().trim().to_owned();
        let at = if at.is_empty() { "09:00".to_owned() } else { at };
        let body = json!({
            "name": c.auto_name.read(cx).value().trim(),
            "prompt": c.auto_prompt.read(cx).value().trim(),
            "cwd": self.cwd.display().to_string(),
            "kind": c.auto_kind,
            "time": at,
            "day": c.auto_day,
            "minutes": c.auto_minutes,
        });
        self.customize.auto_name.update(cx, |s, cx| s.set_value("", window, cx));
        self.customize.auto_prompt.update(cx, |s, cx| s.set_value("", window, cx));
        self.automation_call(AutomationCall::Save(body), cx);
    }

    pub(super) fn automation_rows(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let tk = tk(cx);
        let mut rows: Vec<AnyElement> = Vec::new();
        if let Some((note, error)) = self.customize.auto_note.clone() {
            rows.push(div().pt(px(12.)).text_size(px(t::TEXT_SMALL)).text_color(c(if error { tk.danger } else { tk.teal })).child(SharedString::from(shown(&note))).into_any_element());
        }
        rows.push(div().pt(px(12.)).text_size(px(t::TEXT_SMALL)).text_color(c(tk.text_muted)).child("Each run is a new conversation in its project. Steps that need approval wait for you — nothing is approved on your behalf.").into_any_element());
        rows.push(sub(cx, "scheduled"));
        if self.customize.automations.is_empty() {
            rows.push(div().py(px(10.)).text_size(px(t::TEXT_SMALL)).text_color(c(tk.text_muted)).child("None yet.").into_any_element());
        }
        for (index, a) in self.customize.automations.iter().enumerate() {
            let id = a.get("id").and_then(Value::as_str).unwrap_or_default().to_owned();
            let enabled = a.get("enabled").and_then(Value::as_bool).unwrap_or(true);
            let status = a.get("last_status").and_then(Value::as_str).unwrap_or_default();
            let next = a.get("next").and_then(Value::as_f64);
            let when = schedule_text(a);
            let toggled = {
                let mut body = a.clone();
                if let Some(obj) = body.as_object_mut() {
                    obj.insert("enabled".into(), json!(!enabled));
                }
                body
            };
            let (run_id, del_id) = (id.clone(), id.clone());
            let control = div()
                .flex()
                .items_center()
                .gap(px(6.))
                .child(ui::switch(("auto-on", index), enabled, cx).aria_label("Enabled").on_click(cx.listener(move |this, _, _, cx| this.automation_call(AutomationCall::Save(toggled.clone()), cx))))
                .child(ui::button(("auto-run", index), "Run now", Kind::Ghost, cx).h(px(24.)).on_click(cx.listener(move |this, _, _, cx| this.automation_call(AutomationCall::Run(run_id.clone()), cx))))
                .child(ui::button(("auto-del", index), "Delete", Kind::Danger, cx).h(px(24.)).on_click(cx.listener(move |this, _, _, cx| this.automation_call(AutomationCall::Delete(del_id.clone()), cx))))
                .into_any_element();
            let next_text = next.map(|n| format!(" · next in {}", super::age(n - super::now_secs()))).unwrap_or_default();
            let project = a.get("cwd").and_then(Value::as_str).map(|p| tilde(Path::new(p))).unwrap_or_default();
            let description = format!("{when} · {project}{next_text}{}", if status.is_empty() { String::new() } else { format!(" · last: {status}") });
            rows.push(row(SharedString::from(a.get("name").and_then(Value::as_str).unwrap_or_default().to_owned()), SharedString::from(description), control, enabled, cx));
        }
        rows.push(sub(cx, "new automation (in this project)"));
        let kind = self.customize.auto_kind;
        let kinds: Vec<AnyElement> = [("daily", "daily"), ("weekly", "weekly"), ("interval", "every…")]
            .iter()
            .enumerate()
            .map(|(i, &(value, label))| ui::segment(("auto-kind", i), label, kind == value, cx).on_click(cx.listener(move |this, _, _, cx| {
                this.customize.auto_kind = value;
                cx.notify();
            })).into_any_element())
            .collect();
        let days: Vec<AnyElement> = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"]
            .iter()
            .enumerate()
            .map(|(i, &d)| ui::segment(("auto-day", i), d, self.customize.auto_day == d, cx).on_click(cx.listener(move |this, _, _, cx| {
                this.customize.auto_day = d;
                cx.notify();
            })).into_any_element())
            .collect();
        let minutes: Vec<AnyElement> = [15u32, 30, 60, 180, 360]
            .iter()
            .enumerate()
            .map(|(i, &m)| ui::segment(("auto-min", i), &if m < 60 { format!("{m}m") } else { format!("{}h", m / 60) }, self.customize.auto_minutes == m, cx).on_click(cx.listener(move |this, _, _, cx| {
                this.customize.auto_minutes = m;
                cx.notify();
            })).into_any_element())
            .collect();
        rows.push(
            div()
                .flex()
                .flex_col()
                .gap(px(8.))
                .pt(px(8.))
                .child(field(&self.customize.auto_name, 280., cx))
                .child(div().px(px(10.)).py(px(8.)).rounded(px(t::RADIUS_CONTROL)).bg(c(tk.bg_input)).border_1().border_color(line(cx, t::LINE_CONTROL_STRONG)).text_size(px(t::TEXT_UI)).child(Textarea::new(&self.customize.auto_prompt).appearance(false)))
                .child(div().flex().flex_wrap().items_center().gap(px(8.)).child(ui::segmented_row(cx).children(kinds)).when(kind == "weekly", |r| r.child(ui::segmented_row(cx).children(days))).when(kind == "interval", |r| r.child(ui::segmented_row(cx).children(minutes))).when(kind != "interval", |r| r.child(field(&self.customize.auto_time, 80., cx))))
                .child(div().child(ui::button("auto-save", "Add automation", Kind::Primary, cx).on_click(cx.listener(|this, _, window, cx| this.save_new_automation(window, cx)))))
                .into_any_element(),
        );
        rows
    }
}

pub enum AutomationCall {
    Save(Value),
    Delete(String),
    Run(String),
}

/// `daily at 09:00`, `Fridays at 17:30`, `every 30 minutes`.
pub fn schedule_text(a: &Value) -> String {
    let s = |k: &str| a.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
    match s("kind").as_str() {
        "interval" => {
            let m = a.get("minutes").and_then(Value::as_u64).unwrap_or(60);
            if m.is_multiple_of(60) { format!("every {} hour{}", m / 60, if m == 60 { "" } else { "s" }) } else { format!("every {m} minutes") }
        }
        "weekly" => format!("{}s at {}", day_name(&s("day")), s("time")),
        _ => format!("daily at {}", s("time")),
    }
}

fn day_name(day: &str) -> &'static str {
    match day {
        "tue" => "Tuesday",
        "wed" => "Wednesday",
        "thu" => "Thursday",
        "fri" => "Friday",
        "sat" => "Saturday",
        "sun" => "Sunday",
        _ => "Monday",
    }
}

impl Workspace {
    /// The registry: catalog cards with one-click install, and a quick-add field for anything else.
    fn registry_rows(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let tk = tk(cx);
        let mut rows: Vec<AnyElement> = Vec::new();
        rows.push(sub(cx, "mcp servers — one click"));
        if let Some((note, error)) = self.customize.registry_note.clone() {
            rows.push(div().py(px(6.)).text_size(px(t::TEXT_SMALL)).text_color(c(if error { tk.danger } else { tk.teal })).child(SharedString::from(note)).into_any_element());
        }
        let installed: Vec<&str> = self.customize.mcp_installed.iter().filter_map(|r| r.get("name").and_then(Value::as_str)).collect();
        for (index, entry) in self.customize.mcp_catalog.iter().enumerate() {
            let id = entry.get("id").and_then(Value::as_str).unwrap_or_default().to_owned();
            let needs: Vec<&str> = entry.get("needs").and_then(Value::as_array).map(|n| n.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
            let description = format!("{}{}", entry.get("description").and_then(Value::as_str).unwrap_or_default(), if needs.is_empty() { String::new() } else { format!(" · needs {}", needs.join(", ")) });
            let have = installed.contains(&id.as_str());
            let target = id.clone();
            let control = if have {
                ui::meta("installed", cx).into_any_element()
            } else {
                ui::button(("mcp-install", index), "Install", Kind::Outline, cx).h(px(24.)).on_click(cx.listener(move |this, _, _, cx| this.install_mcp(target.clone(), cx))).into_any_element()
            };
            rows.push(row(SharedString::from(id), SharedString::from(description), control, have, cx));
        }
        rows.push(
            div()
                .flex()
                .items_center()
                .gap(px(6.))
                .pt(px(8.))
                .child(field(&self.customize.mcp_quick, 300., cx))
                .child(ui::button("mcp-quick-add", "Add server", Kind::Outline, cx).h(px(26.)).on_click(cx.listener(|this, _, window, cx| {
                    let spec = this.customize.mcp_quick.read(cx).value().trim().to_owned();
                    this.customize.mcp_quick.update(cx, |s, cx| s.set_value("", window, cx));
                    this.install_mcp(spec, cx);
                })))
                .into_any_element(),
        );
        rows.push(sub(cx, "skills — one click"));
        let have_skills: Vec<&str> = self.customize.skills_installed.iter().filter_map(|r| r.get("name").and_then(Value::as_str)).collect();
        for (index, entry) in self.customize.skill_catalog.iter().enumerate() {
            let id = entry.get("id").and_then(Value::as_str).unwrap_or_default().to_owned();
            let description = entry.get("description").and_then(Value::as_str).unwrap_or_default().to_owned();
            let have = have_skills.iter().any(|name| id.ends_with(name));
            let target = id.clone();
            let control = if have {
                ui::meta("installed", cx).into_any_element()
            } else {
                ui::button(("skill-install", index), "Install", Kind::Outline, cx).h(px(24.)).on_click(cx.listener(move |this, _, _, cx| this.install_skill(target.clone(), cx))).into_any_element()
            };
            rows.push(row(SharedString::from(id), SharedString::from(description), control, have, cx));
        }
        for skill in &self.customize.skills_installed {
            let name = skill.get("name").and_then(Value::as_str).unwrap_or_default().to_owned();
            let scope = skill.get("scope").and_then(Value::as_str).unwrap_or("user");
            rows.push(row(SharedString::from(name), SharedString::from(format!("installed · {scope} · {}", shown(skill.get("description").and_then(Value::as_str).unwrap_or_default()))), ui::meta(scope, cx).into_any_element(), true, cx));
        }
        rows.push(
            div()
                .flex()
                .items_center()
                .gap(px(6.))
                .pt(px(8.))
                .child(field(&self.customize.skill_quick, 300., cx))
                .child(ui::button("skill-quick-add", "Install skill", Kind::Outline, cx).h(px(26.)).on_click(cx.listener(|this, _, window, cx| {
                    let source = this.customize.skill_quick.read(cx).value().trim().to_owned();
                    this.customize.skill_quick.update(cx, |s, cx| s.set_value("", window, cx));
                    this.install_skill(source, cx);
                })))
                .into_any_element(),
        );
        rows
    }
}

fn sub(cx: &mut Context<Workspace>, text: &str) -> AnyElement {
    div().pt(px(22.)).pb(px(4.)).child(ui::section_label(text, cx)).into_any_element()
}

fn field(input: &Entity<InputState>, width: f32, cx: &mut Context<Workspace>) -> AnyElement {
    div().w(px(width)).h(px(28.)).flex().items_center().px(px(8.)).rounded(px(t::RADIUS_CONTROL)).bg(c(tk(cx).bg_input)).border_1().border_color(line(cx, t::LINE_CONTROL_STRONG)).font_family(mono(cx)).text_size(px(t::TEXT_SMALL)).child(Input::new(input).appearance(false)).into_any_element()
}

fn row(label: SharedString, description: SharedString, control: AnyElement, active: bool, cx: &mut Context<Workspace>) -> AnyElement {
    let tk = tk(cx);
    div()
        .flex()
        .items_center()
        .gap(px(16.))
        .py(px(10.))
        .border_b_1()
        .border_color(line(cx, t::LINE_SUBTLE))
        .child(ui::dot(if active { Dot::Ok } else { Dot::Muted }, cx).size(px(t::DOT_SMALL)))
        .child(div().flex_1().min_w_0().flex().flex_col().gap(px(2.)).child(div().truncate().font_family(mono(cx)).text_size(px(t::TEXT_SMALL)).text_color(c(tk.text_primary)).child(label)).child(div().truncate().text_size(px(t::TEXT_SMALL)).text_color(c(tk.text_muted)).child(description)))
        .child(div().flex_shrink_0().child(control))
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_names_and_schedules_read_well() {
        assert!(valid_command("review-security"));
        assert!(!valid_command("../evil"));
        assert!(!valid_command("has space"));
        assert!(!valid_command("-x"));
        assert_eq!(schedule_text(&json!({"kind": "interval", "minutes": 30})), "every 30 minutes");
        assert_eq!(schedule_text(&json!({"kind": "interval", "minutes": 180})), "every 3 hours");
        assert_eq!(schedule_text(&json!({"kind": "weekly", "day": "fri", "time": "17:30"})), "Fridays at 17:30");
        assert_eq!(schedule_text(&json!({"kind": "daily", "time": "09:00"})), "daily at 09:00");
    }
}
