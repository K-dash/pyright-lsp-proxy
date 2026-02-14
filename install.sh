#!/bin/bash
set -e

# Claude Plugin Root (from environment variable)
PLUGIN_ROOT="${CLAUDE_PLUGIN_ROOT}"
BIN_DIR="${PLUGIN_ROOT}/bin"
BINARY_PATH="${BIN_DIR}/typemux-cc"

# Skip if binary already exists
if [ -f "${BINARY_PATH}" ]; then
  echo "[typemux-cc] Binary already installed at ${BINARY_PATH}"
  exit 0
fi

echo "[typemux-cc] Installing binary..."

# Create bin directory
mkdir -p "${BIN_DIR}"

# Detect OS/architecture
OS=$(uname -s)
ARCH=$(uname -m)

case "$OS" in
  Darwin)
    if [ "$ARCH" = "arm64" ]; then
      BINARY_NAME="typemux-cc-macos-arm64"
    else
      echo "[typemux-cc] ERROR: Intel macOS is not supported" >&2
      echo "[typemux-cc] Only Apple Silicon (arm64) is supported on macOS" >&2
      exit 1
    fi
    ;;
  Linux)
    if [ "$ARCH" = "x86_64" ]; then
      BINARY_NAME="typemux-cc-linux-x86_64"
    elif [ "$ARCH" = "aarch64" ] || [ "$ARCH" = "arm64" ]; then
      BINARY_NAME="typemux-cc-linux-arm64"
    else
      echo "[typemux-cc] ERROR: Unsupported Linux architecture: $ARCH" >&2
      echo "[typemux-cc] Supported Linux architectures: x86_64, arm64" >&2
      exit 1
    fi
    ;;
  *)
    echo "[typemux-cc] ERROR: Unsupported platform: $OS" >&2
    echo "[typemux-cc] Supported platforms: macOS (arm64), Linux (x86_64)" >&2
    exit 1
    ;;
esac

echo "[typemux-cc] Detected platform: $OS $ARCH"
echo "[typemux-cc] Binary to download: $BINARY_NAME"

# Get latest version URL from GitHub Release
REPO="K-dash/pyright-lsp-proxy"
LATEST_RELEASE=$(curl -s "https://api.github.com/repos/${REPO}/releases/latest")
DOWNLOAD_URL=$(echo "$LATEST_RELEASE" | grep "browser_download_url.*${BINARY_NAME}" | cut -d '"' -f 4)

if [ -z "$DOWNLOAD_URL" ]; then
  echo "[typemux-cc] ERROR: Failed to find binary for ${BINARY_NAME}" >&2
  echo "[typemux-cc] Please check https://github.com/${REPO}/releases for available binaries" >&2
  exit 1
fi

echo "[typemux-cc] Downloading from: $DOWNLOAD_URL"

# Download and grant execute permission
if ! curl -L -o "${BINARY_PATH}" "$DOWNLOAD_URL"; then
  echo "[typemux-cc] ERROR: Failed to download binary" >&2
  exit 1
fi

chmod +x "${BINARY_PATH}"

# Copy wrapper script
WRAPPER_SRC="${PLUGIN_ROOT}/bin/typemux-cc-wrapper.sh"
WRAPPER_DST="${BIN_DIR}/typemux-cc-wrapper.sh"

if [ -f "${WRAPPER_SRC}" ]; then
  cp "${WRAPPER_SRC}" "${WRAPPER_DST}"
  chmod +x "${WRAPPER_DST}"
  echo "[typemux-cc] Wrapper script installed at ${WRAPPER_DST}"
fi

echo "[typemux-cc] Successfully installed to ${BINARY_PATH}"
echo "[typemux-cc] Version:"
"${BINARY_PATH}" --version || true
