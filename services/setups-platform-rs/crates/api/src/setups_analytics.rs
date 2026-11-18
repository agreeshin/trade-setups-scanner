use crate::analytics::AnalyticsState;
use crate::replay::guard_auth;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use setups_analytics::{
    AnalyticsSetup, EvaluateLimitOptions, LimitStatus, TradeDirection, evaluate_limit, risk_return,
};
use sqlx::Row;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

const MAX_ANALYTICS_SETUPS: i64 = 5000;
const ANALYTICS_HORIZON_MINUTES: i64 = 10_080;
const MAX_ANALYTICS_SLOTS: usize = 64;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SnapshotDto {
    direction: String,
    tp_pct: f64,
    sl_pct: f64,
    mean_r: f64,
    time_shift_minutes: i64,
    #[serde(default)]
    price_change_pct: Option<f64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct EnvelopeDto {
    snapshot: SnapshotDto,
    #[serde(default)]
    study_id: Option<Value>,
    #[serde(default)]
    external_name: Option<String>,
}

fn id_of(value: &Option<Value>) -> Option<i64> {
    match value {
        Some(Value::Number(number)) => number.as_i64(),
        Some(Value::String(text)) => text.parse().ok(),
        _ => None,
    }
}

fn direction_of(raw: &str) -> Option<TradeDirection> {
    match raw {
        "short" => Some(TradeDirection::Short),
        "long" => Some(TradeDirection::Long),
        _ => None,
    }
}

#[derive(Clone)]
struct ParsedRecommendation {
    direction: TradeDirection,
    direction_raw: &'static str,
    tp_pct: f64,
    sl_pct: f64,
    mean_r: f64,
    time_shift_minutes: i64,
    price_change_pct: Option<f64>,
    envelope_study_id: Option<i64>,
    external_name: Option<String>,
}

fn parse_recommendation(value: &Value) -> Result<ParsedRecommendation, String> {
    let envelope: EnvelopeDto = serde_json::from_value(value.clone())
        .map_err(|_| "setups_analytics_invalid_recommendation".to_string())?;
    let snapshot = envelope.snapshot;
    let direction = direction_of(&snapshot.direction)
        .ok_or_else(|| "setups_analytics_invalid_recommendation".to_string())?;
    if !(0.0 < snapshot.tp_pct && snapshot.tp_pct <= 100.0)
        || !(0.0 < snapshot.sl_pct && snapshot.sl_pct <= 100.0)
        || !snapshot.mean_r.is_finite()
        || !(0..=43200).contains(&snapshot.time_shift_minutes)
        || snapshot
            .price_change_pct
            .is_some_and(|pct| !pct.is_finite() || !(-100.0..=1000.0).contains(&pct))
    {
        return Err("setups_analytics_invalid_recommendation".to_string());
    }
    Ok(ParsedRecommendation {
        direction_raw: if matches!(direction, TradeDirection::Short) {
            "short"
        } else {
            "long"
        },
        direction,
        tp_pct: snapshot.tp_pct,
        sl_pct: snapshot.sl_pct,
        mean_r: snapshot.mean_r,
        time_shift_minutes: snapshot.time_shift_minutes,
        price_change_pct: snapshot.price_change_pct,
        envelope_study_id: id_of(&envelope.study_id),
        external_name: envelope.external_name,
    })
}

fn gate_threshold(close_price: f64, pct: f64, direction: TradeDirection) -> f64 {
    let delta = pct.abs() / 100.0;
    match direction {
        TradeDirection::Short => close_price * (1.0 - delta),
        TradeDirection::Long => close_price * (1.0 + delta),
    }
}

fn gate_pass(entry_price: f64, close_price: f64, pct: f64, direction: TradeDirection) -> bool {
    let threshold = gate_threshold(close_price, pct, direction);
    match direction {
        TradeDirection::Short => entry_price >= threshold,
        TradeDirection::Long => entry_price <= threshold,
    }
}

fn display_levels(
    entry_price: f64,
    tp_pct: f64,
    sl_pct: f64,
    direction: TradeDirection,
) -> (f64, f64) {
    match direction {
        TradeDirection::Short => (
            entry_price * (1.0 - tp_pct / 100.0),
            entry_price * (1.0 + sl_pct / 100.0),
        ),
        TradeDirection::Long => (
            entry_price * (1.0 + tp_pct / 100.0),
            entry_price * (1.0 - sl_pct / 100.0),
        ),
    }
}

#[derive(Clone)]
struct SetupOutcome {
    status: &'static str,
    entry_time_unix: Option<i64>,
    entry_price: Option<f64>,
    gate_threshold: Option<f64>,
    tp_price: Option<f64>,
    sl_price: Option<f64>,
    close_time_unix: Option<i64>,
    risk_return: Option<f64>,
}

fn status_str(status: LimitStatus) -> &'static str {
    match status {
        LimitStatus::Tp => "tp",
        LimitStatus::Sl => "sl",
        LimitStatus::Unclosed => "unclosed",
        LimitStatus::NoData => "no_data",
        LimitStatus::PriceChangeFiltered => "gate_failed",
    }
}

fn classify(
    recommendation: &ParsedRecommendation,
    evaluation: setups_analytics::LimitEvaluation,
    close_price: f64,
) -> SetupOutcome {
    let threshold = recommendation
        .price_change_pct
        .map(|pct| gate_threshold(close_price, pct, recommendation.direction));
    let Some(entry_price) = evaluation.entry_price else {
        return SetupOutcome {
            status: "no_data",
            entry_time_unix: None,
            entry_price: None,
            gate_threshold: threshold,
            tp_price: None,
            sl_price: None,
            close_time_unix: None,
            risk_return: None,
        };
    };
    let base = SetupOutcome {
        status: status_str(evaluation.status),
        entry_time_unix: evaluation.entry_time_unix,
        entry_price: Some(entry_price),
        gate_threshold: threshold,
        tp_price: None,
        sl_price: None,
        close_time_unix: evaluation.close_time_unix,
        risk_return: None,
    };
    if let Some(pct) = recommendation.price_change_pct
        && !gate_pass(entry_price, close_price, pct, recommendation.direction)
    {
        return SetupOutcome {
            status: "gate_failed",
            close_time_unix: None,
            ..base
        };
    }
    match evaluation.status {
        LimitStatus::Tp | LimitStatus::Sl | LimitStatus::Unclosed => {
            let (tp_price, sl_price) = display_levels(
                entry_price,
                recommendation.tp_pct,
                recommendation.sl_pct,
                recommendation.direction,
            );
            SetupOutcome {
                tp_price: Some(tp_price),
                sl_price: Some(sl_price),
                risk_return: risk_return(
                    evaluation.status,
                    recommendation.tp_pct,
                    recommendation.sl_pct,
                ),
                ..base
            }
        }
        _ => base,
    }
}

#[derive(Default, Clone, Copy)]
struct Agg {
    selected: i64,
    gate_passed: i64,
    closed: i64,
    risk_sum: f64,
}

impl Agg {
    fn observe(&mut self, status: &str, risk_return: Option<f64>) {
        self.selected += 1;
        match status {
            "tp" | "sl" => {
                self.gate_passed += 1;
                self.closed += 1;
                if let Some(risk) = risk_return {
                    self.risk_sum += risk;
                }
            }
            "unclosed" => self.gate_passed += 1,
            _ => {}
        }
    }

    fn mean_r(&self) -> Option<f64> {
        if self.closed == 0 {
            return None;
        }
        Some(self.risk_sum / self.closed as f64)
    }
}

fn agg_json(agg: &Agg) -> Value {
    json!({
        "selected": agg.selected,
        "gatePassed": agg.gate_passed,
        "closed": agg.closed,
        "meanR": agg.mean_r(),
    })
}

struct AnalyticsSetupRow {
    id: i64,
    exchange_code: String,
    category: String,
    symbol: String,
    algorithm_id: String,
    close_unix: i64,
    close_price: String,
    slot_id: i64,
    generation_id: Option<i64>,
    recommendation: Value,
}

fn error_response(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}

fn query_i64(params: &HashMap<String, String>, key: &str) -> Result<Option<i64>, Response> {
    match params.get(key) {
        None => Ok(None),
        Some(raw) => raw
            .parse::<i64>()
            .map(Some)
            .map_err(|_| error_response(StatusCode::BAD_REQUEST, "invalid_query")),
    }
}

async fn load_setup_rows(
    pool: &sqlx::PgPool,
    algorithm_id: &str,
    from_unix: Option<i64>,
    to_unix: Option<i64>,
    slots: Option<&[i64]>,
    with_bounds: bool,
) -> Result<Vec<AnalyticsSetupRow>, Response> {
    let mut filter = String::from(
        "algorithm_id = $1 and adaptive_slot_id is not null and recommendation is not null",
    );
    if with_bounds {
        filter.push_str(" and close_time >= to_timestamp($2) and close_time < to_timestamp($3)");
    } else if from_unix.is_some() {
        filter.push_str(" and close_time >= to_timestamp($2) and close_time < to_timestamp($3)");
    }
    let slots_active = slots.is_some();
    if slots_active {
        let index = if filter.contains("$3") { 4 } else { 2 };
        filter.push_str(&format!(" and adaptive_slot_id::bigint = any(${index})"));
    }
    let sql = format!(
        "select id, exchange_code, category, symbol, algorithm_id, (extract(epoch from close_time))::bigint as close_unix, \
         close_price, adaptive_slot_id::bigint as slot_id, \
         coalesce(adaptive_generation_id, (adaptive_generation_archive->>'generationId')::bigint) as generation_id, \
         recommendation from live_setups where {filter} order by close_time desc, id desc limit {}",
        MAX_ANALYTICS_SETUPS + 1
    );
    let mut query = sqlx::query(&sql).bind(algorithm_id);
    if filter.contains("$2") {
        query = query
            .bind(from_unix.unwrap_or(i64::MIN))
            .bind(to_unix.unwrap_or(i64::MAX));
    }
    if slots_active {
        query = query.bind(slots.expect("checked"));
    }
    let rows = query.fetch_all(pool).await.map_err(|_| {
        error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "setups_analytics_read_failed",
        )
    })?;
    if rows.len() as i64 > MAX_ANALYTICS_SETUPS {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "setups_analytics_limit_exceeded",
        ));
    }
    rows.iter()
        .map(|row| {
            Ok(AnalyticsSetupRow {
                id: i64::from(row.try_get::<i32, _>("id").map_err(|_| {
                    error_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "setups_analytics_read_failed",
                    )
                })?),
                exchange_code: row.try_get("exchange_code").map_err(|_| {
                    error_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "setups_analytics_read_failed",
                    )
                })?,
                category: row.try_get("category").map_err(|_| {
                    error_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "setups_analytics_read_failed",
                    )
                })?,
                symbol: row.try_get("symbol").map_err(|_| {
                    error_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "setups_analytics_read_failed",
                    )
                })?,
                algorithm_id: row.try_get("algorithm_id").map_err(|_| {
                    error_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "setups_analytics_read_failed",
                    )
                })?,
                close_unix: row.try_get("close_unix").map_err(|_| {
                    error_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "setups_analytics_read_failed",
                    )
                })?,
                close_price: row.try_get("close_price").map_err(|_| {
                    error_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "setups_analytics_read_failed",
                    )
                })?,
                slot_id: row.try_get("slot_id").map_err(|_| {
                    error_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "setups_analytics_read_failed",
                    )
                })?,
                generation_id: row.try_get("generation_id").map_err(|_| {
                    error_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "setups_analytics_read_failed",
                    )
                })?,
                recommendation: row.try_get("recommendation").map_err(|_| {
                    error_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "setups_analytics_read_failed",
                    )
                })?,
            })
        })
        .collect()
}

async fn study_map(
    pool: &sqlx::PgPool,
    generation_ids: &[i64],
) -> Result<HashMap<i64, i64>, Response> {
    if generation_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = sqlx::query("select id, study_id from continuous_generations where id = any($1)")
        .bind(generation_ids)
        .fetch_all(pool)
        .await
        .map_err(|_| {
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "setups_analytics_read_failed",
            )
        })?;
    Ok(rows
        .iter()
        .filter_map(|row| {
            let id: i64 = row.try_get("id").ok()?;
            let study: i64 = row.try_get("study_id").ok()?;
            Some((id, study))
        })
        .collect())
}

fn parse_slots(raw: Option<&str>) -> Result<Option<Vec<i64>>, Response> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let mut slots = Vec::new();
    for part in raw.split(',') {
        let value = part
            .trim()
            .parse::<i64>()
            .map_err(|_| error_response(StatusCode::BAD_REQUEST, "invalid_query"))?;
        if value <= 0 {
            return Err(error_response(StatusCode::BAD_REQUEST, "invalid_query"));
        }
        slots.push(value);
    }
    if slots.is_empty() || slots.len() > MAX_ANALYTICS_SLOTS {
        return Err(error_response(StatusCode::BAD_REQUEST, "invalid_query"));
    }
    Ok(Some(slots))
}

async fn setups_analytics(
    State(state): State<Arc<AnalyticsState>>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    if let Err(response) = guard_auth(&state.auth, &headers, false) {
        return response;
    }
    let algorithm_id = match params.get("algorithmId") {
        Some(value) if !value.trim().is_empty() && value.len() <= 200 => value.clone(),
        _ => return error_response(StatusCode::BAD_REQUEST, "invalid_query"),
    };
    let from_unix = match query_i64(&params, "fromUnix") {
        Ok(value) => value,
        Err(response) => return response,
    };
    let to_unix = match query_i64(&params, "toUnix") {
        Ok(value) => value,
        Err(response) => return response,
    };
    let (Some(from_unix), Some(to_unix)) = (from_unix, to_unix) else {
        return error_response(StatusCode::BAD_REQUEST, "invalid_query");
    };
    if from_unix >= to_unix {
        return error_response(StatusCode::BAD_REQUEST, "invalid_query");
    }
    let slots = match parse_slots(params.get("slots").map(String::as_str)) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let rows = match load_setup_rows(
        &state.pool,
        &algorithm_id,
        Some(from_unix),
        Some(to_unix),
        slots.as_deref(),
        true,
    )
    .await
    {
        Ok(value) => value,
        Err(response) => return response,
    };
    let mut parsed: Vec<(AnalyticsSetupRow, ParsedRecommendation)> = Vec::with_capacity(rows.len());
    for row in rows {
        let recommendation = match parse_recommendation(&row.recommendation) {
            Ok(value) => value,
            Err(message) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, &message),
        };
        parsed.push((row, recommendation));
    }
    let studies = match study_map(
        &state.pool,
        &parsed
            .iter()
            .filter_map(|(row, _)| row.generation_id)
            .collect::<Vec<_>>(),
    )
    .await
    {
        Ok(value) => value,
        Err(response) => return response,
    };
    let mut indexes: HashMap<(String, String, String), Vec<usize>> = HashMap::new();
    for (index, (row, _)) in parsed.iter().enumerate() {
        indexes
            .entry((
                row.exchange_code.clone(),
                row.category.clone(),
                row.symbol.clone(),
            ))
            .or_default()
            .push(index);
    }
    let mut outcomes: Vec<Option<SetupOutcome>> = vec![None; parsed.len()];
    for ((exchange, category, symbol), group) in indexes {
        let entry_times: Vec<i64> = group
            .iter()
            .map(|index| {
                let (row, recommendation) = &parsed[*index];
                row.close_unix + recommendation.time_shift_minutes * 60
            })
            .collect();
        let window_from = entry_times.iter().min().copied().unwrap_or(0) - 60;
        let window_to =
            entry_times.iter().max().copied().unwrap_or(0) + ANALYTICS_HORIZON_MINUTES * 60 + 60;
        let instrument = state
            .reader
            .instrument_id(&exchange, &category, &symbol)
            .await;
        let candles = match instrument {
            Some(id) => state
                .reader
                .read_window(id, window_from, window_to)
                .await
                .ok(),
            None => None,
        };
        let Some(candles) = candles else {
            for index in group {
                let (row, recommendation) = &parsed[index];
                let close_price: f64 = if let Ok(value) = row.close_price.parse() {
                    value
                } else {
                    f64::NAN
                };
                outcomes[index] = Some(classify(
                    recommendation,
                    setups_analytics::LimitEvaluation {
                        status: LimitStatus::NoData,
                        entry_price: None,
                        entry_time_unix: None,
                        tp_price: None,
                        sl_price: None,
                        close_time_unix: None,
                        move_pct: None,
                        close_minutes: None,
                    },
                    close_price,
                ));
            }
            continue;
        };
        for index in group {
            let (row, recommendation) = &parsed[index];
            let fabric = AnalyticsSetup {
                exchange_code: row.exchange_code.clone(),
                category: row.category.clone(),
                symbol: row.symbol.clone(),
                open_time_unix: row.close_unix,
                close_time_unix: row.close_unix,
                close_price: row.close_price.clone(),
            };
            let evaluation = evaluate_limit(EvaluateLimitOptions {
                candles: &candles,
                setup: &fabric,
                direction: recommendation.direction,
                tp_pct: recommendation.tp_pct,
                sl_pct: recommendation.sl_pct,
                time_shift_minutes: recommendation.time_shift_minutes,
                horizon_minutes: ANALYTICS_HORIZON_MINUTES,
            });
            let close_price: f64 = if let Ok(value) = row.close_price.parse() {
                value
            } else {
                f64::NAN
            };
            outcomes[index] = Some(classify(recommendation, evaluation, close_price));
        }
    }
    let mut order: Vec<usize> = (0..parsed.len()).collect();
    order.sort_by(|left, right| {
        parsed[*right]
            .0
            .close_unix
            .cmp(&parsed[*left].0.close_unix)
            .then(parsed[*right].0.id.cmp(&parsed[*left].0.id))
    });
    let study_ids: Vec<Option<i64>> = parsed
        .iter()
        .map(|(row, rec)| {
            row.generation_id
                .and_then(|generation| studies.get(&generation).copied())
                .or(rec.envelope_study_id)
        })
        .collect();
    let mut totals = Agg::default();
    let mut per_study: BTreeMap<i64, Agg> = BTreeMap::new();
    for index in 0..parsed.len() {
        let outcome = outcomes[index].clone().expect("outcome computed");
        totals.observe(outcome.status, outcome.risk_return);
        if let Some(study) = study_ids[index] {
            per_study
                .entry(study)
                .or_default()
                .observe(outcome.status, outcome.risk_return);
        }
    }
    let setup_json: Vec<Value> = order
        .into_iter()
        .map(|index| {
            let (row, recommendation) = &parsed[index];
            let outcome = outcomes[index].clone().expect("outcome computed");
            let study_id = study_ids[index];
            json!({
                "id": row.id,
                "symbol": row.symbol,
                "exchange": row.exchange_code,
                "slotId": row.slot_id,
                "studyId": study_id,
                "direction": recommendation.direction_raw,
                "setupTimeUnix": row.close_unix,
                "entryTimeUnix": outcome.entry_time_unix,
                "entryPrice": outcome.entry_price,
                "gateThreshold": outcome.gate_threshold,
                "tpPrice": outcome.tp_price,
                "slPrice": outcome.sl_price,
                "closeTimeUnix": outcome.close_time_unix,
                "riskReturn": outcome.risk_return,
                "status": outcome.status,
            })
        })
        .collect();
    Json(json!({
        "totals": agg_json(&totals),
        "studies": per_study
            .iter()
            .map(|(study, agg)| {
                let Value::Object(mut map) = agg_json(agg) else {
                    unreachable!("aggregate payload is an object");
                };
                map.insert("studyId".to_string(), json!(study));
                Value::Object(map)
            })
            .collect::<Vec<_>>(),
        "setups": setup_json,
    }))
    .into_response()
}

async fn setups_analytics_profiles(
    State(state): State<Arc<AnalyticsState>>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    if let Err(response) = guard_auth(&state.auth, &headers, false) {
        return response;
    }
    let algorithm_id = match params.get("algorithmId") {
        Some(value) if !value.trim().is_empty() && value.len() <= 200 => value.clone(),
        _ => return error_response(StatusCode::BAD_REQUEST, "invalid_query"),
    };
    let from_unix = match query_i64(&params, "fromUnix") {
        Ok(value) => value,
        Err(response) => return response,
    };
    let to_unix = match query_i64(&params, "toUnix") {
        Ok(value) => value,
        Err(response) => return response,
    };
    match (from_unix, to_unix) {
        (Some(from), Some(to)) if from >= to => {
            return error_response(StatusCode::BAD_REQUEST, "invalid_query");
        }
        (Some(_), None) | (None, Some(_)) => {
            return error_response(StatusCode::BAD_REQUEST, "invalid_query");
        }
        _ => {}
    }
    let rows =
        match load_setup_rows(&state.pool, &algorithm_id, from_unix, to_unix, None, false).await {
            Ok(value) => value,
            Err(response) => return response,
        };
    let studies = match study_map(
        &state.pool,
        &rows
            .iter()
            .filter_map(|row| row.generation_id)
            .collect::<Vec<_>>(),
    )
    .await
    {
        Ok(value) => value,
        Err(response) => return response,
    };
    let mut counts: BTreeMap<i64, i64> = BTreeMap::new();
    let mut latest: BTreeMap<i64, (&AnalyticsSetupRow, ParsedRecommendation)> = BTreeMap::new();
    for row in &rows {
        *counts.entry(row.slot_id).or_default() += 1;
        let recommendation = match parse_recommendation(&row.recommendation) {
            Ok(value) => value,
            Err(message) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, &message),
        };
        let replace = match latest.get(&row.slot_id) {
            Some((existing, _)) => {
                row.close_unix > existing.close_unix
                    || (row.close_unix == existing.close_unix && row.id > existing.id)
            }
            None => true,
        };
        if replace {
            latest.insert(row.slot_id, (row, recommendation));
        }
    }
    let profiles: Vec<Value> = latest
        .into_iter()
        .map(|(slot_id, (row, recommendation))| {
            let study_id = row
                .generation_id
                .and_then(|generation| studies.get(&generation).copied())
                .or(recommendation.envelope_study_id);
            let external_name = recommendation
                .external_name
                .clone()
                .filter(|name| !name.trim().is_empty())
                .unwrap_or_else(|| setups_db::algorithm_name(&row.algorithm_id));
            json!({
                "slotId": slot_id,
                "studyId": study_id,
                "generationId": row.generation_id,
                "setupCount": counts.get(&slot_id).copied().unwrap_or(0),
                "direction": recommendation.direction_raw,
                "meanR": recommendation.mean_r,
                "tpPct": recommendation.tp_pct,
                "slPct": recommendation.sl_pct,
                "timeShiftMinutes": recommendation.time_shift_minutes,
                "priceChangePct": recommendation.price_change_pct,
                "externalName": external_name,
            })
        })
        .collect();
    Json(json!({ "profiles": profiles })).into_response()
}

pub fn setups_analytics_router(state: Arc<AnalyticsState>) -> Router {
    Router::new()
        .route("/v1/setups/analytics", get(setups_analytics))
        .route(
            "/v1/setups/analytics/profiles",
            get(setups_analytics_profiles),
        )
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use setups_analytics::OhlcCandle;

    fn envelope_json() -> Value {
        json!({
            "snapshot": {
                "direction": "short",
                "tpPct": 20.0,
                "slPct": 6.0,
                "rawR": 0.1,
                "meanR": 0.32,
                "robustScore": 0.5,
                "timeShiftMinutes": 5,
                "priceChangePct": 2.0,
                "clusterEnabled": true,
                "clusterGapMinutes": 30,
                "provenance": { "algorithmId": "price_volume_spike" }
            },
            "studyId": 40,
            "generationId": 36,
            "slotId": 1,
            "externalName": "Всплеск цены и оборота",
            "rawSignalAt": 1794000000,
            "expectancy": { "meanR": 0.32 }
        })
    }

    #[test]
    fn parses_envelope_with_extra_keys_and_ids() {
        let parsed = parse_recommendation(&envelope_json()).unwrap();
        assert!(matches!(parsed.direction, TradeDirection::Short));
        assert_eq!(parsed.envelope_study_id, Some(40));
        assert_eq!(parsed.price_change_pct, Some(2.0));
        assert_eq!(parsed.tp_pct, 20.0);
        assert_eq!(parsed.time_shift_minutes, 5);
    }

    #[test]
    fn parses_string_ids_and_missing_price_change() {
        let mut value = envelope_json();
        value["studyId"] = json!("41");
        value["generationId"] = json!("37");
        value["snapshot"]["priceChangePct"] = Value::Null;
        let parsed = parse_recommendation(&value).unwrap();
        assert_eq!(parsed.envelope_study_id, Some(41));
        assert_eq!(parsed.price_change_pct, None);
    }

    #[test]
    fn rejects_invalid_direction_and_ranges() {
        let mut value = envelope_json();
        value["snapshot"]["direction"] = json!("side");
        assert!(parse_recommendation(&value).is_err());
        let mut value = envelope_json();
        value["snapshot"]["tpPct"] = json!(0.0);
        assert!(parse_recommendation(&value).is_err());
        let mut value = envelope_json();
        value["snapshot"]["timeShiftMinutes"] = json!(43201);
        assert!(parse_recommendation(&value).is_err());
        assert!(parse_recommendation(&json!({ "snapshot": {} })).is_err());
    }

    #[test]
    fn caption_gate_matches_direction_semantics() {
        assert_eq!(gate_threshold(100.0, 2.0, TradeDirection::Short), 98.0);
        assert!(!gate_pass(97.9, 100.0, 2.0, TradeDirection::Short));
        assert!(gate_pass(98.0, 100.0, 2.0, TradeDirection::Short));
        assert!(gate_pass(120.0, 100.0, 2.0, TradeDirection::Short));
        assert_eq!(gate_threshold(100.0, 2.0, TradeDirection::Long), 102.0);
        assert!(gate_pass(102.0, 100.0, 2.0, TradeDirection::Long));
        assert!(!gate_pass(102.1, 100.0, 2.0, TradeDirection::Long));
        assert_eq!(gate_threshold(100.0, -2.0, TradeDirection::Short), 98.0);
    }

    fn candle(open_time_unix: i64, high: f64, low: f64, close: f64) -> OhlcCandle {
        OhlcCandle {
            open_time_unix,
            open: close,
            high,
            low,
            close,
        }
    }

    fn fabric() -> AnalyticsSetup {
        AnalyticsSetup {
            exchange_code: "bybit".into(),
            category: "linear".into(),
            symbol: "TESTUSDT".into(),
            open_time_unix: 1_794_000_000,
            close_time_unix: 1_794_000_000,
            close_price: "100".into(),
        }
    }

    fn recommendation(price_change_pct: Option<f64>) -> ParsedRecommendation {
        ParsedRecommendation {
            direction: TradeDirection::Short,
            direction_raw: "short",
            tp_pct: 20.0,
            sl_pct: 6.0,
            mean_r: 0.0,
            time_shift_minutes: 5,
            price_change_pct,
            envelope_study_id: Some(40),
            external_name: None,
        }
    }

    #[test]
    fn classify_reports_no_data_without_entry_candle() {
        let outcome = classify(
            &recommendation(Some(2.0)),
            setups_analytics::LimitEvaluation {
                status: LimitStatus::NoData,
                entry_price: None,
                entry_time_unix: None,
                tp_price: None,
                sl_price: None,
                close_time_unix: None,
                move_pct: None,
                close_minutes: None,
            },
            100.0,
        );
        assert_eq!(outcome.status, "no_data");
        assert_eq!(outcome.entry_price, None);
        assert_eq!(outcome.gate_threshold, Some(98.0));
    }

    #[test]
    fn classify_fails_gate_and_skips_levels() {
        let entry = 1_794_000_300;
        let outcome = classify(
            &recommendation(Some(2.0)),
            setups_analytics::LimitEvaluation {
                status: LimitStatus::Unclosed,
                entry_price: Some(97.0),
                entry_time_unix: Some(entry),
                tp_price: None,
                sl_price: None,
                close_time_unix: None,
                move_pct: None,
                close_minutes: None,
            },
            100.0,
        );
        assert_eq!(outcome.status, "gate_failed");
        assert_eq!(outcome.entry_price, Some(97.0));
        assert_eq!(outcome.tp_price, None);
        assert_eq!(outcome.close_time_unix, None);
    }

    #[test]
    fn evaluate_fixture_produces_sl_and_display_levels_match() {
        let candles = vec![
            candle(1_794_000_300, 99.0, 98.0, 98.5),
            candle(1_794_000_360, 107.0, 100.0, 106.0),
        ];
        let setup = fabric();
        let evaluation = evaluate_limit(EvaluateLimitOptions {
            candles: &candles,
            setup: &setup,
            direction: TradeDirection::Short,
            tp_pct: 20.0,
            sl_pct: 6.0,
            time_shift_minutes: 5,
            horizon_minutes: ANALYTICS_HORIZON_MINUTES,
        });
        assert_eq!(evaluation.status, LimitStatus::Sl);
        assert_eq!(evaluation.entry_price, Some(98.5));
        let outcome = classify(&recommendation(None), evaluation, 100.0);
        assert_eq!(outcome.status, "sl");
        assert_eq!(outcome.risk_return, Some(-1.0));
        let (tp_price, sl_price) = display_levels(98.5, 20.0, 6.0, TradeDirection::Short);
        assert!((tp_price - 98.5 * 0.8).abs() < 1e-12);
        assert!((sl_price - evaluation.sl_price.expect("sl hit")).abs() < 1e-12);
        assert_eq!(outcome.tp_price, Some(tp_price));
        assert_eq!(outcome.sl_price, Some(sl_price));
        assert_eq!(outcome.gate_threshold, None);
    }

    #[test]
    fn evaluate_fixture_produces_tp_risk_ratio() {
        let candles = vec![
            candle(1_794_000_300, 99.0, 98.0, 98.5),
            candle(1_794_000_360, 99.0, 78.0, 79.0),
        ];
        let evaluation = evaluate_limit(EvaluateLimitOptions {
            candles: &candles,
            setup: &fabric(),
            direction: TradeDirection::Short,
            tp_pct: 20.0,
            sl_pct: 6.0,
            time_shift_minutes: 5,
            horizon_minutes: ANALYTICS_HORIZON_MINUTES,
        });
        assert_eq!(evaluation.status, LimitStatus::Tp);
        let outcome = classify(&recommendation(None), evaluation, 100.0);
        assert_eq!(outcome.status, "tp");
        let risk = outcome.risk_return.expect("closed");
        assert!((risk - 20.0 / 6.0).abs() < 1e-12);
    }

    #[test]
    fn aggregate_counts_statuses_and_mean() {
        let mut totals = Agg::default();
        totals.observe("tp", Some(20.0 / 6.0));
        totals.observe("sl", Some(-1.0));
        totals.observe("unclosed", None);
        totals.observe("gate_failed", None);
        totals.observe("no_data", None);
        assert_eq!(totals.selected, 5);
        assert_eq!(totals.gate_passed, 3);
        assert_eq!(totals.closed, 2);
        let mean = totals.mean_r().unwrap();
        assert!((mean - ((20.0 / 6.0) - 1.0) / 2.0).abs() < 1e-12);
        assert_eq!(Agg::default().mean_r(), None);
    }

    #[test]
    fn slots_parser_validates_input() {
        assert!(parse_slots(None).unwrap().is_none());
        let slots = parse_slots(Some("1, 3")).unwrap().unwrap();
        assert_eq!(slots, vec![1, 3]);
        assert!(parse_slots(Some("")).is_err());
        assert!(parse_slots(Some("0")).is_err());
        assert!(parse_slots(Some("a")).is_err());
        assert!(parse_slots(Some("1,,2")).is_err());
    }
}
