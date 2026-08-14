# Internal Makefile for building and deploying morph-reth binaries to AWS S3.
# Mirrors the target naming convention from go-ethereum/MakefileEc2.mk.

DIST_DIR         = dist
BINARY           = morph-reth
TARBALL          = morph-reth.tar.gz
CARGO_TARGET_DIR ?= target
# Production deploys go to EC2 via S3. Default to `profiling` (thin LTO +
# line-table debug) — matches the Dockerfile default. Bench data showed
# `maxperf` (fat LTO + cgu=1) regresses ERC20 median TPS -10% and explodes
# the import_ms long tail (p99.9 90ms, max 456ms) while only winning eth-
# transfer by 7%. `profiling` keeps most of the throughput gain, no ERC20
# regression, and ships line-table symbols for incident diagnosis.
PROFILE          ?= profiling

# Production EC2 binaries intentionally use the default x86_64 CPU baseline.
# Keep this empty so the binary remains compatible across EC2 instance types.
RUSTFLAGS_ARCH :=

# Keep build metadata stable across rebuilds of the same commit.
SOURCE_DATE_EPOCH ?= $(shell git log -1 --format=%ct HEAD)

define cargo_build_and_upload
	if [ ! -d $(DIST_DIR) ]; then mkdir -p $(DIST_DIR); fi
	CARGO_NET_GIT_FETCH_WITH_CLI=true cargo fetch --locked
	SRC=$$(ls -d "$${CARGO_HOME:-$$HOME/.cargo}"/registry/src/*/ | head -1); \
	SRC=$${SRC%/}; \
	SOURCE_DATE_EPOCH="$(SOURCE_DATE_EPOCH)" LC_ALL=C TZ=UTC \
	RUSTFLAGS="--remap-path-prefix=$$(pwd)=/morph-reth --remap-path-prefix=$${SRC}=/registry $(RUSTFLAGS_ARCH)" \
	cargo build --bin $(BINARY) --profile "$(PROFILE)" --locked --target-dir "$(CARGO_TARGET_DIR)"
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
