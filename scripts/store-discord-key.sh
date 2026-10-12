#!/usr/bin/env bash
# Stores the Discord bot token in the macOS Keychain (where the daemon reads it) and
# then VERIFIES it against Discord, so a mistyped, truncated or revoked token is caught
# here rather than as a silent "discord: disabled" in the daemon log at startup.
#
#     make discord-key        (or scripts/store-discord-key.sh)
#
# The token is typed or pasted at a hidden prompt — never echoed, never put on a command
# line, never written to a file. Re-running replaces the stored value. The token is never
# printed by this script, including by the verification step below.
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

if ! command -v security >/dev/null 2>&1; then
  err "the macOS 'security' tool isn't available — export DISCORD_BOT_TOKEN in your shell instead."
  exit 1
fi

log "Paste your Discord bot token (hidden), press Enter, then paste it again to confirm."
# `security -w` with no value prompts twice, with echo off; -U updates an existing item.
security add-generic-password -U -a "$USER" -s cordata-discord-bot-token -w || { err "not stored."; exit 1; }
echo

# A stale exported variable would still win over what was just stored — DiscordRemote's
# own keychain fallback is only consulted when the environment has no DISCORD_BOT_TOKEN —
# so check the way the daemon resolves it, and say so rather than reporting a green check
# for a value that is not the one that will actually be used.
if [[ -n "${DISCORD_BOT_TOKEN:-}" ]]; then
  warn "DISCORD_BOT_TOKEN is exported in this shell and OVERRIDES the Keychain — unset it (fish: set -e DISCORD_BOT_TOKEN) or it will keep being used."
fi

token="$(security find-generic-password -s cordata-discord-bot-token -w 2>/dev/null || true)"
if [[ -z "$token" ]]; then
  err "stored, but reading it back returned nothing."
  exit 1
fi

# GET /users/@me is the cheapest authenticated call Discord has: 200 with the bot's own
# identity when the token is good, 401 otherwise. The response body is parsed for the
# name only — the token never reaches stdout.
response="$(curl -sS -m 10 -w '\n%{http_code}' -H "Authorization: Bot ${token}" https://discord.com/api/v10/users/@me 2>/dev/null || true)"
code="${response##*$'\n'}"
body="${response%$'\n'*}"

case "$code" in
  200)
    name="$(printf '%s' "$body" | sed -n 's/.*"username"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p')"
    ok "Discord accepted the token${name:+ — bot @${name}}."
    dim "Now set ALLOWED_USER_ID and TELEMETRY_CHANNEL_ID, enable Message Content Intent in"
    dim "the Developer Portal, and invite the bot — see backend/.env.example."
    ;;
  401) err "Discord rejected the token (401). Re-copy it from the Developer Portal → Bot → Reset Token."; exit 1 ;;
  000) warn "stored, but Discord could not be reached (offline?). Not verified." ;;
  *)   warn "stored, but verification got HTTP ${code}. Not confirmed." ;;
esac
