# Auto mode and a quiet transcript

Status: built (`app/crates/cordata-view/src/fold.rs`, `tui/crates/acp-core/src/auto.rs`); written as a design on 2026-11-03 and kept as the reasoning. Where it differs from the code, the code is right. Reference: how Claude Code and Antigravity solve the same two problems (read with `rea`, see
"Prior art" at the end).

## The problem

A research turn in Cordata.app is a wall of rows: every `read_file`, every `run_command`, every `thinking` line gets its
own row, with absolute paths (`/Users/…/.cordata/offloaded/native-1793689952091-91485-7…`) and `cd /Users/…/Cordata && `
in front of every command. The one thing that needs you, the approval card, sits under a pending row that repeats its
command. And every network command stops the turn: `curl` of a public README asks, every time, with no "Always".

## Part 1 — a quiet transcript

### 1.1 Activity groups (cordata-view, pure)

A new pure function folds the message list into display rows:

```rust
// app/crates/cordata-view/src/fold.rs
pub enum Row { Message(usize), Activity(Activity) }
pub struct Activity { pub first: usize, pub last: usize, pub reads: usize, pub searches: usize, pub runs: usize,
                      pub other: usize, pub thoughts: usize, pub running: bool, pub hint: Option<String> }
pub fn rows(messages: &[Message]) -> Vec<Row>;
```

Rules (Claude Code's accumulator, Antigravity's flush rule):

- Consecutive **quiet** messages merge into one `Activity`: a tool card whose kind is read, search or run (`verb()` is
  `read`/`search`/`run`) and whose status is not `Failed`, plus `Thought`s and `Notice`s between them.
- Anything else **flushes** the group: user or assistant text, an edit (its diff stays visible), a failed tool (it
  pops out on its own row), an error, and the tool call that is waiting for approval.
- A group of one tool is not folded: it renders as today's row.
- `hint` is the newest card's target, shortened (1.3). `running` is true while the newest card is pending/in progress.

### 1.2 Rendering (cordata-app)

`workspace.rs` lists `fold::rows(&view.messages)` instead of raw messages. An `Activity` is one 28px row in the same
style as a tool row:

```
✓  Read 9 files, ran 6 commands, searched 2 · thought 3×      tui/crates/cordata-floorplan/src/lib.rs   ⌄
```

- Summary: counts joined with ", ", first verb capitalised, zero counts left out (`Read 1 file`, `ran 2 commands`,
  `searched 3 patterns`, `called 1 tool`). While running: `Reading …`/`Running …` with the hint, and a pulsing dot.
- Click expands it in place into today's individual rows (the expanded set keys on the first card's id).
- `view.expand_tools` (the existing "expand all") expands every group.

### 1.3 Shorter targets

One helper in cordata-view, used by tool rows, groups and the approval card:

- A path under the session cwd is shown relative to it; under `$HOME`, as `~/…`; anything else as is.
- A file in `~/.cordata/offloaded/` is shown as `offloaded output` plus its sequence number (`…-16.txt` gives
  `offloaded output 16`).
- A command that starts with `cd <cwd> && ` (or `cd <cwd>; `) is shown without that prefix.
- `summarize_input` keeps returning the raw text; shortening is a display step, so the model and the ledger still see
  full paths.

### 1.4 The approval moment

- While an approval card is up, the pending tool row it belongs to is not drawn: the card already shows the command.
- The card says why it asks when auto mode is on (see 2.4).

## Part 2 — auto mode

### 2.1 Modes

`chat.mode`: `ask` (today, the default) or `auto`. A user-file setting, also switchable per session:

- App: a chip in the composer footer next to the model (`ask` / `auto`, auto in amber), clicked or `⇧⇥` to cycle.
- TUI: `⇧⇥` cycles; the status line shows `auto`.
- The engine gets a `Command::SetMode(Mode)` so the switch applies from the next tool call, mid-turn.

### 2.2 What auto lets through (deterministic, in `acp-core`)
<!-- covers: tui/crates/acp-core/src/auto.rs @ f1d1057 -->

The order in `engine.rs` stays: guard, network-off, deny rules, allow rules. Auto mode only replaces the **ask** step,
through a pure function `auto::judge(tool, args, cwd, policy) -> Judgement { Allow(reason) | Ask(reason) }`:

| Call | Auto |
|---|---|
| `read_file`, `code_search`, `code_outline`, `code_map` | already never ask |
| `edit_file` inside the project or a `chat.writable` folder | allow ("edit inside the project") |
| `run_command` offline | allow ("sandboxed, offline"), unless it is locally destructive (below) |
| `run_command` with network that is a read-only fetch | allow ("read-only fetch") |
| any other network command | ask ("needs the network: <why>") |
| MCP and other tools | ask ("auto mode does not judge <tool>") |

Seatbelt already confines an offline command's writes to the project, temp and caches, so an offline command can only
damage the project; git can undo that unless the command throws history away. **Locally destructive** (asks even
offline): `git reset --hard`, `git clean`, `git checkout -- …`/`git restore` of tracked files, `git stash drop/clear`,
`git branch -D`, `git push` (also network), `rm` of a tracked path, `find … -delete`, `truncate`, and `>` redirection
onto a tracked file. Each is matched on every segment of a chained command, reusing `policy`'s segment splitter.

A **read-only fetch** is a single command (no `;`, `&&`, `|` other than into `head`, `tail`, `wc`, `jq`, `sed -n`,
`grep`, `rg`), whose program is `curl`, `wget`, `git fetch`, `git ls-remote`, `git clone` (into the project), `gh api`
without `-X`/`--method` other than GET and without `-f`/`-F`/`--input`, or `gh … view|list|status|diff`. It is refused as
a fetch when:

- it sends data: `-d`, `--data*`, `-F`, `--form`, `-T`, `--upload-file`, `-X`/`--request` other than GET/HEAD,
  `--post-data`, `--post-file`, `--body-file`;
- it is piped into a shell or interpreter (`sh`, `bash`, `zsh`, `python*`, `node`, `ruby`, `perl`), the
  remote-script-execution pattern;
- it writes outside the project (`-o`/`-O` to an absolute path outside cwd);
- `cordata_guard::redact::scrub` finds a secret anywhere in the command (an API key in a URL is exfiltration);
- the URL host is a raw IP or `localhost`-like (no SSRF to the LAN), or the URL embeds `user:pass@`.

### 2.3 Fail closed

Anything `judge` cannot parse asks. `judge` never calls a model: tests stay hermetic and a decision is instant. (A model
judge for "ask" cases is a later option, behind `chat.autoJudge`, and still fails closed.)

### 2.4 Being seen

- An auto-allowed call emits `AppEvent::AutoAllowed { id, reason }`, not a `Notice`; it shows inside the activity group
  (expanded row meta: `auto · read-only fetch`), never as its own line.
- An ask in auto mode puts the reason on the card: `Auto mode asks: needs the network to POST`.
- The ledger records `auto` decisions like rule decisions, so `/cost` and the History tab can show what ran unasked.

### 2.5 What does not change

Guard refusals, `chat.deny`, `chat.network = off`, and the sandbox all apply in auto mode. A network call is still never
remembered by "Always". A project file cannot turn auto on (`chat.mode` is user-file only, like `chat.allow`).

## Definition of done

```bash
make tui-test && make app-test
```

New tests, all offline:

- `fold`: groups, flushes on text/edit/failure/approval, single-tool groups stay rows, counts and hint.
- shorten: cwd-relative, `~/`, offloaded, `cd <cwd> &&` prefix.
- `auto::judge`: each row of the table above, each refusal reason for a fetch, chained/destructive commands, secrets in
  a URL, raw-IP hosts, fail-closed on unparsable input.
- engine: in auto mode a sandboxed offline command and a read-only fetch run with no `PermissionRequested`; a POST and
  `git reset --hard` still ask with a reason; deny rules and `network = off` still win; `SetMode` mid-session.
- settings: `chat.mode` from a project file is ignored with a warning.

## Prior art

- **Claude Code** groups any run of read/search/list (and, full-screen, bash) calls plus thinking into one
  `Read N files, searched N patterns` row with the latest target as a hint. Any text, edit or visible thinking flushes it,
  and a failed call pops out of the group. Paths are shown cwd-relative or as `~/…`. Its `auto` mode lets edits in the
  working dirs, safe tools and read-only commands through, and sends everything else to an LLM security classifier that
  fails closed. Modes are cycled with shift+tab.
- **Antigravity** splits the transcript into tool groups that any text response flushes. A finished group's header reads
  `Explored N files, ran commands`, and a past turn collapses to `Worked for …`. Its `AUTO` exec policy says to "proceed
  inside the sandbox", and a command that has to leave the sandbox goes to a policy judge that runs deterministic
  pre-checks before an LLM. Its reason codes include remote script execution, sensitive-file exfiltration and recursive
  home deletion.

Cordata already has the sandbox Antigravity's `AUTO` depends on, so auto mode here is a sandbox policy plus a
deterministic network judge, with no model call.
