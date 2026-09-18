use std::{str::FromStr, sync::Arc};

use anyhow::{Context, Result};
use bridge_connector_common::result::BridgeSdkError;
use near_bridge_client::{
    TransactionOptions,
    btc::{DepositMsg, PostAction, SafeDepositMsg},
};
use near_jsonrpc_client::JsonRpcClient;
use near_primitives::{hash::CryptoHash, types::AccountId};
use omni_types::{ChainKind, OmniAddress, UtxoId};
use tracing::{info, warn};

use omni_connector::{BtcDepositArgs, BtcTxType, FinTransferArgs, OmniConnector};

use crate::{
    config,
    metrics::{Metrics, stall_reason},
    utils,
};

use super::{EventAction, Transfer};

#[derive(Debug, serde::Serialize, serde::Deserialize, Clone)]
pub struct SignUtxoTransaction {
    pub chain: ChainKind,
    pub near_tx_hash: String,
    pub relayer: AccountId,
    #[serde(default)]
    pub btc_pending_id: Option<String>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize, Clone)]
pub struct ConfirmedTxHash {
    pub chain: ChainKind,
    pub btc_tx_hash: String,
}

/// Contract failures that a later attempt on the same input can clear.
const SIGN_RETRYABLE_ERRORS: [&str; 4] = [
    "Request has timed out.",
    // Matches `UTXO <key> not exist`, which a later attempt can clear.
    // `BTC pending info not exist` is excluded by `TERMINAL_FAILURE_OVERRIDES`
    // in `utils::near` and drops instead.
    "not exist",
    "Previous btc tx has not been signed",
    "Too many pending sign transactions",
];

/// Signs the inputs of one pending NEAR->UTXO transaction.
///
/// A batched work item (`sign_count = Some(n)`) walks inputs
/// `sign_index..sign_index + n` in order inside this one task, so a transfer
/// that consumes twenty inputs holds one worker permit instead of twenty, and
/// its inputs no longer race each other into the contract's sign ordering
/// check. A legacy item (`sign_count = None`) signs exactly one input, so items
/// already on the stream keep working.
///
/// Progress is recorded per input in Redis. A redelivery resumes at the first
/// unsigned input rather than re-signing the whole transaction.
#[allow(clippy::too_many_lines)]
pub async fn process_near_to_utxo_init_transfer_event(
    config: &config::Config,
    redis: &mut redis::aio::ConnectionManager,
    jsonrpc_client: &JsonRpcClient,
    omni_connector: Arc<OmniConnector>,
    signer: AccountId,
    transfer: Transfer,
    near_nonce: Arc<utils::nonce::NonceManager>,
) -> Result<EventAction> {
    let Transfer::NearToUtxo {
        chain,
        btc_pending_id,
        sign_index,
        sign_count,
        sender,
        creation_timestamp,
    } = transfer
    else {
        warn!("Routing mismatch, dropping: {transfer:?}");
        return Ok(EventAction::Drop);
    };

    let sign_indices = sign_index_range(sign_index, sign_count);
    let context = format!(
        "({btc_pending_id}:{}..={})",
        sign_indices.start,
        sign_indices.end - 1
    );

    if !config.is_signing_utxo_transaction_enabled(chain) {
        info!("Signing NEAR->{chain:?} disabled by config {context}, skipping");
        return Ok(EventAction::Drop);
    }

    let current_timestamp = chrono::Utc::now().timestamp();
    if current_timestamp < creation_timestamp + config.kyt.delay_secs {
        let remaining =
            (creation_timestamp + config.kyt.delay_secs - current_timestamp).unsigned_abs();
        return Ok(EventAction::RetryAfter(std::time::Duration::from_secs(
            remaining,
        )));
    }

    let sender = OmniAddress::Near(sender);
    if let Some(action) = utils::validation::validate_sender(config, &sender, chain, &context).await
    {
        return Ok(action);
    }

    let sign_delay_secs = i64::try_from(config.utxo_sign_delay_secs(chain)).unwrap_or(0);
    if sign_delay_secs > 0 && current_timestamp < creation_timestamp + sign_delay_secs {
        let remaining = (creation_timestamp + sign_delay_secs - current_timestamp).unsigned_abs();
        return Ok(EventAction::RetryAfter(std::time::Duration::from_secs(
            remaining,
        )));
    }

    let signed_key = utils::redis::near_to_utxo_signed_key(&btc_pending_id);

    for index in sign_indices {
        // Re-read on every input: another relayer can broadcast the pending
        // transaction while this batch is still walking its inputs, and every
        // further sign call would then be wasted contract traffic.
        match utils::redis::exists(config, redis, &signed_key).await {
            Some(true) => {
                info!(
                    "Skipping sign for {btc_pending_id}:{index} - already handled by another relayer"
                );
                return Ok(EventAction::Drop);
            }
            Some(false) => {}
            None => {
                warn!(
                    "Redis exists failed for {btc_pending_id}:{index}; proceeding to sign and letting the contract dedupe"
                );
            }
        }

        let index_key = utils::redis::near_to_utxo_sign_index_key(&btc_pending_id, index);
        if utils::redis::exists(config, redis, &index_key).await == Some(true) {
            info!("Input {btc_pending_id}:{index} already signed, skipping");
            continue;
        }

        let nonce = match near_nonce.reserve_nonce() {
            Ok(nonce) => Some(nonce),
            Err(err) => {
                warn!(
                    "Failed to reserve nonce for NEAR->{chain:?} sign ({btc_pending_id}:{index}): {err:?}"
                );
                return Ok(EventAction::Retry);
            }
        };

        let action = match omni_connector
            .near_sign_btc_transaction(
                chain,
                btc_pending_id.clone(),
                index,
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
                    "Signed NEAR->{chain:?} input ({btc_pending_id}:{index}): near_sign_tx_hash={tx_hash}"
                );
                utils::near::resolve_tx_action(
                    jsonrpc_client,
                    tx_hash,
                    signer.clone(),
                    &SIGN_RETRYABLE_ERRORS,
                )
                .await
            }
            Err(err) => {
                return Err(err).with_context(|| {
                    format!("Failed to sign NEAR->{chain:?} input ({btc_pending_id}:{index})")
                });
            }
        };

        match action {
            EventAction::Remove => {
                utils::redis::set_with_ttl(
                    config,
                    redis,
                    &index_key,
                    &index.to_string(),
                    utils::redis::NEAR_TO_UTXO_SIGNED_TTL_SECS,
                )
                .await;
            }
            // Retrying or giving up applies to the whole item. The inputs
            // already marked above are skipped on the next delivery, so the
            // work is not repeated.
            other => {
                info!("Stopping NEAR->{chain:?} sign batch {context} at input {index}: {other:?}");
                return Ok(other);
            }
        }
    }

    Ok(EventAction::Remove)
}

/// The inputs one work item covers. A legacy item carries no count and signs
/// exactly one input; a zero count is treated the same way, so a malformed item
/// still makes progress instead of silently doing nothing.
fn sign_index_range(sign_index: u64, sign_count: Option<u64>) -> std::ops::Range<u64> {
    let count = sign_count.unwrap_or(1).max(1);
    sign_index..sign_index.saturating_add(count)
}

pub async fn process_utxo_to_near_init_transfer_event(
    config: &config::Config,
    redis: &mut redis::aio::ConnectionManager,
    jsonrpc_client: &JsonRpcClient,
    omni_connector: Arc<OmniConnector>,
    transfer: Transfer,
    near_nonce: Arc<utils::nonce::NonceManager>,
) -> Result<EventAction> {
    let Ok(near_bridge_client) = omni_connector.near_bridge_client() else {
        anyhow::bail!("Near bridge client is not configured");
    };

    let Transfer::UtxoToNear {
        chain,
        btc_tx_hash,
        vout,
        deposit_msg,
        amount,
        ..
    } = transfer.clone()
    else {
        warn!("Routing mismatch, dropping: {transfer:?}");
        return Ok(EventAction::Drop);
    };

    if config::Config::is_kyt_enabled() {
        let rpc_url = match chain {
            ChainKind::Btc => config.btc.as_ref().map(|cfg| cfg.rpc_http_url.as_str()),
            ChainKind::Zcash => config.zcash.as_ref().map(|cfg| cfg.rpc_http_url.as_str()),
            _ => {
                warn!("Unsupported chain for UTXO, dropping: {chain:?}");
                return Ok(EventAction::Drop);
            }
        }
        .with_context(|| format!("{chain:?} UTXO config missing for input KYT"))?;

        let input_addresses = match utils::utxo::fetch_input_addresses(rpc_url, chain, &btc_tx_hash)
            .await
        {
            Ok(addresses) => addresses,
            Err(err) => {
                warn!(
                    "Failed to fetch input addresses for {chain:?} tx {btc_tx_hash}, retrying: {err:?}"
                );
                return Ok(EventAction::Retry);
            }
        };

        let context = format!("({chain:?}:{btc_tx_hash}:{vout})");
        if let Some(action) = utils::validation::check_kyt_senders(&input_addresses, &context).await
        {
            return Ok(action);
        }
    }

    let mut nonce = match near_nonce.reserve_nonce() {
        Ok(nonce) => Some(nonce),
        Err(err) => {
            warn!(
                "Failed to reserve nonce for {chain:?}->NEAR fin_transfer ({btc_tx_hash}:{vout}): {err:?}"
            );
            return Ok(EventAction::Retry);
        }
    };

    match omni_connector
        .near_get_required_storage_deposit(
            near_bridge_client.utxo_chain_token(chain)?,
            deposit_msg.recipient_id.clone(),
        )
        .await
    {
        Ok(amount) if amount > 0 => {
            if omni_connector
                .near_storage_deposit_for_token(
                    near_bridge_client.utxo_chain_token(chain)?,
                    amount,
                    deposit_msg.recipient_id.clone(),
                    TransactionOptions {
                        nonce,
                        wait_until: near_primitives::views::TxExecutionStatus::Final,
                        wait_final_outcome_timeout_sec: None,
                    },
                )
                .await
                .is_err()
            {
                warn!(
                    "Failed to deposit storage for {chain:?}->NEAR transfer ({btc_tx_hash}:{vout}): token={:?}, recipient={}",
                    near_bridge_client.utxo_chain_token(chain)?,
                    deposit_msg.recipient_id
                );
                return Ok(EventAction::Retry);
            }

            nonce = match near_nonce.reserve_nonce() {
                Ok(nonce) => Some(nonce),
                Err(err) => {
                    warn!(
                        "Failed to reserve nonce after storage deposit for {chain:?}->NEAR fin_transfer ({btc_tx_hash}:{vout}): {err:?}"
                    );
                    return Ok(EventAction::Retry);
                }
            };
        }
        Ok(_) => {}
        Err(err) => {
            warn!(
                "Failed to get required storage deposit for {chain:?}->NEAR transfer ({btc_tx_hash}:{vout}): token={:?}, recipient={}: {err:?}",
                near_bridge_client.utxo_chain_token(chain)?,
                deposit_msg.recipient_id
            );
            return Ok(EventAction::Retry);
        }
    }

    let Ok(vout_usize) = usize::try_from(vout) else {
        warn!("Invalid vout {vout} for {chain:?}->NEAR transfer ({btc_tx_hash}), dropping");
        return Ok(EventAction::Drop);
    };

    let uses_extra_msg_path = deposit_msg.safe_deposit.is_none() && deposit_msg.extra_msg.is_some();
    let defer_key = format!(
        "utxo-deposit:{}",
        UtxoId {
            tx_hash: btc_tx_hash.clone(),
            vout,
        }
    );

    let fin_transfer_args = FinTransferArgs::NearFinTransferBTC {
        chain_kind: chain,
        btc_tx_hash: btc_tx_hash.clone(),
        vout: vout_usize,
        btc_deposit_args: BtcDepositArgs::DepositMsg {
            msg: DepositMsg {
                recipient_id: deposit_msg.recipient_id.clone(),
                post_actions: deposit_msg.post_actions.map(|optional_actions| {
                    optional_actions
                        .into_iter()
                        .map(|action| PostAction {
                            receiver_id: action.receiver_id,
                            amount: action.amount.0,
                            memo: action.memo,
                            msg: action.msg,
                            gas: action.gas.map(near_primitives::gas::Gas::from_gas),
                        })
                        .collect()
                }),
                extra_msg: deposit_msg.extra_msg,
                safe_deposit: deposit_msg.safe_deposit.map(|safe_deposit| SafeDepositMsg {
                    msg: safe_deposit.msg,
                }),
                refund_address: deposit_msg.refund_address,
            },
        },
        prefetched: None,
        transaction_options: TransactionOptions {
            nonce,
            wait_until: near_primitives::views::TxExecutionStatus::Final,
            wait_final_outcome_timeout_sec: None,
        },
    };

    match omni_connector.fin_transfer(fin_transfer_args).await {
        Ok(tx_hash) => {
            info!(
                "Finalized {chain:?}->NEAR transfer on NEAR ({btc_tx_hash}:{vout}): near_fin_tx_hash={tx_hash:?}"
            );

            let Ok(tx_hash) = CryptoHash::from_str(&tx_hash) else {
                anyhow::bail!(
                    "Invalid NEAR tx hash for {chain:?}->NEAR transfer ({btc_tx_hash}:{vout}): {tx_hash}"
                );
            };
            let signer = omni_connector
                .near_bridge_client()
                .and_then(near_bridge_client::NearBridgeClient::account_id)?;

            match utils::near::tx_has_errors(
                jsonrpc_client,
                tx_hash,
                signer,
                &[
                    "Not enough blocks confirmed",
                    "Not enough confirmations for the block-cumulative bridge amount",
                ],
            )
            .await
            {
                Ok(true) => {}
                Ok(false) => return Ok(EventAction::Remove),
                Err(err) => {
                    warn!(
                        "Failed to check receipts for {chain:?}->NEAR transfer ({btc_tx_hash}:{vout}), retrying: {err:?}"
                    );
                    return Ok(EventAction::Retry);
                }
            }

            match utils::utxo::exact_lc_target_block(
                &omni_connector,
                chain,
                &btc_tx_hash,
                BtcTxType::Deposit {
                    amount: amount.0,
                    uses_extra_msg_path,
                },
            )
            .await
            {
                Ok(target_block) => {
                    return Ok(defer_to_lc_poller(
                        config,
                        redis,
                        chain,
                        target_block,
                        &defer_key,
                        &transfer,
                    )
                    .await);
                }
                Err(err) => warn!(
                    "Failed to compute LC defer target for {chain:?}:{btc_tx_hash}, retrying: {err:?}"
                ),
            }

            Ok(EventAction::Retry)
        }
        Err(err) => {
            if let BridgeSdkError::LightClientNotSynced {
                current_height,
                target_height,
            } = err
            {
                warn!(
                    "{chain:?} light client is not synced yet for {chain:?}->NEAR transfer ({btc_tx_hash}:{vout}), current: {current_height}, waiting for: {target_height}"
                );
                return Ok(defer_to_lc_poller(
                    config,
                    redis,
                    chain,
                    target_height,
                    &defer_key,
                    &transfer,
                )
                .await);
            }

            Err(err).with_context(|| {
                format!(
                    "Failed to finalize {chain:?}->NEAR transfer on NEAR ({btc_tx_hash}:{vout})"
                )
            })
        }
    }
}

const DUPLICATE_BROADCAST_MARKERS: [&str; 6] = [
    "already in state",
    "already in block chain",
    "already in mempool",
    "txn-already-in-mempool",
    "txn-already-known",
    "transaction already exists",
];

fn duplicate_broadcast_marker(err: &BridgeSdkError) -> Option<&'static str> {
    let (BridgeSdkError::UtxoRpcError(msg) | BridgeSdkError::UtxoClientError(msg)) = err else {
        return None;
    };
    let msg = msg.to_lowercase();
    DUPLICATE_BROADCAST_MARKERS
        .into_iter()
        .find(|marker| msg.contains(marker))
}

async fn mark_near_to_utxo_signed(
    config: &config::Config,
    redis: &mut redis::aio::ConnectionManager,
    chain: ChainKind,
    btc_pending_id: Option<&str>,
) {
    if config.utxo_sign_delay_secs(chain) == 0 {
        return;
    }
    let Some(btc_pending_id) = btc_pending_id else {
        return;
    };

    let signed_key = utils::redis::near_to_utxo_signed_key(btc_pending_id);
    let now = chrono::Utc::now().timestamp().to_string();
    utils::redis::set_with_ttl(
        config,
        redis,
        &signed_key,
        &now,
        utils::redis::NEAR_TO_UTXO_SIGNED_TTL_SECS,
    )
    .await;
}

pub async fn process_sign_transaction_event(
    config: &config::Config,
    redis: &mut redis::aio::ConnectionManager,
    omni_connector: Arc<OmniConnector>,
    sign_utxo_transaction_event: SignUtxoTransaction,
) -> Result<EventAction> {
    let chain = sign_utxo_transaction_event.chain;
    let btc_pending_id_log = sign_utxo_transaction_event
        .btc_pending_id
        .as_deref()
        .unwrap_or("?");
    let near_sign_tx_hash_log = sign_utxo_transaction_event.near_tx_hash.clone();

    info!(
        "Processing SignBtcTransaction log on NEAR for NEAR->{chain:?} ({btc_pending_id_log}) via near_sign_tx_hash={near_sign_tx_hash_log}"
    );

    let Ok(near_tx_hash) = CryptoHash::from_str(&sign_utxo_transaction_event.near_tx_hash) else {
        warn!(
            "Invalid tx hash, dropping: NEAR->{chain:?} ({btc_pending_id_log}): {}",
            sign_utxo_transaction_event.near_tx_hash
        );
        return Ok(EventAction::Drop);
    };

    match omni_connector
        .btc_fin_transfer(
            sign_utxo_transaction_event.chain,
            near_tx_hash,
            Some(sign_utxo_transaction_event.relayer),
        )
        .await
    {
        Ok(tx_hash) => {
            info!(
                "Broadcast NEAR->{chain:?} transfer ({btc_pending_id_log}) via near_sign_tx_hash={near_sign_tx_hash_log}: btc_tx_hash={tx_hash}"
            );

            mark_near_to_utxo_signed(
                config,
                redis,
                chain,
                sign_utxo_transaction_event.btc_pending_id.as_deref(),
            )
            .await;

            Ok(EventAction::Remove)
        }
        Err(err) => {
            // The node already has this transaction: an earlier broadcast of the
            // same signed transaction landed. Retrying re-sends the same bytes
            // and gets the same rejection forever, so treat it as relayed.
            if let Some(marker) = duplicate_broadcast_marker(&err) {
                info!(
                    "NEAR->{chain:?} transfer ({btc_pending_id_log}) via near_sign_tx_hash={near_sign_tx_hash_log} was already broadcast (node reported \"{marker}\"), removing: {err:?}"
                );
                mark_near_to_utxo_signed(
                    config,
                    redis,
                    chain,
                    sign_utxo_transaction_event.btc_pending_id.as_deref(),
                )
                .await;
                return Ok(EventAction::Remove);
            }

            // The one stall whose `target_chain` the central classifier cannot
            // recover: this message's origin chain is NEAR, but what rejected
            // the broadcast is the BTC/Zcash node. Classify it here, as main did.
            if matches!(
                err,
                BridgeSdkError::UtxoRpcError(_) | BridgeSdkError::UtxoClientError(_)
            ) {
                warn!(
                    "Failed to broadcast NEAR->{chain:?} transfer ({btc_pending_id_log}) via near_sign_tx_hash={near_sign_tx_hash_log}, retrying: {err:?}"
                );
                Metrics::global().record_stalled_retry(stall_reason::UTXO_RPC, Some(chain));
                return Ok(EventAction::Retry);
            }

            if let BridgeSdkError::UnknownError(msg) = &err
                && msg == "Failed to find correct receipt"
            {
                warn!(
                    "Failed to broadcast NEAR->{chain:?} transfer ({btc_pending_id_log}) via near_sign_tx_hash={near_sign_tx_hash_log}, dropping: {err:?}"
                );
                return Ok(EventAction::Drop);
            }

            Err(err).with_context(|| {
                format!(
                    "Failed to broadcast NEAR->{chain:?} transfer ({btc_pending_id_log}) via near_sign_tx_hash={near_sign_tx_hash_log}"
                )
            })
        }
    }
}

pub async fn process_confirmed_tx_hash(
    config: &config::Config,
    redis: &mut redis::aio::ConnectionManager,
    jsonrpc_client: &JsonRpcClient,
    omni_connector: Arc<OmniConnector>,
    confirmed_tx_hash: ConfirmedTxHash,
    near_nonce: Arc<utils::nonce::NonceManager>,
) -> Result<EventAction> {
    let Ok(client) = omni_connector.near_bridge_client() else {
        anyhow::bail!("Near bridge client is not configured");
    };

    let pending_info = match client
        .get_btc_pending_info(
            confirmed_tx_hash.chain,
            confirmed_tx_hash.btc_tx_hash.clone(),
        )
        .await
    {
        Ok(info) => info,
        Err(BridgeSdkError::InvalidArgument(err)) if err == "BTC pending info not found" => {
            warn!(
                "BTC pending info is not found for {} ({:?}), dropping",
                confirmed_tx_hash.btc_tx_hash, confirmed_tx_hash.chain,
            );
            return Ok(EventAction::Drop);
        }
        Err(err) => {
            warn!(
                "Failed to fetch BTC pending info for {} ({:?}), retrying: {err:?}",
                confirmed_tx_hash.btc_tx_hash, confirmed_tx_hash.chain,
            );
            return Ok(EventAction::Retry);
        }
    };

    let chain = confirmed_tx_hash.chain;
    let btc_tx_hash = &confirmed_tx_hash.btc_tx_hash;

    let action = if pending_info.state.is_active_utxo_management() {
        "active utxo management"
    } else {
        "withdraw"
    };

    let nonce = match near_nonce.reserve_nonce() {
        Ok(nonce) => Some(nonce),
        Err(err) => {
            warn!("Failed to reserve nonce for {chain:?} {action} ({btc_tx_hash}): {err:?}");
            return Ok(EventAction::Retry);
        }
    };

    let transaction_options = TransactionOptions {
        nonce,
        wait_until: near_primitives::views::TxExecutionStatus::Final,
        wait_final_outcome_timeout_sec: None,
    };

    let verify_result = if pending_info.state.is_active_utxo_management() {
        omni_connector
            .near_btc_verify_active_utxo_management(
                confirmed_tx_hash.chain,
                confirmed_tx_hash.btc_tx_hash.clone(),
                transaction_options,
            )
            .await
    } else {
        omni_connector
            .near_btc_verify_withdraw(
                confirmed_tx_hash.chain,
                confirmed_tx_hash.btc_tx_hash.clone(),
                transaction_options,
            )
            .await
    };

    match verify_result {
        Ok(tx_hash) => {
            info!(
                "Verified NEAR->{chain:?} {action} on NEAR ({btc_tx_hash}): near_verify_tx_hash={tx_hash:?}"
            );

            let signer = omni_connector
                .near_bridge_client()
                .and_then(near_bridge_client::NearBridgeClient::account_id)?;

            Ok(utils::near::resolve_tx_action(
                jsonrpc_client,
                tx_hash,
                signer,
                &["Not enough blocks confirmed"],
            )
            .await)
        }
        Err(err) => {
            if let BridgeSdkError::LightClientNotSynced {
                current_height,
                target_height,
            } = err
            {
                warn!(
                    "Light client is not synced yet for NEAR->{chain:?} {action} ({btc_tx_hash}), current: {current_height}, waiting for: {target_height}"
                );
                return Ok(defer_to_lc_poller(
                    config,
                    redis,
                    chain,
                    target_height,
                    &format!("utxo-withdraw:{btc_tx_hash}"),
                    &confirmed_tx_hash,
                )
                .await);
            }

            Err(err).with_context(|| {
                format!("Failed to verify NEAR->{chain:?} {action} ({btc_tx_hash})")
            })
        }
    }
}

async fn defer_to_lc_poller<E>(
    config: &config::Config,
    redis: &mut redis::aio::ConnectionManager,
    chain: ChainKind,
    target_block: u64,
    key: &str,
    event: &E,
) -> EventAction
where
    E: serde::Serialize + std::fmt::Debug + Send,
{
    match utils::utxo::store_pending_lc_event(config, redis, chain, target_block, key, event).await
    {
        Ok(()) => {
            info!("Deferred {key} to LC poller (target_block={target_block})");
            EventAction::Remove
        }
        Err(err) => {
            warn!("Failed to defer {key} to LC poller, retrying: {err:?}");
            EventAction::Retry
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exact text seen in production from Zebra when the NEAR->Zcash
    /// transfer had already been broadcast.
    const ZEBRA_ALREADY_IN_STATE: &str = "Failed to parse sendrawtransaction result: invalid type: null, expected a string. Response: {\"jsonrpc\":\"2.0\",\"id\":1,\"error\":{\"code\":-25,\"message\":\"failed to validate tx: WtxId(\\\"private\\\"), error: transaction is already in state\"}}";

    #[test]
    fn batched_item_covers_every_input() {
        assert_eq!(sign_index_range(0, Some(20)), 0..20);
    }

    #[test]
    fn legacy_item_covers_exactly_one_input() {
        assert_eq!(sign_index_range(7, None), 7..8);
    }

    #[test]
    fn zero_or_missing_count_still_signs_one_input() {
        assert_eq!(sign_index_range(3, Some(0)), 3..4);
    }

    #[test]
    fn resumed_item_starts_at_its_own_index() {
        // A redelivered batch that resumes mid-transaction keeps its span.
        assert_eq!(sign_index_range(5, Some(4)), 5..9);
    }

    #[test]
    fn overflowing_count_does_not_panic() {
        assert_eq!(sign_index_range(u64::MAX, Some(4)), u64::MAX..u64::MAX);
    }

    #[test]
    fn zebra_already_in_state_is_a_duplicate() {
        let err = BridgeSdkError::UtxoRpcError(ZEBRA_ALREADY_IN_STATE.to_string());
        assert_eq!(duplicate_broadcast_marker(&err), Some("already in state"));
    }

    #[test]
    fn bitcoin_core_duplicate_rejections_are_duplicates() {
        for (msg, expected) in [
            (
                "Failed to parse sendrawtransaction result. Response: {\"error\":{\"code\":-27,\"message\":\"Transaction already in block chain\"}}",
                "already in block chain",
            ),
            (
                "sendrawtransaction failed: txn-already-in-mempool",
                "txn-already-in-mempool",
            ),
            (
                "sendrawtransaction failed: transaction already in mempool",
                "already in mempool",
            ),
            (
                "sendrawtransaction failed: txn-already-known",
                "txn-already-known",
            ),
        ] {
            let err = BridgeSdkError::UtxoRpcError(msg.to_string());
            assert_eq!(duplicate_broadcast_marker(&err), Some(expected), "{msg}");
        }
    }

    #[test]
    fn client_error_variant_is_also_matched() {
        let err = BridgeSdkError::UtxoClientError("transaction is already in state".to_string());
        assert_eq!(duplicate_broadcast_marker(&err), Some("already in state"));
    }

    #[test]
    fn transient_utxo_rpc_errors_are_not_duplicates() {
        for msg in [
            "Failed to send transaction: error sending request for url (http://node:8232/)",
            "Failed to read sendrawtransaction response: operation timed out",
            "Failed to parse sendrawtransaction result: invalid type: null, expected a string. Response: {\"error\":{\"code\":-26,\"message\":\"bad-txns-inputs-missingorspent\"}}",
            "Failed to estimate fee_rate: None",
        ] {
            let err = BridgeSdkError::UtxoRpcError(msg.to_string());
            assert_eq!(duplicate_broadcast_marker(&err), None, "{msg}");
        }
    }

    #[test]
    fn non_utxo_errors_are_not_duplicates() {
        let err = BridgeSdkError::UnknownError("transaction is already in state".to_string());
        assert_eq!(duplicate_broadcast_marker(&err), None);
    }
}
