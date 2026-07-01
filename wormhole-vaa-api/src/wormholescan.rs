//! Per-request fallback to the public wormholescan REST API.
//!
//! The spy has no backfill — a VAA missed while it was disconnected (e.g. a spy restart)
//! is never re-delivered. When our store doesn't have a requested VAA, the REST handlers
//! consult wormholescan directly and cache any hit back into the store, so the gap
//! self-heals. This makes the service dual-source: a VAA is served as long as *either*
//! our spy captured it *or* wormholescan has it — and the proxy only ever needs to know
//! our single URL.

use std::time::Duration;

use base64::{Engine, engine::general_purpose::STANDARD};
use serde::Deserialize;

/// Client for the two wormholescan endpoints we mirror.
#[derive(Clone)]
pub struct WormholescanClient {
    http: reqwest::Client,
    base_url: String,
}

#[derive(Deserialize)]
struct SingleResponse {
    data: VaaField,
}

#[derive(Deserialize)]
struct ListResponse {
    data: Vec<VaaField>,
}

#[derive(Deserialize)]
struct VaaField {
    vaa: String,
}

impl WormholescanClient {
    pub fn new(base_url: &str, request_timeout: Duration) -> reqwest::Result<Self> {
        Ok(Self {
            http: reqwest::Client::builder()
                .timeout(request_timeout)
                .build()?,
            base_url: base_url.trim_end_matches('/').to_owned(),
        })
    }

    /// Fetch one VAA by identity. `Ok(None)` = wormholescan doesn't have it (404);
    /// `Err` = transport/parse error (couldn't determine — we don't have it either).
    pub async fn get_vaa(
        &self,
        chain: u16,
        emitter_hex: &str,
        sequence: u64,
    ) -> anyhow::Result<Option<Vec<u8>>> {
        let url = format!(
            "{}/api/v1/vaas/{chain}/{emitter_hex}/{sequence}",
            self.base_url
        );
        let resp = self.http.get(&url).send().await?;
        let status = resp.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !status.is_success() {
            anyhow::bail!("wormholescan GET {url} returned HTTP {status}");
        }
        let parsed: SingleResponse = resp.json().await?;
        Ok(Some(decode_vaa(&parsed.data.vaa)?))
    }

    /// Fetch all VAAs a tx produced, in wormholescan's order. Empty vec = none found.
    pub async fn get_vaa_by_tx(&self, tx_hash: &str) -> anyhow::Result<Vec<Vec<u8>>> {
        let url = format!("{}/api/v1/vaas/", self.base_url);
        let resp = self
            .http
            .get(&url)
            .query(&[("txHash", tx_hash)])
            .send()
            .await?;
        let status = resp.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(Vec::new());
        }
        if !status.is_success() {
            anyhow::bail!("wormholescan GET {url}?txHash={tx_hash} returned HTTP {status}");
        }
        // Unknown txHash returns 200 with `{"data":[]}`.
        let parsed: ListResponse = resp.json().await?;
        parsed.data.iter().map(|d| decode_vaa(&d.vaa)).collect()
    }
}

/// Decode a wormholescan `vaa` field (standard padded base64) to raw bytes.
fn decode_vaa(b64: &str) -> anyhow::Result<Vec<u8>> {
    Ok(STANDARD.decode(b64)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::prelude::*;

    fn client(server: &MockServer) -> WormholescanClient {
        WormholescanClient::new(&server.base_url(), Duration::from_secs(5)).unwrap()
    }

    #[tokio::test]
    async fn get_vaa_decodes_hit() {
        let vaa = vec![1u8, 0, 0, 0, 7, 0xAB, 0xCD];
        let b64 = STANDARD.encode(&vaa);
        let server = MockServer::start_async().await;
        let emitter = "19671a08a9cef6f3a04314ed478fc332a4966f41ad3e6fea76933dede9c6cdfe";
        server
            .mock_async(|when, then| {
                when.method(GET)
                    .path(format!("/api/v1/vaas/1/{emitter}/42597"));
                then.status(200)
                    .header("content-type", "application/json")
                    .json_body(serde_json::json!({ "data": { "vaa": b64 } }));
            })
            .await;
        let got = client(&server).get_vaa(1, emitter, 42597).await.unwrap();
        assert_eq!(got, Some(vaa));
    }

    #[tokio::test]
    async fn get_vaa_404_is_none() {
        let server = MockServer::start_async().await;
        server
            .mock_async(|when, then| {
                when.method(GET);
                then.status(404)
                    .json_body(serde_json::json!({ "code": 5, "message": "NOT FOUND" }));
            })
            .await;
        let got = client(&server).get_vaa(1, "00", 1).await.unwrap();
        assert_eq!(got, None);
    }

    #[tokio::test]
    async fn get_vaa_by_tx_decodes_each() {
        let v1 = vec![1u8, 2, 3];
        let v2 = vec![4u8, 5, 6, 7];
        let server = MockServer::start_async().await;
        server
            .mock_async(|when, then| {
                when.method(GET)
                    .path("/api/v1/vaas/")
                    .query_param("txHash", "0xabc");
                then.status(200).json_body(serde_json::json!({
                    "data": [ { "vaa": STANDARD.encode(&v1) }, { "vaa": STANDARD.encode(&v2) } ]
                }));
            })
            .await;
        let got = client(&server).get_vaa_by_tx("0xabc").await.unwrap();
        assert_eq!(got, vec![v1, v2]);
    }

    #[tokio::test]
    async fn get_vaa_by_tx_empty_is_empty() {
        let server = MockServer::start_async().await;
        server
            .mock_async(|when, then| {
                when.method(GET).path("/api/v1/vaas/");
                then.status(200)
                    .json_body(serde_json::json!({ "data": [], "pagination": { "next": "" } }));
            })
            .await;
        let got = client(&server).get_vaa_by_tx("0xdeadbeef").await.unwrap();
        assert!(got.is_empty());
    }

    #[tokio::test]
    async fn server_error_is_err() {
        let server = MockServer::start_async().await;
        server
            .mock_async(|when, then| {
                when.method(GET);
                then.status(502);
            })
            .await;
        assert!(client(&server).get_vaa(1, "00", 1).await.is_err());
    }
}
