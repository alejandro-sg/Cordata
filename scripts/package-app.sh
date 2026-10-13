#!/usr/bin/env bash
# Build Cordata.app — the native workspace (app/, docs/app.md) — as a signed macOS bundle.
#
#   scripts/package-app.sh                 # release build + bundle + sign into app/target/Cordata.app, then self-test
#   scripts/package-app.sh --install       # …and copy it to ~/Applications
#   SIGN_IDENTITY="Cordata Local" scripts/package-app.sh
#
# A regular Dock app (LSUIElement false) with the `cordata://` URL scheme, the hardened runtime and the entitlements in
# app/assets/Cordata.entitlements (empty on purpose). Signing decides whether macOS privacy grants survive a rebuild —
# see scripts/package-menubar.sh for the full story: ad-hoc (the default) changes the designated requirement every
# build; a self-signed "Code Signing" certificate named in SIGN_IDENTITY keeps it. This script never creates one.
#
# The bundle finds the harness through the daemon (~/.cordata/daemon.json), not through the checkout it was built
# from: launched from Finder it has no repo around it and a cwd of /.
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

BUNDLE_ID="dev.cordata.app"
APP_NAME="Cordata"
INSTALL=0
for arg in "$@"; do
  case "$arg" in
    --install) INSTALL=1 ;;
    -h|--help) sed -n '2,13p' "$0"; exit 0 ;;
    *) err "unknown argument: $arg"; exit 2 ;;
  esac
done

VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' "$ROOT/app/Cargo.toml" | head -1)"
log "building cordata-app ${VERSION:-?} (release)…"
(cd "$ROOT/app" && cargo build --release -q -p cordata-app)

APP="$ROOT/app/target/$APP_NAME.app"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$ROOT/app/target/release/cordata-app" "$APP/Contents/MacOS/cordata-app"

ICON_KEY=""
ICONSET="$ROOT/app/target/Cordata.iconset"
if "$ROOT/backend/.venv/bin/python" "$ROOT/app/assets/make_icon.py" "$ICONSET" >/dev/null 2>&1 && iconutil -c icns "$ICONSET" -o "$APP/Contents/Resources/Cordata.icns"; then
  ICON_KEY="<key>CFBundleIconFile</key><string>Cordata</string>"
else
  warn "no icon (needs backend/.venv with Pillow): the bundle gets the generic one"
fi

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleIdentifier</key><string>$BUNDLE_ID</string>
  <key>CFBundleName</key><string>$APP_NAME</string>
  <key>CFBundleDisplayName</key><string>$APP_NAME</string>
  <key>CFBundleExecutable</key><string>cordata-app</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>${VERSION:-0.1.0}</string>
  <key>CFBundleVersion</key><string>${VERSION:-0.1.0}</string>
  <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
  $ICON_KEY
  <key>LSMinimumSystemVersion</key><string>13.0</string>
  <key>LSApplicationCategoryType</key><string>public.app-category.developer-tools</string>
  <key>LSUIElement</key><false/>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSSupportsAutomaticGraphicsSwitching</key><true/>
  <key>CFBundleURLTypes</key>
  <array>
    <dict>
      <key>CFBundleURLName</key><string>$BUNDLE_ID</string>
      <key>CFBundleURLSchemes</key><array><string>cordata</string></array>
    </dict>
  </array>
</dict>
</plist>
PLIST
plutil -lint -s "$APP/Contents/Info.plist"

IDENTITY="${SIGN_IDENTITY:--}"
codesign --force --options runtime --identifier "$BUNDLE_ID" --entitlements "$ROOT/app/assets/Cordata.entitlements" --sign "$IDENTITY" "$APP" 2>&1 | grep -v 'replacing existing signature' || true
codesign --verify --strict "$APP"
"$ROOT/app/scripts/check-bundle.sh" "$APP"
if [[ "$IDENTITY" == "-" ]]; then
  warn "ad-hoc signed: macOS treats each rebuild as a new app. Set SIGN_IDENTITY to keep privacy grants."
else
  ok "signed with \"$IDENTITY\""
fi

# The bundle's own binary, run as Finder would run it (cwd /): it must start and draw.
if (cd / && "$APP/Contents/MacOS/cordata-app" --selftest 2>&1 | grep -q '^SELFTEST window=ok'); then
  ok "self-test: the bundle starts and draws"
else
  err "self-test failed: run $APP/Contents/MacOS/cordata-app --selftest to see why"
  exit 1
fi
ok "$APP"

if [[ "$INSTALL" == 1 ]]; then
  mkdir -p "$HOME/Applications"
  if [[ -d "$HOME/Applications/$APP_NAME.app" ]] && [[ "$(defaults read "$HOME/Applications/$APP_NAME.app/Contents/Info" CFBundleIdentifier 2>/dev/null)" != "$BUNDLE_ID" ]]; then
    warn "~/Applications/$APP_NAME.app is another app (the menu-bar deck?); it is replaced — its privacy grants go with it"
  fi
  pkill -x cordata-app 2>/dev/null || true
  rm -rf "$HOME/Applications/$APP_NAME.app"
  cp -R "$APP" "$HOME/Applications/"
  ok "installed: ~/Applications/$APP_NAME.app"
fi
