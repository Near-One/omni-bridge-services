//! Observability: tracing→Loki logging and OTLP metrics push, matching omni-proxy's
//! conventions (gated on the same `GRAFANA_*` env vars), plus the application metric
//! handles. There is no Prometheus scrape endpoint — metrics are pushed via OTLP.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use base64::{Engine, engine::general_purpose};
use opentelemetry::metrics::Counter;
use tracing::{info, warn};
use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};
use url::Url;

use crate::config::Network;
use crate::spy::SpyStatus;

const SERVICE_NAME: &str = "wormhole-vaa-api";

/// Application metric handles (OTLP).
#[derive(Clone)]
pub struct Metrics {
    /// REST requests, labeled by `endpoint`, `status`, `outcome`.
    pub http_requests: Counter<u64>,
    /// Store lookups, labeled by `result` (`hit`/`miss`).
    pub vaa_lookups: Counter<u64>,
    /// txHash resolutions, labeled by `outcome` (`resolved`/`empty`/`error`/`cache_hit`).
    pub resolutions: Counter<u64>,
}

impl Metrics {
    #[must_use]
    pub fn new() -> Self {
        let meter = opentelemetry::global::meter(SERVICE_NAME);
        Self {
            http_requests: meter
                .u64_counter("wvaa_http_requests_total")
                .with_description("REST requests by endpoint, status, outcome")
                .build(),
            vaa_lookups: meter
                .u64_counter("wvaa_store_lookups_total")
                .with_description("VAA store lookups by result (hit/miss)")
                .build(),
            resolutions: meter
                .u64_counter("wvaa_txhash_resolutions_total")
                .with_description("txHash resolutions by outcome")
                .build(),
        }
    }
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

/// Register observable gauges that read live spy state. Call after `init_metrics`.
pub fn register_spy_gauges(status: Arc<SpyStatus>) {
    let meter = opentelemetry::global::meter(SERVICE_NAME);

    let connected = Arc::clone(&status);
    meter
        .u64_observable_gauge("wvaa_spy_connected")
        .with_description("1 when the spy stream is connected, else 0")
        .with_callback(move |observer| observer.observe(u64::from(connected.is_connected()), &[]))
        .build();

    meter
        .u64_observable_gauge("wvaa_spy_vaas_ingested")
        .with_description("Total VAAs ingested from the spy since start")
        .with_callback(move |observer| observer.observe(status.total_ingested(), &[]))
        .build();
}

pub fn init_logging(network: Network) -> Result<()> {
    let fmt_layer = fmt::Layer::default()
        .with_timer(fmt::time::ChronoLocal::rfc_3339())
        .with_target(false);
    let filter_layer = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    let grafana_loki_url = std::env::var("GRAFANA_LOKI_URL").ok();
    let grafana_loki_user = std::env::var("GRAFANA_LOKI_USER").ok();
    let grafana_api_key = std::env::var("GRAFANA_CLOUD_API_KEY").ok();

    if let (Some(url), Some(user), Some(key)) =
        (grafana_loki_url, grafana_loki_user, grafana_api_key)
    {
        let encoded = general_purpose::STANDARD.encode(format!("{user}:{key}"));
        let base = Url::parse(&url)?;

        let (loki_layer, loki_task) = tracing_loki::builder()
            .label("app", format!("{SERVICE_NAME}-{network}"))?
            .http_header("Authorization", format!("Basic {encoded}"))?
            .build_url(base)?;

        tracing_subscriber::registry()
            .with(filter_layer)
            .with(fmt_layer)
            .with(loki_layer)
            .try_init()?;

        std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(loki_task);
        });

        info!("Loki logging enabled");
    } else {
        tracing_subscriber::registry()
            .with(filter_layer)
            .with(fmt_layer)
            .try_init()?;
        warn!(
            "Running without Loki (set GRAFANA_LOKI_URL, GRAFANA_LOKI_USER, GRAFANA_CLOUD_API_KEY to enable)"
        );
    }
    Ok(())
}

pub fn init_metrics(network: Network) {
    let otlp_url = std::env::var("GRAFANA_OTLP_URL").ok();
    let otlp_user = std::env::var("GRAFANA_OTLP_USER").ok();
    let api_key = std::env::var("GRAFANA_CLOUD_API_KEY").ok();

    let (endpoint, auth) = if let (Some(url), Some(user), Some(key)) =
        (otlp_url, otlp_user, api_key)
    {
        let auth = format!(
            "Basic {}",
            general_purpose::STANDARD.encode(format!("{user}:{key}"))
        );
        (url, auth)
    } else {
        warn!(
            "Metrics disabled: GRAFANA_OTLP_URL, GRAFANA_OTLP_USER or GRAFANA_CLOUD_API_KEY not set"
        );
        return;
    };

    let network = network.to_string();
    let (tx, rx) = std::sync::mpsc::channel::<Result<()>>();

    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("metrics runtime");

        rt.block_on(async move {
            let result = (|| -> Result<()> {
                use opentelemetry::KeyValue;
                use opentelemetry_otlp::{WithExportConfig, WithHttpConfig};
                use opentelemetry_sdk::Resource;
                use opentelemetry_sdk::metrics::{PeriodicReader, SdkMeterProvider};

                let mut headers = HashMap::new();
                headers.insert("Authorization".to_owned(), auth);

                let exporter = opentelemetry_otlp::MetricExporter::builder()
                    .with_http()
                    .with_endpoint(endpoint)
                    .with_headers(headers)
                    .build()
                    .map_err(|e| anyhow::anyhow!("OTLP exporter: {e}"))?;

                let reader = PeriodicReader::builder(exporter, opentelemetry_sdk::runtime::Tokio)
                    .with_interval(Duration::from_secs(30))
                    .build();

                let mut attributes = vec![
                    KeyValue::new("service.name", SERVICE_NAME),
                    KeyValue::new("network", network),
                ];
                if let Ok(cluster) = std::env::var("CLUSTER_NAME") {
                    attributes.push(KeyValue::new("k8s.cluster.name", cluster));
                }
                if let Ok(pod) = std::env::var("POD_NAME") {
                    attributes.push(KeyValue::new("k8s.pod.name", pod));
                }
                let resource = Resource::new(attributes);
                let provider = SdkMeterProvider::builder()
                    .with_reader(reader)
                    .with_resource(resource)
                    .build();
                opentelemetry::global::set_meter_provider(provider);
                Ok(())
            })();

            tx.send(result).ok();
            std::future::pending::<()>().await;
        });
    });

    match rx.recv() {
        Ok(Ok(())) => info!("OTLP metrics enabled"),
        Ok(Err(e)) => warn!("Metrics disabled: failed to initialize OTLP exporter: {e}"),
        Err(_) => warn!("Metrics disabled: setup thread exited before initialization"),
    }
}
