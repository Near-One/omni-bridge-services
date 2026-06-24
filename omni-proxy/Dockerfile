# syntax=docker/dockerfile:1
FROM rust:1.96.0-bookworm AS builder
WORKDIR /app
RUN apt-get update && apt-get install -y cmake && rm -rf /var/lib/apt/lists/*
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/app/target \
    cargo build --release -p omni-proxy && \
    cp target/release/omni-proxy /tmp/


FROM debian:bookworm-slim AS runtime
RUN apt-get update && apt-get install -y \
    ca-certificates \
    libssl3 \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /tmp/omni-proxy /usr/local/bin/
RUN useradd -m -u 1001 appuser
USER appuser
WORKDIR /app
EXPOSE 8080
ENTRYPOINT ["omni-proxy"]
