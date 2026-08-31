//! Boots the LB: bind both listeners, serve until shutdown. Built to run
//! in-process — tests bind port 0 and read the real addresses back.

use std::net::SocketAddr;

use std::sync::{Arc, atomic::AtomicBool};

use tokio::{net::TcpListener, sync::watch, task::JoinHandle};

use crate::{admin, config::Config, forwarder::Forwarder, monitor, pool, proxy};

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
    #[error("reference url {url:?} does not parse")]
    Reference {
        url: String,
        #[source]
        source: url::ParseError,
    },
}

pub async fn start(config: Config) -> Result<Service, StartError> {
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

    let pool = Arc::new(pool::Pool::new(&config.providers)?);
    // One client for everything outbound — forwards and probes go to the
    // same providers, so they share one connection pool.
    let client = reqwest::Client::new();
    let state = Arc::new(proxy::ProxyState {
        pool: pool.clone(),
        forwarder: Forwarder::new(client.clone(), &config.proxy),
        config: config.proxy.clone(),
        flip_after: config.health.flip_after,
    });

    let ready = Arc::new(AtomicBool::new(false));
    let (shutdown, _) = watch::channel(false);
    let mut tasks = vec![
        serve(public, proxy::router(state), shutdown.subscribe()),
        serve(admin, admin::router(ready.clone()), shutdown.subscribe()),
    ];
    if !config.health.disable_probing {
        let reference = match &config.reference {
            Some(url) => {
                Some(
                    reqwest::Url::parse(url).map_err(|source| StartError::Reference {
                        url: url.clone(),
                        source,
                    })?,
                )
            }
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
