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

struct WorkerTelegramTransport;
impl setups_api::telegram::TelegramTransport for WorkerTelegramTransport {
    fn secret_configured(&self)->bool {setups_worker::telegram::key_presence()}
    fn probe<'a>(&'a self,config:&'a setups_db::telegram::TelegramConfig)->futures::future::BoxFuture<'a,setups_db::telegram::TelegramStatusUpdate> {
        Box::pin(setups_worker::telegram::probe_telegram_relay(config))
    }
    fn test<'a>(&'a self,attempt:&'a setups_db::telegram::TelegramTestAttempt)->futures::future::BoxFuture<'a,setups_db::telegram::TelegramTestResult> {
        Box::pin(setups_worker::telegram::test_telegram_relay(attempt))
    }
}
fn with_telegram_router(app:axum::Router,pool:sqlx::PgPool,auth:Arc<SessionAuth>)->axum::Router {
    app.merge(setups_api::telegram::telegram_router(Arc::new(setups_api::telegram::TelegramState {
        pool,auth,transport:Arc::new(WorkerTelegramTransport),
    })))
}

#[tokio::main]
async fn main() {
    let args=std::env::args().skip(1).collect::<Vec<_>>();
    let materialize=if args.first().is_some_and(|arg|arg=="materialize-study-original") {
        if args.len()==2 && args[1]=="--help" {println!("{}",setups_api::study_actual::legacy_cli::USAGE);return;}
        Some(setups_api::study_actual::legacy_cli::parse(&args[1..]).unwrap_or_else(|error|{
            eprintln!("{error}\n{}",setups_api::study_actual::legacy_cli::USAGE);std::process::exit(2)
        }))
    }else{None};
    let config = match PlatformConfig::from_env() {
        Ok(value) => value,
        Err(message) => {
            eprintln!("{message}");
            std::process::exit(1);
        }
    };
    if let Some(options)=materialize {
        // Explicit one-shot operator mode: no restore, migration, status reset,
        // server listener, live/research/replay workers, or adaptive publication.
        let pool=PgPoolOptions::new().max_connections(2).acquire_timeout(Duration::from_secs(5))
            .connect(&config.database_url).await.unwrap_or_else(|_|{eprintln!("materialization database connection unavailable");std::process::exit(1)});
        let client=setups_mdclient::MarketDataClient::new(&config.market_data_url,&config.market_data_api_key);
        let result=setups_api::study_actual::legacy_cli::run(&pool,&client,&options).await;
        pool.close().await;
        match result {
            Ok(summary)=>println!("{summary}"),
            Err(error)=>{let message:String=error.chars().take(512).map(|c|if c.is_control(){' '}else{c}).collect();eprintln!("materialization failed: {message}");std::process::exit(1);},
        }
        return;
    }
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
    let (live_data_connections,live_control_connections)=config.resources.live_pool_connections();
    let live_data_pool=connect_lane(&config.database_url,live_data_connections,"live-data").await;
    let live_control_pool=connect_lane(&config.database_url,live_control_connections,"live-control").await;
    setups_api::continuous::configure_profile_limits_from_env(&pool).await
        .unwrap_or_else(|error|{eprintln!("profile limit configuration failed: {error}");std::process::exit(1)});
    let manual_pool=connect_lane(&config.database_url,config.resources.manual_db_connections,"manual").await;
    let continuous_pool=connect_lane(&config.database_url,config.resources.continuous_db_connections,"continuous").await;
    setups_db::continuous::outbox::configure_limits(&live_data_pool,config.resources.notification_max_jobs,config.resources.notification_max_bytes)
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
        live_data_pool,
        live_control_pool,
        config.market_data_url.clone(),
        config.market_data_api_key.clone(),
    );
    let optimization_runtime = match setups_api::optimization::start_optimization_runtime(
        manual_pool.clone(),
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
    let actual_runtime=setups_api::study_actual::start_actual_runtime(Arc::new(ReplayState {
        pool:manual_pool,auth:auth.clone(),market_url:config.market_data_url.clone(),market_key:config.market_data_api_key.clone(),limits:config.replay_limits,
    }),config.replay_workers).await.unwrap_or_else(|error|{eprintln!("study actual runtime failed: {error}");std::process::exit(1)});
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
        forecast_cpu: Arc::new(tokio::sync::Semaphore::new(1)),
        outcome_compute: Arc::new(tokio::sync::Semaphore::new(1)),
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
        .merge(setups_api::session_router(auth.clone()))
        .merge(replay_router(replay_state.clone()))
        .merge(setups_api::study_actual::study_actual_router(replay_state.clone()))
        .merge(controls_router(controls_state))
        .merge(batches_router(replay_state))
        .merge(setups_api::analytics::analytics_router(analytics_state.clone()))
        .merge(setups_api::setups_analytics::setups_analytics_router(analytics_state))
        .merge(setups_api::optimization::optimization_router(
            optimization_state,
        ))
        .merge(setups_api::continuous::continuous_router(continuous_state))
        .merge(setups_api::backup::backup_router(backup_state));
    let app=with_telegram_router(app,pool.clone(),auth);
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
        .with_graceful_shutdown(shutdown_signal(jobs, live_runtime, optimization_runtime,continuous_runtime,actual_runtime))
        .await
    {
        eprintln!("server error: {error}");
        std::process::exit(1);
    }
}

async fn connect_lane(url: &str, connections: u32, lane: &str) -> sqlx::PgPool {
    let options=url.parse::<sqlx::postgres::PgConnectOptions>().unwrap_or_else(|_|{eprintln!("{lane} database lane URL is invalid");std::process::exit(1)});
    let options=if matches!(lane,"live-data"|"live-control") {options.application_name(&format!("setups-{lane}"))}else{options};
    // End blocked SQL on the server before promotion's 1s attempt/readback
    // guards; dropping a client future alone can retain the sole connection.
    let options=if lane=="live-control" {
        options.options([("lock_timeout","500ms"),("statement_timeout","750ms")])
    }else{options};
    PgPoolOptions::new().max_connections(connections).acquire_timeout(Duration::from_secs(5)).connect_with(options).await
        .unwrap_or_else(|error|{eprintln!("{lane} database lane connection failed: {error}");std::process::exit(1)})
}

async fn shutdown_signal(
    jobs: setups_worker::JobsHandle,
    live: setups_worker::live::LiveHandle,
    optimization: setups_api::optimization::OptimizationHandle,
    continuous: setups_api::continuous::runtime::ContinuousHandle,
    actual: setups_api::study_actual::ActualHandle,
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
    tokio::join!(live.stop(), jobs.stop(), optimization.stop(),continuous.stop(),actual.stop());
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body,http::{Request,StatusCode}};
    use tower::ServiceExt;
    #[tokio::test]
    async fn production_router_mounts_telegram_and_preserves_existing_session_guard() {
        let pool=PgPoolOptions::new().connect_lazy("postgres://unused:unused@127.0.0.1:1/unused_test").unwrap();
        let auth=Arc::new(SessionAuth::new(&["test-key".into()]));
        let app=with_telegram_router(setups_api::session_router(auth.clone()),pool,auth);
        for (method,path) in [("GET","/v1/session"),("GET","/v1/telegram-relay"),("PUT","/v1/telegram-relay"),("POST","/v1/telegram-relay/probe"),("POST","/v1/telegram-relay/test")] {
            let response=app.clone().oneshot(Request::builder().method(method).uri(path).header("content-type","application/json").body(Body::from("{}")).unwrap()).await.unwrap();
            assert_eq!(response.status(),StatusCode::UNAUTHORIZED,"production route {method} {path} must be mounted and guarded before DB/network");
        }
        assert_eq!(app.oneshot(Request::builder().uri("/not-a-route").body(Body::empty()).unwrap()).await.unwrap().status(),StatusCode::NOT_FOUND);
    }
}
