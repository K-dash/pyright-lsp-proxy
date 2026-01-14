.PHONY: all check fmt lint test build clean

# デフォルトターゲット: 全部実行
all: fmt lint test

# フォーマット
fmt:
	cargo fmt

# フォーマットチェック（CI用）
fmt-check:
	cargo fmt --check

# Lint (clippy)
lint:
	cargo clippy -- -D warnings

# テスト
test:
	cargo test

# ビルド
build:
	cargo build

# リリースビルド
release:
	cargo build --release

# チェック（コンパイルのみ、バイナリ生成なし）
check:
	cargo check

# クリーン
clean:
	cargo clean

# CI用: fmt-check + lint + test
ci: fmt-check lint test
