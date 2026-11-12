use crate::frequency::FrequencyGate;
use crate::validation::{decimal, invalid, normalized_decimal, parse_params, validate_analytics};
use crate::*;
use serde_json::Value;
use setups_analytics::{
    AnalyticsSetup, TradeDirection, find_candle_by_open_time, passes_price_change,
};
use setups_detector::PriceVolumeSpikeDetector;
use setups_domain::decimal::{Decimal, add, sub};
use setups_domain::{AlgorithmParams, PushStatus};
use std::collections::BTreeSet;
use std::mem::size_of;
use std::time::Instant;

pub fn snapshot_bytes(input: &StudyInput) -> usize {
    let mut bytes = size_of::<StudyInput>()
        .saturating_add(
            input
                .instruments
                .capacity()
                .saturating_mul(size_of::<InstrumentInput>()),
        )
        .saturating_add(input.snapshot_hash.capacity())
        .saturating_add(input.captured_at.capacity());
    for instrument in &input.instruments {
        bytes = bytes
            .saturating_add(instrument.instrument.exchange.capacity())
            .saturating_add(instrument.instrument.category.capacity())
            .saturating_add(instrument.instrument.symbol.capacity())
            .saturating_add(
                instrument
                    .detector_bars
                    .capacity()
                    .saturating_mul(size_of::<setups_domain::DetectorBar>()),
            )
            .saturating_add(
                instrument
                    .candles
                    .capacity()
                    .saturating_mul(size_of::<setups_analytics::OhlcCandle>()),
            );
        for bar in &instrument.detector_bars {
            bytes = bytes
                .saturating_add(bar.close.capacity())
                .saturating_add(bar.quote_turnover.capacity());
        }
    }
    bytes
}

pub(crate) fn limit(resource: &str, requested: usize, maximum: usize) -> Result<(), StudyError> {
    if requested > maximum {
        Err(StudyError::ResourceLimit {
            resource: resource.into(),
            limit: maximum,
            requested,
        })
    } else {
        Ok(())
    }
}

pub(crate) struct InputStats {
    max_close: Decimal,
    max_turnover: Decimal,
}

fn update_max(max: &mut Decimal, next: Decimal) -> Result<(), StudyError> {
    let scale = max.scale.max(next.scale);
    let align = |value: Decimal| value.int.checked_mul(10i128.pow(scale - value.scale));
    let left =
        align(*max).ok_or_else(|| StudyError::InvalidInput("decimal alignment overflow".into()))?;
    let right =
        align(next).ok_or_else(|| StudyError::InvalidInput("decimal alignment overflow".into()))?;
    *max = Decimal {
        int: left.max(right),
        scale,
    };
    Ok(())
}

pub(crate) fn validate_input(
    spec: &StudyRequest,
    input: &StudyInput,
    limits: &StudyLimits,
    cancel: &CancelToken,
) -> Result<Option<InputStats>, StudyError> {
    limit(
        "snapshot_bytes",
        snapshot_bytes(input),
        limits.max_snapshot_bytes,
    )?;
    if input.snapshot_hash.is_empty()
        || input.snapshot_hash.len() > 256
        || input.captured_at.is_empty()
        || input.captured_at.len() > 64
    {
        return Err(StudyError::InvalidInput(
            "missing or oversized snapshot provenance".into(),
        ));
    }
    let expected: BTreeSet<_> = spec.instruments.iter().collect();
    let actual: BTreeSet<_> = input
        .instruments
        .iter()
        .map(|data| &data.instrument)
        .collect();
    if expected != actual || actual.len() != input.instruments.len() {
        return Err(StudyError::InvalidInput(
            "snapshot instruments differ from fixed scope".into(),
        ));
    }
    let mut stats = InputStats {
        max_close: Decimal { int: 0, scale: 0 },
        max_turnover: Decimal { int: 0, scale: 0 },
    };
    for data in &input.instruments {
        let mut previous = None;
        for (index, candle) in data.candles.iter().enumerate() {
            if index % 256 == 0 && cancel.is_cancelled() {
                return Ok(None);
            }
            if candle.open_time_unix % 60 != 0
                || previous.is_some_and(|time| time >= candle.open_time_unix)
                || [candle.open, candle.high, candle.low, candle.close]
                    .iter()
                    .any(|v| !v.is_finite() || *v <= 0.0 || *v > 1e38)
                || candle.low > candle.open.min(candle.close)
                || candle.high < candle.open.max(candle.close)
                || candle.low > candle.high
            {
                return Err(StudyError::InvalidInput(
                    "candles must be sorted unique aligned valid OHLC values".into(),
                ));
            }
            previous = Some(candle.open_time_unix);
        }
        previous = None;
        for (index, bar) in data.detector_bars.iter().enumerate() {
            if index % 256 == 0 && cancel.is_cancelled() {
                return Ok(None);
            }
            if bar.open_time_unix % 60 != 0
                || previous.is_some_and(|time| time >= bar.open_time_unix)
            {
                return Err(StudyError::InvalidInput(
                    "detector bars must be sorted unique aligned minutes".into(),
                ));
            }
            previous = Some(bar.open_time_unix);
            let close =
                decimal(&bar.close).map_err(|error| StudyError::InvalidInput(error.to_string()))?;
            let volume = decimal(&bar.quote_turnover)
                .map_err(|error| StudyError::InvalidInput(error.to_string()))?;
            if close.int <= 0 || volume.int < 0 {
                return Err(StudyError::InvalidInput(
                    "nonpositive close or negative turnover".into(),
                ));
            }
            update_max(&mut stats.max_close, close)?;
            update_max(&mut stats.max_turnover, volume)?;
            // Both arrays must represent the same frozen records where they
            // overlap; otherwise shift-zero equivalence is not valid.
            if let Some(candle) = find_candle_by_open_time(&data.candles, bar.open_time_unix)
                && bar.close.parse::<f64>().ok() != Some(candle.close)
            {
                return Err(StudyError::InvalidInput(
                    "detector and analytics close disagree".into(),
                ));
            }
        }
    }
    Ok(Some(stats))
}

pub(crate) struct Control<'a> {
    pub cancel: &'a CancelToken,
    pub deadline: Instant,
    pub detector_passes: &'a std::sync::atomic::AtomicUsize,
}
impl Control<'_> {
    pub fn stopped(&self) -> bool {
        self.cancel.is_cancelled() || Instant::now() >= self.deadline
    }
}

fn numerical_budget(params: &AlgorithmParams, stats: &InputStats) -> Result<(), StudyError> {
    let overflow = || {
        StudyError::Detector(
            "input/parameter combination exceeds exact i128 arithmetic budget".into(),
        )
    };
    let hundred = Decimal { int: 100, scale: 0 };
    let pct = decimal(params.price_pct())?;
    let bound = match params {
        AlgorithmParams::Spike(_) => add(hundred, pct),
        AlgorithmParams::SpikeDrop(_) => sub(hundred, pct),
    }
    .map_err(|_| overflow())?;
    let multiplier = decimal(params.volume_multiplier())?;
    let required = i128::from(params.baseline_long_days()) * 1440;
    let window = i128::from(params.window_minutes());
    let price_factor = 100i128
        .checked_mul(10i128.pow(bound.scale))
        .ok_or_else(overflow)?
        .max(bound.int);
    stats
        .max_close
        .int
        .checked_mul(price_factor)
        .ok_or_else(overflow)?;
    stats
        .max_close
        .int
        .checked_mul(2 * 100 * 10_000)
        .ok_or_else(overflow)?;
    // Bound sums before eviction as well as every cross-product/ratio used by
    // the unchanged detector. This prevents its internal expect() overflows.
    let volume = stats.max_turnover.int;
    let denominator = 10i128.pow(multiplier.scale);
    let long_sum = volume.checked_mul(required + 1).ok_or_else(overflow)?;
    let window_sum = volume.checked_mul(window + 1).ok_or_else(overflow)?;
    long_sum
        .checked_mul(multiplier.int)
        .and_then(|v| v.checked_mul(window))
        .ok_or_else(overflow)?;
    window_sum
        .checked_mul(required)
        .and_then(|v| v.checked_mul(denominator))
        .ok_or_else(overflow)?;
    window_sum
        .checked_mul(required)
        .and_then(|v| v.checked_mul(10_000))
        .ok_or_else(overflow)?;
    // Gap percent computation is exact too, independent of source values.
    let gap = decimal(params.max_gap_percent())?;
    required.checked_mul(gap.int).ok_or_else(overflow)?;
    100i128
        .checked_mul(10i128.pow(gap.scale))
        .ok_or_else(overflow)?;
    Ok(())
}

pub fn canonical_candidate_key(candidate: &StudyCandidate) -> Result<String, StudyError> {
    let mut params = candidate.params.clone();
    for value in params.values_mut() {
        if let Value::String(text) = value {
            *text = normalized_decimal(text)?;
        }
    }
    let analytics = &candidate.analytics;
    let price_change = if analytics.time_shift_minutes == 0 {
        // Entry is the same close, so nonpositive gates always pass. Preserve
        // every positive threshold: the scalar f64 kernel can round a tiny
        // positive pct/100 away and pass it rather than reject it.
        match analytics.price_change_pct {
            Some(pct) if pct > 0.0 => Some(pct),
            _ => None,
        }
    } else {
        analytics
            .price_change_pct
            .map(|v| if v == 0.0 { 0.0 } else { v })
    };
    serde_json::to_string(&(
        params,
        analytics.cluster_enabled,
        analytics
            .cluster_enabled
            .then_some(analytics.cluster_gap_minutes),
        analytics.time_shift_minutes,
        price_change,
    ))
    .map_err(|error| invalid(error.to_string()))
}

pub fn score_matrix(matrix: &[MatrixCell], min_closed_per_cell: usize) -> Option<f64> {
    if matrix.is_empty() || min_closed_per_cell == 0 {
        return None;
    }
    let mut sum = 0.0;
    for cell in matrix {
        if cell.closed_count < min_closed_per_cell {
            return None;
        }
        let score = cell.risk_expectancy.filter(|value| value.is_finite())?;
        sum += score;
    }
    let score = sum / matrix.len() as f64;
    score.is_finite().then_some(score)
}

fn empty_matrix(spec: &StudyRequest) -> Vec<MatrixCell> {
    spec.tp_levels
        .iter()
        .flat_map(|&tp| {
            spec.sl_levels.iter().map(move |&sl| MatrixCell {
                tp_pct: tp,
                sl_pct: sl,
                risk_expectancy: None,
                avg_move_pct: None,
                closed_count: 0,
                tp_count: 0,
                sl_count: 0,
                unclosed_count: 0,
                no_data_count: 0,
                price_change_filtered_count: 0,
            })
        })
        .collect()
}

fn selected_indexes(
    setups: &[AnalyticsSetup],
    analytics: &AnalyticsParams,
    maturity: f64,
    control: &Control<'_>,
) -> Option<Vec<usize>> {
    if !analytics.cluster_enabled {
        return Some((0..setups.len()).collect());
    }
    let mut selected = Vec::new();
    let mut start = 0;
    while start < setups.len() {
        if control.stopped() {
            return None;
        }
        let mut end = start + 1;
        while end < setups.len()
            && setups[end].close_time_unix - setups[end - 1].close_time_unix
                <= analytics.cluster_gap_minutes * 60
        {
            if end % 256 == 0 && control.stopped() {
                return None;
            }
            end += 1;
        }
        let target = setups[start].close_time_unix as f64
            + maturity * (setups[end - 1].close_time_unix - setups[start].close_time_unix) as f64;
        let mut best = start;
        let mut distance = (setups[start].close_time_unix as f64 - target).abs();
        for (index, setup) in setups.iter().enumerate().take(end).skip(start + 1) {
            if index % 256 == 0 && control.stopped() {
                return None;
            }
            let next = (setup.close_time_unix as f64 - target).abs();
            if next < distance {
                best = index;
                distance = next;
            }
        }
        selected.push(best);
        start = end;
    }
    Some(selected)
}

fn accumulate_setup(
    spec: &StudyRequest,
    analytics: &AnalyticsParams,
    data: &InstrumentInput,
    setup: &AnalyticsSetup,
    matrix: &mut [MatrixCell],
    selected_count: &mut usize,
    control: &Control<'_>,
) -> bool {
    let Some(entry) = find_candle_by_open_time(
        &data.candles,
        setup.open_time_unix + analytics.time_shift_minutes * 60,
    ) else {
        for cell in matrix {
            cell.no_data_count += 1;
        }
        return true;
    };
    if let Some(pct) = analytics.price_change_pct {
        let setup_price = setup.close_price.parse::<f64>().expect("validated close");
        if !passes_price_change(setup_price, entry.close, spec.direction, pct) {
            for cell in matrix {
                cell.price_change_filtered_count += 1;
            }
            return true;
        }
    }
    // The frequency population is the very same full-candidate population
    // admitted to TP/SL evaluation. Missing/filtered entries cannot open a
    // trade; every eligible entry counts regardless of its later outcome.
    *selected_count += 1;
    let entry_time = entry.open_time_unix + 60;
    let end = entry_time + spec.horizon_minutes * 60;
    let tp_prices: Vec<f64> = spec
        .tp_levels
        .iter()
        .map(|pct| {
            entry.close
                * match spec.direction {
                    TradeDirection::Short => 1.0 - pct / 100.0,
                    TradeDirection::Long => 1.0 + pct / 100.0,
                }
        })
        .collect();
    let sl_prices: Vec<f64> = spec
        .sl_levels
        .iter()
        .map(|pct| {
            entry.close
                * match spec.direction {
                    TradeDirection::Short => 1.0 + pct / 100.0,
                    TradeDirection::Long => 1.0 - pct / 100.0,
                }
        })
        .collect();
    let mut tp_first = vec![None; tp_prices.len()];
    let mut sl_first = vec![None; sl_prices.len()];
    let mut remaining_tp = tp_first.len();
    let mut remaining_sl = sl_first.len();
    let start = data
        .candles
        .partition_point(|bar| bar.open_time_unix < entry_time);
    for (offset, candle) in data.candles[start..].iter().enumerate() {
        if offset % 128 == 0 && control.stopped() {
            return false;
        }
        if candle.open_time_unix >= end {
            break;
        }
        for (index, price) in tp_prices.iter().enumerate() {
            let hit = match spec.direction {
                TradeDirection::Short => candle.low <= *price,
                TradeDirection::Long => candle.high >= *price,
            };
            if tp_first[index].is_none() && hit {
                tp_first[index] = Some(candle.open_time_unix);
                remaining_tp -= 1;
            }
        }
        for (index, price) in sl_prices.iter().enumerate() {
            let hit = match spec.direction {
                TradeDirection::Short => candle.high >= *price,
                TradeDirection::Long => candle.low <= *price,
            };
            if sl_first[index].is_none() && hit {
                sl_first[index] = Some(candle.open_time_unix);
                remaining_sl -= 1;
            }
        }
        // After BOTH sides of this candle have been processed, either fully
        // hit axis fixes every pair: later opposite hits cannot precede it.
        if remaining_tp == 0 || remaining_sl == 0 {
            break;
        }
    }
    for (tp_index, tp) in tp_first.iter().enumerate() {
        for (sl_index, sl) in sl_first.iter().enumerate() {
            let cell = &mut matrix[tp_index * sl_first.len() + sl_index];
            match (tp, sl) {
                (None, None) => cell.unclosed_count += 1,
                (Some(_), None) => cell.tp_count += 1,
                (None, Some(_)) => cell.sl_count += 1,
                (Some(tp), Some(sl)) if tp < sl => cell.tp_count += 1,
                (Some(_), Some(_)) => cell.sl_count += 1,
            }
        }
    }
    true
}

pub(crate) fn evaluate_inner(
    spec: &StudyRequest,
    input: &StudyInput,
    candidate: &StudyCandidate,
    limits: &StudyLimits,
    stats: &InputStats,
    control: &Control<'_>,
) -> Result<Option<CandidateEvaluation>, StudyError> {
    Ok(
        evaluate_group(spec, input, &[candidate], limits, stats, control)?
            .into_iter()
            .next()
            .flatten(),
    )
}

/// A packet shares ONLY the exact detector parameters. All analytics variants
/// consume the same completed detector output for one instrument, then that
/// output is released before the next instrument. No cross-batch memoization.
pub(crate) fn evaluate_group(
    spec: &StudyRequest,
    input: &StudyInput,
    candidates: &[&StudyCandidate],
    limits: &StudyLimits,
    stats: &InputStats,
    control: &Control<'_>,
) -> Result<Vec<Option<CandidateEvaluation>>, StudyError> {
    if candidates.is_empty() {
        return Ok(Vec::new());
    }
    let params = parse_params(&spec.algorithm_id, &candidates[0].params)?;
    for candidate in candidates {
        if candidate.params != candidates[0].params {
            return Err(StudyError::Execution(
                "mixed detector parameters in packet".into(),
            ));
        }
        validate_analytics(&candidate.analytics)?;
    }
    numerical_budget(&params, stats)?;
    let frequency_gate = FrequencyGate::new(spec)?;
    let required = params.baseline_long_days() as usize * 1440;
    let detector_bytes = (required + 1)
        .saturating_mul(size_of::<Option<Decimal>>() * 2)
        .saturating_add((params.window_minutes() as usize + 1) * 64);
    let matrix_bytes =
        spec.tp_levels.len() * spec.sl_levels.len() * size_of::<MatrixCell>() * candidates.len();
    let base_bytes = detector_bytes
        .saturating_add(matrix_bytes)
        .saturating_add(16 * 1024);
    limit(
        "candidate_working_bytes",
        base_bytes,
        limits.max_working_bytes,
    )?;
    let from_open = spec.from_unix - 60;
    let warmup_from = from_open - required as i64 * 60;
    let to_open = spec.to_unix - 60;
    let mut matrices: Vec<_> = candidates.iter().map(|_| empty_matrix(spec)).collect();
    let mut summary = TrialSummary {
        score: None,
        setup_count: 0,
        selected_setup_count: 0,
        avg_setups_per_day: None,
        required_setup_count: frequency_gate.required_setup_count,
        evaluated_minutes: 0,
        warmup_minutes: 0,
    };
    let mut selected_counts = vec![0; candidates.len()];
    let interrupted = || vec![None; candidates.len()];
    // Stable fixed instrument order, independent of snapshot delivery order.
    for instrument in &spec.instruments {
        if control.stopped() {
            return Ok(interrupted());
        }
        control
            .detector_passes
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let data = input
            .instruments
            .iter()
            .find(|data| &data.instrument == instrument)
            .expect("validated input scope");
        let mut detector = PriceVolumeSpikeDetector::with_params(params.clone(), None)
            .map_err(|error| StudyError::Detector(error.to_string()))?;
        let mut setups = Vec::new();
        let mut setup_bytes = 0usize;
        let start = data
            .detector_bars
            .partition_point(|bar| bar.open_time_unix < warmup_from);
        for (offset, bar) in data.detector_bars[start..].iter().enumerate() {
            if offset % 64 == 0 && control.stopped() {
                return Ok(interrupted());
            }
            if bar.open_time_unix >= to_open {
                break;
            }
            let pushed = detector
                .push(bar)
                .map_err(|error| StudyError::Detector(error.to_string()))?;
            if pushed.status == PushStatus::Warmup {
                summary.warmup_minutes += 1;
            }
            if bar.open_time_unix >= from_open && pushed.status == PushStatus::Evaluated {
                summary.evaluated_minutes += 1;
            }
            if let Some(setup) = pushed.setup
                && setup.close_time_unix >= spec.from_unix
                && setup.close_time_unix < spec.to_unix
            {
                summary.setup_count += 1;
                limit(
                    "setups_per_candidate",
                    summary.setup_count,
                    limits.max_setups_per_candidate,
                )?;
                setup_bytes = setup_bytes.saturating_add(
                    size_of::<AnalyticsSetup>() * 3
                        + instrument.exchange.len()
                        + instrument.category.len()
                        + instrument.symbol.len()
                        + setup.close_price.len(),
                );
                limit(
                    "candidate_working_bytes",
                    base_bytes.saturating_add(setup_bytes),
                    limits.max_working_bytes,
                )?;
                setups.push(AnalyticsSetup {
                    exchange_code: instrument.exchange.clone(),
                    category: instrument.category.clone(),
                    symbol: instrument.symbol.clone(),
                    open_time_unix: setup.open_time_unix,
                    close_time_unix: setup.close_time_unix,
                    close_price: setup.close_price,
                });
            }
        }
        drop(detector);
        for (candidate_index, candidate) in candidates.iter().enumerate() {
            let Some(indexes) =
                selected_indexes(&setups, &candidate.analytics, spec.setup_maturity, control)
            else {
                return Ok(interrupted());
            };
            for index in indexes {
                if control.stopped()
                    || !accumulate_setup(
                        spec,
                        &candidate.analytics,
                        data,
                        &setups[index],
                        &mut matrices[candidate_index],
                        &mut selected_counts[candidate_index],
                        control,
                    )
                {
                    return Ok(interrupted());
                }
            }
        }
    }
    let mut evaluations = Vec::with_capacity(candidates.len());
    for (candidate_index, mut matrix) in matrices.into_iter().enumerate() {
        for cell in &mut matrix {
            if control.stopped() {
                evaluations.extend((evaluations.len()..candidates.len()).map(|_| None));
                return Ok(evaluations);
            }
            cell.closed_count = cell.tp_count + cell.sl_count;
            if cell.closed_count > 0 {
                // Exact operation order of aggregate_limit_outcomes: all TP moves
                // followed by all SL moves, no rounded/count-weighted shortcut.
                let mut sum = 0.0;
                for index in 0..cell.tp_count {
                    if index % 256 == 0 && control.stopped() {
                        evaluations.extend((evaluations.len()..candidates.len()).map(|_| None));
                        return Ok(evaluations);
                    }
                    sum += cell.tp_pct;
                }
                for index in 0..cell.sl_count {
                    if index % 256 == 0 && control.stopped() {
                        evaluations.extend((evaluations.len()..candidates.len()).map(|_| None));
                        return Ok(evaluations);
                    }
                    sum -= cell.sl_pct;
                }
                let avg = sum / cell.closed_count as f64;
                cell.avg_move_pct = Some(avg);
                cell.risk_expectancy = Some(avg / cell.sl_pct);
            }
        }
        let mut summary = summary.clone();
        summary.selected_setup_count = selected_counts[candidate_index];
        summary.avg_setups_per_day = Some(frequency_gate.average(summary.selected_setup_count));
        summary.score = if frequency_gate.accepts(summary.selected_setup_count) {
            score_matrix(&matrix, spec.min_closed_per_cell)
        } else {
            None
        };
        evaluations.push(Some(CandidateEvaluation { summary, matrix }));
    }
    Ok(evaluations)
}

/// A cancelled or time-interrupted candidate returns None, never a partial
/// matrix. run_study validates frozen input once before parallel evaluations.
pub fn evaluate_candidate(
    spec: &StudyRequest,
    input: &StudyInput,
    candidate: &StudyCandidate,
    limits: &StudyLimits,
    cancel: &CancelToken,
) -> Result<Option<CandidateEvaluation>, StudyError> {
    validate_request(spec, &default_parameter_descriptors(&spec.algorithm_id)?)?;
    let Some(stats) = validate_input(spec, input, limits, cancel)? else {
        return Ok(None);
    };
    let passes = std::sync::atomic::AtomicUsize::new(0);
    let control = Control {
        cancel,
        deadline: Instant::now() + std::time::Duration::from_secs(spec.max_seconds),
        detector_passes: &passes,
    };
    evaluate_inner(spec, input, candidate, limits, &stats, &control)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use setups_analytics::{EvaluateLimitOptions, LimitStatus, evaluate_limit};

    fn spec() -> StudyRequest {
        serde_json::from_value(json!({
            "algorithmId":"price_volume_spike", "algorithmVersion":"1.1.1",
            "algorithmParams":setups_domain::SpikeParams::default(),
            "analyticsParams":AnalyticsParams::default(),
            "instruments":[{"exchange":"bybit","category":"linear","symbol":"BTCUSDT"}],
            "fromUnix":120,"toUnix":180,"direction":"short","seed":42
        }))
        .unwrap()
    }

    fn assert_scalar_grid(
        candles: Vec<setups_analytics::OhlcCandle>,
        setup: AnalyticsSetup,
        direction: TradeDirection,
        shift: i64,
        horizon: i64,
    ) {
        let mut spec = spec();
        spec.direction = direction;
        spec.horizon_minutes = horizon;
        let analytics = AnalyticsParams {
            time_shift_minutes: shift,
            ..AnalyticsParams::default()
        };
        let data = InstrumentInput {
            instrument: spec.instruments[0].clone(),
            detector_bars: Vec::new(),
            candles,
        };
        let mut matrix = empty_matrix(&spec);
        let mut selected_count = 0;
        let token = CancelToken::new();
        let passes = std::sync::atomic::AtomicUsize::new(0);
        let control = Control {
            cancel: &token,
            deadline: Instant::now() + std::time::Duration::from_secs(30),
            detector_passes: &passes,
        };
        assert!(accumulate_setup(
            &spec,
            &analytics,
            &data,
            &setup,
            &mut matrix,
            &mut selected_count,
            &control
        ));
        for cell in matrix {
            let scalar = evaluate_limit(EvaluateLimitOptions {
                candles: &data.candles,
                setup: &setup,
                direction,
                tp_pct: cell.tp_pct,
                sl_pct: cell.sl_pct,
                time_shift_minutes: shift,
                horizon_minutes: horizon,
            });
            let observed = if cell.tp_count > 0 {
                LimitStatus::Tp
            } else if cell.sl_count > 0 {
                LimitStatus::Sl
            } else if cell.no_data_count > 0 {
                LimitStatus::NoData
            } else if cell.price_change_filtered_count > 0 {
                LimitStatus::PriceChangeFiltered
            } else {
                LimitStatus::Unclosed
            };
            assert_eq!(
                observed, scalar.status,
                "{direction:?} TP={} SL={} shift={shift} horizon={horizon}",
                cell.tp_pct, cell.sl_pct
            );
            assert_eq!(
                selected_count,
                usize::from(matches!(
                    observed,
                    LimitStatus::Tp | LimitStatus::Sl | LimitStatus::Unclosed
                ))
            );
            assert_eq!(
                cell.tp_count
                    + cell.sl_count
                    + cell.no_data_count
                    + cell.price_change_filtered_count
                    + cell.unclosed_count,
                1
            );
        }
    }

    #[test]
    fn first_hits_match_every_existing_scalar_golden_case_on_all_81_cells() {
        let fixture: Value =
            serde_json::from_str(include_str!("../../../fixtures/parity.json")).unwrap();
        for case in fixture["analytics"]["evaluation"]["cases"]
            .as_array()
            .unwrap()
        {
            assert_scalar_grid(
                serde_json::from_value(case["candles"].clone()).unwrap(),
                serde_json::from_value(case["setup"].clone()).unwrap(),
                serde_json::from_value(case["direction"].clone()).unwrap(),
                case["timeShiftMinutes"].as_i64().unwrap(),
                case["horizonMinutes"].as_i64().unwrap(),
            );
        }
    }

    #[test]
    fn one_axis_complete_opposite_never_or_later_same_bar_and_exact_horizon() {
        let setup = AnalyticsSetup {
            exchange_code: "bybit".into(),
            category: "linear".into(),
            symbol: "BTCUSDT".into(),
            open_time_unix: 60,
            close_time_unix: 120,
            close_price: "100".into(),
        };
        let candle = |time, high, low| setups_analytics::OhlcCandle {
            open_time_unix: time,
            open: 100.0,
            high,
            low,
            close: 100.0,
        };
        for direction in [TradeDirection::Short, TradeDirection::Long] {
            for (high, low, later_high, later_low) in [
                (100.0, 50.0, 200.0, 50.0),
                (150.0, 100.0, 150.0, 50.0),
                (150.0, 50.0, 150.0, 50.0),
                (100.0, 100.0, 100.0, 100.0),
            ] {
                assert_scalar_grid(
                    vec![
                        candle(60, 100.0, 100.0),
                        candle(120, high, low),
                        candle(180, later_high, later_low),
                    ],
                    setup.clone(),
                    direction,
                    0,
                    10080,
                );
            }
            // Touch at entry-close + horizon*60 must NOT close a trade.
            assert_scalar_grid(
                vec![
                    candle(60, 100.0, 100.0),
                    candle(120, 100.0, 100.0),
                    candle(180, 150.0, 50.0),
                ],
                setup.clone(),
                direction,
                0,
                1,
            );
            // Entry candle itself can touch both levels; it must not be scanned.
            assert_scalar_grid(
                vec![candle(60, 150.0, 50.0), candle(120, 100.0, 100.0)],
                setup.clone(),
                direction,
                0,
                10,
            );
        }
    }

    #[test]
    fn cancelled_analytics_does_not_return_completed_counts() {
        let spec = spec();
        let setup = AnalyticsSetup {
            exchange_code: "bybit".into(),
            category: "linear".into(),
            symbol: "BTCUSDT".into(),
            open_time_unix: 60,
            close_time_unix: 120,
            close_price: "100".into(),
        };
        let data = InstrumentInput {
            instrument: spec.instruments[0].clone(),
            detector_bars: Vec::new(),
            candles: vec![
                setups_analytics::OhlcCandle {
                    open_time_unix: 60,
                    open: 100.0,
                    high: 100.0,
                    low: 100.0,
                    close: 100.0,
                },
                setups_analytics::OhlcCandle {
                    open_time_unix: 120,
                    open: 100.0,
                    high: 100.0,
                    low: 100.0,
                    close: 100.0,
                },
            ],
        };
        let token = CancelToken::new();
        token.cancel();
        let passes = std::sync::atomic::AtomicUsize::new(0);
        let control = Control {
            cancel: &token,
            deadline: Instant::now() + std::time::Duration::from_secs(30),
            detector_passes: &passes,
        };
        let mut selected_count = 0;
        assert!(!accumulate_setup(
            &spec,
            &AnalyticsParams::default(),
            &data,
            &setup,
            &mut empty_matrix(&spec),
            &mut selected_count,
            &control
        ));
    }
}
