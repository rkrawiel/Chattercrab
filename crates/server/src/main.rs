//! `chattercrab-signal`: the Chattercrab signaling server binary.
//!
//! Configuration comes from `CHATTERCRAB_*` environment variables (see
//! `.env.example` and `crates/server/README.md`). Logs are JSON lines on stdout.
//! `SIGHUP` reloads the TLS certificate; `SIGTERM`/`SIGINT` shut down gracefully.

use anyhow::Context;
use chattercrab_server::{Config, ServerSignals, run};
use tracing_subscriber::EnvFilter;

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .json()
        .flatten_event(true)
        .with_current_span(false)
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let config = Config::from_env().context("invalid configuration")?;
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        bind = %config.bind,
        tls = config.tls.is_some(),
        stun_url = %config.stun_url,
        auth = config.auth.is_some(),
        max_room_size = config.max_room_size,
        max_rooms = config.max_rooms,
        "starting chattercrab-signal"
    );

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("cannot start tokio runtime")?;
    runtime.block_on(async move {
        let listener = std::net::TcpListener::bind(config.bind)
            .with_context(|| format!("cannot bind {}", config.bind))?;
        let signals = ServerSignals::default();
        spawn_signal_handlers(signals.clone())?;
        run(config, listener, signals).await?;
        Ok(())
    })
}

#[cfg(unix)]
fn spawn_signal_handlers(signals: ServerSignals) -> anyhow::Result<()> {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate()).context("SIGTERM handler")?;
    let mut int = signal(SignalKind::interrupt()).context("SIGINT handler")?;
    let mut hup = signal(SignalKind::hangup()).context("SIGHUP handler")?;
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = term.recv() => { signals.shutdown.cancel(); return; }
                _ = int.recv() => { signals.shutdown.cancel(); return; }
                _ = hup.recv() => {
                    tracing::info!("SIGHUP received, reloading TLS certificate");
                    signals.reload.notify_one();
                }
            }
        }
    });
    Ok(())
}

#[cfg(not(unix))]
fn spawn_signal_handlers(signals: ServerSignals) -> anyhow::Result<()> {
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            signals.shutdown.cancel();
        }
    });
    Ok(())
}
