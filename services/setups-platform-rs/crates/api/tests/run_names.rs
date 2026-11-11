use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use setups_api::SessionAuth;
use setups_api::batches::batches_router;
use setups_api::replay::{ReplayState, replay_router};
use setups_db::admission::ReplayAdmissionLimits;
use setups_db::analytics::list_analytics_sets;
use setups_db::migrate::ensure_migrations;
use setups_domain::{SPIKE_ALGORITHM_ID, SPIKE_DROP_ALGORITHM_ID};
use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions};
use std::str::FromStr;
use std::sync::Arc;
use tower::ServiceExt;

fn app(pool: PgPool) -> Router {
    let state = Arc::new(ReplayState {
        pool,
        auth: Arc::new(SessionAuth::new(&["test-key".to_string()])),
        market_url: "http://127.0.0.1:1".into(),
        market_key: "unused".into(),
        limits: ReplayAdmissionLimits::default(),
    });
    replay_router(state.clone()).merge(batches_router(state))
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
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&body).unwrap())
}

#[tokio::test]
async fn rename_routes_reject_invalid_names_without_querying_database() {
    let pool = PgPoolOptions::new()
        .connect_lazy("postgres://unused:unused@127.0.0.1:1/unused_test")
        .unwrap();
    let app = app(pool);
    for path in ["/v1/replay-runs/1", "/v1/replay-batches/1"] {
        for body in [
            json!({ "name": " \t\n" }),
            json!({ "name": "Я".repeat(121) }),
            json!({}),
            json!({ "name": null }),
            json!({ "name": 42 }),
        ] {
            let (status, body) = request(&app, "PATCH", path, body).await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
            assert!(!body["error"].as_str().unwrap().is_empty());
        }
    }
}

#[tokio::test]
async fn rename_routes_require_auth_and_browser_mutation_guard() {
    let pool = PgPoolOptions::new()
        .connect_lazy("postgres://unused:unused@127.0.0.1:1/unused_test")
        .unwrap();
    let auth = Arc::new(SessionAuth::new(&["test-key".to_string()]));
    let token = match auth.post_session(Some("test-key"), setups_api::now_ms()) {
        setups_api::auth::PostSessionResult::Issued { token, .. } => token,
        result => panic!("unexpected session result: {result:?}"),
    };
    let state = Arc::new(ReplayState {
        pool,
        auth,
        market_url: "http://127.0.0.1:1".into(),
        market_key: "unused".into(),
        limits: ReplayAdmissionLimits::default(),
    });
    let app = replay_router(state.clone()).merge(batches_router(state));
    for path in ["/v1/replay-runs/1", "/v1/replay-batches/1"] {
        for (cookie, expected) in [
            (None, StatusCode::UNAUTHORIZED),
            (Some(&token), StatusCode::FORBIDDEN),
        ] {
            let mut request = Request::builder().method("PATCH").uri(path);
            if let Some(cookie) = cookie {
                request = request.header("cookie", format!("platform_session={cookie}"));
            }
            let response = app
                .clone()
                .oneshot(request.body(Body::from(r#"{"name":"New"}"#)).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), expected);
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
        let url = std::env::var("TEST_DATABASE_URL").expect("TEST_DATABASE_URL is required");
        let options = PgConnectOptions::from_str(&url).expect("valid PostgreSQL URL");
        assert!(
            options
                .get_database()
                .is_some_and(|name| name.ends_with("_test"))
        );
        let admin = PgPoolOptions::new()
            .max_connections(1)
            .connect_with(options.clone())
            .await
            .unwrap();
        let name = format!(
            "run_names_test_{}_{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap()
        );
        sqlx::query(&format!("create database {name}"))
            .execute(&admin)
            .await
            .unwrap();
        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect_with(options.database(&name))
            .await
            .unwrap();
        let baseline = include_str!("../../db/migrations/0010_baseline.sql")
            .lines()
            .filter(|line| !line.starts_with('\\') && !line.starts_with("SET transaction_timeout"))
            .collect::<Vec<_>>()
            .join("\n");
        sqlx::raw_sql(&baseline).execute(&pool).await.unwrap();
        sqlx::raw_sql("set search_path to public; create table kysely_migration (name varchar(255) primary key, timestamp varchar(255) not null); insert into kysely_migration values ('0009_legacy', 'legacy')")
            .execute(&pool).await.unwrap();
        Self { admin, pool, name }
    }

    async fn cleanup(self) {
        self.pool.close().await;
        sqlx::query(&format!("drop database {}", self.name))
            .execute(&self.admin)
            .await
            .unwrap();
        self.admin.close().await;
    }
}

async fn insert_run(
    pool: &PgPool,
    batch_id: Option<i32>,
    algorithm: &str,
    name: Option<&str>,
) -> i32 {
    sqlx::query_scalar(
        "insert into replay_runs (batch_id, exchange_code, category, symbol, from_ts, to_ts, algorithm_id, algorithm_version, params, status, setups, name, updated_at) \
         values ($1, 'bybit', 'linear', 'BTCUSDT', to_timestamp(1000), to_timestamp(2000), $2, '1.1.1', '{}', 'done', '[]', $3, to_timestamp(1)) returning id",
    ).bind(batch_id).bind(algorithm).bind(name).fetch_one(pool).await.unwrap()
}

async fn insert_batch(pool: &PgPool, name: Option<&str>) -> i32 {
    sqlx::query_scalar(
        "insert into replay_batches (exchange_code, category, algorithm_id, algorithm_version, params, from_ts, to_ts, instrument_count, name) \
         values ('bybit', 'linear', $1, '1.1.1', '{}', to_timestamp(1000), to_timestamp(2000), 1, $2) returning id",
    ).bind(SPIKE_DROP_ALGORITHM_ID).bind(name).fetch_one(pool).await.unwrap()
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL ending in _test and permission to create isolated databases"]
async fn run_names_migration_preserves_legacy_data_and_is_idempotent() {
    let db = TestDb::new().await;
    sqlx::query("insert into replay_runs (exchange_code, category, symbol, from_ts, to_ts, algorithm_id, algorithm_version, params) values ('bybit', 'linear', 'BTCUSDT', now(), now(), $1, '1.1.1', '{}')")
        .bind(SPIKE_ALGORITHM_ID).execute(&db.pool).await.unwrap();
    assert_eq!(
        ensure_migrations(&db.pool).await.unwrap(),
        "3 applied, 0 up to date"
    );
    assert_eq!(
        ensure_migrations(&db.pool).await.unwrap(),
        "0 applied, 3 up to date"
    );
    let names: Vec<Option<String>> = sqlx::query_scalar("select name from replay_runs")
        .fetch_all(&db.pool)
        .await
        .unwrap();
    assert_eq!(names, vec![None]);
    let recorded: i64 = sqlx::query_scalar(
        "select count(*) from kysely_migration where name = '0011_run_names.sql'",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(recorded, 1);
    sqlx::raw_sql(include_str!("../../db/migrations/0011_run_names.sql"))
        .execute(&db.pool)
        .await
        .unwrap();
    db.cleanup().await;
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL ending in _test and permission to create isolated databases"]
async fn rename_endpoints_persist_trimmed_names_in_any_status_and_return_404() {
    let db = TestDb::new().await;
    ensure_migrations(&db.pool).await.unwrap();
    let app = app(db.pool.clone());
    let batch_id = insert_batch(&db.pool, None).await;
    let child_id = insert_run(
        &db.pool,
        Some(batch_id),
        SPIKE_DROP_ALGORITHM_ID,
        Some("Child"),
    )
    .await;
    let run_id = insert_run(&db.pool, None, SPIKE_ALGORITHM_ID, None).await;
    for status in [
        "queued",
        "running",
        "cancel_requested",
        "done",
        "failed",
        "cancelled",
    ] {
        sqlx::query("update replay_runs set status = $2 where id = $1 or id = $3")
            .bind(run_id)
            .bind(status)
            .bind(child_id)
            .execute(&db.pool)
            .await
            .unwrap();
        for path in [
            format!("/v1/replay-runs/{run_id}"),
            format!("/v1/replay-batches/{batch_id}"),
        ] {
            let (code, body) = request(
                &app,
                "PATCH",
                &path,
                json!({ "name": format!("  {status} запуск  ") }),
            )
            .await;
            assert_eq!(code, StatusCode::OK);
            assert_eq!(
                body,
                json!({ "ok": true, "name": format!("{status} запуск") })
            );
        }
    }
    let (name, updated_at): (String, chrono::DateTime<chrono::Utc>) =
        sqlx::query_as("select name, updated_at from replay_runs where id = $1")
            .bind(run_id)
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(name, "cancelled запуск");
    assert!(updated_at.timestamp() > 1);
    let child_name: String = sqlx::query_scalar("select name from replay_runs where id = $1")
        .bind(child_id)
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(child_name, "Child");
    for prefix in ["replay-runs", "replay-batches"] {
        for id in ["2147483647", "4294967297"] {
            let (status, body) = request(
                &app,
                "PATCH",
                &format!("/v1/{prefix}/{id}"),
                json!({ "name": "Missing" }),
            )
            .await;
            assert_eq!(status, StatusCode::NOT_FOUND);
            assert!(body["error"].as_str().unwrap().ends_with("not found"));
        }
    }
    drop(app);
    db.cleanup().await;
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL ending in _test and permission to create isolated databases"]
async fn lists_details_and_analytics_use_custom_and_default_run_names() {
    let db = TestDb::new().await;
    ensure_migrations(&db.pool).await.unwrap();
    let app = app(db.pool.clone());
    let run_id = insert_run(&db.pool, None, SPIKE_ALGORITHM_ID, None).await;
    let batch_id = insert_batch(&db.pool, Some("")).await;
    insert_run(&db.pool, Some(batch_id), SPIKE_DROP_ALGORITHM_ID, None).await;
    for (run_name, batch_name, custom) in [
        (
            "Всплеск цены и оборота",
            "Падение цены и всплеск оборота",
            false,
        ),
        ("Мой прогон", "Мой пакет", true),
    ] {
        if custom {
            for (path, name) in [
                (format!("/v1/replay-runs/{run_id}"), run_name),
                (format!("/v1/replay-batches/{batch_id}"), batch_name),
            ] {
                assert_eq!(
                    request(&app, "PATCH", &path, json!({ "name": name }))
                        .await
                        .0,
                    StatusCode::OK
                );
            }
        }
        let (status, list) = request(&app, "GET", "/v1/replay-runs", Value::Null).await;
        assert_eq!(status, StatusCode::OK);
        let runs = list["runs"].as_array().unwrap();
        assert_eq!(runs.len(), 2);
        for (id, is_batch, name, prefix) in [
            (run_id, false, run_name, "replay-runs"),
            (batch_id, true, batch_name, "replay-batches"),
        ] {
            let dto = runs
                .iter()
                .find(|run| {
                    run["id"] == id && run["isBatch"].as_bool().unwrap_or(false) == is_batch
                })
                .unwrap();
            assert_eq!(dto["name"], name);
            let (status, detail) =
                request(&app, "GET", &format!("/v1/{prefix}/{id}"), Value::Null).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(detail["run"]["name"], name);
        }
        let sets = list_analytics_sets(&db.pool).await.unwrap();
        for (set_id, id, name, algorithm) in [
            (
                format!("run-{run_id}"),
                run_id,
                run_name,
                SPIKE_ALGORITHM_ID,
            ),
            (
                format!("batch-{batch_id}"),
                batch_id,
                batch_name,
                SPIKE_DROP_ALGORITHM_ID,
            ),
        ] {
            let set = sets.iter().find(|set| set.set_id == set_id).unwrap();
            assert_eq!(set.label, format!("#{id} · {name}"));
            assert_eq!(set.algorithm_id, algorithm);
            assert_eq!(set.algorithm_version.as_deref(), Some("1.1.1"));
        }
    }
    drop(app);
    db.cleanup().await;
}
