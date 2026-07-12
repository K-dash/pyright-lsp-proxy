<div align="center">

# typemux-cc

**Python type-checker LSP multiplexer for Claude Code — pyright, ty, pyrefly**

<div align="center">
  <a href="https://github.com/K-dash/typemux-cc/graphs/commit-activity"><img alt="GitHub commit activity" src="https://img.shields.io/github/commit-activity/m/K-dash/typemux-cc"/></a>
  <a href="https://github.com/K-dash/typemux-cc/blob/main/LICENSE"><img alt="License" src="https://img.shields.io/badge/LICENSE-MIT-green"/></a>
  <a href="https://www.rust-lang.org/"><img alt="Rust" src="https://img.shields.io/badge/rust-1.88+-orange.svg"/></a>
  <a href="https://deepwiki.com/K-dash/typemux-cc"><img src="https://img.shields.io/badge/DeepWiki-K--dash%2Ftypemux--cc-blue.svg?logo=data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAACwAAAAyCAYAAAAnWDnqAAAAAXNSR0IArs4c6QAAA05JREFUaEPtmUtyEzEQhtWTQyQLHNak2AB7ZnyXZMEjXMGeK/AIi+QuHrMnbChYY7MIh8g01fJoopFb0uhhEqqcbWTp06/uv1saEDv4O3n3dV60RfP947Mm9/SQc0ICFQgzfc4CYZoTPAswgSJCCUJUnAAoRHOAUOcATwbmVLWdGoH//PB8mnKqScAhsD0kYP3j/Yt5LPQe2KvcXmGvRHcDnpxfL2zOYJ1mFwrryWTz0advv1Ut4CJgf5uhDuDj5eUcAUoahrdY/56ebRWeraTjMt/00Sh3UDtjgHtQNHwcRGOC98BJEAEymycmYcWwOprTgcB6VZ5JK5TAJ+fXGLBm3FDAmn6oPPjR4rKCAoJCal2eAiQp2x0vxTPB3ALO2CRkwmDy5WohzBDwSEFKRwPbknEggCPB/imwrycgxX2NzoMCHhPkDwqYMr9tRcP5qNrMZHkVnOjRMWwLCcr8ohBVb1OMjxLwGCvjTikrsBOiA6fNyCrm8V1rP93iVPpwaE+gO0SsWmPiXB+jikdf6SizrT5qKasx5j8ABbHpFTx+vFXp9EnYQmLx02h1QTTrl6eDqxLnGjporxl3NL3agEvXdT0WmEost648sQOYAeJS9Q7bfUVoMGnjo4AZdUMQku50McDcMWcBPvr0SzbTAFDfvJqwLzgxwATnCgnp4wDl6Aa+Ax283gghmj+vj7feE2KBBRMW3FzOpLOADl0Isb5587h/U4gGvkt5v60Z1VLG8BhYjbzRwyQZemwAd6cCR5/XFWLYZRIMpX39AR0tjaGGiGzLVyhse5C9RKC6ai42ppWPKiBagOvaYk8lO7DajerabOZP46Lby5wKjw1HCRx7p9sVMOWGzb/vA1hwiWc6jm3MvQDTogQkiqIhJV0nBQBTU+3okKCFDy9WwferkHjtxib7t3xIUQtHxnIwtx4mpg26/HfwVNVDb4oI9RHmx5WGelRVlrtiw43zboCLaxv46AZeB3IlTkwouebTr1y2NjSpHz68WNFjHvupy3q8TFn3Hos2IAk4Ju5dCo8B3wP7VPr/FGaKiG+T+v+TQqIrOqMTL1VdWV1DdmcbO8KXBz6esmYWYKPwDL5b5FA1a0hwapHiom0r/cKaoqr+27/XcrS5UwSMbQAAAABJRU5ErkJggg==" alt="DeepWiki"></a>


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

Claude Code's official pyright plugin spawns a single LSP backend at startup and holds onto it. If `.venv` doesn't exist yet — or you create a new one later — it never picks it up. You have to restart Claude Code.

This is especially painful with **git worktrees**, now common in AI-assisted development: you spin up a fresh worktree, create `.venv`, and then must restart Claude Code just to get type-checking.

typemux-cc is a Python LSP proxy that fixes this — `.venv` changes are reflected **within your running session**, no restarts required.

This is a recurring, unresolved pain in the Claude Code ecosystem: worktree/venv breakage keeps being reported upstream — [anthropics/claude-code#31391](https://github.com/anthropics/claude-code/issues/31391) (closed as *not planned*), [astral-sh/claude-code-plugins#18](https://github.com/astral-sh/claude-code-plugins/issues/18) (open), [anthropics/claude-code#58365](https://github.com/anthropics/claude-code/issues/58365) — while the official pyright plugin remains a single `pyright-langserver` spawn with no environment handling. typemux-cc exists to solve the environment-lifecycle side of this problem for Python.

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

- **⚡ Late `.venv` creation (worktrees, hooks)** — Spin up a git worktree, create `.venv` later, and typemux-cc picks it up on the next file open. No Claude Code restart needed.
- **🔄 Multi-project venv switching (monorepos)** — typemux-cc keeps a per-`.venv` backend pool and routes requests to the correct one. Switching between projects is instant.
- **🔀 Multi-backend support** — Not locked into pyright. Choose between pyright, ty, or pyrefly — switch via a single env var.

> **Frozen capabilities on cold start** — When no `.venv` exists at startup, `initialize` answers with empty capabilities and that advertisement stays frozen for the session — verified with Claude Code 2.1.207 that the client keeps sending LSP requests regardless, so nothing is gated (see [Frozen Empty Capabilities](docs/ARCHITECTURE.md#frozen-empty-capabilities-important)).

> **Why LSP over text search?** In monorepos, grep returns false positives from same-named types across projects. LSP resolves references at the type-system level. See [real-world benchmarks](./docs/why-lsp.md).

## Supported Backends

| Backend | Command | Status |
|---------|---------|--------|
| [pyright](https://github.com/microsoft/pyright) | `pyright-langserver --stdio` | ✅ Stable (**default** if `TYPEMUX_CC_BACKEND` is not set) |
| [ty](https://github.com/astral-sh/ty) | `ty server` | ✅ Stable |
| [pyrefly](https://github.com/facebook/pyrefly) | `pyrefly lsp` | ✅ Stable |

## Requirements

### Supported OS

| Platform | Architecture |
|----------|--------------|
| macOS | arm64 only |
| Linux | x86_64 / arm64 |

> [!Note]
> Windows is currently unsupported (due to path handling differences).
> Intel macOS users must build from source (prebuilt binaries are arm64 only).

### Prerequisites

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

# ty (by the creators of uv)
pip install ty

# pyrefly (by Meta)
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

### How the Installer Verifies Binaries

The plugin's SessionStart hook downloads the prebuilt binary for your platform from this repository's GitHub Releases over HTTPS, pinned to the plugin's own version tag (never "latest"). Before the downloaded binary is executed or activated, the installer:

1. Verifies its SHA256 checksum against the `.sha256` asset published alongside it by the [release workflow](.github/workflows/release.yml)
2. Confirms the binary reports the expected version

On any failure it keeps the previously working binary and prints an explicit warning, or fails loudly if no binary exists yet. Binaries are built from the tagged commit by GitHub Actions — nothing is built or fetched from anywhere else.

### Method B: Local Build (For Developers)

> Requires Rust 1.88 or later. Running the full test suite (`cargo test` /
> `make ci`) additionally requires [`jq`](https://jqlang.org/) — it's used by
> `scripts/check-versions.sh` to validate the release version across
> `Cargo.toml`, `Cargo.lock`, and the plugin manifests. Install it with
> `brew install jq` (macOS) or `apt-get install -y jq` (Debian/Ubuntu).
> `make ci` fails fast with a clear error if `jq` is missing.

```bash
git clone https://github.com/K-dash/typemux-cc.git
cd typemux-cc
cargo build --release

/plugin marketplace add /path/to/typemux-cc
/plugin install typemux-cc@typemux-cc-marketplace
# Restart Claude Code (initial installation only)
```

## Usage

Automatically starts as a Claude Code plugin — no manual setup required.

### Configuration

Settings are stored in `~/.config/typemux-cc/config`. The file uses `KEY=VALUE` format (shell expansion is **not** supported):

```bash
mkdir -p ~/.config/typemux-cc
cat > ~/.config/typemux-cc/config << 'EOF'
# Select backend (pyright, ty, or pyrefly)
TYPEMUX_CC_BACKEND=pyright

# Enable file logging
TYPEMUX_CC_LOG_FILE=/tmp/typemux-cc.log
EOF
```

> **Note:** `export KEY=VALUE` syntax is also accepted for compatibility with older config files.

Settings priority: **CLI flag > environment variable > config file > default**

| Variable | Description | Default |
|----------|-------------|---------|
| `TYPEMUX_CC_LOG_FILE` | Log file path | Not set (stderr only) |
| `TYPEMUX_CC_BACKEND` | LSP backend to use | `pyright` |
| `TYPEMUX_CC_MAX_BACKENDS` | Max concurrent backend processes | `8` |
| `TYPEMUX_CC_BACKEND_TTL` | Backend TTL in seconds (0 = disabled) | `1800` |
| `TYPEMUX_CC_POOL_SWEEP_INTERVAL` | Interval in seconds between pool sweep ticks (drives both TTL eviction and the venv staleness sweep; must be > 0) | `60` |
| `TYPEMUX_CC_FANOUT_TIMEOUT` | Fan-out timeout in seconds for `workspace/symbol` (0 = no timeout) | `5` |
| `TYPEMUX_CC_VENV_CHECK_INTERVAL` | Interval in seconds between venv identity checks (0 = disable venv identity tracking) | `5` |
| `TYPEMUX_CC_INIT_HANDSHAKE_TIMEOUT` | Backend spawn → `initialize` handshake timeout in seconds | `10` |
| `RUST_LOG` | Log level | `typemux_cc=debug` |

An invalid value for a numeric variable above (e.g. `TYPEMUX_CC_FANOUT_TIMEOUT=5s`) fails startup with an explicit error instead of silently falling back to the default. `TYPEMUX_CC_POOL_SWEEP_INTERVAL=0` also fails startup — unlike the other vars, `0` isn't a valid "disable" sentinel here; use `--backend-ttl 0` / `TYPEMUX_CC_VENV_CHECK_INTERVAL=0` to disable the sweep's individual jobs instead.

## Typical Use Case

### Git Worktree (AI-Assisted Development)

A common workflow with AI coding agents:

```
my-project/                    # main worktree
├── .venv/
└── src/main.py

my-project-worktree/           # new worktree (no .venv yet)
└── src/main.py
```

| Step | What Happens |
|------|-------------|
| 1. Create worktree | `git worktree add ../my-project-worktree feat/new-feature` — no `.venv` exists |
| 2. Create `.venv` | `cd ../my-project-worktree && uv sync` — `.venv` now exists |
| 3. Open a file | Claude Code opens `my-project-worktree/src/main.py` → typemux-cc detects the new `.venv` and spawns a backend automatically |

With the official plugin, step 3 would require restarting Claude Code. With typemux-cc, it just works.

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

| Claude Code Action | Proxy Behavior |
|--------------------|----------------|
| 1. Session starts | Search for fallback .venv (start without venv if not found) |
| 2. Opens `project-a/src/main.py` | Detect `project-a/.venv` → spawn backend (session 1), add to pool |
| 3. Opens `project-b/src/main.py` | Detect `project-b/.venv` → spawn backend (session 2), add to pool |
| 4. Returns to `project-a/src/main.py` | `project-a/.venv` already in pool → route to session 1 (no restart) |

### What Actually Happens

When Claude Code moves from `project-a/main.py` to `project-b/main.py`:

1. Proxy detects different `.venv` (project-a/.venv → project-b/.venv)
2. Checks the backend pool — `project-b/.venv` not found
3. Spawns new backend with `VIRTUAL_ENV=project-b/.venv` (session 2)
4. **Session 1 (project-a) stays alive in the pool** — no restart
5. Restores open documents under project-b/ to session 2
6. Clears diagnostics for documents outside project-b/
7. **All LSP requests for project-b files now use project-b dependencies**

When Claude Code returns to `project-a/main.py` later, session 1 is still in the pool — **zero restart overhead**.

Backends are evicted only when the pool is full (LRU) or after idle timeout (TTL, default 30 min).

From the user's perspective: **Nothing visible happens. LSP just works.**

### Environment Variables

Each backend process is spawned with `VIRTUAL_ENV` and `PATH` set to point at the detected `.venv`. These are **only applied to the child backend process** — your shell environment and system PATH are never modified.

## Troubleshooting

For self-diagnosis (`--doctor`), LSP not working, plugin update issues, `.venv` not switching, and the "Project root is gitignored" warning, see:

**[docs/TROUBLESHOOTING.md](./docs/TROUBLESHOOTING.md)**

## Known Limitations

| Item | Limitation | Workaround |
|------|------------|------------|
| Windows unsupported | Path handling assumes Unix-like systems | Use WSL2 |
| macOS Intel unsupported | Prebuilt is arm64 only | Use Apple Silicon |
| Fixed venv name | Only `.venv` with `pyvenv.cfg` — intentionally strict to avoid silently wrong environments (poetry/conda/etc. not supported) | Rename to `.venv` or create a `.venv` symlink |
| Symlinks | May fail to detect `pyvenv.cfg` if `.venv` is a symlink | Use actual directory |
| setuptools editable installs | Not a typemux-cc bug. All LSP backends (pyright, ty, pyrefly) cannot resolve imports from setuptools-style editable installs that use import hooks ([ty#475](https://github.com/astral-sh/ty/issues/475)) | Switch build backend to hatchling/flit, or add source paths to `extra-paths` in backend config |
| `workspace/symbol` fan-out latency | With multiple backends, `workspace/symbol` fans out to all backends and merges results; response time equals the slowest backend (timeout: 5s default) | Adjust via `TYPEMUX_CC_FANOUT_TIMEOUT` env var |
| `workspace/symbol` always returns empty | Claude Code's LSP tool does not pass a `query` parameter to `workspace/symbol` requests. The LSP spec requires `{ query: "search string" }`, but the tool interface only exposes `operation`, `filePath`, `line`, `character`. With an empty query, pyright returns no results. This is a Claude Code limitation, not a typemux-cc bug. | Use Grep/Glob for cross-project symbol search until Claude Code adds `query` support |
| `goToDefinition`/`findReferences` return empty for gitignored paths (including Claude Code's own worktrees) | Not a typemux-cc bug — the proxy forwards pyright's valid `Location[]` response. Claude Code's LSP tool post-filters URI-bearing results (`goToDefinition`, `findReferences`, `goToImplementation`, `workspaceSymbol`) through `git check-ignore` run in the session's cwd, silently dropping every location whose path is ignored in that git context. Claude Code also registers `**/.claude/worktrees/` in `.git/info/exclude`, so results inside its own worktrees are always dropped when the session runs at the main repo root. Other worktree layouts (e.g., `.worktree/`) are affected only when an ignore rule matches them, and the same filter hides definitions resolving into gitignored directories such as `.venv/` (third-party sources). `hover`/`documentSymbol` results carry no URIs and are unaffected. Verified against Claude Code v2.1.206; reported upstream in [anthropics/claude-code#76371](https://github.com/anthropics/claude-code/issues/76371). | Launch Claude Code directly inside the worktree (paths then resolve relative to the worktree root and no longer match the ignore rules), or keep worktrees in a directory not matched by any gitignore rule |

## Architecture

For design philosophy, state transitions, and internal implementation details, see:

**[docs/ARCHITECTURE.md](./docs/ARCHITECTURE.md)**

## Privacy

typemux-cc runs entirely locally and collects no data. The only network access is the version-pinned, checksum-verified binary download from GitHub Releases at install time. See [PRIVACY.md](./PRIVACY.md).

## Contributing

Contributions are welcome! Please feel free to submit a Pull Request.

## License

This project is licensed under the **MIT License** - see the [LICENSE](LICENSE) file for details.
