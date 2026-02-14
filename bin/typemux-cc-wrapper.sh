#!/bin/bash
# typemux-cc wrapper script
# Loads configuration from ~/.config/typemux-cc/config if it exists

CONFIG_FILE="$HOME/.config/typemux-cc/config"
if [ -f "$CONFIG_FILE" ]; then
  # shellcheck disable=SC1090
  source "$CONFIG_FILE"
fi

# Launch the actual LSP proxy binary
exec "${CLAUDE_PLUGIN_ROOT}/bin/typemux-cc" "$@"
