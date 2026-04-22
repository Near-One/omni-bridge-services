use std::path::Path;

use anyhow::{Context, Result};
use tracing::{info, warn};

use near_crypto::{InMemorySigner, Signer};
use near_jsonrpc_client::JsonRpcClient;
use near_primitives::{hash::CryptoHash, types::AccountId};
use omni_types::{ChainKind, near_events::OmniBridgeEvent};

use crate::{
    config,
    workers::{EventAction, WorkerEvent, Transfer},
};

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
    match resolve_tx_receipts(jsonrpc_client, tx_hash, sender_account_id, retryable_errors).await {
        Ok(_) => EventAction::Remove,
        Err(action) => action,
    }
}

pub async fn resolve_tx_receipts(
    jsonrpc_client: &JsonRpcClient,
    tx_hash: CryptoHash,
    sender_account_id: AccountId,
    retryable_errors: &[&str],
) -> Result<Vec<near_primitives::views::ExecutionOutcomeWithIdView>, EventAction> {
    let request = near_jsonrpc_client::methods::tx::RpcTransactionStatusRequest {
        transaction_info: near_jsonrpc_client::methods::tx::TransactionInfo::TransactionId {
            tx_hash,
            sender_account_id,
        },
        wait_until: near_primitives::views::TxExecutionStatus::Final,
    };

    let response = match jsonrpc_client.call(request).await {
        Ok(response) => response,
        Err(err) => {
            warn!("Failed to get transaction status for {tx_hash}: {err:?}");
            return Err(EventAction::Retry);
        }
    };

    let Some(near_primitives::views::FinalExecutionOutcomeViewEnum::FinalExecutionOutcome(
        outcome,
    )) = response.final_execution_outcome
    else {
        warn!("Receipts missing for transaction {tx_hash}");
        return Err(EventAction::Retry);
    };

    let mut non_retryable_failure = false;
    for receipt_outcome in &outcome.receipts_outcome {
        if let near_primitives::views::ExecutionStatusView::Failure(ref err) =
            receipt_outcome.outcome.status
        {
            let err_str = err.to_string();
            if retryable_errors.iter().any(|e| err_str.contains(e)) {
                warn!("Transaction {tx_hash} has retryable receipt failure: {err:?}");
                return Err(EventAction::Retry);
            }
            warn!("Transaction {tx_hash} has non-retryable receipt failure: {err:?}");
            non_retryable_failure = true;
        }
    }

    if non_retryable_failure {
        Err(EventAction::Remove)
    } else {
        Ok(outcome.receipts_outcome)
    }
}

pub fn extract_sign_transfer_event(
    receipts: &[near_primitives::views::ExecutionOutcomeWithIdView],
) -> Vec<WorkerEvent> {
    receipts
        .iter()
        .flat_map(|r| &r.outcome.logs)
        .find_map(|log| match serde_json::from_str::<OmniBridgeEvent>(log) {
            Ok(event @ OmniBridgeEvent::SignTransferEvent { .. }) => Some(event),
            _ => None,
        })
        .map(|e| WorkerEvent::OmniBridge(Box::new(e)))
        .into_iter()
        .collect()
}

pub fn extract_near_to_utxo(
    receipts: &[near_primitives::views::ExecutionOutcomeWithIdView],
    destination_chain: ChainKind,
) -> Vec<WorkerEvent> {
    const EVENT_JSON_PREFIX: &str = "EVENT_JSON:";
    const GENERATE_BTC_PENDING_INFO_EVENT: &str = "generate_btc_pending_info";
    const UTXO_REMOVED_EVENT: &str = "utxo_removed";

    let mut btc_pending_id = None;
    let mut utxo_count = None;

    for log in receipts.iter().flat_map(|r| &r.outcome.logs) {
        let log = log.strip_prefix(EVENT_JSON_PREFIX).unwrap_or(log);

        let Ok(value) = serde_json::from_str::<serde_json::Value>(log) else {
            continue;
        };

        match value.get("event").and_then(|v| v.as_str()) {
            Some(GENERATE_BTC_PENDING_INFO_EVENT) if btc_pending_id.is_none() => {
                btc_pending_id = value
                    .pointer("/data/0/btc_pending_id")
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
            }
            Some(UTXO_REMOVED_EVENT) if utxo_count.is_none() => {
                utxo_count = value
                    .pointer("/data/0/utxo_storage_keys")
                    .and_then(|v| v.as_array())
                    .and_then(|a| u32::try_from(a.len()).ok());
            }
            _ => {}
        }
    }

    btc_pending_id
        .zip(utxo_count)
        .map(|(btc_pending_id, utxo_count)| {
            (0..u64::from(utxo_count))
                .map(|sign_index| {
                    WorkerEvent::NearToUtxo(Box::new(Transfer::NearToUtxo {
                        chain: destination_chain,
                        btc_pending_id: btc_pending_id.clone(),
                        sign_index,
                    }))
                })
                .collect()
        })
        .unwrap_or_default()
}
