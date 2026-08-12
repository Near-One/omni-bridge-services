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
    DeferUntilBlock { chain: ChainKind, target_block: u64 },
    Remove,
}

pub enum WorkerEvent {
    OmniBridge(Box<OmniBridgeEvent>),
    NearToUtxo(Box<Transfer>),
}

struct MessageResult {
    action: Result<EventAction>,
    needs_evm_nonce_resync: bool,
    fee_key_to_remove: Option<String>,
    produced_events: Vec<WorkerEvent>,
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

async fn handle_nats_ack(
    config: &config::Config,
    redis: &mut redis::aio::ConnectionManager,
    msg: &async_nats::jetstream::message::Message,
    result: &Result<EventAction>,
    consumer_config: &config::RelayerConsumer,
) {
    match result {
        Ok(EventAction::Retry) => {
            nak_with_backoff(msg, None, consumer_config).await;
        }
        Ok(EventAction::RetryAfter(delay)) => {
            nak_with_backoff(msg, Some(*delay), consumer_config).await;
        }
        Ok(EventAction::DeferUntilBlock {
            chain,
            target_block,
        }) => {
            let msg_id = msg
                .headers
                .as_ref()
                .and_then(|headers| headers.get("Nats-Msg-Id"))
                .map(async_nats::HeaderValue::as_str);
            let Some(msg_id) = msg_id else {
                warn!("Missing Nats-Msg-Id header, retrying instead of deferring");
                return nak_with_backoff(msg, None, consumer_config).await;
            };

            let (base_key, last_target) = utils::utxo::split_lc_msg_id(msg_id);
            // A target this event already waited past means the failure is
            // not about missing blocks (e.g. a lagging light client view);
            // deferring again would replay immediately under an
            // already-published msg id, so let the backoff pace it instead.
            if last_target.is_some_and(|last| *target_block <= last) {
                return nak_with_backoff(msg, None, consumer_config).await;
            }

            let event = match serde_json::from_slice::<serde_json::Value>(&msg.payload) {
                Ok(event) => event,
                Err(err) => {
                    warn!("Failed to parse payload while deferring {base_key}: {err:?}");
                    return nak_with_backoff(msg, None, consumer_config).await;
                }
            };
            match utils::utxo::store_pending_lc_event(
                config,
                redis,
                *chain,
                *target_block,
                base_key,
                &event,
            )
            .await
            {
                Ok(()) => {
                    info!("Deferred {base_key} to LC poller (target_block={target_block})");
                    msg.ack().await.ok();
                }
                Err(err) => {
                    warn!("Failed to defer {base_key} to LC poller, retrying: {err:?}");
                    nak_with_backoff(msg, None, consumer_config).await;
                }
            }
        }
        Ok(EventAction::Remove) => {
            msg.ack().await.ok();
        }
        Err(_) => {
            msg.ack_with(async_nats::jetstream::AckKind::Term)
                .await
                .ok();
        }
    }
}

async fn nak_with_backoff(
    msg: &async_nats::jetstream::message::Message,
    explicit_delay: Option<Duration>,
    consumer_config: &config::RelayerConsumer,
) {
    let max_backoff = Duration::from_secs(consumer_config.max_backoff_hours * 3600);
    let max_message_age = Duration::from_secs(consumer_config.max_message_age_hours * 3600);

    if let Ok(info) = msg.info() {
        let now = chrono::Utc::now().timestamp();
        let published_at = info.published.unix_timestamp();
        let age = Duration::from_secs(now.saturating_sub(published_at).unsigned_abs());

        if age > max_message_age {
            warn!("Message exceeded max age ({age:?}), terminating");
            msg.ack_with(async_nats::jetstream::AckKind::Term)
                .await
                .ok();
            return;
        }

        let backoff = explicit_delay
            .unwrap_or_else(|| {
                let delivered = u32::try_from(info.delivered).unwrap_or(u32::MAX);
                Duration::from_secs(3u64.saturating_pow(delivered.saturating_sub(1)))
            })
            .min(max_backoff);
        msg.ack_with(async_nats::jetstream::AckKind::Nak(Some(backoff)))
            .await
            .ok();
    }
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
                continue;
            }
            is_evm_nonce_resync_needed.store(false, Ordering::Relaxed);
        }

        let permit = semaphore.clone().acquire_owned().await?;

        let event: serde_json::Value = match serde_json::from_slice(&msg.payload) {
            Ok(e) => e,
            Err(err) => {
                warn!("Failed to deserialize event: {err:?}");
                msg.ack_with(async_nats::jetstream::AckKind::Term)
                    .await
                    .ok();
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

                if let Some(ref fee_key) = message_result.fee_key_to_remove {
                    utils::redis::remove_event(
                        &config,
                        &mut redis,
                        utils::redis::FEE_MAPPING,
                        fee_key,
                    )
                    .await;
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

                handle_nats_ack(
                    &config,
                    &mut redis,
                    &msg,
                    &message_result.action,
                    &consumer_config,
                )
                .await;

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

                let fee_key_to_remove = action.is_err().then_some(fee_key);
                MessageResult {
                    action,
                    needs_evm_nonce_resync: false,
                    fee_key_to_remove,
                    produced_events,
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

                let fee_key_to_remove =
                    matches!(&result, Ok(EventAction::Remove) | Err(_)).then_some(fee_key);
                MessageResult {
                    action: result,
                    needs_evm_nonce_resync: false,
                    fee_key_to_remove,
                    produced_events: Vec::new(),
                }
            }
            Transfer::Solana {
                sequence,
                ref sender,
                ..
            } => {
                let origin_chain = sender.get_chain();
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
                    origin_chain,
                })
                .unwrap_or_default();

                let fee_key_to_remove =
                    matches!(&result, Ok(EventAction::Remove) | Err(_)).then_some(fee_key);
                MessageResult {
                    action: result,
                    needs_evm_nonce_resync: false,
                    fee_key_to_remove,
                    produced_events: Vec::new(),
                }
            }
            Transfer::NearToUtxo { .. } => {
                let result = utxo::process_near_to_utxo_init_transfer_event(
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
                    fee_key_to_remove: None,
                    produced_events: Vec::new(),
                }
            }
            Transfer::UtxoToNear { .. } => {
                let result = utxo::process_utxo_to_near_init_transfer_event(
                    config,
                    jsonrpc_client,
                    omni_connector.clone(),
                    transfer,
                    near_omni_nonce.clone(),
                )
                .await;
                MessageResult {
                    action: result,
                    needs_evm_nonce_resync: false,
                    fee_key_to_remove: None,
                    produced_events: Vec::new(),
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

                let fee_key_to_remove =
                    matches!(&result, Ok(EventAction::Remove) | Err(_)).then_some(fee_key);
                MessageResult {
                    action: result,
                    needs_evm_nonce_resync: false,
                    fee_key_to_remove,
                    produced_events: Vec::new(),
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

                let fee_key_to_remove =
                    matches!(&result, Ok(EventAction::Remove) | Err(_)).then_some(fee_key);
                MessageResult {
                    action: result,
                    needs_evm_nonce_resync: false,
                    fee_key_to_remove,
                    produced_events: Vec::new(),
                }
            }
            Transfer::Fast { .. } => {
                let Some(near_fast_nonce) = near_fast_nonce.clone() else {
                    return MessageResult {
                        action: Err(anyhow::anyhow!(
                            "Fast transfer event found but near fast nonce manager is not configured"
                        )),
                        needs_evm_nonce_resync: false,
                        fee_key_to_remove: None,
                        produced_events: Vec::new(),
                    };
                };

                let result = near::initiate_fast_transfer(
                    config,
                    fast_connector.clone(),
                    transfer,
                    near_fast_nonce,
                )
                .await;
                MessageResult {
                    action: result,
                    needs_evm_nonce_resync: false,
                    fee_key_to_remove: None,
                    produced_events: Vec::new(),
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

            let fee_key_to_remove =
                matches!(&result, Ok(EventAction::Remove) | Err(_)).then_some(fee_key);
            MessageResult {
                action: result,
                needs_evm_nonce_resync: is_evm,
                fee_key_to_remove,
                produced_events: Vec::new(),
            }
        } else {
            MessageResult {
                action: Err(anyhow::anyhow!("Unhandled OmniBridgeEvent: {event}")),
                needs_evm_nonce_resync: false,
                fee_key_to_remove: None,
                produced_events: Vec::new(),
            }
        }
    } else if let Ok(fin_transfer_event) = serde_json::from_value::<FinTransfer>(event.clone()) {
        fin_transfer_event.log_context().record();

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
            fee_key_to_remove: None,
            produced_events: Vec::new(),
        }
    } else if let Ok(deploy_token_event) = serde_json::from_value::<DeployToken>(event.clone()) {
        deploy_token_event.log_context().record();

        let result = match deploy_token_event {
            DeployToken::Evm { .. } => {
                evm::process_deploy_token_event(
                    omni_connector.clone(),
                    deploy_token_event,
                    near_omni_nonce.clone(),
                )
                .await
            }
            DeployToken::Solana { .. } => {
                solana::process_deploy_token_event(
                    config,
                    omni_connector.clone(),
                    deploy_token_event,
                    near_omni_nonce.clone(),
                )
                .await
            }
            DeployToken::Starknet { .. } => {
                starknet::process_deploy_token_event(
                    omni_connector.clone(),
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
            fee_key_to_remove: None,
            produced_events: Vec::new(),
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
            fee_key_to_remove: None,
            produced_events: Vec::new(),
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

        let result = utxo::process_confirmed_tx_hash(
            jsonrpc_client,
            omni_connector.clone(),
            confirmed_tx_hash,
            near_omni_nonce.clone(),
        )
        .await;
        MessageResult {
            action: result,
            needs_evm_nonce_resync: false,
            fee_key_to_remove: None,
            produced_events: Vec::new(),
        }
    } else {
        MessageResult {
            action: Err(anyhow::anyhow!("Unknown event type: {event}")),
            needs_evm_nonce_resync: false,
            fee_key_to_remove: None,
            produced_events: Vec::new(),
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
