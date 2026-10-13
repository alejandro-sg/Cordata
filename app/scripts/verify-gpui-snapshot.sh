#!/usr/bin/env bash
# Prove the gpui-pre crates we build against are the Zed sources they claim to be (docs/app.md §3 D1, supply chain).
#
# gpui-pre 0.3.9 is a single-maintainer re-publish of Zed's gpui at zed@93d7456. This fetches exactly those Zed
# directories (sparse, blobless) and diffs every gpui-pre-* crate's src/ against them. The only differences allowed are
# the snapshot's own, labelled edits ("Modified for gpui-pre"): the macro facade that lets the crates be renamed.
# Every file the published crate ships is checked (packaging files aside); upstream files it leaves out (tests,
# benches, examples) are not a difference.
# Anything else fails — a bump of the pins must come with a re-run of this script and a reviewed allowlist.
#
#     app/scripts/verify-gpui-snapshot.sh            (needs network once; caches the checkout under target/)
set -euo pipefail

ZED_REV=93d745668805029476e6d7fbb19652116fed51de
VERSION=0.3.9
HERE="$(cd "$(dirname "$0")/.." && pwd)"
CACHE="${GPUI_SNAPSHOT_CACHE:-$HERE/target/zed-snapshot}"
REGISTRY="${CARGO_HOME:-$HOME/.cargo}/registry/src"

# crate in Zed -> crate on crates.io
PAIRS=(
  "gpui:gpui-pre" "gpui_macos:gpui-pre-macos" "gpui_platform:gpui-pre-platform" "gpui_macros:gpui-pre-macros"
  "collections:gpui-pre-collections" "gpui_util:gpui-pre-util" "sum_tree:gpui-pre-sum-tree" "refineable:gpui-pre-refineable"
  "refineable/derive_refineable:gpui-pre-derive-refineable" "scheduler:gpui-pre-scheduler" "http_client:gpui-pre-http-client"
  "gpui_shared_string:gpui-pre-shared-string" "ztracing:gpui-pre-ztracing" "ztracing_macro:gpui-pre-ztracing-macro"
)
# Files the snapshot is known to change, each carrying the "Modified for gpui-pre" marker.
ALLOWED=("gpui/src/action.rs" "gpui_macros/src/gpui_macros.rs" "gpui_macros/src/gpui_pre_facade_paths.rs")

if [ ! -d "$CACHE/.git" ]; then
  git clone -q --filter=blob:none --no-checkout https://github.com/zed-industries/zed.git "$CACHE"
fi
dirs=()
for pair in "${PAIRS[@]}"; do dirs+=("crates/${pair%%:*}"); done
git -C "$CACHE" sparse-checkout set --no-cone "${dirs[@]}"
git -C "$CACHE" checkout -q "$ZED_REV"

status=0
for pair in "${PAIRS[@]}"; do
  upstream="${pair%%:*}"; published="${pair##*:}"
  src_pub=$(ls -d "$REGISTRY"/*/"$published-$VERSION" 2>/dev/null | head -1 || true)
  if [ -z "$src_pub" ]; then echo "skip  $published-$VERSION (not in the cargo registry: run cargo fetch)"; continue; fi
  if [ ! -d "$CACHE/crates/$upstream" ]; then echo "FAIL  crates/$upstream missing at $ZED_REV"; status=1; continue; fi
  checked=0
  while IFS= read -r rel; do
    checked=$((checked + 1))
    ours="$src_pub/$rel"; theirs="$CACHE/crates/$upstream/$rel"
    if [ -f "$theirs" ] && cmp -s "$ours" "$theirs"; then continue; fi
    allowed=no
    for ok in "${ALLOWED[@]}"; do [ "$upstream/$rel" = "$ok" ] && allowed=yes; done
    if [ "$allowed" = yes ] && grep -q "Modified for gpui-pre" "$ours"; then
      echo "ok    $upstream/$rel (labelled snapshot edit)"
    elif [ -f "$theirs" ]; then
      echo "FAIL  $upstream/$rel differs from zed@$ZED_REV"; status=1
    else
      echo "FAIL  $published ships $rel, which zed@$ZED_REV does not have"; status=1
    fi
  done < <(cd "$src_pub" && find . -type f ! -name Cargo.toml ! -name Cargo.toml.orig ! -name Cargo.lock ! -name 'LICENSE*' ! -name .cargo_vcs_info.json ! -name .cargo-ok | sed 's#^\./##' | sort)
  echo "done  $upstream == $published-$VERSION ($checked files)"
done
exit $status
