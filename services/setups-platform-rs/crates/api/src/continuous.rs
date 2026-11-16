//! Authenticated process API. Research identity and independent live permission are separate.
pub mod runtime;
pub mod frozen;
use crate::{SessionAuth, config::OptimizationConfig, replay::guard_auth};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path, Query, State, rejection::QueryRejection},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use setups_db::continuous::{self as db, ADMIN_OWNER};
use setups_optimizer::{ContinuousContext, StudyRequest};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::sync::Notify;

#[derive(Debug, Clone)]
pub struct ContinuousRuntimeConfig {
    /// This lane never borrows the manual optimizer's admission or worker budget.
    pub optimizer: OptimizationConfig,
    pub workers: usize,
    /// Actual admission for complete resident chains, including CPU and frozen input.
    pub max_concurrent_chains: usize,
    pub poll: Duration,
    pub max_database_bytes: i64,
    /// Private durable snapshots/checkpoints, separate from resident RAM and history.
    pub max_runtime_bytes: i64,
    pub replay_admission: setups_db::admission::ReplayAdmissionLimits,
}
impl Default for ContinuousRuntimeConfig {
    fn default() -> Self {
        Self {
            optimizer: OptimizationConfig {
                cpu_threads: 1,
                batch_size: 1,
                max_snapshot_bytes: 256 * 1024 * 1024,
                max_working_bytes: 64 * 1024 * 1024,
                max_setups_per_candidate: 10000,
                ..OptimizationConfig::default()
            },
            workers: 1,
            max_concurrent_chains: 1,
            poll: Duration::from_millis(250),
            max_database_bytes: 48 * 1024 * 1024 * 1024,
            max_runtime_bytes: 16 * 1024 * 1024 * 1024,
            replay_admission: Default::default(),
        }
    }
}
impl ContinuousRuntimeConfig {
    pub fn from_env() -> Result<Self, String> {
        Self::from_lookup(&mut |key| std::env::var(key).ok())
    }

    fn from_lookup(get: &mut dyn FnMut(&str) -> Option<String>) -> Result<Self, String> {
        let mut config = Self::default();
        for (key, target, max) in [
            ("CONTINUOUS_WORKERS", &mut config.workers, 4),
            ("CONTINUOUS_MAX_CONCURRENT_CHAINS", &mut config.max_concurrent_chains, 4),
            (
                "CONTINUOUS_MAX_SNAPSHOT_BYTES",
                &mut config.optimizer.max_snapshot_bytes,
                8 * 1024 * 1024 * 1024u64,
            ),
            (
                "CONTINUOUS_MAX_WORKING_BYTES",
                &mut config.optimizer.max_working_bytes,
                512 * 1024 * 1024,
            ),
            (
                "CONTINUOUS_MAX_SETUPS_PER_CANDIDATE",
                &mut config.optimizer.max_setups_per_candidate,
                100000,
            ),
        ] {
            if let Some(value) = get(key) {
                let value = value
                    .parse::<u64>()
                    .map_err(|_| format!("{key} must be a positive integer"))?;
                if value == 0 || value > max {
                    return Err(format!("{key} must be in 1..={max}"));
                }
                *target = usize::try_from(value)
                    .map_err(|_| format!("{key} does not fit this platform"))?;
            }
        }
        if let Some(value) = get("CONTINUOUS_MAX_DATABASE_BYTES") {
            config.max_database_bytes = value
                .parse::<i64>()
                .ok()
                .filter(|value| *value > 0)
                .ok_or("CONTINUOUS_MAX_DATABASE_BYTES must be a positive integer")?;
        }
        if let Some(value)=get("CONTINUOUS_MAX_RUNTIME_BYTES") {
            config.max_runtime_bytes=value.parse::<i64>().ok().filter(|value|*value>0 && *value<=64*1024*1024*1024)
                .ok_or("CONTINUOUS_MAX_RUNTIME_BYTES must be in 1..=68719476736")?;
        }
        Ok(config)
    }
}

pub struct ContinuousState {
    pub pool: sqlx::PgPool,
    pub auth: Arc<SessionAuth>,
    pub config: ContinuousRuntimeConfig,
    pub notify: Arc<Notify>,
    pub available: Arc<AtomicBool>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ContinuousConfig {
    pub lookback_days: f64,
    pub min_setups_per_day: Vec<f64>,
    #[serde(default = "default_mean")]
    pub min_mean_expectancy: f64,
    pub adaptive_enabled: bool,
    pub study: Value,
}
fn default_mean() -> f64 {
    0.15
}

pub fn registered(id: &str) -> bool {
    setups_optimizer::default_parameter_descriptors(id).is_ok()
}

pub fn normalize_config(
    mut config: ContinuousConfig,
    algorithm: &str,
    limits: &OptimizationConfig,
) -> Result<ContinuousConfig, String> {
    let seconds = config.lookback_days * 86400.0;
    if !config.lookback_days.is_finite()
        || seconds < 60.0
        || seconds > 3660.0 * 86400.0
        || seconds.fract() != 0.0
        || !config.min_mean_expectancy.is_finite()
        || config.min_setups_per_day.is_empty()
        || config.min_setups_per_day.len() > 64
    {
        return Err("invalid continuous window, gate or threshold count".into());
    }
    for (index, value) in config.min_setups_per_day.iter().enumerate() {
        if !value.is_finite() || *value < 0.0 || config.min_setups_per_day[..index].contains(value)
        {
            return Err("frequency thresholds must be unique finite nonnegative numbers".into());
        }
    }
    let object = config
        .study
        .as_object_mut()
        .ok_or("study must be an object")?;
    for field in ["fromUnix", "toUnix", "minSetupsPerDay", "setupMaturity"] {
        if object.contains_key(field) {
            return Err(format!("{field} is owned by continuous execution"));
        }
    }
    let to = chrono::Utc::now().timestamp();
    object.insert("fromUnix".into(), json!(to - seconds as i64));
    object.insert("toUnix".into(), json!(to));
    object.insert(
        "minSetupsPerDay".into(),
        json!(config.min_setups_per_day[0]),
    );
    object.insert("setupMaturity".into(), json!(0));
    let spec: StudyRequest = serde_json::from_value(config.study).map_err(|e| e.to_string())?;
    if spec.algorithm_id != algorithm || !registered(algorithm) {
        return Err("algorithmId differs from process identity".into());
    }
    if spec.instruments.len() > limits.max_instruments
        || spec
            .algorithm_search
            .values()
            .chain(spec.analytics_search.values())
            .map(Vec::len)
            .sum::<usize>()
            > limits.max_domain_values
    {
        return Err("continuous instrument/domain resource limit exceeded".into());
    }
    setups_optimizer::validate_continuous_request(
        &spec,
        ContinuousContext {
            observation_cutoff: to.div_euclid(60) * 60,
        },
    )
    .map_err(|e| e.to_string())?;
    config.study = serde_json::to_value(spec).map_err(|e| e.to_string())?;
    for field in ["fromUnix", "toUnix", "minSetupsPerDay", "setupMaturity"] {
        config.study.as_object_mut().unwrap().remove(field);
    }
    Ok(config)
}

fn response(status: StatusCode, body: Value) -> Response {
    (status, [("cache-control", "no-store")], Json(body)).into_response()
}
fn failure(operation: &str, error: impl std::fmt::Display) -> Response {
    eprintln!("continuous {operation} failed: {error}");
    response(
        StatusCode::INTERNAL_SERVER_ERROR,
        json!({"error":"internal_error"}),
    )
}
fn unconfigured(algorithm: &str) -> Value {
    json!({"algorithmId":algorithm,"status":"stopped","configRevision":0,"config":null,"activeStudyId":null,"phase":null,"error":null,"slots":[]})
}
async fn current_response(
    state: &ContinuousState,
    algorithm: &str,
    status: StatusCode,
) -> Response {
    match db::get(&state.pool, ADMIN_OWNER, algorithm).await {
        Ok(Some(p)) => response(status, json!(p)),
        Ok(None) => response(status, unconfigured(algorithm)),
        Err(e) => failure("get", e),
    }
}

pub fn continuous_router(state: Arc<ContinuousState>) -> Router {
    let cap = state.config.optimizer.max_request_bytes;
    Router::new()
        .route("/v1/continuous-studies", get(list))
        .route("/v1/continuous-studies/cleanup", get(cleanup_preview).post(cleanup_history))
        .route("/v1/continuous-studies/maintenance",get(maintenance_status))
        .route("/v1/continuous-studies/maintenance/drain",post(maintenance_drain))
        .route("/v1/continuous-studies/maintenance/resume",post(maintenance_resume))
        .route(
            "/v1/continuous-studies/{algorithm}",
            get(detail).put(configure),
        )
        .route("/v1/continuous-studies/{algorithm}/start", post(start))
        .route("/v1/continuous-studies/{algorithm}/stop", post(stop))
        .route("/v1/continuous-studies/{algorithm}/recovery/retry",post(retry_recovery))
        .route("/v1/continuous-studies/{algorithm}/slots/{slot}/live-readmission",post(live_readmission))
        .route("/v1/adaptive-profiles",get(profiles))
        .route("/v1/adaptive-profiles/{profile}",get(profile_detail))
        .route("/v1/adaptive-profiles/{profile}/stop",post(profile_stop))
        .route("/v1/adaptive-profiles/{profile}/play",post(profile_play))
        .route("/v1/adaptive-live/{algorithm}/permission",get(live_permission).put(save_live_permission))
        .layer(DefaultBodyLimit::max(cap))
        .with_state(state)
}

#[derive(Deserialize)]
#[serde(rename_all="camelCase",deny_unknown_fields)]
struct ProfilesQuery {algorithm_id:Option<String>,offset:Option<i64>,limit:Option<i64>}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LivePermissionRequest {enabled:bool}
async fn live_permission(State(state):State<Arc<ContinuousState>>,headers:HeaderMap,Path(id):Path<String>)->Response {
    if let Err(error)=guard_auth(&state.auth,&headers,false) {return error;}
    if !registered(&id) {return response(StatusCode::BAD_REQUEST,json!({"error":"unknown_algorithm"}));}
    match db::profiles::permission(&state.pool,ADMIN_OWNER,&id).await {
        Ok(enabled)=>response(StatusCode::OK,json!({"algorithmId":id,"enabled":enabled})),Err(error)=>failure("live permission",error),
    }
}
async fn save_live_permission(State(state):State<Arc<ContinuousState>>,headers:HeaderMap,Path(id):Path<String>,body:String)->Response {
    if let Err(error)=guard_auth(&state.auth,&headers,true) {return error;}
    if !registered(&id) {return response(StatusCode::BAD_REQUEST,json!({"error":"unknown_algorithm"}));}
    let Ok(request)=serde_json::from_str::<LivePermissionRequest>(&body) else {return response(StatusCode::BAD_REQUEST,json!({"error":"invalid_live_permission"}));};
    match db::profiles::set_permission(&state.pool,ADMIN_OWNER,&id,request.enabled).await {
        Ok(())=>response(StatusCode::OK,json!({"algorithmId":id,"enabled":request.enabled})),Err(error)=>failure("live permission",error),
    }
}
async fn profiles(State(state):State<Arc<ContinuousState>>,headers:HeaderMap,query:Result<Query<ProfilesQuery>,QueryRejection>)->Response {
    if let Err(error)=guard_auth(&state.auth,&headers,false) {return error;}
    let Ok(Query(query))=query else {return response(StatusCode::BAD_REQUEST,json!({"error":"invalid_query"}));};
    let offset=query.offset.unwrap_or(0);let limit=query.limit.unwrap_or(50);
    if offset<0 || !(1..=100).contains(&limit) || query.algorithm_id.as_deref().is_some_and(|a|!registered(a)) {
        return response(StatusCode::BAD_REQUEST,json!({"error":"invalid_query"}));
    }
    match db::profiles::list(&state.pool,ADMIN_OWNER,query.algorithm_id.as_deref(),offset,limit).await {
        Ok(value)=>response(StatusCode::OK,value),Err(error)=>failure("profile list",error),
    }
}
async fn profile_detail(State(state):State<Arc<ContinuousState>>,headers:HeaderMap,Path(id):Path<i64>)->Response {
    if let Err(error)=guard_auth(&state.auth,&headers,false) {return error;}
    match db::profiles::detail(&state.pool,ADMIN_OWNER,id).await {
        Ok(Some(value))=>response(StatusCode::OK,value),Ok(None)=>response(StatusCode::NOT_FOUND,json!({"error":"profile_not_found"})),
        Err(error)=>failure("profile detail",error),
    }
}
async fn profile_change(state:Arc<ContinuousState>,headers:HeaderMap,id:i64,body:String,play:bool)->Response {
    if let Err(error)=guard_auth(&state.auth,&headers,true) {return error;}
    let request=match serde_json::from_str::<db::profiles::ProfileCommand>(&body) {
        Ok(request) if id>0 && request.validate().is_ok()=>request,
        _=>return response(StatusCode::BAD_REQUEST,json!({"error":"invalid_profile_command"})),
    };
    match db::profiles::command(&state.pool,ADMIN_OWNER,id,play,&request).await {
        Ok(db::profiles::CommandOutcome::Applied(value))=>response(StatusCode::OK,value),
        Ok(db::profiles::CommandOutcome::NotFound)=>response(StatusCode::NOT_FOUND,json!({"error":"profile_not_found"})),
        Ok(db::profiles::CommandOutcome::Conflict(code))=>response(StatusCode::CONFLICT,json!({"error":code})),
        Ok(db::profiles::CommandOutcome::Limited{resource,limit})=>response(StatusCode::TOO_MANY_REQUESTS,json!({"error":resource,"resource":resource,"limit":limit})),
        Err(error)=>failure("profile command",error),
    }
}
async fn profile_stop(State(state):State<Arc<ContinuousState>>,headers:HeaderMap,Path(id):Path<i64>,body:String)->Response {
    profile_change(state,headers,id,body,false).await
}
async fn profile_play(State(state):State<Arc<ContinuousState>>,headers:HeaderMap,Path(id):Path<i64>,body:String)->Response {
    profile_change(state,headers,id,body,true).await
}
async fn retry_recovery(State(state):State<Arc<ContinuousState>>,headers:HeaderMap,Path(id):Path<String>)->Response {
    if let Err(error)=guard_auth(&state.auth,&headers,true) {return error;}
    if !registered(&id) {return response(StatusCode::BAD_REQUEST,json!({"error":"unknown_algorithm"}));}
    match db::retry_recovery(&state.pool,ADMIN_OWNER,&id).await {
        Ok(true)=>{state.notify.notify_waiters();current_response(&state,&id,StatusCode::ACCEPTED).await},
        Ok(false)=>response(StatusCode::CONFLICT,json!({"error":"research_recovery_not_eligible"})),
        Err(error)=>failure("research recovery",error),
    }
}

/// Optional deployment overrides. Per-owner limits remain an externally owned
/// DB configuration, never supplied by browser profile/research commands.
pub async fn configure_profile_limits_from_env(pool:&sqlx::PgPool)->Result<(),String> {
    let total=std::env::var("ADAPTIVE_PROFILE_TOTAL_LIMIT").ok();
    let running=std::env::var("ADAPTIVE_PROFILE_RUNNING_LIMIT").ok();
    if total.is_none() && running.is_none() {return Ok(());}
    let base=setups_db::settings_get(pool,"adaptiveProfileLimits").await.map_err(|e|e.to_string())?.ok_or("missing adaptive profile limits")?;
    let mut limits:db::profiles::ProfileLimits=serde_json::from_value(base).map_err(|e|e.to_string())?;
    for (value,target,name) in [(total,&mut limits.total_profiles,"ADAPTIVE_PROFILE_TOTAL_LIMIT"),(running,&mut limits.running_profiles,"ADAPTIVE_PROFILE_RUNNING_LIMIT")] {
        if let Some(value)=value {*target=value.parse().map_err(|_|format!("{name} must be a positive integer"))?;}
    }
    db::profiles::configure_default_limits(pool,limits).await.map_err(|e|e.to_string())
}

async fn live_readmission(State(state):State<Arc<ContinuousState>>,headers:HeaderMap,Path((algorithm,slot)):Path<(String,String)>,body:String)->Response {
    if let Err(error)=guard_auth(&state.auth,&headers,true) {return error;}
    let Ok(slot)=slot.parse::<i64>() else {return response(StatusCode::BAD_REQUEST,json!({"error":"invalid_live_readmission"}));};
    let parsed=serde_json::from_str::<db::readmission::LiveReadmissionRequest>(&body);
    let request=match parsed {
        Ok(request) if registered(&algorithm) && slot>0 && request.validate().is_ok()=>request,
        _=>return response(StatusCode::BAD_REQUEST,json!({"error":"invalid_live_readmission"})),
    };
    match db::readmission::grant(&state.pool,ADMIN_OWNER,&algorithm,slot,&request).await {
        Ok(db::readmission::LiveReadmissionOutcome::Granted(receipt))=>response(StatusCode::OK,json!(receipt)),
        Ok(db::readmission::LiveReadmissionOutcome::NotFound)=>response(StatusCode::NOT_FOUND,json!({"error":"live_readmission_not_found"})),
        Ok(db::readmission::LiveReadmissionOutcome::Conflict(code))=>response(StatusCode::CONFLICT,json!({"error":code})),
        Err(error)=>failure("live readmission",error),
    }
}

async fn maintenance_status(State(state):State<Arc<ContinuousState>>,headers:HeaderMap)->Response {
    if let Err(error)=guard_auth(&state.auth,&headers,false) {return error;}
    match db::maintenance::status(&state.pool,ADMIN_OWNER).await {
        Ok(status)=>response(StatusCode::OK,json!(status)),Err(error)=>failure("maintenance status",error),
    }
}
async fn maintenance_change(state:Arc<ContinuousState>,headers:HeaderMap,paused:bool)->Response {
    if let Err(error)=guard_auth(&state.auth,&headers,true) {return error;}
    match db::maintenance::set_paused(&state.pool,ADMIN_OWNER,paused).await {
        Ok(status)=>{state.notify.notify_waiters();response(StatusCode::OK,json!(status))},Err(error)=>failure("maintenance change",error),
    }
}
async fn maintenance_drain(State(state):State<Arc<ContinuousState>>,headers:HeaderMap)->Response {maintenance_change(state,headers,true).await}
async fn maintenance_resume(State(state):State<Arc<ContinuousState>>,headers:HeaderMap)->Response {maintenance_change(state,headers,false).await}

async fn cleanup_preview(State(state): State<Arc<ContinuousState>>, headers: HeaderMap) -> Response {
    if let Err(error)=guard_auth(&state.auth,&headers,false) {return error;}
    match db::cleanup::preview(&state.pool,ADMIN_OWNER).await {
        Ok(preview)=>response(StatusCode::OK,json!(preview)),
        Err(error)=>failure("cleanup preview",error),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CleanupRequest {confirm:bool,fingerprint:String}

async fn cleanup_history(State(state):State<Arc<ContinuousState>>,headers:HeaderMap,body:String)->Response {
    if let Err(error)=guard_auth(&state.auth,&headers,true) {return error;}
    let request=match serde_json::from_str::<CleanupRequest>(&body) {
        Ok(request) if request.confirm && request.fingerprint.len()==32 && request.fingerprint.bytes().all(|b|b.is_ascii_hexdigit())=>request,
        _=>return response(StatusCode::BAD_REQUEST,json!({"error":"continuous_cleanup_confirmation_required"})),
    };
    match db::cleanup::purge(&state.pool,ADMIN_OWNER,&request.fingerprint).await {
        Ok(db::cleanup::Outcome::Deleted(result))=>response(StatusCode::OK,json!(result)),
        Ok(db::cleanup::Outcome::Blocked(preview))=>response(StatusCode::CONFLICT,json!({"error":"continuous_cleanup_blocked","message":"Stop and drain all continuous processes and deliveries before cleanup","preview":preview})),
        Ok(db::cleanup::Outcome::Stale(preview))=>response(StatusCode::CONFLICT,json!({"error":"continuous_cleanup_preview_stale","preview":preview})),
        Err(error)=>failure("cleanup",error),
    }
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ListQuery {
    algorithm_id: Option<String>,
}
async fn list(
    State(state): State<Arc<ContinuousState>>,
    headers: HeaderMap,
    query: Result<Query<ListQuery>, QueryRejection>,
) -> Response {
    if let Err(e) = guard_auth(&state.auth, &headers, false) {
        return e;
    }
    let Ok(Query(query)) = query else {
        return response(StatusCode::BAD_REQUEST, json!({"error":"invalid_query"}));
    };
    if query
        .algorithm_id
        .as_deref()
        .is_some_and(|id| !registered(id))
    {
        return response(
            StatusCode::BAD_REQUEST,
            json!({"error":"unknown_algorithm"}),
        );
    }
    match db::list(&state.pool, ADMIN_OWNER, query.algorithm_id.as_deref()).await {
        Ok(processes) => match db::maintenance::status(&state.pool,ADMIN_OWNER).await {
            Ok(maintenance)=>response(StatusCode::OK,json!({"processes":processes,"maintenance":maintenance})),
            Err(error)=>failure("maintenance list",error),
        },
        Err(e) => failure("list", e),
    }
}
async fn detail(
    State(state): State<Arc<ContinuousState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(e) = guard_auth(&state.auth, &headers, false) {
        return e;
    }
    if !registered(&id) {
        return response(
            StatusCode::BAD_REQUEST,
            json!({"error":"unknown_algorithm"}),
        );
    }
    current_response(&state, &id, StatusCode::OK).await
}
async fn configure(
    State(state): State<Arc<ContinuousState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: String,
) -> Response {
    if let Err(e) = guard_auth(&state.auth, &headers, true) {
        return e;
    }
    let config = serde_json::from_str::<ContinuousConfig>(&body)
        .map_err(|e| e.to_string())
        .and_then(|c| normalize_config(c, &id, &state.config.optimizer));
    let config = match config {
        Ok(c) => c,
        Err(e) => {
            return response(
                StatusCode::BAD_REQUEST,
                json!({"error":"invalid_config","message":e.chars().take(300).collect::<String>()}),
            );
        }
    };
    match db::configure(
        &state.pool,
        ADMIN_OWNER,
        &id,
        &json!(config),
        &config.min_setups_per_day,
    )
    .await
    {
        Ok(true) => current_response(&state, &id, StatusCode::OK).await,
        Ok(false) => response(
            StatusCode::CONFLICT,
            json!({"error":"continuous_process_active","message":"Stop and drain before changing research configuration; adaptiveEnabled can be changed independently"}),
        ),
        Err(e) => failure("configure", e),
    }
}
async fn start(
    State(state): State<Arc<ContinuousState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(e) = guard_auth(&state.auth, &headers, true) {
        return e;
    }
    if !registered(&id) {
        return response(
            StatusCode::BAD_REQUEST,
            json!({"error":"unknown_algorithm"}),
        );
    }
    if !state.available.load(Ordering::SeqCst) {
        return response(
            StatusCode::SERVICE_UNAVAILABLE,
            json!({"error":"continuous_runtime_unavailable"}),
        );
    }
    match db::start(&state.pool, ADMIN_OWNER, &id).await {
        Ok(true) => {
            state.notify.notify_waiters();
            current_response(&state, &id, StatusCode::ACCEPTED).await
        }
        Ok(false) => response(
            StatusCode::CONFLICT,
            json!({"error":"configure_required_or_stopping"}),
        ),
        Err(e) => failure("start", e),
    }
}
async fn stop(
    State(state): State<Arc<ContinuousState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(e) = guard_auth(&state.auth, &headers, true) {
        return e;
    }
    if !registered(&id) {
        return response(
            StatusCode::BAD_REQUEST,
            json!({"error":"unknown_algorithm"}),
        );
    }
    match db::stop(&state.pool, ADMIN_OWNER, &id).await {
        Ok(_) => {
            state.notify.notify_waiters();
            current_response(&state, &id, StatusCode::ACCEPTED).await
        }
        Err(e) => failure("stop", e),
    }
}

#[cfg(test)]
mod tests {
    use super::ContinuousRuntimeConfig;

    #[tokio::test]
    async fn independent_profile_and_permission_routes_authenticate_before_database_access() {
        use std::sync::{Arc,atomic::AtomicBool};
        use tower::ServiceExt;
        let state=Arc::new(super::ContinuousState{pool:sqlx::postgres::PgPoolOptions::new().connect_lazy("postgres://invalid:invalid@127.0.0.1:1/never_connect_test").unwrap(),
            auth:Arc::new(crate::auth::SessionAuth::new(&["fixture-key".into()])),config:ContinuousRuntimeConfig::default(),
            notify:Arc::new(tokio::sync::Notify::new()),available:Arc::new(AtomicBool::new(true))});
        let app=super::continuous_router(state);
        for (method,path) in [("GET","/v1/adaptive-profiles"),("GET","/v1/adaptive-profiles/1"),("POST","/v1/adaptive-profiles/1/stop"),("POST","/v1/adaptive-profiles/1/play"),("GET","/v1/adaptive-live/price_volume_spike/permission"),("PUT","/v1/adaptive-live/price_volume_spike/permission"),("POST","/v1/continuous-studies/price_volume_spike/recovery/retry")] {
            let response=app.clone().oneshot(axum::http::Request::builder().method(method).uri(path).body(axum::body::Body::from("{}")).unwrap()).await.unwrap();
            assert_eq!(response.status(),axum::http::StatusCode::UNAUTHORIZED,"{method} {path}");
        }
        let response=app.oneshot(axum::http::Request::builder().method("POST").uri("/v1/adaptive-profiles/1/play").header("x-api-key","fixture-key").body(axum::body::Body::from(r#"{"expectedRevision":1,"expectedGenerationId":2,"requestId":"fixture-000000001","totalProfiles":999}"#)).unwrap()).await.unwrap();
        assert_eq!(response.status(),axum::http::StatusCode::BAD_REQUEST,"browser cannot raise quotas through profile commands");
    }

    #[test]
    fn trend_settings_are_additive_strict_and_preserve_legacy_mean_config() {
        use serde_json::json;
        let legacy = json!({"lookbackDays":30,"minSetupsPerDay":[0],"adaptiveEnabled":false,"study":{
            "algorithmId":"price_volume_spike","algorithmVersion":"1.1.1",
            "algorithmParams":setups_domain::SpikeParams::default(),
            "analyticsParams":setups_optimizer::AnalyticsParams::default(),
            "instruments":[{"exchange":"bybit","category":"linear","symbol":"BTCUSDT"}],
            "direction":"short","seed":42
        }});
        let normalize = |value| super::normalize_config(serde_json::from_value(value).unwrap(), "price_volume_spike", &crate::config::OptimizationConfig::default());
        let old = normalize(legacy.clone()).unwrap();
        assert!(old.study.get("trendMode").is_none());
        assert!(old.study.get("trendBendDays").is_none());
        assert!(old.study.get("trendBlendLambda").is_none());
        assert!(old.study.get("minForecastExpectancy").is_none());
        let mut explicit = legacy.clone();
        explicit["study"]["trendMode"] = json!("mean");
        explicit["study"]["trendBendDays"] = json!("linear");
        assert_eq!(normalize(explicit).unwrap(), old);
        for bend in ["7","14","30","linear"] {
            let mut request=legacy.clone();
            request["study"]["trendMode"]=json!("forecast");
            request["study"]["trendBendDays"]=json!(bend);
            let normalized=normalize(request).unwrap();
            assert_eq!(normalized.study["trendMode"],"forecast");
            assert_eq!(normalized.study.get("trendBendDays").and_then(|v|v.as_str()).unwrap_or("linear"),bend);
        }
        for (field,value) in [("trendMode","median"),("trendBendDays","8")] {
            let mut request=legacy.clone();request["study"][field]=json!(value);
            assert!(normalize(request).is_err());
        }
        let mut blend=legacy.clone();blend["study"]["trendMode"]=json!("blend");
        blend["study"]["trendBlendLambda"]=json!(0.6);blend["study"]["minForecastExpectancy"]=json!(0.2);
        let normalized=normalize(blend).unwrap();
        assert_eq!(normalized.study["trendBlendLambda"],0.6);
        assert_eq!(normalized.study["minForecastExpectancy"],0.2);
        for (field,value) in [("trendBlendLambda",0.705),("trendBlendLambda",1.0),("minForecastExpectancy",-0.1),("minForecastExpectancy",100.1)] {
            let mut request=legacy.clone();request["study"][field]=json!(value);assert!(normalize(request).is_err());
        }
    }

    fn config(values: &[(&str, &str)]) -> Result<ContinuousRuntimeConfig, String> {
        ContinuousRuntimeConfig::from_lookup(&mut |key| {
            values
                .iter()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| (*value).to_owned())
        })
    }

    #[test]
    fn continuous_snapshot_defaults_remain_small_and_independent_of_manual_settings() {
        let defaults = config(&[]).unwrap();
        let manual_override = config(&[("OPTIMIZATION_MAX_SNAPSHOT_BYTES", "8589934592")]).unwrap();
        for settings in [defaults, manual_override] {
            assert_eq!(settings.optimizer.max_snapshot_bytes, 256 * 1024 * 1024);
            assert_eq!(settings.optimizer.max_working_bytes, 64 * 1024 * 1024);
            assert_eq!(settings.optimizer.cpu_threads, 1);
            assert_eq!(settings.optimizer.batch_size, 1);
            assert_eq!(settings.workers, 1);
        }
    }

    #[test]
    #[cfg(target_pointer_width = "64")]
    fn continuous_snapshot_accepts_explicit_eight_gib_without_changing_other_budgets() {
        let settings = config(&[("CONTINUOUS_MAX_SNAPSHOT_BYTES", "8589934592")]).unwrap();
        assert_eq!(settings.optimizer.max_snapshot_bytes, 8_589_934_592);
        assert_eq!(settings.optimizer.max_working_bytes, 64 * 1024 * 1024);
        assert_eq!(settings.optimizer.max_setups_per_candidate, 10_000);
        assert_eq!(settings.optimizer.cpu_threads, 1);
        assert_eq!(settings.optimizer.batch_size, 1);
        assert_eq!(settings.workers, 1);
    }

    #[test]
    #[cfg(target_pointer_width = "64")]
    fn continuous_working_accepts_half_gib_within_enforced_resident_budget() {
        let mut settings = config(&[
            ("CONTINUOUS_MAX_SNAPSHOT_BYTES", "8589934592"),
            ("CONTINUOUS_MAX_WORKING_BYTES", "536870912"),
        ]).unwrap();
        assert_eq!(settings.optimizer.max_working_bytes, 512 * 1024 * 1024);
        assert_eq!(settings.optimizer.max_snapshot_bytes, 8 * 1024 * 1024 * 1024);
        assert_eq!(settings.optimizer.max_setups_per_candidate, 10_000);
        assert_eq!(settings.optimizer.cpu_threads, 1);
        assert_eq!(settings.optimizer.batch_size, 1);
        assert_eq!(settings.max_concurrent_chains, 1);
        let mut resources = crate::resources::RuntimeResources::from_lookup(&mut |_| None).unwrap();
        assert_eq!(resources.background_memory_bytes, 16 * 1024 * 1024 * 1024);
        resources.validate_continuous(&settings).unwrap();
        // A larger local working ceiling does not bypass aggregate admission.
        settings.workers = 2;
        settings.max_concurrent_chains = 2;
        resources.background_cpu_threads = 2;
        resources.continuous_db_connections = 8;
        assert!(resources.validate_continuous(&settings).unwrap_err().contains("resident"));
    }

    #[test]
    fn continuous_snapshot_rejects_invalid_and_above_eight_gib_values() {
        for value in [
            "",
            "0",
            "-1",
            "1.5",
            "NaN",
            "8589934593",
            "18446744073709551616",
        ] {
            let error = config(&[("CONTINUOUS_MAX_SNAPSHOT_BYTES", value)]).unwrap_err();
            assert!(
                error.contains("CONTINUOUS_MAX_SNAPSHOT_BYTES"),
                "{value}: {error}"
            );
        }
        assert_eq!(
            config(&[("CONTINUOUS_MAX_SNAPSHOT_BYTES", "1")])
                .unwrap()
                .optimizer
                .max_snapshot_bytes,
            1
        );
        for (key, value) in [
            ("CONTINUOUS_WORKERS", "5"),
            ("CONTINUOUS_MAX_WORKING_BYTES", "536870913"),
            ("CONTINUOUS_MAX_SETUPS_PER_CANDIDATE", "100001"),
        ] {
            assert!(config(&[(key, value)]).is_err(), "{key}={value}");
        }
    }

    #[test]
    #[cfg(target_pointer_width = "32")]
    fn continuous_snapshot_rejects_eight_gib_on_a_narrow_target() {
        assert!(config(&[("CONTINUOUS_MAX_SNAPSHOT_BYTES", "8589934592")]).is_err());
    }

    #[test]
    fn continuous_runtime_defaults_to_sixteen_gib() {
        assert_eq!(config(&[]).unwrap().max_runtime_bytes, 16 * 1024 * 1024 * 1024);
    }

    #[test]
    fn continuous_runtime_accepts_tight_positive_and_maximum_budgets() {
        for (value, expected) in [("1", 1_i64), ("68719476736", 68_719_476_736)] {
            let settings = config(&[("CONTINUOUS_MAX_RUNTIME_BYTES", value)]).unwrap();
            assert_eq!(settings.max_runtime_bytes, expected);
            assert_eq!(settings.optimizer.max_snapshot_bytes, 256 * 1024 * 1024);
            assert_eq!(settings.optimizer.max_working_bytes, 64 * 1024 * 1024);
        }
    }

    #[test]
    fn continuous_runtime_rejects_invalid_and_above_sixty_four_gib_values() {
        for value in [
            "",
            "0",
            "-1",
            "1.5",
            "NaN",
            "16GiB",
            "68719476737",
            "9223372036854775808",
            "18446744073709551616",
        ] {
            let error = config(&[("CONTINUOUS_MAX_RUNTIME_BYTES", value)]).unwrap_err();
            assert!(
                error.contains("CONTINUOUS_MAX_RUNTIME_BYTES"),
                "{value}: {error}"
            );
        }
    }

    #[test]
    fn production_compose_explicitly_passes_eight_gib_and_one_continuous_worker() {
        let compose = include_str!("../../../../../infra/prod/docker-compose.yml");
        let signals = compose
            .split("\n  signals-rust:\n")
            .nth(1)
            .unwrap()
            .split("\n  web:\n")
            .next()
            .unwrap();
        assert!(signals.contains("CONTINUOUS_WORKERS: \"1\""));
        assert!(signals.contains(
            "CONTINUOUS_MAX_SNAPSHOT_BYTES: ${CONTINUOUS_MAX_SNAPSHOT_BYTES:-8589934592}"
        ));
        assert!(signals.contains(
            "CONTINUOUS_MAX_RUNTIME_BYTES: ${CONTINUOUS_MAX_RUNTIME_BYTES:-17179869184}"
        ));
        assert!(signals.contains(
            "CONTINUOUS_MAX_WORKING_BYTES: ${CONTINUOUS_MAX_WORKING_BYTES:-67108864}"
        ));
    }
}
