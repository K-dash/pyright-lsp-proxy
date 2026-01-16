#!/usr/bin/env bash
set -euo pipefail

# Usage: ./scripts/bump-version.sh

# Fetch latest tags from remote
echo "Fetching latest tags from remote..."
git fetch --tags --quiet

# Get the latest tag version
LATEST_TAG=$(git tag -l 'v*' | sort -V | tail -n 1)
if [ -z "$LATEST_TAG" ]; then
  echo "No existing tags found."
  LATEST_VERSION="none"
else
  LATEST_VERSION="${LATEST_TAG#v}"
  echo "Current latest tag: ${LATEST_TAG} (${LATEST_VERSION})"
fi

# Prompt for new version
echo ""
read -p "Enter new version: " NEW_VERSION

if [ -z "$NEW_VERSION" ]; then
  echo "Error: Version cannot be empty"
  exit 1
fi

echo "Bumping version to ${NEW_VERSION}..."

# Update Cargo.toml
echo "Updating Cargo.toml..."
sed -i.bak "s/^version = \".*\"/version = \"${NEW_VERSION}\"/" Cargo.toml

# Update .claude-plugin/plugin.json
echo "Updating .claude-plugin/plugin.json..."
sed -i.bak "s/\"version\": \".*\"/\"version\": \"${NEW_VERSION}\"/" .claude-plugin/plugin.json

# Clean up backup files
find . -name "*.bak" -delete

# Update Cargo.lock
echo "Updating Cargo.lock..."
cargo check --quiet

echo "✅ Version bumped to ${NEW_VERSION}"
echo ""
echo "Next steps:"
echo "  1. Review changes: git diff"
echo "  2. Commit: git add -A && git commit -m 'chore: bump version to ${NEW_VERSION}'"
echo "  3. Tag: git tag v${NEW_VERSION}"
echo "  4. Push: git push origin main && git push origin v${NEW_VERSION}"
