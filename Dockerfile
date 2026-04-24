FROM lukemathwalker/cargo-chef:latest-rust-1 AS chef
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

# Build profile. Defaults to `maxperf` (fat LTO + single codegen unit) for
# peak throughput in production containers. Link time is noticeably longer
# but amortized across every block executed. Override with
# `--build-arg BUILD_PROFILE=profiling` to keep line-table symbols for
# flame graphs, or `release` for the slimmer/stripped variant.
ARG BUILD_PROFILE=maxperf
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

# Minimal runtime image
FROM debian:bookworm-slim AS runtime

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
