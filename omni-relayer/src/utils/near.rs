use std::path::Path;

use anyhow::{Context, Result};
use tracing::{info, warn};

use near_crypto::{InMemorySigner, Signer};
use near_jsonrpc_client::JsonRpcClient;
use near_primitives::{hash::CryptoHash, types::AccountId};
use omni_types::{ChainKind, OmniAddress, near_events::OmniBridgeEvent};

use crate::{
    config,
    metrics::{Metrics, receipt_outcome},
    workers::{EventAction, Transfer, WorkerEvent},
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
    let outcome = match fetch_tx_outcome(jsonrpc_client, tx_hash, sender_account_id).await {
        Ok(outcome) => outcome,
        Err(err) => {
            warn!("{err:?}");
            return Err(EventAction::Retry);
        }
    };

    let (has_listed_failure, has_any_failure) =
        scan_receipt_failures(tx_hash, &outcome, retryable_errors);
    if has_listed_failure {
        Metrics::global().record_near_tx_receipt(receipt_outcome::RETRY_FAILURE);
        Err(EventAction::Retry)
    } else if has_any_failure {
        Metrics::global().record_near_tx_receipt(receipt_outcome::REVERTED);
        Err(EventAction::Drop)
    } else {
        Metrics::global().record_near_tx_receipt(receipt_outcome::OK);
        Ok(outcome.receipts_outcome)
    }
}

pub async fn tx_has_errors(
    jsonrpc_client: &JsonRpcClient,
    tx_hash: CryptoHash,
    sender_account_id: AccountId,
    errors: &[&str],
) -> Result<bool> {
    let outcome = fetch_tx_outcome(jsonrpc_client, tx_hash, sender_account_id).await?;
    let (has_listed_failure, _) = scan_receipt_failures(tx_hash, &outcome, errors);
    Ok(has_listed_failure)
}

async fn fetch_tx_outcome(
    jsonrpc_client: &JsonRpcClient,
    tx_hash: CryptoHash,
    sender_account_id: AccountId,
) -> Result<near_primitives::views::FinalExecutionOutcomeView> {
    let request = near_jsonrpc_client::methods::tx::RpcTransactionStatusRequest {
        transaction_info: near_jsonrpc_client::methods::tx::TransactionInfo::TransactionId {
            tx_hash,
            sender_account_id,
        },
        wait_until: near_primitives::views::TxExecutionStatus::Final,
    };

    let Ok(response) = jsonrpc_client.call(request).await else {
        Metrics::global().record_near_tx_receipt(receipt_outcome::RETRY_RPC);
        anyhow::bail!("Failed to get transaction status for {tx_hash}");
    };

    if let Some(near_primitives::views::FinalExecutionOutcomeViewEnum::FinalExecutionOutcome(
        outcome,
    )) = response.final_execution_outcome
    {
        Ok(outcome)
    } else {
        Metrics::global().record_near_tx_receipt(receipt_outcome::RETRY_MISSING);
        Err(anyhow::anyhow!(
            "Receipts missing for transaction {tx_hash}"
        ))
    }
}

const TERMINAL_FAILURE_OVERRIDES: [&str; 1] = ["BTC pending info not exist"];
const RETRYABLE_FAILURE_OVERRIDES: [&str; 1] = ["Pausable: Method is paused"];

fn is_retryable_failure(err_str: &str, errors: &[&str]) -> bool {
    if TERMINAL_FAILURE_OVERRIDES
        .iter()
        .any(|terminal| err_str.contains(terminal))
    {
        return false;
    }

    RETRYABLE_FAILURE_OVERRIDES
        .iter()
        .chain(errors)
        .any(|e| err_str.contains(e))
}

fn scan_receipt_failures(
    tx_hash: CryptoHash,
    outcome: &near_primitives::views::FinalExecutionOutcomeView,
    errors: &[&str],
) -> (bool, bool) {
    let mut has_listed_failure = false;
    let mut has_any_failure = false;

    for receipt_outcome in &outcome.receipts_outcome {
        if let near_primitives::views::ExecutionStatusView::Failure(ref err) =
            receipt_outcome.outcome.status
        {
            has_any_failure = true;
            let err_str = err.to_string();
            if is_retryable_failure(&err_str, errors) {
                has_listed_failure = true;
                warn!("Transaction {tx_hash} has expected receipt failure: {err:?}");
            } else {
                warn!("Transaction {tx_hash} has unexpected receipt failure: {err:?}");
            }
        }
    }

    (has_listed_failure, has_any_failure)
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
    sender: &OmniAddress,
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
                        sender: sender.clone(),
                        creation_timestamp: chrono::Utc::now().timestamp(),
                    }))
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The patterns the UTXO signing path lists as retryable.
    const SIGN_RETRYABLE: [&str; 4] = [
        "Request has timed out.",
        "not exist",
        "Previous btc tx has not been signed",
        "Too many pending sign transactions",
    ];

    #[test]
    fn missing_utxo_is_retryable() {
        let err = "Smart contract panicked: UTXO 0b13776c8a64eaf701c240885afc0c8560258c08201df207863033380b03b0c6:1 not exist";
        assert!(is_retryable_failure(err, &SIGN_RETRYABLE));
    }

    #[test]
    fn missing_btc_pending_info_is_terminal() {
        let err = "Smart contract panicked: BTC pending info not exist";
        assert!(!is_retryable_failure(err, &SIGN_RETRYABLE));
    }

    #[test]
    fn terminal_override_wins_over_every_listed_pattern() {
        // Even when the caller lists the exact terminal text, it stays terminal.
        let err = "Smart contract panicked: BTC pending info not exist";
        assert!(!is_retryable_failure(
            err,
            &["BTC pending info not exist", "not exist"]
        ));
    }

    #[test]
    fn other_listed_patterns_stay_retryable() {
        for err in [
            "Smart contract panicked: Previous btc tx has not been signed",
            "Smart contract panicked: Too many pending sign transactions",
            "Request has timed out.",
        ] {
            assert!(is_retryable_failure(err, &SIGN_RETRYABLE), "{err}");
        }
    }

    #[test]
    fn unlisted_failures_are_not_retryable() {
        let err = "Smart contract panicked: Insufficient balance";
        assert!(!is_retryable_failure(err, &SIGN_RETRYABLE));
    }

    #[test]
    fn empty_pattern_list_never_retries() {
        assert!(!is_retryable_failure("UTXO abc:0 not exist", &[]));
    }

    /// A paused contract must park the transfer, not drop it, on every path.
    #[test]
    fn paused_contract_is_retryable_for_any_caller() {
        let err = "Smart contract panicked: Pausable: Method is paused";
        assert!(is_retryable_failure(err, &SIGN_RETRYABLE));
        assert!(is_retryable_failure(err, &[]));
    }
}
