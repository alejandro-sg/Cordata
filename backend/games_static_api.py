"""Serves backend/workspace/games/<game_id>/* for GameSandbox.tsx's
<iframe src="/api/games/<game_id>/index.html">, proxied through
frontend/app/api/games/[...path]/route.ts (a GET-only pass-through relaying
the real content-type — see that file's own comment for why, unlike
api/sandbox/[...path]/route.ts, it can't fall back to JSON).

Note for anyone debugging a blank game panel while testing through Claude's
own Browser-pane tool specifically (not a real browser): that tool's iframe
loading is unreliable for *any* real-network `<iframe src>`, same-origin or
not — confirmed live by testing three separate controls: (1) this exact URL
via `fetch()` succeeded (200) while the identical URL as an `<iframe src>`
failed with net::ERR_BLOCKED_BY_CLIENT; (2) an `<iframe src="/">` pointed at
this app's own homepage failed identically; (3) an `<iframe src=
"https://example.com">` never even produced a network request. Renaming
this route (tried /play, reverted — same failure, ruling out a URL-keyword
filter) didn't help, confirming it's not about this route specifically. The
one iframe that *does* render in that tool is the pre-existing
generateSandboxedUi feature, whose content is inline (srcdoc/blob) rather
than a real network fetch — the tool's restriction is specifically on
real-network subframe loads. A real browser has no such restriction; this
route, its content-type handling, and the traversal guard below were all
separately confirmed correct by loading this exact URL directly in a plain
browser tab.

Not app.mount(..., StaticFiles(...)): a Mount delegates straight to a raw
ASGI app with no `dependencies=` parameter at all (confirmed:
`inspect.signature(FastAPI.mount)` is `(self, path, app, name=None)`), so
`_require_sandbox_token` could never gate it that way. This uses the same
APIRouter + Depends(_require_sandbox_token) shape sandbox_functions_api.py's
own routes use, reusing that exact dependency (not duplicating the token
check) so there's one source of truth for it — same reasoning that module's
own docstring gives for why /sandbox/* needed a network-edge gate in the
first place: main.py binds to 0.0.0.0, this deployment genuinely runs
frontend and backend on separate LAN machines, and a plain `<iframe src>`
load is a normal browser GET with no way to attach a custom header — but
the Next.js proxy's own hop to this backend can, exactly like it already
does for /sandbox/*.
"""

import mimetypes
from pathlib import Path

from fastapi import APIRouter, Depends, HTTPException
from fastapi.responses import FileResponse

# sandbox_functions_api before agent_common, not just before it in this
# file's own read order: sandbox_functions_api's own `from agent_common
# import WORKSPACE_DIR` is what triggers agent_common's os.chdir() into
# workspace/ (see main.py's own comment on this) - importing it first here
# means that chdir happens *inside* this line, while the '' (cwd) sys.path
# entry this module's own bare-name resolution needs is still backend/.
# Importing agent_common directly first would chdir before sandbox_
# functions_api ever got a chance to resolve, breaking it the same way an
# unpatched import order broke `from games_static_api import ...` in main.py.
from sandbox_functions_api import _require_sandbox_token
from agent_common import WORKSPACE_DIR

router = APIRouter(prefix="/games")

GAMES_DIR = WORKSPACE_DIR / "games"


def _resolve_in_game_dir(game_id: str, file_path: str) -> Path:
    """Same two-check shape as sandbox_functions_api._resolve_in_workspace
    (reject absolute/'..' in the raw string, then re-check is_relative_to
    after .resolve() — see that function's own docstring for why both
    checks matter, symlinks specifically) — scoped to one game's own
    directory rather than the whole workspace, so this route can't
    enumerate a *different* game_id's files even by accident. game_id
    itself is checked as a single safe path segment here independently of
    game_tool.open_game's own validation — this route must not assume every
    caller went through that tool first.
    """
    if not game_id or "/" in game_id or ".." in game_id or game_id.startswith("."):
        raise HTTPException(status_code=400, detail=f"invalid game_id: {game_id!r}")

    game_dir = GAMES_DIR / game_id
    candidate = Path(file_path)
    if candidate.is_absolute() or ".." in candidate.parts:
        raise HTTPException(status_code=400, detail=f"path must be relative and stay inside the game: {file_path!r}")

    resolved_game_dir = game_dir.resolve()
    resolved = (game_dir / candidate).resolve()
    if not resolved.is_relative_to(resolved_game_dir):
        raise HTTPException(status_code=400, detail=f"path escapes the game directory: {file_path!r}")
    return resolved


def register_game_routes(app) -> None:
    """Mirrors sandbox_functions_api.register_sandbox_routes' signature
    shape (a plain function taking `app`, called from main.py once `app`
    exists) — same reason that module gives: this stays import-order-free
    with respect to main.py.
    """

    @router.get("/{game_id}/{file_path:path}", dependencies=[Depends(_require_sandbox_token)])
    def get_game_file(game_id: str, file_path: str):
        resolved = _resolve_in_game_dir(game_id, file_path)
        if not resolved.is_file():
            raise HTTPException(status_code=404, detail=f"no such file in games/{game_id}/: {file_path!r}")
        media_type, _ = mimetypes.guess_type(str(resolved))
        return FileResponse(resolved, media_type=media_type or "application/octet-stream")

    app.include_router(router)
