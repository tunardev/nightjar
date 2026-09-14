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

RUSTFMT_NIGHTLY_OPTS := group_imports=StdExternalCrate,imports_granularity=Module,reorder_impl_items=true,condense_wildcard_suffixes=true

fmt: ## format, including import grouping (needs the nightly rustfmt)
	cargo +nightly fmt --all -- --config $(RUSTFMT_NIGHTLY_OPTS)

fmt-check: ## cargo fmt --check
	cargo fmt --all --check

deny: ## cargo deny check
	cargo deny check

check: fmt-check lint test deny ## every gate, in order

install: ## cargo install --path crates/nightjar-cli
	cargo install --path crates/nightjar-cli

clean: ## cargo clean
	cargo clean
