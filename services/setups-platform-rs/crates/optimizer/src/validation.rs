use crate::*;
use serde_json::Value;
use setups_domain::decimal::{Decimal, compare_scaled};
use setups_domain::{
    AlgorithmParams, SPIKE_ALGORITHM_ID, SPIKE_DROP_ALGORITHM_ID, SpikeDropParams, SpikeParams,
};
use std::collections::{BTreeMap, BTreeSet};

pub(crate) fn invalid(message: impl Into<String>) -> StudyError {
    StudyError::InvalidRequest(message.into())
}

/// Adapter metadata for the currently registered modules. Search itself only
/// handles generic field maps; adding a module changes this adapter, not search.
pub fn default_parameter_descriptors(
    id: &str,
) -> Result<BTreeMap<String, ParameterDescriptor>, StudyError> {
    let metadata =
        setups_domain::algorithm_fields(id).ok_or_else(|| invalid("unknown_algorithm"))?;
    metadata
        .as_array()
        .expect("module metadata array")
        .iter()
        .map(|field| {
            let key = field["key"].as_str().expect("module field key").to_string();
            let kind = match field["type"].as_str() {
                Some("integer") => ParameterKind::Integer,
                Some("decimal") => ParameterKind::Decimal,
                _ => return Err(invalid("unsupported module field kind")),
            };
            Ok((
                key,
                ParameterDescriptor {
                    kind,
                    min: field["min"].clone(),
                    max: field["max"].clone(),
                },
            ))
        })
        .collect()
}

/// Decimal descriptor bounds can be JSON numbers or strings. Expand only the
/// bounded exponent notation produced by JSON numbers, never round via f64.
fn numeric_text(value: &Value) -> Result<String, StudyError> {
    let text = match value {
        Value::String(text) => return Ok(text.clone()),
        Value::Number(number) => number.to_string(),
        _ => return Err(invalid("numeric descriptor expected")),
    };
    let Some((mantissa, exponent)) = text.split_once(['e', 'E']) else {
        return Ok(text);
    };
    let exponent: i32 = exponent
        .parse()
        .map_err(|_| invalid("invalid descriptor exponent"))?;
    if !(-18..=18).contains(&exponent) {
        return Err(invalid("descriptor exponent out of range"));
    }
    let negative = mantissa.starts_with('-');
    let mantissa = mantissa.trim_start_matches('-');
    let point = mantissa.find('.').unwrap_or(mantissa.len()) as i32;
    let digits = mantissa.replace('.', "");
    let point = point + exponent;
    let unsigned = if point <= 0 {
        format!("0.{}{}", "0".repeat((-point) as usize), digits)
    } else if point as usize >= digits.len() {
        format!("{}{}", digits, "0".repeat(point as usize - digits.len()))
    } else {
        format!(
            "{}.{}",
            &digits[..point as usize],
            &digits[point as usize..]
        )
    };
    Ok(if negative {
        format!("-{unsigned}")
    } else {
        unsigned
    })
}

pub(crate) fn decimal(text: &str) -> Result<Decimal, StudyError> {
    if text.len() > 64 {
        return Err(invalid("decimal string exceeds 64 bytes"));
    }
    Decimal::parse(text).map_err(|error| invalid(error.to_string()))
}

pub(crate) fn normalized_decimal(text: &str) -> Result<String, StudyError> {
    let Decimal { mut int, mut scale } = decimal(text)?;
    while scale > 0 && int % 10 == 0 {
        int /= 10;
        scale -= 1;
    }
    let negative = int < 0;
    let digits = int.unsigned_abs().to_string();
    let unsigned = if scale == 0 {
        digits
    } else if digits.len() <= scale as usize {
        format!("0.{}{}", "0".repeat(scale as usize - digits.len()), digits)
    } else {
        let point = digits.len() - scale as usize;
        format!("{}.{}", &digits[..point], &digits[point..])
    };
    Ok(if negative {
        format!("-{unsigned}")
    } else {
        unsigned
    })
}

fn validate_field(key: &str, value: &Value, field: &ParameterDescriptor) -> Result<(), StudyError> {
    match field.kind {
        ParameterKind::Integer => {
            let number = value
                .as_u64()
                .ok_or_else(|| invalid(format!("{key} must be an integer number")))?;
            let min = field
                .min
                .as_u64()
                .ok_or_else(|| invalid("integer descriptor min"))?;
            let max = field
                .max
                .as_u64()
                .ok_or_else(|| invalid("integer descriptor max"))?;
            if min > max || !(min..=max).contains(&number) {
                return Err(invalid(format!("{key} out of range")));
            }
        }
        ParameterKind::Decimal => {
            let number = decimal(
                value
                    .as_str()
                    .ok_or_else(|| invalid(format!("{key} must be a decimal string")))?,
            )?;
            let min = decimal(&numeric_text(&field.min)?)?;
            let max = decimal(&numeric_text(&field.max)?)?;
            let cmp = |a, b| compare_scaled(a, 1, b, 1).map_err(|error| invalid(error.to_string()));
            if cmp(min, max)?.is_gt() || cmp(number, min)?.is_lt() || cmp(number, max)?.is_gt() {
                return Err(invalid(format!("{key} out of range")));
            }
        }
    }
    Ok(())
}

pub(crate) fn parse_params(id: &str, params: &ParamMap) -> Result<AlgorithmParams, StudyError> {
    let fields = default_parameter_descriptors(id)?;
    if params.len() != fields.len() {
        return Err(invalid(
            "algorithmParams must contain exactly the module fields",
        ));
    }
    for (key, value) in params {
        validate_field(
            key,
            value,
            fields
                .get(key)
                .ok_or_else(|| invalid(format!("unknown algorithm field {key}")))?,
        )?;
    }
    let value = serde_json::to_value(params).map_err(|error| invalid(error.to_string()))?;
    let params = match id {
        SPIKE_ALGORITHM_ID => AlgorithmParams::Spike(
            serde_json::from_value::<SpikeParams>(value)
                .map_err(|error| invalid(error.to_string()))?,
        ),
        SPIKE_DROP_ALGORITHM_ID => AlgorithmParams::SpikeDrop(
            serde_json::from_value::<SpikeDropParams>(value)
                .map_err(|error| invalid(error.to_string()))?,
        ),
        _ => return Err(invalid("unknown_algorithm")),
    };
    if params.baseline_short_days() > params.baseline_long_days() {
        return Err(invalid(
            "baselineShortDays must not exceed baselineLongDays",
        ));
    }
    if params.window_minutes() >= params.baseline_short_days() * 1440 {
        return Err(invalid(
            "windowMinutes must be shorter than the short baseline",
        ));
    }
    Ok(params)
}

pub(crate) fn validate_analytics(params: &AnalyticsParams) -> Result<(), StudyError> {
    if !(1..=10_080).contains(&params.cluster_gap_minutes)
        || !(0..=43_200).contains(&params.time_shift_minutes)
    {
        return Err(invalid("analytics gap or time shift out of range"));
    }
    if params
        .price_change_pct
        .is_some_and(|pct| !pct.is_finite() || !(-100.0..=1000.0).contains(&pct))
    {
        return Err(invalid("priceChangePct out of range"));
    }
    Ok(())
}

pub(crate) fn analytics_with_value(
    params: &mut AnalyticsParams,
    key: &str,
    value: &Value,
) -> Result<(), StudyError> {
    match key {
        "clusterEnabled" => {
            params.cluster_enabled = value
                .as_bool()
                .ok_or_else(|| invalid("clusterEnabled must be boolean"))?
        }
        "clusterGapMinutes" => {
            params.cluster_gap_minutes = value
                .as_i64()
                .ok_or_else(|| invalid("clusterGapMinutes must be integer"))?
        }
        "timeShiftMinutes" => {
            params.time_shift_minutes = value
                .as_i64()
                .ok_or_else(|| invalid("timeShiftMinutes must be integer"))?
        }
        "priceChangePct" => {
            params.price_change_pct = if value.is_null() {
                None
            } else {
                Some(
                    value
                        .as_f64()
                        .ok_or_else(|| invalid("priceChangePct must be number or null"))?,
                )
            }
        }
        _ => {
            return Err(invalid(format!(
                "unknown or fixed analytics search field {key}"
            )));
        }
    }
    validate_analytics(params)
}

pub fn validate_request(
    spec: &StudyRequest,
    fields: &BTreeMap<String, ParameterDescriptor>,
) -> Result<(), StudyError> {
    validate_request_mode(spec, fields, false)
}

pub(crate) fn validate_request_mode(
    spec: &StudyRequest,
    fields: &BTreeMap<String, ParameterDescriptor>,
    continuous: bool,
) -> Result<(), StudyError> {
    let params = parse_params(&spec.algorithm_id, &spec.algorithm_params)?;
    if spec.algorithm_version != params.algorithm_version() {
        return Err(invalid("algorithm_version_changed"));
    }
    if spec.from_unix < 0
        || spec.to_unix > 253_402_300_740
        || spec.from_unix >= spec.to_unix
        || (!continuous && (spec.from_unix % 60 != 0 || spec.to_unix % 60 != 0))
    {
        return Err(invalid(
            "signal bounds must be increasing minute-aligned Unix seconds",
        ));
    }
    if spec.instruments.is_empty() || spec.instruments.len() > 10_000 {
        return Err(invalid("instruments must contain 1..10000 entries"));
    }
    let mut identities = BTreeSet::new();
    for instrument in &spec.instruments {
        if [
            &instrument.exchange,
            &instrument.category,
            &instrument.symbol,
        ]
        .iter()
        .any(|s| s.is_empty() || s.len() > 120 || s.chars().any(char::is_control))
            || !identities.insert(instrument)
        {
            return Err(invalid("invalid or duplicate instrument"));
        }
    }
    if !(1..=4096).contains(&spec.max_trials)
        || !(1..=86_400).contains(&spec.max_seconds)
        || !(1..=43_200).contains(&spec.horizon_minutes)
        || spec.min_closed_per_cell == 0
        || !spec.setup_maturity.is_finite()
        || !(0.0..=1.0).contains(&spec.setup_maturity)
        || (spec.setup_maturity * 10.0 - (spec.setup_maturity * 10.0).round()).abs() > 1e-9
    {
        return Err(invalid(
            "invalid budget, horizon, maturity or minClosedPerCell",
        ));
    }
    if !spec.min_setups_per_day.is_finite() || spec.min_setups_per_day < 0.0 {
        return Err(invalid("minSetupsPerDay must be finite and nonnegative"));
    }
    for levels in [&spec.tp_levels, &spec.sl_levels] {
        if levels.is_empty() || levels.len() > 64 {
            return Err(invalid("TP/SL axes require 1..64 levels"));
        }
        let mut seen = BTreeSet::new();
        for level in levels {
            if !level.is_finite()
                || *level <= 0.0
                || *level > 100.0
                || !seen.insert(level.to_bits())
            {
                return Err(invalid(
                    "TP/SL levels must be unique finite numbers in (0,100]",
                ));
            }
        }
    }
    validate_analytics(&spec.analytics_params)?;
    let builtin = default_parameter_descriptors(&spec.algorithm_id)?;
    for (key, value) in &spec.algorithm_params {
        let descriptor = fields
            .get(key)
            .ok_or_else(|| invalid(format!("missing descriptor for {key}")))?;
        if descriptor.kind != builtin[key].kind {
            return Err(invalid(format!("descriptor kind mismatch for {key}")));
        }
        validate_field(key, value, descriptor)?;
    }
    let mut total_values = 0usize;
    for (key, values) in &spec.algorithm_search {
        let field = fields
            .get(key)
            .filter(|_| builtin.contains_key(key))
            .ok_or_else(|| invalid(format!("unknown algorithm search field {key}")))?;
        if values.is_empty() || values.len() > 4096 {
            return Err(invalid("domains require 1..4096 values"));
        }
        total_values = total_values.saturating_add(values.len());
        for value in values {
            validate_field(key, value, field)?;
            validate_field(key, value, &builtin[key])?;
        }
    }
    for (key, values) in &spec.analytics_search {
        if values.is_empty() || values.len() > 4096 {
            return Err(invalid("domains require 1..4096 values"));
        }
        total_values = total_values.saturating_add(values.len());
        for value in values {
            let mut params = spec.analytics_params.clone();
            analytics_with_value(&mut params, key, value)?;
        }
    }
    if total_values > 16_384 {
        return Err(invalid("total domain values exceed 16384"));
    }
    Ok(())
}

pub fn required_input_bounds(spec: &StudyRequest) -> Result<InputBounds, StudyError> {
    validate_request(spec, &default_parameter_descriptors(&spec.algorithm_id)?)?;
    input_bounds_inner(spec)
}

pub(crate) fn input_bounds_inner(spec: &StudyRequest) -> Result<InputBounds, StudyError> {
    let baseline = parse_params(&spec.algorithm_id, &spec.algorithm_params)?;
    let mut max_days = i64::from(baseline.baseline_long_days());
    // The module adapter is the only component interpreting its warmup field.
    if let Some(values) = spec.algorithm_search.get("baselineLongDays") {
        for value in values {
            max_days = max_days.max(
                value
                    .as_i64()
                    .ok_or_else(|| invalid("invalid baselineLongDays"))?,
            );
        }
    }
    let mut max_shift = spec.analytics_params.time_shift_minutes;
    if let Some(values) = spec.analytics_search.get("timeShiftMinutes") {
        for value in values {
            max_shift = max_shift.max(
                value
                    .as_i64()
                    .ok_or_else(|| invalid("invalid timeShiftMinutes"))?,
            );
        }
    }
    let overflow = || invalid("input time bounds overflow");
    let max_warmup_minutes = max_days.checked_mul(1440).ok_or_else(overflow)?;
    let candles_from_unix = spec.from_unix.checked_sub(60).ok_or_else(overflow)?;
    let detector_to_unix = spec.to_unix.checked_sub(60).ok_or_else(overflow)?;
    let detector_from_unix = candles_from_unix
        .checked_sub(max_warmup_minutes.checked_mul(60).ok_or_else(overflow)?)
        .ok_or_else(overflow)?;
    let candles_to_unix = detector_to_unix
        .checked_add(
            max_shift
                .checked_add(spec.horizon_minutes)
                .and_then(|v| v.checked_mul(60))
                .ok_or_else(overflow)?,
        )
        .ok_or_else(overflow)?;
    Ok(InputBounds {
        from_unix: detector_from_unix,
        to_unix: candles_to_unix,
        detector_from_unix,
        detector_to_unix,
        candles_from_unix,
        candles_to_unix,
        max_warmup_minutes,
        max_time_shift_minutes: max_shift,
    })
}
