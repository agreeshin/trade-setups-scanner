use crate::replay::{ReplayState, batch_dto, guard_auth, parse_id, parse_paging, parse_run_name};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{Value, json};
use setups_db::runs::{
    BatchDeleteOutcome, BatchSetupRow, cancel_replay_batch, delete_replay_batch, get_replay_batch,
    get_run_page, live_setups_page, live_setups_search, live_setups_snapshot, rename_replay_batch,
    replay_batch_result_page,
};
use setups_db::{settings_get, settings_set};
use std::collections::HashMap;
use std::sync::Arc;

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
fn guard(state: &ReplayState, headers: &HeaderMap, mutation: bool) -> Result<(), Response> {
    guard_auth(&state.auth, headers, mutation)
}

fn csv_cell(value: &str) -> String {
    let mut rest = value.chars().peekable();
    while matches!(rest.peek(), Some(c) if c.is_whitespace() || c.is_control()) {
        rest.next();
    }
    let formula = matches!(rest.peek(), Some('=' | '+' | '@' | '-'));
    let text = if formula || value.starts_with(['\t', '\r', '\n']) {
        format!("'{value}")
    } else {
        value.to_string()
    };
    format!("\"{}\"", text.replace('"', "\"\""))
}

fn csv_value(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => csv_cell(""),
        Some(Value::String(text)) => csv_cell(text),
        Some(Value::Bool(flag)) => csv_cell(if *flag { "true" } else { "false" }),
        Some(Value::Number(number)) => csv_cell(&number.to_string()),
        Some(_) => csv_cell(""),
    }
}

const REPLAY_CSV_HEADER: &str = "signal,exchange,category,symbol,algorithmId,algorithmVersion,openTimeUnix,closeTimeUnix,closePrice,growthPct,volumeLongRatio,volumeShortRatio,overLong,overShort,missingMinutes,params\r\n";
const LIVE_CSV_HEADER: &str = "exchange,category,symbol,algorithmId,algorithmVersion,openTimeUnix,closeTimeUnix,closePrice\r\n";

fn replay_csv_row(
    exchange: &str,
    category: &str,
    symbol: &str,
    setup: &Value,
    fallback_algorithm_id: &str,
    fallback_algorithm_version: Option<&str>,
    fallback_params: &Value,
) -> String {
    let algorithm_id = setup
        .get("algorithmId")
        .and_then(Value::as_str)
        .unwrap_or(fallback_algorithm_id);
    let algorithm_version = setup
        .get("algorithmVersion")
        .and_then(Value::as_str)
        .or(fallback_algorithm_version);
    let params = setup.get("params").unwrap_or(fallback_params);
    let params_json = serde_json::to_string(params).unwrap_or_default();
    let details = setup.get("details");
    [
        csv_cell("WATCH"),
        csv_cell(exchange),
        csv_cell(category),
        csv_cell(symbol),
        csv_cell(algorithm_id),
        csv_value(
            algorithm_version
                .map(|version| Value::from(version.to_string()))
                .as_ref(),
        ),
        csv_value(setup.get("openTimeUnix")),
        csv_value(setup.get("closeTimeUnix")),
        csv_value(setup.get("closePrice")),
        csv_value(details.and_then(|value| value.get("growthPct"))),
        csv_value(details.and_then(|value| value.get("volumeLongRatio"))),
        csv_value(details.and_then(|value| value.get("volumeShortRatio"))),
        csv_value(details.and_then(|value| value.get("overLong"))),
        csv_value(details.and_then(|value| value.get("overShort"))),
        csv_value(details.and_then(|value| value.get("missingMinutes"))),
        csv_cell(&params_json),
    ]
    .join(",")
        + "\r\n"
}

fn batch_setup_dto(row: &BatchSetupRow) -> Value {
    json!({
        "exchange": row.exchange_code,
        "category": row.category,
        "symbol": row.symbol,
        "algorithmId": row.setup.get("algorithmId").cloned().unwrap_or(json!(row.algorithm_id)),
        "algorithmVersion": row
            .setup
            .get("algorithmVersion")
            .cloned()
            .or_else(|| row.algorithm_version.clone().map(Value::String))
            .unwrap_or(Value::Null),
        "params": row.setup.get("params").cloned().unwrap_or_else(|| row.params.clone()),
        "details": row.setup.get("details").cloned().unwrap_or(Value::Null),
        "openTimeUnix": row.setup.get("openTimeUnix").cloned().unwrap_or(Value::Null),
        "closeTimeUnix": row.setup.get("closeTimeUnix").cloned().unwrap_or(Value::Null),
        "closePrice": row.setup.get("closePrice").cloned().unwrap_or(Value::Null),
    })
}

async fn get_batch(
    State(state): State<Arc<ReplayState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    if let Err(response) = guard(&state, &headers, false) {
        return response;
    }
    let Some(batch_id) = parse_id(&id) else {
        return error_response(StatusCode::BAD_REQUEST, "invalid_id");
    };
    let Some((offset, limit)) = parse_paging(&params, 500) else {
        return error_response(StatusCode::BAD_REQUEST, "invalid_query");
    };
    let batch = match get_replay_batch(&state.pool, batch_id).await {
        Ok(value) => value,
        Err(_) => return internal_error(),
    };
    let Some(batch) = batch else {
        return error_response(StatusCode::NOT_FOUND, "batch not found");
    };
    let (setups, total) = match replay_batch_result_page(&state.pool, batch_id, offset, limit).await
    {
        Ok(Some(page)) => page,
        Ok(None) => return error_response(StatusCode::NOT_FOUND, "batch not found"),
        Err(_) => return internal_error(),
    };
    let setups: Vec<Value> = setups.iter().map(batch_setup_dto).collect();
    Json(json!({
        "offset": offset,
        "limit": limit,
        "total": total,
        "run": batch_dto(&batch),
        "setups": setups,
    }))
    .into_response()
}

async fn cancel_batch(
    State(state): State<Arc<ReplayState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(response) = guard(&state, &headers, true) {
        return response;
    }
    let Some(batch_id) = parse_id(&id) else {
        return error_response(StatusCode::BAD_REQUEST, "invalid_id");
    };
    match cancel_replay_batch(&state.pool, batch_id).await {
        Ok(None) => error_response(StatusCode::NOT_FOUND, "batch not found"),
        Ok(Some((cancelled, cancel_requested))) => {
            Json(json!({ "cancelled": cancelled, "cancelRequested": cancel_requested }))
                .into_response()
        }
        Err(_) => internal_error(),
    }
}

async fn rename_batch(
    State(state): State<Arc<ReplayState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: String,
) -> Response {
    if let Err(response) = guard(&state, &headers, true) {
        return response;
    }
    let Some(batch_id) = parse_id(&id) else {
        return error_response(StatusCode::BAD_REQUEST, "invalid_id");
    };
    let name = match parse_run_name(&body) {
        Ok(name) => name,
        Err(message) => return error_response(StatusCode::BAD_REQUEST, message),
    };
    match rename_replay_batch(&state.pool, batch_id, &name).await {
        Ok(true) => Json(json!({ "ok": true, "name": name })).into_response(),
        Ok(false) => error_response(StatusCode::NOT_FOUND, "batch not found"),
        Err(_) => internal_error(),
    }
}

async fn delete_batch(
    State(state): State<Arc<ReplayState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(response) = guard(&state, &headers, true) {
        return response;
    }
    let Some(batch_id) = parse_id(&id) else {
        return error_response(StatusCode::BAD_REQUEST, "invalid_id");
    };
    match delete_replay_batch(&state.pool, batch_id).await {
        Ok(None) => error_response(StatusCode::NOT_FOUND, "batch not found"),
        Ok(Some(BatchDeleteOutcome::Active)) => {
            error_response(StatusCode::CONFLICT, "batch active")
        }
        Ok(Some(BatchDeleteOutcome::Deleted {
            runs_deleted,
            quality_intervals_deleted,
        })) => Json(json!({
            "deleted": true,
            "runsDeleted": runs_deleted,
            "qualityIntervalsDeleted": quality_intervals_deleted,
        }))
        .into_response(),
        Err(_) => internal_error(),
    }
}

fn csv_response(body: String, filename: &str, no_store: bool) -> Response {
    let disposition = format!("attachment; filename=\"{filename}\"");
    let mut builder = axum::http::Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/csv; charset=utf-8")
        .header(header::CONTENT_DISPOSITION, disposition);
    if no_store {
        builder = builder
            .header(header::CACHE_CONTROL, "no-store")
            .header("x-accel-buffering", "no");
    }
    builder
        .body(axum::body::Body::from(body))
        .unwrap_or_else(|error| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("{error}") })),
            )
                .into_response()
        })
}

async fn export_batch_csv(
    State(state): State<Arc<ReplayState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(response) = guard(&state, &headers, false) {
        return response;
    }
    let Some(batch_id) = parse_id(&id) else {
        return error_response(StatusCode::BAD_REQUEST, "invalid_id");
    };
    let first = match replay_batch_result_page(&state.pool, batch_id, 0, 500).await {
        Ok(Some(page)) => page,
        Ok(None) => return error_response(StatusCode::NOT_FOUND, "batch not found"),
        Err(_) => return internal_error(),
    };
    let (first_rows, total) = first;
    let mut body = String::from(REPLAY_CSV_HEADER);
    let mut rows = first_rows;
    let mut offset: i64 = 0;
    while offset < total {
        for row in &rows {
            body.push_str(&replay_csv_row(
                &row.exchange_code,
                &row.category,
                &row.symbol,
                &row.setup,
                &row.algorithm_id,
                row.algorithm_version.as_deref(),
                &row.params,
            ));
        }
        offset += 500;
        if offset >= total {
            break;
        }
        match replay_batch_result_page(&state.pool, batch_id, offset, (total - offset).min(500))
            .await
        {
            Ok(Some((next, _))) => rows = next,
            _ => return internal_error(),
        }
    }
    csv_response(body, &format!("replay-batch-{batch_id}.csv"), false)
}

async fn export_run_csv(
    State(state): State<Arc<ReplayState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(response) = guard(&state, &headers, false) {
        return response;
    }
    let Some(run_id) = parse_id(&id) else {
        return error_response(StatusCode::BAD_REQUEST, "invalid_id");
    };
    let first = match get_run_page(&state.pool, run_id, 0, 500).await {
        Ok(Some(page)) => page,
        Ok(None) => return error_response(StatusCode::NOT_FOUND, "run not found"),
        Err(_) => return internal_error(),
    };
    let run = &first.run;
    let total = run.setup_count;
    let mut body = String::from(REPLAY_CSV_HEADER);
    let mut offset: i64 = 0;
    while offset < total {
        let page = if offset == 0 {
            first.setups_page.clone()
        } else {
            match get_run_page(&state.pool, run_id, offset, (total - offset).min(500)).await {
                Ok(Some(page)) => page.setups_page,
                _ => return internal_error(),
            }
        };
        let Some(setups) = page.as_array() else {
            break;
        };
        for setup in setups {
            body.push_str(&replay_csv_row(
                &run.exchange_code,
                &run.category,
                &run.symbol,
                setup,
                &run.algorithm_id,
                Some(&run.algorithm_version),
                &run.params,
            ));
        }
        offset += 500;
    }
    csv_response(body, &format!("replay-{run_id}.csv"), false)
}

async fn export_live_setups_csv(
    State(state): State<Arc<ReplayState>>,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = guard(&state, &headers, false) {
        return response;
    }
    let snapshot_id = match live_setups_snapshot(&state.pool).await {
        Ok(value) => value,
        Err(_) => return internal_error(),
    };
    let mut body = String::from(LIVE_CSV_HEADER);
    let mut after: Option<(chrono::DateTime<chrono::Utc>, i64)> = None;
    loop {
        let rows = match live_setups_page(&state.pool, snapshot_id, after, 500).await {
            Ok(value) => value,
            Err(_) => return internal_error(),
        };
        if rows.is_empty() {
            break;
        }
        for row in &rows {
            let line = [
                csv_cell(&row.exchange_code),
                csv_cell(&row.category),
                csv_cell(&row.symbol),
                csv_cell(&row.algorithm_id),
                csv_value(
                    row.algorithm_version
                        .as_ref()
                        .map(|version| Value::from(version.to_string()))
                        .as_ref(),
                ),
                csv_cell(&row.open_time.timestamp().to_string()),
                csv_cell(&row.close_time.timestamp().to_string()),
                csv_cell(&row.close_price),
            ]
            .join(",")
                + "\r\n";
            body.push_str(&line);
        }
        if rows.len() < 500 {
            break;
        }
        let last = rows.last().expect("rows non-empty");
        after = Some((last.close_time, last.id));
    }
    csv_response(body, "live-setups.csv", true)
}

async fn get_notifications_settings(
    State(state): State<Arc<ReplayState>>,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = guard(&state, &headers, false) {
        return response;
    }
    let stored = settings_get(&state.pool, "notifications")
        .await
        .unwrap_or(None)
        .unwrap_or(json!({}));
    let max_age = stored.get("maxAgeSeconds").cloned().unwrap_or(json!(60));
    Json(json!({ "maxAgeSeconds": max_age })).into_response()
}

async fn put_notifications_settings(
    State(state): State<Arc<ReplayState>>,
    headers: HeaderMap,
    body: String,
) -> Response {
    if let Err(response) = guard(&state, &headers, true) {
        return response;
    }
    let parsed: NotificationsBody = match serde_json::from_str(&body) {
        Ok(value) => value,
        Err(_) => return error_response(StatusCode::BAD_REQUEST, "invalid_body"),
    };
    if !(1..=86400).contains(&parsed.max_age_seconds) {
        return error_response(StatusCode::BAD_REQUEST, "invalid_body");
    }
    let value = json!({ "maxAgeSeconds": parsed.max_age_seconds });
    if settings_set(&state.pool, "notifications", &value)
        .await
        .is_err()
    {
        return internal_error();
    }
    Json(value).into_response()
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct NotificationsBody {
    max_age_seconds: i64,
}

async fn get_web_analytics_settings(
    State(state): State<Arc<ReplayState>>,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = guard(&state, &headers, false) {
        return response;
    }
    let value = settings_get(&state.pool, "web-analytics")
        .await
        .unwrap_or(None)
        .unwrap_or(json!({}));
    Json(json!({ "settings": value })).into_response()
}

async fn put_web_analytics_settings(
    State(state): State<Arc<ReplayState>>,
    headers: HeaderMap,
    body: String,
) -> Response {
    if let Err(response) = guard(&state, &headers, true) {
        return response;
    }
    let Ok(value) = serde_json::from_str::<Value>(&body) else {
        return error_response(StatusCode::BAD_REQUEST, "invalid_body");
    };
    if !value.is_object() {
        return error_response(StatusCode::BAD_REQUEST, "invalid_body");
    }
    if settings_set(&state.pool, "web-analytics", &value)
        .await
        .is_err()
    {
        return internal_error();
    }
    Json(json!({ "settings": value })).into_response()
}

async fn get_live_setups(
    State(state): State<Arc<ReplayState>>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    if let Err(response) = guard(&state, &headers, false) {
        return response;
    }
    let offset = match params.get("offset") {
        Some(value) => match value.parse::<i64>() {
            Ok(parsed) if (0..=2_147_483_647).contains(&parsed) => parsed,
            _ => return error_response(StatusCode::BAD_REQUEST, "invalid_query"),
        },
        None => 0,
    };
    let limit = match params.get("limit") {
        Some(value) => match value.parse::<i64>() {
            Ok(parsed) if (1..=500).contains(&parsed) => parsed,
            _ => return error_response(StatusCode::BAD_REQUEST, "invalid_query"),
        },
        None => 500,
    };
    let snapshot_id = match params.get("snapshotId") {
        Some(value) => match value.parse::<i64>() {
            Ok(parsed) if (0..=2_147_483_647).contains(&parsed) => parsed,
            _ => return error_response(StatusCode::BAD_REQUEST, "invalid_query"),
        },
        None => match live_setups_snapshot(&state.pool).await {
            Ok(value) => value,
            Err(_) => return internal_error(),
        },
    };
    let query = params.get("q").map(String::as_str).unwrap_or("");
    if query.len() > 200 {
        return error_response(StatusCode::BAD_REQUEST, "invalid_query");
    }
    let search = query.trim();
    let search = if search.is_empty() {
        None
    } else {
        Some(search)
    };
    let (total, rows) =
        match live_setups_search(&state.pool, snapshot_id, search, offset, limit).await {
            Ok(value) => value,
            Err(_) => return internal_error(),
        };
    let setups: Vec<Value> = rows
        .iter()
        .map(|row| {
            json!({
                "exchange": row.exchange_code,
                "category": row.category,
                "symbol": row.symbol,
                "algorithmId": row.algorithm_id,
                "algorithmVersion": row.algorithm_version,
                "openTimeUnix": row.open_time.timestamp(),
                "params": row.params,
                "details": row.details.clone().unwrap_or(Value::Null),
                "closeTimeUnix": row.close_time.timestamp(),
                "closePrice": row.close_price,
            })
        })
        .collect();
    Json(json!({
        "setups": setups,
        "offset": offset,
        "limit": limit,
        "total": total,
        "snapshotId": snapshot_id,
    }))
    .into_response()
}

pub fn batches_router(state: Arc<ReplayState>) -> Router {
    Router::new()
        .route(
            "/v1/replay-batches/{id}",
            get(get_batch).patch(rename_batch).delete(delete_batch),
        )
        .route(
            "/v1/replay-batches/{id}/cancel",
            axum::routing::post(cancel_batch),
        )
        .route("/v1/replay-batches/{id}/export.csv", get(export_batch_csv))
        .route("/v1/replay-runs/{id}/export.csv", get(export_run_csv))
        .route("/v1/export/setups.csv", get(export_live_setups_csv))
        .route("/v1/setups", get(get_live_setups))
        .route(
            "/v1/settings/notifications",
            get(get_notifications_settings).put(put_notifications_settings),
        )
        .route(
            "/v1/settings/web-analytics",
            get(get_web_analytics_settings).put(put_web_analytics_settings),
        )
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csv_cell_quotes_and_escapes_formula_prefix() {
        assert_eq!(csv_cell("plain"), "\"plain\"");
        assert_eq!(csv_cell("a\"b"), "\"a\"\"b\"");
        assert_eq!(csv_cell("=1+1"), "\"'=1+1\"");
        assert_eq!(csv_cell("+SUM(A1)"), "\"'+SUM(A1)\"");
        assert_eq!(csv_cell("@cmd"), "\"'@cmd\"");
        assert_eq!(csv_cell("-5"), "\"'-5\"");
        assert_eq!(csv_cell("  =x"), "\"'  =x\"");
        assert_eq!(csv_cell("\ttab"), "\"'\ttab\"");
        assert_eq!(csv_cell("with,comma"), "\"with,comma\"");
        assert_eq!(csv_cell("safe-1"), "\"safe-1\"");
    }

    #[test]
    fn csv_value_handles_null_and_numbers() {
        assert_eq!(csv_value(None), "\"\"");
        assert_eq!(csv_value(Some(&Value::Null)), "\"\"");
        assert_eq!(csv_value(Some(&json!(42))), "\"42\"");
        assert_eq!(csv_value(Some(&json!("x"))), "\"x\"");
        assert_eq!(csv_value(Some(&json!(true))), "\"true\"");
    }
}
