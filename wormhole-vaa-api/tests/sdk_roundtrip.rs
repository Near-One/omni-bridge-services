//! Acceptance (a): the real `bridge-sdk-rs` `wormhole-bridge-client` must parse the
//! exact response shapes this service emits, for both endpoints, and recover the VAA.
//!
//! We serve our wormholescan-compatible shapes via a local mock and point the real
//! client at it, asserting it returns `hex::encode(<raw vaa bytes>)`.

use base64::{Engine, engine::general_purpose::STANDARD};
use httpmock::prelude::*;
use wormhole_bridge_client::WormholeBridgeClient;

/// Raw VAA bytes (content is opaque to the client — it just base64-decodes → hex).
fn sample_vaa() -> Vec<u8> {
    vec![
        0x01, 0x00, 0x00, 0x00, 0x07, 0x0d, 0xde, 0xad, 0xbe, 0xef, 0x12, 0x34, 0x56, 0x78, 0x90,
        0xab, 0xcd, 0xef,
    ]
}

#[tokio::test]
async fn client_parses_endpoint_one_by_chain_emitter_sequence() {
    let vaa = sample_vaa();
    let b64 = STANDARD.encode(&vaa);
    let emitter = "19671a08a9cef6f3a04314ed478fc332a4966f41ad3e6fea76933dede9c6cdfe";

    let server = MockServer::start_async().await;
    server
        .mock_async(|when, then| {
            when.method(GET)
                .path(format!("/api/v1/vaas/1/{emitter}/42597"));
            then.status(200)
                .header("content-type", "application/json")
                .json_body(serde_json::json!({ "data": { "vaa": b64 } }));
        })
        .await;

    let client = WormholeBridgeClient {
        endpoint: Some(server.base_url()),
    };

    let got = client
        .get_vaa(1u64, emitter, 42597u64)
        .await
        .expect("client parses endpoint (1)");
    assert_eq!(got, hex::encode(&vaa));
}

#[tokio::test]
async fn client_parses_endpoint_two_by_txhash() {
    let vaa = sample_vaa();
    let b64 = STANDARD.encode(&vaa);
    let tx_hash = "0x2b1d4733e591483ff22641c448017dbdae5d49c76eb40b9ccc101edc852f4a34";

    let server = MockServer::start_async().await;
    server
        .mock_async(|when, then| {
            when.method(GET)
                .path("/api/v1/vaas/")
                .query_param("txHash", tx_hash);
            then.status(200)
                .header("content-type", "application/json")
                .json_body(serde_json::json!({ "data": [{ "vaa": b64 }] }));
        })
        .await;

    let client = WormholeBridgeClient {
        endpoint: Some(server.base_url()),
    };

    let got = client
        .get_vaa_by_tx_hash(tx_hash.to_owned())
        .await
        .expect("client parses endpoint (2)");
    assert_eq!(got, hex::encode(&vaa));
}

#[tokio::test]
async fn client_treats_empty_data_array_as_no_vaa() {
    // Our endpoint (2) 404 body is `{"data":[]}`; the client deserializes it and
    // `.first()` yields None → a "No VAA found" error (the relayer then retries).
    let server = MockServer::start_async().await;
    server
        .mock_async(|when, then| {
            when.method(GET).path("/api/v1/vaas/");
            then.status(200)
                .header("content-type", "application/json")
                .json_body(serde_json::json!({ "data": [] }));
        })
        .await;

    let client = WormholeBridgeClient {
        endpoint: Some(server.base_url()),
    };

    let result = client.get_vaa_by_tx_hash("0xdeadbeef".to_owned()).await;
    assert!(result.is_err(), "empty data array must not yield a VAA");
}
