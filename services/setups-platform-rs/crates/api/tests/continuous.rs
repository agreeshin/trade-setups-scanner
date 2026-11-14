use axum::{Router,Json,body::Body,extract::{State,Query},http::{Request,StatusCode},routing::get,response::{Response,IntoResponse}};
use http_body_util::BodyExt;
use serde::Deserialize;
use serde_json::{Value,json};
use setups_api::{SessionAuth,continuous::{ContinuousConfig,ContinuousRuntimeConfig,ContinuousState,continuous_router,normalize_config,runtime::start_continuous_runtime},optimization::{OptimizationState,optimization_router,start_optimization_runtime}};
use setups_db::continuous::{self as db,ADMIN_OWNER};
use sqlx::{PgPool,Row,postgres::{PgConnectOptions,PgPoolOptions}};
use std::{str::FromStr,sync::{Arc,atomic::{AtomicBool,AtomicUsize,Ordering}},time::Duration};
use tower::ServiceExt;

const ALGORITHM:&str="price_volume_spike";
fn config()->Value {
    json!({"lookbackDays":1.0/24.0,"minSetupsPerDay":[0],"minMeanExpectancy":0.15,"adaptiveEnabled":true,"study":{
        "algorithmId":ALGORITHM,"algorithmVersion":"1.1.1","algorithmParams":{"priceGrowthPct":"0","windowMinutes":1,"baselineLongDays":1,"baselineShortDays":1,"volumeMultiplier":"0.000001","cooldownMinutes":1,"maxGapPercent":"1","maxConsecutiveGapMinutes":5,"requiredRecentMinutes":1},
        "algorithmSearch":{},"analyticsParams":{"clusterEnabled":false,"clusterGapMinutes":480,"timeShiftMinutes":0,"priceChangePct":null},"analyticsSearch":{},
        "instruments":[{"exchange":"bybit","category":"linear","symbol":"BTCUSDT"}],"direction":"short","tpLevels":[1],"slLevels":[1],"horizonMinutes":2,"minClosedPerCell":1,"maxTrials":1,"maxSeconds":10,"seed":42}})
}
fn state(pool:PgPool)->Arc<ContinuousState> {
    Arc::new(ContinuousState{pool,auth:Arc::new(SessionAuth::new(&["test-key".into()])),config:ContinuousRuntimeConfig::default(),notify:Arc::new(tokio::sync::Notify::new()),available:Arc::new(AtomicBool::new(true))})
}
async fn request(router:&Router,method:&str,path:&str,body:Value)->(StatusCode,Value) {
    let response=router.clone().oneshot(Request::builder().method(method).uri(path).header("x-api-key","test-key").header("content-type","application/json").body(Body::from(body.to_string())).unwrap()).await.unwrap();
    let status=response.status();
    let body=response.into_body().collect().await.unwrap().to_bytes();
    (status,serde_json::from_slice(&body).unwrap_or_else(|_|panic!("non-JSON response {status}: {}",String::from_utf8_lossy(&body))))
}
fn path(suffix:&str)->String {format!("/v1/continuous-studies/{ALGORITHM}{suffix}")}

#[test]
fn continuous_config_locks_owned_coordinates_and_normalizes_manual_defaults() {
    let limits=ContinuousRuntimeConfig::default().optimizer;
    let normalized=normalize_config(serde_json::from_value(config()).unwrap(),ALGORITHM,&limits).unwrap();
    for key in ["fromUnix","toUnix","minSetupsPerDay","setupMaturity"] {
        assert!(normalized.study.get(key).is_none());
        let mut body=config();body["study"][key]=json!(0);
        assert!(normalize_config(serde_json::from_value(body).unwrap(),ALGORITHM,&limits).is_err());
    }
    for thresholds in [json!([]),json!([0,0]),json!([-1]),json!([1,1.0])] {
        let mut body=config();body["minSetupsPerDay"]=thresholds;
        assert!(normalize_config(serde_json::from_value(body).unwrap(),ALGORITHM,&limits).is_err());
    }
    assert_eq!(normalized.study["maxTrials"],1);
    assert_eq!(normalized.study["algorithmParams"]["priceGrowthPct"],"0");
    assert!(serde_json::from_value::<ContinuousConfig>({let mut c=config();c["ownerId"]=json!("other");c}).is_err());
}

#[tokio::test]
async fn continuous_routes_authenticate_and_validate_before_database() {
    let pool=PgPoolOptions::new().connect_lazy("postgres://unused:unused@127.0.0.1:1/unused_test").unwrap();
    let router=continuous_router(state(pool));
    for (method,url) in [("GET",path("")),("PUT",path("")),("POST",path("/start")),("POST",path("/stop")),("GET","/v1/continuous-studies/cleanup".into()),("POST","/v1/continuous-studies/cleanup".into()),("GET","/v1/continuous-studies/maintenance".into()),("POST","/v1/continuous-studies/maintenance/drain".into()),("POST","/v1/continuous-studies/maintenance/resume".into())] {
        let response=router.clone().oneshot(Request::builder().method(method).uri(url).header("content-type","application/json").body(Body::from(config().to_string())).unwrap()).await.unwrap();
        assert_eq!(response.status(),StatusCode::UNAUTHORIZED);
    }
    for url in ["/v1/continuous-studies?ownerId=other","/v1/continuous-studies?algorithmId=missing"] {assert_eq!(request(&router,"GET",url,Value::Null).await.0,StatusCode::BAD_REQUEST);}
    let mut invalid=config();invalid["study"]["setupMaturity"]=json!(0);
    assert_eq!(request(&router,"PUT",&path(""),invalid).await.0,StatusCode::BAD_REQUEST);
    for body in [json!({}),json!({"confirm":false,"fingerprint":"0".repeat(32)}),json!({"confirm":true,"fingerprint":"bad"}),json!({"confirm":true,"fingerprint":"0".repeat(32),"ownerId":"other"})] {
        assert_eq!(request(&router,"POST","/v1/continuous-studies/cleanup",body).await.0,StatusCode::BAD_REQUEST);
    }
}

#[tokio::test]
async fn continuous_cleanup_rejects_cookie_mutation_without_browser_origin_before_database() {
    let pool=PgPoolOptions::new().connect_lazy("postgres://unused:unused@127.0.0.1:1/unused_test").unwrap();
    let s=state(pool);
    let setups_api::auth::PostSessionResult::Issued{token,..}=s.auth.post_session(Some("test-key"),setups_api::now_ms()) else {panic!("fixture login failed")};
    let router=continuous_router(s);
    for path in ["/v1/continuous-studies/cleanup","/v1/continuous-studies/maintenance/drain","/v1/continuous-studies/maintenance/resume"] {
        let response=router.clone().oneshot(Request::builder().method("POST").uri(path).header("cookie",format!("{}={token}",setups_api::COOKIE_NAME)).header("content-type","application/json").body(Body::from(json!({"confirm":true,"fingerprint":"0".repeat(32)}).to_string())).unwrap()).await.unwrap();
        assert_eq!(response.status(),StatusCode::FORBIDDEN);
    }
}

struct TestDb{admin:PgPool,pool:PgPool,name:String}
impl TestDb {
    async fn new()->Self {
        let options=PgConnectOptions::from_str(&std::env::var("TEST_DATABASE_URL").expect("isolated TEST_DATABASE_URL required")).unwrap();
        assert!(options.get_database().unwrap().ends_with("_test"));
        let admin=PgPoolOptions::new().max_connections(2).connect_with(options.clone()).await.unwrap();
        let name=format!("continuous_api_{}_{}",std::process::id(),chrono::Utc::now().timestamp_nanos_opt().unwrap());
        sqlx::query(&format!("create database {name}")).execute(&admin).await.unwrap();
        let pool=PgPoolOptions::new().max_connections(10).connect_with(options.database(&name)).await.unwrap();
        let baseline=include_str!("../../db/migrations/0010_baseline.sql").lines().filter(|line|!line.starts_with('\\')&&!line.starts_with("SET transaction_timeout")&&!line.contains("set_config('search_path'")).collect::<Vec<_>>().join("\n");
        sqlx::raw_sql(&baseline).execute(&pool).await.unwrap();
        sqlx::raw_sql("set search_path to public;create table kysely_migration(name varchar(255) primary key,timestamp varchar(255) not null);insert into kysely_migration values('0009_legacy','legacy')").execute(&pool).await.unwrap();
        setups_db::migrate::ensure_migrations(&pool).await.unwrap();
        Self{admin,pool,name}
    }
    async fn cleanup(self) {self.pool.close().await;sqlx::query(&format!("drop database {} with(force)",self.name)).execute(&self.admin).await.unwrap();self.admin.close().await;}
}

#[tokio::test]
#[ignore="requires isolated TEST_DATABASE_URL ending in _test with CREATEDB"]
async fn public_live_readiness_summary_is_active_generation_scoped_and_never_returns_full_projection() {
    let database=TestDb::new().await;
    let router=continuous_router(state(database.pool.clone()));
    assert_eq!(request(&router,"PUT",&path(""),config()).await.0,StatusCode::OK);
    let slot=db::get(&database.pool,ADMIN_OWNER,ALGORITHM).await.unwrap().unwrap().slots[0].id;
    let scope=json!({"instruments":[{"exchange":"bybit","category":"linear","symbol":"BTCUSDT"},
        {"exchange":"bybit","category":"linear","symbol":"NEWUSDT"},
        {"exchange":"bybit","category":"linear","symbol":"MISSINGUSDT"}]});
    let generation:i64=sqlx::query_scalar("insert into continuous_generations(owner_id,algorithm_id,slot_id,config_revision,lease_token,status,spec,execution_started_at,observation_cutoff,accepted,live_readiness) values('admin',$1,$2,1,1,'completed',$3,now(),0,true,$4) returning id")
        .bind(ALGORITHM).bind(slot).bind(scope).bind(json!([
            {"instrumentKey":"bybit/linear/BTCUSDT","status":"ready","reason":null,"publicationFloorCloseUnix":60},
            {"instrumentKey":"bybit/linear/NEWUSDT","status":"waiting","reason":"insufficient_baseline","publicationFloorCloseUnix":60}
        ])).fetch_one(&database.pool).await.unwrap();
    sqlx::query("update continuous_slots set active_generation_id=$2 where id=$1").bind(slot).bind(generation).execute(&database.pool).await.unwrap();
    let (status,body)=request(&router,"GET",&path(""),Value::Null).await;
    assert_eq!(status,StatusCode::OK);
    let actual=&body["slots"][0]["liveReadinessSummary"];
    assert_eq!(actual["generationId"],generation);
    assert_eq!(actual["totalInstruments"],3);
    assert_eq!(actual["readyInstruments"],1);
    assert_eq!(actual["waitingInstruments"],2);
    assert!(actual["waitingReasons"].as_array().unwrap().iter().any(|v|v["reason"]=="unknown" && v["count"]==1));
    assert!(body["slots"][0].get("liveReadiness").is_none(),"polling must not serialize all instruments");
    sqlx::query("update continuous_slots set active_generation_id=null,pending_generation_id=$2,pending_status='warming' where id=$1")
        .bind(slot).bind(generation).execute(&database.pool).await.unwrap();
    let body=request(&router,"GET",&path(""),Value::Null).await.1;
    assert!(body["slots"][0]["liveReadinessSummary"].is_null(),"pending result must never masquerade as active readiness");
    database.cleanup().await;
}

#[tokio::test]
#[ignore="requires isolated TEST_DATABASE_URL ending in _test with CREATEDB"]
async fn cleanup_http_preserves_source_dtos_and_allows_confirmed_terminal_row_deletions() {
    let database=TestDb::new().await;
    let s=state(database.pool.clone());
    let replay=Arc::new(setups_api::replay::ReplayState {pool:database.pool.clone(),auth:s.auth.clone(),market_url:"http://127.0.0.1:1".into(),market_key:"unused".into(),limits:Default::default()});
    let router=continuous_router(s.clone())
        .merge(optimization_router(Arc::new(OptimizationState {pool:database.pool.clone(),auth:s.auth.clone(),config:Default::default(),notify:Arc::new(tokio::sync::Notify::new()),available:Arc::new(AtomicBool::new(false))})))
        .merge(setups_api::replay::replay_router(replay.clone())).merge(setups_api::batches::batches_router(replay));
    assert_eq!(request(&router,"PUT",&path(""),config()).await.0,StatusCode::OK);
    let slot=db::get(&database.pool,ADMIN_OWNER,ALGORITHM).await.unwrap().unwrap().slots[0].id;
    let generation:i64=sqlx::query_scalar("insert into continuous_generations(owner_id,algorithm_id,slot_id,config_revision,lease_token,status,spec,execution_started_at,observation_cutoff,result) values('admin',$1,$2,1,1,'completed','{}',now(),0,'{\"analytics\":{\"matrix\":\"keep\"}}') returning id").bind(ALGORITHM).bind(slot).fetch_one(&database.pool).await.unwrap();
    let study:i64=sqlx::query_scalar("insert into optimization_studies(algorithm_id,status,spec,progress,origin,owner_id,continuous_generation_id,continuous_slot_id) values($1,'completed','{}','{}','continuous','admin',$2,$3) returning id").bind(ALGORITHM).bind(generation).bind(slot).fetch_one(&database.pool).await.unwrap();
    let batch:i32=sqlx::query_scalar("insert into replay_batches(exchange_code,category,algorithm_id,algorithm_version,params,from_ts,to_ts,instrument_count,origin,owner_id,continuous_generation_id) values('bybit','linear',$1,'1.1.1','{}',to_timestamp(0),to_timestamp(60),1,'continuous','admin',$2) returning id").bind(ALGORITHM).bind(generation).fetch_one(&database.pool).await.unwrap();
    let mut runs=Vec::new();
    for parent in [Some(batch),None] {
        let id:i32=sqlx::query_scalar("insert into replay_runs(exchange_code,category,symbol,algorithm_id,algorithm_version,params,from_ts,to_ts,status,setups,batch_id,origin,owner_id,continuous_generation_id) values('bybit','linear','BTCUSDT',$1,'1.1.1','{}',to_timestamp(0),to_timestamp(60),'done','[]',$2,'continuous','admin',$3) returning id").bind(ALGORITHM).bind(parent).bind(generation).fetch_one(&database.pool).await.unwrap();runs.push(id);
    }
    sqlx::query("update continuous_generations set study_id=$2,replay_batch_id=$3 where id=$1").bind(generation).bind(study).bind(batch).execute(&database.pool).await.unwrap();
    for url in [format!("/v1/optimization-studies/{study}"),format!("/v1/replay-runs/{}",runs[1]),format!("/v1/replay-batches/{batch}")] {
        let (status,body)=request(&router,"DELETE",&url,Value::Null).await;
        assert_eq!(status,StatusCode::CONFLICT);assert_eq!(body["error"],"continuous_history_referenced");
    }
    let url="/v1/continuous-studies/cleanup";
    let (status,preview)=request(&router,"GET",url,Value::Null).await;
    assert_eq!(status,StatusCode::OK);assert_eq!(preview["eligible"],true);
    assert_eq!(preview["counts"]["generations"],1);assert_eq!(preview["retained"]["analyticsResults"],1);
    let (status,result)=request(&router,"POST",url,json!({"confirm":true,"fingerprint":preview["fingerprint"]})).await;
    assert_eq!(status,StatusCode::OK,"{result}");assert_eq!(result["deleted"]["studies"],1);
    let (status,details)=request(&router,"GET",&format!("/v1/replay-batches/{batch}"),Value::Null).await;
    assert_eq!(status,StatusCode::OK);assert_eq!(details["run"]["continuousGenerationId"],generation);assert_eq!(details["run"]["continuousStudyId"],study);
    assert_eq!(details["continuousArchive"]["analytics"],json!({"matrix":"keep"}));
    assert_eq!(details["continuousArchive"]["analyticsPresent"],true);
    assert_eq!(details["continuousArchive"]["context"],json!({"effectiveAnalytics":null,"direction":null}));
    let (_,history)=request(&router,"GET","/v1/replay-runs",Value::Null).await;
    assert!(!history.to_string().contains("continuousArchive"),"lists must not load archived analytical blobs");
    for url in [format!("/v1/replay-runs/{}",runs[1]),format!("/v1/replay-batches/{batch}")] {
        let (status,body)=request(&router,"DELETE",&url,Value::Null).await;
        assert_eq!(status,StatusCode::OK,"{body}");assert_eq!(body["deleted"],true);
    }
    let manual:i64=sqlx::query_scalar("insert into optimization_studies(algorithm_id,status,spec,progress,origin,owner_id) values($1,'completed','{}','{}','manual','admin') returning id").bind(ALGORITHM).fetch_one(&database.pool).await.unwrap();
    assert_eq!(request(&router,"DELETE",&format!("/v1/optimization-studies/{manual}"),Value::Null).await.0,StatusCode::OK);
    let active:i64=sqlx::query_scalar("insert into optimization_studies(algorithm_id,status,spec,progress,origin,owner_id) values($1,'running','{}','{}','manual','admin') returning id").bind(ALGORITHM).fetch_one(&database.pool).await.unwrap();
    assert_eq!(request(&router,"DELETE",&format!("/v1/optimization-studies/{active}"),Value::Null).await.0,StatusCode::CONFLICT);
    let (_,again)=request(&router,"GET",url,Value::Null).await;
    let (status,zero)=request(&router,"POST",url,json!({"confirm":true,"fingerprint":again["fingerprint"]})).await;
    assert_eq!(status,StatusCode::OK);assert_eq!(zero["deleted"]["studies"],0);
    assert_eq!(request(&router,"POST",&path("/start"),Value::Null).await.0,StatusCode::ACCEPTED);
    let (status,blocked)=request(&router,"POST",url,json!({"confirm":true,"fingerprint":again["fingerprint"]})).await;
    assert_eq!(status,StatusCode::CONFLICT);assert_eq!(blocked["error"],"continuous_cleanup_blocked");
    database.cleanup().await;
}

#[derive(Default)]
struct MarketState {blocked:AtomicBool,failing:AtomicBool,losing:AtomicBool,catalog_calls:AtomicUsize,candle_calls:AtomicUsize,cutoff:std::sync::Mutex<Option<i64>>}
struct Market {state:Arc<MarketState>,url:String,task:tokio::task::JoinHandle<()>}
#[derive(Deserialize)]
#[serde(rename_all="camelCase")]
struct CandleQuery{instrument_id:i64,from_unix:i64,to_unix:i64,after_unix:Option<i64>,limit:usize}
async fn candles(State(state):State<Arc<MarketState>>,Query(query):Query<CandleQuery>)->Response {
    state.candle_calls.fetch_add(1,Ordering::SeqCst);
    if state.failing.load(Ordering::SeqCst) {return (StatusCode::BAD_GATEWAY,Json(json!({"error":"fixture_transport_failure"}))).into_response();}
    let mut cutoff=state.cutoff.lock().unwrap();if let Some(frozen)=*cutoff {assert_eq!(frozen,query.to_unix);}else{*cutoff=Some(query.to_unix);}drop(cutoff);
    let from=query.after_unix.map(|t|t+60).unwrap_or((query.from_unix+59).div_euclid(60)*60);
    let times=(from..query.to_unix).step_by(60).take(query.limit.min(256)).collect::<Vec<_>>();
    let next=times.last().copied().filter(|last|last+60<query.to_unix);
    let losing=state.losing.load(Ordering::SeqCst);
    Json(json!({"candles":times.iter().map(|time|json!({"instrumentId":query.instrument_id,"openTimeUnix":time,"open":"100","high":if losing{"102"}else{"100"},"low":if losing{"100"}else{"98"},"close":"100","volume":"10","quoteTurnover":"1000"})).collect::<Vec<_>>(),"nextCursorUnix":next})).into_response()
}
impl Market {
    async fn start(blocked:bool)->Self {
        let state=Arc::new(MarketState::default());state.blocked.store(blocked,Ordering::SeqCst);
        let app=Router::new().route("/v1/instruments",get(|State(state):State<Arc<MarketState>>|async move {
            state.catalog_calls.fetch_add(1,Ordering::SeqCst);
            *state.cutoff.lock().unwrap()=None;
            while state.blocked.load(Ordering::SeqCst){tokio::time::sleep(Duration::from_millis(10)).await;}
            Json(json!({"instruments":[{"id":42,"exchange":"bybit","category":"linear","symbol":"BTCUSDT","status":"delisted"}]}))
        })).route("/v1/candles",get(candles)).with_state(state.clone());
        let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();let url=format!("http://{}",listener.local_addr().unwrap());
        let task=tokio::spawn(async move{axum::serve(listener,app).await.unwrap()});
        Self{state,url,task}
    }
}
async fn wait_sql(pool:&PgPool,sql:&str)->Value {
    tokio::time::timeout(Duration::from_secs(15),async {loop {
        let result=sqlx::query_scalar::<_,Value>(sql).fetch_optional(pool).await.unwrap();
        if let Some(result)=result{return result;}
        tokio::time::sleep(Duration::from_millis(20)).await;
    }}).await.unwrap_or_else(|_|panic!("transition timeout: {sql}"))
}

#[tokio::test]
#[ignore="requires isolated TEST_DATABASE_URL ending in _test with CREATEDB"]
async fn maintenance_drain_survives_shutdown_and_completes_same_admitted_chain_without_turning_off_live() {
    let database=TestDb::new().await;let market=Market::start(true).await;
    setups_db::settings_set(&database.pool,"live",&json!({"enabled":true})).await.unwrap();
    let router=continuous_router(state(database.pool.clone()));
    let mut cfg=config();cfg["study"]["maxSeconds"]=json!(60);
    assert_eq!(request(&router,"PUT",&path(""),cfg).await.0,StatusCode::OK);
    request(&router,"POST",&path("/start"),Value::Null).await;
    let runtime=start_continuous_runtime(database.pool.clone(),market.url.clone(),"key".into(),ContinuousRuntimeConfig::default()).await.unwrap();
    let before=wait_sql(&database.pool,"select to_jsonb(g) from continuous_generations g where status='running'").await;
    let (status,draining)=request(&router,"POST","/v1/continuous-studies/maintenance/drain",Value::Null).await;
    assert_eq!(status,StatusCode::OK);assert_eq!(draining["state"],"draining");assert_eq!(draining["activeChains"],1);
    let lease_before:chrono::DateTime<chrono::Utc>=sqlx::query_scalar("select lease_until from continuous_owner_lanes").fetch_one(&database.pool).await.unwrap();
    tokio::time::sleep(Duration::from_millis(1300)).await;
    let lease_after:chrono::DateTime<chrono::Utc>=sqlx::query_scalar("select lease_until from continuous_owner_lanes").fetch_one(&database.pool).await.unwrap();
    assert!(lease_after>lease_before,"drain must continue heartbeat of admitted work");
    runtime.stop().await;
    let paused=wait_sql(&database.pool,"select to_jsonb(g) from continuous_generations g where status='running'").await;
    assert_eq!(paused["id"],before["id"]);assert_eq!(paused["study_id"],before["study_id"]);
    assert_eq!(paused["research_deadline"],before["research_deadline"]);
    assert_eq!(sqlx::query_scalar::<_,i32>("select retry_count from continuous_processes").fetch_one(&database.pool).await.unwrap(),0);
    assert_eq!(setups_db::settings_get(&database.pool,"live").await.unwrap().unwrap()["enabled"],true);
    assert_eq!(request(&router,"GET","/v1/continuous-studies/maintenance",Value::Null).await.1["state"],"draining");
    market.state.blocked.store(false,Ordering::SeqCst);
    let runtime=start_continuous_runtime(database.pool.clone(),market.url.clone(),"key".into(),ContinuousRuntimeConfig::default()).await.unwrap();
    let done=wait_sql(&database.pool,"select to_jsonb(g) from continuous_generations g where status='completed' and accepted=true").await;
    for field in ["id","study_id","execution_started_at","observation_cutoff","research_deadline","spec"] {assert_eq!(done[field],before[field],"resume changed {field}");}
    let status=request(&router,"GET","/v1/continuous-studies/maintenance",Value::Null).await.1;
    assert_eq!(status["state"],"drained");assert_eq!(status["activeChains"],0);
    assert_eq!(sqlx::query_scalar::<_,i64>("select count(*) from continuous_generations").fetch_one(&database.pool).await.unwrap(),1);
    let list=request(&router,"GET","/v1/continuous-studies",Value::Null).await.1;
    assert_eq!(list["maintenance"]["admissionPaused"],true);
    db::stop(&database.pool,ADMIN_OWNER,ALGORITHM).await.unwrap();
    wait_sql(&database.pool,"select to_jsonb(p) from continuous_processes p where status='stopped'").await;
    sqlx::query("update continuous_processes set retry_count=2").execute(&database.pool).await.unwrap();
    request(&router,"POST","/v1/continuous-studies/maintenance/resume",Value::Null).await;
    assert!(!sqlx::query_scalar::<_,bool>("select desired_running from continuous_processes").fetch_one(&database.pool).await.unwrap());
    assert_eq!(sqlx::query_scalar::<_,i32>("select retry_count from continuous_processes").fetch_one(&database.pool).await.unwrap(),2);
    runtime.stop().await;market.task.abort();let _=market.task.await;database.cleanup().await;
}

/// Produce a real fully evaluated fractional matrix, commit a selected durable
/// boundary, then run the actual recovery runtime with an unreachable data API.
async fn resume_post_chain_boundary(boundary:&str) {
    use setups_optimizer as core;
    use setups_db::continuous::{scheduler,recovery};
    let database=TestDb::new().await;
    let mut cfg=config();cfg["study"]["tpLevels"]=json!([1.3]);cfg["study"]["slLevels"]=json!([0.7]);cfg["study"]["maxSeconds"]=json!(120);
    let at_checkpoint=matches!(boundary,"search_checkpoint"|"expired_checkpoint");
    if at_checkpoint {cfg["study"]["maxTrials"]=json!(4);cfg["study"]["algorithmSearch"]=json!({"cooldownMinutes":[1,2,3,4]});}
    if boundary=="expired_checkpoint" {cfg["study"]["maxSeconds"]=json!(2);}
    let limits=ContinuousRuntimeConfig::default();
    let normalized=normalize_config(serde_json::from_value(cfg).unwrap(),ALGORITHM,&limits.optimizer).unwrap();
    db::configure(&database.pool,ADMIN_OWNER,ALGORITHM,&json!(normalized),&[0.0]).await.unwrap();
    db::start(&database.pool,ADMIN_OWNER,ALGORITHM).await.unwrap();
    let lease=scheduler::claim_owner(&database.pool,"boundary-fixture").await.unwrap().unwrap();
    let job=scheduler::prepare_next(&database.pool,&lease).await.unwrap().unwrap();
    let spec:core::StudyRequest=serde_json::from_value(job.spec.clone()).unwrap();
    let context=core::ContinuousContext{observation_cutoff:job.observation_cutoff};
    let bounds=core::required_continuous_input_bounds(&spec,context).unwrap();
    let detector_bars=((bounds.detector_from_unix+59).div_euclid(60)*60..bounds.detector_to_unix).step_by(60)
        .map(|open_time_unix|setups_domain::DetectorBar{open_time_unix,close:"100.0000000000000001".into(),quote_turnover:"1000.0000000000000000".into()}).collect();
    let candles=((bounds.candles_from_unix+59).div_euclid(60)*60..bounds.candles_to_unix).step_by(60)
        .map(|open_time_unix|setups_analytics::OhlcCandle{open_time_unix,open:100.0,high:100.0,low:98.0,close:100.0}).collect();
    let input=Arc::new(core::StudyInput{instruments:vec![core::InstrumentInput{instrument:spec.instruments[0].clone(),detector_bars,candles}],snapshot_hash:format!("{:064x}",42),captured_at:"2026-11-14T00:00:00Z".into()});
    // Small fixture uses the public core/storage interfaces; production bridge
    // remains crate-private and its separate tests cover bounded joined pipes.
    let identity=setups_api::continuous::frozen::identity(&job);
    let mut encoded=Vec::new();
    let codec_limits=core::FrozenInputCodecLimits {max_encoded_bytes:limits.optimizer.max_snapshot_bytes as u64,max_decoded_bytes:limits.optimizer.max_snapshot_bytes,max_record_bytes:1024*1024,max_records:100_000};
    let digest=core::write_frozen_input(&input,&mut encoded,&codec_limits).unwrap();
    let storage=&database.pool;
    db::frozen::reserve(storage,&job,&identity,limits.optimizer.max_snapshot_bytes,limits.max_runtime_bytes,limits.max_database_bytes,&limits.replay_admission).await.unwrap();
    for (index,chunk) in encoded.chunks(db::frozen::CHUNK_BYTES).enumerate() {db::frozen::write_chunk(storage,&job,&identity,index as u32,chunk,limits.optimizer.max_snapshot_bytes).await.unwrap();}
    let manifest=db::frozen::SnapshotManifest {version:db::frozen::FORMAT_VERSION,codec:"framed-cbor-1".into(),chunks:encoded.len().div_ceil(db::frozen::CHUNK_BYTES) as u32,encoded_bytes:digest.encoded_bytes,sha256:digest.sha256.iter().map(|byte|format!("{byte:02x}")).collect(),snapshot_hash:input.snapshot_hash.clone(),captured_at:input.captured_at.clone()};
    db::frozen::seal(storage,&job,&identity,&manifest,limits.optimizer.max_snapshot_bytes,limits.max_runtime_bytes,limits.max_database_bytes,&limits.replay_admission).await.unwrap();
    let cancel=core::CancelToken::new();let pause=core::CancelToken::new();let mut checkpoint=Vec::new();
    let exit=core::search_continuous_resumable(&spec,&input,context,&limits.optimizer.study_limits(),core::ContinuousSearchControl{cancel:&cancel,pause:&pause,remaining_budget:Duration::from_secs(spec.max_seconds),resume_checkpoint:None,max_checkpoint_bytes:recovery::MAX_CHECKPOINT_BYTES},|_|{},|info,bytes|{checkpoint.clear();checkpoint.extend_from_slice(bytes);Ok(if at_checkpoint && info.progress.evaluated_candidates>=1 {core::CheckpointAction::Suspend}else{core::CheckpointAction::Continue})}).unwrap();
    let outcome=match exit {
        core::ContinuousSearchExit::Completed(outcome)=>{assert!(!at_checkpoint);outcome},
        core::ContinuousSearchExit::Suspended(info)=>{
            assert!(at_checkpoint);assert_eq!(info.progress.evaluated_candidates,1);
            // Decode the actual committed checkpoint with a zero budget. This
            // independently proves the expired oracle is its one-trial incumbent,
            // not a later winner of the uninterrupted four-trial reference.
            let expired=boundary=="expired_checkpoint";
            let reference=core::search_continuous_resumable(&spec,&input,context,&limits.optimizer.study_limits(),core::ContinuousSearchControl{cancel:&cancel,pause:&pause,remaining_budget:if expired{Duration::ZERO}else{Duration::from_secs(spec.max_seconds)},resume_checkpoint:if expired{Some(&checkpoint)}else{None},max_checkpoint_bytes:recovery::MAX_CHECKPOINT_BYTES},|_|{},|_,_|Ok(core::CheckpointAction::Continue)).unwrap();
            let core::ContinuousSearchExit::Completed(outcome)=reference else {panic!("reference suspended")};
            if expired {assert_eq!(outcome.reason,core::TerminationReason::TimeBudget);assert_eq!(outcome.progress.evaluated_candidates,1);assert_eq!(outcome.best.as_ref().unwrap().params["cooldownMinutes"],1);}
            outcome
        },
    };
    let best=outcome.best.clone().expect("fractional fixture must be scorable");
    assert!(best.score.fract()!=0.0,"precision regression must not use integer reward");
    recovery::save_checkpoint(&database.pool,&job,&identity,&checkpoint,limits.max_runtime_bytes).await.unwrap();
    if !at_checkpoint {scheduler::finish_search(&database.pool,&job,&json!(outcome),true).await.unwrap();}
    // Actual JSONB round-trip must preserve the full terminal best, not bestScore.
    let saved=setups_db::optimization::get(&database.pool,job.study_id).await.unwrap().unwrap();
    if at_checkpoint {assert_eq!(saved.status,"running");assert!(saved.result.is_none());}else{
        let saved:core::StudyOutcome=serde_json::from_value(saved.result.unwrap()).unwrap();
        assert_eq!(saved.best.as_ref().unwrap().score.to_bits(),best.score.to_bits());assert_eq!(saved.best.unwrap().matrix,best.matrix);
    }
    if matches!(boundary,"replay"|"gate") {
        let candidate=core::StudyCandidate{params:best.params.clone(),analytics:core::AnalyticsParams{cluster_enabled:best.analytics.cluster_enabled,cluster_gap_minutes:best.analytics.cluster_gap_minutes.unwrap_or(spec.analytics_params.cluster_gap_minutes),time_shift_minutes:best.analytics.time_shift_minutes,price_change_pct:best.analytics.price_change_pct}};
        let replay=core::materialize_continuous(&spec,&input,&candidate,context,&limits.optimizer.study_limits(),&cancel).unwrap().unwrap();
        let bootstrap=core::continuous_bootstrap_with_frozen_prefix(&spec,&replay,&input).unwrap();
        let manifest=json!({"fromUnix":replay.from_unix,"toUnix":replay.to_unix,"context":replay.context,"params":replay.params,"provenance":replay.provenance,"bootstrap":bootstrap,
            "instruments":replay.instruments.iter().map(|raw|json!({"instrument":raw.instrument,"previousRawCloseUnix":raw.previous_raw_close_unix,"lastRawCloseUnix":raw.last_raw_close_unix,"evaluatedMinutes":raw.evaluated_minutes,"warmupMinutes":raw.warmup_minutes})).collect::<Vec<_>>()});
        let runs=replay.instruments.iter().map(|raw|scheduler::ServiceReplayRun{exchange:raw.instrument.exchange.clone(),category:raw.instrument.category.clone(),symbol:raw.instrument.symbol.clone(),setups:json!(raw.setups),warmup_minutes:raw.warmup_minutes as i64,evaluated_minutes:raw.evaluated_minutes as i64}).collect::<Vec<_>>();
        let batch=scheduler::persist_replay(&database.pool,&job,&json!(candidate.params),manifest,&runs).await.unwrap().unwrap();
        if boundary=="gate" {
            let analytics=core::analyze_continuous(&spec,&input,&replay,&candidate,&limits.optimizer.study_limits(),&cancel).unwrap().unwrap();
            let recommendation=core::recommend_continuous(&spec,&candidate,&analytics,&best.provenance,context,0.15).unwrap();
            let selected=core::select_robust_cell(&analytics.evaluation.matrix,&spec.tp_levels,&spec.sl_levels,spec.min_closed_per_cell).unwrap().map(|cell|json!(cell));
            let result=json!({"best":best,"selectedCell":selected,"meanExpectancy":analytics.evaluation.summary.score,"analytics":analytics,"recommendation":recommendation,"bootstrap":bootstrap,"setupSetId":format!("batch-{batch}")});
            let gate=recovery::Gate{result,accepted:recommendation.is_some(),mean:Some(best.score),selected};
            recovery::save_gate(&database.pool,&job,&identity,&gate).await.unwrap();
        }
    }
    db::maintenance::set_paused(&database.pool,ADMIN_OWNER,true).await.unwrap();
    if boundary=="expired_checkpoint" {
        let remaining=job.research_deadline.unwrap().signed_duration_since(chrono::Utc::now()).to_std().unwrap_or(Duration::ZERO);
        tokio::time::sleep(remaining+Duration::from_millis(30)).await;
    }
    scheduler::release(&database.pool,&lease).await.unwrap();
    let runtime=start_continuous_runtime(database.pool.clone(),"http://127.0.0.1:1".into(),"unused".into(),limits).await.unwrap();
    let completed=wait_sql(&database.pool,"select to_jsonb(g) from continuous_generations g where status='completed' and accepted=true").await;
    assert_eq!(completed["id"],job.generation_id);assert_eq!(completed["study_id"],job.study_id);
    let matrix:Vec<core::MatrixCell>=serde_json::from_value(completed["result"]["best"]["matrix"].clone()).unwrap();assert_eq!(matrix,best.matrix);
    let score=completed["result"]["meanExpectancy"].as_f64().unwrap();assert_eq!(score.to_bits(),best.score.to_bits());
    assert_eq!(sqlx::query_scalar::<_,i64>("select count(*) from replay_batches").fetch_one(&database.pool).await.unwrap(),1);
    assert_eq!(sqlx::query_scalar::<_,i64>("select count(*) from continuous_generations").fetch_one(&database.pool).await.unwrap(),1);
    assert_eq!(sqlx::query_scalar::<_,i64>("select count(*) from continuous_runtime_artifacts").fetch_one(&database.pool).await.unwrap(),0);
    if boundary=="expired_checkpoint" {
        let study=setups_db::optimization::get(&database.pool,job.study_id).await.unwrap().unwrap();
        let outcome:core::StudyOutcome=serde_json::from_value(study.result.unwrap()).unwrap();
        assert_eq!(outcome.reason,core::TerminationReason::TimeBudget);assert_eq!(outcome.progress.evaluated_candidates,1,"expired original budget must not evaluate another trial");
    }
    runtime.stop().await;database.cleanup().await;
}

#[tokio::test]
#[ignore="requires isolated TEST_DATABASE_URL ending in _test with CREATEDB"]
async fn completed_search_restarts_post_chain_with_exact_fractional_matrix_and_no_market_reread() {resume_post_chain_boundary("search").await;}
#[tokio::test]
#[ignore="requires isolated TEST_DATABASE_URL ending in _test with CREATEDB"]
async fn committed_replay_restarts_analytics_with_exact_fractional_matrix_and_no_search_or_market_reread() {resume_post_chain_boundary("replay").await;}
#[tokio::test]
#[ignore="requires isolated TEST_DATABASE_URL ending in _test with CREATEDB"]
async fn committed_gate_restarts_idempotent_finalize_with_exact_fractional_result() {resume_post_chain_boundary("gate").await;}
#[tokio::test]
#[ignore="requires isolated TEST_DATABASE_URL ending in _test with CREATEDB"]
async fn private_search_checkpoint_restarts_same_search_without_new_ids_or_market_input() {resume_post_chain_boundary("search_checkpoint").await;}
#[tokio::test]
#[ignore="requires isolated TEST_DATABASE_URL ending in _test with CREATEDB"]
async fn expired_original_deadline_finishes_from_checkpoint_without_reset_or_more_trials() {resume_post_chain_boundary("expired_checkpoint").await;}

// Explicit readiness transition for API-only fixtures: no worker/scanner is running here.
async fn promote_fixture_pending(pool:&PgPool)->i64 {
    use setups_db::continuous::live::*;
    let pending=pending_adaptive_slots(pool,ADMIN_OWNER).await.unwrap().remove(0);
    let profile=&pending.profile;
    let bootstrap=&profile.result["bootstrap"];
    let frontier=bootstrap["nextOpenUnix"].as_i64().unwrap();
    let mut expected=Vec::new(); let mut initial=Vec::new();
    for raw in bootstrap["instruments"].as_array().unwrap() {
        let instrument=&raw["instrument"];
        let key=format!("{}/{}/{}",instrument["exchange"].as_str().unwrap(),instrument["category"].as_str().unwrap(),instrument["symbol"].as_str().unwrap());
        expected.push(CheckpointExpectation {instrument_key:key.clone(),checkpoint:db::get_adaptive_checkpoint(pool,profile.slot_id,&key).await.unwrap()});
        initial.push(InitialAdaptiveCheckpoint {instrument_key:key,state:json!({"generationId":profile.generation_id,"nextOpenUnix":frontier,"lastRawOpenUnix":raw["lastRawOpenUnix"],"lastRawClusterTime":raw["lastRawClusterTime"]})});
    }
    setups_db::settings_set(pool,"live",&json!({"enabled":true})).await.unwrap();
    let lease=claim_pending_warmup(pool,&pending,"api-fixture-readiness").await.unwrap().unwrap();
    assert!(report_warmup(pool,&lease,"ready",Some(frontier),None).await.unwrap());
    assert!(promote_pending(pool,&lease,pending.expected_active_generation_id,&expected,frontier,&initial).await.unwrap());
    profile.generation_id
}

#[tokio::test]
#[ignore="requires isolated TEST_DATABASE_URL ending in _test with CREATEDB"]
async fn real_chain_persists_service_set_recomputes_analytics_activates_and_stop_preserves_completed_history() {
    let database=TestDb::new().await;let market=Market::start(false).await;
    setups_db::settings_set(&database.pool,"algorithm:price_volume_spike",&json!({"enabled":true,"params":{"fixedSentinel":42},"externalName":"Alias"})).await.unwrap();
    let fixed=setups_db::settings_get(&database.pool,"algorithm:price_volume_spike").await.unwrap();
    let runtime=start_continuous_runtime(database.pool.clone(),market.url.clone(),"key".into(),ContinuousRuntimeConfig::default()).await.unwrap();
    let mut s=state(database.pool.clone());Arc::get_mut(&mut s).unwrap().notify=runtime.notify.clone();
    let replay=Arc::new(setups_api::replay::ReplayState{pool:database.pool.clone(),auth:s.auth.clone(),market_url:market.url.clone(),market_key:"key".into(),limits:Default::default()});
    let router=continuous_router(s.clone()).merge(optimization_router(Arc::new(OptimizationState{pool:database.pool.clone(),auth:s.auth.clone(),config:Default::default(),notify:Arc::new(tokio::sync::Notify::new()),available:Arc::new(AtomicBool::new(false))})))
        .merge(setups_api::replay::replay_router(replay.clone())).merge(setups_api::batches::batches_router(replay));
    let before=chrono::Utc::now().timestamp();
    assert_eq!(request(&router,"GET",&path(""),Value::Null).await.1["config"],Value::Null);
    assert_eq!(request(&router,"PUT",&path(""),config()).await.0,StatusCode::OK);
    assert_eq!(request(&router,"POST",&path("/start"),Value::Null).await.0,StatusCode::ACCEPTED);
    let generation=wait_sql(&database.pool,"select to_jsonb(g) from continuous_generations g where status='completed' and accepted=true order by id limit 1").await;
    let generation_id=generation["id"].as_i64().unwrap();let study_id=generation["study_id"].as_i64().unwrap();let batch_id=generation["replay_batch_id"].as_i64().unwrap();
    assert!(generation["spec"]["toUnix"].as_i64().unwrap()>=before);
    assert_eq!(generation["spec"]["toUnix"].as_i64().unwrap()-generation["spec"]["fromUnix"].as_i64().unwrap(),3600);
    assert!(generation["observation_cutoff"].as_i64().unwrap()<=generation["spec"]["toUnix"].as_i64().unwrap());
    assert_eq!(*market.state.cutoff.lock().unwrap(),generation["observation_cutoff"].as_i64());
    assert_eq!(generation["result"]["meanExpectancy"],1.0);
    assert_eq!(generation["result"]["recommendation"]["rawR"],1.0);
    assert_eq!(generation["result"]["setupSetId"],format!("batch-{batch_id}"));
    assert_eq!(generation["result"]["analytics"]["evaluation"]["matrix"],generation["result"]["best"]["matrix"]);
    assert!(generation["result"]["bootstrap"]["instruments"][0].get("setups").is_none());
    assert_eq!(generation["result"]["bootstrap"]["frozenPrefix"]["version"],1);
    assert_eq!(generation["result"]["bootstrap"]["frozenPrefix"]["instruments"][0]["sha256"].as_str().unwrap().len(),64);
    let staged=db::get(&database.pool,ADMIN_OWNER,ALGORITHM).await.unwrap().unwrap();
    assert!(staged.slots[0].active_generation_id.is_none());
    assert_eq!(staged.slots[0].pending_generation_id,Some(generation_id));
    assert_eq!(staged.slots[0].pending_status.as_deref(),Some("warming"));
    assert_eq!(market.state.catalog_calls.load(Ordering::SeqCst),1,"post-chain must not refetch market data");
    let (_,history)=request(&router,"GET","/v1/optimization-studies",Value::Null).await;
    assert_eq!(history["studies"][0]["origin"],"continuous");
    assert_eq!(history["studies"][0]["continuousGenerationId"],generation_id);
    assert_eq!(request(&router,"POST",&format!("/v1/optimization-studies/{study_id}/cancel"),Value::Null).await.0,StatusCode::CONFLICT);
    let run_id: i32=sqlx::query_scalar("select id from replay_runs where batch_id=$1").bind(batch_id as i32).fetch_one(&database.pool).await.unwrap();
    for url in [format!("/v1/replay-runs/{run_id}/cancel"),format!("/v1/replay-batches/{batch_id}/cancel")] {
        assert_eq!(request(&router,"POST",&url,Value::Null).await.0,StatusCode::CONFLICT);
    }
    let batch=setups_db::runs::get_replay_batch(&database.pool,batch_id).await.unwrap().unwrap();
    assert_eq!(batch.origin,"continuous");assert_eq!(batch.continuous_study_id,Some(study_id));
    let revision=db::get(&database.pool,ADMIN_OWNER,ALGORITHM).await.unwrap().unwrap().config_revision;
    let mut updated=config();updated["adaptiveEnabled"]=json!(false);
    let (status,disabled)=request(&router,"PUT",&path(""),updated.clone()).await;assert_eq!(status,StatusCode::OK);
    assert_eq!(disabled["configRevision"],revision);assert!(disabled["slots"][0]["activeGenerationId"].is_null());
    assert_eq!(disabled["slots"][0]["pendingGenerationId"],generation_id);
    assert!(db::active_adaptive_slots(&database.pool,ADMIN_OWNER).await.unwrap().is_empty());
    updated["adaptiveEnabled"]=json!(true);assert_eq!(request(&router,"PUT",&path(""),updated).await.0,StatusCode::OK);
    assert_eq!(promote_fixture_pending(&database.pool).await,generation_id);
    assert_eq!(db::active_adaptive_slots(&database.pool,ADMIN_OWNER).await.unwrap()[0].study_id,study_id);
    assert_eq!(request(&router,"POST",&path("/stop"),Value::Null).await.0,StatusCode::ACCEPTED);
    wait_sql(&database.pool,"select to_jsonb(p) from continuous_processes p where status='stopped'").await;
    let study=setups_db::optimization::get(&database.pool,study_id).await.unwrap().unwrap();assert_eq!(study.status,"completed");
    let rows=sqlx::query("select origin,status,jsonb_array_length(setups) n from replay_runs where batch_id=$1").bind(batch_id as i32).fetch_all(&database.pool).await.unwrap();
    assert_eq!(rows.len(),1);assert_eq!(rows[0].get::<String,_>("origin"),"continuous");assert_eq!(rows[0].get::<String,_>("status"),"done");assert!(rows[0].get::<i32,_>("n")>0);
    assert_eq!(setups_db::settings_get(&database.pool,"algorithm:price_volume_spike").await.unwrap(),fixed);
    assert_eq!(sqlx::query_scalar::<_,i64>("select count(*) from replay_cache").fetch_one(&database.pool).await.unwrap(),0);
    assert_eq!(sqlx::query_scalar::<_,i64>("select count(*) from analytics_matrix_cache").fetch_one(&database.pool).await.unwrap(),0);
    runtime.stop().await;market.task.abort();let _=market.task.await;database.cleanup().await;
}

#[tokio::test]
#[ignore="requires isolated TEST_DATABASE_URL ending in _test with CREATEDB"]
async fn continuous_acquisition_does_not_block_real_manual_admission_or_owner_stop() {
    let database=TestDb::new().await;let market=Market::start(true).await;
    let manual=start_optimization_runtime(database.pool.clone(),market.url.clone(),"key".into(),Default::default()).await.unwrap();
    let runtime=start_continuous_runtime(database.pool.clone(),market.url.clone(),"key".into(),ContinuousRuntimeConfig::default()).await.unwrap();
    let mut s=state(database.pool.clone());Arc::get_mut(&mut s).unwrap().notify=runtime.notify.clone();
    let router=continuous_router(s.clone()).merge(optimization_router(Arc::new(OptimizationState{pool:database.pool.clone(),auth:s.auth.clone(),config:Default::default(),notify:manual.notify.clone(),available:manual.available.clone()})));
    assert_eq!(request(&router,"PUT",&path(""),config()).await.0,StatusCode::OK);
    request(&router,"POST",&path("/start"),Value::Null).await;
    let generation=wait_sql(&database.pool,"select to_jsonb(g) from continuous_generations g where status='running'").await;
    let continuous_study=generation["study_id"].as_i64().unwrap();
    let mut spec=config()["study"].clone();let to=chrono::Utc::now().timestamp().div_euclid(60)*60;
    spec["fromUnix"]=json!(to-3600);spec["toUnix"]=json!(to);spec["minSetupsPerDay"]=json!(0);
    let (status,study)=request(&router,"POST","/v1/optimization-studies",spec).await;
    assert_eq!(status,StatusCode::ACCEPTED,"manual admission rejected during continuous work: {study}");
    let manual_id=study["id"].as_i64().unwrap();assert_eq!(study["origin"],"manual");
    assert_eq!(request(&router,"POST",&format!("/v1/optimization-studies/{continuous_study}/cancel"),Value::Null).await.0,StatusCode::CONFLICT);
    let stop_started=std::time::Instant::now();
    request(&router,"POST",&path("/stop"),Value::Null).await;
    wait_sql(&database.pool,"select to_jsonb(p) from continuous_processes p where status='stopped'").await;
    assert!(stop_started.elapsed()<Duration::from_secs(2),"authoritative Stop was delayed by monitor grace");
    assert!(setups_db::optimization::get(&database.pool,continuous_study).await.unwrap().is_none());
    assert!(setups_db::optimization::get(&database.pool,manual_id).await.unwrap().is_some());
    runtime.stop().await;manual.stop().await;market.task.abort();let _=market.task.await;database.cleanup().await;
}

#[tokio::test]
#[ignore="requires isolated TEST_DATABASE_URL ending in _test with CREATEDB"]
async fn liveness_does_not_wait_behind_own_materialization_lane_lock() {
    use setups_db::continuous::scheduler as scheduler;
    let database=TestDb::new().await;
    let normalized=normalize_config(serde_json::from_value(config()).unwrap(),ALGORITHM,&ContinuousRuntimeConfig::default().optimizer).unwrap();
    db::configure(&database.pool,ADMIN_OWNER,ALGORITHM,&json!(normalized),&[0.0]).await.unwrap();
    db::start(&database.pool,ADMIN_OWNER,ALGORITHM).await.unwrap();
    let lease=scheduler::claim_owner(&database.pool,"test-materializer").await.unwrap().unwrap();
    scheduler::recover_owner(&database.pool,&lease).await.unwrap();
    let job=scheduler::prepare_next(&database.pool,&lease).await.unwrap().unwrap();
    let mut materialization=database.pool.begin().await.unwrap();
    sqlx::query("select owner_id from continuous_owner_lanes where owner_id=$1 for update").bind(ADMIN_OWNER).fetch_one(&mut *materialization).await.unwrap();
    assert!(tokio::time::timeout(Duration::from_millis(200),scheduler::current(&database.pool,&job)).await.expect("liveness blocked behind own owner row").unwrap());
    materialization.rollback().await.unwrap();
    scheduler::release(&database.pool,&lease).await.unwrap();
    database.cleanup().await;
}

#[tokio::test]
#[ignore="requires isolated TEST_DATABASE_URL ending in _test with CREATEDB"]
async fn completed_nonpass_replaces_only_own_slot_and_keeps_previous_completed_children() {
    let database=TestDb::new().await;let market=Market::start(false).await;
    let runtime=start_continuous_runtime(database.pool.clone(),market.url.clone(),"key".into(),ContinuousRuntimeConfig::default()).await.unwrap();
    let router=continuous_router(state(database.pool.clone()));
    request(&router,"PUT",&path(""),config()).await;request(&router,"POST",&path("/start"),Value::Null).await;
    let first=wait_sql(&database.pool,"select to_jsonb(g) from continuous_generations g where accepted=true").await;
    market.state.blocked.store(true,Ordering::SeqCst);market.state.losing.store(true,Ordering::SeqCst);
    promote_fixture_pending(&database.pool).await;
    wait_sql(&database.pool,"select to_jsonb(g) from continuous_generations g where status='running'").await;
    assert_eq!(db::get(&database.pool,ADMIN_OWNER,ALGORITHM).await.unwrap().unwrap().slots[0].active_generation_id,first["id"].as_i64());
    market.state.blocked.store(false,Ordering::SeqCst);
    let nonpass=wait_sql(&database.pool,"select to_jsonb(g) from continuous_generations g where status='completed' and accepted=false").await;
    assert_eq!(nonpass["result"]["meanExpectancy"],-1.0);
    assert!(nonpass["result"]["recommendation"].is_null());
    assert!(db::get(&database.pool,ADMIN_OWNER,ALGORITHM).await.unwrap().unwrap().slots[0].active_generation_id.is_none());
    request(&router,"POST",&path("/stop"),Value::Null).await;
    wait_sql(&database.pool,"select to_jsonb(p) from continuous_processes p where status='stopped'").await;
    assert_eq!(sqlx::query_scalar::<_,i64>("select count(*) from replay_runs where status='done'").fetch_one(&database.pool).await.unwrap(),2);
    assert_eq!(sqlx::query_scalar::<_,i64>("select count(*) from optimization_studies where status='completed'").fetch_one(&database.pool).await.unwrap(),2);
    runtime.stop().await;market.task.abort();let _=market.task.await;database.cleanup().await;
}

#[tokio::test]
#[ignore="requires isolated TEST_DATABASE_URL ending in _test with CREATEDB"]
async fn transport_failure_keeps_accepted_slot_and_retries_are_bounded() {
    let database=TestDb::new().await;let market=Market::start(false).await;
    let runtime=start_continuous_runtime(database.pool.clone(),market.url.clone(),"key".into(),ContinuousRuntimeConfig::default()).await.unwrap();
    let router=continuous_router(state(database.pool.clone()));
    request(&router,"PUT",&path(""),config()).await;request(&router,"POST",&path("/start"),Value::Null).await;
    let first=wait_sql(&database.pool,"select to_jsonb(g) from continuous_generations g where accepted=true").await;
    market.state.failing.store(true,Ordering::SeqCst);
    promote_fixture_pending(&database.pool).await;
    wait_sql(&database.pool,"select to_jsonb(p) from continuous_processes p where retry_count=3 and status='error'").await;
    let process=db::get(&database.pool,ADMIN_OWNER,ALGORITHM).await.unwrap().unwrap();
    assert_eq!(process.slots[0].active_generation_id,first["id"].as_i64());
    assert_eq!(process.error.unwrap()["code"],"market_data_unavailable");
    let generations:i64=sqlx::query_scalar("select count(*) from continuous_generations").fetch_one(&database.pool).await.unwrap();
    assert_eq!(generations,4);
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(sqlx::query_scalar::<_,i64>("select count(*) from continuous_generations").fetch_one(&database.pool).await.unwrap(),generations);
    request(&router,"POST",&path("/stop"),Value::Null).await;
    wait_sql(&database.pool,"select to_jsonb(p) from continuous_processes p where status='stopped'").await;
    assert_eq!(sqlx::query_scalar::<_,i64>("select count(*) from optimization_studies where status='failed'").fetch_one(&database.pool).await.unwrap(),3,"Stop must preserve prior terminal failures");
    runtime.stop().await;market.task.abort();let _=market.task.await;database.cleanup().await;
}

#[tokio::test]
#[ignore="requires isolated TEST_DATABASE_URL ending in _test with CREATEDB"]
async fn whole_chains_round_robin_algorithms_and_thresholds_in_one_owner_lane() {
    let database=TestDb::new().await;let market=Market::start(false).await;
    for algorithm in [ALGORITHM,"price_drop_volume_spike"] {
        let mut body=config();body["minSetupsPerDay"]=json!([0,0.01]);body["study"]["algorithmId"]=json!(algorithm);
        if algorithm!=ALGORITHM {body["study"]["algorithmParams"].as_object_mut().unwrap().remove("priceGrowthPct");body["study"]["algorithmParams"]["priceDropPct"]=json!("0");}
        let config=normalize_config(serde_json::from_value(body).unwrap(),algorithm,&ContinuousRuntimeConfig::default().optimizer).unwrap();
        db::configure(&database.pool,ADMIN_OWNER,algorithm,&json!(config),&config.min_setups_per_day).await.unwrap();db::start(&database.pool,ADMIN_OWNER,algorithm).await.unwrap();
    }
    let runtime=start_continuous_runtime(database.pool.clone(),market.url.clone(),"key".into(),ContinuousRuntimeConfig::default()).await.unwrap();
    let rows=wait_sql(&database.pool,"select jsonb_agg(jsonb_build_object('algorithm',algorithm_id,'frequency',spec->'minSetupsPerDay') order by id) from (select id,algorithm_id,spec from continuous_generations where status='completed' order by id limit 4) x having count(*)=4").await;
    let rows=rows.as_array().unwrap();assert_eq!(rows.len(),4);
    assert_ne!(rows[0]["algorithm"],rows[1]["algorithm"]);assert_eq!(rows[0]["algorithm"],rows[2]["algorithm"]);assert_eq!(rows[1]["algorithm"],rows[3]["algorithm"]);
    assert_eq!(rows[0]["frequency"],0.0);assert_eq!(rows[1]["frequency"],0.0);assert_eq!(rows[2]["frequency"],0.01);assert_eq!(rows[3]["frequency"],0.01);
    for algorithm in [ALGORITHM,"price_drop_volume_spike"] {db::stop(&database.pool,ADMIN_OWNER,algorithm).await.unwrap();}
    wait_sql(&database.pool,"select jsonb_build_object('stopped',true) where not exists(select 1 from continuous_processes where status<>'stopped')").await;
    runtime.stop().await;market.task.abort();let _=market.task.await;database.cleanup().await;
}

#[tokio::test]
#[ignore="requires isolated TEST_DATABASE_URL ending in _test with CREATEDB"]
async fn delayed_service_materialization_keeps_heartbeat_renewing_independently() {
    let database=TestDb::new().await;let market=Market::start(false).await;
    sqlx::raw_sql("create function delay_continuous_insert() returns trigger language plpgsql as $$begin perform pg_sleep(2.5);return new;end$$;create trigger delay_continuous_insert before insert on replay_runs for each row when(new.origin='continuous') execute function delay_continuous_insert()")
        .execute(&database.pool).await.unwrap();
    let runtime=start_continuous_runtime(database.pool.clone(),market.url.clone(),"key".into(),ContinuousRuntimeConfig::default()).await.unwrap();
    let router=continuous_router(state(database.pool.clone()));
    request(&router,"PUT",&path(""),config()).await;request(&router,"POST",&path("/start"),Value::Null).await;
    wait_sql(&database.pool,"select to_jsonb(g) from continuous_generations g where phase='replay'").await;
    let original:f64=sqlx::query_scalar("select extract(epoch from lease_until)::double precision from continuous_owner_lanes where owner_id='admin'").fetch_one(&database.pool).await.unwrap();
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let later:f64=sqlx::query_scalar("select extract(epoch from lease_until)::double precision from continuous_owner_lanes where owner_id='admin'").fetch_one(&database.pool).await.unwrap();
    request(&router,"POST",&path("/stop"),Value::Null).await;
    runtime.stop().await;market.task.abort();let _=market.task.await;database.cleanup().await;
    assert!(later>original+0.5,"lease heartbeat waited behind own materialization: original={original} later={later}");
}

#[tokio::test]
#[ignore="requires isolated TEST_DATABASE_URL ending in _test with CREATEDB"]
async fn idle_algorithm_stop_drains_and_can_reconfigure_while_other_algorithm_is_acquiring() {
    let database=TestDb::new().await;let market=Market::start(true).await;
    let runtime=start_continuous_runtime(database.pool.clone(),market.url.clone(),"key".into(),ContinuousRuntimeConfig::default()).await.unwrap();
    let router=continuous_router(state(database.pool.clone()));
    request(&router,"PUT",&path(""),config()).await;request(&router,"POST",&path("/start"),Value::Null).await;
    let active=wait_sql(&database.pool,"select to_jsonb(g) from continuous_generations g where status='running'").await;
    let algorithm="price_drop_volume_spike";let other=format!("/v1/continuous-studies/{algorithm}");
    let mut body=config();body["study"]["algorithmId"]=json!(algorithm);
    body["study"]["algorithmParams"].as_object_mut().unwrap().remove("priceGrowthPct");body["study"]["algorithmParams"]["priceDropPct"]=json!("0");
    assert_eq!(request(&router,"PUT",&other,body.clone()).await.0,StatusCode::OK);
    request(&router,"POST",&format!("{other}/start"),Value::Null).await;request(&router,"POST",&format!("{other}/stop"),Value::Null).await;
    tokio::time::timeout(Duration::from_secs(2),wait_sql(&database.pool,"select to_jsonb(p) from continuous_processes p where algorithm_id='price_drop_volume_spike' and status='stopped'")).await.expect("idle Stop waited for other algorithm's acquisition");
    assert_eq!(setups_db::optimization::get(&database.pool,active["study_id"].as_i64().unwrap()).await.unwrap().unwrap().status,"running");
    body["minSetupsPerDay"]=json!([1]);assert_eq!(request(&router,"PUT",&other,body).await.0,StatusCode::OK);
    assert_eq!(request(&router,"POST",&format!("{other}/start"),Value::Null).await.0,StatusCode::ACCEPTED);
    request(&router,"POST",&format!("{other}/stop"),Value::Null).await;request(&router,"POST",&path("/stop"),Value::Null).await;
    wait_sql(&database.pool,"select jsonb_build_object('stopped',true) where not exists(select 1 from continuous_processes where status<>'stopped')").await;
    runtime.stop().await;market.task.abort();let _=market.task.await;database.cleanup().await;
}

#[tokio::test]
#[ignore="requires isolated TEST_DATABASE_URL ending in _test with CREATEDB"]
async fn exhausted_continuous_storage_is_explicit_and_does_not_read_market_or_delete_history() {
    let database=TestDb::new().await;let market=Market::start(false).await;
    let limits=ContinuousRuntimeConfig{max_database_bytes:1,..Default::default()};
    let runtime=start_continuous_runtime(database.pool.clone(),market.url.clone(),"key".into(),limits).await.unwrap();
    let router=continuous_router(state(database.pool.clone()));
    request(&router,"PUT",&path(""),config()).await;request(&router,"POST",&path("/start"),Value::Null).await;
    wait_sql(&database.pool,"select to_jsonb(p) from continuous_processes p where status='error' and error->>'code'='resource_limit'").await;
    assert_eq!(market.state.catalog_calls.load(Ordering::SeqCst),0);
    assert_eq!(market.state.candle_calls.load(Ordering::SeqCst),0);
    assert_eq!(sqlx::query_scalar::<_,i64>("select count(*) from optimization_studies where status='failed'").fetch_one(&database.pool).await.unwrap(),1);
    request(&router,"POST",&path("/stop"),Value::Null).await;
    wait_sql(&database.pool,"select to_jsonb(p) from continuous_processes p where status='stopped'").await;
    assert_eq!(sqlx::query_scalar::<_,i64>("select count(*) from optimization_studies where status='failed'").fetch_one(&database.pool).await.unwrap(),1);
    runtime.stop().await;market.task.abort();let _=market.task.await;database.cleanup().await;
}

// Exercise the actual acquisition/monitor path, not a simulated CancelToken.
async fn monitor_recovers_from_short_database_stall(stall: &str) {
    let database=TestDb::new().await;
    let market=Market::start(true).await;
    let runtime_pool=PgPoolOptions::new().max_connections(4)
        .connect_with(database.pool.connect_options().as_ref().clone()).await.unwrap();
    let runtime=start_continuous_runtime(runtime_pool.clone(),market.url.clone(),"key".into(),ContinuousRuntimeConfig::default()).await.unwrap();
    let router=continuous_router(state(database.pool.clone()));
    let mut body=config();body["study"]["maxSeconds"]=json!(30);
    assert_eq!(request(&router,"PUT",&path(""),body).await.0,StatusCode::OK);
    request(&router,"POST",&path("/start"),Value::Null).await;
    let generation=wait_sql(&database.pool,"select to_jsonb(g) from continuous_generations g where status='running'").await;
    let id=generation["id"].as_i64().unwrap();
    tokio::time::timeout(Duration::from_secs(3),async {
        while market.state.catalog_calls.load(Ordering::SeqCst)==0 {tokio::time::sleep(Duration::from_millis(10)).await;}
    }).await.unwrap();
    let mut locked=None;
    let mut held=Vec::new();
    match stall {
        "heartbeat_lock"=>{
            let mut tx=database.pool.begin().await.unwrap();
            sqlx::query("select owner_id from continuous_owner_lanes where owner_id='admin' for update").fetch_one(&mut *tx).await.unwrap();
            locked=Some(tx);
        }
        "pool"=>for _ in 0..4 {held.push(runtime_pool.acquire().await.unwrap());},
        "heartbeat_error"=>{
            sqlx::raw_sql("create function reject_test_heartbeat() returns trigger language plpgsql as $$begin raise exception 'fixture transient heartbeat' using errcode='40001';end$$;create trigger reject_test_heartbeat before update of lease_until on continuous_owner_lanes for each row when(new.lease_until>old.lease_until) execute function reject_test_heartbeat()")
                .execute(&database.pool).await.unwrap();
        }
        _=>panic!("unknown fixture stall"),
    }
    tokio::time::sleep(Duration::from_millis(3500)).await;
    if let Some(tx)=locked {tx.rollback().await.unwrap();}
    drop(held);
    if stall=="heartbeat_error" {sqlx::query("drop trigger reject_test_heartbeat on continuous_owner_lanes").execute(&database.pool).await.unwrap();}
    tokio::time::sleep(Duration::from_millis(500)).await;
    let after:String=sqlx::query_scalar("select status from continuous_generations where id=$1").bind(id).fetch_one(&database.pool).await.unwrap();
    market.state.blocked.store(false,Ordering::SeqCst);
    let terminal=wait_sql(&database.pool,&format!("select to_jsonb(g) from continuous_generations g where id={id} and status in ('completed','failed')")).await;
    db::stop(&database.pool,ADMIN_OWNER,ALGORITHM).await.unwrap();
    runtime.stop().await;market.task.abort();let _=market.task.await;
    runtime_pool.close().await;database.cleanup().await;
    assert_eq!(after,"running","{stall}: a short monitor failure cancelled valid acquisition");
    assert_eq!(terminal["status"],"completed","{stall}: acquisition did not recover: {terminal}");
}

#[tokio::test]
#[ignore="requires isolated TEST_DATABASE_URL ending in _test with CREATEDB"]
async fn monitor_recovers_from_heartbeat_query_timeout() {monitor_recovers_from_short_database_stall("heartbeat_lock").await;}

#[tokio::test]
#[ignore="requires isolated TEST_DATABASE_URL ending in _test with CREATEDB"]
async fn monitor_recovers_from_current_pool_timeout() {monitor_recovers_from_short_database_stall("pool").await;}

#[tokio::test]
#[ignore="requires isolated TEST_DATABASE_URL ending in _test with CREATEDB"]
async fn monitor_recovers_from_transient_heartbeat_database_error() {monitor_recovers_from_short_database_stall("heartbeat_error").await;}

#[tokio::test]
#[ignore="requires isolated TEST_DATABASE_URL ending in _test with CREATEDB"]
async fn slow_idle_stop_housekeeping_does_not_starve_active_heartbeat() {
    let database=TestDb::new().await;let market=Market::start(true).await;
    let runtime=start_continuous_runtime(database.pool.clone(),market.url.clone(),"key".into(),ContinuousRuntimeConfig::default()).await.unwrap();
    let router=continuous_router(state(database.pool.clone()));
    let mut body=config();body["study"]["maxSeconds"]=json!(30);
    request(&router,"PUT",&path(""),body.clone()).await;
    request(&router,"POST",&path("/start"),Value::Null).await;
    let generation=wait_sql(&database.pool,"select to_jsonb(g) from continuous_generations g where status='running'").await;
    let id=generation["id"].as_i64().unwrap();
    let other="price_drop_volume_spike";
    body["study"]["algorithmId"]=json!(other);
    body["study"]["algorithmParams"].as_object_mut().unwrap().remove("priceGrowthPct");
    body["study"]["algorithmParams"]["priceDropPct"]=json!("0");
    let normalized=normalize_config(serde_json::from_value(body).unwrap(),other,&ContinuousRuntimeConfig::default().optimizer).unwrap();
    db::configure(&database.pool,ADMIN_OWNER,other,&json!(normalized),&[0.0]).await.unwrap();
    db::start(&database.pool,ADMIN_OWNER,other).await.unwrap();
    sqlx::raw_sql("create function slow_test_idle_stop() returns trigger language plpgsql as $$begin perform pg_sleep(4);return new;end$$;create trigger slow_test_idle_stop before update of status on continuous_processes for each row when(new.algorithm_id='price_drop_volume_spike' and new.status='stopped' and old.status='stopping') execute function slow_test_idle_stop()")
        .execute(&database.pool).await.unwrap();
    let before:f64=sqlx::query_scalar("select extract(epoch from lease_until)::double precision from continuous_owner_lanes where owner_id='admin'").fetch_one(&database.pool).await.unwrap();
    db::stop(&database.pool,ADMIN_OWNER,other).await.unwrap();
    tokio::time::sleep(Duration::from_millis(2700)).await;
    let after:f64=sqlx::query_scalar("select extract(epoch from lease_until)::double precision from continuous_owner_lanes where owner_id='admin'").fetch_one(&database.pool).await.unwrap();
    sqlx::query("drop trigger slow_test_idle_stop on continuous_processes").execute(&database.pool).await.unwrap();
    wait_sql(&database.pool,"select to_jsonb(p) from continuous_processes p where algorithm_id='price_drop_volume_spike' and status='stopped'").await;
    market.state.blocked.store(false,Ordering::SeqCst);
    let terminal=wait_sql(&database.pool,&format!("select to_jsonb(g) from continuous_generations g where id={id} and status in ('completed','failed')")).await;
    db::stop(&database.pool,ADMIN_OWNER,ALGORITHM).await.unwrap();
    runtime.stop().await;market.task.abort();let _=market.task.await;database.cleanup().await;
    assert!(after>before+1.0,"idle housekeeping starved heartbeat: before={before} after={after}");
    assert_eq!(terminal["status"],"completed","idle housekeeping cancelled the active job: {terminal}");
}
