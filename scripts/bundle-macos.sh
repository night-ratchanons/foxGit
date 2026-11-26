#!/usr/bin/env bash
# Build target/release/FoxGit.app (and optionally FoxGit-<version>-macos.dmg).
#
#   scripts/bundle-macos.sh                     this Mac's architecture
#   scripts/bundle-macos.sh --universal         Apple Silicon + Intel in one app
#   scripts/bundle-macos.sh --universal --dmg   … and a .dmg for distribution
#
# Signing (optional — without it the app is ad-hoc signed and downloads show a Gatekeeper
# warning on first launch):
#   MACOS_SIGN_IDENTITY="Developer ID Application: Name (TEAMID)"
# Notarization (needs signing), either a stored notarytool profile:
#   MACOS_NOTARY_PROFILE=foxgit
# or Apple ID credentials:
#   APPLE_ID=you@example.com APPLE_TEAM_ID=TEAMID APPLE_APP_PASSWORD=xxxx-xxxx-xxxx-xxxx
set -euo pipefail
cd "$(dirname "$0")/.."
command -v cargo >/dev/null || source "$HOME/.cargo/env"

UNIVERSAL=0
DMG=0
for a in "$@"; do
  case "$a" in
    --universal) UNIVERSAL=1 ;;
    --dmg) DMG=1 ;;
    *) echo "unknown option: $a" >&2; exit 2 ;;
  esac
done

VERSION=$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)
export MACOSX_DEPLOYMENT_TARGET=11.0

if [[ $UNIVERSAL == 1 ]]; then
  rustup target add aarch64-apple-darwin x86_64-apple-darwin >/dev/null
  cargo build --release --target aarch64-apple-darwin
  cargo build --release --target x86_64-apple-darwin
  BIN=target/release/foxgit-universal
  mkdir -p target/release
  lipo -create -output "$BIN" \
    target/aarch64-apple-darwin/release/foxgit \
    target/x86_64-apple-darwin/release/foxgit
else
  cargo build --release
  BIN=target/release/foxgit
fi

APP=target/release/FoxGit.app
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$BIN" "$APP/Contents/MacOS/foxgit"
cp assets/icon/FoxGit.icns "$APP/Contents/Resources/FoxGit.icns"
cp LICENSE THIRD-PARTY-LICENSES.txt "$APP/Contents/Resources/"

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key>               <string>FoxGit</string>
  <key>CFBundleDisplayName</key>        <string>FoxGit</string>
  <key>CFBundleIdentifier</key>         <string>dev.foxgit.FoxGit</string>
  <key>CFBundleExecutable</key>         <string>foxgit</string>
  <key>CFBundleIconFile</key>           <string>FoxGit</string>
  <key>CFBundlePackageType</key>        <string>APPL</string>
  <key>CFBundleShortVersionString</key> <string>${VERSION}</string>
  <key>CFBundleVersion</key>            <string>${VERSION}</string>
  <key>LSMinimumSystemVersion</key>     <string>11.0</string>
  <key>LSApplicationCategoryType</key>  <string>public.app-category.developer-tools</string>
  <key>NSHighResolutionCapable</key>    <true/>
  <key>NSHumanReadableCopyright</key>   <string>© 2026 FoxGit contributors. MIT License.</string>
</dict>
</plist>
PLIST

if [[ -n "${MACOS_SIGN_IDENTITY:-}" ]]; then
  codesign --force --options runtime --timestamp --sign "$MACOS_SIGN_IDENTITY" "$APP"
  echo "Signed with: $MACOS_SIGN_IDENTITY"
else
  # Ad-hoc signature: required on Apple Silicon, and lets macOS remember Keychain access.
  codesign --force --sign - "$APP"
  echo "Ad-hoc signed (set MACOS_SIGN_IDENTITY for a Developer ID signature)"
fi
codesign --verify --strict "$APP"
echo "Built $APP ($(lipo -archs "$APP/Contents/MacOS/foxgit"))"

notarize() {
  local file=$1
  if [[ -n "${MACOS_NOTARY_PROFILE:-}" ]]; then
    xcrun notarytool submit "$file" --keychain-profile "$MACOS_NOTARY_PROFILE" --wait
  elif [[ -n "${APPLE_ID:-}" && -n "${APPLE_TEAM_ID:-}" && -n "${APPLE_APP_PASSWORD:-}" ]]; then
    xcrun notarytool submit "$file" --apple-id "$APPLE_ID" --team-id "$APPLE_TEAM_ID" \
      --password "$APPLE_APP_PASSWORD" --wait
  else
    return 1
  fi
}

if [[ $DMG == 1 ]]; then
  OUT=target/release/FoxGit-${VERSION}-macos.dmg
  STAGE=$(mktemp -d)
  cp -R "$APP" "$STAGE/"
  ln -s /Applications "$STAGE/Applications"
  rm -f "$OUT"
  hdiutil create -volname "FoxGit ${VERSION}" -srcfolder "$STAGE" -ov -format UDZO "$OUT" >/dev/null
  rm -rf "$STAGE"
  if [[ -n "${MACOS_SIGN_IDENTITY:-}" ]]; then
    codesign --force --timestamp --sign "$MACOS_SIGN_IDENTITY" "$OUT"
    if notarize "$OUT"; then
      xcrun stapler staple "$OUT"
      echo "Notarized and stapled $OUT"
    else
      echo "Not notarized (no notary credentials set)"
    fi
  fi
  echo "Built $OUT"
fi
