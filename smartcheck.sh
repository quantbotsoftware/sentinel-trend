#!/bin/bash
set -euo pipefail

PROJECT_DIR="${SENTINEL_TREND_ROOT:-.}"
APP_NAME="sentinel-trend"
BUILD_SCRIPT="$PROJECT_DIR/build.sh"
DESKTOP_FILE="$HOME/.local/share/applications/${APP_NAME}.desktop"
LAUNCHER="$HOME/.local/share/${APP_NAME}/launch-${APP_NAME}.sh"
BINARY="$PROJECT_DIR/src-tauri/target/release/${APP_NAME}"
ICON_PREFIX="$HOME/.local/share/${APP_NAME}/icons/${APP_NAME}-"

RUN_BUILD=1
LAUNCH_APP=1

usage() {
    cat <<'EOF'
Usage: bash smartcheck.sh [--no-build] [--no-launch]

Default behavior:
  1. Run build.sh
  2. Verify desktop -> launcher -> binary -> icon
  3. Launch the app through the same launcher used by the desktop icon

Options:
  --no-build   Skip running build.sh
  --no-launch  Skip launching the app after verification
EOF
}

fail() {
    echo "ERROR: $1" >&2
    exit 1
}

ok() {
    echo "OK: $1"
}

for arg in "$@"; do
    case "$arg" in
        --no-build) RUN_BUILD=0 ;;
        --no-launch) LAUNCH_APP=0 ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            fail "Unknown argument: $arg"
            ;;
    esac
done

if [ "$RUN_BUILD" -eq 1 ]; then
    bash "$BUILD_SCRIPT"
fi

[ -f "$DESKTOP_FILE" ] || fail "Desktop file missing: $DESKTOP_FILE"
[ -x "$LAUNCHER" ] || fail "Launcher missing or not executable: $LAUNCHER"
[ -x "$BINARY" ] || fail "Binary missing or not executable: $BINARY"

EXEC_LINE="$(sed -n 's/^Exec=//p' "$DESKTOP_FILE")"
ICON_LINE="$(sed -n 's/^Icon=//p' "$DESKTOP_FILE")"
BINARY_LINE="$(sed -n 's/^BINARY=\"\(.*\)\"/\1/p' "$LAUNCHER")"

[ -n "$EXEC_LINE" ] || fail "Exec line missing in $DESKTOP_FILE"
[ -n "$ICON_LINE" ] || fail "Icon line missing in $DESKTOP_FILE"
[ -n "$BINARY_LINE" ] || fail "BINARY line missing in $LAUNCHER"

echo "DESKTOP EXEC  : $EXEC_LINE"
echo "LAUNCHER BIN  : $BINARY_LINE"
echo "DESKTOP ICON  : $ICON_LINE"

[ "$EXEC_LINE" = "$LAUNCHER" ] || fail "Desktop entry does not point to launcher"
ok "desktop -> launcher"

[ "$BINARY_LINE" = "$BINARY" ] || fail "Launcher does not point to release binary"
ok "launcher -> release binary"

[ -f "$ICON_LINE" ] || fail "Icon file does not exist: $ICON_LINE"
[[ "$ICON_LINE" == ${ICON_PREFIX}*.png ]] || fail "Desktop icon is not versioned under $ICON_PREFIX"
ok "icon exists and is versioned"

if [ "$RUN_BUILD" -eq 1 ]; then
    NOW_TS="$(date +%s)"
    DESKTOP_TS="$(stat -c %Y "$DESKTOP_FILE")"
    ICON_TS="$(stat -c %Y "$ICON_LINE")"
    if (( NOW_TS - DESKTOP_TS > 120 )); then
        fail "Desktop file does not look freshly updated by the latest build"
    fi
    if (( NOW_TS - ICON_TS > 120 )); then
        fail "Icon file does not look freshly updated by the latest build"
    fi
    ok "desktop and icon were refreshed by latest build"
fi

if [ "$LAUNCH_APP" -eq 1 ]; then
    "$LAUNCHER" >/dev/null 2>&1 &
    sleep 2
    RUNNING="$(ps -eo pid,cmd | awk -v bin="$BINARY" '$0 ~ bin && $0 !~ /awk/ {print}')"
    [ -n "$RUNNING" ] || fail "Launcher did not start the expected release binary"
    ok "launcher started updated app"
    echo "RUNNING PROC  :"
    echo "$RUNNING"
else
    ok "static checks passed (launch skipped)"
fi
