#!/usr/bin/env bash
# Build a macOS .app bundle (and optional .dmg) from release binaries.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

VERSION="${1:-dev}"
ARCH="$(uname -m)"
APP_NAME="Multerm"
APP_DIR="dist/${APP_NAME}.app"
STAGING="dist/release-macos-${ARCH}"
BIN_UI="${ROOT}/target/release/multerm-ui"
BIN_TERM="${ROOT}/target/release/multerm"
ICON="${ROOT}/multerm-app/assets/icons/multerm_logo_no_bg.png"

if [[ ! -x "$BIN_UI" ]]; then
  echo "error: missing release binary — run: cargo build --release -p multerm-app --bin multerm-ui" >&2
  exit 1
fi

rm -rf "$STAGING" "$APP_DIR"
mkdir -p "$STAGING" "$APP_DIR/Contents/MacOS" "$APP_DIR/Contents/Resources"

cp "$BIN_UI" "$APP_DIR/Contents/MacOS/multerm-ui"
if [[ -x "$BIN_TERM" ]]; then
  cp "$BIN_TERM" "$APP_DIR/Contents/MacOS/multerm"
fi
chmod +x "$APP_DIR/Contents/MacOS/"*

# Ad-hoc sign so Gatekeeper is less likely to report the app as "damaged".
# This is not Apple notarization — browsers still quarantine downloaded files.
if command -v codesign >/dev/null 2>&1; then
  codesign --force --deep --sign - "$APP_DIR" 2>/dev/null || true
fi

if [[ -f "$ICON" ]]; then
  cp "$ICON" "$APP_DIR/Contents/Resources/AppIcon.png"
fi

cat > "$APP_DIR/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleDevelopmentRegion</key>
  <string>en</string>
  <key>CFBundleExecutable</key>
  <string>multerm-ui</string>
  <key>CFBundleIconFile</key>
  <string>AppIcon</string>
  <key>CFBundleIdentifier</key>
  <string>dev.multerm.app</string>
  <key>CFBundleInfoDictionaryVersion</key>
  <string>6.0</string>
  <key>CFBundleName</key>
  <string>${APP_NAME}</string>
  <key>CFBundlePackageType</key>
  <string>APPL</string>
  <key>CFBundleShortVersionString</key>
  <string>${VERSION#v}</string>
  <key>CFBundleVersion</key>
  <string>${VERSION#v}</string>
  <key>LSMinimumSystemVersion</key>
  <string>12.0</string>
  <key>NSHighResolutionCapable</key>
  <true/>
</dict>
</plist>
PLIST

ZIP="${STAGING}/Multerm-${VERSION}-macos-${ARCH}.zip"
ditto -c -k --sequesterRsrc --keepParent "$APP_DIR" "$ZIP"
echo "Created ${ZIP}"

if command -v hdiutil >/dev/null 2>&1; then
  DMG="${STAGING}/Multerm-${VERSION}-macos-${ARCH}.dmg"
  rm -f "$DMG"
  hdiutil create -volname "$APP_NAME" -srcfolder "$APP_DIR" -ov -format UDZO "$DMG" >/dev/null
  echo "Created ${DMG}"
fi
