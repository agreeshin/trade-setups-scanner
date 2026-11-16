//! Evaluation-owned daily forecast scratch. Mean paths do not instantiate it.
use crate::{MatrixCell, StudyError, StudyLimits, StudyRequest, TrendMode};
use crate::evaluation::{Control, limit};
use setups_domain::trend::{MAX_TREND_DAYS, TrendDesign, TrendInput};
use std::{collections::BTreeMap, mem::size_of};

#[derive(Default, Clone, Copy)]
struct DailyCell { sum_r: f64, closed: u32 }

pub(crate) struct DailyMatrix {
    origin_day: i64,
    days: usize,
    rows: Vec<u32>,
    cells: Vec<DailyCell>,
}
impl DailyMatrix {
    /// Includes one shared design, solver scratch, compact input and grouping
    /// metadata; none survives the evaluation or a cancelled/failed call.
    pub fn working_bytes(spec: &StudyRequest) -> Result<usize, StudyError> {
        if spec.trend_mode == TrendMode::Mean { return Ok(0); }
        let days = Self::day_count(spec)?;
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
    pub fn new(spec: &StudyRequest, limits: &StudyLimits) -> Result<Self, StudyError> {
        // Every counter is bounded by the already enforced candidate setup cap.
        limit("forecast_setup_counter", limits.max_setups_per_candidate, u32::MAX as usize)?;
        let days = Self::day_count(spec)?;
        Ok(Self { origin_day: spec.from_unix.div_euclid(86400), days,
            rows: vec![0; days], cells: vec![DailyCell::default(); days * spec.tp_levels.len() * spec.sl_levels.len()] })
    }
    pub fn record_setup(&mut self, signal_close: i64) -> usize {
        let day = (signal_close.div_euclid(86400) - self.origin_day) as usize;
        self.rows[day] += 1;
        day
    }
    pub fn record_outcome(&mut self, cell: usize, day: usize, value: f64) {
        let bucket = &mut self.cells[cell * self.days + day];
        bucket.closed += 1;
        bucket.sum_r += value;
    }
    pub fn apply(&self, spec: &StudyRequest, matrix: &mut [MatrixCell], control: &Control<'_>) -> Result<bool, StudyError> {
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
                (self.origin_day + last as i64) as f64, spec.trend_bend_days)
                .map_err(|e| StudyError::Execution(e.to_string()))?;
            let mut points = Vec::with_capacity(last - first + 1);
            for index in cells {
                if control.stopped() { return Ok(false); }
                points.clear();
                for day in first..=last {
                    let bucket = self.cells[index * self.days + day];
                    points.push(TrendInput { day: (self.origin_day + day as i64) as f64,
                        value: (bucket.closed > 0).then(|| bucket.sum_r / f64::from(bucket.closed)), weight: self.rows[day] });
                }
                matrix[index].risk_expectancy = design.endpoint(&points)
                    .map_err(|e| StudyError::Execution(e.to_string()))?;
            }
        }
        Ok(true)
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
