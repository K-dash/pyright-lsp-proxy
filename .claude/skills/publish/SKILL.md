---
name: publish
description: Prepare a version bump through a feature branch and pull request; merging to main auto-tags and publishes the release. Triggers on 'publish', 'release', 'bump version'.
---

# Publish

## CI/CD Pipeline

`release.yml` builds release binaries for 3 targets and creates the GitHub Release with binary assets:
- `aarch64-apple-darwin` (macOS ARM64)
- `x86_64-unknown-linux-gnu` (Linux x86_64)
- `aarch64-unknown-linux-gnu` (Linux ARM64)

**Flow**: version-bump PR merges to `main` → `release.yml`'s `preflight` job tags the merge
commit itself → same run builds the 3 binaries and creates the GitHub Release. There is no
separate manual tag step: tagging is part of the automated pipeline, not a step this skill
performs.

The `v*` tag-push trigger still exists as a manual/emergency escape hatch (see the last
section below) but is not part of the normal flow.

## Version Locations

Version must be updated in **3 files** (all must match):

1. `Cargo.toml` — `version` field
2. `.claude-plugin/plugin.json` — `version` field
3. `.claude-plugin/marketplace.json` — `version` field inside `plugins[0]`

## Version Bump Workflow

### Step 1: Pre-flight checks

```bash
git branch --show-current   # Must not be main when files are changed
git status                  # Must be clean
git fetch --tags --quiet
```

Show the latest tag and current `Cargo.toml` version. If currently on `main`, create a
release preparation branch before changing files.

### Step 2: Ask for the new version

Use AskUserQuestion. Show the current version and suggest semver options (patch, minor, major).

### Step 3: Update version

1. Use the Edit tool to update the `version` field in all 3 files:
   - `Cargo.toml`
   - `.claude-plugin/plugin.json`
   - `.claude-plugin/marketplace.json`
2. Run `cargo check --quiet` to regenerate `Cargo.lock`.
3. Show `git diff` for user review.

### Step 4: Prepare a pull request

Ask for explicit authorization before each of the following actions. Authorization for
one action does not authorize the next one.

```bash
git add Cargo.toml Cargo.lock .claude-plugin/plugin.json .claude-plugin/marketplace.json
git commit -m "chore: bump version to <NEW_VERSION>"
git push -u origin <release-branch>
gh pr create
```

Wait for the pull request to pass its required checks and be merged through the
repository's normal merge process. Merging is a separate authorization from the
commit/push/PR-creation actions above — ask for it separately. Never commit or push
directly to `main`.

### Step 5: Verify the release

Once merged, `release.yml` runs automatically against the merge commit: its `preflight`
job tags the commit with `v<NEW_VERSION>`, and the same run then builds all 3 binaries and
creates the GitHub Release. There is no manual tag step to perform.

1. Watch the run: `https://github.com/K-dash/typemux-cc/actions`
2. Once it finishes, confirm the `v<NEW_VERSION>` GitHub Release has all 6 assets (3
   binaries + 3 `.sha256` checksums).
3. Confirm release notes were generated.

## Manual/Emergency Tagging

`release.yml` still triggers on `v*` tag pushes, so pushing a tag by hand still works if
the automation ever needs to be bypassed (e.g. the `preflight` job itself is broken). This
is not part of the normal flow above — use it only as a deliberate fallback, on a commit
where all 3 version files already agree:

```bash
git tag v<VERSION> <COMMIT>
git push origin v<VERSION>
```
