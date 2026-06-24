// End-to-end tests for omni-proxy.
//
// Each test starts a real Pingora proxy server on its own port and one or more
// httpmock upstream servers.  Requests are sent via reqwest::blocking.
//
// Failover semantics note:
//   Unlike the JS version which retried within the same request, this proxy
//   records a failure at request end and the *next* request selects a
//   different upstream.  Tests that exercise failover therefore send two
//   requests: the first receives the bad response, the second is routed to
//   the fallback.

use std::net::TcpStream;
use std::thread;
use std::time::Duration;

use httpmock::prelude::*;
use omni_proxy::{config::Config, proxy::RpcProxy};
use reqwest::blocking::Client;

// ---- Port allocation -------------------------------------------------------
//
// Briefly bind to port 0 so the OS assigns a free ephemeral port, then release
// the listener.  Works across processes (nextest runs each test in its own
// process, so a shared AtomicU16 resets to the same base value every time and
// causes collisions).  There is a small TOCTOU window between the drop and
// Pingora's bind, but the kernel does not immediately recycle port numbers.

fn alloc_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    drop(l);
    port
}

// ---- Proxy startup ---------------------------------------------------------

fn start_proxy(config_toml: &str, proxy_port: u16) {
    let routes = toml::from_str::<Config>(config_toml)
        .expect("test config must be valid TOML")
        .routes;

    let addr = format!("127.0.0.1:{proxy_port}");
    thread::spawn(move || {
        let mut server = pingora::server::Server::new(None).unwrap();
        server.bootstrap();
        let mut svc =
            pingora::proxy::http_proxy_service(&server.configuration, RpcProxy::new(routes));
        svc.add_tcp(&addr);
        server.add_service(svc);
        server.run_forever();
    });

    // Poll until the port accepts connections (up to 1 s).
    let bind_addr = format!("127.0.0.1:{proxy_port}");
    for _ in 0..100 {
        if TcpStream::connect(&bind_addr).is_ok() {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("proxy did not bind on {bind_addr}");
}

// ---- Helpers ---------------------------------------------------------------

fn proxy_url(port: u16, path: &str) -> String {
    format!("http://127.0.0.1:{port}{path}")
}

// ---- Tests -----------------------------------------------------------------

/// Requests reach the upstream and the path is rewritten correctly.
/// The route prefix (/near) is stripped and the upstream's own path is used.
#[test]
fn test_basic_routing_and_path_rewrite() {
    let upstream = MockServer::start();
    let port = alloc_port();

    let m = upstream.mock(|when, then| {
        when.method(POST).path("/");
        then.status(200).body(r#"{"ok":true}"#);
    });

    start_proxy(
        &format!(
            r#"
[[routes]]
prefix = "/near"
upstreams = [{{ url = "http://{}" }}]
failover = {{ status_codes = [500] }}
"#,
            upstream.address()
        ),
        port,
    );

    let resp = Client::new().post(proxy_url(port, "/near")).send().unwrap();

    assert_eq!(resp.status(), 200);
    m.assert_hits(1);
}

/// The ?omni-proxy-service= query param must be stripped before forwarding to upstream.
#[test]
fn test_service_param_is_stripped() {
    let upstream = MockServer::start();
    let port = alloc_port();

    // If ?omni-proxy-service= is leaked through, this mock fires and returns 500 (test fails).
    let fail_if_leaked = upstream.mock(|when, then| {
        when.query_param_exists("omni-proxy-service");
        then.status(500);
    });
    // Catch-all for the correctly stripped request.
    let ok = upstream.mock(|when, then| {
        when.any_request();
        then.status(200).body("{}");
    });

    start_proxy(
        &format!(
            r#"
[[routes]]
prefix = "/near"
upstreams = [{{ url = "http://{}" }}]
failover = {{ status_codes = [500] }}
"#,
            upstream.address()
        ),
        port,
    );

    // Client sends ?omni-proxy-service=omni-relayer; proxy must strip it.
    let resp = Client::new()
        .post(proxy_url(port, "/near?omni-proxy-service=omni-relayer"))
        .send()
        .unwrap();

    assert_eq!(resp.status(), 200);
    fail_if_leaked.assert_hits(0);
    ok.assert_hits(1);
}

/// URL credentials are converted to an Authorization: Basic header on the
/// upstream request; the raw credentials must not appear in the forwarded URL.
#[test]
fn test_basic_auth_injection() {
    let upstream = MockServer::start();
    let port = alloc_port();

    let m = upstream.mock(|when, then| {
        when.method(POST)
            .header("Authorization", "Basic eC1hcGkta2V5OnNlY3JldA=="); // x-api-key:secret
        then.status(200).body("{}");
    });

    start_proxy(
        &format!(
            r#"
[[routes]]
prefix = "/btc"
upstreams = [{{ url = "http://x-api-key:secret@{}" }}]
failover = {{ status_codes = [500] }}
"#,
            upstream.address()
        ),
        port,
    );

    let resp = Client::new().post(proxy_url(port, "/btc")).send().unwrap();

    assert_eq!(resp.status(), 200);
    m.assert();
}

/// After the primary returns a configured failure status code the next request
/// is routed to the fallback upstream.
#[test]
fn test_failover_on_http_status() {
    let primary = MockServer::start();
    let fallback = MockServer::start();
    let port = alloc_port();

    primary.mock(|when, then| {
        when.any_request();
        then.status(500);
    });
    fallback.mock(|when, then| {
        when.any_request();
        then.status(200).body(r#"{"ok":true}"#);
    });

    start_proxy(
        &format!(
            r#"
[[routes]]
prefix = "/eth"
upstreams = [
  {{ url = "http://{}" }},
  {{ url = "http://{}" }},
]
failover = {{ status_codes = [500], failure_threshold = 1 }}
"#,
            primary.address(),
            fallback.address()
        ),
        port,
    );

    let client = Client::new();

    // First request: primary is healthy, returns 500, failure is recorded.
    let r1 = client.post(proxy_url(port, "/eth")).send().unwrap();
    assert_eq!(r1.status(), 500);

    // Second request: primary is degraded (1 failure ≥ threshold 1), uses fallback.
    let r2 = client.post(proxy_url(port, "/eth")).send().unwrap();
    assert_eq!(r2.status(), 200);
}

/// After the primary returns a JSON-RPC error whose code matches failoverOnRpcCodes
/// the next request is routed to the fallback upstream.
#[test]
fn test_failover_on_rpc_error_code() {
    let primary = MockServer::start();
    let fallback = MockServer::start();
    let port = alloc_port();

    // Solana rate-limit error
    primary.mock(|when, then| {
        when.any_request();
        then.status(200)
            .header("content-type", "application/json")
            .body(r#"{"jsonrpc":"2.0","error":{"code":-32005,"message":"rate limited"},"id":1}"#);
    });
    fallback.mock(|when, then| {
        when.any_request();
        then.status(200)
            .body(r#"{"jsonrpc":"2.0","result":"ok","id":1}"#);
    });

    start_proxy(
        &format!(
            r#"
[[routes]]
prefix = "/solana"
upstreams = [
  {{ url = "http://{}" }},
  {{ url = "http://{}" }},
]
failover = {{ status_codes = [429, 500], rpc_codes = [-32005], failure_threshold = 1 }}
"#,
            primary.address(),
            fallback.address()
        ),
        port,
    );

    let client = Client::new();

    // First request: primary responds with RPC error, failure is recorded.
    let r1 = client.post(proxy_url(port, "/solana")).send().unwrap();
    assert_eq!(r1.status(), 200); // HTTP status is 200, error is in the body

    // Second request: primary is degraded, uses fallback.
    let r2 = client.post(proxy_url(port, "/solana")).send().unwrap();
    let body = r2.text().unwrap();
    assert!(
        body.contains("\"result\""),
        "expected fallback response, got: {body}"
    );
}

/// GET /healthz returns 200 without hitting any upstream.
#[test]
fn test_healthz() {
    let upstream = MockServer::start();
    let port = alloc_port();

    // The upstream mock should never be called.
    let m = upstream.mock(|when, then| {
        when.any_request();
        then.status(500);
    });

    start_proxy(
        &format!(
            r#"
[[routes]]
prefix = "/near"
upstreams = [{{ url = "http://{}" }}]
failover = {{ status_codes = [500] }}
"#,
            upstream.address()
        ),
        port,
    );

    let resp = Client::new()
        .get(proxy_url(port, "/healthz"))
        .send()
        .unwrap();

    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().unwrap(), "ok");
    m.assert_hits(0);
}

/// Unknown prefix returns 404.
#[test]
fn test_unknown_prefix_returns_404() {
    let upstream = MockServer::start();
    let port = alloc_port();

    upstream.mock(|when, then| {
        when.any_request();
        then.status(200);
    });

    start_proxy(
        &format!(
            r#"
[[routes]]
prefix = "/near"
upstreams = [{{ url = "http://{}" }}]
failover = {{ status_codes = [500] }}
"#,
            upstream.address()
        ),
        port,
    );

    let resp = Client::new().get(proxy_url(port, "/eth")).send().unwrap();

    assert_eq!(resp.status(), 404);
}

/// /near-something must NOT match the /near prefix (path segment boundary).
#[test]
fn test_prefix_does_not_match_across_segment_boundary() {
    let near = MockServer::start();
    let port = alloc_port();

    near.mock(|when, then| {
        when.any_request();
        then.status(200);
    });

    start_proxy(
        &format!(
            r#"
[[routes]]
prefix = "/near"
upstreams = [{{ url = "http://{}" }}]
failover = {{ status_codes = [500] }}
"#,
            near.address()
        ),
        port,
    );

    // /near-something should 404, not match /near
    let resp = Client::new()
        .post(proxy_url(port, "/near-something"))
        .send()
        .unwrap();

    assert_eq!(resp.status(), 404);
    // 404 proves the upstream was never called — no further assertion needed.
}

/// Upstream query params (e.g. API keys) are preserved when the upstream URL
/// carries a query string.
#[test]
fn test_upstream_query_params_forwarded() {
    let upstream = MockServer::start();
    let port = alloc_port();

    let m = upstream.mock(|when, then| {
        when.method(POST).query_param("apiKey", "secret123");
        then.status(200).body("{}");
    });

    start_proxy(
        &format!(
            r#"
[[routes]]
prefix = "/near"
upstreams = [{{ url = "http://{}/?apiKey=secret123" }}]
failover = {{ status_codes = [500] }}
"#,
            upstream.address()
        ),
        port,
    );

    let resp = Client::new().post(proxy_url(port, "/near")).send().unwrap();

    assert_eq!(resp.status(), 200);
    m.assert();
}
