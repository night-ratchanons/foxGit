#!/usr/bin/env bash
# Regenerate the PNG / .icns app icons from assets/icon/foxgit.svg.
# Needs `resvg` (cargo install resvg) and, for .icns, macOS `iconutil`.
set -euo pipefail
cd "$(dirname "$0")/.."
RESVG=${RESVG:-resvg}
SVG=assets/icon/foxgit.svg
OUT=assets/icon

"$RESVG" -w 1024 "$SVG" "$OUT/foxgit-1024.png"
"$RESVG" -w 512 "$SVG" "$OUT/foxgit-512.png"   # embedded window / Dock icon
"$RESVG" -w 256 "$SVG" "$OUT/foxgit-256.png"   # Linux hicolor icon

# Windows .ico (embedded into foxgit.exe by build.rs)
TMP=$(mktemp -d)
for s in 16 24 32 48 64 128 256; do
  "$RESVG" -w "$s" "$SVG" "$TMP/$s.png"
done
python3 scripts/png2ico.py "$OUT/foxgit.ico" "$TMP"/{16,24,32,48,64,128,256}.png

if command -v iconutil >/dev/null; then
  SET=$(mktemp -d)/FoxGit.iconset
  mkdir -p "$SET"
  for s in 16 32 128 256 512; do
    "$RESVG" -w "$s" "$SVG" "$SET/icon_${s}x${s}.png"
    "$RESVG" -w $((s * 2)) "$SVG" "$SET/icon_${s}x${s}@2x.png"
  done
  iconutil -c icns "$SET" -o "$OUT/FoxGit.icns"
fi
echo "icons written to $OUT"
