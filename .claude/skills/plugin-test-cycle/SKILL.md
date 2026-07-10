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

### Step 2: Record the test state

Create a resumable state note before changing Claude Code's plugin state. Record:

- repository path, branch, and commit
- whether the working tree is dirty
- test mode (local build or GitHub release)
- `target/release/typemux-cc --version`
- `shasum -a 256 target/release/typemux-cc`
- expected marketplace source and plugin version
- each completed workflow step and its result

Store the note outside the repository, for example at
`/tmp/typemux-cc-plugin-test-state.md`, so it survives a restart without adding an
untracked project file. Never record credentials or environment variable values.

### Step 3: Approve plugin state changes

Cache deletion, marketplace registration changes, and plugin installation mutate the
user's Claude Code environment. Show the exact cache path, marketplace registration,
plugin identity, and commands that will be affected, then obtain explicit user approval
before each mutation. Approval for one mutation does not authorize another.

Use a dedicated local-development marketplace registration instead of replacing a
release registration when the installed Claude Code version supports naming separate
registrations. If it does not, report the limitation and ask before modifying the
existing `typemux-cc-marketplace` registration. Do not guess unsupported command flags.

### Step 4: Clear plugin cache

This is **critical**. Claude Code caches plugin binaries aggressively. Skipping this step means the old binary gets used silently.

```bash
rm -rf ~/.claude/plugins/cache/typemux-cc-marketplace/
```

Resolve and display the target first, and refuse to delete a path outside
`~/.claude/plugins/cache/`. Record completion in the state note.

### Step 5: Remove old marketplace registration

```
/plugin marketplace remove typemux-cc-marketplace
```

If this fails with "not found", record that result and proceed.

### Step 6: Register marketplace

**For local development (no GitHub release needed):**

```
/plugin marketplace add /path/to/typemux-cc
```

Use the actual project directory path (or worktree path if working in a worktree).

**For testing a GitHub release:**

```
/plugin marketplace add K-dash/typemux-cc
```

### Step 7: Install the plugin

```
/plugin install typemux-cc@typemux-cc-marketplace
```

Verify the plugin appears in the installed list. If it doesn't show up:

1. Confirm cache was cleared (Step 2)
2. Try removing and re-adding the marketplace
3. Check that `.claude-plugin/plugin.json` and `.claude-plugin/marketplace.json` are valid JSON

Before restart, record the installed plugin version and expected binary SHA-256 in the
state note. Do not assume installation proves which binary will be loaded.

### Step 8: Restart Claude Code

The plugin binary is loaded at startup. A restart is required after installation or update.

Tell the user: "Please restart Claude Code, then come back and say 'continue test' to proceed with verification."

After restart, read the state note and confirm that the repository commit, expected
plugin version, and expected binary hash still identify the intended test artifact.
Verify the loaded plugin version using the version information exposed by the installed
plugin. If the loaded binary path can be identified from the installed plugin metadata,
compare its SHA-256 with the recorded hash. If it cannot be identified, report that the
binary identity is unverified rather than treating the test as successful.

### Step 9: Verify functionality

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

### Step 10: Check logs

```bash
cat /tmp/typemux-cc.log
```

Look for:

- `venv found` or `venv not found` messages matching expectations
- No unexpected errors or panics
- Backend startup/shutdown messages if testing pool behavior
- `Discarding stale response` if testing race conditions

If the log file doesn't exist, check that log output is configured via the
`TYPEMUX_CC_LOG_FILE` environment variable.

## Common Issues

| Symptom | Cause | Fix |
|---------|-------|-----|
| Old behavior after update | Plugin cache not cleared | Step 4: clear the approved cache path |
| Plugin not in installed list | Marketplace registration stale | Steps 5-6: remove and re-add the approved marketplace |
| LSP errors after install | Claude Code not restarted | Step 8: restart required |
| No log file at `/tmp/` | Log output not configured | Check `TYPEMUX_CC_LOG_FILE` |
| `cp` prompts for overwrite | Missing `-f` flag | Always use `cp -f` or `rm -f` before copy |

## Examples

### Example 1: Quick local iteration

```
User: "plugin test"
Flow:
1. cargo build --release
2. Record commit, version, binary hash, and expected marketplace in the state note
3. Obtain approval before clearing the exact cache path
4. Obtain separate approval before changing marketplace registration
5. Register the local repository and install the plugin
6. Record the installed version and expected binary hash
7. [User restarts Claude Code]
8. Resume from the state note and verify artifact identity
9. Run the LSP hover test and inspect `/tmp/typemux-cc.log`
```

### Example 2: Testing a GitHub release

```
User: "test the new release"
Flow:
1. (Assumes /publish already done)
2. Record the release version, source, and expected binary hash in the state note
3. Obtain approval before clearing the exact cache path
4. Obtain separate approval before changing marketplace registration
5. Register `K-dash/typemux-cc` and install the plugin
6. [User restarts Claude Code]
7. Resume from the state note and verify artifact identity
8. Run hover, cross-project, and missing-venv verification
9. Inspect `/tmp/typemux-cc.log`
```
