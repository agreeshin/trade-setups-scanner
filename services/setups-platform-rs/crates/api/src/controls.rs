use crate::auth::SessionAuth;
use crate::replay::{guard_auth, merge_objects, validate_spike_params};
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde_json::{Value, json};
use setups_db::controls::{
    instrument_list_by_kind, list_instrument_lists, replace_instrument_list,
};
use setups_db::{DbError, settings_get, settings_set};
use setups_domain::{SPIKE_ALGORITHM_ID, SPIKE_ALGORITHM_VERSION, SpikeParams};
use setups_mdclient::MarketDataClient;
use sqlx::postgres::PgPool;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

pub struct ControlsState {
    pub pool: PgPool,
    pub auth: Arc<SessionAuth>,
    pub market_url: String,
    pub market_key: String,
}

fn error_response(status: StatusCode, code: &str) -> Response {
    (
        status,
        [("cache-control", "no-store")],
        Json(json!({ "error": code })),
    )
        .into_response()
}

fn internal_error() -> Response {
    error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal_error")
}

#[allow(clippy::result_large_err)]
fn guard(state: &ControlsState, headers: &HeaderMap, mutation: bool) -> Result<(), Response> {
    guard_auth(&state.auth, headers, mutation)
}

fn algorithm_fields() -> Value {
    json!([
    { "key": "priceGrowthPct", "label": "Рост цены", "type": "decimal", "min": 0, "max": 1000, "unit": "%" },
    { "key": "windowMinutes", "label": "Окно роста цены и оборота", "type": "integer", "min": 1, "max": 1440, "unit": "мин" },
    { "key": "volumeMultiplier", "label": "Множитель оборота", "type": "decimal", "min": 0.000001, "max": 1000, "unit": "×" },
    { "key": "baselineLongDays", "label": "Длинная база", "type": "integer", "min": 1, "max": 90, "unit": "дней" },
    { "key": "baselineShortDays", "label": "Короткая база", "type": "integer", "min": 1, "max": 90, "unit": "дней" },
    { "key": "cooldownMinutes", "label": "Интервал между сетапами", "type": "integer", "min": 0, "max": 10080, "unit": "мин" },
    { "key": "maxGapPercent", "label": "Допустимая доля пропусков", "type": "decimal", "min": 0, "max": 100, "unit": "%" },
    { "key": "maxConsecutiveGapMinutes", "label": "Максимальный непрерывный пропуск", "type": "integer", "min": 0, "max": 1440, "unit": "мин" },
    { "key": "requiredRecentMinutes", "label": "Последние минуты без пропусков", "type": "integer", "min": 0, "max": 1440, "unit": "мин" }
    ])
}

fn algorithm_dto(id: &str, stored: Option<Value>) -> Value {
    let mut params = serde_json::to_value(SpikeParams::default()).unwrap_or(Value::Null);
    if let Some(stored_params) = stored.as_ref().and_then(|value| value.get("params")) {
        merge_objects(&mut params, stored_params);
    }
    let enabled = stored
        .as_ref()
        .and_then(|value| value.get("enabled"))
        .is_none_or(|value| *value != Value::Bool(false));
    json!({
        "id": id,
        "name": "Всплеск цены и оборота",
        "version": SPIKE_ALGORITHM_VERSION,
        "description": "Нейтральный WATCH: рост цены и оборот выше короткой или длинной базы. Общая конфигурация для истории и live.",
        "params": params,
        "enabled": enabled,
        "fields": algorithm_fields(),
    })
}

async fn get_algorithm(pool: &PgPool, id: &str) -> Result<Option<Value>, DbError> {
    if id != SPIKE_ALGORITHM_ID {
        return Ok(None);
    }
    let stored = settings_get(pool, &format!("algorithm:{id}")).await?;
    Ok(Some(algorithm_dto(id, stored)))
}

async fn list_algorithms(State(state): State<Arc<ControlsState>>, headers: HeaderMap) -> Response {
    if let Err(response) = guard(&state, &headers, false) {
        return response;
    }
    match get_algorithm(&state.pool, SPIKE_ALGORITHM_ID).await {
        Ok(Some(algorithm)) => Json(json!({ "algorithms": [algorithm] })).into_response(),
        Ok(None) => Json(json!({ "algorithms": [] })).into_response(),
        Err(_) => internal_error(),
    }
}

async fn export_config(State(state): State<Arc<ControlsState>>, headers: HeaderMap) -> Response {
    if let Err(response) = guard(&state, &headers, false) {
        return response;
    }
    let algorithm = match get_algorithm(&state.pool, SPIKE_ALGORITHM_ID).await {
        Ok(value) => value,
        Err(_) => return internal_error(),
    };
    let notifications = settings_get(&state.pool, "notifications")
        .await
        .unwrap_or(None)
        .unwrap_or(json!({}));
    let live = settings_get(&state.pool, "live")
        .await
        .unwrap_or(None)
        .unwrap_or(json!({}));
    let replay = settings_get(&state.pool, "replay")
        .await
        .unwrap_or(None)
        .unwrap_or(json!({}));
    let lists = match list_instrument_lists(&state.pool, 2000).await {
        Ok(value) => value,
        Err(_) => return internal_error(),
    };
    let algorithms_entry = algorithm.map(|value| {
        json!({
            "id": value["id"],
            "version": value["version"],
            "params": value["params"],
            "enabled": value["enabled"],
        })
    });
    let body = json!({
        "formatVersion": 1,
        "algorithms": algorithms_entry.map(|entry| vec![entry]).unwrap_or_default(),
        "notifications": { "maxAgeSeconds": notifications.get("maxAgeSeconds").cloned().unwrap_or(json!(60)) },
        "live": { "enabled": live.get("enabled") == Some(&Value::Bool(true)) },
        "replay": { "paused": replay.get("paused") == Some(&Value::Bool(true)) },
        "instrumentLists": lists.iter().map(|row| json!({
            "list_type": row.list_type,
            "exchange_code": row.exchange_code,
            "category": row.category,
            "symbol": row.symbol,
        })).collect::<Vec<_>>(),
    });
    (
        [(
            "content-disposition",
            "attachment; filename=\"platform-config.json\"",
        )],
        Json(body),
    )
        .into_response()
}

async fn update_algorithm(
    State(state): State<Arc<ControlsState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: String,
) -> Response {
    if let Err(response) = guard(&state, &headers, true) {
        return response;
    }
    if get_algorithm(&state.pool, &id)
        .await
        .ok()
        .flatten()
        .is_none()
    {
        return error_response(StatusCode::NOT_FOUND, "unknown_algorithm");
    }
    let parsed: AlgorithmUpdateBody = match serde_json::from_str(&body) {
        Ok(value) => value,
        Err(_) => return error_response(StatusCode::BAD_REQUEST, "invalid_body"),
    };
    if let Some(error) = validate_spike_params(&parsed.params) {
        return error_response(StatusCode::BAD_REQUEST, error);
    }
    let value = json!({ "params": parsed.params, "enabled": parsed.enabled });
    if settings_set(&state.pool, &format!("algorithm:{id}"), &value)
        .await
        .is_err()
    {
        return internal_error();
    }
    match get_algorithm(&state.pool, &id).await {
        Ok(Some(algorithm)) => Json(algorithm).into_response(),
        _ => internal_error(),
    }
}

#[derive(serde::Deserialize)]
struct AlgorithmUpdateBody {
    params: SpikeParams,
    enabled: bool,
}

async fn get_ui_settings(State(state): State<Arc<ControlsState>>, headers: HeaderMap) -> Response {
    if let Err(response) = guard(&state, &headers, false) {
        return response;
    }
    let value = settings_get(&state.pool, "ui")
        .await
        .unwrap_or(None)
        .unwrap_or(json!({}));
    Json(json!({ "value": value })).into_response()
}

async fn put_ui_settings(
    State(state): State<Arc<ControlsState>>,
    headers: HeaderMap,
    body: String,
) -> Response {
    if let Err(response) = guard(&state, &headers, true) {
        return response;
    }
    let parsed: UiSettingsBody = match serde_json::from_str(&body) {
        Ok(value) => value,
        Err(_) => return error_response(StatusCode::BAD_REQUEST, "invalid_body"),
    };
    if settings_set(&state.pool, "ui", &parsed.value)
        .await
        .is_err()
    {
        return internal_error();
    }
    Json(json!({ "value": parsed.value })).into_response()
}

#[derive(serde::Deserialize)]
struct UiSettingsBody {
    value: Value,
}

async fn get_live_settings(
    State(state): State<Arc<ControlsState>>,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = guard(&state, &headers, false) {
        return response;
    }
    let stored = settings_get(&state.pool, "live")
        .await
        .unwrap_or(None)
        .unwrap_or(json!({}));
    let enabled = stored.get("enabled") == Some(&Value::Bool(true));
    Json(json!({ "enabled": enabled })).into_response()
}

async fn put_live_settings(
    State(state): State<Arc<ControlsState>>,
    headers: HeaderMap,
    body: String,
) -> Response {
    if let Err(response) = guard(&state, &headers, true) {
        return response;
    }
    let parsed: LiveSettingsBody = match serde_json::from_str(&body) {
        Ok(value) => value,
        Err(_) => return error_response(StatusCode::BAD_REQUEST, "invalid_body"),
    };
    let value = json!({ "enabled": parsed.enabled });
    if settings_set(&state.pool, "live", &value).await.is_err() {
        return internal_error();
    }
    Json(value).into_response()
}

#[derive(serde::Deserialize)]
struct LiveSettingsBody {
    enabled: bool,
}

async fn live_status(State(state): State<Arc<ControlsState>>, headers: HeaderMap) -> Response {
    if let Err(response) = guard(&state, &headers, false) {
        return response;
    }
    let live = settings_get(&state.pool, "live")
        .await
        .unwrap_or(None)
        .unwrap_or(json!({}));
    let enabled = live.get("enabled") == Some(&Value::Bool(true));
    let mut body = json!({
        "enabled": enabled,
        "running": false,
        "lastScanAt": Value::Null,
        "lastError": Value::Null,
    });
    if let Some(stored) = settings_get(&state.pool, "live-status")
        .await
        .unwrap_or(None)
        && let Some(stored_object) = stored.as_object()
        && let Some(target) = body.as_object_mut()
    {
        for (key, value) in stored_object {
            target.insert(key.clone(), value.clone());
        }
    }
    Json(body).into_response()
}

async fn instruments(State(state): State<Arc<ControlsState>>, headers: HeaderMap) -> Response {
    if let Err(response) = guard(&state, &headers, false) {
        return response;
    }
    match MarketDataClient::new(&state.market_url, &state.market_key)
        .instruments()
        .await
    {
        Ok(catalog) => {
            let instruments: Vec<Value> = catalog
                .iter()
                .map(|item| {
                    json!({
                        "id": item.id,
                        "exchange": item.exchange,
                        "category": item.category,
                        "symbol": item.symbol,
                        "status": item.status,
                    })
                })
                .collect();
            Json(json!({ "instruments": instruments })).into_response()
        }
        Err(_) => internal_error(),
    }
}

fn instrument_list_invalid(
    catalog: &[(String, String, String, String)],
    kind: &str,
    instruments: &[(String, String, String)],
) -> bool {
    let eligible: HashSet<String> = catalog
        .iter()
        .filter(|(_, _, _, status)| kind == "history" || status == "active")
        .map(|(exchange, category, symbol, _)| format!("{exchange}/{category}/{symbol}"))
        .collect();
    let keys: Vec<String> = instruments
        .iter()
        .map(|(exchange, category, symbol)| format!("{exchange}/{category}/{symbol}"))
        .collect();
    let unique: HashSet<&String> = keys.iter().collect();
    keys.iter().any(|key| !eligible.contains(key)) || unique.len() != keys.len()
}

async fn get_instrument_list(
    State(state): State<Arc<ControlsState>>,
    headers: HeaderMap,
    Path(kind): Path<String>,
) -> Response {
    if let Err(response) = guard(&state, &headers, false) {
        return response;
    }
    if kind != "live" && kind != "history" {
        return error_response(StatusCode::BAD_REQUEST, "invalid_kind");
    }
    let stored = settings_get(&state.pool, &format!("instrument-list:{kind}"))
        .await
        .unwrap_or(None)
        .unwrap_or(json!({}));
    let configured = stored.get("configured") == Some(&Value::Bool(true));
    let rows = match instrument_list_by_kind(&state.pool, &kind).await {
        Ok(value) => value,
        Err(_) => return internal_error(),
    };
    let instruments: Vec<Value> = rows
        .iter()
        .map(|row| {
            json!({
                "exchange": row.exchange_code,
                "category": row.category,
                "symbol": row.symbol,
            })
        })
        .collect();
    Json(json!({ "configured": configured, "instruments": instruments })).into_response()
}

#[derive(serde::Deserialize)]
struct InstrumentListBody {
    instruments: Vec<InstrumentListItem>,
}

#[derive(serde::Deserialize)]
struct InstrumentListItem {
    exchange: String,
    category: String,
    symbol: String,
}

async fn put_instrument_list(
    State(state): State<Arc<ControlsState>>,
    headers: HeaderMap,
    Path(kind): Path<String>,
    body: String,
) -> Response {
    if let Err(response) = guard(&state, &headers, true) {
        return response;
    }
    if kind != "live" && kind != "history" {
        return error_response(StatusCode::BAD_REQUEST, "invalid_kind");
    }
    let parsed: InstrumentListBody = match serde_json::from_str(&body) {
        Ok(value) => value,
        Err(_) => return error_response(StatusCode::BAD_REQUEST, "invalid_body"),
    };
    if parsed.instruments.len() > 1000 {
        return error_response(StatusCode::BAD_REQUEST, "invalid_body");
    }
    let catalog = match MarketDataClient::new(&state.market_url, &state.market_key)
        .instruments()
        .await
    {
        Ok(value) => value,
        Err(_) => return internal_error(),
    };
    let catalog_tuples: Vec<(String, String, String, String)> = catalog
        .iter()
        .map(|item| {
            (
                item.exchange.clone(),
                item.category.clone(),
                item.symbol.clone(),
                item.status.clone(),
            )
        })
        .collect();
    let list_tuples: Vec<(String, String, String)> = parsed
        .instruments
        .iter()
        .map(|item| {
            (
                item.exchange.clone(),
                item.category.clone(),
                item.symbol.clone(),
            )
        })
        .collect();
    if instrument_list_invalid(&catalog_tuples, &kind, &list_tuples) {
        return error_response(
            StatusCode::BAD_REQUEST,
            "unknown_inactive_or_duplicate_instruments",
        );
    }
    if replace_instrument_list(&state.pool, &kind, &list_tuples)
        .await
        .is_err()
    {
        return internal_error();
    }
    let value = json!({ "configured": true });
    if settings_set(&state.pool, &format!("instrument-list:{kind}"), &value)
        .await
        .is_err()
    {
        return internal_error();
    }
    let instruments: Vec<Value> = parsed
        .instruments
        .iter()
        .map(|item| {
            json!({
                "exchange": item.exchange,
                "category": item.category,
                "symbol": item.symbol,
            })
        })
        .collect();
    Json(json!({ "configured": true, "instruments": instruments })).into_response()
}

async fn platform_status(State(state): State<Arc<ControlsState>>, headers: HeaderMap) -> Response {
    if let Err(response) = guard(&state, &headers, false) {
        return response;
    }
    let market_data = match MarketDataClient::new(&state.market_url, &state.market_key)
        .ready()
        .await
    {
        Ok(true) => "ready",
        Ok(false) => "not_ready",
        Err(_) => "unreachable",
    };
    Json(json!({ "platform": "ready", "marketData": market_data })).into_response()
}

async fn history_proxy(
    state: &ControlsState,
    headers: &HeaderMap,
    method: &str,
    template: &str,
    params: HashMap<String, String>,
    body: Option<Value>,
    collection: bool,
) -> Response {
    let mutation = method != "GET";
    if let Err(response) = guard(state, headers, mutation) {
        return response;
    }
    let mut path = template.to_string();
    for (name, value) in &params {
        if value.parse::<i64>().map(|id| id < 1).unwrap_or(true) {
            return error_response(StatusCode::BAD_REQUEST, "invalid_id");
        }
        path = path.replace(&format!("{{{name}}}"), value);
    }
    let client = MarketDataClient::new(&state.market_url, &state.market_key);
    match client.proxy(method, &path, body.as_ref()).await {
        Ok((status, payload)) if (200..300).contains(&status) => {
            if collection
                && let Some(instrument_id) = params.get("instrumentId")
                && let Some(payload) = &body
            {
                let _ = settings_set(&state.pool, &format!("collection:{instrument_id}"), payload)
                    .await;
            }
            Json(payload.unwrap_or(Value::Null)).into_response()
        }
        Ok((status, _)) => {
            let code = if status >= 500 { 502 } else { status };
            let status_code = StatusCode::from_u16(code).unwrap_or(StatusCode::BAD_GATEWAY);
            error_response(status_code, "history_service_request_failed")
        }
        Err(_) => error_response(StatusCode::BAD_GATEWAY, "history_service_unavailable"),
    }
}

fn parse_body(value: &str) -> Option<Value> {
    serde_json::from_str(value).ok()
}

async fn h_coverage(
    State(state): State<Arc<ControlsState>>,
    headers: HeaderMap,
    Path(params): Path<HashMap<String, String>>,
) -> Response {
    history_proxy(
        &state,
        &headers,
        "GET",
        "/instruments/{instrumentId}/coverage",
        params,
        None,
        false,
    )
    .await
}

async fn h_coverage_summary(
    State(state): State<Arc<ControlsState>>,
    headers: HeaderMap,
) -> Response {
    history_proxy(
        &state,
        &headers,
        "GET",
        "/instruments/coverage-summary",
        HashMap::new(),
        None,
        false,
    )
    .await
}

async fn h_listing(State(state): State<Arc<ControlsState>>, headers: HeaderMap) -> Response {
    history_proxy(
        &state,
        &headers,
        "GET",
        "/instruments/listing",
        HashMap::new(),
        None,
        false,
    )
    .await
}

async fn h_backfill_jobs(State(state): State<Arc<ControlsState>>, headers: HeaderMap) -> Response {
    history_proxy(
        &state,
        &headers,
        "GET",
        "/backfill-jobs",
        HashMap::new(),
        None,
        false,
    )
    .await
}

async fn h_backfill_jobs_post(
    State(state): State<Arc<ControlsState>>,
    headers: HeaderMap,
    body: String,
) -> Response {
    let Some(payload) = parse_body(&body) else {
        return error_response(StatusCode::BAD_REQUEST, "invalid_body");
    };
    history_proxy(
        &state,
        &headers,
        "POST",
        "/backfill-jobs",
        HashMap::new(),
        Some(payload),
        false,
    )
    .await
}

async fn h_backfill_batch(
    State(state): State<Arc<ControlsState>>,
    headers: HeaderMap,
    body: String,
) -> Response {
    let Some(payload) = parse_body(&body) else {
        return error_response(StatusCode::BAD_REQUEST, "invalid_body");
    };
    history_proxy(
        &state,
        &headers,
        "POST",
        "/backfill-jobs/batch",
        HashMap::new(),
        Some(payload),
        false,
    )
    .await
}

async fn h_backfill_plans(State(state): State<Arc<ControlsState>>, headers: HeaderMap) -> Response {
    history_proxy(
        &state,
        &headers,
        "GET",
        "/backfill-plans",
        HashMap::new(),
        None,
        false,
    )
    .await
}

async fn h_backfill_plans_post(
    State(state): State<Arc<ControlsState>>,
    headers: HeaderMap,
    body: String,
) -> Response {
    let Some(payload) = parse_body(&body) else {
        return error_response(StatusCode::BAD_REQUEST, "invalid_body");
    };
    history_proxy(
        &state,
        &headers,
        "POST",
        "/backfill-plans",
        HashMap::new(),
        Some(payload),
        false,
    )
    .await
}

async fn h_plan_cancel(
    State(state): State<Arc<ControlsState>>,
    headers: HeaderMap,
    Path(params): Path<HashMap<String, String>>,
) -> Response {
    history_proxy(
        &state,
        &headers,
        "POST",
        "/backfill-plans/{id}/cancel",
        params,
        None,
        false,
    )
    .await
}

async fn h_job_cancel(
    State(state): State<Arc<ControlsState>>,
    headers: HeaderMap,
    Path(params): Path<HashMap<String, String>>,
) -> Response {
    history_proxy(
        &state,
        &headers,
        "POST",
        "/backfill-jobs/{id}/cancel",
        params,
        None,
        false,
    )
    .await
}

async fn h_targets(State(state): State<Arc<ControlsState>>, headers: HeaderMap) -> Response {
    history_proxy(
        &state,
        &headers,
        "GET",
        "/collection-targets",
        HashMap::new(),
        None,
        false,
    )
    .await
}

async fn h_target_put(
    State(state): State<Arc<ControlsState>>,
    headers: HeaderMap,
    Path(params): Path<HashMap<String, String>>,
    body: String,
) -> Response {
    let Some(payload) = parse_body(&body) else {
        return error_response(StatusCode::BAD_REQUEST, "invalid_body");
    };
    history_proxy(
        &state,
        &headers,
        "PUT",
        "/collection-targets/{instrumentId}",
        params,
        Some(payload),
        true,
    )
    .await
}

async fn h_targets_put(
    State(state): State<Arc<ControlsState>>,
    headers: HeaderMap,
    body: String,
) -> Response {
    let Some(payload) = parse_body(&body) else {
        return error_response(StatusCode::BAD_REQUEST, "invalid_body");
    };
    history_proxy(
        &state,
        &headers,
        "PUT",
        "/collection-targets",
        HashMap::new(),
        Some(payload),
        false,
    )
    .await
}

async fn h_gap_repairs(State(state): State<Arc<ControlsState>>, headers: HeaderMap) -> Response {
    history_proxy(
        &state,
        &headers,
        "GET",
        "/gap-repairs",
        HashMap::new(),
        None,
        false,
    )
    .await
}

async fn h_gap_repairs_post(
    State(state): State<Arc<ControlsState>>,
    headers: HeaderMap,
    body: String,
) -> Response {
    let Some(payload) = parse_body(&body) else {
        return error_response(StatusCode::BAD_REQUEST, "invalid_body");
    };
    history_proxy(
        &state,
        &headers,
        "POST",
        "/gap-repairs",
        HashMap::new(),
        Some(payload),
        false,
    )
    .await
}

pub fn controls_router(state: Arc<ControlsState>) -> Router {
    Router::new()
        .route("/v1/algorithms", get(list_algorithms))
        .route("/v1/algorithms/{id}", put(update_algorithm))
        .route("/v1/export/config.json", get(export_config))
        .route("/v1/settings/ui", get(get_ui_settings))
        .route(
            "/v1/settings/ui",
            put(put_ui_settings).layer(DefaultBodyLimit::max(16384)),
        )
        .route(
            "/v1/settings/live",
            get(get_live_settings).put(put_live_settings),
        )
        .route("/v1/live/status", get(live_status))
        .route("/v1/instruments", get(instruments))
        .route(
            "/v1/instrument-lists/{kind}",
            get(get_instrument_list).put(put_instrument_list),
        )
        .route("/v1/status", get(platform_status))
        .route(
            "/v1/history/instruments/{instrumentId}/coverage",
            get(h_coverage),
        )
        .route(
            "/v1/history/instruments/coverage-summary",
            get(h_coverage_summary),
        )
        .route("/v1/history/instruments/listing", get(h_listing))
        .route(
            "/v1/history/backfill-jobs",
            get(h_backfill_jobs).post(h_backfill_jobs_post),
        )
        .route("/v1/history/backfill-jobs/batch", post(h_backfill_batch))
        .route(
            "/v1/history/backfill-plans",
            get(h_backfill_plans).post(h_backfill_plans_post),
        )
        .route(
            "/v1/history/backfill-plans/{id}/cancel",
            post(h_plan_cancel),
        )
        .route("/v1/history/backfill-jobs/{id}/cancel", post(h_job_cancel))
        .route(
            "/v1/history/collection-targets",
            get(h_targets).put(h_targets_put),
        )
        .route(
            "/v1/history/collection-targets/{instrumentId}",
            put(h_target_put),
        )
        .route(
            "/v1/history/gap-repairs",
            get(h_gap_repairs).post(h_gap_repairs_post),
        )
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn algorithm_dto_merges_stored_params_and_respects_enabled() {
        let stored = json!({ "params": { "priceGrowthPct": "20" }, "enabled": false });
        let dto = algorithm_dto(SPIKE_ALGORITHM_ID, Some(stored));
        assert_eq!(dto["params"]["priceGrowthPct"], json!("20"));
        let defaults = serde_json::to_value(SpikeParams::default()).unwrap();
        assert_eq!(dto["params"]["windowMinutes"], defaults["windowMinutes"]);
        assert_eq!(dto["enabled"], json!(false));
    }

    #[test]
    fn algorithm_dto_enabled_defaults_to_true() {
        let dto = algorithm_dto(SPIKE_ALGORITHM_ID, Some(json!({ "params": {} })));
        assert_eq!(dto["enabled"], json!(true));
        let dto = algorithm_dto(SPIKE_ALGORITHM_ID, None);
        assert_eq!(dto["enabled"], json!(true));
    }

    #[test]
    fn instrument_list_rejects_inactive_and_duplicates() {
        let catalog = vec![
            ("e".into(), "c".into(), "s1".into(), "active".into()),
            ("e".into(), "c".into(), "s2".into(), "inactive".into()),
        ];
        let good = vec![("e".into(), "c".into(), "s1".into())];
        assert!(!instrument_list_invalid(&catalog, "history", &good));
        assert!(!instrument_list_invalid(&catalog, "live", &good));
        assert!(instrument_list_invalid(
            &catalog,
            "live",
            &[("e".into(), "c".into(), "s2".into())]
        ));
        assert!(instrument_list_invalid(
            &catalog,
            "history",
            &[
                ("e".into(), "c".into(), "s1".into()),
                ("e".into(), "c".into(), "s1".into()),
            ]
        ));
    }
}
