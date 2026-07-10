# Privacy Policy

_Last updated: 2026-07-11_

typemux-cc is a local LSP proxy that runs entirely on your machine as a Claude Code plugin. This document describes what data it handles and where that data goes.

## Data collection

**typemux-cc collects no data.** There is no telemetry, no analytics, no crash reporting, no account, and no phone-home of any kind.

## What stays on your machine

- Your source code and file contents. Document text received over LSP (`didOpen`/`didChange`) is held in the proxy's memory only, for backend state restoration, and is discarded when the process exits.
- All LSP traffic. The proxy communicates with Claude Code over stdio and with language-server backends (pyright, ty, or pyrefly) over local process pipes. Nothing is sent over the network.
- Logs. Written to stderr, and to a local file only if you opt in via `TYPEMUX_CC_LOG_FILE`. Logs never leave your machine.
- Configuration. Read from `~/.config/typemux-cc/config` and environment variables, locally.

## Network access

The plugin makes exactly one kind of network request: at install time, the SessionStart hook downloads the prebuilt binary for your platform from this repository's GitHub Releases over HTTPS, pinned to the plugin's own version and verified against a published SHA256 checksum before execution (see [How the Installer Verifies Binaries](README.md#how-the-installer-verifies-binaries)). This request is served by GitHub and is subject to [GitHub's privacy statement](https://docs.github.com/en/site-policy/privacy-policies/github-privacy-statement). No other network requests are made by typemux-cc.

The proxy also invokes local `git` commands (e.g. `git rev-parse`, `git check-ignore`) against your repository; these do not access the network.

## Third-party components

The language-server backends (pyright, ty, pyrefly) are separate programs you install yourself. typemux-cc spawns them as local processes; their behavior is governed by their respective projects.

## Changes

Changes to this policy are made via pull requests to this repository and are visible in its history.

## Contact

Questions or concerns: [open an issue](https://github.com/K-dash/typemux-cc/issues).
