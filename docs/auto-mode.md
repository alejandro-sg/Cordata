# Auto mode and the folded transcript

Reference for what is built: `tui/crates/acp-core/src/auto.rs` (the judge) and `app/crates/cordata-view/src/fold.rs`
(the folded transcript).

## Auto mode
<!-- covers: tui/crates/acp-core/src/auto.rs @ f30734f -->

`chat.mode` is `ask` (the default) or `auto`. It is read from the user settings file only; a project or local value is
ignored with a warning. In a session, ⇧⇥ cycles it (the app also has a chip in the composer footer), and the engine
applies the switch from the next tool call, mid-turn.

The order in the engine does not change: guard, network-off, `chat.deny`, `chat.allow`. Auto mode only replaces the
final **ask** step with `auto::judge(tool, args, cwd, policy)`, a pure function that returns `Allow(reason)` or
`Ask(reason)`. It never calls a model, so a decision is instant and tests stay hermetic.

| Call | Auto |
|---|---|
| `read_file`, `code_search`, `code_outline`, `code_map` | never asked, in either mode |
| `edit_file` inside the project or a `chat.writable` folder | allow ("edit inside the project") |
| `run_command` offline | allow ("sandboxed, offline"), unless locally destructive |
| `run_command` with network that is a read-only fetch | allow ("read-only fetch") |
| any other network command | ask ("needs the network: why") |
| MCP and other tools | ask ("auto mode does not judge tool") |

**Locally destructive** commands ask even offline: `git reset --hard|--merge|--keep`, `git clean`, `git checkout --`,
`git restore`, `git stash drop|clear`, `git branch -d`, `git push`, `rm -r`/`rm -rf`, `find -delete`, `dd of=`, and `>`
redirection onto a file. Each is matched on every segment of a chained command.

A **read-only fetch** is a single command whose program is `curl`, `wget`, `git fetch`, `git ls-remote`, `git clone`
(into the project), `gh api` with GET only, or `gh … view|list|status|diff`. Only `head`, `tail`, `wc`, `jq`, `sed -n`,
`grep` and `rg` may follow a pipe. It is refused as a fetch when it:

- sends data (`-d`, `--data*`, `-F`, `--form`, `-T`, `--upload-file`, `-X` other than GET or HEAD, `--post-data`, ...);
- pipes into a shell or interpreter (`sh`, `bash`, `zsh`, `python*`, `node`, `ruby`, `perl`);
- writes outside the project;
- contains a secret (`cordata_guard::redact::scrub` finds one, such as an API key in a URL);
- targets a raw IP or `localhost`-like host, or embeds `user:pass@`.

Anything `judge` cannot parse asks (fail closed).

**What does not change.** Guard refusals, `chat.deny`, `chat.network = off` and the Seatbelt sandbox all apply in
auto mode, and a network call is never remembered by "Always".

**Being seen.** An auto-allowed call emits `AppEvent::AutoAllowed { id, reason }`; it shows in the activity group's
expanded row (`auto · read-only fetch`), not as its own line. An ask in auto mode puts its reason on the approval card
(`Auto mode asks: needs the network to POST`).

## The folded transcript
<!-- covers: app/crates/cordata-view/src/fold.rs @ f30734f -->

`fold::rows(messages)` turns the message list into display rows, so a research turn is one line, not a wall:

- Consecutive **quiet** messages (read, search and run tool cards that did not fail, plus thoughts and notices) merge
  into one `Activity`, shown as `Read 9 files, ran 6 commands, searched 2 · thought 3×` with the newest target as a hint.
  While it runs, it reads `Reading …` or `Running …` and pulses.
- Anything else **flushes** the group: user or assistant text, an edit (its diff stays visible), a failed tool, an error,
  and the call waiting for approval. A group of one tool is not folded.
- Click expands a group in place; "expand all" (`view.expand_tools`) expands every one.
- Targets are shortened for display only: relative to the project, `~/…` under home, `offloaded output N` for files in
  `~/.cordata/offloaded/`, and a leading `cd <cwd> &&` dropped. The model and the ledger still see full paths.
- While an approval card is up, the pending row it belongs to is not drawn.
