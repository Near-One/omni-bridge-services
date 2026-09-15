//! Sender validation: the configurable per-destination sender allowlist,
//! KYT screening, and SHIELD scope evaluation applied before a transfer is
//! relayed.
//!
//! These checks are chain-agnostic (every relaying worker uses them), so they
//! live in `utils` rather than inside any single chain worker.

use std::time::Duration;

use near_sdk::AccountId;
use omni_connector::OmniConnector;
use omni_types::{ChainKind, OmniAddress, TransferMessage};
use tracing::{info, warn};

use crate::config;
use crate::metrics::{Metrics, rejection_reason};
use crate::workers::EventAction;

use super::{kyt, shield};

const MIN_SHIELD_RETRY_DELAY: Duration = Duration::from_secs(30);

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

pub(crate) async fn check_shield_deposit(
    origin_chain: ChainKind,
    token_id: &AccountId,
    amount: u128,
    sender: &OmniAddress,
    context: &str,
) -> Option<EventAction> {
    if !config::Config::is_shield_enabled() {
        return None;
    }

    if shield::blockchain_tag(origin_chain).is_none() {
        warn!("SHIELD cannot evaluate deposit {context}: unsupported chain {origin_chain:?}");
        return None;
    }

    map_shield_decision(
        shield::evaluate_deposit(origin_chain, token_id, amount, sender).await,
        "deposit",
        origin_chain,
        context,
    )
}

pub(crate) async fn check_shield_withdrawal(
    omni_connector: &OmniConnector,
    transfer_message: &TransferMessage,
    context: &str,
) -> Option<EventAction> {
    if !config::Config::is_shield_enabled() {
        return None;
    }

    let destination_chain = transfer_message.get_destination_chain();
    if shield::blockchain_tag(destination_chain).is_none() {
        warn!(
            "SHIELD cannot evaluate withdrawal {context}: unsupported chain {destination_chain:?}"
        );
        return None;
    }

    // `TransferMessage::token` is a NEAR account id only for transfers that
    // started on NEAR. One that arrived from a foreign chain and is routed
    // onward to another one keeps its origin-chain token address, because the
    // locker builds the message straight out of the prover result
    // (`fin_transfer_callback`). Resolve it the same way the locker's own
    // `get_token_id` does.
    let token_id = if let OmniAddress::Near(token_id) = &transfer_message.token {
        token_id.clone()
    } else {
        match omni_connector
            .near_get_token_id(transfer_message.token.clone())
            .await
        {
            Ok(token_id) => token_id,
            Err(err) => {
                warn!(
                    "Failed to get token id for transfer {context}: {:?}: {err:?}",
                    transfer_message.token
                );
                return Some(EventAction::Retry);
            }
        }
    };

    map_shield_decision(
        shield::evaluate_withdrawal(
            destination_chain,
            &token_id,
            transfer_message.amount.0,
            &transfer_message.recipient,
        )
        .await,
        "withdrawal",
        destination_chain,
        context,
    )
}

fn map_shield_decision(
    decision: anyhow::Result<shield::Decision>,
    direction: &str,
    chain: ChainKind,
    context: &str,
) -> Option<EventAction> {
    let metrics = Metrics::global();

    match decision {
        Ok(shield::Decision::Allow) => None,
        Ok(shield::Decision::Block { reason }) => {
            warn!("SHIELD blocked {direction} {context} (active incident: {reason}), holding");
            metrics.record_preflight_rejection(rejection_reason::SHIELD_BLOCK, Some(chain));
            Some(EventAction::RetryAfter(MIN_SHIELD_RETRY_DELAY))
        }
        Ok(shield::Decision::Delay { delay, reason }) => {
            info!("SHIELD delayed {direction} {context} ({reason}), holding");
            metrics.record_preflight_rejection(rejection_reason::SHIELD_DELAY, Some(chain));
            Some(EventAction::RetryAfter(
                delay.unwrap_or_default().max(MIN_SHIELD_RETRY_DELAY),
            ))
        }
        Ok(shield::Decision::Approval { reason }) => {
            warn!(
                "SHIELD requires manual approval for {direction} {context} ({reason}); the relayer has no approval flow, holding"
            );
            metrics.record_preflight_rejection(rejection_reason::SHIELD_APPROVAL, Some(chain));
            Some(EventAction::RetryAfter(MIN_SHIELD_RETRY_DELAY))
        }
        Ok(shield::Decision::NotEnoughPermissions { reason }) => {
            warn!("SHIELD grants are misconfigured for {direction} {context}: {reason}, holding");
            metrics.record_preflight_rejection(rejection_reason::SHIELD_MISCONFIGURED, Some(chain));
            Some(EventAction::RetryAfter(MIN_SHIELD_RETRY_DELAY))
        }
        Err(err) => {
            warn!("SHIELD {direction} evaluation failed for {context}: {err:?}, retrying");
            metrics.record_preflight_rejection(rejection_reason::SHIELD_UNAVAILABLE, Some(chain));
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

/// Validates a transfer's sender before relaying: first the configured sender
/// allowlist (local), then KYT screening (network). Returns the `EventAction`
/// to take if the transfer must not be relayed, or `None` if it may proceed.
pub(crate) async fn validate_sender(
    config: &config::Config,
    sender: &OmniAddress,
    destination_chain: ChainKind,
    context: &str,
) -> Option<EventAction> {
    if let Some(action) = enforce_sender_allowlist(config, sender, destination_chain, context) {
        return Some(action);
    }

    check_kyt(sender, context).await
}
