//! Additive research enrichment only: no market reads, clock, selection, gate or
//! change to saved matrix arithmetic. Owned by one sealed research snapshot;
//! at most 3,661 UTC buckets / 1 MiB JSON, with no process-wide retained state.
use crate::{RecommendationDirection, RecommendationProvenance};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{fmt, io};

pub const EXPECTANCY_CALCULATION_VERSION: u32 = 2;
pub const UTC_DAY_SECONDS: i64 = 86_400;
/// Matches the existing continuous lookback admission (3660 days), not a new
/// restriction on research. A fractional start can touch one additional day.
pub const EXPECTANCY_MAX_PERIOD_DAYS: i64 = 3660;
pub const EXPECTANCY_MAX_POINTS: usize = EXPECTANCY_MAX_PERIOD_DAYS as usize + 1;
pub const EXPECTANCY_MAX_ARTIFACT_BYTES: usize = 1024 * 1024;
const MAX_SOURCE_STRING_BYTES: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExpectancyTimezone {
    UTC,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExpectancyUnavailableReason {
    MissingSelectedCell,
    MissingSelectedMean,
    MissingMatrixMean,
    MissingFrozenOutcomes,
    UnprovenOutcomeProvenance,
    IncompleteOutcomeHistory,
    NoClosedOutcomes,
    InsufficientFiniteDays,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum MetricAvailability<T> {
    Available { value: T },
    Unavailable { reason: ExpectancyUnavailableReason },
}

// A direct typed field decoder avoids serde's internally-tagged Content buffer:
// with the workspace's arbitrary_precision JSON feature that buffer represents
// a decimal as a private map and otherwise cannot deserialize Available<f64>.
impl<'de, T: Deserialize<'de>> Deserialize<'de> for MetricAvailability<T> {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(bound(deserialize = "T: Deserialize<'de>"), deny_unknown_fields)]
        struct Wire<T> {
            status: String,
            value: Option<T>,
            reason: Option<ExpectancyUnavailableReason>,
        }
        let wire = Wire::<T>::deserialize(deserializer)?;
        match (wire.status.as_str(), wire.value, wire.reason) {
            ("available", Some(value), None) => Ok(Self::Available { value }),
            ("unavailable", None, Some(reason)) => Ok(Self::Unavailable { reason }),
            _ => Err(serde::de::Error::custom("invalid metric availability")),
        }
    }
}
impl<T> MetricAvailability<T> {
    pub fn as_ref(&self) -> Option<&T> {
        match self {
            Self::Available { value } => Some(value),
            Self::Unavailable { .. } => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExpectancyAnalytics {
    pub cluster_enabled: bool,
    pub cluster_gap_minutes: Option<i64>,
    pub time_shift_minutes: i64,
    pub price_change_pct: Option<f64>,
    pub horizon_minutes: i64,
}

/// Identity/provenance is supplied by the research owner, never looked up from
/// today's winner. derived_snapshot_id identifies this immutable numeric copy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExpectancySource {
    pub study_id: String,
    pub derived_snapshot_id: String,
    pub generation_id: Option<String>,
    pub profile_id: Option<String>,
    pub provenance: RecommendationProvenance,
    pub direction: RecommendationDirection,
    pub tp_pct: f64,
    pub sl_pct: f64,
    pub analytics: ExpectancyAnalytics,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SelectedCellInput {
    /// Already saved rawR of the selected cell, never a mean of daily means.
    pub raw_r: Option<f64>,
    pub tp_count: u64,
    pub sl_count: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SelectedCellMetrics {
    pub raw_r: MetricAvailability<f64>,
    pub tp_count: u64,
    pub sl_count: u64,
    pub closed_count: u64,
    pub win_rate_pct: MetricAvailability<f64>,
}

/// One compact bucket already derived by the owner from proven frozen outcomes
/// using raw-signal CLOSE timestamps. Null with rows is retained as a line break.
/// Missing calendar dates need not be materialized, and are never zero-filled.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UtcDailyInput {
    pub day_start_unix: i64,
    pub row_count: u64,
    pub tp_count: u64,
    pub sl_count: u64,
    pub mean_r: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UtcDailyPoint {
    pub date: String,
    pub day_start_unix: i64,
    pub x_utc_days: f64,
    pub row_count: u64,
    pub tp_count: u64,
    pub sl_count: u64,
    pub mean_r: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UtcLinearTrend {
    pub valid_day_count: usize,
    pub mean_x: f64,
    pub mean_y: f64,
    pub slope_r_per_day: f64,
    pub first_day_start_unix: i64,
    pub last_day_start_unix: i64,
    pub first_x: f64,
    pub last_x: f64,
    pub fitted_start_r: f64,
    pub fitted_end_r: f64,
}
impl UtcLinearTrend {
    fn evaluate(&self, x: f64) -> Result<f64, ExpectancyError> {
        finite(self.mean_y + self.slope_r_per_day * (x - self.mean_x))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExpectancyInput {
    pub source: ExpectancySource,
    pub selected_cell: Option<SelectedCellInput>,
    /// Exact saved equal-cell matrix meanR, including the selected cell.
    pub matrix_mean_r: Option<f64>,
    pub daily: MetricAvailability<Vec<UtcDailyInput>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExpectancyArtifact {
    pub calculation_version: u32,
    pub timezone: ExpectancyTimezone,
    pub origin_unix: i64,
    pub from_unix: i64,
    pub to_unix: i64,
    pub period_days: f64,
    pub source: ExpectancySource,
    pub selected_cell: MetricAvailability<SelectedCellMetrics>,
    pub matrix_mean_r: MetricAvailability<f64>,
    pub daily: MetricAvailability<Vec<UtcDailyPoint>>,
    pub trend: Option<UtcLinearTrend>,
    pub trend_unavailable_reason: Option<ExpectancyUnavailableReason>,
    pub trend_delta_r: MetricAvailability<f64>,
}

/// A study asset identity, not PNG bytes copied into each recommendation. The
/// owner must allocate a new derived_snapshot_id if numeric provenance changes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExpectancyImageKey {
    pub study_id: String,
    pub derived_snapshot_id: String,
    pub input_snapshot_hash: String,
    pub tp_pct: f64,
    pub sl_pct: f64,
    pub calculation_version: u32,
    pub timezone: ExpectancyTimezone,
    pub render_version: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExpectancyError {
    InvalidPeriod,
    InvalidTimestamp,
    InvalidSource,
    InvalidCounts,
    NonFiniteNumber,
    TooManyPoints,
    UnorderedOrDuplicateDay,
    DayOutsidePeriod,
    InconsistentDailyMean,
    InconsistentSelectedCell,
    UnsupportedVersion,
    InconsistentArtifact,
    ArtifactTooLarge,
    InvalidJson,
}
impl fmt::Display for ExpectancyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "expectancy: {self:?}")
    }
}
impl std::error::Error for ExpectancyError {}

fn finite(value: f64) -> Result<f64, ExpectancyError> {
    if value.is_finite() {
        Ok(value)
    } else {
        Err(ExpectancyError::NonFiniteNumber)
    }
}
fn closed_count(tp: u64, sl: u64) -> Result<u64, ExpectancyError> {
    tp.checked_add(sl).ok_or(ExpectancyError::InvalidCounts)
}
fn available_or(
    value: Option<f64>,
    reason: ExpectancyUnavailableReason,
) -> Result<MetricAvailability<f64>, ExpectancyError> {
    Ok(match value {
        Some(value) => MetricAvailability::Available {
            value: finite(value)?,
        },
        None => MetricAvailability::Unavailable { reason },
    })
}

pub fn utc_midnight_unix(timestamp: i64) -> Result<i64, ExpectancyError> {
    let midnight = timestamp
        .div_euclid(UTC_DAY_SECONDS)
        .checked_mul(UTC_DAY_SECONDS)
        .ok_or(ExpectancyError::InvalidTimestamp)?;
    utc_date(midnight)?;
    Ok(midnight)
}
pub fn utc_date(timestamp: i64) -> Result<String, ExpectancyError> {
    let date = DateTime::<Utc>::from_timestamp(timestamp, 0)
        .ok_or(ExpectancyError::InvalidTimestamp)?
        .format("%Y-%m-%d")
        .to_string();
    if date.len() != 10 {
        return Err(ExpectancyError::InvalidTimestamp);
    }
    Ok(date)
}
pub fn utc_day_coordinate(origin_unix: i64, timestamp: i64) -> f64 {
    (i128::from(timestamp) - i128::from(origin_unix)) as f64 / UTC_DAY_SECONDS as f64
}

fn validate_source(source: &ExpectancySource) -> Result<(i64, f64), ExpectancyError> {
    let p = &source.provenance;
    let duration = p
        .to_unix
        .checked_sub(p.from_unix)
        .ok_or(ExpectancyError::InvalidPeriod)?;
    if duration <= 0 || duration > EXPECTANCY_MAX_PERIOD_DAYS * UTC_DAY_SECONDS {
        return Err(ExpectancyError::InvalidPeriod);
    }
    utc_date(p.to_unix)?;
    let origin = utc_midnight_unix(p.from_unix)?;
    for text in [
        &source.study_id,
        &source.derived_snapshot_id,
        &p.snapshot_hash,
        &p.algorithm_id,
        &p.algorithm_version,
        &p.optimizer_version,
        &p.captured_at,
    ] {
        if text.is_empty() || text.len() > MAX_SOURCE_STRING_BYTES {
            return Err(ExpectancyError::InvalidSource);
        }
    }
    for text in [&source.generation_id, &source.profile_id]
        .into_iter()
        .flatten()
    {
        if text.is_empty() || text.len() > MAX_SOURCE_STRING_BYTES {
            return Err(ExpectancyError::InvalidSource);
        }
    }
    let a = &source.analytics;
    if finite(source.tp_pct)? <= 0.0
        || finite(source.sl_pct)? <= 0.0
        || a.time_shift_minutes < 0
        || a.horizon_minutes <= 0
        || a.cluster_gap_minutes.is_some_and(|gap| gap <= 0)
        || p.observation_cutoff > p.to_unix
    {
        return Err(ExpectancyError::InvalidSource);
    }
    if let Some(value) = a.price_change_pct {
        finite(value)?;
    }
    Ok((origin, duration as f64 / UTC_DAY_SECONDS as f64))
}

/// No sorting or truncation: a duplicate/reversed/out-of-scope day is a failed
/// provenance/assembly invariant, not an opportunity to silently repair history.
pub fn build_expectancy_artifact(
    input: ExpectancyInput,
) -> Result<ExpectancyArtifact, ExpectancyError> {
    let (origin_unix, period_days) = validate_source(&input.source)?;
    let from_unix = input.source.provenance.from_unix;
    let to_unix = input.source.provenance.to_unix;
    let selected_cell = match input.selected_cell {
        None => MetricAvailability::Unavailable {
            reason: ExpectancyUnavailableReason::MissingSelectedCell,
        },
        Some(cell) => {
            let count = closed_count(cell.tp_count, cell.sl_count)?;
            if count == 0 && cell.raw_r.is_some() {
                return Err(ExpectancyError::InconsistentSelectedCell);
            }
            MetricAvailability::Available {
                value: SelectedCellMetrics {
                    raw_r: available_or(
                        cell.raw_r,
                        if count == 0 {
                            ExpectancyUnavailableReason::NoClosedOutcomes
                        } else {
                            ExpectancyUnavailableReason::MissingSelectedMean
                        },
                    )?,
                    tp_count: cell.tp_count,
                    sl_count: cell.sl_count,
                    closed_count: count,
                    win_rate_pct: if count == 0 {
                        MetricAvailability::Unavailable {
                            reason: ExpectancyUnavailableReason::NoClosedOutcomes,
                        }
                    } else {
                        MetricAvailability::Available {
                            value: cell.tp_count as f64 / count as f64 * 100.0,
                        }
                    },
                },
            }
        }
    };
    let daily = match input.daily {
        MetricAvailability::Unavailable { reason } => MetricAvailability::Unavailable { reason },
        MetricAvailability::Available { value: days } => {
            let touched_days = ((to_unix - 1 - origin_unix) / UTC_DAY_SECONDS + 1) as usize;
            if days.len() > EXPECTANCY_MAX_POINTS || days.len() > touched_days {
                return Err(ExpectancyError::TooManyPoints);
            }
            let mut previous = None;
            let mut points = Vec::with_capacity(days.len());
            let (mut total_tp, mut total_sl) = (0u64, 0u64);
            for day in days {
                if day.day_start_unix.rem_euclid(UTC_DAY_SECONDS) != 0 {
                    return Err(ExpectancyError::InvalidTimestamp);
                }
                if day.day_start_unix < origin_unix || day.day_start_unix >= to_unix {
                    return Err(ExpectancyError::DayOutsidePeriod);
                }
                if previous.is_some_and(|previous| previous >= day.day_start_unix) {
                    return Err(ExpectancyError::UnorderedOrDuplicateDay);
                }
                previous = Some(day.day_start_unix);
                let count = closed_count(day.tp_count, day.sl_count)?;
                if count > day.row_count {
                    return Err(ExpectancyError::InvalidCounts);
                }
                if day.mean_r.is_some() != (count > 0) {
                    return Err(ExpectancyError::InconsistentDailyMean);
                }
                if let Some(value) = day.mean_r {
                    finite(value)?;
                    // Counts belong to one fixed TP/SL pair. Validate every
                    // bucket before OLS: opposite corrupt daily means can
                    // cancel in the whole-cell weighted-mean check below.
                    let expected = finite(
                        (day.tp_count as f64 * input.source.tp_pct / input.source.sl_pct
                            - day.sl_count as f64)
                            / count as f64,
                    )?;
                    if (value - expected).abs() > 1e-10 * expected.abs().max(1.0) {
                        return Err(ExpectancyError::InconsistentDailyMean);
                    }
                }
                total_tp = total_tp
                    .checked_add(day.tp_count)
                    .ok_or(ExpectancyError::InvalidCounts)?;
                total_sl = total_sl
                    .checked_add(day.sl_count)
                    .ok_or(ExpectancyError::InvalidCounts)?;
                points.push(UtcDailyPoint {
                    date: utc_date(day.day_start_unix)?,
                    day_start_unix: day.day_start_unix,
                    x_utc_days: utc_day_coordinate(origin_unix, day.day_start_unix),
                    row_count: day.row_count,
                    tp_count: day.tp_count,
                    sl_count: day.sl_count,
                    mean_r: day.mean_r,
                });
            }
            if let MetricAvailability::Available { value: cell } = &selected_cell {
                if (total_tp, total_sl) != (cell.tp_count, cell.sl_count) {
                    return Err(ExpectancyError::InconsistentSelectedCell);
                }
                if let Some(raw) = cell.raw_r.as_ref() {
                    let closed = cell.closed_count as f64;
                    let weighted = points
                        .iter()
                        .filter_map(|p| {
                            p.mean_r
                                .map(|r| r * ((p.tp_count + p.sl_count) as f64 / closed))
                        })
                        .sum::<f64>();
                    finite(weighted)?;
                    if (weighted - raw).abs() > 1e-10 * raw.abs().max(1.0) {
                        return Err(ExpectancyError::InconsistentSelectedCell);
                    }
                }
            }
            MetricAvailability::Available { value: points }
        }
    };
    let trend = match &daily {
        MetricAvailability::Available { value } => fit_daily(value)?,
        _ => None,
    };
    let reason = if trend.is_some() {
        None
    } else {
        Some(match &daily {
            MetricAvailability::Unavailable { reason } => *reason,
            _ => ExpectancyUnavailableReason::InsufficientFiniteDays,
        })
    };
    let trend_delta_r = match trend {
        Some(trend) => MetricAvailability::Available {
            value: finite(trend.fitted_end_r - trend.fitted_start_r)?,
        },
        None => MetricAvailability::Unavailable {
            reason: reason.expect("unavailable trend has a reason"),
        },
    };
    let artifact = ExpectancyArtifact {
        calculation_version: EXPECTANCY_CALCULATION_VERSION,
        timezone: ExpectancyTimezone::UTC,
        origin_unix,
        from_unix,
        to_unix,
        period_days,
        source: input.source,
        selected_cell,
        matrix_mean_r: available_or(
            input.matrix_mean_r,
            ExpectancyUnavailableReason::MissingMatrixMean,
        )?,
        daily,
        trend,
        trend_unavailable_reason: reason,
        trend_delta_r,
    };
    artifact.check_byte_budget()?;
    Ok(artifact)
}

fn fit_daily(points: &[UtcDailyPoint]) -> Result<Option<UtcLinearTrend>, ExpectancyError> {
    let count = points.iter().filter(|p| p.mean_r.is_some()).count();
    if count < 2 {
        return Ok(None);
    }
    let n = count as f64;
    let mean_x = points
        .iter()
        .filter(|p| p.mean_r.is_some())
        .map(|p| p.x_utc_days / n)
        .sum::<f64>();
    let mean_y = finite(points.iter().filter_map(|p| p.mean_r.map(|y| y / n)).sum())?;
    let mut numerator = 0.0;
    let mut denominator = 0.0;
    for point in points {
        if let Some(y) = point.mean_r {
            let dx = point.x_utc_days - mean_x;
            numerator += dx * (y - mean_y);
            denominator += dx * dx;
        }
    }
    let slope = finite(numerator / denominator)?;
    let first = points
        .iter()
        .find(|p| p.mean_r.is_some())
        .expect("two finite days");
    let last = points
        .iter()
        .rev()
        .find(|p| p.mean_r.is_some())
        .expect("two finite days");
    let mut trend = UtcLinearTrend {
        valid_day_count: count,
        mean_x,
        mean_y,
        slope_r_per_day: slope,
        first_day_start_unix: first.day_start_unix,
        last_day_start_unix: last.day_start_unix,
        first_x: first.x_utc_days,
        last_x: last.x_utc_days,
        fitted_start_r: 0.0,
        fitted_end_r: 0.0,
    };
    trend.fitted_start_r = trend.evaluate(trend.first_x)?;
    trend.fitted_end_r = trend.evaluate(trend.last_x)?;
    Ok(Some(trend))
}

impl ExpectancyArtifact {
    /// Validate persisted/untrusted structs before use; no silent upgrading of
    /// browser-local/indexed v1 or modified coefficients in an immutable copy.
    pub fn validate(&self) -> Result<(), ExpectancyError> {
        if self.calculation_version != EXPECTANCY_CALCULATION_VERSION {
            return Err(ExpectancyError::UnsupportedVersion);
        }
        let selected_cell = self.selected_cell.as_ref().map(|cell| SelectedCellInput {
            raw_r: cell.raw_r.as_ref().copied(),
            tp_count: cell.tp_count,
            sl_count: cell.sl_count,
        });
        let daily = match &self.daily {
            MetricAvailability::Unavailable { reason } => {
                MetricAvailability::Unavailable { reason: *reason }
            }
            MetricAvailability::Available { value } => {
                if value.len() > EXPECTANCY_MAX_POINTS {
                    return Err(ExpectancyError::TooManyPoints);
                }
                MetricAvailability::Available {
                    value: value
                        .iter()
                        .map(|p| UtcDailyInput {
                            day_start_unix: p.day_start_unix,
                            row_count: p.row_count,
                            tp_count: p.tp_count,
                            sl_count: p.sl_count,
                            mean_r: p.mean_r,
                        })
                        .collect(),
                }
            }
        };
        let rebuilt = build_expectancy_artifact(ExpectancyInput {
            source: self.source.clone(),
            selected_cell,
            matrix_mean_r: self.matrix_mean_r.as_ref().copied(),
            daily,
        })?;
        if &rebuilt != self {
            return Err(ExpectancyError::InconsistentArtifact);
        }
        Ok(())
    }

    /// The caller freezes the resulting number once per NEW recommendation.
    /// No now/commit/entry/dispatch timestamp is accepted by this API.
    pub fn forecast_at_raw_signal(
        &self,
        raw_signal_at_unix: i64,
    ) -> Result<MetricAvailability<f64>, ExpectancyError> {
        self.validate()?;
        match self.trend {
            Some(trend) => Ok(MetricAvailability::Available {
                value: trend.evaluate(utc_day_coordinate(self.origin_unix, raw_signal_at_unix))?,
            }),
            None => Ok(MetricAvailability::Unavailable {
                reason: self.trend_unavailable_reason.expect("validated artifact"),
            }),
        }
    }
    pub fn image_key(&self, render_version: u32) -> ExpectancyImageKey {
        ExpectancyImageKey {
            study_id: self.source.study_id.clone(),
            derived_snapshot_id: self.source.derived_snapshot_id.clone(),
            input_snapshot_hash: self.source.provenance.snapshot_hash.clone(),
            tp_pct: self.source.tp_pct,
            sl_pct: self.source.sl_pct,
            calculation_version: self.calculation_version,
            timezone: self.timezone,
            render_version,
        }
    }
    pub fn from_json(bytes: &[u8]) -> Result<Self, ExpectancyError> {
        if bytes.len() > EXPECTANCY_MAX_ARTIFACT_BYTES {
            return Err(ExpectancyError::ArtifactTooLarge);
        }
        // Preserve arbitrary-precision decimal spellings until typed conversion.
        // The workspace intentionally does not enable serde_json/float_roundtrip;
        // its direct streaming f64 decoder can otherwise shift a saved rawR or
        // coefficient by an ULP. Do not change that global feature/science here.
        let value: serde_json::Value =
            serde_json::from_slice(bytes).map_err(|_| ExpectancyError::InvalidJson)?;
        let artifact: Self =
            serde_json::from_value(value).map_err(|_| ExpectancyError::InvalidJson)?;
        artifact.validate()?;
        Ok(artifact)
    }
    pub fn to_json(&self) -> Result<Vec<u8>, ExpectancyError> {
        self.validate()?;
        serde_json::to_vec(self).map_err(|_| ExpectancyError::InvalidJson)
    }
    fn check_byte_budget(&self) -> Result<(), ExpectancyError> {
        struct ByteBudget(usize);
        impl io::Write for ByteBudget {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.0 = self
                    .0
                    .checked_add(bytes.len())
                    .filter(|n| *n <= EXPECTANCY_MAX_ARTIFACT_BYTES)
                    .ok_or_else(|| io::Error::other("expectancy byte budget exceeded"))?;
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        serde_json::to_writer(ByteBudget(0), self).map_err(|_| ExpectancyError::ArtifactTooLarge)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (ExpectancyInput, serde_json::Value) {
        let value: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/expectancy_utc_v2.json")).unwrap();
        (
            serde_json::from_value(value["input"].clone()).unwrap(),
            value["expected"].clone(),
        )
    }
    fn close(a: f64, b: f64) {
        assert!((a - b).abs() < 1e-12, "{a} != {b}");
    }
    #[test]
    fn calendar_gap_equal_weight_golden_and_fractional_raw_forecast() {
        let (input, expected) = fixture();
        let artifact = build_expectancy_artifact(input).unwrap();
        assert_eq!(artifact.origin_unix, 1_793_491_200);
        close(artifact.period_days, 8.0);
        let points = artifact.daily.as_ref().unwrap();
        assert_eq!(
            points.iter().map(|p| p.x_utc_days).collect::<Vec<_>>(),
            [0.0, 1.0, 2.0, 7.0]
        );
        assert_eq!(points[1].mean_r, None);
        assert_eq!(points[1].row_count, 3);
        let trend = artifact.trend.unwrap();
        close(trend.mean_x, 3.0);
        close(trend.mean_y, 2.0 / 3.0);
        close(
            trend.slope_r_per_day,
            expected["slopeRPerDay"].as_f64().unwrap(),
        );
        close(trend.fitted_start_r, -1.0 / 39.0);
        close(trend.fitted_end_r, 62.0 / 39.0);
        close(*artifact.trend_delta_r.as_ref().unwrap(), 21.0 / 13.0);
        close(
            *artifact
                .forecast_at_raw_signal(1_794_290_400)
                .unwrap()
                .as_ref()
                .unwrap(),
            329.0 / 156.0,
        );
        close(
            *artifact
                .selected_cell
                .as_ref()
                .unwrap()
                .raw_r
                .as_ref()
                .unwrap(),
            83.0 / 45.0,
        );
        close(*artifact.matrix_mean_r.as_ref().unwrap(), 0.21);
        close(
            *artifact
                .selected_cell
                .as_ref()
                .unwrap()
                .win_rate_pct
                .as_ref()
                .unwrap(),
            32.0 / 45.0 * 100.0,
        );
        let json = artifact.to_json().unwrap();
        assert_eq!(artifact, ExpectancyArtifact::from_json(&json).unwrap());
    }
    #[test]
    fn frontend_january_golden_matches_calendar_fit_and_trade_weighted_metrics() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../tests/fixtures/expectancy_utc_january_v2.json"
        ))
        .unwrap();
        let input: ExpectancyInput = serde_json::from_value(fixture["input"].clone()).unwrap();
        let origin = 1_767_225_600;
        let artifact = build_expectancy_artifact(input).unwrap();
        assert_eq!(artifact.origin_unix, origin);
        let trend = artifact.trend.unwrap();
        close(trend.mean_x, 4.0);
        close(trend.mean_y, 2.0 / 3.0);
        close(trend.slope_r_per_day, 1.0 / 6.0);
        close(trend.fitted_start_r, 0.0);
        close(trend.fitted_end_r, 1.5);
        close(*artifact.trend_delta_r.as_ref().unwrap(), 1.5);
        close(
            *artifact
                .selected_cell
                .as_ref()
                .unwrap()
                .raw_r
                .as_ref()
                .unwrap(),
            0.8,
        );
        close(
            *artifact
                .selected_cell
                .as_ref()
                .unwrap()
                .win_rate_pct
                .as_ref()
                .unwrap(),
            60.0,
        );
        close(
            *artifact
                .forecast_at_raw_signal(origin + 12 * 86400 + 43200)
                .unwrap()
                .as_ref()
                .unwrap(),
            12.5 / 6.0,
        );
    }
    #[test]
    fn audit_tp6_sl1_golden_uses_fitted_delta_and_fractional_forecast() {
        let (mut input, _) = fixture();
        let origin = 1_767_225_600;
        input.source.tp_pct = 6.0;
        input.source.sl_pct = 1.0;
        input.source.provenance.from_unix = origin + 21600;
        input.source.provenance.to_unix = origin + 11 * 86400 + 43200;
        input.source.provenance.observation_cutoff = input.source.provenance.to_unix;
        input.selected_cell = Some(SelectedCellInput {
            raw_r: Some(1.8),
            tp_count: 6,
            sl_count: 9,
        });
        input.daily = MetricAvailability::Available {
            value: [(0, 1, 6, 0.0), (1, 1, 0, 6.0), (10, 4, 3, 3.0)]
                .into_iter()
                .map(|(x, tp, sl, r)| UtcDailyInput {
                    day_start_unix: origin + x * 86400,
                    row_count: tp + sl,
                    tp_count: tp,
                    sl_count: sl,
                    mean_r: Some(r),
                })
                .collect(),
        };
        let artifact = build_expectancy_artifact(input).unwrap();
        close(artifact.period_days, 11.25);
        close(artifact.trend.unwrap().slope_r_per_day, 9.0 / 182.0);
        close(*artifact.trend_delta_r.as_ref().unwrap(), 45.0 / 91.0);
        close(
            *artifact
                .selected_cell
                .as_ref()
                .unwrap()
                .win_rate_pct
                .as_ref()
                .unwrap(),
            40.0,
        );
        close(
            *artifact
                .forecast_at_raw_signal(origin + 12 * 86400 + 43200)
                .unwrap()
                .as_ref()
                .unwrap(),
            1251.0 / 364.0,
        );
    }
    #[test]
    fn utc_midnight_and_same_origin_do_not_depend_on_timezone_or_clock() {
        assert_eq!(utc_midnight_unix(-1).unwrap(), -86400);
        assert_eq!(utc_date(-1).unwrap(), "1969-12-31");
        assert_eq!(utc_midnight_unix(1_793_577_600).unwrap(), 1_793_577_600);
        let (input, _) = fixture();
        let artifact = build_expectancy_artifact(input).unwrap();
        let at_midnight = artifact
            .forecast_at_raw_signal(artifact.origin_unix + 9 * 86400)
            .unwrap();
        let at_noon = artifact
            .forecast_at_raw_signal(artifact.origin_unix + 9 * 86400 + 43200)
            .unwrap();
        close(
            *at_noon.as_ref().unwrap() - *at_midnight.as_ref().unwrap(),
            0.5 * 3.0 / 13.0,
        );
    }
    #[test]
    fn empty_single_and_all_null_days_are_explicitly_without_trend() {
        for days in [
            vec![],
            vec![UtcDailyInput {
                day_start_unix: 1_793_491_200,
                row_count: 1,
                tp_count: 1,
                sl_count: 0,
                mean_r: Some(3.0),
            }],
            vec![UtcDailyInput {
                day_start_unix: 1_793_491_200,
                row_count: 3,
                tp_count: 0,
                sl_count: 0,
                mean_r: None,
            }],
        ] {
            let (mut input, _) = fixture();
            input.selected_cell = None;
            input.daily = MetricAvailability::Available { value: days };
            let artifact = build_expectancy_artifact(input).unwrap();
            assert!(artifact.trend.is_none());
            assert_eq!(
                artifact.trend_delta_r,
                MetricAvailability::Unavailable {
                    reason: ExpectancyUnavailableReason::InsufficientFiniteDays
                }
            );
            assert_eq!(
                artifact.forecast_at_raw_signal(artifact.to_unix).unwrap(),
                artifact.trend_delta_r
            );
        }
    }
    #[test]
    fn missing_provenance_does_not_fabricate_series_but_keeps_saved_cell() {
        let (mut input, _) = fixture();
        input.daily = MetricAvailability::Unavailable {
            reason: ExpectancyUnavailableReason::UnprovenOutcomeProvenance,
        };
        let artifact = build_expectancy_artifact(input).unwrap();
        assert!(artifact.trend.is_none());
        assert_eq!(
            artifact.trend_unavailable_reason,
            Some(ExpectancyUnavailableReason::UnprovenOutcomeProvenance)
        );
        assert!(
            artifact
                .selected_cell
                .as_ref()
                .unwrap()
                .raw_r
                .as_ref()
                .is_some()
        );
    }
    #[test]
    fn zero_closed_and_missing_means_are_not_available_zeroes() {
        let (mut input, _) = fixture();
        input.selected_cell = Some(SelectedCellInput {
            raw_r: None,
            tp_count: 0,
            sl_count: 0,
        });
        input.matrix_mean_r = None;
        input.daily = MetricAvailability::Available { value: vec![] };
        let artifact = build_expectancy_artifact(input).unwrap();
        assert!(
            artifact
                .selected_cell
                .as_ref()
                .unwrap()
                .win_rate_pct
                .as_ref()
                .is_none()
        );
        assert!(artifact.matrix_mean_r.as_ref().is_none());
    }
    #[test]
    fn each_daily_mean_rejects_impossible_values_even_when_global_errors_cancel() {
        let (mut input, _) = fixture();
        input.source.tp_pct = 1.0;
        input.source.sl_pct = 1.0;
        input.selected_cell = Some(SelectedCellInput {
            raw_r: Some(0.0),
            tp_count: 1,
            sl_count: 1,
        });
        let origin = utc_midnight_unix(input.source.provenance.from_unix).unwrap();
        for (tp_mean, sl_mean) in [(100.0, -100.0), (0.0, 0.0), (-1.0, 1.0)] {
            input.daily = MetricAvailability::Available {
                value: vec![
                    UtcDailyInput {
                        day_start_unix: origin,
                        row_count: 1,
                        tp_count: 1,
                        sl_count: 0,
                        mean_r: Some(tp_mean),
                    },
                    UtcDailyInput {
                        day_start_unix: origin + 86400,
                        row_count: 1,
                        tp_count: 0,
                        sl_count: 1,
                        mean_r: Some(sl_mean),
                    },
                ],
            };
            assert_eq!(
                build_expectancy_artifact(input.clone()),
                Err(ExpectancyError::InconsistentDailyMean),
                "impossible per-day means must not cancel into a valid whole-cell rawR"
            );
            let mut without_selected = input.clone();
            without_selected.selected_cell = None;
            assert_eq!(
                build_expectancy_artifact(without_selected),
                Err(ExpectancyError::InconsistentDailyMean)
            );
        }
    }
    #[test]
    fn daily_mean_accepts_roundoff_without_rewriting_saved_means() {
        let (mut input, _) = fixture();
        let saved_raw_r = input.selected_cell.unwrap().raw_r.unwrap();
        let saved_matrix_mean_r = input.matrix_mean_r.unwrap();
        let rounded_daily_mean = 1.0 + 5e-11;
        if let MetricAvailability::Available { value } = &mut input.daily {
            value[0].mean_r = Some(rounded_daily_mean);
        }
        let artifact = build_expectancy_artifact(input).unwrap();
        assert_eq!(
            artifact.daily.as_ref().unwrap()[0].mean_r,
            Some(rounded_daily_mean)
        );
        assert_eq!(
            artifact
                .selected_cell
                .as_ref()
                .unwrap()
                .raw_r
                .as_ref()
                .unwrap()
                .to_bits(),
            saved_raw_r.to_bits()
        );
        assert_eq!(
            artifact.matrix_mean_r.as_ref().unwrap().to_bits(),
            saved_matrix_mean_r.to_bits()
        );
        assert_eq!(
            artifact,
            ExpectancyArtifact::from_json(&artifact.to_json().unwrap()).unwrap()
        );
    }
    #[test]
    fn invalid_finite_order_bounds_size_and_consistency_fail_closed() {
        let (baseline, _) = fixture();
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let mut input = baseline.clone();
            input.matrix_mean_r = Some(bad);
            assert_eq!(
                build_expectancy_artifact(input),
                Err(ExpectancyError::NonFiniteNumber)
            );
        }
        let mut input = baseline.clone();
        input.source.provenance.to_unix =
            input.source.provenance.from_unix + (EXPECTANCY_MAX_PERIOD_DAYS * 86400) + 1;
        assert_eq!(
            build_expectancy_artifact(input),
            Err(ExpectancyError::InvalidPeriod)
        );
        let mut input = baseline.clone();
        if let MetricAvailability::Available { value } = &mut input.daily {
            value.swap(0, 1);
        }
        assert_eq!(
            build_expectancy_artifact(input),
            Err(ExpectancyError::UnorderedOrDuplicateDay)
        );
        let mut input = baseline.clone();
        if let MetricAvailability::Available { value } = &mut input.daily {
            value[0].day_start_unix += 1;
        }
        assert_eq!(
            build_expectancy_artifact(input),
            Err(ExpectancyError::InvalidTimestamp)
        );
        let mut input = baseline;
        input.selected_cell.as_mut().unwrap().raw_r = Some(9.0);
        assert_eq!(
            build_expectancy_artifact(input),
            Err(ExpectancyError::InconsistentSelectedCell)
        );
        assert_eq!(
            ExpectancyArtifact::from_json(&vec![b' '; EXPECTANCY_MAX_ARTIFACT_BYTES + 1]),
            Err(ExpectancyError::ArtifactTooLarge)
        );
    }
    #[test]
    fn persisted_tampering_and_old_version_are_not_silently_upgraded() {
        let (input, _) = fixture();
        let mut artifact = build_expectancy_artifact(input).unwrap();
        artifact.calculation_version = 1;
        assert_eq!(
            artifact.validate(),
            Err(ExpectancyError::UnsupportedVersion)
        );
        artifact.calculation_version = EXPECTANCY_CALCULATION_VERSION;
        artifact.origin_unix += 86400;
        assert_eq!(
            artifact.validate(),
            Err(ExpectancyError::InconsistentArtifact)
        );
    }
    #[test]
    fn max_permitted_fractional_window_is_bounded_without_truncation() {
        let (mut input, _) = fixture();
        input.selected_cell = None;
        input.source.provenance.to_unix =
            input.source.provenance.from_unix + EXPECTANCY_MAX_PERIOD_DAYS * 86400;
        let origin = utc_midnight_unix(input.source.provenance.from_unix).unwrap();
        input.daily = MetricAvailability::Available {
            value: (0..EXPECTANCY_MAX_POINTS)
                .map(|i| UtcDailyInput {
                    day_start_unix: origin + i as i64 * 86400,
                    row_count: 0,
                    tp_count: 0,
                    sl_count: 0,
                    mean_r: None,
                })
                .collect(),
        };
        let artifact = build_expectancy_artifact(input.clone()).unwrap();
        assert_eq!(
            artifact.daily.as_ref().unwrap().len(),
            EXPECTANCY_MAX_POINTS
        );
        assert!(artifact.to_json().unwrap().len() < EXPECTANCY_MAX_ARTIFACT_BYTES);
        if let MetricAvailability::Available { value } = &mut input.daily {
            value.push(*value.last().unwrap());
        }
        assert_eq!(
            build_expectancy_artifact(input),
            Err(ExpectancyError::TooManyPoints)
        );
    }
}
