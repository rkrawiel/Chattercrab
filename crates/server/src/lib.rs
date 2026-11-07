//! Chattercrab signaling server.
//!
//! Hosts rooms, relays SDP offers/answers and ICE candidates between members,
//! keeps a short in-memory chat history, and arbitrates the single screen-share slot per
//! room. It never relays media. Clients learn the configured STUN URL from `GET /ice-servers`.
//! When `CHATTERCRAB_PASSWORD` is set, every route except `GET /health` requires HTTP Basic auth.
//!
//! The binary `chattercrab-signal` is a thin wrapper around [`run`]; integration tests
//! call [`run`] directly with an ephemeral listener.

mod auth;
pub mod config;
pub mod http;
pub mod state;
pub mod tls;
pub mod ws;

use std::net::SocketAddr;
use std::sync::Arc;

use axum_server::Handle;
use axum_server::tls_rustls::RustlsConfig;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

pub use config::Config;
use state::AppState;

/// External control signals for a running server.
#[derive(Debug, Clone, Default)]
pub struct ServerSignals {
    /// Cancel to shut down gracefully (SIGTERM).
    pub shutdown: CancellationToken,
    /// Notify to reload the TLS certificate and key (SIGHUP).
    pub reload: Arc<Notify>,
}

/// Fatal server errors.
#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    /// Socket error.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// TLS configuration error at startup.
    #[error(transparent)]
    Tls(#[from] tls::TlsError),
}

/// Runs the server on `listener` until `signals.shutdown` is cancelled.
///
/// After cancellation the server stops accepting connections, every WebSocket gets a
/// `1001` close frame, and the function returns within `config.shutdown_grace`.
pub async fn run(
    config: Config,
    listener: std::net::TcpListener,
    signals: ServerSignals,
) -> Result<(), ServerError> {
    listener.set_nonblocking(true)?;
    let local_addr = listener.local_addr()?;
    let tls = config.tls.clone();
    let grace = config.shutdown_grace;
    tracing::info!(stun_url = %config.stun_url, "STUN server");
    let state = Arc::new(AppState::new(config, signals.shutdown.clone()));
    let app = http::router(Arc::clone(&state)).into_make_service_with_connect_info::<SocketAddr>();

    tokio::spawn(sweeper(Arc::clone(&state)));

    let handle: Handle<SocketAddr> = Handle::new();
    {
        let handle = handle.clone();
        let shutdown = signals.shutdown.clone();
        tokio::spawn(async move {
            shutdown.cancelled().await;
            tracing::info!("shutdown requested");
            handle.graceful_shutdown(Some(grace));
        });
    }

    let serve = async {
        match &tls {
            Some(paths) => {
                let server_config = tls::load_server_config(&paths.cert, &paths.key)?;
                let rustls = RustlsConfig::from_config(server_config);
                tokio::spawn(reload_tls(
                    rustls.clone(),
                    paths.clone(),
                    Arc::clone(&signals.reload),
                    signals.shutdown.clone(),
                ));
                tracing::info!(addr = %local_addr, "listening (https/wss)");
                axum_server::from_tcp_rustls(listener, rustls)?
                    .handle(handle)
                    .serve(app)
                    .await?;
            }
            None => {
                tracing::warn!(addr = %local_addr, "listening (plain http/ws, no TLS configured)");
                axum_server::from_tcp(listener)?
                    .handle(handle)
                    .serve(app)
                    .await?;
            }
        }
        Ok::<(), ServerError>(())
    };

    // Hard deadline: `grace` after shutdown was requested.
    let deadline = async {
        signals.shutdown.cancelled().await;
        tokio::time::sleep(grace).await;
    };
    tokio::pin!(deadline);

    let result = tokio::select! {
        res = serve => res,
        () = &mut deadline => {
            tracing::warn!("graceful shutdown deadline reached");
            Ok(())
        }
    };
    // Make sure every connection task sees the shutdown (also on serve errors).
    signals.shutdown.cancel();
    state.tracker.close();
    tokio::select! {
        () = state.tracker.wait() => {}
        () = &mut deadline => tracing::warn!("websocket tasks still running at deadline"),
    }
    tracing::info!("server stopped");
    result
}

/// Periodically evicts idle empty rooms.
async fn sweeper(state: Arc<AppState>) {
    let mut tick = tokio::time::interval(state.config.sweep_interval);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            () = state.shutdown.cancelled() => return,
            _ = tick.tick() => {
                let evicted = state.sweep().await;
                if evicted > 0 {
                    tracing::info!(evicted, rooms = state.room_count(), "evicted idle rooms");
                }
            }
        }
    }
}

/// Reloads the certificate on every `reload` notification. A failed reload keeps the
/// previous configuration.
async fn reload_tls(
    rustls: RustlsConfig,
    paths: config::TlsPaths,
    reload: Arc<Notify>,
    shutdown: CancellationToken,
) {
    loop {
        tokio::select! {
            () = shutdown.cancelled() => return,
            () = reload.notified() => {}
        }
        let (cert, key) = (paths.cert.clone(), paths.key.clone());
        let loaded =
            tokio::task::spawn_blocking(move || tls::load_server_config(&cert, &key)).await;
        match loaded {
            Ok(Ok(config)) => {
                rustls.reload_from_config(config);
                tracing::info!("TLS certificate reloaded");
            }
            Ok(Err(err)) => {
                tracing::error!(%err, "TLS reload failed, keeping previous certificate")
            }
            Err(err) => tracing::error!(%err, "TLS reload task failed"),
        }
    }
}
