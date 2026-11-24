//! Canonical Decision51 wire contract against a disposable PostgreSQL database.
//! No worker, actualization, replay or production process is started.
use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use setups_api::{
    SessionAuth,
    analytics::{AnalyticsCandleReader, AnalyticsState},
    setups_analytics::setups_analytics_router,
};
use setups_domain::{
    forecast::ForecastRecipe,
    trend::{TrendBendDays, TrendMode},
};
use sqlx::{
    PgPool,
    postgres::{PgConnectOptions, PgPoolOptions},
};
use std::{
    future::Future,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
use tower::ServiceExt;
#[allow(dead_code, unused_imports)]
#[path = "fixtures/issued_projection.rs"]
mod fixture;

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
        "localhost" | "127.0.0.1" | "::1" | "/var/run/postgresql" | "/run/postgresql"
    ));
    let admin = PgPoolOptions::new()
        .max_connections(2)
        .connect_with(options.clone())
        .await
        .unwrap();
    let name = format!(
        "issued_projection_{}_{}_test",
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
    let result=tokio::task::LocalSet::new().run_until(async move {tokio::task::spawn_local(async move {
        let baseline=include_str!("../../db/migrations/0010_baseline.sql").lines().filter(|line|!line.starts_with('\\')&&!line.starts_with("SET transaction_timeout")&&!line.contains("set_config('search_path'")).collect::<Vec<_>>().join("\n");
        sqlx::raw_sql(&baseline).execute(&child).await.unwrap();
        sqlx::raw_sql("set search_path to public;create table kysely_migration(name varchar(255) primary key,timestamp varchar(255) not null);insert into kysely_migration values('0009_fixture','fixture')").execute(&child).await.unwrap();
        setups_db::migrate::ensure_migrations(&child).await.unwrap();body(child).await;
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
async fn http(app: &Router, method: &str, path: &str, body: Value) -> (StatusCode, Value) {
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
    if status == StatusCode::OK {
        assert_eq!(response.headers()["cache-control"], "no-store");
    }
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap())
}
async fn slot(pool: &PgPool) -> i64 {
    let config = json!({"adaptiveEnabled":true,"lookbackDays":1,"minSetupsPerDay":[0],"minMeanExpectancy":0.15,
        "study":{"algorithmId":"price_volume_spike","algorithmVersion":"1.1.1","maxSeconds":3600,"maxTrials":8}});
    assert!(
        setups_db::continuous::configure(pool, "admin", "price_volume_spike", &config, &[0.0])
            .await
            .unwrap()
    );
    sqlx::query_scalar("select id from continuous_slots where owner_id='admin'")
        .fetch_one(pool)
        .await
        .unwrap()
}
async fn seed(pool: &PgPool, slot: i64, f: &fixture::Fixture) -> (i64, i32) {
    let study:i64=sqlx::query_scalar("insert into optimization_studies(algorithm_id,status,spec,progress,result,created_at) values('price_volume_spike','completed',$1,'{}',$2,to_timestamp($3)) returning id")
        .bind(&f.spec).bind(&f.result).bind(f.golden.split as f64).fetch_one(pool).await.unwrap();
    let generation:i64=sqlx::query_scalar("insert into continuous_generations(owner_id,algorithm_id,slot_id,study_id,config_revision,lease_token,status,spec,execution_started_at,observation_cutoff,accepted) values('admin','price_volume_spike',$1,$2,1,0,'completed','{}',to_timestamp($3),$3,true) returning id")
        .bind(slot).bind(study).bind(f.golden.split).fetch_one(pool).await.unwrap();
    sqlx::query("insert into adaptive_profiles(id,owner_id,algorithm_id,min_setups_per_day,accepted_generation_id) values($1,'admin','price_volume_spike',0,$2) on conflict(id) do update set accepted_generation_id=excluded.accepted_generation_id")
        .bind(slot).bind(generation).execute(pool).await.unwrap();
    let mut artifact = f.artifact.clone();
    artifact.source.study_id = study.to_string();
    artifact.source.generation_id = Some(generation.to_string());
    artifact.source.derived_snapshot_id = format!("{study:064x}");
    sqlx::query("insert into study_expectancy_artifacts(artifact_key,owner_id,source_generation_id,source_study_id,source_slot_id,calculation_version,tp_pct,sl_pct,identity,numerical_snapshot) values($1,'admin',$2,$3,$4,$5,10,5,'{}',$6)")
        .bind(&artifact.source.derived_snapshot_id).bind(generation).bind(study).bind(slot).bind(artifact.calculation_version.to_string()).bind(json!(artifact)).execute(pool).await.unwrap();
    // A later non-original artifact must never be substituted for sealed science.
    sqlx::query("insert into study_expectancy_artifacts(artifact_key,owner_id,source_generation_id,source_study_id,source_slot_id,calculation_version,tp_pct,sl_pct,identity,numerical_snapshot,revision_key) values($1,'admin',$2,$3,$4,$5,10,5,'{}',$6,repeat('f',64))")
        .bind(format!("{:064x}",study+10000)).bind(generation).bind(study).bind(slot).bind(artifact.calculation_version.to_string()).bind(json!(artifact)).execute(pool).await.unwrap();
    let mut first = 0;
    for row in &f.rows {
        let mut rec = row["recommendation"].clone();
        rec["studyId"] = json!(study);
        let signal = row["signal"].as_i64().unwrap();
        let id:i32=sqlx::query_scalar("insert into live_setups(exchange_code,category,symbol,algorithm_id,algorithm_version,params,open_time,close_time,close_price,adaptive_slot_id,adaptive_generation_id,recommendation,trade_outcome) values('fixture','linear',$7,'price_volume_spike','1.1.1','{}',to_timestamp($1),to_timestamp($2),'100',$3,$4,$5,$6) returning id")
            .bind((signal-60) as f64).bind(signal as f64).bind(slot).bind(generation).bind(rec).bind(&row["outcome"]).bind(format!("FIX{study}")).fetch_one(pool).await.unwrap();
        if first == 0 {
            first = id;
        }
    }
    (study, first)
}
async fn immutable(pool: &PgPool) -> Value {
    sqlx::query_scalar("select jsonb_build_object('studies',(select jsonb_agg(to_jsonb(s) order by id) from optimization_studies s),'artifacts',(select jsonb_agg(to_jsonb(a) order by id) from study_expectancy_artifacts a),'setups',(select jsonb_agg(to_jsonb(l) order by id) from live_setups l),'generations',(select jsonb_agg(to_jsonb(g) order by id) from continuous_generations g),'profiles',(select jsonb_agg(to_jsonb(p) order by id) from adaptive_profiles p),'actualCount',(select count(*) from fixture_actualizations),'replayCount',(select count(*) from fixture_replay_batches),'outboxCount',(select count(*) from telegram_delivery_outbox))")
        .fetch_one(pool).await.unwrap()
}
fn assert_contract(v: &Value, study: i64) {
    let mut expected = vec![
        "version",
        "studyId",
        "sourceKind",
        "retrospective",
        "objective",
        "recommendedPair",
        "originalSource",
        "issuedSource",
        "filter",
        "snapshot",
        "startDayUnix",
        "originalProjection",
        "combinedProjection",
        "originalDaily",
        "daily",
    ];
    expected.sort_unstable();
    let mut keys = v
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect::<Vec<_>>();
    keys.sort_unstable();
    assert_eq!(keys, expected);
    assert_eq!(v["version"], 1);
    assert_eq!(v["studyId"], study);
    assert_eq!(v["sourceKind"], "persisted_issued_setups");
    assert_eq!(v["retrospective"], false);
    assert_eq!(v["originalSource"]["revisionKey"], "original");
    assert_eq!(v["originalProjection"]["fittedOnIssuedData"], false);
    assert_eq!(v["combinedProjection"]["fittedOnIssuedData"], true);
    assert!(v["startDayUnix"].as_i64().is_some());
    assert!(v["snapshot"]["snapshotId"].as_i64().is_some());
    assert!(v["snapshot"]["observationCutoff"].as_i64().is_some());
    assert_eq!(v["snapshot"]["fingerprint"].as_str().unwrap().len(), 64);
    assert!(v["originalSource"]["artifactId"].as_i64().is_some());
    assert!(v["originalSource"]["observationCutoff"].as_i64().is_some());
    for key in ["originalProjection", "combinedProjection"] {
        if v[key]["availability"]["status"] == "unavailable" {
            assert_eq!(v[key]["points"], json!([]));
            for scalar in ["method", "meanR", "forecastR", "decisionR"] {
                assert!(v[key][scalar].is_null());
            }
        }
        for p in v[key]["points"].as_array().unwrap() {
            assert!(p["atUnix"].as_i64().is_some());
            assert!(p["value"].is_null() || p["value"].as_f64().is_some());
        }
    }
    for key in ["originalDaily", "daily"] {
        for p in v[key].as_array().unwrap() {
            assert_eq!(p["dayUnix"].as_i64().unwrap() % 86400, 0);
        }
    }
}

#[tokio::test]
#[ignore = "requires isolated local TEST_DATABASE_URL ending _test with CREATEDB"]
async fn issued_two_phase_contract_canonical_objectives_fences_and_no_side_effects() {
    isolated(|pool|async move {
        let slot=slot(&pool).await;
        // Missing relation names are deliberate proof that this feature never
        // consults post-study replay/actualization, even for display or status.
        sqlx::raw_sql("alter table study_actual_setups rename to fixture_actual_setups;alter table study_actualizations rename to fixture_actualizations;alter table replay_runs rename to fixture_replay_runs;alter table replay_batches rename to fixture_replay_batches").execute(&pool).await.unwrap();
        let calls=Arc::new(AtomicUsize::new(0));let counted=calls.clone();
        let market=Router::new().fallback(move||{let counted=counted.clone();async move {counted.fetch_add(1,Ordering::SeqCst);StatusCode::INTERNAL_SERVER_ERROR}});
        let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();let url=format!("http://{}",listener.local_addr().unwrap());
        let server=tokio::spawn(async move {axum::serve(listener,market).await.unwrap()});
        let app=setups_analytics_router(Arc::new(AnalyticsState {pool:pool.clone(),auth:Arc::new(SessionAuth::new(&["fixture-key".into()])),
            reader:AnalyticsCandleReader::new(&url,"fixture"),market_url:url,market_key:"fixture".into(),
            forecast_cpu:Arc::new(tokio::sync::Semaphore::new(1)),outcome_compute:Arc::new(tokio::sync::Semaphore::new(1))}));
        let mut first_path=None;
        for recipe in [None,Some(ForecastRecipe::engineering_baseline()),Some(ForecastRecipe::Trend{version:1,bend_days:TrendBendDays::Linear}),Some(ForecastRecipe::Trend{version:1,bend_days:TrendBendDays::Seven})] {
            for mode in [TrendMode::Mean,TrendMode::Forecast,TrendMode::Blend] {
                let f=fixture::issued(mode,recipe);let (id,first)=seed(&pool,slot,&f).await;
                let path=format!("/v1/setups/analytics/study-expectancy/{id}/projection");
                let query=format!("{path}?observationCutoff={}",f.golden.cutoff);
                let before=immutable(&pool).await;
                let (status,get)=http(&app,"GET",&query,json!(null)).await;assert_eq!(status,StatusCode::OK,"{get}");assert_contract(&get,id);
                assert_eq!(get["originalProjection"]["availability"]["status"],"available","{get}");
                assert_eq!(get["originalSource"]["artifactKey"],format!("{id:064x}"),"never use the later recalculation");
                assert_eq!(get["combinedProjection"]["availability"],json!({"status":"unavailable","reason":"issued_projection_not_evaluated"}));
                assert_eq!(http(&app,"GET",&query,json!(null)).await.1,get,"repeated reads are byte-value stable");
                let body=json!({"filter":get["filter"],"snapshot":get["snapshot"]});
                let (status,post)=http(&app,"POST",&path,body.clone()).await;assert_eq!(status,StatusCode::OK,"{post}");assert_contract(&post,id);
                assert_eq!(post["snapshot"],get["snapshot"]);assert_eq!(post["objective"],get["objective"]);assert_eq!(post["originalProjection"],get["originalProjection"]);
                assert_eq!(post["combinedProjection"]["availability"]["status"],"available","{post}");
                assert!((post["combinedProjection"]["decisionR"].as_f64().unwrap()-f.golden.canonical_decision).abs()<1e-10);
                assert_eq!(http(&app,"POST",&path,body.clone()).await.1,post,"same snapshot evaluates deterministically");
                assert_eq!(http(&app,"GET",&query,json!(null)).await.1,get,"POST stores no new projection");
                assert_eq!(immutable(&pool).await,before,"GET/POST cannot change science, history, setup/outcome, profiles, jobs or delivery");
                if first_path.is_none() {first_path=Some((query.clone(),get.clone()));}
                if recipe.is_none() && mode==TrendMode::Mean {
                    let original:Value=sqlx::query_scalar("select trade_outcome from live_setups where id=$1").bind(first).fetch_one(&pool).await.unwrap();
                    sqlx::query("update live_setups set trade_outcome=null where id=$1").bind(first).execute(&pool).await.unwrap();
                    let (status,changed)=http(&app,"POST",&path,body.clone()).await;assert_eq!(status,StatusCode::CONFLICT);assert_eq!(changed["error"],"setups_study_projection_snapshot_changed");
                    let (_,missing)=http(&app,"GET",&query,json!(null)).await;assert_eq!(missing["combinedProjection"]["availability"]["reason"],"issued_outcomes_missing");assert_eq!(missing["originalProjection"],get["originalProjection"]);
                    let missing_body=json!({"filter":missing["filter"],"snapshot":missing["snapshot"]});
                    let (_,missing_post)=http(&app,"POST",&path,missing_body).await;assert_eq!(missing_post["combinedProjection"]["availability"]["reason"],"issued_outcomes_missing");
                    assert_contract(&missing,id);assert_contract(&missing_post,id);
                    assert!(sqlx::query_scalar::<_,bool>("select trade_outcome is null from live_setups where id=$1").bind(first).fetch_one(&pool).await.unwrap());
                    sqlx::query("update live_setups set trade_outcome=$2,recommendation=jsonb_set(recommendation,'{tpPct}','20') where id=$1").bind(first).bind(original).execute(&pool).await.unwrap();
                    let (_,mismatch)=http(&app,"GET",&query,json!(null)).await;assert_eq!(mismatch["combinedProjection"]["availability"]["reason"],"issued_pair_mismatch");assert_eq!(mismatch["originalProjection"],get["originalProjection"]);
                    sqlx::query("update live_setups set recommendation=jsonb_set(recommendation,'{tpPct}','10.0') where id=$1").bind(first).execute(&pool).await.unwrap();
                }
                for invalid in [json!({"live":[]}),json!({"filter":{},"snapshot":get["snapshot"],"timeZone":"UTC"}),json!({"filter":{},"snapshot":get["snapshot"],"executionStartedAt":0})] {
                    assert_eq!(http(&app,"POST",&path,invalid).await.0,StatusCode::BAD_REQUEST);
                }
                assert_eq!(http(&app,"GET",&format!("{query}&timeZone=UTC"),json!(null)).await.0,StatusCode::BAD_REQUEST);
            }
        }
        let (path,first)=first_path.unwrap();
        let (_,old)=http(&app,"GET",&path,json!(null)).await;assert_eq!(old,first,"current accepted profile/winner must not replace the original study");
        assert_eq!(calls.load(Ordering::SeqCst),0,"no GET or evaluation may read markets");
        server.abort();let _=server.await;
    }).await;
}
