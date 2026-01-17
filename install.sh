#!/bin/bash
set -e

# Claude Plugin Root（環境変数から取得）
PLUGIN_ROOT="${CLAUDE_PLUGIN_ROOT}"
BIN_DIR="${PLUGIN_ROOT}/bin"
BINARY_PATH="${BIN_DIR}/pyright-lsp-proxy"

# バイナリが既に存在すれば何もしない
if [ -f "${BINARY_PATH}" ]; then
  echo "[pyright-lsp-proxy] Binary already installed at ${BINARY_PATH}"
  exit 0
fi

echo "[pyright-lsp-proxy] Installing binary..."

# bin ディレクトリを作成
mkdir -p "${BIN_DIR}"

# OS/アーキテクチャの検出
OS=$(uname -s)
ARCH=$(uname -m)

case "$OS" in
  Darwin)
    if [ "$ARCH" = "arm64" ]; then
      BINARY_NAME="pyright-lsp-proxy-macos-arm64"
    else
      echo "[pyright-lsp-proxy] ERROR: Intel macOS is not supported" >&2
      echo "[pyright-lsp-proxy] Only Apple Silicon (arm64) is supported on macOS" >&2
      exit 1
    fi
    ;;
  Linux)
    BINARY_NAME="pyright-lsp-proxy-linux-x86_64"
    ;;
  *)
    echo "[pyright-lsp-proxy] ERROR: Unsupported platform: $OS" >&2
    echo "[pyright-lsp-proxy] Supported platforms: macOS (arm64), Linux (x86_64)" >&2
    exit 1
    ;;
esac

echo "[pyright-lsp-proxy] Detected platform: $OS $ARCH"
echo "[pyright-lsp-proxy] Binary to download: $BINARY_NAME"

# GitHub Release から最新バージョンの URL を取得
REPO="K-dash/pyright-lsp-proxy"
LATEST_RELEASE=$(curl -s "https://api.github.com/repos/${REPO}/releases/latest")
DOWNLOAD_URL=$(echo "$LATEST_RELEASE" | grep "browser_download_url.*${BINARY_NAME}" | cut -d '"' -f 4)

if [ -z "$DOWNLOAD_URL" ]; then
  echo "[pyright-lsp-proxy] ERROR: Failed to find binary for ${BINARY_NAME}" >&2
  echo "[pyright-lsp-proxy] Please check https://github.com/${REPO}/releases for available binaries" >&2
  exit 1
fi

echo "[pyright-lsp-proxy] Downloading from: $DOWNLOAD_URL"

# ダウンロードして実行権限を付与
if ! curl -L -o "${BINARY_PATH}" "$DOWNLOAD_URL"; then
  echo "[pyright-lsp-proxy] ERROR: Failed to download binary" >&2
  exit 1
fi

chmod +x "${BINARY_PATH}"

# wrapper スクリプトもコピー
WRAPPER_SRC="${PLUGIN_ROOT}/bin/pyright-lsp-proxy-wrapper.sh"
WRAPPER_DST="${BIN_DIR}/pyright-lsp-proxy-wrapper.sh"

if [ -f "${WRAPPER_SRC}" ]; then
  cp "${WRAPPER_SRC}" "${WRAPPER_DST}"
  chmod +x "${WRAPPER_DST}"
  echo "[pyright-lsp-proxy] Wrapper script installed at ${WRAPPER_DST}"
fi

echo "[pyright-lsp-proxy] Successfully installed to ${BINARY_PATH}"
echo "[pyright-lsp-proxy] Version:"
"${BINARY_PATH}" --version || true
