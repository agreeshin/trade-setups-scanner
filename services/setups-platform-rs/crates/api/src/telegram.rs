//! Optional Telegram control API. GET never contacts an external service.
use crate::{SessionAuth,replay::guard_auth};
use axum::{Json,Router,extract::{State,DefaultBodyLimit,rejection::JsonRejection},http::{HeaderMap,StatusCode},response::{IntoResponse,Response},routing::{get,post}};
use futures::future::BoxFuture;
use serde::Deserialize;
use serde_json::json;
use setups_db::telegram::{self as db,TelegramConfig,TelegramStatusUpdate,TelegramTestAttempt,TelegramTestResult,TelegramReason};
use std::{sync::Arc,time::Duration};

/// Single typed transport boundary, implemented by the startup adapter around
/// the worker's authenticated/pinned relay client. No raw external errors here.
pub trait TelegramTransport:Send+Sync {
    fn secret_configured(&self)->bool;
    fn probe<'a>(&'a self,config:&'a TelegramConfig)->BoxFuture<'a,TelegramStatusUpdate>;
    fn test<'a>(&'a self,attempt:&'a TelegramTestAttempt)->BoxFuture<'a,TelegramTestResult>;
}
pub struct TelegramState {pub pool:sqlx::PgPool,pub auth:Arc<SessionAuth>,pub transport:Arc<dyn TelegramTransport>}
#[derive(Deserialize)]
#[serde(rename_all="camelCase",deny_unknown_fields)]
struct ConfigBody {enabled:bool,endpoint_url:String,expected_revision:i64}
#[derive(Deserialize)]
#[serde(rename_all="camelCase",deny_unknown_fields)]
struct RevisionBody {expected_revision:i64}
#[derive(Deserialize)]
#[serde(rename_all="camelCase",deny_unknown_fields)]
struct TestBody {expected_revision:i64,request_id:String}
fn error(status:StatusCode,code:&str)->Response {(status,[("cache-control","no-store")],Json(json!({"error":code}))).into_response()}
fn internal(error_value:setups_db::DbError)->Response {
    eprintln!("[telegram-api] database operation failed: {}",error_value.to_string().chars().take(240).collect::<String>());
    error(StatusCode::INTERNAL_SERVER_ERROR,"telegram_relay_database_error")
}
fn origin(value:&str,enabled:bool)->Option<String> {
    if value.is_empty()&&!enabled {return Some(String::new());}
    if value.len()>2048 {return None;}
    let url=reqwest::Url::parse(value).ok()?;
    let authority=value.split_once("://")?.1.split('/').next()?;
    if url.scheme()!="https"||url.host_str().is_none()||!url.username().is_empty()||url.password().is_some()||authority.contains('@')
        ||url.query().is_some()||url.fragment().is_some()||url.path()!="/"||url.port()==Some(0) {return None;}
    let root=format!("{}/",url.origin().ascii_serialization());
    (root.len()<=2048).then_some(root)
}
async fn envelope(state:&TelegramState)->Response {
    match db::get_envelope(&state.pool,state.transport.secret_configured()).await {
        Ok(value)=>([("cache-control","no-store")],Json(value)).into_response(),Err(value)=>internal(value),
    }
}
async fn get_settings(State(state):State<Arc<TelegramState>>,headers:HeaderMap)->Response {
    if let Err(response)=guard_auth(&state.auth,&headers,false) {return response;}envelope(&state).await
}
async fn put_settings(State(state):State<Arc<TelegramState>>,headers:HeaderMap,body:Result<Json<ConfigBody>,JsonRejection>)->Response {
    if let Err(response)=guard_auth(&state.auth,&headers,true) {return response;}
    let Ok(Json(body))=body else{return error(StatusCode::BAD_REQUEST,"invalid_telegram_relay_config");};
    let Some(endpoint)=origin(&body.endpoint_url,body.enabled) else{return error(StatusCode::BAD_REQUEST,"invalid_telegram_relay_origin");};
    if body.expected_revision<0 {return error(StatusCode::BAD_REQUEST,"invalid_settings_revision");}
    match db::set_config(&state.pool,body.expected_revision,body.enabled,&endpoint).await {
        Ok(db::ConfigUpdate::Updated(_))=>envelope(&state).await,
        Ok(db::ConfigUpdate::Stale)=>error(StatusCode::CONFLICT,"telegram_relay_settings_stale"),
        Ok(db::ConfigUpdate::EndpointBusy)=>error(StatusCode::CONFLICT,"telegram_relay_endpoint_busy"),Err(value)=>internal(value),
    }
}
async fn probe(State(state):State<Arc<TelegramState>>,headers:HeaderMap,body:Result<Json<RevisionBody>,JsonRejection>)->Response {
    if let Err(response)=guard_auth(&state.auth,&headers,true) {return response;}
    let Ok(Json(body))=body else{return error(StatusCode::BAD_REQUEST,"invalid_telegram_relay_probe");};
    if body.expected_revision<0 {return error(StatusCode::BAD_REQUEST,"invalid_settings_revision");}
    let admission=match db::begin_probe(&state.pool,body.expected_revision).await {Ok(value)=>value,Err(value)=>return internal(value)};
    let config=match admission {
        db::ProbeAdmission::Admitted(value)=>value,
        db::ProbeAdmission::Stale=>return error(StatusCode::CONFLICT,"telegram_relay_settings_stale"),
        db::ProbeAdmission::RateLimited=>return error(StatusCode::TOO_MANY_REQUESTS,"telegram_relay_probe_rate_limited"),
        db::ProbeAdmission::NotConfigured=>return envelope(&state).await,
    };
    let observation=if !state.transport.secret_configured() {
        TelegramStatusUpdate {relay_authenticated:false,telegram_ready:None,reason:Some(TelegramReason::new("key_not_configured","Relay signing key is not configured."))}
    }else{match tokio::time::timeout(Duration::from_secs(9),state.transport.probe(&config)).await {
        Ok(value)=>value,Err(_)=>TelegramStatusUpdate {relay_authenticated:false,telegram_ready:None,reason:Some(TelegramReason::new("probe_timeout","Relay probe timed out."))},
    }};
    match db::record_status(&state.pool,config.revision,&observation).await {
        Ok(true)=>envelope(&state).await,Ok(false)=>error(StatusCode::CONFLICT,"telegram_relay_settings_stale"),Err(value)=>internal(value),
    }
}
async fn test(State(state):State<Arc<TelegramState>>,headers:HeaderMap,body:Result<Json<TestBody>,JsonRejection>)->Response {
    if let Err(response)=guard_auth(&state.auth,&headers,true) {return response;}
    let Ok(Json(body))=body else{return error(StatusCode::BAD_REQUEST,"invalid_telegram_relay_test");};
    if body.expected_revision<0||!db::valid_test_request_id(&body.request_id) {return error(StatusCode::BAD_REQUEST,"invalid_telegram_relay_test");}
    let admission=match db::begin_test(&state.pool,body.expected_revision,&body.request_id).await {Ok(value)=>value,Err(value)=>return internal(value)};
    let attempt=match admission {
        db::TestAdmission::Admitted(value)=>value,
        db::TestAdmission::Existing(value)=>return ([("cache-control","no-store")],Json(value)).into_response(),
        db::TestAdmission::Stale=>return error(StatusCode::CONFLICT,"telegram_relay_settings_stale"),
        db::TestAdmission::KeyConflict=>return error(StatusCode::CONFLICT,"telegram_relay_test_key_conflict"),
        db::TestAdmission::RateLimited=>return error(StatusCode::TOO_MANY_REQUESTS,"telegram_relay_test_rate_limited"),
        db::TestAdmission::Capacity=>return error(StatusCode::TOO_MANY_REQUESTS,"telegram_relay_test_capacity"),
        db::TestAdmission::NotConfigured=>return ([("cache-control","no-store")],Json(TelegramTestResult {request_id:body.request_id,state:"error".into(),reason:Some(TelegramReason::new("not_configured","Relay endpoint is not configured."))})).into_response(),
    };
    let result=if !state.transport.secret_configured() {
        TelegramTestResult {request_id:attempt.request_id.clone(),state:"error".into(),reason:Some(TelegramReason::new("key_not_configured","Relay signing key is not configured."))}
    }else{match tokio::time::timeout(Duration::from_secs(9),state.transport.test(&attempt)).await {
        Ok(value)=>value,Err(_)=>TelegramTestResult {request_id:attempt.request_id.clone(),state:"unknown".into(),reason:Some(TelegramReason::new("delivery_unconfirmed","Delivery could not be confirmed; it will not be retried automatically."))},
    }};
    match db::record_test(&state.pool,&attempt,&result).await {
        Ok(true)=>([("cache-control","no-store")],Json(result)).into_response(),Ok(false)=>error(StatusCode::CONFLICT,"telegram_relay_settings_stale"),Err(value)=>internal(value),
    }
}
pub fn telegram_router(state:Arc<TelegramState>)->Router {
    Router::new().route("/v1/telegram-relay",get(get_settings).put(put_settings))
        .route("/v1/telegram-relay/probe",post(probe)).route("/v1/telegram-relay/test",post(test))
        .layer(DefaultBodyLimit::max(4096)).with_state(state)
}
