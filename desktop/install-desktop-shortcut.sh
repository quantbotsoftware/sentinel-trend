#!/usr/bin/env bash
# Generates and installs the Linux .desktop shortcut using the current repo location.
set -euo pipefail
REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TEMPLATE="$REPO_DIR/desktop/sentinel-trend.desktop.in"
OUT_APPS="${XDG_DATA_HOME:-$HOME/.local/share}/applications"
mkdir -p "$OUT_APPS"
sed "s|@REPO_DIR@|$REPO_DIR|g" "$TEMPLATE" > "$OUT_APPS/sentinel-trend.desktop"
chmod +x "$OUT_APPS/sentinel-trend.desktop"
if [ -d "$HOME/Desktop" ]; then
  cp "$OUT_APPS/sentinel-trend.desktop" "$HOME/Desktop/sentinel-trend.desktop"
  chmod +x "$HOME/Desktop/sentinel-trend.desktop"
fi
echo "✔ Installed sentinel-trend.desktop pointing to $REPO_DIR"
