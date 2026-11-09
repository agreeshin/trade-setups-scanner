use crate::auth::{SessionAuth, now_ms};
use crate::browser_request;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{Value, json};
use setups_db::admission::{
    AdmissionInstrument, QueueError, ReplayAdmissionInput, ReplayAdmissionLimits, ReserveStatus,
    admit_replay_cache_miss, cancel_replay_run, delete_replay_run, purge_replay_runs,
    reserve_replay_runs,
};
use setups_db::runs::{
    BatchSummary, CacheCompletion, ReplayRunFull, cache_clear_all, cache_clear_by_instrument,
    cache_stats, complete_run_from_cache, get_run_full, get_run_intervals, get_run_page,
    list_recent_runs, list_replay_batches, mark_run_instrument_not_found,
};
use setups_db::{CacheQuery, DbError, cache_lookup, settings_get, settings_set};
use setups_domain::{SPIKE_ALGORITHM_ID, SPIKE_ALGORITHM_VERSION, SpikeParams};
use setups_mdclient::{ApiCandleSource, MarketDataClient};
use setups_worker::run_one;
use sqlx::postgres::PgPool;
use std::collections::HashMap;
use std::sync::Arc;

pub struct ReplayState {
    pub pool: PgPool,
    pub auth: Arc<SessionAuth>,
    pub market_url: String,
    pub market_key: String,
    pub limits: ReplayAdmissionLimits,
}

fn error_response(status: StatusCode, code: &str) -> Response {
    (
        status,
        [("cache-control", "no-store")],
        Json(json!({ "error": code })),
    )
        .into_response()
}

#[allow(clippy::result_large_err)]
pub(crate) fn guard_auth(
    auth: &SessionAuth,
    headers: &HeaderMap,
    mutation: bool,
) -> Result<(), Response> {
    if auth.valid_key(
        headers
            .get("x-api-key")
            .and_then(|value| value.to_str().ok()),
    ) {
        return Ok(());
    }
    let cookie = headers.get("cookie").and_then(|value| value.to_str().ok());
    if auth.session_expiry(cookie, now_ms()).is_none() {
        return Err(error_response(StatusCode::UNAUTHORIZED, "unauthorized"));
    }
    if mutation && !browser_request(headers) {
        return Err(error_response(
            StatusCode::FORBIDDEN,
            "invalid_request_origin",
        ));
    }
    Ok(())
}

#[allow(clippy::result_large_err)]
fn guard(state: &ReplayState, headers: &HeaderMap, mutation: bool) -> Result<(), Response> {
    guard_auth(&state.auth, headers, mutation)
}

fn iso8601(value: &DateTime<Utc>) -> String {
    value.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

fn run_dto(run: &ReplayRunFull) -> Value {
    json!({
        "quality": run.quality.clone(),
        "qualitySourceRunId": run.quality_source_run_id,
        "qualityComputedAt": run.quality_computed_at.as_ref().map(iso8601),
        "id": run.id,
        "exchange": run.exchange_code,
        "category": run.category,
        "symbol": run.symbol,
        "status": run.status,
        "cacheHit": run.cache_hit,
        "setupCount": run.setup_count,
        "warmupMinutes": run.warmup_minutes,
        "processedMinutes": run.processed_minutes,
        "error": run.error.clone(),
        "algorithmId": run.algorithm_id,
        "algorithmVersion": run.algorithm_version,
        "params": run.params.clone(),
        "fromUnix": run.from_ts.timestamp(),
        "toUnix": run.to_ts.timestamp(),
        "createdAt": iso8601(&run.created_at),
    })
}

pub(crate) fn batch_dto(batch: &BatchSummary) -> Value {
    json!({
        "id": batch.id,
        "exchange": batch.exchange_code,
        "category": batch.category,
        "symbol": format!("{} инструментов", batch.instrument_count),
        "status": batch.status,
        "cacheHit": batch.cache_hit,
        "setupCount": batch.setup_count,
        "warmupMinutes": Value::Null,
        "processedMinutes": Value::Null,
        "error": if batch.failed_instruments > 0 {
            Value::String(format!("Инструментов с ошибкой: {}", batch.failed_instruments))
        } else {
            Value::Null
        },
        "algorithmId": batch.algorithm_id,
        "algorithmVersion": batch.algorithm_version,
        "fromUnix": batch.from_ts.timestamp(),
        "toUnix": batch.to_ts.timestamp(),
        "createdAt": iso8601(&batch.created_at),
        "params": batch.params.clone(),
        "isBatch": true,
        "instrumentCount": batch.instrument_count,
        "activeInstruments": batch.active_instruments,
        "doneInstruments": batch.done_instruments,
        "failedInstruments": batch.failed_instruments,
        "cancelledInstruments": batch.cancelled_instruments,
    })
}

fn failed_stub_dto(run: &ReplayRunFull) -> Value {
    json!({
        "id": run.id,
        "exchange": run.exchange_code,
        "category": run.category,
        "symbol": run.symbol,
        "status": run.status,
        "cacheHit": run.cache_hit,
        "setupCount": 0,
        "warmupMinutes": Value::Null,
        "processedMinutes": Value::Null,
        "error": run.error.clone(),
    })
}

fn setup_dto(setup: &Value, exchange: &str, category: &str, symbol: &str) -> Value {
    json!({
        "exchange": exchange,
        "category": category,
        "symbol": symbol,
        "algorithmId": setup.get("algorithmId").cloned().unwrap_or(Value::Null),
        "algorithmVersion": setup.get("algorithmVersion").cloned().unwrap_or(Value::Null),
        "params": setup.get("params").cloned().unwrap_or(Value::Null),
        "details": setup.get("details").cloned().unwrap_or(Value::Null),
        "openTimeUnix": setup.get("openTimeUnix").cloned().unwrap_or(Value::Null),
        "closeTimeUnix": setup.get("closeTimeUnix").cloned().unwrap_or(Value::Null),
        "closePrice": setup.get("closePrice").cloned().unwrap_or(Value::Null),
    })
}

fn number(value: &str) -> f64 {
    value.parse::<f64>().unwrap_or(f64::NAN)
}

pub fn validate_spike_params(params: &SpikeParams) -> Option<&'static str> {
    if number(&params.price_growth_pct) > 1000.0
        || number(&params.volume_multiplier) <= 0.0
        || number(&params.volume_multiplier) > 1000.0
        || number(&params.max_gap_percent) > 100.0
    {
        return Some("Порог вне допустимого диапазона");
    }
    if params.baseline_short_days > params.baseline_long_days {
        return Some("Короткая база не должна превышать длинную");
    }
    if i64::from(params.window_minutes) >= i64::from(params.baseline_short_days) * 1440 {
        return Some("Окно роста должно быть короче базового окна");
    }
    None
}

pub(crate) fn merge_objects(base: &mut Value, overlay: &Value) {
    if let (Some(target), Some(source)) = (base.as_object_mut(), overlay.as_object()) {
        for (key, value) in source {
            target.insert(key.clone(), value.clone());
        }
    }
}

#[derive(Debug, Deserialize)]
struct InstrumentBody {
    exchange: String,
    category: String,
    symbol: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReplayRequestBody {
    instruments: Vec<InstrumentBody>,
    from_unix: i64,
    to_unix: i64,
    algorithm_id: Option<String>,
    algorithm_version: Option<String>,
    params: Option<Value>,
    #[serde(rename = "async")]
    async_run: Option<bool>,
}

struct PreparedReplay {
    instruments: Vec<AdmissionInstrument>,
    from_unix: i64,
    to_unix: i64,
    params: SpikeParams,
    async_run: bool,
}

async fn prepare_replay_request(
    pool: &PgPool,
    body: &ReplayRequestBody,
) -> Result<Result<PreparedReplay, String>, DbError> {
    let invalid = |message: &str| Ok(Err(message.to_string()));
    if body.instruments.is_empty() {
        return invalid("invalid_body");
    }
    let algorithm_id = body
        .algorithm_id
        .clone()
        .unwrap_or_else(|| SPIKE_ALGORITHM_ID.to_string());
    if algorithm_id != SPIKE_ALGORITHM_ID {
        return invalid("unknown_algorithm");
    }
    if let Some(version) = &body.algorithm_version
        && version != SPIKE_ALGORITHM_VERSION
    {
        return invalid("algorithm_version_changed_repeat_preflight");
    }
    let mut params_value = serde_json::to_value(SpikeParams::default()).unwrap_or(Value::Null);
    if let Some(stored) = settings_get(pool, "algorithm:price_volume_spike").await?
        && let Some(stored_params) = stored.get("params")
    {
        merge_objects(&mut params_value, stored_params);
    }
    if let Some(body_params) = &body.params {
        merge_objects(&mut params_value, body_params);
    }
    let params: SpikeParams = match serde_json::from_value(params_value) {
        Ok(value) => value,
        Err(_) => return invalid("invalid_params"),
    };
    let semantic = validate_spike_params(&params);
    let range_invalid = body.from_unix < 0
        || body.to_unix > 253_402_300_740
        || body.from_unix >= body.to_unix
        || body.to_unix - body.from_unix > 366 * 86400;
    if semantic.is_some() || range_invalid {
        return invalid(semantic.unwrap_or("invalid_range_max_366_days"));
    }
    Ok(Ok(PreparedReplay {
        instruments: body
            .instruments
            .iter()
            .map(|instrument| AdmissionInstrument {
                exchange: instrument.exchange.clone(),
                category: instrument.category.clone(),
                symbol: instrument.symbol.clone(),
            })
            .collect(),
        from_unix: body.from_unix,
        to_unix: body.to_unix,
        params,
        async_run: body.async_run.unwrap_or(false),
    }))
}

fn internal_error() -> Response {
    error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal_error")
}

async fn create_replay_runs(
    State(state): State<Arc<ReplayState>>,
    headers: HeaderMap,
    body: String,
) -> Response {
    if let Err(response) = guard(&state, &headers, true) {
        return response;
    }
    let parsed: ReplayRequestBody = match serde_json::from_str(&body) {
        Ok(value) => value,
        Err(_) => return error_response(StatusCode::BAD_REQUEST, "invalid_body"),
    };
    let prepared = match prepare_replay_request(&state.pool, &parsed).await {
        Err(_) => return internal_error(),
        Ok(Err(message)) => return error_response(StatusCode::BAD_REQUEST, &message),
        Ok(Ok(value)) => value,
    };
    let input = ReplayAdmissionInput {
        instruments: prepared.instruments.clone(),
        from_unix: prepared.from_unix,
        to_unix: prepared.to_unix,
        params: prepared.params.clone(),
    };
    let initial_status = if prepared.async_run {
        ReserveStatus::Queued
    } else {
        ReserveStatus::Running
    };
    let admitted =
        match reserve_replay_runs(&state.pool, &input, &state.limits, initial_status).await {
            Ok(runs) => runs,
            Err(QueueError::Admission(error)) => {
                let payload = json!({
                    "error": format!(
                        "resource_limit_exceeded:{}:{}:{}",
                        error.resource.as_str(),
                        error.requested,
                        error.limit
                    ),
                    "resource": error.resource.as_str(),
                    "requested": error.requested,
                    "limit": error.limit,
                });
                return (StatusCode::TOO_MANY_REQUESTS, Json(payload)).into_response();
            }
            Err(_) => return internal_error(),
        };
    if prepared.async_run {
        let mut runs = Vec::new();
        for run in &admitted {
            match get_run_full(&state.pool, run.id).await {
                Ok(Some(full)) => runs.push(run_dto(&full)),
                _ => runs.push(Value::Null),
            }
        }
        return (StatusCode::ACCEPTED, Json(json!({ "runs": runs }))).into_response();
    }
    let catalog = match MarketDataClient::new(&state.market_url, &state.market_key)
        .instruments()
        .await
    {
        Ok(list) => list
            .into_iter()
            .map(|item| {
                (
                    format!("{}/{}/{}", item.exchange, item.category, item.symbol),
                    item.id,
                )
            })
            .collect::<HashMap<String, i64>>(),
        Err(_) => return internal_error(),
    };
    let mut runs: Vec<Value> = Vec::new();
    for (instrument, run) in prepared.instruments.iter().zip(&admitted) {
        let identity = format!(
            "{}/{}/{}",
            instrument.exchange, instrument.category, instrument.symbol
        );
        let Some(&instrument_id) = catalog.get(&identity) else {
            match mark_run_instrument_not_found(&state.pool, run.id).await {
                Ok(failed) => runs.push(failed_stub_dto(&failed)),
                Err(_) => runs.push(Value::Null),
            }
            continue;
        };
        let cache_query = CacheQuery {
            exchange_code: instrument.exchange.clone(),
            category: instrument.category.clone(),
            symbol: instrument.symbol.clone(),
            algorithm_id: SPIKE_ALGORITHM_ID.to_string(),
            algorithm_version: SPIKE_ALGORITHM_VERSION.to_string(),
            params: prepared.params.clone(),
            from_unix: prepared.from_unix,
            to_unix: prepared.to_unix,
        };
        let cached = cache_lookup(&state.pool, &cache_query)
            .await
            .unwrap_or(None);
        if let Some(cached) = cached {
            let completion = CacheCompletion {
                quality: cached
                    .quality
                    .as_ref()
                    .map(|quality| serde_json::to_value(quality).unwrap_or(Value::Null)),
                quality_source_run_id: cached.quality_source_run_id,
                computed_at: cached.computed_at,
                setups: serde_json::to_value(&cached.setups).unwrap_or(Value::Array(Vec::new())),
                warmup_minutes: cached.warmup_minutes,
                processed_minutes: cached.processed_minutes,
            };
            if complete_run_from_cache(&state.pool, run.id, &completion)
                .await
                .is_ok()
                && let Ok(Some(full)) = get_run_full(&state.pool, run.id).await
            {
                runs.push(run_dto(&full));
                continue;
            }
        }
        if run.cache_hit {
            let _ = admit_replay_cache_miss(&state.pool, run, &state.limits).await;
        }
        let source = ApiCandleSource::new(
            state.market_url.clone(),
            state.market_key.clone(),
            instrument_id,
        );
        let _ = run_one(&state.pool, run, &source).await;
        match get_run_full(&state.pool, run.id).await {
            Ok(Some(full)) => runs.push(run_dto(&full)),
            _ => runs.push(Value::Null),
        }
    }
    (StatusCode::OK, Json(json!({ "runs": runs }))).into_response()
}

async fn list_replay_runs(State(state): State<Arc<ReplayState>>, headers: HeaderMap) -> Response {
    if let Err(response) = guard(&state, &headers, false) {
        return response;
    }
    let rows = match list_recent_runs(&state.pool, 200).await {
        Ok(value) => value,
        Err(_) => return internal_error(),
    };
    let batches = match list_replay_batches(&state.pool, 100).await {
        Ok(value) => value,
        Err(_) => return internal_error(),
    };
    let mut entries: Vec<(String, Value)> = Vec::new();
    for batch in &batches {
        let dto = batch_dto(batch);
        let created = dto
            .get("createdAt")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        entries.push((created, dto));
    }
    for run in &rows {
        let dto = run_dto(run);
        let created = iso8601(&run.created_at);
        entries.push((created, dto));
    }
    entries.sort_by(|a, b| b.0.cmp(&a.0));
    let runs: Vec<Value> = entries.into_iter().take(200).map(|(_, dto)| dto).collect();
    Json(json!({ "runs": runs })).into_response()
}

pub(crate) fn parse_id(value: &str) -> Option<i64> {
    value.parse::<i64>().ok().filter(|id| *id >= 1)
}

pub(crate) fn parse_paging(params: &HashMap<String, String>, limit_max: i64) -> Option<(i64, i64)> {
    let offset = match params.get("offset") {
        Some(value) => value.parse::<i64>().ok()?,
        None => 0,
    };
    let limit = match params.get("limit") {
        Some(value) => value.parse::<i64>().ok()?,
        None => 100,
    };
    if !(0..=9_007_199_254_740_991 - 500).contains(&offset) || !(1..=limit_max).contains(&limit) {
        return None;
    }
    Some((offset, limit))
}

async fn get_replay_run(
    State(state): State<Arc<ReplayState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    if let Err(response) = guard(&state, &headers, false) {
        return response;
    }
    let Some(run_id) = parse_id(&id) else {
        return error_response(StatusCode::BAD_REQUEST, "invalid_id");
    };
    let Some((offset, limit)) = parse_paging(&params, 500) else {
        return error_response(StatusCode::BAD_REQUEST, "invalid_query");
    };
    let Some(page) = get_run_page(&state.pool, run_id, offset, limit)
        .await
        .unwrap_or(None)
    else {
        return error_response(StatusCode::NOT_FOUND, "run not found");
    };
    let run = &page.run;
    let setups: Vec<Value> = page
        .setups_page
        .as_array()
        .map(|values| {
            values
                .iter()
                .map(|setup| setup_dto(setup, &run.exchange_code, &run.category, &run.symbol))
                .collect()
        })
        .unwrap_or_default();
    Json(json!({
        "offset": offset,
        "limit": limit,
        "total": run.setup_count,
        "run": run_dto(run),
        "setups": setups,
    }))
    .into_response()
}

async fn get_quality_intervals(
    State(state): State<Arc<ReplayState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    if let Err(response) = guard(&state, &headers, false) {
        return response;
    }
    let Some(run_id) = parse_id(&id) else {
        return error_response(StatusCode::BAD_REQUEST, "invalid_id");
    };
    let Some((offset, limit)) = parse_paging(&params, 500) else {
        return error_response(StatusCode::BAD_REQUEST, "invalid_query");
    };
    let Some(snapshot) = get_run_intervals(&state.pool, run_id, offset, limit)
        .await
        .unwrap_or(None)
    else {
        return error_response(StatusCode::NOT_FOUND, "run not found");
    };
    if !snapshot.available {
        return Json(json!({
            "intervals": [],
            "offset": offset,
            "limit": limit,
            "total": 0,
            "available": false,
        }))
        .into_response();
    }
    let intervals: Vec<Value> = snapshot
        .intervals
        .iter()
        .map(|(from, to, reason)| json!({ "fromUnix": from, "toUnix": to, "reason": reason }))
        .collect();
    Json(json!({
        "intervals": intervals,
        "offset": offset,
        "limit": limit,
        "total": snapshot.total,
        "available": true,
    }))
    .into_response()
}

async fn cancel_run(
    State(state): State<Arc<ReplayState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(response) = guard(&state, &headers, true) {
        return response;
    }
    let Some(run_id) = parse_id(&id) else {
        return error_response(StatusCode::BAD_REQUEST, "invalid_id");
    };
    match cancel_replay_run(&state.pool, run_id).await {
        Ok(None) => error_response(StatusCode::NOT_FOUND, "run not found"),
        Ok(Some(run)) => match get_run_full(&state.pool, run.id).await {
            Ok(Some(full)) => Json(json!({ "run": run_dto(&full) })).into_response(),
            _ => internal_error(),
        },
        Err(_) => internal_error(),
    }
}

async fn purge_runs(State(state): State<Arc<ReplayState>>, headers: HeaderMap) -> Response {
    if let Err(response) = guard(&state, &headers, true) {
        return response;
    }
    match purge_replay_runs(&state.pool).await {
        Ok(setups_db::admission::PurgeOutcome::Active {
            active,
            queued_cancelled,
            running_cancel_requested,
        }) => (
            StatusCode::CONFLICT,
            Json(json!({
                "error": "active_runs",
                "active": active,
                "queuedCancelled": queued_cancelled,
                "runningCancelRequested": running_cancel_requested,
            })),
        )
            .into_response(),
        Ok(setups_db::admission::PurgeOutcome::Purged {
            runs_deleted,
            batches_deleted,
            cache_deleted,
            quality_intervals_deleted,
        }) => Json(json!({
            "runsDeleted": runs_deleted,
            "batchesDeleted": batches_deleted,
            "cacheDeleted": cache_deleted,
            "qualityIntervalsDeleted": quality_intervals_deleted,
        }))
        .into_response(),
        Err(_) => internal_error(),
    }
}

async fn delete_run(
    State(state): State<Arc<ReplayState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(response) = guard(&state, &headers, true) {
        return response;
    }
    let Some(run_id) = parse_id(&id) else {
        return error_response(StatusCode::BAD_REQUEST, "invalid_id");
    };
    match delete_replay_run(&state.pool, run_id).await {
        Ok(None) => error_response(StatusCode::NOT_FOUND, "run not found"),
        Ok(Some(setups_db::admission::DeleteRunOutcome::Active)) => {
            error_response(StatusCode::CONFLICT, "run active")
        }
        Ok(Some(setups_db::admission::DeleteRunOutcome::Deleted {
            quality_intervals_deleted,
        })) => Json(json!({
            "deleted": true,
            "qualityIntervalsDeleted": quality_intervals_deleted,
        }))
        .into_response(),
        Err(_) => internal_error(),
    }
}

#[derive(Deserialize)]
struct PausedBody {
    paused: bool,
}

async fn get_replay_settings(
    State(state): State<Arc<ReplayState>>,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = guard(&state, &headers, false) {
        return response;
    }
    let stored = settings_get(&state.pool, "replay").await.unwrap_or(None);
    let paused = stored
        .as_ref()
        .and_then(|value| value.get("paused"))
        .is_some_and(|paused| *paused == Value::Bool(true));
    Json(json!({ "paused": paused })).into_response()
}

async fn put_replay_settings(
    State(state): State<Arc<ReplayState>>,
    headers: HeaderMap,
    body: String,
) -> Response {
    if let Err(response) = guard(&state, &headers, true) {
        return response;
    }
    let parsed: PausedBody = match serde_json::from_str(&body) {
        Ok(value) => value,
        Err(_) => return error_response(StatusCode::BAD_REQUEST, "invalid_body"),
    };
    let value = json!({ "paused": parsed.paused });
    if settings_set(&state.pool, "replay", &value).await.is_err() {
        return internal_error();
    }
    Json(value).into_response()
}

async fn cache_stats_handler(
    State(state): State<Arc<ReplayState>>,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = guard(&state, &headers, false) {
        return response;
    }
    match cache_stats(&state.pool).await {
        Ok((entries, bytes)) => Json(json!({ "entries": entries, "bytes": bytes })).into_response(),
        Err(_) => internal_error(),
    }
}

async fn clear_instrument_cache(
    State(state): State<Arc<ReplayState>>,
    headers: HeaderMap,
    Path((exchange, category, symbol)): Path<(String, String, String)>,
) -> Response {
    if let Err(response) = guard(&state, &headers, true) {
        return response;
    }
    match cache_clear_by_instrument(&state.pool, &exchange, &category, &symbol).await {
        Ok(removed) => Json(json!({ "removed": removed })).into_response(),
        Err(_) => internal_error(),
    }
}

async fn clear_cache_handler(
    State(state): State<Arc<ReplayState>>,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = guard(&state, &headers, true) {
        return response;
    }
    match cache_clear_all(&state.pool).await {
        Ok(removed) => Json(json!({ "removed": removed })).into_response(),
        Err(_) => internal_error(),
    }
}

pub fn replay_router(state: Arc<ReplayState>) -> Router {
    Router::new()
        .route(
            "/v1/replay-runs",
            post(create_replay_runs).get(list_replay_runs),
        )
        .route("/v1/replay-runs/purge", post(purge_runs))
        .route("/v1/replay-preflight", post(preflight_replay))
        .route(
            "/v1/replay-runs/{id}",
            get(get_replay_run).delete(delete_run),
        )
        .route("/v1/replay-runs/{id}/cancel", post(cancel_run))
        .route(
            "/v1/replay-runs/{id}/quality-intervals",
            get(get_quality_intervals),
        )
        .route(
            "/v1/settings/replay",
            get(get_replay_settings).put(put_replay_settings),
        )
        .route("/v1/replay-cache/stats", get(cache_stats_handler))
        .route(
            "/v1/replay-cache/{exchange}/{category}/{symbol}",
            delete(clear_instrument_cache),
        )
        .route("/v1/replay-cache", delete(clear_cache_handler))
        .with_state(state)
}

fn ceil_to_minute(value: i64) -> i64 {
    (value + 59) / 60 * 60
}

fn validate_coverage(
    coverage: &setups_mdclient::CoverageRangeDto,
    instrument_id: i64,
    from_unix: i64,
    to_unix: i64,
    split_unix: i64,
) -> bool {
    let counters = [
        coverage.expected_minutes,
        coverage.present_minutes,
        coverage.missing_minutes,
        coverage.warmup_expected_minutes,
        coverage.warmup_present_minutes,
        coverage.signal_expected_minutes,
        coverage.signal_present_minutes,
        coverage.gap_count,
    ];
    coverage.instrument_id == instrument_id
        && coverage.from_unix == ceil_to_minute(from_unix)
        && coverage.to_unix == ceil_to_minute(to_unix)
        && coverage.split_unix == ceil_to_minute(split_unix)
        && coverage.gaps.len() <= 50
        && counters.iter().all(|value| *value >= 0)
        && coverage.expected_minutes == (coverage.to_unix - coverage.from_unix) / 60
        && coverage.missing_minutes == coverage.expected_minutes - coverage.present_minutes
        && coverage.warmup_expected_minutes == (coverage.split_unix - coverage.from_unix) / 60
        && coverage.signal_expected_minutes == (coverage.to_unix - coverage.split_unix) / 60
        && coverage.warmup_present_minutes <= coverage.warmup_expected_minutes
        && coverage.signal_present_minutes <= coverage.signal_expected_minutes
        && coverage.present_minutes
            == coverage.warmup_present_minutes + coverage.signal_present_minutes
        && coverage.offset == 0
        && coverage.limit == 50
        && coverage.gaps.len() as i64 == coverage.gap_count.min(50)
        && (coverage.gap_count == 0) == (coverage.missing_minutes == 0)
        && chrono::DateTime::parse_from_rfc3339(&coverage.checked_at).is_ok()
        && coverage.gaps.iter().enumerate().all(|(index, gap)| {
            gap.from_unix % 60 == 0
                && gap.to_unix % 60 == 0
                && gap.from_unix >= coverage.from_unix
                && gap.to_unix <= coverage.to_unix
                && gap.from_unix < gap.to_unix
                && (index == 0 || gap.from_unix >= coverage.gaps[index - 1].to_unix)
        })
}

struct PreflightItem {
    exchange: String,
    category: String,
    symbol: String,
    cache_hit: bool,
    status: &'static str,
    computed_at: Option<String>,
    quality: Option<Value>,
    setup_count: Option<i64>,
    coverage: Option<Value>,
    error: Option<&'static str>,
}

enum PreflightOutcome {
    NotFound,
    Ready(setups_mdclient::CoverageRangeDto),
    Invalid,
    Unavailable,
}

async fn preflight_replay(
    State(state): State<Arc<ReplayState>>,
    headers: HeaderMap,
    body: String,
) -> Response {
    if let Err(response) = guard(&state, &headers, true) {
        return response;
    }
    let parsed: ReplayRequestBody = match serde_json::from_str(&body) {
        Ok(value) => value,
        Err(_) => return error_response(StatusCode::BAD_REQUEST, "invalid_body"),
    };
    if parsed.instruments.len() > 100 {
        return error_response(StatusCode::BAD_REQUEST, "invalid_body");
    }
    let prepared = match prepare_replay_request(&state.pool, &parsed).await {
        Ok(Ok(prepared)) => prepared,
        Ok(Err(message)) => return error_response(StatusCode::BAD_REQUEST, &message),
        Err(_) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal_error"),
    };
    match run_preflight(&state, &prepared).await {
        Ok(value) => Json(value).into_response(),
        Err(_) => error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal_error"),
    }
}

async fn run_preflight(state: &ReplayState, prepared: &PreparedReplay) -> Result<Value, DbError> {
    let required_history_minutes = i64::from(prepared.params.baseline_long_days) * 1440;
    let keys: Vec<String> = prepared
        .instruments
        .iter()
        .map(|instrument| {
            setups_db::canonical_cache_key(&setups_db::CacheQuery {
                exchange_code: instrument.exchange.clone(),
                category: instrument.category.clone(),
                symbol: instrument.symbol.clone(),
                algorithm_id: SPIKE_ALGORITHM_ID.to_string(),
                algorithm_version: SPIKE_ALGORITHM_VERSION.to_string(),
                params: prepared.params.clone(),
                from_unix: prepared.from_unix,
                to_unix: prepared.to_unix,
            })
        })
        .collect();
    let cached = setups_db::runs::preflight_cache_metadata(&state.pool, &keys).await?;
    let by_key: HashMap<String, &setups_db::runs::PreflightCacheEntry> = cached
        .iter()
        .map(|entry| (entry.cache_key.clone(), entry))
        .collect();
    let mut items: Vec<PreflightItem> = prepared
        .instruments
        .iter()
        .enumerate()
        .map(|(index, instrument)| {
            let instrument_item =
                |cache_hit, status, computed_at, quality, setup_count, coverage, error| {
                    PreflightItem {
                        exchange: instrument.exchange.clone(),
                        category: instrument.category.clone(),
                        symbol: instrument.symbol.clone(),
                        cache_hit,
                        status,
                        computed_at,
                        quality,
                        setup_count,
                        coverage,
                        error,
                    }
                };
            match by_key.get(&keys[index]) {
                Some(entry) => instrument_item(
                    true,
                    "cached",
                    Some(iso8601(&entry.computed_at)),
                    entry.quality.clone(),
                    Some(entry.setup_count),
                    None,
                    None,
                ),
                None => instrument_item(
                    false,
                    "unavailable",
                    None,
                    None,
                    None,
                    None,
                    Some("market_data_unavailable"),
                ),
            }
        })
        .collect();
    let miss_indices: Vec<usize> = items
        .iter()
        .enumerate()
        .filter(|(_, item)| !item.cache_hit)
        .map(|(index, _)| index)
        .collect();
    if !miss_indices.is_empty() {
        let catalog_result = MarketDataClient::new(&state.market_url, &state.market_key)
            .instruments()
            .await;
        if let Ok(catalog) = catalog_result {
            let identities: HashMap<(String, String, String), i64> = catalog
                .iter()
                .map(|item| {
                    (
                        (
                            item.exchange.clone(),
                            item.category.clone(),
                            item.symbol.clone(),
                        ),
                        item.id,
                    )
                })
                .collect();
            let from_unix = prepared.from_unix - required_history_minutes * 60;
            let to_unix = prepared.to_unix;
            let split_unix = prepared.from_unix;
            let chunk_count = miss_indices.len().min(4);
            let chunk_size = miss_indices.len().div_ceil(chunk_count);
            let mut handles = Vec::new();
            for chunk in miss_indices.chunks(chunk_size) {
                let market_url = state.market_url.clone();
                let market_key = state.market_key.clone();
                let identities = identities.clone();
                let requests: Vec<(usize, (String, String, String))> = chunk
                    .iter()
                    .map(|&index| {
                        let item = &items[index];
                        (
                            index,
                            (
                                item.exchange.clone(),
                                item.category.clone(),
                                item.symbol.clone(),
                            ),
                        )
                    })
                    .collect();
                handles.push(tokio::spawn(async move {
                    let client = MarketDataClient::new(&market_url, &market_key);
                    let mut outcomes = Vec::new();
                    for (index, identity) in requests {
                        let outcome = match identities.get(&identity) {
                            None => PreflightOutcome::NotFound,
                            Some(&instrument_id) => match client
                                .coverage_range(
                                    instrument_id,
                                    from_unix,
                                    to_unix,
                                    split_unix,
                                    0,
                                    50,
                                )
                                .await
                            {
                                Ok(coverage) => {
                                    if validate_coverage(
                                        &coverage,
                                        instrument_id,
                                        from_unix,
                                        to_unix,
                                        split_unix,
                                    ) {
                                        PreflightOutcome::Ready(coverage)
                                    } else {
                                        PreflightOutcome::Invalid
                                    }
                                }
                                Err(setups_mdclient::MdClientError::Http {
                                    status: 404, ..
                                }) => PreflightOutcome::NotFound,
                                Err(_) => PreflightOutcome::Unavailable,
                            },
                        };
                        outcomes.push((index, outcome));
                    }
                    outcomes
                }));
            }
            for handle in handles {
                let Ok(outcomes) = handle.await else {
                    continue;
                };
                for (index, outcome) in outcomes {
                    let item = &mut items[index];
                    match outcome {
                        PreflightOutcome::NotFound => {
                            item.status = "not_found";
                            item.error = Some("instrument_not_found");
                        }
                        PreflightOutcome::Ready(coverage) => {
                            item.status = "ready";
                            item.coverage =
                                Some(serde_json::to_value(&coverage).unwrap_or(Value::Null));
                            item.error = None;
                        }
                        PreflightOutcome::Invalid => {
                            item.error = Some("invalid_coverage_response");
                        }
                        PreflightOutcome::Unavailable => {}
                    }
                }
            }
        }
    }
    let can_run = items
        .iter()
        .all(|item| item.status == "cached" || item.status == "ready");
    let results: Vec<Value> = items
        .iter()
        .map(|item| {
            let mut value = json!({
                "exchange": item.exchange,
                "category": item.category,
                "symbol": item.symbol,
                "cacheHit": item.cache_hit,
                "status": item.status,
            });
            let object = value.as_object_mut().expect("object literal");
            if let Some(computed_at) = &item.computed_at {
                object.insert("computedAt".into(), json!(computed_at));
            }
            if item.cache_hit {
                object.insert(
                    "quality".into(),
                    item.quality.clone().unwrap_or(Value::Null),
                );
            }
            if let Some(setup_count) = item.setup_count {
                object.insert("setupCount".into(), json!(setup_count));
            }
            if let Some(coverage) = &item.coverage {
                object.insert("coverage".into(), coverage.clone());
            }
            if let Some(error) = item.error {
                object.insert("error".into(), json!(error));
            }
            value
        })
        .collect();
    Ok(json!({
        "checkedAt": iso8601(&Utc::now()),
        "algorithmVersion": SPIKE_ALGORITHM_VERSION,
        "requiredHistoryMinutes": required_history_minutes,
        "canRun": can_run,
        "request": {
            "instruments": prepared.instruments.iter().map(|instrument| json!({
                "exchange": instrument.exchange,
                "category": instrument.category,
                "symbol": instrument.symbol,
            })).collect::<Vec<_>>(),
            "fromUnix": prepared.from_unix,
            "toUnix": prepared.to_unix,
            "algorithmId": SPIKE_ALGORITHM_ID,
            "algorithmVersion": SPIKE_ALGORITHM_VERSION,
            "params": serde_json::to_value(&prepared.params).unwrap_or(Value::Null),
            "async": true,
        },
        "results": results,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_coverage() -> setups_mdclient::CoverageRangeDto {
        setups_mdclient::CoverageRangeDto {
            instrument_id: 7,
            from_unix: 0,
            to_unix: 600,
            split_unix: 300,
            expected_minutes: 10,
            present_minutes: 8,
            missing_minutes: 2,
            warmup_expected_minutes: 5,
            warmup_present_minutes: 4,
            signal_expected_minutes: 5,
            signal_present_minutes: 4,
            gaps: vec![setups_mdclient::CoverageGap {
                from_unix: 120,
                to_unix: 240,
            }],
            gap_count: 1,
            offset: 0,
            limit: 50,
            checked_at: "2026-01-01T00:00:00Z".to_string(),
        }
    }

    #[test]
    fn coverage_validation_accepts_consistent_response() {
        let coverage = sample_coverage();
        assert!(validate_coverage(&coverage, 7, 0, 600, 300));
    }

    #[test]
    fn coverage_validation_rejects_invariant_violations() {
        let mut coverage = sample_coverage();
        coverage.from_unix = 1;
        assert!(!validate_coverage(&coverage, 7, 0, 600, 300));

        let mut coverage = sample_coverage();
        coverage.missing_minutes = 3;
        assert!(!validate_coverage(&coverage, 7, 0, 600, 300));

        let mut coverage = sample_coverage();
        coverage.gaps = vec![
            setups_mdclient::CoverageGap {
                from_unix: 120,
                to_unix: 300,
            },
            setups_mdclient::CoverageGap {
                from_unix: 240,
                to_unix: 360,
            },
        ];
        assert!(!validate_coverage(&coverage, 7, 0, 600, 300));

        let mut coverage = sample_coverage();
        coverage.gap_count = 0;
        assert!(!validate_coverage(&coverage, 7, 0, 600, 300));

        let mut coverage = sample_coverage();
        coverage.instrument_id = 8;
        assert!(!validate_coverage(&coverage, 7, 0, 600, 300));

        let mut coverage = sample_coverage();
        coverage.present_minutes = 9;
        coverage.warmup_present_minutes = 5;
        coverage.signal_present_minutes = 4;
        assert!(!validate_coverage(&coverage, 7, 0, 600, 300));
    }

    fn params_with(overlay: Value) -> SpikeParams {
        let mut value = serde_json::to_value(SpikeParams::default()).expect("default params");
        merge_objects(&mut value, &overlay);
        serde_json::from_value(value).expect("merged params")
    }

    #[test]
    fn validation_rules_match_ts() {
        assert_eq!(validate_spike_params(&SpikeParams::default()), None);
        assert_eq!(
            validate_spike_params(&params_with(json!({ "priceGrowthPct": "1001" }))),
            Some("Порог вне допустимого диапазона")
        );
        assert_eq!(
            validate_spike_params(&params_with(json!({ "volumeMultiplier": "0" }))),
            Some("Порог вне допустимого диапазона")
        );
        assert_eq!(
            validate_spike_params(&params_with(json!({ "maxGapPercent": "100.5" }))),
            Some("Порог вне допустимого диапазона")
        );
        assert_eq!(
            validate_spike_params(&params_with(json!({
                "baselineShortDays": 90,
                "baselineLongDays": 30
            }))),
            Some("Короткая база не должна превышать длинную")
        );
        assert_eq!(
            validate_spike_params(&params_with(json!({
                "windowMinutes": 1440,
                "baselineShortDays": 1
            }))),
            Some("Окно роста должно быть короче базового окна")
        );
    }

    #[test]
    fn merge_overlays_only_provided_fields() {
        let mut base = serde_json::to_value(SpikeParams::default()).expect("default params");
        let before = base.clone();
        merge_objects(&mut base, &json!({ "cooldownMinutes": 42 }));
        assert_eq!(base["cooldownMinutes"], json!(42));
        assert_eq!(base["windowMinutes"], before["windowMinutes"]);
    }
}
