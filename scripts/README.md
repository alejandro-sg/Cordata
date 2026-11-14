# Scripts

macOS-oriented, plain bash.

| Script | Does |
|---|---|
| `install-cli.sh` | Symlinks `bin/cordata` into `~/.local/bin` (`make cli`). |
| `store-key.sh` / `check-key.sh` | Store a provider's API key (`PROVIDER=anthropic`, the default, or `ollama`) in the macOS Keychain; for Ollama, show where the key comes from and whether it is accepted. |
| `package-app.sh` | Builds, signs and self-tests `Cordata.app` (`make app`). |
| `verify.sh` | Secrets check + `make tui-test` (`--all` adds `make app-test`). Exit 0 or fail (`make verify`). |
| `bench.sh` | Release-build timings of the hot paths, offline: redaction, frame draw, session save, MCP start, repo map vs grep, cold starts (`make bench`). |
| `lib.sh` | Shared helpers the other scripts source. Not run directly. |

The secrets scan is a Rust binary (`tui/crates/cordata-guard`): `make secrets`, `make secrets ARGS=--history`, and the
pre-commit hook (`make hooks`).
