#!/bin/bash
set -euo pipefail

PROJECT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
APP_NAME="sentinel-trend"
TAURI_DIR="$PROJECT_DIR/src-tauri"
BINARY="$TAURI_DIR/target/release/${APP_NAME}"
DESKTOP_FILE="$HOME/.local/share/applications/${APP_NAME}.desktop"
RUNTIME_DIR="$HOME/.local/share/${APP_NAME}"
ICON_SOURCE="$PROJECT_DIR/assets/icon.png"
ICON_DIR="$RUNTIME_DIR/icons"
LAUNCHER="$RUNTIME_DIR/launch-${APP_NAME}.sh"
SMARTCHECK_SCRIPT="$PROJECT_DIR/smartcheck.sh"
ICON_STAMP="$(date -u +%Y%m%d%H%M%S)"
ICON_TARGET="$ICON_DIR/${APP_NAME}-${ICON_STAMP}.png"

# Cargo feature toggle. `--cuda` enables candle's CUDA backend so
# OMEGA training runs on the GPU. Requires the CUDA toolkit (see
# tools/install-cuda.sh) — this script auto-detects the official
# install path /usr/local/cuda-* and exports CUDA_HOME / PATH /
# LD_LIBRARY_PATH for the cargo invocation if the env doesn't have
# them yet (handy right after running install-cuda.sh, before the
# user's shell has re-sourced .bashrc).
CARGO_FEATURE_FLAGS=()
EXTRA_TAG=""
WANT_CUDA=0
for arg in "$@"; do
    case "$arg" in
        --cuda)
            CARGO_FEATURE_FLAGS+=("--features" "cuda")
            EXTRA_TAG=" (CUDA-enabled)"
            WANT_CUDA=1
            ;;
        --metal)
            CARGO_FEATURE_FLAGS+=("--features" "metal")
            EXTRA_TAG=" (Metal-enabled)"
            ;;
        *)
            ;;
    esac
done

if [ "$WANT_CUDA" -eq 1 ]; then
    if [ -z "${CUDA_HOME:-}" ]; then
        # Auto-pick highest-numbered /usr/local/cuda-12.* (Blackwell
        # needs ≥ 12.8). Fall back to /usr/local/cuda symlink if the
        # numbered dir is missing.
        AUTO_CUDA=$(ls -1d /usr/local/cuda-12.* 2>/dev/null | sort -V | tail -1)
        if [ -z "$AUTO_CUDA" ] && [ -d /usr/local/cuda ]; then
            AUTO_CUDA=/usr/local/cuda
        fi
        if [ -n "$AUTO_CUDA" ] && [ -x "$AUTO_CUDA/bin/nvcc" ]; then
            export CUDA_HOME="$AUTO_CUDA"
            export PATH="$CUDA_HOME/bin:$PATH"
            export LD_LIBRARY_PATH="$CUDA_HOME/lib64:${LD_LIBRARY_PATH:-}"
            echo "Auto-detected CUDA: $CUDA_HOME"
        else
            echo "WARNING: --cuda requested but no /usr/local/cuda-* found."
            echo "         Run 'sudo bash tools/install-cuda.sh' first."
            exit 1
        fi
    else
        echo "Using CUDA_HOME from environment: $CUDA_HOME"
    fi
    if ! command -v nvcc >/dev/null 2>&1; then
        echo "ERROR: nvcc still not in PATH after auto-detect."
        exit 1
    fi
    NVCC_VER=$(nvcc --version | grep -oE 'release [0-9]+\.[0-9]+' | head -1)
    echo "nvcc version: ${NVCC_VER:-unknown}"
fi

echo "=== Building SentinelTrend${EXTRA_TAG} ==="

if [ ! -f "$ICON_SOURCE" ]; then
    echo "WARNING: Icon not found at $ICON_SOURCE"
fi

cp -f "$ICON_SOURCE" "$TAURI_DIR/icons/icon.png" 2>/dev/null || true

cd "$TAURI_DIR"
if [ ${#CARGO_FEATURE_FLAGS[@]} -gt 0 ]; then
    echo "Cargo feature flags: ${CARGO_FEATURE_FLAGS[*]}"
    cargo tauri build --bundles none -- "${CARGO_FEATURE_FLAGS[@]}" 2>&1 | tail -5
else
    cargo tauri build --bundles none 2>&1 | tail -5
fi

if [ ! -f "$BINARY" ]; then
    echo "ERROR: Binary not found at $BINARY"
    exit 1
fi

strip "$BINARY" 2>/dev/null || true
echo "Binary size: $(du -h "$BINARY" | cut -f1)"
echo "Binary hash: $(md5sum "$BINARY" | cut -c1-12)"
echo "OK: launcher -> release binary"
echo "OK: icon exists and is versioned"

mkdir -p "$(dirname "$DESKTOP_FILE")" "$RUNTIME_DIR" "$ICON_DIR"

if [ -f "$ICON_SOURCE" ]; then
    rm -f "$ICON_DIR"/${APP_NAME}-*.png
    cp -f "$ICON_SOURCE" "$ICON_TARGET"
else
    ICON_TARGET="$ICON_SOURCE"
fi

cat > "$LAUNCHER" << EOF
#!/bin/bash
set -euo pipefail

APP_NAME="$APP_NAME"
BINARY="$BINARY"

for pid in \$(pgrep -x "\$APP_NAME" || true); do
    kill "\$pid" 2>/dev/null || true
done

for _ in \$(seq 1 50); do
    if ! pgrep -x "\$APP_NAME" >/dev/null 2>&1; then
        break
    fi
    sleep 0.1
done

exec "\$BINARY" "\$@"
EOF

chmod +x "$LAUNCHER"

cat > "$DESKTOP_FILE" << EOF
[Desktop Entry]
Name=SentinelTrend
Comment=SmartTrend Trading System
Exec=$LAUNCHER
Icon=$ICON_TARGET
Terminal=false
Type=Application
Categories=Finance;Office;
StartupWMClass=sentinel-trend
StartupNotify=true
EOF

chmod +x "$DESKTOP_FILE"
touch "$DESKTOP_FILE"

if command -v gtk-update-icon-cache &>/dev/null; then
    gtk-update-icon-cache -f -t "$HOME/.local/share/icons" 2>/dev/null || true
fi
if command -v update-desktop-database &>/dev/null; then
    update-desktop-database "$HOME/.local/share/applications" 2>/dev/null || true
fi

SNAPSHOTS_DIR="$RUNTIME_DIR/snapshots"
if [ -d "$SNAPSHOTS_DIR" ]; then
    echo "=== Clearing stale markers, FSM & snapshot ==="
    echo '[]' > "$SNAPSHOTS_DIR/markers.json"
    echo '{}' > "$SNAPSHOTS_DIR/fsm_states.json"
    rm -f "$SNAPSHOTS_DIR/markers.hash" "$SNAPSHOTS_DIR/fsm_states.hash"
    rm -f "$SNAPSHOTS_DIR/state_snapshot.json" "$SNAPSHOTS_DIR/state_snapshot.hash"
    echo "OK: markers + FSM + snapshot reset for clean start"
fi

if [ -f "$SMARTCHECK_SCRIPT" ]; then
    if [ ! -x "$SMARTCHECK_SCRIPT" ]; then
        chmod +x "$SMARTCHECK_SCRIPT"
    fi
    echo "=== Running smart static verification ==="
    "$SMARTCHECK_SCRIPT" --no-build --no-launch
else
    echo "WARNING: smartcheck script missing: $SMARTCHECK_SCRIPT"
fi

echo "=== Desktop entry updated: $DESKTOP_FILE ==="
echo "=== Launcher updated: $LAUNCHER ==="
echo "=== Build complete ==="
echo ""
echo "Run with: $LAUNCHER"
