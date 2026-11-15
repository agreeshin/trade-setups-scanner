//! Manual analytics aggregation; never called by research scoring or live gates.
use setups_domain::trend::WeightedTrend;
pub use setups_domain::trend::{
    MAX_TREND_DAYS, TrendBendDays, TrendError, TrendInput, TrendMode, TrendSummary,
    fit_weighted_trend, trend_matrix_scope,
};
use std::collections::BTreeMap;

/// Caller supplies the same selected, non-price-filtered population as the UI.
/// Timestamp is signal close, never trade exit. Null returns still count as setups.
pub fn fit_daily_outcomes(
    outcomes: impl IntoIterator<Item = (i64, Option<f64>)>,
    bend: TrendBendDays,
) -> Result<Option<WeightedTrend>, TrendError> {
    let mut daily = BTreeMap::<i64, (u32, u32, f64)>::new();
    for (signal_close, value) in outcomes {
        let day = signal_close.div_euclid(86_400);
        let bucket = daily.entry(day).or_default();
        bucket.0 = bucket
            .0
            .checked_add(1)
            .ok_or(TrendError::NumericalFailure)?;
        if let Some(value) = value {
            if !value.is_finite() {
                return Err(TrendError::InvalidDayOrValue);
            }
            bucket.1 += 1;
            bucket.2 += value;
        }
        if daily.len() > MAX_TREND_DAYS {
            return Err(TrendError::TooManyDays);
        }
    }
    let points: Vec<_> = daily
        .into_iter()
        .map(|(day, (weight, closed, sum))| TrendInput {
            day: day as f64,
            value: (closed > 0).then(|| sum / f64::from(closed)),
            weight,
        })
        .collect();
    fit_weighted_trend(&points, bend)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn forecast_is_endpoint_of_signal_day_series_not_mean_or_exit_time() {
        let outcomes = [
            (0, Some(-1.0)),
            (86_400, Some(0.0)),
            (172_800, Some(1.0)),
            (259_200, Some(2.0)),
        ];
        let mean = outcomes.iter().map(|p| p.1.unwrap()).sum::<f64>() / 4.0;
        let fit = fit_daily_outcomes(outcomes, TrendBendDays::Linear)
            .unwrap()
            .unwrap();
        assert_eq!(mean, 0.5);
        assert_eq!(fit.endpoint().value, 2.0);
        assert_eq!(fit.endpoint().day, 3.0);
        assert_eq!(fit.endpoint().se, Some(0.0));
    }
    #[test]
    fn setup_weight_includes_null_rows_but_null_days_are_not_zeroes() {
        let outcomes = [
            (0, Some(0.0)),
            (86_400, Some(3.0)),
            (86_400, None),
            (86_400, None),
            (172_800, Some(0.0)),
            (259_200, None),
        ];
        let fit = fit_daily_outcomes(outcomes, TrendBendDays::Linear)
            .unwrap()
            .unwrap();
        assert!((fit.endpoint().value - 1.8).abs() < 1e-10);
        assert_eq!(fit.endpoint().day, 2.0);
        assert!(
            fit_daily_outcomes([(0, Some(1.0))], TrendBendDays::Seven)
                .unwrap()
                .is_none()
        );
    }
}
