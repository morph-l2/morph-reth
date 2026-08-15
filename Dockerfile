FROM lukemathwalker/cargo-chef:latest-rust-1.95-trixie AS chef
WORKDIR /app

# reth-mdbx-sys requires libclang for bindgen
RUN apt-get update && \
    apt-get install -y --no-install-recommends libclang-dev pkg-config && \
    rm -rf /var/lib/apt/lists/*

# Generate dependency recipe
FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# Build dependencies + application
FROM chef AS builder
COPY --from=planner /app/recipe.json recipe.json

# Build profile. Defaults to `profiling` (thin LTO + line-table debug info).
# Bench data on M4 Pro showed `maxperf` (fat LTO + cgu=1) hitting a hard
# trade-off on ERC20 workloads: peak `eth_transfer` throughput +13% but
# median ERC20 TPS -10% and import_ms long tail catastrophic (p99.9 90ms,
# max 456ms, 0.36% blocks > 50ms vs profiling's 0%). `profiling` keeps
# ~80% of maxperf's hot-path gain, no ERC20 regression, AND gives prod
# binaries line-table symbols so incidents produce clean stack traces.
# Override with `--build-arg BUILD_PROFILE=maxperf` for eth-heavy nodes
# that don't care about long tail, or `release` for slim/stripped.
ARG BUILD_PROFILE=profiling
ENV BUILD_PROFILE=$BUILD_PROFILE

# Architecture-conditional RUSTFLAGS:
# - linux/amd64 (default): enable x86-64-v3 baseline (Haswell+ / Excavator+)
#   plus pclmulqdq (carry-less multiply, used by keccak/GHASH but not auto-
#   enabled by v3). Matches upstream reth release defaults. Pre-2013 Intel
#   and pre-2015 AMD CPUs will SIGILL on these binaries — pass an explicit
#   `--build-arg RUSTFLAGS=""` for those hosts.
# - other platforms (linux/arm64, etc.): no architecture flag.
ARG TARGETPLATFORM
ARG RUSTFLAGS=

# Build dependencies (cached layer)
RUN if [ -z "$RUSTFLAGS" ] && [ "$TARGETPLATFORM" = "linux/amd64" ]; then \
        export RUSTFLAGS="-C target-cpu=x86-64-v3 -C target-feature=+pclmulqdq"; \
    fi && \
    cargo chef cook --profile $BUILD_PROFILE --recipe-path recipe.json

# Build the application
COPY . .
RUN if [ -z "$RUSTFLAGS" ] && [ "$TARGETPLATFORM" = "linux/amd64" ]; then \
        export RUSTFLAGS="-C target-cpu=x86-64-v3 -C target-feature=+pclmulqdq"; \
    fi && \
    cargo build --profile $BUILD_PROFILE --locked --bin morph-reth

# Copy binary to a fixed location (ARG not resolved in COPY)
RUN cp /app/target/$BUILD_PROFILE/morph-reth /app/morph-reth

# Runtime must provide glibc >= the builder (Debian Trixie / Ubuntu 24.04).
# Matches upstream reth's `ubuntu:24.04` runtime stage.
FROM ubuntu:24.04 AS runtime

LABEL org.opencontainers.image.source=https://github.com/morph-l2/morph-reth
LABEL org.opencontainers.image.licenses="MIT OR Apache-2.0"
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && \
    useradd --system --create-home --home-dir /var/lib/morph-reth --shell /usr/sbin/nologin morph-reth && \
    rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/morph-reth /usr/local/bin/

EXPOSE 8545 8546 8551 30303 30303/udp

WORKDIR /var/lib/morph-reth
USER morph-reth
ENTRYPOINT ["/usr/local/bin/morph-reth"]
