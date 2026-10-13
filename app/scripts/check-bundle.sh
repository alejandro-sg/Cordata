#!/usr/bin/env bash
# Check a signed Cordata.app against what it is allowed to be (docs/app.md §12, THREAT_MODEL.md "Native app"):
# hardened runtime on, the signed entitlements exactly app/assets/Cordata.entitlements, no JIT or DYLD keys, every
# dylib it loads either a system one or inside the bundle (no @rpath or relative load that could pick up a planted
# library), and the identifier it should have.
#
#     app/scripts/check-bundle.sh app/target/Cordata.app
set -euo pipefail
APP="${1:?usage: check-bundle.sh path/to/Cordata.app}"
HERE="$(cd "$(dirname "$0")/.." && pwd)"
BIN="$APP/Contents/MacOS/cordata-app"
fail() { echo "✗ $*" >&2; exit 1; }

info="$(codesign -dvvv "$APP" 2>&1)"
grep -q 'flags=.*runtime' <<<"$info" || fail "hardened runtime is off"
grep -q '^Identifier=dev.cordata.app$' <<<"$info" || fail "identifier is not dev.cordata.app"

signed="$(codesign -d --entitlements - --xml "$APP" 2>/dev/null | plutil -convert json -o - - 2>/dev/null || echo '{}')"
wanted="$(plutil -convert json -o - "$HERE/assets/Cordata.entitlements")"
[[ "$signed" == "$wanted" ]] || fail "signed entitlements differ from app/assets/Cordata.entitlements: $signed"
for key in allow-jit allow-unsigned-executable-memory disable-library-validation allow-dyld-environment-variables disable-executable-page-protection; do
  grep -q "$key" <<<"$signed" && fail "entitlement $key is not allowed"
done

while read -r lib; do
  case "$lib" in
    /usr/lib/*|/System/Library/*) ;;
    @executable_path/*|@loader_path/*) [[ -e "$APP/Contents/MacOS/${lib#*/}" ]] || fail "bundle-relative load missing: $lib" ;;
    *) fail "unexpected load: $lib (only system libraries and the bundle's own are allowed)" ;;
  esac
done < <(otool -L "$BIN" | tail -n +2 | awk '{print $1}')
echo "✓ bundle: hardened runtime, entitlements as declared, $(otool -L "$BIN" | tail -n +2 | wc -l | tr -d ' ') loads all system"
