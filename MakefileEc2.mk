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

define cargo_build_and_upload
	if [ ! -d $(DIST_DIR) ]; then mkdir -p $(DIST_DIR); fi
	CARGO_NET_GIT_FETCH_WITH_CLI=true cargo build --bin $(BINARY) --profile "$(PROFILE)" --target-dir "$(CARGO_TARGET_DIR)"
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
