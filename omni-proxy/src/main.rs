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
    errors::LoggerError,
    proxy::RpcProxy,
};

#[derive(Parser)]
struct CliArgs {
    #[clap(long)]
    network: Network,
    #[clap(long)]
    config: PathBuf,
    #[clap(long, default_value = "8080")]
    port: u16,
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

fn init_metrics() {
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

    let (tx, rx) = std::sync::mpsc::channel::<Result<()>>();

    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("metrics runtime");

        rt.block_on(async move {
            let result = (|| -> Result<()> {
                use opentelemetry_otlp::{WithExportConfig, WithHttpConfig};
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

                let provider = SdkMeterProvider::builder().with_reader(reader).build();
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

fn main() -> Result<()> {
    dotenv::dotenv().ok();

    let args = CliArgs::parse();
    let config = Config::load_config(args.config)?;

    init_logging(&args.network)?;
    init_metrics();

    info!(
        "Starting omni-proxy v{} on {} port {}",
        env!("CARGO_PKG_VERSION"),
        args.network,
        args.port
    );

    let mut server = pingora::server::Server::new(None).unwrap();
    server.bootstrap();

    let proxy = RpcProxy::new(config.routes);
    let mut svc = pingora::proxy::http_proxy_service(&server.configuration, proxy);
    svc.add_tcp(&format!("0.0.0.0:{}", args.port));
    server.add_service(svc);
    server.run_forever();
}
