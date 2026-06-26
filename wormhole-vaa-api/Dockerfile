# syntax=docker/dockerfile:1
FROM rust:1.96.0-bookworm AS builder
WORKDIR /app
# No system protoc needed: the build script uses the vendored protoc
# (protoc-bin-vendored). No cmake needed either (no pingora/boringssl here).
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/app/target \
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
