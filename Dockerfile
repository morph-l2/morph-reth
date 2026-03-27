FROM lukemathwalker/cargo-chef:latest-rust-1 AS chef
WORKDIR /app

LABEL org.opencontainers.image.source=https://github.com/morph-l2/morph-reth
LABEL org.opencontainers.image.licenses="MIT OR Apache-2.0"

# reth-mdbx-sys requires libclang for bindgen
RUN apt-get update && apt-get -y upgrade && apt-get install -y libclang-dev pkg-config

# Generate dependency recipe
FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# Build dependencies + application
FROM chef AS builder
COPY --from=planner /app/recipe.json recipe.json

# Build profile, release by default
ARG BUILD_PROFILE=release
ENV BUILD_PROFILE=$BUILD_PROFILE

# Extra Cargo flags
ARG RUSTFLAGS=""
ENV RUSTFLAGS="$RUSTFLAGS"

# Build dependencies (cached layer)
RUN cargo chef cook --profile $BUILD_PROFILE --recipe-path recipe.json

# Build the application
COPY . .
RUN cargo build --profile $BUILD_PROFILE --locked --bin morph-reth

# Copy binary to a fixed location (ARG not resolved in COPY)
RUN cp /app/target/$BUILD_PROFILE/morph-reth /app/morph-reth

# Minimal runtime image
FROM debian:bookworm-slim AS runtime
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && \
    rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/morph-reth /usr/local/bin/

EXPOSE 8545 8546 8551 30303 30303/udp

ENTRYPOINT ["/usr/local/bin/morph-reth"]
