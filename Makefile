# running `make` with no target shows help instead of silently building
.DEFAULT_GOAL := help

.PHONY: help build test lint fmt fmt-check deny check install clean

help: ## list available targets
	@grep -E '^[a-zA-Z_-]+:.*## ' $(MAKEFILE_LIST) | awk 'BEGIN {FS = ":.*## "}; {printf "%-12s %s\n", $$1, $$2}'

build: ## cargo build --workspace
	cargo build --workspace

test: ## cargo test --workspace --lib --tests
	cargo test --workspace --lib --tests

lint: ## cargo clippy --workspace --all-targets -- -D warnings
	cargo clippy --workspace --all-targets -- -D warnings

fmt: ## cargo fmt
	cargo fmt

fmt-check: ## cargo fmt --check
	cargo fmt --check

deny: ## cargo deny check
	cargo deny check

check: fmt-check lint test deny ## fmt-check, lint, test, deny (in that order)

install: ## cargo install --path crates/nightjar-cli
	cargo install --path crates/nightjar-cli

clean: ## cargo clean
	cargo clean
