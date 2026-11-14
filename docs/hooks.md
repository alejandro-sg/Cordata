# Claude Code hooks for the app's terminal (OSC 7501)

How to make Claude Code declare its state to Cordata.app's dock terminal (or any host that understands OSC 7501),
instead of the terminal guessing from the screen.

These are Claude Code's hooks, not Cordata's own tool hooks (`.cordata/hooks.json`, see
[`settings.md`](settings.md#tool-hooks-hooksjson)).

## Declared state

A program prints an escape sequence when its state changes:

```text
\x1b]7501;state=<idle|working|done|blocked|error>[;kind=<permission|question|auth>]\x07
```

Fields are separated by `;`. `blocked` and `error` show as blocked, `idle` and `done` as idle. The dock terminal's
parser (`app/crates/cordata-app/src/vt.rs`) reads the sequence from the byte stream and prefers it over the screen
rules (`detect.rs`, `detect.toml`) until the program declares something else.

## Claude Code configuration

Hook output goes to Claude Code, not the terminal, so write to `/dev/tty`. In `~/.claude/settings.json` or the
project's `.claude/settings.json`:

```json
{
  "hooks": {
    "UserPromptSubmit": [
      { "hooks": [{ "type": "command", "command": "printf '\\033]7501;state=working\\007' > /dev/tty" }] }
    ],
    "Notification": [
      { "matcher": "permission_prompt",
        "hooks": [{ "type": "command", "command": "printf '\\033]7501;state=blocked;kind=permission\\007' > /dev/tty" }] },
      { "matcher": "idle_prompt",
        "hooks": [{ "type": "command", "command": "printf '\\033]7501;state=idle\\007' > /dev/tty" }] }
    ],
    "Stop": [
      { "hooks": [{ "type": "command", "command": "printf '\\033]7501;state=done\\007' > /dev/tty" }] }
    ]
  }
}
```

Not verified against a running Claude Code in this repository: check the field names against
[Claude Code's hooks reference](https://code.claude.com/docs/en/hooks) if a hook does not fire.

## Cordata's own client

`acp-tui` emits the sequence itself on state changes (`tui/crates/acp-tui/src/signals.rs`):

| state | sequence |
|---|---|
| working | `\x1b]7501;state=working\x07` |
| waiting on an approval | `\x1b]7501;state=blocked;kind=permission\x07` |
| done | `\x1b]7501;state=done\x07` |
| idle, connecting, closed | `\x1b]7501;state=idle\x07` |
