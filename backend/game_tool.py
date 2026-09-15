"""Tells the frontend a browser game is ready to play. get_game_state /
apply_game_move (the other two tools this capability adds) are *frontend*
tools instead — see frontend/app/GameTools.tsx; nothing here proxies them,
ag_ui_strands does that on its own once the frontend registers them.

Deliberately writes nothing itself: the game's HTML must already exist on
disk via a real, human-approved file_write call to
games/<game_id>/index.html before this ever runs — this only confirms that
and tells the frontend which game_id/title to render. See
backend/skills/build-a-game/SKILL.md for the full write-then-announce
sequence and the postMessage contract the written file must implement for
get_game_state/apply_game_move to actually work against it.
"""

import re

from strands import tool

_GAME_ID_RE = re.compile(r"^[a-z0-9][a-z0-9-]{0,63}$")


@tool
def open_game(game_id: str, title: str) -> str:
    """Announce that a browser game is written and ready to play, opening it
    in the user's game panel.

    Call this ONLY after file_write has already succeeded at writing
    games/<game_id>/index.html — this tool never writes anything itself, it
    only checks that file exists. If file_write hasn't run yet, or was
    denied, call it first and wait for it to actually succeed.

    Args:
        game_id: The exact slug used in the games/<game_id>/ path passed to
            file_write. 1-64 characters, lowercase letters/digits/hyphens
            only (e.g. "tic-tac-toe") — no slashes, dots, or uppercase.
        title: Short human-readable title shown above the game, e.g.
            "Tic-Tac-Toe".
    """
    if not _GAME_ID_RE.match(game_id):
        return (
            f"Error: '{game_id}' isn't a valid game_id — use 1-64 characters of "
            "lowercase letters, digits, and hyphens only, no slashes or dots."
        )

    # Deferred: agent_common.WORKSPACE_DIR doesn't exist yet at THIS
    # module's own import time — agent_common imports this module (to add
    # open_game to its TOOLS list) partway through its own top-to-bottom
    # execution, before WORKSPACE_DIR is assigned further down that file.
    # Importing agent_common eagerly here would hit a half-initialized
    # module. model_control_tool.py's set_active_model/set_temperature/
    # set_memory_window do the identical deferred `import agent_common` for
    # the same reason — see agent_common.py's own comment on why its tool
    # imports precede the chdir.
    import agent_common

    index_path = agent_common.WORKSPACE_DIR / "games" / game_id / "index.html"
    if not index_path.is_file():
        return (
            f"Error: games/{game_id}/index.html doesn't exist yet — call file_write "
            "to create it first, then call open_game again."
        )

    return f"Game '{title}' ({game_id}) is ready — opening it now."
