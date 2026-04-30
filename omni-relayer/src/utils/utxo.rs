use omni_connector::OmniConnector;
use omni_types::ChainKind;
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::{config, utils};

pub async fn compute_lc_target_block(
    config: &config::Config,
    omni_connector: &OmniConnector,
    chain: ChainKind,
    tx_hash: &str,
) -> Option<u64> {
    let utxo_config = match chain {
        ChainKind::Btc => config.btc.as_ref(),
        ChainKind::Zcash => config.zcash.as_ref(),
        _ => None,
    }?;

    let block_height = match chain {
        ChainKind::Btc => {
            fetch_utxo_block_height(omni_connector.btc_bridge_client().ok()?, chain, tx_hash).await
        }
        ChainKind::Zcash => {
            fetch_utxo_block_height(omni_connector.zcash_bridge_client().ok()?, chain, tx_hash)
                .await
        }
        _ => None,
    }?;

    Some(block_height + utxo_config.confirmations)
}

async fn fetch_utxo_block_height<C: utxo_bridge_client::types::UTXOChain>(
    client: &utxo_bridge_client::UTXOBridgeClient<C>,
    chain: ChainKind,
    tx_hash: &str,
) -> Option<u64> {
    let block_hash = match client.get_block_hash_by_tx_hash(tx_hash).await {
        Ok(hash) => hash,
        Err(err) => {
            warn!("Failed to get {chain:?} block hash for {tx_hash}: {err:?}");
            return None;
        }
    };
    match client
        .get_block_height_by_block_hash(&block_hash.to_string())
        .await
    {
        Ok(height) => Some(height),
        Err(err) => {
            warn!("Failed to get {chain:?} block height for {block_hash}: {err:?}");
            None
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingLcEvent {
    pub key: String,
    pub event: serde_json::Value,
}

pub fn pending_lc_key(chain: ChainKind) -> Option<String> {
    if chain.is_utxo_chain() {
        Some(format!(
            "pending_lc:{}",
            chain.as_ref().to_ascii_lowercase()
        ))
    } else {
        None
    }
}

pub async fn store_pending_lc_event<E>(
    config: &config::Config,
    redis_connection_manager: &mut redis::aio::ConnectionManager,
    chain: ChainKind,
    target_block: u64,
    original_key: String,
    event: &E,
) -> bool
where
    E: serde::Serialize + std::fmt::Debug + Send,
{
    let Some(redis_key) = pending_lc_key(chain) else {
        return false;
    };
    let Ok(value) = serde_json::to_value(event) else {
        return false;
    };
    let pending = PendingLcEvent {
        key: original_key,
        event: value,
    };
    utils::redis::zadd(
        config,
        redis_connection_manager,
        &redis_key,
        target_block,
        pending,
    )
    .await
}
