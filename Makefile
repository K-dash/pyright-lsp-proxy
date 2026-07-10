.PHONY: all check fmt lint doc test build clean

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

# Clean
clean:
	cargo clean

# For CI: fmt-check + lint + doc + test
ci: fmt-check lint doc test
