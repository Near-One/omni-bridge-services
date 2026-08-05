//! Sender validation: the configurable per-destination sender allowlist,
//! KYT screening, and SHIELD scope evaluation applied before a transfer is
//! relayed.
//!
//! These checks are chain-agnostic (every relaying worker uses them), so they
//! live in `utils` rather than inside any single chain worker.

use std::time::Duration;

use omni_connector::OmniConnector;
use omni_types::{ChainKind, OmniAddress, TransferMessage};
use tracing::{info, warn};

use crate::config;
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

    match kyt::check_senders(senders).await {
        Ok(kyt::SuggestedAction::StopRelaying) => {
            warn!(
                "KYT suggested STOP_RELAYING for senders {senders:?}, rejecting transfer {context}"
            );
            Some(EventAction::Remove)
        }
        Ok(kyt::SuggestedAction::None) => None,
        Err(err) => {
            warn!("KYT check failed for {senders:?}: {err:?}, retrying");
            Some(EventAction::Retry)
        }
    }
}

pub(crate) async fn check_shield_deposit(
    origin_chain: ChainKind,
    token: &OmniAddress,
    amount: u128,
    sender_address: &str,
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
        shield::evaluate_deposit(origin_chain, token, amount, sender_address).await,
        "deposit",
        context,
    )
}

async fn check_shield_withdrawal(
    destination_chain: ChainKind,
    token: &OmniAddress,
    amount: u128,
    recipient: &str,
    context: &str,
) -> Option<EventAction> {
    if !config::Config::is_shield_enabled() {
        return None;
    }

    if shield::blockchain_tag(destination_chain).is_none() {
        warn!(
            "SHIELD cannot evaluate withdrawal {context}: unsupported chain {destination_chain:?}"
        );
        return None;
    }

    map_shield_decision(
        shield::evaluate_withdrawal(destination_chain, token, amount, recipient).await,
        "withdrawal",
        context,
    )
}

pub(crate) async fn check_shield_transfer_withdrawal(
    omni_connector: &OmniConnector,
    transfer_message: &TransferMessage,
    context: &str,
) -> Option<EventAction> {
    if !config::Config::is_shield_enabled() {
        return None;
    }

    let token_id = if let OmniAddress::Near(token_id) = transfer_message.token.clone() {
        token_id
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

    check_shield_withdrawal(
        transfer_message.get_destination_chain(),
        &OmniAddress::Near(token_id),
        transfer_message.amount.0,
        &shield::bare_address(&transfer_message.recipient),
        context,
    )
    .await
}

fn map_shield_decision(
    decision: anyhow::Result<shield::Decision>,
    direction: &str,
    context: &str,
) -> Option<EventAction> {
    match decision {
        Ok(shield::Decision::Allow) => None,
        Ok(shield::Decision::Block { reason }) => {
            warn!("SHIELD blocked {direction} {context} (active incident: {reason}), retrying");
            Some(EventAction::Retry)
        }
        Ok(shield::Decision::Delay { delay, reason }) => {
            info!("SHIELD delayed {direction} {context} ({reason}), retrying");
            Some(delay.map_or(EventAction::Retry, |delay| {
                EventAction::RetryAfter(delay.max(MIN_SHIELD_RETRY_DELAY))
            }))
        }
        Ok(shield::Decision::Approval { reason }) => {
            warn!(
                "SHIELD requires approval for {direction} {context} ({reason}); the relayer has no approval flow, retrying"
            );
            Some(EventAction::Retry)
        }
        Ok(shield::Decision::NotEnoughPermissions { reason }) => {
            warn!("SHIELD grants are misconfigured for {direction} {context}: {reason}, retrying");
            Some(EventAction::Retry)
        }
        Err(err) => {
            warn!("SHIELD {direction} evaluation failed for {context}: {err:?}, retrying");
            Some(EventAction::Retry)
        }
    }
}

/// Enforces the configured sender allowlist for `destination_chain`. Returns
/// `Some(EventAction::Remove)` (and logs) if `sender` is not allowed to bridge
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
    Some(EventAction::Remove)
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
