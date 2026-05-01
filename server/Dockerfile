# syntax=docker/dockerfile:1.7

# Build stage: musl-cross compile so the final binary has zero runtime libc deps.
FROM rust:1-slim-bookworm AS builder

WORKDIR /build
RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config musl-tools \
 && rm -rf /var/lib/apt/lists/*

ARG TARGET=x86_64-unknown-linux-musl
# x86_64 Linux builds inherit `-C target-cpu=x86-64-v3` (AVX2/BMI/FMA baseline)
# from .cargo/config.toml. The AVX2 step kernel is selected automatically via
# `cfg(target_feature = "avx2")`. ARM64 / non-AVX2 builds use the scalar path.

COPY rust-toolchain.toml Cargo.toml Cargo.lock ./
COPY .cargo      .cargo
RUN rustup target add $TARGET
COPY simulation  simulation
COPY protocol    protocol
COPY server      server

# Cache mounts hold the cargo registry and the target dir across builds; copy
# the binary out before the RUN ends since the mounts unmount after.
RUN --mount=type=cache,target=/usr/local/cargo/registry,id=lazos-registry \
    --mount=type=cache,target=/build/target,id=lazos-target-${TARGET} \
    cargo build --release --target $TARGET -p server \
 && strip target/$TARGET/release/lazos-server \
 && cp target/$TARGET/release/lazos-server /lazos-server

# Runtime: distroless static (no libc, just /etc/passwd + nonroot user + tzdata).
# UID 65532 = nonroot. Read-only rootfs hardening lives in docker-compose.yml.
FROM gcr.io/distroless/static-debian12:nonroot

WORKDIR /app
COPY --from=builder /lazos-server /app/lazos-server
COPY config /app/config

ENV NETGOL_CONFIG=/app/config/server.toml
ENV RUST_LOG=info

EXPOSE 8080 9090
USER nonroot
ENTRYPOINT ["/app/lazos-server"]
