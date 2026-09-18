//! Transfer validation: the `disabled_transfers` kill switch, the configurable
//! per-destination sender allowlist and KYT screening applied before a transfer
//! is relayed.
//!
//! These checks are chain-agnostic (every relaying worker uses them), so they
//! live in `utils` rather than inside any single chain worker.

use omni_types::{ChainKind, OmniAddress};
use tracing::warn;

use crate::config;
use crate::metrics::{Metrics, rejection_reason};
use crate::workers::EventAction;

use super::kyt;

async fn check_kyt(sender: &OmniAddress, context: &str) -> Option<EventAction> {
    check_kyt_senders(std::slice::from_ref(sender), context).await
}

pub(crate) async fn check_kyt_senders(
    senders: &[OmniAddress],
    context: &str,
) -> Option<EventAction> {
    if !config::Config::is_kyt_enabled() {
        return None;
    }

    let origin_chain = senders.first().map(OmniAddress::get_chain);

    match kyt::check_senders(senders).await {
        Ok(kyt::SuggestedAction::StopRelaying) => {
            warn!(
                "KYT suggested STOP_RELAYING for senders {senders:?}, rejecting transfer {context}"
            );
            Metrics::global().record_preflight_rejection(rejection_reason::KYT_STOP, origin_chain);
            Some(EventAction::Drop)
        }
        Ok(kyt::SuggestedAction::None) => None,
        Err(err) => {
            warn!("KYT check failed for {senders:?}: {err:?}, retrying");
            Metrics::global()
                .record_preflight_rejection(rejection_reason::KYT_UNAVAILABLE, origin_chain);
            Some(EventAction::Retry)
        }
    }
}

/// Enforces the configured sender allowlist for `destination_chain`. Returns
/// `Some(EventAction::Drop)` (and logs) if `sender` is not allowed to bridge
/// to `destination_chain`, otherwise `None`.
pub(crate) fn enforce_sender_allowlist(
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
    Metrics::global()
        .record_preflight_rejection(rejection_reason::ALLOWLIST_DENIED, Some(sender.get_chain()));
    Some(EventAction::Drop)
}

/// Enforces the `disabled_transfers` kill switch. Returns
/// `Some(EventAction::Drop)` (and logs) if the route or token is disabled in
/// config, otherwise `None`. `token` may be `None` when the caller cannot
/// resolve it; only the chain rules are then applied.
pub(crate) fn enforce_transfer_enabled(
    config: &config::Config,
    source_chain: ChainKind,
    destination_chain: ChainKind,
    token: Option<&OmniAddress>,
    context: &str,
) -> Option<EventAction> {
    let reason = config.check_transfer_disabled(source_chain, destination_chain, token)?;

    let (what, metric) = match reason {
        config::DisabledReason::SourceChain => (
            format!("source chain {source_chain:?}"),
            rejection_reason::DISABLED_SOURCE_CHAIN,
        ),
        config::DisabledReason::DestinationChain => (
            format!("destination chain {destination_chain:?}"),
            rejection_reason::DISABLED_DESTINATION_CHAIN,
        ),
        config::DisabledReason::Token => (
            format!(
                "token {}",
                token.map_or_else(String::new, ToString::to_string)
            ),
            rejection_reason::DISABLED_TOKEN,
        ),
    };

    warn!("Transfers are disabled by config for {what}, dropping transfer {context}");
    Metrics::global().record_preflight_rejection(metric, Some(source_chain));
    Some(EventAction::Drop)
}

/// Validates a transfer before relaying: the `disabled_transfers` kill switch,
/// then the configured sender allowlist (local), then KYT screening (network).
/// Returns the `EventAction` to take if the transfer must not be relayed, or
/// `None` if it may proceed.
pub(crate) async fn validate_sender(
    config: &config::Config,
    sender: &OmniAddress,
    destination_chain: ChainKind,
    token: Option<&OmniAddress>,
    context: &str,
) -> Option<EventAction> {
    if let Some(action) = enforce_transfer_enabled(
        config,
        sender.get_chain(),
        destination_chain,
        token,
        context,
    ) {
        return Some(action);
    }

    if let Some(action) = enforce_sender_allowlist(config, sender, destination_chain, context) {
        return Some(action);
    }

    check_kyt(sender, context).await
}
