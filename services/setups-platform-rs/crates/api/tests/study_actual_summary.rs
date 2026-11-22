//! Browser-session HTTP history summaries against disposable PostgreSQL; no worker/market.
use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use setups_api::{
    SessionAuth,
    auth::PostSessionResult,
    optimization::{OptimizationState, optimization_router},
    replay::ReplayState,
    study_actual::study_actual_router,
};
use setups_db::study_actual as db;
use sqlx::{
    PgPool,
    postgres::{PgConnectOptions, PgPoolOptions},
};
use std::{
    future::Future,
    sync::{Arc, atomic::AtomicBool},
};
use tower::ServiceExt;

async fn isolated<F, Fut>(body: F)
where
    F: FnOnce(PgPool) -> Fut + 'static,
    Fut: Future<Output = ()> + 'static,
{
    let options: PgConnectOptions = std::env::var("TEST_DATABASE_URL")
        .expect("explicit local *_test URL required")
        .parse()
        .unwrap();
    assert!(options.get_database().unwrap_or("").ends_with("_test"));
    assert!(matches!(
        options.get_host(),
        "127.0.0.1" | "localhost" | "::1"
    ));
    let admin = PgPoolOptions::new()
        .max_connections(2)
        .connect_with(options.clone())
        .await
        .unwrap();
    let name = format!(
        "actual_summary_{}_{}_test",
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
    let child = pool.clone();
    let result = tokio::task::LocalSet::new().run_until(async move { tokio::task::spawn_local(async move {
        let baseline = include_str!("../../db/migrations/0010_baseline.sql").lines()
            .filter(|line| !line.starts_with('\\') && !line.starts_with("SET transaction_timeout") && !line.contains("set_config('search_path'"))
            .collect::<Vec<_>>().join("\n");
        sqlx::raw_sql(&baseline).execute(&child).await.unwrap();
        sqlx::raw_sql("set search_path to public;create table kysely_migration(name varchar(255) primary key,timestamp varchar(255) not null);insert into kysely_migration values('0009_fixture','fixture')").execute(&child).await.unwrap();
        setups_db::migrate::ensure_migrations(&child).await.unwrap();
        assert!(setups_db::migrate::ensure_migrations(&child).await.unwrap().starts_with("0 applied"));
        body(child).await;
    }).await }).await;
    pool.close().await;
    sqlx::query(&format!("drop database {name} with(force)"))
        .execute(&admin)
        .await
        .unwrap();
    admin.close().await;
    if let Err(error) = result {
        std::panic::resume_unwind(error.into_panic());
    }
}

async fn seed(pool: &PgPool, owner: &str, start: i64) -> i64 {
    let mut params = json!(setups_domain::SpikeParams::default());
    params["baselineLongDays"] = json!(1);
    params["baselineShortDays"] = json!(1);
    let spec = json!({"algorithmId":"price_volume_spike","algorithmVersion":setups_domain::SPIKE_ALGORITHM_VERSION,
        "algorithmParams":params,"analyticsParams":{"clusterEnabled":false,"clusterGapMinutes":480,"timeShiftMinutes":0,"priceChangePct":null},
        "instruments":[{"exchange":"fixture","category":"linear","symbol":"FIX"}],"fromUnix":start-7*86400,"toUnix":start-60,
        "direction":"short","seed":7,"tpLevels":[10.0],"slLevels":[10.0],"minClosedPerCell":1});
    let result = json!({"best":{"score":1.0,"partial":false,"params":params,
        "analytics":{"clusterEnabled":false,"clusterGapMinutes":null,"timeShiftMinutes":0,"priceChangePct":null,"setupMaturity":null,"horizonMinutes":10080},
        "matrix":[{"tpPct":10.0,"slPct":10.0,"riskExpectancy":1.0,"avgMovePct":10.0,"closedCount":1,"tpCount":1,"slCount":0,"unclosedCount":0,"noDataCount":0,"priceChangeFilteredCount":0}],
        "provenance":{"optimizerVersion":"plan-a-3","seed":7,"snapshotHash":"immutable-summary-fixture","capturedAt":"2026-11-20T00:00:00Z"},"conclusions":[],
        "support":{"score":1.0,"setupCount":1,"selectedSetupCount":1,"evaluatedMinutes":1000,"warmupMinutes":1440},"trialIndex":0}});
    sqlx::query_scalar("insert into optimization_studies(algorithm_id,status,spec,progress,result,owner_id,execution_started_at) values('price_volume_spike','completed',$1,'{}',$2,$3,to_timestamp($4)) returning id")
        .bind(spec).bind(result).bind(owner).bind(start as f64).fetch_one(pool).await.unwrap()
}

fn fixture(pool: &PgPool) -> (Router, String) {
    let auth = Arc::new(SessionAuth::new(&["summary-fixture-key".into()]));
    let PostSessionResult::Issued { token, .. } =
        auth.post_session(Some("summary-fixture-key"), setups_api::now_ms())
    else {
        panic!("fixture login failed")
    };
    let cookie = format!("{}={token}", setups_api::COOKIE_NAME);
    let optimizer = optimization_router(Arc::new(OptimizationState {
        pool: pool.clone(),
        auth: auth.clone(),
        config: Default::default(),
        notify: Arc::new(tokio::sync::Notify::new()),
        available: Arc::new(AtomicBool::new(true)),
    }));
    let actual = study_actual_router(Arc::new(ReplayState {
        pool: pool.clone(),
        auth,
        market_url: "http://127.0.0.1:1".into(),
        market_key: "unused".into(),
        limits: Default::default(),
    }));
    (optimizer.merge(actual), cookie)
}

async fn http(app: &Router, cookie: &str, method: &str, path: &str) -> (StatusCode, Value) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header("cookie", cookie)
                .header("host", "fixture.test")
                .header("origin", "http://fixture.test")
                .header("x-platform-request", "1")
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    assert_eq!(response.headers()["cache-control"], "no-store");
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap())
}

const KEYS: [&str; 12] = [
    "studyId",
    "eligibility",
    "jobId",
    "requestOrigin",
    "revision",
    "status",
    "stage",
    "observedThroughUnix",
    "counts",
    "meanR",
    "lastSetupAt",
    "error",
];
fn project(value: &Value) -> Value {
    Value::Object(
        KEYS.into_iter()
            .map(|key| (key.to_owned(), value[key].clone()))
            .collect(),
    )
}
fn schema(value: &Value, id: i64) {
    assert_eq!(value["studyId"], id);
    for key in KEYS {
        assert!(value.get(key).is_some(), "missing TS field {key}: {value}");
    }
    assert!(value["eligibility"]["eligible"].is_boolean());
    assert!(value["eligibility"]["reason"].is_null() || value["eligibility"]["reason"].is_string());
    assert!(value["jobId"].is_null() || value["jobId"].is_string());
    assert!(value["revision"].as_u64().is_some());
    assert!(matches!(
        value["status"].as_str(),
        Some("not_calculated" | "queued" | "running" | "completed" | "failed")
    ));
    if value["status"] == "not_calculated" {
        assert!(value["stage"].is_null());
    } else {
        assert!(matches!(
            value["stage"].as_str(),
            Some(
                "queued"
                    | "acquiring"
                    | "replaying"
                    | "evaluating"
                    | "publishing"
                    | "completed"
                    | "failed"
            )
        ));
    }
    assert!(
        value["observedThroughUnix"].is_null() || value["observedThroughUnix"].as_u64().is_some()
    );
    assert!(value["meanR"].is_null() || value["meanR"].as_f64().is_some_and(f64::is_finite));
    assert!(
        value["lastSetupAt"].is_null()
            || value["lastSetupAt"].as_str().is_some_and(
                |at| at.ends_with('Z') && chrono::DateTime::parse_from_rfc3339(at).is_ok()
            )
    );
    if !value["counts"].is_null() {
        for key in [
            "total",
            "closed",
            "tp",
            "sl",
            "gateFailed",
            "unclosed",
            "noData",
        ] {
            assert!(value["counts"][key].as_u64().is_some(), "TS count {key}");
        }
        assert!(value["counts"]["closed"].as_u64() <= value["counts"]["total"].as_u64());
        if value["counts"]["closed"] == 0 {
            assert!(value["meanR"].is_null());
        }
    }
    if !value["error"].is_null() {
        for key in ["code", "stage", "message"] {
            assert!(value["error"][key].is_string());
        }
    }
    for heavy in [
        "source",
        "sourceFacts",
        "report",
        "matrix",
        "events",
        "artifact",
        "candles",
    ] {
        assert!(value.get(heavy).is_none(), "heavy list field {heavy}");
    }
    assert_eq!(value.as_object().unwrap().len(), KEYS.len());
}

async fn listed(app: &Router, cookie: &str, id: i64) -> Value {
    let (status, page) = http(app, cookie, "GET", "/v1/optimization-studies?limit=100").await;
    assert_eq!(status, StatusCode::OK, "{page}");
    let value = page["studies"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == id)
        .unwrap()["actual"]
        .clone();
    schema(&value, id);
    value
}

async fn same_as_get(app: &Router, cookie: &str, id: i64, expected_status: &str) -> Value {
    let summary = listed(app, cookie, id).await;
    let (status, detail) = http(
        app,
        cookie,
        "GET",
        &format!("/v1/optimization-studies/{id}/actual"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{detail}");
    assert_eq!(summary, project(&detail));
    assert_eq!(summary["status"], expected_status);
    summary
}

async fn capture_page(
    pages: &mut serde_json::Map<String, Value>,
    app: &Router,
    cookie: &str,
    label: &str,
) {
    assert!(pages.len() < 5);
    let (status, page) = http(app, cookie, "GET", "/v1/optimization-studies?limit=100").await;
    assert_eq!(status, StatusCode::OK);
    pages.insert(label.to_owned(), page);
}

#[tokio::test]
#[ignore = "explicit local TEST_DATABASE_URL ending _test and CREATEDB required"]
async fn browser_history_enables_first_refresh_and_matches_real_get_for_every_head_state() {
    isolated(|pool| async move {
        let start = chrono::Utc::now().timestamp().div_euclid(60) * 60 - 600;
        let id = seed(&pool, "admin", start).await;
        let legacy_ineligible = seed(&pool, "admin", start).await;
        sqlx::query("update optimization_studies set execution_started_at=null,events='[]' where id=$1")
            .bind(legacy_ineligible).execute(&pool).await.unwrap();
        let (app, cookie) = fixture(&pool);
        let mut producer_pages = serde_json::Map::new();
        let idle = same_as_get(&app, &cookie, id, "not_calculated").await;
        assert_eq!(idle["eligibility"], json!({"eligible":true,"reason":null}), "no original chart/head must not disable first click");
        assert_eq!(idle["revision"], 0);
        assert!(idle["lastSetupAt"].is_null());
        assert!(idle["jobId"].is_null() && idle["counts"].is_null() && idle["observedThroughUnix"].is_null());
        assert_eq!(listed(&app, &cookie, legacy_ineligible).await["eligibility"],
            json!({"eligible":false,"reason":"execution_start_unavailable"}));
        capture_page(&mut producer_pages, &app, &cookie, "idle").await;
        let refresh = format!("/v1/optimization-studies/{id}/actual/refresh");
        let (status, accepted) = http(&app, &cookie, "POST", &refresh).await;
        assert_eq!(status, StatusCode::ACCEPTED, "{accepted}");
        assert_eq!(same_as_get(&app, &cookie, id, "queued").await, project(&accepted));
        capture_page(&mut producer_pages, &app, &cookie, "queued").await;
        let job = db::claim(&pool, 2).await.unwrap().unwrap();
        same_as_get(&app, &cookie, id, "running").await;
        assert!(db::progress(&pool, &job, "evaluating", &json!({"evaluatedSetups":0})).await.unwrap());
        assert_eq!(same_as_get(&app, &cookie, id, "running").await["stage"], "evaluating");
        capture_page(&mut producer_pages, &app, &cookie, "running").await;
        let batch = db::create_batch(&pool, &job).await.unwrap().unwrap();
        let setup_rows = vec![json!({"setupTimeUnix":start+60,"status":"tp","riskReturn":1.0}),
            json!({"setupTimeUnix":start+120,"status":"sl","riskReturn":-1.0})];
        assert!(db::append(&pool, &job, batch, db::InstrumentOutput { exchange:"fixture",category:"linear",symbol:"FIX",
            emitted:&json!(setup_rows),rows:&setup_rows,quality:&json!({}) }).await.unwrap());
        assert!(same_as_get(&app, &cookie, id, "running").await["lastSetupAt"].is_null(), "staging rows must stay unpublished");
        let totals = json!({"total":2,"closed":2,"tp":1,"sl":1,"gateFailed":0,"unclosed":0,"noData":0,"meanR":0.0});
        assert!(db::finish(&pool, &job, &json!({"privateReport":"x".repeat(4096)}), &totals).await.unwrap());
        let done = same_as_get(&app, &cookie, id, "completed").await;
        assert_eq!(done["counts"], totals);
        assert_eq!(done["meanR"], 0.0);
        assert_eq!(done["lastSetupAt"], json!(chrono::DateTime::from_timestamp(start+120, 0).unwrap()));
        assert_eq!(db::page(&pool,"admin",id,1,0,100).await.unwrap().unwrap().len(), 2);
        assert_eq!(done["eligibility"]["eligible"], true);
        capture_page(&mut producer_pages, &app, &cookie, "completed").await;
        let (status, _) = http(&app, &cookie, "POST", &refresh).await;
        assert_eq!(status, StatusCode::ACCEPTED);
        let retry = db::claim(&pool, 2).await.unwrap().unwrap();
        db::fail(&pool, &retry, "fixture-failure").await.unwrap();
        let failed = same_as_get(&app, &cookie, id, "failed").await;
        assert_eq!(failed["revision"], 2);
        assert!(failed["counts"].is_null() && failed["meanR"].is_null());
        assert!(failed["lastSetupAt"].is_null());
        assert_eq!(failed["error"]["code"], "fixture-failure");
        capture_page(&mut producer_pages, &app, &cookie, "failed").await;
        // Optional producer evidence for frontend parity. These are complete
        // real HTTP response bodies, never hand-authored frontend DTO fixtures.
        if let Some(path) = std::env::var_os("STUDY_ACTUAL_PRODUCER_PAGES") {
            let bytes = serde_json::to_vec_pretty(&Value::Object(producer_pages)).unwrap();
            assert!(bytes.len() <= 256 * 1024);
            let text = std::str::from_utf8(&bytes).unwrap();
            assert!(!text.contains("summary-fixture-key"));
            assert!(!text.contains(cookie.split_once('=').unwrap().1), "never export session credentials");
            std::fs::write(path, bytes).expect("write sanitized real producer pages");
        }
    }).await;
}

#[tokio::test]
#[ignore = "explicit local TEST_DATABASE_URL ending _test and CREATEDB required"]
async fn last_setup_at_is_published_revision_owned_utc_max_for_every_outcome() {
    isolated(|pool| async move {
        // Database session timezone must not change the wire's UTC timestamp.
        let zoned = PgPoolOptions::new().max_connections(3).connect_with(
            (*pool.connect_options()).clone().options([("timezone", "Asia/Kathmandu")]),
        ).await.unwrap();
        let (app, cookie) = fixture(&zoned);
        let start = chrono::Utc::now().timestamp().div_euclid(60) * 60 - 600;
        let latest = start + 300;
        for outcome in ["tp", "sl", "gate_failed", "unclosed", "no_data"] {
            let id = seed(&pool, "admin", start).await;
            let actual = format!("/v1/optimization-studies/{id}/actual");
            let refresh = format!("{actual}/refresh");
            let idle = same_as_get(&app, &cookie, id, "not_calculated").await;
            assert!(idle["lastSetupAt"].is_null());
            assert_eq!(http(&app, &cookie, "POST", &refresh).await.0, StatusCode::ACCEPTED);
            let job = db::claim(&pool, 2).await.unwrap().unwrap();
            let batch = db::create_batch(&pool, &job).await.unwrap().unwrap();
            // Latest signal is inserted first. A later trade-close/evaluation
            // time on the older signal is deliberately not the requested max.
            let rows = vec![
                json!({"setupTimeUnix":latest,"status":outcome,"riskReturn":match outcome {"tp"=>Some(1.0),"sl"=>Some(-1.0),_=>None}}),
                json!({"setupTimeUnix":start+60,"closeTimeUnix":start+360,"checkedAtUnix":start+540,"status":"tp","riskReturn":1.0}),
            ];
            assert!(db::append(&pool, &job, batch, db::InstrumentOutput { exchange:"fixture",category:"linear",symbol:"FIX",
                emitted:&json!(rows),rows:&rows,quality:&json!({}) }).await.unwrap());
            assert!(same_as_get(&app, &cookie, id, "running").await["lastSetupAt"].is_null());
            let closed = if matches!(outcome, "tp" | "sl") { 2 } else { 1 };
            let totals = json!({"total":2,"closed":closed,"tp":1+i32::from(outcome=="tp"),"sl":i32::from(outcome=="sl"),
                "gateFailed":i32::from(outcome=="gate_failed"),"unclosed":i32::from(outcome=="unclosed"),"noData":i32::from(outcome=="no_data"),
                "meanR":if outcome=="sl" {0.0} else {1.0}});
            assert!(db::finish(&pool, &job, &json!({"fixture":true}), &totals).await.unwrap());
            // Fault-injected leftover revision must not contaminate this max.
            sqlx::query("insert into study_actual_setups(owner_id,study_id,revision,run_id,signal_close_unix,instrument_key,row_data) select owner_id,study_id,revision+100,run_id,$2,instrument_key,row_data from study_actual_setups where study_id=$1 limit 1")
                .bind(id).bind(start+540).execute(&pool).await.unwrap();
            let done = same_as_get(&app, &cookie, id, "completed").await;
            assert_eq!(done["lastSetupAt"], json!(chrono::DateTime::from_timestamp(latest, 0).unwrap()), "latest {outcome} must count before the price gate");
            let (status, displayed) = http(&app, &cookie, "GET", &format!("{actual}?timeZone=Asia%2FKathmandu")).await;
            assert_eq!(status, StatusCode::OK, "{displayed}");
            assert_eq!(displayed["lastSetupAt"], done["lastSetupAt"]);
            let raw_page = setups_db::optimization::list(&pool, None, 100, 0).await.unwrap();
            assert_eq!(raw_page.studies.iter().find(|s|s.id==id).unwrap().actual.as_ref().unwrap()["lastSetupAt"], done["lastSetupAt"]);
            // Invalid browser command must preserve the published dataset/head.
            let response = app.clone().oneshot(Request::builder().method("POST").uri(&refresh)
                .header("cookie", &cookie).header("host", "fixture.test").header("origin", "http://fixture.test")
                .header("x-platform-request", "1").header("content-type", "application/json")
                .body(Body::from("{\"winner\":{}}")).unwrap()).await.unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            assert_eq!(same_as_get(&app, &cookie, id, "completed").await, done);
            // Remove only the fault-injected row before exercising real cleanup.
            sqlx::query("delete from study_actual_setups where study_id=$1 and revision<>$2")
                .bind(id).bind(job.revision).execute(&pool).await.unwrap();
            assert_eq!(http(&app, &cookie, "POST", &refresh).await.0, StatusCode::ACCEPTED);
            assert!(same_as_get(&app, &cookie, id, "queued").await["lastSetupAt"].is_null());
            let replacement = db::claim(&pool, 2).await.unwrap().unwrap();
            db::fail(&pool, &replacement, "fixture_failure").await.unwrap();
            assert!(same_as_get(&app, &cookie, id, "failed").await["lastSetupAt"].is_null());
            assert_eq!(http(&app, &cookie, "POST", &refresh).await.0, StatusCode::ACCEPTED);
            let empty = db::claim(&pool, 2).await.unwrap().unwrap();
            let empty_batch = db::create_batch(&pool, &empty).await.unwrap().unwrap();
            assert!(db::append(&pool, &empty, empty_batch, db::InstrumentOutput { exchange:"fixture",category:"linear",symbol:"FIX",
                emitted:&json!([]),rows:&[],quality:&json!({}) }).await.unwrap());
            assert!(db::finish(&pool, &empty, &json!({}), &json!({"total":0,"closed":0,"tp":0,"sl":0,"gateFailed":0,"unclosed":0,"noData":0,"meanR":null})).await.unwrap());
            let empty = same_as_get(&app, &cookie, id, "completed").await;
            assert!(empty["lastSetupAt"].is_null());
            assert_eq!(empty["counts"]["total"], 0);
        }
        zoned.close().await;
    }).await;
}

#[tokio::test]
#[ignore = "explicit local TEST_DATABASE_URL ending _test and CREATEDB required"]
async fn bulk_idle_eligibility_requires_proven_start_winner_owner_and_supported_recipe() {
    isolated(|pool| async move {
        let start = chrono::Utc::now().timestamp().div_euclid(60) * 60 - 600;
        let missing = seed(&pool, "admin", start).await;
        let invalid = seed(&pool, "admin", start).await;
        let legacy = seed(&pool, "admin", start).await;
        let no_winner = seed(&pool, "admin", start).await;
        let foreign = seed(&pool, "foreign", start).await;
        let unsupported = seed(&pool, "admin", start).await;
        let continuous = seed(&pool, "admin", start).await;
        sqlx::raw_sql("insert into continuous_owner_lanes(owner_id) values('admin');insert into continuous_processes(owner_id,algorithm_id,config) values('admin','price_volume_spike','{}');insert into continuous_slots(owner_id,algorithm_id,min_setups_per_day) values('admin','price_volume_spike',0)").execute(&pool).await.unwrap();
        let generation: i64 = sqlx::query_scalar("insert into continuous_generations(owner_id,algorithm_id,slot_id,config_revision,lease_token,status,spec,execution_started_at,observation_cutoff) select owner_id,algorithm_id,id,1,0,'completed','{}',to_timestamp($1),$2 from continuous_slots where owner_id='admin' returning id")
            .bind(start as f64).bind(start-60).fetch_one(&pool).await.unwrap();
        sqlx::query("update optimization_studies set origin='continuous',continuous_generation_id=$2,execution_started_at=null where id=$1")
            .bind(continuous).bind(generation).execute(&pool).await.unwrap();
        sqlx::query("update optimization_studies set execution_started_at=null where id=any($1)").bind(vec![missing, invalid, legacy]).execute(&pool).await.unwrap();
        sqlx::query("update optimization_studies set events=$2 where id=$1").bind(invalid)
            .bind(json!([{"kind":"started","at":"not-a-timestamp"}])).execute(&pool).await.unwrap();
        sqlx::query("update optimization_studies set events=$2 where id=$1").bind(legacy)
            .bind(json!([{"kind":"started","at":"malformed"},{"kind":"started","at":chrono::DateTime::from_timestamp(start,0).unwrap().to_rfc3339()}])).execute(&pool).await.unwrap();
        sqlx::query("update optimization_studies set result=null where id=$1").bind(no_winner).execute(&pool).await.unwrap();
        sqlx::query("update optimization_studies set spec=jsonb_set(spec,'{algorithmVersion}','\"old\"') where id=$1").bind(unsupported).execute(&pool).await.unwrap();
        let (app, cookie) = fixture(&pool);
        for (id, reason) in [(missing,"execution_start_unavailable"),(invalid,"execution_start_unavailable"),
            (no_winner,"winner_missing"),(foreign,"study_not_found"),(unsupported,"algorithm_version_unavailable")] {
            let summary = listed(&app, &cookie, id).await;
            assert_eq!(summary["eligibility"], json!({"eligible":false,"reason":reason}));
            assert_eq!(summary["status"], "not_calculated");
        }
        assert_eq!(same_as_get(&app, &cookie, legacy, "not_calculated").await["eligibility"]["eligible"], true);
        assert_eq!(same_as_get(&app, &cookie, continuous, "not_calculated").await["eligibility"]["eligible"], true,
            "continuous generation start must work without a manual start or started event");
        assert_eq!(http(&app, &cookie, "GET", &format!("/v1/optimization-studies/{foreign}/actual")).await.0, StatusCode::NOT_FOUND);
        let (_, empty) = http(&app, &cookie, "GET", "/v1/optimization-studies?limit=1&offset=100").await;
        assert_eq!(empty["studies"], json!([]));
    }).await;
}
