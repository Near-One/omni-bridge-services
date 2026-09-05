use std::collections::HashMap;
use std::future::Future;
use std::time::Duration;

use anyhow::{Context, Result};
use base64::{Engine, engine::general_purpose};
use opentelemetry::KeyValue;
use tracing::{error, info, warn};
use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};
use url::Url;

use omni_proxy::config::{Config, Network};
use omni_proxy::dynamic_config;
use omni_proxy::errors::LoggerError;
use omni_proxy::proxy::RoutesHandle;

const DYNAMIC_CONFIG_REFRESH_INTERVAL: Duration = Duration::from_mins(1);

/// Shared runtime & http client for background tasks
///
/// Separate from pingora runtime.
/// If dropped, cancels tasks.
pub struct BackgroundTaskManager {
    rt: tokio::runtime::Runtime,
    http_client: reqwest::Client,
}

impl BackgroundTaskManager {
    pub fn new() -> Result<Self> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(3)
            .enable_all()
            .build()?;
        let http_client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(10))
            .build()?;
        Ok(Self { rt, http_client })
    }

    /// Spawns `future` and logs if it ever stops as every task run this way is expected to
    /// run forever, so a dropped handle would otherwise let a panic kill the task silently.
    fn spawn_supervised<F>(&self, name: &'static str, future: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let handle = self.rt.spawn(future);
        self.rt.spawn(async move {
            match handle.await {
                Ok(()) => {
                    warn!("background task `{name}` exited unexpectedly (expected to run forever)");
                }
                Err(e) => error!("background task `{name}` panicked: {e}"),
            }
        });
    }

    pub fn init_logging(&self, network: &Network) -> Result<(), LoggerError> {
        let fmt_layer = fmt::Layer::default()
            .with_timer(fmt::time::ChronoLocal::rfc_3339())
            .with_target(false);
        let filter_layer =
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

        let grafana_loki_url = std::env::var("GRAFANA_LOKI_URL").ok();
        let grafana_loki_user = std::env::var("GRAFANA_LOKI_USER").ok();
        let grafana_api_key = std::env::var("GRAFANA_CLOUD_API_KEY").ok();

        if let (Some(url), Some(user), Some(key)) =
            (grafana_loki_url, grafana_loki_user, grafana_api_key)
        {
            let basic = format!("{user}:{key}");
            let encoded = general_purpose::STANDARD.encode(basic);
            let base = Url::parse(&url)?;

            let (loki_layer, loki_task) = tracing_loki::builder()
                .label("app", format!("omni-proxy-{network}"))?
                .http_header("Authorization", format!("Basic {encoded}"))?
                .build_url(base)?;

            tracing_subscriber::registry()
                .with(filter_layer)
                .with(fmt_layer)
                .with(loki_layer)
                .try_init()?;

            self.spawn_supervised("loki-task", loki_task);

            info!("Loki logging enabled");
        } else {
            tracing_subscriber::registry()
                .with(filter_layer)
                .with(fmt_layer)
                .try_init()?;

            warn!(
                "Running without Loki due to missing one of `GRAFANA_LOKI_URL`, `GRAFANA_LOKI_USER` or `GRAFANA_CLOUD_API_KEY` environment variables"
            );
        }

        Ok(())
    }

    pub fn init_metrics(&self, network: &Network) {
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

        self.spawn_supervised("otlp-metrics", async move {
            let result = (|| -> Result<()> {
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
                    KeyValue::new("service.name", "omni-proxy"),
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

        match rx.recv() {
            Ok(Ok(())) => info!("OTLP metrics enabled"),
            Ok(Err(e)) => warn!("Metrics disabled: failed to initialize OTLP exporter: {e}"),
            Err(_) => warn!("Metrics disabled: setup thread exited before initialization"),
        }
    }

    pub fn init_dynamic_config_refresher(&self, routes_handle: RoutesHandle) -> Result<()> {
        let config_service_url =
            std::env::var("CONFIG_SERVICE_URL").context("CONFIG_SERVICE_URL must be set")?;
        let config_service_jwt =
            std::env::var("CONFIG_SERVICE_JWT").context("CONFIG_SERVICE_JWT must be set")?;

        let meter = opentelemetry::global::meter("omni-proxy");
        let refresh_outcomes = meter
            .u64_counter("dynamic_config_refresh_total")
            .with_description("Dynamic-config refresh attempts by outcome (success/failure)")
            .build();

        let http_client = self.http_client.clone();

        self.spawn_supervised("dynamic-config-refresher", async move {
            // First tick completes immediately, skip as fetch is firstly done in main().
            let mut interval = tokio::time::interval(DYNAMIC_CONFIG_REFRESH_INTERVAL);
            interval.tick().await;

            loop {
                interval.tick().await;
                match dynamic_config::fetch_config(
                    &http_client,
                    &config_service_url,
                    &config_service_jwt,
                )
                .await
                {
                    Ok(config) => {
                        routes_handle.apply(config.routes);
                        refresh_outcomes.add(1, &[KeyValue::new("outcome", "success")]);
                        info!("dynamic config: route table reloaded");
                    }
                    Err(e) => {
                        refresh_outcomes.add(1, &[KeyValue::new("outcome", "failure")]);
                        warn!(
                            "dynamic config: refresh failed, keeping last-known-good routes: {e}"
                        );
                    }
                }
            }
        });

        Ok(())
    }

    pub fn load_dynamic_config_blocking(&self) -> Result<Config> {
        let config_service_url =
            std::env::var("CONFIG_SERVICE_URL").context("CONFIG_SERVICE_URL must be set")?;
        let config_service_jwt =
            std::env::var("CONFIG_SERVICE_JWT").context("CONFIG_SERVICE_JWT must be set")?;

        let http_client = self.http_client.clone();

        Ok(self.rt.block_on(async move {
            dynamic_config::fetch_config(&http_client, &config_service_url, &config_service_jwt)
                .await
        })?)
    }
}
