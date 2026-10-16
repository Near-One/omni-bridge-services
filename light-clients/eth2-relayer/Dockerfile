# syntax=docker/dockerfile:1
# Build context is the repo root; this crate is its own workspace.
FROM rust:1.96.0-bookworm AS builder
RUN apt-get update && apt-get install -y \
    cmake \
    build-essential \
    libclang-dev \
    pkg-config \
    libssl-dev \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY light-clients/eth2-relayer ./
# See omni-relayer/Dockerfile for why the cache mounts carry per-image ids.
RUN --mount=type=cache,id=eth2-relayer-cargo-registry,sharing=locked,target=/usr/local/cargo/registry \
    --mount=type=cache,id=eth2-relayer-cargo-git,sharing=locked,target=/usr/local/cargo/git \
    --mount=type=cache,id=eth2-relayer-target,sharing=locked,target=/app/target \
    cargo build --release --locked && \
    cp target/release/relayer /tmp/


FROM debian:bookworm-slim AS runtime
RUN apt-get update && apt-get install -y \
    ca-certificates \
    libssl3 \
    && rm -rf /var/lib/apt/lists/*
RUN useradd -m -u 1001 appuser
WORKDIR /app
# Deployments invoke `./relayer` from /app; keep both the path and the name.
COPY --from=builder /tmp/relayer /app/relayer
USER appuser
HEALTHCHECK --interval=30s --timeout=10s --start-period=5s --retries=3 \
    CMD ./relayer validate-config || exit 1
CMD ["./relayer", "run-job"]
