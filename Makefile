# Heavily inspired by Lighthouse: https://github.com/sigp/lighthouse/blob/stable/Makefile
.DEFAULT_GOAL := help

GIT_SHA ?= $(shell git rev-parse HEAD)
GIT_TAG ?= $(shell git describe --tags --abbrev=0 2>/dev/null || git rev-parse --short HEAD)
BIN_DIR = "dist/bin"

CARGO_TARGET_DIR ?= target

# Cargo profile for builds. Default is release; CI can override.
PROFILE ?= release

# Extra flags for Cargo
CARGO_INSTALL_EXTRA_FLAGS ?=

# The docker image name
DOCKER_IMAGE_NAME ?= ghcr.io/morph-l2/morph-reth

##@ Help

.PHONY: help
help: ## Display this help.
	@awk 'BEGIN {FS = ":.*##"; printf "Usage:\n  make \033[36m<target>\033[0m\n"} /^[a-zA-Z_0-9-]+:.*?##/ { printf "  \033[36m%-30s\033[0m %s\n", $$1, $$2 } /^##@/ { printf "\n\033[1m%s\033[0m\n", substr($$0, 5) } ' $(MAKEFILE_LIST)

##@ Build

.PHONY: build
build: ## Build the morph-reth binary into the `target` directory.
	cargo build --bin morph-reth --profile "$(PROFILE)"

.PHONY: build-debug
build-debug: ## Build the morph-reth binary into `target/debug`.
	cargo build --bin morph-reth

.PHONY: install
install: ## Build and install the morph-reth binary under `$(CARGO_HOME)/bin`.
	cargo install --path bin/morph-reth --bin morph-reth --force --locked \
		--profile "$(PROFILE)" \
		$(CARGO_INSTALL_EXTRA_FLAGS)

# Create a `.tar.gz` containing the morph-reth binary for a specific target.
define tarball_release_binary
	cp $(CARGO_TARGET_DIR)/$(PROFILE)/morph-reth $(BIN_DIR)/morph-reth
	cd $(BIN_DIR) && \
		tar -czf morph-reth-$(GIT_TAG).tar.gz morph-reth && \
		rm morph-reth
endef

.PHONY: build-release-tarballs
build-release-tarballs: ## Build and package morph-reth into a `.tar.gz` in the `dist/bin` directory.
	[ -d $(BIN_DIR) ] || mkdir -p $(BIN_DIR)
	$(MAKE) build
	$(call tarball_release_binary)

##@ Test

.PHONY: test
test: ## Run all tests (unit only, fast).
	cargo test --all

.PHONY: test-unit
test-unit: ## Run unit tests with cargo-nextest (install with: cargo install cargo-nextest).
	cargo nextest run --locked --workspace -E 'kind(lib)' -E 'kind(bin)' -E 'kind(proc-macro)'

.PHONY: test-e2e
test-e2e: ## Run e2e integration tests (spawns full nodes, slower).
	cargo nextest run --locked -p morph-node --features test-utils -E 'binary(it)'

.PHONY: test-all
test-all: test test-e2e ## Run all tests including e2e.

##@ Lint

.PHONY: fmt
fmt: ## Check code formatting.
	cargo fmt --all -- --check

.PHONY: fmt-fix
fmt-fix: ## Auto-fix code formatting.
	cargo fmt --all

.PHONY: clippy
clippy: ## Run clippy lints.
	cargo clippy --all --all-targets -- -D warnings

.PHONY: clippy-e2e
clippy-e2e: ## Run clippy on morph-node with e2e test-utils feature.
	cargo clippy -p morph-node --all-targets --features test-utils -- -D warnings

.PHONY: clippy-fix
clippy-fix: ## Run clippy and auto-fix warnings.
	cargo clippy --all --all-targets --fix --allow-staged --allow-dirty -- -D warnings

.PHONY: lint
lint: fmt clippy ## Run all lints (fmt + clippy).

.PHONY: fix-lint
fix-lint: clippy-fix fmt-fix ## Auto-fix all lint issues.

##@ Docker

# Note: Requires Docker with buildx support.
# Setup example:
#   docker buildx create --use --driver docker-container --name cross-builder
.PHONY: docker-build-push
docker-build-push: ## Build and push a Docker image tagged with the latest git tag.
	$(call docker_build_push,$(GIT_TAG),$(GIT_TAG))

.PHONY: docker-build-push-latest
docker-build-push-latest: ## Build and push a Docker image tagged with the latest git tag and `latest`.
	$(call docker_build_push,$(GIT_TAG),latest)

.PHONY: docker-build-push-git-sha
docker-build-push-git-sha: ## Build and push a Docker image tagged with the latest git sha.
	$(call docker_build_push,$(GIT_SHA),$(GIT_SHA))

define docker_build_push
	docker buildx build --file ./Dockerfile . \
		--platform linux/amd64 \
		--tag $(DOCKER_IMAGE_NAME):$(1) \
		--tag $(DOCKER_IMAGE_NAME):$(2) \
		--provenance=false \
		--push
endef

##@ Other

.PHONY: clean
clean: ## Clean build artifacts and the dist directory.
	cargo clean
	rm -rf $(BIN_DIR)
