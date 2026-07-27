use std::sync::Arc;

use alloy::transports::RpcError as AlloyRpcError;
use anyhow::{Context, Result};
use bridge_connector_common::result::{BridgeSdkError, EthRpcError};
use near_sdk::json_types::U64;
use tracing::{info, warn};

use near_bridge_client::TransactionOptions;
use near_jsonrpc_client::JsonRpcClient;
use near_primitives::types::AccountId;
use solana_rpc_client_api::{
    client_error::ErrorKind,
    request::{RpcError, RpcResponseErrorData},
};
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
                warn!("Failed to build TransferId from {new_transfer_id:?}, removing");
                return Ok((EventAction::Remove, Vec::new()));
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
            warn!("Routing mismatch, removing: {transfer:?}");
            return Ok((EventAction::Remove, Vec::new()));
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
    if let Some(action) = utils::validation::validate_sender(
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
        Ok(true) => {
            warn!("Transfer is already finalised, removing: {transfer_message:?}");
            return Ok((EventAction::Remove, Vec::new()));
        }
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
        warn!("Routing mismatch, removing: {transfer:?}");
        return Ok((EventAction::Remove, Vec::new()));
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
    let destination_chain = transfer_message.get_destination_chain();
    if let Some(action) = utils::validation::validate_sender(
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
        warn!(
            "Expected NEAR sender for NEAR to UTXO transfer, got: {:?}, removing",
            transfer_message.sender
        );
        return Ok((EventAction::Remove, Vec::new()));
    };

    let Some(recipient) = transfer_message.recipient.get_utxo_address() else {
        warn!(
            "Expected UTXO recipient address, got: {:?}, removing",
            transfer_message.recipient
        );
        return Ok((EventAction::Remove, Vec::new()));
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
                        "Invalid expiry height",
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
            if matches!(err, BridgeSdkError::NearRpcError(_)) {
                utxo_set.mark_dirty(destination_chain);
            }

            if let BridgeSdkError::InsufficientUTXOBalance = err {
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
            } else if let BridgeSdkError::UtxoRpcError(ref msg) = err
                && msg.starts_with("Failed to estimate fee_rate")
            {
                warn!(
                    "Failed to estimate fee_rate for {:?} transfer ({}), retrying",
                    transfer_message.recipient.get_chain(),
                    transfer_message.origin_nonce
                );
                return Ok((EventAction::Retry, Vec::new()));
            }

            if let BridgeSdkError::InvalidArgument(ref msg) = err
                && msg == "Amount is smaller than `fee`"
            {
                warn!(
                    "Amount below withdraw_fee for {:?} transfer ({}), removing",
                    transfer_message.recipient.get_chain(),
                    transfer_message.origin_nonce
                );
                return Ok((EventAction::Remove, Vec::new()));
            }

            anyhow::bail!(
                "Failed to submit {:?} transfer ({}): {err:?}",
                transfer_message.recipient.get_chain(),
                transfer_message.origin_nonce
            );
        }
    }
}

#[allow(clippy::too_many_lines)]
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
        warn!("Routing mismatch, removing: {omni_bridge_event:?}");
        return Ok(EventAction::Remove);
    };

    info!(
        "Processing SignTransferEvent ({:?}:{})",
        message_payload.transfer_id.origin_chain, message_payload.transfer_id.origin_nonce
    );

    if message_payload.fee_recipient != Some(signer) {
        warn!("Fee recipient mismatch, removing: {omni_bridge_event:?}");
        return Ok(EventAction::Remove);
    }

    match omni_connector
        .is_transfer_finalised(
            None,
            message_payload.recipient.get_chain(),
            message_payload.destination_nonce,
        )
        .await
    {
        Ok(true) => {
            warn!(
                "Transfer is already finalised, removing: {:?}",
                message_payload.transfer_id
            );
            return Ok(EventAction::Remove);
        }
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
                let err_str = err.to_string();
                if err_str.contains("The transfer does not exist")
                    || err_str.contains(&omni_types::errors::BridgeError::TransferNotExist.as_ref())
                {
                    warn!(
                        "Transfer does not exist (fee=0 or already finalized), removing: {:?}",
                        message_payload.transfer_id
                    );
                    return Ok(EventAction::Remove);
                }

                warn!(
                    "Failed to get transfer message: {:?}",
                    message_payload.transfer_id
                );

                return Ok(EventAction::Retry);
            }
        };

        if let Some(action) = utils::validation::enforce_sender_allowlist(
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
            warn!("Near-to-Near transfer not supported, removing: {omni_bridge_event:?}");
            return Ok(EventAction::Remove);
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
                warn!(
                    "SVM token address mismatch, removing: {:?}",
                    message_payload.token_address
                );
                return Ok(EventAction::Remove);
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
        ChainKind::Aptos => (
            omni_connector::FinTransferArgs::AptosFinTransfer {
                event: omni_bridge_event.clone(),
            },
            None,
        ),
        ChainKind::Btc | ChainKind::Zcash => {
            warn!("BTC/ZEC not supported for fast transfer, removing: {omni_bridge_event:?}");
            return Ok(EventAction::Remove);
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
                    | ChainKind::Aptos
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
                    warn!(
                        "Failed to finalize deposit (non-retryable selector matched), removing: {err}"
                    );
                    return Ok(EventAction::Remove);
                }

                warn!("Failed to finalize deposit, retrying: {err}");
                return Ok(EventAction::Retry);
            }

            if let BridgeSdkError::EthRpcError(ref eth_err) = err {
                match eth_err {
                    EthRpcError::ContractError(reason) => {
                        warn!("EVM contract/ABI error (non-retryable), removing: {reason}");
                        return Ok(EventAction::Remove);
                    }
                    EthRpcError::RpcError(AlloyRpcError::ErrorResp(payload))
                        if !payload.is_retry_err()
                            && (payload.code == 3
                                || payload.message.contains("execution reverted")) =>
                    {
                        warn!(
                            "EVM execution reverted at submission (non-retryable), removing: code={} msg={}",
                            payload.code, payload.message
                        );
                        return Ok(EventAction::Remove);
                    }
                    _ => {
                        warn!("EVM fin_transfer transient error, retrying: {err}");
                        return Ok(EventAction::Retry);
                    }
                }
            }

            if let BridgeSdkError::SolanaRpcError(ref client_error) = err
                && let ErrorKind::RpcError(RpcError::RpcResponseError {
                    data: RpcResponseErrorData::SendTransactionPreflightFailure(result),
                    ..
                }) = &*client_error.kind
            {
                match result.err.clone().map(TransactionError::from) {
                    // Program-level (Anchor / SPL / System-CPI) custom errors show
                    // up at preflight but reflect MUTABLE on-chain state — fee-payer
                    // or vault funding (System 0x1 = insufficient lamports), account
                    // / sysvar state (Anchor 3015 = AccountSysvarMismatch), the
                    // paused flag, or CPI races — which can clear on a later attempt.
                    // Retry rather than drop a recoverable transfer.
                    Some(TransactionError::InstructionError(
                        _,
                        InstructionError::Custom(error_code),
                    )) => {
                        if error_code == PAUSED_ERROR {
                            warn!("Solana bridge is paused, retrying");
                        } else {
                            warn!(
                                "Solana program custom error at preflight (retryable): {result:?}"
                            );
                        }
                        return Ok(EventAction::Retry);
                    }
                    Some(
                        TransactionError::BlockhashNotFound
                        | TransactionError::AccountInUse
                        | TransactionError::WouldExceedMaxBlockCostLimit
                        | TransactionError::WouldExceedMaxAccountCostLimit
                        | TransactionError::WouldExceedAccountDataBlockLimit
                        | TransactionError::WouldExceedMaxVoteCostLimit
                        | TransactionError::WouldExceedAccountDataTotalLimit
                        | TransactionError::MaxLoadedAccountsDataSizeExceeded
                        | TransactionError::ClusterMaintenance
                        | TransactionError::ProgramCacheHitMaxLimit
                        | TransactionError::ProgramExecutionTemporarilyRestricted { .. }
                        | TransactionError::AccountBorrowOutstanding
                        | TransactionError::CommitCancelled
                        | TransactionError::ResanitizationNeeded,
                    ) => {
                        warn!("Solana preflight transient failure, retrying: {result:?}");
                        return Ok(EventAction::Retry);
                    }
                    Some(
                        TransactionError::InsufficientFundsForFee
                        | TransactionError::InsufficientFundsForRent { .. }
                        | TransactionError::AccountNotFound,
                    ) => {
                        warn!("Solana fee-payer funding issue, retrying: {result:?}");
                        return Ok(EventAction::Retry);
                    }
                    None => {
                        warn!(
                            "Solana preflight failure without structured error, retrying: {result:?}"
                        );
                        return Ok(EventAction::Retry);
                    }
                    Some(_) => {
                        warn!("Solana preflight deterministic failure, removing: {result:?}");
                        return Ok(EventAction::Remove);
                    }
                }
            }

            if let BridgeSdkError::StarknetOtherError(ref reason) = err
                && reason.contains("Transaction reverted:")
            {
                warn!("Starknet fin_transfer reverted (non-retryable), removing: {reason}");
                return Ok(EventAction::Remove);
            }

            if let BridgeSdkError::InvalidArgument(ref reason) = err {
                warn!("Non-retryable invalid argument, removing: {reason}");
                return Ok(EventAction::Remove);
            }

            warn!("Failed to finalize deposit, retrying: {err}");
            Ok(EventAction::Retry)
        }
    }
}

pub async fn initiate_fast_transfer(
    config: &config::Config,
    jsonrpc_client: &JsonRpcClient,
    fast_connector: Arc<OmniConnector>,
    transfer: Transfer,
    near_fast_nonce: Arc<utils::nonce::NonceManager>,
) -> Result<EventAction> {
    let Ok(near_bridge_client) = fast_connector.near_bridge_client() else {
        anyhow::bail!("Near bridge client is not configured");
    };

    let Ok(fast_signer) = fast_connector
        .near_bridge_client()
        .and_then(near_bridge_client::NearBridgeClient::account_id)
    else {
        warn!("Failed to get fast relayer account id, retrying");
        return Ok(EventAction::Retry);
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
        warn!("Routing mismatch, removing: {transfer:?}");
        return Ok(EventAction::Remove);
    };

    // TODO: Fast transfer to other chain increases origin nonce by one, so regular relayer won't
    // be able to finalize it with a normal sign transfer. We need to catch and sign
    // `FastTransferEvent`. This will be possible once bridge-indexer will track these events
    // Related PR: https://github.com/Near-One/bridge-indexer-rs/pull/195
    if recipient.get_chain() != ChainKind::Near {
        warn!(
            "Fast transfer to non-NEAR chain not supported, removing: {:?}",
            recipient.get_chain()
        );
        return Ok(EventAction::Remove);
    }

    let context = format!(
        "({:?}:{})",
        transfer_id.origin_chain, transfer_id.origin_nonce
    );
    if let Some(action) =
        utils::validation::enforce_sender_allowlist(config, &sender, ChainKind::Near, &context)
    {
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
        Ok(true) => {
            warn!("Transfer is already finalised, removing: {transfer:?}");
            return Ok(EventAction::Remove);
        }
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
        Ok(Some(_)) => {
            warn!("Fast transfer is already finalised, removing: {transfer:?}");
            return Ok(EventAction::Remove);
        }
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
            info!("Fast transfer initiated successfully: {tx_hash}");
            Ok(utils::near::resolve_tx_action(
                jsonrpc_client,
                tx_hash,
                fast_signer,
                &["Request has timed out."],
            )
            .await)
        }
        Err(err) => {
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
