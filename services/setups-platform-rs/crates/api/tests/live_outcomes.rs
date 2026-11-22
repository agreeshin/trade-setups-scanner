//! Actual PostgreSQL + HTTP contracts; owns disposable child DBs and a fake market only.
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
    analytics::{AnalyticsCandleReader, AnalyticsState},
    batches::batches_router,
    replay::ReplayState,
    setups_analytics::setups_analytics_router,
};
use sqlx::{
    PgPool,
    postgres::{PgConnectOptions, PgPoolOptions},
};
use std::{
    collections::HashMap,
    future::Future,
    str::FromStr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};
use tower::ServiceExt;

const RAW: i64 = 1_794_000_000;
const ENTRY: i64 = RAW + 300;

async fn isolated<F, Fut>(body: F)
where
    F: FnOnce(PgPool) -> Fut + 'static,
    Fut: Future<Output = ()> + 'static,
{
    let options = PgConnectOptions::from_str(
        &std::env::var("TEST_DATABASE_URL").expect("explicit local *_test PostgreSQL URL required"),
    )
    .unwrap();
    assert!(options.get_database().unwrap().ends_with("_test"));
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
        "live_outcomes_{}_{}_test",
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
    let result = tokio::task::LocalSet::new().run_until(async move {
        tokio::task::spawn_local(async move {
            let baseline = include_str!("../../db/migrations/0010_baseline.sql").lines()
                .filter(|line| !line.starts_with('\\') && !line.starts_with("SET transaction_timeout")
                    && !line.contains("set_config('search_path'"))
                .collect::<Vec<_>>().join("\n");
            sqlx::raw_sql(&baseline).execute(&child).await.unwrap();
            sqlx::raw_sql("set search_path to public; create table kysely_migration(name varchar(255) primary key,timestamp varchar(255) not null); insert into kysely_migration values('0009_fixture','fixture'); \
                insert into live_setups(exchange_code,category,symbol,algorithm_id,algorithm_version,params,open_time,close_time,close_price) \
                values('bybit','linear','WATCHUSDT','price_volume_spike','1.1.1','{}',to_timestamp(0),to_timestamp(60),'100')")
                .execute(&child).await.unwrap();
            setups_db::migrate::ensure_migrations(&child).await.unwrap();
            let again = setups_db::migrate::ensure_migrations(&child).await.unwrap();
            assert!(again.starts_with("0 applied"));
            assert!(sqlx::query_scalar::<_, bool>("select trade_outcome is null from live_setups where symbol='WATCHUSDT'")
                .fetch_one(&child).await.unwrap());
            body(child).await;
        }).await
    }).await;
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

async fn profile(pool: &PgPool, owner: &str) -> i64 {
    let config = json!({"adaptiveEnabled":true,"lookbackDays":1,"minSetupsPerDay":[0],"minMeanExpectancy":0.15,
        "study":{"algorithmId":"price_volume_spike","algorithmVersion":"1.1.1","maxSeconds":3600,"maxTrials":8}});
    assert!(
        setups_db::continuous::configure(pool, owner, "price_volume_spike", &config, &[0.0])
            .await
            .unwrap()
    );
    let slot: i64 = sqlx::query_scalar("select id from continuous_slots where owner_id=$1")
        .bind(owner)
        .fetch_one(pool)
        .await
        .unwrap();
    let generation: i64 = sqlx::query_scalar("insert into continuous_generations(owner_id,algorithm_id,slot_id,config_revision,lease_token,status,spec,execution_started_at,observation_cutoff,accepted) \
        values($1,'price_volume_spike',$2,1,0,'completed','{}',clock_timestamp(),0,true) returning id")
        .bind(owner).bind(slot).fetch_one(pool).await.unwrap();
    sqlx::query("insert into adaptive_profiles(id,owner_id,algorithm_id,min_setups_per_day,accepted_generation_id) values($1,$2,'price_volume_spike',0,$3)")
        .bind(slot).bind(owner).bind(generation).execute(pool).await.unwrap();
    slot
}

async fn setup(pool: &PgPool, slot: i64, raw: i64) -> i64 {
    let recommendation = json!({"direction":"short","tpPct":20.0,"slPct":6.0,"meanR":0.32,
        "timeShiftMinutes":5,"priceChangePct":null,"studyId":40});
    i64::from(sqlx::query_scalar::<_, i32>("insert into live_setups(exchange_code,category,symbol,algorithm_id,algorithm_version,params,open_time,close_time,close_price,adaptive_slot_id,recommendation) \
        values('bybit','linear','TESTUSDT','price_volume_spike','1.1.1','{}',to_timestamp($1),to_timestamp($2),'100',$3,$4) returning id")
        .bind((raw-60) as f64).bind(raw as f64).bind(slot).bind(recommendation).fetch_one(pool).await.unwrap())
}

#[derive(Default)]
struct Market {
    calls: AtomicUsize,
    failure: AtomicUsize,
    candles: Mutex<Vec<Value>>,
}

async fn candles(
    State(market): State<Arc<Market>>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    market.calls.fetch_add(1, Ordering::SeqCst);
    match market.failure.load(Ordering::SeqCst) {
        1 => return (StatusCode::SERVICE_UNAVAILABLE, "private fixture failure").into_response(),
        2 => return Json(json!({"candles":[],"nextCursorUnix":123})).into_response(),
        _ => {}
    }
    let from: i64 = query["fromUnix"].parse().unwrap();
    let to: i64 = query["toUnix"].parse().unwrap();
    let values: Vec<_> = market
        .candles
        .lock()
        .unwrap()
        .iter()
        .filter(|bar| {
            let at = bar["openTimeUnix"].as_i64().unwrap();
            at >= from && at < to
        })
        .cloned()
        .collect();
    Json(json!({"candles":values,"nextCursorUnix":null})).into_response()
}

async fn app(
    pool: PgPool,
) -> (
    Router,
    Arc<Market>,
    tokio::task::JoinHandle<()>,
    Arc<SessionAuth>,
) {
    let market = Arc::new(Market::default());
    let server = Router::new().route("/v1/instruments", get(|State(market): State<Arc<Market>>| async move {
        market.calls.fetch_add(1, Ordering::SeqCst);
        Json(json!({"instruments":[{"id":7,"exchange":"bybit","category":"linear","symbol":"TESTUSDT","status":"active"}]}))
    })).route("/v1/candles", get(candles)).with_state(market.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, server).await.unwrap() });
    let auth = Arc::new(SessionAuth::new(&["fixture-key".into()]));
    let state = Arc::new(AnalyticsState {
        pool: pool.clone(),
        auth: auth.clone(),
        market_url: url.clone(),
        market_key: "fixture".into(),
        reader: AnalyticsCandleReader::new(&url, "fixture"),
        forecast_cpu: Arc::new(tokio::sync::Semaphore::new(1)),
        outcome_compute: Arc::new(tokio::sync::Semaphore::new(1)),
    });
    let feed = Arc::new(ReplayState {
        pool,
        auth: auth.clone(),
        market_url: url,
        market_key: "fixture".into(),
        limits: Default::default(),
    });
    (
        setups_analytics_router(state).merge(batches_router(feed)),
        market,
        task,
        auth,
    )
}

async fn request(app: &Router, method: &str, uri: &str, body: Value) -> (StatusCode, Value) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .header("x-api-key", "fixture-key")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or_else(|_| panic!("non-JSON {status}: {bytes:?}")),
    )
}

fn bar(at: i64, high: &str, low: &str) -> Value {
    json!({"openTimeUnix":at,"open":"100","high":high,"low":low,"close":"100","volume":"1","quoteTurnover":"100"})
}

fn final_outcome(r: f64) -> Value {
    json!({"status":if r > 0.0 {"tp"} else {"sl"},"entryTimeUnix":ENTRY,"entryPrice":100.0,"gateThreshold":null,
        "tpPrice":120.0,"slPrice":94.0,"closeTimeUnix":ENTRY+60,"closePrice":if r > 0.0 {120.0} else {94.0},
        "riskReturn":r,"evaluatedAtUnix":ENTRY+120,"observedThroughUnix":ENTRY+120,"isFinal":true})
}

async fn forecast_storage(pool: &PgPool) -> Value {
    sqlx::query_scalar(
        "select jsonb_build_object(\
            'setups', (select jsonb_agg(to_jsonb(l) order by id) from live_setups l), \
            'studies', (select jsonb_agg(to_jsonb(s) order by id) from optimization_studies s), \
            'generations', (select jsonb_agg(to_jsonb(g) order by id) from continuous_generations g), \
            'artifacts', (select jsonb_agg(to_jsonb(a) order by id) from study_expectancy_artifacts a))",
    )
    .fetch_one(pool)
    .await
    .unwrap()
}

#[tokio::test]
#[ignore = "requires isolated TEST_DATABASE_URL ending _test with CREATEDB"]
async fn postgres_forecast_view_calendar_unavailable_and_user_zone_model_are_readonly() {
    isolated(|pool| async move {
        let slot = profile(&pool, "admin").await;
        let spec = json!({"algorithmId":"price_volume_spike","algorithmVersion":"1.1.1",
            "direction":"short","tpLevels":[20.0],"slLevels":[6.0]});
        let result = json!({"best":{"score":0.3,"matrix":[{"tpPct":20.0,"slPct":6.0,
            "riskExpectancy":0.3,"closedCount":10,"tpCount":3,"slCount":7}]}});
        let study: i64 = sqlx::query_scalar("insert into optimization_studies(algorithm_id,status,spec,progress,result,owner_id) values('price_volume_spike','completed',$1,'{}',$2,'admin') returning id")
            .bind(&spec).bind(&result).fetch_one(&pool).await.unwrap();
        sqlx::query("update continuous_generations set study_id=$1,spec=$2,result=$3 where slot_id=$4")
            .bind(study).bind(spec).bind(result).bind(slot).execute(&pool).await.unwrap();
        let (app, market, server, _) = app(pool.clone()).await;
        let assertions = tokio::spawn(async move {
            let day = RAW.div_euclid(86_400) * 86_400;
            let query = format!("algorithmId=price_volume_spike&fromUnix={day}&toUnix={}&studies={study}&q=TESTUSDT", day + 86_400);
            for count in 1..=2 {
                // 18:30Z and 19:30Z share a UTC date but straddle midnight in UTC+5.
                let entry = day + 18 * 3600 + 1800 + (count - 1) * 3600;
                let id = setup(&pool, slot, entry - 300).await;
                let outcome = json!({"status":if count == 1 {"tp"} else {"sl"},
                    "entryTimeUnix":entry,"entryPrice":100.0,"gateThreshold":null,
                    "tpPrice":80.0,"slPrice":106.0,"closeTimeUnix":entry+60,
                    "closePrice":if count == 1 {80.0} else {106.0},
                    "riskReturn":if count == 1 {20.0/6.0} else {-1.0},
                    "evaluatedAtUnix":entry+120,"observedThroughUnix":entry+120,"isFinal":true});
                sqlx::query("update live_setups set trade_outcome=$1,recommendation=recommendation || jsonb_build_object('studyId',$2::bigint) where id=$3")
                    .bind(outcome).bind(study).bind(id as i32).execute(&pool).await.unwrap();
                let before = forecast_storage(&pool).await;
                // Obtain the fingerprint/cutoff from the public saved-analytics flow,
                // not from a reimplementation of the private snapshot hash.
                let (status, saved) = request(&app, "GET", &format!("/v1/setups/analytics?{query}"), json!({})).await;
                assert_eq!(status, StatusCode::OK, "{saved}");
                assert_eq!(saved["totals"]["selected"], count);
                assert_eq!(saved["totals"]["closed"], count);
                assert_eq!(saved["snapshot"]["snapshotId"], id);
                assert_eq!(saved["snapshot"]["fingerprint"].as_str().unwrap().len(), 64);
                for zone in ["UTC", "Asia/Yekaterinburg"] {
                    let body = json!({"snapshot":saved["snapshot"],"forecast":{"method":"trend","version":1,"bendDays":"linear"},
                        "trendMode":"forecast","trendBlendLambda":0.7,"timeZone":zone});
                    let (status, view) = request(&app, "POST", &format!("/v1/setups/analytics/forecast-view?{query}"), body).await;
                    assert_eq!(status, StatusCode::OK, "count={count} zone={zone}: {view}");
                    assert_eq!(view["snapshot"], saved["snapshot"]);
                    assert_eq!(view["snapshotId"], id);
                    assert_eq!(view["selected"], count);
                    assert_eq!(view["closed"], count);
                    assert_eq!(view["meanR"], saved["totals"]["meanR"]);
                    assert_eq!(view["timeZone"], zone);
                    assert_eq!(view["observationCutoff"], saved["snapshot"]["observationCutoff"]);
                    assert!(view["nowUnix"].as_i64().unwrap() >= view["observationCutoff"].as_i64().unwrap());
                    assert_eq!(view["retrospective"], true);
                    assert_eq!(view["model"]["target"], "daily_sum_r");
                    assert_eq!(view["model"]["endpoint"]["atUnix"], view["nowUnix"]);
                    if count == 1 || zone == "UTC" {
                        let unavailable = json!({"status":"unavailable","reason":"insufficient_finite_days"});
                        assert_eq!(view["availability"], unavailable);
                        assert_eq!(view["forecastAvailability"], unavailable);
                        for field in ["forecastR", "decisionR"] {
                            assert!(view[field].is_null());
                            assert!(view["model"]["endpoint"][field].is_null());
                        }
                        assert_eq!(view["model"]["points"], json!([]));
                    } else {
                        assert_eq!(view["availability"], json!({"status":"available"}));
                        assert_eq!(view["forecastAvailability"], json!({"status":"available"}));
                        for field in ["forecastR", "decisionR"] {
                            assert!((view[field].as_f64().unwrap() + 1.0).abs() < 1e-12);
                            assert_eq!(view["model"]["endpoint"][field], view[field]);
                        }
                        let points = view["model"]["points"].as_array().unwrap();
                        assert_eq!(points[0]["atUnix"], day - 5 * 3600);
                        assert!((points[0]["forecastR"].as_f64().unwrap() - 20.0 / 6.0).abs() < 1e-12);
                        assert!(points.iter().any(|point| point["atUnix"] == day + 19 * 3600));
                        assert_eq!(points.last().unwrap(), &view["model"]["endpoint"]);
                    }
                }
                assert_eq!(market.calls.load(Ordering::SeqCst), 0, "forecast display cannot read candles or refresh outcomes");
                assert_eq!(forecast_storage(&pool).await, before, "setup/outcome/recommendation and saved science rows must remain unchanged");
            }
        }).await;
        server.abort();
        let _ = server.await;
        assertions.expect("forecast HTTP/DB assertions after fake market cleanup");
    }).await;
}

#[tokio::test]
#[ignore = "requires isolated TEST_DATABASE_URL ending _test with CREATEDB"]
async fn postgres_unified_literal_filters_match_math_options_history_and_readonly_details() {
    isolated(|pool| async move {
        let slot = profile(&pool,"admin").await;
        let mut ids = Vec::new();
        for (index,(symbol,r)) in [("SPECIAL%_USDT",2.0),("SPECIALX_USDT",-1.0),("SPECIALXYUSDT",2.0)].into_iter().enumerate() {
            let id = setup(&pool,slot,RAW+index as i64*60).await;
            sqlx::query("update live_setups set symbol=$1,trade_outcome=$2 where id=$3").bind(symbol).bind(final_outcome(r)).bind(id as i32).execute(&pool).await.unwrap();
            ids.push(id);
        }
        let before: Value = sqlx::query_scalar("select jsonb_agg(to_jsonb(l) order by id) from live_setups l").fetch_one(&pool).await.unwrap();
        let (app,market,server,_) = app(pool.clone()).await;
        for (q,count,mean) in [("%25_",1,2.0),("specialx_",1,-1.0),("BYBIT",3,1.0),("price_volume",3,1.0)] {
            let query = format!("algorithmId=price_volume_spike&fromUnix={RAW}&toUnix={}&q={q}",RAW+1000);
            for method in ["GET","POST"] {
                let (status,value) = request(&app,method,&format!("/v1/setups/analytics?{query}"),json!({})).await;
                assert_eq!(status,StatusCode::OK,"{value}"); assert_eq!(value["totals"]["selected"],count); assert_eq!(value["totals"]["meanR"],mean);
                assert_eq!(value["setups"].as_array().unwrap().len(),count as usize);
            }
            let (status,options) = request(&app,"GET",&format!("/v1/setups/analytics/studies?{query}"),json!({})).await;
            assert_eq!(status,StatusCode::OK,"{options}"); assert_eq!(options["studies"][0]["setupCount"],count);
            let (status,history) = request(&app,"GET",&format!("/v1/setups?{query}&limit=25"),json!({})).await;
            assert_eq!(status,StatusCode::OK,"{history}"); assert_eq!(history["total"],count);
        }
        let query = format!("algorithmId=price_volume_spike&fromUnix={RAW}&toUnix={}&studies=",RAW+1000);
        let (_,empty) = request(&app,"GET",&format!("/v1/setups?{query}"),json!({})).await;
        assert_eq!(empty["total"],0);
        let (_,empty) = request(&app,"POST",&format!("/v1/setups/analytics?{query}"),json!({})).await;
        assert_eq!(empty["totals"]["selected"],0);
        let (status,detail) = request(&app,"GET","/v1/setups/1/details",json!({})).await;
        assert_eq!(status,StatusCode::OK,"{detail}"); assert!(detail["setup"]["recommendation"].is_null());
        assert!(detail["setup"]["tradeOutcome"].is_null()); assert!(detail["telegramMessage"]["text"].as_str().unwrap().starts_with("WATCH WATCHUSDT"));
        let (status,unsupported) = request(&app,"GET",&format!("/v1/setups/{}/details",ids[0]),json!({})).await;
        assert_eq!(status,StatusCode::OK); assert!(unsupported["telegramMessage"].is_null()); assert!(unsupported["reason"].is_string());
        let anonymous = app.clone().oneshot(Request::builder().uri("/v1/setups/1/details").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(anonymous.status(),StatusCode::UNAUTHORIZED);
        assert_eq!(request(&app,"GET","/v1/setups/2147483648/details",json!({})).await.0,StatusCode::BAD_REQUEST);
        assert_eq!(request(&app,"GET","/v1/setups/2147483647/details",json!({})).await.0,StatusCode::NOT_FOUND);
        assert_eq!(market.calls.load(Ordering::SeqCst),0,"saved reads/details/filtering must not consult market data");
        let after: Value = sqlx::query_scalar("select jsonb_agg(to_jsonb(l) order by id) from live_setups l").fetch_one(&pool).await.unwrap();
        assert_eq!(before,after,"no saved row or recommendation may be rewritten");
        let csv = app.clone().oneshot(Request::builder().uri("/v1/export/setups.csv?q=NOTHING&studies=").header("x-api-key","fixture-key").body(Body::empty()).unwrap()).await.unwrap();
        let bytes = csv.into_body().collect().await.unwrap().to_bytes();
        let text = std::str::from_utf8(&bytes).unwrap();
        assert!(text.starts_with("exchange,category,symbol,algorithmId,algorithmVersion,openTimeUnix,closeTimeUnix,closePrice\r\n"));
        assert!(text.contains("WATCHUSDT") && text.contains("SPECIAL%_USDT") && text.contains("SPECIALXYUSDT"));
        server.abort();
    }).await;
}

#[tokio::test]
#[ignore = "requires isolated TEST_DATABASE_URL ending _test with CREATEDB"]
async fn postgres_unified_history_and_study_options_remain_accessible_beyond_math_limit() {
    isolated(|pool| async move {
        let slot = profile(&pool,"admin").await;
        let id = setup(&pool,slot,RAW).await;
        let rec: Value = sqlx::query_scalar("select recommendation from live_setups where id=$1").bind(id as i32).fetch_one(&pool).await.unwrap();
        sqlx::query("insert into live_setups(exchange_code,category,symbol,algorithm_id,algorithm_version,params,open_time,close_time,close_price,adaptive_slot_id,recommendation) select 'bybit','linear','TESTUSDT','price_volume_spike','1.1.1','{}',to_timestamp($1+g*60-60),to_timestamp($1+g*60),'100',$2,$3 from generate_series(1,5000) g")
            .bind(RAW as f64).bind(slot).bind(rec).execute(&pool).await.unwrap();
        sqlx::query("insert into live_setups(exchange_code,category,symbol,algorithm_id,algorithm_version,params,open_time,close_time,close_price) values('bybit','linear','WATCH_MORE','price_volume_spike','1.1.1','{}',to_timestamp($1),to_timestamp($1+60),'100')").bind(RAW as f64).execute(&pool).await.unwrap();
        let orphan = profile(&pool,"orphan-fixture").await;
        setup(&pool,orphan,RAW+5002*60).await;
        sqlx::query("delete from adaptive_profiles where id=$1").bind(orphan).execute(&pool).await.unwrap();
        let (app,market,server,_) = app(pool.clone()).await;
        let query = format!("algorithmId=price_volume_spike&fromUnix={RAW}&toUnix={}",RAW+1_000_000);
        let (status,failed) = request(&app,"POST",&format!("/v1/setups/analytics?{query}"),json!({})).await;
        assert_eq!(status,StatusCode::BAD_REQUEST); assert_eq!(failed["error"],"setups_analytics_limit_exceeded");
        let (status,options) = request(&app,"GET",&format!("/v1/setups/analytics/studies?{query}"),json!({})).await;
        assert_eq!(status,StatusCode::OK,"{options}"); assert_eq!(options["studies"][0]["setupCount"],5001);
        let (status,page) = request(&app,"GET",&format!("/v1/setups?{query}&offset=5000&limit=25"),json!({})).await;
        assert_eq!(status,StatusCode::OK,"{page}"); assert_eq!(page["total"],5003); assert_eq!(page["setups"].as_array().unwrap().len(),3);
        let (status,subset) = request(&app,"GET",&format!("/v1/setups?{query}&studies=40&limit=25"),json!({})).await;
        assert_eq!(status,StatusCode::OK); assert_eq!(subset["total"],5002,"an explicit subset matches actual saved recommendation study, not WATCH attribution");
        let (status,detail) = request(&app,"GET",&format!("/v1/setups/{id}/details"),json!({})).await;
        assert_eq!(status,StatusCode::OK); assert_eq!(detail["setup"]["id"],id);
        assert_eq!(market.calls.load(Ordering::SeqCst),0);
        server.abort();
    }).await;
}

#[tokio::test]
#[ignore = "requires isolated TEST_DATABASE_URL ending _test with CREATEDB"]
async fn postgres_feed_gate_filter_preserves_unknown_outcomes_counts_search_and_snapshot_pages() {
    isolated(|pool| async move {
        let mut visible = Vec::new();
        let mut all = Vec::new();
        for (index, status) in [Some("gate_failed"), Some("tp"), Some("sl"), Some("unclosed"), Some("no_data"), None, Some("gate_failed")].into_iter().enumerate() {
            let outcome = status.map(|status| json!({"status":status}));
            let id: i32 = sqlx::query_scalar("insert into live_setups(exchange_code,category,symbol,algorithm_id,algorithm_version,params,open_time,close_time,close_price,trade_outcome) values('fixture','linear',$1,'price_volume_spike','1.1.1','{}',to_timestamp($2),to_timestamp($2+60),'100',$3) returning id")
                .bind(format!("GATE{index}"))
                .bind((RAW + index as i64 * 60) as f64)
                .bind(outcome)
                .fetch_one(&pool).await.unwrap();
            all.push(i64::from(id));
            if status != Some("gate_failed") { visible.push(i64::from(id)); }
        }
        visible.reverse(); all.reverse();
        let original: Value = sqlx::query_scalar("select jsonb_agg(to_jsonb(l) order by id) from live_setups l").fetch_one(&pool).await.unwrap();
        let (app, market, server, _) = app(pool.clone()).await;
        for flag in ["", "&hideGateFailed=false"] {
            let (status, page) = request(&app, "GET", &format!("/v1/setups?q=GATE&limit=2{flag}"), json!({})).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(page["total"], 7);
            assert_eq!(page["setups"][0]["id"], all[0]);
        }
        let (status, first) = request(&app, "GET", "/v1/setups?q=GATE&hideGateFailed=true&limit=2", json!({})).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(first["total"], 5);
        assert_eq!(first["setups"].as_array().unwrap().len(), 2);
        assert!(first["setups"][0]["tradeOutcome"].is_null(), "an unknown outcome must remain visible");
        let snapshot = first["snapshotId"].as_i64().unwrap();
        let (_, without_search) = request(&app, "GET", &format!("/v1/setups?hideGateFailed=true&snapshotId={snapshot}"), json!({})).await;
        assert_eq!(without_search["total"], 6, "the no-search query retains WATCH and all five non-failed rows");
        let newer: i32 = sqlx::query_scalar("insert into live_setups(exchange_code,category,symbol,algorithm_id,algorithm_version,params,open_time,close_time,close_price) values('fixture','linear','GATE_NEW','price_volume_spike','1.1.1','{}',to_timestamp($1),to_timestamp($1+60),'100') returning id")
            .bind((RAW + 1000) as f64).fetch_one(&pool).await.unwrap();
        let mut actual = Vec::new();
        for offset in [0, 2, 4] {
            let (status, page) = request(&app, "GET", &format!("/v1/setups?q=GATE&hideGateFailed=true&limit=2&offset={offset}&snapshotId={snapshot}"), json!({})).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(page["total"], 5); assert_eq!(page["offset"], offset); assert_eq!(page["snapshotId"], snapshot);
            for setup in page["setups"].as_array().unwrap() {
                assert_ne!(setup["tradeOutcome"]["status"], "gate_failed");
                actual.push(setup["id"].as_i64().unwrap());
            }
        }
        assert_eq!(actual, visible);
        let (_, empty) = request(&app, "GET", &format!("/v1/setups?q=GATE&hideGateFailed=true&limit=2&offset=6&snapshotId={snapshot}"), json!({})).await;
        assert_eq!(empty["total"], 5); assert!(empty["setups"].as_array().unwrap().is_empty());
        let (_, unhidden) = request(&app, "GET", &format!("/v1/setups?q=GATE&hideGateFailed=false&limit=25&snapshotId={snapshot}"), json!({})).await;
        assert_eq!(unhidden["total"], 7);
        assert_eq!(unhidden["setups"].as_array().unwrap().iter().map(|row| row["id"].as_i64().unwrap()).collect::<Vec<_>>(), all);
        let (_, latest) = request(&app, "GET", "/v1/live/setups?q=GATE&hideGateFailed=true&limit=25", json!({})).await;
        assert_eq!(latest["total"], 6); assert_eq!(latest["setups"][0]["id"], newer);
        let (_, watch) = request(&app, "GET", "/v1/setups?q=WATCH&hideGateFailed=true", json!({})).await;
        assert_eq!(watch["total"], 1); assert!(watch["setups"][0]["tradeOutcome"].is_null());
        for invalid in ["1", "yes", "TRUE", ""] {
            let (status, value) = request(&app, "GET", &format!("/v1/setups?hideGateFailed={invalid}"), json!({})).await;
            assert_eq!(status, StatusCode::BAD_REQUEST); assert_eq!(value["error"], "invalid_query");
        }
        let retained: Value = sqlx::query_scalar("select jsonb_agg(to_jsonb(l) order by id) from live_setups l where id <= $1").bind(snapshot as i32).fetch_one(&pool).await.unwrap();
        assert_eq!(retained, original, "feed filtering cannot rewrite setup or delivery data");
        assert_eq!(market.calls.load(Ordering::SeqCst), 0, "feed filtering cannot recalculate outcomes");
        server.abort(); let _ = server.await;
    }).await;
}

#[tokio::test]
#[ignore = "requires isolated TEST_DATABASE_URL ending _test with CREATEDB"]
async fn postgres_http_persist_refresh_cached_reads_pagination_failures_and_auth() {
    isolated(|pool| async move {
        let slot = profile(&pool, "admin").await;
        let id = setup(&pool, slot, RAW).await;
        let (app, market, server, auth) = app(pool.clone()).await;
        let uri = format!(
            "/v1/setups/analytics?algorithmId=price_volume_spike&fromUnix={RAW}&toUnix={}",
            RAW + 3600
        );
        let (status, value) = request(&app, "GET", &uri, json!({})).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(value["error"], "setups_analytics_outcomes_missing");
        assert_eq!(market.calls.load(Ordering::SeqCst), 0);
        market.failure.store(1, Ordering::SeqCst);
        assert_eq!(request(&app, "POST", &uri, json!({})).await.0, StatusCode::BAD_GATEWAY);
        assert!(sqlx::query_scalar::<_, bool>("select trade_outcome is null from live_setups where id=$1")
            .bind(id as i32).fetch_one(&pool).await.unwrap());
        market.failure.store(0, Ordering::SeqCst);
        let (status, first) = request(&app, "POST", &uri, json!({})).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(first["setups"][0]["status"], "no_data");
        let calls = market.calls.load(Ordering::SeqCst);
        let (_, again) = request(&app, "POST", &uri, json!({})).await;
        assert_eq!(again, first);
        assert_eq!(market.calls.load(Ordering::SeqCst), calls);

        market.failure.store(1, Ordering::SeqCst);
        let (status, _) = request(&app, "POST", "/v1/setups/outcomes/refresh", json!({})).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        let saved: Value = sqlx::query_scalar("select trade_outcome from live_setups where id=$1")
            .bind(id as i32)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(saved["status"], "no_data");
        market.failure.store(2, Ordering::SeqCst);
        assert_eq!(
            request(&app, "POST", "/v1/setups/outcomes/refresh", json!({}))
                .await
                .0,
            StatusCode::BAD_GATEWAY
        );
        market.failure.store(0, Ordering::SeqCst);
        *market.candles.lock().unwrap() = vec![bar(ENTRY, "101", "99")];
        let (status, refresh) =
            request(&app, "POST", "/v1/setups/outcomes/refresh", json!({})).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(refresh["updated"], 1);
        let (_, open) = request(&app, "POST", &uri, json!({})).await;
        assert_eq!(open["setups"][0]["status"], "unclosed");
        market
            .candles
            .lock()
            .unwrap()
            .push(bar(ENTRY + 60, "107", "79"));
        let calls = market.calls.load(Ordering::SeqCst);
        assert_eq!(request(&app, "POST", &uri, json!({})).await.1, open);
        assert_eq!(market.calls.load(Ordering::SeqCst), calls);
        assert_eq!(
            request(&app, "POST", "/v1/setups/outcomes/refresh", json!({}))
                .await
                .0,
            StatusCode::OK
        );
        let (status, closed) = request(&app, "GET", &uri, json!({})).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(closed["setups"][0]["status"], "sl");
        assert_eq!(closed["setups"][0]["closePrice"], 106.0);
        assert_eq!(closed["setups"][0]["closeTimeUnix"], ENTRY + 60);
        assert_eq!(
            closed["totals"],
            json!({"selected":1,"gatePassed":1,"closed":1,"meanR":-1.0})
        );
        market.failure.store(1, Ordering::SeqCst);
        let calls = market.calls.load(Ordering::SeqCst);
        assert_eq!(request(&app, "POST", &uri, json!({})).await.1, closed);
        assert_eq!(
            request(&app, "POST", "/v1/setups/outcomes/refresh", json!({}))
                .await
                .1["processed"],
            0
        );
        let (_, feed) = request(&app, "GET", "/v1/setups?limit=25", json!({})).await;
        let item = feed["setups"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["id"] == id)
            .unwrap();
        assert_eq!(item["tradeOutcome"]["closePrice"], 106.0);
        assert_eq!(item["closePrice"], "100");
        assert_eq!(market.calls.load(Ordering::SeqCst), calls);

        market.failure.store(0, Ordering::SeqCst);
        let id2 = setup(&pool, slot, RAW + 60).await;
        let id3 = setup(&pool, slot, RAW + 120).await;
        let (_, page1) = request(
            &app,
            "POST",
            "/v1/setups/outcomes/refresh",
            json!({"limit":1}),
        )
        .await;
        assert_eq!(page1["processed"], 1);
        assert!(page1["nextCursor"].is_string());
        let newest = setup(&pool, slot, RAW + 180).await;
        let (_, page2) = request(
            &app,
            "POST",
            "/v1/setups/outcomes/refresh",
            json!({"cursor":page1["nextCursor"],"limit":1}),
        )
        .await;
        assert_eq!(page2["processed"], 1);
        assert!(page2["nextCursor"].is_null());
        let rows: Vec<(i32, bool)> = sqlx::query_as(
            "select id,trade_outcome is not null from live_setups where id=any($1) order by id",
        )
        .bind(vec![id2 as i32, id3 as i32, newest as i32])
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            rows,
            vec![
                (id2 as i32, true),
                (id3 as i32, true),
                (newest as i32, false)
            ]
        );
        assert_eq!(
            request(
                &app,
                "POST",
                "/v1/setups/outcomes/refresh",
                json!({"limit":101})
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            request(
                &app,
                "POST",
                "/v1/setups/outcomes/refresh",
                json!({"cursor":"bad"})
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );

        for method in ["GET", "POST"] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(&uri)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        }
        let setups_api::auth::PostSessionResult::Issued { token, .. } =
            auth.post_session(Some("fixture-key"), setups_api::now_ms())
        else {
            panic!("session")
        };
        for path in [&*uri, "/v1/setups/outcomes/refresh"] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri(path)
                        .header("cookie", format!("{}={token}", setups_api::COOKIE_NAME))
                        .header("content-type", "application/json")
                        .header("host", "app.example")
                        .header("origin", "https://foreign.example")
                        .body(Body::from("{}"))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::FORBIDDEN);
        }
        server.abort();
        let _ = server.await;
    })
    .await;
}

#[tokio::test]
#[ignore = "requires isolated TEST_DATABASE_URL ending _test with CREATEDB"]
async fn postgres_same_cutoff_repair_cas_terminal_and_owner_protection() {
    isolated(|pool| async move {
        let slot = profile(&pool, "admin").await;
        let id = setup(&pool, slot, RAW).await;
        let pending = json!({"status":"no_data","observedThroughUnix":600,"evaluatedAtUnix":650});
        let open = json!({"status":"unclosed","observedThroughUnix":600,"evaluatedAtUnix":650});
        let closed = json!({"status":"tp","observedThroughUnix":600,"evaluatedAtUnix":650});
        assert!(
            setups_db::live_outcomes::save(&pool, "admin", id, &pending, false, None)
                .await
                .unwrap()
                .1
        );
        assert!(
            !setups_db::live_outcomes::save(&pool, "admin", id, &open, false, Some(&pending))
                .await
                .unwrap()
                .1
        );
        assert!(
            setups_db::live_outcomes::save(&pool, "admin", id, &open, true, Some(&pending))
                .await
                .unwrap()
                .1
        );
        let stale =
            setups_db::live_outcomes::save(&pool, "admin", id, &pending, true, Some(&pending))
                .await
                .unwrap();
        assert!(!stale.1);
        assert_eq!(stale.0, open);
        assert!(
            !setups_db::live_outcomes::save(&pool, "admin", id, &pending, true, Some(&open))
                .await
                .unwrap()
                .1
        );
        let (a, b) = tokio::join!(
            setups_db::live_outcomes::save(&pool, "admin", id, &closed, true, Some(&open)),
            setups_db::live_outcomes::save(&pool, "admin", id, &pending, true, Some(&open))
        );
        assert!(a.unwrap().1);
        assert!(!b.unwrap().1);
        assert_eq!(
            setups_db::live_outcomes::save(&pool, "admin", id, &open, true, Some(&closed))
                .await
                .unwrap(),
            (closed.clone(), false)
        );
        let other = profile(&pool, "other").await;
        let other_id = setup(&pool, other, RAW).await;
        assert!(
            setups_db::live_outcomes::save(&pool, "admin", other_id, &closed, false, None)
                .await
                .is_err()
        );
        let untouched: bool =
            sqlx::query_scalar("select trade_outcome is null from live_setups where id=$1")
                .bind(other_id as i32)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(untouched);
    })
    .await;
}
