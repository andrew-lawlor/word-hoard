#!/usr/bin/env bash
# Fast iteration: builds the app and CLI incrementally (debug build, app ID
# io.github.andrew_lawlor.WordHoard.Devel, so it runs beside the Flatpak) and
# installs a "Word Hoard (Dev)" launcher for it. The dev app uses the
# Flatpak's library, so the same books and highlights are there.
#
#   scripts/dev.sh          build and (re)install the launcher
#   scripts/dev.sh run      build and start it from the terminal
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT=$(pwd)
LIBRARY="$HOME/.var/app/io.github.andrew_lawlor.WordHoard/data/word-hoard/library.db"

cargo build -p word-hoard -p word-hoard-cli

APPS="${XDG_DATA_HOME:-$HOME/.local/share}/applications"
mkdir -p "$APPS"
cat > "$APPS/io.github.andrew_lawlor.WordHoard.Devel.desktop" <<DESKTOP
[Desktop Entry]
Type=Application
Name=Word Hoard (Dev)
Comment=Latest local build of Word Hoard, with the Flatpak's library
Exec=env WORD_HOARD_LIBRARY=$LIBRARY $ROOT/target/debug/word-hoard
Icon=$ROOT/crates/word-hoard/data/io.github.andrew_lawlor.WordHoard.svg
Terminal=false
Categories=Office;Viewer;
StartupWMClass=io.github.andrew_lawlor.WordHoard.Devel
DESKTOP
update-desktop-database "$APPS" 2>/dev/null || true
echo "Built. Launch \"Word Hoard (Dev)\" from your app menu."

if [ "${1:-}" = run ]; then
    WORD_HOARD_LIBRARY="$LIBRARY" exec target/debug/word-hoard
fi
