---
name: plugin-test-cycle
description: "Builds, deploys, and tests the typemux-cc Claude Code plugin locally. Handles the full cycle from cargo build through cache clearing, marketplace registration, plugin install, and LSP verification. Triggers on: 'plugin test', 'test plugin', 'plugin cycle', 'deploy and test', 'try new build', 'test the plugin'."
user-invocable: true
trigger: plugin test, test plugin, plugin cycle, deploy and test, try new build
---

# Plugin Test Cycle

End-to-end workflow for building, deploying, and testing the typemux-cc plugin locally. This eliminates the repetitive manual steps that are easy to forget or get wrong.

## Prerequisites

- Rust toolchain installed (`cargo`)
- Claude Code running
- A Python project with `.venv/` available for testing (e.g., rcmr_stadium subprojects)

## Workflow

### Step 1: Build the binary

Ask the user which version to build, or whether to just test the current code without bumping.

**Option A — Build without version bump (quick iteration):**

```bash
cargo build --release
```

The binary is at `target/release/typemux-cc`.

**Option B — Build with version bump:**

Use the `/publish` skill instead, then return here at Step 2.

### Step 2: Clear plugin cache

This is **critical**. Claude Code caches plugin binaries aggressively. Skipping this step means the old binary gets used silently.

```bash
rm -rf ~/.claude/plugins/cache/typemux-cc-marketplace/
```

### Step 3: Remove old marketplace registration

```
/plugin marketplace remove typemux-cc-marketplace
```

If this fails with "not found", that's fine — proceed to Step 4.

### Step 4: Register marketplace

**For local development (no GitHub release needed):**

```
/plugin marketplace add /path/to/typemux-cc
```

Use the actual project directory path (or worktree path if working in a worktree).

**For testing a GitHub release:**

```
/plugin marketplace add K-dash/typemux-cc
```

### Step 5: Install the plugin

```
/plugin install typemux-cc@typemux-cc-marketplace
```

Verify the plugin appears in the installed list. If it doesn't show up:

1. Confirm cache was cleared (Step 2)
2. Try removing and re-adding the marketplace
3. Check that `.claude-plugin/plugin.json` and `.claude-plugin/marketplace.json` are valid JSON

### Step 6: Restart Claude Code

The plugin binary is loaded at startup. A restart is required after installation or update.

Tell the user: "Please restart Claude Code, then come back and say 'continue test' to proceed with verification."

### Step 7: Verify functionality

After restart, run these LSP operations to verify the plugin works:

1. **Hover test** — Pick a Python file in a project with `.venv/`:
   ```
   LSP hover on a symbol in a Python file
   ```

2. **Cross-project switch test** (if multi-backend is relevant):
   - Open a file in project A (has `.venv/`)
   - Open a file in project B (has `.venv/`)
   - Verify both resolve correctly

3. **Missing venv test** (strict mode):
   - Open a file in a project without `.venv/`
   - Verify an appropriate error is returned (not stale results)

### Step 8: Check logs

```bash
cat /tmp/typemux-cc.log
```

Look for:

- `venv found` or `venv not found` messages matching expectations
- No unexpected errors or panics
- Backend startup/shutdown messages if testing pool behavior
- `Discarding stale response` if testing race conditions

If the log file doesn't exist, check that log output is configured (via `TYPEMUX_LOG` env var or config).

## Common Issues

| Symptom | Cause | Fix |
|---------|-------|-----|
| Old behavior after update | Plugin cache not cleared | Step 2: `rm -rf ~/.claude/plugins/cache/typemux-cc-marketplace/` |
| Plugin not in installed list | Marketplace registration stale | Steps 3-4: Remove and re-add marketplace |
| LSP errors after install | Claude Code not restarted | Step 6: Restart required |
| No log file at `/tmp/` | Log output not configured | Check config or `TYPEMUX_LOG` environment variable |
| `cp` prompts for overwrite | Missing `-f` flag | Always use `cp -f` or `rm -f` before copy |

## Examples

### Example 1: Quick local iteration

```
User: "plugin test"
Flow:
1. cargo build --release
2. rm -rf ~/.claude/plugins/cache/typemux-cc-marketplace/
3. /plugin marketplace remove typemux-cc-marketplace
4. /plugin marketplace add /path/to/typemux-cc
5. /plugin install typemux-cc@typemux-cc-marketplace
6. [User restarts Claude Code]
7. LSP hover test on Python file
8. Read /tmp/typemux-cc.log
```

### Example 2: Testing a GitHub release

```
User: "test the new release"
Flow:
1. (Assumes /publish already done)
2. rm -rf ~/.claude/plugins/cache/typemux-cc-marketplace/
3. /plugin marketplace remove typemux-cc-marketplace
4. /plugin marketplace add K-dash/typemux-cc
5. /plugin install typemux-cc@typemux-cc-marketplace
6. [User restarts Claude Code]
7. Full verification: hover + cross-project + missing venv
8. cat /tmp/typemux-cc.log
```
