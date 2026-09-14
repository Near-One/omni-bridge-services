use std::sync::Arc;

use alloy::{primitives::U256, transports::RpcError as AlloyRpcError};
use anyhow::{Context, Result};
use bridge_connector_common::result::{BridgeSdkError, EthRpcError};
use tracing::{info, warn};

use omni_connector::{OmniConnector, PreInitTransferFilter};
use omni_types::ChainKind;

use crate::{
    config,
    metrics::{Metrics, stall_reason},
    utils,
};

use super::{EventAction, Transfer};

/// Phase two of the deferred `initTransfer`: the `InitTransfer` this emits
/// re-enters the pipeline through the indexer as a `Transfer::Evm`.
pub async fn process_pre_init_transfer_event(
    config: &config::Config,
    omni_connector: Arc<OmniConnector>,
    transfer: Transfer,
    evm_nonces: Arc<utils::nonce::EvmNonceManagers>,
) -> Result<EventAction> {
    let Transfer::HyperEvmPreInit {
        origin_nonce,
        token_address,
        sender,
        core_nonce,
        amount,
        fee,
        ref recipient,
        ref message,
        tx_hash,
        ..
    } = transfer
    else {
        warn!("Routing mismatch, dropping: {transfer:?}");
        return Ok(EventAction::Drop);
    };

    info!("Processing PreInitTransfer (HyperEvm:{origin_nonce}): {tx_hash:?}");

    let evm_bridge_client = omni_connector
        .evm_bridge_client(ChainKind::HyperEvm)
        .context("Failed to get HyperEvm bridge client")?;

    // Zero commitment means already submitted (the call is permissionless, so
    // by anyone) or rolled back with its block.
    match evm_bridge_client
        .is_init_transfer_pending(origin_nonce)
        .await
    {
        Ok(true) => {}
        Ok(false) => {
            info!("Nothing pending, removing: HyperEvm:{origin_nonce}");
            return Ok(EventAction::Remove);
        }
        Err(err) => {
            warn!("Failed to read the pending commitment, retrying: {err:?}");
            Metrics::global()
                .record_stalled_retry(stall_reason::EVM_RPC, Some(ChainKind::HyperEvm));
            return Ok(EventAction::Retry);
        }
    }

    let pre_init = PreInitTransferFilter {
        origin_nonce,
        token_address,
        sender,
        core_nonce,
        amount: amount.0,
        fee: fee.0,
        recipient: recipient.clone(),
        message: message.clone(),
    };

    let nonce = evm_nonces
        .reserve_nonce(ChainKind::HyperEvm)
        .context("Failed to reserve nonce for hyperevm transaction")?;

    match omni_connector
        .hypercore_trigger_pending_init_transfer(&pre_init, Some(U256::from(nonce)))
        .await
    {
        Ok(tx_hash) => {
            info!("Submitted pending init transfer (HyperEvm:{origin_nonce}): {tx_hash:?}");
            Ok(EventAction::Remove)
        }
        Err(err) => classify_submission_error(config, err, origin_nonce),
    }
}

/// A revert the same payload would hit again is a give-up; the rest retries.
fn classify_submission_error(
    config: &config::Config,
    err: BridgeSdkError,
    origin_nonce: u64,
) -> Result<EventAction> {
    if let BridgeSdkError::EvmGasEstimateError(ref reason) = err {
        if utils::evm::is_terminal_hl_revert(reason)
            || config.hyperevm.as_ref().is_some_and(|hyperevm| {
                hyperevm
                    .error_selectors_to_remove
                    .iter()
                    .any(|selector| reason.contains(selector))
            })
        {
            warn!(
                "Failed to submit pending init transfer (non-retryable revert), dropping (HyperEvm:{origin_nonce}): {reason}"
            );
            return Ok(EventAction::Drop);
        }

        warn!("Failed to estimate gas, retrying (HyperEvm:{origin_nonce}): {reason}");
        Metrics::global()
            .record_stalled_retry(stall_reason::EVM_GAS_ESTIMATE, Some(ChainKind::HyperEvm));
        return Ok(EventAction::Retry);
    }

    if let BridgeSdkError::EthRpcError(ref eth_err) = err {
        match eth_err {
            EthRpcError::ContractError(reason) => {
                warn!("EVM contract/ABI error (non-retryable), dropping: {reason}");
                return Ok(EventAction::Drop);
            }
            EthRpcError::RpcError(AlloyRpcError::ErrorResp(payload))
                if !payload.is_retry_err()
                    && (payload.code == 3 || payload.message.contains("execution reverted")) =>
            {
                warn!(
                    "EVM execution reverted at submission (non-retryable), dropping: code={} msg={}",
                    payload.code, payload.message
                );
                return Ok(EventAction::Drop);
            }
            _ => {}
        }
    }

    Err(err).with_context(|| {
        format!("Failed to submit pending init transfer (HyperEvm:{origin_nonce})")
    })
}
