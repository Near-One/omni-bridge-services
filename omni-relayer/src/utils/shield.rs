//! SHIELD (proactive intents security) client.
//!
//! Before relaying a transfer the relayer asks SHIELD whether the affected
//! chain/token/address scope is operational: deposits (funds entering the
//! bridge from a foreign chain) go through `POST /evaluate/deposit`,
//! withdrawals (funds leaving towards a foreign chain) through
//! `POST /evaluate/withdrawal`. The returned decision (`allow`/`block`/
//! `delay`/`approval`) is mapped to an `EventAction` in
//! `utils::validation`.
//!
//! Enabled by setting the `SHIELD_API_URL` and `SHIELD_API_TOKEN` (a partner
//! JWT) environment variables, mirroring how KYT is toggled.

use std::{sync::OnceLock, time::Duration};

use anyhow::{Context, Result, anyhow};
use omni_types::{ChainKind, OmniAddress};
use reqwest::{Client, header::HeaderMap};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

const BRIDGE: &str = "omni";
const AMOUNT_USD_UNKNOWN: f64 = 0.0;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Block {
        reason: String,
    },
    Delay {
        delay: Option<Duration>,
        reason: String,
    },
    Approval {
        reason: String,
    },
    NotEnoughPermissions {
        reason: String,
    },
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct DepositRequest<'a> {
    blockchain: &'static str,
    bridge: &'static str,
    token: &'a str,
    amount: String,
    amount_usd: f64,
    timestamp: String,
    sender_address: &'a str,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct WithdrawalRequest<'a> {
    blockchain: &'static str,
    bridge: &'static str,
    token: &'a str,
    amount: String,
    amount_usd: f64,
    recipient: &'a str,
    timestamp: String,
}

#[derive(serde::Deserialize)]
struct EvaluateResponse {
    #[serde(rename = "type")]
    decision: String,
    #[serde(default)]
    reason: String,
    #[serde(default)]
    metadata: Option<ResponseMetadata>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ResponseMetadata {
    #[serde(default)]
    delay_ms: Option<u64>,
}

fn client() -> Result<&'static Client> {
    static CLIENT: OnceLock<Result<Client, String>> = OnceLock::new();

    CLIENT
        .get_or_init(build_client)
        .as_ref()
        .map_err(|err| anyhow!("{err}"))
}

fn build_client() -> Result<Client, String> {
    let token =
        std::env::var("SHIELD_API_TOKEN").map_err(|_| "SHIELD_API_TOKEN env var is not set")?;

    let mut headers = HeaderMap::new();
    headers.insert(
        "Authorization",
        format!("Bearer {}", token.trim())
            .parse()
            .map_err(|_| "SHIELD_API_TOKEN is not a valid HTTP header value")?,
    );

    Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .default_headers(headers)
        .build()
        .map_err(|err| format!("Failed to build SHIELD reqwest client: {err}"))
}

pub fn blockchain_tag(chain: ChainKind) -> Option<&'static str> {
    match chain {
        ChainKind::Near => Some("near"),
        ChainKind::Eth => Some("eth"),
        ChainKind::Base => Some("base"),
        ChainKind::Arb => Some("arb"),
        ChainKind::Bnb => Some("bsc"),
        ChainKind::Pol => Some("pol"),
        ChainKind::Sol => Some("sol"),
        ChainKind::Fogo => Some("fogo"),
        ChainKind::Strk => Some("starknet"),
        ChainKind::Aptos => Some("aptos"),
        ChainKind::Btc => Some("btc"),
        ChainKind::Zcash => Some("zec"),
        // SHIELD's enum has `hypercore` (the HyperLiquid L1), which is not
        // the same thing as HyperEVM; Abstract is absent entirely.
        ChainKind::HyperEvm | ChainKind::Abs => None,
    }
}

fn to_asset_id(token: &OmniAddress) -> Result<String> {
    match token {
        OmniAddress::Near(account_id) => Ok(format!("nep141:{account_id}")),
        other => Err(anyhow!(
            "Cannot build SHIELD asset id from non-NEAR token address: {other:?}"
        )),
    }
}

pub fn bare_address(address: &OmniAddress) -> String {
    let full = address.to_string();
    full.split_once(':')
        .map_or(full.clone(), |(_, addr)| addr.to_string())
}

pub async fn evaluate_deposit(
    chain: ChainKind,
    token: &OmniAddress,
    amount: u128,
    sender_address: &str,
) -> Result<Decision> {
    let blockchain =
        blockchain_tag(chain).with_context(|| format!("No SHIELD blockchain tag for {chain:?}"))?;
    let token = to_asset_id(token)?;

    let request = DepositRequest {
        blockchain,
        bridge: BRIDGE,
        token: &token,
        amount: amount.to_string(),
        amount_usd: AMOUNT_USD_UNKNOWN,
        timestamp: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        sender_address,
    };

    evaluate("deposit", &request).await
}

pub async fn evaluate_withdrawal(
    chain: ChainKind,
    token: &OmniAddress,
    amount: u128,
    recipient: &str,
) -> Result<Decision> {
    let blockchain =
        blockchain_tag(chain).with_context(|| format!("No SHIELD blockchain tag for {chain:?}"))?;
    let token = to_asset_id(token)?;

    let request = WithdrawalRequest {
        blockchain,
        bridge: BRIDGE,
        token: &token,
        amount: amount.to_string(),
        amount_usd: AMOUNT_USD_UNKNOWN,
        recipient,
        timestamp: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
    };

    evaluate("withdrawal", &request).await
}

async fn evaluate<T: serde::Serialize>(endpoint: &str, request: &T) -> Result<Decision> {
    let base_url = std::env::var("SHIELD_API_URL").context("SHIELD_API_URL env var is not set")?;
    let url = format!(
        "{}/evaluate/{endpoint}",
        base_url.trim().trim_end_matches('/')
    );

    let response = client()?
        .post(url)
        .json(request)
        .send()
        .await
        .with_context(|| format!("SHIELD {endpoint} evaluation request failed"))?;

    let status = response.status();
    let raw = response
        .text()
        .await
        .with_context(|| format!("SHIELD {endpoint} evaluation body read failed"))?;

    if !status.is_success() {
        return Err(anyhow!(
            "SHIELD {endpoint} evaluation returned {status}: {raw}"
        ));
    }

    decision_from_response(&raw)
}

fn decision_from_response(raw: &str) -> Result<Decision> {
    let response: EvaluateResponse = serde_json::from_str(raw).with_context(|| {
        format!("SHIELD response did not match expected schema. Raw body: {raw}")
    })?;

    match response.decision.as_str() {
        "allow" => Ok(Decision::Allow),
        "block" => Ok(Decision::Block {
            reason: response.reason,
        }),
        "delay" => Ok(Decision::Delay {
            delay: response
                .metadata
                .and_then(|metadata| metadata.delay_ms)
                .map(Duration::from_millis),
            reason: response.reason,
        }),
        "approval" => Ok(Decision::Approval {
            reason: response.reason,
        }),
        "not_enough_permissions" => Ok(Decision::NotEnoughPermissions {
            reason: response.reason,
        }),
        other => Err(anyhow!("Unexpected SHIELD decision type: {other}")),
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;

    #[test]
    fn decision_from_response_parses_allow() {
        assert_eq!(
            decision_from_response(r#"{"type":"allow","reason":"No active security mode"}"#)
                .unwrap(),
            Decision::Allow
        );
    }

    #[test]
    fn decision_from_response_parses_block() {
        assert_eq!(
            decision_from_response(r#"{"type":"block","reason":"chain:eth:withdraw"}"#).unwrap(),
            Decision::Block {
                reason: "chain:eth:withdraw".to_string()
            }
        );
    }

    #[test]
    fn decision_from_response_parses_delay_with_metadata() {
        assert_eq!(
            decision_from_response(
                r#"{"type":"delay","reason":"Security mode is paranoid","metadata":{"delayMs":30000}}"#
            )
            .unwrap(),
            Decision::Delay {
                delay: Some(Duration::from_secs(30)),
                reason: "Security mode is paranoid".to_string()
            }
        );
    }

    #[test]
    fn decision_from_response_parses_delay_without_metadata() {
        assert_eq!(
            decision_from_response(r#"{"type":"delay","reason":"Security mode is paranoid"}"#)
                .unwrap(),
            Decision::Delay {
                delay: None,
                reason: "Security mode is paranoid".to_string()
            }
        );
    }

    #[test]
    fn decision_from_response_parses_approval() {
        assert_eq!(
            decision_from_response(
                r#"{"type":"approval","reason":"Security mode is under_attack","metadata":{"approvalThresholdUsd":1000}}"#
            )
            .unwrap(),
            Decision::Approval {
                reason: "Security mode is under_attack".to_string()
            }
        );
    }

    #[test]
    fn decision_from_response_parses_not_enough_permissions() {
        assert_eq!(
            decision_from_response(
                r#"{"type":"not_enough_permissions","reason":"Partner is not allowed"}"#
            )
            .unwrap(),
            Decision::NotEnoughPermissions {
                reason: "Partner is not allowed".to_string()
            }
        );
    }

    #[test]
    fn decision_from_response_errors_on_unknown_type() {
        assert!(decision_from_response(r#"{"type":"wat","reason":""}"#).is_err());
    }

    #[test]
    fn decision_from_response_errors_on_malformed_body() {
        assert!(decision_from_response("not json").is_err());
        assert!(decision_from_response(r#"{"reason":"missing type"}"#).is_err());
    }

    #[test]
    fn deposit_request_serializes_to_expected_schema() {
        let request = DepositRequest {
            blockchain: "eth",
            bridge: BRIDGE,
            token: "nep141:eth.omft.near",
            amount: "1000000".to_string(),
            amount_usd: 0.0,
            timestamp: "2026-05-28T08:00:00.000Z".to_string(),
            sender_address: "0xsender",
        };

        assert_eq!(
            serde_json::to_value(&request).unwrap(),
            serde_json::json!({
                "blockchain": "eth",
                "bridge": "omni",
                "token": "nep141:eth.omft.near",
                "amount": "1000000",
                "amountUsd": 0.0,
                "timestamp": "2026-05-28T08:00:00.000Z",
                "senderAddress": "0xsender",
            })
        );
    }

    #[test]
    fn withdrawal_request_serializes_to_expected_schema() {
        let request = WithdrawalRequest {
            blockchain: "eth",
            bridge: BRIDGE,
            token: "nep141:eth.omft.near",
            amount: "1000000".to_string(),
            amount_usd: 0.0,
            recipient: "0xrecipient",
            timestamp: "2026-05-28T08:00:00.000Z".to_string(),
        };

        assert_eq!(
            serde_json::to_value(&request).unwrap(),
            serde_json::json!({
                "blockchain": "eth",
                "bridge": "omni",
                "token": "nep141:eth.omft.near",
                "amount": "1000000",
                "amountUsd": 0.0,
                "recipient": "0xrecipient",
                "timestamp": "2026-05-28T08:00:00.000Z",
            })
        );
    }

    #[test]
    fn blockchain_tag_maps_supported_chains() {
        assert_eq!(blockchain_tag(ChainKind::Near), Some("near"));
        assert_eq!(blockchain_tag(ChainKind::Bnb), Some("bsc"));
        assert_eq!(blockchain_tag(ChainKind::Strk), Some("starknet"));
        assert_eq!(blockchain_tag(ChainKind::Zcash), Some("zec"));
        assert_eq!(blockchain_tag(ChainKind::Fogo), Some("fogo"));
        assert_eq!(blockchain_tag(ChainKind::Aptos), Some("aptos"));
        assert_eq!(blockchain_tag(ChainKind::HyperEvm), None);
        assert_eq!(blockchain_tag(ChainKind::Abs), None);
    }

    #[test]
    fn to_asset_id_requires_near_token() {
        let near_token = OmniAddress::Near("eth.omft.near".parse().unwrap());
        assert_eq!(
            to_asset_id(&near_token).unwrap(),
            "nep141:eth.omft.near".to_string()
        );

        let eth_token =
            OmniAddress::from_str("eth:0x0000000000000000000000000000000000000001").unwrap();
        assert!(to_asset_id(&eth_token).is_err());
    }

    #[test]
    fn bare_address_strips_chain_prefix() {
        let eth = OmniAddress::from_str("eth:0x0000000000000000000000000000000000000001").unwrap();
        assert_eq!(
            bare_address(&eth),
            "0x0000000000000000000000000000000000000001".to_string()
        );

        let near = OmniAddress::Near("alice.near".parse().unwrap());
        assert_eq!(bare_address(&near), "alice.near".to_string());
    }
}
