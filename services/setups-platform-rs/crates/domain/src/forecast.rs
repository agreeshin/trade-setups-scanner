//! Versioned shared forecast math. No clock, market reads, cache or global state.
//! A trial owns one constant-size accumulator per cell; only displayed cells
//! allocate a curve (at most MAX_FORECAST_DAYS points). Owners must prove that
//! aggregate outcomes were observable at the supplied frozen cutoff.
use crate::trend::{TrendBendDays, TrendBlendLambda, TrendMode};
use serde::{Deserialize, Serialize};
use std::fmt;

pub const FORECAST_RECIPE_VERSION: u32 = 1;
pub const MAX_FORECAST_DAYS: usize = 3661;
const DAY_SECONDS: i64 = 86_400;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ForecastMethod { DecayedProbability, Trend }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PriorPolicy { GrossBreakEven }

/// Optional `forecast` field in new study/config/artifact DTOs. None retains
/// the old trend path; never use a serde default to upgrade a saved recipe.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(tag = "method", rename_all = "snake_case")]
pub enum ForecastRecipe {
    DecayedProbability {
        version: u32,
        #[serde(rename = "halfLifeDays")]
        half_life_days: f64,
        kappa: f64,
        #[serde(rename = "priorPolicy")]
        prior_policy: PriorPolicy,
    },
    Trend {
        version: u32,
        #[serde(rename = "bendDays")]
        bend_days: TrendBendDays,
    },
}

// Do not derive internally-tagged Deserialize: serde's Content buffer cannot
// decode f64 under this workspace's serde_json arbitrary_precision feature.
impl<'de> Deserialize<'de> for ForecastRecipe {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase", deny_unknown_fields)]
        struct Wire {
            method: ForecastMethod,
            version: u32,
            half_life_days: Option<serde_json::Number>,
            kappa: Option<serde_json::Number>,
            prior_policy: Option<PriorPolicy>,
            bend_days: Option<TrendBendDays>,
        }
        let w = Wire::deserialize(deserializer)?;
        let recipe = match (w.method, w.half_life_days.and_then(|n| n.as_f64()), w.kappa.and_then(|n| n.as_f64()), w.prior_policy, w.bend_days) {
            (ForecastMethod::DecayedProbability, Some(half_life_days), Some(kappa), Some(prior_policy), None) =>
                Self::DecayedProbability { version: w.version, half_life_days, kappa, prior_policy },
            (ForecastMethod::Trend, None, None, None, Some(bend_days)) =>
                Self::Trend { version: w.version, bend_days },
            _ => return Err(serde::de::Error::custom("forecast settings do not match method")),
        };
        recipe.validate().map_err(serde::de::Error::custom)?;
        Ok(recipe)
    }
}

impl ForecastRecipe {
    /// Engineering baseline, not a calibrated optimum. Two-week half-life and
    /// ten weighted-trial equivalents provide a simple modest sparse-data
    /// anchor. Callers persist these explicit values; absence stays legacy.
    /// Sensitivity to H=7/14/28 and kappa=5/10/20 is covered below, not OOS proof.
    pub fn engineering_baseline() -> Self {
        Self::DecayedProbability { version: FORECAST_RECIPE_VERSION, half_life_days: 14.0,
            kappa: 10.0, prior_policy: PriorPolicy::GrossBreakEven }
    }
    pub fn method(self) -> ForecastMethod {
        match self { Self::DecayedProbability { .. } => ForecastMethod::DecayedProbability, Self::Trend { .. } => ForecastMethod::Trend }
    }
    pub fn validate(self) -> Result<(), ForecastError> {
        let version = match self { Self::DecayedProbability { version, .. } | Self::Trend { version, .. } => version };
        if version != FORECAST_RECIPE_VERSION { return Err(ForecastError::UnsupportedVersion); }
        if let Self::DecayedProbability { half_life_days, kappa, .. } = self
            && (!half_life_days.is_finite() || half_life_days <= 0.0 || !kappa.is_finite() || kappa <= 0.0) {
            return Err(ForecastError::InvalidRecipe);
        }
        Ok(())
    }
    /// Model fragment only. The owner also keys data/population, cutoff, q,
    /// mode/lambda and outcome policy; this alone is not a result cache key.
    pub fn identity_key(self) -> Result<String, ForecastError> {
        self.validate()?;
        Ok(match self {
            Self::DecayedProbability { version, half_life_days, kappa, .. } =>
                format!("forecast:v{version}:decayed_probability:h{half_life_days}:k{kappa}:gross_break_even"),
            Self::Trend { version, bend_days } => format!("forecast:v{version}:trend:{}", bend_days.key()),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForecastError {
    UnsupportedVersion, InvalidRecipe, WrongMethod, InvalidPayoff, InvalidTimestamp,
    InvalidDay, CountOverflow, TooManyDays, UnorderedDays, PeriodTooLong,
    NonFiniteNumber, InvalidWeight, BeforeCutoff,
}
impl fmt::Display for ForecastError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { write!(f, "forecast: {self:?}") }
}
impl std::error::Error for ForecastError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ForecastUnavailableReason { NoClosedOutcomes, NoWeightedEvidence }

// Preserve exact parsed f64 values on direct streaming JSON roundtrips too.
// Number retains its decimal spelling with arbitrary_precision; the default
// streaming f64 decoder can otherwise shift a frozen value by one ULP.
fn deserialize_number<'de, D: serde::Deserializer<'de>>(d: D) -> Result<f64, D::Error> {
    serde_json::Number::deserialize(d)?.as_f64().ok_or_else(|| serde::de::Error::custom("non-finite forecast number"))
}
fn deserialize_optional_number<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<f64>, D::Error> {
    Option::<serde_json::Number>::deserialize(d)?.map(|n| n.as_f64().ok_or_else(|| serde::de::Error::custom("non-finite forecast number"))).transpose()
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProbabilityEstimate {
    pub observation_cutoff: i64,
    #[serde(deserialize_with = "deserialize_number")]
    pub mean_r: f64,
    #[serde(deserialize_with = "deserialize_number")]
    pub forecast_r: f64,
    #[serde(deserialize_with = "deserialize_number")]
    pub tp_probability: f64,
    #[serde(deserialize_with = "deserialize_number")]
    pub prior_probability: f64,
    pub tp_count: u64,
    pub sl_count: u64,
    #[serde(deserialize_with = "deserialize_number")]
    pub weighted_tp: f64,
    #[serde(deserialize_with = "deserialize_number")]
    pub weighted_sl: f64,
    #[serde(deserialize_with = "deserialize_number")]
    pub weight_sum: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ProbabilityForecast {
    Available { value: ProbabilityEstimate },
    Unavailable { reason: ForecastUnavailableReason },
}
impl<'de> Deserialize<'de> for ProbabilityForecast {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire { status: String, value: Option<ProbabilityEstimate>, reason: Option<ForecastUnavailableReason> }
        let w = Wire::deserialize(deserializer)?;
        match (w.status.as_str(), w.value, w.reason) {
            ("available", Some(value), None) => Ok(Self::Available { value }),
            ("unavailable", None, Some(reason)) => Ok(Self::Unavailable { reason }),
            _ => Err(serde::de::Error::custom("invalid probability forecast availability")),
        }
    }
}
impl ProbabilityForecast {
    pub fn as_ref(&self) -> Option<&ProbabilityEstimate> {
        match self { Self::Available { value } => Some(value), Self::Unavailable { .. } => None }
    }
    /// Frozen forecast: no clock-driven decay beyond the observation cutoff.
    pub fn at(&self, at_unix: i64) -> Result<Option<f64>, ForecastError> {
        match self {
            Self::Available { value } if at_unix < value.observation_cutoff => Err(ForecastError::BeforeCutoff),
            Self::Available { value } => Ok(Some(value.forecast_r)),
            Self::Unavailable { .. } => Ok(None),
        }
    }
}

/// Counts must already exclude outcomes not observable at the request cutoff.
/// dayStartUnix is a UTC raw-signal cohort, never the exit day or epoch days.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProbabilityDay { pub day_start_unix: i64, pub tp_count: u64, pub sl_count: u64 }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClosedOutcome { TakeProfit, StopLoss }

/// Exact nonnegative sum of binary f64 weights multiplied by integer counts.
/// Eighteen limbs cover units of 2^-1074 through a u64 count of unit weights.
/// Rounding happens once on read, so event order and daily batching cannot
/// shift optimizer F versus the same graph/caption cohorts by an ULP. Fixed
/// 144-byte storage; no per-day collection, quantized weights or global state.
#[derive(Debug, Clone, Copy, Default)]
struct ExactWeightSum { limbs: [u64;18] }
impl ExactWeightSum {
    fn add(&mut self, weight:f64, count:u64)->Result<(),ForecastError> {
        if !weight.is_finite() || !(0.0..=1.0).contains(&weight) {return Err(ForecastError::InvalidWeight);}
        if weight==0.0 || count==0 {return Ok(());}
        let bits=weight.to_bits();let exponent=((bits>>52)&0x7ff) as usize;
        let significand=(bits&((1u64<<52)-1)) | if exponent==0 {0}else{1u64<<52};
        let shift=exponent.saturating_sub(1);
        let product=u128::from(significand)*u128::from(count);
        let index=shift/64;let offset=shift%64;
        let low=product as u64;let high=(product>>64) as u64;
        self.add_limb(index,low<<offset)?;
        if offset==0 {self.add_limb(index+1,high)?;} else {
            self.add_limb(index+1,(high<<offset)|(low>>(64-offset)))?;
            self.add_limb(index+2,high>>(64-offset))?;
        }
        Ok(())
    }
    fn add_limb(&mut self,mut index:usize,mut value:u64)->Result<(),ForecastError> {
        while value!=0 {
            let limb=self.limbs.get_mut(index).ok_or(ForecastError::CountOverflow)?;
            let (sum,carry)=limb.overflowing_add(value);*limb=sum;
            value=u64::from(carry);index+=1;
        }
        Ok(())
    }
    fn any_below(&self,bit:usize)->bool {
        let whole=bit/64;let remainder=bit%64;
        self.limbs[..whole].iter().any(|&word|word!=0)
            || (remainder!=0 && self.limbs[whole]&((1u64<<remainder)-1)!=0)
    }
    fn value(&self)->f64 {
        let Some(index)=self.limbs.iter().rposition(|&word|word!=0) else {return 0.0;};
        let mut highest=index*64+63-self.limbs[index].leading_zeros() as usize;
        if highest<52 {return f64::from_bits(self.limbs[0]);}
        let shift=highest-52;let word=shift/64;let offset=shift%64;
        let mut significand=self.limbs[word]>>offset;
        if offset!=0 && word+1<self.limbs.len() {significand|=self.limbs[word+1]<<(64-offset);}
        if shift>0 {
            let half=shift-1;
            let halfway=self.limbs[half/64]&(1u64<<(half%64))!=0;
            if halfway && (self.any_below(half) || significand&1!=0) {
                significand+=1;
                if significand==1u64<<53 {significand>>=1;highest+=1;}
            }
        }
        f64::from_bits(((highest as u64-51)<<52)|(significand&((1u64<<52)-1)))
    }
}

/// Constant-size, evaluation-owned state. There is no retained daily vector.
#[derive(Debug, Clone, Copy)]
pub struct ProbabilityAccumulator {
    half_life_days: f64, kappa: f64, q: f64, cutoff: i64,
    tp: u64, sl: u64, weighted_tp: ExactWeightSum, weighted_sl: ExactWeightSum,
}
impl ProbabilityAccumulator {
    pub fn new(recipe: ForecastRecipe, q: f64, observation_cutoff: i64) -> Result<Self, ForecastError> {
        recipe.validate()?;
        if !q.is_finite() || q <= 0.0 { return Err(ForecastError::InvalidPayoff); }
        let ForecastRecipe::DecayedProbability { half_life_days, kappa, .. } = recipe else { return Err(ForecastError::WrongMethod); };
        Ok(Self { half_life_days, kappa, q, cutoff: observation_cutoff,
            tp: 0, sl: 0, weighted_tp: ExactWeightSum::default(), weighted_sl: ExactWeightSum::default() })
    }
    pub fn add_day(&mut self, day: ProbabilityDay) -> Result<(), ForecastError> {
        if day.day_start_unix.rem_euclid(DAY_SECONDS) != 0 || day.day_start_unix > self.cutoff {
            return Err(ForecastError::InvalidDay);
        }
        let age = self.cutoff.checked_sub(day.day_start_unix).ok_or(ForecastError::InvalidTimestamp)?;
        let weight = (-(age as f64 / DAY_SECONDS as f64) / self.half_life_days).exp2();
        let tp = self.tp.checked_add(day.tp_count).ok_or(ForecastError::CountOverflow)?;
        let sl = self.sl.checked_add(day.sl_count).ok_or(ForecastError::CountOverflow)?;
        tp.checked_add(sl).ok_or(ForecastError::CountOverflow)?;
        let mut weighted_tp=self.weighted_tp;let mut weighted_sl=self.weighted_sl;
        weighted_tp.add(weight,day.tp_count)?;weighted_sl.add(weight,day.sl_count)?;
        // Commit all counters only after validation, including overflow.
        self.tp = tp; self.sl = sl;
        self.weighted_tp=weighted_tp;self.weighted_sl=weighted_sl;
        Ok(())
    }
    /// Optional event adapter. A future raw signal/label is excluded, not SL.
    /// available_at is actual closed-candle availability, not touch time.
    pub fn add_outcome(&mut self, raw_signal_at: i64, available_at: i64, outcome: ClosedOutcome) -> Result<bool, ForecastError> {
        if raw_signal_at > self.cutoff || available_at > self.cutoff { return Ok(false); }
        let day_start_unix = raw_signal_at.div_euclid(DAY_SECONDS).checked_mul(DAY_SECONDS).ok_or(ForecastError::InvalidTimestamp)?;
        self.add_day(ProbabilityDay { day_start_unix,
            tp_count: u64::from(outcome == ClosedOutcome::TakeProfit),
            sl_count: u64::from(outcome == ClosedOutcome::StopLoss) })?;
        Ok(true)
    }
    pub fn finish(&self) -> Result<ProbabilityForecast, ForecastError> {
        let n = self.tp + self.sl; // add_day checked the combined count.
        if n == 0 { return Ok(ProbabilityForecast::Unavailable { reason: ForecastUnavailableReason::NoClosedOutcomes }); }
        let weighted_tp=self.weighted_tp.value();let weighted_sl=self.weighted_sl.value();
        let w = weighted_tp + weighted_sl;
        if w <= 0.0 { return Ok(ProbabilityForecast::Unavailable { reason: ForecastUnavailableReason::NoWeightedEvidence }); }
        let denominator = w + self.kappa;
        let prior_probability = 1.0 / (1.0 + self.q);
        let tp_probability = weighted_tp / denominator + (self.kappa / denominator) * prior_probability;
        // Equivalent to (1+q)*p-1, without cancellation near the neutral prior
        // or overflowing q*weightedTP before dividing by the denominator.
        let forecast_r = self.q * (weighted_tp / denominator) - weighted_sl / denominator;
        let mean_r = self.q * (self.tp as f64 / n as f64) - self.sl as f64 / n as f64;
        if ![w, tp_probability, forecast_r, mean_r].iter().all(|v| v.is_finite()) { return Err(ForecastError::NonFiniteNumber); }
        Ok(ProbabilityForecast::Available { value: ProbabilityEstimate {
            observation_cutoff: self.cutoff, mean_r, forecast_r, tp_probability, prior_probability,
            tp_count: self.tp, sl_count: self.sl, weighted_tp, weighted_sl, weight_sum: w,
        } })
    }
}

pub fn evaluate_probability(recipe: ForecastRecipe, q: f64, observation_cutoff: i64, daily: &[ProbabilityDay]) -> Result<ProbabilityForecast, ForecastError> {
    validate_daily(daily, observation_cutoff)?;
    let mut acc = ProbabilityAccumulator::new(recipe, q, observation_cutoff)?;
    for &day in daily { acc.add_day(day)?; }
    acc.finish()
}

fn validate_daily(daily: &[ProbabilityDay], cutoff: i64) -> Result<(), ForecastError> {
    if daily.len() > MAX_FORECAST_DAYS { return Err(ForecastError::TooManyDays); }
    if daily.windows(2).any(|w| w[0].day_start_unix >= w[1].day_start_unix) { return Err(ForecastError::UnorderedDays); }
    if let (Some(first), Some(last)) = (daily.first(), daily.last()) {
        // Bound the cohort calendar, not label delay after that calendar.
        // A late observation cutoff does not allocate more daily buckets.
        let span = last.day_start_unix.checked_sub(first.day_start_unix).ok_or(ForecastError::InvalidTimestamp)?;
        if span > (MAX_FORECAST_DAYS as i64 - 1) * DAY_SECONDS { return Err(ForecastError::PeriodTooLong); }
        if last.day_start_unix > cutoff { return Err(ForecastError::InvalidDay); }
    }
    Ok(())
}

/// Numeric blend helper permits zero unused weight; this does NOT expand the
/// user-facing TrendBlendLambda domain (0.05..0.95, step 0.01, default 0.7).
pub fn blend_r(mean_r: Option<f64>, forecast_r: Option<f64>, forecast_weight: f64) -> Result<Option<f64>, ForecastError> {
    if !forecast_weight.is_finite() || !(0.0..=1.0).contains(&forecast_weight) { return Err(ForecastError::InvalidWeight); }
    let finite = |v: Option<f64>| -> Result<Option<f64>, ForecastError> {
        if v.is_some_and(|v| !v.is_finite()) { Err(ForecastError::NonFiniteNumber) } else { Ok(v) }
    };
    if forecast_weight == 0.0 { return finite(mean_r); }
    if forecast_weight == 1.0 { return finite(forecast_r); }
    match (finite(mean_r)?, finite(forecast_r)?) {
        (Some(mean), Some(forecast)) => finite(Some((1.0 - forecast_weight) * mean + forecast_weight * forecast)),
        _ => Ok(None),
    }
}
pub fn decision_r(mode: TrendMode, mean_r: Option<f64>, forecast_r: Option<f64>, lambda: TrendBlendLambda) -> Result<Option<f64>, ForecastError> {
    blend_r(mean_r, forecast_r, match mode { TrendMode::Mean => 0.0, TrendMode::Forecast => 1.0, TrendMode::Blend => lambda.fraction() })
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProbabilityCurveRequest {
    pub recipe: ForecastRecipe,
    #[serde(deserialize_with = "deserialize_number")]
    pub q: f64,
    pub observation_cutoff: i64,
    pub daily: Vec<ProbabilityDay>,
}
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProbabilityCurvePoint {
    /// Unix SECONDS, unlike legacy TrendPoint.day (epoch DAYS).
    pub at_unix: i64,
    #[serde(default, deserialize_with = "deserialize_optional_number")]
    pub forecast_r: Option<f64>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProbabilityCurve {
    pub recipe: ForecastRecipe,
    pub observation_cutoff: i64,
    pub retrospective: bool,
    pub estimate: ProbabilityForecast,
    pub curve: Vec<ProbabilityCurvePoint>,
}

/// Prefix curves are RETROSPECTIVE: sealed buckets do not establish historical
/// label availability. The last point is the canonical scalar at C, exactly.
/// Only rendered cells call this. O(days) owned output, O(days^2) simple work
/// bounded at 3661; no per-trial curve generation or retained cache.
pub fn probability_curve(request: &ProbabilityCurveRequest) -> Result<ProbabilityCurve, ForecastError> {
    let estimate = evaluate_probability(request.recipe, request.q, request.observation_cutoff, &request.daily)?;
    let mut curve = Vec::with_capacity(request.daily.len());
    for (index, day) in request.daily.iter().enumerate() {
        let last = index + 1 == request.daily.len();
        let at_unix = if last { request.observation_cutoff } else {
            day.day_start_unix.checked_add(DAY_SECONDS).ok_or(ForecastError::InvalidTimestamp)?.min(request.observation_cutoff)
        };
        let point = if last { estimate } else {
            evaluate_probability(request.recipe, request.q, at_unix, &request.daily[..=index])?
        };
        curve.push(ProbabilityCurvePoint { at_unix, forecast_r: point.as_ref().map(|p| p.forecast_r) });
    }
    Ok(ProbabilityCurve { recipe: request.recipe, observation_cutoff: request.observation_cutoff,
        retrospective: true, estimate, curve })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn close(a: f64, b: f64) { assert!((a-b).abs() <= 1e-12 * (1.0 + b.abs()), "{a} != {b}"); }
    fn day(d: i64, tp: u64, sl: u64) -> ProbabilityDay { ProbabilityDay { day_start_unix: d * DAY_SECONDS, tp_count: tp, sl_count: sl } }
    fn recipe(h: f64, k: f64) -> ForecastRecipe { ForecastRecipe::DecayedProbability { version: 1, half_life_days: h, kappa: k, prior_policy: PriorPolicy::GrossBreakEven } }

    #[test]
    fn explicit_recipe_wire_roundtrips_without_legacy_default_or_numeric_buffer() {
        let value = json!({"method":"decayed_probability","version":1,"halfLifeDays":14,"kappa":10,"priorPolicy":"gross_break_even"});
        let r: ForecastRecipe = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(r, ForecastRecipe::engineering_baseline());
        assert_eq!(serde_json::to_value(r).unwrap(), json!({"method":"decayed_probability","version":1,"halfLifeDays":14.0,"kappa":10.0,"priorPolicy":"gross_break_even"}));
        assert_eq!(serde_json::from_str::<ForecastRecipe>(&serde_json::to_string(&r).unwrap()).unwrap(), r);
        let trend: ForecastRecipe = serde_json::from_value(json!({"method":"trend","version":1,"bendDays":"7"})).unwrap();
        assert_eq!(trend.method(), ForecastMethod::Trend);
        for invalid in [json!({}), json!({"method":"trend","version":2,"bendDays":"linear"}),
            json!({"method":"trend","version":1,"bendDays":"7","kappa":10}),
            json!({"method":"decayed_probability","version":1,"halfLifeDays":0,"kappa":10,"priorPolicy":"gross_break_even"}),
            json!({"method":"decayed_probability","version":1,"halfLifeDays":14,"kappa":-1,"priorPolicy":"gross_break_even"}),
            json!({"method":"decayed_probability","version":1,"halfLifeDays":14,"kappa":10,"priorPolicy":"historical"})] {
            assert!(serde_json::from_value::<ForecastRecipe>(invalid).is_err());
        }
        #[derive(Serialize, Deserialize)] struct Legacy { #[serde(default, skip_serializing_if="Option::is_none")] forecast: Option<ForecastRecipe> }
        let legacy: Legacy = serde_json::from_str("{}").unwrap();
        assert!(legacy.forecast.is_none());
        assert_eq!(serde_json::to_string(&legacy).unwrap(), "{}");
        assert_eq!(r.identity_key().unwrap(), "forecast:v1:decayed_probability:h14:k10:gross_break_even");
        assert_ne!(r.identity_key().unwrap(), recipe(7.0, 10.0).identity_key().unwrap());
        assert_ne!(r.identity_key().unwrap(), recipe(14.0, 5.0).identity_key().unwrap());
    }
    #[test]
    fn closed_counts_use_utc_raw_cohort_decay_and_payoff_specific_neutral_prior() {
        let result = evaluate_probability(recipe(1.0, 2.0), 2.5, 2*DAY_SECONDS, &[day(0, 4, 0), day(1, 0, 2)]).unwrap();
        let p = result.as_ref().unwrap();
        close(p.weighted_tp, 1.0); close(p.weighted_sl, 1.0);
        close(p.prior_probability, 1.0/3.5); close(p.forecast_r, 1.5/4.0);
        close(p.mean_r, 8.0/6.0); close((1.0+2.5)*p.tp_probability-1.0, p.forecast_r);
        assert_eq!((p.tp_count, p.sl_count), (4,2));
        let alternate = evaluate_probability(recipe(1.0, 2.0), 1.0, 2*DAY_SECONDS, &[day(0, 4, 0),day(1,0,2)]).unwrap();
        close(alternate.as_ref().unwrap().forecast_r, 0.0);
        assert_ne!(p.prior_probability, alternate.as_ref().unwrap().prior_probability);
        assert_eq!(serde_json::from_str::<ProbabilityForecast>(&serde_json::to_string(&result).unwrap()).unwrap(), result);
    }
    #[test]
    fn no_evidence_is_unavailable_but_one_closed_day_is_sufficient() {
        for daily in [&[][..], &[day(0,0,0)][..]] {
            assert_eq!(evaluate_probability(recipe(14.0, 10.0),2.5,DAY_SECONDS,daily).unwrap(),
                ProbabilityForecast::Unavailable { reason: ForecastUnavailableReason::NoClosedOutcomes });
        }
        assert!(evaluate_probability(recipe(14.0,10.0),2.5,DAY_SECONDS,&[day(0,1,0)]).unwrap().as_ref().is_some());
        assert_eq!(evaluate_probability(recipe(f64::MIN_POSITIVE,10.0),2.5,DAY_SECONDS,&[day(0,1,0)]).unwrap(),
            ProbabilityForecast::Unavailable { reason: ForecastUnavailableReason::NoWeightedEvidence });
    }
    #[test]
    fn event_adapter_excludes_delayed_labels_and_future_signals_without_making_losses() {
        let mut acc = ProbabilityAccumulator::new(recipe(1.0,2.0),2.5,DAY_SECONDS).unwrap();
        assert!(!acc.add_outcome(1,DAY_SECONDS+1,ClosedOutcome::StopLoss).unwrap());
        assert!(!acc.add_outcome(DAY_SECONDS+1,DAY_SECONDS,ClosedOutcome::TakeProfit).unwrap());
        assert!(acc.add_outcome(1,DAY_SECONDS,ClosedOutcome::TakeProfit).unwrap());
        let p = acc.finish().unwrap(); let p = p.as_ref().unwrap();
        assert_eq!((p.tp_count,p.sl_count),(1,0)); close(p.weighted_tp,0.5);
    }
    #[test]
    fn frozen_future_is_flat_and_curve_endpoint_is_exact_scalar() {
        let request = ProbabilityCurveRequest { recipe: recipe(14.0,10.0), q: 2.5,
            observation_cutoff: 4*DAY_SECONDS+300, daily: vec![day(0,0,0),day(1,3,1),day(2,1,2)] };
        let curve = probability_curve(&request).unwrap();
        assert!(curve.retrospective); assert_eq!(curve.curve.len(),3);
        assert_eq!(curve.curve[0].forecast_r,None);
        let scalar = evaluate_probability(request.recipe,request.q,request.observation_cutoff,&request.daily).unwrap();
        assert_eq!(curve.estimate,scalar);
        assert_eq!(curve.curve.last().unwrap().at_unix,request.observation_cutoff);
        assert_eq!(curve.curve.last().unwrap().forecast_r,scalar.as_ref().map(|p|p.forecast_r));
        assert_eq!(scalar.at(request.observation_cutoff).unwrap(),scalar.at(request.observation_cutoff+30*DAY_SECONDS).unwrap());
        assert_eq!(scalar.at(request.observation_cutoff-1),Err(ForecastError::BeforeCutoff));
        assert_eq!(serde_json::from_str::<ProbabilityCurveRequest>(&serde_json::to_string(&request).unwrap()).unwrap(),request);
    }
    #[test]
    fn shrinkage_is_not_an_independent_forecast_or_universal_pessimistic_penalty() {
        for (tp,sl) in [(4,1),(1,4)] {
            let p = evaluate_probability(recipe(1e100,10.0),1.0,DAY_SECONDS,&[day(0,tp,sl)]).unwrap();
            let p = p.as_ref().unwrap();
            close(p.forecast_r,5.0/15.0*p.mean_r);
            close(blend_r(Some(p.mean_r),Some(p.forecast_r),0.7).unwrap().unwrap(),(1.0-0.7*10.0/15.0)*p.mean_r);
            assert!(p.forecast_r.abs()<p.mean_r.abs());
        }
    }
    #[test]
    fn baseline_sensitivity_is_bounded_reproducible_and_not_claimed_optimal() {
        let daily=[day(0,0,10),day(14,10,0)];
        let mut values=Vec::new();
        for h in [7.0,14.0,28.0] {
            let mut previous_abs=f64::INFINITY;
            for k in [5.0,10.0,20.0] {
                let a=evaluate_probability(recipe(h,k),2.5,15*DAY_SECONDS,&daily).unwrap();
                assert_eq!(a,evaluate_probability(recipe(h,k),2.5,15*DAY_SECONDS,&daily).unwrap());
                let p=a.as_ref().unwrap(); assert!((-1.0..=2.5).contains(&p.forecast_r));
                assert!(p.forecast_r.abs()<previous_abs); previous_abs=p.forecast_r.abs();
                if k==10.0 {values.push(p.forecast_r);}
            }
        }
        assert!(values[0]>values[1] && values[1]>values[2]);
    }
    #[test]
    fn scalar_event_order_and_aggregated_daily_counts_have_identical_forecast_bits() {
        let r=ForecastRecipe::engineering_baseline();let c=30*DAY_SECONDS;
        let daily=[day(0,70,30),day(1,80,20),day(14,9,50)];
        let aggregated=evaluate_probability(r,2.5,c,&daily).unwrap();
        let mut events=ProbabilityAccumulator::new(r,2.5,c).unwrap();
        for d in daily.into_iter().rev() {
            for _ in 0..d.sl_count {events.add_day(ProbabilityDay{sl_count:1,tp_count:0,..d}).unwrap();}
            for _ in 0..d.tp_count {events.add_day(ProbabilityDay{sl_count:0,tp_count:1,..d}).unwrap();}
        }
        assert_eq!(events.finish().unwrap(),aggregated);
    }
    #[test]
    fn exact_weight_sum_rounds_subnormal_ties_carries_and_large_counts_without_growth() {
        for (weight,count) in [(1.0,1),(1.0,u64::MAX),(0.5,3),(f64::MIN_POSITIVE,7),
            (f64::from_bits(1),1),(f64::from_bits(1),1u64<<63),(0.6095068271022377,10000)] {
            let mut sum=ExactWeightSum::default();sum.add(weight,count).unwrap();
            // Powers of two are exact across the whole supported count range;
            // the arbitrary fractional case uses a small exactly-converted N.
            assert_eq!(sum.value().to_bits(),(weight*count as f64).to_bits());
        }
        let mut sum=ExactWeightSum::default();sum.add(1.0,1).unwrap();sum.add(2f64.powi(-53),1).unwrap();
        assert_eq!(sum.value(),1.0); // ties to even
        sum.add(f64::from_bits(1),1).unwrap();assert_eq!(sum.value(),f64::from_bits(1.0f64.to_bits()+1));
        let mut sum=ExactWeightSum::default();sum.add(1.0,1).unwrap();sum.add(2f64.powi(-52),1).unwrap();sum.add(2f64.powi(-53),1).unwrap();
        assert_eq!(sum.value(),f64::from_bits(1.0f64.to_bits()+2));
        assert_eq!(std::mem::size_of::<ExactWeightSum>(),144);
        assert!(std::mem::size_of::<ProbabilityAccumulator>()<512);
    }
    #[test]
    fn missing_components_do_not_renormalize_or_expand_ui_lambda_domain() {
        assert_eq!(blend_r(Some(0.4),None,0.7).unwrap(),None);
        assert_eq!(blend_r(Some(0.4),None,0.0).unwrap(),Some(0.4));
        assert_eq!(blend_r(None,Some(0.8),1.0).unwrap(),Some(0.8));
        assert_eq!(decision_r(TrendMode::Mean,Some(0.4),None,TrendBlendLambda::default()).unwrap(),Some(0.4));
        assert_eq!(decision_r(TrendMode::Forecast,None,Some(0.8),TrendBlendLambda::default()).unwrap(),Some(0.8));
        close(decision_r(TrendMode::Blend,Some(0.4),Some(0.8),TrendBlendLambda::default()).unwrap().unwrap(),0.68);
        assert!(TrendBlendLambda::try_from(0.0).is_err()); assert!(TrendBlendLambda::try_from(1.0).is_err());
        assert!(blend_r(Some(f64::NAN),Some(0.0),0.5).is_err());
    }
    #[test]
    fn bounds_invalid_inputs_and_counter_overflow_fail_without_partial_mutation() {
        assert_eq!(evaluate_probability(recipe(14.0,10.0),0.0,DAY_SECONDS,&[]),Err(ForecastError::InvalidPayoff));
        assert_eq!(evaluate_probability(recipe(14.0,10.0),1.0,DAY_SECONDS,&[day(0,1,0),day(0,1,0)]),Err(ForecastError::UnorderedDays));
        assert_eq!(evaluate_probability(recipe(14.0,10.0),1.0,DAY_SECONDS,&[ProbabilityDay{day_start_unix:1,tp_count:1,sl_count:0}]),Err(ForecastError::InvalidDay));
        assert_eq!(evaluate_probability(recipe(14.0,10.0),1.0,DAY_SECONDS,&vec![day(0,0,0);MAX_FORECAST_DAYS+1]),Err(ForecastError::TooManyDays));
        assert_eq!(evaluate_probability(recipe(14.0,10.0),1.0,(MAX_FORECAST_DAYS as i64+1)*DAY_SECONDS,&[day(0,1,0),day(MAX_FORECAST_DAYS as i64,0,1)]),Err(ForecastError::PeriodTooLong));
        assert!(evaluate_probability(recipe(14.0,10.0),1.0,(MAX_FORECAST_DAYS as i64+7)*DAY_SECONDS,&[day(0,1,0)]).is_ok());
        let mut a=ProbabilityAccumulator::new(recipe(14.0,10.0),1.0,DAY_SECONDS).unwrap();
        a.add_day(day(0,u64::MAX,0)).unwrap(); let before=a.finish().unwrap();
        assert_eq!(a.add_day(day(0,0,1)),Err(ForecastError::CountOverflow));
        assert_eq!(a.finish().unwrap(),before);
    }
}
