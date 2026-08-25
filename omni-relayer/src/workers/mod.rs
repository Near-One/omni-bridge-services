use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use crate::types::DepositMsg;
use alloy::primitives::TxHash;
use anyhow::{Context, Result};
use bridge_connector_common::result::BridgeSdkError;
use near_jsonrpc_client::JsonRpcClient;
use near_primitives::types::AccountId;
use tokio_stream::StreamExt;
use tracing::{Instrument, info, warn};

use near_sdk::json_types::U128;
use sha2::{Digest, Sha256};
use solana_sdk::pubkey::Pubkey;

use omni_connector::OmniConnector;
use omni_types::{
    ChainKind, Fee, OmniAddress, TransferId, TransferIdKind, TransferMessage, UnifiedTransferId,
    UtxoFinTransferMsg, UtxoId, near_events::OmniBridgeEvent,
};

use crate::metrics::{Metrics, event_outcome, stall_reason};
use crate::{config, utils};

mod aptos;
mod evm;
mod near;
mod solana;
mod starknet;
pub mod utxo;

const PAUSED_ERROR: u32 = 6008;

fn default_sol_chain_kind() -> ChainKind {
    ChainKind::Sol
}

/// Fields attached to the per-message `transfer` span so that every log line for
/// one event (worker + bridge-sdk + post-processing) can be searched and
/// filtered in Loki. Populated best-effort; absent fields stay empty.
/// - `transfer_id`: canonical `UnifiedTransferId` (`eth:123`), the domain key.
/// - `kind`: the processed enum variant (e.g. `Transfer::Evm`, `FinTransfer::Solana`).
/// - `tx`: origin transaction hash (explorer-facing), where the event carries it.
struct LogContext {
    transfer_id: Option<UnifiedTransferId>,
    kind: &'static str,
    tx: Option<String>,
}

impl LogContext {
    fn record(self) {
        let span = tracing::Span::current();
        span.record("kind", self.kind);
        if let Some(transfer_id) = self.transfer_id {
            span.record("transfer_id", tracing::field::display(transfer_id));
        }
        if let Some(ref tx) = self.tx {
            span.record("tx", tx.as_str());
        }
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct RetryableEvent<E> {
    pub event: E,
    pub creation_timestamp: i64,
    pub last_updated_timestamp: i64,
    pub retries: u32,
}

impl<E> RetryableEvent<E> {
    pub fn new(event: E) -> Self {
        let current_timestamp = chrono::Utc::now().timestamp();

        Self {
            event,
            creation_timestamp: current_timestamp,
            last_updated_timestamp: current_timestamp,
            retries: 0,
        }
    }
}

pub enum EventAction {
    Retry,
    RetryAfter(Duration),
    Remove,
}

#[derive(Debug)]
enum NatsAckDecision {
    Ack,
    NakWithBackoff(Duration),
    Term,
}

fn compute_ack_decision(
    result: &Result<EventAction>,
    age: Duration,
    delivered: u32,
    max_backoff: Duration,
    max_message_age: Duration,
) -> NatsAckDecision {
    if let Ok(EventAction::Remove) = result {
        return NatsAckDecision::Ack;
    }

    if age > max_message_age {
        return NatsAckDecision::Term;
    }
    let backoff = match result {
        Ok(EventAction::RetryAfter(d)) => (*d).min(max_backoff),
        _ => Duration::from_secs(3u64.saturating_pow(delivered.saturating_sub(1))).min(max_backoff),
    };
    NatsAckDecision::NakWithBackoff(backoff)
}

pub enum WorkerEvent {
    OmniBridge(Box<OmniBridgeEvent>),
    NearToUtxo(Box<Transfer>),
}

struct MessageResult {
    action: Result<EventAction>,
    needs_evm_nonce_resync: bool,
    /// `FEE_MAPPING` key for this event, if it has one. Removed only when the
    /// event leaves the queue (Remove or max-age Term), not on retry.
    fee_key: Option<String>,
    produced_events: Vec<WorkerEvent>,
    origin_chain: Option<ChainKind>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "init_transfer")]
pub enum Transfer {
    Near {
        transfer_message: TransferMessage,
        #[serde(default)]
        creation_timestamp: i64,
    },
    Evm {
        chain_kind: ChainKind,
        tx_hash: TxHash,
        log: utils::evm::InitTransferMessage,
        creation_timestamp: i64,
        expected_finalization_time: i64,
    },
    Solana {
        amount: U128,
        token: Pubkey,
        sender: OmniAddress,
        recipient: OmniAddress,
        fee: U128,
        native_fee: u64,
        message: String,
        emitter: Pubkey,
        sequence: u64,
        #[serde(default)]
        creation_timestamp: i64,
    },
    Starknet {
        tx_hash: String,
        sender: OmniAddress,
        token: OmniAddress,
        origin_nonce: u64,
        amount: U128,
        fee: Fee,
        recipient: OmniAddress,
        message: String,
        #[serde(default)]
        creation_timestamp: i64,
    },
    Aptos {
        tx_hash: String,
        sender: OmniAddress,
        token: OmniAddress,
        origin_nonce: u64,
        amount: U128,
        fee: Fee,
        recipient: OmniAddress,
        message: String,
        #[serde(default)]
        creation_timestamp: i64,
    },
    Utxo {
        utxo_transfer_message: UtxoFinTransferMsg,
        new_transfer_id: UnifiedTransferId,
    },
    NearToUtxo {
        chain: ChainKind,
        btc_pending_id: String,
        sign_index: u64,
        sender: AccountId,
        #[serde(default)]
        creation_timestamp: i64,
    },
    UtxoToNear {
        chain: ChainKind,
        btc_tx_hash: String,
        vout: u32,
        deposit_msg: DepositMsg,
        #[serde(default)]
        amount: U128,
    },
    Fast {
        block_number: u64,
        tx_hash: String,
        token: String,
        amount: U128,
        transfer_id: TransferId,
        sender: OmniAddress,
        recipient: OmniAddress,
        fee: Fee,
        msg: String,
        storage_deposit_amount: Option<U128>,
        safe_confirmations: u64,
    },
}

impl Transfer {
    /// The canonical transfer id for this event, if it has one. `NearToUtxo`
    /// is not tied to a single transfer id (it only carries the pending BTC
    /// tx hash).
    fn transfer_id(&self) -> Option<UnifiedTransferId> {
        Some(match self {
            Transfer::Near {
                transfer_message, ..
            } => transfer_message.get_transfer_id().into(),
            Transfer::Utxo {
                new_transfer_id, ..
            } => new_transfer_id.clone(),
            Transfer::Evm {
                chain_kind, log, ..
            } => TransferId {
                origin_chain: *chain_kind,
                origin_nonce: log.origin_nonce,
            }
            .into(),
            Transfer::Solana {
                sender, sequence, ..
            } => TransferId {
                origin_chain: sender.get_chain(),
                origin_nonce: *sequence,
            }
            .into(),
            Transfer::Starknet { origin_nonce, .. } => TransferId {
                origin_chain: ChainKind::Strk,
                origin_nonce: *origin_nonce,
            }
            .into(),
            Transfer::Aptos { origin_nonce, .. } => TransferId {
                origin_chain: ChainKind::Aptos,
                origin_nonce: *origin_nonce,
            }
            .into(),
            Transfer::Fast { transfer_id, .. } => (*transfer_id).into(),
            Transfer::UtxoToNear {
                chain,
                btc_tx_hash,
                vout,
                ..
            } => UnifiedTransferId {
                origin_chain: *chain,
                kind: TransferIdKind::Utxo(UtxoId {
                    tx_hash: btc_tx_hash.clone(),
                    vout: *vout,
                }),
            },
            Transfer::NearToUtxo { .. } => return None,
        })
    }

    fn origin_chain(&self) -> ChainKind {
        match self {
            Transfer::Near {
                transfer_message, ..
            } => transfer_message.get_transfer_id().origin_chain,
            Transfer::Evm { chain_kind, .. } => *chain_kind,
            Transfer::Solana { sender, .. } => sender.get_chain(),
            Transfer::Starknet { .. } => ChainKind::Strk,
            Transfer::Aptos { .. } => ChainKind::Aptos,
            Transfer::Fast { transfer_id, .. } => transfer_id.origin_chain,
            Transfer::Utxo {
                new_transfer_id, ..
            } => new_transfer_id.origin_chain,
            Transfer::UtxoToNear { chain, .. } => *chain,
            Transfer::NearToUtxo { .. } => ChainKind::Near,
        }
    }

    fn log_context(&self) -> LogContext {
        let (kind, tx) = match self {
            Transfer::Near { .. } => ("Transfer::Near", None),
            Transfer::Evm { tx_hash, .. } => ("Transfer::Evm", Some(tx_hash.to_string())),
            Transfer::Solana { .. } => ("Transfer::Solana", None),
            Transfer::Starknet { tx_hash, .. } => ("Transfer::Starknet", Some(tx_hash.clone())),
            Transfer::Aptos { tx_hash, .. } => ("Transfer::Aptos", Some(tx_hash.clone())),
            Transfer::Fast { tx_hash, .. } => ("Transfer::Fast", Some(tx_hash.clone())),
            Transfer::Utxo {
                utxo_transfer_message,
                ..
            } => (
                "Transfer::Utxo",
                Some(utxo_transfer_message.utxo_id.tx_hash.clone()),
            ),
            Transfer::NearToUtxo { btc_pending_id, .. } => {
                ("Transfer::NearToUtxo", Some(btc_pending_id.clone()))
            }
            Transfer::UtxoToNear { btc_tx_hash, .. } => {
                ("Transfer::UtxoToNear", Some(btc_tx_hash.clone()))
            }
        };
        LogContext {
            transfer_id: self.transfer_id(),
            kind,
            tx,
        }
    }
}

#[derive(Debug, serde::Serialize, serde::Deserialize, Clone)]
#[serde(tag = "fin_transfer")]
pub enum FinTransfer {
    Evm {
        chain_kind: ChainKind,
        tx_hash: TxHash,
        creation_timestamp: i64,
        expected_finalization_time: i64,
        transfer_id: TransferId,
    },
    Solana {
        emitter: String,
        sequence: u64,
        transfer_id: Option<TransferId>,
        #[serde(default = "default_sol_chain_kind")]
        chain_kind: ChainKind,
        #[serde(default)]
        creation_timestamp: i64,
    },
    Starknet {
        tx_hash: String,
        transfer_id: TransferId,
    },
    Aptos {
        tx_hash: String,
        transfer_id: TransferId,
    },
}

impl FinTransfer {
    /// The canonical transfer id for this fin-transfer event, if known.
    fn transfer_id(&self) -> Option<UnifiedTransferId> {
        match self {
            FinTransfer::Evm { transfer_id, .. }
            | FinTransfer::Starknet { transfer_id, .. }
            | FinTransfer::Aptos { transfer_id, .. } => Some((*transfer_id).into()),
            FinTransfer::Solana { transfer_id, .. } => transfer_id.map(Into::into),
        }
    }

    /// The chain this fin-transfer's underlying transfer originated from, for
    /// the `chain` metric label.
    fn origin_chain(&self) -> Option<ChainKind> {
        self.transfer_id().map(|id| id.origin_chain)
    }

    fn log_context(&self) -> LogContext {
        let (kind, tx) = match self {
            FinTransfer::Evm { tx_hash, .. } => ("FinTransfer::Evm", Some(tx_hash.to_string())),
            FinTransfer::Solana { .. } => ("FinTransfer::Solana", None),
            FinTransfer::Starknet { tx_hash, .. } => {
                ("FinTransfer::Starknet", Some(tx_hash.clone()))
            }
            FinTransfer::Aptos { tx_hash, .. } => ("FinTransfer::Aptos", Some(tx_hash.clone())),
        };
        LogContext {
            transfer_id: self.transfer_id(),
            kind,
            tx,
        }
    }
}

#[derive(Debug, serde::Serialize, serde::Deserialize, Clone)]
#[serde(tag = "deploy_token")]
pub enum DeployToken {
    Evm {
        chain_kind: ChainKind,
        tx_hash: TxHash,
        creation_timestamp: i64,
        expected_finalization_time: i64,
    },
    Solana {
        emitter: String,
        sequence: u64,
        #[serde(default = "default_sol_chain_kind")]
        chain_kind: ChainKind,
    },
    Starknet {
        tx_hash: String,
    },
    Aptos {
        tx_hash: String,
    },
}

impl DeployToken {
    /// The chain the token is being deployed on, for the `chain` metric label.
    /// Always known, unlike the transfer-carrying events.
    fn origin_chain(&self) -> ChainKind {
        match self {
            DeployToken::Evm { chain_kind, .. } | DeployToken::Solana { chain_kind, .. } => {
                *chain_kind
            }
            DeployToken::Starknet { .. } => ChainKind::Strk,
            DeployToken::Aptos { .. } => ChainKind::Aptos,
        }
    }

    fn log_context(&self) -> LogContext {
        let (kind, tx) = match self {
            DeployToken::Evm { tx_hash, .. } => ("DeployToken::Evm", Some(tx_hash.to_string())),
            DeployToken::Solana { .. } => ("DeployToken::Solana", None),
            DeployToken::Starknet { tx_hash } => ("DeployToken::Starknet", Some(tx_hash.clone())),
            DeployToken::Aptos { tx_hash } => ("DeployToken::Aptos", Some(tx_hash.clone())),
        };
        LogContext {
            transfer_id: None,
            kind,
            tx,
        }
    }
}

/// The `reason` and `target_chain` labels for a retry caused by a worker error.
fn stall_reason_for(
    err: &anyhow::Error,
    origin_chain: Option<ChainKind>,
) -> (&'static str, Option<ChainKind>) {
    match err.downcast_ref::<BridgeSdkError>() {
        Some(BridgeSdkError::NearRpcError(_)) => (stall_reason::NEAR_RPC, Some(ChainKind::Near)),
        Some(BridgeSdkError::EthRpcError(_)) => (stall_reason::EVM_RPC, origin_chain),
        Some(BridgeSdkError::EvmGasEstimateError(_)) => {
            (stall_reason::EVM_GAS_ESTIMATE, origin_chain)
        }
        Some(BridgeSdkError::MpcFinalityNotReached) => (stall_reason::MPC_FINALITY, origin_chain),
        Some(BridgeSdkError::LightClientNotSynced { .. }) => {
            (stall_reason::LIGHT_CLIENT_NOT_SYNCED, origin_chain)
        }
        Some(BridgeSdkError::UtxoRpcError(_) | BridgeSdkError::UtxoClientError(_)) => {
            (stall_reason::UTXO_RPC, origin_chain)
        }
        Some(BridgeSdkError::InsufficientUTXOBalance) => (stall_reason::UTXO_BALANCE, origin_chain),
        Some(BridgeSdkError::InsufficientUTXOGasFee(_)) => (stall_reason::UTXO_FEE, origin_chain),
        _ => (stall_reason::OTHER, origin_chain),
    }
}

/// Records exactly one disposition for a message: `outcome` when the
/// acknowledgement reached the server, or [`event_outcome::DROPPED_UNACKED`]
/// when it did not. An unacknowledged message stays on the stream and is
/// redelivered, so the intended disposition never took effect and reporting it
/// would overstate both throughput and permanent drops.
///
/// Returns whether the acknowledgement landed.
fn record_ack(
    ack: &std::result::Result<(), async_nats::Error>,
    origin_chain: Option<ChainKind>,
    outcome: &'static str,
) -> bool {
    let metrics = Metrics::global();
    match ack {
        Ok(()) => {
            metrics.record_event(origin_chain, outcome);
            true
        }
        Err(err) => {
            warn!("Failed to acknowledge message (intended {outcome}): {err:?}");
            metrics.record_event(origin_chain, event_outcome::DROPPED_UNACKED);
            false
        }
    }
}

/// Acknowledges the NATS message according to the worker result and message age.
/// Returns `true` if the event left the queue (acked or terminated), `false` if
/// it was re-queued for retry — the caller uses this to clean up terminal state.
/// If `msg.info()` is unavailable, nothing is acked and NATS redelivers after
/// `ack_wait`; this returns `false` (not terminal).
async fn handle_nats_ack(
    msg: &async_nats::jetstream::message::Message,
    result: &Result<EventAction>,
    config: &config::RelayerConsumer,
    origin_chain: Option<ChainKind>,
) -> bool {
    if let Ok(EventAction::Remove) = result {
        // Terminal only if the ack actually landed; otherwise NATS may redeliver,
        // so the caller must keep any terminal-only state (e.g. the fee cache).
        let ack = msg.ack().await;
        return record_ack(&ack, origin_chain, event_outcome::DONE);
    }

    if let Err(err) = result {
        warn!("Worker returned error: {err:?}");
    }

    let metrics = Metrics::global();
    let max_backoff = Duration::from_secs(config.max_backoff_hours.saturating_mul(3600));
    let max_message_age = Duration::from_secs(config.max_message_age_hours.saturating_mul(3600));

    if let Ok(info) = msg.info() {
        let now = chrono::Utc::now().timestamp();
        let published_at = info.published.unix_timestamp();
        let age = Duration::from_secs(now.saturating_sub(published_at).unsigned_abs());
        let delivered = u32::try_from(info.delivered).unwrap_or(u32::MAX);

        // Ack/Term are terminal only if the ack landed; a failed Nak (or failed
        // Ack/Term) means NATS will redeliver, so we are not done with the event.
        return match compute_ack_decision(result, age, delivered, max_backoff, max_message_age) {
            NatsAckDecision::Ack => {
                let ack = msg.ack().await;
                record_ack(&ack, origin_chain, event_outcome::DONE)
            }
            NatsAckDecision::Term => {
                warn!("Message exceeded max age ({age:?}), terminating");
                metrics.record_message_age(origin_chain, age);
                let ack = msg.ack_with(async_nats::jetstream::AckKind::Term).await;
                record_ack(&ack, origin_chain, event_outcome::DROPPED_MAX_AGE)
            }
            NatsAckDecision::NakWithBackoff(backoff) => {
                // `RetryAfter` is the scheduled finality wait, which fires on
                // essentially every transfer. Counted apart from `RETRY` so that
                // the latter stays a usable stall signal.
                let outcome = if matches!(result, Ok(EventAction::RetryAfter(_))) {
                    event_outcome::RETRY_SCHEDULED
                } else {
                    metrics.record_message_age(origin_chain, age);
                    if let Err(err) = result {
                        let (reason, target_chain) = stall_reason_for(err, origin_chain);
                        metrics.record_stalled_retry(reason, target_chain);
                    }
                    event_outcome::RETRY
                };
                let ack = msg
                    .ack_with(async_nats::jetstream::AckKind::Nak(Some(backoff)))
                    .await;
                record_ack(&ack, origin_chain, outcome);
                false
            }
        };
    }

    // msg.info() failed: no ack sent, NATS will redeliver after ack-wait — not terminal.
    metrics.record_event(origin_chain, event_outcome::DROPPED_UNACKED);
    false
}

#[allow(clippy::too_many_arguments)]
pub async fn process_events(
    config: Arc<config::Config>,
    redis_connection_manager: redis::aio::ConnectionManager,
    nats_client: Arc<utils::nats::NatsClient>,
    omni_connector: Arc<OmniConnector>,
    fast_connector: Arc<OmniConnector>,
    jsonrpc_client: JsonRpcClient,
    near_omni_nonce: Arc<utils::nonce::NonceManager>,
    near_fast_nonce: Option<Arc<utils::nonce::NonceManager>>,
    evm_nonces: Arc<utils::nonce::EvmNonceManagers>,
) -> Result<()> {
    let signer = omni_connector
        .near_bridge_client()
        .and_then(near_bridge_client::NearBridgeClient::account_id)?;

    near_omni_nonce
        .resync_nonce()
        .await
        .context("Failed to resync near nonce")?;

    if let Some(near_fast_nonce) = near_fast_nonce.clone() {
        near_fast_nonce
            .resync_nonce()
            .await
            .context("Failed to resync near fast nonce")?;
    }

    let is_evm_nonce_resync_needed = Arc::new(AtomicBool::new(true));

    let nats_config = config
        .nats
        .as_ref()
        .context("NATS config is required for event processing")?;

    let semaphore = Arc::new(tokio::sync::Semaphore::new(
        nats_config.relayer_consumer.worker_count,
    ));
    crate::metrics::register_workers_in_flight(
        &semaphore,
        nats_config.relayer_consumer.worker_count,
    );

    info!(
        "Starting event processing with {} concurrent workers",
        nats_config.relayer_consumer.worker_count
    );

    let consumer = nats_client.relayer_consumer(nats_config).await?;
    let mut messages = consumer
        .stream()
        .max_messages_per_batch(nats_config.relayer_consumer.worker_count.div_ceil(2))
        .messages()
        .await
        .context("Failed to start consuming NATS messages")?;

    while let Some(msg) = messages.next().await {
        let msg = msg.context("NATS message error")?;

        if is_evm_nonce_resync_needed.load(Ordering::Relaxed) {
            if let Err(err) = evm_nonces.resync_nonces().await {
                warn!("Failed to resync evm nonces: {err:?}");
                Metrics::global().record_event(None, event_outcome::DROPPED_UNACKED);
                continue;
            }
            is_evm_nonce_resync_needed.store(false, Ordering::Relaxed);
        }

        let permit = semaphore.clone().acquire_owned().await?;

        let event: serde_json::Value = match serde_json::from_slice(&msg.payload) {
            Ok(e) => e,
            Err(err) => {
                warn!("Failed to deserialize event: {err:?}");
                let ack = msg.ack_with(async_nats::jetstream::AckKind::Term).await;
                record_ack(&ack, None, event_outcome::DROPPED_UNDECODABLE);
                drop(permit);
                continue;
            }
        };

        let consumer_config = nats_config.relayer_consumer.clone();
        let config = config.clone();
        let mut redis = redis_connection_manager.clone();
        let jsonrpc_client = jsonrpc_client.clone();
        let omni_connector = omni_connector.clone();
        let fast_connector = fast_connector.clone();
        let signer = signer.clone();
        let near_omni_nonce = near_omni_nonce.clone();
        let near_fast_nonce = near_fast_nonce.clone();
        let evm_nonces = evm_nonces.clone();
        let nats_client = nats_client.clone();
        let is_evm_nonce_resync_needed = is_evm_nonce_resync_needed.clone();

        let span = tracing::info_span!(
            "transfer",
            topic = %msg.subject,
            transfer_id = tracing::field::Empty,
            kind = tracing::field::Empty,
            tx = tracing::field::Empty,
        );

        tokio::spawn(
            async move {
                msg.ack_with(async_nats::jetstream::AckKind::Progress)
                    .await
                    .ok();

                let message_result = process_message(
                    event,
                    &config,
                    &mut redis,
                    &jsonrpc_client,
                    omni_connector,
                    fast_connector,
                    signer,
                    near_omni_nonce,
                    near_fast_nonce,
                    evm_nonces,
                )
                .await;

                if let Err(ref err) = message_result.action {
                    warn!("{err:?}");
                }

                if message_result.needs_evm_nonce_resync
                    && matches!(
                        message_result.action,
                        Ok(EventAction::Retry | EventAction::RetryAfter(_)) | Err(_)
                    )
                {
                    is_evm_nonce_resync_needed.store(true, Ordering::Relaxed);
                }

                for event in &message_result.produced_events {
                    publish_event(&config, &nats_client, event).await;
                }

                let left_queue = handle_nats_ack(
                    &msg,
                    &message_result.action,
                    &consumer_config,
                    message_result.origin_chain,
                )
                .await;

                // Clean up the cached required-fee only when the event leaves the queue
                // (acked on Remove, or terminated by max age). Retries keep it so the fee
                // check doesn't re-fetch and re-store the entry on every attempt.
                if left_queue && let Some(ref fee_key) = message_result.fee_key {
                    utils::redis::remove_event(
                        &config,
                        &mut redis,
                        utils::redis::FEE_MAPPING,
                        fee_key,
                    )
                    .await;
                }

                drop(permit);
            }
            .instrument(span),
        );
    }

    Ok(())
}

#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
async fn process_message(
    event: serde_json::Value,
    config: &config::Config,
    redis: &mut redis::aio::ConnectionManager,
    jsonrpc_client: &JsonRpcClient,
    omni_connector: Arc<OmniConnector>,
    fast_connector: Arc<OmniConnector>,
    signer: AccountId,
    near_omni_nonce: Arc<utils::nonce::NonceManager>,
    near_fast_nonce: Option<Arc<utils::nonce::NonceManager>>,
    evm_nonces: Arc<utils::nonce::EvmNonceManagers>,
) -> MessageResult {
    if let Ok(transfer) = serde_json::from_value::<Transfer>(event.clone()) {
        transfer.log_context().record();
        let origin_chain = Some(transfer.origin_chain());

        match transfer {
            Transfer::Near { .. } | Transfer::Utxo { .. } => {
                let (is_utxo, fee_key) = match &transfer {
                    Transfer::Near {
                        transfer_message, ..
                    } => (
                        transfer_message.recipient.is_utxo_chain(),
                        serde_json::to_string(&transfer_message.get_transfer_id())
                            .unwrap_or_default(),
                    ),
                    Transfer::Utxo {
                        utxo_transfer_message,
                        new_transfer_id,
                    } => (
                        utxo_transfer_message.recipient.is_utxo_chain(),
                        serde_json::to_string(new_transfer_id).unwrap_or_default(),
                    ),
                    _ => unreachable!(),
                };

                let result = if is_utxo {
                    near::process_transfer_to_utxo_event(
                        config,
                        jsonrpc_client,
                        omni_connector.clone(),
                        transfer,
                        near_omni_nonce.clone(),
                    )
                    .await
                } else {
                    near::process_transfer_event(
                        config,
                        redis,
                        jsonrpc_client,
                        omni_connector.clone(),
                        signer.clone(),
                        transfer,
                        near_omni_nonce.clone(),
                    )
                    .await
                };

                let (action, produced_events) = match result {
                    Ok((action, emitted)) => (Ok(action), emitted),
                    Err(err) => (Err(err), Vec::new()),
                };

                MessageResult {
                    action,
                    needs_evm_nonce_resync: false,
                    fee_key: Some(fee_key),
                    produced_events,
                    origin_chain,
                }
            }
            Transfer::Evm {
                ref log,
                chain_kind,
                ..
            } => {
                let fee_key = serde_json::to_string(&TransferId {
                    origin_nonce: log.origin_nonce,
                    origin_chain: chain_kind,
                })
                .unwrap_or_default();

                let result = evm::process_init_transfer_event(
                    config,
                    redis,
                    jsonrpc_client,
                    omni_connector.clone(),
                    signer,
                    transfer,
                    near_omni_nonce.clone(),
                )
                .await;

                MessageResult {
                    action: result,
                    needs_evm_nonce_resync: false,
                    fee_key: Some(fee_key),
                    produced_events: Vec::new(),
                    origin_chain,
                }
            }
            Transfer::Solana {
                sequence,
                ref sender,
                ..
            } => {
                let sender_chain = sender.get_chain();
                let result = solana::process_init_transfer_event(
                    config,
                    redis,
                    jsonrpc_client,
                    omni_connector.clone(),
                    signer,
                    transfer,
                    near_omni_nonce.clone(),
                )
                .await;

                let fee_key = serde_json::to_string(&TransferId {
                    origin_nonce: sequence,
                    origin_chain: sender_chain,
                })
                .unwrap_or_default();

                MessageResult {
                    action: result,
                    needs_evm_nonce_resync: false,
                    fee_key: Some(fee_key),
                    produced_events: Vec::new(),
                    origin_chain,
                }
            }
            Transfer::NearToUtxo { .. } => {
                let result = utxo::process_near_to_utxo_init_transfer_event(
                    config,
                    redis,
                    jsonrpc_client,
                    omni_connector.clone(),
                    signer.clone(),
                    transfer,
                    near_omni_nonce.clone(),
                )
                .await;
                MessageResult {
                    action: result,
                    needs_evm_nonce_resync: false,
                    fee_key: None,
                    produced_events: Vec::new(),
                    origin_chain,
                }
            }
            Transfer::UtxoToNear { .. } => {
                let result = utxo::process_utxo_to_near_init_transfer_event(
                    config,
                    redis,
                    jsonrpc_client,
                    omni_connector.clone(),
                    transfer,
                    near_omni_nonce.clone(),
                )
                .await;
                MessageResult {
                    action: result,
                    needs_evm_nonce_resync: false,
                    fee_key: None,
                    produced_events: Vec::new(),
                    origin_chain,
                }
            }
            Transfer::Starknet { origin_nonce, .. } => {
                let result = starknet::process_init_transfer_event(
                    config,
                    redis,
                    jsonrpc_client,
                    omni_connector,
                    signer,
                    transfer,
                    near_omni_nonce.clone(),
                )
                .await;

                let fee_key = serde_json::to_string(&TransferId {
                    origin_nonce,
                    origin_chain: ChainKind::Strk,
                })
                .unwrap_or_default();

                MessageResult {
                    action: result,
                    needs_evm_nonce_resync: false,
                    fee_key: Some(fee_key),
                    produced_events: Vec::new(),
                    origin_chain,
                }
            }
            Transfer::Aptos { origin_nonce, .. } => {
                let result = aptos::process_init_transfer_event(
                    config,
                    redis,
                    jsonrpc_client,
                    omni_connector,
                    signer,
                    transfer,
                    near_omni_nonce.clone(),
                )
                .await;

                let fee_key = serde_json::to_string(&TransferId {
                    origin_nonce,
                    origin_chain: ChainKind::Aptos,
                })
                .unwrap_or_default();

                MessageResult {
                    action: result,
                    needs_evm_nonce_resync: false,
                    fee_key: Some(fee_key),
                    produced_events: Vec::new(),
                    origin_chain,
                }
            }
            Transfer::Fast { .. } => {
                let Some(near_fast_nonce) = near_fast_nonce.clone() else {
                    warn!("Fast transfer event but fast nonce manager not configured, removing");
                    return MessageResult {
                        action: Ok(EventAction::Remove),
                        needs_evm_nonce_resync: false,
                        fee_key: None,
                        produced_events: Vec::new(),
                        origin_chain,
                    };
                };

                let result = near::initiate_fast_transfer(
                    config,
                    jsonrpc_client,
                    fast_connector.clone(),
                    transfer,
                    near_fast_nonce,
                )
                .await;
                MessageResult {
                    action: result,
                    needs_evm_nonce_resync: false,
                    fee_key: None,
                    produced_events: Vec::new(),
                    origin_chain,
                }
            }
        }
    } else if let Ok(omni_bridge_event) = serde_json::from_value::<OmniBridgeEvent>(event.clone()) {
        if let OmniBridgeEvent::SignTransferEvent {
            ref message_payload,
            ..
        } = omni_bridge_event
        {
            LogContext {
                transfer_id: Some(message_payload.transfer_id.into()),
                kind: "OmniBridgeEvent::SignTransferEvent",
                tx: None,
            }
            .record();

            let is_evm = message_payload.recipient.get_chain().is_evm_chain();
            let origin_chain = Some(message_payload.transfer_id.origin_chain);
            let fee_key = serde_json::to_string(&message_payload.transfer_id).unwrap_or_default();

            let result = near::process_sign_transfer_event(
                config,
                redis,
                omni_connector.clone(),
                signer.clone(),
                omni_bridge_event,
                evm_nonces.clone(),
            )
            .await;

            MessageResult {
                action: result,
                needs_evm_nonce_resync: is_evm,
                fee_key: Some(fee_key),
                produced_events: Vec::new(),
                origin_chain,
            }
        } else {
            warn!("Unhandled OmniBridgeEvent, removing: {event}");
            MessageResult {
                action: Ok(EventAction::Remove),
                needs_evm_nonce_resync: false,
                fee_key: None,
                produced_events: Vec::new(),
                origin_chain: None,
            }
        }
    } else if let Ok(fin_transfer_event) = serde_json::from_value::<FinTransfer>(event.clone()) {
        fin_transfer_event.log_context().record();
        let origin_chain = fin_transfer_event.origin_chain();

        let result = match fin_transfer_event {
            FinTransfer::Evm { .. } => {
                evm::process_evm_transfer_event(
                    jsonrpc_client,
                    omni_connector.clone(),
                    signer,
                    fin_transfer_event,
                    near_omni_nonce.clone(),
                )
                .await
            }
            FinTransfer::Solana { .. } => {
                solana::process_fin_transfer_event(
                    config,
                    jsonrpc_client,
                    omni_connector.clone(),
                    signer,
                    fin_transfer_event,
                    near_omni_nonce.clone(),
                )
                .await
            }
            FinTransfer::Starknet { .. } => {
                starknet::process_fin_transfer_event(
                    jsonrpc_client,
                    omni_connector.clone(),
                    signer,
                    fin_transfer_event,
                    near_omni_nonce,
                )
                .await
            }
            FinTransfer::Aptos { .. } => {
                aptos::process_fin_transfer_event(
                    jsonrpc_client,
                    omni_connector.clone(),
                    signer,
                    fin_transfer_event,
                    near_omni_nonce,
                )
                .await
            }
        };
        MessageResult {
            action: result,
            needs_evm_nonce_resync: false,
            fee_key: None,
            produced_events: Vec::new(),
            origin_chain,
        }
    } else if let Ok(deploy_token_event) = serde_json::from_value::<DeployToken>(event.clone()) {
        deploy_token_event.log_context().record();
        let origin_chain = Some(deploy_token_event.origin_chain());

        let result = match deploy_token_event {
            DeployToken::Evm { .. } => {
                evm::process_deploy_token_event(
                    jsonrpc_client,
                    omni_connector.clone(),
                    signer.clone(),
                    deploy_token_event,
                    near_omni_nonce.clone(),
                )
                .await
            }
            DeployToken::Solana { .. } => {
                solana::process_deploy_token_event(
                    config,
                    jsonrpc_client,
                    omni_connector.clone(),
                    signer.clone(),
                    deploy_token_event,
                    near_omni_nonce.clone(),
                )
                .await
            }
            DeployToken::Starknet { .. } => {
                starknet::process_deploy_token_event(
                    jsonrpc_client,
                    omni_connector.clone(),
                    signer.clone(),
                    deploy_token_event,
                    near_omni_nonce.clone(),
                )
                .await
            }
            DeployToken::Aptos { .. } => {
                aptos::process_deploy_token_event(
                    omni_connector.clone(),
                    deploy_token_event,
                    near_omni_nonce.clone(),
                )
                .await
            }
        };
        MessageResult {
            action: result,
            needs_evm_nonce_resync: false,
            fee_key: None,
            produced_events: Vec::new(),
            origin_chain,
        }
    } else if let Ok(sign_utxo_transaction_event) =
        serde_json::from_value::<utxo::SignUtxoTransaction>(event.clone())
    {
        LogContext {
            transfer_id: None,
            kind: "SignUtxoTransaction",
            tx: sign_utxo_transaction_event.btc_pending_id.clone(),
        }
        .record();
        let origin_chain = Some(ChainKind::Near);

        let result = utxo::process_sign_transaction_event(
            config,
            redis,
            omni_connector.clone(),
            sign_utxo_transaction_event,
        )
        .await;
        MessageResult {
            action: result,
            needs_evm_nonce_resync: false,
            fee_key: None,
            produced_events: Vec::new(),
            origin_chain,
        }
    } else if let Ok(confirmed_tx_hash) =
        serde_json::from_value::<utxo::ConfirmedTxHash>(event.clone())
    {
        LogContext {
            transfer_id: None,
            kind: "ConfirmedTxHash",
            tx: Some(confirmed_tx_hash.btc_tx_hash.clone()),
        }
        .record();

        let origin_chain = Some(ChainKind::Near);

        let result = utxo::process_confirmed_tx_hash(
            config,
            redis,
            jsonrpc_client,
            omni_connector.clone(),
            confirmed_tx_hash,
            near_omni_nonce.clone(),
        )
        .await;
        MessageResult {
            action: result,
            needs_evm_nonce_resync: false,
            fee_key: None,
            produced_events: Vec::new(),
            origin_chain,
        }
    } else {
        warn!("Unknown event type, removing: {event}");
        MessageResult {
            action: Ok(EventAction::Remove),
            needs_evm_nonce_resync: false,
            fee_key: None,
            produced_events: Vec::new(),
            origin_chain: None,
        }
    }
}

struct PublishInfo {
    subject_chain: ChainKind,
    key: String,
    payload: Vec<u8>,
}

impl WorkerEvent {
    fn publish_info(&self) -> Option<PublishInfo> {
        match self {
            WorkerEvent::OmniBridge(event) => {
                let OmniBridgeEvent::SignTransferEvent {
                    message_payload,
                    signature,
                } = event.as_ref()
                else {
                    return None;
                };

                let payload = serde_json::to_vec(event.as_ref())
                    .expect("SignTransferEvent serialization cannot fail");
                let signature_hash = hex::encode(Sha256::digest(signature.to_bytes()));
                Some(PublishInfo {
                    subject_chain: message_payload.recipient.get_chain(),
                    key: format!("sign:{signature_hash}"),
                    payload,
                })
            }
            WorkerEvent::NearToUtxo(transfer) => {
                let Transfer::NearToUtxo {
                    btc_pending_id,
                    sign_index,
                    ..
                } = transfer.as_ref()
                else {
                    return None;
                };
                let payload = serde_json::to_vec(transfer.as_ref())
                    .expect("NearToUtxo transfer serialization cannot fail");
                Some(PublishInfo {
                    subject_chain: ChainKind::Near,
                    key: format!("{btc_pending_id}:{sign_index}"),
                    payload,
                })
            }
        }
    }
}

async fn publish_event(
    config: &config::Config,
    nats_client: &utils::nats::NatsClient,
    event: &WorkerEvent,
) {
    let Some(nats_config) = config.nats.as_ref() else {
        return;
    };

    let Some(info) = event.publish_info() else {
        return;
    };

    let chain = info.subject_chain.as_ref().to_ascii_lowercase();
    let subject = format!("{}.{chain}", nats_config.relayer_subject);
    if let Err(err) = nats_client.publish(subject, &info.key, info.payload).await {
        warn!("Failed to publish produced event to NATS: {err:?}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn make_decision(
        result: &Result<EventAction>,
        age_secs: u64,
        delivered: u32,
    ) -> NatsAckDecision {
        compute_ack_decision(
            result,
            Duration::from_secs(age_secs),
            delivered,
            Duration::from_hours(1),      // max_backoff = 1h
            Duration::from_hours(7 * 24), // max_message_age = 7 days
        )
    }

    #[test]
    fn stall_reason_survives_worker_context() {
        // The workers attach their message with `.context(...)`; the typed SDK
        // error must still be recoverable here or every retry reads as "other".
        let err = Err::<(), _>(BridgeSdkError::MpcFinalityNotReached)
            .context("Failed to finalize transfer (Eth:7)")
            .unwrap_err();
        assert_eq!(
            stall_reason_for(&err, Some(ChainKind::Eth)),
            (stall_reason::MPC_FINALITY, Some(ChainKind::Eth))
        );
    }

    #[test]
    fn stall_reason_reads_struct_variants() {
        let err = Err::<(), _>(BridgeSdkError::LightClientNotSynced {
            current_height: 10,
            target_height: 42,
        })
        .context("Failed to finalize Btc->NEAR transfer")
        .unwrap_err();
        assert_eq!(
            stall_reason_for(&err, Some(ChainKind::Btc)),
            (stall_reason::LIGHT_CLIENT_NOT_SYNCED, Some(ChainKind::Btc))
        );
    }

    #[test]
    fn stall_reason_defaults_to_other_for_untyped_errors() {
        let err = anyhow::anyhow!("Near bridge client is not configured");
        assert_eq!(
            stall_reason_for(&err, Some(ChainKind::Sol)),
            (stall_reason::OTHER, Some(ChainKind::Sol))
        );
    }

    #[test]
    fn near_rpc_stalls_are_attributed_to_near_not_the_origin() {
        // `target_chain` is the unready dependency's chain: a NEAR RPC failure
        // while relaying an Eth transfer is NEAR being unavailable, not Eth.
        let err = Err::<(), _>(BridgeSdkError::NearRpcError(
            near_rpc_client::NearRpcError::NonceError,
        ))
        .context("Failed to finalize transfer (Eth:7)")
        .unwrap_err();
        assert_eq!(
            stall_reason_for(&err, Some(ChainKind::Eth)),
            (stall_reason::NEAR_RPC, Some(ChainKind::Near))
        );
    }

    #[test]
    fn err_within_age_retries() {
        let result: Result<EventAction> = Err(anyhow::anyhow!("some rpc failure"));
        assert!(matches!(
            make_decision(&result, 10, 1),
            NatsAckDecision::NakWithBackoff(_)
        ));
    }

    #[test]
    fn retry_within_age_naks() {
        let result: Result<EventAction> = Ok(EventAction::Retry);
        assert!(matches!(
            make_decision(&result, 10, 1),
            NatsAckDecision::NakWithBackoff(_)
        ));
    }

    #[test]
    fn retry_exceeded_age_terminates() {
        let result: Result<EventAction> = Ok(EventAction::Retry);
        let over_max = 8 * 24 * 3600; // 8 days > 7 days
        assert!(matches!(
            make_decision(&result, over_max, 1),
            NatsAckDecision::Term
        ));
    }

    #[test]
    fn err_exceeded_age_terminates() {
        let result: Result<EventAction> = Err(anyhow::anyhow!("old failure"));
        let over_max = 8 * 24 * 3600;
        assert!(matches!(
            make_decision(&result, over_max, 1),
            NatsAckDecision::Term
        ));
    }

    #[test]
    fn retry_after_uses_explicit_delay() {
        let delay = Duration::from_secs(30);
        let result: Result<EventAction> = Ok(EventAction::RetryAfter(delay));
        match make_decision(&result, 10, 1) {
            NatsAckDecision::NakWithBackoff(d) => assert_eq!(d, delay),
            other => panic!("expected NakWithBackoff, got {other:?}"),
        }
    }

    #[test]
    fn exponential_backoff_first_delivery() {
        // delivered=1: 3^(1-1) = 3^0 = 1 second
        let result: Result<EventAction> = Ok(EventAction::Retry);
        match make_decision(&result, 10, 1) {
            NatsAckDecision::NakWithBackoff(d) => assert_eq!(d, Duration::from_secs(1)),
            other => panic!("expected NakWithBackoff, got {other:?}"),
        }
    }

    #[test]
    fn exponential_backoff_second_delivery() {
        // delivered=2: 3^(2-1) = 3^1 = 3 seconds
        let result: Result<EventAction> = Ok(EventAction::Retry);
        match make_decision(&result, 10, 2) {
            NatsAckDecision::NakWithBackoff(d) => assert_eq!(d, Duration::from_secs(3)),
            other => panic!("expected NakWithBackoff, got {other:?}"),
        }
    }

    #[test]
    fn remove_acks() {
        // Ok(EventAction::Remove) should always result in Ack, regardless of age or delivery count
        let result: Result<EventAction> = Ok(EventAction::Remove);
        assert!(matches!(
            make_decision(&result, 10, 1),
            NatsAckDecision::Ack
        ));
    }

    #[test]
    fn exact_max_age_retries() {
        let result = Ok(EventAction::Retry);
        let max_age_secs = 7 * 24 * 3600_u64;
        // Exactly at the limit — should NOT terminate (strict > comparison)
        assert!(matches!(
            make_decision(&result, max_age_secs, 1),
            NatsAckDecision::NakWithBackoff(_)
        ));
    }

    #[test]
    fn retry_after_capped_at_max_backoff() {
        let two_hours = Duration::from_hours(2);
        let result = Ok(EventAction::RetryAfter(two_hours));
        // max_backoff in make_decision is 1h
        let decision = make_decision(&result, 0, 1);
        assert!(matches!(
            decision,
            NatsAckDecision::NakWithBackoff(d) if d == Duration::from_hours(1)
        ));
    }

    #[test]
    fn exponential_backoff_capped_at_max_backoff() {
        let result = Ok(EventAction::Retry);
        // delivered=20 → 3^19 ≈ 3.5 years >> 1h max_backoff
        let decision = make_decision(&result, 0, 20);
        assert!(matches!(
            decision,
            NatsAckDecision::NakWithBackoff(d) if d == Duration::from_hours(1)
        ));
    }
}
