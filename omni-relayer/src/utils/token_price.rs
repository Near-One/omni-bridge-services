use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use near_sdk::AccountId;
use omni_types::ChainKind;
use reqwest::{Client, Url};
use std::sync::OnceLock;
use tracing::warn;

use crate::config;
use crate::metrics::{Metrics, price_outcome};

/// Deliberately short: this lookup is a pre-flight fallback, not a dependency.
/// A price that takes longer than this to arrive is one to give up on rather
/// than hold a transfer for.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(3);

/// The value sent to SHIELD when the price is unknown: no Coingecko listing for
/// the token, the indexer being unreachable, or the bridge API not configured.
/// SHIELD's API requires `amountUsd`, so there is nothing to omit; `0` keeps the
/// incident- and address-scoped rules working while USD thresholds treat the
/// transfer as below-threshold.
const AMOUNT_USD_UNKNOWN: f64 = 0.0;

#[derive(Clone, Copy)]
struct Price {
    usd_price: f64,
    decimals: u32,
}

#[derive(Debug, serde::Deserialize)]
struct TokenPriceResponse {
    /// `None` for a token the indexer cannot price: not in its allowlist price
    /// map, or no Coingecko listing for its price address. Defaulted so a
    /// response that omits the field rather than nulling it is still read as
    /// "unpriceable" instead of failing to deserialize.
    #[serde(default)]
    usd_price: Option<f64>,
    decimals: u32,
}

fn client() -> &'static Client {
    static CLIENT: OnceLock<Client> = OnceLock::new();

    CLIENT.get_or_init(|| {
        Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .expect("Failed to build token_price reqwest client")
    })
}

pub async fn amount_usd(
    config: &config::Config,
    token_id: &AccountId,
    amount: u128,
    chain: ChainKind,
) -> f64 {
    if !config.is_bridge_api_enabled() {
        return AMOUNT_USD_UNKNOWN;
    }

    let Some(price) = price(config, token_id, chain).await else {
        return AMOUNT_USD_UNKNOWN;
    };

    usd_value(amount, price.decimals, price.usd_price)
}

fn usd_value(amount: u128, decimals: u32, usd_price: f64) -> f64 {
    #[allow(clippy::as_conversions, clippy::cast_precision_loss)]
    let amount = amount as f64;

    amount / 10_f64.powi(i32::try_from(decimals).unwrap_or(i32::MAX)) * usd_price
}

async fn price(config: &config::Config, token_id: &AccountId, chain: ChainKind) -> Option<Price> {
    let metrics = Metrics::global();

    match fetch_price(config, token_id, chain).await {
        Ok(Some(price)) => {
            metrics.record_token_price_lookup(price_outcome::PRICED, chain, token_id);
            Some(price)
        }
        Ok(None) => {
            warn!("No USD price for token {token_id}, reporting the amount as unknown to SHIELD");
            metrics.record_token_price_lookup(price_outcome::UNPRICEABLE, chain, token_id);
            None
        }
        Err(err) => {
            warn!(
                "Failed to fetch USD price for token {token_id}: {err:?}, reporting the amount as unknown to SHIELD"
            );
            metrics.record_token_price_lookup(price_outcome::UNAVAILABLE, chain, token_id);
            None
        }
    }
}

async fn fetch_price(
    config: &config::Config,
    token_id: &AccountId,
    chain: ChainKind,
) -> Result<Option<Price>> {
    let base_url = config
        .bridge_indexer
        .api_url
        .as_ref()
        .context("No api url was provided")?;

    let mut url = Url::parse(base_url)
        .context("Failed to parse bridge_indexer.api_url")?
        .join("api/v3/token-price")
        .context("Failed to build token-price API URL")?;
    url.query_pairs_mut()
        .append_pair("token", &format!("near:{token_id}"))
        .append_pair("chain", &chain.as_ref().to_lowercase());

    let response = client()
        .get(url)
        .send()
        .await
        .context("Token price request failed")?;

    // The indexer's error body names what it rejected (an unregistered token,
    // an unparseable chain), which is the part worth having in the log;
    // `error_for_status` would drop it.
    let status = response.status();
    let body = response
        .text()
        .await
        .context("Token price response body read failed")?;

    if !status.is_success() {
        return Err(anyhow!("Token price request returned {status}: {body}"));
    }

    let response: TokenPriceResponse =
        serde_json::from_str(&body).context("Failed to parse the token price response")?;

    Ok(response.usd_price.map(|usd_price| Price {
        usd_price,
        decimals: response.decimals,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usd_value_scales_by_decimals() {
        // 1.5 ETH at $2000.
        assert!((usd_value(1_500_000_000_000_000_000, 18, 2000.0) - 3000.0).abs() < 1e-6);
    }

    #[test]
    fn usd_value_of_zero_decimals_is_the_price_per_unit() {
        assert!((usd_value(7, 0, 3.5) - 24.5).abs() < 1e-6);
    }

    #[test]
    fn chain_tag_matches_the_serde_aliases() {
        fn chain_tag(chain: ChainKind) -> String {
            chain.as_ref().to_lowercase()
        }

        assert_eq!(chain_tag(ChainKind::Near), "near");
        assert_eq!(chain_tag(ChainKind::Eth), "eth");
        assert_eq!(chain_tag(ChainKind::Btc), "btc");
        assert_eq!(chain_tag(ChainKind::HyperEvm), "hlevm");
    }
}
