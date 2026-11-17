//! Evaluation-owned daily forecast scratch. Mean paths do not instantiate it.
use crate::{MatrixCell, StudyError, StudyLimits, StudyRequest, TrendMode};
use crate::evaluation::{Control, limit};
use setups_domain::trend::{MAX_TREND_DAYS, TrendDesign, TrendInput};
use setups_domain::forecast::{ForecastError, ForecastMethod, ForecastRecipe, ProbabilityAccumulator, ProbabilityDay, decision_r};
use std::{collections::BTreeMap, mem::size_of};

#[derive(Default, Clone, Copy)]
struct DailyCell { sum_r: f64, closed: u32 }

pub(crate) struct DailyMatrix {
    origin_day: i64,
    days: usize,
    rows: Vec<u32>,
    cells: Vec<DailyCell>,
    probability: Vec<ProbabilityAccumulator>,
    probability_error: Option<ForecastError>,
}
impl DailyMatrix {
    /// Includes one shared design, solver scratch, compact input and grouping
    /// metadata; none survives the evaluation or a cancelled/failed call.
    pub fn working_bytes(spec: &StudyRequest) -> Result<usize, StudyError> {
        if spec.trend_mode == TrendMode::Mean { return Ok(0); }
        let days = Self::day_count(spec)?;
        if spec.forecast.is_some_and(|r| r.method() == ForecastMethod::DecayedProbability) {
            return Ok(spec.tp_levels.len().saturating_mul(spec.sl_levels.len())
                .saturating_mul(size_of::<ProbabilityAccumulator>()).saturating_add(size_of::<Self>()));
        }
        Ok(days.saturating_mul(spec.tp_levels.len() * spec.sl_levels.len())
            .saturating_mul(size_of::<DailyCell>())
            .saturating_add(days * size_of::<u32>())
            .saturating_add(1024 * 1024))
    }
    fn day_count(spec: &StudyRequest) -> Result<usize, StudyError> {
        let days = (spec.to_unix - 1).div_euclid(86400) - spec.from_unix.div_euclid(86400) + 1;
        if !(1..=MAX_TREND_DAYS as i64).contains(&days) {
            return Err(StudyError::InvalidRequest("forecast period exceeds 3661 UTC days".into()));
        }
        Ok(days as usize)
    }
    #[cfg(test)]
    pub fn new(spec: &StudyRequest, limits: &StudyLimits) -> Result<Self, StudyError> {
        Self::with_cutoff(spec, limits, spec.to_unix)
    }
    pub fn with_cutoff(spec: &StudyRequest, limits: &StudyLimits, cutoff: i64) -> Result<Self, StudyError> {
        // Every counter is bounded by the already enforced candidate setup cap.
        limit("forecast_setup_counter", limits.max_setups_per_candidate, u32::MAX as usize)?;
        let days = Self::day_count(spec)?;
        if let Some(recipe) = spec.forecast.filter(|r| r.method() == ForecastMethod::DecayedProbability) {
            let mut probability=Vec::with_capacity(spec.tp_levels.len()*spec.sl_levels.len());
            for tp in &spec.tp_levels { for sl in &spec.sl_levels {
                probability.push(ProbabilityAccumulator::new(recipe,tp/sl,cutoff)
                    .map_err(|e|StudyError::Execution(e.to_string()))?);
            }}
            return Ok(Self {origin_day:spec.from_unix.div_euclid(86400),days,
                rows:Vec::new(),cells:Vec::new(),probability,probability_error:None});
        }
        Ok(Self { origin_day: spec.from_unix.div_euclid(86400), days,
            rows: vec![0; days], cells: vec![DailyCell::default(); days * spec.tp_levels.len() * spec.sl_levels.len()],
            probability:Vec::new(),probability_error:None })
    }
    pub fn record_setup(&mut self, signal_close: i64) -> usize {
        let day = (signal_close.div_euclid(86400) - self.origin_day) as usize;
        if !self.rows.is_empty() { self.rows[day] += 1; }
        day
    }
    pub fn record_outcome(&mut self, cell: usize, day: usize, value: f64) {
        if let Some(acc) = self.probability.get_mut(cell) {
            // Production callers supply exactly +q for TP and -1 for SL.
            // Never derive counts from rounded means or eligible row counts.
            if self.probability_error.is_none() {
                let outcome = if value.is_finite() && value > 0.0 { Some((1,0)) }
                    else if value == -1.0 { Some((0,1)) } else { None };
                self.probability_error = match outcome {
                    Some((tp_count,sl_count)) => acc.add_day(ProbabilityDay {
                        day_start_unix:(self.origin_day+day as i64)*86400,tp_count,sl_count }).err(),
                    None => Some(ForecastError::InvalidPayoff),
                };
            }
            return;
        }
        let bucket = &mut self.cells[cell * self.days + day];
        bucket.closed += 1;
        bucket.sum_r += value;
    }
    pub fn apply(&self, spec: &StudyRequest, matrix: &mut [MatrixCell], control: &Control<'_>) -> Result<bool, StudyError> {
        if let Some(error) = self.probability_error { return Err(StudyError::Execution(error.to_string())); }
        if !self.probability.is_empty() {
            if matrix.len()!=self.probability.len() {
                return Err(StudyError::Execution("probability matrix differs from fixed original axes".into()));
            }
            for (cell, acc) in matrix.iter_mut().zip(&self.probability) {
                if control.stopped() { return Ok(false); }
                let result=acc.finish().map_err(|e| StudyError::Execution(e.to_string()))?;
                if let Some(value)=result.as_ref()
                    && (value.tp_count != cell.tp_count as u64 || value.sl_count != cell.sl_count as u64) {
                    return Err(StudyError::Execution("probability counts differ from closed matrix population".into()));
                }
                cell.mean_risk_expectancy=cell.risk_expectancy;
                cell.forecast_risk_expectancy=result.as_ref().map(|p|p.forecast_r);
                cell.risk_expectancy=decision_r(spec.trend_mode,cell.mean_risk_expectancy,
                    cell.forecast_risk_expectancy,spec.trend_blend_lambda)
                    .map_err(|e|StudyError::Execution(e.to_string()))?;
            }
            return Ok(true);
        }
        let mut groups = BTreeMap::<(usize, usize), Vec<usize>>::new();
        for (index, cell) in matrix.iter_mut().enumerate() {
            if control.stopped() { return Ok(false); }
            cell.mean_risk_expectancy = cell.risk_expectancy;
            cell.risk_expectancy = None;
            let daily = &self.cells[index * self.days..(index + 1) * self.days];
            if let (Some(first), Some(last)) = (daily.iter().position(|b| b.closed > 0), daily.iter().rposition(|b| b.closed > 0))
                && first != last {
                groups.entry((first, last)).or_default().push(index);
            }
        }
        for ((first, last), cells) in groups {
            if control.stopped() { return Ok(false); }
            // Usually one group. A cell with a null end must retain its OWN
            // last finite day, exactly like Analytics; do not extend its fit.
            let design = TrendDesign::new((self.origin_day + first as i64) as f64,
                (self.origin_day + last as i64) as f64, effective_bend(spec))
                .map_err(|e| StudyError::Execution(e.to_string()))?;
            let mut points = Vec::with_capacity(last - first + 1);
            for index in cells {
                if control.stopped() { return Ok(false); }
                points.clear();
                let mut period_sum = 0.0;
                let mut period_closed = 0_u64;
                for day in first..=last {
                    let bucket = self.cells[index * self.days + day];
                    period_sum += bucket.sum_r;
                    period_closed += u64::from(bucket.closed);
                    points.push(TrendInput { day: (self.origin_day + day as i64) as f64,
                        value: (bucket.closed > 0).then(|| bucket.sum_r / f64::from(bucket.closed)), weight: self.rows[day] });
                }
                let forecast = design.endpoint(&points)
                    .map_err(|e| StudyError::Execution(e.to_string()))?;
                if spec.trend_mode == TrendMode::Blend {
                    let mean = if spec.forecast.is_some() {
                        matrix[index].mean_risk_expectancy.expect("closed explicit cell has period mean")
                    } else { period_sum / period_closed as f64 };
                    matrix[index].mean_risk_expectancy = Some(mean);
                    matrix[index].forecast_risk_expectancy = forecast;
                    matrix[index].risk_expectancy = forecast.map(|forecast| spec.trend_blend_lambda.combine(mean, forecast));
                } else {
                    matrix[index].risk_expectancy = forecast;
                    if spec.forecast.is_some() { matrix[index].forecast_risk_expectancy=forecast; }
                }
            }
        }
        Ok(true)
    }
}

pub(crate) fn effective_bend(spec:&StudyRequest) -> crate::TrendBendDays {
    match spec.forecast {
        Some(ForecastRecipe::Trend {bend_days,..}) => bend_days,
        Some(ForecastRecipe::DecayedProbability {..}) => crate::TrendBendDays::Linear,
        None => spec.trend_bend_days,
    }
}

/// Original period mean, even when riskExpectancy holds a forecast. Callers
/// retain meanExpectancy/rawR semantics rather than relabeling a forecast mean.
pub fn score_period_mean(matrix: &[MatrixCell], minimum: usize) -> Option<f64> {
    if matrix.is_empty() || minimum == 0 { return None; }
    let mut sum = 0.0;
    for cell in matrix {
        if cell.closed_count < minimum { return None; }
        sum += cell.mean_risk_expectancy.or(cell.risk_expectancy).filter(|v| v.is_finite())?;
    }
    let mean = sum / matrix.len() as f64;
    mean.is_finite().then_some(mean)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CancelToken, TrendBendDays, evaluation::empty_matrix};
    use serde_json::json;
    use std::{sync::atomic::AtomicUsize, time::{Duration, Instant}};

    #[test]
    fn probability_owns_only_scalars_and_prior_does_not_replace_real_closed_support() {
        let spec:StudyRequest=serde_json::from_value(json!({
            "algorithmId":"price_volume_spike","algorithmVersion":"1.1.1","algorithmParams":{},
            "analyticsParams":crate::AnalyticsParams::default(),"instruments":[],"fromUnix":0,"toUnix":2*86400,
            "direction":"short","seed":42,"tpLevels":[10],"slLevels":[4],"trendMode":"forecast",
            "forecast":{"method":"decayed_probability","version":1,"halfLifeDays":1,"kappa":2,"priorPolicy":"gross_break_even"}
        })).unwrap();
        let limits=StudyLimits::default();let mut daily=DailyMatrix::new(&spec,&limits).unwrap();
        assert!(daily.rows.is_empty() && daily.cells.is_empty());assert_eq!(daily.probability.len(),1);
        for _ in 0..10000 {daily.record_setup(0);}
        for _ in 0..4 {let day=daily.record_setup(0);daily.record_outcome(0,day,2.5);}
        for _ in 0..2 {let day=daily.record_setup(86400);daily.record_outcome(0,day,-1.0);}
        let cancel=CancelToken::new();let passes=AtomicUsize::new(0);
        let control=Control{cancel:&cancel,deadline:Instant::now()+Duration::from_secs(30),detector_passes:&passes};
        let mut matrix=empty_matrix(&spec);matrix[0].risk_expectancy=Some(8.0/6.0);
        matrix[0].tp_count=4;matrix[0].sl_count=2;matrix[0].closed_count=6;
        assert!(daily.apply(&spec,&mut matrix,&control).unwrap());
        assert_eq!(matrix[0].forecast_risk_expectancy,Some(0.375));
        assert_eq!(matrix[0].risk_expectancy,Some(0.375));
        assert_eq!(matrix[0].mean_risk_expectancy,Some(8.0/6.0));
        assert_eq!(crate::score_matrix(&matrix,7),None);
        assert_eq!(crate::score_matrix(&matrix,6),Some(0.375));
        let mut large=spec.clone();large.to_unix=3660*86400;large.tp_levels=(1..=64).map(f64::from).collect();large.sl_levels=large.tp_levels.clone();
        assert!(DailyMatrix::working_bytes(&large).unwrap()<4*1024*1024);
        cancel.cancel();assert!(!daily.apply(&spec,&mut matrix,&control).unwrap());
    }

    #[test]
    fn blend_uses_the_same_daily_buckets_and_closed_count_weighted_mean() {
        let mut spec: StudyRequest = serde_json::from_value(json!({
            "algorithmId":"price_volume_spike","algorithmVersion":"1.1.1","algorithmParams":{},
            "analyticsParams":crate::AnalyticsParams::default(),"instruments":[],"fromUnix":0,"toUnix":4*86400,
            "direction":"short","seed":42,"tpLevels":[4],"slLevels":[4],"trendMode":"forecast","trendBendDays":"7"
        })).unwrap();
        let limits=StudyLimits::default(); let mut daily=DailyMatrix::new(&spec,&limits).unwrap();
        for (day, outcomes) in [(0,vec![Some(-1.0),Some(1.0)]),(1,vec![Some(4.0)]),(2,vec![Some(-1.0),None]),(3,vec![Some(2.0)])] {
            for value in outcomes {let index=daily.record_setup(day*86400);if let Some(value)=value {daily.record_outcome(0,index,value);}}
        }
        let cancel=CancelToken::new(); let passes=AtomicUsize::new(0);
        let control=Control {cancel:&cancel,deadline:Instant::now()+Duration::from_secs(30),detector_passes:&passes};
        let mut forecast=empty_matrix(&spec);forecast[0].risk_expectancy=Some(1.0);forecast[0].closed_count=5;
        let mut blend=forecast.clone();
        daily.apply(&spec,&mut forecast,&control).unwrap();
        spec.trend_mode=TrendMode::Blend;
        assert_eq!(DailyMatrix::working_bytes(&spec).unwrap(),DailyMatrix::working_bytes(&StudyRequest {trend_mode:TrendMode::Forecast,..spec.clone()}).unwrap());
        daily.apply(&spec,&mut blend,&control).unwrap();
        assert_eq!(blend[0].mean_risk_expectancy,Some(1.0));
        assert_eq!(blend[0].forecast_risk_expectancy,forecast[0].risk_expectancy);
        assert_eq!(blend[0].risk_expectancy,Some(spec.trend_blend_lambda.combine(1.0,forecast[0].risk_expectancy.unwrap())));
    }

    #[test]
    fn full_64_by_64_calendar_shares_basis_and_handles_distinct_finite_endpoints() {
        let levels: Vec<_> = (1..=64).map(f64::from).collect();
        let spec: StudyRequest = serde_json::from_value(json!({
            "algorithmId":"price_volume_spike","algorithmVersion":"1.1.1",
            "algorithmParams":{},"analyticsParams":crate::AnalyticsParams::default(),"instruments":[],
            "fromUnix":0,"toUnix":30*86400,"direction":"short","seed":42,
            "tpLevels":levels,"slLevels":levels,"trendMode":"forecast","trendBendDays":"7"
        })).unwrap();
        let limits=StudyLimits::default();
        let mut daily=DailyMatrix::new(&spec,&limits).unwrap();
        assert!(DailyMatrix::working_bytes(&spec).unwrap()<limits.max_working_bytes);
        let mut matrix=empty_matrix(&spec);
        for day in 0..30 {
            for _ in 0..=day%3 {
                let index=daily.record_setup(day*86400);
                for (cell, value) in matrix.iter().enumerate() {
                    if (cell==1 && day==29) || (cell==2 && day==0) { continue; }
                    daily.record_outcome(cell,index,if day<20 {value.tp_pct/value.sl_pct} else {-1.0});
                }
            }
        }
        for cell in &mut matrix {cell.risk_expectancy=Some(0.25);cell.closed_count=1;}
        let cancel=CancelToken::new();let passes=AtomicUsize::new(0);
        let control=Control{cancel:&cancel,deadline:Instant::now()+Duration::from_secs(60),detector_passes:&passes};
        assert!(daily.apply(&spec,&mut matrix,&control).unwrap());
        assert!(matrix.iter().all(|cell|cell.risk_expectancy.is_some() && cell.mean_risk_expectancy==Some(0.25)));
        for index in [0,1,2,4095] {
            let inputs:Vec<_>=(0..30).map(|day| TrendInput {day:day as f64,
                value:(!((index==1 && day==29)||(index==2 && day==0))).then(||if day<20 {matrix[index].tp_pct/matrix[index].sl_pct} else {-1.0}),
                weight:day%3+1}).collect();
            let endpoint=setups_domain::trend::fit_weighted_trend(&inputs,TrendBendDays::Seven).unwrap().unwrap().endpoint().value;
            assert!((matrix[index].risk_expectancy.unwrap()-endpoint).abs()<1e-10);
        }
        cancel.cancel();
        assert!(!daily.apply(&spec,&mut matrix,&control).unwrap());
        let mut mean=spec;mean.trend_mode=TrendMode::Mean;
        assert_eq!(DailyMatrix::working_bytes(&mean).unwrap(),0);
    }
}
