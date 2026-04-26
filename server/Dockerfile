# syntax=docker/dockerfile:1.7

# Build stage: musl-cross compile so the final binary has zero runtime libc deps.
# Layer-cache deps with the standard manifest+stub trick.
FROM rust:1-slim-bookworm AS builder

WORKDIR /build
RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config musl-tools \
 && rm -rf /var/lib/apt/lists/*

ARG TARGET=x86_64-unknown-linux-musl
# AMD64 default: AVX2 step kernel + x86-64-v3 baseline (Haswell+/Excavator+).
# ARM64 / non-AVX2 builds must override both ARGs to empty strings.
ARG FEATURES=avx2
ARG RUSTFLAGS="-C target-cpu=x86-64-v3"
ENV RUSTFLAGS=$RUSTFLAGS

# Copy rust-toolchain.toml first so rustup picks the right channel before we
# install the musl target onto it.
COPY rust-toolchain.toml ./
RUN rustup target add $TARGET

# Pre-cache dependency compilation.
COPY Cargo.toml Cargo.lock ./
COPY simulation/Cargo.toml simulation/Cargo.toml
COPY protocol/Cargo.toml   protocol/Cargo.toml
COPY server/Cargo.toml     server/Cargo.toml
RUN mkdir -p simulation/src protocol/src server/src \
 && echo "fn main() {}" > server/src/main.rs \
 && echo ""             > simulation/src/lib.rs \
 && echo ""             > protocol/src/lib.rs \
 && cargo build --release --target $TARGET ${FEATURES:+--features $FEATURES} -p server

# Real build. The COPY commands replace the stub source files with the real
# ones; cargo's fingerprinting detects the content change and rebuilds the
# workspace crates while reusing third-party deps from cache.
COPY simulation/src simulation/src
COPY protocol/src   protocol/src
COPY server/src     server/src
RUN cargo clean --release --target $TARGET -p simulation -p protocol -p server \
 && cargo build --release --target $TARGET ${FEATURES:+--features $FEATURES} -p server \
 && strip target/$TARGET/release/lazos-server \
 && cp target/$TARGET/release/lazos-server /lazos-server

# Runtime: distroless static (no libc, just /etc/passwd + nonroot user + tzdata).
# UID 65532 = nonroot. Read-only rootfs hardening lives in docker-compose.yml.
FROM gcr.io/distroless/static-debian12:nonroot

WORKDIR /app
COPY --from=builder /lazos-server /app/lazos-server
COPY config /app/config

ENV LAZOS_CONFIG=/app/config/server.toml
ENV RUST_LOG=info

EXPOSE 8080 9090
USER nonroot
ENTRYPOINT ["/app/lazos-server"]
