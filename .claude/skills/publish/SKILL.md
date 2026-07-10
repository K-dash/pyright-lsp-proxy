---
name: publish
description: Prepare and publish a version through a feature branch, pull request, and approved tag push. Triggers on 'publish', 'release', 'bump version'.
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

Do not tag the feature branch. Wait for the pull request to pass its required checks and
be merged through the repository's normal merge process. Do not merge it unless the
user explicitly authorizes the merge.

### Step 5: Tag the merged commit

After the pull request is merged:

1. Fetch the updated `main` and tags.
2. Verify that the local target commit is the merged `main` commit and that all version
   locations contain `<NEW_VERSION>`.
3. Show the exact commit that will be tagged.
4. Ask for explicit authorization to create the tag.
5. Create `v<NEW_VERSION>` on that commit.
6. Ask separately for explicit authorization to push the tag.

```bash
git tag v<NEW_VERSION> <MERGED_COMMIT>
git push origin v<NEW_VERSION>
```

Never commit or push directly to `main`.

### Step 6: Verify

Verify that the CI pipeline builds all release assets and creates the GitHub Release. Link to:
`https://github.com/K-dash/typemux-cc/actions`
