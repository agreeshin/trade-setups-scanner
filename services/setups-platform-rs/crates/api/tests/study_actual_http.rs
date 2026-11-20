//! Actual HTTP producers + worker/session lifecycle against disposable PostgreSQL.
use axum::{Router,Json,body::Body,extract::{State,Query},http::{Request,StatusCode},response::{IntoResponse,Response},routing::get};
use http_body_util::BodyExt;
use serde_json::{Value,json};
use setups_api::{SessionAuth,replay::ReplayState,optimization::{OptimizationState,optimization_router},study_actual::{study_actual_router,start_actual_runtime}};
use setups_db::study_actual as db;
use sqlx::{PgPool,postgres::{PgPoolOptions,PgConnectOptions}};
use std::{sync::{Arc,atomic::{AtomicBool,AtomicUsize,Ordering}},collections::HashMap,future::Future,time::Duration};
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

async fn isolated<F,Fut>(body:F) where F:FnOnce(PgPool)->Fut+'static,Fut:Future<Output=()>+'static {
    let options:PgConnectOptions=std::env::var("TEST_DATABASE_URL").expect("explicit local *_test URL required").parse().unwrap();
    assert!(options.get_database().unwrap().ends_with("_test"));assert!(matches!(options.get_host(),"localhost"|"127.0.0.1"|"::1"));
    let admin=PgPoolOptions::new().max_connections(2).connect_with(options.clone()).await.unwrap();
    let name=format!("actual_http_{}_{}_test",std::process::id(),chrono::Utc::now().timestamp_nanos_opt().unwrap());
    sqlx::query(&format!("create database {name}")).execute(&admin).await.unwrap();
    let pool=PgPoolOptions::new().max_connections(12).connect_with(options.database(&name)).await.unwrap();let child=pool.clone();
    let result=tokio::task::LocalSet::new().run_until(async move {tokio::task::spawn_local(async move {
        setups_db::migrate::ensure_migrations(&child).await.unwrap();assert!(setups_db::migrate::ensure_migrations(&child).await.unwrap().starts_with("0 applied"));body(child).await;
    }).await}).await;
    pool.close().await;sqlx::query(&format!("drop database {name} with(force)")).execute(&admin).await.unwrap();admin.close().await;
    if let Err(error)=result {std::panic::resume_unwind(error.into_panic());}
}

async fn seed(pool:&PgPool,start:i64)->i64 {
    let mut params=json!(setups_domain::SpikeParams::default());params["baselineLongDays"]=json!(1);params["baselineShortDays"]=json!(1);
    let analytics=json!({"clusterEnabled":false,"clusterGapMinutes":480,"timeShiftMinutes":1,"priceChangePct":null});
    let spec=json!({"algorithmId":"price_volume_spike","algorithmVersion":"1.1.1","algorithmParams":params,"analyticsParams":analytics,
        "instruments":[{"exchange":"fixture","category":"linear","symbol":"FIX"}],"fromUnix":start-86400*7,"toUnix":start-60,"direction":"short","seed":7,"tpLevels":[10.0],"slLevels":[10.0],"minClosedPerCell":1});
    let result=json!({"best":{"score":1.0,"partial":false,"params":params,"analytics":{"clusterEnabled":false,"clusterGapMinutes":null,"timeShiftMinutes":1,"priceChangePct":null,"setupMaturity":null,"horizonMinutes":10080},
        "matrix":[{"tpPct":10.0,"slPct":10.0,"riskExpectancy":1.0,"avgMovePct":10.0,"closedCount":1,"tpCount":1,"slCount":0,"unclosedCount":0,"noDataCount":0,"priceChangeFilteredCount":0}],
        "provenance":{"optimizerVersion":"plan-a-3","seed":7,"snapshotHash":"immutable-fixture","capturedAt":"2026-11-20T00:00:00Z"},"conclusions":[],
        "support":{"score":1.0,"setupCount":1,"selectedSetupCount":1,"evaluatedMinutes":1000,"warmupMinutes":1440},"trialIndex":0}});
    sqlx::query_scalar("insert into optimization_studies(algorithm_id,status,spec,progress,result,execution_started_at) values('price_volume_spike','completed',$1,'{}',$2,to_timestamp($3)) returning id")
        .bind(spec).bind(result).bind(start as f64).fetch_one(pool).await.unwrap()
}

struct Market {start:i64,hold:AtomicBool,fail:AtomicBool,calls:AtomicUsize,stop:CancellationToken,release:tokio::sync::Notify}
async fn candles(State(m):State<Arc<Market>>,Query(q):Query<HashMap<String,String>>)->Response {
    m.calls.fetch_add(1,Ordering::SeqCst);
    if m.hold.load(Ordering::SeqCst) {tokio::select!{_=m.release.notified()=>{},_=m.stop.cancelled()=>return StatusCode::SERVICE_UNAVAILABLE.into_response(),_=tokio::time::sleep(Duration::from_secs(10))=>return StatusCode::SERVICE_UNAVAILABLE.into_response()}}
    if m.fail.load(Ordering::SeqCst) {return StatusCode::SERVICE_UNAVAILABLE.into_response();}
    let from=q["fromUnix"].parse::<i64>().unwrap();let to=q["toUnix"].parse::<i64>().unwrap();
    let after=q.get("afterUnix").map(|s|s.parse::<i64>().unwrap());let mut rows=Vec::new();
    for open in (m.start-86400-600..m.start+600).step_by(60) {
        if open<from||open>=to||after.is_some_and(|last|open<=last) {continue;}
        let (o,c,h,l,quote)=if open==m.start+120 {("100","120","120","100","10000")}else if open==m.start+480 {("120","120","121","100","100")}else if open>m.start+120 {("120","120","121","119","100")}else{("100","100","101","99","100")};
        rows.push(json!({"openTimeUnix":open,"open":o,"high":h,"low":l,"close":c,"volume":"1","quoteTurnover":quote}));
    }
    Json(json!({"candles":rows,"nextCursorUnix":null})).into_response()
}
async fn fixture(pool:&PgPool,start:i64)->(Router,Arc<ReplayState>,Arc<Market>,tokio::task::JoinHandle<()>) {
    let market=Arc::new(Market{start,hold:AtomicBool::new(false),fail:AtomicBool::new(false),calls:AtomicUsize::new(0),stop:CancellationToken::new(),release:tokio::sync::Notify::new()});
    let md=Router::new().route("/v1/instruments",get(||async {Json(json!({"instruments":[{"id":1,"exchange":"fixture","category":"linear","symbol":"FIX","status":"trading"}]}))})).route("/v1/candles",get(candles)).with_state(market.clone());
    let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();let url=format!("http://{}",listener.local_addr().unwrap());
    let server=tokio::spawn(async move {axum::serve(listener,md).await.unwrap();});let auth=Arc::new(SessionAuth::new(&["fixture-key".into()]));
    let state=Arc::new(ReplayState{pool:pool.clone(),auth:auth.clone(),market_url:url,market_key:"fixture".into(),limits:Default::default()});
    let app=study_actual_router(state.clone()).merge(optimization_router(Arc::new(OptimizationState{pool:pool.clone(),auth,config:Default::default(),notify:Arc::new(tokio::sync::Notify::new()),available:Arc::new(AtomicBool::new(true))})));
    (app,state,market,server)
}
async fn http(app:&Router,method:&str,path:&str)->(StatusCode,Value) {
    let response=app.clone().oneshot(Request::builder().method(method).uri(path).header("x-api-key","fixture-key").header("content-type","application/json").body(Body::from("{}")).unwrap()).await.unwrap();
    let status=response.status();let data=response.into_body().collect().await.unwrap().to_bytes();(status,serde_json::from_slice(&data).unwrap())
}
fn canonical(value:&Value,id:i64) {
    assert_eq!(value["studyId"],id);assert!(value["eligibility"]["eligible"].is_boolean());assert!(value["eligibility"]["reason"].is_null()||value["eligibility"]["reason"].is_string());
    for key in ["jobId","revision","status","stage","observedThroughUnix","counts","meanR","error"] {assert!(value.get(key).is_some(),"missing TS summary field {key}: {value}");}
    assert!(value["revision"].as_u64().is_some());
    if value["status"]=="not_calculated" {assert!(value["stage"].is_null());}else{assert!(value["stage"].is_string());}
    if !value["counts"].is_null() {for key in ["total","closed","tp","sl","gateFailed","unclosed","noData"] {assert!(value["counts"][key].as_u64().is_some(),"invalid TS count {key}");}}
}
async fn wait(pool:&PgPool,id:i64,status:&str)->Value {
    tokio::time::timeout(Duration::from_secs(15),async {loop {let row=db::get(pool,"admin",id).await.unwrap().unwrap();if row["status"]==status{return row}tokio::time::sleep(Duration::from_millis(25)).await;}}).await.expect("job did not reach expected state")
}

#[tokio::test] #[ignore="requires isolated local TEST_DATABASE_URL ending _test with CREATEDB"]
async fn real_history_http_is_canonical_before_first_refresh_queued_and_completed() {isolated(|pool|async move {
    let start=chrono::Utc::now().timestamp().div_euclid(60)*60-600;let id=seed(&pool,start).await;let (app,state,market,server)=fixture(&pool,start).await;
    let original:String=sqlx::query_scalar("select md5(to_jsonb(s)::text) from optimization_studies s where id=$1").bind(id).fetch_one(&pool).await.unwrap();
    let history=format!("/v1/optimization-studies?limit=100");let actual=format!("/v1/optimization-studies/{id}/actual");
    let (status,before)=http(&app,"GET",&history).await;assert_eq!(status,StatusCode::OK);let row=&before["studies"][0]["actual"];canonical(row,id);assert_eq!(row["status"],"not_calculated");assert_eq!(row["eligibility"]["eligible"],true,"first refresh must be enabled without a chart/head");
    let (status,queued)=http(&app,"POST",&format!("{actual}/refresh")).await;assert_eq!(status,StatusCode::ACCEPTED,"{queued}");canonical(&queued,id);
    let (_,pending)=http(&app,"GET",&history).await;assert_eq!(pending["studies"][0]["actual"],serde_json::to_value(db::compact_summary(&db::get(&pool,"admin",id).await.unwrap().unwrap())).unwrap());canonical(&pending["studies"][0]["actual"],id);
    let runtime=start_actual_runtime(state,2).await.unwrap();let completed=wait(&pool,id,"completed").await;
    let (status,detail)=http(&app,"GET",&actual).await;assert_eq!(status,StatusCode::OK);canonical(&detail,id);assert_eq!(detail["report"]["historical"]["availability"]["status"],"unavailable");assert!(detail["counts"]["total"].as_u64().unwrap()>0,"fixture must exercise emitted setup evaluation: {detail}");
    let (_,history)=http(&app,"GET",&history).await;canonical(&history["studies"][0]["actual"],id);for key in ["studyId","eligibility","jobId","revision","status","stage","counts","meanR","observedThroughUnix","error"] {assert_eq!(history["studies"][0]["actual"][key],detail[key],"history/detail mismatch {key}");}
    assert_eq!(completed["reserved_bytes"],0);let (status,page)=http(&app,"GET",&format!("{actual}/setups?revision=1&limit=50")).await;assert_eq!(status,StatusCode::OK);assert_eq!(page["total"],detail["counts"]["total"]);assert_eq!(page["setups"][0]["slotId"],Value::Null);
    assert_eq!(page["setups"][0]["entryTimeUnix"].as_i64().unwrap()-page["setups"][0]["setupTimeUnix"].as_i64().unwrap(),60,"persisted output must use raw close + saved one-minute shift");
    assert_eq!(sqlx::query_scalar::<_,String>("select md5(to_jsonb(s)::text) from optimization_studies s where id=$1").bind(id).fetch_one(&pool).await.unwrap(),original,"original science must remain byte-stable");
    for table in ["replay_cache","live_setups","adaptive_notification_outbox","telegram_delivery_outbox"] {assert_eq!(sqlx::query_scalar::<_,i64>(&format!("select count(*) from {table}")).fetch_one(&pool).await.unwrap(),0,"retrospective job must not publish into {table}");}
    runtime.stop().await;market.stop.cancel();server.abort();let _=server.await;
}).await;}

#[tokio::test] #[ignore="requires isolated local TEST_DATABASE_URL ending _test with CREATEDB"]
async fn killed_runtime_session_cannot_claim_under_another_owner_then_recovers_without_restart() {isolated(|pool|async move {
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
}).await;}

#[tokio::test] #[ignore="requires isolated local TEST_DATABASE_URL ending _test with CREATEDB"]
async fn transient_failure_settlement_is_retried_and_releases_job_without_restart() {isolated(|pool|async move {
    sqlx::raw_sql("create table actual_fail_fixture(blocked boolean);insert into actual_fail_fixture values(true);create function actual_fail_fixture() returns trigger language plpgsql as $$begin if new.status='failed' and exists(select 1 from actual_fail_fixture where blocked) then raise exception 'fixture transient settlement failure';end if;return new;end$$;create trigger actual_fail_fixture before update on study_actualizations for each row execute function actual_fail_fixture();").execute(&pool).await.unwrap();
    let start=chrono::Utc::now().timestamp().div_euclid(60)*60-600;let id=seed(&pool,start).await;let (app,state,market,server)=fixture(&pool,start).await;market.fail.store(true,Ordering::SeqCst);
    assert_eq!(http(&app,"POST",&format!("/v1/optimization-studies/{id}/actual/refresh")).await.0,StatusCode::ACCEPTED);let runtime=start_actual_runtime(state,2).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5),async {while market.calls.load(Ordering::SeqCst)==0{tokio::time::sleep(Duration::from_millis(25)).await;}}).await.unwrap();tokio::time::sleep(Duration::from_millis(350)).await;
    let pending=db::get(&pool,"admin",id).await.unwrap().unwrap();assert_eq!(pending["status"],"running");assert!(pending["reserved_bytes"].as_i64().unwrap()>0);
    sqlx::query("delete from actual_fail_fixture").execute(&pool).await.unwrap();let failed=wait(&pool,id,"failed").await;assert_eq!(failed["reserved_bytes"],0);assert!(failed["batch_id"].is_null());
    market.fail.store(false,Ordering::SeqCst);assert_eq!(http(&app,"POST",&format!("/v1/optimization-studies/{id}/actual/refresh")).await.0,StatusCode::ACCEPTED);assert_eq!(wait(&pool,id,"completed").await["revision"],2);
    runtime.stop().await;market.stop.cancel();server.abort();let _=server.await;
}).await;}

#[tokio::test] #[ignore="requires isolated local TEST_DATABASE_URL ending _test with CREATEDB"]
async fn continuous_cleanup_drops_only_artifact_free_failure_metadata_and_blocks_published_actual() {isolated(|pool|async move {
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
}).await;}
