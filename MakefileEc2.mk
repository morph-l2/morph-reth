# Internal Makefile for building and deploying morph-reth binaries to AWS S3.
# Mirrors the target naming convention from go-ethereum/MakefileEc2.mk.

DIST_DIR = dist
BINARY   = morph-reth
TARBALL  = morph-reth.tar.gz

define cargo_build_and_upload
	if [ ! -d $(DIST_DIR) ]; then mkdir -p $(DIST_DIR); fi
	cargo build --release --bin $(BINARY)
	cp target/release/$(BINARY) $(DIST_DIR)/
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
