//! axum REST service: the two wormholescan endpoints the relayer calls, plus health.
//!
//! Status contract (drives the proxy's primary/fallback behavior):
//!   * `200` only with a real, fully-signed VAA in the exact shapes.
//!   * `404` when we don't have it (proxy fails over to wormholescan).
//!   * `5xx` on internal errors (Redis/all-probes-down → proxy fails over).
//!
//! `vaa` is standard padded base64, matching wormholescan exactly.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::warn;

use crate::address::{emitter_hex, normalize_emitter};
use crate::config::Config;
use crate::metrics::Metrics;
use crate::resolver::{Resolution, Resolver, normalize_hash};
use crate::spy::SpyStatus;
use crate::store::Store;

#[derive(Clone)]
pub struct AppState {
    pub store: Store,
    pub resolver: Resolver,
    pub spy_status: Arc<SpyStatus>,
    pub config: Arc<Config>,
    pub metrics: Metrics,
}

#[derive(Serialize)]
struct VaaData {
    vaa: String,
}

#[derive(Serialize)]
struct SingleResponse {
    data: VaaData,
}

#[derive(Serialize)]
struct ListResponse {
    data: Vec<VaaData>,
}

/// Cache DTO for `txres:` entries (emitter as hex so the JSON stays compact/stable).
#[derive(Serialize, Deserialize)]
struct CachedResolution {
    chain: u16,
    emitter: String,
    sequence: u64,
}

impl CachedResolution {
    fn from_resolution(r: &Resolution) -> Self {
        Self {
            chain: r.chain,
            emitter: emitter_hex(&r.emitter),
            sequence: r.sequence,
        }
    }

    fn to_resolution(&self) -> Option<Resolution> {
        Some(Resolution {
            chain: self.chain,
            emitter: normalize_emitter(&self.emitter).ok()?,
            sequence: self.sequence,
        })
    }
}

// Note: no inbound concurrency-limit or request-timeout layer is applied. The only
// client is the trusted in-cluster relayer (via omni-proxy), and each handler's
// outbound work is already time-bounded (PROXY_RPC_TIMEOUT on resolver probes). If this
// service is ever exposed more broadly, add a tower request-timeout / concurrency layer.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/api/v1/vaas/:chain/:emitter/:sequence", get(get_vaa))
        .route("/api/v1/vaas/", get(get_vaa_by_tx))
        .route("/api/v1/vaas", get(get_vaa_by_tx))
        .route("/healthz", get(healthz))
        .with_state(state)
}

/// Endpoint (1): `GET /api/v1/vaas/{chain}/{emitter}/{sequence}`.
async fn get_vaa(
    State(state): State<AppState>,
    Path((chain, emitter, sequence)): Path<(String, String, String)>,
) -> Response {
    // Parse segments in-handler so malformed input yields our 404 envelope rather than
    // axum's default 400 (which is outside the 200/404/5xx contract).
    let (Ok(chain), Ok(emitter), Ok(sequence)) = (
        chain.parse::<u16>(),
        normalize_emitter(&emitter),
        sequence.parse::<u64>(),
    ) else {
        return record(&state, "by_id", not_found_single());
    };

    match state.store.get_vaa(chain, &emitter, sequence).await {
        Ok(Some(bytes)) => {
            state.metrics.vaa_lookups.add(1, &[kv("result", "hit")]);
            record(&state, "by_id", ok_single(&bytes))
        }
        Ok(None) => {
            state.metrics.vaa_lookups.add(1, &[kv("result", "miss")]);
            record(&state, "by_id", not_found_single())
        }
        Err(e) => {
            warn!("store error on get_vaa: {e}");
            record(&state, "by_id", internal_error())
        }
    }
}

#[derive(Deserialize)]
struct TxQuery {
    #[serde(rename = "txHash")]
    tx_hash: Option<String>,
}

/// Endpoint (2): `GET /api/v1/vaas/?txHash=<hash>`.
async fn get_vaa_by_tx(State(state): State<AppState>, Query(q): Query<TxQuery>) -> Response {
    let Some(hash) = q.tx_hash.filter(|h| !h.is_empty()) else {
        return record(&state, "by_tx", not_found_list());
    };

    let resolutions = match resolve_with_cache(&state, &hash).await {
        Ok(r) => r,
        Err(e) => {
            warn!("resolve error for {hash}: {e}");
            state.metrics.resolutions.add(1, &[kv("outcome", "error")]);
            return record(&state, "by_tx", internal_error());
        }
    };

    // Look up each resolved identity in the store, preserving wormholescan's order.
    let mut data = Vec::new();
    for r in &resolutions {
        match state.store.get_vaa(r.chain, &r.emitter, r.sequence).await {
            Ok(Some(bytes)) => data.push(VaaData {
                vaa: STANDARD.encode(bytes),
            }),
            Ok(None) => {}
            Err(e) => {
                warn!("store error on get_vaa_by_tx: {e}");
                return record(&state, "by_tx", internal_error());
            }
        }
    }

    if data.is_empty() {
        record(&state, "by_tx", not_found_list())
    } else {
        record(
            &state,
            "by_tx",
            (StatusCode::OK, Json(ListResponse { data })).into_response(),
        )
    }
}

/// Resolve a txHash, consulting (and populating) the `txres:` cache.
async fn resolve_with_cache(state: &AppState, hash: &str) -> anyhow::Result<Vec<Resolution>> {
    let key = normalize_hash(hash);

    if let Ok(Some(cached_json)) = state.store.get_txres(&key).await
        && let Ok(cached) = serde_json::from_str::<Vec<CachedResolution>>(&cached_json)
    {
        state
            .metrics
            .resolutions
            .add(1, &[kv("outcome", "cache_hit")]);
        return Ok(cached
            .iter()
            .filter_map(CachedResolution::to_resolution)
            .collect());
    }

    let resolutions = state.resolver.resolve(hash).await?;

    if resolutions.is_empty() {
        state.metrics.resolutions.add(1, &[kv("outcome", "empty")]);
    } else {
        state
            .metrics
            .resolutions
            .add(1, &[kv("outcome", "resolved")]);
        let cached: Vec<CachedResolution> = resolutions
            .iter()
            .map(CachedResolution::from_resolution)
            .collect();
        if let Ok(json) = serde_json::to_string(&cached)
            && let Err(e) = state.store.put_txres(&key, &json).await
        {
            warn!("failed to cache resolution for {hash}: {e}");
        }
    }

    Ok(resolutions)
}

async fn healthz(State(state): State<AppState>) -> Response {
    let spy_ok = state.spy_status.is_connected();
    let redis_ok = state.store.ping().await.is_ok();
    if spy_ok && redis_ok {
        (StatusCode::OK, "ok").into_response()
    } else {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "spy_connected": spy_ok, "redis_ok": redis_ok })),
        )
            .into_response()
    }
}

fn ok_single(bytes: &[u8]) -> Response {
    (
        StatusCode::OK,
        Json(SingleResponse {
            data: VaaData {
                vaa: STANDARD.encode(bytes),
            },
        }),
    )
        .into_response()
}

/// Endpoint (1) miss: wormholescan-style 404 error envelope.
fn not_found_single() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({ "code": 5, "message": "NOT FOUND" })),
    )
        .into_response()
}

/// Endpoint (2) miss: 404 with an empty data array (the SDK parses it as "no VAA yet").
fn not_found_list() -> Response {
    (StatusCode::NOT_FOUND, Json(json!({ "data": [] }))).into_response()
}

fn internal_error() -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "code": 13, "message": "INTERNAL" })),
    )
        .into_response()
}

/// Record per-request metrics and pass the response through.
fn record(state: &AppState, endpoint: &'static str, resp: Response) -> Response {
    let status = resp.status();
    let outcome = if status.is_success() {
        "ok"
    } else if status.is_server_error() {
        "error"
    } else {
        "miss"
    };
    state.metrics.http_requests.add(
        1,
        &[
            kv("endpoint", endpoint),
            opentelemetry::KeyValue::new("status", i64::from(status.as_u16())),
            kv("outcome", outcome),
        ],
    );
    resp
}

fn kv(key: &'static str, value: &'static str) -> opentelemetry::KeyValue {
    opentelemetry::KeyValue::new(key, value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_response_matches_wormholescan_shape() {
        let resp = SingleResponse {
            data: VaaData {
                vaa: STANDARD.encode([1u8, 2, 3]),
            },
        };
        assert_eq!(
            serde_json::to_value(&resp).unwrap(),
            json!({ "data": { "vaa": "AQID" } })
        );
    }

    #[test]
    fn list_response_matches_wormholescan_shape() {
        let resp = ListResponse {
            data: vec![VaaData {
                vaa: "AQID".to_owned(),
            }],
        };
        assert_eq!(
            serde_json::to_value(&resp).unwrap(),
            json!({ "data": [{ "vaa": "AQID" }] })
        );
    }

    #[test]
    fn cached_resolution_roundtrips_through_json() {
        let r = Resolution {
            chain: 1,
            emitter: [0xAB; 32],
            sequence: 42597,
        };
        let json = serde_json::to_string(&[CachedResolution::from_resolution(&r)]).unwrap();
        let back: Vec<CachedResolution> = serde_json::from_str(&json).unwrap();
        assert_eq!(back[0].to_resolution(), Some(r));
    }

    #[test]
    fn router_routes_do_not_conflict() {
        // Registering overlapping paths panics in axum/matchit on conflict; building
        // the router here exercises that without needing Redis/state.
        let _router: Router<AppState> = Router::new()
            .route("/api/v1/vaas/:chain/:emitter/:sequence", get(get_vaa))
            .route("/api/v1/vaas/", get(get_vaa_by_tx))
            .route("/api/v1/vaas", get(get_vaa_by_tx))
            .route("/healthz", get(healthz));
    }
}
