use std::{sync::OnceLock, time::Duration};

use anyhow::{Context, Result, anyhow};
use omni_types::{ChainKind, OmniAddress};
use reqwest::{Client, header::HeaderMap};
use tracing::warn;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const WAIT_TIME_MS: u32 = 2000;

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
struct KytRequest {
    correlation_id: String,
    addresses: Vec<KytAddress>,
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
        ChainKind::HyperEvm | ChainKind::Abs => None,
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
    // TODO: remove once api supports >10 addresses in a single call
    let senders = &senders[..senders.len().min(10)];

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

    let body = KytRequest {
        correlation_id: uuid::Uuid::new_v4().to_string(),
        addresses: kyt_addresses,
        wait_time_ms: WAIT_TIME_MS,
    };

    let raw = client()
        .post(&url)
        .json(&body)
        .send()
        .await
        .context("KYT request failed")?
        .text()
        .await
        .context("KYT response body read failed")?;
    let resp: KytResponse = serde_json::from_str(&raw)
        .with_context(|| format!("KYT response did not match expected schema. Raw body: {raw}"))?;

    match resp.suggested_action.as_str() {
        "NONE" => Ok(SuggestedAction::None),
        "STOP_RELAYING" => Ok(SuggestedAction::StopRelaying),
        other => Err(anyhow!("Unexpected KYT suggestedAction: {other}")),
    }
}
