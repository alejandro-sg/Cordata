# Dev environment scripts

macOS-oriented (this project targets Apple Silicon for the oMLX model server),
but the shell scripts themselves are plain bash and should work on Linux too.

| Script | Does |
|---|---|
| `setup.sh` | One-time: backend venv + pinned deps, frontend deps, `.env`/`.env.local` from their `.example` templates. Safe to re-run. |
| `dev-up.sh` | Starts Qdrant (container, auto-detects docker/podman), backend, frontend. Checks your oMLX server is reachable and warns (doesn't start it — that's external). `--no-qdrant` to skip it. |
| `dev-down.sh` | Stops backend + frontend. Leaves Qdrant running (`--with-qdrant` to stop it too). |
| `dev-restart.sh` | Stops and restarts backend + frontend only. |
| `dev-status.sh` | Prints what's running and whether each is actually responding. |

Also available as `make setup` / `make up` / `make down` / `make restart` / `make status` from the repo root.

State lives in `.dev/` at the repo root (gitignored): one `<name>.pid` + `<name>.log` per process. `tail -f .dev/backend.log` / `.dev/frontend.log` to watch either live.
