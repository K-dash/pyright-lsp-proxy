# Troubleshooting

## Self-Diagnosis (`--doctor`)

Run `--doctor` to dump configuration, environment, and system info at a glance:

```bash
# Find the binary in the plugin cache, then run --doctor
ls ~/.claude/plugins/cache/typemux-cc-marketplace/typemux-cc/
# → 0.2.9
~/.claude/plugins/cache/typemux-cc-marketplace/typemux-cc/0.2.9/bin/typemux-cc --doctor
```

The binary reads `~/.config/typemux-cc/config` directly, so your settings are reflected in the output:

```
typemux-cc v0.2.9

Config file:
  Path              /Users/foo/.config/typemux-cc/config
  Status            loaded

Configuration:
  backend         pyright              (default)
  max_backends    8                    (default)
  backend_ttl     1800                 (default)
  warmup_timeout  2                    (default)
  fanout_timeout  5                    (default)
  log_file        /tmp/typemux-cc.log  (config: /Users/foo/.config/typemux-cc/config)

Environment:
  Backend binary    pyright-langserver
    Path            /usr/local/bin/pyright-langserver
    Version         pyright 1.1.350
  Git toplevel      /Users/foo/project
  Startup .venv     /Users/foo/project/.venv (detection only; backends spawn lazily on the first venv-resolving message)

System:
  OS                macos (Darwin 24.0.0)
  Arch              aarch64
```

Add `--json` for machine-readable output:

```bash
~/.claude/plugins/cache/typemux-cc-marketplace/typemux-cc/0.2.9/bin/typemux-cc --doctor --json
```

## LSP Not Working

> **Tip**: Run `--doctor` first to check your configuration and backend availability. For detailed logs, add `TYPEMUX_CC_LOG_FILE=/tmp/typemux-cc.log` to your [config](../README.md#configuration).

```bash
# Quick self-diagnosis (replace version number as needed)
~/.claude/plugins/cache/typemux-cc-marketplace/typemux-cc/0.2.9/bin/typemux-cc --doctor
cat ~/.claude/settings.json | grep typemux   # Check plugin settings
tail -100 /tmp/typemux-cc.log               # Check logs (if file logging enabled)
```

## Plugin Update Not Taking Effect

Due to a [known Claude Code issue](https://github.com/anthropics/claude-code/issues/13799), `/plugin update` may not refresh the cached plugin files. If you still see the old version after updating, manually clear the cache:

```bash
# 1. Remove cached plugin
rm -rf ~/.claude/plugins/cache/typemux-cc-marketplace/

# 2. Reinstall
/plugin install typemux-cc@typemux-cc-marketplace

# 3. Restart Claude Code
```

If the binary version still lags behind the plugin version (`typemux-cc --version` inside the cache disagrees with the cache directory name), a stale gitignored binary in the marketplace clone is shadowing the download. Remove it once:

```bash
rm ~/.claude/plugins/marketplaces/typemux-cc-marketplace/bin/typemux-cc
# Restart Claude Code — the installer re-downloads the matching version
```

Since v0.2.14 the installer verifies the installed binary's version against the plugin manifest and re-downloads on mismatch, so this cleanup is only needed once for installations created with older versions.

## Empty `bin/` Right After Updating

Symptom: `/plugin update` reports the new version, but `~/.claude/plugins/cache/typemux-cc-marketplace/typemux-cc/<version>/bin/` is empty and the LSP doesn't start.

This is the opposite of "Plugin Update Not Taking Effect" above (old version persisting): here the new version's cache directory exists, but with nothing in `bin/`.

The plugin manifest (`.claude-plugin/plugin.json`) lives in this repo, so `/plugin update` starts advertising a new version the moment a version-bump PR merges to `main` — before the matching GitHub Release exists. The [`Release` workflow](../.github/workflows/release.yml) now tags and builds that merge commit automatically, so the gap between "manifest says vX.Y.Z" and "release vX.Y.Z has assets" is roughly one CI build (a few minutes), not however long a human takes to notice and tag it. If you update and restart inside that window, `install.sh` (run by the SessionStart hook) creates `bin/` before downloading, then 404s against a release that doesn't exist yet and exits 1 — leaving `bin/` present but empty.

Remedy: start a new Claude Code session once the release is live (check the [Releases page](https://github.com/K-dash/typemux-cc/releases)). The SessionStart hook re-runs `install.sh` on every session start, so the next one downloads normally. No cache removal needed — this is not the stale-cache issue above.

## `.venv` Not Switching

- Verify `.venv/pyvenv.cfg` exists
- Verify file is within git repository
- Use `RUST_LOG=trace` for detailed venv search logs

> [!Note]
> If `.venv` didn't exist when a file was first opened, typemux-cc automatically re-searches for it on the next LSP request. No need to reopen the file.

> [!Note]
> If `.venv` is **replaced** (e.g. `uv sync` recreating it), typemux-cc detects the change on the next LSP request and restarts the backend with the new environment automatically. If `.venv` is removed and not recreated, the backend is evicted after a short grace period and requests return an explicit error instead of stale results.

## "Project root is gitignored" Warning

Claude Code's LSP tool filters `goToDefinition`/`findReferences` results through `git check-ignore` run in the session's working directory, silently dropping results whose paths are gitignored there ([anthropics/claude-code#76371](https://github.com/anthropics/claude-code/issues/76371)). When a backend's project root is gitignored from where Claude Code is running (e.g. a project nested inside a `.claude/worktrees/` directory), typemux-cc shows a `window/showMessage` warning so missing results don't look like an LSP failure. Launch Claude Code from inside the project directory to avoid this.
