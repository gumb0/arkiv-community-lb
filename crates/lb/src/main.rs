//! Thin binary over the `lb` library.

use std::{path::Path, process};

use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .compact()
        .init();

    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "config.toml".into());
    let mut config = match lb::config::Config::load(Path::new(&path)) {
        Ok(config) => config,
        Err(error) => fail(&error),
    };
    config.reference = std::env::var("ARKIV_RPC_URL")
        .ok()
        .filter(|url| !url.is_empty());
    let providers = config.providers.len();

    let service = match lb::service::start(config).await {
        Ok(service) => service,
        Err(error) => fail(&error),
    };
    tracing::info!(
        public = %service.public_addr,
        admin = %service.admin_addr,
        providers,
        "arkiv-lb started"
    );

    if let Err(error) = tokio::signal::ctrl_c().await {
        tracing::error!(%error, "cannot wait for ctrl-c");
    }
    tracing::info!("shutting down");
    service.shutdown().await;
}

fn fail(error: &dyn std::error::Error) -> ! {
    eprint!("arkiv-lb: {error}");
    let mut source = error.source();
    while let Some(cause) = source {
        eprint!(": {cause}");
        source = cause.source();
    }
    eprintln!();
    process::exit(1);
}
