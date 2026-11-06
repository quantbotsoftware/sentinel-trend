#!/bin/bash
# Install the CUDA toolkit needed by candle's `cuda` feature on a
# Pop!_OS 24.04 / Ubuntu 24.04 box with an RTX 5060 (Blackwell, sm_120).
#
# Why NOT `apt install nvidia-cuda-toolkit`:
#   The Pop!_OS / Ubuntu 24.04 apt repo ships CUDA 12.0, which lacks
#   sm_120 support. `nvcc` would compile but kernels would fail to
#   launch on the RTX 5060 at runtime. Blackwell needs CUDA 12.8 or
#   newer; this script installs 12.9 (current stable) from NVIDIA's
#   official Ubuntu 24.04 repo.
#
# Run:  sudo bash tools/install-cuda.sh

set -euo pipefail

if [ "$EUID" -ne 0 ]; then
    echo "ERROR: this script must be run as root."
    echo "       sudo bash tools/install-cuda.sh"
    exit 1
fi

CUDA_VERSION="12-9"
CUDA_DIR="/usr/local/cuda-12.9"
KEYRING_URL="https://developer.download.nvidia.com/compute/cuda/repos/ubuntu2404/x86_64/cuda-keyring_1.1-1_all.deb"
TARGET_USER="${SUDO_USER:-tony}"

echo "=== Detecting GPU + driver ==="
if ! command -v nvidia-smi >/dev/null 2>&1; then
    echo "WARNING: nvidia-smi not found. Install the proprietary driver first."
fi
nvidia-smi --query-gpu=name,driver_version,memory.total --format=csv,noheader 2>/dev/null \
    || echo "(nvidia-smi unavailable; continuing)"

echo
echo "=== Adding NVIDIA Ubuntu 24.04 CUDA repo ==="
TMP_KEYRING="/tmp/cuda-keyring_1.1-1_all.deb"
if [ ! -f "$TMP_KEYRING" ]; then
    wget -q -O "$TMP_KEYRING" "$KEYRING_URL"
fi
dpkg -i "$TMP_KEYRING"

echo
echo "=== apt update (may take a moment) ==="
apt update

echo
echo "=== Installing cuda-toolkit-${CUDA_VERSION} (~3 GB download) ==="
DEBIAN_FRONTEND=noninteractive apt install -y "cuda-toolkit-${CUDA_VERSION}"

echo
echo "=== Verifying installation ==="
if [ -x "${CUDA_DIR}/bin/nvcc" ]; then
    "${CUDA_DIR}/bin/nvcc" --version | head -5
    echo "Supported architectures (excerpt):"
    "${CUDA_DIR}/bin/nvcc" --help 2>&1 \
        | grep -oE 'sm_[0-9]+' | sort -u | tr '\n' ' '
    echo
else
    echo "ERROR: nvcc not found at ${CUDA_DIR}/bin/nvcc"
    exit 1
fi

echo
echo "=== Wiring environment for ${TARGET_USER} ==="
HOME_DIR=$(eval echo "~${TARGET_USER}")
PROFILE="${HOME_DIR}/.bashrc"
MARKER="# >>> SentinelTrend CUDA env <<<"
if ! grep -qF "$MARKER" "$PROFILE" 2>/dev/null; then
    cat >> "$PROFILE" <<EOF

$MARKER
export CUDA_HOME=${CUDA_DIR}
export PATH=\$CUDA_HOME/bin:\$PATH
export LD_LIBRARY_PATH=\$CUDA_HOME/lib64:\$LD_LIBRARY_PATH
# <<< SentinelTrend CUDA env >>>
EOF
    echo "Appended CUDA_HOME / PATH / LD_LIBRARY_PATH to ${PROFILE}"
else
    echo "${PROFILE} already has the CUDA env block, skipping append"
fi

echo
echo "=== Done ==="
echo "Next:"
echo "  1. Open a new terminal (or 'source ~/.bashrc') so CUDA_HOME is set."
echo "  2. cd ${SENTINEL_TREND_ROOT:-.} && bash build.sh --cuda"
echo "  3. Launch the app and look for"
echo "       'ML accelerator: CUDA available — OMEGA training will use the GPU'"
echo "     in the boot logs."
echo "  4. From the OMEGA UI tab → 'Force Retrain'. Logs will report"
echo "       'LSTM trainer using CUDA' / 'regime_xfmr trainer using CUDA'."
