pub mod quality;
pub mod shared;

use std::collections::VecDeque;

use setups_domain::decimal::{
    Decimal, DecimalError, ZERO, add, compare_scaled, mul_by_i128, ratio, sub,
};
use setups_domain::{
    AlgorithmParams, DetectorBar, PushResult, PushStatus, SPIKE_ALGORITHM_ID,
    SPIKE_ALGORITHM_VERSION, SPIKE_DROP_ALGORITHM_ID, SPIKE_DROP_ALGORITHM_VERSION, SetupDetails,
    SkipReason, SpikeDropParams, SpikeParams, SpikeSetup,
};

const MINUTE_SECONDS: i64 = 60;
const HUNDRED: Decimal = Decimal { int: 100, scale: 0 };

struct CloseEntry {
    time: i64,
    close: Option<Decimal>,
}

pub struct PriceVolumeSpikeDetector {
    params: AlgorithmParams,
    algorithm_id: &'static str,
    algorithm_version: &'static str,
    price_drop: bool,
    required_minutes: usize,
    short_minutes: usize,
    window_minutes: usize,
    cooldown_seconds: i64,
    price_numerator: i128,
    price_denominator: i128,
    multiplier: i128,
    multiplier_denominator: i128,
    max_consecutive_gap_minutes: i64,
    required_recent_minutes: usize,
    max_missing_count: usize,
    volumes: VecDeque<Option<Decimal>>,
    missing_in_window: usize,
    sum_window: Decimal,
    sum_short: Decimal,
    sum_long: Decimal,
    closes: VecDeque<CloseEntry>,
    prev_time: Option<i64>,
    last_setup_time: Option<i64>,
}

impl PriceVolumeSpikeDetector {
    pub fn new(params: SpikeParams, last_setup_time: Option<i64>) -> Result<Self, DecimalError> {
        Self::with_params(AlgorithmParams::Spike(params), last_setup_time)
    }

    pub fn new_drop(
        params: SpikeDropParams,
        last_setup_time: Option<i64>,
    ) -> Result<Self, DecimalError> {
        Self::with_params(AlgorithmParams::SpikeDrop(params), last_setup_time)
    }

    pub fn with_params(
        params: AlgorithmParams,
        last_setup_time: Option<i64>,
    ) -> Result<Self, DecimalError> {
        let required_minutes = params.baseline_long_days() as usize * 1440;
        let short_minutes = params.baseline_short_days() as usize * 1440;
        let window_minutes = params.window_minutes() as usize;

        let gap_percent = Decimal::parse(params.max_gap_percent())?;
        let gap_divisor = 100i128
            .checked_mul(10i128.pow(gap_percent.scale))
            .ok_or(DecimalError::Overflow("gap percent divisor"))?;
        let max_missing_count = ((required_minutes as i128)
            .checked_mul(gap_percent.int)
            .ok_or(DecimalError::Overflow("gap percent"))?
            / gap_divisor) as usize;

        let pct = Decimal::parse(params.price_pct())?;
        let (algorithm_id, algorithm_version, price_drop, price_bound) = match &params {
            AlgorithmParams::Spike(_) => (
                SPIKE_ALGORITHM_ID,
                SPIKE_ALGORITHM_VERSION,
                false,
                add(HUNDRED, pct)?,
            ),
            AlgorithmParams::SpikeDrop(_) => (
                SPIKE_DROP_ALGORITHM_ID,
                SPIKE_DROP_ALGORITHM_VERSION,
                true,
                sub(HUNDRED, pct)?,
            ),
        };
        let price_numerator = price_bound.int;
        let price_denominator = 100i128
            .checked_mul(10i128.pow(price_bound.scale))
            .ok_or(DecimalError::Overflow("price denominator"))?;

        let multiplier = Decimal::parse(params.volume_multiplier())?;
        let multiplier_denominator = 10i128
            .checked_pow(multiplier.scale)
            .ok_or(DecimalError::Overflow("multiplier denominator"))?;

        Ok(PriceVolumeSpikeDetector {
            required_minutes,
            short_minutes,
            window_minutes,
            cooldown_seconds: i64::from(params.cooldown_minutes()) * MINUTE_SECONDS,
            price_denominator,
            price_numerator,
            multiplier: multiplier.int,
            multiplier_denominator,
            max_consecutive_gap_minutes: params.max_consecutive_gap_minutes() as i64,
            required_recent_minutes: params.required_recent_minutes() as usize,
            max_missing_count,
            params,
            algorithm_id,
            algorithm_version,
            price_drop,
            volumes: VecDeque::new(),
            missing_in_window: 0,
            sum_window: ZERO,
            sum_short: ZERO,
            sum_long: ZERO,
            closes: VecDeque::new(),
            prev_time: None,
            last_setup_time,
        })
    }

    pub fn push(&mut self, bar: &DetectorBar) -> Result<PushResult, DecimalError> {
        if let Some(prev_time) = self.prev_time {
            let gap_minutes =
                (bar.open_time_unix - prev_time - MINUTE_SECONDS).div_euclid(MINUTE_SECONDS);
            if gap_minutes > 0 {
                if gap_minutes > self.max_consecutive_gap_minutes {
                    self.reset_series();
                } else {
                    for i in 0..gap_minutes {
                        self.push_entry(prev_time + (i + 1) * MINUTE_SECONDS, None, None);
                    }
                    if self.missing_in_window > self.max_missing_count {
                        self.reset_series();
                    }
                }
            }
        }
        self.prev_time = Some(bar.open_time_unix);

        let close = Decimal::parse(&bar.close)?;
        let volume = Decimal::parse(&bar.quote_turnover)?;
        self.push_entry(bar.open_time_unix, Some(volume), Some(close));

        if self.volumes.len() < self.required_minutes {
            return Ok(PushResult {
                status: PushStatus::Warmup,
                continuous_minutes: self.volumes.len(),
                setup: None,
                skip_reason: Some(SkipReason::InsufficientWarmup),
                missing_minutes: None,
            });
        }
        if self.recent_has_gap() {
            return Ok(PushResult {
                status: PushStatus::Warmup,
                continuous_minutes: self.volumes.len(),
                setup: None,
                skip_reason: Some(SkipReason::RecentGap),
                missing_minutes: None,
            });
        }

        let (setup, skip_reason) = self.evaluate(bar)?;
        Ok(PushResult {
            status: PushStatus::Evaluated,
            continuous_minutes: self.volumes.len(),
            missing_minutes: Some(self.missing_in_window as u32),
            setup,
            skip_reason,
        })
    }

    fn recent_has_gap(&self) -> bool {
        let start = self
            .volumes
            .len()
            .saturating_sub(self.required_recent_minutes);
        self.volumes
            .iter()
            .skip(start)
            .any(|volume| volume.is_none())
    }

    fn push_entry(&mut self, time: i64, volume: Option<Decimal>, close: Option<Decimal>) {
        let vol = volume.unwrap_or(ZERO);

        self.volumes.push_back(volume);
        if volume.is_none() {
            self.missing_in_window += 1;
        }
        self.sum_window = add(self.sum_window, vol).expect("sum_window overflow");
        self.sum_short = add(self.sum_short, vol).expect("sum_short overflow");
        self.sum_long = add(self.sum_long, vol).expect("sum_long overflow");

        let len = self.volumes.len();
        if len > self.short_minutes
            && let Some(leaving) = self.volumes.get(len - 1 - self.short_minutes)
        {
            self.sum_short =
                sub(self.sum_short, leaving.unwrap_or(ZERO)).expect("sum_short underflow");
        }
        if len > self.window_minutes
            && let Some(leaving) = self.volumes.get(len - 1 - self.window_minutes)
        {
            self.sum_window =
                sub(self.sum_window, leaving.unwrap_or(ZERO)).expect("sum_window underflow");
        }
        if len > self.required_minutes
            && let Some(leaving) = self.volumes.pop_front()
        {
            if leaving.is_none() {
                self.missing_in_window -= 1;
            }
            self.sum_long =
                sub(self.sum_long, leaving.unwrap_or(ZERO)).expect("sum_long underflow");
        }

        self.closes.push_back(CloseEntry { time, close });
        if self.closes.len() > self.window_minutes + 1 {
            self.closes.pop_front();
        }
    }

    fn evaluate(
        &mut self,
        bar: &DetectorBar,
    ) -> Result<(Option<SpikeSetup>, Option<SkipReason>), DecimalError> {
        let recent_start = self
            .volumes
            .len()
            .saturating_sub(self.required_recent_minutes);
        for index in recent_start..self.volumes.len() {
            if self.volumes[index].is_none() {
                return Ok((None, Some(SkipReason::RecentGap)));
            }
        }

        if self.missing_in_window > self.max_missing_count {
            return Ok((None, Some(SkipReason::GapToleranceExceeded)));
        }

        let target_time = bar.open_time_unix - self.window_minutes as i64 * MINUTE_SECONDS;
        let previous_close = match self.closes.iter().find(|entry| entry.time == target_time) {
            Some(CloseEntry {
                close: Some(close), ..
            }) => *close,
            _ => return Ok((None, Some(SkipReason::PriceReferenceMissing))),
        };
        let current_close = match self.closes.back() {
            Some(CloseEntry {
                close: Some(close), ..
            }) => *close,
            _ => return Ok((None, Some(SkipReason::PriceReferenceMissing))),
        };

        let price_ordering = compare_scaled(
            current_close,
            self.price_denominator,
            previous_close,
            self.price_numerator,
        )?;
        let price_condition_met = if self.price_drop {
            price_ordering != std::cmp::Ordering::Greater
        } else {
            price_ordering != std::cmp::Ordering::Less
        };
        if !price_condition_met {
            return Ok((None, None));
        }

        let window_factor = self.window_minutes as i128;
        let long_factor = self.required_minutes as i128;
        let short_factor = self.short_minutes as i128;

        let over_long = compare_scaled(
            self.sum_window,
            long_factor
                .checked_mul(self.multiplier_denominator)
                .expect("overflow"),
            mul_by_i128(
                self.sum_long,
                self.multiplier
                    .checked_mul(window_factor)
                    .expect("overflow"),
            )?,
            1,
        )? == std::cmp::Ordering::Greater;
        let over_short = compare_scaled(
            self.sum_window,
            short_factor
                .checked_mul(self.multiplier_denominator)
                .expect("overflow"),
            mul_by_i128(
                self.sum_short,
                self.multiplier
                    .checked_mul(window_factor)
                    .expect("overflow"),
            )?,
            1,
        )? == std::cmp::Ordering::Greater;
        if !over_long && !over_short {
            return Ok((None, None));
        }

        if let Some(last_setup_time) = self.last_setup_time
            && bar.open_time_unix - last_setup_time < self.cooldown_seconds
        {
            return Ok((None, None));
        }
        self.last_setup_time = Some(bar.open_time_unix);

        let growth_pct = ratio(
            mul_by_i128(sub(current_close, previous_close)?, 100)?,
            previous_close,
        )?;
        let volume_long_ratio = ratio(
            mul_by_i128(self.sum_window, long_factor)?,
            mul_by_i128(self.sum_long, window_factor)?,
        )?;
        let volume_short_ratio = ratio(
            mul_by_i128(self.sum_window, short_factor)?,
            mul_by_i128(self.sum_short, window_factor)?,
        )?;

        Ok((
            Some(SpikeSetup {
                algorithm_id: self.algorithm_id.to_string(),
                algorithm_version: self.algorithm_version.to_string(),
                open_time_unix: bar.open_time_unix,
                close_time_unix: bar.open_time_unix + MINUTE_SECONDS,
                close_price: bar.close.clone(),
                params: self.params.clone(),
                details: Some(SetupDetails {
                    growth_pct,
                    volume_long_ratio,
                    volume_short_ratio,
                    over_long,
                    over_short,
                    missing_minutes: self.missing_in_window as u32,
                }),
            }),
            None,
        ))
    }

    fn reset_series(&mut self) {
        self.volumes.clear();
        self.missing_in_window = 0;
        self.sum_window = ZERO;
        self.sum_short = ZERO;
        self.sum_long = ZERO;
        self.closes.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: i64 = 1_699_999_980;

    fn minute(index: usize) -> i64 {
        T0 + index as i64 * 60
    }

    fn bar(index: usize, close: &str, turnover: &str) -> DetectorBar {
        DetectorBar {
            open_time_unix: minute(index),
            close: close.to_string(),
            quote_turnover: turnover.to_string(),
        }
    }

    fn drop_params() -> SpikeDropParams {
        SpikeDropParams {
            price_drop_pct: "12".to_string(),
            window_minutes: 5,
            baseline_long_days: 1,
            baseline_short_days: 1,
            volume_multiplier: "3".to_string(),
            cooldown_minutes: 5,
            max_gap_percent: "1".to_string(),
            max_consecutive_gap_minutes: 5,
            required_recent_minutes: 10,
        }
    }

    fn spike_params() -> SpikeParams {
        SpikeParams {
            price_growth_pct: "12".to_string(),
            window_minutes: 5,
            baseline_long_days: 1,
            baseline_short_days: 1,
            volume_multiplier: "3".to_string(),
            cooldown_minutes: 5,
            max_gap_percent: "1".to_string(),
            max_consecutive_gap_minutes: 5,
            required_recent_minutes: 10,
        }
    }

    /// 1439 warmup bars plus the signal bar; the four bars before the signal
    /// join the signal window. `history_turnover` fills warmup, `window_turnover`
    /// the five window bars (1435..=1439).
    fn series(
        signal_close: &str,
        history_turnover: &str,
        window_turnover: &str,
    ) -> Vec<DetectorBar> {
        let mut bars: Vec<DetectorBar> = (0..1439)
            .map(|index| bar(index, "100", history_turnover))
            .collect();
        for entry in bars.iter_mut().skip(1435) {
            entry.quote_turnover = window_turnover.to_string();
        }
        bars.push(bar(1439, signal_close, window_turnover));
        bars
    }

    fn last_push(detector: &mut PriceVolumeSpikeDetector, bars: &[DetectorBar]) -> PushResult {
        let mut last = None;
        for entry in bars {
            last = Some(detector.push(entry).expect("push succeeds"));
        }
        last.expect("at least one bar")
    }

    #[test]
    fn drop_triggers_on_twelve_percent_fall_with_volume_spike() {
        let mut detector = PriceVolumeSpikeDetector::new_drop(drop_params(), None).unwrap();
        let result = last_push(&mut detector, &series("88", "10", "3000"));
        let setup = result.setup.expect("setup emitted");
        assert_eq!(setup.algorithm_id, "price_drop_volume_spike");
        assert_eq!(setup.algorithm_version, "1.1.1");
        assert_eq!(setup.open_time_unix, minute(1439));
        assert_eq!(setup.close_price, "88");
        let details = setup.details.expect("details present");
        assert_eq!(details.growth_pct.as_deref(), Some("-12.0000"));
        assert!(details.over_long || details.over_short);
    }

    #[test]
    fn drop_does_not_trigger_on_rise_and_spike_does_not_trigger_on_fall() {
        let mut drop_detector = PriceVolumeSpikeDetector::new_drop(drop_params(), None).unwrap();
        let result = last_push(&mut drop_detector, &series("112", "10", "3000"));
        assert!(result.setup.is_none(), "drop must not fire on a rise");

        let mut spike_detector = PriceVolumeSpikeDetector::new(spike_params(), None).unwrap();
        let result = last_push(&mut spike_detector, &series("88", "10", "3000"));
        assert!(result.setup.is_none(), "spike must not fire on a fall");
    }

    #[test]
    fn spike_still_triggers_on_twelve_percent_rise_with_volume_spike() {
        let mut detector = PriceVolumeSpikeDetector::new(spike_params(), None).unwrap();
        let result = last_push(&mut detector, &series("112", "10", "3000"));
        let setup = result.setup.expect("setup emitted");
        assert_eq!(setup.algorithm_id, "price_volume_spike");
        assert_eq!(setup.algorithm_version, "1.1.1");
        let details = setup.details.expect("details present");
        assert_eq!(details.growth_pct.as_deref(), Some("12.0000"));
    }

    #[test]
    fn drop_volume_exactly_3x_is_not_a_setup() {
        let mut detector = PriceVolumeSpikeDetector::new_drop(drop_params(), None).unwrap();
        let result = last_push(&mut detector, &series("88", "95", "287"));
        assert!(
            result.setup.is_none(),
            "exactly x3 turnover must not create a setup"
        );

        let mut detector = PriceVolumeSpikeDetector::new_drop(drop_params(), None).unwrap();
        let result = last_push(&mut detector, &series("88", "95", "288"));
        assert!(
            result.setup.is_some(),
            "turnover above x3 must create a setup"
        );
    }

    #[test]
    fn drop_price_above_threshold_is_not_a_setup() {
        let mut detector = PriceVolumeSpikeDetector::new_drop(drop_params(), None).unwrap();
        let result = last_push(&mut detector, &series("88.5", "10", "3000"));
        assert!(result.setup.is_none(), "11.5% fall is below the threshold");
    }

    #[test]
    fn drop_cooldown_suppresses_recent_repetition() {
        let signal_time = minute(1439);
        let mut detector =
            PriceVolumeSpikeDetector::new_drop(drop_params(), Some(signal_time - 240)).unwrap();
        let result = last_push(&mut detector, &series("88", "10", "3000"));
        assert!(result.setup.is_none(), "setup 4 minutes ago must suppress");

        let mut detector =
            PriceVolumeSpikeDetector::new_drop(drop_params(), Some(signal_time - 300)).unwrap();
        let result = last_push(&mut detector, &series("88", "10", "3000"));
        assert!(
            result.setup.is_some(),
            "boundary cooldown of 300s allows a setup"
        );
    }
}
