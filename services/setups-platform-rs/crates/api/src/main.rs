use serde_json::json;
use setups_api::auth::SessionAuth;
use setups_api::batches::batches_router;
use setups_api::config::PlatformConfig;
use setups_api::controls::{ControlsState, controls_router};
use setups_api::replay::{ReplayState, replay_router};
use setups_db::settings_set;
use sqlx::postgres::PgPoolOptions;
use std::sync::Arc;
use std::time::Duration;

const REPLAY_POLL: Duration = Duration::from_millis(500);

#[tokio::main]
async fn main() {
    let config = match PlatformConfig::from_env() {
        Ok(value) => value,
        Err(message) => {
            eprintln!("{message}");
            std::process::exit(1);
        }
    };
    let pool = match PgPoolOptions::new()
        .max_connections(10)
        .connect(&config.database_url)
        .await
    {
        Ok(value) => value,
        Err(error) => {
            eprintln!("database connection failed: {error}");
            std::process::exit(1);
        }
    };
    let live_status = json!({
        "running": false,
        "lastScanAt": serde_json::Value::Null,
        "lastError": serde_json::Value::Null,
    });
    if let Err(error) = settings_set(&pool, "live-status", &live_status).await {
        eprintln!("live status reset failed: {error}");
        std::process::exit(1);
    }
    let auth = Arc::new(SessionAuth::new(&config.api_keys));
    let replay_state = Arc::new(ReplayState {
        pool: pool.clone(),
        auth: auth.clone(),
        market_url: config.market_data_url.clone(),
        market_key: config.market_data_api_key.clone(),
        limits: config.replay_limits,
    });
    let controls_state = Arc::new(ControlsState {
        pool: pool.clone(),
        auth: auth.clone(),
        market_url: config.market_data_url.clone(),
        market_key: config.market_data_api_key.clone(),
    });
    let jobs = match setups_worker::start_replay_jobs(
        pool.clone(),
        config.market_data_url.clone(),
        config.market_data_api_key.clone(),
        config.replay_workers,
        REPLAY_POLL,
    )
    .await
    {
        Ok(value) => value,
        Err(error) => {
            eprintln!("replay runtime failed to start: {error}");
            std::process::exit(1);
        }
    };
    let live_runtime = setups_worker::live::start_live_runtime(
        pool.clone(),
        config.market_data_url.clone(),
        config.market_data_api_key.clone(),
    );
    let analytics_state = std::sync::Arc::new(setups_api::analytics::AnalyticsState {
        pool: pool.clone(),
        auth: auth.clone(),
        market_url: config.market_data_url.clone(),
        market_key: config.market_data_api_key.clone(),
        reader: setups_api::analytics::AnalyticsCandleReader::new(
            &config.market_data_url,
            &config.market_data_api_key,
        ),
    });
    let app = axum::Router::new()
        .merge(setups_api::session_router(auth))
        .merge(replay_router(replay_state.clone()))
        .merge(controls_router(controls_state))
        .merge(batches_router(replay_state))
        .merge(setups_api::analytics::analytics_router(analytics_state));
    let listener = match tokio::net::TcpListener::bind((config.host.as_str(), config.port)).await {
        Ok(value) => value,
        Err(error) => {
            eprintln!("failed to bind {}:{}: {error}", config.host, config.port);
            std::process::exit(1);
        }
    };
    println!(
        "signals platform listening at http://{}:{}",
        config.host, config.port
    );
    if let Err(error) = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal(jobs, live_runtime))
        .await
    {
        eprintln!("server error: {error}");
        std::process::exit(1);
    }
}

async fn shutdown_signal(jobs: setups_worker::JobsHandle, live: setups_worker::live::LiveHandle) {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut stream) => {
                stream.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    let _ = live.stop().await;
    let _ = jobs.stop().await;
}
