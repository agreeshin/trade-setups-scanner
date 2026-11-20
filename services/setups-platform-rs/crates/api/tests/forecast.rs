//! HTTP + real PostgreSQL adapter contracts. Synthetic candles are contiguous;
//! the test owns its disposable child DB and never uses a real market source.
use axum::{
    Json, Router,
    body::Body,
    extract::Query,
    http::{Request, StatusCode},
    routing::get,
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use setups_api::{
    SessionAuth,
    analytics::{AnalyticsCandleReader, AnalyticsState, analytics_router},
};
use setups_domain::forecast::{
    ForecastRecipe, ProbabilityCurveRequest, ProbabilityDay, probability_curve,
};
use sqlx::{
    PgPool,
    postgres::{PgConnectOptions, PgPoolOptions},
};
use std::{collections::HashMap, str::FromStr, sync::Arc};
use tower::ServiceExt;

fn state(pool: PgPool, market: &str) -> Arc<AnalyticsState> {
    Arc::new(AnalyticsState {
        pool,
        auth: Arc::new(SessionAuth::new(&["fixture-key".into()])),
        market_url: market.into(),
        market_key: "fixture-market-key".into(),
        reader: AnalyticsCandleReader::new(market, "fixture-market-key"),
        forecast_cpu: Arc::new(tokio::sync::Semaphore::new(1)),
        outcome_compute: Arc::new(tokio::sync::Semaphore::new(1)),
    })
}
fn graph_body() -> Value {
    json!({"forecast": ForecastRecipe::engineering_baseline(), "q": 2.5,
        "trendMode": "forecast", "fromUnix": 0, "toUnix": 172800,
        "observationCutoff": 172920, "atUnix": 259200,
        "daily": [{"dayStartUnix":0,"tpCount":0,"slCount":1},
            {"dayStartUnix":86400,"tpCount":1,"slCount":0}]})
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
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| panic!("non-JSON {status}: {}", String::from_utf8_lossy(&bytes))),
    )
}

#[tokio::test]
async fn forecast_http_auth_validation_busy_lane_and_cleanup_do_not_need_database() {
    let pool = PgPoolOptions::new()
        .connect_lazy("postgres://unused:unused@127.0.0.1:1/unused_test")
        .unwrap();
    let state = state(pool, "http://127.0.0.1:1");
    let app = analytics_router(state.clone());
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/analytics/forecast")
                .header("content-type", "application/json")
                .body(Body::from(graph_body().to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let setups_api::auth::PostSessionResult::Issued { token, .. } = state
        .auth
        .post_session(Some("fixture-key"), setups_api::now_ms())
    else {
        panic!("fixture login")
    };
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/analytics/forecast")
                .header("cookie", format!("{}={token}", setups_api::COOKIE_NAME))
                .header("content-type", "application/json")
                .body(Body::from(graph_body().to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::FORBIDDEN,
        "browser mutation boundary remains unchanged"
    );
    let permit = state.forecast_cpu.clone().acquire_owned().await.unwrap();
    assert_eq!(
        request(&app, "POST", "/v1/analytics/forecast", graph_body())
            .await
            .0,
        StatusCode::TOO_MANY_REQUESTS
    );
    drop(permit);
    for _ in 0..3 {
        let (status, response) =
            request(&app, "POST", "/v1/analytics/forecast", graph_body()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(response["retrospective"], true);
        assert_eq!(response["forecastR"], response["flatFuture"]["forecastR"]);
        assert_eq!(
            state.forecast_cpu.available_permits(),
            1,
            "no retained CPU ownership after completion"
        );
    }
    let mut bad = graph_body();
    bad["observationCutoff"] = json!(chrono::Utc::now().timestamp() + 3600);
    assert_eq!(
        request(&app, "POST", "/v1/analytics/forecast", bad).await.0,
        StatusCode::BAD_REQUEST
    );
    let mut bad = graph_body();
    bad["daily"] = json!(vec![
        json!({"dayStartUnix":0,"tpCount":1,"slCount":0});
        3662
    ]);
    assert_eq!(
        request(&app, "POST", "/v1/analytics/forecast", bad).await.0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(state.forecast_cpu.available_permits(), 1);
    let (status, legacy) = request(
        &app,
        "POST",
        "/v1/analytics/trend",
        json!({"points":[
        {"day":0,"value":1,"weight":1},{"day":1,"value":2,"weight":1}]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(legacy.get("trend").is_some());
    assert!(legacy.get("estimate").is_none());
}

struct TestDb {
    admin: PgPool,
    pool: PgPool,
    name: String,
}
impl TestDb {
    async fn new() -> Self {
        let options = PgConnectOptions::from_str(
            &std::env::var("TEST_DATABASE_URL")
                .expect("isolated *_test TEST_DATABASE_URL required"),
        )
        .unwrap();
        assert!(options.get_database().unwrap().ends_with("_test"));
        let admin = PgPoolOptions::new()
            .max_connections(2)
            .connect_with(options.clone())
            .await
            .unwrap();
        let name = format!(
            "forecast_api_{}_{}_test",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap()
        );
        sqlx::query(&format!("create database {name}"))
            .execute(&admin)
            .await
            .unwrap();
        let pool = PgPoolOptions::new()
            .max_connections(5)
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
        sqlx::raw_sql("set search_path to public;create table kysely_migration(name varchar(255) primary key,timestamp varchar(255) not null);insert into kysely_migration values('0009_legacy','fixture')").execute(&pool).await.unwrap();
        setups_db::migrate::ensure_migrations(&pool).await.unwrap();
        Self { admin, pool, name }
    }
    async fn cleanup(self) {
        self.pool.close().await;
        sqlx::query(&format!("drop database {} with(force)", self.name))
            .execute(&self.admin)
            .await
            .unwrap();
        self.admin.close().await;
    }
}
async fn candles(Query(query): Query<HashMap<String, String>>) -> Json<Value> {
    let from = query["fromUnix"].parse::<i64>().unwrap().max(0);
    let to = query["toUnix"].parse::<i64>().unwrap().min(172980);
    let after = query
        .get("afterUnix")
        .map(|value| value.parse::<i64>().unwrap());
    let first = from.div_euclid(60) * 60;
    let values: Vec<_> = (first..to)
        .step_by(60)
        .filter(|open| after.is_none_or(|after| *open > after))
        .map(|open| {
            json!({"openTimeUnix":open,"open":"100","close":"100",
            "high":if matches!(open,120|172920){"101"}else{"100"},
            "low":if open==172860{"97.5"}else{"100"},"volume":"1","quoteTurnover":"100"})
        })
        .collect();
    assert!(values.len() <= 5000);
    Json(json!({"candles":values,"nextCursorUnix":null}))
}
fn uri(path: &str, query: &[(String, String)]) -> String {
    let mut url = reqwest::Url::parse(&format!("http://fixture.invalid{path}")).unwrap();
    url.query_pairs_mut().extend_pairs(
        query
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str())),
    );
    format!("{}?{}", url.path(), url.query().unwrap())
}
fn set(query: &mut Vec<(String, String)>, key: &str, value: impl ToString) {
    query.retain(|(name, _)| name != key);
    query.push((key.into(), value.to_string()));
}
async fn cell(app: &Router, query: &[(String, String)]) -> Value {
    let (status, value) = request(
        app,
        "GET",
        &uri("/v1/analytics/matrix-cell", query),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{value}");
    value
}

#[tokio::test]
#[ignore = "requires isolated TEST_DATABASE_URL ending _test with CREATEDB"]
async fn manual_http_fixed_cutoff_late_closures_curve_scalar_grid_and_legacy_cache_parity() {
    let fixture = TestDb::new().await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let market = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener,Router::new()
        .route("/v1/instruments",get(||async {Json(json!({"instruments":[{"id":42,"exchange":"bybit","category":"linear","symbol":"FIXTUREUSDT","status":"active"}]}))}))
        .route("/v1/candles",get(candles))).await.unwrap()
    });
    let app = analytics_router(state(fixture.pool.clone(), &market));
    let pool = fixture.pool.clone();
    let outcome=tokio::spawn(async move {
        let run:i32=sqlx::query_scalar("insert into replay_runs(exchange_code,category,symbol,algorithm_id,algorithm_version,params,from_ts,to_ts,status,setups) values('bybit','linear','FIXTUREUSDT','price_volume_spike','1.1.1','{}',to_timestamp(0),to_timestamp(172800),'done',$1) returning id")
            .bind(json!([{"openTimeUnix":0,"closeTimeUnix":60,"closePrice":"100"},
                {"openTimeUnix":86400,"closeTimeUnix":86460,"closePrice":"100"}])).fetch_one(&pool).await.unwrap();
        let mut query=vec![("set".into(),format!("run-{run}")),("direction".into(),"short".into()),
            ("fromDay".into(),"1970-01-01".into()),("toDay".into(),"1970-01-02".into()),
            ("horizonMinutes".into(),"10080".into()),("tpPct".into(),"2.5".into()),("slPct".into(),"1".into())];
        let mut legacy=cell(&app,&query).await;
        assert_eq!(legacy["riskExpectancy"],0.75);assert_eq!(legacy["tpCount"],1);assert_eq!(legacy["slCount"],1);
        assert_eq!(legacy["cached"],false);assert!(legacy.get("forecast").is_none());
        let recipe=ForecastRecipe::engineering_baseline();
        set(&mut query,"forecast",serde_json::to_string(&recipe).unwrap());set(&mut query,"observationCutoff",172860);
        set(&mut query,"trendMode","mean");set(&mut query,"trendBlendLambda","0.95");
        let inactive=cell(&app,&query).await;legacy["cached"]=json!(true);
        assert_eq!(inactive,legacy,"inactive recipe/C/lambda do not fragment or rewrite legacy mean cache");
        set(&mut query,"trendMode","forecast");
        let before=cell(&app,&query).await;
        assert_eq!(before["observationCutoff"],172860);assert_eq!(before["tpCount"],0);assert_eq!(before["slCount"],1);
        assert_eq!(before["unclosedCount"],1);assert_eq!(before["meanR"],-1.0);
        assert!(before["forecastR"].as_f64().unwrap()<0.0);assert_eq!(before["decisionR"],before["forecastR"]);
        set(&mut query,"observationCutoff",172920);
        let after=cell(&app,&query).await;
        assert_eq!(after["tpCount"],1,"TP became available after signal-period end, exactly at candle close C");
        assert_eq!(after["slCount"],1);assert_eq!(after["meanR"],0.75);
        assert_ne!(after["forecastR"],before["forecastR"]);
        assert_eq!(after["probabilityDays"],json!([{"dayStartUnix":0,"tpCount":0,"slCount":1},
            {"dayStartUnix":86400,"tpCount":1,"slCount":0}]));
        for (tp,sl) in [(2.5_f64,1.0_f64),(3.0,1.0),(2.0,3.0)] {
            set(&mut query,"tpPct",tp);set(&mut query,"slPct",sl);
            let matrix=cell(&app,&query).await;
            let daily:Vec<ProbabilityDay>=serde_json::from_value(matrix["probabilityDays"].clone()).unwrap();
            let expected=probability_curve(&ProbabilityCurveRequest {recipe,q:tp/sl,observation_cutoff:172920,daily}).unwrap();
            let body=json!({"forecast":recipe,"trendMode":"forecast","q":tp/sl,"fromUnix":0,"toUnix":172800,
                "observationCutoff":172920,"atUnix":259200,"daily":matrix["probabilityDays"]});
            let (status,graph)=request(&app,"POST","/v1/analytics/forecast",body).await;
            assert_eq!(status,StatusCode::OK,"{graph}");assert_eq!(graph["observationCutoff"],172920);
            assert_eq!(graph["forecastR"].as_f64().unwrap().to_bits(),matrix["forecastR"].as_f64().unwrap().to_bits());
            assert_eq!(graph["forecastR"].as_f64().unwrap().to_bits(),expected.estimate.as_ref().unwrap().forecast_r.to_bits());
            assert_eq!(graph["curve"],json!(expected.curve));assert_eq!(graph["forecastR"],graph["flatFuture"]["forecastR"]);
        }
        set(&mut query,"tpPct","2.5,3");set(&mut query,"slPct",1);
        let (status,grid)=request(&app,"GET",&uri("/v1/analytics/matrix-grid",&query),Value::Null).await;
        assert_eq!(status,StatusCode::OK,"{grid}");assert_eq!(grid["items"].as_array().unwrap().len(),2);
        for item in grid["items"].as_array().unwrap() {
            assert_eq!(item["observationCutoff"],172920);assert!(item.get("probabilityDays").is_none());
        }
        set(&mut query,"tpPct",2.5);set(&mut query,"observationCutoff",172860);
        let repeated=cell(&app,&query).await;
        assert_eq!(repeated["cached"],true);assert_eq!(repeated["forecastR"],before["forecastR"]);
        assert_eq!(repeated["probabilityDays"],before["probabilityDays"],"later C does not mutate the frozen earlier cell");
        query.retain(|(key,_)|key!="observationCutoff");
        assert_eq!(request(&app,"GET",&uri("/v1/analytics/matrix-cell",&query),Value::Null).await.0,StatusCode::BAD_REQUEST);
    }).await;
    server.abort();
    let _ = server.await;
    fixture.cleanup().await;
    outcome.expect("manual forecast HTTP/DB assertions after isolated cleanup");
}
