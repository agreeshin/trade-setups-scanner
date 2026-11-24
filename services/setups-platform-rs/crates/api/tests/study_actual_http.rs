//! Actual HTTP producers + worker/session lifecycle against disposable PostgreSQL.
use axum::{
    Json, Router,
    body::Body,
    extract::{Query, State},
    http::{Request, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use setups_api::{
    SessionAuth,
    optimization::{OptimizationState, optimization_router},
    replay::ReplayState,
    study_actual::{start_actual_runtime, study_actual_router},
};
use setups_db::study_actual as db;
use sqlx::{
    PgPool,
    postgres::{PgConnectOptions, PgPoolOptions},
};
use std::{
    collections::HashMap,
    future::Future,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio_util::sync::CancellationToken;
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
    assert!(options.get_database().unwrap().ends_with("_test"));
    assert!(matches!(
        options.get_host(),
        "localhost" | "127.0.0.1" | "::1"
    ));
    let admin = PgPoolOptions::new()
        .max_connections(2)
        .connect_with(options.clone())
        .await
        .unwrap();
    let name = format!(
        "actual_http_{}_{}_test",
        std::process::id(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap()
    );
    sqlx::query(&format!("create database {name}"))
        .execute(&admin)
        .await
        .unwrap();
    let pool = PgPoolOptions::new()
        .max_connections(12)
        .connect_with(options.database(&name))
        .await
        .unwrap();
    let child = pool.clone();
    let result=tokio::task::LocalSet::new().run_until(async move {tokio::task::spawn_local(async move {
        // pg_dump 17's transaction_timeout is not available on the isolated PG16 fixture.
        let baseline=include_str!("../../db/migrations/0010_baseline.sql").lines().filter(|line|!line.starts_with('\\')&&!line.starts_with("SET transaction_timeout")&&!line.contains("set_config('search_path'")).collect::<Vec<_>>().join("\n");
        sqlx::raw_sql(&baseline).execute(&child).await.unwrap();
        sqlx::raw_sql("set search_path to public;create table kysely_migration(name varchar(255) primary key,timestamp varchar(255) not null);insert into kysely_migration values('0009_legacy','fixture')").execute(&child).await.unwrap();
        setups_db::migrate::ensure_migrations(&child).await.unwrap();assert!(setups_db::migrate::ensure_migrations(&child).await.unwrap().starts_with("0 applied"));body(child).await;
    }).await}).await;
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

async fn seed(pool: &PgPool, start: i64) -> i64 {
    let mut params = json!(setups_domain::SpikeParams::default());
    params["baselineLongDays"] = json!(1);
    params["baselineShortDays"] = json!(1);
    let analytics = json!({"clusterEnabled":false,"clusterGapMinutes":480,"timeShiftMinutes":1,"priceChangePct":null});
    let spec = json!({"algorithmId":"price_volume_spike","algorithmVersion":"1.1.1","algorithmParams":params,"analyticsParams":analytics,
        "instruments":[{"exchange":"fixture","category":"linear","symbol":"FIX"}],"fromUnix":start-86400*7,"toUnix":start-60,"direction":"short","seed":7,"tpLevels":[10.0],"slLevels":[10.0],"minClosedPerCell":1});
    let result = json!({"best":{"score":1.0,"partial":false,"params":params,"analytics":{"clusterEnabled":false,"clusterGapMinutes":null,"timeShiftMinutes":1,"priceChangePct":null,"setupMaturity":null,"horizonMinutes":10080},
        "matrix":[{"tpPct":10.0,"slPct":10.0,"riskExpectancy":1.0,"avgMovePct":10.0,"closedCount":1,"tpCount":1,"slCount":0,"unclosedCount":0,"noDataCount":0,"priceChangeFilteredCount":0}],
        "provenance":{"optimizerVersion":"plan-a-3","seed":7,"snapshotHash":"immutable-fixture","capturedAt":"2026-11-20T00:00:00Z"},"conclusions":[],
        "support":{"score":1.0,"setupCount":1,"selectedSetupCount":1,"evaluatedMinutes":1000,"warmupMinutes":1440},"trialIndex":0}});
    sqlx::query_scalar("insert into optimization_studies(algorithm_id,status,spec,progress,result,execution_started_at) values('price_volume_spike','completed',$1,'{}',$2,to_timestamp($3)) returning id")
        .bind(spec).bind(result).bind(start as f64).fetch_one(pool).await.unwrap()
}

struct Market {
    start: i64,
    ready: AtomicBool,
    ready_calls: AtomicUsize,
    hold: AtomicBool,
    fail: AtomicBool,
    calls: AtomicUsize,
    stop: CancellationToken,
    release: tokio::sync::Notify,
}
async fn ready(State(m): State<Arc<Market>>) -> StatusCode {
    m.ready_calls.fetch_add(1, Ordering::SeqCst);
    if m.ready.load(Ordering::SeqCst) { StatusCode::OK } else { StatusCode::SERVICE_UNAVAILABLE }
}
async fn candles(
    State(m): State<Arc<Market>>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    m.calls.fetch_add(1, Ordering::SeqCst);
    if m.hold.load(Ordering::SeqCst) {
        tokio::select! {_=m.release.notified()=>{},_=m.stop.cancelled()=>return StatusCode::SERVICE_UNAVAILABLE.into_response(),_=tokio::time::sleep(Duration::from_secs(10))=>return StatusCode::SERVICE_UNAVAILABLE.into_response()}
    }
    if m.fail.load(Ordering::SeqCst) {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let from = q["fromUnix"].parse::<i64>().unwrap();
    let to = q["toUnix"].parse::<i64>().unwrap();
    let after = q.get("afterUnix").map(|s| s.parse::<i64>().unwrap());
    let mut rows = Vec::new();
    for open in (m.start - 86400 - 600..m.start + 600).step_by(60) {
        if open < from || open >= to || after.is_some_and(|last| open <= last) {
            continue;
        }
        let (o, c, h, l, quote) = if open == m.start + 120 {
            ("100", "120", "120", "100", "10000")
        } else if open == m.start + 480 {
            ("120", "120", "121", "100", "100")
        } else if open > m.start + 120 {
            ("120", "120", "121", "119", "100")
        } else {
            ("100", "100", "101", "99", "100")
        };
        rows.push(json!({"openTimeUnix":open,"open":o,"high":h,"low":l,"close":c,"volume":"1","quoteTurnover":quote}));
    }
    Json(json!({"candles":rows,"nextCursorUnix":null})).into_response()
}
async fn fixture(
    pool: &PgPool,
    start: i64,
) -> (
    Router,
    Arc<ReplayState>,
    Arc<Market>,
    tokio::task::JoinHandle<()>,
) {
    // Manual lifecycle tests isolate their explicit jobs. Automatic tests below
    // re-enable or remove these rows to exercise genuine headless bootstrap.
    for algorithm in ["price_volume_spike", "price_drop_volume_spike"] {
        let settings = db::continuous::ensure(pool, "admin", algorithm)
            .await
            .unwrap();
        db::continuous::configure(
            pool,
            "admin",
            algorithm,
            settings.revision,
            false,
            &settings.from_date.to_string(),
            &settings.time_zone,
        )
        .await
        .unwrap();
    }
    let market = Arc::new(Market {
        start,
        ready: AtomicBool::new(true),
        ready_calls: AtomicUsize::new(0),
        hold: AtomicBool::new(false),
        fail: AtomicBool::new(false),
        calls: AtomicUsize::new(0),
        stop: CancellationToken::new(),
        release: tokio::sync::Notify::new(),
    });
    let md=Router::new().route("/ready",get(ready)).route("/v1/instruments",get(||async {Json(json!({"instruments":[{"id":1,"exchange":"fixture","category":"linear","symbol":"FIX","status":"trading"}]}))})).route("/v1/candles",get(candles)).with_state(market.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, md).await.unwrap();
    });
    let auth = Arc::new(SessionAuth::new(&["fixture-key".into()]));
    let state = Arc::new(ReplayState {
        pool: pool.clone(),
        auth: auth.clone(),
        market_url: url,
        market_key: "fixture".into(),
        limits: Default::default(),
    });
    let app = study_actual_router(state.clone()).merge(optimization_router(Arc::new(
        OptimizationState {
            pool: pool.clone(),
            auth,
            config: Default::default(),
            notify: Arc::new(tokio::sync::Notify::new()),
            available: Arc::new(AtomicBool::new(true)),
        },
    )));
    (app, state, market, server)
}
async fn http(app: &Router, method: &str, path: &str) -> (StatusCode, Value) {
    http_body(app, method, path, json!({})).await
}
async fn http_body(app: &Router, method: &str, path: &str, body: Value) -> (StatusCode, Value) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header("x-api-key", "fixture-key")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let data = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&data).unwrap())
}

async fn publish_setup_page_fixture(app: &Router, pool: &PgPool, id: i64, rows: &[Value]) -> Value {
    let path = format!("/v1/optimization-studies/{id}/actual");
    assert_eq!(
        http(app, "POST", &format!("{path}/refresh")).await.0,
        StatusCode::ACCEPTED
    );
    let job = db::claim(pool, 2).await.unwrap().unwrap();
    assert_eq!(job.study, id);
    let batch = db::create_batch(pool, &job).await.unwrap().unwrap();
    assert!(
        db::append(
            pool,
            &job,
            batch,
            db::InstrumentOutput {
                exchange: "fixture",
                category: "linear",
                symbol: "FIX",
                emitted: &json!([]),
                rows,
                quality: &json!({}),
            }
        )
        .await
        .unwrap()
    );
    let count = |status| rows.iter().filter(|row| row["status"] == status).count();
    let (tp, sl) = (count("tp"), count("sl"));
    assert!(db::finish(pool, &job, &json!({"sealedReport":"unchanged"}), &json!({
        "total": rows.len(), "closed": tp + sl, "tp": tp, "sl": sl, "gateFailed": count("gate_failed"),
        "unclosed": count("unclosed"), "noData": count("no_data"), "meanR": (tp + sl > 0).then(|| (tp as f64 - sl as f64) / (tp + sl) as f64),
    })).await.unwrap());
    http(app, "GET", &path).await.1
}

#[tokio::test]
#[ignore = "requires isolated local TEST_DATABASE_URL ending _test with CREATEDB"]
async fn setup_list_gate_filter_counts_before_paging_is_readonly_and_keeps_published_totals() {
    isolated(|pool| async move {
        let start = chrono::Utc::now().timestamp().div_euclid(60) * 60 - 600;
        let id = seed(&pool, start).await;
        let (app, state, market, server) = fixture(&pool, start).await;
        let statuses = ["gate_failed", "tp", "sl", "unclosed", "no_data"];
        let mut rows: Vec<_> = (0..130).map(|i| json!({"setupTimeUnix":start + i, "status":statuses[i as usize % 5], "sentinel":i})).collect();
        // Only an explicit saved gate failure is excluded, not WATCH/missing/null outcomes.
        rows.extend([json!({"setupTimeUnix":start + 130,"status":"watch","sentinel":130}),
            json!({"setupTimeUnix":start + 131,"status":null,"sentinel":131}),
            json!({"setupTimeUnix":start + 132,"sentinel":132})]);
        let published = publish_setup_page_fixture(&app, &pool, id, &rows).await;
        let failed_id = seed(&pool, start).await;
        let failures: Vec<_> = (0..65).map(|i| json!({"setupTimeUnix":start + i,"status":"gate_failed"})).collect();
        let all_failed = publish_setup_page_fixture(&app, &pool, failed_id, &failures).await;
        let foreign = seed(&pool, start).await;
        sqlx::query("update optimization_studies set owner_id='foreign' where id=$1").bind(foreign).execute(&pool).await.unwrap();
        let readonly = PgPoolOptions::new().max_connections(3).connect_with(
            (*pool.connect_options()).clone().options([("default_transaction_read_only", "on")]),
        ).await.unwrap();
        let reader = study_actual_router(Arc::new(ReplayState {
            pool: readonly.clone(), auth: state.auth.clone(), market_url: state.market_url.clone(),
            market_key: state.market_key.clone(), limits: state.limits,
        }));
        async fn snapshot(pool: &PgPool) -> Value {
            sqlx::query_scalar("select jsonb_build_object('studies',(select jsonb_agg(to_jsonb(s) order by id) from optimization_studies s),'heads',(select jsonb_agg(to_jsonb(a)||jsonb_build_object('xmin',a.xmin::text) order by study_id) from study_actualizations a),'rows',(select jsonb_agg(to_jsonb(s)||jsonb_build_object('xmin',s.xmin::text) order by id) from study_actual_setups s),'runs',(select jsonb_agg(to_jsonb(r) order by id) from replay_runs r),'batches',(select jsonb_agg(to_jsonb(b) order by id) from replay_batches b))")
                .fetch_one(pool).await.unwrap()
        }
        let before = snapshot(&pool).await;
        let path = format!("/v1/optimization-studies/{id}/actual/setups?revision=1");
        for (suffix, hidden) in [("", true), ("&hideGateFailed=true", true), ("&hideGateFailed=false", false)] {
            let expected: Vec<_> = rows.iter().rev().filter(|row| !hidden || row["status"] != "gate_failed").map(|row| row["sentinel"].clone()).collect();
            let mut received = Vec::new();
            for offset in [0, 50, 100, 150] {
                let (status, page) = http(&reader, "GET", &format!("{path}{suffix}&limit=50&offset={offset}")).await;
                assert_eq!(status, StatusCode::OK, "{page}");
                assert_eq!(page["total"], expected.len());
                assert_eq!(page["jobId"], published["jobId"]); assert_eq!(page["revision"], 1);
                assert_eq!(page["offset"], offset); assert_eq!(page["limit"], 50);
                let page_rows = page["setups"].as_array().unwrap();
                assert_eq!(page_rows.len(), expected.len().saturating_sub(offset as usize).min(50));
                assert!(page_rows.iter().all(|row| !hidden || row["status"] != "gate_failed"));
                received.extend(page_rows.iter().map(|row| row["sentinel"].clone()));
            }
            assert_eq!(received, expected, "filter must run before offset/limit, preserving order without holes/duplicates");
        }
        let failed_path = format!("/v1/optimization-studies/{failed_id}/actual");
        let (status, empty) = http(&reader, "GET", &format!("{failed_path}/setups?revision=1")).await;
        assert_eq!(status, StatusCode::OK); assert_eq!(empty["total"], 0); assert_eq!(empty["setups"], json!([]));
        let (status, restored) = http(&reader, "GET", &format!("{failed_path}/setups?revision=1&hideGateFailed=false")).await;
        assert_eq!(status, StatusCode::OK); assert_eq!(restored["total"], 65); assert_eq!(restored["setups"].as_array().unwrap().len(), 50);
        assert_eq!(http(&reader, "GET", &failed_path).await.1, all_failed);
        assert_eq!(all_failed["counts"]["total"], 65); assert_eq!(all_failed["counts"]["gateFailed"], 65);
        assert_eq!(all_failed["counts"]["closed"], 0); assert!(all_failed["meanR"].is_null());
        assert_eq!(http(&reader, "GET", &format!("/v1/optimization-studies/{id}/actual")).await.1, published);
        for missing in [foreign, 9_999_999] {
            assert_eq!(http(&reader, "GET", &format!("/v1/optimization-studies/{missing}/actual/setups?revision=1")).await.0, StatusCode::NOT_FOUND);
        }
        assert_eq!(http(&reader, "GET", &format!("{path}&hideGateFailed=false&revision=2")).await.0, StatusCode::BAD_REQUEST);
        assert_eq!(http(&reader, "GET", &format!("/v1/optimization-studies/{id}/actual/setups?revision=2")).await.0, StatusCode::CONFLICT);
        assert_eq!(snapshot(&pool).await, before, "GETs cannot rewrite study science, stored outcomes, reports, totals, jobs or row versions");
        assert_eq!(market.calls.load(Ordering::SeqCst), 0);
        readonly.close().await; market.stop.cancel(); server.abort(); let _ = server.await;
    }).await;
}

#[tokio::test]
#[ignore = "requires isolated local TEST_DATABASE_URL ending _test with CREATEDB"]
async fn setup_list_gate_filter_job_count_and_rows_share_repeatable_revision_snapshot() {
    isolated(|pool| async move {
        let start = chrono::Utc::now().timestamp().div_euclid(60) * 60 - 600;
        let id = seed(&pool, start).await;
        let (app, state, market, server) = fixture(&pool, start).await;
        let rows: Vec<_> = (0..130).map(|i| json!({"setupTimeUnix":start + i,"status":if i % 2 == 0 {"gate_failed"} else {"tp"},"sentinel":i})).collect();
        publish_setup_page_fixture(&app, &pool, id, &rows).await;
        let path = format!("/v1/optimization-studies/{id}/actual/setups?revision=1&offset=50");
        let (status, expected) = http(&app, "GET", &path).await; assert_eq!(status, StatusCode::OK);
        sqlx::raw_sql("alter table study_actual_setups rename to fixture_actual_setups;create function fixture_page_barrier() returns boolean language plpgsql volatile as $$begin if current_setting('application_name')='actual_page_reader' then perform pg_advisory_xact_lock(73100051);end if;return true;end$$;create view study_actual_setups as select s.* from fixture_actual_setups s where fixture_page_barrier();").execute(&pool).await.unwrap();
        let reader = PgPoolOptions::new().max_connections(2).connect_with((*pool.connect_options()).clone()
            .application_name("actual_page_reader").options([("default_transaction_read_only", "on")])).await.unwrap();
        let reader_app = study_actual_router(Arc::new(ReplayState {
            pool:reader.clone(), auth:state.auth.clone(), market_url:state.market_url.clone(), market_key:state.market_key.clone(), limits:state.limits,
        }));
        let mut held = pool.begin().await.unwrap();
        sqlx::query("select pg_advisory_xact_lock(73100051)").execute(&mut *held).await.unwrap();
        let old_path = path.clone();
        let pending = tokio::spawn(async move { http(&reader_app, "GET", &old_path).await });
        tokio::time::timeout(Duration::from_secs(3), async { loop {
            let waiting: bool = sqlx::query_scalar("select exists(select 1 from pg_stat_activity where datname=current_database() and application_name='actual_page_reader' and wait_event_type='Lock')").fetch_one(&pool).await.unwrap();
            if waiting { break; } tokio::time::sleep(Duration::from_millis(10)).await;
        }}).await.expect("reader must reach count after capturing revision/job head");
        let replacement = [json!({"setupTimeUnix":start+200,"status":"sl","sentinel":"replacement"})];
        let published = publish_setup_page_fixture(&app, &pool, id, &replacement).await;
        held.rollback().await.unwrap();
        let (status, old) = pending.await.unwrap();
        assert_eq!(status, StatusCode::OK, "{old}"); assert_eq!(old, expected, "head, filtered count, and deleted rows must stay together in snapshot N");
        let (status, invalidated) = http(&app, "GET", &path).await;
        assert_eq!(status, StatusCode::CONFLICT); assert_eq!(invalidated["error"], "study_actual_revision_changed");
        let (status, current) = http(&app, "GET", &format!("/v1/optimization-studies/{id}/actual/setups?revision=2")).await;
        assert_eq!(status, StatusCode::OK); assert_eq!(current["total"], 1); assert_eq!(current["jobId"], published["jobId"]);
        assert_ne!(current["jobId"], old["jobId"]); assert_eq!(current["setups"][0]["sentinel"], "replacement");
        assert_eq!(market.calls.load(Ordering::SeqCst), 0);
        reader.close().await; market.stop.cancel(); server.abort(); let _ = server.await;
    }).await;
}

#[tokio::test]
#[ignore = "requires isolated local TEST_DATABASE_URL ending _test with CREATEDB"]
async fn market_readiness_precedes_claim_and_ready_recovery_preserves_the_queued_job() {
    isolated(|pool| async move {
        let start = chrono::Utc::now().timestamp().div_euclid(60)*60-600;
        let id = seed(&pool,start).await; let (app,state,market,server) = fixture(&pool,start).await;
        market.ready.store(false,Ordering::SeqCst);
        let (status,queued) = http(&app,"POST",&format!("/v1/optimization-studies/{id}/actual/refresh")).await;
        assert_eq!(status,StatusCode::ACCEPTED,"{queued}");
        let head = db::get(&pool,"admin",id).await.unwrap().unwrap();
        let runtime = start_actual_runtime(state,2).await.unwrap();
        // Observe two actual rejected readiness calls, not an arbitrary sleep.
        // At least the first response has been consumed before the second call.
        tokio::time::timeout(Duration::from_secs(5),async {while market.ready_calls.load(Ordering::SeqCst)<2 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }}).await.expect("runtime must probe market readiness while a job is queued");
        assert_eq!(db::get(&pool,"admin",id).await.unwrap().unwrap(),head,"not-ready must not claim or replace the accepted revision");
        assert_eq!(market.calls.load(Ordering::SeqCst),0,"no candles before readiness");
        market.ready.store(true,Ordering::SeqCst);
        let completed = wait(&pool,id,"completed").await;
        assert_eq!(completed["revision"],head["revision"]); assert_eq!(completed["job_id"],head["job_id"]);
        assert_eq!(completed["observed_through_unix"],head["observed_through_unix"]);
        assert!(market.calls.load(Ordering::SeqCst)>0);
        runtime.stop().await; market.stop.cancel(); server.abort(); let _ = server.await;
    }).await;
}

#[tokio::test]
#[ignore = "requires isolated local TEST_DATABASE_URL ending _test with CREATEDB"]
async fn continuous_http_strict_revision_validation_readonly_get_and_browser_mutation_guards() {
    isolated(|pool|async move {
    let start=chrono::Utc::now().timestamp().div_euclid(60)*60-600;let id=seed(&pool,start).await;let (app,state,market,server)=fixture(&pool,start).await;
    let url="/v1/optimization-studies/actual/continuous?algorithmId=price_volume_spike";
    let (status,settings)=http(&app,"GET",url).await;assert_eq!(status,StatusCode::OK);assert_eq!(settings["enabled"],false);
    assert!(db::get(&pool,"admin",id).await.unwrap().is_none());assert_eq!(market.calls.load(Ordering::SeqCst),0);
    let revision=settings["revision"].as_i64().unwrap();let body=json!({"enabled":true,"fromDate":"2026-11-22","timeZone":"Asia/Kathmandu","revision":revision});
    for invalid in [json!({"enabled":"yes","fromDate":"2026-11-22","revision":revision}),json!({"enabled":true,"fromDate":"2026-02-30","revision":revision}),json!({"enabled":true,"fromDate":"2026-11-22","timeZone":"bad/zone","revision":revision}),json!({"enabled":true,"fromDate":"2026-11-22","revision":revision,"cursorStudyId":123}),json!({"enabled":true,"fromDate":"2026-11-22","revision":0})] {
        assert_eq!(http_body(&app,"PUT",url,invalid).await.0,StatusCode::BAD_REQUEST);
    }
    for query in ["", "?algorithmId=unknown", "?algorithmId=price_volume_spike&ownerId=foreign"] {assert_eq!(http(&app,"GET",&format!("/v1/optimization-studies/actual/continuous{query}")).await.0,StatusCode::BAD_REQUEST);}
    let (token,_)=state.auth.issue_session(chrono::Utc::now().timestamp_millis());
    for (platform,site) in [(false,"same-origin"),(true,"cross-site")] {
        let mut request=Request::builder().method("PUT").uri(url).header("cookie",format!("platform_session={token}")).header("content-type","application/json").header("sec-fetch-site",site);
        if platform {request=request.header("x-platform-request","1");}
        assert_eq!(app.clone().oneshot(request.body(Body::from(body.to_string())).unwrap()).await.unwrap().status(),StatusCode::FORBIDDEN);
    }
    let (status,saved)=http_body(&app,"PUT",url,body.clone()).await;assert_eq!(status,StatusCode::OK,"{saved}");assert_eq!(saved["revision"],revision+1);
    assert_eq!(http_body(&app,"PUT",url,body).await.0,StatusCode::CONFLICT);
    assert!(db::get(&pool,"admin",id).await.unwrap().is_none(),"settings PUT and GET do not execute a job");assert_eq!(market.calls.load(Ordering::SeqCst),0);
    market.stop.cancel();server.abort();let _=server.await;
}).await;
}

#[tokio::test]
#[ignore = "requires isolated local TEST_DATABASE_URL ending _test with CREATEDB"]
async fn missing_continuous_get_is_select_only_concurrent_and_put_can_initialize() {
    isolated(|pool| async move {
        let start = chrono::Utc::now().timestamp().div_euclid(60) * 60 - 600;
        let id = seed(&pool, start).await;
        let (_app, state, market, server) = fixture(&pool, start).await;
        sqlx::query("delete from study_actual_continuous").execute(&pool).await.unwrap();
        setups_db::settings_set(&pool, "ui", &json!({"interface":{"timezone":"Asia/Kathmandu"}})).await.unwrap();
        let readonly = PgPoolOptions::new().max_connections(3)
            .connect_with((*pool.connect_options()).clone().options([("default_transaction_read_only", "on")])).await.unwrap();
        let app = study_actual_router(Arc::new(ReplayState {
            pool: readonly.clone(), auth: state.auth.clone(), market_url: state.market_url.clone(),
            market_key: state.market_key.clone(), limits: state.limits,
        }));
        let url = "/v1/optimization-studies/actual/continuous?algorithmId=price_volume_spike";
        let mut held = pool.begin().await.unwrap();
        sqlx::query("select pg_advisory_xact_lock(hashtextextended('admin',72423415))").execute(&mut *held).await.unwrap();
        let before = chrono::Utc::now();
        let responses = tokio::time::timeout(Duration::from_secs(2), async {
            tokio::join!(http(&app, "GET", url), http(&app, "GET", url))
        }).await;
        held.rollback().await.unwrap();
        let ((left_status, left), (right_status, right)) = responses.expect("GET must not acquire the owner mutation lock");
        let after = chrono::Utc::now();
        for (status, response) in [(left_status, &left), (right_status, &right)] {
            assert_eq!(status, StatusCode::OK, "read-only PostgreSQL session must accept GET: {response}");
            assert_eq!(response["algorithmId"], "price_volume_spike");
            assert_eq!(response["enabled"], true);
            assert_eq!(response["revision"], 1);
            assert_eq!(response["cursorGeneration"], 1);
            assert_eq!(response["resetPending"], false);
            assert!(response["cursorStudyId"].is_null());
            assert!(response["cursorExecutionStartedAt"].is_null());
            assert!(response["lastSkipReason"].is_null());
            assert_eq!(response["timeZone"], "Asia/Kathmandu");
            assert_eq!(response["queue"], json!({"manualQueued":0,"automaticQueued":0,"running":null}));
            let due = chrono::DateTime::parse_from_rfc3339(response["nextDueAt"].as_str().unwrap()).unwrap().with_timezone(&chrono::Utc);
            assert!(due >= before && due <= after);
        }
        let expected: (String, chrono::DateTime<chrono::Utc>) = sqlx::query_as("select timezone('Asia/Kathmandu',clock_timestamp())::date::text,to_timestamp(study_local_day_start('Asia/Kathmandu',timezone('Asia/Kathmandu',clock_timestamp())::date))").fetch_one(&pool).await.unwrap();
        assert_eq!(left["fromDate"], expected.0);
        assert_eq!(left["fromBoundary"], json!(expected.1));
        let mut left_stable = left.clone();let mut right_stable = right.clone();
        left_stable.as_object_mut().unwrap().remove("nextDueAt");right_stable.as_object_mut().unwrap().remove("nextDueAt");
        assert_eq!(left_stable, right_stable, "concurrent reads resolve the same effective defaults");
        for zone in ["auto", "invalid/zone"] {
            setups_db::settings_set(&pool, "ui", &json!({"interface":{"timezone":zone}})).await.unwrap();
            let (status, fallback) = http(&app, "GET", url).await;
            assert_eq!(status, StatusCode::OK);assert_eq!(fallback["timeZone"], "UTC");
        }
        assert_eq!(sqlx::query_scalar::<_, i64>("select count(*) from study_actual_continuous").fetch_one(&pool).await.unwrap(), 0);
        assert_eq!(sqlx::query_scalar::<_, i64>("select count(*) from study_actualizations").fetch_one(&pool).await.unwrap(), 0);
        assert_eq!(market.calls.load(Ordering::SeqCst), 0);
        let write_app = study_actual_router(state.clone());
        assert_eq!(http(&write_app, "POST", &format!("/v1/optimization-studies/{id}/actual/refresh")).await.0, StatusCode::ACCEPTED);
        let queued: Value = sqlx::query_scalar("select to_jsonb(a) from study_actualizations a where study_id=$1").bind(id).fetch_one(&pool).await.unwrap();
        let (status, queued_view) = http(&app, "GET", url).await;
        assert_eq!(status, StatusCode::OK);assert_eq!(queued_view["queue"], json!({"manualQueued":1,"automaticQueued":0,"running":null}));
        assert_eq!(sqlx::query_scalar::<_, Value>("select to_jsonb(a) from study_actualizations a where study_id=$1").bind(id).fetch_one(&pool).await.unwrap(), queued);
        assert_eq!(sqlx::query_scalar::<_, i64>("select count(*) from study_actual_continuous").fetch_one(&pool).await.unwrap(), 0);
        let job = db::claim(&pool, 2).await.unwrap().unwrap();
        let running: Value = sqlx::query_scalar("select to_jsonb(a) from study_actualizations a where study_id=$1").bind(id).fetch_one(&pool).await.unwrap();
        let (status, running_view) = http(&app, "GET", url).await;
        assert_eq!(status, StatusCode::OK);assert_eq!(running_view["queue"]["manualQueued"], 0);
        assert_eq!(running_view["queue"]["running"]["jobId"], job.job_id);
        assert_eq!(running_view["queue"]["running"]["requestOrigin"], "manual");
        assert_eq!(sqlx::query_scalar::<_, Value>("select to_jsonb(a) from study_actualizations a where study_id=$1").bind(id).fetch_one(&pool).await.unwrap(), running);
        assert_eq!(sqlx::query_scalar::<_, i64>("select count(*) from study_actual_continuous").fetch_one(&pool).await.unwrap(), 0);
        db::fail(&pool, &job, "fixture_manual_terminal").await.unwrap();
        let terminal: Value = sqlx::query_scalar("select to_jsonb(a) from study_actualizations a where study_id=$1").bind(id).fetch_one(&pool).await.unwrap();
        assert_eq!(market.calls.load(Ordering::SeqCst), 0);
        readonly.close().await;
        setups_db::settings_set(&pool, "ui", &json!({"interface":{"timezone":"Asia/Kathmandu"}})).await.unwrap();
        let app = study_actual_router(state);
        let (status, saved) = http_body(&app, "PUT", url, json!({"enabled":true,"fromDate":left["fromDate"],"timeZone":left["timeZone"],"revision":left["revision"]})).await;
        assert_eq!(status, StatusCode::OK, "virtual revision 1 must be usable by explicit initializing PUT: {saved}");
        assert_eq!(saved["revision"], 1, "initializing identical defaults needs no extra settings revision");
        assert_eq!(sqlx::query_scalar::<_, i64>("select count(*) from study_actual_continuous").fetch_one(&pool).await.unwrap(), 1);
        assert_eq!(sqlx::query_scalar::<_, i64>("select count(*) from study_actualizations").fetch_one(&pool).await.unwrap(), 1);
        assert_eq!(sqlx::query_scalar::<_, Value>("select to_jsonb(a) from study_actualizations a where study_id=$1").bind(id).fetch_one(&pool).await.unwrap(), terminal);
        assert_eq!(market.calls.load(Ordering::SeqCst), 0);
        market.stop.cancel();server.abort();let _ = server.await;
    }).await;
}

#[tokio::test]
#[ignore = "requires isolated local TEST_DATABASE_URL ending _test with CREATEDB"]
async fn identical_continuous_put_preserves_queued_auto_and_terminal_cursor() {
    isolated(|pool| async move {
        let start = chrono::Utc::now().timestamp().div_euclid(60) * 60 - 600;
        let id = seed(&pool, start).await;
        let (app, state, market, server) = fixture(&pool, start).await;
        assert_eq!(http(&app, "POST", &format!("/v1/optimization-studies/{id}/actual/refresh")).await.0, StatusCode::ACCEPTED);
        let source: Value = sqlx::query_scalar("select source from study_actualizations where study_id=$1").bind(id).fetch_one(&pool).await.unwrap();
        let prime = db::claim(&pool, 2).await.unwrap().unwrap();db::fail(&pool, &prime, "prime_source").await.unwrap();
        let url = "/v1/optimization-studies/actual/continuous?algorithmId=price_volume_spike";
        let (_, old) = http(&app, "GET", url).await;
        assert_eq!(http_body(&app, "PUT", url, json!({"enabled":true,"fromDate":"1970-01-01","timeZone":"UTC","revision":old["revision"]})).await.0, StatusCode::OK);
        let ticket = db::continuous::next(&pool).await.unwrap().unwrap();
        let accepted = db::continuous::admit(&pool, &ticket, db::Request {
            owner: "admin", study: id, job: "http-automatic-no-op", start: chrono::DateTime::from_timestamp(start, 0).unwrap(), cutoff: ticket.cutoff,
            source: &source, identity: source["sourceIdentity"].as_str().unwrap(), work: 1500, limits: &state.limits,
        }).await.unwrap();
        let db::Admission::Created(accepted) = accepted else {panic!("automatic admission rejected: {accepted:?}");};
        let (_, settings) = http(&app, "GET", url).await;
        let before: Value = sqlx::query_scalar("select jsonb_build_object('settings',(select to_jsonb(c)||jsonb_build_object('rowXmin',c.xmin::text) from study_actual_continuous c where owner_id='admin' and algorithm_id='price_volume_spike'),'head',(select to_jsonb(a)||jsonb_build_object('rowXmin',a.xmin::text) from study_actualizations a where study_id=$1))").bind(id).fetch_one(&pool).await.unwrap();
        let (status, saved) = http_body(&app, "PUT", url, json!({"enabled":settings["enabled"],"fromDate":settings["fromDate"],"timeZone":settings["timeZone"],"revision":settings["revision"]})).await;
        assert_eq!(status, StatusCode::OK);assert_eq!(saved, settings);
        let after: Value = sqlx::query_scalar("select jsonb_build_object('settings',(select to_jsonb(c)||jsonb_build_object('rowXmin',c.xmin::text) from study_actual_continuous c where owner_id='admin' and algorithm_id='price_volume_spike'),'head',(select to_jsonb(a)||jsonb_build_object('rowXmin',a.xmin::text) from study_actualizations a where study_id=$1))").bind(id).fetch_one(&pool).await.unwrap();
        assert_eq!(after, before, "identical PUT cannot rewrite settings or the accepted automatic head");
        let job = db::claim(&pool, 2).await.unwrap().unwrap();
        assert_eq!(job.job_id, "http-automatic-no-op");assert_eq!(job.revision, accepted["revision"].as_i64().unwrap());assert_eq!(job.cutoff, ticket.cutoff);
        assert!(db::continuous::state(&pool, "admin", "price_volume_spike").await.unwrap()["cursorStudyId"].is_null());
        db::fail(&pool, &job, "fixture_terminal").await.unwrap();
        assert_eq!(db::continuous::state(&pool, "admin", "price_volume_spike").await.unwrap()["cursorStudyId"], id);
        assert_eq!(market.calls.load(Ordering::SeqCst), 0);
        market.stop.cancel();server.abort();let _ = server.await;
    }).await;
}

#[tokio::test]
#[ignore = "requires isolated local TEST_DATABASE_URL ending _test with CREATEDB"]
async fn headless_default_automatic_runtime_skips_invalid_source_and_reports_canonical_origin() {
    isolated(|pool|async move {
    let start=chrono::Utc::now().timestamp().div_euclid(60)*60-600;let invalid=seed(&pool,start).await;let valid=seed(&pool,start).await;
    sqlx::query("update optimization_studies set result=jsonb_set(result,'{best,params}','{}') where id=$1").bind(invalid).execute(&pool).await.unwrap();
    let (app,state,market,server)=fixture(&pool,start).await;
    // No GET/PUT/browser required; runtime initializes default-on itself.
    // Keep the fixture start in today's local date even across UTC midnight.
    let zone=if chrono::Utc::now().timestamp().rem_euclid(86400)<600 {"Etc/GMT+12"}else{"UTC"};
    setups_db::settings_set(&pool,"ui",&json!({"interface":{"timezone":zone}})).await.unwrap();
    sqlx::query("delete from study_actual_continuous").execute(&pool).await.unwrap();
    let runtime=start_actual_runtime(state,2).await.unwrap();let completed=wait(&pool,valid,"completed").await;
    assert_eq!(completed["request_origin"],"automatic");assert!(db::get(&pool,"admin",invalid).await.unwrap().is_none(),"invalid source never admits or cleans an actual revision");
    let (_,detail)=http(&app,"GET",&format!("/v1/optimization-studies/{valid}/actual")).await;canonical(&detail,valid);assert_eq!(detail["requestOrigin"],"automatic");
    let (_,history)=http(&app,"GET","/v1/optimization-studies?limit=100").await;
    assert_eq!(history["studies"].as_array().unwrap().iter().find(|s|s["id"]==valid).unwrap()["actual"]["requestOrigin"],"automatic");
    runtime.stop().await;market.stop.cancel();server.abort();let _=server.await;
}).await;
}

#[tokio::test]
#[ignore = "requires isolated local TEST_DATABASE_URL ending _test with CREATEDB"]
async fn automatic_per_candidate_work_limit_skips_only_a_and_runs_b() {
    isolated(|pool| async move {
        let start = chrono::Utc::now().timestamp().div_euclid(60) * 60 - 600;
        let a = seed(&pool, start - 86400).await;
        let b = seed(&pool, start).await;
        let (_app, state, market, server) = fixture(&pool, start).await;
        let config = db::continuous::ensure(&pool, "admin", "price_volume_spike")
            .await
            .unwrap();
        db::continuous::configure(
            &pool,
            "admin",
            "price_volume_spike",
            config.revision,
            true,
            "1970-01-01",
            "UTC",
        )
        .await
        .unwrap();
        let limited = ReplayState {
            pool: pool.clone(),
            auth: state.auth.clone(),
            market_url: state.market_url.clone(),
            market_key: state.market_key.clone(),
            limits: setups_db::admission::ReplayAdmissionLimits {
                max_work_minutes: 2000,
                ..state.limits
            },
        };
        let runtime = start_actual_runtime(Arc::new(limited), 2).await.unwrap();
        let completed = wait(&pool, b, "completed").await;
        assert_eq!(completed["request_origin"], "automatic");
        assert!(
            db::get(&pool, "admin", a).await.unwrap().is_none(),
            "over-budget A cannot skip uninspected fitting B"
        );
        runtime.stop().await;
        market.stop.cancel();
        server.abort();
        let _ = server.await;
    })
    .await;
}

#[tokio::test]
#[ignore = "requires isolated local TEST_DATABASE_URL ending _test with CREATEDB"]
async fn delayed_auto_source_preparation_does_not_block_manual_claim_or_shutdown() {
    isolated(|pool|async move {
    let start=chrono::Utc::now().timestamp().div_euclid(60)*60-600;let a=seed(&pool,start).await;let b=seed(&pool,start).await;
    let (app,state,market,server)=fixture(&pool,start).await;
    assert_eq!(http(&app,"POST",&format!("/v1/optimization-studies/{a}/actual/refresh")).await.0,StatusCode::ACCEPTED);
    let source:Value=sqlx::query_scalar("select source from study_actualizations where study_id=$1").bind(a).fetch_one(&pool).await.unwrap();
    let job=db::claim(&pool,2).await.unwrap().unwrap();db::fail(&pool,&job,"prime_source").await.unwrap();
    let config=db::continuous::ensure(&pool,"admin","price_volume_spike").await.unwrap();db::continuous::configure(&pool,"admin","price_volume_spike",config.revision,true,"1970-01-01","UTC").await.unwrap();
    // Delay ONLY source SELECT, not foreign-key table locks used by the worker.
    // This disposable view/function is never part of production instrumentation.
    sqlx::query("insert into study_expectancy_artifacts(artifact_key,owner_id,source_generation_id,source_study_id,source_slot_id,calculation_version,tp_pct,sl_pct,identity,numerical_snapshot) values(repeat('a',64),'admin',999,$1,1,'fixture',10,10,'{}','{}')").bind(a).execute(&pool).await.unwrap();
    sqlx::raw_sql("alter table study_expectancy_artifacts rename to fixture_actual_artifacts;create function fixture_source_barrier() returns boolean language plpgsql volatile as $$begin perform pg_advisory_xact_lock(73100042);return true;end$$;create view study_expectancy_artifacts as select a.* from fixture_actual_artifacts a where fixture_source_barrier();").execute(&pool).await.unwrap();
    let mut held=pool.begin().await.unwrap();sqlx::query("select pg_advisory_xact_lock(73100042)").execute(&mut *held).await.unwrap();
    market.hold.store(true,Ordering::SeqCst);let runtime=start_actual_runtime(state.clone(),2).await.unwrap();
    tokio::time::timeout(Duration::from_secs(4),async {loop {
        let waiting:bool=sqlx::query_scalar("select exists(select 1 from pg_stat_activity where datname=current_database() and wait_event_type='Lock' and query like 'select id,numerical_snapshot%')").fetch_one(&pool).await.unwrap();
        if waiting {break;}tokio::time::sleep(Duration::from_millis(25)).await;
    }}).await.expect("automatic source query did not enter the delayed boundary");
    let pid:i32=sqlx::query_scalar("select pid from pg_stat_activity where datname=current_database() and application_name='study_actual_runtime'").fetch_one(&pool).await.unwrap();
    let mut replacement=pool.acquire().await.unwrap();sqlx::query_scalar::<_,bool>("select pg_terminate_backend($1)").bind(pid).fetch_one(&mut *replacement).await.unwrap();
    sqlx::query("select pg_advisory_lock($1)").bind(db::RUNTIME_LOCK).execute(&mut *replacement).await.unwrap();
    let identity:String=sqlx::query_scalar("select md5(spec::text||coalesce(result::text,'null')) from optimization_studies where id=$1").bind(b).fetch_one(&pool).await.unwrap();
    let limits=Default::default();assert!(matches!(db::admit(&pool,db::Request{owner:"admin",study:b,job:"manual-during-source",start:chrono::DateTime::from_timestamp(start,0).unwrap(),cutoff:start+600,source:&source,identity:&identity,work:1500,limits:&limits}).await.unwrap(),db::Admission::Created(_)));
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert_eq!(db::get(&pool,"admin",b).await.unwrap().unwrap()["status"],"queued","delayed source cannot bypass lost runtime ownership");
    sqlx::query("select pg_advisory_unlock($1)").bind(db::RUNTIME_LOCK).execute(&mut *replacement).await.unwrap();drop(replacement);
    let begin=std::time::Instant::now();wait(&pool,b,"running").await;assert!(begin.elapsed()<Duration::from_secs(2),"source preparation blocked manual claims");
    tokio::time::timeout(Duration::from_secs(2),runtime.stop()).await.expect("source preparation blocked shutdown cancellation");
    assert_eq!(db::get(&pool,"admin",b).await.unwrap().unwrap()["status"],"failed");
    held.rollback().await.unwrap();market.stop.cancel();server.abort();let _=server.await;
}).await;
}

#[tokio::test]
#[ignore = "requires isolated local TEST_DATABASE_URL ending _test with CREATEDB"]
async fn legacy_event_only_sources_keep_exact_fraction_offsets_and_first_valid_start() {
    isolated(|pool| async move {
        let start = chrono::Utc::now().timestamp().div_euclid(60) * 60 - 600;
        let id = seed(&pool, start).await;
        let (app, _, market, server) = fixture(&pool, start).await;
        for nanos in [1, 999_999_999] {
            let exact = chrono::DateTime::from_timestamp(start + 59, nanos).unwrap();
            let timestamp = exact
                .with_timezone(&chrono::FixedOffset::east_opt(5 * 3600).unwrap())
                .to_rfc3339();
            sqlx::query(
                "update optimization_studies set execution_started_at=null,events=$2 where id=$1",
            )
            .bind(id)
            .bind(json!([{"kind":"started","at":"malformed"},{"kind":"started","at":timestamp}]))
            .execute(&pool)
            .await
            .unwrap();
            let (status, queued) = http(
                &app,
                "POST",
                &format!("/v1/optimization-studies/{id}/actual/refresh"),
            )
            .await;
            assert_eq!(status, StatusCode::ACCEPTED, "{queued}");
            let source: Value =
                sqlx::query_scalar("select source from study_actualizations where study_id=$1")
                    .bind(id)
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            assert_eq!(
                source["executionStartedAt"],
                json!(exact),
                "manual source must not round across a minute or discard offset/nanos"
            );
            let job = db::claim(&pool, 2).await.unwrap().unwrap();
            db::fail(&pool, &job, "fixture").await.unwrap();
        }
        market.stop.cancel();
        server.abort();
        let _ = server.await;
    })
    .await;
}
#[test]
fn materialize_command_help_and_invalid_arguments_do_not_start_server_or_require_environment() {
    let executable = env!("CARGO_BIN_EXE_setups-platform-server");
    let help = std::process::Command::new(executable)
        .args(["materialize-study-original", "--help"])
        .env_clear()
        .output()
        .unwrap();
    assert!(help.status.success());
    let text = String::from_utf8(help.stdout).unwrap();
    assert!(text.contains("Default: bounded read-only calculation"));
    assert!(text.contains("[--publish]"));
    let invalid = std::process::Command::new(executable)
        .args(["materialize-study-original", "--study-id", "-1"])
        .env_clear()
        .output()
        .unwrap();
    assert_eq!(invalid.status.code(), Some(2));
    assert!(
        String::from_utf8(invalid.stderr)
            .unwrap()
            .contains("verification is required")
    );
}
fn canonical(value: &Value, id: i64) {
    assert_eq!(value["studyId"], id);
    assert!(value["eligibility"]["eligible"].is_boolean());
    assert!(value["eligibility"]["reason"].is_null() || value["eligibility"]["reason"].is_string());
    for key in [
        "jobId",
        "revision",
        "status",
        "stage",
        "observedThroughUnix",
        "counts",
        "meanR",
        "lastSetupAt",
        "error",
    ] {
        assert!(
            value.get(key).is_some(),
            "missing TS summary field {key}: {value}"
        );
    }
    assert!(value["revision"].as_u64().is_some());
    if value["status"] == "not_calculated" {
        assert!(value["stage"].is_null());
    } else {
        assert!(value["stage"].is_string());
    }
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
            assert!(
                value["counts"][key].as_u64().is_some(),
                "invalid TS count {key}"
            );
        }
    }
}
async fn wait(pool: &PgPool, id: i64, status: &str) -> Value {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if let Some(row) = db::get(pool, "admin", id).await.unwrap() {
                if row["status"] == status {
                    return row;
                }
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("job did not reach expected state")
}

#[tokio::test]
#[ignore = "requires isolated local TEST_DATABASE_URL ending _test with CREATEDB"]
async fn real_history_http_is_canonical_before_first_refresh_queued_and_completed() {
    isolated(|pool| async move {
        let start = chrono::Utc::now().timestamp().div_euclid(60) * 60 - 600;
        let id = seed(&pool, start).await;
        let (app, state, market, server) = fixture(&pool, start).await;
        let original: String = sqlx::query_scalar(
            "select md5(to_jsonb(s)::text) from optimization_studies s where id=$1",
        )
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
        let history = format!("/v1/optimization-studies?limit=100");
        let actual = format!("/v1/optimization-studies/{id}/actual");
        let (status, before) = http(&app, "GET", &history).await;
        assert_eq!(status, StatusCode::OK);
        let row = &before["studies"][0]["actual"];
        canonical(row, id);
        assert_eq!(row["status"], "not_calculated");
        assert_eq!(
            row["eligibility"]["eligible"], true,
            "first refresh must be enabled without a chart/head"
        );
        let (status, queued) = http(&app, "POST", &format!("{actual}/refresh")).await;
        assert_eq!(status, StatusCode::ACCEPTED, "{queued}");
        canonical(&queued, id);
        let (_, pending) = http(&app, "GET", &history).await;
        assert_eq!(
            pending["studies"][0]["actual"],
            serde_json::to_value(db::compact_summary(
                &db::get(&pool, "admin", id).await.unwrap().unwrap()
            ))
            .unwrap()
        );
        canonical(&pending["studies"][0]["actual"], id);
        let runtime = start_actual_runtime(state, 2).await.unwrap();
        let completed = wait(&pool, id, "completed").await;
        let (status, detail) = http(&app, "GET", &actual).await;
        assert_eq!(status, StatusCode::OK);
        canonical(&detail, id);
        assert_eq!(
            detail["report"]["historical"]["availability"]["status"],
            "unavailable"
        );
        assert!(
            detail["counts"]["total"].as_u64().unwrap() > 0,
            "fixture must exercise emitted setup evaluation: {detail}"
        );
        let (_, history) = http(&app, "GET", &history).await;
        canonical(&history["studies"][0]["actual"], id);
        for key in [
            "studyId",
            "eligibility",
            "jobId",
            "revision",
            "status",
            "stage",
            "counts",
            "meanR",
            "observedThroughUnix",
            "error",
        ] {
            assert_eq!(
                history["studies"][0]["actual"][key], detail[key],
                "history/detail mismatch {key}"
            );
        }
        assert_eq!(completed["reserved_bytes"], 0);
        let (status, page) =
            http(&app, "GET", &format!("{actual}/setups?revision=1&limit=50")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(page["total"], detail["counts"]["total"]);
        assert_eq!(page["setups"][0]["slotId"], Value::Null);
        assert_eq!(
            page["setups"][0]["entryTimeUnix"].as_i64().unwrap()
                - page["setups"][0]["setupTimeUnix"].as_i64().unwrap(),
            60,
            "persisted output must use raw close + saved one-minute shift"
        );
        assert_eq!(
            sqlx::query_scalar::<_, String>(
                "select md5(to_jsonb(s)::text) from optimization_studies s where id=$1"
            )
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap(),
            original,
            "original science must remain byte-stable"
        );
        for table in [
            "replay_cache",
            "live_setups",
            "adaptive_notification_outbox",
            "telegram_delivery_outbox",
        ] {
            assert_eq!(
                sqlx::query_scalar::<_, i64>(&format!("select count(*) from {table}"))
                    .fetch_one(&pool)
                    .await
                    .unwrap(),
                0,
                "retrospective job must not publish into {table}"
            );
        }
        runtime.stop().await;
        market.stop.cancel();
        server.abort();
        let _ = server.await;
    })
    .await;
}

fn unix(text: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(text)
        .unwrap()
        .timestamp()
}

async fn original_ledger_fixture(
    pool: &PgPool,
    start: i64,
    points: Vec<setups_db::study_display::OriginalMinute>,
) -> (i64, i64, Value) {
    use setups_domain::{RecommendationDirection, RecommendationProvenance, expectancy::*};
    let id = seed(pool, start).await;
    let tp: u64 = points.iter().map(|p| p.tp_count).sum();
    let sl: u64 = points.iter().map(|p| p.sl_count).sum();
    let count = tp + sl;
    let mean = (tp as f64 - sl as f64) / count as f64;
    let mut result: Value =
        sqlx::query_scalar("select result from optimization_studies where id=$1")
            .bind(id)
            .fetch_one(pool)
            .await
            .unwrap();
    result["best"]["provenance"]["snapshotHash"] = json!("f".repeat(64));
    result["best"]["score"] = json!(mean);
    let cell = &mut result["best"]["matrix"][0];
    cell["riskExpectancy"] = json!(mean);
    cell["closedCount"] = json!(count);
    cell["tpCount"] = json!(tp);
    cell["slCount"] = json!(sl);
    sqlx::query("update optimization_studies set result=$2 where id=$1")
        .bind(id)
        .bind(result)
        .execute(pool)
        .await
        .unwrap();
    let mut utc = std::collections::BTreeMap::<i64, (u64, u64)>::new();
    for p in &points {
        let d = utc
            .entry(p.signal_close_unix.div_euclid(86400) * 86400)
            .or_default();
        d.0 += p.tp_count;
        d.1 += p.sl_count;
    }
    let key = format!("{id:064x}");
    let artifact = build_expectancy_artifact(ExpectancyInput {
        source: ExpectancySource {
            study_id: id.to_string(),
            derived_snapshot_id: key.clone(),
            generation_id: Some("777".into()),
            profile_id: Some("1".into()),
            provenance: RecommendationProvenance {
                algorithm_id: "price_volume_spike".into(),
                algorithm_version: "1.1.1".into(),
                optimizer_version: "plan-a-3".into(),
                seed: 7,
                snapshot_hash: "f".repeat(64),
                captured_at: "2026-11-20T00:00:00Z".into(),
                from_unix: start - 7 * 86400,
                to_unix: start - 60,
                observation_cutoff: start - 60,
            },
            direction: RecommendationDirection::Short,
            tp_pct: 10.0,
            sl_pct: 10.0,
            forecast: None,
            analytics: ExpectancyAnalytics {
                cluster_enabled: false,
                cluster_gap_minutes: None,
                time_shift_minutes: 1,
                price_change_pct: None,
                horizon_minutes: 10080,
            },
        },
        selected_cell: Some(SelectedCellInput {
            raw_r: Some(mean),
            tp_count: tp,
            sl_count: sl,
        }),
        matrix_mean_r: Some(mean),
        daily: MetricAvailability::Available {
            value: utc
                .into_iter()
                .map(|(day, (tp, sl))| UtcDailyInput {
                    day_start_unix: day,
                    row_count: tp + sl,
                    tp_count: tp,
                    sl_count: sl,
                    mean_r: Some((tp as f64 - sl as f64) / (tp + sl) as f64),
                })
                .collect(),
        },
    })
    .unwrap();
    setups_db::study_display::validate_minutes(&artifact, &points).unwrap();
    let artifact_id:i64=sqlx::query_scalar("insert into study_expectancy_artifacts(artifact_key,owner_id,source_generation_id,source_study_id,source_slot_id,calculation_version,tp_pct,sl_pct,identity,numerical_snapshot) values($1,'admin',777,$2,1,'2',10,10,'{}',$3) returning id")
        .bind(&key).bind(id).bind(json!(artifact)).fetch_one(pool).await.unwrap();
    use sha2::{Digest, Sha256};
    let hash = format!("{:x}", Sha256::digest(serde_json::to_vec(&points).unwrap()));
    sqlx::query("insert into study_original_outcomes(artifact_id,source_key,kernel,minutes,payload_sha256,payload_bytes) values($1,$2,'original-historical-close-1',$3,$4,octet_length($3::jsonb::text))")
        .bind(artifact_id).bind(key).bind(json!(points)).bind(hash).execute(pool).await.unwrap();
    let spec: Value = sqlx::query_scalar("select spec from optimization_studies where id=$1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap();
    (id, artifact_id, spec)
}

#[tokio::test]
#[ignore = "requires isolated local TEST_DATABASE_URL ending _test with CREATEDB"]
async fn exact_display_original_only_weighted_merge_and_refresh_preservation_wire() {
    isolated(|pool|async move {
    use setups_db::study_display::OriginalMinute;
    let start=unix("2026-11-20T23:30:00Z");
    let points=vec![OriginalMinute{signal_close_unix:unix("2026-11-20T19:00:00Z"),sum_r:2.0,closed_count:2,tp_count:2,sl_count:0},
        OriginalMinute{signal_close_unix:unix("2026-11-20T23:00:00Z"),sum_r:0.0,closed_count:2,tp_count:1,sl_count:1}];
    let (id,artifact,spec)=original_ledger_fixture(&pool,start,points).await;
    sqlx::query("update optimization_studies set execution_started_at=to_timestamp($2) where id=$1").bind(id).bind(start as f64+0.25).execute(&pool).await.unwrap();
    let immutable:(String,String)=sqlx::query_as("select s.result::text,a.numerical_snapshot::text from optimization_studies s join study_expectancy_artifacts a on a.source_study_id=s.id where s.id=$1").bind(id).fetch_one(&pool).await.unwrap();
    let original_archive:Value=sqlx::query_scalar("select to_jsonb(o) from study_original_outcomes o where artifact_id=$1").bind(artifact).fetch_one(&pool).await.unwrap();
    let (app,_,market,server)=fixture(&pool,start).await;let path=format!("/v1/optimization-studies/{id}/actual");let zoned=format!("{path}?timeZone=Asia%2FKathmandu");
    let (_,legacy)=http(&app,"GET",&path).await;assert!(legacy.get("originalReport").is_none());assert!(legacy["report"].is_null());
    let (status,before)=http(&app,"GET",&zoned).await;assert_eq!(status,StatusCode::OK,"{before}");assert_eq!(before["status"],"not_calculated");assert!(before["report"].is_null());
    assert!(before.get("lastSetupAt").is_some_and(Value::is_null));
    let original=&before["originalReport"];let display=&original["display"];
    assert_eq!(original["executionStartedAt"],"2026-11-20T23:30:00.250Z");assert_eq!(display["timeZone"],"Asia/Kathmandu");
    assert_eq!(display["startDayUnix"],unix("2026-11-20T18:15:00Z"));assert_eq!(display["daily"].as_array().unwrap().len(),1);
    assert_eq!(display["daily"][0]["date"],"2026-11-21");assert_eq!(display["daily"][0]["closedCount"],4);assert_eq!(display["daily"][0]["sumR"],2.0);assert_eq!(display["daily"][0]["meanR"],0.5);
    assert_eq!(display["historical"]["availability"]["status"],"available");assert_eq!(display["historical"]["complete"],true);assert_eq!(display["historical"]["fidelity"],"native_original");assert_eq!(display["historical"]["rawReplayAvailable"],false);
    assert!(db::get(&pool,"admin",id).await.unwrap().is_none(),"GET must not admit replay");
    let identity:String=sqlx::query_scalar("select md5(spec::text||coalesce(result::text,'null')) from optimization_studies where id=$1").bind(id).fetch_one(&pool).await.unwrap();
    let source=json!({"algorithmId":"price_volume_spike","algorithmVersion":"1.1.1","params":spec["algorithmParams"],"startUnix":start as f64+0.25,"instruments":spec["instruments"],
        "tpPct":10.0,"slPct":10.0,"objective":original["originalObjective"]});
    let limits=setups_db::admission::ReplayAdmissionLimits::default();
    let admit=|job:&'static str|db::Request{owner:"admin",study:id,job,start:chrono::DateTime::from_timestamp(start,250_000_000).unwrap(),cutoff:start+7200,source:&source,identity:&identity,work:60,limits:&limits};
    assert!(matches!(db::admit(&pool,admit("display-first")).await.unwrap(),db::Admission::Created(_)));
    let job=db::claim(&pool,2).await.unwrap().unwrap();let batch=db::create_batch(&pool,&job).await.unwrap().unwrap();
    let rows=vec![json!({"setupTimeUnix":unix("2026-11-21T00:30:00Z"),"entryTimeUnix":unix("2026-11-21T00:31:00Z"),"status":"sl","riskReturn":-1.0})];
    assert!(db::append(&pool,&job,batch,db::InstrumentOutput{exchange:"fixture",category:"linear",symbol:"FIX",emitted:&json!([]),rows:&rows,quality:&json!({})}).await.unwrap());
    let mut report=original.clone();report.as_object_mut().unwrap().remove("display");report["observedThroughUnix"]=json!(start+7200);
    assert!(db::finish(&pool,&job,&report,&json!({"total":1,"closed":1,"tp":0,"sl":1,"gateFailed":0,"unclosed":0,"noData":0,"meanR":-1.0})).await.unwrap());
    let (status,completed)=http(&app,"GET",&zoned).await;assert_eq!(status,StatusCode::OK,"{completed}");let merged=&completed["report"]["display"];
    assert_eq!(completed["lastSetupAt"],"2026-11-21T00:30:00Z");
    assert_eq!(merged["daily"].as_array().unwrap().len(),1);assert_eq!(merged["daily"][0]["closedCount"],5);assert_eq!(merged["daily"][0]["tpCount"],3);assert_eq!(merged["daily"][0]["slCount"],2);
    assert_eq!(merged["daily"][0]["sumR"],1.0);assert_eq!(merged["daily"][0]["meanR"],0.2);assert_eq!(completed["originalReport"],*original);
    let combined=&completed["report"]["combinedProjection"];
    assert_eq!(combined["availability"]["status"],"available","{combined}");assert_eq!(combined["method"],"mean");
    assert_eq!(combined["revision"],completed["revision"]);assert_eq!(combined["jobId"],completed["jobId"]);
    assert_eq!(combined["observationCutoff"],start+7200);assert_eq!(combined["fittedOnPostStudyData"],true);
    assert!(combined["points"].as_array().unwrap().iter().all(|p|p["value"].as_f64()==Some(0.2)),"all-data mean is horizontal; never average the two period means");
    for key in ["originalObjective","originalEndpoint","projection","executionStartedAt"] {assert_eq!(completed["report"][key],report[key],"original model field {key} changed");}
    let saved_source:Value=sqlx::query_scalar("select source from study_actualizations where owner_id='admin' and study_id=$1").bind(id).fetch_one(&pool).await.unwrap();
    sqlx::query("update study_actualizations set source=jsonb_set(jsonb_set(source,'{tpPct}','20'::jsonb),'{slPct}','20'::jsonb) where owner_id='admin' and study_id=$1")
        .bind(id).execute(&pool).await.unwrap();
    let mismatched_before:Value=sqlx::query_scalar("select to_jsonb(a) from study_actualizations a where owner_id='admin' and study_id=$1").bind(id).fetch_one(&pool).await.unwrap();
    let (status,mismatched)=http(&app,"GET",&zoned).await;assert_eq!(status,StatusCode::OK,"{mismatched}");
    assert_eq!(mismatched["report"]["display"]["actual"]["availability"]["status"],"unavailable");
    assert_eq!(mismatched["report"]["display"]["actual"]["reason"],"post_study_pair_mismatch");
    assert_eq!(mismatched["report"]["display"]["daily"],original["display"]["daily"],"different TP/SL outcomes must not enter the observed all-data line");
    assert_eq!(mismatched["report"]["combinedProjection"]["availability"]["reason"],"post_study_pair_mismatch");
    assert_eq!(mismatched["originalReport"],*original);
    assert_eq!(sqlx::query_scalar::<_,Value>("select to_jsonb(a) from study_actualizations a where owner_id='admin' and study_id=$1").bind(id).fetch_one(&pool).await.unwrap(),mismatched_before,"mismatch GET must remain read-only");
    sqlx::query("update study_actualizations set source=$2 where owner_id='admin' and study_id=$1").bind(id).bind(saved_source).execute(&pool).await.unwrap();
    let (_,old_completed)=http(&app,"GET",&path).await;assert_eq!(old_completed["report"],report,"without timeZone saved wire report stays untouched");assert!(old_completed.get("originalReport").is_none());
    assert_eq!(old_completed["lastSetupAt"],completed["lastSetupAt"]);
    assert!(matches!(db::admit(&pool,admit("display-replacement")).await.unwrap(),db::Admission::Created(_)));
    let (_,queued)=http(&app,"GET",&zoned).await;assert_eq!(queued["status"],"queued");assert!(queued["report"].is_null());assert_eq!(queued["originalReport"],*original);
    assert!(queued["lastSetupAt"].is_null());
    let replacement=db::claim(&pool,2).await.unwrap().unwrap();let (_,running)=http(&app,"GET",&zoned).await;assert_eq!(running["status"],"running");assert_eq!(running["originalReport"],*original);
    db::fail(&pool,&replacement,"fixture_failure").await.unwrap();let (_,failed)=http(&app,"GET",&zoned).await;assert_eq!(failed["status"],"failed");assert!(failed["report"].is_null());assert_eq!(failed["originalReport"],*original);
    assert!(failed["lastSetupAt"].is_null());
    assert_eq!(sqlx::query_scalar::<_,i64>("select count(*) from study_original_outcomes where artifact_id=$1").bind(artifact).fetch_one(&pool).await.unwrap(),1);
    assert_eq!(sqlx::query_scalar::<_,Value>("select to_jsonb(o) from study_original_outcomes o where artifact_id=$1").bind(artifact).fetch_one(&pool).await.unwrap(),original_archive,"refresh must preserve the sealed ledger/provenance/timestamps byte-for-byte");
    assert_eq!(sqlx::query_as::<_,(String,String)>("select s.result::text,a.numerical_snapshot::text from optimization_studies s join study_expectancy_artifacts a on a.source_study_id=s.id where s.id=$1").bind(id).fetch_one(&pool).await.unwrap(),immutable);
    assert_eq!(market.calls.load(Ordering::SeqCst),0,"GET must never acquire market history");
    if let Ok(path)=std::env::var("STUDY_DISPLAY_WIRE_PATH") {std::fs::write(path,serde_json::to_vec_pretty(&json!({"before":before,"completed":completed,"queued":queued,"running":running,"failed":failed})).unwrap()).unwrap();}
    market.stop.cancel();server.abort();let _=server.await;
}).await;
}

#[path = "fixtures/combined_projection.rs"]
mod combined_golden;

async fn seed_combined_golden(pool: &PgPool, g: &combined_golden::Golden) -> (i64, i64, Value, String) {
    let id = seed(pool, g.split).await;
    let mut spec = serde_json::to_value(&g.spec).unwrap();
    spec["toUnix"] = json!(g.split);
    let mut result: Value = sqlx::query_scalar("select result from optimization_studies where id=$1").bind(id).fetch_one(pool).await.unwrap();
    let metrics = g.original.selected_cell.as_ref().unwrap();
    let series = g.original.objective_series(g.objective).unwrap();
    let best = &mut result["best"];
    best["params"] = spec["algorithmParams"].clone();
    best["analytics"] = json!({"clusterEnabled":false,"clusterGapMinutes":null,"timeShiftMinutes":1,"priceChangePct":1.0,"setupMaturity":0.0,"horizonMinutes":3});
    best["provenance"]["snapshotHash"] = json!(g.original.source.provenance.snapshot_hash);
    best["observationCutoff"] = json!(g.split);
    best["matrix"][0] = json!({"tpPct":10.0,"slPct":5.0,"riskExpectancy":series.decision_r.as_ref(),
        "meanRiskExpectancy":metrics.raw_r.as_ref(),"forecastRiskExpectancy":series.forecast_r.as_ref(),
        "avgMovePct":metrics.raw_r.as_ref().map(|r|r*5.0),"closedCount":metrics.closed_count,"tpCount":metrics.tp_count,"slCount":metrics.sl_count,
        "unclosedCount":0,"noDataCount":0,"priceChangeFilteredCount":0});
    sqlx::query("update optimization_studies set spec=$2,result=$3 where id=$1").bind(id).bind(&spec).bind(result).execute(pool).await.unwrap();
    let mut original = g.original.clone();
    original.source.study_id = id.to_string(); original.source.derived_snapshot_id = format!("{id:064x}");
    original.validate().unwrap();
    let artifact: i64 = sqlx::query_scalar("insert into study_expectancy_artifacts(artifact_key,owner_id,source_generation_id,source_study_id,source_slot_id,calculation_version,tp_pct,sl_pct,identity,numerical_snapshot) values($1,'admin',777,$2,1,$3,10,5,'{}',$4) returning id")
        .bind(&original.source.derived_snapshot_id).bind(id).bind(original.calculation_version.to_string()).bind(json!(original)).fetch_one(pool).await.unwrap();
    let identity: String = sqlx::query_scalar("select md5(spec::text||coalesce(result::text,'null')) from optimization_studies where id=$1").bind(id).fetch_one(pool).await.unwrap();
    let source = json!({"algorithmId":"price_volume_spike","algorithmVersion":"1.1.1","params":spec["algorithmParams"],
        "startUnix":g.split,"instruments":spec["instruments"],"tpPct":10.0,"slPct":5.0,"objective":g.objective});
    (id, artifact, source, identity)
}

async fn publish_combined_fixture(pool: &PgPool, id: i64, g: &combined_golden::Golden, source: &Value, identity: &str, job: &str, rows: &[Value], report: &Value) {
    let limits = setups_db::admission::ReplayAdmissionLimits::default();
    assert!(matches!(db::admit(pool, db::Request { owner:"admin", study:id, job, start:chrono::DateTime::from_timestamp(g.split,0).unwrap(),
        cutoff:g.cutoff, source, identity, work:60, limits:&limits }).await.unwrap(), db::Admission::Created(_)));
    let job = db::claim(pool,2).await.unwrap().unwrap(); let batch = db::create_batch(pool,&job).await.unwrap().unwrap();
    assert!(db::append(pool,&job,batch,db::InstrumentOutput { exchange:"fixture",category:"linear",symbol:"FIX",
        emitted:&json!([]),rows,quality:&json!({}) }).await.unwrap());
    let count = |status| rows.iter().filter(|r|r["status"]==status).count();
    let tp = count("tp"); let sl = count("sl");
    assert!(db::finish(pool,&job,report,&json!({"total":rows.len(),"closed":tp+sl,"tp":tp,"sl":sl,
        "gateFailed":count("gate_failed"),"unclosed":count("unclosed"),"noData":count("no_data"),
        "meanR":(tp+sl>0).then(||(2.0*tp as f64-sl as f64)/(tp+sl) as f64)})).await.unwrap());
}

#[tokio::test]
#[ignore = "requires isolated local TEST_DATABASE_URL ending _test with CREATEDB"]
async fn combined_display_canonical_golden_and_repeatable_revision_snapshot() {
    isolated(|pool| async move {
        use setups_domain::{forecast::ForecastRecipe, trend::{TrendMode, TrendBendDays}};
        for (recipe_index, recipe) in [None, Some(ForecastRecipe::engineering_baseline()),
            Some(ForecastRecipe::Trend { version:1,bend_days:TrendBendDays::Linear }),
            Some(ForecastRecipe::Trend { version:1,bend_days:TrendBendDays::Seven })].into_iter().enumerate() {
            for mode in [TrendMode::Mean, TrendMode::Forecast, TrendMode::Blend] {
                let g = combined_golden::golden(mode, recipe);
                let (id, artifact, source, identity) = seed_combined_golden(&pool,&g).await;
                let (app,state,market,server) = fixture(&pool,g.split).await;
                let path = format!("/v1/optimization-studies/{id}/actual?timeZone=UTC");
                let (status,before) = http(&app,"GET",&path).await; assert_eq!(status,StatusCode::OK,"{before}");
                assert!(before["originalReport"].is_object(),"{before}");
                let mut report = before["originalReport"].clone(); report["observedThroughUnix"] = json!(g.cutoff);
                let job_id = format!("golden-{recipe_index}-{}", match mode {
                    TrendMode::Mean => "mean",
                    TrendMode::Forecast => "forecast",
                    TrendMode::Blend => "blend",
                });
                publish_combined_fixture(&pool,id,&g,&source,&identity,&job_id,&g.actual_rows,&report).await;
                let immutable: Value = sqlx::query_scalar("select jsonb_build_object('study',to_jsonb(s),'artifact',to_jsonb(a),'actual',to_jsonb(h)) from optimization_studies s join study_expectancy_artifacts a on a.source_study_id=s.id join study_actualizations h on h.study_id=s.id where s.id=$1").bind(id).fetch_one(&pool).await.unwrap();
                let (status,completed) = http(&app,"GET",&path).await; assert_eq!(status,StatusCode::OK,"{completed}");
                let model = &completed["report"]["combinedProjection"];
                assert_eq!(model["availability"]["status"],"available","{recipe:?}/{mode:?}: {model}");
                assert_eq!(model["revision"],completed["revision"]); assert_eq!(model["jobId"],completed["jobId"]);
                assert_eq!(model["observationCutoff"],g.cutoff);
                let expected = g.one_shot.objective_series(g.objective).unwrap();
                assert_eq!(model["method"],expected.method);
                let points = model["points"].as_array().unwrap(); assert_eq!(points.len(),expected.curve.len());
                for (p,(at,value)) in points.iter().zip(&expected.curve) {
                    assert_eq!(p["atUnix"].as_f64(),Some(*at));
                    match (p["value"].as_f64(),value) { (Some(a),Some(b))=>assert!((a-b).abs()<1e-10), (None,None)=>{}, _=>panic!("different availability") }
                }
                assert!((points.last().unwrap()["value"].as_f64().unwrap()-g.canonical_decision).abs()<1e-10);
                assert_eq!(completed["originalReport"],before["originalReport"]);
                let snapshot = setups_db::study_display::display_snapshot(&pool,"admin",id,"UTC",
                    Some(chrono::DateTime::from_timestamp(g.split,0).unwrap()),Some(artifact),Some(&identity)).await.unwrap();
                assert_eq!(snapshot.actual_utc_daily,g.actual_daily,"SQL scientific population must match canonical capture, including gate-only/null/split days");
                let after: Value = sqlx::query_scalar("select jsonb_build_object('study',to_jsonb(s),'artifact',to_jsonb(a),'actual',to_jsonb(h)) from optimization_studies s join study_expectancy_artifacts a on a.source_study_id=s.id join study_actualizations h on h.study_id=s.id where s.id=$1").bind(id).fetch_one(&pool).await.unwrap();
                assert_eq!(after,immutable,"GET must not persist/refit/publicize anything");

                if recipe.is_none() && mode == TrendMode::Forecast {
                    // Pause the reader AFTER it has read actual head N, but BEFORE
                    // it reads cohorts. Replace and publish N+1 on another session.
                    // The barrier exists only in this disposable database.
                    sqlx::raw_sql("alter function study_local_day_start(text,date) rename to fixture_day_start; create function study_local_day_start(zone text,local_day date) returns bigint language plpgsql volatile as $$begin if current_setting('application_name')='combined_race_reader' then perform pg_advisory_xact_lock(73100043);end if;return fixture_day_start(zone,local_day);end$$;").execute(&pool).await.unwrap();
                    let reader = PgPoolOptions::new().max_connections(2).connect_with((*pool.connect_options()).clone()
                        .application_name("combined_race_reader")).await.unwrap();
                    let reader_app = study_actual_router(Arc::new(ReplayState { pool:reader.clone(),auth:state.auth.clone(),
                        market_url:state.market_url.clone(),market_key:state.market_key.clone(),limits:state.limits }));
                    let mut held = pool.begin().await.unwrap(); sqlx::query("select pg_advisory_xact_lock(73100043)").execute(&mut *held).await.unwrap();
                    let read_path = path.clone(); let pending = tokio::spawn(async move {http(&reader_app,"GET",&read_path).await});
                    tokio::time::timeout(Duration::from_secs(3),async {loop {
                        let waiting: bool = sqlx::query_scalar("select exists(select 1 from pg_stat_activity where datname=current_database() and application_name='combined_race_reader' and wait_event_type='Lock')").fetch_one(&pool).await.unwrap();
                        if waiting {break;} tokio::time::sleep(Duration::from_millis(10)).await;
                    }}).await.expect("reader must reach after-head barrier");
                    // A signal exactly AT the inclusive midnight cutoff also
                    // exercises the C+1 provenance adapter through real SQL.
                    let replacement = [json!({"setupTimeUnix":g.cutoff-3600,"entryTimeUnix":g.cutoff-3540,"status":"sl","riskReturn":-1.0}),
                        json!({"setupTimeUnix":g.cutoff,"entryTimeUnix":null,"status":"no_data","riskReturn":null})];
                    publish_combined_fixture(&pool,id,&g,&source,&identity,"golden-next",&replacement,&report).await;
                    held.rollback().await.unwrap();
                    let (status,old) = pending.await.unwrap(); assert_eq!(status,StatusCode::OK,"{old}");
                    assert_eq!(old,completed,"MVCC must keep head N, its deleted cohorts and the N projection together");
                    let (status,next) = http(&app,"GET",&path).await; assert_eq!(status,StatusCode::OK,"{next}");
                    assert_eq!(next["revision"],2); assert_eq!(next["jobId"],"golden-next");
                    assert_eq!(next["report"]["combinedProjection"]["availability"]["status"],"available");
                    assert_eq!(next["report"]["combinedProjection"]["revision"],2);
                    assert_eq!(next["report"]["combinedProjection"]["jobId"],"golden-next");
                    assert_eq!(next["report"]["combinedProjection"]["points"].as_array().unwrap().last().unwrap()["atUnix"],json!(g.cutoff as f64));
                    assert_ne!(next["report"]["combinedProjection"]["points"],model["points"]);
                    assert_eq!(next["originalReport"],before["originalReport"]);
                    reader.close().await;
                    sqlx::raw_sql("drop function study_local_day_start(text,date);alter function fixture_day_start(text,date) rename to study_local_day_start").execute(&pool).await.unwrap();
                }
                assert_eq!(market.calls.load(Ordering::SeqCst),0);
                market.stop.cancel(); server.abort(); let _ = server.await;
            }
        }
    }).await;
}

#[tokio::test]
#[ignore = "requires isolated local TEST_DATABASE_URL ending _test with CREATEDB"]
async fn exact_display_dst_iana_midnights_old_original_and_unavailable_legacy() {
    isolated(|pool| async move {
        use setups_db::study_display::OriginalMinute;
        for (zone, times, first, next, hours) in [
            (
                "America%2FNew_York",
                [
                    "2026-03-08T06:30:00Z",
                    "2026-03-09T03:30:00Z",
                    "2026-03-09T04:30:00Z",
                ],
                "2026-03-08T05:00:00Z",
                "2026-03-09T04:00:00Z",
                23,
            ),
            (
                "America%2FNew_York",
                [
                    "2026-11-01T05:30:00Z",
                    "2026-11-01T06:30:00Z",
                    "2026-11-02T05:30:00Z",
                ],
                "2026-11-01T04:00:00Z",
                "2026-11-02T05:00:00Z",
                25,
            ),
            (
                "America%2FHavana",
                [
                    "2026-11-01T04:30:00Z",
                    "2026-11-01T05:30:00Z",
                    "2026-11-02T05:30:00Z",
                ],
                "2026-11-01T04:00:00Z",
                "2026-11-02T05:00:00Z",
                25,
            ),
        ] {
            let start = unix(times[2]) + 7200;
            let points = times
                .into_iter()
                .map(|t| OriginalMinute {
                    signal_close_unix: unix(t),
                    sum_r: 1.0,
                    closed_count: 1,
                    tp_count: 1,
                    sl_count: 0,
                })
                .collect();
            let (id, artifact, _) = original_ledger_fixture(&pool, start, points).await;
            let (app, _, market, server) = fixture(&pool, start).await;
            let path = format!("/v1/optimization-studies/{id}/actual?timeZone={zone}");
            let (status, detail) = http(&app, "GET", &path).await;
            assert_eq!(status, StatusCode::OK, "{detail}");
            let daily = detail["originalReport"]["display"]["daily"]
                .as_array()
                .unwrap();
            assert_eq!(daily.len(), 2);
            assert_eq!(daily[0]["dayUnix"], unix(first));
            assert_eq!(daily[1]["dayUnix"], unix(next));
            assert_eq!(unix(next) - unix(first), hours * 3600);
            assert_eq!(daily[0]["closedCount"], 2);
            assert_eq!(daily[1]["closedCount"], 1);
            if start < unix("2026-04-01T00:00:00Z") {
                assert_eq!(detail["eligibility"]["eligible"], false);
                assert_eq!(
                    detail["eligibility"]["reason"], "study_actual_period_budget_exceeded",
                    "original-only viewing must not enable over-budget refresh"
                );
            }
            assert_eq!(
                daily
                    .iter()
                    .map(|d| d["sumR"].as_f64().unwrap())
                    .sum::<f64>(),
                3.0
            );
            let (bad, error) = http(
                &app,
                "GET",
                &format!("/v1/optimization-studies/{id}/actual?timeZone=Mars%2FOlympus"),
            )
            .await;
            assert_eq!(bad, StatusCode::BAD_REQUEST);
            assert_eq!(error["error"], "invalid_time_zone");
            sqlx::query("delete from study_original_outcomes where artifact_id=$1")
                .bind(artifact)
                .execute(&pool)
                .await
                .unwrap();
            let (_, missing) = http(&app, "GET", &path).await;
            assert_eq!(
                missing["originalReport"]["display"]["historical"]["availability"]["status"],
                "unavailable"
            );
            assert_eq!(
                missing["originalReport"]["display"]["historical"]["reason"],
                "original_outcomes_not_persisted"
            );
            assert_eq!(missing["originalReport"]["display"]["daily"], json!([]));
            assert!(
                missing["originalReport"]["historical"]["daily"]
                    .as_array()
                    .unwrap()
                    .len()
                    > 0,
                "old sealed UTC model history is not relabeled as exact local-day data"
            );
            assert_eq!(market.calls.load(Ordering::SeqCst), 0);
            market.stop.cancel();
            server.abort();
            let _ = server.await;
        }
    })
    .await;
}

#[tokio::test]
#[ignore = "requires isolated local TEST_DATABASE_URL ending _test with CREATEDB"]
async fn exact_display_verified_reconstruction_is_separate_fenced_immutable_publication() {
    isolated(|pool|async move {
    use setups_db::study_display::{self as display,OriginalMinute,VerifiedOriginalIdentity};
    let start=unix("2026-11-20T23:30:00Z");let points=vec![OriginalMinute{signal_close_unix:start-3600,sum_r:0.0,closed_count:2,tp_count:1,sl_count:1}];
    let (id,artifact,_)=original_ledger_fixture(&pool,start,points.clone()).await;
    let immutable:(String,String)=sqlx::query_as("select s.result::text,a.numerical_snapshot::text from optimization_studies s join study_expectancy_artifacts a on a.source_study_id=s.id where s.id=$1").bind(id).fetch_one(&pool).await.unwrap();
    let identity=VerifiedOriginalIdentity{owner_id:"admin".into(),study_id:id,artifact_id:artifact,artifact_key:format!("{id:064x}"),
        study_identity:sqlx::query_scalar("select md5(spec::text||coalesce(result::text,'null')) from optimization_studies where id=$1").bind(id).fetch_one(&pool).await.unwrap(),
        original_snapshot_hash:"f".repeat(64),source_hash:"1".repeat(64),input_hash:"2".repeat(64),proof_hash:"3".repeat(64),tp_pct:10.0,sl_pct:10.0};
    let evidence=json!({"fidelity":"verified_original_reconstruction","scope":"closed_outcomes_only","proofBasis":"add_only_closed_set_monotonicity",
        "sourceHash":identity.source_hash,"inputHash":identity.input_hash,"proofHash":identity.proof_hash,"originalSnapshotHash":identity.original_snapshot_hash,"originalArtifactKey":identity.artifact_key,
        "studyIdentity":identity.study_identity,"conservationMatched":true,"evaluator":"original-historical-close-1","writerAuditHash":"4".repeat(64),
        "firstCanonicalAddOnlyVerified":true,"canonicalOhlcPreservedVerified":true,"historicalCloseKernelCompatibleVerified":true,
        "instrumentCount":1,"utcDayCount":1,"closedCount":2,"tpCount":1,"slCount":1});
    assert!(display::publish_verified_original_reconstruction(&pool,&identity,&points,&evidence).await.unwrap_err().to_string().contains("immutable_ledger_conflict"),"reconstruction cannot relabel/overwrite native evidence");
    sqlx::query("delete from study_original_outcomes where artifact_id=$1").bind(artifact).execute(&pool).await.unwrap();
    let mut stale=identity.clone();stale.study_identity="0".repeat(32);let mut stale_evidence=evidence.clone();stale_evidence["studyIdentity"]=json!(stale.study_identity);
    assert!(display::publish_verified_original_reconstruction(&pool,&stale,&points,&stale_evidence).await.unwrap_err().to_string().contains("verified_source_changed"));
    let mut wrong=points.clone();wrong[0].tp_count=2;wrong[0].sl_count=0;wrong[0].sum_r=2.0;
    assert!(display::publish_verified_original_reconstruction(&pool,&identity,&wrong,&evidence).await.is_err(),"closed-set conservation is mandatory");
    let mut unproven=evidence.clone();unproven["historicalCloseKernelCompatibleVerified"]=json!(false);
    assert!(display::publish_verified_original_reconstruction(&pool,&identity,&points,&unproven).await.unwrap_err().to_string().contains("verified_evidence_invalid"));
    let publication=display::publish_verified_original_reconstruction(&pool,&identity,&points,&evidence).await.unwrap();assert!(publication.created);assert_eq!(publication.fidelity,"verified_original_reconstruction");
    let retry=display::publish_verified_original_reconstruction(&pool,&identity,&points,&evidence).await.unwrap();assert!(!retry.created);assert_eq!(publication.source_identity,retry.source_identity);
    let mut changed=evidence.clone();changed["writerAuditHash"]=json!("5".repeat(64));assert!(display::publish_verified_original_reconstruction(&pool,&identity,&points,&changed).await.unwrap_err().to_string().contains("immutable_ledger_conflict"));
    let (app,_,market,server)=fixture(&pool,start).await;let (status,detail)=http(&app,"GET",&format!("/v1/optimization-studies/{id}/actual?timeZone=Asia%2FKathmandu")).await;assert_eq!(status,StatusCode::OK,"{detail}");
    assert_eq!(detail["originalReport"]["display"]["historical"]["fidelity"],"verified_original_reconstruction");assert_eq!(detail["originalReport"]["display"]["historical"]["sourceIdentity"],publication.source_identity);
    assert_eq!(detail["originalReport"]["display"]["daily"][0]["meanR"],0.0,"zero with observed closes is not a null gap");
    let cached=display::original_observed(&pool,"admin",id,Some(artifact),"UTC").await.unwrap().unwrap();
    use sha2::{Digest,Sha256};
    let old_hash=format!("{:x}",Sha256::digest(serde_json::to_vec(&evidence).unwrap()));
    // Disposable DB fault injection only. Real 1c5 rows have the original CLI
    // digest and NULL projection columns; both UTC and IANA remain available.
    sqlx::query("alter table study_original_outcomes disable trigger user").execute(&pool).await.unwrap();
    sqlx::query("update study_original_outcomes set evidence_sha256=$2,utc_daily=null,utc_daily_sha256=null,utc_source_payload_sha256=null where artifact_id=$1")
        .bind(artifact).bind(&old_hash).execute(&pool).await.unwrap();
    let legacy=display::original_observed(&pool,"admin",id,Some(artifact),"UTC").await.unwrap().unwrap();
    assert_eq!(serde_json::to_value(&legacy.daily).unwrap(),serde_json::to_value(&cached.daily).unwrap());
    assert!(legacy.source_identity.ends_with(&old_hash));
    assert_eq!(display::original_observed(&pool,"admin",id,Some(artifact),"Asia/Kathmandu").await.unwrap().unwrap().daily[0].closed_count,2);
    let mut corrupt=evidence.clone();corrupt["canonicalOhlcPreservedVerified"]=json!(false);
    let corrupt_hash=format!("{:x}",Sha256::digest(serde_json::to_vec(&corrupt).unwrap()));
    sqlx::query("update study_original_outcomes set evidence=$2,evidence_sha256=$3 where artifact_id=$1").bind(artifact).bind(corrupt).bind(corrupt_hash).execute(&pool).await.unwrap();
    for zone in ["UTC","Asia/Kathmandu"] {
        assert!(display::original_observed(&pool,"admin",id,Some(artifact),zone).await.unwrap_err().to_string().contains("verified_evidence_invalid"));
    }
    sqlx::query("update study_original_outcomes set evidence=$2,evidence_sha256=$3,utc_daily=$4,utc_daily_sha256=repeat('0',64),utc_source_payload_sha256=payload_sha256 where artifact_id=$1")
        .bind(artifact).bind(&evidence).bind(old_hash).bind(serde_json::to_value(&cached.daily).unwrap()).execute(&pool).await.unwrap();
    assert!(display::original_observed(&pool,"admin",id,Some(artifact),"UTC").await.unwrap_err().to_string().contains("archive_projection_corrupt"));
    sqlx::query("alter table study_original_outcomes enable trigger user").execute(&pool).await.unwrap();
    assert_eq!(sqlx::query_as::<_,(String,String)>("select s.result::text,a.numerical_snapshot::text from optimization_studies s join study_expectancy_artifacts a on a.source_study_id=s.id where s.id=$1").bind(id).fetch_one(&pool).await.unwrap(),immutable);
    assert_eq!(market.calls.load(Ordering::SeqCst),0);market.stop.cancel();server.abort();let _=server.await;
}).await;
}

#[tokio::test]
#[ignore = "requires isolated local TEST_DATABASE_URL ending _test with CREATEDB"]
async fn exact_display_havana_ambiguous_midnight_marker_matches_earliest_grid_wire() {
    isolated(|pool| async move {
        use setups_db::study_display::OriginalMinute;
        let start = unix("2026-11-01T05:40:00Z");
        let points = vec![
            OriginalMinute {
                signal_close_unix: unix("2026-11-01T04:05:00Z"),
                sum_r: 1.0,
                closed_count: 1,
                tp_count: 1,
                sl_count: 0,
            },
            OriginalMinute {
                signal_close_unix: unix("2026-11-01T05:05:00Z"),
                sum_r: 1.0,
                closed_count: 1,
                tp_count: 1,
                sl_count: 0,
            },
        ];
        let (id, _, _) = original_ledger_fixture(&pool, start, points).await;
        let (app, _, market, server) = fixture(&pool, start).await;
        let (status, detail) = http(
            &app,
            "GET",
            &format!("/v1/optimization-studies/{id}/actual?timeZone=America%2FHavana"),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{detail}");
        let report = &detail["originalReport"];
        assert_eq!(report["executionStartedAt"], "2026-11-01T05:40:00Z");
        assert_eq!(
            report["display"]["startDayUnix"],
            unix("2026-11-01T04:00:00Z")
        );
        assert_eq!(
            report["display"]["daily"][0]["dayUnix"],
            report["display"]["startDayUnix"]
        );
        assert_eq!(report["display"]["daily"][0]["closedCount"], 2);
        if let Ok(path) = std::env::var("STUDY_DISPLAY_FOLD_WIRE_PATH") {
            std::fs::write(path, serde_json::to_vec_pretty(&detail).unwrap()).unwrap();
        }
        assert_eq!(market.calls.load(Ordering::SeqCst), 0);
        market.stop.cancel();
        server.abort();
        let _ = server.await;
    })
    .await;
}

#[tokio::test]
#[ignore = "requires isolated local TEST_DATABASE_URL ending _test with CREATEDB"]
async fn killed_runtime_session_cannot_claim_under_another_owner_then_recovers_without_restart() {
    isolated(|pool|async move {
    let start=chrono::Utc::now().timestamp().div_euclid(60)*60-600;let id=seed(&pool,start).await;let (app,state,market,server)=fixture(&pool,start).await;market.hold.store(true,Ordering::SeqCst);
    assert_eq!(http(&app,"POST",&format!("/v1/optimization-studies/{id}/actual/refresh")).await.0,StatusCode::ACCEPTED);
    let runtime=start_actual_runtime(state,2).await.unwrap();wait(&pool,id,"running").await;
    let pid:i32=sqlx::query_scalar("select pid from pg_stat_activity where datname=current_database() and application_name='study_actual_runtime'").fetch_one(&pool).await.unwrap();
    let mut replacement=pool.acquire().await.unwrap();assert!(sqlx::query_scalar::<_,bool>("select pg_terminate_backend($1)").bind(pid).fetch_one(&mut *replacement).await.unwrap());
    sqlx::query("select pg_advisory_lock($1)").bind(db::RUNTIME_LOCK).execute(&mut *replacement).await.unwrap();
    let failed=wait(&pool,id,"failed").await;assert_eq!(failed["reserved_bytes"],0);assert!(failed["batch_id"].is_null());
    let next=seed(&pool,start).await;assert_eq!(http(&app,"POST",&format!("/v1/optimization-studies/{next}/actual/refresh")).await.0,StatusCode::ACCEPTED);
    tokio::time::sleep(Duration::from_millis(1400)).await;assert_eq!(db::get(&pool,"admin",next).await.unwrap().unwrap()["status"],"queued","old process must not claim through a pool after losing its ownership session");
    market.hold.store(false,Ordering::SeqCst);market.release.notify_waiters();sqlx::query("select pg_advisory_unlock($1)").bind(db::RUNTIME_LOCK).execute(&mut *replacement).await.unwrap();drop(replacement);
    wait(&pool,next,"completed").await;runtime.stop().await;market.stop.cancel();server.abort();let _=server.await;
}).await;
}

#[tokio::test]
#[ignore = "requires isolated local TEST_DATABASE_URL ending _test with CREATEDB"]
async fn transient_failure_settlement_is_retried_and_releases_job_without_restart() {
    isolated(|pool|async move {
    sqlx::raw_sql("create table actual_fail_fixture(blocked boolean);insert into actual_fail_fixture values(true);create function actual_fail_fixture() returns trigger language plpgsql as $$begin if new.status='failed' and exists(select 1 from actual_fail_fixture where blocked) then raise exception 'fixture transient settlement failure';end if;return new;end$$;create trigger actual_fail_fixture before update on study_actualizations for each row execute function actual_fail_fixture();").execute(&pool).await.unwrap();
    let start=chrono::Utc::now().timestamp().div_euclid(60)*60-600;let id=seed(&pool,start).await;let (app,state,market,server)=fixture(&pool,start).await;market.fail.store(true,Ordering::SeqCst);
    assert_eq!(http(&app,"POST",&format!("/v1/optimization-studies/{id}/actual/refresh")).await.0,StatusCode::ACCEPTED);let runtime=start_actual_runtime(state,2).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5),async {while market.calls.load(Ordering::SeqCst)==0{tokio::time::sleep(Duration::from_millis(25)).await;}}).await.unwrap();tokio::time::sleep(Duration::from_millis(350)).await;
    let pending=db::get(&pool,"admin",id).await.unwrap().unwrap();assert_eq!(pending["status"],"running");assert!(pending["reserved_bytes"].as_i64().unwrap()>0);
    sqlx::query("delete from actual_fail_fixture").execute(&pool).await.unwrap();let failed=wait(&pool,id,"failed").await;assert_eq!(failed["reserved_bytes"],0);assert!(failed["batch_id"].is_null());
    market.fail.store(false,Ordering::SeqCst);assert_eq!(http(&app,"POST",&format!("/v1/optimization-studies/{id}/actual/refresh")).await.0,StatusCode::ACCEPTED);assert_eq!(wait(&pool,id,"completed").await["revision"],2);
    runtime.stop().await;market.stop.cancel();server.abort();let _=server.await;
}).await;
}

#[tokio::test]
#[ignore = "requires isolated local TEST_DATABASE_URL ending _test with CREATEDB"]
async fn continuous_cleanup_drops_only_artifact_free_failure_metadata_and_blocks_published_actual()
{
    isolated(|pool|async move {
    use setups_db::continuous::cleanup;
    let start=chrono::Utc::now().timestamp().div_euclid(60)*60-600;let id=seed(&pool,start).await;
    sqlx::query("update optimization_studies set origin='continuous' where id=$1").bind(id).execute(&pool).await.unwrap();
    let retained:i32=sqlx::query_scalar("insert into replay_runs(exchange_code,category,symbol,from_ts,to_ts,algorithm_id,algorithm_version,params,status,setups,origin) values('fixture','linear','KEEP',to_timestamp(0),to_timestamp(60),'price_volume_spike','1.1.1','{}','done','[]','continuous') returning id").fetch_one(&pool).await.unwrap();
    let (app,_,market,server)=fixture(&pool,start).await;
    assert_eq!(http(&app,"POST",&format!("/v1/optimization-studies/{id}/actual/refresh")).await.0,StatusCode::ACCEPTED);
    let job=db::claim(&pool,2).await.unwrap().unwrap();db::fail(&pool,&job,"fixture_failure").await.unwrap();
    let preview=cleanup::preview(&pool,"admin").await.unwrap();assert!(preview.eligible,"artifact-free failure must not block cleanup: {:?}",preview.blockers);
    assert!(matches!(cleanup::purge(&pool,"admin",&preview.fingerprint).await.unwrap(),cleanup::Outcome::Deleted(_)));
    assert!(db::get(&pool,"admin",id).await.unwrap().is_none());assert!(sqlx::query_scalar::<_,bool>("select exists(select 1 from replay_runs where id=$1)").bind(retained).fetch_one(&pool).await.unwrap(),"original retained outputs must remain");
    let next=seed(&pool,start).await;sqlx::query("update optimization_studies set origin='continuous' where id=$1").bind(next).execute(&pool).await.unwrap();
    assert_eq!(http(&app,"POST",&format!("/v1/optimization-studies/{next}/actual/refresh")).await.0,StatusCode::ACCEPTED);
    let job=db::claim(&pool,2).await.unwrap().unwrap();let batch=db::create_batch(&pool,&job).await.unwrap().unwrap();
    assert!(db::append(&pool,&job,batch,db::InstrumentOutput{exchange:"fixture",category:"linear",symbol:"FIX",emitted:&json!([]),rows:&[],quality:&json!({})}).await.unwrap());
    assert!(db::finish(&pool,&job,&json!({"fixture":true}),&json!({"total":0})).await.unwrap());
    let preview=cleanup::preview(&pool,"admin").await.unwrap();assert!(!preview.eligible);assert!(preview.blockers.iter().any(|b|b=="study_actual_references"));assert!(!preview.blockers.iter().any(|b|b=="accepted_profile_references"));
    assert!(matches!(cleanup::purge(&pool,"admin",&preview.fingerprint).await.unwrap(),cleanup::Outcome::Blocked(_)));
    assert_eq!(db::get(&pool,"admin",next).await.unwrap().unwrap()["batch_id"],batch);market.stop.cancel();server.abort();let _=server.await;
}).await;
}
