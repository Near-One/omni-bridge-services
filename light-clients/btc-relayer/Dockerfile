# syntax=docker/dockerfile:1
# Build context is the repo root; this crate is its own workspace.
FROM rust:1.96.0-bookworm AS builder
RUN apt-get update && apt-get install -y \
    build-essential \
    pkg-config \
    libssl-dev \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY light-clients/btc-relayer ./

# Chain variant: "" (bitcoin/litecoin), "zcash" or "dogecoin".
ARG FEATURES=""
# See omni-relayer/Dockerfile for why the cache mounts carry per-image ids.
RUN --mount=type=cache,id=btc-relayer-cargo-registry,sharing=locked,target=/usr/local/cargo/registry \
    --mount=type=cache,id=btc-relayer-cargo-git,sharing=locked,target=/usr/local/cargo/git \
    --mount=type=cache,id=btc-relayer-target,sharing=locked,target=/app/target \
    if [ -n "$FEATURES" ]; then \
        cargo build --release --locked --features "$FEATURES"; \
    else \
        cargo build --release --locked; \
    fi && \
    cp target/release/btc-relayer /tmp/


FROM debian:bookworm-slim AS runtime
RUN apt-get update && apt-get install -y \
    ca-certificates \
    libssl3 \
    && rm -rf /var/lib/apt/lists/*
# Deployments invoke /bin/relayer; keep that path.
COPY --from=builder /tmp/btc-relayer /bin/relayer
ENTRYPOINT ["/bin/relayer"]
