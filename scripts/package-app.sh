#!/usr/bin/env bash
# Build Cordata.app — the native workspace (app/, docs/app.md) — as a signed macOS bundle.
#
#   scripts/package-app.sh                 # release build + bundle + sign into app/target/Cordata.app, then self-test
#   scripts/package-app.sh --install       # …and copy it to ~/Applications
#   SIGN_IDENTITY="Cordata Local" scripts/package-app.sh
#
# A regular Dock app (LSUIElement false) with the `cordata://` URL scheme, the hardened runtime and the entitlements in
# app/assets/Cordata.entitlements (empty on purpose). Signing decides whether macOS privacy grants survive a rebuild —
# ad-hoc (the default) changes the designated requirement every build, so TCC forgets the grant; a self-signed
# "Code Signing" certificate named in SIGN_IDENTITY (Keychain Access → Certificate Assistant) keeps it. This script
# never creates one.
#
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
# Bundled fonts (Geist, OFL) when vendored in app/assets/fonts; the app falls back to the system fonts without them.
if compgen -G "$ROOT/app/assets/fonts/*.[ot]tf" >/dev/null; then
  mkdir -p "$APP/Contents/Resources/fonts"
  cp "$ROOT"/app/assets/fonts/*.[ot]tf "$ROOT"/app/assets/fonts/OFL.txt "$APP/Contents/Resources/fonts/" 2>/dev/null || true
fi
cp "$ROOT/app/target/release/cordata-app" "$APP/Contents/MacOS/cordata-app"
# The quota meter and the run field read the ledger through `cordata-run`; ship it beside the app so they work out of the
# box (the app looks next to its own executable first).
log "building cordata-run (release)…"
(cd "$ROOT/tui" && cargo build --release -q -p cordata-run)
cp "$ROOT/tui/target/release/cordata-run" "$APP/Contents/MacOS/cordata-run"

# The app icon is app/assets/brand/Cordata.icns (rebuilt from cordata-icon.svg, see the README beside it); without it
# the bundle gets the generic icon.
ICON_KEY=""
if [ -f "$ROOT/app/assets/brand/Cordata.icns" ]; then
  cp "$ROOT/app/assets/brand/Cordata.icns" "$APP/Contents/Resources/Cordata.icns"
  ICON_KEY="<key>CFBundleIconFile</key><string>Cordata</string>"
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
codesign --force --options runtime --identifier "dev.cordata.run" --sign "$IDENTITY" "$APP/Contents/MacOS/cordata-run" 2>&1 | grep -v 'replacing existing signature' || true
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
  # Unregister and remove the build artifact copy so LaunchServices and Spotlight only see the installed app.
  /System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister -u "$APP" 2>/dev/null || true
  rm -rf "$APP"
  /System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister -f "$HOME/Applications/$APP_NAME.app" 2>/dev/null || true
  ok "installed: ~/Applications/$APP_NAME.app"
fi
