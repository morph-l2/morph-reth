# Internal Makefile for building and deploying morph-reth binaries to AWS S3.
# Mirrors the target naming convention from go-ethereum/MakefileBuild.mk.
#
# New release system: MakefileBuild.mk only produces build artifacts.
# Service start commands live in MakefileEc2.mk / MakefileEks.mk.

DIST_DIR         = dist
BINARY           = morph-reth
TARBALL          = morph-reth.tar.gz
CARGO_TARGET_DIR ?= target
# Production deploys use the deterministic `reproducible` profile by default.
# Override with PROFILE=profiling when line-table symbols are needed for
# diagnostics. `maxperf` remains available for throughput-sensitive builds.
PROFILE          ?= reproducible

# Production EC2 binaries intentionally use the default x86_64 CPU baseline.
# Keep this empty so the binary remains compatible across EC2 instance types.
RUSTFLAGS_ARCH :=

# Keep build metadata stable across rebuilds of the same commit.
SOURCE_DATE_EPOCH ?= $(shell git log -1 --format=%ct HEAD)

# Internal build hosts cannot reach GitHub, so the Cargo git dependencies (reth,
# discv5) come from the committed vendor/ directory instead. Plain cargo commands
# never load this file, so external users keep fetching them from GitHub.
VENDOR_CONFIG := --config vendor/cargo-config.toml

# Cargo keeps no checksums for vendored git sources and would silently build a
# stale vendor/, so refuse to build unless vendor/ was generated for the commits
# pinned in Cargo.lock. Regenerate with `make vendor` on a host with GitHub access.
CHECK_VENDOR = LC_ALL=C grep '^source = "git+' Cargo.lock | LC_ALL=C sort -u | diff -q - vendor/SOURCES >/dev/null \
	|| { echo "error: vendor/ does not match the git dependencies in Cargo.lock; run 'make vendor'" >&2; exit 1; }

define cargo_build_and_upload
	if [ ! -d $(DIST_DIR) ]; then mkdir -p $(DIST_DIR); fi
	$(CHECK_VENDOR)
	CARGO_NET_GIT_FETCH_WITH_CLI=true cargo fetch --locked $(VENDOR_CONFIG)
	SRC=$$(ls -d "$${CARGO_HOME:-$$HOME/.cargo}"/registry/src/*/ | head -1); \
	SRC=$${SRC%/}; \
	SOURCE_DATE_EPOCH="$(SOURCE_DATE_EPOCH)" LC_ALL=C TZ=UTC \
	RUSTFLAGS="--remap-path-prefix=$$(pwd)=/morph-reth --remap-path-prefix=$${SRC}=/registry $(RUSTFLAGS_ARCH)" \
	cargo build --bin $(BINARY) --profile "$(PROFILE)" --locked $(VENDOR_CONFIG) --target-dir "$(CARGO_TARGET_DIR)"
	cp "$(CARGO_TARGET_DIR)/$(PROFILE)/$(BINARY)" "$(DIST_DIR)/"
	tar -czvf $(TARBALL) $(DIST_DIR)
	aws s3 cp $(TARBALL) $(1)
endef

# ─── Mainnet ─────────────────────────────────────────────────────────────────

build-bk-prod-morph-prod-mainnet-to-morph-reth:
	$(call cargo_build_and_upload,s3://morph-0582-morph-technical-department-mainnet-data/morph-setup/morph-reth.tar.gz)

# ─── Testnet (Hoodi) ────────────────────────────────────────────────────────

build-bk-prod-morph-prod-testnet-to-morph-reth-hoodi:
	$(call cargo_build_and_upload,s3://morph-0582-morph-technical-department-testnet-data/testnet/hoodi/morph-setup/morph-reth.tar.gz)

# ─── QA Net ──────────────────────────────────────────────────────────────────

build-bk-test-morph-test-qanet-to-morph-reth-qanet:
	$(call cargo_build_and_upload,s3://morph-7637-morph-technical-department-qanet-data/morph-setup/morph-reth.tar.gz)

# ─── Checks ──────────────────────────────────────────────────────────────────

.PHONY: check-vendor
check-vendor:
	@$(CHECK_VENDOR)
