use std::{
    collections::HashMap,
    sync::{
        OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result};
use omni_connector::{BtcTransferSelection, OmniConnector};
use omni_types::{ChainKind, OmniAddress};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{Mutex, MutexGuard};
use tracing::{info, warn};
use utxo_utils::UTXO;

use crate::{config, utils};

struct ChainSlot {
    utxos: Mutex<HashMap<String, UTXO>>,
    dirty: AtomicBool,
}

impl ChainSlot {
    fn new() -> Self {
        Self {
            utxos: Mutex::new(HashMap::new()),
            dirty: AtomicBool::new(true),
        }
    }
}

/// In-memory cache of UTXOs held by the bridge contract on NEAR, with a
/// separate mutex per chain. Each chain also tracks a `dirty` flag: the
/// LC poller marks it on tip advance, and the next `lock` caller pays
/// for the contract RPC before being handed the guard. Submitters hold
/// the lock only across selection (see `lock`), drain the selected
/// outpoints with `take_outpoints`, drop the lock, and restore via
/// `restore_outpoints` if the downstream submit fails.
pub struct UtxoSet {
    btc: ChainSlot,
    zcash: ChainSlot,
}

impl UtxoSet {
    pub fn global() -> &'static Self {
        static INSTANCE: OnceLock<UtxoSet> = OnceLock::new();
        INSTANCE.get_or_init(|| Self {
            btc: ChainSlot::new(),
            zcash: ChainSlot::new(),
        })
    }

    fn slot(&self, chain: ChainKind) -> Option<&ChainSlot> {
        match chain {
            ChainKind::Btc => Some(&self.btc),
            ChainKind::Zcash => Some(&self.zcash),
            _ => None,
        }
    }

    /// Mark the chain's UTXO set as stale. The next `lock` caller pulls
    /// the contract's state before being given the guard. Cheap & sync;
    /// safe to call from anywhere. No-op for non-UTXO chains.
    pub fn mark_dirty(&self, chain: ChainKind) {
        if let Some(slot) = self.slot(chain) {
            slot.dirty.store(true, Ordering::SeqCst);
        }
    }

    /// Acquire the exclusive guard for `chain`. If the set has been
    /// marked dirty (LC tip advanced, post-submit invalidation, or
    /// never populated), the contract's UTXO set is fetched and
    /// installed under the guard before it is returned. Returns `None`
    /// for non-UTXO chains. A failed refresh re-arms the dirty flag and
    /// hands back the guard with whatever was cached — the next caller
    /// retries.
    ///
    /// Hold the guard across `near_select_btc_utxos` only; drop it
    /// before the (slow) submit call.
    pub async fn lock(
        &self,
        omni_connector: &OmniConnector,
        chain: ChainKind,
    ) -> Option<MutexGuard<'_, HashMap<String, UTXO>>> {
        let slot = self.slot(chain)?;
        let mut guard = slot.utxos.lock().await;
        if slot.dirty.swap(false, Ordering::SeqCst) {
            match Self::fetch(omni_connector, chain).await {
                Ok(fresh) => {
                    let count = fresh.len();
                    *guard = fresh;
                    info!("Refreshed {chain:?} UTXO set ({count} UTXOs)");
                }
                Err(err) => {
                    slot.dirty.store(true, Ordering::SeqCst);
                    warn!("Refresh of {chain:?} UTXO set failed, using cached data: {err:?}");
                }
            }
        }
        Some(guard)
    }

    async fn fetch(
        omni_connector: &OmniConnector,
        chain: ChainKind,
    ) -> Result<HashMap<String, UTXO>> {
        let client = omni_connector
            .near_bridge_client()
            .context("NEAR bridge client unavailable for UTXO refresh")?;
        client
            .get_utxos(chain)
            .await
            .with_context(|| format!("Failed to fetch UTXOs for {chain:?}"))
    }

    /// Remove every outpoint in `selection` from the locked chain cache,
    /// returning the removed entries so the caller can restore them via
    /// `restore_outpoints` if the downstream submit fails. A `None` guard
    /// or entries missing from the cache are silently skipped — the SDK
    /// may have selected outpoints by falling back to a direct contract
    /// query when our cache snapshot was empty.
    pub fn take_outpoints(
        guard: Option<&mut MutexGuard<'_, HashMap<String, UTXO>>>,
        selection: &BtcTransferSelection,
    ) -> Vec<(String, UTXO)> {
        let Some(guard) = guard else {
            return Vec::new();
        };
        selection
            .out_points
            .iter()
            .filter_map(|op| {
                let key = format!("{}@{}", op.txid, op.vout);
                guard.remove(&key).map(|utxo| (key, utxo))
            })
            .collect()
    }

    /// Re-insert entries previously taken with `take_outpoints`. No-op for
    /// non-UTXO chains or when `removed` is empty.
    pub async fn restore_outpoints(&self, chain: ChainKind, removed: Vec<(String, UTXO)>) {
        if removed.is_empty() {
            return;
        }
        let Some(slot) = self.slot(chain) else {
            return;
        };
        let mut guard = slot.utxos.lock().await;
        for (key, utxo) in removed {
            guard.insert(key, utxo);
        }
    }
}

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

/// Embeds the defer target into the stored key, and thus the NATS msg id the
/// poller republishes under.
pub const LC_TARGET_SEPARATOR: &str = "@lc-";

pub fn split_lc_msg_id(msg_id: &str) -> (&str, Option<u64>) {
    if let Some((base, target)) = msg_id.rsplit_once(LC_TARGET_SEPARATOR)
        && let Ok(target) = target.parse()
    {
        (base, Some(target))
    } else {
        (msg_id, None)
    }
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
    base_key: &str,
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
        key: format!("{base_key}{LC_TARGET_SEPARATOR}{target_block}"),
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

const MAX_BATCH_SIZE: usize = 50;

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

#[derive(Debug, Deserialize)]
struct RawTxVin {
    #[serde(default)]
    vin: Option<Vec<Vin>>,
}

#[derive(Debug, Deserialize)]
struct RawTxVout {
    #[serde(default)]
    vout: Vec<Vout>,
}

#[derive(Debug, Deserialize)]
struct Vin {
    #[serde(default)]
    coinbase: Option<String>,
    #[serde(default)]
    txid: Option<String>,
    #[serde(default)]
    vout: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct Vout {
    #[serde(default, rename = "scriptPubKey")]
    script_pub_key: ScriptPubKey,
}

#[derive(Debug, Default, Deserialize)]
struct ScriptPubKey {
    #[serde(default)]
    address: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SingleTxResponse {
    #[serde(default)]
    result: Option<RawTxVin>,
    #[serde(default)]
    error: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct BatchEntry {
    id: u64,
    #[serde(default)]
    result: Option<RawTxVout>,
    #[serde(default)]
    error: Option<Value>,
}

type PrevOutRef = (String, usize);

fn collect_input_refs(
    tx: &RawTxVin,
    chain: ChainKind,
    tx_hash: &str,
) -> Result<(Vec<PrevOutRef>, Vec<String>)> {
    let vin = tx
        .vin
        .as_ref()
        .with_context(|| format!("vin missing in {chain:?} tx {tx_hash}"))?;

    let mut input_refs: Vec<PrevOutRef> = Vec::with_capacity(vin.len());
    let mut unique_txids: Vec<String> = Vec::new();
    for input in vin {
        if input.coinbase.is_some() {
            continue;
        }
        let Some(prev_txid) = input.txid.as_deref() else {
            continue;
        };
        let Some(prev_vout) = input.vout else {
            continue;
        };
        let prev_vout = usize::try_from(prev_vout)
            .with_context(|| format!("vout {prev_vout} out of usize range for {prev_txid}"))?;
        if !unique_txids.iter().any(|t| t == prev_txid) {
            unique_txids.push(prev_txid.to_string());
        }
        input_refs.push((prev_txid.to_string(), prev_vout));
    }
    Ok((input_refs, unique_txids))
}

fn resolve_addresses(
    chain: ChainKind,
    input_refs: &[PrevOutRef],
    prev_txs: &HashMap<String, RawTxVout>,
) -> Vec<OmniAddress> {
    let mut addresses = Vec::with_capacity(input_refs.len());
    for (prev_txid, prev_vout) in input_refs {
        let Some(prev_tx) = prev_txs.get(prev_txid) else {
            continue;
        };
        let Some(output) = prev_tx.vout.get(*prev_vout) else {
            continue;
        };
        let Some(address) = output.script_pub_key.address.as_deref() else {
            continue;
        };
        let omni_address = match chain {
            ChainKind::Btc => OmniAddress::Btc(address.to_string()),
            ChainKind::Zcash => OmniAddress::Zcash(address.to_string()),
            _ => continue,
        };
        addresses.push(omni_address);
    }
    addresses
}

fn index_batch_responses(
    chain: ChainKind,
    tx_hashes: &[String],
    entries: Vec<BatchEntry>,
) -> Result<HashMap<String, RawTxVout>> {
    let mut by_txid = HashMap::with_capacity(tx_hashes.len());
    for entry in entries {
        let idx = usize::try_from(entry.id)
            .with_context(|| format!("batched response id {} out of usize range", entry.id))?;
        let tx_hash = tx_hashes.get(idx).with_context(|| {
            format!(
                "batched response id {idx} out of range (sent {})",
                tx_hashes.len()
            )
        })?;
        let Some(result) = entry.result else {
            if let Some(error) = entry.error.filter(|e| !e.is_null()) {
                anyhow::bail!("getrawtransaction failed for {chain:?}:{tx_hash} in batch: {error}");
            }
            anyhow::bail!("getrawtransaction returned null for {chain:?}:{tx_hash} in batch");
        };
        by_txid.insert(tx_hash.clone(), result);
    }
    Ok(by_txid)
}

async fn get_raw_transaction(rpc_url: &str, chain: ChainKind, tx_hash: &str) -> Result<RawTxVin> {
    let body = json!({
        "id": 1,
        "jsonrpc": "2.0",
        "method": "getrawtransaction",
        "params": [tx_hash, verbosity_for(chain)],
    });

    let response: SingleTxResponse = utxo_rpc_client()
        .post(rpc_url)
        .json(&body)
        .send()
        .await
        .with_context(|| format!("getrawtransaction request failed for {chain:?}:{tx_hash}"))?
        .json()
        .await
        .with_context(|| format!("getrawtransaction body parse failed for {chain:?}:{tx_hash}"))?;

    if let Some(error) = response.error.filter(|e| !e.is_null()) {
        anyhow::bail!("getrawtransaction failed for {chain:?}:{tx_hash}: {error}");
    }
    response
        .result
        .with_context(|| format!("getrawtransaction returned null result for {chain:?}:{tx_hash}"))
}

/// Fetch many raw transactions, decoding only each tx's outputs. The prev txids
/// are split into batches of at most `MAX_BATCH_SIZE` and fetched sequentially
async fn get_raw_transactions_batch(
    rpc_url: &str,
    chain: ChainKind,
    tx_hashes: &[String],
) -> Result<HashMap<String, RawTxVout>> {
    let mut by_txid = HashMap::with_capacity(tx_hashes.len());
    for chunk in tx_hashes.chunks(MAX_BATCH_SIZE) {
        let entries = fetch_batch_entries(rpc_url, chain, chunk).await?;
        by_txid.extend(index_batch_responses(chain, chunk, entries)?);
    }
    Ok(by_txid)
}

async fn fetch_batch_entries(
    rpc_url: &str,
    chain: ChainKind,
    chunk: &[String],
) -> Result<Vec<BatchEntry>> {
    let verbosity = verbosity_for(chain);
    let body: Vec<Value> = chunk
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

    utxo_rpc_client()
        .post(rpc_url)
        .json(&body)
        .send()
        .await
        .with_context(|| {
            format!(
                "batched getrawtransaction request failed for {chain:?} ({} tx)",
                chunk.len()
            )
        })?
        .json()
        .await
        .with_context(|| {
            format!(
                "batched getrawtransaction body parse failed for {chain:?} ({} tx) — server may not support JSON-RPC 2.0 batching",
                chunk.len()
            )
        })
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
    let (input_refs, unique_txids) = collect_input_refs(&tx, chain, tx_hash)?;

    // Single batched JSON-RPC POST for all prev txs.
    let prev_txs = get_raw_transactions_batch(rpc_url, chain, &unique_txids).await?;

    Ok(resolve_addresses(chain, &input_refs, &prev_txs))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::kyt;

    const DEFAULT_TAINTED_TX: &str =
        "0b13776c8a64eaf701c240885afc0c8560258c08201df207863033380b03b0c6";

    // ---------------------------------------------------------------------
    // Pure parse/extraction unit tests — no network. These pin the behavior
    // the typed-struct rewrite must preserve.
    // ---------------------------------------------------------------------

    #[test]
    fn collect_input_refs_skips_coinbase_and_dedups_prev_txids() {
        // Includes a coinbase input, a dup prevout, an input missing `vout`,
        // and unknown fields — all of which the extraction must handle.
        let main_json = r#"{
            "txid": "main",
            "hex": "deadbeef",
            "size": 321,
            "vin": [
                { "coinbase": "0340", "sequence": 4294967295 },
                { "txid": "prevA", "vout": 0, "scriptSig": { "hex": "48" } },
                { "txid": "prevB", "vout": 1 },
                { "txid": "prevA", "vout": 0 },
                { "txid": "prevC" }
            ],
            "vout": []
        }"#;
        let tx: RawTxVin = serde_json::from_str(main_json).expect("parse main tx");
        let (input_refs, unique_txids) =
            collect_input_refs(&tx, ChainKind::Btc, "main").expect("collect");

        assert_eq!(
            input_refs,
            vec![
                ("prevA".to_string(), 0),
                ("prevB".to_string(), 1),
                ("prevA".to_string(), 0),
            ]
        );
        assert_eq!(unique_txids, vec!["prevA".to_string(), "prevB".to_string()]);
    }

    #[test]
    fn collect_input_refs_errors_when_vin_missing() {
        let tx: RawTxVin = serde_json::from_str(r#"{ "txid": "x" }"#).expect("parse");
        assert!(collect_input_refs(&tx, ChainKind::Btc, "x").is_err());
    }

    #[test]
    fn resolve_addresses_extracts_transparent_and_skips_unresolvable() {
        let prev_a: RawTxVout = serde_json::from_str(
            r#"{ "vout": [
                { "value": 1.0, "n": 0,
                  "scriptPubKey": { "hex": "0014aa", "address": "bc1qA", "type": "witness_v0_keyhash" } }
            ] }"#,
        )
        .unwrap();
        let prev_b: RawTxVout = serde_json::from_str(
            r#"{ "vout": [
                { "value": 0.0, "n": 0, "scriptPubKey": { "hex": "6a0102", "type": "nulldata" } },
                { "value": 2.0, "n": 1, "scriptPubKey": { "address": "bc1qB", "type": "witness_v0_keyhash" } }
            ] }"#,
        )
        .unwrap();
        let mut prev_txs = HashMap::new();
        prev_txs.insert("prevA".to_string(), prev_a);
        prev_txs.insert("prevB".to_string(), prev_b);

        let input_refs = vec![
            ("prevA".to_string(), 0usize), // -> bc1qA
            ("prevB".to_string(), 0usize), // OP_RETURN, no address -> skip
            ("prevB".to_string(), 1usize), // -> bc1qB
            ("prevA".to_string(), 5usize), // vout out of range -> skip
            ("prevD".to_string(), 0usize), // parent tx not fetched -> skip
        ];
        let addresses = resolve_addresses(ChainKind::Btc, &input_refs, &prev_txs);

        assert_eq!(
            addresses,
            vec![
                OmniAddress::Btc("bc1qA".to_string()),
                OmniAddress::Btc("bc1qB".to_string()),
            ]
        );
    }

    #[test]
    fn resolve_addresses_uses_zcash_variant_for_zcash() {
        let prev: RawTxVout =
            serde_json::from_str(r#"{ "vout": [ { "scriptPubKey": { "address": "t1abc" } } ] }"#)
                .unwrap();
        let mut prev_txs = HashMap::new();
        prev_txs.insert("p".to_string(), prev);
        let addresses = resolve_addresses(ChainKind::Zcash, &[("p".to_string(), 0)], &prev_txs);
        assert_eq!(addresses, vec![OmniAddress::Zcash("t1abc".to_string())]);
    }

    #[test]
    fn index_batch_responses_maps_results_by_original_txid() {
        let tx_hashes = vec!["txA".to_string(), "txB".to_string()];
        // ids echo the request index; returned out of order to prove the
        // mapping is by `id`, not response position.
        let entries: Vec<BatchEntry> = serde_json::from_str(
            r#"[
                { "id": 1, "jsonrpc": "2.0", "result": { "vout": [ { "scriptPubKey": { "address": "addrB" } } ] } },
                { "id": 0, "jsonrpc": "2.0", "result": { "vout": [ { "scriptPubKey": { "address": "addrA" } } ] } }
            ]"#,
        )
        .unwrap();
        let map = index_batch_responses(ChainKind::Btc, &tx_hashes, entries).expect("index");

        assert_eq!(map.len(), 2);
        assert_eq!(
            map["txA"].vout[0].script_pub_key.address.as_deref(),
            Some("addrA")
        );
        assert_eq!(
            map["txB"].vout[0].script_pub_key.address.as_deref(),
            Some("addrB")
        );
    }

    #[test]
    fn index_batch_responses_errors_on_rpc_error_entry() {
        let tx_hashes = vec!["txA".to_string()];
        let entries: Vec<BatchEntry> = serde_json::from_str(
            r#"[ { "id": 0, "result": null, "error": { "code": -5, "message": "No such transaction" } } ]"#,
        )
        .unwrap();
        assert!(index_batch_responses(ChainKind::Btc, &tx_hashes, entries).is_err());
    }

    #[test]
    fn index_batch_responses_errors_on_null_result() {
        let tx_hashes = vec!["txA".to_string()];
        let entries: Vec<BatchEntry> =
            serde_json::from_str(r#"[ { "id": 0, "result": null } ]"#).unwrap();
        assert!(index_batch_responses(ChainKind::Btc, &tx_hashes, entries).is_err());
    }

    #[test]
    fn split_txids_for_batch_caps_at_max_batch_size() {
        assert_eq!(MAX_BATCH_SIZE, 50);
        let ids: Vec<String> = (0..120).map(|i| i.to_string()).collect();
        let chunks: Vec<&[String]> = ids.chunks(MAX_BATCH_SIZE).collect();

        // 120 -> 50 + 50 + 20, every chunk within the cap, none empty.
        assert_eq!(chunks.len(), 3);
        assert!(
            chunks
                .iter()
                .all(|c| !c.is_empty() && c.len() <= MAX_BATCH_SIZE)
        );
        assert_eq!(chunks.iter().map(|c| c.len()).sum::<usize>(), ids.len());
        assert_eq!(chunks[0].len(), MAX_BATCH_SIZE);
        assert_eq!(chunks.last().unwrap().len(), 20);
        // Exactly one batch when under the cap; none when empty.
        assert_eq!(ids[..10].chunks(MAX_BATCH_SIZE).count(), 1);
        assert_eq!(ids[..0].chunks(MAX_BATCH_SIZE).count(), 0);
    }

    #[tokio::test]
    async fn get_raw_transactions_batch_empty_input_makes_no_request() {
        // Empty input must yield an empty map without issuing any HTTP request,
        // even to an unroutable URL.
        let map = get_raw_transactions_batch("http://255.255.255.255:1", ChainKind::Btc, &[])
            .await
            .expect("empty batch should be Ok");
        assert!(map.is_empty());
    }

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
    ///
    /// ```text
    /// CONFIG_PATH=/path/to/mainnet-config.toml \
    ///   cargo test btc_input_addresses_lists_transparent_inputs \
    ///       -- --ignored --nocapture
    /// ```
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
    ///
    /// ```text
    /// CONFIG_PATH=/path/to/mainnet-config.toml \
    ///   KYT_API_URL=https://... \
    ///   KYT_API_KEY=... \
    ///   cargo test btc_input_kyt_flags_known_tainted_tx \
    ///       -- --ignored --nocapture
    /// ```
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
