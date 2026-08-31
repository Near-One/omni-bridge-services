use std::{sync::Arc, time::Duration};

use anyhow::{Context, Result};
use bridge_connector_common::result::BridgeSdkError;
use near_sdk::AccountId;
use tracing::{info, warn};

use near_bridge_client::{NearBridgeClient, TransactionOptions};
use near_jsonrpc_client::{
    JsonRpcClient,
    errors::{JsonRpcError, JsonRpcServerError},
    methods::query::RpcQueryError,
};
use near_primitives::views::TxExecutionStatus;
use near_rpc_client::NearRpcError;

use omni_connector::OmniConnector;
use omni_types::{ChainKind, Fee, TransferId};

use crate::{config, utils};

use super::{DeployToken, EventAction, FinTransfer, Transfer};

pub async fn process_init_transfer_event(
    config: &config::Config,
    redis_connection_manager: &mut redis::aio::ConnectionManager,
    jsonrpc_client: &JsonRpcClient,
    omni_connector: Arc<OmniConnector>,
    signer: AccountId,
    transfer: Transfer,
    near_nonce: Arc<utils::nonce::NonceManager>,
) -> Result<EventAction> {
    let Transfer::Starknet {
        ref tx_hash,
        ref sender,
        ref recipient,
        origin_nonce,
        ref token,
        amount,
        ref fee,
        creation_timestamp,
        ..
    } = transfer
    else {
        warn!("Routing mismatch, dropping: {transfer:?}");
        return Ok(EventAction::Drop);
    };

    let transfer_id = TransferId {
        origin_chain: ChainKind::Strk,
        origin_nonce,
    };

    let current_timestamp = chrono::Utc::now().timestamp();
    let effective_wait = config.kyt.delay_secs;
    if current_timestamp < creation_timestamp + effective_wait {
        let remaining = (creation_timestamp + effective_wait - current_timestamp).unsigned_abs();
        return Ok(EventAction::RetryAfter(Duration::from_secs(remaining)));
    }

    info!(
        "Processing Starknet InitTransfer ({:?}:{}): {tx_hash}",
        transfer_id.origin_chain, transfer_id.origin_nonce
    );

    let context = format!(
        "({:?}:{})",
        transfer_id.origin_chain, transfer_id.origin_nonce
    );
    if let Some(action) =
        utils::validation::validate_sender(config, sender, ChainKind::Near, &context).await
    {
        return Ok(action);
    }

    match omni_connector
        .is_transfer_finalised(Some(ChainKind::Strk), ChainKind::Near, origin_nonce)
        .await
    {
        Ok(true) => {
            warn!("Transfer is already finalised, dropping: {transfer_id:?}");
            return Ok(EventAction::Drop);
        }
        Ok(false) => {}
        Err(err) => {
            warn!("Failed to check if transfer is finalised: {err:?}");
            return Ok(EventAction::Retry);
        }
    }

    if config::Config::is_shield_enabled() {
        let Ok(token_id) = utils::storage::get_token_id(
            &omni_connector,
            transfer_id.origin_chain,
            &token.to_string(),
        )
        .await
        else {
            warn!("Failed to get token id for transfer: {transfer_id:?}");
            return Ok(EventAction::Retry);
        };

        if let Some(action) = utils::validation::check_shield_deposit(
            transfer_id.origin_chain,
            &token_id,
            amount.0,
            sender,
            &context,
        )
        .await
        {
            return Ok(action);
        }
    }

    if config.is_bridge_api_enabled() {
        let Ok(needed_fee) =
            utils::bridge_api::TransferFee::get_transfer_fee(config, sender, recipient, token)
                .await
        else {
            warn!("Failed to get transfer fee for transfer: {transfer:?}");
            return Ok(EventAction::Retry);
        };

        let provided_fee = Fee {
            fee: fee.fee,
            native_fee: fee.native_fee,
        };

        if let Some(event_action) = needed_fee
            .check_fee(
                config,
                redis_connection_manager,
                &transfer,
                transfer_id,
                &provided_fee,
            )
            .await
        {
            return Ok(event_action);
        }
    }

    let fee_recipient = omni_connector
        .near_bridge_client()
        .and_then(NearBridgeClient::account_id)
        .context("Failed to get relayer account id")?;

    let storage_deposit_actions = match utils::storage::get_storage_deposit_actions(
        &omni_connector,
        ChainKind::Strk,
        recipient,
        &fee_recipient,
        &token.to_string(),
        fee.fee.0,
        fee.native_fee.0,
    )
    .await
    {
        Ok(actions) => actions,
        Err(err) => {
            warn!("Failed to get storage deposit actions: {err}");
            return Ok(EventAction::Retry);
        }
    };

    let nonce = near_nonce
        .reserve_nonce()
        .context("Failed to reserve nonce for near transaction")?;

    let fin_transfer_args = omni_connector::FinTransferArgs::NearFinTransferWithMpcProof {
        chain_kind: ChainKind::Strk,
        destination_chain: recipient.get_chain(),
        storage_deposit_actions,
        tx_hash: tx_hash.clone(),
        transaction_options: TransactionOptions {
            nonce: Some(nonce),
            wait_until: TxExecutionStatus::Included,
            wait_final_outcome_timeout_sec: None,
        },
    };

    match omni_connector.fin_transfer(fin_transfer_args).await {
        Ok(tx_hash) => {
            let Ok(crypto_hash) = tx_hash.parse() else {
                warn!("Failed to parse {tx_hash} as CryptoHash");
                return Ok(EventAction::Remove);
            };

            Ok(utils::near::resolve_tx_action(
                jsonrpc_client,
                crypto_hash,
                signer,
                &["Request has timed out."],
            )
            .await)
        }
        Err(err) => Err(err).with_context(|| {
            format!(
                "Failed to finalize Starknet transfer ({:?}:{})",
                transfer_id.origin_chain, transfer_id.origin_nonce
            )
        }),
    }
}

pub async fn process_fin_transfer_event(
    jsonrpc_client: &JsonRpcClient,
    omni_connector: Arc<OmniConnector>,
    signer: AccountId,
    fin_transfer: FinTransfer,
    near_nonce: Arc<utils::nonce::NonceManager>,
) -> Result<EventAction> {
    let FinTransfer::Starknet {
        tx_hash,
        transfer_id,
    } = fin_transfer
    else {
        warn!("Routing mismatch, dropping: {fin_transfer:?}");
        return Ok(EventAction::Drop);
    };

    info!(
        "Processing Starknet FinTransfer ({:?}): {tx_hash}",
        ChainKind::Strk
    );

    if let Err(BridgeSdkError::NearRpcError(NearRpcError::RpcQueryError(
        JsonRpcError::ServerError(JsonRpcServerError::HandlerError(
            RpcQueryError::ContractExecutionError { vm_error, .. },
        )),
    ))) = omni_connector.near_get_transfer_message(transfer_id).await
        && (vm_error.contains("The transfer does not exist")
            || vm_error.contains(&omni_types::errors::BridgeError::TransferNotExist.as_ref()))
    {
        info!("No fee to claim for Starknet FinTransfer ({transfer_id:?})");
        return Ok(EventAction::Remove);
    }

    let nonce = near_nonce
        .reserve_nonce()
        .context("Failed to reserve nonce for near transaction")?;

    let claim_fee_args = omni_connector::ClaimFeeArgs::ClaimFeeWithMpcProofTx {
        chain_kind: ChainKind::Strk,
        tx_hash: tx_hash.clone(),
        transaction_options: TransactionOptions {
            nonce: Some(nonce),
            wait_until: TxExecutionStatus::Included,
            wait_final_outcome_timeout_sec: None,
        },
    };

    match omni_connector.claim_fee(claim_fee_args).await {
        Ok(tx_hash) => {
            let Ok(crypto_hash) = tx_hash.parse() else {
                warn!("Failed to parse {tx_hash} as CryptoHash");
                return Ok(EventAction::Remove);
            };

            Ok(utils::near::resolve_tx_action(
                jsonrpc_client,
                crypto_hash,
                signer,
                &["Request has timed out."],
            )
            .await)
        }
        Err(err) => Err(err).context("Failed to claim Starknet fee"),
    }
}

pub async fn process_deploy_token_event(
    jsonrpc_client: &JsonRpcClient,
    omni_connector: Arc<OmniConnector>,
    signer: AccountId,
    deploy_token_event: DeployToken,
    near_nonce: Arc<utils::nonce::NonceManager>,
) -> Result<EventAction> {
    let DeployToken::Starknet { tx_hash } = deploy_token_event else {
        warn!("Routing mismatch, dropping: {deploy_token_event:?}");
        return Ok(EventAction::Drop);
    };

    info!(
        "Processing Starknet DeployToken ({:?}): {tx_hash}",
        ChainKind::Strk
    );

    let nonce = match near_nonce.reserve_nonce() {
        Ok(nonce) => Some(nonce),
        Err(err) => {
            warn!("Failed to reserve nonce: {err:?}");
            return Ok(EventAction::Retry);
        }
    };

    let bind_token_args = omni_connector::BindTokenArgs::BindTokenWithMpcProofTx {
        chain_kind: ChainKind::Strk,
        tx_hash,
        transaction_options: TransactionOptions {
            nonce,
            wait_until: near_primitives::views::TxExecutionStatus::Included,
            wait_final_outcome_timeout_sec: None,
        },
    };

    match omni_connector.bind_token(bind_token_args).await {
        Ok(near_tx_hash) => {
            info!("Bound Starknet token: {near_tx_hash}");
            let Ok(crypto_hash) = near_tx_hash.parse() else {
                warn!("Failed to parse {near_tx_hash} as CryptoHash, removing");
                return Ok(EventAction::Remove);
            };
            Ok(utils::near::resolve_tx_action(
                jsonrpc_client,
                crypto_hash,
                signer,
                &["Request has timed out."],
            )
            .await)
        }
        Err(err) => Err(err).context("Failed to bind Starknet token"),
    }
}
