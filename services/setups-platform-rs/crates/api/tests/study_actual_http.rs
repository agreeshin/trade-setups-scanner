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
        // pg_dump 17's transaction_timeout is not available on the isolated PG16 fixture.
        let baseline=include_str!("../../db/migrations/0010_baseline.sql").lines().filter(|line|!line.starts_with('\\')&&!line.starts_with("SET transaction_timeout")&&!line.contains("set_config('search_path'")).collect::<Vec<_>>().join("\n");
        sqlx::raw_sql(&baseline).execute(&child).await.unwrap();
        sqlx::raw_sql("set search_path to public;create table kysely_migration(name varchar(255) primary key,timestamp varchar(255) not null);insert into kysely_migration values('0009_legacy','fixture')").execute(&child).await.unwrap();
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
#[test]
fn materialize_command_help_and_invalid_arguments_do_not_start_server_or_require_environment() {
    let executable=env!("CARGO_BIN_EXE_setups-platform-server");
    let help=std::process::Command::new(executable).args(["materialize-study-original","--help"]).env_clear().output().unwrap();
    assert!(help.status.success());let text=String::from_utf8(help.stdout).unwrap();assert!(text.contains("Default: bounded read-only calculation"));assert!(text.contains("[--publish]"));
    let invalid=std::process::Command::new(executable).args(["materialize-study-original","--study-id","-1"]).env_clear().output().unwrap();
    assert_eq!(invalid.status.code(),Some(2));assert!(String::from_utf8(invalid.stderr).unwrap().contains("verification is required"));
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

fn unix(text:&str)->i64 {chrono::DateTime::parse_from_rfc3339(text).unwrap().timestamp()}

async fn original_ledger_fixture(pool:&PgPool,start:i64,points:Vec<setups_db::study_display::OriginalMinute>)->(i64,i64,Value) {
    use setups_domain::{RecommendationProvenance,RecommendationDirection,expectancy::*};
    let id=seed(pool,start).await;
    let tp:u64=points.iter().map(|p|p.tp_count).sum();let sl:u64=points.iter().map(|p|p.sl_count).sum();let count=tp+sl;
    let mean=(tp as f64-sl as f64)/count as f64;
    let mut result:Value=sqlx::query_scalar("select result from optimization_studies where id=$1").bind(id).fetch_one(pool).await.unwrap();
    result["best"]["provenance"]["snapshotHash"]=json!("f".repeat(64));
    result["best"]["score"]=json!(mean);let cell=&mut result["best"]["matrix"][0];
    cell["riskExpectancy"]=json!(mean);cell["closedCount"]=json!(count);cell["tpCount"]=json!(tp);cell["slCount"]=json!(sl);
    sqlx::query("update optimization_studies set result=$2 where id=$1").bind(id).bind(result).execute(pool).await.unwrap();
    let mut utc=std::collections::BTreeMap::<i64,(u64,u64)>::new();
    for p in &points {let d=utc.entry(p.signal_close_unix.div_euclid(86400)*86400).or_default();d.0+=p.tp_count;d.1+=p.sl_count;}
    let key=format!("{id:064x}");
    let artifact=build_expectancy_artifact(ExpectancyInput {
        source:ExpectancySource{study_id:id.to_string(),derived_snapshot_id:key.clone(),generation_id:Some("777".into()),profile_id:Some("1".into()),
            provenance:RecommendationProvenance{algorithm_id:"price_volume_spike".into(),algorithm_version:"1.1.1".into(),optimizer_version:"plan-a-3".into(),seed:7,snapshot_hash:"f".repeat(64),captured_at:"2026-11-20T00:00:00Z".into(),from_unix:start-7*86400,to_unix:start-60,observation_cutoff:start-60},
            direction:RecommendationDirection::Short,tp_pct:10.0,sl_pct:10.0,forecast:None,
            analytics:ExpectancyAnalytics{cluster_enabled:false,cluster_gap_minutes:None,time_shift_minutes:1,price_change_pct:None,horizon_minutes:10080}},
        selected_cell:Some(SelectedCellInput{raw_r:Some(mean),tp_count:tp,sl_count:sl}),matrix_mean_r:Some(mean),
        daily:MetricAvailability::Available{value:utc.into_iter().map(|(day,(tp,sl))|UtcDailyInput{day_start_unix:day,row_count:tp+sl,tp_count:tp,sl_count:sl,mean_r:Some((tp as f64-sl as f64)/(tp+sl) as f64)}).collect()},
    }).unwrap();
    setups_db::study_display::validate_minutes(&artifact,&points).unwrap();
    let artifact_id:i64=sqlx::query_scalar("insert into study_expectancy_artifacts(artifact_key,owner_id,source_generation_id,source_study_id,source_slot_id,calculation_version,tp_pct,sl_pct,identity,numerical_snapshot) values($1,'admin',777,$2,1,'2',10,10,'{}',$3) returning id")
        .bind(&key).bind(id).bind(json!(artifact)).fetch_one(pool).await.unwrap();
    use sha2::{Sha256,Digest};let hash=format!("{:x}",Sha256::digest(serde_json::to_vec(&points).unwrap()));
    sqlx::query("insert into study_original_outcomes(artifact_id,source_key,kernel,minutes,payload_sha256,payload_bytes) values($1,$2,'original-historical-close-1',$3,$4,octet_length($3::jsonb::text))")
        .bind(artifact_id).bind(key).bind(json!(points)).bind(hash).execute(pool).await.unwrap();
    let spec:Value=sqlx::query_scalar("select spec from optimization_studies where id=$1").bind(id).fetch_one(pool).await.unwrap();
    (id,artifact_id,spec)
}

#[tokio::test] #[ignore="requires isolated local TEST_DATABASE_URL ending _test with CREATEDB"]
async fn exact_display_original_only_weighted_merge_and_refresh_preservation_wire() {isolated(|pool|async move {
    use setups_db::study_display::OriginalMinute;
    let start=unix("2026-11-20T23:30:00Z");
    let points=vec![OriginalMinute{signal_close_unix:unix("2026-11-20T19:00:00Z"),sum_r:2.0,closed_count:2,tp_count:2,sl_count:0},
        OriginalMinute{signal_close_unix:unix("2026-11-20T23:00:00Z"),sum_r:0.0,closed_count:2,tp_count:1,sl_count:1}];
    let (id,artifact,spec)=original_ledger_fixture(&pool,start,points).await;
    sqlx::query("update optimization_studies set execution_started_at=to_timestamp($2) where id=$1").bind(id).bind(start as f64+0.25).execute(&pool).await.unwrap();
    let immutable:(String,String)=sqlx::query_as("select s.result::text,a.numerical_snapshot::text from optimization_studies s join study_expectancy_artifacts a on a.source_study_id=s.id where s.id=$1").bind(id).fetch_one(&pool).await.unwrap();
    let (app,_,market,server)=fixture(&pool,start).await;let path=format!("/v1/optimization-studies/{id}/actual");let zoned=format!("{path}?timeZone=Asia%2FKathmandu");
    let (_,legacy)=http(&app,"GET",&path).await;assert!(legacy.get("originalReport").is_none());assert!(legacy["report"].is_null());
    let (status,before)=http(&app,"GET",&zoned).await;assert_eq!(status,StatusCode::OK,"{before}");assert_eq!(before["status"],"not_calculated");assert!(before["report"].is_null());
    let original=&before["originalReport"];let display=&original["display"];
    assert_eq!(original["executionStartedAt"],"2026-11-20T23:30:00.250Z");assert_eq!(display["timeZone"],"Asia/Kathmandu");
    assert_eq!(display["startDayUnix"],unix("2026-11-20T18:15:00Z"));assert_eq!(display["daily"].as_array().unwrap().len(),1);
    assert_eq!(display["daily"][0]["date"],"2026-11-21");assert_eq!(display["daily"][0]["closedCount"],4);assert_eq!(display["daily"][0]["sumR"],2.0);assert_eq!(display["daily"][0]["meanR"],0.5);
    assert_eq!(display["historical"]["availability"]["status"],"available");assert_eq!(display["historical"]["complete"],true);assert_eq!(display["historical"]["fidelity"],"native_original");assert_eq!(display["historical"]["rawReplayAvailable"],false);
    assert!(db::get(&pool,"admin",id).await.unwrap().is_none(),"GET must not admit replay");
    let identity:String=sqlx::query_scalar("select md5(spec::text||coalesce(result::text,'null')) from optimization_studies where id=$1").bind(id).fetch_one(&pool).await.unwrap();
    let source=json!({"algorithmId":"price_volume_spike","algorithmVersion":"1.1.1","params":spec["algorithmParams"],"startUnix":start as f64+0.25,"instruments":spec["instruments"]});
    let limits=setups_db::admission::ReplayAdmissionLimits::default();
    let admit=|job:&'static str|db::Request{owner:"admin",study:id,job,start:chrono::DateTime::from_timestamp(start,250_000_000).unwrap(),cutoff:start+7200,source:&source,identity:&identity,work:60,limits:&limits};
    assert!(matches!(db::admit(&pool,admit("display-first")).await.unwrap(),db::Admission::Created(_)));
    let job=db::claim(&pool,2).await.unwrap().unwrap();let batch=db::create_batch(&pool,&job).await.unwrap().unwrap();
    let rows=vec![json!({"setupTimeUnix":unix("2026-11-21T00:30:00Z"),"entryTimeUnix":unix("2026-11-21T00:31:00Z"),"status":"sl","riskReturn":-1.0})];
    assert!(db::append(&pool,&job,batch,db::InstrumentOutput{exchange:"fixture",category:"linear",symbol:"FIX",emitted:&json!([]),rows:&rows,quality:&json!({})}).await.unwrap());
    let mut report=original.clone();report.as_object_mut().unwrap().remove("display");report["observedThroughUnix"]=json!(start+7200);
    assert!(db::finish(&pool,&job,&report,&json!({"total":1,"closed":1,"tp":0,"sl":1,"gateFailed":0,"unclosed":0,"noData":0,"meanR":-1.0})).await.unwrap());
    let (status,completed)=http(&app,"GET",&zoned).await;assert_eq!(status,StatusCode::OK,"{completed}");let merged=&completed["report"]["display"];
    assert_eq!(merged["daily"].as_array().unwrap().len(),1);assert_eq!(merged["daily"][0]["closedCount"],5);assert_eq!(merged["daily"][0]["tpCount"],3);assert_eq!(merged["daily"][0]["slCount"],2);
    assert_eq!(merged["daily"][0]["sumR"],1.0);assert_eq!(merged["daily"][0]["meanR"],0.2);assert_eq!(completed["originalReport"],*original);
    for key in ["originalObjective","originalEndpoint","projection","executionStartedAt"] {assert_eq!(completed["report"][key],report[key],"original model field {key} changed");}
    let (_,old_completed)=http(&app,"GET",&path).await;assert_eq!(old_completed["report"],report,"without timeZone saved wire report stays untouched");assert!(old_completed.get("originalReport").is_none());
    assert!(matches!(db::admit(&pool,admit("display-replacement")).await.unwrap(),db::Admission::Created(_)));
    let (_,queued)=http(&app,"GET",&zoned).await;assert_eq!(queued["status"],"queued");assert!(queued["report"].is_null());assert_eq!(queued["originalReport"],*original);
    let replacement=db::claim(&pool,2).await.unwrap().unwrap();let (_,running)=http(&app,"GET",&zoned).await;assert_eq!(running["status"],"running");assert_eq!(running["originalReport"],*original);
    db::fail(&pool,&replacement,"fixture_failure").await.unwrap();let (_,failed)=http(&app,"GET",&zoned).await;assert_eq!(failed["status"],"failed");assert!(failed["report"].is_null());assert_eq!(failed["originalReport"],*original);
    assert_eq!(sqlx::query_scalar::<_,i64>("select count(*) from study_original_outcomes where artifact_id=$1").bind(artifact).fetch_one(&pool).await.unwrap(),1);
    assert_eq!(sqlx::query_as::<_,(String,String)>("select s.result::text,a.numerical_snapshot::text from optimization_studies s join study_expectancy_artifacts a on a.source_study_id=s.id where s.id=$1").bind(id).fetch_one(&pool).await.unwrap(),immutable);
    assert_eq!(market.calls.load(Ordering::SeqCst),0,"GET must never acquire market history");
    if let Ok(path)=std::env::var("STUDY_DISPLAY_WIRE_PATH") {std::fs::write(path,serde_json::to_vec_pretty(&json!({"before":before,"completed":completed,"queued":queued,"running":running,"failed":failed})).unwrap()).unwrap();}
    market.stop.cancel();server.abort();let _=server.await;
}).await;}

#[tokio::test] #[ignore="requires isolated local TEST_DATABASE_URL ending _test with CREATEDB"]
async fn exact_display_dst_iana_midnights_old_original_and_unavailable_legacy() {isolated(|pool|async move {
    use setups_db::study_display::OriginalMinute;
    for (zone,times,first,next,hours) in [
        ("America%2FNew_York",["2026-03-08T06:30:00Z","2026-03-09T03:30:00Z","2026-03-09T04:30:00Z"],"2026-03-08T05:00:00Z","2026-03-09T04:00:00Z",23),
        ("America%2FNew_York",["2026-11-01T05:30:00Z","2026-11-01T06:30:00Z","2026-11-02T05:30:00Z"],"2026-11-01T04:00:00Z","2026-11-02T05:00:00Z",25),
        ("America%2FHavana",["2026-11-01T04:30:00Z","2026-11-01T05:30:00Z","2026-11-02T05:30:00Z"],"2026-11-01T04:00:00Z","2026-11-02T05:00:00Z",25),
    ] {
        let start=unix(times[2])+7200;let points=times.into_iter().map(|t|OriginalMinute{signal_close_unix:unix(t),sum_r:1.0,closed_count:1,tp_count:1,sl_count:0}).collect();
        let (id,artifact,_)=original_ledger_fixture(&pool,start,points).await;let (app,_,market,server)=fixture(&pool,start).await;
        let path=format!("/v1/optimization-studies/{id}/actual?timeZone={zone}");let (status,detail)=http(&app,"GET",&path).await;assert_eq!(status,StatusCode::OK,"{detail}");
        let daily=detail["originalReport"]["display"]["daily"].as_array().unwrap();assert_eq!(daily.len(),2);assert_eq!(daily[0]["dayUnix"],unix(first));assert_eq!(daily[1]["dayUnix"],unix(next));
        assert_eq!(unix(next)-unix(first),hours*3600);assert_eq!(daily[0]["closedCount"],2);assert_eq!(daily[1]["closedCount"],1);
        if start<unix("2026-04-01T00:00:00Z") {assert_eq!(detail["eligibility"]["eligible"],false);assert_eq!(detail["eligibility"]["reason"],"study_actual_period_budget_exceeded","original-only viewing must not enable over-budget refresh");}
        assert_eq!(daily.iter().map(|d|d["sumR"].as_f64().unwrap()).sum::<f64>(),3.0);
        let (bad,error)=http(&app,"GET",&format!("/v1/optimization-studies/{id}/actual?timeZone=Mars%2FOlympus")).await;assert_eq!(bad,StatusCode::BAD_REQUEST);assert_eq!(error["error"],"invalid_time_zone");
        sqlx::query("delete from study_original_outcomes where artifact_id=$1").bind(artifact).execute(&pool).await.unwrap();
        let (_,missing)=http(&app,"GET",&path).await;assert_eq!(missing["originalReport"]["display"]["historical"]["availability"]["status"],"unavailable");assert_eq!(missing["originalReport"]["display"]["historical"]["reason"],"original_outcomes_not_persisted");assert_eq!(missing["originalReport"]["display"]["daily"],json!([]));
        assert!(missing["originalReport"]["historical"]["daily"].as_array().unwrap().len()>0,"old sealed UTC model history is not relabeled as exact local-day data");
        assert_eq!(market.calls.load(Ordering::SeqCst),0);market.stop.cancel();server.abort();let _=server.await;
    }
}).await;}

#[tokio::test] #[ignore="requires isolated local TEST_DATABASE_URL ending _test with CREATEDB"]
async fn exact_display_verified_reconstruction_is_separate_fenced_immutable_publication() {isolated(|pool|async move {
    use setups_db::study_display::{self as display,OriginalMinute,VerifiedOriginalIdentity};
    let start=unix("2026-11-20T23:30:00Z");let points=vec![OriginalMinute{signal_close_unix:start-3600,sum_r:0.0,closed_count:2,tp_count:1,sl_count:1}];
    let (id,artifact,_)=original_ledger_fixture(&pool,start,points.clone()).await;
    let immutable:(String,String)=sqlx::query_as("select s.result::text,a.numerical_snapshot::text from optimization_studies s join study_expectancy_artifacts a on a.source_study_id=s.id where s.id=$1").bind(id).fetch_one(&pool).await.unwrap();
    let identity=VerifiedOriginalIdentity{owner_id:"admin".into(),study_id:id,artifact_id:artifact,artifact_key:format!("{id:064x}"),
        study_identity:sqlx::query_scalar("select md5(spec::text||coalesce(result::text,'null')) from optimization_studies where id=$1").bind(id).fetch_one(&pool).await.unwrap(),
        original_snapshot_hash:"f".repeat(64),source_hash:"1".repeat(64),input_hash:"2".repeat(64),proof_hash:"3".repeat(64),tp_pct:10.0,sl_pct:10.0};
    let evidence=json!({"fidelity":"verified_original_reconstruction","scope":"closed_outcomes_only","conservationMatched":true,"evaluator":"original-historical-close-1",
        "firstCanonicalAddOnlyVerified":true,"canonicalOhlcPreservedVerified":true,"historicalCloseKernelCompatibleVerified":true,"writerAuditHash":"4".repeat(64),
        "sourceHash":identity.source_hash,"inputHash":identity.input_hash,"proofHash":identity.proof_hash,"originalSnapshotHash":identity.original_snapshot_hash,"originalArtifactKey":identity.artifact_key,"studyIdentity":identity.study_identity});
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
    assert_eq!(sqlx::query_as::<_,(String,String)>("select s.result::text,a.numerical_snapshot::text from optimization_studies s join study_expectancy_artifacts a on a.source_study_id=s.id where s.id=$1").bind(id).fetch_one(&pool).await.unwrap(),immutable);
    assert_eq!(market.calls.load(Ordering::SeqCst),0);market.stop.cancel();server.abort();let _=server.await;
}).await;}

#[tokio::test] #[ignore="requires isolated local TEST_DATABASE_URL ending _test with CREATEDB"]
async fn exact_display_havana_ambiguous_midnight_marker_matches_earliest_grid_wire() {isolated(|pool|async move {
    use setups_db::study_display::OriginalMinute;
    let start=unix("2026-11-01T05:40:00Z");let points=vec![OriginalMinute{signal_close_unix:unix("2026-11-01T04:05:00Z"),sum_r:1.0,closed_count:1,tp_count:1,sl_count:0},
        OriginalMinute{signal_close_unix:unix("2026-11-01T05:05:00Z"),sum_r:1.0,closed_count:1,tp_count:1,sl_count:0}];
    let (id,_,_)=original_ledger_fixture(&pool,start,points).await;let (app,_,market,server)=fixture(&pool,start).await;
    let (status,detail)=http(&app,"GET",&format!("/v1/optimization-studies/{id}/actual?timeZone=America%2FHavana")).await;assert_eq!(status,StatusCode::OK,"{detail}");
    let report=&detail["originalReport"];assert_eq!(report["executionStartedAt"],"2026-11-01T05:40:00Z");
    assert_eq!(report["display"]["startDayUnix"],unix("2026-11-01T04:00:00Z"));assert_eq!(report["display"]["daily"][0]["dayUnix"],report["display"]["startDayUnix"]);
    assert_eq!(report["display"]["daily"][0]["closedCount"],2);
    if let Ok(path)=std::env::var("STUDY_DISPLAY_FOLD_WIRE_PATH") {std::fs::write(path,serde_json::to_vec_pretty(&detail).unwrap()).unwrap();}
    assert_eq!(market.calls.load(Ordering::SeqCst),0);market.stop.cancel();server.abort();let _=server.await;
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
