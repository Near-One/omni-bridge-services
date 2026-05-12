use std::{sync::Arc, time::Duration};

use anyhow::{Context, Result};
use near_bridge_client::TransactionOptions;
use omni_connector::OmniConnector;
use omni_types::ChainKind;
use tokio::time::Instant;
use tracing::{info, warn};

use crate::{config, utils};

pub async fn start_active_utxo_manager(
    config: Arc<config::Config>,
    chain: ChainKind,
    omni_connector: Arc<OmniConnector>,
    near_nonce: Arc<utils::nonce::NonceManager>,
) -> Result<()> {
    let settings = config
        .active_utxo_management(chain)
        .with_context(|| format!("Active UTXO management config missing for {chain:?}"))?
        .clone();

    let interval = Duration::from_secs(settings.polling_interval_secs);
    let force_interval = settings.force_interval_secs.map(Duration::from_secs);

    info!(
        "Starting active UTXO manager for {chain:?} (threshold={}, interval={}s, force_interval={:?})",
        settings.utxo_count_threshold, settings.polling_interval_secs, settings.force_interval_secs,
    );

    let mut last_run: Option<Instant> = None;

    loop {
        tokio::time::sleep(interval).await;

        let near_bridge_client = match omni_connector.near_bridge_client() {
            Ok(client) => client,
            Err(err) => {
                warn!("Active UTXO manager: NEAR bridge client unavailable for {chain:?}: {err:?}");
                continue;
            }
        };

        let utxos_num = match near_bridge_client.get_utxo_num(chain).await {
            Ok(num) => num,
            Err(err) => {
                warn!("Active UTXO manager: failed to fetch UTXO count for {chain:?}: {err:?}");
                continue;
            }
        };

        let above_threshold = utxos_num > settings.utxo_count_threshold;
        let force_due = force_interval
            .is_some_and(|forced| last_run.is_none_or(|last| last.elapsed() >= forced));

        if !above_threshold && !force_due {
            info!(
                "Active UTXO manager: {chain:?} has {utxos_num} UTXOs (threshold {}); skipping",
                settings.utxo_count_threshold
            );
            continue;
        }

        let reason = if above_threshold {
            format!("above threshold {}", settings.utxo_count_threshold)
        } else {
            "force interval elapsed".to_string()
        };

        info!(
            "Active UTXO manager: {chain:?} has {utxos_num} UTXOs ({reason}); triggering active_utxo_management"
        );

        let nonce = match near_nonce.reserve_nonce().await {
            Ok(nonce) => Some(nonce),
            Err(err) => {
                warn!("Active UTXO manager: failed to reserve nonce for {chain:?}: {err:?}");
                continue;
            }
        };

        match omni_connector
            .active_utxo_management(
                chain,
                TransactionOptions {
                    nonce,
                    wait_until: near_primitives::views::TxExecutionStatus::ExecutedOptimistic,
                    wait_final_outcome_timeout_sec: None,
                },
            )
            .await
        {
            Ok(tx_hash) => {
                last_run = Some(Instant::now());
                info!(
                    "Active UTXO manager: submitted active_utxo_management for {chain:?}: {tx_hash}"
                );
            }
            Err(err) => {
                warn!("Active UTXO manager: active_utxo_management failed for {chain:?}: {err:?}");
            }
        }
    }
}
