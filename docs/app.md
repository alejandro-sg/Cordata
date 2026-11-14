# Cordata.app — the native macOS window

A Rust/GPUI app that runs the same engine as the terminal client, in its own process (`acp-core`'s native engine). There is
no backend, daemon or hub to start. Apple Silicon, macOS 13 or later, Command Line Tools only (GPUI builds with runtime
shaders). Personal use: the bundle is signed locally, not notarized, and has no auto-update.

## Build and run

```bash
make app-run                       # run from the checkout
make app INSTALL=1                 # build, sign, self-test and copy to ~/Applications/Cordata.app
make app-test                      # headless tests (shared state, palette, app)
make app-check                     # clippy -D warnings, cargo-deny, and the gpui-pre snapshot proof (needs network once)
```

The first release build compiles GPUI and takes several minutes; the release profile (`app/Cargo.toml`: thin LTO, one
codegen unit, stripped symbols) makes later ones slower and the binary smaller, 59.9 MB to 45.5 MB. `make app` signs ad hoc by default, so macOS treats each
rebuild as a new app and forgets privacy grants; set `SIGN_IDENTITY` to a self-signed "Code Signing" certificate to keep
them. A friend running a copy you built has to open it with right-click → Open the first time.

Keys are never stored in the app's preferences: it reads the same environment variables and Keychain items as the
terminal client (`Settings → Account`, service `cordata-*-api-key`). Window preferences are in
`~/Library/Application Support/Cordata/prefs.toml`.

## What it has

Conversation rail and transcript, approval cards with the exact command or diff, composer with `/` and `@` menus,
command palette (⌘K), project picker (⌘O), settings (⌘,: launch, appearance, shortcuts, your API key), and a side panel
with Files, Editor, Review (per-hunk keep/revert, git and pull requests) and Terminal. Both clients share one
conversation reducer. Like the terminal client, sessions are saved and resumable (`/history`, `/resume`, `/forget`),
edits and commands ask first unless your `chat.allow` rules say otherwise, and there is no undo or goal loop in the chat (`/run` hands a goal to the orchestrator). The
window has three views of the same threads (`⌘0` canvas, `⌘1` transcript, `⌘D` change sheet); Settings → Default workspace
chooses the one it opens in.

The rail keeps three different things apart. **Threads** are the conversations open in the window, in the order you
opened them (a row never jumps); **Earlier** lists this project's saved conversations, and a click resumes one as a thread
of its own; **Other checkouts** (folded) are the repository's other git worktrees, made by Claude Code, agy or you. A
checkout is a place with files, not a conversation: it is named by its branch, and a click moves the window to that
folder. The header says `needs you`, `working` or `unread` only when there is something to say.

Open threads survive a relaunch (Settings → General → Reopen threads on launch; on by default). The conversations
themselves are the engine's session files (`~/.cordata/sessions/<project>/<id>.json`, written after every turn,
owner-only, secrets masked, newest 50 per project). The window keeps only a list of which were open, in order, and which
was shown, in `~/Library/Application Support/Cordata/threads.toml` — ids and folders, never text, at most 12, rewritten
only when the list changes. On launch each entry is checked (existing folder, well-formed id, session file present) and
resumed; a thread that never finished a turn has no file and is not restored. A folder named on the command line is
shown on top of them; a prompt, `--selftest` or the picker start clean. Turned off, nothing is remembered and the file
is deleted. Losing `threads.toml` costs nothing: everything is still under **Earlier**.

Two copies of the app do not fight over it. The window that holds `threads.lock` (an advisory lock that goes with the
process, so a crash leaves nothing stale) owns the list; a second copy starts clean, never writes it, and takes it over
the next time its own threads change after the first has quit. Below that, a conversation has one writer: the engine
holds `<id>.lock` beside the session file while it writes it, so resuming a conversation that is open in another window,
thread or terminal is refused with a note instead of two engines appending to one file.

Every thread in the rail shows what it is doing; one that finished while you were elsewhere is marked **Done** until
you look, and ⌘J (Go → Thread That Needs You) goes to the next one waiting on an approval, then the next Done one. A
notification says when a thread finishes or needs you while the window is in the back (Settings → General).

### The field (⌘0)
<!-- covers: app/crates/cordata-view/src/field.rs, app/crates/cordata-view/src/runs.rs, app/crates/cordata-app/src/workspace/field.rs @ b3a342a -->

The canvas opens on the **field**: the project's open threads as nodes, this project's newest orchestrated runs beside
them, a goal bar on top (the composer), an attention rail on the right and a tape of the newest run along the bottom.
There is no thread list or transcript here; `⌘1` goes back to them, and clicking a picked node opens its thread.

- **Only what asks for attention is drawn**: a thread with an open approval, one that is working, one that collides with
  another on a component, one ready to review (changed files, no gate, no collision, no failed check), or the one you
  picked. The rest is counted in one line ("N idle threads · show").
- **Detail follows zoom** (`⌘`-scroll, or − and + in the corner): marks and names; then what each is doing and the
  components it stands on, with its approval and Allow or Reject; then its last actions. Scroll or drag pans.
- **A plan waiting for you**: a run started with `--review-plan` shows as "plan awaits you" with each step's expected
  behavior under its name (its tooltip too), and an Approve and build button where a held run has Resume. The menu bar
  item offers the same. Nothing is built until you press it.
- **The rail** holds what needs you (the exact command and Allow or Reject; a high-risk call says to open the thread),
  what is ready to review (files, +/−, check result; a click opens the change sheet) and the components two threads both
  write. Each fact is in one place.
- **Runs** come from `cordata-run status --json` and `show --json` (found in `CORDATA_RUN_BIN`, `PATH`, or this
  checkout's release build) and are read every few seconds while the field is open: the plan drawn as a graph (steps that wait on the same step
  side by side under it, joined by cords; a cord draws itself once, when a read shows its step has started), the worker
  that last ran each step, handoffs (why a worker stopped and who took over), and the run's own state transitions on the
  tape. **Hold at next safe point** writes the pause line described in `docs/orchestrator.md`; a held run offers
  **Resume**, **Resume on** a worker family, and **Resume with the bar's text as direction**. Steps of a run never raise
  approvals in the window: workers approve inside their own scope.
- The tape records; it does not rewind. Workers' messages on the run bus are not shown: the bus is in memory and closed
  to everything but the run's workers.

The models are pure and tested: `cordata-view`'s `field` (layout, camera, which threads matter), `runs` (what the CLI
printed) and `tray` (below). Why it is laid out this way: threads and runs are supervised by exception (Sheridan's
supervisory control), awareness has three levels (Endsley 1995: perceive, understand, project), and a person handed back
control after long automation is worst placed to take it (Bainbridge 1983), so approvals show the exact command and a
handoff shows what the next worker is given. No usability study has been run; the only usage data is the window's own
`~/.cordata/ux-events.jsonl`, from one user.

### The menu bar item

While the app runs, a mark sits in the menu bar: `C` quiet, `(•` working, `C● 2` when two things wait on you. Its menu
lists what needs you (thread, exact command with credential-shaped spans masked, Allow and Reject), runs (Hold or
Resume), up to six threads (working first), then Open Cordata, Show the field, New thread, Settings and Quit. Allow is
offered only for a call the engine does not rate high risk, and a click answers only the call the menu showed. Closing
the window hides the app so the item stays; ⌘Q quits. Turn it off in Settings → Show in the menu bar; it is never
created in tests or the packaged self-test (`CORDATA_APP_TRAY=off` also skips it).

### The look

A quiet instrument: "The Rope" layout. The rope bar across the top displays open threads as nodes on one line (a thread needing you sits in an ochre gap). The page is a manuscript column (680 px) with a 120 px left margin cord where every event hangs with mono marginalia. Your prompt is an anchor; agent prose is a pitch in 17 px Charter; tool calls are quiet lines off the cord. The next line composer has no box: a caret line at the end of the cord with `›` hanging in the margin, a hairline above, and right-aligned footer metadata. Logbook (⌘B) and Tool sheet (⌘\) slide in over the page as drawers with tight low shadows.

Typography uses native macOS system fonts exclusively: Charter (editorial prose, brief, gate question), SF Pro (`.SystemUIFont`) for prompts, rope bar and controls, and SF Mono / Menlo for machine facts. The gate is the one standout: editorial pull-quote with 22 px Charter question, ruled mono block, and two-column "If allowed / If denied" ledger.

Nothing moves on a timer: movements (draw, beat, tug, arrive, close, slide, tick, stack in `motion.rs`) are started by events, run once, and stop under Reduce Motion. Icons are Cordata's own set on a 16 grid with 1.25 stroke. The rules, colours and files: `app/assets/brand/README.md`.

### Driving the window from a shell

```bash
cordata-app status            # the shown thread: title, state (working, blocked, done, idle), session id
cordata-app ls                # every open thread
cordata-app read              # what the dock terminal shows, as text (and the shell's exit code once it ended)
cordata-app do new            # also: close, next, prev, switch N, attention, focus, prompt TEXT
cordata-app do run 'cargo test'   # types a command into the dock terminal and presses Return
```

### The dock terminal

Terminal (⌃`, ⌘⌥5) is a real terminal: your login shell in the project on a PTY, parsed by `alacritty_terminal` (the
emulator Zed's terminal uses), so full-screen programs — `claude`, `vim`, `htop`, `less` — work, in 256 colours and
true colour, sized to the pane. It is your shell with your rights, like Terminal.app: the agent's sandbox does not apply,
and the agent never types into it (`cordata-app do run` is you, from another shell). Keys go to the program — Escape,
⌃R, ⌃K and ⌃E included — except ⌘ shortcuts, which stay the window's; ⌘V pastes (bracketed, so a pasted newline does
not run anything early); the wheel scrolls the history, or acts as arrow keys in a full-screen program without mouse
support. A program may copy to the clipboard (OSC 52) but not read it. The strip shows the program's title and, after
the shell ends, its exit code and Restart.

**Agents in the terminal.** When a coding agent runs there — Claude Code, Codex, Gemini CLI, OpenCode, aider, or
Cordata's own terminal client — the strip names it and says whether it is working, **blocked** (waiting for your
answer) or idle. The agent is recognised by the terminal's foreground process (through node, bun, python or a shell
wrapper), its state by rules over the title and the bottom rows, and recent output as working when no rule decides.
A blocked agent sends a notification when the terminal is not in front of you (Settings → General → approvals), and
⌘J goes to it after any thread waiting on an approval. `cordata-app status` and `read` report it as `terminal_agent` /
`agent`. The rules are data (`app/crates/cordata-app/src/detect.toml`); add or override an agent in
`~/Library/Application Support/Cordata/detect.toml` — a file that does not parse is flagged in the strip and the
built-in rules stay.

One JSON line each way over `~/Library/Application Support/Cordata/cordata-app.sock` (owner-only; the window checks
the connecting process's user, not just the file's mode). A prompt sent this way still asks before any edit or command,
and the chat sandbox cannot reach the socket. `CORDATA_APP_CONTROL=off` keeps the window from listening.

## Crates (`app/crates`)
<!-- covers: app/Cargo.toml @ b3a342a -->

- `cordata-view`: the state the terminal and the app share: the reducer (`App::on_event`), slash commands, input, menus,
  the settings schema and editor, run board, the field, run feed and menu bar models, and the secret-masking table (`redact.rs`, tested against
  `proto/fixtures/redact_cases.json`).
- `cordata-theme`: colour roles with WCAG contrast tests; the terminal's palette reads it.
- `cordata-app`: the window: `session.rs` (acp-core on a tokio runtime → the reducer → effects), `workspace.rs` and its
  modules (`field.rs`, `menubar.rs`, ...), `transcript.rs`, `platform.rs`, `tray.rs` (the native status item).

The UI thread only draws; the agent's events reach it over channels a task drains.

## Dependencies

GPUI comes from `gpui-pre =0.3.9`, a republication of Zed commit `93d7456`, with `gpui-component =0.7.3`, both exactly
pinned with a committed lockfile. `app/scripts/verify-gpui-snapshot.sh` checks every shipped file of the gpui-pre crates
against Zed (only three labelled facade edits differ) and `app/deny.toml` bans a network stack from the graph. Zed's
GPL editor crates are never copied.
