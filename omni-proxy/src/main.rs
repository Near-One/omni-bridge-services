use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use base64::{Engine, engine::general_purpose};
use clap::Parser;
use tracing::{info, warn};
use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};
use url::Url;

use omni_proxy::{
    config::{Config, Network},
    dynamic_config,
    errors::LoggerError,
    proxy::{RoutesHandle, RpcProxy},
};

const DYNAMIC_CONFIG_REFRESH_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Parser)]
struct CliArgs {
    #[clap(long)]
    network: Network,
    #[clap(long)]
    config: Option<PathBuf>,
    #[clap(long, default_value = "8080")]
    port: u16,
    /// Enable periodic fetching of routes from config service instead of config file.
    #[clap(long)]
    dynamic_config: bool,
}

fn init_logging(network: &Network) -> Result<(), LoggerError> {
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
            "Running without Loki due to missing one of `GRAFANA_LOKI_URL`, `GRAFANA_LOKI_USER` or `GRAFANA_CLOUD_API_KEY` environment variables"
        );
    }

    Ok(())
}

fn init_metrics(network: &Network) {
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
    });

    match rx.recv() {
        Ok(Ok(())) => info!("OTLP metrics enabled"),
        Ok(Err(e)) => warn!("Metrics disabled: failed to initialize OTLP exporter: {e}"),
        Err(_) => warn!("Metrics disabled: setup thread exited before initialization"),
    }
}

fn spawn_dynamic_config_refresher(
    client: reqwest::Client,
    config_service_url: String,
    config_service_jwt: String,
    routes_handle: RoutesHandle,
) {
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("dynamic-config refresher runtime");

        rt.block_on(async move {
            // First tick completes immediately, skip as fetch is firstly done in main().
            let mut interval = tokio::time::interval(DYNAMIC_CONFIG_REFRESH_INTERVAL);
            interval.tick().await;

            loop {
                interval.tick().await;
                match dynamic_config::fetch_config(&client, &config_service_url, &config_service_jwt).await {
                    Ok(config) => {
                        routes_handle.apply(config.routes);
                        info!("dynamic config: route table reloaded");
                    }
                    Err(e) => {
                        warn!("dynamic config: refresh failed, keeping last-known-good routes: {e}");
                    }
                }
            }
        });
    });
}

fn main() -> Result<()> {
    dotenv::dotenv().ok();

    let args = CliArgs::parse();

    let config_service_url = std::env::var("CONFIG_SERVICE_URL").ok();
    let config_service_jwt = std::env::var("CONFIG_SERVICE_JWT").ok();

    if args.dynamic_config && args.config.is_some() {
        anyhow::bail!("--config cannot be used together with --dynamic-config, select one");
    }
    if args.dynamic_config && (config_service_url.is_none() || config_service_jwt.is_none()) {
        anyhow::bail!(
            "--dynamic-config requires CONFIG_SERVICE_URL and CONFIG_SERVICE_JWT env vars"
        );
    }
    if !args.dynamic_config && args.config.is_none() {
        anyhow::bail!("--config is required unless --dynamic-config is set");
    }

    init_logging(&args.network)?;

    let http_client = reqwest::Client::new();

    // Fetch config initially
    let config = if args.dynamic_config {
        let config_service_url = config_service_url.clone().expect("validated above");
        let config_service_jwt = config_service_jwt.clone().expect("validated above");
        let http_client = http_client.clone();
        let init_rt = tokio::runtime::Runtime::new()?;
        init_rt.block_on(async move {
            dynamic_config::fetch_config(&http_client, &config_service_url, &config_service_jwt).await
        })?
    } else {
        Config::load_config(args.config.expect("validated above"))?
    };

    init_metrics(&args.network);

    info!(
        "Starting omni-proxy v{} on {} port {}",
        env!("CARGO_PKG_VERSION"),
        args.network,
        args.port
    );

    let mut server = pingora::server::Server::new(None).unwrap();
    server.bootstrap();

    let proxy = RpcProxy::new(config.routes);
    if args.dynamic_config {
        let config_service_url = config_service_url.expect("validated above");
        let config_service_jwt = config_service_jwt.expect("validated above");
        spawn_dynamic_config_refresher(
            http_client,
            config_service_url,
            config_service_jwt,
            proxy.routes_handle(),
        );
    }

    let mut svc = pingora::proxy::http_proxy_service(&server.configuration, proxy);
    svc.add_tcp(&format!("0.0.0.0:{}", args.port));
    server.add_service(svc);
    server.run_forever();
}
