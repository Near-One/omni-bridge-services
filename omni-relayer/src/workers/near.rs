use std::sync::Arc;

use anyhow::{Context, Result};
use bridge_connector_common::result::BridgeSdkError;
use near_sdk::json_types::U64;
use tracing::{info, warn};

use near_bridge_client::TransactionOptions;
use near_jsonrpc_client::{JsonRpcClient, errors::JsonRpcError};
use near_primitives::types::AccountId;
use near_rpc_client::NearRpcError;
use solana_client::rpc_request::RpcResponseErrorData;
use solana_rpc_client_api::{client_error::ErrorKind, request::RpcError};
use solana_sdk::{instruction::InstructionError, pubkey::Pubkey, transaction::TransactionError};

use omni_connector::OmniConnector;
use omni_types::{ChainKind, FastTransfer, OmniAddress, TransferId, near_events::OmniBridgeEvent};

use crate::{
    config, utils, utils::pending_transactions::PendingTransaction, workers::PAUSED_ERROR,
};

use super::{EventAction, Transfer, WorkerEvent};

#[derive(Debug, serde::Deserialize)]
enum UTXOChainMsg {
    MaxGasFee(U64),
}

pub(super) async fn check_kyt(sender: &OmniAddress, context: &str) -> Option<EventAction> {
    check_kyt_senders(std::slice::from_ref(sender), context).await
}

pub(super) async fn check_kyt_senders(
    senders: &[OmniAddress],
    context: &str,
) -> Option<EventAction> {
    if !config::Config::is_kyt_enabled() {
        return None;
    }

    match utils::kyt::check_senders(senders).await {
        Ok(utils::kyt::SuggestedAction::StopRelaying) => {
            warn!(
                "KYT suggested STOP_RELAYING for senders {senders:?}, rejecting transfer {context}"
            );
            Some(EventAction::Remove)
        }
        Ok(utils::kyt::SuggestedAction::None) => None,
        Err(err) => {
            warn!("KYT check failed for {senders:?}: {err:?}, retrying");
            Some(EventAction::Retry)
        }
    }
}

/// Enforces the configured sender allowlist for `destination_chain`. Returns
/// `Some(EventAction::Remove)` (and logs) if `sender` is not allowed to bridge
/// to `destination_chain`, otherwise `None`.
pub(super) fn enforce_sender_allowlist(
    config: &config::Config,
    sender: &OmniAddress,
    destination_chain: ChainKind,
    context: &str,
) -> Option<EventAction> {
    if config.is_sender_allowed(sender, destination_chain) {
        return None;
    }

    warn!(
        "Sender {sender} is not allowed to bridge to {destination_chain:?}, dropping transfer {context}"
    );
    Some(EventAction::Remove)
}

/// Validates a transfer's sender before relaying: first the configured sender
/// allowlist (local), then KYT screening (network). Returns the `EventAction`
/// to take if the transfer must not be relayed, or `None` if it may proceed.
pub(super) async fn validate_sender(
    config: &config::Config,
    sender: &OmniAddress,
    destination_chain: ChainKind,
    context: &str,
) -> Option<EventAction> {
    if let Some(action) = enforce_sender_allowlist(config, sender, destination_chain, context) {
        return Some(action);
    }

    check_kyt(sender, context).await
}

pub async fn process_transfer_event(
    config: &config::Config,
    redis_connection_manager: &mut redis::aio::ConnectionManager,
    jsonrpc_client: &JsonRpcClient,
    omni_connector: Arc<OmniConnector>,
    signer: AccountId,
    transfer: Transfer,
    near_nonce: Arc<utils::nonce::NonceManager>,
) -> Result<(EventAction, Vec<WorkerEvent>)> {
    let (transfer_message, creation_timestamp) = match transfer {
        Transfer::Near {
            ref transfer_message,
            creation_timestamp,
        } => (transfer_message.clone(), creation_timestamp),
        Transfer::Utxo {
            ref new_transfer_id,
            ..
        } => {
            let Ok(new_transfer_id) = new_transfer_id.try_into() else {
                warn!("Failed to build TransferId from: {new_transfer_id:?}");
                return Ok((EventAction::Retry, Vec::new()));
            };

            let Ok(transfer_message) = omni_connector
                .near_get_transfer_message(new_transfer_id)
                .await
            else {
                warn!("Failed to get transfer message for UTXO transfer: {new_transfer_id:?}");
                return Ok((EventAction::Retry, Vec::new()));
            };

            (transfer_message, 0)
        }
        _ => {
            anyhow::bail!("Expected Transfer::Near or Transfer::Utxo variant, got: {transfer:?}");
        }
    };

    let origin_chain = transfer_message.get_origin_chain();
    let origin_nonce = transfer_message.origin_nonce;

    info!("Processing transfer ({origin_chain:?}:{origin_nonce}) on NEAR");

    let current_timestamp = chrono::Utc::now().timestamp();
    if current_timestamp < creation_timestamp + config.kyt.delay_secs {
        let remaining =
            (creation_timestamp + config.kyt.delay_secs - current_timestamp).unsigned_abs();
        return Ok((
            EventAction::RetryAfter(std::time::Duration::from_secs(remaining)),
            Vec::new(),
        ));
    }

    let context = format!("({origin_chain:?}:{origin_nonce})");

    let destination_chain = transfer_message.get_destination_chain();
    if let Some(action) = validate_sender(
        config,
        &transfer_message.sender,
        destination_chain,
        &context,
    )
    .await
    {
        return Ok((action, Vec::new()));
    }

    match omni_connector
        .is_transfer_finalised(
            Some(transfer_message.get_origin_chain()),
            transfer_message.get_destination_chain(),
            transfer_message.destination_nonce,
        )
        .await
    {
        Ok(true) => anyhow::bail!("Transfer is already finalised: {transfer_message:?}"),
        Ok(false) => {}
        Err(err) => {
            warn!("Failed to check if transfer is finalised: {err:?}");
            return Ok((EventAction::Retry, Vec::new()));
        }
    }

    if config.is_bridge_api_enabled()
        && !config
            .near
            .sign_without_checking_fee
            .as_ref()
            .is_some_and(|list| list.contains(&transfer_message.sender))
    {
        let Ok(needed_fee) = utils::bridge_api::TransferFee::get_transfer_fee(
            config,
            &transfer_message.sender,
            &transfer_message.recipient,
            &transfer_message.token,
        )
        .await
        else {
            warn!("Failed to get transfer fee for transfer: {transfer_message:?}");
            return Ok((EventAction::Retry, Vec::new()));
        };

        if let Some(event_action) = needed_fee
            .check_fee(
                config,
                redis_connection_manager,
                &transfer_message,
                transfer_message.get_transfer_id(),
                &transfer_message.fee,
            )
            .await
        {
            return Ok((event_action, Vec::new()));
        }
    }

    let nonce = near_nonce
        .reserve_nonce()
        .context("Failed to reserve nonce for near transaction")?;

    match omni_connector
        .near_sign_transfer(
            TransferId {
                origin_chain: transfer_message.sender.get_chain(),
                origin_nonce: transfer_message.origin_nonce,
            },
            Some(signer.clone()),
            Some(transfer_message.fee.clone()),
            TransactionOptions {
                nonce: Some(nonce),
                wait_until: near_primitives::views::TxExecutionStatus::Final,
                wait_final_outcome_timeout_sec: None,
            },
        )
        .await
    {
        Ok(tx_hash) => Ok(
            match utils::near::resolve_tx_receipts(
                jsonrpc_client,
                tx_hash,
                signer,
                &["Request has timed out."],
            )
            .await
            {
                Ok(receipts) => (
                    EventAction::Remove,
                    utils::near::extract_sign_transfer_event(&receipts),
                ),
                Err(action) => (action, Vec::new()),
            },
        ),
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
                            "Failed to sign transfer ({origin_chain:?}:{origin_nonce}), retrying: {near_rpc_error:?}"
                        );
                        return Ok((EventAction::Retry, Vec::new()));
                    }
                    _ => {
                        anyhow::bail!(
                            "Failed to sign transfer ({origin_chain:?}:{origin_nonce}): {near_rpc_error:?}"
                        );
                    }
                };
            }
            anyhow::bail!("Failed to sign transfer ({origin_chain:?}:{origin_nonce}): {err:?}");
        }
    }
}

pub async fn process_transfer_to_utxo_event(
    config: &config::Config,
    jsonrpc_client: &JsonRpcClient,
    omni_connector: Arc<OmniConnector>,
    transfer: Transfer,
    near_nonce: Arc<utils::nonce::NonceManager>,
) -> Result<(EventAction, Vec<WorkerEvent>)> {
    let Transfer::Near {
        ref transfer_message,
        creation_timestamp,
    } = transfer
    else {
        anyhow::bail!("Expected NearTransferWithTimestamp, got: {transfer:?}");
    };

    info!(
        "Processing NEAR to UTXO transfer ({:?}:{})",
        transfer_message.get_origin_chain(),
        transfer_message.origin_nonce
    );

    let current_timestamp = chrono::Utc::now().timestamp();
    if current_timestamp < creation_timestamp + config.kyt.delay_secs {
        let remaining =
            (creation_timestamp + config.kyt.delay_secs - current_timestamp).unsigned_abs();
        return Ok((
            EventAction::RetryAfter(std::time::Duration::from_secs(remaining)),
            Vec::new(),
        ));
    }

    let context = format!(
        "({:?}:{})",
        transfer_message.get_origin_chain(),
        transfer_message.origin_nonce
    );
    let destination_chain = transfer_message.recipient.get_chain();
    if let Some(action) = validate_sender(
        config,
        &transfer_message.sender,
        destination_chain,
        &context,
    )
    .await
    {
        return Ok((action, Vec::new()));
    }

    let OmniAddress::Near(ref sender) = transfer_message.sender else {
        anyhow::bail!(
            "Expected NEAR sender for NEAR to UTXO transfer, got: {:?}",
            transfer_message.sender
        );
    };

    let Some(recipient) = transfer_message.recipient.get_utxo_address() else {
        anyhow::bail!(
            "Expected UTXO recipient address, got: {:?}",
            transfer_message.recipient
        );
    };

    let fee_rate = if destination_chain == ChainKind::Btc && config.is_bridge_api_enabled() {
        match utils::bridge_api::get_btc_fee_rate(config).await {
            Ok(fee_rate) => Some(fee_rate),
            Err(err) => {
                warn!(
                    "Failed to fetch BTC fee rate from bridge indexer for {:?} transfer ({}): {err:?}, retrying",
                    destination_chain, transfer_message.origin_nonce
                );
                return Ok((EventAction::Retry, Vec::new()));
            }
        }
    } else {
        None
    };

    let nonce = near_nonce
        .reserve_nonce()
        .context("Failed to reserve nonce for near transaction")?;

    let utxo_cfg = match destination_chain {
        ChainKind::Btc => config.btc.as_ref(),
        ChainKind::Zcash => config.zcash.as_ref(),
        _ => None,
    };
    let max_gas_fee_percent = utxo_cfg.map_or(75u8, |u| u.max_gas_fee_percent);
    let change_reserve = utxo_cfg.map_or(5000u128, |u| u.change_reserve);

    let max_gas_fee = serde_json::from_str::<UTXOChainMsg>(&transfer_message.msg)
        .map(|msg| match msg {
            UTXOChainMsg::MaxGasFee(max_fee) => max_fee.0,
        })
        .ok();
    // Selector gets only `max_gas_fee_percent` of the user's cap so a
    // later RBF can bump within the same budget; the submit itself
    // enforces the user's real cap unchanged.
    let max_gas_fee_for_selector = max_gas_fee.map(|m| {
        let scaled = u128::from(m).saturating_mul(u128::from(max_gas_fee_percent)) / 100;
        u64::try_from(scaled).unwrap_or(m)
    });

    // The lock is held only across selection: pick UTXOs, then remove them
    // from the cache so concurrent submitters can't pick the same inputs.
    // The (slow) submit runs without the lock; on failure we put the
    // removed UTXOs back.
    let utxo_set = utils::utxo::UtxoSet::global();
    let mut utxos_guard = utxo_set.lock(&omni_connector, destination_chain).await;
    let utxos_snapshot = utxos_guard.as_ref().map(|g| (**g).clone());

    let mut removed: Vec<(String, utxo_utils::UTXO)> = Vec::new();

    let submit_result = match omni_connector
        .near_select_btc_utxos(
            destination_chain,
            recipient.clone(),
            transfer_message.amount.0 - transfer_message.fee.fee.0,
            fee_rate,
            max_gas_fee_for_selector,
            Some(change_reserve),
            None,
            utxos_snapshot,
        )
        .await
    {
        Ok(selection) => {
            removed = utils::utxo::UtxoSet::take_outpoints(utxos_guard.as_mut(), &selection);
            drop(utxos_guard);

            omni_connector
                .near_submit_prepared_btc_transfer(
                    recipient,
                    TransferId {
                        origin_chain: transfer_message.sender.get_chain(),
                        origin_nonce: transfer_message.origin_nonce,
                    },
                    TransactionOptions {
                        nonce: Some(nonce),
                        wait_until: near_primitives::views::TxExecutionStatus::Final,
                        wait_final_outcome_timeout_sec: None,
                    },
                    max_gas_fee,
                    selection,
                )
                .await
        }
        Err(err) => {
            drop(utxos_guard);
            Err(err)
        }
    };

    if submit_result.is_err() {
        utxo_set.restore_outpoints(destination_chain, removed).await;
    }

    match submit_result {
        Ok(tx_hash) => {
            info!(
                "Submitted NEAR->{destination_chain:?} transfer on NEAR ({:?}:{}): near_submit_tx_hash={tx_hash:?}",
                transfer_message.get_origin_chain(),
                transfer_message.origin_nonce
            );

            let signer = omni_connector
                .near_bridge_client()
                .and_then(near_bridge_client::NearBridgeClient::account_id)?;

            Ok(
                match utils::near::resolve_tx_receipts(
                    jsonrpc_client,
                    tx_hash,
                    signer,
                    &[
                        "not exist",
                        "Previous btc tx has not been signed",
                        "Too many pending sign transactions",
                    ],
                )
                .await
                {
                    Ok(receipts) => {
                        let events =
                            utils::near::extract_near_to_utxo(&receipts, destination_chain, sender);
                        if let Some(WorkerEvent::NearToUtxo(transfer)) = events.first()
                            && let Transfer::NearToUtxo { btc_pending_id, .. } = transfer.as_ref()
                        {
                            info!(
                                "Extracted btc_pending_id={btc_pending_id} for NEAR->{destination_chain:?} transfer ({:?}:{}): utxo_inputs={}",
                                transfer_message.get_origin_chain(),
                                transfer_message.origin_nonce,
                                events.len()
                            );
                        }
                        (EventAction::Remove, events)
                    }
                    Err(action) => (action, Vec::new()),
                },
            )
        }
        Err(err) => {
            if let BridgeSdkError::NearRpcError(near_rpc_error) = err {
                utxo_set.mark_dirty(destination_chain);

                match near_rpc_error {
                    NearRpcError::NonceError
                    | NearRpcError::FinalizationError
                    | NearRpcError::RpcBroadcastTxAsyncError(_)
                    | NearRpcError::RpcQueryError(
                        JsonRpcError::TransportError(_) | JsonRpcError::ServerError(_),
                    )
                    | NearRpcError::RpcTransactionError(_) => {
                        warn!(
                            "Failed to submit {:?} transfer ({}), retrying: {near_rpc_error:?}",
                            transfer_message.recipient.get_chain(),
                            transfer_message.origin_nonce
                        );
                        return Ok((EventAction::Retry, Vec::new()));
                    }
                    _ => {
                        anyhow::bail!(
                            "Failed to submit {:?} transfer ({}): {near_rpc_error:?}",
                            transfer_message.recipient.get_chain(),
                            transfer_message.origin_nonce
                        );
                    }
                };
            } else if let BridgeSdkError::InsufficientUTXOBalance = err {
                warn!(
                    "Insufficient UTXO balance for {:?} transfer ({}), retrying",
                    transfer_message.recipient.get_chain(),
                    transfer_message.origin_nonce
                );
                return Ok((EventAction::Retry, Vec::new()));
            } else if let BridgeSdkError::InsufficientUTXOGasFee(err) = err {
                warn!(
                    "Gas fee is too large for {:?} transfer ({}): {err}, retrying",
                    transfer_message.recipient.get_chain(),
                    transfer_message.origin_nonce
                );
                return Ok((EventAction::Retry, Vec::new()));
            } else if let BridgeSdkError::UtxoClientError(ref msg) = err
                && msg == "Failed to estimate fee_rate"
            {
                warn!(
                    "Failed to estimate fee_rate for {:?} transfer ({}), retrying",
                    transfer_message.recipient.get_chain(),
                    transfer_message.origin_nonce
                );
                return Ok((EventAction::Retry, Vec::new()));
            }

            anyhow::bail!(
                "Failed to submit {:?} transfer ({}): {err:?}",
                transfer_message.recipient.get_chain(),
                transfer_message.origin_nonce
            );
        }
    }
}

pub async fn process_sign_transfer_event(
    config: &config::Config,
    redis_connection_manager: &mut redis::aio::ConnectionManager,
    omni_connector: Arc<OmniConnector>,
    signer: AccountId,
    omni_bridge_event: OmniBridgeEvent,
    evm_nonces: Arc<utils::nonce::EvmNonceManagers>,
) -> Result<EventAction> {
    let OmniBridgeEvent::SignTransferEvent {
        message_payload, ..
    } = &omni_bridge_event
    else {
        anyhow::bail!("Expected SignTransferEvent, got: {omni_bridge_event:?}");
    };

    info!(
        "Processing SignTransferEvent ({:?}:{})",
        message_payload.transfer_id.origin_chain, message_payload.transfer_id.origin_nonce
    );

    if message_payload.fee_recipient != Some(signer) {
        anyhow::bail!("Fee recipient mismatch");
    }

    match omni_connector
        .is_transfer_finalised(
            None,
            message_payload.recipient.get_chain(),
            message_payload.destination_nonce,
        )
        .await
    {
        Ok(true) => anyhow::bail!(
            "Transfer is already finalised: {:?}",
            message_payload.transfer_id
        ),
        Ok(false) => {}
        Err(err) => {
            warn!("Failed to check if transfer is finalised: {err:?}");
            return Ok(EventAction::Retry);
        }
    }

    // The sender allowlist is enforced on the finalization path too, not just at
    // signing. The allowlist needs the transfer's sender, which is resolved from
    // the transfer message (the sign payload does not carry it).
    let destination_chain = message_payload.recipient.get_chain();
    let allowlist_active = config.is_destination_restricted(destination_chain);

    if config.is_bridge_api_enabled() || allowlist_active {
        let transfer_message = match omni_connector
            .near_get_transfer_message(message_payload.transfer_id)
            .await
        {
            Ok(transfer_message) => transfer_message,
            Err(err) => {
                if err.to_string().contains("The transfer does not exist") {
                    anyhow::bail!(
                        "Transfer does not exist: {:?} (probably fee is 0 or transfer was already finalized)",
                        message_payload.transfer_id
                    );
                }

                warn!(
                    "Failed to get transfer message: {:?}",
                    message_payload.transfer_id
                );

                return Ok(EventAction::Retry);
            }
        };

        if let Some(action) = enforce_sender_allowlist(
            config,
            &transfer_message.sender,
            destination_chain,
            &format!(
                "({:?}:{})",
                message_payload.transfer_id.origin_chain, message_payload.transfer_id.origin_nonce
            ),
        ) {
            return Ok(action);
        }

        if config.is_bridge_api_enabled() {
            let Ok(needed_fee) = utils::bridge_api::TransferFee::get_transfer_fee(
                config,
                &transfer_message.sender,
                &transfer_message.recipient,
                &transfer_message.token,
            )
            .await
            else {
                warn!("Failed to get transfer fee for transfer: {transfer_message:?}");
                return Ok(EventAction::Retry);
            };

            if let Some(event_action) = needed_fee
                .check_fee(
                    config,
                    redis_connection_manager,
                    &transfer_message,
                    transfer_message.get_transfer_id(),
                    &transfer_message.fee,
                )
                .await
            {
                return Ok(event_action);
            }
        }
    }

    let chain_kind = message_payload.recipient.get_chain();

    let (fin_transfer_args, evm_nonce) = match chain_kind {
        ChainKind::Near => {
            anyhow::bail!("Near to Near transfers are not supported");
        }
        ChainKind::Eth
        | ChainKind::Base
        | ChainKind::Arb
        | ChainKind::Bnb
        | ChainKind::Pol
        | ChainKind::HyperEvm
        | ChainKind::Abs => {
            let nonce = evm_nonces
                .reserve_nonce(chain_kind)
                .context("Failed to reserve nonce for evm transaction")?;

            (
                omni_connector::FinTransferArgs::EvmFinTransfer {
                    chain_kind,
                    event: omni_bridge_event.clone(),
                    tx_nonce: Some(alloy::primitives::U256::from(nonce)),
                },
                Some(nonce),
            )
        }
        ChainKind::Sol | ChainKind::Fogo => {
            let (OmniAddress::Sol(token) | OmniAddress::Fogo(token)) =
                message_payload.token_address.clone()
            else {
                anyhow::bail!(
                    "Expected SVM token address, got: {:?}",
                    message_payload.token_address
                );
            };

            (
                omni_connector::FinTransferArgs::SvmFinTransfer {
                    chain_kind,
                    event: omni_bridge_event.clone(),
                    svm_token: Pubkey::new_from_array(token.0),
                },
                None,
            )
        }
        ChainKind::Strk => (
            omni_connector::FinTransferArgs::StarknetFinTransfer {
                event: omni_bridge_event.clone(),
            },
            None,
        ),
        ChainKind::Btc | ChainKind::Zcash => {
            anyhow::bail!("Finishing BTC/ZEC transfers is not supported");
        }
    };

    match omni_connector.fin_transfer(fin_transfer_args).await {
        Ok(tx_hash) => {
            info!(
                "Finalized deposit ({:?}:{}): {tx_hash}",
                message_payload.transfer_id.origin_chain, message_payload.transfer_id.origin_nonce
            );

            if let Some(nonce) = evm_nonce
                && config.is_fee_bumping_enabled(chain_kind)
                && let Err(err) = store_pending_transaction(
                    config,
                    redis_connection_manager,
                    chain_kind,
                    &tx_hash,
                    nonce,
                    omni_bridge_event,
                )
                .await
            {
                warn!("Failed to store pending transaction {tx_hash}: {err:?}");
            }

            Ok(EventAction::Remove)
        }
        Err(err) => {
            if let BridgeSdkError::EvmGasEstimateError(err) = err {
                let Some(evm) = (match chain_kind {
                    ChainKind::Eth => &config.eth,
                    ChainKind::Base => &config.base,
                    ChainKind::Arb => &config.arb,
                    ChainKind::Bnb => &config.bnb,
                    ChainKind::Pol => &config.pol,
                    ChainKind::HyperEvm => &config.hyperevm,
                    ChainKind::Abs => &config.abs,
                    ChainKind::Near
                    | ChainKind::Sol
                    | ChainKind::Fogo
                    | ChainKind::Strk
                    | ChainKind::Btc
                    | ChainKind::Zcash => {
                        anyhow::bail!(
                            "Failed to finalize deposit (unexpected: failed to get evm config): {err}"
                        );
                    }
                }) else {
                    anyhow::bail!(
                        "Failed to finalize deposit (unexpected: config for {chain_kind:?} is not accessible): {err}"
                    );
                };

                if evm
                    .error_selectors_to_remove
                    .iter()
                    .any(|selector| err.contains(selector))
                {
                    anyhow::bail!(
                        "Failed to finalize deposit: {err}. Found selector from the list of non-retryable errors in the config"
                    );
                }

                warn!("Failed to finalize deposit, retrying: {err}");
                return Ok(EventAction::Retry);
            }

            if let BridgeSdkError::SolanaRpcError(ref client_error) = err
                && let ErrorKind::RpcError(RpcError::RpcResponseError {
                    data: RpcResponseErrorData::SendTransactionPreflightFailure(ref result),
                    ..
                }) = client_error.kind
                && let Some(TransactionError::InstructionError(
                    _,
                    InstructionError::Custom(error_code),
                )) = result.err
            {
                if error_code == PAUSED_ERROR {
                    warn!("Solana bridge is paused");
                    return Ok(EventAction::Retry);
                }

                anyhow::bail!("Failed to finalize deposit: {err}");
            }

            warn!("Failed to finalize deposit, retrying: {err}");
            Ok(EventAction::Retry)
        }
    }
}

pub async fn initiate_fast_transfer(
    config: &config::Config,
    fast_connector: Arc<OmniConnector>,
    transfer: Transfer,
    near_fast_nonce: Arc<utils::nonce::NonceManager>,
) -> Result<EventAction> {
    let Ok(near_bridge_client) = fast_connector.near_bridge_client() else {
        anyhow::bail!("Near bridge client is not configured");
    };

    let Transfer::Fast {
        block_number,
        tx_hash,
        token,
        amount,
        transfer_id,
        sender,
        recipient,
        fee,
        msg,
        storage_deposit_amount,
        safe_confirmations,
    } = transfer.clone()
    else {
        anyhow::bail!("Expected FastTransferEvent, got: {transfer:?}");
    };

    // TODO: Fast transfer to other chain increases origin nonce by one, so regular relayer won't
    // be able to finalize it with a normal sign transfer. We need to catch and sign
    // `FastTransferEvent`. This will be possible once bridge-indexer will track these events
    // Related PR: https://github.com/Near-One/bridge-indexer-rs/pull/195
    if recipient.get_chain() != ChainKind::Near {
        anyhow::bail!(
            "Fast transfer is supported only for transfers to NEAR for now, got: {:?}",
            recipient.get_chain()
        );
    }

    let context = format!(
        "({:?}:{})",
        transfer_id.origin_chain, transfer_id.origin_nonce
    );
    if let Some(action) = enforce_sender_allowlist(config, &sender, ChainKind::Near, &context) {
        return Ok(action);
    }

    info!("Trying to initiate FastTransfer on NEAR");

    let Ok(token_id) =
        utils::storage::get_token_id(&fast_connector, transfer_id.origin_chain, &token).await
    else {
        warn!("Failed to get token id for transfer: {transfer_id:?}");
        return Ok(EventAction::Retry);
    };

    let fast_transfer = FastTransfer {
        transfer_id: transfer_id.into(),
        token_id: token_id.clone(),
        amount,
        fee: fee.clone(),
        recipient: recipient.clone(),
        msg: msg.clone(),
    };

    match fast_connector.near_is_transfer_finalised(transfer_id).await {
        Ok(true) => anyhow::bail!("Transfer is already finalised: {transfer:?}"),
        Ok(false) => {}
        Err(err) => {
            warn!("Failed to check if transfer is finalised: {err:?}");
            return Ok(EventAction::Retry);
        }
    }

    match fast_connector
        .near_get_fast_transfer_status(fast_transfer.id())
        .await
    {
        Ok(Some(_)) => anyhow::bail!("Fast transfer is already finalised: {transfer:?}"),
        Ok(None) => {}
        Err(err) => {
            warn!("Failed to check if fast transfer is finalised: {err:?}");
            return Ok(EventAction::Retry);
        }
    }

    let Ok(last_finalized_block_number) = fast_connector
        .evm_get_last_block_number(transfer_id.origin_chain)
        .await
    else {
        warn!("Failed to get last finalized block number for EVM chain");
        return Ok(EventAction::Retry);
    };

    let current_confirmations = last_finalized_block_number.saturating_sub(block_number);

    if current_confirmations < safe_confirmations {
        warn!(
            "Fast transfer block number ({block_number}) is not finalized yet, waiting for more confirmations. Current confirmations: {current_confirmations}",
        );
        return Ok(EventAction::Retry);
    }

    let mut nonce = Some(
        near_fast_nonce
            .reserve_nonce()
            .context("Failed to reserve nonce for near transaction")?,
    );

    let Ok(required_balance) = near_bridge_client
        .get_required_balance_for_fast_fin_transfer()
        .await
    else {
        warn!("Failed to get required balance for fast transfer");
        return Ok(EventAction::Retry);
    };

    match near_bridge_client
        .deposit_storage_if_required(
            required_balance + storage_deposit_amount.unwrap_or(0.into()).0,
            TransactionOptions {
                nonce,
                wait_until: near_primitives::views::TxExecutionStatus::Final,
                wait_final_outcome_timeout_sec: None,
            },
        )
        .await
    {
        Ok(true) => {
            nonce = Some(
                near_fast_nonce
                    .reserve_nonce()
                    .context("Failed to reserve nonce for near transaction")?,
            );
        }
        Ok(false) => {}
        Err(err) => {
            warn!("Failed to deposit storage for fast transfer: {err:?}");
            return Ok(EventAction::Retry);
        }
    }

    match fast_connector
        .near_fast_transfer(
            transfer_id.origin_chain,
            tx_hash,
            storage_deposit_amount.map(|storage_deposit_amount| storage_deposit_amount.0),
            TransactionOptions {
                nonce,
                wait_until: near_primitives::views::TxExecutionStatus::Included,
                wait_final_outcome_timeout_sec: None,
            },
        )
        .await
    {
        Ok(tx_hash) => {
            info!("Fast transfer initiated successfully: {tx_hash:?}");
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
                        warn!("Failed to initiate fast transfer, retrying: {near_rpc_error:?}");
                        return Ok(EventAction::Retry);
                    }
                    _ => {
                        anyhow::bail!("Failed to initiate fast transfer: {near_rpc_error:?}");
                    }
                };
            }
            anyhow::bail!("Failed to initiate fast transfer: {err:?}");
        }
    }
}

async fn store_pending_transaction(
    config: &config::Config,
    redis_connection_manager: &mut redis::aio::ConnectionManager,
    chain_kind: ChainKind,
    tx_hash: &str,
    nonce: u64,
    omni_bridge_event: OmniBridgeEvent,
) -> Result<()> {
    let pending_tx =
        PendingTransaction::new(tx_hash.to_string(), nonce, chain_kind, omni_bridge_event);

    utils::redis::zadd(
        config,
        redis_connection_manager,
        &utils::pending_transactions::get_pending_tx_key(chain_kind),
        nonce,
        pending_tx,
    )
    .await;

    info!("Stored pending transaction {tx_hash} (nonce: {nonce}) for {chain_kind:?}");

    Ok(())
}
