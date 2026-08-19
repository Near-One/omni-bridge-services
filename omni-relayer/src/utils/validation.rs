//! Checks applied before a transfer is relayed: the configurable allowlist
//! and KYT screening.

use omni_types::{ChainKind, OmniAddress};
use tracing::warn;

use crate::config;
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

pub(crate) fn enforce_allowlist(
    config: &config::Config,
    sender: &OmniAddress,
    recipient: &OmniAddress,
    context: &str,
) -> Option<EventAction> {
    if config.is_transfer_allowed(sender, recipient) {
        return None;
    }

    warn!(
        "Transfer from {sender} to {recipient} is not allowed by the allowlist, dropping transfer {context}"
    );

    Some(EventAction::Remove)
}

pub(crate) fn enforce_sender_allowlist(
    config: &config::Config,
    sender: &OmniAddress,
    destination_chain: ChainKind,
    context: &str,
) -> Option<EventAction> {
    if config.is_sender_possibly_allowed(sender, destination_chain) {
        return None;
    }

    warn!(
        "Sender {sender} is not allowed to bridge to {destination_chain:?}, dropping transfer {context}"
    );

    Some(EventAction::Remove)
}

pub(crate) fn enforce_recipient_allowlist(
    config: &config::Config,
    recipient: &OmniAddress,
    sender_chain: ChainKind,
    context: &str,
) -> Option<EventAction> {
    if config.is_recipient_possibly_allowed(recipient, sender_chain) {
        return None;
    }

    warn!("Recipient {recipient} is not allowed by the allowlist, dropping transfer {context}");

    Some(EventAction::Remove)
}

pub(crate) async fn validate_transfer(
    config: &config::Config,
    sender: &OmniAddress,
    recipient: &OmniAddress,
    context: &str,
) -> Option<EventAction> {
    if let Some(action) = enforce_allowlist(config, sender, recipient, context) {
        return Some(action);
    }

    check_kyt(sender, context).await
}

/// Sender-only variant of [`validate_transfer`] for paths without a recipient.
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
