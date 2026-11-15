use axum::{Router,body::Body,http::{Request,StatusCode}};
use futures::future::BoxFuture;
use http_body_util::BodyExt;
use serde_json::{Value,json};
use setups_api::{SessionAuth,telegram::{TelegramState,TelegramTransport,telegram_router}};
use setups_db::telegram::{self as db,TelegramConfig,TelegramStatusUpdate,TelegramTestAttempt,TelegramTestResult,TelegramReason};
use sqlx::{PgPool,postgres::{PgConnectOptions,PgPoolOptions}};
use std::{str::FromStr,sync::{Arc,atomic::{AtomicBool,AtomicUsize,Ordering}}};
use tower::ServiceExt;
struct Transport {
    pool:PgPool,configured:bool,probes:AtomicUsize,tests:AtomicUsize,switch:AtomicBool,
    delay_test:AtomicBool,test_started:tokio::sync::Notify,test_release:tokio::sync::Notify,
    test_posts:AtomicUsize,test_target:std::sync::Mutex<Option<String>>,
}
impl TelegramTransport for Transport {
    fn secret_configured(&self)->bool {self.configured}
    fn probe<'a>(&'a self,config:&'a TelegramConfig)->BoxFuture<'a,TelegramStatusUpdate> {Box::pin(async move {
        self.probes.fetch_add(1,Ordering::SeqCst);
        if self.switch.swap(false,Ordering::SeqCst) {db::set_config(&self.pool,config.revision,true,"https://new.example").await.unwrap();}
        TelegramStatusUpdate {relay_authenticated:true,telegram_ready:Some(false),reason:Some(TelegramReason::new("telegram_unavailable","Telegram is unavailable."))}
    })}
    fn test<'a>(&'a self,attempt:&'a TelegramTestAttempt)->BoxFuture<'a,TelegramTestResult> {Box::pin(async move {
        self.tests.fetch_add(1,Ordering::SeqCst);
        if self.delay_test.load(Ordering::SeqCst) {self.test_started.notify_one();self.test_release.notified().await;}
        *self.test_target.lock().unwrap()=Some(attempt.config.endpoint_url.clone());self.test_posts.fetch_add(1,Ordering::SeqCst);
        let raw:Value=serde_json::from_slice(&attempt.request_body).unwrap();assert_eq!(raw["text"],"Telegram relay delivery test.");assert_eq!(raw["id"],attempt.delivery_id);
        TelegramTestResult {request_id:attempt.request_id.clone(),state:"unknown".into(),reason:Some(TelegramReason::new("delivery_unconfirmed","Delivery could not be confirmed."))}
    })}
}
fn state(pool:PgPool,configured:bool)->(Arc<TelegramState>,Arc<Transport>) {
    let transport=Arc::new(Transport {pool:pool.clone(),configured,probes:AtomicUsize::new(0),tests:AtomicUsize::new(0),switch:AtomicBool::new(false),delay_test:AtomicBool::new(false),test_started:tokio::sync::Notify::new(),test_release:tokio::sync::Notify::new(),test_posts:AtomicUsize::new(0),test_target:std::sync::Mutex::new(None)});
    (Arc::new(TelegramState {pool,auth:Arc::new(SessionAuth::new(&["test-key".into()])),transport:transport.clone()}),transport)
}
async fn request(router:&Router,method:&str,path:&str,body:Value)->(StatusCode,Value) {
    let response=router.clone().oneshot(Request::builder().method(method).uri(path).header("x-api-key","test-key").header("content-type","application/json").body(Body::from(body.to_string())).unwrap()).await.unwrap();
    assert_eq!(response.headers()["cache-control"],"no-store");let status=response.status();let bytes=response.into_body().collect().await.unwrap().to_bytes();(status,serde_json::from_slice(&bytes).unwrap())
}
#[tokio::test]
async fn telegram_routes_auth_csrf_unknown_fields_uuid_and_origin_validate_before_database() {
    let pool=PgPoolOptions::new().connect_lazy("postgres://unused:unused@127.0.0.1:1/unused_test").unwrap();let (state,transport)=state(pool,true);let router=telegram_router(state.clone());
    for (method,path) in [("GET","/v1/telegram-relay"),("PUT","/v1/telegram-relay"),("POST","/v1/telegram-relay/probe"),("POST","/v1/telegram-relay/test")] {
        let response=router.clone().oneshot(Request::builder().method(method).uri(path).header("content-type","application/json").body(Body::from("{}")).unwrap()).await.unwrap();assert_eq!(response.status(),StatusCode::UNAUTHORIZED);
    }
    for url in ["http://relay.example","https://user@relay.example","https://@relay.example","https://relay.example/path","https://relay.example/?q=1","https://relay.example/#fragment"] {
        assert_eq!(request(&router,"PUT","/v1/telegram-relay",json!({"enabled":true,"endpointUrl":url,"expectedRevision":0})).await.0,StatusCode::BAD_REQUEST);
    }
    for (path,body) in [("/v1/telegram-relay",json!({"enabled":true,"endpointUrl":"https://relay.example","expectedRevision":0,"key":"forbidden"})),("/v1/telegram-relay/probe",json!({"expectedRevision":0,"secret":"forbidden"})),("/v1/telegram-relay/test",json!({"expectedRevision":0,"requestId":"not-a-uuid"}))] {
        assert_eq!(request(&router,if path=="/v1/telegram-relay"{"PUT"}else{"POST"},path,body).await.0,StatusCode::BAD_REQUEST);
    }
    let setups_api::auth::PostSessionResult::Issued {token,..}=state.auth.post_session(Some("test-key"),setups_api::now_ms()) else {panic!("fixture login")};
    for (method,path) in [("PUT","/v1/telegram-relay"),("POST","/v1/telegram-relay/probe"),("POST","/v1/telegram-relay/test")] {
        let response=router.clone().oneshot(Request::builder().method(method).uri(path).header("cookie",format!("{}={token}",setups_api::COOKIE_NAME)).header("content-type","application/json").body(Body::from("{}")).unwrap()).await.unwrap();assert_eq!(response.status(),StatusCode::FORBIDDEN);
    }
    assert_eq!(transport.probes.load(Ordering::SeqCst)+transport.tests.load(Ordering::SeqCst),0);
}
#[tokio::test]
#[ignore="requires isolated TEST_DATABASE_URL ending in _test"]
async fn delayed_explicit_test_blocks_origin_switch_until_unknown_completion_then_replays_once() {
    let fixture=TestDb::new().await;let pool=fixture.pool.clone();let result=tokio::spawn(async move {
        let (app,transport)=state(pool.clone(),true);let router=telegram_router(app);
        assert_eq!(request(&router,"PUT","/v1/telegram-relay",json!({"enabled":true,"endpointUrl":"https://a.example/","expectedRevision":0})).await.0,StatusCode::OK);
        transport.delay_test.store(true,Ordering::SeqCst);
        let body=json!({"expectedRevision":1,"requestId":"33333333-3333-4333-8333-333333333333"});
        let pending={let router=router.clone();let body=body.clone();tokio::spawn(async move {request(&router,"POST","/v1/telegram-relay/test",body).await})};
        tokio::time::timeout(std::time::Duration::from_secs(2),transport.test_started.notified()).await.expect("diagnostic transport never started");
        let switch=json!({"enabled":true,"endpointUrl":"https://b.example/","expectedRevision":1});
        let blocked=tokio::time::timeout(std::time::Duration::from_secs(2),request(&router,"PUT","/v1/telegram-relay",switch.clone())).await;
        let posts_before_release=transport.test_posts.load(Ordering::SeqCst);
        transport.test_release.notify_one();let finished=pending.await.unwrap();
        let blocked=blocked.expect("a SQL transaction must not be held across diagnostic network wait");
        assert_eq!(blocked.0,StatusCode::CONFLICT,"changing origin must not overtake an admitted diagnostic");assert_eq!(blocked.1["error"],"telegram_relay_endpoint_busy");
        assert_eq!(posts_before_release,0);assert_eq!(finished.0,StatusCode::OK);assert_eq!(finished.1["state"],"unknown");
        assert_eq!(transport.test_target.lock().unwrap().as_deref(),Some("https://a.example/"));
        assert!(sqlx::query_scalar::<_,bool>("select in_flight_until is null from telegram_relay_tests").fetch_one(&pool).await.unwrap());
        assert_eq!(request(&router,"POST","/v1/telegram-relay/test",body.clone()).await,finished,"same-key replay must return the original unknown receipt");
        assert_eq!(transport.tests.load(Ordering::SeqCst),1);assert_eq!(transport.test_posts.load(Ordering::SeqCst),1);
        let changed=request(&router,"PUT","/v1/telegram-relay",switch).await;assert_eq!(changed.0,StatusCode::OK);assert_eq!(changed.1["settings"]["endpointUrl"],"https://b.example/");
        assert_eq!(request(&router,"POST","/v1/telegram-relay/test",body).await.0,StatusCode::CONFLICT);assert_eq!(transport.tests.load(Ordering::SeqCst),1);
        assert_eq!(sqlx::query_scalar::<_,i64>("select count(*) from telegram_relay_tests").fetch_one(&pool).await.unwrap(),1,"completion retains the 24-hour receipt without holding the origin");
    }).await;fixture.cleanup().await;result.expect("delayed diagnostic origin authorization failed after fixture cleanup");
}

struct TestDb {admin:PgPool,pool:PgPool,name:String}
impl TestDb {
    async fn new()->Self {
        let options=PgConnectOptions::from_str(&std::env::var("TEST_DATABASE_URL").unwrap()).unwrap();assert!(options.get_database().unwrap().ends_with("_test"));
        let admin=PgPoolOptions::new().max_connections(2).connect_with(options.clone()).await.unwrap();let name=format!("telegram_api_{}_{}",std::process::id(),chrono::Utc::now().timestamp_nanos_opt().unwrap());
        sqlx::query(&format!("create database {name}")).execute(&admin).await.unwrap();let pool=PgPoolOptions::new().max_connections(8).connect_with(options.database(&name)).await.unwrap();
        let baseline=include_str!("../../db/migrations/0010_baseline.sql").lines().filter(|line|!line.starts_with('\\')&&!line.starts_with("SET transaction_timeout")&&!line.contains("set_config('search_path'")).collect::<Vec<_>>().join("\n");
        sqlx::raw_sql(&baseline).execute(&pool).await.unwrap();sqlx::raw_sql("set search_path to public;create table kysely_migration(name varchar(255) primary key,timestamp varchar(255) not null);insert into kysely_migration values('0009_legacy','legacy')").execute(&pool).await.unwrap();setups_db::migrate::ensure_migrations(&pool).await.unwrap();Self {admin,pool,name}
    }
    async fn cleanup(self) {self.pool.close().await;sqlx::query(&format!("drop database {} with(force)",self.name)).execute(&self.admin).await.unwrap();self.admin.close().await;}
}
#[tokio::test]
#[ignore="requires isolated TEST_DATABASE_URL ending in _test"]
async fn telegram_http_envelopes_real_pg_cas_probe_separation_single_test_unknown_and_no_get_network() {
    let fixture=TestDb::new().await;let pool=fixture.pool.clone();let result=tokio::spawn(async move {
        let (app,transport)=state(pool.clone(),true);let router=telegram_router(app);
        let (status,initial)=request(&router,"GET","/v1/telegram-relay",Value::Null).await;assert_eq!(status,StatusCode::OK);assert_eq!(initial["settings"],json!({"enabled":false,"endpointUrl":"","revision":0}));assert_eq!(initial["status"]["state"],"disabled");
        let config=json!({"enabled":true,"endpointUrl":"https://Relay.example/","expectedRevision":0});
        let (status,saved)=request(&router,"PUT","/v1/telegram-relay",config.clone()).await;assert_eq!(status,StatusCode::OK);if let Some(path)=std::env::var_os("TELEGRAM_API_ENVELOPE_FIXTURE") {std::fs::write(path,serde_json::to_vec_pretty(&saved).unwrap()).unwrap();}
        assert_eq!(saved["settings"]["endpointUrl"],"https://relay.example/");assert_eq!(saved["status"]["endpointUrl"],"https://relay.example/");assert_eq!(saved["settings"]["revision"],1);assert_eq!(saved["status"]["state"],"unknown");assert_eq!(saved["status"]["checkedAt"],Value::Null);assert_eq!(saved["status"]["secretConfigured"],true);
        let persisted=request(&router,"GET","/v1/telegram-relay",Value::Null).await.1;assert_eq!(persisted["settings"]["endpointUrl"],"https://relay.example/");assert_eq!(persisted["status"]["endpointUrl"],"https://relay.example/");assert_eq!(persisted,saved);
        assert_eq!(request(&router,"PUT","/v1/telegram-relay",config).await.0,StatusCode::CONFLICT);
        assert_eq!(request(&router,"POST","/v1/telegram-relay/probe",json!({"expectedRevision":0})).await.0,StatusCode::CONFLICT);
        let (status,probed)=request(&router,"POST","/v1/telegram-relay/probe",json!({"expectedRevision":1})).await;assert_eq!(status,StatusCode::OK);assert_eq!(probed["status"]["state"],"connected");assert_eq!(probed["status"]["telegramChannel"],"error");assert!(probed["status"]["checkedAt"].is_string());
        assert_eq!(request(&router,"POST","/v1/telegram-relay/probe",json!({"expectedRevision":1})).await.0,StatusCode::TOO_MANY_REQUESTS);
        request(&router,"GET","/v1/telegram-relay",Value::Null).await;assert_eq!(transport.probes.load(Ordering::SeqCst),1);assert_eq!(transport.tests.load(Ordering::SeqCst),0);
        let test=json!({"expectedRevision":1,"requestId":"11111111-1111-4111-8111-111111111111"});let first=request(&router,"POST","/v1/telegram-relay/test",test.clone()).await;assert_eq!(first.0,StatusCode::OK);assert_eq!(first.1["state"],"unknown");assert_eq!(request(&router,"POST","/v1/telegram-relay/test",test).await,first);assert_eq!(transport.tests.load(Ordering::SeqCst),1);
        assert_eq!(request(&router,"POST","/v1/telegram-relay/test",json!({"expectedRevision":1,"requestId":"22222222-2222-4222-8222-222222222222"})).await.0,StatusCode::TOO_MANY_REQUESTS);
        sqlx::query("update telegram_relay_config set probe_started_at=null").execute(&pool).await.unwrap();transport.switch.store(true,Ordering::SeqCst);
        assert_eq!(request(&router,"POST","/v1/telegram-relay/probe",json!({"expectedRevision":1})).await.0,StatusCode::CONFLICT);
        let current=request(&router,"GET","/v1/telegram-relay",Value::Null).await.1;assert_eq!(current["settings"]["revision"],2);assert_eq!(current["status"]["state"],"unknown");assert_eq!(current["status"]["checkedAt"],Value::Null);
        let (missing,unused)=state(pool.clone(),false);let missing=telegram_router(missing);assert_eq!(request(&missing,"GET","/v1/telegram-relay",Value::Null).await.1["status"]["state"],"not_configured");assert_eq!(unused.probes.load(Ordering::SeqCst),0);
    }).await;fixture.cleanup().await;result.expect("Telegram HTTP contract failed after fixture cleanup");
}
