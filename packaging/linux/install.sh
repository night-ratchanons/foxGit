#!/usr/bin/env sh
# Install FoxGit for the current user (no root needed): ~/.local/bin + app menu entry.
set -e
cd "$(dirname "$0")"
BIN="$HOME/.local/bin"
APPS="${XDG_DATA_HOME:-$HOME/.local/share}/applications"
ICONS="${XDG_DATA_HOME:-$HOME/.local/share}/icons/hicolor/512x512/apps"
mkdir -p "$BIN" "$APPS" "$ICONS"
install -m 755 foxgit "$BIN/foxgit"
install -m 644 foxgit.png "$ICONS/foxgit.png"
sed "s|^Exec=foxgit|Exec=$BIN/foxgit|" foxgit.desktop > "$APPS/foxgit.desktop"
command -v update-desktop-database >/dev/null && update-desktop-database "$APPS" || true
echo "FoxGit installed. Start it from your app menu or run: $BIN/foxgit"
case ":$PATH:" in *":$BIN:"*) ;; *) echo "(add $BIN to your PATH to run 'foxgit' from a terminal)";; esac
