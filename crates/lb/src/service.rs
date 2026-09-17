//! Boots the LB: bind both listeners, serve until shutdown. Built to run
//! in-process — tests bind port 0 and read the real addresses back, and
//! hand in a fake chain where the binary hands in the real clients.

use std::{
    net::SocketAddr,
    sync::{Arc, atomic::AtomicBool},
    time::Duration,
};

use tokio::{net::TcpListener, sync::watch, task::JoinHandle};

use crate::{
    admin,
    chain::{ChainReader, ChainWriter, reader::Reader, writer::Writer},
    config::Config,
    forwarder::Forwarder,
    marketplace::agent::{self, Agent},
    monitor, pool, proxy,
};

/// The agent reads pages of records, heavier than a probe's one number.
const AGENT_READ_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug)]
pub struct Service {
    pub public_addr: SocketAddr,
    pub admin_addr: SocketAddr,
    pub pool: Arc<pool::Pool>,
    shutdown: watch::Sender<bool>,
    tasks: Vec<JoinHandle<()>>,
}

#[derive(Debug, thiserror::Error)]
pub enum StartError {
    #[error("cannot listen on {addr}")]
    Bind {
        addr: SocketAddr,
        #[source]
        source: std::io::Error,
    },
    #[error(transparent)]
    Provider(#[from] pool::InvalidUrl),
    #[error("{what} {url:?} does not parse")]
    Url {
        what: &'static str,
        url: String,
        #[source]
        source: url::ParseError,
    },
    #[error(
        "[marketplace] is configured but ARKIV_RPC_URL is not set: the agent reads offers and \
         its own records from the reference"
    )]
    MarketplaceNeedsReference,
    #[error("the marketplace agent could not start")]
    Marketplace(#[source] agent::StartError),
    #[error("health.chain_id is {configured}, but the sidecar writes to chain {actual}")]
    ChainMismatch { configured: u64, actual: u64 },
}

/// The service with the real chain clients, when the marketplace is
/// configured: the read client on the reference, the writer client on
/// the sidecar.
pub async fn start(config: Config) -> Result<Service, StartError> {
    let chain = match &config.marketplace {
        Some(marketplace) => {
            let reference_url = parse_url(
                "reference url",
                config
                    .reference
                    .as_deref()
                    .ok_or(StartError::MarketplaceNeedsReference)?,
            )?;
            let writer_url = parse_url("marketplace.writer_url", &marketplace.writer_url)?;
            // Its own client: the chain clients talk to the reference and the
            // sidecar, not to providers.
            let client = reqwest::Client::new();
            Some((
                Reader::new(
                    client.clone(),
                    reference_url,
                    config.reference_key.clone(),
                    AGENT_READ_TIMEOUT,
                ),
                Writer::new(client, writer_url),
            ))
        }
        None => None,
    };
    start_with(config, chain).await
}

fn parse_url(what: &'static str, url: &str) -> Result<reqwest::Url, StartError> {
    reqwest::Url::parse(url).map_err(|source| StartError::Url {
        what,
        url: url.to_owned(),
        source,
    })
}

/// The service over the given chain clients, which the marketplace
/// needs and a static-only configuration ignores. With the marketplace
/// configured, the chain is read before anything binds: a start that
/// cannot reach the reference or the sidecar touches no port.
pub async fn start_with<R: ChainReader + 'static, W: ChainWriter + 'static>(
    config: Config,
    chain: Option<(R, W)>,
) -> Result<Service, StartError> {
    let pool = Arc::new(pool::Pool::new(&config.providers)?);
    let agent = if let Some(marketplace) = &config.marketplace {
        let (reader, writer) = chain.expect("the marketplace needs chain clients");
        let agent = Agent::start(reader, writer, marketplace.clone(), pool.clone())
            .await
            .map_err(StartError::Marketplace)?;
        let identity = agent.identity();
        if let Some(configured) = config.health.chain_id
            && configured != identity.chain_id
        {
            return Err(StartError::ChainMismatch {
                configured,
                actual: identity.chain_id,
            });
        }
        tracing::info!(
            address = %identity.address,
            chain = identity.chain_id,
            agreements = agent.agreements().len(),
            "marketplace agent started"
        );
        Some(Arc::new(agent))
    } else {
        None
    };

    let bind = |addr: SocketAddr| async move {
        TcpListener::bind(addr)
            .await
            .map_err(|source| StartError::Bind { addr, source })
    };
    let public = bind(config.listen.public).await?;
    let admin = bind(config.listen.admin).await?;
    let public_addr = public.local_addr().map_err(|source| StartError::Bind {
        addr: config.listen.public,
        source,
    })?;
    let admin_addr = admin.local_addr().map_err(|source| StartError::Bind {
        addr: config.listen.admin,
        source,
    })?;

    // One client for everything outbound to providers — forwards and
    // probes go to the same hosts, so they share one connection pool.
    let client = reqwest::Client::new();
    // One forwarder for both listeners: same client, same caps.
    let forwarder = Forwarder::new(client.clone(), &config.proxy);
    let state = Arc::new(proxy::ProxyState {
        pool: pool.clone(),
        forwarder: forwarder.clone(),
        config: config.proxy.clone(),
        flip_after: config.health.flip_after,
    });

    let ready = Arc::new(AtomicBool::new(false));
    let (shutdown, _) = watch::channel(false);
    let mut tasks = vec![
        serve(public, proxy::router(state), shutdown.subscribe()),
        serve(
            admin,
            admin::router(pool.clone(), ready.clone(), forwarder, &config.proxy),
            shutdown.subscribe(),
        ),
    ];
    if let Some(agent) = agent {
        tasks.push(tokio::spawn(agent.run(shutdown.subscribe())));
    }
    if !config.health.disable_probing {
        let reference = match &config.reference {
            Some(url) => Some(Reader::new(
                client.clone(),
                parse_url("reference url", url)?,
                config.reference_key.clone(),
                config.health.probe_timeout,
            )),
            None => {
                tracing::warn!(
                    "no reference endpoint (ARKIV_RPC_URL): chain head lag goes unchecked"
                );
                None
            }
        };
        if config.health.chain_id.is_none() {
            tracing::warn!("health.chain_id is not set: chain identity goes unchecked");
        }
        let monitor = monitor::Monitor::new(
            pool.clone(),
            client,
            config.health.clone(),
            reference,
            ready,
        );
        tasks.push(tokio::spawn(monitor.run(shutdown.subscribe())));
    }

    Ok(Service {
        public_addr,
        admin_addr,
        pool,
        shutdown,
        tasks,
    })
}

/// Resolves when the process is asked to stop: Ctrl-C, or SIGTERM —
/// what `docker stop` sends.
pub async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(error) = tokio::signal::ctrl_c().await {
            tracing::error!(%error, "cannot wait for ctrl-c");
        }
    };
    let terminate = async {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(error) => tracing::error!(%error, "cannot wait for SIGTERM"),
        }
    };

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
}

fn serve(
    listener: TcpListener,
    router: axum::Router,
    mut shutdown: watch::Receiver<bool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let result = axum::serve(listener, router)
            .with_graceful_shutdown(async move {
                let _ = shutdown.changed().await;
            })
            .await;
        if let Err(error) = result {
            tracing::error!(%error, "listener exited with an error");
        }
    })
}

impl Service {
    /// Stops every task — listeners and Monitor — and waits them out.
    pub async fn shutdown(self) {
        let _ = self.shutdown.send(true);
        for task in self.tasks {
            let _ = task.await;
        }
    }
}
