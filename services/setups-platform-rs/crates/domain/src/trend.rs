//! Manual analytics only; immutable research/recommendation OLS is unchanged.
//! One fit owns O(days + coefficients) memory (< 1 MiB at the admitted bounds).
//! No cache, clock, I/O or extrapolation. Weights are setup counts, not returns.
use serde::{Deserialize, Serialize};

pub const MAX_TREND_DAYS: usize = 3661;
pub const TREND_CI_K: f64 = 1.96;
const LAMBDA: f64 = 0.1;
const BAND: usize = 4; // cubic B-splines overlap at most four coefficients

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TrendBendDays {
    #[serde(rename = "7")]
    Seven,
    #[serde(rename = "14")]
    Fourteen,
    #[serde(rename = "30")]
    Thirty,
    #[default]
    #[serde(rename = "linear")]
    Linear,
}
impl TrendBendDays {
    pub fn key(self) -> &'static str {
        match self {
            Self::Seven => "7",
            Self::Fourteen => "14",
            Self::Thirty => "30",
            Self::Linear => "linear",
        }
    }
    fn spacing(self) -> Option<f64> {
        match self {
            Self::Seven => Some(7.0),
            Self::Fourteen => Some(14.0),
            Self::Thirty => Some(30.0),
            Self::Linear => None,
        }
    }
}
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TrendMode {
    #[default]
    Mean,
    Forecast,
}
/// Mean keys/payloads deliberately remain byte-compatible with pre-trend caches.
pub fn trend_matrix_scope(scope: &str, mode: TrendMode, bend: TrendBendDays) -> String {
    match mode {
        TrendMode::Mean => scope.to_owned(),
        TrendMode::Forecast => format!("{scope}:trend:v1:forecast:{}", bend.key()),
    }
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TrendInput {
    pub day: f64,
    pub value: Option<f64>,
    pub weight: u32,
}
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TrendPoint {
    pub day: f64,
    pub value: f64,
    pub se: Option<f64>,
}
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TrendSummary {
    pub method: &'static str,
    pub start: TrendPoint,
    pub end: TrendPoint,
    pub endpoint_se: Option<f64>,
    pub curve: Vec<TrendPoint>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrendError {
    TooManyDays,
    InvalidDayOrValue,
    UnorderedDays,
    PeriodTooLong,
    NumericalFailure,
}
impl std::fmt::Display for TrendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "trend: {self:?}")
    }
}
impl std::error::Error for TrendError {}

// Symmetric lower band, and then its Cholesky factor. All indexing is local to
// this fit; neither a dense inverse nor an O(days²) smoother is retained.
#[derive(Clone)]
struct Banded(Vec<[f64; BAND]>);
impl Banded {
    fn get(&self, i: usize, j: usize) -> f64 {
        let (i, j) = (i.max(j), i.min(j));
        if i - j < BAND { self.0[i][i - j] } else { 0.0 }
    }
    fn add(&mut self, i: usize, j: usize, value: f64) {
        let (i, j) = (i.max(j), i.min(j));
        self.0[i][i - j] += value;
    }
    fn cholesky(mut self) -> Option<Self> {
        for i in 0..self.0.len() {
            for j in i.saturating_sub(BAND - 1)..=i {
                let sum = (i.saturating_sub(BAND - 1)..j)
                    .map(|k| self.get(i, k) * self.get(j, k))
                    .sum::<f64>();
                let value = self.get(i, j) - sum;
                let factor = if i == j {
                    if !value.is_finite() || value <= 1e-12 {
                        return None;
                    }
                    value.sqrt()
                } else {
                    value / self.get(j, j)
                };
                self.0[i][i - j] = factor;
            }
        }
        Some(self)
    }
    fn solve(&self, rhs: &[f64]) -> Vec<f64> {
        let mut x = rhs.to_vec();
        for i in 0..x.len() {
            x[i] = (x[i]
                - (i.saturating_sub(BAND - 1)..i)
                    .map(|j| self.get(i, j) * x[j])
                    .sum::<f64>())
                / self.get(i, i);
        }
        for i in (0..x.len()).rev() {
            x[i] = (x[i]
                - (i + 1..(i + BAND).min(x.len()))
                    .map(|j| self.get(j, i) * x[j])
                    .sum::<f64>())
                / self.get(i, i);
        }
        x
    }
    fn quadratic(&self, x: &[f64]) -> f64 {
        (0..x.len())
            .map(|i| {
                x[i] * (self.get(i, i) * x[i]
                    + 2.0
                        * (i.saturating_sub(BAND - 1)..i)
                            .map(|j| self.get(i, j) * x[j])
                            .sum::<f64>())
            })
            .sum()
    }
}
// Cox–de Boor restricted to the four active functions. Open/clamped endpoints
// include the last observation exactly, with no epsilon or out-of-data sample.
fn basis(knots: &[f64], x: f64) -> (usize, [f64; BAND]) {
    let n = knots.len() - BAND;
    let span = if x >= knots[n] {
        n - 1
    } else {
        knots.partition_point(|k| *k <= x).saturating_sub(1).max(3)
    };
    let mut b = [1.0, 0.0, 0.0, 0.0];
    for degree in 1..BAND {
        let mut saved = 0.0;
        for r in 0..degree {
            let left = x - knots[span + 1 - degree + r];
            let right = knots[span + r + 1] - x;
            let term = b[r] / (left + right);
            b[r] = saved + right * term;
            saved = left * term;
        }
        b[degree] = saved;
    }
    (span - 3, b)
}
fn dot_at(coefficients: &[f64], offset: usize, b: &[f64; BAND]) -> f64 {
    b.iter()
        .enumerate()
        .map(|(j, value)| value * coefficients[offset + j])
        .sum()
}
enum Model {
    Linear {
        mean_x: f64,
        mean_y: f64,
        slope: f64,
        weight: f64,
        xx: f64,
    },
    Spline {
        knots: Vec<f64>,
        coefficients: Vec<f64>,
        factor: Banded,
        gram: Banded,
    },
}
pub struct WeightedTrend {
    first: f64,
    last: f64,
    variance: Option<f64>,
    model: Model,
}
impl WeightedTrend {
    /// None outside the observed domain: this API cannot accidentally extrapolate.
    pub fn evaluate(&self, day: f64) -> Option<TrendPoint> {
        if !day.is_finite() || day < self.first || day > self.last {
            return None;
        }
        let (value, leverage) = match &self.model {
            Model::Linear {
                mean_x,
                mean_y,
                slope,
                weight,
                xx,
            } => (
                mean_y + slope * (day - mean_x),
                1.0 / weight + (day - mean_x).powi(2) / xx,
            ),
            Model::Spline {
                knots,
                coefficients,
                factor,
                gram,
            } => {
                let (offset, b) = basis(knots, day);
                let mut rhs = vec![0.0; coefficients.len()];
                rhs[offset..offset + BAND].copy_from_slice(&b);
                let solved = factor.solve(&rhs);
                (
                    dot_at(coefficients, offset, &b),
                    gram.quadratic(&solved).max(0.0),
                )
            }
        };
        Some(TrendPoint {
            day,
            value,
            se: self.variance.map(|v| (v * leverage).sqrt()),
        })
    }
    pub fn endpoint(&self) -> TrendPoint {
        self.evaluate(self.last)
            .expect("last observation belongs to fit domain")
    }
    pub fn summary(&self) -> TrendSummary {
        let end = self.endpoint();
        // <= 257 samples, at most quarter-day spacing for shorter periods.
        let steps = (((self.last - self.first) * 4.0).ceil() as usize).clamp(1, 256);
        let curve = (0..=steps)
            .map(|i| {
                self.evaluate(if i == steps {
                    self.last
                } else {
                    self.first + (self.last - self.first) * i as f64 / steps as f64
                })
                .expect("sample inside observed domain")
            })
            .collect();
        TrendSummary {
            method: match self.model {
                Model::Linear { .. } => "linear",
                Model::Spline { .. } => "p_spline",
            },
            start: self
                .evaluate(self.first)
                .expect("first observation in domain"),
            end,
            endpoint_se: end.se,
            curve,
        }
    }
}

pub fn fit_weighted_trend(
    input: &[TrendInput],
    bend: TrendBendDays,
) -> Result<Option<WeightedTrend>, TrendError> {
    if input.len() > MAX_TREND_DAYS {
        return Err(TrendError::TooManyDays);
    }
    for (i, point) in input.iter().enumerate() {
        if !point.day.is_finite()
            || point.day.fract() != 0.0
            || point.day.abs() > 100_000_000.0
            || point
                .value
                .is_some_and(|y| !y.is_finite() || y.abs() > 1e12)
        {
            return Err(TrendError::InvalidDayOrValue);
        }
        if i > 0 && point.day <= input[i - 1].day {
            return Err(TrendError::UnorderedDays);
        }
    }
    if input
        .last()
        .zip(input.first())
        .is_some_and(|(last, first)| last.day - first.day > 3660.0)
    {
        return Err(TrendError::PeriodTooLong);
    }
    let mut points: Vec<_> = input
        .iter()
        .filter(|p| p.weight > 0 && p.value.is_some())
        .map(|p| (p.day, p.value.expect("finite point"), f64::from(p.weight)))
        .collect();
    if points.len() < 2 {
        return Ok(None);
    }
    let count = points.len() as f64;
    let weight_scale = count / points.iter().map(|p| p.2).sum::<f64>();
    for p in &mut points {
        p.2 *= weight_scale;
    }
    let first = points[0].0;
    let last = points[points.len() - 1].0;
    let mean_x = points.iter().map(|p| p.0 * p.2).sum::<f64>() / count;
    let mean_y = points.iter().map(|p| p.1 * p.2).sum::<f64>() / count;
    let xx = points
        .iter()
        .map(|p| p.2 * (p.0 - mean_x).powi(2))
        .sum::<f64>();
    let slope = points
        .iter()
        .map(|p| p.2 * (p.0 - mean_x) * (p.1 - mean_y))
        .sum::<f64>()
        / xx;
    let linear = || {
        let sse = points
            .iter()
            .map(|p| p.2 * (p.1 - mean_y - slope * (p.0 - mean_x)).powi(2))
            .sum::<f64>();
        WeightedTrend {
            first,
            last,
            variance: (points.len() > 2).then(|| sse / (count - 2.0)),
            model: Model::Linear {
                mean_x,
                mean_y,
                slope,
                weight: count,
                xx,
            },
        }
    };
    if !xx.is_finite() || xx <= 0.0 || !slope.is_finite() {
        return Err(TrendError::NumericalFailure);
    }
    let Some(spacing) = bend.spacing().filter(|_| points.len() >= 4) else {
        return Ok(Some(linear()));
    };
    let mut knots = vec![first; BAND];
    let mut knot = first + spacing;
    while knot < last {
        knots.push(knot);
        knot += spacing;
    }
    knots.extend([last; BAND]);
    let n = knots.len() - BAND;
    let mut gram = Banded(vec![[0.0; BAND]; n]);
    let mut rhs = vec![0.0; n];
    let rows: Vec<_> = points.iter().map(|p| basis(&knots, p.0)).collect();
    for ((_, y, w), (offset, b)) in points.iter().zip(&rows) {
        for j in 0..BAND {
            rhs[offset + j] += w * b[j] * y;
            for k in 0..=j {
                gram.add(offset + j, offset + k, w * b[j] * b[k]);
            }
        }
    }
    let mut penalized = gram.clone();
    for i in 0..n - 2 {
        let d = [1.0, -2.0, 1.0];
        for j in 0..3 {
            for k in 0..=j {
                penalized.add(i + j, i + k, LAMBDA * d[j] * d[k]);
            }
        }
    }
    let Some(factor) = penalized.cholesky() else {
        return Ok(Some(linear()));
    };
    let coefficients = factor.solve(&rhs);
    if coefficients.iter().any(|c| !c.is_finite()) {
        return Ok(Some(linear()));
    }
    let mut sse = 0.0;
    let mut edf = 0.0;
    for ((_, y, w), (offset, b)) in points.iter().zip(&rows) {
        sse += w * (y - dot_at(&coefficients, *offset, b)).powi(2);
        let mut unit = vec![0.0; n];
        unit[*offset..offset + BAND].copy_from_slice(b);
        edf += w * dot_at(&factor.solve(&unit), *offset, b);
    }
    Ok(Some(WeightedTrend {
        first,
        last,
        variance: (count - edf > 1e-8).then(|| sse / (count - edf)),
        model: Model::Spline {
            knots,
            coefficients,
            factor,
            gram,
        },
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn series(n: usize, f: impl Fn(f64) -> f64) -> Vec<TrendInput> {
        (0..n)
            .map(|i| TrendInput {
                day: i as f64,
                value: Some(f(i as f64)),
                weight: 1,
            })
            .collect()
    }
    #[test]
    fn linear_equal_weights_match_existing_ols_and_infinity_is_weighted() {
        // The existing calendar-OLS golden has gaps, non-collinear values and
        // fitted endpoints different from the observed first/last outcomes.
        let golden = [(0.0, 1.0), (3.0, -1.0), (9.0, 2.0)].map(|(day, value)| TrendInput {
            day,
            value: Some(value),
            weight: 1,
        });
        let old = fit_weighted_trend(&golden, TrendBendDays::Linear)
            .unwrap()
            .unwrap()
            .summary();
        assert!(old.start.value.abs() < 1e-12);
        assert!((old.end.value - 1.5).abs() < 1e-12);
        let points = series(10, |x| 2.0 + 3.0 * x);
        let fit = fit_weighted_trend(&points, TrendBendDays::Linear)
            .unwrap()
            .unwrap();
        assert!((fit.endpoint().value - 29.0).abs() < 1e-10);
        let mut weighted = series(3, |x| [0.0, 3.0, 0.0][x as usize]);
        weighted[1].weight = 10;
        let fit = fit_weighted_trend(&weighted, TrendBendDays::Linear)
            .unwrap()
            .unwrap();
        assert!((fit.endpoint().value - 2.5).abs() < 1e-10);
        assert_eq!(fit.summary().method, "linear");
    }
    #[test]
    fn knot_spacing_changes_curvature_and_endpoint_is_fitted_not_extrapolated() {
        let points = series(91, |x| (x / 6.0).sin() + x / 90.0);
        let a = fit_weighted_trend(&points, TrendBendDays::Seven)
            .unwrap()
            .unwrap();
        let b = fit_weighted_trend(&points, TrendBendDays::Thirty)
            .unwrap()
            .unwrap();
        assert_eq!(a.summary().method, "p_spline");
        assert!((a.evaluate(20.0).unwrap().value - b.evaluate(20.0).unwrap().value).abs() > 0.1);
        assert_eq!(a.endpoint(), a.evaluate(90.0).unwrap());
        assert_eq!(a.summary().curve.last(), Some(&a.endpoint()));
        assert!(a.evaluate(90.01).is_none());
        assert!(a.evaluate(-0.01).is_none());
        assert_ne!(a.endpoint().value, points.last().unwrap().value.unwrap());
    }
    #[test]
    fn zero_weight_null_days_and_few_points_fallback_are_explicit() {
        let mut points = series(5, |x| 1.0 + x);
        points[1].weight = 0;
        points[1].value = Some(10000.0);
        points[3].value = None;
        let fit = fit_weighted_trend(&points, TrendBendDays::Seven)
            .unwrap()
            .unwrap();
        assert_eq!(fit.summary().method, "linear");
        assert!((fit.endpoint().value - 5.0).abs() < 1e-10);
        assert!(
            fit_weighted_trend(&points[..2], TrendBendDays::Seven)
                .unwrap()
                .is_none()
        );
        let two = fit_weighted_trend(&series(2, |x| x), TrendBendDays::Seven)
            .unwrap()
            .unwrap();
        assert!(two.endpoint().se.is_none());
    }
    #[test]
    fn ci_scales_with_noise_and_band_width_and_weights_are_scale_invariant() {
        for bend in [TrendBendDays::Linear, TrendBendDays::Seven] {
            let points = series(61, |x| (x / 5.0).sin());
            let fit = fit_weighted_trend(&points, bend).unwrap().unwrap();
            let mut scaled = points.clone();
            for p in &mut scaled {
                p.value = p.value.map(|y| y * 3.0);
                p.weight *= 10;
            }
            let other = fit_weighted_trend(&scaled, bend).unwrap().unwrap();
            let se = fit.endpoint().se.unwrap();
            assert!(se > 0.0 && se.is_finite());
            assert!((other.endpoint().se.unwrap() - se * 3.0).abs() < 1e-8);
            assert!(TREND_CI_K * se > se);
            assert!(
                fit.summary()
                    .curve
                    .iter()
                    .all(|p| p.se.is_some_and(|v| v.is_finite() && v >= 0.0))
            );
        }
    }
    #[test]
    fn basis_is_partition_of_unity_and_banded_solver_matches_equations() {
        let knots = [0., 0., 0., 0., 7., 14., 20., 20., 20., 20.];
        for x in [0., 0.1, 7., 12., 19.9, 20.] {
            let (_, b) = basis(&knots, x);
            assert!((b.iter().sum::<f64>() - 1.0).abs() < 1e-12);
            assert!(b.iter().all(|v| *v >= 0.0));
        }
        let matrix = Banded(vec![
            [5., 0., 0., 0.],
            [6., 1., 0., 0.],
            [7., 2., 1., 0.],
            [8., 3., 1., 0.],
        ]);
        let rhs = [1., 2., 3., 4.];
        let x = matrix.clone().cholesky().unwrap().solve(&rhs);
        for (i, expected) in rhs.iter().enumerate() {
            assert!(
                ((0..4).map(|j| matrix.get(i, j) * x[j]).sum::<f64>() - expected).abs() < 1e-12
            );
        }
    }
    #[test]
    fn validation_and_memory_bounds() {
        assert!(matches!(
            fit_weighted_trend(&series(MAX_TREND_DAYS + 1, |_| 1.0), TrendBendDays::Seven),
            Err(TrendError::TooManyDays)
        ));
        let mut points = series(4, |x| x);
        points[2].day = 1.0;
        assert!(matches!(
            fit_weighted_trend(&points, TrendBendDays::Seven),
            Err(TrendError::UnorderedDays)
        ));
        points[2].day = f64::NAN;
        assert!(matches!(
            fit_weighted_trend(&points, TrendBendDays::Seven),
            Err(TrendError::InvalidDayOrValue)
        ));
        let fit = fit_weighted_trend(
            &series(MAX_TREND_DAYS, |x| (x / 50.).sin()),
            TrendBendDays::Seven,
        )
        .unwrap()
        .unwrap();
        assert_eq!(fit.summary().curve.len(), 257);
    }
    #[test]
    fn mean_cache_is_identical_and_forecast_modes_are_disjoint() {
        let old = "batch:42:100:10:20:30";
        let mut keys = std::collections::HashSet::new();
        for bend in [
            TrendBendDays::Seven,
            TrendBendDays::Fourteen,
            TrendBendDays::Thirty,
            TrendBendDays::Linear,
        ] {
            assert_eq!(trend_matrix_scope(old, TrendMode::Mean, bend), old);
            assert!(keys.insert(trend_matrix_scope(old, TrendMode::Forecast, bend)));
        }
        assert!(!keys.contains(old));
    }
}
