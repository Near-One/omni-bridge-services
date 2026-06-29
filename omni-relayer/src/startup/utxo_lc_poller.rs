use std::{sync::Arc, time::Duration};

use anyhow::{Context, Result};
use omni_connector::OmniConnector;
use omni_types::ChainKind;
use tracing::{info, warn};

use crate::{
    config,
    utils::{
        self,
        utxo::{PendingLcEvent, pending_lc_key},
    },
};

#[tracing::instrument(
    name = "utxo_lc_poller",
    skip_all,
    fields(chain = ?chain)
)]
pub async fn start_utxo_lc_poller(
    config: Arc<config::Config>,
    chain: ChainKind,
    omni_connector: Arc<OmniConnector>,
    nats_client: Arc<utils::nats::NatsClient>,
    mut redis_connection_manager: redis::aio::ConnectionManager,
) -> Result<()> {
    let nats_config = config
        .nats
        .as_ref()
        .context("NATS config is required for UTXO LC poller")?;

    let utxo_config = match chain {
        ChainKind::Btc => config.btc.as_ref(),
        ChainKind::Zcash => config.zcash.as_ref(),
        _ => anyhow::bail!("UTXO LC poller does not support {chain:?}"),
    }
    .with_context(|| format!("{chain:?} UTXO config is missing"))?;

    let redis_key = pending_lc_key(chain).context("Unsupported chain for LC poller")?;
    let interval = Duration::from_secs(utxo_config.lc_polling_interval_secs);
    let subject = format!(
        "{}.{}",
        nats_config.relayer_subject,
        ChainKind::Near.as_ref().to_ascii_lowercase()
    );

    info!(
        interval_secs = utxo_config.lc_polling_interval_secs,
        "Starting LC poller"
    );

    let utxo_set = utils::utxo::UtxoSet::global();
    let mut last_refreshed_tip: Option<u64> = None;

    loop {
        tokio::time::sleep(interval).await;

        let tip = match omni_connector.light_client(chain) {
            Ok(lc) => match lc.get_last_block_number().await {
                Ok(tip) => tip,
                Err(err) => {
                    warn!(?err, "Failed to query light client tip");
                    continue;
                }
            },
            Err(err) => {
                warn!(?err, "Failed to get light client");
                continue;
            }
        };

        if last_refreshed_tip != Some(tip) {
            utxo_set.mark_dirty(chain);
            last_refreshed_tip = Some(tip);
        }

        let Some(ready) = utils::redis::zrangebyscore::<PendingLcEvent>(
            &config,
            &mut redis_connection_manager,
            &redis_key,
            0,
            tip,
        )
        .await
        else {
            continue;
        };

        if ready.is_empty() {
            continue;
        }

        info!(
            tip,
            pending = ready.len(),
            "LC tip reached; replaying pending event(s)"
        );

        for pending in ready {
            let payload = match serde_json::to_vec(&pending.event) {
                Ok(payload) => payload,
                Err(err) => {
                    warn!(?err, "Failed to serialize pending LC event");
                    continue;
                }
            };

            if let Err(err) = nats_client
                .publish(subject.clone(), &pending.key, payload)
                .await
            {
                warn!(?err, "Failed to publish replayed LC event to NATS");
                continue;
            }

            utils::redis::zrem(&config, &mut redis_connection_manager, &redis_key, pending).await;
        }
    }
}
