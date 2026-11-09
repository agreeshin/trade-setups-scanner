pub mod quality;

use std::collections::VecDeque;

use setups_domain::decimal::{
    Decimal, DecimalError, ZERO, add, compare_scaled, mul_by_i128, ratio, sub,
};
use setups_domain::{
    DetectorBar, PushResult, PushStatus, SPIKE_ALGORITHM_ID, SPIKE_ALGORITHM_VERSION, SetupDetails,
    SkipReason, SpikeParams, SpikeSetup,
};

const MINUTE_SECONDS: i64 = 60;

struct CloseEntry {
    time: i64,
    close: Option<Decimal>,
}

pub struct PriceVolumeSpikeDetector {
    params: SpikeParams,
    required_minutes: usize,
    short_minutes: usize,
    window_minutes: usize,
    cooldown_seconds: i64,
    growth_denominator: i128,
    growth_numerator: i128,
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
        let required_minutes = params.baseline_long_days as usize * 1440;
        let short_minutes = params.baseline_short_days as usize * 1440;
        let window_minutes = params.window_minutes as usize;

        let gap_percent = Decimal::parse(&params.max_gap_percent)?;
        let gap_divisor = 100i128
            .checked_mul(10i128.pow(gap_percent.scale))
            .ok_or(DecimalError::Overflow("gap percent divisor"))?;
        let max_missing_count = ((required_minutes as i128)
            .checked_mul(gap_percent.int)
            .ok_or(DecimalError::Overflow("gap percent"))?
            / gap_divisor) as usize;

        let pct = Decimal::parse(&params.price_growth_pct)?;
        let growth = add(Decimal { int: 100, scale: 0 }, pct)?;
        let growth_numerator = growth.int;
        let growth_denominator = 100i128
            .checked_mul(10i128.pow(growth.scale))
            .ok_or(DecimalError::Overflow("growth denominator"))?;

        let multiplier = Decimal::parse(&params.volume_multiplier)?;
        let multiplier_denominator = 10i128
            .checked_pow(multiplier.scale)
            .ok_or(DecimalError::Overflow("multiplier denominator"))?;

        Ok(PriceVolumeSpikeDetector {
            required_minutes,
            short_minutes,
            window_minutes,
            cooldown_seconds: params.cooldown_minutes as i64 * MINUTE_SECONDS,
            growth_denominator,
            growth_numerator,
            multiplier: multiplier.int,
            multiplier_denominator,
            max_consecutive_gap_minutes: params.max_consecutive_gap_minutes as i64,
            required_recent_minutes: params.required_recent_minutes as usize,
            max_missing_count,
            params,
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

        if compare_scaled(
            current_close,
            self.growth_denominator,
            previous_close,
            self.growth_numerator,
        )? == std::cmp::Ordering::Less
        {
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
                algorithm_id: SPIKE_ALGORITHM_ID.to_string(),
                algorithm_version: SPIKE_ALGORITHM_VERSION.to_string(),
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
