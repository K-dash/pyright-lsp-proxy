#!/usr/bin/env bash
set -e

# Claude Plugin Root (from environment variable)
PLUGIN_ROOT="${CLAUDE_PLUGIN_ROOT}"
BIN_DIR="${PLUGIN_ROOT}/bin"
BINARY_PATH="${BIN_DIR}/typemux-cc"
REPO="K-dash/typemux-cc"

# The expected version comes from the plugin manifest, so the installed
# binary always matches the plugin version (never whatever "latest" is).
EXPECTED_VERSION=$(sed -n 's/.*"version"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' \
  "${PLUGIN_ROOT}/.claude-plugin/plugin.json" | head -1)
if [ -z "${EXPECTED_VERSION}" ]; then
  echo "[typemux-cc] ERROR: cannot read version from ${PLUGIN_ROOT}/.claude-plugin/plugin.json" >&2
  exit 1
fi

binary_version() {
  "$1" --version 2>/dev/null | awk '{print $2}'
}

file_sha256() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  else
    shasum -a 256 "$1" | awk '{print $1}'
  fi
}

# Skip only when the existing binary matches the plugin version. A bare
# existence check is not enough: a gitignored binary left in the
# marketplace clone gets copied into every new plugin version's cache
# directory and would shadow newer releases forever.
if [ -f "${BINARY_PATH}" ]; then
  CURRENT_VERSION=$(binary_version "${BINARY_PATH}" || true)
  if [ "${CURRENT_VERSION}" = "${EXPECTED_VERSION}" ]; then
    echo "[typemux-cc] Binary ${EXPECTED_VERSION} already installed at ${BINARY_PATH}"
    exit 0
  fi
  echo "[typemux-cc] Installed binary is ${CURRENT_VERSION:-unknown}, expected ${EXPECTED_VERSION} — reinstalling"
fi

echo "[typemux-cc] Installing binary ${EXPECTED_VERSION}..."

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

# Download the asset for the plugin's own version tag (no "latest" API
# call: avoids GitHub API rate limits and version skew).
DOWNLOAD_URL="https://github.com/${REPO}/releases/download/v${EXPECTED_VERSION}/${BINARY_NAME}"
echo "[typemux-cc] Downloading from: ${DOWNLOAD_URL}"

# Download to a temp path and verify before replacing the binary, keeping
# a working (if outdated) binary usable when the download fails. The
# degradation is explicit: a WARNING is always printed.
TMP_PATH="${BINARY_PATH}.download"
if ! curl -fsSL -o "${TMP_PATH}" "${DOWNLOAD_URL}"; then
  rm -f "${TMP_PATH}"
  if [ -f "${BINARY_PATH}" ]; then
    echo "[typemux-cc] WARNING: download failed; keeping existing binary ($(binary_version "${BINARY_PATH}" || echo unknown))" >&2
    exit 0
  fi
  echo "[typemux-cc] ERROR: Failed to download binary from ${DOWNLOAD_URL}" >&2
  echo "[typemux-cc] Please check https://github.com/${REPO}/releases for available binaries" >&2
  exit 1
fi

# Verify integrity against the release's checksum asset BEFORE executing
# the downloaded binary. No checksum, no install.
EXPECTED_SHA=$(curl -fsSL "${DOWNLOAD_URL}.sha256" 2>/dev/null | awk '{print $1}' || true)
ACTUAL_SHA=$(file_sha256 "${TMP_PATH}")
if [ -z "${EXPECTED_SHA}" ] || [ "${ACTUAL_SHA}" != "${EXPECTED_SHA}" ]; then
  rm -f "${TMP_PATH}"
  if [ -f "${BINARY_PATH}" ]; then
    echo "[typemux-cc] WARNING: checksum verification failed (expected ${EXPECTED_SHA:-unavailable}, got ${ACTUAL_SHA}); keeping existing binary" >&2
    exit 0
  fi
  echo "[typemux-cc] ERROR: checksum verification failed for ${DOWNLOAD_URL} (expected ${EXPECTED_SHA:-unavailable}, got ${ACTUAL_SHA})" >&2
  exit 1
fi

chmod +x "${TMP_PATH}"

DOWNLOADED_VERSION=$(binary_version "${TMP_PATH}" || true)
if [ "${DOWNLOADED_VERSION}" != "${EXPECTED_VERSION}" ]; then
  rm -f "${TMP_PATH}"
  if [ -f "${BINARY_PATH}" ]; then
    echo "[typemux-cc] WARNING: downloaded binary reports ${DOWNLOADED_VERSION:-unknown}, expected ${EXPECTED_VERSION}; keeping existing binary" >&2
    exit 0
  fi
  echo "[typemux-cc] ERROR: downloaded binary reports ${DOWNLOADED_VERSION:-unknown}, expected ${EXPECTED_VERSION}" >&2
  exit 1
fi

mv "${TMP_PATH}" "${BINARY_PATH}"

# The wrapper script ships with the plugin (tracked in bin/); just make
# sure it is executable.
if [ -f "${BIN_DIR}/typemux-cc-wrapper.sh" ]; then
  chmod +x "${BIN_DIR}/typemux-cc-wrapper.sh"
fi

echo "[typemux-cc] Successfully installed ${EXPECTED_VERSION} to ${BINARY_PATH}"
