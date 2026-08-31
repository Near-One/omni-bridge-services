use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::Result;
use base64::{Engine, engine::general_purpose};
use omni_types::ChainKind;
use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Histogram};
use tokio::sync::Semaphore;
use tracing::{info, warn};

use crate::config;

const SERVICE_NAME: &str = "omni-relayer";

/// Chain label used where the origin chain is genuinely not known at the emit
/// site — e.g. a payload that failed to deserialize, so it was never classified.
pub const CHAIN_UNKNOWN: &str = "unknown";

/// Terminal disposition of a work item pulled from the relayer's NATS consumer.
pub mod event_outcome {
    /// Acked as handled: the relayer did its part — it submitted the
    /// transaction, or handed the transfer to the next stage. Note this is
    /// "the relayer is done with it", not "the transfer settled" — see
    /// [`super::receipt_outcome::REVERTED`]. A give-up is counted under
    /// [`DROPPED_TERMINAL`] instead, so this stays a usable throughput signal.
    pub const DONE: &str = "done";
    /// Nak'd for immediate retry: a real stall (dependency not ready, RPC error).
    pub const RETRY: &str = "retry";
    /// Nak'd with an explicit delay: the scheduled finality wait, which fires on
    /// essentially every transfer. Kept separate so [`RETRY`] stays alertable.
    pub const RETRY_SCHEDULED: &str = "retry_scheduled";
    /// Terminated because a worker returned `EventAction::Drop`: the relayer
    /// gave up on the event permanently and nothing downstream follows it — an
    /// unroutable payload, a deterministic revert, a compliance stop, an
    /// already-finalised duplicate. Has a healthy nonzero baseline: the
    /// "Transfer is already finalised" de-duplication path lands here. Alert on
    /// rate-of-change, not on an absolute threshold.
    pub const DROPPED_TERMINAL: &str = "dropped_terminal";
    /// Terminated for exceeding `max_message_age_hours` — a transfer the relayer
    /// gave up on entirely.
    pub const DROPPED_MAX_AGE: &str = "dropped_max_age";
    /// Terminated because the payload was not valid JSON.
    pub const DROPPED_UNDECODABLE: &str = "dropped_undecodable";
    /// Pulled, but the acknowledgement never reached the server, so the message
    /// stays on the stream and will be redelivered. Three causes: the EVM nonce
    /// resync failed and the loop restarted, the message carried no stream info
    /// to compute a backoff from, or the ack/nak/term call itself errored.
    /// In every case throughput drops while the process still looks healthy.
    pub const DROPPED_UNACKED: &str = "dropped_unacked";
}

/// Outcome of reading back the receipts of an already-broadcast NEAR transaction.
pub mod receipt_outcome {
    /// All receipts succeeded.
    pub const OK: &str = "ok";
    /// The status RPC call itself failed; will retry.
    pub const RETRY_RPC: &str = "retry_rpc";
    /// The final execution outcome was absent; will retry.
    pub const RETRY_MISSING: &str = "retry_missing";
    /// A receipt failed with a configured-retryable error; will retry.
    pub const RETRY_FAILURE: &str = "retry_failure";
    /// A receipt failed non-retryably: the transaction was broadcast but the
    /// contract call reverted. The caller converts this to `EventAction::Drop`,
    /// so it is also counted under [`super::event_outcome::DROPPED_TERMINAL`];
    /// this counter is what separates a revert from the other give-ups.
    pub const REVERTED: &str = "reverted";
}

/// Why a work item is being retried because an external dependency is not ready.
/// These paths retry indefinitely, so they show up as a throughput collapse
/// rather than as an error rate.
pub mod stall_reason {
    /// Wormhole VAA not yet observable.
    pub const VAA_NOT_READY: &str = "vaa_not_ready";
    /// Destination light client has not reached the required height.
    pub const LIGHT_CLIENT_NOT_SYNCED: &str = "light_client_not_synced";
    /// MPC signing finality not yet reached.
    pub const MPC_FINALITY: &str = "mpc_finality";
    /// EVM RPC provider returned an error.
    pub const EVM_RPC: &str = "evm_rpc";
    /// EVM gas estimation failed. Also how an underfunded EVM signer surfaces.
    pub const EVM_GAS_ESTIMATE: &str = "evm_gas_estimate";
    /// The bridge contract does not hold enough UTXOs to cover the withdrawal.
    pub const UTXO_BALANCE: &str = "utxo_balance";
    /// UTXO fee rate too high, or fee rate estimation failed.
    pub const UTXO_FEE: &str = "utxo_fee";
    /// The BTC/Zcash node rejected the broadcast.
    pub const UTXO_RPC: &str = "utxo_rpc";
    /// The Solana bridge contract is paused.
    pub const SOLANA_PAUSED: &str = "solana_paused";
    /// The bridge program returned a custom error at preflight other than the
    /// paused flag (Anchor / SPL / System-CPI). Reflects mutable on-chain state
    /// that can clear on a later attempt, so it retries rather than drops.
    pub const SOLANA_PROGRAM_ERROR: &str = "solana_program_error";
    /// Solana preflight rejected the transaction for a transient cluster-side
    /// reason: stale blockhash, account in use, cost limits, maintenance.
    /// Self-healing, so alert on it being sustained rather than on any of it.
    pub const SOLANA_PREFLIGHT: &str = "solana_preflight";
    /// The Solana fee payer cannot cover the fee or rent. The SVM counterpart of
    /// an underfunded EVM signer surfacing as [`EVM_GAS_ESTIMATE`]: it needs a
    /// top-up and will not clear on its own.
    pub const SOLANA_FUNDING: &str = "solana_funding";
    /// A retryable NEAR RPC error. NEAR is the hub every transfer crosses, so
    /// this moving across many `target_chain` values at once is the fastest
    /// discriminator between "one chain is broken" and "NEAR is broken".
    pub const NEAR_RPC: &str = "near_rpc";
    /// Any other retryable error. Every worker error that reaches the NATS ack
    /// path unclassified lands here, so it has a nonzero baseline: a failure
    /// that is permanent but not *recognised* as permanent (a missing config
    /// section, a bad signer) retries until it ages out and counts once per
    /// delivery. Conditions a worker can recognise as permanent return
    /// `EventAction::Drop` and never reach this bucket. A *rise* here means a
    /// failure mode nobody has classified yet — it is the bucket to watch after
    /// an SDK upgrade.
    pub const OTHER: &str = "other";
}

/// Why a transfer was rejected before any chain interaction.
pub mod rejection_reason {
    /// KYT screening returned `STOP_RELAYING`. A compliance hit; the transfer is
    /// dropped permanently.
    pub const KYT_STOP: &str = "kyt_stop";
    /// The KYT provider was unreachable. An outage, not a compliance hit — this
    /// retries forever, so it stalls rather than drops.
    pub const KYT_UNAVAILABLE: &str = "kyt_unavailable";
    /// The sender is not on the allowlist for this destination chain.
    pub const ALLOWLIST_DENIED: &str = "allowlist_denied";
    /// The transfer carried no fee at all; dropped.
    pub const NO_FEE: &str = "no_fee";
    /// The fee does not cover the relay cost; parked for retry. This is the
    /// dominant source of stuck-transfer backlog.
    pub const INSUFFICIENT_FEE: &str = "insufficient_fee";
    /// The transfer id could not be serialized, so the fee check cannot proceed
    /// and the transfer is dropped permanently. Should be identically zero.
    pub const UNPROCESSABLE: &str = "unprocessable";
    /// SHIELD reported an active incident on the transfer's scope. The transfer
    /// is held, not dropped, so it resumes once the incident is resolved.
    pub const SHIELD_BLOCK: &str = "shield_block";
    /// SHIELD asked for the transfer to be delayed (a security mode, not an
    /// incident); held for the delay it returned.
    pub const SHIELD_DELAY: &str = "shield_delay";
    /// SHIELD wants the transfer approved by a human. The relayer has no
    /// approval flow, so it is held exactly like a block.
    pub const SHIELD_APPROVAL: &str = "shield_approval";
    /// The relayer's SHIELD token lacks the grants for the evaluated scope —
    /// our own misconfiguration, and the one SHIELD reason that never clears on
    /// its own. Alert on any nonzero rate.
    pub const SHIELD_MISCONFIGURED: &str = "shield_misconfigured";
    /// SHIELD was unreachable or answered with something unparseable. An
    /// outage, not a decision; retried with the standard backoff.
    pub const SHIELD_UNAVAILABLE: &str = "shield_unavailable";
}

/// Disposition of the head-of-line pending EVM transaction each fee-bumping pass.
pub mod pending_tx_outcome {
    /// The transaction was mined.
    pub const INCLUDED: &str = "included";
    /// The transaction vanished from the mempool; its source event was replayed.
    pub const MISSING_REPLAYED: &str = "missing_replayed";
    /// A replacement transaction was successfully sent at a higher fee.
    pub const BUMPED: &str = "bumped";
    /// Sending the replacement transaction failed.
    pub const BUMP_FAILED: &str = "bump_failed";
    /// No bump was made. Covers both the benign case (network fees fell below the
    /// original) and the paging case (the bump would exceed `max_fee_in_wei`).
    /// Splitting these requires `ShouldBump::No` to carry a discriminant instead
    /// of a formatted `String`; do not parse that string into a label.
    pub const NOT_BUMPED: &str = "not_bumped";
    /// The status check RPC failed.
    pub const STATUS_CHECK_FAILED: &str = "status_check_failed";
}

/// Bucket boundaries, in seconds, for how long a work item has been retrying.
/// Chosen to span 30s up to the 96h `max_message_age_hours` ceiling; the
/// OpenTelemetry defaults top out at 10000 and would leave everything past ~3h
/// in one bucket.
const AGE_BUCKETS_SECS: &[f64] = &[
    30.0, 120.0, 600.0, 1800.0, 3600.0, 10800.0, 21600.0, 43200.0, 86400.0, 172_800.0, 345_600.0,
];

/// The lowercase chain label vocabulary. Exhaustive on purpose: a new
/// `ChainKind` variant should fail the build here rather than silently land in
/// an "other" bucket. Matches the NATS subject convention
/// (`ChainKind::as_ref().to_ascii_lowercase()`), including `HyperEvm` -> `hlevm`.
#[must_use]
pub const fn chain_label(chain: ChainKind) -> &'static str {
    match chain {
        ChainKind::Eth => "eth",
        ChainKind::Near => "near",
        ChainKind::Sol => "sol",
        ChainKind::Arb => "arb",
        ChainKind::Base => "base",
        ChainKind::Bnb => "bnb",
        ChainKind::Btc => "btc",
        ChainKind::Zcash => "zcash",
        ChainKind::Pol => "pol",
        ChainKind::HyperEvm => "hlevm",
        ChainKind::Strk => "strk",
        ChainKind::Abs => "abs",
        ChainKind::Fogo => "fogo",
        ChainKind::Aptos => "aptos",
    }
}

fn optional_chain_label(chain: Option<ChainKind>) -> &'static str {
    chain.map_or(CHAIN_UNKNOWN, chain_label)
}

/// Maps the trailing segment of a NATS subject back to a chain label. The
/// consumer filter is a `>` wildcard, so an unexpected segment must be bucketed
/// rather than passed through — otherwise a stray publisher creates unbounded
/// label values.
#[must_use]
pub fn chain_label_from_subject(subject: &str) -> &'static str {
    let segment = subject.rsplit('.').next().unwrap_or_default();
    [
        ChainKind::Eth,
        ChainKind::Near,
        ChainKind::Sol,
        ChainKind::Arb,
        ChainKind::Base,
        ChainKind::Bnb,
        ChainKind::Btc,
        ChainKind::Zcash,
        ChainKind::Pol,
        ChainKind::HyperEvm,
        ChainKind::Strk,
        ChainKind::Abs,
        ChainKind::Fogo,
        ChainKind::Aptos,
    ]
    .into_iter()
    .map(chain_label)
    .find(|label| label.eq_ignore_ascii_case(segment))
    .unwrap_or(CHAIN_UNKNOWN)
}

/// Application metric handles.
pub struct Metrics {
    events: Counter<u64>,
    message_age: Histogram<f64>,
    near_tx_receipt: Counter<u64>,
    stalled_retries: Counter<u64>,
    preflight_rejections: Counter<u64>,
    nats_publish: Counter<u64>,
    evm_pending_tx: Counter<u64>,
}

static METRICS: OnceLock<Metrics> = OnceLock::new();

impl Metrics {
    fn new() -> Self {
        let meter = opentelemetry::global::meter(SERVICE_NAME);
        Self {
            events: meter
                .u64_counter("relayer_events_total")
                .with_description("Work items by origin chain and terminal disposition")
                .build(),
            message_age: meter
                .f64_histogram("relayer_message_age_seconds")
                .with_description("Age of a stalled work item when retried or given up on")
                .with_unit("s")
                .with_boundaries(AGE_BUCKETS_SECS.to_vec())
                .build(),
            near_tx_receipt: meter
                .u64_counter("relayer_near_tx_receipt_total")
                .with_description("Receipt resolution outcomes for broadcast NEAR transactions")
                .build(),
            stalled_retries: meter
                .u64_counter("relayer_stalled_retries_total")
                .with_description("Retries by unready dependency and the chain it belongs to")
                .build(),
            preflight_rejections: meter
                .u64_counter("relayer_preflight_rejections_total")
                .with_description("Transfers rejected before any chain interaction")
                .build(),
            nats_publish: meter
                .u64_counter("relayer_nats_publish_total")
                .with_description("Work items published to NATS by the relayer")
                .build(),
            evm_pending_tx: meter
                .u64_counter("relayer_evm_pending_tx_total")
                .with_description("Disposition of the head-of-line pending EVM transaction")
                .build(),
        }
    }

    /// The process-wide handles.
    ///
    /// Instruments bind to whichever meter provider is installed when this is
    /// first called, so [`init_metrics`] must run before any emit site. `main`
    /// guarantees that; `init_metrics` also forces initialization itself.
    pub fn global() -> &'static Self {
        METRICS.get_or_init(Self::new)
    }

    /// Records the terminal disposition of one work item. See [`event_outcome`].
    pub fn record_event(&self, chain: Option<ChainKind>, outcome: &'static str) {
        self.events.add(
            1,
            &[
                KeyValue::new("chain", optional_chain_label(chain)),
                KeyValue::new("outcome", outcome),
            ],
        );
    }

    /// Records how long a work item has been alive.
    ///
    /// Recorded only for real stalls (`event_outcome::RETRY`) and for the
    /// `event_outcome::DROPPED_MAX_AGE` give-up. Never for
    /// `event_outcome::RETRY_SCHEDULED` (the scheduled finality wait fires on
    /// essentially every transfer) and never for
    /// `event_outcome::DROPPED_TERMINAL` (a recognised give-up usually lands on
    /// the first delivery): including either set of near-zero samples would make
    /// every percentile track transfer volume instead of backlog.
    pub fn record_message_age(&self, chain: Option<ChainKind>, age: Duration) {
        self.message_age.record(
            age.as_secs_f64(),
            &[KeyValue::new("chain", optional_chain_label(chain))],
        );
    }

    /// Records a NEAR receipt resolution outcome. See [`receipt_outcome`].
    pub fn record_near_tx_receipt(&self, outcome: &'static str) {
        self.near_tx_receipt
            .add(1, &[KeyValue::new("outcome", outcome)]);
    }

    /// Records a retry caused by an unready dependency. See [`stall_reason`].
    pub fn record_stalled_retry(&self, reason: &'static str, chain: Option<ChainKind>) {
        self.stalled_retries.add(
            1,
            &[
                KeyValue::new("reason", reason),
                KeyValue::new("target_chain", optional_chain_label(chain)),
            ],
        );
    }

    /// Records a pre-flight rejection. See [`rejection_reason`].
    pub fn record_preflight_rejection(&self, reason: &'static str, chain: Option<ChainKind>) {
        self.preflight_rejections.add(
            1,
            &[
                KeyValue::new("reason", reason),
                KeyValue::new("chain", optional_chain_label(chain)),
            ],
        );
    }

    /// Records a NATS publish attempt.
    ///
    /// `ok` means the message was accepted by the client connection, NOT that
    /// the stream stored it: `publish_with_headers` returns a `PublishAckFuture`
    /// that the caller drops un-awaited, so a server-side rejection (stream
    /// full, no matching subject) is invisible here. Treat `outcome="err"` as a
    /// real signal and `outcome="ok"` as "submitted".
    ///
    /// Takes an already-resolved chain label (from
    /// [`chain_label_from_subject`]) rather than the subject, because the
    /// publish call consumes the subject before the outcome is known.
    pub fn record_nats_publish(&self, chain: &'static str, ok: bool) {
        self.nats_publish.add(
            1,
            &[
                KeyValue::new("target_chain", chain),
                KeyValue::new("outcome", if ok { "ok" } else { "err" }),
            ],
        );
    }

    /// Records the fate of a pending EVM transaction. See [`pending_tx_outcome`].
    pub fn record_evm_pending_tx(&self, chain: ChainKind, outcome: &'static str) {
        self.evm_pending_tx.add(
            1,
            &[
                KeyValue::new("target_chain", chain_label(chain)),
                KeyValue::new("outcome", outcome),
            ],
        );
    }
}

/// Registers the in-flight worker gauge against the event loop's semaphore.
/// Call once, right after the semaphore is built.
pub fn register_workers_in_flight(semaphore: &Arc<Semaphore>, worker_count: usize) {
    let semaphore = Arc::clone(semaphore);
    opentelemetry::global::meter(SERVICE_NAME)
        .u64_observable_gauge("relayer_workers_in_flight")
        .with_description("Event handlers currently holding a worker permit")
        .with_callback(move |observer| {
            let in_flight = worker_count.saturating_sub(semaphore.available_permits());
            observer.observe(u64::try_from(in_flight).unwrap_or(u64::MAX), &[]);
        })
        .build();
}

/// Installs the OTLP meter provider. No-ops (with a warning) when the Grafana
/// credentials are absent, which leaves every emit site bound to OpenTelemetry's
/// no-op instruments — so worker code never needs to branch on whether metrics
/// are enabled.
pub fn init_metrics(config: &config::Config) {
    let otlp_url = std::env::var("GRAFANA_OTLP_URL").ok();
    let otlp_user = std::env::var("GRAFANA_OTLP_USER").ok();
    let api_key = std::env::var("GRAFANA_CLOUD_API_KEY").ok();

    let (endpoint, auth) = if let (Some(url), Some(user), Some(key)) =
        (otlp_url, otlp_user, api_key)
    {
        let auth = format!(
            "Basic {}",
            general_purpose::STANDARD.encode(format!("{user}:{key}"))
        );
        (url, auth)
    } else {
        warn!(
            "Metrics disabled: GRAFANA_OTLP_URL, GRAFANA_OTLP_USER or GRAFANA_CLOUD_API_KEY not set"
        );
        return;
    };

    let network = config.near.network.to_string();
    let (tx, rx) = std::sync::mpsc::channel::<Result<()>>();

    // The PeriodicReader needs a live runtime handle for the lifetime of the
    // process, and it must not compete with the relayer's worker runtime, so it
    // gets its own thread and current-thread runtime (as in omni-proxy and
    // wormhole-vaa-api). `pending()` keeps that runtime alive forever.
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("metrics runtime");

        rt.block_on(async move {
            let result = (|| -> Result<()> {
                use opentelemetry_otlp::{WithExportConfig, WithHttpConfig};
                use opentelemetry_sdk::Resource;
                use opentelemetry_sdk::metrics::{PeriodicReader, SdkMeterProvider};

                let mut headers = HashMap::new();
                headers.insert("Authorization".to_owned(), auth);

                let exporter = opentelemetry_otlp::MetricExporter::builder()
                    .with_http()
                    .with_endpoint(endpoint)
                    .with_headers(headers)
                    .build()
                    .map_err(|e| anyhow::anyhow!("OTLP exporter: {e}"))?;

                let reader = PeriodicReader::builder(exporter, opentelemetry_sdk::runtime::Tokio)
                    .with_interval(Duration::from_secs(30))
                    .build();

                let mut attributes = vec![
                    KeyValue::new("service.name", SERVICE_NAME),
                    KeyValue::new("service.version", env!("CARGO_PKG_VERSION")),
                    KeyValue::new("network", network),
                ];
                if let Ok(cluster) = std::env::var("CLUSTER_NAME") {
                    attributes.push(KeyValue::new("k8s.cluster.name", cluster));
                }
                if let Ok(pod) = std::env::var("POD_NAME") {
                    attributes.push(KeyValue::new("k8s.pod.name", pod));
                }
                let resource = Resource::new(attributes);
                let provider = SdkMeterProvider::builder()
                    .with_reader(reader)
                    .with_resource(resource)
                    .build();
                opentelemetry::global::set_meter_provider(provider);
                Ok(())
            })();

            let installed = result.is_ok();
            tx.send(result).ok();
            if installed {
                // Keeps this runtime alive for the PeriodicReader's lifetime.
                // On failure there is nothing to drive, so the thread exits
                // rather than parking a runtime and an HTTP client forever.
                std::future::pending::<()>().await;
            }
        });
    });

    match rx.recv() {
        Ok(Ok(())) => {
            // Bind the instruments now, while the provider is known to be
            // installed: a later first touch would be racing nothing, but an
            // earlier one would have permanently bound no-op instruments.
            let _ = Metrics::global();
            info!("OTLP metrics enabled");
        }
        Ok(Err(e)) => warn!("Metrics disabled: failed to initialize OTLP exporter: {e}"),
        Err(_) => warn!("Metrics disabled: setup thread exited before initialization"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chain_label_matches_nats_subject_convention() {
        // The subject is built as `ChainKind::as_ref().to_ascii_lowercase()`, so
        // the label vocabulary must agree with it for every variant.
        for chain in [
            ChainKind::Eth,
            ChainKind::Near,
            ChainKind::Sol,
            ChainKind::Arb,
            ChainKind::Base,
            ChainKind::Bnb,
            ChainKind::Btc,
            ChainKind::Zcash,
            ChainKind::Pol,
            ChainKind::HyperEvm,
            ChainKind::Strk,
            ChainKind::Abs,
            ChainKind::Fogo,
            ChainKind::Aptos,
        ] {
            assert_eq!(chain_label(chain), chain.as_ref().to_ascii_lowercase());
        }
    }

    #[test]
    fn chain_label_from_subject_extracts_known_chain() {
        assert_eq!(chain_label_from_subject("relayer.tasks.eth"), "eth");
        assert_eq!(chain_label_from_subject("relayer.tasks.hlevm"), "hlevm");
    }

    #[test]
    fn chain_label_from_subject_buckets_unknown_segment() {
        // The consumer filter is a `>` wildcard, so anything may arrive here.
        assert_eq!(
            chain_label_from_subject("relayer.tasks.bogus"),
            CHAIN_UNKNOWN
        );
        assert_eq!(chain_label_from_subject("relayer.tasks"), CHAIN_UNKNOWN);
        assert_eq!(chain_label_from_subject(""), CHAIN_UNKNOWN);
    }

    #[test]
    fn optional_chain_label_falls_back_to_unknown() {
        assert_eq!(optional_chain_label(None), CHAIN_UNKNOWN);
        assert_eq!(optional_chain_label(Some(ChainKind::Near)), "near");
    }
}
