mod background;

use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use tracing::info;

use background::BackgroundTaskManager;
use omni_proxy::{
    config::{Config, Network},
    proxy::RpcProxy,
};

#[derive(Parser)]
struct CliArgs {
    #[clap(long)]
    network: Network,
    #[clap(long)]
    config: Option<PathBuf>,
    #[clap(long, default_value = "8080")]
    port: u16,
    /// Use periodic fetching of routes from config service instead of config file.
    #[clap(long)]
    dynamic_config: bool,
}

fn main() -> Result<()> {
    dotenv::dotenv().ok();

    let args = CliArgs::parse();

    if args.dynamic_config && args.config.is_some() {
        anyhow::bail!("--config cannot be used together with --dynamic-config, select one");
    }
    if !args.dynamic_config && args.config.is_none() {
        anyhow::bail!("--config is required unless --dynamic-config is set");
    }

    let bg_task_mgr = BackgroundTaskManager::new()?;

    bg_task_mgr.init_logging(&args.network)?;

    let config = if args.dynamic_config {
        bg_task_mgr.load_dynamic_config_blocking()?
    } else {
        Config::load_config(args.config.expect("validated above"))?
    };

    bg_task_mgr.init_metrics(&args.network);

    let proxy = RpcProxy::new(config.routes);
    if args.dynamic_config {
        bg_task_mgr.init_dynamic_config_refresher(proxy.routes_handle())?;
    }

    info!(
        "Starting omni-proxy v{} on {} port {}",
        env!("CARGO_PKG_VERSION"),
        args.network,
        args.port
    );

    let mut server = pingora::server::Server::new(None).unwrap();
    server.bootstrap();

    let mut svc = pingora::proxy::http_proxy_service(&server.configuration, proxy);
    svc.add_tcp(&format!("0.0.0.0:{}", args.port));
    server.add_service(svc);
    server.run_forever();
}
