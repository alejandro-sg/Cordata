# Claude Code Hooks for Cordata & OSC 7501 Terminals

This document explains how to configure Claude Code to declare its execution state directly to Cordata (or any OSC 7501-compliant terminal host) via terminal escape sequences, eliminating screen scraping and guessing.

---

## 1. Background: Declared State via OSC 7501

Rather than guessing whether an agent is `working`, `blocked` (waiting on user permission), `idle`, or `done` from terminal line scraping, programs emit OSC 7501 escape sequences:

```text
\x1b]7501;state=<idle|working|done|blocked|error>[:kind=<permission|question|auth>]\x07
```

Cordata's terminal PTY parser (`vt.rs`) detects OSC 7501 sequences in the byte stream and prioritizes declared state over screen-scraping heuristics (`detect.rs`).

---

## 2. Claude Code Hook Configuration

In your user or project settings (e.g. `~/.claude/settings.json` or `.claude/settings.json`), add the following `hooks` configuration:

```json
{
  "hooks": {
    "UserPromptSubmit": [
      {
        "matcher": ".*",
        "command": "printf '\\033]7501;state=working\\007'"
      }
    ],
    "Notification": [
      {
        "matcher": "permission_prompt",
        "command": "printf '\\033]7501;state=blocked:kind=permission\\007'"
      },
      {
        "matcher": "idle_prompt",
        "command": "printf '\\033]7501;state=idle\\007'"
      }
    ],
    "Stop": [
      {
        "matcher": ".*",
        "command": "printf '\\033]7501;state=done\\007'"
      }
    ]
  }
}
```

---

## 3. Alternative: Shell Hook Script

If you prefer a standalone script (e.g., `~/.claude/hooks/osc7501.sh`):

```bash
#!/usr/bin/env bash
# ~/.claude/hooks/osc7501.sh

EVENT_TYPE="$1"
NOTIFICATION_TYPE="$2"

case "$EVENT_TYPE" in
  "UserPromptSubmit"|"Start")
    printf '\033]7501;state=working\007'
    ;;
  "Notification")
    case "$NOTIFICATION_TYPE" in
      "permission_prompt")
        printf '\033]7501;state=blocked:kind=permission\007'
        ;;
      "idle_prompt")
        printf '\033]7501;state=idle\007'
        ;;
    esac
    ;;
  "Stop")
    printf '\033]7501;state=done\007'
    ;;
esac
```

---

## 4. Cordata Client Emission

For Cordata's native TUI and client processes (`acp-tui`), OSC 7501 sequences are automatically emitted by `acp-tui/src/signals.rs` on state transitions:

- Working: `\x1b]7501;state=working\x07`
- Blocked (waiting on permission): `\x1b]7501;state=blocked:kind=permission\x07`
- Idle: `\x1b]7501;state=idle\x07`
- Done: `\x1b]7501;state=done\x07`
