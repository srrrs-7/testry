# testry — all targets run inside containers (docs/adr/0002).
# The host needs only a container engine and compose.

SHELL := /bin/sh

# `docker compose` (plugin) when available, otherwise standalone docker-compose.
COMPOSE ?= $(shell if docker compose version >/dev/null 2>&1; then echo "docker compose"; else echo "docker-compose"; fi)

DEV     := $(COMPOSE) -f compose.yml
GATES   := $(COMPOSE) -f compose.test.yml
RUN_DEV := $(DEV) run --rm dev

.DEFAULT_GOAL := help
.PHONY: help image shell lock fetch check build fmt fmt-check lint test ci bench testry clean

help: ## Show targets
	@awk 'BEGIN {FS = ":.*## "} /^[a-zA-Z_-]+:.*## / {printf "  %-10s %s\n", $$1, $$2}' $(MAKEFILE_LIST)

image: ## Build the dev image (pinned Rust toolchain)
	$(DEV) build dev

shell: ## Open a shell in the dev container
	$(RUN_DEV) bash

lock: ## Regenerate Cargo.lock (network; review the diff)
	$(RUN_DEV) cargo generate-lockfile

fetch: ## Download locked dependencies into the cargo volume (network)
	$(RUN_DEV) cargo fetch --locked

check: ## cargo check for the whole workspace
	$(RUN_DEV) cargo check --workspace --all-targets --locked

build: ## Build the testry binary (debug)
	$(RUN_DEV) cargo build --workspace --locked

fmt: ## Format sources in place
	$(RUN_DEV) cargo fmt --all

fmt-check: ## Gate: formatting (no network)
	$(GATES) run --rm fmt

lint: ## Gate: clippy with -D warnings (no network)
	$(GATES) run --rm clippy

test: ## Gate: cargo test --workspace --locked (no network)
	$(GATES) run --rm test

ci: fmt-check lint test ## All gates of spec section 19

bench: ## Benchmarks (spec section 20): release build, raw samples in docs/benchmarks/ (no network)
	$(GATES) run --rm bench

testry: ## Run the CLI in the dev container: make testry ARGS="plan --mode full"
	$(RUN_DEV) cargo run --quiet --locked -p testry-cli -- $(ARGS)

clean: ## Remove containers and the cargo volumes of this project
	$(DEV) down --volumes
