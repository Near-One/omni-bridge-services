use std::{str::FromStr, sync::Arc};

use anyhow::{Context, Result};
use bridge_connector_common::result::BridgeSdkError;
use near_bridge_client::{
    TransactionOptions,
    btc::{DepositMsg, PendingInfoState, PostAction, SafeDepositMsg},
};
use near_jsonrpc_client::{JsonRpcClient, errors::JsonRpcError};
use near_primitives::{hash::CryptoHash, types::AccountId};
use near_rpc_client::NearRpcError;
use omni_types::{ChainKind, OmniAddress};
use tracing::{info, warn};

use omni_connector::{BtcDepositArgs, FinTransferArgs, OmniConnector};

use crate::{config, utils};

use super::{EventAction, Transfer};

#[derive(Debug, serde::Serialize, serde::Deserialize, Clone)]
pub struct SignUtxoTransaction {
    pub chain: ChainKind,
    pub near_tx_hash: String,
    pub relayer: AccountId,
}

#[derive(Debug, serde::Serialize, serde::Deserialize, Clone)]
pub struct ConfirmedTxHash {
    pub chain: ChainKind,
    pub btc_tx_hash: String,
}

pub async fn process_near_to_utxo_init_transfer_event(
    config: &config::Config,
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
        info!("Signing UTXO transactions for {chain:?} is disabled, skipping");
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

    let context = format!("(btc_pending_id={btc_pending_id}, sign_index={sign_index})");
    if let Some(action) = super::near::check_kyt(&OmniAddress::Near(sender), &context).await {
        return Ok(action);
    }

    let nonce = match near_nonce.reserve_nonce().await {
        Ok(nonce) => Some(nonce),
        Err(err) => {
            warn!("Failed to reserve nonce: {err:?}");
            return Ok(EventAction::Retry);
        }
    };

    match omni_connector
        .near_sign_btc_transaction(
            chain,
            btc_pending_id,
            sign_index,
            TransactionOptions {
                nonce,
                wait_until: near_primitives::views::TxExecutionStatus::Included,
                wait_final_outcome_timeout_sec: None,
            },
        )
        .await
    {
        Ok(tx_hash) => {
            info!("Signed {chain:?} transaction: {tx_hash:?}");
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
                        warn!("Failed to sign {chain:?} transaction, retrying: {near_rpc_error:?}");
                        return Ok(EventAction::Retry);
                    }
                    _ => {
                        anyhow::bail!("Failed to sign {chain:?} transaction: {near_rpc_error:?}");
                    }
                };
            }
            anyhow::bail!("Failed to sign {chain:?} transaction: {err:?}");
        }
    }
}

pub async fn process_utxo_to_near_init_transfer_event(
    config: &config::Config,
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
    } = transfer
    else {
        anyhow::bail!("Expected UtxoToNearTransfer, got: {transfer:?}");
    };

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
        if let Some(action) = super::near::check_kyt_senders(&input_addresses, &context).await {
            return Ok(action);
        }
    }

    let mut nonce = match near_nonce.reserve_nonce().await {
        Ok(nonce) => Some(nonce),
        Err(err) => {
            warn!("Failed to reserve nonce: {err:?}");
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
                    "Failed to deposit storage for token {:?} to {}",
                    near_bridge_client.utxo_chain_token(chain)?,
                    deposit_msg.recipient_id
                );
                return Ok(EventAction::Retry);
            }

            nonce = match near_nonce.reserve_nonce().await {
                Ok(nonce) => Some(nonce),
                Err(err) => {
                    warn!("Failed to reserve nonce: {err:?}");
                    return Ok(EventAction::Retry);
                }
            };
        }
        Ok(_) => {}
        Err(err) => {
            warn!(
                "Failed to get required storage deposit for token {:?} to {}: {err:?}",
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
        transaction_options: TransactionOptions {
            nonce,
            wait_until: near_primitives::views::TxExecutionStatus::Included,
            wait_final_outcome_timeout_sec: None,
        },
    };

    match omni_connector.fin_transfer(fin_transfer_args).await {
        Ok(tx_hash) => {
            info!("Finalized {chain:?} transaction: {tx_hash:?}");
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
                            "Failed to finalize {chain:?} transaction, retrying: {near_rpc_error:?}"
                        );
                        return Ok(EventAction::Retry);
                    }
                    _ => {
                        anyhow::bail!(
                            "Failed to finalize {chain:?} transaction: {near_rpc_error:?}"
                        );
                    }
                };
            }

            if let BridgeSdkError::LightClientNotSynced(block) = err {
                warn!(
                    "{chain:?} light client is not synced yet for transfer ({btc_tx_hash}), block: {block}",
                );
                return Ok(EventAction::Retry);
            }

            anyhow::bail!("Failed to finalize {chain:?} transaction: {err:?}");
        }
    }
}

pub async fn process_sign_transaction_event(
    omni_connector: Arc<OmniConnector>,
    sign_utxo_transaction_event: SignUtxoTransaction,
) -> Result<EventAction> {
    info!("Trying to process SignBtcTransaction log on NEAR");

    let Ok(near_tx_hash) = CryptoHash::from_str(&sign_utxo_transaction_event.near_tx_hash) else {
        anyhow::bail!(
            "Invalid near tx hash: {}",
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
                "Finalized {:?} transaction: {tx_hash}",
                sign_utxo_transaction_event.chain
            );
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
                            "Failed to finalize {:?} transaction ({}), retrying: {near_rpc_error:?}",
                            sign_utxo_transaction_event.chain,
                            sign_utxo_transaction_event.near_tx_hash
                        );
                        return Ok(EventAction::Retry);
                    }
                    _ => {
                        anyhow::bail!(
                            "Failed to finalize {:?} transaction ({}): {near_rpc_error:?}",
                            sign_utxo_transaction_event.chain,
                            sign_utxo_transaction_event.near_tx_hash
                        );
                    }
                };
            } else if let BridgeSdkError::UtxoRpcError(err) = err {
                warn!(
                    "Failed to finalize {:?} transaction ({}), retrying: {err:?}",
                    sign_utxo_transaction_event.chain, sign_utxo_transaction_event.near_tx_hash
                );
                return Ok(EventAction::Retry);
            }

            anyhow::bail!(
                "Failed to finalize {:?} transaction ({}): {err:?}",
                sign_utxo_transaction_event.chain,
                sign_utxo_transaction_event.near_tx_hash
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
        .get_btc_pending_info(confirmed_tx_hash.chain, confirmed_tx_hash.btc_tx_hash.clone())
        .await
    {
        Ok(info) => info,
        Err(err) => {
            warn!(
                "Failed to fetch BTC pending info for {} ({:?}), retrying: {err:?}",
                confirmed_tx_hash.btc_tx_hash, confirmed_tx_hash.chain,
            );
            return Ok(EventAction::Retry);
        }
    };

    let is_active_management = matches!(
        pending_info.state,
        PendingInfoState::ActiveUtxoManagementOriginal(_)
            | PendingInfoState::ActiveUtxoManagementRbf(_)
            | PendingInfoState::ActiveUtxoManagementCancelRbf(_)
    );

    let nonce = match near_nonce.reserve_nonce().await {
        Ok(nonce) => Some(nonce),
        Err(err) => {
            warn!("Failed to reserve nonce: {err:?}");
            return Ok(EventAction::Retry);
        }
    };

    let transaction_options = TransactionOptions {
        nonce,
        wait_until: near_primitives::views::TxExecutionStatus::Final,
        wait_final_outcome_timeout_sec: None,
    };

    let verify_result = if is_active_management {
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

    let action = if is_active_management {
        "verify active utxo management"
    } else {
        "verify withdraw"
    };

    match verify_result {
        Ok(tx_hash) => {
            info!("Verified {action} ({}): {tx_hash:?}", confirmed_tx_hash.btc_tx_hash);

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
                        warn!("Failed to {action}, retrying: {near_rpc_error:?}");
                        return Ok(EventAction::Retry);
                    }
                    _ => {
                        anyhow::bail!("Failed to {action}: {near_rpc_error:?}");
                    }
                };
            }

            if let BridgeSdkError::LightClientNotSynced(block) = err {
                warn!(
                    "Light client is not synced yet for {}, block: {block}",
                    confirmed_tx_hash.btc_tx_hash
                );
                return Ok(EventAction::Retry);
            }

            anyhow::bail!("Failed to {action}: {err:?}");
        }
    }
}
