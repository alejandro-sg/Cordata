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

The first release build compiles GPUI and takes several minutes. `make app` signs ad hoc by default, so macOS treats each
rebuild as a new app and forgets privacy grants; set `SIGN_IDENTITY` to a self-signed "Code Signing" certificate to keep
them. A friend running a copy you built has to open it with right-click → Open the first time.

Keys are never stored in the app's preferences: it reads the same environment variables and Keychain items as the
terminal client (`Settings → Account`, service `cordata-*-api-key`). Window preferences are in
`~/Library/Application Support/Cordata/prefs.toml`.

## What it has

Conversation rail (the open threads) and transcript, approval cards with the exact command or diff, composer with `/` and `@` menus,
command palette (⌘K), project picker (⌘O), settings (⌘,: launch, appearance, shortcuts, your API key), and a side panel
with Files, Editor, Review (per-hunk keep/revert, git and pull requests) and Terminal. Both clients share one
conversation reducer. Like the terminal client, the chat has no modes, undo, goal loops or saved sessions; every edit and
command asks first.

## Crates (`app/crates`)

- `cordata-view`: the state the terminal and the app share: the reducer (`App::on_event`), slash commands, input, menus,
  the settings schema and editor, run board, and the secret-masking table (`redact.rs`, tested against
  `proto/fixtures/redact_cases.json`).
- `cordata-theme`: colour roles with WCAG contrast tests; the terminal's palette reads it.
- `cordata-app`: the window: `session.rs` (acp-core on a tokio runtime → the reducer → effects), `workspace.rs` and its
  modules, `transcript.rs`, `platform.rs`.

The UI thread only draws; the agent's events reach it over channels a task drains.

## Dependencies

GPUI comes from `gpui-pre =0.3.9`, a republication of Zed commit `93d7456`, with `gpui-component =0.7.3`, both exactly
pinned with a committed lockfile. `app/scripts/verify-gpui-snapshot.sh` checks every shipped file of the gpui-pre crates
against Zed (only three labelled facade edits differ) and `app/deny.toml` bans a network stack from the graph. Zed's
GPL editor crates are never copied.
