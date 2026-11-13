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
    let mut continuous_config=setups_api::continuous::ContinuousRuntimeConfig::from_env().unwrap_or_else(|error|{eprintln!("{error}");std::process::exit(1)});
    config.resources.validate_continuous(&continuous_config).unwrap_or_else(|error|{eprintln!("runtime resource admission failed: {error}");std::process::exit(1)});
    continuous_config.replay_admission=config.replay_limits;
    // Continuous storage cannot consume the ordinary lane's final five-percent reserve.
    continuous_config.max_database_bytes=continuous_config.max_database_bytes.min(config.replay_limits.max_database_bytes.saturating_sub(config.replay_limits.max_database_bytes/20));
    if let Err(message) = setups_api::restore::apply_pending_restore(&config) {
        eprintln!("pending restore failed: {message}");
        std::process::exit(1);
    }
    let pool = match PgPoolOptions::new()
        .max_connections(config.resources.api_db_connections)
        .acquire_timeout(Duration::from_secs(5))
        .connect(&config.database_url)
        .await
    {
        Ok(value) => value,
        Err(error) => {
            eprintln!("database connection failed: {error}");
            std::process::exit(1);
        }
    };
    match setups_db::migrate::ensure_migrations(&pool).await {
        Ok(summary) => println!("migrations: {summary}"),
        Err(error) => {
            eprintln!("migrations failed: {error}");
            std::process::exit(1);
        }
    }
    let live_pool=connect_lane(&config.database_url,config.resources.live_db_connections,"live").await;
    let manual_pool=connect_lane(&config.database_url,config.resources.manual_db_connections,"manual").await;
    let continuous_pool=connect_lane(&config.database_url,config.resources.continuous_db_connections,"continuous").await;
    setups_db::continuous::outbox::configure_limits(&live_pool,config.resources.notification_max_jobs,config.resources.notification_max_bytes)
        .await.unwrap_or_else(|error|{eprintln!("notification budget configuration failed: {error}");std::process::exit(1)});
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
        manual_pool.clone(),
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
        live_pool,
        config.market_data_url.clone(),
        config.market_data_api_key.clone(),
    );
    let optimization_runtime = match setups_api::optimization::start_optimization_runtime(
        manual_pool,
        config.market_data_url.clone(),
        config.market_data_api_key.clone(),
        config.optimization.clone(),
    )
    .await
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("optimization runtime failed to start: {error}");
            std::process::exit(1);
        }
    };
    let optimization_state = Arc::new(setups_api::optimization::OptimizationState {
        pool: pool.clone(),
        auth: auth.clone(),
        config: config.optimization.clone(),
        notify: optimization_runtime.notify.clone(),
        available: optimization_runtime.available.clone(),
    });
    let continuous_runtime=setups_api::continuous::runtime::start_continuous_runtime(continuous_pool,config.market_data_url.clone(),config.market_data_api_key.clone(),continuous_config.clone()).await.unwrap_or_else(|error|{eprintln!("continuous runtime failed to start: {error}");std::process::exit(1)});
    let continuous_state=Arc::new(setups_api::continuous::ContinuousState{pool:pool.clone(),auth:auth.clone(),config:continuous_config,notify:continuous_runtime.notify.clone(),available:continuous_runtime.available.clone()});
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
    let backup_state = std::sync::Arc::new(setups_api::backup::BackupState {
        auth: auth.clone(),
        manager: setups_api::backup::BackupManager {
            scope: "platform".to_string(),
            max_upload_bytes: config.backup_max_upload_bytes,
            database_url: config.database_url.clone(),
            backup_dir: config.backup_dir.clone(),
            script_path: config.backup_script.clone(),
        },
        market_url: config.market_data_url.clone(),
        market_key: config.market_data_api_key.clone(),
        market_max_upload_bytes: config.market_backup_max_upload_bytes,
        http: reqwest::Client::new(),
    });
    let app = axum::Router::new()
        .merge(setups_api::session_router(auth))
        .merge(replay_router(replay_state.clone()))
        .merge(controls_router(controls_state))
        .merge(batches_router(replay_state))
        .merge(setups_api::analytics::analytics_router(analytics_state))
        .merge(setups_api::optimization::optimization_router(
            optimization_state,
        ))
        .merge(setups_api::continuous::continuous_router(continuous_state))
        .merge(setups_api::backup::backup_router(backup_state));
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
        .with_graceful_shutdown(shutdown_signal(jobs, live_runtime, optimization_runtime,continuous_runtime))
        .await
    {
        eprintln!("server error: {error}");
        std::process::exit(1);
    }
}

async fn connect_lane(url: &str, connections: u32, lane: &str) -> sqlx::PgPool {
    PgPoolOptions::new().max_connections(connections).acquire_timeout(Duration::from_secs(5)).connect(url).await
        .unwrap_or_else(|error|{eprintln!("{lane} database lane connection failed: {error}");std::process::exit(1)})
}

async fn shutdown_signal(
    jobs: setups_worker::JobsHandle,
    live: setups_worker::live::LiveHandle,
    optimization: setups_api::optimization::OptimizationHandle,
    continuous: setups_api::continuous::runtime::ContinuousHandle,
) {
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
    tokio::join!(live.stop(), jobs.stop(), optimization.stop(),continuous.stop());
}
