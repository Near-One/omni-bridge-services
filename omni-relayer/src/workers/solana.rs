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
use omni_types::{
    ChainKind, Fee, OmniAddress, TransferId, locker_args::ClaimFeeArgs,
    prover_args::WormholeVerifyProofArgs, prover_result::ProofKind,
};

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
    let Transfer::Solana {
        ref sender,
        ref token,
        ref recipient,
        fee,
        native_fee,
        ref emitter,
        sequence,
        creation_timestamp,
        ..
    } = transfer
    else {
        warn!("Routing mismatch, removing: {transfer:?}");
        return Ok(EventAction::Remove);
    };

    let chain_kind = sender.get_chain();

    let transfer_id = TransferId {
        origin_chain: chain_kind,
        origin_nonce: sequence,
    };

    let expected_finalization_time = match chain_kind {
        ChainKind::Fogo => config
            .fogo
            .as_ref()
            .map_or(0, |c| c.expected_finalization_time),
        ChainKind::Sol => config
            .solana
            .as_ref()
            .map_or(0, |c| c.expected_finalization_time),
        _ => unreachable!("SVM worker invoked with non-SVM chain: {chain_kind:?}"),
    };
    let current_timestamp = chrono::Utc::now().timestamp();
    let effective_wait = std::cmp::max(expected_finalization_time, config.kyt.delay_secs);
    if current_timestamp < creation_timestamp + effective_wait {
        let remaining = (creation_timestamp + effective_wait - current_timestamp).unsigned_abs();
        return Ok(EventAction::RetryAfter(Duration::from_secs(remaining)));
    }

    info!(
        "Processing SVM InitTransfer ({:?}:{})",
        transfer_id.origin_chain, transfer_id.origin_nonce
    );

    let context = format!(
        "({:?}:{})",
        transfer_id.origin_chain, transfer_id.origin_nonce
    );
    if let Some(action) = super::near::check_kyt(sender, &context).await {
        return Ok(action);
    }

    match omni_connector
        .is_transfer_finalised(Some(chain_kind), ChainKind::Near, sequence)
        .await
    {
        Ok(true) => {
            warn!("Transfer is already finalised, removing: {transfer_id:?}");
            return Ok(EventAction::Remove);
        }
        Ok(false) => {}
        Err(err) => {
            warn!("Failed to check if transfer is finalised: {err:?}");
            return Ok(EventAction::Retry);
        }
    }

    if config.is_bridge_api_enabled() {
        let Ok(token) = OmniAddress::new_from_slice(chain_kind, &token.to_bytes()) else {
            warn!("Failed to parse token \"{token}\" as `OmniAddress`, removing");
            return Ok(EventAction::Remove);
        };

        let Ok(needed_fee) =
            utils::bridge_api::TransferFee::get_transfer_fee(config, sender, recipient, &token)
                .await
        else {
            warn!("Failed to get transfer fee for transfer: {transfer:?}");
            return Ok(EventAction::Retry);
        };

        let provided_fee = Fee {
            fee,
            native_fee: u128::from(native_fee).into(),
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

    let wormhole_chain_id = config.wormhole.svm_chain_id(chain_kind);
    let Ok(vaa) = omni_connector
        .wormhole_get_vaa(wormhole_chain_id, &emitter, sequence)
        .await
    else {
        warn!(
            "VAA is not ready for {:?}:{}",
            transfer_id.origin_chain, transfer_id.origin_nonce
        );
        return Ok(EventAction::Retry);
    };

    let fee_recipient = omni_connector
        .near_bridge_client()
        .and_then(NearBridgeClient::account_id)
        .context("Failed to get relayer account id")?;

    let storage_deposit_actions = match utils::storage::get_storage_deposit_actions(
        &omni_connector,
        chain_kind,
        recipient,
        &fee_recipient,
        &token.to_string(),
        fee.0,
        u128::from(native_fee),
    )
    .await
    {
        Ok(actions) => actions,
        Err(err) => {
            anyhow::bail!("Failed to get storage deposit actions: {err}");
        }
    };

    let nonce = near_nonce
        .reserve_nonce()
        .context("Failed to reserve nonce for near transaction")?;

    let fin_transfer_args = omni_connector::FinTransferArgs::NearFinTransferWithVaa {
        chain_kind,
        destination_chain: recipient.get_chain(),
        storage_deposit_actions,
        vaa,
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
        Err(err) => {
            anyhow::bail!("Failed to finalize transfer: {err:?}");
        }
    }
}

pub async fn process_fin_transfer_event(
    config: &config::Config,
    jsonrpc_client: &JsonRpcClient,
    omni_connector: Arc<OmniConnector>,
    signer: AccountId,
    fin_transfer: FinTransfer,
    near_nonce: Arc<utils::nonce::NonceManager>,
) -> Result<EventAction> {
    let FinTransfer::Solana {
        emitter,
        sequence,
        transfer_id,
        chain_kind,
        creation_timestamp,
    } = fin_transfer
    else {
        warn!("Routing mismatch, removing: {fin_transfer:?}");
        return Ok(EventAction::Remove);
    };

    let expected_finalization_time = match chain_kind {
        ChainKind::Fogo => config
            .fogo
            .as_ref()
            .map_or(0, |c| c.expected_finalization_time),
        ChainKind::Sol => config
            .solana
            .as_ref()
            .map_or(0, |c| c.expected_finalization_time),
        _ => unreachable!("SVM worker invoked with non-SVM chain: {chain_kind:?}"),
    };
    let current_timestamp = chrono::Utc::now().timestamp();
    if current_timestamp < creation_timestamp + expected_finalization_time {
        let remaining =
            (creation_timestamp + expected_finalization_time - current_timestamp).unsigned_abs();
        return Ok(EventAction::RetryAfter(Duration::from_secs(remaining)));
    }

    info!("Processing SVM FinTransfer ({chain_kind:?}:{sequence})");

    if let Some(transfer_id) = transfer_id
        && let Err(BridgeSdkError::NearRpcError(NearRpcError::RpcQueryError(
            JsonRpcError::ServerError(JsonRpcServerError::HandlerError(
                RpcQueryError::ContractExecutionError { vm_error, .. },
            )),
        ))) = omni_connector.near_get_transfer_message(transfer_id).await
    {
        // TODO: refactor when enum errors will become available on mainnet
        if vm_error.contains("The transfer does not exist") {
            info!("No fee to claim for FinTransfer ({transfer_id:?})");
            return Ok(EventAction::Remove);
        }
    }

    let wormhole_chain_id = config.wormhole.svm_chain_id(chain_kind);
    let Ok(vaa) = omni_connector
        .wormhole_get_vaa(wormhole_chain_id, emitter, sequence)
        .await
    else {
        warn!("VAA is not ready for {chain_kind:?}:{sequence}");
        return Ok(EventAction::Retry);
    };

    let Ok(prover_args) = borsh::to_vec(&WormholeVerifyProofArgs {
        proof_kind: ProofKind::FinTransfer,
        vaa,
    }) else {
        anyhow::bail!("Failed to serialize prover args for {sequence}");
    };

    let claim_fee_args = ClaimFeeArgs {
        chain_kind,
        prover_args,
    };

    let nonce = near_nonce
        .reserve_nonce()
        .context("Failed to reserve nonce for near transaction")?;

    match omni_connector
        .near_claim_fee(
            claim_fee_args,
            TransactionOptions {
                nonce: Some(nonce),
                wait_until: near_primitives::views::TxExecutionStatus::Included,
                wait_final_outcome_timeout_sec: None,
            },
        )
        .await
    {
        Ok(tx_hash) => Ok(utils::near::resolve_tx_action(
            jsonrpc_client,
            tx_hash,
            signer,
            &["Request has timed out."],
        )
        .await),
        Err(err) => {
            anyhow::bail!("Failed to claim fee: {err:?}");
        }
    }
}

pub async fn process_deploy_token_event(
    config: &config::Config,
    jsonrpc_client: &JsonRpcClient,
    omni_connector: Arc<OmniConnector>,
    signer: AccountId,
    deploy_token_event: DeployToken,
    near_nonce: Arc<utils::nonce::NonceManager>,
) -> Result<EventAction> {
    let DeployToken::Solana {
        emitter,
        sequence,
        chain_kind,
    } = deploy_token_event
    else {
        warn!("Routing mismatch, removing: {deploy_token_event:?}");
        return Ok(EventAction::Remove);
    };

    info!("Processing SVM DeployToken ({chain_kind:?}:{sequence})");

    let wormhole_chain_id = config.wormhole.svm_chain_id(chain_kind);
    let Ok(vaa) = omni_connector
        .wormhole_get_vaa(wormhole_chain_id, emitter, sequence)
        .await
    else {
        warn!("VAA is not ready for {chain_kind:?}:{sequence}");
        return Ok(EventAction::Retry);
    };

    let Ok(prover_args) = borsh::to_vec(&WormholeVerifyProofArgs {
        proof_kind: ProofKind::DeployToken,
        vaa,
    }) else {
        anyhow::bail!("Failed to serialize prover args for {sequence}");
    };

    let nonce = match near_nonce.reserve_nonce() {
        Ok(nonce) => Some(nonce),
        Err(err) => {
            warn!("Failed to reserve nonce: {err:?}");
            return Ok(EventAction::Retry);
        }
    };

    let bind_token_args = omni_connector::BindTokenArgs::BindTokenWithArgs {
        chain_kind,
        prover_args,
        transaction_options: TransactionOptions {
            nonce,
            wait_until: near_primitives::views::TxExecutionStatus::Included,
            wait_final_outcome_timeout_sec: None,
        },
    };

    match omni_connector.bind_token(bind_token_args).await {
        Ok(tx_hash) => {
            info!("Bound token: {tx_hash}");
            let Ok(crypto_hash) = tx_hash.parse() else {
                warn!("Failed to parse {tx_hash} as CryptoHash, removing");
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
        Err(err) => {
            anyhow::bail!("Failed to bind token: {err:?}");
        }
    }
}
