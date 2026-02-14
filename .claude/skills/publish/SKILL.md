---
name: publish
description: Bump version, tag, and push to trigger the automated release pipeline. Triggers on 'publish', 'release', 'bump version'. This skill handles the full version bump workflow and knows the CI/CD pipeline to avoid manual mistakes.
---

# Publish

## CI/CD Pipeline

Single-stage GitHub Actions pipeline triggered by git tags:

**`release.yml`** (`v*` tag push) — build release binaries for 3 targets, create GitHub Release with binary assets:
- `aarch64-apple-darwin` (macOS ARM64)
- `x86_64-unknown-linux-gnu` (Linux x86_64)
- `aarch64-unknown-linux-gnu` (Linux ARM64)

**Flow**: `git push tag` → release.yml (build 3 binaries + GitHub Release)

## Version Locations

Version must be updated in **3 files** (all must match):

1. `Cargo.toml` — `version` field
2. `.claude-plugin/plugin.json` — `version` field
3. `.claude-plugin/marketplace.json` — `version` field inside `plugins[0]`

## Version Bump Workflow

### Step 1: Pre-flight checks

```bash
git branch --show-current   # Must be on main
git status                  # Must be clean
git fetch --tags --quiet
```

Confirm clean working directory on `main`. Show the latest tag and current `Cargo.toml` version.

### Step 2: Ask for the new version

Use AskUserQuestion. Show the current version and suggest semver options (patch, minor, major).

### Step 3: Update version

1. Use the Edit tool to update the `version` field in all 3 files:
   - `Cargo.toml`
   - `.claude-plugin/plugin.json`
   - `.claude-plugin/marketplace.json`
2. Run `cargo check --quiet` to regenerate `Cargo.lock`.
3. Show `git diff` for user review.

### Step 4: Commit, tag, and push

```bash
git add Cargo.toml Cargo.lock .claude-plugin/plugin.json .claude-plugin/marketplace.json
git commit -m "chore: bump version to <NEW_VERSION>"
git tag v<NEW_VERSION>
git push origin main
git push origin v<NEW_VERSION>
```

### Step 5: Verify

Inform the user the CI pipeline will build and create the GitHub Release. Link to:
`https://github.com/K-dash/pyright-lsp-proxy/actions`
