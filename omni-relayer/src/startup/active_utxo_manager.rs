use std::{sync::Arc, time::Duration};

use anyhow::Result;
use near_bridge_client::TransactionOptions;
use omni_connector::OmniConnector;
use omni_types::ChainKind;
use tracing::{info, warn};

use crate::{config::ActiveUtxoManagement, utils};

pub async fn start_active_utxo_manager(
    settings: ActiveUtxoManagement,
    chain: ChainKind,
    omni_connector: Arc<OmniConnector>,
    near_nonce: Arc<utils::nonce::NonceManager>,
) -> Result<()> {
    let interval = Duration::from_secs(settings.polling_interval_secs);

    info!(
        "Starting active UTXO manager for {chain:?} (threshold={}, interval={}s)",
        settings.utxo_count_threshold, settings.polling_interval_secs,
    );

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

        if utxos_num <= settings.utxo_count_threshold {
            info!(
                "Active UTXO manager: {chain:?} has {utxos_num} UTXOs (threshold {}); skipping",
                settings.utxo_count_threshold
            );
            continue;
        }

        info!(
            "Active UTXO manager: {chain:?} has {utxos_num} UTXOs (above threshold {}); triggering active_utxo_management",
            settings.utxo_count_threshold
        );

        let nonce = match near_nonce.reserve_nonce() {
            Ok(nonce) => Some(nonce),
            Err(err) => {
                warn!("Active UTXO manager: failed to reserve nonce for {chain:?}: {err:?}");
                continue;
            }
        };

        match omni_connector
            .active_utxo_management(
                chain,
                settings.fixed_fee_rate,
                settings.max_input_number,
                false,
                None,
                None,
                TransactionOptions {
                    nonce,
                    wait_until: near_primitives::views::TxExecutionStatus::Final,
                    wait_final_outcome_timeout_sec: None,
                },
            )
            .await
        {
            Ok(tx_hash) => {
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
