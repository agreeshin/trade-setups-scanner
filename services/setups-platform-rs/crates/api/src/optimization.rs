//! Server-owned optimizer lifecycle, isolated from ordinary replay and analytics.
mod snapshot;

use crate::{
    auth::SessionAuth, config::OptimizationConfig, controls::optimization_descriptors,
    replay::guard_auth,
};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path, Query, State, rejection::QueryRejection},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};
use setups_db::optimization::{self as db, Admission, StudyRecord};
use setups_domain::{
    SPIKE_ALGORITHM_ID, SPIKE_ALGORITHM_VERSION, SPIKE_DROP_ALGORITHM_ID,
    SPIKE_DROP_ALGORITHM_VERSION,
};
use setups_optimizer::{
    CancelToken, StudyEvent, StudyOutcome, StudyPhase, StudyProgress, StudyRequest, StudyUpdate,
    TerminationReason,
};
use sqlx::{Connection, PgPool};
use std::{
    collections::VecDeque,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::{Notify, watch};
use tokio_util::sync::CancellationToken;

const POLL: Duration = Duration::from_millis(100);
const MAX_EVENTS: usize = 64;

pub struct OptimizationState {
    pub pool: PgPool,
    pub auth: Arc<SessionAuth>,
    pub config: OptimizationConfig,
    pub notify: Arc<Notify>,
    pub available: Arc<AtomicBool>,
}

pub struct OptimizationHandle {
    stop: CancellationToken,
    task: tokio::task::JoinHandle<()>,
    pub notify: Arc<Notify>,
    pub available: Arc<AtomicBool>,
}

impl OptimizationHandle {
    pub async fn stop(self) {
        self.available.store(false, Ordering::SeqCst);
        self.stop.cancel();
        if let Err(error) = self.task.await {
            eprintln!("optimization runtime join failed: {error}");
        }
    }
}

pub fn optimization_router(state: Arc<OptimizationState>) -> Router {
    let max_body = state.config.max_request_bytes;
    Router::new()
        .route(
            "/v1/optimization-studies",
            post(create_study).get(list_studies),
        )
        .route(
            "/v1/optimization-studies/{id}",
            get(get_study).delete(delete_study),
        )
        .route("/v1/optimization-studies/{id}/cancel", post(cancel_study))
        .layer(DefaultBodyLimit::max(max_body))
        .with_state(state)
}

fn response(status: StatusCode, body: Value) -> Response {
    (status, [("cache-control", "no-store")], Json(body)).into_response()
}

fn record_response(status: StatusCode, record: StudyRecord) -> Response {
    (status, [("cache-control", "no-store")], Json(record)).into_response()
}

fn internal_error(operation: &str, error: impl std::fmt::Display) -> Response {
    eprintln!("optimization {operation} failed: {error}");
    response(
        StatusCode::INTERNAL_SERVER_ERROR,
        json!({"error":"internal_error"}),
    )
}

fn version(id: &str) -> Option<&'static str> {
    match id {
        SPIKE_ALGORITHM_ID => Some(SPIKE_ALGORITHM_VERSION),
        SPIKE_DROP_ALGORITHM_ID => Some(SPIKE_DROP_ALGORITHM_VERSION),
        _ => None,
    }
}

fn initial_progress(spec: &StudyRequest) -> StudyProgress {
    StudyProgress {
        phase: StudyPhase::Acquiring,
        evaluated_candidates: 0,
        scorable_candidates: 0,
        max_trials: spec.max_trials,
        elapsed_seconds: 0.0,
        best_score: None,
    }
}

fn resource_error(resource: &str, limit: usize, requested: usize) -> Response {
    response(
        StatusCode::TOO_MANY_REQUESTS,
        json!({"error":"resource_limit","resource":resource,"limit":limit,"requested":requested}),
    )
}

async fn create_study(
    State(state): State<Arc<OptimizationState>>,
    headers: HeaderMap,
    body: String,
) -> Response {
    if let Err(error) = guard_auth(&state.auth, &headers, true) {
        return error;
    }
    let spec: StudyRequest = match serde_json::from_str(&body) {
        Ok(spec) => spec,
        Err(_) => return response(StatusCode::BAD_REQUEST, json!({"error":"invalid_body"})),
    };
    let Some(current) = version(&spec.algorithm_id) else {
        return response(
            StatusCode::BAD_REQUEST,
            json!({"error":"unknown_algorithm"}),
        );
    };
    if current != spec.algorithm_version {
        return response(
            StatusCode::CONFLICT,
            json!({"error":"algorithm_version_changed","algorithmVersion":current}),
        );
    }
    if spec.instruments.len() > state.config.max_instruments {
        return resource_error(
            "instruments",
            state.config.max_instruments,
            spec.instruments.len(),
        );
    }
    let domain_values = spec
        .algorithm_search
        .values()
        .chain(spec.analytics_search.values())
        .map(Vec::len)
        .sum::<usize>();
    if domain_values > state.config.max_domain_values {
        return resource_error(
            "domain_values",
            state.config.max_domain_values,
            domain_values,
        );
    }
    let descriptors = optimization_descriptors(&spec.algorithm_id).expect("registered algorithm");
    if let Err(error) = setups_optimizer::validate_request(&spec, &descriptors) {
        return response(
            StatusCode::BAD_REQUEST,
            json!({"error":"invalid_request","message":error.to_string()}),
        );
    }
    // Only new admissions receive serde defaults (including minSetupsPerDay).
    // Detail returns persisted raw spec/result JSON, preserving legacy meaning;
    // list projects the same raw spec without loading the full result/events.
    let normalized = match serde_json::to_value(&spec) {
        Ok(value) => value,
        Err(error) => return internal_error("serialize_spec", error),
    };
    if !state.available.load(Ordering::SeqCst) {
        return response(
            StatusCode::SERVICE_UNAVAILABLE,
            json!({"error":"optimization_runtime_unavailable"}),
        );
    }
    match db::runtime_owned(&state.pool).await {
        Ok(true) => {}
        Ok(false) => {
            return response(
                StatusCode::SERVICE_UNAVAILABLE,
                json!({"error":"optimization_runtime_unavailable"}),
            );
        }
        Err(error) => {
            eprintln!("optimization admission ownership check failed: {error}");
            return response(
                StatusCode::SERVICE_UNAVAILABLE,
                json!({"error":"optimization_runtime_unavailable"}),
            );
        }
    }
    match db::create(
        &state.pool,
        &spec.algorithm_id,
        &normalized,
        &json!(initial_progress(&spec)),
    )
    .await
    {
        Ok(Admission::Created(record)) => {
            state.notify.notify_one();
            record_response(StatusCode::ACCEPTED, *record)
        }
        Ok(Admission::Busy(id)) => response(
            StatusCode::CONFLICT,
            json!({"error":"study_active","activeStudyId":id}),
        ),
        Err(error) => internal_error("create", error),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ListQuery {
    algorithm_id: Option<String>,
    limit: Option<i64>,
    offset: Option<i64>,
}

async fn list_studies(
    State(state): State<Arc<OptimizationState>>,
    headers: HeaderMap,
    query: Result<Query<ListQuery>, QueryRejection>,
) -> Response {
    if let Err(error) = guard_auth(&state.auth, &headers, false) {
        return error;
    }
    let Ok(Query(query)) = query else {
        return response(StatusCode::BAD_REQUEST, json!({"error":"invalid_query"}));
    };
    let limit = query.limit.unwrap_or(20);
    let offset = query.offset.unwrap_or(0);
    if !(1..=100).contains(&limit)
        || offset < 0
        || query
            .algorithm_id
            .as_deref()
            .is_some_and(|id| version(id).is_none())
    {
        return response(StatusCode::BAD_REQUEST, json!({"error":"invalid_query"}));
    }
    match db::list(&state.pool, query.algorithm_id.as_deref(), limit, offset).await {
        Ok(page) => ([("cache-control", "no-store")], Json(page)).into_response(),
        Err(error) => internal_error("list", error),
    }
}

async fn get_study(
    State(state): State<Arc<OptimizationState>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Response {
    if let Err(error) = guard_auth(&state.auth, &headers, false) {
        return error;
    }
    if id < 1 {
        return response(StatusCode::BAD_REQUEST, json!({"error":"invalid_id"}));
    }
    match db::get(&state.pool, id).await {
        Ok(Some(record)) => record_response(StatusCode::OK, record),
        Ok(None) => response(StatusCode::NOT_FOUND, json!({"error":"not_found"})),
        Err(error) => internal_error("get", error),
    }
}

async fn delete_study(
    State(state): State<Arc<OptimizationState>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Response {
    if let Err(error) = guard_auth(&state.auth, &headers, true) {
        return error;
    }
    if id < 1 {
        return response(StatusCode::BAD_REQUEST, json!({"error":"invalid_id"}));
    }
    match db::delete(&state.pool, id).await {
        Ok(Some(db::DeleteStudyOutcome::Deleted)) => {
            response(StatusCode::OK, json!({"deleted":true,"id":id}))
        }
        Ok(Some(db::DeleteStudyOutcome::Active)) => {
            response(StatusCode::CONFLICT, json!({"error":"study_active"}))
        }
        Ok(None) => response(StatusCode::NOT_FOUND, json!({"error":"not_found"})),
        Err(error) => internal_error("delete", error),
    }
}

async fn cancel_study(
    State(state): State<Arc<OptimizationState>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Response {
    if let Err(error) = guard_auth(&state.auth, &headers, true) {
        return error;
    }
    if id < 1 {
        return response(StatusCode::BAD_REQUEST, json!({"error":"invalid_id"}));
    }
    match db::cancel(&state.pool, id).await {
        Ok(Some((record, accepted))) => {
            state.notify.notify_one();
            record_response(
                if accepted {
                    StatusCode::ACCEPTED
                } else {
                    StatusCode::OK
                },
                record,
            )
        }
        Ok(None) => response(StatusCode::NOT_FOUND, json!({"error":"not_found"})),
        Err(error) => internal_error("cancel", error),
    }
}

/// Owns one advisory-locked connection for the runtime lifetime. Detaching it
/// ensures dropping the owner closes the socket rather than leaking a session
/// lock back into the general connection pool.
pub async fn start_optimization_runtime(
    pool: PgPool,
    market_url: String,
    market_key: String,
    config: OptimizationConfig,
) -> Result<OptimizationHandle, String> {
    let mut owner = pool.acquire().await.map_err(|e| e.to_string())?.detach();
    let acquired: bool = sqlx::query_scalar("select pg_try_advisory_lock($1)")
        .bind(db::RUNTIME_LOCK)
        .fetch_one(&mut owner)
        .await
        .map_err(|e| e.to_string())?;
    if !acquired {
        let _ = owner.close().await;
        return Err("optimization_runtime_already_owned".into());
    }
    let interrupted = db::interrupt(&pool).await.map_err(|e| e.to_string())?;
    if interrupted > 0 {
        println!("optimization restart recovery: interrupted={interrupted}");
    }
    let stop = CancellationToken::new();
    let stopped = stop.clone();
    let notify = Arc::new(Notify::new());
    let wake = notify.clone();
    let available = Arc::new(AtomicBool::new(true));
    let healthy = available.clone();
    let task = tokio::spawn(async move {
        let mut owns_lock = true;
        loop {
            if stopped.is_cancelled() {
                break;
            }
            // Detect loss of the lock-owning connection before admitting work.
            if let Err(error) = owner.ping().await {
                healthy.store(false, Ordering::SeqCst);
                eprintln!("optimization runtime ownership lost: {error}");
                break;
            }
            healthy.store(true, Ordering::SeqCst);
            match db::claim(&pool).await {
                Ok(Some(record)) => {
                    let execution = execute(
                        &pool,
                        record,
                        &market_url,
                        &market_key,
                        &config,
                        &stopped,
                        &healthy,
                    );
                    tokio::pin!(execution);
                    let mut monitor = tokio::time::interval(Duration::from_millis(250));
                    monitor.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                    loop {
                        tokio::select! {
                            _=&mut execution=>break,
                            _=monitor.tick()=>{
                                if !matches!(tokio::time::timeout(Duration::from_secs(1),owner.ping()).await,Ok(Ok(()))) {
                                    // Lease loss is a failure, not an explicit user Stop.
                                    healthy.store(false,Ordering::SeqCst);
                                    owns_lock=false;
                                    stopped.cancel();
                                    execution.await;
                                    break;
                                }
                            }
                        }
                    }
                }
                Ok(None) => tokio::select! {
                    _ = stopped.cancelled() => {},
                    _ = wake.notified() => {},
                    _ = tokio::time::sleep(Duration::from_millis(500)) => {},
                },
                Err(error) => {
                    healthy.store(false, Ordering::SeqCst);
                    eprintln!("optimization claim failed: {error}");
                    tokio::select! { _ = stopped.cancelled() => {}, _ = tokio::time::sleep(Duration::from_secs(1)) => {} }
                }
            }
        }
        healthy.store(false, Ordering::SeqCst);
        if stopped.is_cancelled()
            && owns_lock
            && !matches!(
                tokio::time::timeout(Duration::from_secs(1), db::interrupt(&pool)).await,
                Ok(Ok(_))
            )
        {
            eprintln!("optimization shutdown metadata unavailable; startup recovery required");
        }
        let _ = owner.close().await;
    });
    Ok(OptimizationHandle {
        stop,
        task,
        notify,
        available,
    })
}

#[derive(Clone)]
struct Latest {
    progress: StudyProgress,
    events: VecDeque<StudyEvent>,
}

impl Latest {
    fn event(&mut self, kind: &str, message: &str) {
        if self.events.len() == MAX_EVENTS {
            self.events.pop_front();
        }
        self.events.push_back(StudyEvent {
            at: chrono::Utc::now().to_rfc3339(),
            kind: bounded_text(kind, 64),
            message: bounded_text(message, 400),
        });
    }
    fn merge(&mut self, update: StudyUpdate, elapsed: f64) {
        self.progress = update.progress;
        self.progress.elapsed_seconds += elapsed;
        // Full incumbents never enter the watch channel or persistent progress.
        // The core retains and returns the latest fully completed winner.
        if let Some(event) = update.event {
            self.event(&event.kind, &event.message);
        }
    }
}

fn bounded_text(text: &str, max_bytes: usize) -> String {
    let mut output = String::new();
    for c in text.chars().map(|c| if c.is_control() { ' ' } else { c }) {
        if output.len() + c.len_utf8() > max_bytes {
            break;
        }
        output.push(c);
    }
    output
}

async fn persist(pool: &PgPool, id: i64, latest: &Latest) -> Result<(), String> {
    db::save_progress(pool, id, &json!(latest.progress), &json!(latest.events))
        .await
        .map_err(|e| e.to_string())
}

async fn execute(
    pool: &PgPool,
    record: StudyRecord,
    market_url: &str,
    market_key: &str,
    config: &OptimizationConfig,
    stop: &CancellationToken,
    available: &AtomicBool,
) {
    let id = record.id;
    let started = Instant::now();
    println!("optimization study start: id={id}");
    let spec: StudyRequest = match serde_json::from_value(record.spec) {
        Ok(value) => value,
        Err(error) => {
            let error = json!({"code":"invalid_persisted_spec","message":error.to_string().chars().take(300).collect::<String>()});
            finish_retry(
                pool,
                TerminalPayload {
                    id,
                    status: "failed",
                    progress: record.progress,
                    result: None,
                    error: Some(error),
                    events: record.events,
                },
                stop,
                available,
            )
            .await;
            return;
        }
    };
    let mut latest = Latest {
        progress: initial_progress(&spec),
        events: VecDeque::new(),
    };
    latest.event("acquiring", "Фиксация свечей и пропусков исследования");
    let outcome = execute_inner(
        pool,
        id,
        &spec,
        market_url,
        market_key,
        config,
        stop,
        started,
        &mut latest,
    )
    .await;
    latest.progress.phase = StudyPhase::Cleanup;
    latest.progress.elapsed_seconds = started.elapsed().as_secs_f64();
    // execute_inner has joined CPU work and released all raw input before the
    // terminal update releases the unique active-study slot.
    let (status, result, error) = match outcome {
        Ok(mut outcome) => {
            let cancelled = outcome.reason == TerminationReason::Cancelled;
            outcome.progress = latest.progress.clone();
            let status = if cancelled { "cancelled" } else { "completed" };
            latest.event(
                status,
                if cancelled {
                    "Исследование остановлено; сохранён только завершённый результат"
                } else {
                    "Исследование завершено"
                },
            );
            (status, Some(json!(outcome)), None)
        }
        Err(error) => {
            eprintln!("optimization study failed: id={id} code={}", error.code);
            latest.event("failed", &error.message);
            (
                "failed",
                None,
                Some(json!({"code":error.code,"message":error.message})),
            )
        }
    };
    drop(spec);
    finish_retry(
        pool,
        TerminalPayload {
            id,
            status,
            progress: json!(latest.progress),
            result,
            error,
            events: json!(latest.events),
        },
        stop,
        available,
    )
    .await;
    println!(
        "optimization study resources released: id={id} elapsedSeconds={:.3}",
        started.elapsed().as_secs_f64()
    );
}

struct TerminalPayload {
    id: i64,
    status: &'static str,
    progress: Value,
    result: Option<Value>,
    error: Option<Value>,
    events: Value,
}

async fn finish_retry(
    pool: &PgPool,
    mut payload: TerminalPayload,
    stop: &CancellationToken,
    available: &AtomicBool,
) {
    // All raw snapshot/CPU references have been released. A transient DB outage
    // retains exactly one bounded terminal payload, never an input or trial log.
    if payload.result.as_ref().is_some_and(|value| {
        serde_json::to_vec(value).map_or(true, |bytes| bytes.len() > 8 * 1024 * 1024)
    }) {
        payload.status = "failed";
        payload.result = None;
        payload.error = Some(
            json!({"code":"resource_limit","message":"Итог исследования превышает допустимый размер"}),
        );
    }
    let mut failures = 0u64;
    let mut shutdown_deadline = None;
    loop {
        if stop.is_cancelled() {
            payload.status = "failed";
            payload.result = None;
            payload.error = Some(
                json!({"code":"interrupted_by_restart","message":"Исследование прервано остановкой сервера"}),
            );
            payload.events = json!([{ "at":chrono::Utc::now().to_rfc3339(),"kind":"failed","message":"Исследование прервано остановкой сервера" }]);
            shutdown_deadline
                .get_or_insert_with(|| tokio::time::Instant::now() + Duration::from_secs(2));
        }
        let finish = db::finish(
            pool,
            payload.id,
            payload.status,
            &payload.progress,
            payload.result.as_ref(),
            payload.error.as_ref(),
            &payload.events,
        );
        let result = if let Some(deadline) = shutdown_deadline {
            match tokio::time::timeout_at(deadline, finish).await {
                Ok(result) => result,
                Err(_) => {
                    eprintln!(
                        "optimization shutdown terminal persistence deferred to startup: id={}",
                        payload.id
                    );
                    return;
                }
            }
        } else {
            tokio::select! {result=finish=>result,_=stop.cancelled()=>continue}
        };
        match result {
            Ok(()) => return,
            Err(error) => {
                available.store(false, Ordering::SeqCst);
                if failures.is_multiple_of(60) {
                    eprintln!(
                        "optimization terminal persistence retry: id={} failures={failures} {error}",
                        payload.id
                    );
                }
                failures = failures.saturating_add(1);
                if shutdown_deadline.is_some_and(|deadline| tokio::time::Instant::now() >= deadline)
                {
                    eprintln!(
                        "optimization shutdown terminal persistence deferred to startup: id={}",
                        payload.id
                    );
                    return;
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    }
}

#[derive(Debug)]
struct JobError {
    code: &'static str,
    message: String,
}
impl JobError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into().chars().take(500).collect(),
        }
    }
}

fn empty_outcome(latest: &Latest, reason: TerminationReason) -> StudyOutcome {
    StudyOutcome {
        progress: latest.progress.clone(),
        best: None,
        reason,
        baseline: None,
        invalid_proposals: 0,
        detector_passes: 0,
    }
}

#[allow(clippy::too_many_arguments)]
async fn execute_inner(
    pool: &PgPool,
    id: i64,
    spec: &StudyRequest,
    market_url: &str,
    market_key: &str,
    config: &OptimizationConfig,
    stop: &CancellationToken,
    started: Instant,
    latest: &mut Latest,
) -> Result<StudyOutcome, JobError> {
    let deadline = tokio::time::Instant::from_std(started) + Duration::from_secs(spec.max_seconds);
    persist(pool, id, latest)
        .await
        .map_err(|e| JobError::new("database_error", e))?;
    let input = {
        let acquisition = snapshot::acquire(spec, market_url, market_key, config);
        tokio::pin!(acquisition);
        loop {
            tokio::select! {
                biased;
                _ = stop.cancelled() => return Err(JobError::new("interrupted_by_restart","Исследование прервано остановкой сервера")),
                _ = tokio::time::sleep_until(deadline) => return Ok(empty_outcome(latest,TerminationReason::TimeBudget)),
                result = &mut acquisition => break result?,
                _ = tokio::time::sleep(POLL) => {
                    if db::cancellation_requested(pool,id).await.map_err(|e| JobError::new("database_error",e.to_string()))? {
                        return Ok(empty_outcome(latest,TerminationReason::Cancelled));
                    }
                    latest.progress.elapsed_seconds = started.elapsed().as_secs_f64();
                }
            }
        }
    };
    latest.event(
        "snapshot_ready",
        "Неизменный снимок подготовлен; запуск поиска",
    );
    let acquisition_elapsed = started.elapsed().as_secs_f64();
    let cancel = Arc::new(CancelToken::new());
    let cpu_cancel = cancel.clone();
    let deadline_cancel = cancel.clone();
    let shutdown = stop.clone();
    let watchdog = tokio::spawn(async move {
        tokio::select! { _=tokio::time::sleep_until(deadline)=>{}, _=shutdown.cancelled()=>{} }
        deadline_cancel.cancel();
    });
    let cpu_spec = spec.clone();
    let limits = config.study_limits();
    let mut callback_state = latest.clone();
    let (sender, mut receiver) = watch::channel(callback_state.clone());
    let mut cpu = tokio::task::spawn_blocking(move || {
        // All merging happens synchronously on the evaluator thread BEFORE
        // publication. A slow DB writer can skip snapshots, not events.
        setups_optimizer::run_study(&cpu_spec, &input, &limits, &cpu_cancel, move |update| {
            callback_state.merge(update, acquisition_elapsed);
            sender.send_replace(callback_state.clone());
        })
    });
    let mut forced_reason = None;
    let mut persistence_error = None;
    let mut tick = tokio::time::interval(POLL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last_saved = Instant::now();
    let result = loop {
        tokio::select! {
            result = &mut cpu => break result,
            _ = tick.tick() => {
                *latest = receiver.borrow_and_update().clone();
                latest.progress.elapsed_seconds = started.elapsed().as_secs_f64();
                if forced_reason.is_none() {
                    if stop.is_cancelled() {
                        forced_reason = Some(TerminationReason::Cancelled);
                    } else {
                        match db::cancellation_requested(pool,id).await {
                            Ok(true) => forced_reason = Some(TerminationReason::Cancelled),
                            Ok(false) => if tokio::time::Instant::now() >= deadline { forced_reason = Some(TerminationReason::TimeBudget); },
                            Err(error) => { persistence_error = Some(error.to_string()); cancel.cancel(); }
                        }
                    }
                    if forced_reason.is_some() { cancel.cancel(); }
                }
                if last_saved.elapsed() >= Duration::from_millis(500) && persistence_error.is_none() {
                    if let Err(error) = persist(pool,id,latest).await { persistence_error=Some(error); cancel.cancel(); }
                    last_saved=Instant::now();
                }
            }
        }
    };
    watchdog.abort();
    let _ = watchdog.await;
    *latest = receiver.borrow().clone();
    if stop.is_cancelled() {
        return Err(JobError::new(
            "interrupted_by_restart",
            "Исследование прервано остановкой сервера",
        ));
    }
    if let Some(error) = persistence_error {
        return Err(JobError::new("database_error", error));
    }
    let mut outcome = result.map_err(|_| JobError::new("worker_failed","Вычислительная задача завершилась с ошибкой"))?
        .map_err(|error| match error {
            setups_optimizer::StudyError::ResourceLimit { .. } => JobError::new("resource_limit",format!("{error}. Уменьшите область исследования или увеличьте соответствующий OPTIMIZATION_* лимит.")),
            _ => JobError::new("evaluation_failed",error.to_string()),
        })?;
    latest.progress = outcome.progress.clone();
    if forced_reason.is_none() && tokio::time::Instant::now() >= deadline {
        forced_reason = Some(TerminationReason::TimeBudget);
    }
    if let Some(reason) = forced_reason {
        outcome.reason = reason;
        if let Some(best) = &mut outcome.best {
            best.partial = reason == TerminationReason::Cancelled;
        }
    }
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn synchronous_callback_merge_keeps_bounded_events_when_watch_is_overwritten() {
        let progress = StudyProgress {
            phase: StudyPhase::Initial,
            evaluated_candidates: 0,
            scorable_candidates: 0,
            max_trials: 100,
            elapsed_seconds: 0.0,
            best_score: None,
        };
        let mut state = Latest {
            progress: progress.clone(),
            events: VecDeque::new(),
        };
        let (sender, receiver) = watch::channel(state.clone());
        for i in 0..100 {
            let mut progress = progress.clone();
            progress.best_score = Some(i as f64);
            progress.evaluated_candidates = i;
            state.merge(
                StudyUpdate {
                    progress,
                    incumbent: None,
                    event: Some(StudyEvent {
                        at: String::new(),
                        kind: "incumbent".into(),
                        message: format!("{i}{}", "x".repeat(1000)),
                    }),
                },
                2.0,
            );
            sender.send_replace(state.clone());
        }
        let latest = receiver.borrow();
        assert_eq!(latest.progress.best_score, Some(99.0));
        assert_eq!(latest.events.len(), MAX_EVENTS);
        assert!(
            latest
                .events
                .iter()
                .all(|event| event.message.chars().count() <= 400)
        );
        assert!(latest.events.back().unwrap().message.starts_with("99"));
        assert_eq!(latest.progress.elapsed_seconds, 2.0);
        let value = json!(latest.progress);
        assert!(value.get("bestScore").is_some());
        assert!(value.get("incumbent").is_none());
    }
}
