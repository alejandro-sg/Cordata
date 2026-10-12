#!/usr/bin/env bash
# Build the menu-bar deck as a real macOS app: `Cordata.app`, a menu-bar-only bundle (no Dock
# icon) with its own identity, so the Accessibility and Screen Recording grants a person gives it
# belong to *this app* rather than to whichever terminal launched a bare binary.
#
#   scripts/package-menubar.sh               # build + bundle + sign into lab/target/Cordata.app
#   scripts/package-menubar.sh --install     # …and copy it to ~/Applications
#   SIGN_IDENTITY="Cordata Local" scripts/package-menubar.sh --install
#
# Signing decides whether a grant survives a rebuild, because macOS keys a grant to the app's
# designated requirement:
#   * no SIGN_IDENTITY → ad-hoc signed. The requirement is the build's own hash, so every rebuild is
#     a new app to macOS and Accessibility must be switched on again (remove the old row first).
#   * SIGN_IDENTITY="<name>" → signed with that certificate. The requirement names the certificate
#     and the bundle id, so the grant outlives rebuilds. A self-signed one is enough: Keychain Access
#     › Certificate Assistant › Create a Certificate…, type "Code Signing". That is a change to your
#     keychain, so this script never makes one for you.
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

BUNDLE_ID="dev.cordata.menubar"
APP_NAME="Cordata"
INSTALL=0
for arg in "$@"; do
  case "$arg" in
    --install) INSTALL=1 ;;
    -h|--help) sed -n '2,17p' "$0"; exit 0 ;;
    *) err "unknown argument: $arg"; exit 2 ;;
  esac
done

VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' "$ROOT/lab/Cargo.toml" | head -1)"
log "building harness-menubar ${VERSION:-?} (release)…"
(cd "$ROOT/lab" && cargo build --release -q -p harness-menubar)

APP="$ROOT/lab/target/$APP_NAME.app"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$ROOT/lab/target/release/harness-menubar" "$APP/Contents/MacOS/harness-menubar"

# LSUIElement: a menu-bar app has no Dock icon and no app menu. The usage strings are what macOS
# shows in its own permission prompts; a missing NSScreenCaptureUsageDescription terminates a process
# the moment it asks for the screen, so it is present before any screenshot code can run.
cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleIdentifier</key><string>$BUNDLE_ID</string>
  <key>CFBundleName</key><string>$APP_NAME</string>
  <key>CFBundleDisplayName</key><string>$APP_NAME</string>
  <key>CFBundleExecutable</key><string>harness-menubar</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>${VERSION:-0.1.0}</string>
  <key>CFBundleVersion</key><string>${VERSION:-0.1.0}</string>
  <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
  <key>LSMinimumSystemVersion</key><string>13.0</string>
  <key>LSUIElement</key><true/>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSScreenCaptureUsageDescription</key><string>Cordata captures the frontmost window only when you click Screenshot in the deck, and only for apps you allowed.</string>
  <key>NSAppleEventsUsageDescription</key><string>Cordata does not send Apple Events.</string>
</dict>
</plist>
PLIST

IDENTITY="${SIGN_IDENTITY:--}"
codesign --force --options runtime --identifier "$BUNDLE_ID" --sign "$IDENTITY" "$APP" 2>&1 | grep -v 'replacing existing signature' || true
codesign --verify --strict "$APP"
if [[ "$IDENTITY" == "-" ]]; then
  warn "ad-hoc signed: macOS will treat each rebuild as a new app (re-grant Accessibility after rebuilding). Set SIGN_IDENTITY to keep grants."
else
  ok "signed with \"$IDENTITY\": grants survive rebuilds"
fi
dim "$(codesign -d -r- "$APP" 2>&1 | sed -n 's/^# designated => /designated requirement: /p')"
ok "$APP"

if [[ "$INSTALL" == 1 ]]; then
  mkdir -p "$HOME/Applications"
  # Quit a running copy first: replacing the bundle under a live process leaves it running old code.
  pkill -x harness-menubar 2>/dev/null || true
  rm -rf "$HOME/Applications/$APP_NAME.app"
  cp -R "$APP" "$HOME/Applications/"
  ok "installed: ~/Applications/$APP_NAME.app — open it, then System Settings › Privacy & Security › Accessibility › $APP_NAME"
fi
