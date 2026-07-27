use std::{sync::OnceLock, time::Duration};

use anyhow::{Context, Result, anyhow};
use omni_types::{ChainKind, OmniAddress};
use reqwest::{Client, header::HeaderMap};
use tracing::warn;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const WAIT_TIME_MS: u32 = 2000;

const MAX_KYT_BATCH_SIZE: usize = 500;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SuggestedAction {
    None,
    StopRelaying,
}

#[derive(serde::Serialize)]
struct KytAddress {
    address: String,
    blockchain: &'static str,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct KytRequest<'a> {
    correlation_id: String,
    addresses: &'a [KytAddress],
    wait_time_ms: u32,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct KytResponse {
    suggested_action: String,
}

fn client() -> &'static Client {
    static CLIENT: OnceLock<Client> = OnceLock::new();

    CLIENT.get_or_init(|| {
        let api_key = std::env::var("KYT_API_KEY").expect("KYT_API_KEY env var is not set");

        let mut headers = HeaderMap::new();
        headers.insert(
            "x-api-key",
            api_key
                .parse()
                .expect("KYT_API_KEY is not a valid HTTP header value"),
        );

        Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .default_headers(headers)
            .build()
            .expect("Failed to build KYT reqwest client")
    })
}

fn blockchain_tag(chain: ChainKind) -> Option<&'static str> {
    match chain {
        ChainKind::Near => Some("near"),
        ChainKind::Eth => Some("eth"),
        ChainKind::Base => Some("base"),
        ChainKind::Arb => Some("arb"),
        ChainKind::Bnb => Some("bsc"),
        ChainKind::Pol => Some("pol"),
        ChainKind::Sol => Some("sol"),
        ChainKind::Strk => Some("starknet"),
        ChainKind::Btc => Some("btc"),
        ChainKind::Zcash => Some("zec"),
        ChainKind::HyperEvm | ChainKind::Abs | ChainKind::Fogo | ChainKind::Aptos => None,
    }
}

fn to_kyt_address(sender: &OmniAddress) -> Option<KytAddress> {
    let blockchain = blockchain_tag(sender.get_chain())?;
    let full = sender.to_string();
    let address = full
        .split_once(':')
        .map_or(full.clone(), |(_, a)| a.to_string());
    Some(KytAddress {
        address,
        blockchain,
    })
}

pub async fn check_senders(senders: &[OmniAddress]) -> Result<SuggestedAction> {
    let mut kyt_addresses = Vec::with_capacity(senders.len());
    let mut dropped = Vec::new();
    for sender in senders {
        match to_kyt_address(sender) {
            Some(addr) => kyt_addresses.push(addr),
            None => dropped.push(sender),
        }
    }
    if !dropped.is_empty() {
        warn!(
            "KYT cannot screen {}/{} sender(s) (no blockchain tag for their chain): {dropped:?}",
            dropped.len(),
            senders.len()
        );
    }
    if kyt_addresses.is_empty() {
        return Ok(SuggestedAction::None);
    }

    let url = std::env::var("KYT_API_URL").context("KYT_API_URL env var is not set")?;

    for batch in kyt_addresses.chunks(MAX_KYT_BATCH_SIZE) {
        if screen_batch(&url, batch).await? == SuggestedAction::StopRelaying {
            return Ok(SuggestedAction::StopRelaying);
        }
    }
    Ok(SuggestedAction::None)
}

async fn screen_batch(url: &str, addresses: &[KytAddress]) -> Result<SuggestedAction> {
    let body = KytRequest {
        correlation_id: uuid::Uuid::new_v4().to_string(),
        addresses,
        wait_time_ms: WAIT_TIME_MS,
    };

    let raw = client()
        .post(url)
        .json(&body)
        .send()
        .await
        .context("KYT request failed")?
        .text()
        .await
        .context("KYT response body read failed")?;

    suggested_action_from_response(&raw)
}

fn suggested_action_from_response(raw: &str) -> Result<SuggestedAction> {
    let resp: KytResponse = serde_json::from_str(raw)
        .with_context(|| format!("KYT response did not match expected schema. Raw body: {raw}"))?;
    match resp.suggested_action.as_str() {
        "NONE" => Ok(SuggestedAction::None),
        "STOP_RELAYING" => Ok(SuggestedAction::StopRelaying),
        other => Err(anyhow!("Unexpected KYT suggestedAction: {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suggested_action_from_response_parses_none() {
        assert_eq!(
            suggested_action_from_response(r#"{"suggestedAction":"NONE"}"#).unwrap(),
            SuggestedAction::None
        );
    }

    #[test]
    fn suggested_action_from_response_parses_stop_relaying() {
        assert_eq!(
            suggested_action_from_response(r#"{"suggestedAction":"STOP_RELAYING"}"#).unwrap(),
            SuggestedAction::StopRelaying
        );
    }

    #[test]
    fn suggested_action_from_response_ignores_unknown_fields() {
        assert_eq!(
            suggested_action_from_response(
                r#"{"suggestedAction":"NONE","correlationId":"x","extra":[1,2]}"#
            )
            .unwrap(),
            SuggestedAction::None
        );
    }

    #[test]
    fn suggested_action_from_response_errors_on_unknown_action() {
        assert!(suggested_action_from_response(r#"{"suggestedAction":"WAT"}"#).is_err());
    }

    #[test]
    fn suggested_action_from_response_errors_on_malformed_body() {
        assert!(suggested_action_from_response("not json").is_err());
        assert!(suggested_action_from_response(r#"{"noAction":true}"#).is_err());
    }

    #[test]
    fn kyt_batches_are_capped_at_max_kyt_batch_size() {
        assert_eq!(MAX_KYT_BATCH_SIZE, 500);
        let addrs: Vec<u32> = (0..1399).collect();
        let chunks: Vec<&[u32]> = addrs.chunks(MAX_KYT_BATCH_SIZE).collect();
        // 1399 -> 500 + 500 + 399
        assert_eq!(chunks.len(), 3);
        assert!(
            chunks
                .iter()
                .all(|c| !c.is_empty() && c.len() <= MAX_KYT_BATCH_SIZE)
        );
        assert_eq!(chunks.iter().map(|c| c.len()).sum::<usize>(), addrs.len());
        assert_eq!(chunks.last().unwrap().len(), 399);
    }

    #[tokio::test]
    async fn check_senders_returns_none_for_empty_input() {
        // Empty input must short-circuit before any env read or network call.
        assert_eq!(check_senders(&[]).await.unwrap(), SuggestedAction::None);
    }
}
