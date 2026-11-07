#!/usr/bin/env bash
# Cordata's hot paths, in release builds, offline: `scripts/bench.sh` (or `make bench`). Prints `BENCH` lines.
#   redaction throughput · a transcript frame's draw cost · session save/list · MCP server start · repo map vs grep ·
#   cold starts · git
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

ms() { # median wall time of 11 runs of "$@", in ms
  python3 -I -c 'import subprocess,sys,time,statistics
t=[]
for _ in range(11):
    s=time.perf_counter(); subprocess.run(sys.argv[1:],stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL); t.append((time.perf_counter()-s)*1e3)
print(f"{statistics.median(t):.1f}")' "$@"
}

log "=== micro (release, hermetic)"
(cd "$ROOT/tui" && cargo test -q --release -p cordata-guard bench_ -- --ignored --nocapture 2>/dev/null | grep BENCH)
(cd "$ROOT/tui" && cargo test -q --release -p acp-core bench_ -- --ignored --nocapture 2>/dev/null | grep BENCH)
(cd "$ROOT/tui" && cargo test -q --release -p cordata-repomap --features embed bench_ -- --ignored --nocapture 2>/dev/null | grep BENCH)
(cd "$ROOT/app" && cargo test -q --release -p cordata-view --features crossterm bench_ -- --ignored --nocapture 2>/dev/null | grep BENCH)
(cd "$ROOT/app" && cargo test -q --release -p cordata-app bench_ -- --ignored --nocapture 2>/dev/null | grep BENCH)

log "=== process"
(cd "$ROOT/tui" && cargo build -q --release)
for bin in acp-tui cordata-run cordata-doctor; do
  exe="$ROOT/tui/target/release/$bin"
  [[ -x "$exe" ]] && log "BENCH cold start $bin --help $(ms "$exe" --help)ms · $(du -h "$exe" | cut -f1) on disk"
done
log "BENCH git refresh, sequential: status $(ms git -C "$ROOT" status --porcelain=v2 --branch)ms + worktree list $(ms git -C "$ROOT" worktree list --porcelain)ms"
