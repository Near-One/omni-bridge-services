use serde::Deserialize;
use serde_json::json;

use crate::config::Config;
use crate::errors::DynamicConfigError;

/// TODO: add support for these on backend
const DEFAULT_FAILOVER_STATUS_CODES: [u16; 5] = [429, 500, 502, 503, 504];

#[derive(Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum ConnType {
    Http,
    Ws,
    Grpc,
    Graphql,
}

impl ConnType {
    fn as_str(self) -> &'static str {
        match self {
            ConnType::Http => "http",
            ConnType::Ws => "ws",
            ConnType::Grpc => "grpc",
            ConnType::Graphql => "graphql",
        }
    }
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct UpstreamDto {
    url: String,
    timeout_ms: Option<u32>,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct RouteDto {
    blockchain: String,
    conn_type: ConnType,
    failure_threshold: u32,
    window_secs: u64,
    upstreams: Vec<UpstreamDto>,
}

/// `http` is the implicit/default connection type and is omitted from the path,
/// any other connection type is kept explicit.
fn route_prefix(blockchain: &str, conn_type: &str) -> String {
    if conn_type == "http" {
        format!("/{blockchain}")
    } else {
        format!("/{conn_type}/{blockchain}")
    }
}

fn assemble_config_value(routes: Vec<RouteDto>) -> serde_json::Value {
    let route_values: Vec<serde_json::Value> = routes
        .into_iter()
        .map(|route| {
            let upstreams: Vec<serde_json::Value> = route
                .upstreams
                .into_iter()
                .map(|u| {
                    json!({
                        "url": u.url,
                        "timeout_ms": u.timeout_ms,
                    })
                })
                .collect();
            json!({
                "prefix": route_prefix(&route.blockchain, route.conn_type.as_str()),
                "upstreams": upstreams,
                "failover": {
                    "status_codes": DEFAULT_FAILOVER_STATUS_CODES,
                    "rpc_codes": Vec::<i32>::new(),
                    "failure_threshold": route.failure_threshold,
                    "window_secs": route.window_secs,
                },
            })
        })
        .collect();

    json!({ "routes": route_values })
}

pub async fn fetch_config(
    client: &reqwest::Client,
    base_url: &str,
    jwt: &str,
) -> Result<Config, DynamicConfigError> {
    let routes: Vec<RouteDto> = client
        .get(format!("{base_url}/admin/route"))
        .bearer_auth(jwt)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let value = assemble_config_value(routes);
    Ok(Config::from_dynamic_value(value)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::MockServer;
    use serde_json::Value;

    fn route_dto_json(
        blockchain: &str,
        conn_type: &str,
        failure_threshold: u32,
        window_secs: u64,
        upstreams: Vec<Value>,
    ) -> Value {
        json!({
            "id": 1,
            "service": "test-service",
            "blockchain": blockchain,
            "connType": conn_type,
            "keyHash": "irrelevant",
            "failureThreshold": failure_threshold,
            "windowSecs": window_secs,
            "createdAt": "2026-01-01T00:00:00Z",
            "updatedAt": "2026-01-01T00:00:00Z",
            "upstreams": upstreams,
        })
    }

    fn upstream_dto_json(url: &str, timeout_ms: Option<u32>, sort_order: u32) -> Value {
        json!({
            "id": 1,
            "routeId": 1,
            "url": url,
            "timeoutMs": timeout_ms,
            "sortOrder": sort_order,
        })
    }

    #[test]
    fn test_assemble_preserves_upstream_order() {
        let value = json!([route_dto_json(
            "eth",
            "http",
            3,
            60,
            vec![
                upstream_dto_json("http://a.example.com", None, 0),
                upstream_dto_json("http://b.example.com", Some(500), 1),
            ],
        )]);
        let routes: Vec<RouteDto> = serde_json::from_value(value).unwrap();
        let config = Config::from_dynamic_value(assemble_config_value(routes)).unwrap();
        assert_eq!(config.routes.len(), 1);
        assert_eq!(config.routes[0].upstreams().len(), 2);
        assert_eq!(config.routes[0].upstreams()[0].addr(), "a.example.com:80");
        assert_eq!(config.routes[0].upstreams()[1].addr(), "b.example.com:80");
    }

    #[test]
    fn test_assemble_groups_multiple_routes() {
        let value = json!([
            route_dto_json("eth", "http", 3, 60, vec![upstream_dto_json("http://a.example.com", None, 0)]),
            route_dto_json("arb", "http", 3, 60, vec![upstream_dto_json("http://b.example.com", None, 0)]),
        ]);
        let routes: Vec<RouteDto> = serde_json::from_value(value).unwrap();
        let config = Config::from_dynamic_value(assemble_config_value(routes)).unwrap();
        assert_eq!(config.routes.len(), 2);
    }

    #[test]
    fn test_assemble_applies_default_failover_status_codes() {
        let value = json!([route_dto_json(
            "eth",
            "http",
            2,
            30,
            vec![upstream_dto_json("http://a.example.com", None, 0)],
        )]);
        let routes: Vec<RouteDto> = serde_json::from_value(value).unwrap();
        let config = Config::from_dynamic_value(assemble_config_value(routes)).unwrap();
        for code in DEFAULT_FAILOVER_STATUS_CODES {
            assert!(config.routes[0].failover().is_failure_status(code));
        }
        assert!(!config.routes[0].failover().is_failure_rpc_code(-32000));
    }

    #[test]
    fn test_assemble_rejects_empty_upstreams_via_validate() {
        let value = json!([route_dto_json("eth", "http", 3, 60, vec![])]);
        let routes: Vec<RouteDto> = serde_json::from_value(value).unwrap();
        assert!(Config::from_dynamic_value(assemble_config_value(routes)).is_err());
    }

    #[test]
    fn test_route_prefix_omits_default_http_conn_type() {
        assert_eq!(route_prefix("eth", "http"), "/eth");
        assert_eq!(route_prefix("solana", "ws"), "/ws/solana");
    }

    #[tokio::test]
    async fn test_fetch_config_sends_bearer_token_and_parses_response() {
        let server = MockServer::start_async().await;
        let mock = server
            .mock_async(|when, then| {
                when.method(httpmock::Method::GET)
                    .path("/admin/route")
                    .header("Authorization", "Bearer test-jwt");
                then.status(200).json_body(json!([route_dto_json(
                    "eth",
                    "http",
                    3,
                    60,
                    vec![upstream_dto_json("http://a.example.com", None, 0)],
                )]));
            })
            .await;

        let client = reqwest::Client::new();
        let config = fetch_config(&client, &server.base_url(), "test-jwt")
            .await
            .expect("fetch should succeed");

        mock.assert_async().await;
        assert_eq!(config.routes.len(), 1);
        assert_eq!(config.routes[0].prefix().as_str(), "/eth");
    }

    #[tokio::test]
    async fn test_fetch_config_propagates_http_error_status() {
        let server = MockServer::start_async().await;
        server
            .mock_async(|when, then| {
                when.method(httpmock::Method::GET).path("/admin/route");
                then.status(401);
            })
            .await;

        let client = reqwest::Client::new();
        let result = fetch_config(&client, &server.base_url(), "test-jwt").await;
        assert!(matches!(result, Err(DynamicConfigError::Http(_))));
    }
}
