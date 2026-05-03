use anyhow::{Context, Result};
use omni_connector::OmniConnector;
use omni_types::ChainKind;
use serde::{Deserialize, Serialize};

use crate::{config, utils};

pub async fn lc_defer_target(
    omni_connector: &OmniConnector,
    chain: ChainKind,
    tx_hash: &str,
    amount: u128,
    uses_extra_msg_path: bool,
) -> Result<Option<u64>> {
    let target =
        compute_lc_target_block(omni_connector, chain, tx_hash, amount, uses_extra_msg_path)
            .await?;
    let lc = omni_connector
        .light_client(chain)
        .with_context(|| format!("Failed to get {chain:?} light client"))?;
    let tip = lc
        .get_last_block_number()
        .await
        .with_context(|| format!("Failed to query {chain:?} light client tip"))?;
    Ok(if tip >= target { None } else { Some(target) })
}

async fn compute_lc_target_block(
    omni_connector: &OmniConnector,
    chain: ChainKind,
    tx_hash: &str,
    amount: u128,
    uses_extra_msg_path: bool,
) -> Result<u64> {
    let near_bridge_client = omni_connector
        .near_bridge_client()
        .context("Failed to get NEAR bridge client")?;
    let required_confirmations = near_bridge_client
        .get_btc_confirmation_context(chain)
        .await
        .with_context(|| format!("Failed to get {chain:?} BTC confirmation context"))?
        .required_confirmations(amount, uses_extra_msg_path)
        .with_context(|| format!("Failed to compute required confirmations for {chain:?}"))?;

    let block_height = match chain {
        ChainKind::Btc => {
            fetch_utxo_block_height(omni_connector.btc_bridge_client()?, chain, tx_hash).await?
        }
        ChainKind::Zcash => {
            fetch_utxo_block_height(omni_connector.zcash_bridge_client()?, chain, tx_hash).await?
        }
        _ => anyhow::bail!("Unsupported chain {chain:?} for UTXO LC target"),
    };

    Ok(block_height + required_confirmations)
}

pub async fn fetch_deposit_amount(
    omni_connector: &OmniConnector,
    chain: ChainKind,
    tx_hash: &str,
    vout: u32,
) -> Result<u128> {
    let proof = omni_connector
        .utxo_bridge_client(chain)?
        .extract_btc_proof(tx_hash)
        .await
        .with_context(|| format!("Failed to extract {chain:?} BTC proof for {tx_hash}"))?;
    let btc_tx = utxo_utils::try_bytes_to_btc_transaction(&proof.tx_bytes)
        .map_err(|err| anyhow::anyhow!("Failed to parse {chain:?} tx bytes: {err}"))?;
    let vout_idx =
        usize::try_from(vout).with_context(|| format!("vout {vout} out of usize range"))?;
    let output = btc_tx
        .output
        .get(vout_idx)
        .with_context(|| format!("vout {vout} out of range for tx {tx_hash}"))?;
    Ok(u128::from(output.value.to_sat()))
}

async fn fetch_utxo_block_height<C: utxo_bridge_client::types::UTXOChain>(
    client: &utxo_bridge_client::UTXOBridgeClient<C>,
    chain: ChainKind,
    tx_hash: &str,
) -> Result<u64> {
    let block_hash = client
        .get_block_hash_by_tx_hash(tx_hash)
        .await
        .with_context(|| format!("Failed to get {chain:?} block hash for {tx_hash}"))?;
    client
        .get_block_height_by_block_hash(&block_hash.to_string())
        .await
        .with_context(|| format!("Failed to get {chain:?} block height for {block_hash}"))
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
) -> Result<()>
where
    E: serde::Serialize + std::fmt::Debug + Send,
{
    let redis_key = pending_lc_key(chain)
        .with_context(|| format!("No pending LC redis key for chain {chain:?}"))?;
    let value =
        serde_json::to_value(event).context("Failed to serialize pending LC event payload")?;
    let pending = PendingLcEvent {
        key: original_key,
        event: value,
    };
    if !utils::redis::zadd(
        config,
        redis_connection_manager,
        &redis_key,
        target_block,
        pending,
    )
    .await
    {
        anyhow::bail!("Failed to add pending LC event to redis sorted set ({redis_key})");
    }
    Ok(())
}
