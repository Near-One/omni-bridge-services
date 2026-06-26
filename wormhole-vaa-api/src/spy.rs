//! Wormhole spy gRPC subscriber.
//!
//! Opens `SubscribeSignedVAA` with an `EmitterFilter` per configured `(chain, emitter)`
//! so only our VAAs are streamed, parses each, and stores the raw bytes. Reconnects
//! with capped exponential backoff (re-sending the filters) and tracks liveness for
//! `/healthz`. The spy has no backfill — a VAA missed while disconnected is served by
//! the proxy's wormholescan fallback (our 404).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use tokio_stream::StreamExt;
use tonic::transport::Endpoint;
use tracing::{debug, info, warn};

use crate::address::emitter_hex;
use crate::config::Config;
use crate::proto::spy_rpc_service_client::SpyRpcServiceClient;
use crate::proto::{EmitterFilter, FilterEntry, SubscribeSignedVaaRequest, filter_entry::Filter};
use crate::store::Store;
use crate::vaa::parse_vaa_id;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);
const BACKOFF_INITIAL: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(30);

/// Shared liveness/observability state for the spy stream.
#[derive(Default)]
pub struct SpyStatus {
    connected: AtomicBool,
    total_ingested: AtomicU64,
}

impl SpyStatus {
    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    pub fn total_ingested(&self) -> u64 {
        self.total_ingested.load(Ordering::Relaxed)
    }
}

/// Run the subscriber forever, reconnecting with backoff. Never returns.
pub async fn run(config: Arc<Config>, store: Store, status: Arc<SpyStatus>) {
    let mut backoff = BACKOFF_INITIAL;
    loop {
        // `subscribe_once` resets `backoff` to BACKOFF_INITIAL once a connection is
        // established, so the exponential growth below only accrues across consecutive
        // *connect* failures — a long-lived healthy stream that drops reconnects fast.
        match subscribe_once(&config, &store, &status, &mut backoff).await {
            Ok(()) => warn!("spy stream closed by server; reconnecting"),
            Err(e) => warn!("spy stream error: {e}; reconnecting in {backoff:?}"),
        }
        status.connected.store(false, Ordering::Relaxed);
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(BACKOFF_MAX);
    }
}

async fn subscribe_once(
    config: &Config,
    store: &Store,
    status: &SpyStatus,
    backoff: &mut Duration,
) -> anyhow::Result<()> {
    let uri = if config.spy_addr.starts_with("http://") || config.spy_addr.starts_with("https://") {
        config.spy_addr.clone()
    } else {
        format!("http://{}", config.spy_addr)
    };

    let channel = Endpoint::from_shared(uri)?
        .connect_timeout(CONNECT_TIMEOUT)
        .http2_keep_alive_interval(KEEPALIVE_INTERVAL)
        .keep_alive_while_idle(true)
        .connect()
        .await?;

    let mut client = SpyRpcServiceClient::new(channel);
    let filters = build_filters(config)?;
    let filter_count = filters.len();
    let request = SubscribeSignedVaaRequest { filters };

    let mut stream = client.subscribe_signed_vaa(request).await?.into_inner();
    status.connected.store(true, Ordering::Relaxed);
    *backoff = BACKOFF_INITIAL;
    info!(filters = filter_count, "spy connected");

    while let Some(message) = stream.next().await {
        let message = message?;
        ingest(&message.vaa_bytes, store, status).await;
    }
    Ok(())
}

fn build_filters(config: &Config) -> anyhow::Result<Vec<FilterEntry>> {
    let mut filters = Vec::with_capacity(config.chains.len());
    for chain in &config.chains {
        let emitter = chain.emitter_bytes()?;
        filters.push(FilterEntry {
            filter: Some(Filter::EmitterFilter(EmitterFilter {
                chain_id: i32::from(chain.wh_chain_id),
                emitter_address: emitter_hex(&emitter),
            })),
        });
    }
    Ok(filters)
}

async fn ingest(vaa_bytes: &[u8], store: &Store, status: &SpyStatus) {
    let id = match parse_vaa_id(vaa_bytes) {
        Ok(id) => id,
        Err(e) => {
            debug!("skipping unparseable VAA ({} bytes): {e}", vaa_bytes.len());
            return;
        }
    };

    // A real, fully-signed VAA always carries guardian signatures. Refuse to store an
    // unsigned/degenerate one so it can never be served as a 200 (defense-in-depth; a
    // conforming spy only emits quorum VAAs on this channel).
    if crate::vaa::signature_count(vaa_bytes) == 0 {
        debug!(
            chain = id.emitter_chain,
            sequence = id.sequence,
            "skipping VAA with zero signatures"
        );
        return;
    }

    match store.put_vaa(&id, vaa_bytes).await {
        Ok(()) => {
            status.total_ingested.fetch_add(1, Ordering::Relaxed);
            debug!(
                chain = id.emitter_chain,
                sequence = id.sequence,
                "stored VAA"
            );
        }
        Err(e) => warn!(
            chain = id.emitter_chain,
            sequence = id.sequence,
            "failed to store VAA: {e}"
        ),
    }
}
