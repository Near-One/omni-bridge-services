use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result};
use omni_connector::OmniConnector;
use omni_types::ChainKind;
use tracing::{info, warn};

use crate::{config, metrics};

/// Alerts on `HyperCore` transfers the bridge committed and nobody submitted.
/// Reads contract state, not logs: the event never arriving is the failure it
/// has to survive. It cannot self-heal — the payload lives only in that event.
#[tracing::instrument(name = "hyperevm_pre_init_watchdog", skip_all)]
pub async fn start_hyperevm_pre_init_watchdog(
    config: Arc<config::Config>,
    omni_connector: Arc<OmniConnector>,
) -> Result<()> {
    let watchdog = config
        .hyperevm_pre_init_watchdog()
        .context("HyperEVM pre-init watchdog is not configured")?;
    let interval = Duration::from_secs(watchdog.polling_interval_secs);
    let stale_after_secs = watchdog.stale_after_secs;
    let initial_lookback = watchdog.initial_lookback;

    let stale_gauge = Arc::new(AtomicU64::new(0));
    metrics::register_hl_stale_pre_init_transfers(&stale_gauge);

    info!(
        polling_interval_secs = watchdog.polling_interval_secs,
        stale_after_secs, "Starting HyperEVM pre-init watchdog"
    );

    // When this process first saw the commitment, not when it was queued: a
    // restart resets the clock.
    let mut first_seen: BTreeMap<u64, i64> = BTreeMap::new();
    let mut next_nonce: Option<u64> = None;

    loop {
        tokio::time::sleep(interval).await;

        let evm_bridge_client = match omni_connector.evm_bridge_client(ChainKind::HyperEvm) {
            Ok(evm_bridge_client) => evm_bridge_client,
            Err(err) => {
                warn!(?err, "Failed to get HyperEvm bridge client");
                continue;
            }
        };

        let cursor = match evm_bridge_client.current_origin_nonce().await {
            Ok(cursor) => cursor,
            Err(err) => {
                warn!(?err, "Failed to read the origin nonce cursor");
                continue;
            }
        };

        let mut nonce =
            next_nonce.unwrap_or_else(|| cursor.saturating_sub(initial_lookback).saturating_add(1));

        while nonce <= cursor {
            match evm_bridge_client.is_init_transfer_pending(nonce).await {
                Ok(true) => {
                    first_seen
                        .entry(nonce)
                        .or_insert_with(|| chrono::Utc::now().timestamp());
                }
                Ok(false) => {}
                Err(err) => {
                    // Leave the cursor on this nonce so the next tick re-reads it.
                    warn!(origin_nonce = nonce, ?err, "Failed to read a commitment");
                    break;
                }
            }

            nonce += 1;
        }

        next_nonce = Some(nonce);

        let now = chrono::Utc::now().timestamp();
        let mut stale = 0u64;
        let mut submitted = Vec::new();

        for (&nonce, &seen_at) in &first_seen {
            match evm_bridge_client.is_init_transfer_pending(nonce).await {
                Ok(true) => {
                    if now - seen_at >= stale_after_secs {
                        stale += 1;
                        warn!(
                            origin_nonce = nonce,
                            pending_for_secs = now - seen_at,
                            "HyperCore transfer is committed but not submitted; its PreInitTransfer never reached the relayer. Submit it with bridge-cli"
                        );
                    }
                }
                Ok(false) => submitted.push(nonce),
                Err(err) => warn!(origin_nonce = nonce, ?err, "Failed to re-read a commitment"),
            }
        }

        for nonce in submitted {
            first_seen.remove(&nonce);
        }

        stale_gauge.store(stale, Ordering::Relaxed);
    }
}
