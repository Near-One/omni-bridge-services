use omni_types::ChainKind;
use serde::{Deserialize, Serialize};

use crate::{config, utils};

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
