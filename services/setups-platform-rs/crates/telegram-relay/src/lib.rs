//! Independent text-only Telegram relay. No platform/database worker coupling.
pub mod config;
mod http;
mod store;
mod telegram;

use std::{
    fmt,
    sync::{Arc, atomic::Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelayError {
    Config(&'static str),
    Storage,
    StaleLease,
    Listen,
    Server,
}
impl RelayError {
    pub fn code(self) -> &'static str {
        match self {
            Self::Config(code) => code,
            Self::Storage => "storage_unavailable",
            Self::StaleLease => "stale_delivery_lease",
            Self::Listen => "listen_failed",
            Self::Server => "server_failed",
        }
    }
}
impl fmt::Display for RelayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code())
    }
}
impl std::error::Error for RelayError {}
pub fn unix_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_secs()).ok())
        .unwrap_or(-1)
}

pub async fn run(config: config::Config) -> Result<(), RelayError> {
    let store = store::Store::open(
        &config.state_path,
        config.bot_id,
        config.chat_id,
        config.limits,
    )?;
    store.cleanup(unix_seconds()).await?;
    let telegram = telegram::Telegram::new(config.token, config.bot_id, config.chat_id)?;
    let app = http::App::new(
        config.key,
        store.clone(),
        telegram,
        config.concurrency,
        Arc::new(unix_seconds),
    );
    let listener = tokio::net::TcpListener::bind(config.listen)
        .await
        .map_err(|_| RelayError::Listen)?;
    let (stop, mut stopped) = tokio::sync::watch::channel(false);
    let cleanup = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(30));
        loop {
            tokio::select! {
                _=interval.tick()=>{if store.cleanup(unix_seconds()).await.is_err(){eprintln!("relay_cleanup code=storage_unavailable");}},
                _=stopped.changed()=>break,
            }
        }
    });
    let shutdown = app.clone();
    eprintln!("relay_lifecycle state=listening");
    let result = axum::serve(listener, app.router())
        .with_graceful_shutdown(async move {
            shutdown_signal().await;
            shutdown.stopping.store(true, Ordering::Release);
            let _ = stop.send(true);
            eprintln!("relay_lifecycle state=stopping");
        })
        .await
        .map_err(|_| RelayError::Server);
    cleanup.abort();
    let _ = cleanup.await;
    eprintln!("relay_lifecycle state=stopped");
    result
}
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {_=ctrl_c=>{},_=terminate=>{}}
}
