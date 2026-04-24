# Internal Makefile for building and deploying morph-reth binaries to AWS S3.
# Mirrors the target naming convention from go-ethereum/MakefileEc2.mk.

DIST_DIR         = dist
BINARY           = morph-reth
TARBALL          = morph-reth.tar.gz
CARGO_TARGET_DIR ?= target
# Production deploys go to EC2 via S3. Default to `maxperf` (fat LTO +
# single codegen unit) for peak throughput on prod nodes — matches the
# Dockerfile default. Override with `PROFILE=profiling` to keep line-table
# symbols for flame graphs when diagnosing a prod incident.
PROFILE          ?= maxperf

# Architecture-conditional RUSTFLAGS based on the build host's CPU. EC2
# build hosts native-compile and upload to S3 → prod hosts pull. As long
# as build host and prod host share architecture, the v3 baseline is
# safe for any 2015+ x86_64 EC2 instance type (m5/m6i/c5/c6i/r5/r6i etc.).
# Graviton ARM hosts skip the flag.
ARCH := $(shell uname -m)
ifeq ($(ARCH),x86_64)
RUSTFLAGS_ARCH := -C target-cpu=x86-64-v3 -C target-feature=+pclmulqdq
else
RUSTFLAGS_ARCH :=
endif

define cargo_build_and_upload
	if [ ! -d $(DIST_DIR) ]; then mkdir -p $(DIST_DIR); fi
	CARGO_NET_GIT_FETCH_WITH_CLI=true RUSTFLAGS="$(RUSTFLAGS_ARCH)" cargo build --bin $(BINARY) --profile "$(PROFILE)" --target-dir "$(CARGO_TARGET_DIR)"
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
