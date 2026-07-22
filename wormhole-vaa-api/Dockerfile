# syntax=docker/dockerfile:1
FROM rust:1.96.0-bookworm AS builder
WORKDIR /app
# No system protoc needed: the build script uses the vendored protoc
# (protoc-bin-vendored). No cmake needed either (no pingora/boringssl here).
COPY . .
# Per-image cache ids + sharing=locked: the release workflow builds several images
# concurrently on a shared BuildKit builder. With the default (shared, target-derived)
# cache id they contend on the same cargo git-db lock, which leaves a stale
# `config.lock` and fails the next build ("failed to create locked file ...: File
# exists"). Distinct ids isolate each image; `locked` serializes same-image builds.
RUN --mount=type=cache,id=wormhole-vaa-api-cargo-registry,sharing=locked,target=/usr/local/cargo/registry \
    --mount=type=cache,id=wormhole-vaa-api-cargo-git,sharing=locked,target=/usr/local/cargo/git \
    --mount=type=cache,id=wormhole-vaa-api-target,sharing=locked,target=/app/target \
    cargo build --release --locked -p wormhole-vaa-api && \
    cp target/release/wormhole-vaa-api /tmp/


FROM debian:bookworm-slim AS runtime
RUN apt-get update && apt-get install -y \
    ca-certificates \
    libssl3 \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /tmp/wormhole-vaa-api /usr/local/bin/
RUN useradd -m -u 1001 appuser
USER appuser
WORKDIR /app
EXPOSE 8080
# Args (network/config/port) come from the Deployment, e.g.:
#   ["--network", "mainnet", "--config", "/config/config.toml", "--port", "8080"]
ENTRYPOINT ["wormhole-vaa-api"]
