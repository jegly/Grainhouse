#!/bin/sh
# Builds Grainhouse and adds it to your app menu (just for you, nothing system-wide).
# Undo with: ./install.sh --remove
set -e
cd "$(dirname "$0")"
BIN="$HOME/.local/bin"
APPS="$HOME/.local/share/applications"
ICONS="$HOME/.local/share/icons/hicolor/scalable/apps"
ID=io.github.jegly.Grainhouse
if [ "$1" = "--remove" ]; then
    rm -f "$BIN/grainhouse" "$BIN/grainhouse-cli" "$APPS/$ID.desktop" "$ICONS/$ID.svg"
    update-desktop-database "$APPS" 2>/dev/null || true
    echo "Removed Grainhouse from your app menu."
    exit 0
fi
cargo build --release
mkdir -p "$BIN" "$APPS" "$ICONS"
install -m 755 target/release/grainhouse target/release/grainhouse-cli "$BIN/"
install -m 644 data/$ID.desktop "$APPS/"
install -m 644 data/$ID.svg "$ICONS/"
update-desktop-database "$APPS" 2>/dev/null || true
gtk-update-icon-cache -q "$HOME/.local/share/icons/hicolor" 2>/dev/null || true
echo "Installed. Grainhouse is in your app menu, and you can open photos with it from Files."
