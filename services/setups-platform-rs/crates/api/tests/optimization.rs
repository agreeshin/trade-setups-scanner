use axum::{
    Json, Router,
    body::Body,
    extract::{Query, State},
    http::{Request, StatusCode},
    routing::get,
};
use http_body_util::BodyExt;
use serde::Deserialize;
use serde_json::{Value, json};
use setups_api::{
    SessionAuth,
    config::OptimizationConfig,
    optimization::{OptimizationState, optimization_router, start_optimization_runtime},
};
use setups_db::optimization::{self as db, Admission};
use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions};
use std::{
    str::FromStr,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tower::ServiceExt;

fn spec() -> Value {
    json!({"algorithmId":"price_volume_spike","algorithmVersion":"1.1.1",
        "algorithmParams":{"priceGrowthPct":"1","windowMinutes":1,"baselineLongDays":1,"baselineShortDays":1,"volumeMultiplier":"0.1","cooldownMinutes":0,"maxGapPercent":"1","maxConsecutiveGapMinutes":5,"requiredRecentMinutes":1},
        "algorithmSearch":{},"analyticsParams":{"clusterEnabled":false,"clusterGapMinutes":480,"timeShiftMinutes":0,"priceChangePct":null},"analyticsSearch":{},
        "instruments":[{"exchange":"bybit","category":"linear","symbol":"BTCUSDT"}],
        "fromUnix":172800,"toUnix":173400,"direction":"long","tpLevels":[4],"slLevels":[4],"horizonMinutes":60,"maxTrials":1,"maxSeconds":30,"seed":42})
}

fn app(pool: PgPool, config: OptimizationConfig) -> Router {
    app_with_health(
        pool,
        config,
        Arc::new(std::sync::atomic::AtomicBool::new(true)),
    )
}
fn app_with_health(
    pool: PgPool,
    config: OptimizationConfig,
    available: Arc<std::sync::atomic::AtomicBool>,
) -> Router {
    optimization_router(Arc::new(OptimizationState {
        pool,
        config,
        auth: Arc::new(SessionAuth::new(&["test-key".into()])),
        notify: Arc::new(tokio::sync::Notify::new()),
        available,
    }))
}

async fn request(app: &Router, method: &str, path: &str, body: Value) -> (StatusCode, Value) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header("x-api-key", "test-key")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    assert_eq!(response.headers()["cache-control"], "no-store");
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap())
}

#[tokio::test]
async fn routes_validate_without_database_access() {
    let pool = PgPoolOptions::new()
        .connect_lazy("postgres://unused:unused@127.0.0.1:1/unused_test")
        .unwrap();
    let app = app(pool, OptimizationConfig::default());
    for (field, value) in [
        ("algorithmId", json!("missing")),
        ("fromUnix", json!(172801)),
        ("maxTrials", json!(4097)),
        ("maxSeconds", json!(0)),
        ("setupMaturity", json!(0.15)),
        ("tpLevels", json!([4, 4])),
        ("algorithmSearch", json!({"priceDropPct":["4"]})),
        ("analyticsSearch", json!({"setupMaturity":[0.5]})),
    ] {
        let mut body = spec();
        body[field] = value;
        assert_eq!(
            request(&app, "POST", "/v1/optimization-studies", body)
                .await
                .0,
            StatusCode::BAD_REQUEST,
            "{field}"
        );
    }
    let mut body = spec();
    body["algorithmVersion"] = json!("old");
    assert_eq!(
        request(&app, "POST", "/v1/optimization-studies", body)
            .await
            .0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        request(
            &app,
            "GET",
            "/v1/optimization-studies?limit=101",
            Value::Null
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        request(&app, "GET", "/v1/optimization-studies/0", Value::Null)
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn all_routes_require_auth_and_cookie_mutations_require_origin_guard() {
    let pool = PgPoolOptions::new()
        .connect_lazy("postgres://unused:unused@127.0.0.1:1/unused_test")
        .unwrap();
    let auth = Arc::new(SessionAuth::new(&["test-key".into()]));
    let token = match auth.post_session(Some("test-key"), setups_api::now_ms()) {
        setups_api::auth::PostSessionResult::Issued { token, .. } => token,
        _ => panic!("session"),
    };
    let app = optimization_router(Arc::new(OptimizationState {
        pool,
        auth,
        config: OptimizationConfig::default(),
        notify: Arc::new(tokio::sync::Notify::new()),
        available: Arc::new(std::sync::atomic::AtomicBool::new(true)),
    }));
    for (method, path) in [
        ("POST", "/v1/optimization-studies"),
        ("GET", "/v1/optimization-studies"),
        ("GET", "/v1/optimization-studies/1"),
        ("POST", "/v1/optimization-studies/1/cancel"),
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(path)
                    .body(Body::from(spec().to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        if method == "POST" {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(path)
                        .header("cookie", format!("platform_session={token}"))
                        .body(Body::from(spec().to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::FORBIDDEN);
        }
    }
}

struct TestDb {
    admin: PgPool,
    pool: PgPool,
    name: String,
}
impl TestDb {
    async fn new() -> Self {
        let options = PgConnectOptions::from_str(
            &std::env::var("TEST_DATABASE_URL").expect("TEST_DATABASE_URL required"),
        )
        .unwrap();
        assert!(options.get_database().unwrap().ends_with("_test"));
        let admin = PgPoolOptions::new()
            .max_connections(2)
            .connect_with(options.clone())
            .await
            .unwrap();
        let name = format!(
            "optimization_test_{}_{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap()
        );
        sqlx::query(&format!("create database {name}"))
            .execute(&admin)
            .await
            .unwrap();
        let pool = PgPoolOptions::new()
            .max_connections(6)
            .connect_with(options.database(&name))
            .await
            .unwrap();
        let baseline = include_str!("../../db/migrations/0010_baseline.sql")
            .lines()
            .filter(|line| {
                !line.starts_with('\\')
                    && !line.starts_with("SET transaction_timeout")
                    && !line.contains("set_config('search_path'")
            })
            .collect::<Vec<_>>()
            .join("\n");
        sqlx::raw_sql(&baseline).execute(&pool).await.unwrap();
        sqlx::raw_sql("set search_path to public; create table kysely_migration (name varchar(255) primary key, timestamp varchar(255) not null); insert into kysely_migration values ('0009_legacy','legacy')").execute(&pool).await.unwrap();
        setups_db::migrate::ensure_migrations(&pool).await.unwrap();
        Self { admin, pool, name }
    }
    async fn cleanup(self) {
        self.pool.close().await;
        sqlx::query(&format!("drop database {} with (force)", self.name))
            .execute(&self.admin)
            .await
            .unwrap();
        self.admin.close().await;
    }
}

async fn wait_for(
    pool: &PgPool,
    id: i64,
    predicate: impl Fn(&db::StudyRecord) -> bool,
) -> db::StudyRecord {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let row = db::get(pool, id).await.unwrap().unwrap();
            if predicate(&row) {
                return row;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("study transition timeout")
}

#[derive(Clone)]
struct Market {
    delay: Duration,
    requests: Arc<AtomicUsize>,
    mode: u8,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CandleQuery {
    from_unix: i64,
    to_unix: i64,
    after_unix: Option<i64>,
    limit: usize,
}
async fn candles(
    State(market): State<Market>,
    Query(query): Query<CandleQuery>,
) -> (StatusCode, Json<Value>) {
    market.requests.fetch_add(1, Ordering::SeqCst);
    tokio::time::sleep(market.delay).await;
    if market.mode == 1 {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error":"unavailable"})),
        );
    }
    let start = query.after_unix.map_or(query.from_unix, |after| after + 60);
    let mut candles = Vec::new();
    for time in (start..query.to_unix).step_by(60).take(query.limit) {
        let close = if time < 172800 { 100 } else { 110 };
        candles.push(json!({"instrumentId":if market.mode==3 {43}else{42},"openTimeUnix":time,"open":close.to_string(),"high":(close+5).to_string(),"low":(close-5).to_string(),"close":close.to_string(),"volume":"100","quoteTurnover":"1000"}));
    }
    let last = candles.last().and_then(|c| c["openTimeUnix"].as_i64());
    let next = if last.is_some_and(|last| last + 60 < query.to_unix) {
        last
    } else {
        None
    };
    if market.mode == 2 {
        return (StatusCode::OK, Json(json!({"candles":candles})));
    }
    if market.mode == 4 {
        return (
            StatusCode::OK,
            Json(
                json!({"candles":candles,"nextCursorUnix":query.after_unix.or(Some(query.from_unix))}),
            ),
        );
    }
    if market.mode == 5 && query.after_unix.is_some() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error":"mid_page_failure"})),
        );
    }
    (
        StatusCode::OK,
        Json(json!({"candles":candles,"nextCursorUnix":next})),
    )
}
async fn market(
    delay: Duration,
    mode: u8,
) -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
    let requests = Arc::new(AtomicUsize::new(0));
    let state = Market {
        delay,
        requests: requests.clone(),
        mode,
    };
    let app=Router::new().route("/v1/instruments",get(||async {Json(json!({"instruments":[{"id":42,"exchange":"bybit","category":"linear","symbol":"BTCUSDT","status":"delisted"}]}))})).route("/v1/candles",get(candles)).with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (url, requests, task)
}

#[tokio::test]
#[ignore = "requires isolated TEST_DATABASE_URL ending in _test with CREATEDB"]
async fn detached_execution_reload_discovery_and_cache_isolation() {
    let db = TestDb::new().await;
    let (url, _, server) = market(Duration::from_millis(20), 0).await;
    let runtime = start_optimization_runtime(
        db.pool.clone(),
        url,
        "test-market-key".into(),
        OptimizationConfig::default(),
    )
    .await
    .unwrap();
    let originating_router = app(db.pool.clone(), OptimizationConfig::default());
    let (status, created) = request(
        &originating_router,
        "POST",
        "/v1/optimization-studies",
        spec(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert!(created["result"].is_null());
    assert!(created["progress"].get("bestScore").is_some());
    let id = created["id"].as_i64().unwrap();
    drop(originating_router); // The originating request and router are gone; the server owns work.
    let final_row = wait_for(&db.pool, id, |r| !r.active()).await;
    assert_eq!(final_row.status, "completed", "{:?}", final_row.error);
    assert!(final_row.result.as_ref().unwrap()["best"].is_object());
    let reloaded = app(db.pool.clone(), OptimizationConfig::default());
    let (_, list) = request(
        &reloaded,
        "GET",
        "/v1/optimization-studies?algorithmId=price_volume_spike",
        Value::Null,
    )
    .await;
    assert_eq!(list["studies"][0]["id"], id);
    let (_, detail) = request(
        &reloaded,
        "GET",
        &format!("/v1/optimization-studies/{id}"),
        Value::Null,
    )
    .await;
    assert_eq!(detail["result"], final_row.result.unwrap());
    let tables = [
        "replay_runs",
        "replay_batches",
        "replay_cache",
        "analytics_selection_cache",
        "analytics_matrix_cache",
        "analytics_trades_cache",
        "settings",
    ];
    for table in tables {
        let exists: bool = sqlx::query_scalar("select to_regclass($1) is not null")
            .bind(table)
            .fetch_one(&db.pool)
            .await
            .unwrap();
        if exists {
            let count: i64 = sqlx::query_scalar(&format!("select count(*) from {table}"))
                .fetch_one(&db.pool)
                .await
                .unwrap();
            assert_eq!(count, 0, "{table}");
        }
    }
    runtime.stop().await;
    server.abort();
    db.cleanup().await;
}

#[tokio::test]
#[ignore = "requires isolated TEST_DATABASE_URL ending in _test with CREATEDB"]
async fn acquisition_cancel_keeps_active_slot_and_is_idempotent() {
    let db = TestDb::new().await;
    let (url, requests, server) = market(Duration::from_secs(10), 0).await;
    let runtime = start_optimization_runtime(
        db.pool.clone(),
        url,
        "key".into(),
        OptimizationConfig::default(),
    )
    .await
    .unwrap();
    let router = app(db.pool.clone(), OptimizationConfig::default());
    let (_, created) = request(&router, "POST", "/v1/optimization-studies", spec()).await;
    let id = created["id"].as_i64().unwrap();
    wait_for(&db.pool, id, |r| r.status == "running").await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while requests.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    for _ in 0..3 {
        let (status, busy) = request(&router, "POST", "/v1/optimization-studies", spec()).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(busy["activeStudyId"], id);
    }
    assert_eq!(
        request(
            &router,
            "POST",
            &format!("/v1/optimization-studies/{id}/cancel"),
            Value::Null
        )
        .await
        .0,
        StatusCode::ACCEPTED
    );
    let final_row = wait_for(&db.pool, id, |r| !r.active()).await;
    assert_eq!(final_row.status, "cancelled");
    assert_eq!(final_row.result.unwrap()["reason"], "cancelled");
    assert_eq!(
        request(
            &router,
            "POST",
            &format!("/v1/optimization-studies/{id}/cancel"),
            Value::Null
        )
        .await
        .0,
        StatusCode::OK
    );
    let count: i64 = sqlx::query_scalar("select count(*) from optimization_studies")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
    runtime.stop().await;
    server.abort();
    db.cleanup().await;
}

#[tokio::test]
#[ignore = "requires isolated TEST_DATABASE_URL ending in _test with CREATEDB"]
async fn acquisition_timeout_and_network_failure_are_distinct() {
    let db = TestDb::new().await;
    let (url, _, server) = market(Duration::from_secs(5), 0).await;
    let runtime = start_optimization_runtime(
        db.pool.clone(),
        url,
        "key".into(),
        OptimizationConfig::default(),
    )
    .await
    .unwrap();
    let router = app(db.pool.clone(), OptimizationConfig::default());
    let mut body = spec();
    body["maxSeconds"] = json!(1);
    let (_, created) = request(&router, "POST", "/v1/optimization-studies", body).await;
    let row = wait_for(&db.pool, created["id"].as_i64().unwrap(), |r| !r.active()).await;
    assert_eq!(row.status, "completed");
    assert_eq!(row.result.unwrap()["reason"], "time_budget");
    assert!(row.error.is_none());
    runtime.stop().await;
    server.abort();
    let (url, _, server) = market(Duration::ZERO, 1).await;
    let runtime = start_optimization_runtime(
        db.pool.clone(),
        url,
        "key".into(),
        OptimizationConfig::default(),
    )
    .await
    .unwrap();
    let (_, created) = request(&router, "POST", "/v1/optimization-studies", spec()).await;
    let row = wait_for(&db.pool, created["id"].as_i64().unwrap(), |r| !r.active()).await;
    assert_eq!(row.status, "failed");
    assert_eq!(row.error.unwrap()["code"], "market_data_unavailable");
    assert!(row.result.is_none());
    runtime.stop().await;
    server.abort();
    db.cleanup().await;
}

#[tokio::test]
#[ignore = "requires isolated TEST_DATABASE_URL ending in _test with CREATEDB"]
async fn admission_race_restart_recovery_and_late_cancel_finalize() {
    let db = TestDb::new().await;
    let progress = json!({"phase":"acquiring","evaluatedCandidates":0,"scorableCandidates":0,"maxTrials":1,"elapsedSeconds":0,"bestScore":null});
    let body = spec();
    let (a, b) = tokio::join!(
        db::create(&db.pool, "price_volume_spike", &body, &progress),
        db::create(&db.pool, "price_volume_spike", &body, &progress)
    );
    let (id, busy) = match (a.unwrap(), b.unwrap()) {
        (Admission::Created(row), Admission::Busy(id))
        | (Admission::Busy(id), Admission::Created(row)) => (row.id, id),
        _ => panic!("exactly one admission"),
    };
    assert_eq!(id, busy);
    db::claim(&db.pool).await.unwrap().unwrap();
    let runtime = start_optimization_runtime(
        db.pool.clone(),
        "http://127.0.0.1:1".into(),
        "key".into(),
        OptimizationConfig::default(),
    )
    .await
    .unwrap();
    let row = db::get(&db.pool, id).await.unwrap().unwrap();
    assert_eq!(row.status, "failed");
    assert_eq!(row.error.unwrap()["code"], "interrupted_by_restart");
    assert!(
        start_optimization_runtime(
            db.pool.clone(),
            "http://127.0.0.1:1".into(),
            "key".into(),
            OptimizationConfig::default()
        )
        .await
        .is_err()
    );
    runtime.stop().await;
    let row = match db::create(&db.pool, "price_volume_spike", &body, &progress)
        .await
        .unwrap()
    {
        Admission::Created(row) => row,
        _ => panic!("slot released"),
    };
    db::claim(&db.pool).await.unwrap();
    let id = row.id;
    // CPU has already completed successfully. Hold the row so cancel and finish
    // queue concurrently; PostgreSQL grants the earlier cancellation lock first.
    let mut barrier = db.pool.begin().await.unwrap();
    sqlx::query("select id from optimization_studies where id=$1 for update")
        .bind(id)
        .fetch_one(&mut *barrier)
        .await
        .unwrap();
    let cancel_pool = db.pool.clone();
    let cancel = tokio::spawn(async move { db::cancel(&cancel_pool, id).await.unwrap() });
    tokio::time::sleep(Duration::from_millis(50)).await;
    let finish_pool = db.pool.clone();
    let finish_progress = progress.clone();
    let finish = tokio::spawn(async move {
        db::finish(
            &finish_pool,
            id,
            "completed",
            &finish_progress,
            Some(&json!({"reason":"trial_budget","best":{"partial":false,"score":1}})),
            None,
            &json!([{ "at":"fixture", "kind":"completed", "message":"Исследование завершено" }]),
        )
        .await
        .unwrap()
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    barrier.commit().await.unwrap();
    cancel.await.unwrap();
    finish.await.unwrap();
    let row = db::get(&db.pool, id).await.unwrap().unwrap();
    assert_eq!(row.status, "cancelled");
    assert_eq!(row.result.as_ref().unwrap()["reason"], "cancelled");
    assert_eq!(row.result.unwrap()["best"]["partial"], true);
    assert_eq!(row.events[0]["kind"], "cancelled");
    assert!(
        row.events
            .as_array()
            .unwrap()
            .iter()
            .all(|event| event["kind"] != "completed")
    );
    db.cleanup().await;
}

#[tokio::test]
#[ignore = "requires isolated TEST_DATABASE_URL ending in _test with CREATEDB"]
async fn cpu_cancel_drains_owned_evaluation_before_releasing_slot() {
    let db = TestDb::new().await;
    let (url, _, server) = market(Duration::ZERO, 0).await;
    let config = OptimizationConfig {
        cpu_threads: 1,
        batch_size: 1,
        ..OptimizationConfig::default()
    };
    let runtime = start_optimization_runtime(db.pool.clone(), url, "key".into(), config.clone())
        .await
        .unwrap();
    let router = app(db.pool.clone(), config);
    let mut body = spec();
    body["toUnix"] = json!(172800 + 7 * 86400);
    body["maxTrials"] = json!(4096);
    body["algorithmSearch"] = json!({"priceGrowthPct":(1..=128).map(|n|n.to_string()).collect::<Vec<_>>(),"windowMinutes":(1..=16).collect::<Vec<_>>()});
    let (_, created) = request(&router, "POST", "/v1/optimization-studies", body).await;
    let id = created["id"].as_i64().expect("created study");
    let running = wait_for(&db.pool, id, |row| {
        row.status == "running" && row.progress["phase"] != "acquiring"
    })
    .await;
    assert!(
        running.result.is_none(),
        "full winners must stay RAM-only while running"
    );
    assert_eq!(
        request(
            &router,
            "POST",
            &format!("/v1/optimization-studies/{id}/cancel"),
            Value::Null
        )
        .await
        .0,
        StatusCode::ACCEPTED
    );
    let terminal = wait_for(&db.pool, id, |row| !row.active()).await;
    assert_eq!(terminal.status, "cancelled");
    let outcome = terminal.result.unwrap();
    assert_eq!(outcome["reason"], "cancelled");
    if !outcome["best"].is_null() {
        assert_eq!(outcome["best"]["partial"], true);
    }
    let (_, next) = request(&router, "POST", "/v1/optimization-studies", spec()).await;
    let next_id = next["id"].as_i64().expect("slot released after drain");
    assert_ne!(next_id, id);
    wait_for(&db.pool, next_id, |row| !row.active()).await;
    runtime.stop().await;
    server.abort();
    db.cleanup().await;
}

#[tokio::test]
#[ignore = "requires isolated TEST_DATABASE_URL ending in _test with CREATEDB"]
async fn snapshot_byte_cap_is_explicit_and_failure_releases_owned_input() {
    let db = TestDb::new().await;
    let (url, _, server) = market(Duration::ZERO, 0).await;
    let config = OptimizationConfig {
        max_snapshot_bytes: 32 * 1024,
        ..OptimizationConfig::default()
    };
    let runtime = start_optimization_runtime(db.pool.clone(), url, "key".into(), config.clone())
        .await
        .unwrap();
    let router = app(db.pool.clone(), config);
    for _ in 0..2 {
        let (status, created) = request(&router, "POST", "/v1/optimization-studies", spec()).await;
        assert_eq!(status, StatusCode::ACCEPTED);
        let row = wait_for(&db.pool, created["id"].as_i64().unwrap(), |row| {
            !row.active()
        })
        .await;
        assert_eq!(row.status, "failed");
        assert!(row.result.is_none());
        let error = row.error.unwrap();
        assert_eq!(error["code"], "resource_limit");
        assert!(
            error["message"]
                .as_str()
                .unwrap()
                .contains("OPTIMIZATION_MAX_SNAPSHOT_BYTES")
        );
    }
    runtime.stop().await;
    server.abort();
    db.cleanup().await;
}

fn compute_spec() -> Value {
    let mut body = spec();
    body["toUnix"] = json!(172800 + 7 * 86400);
    body["maxTrials"] = json!(4096);
    body["algorithmSearch"] = json!({"priceGrowthPct":(1..=128).map(|n|n.to_string()).collect::<Vec<_>>(),"windowMinutes":(1..=16).collect::<Vec<_>>()});
    body
}

#[tokio::test]
#[ignore = "requires isolated TEST_DATABASE_URL ending in _test with CREATEDB"]
async fn strict_snapshot_rejects_missing_cursor_identity_and_incomplete_pagination() {
    let db = TestDb::new().await;
    for mode in [2, 3, 4, 5] {
        let (url, requests, server) = market(Duration::ZERO, mode).await;
        let runtime = start_optimization_runtime(
            db.pool.clone(),
            url,
            "key".into(),
            OptimizationConfig::default(),
        )
        .await
        .unwrap();
        let router = app_with_health(
            db.pool.clone(),
            OptimizationConfig::default(),
            runtime.available.clone(),
        );
        let mut body = spec();
        if mode == 5 {
            body["toUnix"] = json!(172800 + 4 * 86400);
        }
        let (status, created) = request(&router, "POST", "/v1/optimization-studies", body).await;
        assert_eq!(status, StatusCode::ACCEPTED);
        let row = wait_for(&db.pool, created["id"].as_i64().unwrap(), |r| !r.active()).await;
        assert_eq!(row.status, "failed", "mode={mode}");
        assert!(row.result.is_none());
        assert_eq!(
            row.error.unwrap()["code"],
            if mode == 5 {
                "market_data_unavailable"
            } else {
                "invalid_market_data"
            }
        );
        if mode == 5 {
            assert!(requests.load(Ordering::SeqCst) >= 2);
        }
        runtime.stop().await;
        server.abort();
    }
    db.cleanup().await;
}

#[tokio::test]
#[ignore = "requires isolated TEST_DATABASE_URL ending in _test with CREATEDB"]
async fn compute_deadline_is_normal_completion_and_does_not_mark_winner_partial() {
    let db = TestDb::new().await;
    let (url, _, server) = market(Duration::ZERO, 0).await;
    let config = OptimizationConfig {
        cpu_threads: 1,
        batch_size: 1,
        ..OptimizationConfig::default()
    };
    let runtime = start_optimization_runtime(db.pool.clone(), url, "key".into(), config.clone())
        .await
        .unwrap();
    let router = app_with_health(db.pool.clone(), config, runtime.available.clone());
    let mut body = compute_spec();
    body["maxSeconds"] = json!(2);
    let (_, created) = request(&router, "POST", "/v1/optimization-studies", body).await;
    let row = wait_for(&db.pool, created["id"].as_i64().unwrap(), |r| !r.active()).await;
    assert_eq!(row.status, "completed", "{:?}", row.error);
    assert!(row.error.is_none());
    let result = row.result.unwrap();
    assert_eq!(result["reason"], "time_budget");
    assert!(
        result["best"].is_object(),
        "fixture baseline completes before deadline"
    );
    assert_eq!(result["best"]["partial"], false);
    runtime.stop().await;
    server.abort();
    db.cleanup().await;
}

#[tokio::test]
#[ignore = "requires isolated TEST_DATABASE_URL ending in _test with CREATEDB"]
async fn graceful_shutdown_fails_acquisition_and_cpu_work_instead_of_user_cancel() {
    let db = TestDb::new().await;
    for computing in [false, true] {
        let (url, requests, server) = market(
            if computing {
                Duration::ZERO
            } else {
                Duration::from_secs(10)
            },
            0,
        )
        .await;
        let config = OptimizationConfig {
            cpu_threads: 1,
            batch_size: 1,
            ..OptimizationConfig::default()
        };
        let runtime =
            start_optimization_runtime(db.pool.clone(), url, "key".into(), config.clone())
                .await
                .unwrap();
        let router = app_with_health(db.pool.clone(), config, runtime.available.clone());
        let (_, created) = request(
            &router,
            "POST",
            "/v1/optimization-studies",
            if computing { compute_spec() } else { spec() },
        )
        .await;
        let id = created["id"].as_i64().unwrap();
        wait_for(&db.pool, id, |r| {
            r.status == "running" && (!computing || r.progress["phase"] != "acquiring")
        })
        .await;
        tokio::time::timeout(Duration::from_secs(5), async {
            while requests.load(Ordering::SeqCst) == 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        tokio::time::timeout(Duration::from_secs(5), runtime.stop())
            .await
            .expect("shutdown drains cooperative work");
        let row = db::get(&db.pool, id).await.unwrap().unwrap();
        assert_eq!(row.status, "failed");
        assert!(row.result.is_none());
        assert_eq!(row.error.unwrap()["code"], "interrupted_by_restart");
        assert!(
            row.events
                .as_array()
                .unwrap()
                .iter()
                .all(|event| event["kind"] != "cancelled")
        );
        assert_eq!(
            request(&router, "POST", "/v1/optimization-studies", spec())
                .await
                .0,
            StatusCode::SERVICE_UNAVAILABLE
        );
        server.abort();
    }
    db.cleanup().await;
}

#[tokio::test]
#[ignore = "requires isolated TEST_DATABASE_URL ending in _test with CREATEDB"]
async fn transient_terminal_failure_retries_compact_payload_and_dead_owner_rejects_creates() {
    let db = TestDb::new().await;
    let (url, _, server) = market(Duration::ZERO, 0).await;
    let runtime = start_optimization_runtime(
        db.pool.clone(),
        url,
        "key".into(),
        OptimizationConfig::default(),
    )
    .await
    .unwrap();
    let router = app_with_health(
        db.pool.clone(),
        OptimizationConfig::default(),
        runtime.available.clone(),
    );
    sqlx::raw_sql("create table optimizer_finish_gate (blocked boolean not null); insert into optimizer_finish_gate values(true); create function optimizer_fail_finish() returns trigger language plpgsql as $$ begin if NEW.status='completed' and (select blocked from optimizer_finish_gate) then raise exception 'injected transient terminal failure'; end if; return NEW; end $$; create trigger optimizer_finish_fault before update on optimization_studies for each row execute function optimizer_fail_finish();").execute(&db.pool).await.unwrap();
    let (_, created) = request(&router, "POST", "/v1/optimization-studies", spec()).await;
    let id = created["id"].as_i64().unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while runtime.available.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let pending = db::get(&db.pool, id).await.unwrap().unwrap();
    assert!(pending.active());
    assert!(pending.result.is_none());
    assert_eq!(
        request(&router, "POST", "/v1/optimization-studies", spec())
            .await
            .0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    sqlx::query("update optimizer_finish_gate set blocked=false")
        .execute(&db.pool)
        .await
        .unwrap();
    let row = wait_for(&db.pool, id, |r| !r.active()).await;
    assert_eq!(row.status, "completed");
    assert!(row.result.unwrap()["best"].is_object());
    tokio::time::timeout(Duration::from_secs(5), async {
        while !runtime.available.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let pid:i32=sqlx::query_scalar("select pid from pg_locks where locktype='advisory' and database=(select oid from pg_database where datname=current_database()) and objid=$1::bigint::oid and granted").bind(db::RUNTIME_LOCK).fetch_one(&db.pool).await.unwrap();
    sqlx::query("select pg_terminate_backend($1)")
        .bind(pid)
        .execute(&db.pool)
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while runtime.available.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let (status, error) = request(&router, "POST", "/v1/optimization-studies", spec()).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(error["error"], "optimization_runtime_unavailable");
    let count: i64 = sqlx::query_scalar("select count(*) from optimization_studies")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
    runtime.stop().await;
    server.abort();
    db.cleanup().await;
}

#[tokio::test]
#[ignore = "requires isolated TEST_DATABASE_URL ending in _test with CREATEDB"]
async fn lease_loss_during_acquisition_and_compute_cancels_and_drains_owned_work() {
    let db = TestDb::new().await;
    for computing in [false, true] {
        let (url, _, server) = market(
            if computing {
                Duration::ZERO
            } else {
                Duration::from_secs(10)
            },
            0,
        )
        .await;
        let config = OptimizationConfig {
            cpu_threads: 1,
            batch_size: 1,
            ..OptimizationConfig::default()
        };
        let runtime =
            start_optimization_runtime(db.pool.clone(), url, "key".into(), config.clone())
                .await
                .unwrap();
        let router = app_with_health(db.pool.clone(), config, runtime.available.clone());
        let (_, created) = request(
            &router,
            "POST",
            "/v1/optimization-studies",
            if computing { compute_spec() } else { spec() },
        )
        .await;
        let id = created["id"].as_i64().unwrap();
        wait_for(&db.pool, id, |r| {
            r.status == "running" && (!computing || r.progress["phase"] != "acquiring")
        })
        .await;
        let pid:i32=sqlx::query_scalar("select pid from pg_locks where locktype='advisory' and database=(select oid from pg_database where datname=current_database()) and objid=$1::bigint::oid and granted").bind(db::RUNTIME_LOCK).fetch_one(&db.pool).await.unwrap();
        sqlx::query("select pg_terminate_backend($1)")
            .bind(pid)
            .execute(&db.pool)
            .await
            .unwrap();
        let row = wait_for(&db.pool, id, |r| !r.active()).await;
        assert_eq!(row.status, "failed");
        assert!(row.result.is_none());
        assert_eq!(row.error.unwrap()["code"], "interrupted_by_restart");
        assert!(!runtime.available.load(Ordering::SeqCst));
        assert_eq!(
            request(&router, "POST", "/v1/optimization-studies", spec())
                .await
                .0,
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert!(
            db::cancellation_requested(&db.pool, id).await.is_err(),
            "a terminal row is ownership loss, not continue"
        );
        tokio::time::timeout(Duration::from_secs(5), runtime.stop())
            .await
            .unwrap();
        server.abort();
    }
    db.cleanup().await;
}

#[tokio::test]
#[ignore = "requires isolated TEST_DATABASE_URL ending in _test with CREATEDB"]
async fn shutdown_with_unavailable_terminal_storage_is_bounded_and_next_start_recovers() {
    let db = TestDb::new().await;
    let (url, _, server) = market(Duration::ZERO, 0).await;
    let runtime = start_optimization_runtime(
        db.pool.clone(),
        url.clone(),
        "key".into(),
        OptimizationConfig::default(),
    )
    .await
    .unwrap();
    let router = app_with_health(
        db.pool.clone(),
        OptimizationConfig::default(),
        runtime.available.clone(),
    );
    sqlx::raw_sql("create table optimizer_finish_gate (blocked boolean not null); insert into optimizer_finish_gate values(true); create function optimizer_fail_finish() returns trigger language plpgsql as $$ begin if NEW.status in ('completed','failed','cancelled') and (select blocked from optimizer_finish_gate) then raise exception 'injected unavailable terminal storage'; end if; return NEW; end $$; create trigger optimizer_finish_fault before update on optimization_studies for each row execute function optimizer_fail_finish();").execute(&db.pool).await.unwrap();
    let (_, created) = request(&router, "POST", "/v1/optimization-studies", spec()).await;
    let id = created["id"].as_i64().unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while runtime.available.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(5), runtime.stop())
        .await
        .expect("shutdown metadata flush must be bounded");
    let abandoned = db::get(&db.pool, id).await.unwrap().unwrap();
    assert!(abandoned.active());
    assert!(abandoned.result.is_none());
    assert_eq!(
        request(&router, "POST", "/v1/optimization-studies", spec())
            .await
            .0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    sqlx::query("update optimizer_finish_gate set blocked=false")
        .execute(&db.pool)
        .await
        .unwrap();
    let resumed = start_optimization_runtime(
        db.pool.clone(),
        url,
        "key".into(),
        OptimizationConfig::default(),
    )
    .await
    .unwrap();
    let recovered = db::get(&db.pool, id).await.unwrap().unwrap();
    assert_eq!(recovered.status, "failed");
    assert_eq!(recovered.error.unwrap()["code"], "interrupted_by_restart");
    assert!(recovered.result.is_none());
    resumed.stop().await;
    server.abort();
    db.cleanup().await;
}
