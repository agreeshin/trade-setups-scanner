use serde::{Deserialize, Serialize};
use std::fmt;

pub const SETUP_CLUSTER_MAX_GAP_SECONDS: i64 = 8 * 60 * 60;
pub const DEFAULT_CLUSTER_GAP_HOURS: i64 = 8;
pub const DEFAULT_HORIZON_MINUTES: i64 = 10_080;
pub const MINUTE_SECONDS: i64 = 60;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AnalyticsSetup {
    pub exchange_code: String,
    pub category: String,
    pub symbol: String,
    pub open_time_unix: i64,
    pub close_time_unix: i64,
    pub close_price: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OhlcCandle {
    pub open_time_unix: i64,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LimitStatus {
    Tp,
    Sl,
    Unclosed,
    NoData,
    PriceChangeFiltered,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LimitEvaluation {
    pub status: LimitStatus,
    pub entry_price: Option<f64>,
    pub entry_time_unix: Option<i64>,
    pub tp_price: Option<f64>,
    pub sl_price: Option<f64>,
    pub close_time_unix: Option<i64>,
    pub move_pct: Option<f64>,
    pub close_minutes: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LimitAggregate {
    pub closed_count: usize,
    pub tp_count: usize,
    pub sl_count: usize,
    pub unclosed_count: usize,
    pub no_data_count: usize,
    pub avg_move_pct: Option<f64>,
    pub median_move_pct: Option<f64>,
    pub win_rate_pct: Option<f64>,
    pub avg_close_minutes: Option<f64>,
    pub median_close_minutes: Option<f64>,
    pub risk_expectancy: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MaturitySelection {
    pub rows: Vec<AnalyticsSetup>,
    pub avg_selection_ordinal: Option<f64>,
    pub avg_selection_delay_minutes: Option<f64>,
    pub total_clusters_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineError {
    InvalidMonthKey(String),
    InvalidDayRange(String, String),
}

impl fmt::Display for EngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EngineError::InvalidMonthKey(key) => write!(f, "invalid month key: {key}"),
            EngineError::InvalidDayRange(from, to) => write!(f, "invalid day range: {from}..{to}"),
        }
    }
}

impl std::error::Error for EngineError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TradeDirection {
    Short,
    Long,
}

fn mean(values: &[f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    Some(values.iter().fold(0.0, |acc, value| acc + value) / values.len() as f64)
}

fn median(values: &[f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).expect("no NaN in analytics inputs"));
    let middle = sorted.len() / 2;
    let at = sorted[middle];
    if sorted.len() % 2 == 1 {
        Some(at)
    } else {
        let before = sorted[middle - 1];
        Some((before + at) / 2.0)
    }
}

pub fn setup_instrument_key(setup: &AnalyticsSetup) -> String {
    format!(
        "{}\u{0}{}\u{0}{}",
        setup.exchange_code, setup.category, setup.symbol
    )
}

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let m = month;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

pub fn month_key_of(unix_seconds: i64) -> String {
    let (year, month, _) = civil_from_days(unix_seconds.div_euclid(86_400));
    format!("{year:04}-{month:02}")
}

pub fn month_key_to_start_unix(month_key: &str) -> Result<i64, EngineError> {
    let error = || EngineError::InvalidMonthKey(month_key.to_string());
    let (year_str, month_str) = month_key.split_once('-').ok_or_else(error)?;
    let year: i64 = year_str.parse().map_err(|_| error())?;
    let month: i64 = month_str.parse().map_err(|_| error())?;
    if !(1..=12).contains(&month) {
        return Err(error());
    }
    Ok(days_from_civil(year, month, 1) * 86_400)
}

pub fn setup_months(setups: &[AnalyticsSetup]) -> Vec<String> {
    let mut months: Vec<String> = Vec::new();
    for setup in setups {
        let key = month_key_of(setup.close_time_unix);
        if !months.contains(&key) {
            months.push(key);
        }
    }
    months.sort();
    months
}

fn next_month_key(month_key: &str) -> Result<String, EngineError> {
    let error = || EngineError::InvalidMonthKey(month_key.to_string());
    let (year_str, month_str) = month_key.split_once('-').ok_or_else(error)?;
    let mut year: i64 = year_str.parse().map_err(|_| error())?;
    let mut month: i64 = month_str.parse().map_err(|_| error())?;
    if month == 12 {
        year += 1;
        month = 1;
    } else {
        month += 1;
    }
    Ok(format!("{year:04}-{month:02}"))
}

pub fn filter_by_month(
    setups: &[AnalyticsSetup],
    month_key: &str,
) -> Result<Vec<AnalyticsSetup>, EngineError> {
    let start = month_key_to_start_unix(month_key)?;
    let next_month = next_month_key(month_key)?;
    let next_start = month_key_to_start_unix(&next_month)?;
    Ok(setups
        .iter()
        .filter(|setup| setup.close_time_unix >= start && setup.close_time_unix < next_start)
        .cloned()
        .collect())
}

pub fn day_range_to_unix_bounds(from_day: &str, to_day: &str) -> Option<(i64, i64)> {
    let day_to_unix = |day: &str| -> Option<i64> {
        let pattern_ok = day.len() == 10 && day.as_bytes()[4] == b'-' && day.as_bytes()[7] == b'-';
        if !pattern_ok {
            return None;
        }
        let year: i64 = day[0..4].parse().ok()?;
        let month: i64 = day[5..7].parse().ok()?;
        let dom: i64 = day[8..10].parse().ok()?;
        if !(1..=12).contains(&month) || !(1..=31).contains(&dom) {
            return None;
        }
        let days = days_from_civil(year, month, dom);
        let (y2, m2, d2) = civil_from_days(days);
        if (y2, m2, d2) != (year, month, dom) {
            return None;
        }
        Some(days * 86_400)
    };
    let from_unix = day_to_unix(from_day)?;
    let to_unix = day_to_unix(to_day)? + 86_399;
    Some((from_unix, to_unix))
}

pub fn filter_by_day_range(
    setups: &[AnalyticsSetup],
    from_day: &str,
    to_day: &str,
) -> Result<Vec<AnalyticsSetup>, EngineError> {
    match day_range_to_unix_bounds(from_day, to_day) {
        Some((from_unix, to_unix)) => Ok(setups
            .iter()
            .filter(|setup| setup.close_time_unix >= from_unix && setup.close_time_unix <= to_unix)
            .cloned()
            .collect()),
        None => Err(EngineError::InvalidDayRange(
            from_day.to_string(),
            to_day.to_string(),
        )),
    }
}

fn iter_instrument_clusters(
    sorted_setups: &[AnalyticsSetup],
    max_gap_seconds: i64,
) -> Vec<Vec<AnalyticsSetup>> {
    let mut clusters: Vec<Vec<AnalyticsSetup>> = Vec::new();
    let mut current: Vec<AnalyticsSetup> = Vec::new();
    let mut previous_time: Option<i64> = None;
    for setup in sorted_setups {
        if let Some(previous) = previous_time
            && setup.close_time_unix - previous > max_gap_seconds
        {
            if !current.is_empty() {
                clusters.push(current.clone());
            }
            current.clear();
        }
        current.push(setup.clone());
        previous_time = Some(setup.close_time_unix);
    }
    if !current.is_empty() {
        clusters.push(current);
    }
    clusters
}

pub fn iter_setup_clusters(
    setups: &[AnalyticsSetup],
    max_gap_seconds: i64,
) -> Vec<Vec<AnalyticsSetup>> {
    let mut order: Vec<String> = Vec::new();
    let mut by_instrument: Vec<Vec<AnalyticsSetup>> = Vec::new();
    let mut index_of: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for setup in setups {
        let key = setup_instrument_key(setup);
        match index_of.get(&key) {
            Some(&index) => by_instrument[index].push(setup.clone()),
            None => {
                index_of.insert(key.clone(), order.len());
                order.push(key);
                by_instrument.push(vec![setup.clone()]);
            }
        }
    }
    let mut clusters: Vec<Vec<AnalyticsSetup>> = Vec::new();
    for mut list in by_instrument {
        list.sort_by_key(|setup| setup.close_time_unix);
        clusters.extend(iter_instrument_clusters(&list, max_gap_seconds));
    }
    clusters
}

fn pick_by_maturity(cluster: &[AnalyticsSetup], maturity: f64) -> (AnalyticsSetup, usize, i64) {
    let first = &cluster[0];
    let last = cluster.last().unwrap_or(first);
    let start = first.close_time_unix;
    let end = last.close_time_unix;
    let target = start as f64 + maturity * (end - start) as f64;
    let mut best = first.clone();
    let mut best_index = 0usize;
    let mut best_distance = (first.close_time_unix as f64 - target).abs();
    for (index, setup) in cluster.iter().enumerate().skip(1) {
        let distance = (setup.close_time_unix as f64 - target).abs();
        if distance < best_distance {
            best = setup.clone();
            best_index = index;
            best_distance = distance;
        }
    }
    let delay_seconds = best.close_time_unix - start;
    (best, best_index + 1, delay_seconds)
}

pub fn select_by_maturity(
    setups: &[AnalyticsSetup],
    maturity: f64,
    max_gap_seconds: i64,
) -> MaturitySelection {
    let mut selected: Vec<AnalyticsSetup> = Vec::new();
    let mut ordinals: Vec<f64> = Vec::new();
    let mut delays_seconds: Vec<f64> = Vec::new();
    let mut total_clusters_count = 0usize;
    for cluster in iter_setup_clusters(setups, max_gap_seconds) {
        total_clusters_count += 1;
        let (setup, ordinal, delay_seconds) = pick_by_maturity(&cluster, maturity);
        selected.push(setup);
        ordinals.push(ordinal as f64);
        delays_seconds.push(delay_seconds as f64);
    }
    let mut rows = selected;
    rows.sort_by_key(|setup| setup.close_time_unix);
    let delays_mean = mean(&delays_seconds);
    MaturitySelection {
        rows,
        avg_selection_ordinal: mean(&ordinals),
        avg_selection_delay_minutes: delays_mean.map(|value| value / 60.0),
        total_clusters_count,
    }
}

pub fn setup_clusters_count(setups: &[AnalyticsSetup], max_gap_seconds: i64) -> usize {
    iter_setup_clusters(setups, max_gap_seconds).len()
}

pub fn find_candle_by_open_time(
    candles: &[OhlcCandle],
    open_time_unix: i64,
) -> Option<&OhlcCandle> {
    let mut low = 0usize;
    let mut high = candles.len() as isize - 1;
    while low as isize <= high {
        let mid = ((low as isize + high) / 2) as usize;
        let candle = &candles[mid];
        if candle.open_time_unix == open_time_unix {
            return Some(candle);
        }
        if candle.open_time_unix < open_time_unix {
            low = mid + 1;
        } else {
            high = mid as isize - 1;
        }
    }
    None
}

pub fn resolve_entry<'a>(
    candles: &'a [OhlcCandle],
    setup: &AnalyticsSetup,
    time_shift_minutes: i64,
) -> Option<(f64, &'a OhlcCandle)> {
    let candle = find_candle_by_open_time(
        candles,
        setup.open_time_unix + time_shift_minutes * MINUTE_SECONDS,
    )?;
    Some((candle.close, candle))
}

pub fn passes_price_change(
    setup_close_price: f64,
    shifted_entry_price: f64,
    direction: TradeDirection,
    acceptable_price_change_pct: f64,
) -> bool {
    match direction {
        TradeDirection::Short => {
            shifted_entry_price >= setup_close_price * (1.0 + acceptable_price_change_pct / 100.0)
        }
        TradeDirection::Long => {
            shifted_entry_price <= setup_close_price * (1.0 - acceptable_price_change_pct / 100.0)
        }
    }
}

pub fn risk_return(status: LimitStatus, tp_pct: f64, sl_pct: f64) -> Option<f64> {
    match status {
        LimitStatus::Tp => Some(tp_pct / sl_pct),
        LimitStatus::Sl => Some(-1.0),
        _ => None,
    }
}

fn lower_bound(candles: &[OhlcCandle], target_unix: i64) -> usize {
    let mut low = 0usize;
    let mut high = candles.len();
    while low < high {
        let mid = (low + high) / 2;
        if candles[mid].open_time_unix < target_unix {
            low = mid + 1;
        } else {
            high = mid;
        }
    }
    low
}

pub fn evaluate_limit(options: EvaluateLimitOptions<'_>) -> LimitEvaluation {
    let EvaluateLimitOptions {
        candles,
        setup,
        direction,
        tp_pct,
        sl_pct,
        time_shift_minutes,
        horizon_minutes,
    } = options;
    let no_data = LimitEvaluation {
        status: LimitStatus::NoData,
        entry_price: None,
        entry_time_unix: None,
        tp_price: None,
        sl_price: None,
        close_time_unix: None,
        move_pct: None,
        close_minutes: None,
    };
    let Some((entry_price, entry_candle)) = resolve_entry(candles, setup, time_shift_minutes)
    else {
        return no_data;
    };
    let entry_time_unix = entry_candle.open_time_unix + MINUTE_SECONDS;
    let scan_start_unix = entry_time_unix;
    let scan_end_unix = entry_time_unix + horizon_minutes * MINUTE_SECONDS;
    let tp_price = match direction {
        TradeDirection::Short => entry_price * (1.0 - tp_pct / 100.0),
        TradeDirection::Long => entry_price * (1.0 + tp_pct / 100.0),
    };
    let sl_price = match direction {
        TradeDirection::Short => entry_price * (1.0 + sl_pct / 100.0),
        TradeDirection::Long => entry_price * (1.0 - sl_pct / 100.0),
    };
    let touches_tp = |candle: &OhlcCandle| match direction {
        TradeDirection::Short => candle.low <= tp_price,
        TradeDirection::Long => candle.high >= tp_price,
    };
    let touches_sl = |candle: &OhlcCandle| match direction {
        TradeDirection::Short => candle.high >= sl_price,
        TradeDirection::Long => candle.low <= sl_price,
    };
    let mut start = lower_bound(candles, scan_start_unix);
    while start < candles.len() {
        let candle = &candles[start];
        if candle.open_time_unix >= scan_end_unix {
            break;
        }
        let hit_sl = touches_sl(candle);
        if hit_sl || touches_tp(candle) {
            let close_time_unix = candle.open_time_unix;
            return LimitEvaluation {
                status: if hit_sl {
                    LimitStatus::Sl
                } else {
                    LimitStatus::Tp
                },
                entry_price: Some(entry_price),
                entry_time_unix: Some(entry_time_unix),
                tp_price: if hit_sl { None } else { Some(tp_price) },
                sl_price: if hit_sl { Some(sl_price) } else { None },
                close_time_unix: Some(close_time_unix),
                move_pct: Some(if hit_sl { -sl_pct } else { tp_pct }),
                close_minutes: Some((close_time_unix - entry_time_unix) as f64 / 60.0),
            };
        }
        start += 1;
    }
    LimitEvaluation {
        status: LimitStatus::Unclosed,
        entry_price: Some(entry_price),
        entry_time_unix: Some(entry_time_unix),
        tp_price: None,
        sl_price: None,
        close_time_unix: None,
        move_pct: None,
        close_minutes: None,
    }
}

pub struct EvaluateLimitOptions<'a> {
    pub candles: &'a [OhlcCandle],
    pub setup: &'a AnalyticsSetup,
    pub direction: TradeDirection,
    pub tp_pct: f64,
    pub sl_pct: f64,
    pub time_shift_minutes: i64,
    pub horizon_minutes: i64,
}

pub fn aggregate_limit_outcomes(evaluations: &[LimitEvaluation], sl_pct: f64) -> LimitAggregate {
    let tp: Vec<&LimitEvaluation> = evaluations
        .iter()
        .filter(|e| e.status == LimitStatus::Tp)
        .collect();
    let sl: Vec<&LimitEvaluation> = evaluations
        .iter()
        .filter(|e| e.status == LimitStatus::Sl)
        .collect();
    let unclosed = evaluations
        .iter()
        .filter(|e| e.status == LimitStatus::Unclosed)
        .count();
    let no_data = evaluations
        .iter()
        .filter(|e| e.status == LimitStatus::NoData)
        .count();
    let mut closed: Vec<&LimitEvaluation> = Vec::with_capacity(tp.len() + sl.len());
    closed.extend(&tp);
    closed.extend(&sl);
    let moves: Vec<f64> = closed.iter().filter_map(|e| e.move_pct).collect();
    let close_minutes: Vec<f64> = closed.iter().filter_map(|e| e.close_minutes).collect();
    let avg_move_pct = mean(&moves);
    LimitAggregate {
        closed_count: closed.len(),
        tp_count: tp.len(),
        sl_count: sl.len(),
        unclosed_count: unclosed,
        no_data_count: no_data,
        avg_move_pct,
        median_move_pct: median(&moves),
        win_rate_pct: if closed.is_empty() {
            None
        } else {
            Some(tp.len() as f64 / closed.len() as f64 * 100.0)
        },
        avg_close_minutes: mean(&close_minutes),
        median_close_minutes: median(&close_minutes),
        risk_expectancy: avg_move_pct.map(|value| value / sl_pct),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn month_key_matches_utc_calendar() {
        assert_eq!(month_key_of(1_700_000_000), "2023-11");
        assert_eq!(month_key_of(0), "1970-01");
        assert_eq!(month_key_to_start_unix("1970-01").unwrap(), 0);
        assert!(month_key_to_start_unix("2023-13").is_err());
        assert!(month_key_to_start_unix("bad").is_err());
    }

    #[test]
    fn day_range_bounds_and_validation() {
        assert_eq!(
            day_range_to_unix_bounds("2023-11-01", "2023-11-02"),
            Some((1_698_796_800, 1_698_969_599))
        );
        assert_eq!(day_range_to_unix_bounds("2023-02-30", "2023-03-01"), None);
        assert_eq!(day_range_to_unix_bounds("2023-11-1", "2023-11-02"), None);
    }

    #[test]
    fn double_touch_candle_counts_as_sl() {
        let setup = AnalyticsSetup {
            exchange_code: "bybit".into(),
            category: "linear".into(),
            symbol: "BTCUSDT".into(),
            open_time_unix: 60,
            close_time_unix: 120,
            close_price: "100".into(),
        };
        let candles = vec![
            OhlcCandle {
                open_time_unix: 60,
                open: 100.0,
                high: 100.0,
                low: 100.0,
                close: 100.0,
            },
            OhlcCandle {
                open_time_unix: 120,
                open: 100.0,
                high: 120.0,
                low: 80.0,
                close: 100.0,
            },
        ];
        let evaluation = evaluate_limit(EvaluateLimitOptions {
            candles: &candles,
            setup: &setup,
            direction: TradeDirection::Short,
            tp_pct: 10.0,
            sl_pct: 10.0,
            time_shift_minutes: 0,
            horizon_minutes: 60,
        });
        assert_eq!(evaluation.status, LimitStatus::Sl);
        assert_eq!(evaluation.move_pct, Some(-10.0));
    }
}
