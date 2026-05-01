# syntax=docker/dockerfile:1.7

ARG RUST_VERSION=1.95

# ── Stage 1: build WASM (scalar + SIMD128) ───────────────────────────────
FROM rust:${RUST_VERSION}-slim AS wasm-builder
RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config libssl-dev \
    && rm -rf /var/lib/apt/lists/*
RUN cargo install wasm-pack --version 0.14.0 --locked
RUN rustup target add wasm32-unknown-unknown
WORKDIR /build
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY simulation/ simulation/
# Workspace manifest requires all member Cargo.toml files to exist,
# even for members that aren't being built.
COPY protocol/Cargo.toml protocol/Cargo.toml
COPY server/Cargo.toml server/Cargo.toml
RUN mkdir -p protocol/src server/src \
    && touch protocol/src/lib.rs server/src/main.rs
RUN cd simulation \
    && wasm-pack build --target web --out-dir ../web/wasm/scalar --features wasm \
    && RUSTFLAGS="-C target-feature=+simd128" \
       wasm-pack build --target web --out-dir ../web/wasm/simd --features wasm

# ── Stage 2: build web app ────────────────────────────────────────────────
FROM node:22-alpine AS web-builder
WORKDIR /build
COPY web/package.json web/package-lock.json ./
RUN npm ci --no-audit --no-fund
COPY web/ ./
COPY --from=wasm-builder /build/web/wasm/ wasm/
RUN npx tsc && npx vite build

# ── Stage 3: serve ────────────────────────────────────────────────────────
FROM nginxinc/nginx-unprivileged:1.27-alpine
COPY --from=web-builder /build/dist /usr/share/nginx/html
COPY web/nginx.conf /etc/nginx/conf.d/default.conf
EXPOSE 8080
