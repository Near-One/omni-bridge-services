use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use clap::Parser;
use tracing::info;

use wormhole_vaa_api::api::{self, AppState};
use wormhole_vaa_api::config::{CliArgs, Config};
use wormhole_vaa_api::metrics::{self, Metrics};
use wormhole_vaa_api::proxy_client::ProxyClient;
use wormhole_vaa_api::resolver::Resolver;
use wormhole_vaa_api::spy::{self, SpyStatus};
use wormhole_vaa_api::store::Store;

const PROXY_RPC_TIMEOUT: Duration = Duration::from_secs(20);

#[tokio::main]
async fn main() -> Result<()> {
    dotenv::dotenv().ok();

    let args = CliArgs::parse();
    let config = Arc::new(Config::load(args.config.clone())?);

    metrics::init_logging(args.network)?;
    metrics::init_metrics(args.network);

    info!(
        "Starting wormhole-vaa-api v{} on {} port {}",
        env!("CARGO_PKG_VERSION"),
        args.network,
        args.port
    );

    let store = Store::connect(
        &config.redis_url,
        config.vaa_ttl_secs,
        config.txres_ttl_secs,
    )
    .await
    .map_err(|e| anyhow::anyhow!("connecting to Redis: {e}"))?;
    let proxy = ProxyClient::new(&config.proxy_base_url, PROXY_RPC_TIMEOUT)?;
    let resolver = Resolver::from_config(&config, proxy)?;
    let spy_status = Arc::new(SpyStatus::default());

    // Metrics handles must be built after `init_metrics` set the global provider.
    let app_metrics = Metrics::new();
    metrics::register_spy_gauges(Arc::clone(&spy_status));

    let spy_handle = tokio::spawn(spy::run(
        Arc::clone(&config),
        store.clone(),
        Arc::clone(&spy_status),
    ));

    let state = AppState {
        store,
        resolver,
        spy_status,
        config: Arc::clone(&config),
        metrics: app_metrics,
    };
    let app = api::router(state);

    let addr = SocketAddr::from(([0, 0, 0, 0], args.port));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!("listening on {addr}");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    spy_handle.abort();
    Ok(())
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    info!("shutdown signal received, draining");
}
