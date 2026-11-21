use crate::analytics::AnalyticsState;
use crate::replay::guard_auth;
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use setups_analytics::live_outcomes::{self, TradeOutcome as SetupOutcome, TradeTerms};
#[cfg(test)]
use setups_analytics::live_outcomes::{display_levels, gate_threshold};
use setups_analytics::{AnalyticsSetup, TradeDirection};
#[cfg(test)]
use setups_analytics::{
    EvaluateLimitOptions, LimitStatus, evaluate_limit_at_open, passes_price_change,
};
use sqlx::Row;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

const MAX_ANALYTICS_SETUPS: i64 = 5000;
const ANALYTICS_HORIZON_MINUTES: i64 = 10_080;
const MAX_ANALYTICS_SLOTS: usize = 64;
// Bounded metadata, not a cap on accessible setup history. The prior endpoint
// could expose up to 5000 distinct studies; do not reduce that to the subset-ID cap.
const MAX_STUDY_OPTIONS: usize = 5000;
const MAX_WINDOW_SECONDS: i64 = 2 * ANALYTICS_HORIZON_MINUTES * 60;
const MAX_OUTCOME_CANDLES: usize = 1_100_000;
const MAX_OUTCOME_PAGES: usize = 512;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RecommendationDto {
    direction: String,
    tp_pct: f64,
    sl_pct: f64,
    mean_r: f64,
    time_shift_minutes: i64,
    #[serde(default)]
    price_change_pct: Option<f64>,
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
    time_shift_minutes: i64,
    price_change_pct: Option<f64>,
    envelope_study_id: Option<i64>,
    external_name: Option<String>,
}

fn parse_recommendation(value: &Value) -> Result<ParsedRecommendation, String> {
    let item: RecommendationDto = serde_json::from_value(value.clone())
        .map_err(|_| "setups_analytics_invalid_recommendation".to_string())?;
    let direction = direction_of(&item.direction)
        .ok_or_else(|| "setups_analytics_invalid_recommendation".to_string())?;
    if !(0.0 < item.tp_pct && item.tp_pct <= 100.0)
        || !(0.0 < item.sl_pct && item.sl_pct <= 100.0)
        || !item.mean_r.is_finite()
        || !(0..=43200).contains(&item.time_shift_minutes)
        || item
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
        tp_pct: item.tp_pct,
        sl_pct: item.sl_pct,
        time_shift_minutes: item.time_shift_minutes,
        price_change_pct: item.price_change_pct,
        envelope_study_id: id_of(&item.study_id),
        external_name: item.external_name,
    })
}

impl ParsedRecommendation {
    fn terms(&self) -> TradeTerms {
        TradeTerms {
            direction: self.direction,
            tp_pct: self.tp_pct,
            sl_pct: self.sl_pct,
            time_shift_minutes: self.time_shift_minutes,
            price_change_pct: self.price_change_pct,
        }
    }
}

#[cfg(test)]
fn classify(
    recommendation: &ParsedRecommendation,
    evaluation: setups_analytics::LimitEvaluation,
    close_price: f64,
) -> SetupOutcome {
    live_outcomes::classify(recommendation.terms(), evaluation, close_price)
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

#[derive(Clone)]
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
    artifact_id: Option<i64>,
    trade_outcome: Option<Value>,
}

fn error_response(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}

fn read_error() -> Response {
    error_response(
        StatusCode::INTERNAL_SERVER_ERROR,
        "setups_analytics_read_failed",
    )
}

fn outcome_read_limit() -> Response {
    error_response(StatusCode::BAD_REQUEST, "setups_outcomes_read_budget_exceeded")
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
    studies: Option<&[i64]>,
    with_bounds: bool,
    q: Option<&str>,
) -> Result<Vec<AnalyticsSetupRow>, Response> {
    let mut query = sqlx::QueryBuilder::new(
        "select id, exchange_code, category, symbol, algorithm_id, (extract(epoch from close_time))::bigint as close_unix, \
         close_price, adaptive_slot_id::bigint as slot_id, \
         coalesce(adaptive_generation_id, (adaptive_generation_archive->>'generationId')::bigint) as generation_id, \
         expectancy_artifact_id as artifact_id, recommendation, trade_outcome from live_setups where adaptive_slot_id is not null and recommendation is not null \
         and exists(select 1 from adaptive_profiles p where p.id=adaptive_slot_id and p.owner_id='admin')",
    );
    debug_assert!(!with_bounds || (from_unix.is_some() && to_unix.is_some()));
    setups_db::runs::push_live_setup_filters(&mut query, &setups_db::runs::LiveSetupFilter {
        algorithm_id: Some(algorithm_id.to_owned()), from_unix, to_unix,
        studies: studies.map(<[i64]>::to_vec), q: q.map(str::to_owned),
    });
    query.push(" order by close_time desc, id desc limit ").push_bind(MAX_ANALYTICS_SETUPS + 1);
    let rows = query.build().fetch_all(pool).await.map_err(|_| {
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
                artifact_id: row.try_get("artifact_id").map_err(|_| {
                    error_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "setups_analytics_read_failed",
                    )
                })?,
                trade_outcome: row.try_get("trade_outcome").map_err(|_| read_error())?,
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

pub(crate) fn parse_studies(raw: Option<&str>) -> Result<Option<Vec<i64>>, Response> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let mut studies = Vec::new();
    if raw.is_empty() { return Ok(Some(studies)); }
    for part in raw.split(',') {
        let value = part
            .trim()
            .parse::<i64>()
            .map_err(|_| error_response(StatusCode::BAD_REQUEST, "invalid_query"))?;
        if value <= 0 {
            return Err(error_response(StatusCode::BAD_REQUEST, "invalid_query"));
        }
        studies.push(value);
    }
    if studies.is_empty() || studies.len() > MAX_ANALYTICS_SLOTS {
        return Err(error_response(StatusCode::BAD_REQUEST, "invalid_query"));
    }
    Ok(Some(studies))
}

async fn setups_analytics(
    State(state): State<Arc<AnalyticsState>>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    analytics_request(state, headers, params, true).await
}

async fn saved_setups_analytics(
    State(state): State<Arc<AnalyticsState>>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    analytics_request(state, headers, params, false).await
}

async fn analytics_request(
    state: Arc<AnalyticsState>,
    headers: HeaderMap,
    params: HashMap<String, String>,
    fill_missing: bool,
) -> Response {
    if let Err(response) = guard_auth(&state.auth, &headers, fill_missing) {
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
    let filter = match crate::batches::parse_setup_filter(&params) {
        Ok(filter) => filter,
        Err(response) => return response,
    };
    let studies = match parse_studies(params.get("studies").map(String::as_str)) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let rows = match load_setup_rows(
        &state.pool,
        &algorithm_id,
        Some(from_unix),
        Some(to_unix),
        studies.as_deref(),
        true,
        filter.q.as_deref(),
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
    if !fill_missing && parsed.iter().any(|(row, _)| row.trade_outcome.is_none()) {
        return error_response(StatusCode::CONFLICT, "setups_analytics_outcomes_missing");
    }
    let cutoff = chrono::Utc::now().timestamp().div_euclid(60) * 60;
    let outcomes = match persisted_outcomes(&state, &parsed, false, cutoff).await {
        Ok(outcomes) => outcomes,
        Err(response) => return response,
    };
    analytics_result(&parsed, &studies, &outcomes)
}

fn market_read_error(symbol: &str, cause: &str) -> Response {
    let safe_symbol: String = symbol
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-'))
        .take(64)
        .collect();
    // Cause is an allowlisted class/status, never the upstream body, URL or credentials.
    eprintln!(
        "setups_analytics outcome=error code=market_data_read_failed symbol={safe_symbol} cause={cause}"
    );
    error_response(
        StatusCode::BAD_GATEWAY,
        "setups_analytics_market_data_unavailable",
    )
}

fn window_error_cause(error: &str) -> &'static str {
    if error.starts_with("market data api ") {
        "candles_http_error"
    } else if error.starts_with("market data decode error:") {
        "candles_decode_error"
    } else if error.starts_with("market data network error:") {
        "candles_network_error"
    } else {
        "candles_read_error"
    }
}

#[cfg(test)]
async fn calculate_outcomes(
    state: &AnalyticsState,
    parsed: &[(AnalyticsSetupRow, ParsedRecommendation)],
) -> Result<Vec<Option<SetupOutcome>>, Response> {
    calculate_at(
        state,
        parsed,
        chrono::Utc::now().timestamp().div_euclid(60) * 60,
    )
    .await
}

async fn calculate_at(
    state: &AnalyticsState,
    parsed: &[(AnalyticsSetupRow, ParsedRecommendation)],
    cutoff: i64,
) -> Result<Vec<Option<SetupOutcome>>, Response> {
    if parsed.is_empty() {
        return Ok(Vec::new());
    }
    // Fresh, operation-owned input only. Never reuse the ordinary analytics reader's stale tails.
    let market = setups_mdclient::MarketDataClient::new(&state.market_url, &state.market_key);
    let catalog = market.instruments().await.map_err(|error| {
        let cause = match error {
            setups_mdclient::MdClientError::Http { .. } => "catalog_http_error",
            setups_mdclient::MdClientError::Network(_) => "catalog_network_error",
            setups_mdclient::MdClientError::Decode(_) => "catalog_decode_error",
        };
        market_read_error("catalog", cause)
    })?;
    if catalog.len() > 10_000 {
        return Err(outcome_read_limit());
    }
    let instruments: HashMap<_, _> = catalog
        .into_iter()
        .map(|instrument| {
            (
                (instrument.exchange, instrument.category, instrument.symbol),
                instrument.id,
            )
        })
        .collect();
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
    let mut budget = ReadBudget {
        candles: 0,
        pages: 0,
        deadline: Instant::now() + Duration::from_secs(60),
    };
    for ((exchange, category, symbol), group) in indexes {
        let instrument = instruments
            .get(&(exchange, category, symbol.clone()))
            .copied();
        for window in outcome_windows(parsed, &group, cutoff)? {
            let candles = match instrument {
                Some(id) if window.from < window.to => {
                    read_outcome_window(&market, id, window.from, window.to, &symbol, &mut budget)
                        .await?
                }
                _ => Vec::new(),
            };
            for index in window.indexes {
                let (row, recommendation) = &parsed[index];
                let fabric = AnalyticsSetup {
                    exchange_code: row.exchange_code.clone(),
                    category: row.category.clone(),
                    symbol: row.symbol.clone(),
                    open_time_unix: row.close_unix,
                    close_time_unix: row.close_unix,
                    close_price: row.close_price.clone(),
                };
                let outcome = live_outcomes::evaluate(
                    &fabric,
                    recommendation.terms(),
                    &candles,
                    cutoff,
                    chrono::Utc::now().timestamp(),
                );
                if [
                    &outcome.entry_price,
                    &outcome.gate_threshold,
                    &outcome.tp_price,
                    &outcome.sl_price,
                    &outcome.close_price,
                    &outcome.risk_return,
                ]
                .iter()
                .any(|value| value.is_some_and(|n| !n.is_finite()))
                {
                    return Err(market_read_error(&symbol, "invalid_outcome_number"));
                }
                outcomes[index] = Some(outcome);
            }
        }
        tokio::task::yield_now().await;
    }
    Ok(outcomes)
}

struct OutcomeWindow {
    from: i64,
    to: i64,
    indexes: Vec<usize>,
}

fn outcome_windows(
    parsed: &[(AnalyticsSetupRow, ParsedRecommendation)],
    group: &[usize],
    cutoff: i64,
) -> Result<Vec<OutcomeWindow>, Response> {
    let mut entries = Vec::with_capacity(group.len());
    for &index in group {
        let (row, rec) = &parsed[index];
        let close = row
            .close_price
            .parse::<f64>()
            .ok()
            .filter(|n| n.is_finite() && *n > 0.0);
        let from = row
            .close_unix
            .checked_add(rec.time_shift_minutes * 60)
            .filter(|n| *n >= 0 && *n % 60 == 0)
            .ok_or_else(read_error)?;
        let to = from
            .checked_add(ANALYTICS_HORIZON_MINUTES * 60)
            .ok_or_else(read_error)?
            .min(cutoff);
        if close.is_none() {
            return Err(read_error());
        }
        entries.push((from, to.max(from), index));
    }
    entries.sort_unstable();
    let mut windows: Vec<OutcomeWindow> = Vec::new();
    for (from, to, index) in entries {
        if let Some(last) = windows.last_mut()
            && from <= last.to
            && to.max(last.to) - last.from <= MAX_WINDOW_SECONDS
        {
            last.to = last.to.max(to);
            last.indexes.push(index);
        } else {
            windows.push(OutcomeWindow {
                from,
                to,
                indexes: vec![index],
            });
        }
    }
    Ok(windows)
}

struct ReadBudget {
    candles: usize,
    pages: usize,
    deadline: Instant,
}

async fn read_outcome_window(
    market: &setups_mdclient::MarketDataClient,
    id: i64,
    from: i64,
    to: i64,
    symbol: &str,
    budget: &mut ReadBudget,
) -> Result<Vec<setups_analytics::OhlcCandle>, Response> {
    let mut candles = Vec::new();
    let mut after = None;
    loop {
        budget.pages += 1;
        if budget.pages > MAX_OUTCOME_PAGES || Instant::now() >= budget.deadline {
            return Err(outcome_read_limit());
        }
        let page = market
            .candles(
                &setups_mdclient::CandlesQuery {
                    instrument_id: id,
                    from_unix: from,
                    to_unix: to,
                    after_unix: after,
                    limit: Some(5000),
                },
                Duration::from_secs(10),
            )
            .await
            .map_err(|error| market_read_error(symbol, window_error_cause(&error.to_string())))?;
        if page.candles.len() > 5000 {
            return Err(market_read_error(symbol, "candles_page_size"));
        }
        let mut previous = after;
        for candle in &page.candles {
            let at = candle.open_time_unix;
            if at < from || at >= to || at % 60 != 0 || previous.is_some_and(|last| at <= last) {
                return Err(market_read_error(symbol, "candles_scope_or_order"));
            }
            let number = |raw: &str| -> Result<f64, Response> {
                raw.parse::<f64>()
                    .ok()
                    .filter(|n| raw.len() <= 128 && n.is_finite() && *n > 0.0)
                    .ok_or_else(|| market_read_error(symbol, "candles_invalid_number"))
            };
            candles.push(setups_analytics::OhlcCandle {
                open_time_unix: at,
                open: number(&candle.open)?,
                high: number(&candle.high)?,
                low: number(&candle.low)?,
                close: number(&candle.close)?,
            });
            previous = Some(at);
            budget.candles += 1;
            if budget.candles > MAX_OUTCOME_CANDLES || candles.len() > 25_000 {
                return Err(outcome_read_limit());
            }
        }
        match page.next_cursor_unix {
            None => break,
            Some(cursor)
                if previous == Some(cursor)
                    && after.is_none_or(|old| cursor > old)
                    && !page.candles.is_empty() =>
            {
                after = Some(cursor)
            }
            _ => {
                return Err(market_read_error(
                    symbol,
                    "candles_pagination_incomplete_or_nonforward",
                ));
            }
        }
    }
    Ok(candles)
}

async fn persisted_outcomes(
    state: &AnalyticsState,
    parsed: &[(AnalyticsSetupRow, ParsedRecommendation)],
    refresh: bool,
    cutoff: i64,
) -> Result<Vec<Option<SetupOutcome>>, Response> {
    materialize_outcomes(state, parsed, refresh, cutoff)
        .await
        .map(|result| result.0)
}

async fn materialize_outcomes(
    state: &AnalyticsState,
    parsed: &[(AnalyticsSetupRow, ParsedRecommendation)],
    refresh: bool,
    cutoff: i64,
) -> Result<(Vec<Option<SetupOutcome>>, usize, usize), Response> {
    let mut result = Vec::with_capacity(parsed.len());
    let mut missing = Vec::new();
    let mut positions = Vec::new();
    for (index, (row, rec)) in parsed.iter().enumerate() {
        let saved = row
            .trade_outcome
            .as_ref()
            .map(|value| serde_json::from_value::<SetupOutcome>(value.clone()))
            .transpose()
            .map_err(|_| read_error())?;
        if saved.is_none()
            || (refresh
                && saved.as_ref().is_some_and(|outcome| {
                    matches!(outcome.status.as_str(), "unclosed" | "no_data")
                }))
        {
            missing.push((row.clone(), rec.clone()));
            positions.push(index);
        }
        result.push(saved);
    }
    if missing.is_empty() {
        return Ok((result, 0, 0));
    }
    let _permit = state
        .outcome_compute
        .clone()
        .try_acquire_owned()
        .map_err(|_| error_response(StatusCode::CONFLICT, "setups_outcomes_busy"))?;
    // Finish all bounded reads before writing: a transport/parse/pagination failure writes no result.
    let computed = calculate_at(state, &missing, cutoff).await?;
    let computed_count = missing.len();
    let mut updated_count = 0;
    for ((row, _), (position, outcome)) in missing.iter().zip(positions.into_iter().zip(computed)) {
        let outcome = outcome.ok_or_else(read_error)?;
        let value = serde_json::to_value(&outcome).map_err(|_| read_error())?;
        let (winner, updated) = setups_db::live_outcomes::save(
            &state.pool,
            "admin",
            row.id,
            &value,
            refresh,
            row.trade_outcome.as_ref(),
        )
        .await
        .map_err(|_| read_error())?;
        updated_count += usize::from(updated);
        result[position] = Some(serde_json::from_value(winner).map_err(|_| read_error())?);
    }
    Ok((result, computed_count, updated_count))
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RefreshRequest {
    cursor: Option<String>,
    limit: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RefreshCursor {
    snapshot: i64,
    after: i64,
    cutoff: i64,
}

impl RefreshCursor {
    fn encode(self) -> String {
        format!("v1:{}:{}:{}", self.snapshot, self.after, self.cutoff)
    }
    fn parse(raw: &str, now: i64) -> Option<Self> {
        if raw.len() > 128 {
            return None;
        }
        let mut parts = raw.split(':');
        if parts.next()? != "v1" {
            return None;
        }
        let value = Self {
            snapshot: parts.next()?.parse().ok()?,
            after: parts.next()?.parse().ok()?,
            cutoff: parts.next()?.parse().ok()?,
        };
        (parts.next().is_none()
            && (0..=i64::from(i32::MAX)).contains(&value.snapshot)
            && (0..=value.snapshot).contains(&value.after)
            && value.cutoff >= 0
            && value.cutoff % 60 == 0
            && value.cutoff <= now)
            .then_some(value)
    }
}

async fn refresh_outcomes(
    State(state): State<Arc<AnalyticsState>>,
    headers: HeaderMap,
    Json(request): Json<RefreshRequest>,
) -> Response {
    if let Err(response) = guard_auth(&state.auth, &headers, true) {
        return response;
    }
    let limit = request.limit.unwrap_or(50);
    if !(1..=100).contains(&limit) {
        return error_response(StatusCode::BAD_REQUEST, "invalid_body");
    }
    let now = chrono::Utc::now().timestamp().div_euclid(60) * 60;
    let cursor = match request.cursor {
        Some(raw) => match RefreshCursor::parse(&raw, now) {
            Some(cursor) => cursor,
            None => return error_response(StatusCode::BAD_REQUEST, "invalid_cursor"),
        },
        None => {
            let snapshot = sqlx::query_scalar::<_, i32>("select coalesce(max(l.id),0) from live_setups l \
                where l.adaptive_slot_id is not null and l.recommendation is not null \
                and exists(select 1 from adaptive_profiles p where p.id=l.adaptive_slot_id and p.owner_id='admin')")
                .fetch_one(&state.pool).await;
            match snapshot {
                Ok(snapshot) => RefreshCursor {
                    snapshot: i64::from(snapshot),
                    after: 0,
                    cutoff: now,
                },
                Err(_) => return read_error(),
            }
        }
    };
    let rows = sqlx::query("select id,exchange_code,category,symbol,algorithm_id, \
        extract(epoch from close_time)::bigint close_unix,close_price,adaptive_slot_id::bigint slot_id, \
        coalesce(adaptive_generation_id,(adaptive_generation_archive->>'generationId')::bigint) generation_id, \
        expectancy_artifact_id artifact_id,recommendation,trade_outcome from live_setups l \
        where id>$1 and id<=$2 and adaptive_slot_id is not null and recommendation is not null \
        and exists(select 1 from adaptive_profiles p where p.id=l.adaptive_slot_id and p.owner_id='admin') \
        and (trade_outcome is null or trade_outcome->>'status' in ('unclosed','no_data')) order by id limit $3")
        .bind(cursor.after as i32).bind(cursor.snapshot as i32).bind((limit + 1) as i64)
        .fetch_all(&state.pool).await;
    let mut rows = match rows {
        Ok(rows) => rows,
        Err(_) => return read_error(),
    };
    let more = rows.len() > limit;
    rows.truncate(limit);
    let parsed = rows
        .into_iter()
        .map(|row| -> Result<_, Response> {
            let row = AnalyticsSetupRow {
                id: i64::from(row.try_get::<i32, _>("id").map_err(|_| read_error())?),
                exchange_code: row.try_get("exchange_code").map_err(|_| read_error())?,
                category: row.try_get("category").map_err(|_| read_error())?,
                symbol: row.try_get("symbol").map_err(|_| read_error())?,
                algorithm_id: row.try_get("algorithm_id").map_err(|_| read_error())?,
                close_unix: row.try_get("close_unix").map_err(|_| read_error())?,
                close_price: row.try_get("close_price").map_err(|_| read_error())?,
                slot_id: row.try_get("slot_id").map_err(|_| read_error())?,
                generation_id: row.try_get("generation_id").map_err(|_| read_error())?,
                artifact_id: row.try_get("artifact_id").map_err(|_| read_error())?,
                recommendation: row.try_get("recommendation").map_err(|_| read_error())?,
                trade_outcome: row.try_get("trade_outcome").map_err(|_| read_error())?,
            };
            let recommendation = parse_recommendation(&row.recommendation).map_err(|_| {
                error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "setups_analytics_invalid_recommendation",
                )
            })?;
            Ok((row, recommendation))
        })
        .collect::<Result<Vec<_>, _>>();
    let parsed = match parsed {
        Ok(parsed) => parsed,
        Err(response) => return response,
    };
    let next = if more {
        parsed.last().map(|(row, _)| {
            RefreshCursor {
                after: row.id,
                ..cursor
            }
            .encode()
        })
    } else {
        None
    };
    match materialize_outcomes(&state, &parsed, true, cursor.cutoff).await {
        Ok((_, computed, updated)) => Json(json!({"snapshotId":cursor.snapshot,
            "observedThroughUnix":cursor.cutoff,"processed":parsed.len(),"computed":computed,
            "updated":updated,"nextCursor":next}))
        .into_response(),
        Err(response) => response,
    }
}

fn analytics_result(
    parsed: &[(AnalyticsSetupRow, ParsedRecommendation)],
    studies: &HashMap<i64, i64>,
    outcomes: &[Option<SetupOutcome>],
) -> Response {
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
        totals.observe(&outcome.status, outcome.risk_return);
        if let Some(study) = study_ids[index] {
            per_study
                .entry(study)
                .or_default()
                .observe(&outcome.status, outcome.risk_return);
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
                "closePrice": outcome.close_price,
                "riskReturn": outcome.risk_return,
                "status": outcome.status,
                "evaluatedAtUnix": outcome.evaluated_at_unix,
                "observedThroughUnix": outcome.observed_through_unix,
                "isFinal": outcome.is_final,
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

async fn setups_analytics_studies(
    State(state): State<Arc<AnalyticsState>>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    if let Err(response) = guard_auth(&state.auth, &headers, false) {
        return response;
    }
    let filter = match crate::batches::parse_setup_filter(&params) {
        Ok(filter) => filter,
        Err(response) => return response,
    };
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
    // Aggregate in PostgreSQL and return only bounded study metadata. The 5000
    // trade cap applies to numerical calculations, not access to history/options.
    let mut query = sqlx::QueryBuilder::new("with matched as (select id, close_time, algorithm_id, recommendation, expectancy_artifact_id, coalesce(adaptive_generation_id,(adaptive_generation_archive->>'generationId')::bigint) as generation_id from live_setups where adaptive_slot_id is not null and recommendation is not null and exists(select 1 from adaptive_profiles p where p.id=adaptive_slot_id and p.owner_id='admin')");
    setups_db::runs::push_live_setup_filters(&mut query, &setups_db::runs::LiveSetupFilter {
        algorithm_id: Some(algorithm_id), from_unix, to_unix, q: filter.q, studies: None,
    });
    query.push("), resolved as (select *, coalesce((select g.study_id from continuous_generations g where g.id=matched.generation_id), case when (recommendation->>'studyId') ~ '^[0-9]{1,18}$' then (recommendation->>'studyId')::bigint end) as study_id from matched) select distinct on(study_id) study_id, algorithm_id, recommendation->>'externalName' as external_name, count(*) over(partition by study_id)::bigint as setup_count, generation_id, expectancy_artifact_id from resolved where study_id is not null order by study_id, close_time desc, id desc limit ")
        .push_bind((MAX_STUDY_OPTIONS + 1) as i64);
    let rows = match query.build().fetch_all(&state.pool).await {
        Ok(rows) => rows,
        Err(_) => return read_error(),
    };
    if rows.len() > MAX_STUDY_OPTIONS {
        return error_response(StatusCode::BAD_REQUEST, "setups_analytics_studies_limit_exceeded");
    }
    let items: Result<Vec<Value>, sqlx::Error> = rows.iter().map(|row| {
        let algorithm: String = row.try_get("algorithm_id")?;
        let external: Option<String> = row.try_get("external_name")?;
        Ok(json!({"studyId":row.try_get::<i64,_>("study_id")?, "externalName":external.filter(|name| !name.trim().is_empty()).unwrap_or_else(|| setups_db::algorithm_name(&algorithm)),
            "setupCount":row.try_get::<i64,_>("setup_count")?, "generationId":row.try_get::<Option<i64>,_>("generation_id")?, "artifactId":row.try_get::<Option<i64>,_>("expectancy_artifact_id")?}))
    }).collect();
    match items {
        Ok(items) => Json(json!({"studies":items})).into_response(),
        Err(_) => read_error(),
    }
}

#[cfg(test)]
fn row_study_id(row: &AnalyticsSetupRow, generation_studies: &HashMap<i64, i64>) -> Option<i64> {
    row.generation_id
        .and_then(|generation| generation_studies.get(&generation).copied())
        .or_else(|| {
            parse_recommendation(&row.recommendation)
                .ok()
                .and_then(|recommendation| recommendation.envelope_study_id)
        })
}

#[cfg(test)]
fn studies_list(rows: &[AnalyticsSetupRow], generation_studies: &HashMap<i64, i64>) -> Vec<Value> {
    let mut counts: BTreeMap<i64, i64> = BTreeMap::new();
    let mut latest: BTreeMap<i64, &AnalyticsSetupRow> = BTreeMap::new();
    for row in rows {
        let Some(study_id) = row_study_id(row, generation_studies) else {
            continue;
        };
        *counts.entry(study_id).or_default() += 1;
        let replace = match latest.get(&study_id) {
            Some(existing) => {
                row.close_unix > existing.close_unix
                    || (row.close_unix == existing.close_unix && row.id > existing.id)
            }
            None => true,
        };
        if replace {
            latest.insert(study_id, row);
        }
    }
    latest
        .into_iter()
        .map(|(study_id, row)| {
            let external_name = parse_recommendation(&row.recommendation)
                .ok()
                .and_then(|recommendation| {
                    recommendation
                        .external_name
                        .filter(|name| !name.trim().is_empty())
                })
                .unwrap_or_else(|| setups_db::algorithm_name(&row.algorithm_id));
            json!({
                "studyId": study_id,
                "externalName": external_name,
                "setupCount": counts.get(&study_id).copied().unwrap_or(0),
                "generationId": row.generation_id,
                "artifactId": row.artifact_id,
            })
        })
        .collect()
}

async fn setups_analytics_expectancy_png(
    State(state): State<Arc<AnalyticsState>>,
    headers: HeaderMap,
    Path(artifact_id): Path<i64>,
) -> Response {
    if let Err(response) = guard_auth(&state.auth, &headers, false) {
        return response;
    }
    let png = sqlx::query_scalar::<_, Vec<u8>>(
        "select p.png from study_expectancy_pngs p join study_expectancy_artifacts a on a.id = p.artifact_id \
         where a.id = $1 and a.owner_id = $2 order by p.rendering_version desc limit 1",
    )
    .bind(artifact_id)
    .bind("admin")
    .fetch_optional(&state.pool)
    .await;
    let bytes = match png {
        Ok(Some(bytes)) => Ok(bytes),
        Ok(None) => render_expectancy_png(&state.pool, artifact_id).await,
        Err(_) => Err(error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "expectancy_png_unavailable",
        )),
    };
    match bytes {
        Ok(bytes) => Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "image/png")
            .body(Body::from(bytes))
            .unwrap_or_else(|_| {
                error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "expectancy_png_unavailable",
                )
            }),
        Err(response) => response,
    }
}

async fn render_expectancy_png(pool: &sqlx::PgPool, artifact_id: i64) -> Result<Vec<u8>, Response> {
    let unavailable = |status: StatusCode| error_response(status, "expectancy_png_unavailable");
    let mut artifact = setups_db::expectancy::get(pool, "admin", artifact_id)
        .await
        .map_err(|_| unavailable(StatusCode::INTERNAL_SERVER_ERROR))?
        .ok_or_else(|| unavailable(StatusCode::NOT_FOUND))?;
    let numerical: setups_domain::expectancy::ExpectancyArtifact =
        serde_json::from_value(std::mem::take(&mut artifact.numerical_snapshot))
            .map_err(|_| unavailable(StatusCode::INTERNAL_SERVER_ERROR))?;
    let recipe = numerical.source.forecast;
    let saved = numerical
        .forecast
        .as_ref()
        .and_then(|model| model.trend.as_ref())
        .map(|trend| trend.end.value);
    tokio::task::spawn_blocking(move || {
        if let (Some(_), Some(value)) = (recipe, saved) {
            setups_analytics::expectancy_chart::render_png_with_model(
                &numerical,
                numerical.source.provenance.observation_cutoff,
                value,
            )
        } else {
            setups_analytics::expectancy_chart::render_png_with_trend(
                &numerical,
                setups_analytics::expectancy_chart::StudyTrend::default(),
            )
        }
    })
    .await
    .map_err(|_| unavailable(StatusCode::INTERNAL_SERVER_ERROR))?
    .map_err(|_| unavailable(StatusCode::NOT_FOUND))
}

async fn setups_analytics_study_expectancy(
    State(state): State<Arc<AnalyticsState>>,
    headers: HeaderMap,
    Path(study_id): Path<i64>,
    Json(body): Json<Value>,
) -> Response {
    if let Err(response) = guard_auth(&state.auth, &headers, false) {
        return response;
    }
    let mut live: Vec<setups_analytics::expectancy_chart::LiveDailyPoint> = Vec::new();
    if let Some(items) = body.get("live").and_then(Value::as_array) {
        for item in items.iter().take(1000) {
            if let (Some(day_unix), Some(mean_r), Some(closed)) = (
                item.get("dayUnix").and_then(Value::as_i64),
                item.get("meanR").and_then(Value::as_f64),
                item.get("closedCount").and_then(Value::as_u64),
            ) {
                live.push(setups_analytics::expectancy_chart::LiveDailyPoint {
                    day_unix,
                    mean_r,
                    closed: u32::try_from(closed).unwrap_or(u32::MAX),
                });
            }
        }
    }
    let row = match sqlx::query(
        "select numerical_snapshot from study_expectancy_artifacts where source_study_id=$1 and owner_id='admin' order by id desc limit 1",
    )
    .bind(study_id)
    .fetch_optional(&state.pool)
    .await
    {
        Ok(Some(row)) => row,
        Ok(None) => {
            return error_response(StatusCode::NOT_FOUND, "study_expectancy_unavailable")
        }
        Err(_) => {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "study_expectancy_unavailable",
            )
        }
    };
    let snapshot: Value = match row.try_get("numerical_snapshot") {
        Ok(value) => value,
        Err(_) => {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "study_expectancy_unavailable",
            );
        }
    };
    let artifact: setups_domain::expectancy::ExpectancyArtifact =
        match serde_json::from_value(snapshot) {
            Ok(value) => value,
            Err(_) => {
                return error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "study_expectancy_unavailable",
                );
            }
        };
    let trend =
        match sqlx::query("select spec from optimization_studies where id=$1 and owner_id='admin'")
            .bind(study_id)
            .fetch_optional(&state.pool)
            .await
        {
            Ok(Some(row)) => row
                .try_get::<Value, _>("spec")
                .ok()
                .and_then(|spec| {
                    serde_json::from_value::<setups_analytics::expectancy_chart::StudyTrend>(spec)
                        .ok()
                })
                .unwrap_or_default(),
            _ => setups_analytics::expectancy_chart::StudyTrend::default(),
        };
    let cutoff = artifact.source.provenance.observation_cutoff;
    let origin_unix = artifact.origin_unix;
    let daily = match &artifact.daily {
        setups_domain::expectancy::MetricAvailability::Available { value } => value
            .iter()
            .map(|point| {
                json!({
                    "dayUnix": point.day_start_unix,
                    "meanR": point.mean_r,
                    "tpCount": point.tp_count,
                    "slCount": point.sl_count,
                })
            })
            .collect::<Vec<_>>(),
        _ => Vec::new(),
    };
    let series = match tokio::task::spawn_blocking(move || {
        setups_analytics::expectancy_chart::study_series(&artifact, trend, &live)
    })
    .await
    {
        Ok(Ok(series)) => series,
        _ => return error_response(StatusCode::NOT_FOUND, "study_expectancy_unavailable"),
    };
    let trend_points = series
        .trend_points
        .iter()
        .map(|&(day, value)| {
            json!({
                "dayUnix": origin_unix + (day * 86400.0).round() as i64,
                "value": value,
            })
        })
        .collect::<Vec<_>>();
    Json(json!({
        "studyId": study_id,
        "observationCutoff": cutoff,
        "meanR": series.mean_r,
        "daily": daily,
        "trend": { "method": series.trend_method, "points": trend_points },
    }))
    .into_response()
}

pub fn setups_analytics_router(state: Arc<AnalyticsState>) -> Router {
    Router::new()
        .route(
            "/v1/setups/analytics",
            get(saved_setups_analytics).post(setups_analytics),
        )
        .route("/v1/setups/outcomes/refresh", post(refresh_outcomes))
        .route(
            "/v1/setups/analytics/studies",
            get(setups_analytics_studies),
        )
        .route(
            "/v1/setups/analytics/expectancy-png/{artifactId}",
            get(setups_analytics_expectancy_png),
        )
        .route(
            "/v1/setups/analytics/study-expectancy/{studyId}",
            post(setups_analytics_study_expectancy),
        )
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use setups_analytics::OhlcCandle;
    use std::sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    };

    fn recommendation_json() -> Value {
        json!({
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
            "provenance": { "algorithmId": "price_volume_spike" },
            "studyId": 40,
            "generationId": 36,
            "slotId": 1,
            "ownerId": "admin",
            "externalName": "Всплеск цены и оборота",
            "rawSignalAt": 1794000000,
            "expectancy": { "meanR": 0.32 },
            "recommendedEntryAt": 1794000300
        })
    }

    #[test]
    fn parses_flat_recommendation_with_extra_keys_and_ids() {
        let parsed = parse_recommendation(&recommendation_json()).unwrap();
        assert!(matches!(parsed.direction, TradeDirection::Short));
        assert_eq!(parsed.envelope_study_id, Some(40));
        assert_eq!(parsed.price_change_pct, Some(2.0));
        assert_eq!(parsed.tp_pct, 20.0);
        assert_eq!(parsed.time_shift_minutes, 5);
    }

    #[test]
    fn parses_string_ids_and_missing_price_change() {
        let mut value = recommendation_json();
        value["studyId"] = json!("41");
        value["priceChangePct"] = Value::Null;
        let parsed = parse_recommendation(&value).unwrap();
        assert_eq!(parsed.envelope_study_id, Some(41));
        assert_eq!(parsed.price_change_pct, None);
    }

    #[test]
    fn rejects_invalid_direction_and_ranges() {
        let mut value = recommendation_json();
        value["direction"] = json!("side");
        assert!(parse_recommendation(&value).is_err());
        let mut value = recommendation_json();
        value["tpPct"] = json!(0.0);
        assert!(parse_recommendation(&value).is_err());
        let mut value = recommendation_json();
        value["timeShiftMinutes"] = json!(43201);
        assert!(parse_recommendation(&value).is_err());
        assert!(parse_recommendation(&json!({})).is_err());
    }

    #[test]
    fn caption_gate_matches_direction_semantics() {
        assert_eq!(gate_threshold(100.0, 7.0, TradeDirection::Short), 107.0);
        assert!(!passes_price_change(
            100.0,
            106.9,
            TradeDirection::Short,
            7.0
        ));
        assert!(passes_price_change(
            100.0,
            107.0,
            TradeDirection::Short,
            7.0
        ));
        assert!(passes_price_change(
            100.0,
            120.0,
            TradeDirection::Short,
            7.0
        ));
        assert_eq!(gate_threshold(100.0, 7.0, TradeDirection::Long), 93.0);
        assert!(passes_price_change(100.0, 93.0, TradeDirection::Long, 7.0));
        assert!(passes_price_change(100.0, 90.0, TradeDirection::Long, 7.0));
        assert!(!passes_price_change(100.0, 93.1, TradeDirection::Long, 7.0));
        assert_eq!(gate_threshold(100.0, -2.0, TradeDirection::Short), 98.0);
        assert!(passes_price_change(
            100.0,
            98.0,
            TradeDirection::Short,
            -2.0
        ));
    }

    fn candle(open_time_unix: i64, open: f64, high: f64, low: f64, close: f64) -> OhlcCandle {
        OhlcCandle {
            open_time_unix,
            open,
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
        assert_eq!(outcome.gate_threshold, Some(102.0));
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
            candle(1_794_000_300, 98.5, 99.0, 98.0, 98.2),
            candle(1_794_000_360, 99.0, 107.0, 100.0, 106.0),
        ];
        let setup = fabric();
        let evaluation = evaluate_limit_at_open(EvaluateLimitOptions {
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
        assert_eq!(evaluation.entry_time_unix, Some(1_794_000_300));
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
            candle(1_794_000_300, 98.5, 99.0, 98.0, 98.2),
            candle(1_794_000_360, 98.0, 99.0, 78.0, 79.0),
        ];
        let evaluation = evaluate_limit_at_open(EvaluateLimitOptions {
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
    fn irys_regression_rejects_gate_and_uses_open_entry() {
        let parsed = recommendation(Some(7.0));
        assert!((gate_threshold(0.017465, 7.0, TradeDirection::Short) - 0.01868755).abs() < 1e-9);
        assert!(!passes_price_change(
            0.017465,
            0.017442,
            TradeDirection::Short,
            7.0
        ));
        let setup = AnalyticsSetup {
            exchange_code: "bybit".into(),
            category: "linear".into(),
            symbol: "IRYSUSDT".into(),
            open_time_unix: 1_794_996_300,
            close_time_unix: 1_794_996_300,
            close_price: "0.017465".into(),
        };
        let candles = vec![candle(1_794_997_140, 0.017442, 0.0175, 0.0172, 0.017248)];
        let evaluation = evaluate_limit_at_open(EvaluateLimitOptions {
            candles: &candles,
            setup: &setup,
            direction: TradeDirection::Short,
            tp_pct: 8.0,
            sl_pct: 4.0,
            time_shift_minutes: 14,
            horizon_minutes: ANALYTICS_HORIZON_MINUTES,
        });
        assert_eq!(evaluation.entry_price, Some(0.017442));
        assert_eq!(evaluation.entry_time_unix, Some(1_794_997_140));
        let outcome = classify(&parsed, evaluation, 0.017465);
        assert_eq!(outcome.status, "gate_failed");
        assert_eq!(outcome.entry_price, Some(0.017442));
        assert!((outcome.gate_threshold.unwrap() - 0.01868755).abs() < 1e-9);
        assert_eq!(outcome.tp_price, None);
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

    fn analytics_setup_row(
        id: i64,
        close_unix: i64,
        recommendation: Value,
        generation_id: Option<i64>,
        artifact_id: Option<i64>,
    ) -> AnalyticsSetupRow {
        AnalyticsSetupRow {
            id,
            exchange_code: "bybit".into(),
            category: "linear".into(),
            symbol: "TESTUSDT".into(),
            algorithm_id: "price_volume_spike".into(),
            close_unix,
            close_price: "100".into(),
            slot_id: 1,
            generation_id,
            recommendation,
            artifact_id,
            trade_outcome: None,
        }
    }

    #[test]
    fn studies_list_groups_counts_and_latest_per_study() {
        let other = {
            let mut value = recommendation_json();
            value["studyId"] = json!("41");
            value
        };
        let rows = vec![
            analytics_setup_row(2, 200, recommendation_json(), Some(10), Some(7)),
            analytics_setup_row(1, 100, recommendation_json(), Some(10), Some(5)),
            analytics_setup_row(3, 300, other, Some(11), None),
        ];
        let studies = studies_list(&rows, &HashMap::new());
        assert_eq!(studies.len(), 2);
        assert_eq!(studies[0]["studyId"], 40);
        assert_eq!(studies[0]["setupCount"], 2);
        assert_eq!(studies[0]["generationId"], 10);
        assert_eq!(studies[0]["artifactId"], 7);
        assert!(studies[0]["externalName"].is_string());
        assert_eq!(studies[1]["studyId"], 41);
        assert_eq!(studies[1]["setupCount"], 1);
        assert!(studies[1]["artifactId"].is_null());
    }

    #[test]
    fn studies_list_resolves_study_via_generation_map_and_skips_unresolvable() {
        let mut legacy = recommendation_json();
        legacy.as_object_mut().unwrap().remove("studyId");
        let rows = vec![
            analytics_setup_row(1, 100, legacy.clone(), Some(10), Some(9)),
            analytics_setup_row(2, 200, legacy, None, None),
        ];
        let mut generation_studies = HashMap::new();
        generation_studies.insert(10, 55);
        let studies = studies_list(&rows, &generation_studies);
        assert_eq!(studies.len(), 1);
        assert_eq!(studies[0]["studyId"], 55);
        assert_eq!(studies[0]["artifactId"], 9);
    }

    #[test]
    fn studies_parser_validates_input() {
        assert!(parse_studies(None).unwrap().is_none());
        let studies = parse_studies(Some("1, 3")).unwrap().unwrap();
        assert_eq!(studies, vec![1, 3]);
        assert_eq!(parse_studies(Some("")).unwrap(), Some(vec![]));
        assert!(parse_studies(Some("0")).is_err());
        assert!(parse_studies(Some("a")).is_err());
        assert!(parse_studies(Some("1,,2")).is_err());
    }

    #[derive(Default)]
    struct ChangingMarket {
        candles: Mutex<Vec<OhlcCandle>>,
        reads: AtomicUsize,
        failure: AtomicUsize,
    }

    async fn market_fixture() -> (
        AnalyticsState,
        Arc<ChangingMarket>,
        tokio::task::JoinHandle<()>,
    ) {
        let market = Arc::new(ChangingMarket::default());
        let router = Router::new()
            .route("/v1/instruments", get(|| async {
                Json(json!({"instruments": [{"id": 7, "exchange": "bybit", "category": "linear", "symbol": "TESTUSDT", "status": "active"}]}))
            }))
            .route("/v1/candles", get(|State(market): State<Arc<ChangingMarket>>| async move {
                market.reads.fetch_add(1, Ordering::SeqCst);
                match market.failure.load(Ordering::SeqCst) {
                    1 => return (StatusCode::SERVICE_UNAVAILABLE, "private upstream error body").into_response(),
                    2 => return (StatusCode::OK, "private malformed upstream body").into_response(),
                    _ => {}
                }
                let candles: Vec<Value> = market.candles.lock().unwrap().iter().map(|candle| json!({
                    "openTimeUnix": candle.open_time_unix,
                    "open": candle.open.to_string(), "high": candle.high.to_string(),
                    "low": candle.low.to_string(), "close": candle.close.to_string(),
                    "volume": "1", "quoteTurnover": "100",
                })).collect();
                Json(json!({"candles": candles, "nextCursorUnix": null})).into_response()
            }))
            .with_state(market.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let state = AnalyticsState {
            pool: sqlx::postgres::PgPoolOptions::new()
                .connect_lazy("postgres://unused:unused@127.0.0.1:1/unused_test")
                .unwrap(),
            auth: Arc::new(crate::auth::SessionAuth::new(&[])),
            reader: crate::analytics::AnalyticsCandleReader::new(&url, "fixture-key"),
            market_url: url,
            market_key: "fixture-key".into(),
            forecast_cpu: Arc::new(tokio::sync::Semaphore::new(1)),
            outcome_compute: Arc::new(tokio::sync::Semaphore::new(1)),
        };
        (state, market, task)
    }

    async fn calculated_payload(state: &AnalyticsState) -> Value {
        let parsed = vec![(
            analytics_setup_row(
                1,
                fabric().close_time_unix,
                recommendation_json(),
                None,
                None,
            ),
            recommendation(None),
        )];
        let outcomes = calculate_outcomes(state, &parsed)
            .await
            .unwrap_or_else(|_| panic!("fixture market read failed"));
        let response = analytics_result(&parsed, &HashMap::new(), &outcomes);
        let body = axum::body::to_bytes(response.into_body(), 16_384)
            .await
            .unwrap();
        serde_json::from_slice(&body).unwrap()
    }

    #[tokio::test]
    async fn repeat_calculation_refreshes_missing_entry_and_totals_without_caching_no_data() {
        let (state, market, server) = market_fixture().await;
        let first = calculated_payload(&state).await;
        assert_eq!(first["setups"][0]["status"], "no_data");
        assert_eq!(
            first["totals"],
            json!({"selected": 1, "gatePassed": 0, "closed": 0, "meanR": null})
        );

        *market.candles.lock().unwrap() = vec![
            candle(1_794_000_300, 100.0, 101.0, 99.0, 100.0),
            candle(1_794_000_360, 100.0, 107.0, 99.0, 106.0),
        ];
        let second = calculated_payload(&state).await;
        server.abort();
        let _ = server.await;
        assert_eq!(second["setups"][0]["status"], "sl");
        assert_eq!(second["setups"][0]["entryTimeUnix"], 1_794_000_300_i64);
        assert_eq!(
            second["totals"],
            json!({"selected": 1, "gatePassed": 1, "closed": 1, "meanR": -1.0})
        );
        assert_eq!(market.reads.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn repeat_calculation_observes_later_exit_candle_instead_of_caching_unclosed() {
        let (state, market, server) = market_fixture().await;
        *market.candles.lock().unwrap() = vec![candle(1_794_000_300, 100.0, 101.0, 99.0, 100.0)];
        let first = calculated_payload(&state).await;
        assert_eq!(first["setups"][0]["status"], "unclosed");
        assert_eq!(first["totals"]["closed"], 0);

        market
            .candles
            .lock()
            .unwrap()
            .push(candle(1_794_000_360, 100.0, 101.0, 79.0, 80.0));
        let second = calculated_payload(&state).await;
        server.abort();
        let _ = server.await;
        assert_eq!(second["setups"][0]["status"], "tp");
        assert_eq!(second["setups"][0]["closeTimeUnix"], 1_794_000_360_i64);
        assert_eq!(second["totals"]["closed"], 1);
        assert!((second["totals"]["meanR"].as_f64().unwrap() - 20.0 / 6.0).abs() < 1e-12);
        assert_eq!(market.reads.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn operational_candle_failures_are_not_reported_as_missing_data() {
        let (state, market, server) = market_fixture().await;
        let parsed = vec![(
            analytics_setup_row(
                1,
                fabric().close_time_unix,
                recommendation_json(),
                None,
                None,
            ),
            recommendation(None),
        )];
        for failure in [1, 2] {
            market.failure.store(failure, Ordering::SeqCst);
            let response = match calculate_outcomes(&state, &parsed).await {
                Ok(_) => panic!("market failure must not produce successful no_data outcomes"),
                Err(response) => response,
            };
            assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
            let body = axum::body::to_bytes(response.into_body(), 16_384)
                .await
                .unwrap();
            assert_eq!(
                serde_json::from_slice::<Value>(&body).unwrap(),
                json!({"error": "setups_analytics_market_data_unavailable"})
            );
        }
        server.abort();
        let _ = server.await;
        assert_eq!(market.reads.load(Ordering::SeqCst), 2);
    }
}
