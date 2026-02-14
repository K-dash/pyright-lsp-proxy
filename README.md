<div align="center">

# typemux-cc

**Python type-checker LSP multiplexer for Claude Code — pyright, ty, pyrefly**

<div align="center">
  <a href="https://github.com/K-dash/typemux-cc/graphs/commit-activity"><img alt="GitHub commit activity" src="https://img.shields.io/github/commit-activity/m/K-dash/typemux-cc"/></a>
  <a href="https://github.com/K-dash/typemux-cc/blob/main/LICENSE"><img alt="License" src="https://img.shields.io/badge/LICENSE-MIT-green"/></a>
  <a href="https://www.rust-lang.org/"><img alt="Rust" src="https://img.shields.io/badge/rust-1.75+-orange.svg"/></a>
</div>

<p>
  <a href="#quickstart">Quickstart</a>
  ◆ <a href="#problems-solved">Problems Solved</a>
  ◆ <a href="#supported-backends">Backends</a>
  ◆ <a href="#installation">Installation</a>
  ◆ <a href="#typical-use-case">Typical Use Case</a>
  ◆ <a href="#architecture">Architecture</a>
</p>

</div>

---

Claude Code cannot handle language server restarts or reconnections, so reflecting `.venv` creation or switching previously required restarting Claude Code itself.
typemux-cc breaks through this limitation, reflecting virtual environment changes **within your running session**.

## Quickstart

```bash
# 1. Install a backend (pyright recommended)
npm install -g pyright

# 2. Disable the official pyright plugin
/plugin disable pyright-lsp@claude-plugins-official

# 3. Add marketplace and install
/plugin marketplace add K-dash/typemux-cc
/plugin install typemux-cc@typemux-cc-marketplace

# 4. Restart Claude Code (initial installation only)
```

> For **ty/pyrefly**, set `TYPEMUX_CC_BACKEND` in your [config](#configuration).

## Problems Solved

- **🔄 venv switching in monorepos** - Python type-checkers assume a single venv, causing incorrect type checking and completions when moving between projects
- **⚡ Dynamic .venv creation in worktrees** - When `.venv` is created later via hooks, etc., Claude Code restart was previously required
- **🔀 Transparent switch on venv change** - LSP requests (hover, definition, etc.) are sent to the new backend after a switch, so the current request does not surface "Request cancelled"

typemux-cc manages a pool of LSP backends (one per `.venv`) and routes requests to the correct one. Claude Code always communicates with the proxy, so it doesn't notice backend switches.

## Supported Backends

| Backend | Command | Status |
|---------|---------|--------|
| [pyright](https://github.com/microsoft/pyright) | `pyright-langserver --stdio` | ✅ Stable (default) |
| [ty](https://github.com/astral-sh/ty) | `ty server` | 🧪 Experimental (verified) |
| [pyrefly](https://github.com/facebook/pyrefly) | `pyrefly lsp` | 🧪 Experimental (verified) |

Select with `--backend` flag or `TYPEMUX_CC_BACKEND` environment variable:

```bash
typemux-cc --backend pyright   # default
typemux-cc --backend ty
typemux-cc --backend pyrefly
```

## Requirements

### Supported OS

| Platform | Architecture |
|----------|--------------|
| macOS | arm64 only |
| Linux | x86_64 / arm64 |

> **Note**: Windows is currently unsupported (due to path handling differences).
> Intel macOS users must build from source (prebuilt binaries are arm64 only).

### Prerequisites

- Rust 1.75 or later (for building)
- One of the supported LSP backends available in PATH:
  - `pyright-langserver` (install via `npm install -g pyright` or `pip install pyright`)
  - `ty` (install via `pip install ty` or `uvx ty`)
  - `pyrefly` (install via `pip install pyrefly`)
- Git (used to determine `.venv` search boundary, works without it)

## Installation

> [!Note]
> Claude Code restart is required only for initial installation. After installation, `.venv` creation and switching no longer require restarts.

### Prerequisites

#### 1. Install your preferred LSP backend

```bash
# pyright (default, recommended)
npm install -g pyright

# ty (experimental — by the creators of uv)
pip install ty

# pyrefly (experimental — by Meta)
pip install pyrefly
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
/plugin marketplace add K-dash/typemux-cc

# 2. Install plugin
/plugin install typemux-cc@typemux-cc-marketplace

# 3. Restart Claude Code (initial installation only)
```

After installation, verify in `~/.claude/settings.json`:

```json
{
  "enabledPlugins": {
    "pyright-lsp@claude-plugins-official": false,
    "typemux-cc@typemux-cc-marketplace": true
  }
}
```

#### Update / Uninstall

```bash
# Update
/plugin update typemux-cc@typemux-cc-marketplace

# Uninstall
/plugin uninstall typemux-cc@typemux-cc-marketplace
/plugin marketplace remove typemux-cc-marketplace
```

### Method B: Local Build (For Developers)

```bash
git clone https://github.com/K-dash/typemux-cc.git
cd typemux-cc
cargo build --release

/plugin marketplace add /path/to/typemux-cc
/plugin install typemux-cc@typemux-cc-marketplace
# Restart Claude Code (initial installation only)
```

## Usage

Automatically starts as a Claude Code plugin. For manual execution:

```bash
./target/release/typemux-cc
./target/release/typemux-cc --help
```

### Backend Selection

```bash
# Via CLI flag
./target/release/typemux-cc --backend ty

# Via environment variable
TYPEMUX_CC_BACKEND=ty ./target/release/typemux-cc
```

### Configuration

To configure the backend via the wrapper script (persistent across sessions):

```bash
mkdir -p ~/.config/typemux-cc
cat > ~/.config/typemux-cc/config << 'EOF'
# Select backend (pyright, ty, or pyrefly)
export TYPEMUX_CC_BACKEND="pyright"

# Enable file output
export TYPEMUX_CC_LOG_FILE="/tmp/typemux-cc.log"
EOF
```

### Logging

Default output is stderr. For file output:

```bash
TYPEMUX_CC_LOG_FILE=/tmp/typemux-cc.log ./target/release/typemux-cc
```

| Environment Variable | Description | Default |
|----------------------|-------------|---------|
| `TYPEMUX_CC_LOG_FILE` | Log file path | Not set (stderr only) |
| `TYPEMUX_CC_BACKEND` | LSP backend to use | `pyright` |
| `TYPEMUX_CC_MAX_BACKENDS` | Max concurrent backend processes | `8` |
| `TYPEMUX_CC_BACKEND_TTL` | Backend TTL in seconds (0 = disabled) | `1800` |
| `RUST_LOG` | Log level | `typemux_cc=debug` |

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

| Action | Proxy Behavior |
|--------|----------------|
| 1. Start Claude Code | Search for fallback .venv (start without venv if not found) |
| 2. Open `project-a/src/main.py` | Detect `project-a/.venv` → spawn backend (session 1), add to pool |
| 3. Open `project-b/src/main.py` | Detect `project-b/.venv` → spawn backend (session 2), add to pool |
| 4. Return to `project-a/src/main.py` | `project-a/.venv` already in pool → route to session 1 (no restart) |

### What Actually Happens

When you move from `project-a/main.py` to `project-b/main.py`:

1. Proxy detects different `.venv` (project-a/.venv → project-b/.venv)
2. Checks the backend pool — `project-b/.venv` not found
3. Spawns new backend with `VIRTUAL_ENV=project-b/.venv` (session 2)
4. **Session 1 (project-a) stays alive in the pool** — no restart
5. Restores open documents under project-b/ to session 2
6. Clears diagnostics for documents outside project-b/
7. **All LSP requests for project-b files now use project-b dependencies**

When you return to `project-a/main.py` later, session 1 is still in the pool — **zero restart overhead**.

Backends are evicted only when the pool is full (LRU) or after idle timeout (TTL, default 30 min).

From the user's perspective: **Nothing visible happens. LSP just works.**

### Environment Variables

Each backend process is spawned with `VIRTUAL_ENV` and `PATH` set to point at the detected `.venv`. These are **only applied to the child backend process** — your shell environment and system PATH are never modified.

## Troubleshooting

### LSP Not Working

```bash
which pyright-langserver              # Check if backend is in PATH (or: which ty, which pyrefly)
cat ~/.claude/settings.json | grep typemux  # Check plugin settings
tail -100 /tmp/typemux-cc.log        # Check logs
```

### Plugin Update Not Taking Effect

Due to a [known Claude Code issue](https://github.com/anthropics/claude-code/issues/13799), `/plugin update` may not refresh the cached plugin files. If you still see the old version after updating, manually clear the cache:

```bash
# 1. Remove cached plugin
rm -rf ~/.claude/plugins/cache/typemux-cc-marketplace/

# 2. Reinstall
/plugin install typemux-cc@typemux-cc-marketplace

# 3. Restart Claude Code
```

### `.venv` Not Switching

- Verify `.venv/pyvenv.cfg` exists
- Verify file is within git repository
- Check the log for `venv_path=None` — this means the document was cached before `.venv` existed
- If `.venv` was created after the file was opened, **reopen the file** to trigger venv re-detection
- Use `RUST_LOG=trace` for detailed venv search logs

> **Why does this happen?** typemux-cc caches the venv for each document on first open. If `.venv` doesn't exist yet (e.g., created later by a hook), the cache stores `None`. Subsequent requests reuse the cached value without re-searching. Reopening the file clears the cache entry and triggers a fresh search.

## Known Limitations

| Item | Limitation | Workaround |
|------|------------|------------|
| Windows unsupported | Path handling assumes Unix-like systems | Use WSL2 |
| macOS Intel unsupported | Prebuilt is arm64 only | Use Apple Silicon |
| Fixed venv name | Only `.venv` with `pyvenv.cfg` — intentionally strict to avoid silently wrong environments (poetry/conda/etc. not supported) | Rename to `.venv` or create a `.venv` symlink |
| Symlinks | May fail to detect `pyvenv.cfg` if `.venv` is a symlink | Use actual directory |
| Late `.venv` creation | venv cached as `None` if `.venv` didn't exist when file was opened | Reopen the file after creating `.venv` |

## Architecture

For design philosophy, state transitions, and internal implementation details, see:

**[ARCHITECTURE.md](./ARCHITECTURE.md)**

## License

This project is licensed under the **MIT License** - see the [LICENSE](LICENSE) file for details.
