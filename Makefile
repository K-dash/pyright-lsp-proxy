.PHONY: all check check-versions fmt lint doc test build clean

# Default target: run all
all: fmt lint doc test

# Format
fmt:
	cargo fmt

# Format check (for CI)
fmt-check:
	cargo fmt --check

# Lint (clippy, including test/bin targets)
lint:
	cargo clippy --workspace --all-targets --locked -- -D warnings

# Doc (rustdoc lints; matches [lints.rustdoc] in Cargo.toml)
doc:
	RUSTDOCFLAGS="-D warnings" cargo doc --workspace --lib --no-deps --locked

# Test
test:
	cargo test

# Build
build:
	cargo build

# Release build
release:
	cargo build --release

# Check (compile only, no binary generation)
check:
	cargo check

# Check that Cargo.toml, Cargo.lock, plugin.json, and marketplace.json all
# agree on the same version (read-only, no files are modified)
check-versions:
	./scripts/check-versions.sh

# Clean
clean:
	cargo clean

# For CI: fmt-check + lint + doc + test
ci: fmt-check lint doc test
