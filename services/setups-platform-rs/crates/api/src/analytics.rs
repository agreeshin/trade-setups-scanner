use crate::auth::SessionAuth;
use crate::replay::guard_auth;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get};
use axum::{Json, Router};
use serde_json::{Map, Value, json};
use setups_analytics::{
    AnalyticsSetup, DEFAULT_CLUSTER_GAP_HOURS, DEFAULT_HORIZON_MINUTES, EvaluateLimitOptions,
    LimitEvaluation, LimitStatus, OhlcCandle, TradeDirection, aggregate_limit_outcomes,
    day_range_to_unix_bounds, evaluate_limit, filter_by_day_range, filter_by_month,
    passes_price_change, resolve_entry, risk_return, select_by_maturity, setup_months,
};
use setups_db::analytics::{
    AnalyticsSetupRow, CachedPayload, ComputeParams, SetRef, analytics_cache_summary,
    clear_analytics_cache, get_matrix_cell, get_selection, get_trade_row, list_analytics_sets,
    list_matrix_cells, list_trade_rows, load_analytics_setups, matrix_scope_key,
    maturity_tenths_of, parse_set_id, price_change_ppm_of, save_matrix_cell, save_selection,
    save_trade_row, selection_scope_key,
};
use setups_mdclient::{CandlesQuery, MarketDataClient};
use sqlx::postgres::PgPool;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Hard cap protecting one request from unbounded setup sets.
const MAX_SETUPS_PER_COMPUTE: usize = 100_000;
const DEFAULT_CLUSTER_GAP_MINUTES: i64 = DEFAULT_CLUSTER_GAP_HOURS * 60;

pub struct AnalyticsState {
    pub pool: PgPool,
    pub auth: Arc<SessionAuth>,
    pub market_url: String,
    pub market_key: String,
    pub reader: AnalyticsCandleReader,
}

fn error_response(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}

fn internal_error() -> Response {
    error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal_error")
}

const PAGE_LIMIT: u32 = 5000;
/** Bounded in-memory window cache: max candles retained across instruments. */
const MAX_CACHED_CANDLES: usize = 2_000_000;

struct WindowCache {
    map: HashMap<String, Arc<Vec<OhlcCandle>>>,
    order: VecDeque<String>,
    count: usize,
}

pub struct AnalyticsCandleReader {
    market: MarketDataClient,
    catalog: Mutex<Option<Arc<HashMap<String, i64>>>>,
    windows: Mutex<WindowCache>,
}

impl AnalyticsCandleReader {
    pub fn new(base_url: &str, api_key: &str) -> Self {
        Self {
            market: MarketDataClient::new(base_url.to_string(), api_key.to_string()),
            catalog: Mutex::new(None),
            windows: Mutex::new(WindowCache {
                map: HashMap::new(),
                order: VecDeque::new(),
                count: 0,
            }),
        }
    }

    fn catalog_key(exchange: &str, category: &str, symbol: &str) -> String {
        format!("{exchange}\0{category}\0{symbol}")
    }

    async fn catalog(&self) -> Option<Arc<HashMap<String, i64>>> {
        {
            let guard = self.catalog.lock().expect("analytics catalog lock");
            if let Some(existing) = guard.as_ref() {
                return Some(existing.clone());
            }
        }
        let instruments = self.market.instruments().await.ok()?;
        let mut map = HashMap::new();
        for instrument in instruments {
            map.insert(
                Self::catalog_key(
                    &instrument.exchange,
                    &instrument.category,
                    &instrument.symbol,
                ),
                instrument.id,
            );
        }
        let shared = Arc::new(map);
        *self.catalog.lock().expect("analytics catalog lock") = Some(shared.clone());
        Some(shared)
    }

    pub async fn instrument_id(&self, exchange: &str, category: &str, symbol: &str) -> Option<i64> {
        let catalog = self.catalog().await?;
        catalog
            .get(&Self::catalog_key(exchange, category, symbol))
            .copied()
    }

    /// Read all minute candles of one instrument inside [fromUnix, toUnix).
    /// Windows are rounded to hour boundaries so repeated matrix cells over
    /// the same set reuse the same in-memory window.
    pub async fn read_window(
        &self,
        instrument_id: i64,
        from_unix: i64,
        to_unix: i64,
    ) -> Result<Arc<Vec<OhlcCandle>>, String> {
        let aligned_from = from_unix.div_euclid(3600) * 3600;
        let aligned_to = (to_unix + 3599).div_euclid(3600) * 3600;
        let key = format!("{instrument_id}:{aligned_from}:{aligned_to}");
        {
            let windows = self.windows.lock().expect("analytics window lock");
            if let Some(cached) = windows.map.get(&key) {
                return Ok(cached.clone());
            }
        }
        let mut candles: Vec<OhlcCandle> = Vec::new();
        let mut after_unix: Option<i64> = None;
        loop {
            let response = self
                .market
                .candles(
                    &CandlesQuery {
                        instrument_id,
                        from_unix: aligned_from,
                        to_unix: aligned_to,
                        after_unix,
                        limit: Some(PAGE_LIMIT),
                    },
                    Duration::from_secs(30),
                )
                .await
                .map_err(|error| error.to_string())?;
            for candle in &response.candles {
                candles.push(OhlcCandle {
                    open_time_unix: candle.open_time_unix,
                    open: candle.open.parse().unwrap_or(f64::NAN),
                    high: candle.high.parse().unwrap_or(f64::NAN),
                    low: candle.low.parse().unwrap_or(f64::NAN),
                    close: candle.close.parse().unwrap_or(f64::NAN),
                });
            }
            if response.next_cursor_unix.is_none() || response.candles.is_empty() {
                break;
            }
            after_unix = response.next_cursor_unix;
        }
        candles.sort_by_key(|candle| candle.open_time_unix);
        let shared = Arc::new(candles);
        {
            let mut windows = self.windows.lock().expect("analytics window lock");
            if windows.map.contains_key(&key) {
                return Ok(windows.map.get(&key).expect("window present").clone());
            }
            let len = shared.len();
            while windows.count + len > MAX_CACHED_CANDLES && !windows.order.is_empty() {
                let oldest_key = windows.order.pop_front().expect("oldest key");
                if let Some(oldest) = windows.map.remove(&oldest_key) {
                    windows.count -= oldest.len();
                }
            }
            windows.order.push_back(key.clone());
            windows.map.insert(key, shared.clone());
            windows.count += len;
        }
        Ok(shared)
    }

    async fn read_window_lossy(
        &self,
        instrument_id: i64,
        from_unix: i64,
        to_unix: i64,
    ) -> Arc<Vec<OhlcCandle>> {
        self.read_window(instrument_id, from_unix, to_unix)
            .await
            .unwrap_or_else(|_| Arc::new(Vec::new()))
    }
}

fn to_engine_setups(rows: &[AnalyticsSetupRow]) -> Vec<AnalyticsSetup> {
    rows.iter()
        .map(|row| AnalyticsSetup {
            exchange_code: row.exchange_code.clone(),
            category: row.category.clone(),
            symbol: row.symbol.clone(),
            open_time_unix: row.open_time_unix,
            close_time_unix: row.close_time_unix,
            close_price: row.close_price.clone(),
        })
        .collect()
}

fn identity_of(setup: &AnalyticsSetup) -> String {
    format!(
        "{}\0{}\0{}\0{}",
        setup.exchange_code, setup.category, setup.symbol, setup.close_time_unix
    )
}

#[derive(Debug, Clone, Default)]
struct RawAnalyticsQuery {
    set: Option<String>,
    direction: Option<String>,
    month: Option<String>,
    from_day: Option<String>,
    to_day: Option<String>,
    setup_maturity: Option<String>,
    cluster_gap_hours: Option<String>,
    tp_pct: Option<String>,
    sl_pct: Option<String>,
    time_shift_minutes: Option<String>,
    horizon_minutes: Option<String>,
    price_change_pct: Option<String>,
    exchange: Option<String>,
    category: Option<String>,
}

fn parse_number(value: Option<&String>) -> Result<Option<f64>, String> {
    match value {
        None => Ok(None),
        Some(raw) => raw
            .parse::<f64>()
            .map(Some)
            .map_err(|_| "invalid_query".to_string()),
    }
}

fn parse_integer(value: Option<&String>) -> Result<Option<i64>, String> {
    match value {
        None => Ok(None),
        Some(raw) => raw
            .parse::<i64>()
            .map(Some)
            .map_err(|_| "invalid_query".to_string()),
    }
}

fn valid_month_key(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 7
        && bytes[4] == b'-'
        && bytes[..4].iter().all(u8::is_ascii_digit)
        && bytes[5..].iter().all(u8::is_ascii_digit)
        && matches!(
            &value[5..7],
            "01" | "02" | "03" | "04" | "05" | "06" | "07" | "08" | "09" | "10" | "11" | "12"
        )
}

fn valid_day_key(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes
            .iter()
            .enumerate()
            .all(|(index, byte)| index == 4 || index == 7 || byte.is_ascii_digit())
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TimeFilter {
    All,
    Month(String),
    Range(String, String),
}

#[derive(Debug, Clone)]
struct ParsedAnalyticsParams {
    set_ref: SetRef,
    direction: TradeDirection,
    time_filter: TimeFilter,
    cache_time_key: String,
    maturity: Option<f64>,
    maturity_tenths: i64,
    cluster_gap_minutes: i64,
    tp_pct: f64,
    sl_pct: f64,
    time_shift_minutes: i64,
    horizon_minutes: i64,
    price_change_pct: Option<f64>,
    compute: ComputeParams,
}

impl ParsedAnalyticsParams {
    fn direction_str(&self) -> &'static str {
        match self.direction {
            TradeDirection::Short => "short",
            TradeDirection::Long => "long",
        }
    }
}

fn parse_analytics_query(
    query: &RawAnalyticsQuery,
    require_tp_sl: bool,
) -> Result<ParsedAnalyticsParams, String> {
    let set_raw = query.set.as_deref().unwrap_or_default();
    let set_ref = parse_set_id(set_raw).ok_or_else(|| "invalid set id".to_string())?;
    let direction = match query.direction.as_deref() {
        None => TradeDirection::Short,
        Some("short") => TradeDirection::Short,
        Some("long") => TradeDirection::Long,
        Some(_) => return Err("invalid_query".to_string()),
    };
    let setup_maturity = parse_number(query.setup_maturity.as_ref())?;
    if let Some(maturity) = setup_maturity {
        let tenths = maturity * 10.0;
        if (tenths - tenths.round()).abs() > f64::EPSILON || !(0.0..=10.0).contains(&tenths) {
            return Err("setupMaturity must be 0.0..1.0 with step 0.1".to_string());
        }
    }
    let tp_pct = parse_number(query.tp_pct.as_ref())?;
    let sl_pct = parse_number(query.sl_pct.as_ref())?;
    if require_tp_sl && (tp_pct.is_none() || sl_pct.is_none()) {
        return Err("tpPct and slPct are required".to_string());
    }
    for value in [tp_pct, sl_pct].into_iter().flatten() {
        if !(0.000_001..=100.0).contains(&value) {
            return Err("invalid_query".to_string());
        }
    }
    let cluster_gap_hours = parse_number(query.cluster_gap_hours.as_ref())?;
    let cluster_gap_minutes = cluster_gap_hours
        .map(|hours| (hours * 60.0).round() as i64)
        .unwrap_or(DEFAULT_CLUSTER_GAP_MINUTES);
    if !(1..=168 * 60).contains(&cluster_gap_minutes) {
        return Err("clusterGapHours must be 0.01..168".to_string());
    }
    let from_day = query.from_day.clone();
    let to_day = query.to_day.clone();
    if from_day.is_some() != to_day.is_some() {
        return Err("fromDay and toDay must be set together".to_string());
    }
    if let Some(day) = &from_day
        && !valid_day_key(day)
    {
        return Err("invalid_query".to_string());
    }
    if let Some(day) = &to_day
        && !valid_day_key(day)
    {
        return Err("invalid_query".to_string());
    }
    let month = query.month.clone();
    if let Some(key) = &month
        && !valid_month_key(key)
    {
        return Err("invalid_query".to_string());
    }
    let mut time_filter = TimeFilter::All;
    let mut cache_time_key = "all".to_string();
    if let (Some(from), Some(to)) = (&from_day, &to_day) {
        let Some((from_unix, to_unix)) = day_range_to_unix_bounds(from, to) else {
            return Err("invalid fromDay/toDay range".to_string());
        };
        if from_unix > to_unix {
            return Err("invalid fromDay/toDay range".to_string());
        }
        time_filter = TimeFilter::Range(from.clone(), to.clone());
        cache_time_key = format!("d:{from}:{to}");
    } else if let Some(key) = month {
        time_filter = TimeFilter::Month(key.clone());
        cache_time_key = key;
    }
    let time_shift_minutes = parse_integer(query.time_shift_minutes.as_ref())?.unwrap_or(0);
    if !(0..=43_200).contains(&time_shift_minutes) {
        return Err("invalid_query".to_string());
    }
    let horizon_minutes = parse_integer(query.horizon_minutes.as_ref())?;
    let horizon_minutes = horizon_minutes.unwrap_or(DEFAULT_HORIZON_MINUTES);
    if !(1..=43_200).contains(&horizon_minutes) {
        return Err("invalid_query".to_string());
    }
    let price_change_pct = parse_number(query.price_change_pct.as_ref())?;
    if let Some(value) = price_change_pct
        && !(-100.0..=1000.0).contains(&value)
    {
        return Err("invalid_query".to_string());
    }
    let maturity = setup_maturity;
    let effective_gap_minutes = if maturity.is_some() {
        cluster_gap_minutes
    } else {
        -1
    };
    let tp_pct = tp_pct.unwrap_or(0.0);
    let sl_pct = sl_pct.unwrap_or(0.0);
    let price_change_ppm = price_change_ppm_of(price_change_pct);
    let maturity_tenths = maturity_tenths_of(maturity);
    Ok(ParsedAnalyticsParams {
        set_ref,
        direction,
        time_filter,
        cache_time_key,
        maturity,
        maturity_tenths,
        cluster_gap_minutes: effective_gap_minutes,
        tp_pct,
        sl_pct,
        time_shift_minutes,
        horizon_minutes,
        price_change_pct,
        compute: ComputeParams {
            direction: match direction {
                TradeDirection::Short => "short".to_string(),
                TradeDirection::Long => "long".to_string(),
            },
            tp_pct,
            sl_pct,
            horizon_minutes,
            time_shift_minutes,
            price_change_ppm,
            maturity_tenths,
            cluster_gap_minutes: effective_gap_minutes,
        },
    })
}

fn filter_setups_by_time(setups: &[AnalyticsSetup], filter: &TimeFilter) -> Vec<AnalyticsSetup> {
    match filter {
        TimeFilter::All => setups.to_vec(),
        TimeFilter::Month(key) => filter_by_month(setups, key).unwrap_or_default(),
        TimeFilter::Range(from, to) => filter_by_day_range(setups, from, to).unwrap_or_default(),
    }
}

struct SelectionResult {
    rows: Vec<AnalyticsSetup>,
    avg_selection_ordinal: Option<f64>,
    avg_selection_delay_minutes: Option<f64>,
    total_clusters_count: Option<i64>,
    cached_selection: bool,
}

fn setups_fingerprint(setups: &[AnalyticsSetup]) -> (i64, Option<i64>, Option<i64>) {
    (
        setups.len() as i64,
        setups.first().map(|setup| setup.close_time_unix),
        setups.last().map(|setup| setup.close_time_unix),
    )
}

async fn select_setup_rows(
    pool: &PgPool,
    setups: &[AnalyticsSetup],
    params: &ParsedAnalyticsParams,
) -> SelectionResult {
    let in_scope = filter_setups_by_time(setups, &params.time_filter);
    if params.maturity.is_none() {
        return SelectionResult {
            rows: in_scope,
            avg_selection_ordinal: None,
            avg_selection_delay_minutes: None,
            total_clusters_count: None,
            cached_selection: false,
        };
    }
    let (count, first, last) = setups_fingerprint(setups);
    let scope_key = selection_scope_key(
        params.set_ref.kind.as_str(),
        params.set_ref.id,
        count,
        first,
        last,
    );
    if let Ok(Some(cached)) = get_selection(
        pool,
        &scope_key,
        params.maturity_tenths,
        params.cluster_gap_minutes,
        &params.cache_time_key,
    )
    .await
        && let Some(identities) = cached.get("identities").and_then(Value::as_array)
    {
        let identity_set: HashSet<String> = identities
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect();
        let rows = in_scope
            .iter()
            .filter(|setup| identity_set.contains(&identity_of(setup)))
            .cloned()
            .collect();
        return SelectionResult {
            rows,
            avg_selection_ordinal: cached.get("avgSelectionOrdinal").and_then(Value::as_f64),
            avg_selection_delay_minutes: cached
                .get("avgSelectionDelayMinutes")
                .and_then(Value::as_f64),
            total_clusters_count: cached.get("totalClustersCount").and_then(Value::as_i64),
            cached_selection: true,
        };
    }
    let selection = select_by_maturity(
        &in_scope,
        params.maturity.expect("maturity present"),
        params.cluster_gap_minutes * 60,
    );
    let payload = json!({
        "identities": selection.rows.iter().map(identity_of).collect::<Vec<_>>(),
        "avgSelectionOrdinal": selection.avg_selection_ordinal,
        "avgSelectionDelayMinutes": selection.avg_selection_delay_minutes,
        "totalClustersCount": selection.total_clusters_count,
    });
    let _ = save_selection(
        pool,
        &scope_key,
        params.maturity_tenths,
        params.cluster_gap_minutes,
        &params.cache_time_key,
        &payload,
    )
    .await;
    SelectionResult {
        rows: selection.rows,
        avg_selection_ordinal: selection.avg_selection_ordinal,
        avg_selection_delay_minutes: selection.avg_selection_delay_minutes,
        total_clusters_count: Some(selection.total_clusters_count as i64),
        cached_selection: false,
    }
}

async fn compute_scope(
    state: &AnalyticsState,
    setups: &[AnalyticsSetup],
    params: &ParsedAnalyticsParams,
) -> setups_db::analytics::SetScope {
    let (count, first, last) = setups_fingerprint(setups);
    let mut last_candle = -1i64;
    if let Some(last_setup) = setups.last()
        && let Some(instrument_id) = state
            .reader
            .instrument_id(
                &last_setup.exchange_code,
                &last_setup.category,
                &last_setup.symbol,
            )
            .await
    {
        let from = last_setup.open_time_unix + params.time_shift_minutes * 60;
        let to = from + 60 + params.horizon_minutes * 60;
        let candles = state
            .reader
            .read_window_lossy(instrument_id, from, to)
            .await;
        if let Some(last) = candles.last() {
            last_candle = last.open_time_unix;
        }
    }
    setups_db::analytics::SetScope {
        set_kind: params.set_ref.kind.as_str().to_string(),
        set_id: params.set_ref.id,
        setup_count: count,
        first_setup_unix: first,
        last_setup_unix: last,
        last_setup_last_candle_open_time_unix: last_candle,
    }
}

fn no_data_evaluation() -> LimitEvaluation {
    LimitEvaluation {
        status: LimitStatus::NoData,
        entry_price: None,
        entry_time_unix: None,
        tp_price: None,
        sl_price: None,
        close_time_unix: None,
        move_pct: None,
        close_minutes: None,
    }
}

async fn evaluate_rows(
    state: &AnalyticsState,
    rows: &[AnalyticsSetup],
    params: &ParsedAnalyticsParams,
) -> (Vec<LimitEvaluation>, usize, usize) {
    let mut by_instrument: HashMap<String, Vec<&AnalyticsSetup>> = HashMap::new();
    for row in rows {
        by_instrument
            .entry(format!(
                "{}\0{}\0{}",
                row.exchange_code, row.category, row.symbol
            ))
            .or_default()
            .push(row);
    }
    let mut evaluations: HashMap<String, LimitEvaluation> = HashMap::new();
    let mut eligible_count = 0usize;
    let mut filtered_count = 0usize;
    for group in by_instrument.values() {
        let Some(first) = group.first() else {
            continue;
        };
        let Some(instrument_id) = state
            .reader
            .instrument_id(&first.exchange_code, &first.category, &first.symbol)
            .await
        else {
            for setup in group {
                evaluations.insert(identity_of(setup), no_data_evaluation());
            }
            continue;
        };
        let min_open = group
            .iter()
            .map(|setup| setup.open_time_unix)
            .min()
            .unwrap_or(i64::MAX);
        let max_open = group
            .iter()
            .map(|setup| setup.open_time_unix)
            .max()
            .unwrap_or(0);
        let window = state
            .reader
            .read_window_lossy(
                instrument_id,
                min_open,
                max_open + params.time_shift_minutes * 60 + 60 + params.horizon_minutes * 60,
            )
            .await;
        for setup in group {
            let Some((entry_price, entry_candle)) =
                resolve_entry(&window, setup, params.time_shift_minutes)
            else {
                evaluations.insert(identity_of(setup), no_data_evaluation());
                continue;
            };
            let close_price: f64 = setup.close_price.parse().unwrap_or(f64::NAN);
            if let Some(price_change_pct) = params.price_change_pct
                && !passes_price_change(
                    close_price,
                    entry_price,
                    params.direction,
                    price_change_pct,
                )
            {
                filtered_count += 1;
                evaluations.insert(
                    identity_of(setup),
                    LimitEvaluation {
                        status: LimitStatus::PriceChangeFiltered,
                        entry_price: Some(entry_price),
                        entry_time_unix: Some(entry_candle.open_time_unix + 60),
                        tp_price: None,
                        sl_price: None,
                        close_time_unix: None,
                        move_pct: None,
                        close_minutes: None,
                    },
                );
                continue;
            }
            eligible_count += 1;
            evaluations.insert(
                identity_of(setup),
                evaluate_limit(EvaluateLimitOptions {
                    candles: &window,
                    setup,
                    direction: params.direction,
                    tp_pct: params.tp_pct,
                    sl_pct: params.sl_pct,
                    time_shift_minutes: params.time_shift_minutes,
                    horizon_minutes: params.horizon_minutes,
                }),
            );
        }
    }
    let ordered = rows
        .iter()
        .filter_map(|setup| evaluations.get(&identity_of(setup)).copied())
        .collect();
    (ordered, eligible_count, filtered_count)
}

fn aggregate_payload(
    params: &ParsedAnalyticsParams,
    selection: &SelectionResult,
    total_matching: usize,
    eligible_count: usize,
    filtered_count: usize,
    evaluations: &[LimitEvaluation],
) -> CachedPayload {
    let aggregate = aggregate_limit_outcomes(
        &evaluations
            .iter()
            .filter(|evaluation| evaluation.status != LimitStatus::PriceChangeFiltered)
            .copied()
            .collect::<Vec<_>>(),
        params.sl_pct,
    );
    let mut payload = Map::new();
    payload.insert("tpPct".into(), json!(params.tp_pct));
    payload.insert("slPct".into(), json!(params.sl_pct));
    payload.insert("direction".into(), json!(params.direction_str()));
    payload.insert(
        "month".into(),
        match &params.time_filter {
            TimeFilter::Month(key) => json!(key),
            _ => Value::Null,
        },
    );
    payload.insert("setupMaturity".into(), json!(params.maturity));
    payload.insert("timeShiftMinutes".into(), json!(params.time_shift_minutes));
    payload.insert("horizonMinutes".into(), json!(params.horizon_minutes));
    payload.insert("priceChangePct".into(), json!(params.price_change_pct));
    payload.insert("totalMatchingSetups".into(), json!(total_matching));
    payload.insert("filteredSetupsCount".into(), json!(selection.rows.len()));
    payload.insert("eligibleSetupsCount".into(), json!(eligible_count));
    payload.insert("priceChangeFilteredCount".into(), json!(filtered_count));
    payload.insert(
        "avgSelectionOrdinal".into(),
        json!(selection.avg_selection_ordinal),
    );
    payload.insert(
        "avgSelectionDelayMinutes".into(),
        json!(selection.avg_selection_delay_minutes),
    );
    payload.insert(
        "totalClustersCount".into(),
        json!(selection.total_clusters_count),
    );
    payload.insert("closedCount".into(), json!(aggregate.closed_count));
    payload.insert("tpCount".into(), json!(aggregate.tp_count));
    payload.insert("slCount".into(), json!(aggregate.sl_count));
    payload.insert("unclosedCount".into(), json!(aggregate.unclosed_count));
    payload.insert("noDataCount".into(), json!(aggregate.no_data_count));
    payload.insert("avgMovePct".into(), json!(aggregate.avg_move_pct));
    payload.insert("medianMovePct".into(), json!(aggregate.median_move_pct));
    payload.insert("winRatePct".into(), json!(aggregate.win_rate_pct));
    payload.insert("avgCloseMinutes".into(), json!(aggregate.avg_close_minutes));
    payload.insert(
        "medianCloseMinutes".into(),
        json!(aggregate.median_close_minutes),
    );
    payload.insert("riskExpectancy".into(), json!(aggregate.risk_expectancy));
    Value::Object(payload)
}

fn setup_dto(setup: &AnalyticsSetup) -> Value {
    json!({
        "exchange": setup.exchange_code,
        "category": setup.category,
        "symbol": setup.symbol,
        "openTimeUnix": setup.open_time_unix,
        "closeTimeUnix": setup.close_time_unix,
        "closePrice": setup.close_price,
    })
}

fn set_dto(info: &setups_db::analytics::AnalyticsSetInfo) -> Value {
    json!({
        "setId": info.set_id,
        "kind": info.kind.as_str(),
        "numericId": info.numeric_id,
        "label": info.label,
        "algorithmId": info.algorithm_id,
        "algorithmVersion": info.algorithm_version,
        "params": info.params,
        "exchange": info.exchange,
        "category": info.category,
        "symbol": info.symbol,
        "setupCount": info.setup_count,
        "fromUnix": info.from_unix,
        "toUnix": info.to_unix,
        "createdAt": info.created_at,
    })
}

fn raw_query(params: &HashMap<String, String>) -> RawAnalyticsQuery {
    RawAnalyticsQuery {
        set: params.get("set").cloned(),
        direction: params.get("direction").cloned(),
        month: params.get("month").cloned(),
        from_day: params.get("fromDay").cloned(),
        to_day: params.get("toDay").cloned(),
        setup_maturity: params.get("setupMaturity").cloned(),
        cluster_gap_hours: params.get("clusterGapHours").cloned(),
        tp_pct: params.get("tpPct").cloned(),
        sl_pct: params.get("slPct").cloned(),
        time_shift_minutes: params.get("timeShiftMinutes").cloned(),
        horizon_minutes: params.get("horizonMinutes").cloned(),
        price_change_pct: params.get("priceChangePct").cloned(),
        exchange: params.get("exchange").cloned(),
        category: params.get("category").cloned(),
    }
}

async fn load_set(
    state: &AnalyticsState,
    parsed: Result<ParsedAnalyticsParams, String>,
) -> Result<(ParsedAnalyticsParams, Vec<AnalyticsSetup>), Box<Response>> {
    let params =
        parsed.map_err(|message| Box::new(error_response(StatusCode::BAD_REQUEST, &message)))?;
    let rows = load_analytics_setups(&state.pool, &params.set_ref)
        .await
        .map_err(|_| Box::new(internal_error()))?;
    let Some(rows) = rows else {
        return Err(Box::new(error_response(
            StatusCode::NOT_FOUND,
            "set not found",
        )));
    };
    Ok((params, to_engine_setups(&rows)))
}

async fn analytics_sets(State(state): State<Arc<AnalyticsState>>, headers: HeaderMap) -> Response {
    if let Err(response) = guard_auth(&state.auth, &headers, false) {
        return response;
    }
    match list_analytics_sets(&state.pool).await {
        Ok(sets) => {
            Json(json!({ "sets": sets.iter().map(set_dto).collect::<Vec<_>>() })).into_response()
        }
        Err(_) => internal_error(),
    }
}

async fn analytics_months(
    State(state): State<Arc<AnalyticsState>>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    if let Err(response) = guard_auth(&state.auth, &headers, false) {
        return response;
    }
    let Some(set_ref) = parse_set_id(params.get("set").map(String::as_str).unwrap_or_default())
    else {
        return error_response(StatusCode::BAD_REQUEST, "invalid set id");
    };
    let rows = match load_analytics_setups(&state.pool, &set_ref).await {
        Ok(Some(rows)) => rows,
        Ok(None) => return error_response(StatusCode::NOT_FOUND, "set not found"),
        Err(_) => return internal_error(),
    };
    let setups = to_engine_setups(&rows);
    Json(json!({ "months": setup_months(&setups) })).into_response()
}

async fn analytics_setups(
    State(state): State<Arc<AnalyticsState>>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    if let Err(response) = guard_auth(&state.auth, &headers, false) {
        return response;
    }
    let raw = raw_query(&params);
    let parsed = parse_analytics_query(&raw, false);
    let (params, setups) = match load_set(&state, parsed).await {
        Ok(value) => value,
        Err(response) => return *response,
    };
    let in_scope = filter_setups_by_time(&setups, &params.time_filter);
    let selection = select_setup_rows(&state.pool, &setups, &params).await;
    Json(json!({
        "setups": selection.rows.iter().map(setup_dto).collect::<Vec<_>>(),
        "totalMatchingSetups": in_scope.len(),
        "avgSelectionOrdinal": selection.avg_selection_ordinal,
        "avgSelectionDelayMinutes": selection.avg_selection_delay_minutes,
        "totalClustersCount": selection.total_clusters_count,
        "cachedSelection": selection.cached_selection,
    }))
    .into_response()
}

async fn analytics_matrix_cells(
    State(state): State<Arc<AnalyticsState>>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    if let Err(response) = guard_auth(&state.auth, &headers, false) {
        return response;
    }
    let raw = raw_query(&params);
    let parsed = parse_analytics_query(&raw, false);
    let (params, setups) = match load_set(&state, parsed).await {
        Ok(value) => value,
        Err(response) => return *response,
    };
    let in_scope = filter_setups_by_time(&setups, &params.time_filter);
    let selection = select_setup_rows(&state.pool, &setups, &params).await;
    let scope = compute_scope(&state, &setups, &params).await;
    let rows = list_matrix_cells(
        &state.pool,
        &matrix_scope_key(&scope),
        &params.compute,
        &params.cache_time_key,
    )
    .await
    .unwrap_or_default();
    Json(json!({
        "items": rows.iter().map(|row| json!({
            "tpPct": row.tp_pct,
            "slPct": row.sl_pct,
            "avgMovePct": row.payload.get("avgMovePct").filter(|value| !value.is_null()).and_then(Value::as_f64),
            "riskExpectancy": row.payload.get("riskExpectancy").filter(|value| !value.is_null()).and_then(Value::as_f64),
        })).collect::<Vec<_>>(),
        "totalMatchingSetups": in_scope.len(),
        "filteredSetupsCount": selection.rows.len(),
        "avgSelectionOrdinal": selection.avg_selection_ordinal,
        "avgSelectionDelayMinutes": selection.avg_selection_delay_minutes,
    }))
    .into_response()
}

async fn analytics_matrix_cell(
    State(state): State<Arc<AnalyticsState>>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    if let Err(response) = guard_auth(&state.auth, &headers, false) {
        return response;
    }
    let raw = raw_query(&params);
    let parsed = parse_analytics_query(&raw, true);
    let (params, setups) = match load_set(&state, parsed).await {
        Ok(value) => value,
        Err(response) => return *response,
    };
    if setups.len() > MAX_SETUPS_PER_COMPUTE {
        return error_response(
            StatusCode::BAD_REQUEST,
            &format!(
                "set is too large for one computation ({} setups)",
                setups.len()
            ),
        );
    }
    let in_scope = filter_setups_by_time(&setups, &params.time_filter);
    let selection = select_setup_rows(&state.pool, &setups, &params).await;
    let scope = compute_scope(&state, &setups, &params).await;
    let scope_key = matrix_scope_key(&scope);
    if !setups.is_empty()
        && let Ok(Some(cached)) = get_matrix_cell(
            &state.pool,
            &scope_key,
            &params.compute,
            &params.cache_time_key,
        )
        .await
    {
        let mut response = cached;
        response["cached"] = json!(true);
        response["cachedSelection"] = json!(selection.cached_selection);
        return Json(response).into_response();
    }
    let (evaluations, eligible_count, filtered_count) =
        evaluate_rows(&state, &selection.rows, &params).await;
    let payload = aggregate_payload(
        &params,
        &selection,
        in_scope.len(),
        eligible_count,
        filtered_count,
        &evaluations,
    );
    if !setups.is_empty() {
        let _ = save_matrix_cell(
            &state.pool,
            &scope_key,
            &params.compute,
            &params.cache_time_key,
            &payload,
        )
        .await;
    }
    let mut response = payload;
    response["cached"] = json!(false);
    response["cachedSelection"] = json!(selection.cached_selection);
    Json(response).into_response()
}

async fn analytics_trades(
    State(state): State<Arc<AnalyticsState>>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    if let Err(response) = guard_auth(&state.auth, &headers, false) {
        return response;
    }
    let raw = raw_query(&params);
    let parsed = parse_analytics_query(&raw, true);
    let (params, setups) = match load_set(&state, parsed).await {
        Ok(value) => value,
        Err(response) => return *response,
    };
    let scope = compute_scope(&state, &setups, &params).await;
    let rows = list_trade_rows(&state.pool, &matrix_scope_key(&scope), &params.compute)
        .await
        .unwrap_or_default();
    let setups_by_identity: HashMap<String, &AnalyticsSetup> = setups
        .iter()
        .map(|setup| (identity_of(setup), setup))
        .collect();
    let items: Vec<Value> = rows
        .iter()
        .filter_map(|row| {
            let setup = setups_by_identity.get(&identity_of(&AnalyticsSetup {
                exchange_code: row.identity.exchange_code.clone(),
                category: row.identity.category.clone(),
                symbol: row.identity.symbol.clone(),
                open_time_unix: 0,
                close_time_unix: row.identity.close_time_unix,
                close_price: String::new(),
            }))?;
            Some(json!({
                "setup": setup_dto(setup),
                "direction": params.direction_str(),
                "tpPct": params.tp_pct,
                "slPct": params.sl_pct,
                "status": row.payload.get("status").and_then(Value::as_str).unwrap_or("no_data"),
                "entryPrice": nullable_number(&row.payload, "entryPrice"),
                "tpPrice": nullable_number(&row.payload, "tpPrice"),
                "slPrice": nullable_number(&row.payload, "slPrice"),
                "closeTimeUnix": nullable_number(&row.payload, "closeTimeUnix"),
                "riskReturn": nullable_number(&row.payload, "riskReturn"),
            }))
        })
        .collect();
    let cached_count = items.len();
    Json(json!({ "items": items, "cachedCount": cached_count })).into_response()
}

fn nullable_number(payload: &Value, key: &str) -> Value {
    payload
        .get(key)
        .filter(|value| !value.is_null())
        .cloned()
        .unwrap_or(Value::Null)
}

async fn analytics_trade_detail(
    State(state): State<Arc<AnalyticsState>>,
    headers: HeaderMap,
    Path((symbol, close_time_unix)): Path<(String, i64)>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    if let Err(response) = guard_auth(&state.auth, &headers, false) {
        return response;
    }
    if symbol.is_empty() || symbol.len() > 64 || close_time_unix < 0 {
        return error_response(StatusCode::BAD_REQUEST, "invalid_query");
    }
    let raw = raw_query(&params);
    let parsed = parse_analytics_query(&raw, true);
    let (params, setups) = match load_set(&state, parsed).await {
        Ok(value) => value,
        Err(response) => return *response,
    };
    let setup = setups.iter().find(|candidate| {
        candidate.symbol == symbol
            && candidate.close_time_unix == close_time_unix
            && raw
                .exchange
                .as_ref()
                .is_none_or(|value| candidate.exchange_code == *value)
            && raw
                .category
                .as_ref()
                .is_none_or(|value| candidate.category == *value)
    });
    let Some(setup) = setup else {
        return error_response(StatusCode::NOT_FOUND, "setup not found in set");
    };
    let setup_dto_value = setup_dto(setup);
    let scope = compute_scope(&state, &setups, &params).await;
    let scope_key = matrix_scope_key(&scope);
    if !setups.is_empty()
        && let Ok(Some(cached)) = get_trade_row(&state.pool, &scope_key, &params.compute, &{
            setups_db::analytics::TradeSetupIdentity {
                exchange_code: setup.exchange_code.clone(),
                category: setup.category.clone(),
                symbol: setup.symbol.clone(),
                close_time_unix: setup.close_time_unix,
            }
        })
        .await
    {
        let mut response = Map::new();
        response.insert("setup".into(), setup_dto_value);
        response.insert("direction".into(), json!(params.direction_str()));
        response.insert("tpPct".into(), json!(params.tp_pct));
        response.insert("slPct".into(), json!(params.sl_pct));
        for key in [
            "status",
            "entryPrice",
            "tpPrice",
            "slPrice",
            "closeTimeUnix",
            "riskReturn",
        ] {
            response.insert(key.into(), nullable_number(&cached, key));
        }
        response.insert("cached".into(), json!(true));
        return Json(Value::Object(response)).into_response();
    }
    let instrument_id = state
        .reader
        .instrument_id(&setup.exchange_code, &setup.category, &setup.symbol)
        .await;
    let result: Value = match instrument_id {
        None => json!({
            "status": "no_data", "entryPrice": null, "tpPrice": null,
            "slPrice": null, "closeTimeUnix": null, "riskReturn": null,
        }),
        Some(instrument_id) => {
            let window = state
                .reader
                .read_window_lossy(
                    instrument_id,
                    setup.open_time_unix,
                    setup.open_time_unix
                        + params.time_shift_minutes * 60
                        + 60
                        + params.horizon_minutes * 60,
                )
                .await;
            match resolve_entry(&window, setup, params.time_shift_minutes) {
                None => json!({
                    "status": "no_data", "entryPrice": null, "tpPrice": null,
                    "slPrice": null, "closeTimeUnix": null, "riskReturn": null,
                }),
                Some((entry_price, _entry_candle)) => {
                    let close_price: f64 = setup.close_price.parse().unwrap_or(f64::NAN);
                    if let Some(price_change_pct) = params.price_change_pct
                        && !passes_price_change(
                            close_price,
                            entry_price,
                            params.direction,
                            price_change_pct,
                        )
                    {
                        json!({
                            "status": "price_change_filtered", "entryPrice": entry_price,
                            "tpPrice": null, "slPrice": null, "closeTimeUnix": null, "riskReturn": null,
                        })
                    } else {
                        let evaluation = evaluate_limit(EvaluateLimitOptions {
                            candles: &window,
                            setup,
                            direction: params.direction,
                            tp_pct: params.tp_pct,
                            sl_pct: params.sl_pct,
                            time_shift_minutes: params.time_shift_minutes,
                            horizon_minutes: params.horizon_minutes,
                        });
                        json!({
                            "status": evaluation.status,
                            "entryPrice": evaluation.entry_price,
                            "tpPrice": evaluation.tp_price,
                            "slPrice": evaluation.sl_price,
                            "closeTimeUnix": evaluation.close_time_unix,
                            "riskReturn": risk_return(evaluation.status, params.tp_pct, params.sl_pct),
                        })
                    }
                }
            }
        }
    };
    let payload = json!({
        "status": result.get("status").cloned().unwrap_or(Value::Null),
        "entryPrice": result.get("entryPrice").cloned().unwrap_or(Value::Null),
        "tpPrice": result.get("tpPrice").cloned().unwrap_or(Value::Null),
        "slPrice": result.get("slPrice").cloned().unwrap_or(Value::Null),
        "closeTimeUnix": result.get("closeTimeUnix").cloned().unwrap_or(Value::Null),
        "riskReturn": result.get("riskReturn").cloned().unwrap_or(Value::Null),
    });
    if !setups.is_empty() {
        let identity = setups_db::analytics::TradeSetupIdentity {
            exchange_code: setup.exchange_code.clone(),
            category: setup.category.clone(),
            symbol: setup.symbol.clone(),
            close_time_unix: setup.close_time_unix,
        };
        let _ = save_trade_row(
            &state.pool,
            &scope_key,
            &params.compute,
            &identity,
            &payload,
        )
        .await;
    }
    let mut response = payload;
    response["setup"] = setup_dto_value;
    response["direction"] = json!(params.direction_str());
    response["tpPct"] = json!(params.tp_pct);
    response["slPct"] = json!(params.sl_pct);
    response["cached"] = json!(false);
    Json(response).into_response()
}

async fn analytics_cache_summary_handler(
    State(state): State<Arc<AnalyticsState>>,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = guard_auth(&state.auth, &headers, false) {
        return response;
    }
    match analytics_cache_summary(&state.pool).await {
        Ok(tables) => {
            let total: i64 = tables.iter().map(|table| table.count).sum();
            Json(json!({
                "tables": tables.iter().map(|table| json!({"name": table.name, "count": table.count})).collect::<Vec<_>>(),
                "total": total,
            }))
            .into_response()
        }
        Err(_) => internal_error(),
    }
}

async fn analytics_cache_clear(
    State(state): State<Arc<AnalyticsState>>,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = guard_auth(&state.auth, &headers, true) {
        return response;
    }
    match clear_analytics_cache(&state.pool).await {
        Ok(deleted) => {
            let total: i64 = deleted.iter().map(|table| table.count).sum();
            Json(json!({
                "deleted": deleted.iter().map(|table| json!({"name": table.name, "count": table.count})).collect::<Vec<_>>(),
                "total": total,
            }))
            .into_response()
        }
        Err(_) => internal_error(),
    }
}

pub fn analytics_router(state: Arc<AnalyticsState>) -> Router {
    Router::new()
        .route("/v1/analytics/sets", get(analytics_sets))
        .route("/v1/analytics/months", get(analytics_months))
        .route("/v1/analytics/setups", get(analytics_setups))
        .route("/v1/analytics/matrix-cells", get(analytics_matrix_cells))
        .route("/v1/analytics/matrix-cell", get(analytics_matrix_cell))
        .route("/v1/analytics/trades", get(analytics_trades))
        .route(
            "/v1/analytics/trades/{symbol}/{closeTimeUnix}",
            get(analytics_trade_detail),
        )
        .route(
            "/v1/analytics/cache/summary",
            get(analytics_cache_summary_handler),
        )
        .route("/v1/analytics/cache", delete(analytics_cache_clear))
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(set: &str) -> RawAnalyticsQuery {
        RawAnalyticsQuery {
            set: Some(set.to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn parse_requires_valid_set_id() {
        assert!(parse_analytics_query(&raw("batch-1"), false).is_ok());
        let error = parse_analytics_query(&raw("nope"), false).unwrap_err();
        assert_eq!(error, "invalid set id");
    }

    #[test]
    fn parse_enforces_tp_sl_for_matrix_cell() {
        let mut query = raw("run-2");
        query.tp_pct = Some("1.0".into());
        assert_eq!(
            parse_analytics_query(&query, true).unwrap_err(),
            "tpPct and slPct are required"
        );
        query.sl_pct = Some("0.5".into());
        assert!(parse_analytics_query(&query, true).is_ok());
    }

    #[test]
    fn parse_maturity_step_and_range() {
        let mut query = raw("run-2");
        query.setup_maturity = Some("0.35".into());
        assert_eq!(
            parse_analytics_query(&query, false).unwrap_err(),
            "setupMaturity must be 0.0..1.0 with step 0.1"
        );
        query.setup_maturity = Some("0.7".into());
        let parsed = parse_analytics_query(&query, false).unwrap();
        assert_eq!(parsed.maturity_tenths, 7);
        assert_eq!(parsed.cluster_gap_minutes, 480);
        query.setup_maturity = Some("1.2".into());
        assert!(parse_analytics_query(&query, false).is_err());
    }

    #[test]
    fn parse_time_keys_and_cache_dimension() {
        let mut query = raw("batch-3");
        query.month = Some("2025-03".into());
        let parsed = parse_analytics_query(&query, false).unwrap();
        assert_eq!(parsed.cache_time_key, "2025-03");
        query.month = None;
        query.from_day = Some("2025-03-01".into());
        query.to_day = Some("2025-03-31".into());
        let parsed = parse_analytics_query(&query, false).unwrap();
        assert_eq!(parsed.cache_time_key, "d:2025-03-01:2025-03-31");
        query.from_day = Some("2025-03-31".into());
        query.to_day = Some("2025-03-30".into());
        assert!(parse_analytics_query(&query, false).is_err());
    }
}
