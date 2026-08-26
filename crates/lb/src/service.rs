//! Boots the LB: bind both listeners, serve until shutdown. Built to run
//! in-process — tests bind port 0 and read the real addresses back.

use std::net::SocketAddr;

use std::sync::Arc;

use tokio::{net::TcpListener, sync::watch, task::JoinHandle};

use crate::{admin, config::Config, forwarder::Forwarder, pool, proxy};

pub struct Service {
    pub public_addr: SocketAddr,
    pub admin_addr: SocketAddr,
    pub pool: Arc<pool::Pool>,
    shutdown: watch::Sender<bool>,
    servers: Vec<JoinHandle<()>>,
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
    let state = Arc::new(proxy::ProxyState {
        pool: pool.clone(),
        forwarder: Forwarder::new(&config.proxy),
        config: config.proxy.clone(),
        flip_after: config.health.flip_after,
    });

    let (shutdown, _) = watch::channel(false);
    let servers = vec![
        serve(public, proxy::router(state), shutdown.subscribe()),
        serve(admin, admin::router(), shutdown.subscribe()),
    ];

    Ok(Service {
        public_addr,
        admin_addr,
        pool,
        shutdown,
        servers,
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
    /// Stops both listeners and waits for them to finish.
    pub async fn shutdown(self) {
        let _ = self.shutdown.send(true);
        for server in self.servers {
            let _ = server.await;
        }
    }
}
