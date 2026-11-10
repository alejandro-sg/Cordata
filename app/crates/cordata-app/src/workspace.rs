//! The window: the sidebar on the left (project, open conversations), the conversation in the centre (header, transcript, the approval card, the
//! composer), the side panel on the right (files, editor, review, terminal), the status bar
//! across the bottom, and the overlays above it all (the command palette, the project picker, settings).
//!
//! Several sessions can be open at once, one shown; each is its own engine. Keys: ⌘K palette, ⌘O project,
//! ⌘N new, ⌘1–9 switch, ⌘\ side panel, ⌃` terminal, ⌘, settings, esc deny / close, esc esc stop.
//! The rendering lives in the submodules; this file holds the state and the behaviour.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use acp_core::ApprovalDecision;
use cordata_theme::tokens as t;
use cordata_view::app::{ConnState, SwitchTarget};
use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, App, AppContext as _, ClipboardItem, Context, Entity, EntityId, Focusable as _, InteractiveElement as _, IntoElement, KeyDownEvent, ListAlignment, ListState, ParentElement, Render, SharedString,
    StatefulInteractiveElement as _, Styled, Subscription, Window, actions, div, list, px,
};
use gpui_component::input::{InputEvent, InputState, TextareaState};

use crate::prefs::{self, Prefs};
use crate::session::{Connection, Session, SessionEvent, WindowRequest};
use crate::threads;
use crate::theme::{c, ca, line, mono, tk};
use crate::transcript::{self, GroupEdge};
use cordata_view::fold;
use crate::ui::{self, Dot, Kind};

mod agents;
mod attachments;
mod conversation;
pub(crate) use conversation::prompt_mark;
mod desk;
mod field;
mod menubar;
mod rack;
mod sheet;
pub use sheet::DeskMode;
mod artifacts;
mod editor;
mod git_ops;
mod palette;
mod picker;
mod meter;
mod resize;
mod review;
mod settings_modal;
mod settings_view;
mod side_panel;
mod sidebar;
mod terminal;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DockTab {
    #[default]
    Files,
    Editor,
    Review,
    Artifacts,
    Terminal,
}

/// What floats over the window; the last one is on top and esc closes it first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Overlay {
    Palette,
    Picker,
    Settings,
    Clone,
    Confirm,
    Rename,
    About,
}

/// What the small naming dialog names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Naming {
    Worktree,
}

/// A destructive or trust-changing action waiting for a yes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Confirm {
    SignOut,
    ResetPrefs,
    /// A merged linked checkout, and its branch.
    RemoveWorktree { path: PathBuf, branch: Option<String> },
}

actions!(
    cordata,
    [
        StopTurn,
        FocusComposer,
        NewSession,
        CloseSession,
        NextSession,
        PrevSession,
        NextAttention,
        CommandMenu,
        OpenPalette,
        SwitchProject,
        ToggleSidebar,
        TogglePanel,
        ToggleTerminal,
        OpenSettings,
        ToggleThoughts,
        ToggleTools,
        CycleMode,
        Help,
        ShowFiles,
        ShowEditor,
        ShowReview,
        ShowArtifacts,
        ShowTerminal,
        ShowMcpHub,
        OpenFiles,
        SaveFile,
        SwitchTo1,
        SwitchTo2,
        SwitchTo3,
        SwitchTo4,
        SwitchTo5,
        SwitchTo6,
        SwitchTo7,
        SwitchTo8,
        SwitchTo9,
        ToggleDeck,
        ToggleRack,
        ForceAllowGate,
        About,
        HideApp,
        HideOthers,
        ShowAll,
        QuitApp,
        Minimize,
        Zoom,
        AttachFiles,
    ]
);

/// How many of the newest blocks are re-measured when anything changes (a streaming reply, a tool card updating).
const REMEASURE_TAIL: usize = 8;
/// Two presses of esc this close together stop the run.
const DOUBLE_ESC: Duration = Duration::from_millis(700);

pub struct Workspace {
    connection: Connection,
    pub(super) cwd: PathBuf,
    /// Every open session, and the one shown.
    sessions: Vec<Entity<Session>>,
    pub(super) session: Entity<Session>,
    subscriptions: Vec<(EntityId, Vec<Subscription>)>,
    composer: Entity<TextareaState>,
    composer_placeholder: &'static str,
    transcript: ListState,
    /// (session, messages, version) last laid out.
    seen: (EntityId, usize, u64),
    /// Tool rows opened to their output.
    expanded: HashSet<String>,
    /// The project's conversations (the hub's, or the saved stdio sessions).
    /// `w` on an approval card: the next Enter sends the composer's text as the answer (the reason, or one's own words).
    saying: bool,
    pub(super) dock_tab: DockTab,
    /// The left sidebar is shown (`⌘B`).
    pub(super) sidebar_open: bool,
    /// The side panel is shown (the person's choice, or the width's default).
    pub(super) panel_open: bool,
    pub(super) files: editor::Files,
    pub(super) file_filter: Entity<InputState>,
    pub(super) review: review::Review,
    pub(super) artifacts: artifacts::ArtifactsState,
    pub(super) terminal: terminal::Terminal,
    /// Start the dock shell when the Terminal tab is shown (`Launch::shell`).
    shell_on_show: bool,
    pub(super) settings_view: settings_view::SettingsView,
    /// The window's own preferences.
    pub(super) prefs: Prefs,
    pub(super) desk_mode: DeskMode,
    /// Selected agent slot in the Supervisory Control Room (`None` = overview).
    pub(super) rack_selected_slot: Option<usize>,
    /// Lanes (when) or ground (where) on the flight deck.
    pub(super) rack_view_mode: cordata_view::flight::RackViewMode,
    /// Where the field looks (pan and zoom); kept while the window is open.
    pub(super) field_cam: cordata_view::field::Camera,
    /// The field's box in the window as last drawn, so zoom can hold the point under the cursor.
    pub(super) field_bounds: std::rc::Rc<std::cell::Cell<gpui::Bounds<gpui::Pixels>>>,
    /// Last mouse position while the ground is being dragged.
    pub(super) field_drag: Option<gpui::Point<gpui::Pixels>>,
    /// This project's newest orchestrated runs, read from `cordata-run` while the field is open.
    pub(super) field_runs: Vec<cordata_view::runs::Run>,
    pub(super) field_runs_note: Option<String>,
    pub(super) field_runs_busy: bool,
    pub(super) field_polling: bool,
    /// Show the idle threads on the field too (they are only counted by default).
    pub(super) field_idle_open: bool,
    /// The menu bar item, while the pref is on (`menubar.rs`).
    pub(super) tray: Option<crate::tray::Native>,
    pub(super) tray_tick: u32,
    /// Selected tape stop for forensic inspection, turn-level forking, or post-turn rollback.
    pub(super) selected_turn: Option<usize>,
    /// Whether the Change Sheet's agent summary is expanded beyond its 5-line clamp.
    pub(super) sheet_expanded: bool,
    /// Per-file fold overrides in the Change Sheet (`⌘D`).
    pub(super) sheet_toggled_files: HashSet<String>,
    /// Expanded large hunks (`(raw_path, hunk_idx)`) in the Change Sheet (`⌘D`).
    pub(super) sheet_expanded_hunks: HashSet<(String, usize)>,
    /// Explicit `[expand all]` / `[collapse all]` override on the Change Sheet (`None` = smart default: first 2 + sensitive).
    pub(super) sheet_all_expanded: Option<bool>,
    /// Memoized desk & folded-row snapshot for the active session, avoiding per-frame message/diff scans at 120Hz.
    pub(super) desk_cache: DeskCache,
    /// Paces the header cord's pulses to the shown thread's events.
    pub(super) tugs: crate::motion::Tugs,
    /// Tool call ID of a high-risk gate whose challenge token was explicitly confirmed.
    pub(super) gate_unlocked: Option<String>,
    /// Inline feedback shown when a locked high-risk gate is triggered without token confirmation.
    pub(super) gate_hint: Option<String>,
    overlays: Vec<Overlay>,
    confirm: Option<Confirm>,
    palette_input: Entity<InputState>,
    palette_ix: usize,
    picker_input: Entity<InputState>,
    picker_ix: usize,
    picker_note: Option<(String, bool)>,
    clone_url: Entity<InputState>,
    clone_dest: Entity<InputState>,
    clone_note: Option<(String, bool)>,
    settings_section: settings_modal::Section,
    open_select: Option<settings_modal::Select>,
    api_key_input: Entity<InputState>,
    api_key_masked: Option<String>,
    /// The MCP servers a chat here starts, read when the MCP section opens (never per frame).
    mcp: crate::mcp_catalog::Snapshot,
    pub(super) mcp_search: Entity<InputState>,
    pub(super) mcp_category: &'static str,
    /// Settings' “Search settings…” box: narrows the section list.
    pub(super) settings_search: Entity<InputState>,
    /// Focus for the settings modal and the confirm dialog (their keys).
    settings_focus: gpui::FocusHandle,
    /// The first esc of a possible double press.
    esc_at: Option<Instant>,
    /// Whether each session was mid-turn when last seen (a finished turn plays the completion sound).
    turn_seen: HashMap<EntityId, bool>,
    /// Sessions that already saw their current pending approval (a held approval notifies once, not on every redraw).
    pending_seen: HashSet<EntityId>,
    /// A notification went out since the window was last in front: a burst of approvals while away is one banner, not one each.
    notified_away: bool,
    /// Cordata's two sounds (`sound.rs`), one at a time.
    pub(super) sounds: crate::sound::Gate,
    /// Images and files waiting to go with the next prompt.
    staged: Vec<attachments::Staged>,
    /// Why the last attachment or send was refused, shown above the composer.
    attach_note: Option<String>,
    /// What the naming dialog is for: naming a new worktree.
    renaming: Option<Naming>,
    /// Git and GitHub state for the Review tab and the header.
    git: git_ops::Git,
    /// This project's saved conversations (read off the UI thread, refreshed when a thread opens, closes or finishes).
    saved: Vec<acp_core::history::Summary>,
    /// What `threads.toml` holds now, and the (conversation, turns finished) each thread had when it was written: a
    /// thread is written again only when one of those moved, not on every streamed token.
    threads: threads::Store,
    thread_marks: HashMap<EntityId, (String, u64)>,
    /// An empty list is written only once the window has had something to list (or a thread was closed): a window
    /// that has just opened on a fresh folder must not erase what the last run left until it has its own.
    threads_armed: bool,
    /// Whether a conversation exists on disk (`acp_core::history::exists`); a test swaps it.
    session_exists: fn(&Path, &str) -> bool,
    /// The rail's "Earlier" list shows more than its first few.
    rail_more: bool,
    /// The rail's "Other checkouts" list is unfolded.
    rail_checkouts: bool,
    rename_input: Entity<InputState>,
    /// The sidebar row whose ⋯ menu is open (a hub handle).
    row_menu: Option<String>,
    /// Full-text matches for the palette's query (hub), and which query they answer.
    search_for: String,
    /// `--selftest`: answer every approval with Allow and quit with a summary once the first turn ends.
    selftest: Option<SelfTest>,
    pub(super) agents: agents::Agents,
    /// The project's check command (`cordata_view::desk::check_command`), and the folder it was read for.
    pub(super) check: (PathBuf, Option<String>),
    /// The column edge being dragged.
    resizing: Option<resize::Edge>,
    /// The meter's readout or the model switcher, when one is open.
    popup: Option<meter::Popup>,
    keys: meter::Keys,
    model_filter: Entity<InputState>,
    _subscriptions: Vec<Subscription>,
}

/// What the window was launched to do (command-line flags).
#[derive(Default)]
pub struct Launch {
    /// `--prompt`: sent once connected.
    pub prompt: Option<String>,
    /// `--selftest` with a prompt or a run: drive it to its end and quit with a summary.
    pub selftest: bool,
    /// Open the project picker first (Settings → Open on launch).
    pub picker: bool,
    /// `CORDATA_APP_SHOW`: open one surface at launch for layout checks — `terminal`, `about`, or `settings:<section>`.
    pub show: Option<String>,
    /// Serve `cordata-app status|ls|do` (`control.rs`). Off in tests, which must not take the real socket.
    pub control: bool,
    /// Start a real shell when the Terminal tab is first shown. Off in tests, which start one by clicking.
    pub shell: bool,
    /// The threads the last run left open (`threads.rs`), already checked against disk. Empty: start one fresh thread.
    pub restore: threads::Layout,
    /// Where the open-thread list is kept; in memory only unless given a folder.
    pub store: threads::Store,
    /// A folder was named at launch: with threads restored, it is shown too (its open thread, else a new one).
    pub open_folder: bool,
}

#[derive(Default, PartialEq, Eq)]
pub(super) struct DeskCacheKey {
    session_id: EntityId,
    version: u64,
    msg_len: usize,
    pending_id: Option<String>,
    check: Option<String>,
    peers: Vec<(usize, EntityId, u64, usize)>,
}

pub(super) struct DeskCache {
    key: Option<DeskCacheKey>,
    pub(super) snap: std::rc::Rc<cordata_view::desk::Snapshot>,
    pub(super) rows: std::rc::Rc<Vec<fold::Row>>,
    pub(super) fold_ctx: std::rc::Rc<fold::Context>,
}

impl Default for DeskCache {
    fn default() -> Self {
        Self {
            key: None,
            snap: std::rc::Rc::new(cordata_view::desk::Snapshot::default()),
            rows: std::rc::Rc::new(Vec::new()),
            fold_ctx: std::rc::Rc::new(fold::Context::here(PathBuf::new())),
        }
    }
}

#[derive(Default)]
struct SelfTest {
    approvals: usize,
    /// A turn has started (`turns_done` counts the real backend's `_cordata/turn` receipts, which a fake agent lacks).
    started: bool,
}

/// `~/x` for a path under the home folder.
pub fn tilde(path: &Path) -> String {
    let text = path.display().to_string();
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() && text.starts_with(&home) => format!("~{}", &text[home.len()..]),
        _ => text,
    }
}

/// The harness's state folder.
pub fn cordata_home() -> PathBuf {
    std::env::var_os("CORDATA_HOME").map(PathBuf::from).unwrap_or_else(|| std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default().join(".cordata"))
}

impl Workspace {
    /// A thread's event count (its session's `version`): what its mark beats to. 0 for a thread that is not open.
    pub(super) fn beat_of(&self, index: usize, cx: &App) -> u64 {
        self.sessions.get(index).map_or(0, |s| s.read(cx).version)
    }

    pub fn new(connection: Connection, cwd: PathBuf, launch: Launch, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let cwd = cwd.canonicalize().unwrap_or(cwd);
        let Launch { prompt: initial_prompt, selftest, picker, show, control, shell, restore, store, open_folder } = launch;
        let launch_cwd = cwd.clone();
        if control {
            Self::serve_control(window, cx);
        }
        let prefs = cx.try_global::<Prefs>().cloned().unwrap_or_default();
        let default_desk_mode = match prefs.default_mode.as_str() {
            "transcript" => DeskMode::Transcript,
            "sheet" => DeskMode::ChangeSheet,
            _ => DeskMode::Rack,
        };
        let composer = cx.new(|cx| TextareaState::new(window, cx).auto_grow(1, 10).submit_on_enter(prefs.send_with != "cmd-enter").placeholder("Ask anything, @mention files, or type / for commands…"));
        // Threads left open by the last run come back as they were: the shown one is built first (the window starts in
        // its folder), the rest are added in their old order below.
        let restoring = (!restore.is_empty()).then_some(&restore);
        let (cwd, first) = match restoring {
            Some(layout) => {
                let shown = &layout.open[layout.active_index()];
                (shown.cwd.canonicalize().unwrap_or_else(|_| shown.cwd.clone()), Self::resumed(&connection, shown, prefs.model.clone(), cx))
            }
            None => {
                let first = cx.new(|cx| Session::start(&connection, cwd.clone(), initial_prompt, prefs.model.clone(), cx));
                (cwd, first)
            }
        };
        let file_filter = cx.new(|cx| InputState::new(window, cx).placeholder("Filter files…"));
        let palette_input = cx.new(|cx| InputState::new(window, cx).placeholder("Type a command or search conversations…"));
        let picker_input = cx.new(|cx| InputState::new(window, cx).placeholder("Search projects or paste a path…"));
        let clone_url = cx.new(|cx| InputState::new(window, cx).placeholder("https://github.com/owner/repo.git"));
        let clone_dest = cx.new(|cx| InputState::new(window, cx).placeholder("~/Development"));
        let api_key_input = cx.new(|cx| InputState::new(window, cx).masked(true).placeholder("sk-ant-…"));
        let settings_search = cx.new(|cx| InputState::new(window, cx).placeholder("Search settings…"));
        let mcp_search = cx.new(|cx| InputState::new(window, cx).placeholder("Search MCP servers (e.g. postgres, sqlite, docker, git)…"));
        let rename_input = cx.new(|cx| InputState::new(window, cx).placeholder("Conversation title"));
        let model_filter = cx.new(|cx| InputState::new(window, cx).placeholder("Filter, or type any model id…"));
        let git = git_ops::Git::new(window, cx);
        let terminal = terminal::Terminal::new(window, cx);
        let settings_view = settings_view::SettingsView::default();
        let subscriptions = vec![
            cx.observe_window_activation(window, |this: &mut Self, window, cx| {
                if window.is_window_active() {
                    this.notified_away = false;
                    // Back in front: what the shown thread finished meanwhile is seen now.
                    this.session.update(cx, |s, _| s.view.mark_seen());
                    cx.notify();
                }
            }),
            cx.subscribe_in(&composer, window, Self::on_composer),
            cx.subscribe_in(&file_filter, window, Self::on_file_filter),
            cx.subscribe_in(&palette_input, window, |this: &mut Self, _, event: &InputEvent, _, cx| {
                if matches!(event, InputEvent::Change) {
                    this.palette_ix = 0;
                    this.search_conversations(cx);
                    cx.notify();
                }
            }),
            cx.subscribe_in(&model_filter, window, |this: &mut Self, _, event: &InputEvent, _, cx| this.on_model_filter(event, cx)),
            cx.subscribe_in(&rename_input, window, |this: &mut Self, _, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    this.finish_rename(window, cx);
                }
            }),
            cx.subscribe_in(&picker_input, window, |this: &mut Self, _, event: &InputEvent, _, cx| {
                if matches!(event, InputEvent::Change) {
                    this.picker_ix = 0;
                    cx.notify();
                }
            }),
            cx.subscribe_in(&settings_search, window, |this: &mut Self, input, event: &InputEvent, _, cx| {
                if matches!(event, InputEvent::Change | InputEvent::PressEnter { .. }) {
                    let query = input.read(cx).value().to_string();
                    this.on_settings_search(&query, cx);
                }
            }),
            cx.subscribe_in(&mcp_search, window, |_, _, event: &InputEvent, _, cx| {
                if matches!(event, InputEvent::Change) {
                    cx.notify();
                }
            }),
            cx.subscribe_in(&api_key_input, window, |this: &mut Self, _, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    this.save_api_key(window, cx);
                }
            }),
        ];
        let mut subscriptions = subscriptions;
        let this_handle = cx.entity().downgrade();
        subscriptions.push(cx.intercept_keystrokes(move |event, window, cx| {
            if let Some(this) = this_handle.upgrade() {
                this.update(cx, |this, cx| {
                    if this.should_intercept_keystroke(&event.keystroke, window, cx) {
                        cx.stop_propagation();
                    }
                });
            }
        }));
        composer.update(cx, |state, cx| state.focus(window, cx));
        let sidebar_open = prefs.sidebar_open.unwrap_or(true);
        // Closed until the person opens it (⌘\): the conversation is the window's default, not three columns.
        let panel_open = prefs.panel_open.unwrap_or(false);
        let mut this = Self {
            connection,
            cwd,
            sessions: Vec::new(),
            session: first.clone(),
            subscriptions: Vec::new(),
            composer,
            composer_placeholder: "Ask anything, @mention files, or type / for commands…",
            transcript: ListState::new(0, ListAlignment::Bottom, px(1200.)),
            seen: (first.entity_id(), 0, 0),
            expanded: HashSet::new(),
            saying: false,
            dock_tab: DockTab::default(),
            sidebar_open,
            panel_open,
            files: editor::Files::default(),
            file_filter,
            review: review::Review::default(),
            artifacts: artifacts::ArtifactsState::default(),
            terminal,
            shell_on_show: shell,
            settings_view,
            prefs,
            overlays: Vec::new(),
            confirm: None,
            palette_input,
            palette_ix: 0,
            picker_input,
            picker_ix: 0,
            picker_note: None,
            clone_url,
            clone_dest,
            clone_note: None,
            settings_section: settings_modal::Section::default(),
            open_select: None,
            api_key_input,
            api_key_masked: None,
            mcp: Default::default(),
            mcp_search,
            mcp_category: "All",
            settings_search,
            settings_focus: cx.focus_handle(),
            esc_at: None,
            turn_seen: HashMap::new(),
            pending_seen: HashSet::new(),
            notified_away: false,
            sounds: crate::sound::Gate::default(),
            staged: Vec::new(),
            attach_note: None,
            renaming: None,
            git,
            saved: Vec::new(),
            threads: store,
            thread_marks: HashMap::new(),
            threads_armed: false,
            session_exists: acp_core::history::exists,
            rail_more: false,
            rail_checkouts: false,
            rename_input,
            row_menu: None,
            search_for: String::new(),
            selftest: selftest.then(SelfTest::default),
            resizing: None,
            agents: agents::Agents::default(),
            check: (PathBuf::new(), None),
            popup: None,
            keys: meter::Keys::default(),
            model_filter,
            desk_mode: default_desk_mode,
            rack_selected_slot: None,
            rack_view_mode: cordata_view::flight::RackViewMode::Field,
            field_cam: cordata_view::field::Camera::default(),
            field_bounds: Default::default(),
            field_drag: None,
            field_runs: Vec::new(),
            field_runs_note: None,
            field_runs_busy: false,
            field_polling: false,
            field_idle_open: false,
            tray: None,
            tray_tick: 0,
            selected_turn: None,
            sheet_expanded: false,
            sheet_toggled_files: HashSet::new(),
            sheet_expanded_hunks: HashSet::new(),
            sheet_all_expanded: None,
            desk_cache: DeskCache::default(),
            tugs: crate::motion::Tugs::default(),
            gate_unlocked: None,
            gate_hint: None,
            _subscriptions: subscriptions,
        };
        if this.desk_mode == DeskMode::Rack {
            this.ensure_run_poll(cx);
        }
        this.start_tray(window, cx);
        let cwd = this.cwd.clone();
        this.prefs.touch_recent(&cwd, prefs::now());
        this.prefs.save();
        if restoring.is_some() {
            let shown = restore.active_index();
            for (i, entry) in restore.open.iter().enumerate() {
                let session = if i == shown { first.clone() } else { Self::resumed(&this.connection, entry, this.prefs.model.clone(), cx) };
                this.track(session, window, cx);
            }
            this.show(first, cx);
            // Retain user's configured default desk mode upon launch/restoration
            this.desk_mode = default_desk_mode;
            if open_folder {
                this.switch_project(launch_cwd, window, cx);
            }
        } else {
            this.adopt(first, window, cx);
        }
        this.schedule_git(cx);
        this.watch_hotkey(window, cx);
        this.start_meter(window, cx);
        this.load_tree(cx);
        this.load_review(cx);
        if picker {
            this.open_overlay(Overlay::Picker, window, cx);
        }
        if let Some(show) = show {
            this.show_at_launch(&show, window, cx);
        }
        this
    }

    fn show_at_launch(&mut self, what: &str, window: &mut Window, cx: &mut Context<Self>) {
        match what.split_once(':') {
            Some(("settings", section)) => {
                if let Some(&found) = settings_modal::Section::ALL.iter().find(|s| s.matches(section)) {
                    self.settings_section = found;
                }
                self.open_overlay(Overlay::Settings, window, cx);
            }
            _ if what == "about" => self.open_overlay(Overlay::About, window, cx),
            _ if what == "agents" => {
                self.settings_section = settings_modal::Section::Agents;
                self.open_overlay(Overlay::Settings, window, cx);
            }
            _ if what == "sheet" => self.desk_mode = DeskMode::ChangeSheet,
            _ if what == "rack" => self.desk_mode = DeskMode::Rack,
            _ if what == "field" => {
                self.desk_mode = DeskMode::Rack;
                self.rack_view_mode = cordata_view::flight::RackViewMode::Field;
                self.ensure_run_poll(cx);
            }
            _ if what == "models" => self.popup = Some(meter::Popup::Models),
            _ if what == "terminal" => {
                self.panel_open = true;
                self.dock_tab = DockTab::Terminal;
                if self.shell_on_show {
                    self.ensure_terminal(cx);
                }
            }
            _ => {}
        }
    }

    /// The quick-ask combination (prefs are normalised on load, so this parses).
    pub(crate) fn quick_ask_combo(&self) -> crate::platform::Combo {
        crate::platform::Combo::parse(&self.prefs.quick_ask_key).unwrap_or_else(|_| crate::platform::Combo::parse("ctrl-alt-space").expect("the default parses"))
    }

    /// Bind (or unbind) the hotkey as the prefs say. False when another app holds the combination.
    pub(crate) fn apply_hotkey(&self) -> bool {
        // Never from a test run: a global hotkey belongs to the installed app.
        if cfg!(test) {
            return true;
        }
        let combo = self.quick_ask_combo();
        crate::platform::set_hotkey(self.prefs.quick_ask_hotkey.then_some(&combo))
    }

    /// The quick-ask hotkey from anywhere: the window comes forward with the composer focused.
    /// `cordata-app status|ls|do`: requests from the control socket, answered on the UI thread.
    fn serve_control(window: &mut Window, cx: &mut Context<Self>) {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        crate::control::listen(tx);
        cx.spawn_in(window, async move |this, cx| {
            while let Some(request) = rx.recv().await {
                let crate::control::Request { line, reply } = request;
                let answered = this.update_in(cx, |this, window, cx| this.on_control(&line, window, cx));
                match answered {
                    Ok(value) => {
                        let _ = reply.send(value);
                    }
                    Err(_) => break,
                }
            }
        })
        .detach();
    }

    /// The agent in the dock terminal, if one is recognised (`detect.rs`).
    fn control_agent(&self) -> serde_json::Value {
        self.terminal.detection.as_ref().map_or(serde_json::Value::Null, |d| serde_json::json!({"id": d.id, "name": d.name, "state": d.state.label()}))
    }

    /// One thread as a script sees it.
    fn control_thread(&self, index: usize, cx: &App) -> serde_json::Value {
        let entity = &self.sessions[index];
        let view = &entity.read(cx).view;
        serde_json::json!({
            "n": index + 1,
            "title": view.title(),
            "state": view.tab_state().lifecycle(),
            "shown": entity.entity_id() == self.session.entity_id(),
            "session_id": view.session.as_ref().map(|s| s.session_id.clone()),
        })
    }

    fn on_control(&mut self, request: &serde_json::Value, window: &mut Window, cx: &mut Context<Self>) -> serde_json::Value {
        use serde_json::json;
        let shown = |this: &Self| this.sessions.iter().position(|s| s.entity_id() == this.session.entity_id()).unwrap_or(0);
        let arg = request.get("arg").and_then(serde_json::Value::as_str).map(str::trim).filter(|a| !a.is_empty());
        match (request.get("cmd").and_then(serde_json::Value::as_str), request.get("action").and_then(serde_json::Value::as_str)) {
            (Some("status"), _) => json!({"ok": true, "project": self.cwd.display().to_string(), "threads": self.sessions.len(), "thread": self.control_thread(shown(self), cx), "terminal_agent": self.control_agent()}),
            (Some("ls"), _) => json!({"ok": true, "threads": (0..self.sessions.len()).map(|i| self.control_thread(i, cx)).collect::<Vec<_>>()}),
            // herdr's `pane read`: what the dock terminal shows, as text.
            (Some("read"), _) => match self.terminal_text(cx) {
                Some(text) => json!({"ok": true, "title": text.title, "lines": text.lines, "exited": text.exited, "agent": self.control_agent()}),
                None => json!({"ok": false, "error": "no terminal is running; `cordata-app do run <command>` starts one"}),
            },
            (Some("do"), Some(action)) => {
                match (action, arg) {
                    ("new", None) => self.open_new(window, cx),
                    ("close", None) => self.close_active(cx),
                    ("next", None) => self.switch(SwitchTarget::Next, cx),
                    ("prev", None) => self.switch(SwitchTarget::Prev, cx),
                    ("attention", None) => self.switch(SwitchTarget::Attention, cx),
                    ("switch", Some(n)) => match n.parse::<usize>() {
                        Ok(n) if (1..=self.sessions.len()).contains(&n) => self.switch(SwitchTarget::Slot(n), cx),
                        _ => return json!({"ok": false, "error": format!("no thread {n}; `cordata-app ls` lists them")}),
                    },
                    ("prompt", Some(text)) => {
                        let session = self.session.clone();
                        if !session.read(cx).view.input_enabled() {
                            return json!({"ok": false, "error": "the shown thread is busy or waiting for an approval"});
                        }
                        session.update(cx, |s, cx| s.submit(text, cx));
                    }
                    // herdr's `pane run`: types a command into the dock terminal (your shell) and presses Return.
                    ("run", Some(command)) => {
                        if !self.panel_open || self.dock_tab != DockTab::Terminal {
                            self.switch_dock(DockTab::Terminal, cx);
                        }
                        self.ensure_terminal(cx);
                        if !self.terminal_type(&format!("{command}\r")) {
                            return json!({"ok": false, "error": "the terminal is not running"});
                        }
                    }
                    ("focus", None) => {
                        cx.activate(true);
                        window.activate_window();
                    }
                    _ => return json!({"ok": false, "error": format!("unknown command `{action}`{}", arg.map_or(String::new(), |a| format!(" {a}")))}),
                }
                cx.notify();
                json!({"ok": true, "thread": self.control_thread(shown(self), cx)})
            }
            _ => json!({"ok": false, "error": "expected {\"cmd\": \"status\" | \"ls\" | \"do\"}"}),
        }
    }

    fn watch_hotkey(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.apply_hotkey() {
            eprintln!("cordata: {} is taken by another app; quick ask is off", self.quick_ask_combo().label);
        }
        cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_millis(200)).await;
                if crate::platform::take_hotkey() {
                    let alive = this.update_in(cx, |this, window, cx| {
                        cx.activate(true);
                        window.activate_window();
                        this.focus_top(window, cx);
                        if this.overlays.is_empty() {
                            this.composer.update(cx, |s, cx| s.focus(window, cx));
                        }
                    });
                    if alive.is_err() {
                        break;
                    }
                } else if this.update(cx, |_, _| ()).is_err() {
                    break;
                }
            }
        })
        .detach();
    }

    pub(super) fn project_name(&self) -> String {
        self.cwd.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| self.cwd.display().to_string())
    }

    // -------------------------------------------------------------------------------------------- sessions

    /// Watch a session and show it.
    fn adopt(&mut self, session: Entity<Session>, window: &mut Window, cx: &mut Context<Self>) {
        self.track(session.clone(), window, cx);
        self.show(session, cx);
    }

    /// A thread continuing the saved conversation `entry` names, in its folder.
    fn resumed(connection: &Connection, entry: &threads::Open, model: Option<String>, cx: &mut Context<Self>) -> Entity<Session> {
        cx.new(|cx| {
            let mut session = Session::start(connection, entry.cwd.clone(), None, model, cx);
            session.resume(entry.id.clone(), cx);
            session
        })
    }

    /// Watch a session and add it to the open threads, without showing it.
    fn track(&mut self, session: Entity<Session>, window: &mut Window, cx: &mut Context<Self>) {
        let id = session.entity_id();
        self.refresh_saved(cx);
        let subs = vec![
            cx.observe_in(&session, window, move |this, observed, window, cx| {
                this.sound_and_notify(&observed, window, cx);
                // The conversation's id arrives after the engine starts, and its file appears when a turn ends.
                let mark = observed.read(cx).saved_id().map(|id| (id.to_owned(), observed.read(cx).view.turns_done));
                let mark_changed = mark.is_some() && this.thread_marks.get(&observed.entity_id()) != mark.as_ref();
                if mark_changed {
                    this.persist_threads(cx);
                    if observed.entity_id() == this.session.entity_id() && this.desk_mode == DeskMode::ChangeSheet {
                        this.load_review(cx);
                    }
                }
                if observed.entity_id() == this.session.entity_id() {
                    this.step_selftest(cx);
                }
                cx.notify();
            }),
            cx.subscribe_in(&session, window, |this, _, event: &SessionEvent, window, cx| match event {
                // The Dock icon bounces when the window is not in front (an approval, a finished turn).
                SessionEvent::NeedsAttention => {
                    window.request_attention();
                    this.refresh_saved(cx);
                }
                SessionEvent::Window(request) => this.on_window_request(request.clone(), window, cx),
                SessionEvent::ModelChosen(model) => {
                    this.prefs.model = Some(model.clone());
                    this.prefs.save();
                }
            }),
        ];
        self.subscriptions.push((id, subs));
        self.sessions.push(session);
    }

    /// Writes which conversations are open, in order, and which is shown — when that changed. Only conversations that
    /// exist on disk are listed: a thread that has not finished a turn has nothing to come back to.
    fn persist_threads(&mut self, cx: &mut Context<Self>) {
        if !self.prefs.restore_threads {
            return;
        }
        let exists = self.session_exists;
        let shown = self.session.read(cx).saved_id().map(str::to_owned);
        let mut marks = HashMap::new();
        let mut open = Vec::new();
        for entity in &self.sessions {
            let session = entity.read(cx);
            let Some(id) = session.saved_id() else { continue };
            marks.insert(entity.entity_id(), (id.to_owned(), session.view.turns_done));
            if exists(&session.cwd, id) {
                open.push(threads::Open { id: id.to_owned(), cwd: session.cwd.clone() });
            }
        }
        self.thread_marks = marks;
        self.threads_armed |= !open.is_empty();
        if self.threads_armed {
            self.threads.update(threads::Layout::new(shown, open));
        }
    }

    pub(super) fn show(&mut self, session: Entity<Session>, cx: &mut Context<Self>) {
        session.update(cx, |s, _| s.view.mark_seen());
        // A thread owns its folder: the tree, review and branch follow it, so an approval is never read against another
        // project's diff.
        let home = session.read(cx).cwd.clone();
        self.session = session;
        self.desk_mode = DeskMode::Transcript;
        self.selected_turn = None;
        self.sheet_expanded = false;
        self.gate_unlocked = None;
        self.gate_hint = None;
        if home != self.cwd {
            self.retarget(home, cx);
        }
        let len = self.row_count(cx);
        self.transcript.reset(len);
        self.seen = (self.session.entity_id(), len, self.session.read(cx).version);
        self.persist_threads(cx);
        cx.notify();
    }

    pub(super) fn open_new(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (connection, cwd, model) = (self.connection.clone(), self.cwd.clone(), self.prefs.model.clone());
        let session = cx.new(|cx| Session::start(&connection, cwd, None, model, cx));
        self.adopt(session, window, cx);
        self.focus_composer(&FocusComposer, window, cx);
    }

    fn close_active(&mut self, cx: &mut Context<Self>) {
        let id = self.session.entity_id();
        if let Some(index) = self.sessions.iter().position(|s| s.entity_id() == id) {
            self.close_session_at(index, cx);
        }
    }

    pub(super) fn close_session_at(&mut self, index: usize, cx: &mut Context<Self>) {
        if self.sessions.len() <= 1 || index >= self.sessions.len() {
            return; // the window always shows one session; ⌘Q quits
        }
        let removed = self.sessions.remove(index);
        let removed_id = removed.entity_id();
        self.threads_armed = true;
        self.refresh_saved(cx);
        self.subscriptions.retain(|(sid, _)| *sid != removed_id);
        if self.session.entity_id() == removed_id {
            let next = self.sessions[index.min(self.sessions.len() - 1)].clone();
            self.show(next, cx);
        } else {
            self.persist_threads(cx);
            cx.notify();
        }
    }

    fn switch(&mut self, target: SwitchTarget, cx: &mut Context<Self>) {
        let len = self.sessions.len();
        let index = self.sessions.iter().position(|s| s.entity_id() == self.session.entity_id()).unwrap_or(0);
        let next = match target {
            SwitchTarget::Next => (index + 1) % len,
            SwitchTarget::Prev => (index + len - 1) % len,
            SwitchTarget::Slot(n) => n.saturating_sub(1).min(len - 1),
            SwitchTarget::Attention => {
                use cordata_view::app::TabState;
                let states: Vec<_> = self.sessions.iter().map(|s| s.read(cx).view.tab_state()).collect();
                let next = cordata_view::app::attention_index(&states, index);
                // A thread waiting on an approval first, then an agent waiting in the terminal, then a Done thread.
                let terminal_waits = self.terminal.blocked() && !(self.panel_open && self.dock_tab == DockTab::Terminal);
                match next {
                    Some(next) if states[next] == TabState::NeedsApproval => next,
                    _ if terminal_waits => return self.switch_dock(DockTab::Terminal, cx),
                    Some(next) => next,
                    None => return,
                }
            }
        };
        let session = self.sessions[next].clone();
        if self.desk_mode == DeskMode::Rack {
            self.desk_mode = DeskMode::Transcript;
        }
        self.show(session, cx);
    }

    fn on_window_request(&mut self, request: WindowRequest, window: &mut Window, cx: &mut Context<Self>) {
        match request {
            WindowRequest::New => self.open_new(window, cx),
            WindowRequest::Close => self.close_active(cx),
            WindowRequest::Review => {
                self.load_review(cx);
                sheet::record_ux_event("mode_switch", serde_json::json!({ "mode": "change_sheet", "source": "slash_review" }));
                self.desk_mode = DeskMode::ChangeSheet;
                cx.notify();
            }
            WindowRequest::Switch(target) => self.switch(target, cx),
        }
    }

    /// Retry, by hand: a fresh session in the stopped one's place. The error stays on screen until this is clicked, and
    /// the conversation lived in the stopped engine.
    pub(super) fn reconnect(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let id = self.session.entity_id();
        let (connection, cwd, model) = (self.connection.clone(), self.cwd.clone(), self.prefs.model.clone());
        let fresh = cx.new(|cx| Session::start(&connection, cwd, None, model, cx));
        self.sessions.retain(|s| s.entity_id() != id);
        self.subscriptions.retain(|(sid, _)| *sid != id);
        self.adopt(fresh, window, cx);
    }

    // -------------------------------------------------------------------------------------------- selftest

    /// Driven by the session's own changes, not by frames: an occluded window draws nothing, and this must still run.
    fn step_selftest(&mut self, cx: &mut Context<Self>) {
        let Some(test) = self.selftest.as_mut() else { return };
        let (pending, done, active, closed) = {
            let view = &self.session.read(cx).view;
            (!view.pending.is_empty(), view.turns_done, view.turn_active, matches!(view.conn, ConnState::Closed { .. }))
        };
        if std::env::var_os("CORDATA_SELFTEST_TRACE").is_some() {
            eprintln!("selftest: pending={pending} turns_done={done} active={active} closed={closed}");
        }
        test.started |= active;
        if pending {
            test.approvals += 1;
            let session = self.session.clone();
            // Deferred: answering re-enters the session, which is being observed right now.
            cx.defer(move |cx| session.update(cx, |session, cx| session.answer(ApprovalDecision::Allow, cx)));
            return;
        }
        if (test.started && !active) || closed {
            let view = &self.session.read(cx).view;
            let tools = view.messages.iter().filter(|m| matches!(m, cordata_view::app::Message::Tool(_))).count();
            let agent = view.messages.iter().filter(|m| matches!(m, cordata_view::app::Message::Agent(_))).count();
            let errors: Vec<String> = view.messages.iter().filter_map(|m| if let cordata_view::app::Message::Error(e) = m { Some(e.clone()) } else { None }).collect();
            println!("SELFTEST connected={} turns={} messages={} agent_blocks={} tool_cards={} approvals={} errors={:?}", !closed, done, view.messages.len(), agent, tools, test.approvals, errors);
            self.selftest = None;
            cx.quit();
        }
    }

    /// How many display rows the transcript has (messages after folding).
    fn row_count(&self, cx: &App) -> usize {
        display_rows(self.session.read(cx)).0.len()
    }

    fn sync_desk_cache(&mut self, cx: &App) {
        let active_id = self.session.entity_id();
        let check = self.check_command();
        let peers_sig: Vec<(usize, EntityId, u64, usize)> = self
            .sessions
            .iter()
            .enumerate()
            .filter(|(_, entity)| entity.entity_id() != active_id)
            .map(|(ix, entity)| {
                let p = entity.read(cx);
                (ix + 1, entity.entity_id(), p.version, p.view.messages.len())
            })
            .collect();
        let (version, msg_len, pending_id) = {
            let s = self.session.read(cx);
            (s.version, s.view.messages.len(), s.view.pending.front().map(|p| p.info.tool_call_id.clone()))
        };
        let key = DeskCacheKey {
            session_id: active_id,
            version,
            msg_len,
            pending_id: pending_id.clone(),
            check: check.clone(),
            peers: peers_sig,
        };
        if self.desk_cache.key.as_ref() == Some(&key) {
            return;
        }
        let active_cwd = self.session.read(cx).cwd.clone();
        let peers_edited: Vec<(usize, Vec<String>)> = self
            .sessions
            .iter()
            .enumerate()
            .filter(|(_, entity)| entity.entity_id() != active_id)
            .filter_map(|(ix, entity)| {
                let peer = entity.read(cx);
                (peer.cwd == active_cwd).then(|| (ix + 1, cordata_view::desk::edited_paths(&peer.view.messages, &peer.cwd)))
            })
            .filter(|(_, paths)| !paths.is_empty())
            .collect();
        let session = self.session.read(cx);
        let snap = cordata_view::desk::snapshot(&session.view.messages, &session.cwd, pending_id.as_deref(), check.as_deref(), &peers_edited);
        let (rows, fold_ctx) = display_rows(session);
        self.desk_cache = DeskCache {
            key: Some(key),
            snap: std::rc::Rc::new(snap),
            rows: std::rc::Rc::new(rows),
            fold_ctx: std::rc::Rc::new(fold_ctx),
        };
    }

    fn sync_transcript(&mut self, cx: &App) {
        let session = self.session.read(cx);
        let (id, len, version) = (self.session.entity_id(), self.desk_cache.rows.len(), session.version);
        let (old_id, old_len, old_version) = self.seen;
        // Another thread, or a cleared one (folding shrinks the count by a row or two, never by a screenful).
        if id != old_id || len + REMEASURE_TAIL < old_len {
            self.transcript.reset(len);
        } else if len != old_len || version != old_version {
            // A new card may fold into the group before it, so the row count can shrink: re-measure the tail either way.
            let start = old_len.min(len).saturating_sub(REMEASURE_TAIL);
            self.transcript.splice(start..old_len, len - start);
        }
        self.seen = (id, len, version);
    }

    // -------------------------------------------------------------------------------------------- actions

    /// esc: the topmost overlay or menu closes first; then an approval is denied; then, pressed twice, the run stops.
    fn stop(&mut self, _: &StopTurn, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(top) = self.top_overlay() {
            self.close_overlay(top, window, cx);
            return;
        }
        if self.row_menu.is_some() {
            self.row_menu = None;
            cx.notify();
            return;
        }
        if self.session.read(cx).view.menu.is_some() {
            self.session.update(cx, |session, cx| session.menu_dismiss(cx));
            return;
        }
        if self.saying {
            self.saying = false;
            cx.notify();
            return;
        }
        if self.desk_mode == DeskMode::Rack {
            self.desk_mode = DeskMode::Transcript;
            cx.notify();
            return;
        }
        if !self.session.read(cx).view.pending.is_empty() {
            self.answer_gate(ApprovalDecision::Reject, cx);
            return;
        }
        if self.desk_mode == DeskMode::ChangeSheet {
            self.desk_mode = DeskMode::Transcript;
            cx.notify();
            return;
        }
        if self.session.read(cx).view.turn_active {
            match self.esc_at {
                Some(at) if at.elapsed() < DOUBLE_ESC => {
                    self.esc_at = None;
                    self.session.update(cx, |session, cx| session.cancel(cx));
                }
                _ => {
                    self.esc_at = Some(Instant::now());
                    cx.spawn(async move |this, cx| {
                        cx.background_executor().timer(DOUBLE_ESC).await;
                        let _ = this.update(cx, |_, cx| cx.notify());
                    })
                    .detach();
                }
            }
            cx.notify();
        }
    }

    fn focus_composer(&mut self, _: &FocusComposer, window: &mut Window, cx: &mut Context<Self>) {
        self.composer.update(cx, |state, cx| state.focus(window, cx));
    }

    pub(super) fn new_session(&mut self, _: &NewSession, window: &mut Window, cx: &mut Context<Self>) {
        self.open_new(window, cx);
    }

    fn close_session(&mut self, _: &CloseSession, _: &mut Window, cx: &mut Context<Self>) {
        if self.dock_tab == DockTab::Editor && !self.files.buffers.is_empty() {
            self.close_active_buffer(cx);
            return;
        }
        self.close_active(cx);
    }

    fn show_files_action(&mut self, _: &ShowFiles, _: &mut Window, cx: &mut Context<Self>) {
        self.switch_dock(DockTab::Files, cx);
    }

    fn show_editor_action(&mut self, _: &ShowEditor, _: &mut Window, cx: &mut Context<Self>) {
        self.switch_dock(DockTab::Editor, cx);
    }

    fn show_review_action(&mut self, _: &ShowReview, _: &mut Window, cx: &mut Context<Self>) {
        self.switch_dock(DockTab::Review, cx);
    }

    fn show_artifacts_action(&mut self, _: &ShowArtifacts, _: &mut Window, cx: &mut Context<Self>) {
        self.switch_dock(DockTab::Artifacts, cx);
    }

    fn show_terminal_action(&mut self, _: &ShowTerminal, window: &mut Window, cx: &mut Context<Self>) {
        self.toggle_terminal(&ToggleTerminal, window, cx);
    }

    fn show_mcphub_action(&mut self, _: &ShowMcpHub, window: &mut Window, cx: &mut Context<Self>) {
        self.settings_section = settings_modal::Section::Mcp;
        self.on_settings_section(cx);
        self.open_overlay(Overlay::Settings, window, cx);
    }

    fn open_files_action(&mut self, _: &OpenFiles, window: &mut Window, cx: &mut Context<Self>) {
        self.switch_dock(DockTab::Files, cx);
        self.file_filter.update(cx, |state, cx| state.focus(window, cx));
    }

    fn save_file_action(&mut self, _: &SaveFile, _: &mut Window, cx: &mut Context<Self>) {
        self.save_active(cx);
    }

    fn toggle_deck_action(&mut self, _: &ToggleDeck, _: &mut Window, cx: &mut Context<Self>) {
        self.toggle_change_sheet(cx);
    }

    fn toggle_rack_action(&mut self, _: &ToggleRack, _: &mut Window, cx: &mut Context<Self>) {
        self.toggle_rack(cx);
    }

    fn force_allow_gate_action(&mut self, _: &ForceAllowGate, _: &mut Window, cx: &mut Context<Self>) {
        if self.desk_mode != DeskMode::Rack && !self.session.read(cx).view.pending.is_empty() {
            self.answer_gate(ApprovalDecision::Allow, cx);
        }
    }

    pub(super) fn switch_dock(&mut self, tab: DockTab, cx: &mut Context<Self>) {
        self.dock_tab = tab;
        if !self.panel_open {
            self.set_panel(true, cx);
        }
        match tab {
            DockTab::Files => self.load_tree(cx),
            DockTab::Review => self.load_review(cx),
            DockTab::Artifacts => self.load_artifacts(cx),
            // The shell starts the first time the pane is shown (tests click to start one).
            DockTab::Terminal if self.shell_on_show => self.ensure_terminal(cx),
            _ => {}
        }
        cx.notify();
    }

    fn next_session(&mut self, _: &NextSession, _: &mut Window, cx: &mut Context<Self>) {
        self.switch(SwitchTarget::Next, cx);
    }

    fn prev_session(&mut self, _: &PrevSession, _: &mut Window, cx: &mut Context<Self>) {
        self.switch(SwitchTarget::Prev, cx);
    }

    /// ⌘J: the thread that needs you — one waiting on an approval, then one that finished while you were elsewhere.
    fn next_attention(&mut self, _: &NextAttention, _: &mut Window, cx: &mut Context<Self>) {
        self.switch(SwitchTarget::Attention, cx);
    }

    /// ⌘K: the command menu — the composer with `/` typed, so every command (and custom one) is listed and narrows as
    /// you type, exactly as in the terminal.
    fn command_menu(&mut self, _: &CommandMenu, window: &mut Window, cx: &mut Context<Self>) {
        self.composer.update(cx, |state, cx| {
            state.set_value("/", window, cx);
            state.focus(window, cx);
        });
        self.session.update(cx, |session, cx| session.mirror_input("/", cx));
    }

    fn toggle_thoughts(&mut self, _: &ToggleThoughts, _: &mut Window, cx: &mut Context<Self>) {
        self.session.update(cx, |session, cx| session.toggle_thoughts(cx));
        let len = self.row_count(cx);
        self.transcript.reset(len);
        self.seen.1 = len;
    }

    fn toggle_tools(&mut self, _: &ToggleTools, _: &mut Window, cx: &mut Context<Self>) {
        self.session.update(cx, |session, cx| session.toggle_tools(cx));
        let len = self.row_count(cx);
        self.transcript.reset(len);
        self.seen.1 = len;
    }

    fn help(&mut self, _: &Help, _: &mut Window, cx: &mut Context<Self>) {
        self.session.update(cx, |session, cx| session.submit("/help", cx));
    }

    // -------------------------------------------------------------------------------------------- the composer

    fn on_composer(&mut self, input: &Entity<TextareaState>, event: &InputEvent, window: &mut Window, cx: &mut Context<Self>) {
        match event {
            InputEvent::Change => {
                let text = input.read(cx).value().to_string();
                self.session.update(cx, |session, cx| session.mirror_input(&text, cx));
            }
            InputEvent::PressEnter { secondary, shift: false } => {
                let enter_sends = self.prefs.send_with != "cmd-enter";
                let has_menu = self.session.read(cx).view.menu.is_some();
                if has_menu && !*secondary {
                    let text = self.session.update(cx, |session, cx| session.menu_accept(true, cx));
                    input.update(cx, |state, cx| state.set_value(text, window, cx));
                    return;
                }
                // ↵ sends (⇧↵ is a newline), or with ⌘↵ chosen in Settings, ⌘↵ sends and ↵ is a newline.
                if enter_sends == *secondary {
                    return;
                }
                if self.saying {
                    let text = input.read(cx).value().trim_end_matches('\n').to_string();
                    self.saying = false;
                    input.update(cx, |state, cx| state.set_value("", window, cx));
                    if !text.trim().is_empty() {
                        self.session.update(cx, |session, cx| session.answer(ApprovalDecision::Say(text), cx));
                    }
                    return;
                }
                self.send_composer(window, cx);
            }
            _ => {}
        }
    }

    /// Whether a keystroke should be intercepted before inner input keybindings (e.g. MoveUp/MoveDown in Textarea)
    /// swallow it.
    ///
    /// Menus, overlays and the approval's one-key answers take arrows, Tab, Enter and Escape before the text box
    /// underneath turns them into caret movement.
    fn should_intercept_keystroke(&self, keystroke: &gpui::Keystroke, window: &Window, cx: &App) -> bool {
        let plain = !keystroke.modifiers.modified();
        let key = keystroke.key.as_str();

        // 1. Overlay: Palette takes up/down/enter/escape before its input field sees them.
        if self.overlays.last() == Some(&Overlay::Palette) {
            return matches!(key, "up" | "down" | "enter" | "escape");
        }

        // 2. Overlay: Picker takes up/down/enter/escape before its input field sees them.
        if self.overlays.last() == Some(&Overlay::Picker) {
            return matches!(key, "up" | "down" | "enter" | "escape");
        }

        // 3. Other overlays close on plain Escape.
        if self.overlays.last().is_some() && key == "escape" && plain {
            return true;
        }

        // Everything below is the composer's: with focus elsewhere (the terminal, the editor) keys are theirs.
        let is_composer_focused = self.composer.read(cx).focus_handle(cx).is_focused(window);
        if !is_composer_focused {
            return false;
        }

        // 4. The `/` and `@` menu above the composer.
        if self.session.read(cx).view.menu.is_some() {
            return matches!(key, "up" | "down" | "tab" | "enter" | "escape");
        }

        // 5. The composer without a menu.
        {
            let (pending, empty) = {
                let view = &self.session.read(cx).view;
                let pending = view.pending.front();
                (pending.is_some(), self.composer.read(cx).value().is_empty())
            };
            if self.desk_mode != DeskMode::Rack && pending && empty && plain && !self.saying {
                return matches!(key, "a" | "y" | "s" | "r" | "n" | "k" | "w" | "enter");
            }
            let single_line = !self.composer.read(cx).value().contains('\n');
            if plain && single_line && matches!(key, "up" | "down") {
                return true;
            }
        }

        false
    }

    /// Arrow keys, Tab and Escape belong to the `/`/`@` menu while it is open (before the text box sees them).
    fn composer_keys(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let mods = event.keystroke.modifiers;
        // ⌘V with an image or files on the clipboard attaches them (text pastes as usual).
        if event.keystroke.key == "v" && mods.platform && !mods.shift && !mods.alt && self.paste_image(cx) {
            cx.stop_propagation();
            return;
        }
        let (has_menu, pending, empty) = {
            let view = &self.session.read(cx).view;
            let pending = view.pending.front();
            (view.menu.is_some(), pending.is_some(), self.composer.read(cx).value().is_empty())
        };
        let plain = !event.keystroke.modifiers.modified();
        let gate_visible = pending && self.desk_mode != DeskMode::Rack;
        // An approval card takes single keys while the composer is empty: routine gates accept a/y/s/↵; high-risk
        // gates hold Allow until their challenge token is confirmed (or ⌘Y is pressed), while r/n/k/esc/w remain instant.
        if gate_visible && empty && plain && !self.saying {
            let locked = self.active_gate_locked(cx);
            match event.keystroke.key.as_str() {
                "a" | "y" | "s" if locked => {
                    self.gate_hint = Some("High-risk gate: confirm the risk token above or press ⌘Y to allow.".into());
                    cx.stop_propagation();
                    cx.notify();
                    return;
                }
                "a" | "y" => {
                    self.answer_gate(ApprovalDecision::Allow, cx);
                    cx.stop_propagation();
                    return;
                }
                "s" => {
                    let has_always = self.session.read(cx).view.pending.front().is_some_and(|p| p.info.always_label.is_some());
                    let dec = if has_always { ApprovalDecision::AllowAlways } else { ApprovalDecision::Allow };
                    self.answer_gate(dec, cx);
                    cx.stop_propagation();
                    return;
                }
                "r" | "n" | "k" => {
                    self.answer_gate(ApprovalDecision::Reject, cx);
                    cx.stop_propagation();
                    return;
                }
                "w" => {
                    self.saying = true;
                    cx.stop_propagation();
                    cx.notify();
                    return;
                }
                _ => {}
            }
        }
        // ↵ on an empty composer answers a routine card; high-risk cards require challenge confirmation or ⌘Y.
        if gate_visible && empty && !self.saying && event.keystroke.key == "enter" && !event.keystroke.modifiers.modified() {
            if self.active_gate_locked(cx) {
                self.gate_hint = Some("High-risk gate: confirm the risk token above or press ⌘Y to allow.".into());
                cx.notify();
            } else {
                self.answer_gate(ApprovalDecision::Allow, cx);
            }
            cx.stop_propagation();
            return;
        }
        if !has_menu {
            // ↑/↓ walk the prompts already sent, while the box holds one line.
            let single_line = !self.composer.read(cx).value().contains('\n');
            if plain && single_line && matches!(event.keystroke.key.as_str(), "up" | "down") {
                let back = event.keystroke.key == "up";
                let text = self.session.update(cx, |session, cx| session.history(back, cx));
                self.composer.update(cx, |state, cx| state.set_value(text, window, cx));
                cx.stop_propagation();
            }
            return;
        }
        match event.keystroke.key.as_str() {
            "up" => self.session.update(cx, |session, cx| session.menu_move(-1, cx)),
            "down" => self.session.update(cx, |session, cx| session.menu_move(1, cx)),
            "tab" => {
                let text = self.session.update(cx, |session, cx| session.menu_accept(false, cx));
                self.composer.update(cx, |state, cx| state.set_value(text, window, cx));
            }
            "enter" => {
                let text = self.session.update(cx, |session, cx| session.menu_accept(true, cx));
                self.composer.update(cx, |state, cx| state.set_value(text, window, cx));
            }
            "escape" => self.session.update(cx, |session, cx| session.menu_dismiss(cx)),
            _ => return,
        }
        cx.stop_propagation();
    }

    // -------------------------------------------------------------------------------------------- overlays

    pub(super) fn open_overlay(&mut self, overlay: Overlay, window: &mut Window, cx: &mut Context<Self>) {
        self.overlays.retain(|o| *o != overlay);
        self.overlays.push(overlay);
        match overlay {
            Overlay::Palette => {
                self.palette_ix = 0;
                self.palette_input.update(cx, |s, cx| {
                    s.set_value("", window, cx);
                    s.focus(window, cx);
                });
            }
            Overlay::Picker => {
                self.picker_ix = 0;
                self.picker_note = None;
                self.picker_input.update(cx, |s, cx| {
                    s.set_value("", window, cx);
                    s.focus(window, cx);
                });
            }
            Overlay::Clone => {
                self.reset_clone_sheet(window, cx);
                self.clone_url.update(cx, |s, cx| s.focus(window, cx));
            }
            Overlay::Settings => {
                self.open_select = None;
                self.on_settings_section(cx);
                window.focus(&self.settings_focus, cx);
            }
            Overlay::Confirm | Overlay::About => window.focus(&self.settings_focus, cx),
            Overlay::Rename => self.rename_input.update(cx, |s, cx| s.focus(window, cx)),
        }
        cx.notify();
    }

    pub(super) fn close_overlay(&mut self, overlay: Overlay, window: &mut Window, cx: &mut Context<Self>) {
        self.overlays.retain(|o| *o != overlay);
        if overlay == Overlay::Confirm {
            self.confirm = None;
        }
        self.focus_top(window, cx);
        cx.notify();
    }

    /// Focus what is on top: the overlay's input, or the composer when nothing floats.
    fn focus_top(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.top_overlay() {
            Some(Overlay::Palette) => self.palette_input.update(cx, |s, cx| s.focus(window, cx)),
            Some(Overlay::Picker) => self.picker_input.update(cx, |s, cx| s.focus(window, cx)),
            Some(Overlay::Clone) => self.clone_url.update(cx, |s, cx| s.focus(window, cx)),
            Some(Overlay::Settings | Overlay::Confirm | Overlay::About) => window.focus(&self.settings_focus, cx),
            Some(Overlay::Rename) => self.rename_input.update(cx, |s, cx| s.focus(window, cx)),
            None => self.composer.update(cx, |s, cx| s.focus(window, cx)),
        }
    }

    fn top_overlay(&self) -> Option<Overlay> {
        self.overlays.last().copied()
    }

    pub(super) fn ask_confirm(&mut self, what: Confirm, window: &mut Window, cx: &mut Context<Self>) {
        self.confirm = Some(what);
        self.open_overlay(Overlay::Confirm, window, cx);
    }

    fn confirmed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(what) = self.confirm.take() else { return };
        self.close_overlay(Overlay::Confirm, window, cx);
        match what {
            Confirm::SignOut => {
                match crate::keychain::forget() {
                    Ok(()) => self.settings_view.set_notice("Signed out: the key is gone from the Keychain.".into(), false),
                    Err(error) => self.settings_view.set_notice(error, true),
                }
                self.api_key_masked = crate::keychain::masked();
            }
            Confirm::ResetPrefs => {
                let recents = std::mem::take(&mut self.prefs.recents);
                self.prefs = Prefs { recents: if self.prefs.recents.is_empty() { Vec::new() } else { recents }, ..Prefs::default() };
                self.prefs.touch_recent(&self.cwd.clone(), prefs::now());
                self.prefs.save();
                cx.set_global(self.prefs.clone());
                crate::theme::set_theme(&self.prefs.theme, cx);
                self.apply_send_with(cx);
                self.sidebar_open = true;
                self.settings_view.set_notice("This window's preferences are back to their defaults.".into(), false);
            }
            Confirm::RemoveWorktree { path, branch } => {
                let cwd = self.cwd.clone();
                let job = cx.background_executor().spawn(async move { git_ops::remove_merged_worktree(&cwd, &path, branch.as_deref()) });
                cx.spawn(async move |this, cx| {
                    let outcome = job.await;
                    let _ = this.update(cx, |this, cx| {
                        this.git.note = Some(match outcome {
                            Ok(said) => (said, false),
                            Err(why) => (cordata_view::shown(&why), true),
                        });
                        this.refresh_git(false, cx);
                    });
                })
                .detach();
            }
        }
        cx.notify();
    }

    fn confirm_dialog(&self, cx: &mut Context<Self>) -> AnyElement {
        let tk = tk(cx);
        let (title, body, yes): (&str, String, &str) = match self.confirm.as_ref() {
            Some(Confirm::SignOut) => ("Sign out?", "The API key is removed from the Keychain. Requests fail until you add one again (or export it in your shell).".into(), "Sign out"),
            Some(Confirm::ResetPrefs) => ("Reset all settings?", "Theme, layout, sending and notification choices go back to their defaults. Harness settings files are not touched.".into(), "Reset"),
            Some(Confirm::RemoveWorktree { path, branch }) => (
                "Remove this checkout?",
                format!(
                    "{} is deleted from disk{}. Its commits are already in the default branch. Git refuses if anything there is uncommitted.",
                    tilde(path),
                    branch.as_ref().map(|b| format!(", and the merged branch {b} with it")).unwrap_or_default()
                ),
                "Remove",
            ),
            None => ("", String::new(), ""),
        };
        let danger = true;
        div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .child(
                div()
                    .id("confirm")
                    .role(gpui::Role::AlertDialog)
                    .aria_label(SharedString::from(title.to_owned()))
                    .track_focus(&self.settings_focus)
                    .key_context("Confirm")
                    .capture_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| match event.keystroke.key.as_str() {
                        "escape" => {
                            this.close_overlay(Overlay::Confirm, window, cx);
                            cx.stop_propagation();
                        }
                        "enter" => {
                            this.confirmed(window, cx);
                            cx.stop_propagation();
                        }
                        _ => {}
                    }))
                    .w(px(420.))
                    .flex()
                    .flex_col()
                    .gap(px(12.))
                    .p(px(20.))
                    .rounded(px(t::RADIUS_MODAL))
                    .bg(c(tk.bg_input))
                    .border_1()
                    .border_color(line(cx, t::LINE_CONTROL_STRONG))
                    .shadow(crate::theme::modal_shadow(cx))
                    .child(div().text_size(px(t::TEXT_SETTINGS_TITLE)).font_weight(gpui::FontWeight::MEDIUM).text_color(c(tk.text_primary)).child(title))
                    .child(div().text_size(px(t::TEXT_UI)).line_height(gpui::relative(1.5)).text_color(c(tk.text_secondary)).child(SharedString::from(body)))
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .gap(px(8.))
                            .pt(px(4.))
                            .child(ui::button("confirm-no", "Cancel  esc", Kind::Ghost, cx).on_click(cx.listener(|this, _, window, cx| this.close_overlay(Overlay::Confirm, window, cx))))
                            .child(ui::button("confirm-yes", SharedString::from(format!("{yes}  ↵")), if danger { Kind::Danger } else { Kind::Primary }, cx).on_click(cx.listener(|this, _, window, cx| this.confirmed(window, cx)))),
                    ),
            )
            .into_any_element()
    }

    fn about_dialog(&self, cx: &mut Context<Self>) -> AnyElement {
        let tk = tk(cx);
        let line_of = |text: SharedString, size: f32, color| div().text_size(px(size)).text_color(color).child(text);
        div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .on_mouse_down(gpui::MouseButton::Left, cx.listener(|this, _, window, cx| this.close_overlay(Overlay::About, window, cx)))
            .child(
                div()
                    .id("about")
                    .role(gpui::Role::Dialog)
                    .aria_label("About Cordata")
                    .track_focus(&self.settings_focus)
                    .key_context("About")
                    .capture_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                        if matches!(event.keystroke.key.as_str(), "escape" | "enter") {
                            this.close_overlay(Overlay::About, window, cx);
                            cx.stop_propagation();
                        }
                    }))
                    .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .w(px(340.))
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(px(6.))
                    .p(px(24.))
                    .rounded(px(t::RADIUS_MODAL))
                    .bg(crate::theme::c(crate::theme::tk(cx).bg_input))
                    .border_1()
                    .border_color(line(cx, t::LINE_CONTROL_STRONG))
                    .shadow(crate::theme::modal_shadow(cx))
                    .child(ui::cordata_logo(54., cx))
                    .child(div().pt(px(4.)).text_size(px(t::TEXT_SETTINGS_TITLE)).font_weight(gpui::FontWeight::MEDIUM).text_color(c(tk.text_primary)).child("Cordata"))
                    .child(line_of(SharedString::from(format!("Version {}", env!("CARGO_PKG_VERSION"))), t::TEXT_UI, c(tk.text_secondary)))
                    .child(line_of("A native harness for coding agents.".into(), t::TEXT_UI, c(tk.text_secondary)))
                    .child(line_of(SharedString::from(format!("Project: {}", self.cwd.display())), t::TEXT_UI, c(tk.text_muted)))
                    .child(div().pt(px(10.)).child(ui::button("about-ok", "OK  ↵", Kind::Primary, cx).on_click(cx.listener(|this, _, window, cx| this.close_overlay(Overlay::About, window, cx))))),
            )
            .into_any_element()
    }

    // -------------------------------------------------------------------------------------------- panel, project, trust

    pub(super) fn set_sidebar(&mut self, open: bool, cx: &mut Context<Self>) {
        if self.sidebar_open != open {
            self.sidebar_open = open;
            self.prefs.sidebar_open = Some(open);
            self.prefs.save();
        }
        cx.notify();
    }

    pub(super) fn toggle_sidebar(&mut self, _: &ToggleSidebar, window: &mut Window, cx: &mut Context<Self>) {
        let open = !self.sidebar_open;
        self.set_sidebar(open, cx);
        if !open {
            self.composer.update(cx, |s, cx| s.focus(window, cx));
        }
    }

    pub(super) fn set_panel(&mut self, open: bool, cx: &mut Context<Self>) {
        if self.panel_open != open {
            self.panel_open = open;
            self.prefs.panel_open = Some(open);
            self.prefs.save();
        }
        cx.notify();
    }

    pub(super) fn toggle_panel(&mut self, _: &TogglePanel, window: &mut Window, cx: &mut Context<Self>) {
        let open = !self.panel_open;
        self.set_panel(open, cx);
        if !open {
            // Focus may have been inside the panel: keys must still reach the window.
            self.composer.update(cx, |s, cx| s.focus(window, cx));
        }
    }

    pub(super) fn toggle_terminal(&mut self, _: &ToggleTerminal, window: &mut Window, cx: &mut Context<Self>) {
        if self.panel_open && self.dock_tab == DockTab::Terminal {
            self.set_panel(false, cx);
            self.composer.update(cx, |s, cx| s.focus(window, cx));
        } else {
            self.set_panel(true, cx);
            self.switch_dock(DockTab::Terminal, cx);
            { let focus = self.terminal.focus.clone(); window.focus(&focus, cx); }
        }
    }

    fn open_palette(&mut self, _: &OpenPalette, window: &mut Window, cx: &mut Context<Self>) {
        if self.top_overlay() == Some(Overlay::Palette) {
            self.close_overlay(Overlay::Palette, window, cx);
        } else {
            self.open_overlay(Overlay::Palette, window, cx);
        }
    }

    fn switch_project_action(&mut self, _: &SwitchProject, window: &mut Window, cx: &mut Context<Self>) {
        // ⌘O again inside the picker is its "Open folder…".
        if self.top_overlay() == Some(Overlay::Picker) {
            self.choose_folder(window, cx);
        } else {
            self.open_overlay(Overlay::Picker, window, cx);
        }
    }

    fn open_settings(&mut self, _: &OpenSettings, window: &mut Window, cx: &mut Context<Self>) {
        self.open_overlay(Overlay::Settings, window, cx);
    }

    /// Open a folder as the project: new conversations run there, the panes follow, it goes first in the recents.
    pub(super) fn switch_project(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let path = match path.canonicalize() {
            Ok(path) if path.is_dir() => path,
            _ => {
                self.picker_note = Some((format!("{} is not a folder.", tilde(&path)), true));
                cx.notify();
                return;
            }
        };
        self.overlays.retain(|o| !matches!(o, Overlay::Picker | Overlay::Clone));
        if path == self.cwd {
            cx.notify();
            return;
        }
        self.prefs.touch_recent(&path, prefs::now());
        self.prefs.save();
        // Back to a folder with a thread still open (an agent's worktree, the main checkout): pick it up where it was.
        let open = self.sessions.iter().rev().find(|s| s.read(cx).cwd == path).cloned();
        match open {
            Some(session) => self.show(session, cx),
            None => {
                self.retarget(path.clone(), cx);
                let (connection, model) = (self.connection.clone(), self.prefs.model.clone());
                let session = cx.new(|cx| Session::start(&connection, path, None, model, cx));
                self.adopt(session, window, cx);
            }
        }
        self.composer.update(cx, |s, cx| s.focus(window, cx));
        cx.notify();
    }

    /// Reads this project's saved conversations off the UI thread; the rail lists the ones that are not open.
    pub(super) fn refresh_saved(&mut self, cx: &mut Context<Self>) {
        let owner = self.cwd.clone();
        let cwd = owner.clone();
        let job = cx.background_executor().spawn(async move { acp_core::history::list(&cwd) });
        cx.spawn(async move |this, cx| {
            let saved = job.await;
            let _ = this.update(cx, |this, cx| {
                if this.cwd == owner && this.saved != saved {
                    this.saved = saved;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Opens a saved conversation as a thread of its own (the one shown is left alone).
    pub(super) fn open_saved(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        let (connection, cwd, model) = (self.connection.clone(), self.cwd.clone(), self.prefs.model.clone());
        let session = Self::resumed(&connection, &threads::Open { id, cwd }, model, cx);
        self.adopt(session, window, cx);
        self.focus_composer(&FocusComposer, window, cx);
    }

    /// Points the folder-bound panes — tree, review, branch and PR — at `path`. Threads keep their own folders.
    fn retarget(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.cwd = path;
        self.files = editor::Files::default();
        self.review = review::Review::default();
        self.sheet_toggled_files.clear();
        self.sheet_expanded_hunks.clear();
        self.sheet_all_expanded = None;
        self.git.pr = None;
        self.git.note = None;
        self.git.worktrees.clear();
        self.saved.clear();
        self.refresh_saved(cx);
        self.load_tree(cx);
        self.load_review(cx);
        self.refresh_git(true, cx);
    }

    /// The sidebar footer and status bar: is the connection well?
    pub(super) fn connection_status(&self, cx: &App) -> (Dot, String) {
        let session = self.session.read(cx);
        match &session.view.conn {
            ConnState::Connecting => (Dot::Hollow, "Connecting…".into()),
            ConnState::Closed { .. } => (Dot::Amber, "Engine stopped".into()),
            _ => (Dot::Ok, format!("Local · {}", session.transport)),
        }
    }


    // -------------------------------------------------------------------------------------------- composer helpers

    pub(super) fn begin_saying(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.saying = true;
        self.composer.update(cx, |s, cx| s.focus(window, cx));
        cx.notify();
    }

    pub(super) fn apply_starter(&mut self, prompt: &str, send_now: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.composer.update(cx, |s, cx| {
            s.set_value(prompt.to_owned(), window, cx);
            s.focus(window, cx);
        });
        self.session.update(cx, |session, cx| session.mirror_input(prompt, cx));
        if send_now {
            self.send_composer(window, cx);
        } else {
            cx.notify();
        }
    }


    pub(super) fn send_composer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.composer.read(cx).value().trim_end_matches('\n').to_string();
        if text.trim().is_empty() && self.staged.is_empty() {
            return;
        }
        if !self.staged.is_empty() {
            let view = &self.session.read(cx).view;
            if view.turn_active || !view.pending.is_empty() {
                self.attach_note = Some("Attachments go with a new prompt — wait for this run to finish (or steer it with text).".into());
                cx.notify();
                return;
            }
            if text.trim_start().starts_with('/') || text.trim_start().starts_with('!') {
                self.attach_note = Some("Attachments go with a prompt, not a command.".into());
                cx.notify();
                return;
            }
            let names: Vec<String> = self.staged.iter().map(|s| s.name.clone()).collect();
            let body = if text.trim().is_empty() { "Here are the attached files.".to_owned() } else { text };
            let shown = format!("{body}\n\n[attached: {}]", names.join(", "));
            let items: Vec<acp_core::Attachment> = self.staged.drain(..).map(|s| s.attachment).collect();
            self.attach_note = None;
            if self.desk_mode != DeskMode::Transcript {
                self.desk_mode = DeskMode::Transcript;
            }
            self.composer.update(cx, |state, cx| state.set_value("", window, cx));
            self.session.update(cx, |session, cx| session.submit_with(&shown, items, cx));
            return;
        }
        self.attach_note = None;
        if self.desk_mode != DeskMode::Transcript {
            self.desk_mode = DeskMode::Transcript;
        }
        self.composer.update(cx, |state, cx| state.set_value("", window, cx));
        self.session.update(cx, |session, cx| session.submit(&text, cx));
    }

    pub(super) fn stage(&mut self, result: Result<attachments::Staged, String>, cx: &mut Context<Self>) {
        match result {
            Ok(_) if self.staged.len() >= attachments::MAX_ATTACHMENTS => self.attach_note = Some(format!("At most {} attachments on one prompt.", attachments::MAX_ATTACHMENTS)),
            Ok(item) => {
                self.attach_note = None;
                self.staged.push(item);
            }
            Err(why) => self.attach_note = Some(why),
        }
        cx.notify();
    }

    pub(super) fn add_paths(&mut self, paths: &[PathBuf], cx: &mut Context<Self>) {
        for path in paths {
            self.stage(attachments::from_path(path), cx);
        }
    }

    /// ⌘V with an image on the clipboard attaches it; anything else pastes as text. Returns whether it attached.
    pub(super) fn paste_image(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(item) = cx.read_from_clipboard() else { return false };
        let mut took = false;
        for entry in item.entries() {
            match entry {
                gpui::ClipboardEntry::Image(image) => {
                    let ext = match image.format {
                        gpui::ImageFormat::Png => "png",
                        gpui::ImageFormat::Jpeg => "jpg",
                        gpui::ImageFormat::Webp => "webp",
                        gpui::ImageFormat::Gif => "gif",
                        gpui::ImageFormat::Svg => "svg",
                        gpui::ImageFormat::Tiff => "tiff",
                        gpui::ImageFormat::Bmp => "bmp",
                        gpui::ImageFormat::Ico => "ico",
                        gpui::ImageFormat::Pnm => "pnm",
                    };
                    let name = format!("pasted-{}.{ext}", self.staged.len() + 1);
                    self.stage(attachments::clipboard_image(&name, image.format, &image.bytes), cx);
                    took = true;
                }
                gpui::ClipboardEntry::ExternalPaths(paths) => {
                    self.add_paths(paths.paths(), cx);
                    took = true;
                }
                _ => {}
            }
        }
        // Screenshots and Preview copies can put only TIFF/HEIC on the pasteboard, which GPUI does not surface.
        if !took
            && item.text().is_none_or(|t| t.is_empty())
            && let Some(png) = crate::platform::pasteboard_image_png()
        {
            let name = format!("pasted-{}.png", self.staged.len() + 1);
            self.stage(attachments::image(&name, "image/png", &png), cx);
            took = true;
        }
        took
    }

    /// The system's file panel, for the composer's +.
    pub(super) fn choose_attachments(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let answer = cx.prompt_for_paths(gpui::PathPromptOptions { files: true, directories: false, multiple: true, prompt: Some("Attach".into()) });
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(paths))) = answer.await {
                let _ = this.update(cx, |this, cx| this.add_paths(&paths, cx));
            }
        })
        .detach();
    }

    // -------------------------------------------------------------------------------------------- rename, archive, search

    pub(super) fn begin_worktree(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.renaming = Some(Naming::Worktree);
        self.rename_input.update(cx, |s, cx| s.set_value("", window, cx));
        self.open_overlay(Overlay::Rename, window, cx);
    }

    fn finish_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let title = self.rename_input.read(cx).value().trim().to_owned();
        let Some(naming) = self.renaming.take() else { return };
        self.close_overlay(Overlay::Rename, window, cx);
        match naming {
            Naming::Worktree => self.new_worktree_conversation(&title, window, cx),
        }
    }

    /// The palette's query is remembered so a stale answer is never shown.
    fn search_conversations(&mut self, cx: &mut Context<Self>) {
        self.search_for = self.palette_input.read(cx).value().trim().to_owned();
    }

    fn rename_dialog(&self, cx: &mut Context<Self>) -> AnyElement {
        let tk = tk(cx);
        div()
            .size_full()
            .flex()
            .items_start()
            .justify_center()
            .pt(gpui::relative(0.2))
            .child(
                div()
                    .id("rename")
                    .role(gpui::Role::Dialog)
                    .aria_label("Rename conversation")
                    .capture_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                        if event.keystroke.key == "escape" {
                            this.renaming = None;
                            this.close_overlay(Overlay::Rename, window, cx);
                            cx.stop_propagation();
                        }
                    }))
                    .w(px(420.))
                    .flex()
                    .flex_col()
                    .gap(px(12.))
                    .p(px(20.))
                    .rounded(px(t::RADIUS_MODAL))
                    .bg(c(tk.bg_input))
                    .border_1()
                    .border_color(line(cx, t::LINE_CONTROL_STRONG))
                    .shadow(crate::theme::modal_shadow(cx))
                    .child(div().text_size(px(t::TEXT_SETTINGS_TITLE)).text_color(c(tk.text_primary)).child("New conversation in a worktree"))
                    .when(self.renaming == Some(Naming::Worktree), |d| d.child(div().text_size(px(t::TEXT_SMALL)).text_color(c(tk.text_muted)).child("A name for the task: it becomes branch cordata/<name> in a folder beside this repository.")))
                    .child(div().h(px(32.)).flex().items_center().px(px(10.)).rounded(px(t::RADIUS_CARD)).bg(c(tk.bg_window)).border_1().border_color(line(cx, t::LINE_CONTROL_STRONG)).text_size(px(t::TEXT_UI)).child(gpui_component::input::Input::new(&self.rename_input).appearance(false)))
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .gap(px(8.))
                            .child(ui::button("rename-cancel", "Cancel  esc", Kind::Ghost, cx).on_click(cx.listener(|this, _, window, cx| {
                                this.renaming = None;
                                this.close_overlay(Overlay::Rename, window, cx);
                            })))
                            .child(ui::button("rename-save", "Create  ↵", Kind::Primary, cx).on_click(cx.listener(|this, _, window, cx| this.finish_rename(window, cx)))),
                    ),
            )
            .into_any_element()
    }

    pub(super) fn copy_last_reply(&mut self, cx: &mut Context<Self>) {
        let last = self.session.read(cx).view.messages.iter().rev().find_map(|m| match m {
            cordata_view::app::Message::Agent(text) if !text.trim().is_empty() => Some(cordata_view::shown(text)),
            _ => None,
        });
        if let Some(text) = last {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    /// The send key from the preferences: ↵ (⇧↵ newline) or ⌘↵ (↵ newline).
    pub(super) fn apply_send_with(&mut self, cx: &mut Context<Self>) {
        let enter_sends = self.prefs.send_with != "cmd-enter";
        self.composer.update(cx, |s, cx| s.set_submit_on_enter(enter_sends, cx));
    }

    /// The status bar's "press esc again" hint, while the first press is fresh.
    pub(super) fn esc_hint(&self) -> Option<&'static str> {
        self.esc_at.filter(|at| at.elapsed() < DOUBLE_ESC).map(|_| "esc again stops the run")
    }

    fn switch_slot(&mut self, n: usize, cx: &mut Context<Self>) {
        if n <= self.sessions.len() {
            self.switch(SwitchTarget::Slot(n), cx);
        }
    }

    fn sound_and_notify(&mut self, session: &Entity<Session>, window: &mut Window, cx: &mut Context<Self>) {
        let id = session.entity_id();
        let (active, pending) = {
            let view = &session.read(cx).view;
            (view.turn_active, !view.pending.is_empty())
        };
        let was = self.turn_seen.insert(id, active).unwrap_or(false);
        let in_front = window.is_window_active();
        if !active && in_front && id == self.session.entity_id() {
            // Finished in front of the person: nothing to mark Done.
            session.update(cx, |s, _| s.view.mark_seen());
        }
        // What you can see: the window is in front and this is the thread on screen.
        let in_view = in_front && id == self.session.entity_id();
        if was && !active {
            let (unseen, title) = {
                let view = &session.read(cx).view;
                (view.unseen, view.title())
            };
            // `unseen` is false for a cancelled turn and for one that finished in front of you.
            if unseen && !pending && !in_view && self.prefs.sound_on_completion {
                self.sounds.play(crate::sound::Cue::Done);
            }
            if unseen && !in_front && self.prefs.notify_done && !pending {
                crate::platform::notify(crate::platform::Notice::Done, "Cordata finished", &title);
            }
        }
        if pending {
            let first = self.pending_seen.insert(id);
            if first && !in_view && self.prefs.sound_on_completion {
                self.sounds.play(crate::sound::Cue::NeedsYou);
            }
            if should_notify(first, self.prefs.notify_approval, window.is_window_active(), self.notified_away) {
                self.notified_away = true;
                crate::platform::notify(crate::platform::Notice::Approval, "Cordata needs you", "A command is waiting for your approval.");
            }
        } else {
            self.pending_seen.remove(&id);
        }
    }

    fn overlay_layer(&self, window: &Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.overlays.is_empty() {
            return None;
        }
        let tk = tk(cx);
        // A scrim under modals; the picker is a popover and keeps the window visible.
        let modal = self.overlays.iter().any(|o| matches!(o, Overlay::Palette | Overlay::Settings | Overlay::Clone | Overlay::Confirm | Overlay::Rename | Overlay::About));
        let mut layer = div().id("overlays").absolute().inset_0().when(modal, |l| l.bg(ca(tk.bg_window, t::SCRIM))).occlude();
        for overlay in &self.overlays {
            let element = match overlay {
                Overlay::Palette => self.palette(window, cx),
                Overlay::Picker => self.picker(window, cx),
                Overlay::Settings => self.settings_modal(window, cx),
                Overlay::Clone => self.clone_sheet(cx),
                Overlay::Confirm => self.confirm_dialog(cx),
                Overlay::Rename => self.rename_dialog(cx),
                Overlay::About => self.about_dialog(cx),
            };
            layer = layer.child(div().absolute().inset_0().child(element));
        }
        Some(layer.into_any_element())
    }
}

/// What the transcript draws (docs/auto-mode.md §1.1): quiet tool calls folded into activity rows, targets shortened
/// against the thread's folder and `$HOME`. A call waiting for approval stays its own row.
fn display_rows(session: &Session) -> (Vec<fold::Row>, fold::Context) {
    let mut ctx = fold::Context::here(session.cwd.clone());
    ctx.waiting = session.view.pending.iter().map(|p| p.info.tool_call_id.clone()).collect();
    // The session's own notes are said once, in the brief's meta line, not as the transcript's first screen; and
    // the brief's prompt is the page's title, so its row is not drawn again.
    let notes = cordata_view::desk::session_notes(&session.view.messages);
    let brief = cordata_view::desk::brief_message(&session.view.messages);
    let rows = fold::rows_with(&session.view.messages, &ctx).into_iter().filter(|r| !matches!(r, fold::Row::Message(i) if notes.contains(i) || Some(*i) == brief)).collect();
    (rows, ctx)
}

/// Whether a pending approval earns a system notification: only the first sighting, only with the window out of front,
/// and only once per absence (the Dock bounce covers the rest), so ten approvals in a minute are one banner.
fn should_notify(first_sighting: bool, enabled: bool, window_active: bool, notified_away: bool) -> bool {
    first_sighting && enabled && !window_active && !notified_away
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.poll_disk(window, cx);
        self.refresh_check();
        self.sync_desk_cache(cx);
        self.sync_transcript(cx);
        let placeholder = self.placeholder(cx);
        if placeholder != self.composer_placeholder {
            self.composer_placeholder = placeholder;
            self.composer.update(cx, |s, cx| s.set_placeholder(placeholder, window, cx));
        }
        let tk = tk(cx);
        let size = window.viewport_size();
        let window_w = size.width.as_f32();
        let sidebar_w = self.rail_w();
        let panel_w = self.dock_w(window_w);
        let column_w = (window_w - sidebar_w - panel_w).max(0.);
        let session = self.session.clone();
        let expanded = self.expanded.clone();
        let thoughts = session.read(cx).view.thoughts_expanded;
        // Anything past the session banner (a reply, a `!command`'s output, `/cost`) is worth the transcript.
        let has_conversation = transcript::has_content(&session.read(cx).view);
        let this = cx.entity().downgrade();
        // Memoized across frames when session.version is unchanged.
        let rows = std::rc::Rc::clone(&self.desk_cache.rows);
        let fold_ctx = std::rc::Rc::clone(&self.desk_cache.fold_ctx);
        let transcript_list = list(self.transcript.clone(), move |ix, _window, cx| {
            let view = &session.read(cx).view;
            let Some(row) = rows.get(ix) else { return div().into_any_element() };
            let is_tool_row = |r: Option<&fold::Row>| matches!(r, Some(fold::Row::Message(j)) if matches!(view.messages.get(*j), Some(cordata_view::app::Message::Tool(_))));
            let edge = GroupEdge { first: ix == 0 || !is_tool_row(rows.get(ix - 1)), last: !is_tool_row(rows.get(ix + 1)) };
            let (card_id, is_thought) = match row {
                // An activity row opens and closes on its first card's id.
                fold::Row::Activity(a) => (match view.messages.get(a.first) {
                    Some(cordata_view::app::Message::Tool(card)) => Some(card.id.clone()),
                    _ => None,
                }, false),
                fold::Row::Message(i) => match view.messages.get(*i) {
                    Some(cordata_view::app::Message::Tool(card)) => (Some(card.id.clone()), false),
                    Some(cordata_view::app::Message::Thought(_)) => (None, true),
                    _ => (None, false),
                },
            };
            // The approval card shows the call waiting on it: its own row would say it twice.
            if let fold::Row::Message(i) = row
                && let Some(cordata_view::app::Message::Tool(card)) = view.messages.get(*i)
                && view.awaiting_approval(&card.id)
            {
                return div().into_any_element();
            }
            let open = view.expand_tools || card_id.as_ref().is_some_and(|id| expanded.contains(id));
            let fold_ctx = fold_ctx.clone();
            let last_row = ix + 1 == rows.len();
            let row = row.clone();
            let (this, session) = (this.clone(), session.clone());

            let (turn_label, is_anchor, is_pitch, is_tool) = match &row {
                fold::Row::Message(i) => match view.messages.get(*i) {
                    Some(cordata_view::app::Message::User(_)) => {
                        let turn = view.messages.iter().take(*i + 1).filter(|m| matches!(m, cordata_view::app::Message::User(_))).count();
                        (Some(format!("{:02}", turn)), true, false, false)
                    }
                    Some(cordata_view::app::Message::Agent(_)) => (None, false, true, false),
                    Some(cordata_view::app::Message::Tool(_)) => (None, false, false, true),
                    _ => (None, false, false, false),
                },
                fold::Row::Activity(_) => (None, false, false, true),
            };

            let block = match &row {
                fold::Row::Message(i) => match view.messages.get(*i) {
                    Some(message) => transcript::block(message, *i, edge, open, thoughts, &fold_ctx, cx),
                    None => div().into_any_element(),
                },
                fold::Row::Activity(a) => {
                    // Opened, the group lists its calls as plain rows (Ctrl+E opens each one's output).
                    let body = if open {
                        let tool_at = |j: usize| matches!(view.messages.get(j), Some(cordata_view::app::Message::Tool(_)));
                        (a.first..=a.last)
                            .filter_map(|j| {
                                let message = view.messages.get(j)?;
                                let edge = GroupEdge { first: j == a.first || !tool_at(j - 1), last: j == a.last || !tool_at(j + 1) };
                                Some(transcript::block(message, j, edge, view.expand_tools, thoughts, &fold_ctx, cx))
                            })
                            .collect()
                    } else {
                        Vec::new()
                    };
                    transcript::activity(a, open, body, cx)
                }
            };
            // The newest block rises 6px into place as it fades in (200 ms): you see what arrived.
            let animated_block = if last_row && crate::ui::motion(cx) {
                gpui::AnimationExt::with_animation(
                    div().relative().child(block),
                    ("arrive", ix), gpui::Animation::new(crate::motion::SETTLE).with_easing(gpui::ease_out_quint()), |el, t| {
                        el.opacity(t).top(px(crate::motion::SETTLE_RISE * (1. - t)))
                    },
                )
                .into_any_element()
            } else {
                block
            };

            div()
                .w_full()
                .flex()
                .justify_center()
                .px(px(24.))
                .child(
                    div()
                        .id(("block", ix))
                        .w_full()
                        .max_w(px(t::PAGE_WIDTH + t::ROPE_MARGIN))
                        .when(card_id.is_some() || is_thought, |el| el.cursor_pointer())
                        .on_click(move |_, _, cx| {
                            if let Some(id) = &card_id {
                                let id = id.clone();
                                let _ = this.update(cx, |this, cx| {
                                    if !this.expanded.remove(&id) {
                                        this.expanded.insert(id);
                                    }
                                    // The row's height changes: lay it out again.
                                    this.transcript.splice(ix..ix + 1, 1);
                                    cx.notify();
                                });
                            } else if is_thought {
                                session.update(cx, |session, cx| session.toggle_thoughts(cx));
                            }
                        })
                        .child(
                            div()
                                .w_full()
                                .flex()
                                .flex_row()
                                .child(
                                    // 120px left margin: cord + marginalia
                                    div()
                                        .w(px(t::ROPE_MARGIN))
                                        .flex_shrink_0()
                                        .relative()
                                        .flex()
                                        .justify_end()
                                        .items_start()
                                        .pr(px(20.))
                                        .pt(px(if is_anchor { 18. } else if is_pitch { 10. } else { 6. }))
                                        // The vertical cord
                                        .child(
                                            div()
                                                .absolute()
                                                .top_0()
                                                .bottom_0()
                                                .right(px(10.))
                                                .w(px(1.))
                                                .bg(line(cx, t::LINE_DEFAULT)),
                                        )
                                        // Node on the cord
                                        .child(if is_anchor {
                                            // Small square knot
                                            div()
                                                .absolute()
                                                .top(px(24.))
                                                .right(px(8.5))
                                                .size(px(4.))
                                                .rounded(px(1.))
                                                .bg(c(tk.amber))
                                                .into_any_element()
                                        } else if is_pitch {
                                            // Pitch round node
                                            div()
                                                .absolute()
                                                .top(px(14.))
                                                .right(px(8.5))
                                                .size(px(4.))
                                                .rounded_full()
                                                .bg(ca(tk.amber, t::LIVE_REST))
                                                .into_any_element()
                                        } else if is_tool {
                                            // Quiet tool node
                                            div()
                                                .absolute()
                                                .top(px(14.))
                                                .right(px(9.))
                                                .size(px(3.))
                                                .rounded_full()
                                                .bg(c(tk.text_faint))
                                                .into_any_element()
                                        } else {
                                            div().into_any_element()
                                        })
                                        // Marginalia (turn number)
                                        .children(turn_label.map(|lbl| {
                                            div()
                                                .font_family(mono(cx))
                                                .text_size(px(t::TEXT_META))
                                                .text_color(c(tk.text_faint))
                                                .child(SharedString::from(lbl))
                                        })),
                                )
                                // Manuscript column (680px)
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .max_w(px(t::PAGE_WIDTH))
                                        .child(animated_block),
                                ),
                        ),
                )
                .into_any_element()
        })
        .size_full();
        let pending_info = self.session.read(cx).view.pending.front().map(|p| p.info.clone());
        let pending = (self.desk_mode == DeskMode::Transcript).then(|| pending_info.as_ref().map(|info| self.approval_card(info, cx))).flatten();
        let actions_row = (self.desk_mode == DeskMode::Transcript).then(|| self.receipt(cx)).flatten();
        let brief = (self.desk_mode != DeskMode::Rack).then(|| self.brief_block(cx)).flatten();
        let places = (has_conversation && self.desk_mode != DeskMode::Rack && column_w >= desk::PLACES_MIN_COLUMN).then(|| self.places_margin(cx));
        let menu = self.slash_menu(cx);
        // The field takes the whole window: no thread list, no side panel, no header, no bottom composer.
        let field_window = self.desk_mode == DeskMode::Rack && self.rack_view_mode == cordata_view::flight::RackViewMode::Field;
        let sidebar = (self.sidebar_open && !field_window).then(|| self.sidebar(cx));
        let header = self.header(column_w, cx);
        let composer = self.composer(cx);
        let panel = (self.panel_open && !field_window).then(|| self.side_panel(window_w, window, cx));
        let overlays = self.overlay_layer(window, cx);
        let grips = if field_window { Vec::new() } else { self.resize_grips(window_w, cx) };
        let drop_ring = crate::theme::amber(cx, t::AMBER_BORDER_STRONG);
        let body: AnyElement = match self.desk_mode {
            DeskMode::Rack => div().flex_1().min_h_0().child(self.triage_rack_view(cx)).into_any_element(),
            DeskMode::ChangeSheet => div().flex_1().min_h_0().child(self.change_sheet(cx)).into_any_element(),
            _ if has_conversation => div().id("transcript").role(gpui::Role::Log).aria_label("Transcript").flex_1().min_h_0().child(transcript_list).into_any_element(),
            _ => div().flex_1().min_h_0().child(self.empty_state(cx)).into_any_element(),
        };

        let header = (!field_window).then_some(header);
        let desk: AnyElement = if field_window {
            self.field_window(composer, menu, cx)
        } else {
            div()
                .flex()
                .flex_1()
                .min_h_0()
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_w_0()
                        .children(brief)
                        .child(body)
                        .child(
                            div()
                                .w_full()
                                .flex()
                                .justify_center()
                                .px(px(24.))
                                .pb(px(16.))
                                .child(div().w_full().max_w(px(t::PAGE_WIDTH + t::ROPE_MARGIN)).flex().flex_col().children(actions_row).children(pending).children(menu).child(composer)),
                        ),
                )
                .children(places)
                .into_any_element()
        };

        div()
            .id("workspace")
            .key_context("Workspace")
            .on_action(cx.listener(Self::stop))
            .on_action(cx.listener(Self::focus_composer))
            .on_action(cx.listener(|this, _: &CycleMode, _, cx| this.cycle_mode(cx)))
            .on_action(cx.listener(Self::new_session))
            .on_action(cx.listener(Self::close_session))
            .on_action(cx.listener(Self::next_session))
            .on_action(cx.listener(Self::next_attention))
            .on_action(cx.listener(Self::prev_session))
            .on_action(cx.listener(Self::command_menu))
            .on_action(cx.listener(Self::open_palette))
            .on_action(cx.listener(Self::switch_project_action))
            .on_action(cx.listener(Self::toggle_sidebar))
            .on_action(cx.listener(Self::toggle_panel))
            .on_action(cx.listener(Self::toggle_terminal))
            .on_action(cx.listener(Self::open_settings))
            .on_action(cx.listener(Self::toggle_thoughts))
            .on_action(cx.listener(Self::toggle_tools))
            .on_action(cx.listener(Self::help))
            .on_action(cx.listener(Self::show_files_action))
            .on_action(cx.listener(Self::show_editor_action))
            .on_action(cx.listener(Self::show_review_action))
            .on_action(cx.listener(Self::show_artifacts_action))
            .on_action(cx.listener(Self::show_terminal_action))
            .on_action(cx.listener(Self::show_mcphub_action))
            .on_action(cx.listener(Self::open_files_action))
            .on_action(cx.listener(Self::save_file_action))
            .on_action(cx.listener(Self::toggle_deck_action))
            .on_action(cx.listener(Self::toggle_rack_action))
            .on_action(cx.listener(Self::force_allow_gate_action))
            .on_action(cx.listener(|this, _: &SwitchTo1, _, cx| this.switch_slot(1, cx)))
            .on_action(cx.listener(|this, _: &SwitchTo2, _, cx| this.switch_slot(2, cx)))
            .on_action(cx.listener(|this, _: &SwitchTo3, _, cx| this.switch_slot(3, cx)))
            .on_action(cx.listener(|this, _: &SwitchTo4, _, cx| this.switch_slot(4, cx)))
            .on_action(cx.listener(|this, _: &SwitchTo5, _, cx| this.switch_slot(5, cx)))
            .on_action(cx.listener(|this, _: &SwitchTo6, _, cx| this.switch_slot(6, cx)))
            .on_action(cx.listener(|this, _: &SwitchTo7, _, cx| this.switch_slot(7, cx)))
            .on_action(cx.listener(|this, _: &SwitchTo8, _, cx| this.switch_slot(8, cx)))
            .on_action(cx.listener(|this, _: &SwitchTo9, _, cx| this.switch_slot(9, cx)))
            .on_action(cx.listener(|this, _: &About, window, cx| this.open_overlay(Overlay::About, window, cx)))
            .on_action(cx.listener(|this, _: &AttachFiles, window, cx| this.choose_attachments(window, cx)))
            .on_action(cx.listener(|_, _: &Minimize, window, _| window.minimize_window()))
            .on_action(cx.listener(|_, _: &Zoom, window, _| window.zoom_window()))
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .bg(crate::theme::c(crate::theme::tk(cx).bg_window))
            .font_family(crate::theme::sans(cx))
            .text_color(c(tk.text_body))
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .relative()
                    .when(self.resizing.is_some(), |el| el.cursor_col_resize())
                    .on_mouse_move(cx.listener(move |this, event: &gpui::MouseMoveEvent, _, cx| this.on_resize_move(event, window_w, cx)))
                    .on_mouse_up(gpui::MouseButton::Left, cx.listener(|this, _, _, cx| this.end_resize(cx)))
                    .children(sidebar)
                    .child(
                        div()
                            .id("conversation")
                            .on_drop(cx.listener(|this, paths: &gpui::ExternalPaths, _, cx| this.add_paths(paths.paths(), cx)))
                            .drag_over::<gpui::ExternalPaths>(move |style, _, _, _| style.border_2().border_color(drop_ring))
                            .flex_1()
                            .h_full()
                            .min_w_0()
                            .relative()
                            .flex()
                            .flex_col()
                            .bg(crate::theme::c(crate::theme::tk(cx).bg_main))
                            .children(header)
                            .child(desk)
                    )
                    .children(panel)
                    .children(grips),
            )
            .children(overlays)
            .child(div().absolute().bottom(px(36.)).right(px(panel_w + 12.)).when(std::env::var_os("CORDATA_FPS").is_some(), |this| this.child(gpui_fps::fps_monitor(window, cx))))
    }
}

pub(super) fn age(seconds: f64) -> String {
    let s = seconds.max(0.0) as u64;
    match s {
        0..60 => "now".into(),
        60..3600 => format!("{}m", s / 60),
        3600..86_400 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86_400),
    }
}

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        gpui::KeyBinding::new("escape", StopTurn, Some("Workspace")),
        gpui::KeyBinding::new("cmd-.", StopTurn, Some("Workspace")),
        // In the terminal these keys belong to the program (vim's Escape, readline's ⌃R, ⌃K, ⌃E).
        gpui::KeyBinding::new("escape", gpui::NoAction, Some("CordataTerminal")),
        gpui::KeyBinding::new("ctrl-k", gpui::NoAction, Some("CordataTerminal")),
        gpui::KeyBinding::new("ctrl-r", gpui::NoAction, Some("CordataTerminal")),
        gpui::KeyBinding::new("ctrl-e", gpui::NoAction, Some("CordataTerminal")),
        gpui::KeyBinding::new("cmd-l", FocusComposer, Some("Workspace")),
        gpui::KeyBinding::new("shift-tab", CycleMode, Some("Workspace")),
        gpui::KeyBinding::new("cmd-n", NewSession, Some("Workspace")),
        gpui::KeyBinding::new("cmd-w", CloseSession, Some("Workspace")),
        gpui::KeyBinding::new("cmd-w", CloseSession, Some("CordataEditor")),
        gpui::KeyBinding::new("cmd-]", NextSession, Some("Workspace")),
        gpui::KeyBinding::new("cmd-[", PrevSession, Some("Workspace")),
        gpui::KeyBinding::new("cmd-j", NextAttention, Some("Workspace")),
        gpui::KeyBinding::new("cmd-k", OpenPalette, Some("Workspace")),
        gpui::KeyBinding::new("ctrl-k", CommandMenu, Some("Workspace")),
        gpui::KeyBinding::new("cmd-o", SwitchProject, Some("Workspace")),
        gpui::KeyBinding::new("cmd-b", ToggleSidebar, Some("Workspace")),
        gpui::KeyBinding::new("cmd-d", ToggleDeck, Some("Workspace")),
        gpui::KeyBinding::new("cmd-0", ToggleRack, Some("Workspace")),
        gpui::KeyBinding::new("cmd-y", ForceAllowGate, Some("Workspace")),
        gpui::KeyBinding::new("cmd-\\", TogglePanel, Some("Workspace")),
        gpui::KeyBinding::new("ctrl-`", ToggleTerminal, Some("Workspace")),
        gpui::KeyBinding::new("cmd-,", OpenSettings, Some("Workspace")),
        gpui::KeyBinding::new("ctrl-r", ToggleThoughts, Some("Workspace")),
        gpui::KeyBinding::new("ctrl-e", ToggleTools, Some("Workspace")),
        gpui::KeyBinding::new("f1", Help, Some("Workspace")),
        gpui::KeyBinding::new("cmd-1", SwitchTo1, Some("Workspace")),
        gpui::KeyBinding::new("cmd-2", SwitchTo2, Some("Workspace")),
        gpui::KeyBinding::new("cmd-3", SwitchTo3, Some("Workspace")),
        gpui::KeyBinding::new("cmd-4", SwitchTo4, Some("Workspace")),
        gpui::KeyBinding::new("cmd-5", SwitchTo5, Some("Workspace")),
        gpui::KeyBinding::new("cmd-6", SwitchTo6, Some("Workspace")),
        gpui::KeyBinding::new("cmd-7", SwitchTo7, Some("Workspace")),
        gpui::KeyBinding::new("cmd-8", SwitchTo8, Some("Workspace")),
        gpui::KeyBinding::new("cmd-9", SwitchTo9, Some("Workspace")),
        gpui::KeyBinding::new("cmd-alt-2", ShowFiles, Some("Workspace")),
        gpui::KeyBinding::new("cmd-alt-3", ShowEditor, Some("Workspace")),
        gpui::KeyBinding::new("cmd-alt-4", ShowReview, Some("Workspace")),
        gpui::KeyBinding::new("cmd-alt-5", ShowTerminal, Some("Workspace")),
        gpui::KeyBinding::new("cmd-alt-6", ShowArtifacts, Some("Workspace")),
        gpui::KeyBinding::new("cmd-alt-7", ShowMcpHub, Some("Workspace")),
        gpui::KeyBinding::new("cmd-p", OpenFiles, Some("Workspace")),
        gpui::KeyBinding::new("cmd-s", SaveFile, Some("Workspace")),
        gpui::KeyBinding::new("cmd-s", SaveFile, Some("CordataEditor")),
        gpui::KeyBinding::new("cmd-shift-a", AttachFiles, Some("Workspace")),
        gpui::KeyBinding::new("cmd-m", Minimize, Some("Workspace")),
        gpui::KeyBinding::new("cmd-h", HideApp, None),
        gpui::KeyBinding::new("cmd-alt-h", HideOthers, None),
        gpui::KeyBinding::new("cmd-q", QuitApp, None),
    ]);
}

/// The macOS menu bar and the Dock icon's menu. Call after [`bind_keys`]: each item shows the key its action is bound to.
pub fn install_menus(cx: &mut App) {
    use gpui::{Menu, MenuItem, OsAction, SystemMenuType};
    use gpui_component::input::{Copy, Cut, Paste, Redo, SelectAll, Undo};
    cx.on_action(|_: &HideApp, cx| cx.hide());
    cx.on_action(|_: &HideOthers, cx| cx.hide_other_apps());
    cx.on_action(|_: &ShowAll, cx| cx.unhide_other_apps());
    cx.on_action(|_: &QuitApp, cx| {
        crate::tray::QUITTING.store(true, std::sync::atomic::Ordering::Relaxed);
        cx.quit();
    });
    cx.set_menus([
        Menu::new("Cordata").items([
            MenuItem::action("About Cordata", About),
            MenuItem::separator(),
            MenuItem::action("Settings…", OpenSettings),
            MenuItem::separator(),
            MenuItem::os_submenu("Services", SystemMenuType::Services),
            MenuItem::separator(),
            MenuItem::action("Hide Cordata", HideApp),
            MenuItem::action("Hide Others", HideOthers),
            MenuItem::action("Show All", ShowAll),
            MenuItem::separator(),
            MenuItem::action("Quit Cordata", QuitApp),
        ]),
        Menu::new("File").items([
            MenuItem::action("New Thread", NewSession),
            MenuItem::action("Open Project…", SwitchProject),
            MenuItem::separator(),
            MenuItem::action("Attach Files or Images…", AttachFiles),
            MenuItem::action("Open File…", OpenFiles),
            MenuItem::action("Save", SaveFile),
            MenuItem::separator(),
            MenuItem::action("Close", CloseSession),
        ]),
        Menu::new("Edit").items([
            MenuItem::os_action("Undo", Undo, OsAction::Undo),
            MenuItem::os_action("Redo", Redo, OsAction::Redo),
            MenuItem::separator(),
            MenuItem::os_action("Cut", Cut, OsAction::Cut),
            MenuItem::os_action("Copy", Copy, OsAction::Copy),
            MenuItem::os_action("Paste", Paste, OsAction::Paste),
            MenuItem::os_action("Select All", SelectAll, OsAction::SelectAll),
        ]),
        Menu::new("View").items([
            MenuItem::action("Command Palette", OpenPalette),
            MenuItem::separator(),
            MenuItem::action("Toggle Sidebar", ToggleSidebar),
            MenuItem::action("Toggle Panel", TogglePanel),
            MenuItem::action("Change Sheet / Transcript", ToggleDeck),
            MenuItem::action("Every Thread at Once", ToggleRack),
            MenuItem::action("Terminal", ToggleTerminal),
            MenuItem::separator(),
            MenuItem::action("Show Thinking", ToggleThoughts),
            MenuItem::action("Show Tool Output", ToggleTools),
        ]),
        Menu::new("Go").items([
            MenuItem::action("Next Thread", NextSession),
            MenuItem::action("Previous Thread", PrevSession),
            MenuItem::action("Thread That Needs You", NextAttention),
            MenuItem::separator(),
            MenuItem::action("Files", ShowFiles),
            MenuItem::action("Editor", ShowEditor),
            MenuItem::action("Review", ShowReview),
            MenuItem::action("Canvas", ShowArtifacts),
            MenuItem::action("Terminal", ShowTerminal),
            MenuItem::action("MCP Hub", ShowMcpHub),
            MenuItem::separator(),
            MenuItem::action("Focus Composer", FocusComposer),
        ]),
        Menu::new("Window").items([MenuItem::action("Minimize", Minimize), MenuItem::action("Zoom", Zoom)]),
        Menu::new("Help").items([MenuItem::action("Cordata Help", Help), MenuItem::action("Command Palette", OpenPalette)]),
    ]);
    cx.set_dock_menu(vec![
        MenuItem::action("New Thread", NewSession),
        MenuItem::action("Open Project…", SwitchProject),
        MenuItem::action("Settings…", OpenSettings),
    ]);
}

#[cfg(test)]
mod tests {
    use super::{age, should_notify};

    #[test]
    fn one_banner_per_absence() {
        assert!(should_notify(true, true, false, false));
        assert!(!should_notify(true, true, false, true), "a second approval while away is silent");
        assert!(!should_notify(false, true, false, false), "a held approval is not a new one");
        assert!(!should_notify(true, true, true, false), "no banner over a window in front");
        assert!(!should_notify(true, false, false, false), "the setting wins");
    }

    #[test]
    fn ages_read_at_a_glance() {
        assert_eq!(age(5.0), "now");
        assert_eq!(age(150.0), "2m");
        assert_eq!(age(7200.0), "2h");
        assert_eq!(age(3.0 * 86_400.0), "3d");
    }
}

#[cfg(test)]
mod window_tests {
    use super::{DeskMode, Workspace};
    use crate::session::Connection;
    use gpui::{AppContext as _, TestAppContext, VisualTestContext};

    fn offline() -> Connection {
        // No way to connect: every session says why and spawns nothing — the window's own behaviour, in isolation.
        Connection { transport: acp_core::BackendTransport::Native, problem: Some("offline for a test".into()) }
    }

    fn workspace(cx: &mut TestAppContext) -> (gpui::Entity<Workspace>, &mut VisualTestContext) {
        workspace_with(cx, super::Launch::default())
    }

    fn workspace_with(cx: &mut TestAppContext, launch: super::Launch) -> (gpui::Entity<Workspace>, &mut VisualTestContext) {
        // Never the person's own preferences file.
        let prefs_dir = std::env::temp_dir().join(format!("cordata-prefs-test-{}", std::process::id()));
        // SAFETY: set before any thread of this test reads it; every test sets the same value.
        unsafe { std::env::set_var("CORDATA_APP_PREFS_DIR", &prefs_dir) };
        cx.update(|cx| {
            gpui_component::init(cx);
            crate::theme::install(false, "2049", cx);
            cx.set_global(crate::prefs::Prefs::default());
            super::bind_keys(cx);
        });
        let (view, cx) = cx.add_window_view(|window, cx| Workspace::new(offline(), std::env::temp_dir(), launch, window, cx));
        cx.run_until_parked();
        (view, cx)
    }

    #[gpui::test]
    fn panes_follow_the_thread_and_a_folder_reuses_its_open_thread(cx: &mut TestAppContext) {
        let (view, cx) = workspace(cx);
        let a = view.read_with(cx, |this, cx| this.session.read(cx).cwd.clone());
        let b = std::env::temp_dir().join(format!("cordata-proj-b-{}", std::process::id()));
        std::fs::create_dir_all(&b).unwrap();
        let b = std::fs::canonicalize(b).unwrap();
        view.update_in(cx, |this, window, cx| this.switch_project(b.clone(), window, cx));
        cx.run_until_parked();
        let (cwd, threads) = view.read_with(cx, |this, _| (this.cwd.clone(), this.sessions.len()));
        assert_eq!((cwd, threads), (b.clone(), 2), "a new folder gets a new thread");
        // Pick the first thread (folder A): the tree, review and branch follow it.
        view.update(cx, |this, cx| {
            let first = this.sessions[0].clone();
            this.show(first, cx);
        });
        assert_eq!(view.read_with(cx, |this, _| this.cwd.clone()), a);
        // Back to B: its thread is still open, so no third one.
        view.update_in(cx, |this, window, cx| this.switch_project(b.clone(), window, cx));
        cx.run_until_parked();
        let (cwd, threads, shown_in) = view.read_with(cx, |this, cx| (this.cwd.clone(), this.sessions.len(), this.session.read(cx).cwd.clone()));
        assert_eq!((cwd, threads, shown_in), (b.clone(), 2, b));
    }

    #[gpui::test]
    fn an_earlier_conversation_opens_as_its_own_thread_and_leaves_the_shown_one_alone(cx: &mut TestAppContext) {
        let (view, cx) = workspace(cx);
        let before = view.read_with(cx, |this, cx| (this.sessions.len(), this.session.entity_id(), this.session.read(cx).cwd.clone()));
        view.update_in(cx, |this, window, cx| this.open_saved("19ab-000001".into(), window, cx));
        cx.run_until_parked();
        let after = view.read_with(cx, |this, cx| (this.sessions.len(), this.session.read(cx).cwd.clone()));
        assert_eq!((after.0, after.1), (before.0 + 1, before.2), "one more thread, in the same folder");
    }

    fn project(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("cordata-restore-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::canonicalize(dir).unwrap()
    }

    #[gpui::test]
    fn a_relaunch_brings_back_the_open_threads_in_order_and_shows_the_one_that_was_shown(cx: &mut TestAppContext) {
        let (a, b) = (project("a"), project("b"));
        let restore = crate::threads::Layout::new(
            Some("2-bb".into()),
            vec![crate::threads::Open { id: "1-aa".into(), cwd: a.clone() }, crate::threads::Open { id: "2-bb".into(), cwd: b.clone() }, crate::threads::Open { id: "3-cc".into(), cwd: a.clone() }],
        );
        let (view, cx) = workspace_with(cx, super::Launch { restore, ..Default::default() });
        let (ids, shown, cwd) = view.read_with(cx, |this, cx| {
            let ids: Vec<_> = this.sessions.iter().map(|s| s.read(cx).saved_id().map(str::to_owned)).collect();
            (ids, this.session.read(cx).saved_id().map(str::to_owned), this.cwd.clone())
        });
        assert_eq!(ids, vec![Some("1-aa".into()), Some("2-bb".into()), Some("3-cc".into())], "the old order");
        assert_eq!((shown.as_deref(), cwd), (Some("2-bb"), b), "the shown thread's folder is the window's");
    }

    #[gpui::test]
    fn no_saved_threads_means_one_fresh_thread_and_the_old_list_is_left_alone(cx: &mut TestAppContext) {
        let (view, cx) = workspace(cx);
        // Nothing to list yet: an empty list is not written over what the last run left.
        assert_eq!((view.read_with(cx, |this, _| this.sessions.len()), view.read_with(cx, |this, _| this.threads_armed)), (1, false));
    }

    #[gpui::test]
    fn a_folder_named_at_launch_is_shown_on_top_of_the_restored_threads(cx: &mut TestAppContext) {
        let a = project("named-a");
        let restore = crate::threads::Layout::new(Some("1-aa".into()), vec![crate::threads::Open { id: "1-aa".into(), cwd: a }]);
        let (view, cx) = workspace_with(cx, super::Launch { restore, open_folder: true, ..Default::default() });
        let named = std::fs::canonicalize(std::env::temp_dir()).unwrap();
        let (threads, shown) = view.read_with(cx, |this, cx| (this.sessions.len(), this.session.read(cx).cwd.clone()));
        assert_eq!((threads, shown), (2, named), "the restored thread, plus one in the folder that was named");
    }

    #[gpui::test]
    fn open_threads_are_written_when_they_change_and_not_before_they_exist_on_disk(cx: &mut TestAppContext) {
        let (view, cx) = workspace(cx);
        let listed = |view: &gpui::Entity<Workspace>, cx: &mut VisualTestContext| view.read_with(cx, |this, _| this.threads.last().open.iter().map(|o| o.id.clone()).collect::<Vec<_>>());
        // A conversation with no file on disk (a thread that has not finished a turn) is not listed.
        view.update_in(cx, |this, window, cx| this.open_saved("19ab-000001".into(), window, cx));
        assert!(listed(&view, cx).is_empty(), "nothing on disk, nothing to come back to");
        // Once its file exists, the next change lists it, and the thread shown is remembered.
        view.update(cx, |this, _| this.session_exists = |_, _| true);
        view.update_in(cx, |this, window, cx| this.open_saved("19ab-000002".into(), window, cx));
        assert_eq!(listed(&view, cx), vec!["19ab-000001".to_owned(), "19ab-000002".to_owned()]);
        assert_eq!(view.read_with(cx, |this, _| this.threads.last().active.clone()).as_deref(), Some("19ab-000002"));
        // Closing a thread takes it off the list.
        view.update(cx, |this, cx| this.close_session_at(1, cx));
        assert_eq!(listed(&view, cx), vec!["19ab-000002".to_owned()]);
    }

    #[gpui::test]
    fn with_reopen_turned_off_no_list_is_kept_and_turning_it_on_keeps_it_again(cx: &mut TestAppContext) {
        let (view, cx) = workspace(cx);
        view.update(cx, |this, _| {
            this.prefs.restore_threads = false;
            this.session_exists = |_, _| true;
        });
        view.update_in(cx, |this, window, cx| this.open_saved("19ab-000003".into(), window, cx));
        assert!(view.read_with(cx, |this, _| this.threads.last().is_empty()), "off: nothing is remembered");
        view.update(cx, |this, cx| {
            this.prefs.restore_threads = true;
            this.persist_threads(cx);
        });
        assert_eq!(view.read_with(cx, |this, _| this.threads.last().open.len()), 1, "on again: the open thread is listed");
    }

    #[gpui::test]
    fn a_slash_opens_the_command_menu_and_enter_runs_the_choice(cx: &mut TestAppContext) {
        let (view, cx) = workspace(cx);
        // Menus open only while the session can take input (as in the terminal): pretend it connected.
        view.update(cx, |this, cx| this.session.update(cx, |s, _| s.view.conn = cordata_view::app::ConnState::Connected));
        cx.simulate_input("/hel");
        cx.run_until_parked();
        let first = view.read_with(cx, |this, cx| this.session.read(cx).view.menu.as_ref().and_then(|m| m.current()).map(|i| i.label.clone()));
        assert_eq!(first.as_deref(), Some("/help"), "typing narrows the commands");
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        let (menu, help_shown) = view.read_with(cx, |this, cx| {
            let v = &this.session.read(cx).view;
            (v.menu.is_some(), v.messages.iter().any(|m| matches!(m, cordata_view::app::Message::Help)))
        });
        assert!(!menu && help_shown, "Enter runs /help locally (never sent to a model)");
    }

    #[gpui::test]
    fn arrow_keys_navigate_the_menu(cx: &mut TestAppContext) {
        let (view, cx) = workspace(cx);
        view.update(cx, |this, cx| this.session.update(cx, |s, _| s.view.conn = cordata_view::app::ConnState::Connected));
        cx.simulate_input("/");
        cx.run_until_parked();
        let selected_0 = view.read_with(cx, |this, cx| this.session.read(cx).view.menu.as_ref().map(|m| m.selected));
        assert_eq!(selected_0, Some(0));
        cx.simulate_keystrokes("down");
        cx.run_until_parked();
        let selected_1 = view.read_with(cx, |this, cx| this.session.read(cx).view.menu.as_ref().map(|m| m.selected));
        assert_eq!(selected_1, Some(1));
        cx.simulate_keystrokes("down");
        cx.run_until_parked();
        let selected_2 = view.read_with(cx, |this, cx| this.session.read(cx).view.menu.as_ref().map(|m| m.selected));
        assert_eq!(selected_2, Some(2));
        cx.simulate_keystrokes("up");
        cx.run_until_parked();
        let selected_back = view.read_with(cx, |this, cx| this.session.read(cx).view.menu.as_ref().map(|m| m.selected));
        assert_eq!(selected_back, Some(1));
        cx.simulate_keystrokes("tab");
        cx.run_until_parked();
        let text = view.read_with(cx, |this, cx| this.composer.read(cx).value().to_string());
        assert_eq!(text, "/thinking ");
        assert!(view.read_with(cx, |this, cx| this.session.read(cx).view.menu.is_none()));
    }

    #[gpui::test]
    fn history_navigates_with_up_and_down_arrows(cx: &mut TestAppContext) {
        let (view, cx) = workspace(cx);
        view.update(cx, |this, cx| {
            this.session.update(cx, |s, _| {
                s.view.conn = cordata_view::app::ConnState::Connected;
                s.view.submit_text("first prompt");
                s.view.submit_text("second prompt");
            });
        });
        cx.simulate_keystrokes("up");
        cx.run_until_parked();
        let text1 = view.read_with(cx, |this, cx| this.composer.read(cx).value().to_string());
        assert_eq!(text1, "second prompt");
        cx.simulate_keystrokes("up");
        cx.run_until_parked();
        let text2 = view.read_with(cx, |this, cx| this.composer.read(cx).value().to_string());
        assert_eq!(text2, "first prompt");
        cx.simulate_keystrokes("down");
        cx.run_until_parked();
        let text3 = view.read_with(cx, |this, cx| this.composer.read(cx).value().to_string());
        assert_eq!(text3, "second prompt");
    }

    #[gpui::test]
    fn sessions_open_switch_and_close_from_the_keyboard(cx: &mut TestAppContext) {
        let (view, cx) = workspace(cx);
        cx.simulate_keystrokes("cmd-n");
        cx.run_until_parked();
        let (count, shown_is_last) = view.read_with(cx, |this, _| (this.sessions.len(), this.session.entity_id() == this.sessions[1].entity_id()));
        assert_eq!(count, 2, "⌘N opens a session");
        assert!(shown_is_last, "…and shows it");
        cx.simulate_keystrokes("cmd-]");
        cx.run_until_parked();
        assert!(view.read_with(cx, |this, _| this.session.entity_id() == this.sessions[0].entity_id()), "⌘] switches (and wraps)");
        cx.simulate_keystrokes("cmd-w");
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |this, _| this.sessions.len()), 1, "⌘W closes the shown session");
        cx.simulate_keystrokes("cmd-w");
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |this, _| this.sessions.len()), 1, "…but never the last one");
    }

    #[gpui::test]
    fn cmd_j_goes_to_the_thread_that_finished_unseen_and_showing_it_clears_done(cx: &mut TestAppContext) {
        use cordata_view::app::TabState;
        let (view, cx) = workspace(cx);
        cx.simulate_keystrokes("cmd-n");
        cx.run_until_parked();
        // The first thread finished while the second was shown.
        view.update(cx, |this, cx| {
            this.sessions[0].update(cx, |s, _| {
                s.view.conn = cordata_view::app::ConnState::Connected;
                s.view.unseen = true;
            });
        });
        assert_eq!(view.read_with(cx, |this, cx| this.sessions[0].read(cx).view.tab_state()), TabState::Done);
        cx.simulate_keystrokes("cmd-j");
        cx.run_until_parked();
        let (shown_first, state) = view.read_with(cx, |this, cx| {
            (this.session.entity_id() == this.sessions[0].entity_id(), this.sessions[0].read(cx).view.tab_state())
        });
        assert!(shown_first, "⌘J shows the thread that wants you");
        assert_eq!(state, TabState::Idle, "…and once shown it is seen");
        cx.simulate_keystrokes("cmd-j");
        cx.run_until_parked();
        assert!(view.read_with(cx, |this, _| this.session.entity_id() == this.sessions[0].entity_id()), "nothing else waits: ⌘J stays");
    }

    #[gpui::test]
    fn cmd_j_opens_the_terminal_when_its_agent_waits_but_an_approval_comes_first(cx: &mut TestAppContext) {
        let (view, cx) = workspace(cx);
        let waiting = crate::detect::Detection { id: "claude".into(), name: "Claude Code".into(), state: crate::detect::State::Blocked };
        view.update(cx, |this, cx| {
            this.set_panel(false, cx);
            this.terminal.detection = Some(waiting);
        });
        cx.simulate_keystrokes("cmd-j");
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |this, _| (this.panel_open, this.dock_tab)), (true, super::DockTab::Terminal), "⌘J shows the waiting agent");

        // A thread waiting on an approval outranks it.
        cx.simulate_keystrokes("cmd-n");
        cx.run_until_parked();
        let rx = view.update(cx, |this, cx| {
            let first = this.sessions[0].clone();
            let (tx, rx) = tokio::sync::oneshot::channel();
            let info = acp_core::PermissionInfo { tool_call_id: "c".into(), tool_name: "run_command".into(), title: "Run run_command".into(), kind: acp_core::ToolKind::Execute, raw_input: None, always_label: None, reason: None };
            first.update(cx, |s, _| {
                s.view.conn = cordata_view::app::ConnState::Connected;
                s.view.on_event(acp_core::AppEvent::PermissionRequested { info, decision: tx });
            });
            this.set_panel(false, cx);
            rx
        });
        cx.simulate_keystrokes("cmd-j");
        cx.run_until_parked();
        let (shown_first, panel) = view.read_with(cx, |this, _| (this.session.entity_id() == this.sessions[0].entity_id(), this.panel_open));
        assert!(shown_first && !panel, "the approval first, the terminal next time");
        drop(rx);
    }

    fn ask(view: &gpui::Entity<Workspace>, cx: &mut VisualTestContext, tool: &str) -> tokio::sync::oneshot::Receiver<acp_core::ApprovalDecision> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let info = acp_core::PermissionInfo {
            tool_call_id: "call-1".into(),
            tool_name: tool.into(),
            title: format!("Run {tool}"),
            kind: acp_core::ToolKind::Execute,
            raw_input: Some(serde_json::json!({"command": "make deploy"})),
            always_label: Some("shell(make deploy)".into()),
            reason: None,
        };
        view.update(cx, |this, cx| {
            this.session.update(cx, |s, cx| {
                s.view.conn = cordata_view::app::ConnState::Connected;
                s.view.on_event(acp_core::AppEvent::PermissionRequested { info, decision: tx });
                cx.notify();
            })
        });
        cx.run_until_parked();
        rx
    }

    #[gpui::test]
    fn an_approval_card_answers_to_its_keys(cx: &mut TestAppContext) {
        let (view, cx) = workspace(cx);
        let mut rx = ask(&view, cx, "shell");
        cx.simulate_keystrokes("a");
        cx.run_until_parked();
        assert_eq!(rx.try_recv().ok(), Some(acp_core::ApprovalDecision::Allow), "a allows");
        assert!(view.read_with(cx, |this, app| this.composer.read(app).value().is_empty()), "…and is not typed into the composer");

        let mut rx = ask(&view, cx, "shell");
        cx.simulate_keystrokes("w");
        cx.simulate_input("not on a Friday");
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        assert_eq!(rx.try_recv().ok(), Some(acp_core::ApprovalDecision::Say("not on a Friday".into())), "w then Enter rejects in your own words");

        let mut rx = ask(&view, cx, "shell");
        cx.simulate_keystrokes("r");
        cx.run_until_parked();
        assert_eq!(rx.try_recv().ok(), Some(acp_core::ApprovalDecision::Reject), "r rejects");
    }

    #[gpui::test]
    fn an_offline_session_says_why_instead_of_an_empty_window(cx: &mut TestAppContext) {
        let (view, cx) = workspace(cx);
        let said = view.read_with(cx, |this, cx| this.session.read(cx).view.messages.iter().any(|m| matches!(m, cordata_view::app::Message::Error(e) if e.contains("offline for a test"))));
        assert!(said);
        let _ = cx.new(|_| ()); // the context stays usable
    }

    #[gpui::test]
    fn side_panel_tabs_and_keys(cx: &mut TestAppContext) {
        let (view, cx) = workspace(cx);
        assert_eq!(view.read_with(cx, |this, _| this.dock_tab), super::DockTab::Files);
        for (keys, tab) in [
            ("cmd-alt-2", super::DockTab::Files),
            ("cmd-alt-3", super::DockTab::Editor),
            ("cmd-alt-4", super::DockTab::Review),
            ("cmd-p", super::DockTab::Files),
        ] {
            cx.simulate_keystrokes(keys);
            cx.run_until_parked();
            assert_eq!(view.read_with(cx, |this, _| this.dock_tab), tab, "{keys}");
            assert!(view.read_with(cx, |this, _| this.panel_open), "{keys} opens the panel");
        }
        cx.simulate_keystrokes("cmd-\\");
        cx.run_until_parked();
        assert!(!view.read_with(cx, |this, _| this.panel_open), "⌘\\ hides the panel");
        assert_eq!(view.read_with(cx, |this, _| this.prefs.panel_open), Some(false), "…and the choice is kept");
        cx.simulate_keystrokes("ctrl-`");
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |this, _| (this.panel_open, this.dock_tab)), (true, super::DockTab::Terminal), "⌃` shows the terminal");
        cx.simulate_keystrokes("ctrl-`");
        cx.run_until_parked();
        assert!(!view.read_with(cx, |this, _| this.panel_open), "…and again hides it");
    }

    #[gpui::test]
    fn the_about_box_opens_from_the_menu_action_and_esc_closes_it(cx: &mut TestAppContext) {
        let (view, cx) = workspace(cx);
        cx.dispatch_action(super::About);
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |this, _| this.overlays.clone()), vec![super::Overlay::About], "Cordata ▸ About opens the about box");
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        assert!(view.read_with(cx, |this, _| this.overlays.is_empty()), "esc closes it");
    }

    #[gpui::test]
    fn the_menu_bar_names_every_menu(cx: &mut TestAppContext) {
        cx.update(|cx| {
            super::bind_keys(cx);
            super::install_menus(cx);
        });
        let names: Vec<String> = cx.update(|cx| cx.get_menus()).unwrap_or_default().iter().map(|m| m.name.to_string()).collect();
        assert_eq!(names, ["Cordata", "File", "Edit", "View", "Go", "Window", "Help"]);
    }

    #[gpui::test]
    fn settings_search_narrows_the_sections_and_jumps_to_a_match(cx: &mut TestAppContext) {
        let (view, cx) = workspace(cx);
        cx.simulate_keystrokes("cmd-,");
        cx.run_until_parked();
        view.update_in(cx, |this, window, cx| this.settings_search.update(cx, |s, cx| s.focus(window, cx)));
        cx.simulate_input("theme");
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |this, _| this.settings_section), super::settings_modal::Section::Appearance, "“theme” finds the Appearance section");
        assert!(super::settings_modal::Section::Appearance.matches("dark theme") && !super::settings_modal::Section::Account.matches("theme"));
    }

    #[gpui::test]
    fn overlays_open_from_keys_and_esc_closes_the_top_one_first(cx: &mut TestAppContext) {
        let (view, cx) = workspace(cx);
        cx.simulate_keystrokes("cmd-,");
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |this, _| this.overlays.clone()), vec![super::Overlay::Settings]);
        view.update_in(cx, |this, window, cx| this.open_overlay(super::Overlay::Palette, window, cx));
        cx.run_until_parked();
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |this, _| this.overlays.clone()), vec![super::Overlay::Settings], "esc closes the palette, not settings");
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        assert!(view.read_with(cx, |this, _| this.overlays.is_empty()));
        cx.simulate_keystrokes("cmd-o");
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |this, _| this.overlays.clone()), vec![super::Overlay::Picker], "⌘O opens the project picker");
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        cx.simulate_keystrokes("cmd-k");
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |this, _| this.overlays.clone()), vec![super::Overlay::Palette], "⌘K opens the palette");
    }

    #[gpui::test]
    fn the_palette_filters_and_runs_with_the_keyboard(cx: &mut TestAppContext) {
        let (view, cx) = workspace(cx);
        cx.simulate_keystrokes("cmd-k");
        cx.run_until_parked();
        let total = view.read_with(cx, |this, cx| this.palette_matches(cx).len());
        assert!(total > 1);
        cx.simulate_keystrokes("down");
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |this, _| this.palette_ix), 1, "down arrow navigates palette");
        cx.simulate_keystrokes("up");
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |this, _| this.palette_ix), 0, "up arrow navigates palette back");
        cx.simulate_input("toggle side");
        cx.run_until_parked();
        let first = view.read_with(cx, |this, cx| this.palette_matches(cx).first().map(|i| i.label.clone()));
        assert_eq!(first.as_deref(), Some("Toggle side panel"));
        let before = view.read_with(cx, |this, _| this.panel_open);
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |this, _| this.panel_open), !before, "↵ runs the first match");
        assert!(view.read_with(cx, |this, _| this.overlays.is_empty()), "…and closes the palette");
    }

    #[gpui::test]
    fn the_picker_opens_a_pasted_path_as_the_project(cx: &mut TestAppContext) {
        let (view, cx) = workspace(cx);
        let dir = tempfile_dir().join("picked");
        std::fs::create_dir_all(&dir).unwrap();
        cx.simulate_keystrokes("cmd-o");
        cx.run_until_parked();
        cx.simulate_input(&dir.display().to_string());
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        let (cwd, recent, sessions) = view.read_with(cx, |this, _| (this.cwd.clone(), this.prefs.recents.first().map(|r| r.path.clone()), this.sessions.len()));
        assert_eq!(cwd, dir.canonicalize().unwrap());
        assert_eq!(recent, Some(cwd.clone()), "first in the recents");
        assert_eq!(sessions, 2, "a conversation opens in the new project");
        assert!(view.read_with(cx, |this, _| this.overlays.is_empty()));
    }

    #[gpui::test]
    fn attachments_stage_and_refuse_what_they_must(cx: &mut TestAppContext) {
        let (view, cx) = workspace(cx);
        let dir = tempfile_dir();
        std::fs::write(dir.join("notes.md"), "hello").unwrap();
        std::fs::write(dir.join(".env"), "KEY=1").unwrap();
        view.update(cx, |this, cx| this.add_paths(&[dir.join("notes.md"), dir.join(".env")], cx));
        let (count, note) = view.read_with(cx, |this, _| (this.staged.len(), this.attach_note.clone()));
        assert_eq!(count, 1, "the text file is staged");
        assert!(note.unwrap_or_default().contains("credentials"), "the .env is refused, and says why");
        // A running turn: attachments wait instead of being dropped.
        view.update_in(cx, |this, window, cx| {
            this.session.update(cx, |s, _| {
                s.view.conn = cordata_view::app::ConnState::Connected;
                s.view.turn_active = true;
            });
            this.composer.update(cx, |c, cx| c.set_value("look at this", window, cx));
            this.send_composer(window, cx);
        });
        let (count, note) = view.read_with(cx, |this, _| (this.staged.len(), this.attach_note.clone()));
        assert_eq!(count, 1, "still staged while the turn runs");
        assert!(note.unwrap_or_default().contains("wait for this run"), "…and the window says so");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[gpui::test]
    fn a_review_sends_the_diff_as_an_attachment(cx: &mut TestAppContext) {
        let (view, cx) = workspace(cx);
        let dir = tempfile_dir().join("repo");
        std::fs::create_dir_all(&dir).unwrap();
        let g = |args: &[&str]| super::git_ops::run("git", &[&["-c", "user.email=t@t", "-c", "user.name=t"], args].concat(), &dir, None, std::time::Duration::from_secs(20)).unwrap();
        g(&["init", "-q", "-b", "main"]);
        std::fs::write(dir.join("a.rs"), "fn a() {}\n").unwrap();
        g(&["add", "-A"]);
        g(&["commit", "-qm", "init"]);
        std::fs::write(dir.join("a.rs"), "fn a() { todo!() }\n").unwrap();
        view.update_in(cx, |this, window, cx| {
            this.cwd = dir.clone();
            this.session.update(cx, |s, _| s.view.conn = cordata_view::app::ConnState::Connected);
            this.request_review(false, window, cx);
        });
        cx.run_until_parked();
        let (staged, said) = view.read_with(cx, |this, cx| {
            let said = this.session.read(cx).view.messages.iter().any(|m| matches!(m, cordata_view::app::Message::User(t) if t.contains("Review the uncommitted changes") && t.contains("[attached: uncommitted.diff]")));
            (this.staged.len(), said)
        });
        assert!(said, "the review prompt went out, naming its attachment");
        assert_eq!(staged, 0, "the diff left with it");
    }

    #[gpui::test]
    fn conversations_switch_with_cmd_digits(cx: &mut TestAppContext) {
        let (view, cx) = workspace(cx);
        cx.simulate_keystrokes("cmd-n");
        cx.run_until_parked();
        cx.simulate_keystrokes("cmd-1");
        cx.run_until_parked();
        assert!(view.read_with(cx, |this, _| this.session.entity_id() == this.sessions[0].entity_id()));
        cx.simulate_keystrokes("cmd-2");
        cx.run_until_parked();
        assert!(view.read_with(cx, |this, _| this.session.entity_id() == this.sessions[1].entity_id()));
        cx.simulate_keystrokes("cmd-9");
        cx.run_until_parked();
        assert!(view.read_with(cx, |this, _| this.session.entity_id() == this.sessions[1].entity_id()), "a slot with no conversation does nothing");
    }

    #[gpui::test]
    fn enter_allows_and_esc_denies_an_approval(cx: &mut TestAppContext) {
        let (view, cx) = workspace(cx);
        let mut rx = ask(&view, cx, "shell");
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        assert_eq!(rx.try_recv().ok(), Some(acp_core::ApprovalDecision::Allow), "↵ on an empty composer allows once");
        let mut rx = ask(&view, cx, "shell");
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        assert_eq!(rx.try_recv().ok(), Some(acp_core::ApprovalDecision::Reject), "esc denies");
    }

    #[gpui::test]
    fn cmd_w_closes_editor_tab_first(cx: &mut TestAppContext) {
        let (view, cx) = workspace(cx);
        view.update(cx, |this, _| {
            // The dock starts closed; ⌘W closes a buffer only when the editor is in view.
            this.panel_open = true;
            this.dock_tab = super::DockTab::Editor;
        });
        let dir = tempfile_dir();
        let file_path = dir.join("foo.rs");
        std::fs::write(&file_path, "fn foo() {}\n").unwrap();
        let rel = "foo.rs".to_string();
        let buffer_state = cx.update(|window, cx| cx.new(|cx| gpui_component::input::EditorState::new(window, cx)));
        view.update(cx, |this, _| {
            this.add_test_buffer(&rel, file_path.clone(), buffer_state);
        });

        assert_eq!(view.read_with(cx, |this, _| this.files.buffers.len()), 1);
        cx.simulate_keystrokes("cmd-w");
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |this, _| this.files.buffers.len()), 0, "⌘W closed the editor buffer");
        assert_eq!(view.read_with(cx, |this, _| this.sessions.len()), 1, "session remains open");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[gpui::test]
    fn sidebar_toggles_and_starter_cards_prepare_the_composer(cx: &mut TestAppContext) {
        let (view, cx) = workspace(cx);
        assert!(view.read_with(cx, |this, _| this.sidebar_open), "sidebar starts open");
        cx.simulate_keystrokes("cmd-b");
        cx.run_until_parked();
        assert!(!view.read_with(cx, |this, _| this.sidebar_open), "⌘B hides the sidebar");
        assert_eq!(view.read_with(cx, |this, _| this.prefs.sidebar_open), Some(false), "…and remembers it");
        cx.simulate_keystrokes("cmd-b");
        cx.run_until_parked();
        assert!(view.read_with(cx, |this, _| this.sidebar_open), "⌘B shows the sidebar again");

        view.update_in(cx, |this, window, cx| {
            this.apply_starter("Explore key modules", false, window, cx);
        });
        assert_eq!(view.read_with(cx, |this, app| this.composer.read(app).value().to_string()), "Explore key modules");
    }

    #[gpui::test]
    fn high_risk_gate_blocks_reflex_enter_until_token_confirmed_or_cmd_y(cx: &mut TestAppContext) {
        let (view, cx) = workspace(cx);
        let (tx, mut rx) = tokio::sync::oneshot::channel();
        let info = acp_core::PermissionInfo {
            tool_call_id: "risk-1".into(),
            tool_name: "run_command".into(),
            title: "Run run_command with network access".into(),
            kind: acp_core::ToolKind::Execute,
            raw_input: Some(serde_json::json!({ "cmd": "curl -X POST https://api.example.com/hook", "network": true })),
            always_label: None,
            reason: Some("network access requested".into()),
        };
        view.update(cx, |this, cx| {
            this.session.update(cx, |s, cx| {
                s.view.conn = cordata_view::app::ConnState::Connected;
                s.view.on_event(acp_core::AppEvent::PermissionRequested { info, decision: tx });
                cx.notify();
            });
        });
        cx.run_until_parked();
        assert!(view.read_with(cx, |this, app| this.active_gate_locked(app)), "high-risk gate starts locked");

        // Reflex Enter and plain `a` must NOT approve a locked high-risk gate.
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        assert!(rx.try_recv().is_err(), "reflex Enter is blocked while locked");
        assert!(view.read_with(cx, |this, _| this.gate_hint.is_some()), "inline hint explains why");

        cx.simulate_keystrokes("a");
        cx.run_until_parked();
        assert!(rx.try_recv().is_err(), "reflex 'a' is also blocked while locked");

        // Confirming the risk token unlocks Enter, or ⌘Y allows explicitly.
        cx.simulate_keystrokes("cmd-y");
        cx.run_until_parked();
        assert_eq!(rx.try_recv().ok(), Some(acp_core::ApprovalDecision::Allow), "⌘Y unlocks and approves high-risk gate");
    }

    #[gpui::test]
    fn change_sheet_cmd_d_and_triage_rack_cmd_0_batch_gate(cx: &mut TestAppContext) {
        let (view, cx) = workspace(cx);
        cx.simulate_keystrokes("cmd-n");
        cx.run_until_parked();

        // Both threads wait on the identical safe check command.
        let mut rxs = Vec::new();
        view.update(cx, |this, cx| {
            for (ix, ses) in this.sessions.iter().enumerate() {
                let (tx, rx) = tokio::sync::oneshot::channel();
                rxs.push(rx);
                let info = acp_core::PermissionInfo {
                    tool_call_id: format!("chk-{ix}"),
                    tool_name: "run_command".into(),
                    title: "Run run_command".into(),
                    kind: acp_core::ToolKind::Execute,
                    raw_input: Some(serde_json::json!({ "cmd": "cargo test" })),
                    always_label: Some("Always allow run_command (cargo) this session".into()),
                    reason: None,
                };
                ses.update(cx, |s, cx| {
                    s.view.conn = cordata_view::app::ConnState::Connected;
                    s.view.messages.push(cordata_view::app::Message::User(format!("Task {}", ix + 1)));
                    s.view.on_event(acp_core::AppEvent::PermissionRequested { info, decision: tx });
                    cx.notify();
                });
            }
        });
        cx.run_until_parked();

        assert_eq!(view.read_with(cx, |this, _| this.desk_mode), DeskMode::Transcript);
        cx.simulate_keystrokes("cmd-d");
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |this, _| this.desk_mode), DeskMode::ChangeSheet, "⌘D switches to proof-first Change Sheet");

        cx.simulate_keystrokes("cmd-0");
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |this, _| this.desk_mode), DeskMode::Rack, "⌘0 opens Supervisory Triage Rack");

        // While the Rack is open, plain `a` does not silently approve a hidden single-session gate, and `esc` closes the Rack.
        cx.simulate_keystrokes("a");
        cx.run_until_parked();
        for rx in &mut rxs {
            assert!(rx.try_recv().is_err(), "hidden single-session gate is not answered from the Rack");
        }
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |this, _| this.desk_mode), DeskMode::Transcript, "esc exits the Triage Rack");
        for rx in &mut rxs {
            assert!(rx.try_recv().is_err(), "esc exited the Rack without rejecting pending gates");
        }

        cx.simulate_keystrokes("cmd-0");
        cx.run_until_parked();
        let sig = view.read_with(cx, |this, app| {
            let cwd = this.sessions[0].read(app).cwd.clone();
            format!("{}|run_command|{{\"cmd\":\"cargo test\"}}|", cwd.display())
        });
        // Batch-approving by canonical signature answers both matching threads in a single decision.
        view.update(cx, |this, cx| {
            this.answer_batch_gate(&sig, acp_core::ApprovalDecision::Allow, cx);
        });
        cx.run_until_parked();
        for rx in &mut rxs {
            assert_eq!(rx.try_recv().ok(), Some(acp_core::ApprovalDecision::Allow));
        }
    }

    #[gpui::test]
    fn control_room_graph_mode_node_selection_and_inspector(cx: &mut TestAppContext) {
        let (view, cx) = workspace(cx);
        cx.simulate_keystrokes("cmd-n");
        cx.run_until_parked();

        // Slot 1: Supervisor, Slot 2: Invariant Probe
        view.update(cx, |this, cx| {
            this.sessions[0].update(cx, |s, cx| {
                s.view.conn = cordata_view::app::ConnState::Connected;
                s.view.messages.push(cordata_view::app::Message::User("Supervisor planning wave 1".into()));
                cx.notify();
            });
            this.sessions[1].update(cx, |s, cx| {
                s.view.conn = cordata_view::app::ConnState::Connected;
                s.view.messages.push(cordata_view::app::Message::User("invariant probe: check invariants".into()));
                cx.notify();
            });
        });
        cx.run_until_parked();

        cx.simulate_keystrokes("cmd-0");
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |this, _| this.desk_mode), DeskMode::Rack);
        assert_eq!(view.read_with(cx, |this, _| this.rack_view_mode), cordata_view::flight::RackViewMode::Field, "opens on the field");
        assert_eq!(view.read_with(cx, |this, _| this.rack_selected_slot), None);

        // Select Node Slot 2 (Probe)
        view.update(cx, |this, cx| {
            this.rack_selected_slot = Some(2);
            cx.notify();
        });
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |this, _| this.rack_selected_slot), Some(2));

        // Ground and back
        view.update(cx, |this, cx| {
            this.rack_view_mode = cordata_view::flight::RackViewMode::Ground;
            cx.notify();
        });
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |this, _| this.rack_view_mode), cordata_view::flight::RackViewMode::Ground);

        // The field: draws both threads as nodes, zooms from the buttons, and a reset puts the camera back.
        view.update(cx, |this, cx| {
            this.rack_view_mode = cordata_view::flight::RackViewMode::Field;
            cx.notify();
        });
        cx.run_until_parked();
        view.update(cx, |this, cx| this.field_zoom_step(0.5, cx));
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |this, _| cordata_view::field::altitude(this.field_cam.zoom)), cordata_view::field::Altitude::Fleet);
        view.update(cx, |this, cx| this.field_reset(cx));
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |this, _| this.field_cam), cordata_view::field::Camera::default());
    }

    #[gpui::test]
    fn tape_scrubber_reverts_post_turn_edits_and_forks_clean_thread(cx: &mut TestAppContext) {
        let (view, cx) = workspace(cx);
        let dir = tempfile_dir();
        let file_a = dir.join("lib.rs");
        std::fs::write(&file_a, "pub fn answer() -> i32 { 42 }\n").unwrap();

        // Simulate Turn 1 (good edit) followed by Turn 2 (bad edit to lib.rs + unwanted extra.rs + edit to pre-existing empty mod.rs).
        std::fs::write(&file_a, "pub fn answer() -> i32 { 0 }\n").unwrap();
        let file_b = dir.join("extra.rs");
        std::fs::write(&file_b, "// unwanted\n").unwrap();
        let file_empty = dir.join("mod.rs");
        std::fs::write(&file_empty, "pub mod extra;\n").unwrap();

        view.update(cx, |this, cx| {
            this.cwd = dir.clone();
            this.session.update(cx, |s, _| {
                s.cwd = dir.clone();
                s.view.conn = cordata_view::app::ConnState::Connected;
                s.view.messages = vec![
                    cordata_view::app::Message::User("Turn 1: implement answer()".into()),
                    cordata_view::app::Message::Tool(cordata_view::app::ToolCard {
                        id: "t1".into(),
                        name: "edit_file".into(),
                        kind: acp_core::ToolKind::Edit,
                        status: acp_core::ToolStatus::Completed,
                        raw_input: None,
                        content: "edited lib.rs".into(),
                        diffs: vec![acp_core::FileDiff {
                            path: "lib.rs".into(),
                            old_text: Some("pub fn answer() -> i32 { 0 }\n".into()),
                            new_text: "pub fn answer() -> i32 { 42 }\n".into(),
                        }],
                    }),
                    cordata_view::app::Message::Agent("Implemented answer = 42.".into()),
                    cordata_view::app::Message::User("Turn 2: experimental refactor".into()),
                    cordata_view::app::Message::Tool(cordata_view::app::ToolCard {
                        id: "t2".into(),
                        name: "edit_file".into(),
                        kind: acp_core::ToolKind::Edit,
                        status: acp_core::ToolStatus::Completed,
                        raw_input: None,
                        content: "edited lib.rs and extra.rs".into(),
                        diffs: vec![
                            acp_core::FileDiff {
                                path: "lib.rs".into(),
                                old_text: Some("pub fn answer() -> i32 { 42 }\n".into()),
                                new_text: "pub fn answer() -> i32 { 0 }\n".into(),
                            },
                            acp_core::FileDiff {
                                path: "extra.rs".into(),
                                old_text: None,
                                new_text: "// unwanted\n".into(),
                            },
                            acp_core::FileDiff {
                                path: "mod.rs".into(),
                                old_text: Some(String::new()),
                                new_text: "pub mod extra;\n".into(),
                            },
                        ],
                    }),
                    cordata_view::app::Message::Agent("Broke answer.".into()),
                ];
            });
        });

        let reverted = view.update_in(cx, |this, window, cx| this.revert_edits_after_turn(0, window, cx));
        assert_eq!(reverted, 3, "rolled back all 3 post-turn-1 file diffs");
        assert_eq!(std::fs::read_to_string(&file_a).unwrap(), "pub fn answer() -> i32 { 42 }\n", "Turn 1 state restored");
        assert!(!file_b.exists(), "newly created file in Turn 2 removed");
        assert_eq!(std::fs::read_to_string(&file_empty).unwrap(), "", "pre-existing empty file restored without deletion");

        // Forking from Turn 1 opens a clean 2nd session containing only Turn 1's messages.
        view.update_in(cx, |this, window, cx| this.fork_from_turn(0, window, cx));
        cx.run_until_parked();
        let (session_count, forked_turns) = view.read_with(cx, |this, app| {
            let turns = cordata_view::desk::turns(&this.session.read(app).view.messages).len();
            (this.sessions.len(), turns)
        });
        assert_eq!(session_count, 2, "forked into a new open thread");
        assert_eq!(forked_turns, 1, "forked thread holds only Turn 1, dropping Turn 2");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[gpui::test]
    fn empty_thread_change_sheet_esc_send_new_and_slash_review(cx: &mut TestAppContext) {
        let (view, cx) = workspace(cx);
        view.update(cx, |this, cx| {
            this.session.update(cx, |s, _| {
                s.view.conn = cordata_view::app::ConnState::Connected;
                s.view.messages.clear();
            });
            let patch = "diff --git a/src/a.rs b/src/a.rs\n--- a/src/a.rs\n+++ b/src/a.rs\n@@ -1 +1,2 @@\n fn a() {}\n+fn b() {}\n";
            let files = cordata_view::review::parse(patch);
            this.review.display = super::review::SharedFile::from_diffs(&files);
            this.review.files = files;
        });
        cx.run_until_parked();

        // 1. ⌘D opens Change Sheet even on a brand-new empty thread, and esc returns to Transcript.
        cx.simulate_keystrokes("cmd-d");
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |this, _| this.desk_mode), DeskMode::ChangeSheet, "⌘D opens Change Sheet on empty thread");
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |this, _| this.desk_mode), DeskMode::Transcript, "esc exits Change Sheet back to Transcript");

        // 2. Typing `/review` opens the Proof-First Change Sheet instead of printing a placeholder message.
        cx.simulate_input("/review");
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |this, _| this.desk_mode), DeskMode::ChangeSheet, "/review opens Change Sheet");

        // 3. Sending a prompt while in Change Sheet switches back to Transcript so streaming replies are visible.
        view.update_in(cx, |this, window, cx| {
            this.composer.update(cx, |c, cx| c.set_value("fix src/a.rs", window, cx));
            this.send_composer(window, cx);
        });
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |this, _| this.desk_mode), DeskMode::Transcript, "sending a prompt switches to Transcript");

        // 4. Opening a new thread (⌘N) while in Supervisory Rack resets to Transcript for the new thread.
        cx.simulate_keystrokes("cmd-0");
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |this, _| this.desk_mode), DeskMode::Rack);
        cx.simulate_keystrokes("cmd-n");
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |this, _| this.desk_mode), DeskMode::Transcript, "⌘N exits Rack and shows the new thread");
    }

    #[gpui::test]
    #[ignore]
    fn bench_gpui_window_render_sheet_and_rack(cx: &mut TestAppContext) {
        let (view, cx) = workspace(cx);
        for _ in 0..3 {
            cx.simulate_keystrokes("cmd-n");
            cx.run_until_parked();
        }
        let mut patch = String::new();
        for f in 0..22 {
            patch.push_str(&format!(
                "diff --git a/src/file_{f}.rs b/src/file_{f}.rs\n--- a/src/file_{f}.rs\n+++ b/src/file_{f}.rs\n@@ -1,30 +1,110 @@\n"
            ));
            for l in 0..30 {
                patch.push_str(&format!(" fn ctx_{f}_{l}(s: &Workspace) -> usize {{ s.sessions.len() }}\n"));
            }
            for l in 0..80 {
                patch.push_str(&format!("+    let val_{l} = fast_frame_metric(s, {l});\n"));
            }
        }
        let parsed = cordata_view::review::parse(&patch);
        let shared = super::review::SharedFile::from_diffs(&parsed);
        view.update(cx, |this, cx| {
            let first = this.sessions[0].clone();
            this.show(first, cx);
            this.session.update(cx, |s, cx| {
                s.view.conn = cordata_view::app::ConnState::Connected;
                for t in 0..25 {
                    s.view.messages.push(cordata_view::app::Message::User(format!("Turn {t}: optimize rendering path")));
                    s.view.messages.push(cordata_view::app::Message::Agent(format!("Updated rendering path in turn {t}.")));
                }
                s.version += 1;
                cx.notify();
            });
            this.review.files = parsed;
            this.review.display = shared;
            cx.notify();
        });
        cx.run_until_parked();

        let measure_mode = |mode: DeskMode, view: &gpui::Entity<Workspace>, cx: &mut VisualTestContext| -> f64 {
            view.update(cx, |this, cx| {
                this.desk_mode = mode;
                cx.notify();
            });
            cx.run_until_parked();
            let mut samples: Vec<f64> = (0..21)
                .map(|_| {
                    let s = std::time::Instant::now();
                    view.update(cx, |_, cx| cx.notify());
                    cx.run_until_parked();
                    s.elapsed().as_secs_f64() * 1e3
                })
                .collect();
            samples.sort_by(f64::total_cmp);
            samples[10]
        };

        let transcript_ms = measure_mode(DeskMode::Transcript, &view, cx);
        let sheet_ms = measure_mode(DeskMode::ChangeSheet, &view, cx);
        let rack_ms = measure_mode(DeskMode::Rack, &view, cx);
        println!(
            "BENCH gpui window layout+draw (25 turns, 22 files / 2420 diff lines, 4 threads): transcript {transcript_ms:.2}ms | change_sheet {sheet_ms:.2}ms | triage_rack {rack_ms:.2}ms | 120Hz budget 8.33ms"
        );
        assert!(sheet_ms < 8.33, "ChangeSheet GPUI window frame ({sheet_ms:.2}ms) exceeded 8.33ms 120Hz budget");
        assert!(rack_ms < 8.33, "TriageRack GPUI window frame ({rack_ms:.2}ms) exceeded 8.33ms 120Hz budget");
    }

    fn tempfile_dir() -> std::path::PathBuf {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("cordata-test-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.canonicalize().unwrap()
    }
}
