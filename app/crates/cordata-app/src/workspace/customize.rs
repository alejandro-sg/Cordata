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
use gpui::{AnyElement, AppContext as _, Context, Entity, IntoElement, ParentElement, SharedString, StatefulInteractiveElement as _, Styled, Window, div, px};
use gpui_component::input::{Input, InputState};
use serde_json::{Value, json};

use super::{Overlay, Workspace, tilde};
use crate::theme::{c, line, mono, tk};
use crate::ui::{self, Dot, Kind};

pub const SKILLS_TAG: &str = "gui-skills";
pub const SKILL_RETIRE_TAG: &str = "gui-skill-retire";
pub const MCP_CATALOG_TAG: &str = "gui-mcp-catalog";
pub const MCP_INSTALL_TAG: &str = "gui-mcp-install";
pub const MCP_REMOVE_TAG: &str = "gui-mcp-remove";
pub const SKILL_CATALOG_TAG: &str = "gui-skill-catalog";
pub const SKILL_INSTALL_TAG: &str = "gui-skill-install";
pub const SKILL_REMOVE_TAG: &str = "gui-skill-remove";
/// Every tag `on_customize_data` answers (the workspace routes these here).
pub const TAGS: [&str; 8] = [SKILLS_TAG, SKILL_RETIRE_TAG, MCP_CATALOG_TAG, MCP_INSTALL_TAG, MCP_REMOVE_TAG, SKILL_CATALOG_TAG, SKILL_INSTALL_TAG, SKILL_REMOVE_TAG];

pub struct Customize {
    pub skills: Vec<Value>,
    pub command_name: Entity<InputState>,
    pub note: Option<(String, bool)>,
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

    pub(super) fn install_mcp(&mut self, spec: String, cx: &mut Context<Self>) {
        if spec.trim().is_empty() {
            return;
        }
        self.customize.registry_note = Some((format!("Adding {spec}…"), false));
        self.session.read(cx).request("_cordata/plugins/install", json!({ "spec": spec }), MCP_INSTALL_TAG);
        cx.notify();
    }

    pub(super) fn remove_mcp(&mut self, name: String, project: bool, cx: &mut Context<Self>) {
        if name.trim().is_empty() {
            return;
        }
        self.customize.registry_note = Some((format!("Removing {name}…"), false));
        self.session.read(cx).request("_cordata/plugins/remove", json!({ "name": name, "project": project }), MCP_REMOVE_TAG);
        cx.notify();
    }

    pub(super) fn install_skill(&mut self, source: String, cx: &mut Context<Self>) {
        if source.trim().is_empty() {
            return;
        }
        self.customize.registry_note = Some((format!("Installing {source}…"), false));
        self.session.read(cx).request("_cordata/skills/install", json!({ "source": source }), SKILL_INSTALL_TAG);
        cx.notify();
    }

    pub(super) fn remove_skill(&mut self, name: String, project: bool, cx: &mut Context<Self>) {
        if name.trim().is_empty() {
            return;
        }
        self.customize.registry_note = Some((format!("Removing {name}…"), false));
        self.session.read(cx).request("_cordata/skills/remove", json!({ "name": name, "project": project }), SKILL_REMOVE_TAG);
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
            MCP_REMOVE_TAG | SKILL_REMOVE_TAG => {
                let ok = value.get("ok").and_then(Value::as_bool).unwrap_or(false);
                let name = value.get("name").and_then(Value::as_str).unwrap_or("entry");
                self.customize.registry_note = Some(if ok {
                    (format!("Removed {name}."), false)
                } else {
                    (value.get("reason").and_then(Value::as_str).unwrap_or("not removed").to_owned(), true)
                });
                if tag == MCP_REMOVE_TAG {
                    self.session.read(cx).request("_cordata/plugins/list", json!({}), super::PLUGINS_TAG);
                }
                self.load_skills(cx);
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
        let installed: Vec<(&str, &str)> = self
            .customize
            .mcp_installed
            .iter()
            .filter_map(|r| Some((r.get("name")?.as_str()?, r.get("scope").and_then(Value::as_str).unwrap_or("user"))))
            .collect();
        for (index, entry) in self.customize.mcp_catalog.iter().enumerate() {
            let id = entry.get("id").and_then(Value::as_str).unwrap_or_default().to_owned();
            let needs: Vec<&str> = entry.get("needs").and_then(Value::as_array).map(|n| n.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
            let description = format!("{}{}", entry.get("description").and_then(Value::as_str).unwrap_or_default(), if needs.is_empty() { String::new() } else { format!(" · needs {}", needs.join(", ")) });
            let found = installed.iter().find(|&&(n, _)| n == id.as_str()).copied();
            let have = found.is_some();
            let target = id.clone();
            let control = if let Some((_, scope)) = found {
                let is_proj = scope == "project";
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .child(ui::meta("installed", cx))
                    .child(ui::button(("mcp-remove", index), "Remove", Kind::Ghost, cx).h(px(24.)).on_click(cx.listener(move |this, _, _, cx| this.remove_mcp(target.clone(), is_proj, cx))))
                    .into_any_element()
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
        let have_skills: Vec<(&str, &str)> = self
            .customize
            .skills_installed
            .iter()
            .filter_map(|r| Some((r.get("name")?.as_str()?, r.get("scope").and_then(Value::as_str).unwrap_or("user"))))
            .collect();
        for (index, entry) in self.customize.skill_catalog.iter().enumerate() {
            let id = entry.get("id").and_then(Value::as_str).unwrap_or_default().to_owned();
            let skill_name = entry.get("name").and_then(Value::as_str).unwrap_or(id.as_str()).to_owned();
            let description = entry.get("description").and_then(Value::as_str).unwrap_or_default().to_owned();
            let found = have_skills.iter().find(|&&(n, _)| n == skill_name.as_str()).copied();
            let have = found.is_some();
            let target = id.clone();
            let rm_target = skill_name.clone();
            let control = if let Some((_, scope)) = found {
                let is_proj = scope == "project";
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .child(ui::meta("installed", cx))
                    .child(ui::button(("skill-cat-rm", index), "Remove", Kind::Ghost, cx).h(px(24.)).on_click(cx.listener(move |this, _, _, cx| this.remove_skill(rm_target.clone(), is_proj, cx))))
                    .into_any_element()
            } else {
                ui::button(("skill-install", index), "Install", Kind::Outline, cx).h(px(24.)).on_click(cx.listener(move |this, _, _, cx| this.install_skill(target.clone(), cx))).into_any_element()
            };
            rows.push(row(SharedString::from(id), SharedString::from(description), control, have, cx));
        }
        for (index, skill) in self.customize.skills_installed.iter().enumerate() {
            let name = skill.get("name").and_then(Value::as_str).unwrap_or_default().to_owned();
            let scope = skill.get("scope").and_then(Value::as_str).unwrap_or("user");
            let is_proj = scope == "project";
            let rm_name = name.clone();
            let control = div()
                .flex()
                .items_center()
                .gap(px(6.))
                .child(ui::meta(scope, cx))
                .child(ui::button(("skill-inst-rm", index), "Remove", Kind::Ghost, cx).h(px(24.)).on_click(cx.listener(move |this, _, _, cx| this.remove_skill(rm_name.clone(), is_proj, cx))))
                .into_any_element();
            rows.push(row(SharedString::from(name), SharedString::from(format!("installed · {scope} · {}", shown(skill.get("description").and_then(Value::as_str).unwrap_or_default()))), control, true, cx));
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
