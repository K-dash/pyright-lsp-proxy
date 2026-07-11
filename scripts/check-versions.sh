#!/usr/bin/env bash
# Verify that the release version agrees across every source of truth:
# Cargo.toml, Cargo.lock (root package), .claude-plugin/plugin.json,
# .claude-plugin/marketplace.json, and (in --release mode) the git tag.
# Read-only: never modifies any file. Never guesses or auto-corrects a
# mismatch — it only reports the actual value from each source.
set -euo pipefail

die() {
  echo "error: $*" >&2
  exit 1
}

usage() {
  cat >&2 <<'EOF'
Usage: check-versions.sh [--release TAG] [--root DIR]

  --release TAG   Also verify TAG equals "v<version>" (release mode).
  --root DIR      Repository root to check (default: parent of this script).
EOF
  exit 1
}

script_dir=$(cd "$(dirname "$0")" && pwd)
root="$script_dir/.."
release_tag=""

while [ $# -gt 0 ]; do
  case "$1" in
    --release)
      [ $# -ge 2 ] || usage
      release_tag="$2"
      shift 2
      ;;
    --root)
      [ $# -ge 2 ] || usage
      root="$2"
      shift 2
      ;;
    -h|--help)
      usage
      ;;
    *)
      usage
      ;;
  esac
done

command -v jq >/dev/null 2>&1 \
  || die "jq is required but not found. Install it with 'brew install jq' (macOS) or 'apt-get install -y jq' (Debian/Ubuntu)."

cargo_toml="$root/Cargo.toml"
cargo_lock="$root/Cargo.lock"
plugin_json="$root/.claude-plugin/plugin.json"
marketplace_json="$root/.claude-plugin/marketplace.json"
hooks_json="$root/hooks.json"
lsp_json="$root/.lsp.json"

for f in "$cargo_toml" "$cargo_lock" "$plugin_json" "$marketplace_json" "$hooks_json" "$lsp_json"; do
  [ -f "$f" ] || die "required file not found: $f"
done

# --- JSON syntax --------------------------------------------------------

for f in "$plugin_json" "$marketplace_json" "$hooks_json" "$lsp_json"; do
  if ! err=$(jq empty "$f" 2>&1); then
    die "malformed JSON in $f: $err"
  fi
done

# --- Extraction ----------------------------------------------------------

# Prints the "version" value from the [package] section of a Cargo.toml.
# Scoped to that one section so a dependency's own "version = ..." line
# (e.g. under [dependencies]) is never picked up.
extract_cargo_toml_version() {
  awk '
    /^\[/ { in_pkg = ($0 == "[package]") }
    in_pkg && /^version[ \t]*=/ {
      if (match($0, /"[^"]*"/)) {
        print substr($0, RSTART + 1, RLENGTH - 2)
        exit
      }
    }
  ' "$1"
}

# Prints the "version" of the [[package]] block whose name equals $2 in a
# Cargo.lock. Resets its name/version state at every [[package]] boundary,
# so it cannot return a dependency crate's version by accident.
extract_cargo_lock_version() {
  local file="$1" pkg="$2"
  awk -v want="$pkg" '
    /^\[\[package\]\]/ { name = ""; ver = "" }
    /^name[ \t]*=/ {
      if (match($0, /"[^"]*"/)) name = substr($0, RSTART + 1, RLENGTH - 2)
    }
    /^version[ \t]*=/ {
      if (match($0, /"[^"]*"/)) {
        ver = substr($0, RSTART + 1, RLENGTH - 2)
        if (name == want) { print ver; exit }
      }
    }
  ' "$file"
}

cargo_toml_version=$(extract_cargo_toml_version "$cargo_toml")
[ -n "$cargo_toml_version" ] || die "could not find [package] version in $cargo_toml"

cargo_lock_version=$(extract_cargo_lock_version "$cargo_lock" "typemux-cc")
[ -n "$cargo_lock_version" ] || die "could not find package \"typemux-cc\" version in $cargo_lock"

plugin_version=$(jq -r '.version // empty' "$plugin_json")
[ -n "$plugin_version" ] || die "could not find .version in $plugin_json"

marketplace_version=$(jq -r '.plugins[0].version // empty' "$marketplace_json")
[ -n "$marketplace_version" ] || die "could not find .plugins[0].version in $marketplace_json"

# --- Comparison ------------------------------------------------------------

names=("Cargo.toml" "Cargo.lock (typemux-cc package)" ".claude-plugin/plugin.json" ".claude-plugin/marketplace.json")
values=("$cargo_toml_version" "$cargo_lock_version" "$plugin_version" "$marketplace_version")

mismatch=0
reference="${values[0]}"
for v in "${values[@]}"; do
  [ "$v" = "$reference" ] || mismatch=1
done

if [ "$mismatch" -eq 1 ]; then
  echo "Version mismatch detected:" >&2
  i=0
  while [ "$i" -lt "${#names[@]}" ]; do
    printf '  %-38s %s\n' "${names[$i]}:" "${values[$i]}" >&2
    i=$((i + 1))
  done
  exit 1
fi

version="$reference"

# --- Release tag (only in --release mode) -----------------------------

if [ -n "$release_tag" ]; then
  expected_tag="v$version"
  if [ "$release_tag" != "$expected_tag" ]; then
    echo "Release tag mismatch:" >&2
    printf '  %-38s %s\n' "git tag:" "$release_tag" >&2
    printf '  %-38s %s\n' "expected (v<version>):" "$expected_tag" >&2
    exit 1
  fi
fi

if [ -n "$release_tag" ]; then
  echo "OK: all version sources and tag agree on $version (tag $release_tag)"
else
  echo "OK: all version sources agree on $version"
fi
