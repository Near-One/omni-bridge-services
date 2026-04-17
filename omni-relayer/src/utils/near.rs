use std::path::Path;

use anyhow::{Context, Result};
use tracing::{info, warn};

use near_crypto::{InMemorySigner, Signer};
use near_jsonrpc_client::JsonRpcClient;
use near_primitives::{hash::CryptoHash, types::AccountId};
use omni_types::{ChainKind, near_events::OmniBridgeEvent};

use crate::{config, workers::EventAction};

pub fn get_signer(
    config: &config::Config,
    near_signer_type: config::NearSignerType,
) -> Result<InMemorySigner> {
    info!("Getting NEAR signer");

    let file = match near_signer_type {
        config::NearSignerType::Omni => config.near.omni_credentials_path.as_deref(),
        config::NearSignerType::Fast => config.near.fast_credentials_path.as_deref(),
    };

    if let Some(file) = file {
        info!("Using NEAR credentials file: {file}");
        if let Ok(Signer::InMemory(signer)) = InMemorySigner::from_file(Path::new(file)) {
            return Ok(signer);
        }
    }

    info!("Retrieving NEAR credentials from env");

    let account_id_env = match near_signer_type {
        config::NearSignerType::Omni => "NEAR_OMNI_ACCOUNT_ID",
        config::NearSignerType::Fast => "NEAR_FAST_ACCOUNT_ID",
    };

    let account_id = std::env::var(account_id_env)
        .context(format!(
            "Failed to get `{account_id_env}` environment variable"
        ))?
        .parse()
        .context(format!("Failed to parse `{account_id_env}`"))?;

    let private_key = config::get_private_key(ChainKind::Near, Some(near_signer_type))
        .parse()
        .context("Failed to parse private key")?;

    if let Signer::InMemory(signer) = InMemorySigner::from_secret_key(account_id, private_key) {
        Ok(signer)
    } else {
        anyhow::bail!("Failed to create NEAR signer")
    }
}

pub async fn resolve_tx_action(
    jsonrpc_client: &JsonRpcClient,
    tx_hash: CryptoHash,
    sender_account_id: AccountId,
    retryable_errors: &[&str],
) -> EventAction {
    match get_final_tx_receipts(jsonrpc_client, tx_hash, sender_account_id).await {
        Ok(receipts) => {
            for receipt_outcome in receipts {
                if let near_primitives::views::ExecutionStatusView::Failure(ref err) =
                    receipt_outcome.outcome.status
                {
                    let err_str = err.to_string();
                    if retryable_errors.iter().any(|e| err_str.contains(e)) {
                        warn!("Transaction {tx_hash} has retryable receipt failure: {err:?}");
                        return EventAction::Retry;
                    }
                }
            }

            EventAction::Remove
        },
        Err(err) => {
            warn!("Failed to get transaction receipts for {tx_hash}: {err:?}");
            EventAction::Retry
        }
    }
}

pub async fn resolve_tx_action_and_extract_sign_event(
    jsonrpc_client: &JsonRpcClient,
    tx_hash: CryptoHash,
    sender_account_id: AccountId,
    retryable_errors: &[&str],
) -> (EventAction, Option<OmniBridgeEvent>) {
    match get_final_tx_receipts(jsonrpc_client, tx_hash, sender_account_id).await {
        Ok(receipts) => {
            let mut sign_event = None;
            for receipt_outcome in receipts {
                if let near_primitives::views::ExecutionStatusView::Failure(ref err) =
                    receipt_outcome.outcome.status
                {
                    let err_str = err.to_string();
                    if retryable_errors.iter().any(|e| err_str.contains(e)) {
                        warn!("Transaction {tx_hash} has retryable receipt failure: {err:?}");
                        return (EventAction::Retry, None);
                    }
                }

                for log in &receipt_outcome.outcome.logs {
                    if let Ok(event @ OmniBridgeEvent::SignTransferEvent { .. }) =
                        serde_json::from_str::<OmniBridgeEvent>(log)
                    {
                        sign_event = Some(event);
                    }
                }
            }

            (EventAction::Remove, sign_event)
        }
        Err(err) => {
            warn!("Failed to get transaction receipts for {tx_hash}: {err:?}");
            (EventAction::Retry, None)
        }
    }
}

async fn get_final_tx_receipts(
    jsonrpc_client: &JsonRpcClient,
    tx_hash: CryptoHash,
    sender_account_id: AccountId,
) -> Result<Vec<near_primitives::views::ExecutionOutcomeWithIdView>> {
    let request = near_jsonrpc_client::methods::tx::RpcTransactionStatusRequest {
        transaction_info: near_jsonrpc_client::methods::tx::TransactionInfo::TransactionId {
            tx_hash,
            sender_account_id,
        },
        wait_until: near_primitives::views::TxExecutionStatus::Final,
    };

    let response = jsonrpc_client
        .call(request)
        .await
        .context(format!("Failed to get transaction status for {tx_hash}"))?;

    if let Some(near_primitives::views::FinalExecutionOutcomeViewEnum::FinalExecutionOutcome(
        outcome,
    )) = response.final_execution_outcome
    {
        Ok(outcome.receipts_outcome)
    } else {
        anyhow::bail!("Receipts missing for transaction {tx_hash}")
    }
}
