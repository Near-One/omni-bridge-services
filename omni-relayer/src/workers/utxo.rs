use std::{str::FromStr, sync::Arc};

use anyhow::{Context, Result};
use bridge_connector_common::result::BridgeSdkError;
use near_bridge_client::{
    TransactionOptions,
    btc::{DepositMsg, PostAction, SafeDepositMsg},
};
use near_jsonrpc_client::{JsonRpcClient, errors::JsonRpcError};
use near_primitives::{hash::CryptoHash, types::AccountId};
use near_rpc_client::NearRpcError;
use omni_types::{ChainKind, OmniAddress};
use tracing::{info, warn};

use omni_connector::{BtcDepositArgs, BtcTxType, FinTransferArgs, OmniConnector};

use crate::{config, utils};

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

pub async fn process_near_to_utxo_init_transfer_event(
    config: &config::Config,
    redis: &mut redis::aio::ConnectionManager,
    jsonrpc_client: &JsonRpcClient,
    omni_connector: Arc<OmniConnector>,
    transfer: Transfer,
    near_nonce: Arc<utils::nonce::NonceManager>,
) -> Result<EventAction> {
    let Transfer::NearToUtxo {
        chain,
        btc_pending_id,
        sign_index,
        sender,
        creation_timestamp,
    } = transfer
    else {
        anyhow::bail!("Expected NearToUtxoTransfer, got: {transfer:?}");
    };

    if !config.is_signing_utxo_transaction_enabled(chain) {
        info!(
            "Signing NEAR->{chain:?} disabled by config ({btc_pending_id}:{sign_index}), skipping"
        );
        return Ok(EventAction::Remove);
    }

    let current_timestamp = chrono::Utc::now().timestamp();
    if current_timestamp < creation_timestamp + config.kyt.delay_secs {
        let remaining =
            (creation_timestamp + config.kyt.delay_secs - current_timestamp).unsigned_abs();
        return Ok(EventAction::RetryAfter(std::time::Duration::from_secs(
            remaining,
        )));
    }

    let context = format!("({btc_pending_id}:{sign_index})");
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

    match utils::redis::exists(config, redis, &signed_key).await {
        Some(true) => {
            info!(
                "Skipping sign for {btc_pending_id}:{sign_index} - already handled by another relayer"
            );
            return Ok(EventAction::Remove);
        }
        Some(false) => {}
        None => {
            warn!(
                "Redis exists failed for {btc_pending_id}:{sign_index}; proceeding to sign and letting the contract dedupe"
            );
        }
    }

    let nonce = match near_nonce.reserve_nonce() {
        Ok(nonce) => Some(nonce),
        Err(err) => {
            warn!(
                "Failed to reserve nonce for NEAR->{chain:?} sign ({btc_pending_id}:{sign_index}): {err:?}"
            );
            return Ok(EventAction::Retry);
        }
    };

    let btc_pending_id_log = btc_pending_id.clone();
    match omni_connector
        .near_sign_btc_transaction(
            chain,
            btc_pending_id,
            sign_index,
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
                "Signed NEAR->{chain:?} input ({btc_pending_id_log}:{sign_index}): near_sign_tx_hash={tx_hash:?}"
            );

            let signer = omni_connector
                .near_bridge_client()
                .and_then(near_bridge_client::NearBridgeClient::account_id)?;

            Ok(utils::near::resolve_tx_action(
                jsonrpc_client,
                tx_hash,
                signer,
                &["Request has timed out."],
            )
            .await)
        }
        Err(err) => {
            if let BridgeSdkError::NearRpcError(near_rpc_error) = err {
                match near_rpc_error {
                    NearRpcError::NonceError
                    | NearRpcError::FinalizationError
                    | NearRpcError::RpcBroadcastTxAsyncError(_)
                    | NearRpcError::RpcQueryError(
                        JsonRpcError::TransportError(_) | JsonRpcError::ServerError(_),
                    )
                    | NearRpcError::RpcTransactionError(_) => {
                        warn!(
                            "Failed to sign NEAR->{chain:?} input ({btc_pending_id_log}:{sign_index}), retrying: {near_rpc_error:?}"
                        );
                        return Ok(EventAction::Retry);
                    }
                    _ => {
                        anyhow::bail!(
                            "Failed to sign NEAR->{chain:?} input ({btc_pending_id_log}:{sign_index}): {near_rpc_error:?}"
                        );
                    }
                };
            }
            anyhow::bail!(
                "Failed to sign NEAR->{chain:?} input ({btc_pending_id_log}:{sign_index}): {err:?}"
            );
        }
    }
}

pub async fn process_utxo_to_near_init_transfer_event(
    config: &config::Config,
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
    } = transfer
    else {
        anyhow::bail!("Expected UtxoToNearTransfer, got: {transfer:?}");
    };

    let uses_extra_msg_path = deposit_msg.safe_deposit.is_none() && deposit_msg.extra_msg.is_some();

    if config::Config::is_kyt_enabled() {
        let rpc_url = match chain {
            ChainKind::Btc => config.btc.as_ref().map(|cfg| cfg.rpc_http_url.as_str()),
            ChainKind::Zcash => config.zcash.as_ref().map(|cfg| cfg.rpc_http_url.as_str()),
            _ => anyhow::bail!("UtxoToNear transfer for unsupported chain {chain:?}"),
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

    let fin_transfer_args = FinTransferArgs::NearFinTransferBTC {
        chain_kind: chain,
        btc_tx_hash: btc_tx_hash.clone(),
        vout: usize::try_from(vout)?,
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

            let event_action = utils::near::resolve_tx_action(
                jsonrpc_client,
                tx_hash,
                signer,
                &[
                    "Not enough blocks confirmed",
                    "Not enough confirmations for the block-cumulative bridge amount",
                ],
            )
            .await;

            if matches!(event_action, EventAction::Retry) && amount.0 > 0 {
                return Ok(utils::utxo::defer_action(
                    &omni_connector,
                    chain,
                    &btc_tx_hash,
                    BtcTxType::Deposit {
                        amount: amount.0,
                        uses_extra_msg_path,
                    },
                )
                .await);
            }

            Ok(event_action)
        }
        Err(err) => {
            if let BridgeSdkError::NearRpcError(near_rpc_error) = err {
                match near_rpc_error {
                    NearRpcError::NonceError
                    | NearRpcError::FinalizationError
                    | NearRpcError::RpcBroadcastTxAsyncError(_)
                    | NearRpcError::RpcQueryError(
                        JsonRpcError::TransportError(_) | JsonRpcError::ServerError(_),
                    )
                    | NearRpcError::RpcTransactionError(_) => {
                        warn!(
                            "Failed to finalize {chain:?}->NEAR transfer on NEAR ({btc_tx_hash}:{vout}), retrying: {near_rpc_error:?}"
                        );
                        return Ok(EventAction::Retry);
                    }
                    _ => {
                        anyhow::bail!(
                            "Failed to finalize {chain:?}->NEAR transfer on NEAR ({btc_tx_hash}:{vout}): {near_rpc_error:?}"
                        );
                    }
                };
            }

            if let BridgeSdkError::LightClientNotSynced {
                current_height,
                target_height,
            } = err
            {
                warn!(
                    "{chain:?} light client is not synced yet for {chain:?}->NEAR transfer ({btc_tx_hash}:{vout}), current: {current_height}, waiting for: {target_height}"
                );
                return Ok(EventAction::DeferUntilBlock {
                    chain,
                    target_block: target_height,
                });
            }

            anyhow::bail!(
                "Failed to finalize {chain:?}->NEAR transfer on NEAR ({btc_tx_hash}:{vout}): {err:?}"
            );
        }
    }
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
        anyhow::bail!(
            "Invalid near tx hash for NEAR->{chain:?} ({btc_pending_id_log}): {}",
            sign_utxo_transaction_event.near_tx_hash
        );
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

            if config.utxo_sign_delay_secs(chain) > 0
                && let Some(btc_pending_id) = sign_utxo_transaction_event.btc_pending_id.as_deref()
            {
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

            Ok(EventAction::Remove)
        }
        Err(err) => {
            if let BridgeSdkError::NearRpcError(near_rpc_error) = err {
                match near_rpc_error {
                    NearRpcError::NonceError
                    | NearRpcError::FinalizationError
                    | NearRpcError::RpcBroadcastTxAsyncError(_)
                    | NearRpcError::RpcQueryError(
                        JsonRpcError::TransportError(_) | JsonRpcError::ServerError(_),
                    )
                    | NearRpcError::RpcTransactionError(_) => {
                        warn!(
                            "Failed to broadcast NEAR->{chain:?} transfer ({btc_pending_id_log}) via near_sign_tx_hash={near_sign_tx_hash_log}, retrying: {near_rpc_error:?}"
                        );
                        return Ok(EventAction::Retry);
                    }
                    _ => {
                        anyhow::bail!(
                            "Failed to broadcast NEAR->{chain:?} transfer ({btc_pending_id_log}) via near_sign_tx_hash={near_sign_tx_hash_log}: {near_rpc_error:?}"
                        );
                    }
                };
            } else if let BridgeSdkError::UtxoRpcError(err) = err {
                warn!(
                    "Failed to broadcast NEAR->{chain:?} transfer ({btc_pending_id_log}) via near_sign_tx_hash={near_sign_tx_hash_log}, retrying: {err:?}"
                );
                return Ok(EventAction::Retry);
            }

            anyhow::bail!(
                "Failed to broadcast NEAR->{chain:?} transfer ({btc_pending_id_log}) via near_sign_tx_hash={near_sign_tx_hash_log}: {err:?}"
            );
        }
    }
}

pub async fn process_confirmed_tx_hash(
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
            anyhow::bail!(
                "BTC pending info is not found for {} ({:?})",
                confirmed_tx_hash.btc_tx_hash,
                confirmed_tx_hash.chain,
            );
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
            if let BridgeSdkError::NearRpcError(near_rpc_error) = err {
                match near_rpc_error {
                    NearRpcError::NonceError
                    | NearRpcError::FinalizationError
                    | NearRpcError::RpcBroadcastTxAsyncError(_)
                    | NearRpcError::RpcQueryError(
                        JsonRpcError::TransportError(_) | JsonRpcError::ServerError(_),
                    )
                    | NearRpcError::RpcTransactionError(_) => {
                        warn!(
                            "Failed to verify NEAR->{chain:?} {action} ({btc_tx_hash}), retrying: {near_rpc_error:?}"
                        );
                        return Ok(EventAction::Retry);
                    }
                    _ => {
                        anyhow::bail!(
                            "Failed to verify NEAR->{chain:?} {action} ({btc_tx_hash}): {near_rpc_error:?}"
                        );
                    }
                };
            }

            if let BridgeSdkError::LightClientNotSynced {
                current_height,
                target_height,
            } = err
            {
                warn!(
                    "Light client is not synced yet for NEAR->{chain:?} {action} ({btc_tx_hash}), current: {current_height}, waiting for: {target_height}"
                );
                return Ok(EventAction::DeferUntilBlock {
                    chain,
                    target_block: target_height,
                });
            }

            anyhow::bail!("Failed to verify NEAR->{chain:?} {action} ({btc_tx_hash}): {err:?}");
        }
    }
}
