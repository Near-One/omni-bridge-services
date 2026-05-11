use std::{collections::HashMap, sync::OnceLock, time::Duration};

use anyhow::{Context, Result};
use omni_connector::OmniConnector;
use omni_types::{ChainKind, OmniAddress};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{config, utils};

pub async fn lc_defer_target(
    omni_connector: &OmniConnector,
    chain: ChainKind,
    tx_hash: &str,
    amount: u128,
    uses_extra_msg_path: bool,
) -> Result<Option<u64>> {
    let lc = omni_connector
        .light_client(chain)
        .with_context(|| format!("Failed to get {chain:?} light client"))?;
    let tip = lc
        .get_last_block_number()
        .await
        .with_context(|| format!("Failed to query {chain:?} light client tip"))?;
    let target =
        compute_lc_target_block(omni_connector, chain, tx_hash, amount, uses_extra_msg_path)
            .await?;
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

    Ok(block_height + required_confirmations - 1)
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

const UTXO_RPC_TIMEOUT: Duration = Duration::from_secs(10);

fn utxo_rpc_client() -> &'static Client {
    static CLIENT: OnceLock<Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        Client::builder()
            .timeout(UTXO_RPC_TIMEOUT)
            .build()
            .expect("Failed to build UTXO RPC reqwest client")
    })
}

fn verbosity_for(chain: ChainKind) -> Value {
    if matches!(chain, ChainKind::Zcash) {
        json!(1)
    } else {
        json!(true)
    }
}

async fn get_raw_transaction(rpc_url: &str, chain: ChainKind, tx_hash: &str) -> Result<Value> {
    let body = json!({
        "id": 1,
        "jsonrpc": "2.0",
        "method": "getrawtransaction",
        "params": [tx_hash, verbosity_for(chain)],
    });
    let response: Value = utxo_rpc_client()
        .post(rpc_url)
        .json(&body)
        .send()
        .await
        .with_context(|| format!("getrawtransaction request failed for {chain:?}:{tx_hash}"))?
        .json()
        .await
        .with_context(|| format!("getrawtransaction body parse failed for {chain:?}:{tx_hash}"))?;
    let result = response.get("result").cloned().with_context(|| {
        format!("getrawtransaction response missing `result` for {chain:?}:{tx_hash}: {response}")
    })?;
    if result.is_null() {
        anyhow::bail!("getrawtransaction returned null for {chain:?}:{tx_hash}: {response}");
    }
    Ok(result)
}

/// Fetch many raw transactions in a single JSON-RPC 2.0 batched POST. Returns a
/// `txid -> result` map. Each request is keyed by its array index as the
/// JSON-RPC `id`, so order doesn't matter on the response side.
async fn get_raw_transactions_batch(
    rpc_url: &str,
    chain: ChainKind,
    tx_hashes: &[String],
) -> Result<HashMap<String, Value>> {
    if tx_hashes.is_empty() {
        return Ok(HashMap::new());
    }
    let verbosity = verbosity_for(chain);
    let body: Vec<Value> = tx_hashes
        .iter()
        .enumerate()
        .map(|(idx, tx)| {
            json!({
                "id": idx,
                "jsonrpc": "2.0",
                "method": "getrawtransaction",
                "params": [tx, verbosity],
            })
        })
        .collect();

    let responses: Vec<Value> = utxo_rpc_client()
        .post(rpc_url)
        .json(&body)
        .send()
        .await
        .with_context(|| {
            format!(
                "batched getrawtransaction request failed for {chain:?} ({} tx)",
                tx_hashes.len()
            )
        })?
        .json()
        .await
        .with_context(|| {
            format!(
                "batched getrawtransaction body parse failed for {chain:?} ({} tx) — server may not support JSON-RPC 2.0 batching",
                tx_hashes.len()
            )
        })?;

    let mut by_txid = HashMap::with_capacity(tx_hashes.len());
    for response in responses {
        let id = response
            .get("id")
            .and_then(Value::as_u64)
            .with_context(|| format!("batched response missing `id`: {response}"))?;
        let idx = usize::try_from(id)
            .with_context(|| format!("batched response id {id} out of usize range"))?;
        let tx_hash = tx_hashes.get(idx).with_context(|| {
            format!(
                "batched response id {idx} out of range (sent {})",
                tx_hashes.len()
            )
        })?;
        let result = response.get("result").cloned().with_context(|| {
            format!("batched response for {chain:?}:{tx_hash} missing `result`: {response}")
        })?;
        if result.is_null() {
            if let Some(error) = response.get("error").filter(|e| !e.is_null()) {
                anyhow::bail!("getrawtransaction failed for {chain:?}:{tx_hash} in batch: {error}");
            }
            anyhow::bail!("getrawtransaction returned null for {chain:?}:{tx_hash} in batch");
        }
        by_txid.insert(tx_hash.clone(), result);
    }
    Ok(by_txid)
}

/// Fetch the spending address for each input of `tx_hash`. Skips inputs whose
/// previous output has no extractable address (coinbase, raw multisig, `OP_RETURN`,
/// shielded Zcash spends, etc.) — only transparent inputs with a derived address
/// are returned.
pub async fn fetch_input_addresses(
    rpc_url: &str,
    chain: ChainKind,
    tx_hash: &str,
) -> Result<Vec<OmniAddress>> {
    if !matches!(chain, ChainKind::Btc | ChainKind::Zcash) {
        anyhow::bail!("fetch_input_addresses unsupported for {chain:?}");
    }

    let tx = get_raw_transaction(rpc_url, chain, tx_hash).await?;
    let vin = tx
        .get("vin")
        .and_then(Value::as_array)
        .with_context(|| format!("vin missing in {chain:?} tx {tx_hash}"))?;

    // First pass: collect (prev_txid, prev_vout) for every transparent input,
    // and the set of unique prev txids to batch-fetch.
    let mut input_refs: Vec<(String, usize)> = Vec::with_capacity(vin.len());
    let mut unique_txids: Vec<String> = Vec::new();
    for input in vin {
        if input.get("coinbase").is_some() {
            continue;
        }
        let Some(prev_txid) = input.get("txid").and_then(Value::as_str) else {
            continue;
        };
        let Some(prev_vout) = input.get("vout").and_then(Value::as_u64) else {
            continue;
        };
        let prev_vout = usize::try_from(prev_vout)
            .with_context(|| format!("vout {prev_vout} out of usize range for {prev_txid}"))?;
        if !unique_txids.iter().any(|t| t == prev_txid) {
            unique_txids.push(prev_txid.to_string());
        }
        input_refs.push((prev_txid.to_string(), prev_vout));
    }

    // Single batched JSON-RPC POST for all prev txs.
    let prev_txs = get_raw_transactions_batch(rpc_url, chain, &unique_txids).await?;

    // Second pass: resolve each input to its spend address.
    let mut addresses = Vec::with_capacity(input_refs.len());
    for (prev_txid, prev_vout) in input_refs {
        let Some(prev_tx) = prev_txs.get(&prev_txid) else {
            continue;
        };
        let Some(prev_outputs) = prev_tx.get("vout").and_then(Value::as_array) else {
            continue;
        };
        let Some(output) = prev_outputs.get(prev_vout) else {
            continue;
        };
        let Some(address) = output
            .pointer("/scriptPubKey/address")
            .and_then(Value::as_str)
        else {
            continue;
        };

        let omni_address = match chain {
            ChainKind::Btc => OmniAddress::Btc(address.to_string()),
            ChainKind::Zcash => OmniAddress::Zcash(address.to_string()),
            _ => unreachable!("guarded above"),
        };
        addresses.push(omni_address);
    }

    Ok(addresses)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::kyt;

    const DEFAULT_TAINTED_TX: &str =
        "0b13776c8a64eaf701c240885afc0c8560258c08201df207863033380b03b0c6";

    /// Pulls just `btc.rpc_http_url` out of a TOML config without going through
    /// `Config` deserialization (which would trigger env-var substitution for
    /// unrelated fields like `MONGODB_*` / `INFURA_API_KEY`). Any
    /// `${VAR}`-style placeholder inside the URL itself must be expanded by the
    /// shell before invoking the test (or substituted in a copy of the file).
    fn btc_rpc_url() -> String {
        if let Ok(url) = std::env::var("BTC_RPC_URL") {
            return url;
        }
        let path = std::env::var("CONFIG_PATH")
            .expect("Set BTC_RPC_URL or CONFIG_PATH=/path/to/mainnet-config.toml");
        let raw = std::fs::read_to_string(&path)
            .unwrap_or_else(|err| panic!("Failed to read {path}: {err}"));
        let value: toml::Value =
            toml::from_str(&raw).expect("Failed to parse config as generic TOML");
        value
            .get("btc")
            .and_then(|btc| btc.get("rpc_http_url"))
            .and_then(toml::Value::as_str)
            .map(str::to_string)
            .expect("`btc.rpc_http_url` missing from config")
    }

    fn tx_under_test() -> String {
        std::env::var("BTC_TX_HASH").unwrap_or_else(|_| DEFAULT_TAINTED_TX.to_string())
    }

    /// Smoke test: prints the transparent input addresses for the configured BTC
    /// tx. Useful before running the full KYT path to confirm the BTC RPC route.
    /// Run with:
    ///   CONFIG_PATH=/path/to/mainnet-config.toml \
    ///   cargo test btc_input_addresses_lists_transparent_inputs \
    ///       -- --ignored --nocapture
    #[tokio::test]
    #[ignore = "needs live BTC RPC"]
    async fn btc_input_addresses_lists_transparent_inputs() {
        let rpc_url = btc_rpc_url();
        let tx = tx_under_test();
        let addresses = fetch_input_addresses(&rpc_url, ChainKind::Btc, &tx)
            .await
            .expect("fetch_input_addresses failed");
        eprintln!(
            "Got {} transparent input address(es) for {tx}:",
            addresses.len()
        );
        for addr in &addresses {
            eprintln!("  {addr}");
        }
        assert!(
            !addresses.is_empty(),
            "no transparent input addresses extracted for {tx}"
        );
    }

    /// Full pipeline: fetches BTC inputs and asserts the KYT provider returns
    /// `STOP_RELAYING` for the configured (known-tainted) tx. Run with:
    ///   CONFIG_PATH=/path/to/mainnet-config.toml \
    ///   KYT_API_URL=https://... \
    ///   KYT_API_KEY=... \
    ///   cargo test btc_input_kyt_flags_known_tainted_tx \
    ///       -- --ignored --nocapture
    #[tokio::test]
    #[ignore = "needs live BTC RPC + KYT credentials"]
    async fn btc_input_kyt_flags_known_tainted_tx() {
        let rpc_url = btc_rpc_url();
        let tx = tx_under_test();
        let addresses = fetch_input_addresses(&rpc_url, ChainKind::Btc, &tx)
            .await
            .expect("fetch_input_addresses failed");
        eprintln!("Screening {} input(s) for {tx}:", addresses.len());
        for addr in &addresses {
            eprintln!("  {addr}");
        }
        let action = kyt::check_senders(&addresses)
            .await
            .expect("check_senders failed (KYT_API_URL / KYT_API_KEY set?)");
        eprintln!("KYT verdict: {action:?}");
        assert_eq!(
            action,
            kyt::SuggestedAction::StopRelaying,
            "expected STOP_RELAYING for {tx}"
        );
    }
}
