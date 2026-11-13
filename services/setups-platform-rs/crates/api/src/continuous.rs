//! Authenticated process API. Research identity and independent live permission are separate.
pub mod runtime;
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
    pub poll: Duration,
    pub max_database_bytes: i64,
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
            poll: Duration::from_millis(250),
            max_database_bytes: 48 * 1024 * 1024 * 1024,
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
        .route(
            "/v1/continuous-studies/{algorithm}",
            get(detail).put(configure),
        )
        .route("/v1/continuous-studies/{algorithm}/start", post(start))
        .route("/v1/continuous-studies/{algorithm}/stop", post(stop))
        .layer(DefaultBodyLimit::max(cap))
        .with_state(state)
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
        Ok(processes) => response(StatusCode::OK, json!({"processes":processes})),
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
    }
}
