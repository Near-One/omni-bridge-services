//! Thin JSON-RPC client to the in-cluster omni-proxy.
//!
//! All chain RPC goes through the proxy at `{base}/{prefix}?omni-proxy-service=…`
//! (never to public RPC). The `omni-proxy-service` query param is used only for the
//! proxy's metrics labeling and is stripped before the request is forwarded upstream.

use std::time::Duration;

use futures::StreamExt;
use serde_json::{Value, json};

const SERVICE_NAME: &str = "wormhole-vaa-api";

/// Cap on a single RPC response body, to bound memory regardless of upstream behavior.
const MAX_RPC_BODY: usize = 16 * 1024 * 1024;

#[derive(Clone)]
pub struct ProxyClient {
    http: reqwest::Client,
    base_url: String,
}

impl ProxyClient {
    pub fn new(base_url: &str, request_timeout: Duration) -> reqwest::Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(request_timeout)
            .build()?;
        Ok(Self {
            http,
            base_url: base_url.trim_end_matches('/').to_owned(),
        })
    }

    fn route_url(&self, prefix: &str) -> String {
        let prefix = if prefix.starts_with('/') {
            prefix.to_owned()
        } else {
            format!("/{prefix}")
        };
        format!(
            "{}{prefix}?omni-proxy-service={SERVICE_NAME}",
            self.base_url
        )
    }

    /// Issue a JSON-RPC call against a proxy route and return the `result` value
    /// (`Value::Null` when the node returns a null result, e.g. unknown tx).
    pub async fn rpc(&self, prefix: &str, method: &str, params: Value) -> anyhow::Result<Value> {
        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": method,
            "params": params,
        });

        let resp = self
            .http
            .post(self.route_url(prefix))
            .json(&body)
            .send()
            .await?;

        let status = resp.status();
        if !status.is_success() {
            anyhow::bail!("proxy {prefix} {method} returned HTTP {status}");
        }

        // Bounded read: reqwest applies no body-size limit, so buffer chunks ourselves
        // and bail past MAX_RPC_BODY rather than risk OOM on a pathological response.
        if resp
            .content_length()
            .is_some_and(|n| n > u64::try_from(MAX_RPC_BODY).unwrap_or(u64::MAX))
        {
            anyhow::bail!("proxy {prefix} {method} response exceeds {MAX_RPC_BODY} bytes");
        }
        let mut stream = resp.bytes_stream();
        let mut buf = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            if buf.len() + chunk.len() > MAX_RPC_BODY {
                anyhow::bail!("proxy {prefix} {method} response exceeds {MAX_RPC_BODY} bytes");
            }
            buf.extend_from_slice(&chunk);
        }

        let value: Value = serde_json::from_slice(&buf)?;
        if let Some(err) = value.get("error").filter(|e| !e.is_null()) {
            anyhow::bail!("proxy {prefix} {method} JSON-RPC error: {err}");
        }
        Ok(value.get("result").cloned().unwrap_or(Value::Null))
    }
}
