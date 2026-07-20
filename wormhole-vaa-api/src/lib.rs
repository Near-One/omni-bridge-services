//! Self-hosted, wormholescan-compatible Wormhole VAA API.
//!
//! A Wormhole spy gRPC subscriber feeds a Redis-backed VAA store; an axum REST
//! service serves the two wormholescan endpoints the omni-bridge relayer calls,
//! resolving `txHash` lookups on demand against chain RPCs (via omni-proxy).

pub mod address;
pub mod api;
pub mod config;
pub mod errors;
pub mod metrics;
pub mod proxy_client;
pub mod resolver;
pub mod spy;
pub mod store;
pub mod vaa;
pub mod wormholescan;

/// Generated Wormhole spy gRPC client (from `proto/spy.proto`).
#[allow(clippy::all, clippy::pedantic, clippy::as_conversions)]
pub mod proto {
    tonic::include_proto!("spy.v1");
}
