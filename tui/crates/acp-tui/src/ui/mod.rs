pub mod diff;
pub mod footer;
mod help;
pub mod hud;
pub(crate) mod input;
mod markdown;
pub(crate) mod menu;
pub(crate) mod panels;
pub mod text;
mod tool_card;
pub mod transcript;

use acp_core::SessionInfo;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

use crate::app::App;

/// What one frame draws: the live region (as a buffer) and where the text
/// cursor goes within it.
pub struct LiveFrame {
    pub buffer: Buffer,
    pub cursor: Option<(u16, u16)>,
}

/// The banner written once when the session connects: who, where, on what model, and the keys that matter.
///
/// ```text
///  ⌜ CORDATA ⌝  ░▒▓  abcdef12
///  ▌ ~/proj  ⎇ main
///  ▌ kimi-k2.7-code · medium
///  ╴ /  commands   @  files   !  shell   Ctrl+K  palette   ?  help
/// ```
pub fn welcome_lines(info: &SessionInfo, width: u16) -> Vec<Line<'static>> {
    let dim = Style::new().fg(Color::DarkGray);
    let rail = Style::new().fg(Color::Cyan);
    let session: String = info.session_id.chars().take(8).collect();
    let cwd = footer::short_path(&info.cwd);

    let mut title = vec![Span::raw(" ")];
    // The literal word, not the letter-spaced one the overlays use: scripts and the pty tests wait for it.
    title.extend(hud::bracketed("CORDATA", Color::White));
    title.push(Span::styled("  ░▒▓  ", Style::new().fg(Color::Cyan)));
    title.push(Span::styled(session, Style::new().fg(Color::Cyan)));

    let mut place = vec![Span::styled(" ▌ ", rail), Span::styled(cwd, Style::new().fg(Color::Gray))];
    if let Some(branch) = crate::extras::git_branch(std::path::Path::new(&info.cwd)) {
        place.push(Span::styled(format!("  ⎇ {branch}"), Style::new().fg(Color::Gray)));
    }

    let model = Some(info.model.clone()).filter(|name| !name.is_empty());
    let keys = ["/  commands", "@  files", "!  shell", "Ctrl+K  palette", "?  help"].join("   ");

    let mut lines = vec![Line::from(title), Line::from(place)];
    if let Some(model) = model {
        lines.push(Line::from(vec![Span::styled(" ▌ ", rail), Span::styled(model, Style::new().fg(Color::Magenta))]));
    }
    lines.push(Line::from(vec![
        Span::styled(" ╴ ", dim),
        Span::styled(text::truncate(&keys, (width as usize).saturating_sub(4)), dim),
    ]));
    lines
}

/// Draw the live region: reply tail / running tool cards, then the prompt box (or
/// an approval / picker panel in its place), then the footer. It is only as tall
/// as it needs to be, up to `max_rows`.
pub fn render_live(app: &App, width: u16, max_rows: u16) -> LiveFrame {
    let width = width.max(20);
    let footer_height = 1u16;
    let panel = panels::current(app, width);
    let bottom_height = match &panel {
        Some(panel) => panels::height(panel, max_rows.saturating_sub(footer_height + 1)),
        None => input::height_for(app, width).min(max_rows.saturating_sub(footer_height).max(3)),
    };
    // Pinned just above the prompt box, so they are never the rows that get cut when the terminal is short.
    let mut pinned = input::queue_lines(app, width);
    if panel.is_none() {
        pinned.extend(menu::lines(app, width));
    }
    let available = max_rows.saturating_sub(bottom_height + footer_height + pinned.len() as u16) as usize;

    let mut content = transcript::live_lines(app, width);
    if content.len() > available {
        // Only the newest rows fit; the rest is still reachable once it settles into scrollback.
        content.drain(..content.len() - available);
    }
    content.extend(pinned);

    let total = content.len() as u16 + bottom_height + footer_height;
    let mut buffer = Buffer::empty(Rect::new(0, 0, width, total));
    for (y, line) in content.iter().enumerate() {
        buffer.set_line(0, y as u16, line, width);
    }
    let bottom = Rect::new(0, content.len() as u16, width, bottom_height);
    let cursor = match panel {
        Some(panel) => {
            panels::render(&mut buffer, bottom, panel);
            None
        }
        None => input::render(&mut buffer, bottom, app),
    };
    buffer.set_line(0, total - 1, &footer::line(app, width), width);
    LiveFrame { buffer, cursor }
}

#[cfg(test)]
mod tests {
    use acp_core::{AppEvent, PermissionInfo, SessionInfo, ToolCallInfo, ToolKind, ToolStatus};

    use super::*;
    use crate::app::Message;
    use crate::term::testing::{emulator, transcript};
    use crate::term::{InlineTerm, buffer_to_ansi_rows, lines_to_ansi};

    /// A terminal of `w`×`h` that the UI draws into exactly as `main` does.
    /// `show` returns everything the user could see by scrolling: scrollback
    /// followed by the screen.
    struct Screen {
        term: InlineTerm<Vec<u8>>,
        parser: vt100::Parser,
        w: u16,
        h: u16,
    }

    impl Screen {
        fn new(w: u16, h: u16) -> Self {
            Self {
                term: InlineTerm::new(Vec::new()),
                parser: emulator(h, w),
                w,
                h,
            }
        }

        fn show(&mut self, app: &mut App) -> String {
            let commits = transcript::take_commits(app, self.w);
            let live = render_live(app, self.w, self.h);
            self.term
                .draw(
                    &lines_to_ansi(&commits, self.w),
                    &buffer_to_ansi_rows(&live.buffer),
                    live.cursor,
                )
                .unwrap();
            self.parser.process(&self.term.take_output());
            transcript(&mut self.parser).join("\n")
        }
    }

    fn screen(app: &mut App, w: u16, h: u16) -> String {
        Screen::new(w, h).show(app)
    }

    fn connected() -> App {
        let mut app = crate::overlay::new_app();
        app.on_event(AppEvent::Connected(SessionInfo { session_id: "abcdef12-3456".into(), cwd: "/proj".into(), model: "m1".into(), transport: "native".into() }));
        app
    }

    #[test]
    fn the_banner_and_footer_show_session_and_model() {
        let mut app = connected();
        let screen = screen(&mut app, 100, 20);
        for needle in ["abcdef12", "/proj", "m1", "? help"] {
            assert!(screen.contains(needle), "missing {needle:?} in\n{screen}");
        }
    }

    #[test]
    fn chat_shows_user_agent_and_tool_card() {
        let mut app = connected();
        app.messages.push(Message::User("hi there".into()));
        app.on_event(AppEvent::ToolCallStarted(ToolCallInfo {
            id: "t".into(),
            name: "shell".into(),
            kind: ToolKind::Execute,
            status: ToolStatus::Completed,
            raw_input: Some(serde_json::json!({"command": "echo hi"})),
            content: "hi".into(),
            diffs: vec![],
        }));
        app.on_event(AppEvent::AgentText("all done".into()));
        let screen = screen(&mut app, 80, 24);
        for needle in [
            "❯ hi there",
            "╭",
            "shell · execute",
            "$ echo hi",
            "all done",
        ] {
            assert!(screen.contains(needle), "missing {needle:?} in\n{screen}");
        }
    }

    /// `CORDATA_DUMP_DIR=… cargo test dump_inline -- --ignored`: the inline views as ANSI, for looking at.
    #[test]
    fn any_approval_can_be_refused_with_a_reason() {
        use acp_core::ApprovalDecision::Say;
        use cordata_view::key::{KeyCode, KeyEvent, KeyModifiers};
        let mut app = connected();
        let mut rx = ask(&mut app, "t1");
        let shown = screen(&mut app, 110, 30);
        assert!(shown.contains("Reject, saying why"), "{shown}");
        for code in [KeyCode::Char('w'), KeyCode::Char('n'), KeyCode::Char('o'), KeyCode::Enter] {
            app.on_key(KeyEvent::new(code, KeyModifiers::NONE));
        }
        assert_eq!(rx.try_recv().unwrap(), Say("no".into()));
    }

    #[test]
    fn reasoning_is_a_live_tail_while_streaming_then_one_line_then_expandable() {
        let mut app = connected();
        let mut term = Screen::new(60, 20);
        app.turn_active = true;
        let long = (1..=60)
            .map(|i| format!("word{i}"))
            .collect::<Vec<_>>()
            .join(" ");
        app.on_event(AppEvent::AgentThought(long));
        let live = term.show(&mut app);
        assert!(
            live.contains("· thinking…") && live.contains("word60"),
            "live tail shows the latest words:\n{live}"
        );
        assert!(!live.contains("word1 "), "…not the whole reasoning");

        app.on_event(AppEvent::AgentText("answer".into()));
        let done = term.show(&mut app);
        assert!(done.contains("reasoned (60 words)"), "{done}");
        assert!(!done.contains("word30"), "collapsed once the answer starts");
        assert!(
            !done.contains("thinking…"),
            "the live tail is gone, not left behind:\n{done}"
        );

        // Expanding applies to reasoning that settles afterwards.
        app.on_event(AppEvent::TurnComplete(acp_core::StopReason::EndTurn));
        app.thoughts_expanded = true;
        app.messages.push(Message::User("again".into()));
        app.turn_active = true;
        let second = (1..=40)
            .map(|i| format!("alpha{i}"))
            .collect::<Vec<_>>()
            .join(" ");
        app.on_event(AppEvent::AgentThought(second));
        app.on_event(AppEvent::AgentText("second answer".into()));
        assert!(
            term.show(&mut app).contains("alpha20"),
            "Ctrl+R shows new reasoning in full"
        );
    }

    #[test]
    fn replies_render_markdown_not_raw_markers() {
        let mut app = connected();
        app.on_event(AppEvent::AgentText(
            "# Result\n- run `cargo test`\n- **done**".into(),
        ));
        let screen = screen(&mut app, 60, 20);
        assert!(
            screen.contains("Result")
                && screen.contains("• run cargo test")
                && screen.contains("• done"),
            "{screen}"
        );
        assert!(
            !screen.contains("**") && !screen.contains('`') && !screen.contains("# Result"),
            "{screen}"
        );
    }

    #[test]
    fn the_slash_menu_and_the_queue_sit_above_the_prompt() {
        let mut app = connected();
        app.input.set_text("/c");
        app.refresh_menu();
        let shown = screen(&mut app, 90, 24);
        for needle in ["▸ /close", "/copy", "↑↓ choose · tab complete · enter run", "❯ /c"] {
            assert!(shown.contains(needle), "missing {needle:?} in\n{shown}");
        }
        let rows: Vec<&str> = shown.lines().collect();
        let menu = rows.iter().position(|r| r.contains("▸ /close")).unwrap();
        let prompt = rows.iter().position(|r| r.contains("❯ /c")).unwrap();
        assert!(menu < prompt, "the menu is above the box:\n{shown}");
        eprintln!("{shown}");

        let mut busy = connected();
        busy.turn_active = true;
        busy.queue.push_back("then run the tests".into());
        busy.queue.push_back("and commit".into());
        let shown = screen(&mut busy, 90, 24);
        assert!(shown.contains("⏵ next  then run the tests") && shown.contains("working — enter queues"), "{shown}");
        eprintln!("{shown}");
    }

    #[test]
    fn a_suggested_next_prompt_shows_as_grey_text_and_tab_takes_it() {
        let mut app = connected();
        app.suggestion = Some("run the tests and show me the result".into());
        let shown = screen(&mut app, 90, 20);
        assert!(shown.contains("❯ run the tests and show me the result") && shown.contains("tab ⇥"), "{shown}");
        app.on_key(cordata_view::key::KeyEvent::from(cordata_view::key::KeyCode::Tab));
        assert_eq!(app.input.text(), "run the tests and show me the result");
        assert!(app.suggestion.is_none());
        // Typing instead throws it away.
        let mut other = connected();
        other.suggestion = Some("commit".into());
        other.on_key(cordata_view::key::KeyEvent::from(cordata_view::key::KeyCode::Char('x')));
        assert!(other.suggestion.is_none() && other.input.text() == "x");
    }

    #[test]
    fn a_diff_is_coloured_by_line_kind() {
        let mut app = connected();
        app.on_local(crate::app::LocalEvent::ShellDone {
            command: "git diff".into(),
            outcome: crate::shell::Outcome { output: "diff --git a/x b/x\n@@ -1 +1 @@\n-old\n+new\n".into(), code: Some(0), timed_out: false, cut: false },
            share: false,
            as_diff: true,
        });
        let shown = screen(&mut app, 80, 20);
        assert!(shown.contains("-old") && shown.contains("+new") && !shown.contains("$ git diff"), "{shown}");
        assert!(app.shell_context.is_empty(), "/diff is for the person, not sent to the agent");

    }

    #[test]
    fn help_is_a_transcript_block_listing_keys_and_commands() {
        let mut app = connected();
        app.messages.push(Message::Help);
        let screen = screen(&mut app, 110, 60);
        for needle in [
            "Keys",
            "Esc / Ctrl+C",
            "/thinking",
            "Scroll with your terminal",
        ] {
            assert!(screen.contains(needle), "missing {needle:?} in\n{screen}");
        }
    }

    #[test]
    fn the_hint_line_shows_elapsed_time_and_tool_calls_while_working() {
        let mut app = connected();
        app.turn_active = true;
        app.turn_started = Some(std::time::Instant::now());
        app.turn_tools = 2;
        let screen = screen(&mut app, 110, 20);
        assert!(
            screen.contains("working 0s · 2 tool calls · Esc cancels"),
            "{screen}"
        );
    }

    #[test]
    fn the_approval_box_replaces_the_prompt_box() {
        let mut app = connected();
        let (tx, _rx) = tokio::sync::oneshot::channel();
        app.on_event(AppEvent::PermissionRequested {
            info: PermissionInfo {
                tool_call_id: "t".into(),
                tool_name: "shell".into(),
                title: "Run shell".into(),
                kind: ToolKind::Execute,
                raw_input: Some(serde_json::json!({"command": "rm -rf /tmp/x"})),
                always_label: None,
            },
            decision: tx,
        });
        let screen = screen(&mut app, 80, 24);
        for needle in ["rm -rf /tmp/x", "Allow", "Reject"] {
            assert!(screen.contains(needle), "missing {needle:?} in\n{screen}");
        }
        assert!(
            !screen.contains("❯ "),
            "no prompt box while asking:\n{screen}"
        );
    }

    #[test]
    fn the_approval_box_offers_always_allow_only_when_the_server_does() {
        let ask = |label: Option<&str>| {
            let mut app = connected();
            let (tx, _rx) = tokio::sync::oneshot::channel();
            app.on_event(AppEvent::PermissionRequested {
                info: PermissionInfo {
                    tool_call_id: "t".into(),
                    tool_name: "shell".into(),
                    title: "Run shell".into(),
                    kind: ToolKind::Execute,
                    raw_input: Some(serde_json::json!({"command": "git status"})),
                    always_label: label.map(Into::into),
                },
                decision: tx,
            });
            screen(&mut app, 80, 24)
        };
        let with = ask(Some("Always allow shell(git status)"));
        assert!(
            with.contains("Always allow shell(git status)") && with.contains(" s "),
            "{with}"
        );
        assert!(!ask(None).contains("Always allow"));
    }

    #[test]
    fn a_long_reply_is_ordinary_scrollback_with_one_prompt_box_left_on_screen() {
        let mut app = connected();
        let mut term = Screen::new(60, 12);
        app.messages.push(Message::User("write a lot".into()));
        app.turn_active = true;
        let reply: String = (1..=100).map(|i| format!("line {i}\n")).collect();
        // Stream it in a few chunks, as the model would.
        for chunk in reply.as_bytes().chunks(97) {
            app.on_event(AppEvent::AgentText(String::from_utf8_lossy(chunk).into()));
            term.show(&mut app);
        }
        app.on_event(AppEvent::TurnComplete(acp_core::StopReason::EndTurn));
        let all = term.show(&mut app);
        for i in 1..=100 {
            assert!(
                all.lines()
                    .any(|row| row.trim_end().ends_with(&format!("line {i}"))),
                "line {i} reachable by scrolling up"
            );
        }
        assert_eq!(
            all.matches("Ask anything").count(),
            1,
            "one prompt box:\n{all}"
        );
        // …and the reply is not duplicated by the live region.
        assert_eq!(all.matches("line 50").count(), 1, "no duplicated rows");
    }

    #[test]
    fn clear_resets_the_commit_cursor() {
        let mut app = connected();
        app.messages.push(Message::User("one".into()));
        let mut term = Screen::new(60, 20);
        term.show(&mut app);
        assert!(app.committed > 0);
        app.input.set_text("/clear");
        let effect = app.on_key(cordata_view::key::KeyEvent::from(
            cordata_view::key::KeyCode::Enter,
        ));
        assert_eq!(effect, Some(crate::app::Effect::ClearScreen));
        assert_eq!((app.committed, app.committed_rows), (0, 0));
    }

    fn edit_card(app: &mut App, id: &str) {
        app.on_event(AppEvent::ToolCallStarted(ToolCallInfo {
            id: id.into(),
            name: "editor".into(),
            kind: ToolKind::Edit,
            status: ToolStatus::Pending,
            raw_input: None,
            content: String::new(),
            diffs: vec![],
        }));
        let old: String = (1..=40).map(|i| format!("line {i}\n")).collect();
        let new = old.replace("line 20\n", "LINE TWENTY\n");
        app.on_event(AppEvent::ToolCallUpdated(acp_core::ToolCallPatch {
            id: id.into(),
            raw_input: Some(serde_json::json!({"command": "str_replace", "path": "src/a.rs"})),
            diffs: Some(vec![acp_core::FileDiff {
                path: "/proj/src/a.rs".into(),
                old_text: Some(old),
                new_text: new,
            }]),
            ..Default::default()
        }));
    }

    fn ask(app: &mut App, id: &str) -> tokio::sync::oneshot::Receiver<acp_core::ApprovalDecision> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        app.on_event(AppEvent::PermissionRequested {
            info: PermissionInfo {
                tool_call_id: id.into(),
                tool_name: "editor".into(),
                title: "Run editor".into(),
                kind: ToolKind::Edit,
                raw_input: Some(serde_json::json!({"command": "str_replace", "path": "src/a.rs", "old_str": "line 20", "new_str": "LINE TWENTY"})),
                always_label: None,
            },
            decision: tx,
        });
        rx
    }

    #[test]
    fn the_approval_panel_shows_the_diff_before_the_edit_runs() {
        let mut app = connected();
        app.messages.push(Message::User("rename it".into()));
        edit_card(&mut app, "e1");
        let _rx = ask(&mut app, "e1");
        let shown = screen(&mut app, 90, 30);
        for needle in [
            "Permission required",
            "✎ src/a.rs",
            "+1 −1",
            "  20 − line 20",
            "  20 + LINE TWENTY",
            " a ",
            " r ",
            " d ",
        ] {
            assert!(shown.contains(needle), "missing {needle:?} in\n{shown}");
        }
        assert!(
            !shown.contains("\"old_str\""),
            "the raw JSON is replaced by the diff:\n{shown}"
        );
    }

    #[test]
    fn on_a_short_terminal_the_diff_is_cut_but_the_keys_stay() {
        let mut app = connected();
        edit_card(&mut app, "e1");
        // A long multi-hunk change in a 14-row terminal.
        if let Some(Message::Tool(card)) = app.messages.last_mut() {
            let old: String = (1..=80).map(|i| format!("l{i}\n")).collect();
            let new: String = (1..=80)
                .map(|i| {
                    if i % 9 == 0 {
                        format!("CHANGED {i}\n")
                    } else {
                        format!("l{i}\n")
                    }
                })
                .collect();
            card.diffs[0] = acp_core::FileDiff {
                path: "/proj/b.txt".into(),
                old_text: Some(old),
                new_text: new,
            };
        }
        let _rx = ask(&mut app, "e1");
        let shown = screen(&mut app, 80, 14);
        assert!(
            shown.contains(" a ") && shown.contains("Allow") && shown.contains("Reject"),
            "keys visible:\n{shown}"
        );
        assert!(shown.contains("more rows"), "the cut is marked:\n{shown}");
        app.on_key(cordata_view::key::KeyEvent::from(
            cordata_view::key::KeyCode::Char('d'),
        ));
        assert!(app.diff_expanded, "d toggles the whole diff");
    }

    #[test]
    fn the_footer_shows_a_tab_strip_with_sessions_needing_approval() {
        let mut app = connected();
        app.tabs = vec![
            crate::app::TabBadge {
                number: 1,
                active: true,
                state: crate::app::TabState::Idle,
            },
            crate::app::TabBadge {
                number: 2,
                active: false,
                state: crate::app::TabState::NeedsApproval,
            },
            crate::app::TabBadge {
                number: 3,
                active: false,
                state: crate::app::TabState::Working,
            },
        ];
        let shown = screen(&mut app, 100, 12);
        let footer = shown.lines().last().unwrap();
        assert!(
            footer.contains("[1]") && footer.contains(" 2⚠ ") && footer.contains(" 3● "),
            "{footer}"
        );
    }

}
