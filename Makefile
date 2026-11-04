.PHONY: help build test lint fmt fmt-check example bench check

help: ## Show available targets
	@grep -E '^[a-z-]+:.*##' $(MAKEFILE_LIST) | awk -F':.*## ' '{printf "  %-12s %s\n", $$1, $$2}'

build: ## Compile the library
	cargo build

test: ## Run unit, integration and doc tests
	cargo test

lint: ## Clippy with warnings as errors
	cargo clippy --all-targets -- -D warnings

fmt: ## Format the code
	cargo fmt

fmt-check: ## Check formatting
	cargo fmt --check

example: ## Render the demo report
	cargo run --example report

bench: ## Compare native, Python-module and inline-Python filter cost
	cargo run --release --example bench

check: fmt-check lint test ## Everything CI runs
