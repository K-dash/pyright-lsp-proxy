# pyright-lsp-proxy

**Claude Code-specific** LSP proxy.

Claude Code cannot handle language server restarts or reconnections, so reflecting `.venv` creation or switching previously required restarting Claude Code itself.
pyright-lsp-proxy breaks through this limitation, reflecting virtual environment changes **within your running session**.

## Problems Solved

- **venv switching in monorepos**: Pyright assumes a single venv, causing incorrect type checking and completions when moving between projects
- **Dynamic .venv creation in worktrees**: When `.venv` is created later via hooks, etc., Claude Code restart was previously required
- **Transparent switch on venv change**: LSP requests (hover, definition, etc.) are sent to the new backend after a switch, so the current request does not surface "Request cancelled"

pyright-lsp-proxy restarts pyright-langserver in the background and automatically restores open documents. Claude Code always communicates with the proxy, so it doesn't notice backend switches.

## Requirements

### Supported OS

- macOS (arm64 only)
- Linux (x86_64 / arm64)

Windows is currently unsupported (due to path handling differences).
Intel macOS users must build from source (prebuilt binaries are arm64 only).

### Prerequisites

- Rust 1.75 or later (for building)
- `pyright-langserver` command available in PATH
- Git (used to determine `.venv` search boundary, works without it)

## Installation

> [!Note]
> Claude Code restart is required only for initial installation. After installation, `.venv` creation and switching no longer require restarts.

### Prerequisites

#### 1. Install pyright-langserver

This proxy requires `pyright-langserver` to be available in your PATH.

```bash
# Install via npm (recommended)
npm install -g pyright

# Or via pip
pip install pyright

# Verify installation
which pyright-langserver
```

#### 2. Disable Official pyright Plugin

> [!Important]
> You must disable the official pyright plugin. Having both enabled causes conflicts.

```bash
/plugin disable pyright-lsp@claude-plugins-official
```

### Method A: From GitHub Marketplace (Recommended)

> [!Note]
> Installation uses GitHub API and `curl`. It may fail in offline environments or under rate limiting.

```bash
# 1. Add marketplace
/plugin marketplace add K-dash/pyright-lsp-proxy

# 2. Install plugin
/plugin install pyright-lsp-proxy@pyright-lsp-proxy-marketplace

# 3. Restart Claude Code (initial installation only)
```

After installation, verify in `~/.claude/settings.json`:

```json
{
  "enabledPlugins": {
    "pyright-lsp@claude-plugins-official": false,
    "pyright-lsp-proxy@pyright-lsp-proxy-marketplace": true
  }
}
```

#### Update / Uninstall

```bash
# Update
/plugin update pyright-lsp-proxy@pyright-lsp-proxy-marketplace

# Uninstall
/plugin uninstall pyright-lsp-proxy@pyright-lsp-proxy-marketplace
/plugin marketplace remove pyright-lsp-proxy-marketplace
```

### Method B: Local Build (For Developers)

```bash
git clone https://github.com/K-dash/pyright-lsp-proxy.git
cd pyright-lsp-proxy
cargo build --release

/plugin marketplace add /path/to/pyright-lsp-proxy
/plugin install pyright-lsp-proxy@pyright-lsp-proxy-marketplace
# Restart Claude Code (initial installation only)
```

## Usage

Automatically starts as a Claude Code plugin. For manual execution:

```bash
./target/release/pyright-lsp-proxy
./target/release/pyright-lsp-proxy --help
```

### Logging

Default output is stderr. For file output:

```bash
PYRIGHT_LSP_PROXY_LOG_FILE=/tmp/pyright-lsp-proxy.log ./target/release/pyright-lsp-proxy
```

| Environment Variable         | Description   | Default                   |
| ---------------------------- | ------------- | ------------------------- |
| `PYRIGHT_LSP_PROXY_LOG_FILE` | Log file path | Not set (stderr only)     |
| `RUST_LOG`                   | Log level     | `pyright_lsp_proxy=debug` |

For config file method and details, see [ARCHITECTURE.md](./ARCHITECTURE.md).

## Typical Use Case

### Monorepo Structure

```
my-monorepo/
├── project-a/
│   ├── .venv/          # project-a specific virtual environment
│   └── src/main.py
├── project-b/
│   ├── .venv/          # project-b specific virtual environment
│   └── src/main.py
└── project-c/
    ├── .venv/          # project-c specific virtual environment
    └── src/main.py
```

### Operation Sequence

| Action                          | Proxy Behavior                                              |
| ------------------------------- | ----------------------------------------------------------- |
| 1. Start Claude Code            | Search for fallback .venv (start without venv if not found) |
| 2. Open `project-a/src/main.py` | Detect `project-a/.venv` → start session 1                  |
| 3. Open `project-b/src/main.py` | Detect `project-b/.venv` → switch to session 2              |
| 4. Session 2 startup complete   | Restore only documents under project-b                      |

### What Actually Happens

When you switch from `project-a/main.py` to `project-b/main.py`:

1. Proxy detects different `.venv` (project-a/.venv → project-b/.venv)
2. Gracefully shuts down old backend (session 1)
3. Spawns new backend with `VIRTUAL_ENV=project-b/.venv` (session 2)
4. Restores open documents under project-b/ to new backend
5. Clears diagnostics for documents outside project-b/
6. **All LSP requests now use project-b dependencies**

From the user's perspective: **Nothing visible happens. LSP just works.**

### Cache Limitation (Important)

If a file was opened before `.venv` existed, the cached venv stays `None`.
Create `.venv` later? You must reopen the file (or refresh the document cache)
to trigger venv detection for that file.

## Troubleshooting

### LSP Not Working

```bash
which pyright-langserver              # Check if in PATH
cat ~/.claude/settings.json | grep pyright  # Check plugin settings
tail -100 /tmp/pyright-lsp-proxy.log  # Check logs
```

### `.venv` Not Switching

- Verify `.venv/pyvenv.cfg` exists
- Verify file is within git repository
- If `.venv` was created later, reopen the target file (or trigger an LSP request like hover)
- Use `RUST_LOG=trace` for detailed logs

## Known Limitations

| Item                    | Limitation                                              | Workaround           |
| ----------------------- | ------------------------------------------------------- | -------------------- |
| Windows unsupported     | Path handling assumes Unix-like systems                 | Use WSL2             |
| macOS Intel unsupported | Prebuilt is arm64 only                                  | Use Apple Silicon    |
| Fixed venv name         | Only detects `.venv` (`venv`, `env` not supported)      | Rename to `.venv`    |
| Symlinks                | May fail to detect `pyvenv.cfg` if `.venv` is a symlink | Use actual directory |

## Architecture

For design philosophy, state transitions, and internal implementation details, see:

[ARCHITECTURE.md](./ARCHITECTURE.md)

## License

MIT
