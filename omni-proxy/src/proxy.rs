use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bytes::Bytes;
use opentelemetry::KeyValue;
use opentelemetry::metrics::Counter;
use pingora::prelude::*;
use pingora::upstreams::peer::HttpPeer;
use serde_json::Value;
use tracing::warn;

use crate::config::Route;
use crate::types::Prefix;

const MAX_RPC_BODY_BYTES: usize = 256 * 1024;

struct UpstreamHealth {
    failures: Mutex<VecDeque<Instant>>,
}

impl UpstreamHealth {
    fn new() -> Self {
        Self {
            failures: Mutex::new(VecDeque::new()),
        }
    }

    fn record_failure(&self) {
        self.failures.lock().unwrap().push_back(Instant::now());
    }

    fn record_success(&self) {
        self.failures.lock().unwrap().pop_front();
    }

    fn recent_failures(&self, window: Duration) -> usize {
        let mut guard = self.failures.lock().unwrap();
        let now = Instant::now();
        while guard
            .front()
            .is_some_and(|t| now.saturating_duration_since(*t) > window)
        {
            guard.pop_front();
        }
        guard.len()
    }
}

struct RouteState {
    route: Route,
    health: Vec<UpstreamHealth>,
}

impl RouteState {
    fn new(route: Route) -> Self {
        let count = route.upstreams().len();
        Self {
            route,
            health: (0..count).map(|_| UpstreamHealth::new()).collect(),
        }
    }

    fn select(&self) -> usize {
        let failover = self.route.failover();
        let threshold = failover.failure_threshold();
        let window = failover.window();

        for (i, health) in self.health.iter().enumerate() {
            if health.recent_failures(window) < threshold {
                return i;
            }
        }

        0
    }
}

pub struct RequestCtx {
    upstream_idx: usize,
    route_prefix: Option<Prefix>,
    body_buf: Option<Vec<u8>>,
    is_failure: bool,
    pub service: String,
    upstream_host: String,
    status_code: u16,
}

impl Default for RequestCtx {
    fn default() -> Self {
        Self {
            upstream_idx: 0,
            route_prefix: None,
            body_buf: None,
            is_failure: false,
            service: "unknown".to_owned(),
            upstream_host: String::new(),
            status_code: 0,
        }
    }
}

fn sanitize_service(raw: &str) -> String {
    let s: String = raw
        .to_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        .take(64)
        .collect();
    if s.is_empty() {
        "unknown".to_owned()
    } else {
        s
    }
}

pub struct RpcProxy {
    routes: Arc<HashMap<Prefix, RouteState>>,
    requests: Counter<u64>,
}

impl RpcProxy {
    #[must_use]
    pub fn new(routes: Vec<Route>) -> Self {
        let map = routes
            .into_iter()
            .map(|r| (r.prefix().clone(), RouteState::new(r)))
            .collect();
        let requests = opentelemetry::global::meter("omni-proxy")
            .u64_counter("rpc_requests_total")
            .with_description("Total RPC requests proxied")
            .build();
        Self {
            routes: Arc::new(map),
            requests,
        }
    }

    fn match_route(&self, path: &str) -> Option<&RouteState> {
        self.routes
            .iter()
            .filter(|(prefix, _)| {
                let p = prefix.as_str();
                path == p || (path.starts_with(p) && path.as_bytes().get(p.len()) == Some(&b'/'))
            })
            .max_by_key(|(prefix, _)| prefix.as_str().len())
            .map(|(_, state)| state)
    }
}

#[async_trait]
impl ProxyHttp for RpcProxy {
    type CTX = RequestCtx;

    fn new_ctx(&self) -> Self::CTX {
        RequestCtx::default()
    }

    async fn request_filter(&self, session: &mut Session, _ctx: &mut Self::CTX) -> Result<bool>
    where
        Self::CTX: Send + Sync,
    {
        if session.req_header().uri.path() == "/healthz" {
            let resp = ResponseHeader::build(200, None)?;
            session.write_response_header(Box::new(resp), false).await?;
            session
                .write_response_body(Some(Bytes::from_static(b"ok")), true)
                .await?;
            return Ok(true);
        }

        Ok(false)
    }

    async fn upstream_peer(
        &self,
        session: &mut Session,
        ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        let uri = session.req_header().uri.clone();

        ctx.service = uri
            .query()
            .and_then(|q| {
                q.split('&')
                    .find_map(|p| p.strip_prefix("omni-proxy-service="))
            })
            .map_or_else(|| "unknown".to_owned(), sanitize_service);

        let state = self
            .match_route(uri.path())
            .ok_or_else(|| Error::explain(HTTPStatus(404), "no matching route"))?;

        let idx = state.select();
        let upstream = &state.route.upstreams()[idx];

        ctx.upstream_idx = idx;
        ctx.route_prefix = Some(state.route.prefix().clone());
        if !state.route.failover().rpc_codes().is_empty() {
            ctx.body_buf = Some(Vec::new());
        }

        ctx.upstream_host = upstream.sni();

        let mut peer = HttpPeer::new(upstream.addr(), upstream.is_tls(), upstream.sni());
        if upstream.is_tls() && !upstream.is_websocket() {
            peer.options.set_http_version(2, 1);
        }
        if let Some(t) = upstream.timeout() {
            peer.options.connection_timeout = Some(t);
            peer.options.read_timeout = Some(t);
            peer.options.write_timeout = Some(t);
        }

        Ok(Box::new(peer))
    }

    async fn upstream_request_filter(
        &self,
        session: &mut Session,
        upstream_request: &mut RequestHeader,
        ctx: &mut Self::CTX,
    ) -> Result<()>
    where
        Self::CTX: Send + Sync,
    {
        let Some(ref prefix) = ctx.route_prefix else {
            return Ok(());
        };
        let Some(state) = self.routes.get(prefix) else {
            return Ok(());
        };
        let upstream = &state.route.upstreams()[ctx.upstream_idx];

        upstream_request.insert_header("Host", upstream.authority())?;
        if let Some(auth) = upstream.auth_header() {
            upstream_request.insert_header("Authorization", auth)?;
        }
        if !state.route.failover().rpc_codes().is_empty() {
            upstream_request.insert_header("Accept-Encoding", "identity")?;
        }

        let orig = session.req_header().uri.clone();
        let suffix = orig
            .path()
            .strip_prefix(prefix.as_str())
            .unwrap_or("")
            .trim_start_matches('/');
        let upstream_base = upstream.url_path().trim_end_matches('/');
        let new_path = if suffix.is_empty() {
            if upstream_base.is_empty() {
                "/".to_owned()
            } else {
                upstream_base.to_owned()
            }
        } else {
            format!("{upstream_base}/{suffix}")
        };

        let client_query: String = orig
            .query()
            .unwrap_or("")
            .split('&')
            .filter(|p| !p.is_empty() && !p.starts_with("omni-proxy-service="))
            .collect::<Vec<_>>()
            .join("&");
        let client_query = if client_query.is_empty() {
            None
        } else {
            Some(client_query)
        };

        let new_uri = match (upstream.url_query(), client_query.as_deref()) {
            (Some(uq), Some(cq)) => format!("{new_path}?{uq}&{cq}"),
            (Some(uq), None) => format!("{new_path}?{uq}"),
            (None, Some(cq)) => format!("{new_path}?{cq}"),
            (None, None) => new_path,
        };

        let scheme = if upstream.is_tls() { "https" } else { "http" };
        let abs_uri = format!("{scheme}://{}{}", upstream.authority(), new_uri);
        upstream_request.set_uri(
            abs_uri
                .parse()
                .map_err(|_| Error::explain(HTTPStatus(500), "failed to build upstream uri"))?,
        );

        Ok(())
    }

    async fn upstream_response_filter(
        &self,
        _session: &mut Session,
        upstream_response: &mut ResponseHeader,
        ctx: &mut Self::CTX,
    ) -> Result<()>
    where
        Self::CTX: Send + Sync,
    {
        ctx.status_code = upstream_response.status.as_u16();
        if let Some(ref prefix) = ctx.route_prefix
            && let Some(state) = self.routes.get(prefix)
            && state.route.failover().is_failure_status(ctx.status_code)
        {
            ctx.is_failure = true;
        }

        Ok(())
    }

    fn upstream_response_body_filter(
        &self,
        _session: &mut Session,
        body: &mut Option<Bytes>,
        end_of_stream: bool,
        ctx: &mut Self::CTX,
    ) -> Result<Option<Duration>> {
        if let Some(ref mut buf) = ctx.body_buf {
            if let Some(chunk) = body {
                let take = chunk
                    .len()
                    .min(MAX_RPC_BODY_BYTES.saturating_sub(buf.len()));
                buf.extend_from_slice(&chunk[..take]);
            }

            if end_of_stream
                && !ctx.is_failure
                && buf.len() < MAX_RPC_BODY_BYTES
                && let Some(ref prefix) = ctx.route_prefix
                && let Some(state) = self.routes.get(prefix)
                && let Ok(json) = serde_json::from_slice::<Value>(buf)
                && let Some(code) = json.pointer("/error/code").and_then(Value::as_i64)
                && let Ok(code) = i32::try_from(code)
                && state.route.failover().is_failure_rpc_code(code)
            {
                ctx.is_failure = true;
            }
        }
        Ok(None)
    }

    async fn logging(&self, _session: &mut Session, e: Option<&Error>, ctx: &mut Self::CTX)
    where
        Self::CTX: Send + Sync,
    {
        let failed = ctx.is_failure || e.is_some();
        if let Some(ref prefix) = ctx.route_prefix
            && let Some(state) = self.routes.get(prefix)
            && let Some(health) = state.health.get(ctx.upstream_idx)
        {
            if failed {
                health.record_failure();
                let detail = match e {
                    Some(err) => err.to_string(),
                    None => "failure status or rpc error code".to_owned(),
                };
                warn!(
                    route = prefix.as_str(),
                    service = %ctx.service,
                    upstream = %ctx.upstream_host,
                    status = ctx.status_code,
                    "upstream request failed: {}",
                    detail
                );
            } else {
                health.record_success();
            }

            self.requests.add(
                1,
                &[
                    KeyValue::new("route", prefix.as_str().trim_start_matches('/').to_owned()),
                    KeyValue::new("service", ctx.service.clone()),
                    KeyValue::new("upstream", ctx.upstream_host.clone()),
                    KeyValue::new("status", ctx.status_code.to_string()),
                ],
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn make_proxy(threshold: u32, window_secs: u64) -> RpcProxy {
        let config: Config = toml::from_str(&format!(
            r#"
[[routes]]
prefix = "/test"
upstreams = [
  {{ url = "http://primary.example.com" }},
  {{ url = "http://fallback.example.com" }},
]
failover = {{ status_codes = [500], failure_threshold = {threshold}, window_secs = {window_secs} }}
"#
        ))
        .unwrap();
        RpcProxy::new(config.routes)
    }

    #[test]
    fn test_health_starts_empty() {
        let h = UpstreamHealth::new();
        assert_eq!(h.recent_failures(Duration::from_mins(1)), 0);
    }

    #[test]
    fn test_health_records_failures() {
        let h = UpstreamHealth::new();
        h.record_failure();
        h.record_failure();
        assert_eq!(h.recent_failures(Duration::from_mins(1)), 2);
    }

    #[test]
    fn test_record_success_decays_one_failure_not_all() {
        let h = UpstreamHealth::new();
        h.record_failure();
        h.record_failure();
        assert_eq!(h.recent_failures(Duration::from_mins(1)), 2);
        h.record_success();
        assert_eq!(h.recent_failures(Duration::from_mins(1)), 1);
        h.record_success();
        assert_eq!(h.recent_failures(Duration::from_mins(1)), 0);
        h.record_success();
        assert_eq!(h.recent_failures(Duration::from_mins(1)), 0);
    }

    #[test]
    fn test_select_primary_when_healthy() {
        let proxy = make_proxy(3, 60);
        let state = proxy.routes.values().next().unwrap();
        assert_eq!(state.select(), 0);
    }

    #[test]
    fn test_select_failover_when_primary_degraded() {
        let proxy = make_proxy(3, 60);
        let state = proxy.routes.values().next().unwrap();
        for _ in 0..3 {
            state.health[0].record_failure();
        }
        assert_eq!(state.select(), 1);
    }

    #[test]
    fn test_select_defaults_to_primary_when_all_degraded() {
        let proxy = make_proxy(2, 60);
        let state = proxy.routes.values().next().unwrap();
        for h in &state.health {
            for _ in 0..2 {
                h.record_failure();
            }
        }
        assert_eq!(state.select(), 0);
    }

    #[test]
    fn test_match_route_exact() {
        let proxy = make_proxy(3, 60);
        assert!(proxy.match_route("/test").is_some());
    }

    #[test]
    fn test_match_route_subpath() {
        let proxy = make_proxy(3, 60);
        assert!(proxy.match_route("/test/foo").is_some());
    }

    #[test]
    fn test_match_route_rejects_partial_prefix() {
        let proxy = make_proxy(3, 60);
        assert!(proxy.match_route("/testing").is_none());
    }

    #[test]
    fn test_match_route_unknown_returns_none() {
        let proxy = make_proxy(3, 60);
        assert!(proxy.match_route("/eth").is_none());
    }
}
