#!/bin/bash
# pyright-lsp-proxy wrapper script
# Loads configuration from ~/.config/pyright-lsp-proxy/config if it exists

CONFIG_FILE="$HOME/.config/pyright-lsp-proxy/config"
if [ -f "$CONFIG_FILE" ]; then
  # shellcheck disable=SC1090
  source "$CONFIG_FILE"
fi

# Launch the actual LSP proxy binary
exec "${CLAUDE_PLUGIN_ROOT}/bin/pyright-lsp-proxy" "$@"
