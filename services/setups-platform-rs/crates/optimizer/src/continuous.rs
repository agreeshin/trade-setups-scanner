//! Additive causal kernels. The caller owns one immutable StudyInput through
//! search, service replay persistence, then analytics. No IO or shared cache.
use crate::evaluation::{
    Control, InputStats, accumulate_setup_with_daily, empty_matrix, limit, numerical_budget, validate_input,
};
use crate::trend::DailyMatrix;
use crate::frequency::FrequencyGate;
use crate::validation::{invalid, parse_params, validate_analytics, validate_request_mode};
use crate::*;
use serde::{Deserialize, Serialize};
use setups_analytics::{AnalyticsSetup, CausalCluster, TradeDirection, find_candle_by_open_time};
use setups_detector::PriceVolumeSpikeDetector;
use setups_domain::{PushStatus, SpikeSetup};
use setups_domain::expectancy::{
    EXPECTANCY_MAX_PERIOD_DAYS, EXPECTANCY_MAX_POINTS, UTC_DAY_SECONDS, UtcDailyInput,
    utc_midnight_unix,
};
use std::mem::size_of;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

pub use setups_domain::{
    AdaptiveRecommendationSnapshot, RecommendationDirection, RecommendationProvenance,
};

pub const DEFAULT_CONTINUOUS_MEAN_R: f64 = 0.15;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ContinuousContext {
    /// Close-time cutoff fixed at actual start (spec.to_unix), shared by every
    /// instrument/stage. Candles closing later are rejected, never trimmed.
    pub observation_cutoff: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContinuousInstrumentReplay {
    pub instrument: StudyInstrument,
    /// All raw detector WATCHs in the original period, before analytics gates.
    pub setups: Vec<SpikeSetup>,
    /// Last raw signal before fromUnix; causal clustering MUST start with it.
    /// Explicit null means proven-empty prehistory; a missing field is unknown,
    /// not permission to restart clustering or use the end-of-period raw seed.
    #[serde(deserialize_with = "Deserialize::deserialize")]
    pub previous_raw_close_unix: Option<i64>,
    pub last_raw_close_unix: Option<i64>,
    pub evaluated_minutes: u64,
    pub warmup_minutes: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContinuousReplay {
    pub from_unix: i64,
    pub to_unix: i64,
    pub context: ContinuousContext,
    pub params: ParamMap,
    pub provenance: Provenance,
    pub instruments: Vec<ContinuousInstrumentReplay>,
}

/// Compact generation metadata, O(instruments), with no copied raw setup set.
/// Both raw seeds describe the state immediately BEFORE next_open_unix.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ContinuousInstrumentBootstrap {
    pub instrument: StudyInstrument,
    /// Detector cooldown uses candle OPEN time, not raw CLOSE time.
    pub last_raw_open_unix: Option<i64>,
    /// Causal cluster state, including unpublished/filtered raw signals.
    pub last_raw_cluster_time: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ContinuousBootstrap {
    /// Actual winning candidate's original detector initialization boundary
    /// on the OPEN axis, NOT the maximum warmup envelope of all search domains.
    /// A cold rebuild from here starts with both raw seeds None.
    pub detector_warmup_from_unix: i64,
    /// Original exact historical CLOSE-time export period [from,to).
    pub export_from_unix: i64,
    pub export_to_unix: i64,
    pub observation_cutoff: i64,
    /// First OPEN minute not processed by the detector materialization. May
    /// precede observationCutoff by one minute when exportToUnix is aligned.
    /// The per-instrument seeds below belong HERE, not at warmup start.
    pub next_open_unix: i64,
    /// Earliest live CLOSE strictly after the frozen observation cutoff.
    /// Rebuilding/catching up must still advance all earlier raw state.
    pub emit_from_close_unix: i64,
    pub provenance: Provenance,
    pub instruments: Vec<ContinuousInstrumentBootstrap>,
    /// Missing only on legacy generations. Never interpret absence as a matching prefix.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frozen_prefix: Option<ContinuousFrozenPrefix>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ContinuousFrozenPrefix {
    pub version: u32,
    pub from_open_unix: i64,
    pub to_open_unix: i64,
    pub instruments: Vec<ContinuousInstrumentPrefix>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ContinuousInstrumentPrefix {
    pub instrument: StudyInstrument,
    pub bar_count: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetectorPrefixFingerprint {
    pub bar_count: u64,
    pub sha256: String,
}

/// Canonical v1: domain, signed i64 BE OPEN bounds, u64 BE row count; then each
/// ordered i64 BE timestamp and (i128 BE coefficient, u32 BE scale) for close
/// and turnover. Trailing decimal zeroes are removed; zero has scale zero.
/// Bounds plus timestamps bind gaps. No float conversion or OHLC reconstruction.
pub fn detector_prefix_fingerprint(bars: &[setups_domain::DetectorBar], from_open_unix: i64, to_open_unix: i64) -> Result<DetectorPrefixFingerprint, StudyError> {
    use sha2::{Digest, Sha256};
    use setups_domain::decimal::Decimal;
    if from_open_unix > to_open_unix || from_open_unix%60!=0 || to_open_unix%60!=0 {
        return Err(StudyError::InvalidInput("invalid frozen detector prefix bounds".into()));
    }
    let selected = |bar: &&setups_domain::DetectorBar| bar.open_time_unix>=from_open_unix && bar.open_time_unix<to_open_unix;
    let bar_count = bars.iter().filter(selected).count() as u64;
    let mut hash = Sha256::new();
    hash.update(b"continuous-detector-prefix-v1\0");
    hash.update(from_open_unix.to_be_bytes()); hash.update(to_open_unix.to_be_bytes()); hash.update(bar_count.to_be_bytes());
    let mut previous = None;
    for bar in bars.iter().filter(selected) {
        if bar.open_time_unix%60!=0 || previous.is_some_and(|time| time>=bar.open_time_unix) {
            return Err(StudyError::InvalidInput("frozen detector prefix is not strictly ordered on minute boundaries".into()));
        }
        previous = Some(bar.open_time_unix);
        hash.update(bar.open_time_unix.to_be_bytes());
        for text in [&bar.close,&bar.quote_turnover] {
            let mut number = Decimal::parse(text).map_err(|_|StudyError::InvalidInput("invalid exact decimal in frozen detector prefix".into()))?;
            while number.scale>0 && number.int%10==0 {number.int/=10;number.scale-=1;}
            hash.update(number.int.to_be_bytes()); hash.update(number.scale.to_be_bytes());
        }
    }
    Ok(DetectorPrefixFingerprint {bar_count,sha256:format!("{:x}",hash.finalize())})
}

/// Capture while the one immutable input is still owned by the generation.
pub fn continuous_bootstrap_with_frozen_prefix(spec: &StudyRequest, replay: &ContinuousReplay, input: &StudyInput) -> Result<ContinuousBootstrap, StudyError> {
    let mut bootstrap = continuous_bootstrap(spec,replay)?;
    if input.snapshot_hash!=replay.provenance.snapshot_hash || input.captured_at!=replay.provenance.captured_at
        || input.instruments.len()!=spec.instruments.len() || input.instruments.len()>1000 {
        return Err(StudyError::InvalidInput("frozen prefix input differs from generation provenance/scope".into()));
    }
    let mut instruments = Vec::with_capacity(input.instruments.len());
    for (raw,instrument) in input.instruments.iter().zip(&spec.instruments) {
        if &raw.instrument!=instrument {return Err(StudyError::InvalidInput("frozen prefix instrument order differs from generation".into()));}
        let digest = detector_prefix_fingerprint(&raw.detector_bars,bootstrap.detector_warmup_from_unix,bootstrap.next_open_unix)?;
        instruments.push(ContinuousInstrumentPrefix {instrument:instrument.clone(),bar_count:digest.bar_count,sha256:digest.sha256});
    }
    bootstrap.frozen_prefix = Some(ContinuousFrozenPrefix {version:1,from_open_unix:bootstrap.detector_warmup_from_unix,to_open_unix:bootstrap.next_open_unix,instruments});
    Ok(bootstrap)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContinuousCellDiagnostics {
    pub tp_pct: f64,
    pub sl_pct: f64,
    pub future_entry_count: usize,
    pub missing_entry_count: usize,
    pub right_censored_count: usize,
    pub observation_gap_count: usize,
    pub horizon_expired_count: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContinuousEvaluation {
    /// In this new path unclosedCount means observed horizon-expired only;
    /// censored/gapped opened entries are reported separately below.
    pub evaluation: CandidateEvaluation,
    pub diagnostics: Vec<ContinuousCellDiagnostics>,
}

/// The caller supplies the already selected FINAL frozen pair. This opt-in
/// capture does not select/rank a cell or change any admission/scientific value.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContinuousCaptureCell {
    pub tp_pct: f64,
    pub sl_pct: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContinuousSelectedDaily {
    pub tp_pct: f64,
    pub sl_pct: f64,
    pub points: Vec<UtcDailyInput>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CapturedContinuousAnalysis {
    pub evaluation: ContinuousEvaluation,
    pub selected_daily: Option<ContinuousSelectedDaily>,
}

#[derive(Default, Clone, Copy)]
struct DailyCaptureBucket {
    rows: u64,
    tp: u64,
    sl: u64,
    sum_r: f64,
}

/// One analysis owns these dense, bounded counters. They retain neither input
/// candles nor setup references. Missing dates are omitted on finish; a date
/// with rows but no observed closes is retained as mean_r=None (line break).
/// None on all search/trial paths allocates nothing. Drop frees cancellation,
/// deadline and failure state; no global/per-instrument cache survives a call.
struct SelectedDailyCapture {
    pair: ContinuousCaptureCell,
    cell_index: usize,
    origin: i64,
    to_unix: i64,
    days: Vec<DailyCaptureBucket>,
}

impl SelectedDailyCapture {
    fn new(spec: &StudyRequest, pair: ContinuousCaptureCell, working_base: usize, limits: &StudyLimits) -> Result<Self, StudyError> {
        spec.to_unix.checked_sub(spec.from_unix)
            .filter(|duration| *duration > 0 && *duration <= EXPECTANCY_MAX_PERIOD_DAYS * UTC_DAY_SECONDS)
            .ok_or_else(|| invalid("selected daily capture exceeds permitted research period"))?;
        let tp = spec.tp_levels.iter().position(|level| *level == pair.tp_pct);
        let sl = spec.sl_levels.iter().position(|level| *level == pair.sl_pct);
        let (Some(tp), Some(sl)) = (tp, sl) else {
            return Err(invalid("selected daily capture pair is outside original axes"));
        };
        if !(pair.tp_pct / pair.sl_pct).is_finite() {
            return Err(invalid("selected daily capture R must be finite"));
        }
        let origin = utc_midnight_unix(spec.from_unix)
            .map_err(|_| invalid("invalid selected daily capture UTC origin"))?;
        let count = ((spec.to_unix - 1 - origin) / UTC_DAY_SECONDS + 1) as usize;
        limit("selected_daily_points", count, EXPECTANCY_MAX_POINTS)?;
        let capture_bytes = count.saturating_mul(size_of::<DailyCaptureBucket>() + size_of::<UtcDailyInput>())
            .saturating_add(size_of::<Self>() + size_of::<ContinuousSelectedDaily>());
        limit("continuous_working_bytes", working_base.saturating_add(capture_bytes), limits.max_working_bytes)?;
        Ok(Self { pair, cell_index: tp * spec.sl_levels.len() + sl, origin,
            to_unix: spec.to_unix, days: vec![DailyCaptureBucket::default(); count] })
    }

    fn working_bytes(&self) -> usize {
        // Reserve dense scratch AND the maximum compact output simultaneously.
        // Output has exact capacity; neither Vec grows beyond touched days.
        self.days.len().saturating_mul(size_of::<DailyCaptureBucket>() + size_of::<UtcDailyInput>())
            .saturating_add(size_of::<Self>() + size_of::<ContinuousSelectedDaily>())
    }

    fn record(&mut self, signal_close: i64, before: (usize, usize), after: &MatrixCell) -> Result<(), StudyError> {
        let tp = after.tp_count.checked_sub(before.0);
        let sl = after.sl_count.checked_sub(before.1);
        let (Some(tp), Some(sl)) = (tp, sl) else {
            return Err(StudyError::InvalidInput("selected daily counter moved backwards".into()));
        };
        if tp + sl > 1 || signal_close < self.origin || signal_close >= self.to_unix {
            return Err(StudyError::InvalidInput("invalid selected daily outcome delta/time".into()));
        }
        let day = ((signal_close - self.origin) / UTC_DAY_SECONDS) as usize;
        let bucket = &mut self.days[day];
        bucket.rows += 1;
        bucket.tp += tp as u64;
        bucket.sl += sl as u64;
        if tp == 1 { bucket.sum_r += self.pair.tp_pct / self.pair.sl_pct; }
        if sl == 1 { bucket.sum_r -= 1.0; }
        if !bucket.sum_r.is_finite() {
            return Err(StudyError::InvalidInput("nonfinite selected daily outcome sum".into()));
        }
        Ok(())
    }

    fn finish(self, matrix: &[MatrixCell], control: &Control<'_>) -> Result<Option<ContinuousSelectedDaily>, StudyError> {
        let mut points = Vec::with_capacity(self.days.len());
        let (mut tp, mut sl) = (0u64, 0u64);
        for (index, bucket) in self.days.into_iter().enumerate() {
            if index % 128 == 0 && control.stopped() { return Ok(None); }
            tp += bucket.tp; sl += bucket.sl;
            if bucket.rows > 0 {
                let closed = bucket.tp + bucket.sl;
                points.push(UtcDailyInput { day_start_unix: self.origin + index as i64 * UTC_DAY_SECONDS,
                    row_count: bucket.rows, tp_count: bucket.tp, sl_count: bucket.sl,
                    mean_r: (closed > 0).then(|| bucket.sum_r / closed as f64) });
            }
        }
        let cell = &matrix[self.cell_index];
        if (tp, sl) != (cell.tp_count as u64, cell.sl_count as u64) {
            return Err(StudyError::InvalidInput("selected daily capture differs from final cell counts".into()));
        }
        Ok(Some(ContinuousSelectedDaily { tp_pct: self.pair.tp_pct, sl_pct: self.pair.sl_pct, points }))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RobustCell {
    pub tp_pct: f64,
    pub sl_pct: f64,
    pub raw_r: f64,
    pub mean_r: f64,
    pub robust_score: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forecast_r: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forecast_matrix_r: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blend_r: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blend_matrix_r: Option<f64>,
}

pub fn validate_continuous_request(
    spec: &StudyRequest,
    context: ContinuousContext,
) -> Result<(), StudyError> {
    validate_request_mode(
        spec,
        &default_parameter_descriptors(&spec.algorithm_id)?,
        true,
    )?;
    if spec.setup_maturity != 0.0 {
        return Err(invalid("continuous maturity must be locked at zero"));
    }
    if context.observation_cutoff < 0
        || context.observation_cutoff % 60 != 0
        || context.observation_cutoff > spec.to_unix
    {
        return Err(invalid(
            "observationCutoff must be a closed minute <= actual start",
        ));
    }
    Ok(())
}

// A fixed causal prehistory span covers every allowed cluster gap and cooldown.
// Every candidate still has its OWN baseline-length detector warmup before it.
fn bootstrap_minutes(spec: &StudyRequest) -> i64 {
    let enabled = spec.analytics_params.cluster_enabled
        || spec
            .analytics_search
            .get("clusterEnabled")
            .is_some_and(|values| values.iter().any(|value| value.as_bool() == Some(true)));
    let gap = if enabled {
        spec.analytics_search
            .get("clusterGapMinutes")
            .into_iter()
            .flatten()
            .filter_map(|value| value.as_i64())
            .fold(spec.analytics_params.cluster_gap_minutes, i64::max)
    } else {
        0
    };
    let cooldown = spec
        .algorithm_search
        .get("cooldownMinutes")
        .into_iter()
        .flatten()
        .filter_map(|value| value.as_i64())
        .fold(
            spec.algorithm_params["cooldownMinutes"]
                .as_i64()
                .unwrap_or(0),
            i64::max,
        );
    gap + cooldown
}

fn first_signal_open(spec: &StudyRequest) -> i64 {
    ((spec.from_unix + 59) / 60) * 60 - 60
}

fn detector_warmup_from(spec: &StudyRequest, required_minutes: i64) -> i64 {
    first_signal_open(spec) - (required_minutes + bootstrap_minutes(spec)) * 60
}

fn detector_export_to_open(spec: &StudyRequest, context: ContinuousContext) -> i64 {
    ((spec.to_unix - 1) / 60 * 60).min(context.observation_cutoff)
}

/// Extract this once BEFORE dropping the materialized replay; persist it next
/// to the winner, not inside each recommendation or duplicated raw setup JSON.
///
/// Worker mapping: bootstrap_from_unix = detector_warmup_from_unix;
/// initial_state = { next_open_unix, this instrument's two raw seeds };
/// emit_from_close_unix = emit_from_close_unix. Attach generation_id outside
/// this pure layer. Resume rebuilds max(bootstrap_from, next_open-required*60+60)
/// through next_open using the SAME frozen/revalidated prefix, without emitting
/// or advancing raw seeds for that prefix. Then process next_open onwards.
/// These scalar seeds are not a serialized detector window: a changed/repaired
/// prefix cannot silently be called an exact reconstruction of frozen state.
pub fn continuous_bootstrap(
    spec: &StudyRequest,
    replay: &ContinuousReplay,
) -> Result<ContinuousBootstrap, StudyError> {
    validate_continuous_request(spec, replay.context)?;
    let params = parse_params(&spec.algorithm_id, &replay.params)?;
    if replay.from_unix != spec.from_unix
        || replay.to_unix != spec.to_unix
        || replay.provenance.optimizer_version != CONTINUOUS_OPTIMIZER_VERSION
        || replay.provenance.seed != spec.seed
        || replay.provenance.snapshot_hash.is_empty()
        || replay.provenance.snapshot_hash.len() > 256
        || replay.provenance.captured_at.is_empty()
        || replay.provenance.captured_at.len() > 64
        || replay.instruments.len() != spec.instruments.len()
    {
        return Err(StudyError::InvalidInput(
            "bootstrap differs from original continuous scope/provenance".into(),
        ));
    }
    let next_open_unix = detector_export_to_open(spec, replay.context);
    let mut instruments = Vec::with_capacity(spec.instruments.len());
    for (raw, instrument) in replay.instruments.iter().zip(&spec.instruments) {
        let expected_last = raw
            .setups
            .last()
            .map(|setup| setup.close_time_unix)
            .or(raw.previous_raw_close_unix);
        if &raw.instrument != instrument
            || raw
                .previous_raw_close_unix
                .is_some_and(|time| time >= spec.from_unix)
            || raw.last_raw_close_unix != expected_last
            || expected_last.is_some_and(|time| {
                time < 0
                    || time % 60 != 0
                    || time > replay.context.observation_cutoff
                    || time - 60 >= next_open_unix
            })
        {
            return Err(StudyError::InvalidInput(
                "invalid materialized bootstrap raw seed".into(),
            ));
        }
        instruments.push(ContinuousInstrumentBootstrap {
            instrument: instrument.clone(),
            last_raw_open_unix: expected_last.map(|time| time - 60),
            last_raw_cluster_time: expected_last,
        });
    }
    Ok(ContinuousBootstrap {
        detector_warmup_from_unix: detector_warmup_from(
            spec,
            i64::from(params.baseline_long_days()) * 1440,
        ),
        export_from_unix: spec.from_unix,
        export_to_unix: spec.to_unix,
        observation_cutoff: replay.context.observation_cutoff,
        next_open_unix,
        emit_from_close_unix: replay.context.observation_cutoff + 60,
        provenance: replay.provenance.clone(),
        instruments,
        frozen_prefix: None,
    })
}

/// Strict OPEN-time read envelope; unlike one-shot, it never extends beyond
/// observationCutoff. Call before acquiring the one frozen StudyInput.
pub fn required_continuous_input_bounds(
    spec: &StudyRequest,
    context: ContinuousContext,
) -> Result<InputBounds, StudyError> {
    validate_continuous_request(spec, context)?;
    let mut bounds = crate::validation::input_bounds_inner(spec)?;
    bounds.max_warmup_minutes += bootstrap_minutes(spec);
    bounds.candles_from_unix = first_signal_open(spec);
    bounds.detector_from_unix = bounds.candles_from_unix - bounds.max_warmup_minutes * 60;
    bounds.detector_to_unix = detector_export_to_open(spec, context);
    bounds.candles_to_unix = context.observation_cutoff;
    bounds.from_unix = bounds.detector_from_unix;
    bounds.to_unix = context.observation_cutoff;
    Ok(bounds)
}

pub(crate) fn validate_frozen_cutoff(
    input: &StudyInput,
    context: ContinuousContext,
    cancel: &CancelToken,
) -> Result<bool, StudyError> {
    for data in &input.instruments {
        if cancel.is_cancelled() {
            return Ok(false);
        }
        // validate_input establishes ascending order. Last items suffice.
        if data.detector_bars.last().is_some_and(|bar| {
            bar.open_time_unix
                .checked_add(60)
                .is_none_or(|close| close > context.observation_cutoff)
        }) || data.candles.last().is_some_and(|bar| {
            bar.open_time_unix
                .checked_add(60)
                .is_none_or(|close| close > context.observation_cutoff)
        }) {
            return Err(StudyError::InvalidInput(
                "snapshot contains a candle closing after observationCutoff".into(),
            ));
        }
    }
    Ok(true)
}

fn replay_bytes(replay: &ContinuousReplay) -> usize {
    // Conservatively reserve Vec growth, parameter/detail strings and identity
    // buffers. All source string sizes were bounded by input/module validation.
    replay.instruments.iter().fold(
        4096 + replay.instruments.capacity() * (size_of::<ContinuousInstrumentReplay>() + 720),
        |bytes, data| {
            bytes.saturating_add(
                data.setups
                    .capacity()
                    .saturating_mul(size_of::<SpikeSetup>() * 3 + 4096),
            )
        },
    )
}

pub(crate) fn materialize_inner(
    spec: &StudyRequest,
    input: &StudyInput,
    candidate: &StudyCandidate,
    context: ContinuousContext,
    limits: &StudyLimits,
    stats: &InputStats,
    control: &Control<'_>,
) -> Result<Option<ContinuousReplay>, StudyError> {
    let params = parse_params(&spec.algorithm_id, &candidate.params)?;
    numerical_budget(&params, stats)?;
    let required = i64::from(params.baseline_long_days()) * 1440;
    let detector_bytes =
        (required as usize + 1) * 128 + (params.window_minutes() as usize + 1) * 128;
    let matrix_bytes = spec.tp_levels.len() * spec.sl_levels.len() * 256;
    limit(
        "continuous_working_bytes",
        detector_bytes.saturating_add(matrix_bytes).saturating_add(
            spec.instruments.len() * (size_of::<ContinuousInstrumentReplay>() + 720) + 4096,
        ),
        limits.max_working_bytes,
    )?;
    let mut replay = ContinuousReplay {
        from_unix: spec.from_unix,
        to_unix: spec.to_unix,
        context,
        params: candidate.params.clone(),
        provenance: Provenance {
            optimizer_version: CONTINUOUS_OPTIMIZER_VERSION.into(),
            seed: spec.seed,
            snapshot_hash: input.snapshot_hash.clone(),
            captured_at: input.captured_at.clone(),
        },
        instruments: Vec::with_capacity(spec.instruments.len()),
    };
    let first_open = first_signal_open(spec);
    let warmup_from = detector_warmup_from(spec, required);
    let to_open = detector_export_to_open(spec, context);
    let mut setup_count = 0;
    for identity in &spec.instruments {
        if control.stopped() {
            return Ok(None);
        }
        control.detector_passes.fetch_add(1, Ordering::Relaxed);
        let data = input
            .instruments
            .iter()
            .find(|data| &data.instrument == identity)
            .expect("validated scope");
        let mut detector = PriceVolumeSpikeDetector::with_params(params.clone(), None)
            .map_err(|error| StudyError::Detector(error.to_string()))?;
        let mut result = ContinuousInstrumentReplay {
            instrument: identity.clone(),
            setups: Vec::new(),
            previous_raw_close_unix: None,
            last_raw_close_unix: None,
            evaluated_minutes: 0,
            warmup_minutes: 0,
        };
        let start = data
            .detector_bars
            .partition_point(|bar| bar.open_time_unix < warmup_from);
        for (offset, bar) in data.detector_bars[start..].iter().enumerate() {
            if offset % 64 == 0 && control.stopped() {
                return Ok(None);
            }
            if bar.open_time_unix >= to_open {
                break;
            }
            let pushed = detector
                .push(bar)
                .map_err(|error| StudyError::Detector(error.to_string()))?;
            if pushed.status == PushStatus::Warmup {
                result.warmup_minutes += 1;
            }
            if bar.open_time_unix >= first_open && pushed.status == PushStatus::Evaluated {
                result.evaluated_minutes += 1;
            }
            if let Some(setup) = pushed.setup {
                result.last_raw_close_unix = Some(setup.close_time_unix);
                if setup.close_time_unix < spec.from_unix {
                    result.previous_raw_close_unix = Some(setup.close_time_unix);
                } else {
                    setup_count += 1;
                    limit(
                        "setups_per_candidate",
                        setup_count,
                        limits.max_setups_per_candidate,
                    )?;
                    let next_capacity = if result.setups.len() == result.setups.capacity() {
                        result.setups.capacity().max(4).saturating_mul(2)
                    } else {
                        result.setups.capacity()
                    };
                    let bytes = replay_bytes(&replay).saturating_add(
                        next_capacity.saturating_mul(size_of::<SpikeSetup>() * 3 + 4096),
                    );
                    limit(
                        "continuous_working_bytes",
                        bytes.saturating_add(detector_bytes + matrix_bytes),
                        limits.max_working_bytes,
                    )?;
                    result.setups.push(setup);
                }
            }
        }
        replay.instruments.push(result);
    }
    Ok(Some(replay))
}

/// Materialize actual raw service replay output; caller persists it before
/// calling analyze_continuous. None means cancelled/deadline, NOT a partial set.
pub fn materialize_continuous(
    spec: &StudyRequest,
    input: &StudyInput,
    candidate: &StudyCandidate,
    context: ContinuousContext,
    limits: &StudyLimits,
    cancel: &CancelToken,
) -> Result<Option<ContinuousReplay>, StudyError> {
    validate_continuous_request(spec, context)?;
    validate_analytics(&candidate.analytics)?;
    let Some(stats) = validate_input(spec, input, limits, cancel)? else {
        return Ok(None);
    };
    if !validate_frozen_cutoff(input, context, cancel)? {
        return Ok(None);
    }
    let passes = AtomicUsize::new(0);
    let control = Control {
        cancel,
        deadline: Instant::now() + Duration::from_secs(spec.max_seconds),
        detector_passes: &passes,
    };
    materialize_inner(spec, input, candidate, context, limits, &stats, &control)
}

// Contiguous evidence is required up to a touch. A later observed TP must not
// fabricate an outcome over an earlier missing minute that could have hit SL.
// TODO: fuse this cancellable evidence walk into the shared first-hit scan if
// production profiling shows the extra horizon pass dominates trial cost.
#[allow(clippy::too_many_arguments)]
fn accumulate_observed(
    spec: &StudyRequest,
    analytics: &AnalyticsParams,
    data: &InstrumentInput,
    setup: &AnalyticsSetup,
    context: ContinuousContext,
    evaluation: &mut ContinuousEvaluation,
    control: &Control<'_>,
    daily: Option<&mut DailyMatrix>,
) -> bool {
    let entry_open = setup.open_time_unix + analytics.time_shift_minutes * 60;
    let mut end_index = data.candles.len();
    let mut gap = false;
    let mut expired = false;
    let entry = find_candle_by_open_time(&data.candles, entry_open);
    if let Some(entry) = entry {
        let mut expected = entry.open_time_unix + 60;
        let end = expected + spec.horizon_minutes * 60;
        let start = data
            .candles
            .partition_point(|bar| bar.open_time_unix < expected);
        for (index, candle) in data.candles.iter().enumerate().skip(start) {
            if index % 128 == 0 && control.stopped() {
                return false;
            }
            if expected >= end {
                end_index = index;
                break;
            }
            if candle.open_time_unix != expected {
                gap = true;
                end_index = index;
                break;
            }
            expected += 60;
        }
        // A gap before the common cutoff is missing evidence, not merely the
        // expected future beyond it. Candles are already cutoff-validated.
        if expected < end && expected + 60 <= context.observation_cutoff {
            gap = true;
        }
        expired = expected >= end;
    }
    let before: Vec<_> = evaluation
        .evaluation
        .matrix
        .iter()
        .map(|cell| cell.unclosed_count)
        .collect();
    if !accumulate_setup_with_daily(
        spec,
        analytics,
        &data.candles[..end_index],
        setup,
        &mut evaluation.evaluation.matrix,
        &mut evaluation.evaluation.summary.selected_setup_count,
        control,
        daily,
    ) {
        return false;
    }
    for ((cell, diagnostic), before) in evaluation
        .evaluation
        .matrix
        .iter_mut()
        .zip(&mut evaluation.diagnostics)
        .zip(before)
    {
        if entry.is_none() {
            if entry_open + 60 > context.observation_cutoff {
                diagnostic.future_entry_count += 1;
            } else {
                diagnostic.missing_entry_count += 1;
            }
        } else if cell.unclosed_count > before {
            if expired {
                diagnostic.horizon_expired_count += 1;
            } else {
                cell.unclosed_count -= 1;
                if gap {
                    diagnostic.observation_gap_count += 1;
                } else {
                    diagnostic.right_censored_count += 1;
                }
            }
        }
    }
    true
}

pub(crate) fn analyze_inner(
    spec: &StudyRequest,
    input: &StudyInput,
    replay: &ContinuousReplay,
    candidate: &StudyCandidate,
    limits: &StudyLimits,
    control: &Control<'_>,
) -> Result<Option<ContinuousEvaluation>, StudyError> {
    // Mean search paths never allocate daily capture; forecast trials own their
    // bounded all-cell buckets independently of final selected-pair reporting.
    Ok(analyze_inner_with_daily(spec, input, replay, candidate, limits, control, None)?
        .map(|result| result.evaluation))
}

fn analyze_inner_with_daily(
    spec: &StudyRequest,
    input: &StudyInput,
    replay: &ContinuousReplay,
    candidate: &StudyCandidate,
    limits: &StudyLimits,
    control: &Control<'_>,
    capture: Option<ContinuousCaptureCell>,
) -> Result<Option<CapturedContinuousAnalysis>, StudyError> {
    if control.stopped() {
        return Ok(None);
    }
    validate_analytics(&candidate.analytics)?;
    let working_base = replay_bytes(replay)
        .saturating_add(spec.tp_levels.len() * spec.sl_levels.len() * 256)
        .saturating_add(DailyMatrix::working_bytes(spec)?);
    let mut capture = capture
        .map(|pair| SelectedDailyCapture::new(spec, pair, working_base, limits))
        .transpose()?;
    limit(
        "continuous_working_bytes",
        working_base.saturating_add(capture.as_ref().map_or(0, SelectedDailyCapture::working_bytes)),
        limits.max_working_bytes,
    )?;
    let mut daily = if !spec.trend_mode.is_mean() { Some(DailyMatrix::with_cutoff(spec, limits,replay.context.observation_cutoff)?) } else { None };
    let frequency = FrequencyGate::new(spec)?;
    let matrix = empty_matrix(spec);
    let diagnostics = matrix
        .iter()
        .map(|cell| ContinuousCellDiagnostics {
            tp_pct: cell.tp_pct,
            sl_pct: cell.sl_pct,
            future_entry_count: 0,
            missing_entry_count: 0,
            right_censored_count: 0,
            observation_gap_count: 0,
            horizon_expired_count: 0,
        })
        .collect();
    let mut result = ContinuousEvaluation {
        evaluation: CandidateEvaluation {
            summary: TrialSummary {
                score: None,
                setup_count: 0,
                selected_setup_count: 0,
                avg_setups_per_day: None,
                required_setup_count: frequency.required_setup_count,
                evaluated_minutes: 0,
                warmup_minutes: 0,
            },
            matrix,
        },
        diagnostics,
    };
    for raw in &replay.instruments {
        let data = input
            .instruments
            .iter()
            .find(|data| data.instrument == raw.instrument)
            .expect("validated scope");
        let mut cluster = CausalCluster::new(
            candidate.analytics.cluster_gap_minutes * 60,
            raw.previous_raw_close_unix,
        )
        .map_err(|error| StudyError::InvalidInput(error.to_string()))?;
        result.evaluation.summary.setup_count += raw.setups.len();
        result.evaluation.summary.evaluated_minutes += raw.evaluated_minutes;
        result.evaluation.summary.warmup_minutes += raw.warmup_minutes;
        for setup in &raw.setups {
            if control.stopped() {
                return Ok(None);
            }
            let first = cluster
                .observe_raw(setup.close_time_unix)
                .map_err(|error| StudyError::InvalidInput(error.to_string()))?;
            if candidate.analytics.cluster_enabled && !first {
                continue;
            }
            let setup = AnalyticsSetup {
                exchange_code: raw.instrument.exchange.clone(),
                category: raw.instrument.category.clone(),
                symbol: raw.instrument.symbol.clone(),
                open_time_unix: setup.open_time_unix,
                close_time_unix: setup.close_time_unix,
                close_price: setup.close_price.clone(),
            };
            let before = capture.as_ref().map(|capture| {
                let cell = &result.evaluation.matrix[capture.cell_index];
                (cell.tp_count, cell.sl_count, cell.price_change_filtered_count)
            });
            if !accumulate_observed(
                spec,
                &candidate.analytics,
                data,
                &setup,
                replay.context,
                &mut result,
                control,
                daily.as_mut(),
            ) {
                return Ok(None);
            }
            if let Some(capture) = &mut capture {
                let before = before.expect("capture took counters");
                let after = &result.evaluation.matrix[capture.cell_index];
                if (spec.trend_mode.is_mean() && spec.forecast.is_none()) || after.price_change_filtered_count == before.2 {
                    capture.record(setup.close_time_unix, (before.0, before.1), after)?;
                }
            }
        }
    }
    for cell in &mut result.evaluation.matrix {
        if control.stopped() {
            return Ok(None);
        }
        cell.closed_count = cell.tp_count + cell.sl_count;
        if cell.closed_count > 0 {
            let mut sum = 0.0;
            for index in 0..cell.tp_count {
                if index % 256 == 0 && control.stopped() {
                    return Ok(None);
                }
                sum += cell.tp_pct;
            }
            for index in 0..cell.sl_count {
                if index % 256 == 0 && control.stopped() {
                    return Ok(None);
                }
                sum -= cell.sl_pct;
            }
            let avg = sum / cell.closed_count as f64;
            cell.avg_move_pct = Some(avg);
            cell.risk_expectancy = Some(avg / cell.sl_pct);
        }
    }
    if let Some(daily) = daily
        && !daily.apply(spec, &mut result.evaluation.matrix, control)? { return Ok(None); }
    let summary = &mut result.evaluation.summary;
    let selected = summary.selected_setup_count;
    summary.avg_setups_per_day = Some(frequency.average(selected));
    summary.score = frequency
        .accepts(selected)
        .then(|| score_matrix(&result.evaluation.matrix, spec.min_closed_per_cell))
        .flatten();
    let selected_daily = match capture {
        Some(capture) => {
            let Some(daily) = capture.finish(&result.evaluation.matrix, control)? else { return Ok(None); };
            Some(daily)
        }
        None => None,
    };
    Ok(Some(CapturedContinuousAnalysis { evaluation: result, selected_daily }))
}

/// Recompute analytics from the materialized raw service replay and ORIGINAL
/// axes/scope. This never reads/copies a StudyBest matrix.
pub fn analyze_continuous(
    spec: &StudyRequest,
    input: &StudyInput,
    replay: &ContinuousReplay,
    candidate: &StudyCandidate,
    limits: &StudyLimits,
    cancel: &CancelToken,
) -> Result<Option<ContinuousEvaluation>, StudyError> {
    Ok(analyze_continuous_with_daily(spec, input, replay, candidate, limits, cancel, None)?
        .map(|result| result.evaluation))
}

/// Same original analysis/classifier and validation, with optional compact
/// daily outcomes for the caller's FINAL selected pair. Not an ordinary-cache
/// rerun, not used by optimization trials, and not a new selection/gate.
pub fn analyze_continuous_with_daily(
    spec: &StudyRequest,
    input: &StudyInput,
    replay: &ContinuousReplay,
    candidate: &StudyCandidate,
    limits: &StudyLimits,
    cancel: &CancelToken,
    capture: Option<ContinuousCaptureCell>,
) -> Result<Option<CapturedContinuousAnalysis>, StudyError> {
    validate_continuous_request(spec, replay.context)?;
    if validate_input(spec, input, limits, cancel)?.is_none()
        || !validate_frozen_cutoff(input, replay.context, cancel)?
    {
        return Ok(None);
    }
    if replay.from_unix != spec.from_unix
        || replay.to_unix != spec.to_unix
        || replay.params != candidate.params
        || replay.provenance.snapshot_hash != input.snapshot_hash
        || replay.provenance.captured_at != input.captured_at
        || replay.provenance.seed != spec.seed
        || replay.provenance.optimizer_version != CONTINUOUS_OPTIMIZER_VERSION
        || replay.instruments.len() != spec.instruments.len()
        || replay
            .instruments
            .iter()
            .zip(&spec.instruments)
            .any(|(raw, expected)| &raw.instrument != expected)
    {
        return Err(StudyError::InvalidInput(
            "service replay differs from frozen scope/provenance".into(),
        ));
    }
    analyze_validated_continuous_with_daily(spec, input, replay, candidate, limits, cancel, capture)
}

/// Evaluate SAVED raw setups against separately identified available historical
/// candles. This is NOT an original frozen replay or a new detector run. Keep
/// original_replay.provenance immutable as the original raw-set anchor; input
/// carries the actual historical read's own fingerprint and capture time.
///
/// The caller must persist both provenances and compare outcomes/counters with
/// the saved original before publication. Equality of counts is not frozen-input
/// proof. This does not select winners, recommend or admit/revalidate profiles.
/// The inherited summary score is diagnostic, never a publication decision.
///
/// For bounded processing, pass one original instrument at a time in spec,
/// original_replay and input, with empty detector_bars. Drop that instrument's
/// candles after consuming the compact output. Preserve the original period,
/// cutoff, effective analytics, axes and known pre-period raw seed; never use
/// last_raw_close_unix or a live/end-of-period bootstrap as the initial seed.
pub fn analyze_historical_continuous_with_daily(
    spec: &StudyRequest,
    input: &StudyInput,
    original_replay: &ContinuousReplay,
    candidate: &StudyCandidate,
    limits: &StudyLimits,
    cancel: &CancelToken,
    capture: Option<ContinuousCaptureCell>,
) -> Result<Option<CapturedContinuousAnalysis>, StudyError> {
    validate_continuous_request(spec, original_replay.context)?;
    if validate_input(spec, input, limits, cancel)?.is_none()
        || !validate_frozen_cutoff(input, original_replay.context, cancel)?
    {
        return Ok(None);
    }
    if original_replay.from_unix != spec.from_unix
        || original_replay.to_unix != spec.to_unix
        || original_replay.params != candidate.params
        || original_replay.provenance.snapshot_hash.is_empty()
        || original_replay.provenance.snapshot_hash.len() > 256
        || original_replay.provenance.captured_at.is_empty()
        || original_replay.provenance.captured_at.len() > 64
        || original_replay.provenance.seed != spec.seed
        || original_replay.provenance.optimizer_version != CONTINUOUS_OPTIMIZER_VERSION
        || original_replay.instruments.len() != spec.instruments.len()
        || original_replay
            .instruments
            .iter()
            .zip(&spec.instruments)
            .any(|(raw, expected)| &raw.instrument != expected)
    {
        return Err(StudyError::InvalidInput(
            "historical raw replay differs from original scope/provenance".into(),
        ));
    }
    analyze_validated_continuous_with_daily(
        spec, input, original_replay, candidate, limits, cancel, capture,
    )
}

// Both public entries establish their DISTINCT input-provenance contract before
// sharing original raw-set validation, causal selection and outcome arithmetic.
fn analyze_validated_continuous_with_daily(
    spec: &StudyRequest,
    input: &StudyInput,
    replay: &ContinuousReplay,
    candidate: &StudyCandidate,
    limits: &StudyLimits,
    cancel: &CancelToken,
    capture: Option<ContinuousCaptureCell>,
) -> Result<Option<CapturedContinuousAnalysis>, StudyError> {
    let expected_params = parse_params(&spec.algorithm_id, &candidate.params)?;
    let mut total = 0usize;
    for raw in &replay.instruments {
        total = total.saturating_add(raw.setups.len());
        limit(
            "setups_per_candidate",
            total,
            limits.max_setups_per_candidate,
        )?;
        if raw
            .previous_raw_close_unix
            .is_some_and(|time| time >= spec.from_unix)
        {
            return Err(StudyError::InvalidInput(
                "invalid prehistory raw seed".into(),
            ));
        }
        let mut previous = raw.previous_raw_close_unix;
        for setup in &raw.setups {
            if cancel.is_cancelled() {
                return Ok(None);
            }
            if setup.algorithm_id != spec.algorithm_id
                || setup.algorithm_version != spec.algorithm_version
                || setup.close_time_unix < spec.from_unix
                || setup.close_time_unix >= spec.to_unix
                || setup.close_time_unix > replay.context.observation_cutoff
                || setup.open_time_unix.checked_add(60) != Some(setup.close_time_unix)
                || previous.is_some_and(|time| time >= setup.close_time_unix)
                || setup.params != expected_params
                || !setup
                    .close_price
                    .parse::<f64>()
                    .is_ok_and(|price| price.is_finite() && price > 0.0)
            {
                return Err(StudyError::InvalidInput(
                    "invalid materialized raw replay setup".into(),
                ));
            }
            previous = Some(setup.close_time_unix);
        }
    }
    let passes = AtomicUsize::new(0);
    let control = Control {
        cancel,
        deadline: Instant::now() + Duration::from_secs(spec.max_seconds),
        detector_passes: &passes,
    };
    analyze_inner_with_daily(spec, input, replay, candidate, limits, &control, capture)
}

pub fn evaluate_continuous(
    spec: &StudyRequest,
    input: &StudyInput,
    candidate: &StudyCandidate,
    context: ContinuousContext,
    limits: &StudyLimits,
    cancel: &CancelToken,
) -> Result<Option<ContinuousEvaluation>, StudyError> {
    let Some(replay) = materialize_continuous(spec, input, candidate, context, limits, cancel)?
    else {
        return Ok(None);
    };
    analyze_continuous(spec, input, &replay, candidate, limits, cancel)
}

pub fn search_continuous(
    spec: &StudyRequest,
    input: &StudyInput,
    context: ContinuousContext,
    limits: &StudyLimits,
    cancel: &CancelToken,
    progress: impl FnMut(StudyUpdate),
) -> Result<StudyOutcome, StudyError> {
    crate::search::run_study_mode(spec, input, limits, cancel, progress, Some(context))
}

/// None rejects the WHOLE original matrix if any cell is invalid/underpowered.
/// This ranking does not apply the separate mean-R or frequency admission gate.
/// Compatibility entry for legacy field-presence conventions. New explicit
/// recipes must use select_robust_cell_for_mode with their saved mode.
pub fn select_robust_cell(
    matrix: &[MatrixCell],
    tp_levels: &[f64],
    sl_levels: &[f64],
    min_closed_per_cell: usize,
) -> Result<Option<RobustCell>, StudyError> {
    select_robust_cell_inner(matrix,tp_levels,sl_levels,min_closed_per_cell,None)
}

/// New explicit recipes save pure F in forecastRiskExpectancy in BOTH forward
/// modes. Use the immutable mode, not presence of that field, to identify blend.
pub fn select_robust_cell_for_mode(
    matrix:&[MatrixCell],tp_levels:&[f64],sl_levels:&[f64],min_closed_per_cell:usize,mode:TrendMode,
) -> Result<Option<RobustCell>,StudyError> {
    select_robust_cell_inner(matrix,tp_levels,sl_levels,min_closed_per_cell,Some(mode))
}

fn select_robust_cell_inner(
    matrix:&[MatrixCell],tp_levels:&[f64],sl_levels:&[f64],min_closed_per_cell:usize,mode:Option<TrendMode>,
) -> Result<Option<RobustCell>,StudyError> {
    if tp_levels.is_empty() || tp_levels.len() > 64 || sl_levels.is_empty() || sl_levels.len() > 64
    {
        return Err(invalid("invalid robust selection axis"));
    }
    let mut tp = tp_levels.to_vec();
    let mut sl = sl_levels.to_vec();
    for axis in [&mut tp, &mut sl] {
        if axis.is_empty()
            || axis.len() > 64
            || axis
                .iter()
                .any(|level| !level.is_finite() || *level <= 0.0 || *level > 100.0)
        {
            return Err(invalid("invalid robust selection axis"));
        }
        axis.sort_by(f64::total_cmp);
        if axis.windows(2).any(|pair| pair[0] == pair[1]) {
            return Err(invalid("duplicate robust selection axis"));
        }
    }
    if matrix.len() != tp.len() * sl.len() {
        return Err(StudyError::InvalidInput(
            "matrix does not cover original axes".into(),
        ));
    }
    let mut ordered = vec![None; matrix.len()];
    for cell in matrix {
        let i = tp.iter().position(|level| *level == cell.tp_pct);
        let j = sl.iter().position(|level| *level == cell.sl_pct);
        let (Some(i), Some(j)) = (i, j) else {
            return Err(StudyError::InvalidInput(
                "matrix cell outside original axes".into(),
            ));
        };
        if ordered[i * sl.len() + j].replace(cell).is_some() {
            return Err(StudyError::InvalidInput("duplicate matrix cell".into()));
        }
    }
    let Some(metric_r) = score_matrix(matrix, min_closed_per_cell) else {
        return Ok(None);
    };
    let mean_r = score_period_mean(matrix, min_closed_per_cell).expect("scorable matrix has period means");
    if matrix.iter().any(|cell| cell.mean_risk_expectancy.is_some())
        && matrix.iter().any(|cell| cell.mean_risk_expectancy.is_none()) {
        return Err(StudyError::InvalidInput("mixed mean/forecast matrix".into()));
    }
    let forecast_matrix_r = matrix.iter().any(|cell| cell.mean_risk_expectancy.is_some()).then(|| {
        matrix.iter().map(|cell| cell.forecast_risk_expectancy.unwrap_or(cell.risk_expectancy.expect("scorable"))).sum::<f64>() / matrix.len() as f64
    });
    let is_blend=mode.map_or_else(||matrix.iter().any(|cell|cell.forecast_risk_expectancy.is_some()),|m|m==TrendMode::Blend);
    let mut best: Option<RobustCell> = None;
    for (i, &tp_pct) in tp.iter().enumerate() {
        for (j, &sl_pct) in sl.iter().enumerate() {
            let mut neighbors = Vec::with_capacity(9);
            for row in i.saturating_sub(1)..=(i + 1).min(tp.len() - 1) {
                for col in j.saturating_sub(1)..=(j + 1).min(sl.len() - 1) {
                    neighbors.push(
                        ordered[row * sl.len() + col]
                            .expect("complete matrix")
                            .risk_expectancy
                            .expect("validated R"),
                    );
                }
            }
            neighbors.sort_by(f64::total_cmp);
            let median = if neighbors.len() % 2 == 1 {
                neighbors[neighbors.len() / 2]
            } else {
                neighbors[neighbors.len() / 2 - 1] / 2.0 + neighbors[neighbors.len() / 2] / 2.0
            };
            let selected = ordered[i * sl.len() + j].expect("complete matrix");
            let metric_cell_r = selected
                .risk_expectancy
                .expect("validated R");
            let next = RobustCell {
                tp_pct,
                sl_pct,
                raw_r: selected.mean_risk_expectancy.unwrap_or(metric_cell_r),
                mean_r,
                robust_score: metric_cell_r.min(median),
                forecast_r: selected.mean_risk_expectancy.map(|_| selected.forecast_risk_expectancy.unwrap_or(metric_cell_r)),
                forecast_matrix_r,
                blend_r: is_blend.then_some(metric_cell_r),
                blend_matrix_r: is_blend.then_some(metric_r),
            };
            if best.is_none_or(|old| {
                next.robust_score > old.robust_score
                    || (next.robust_score == old.robust_score && next.blend_r.or(next.forecast_r).unwrap_or(next.raw_r) > old.blend_r.or(old.forecast_r).unwrap_or(old.raw_r))
            }) {
                best = Some(next);
            }
        }
    }
    Ok(best)
}

pub fn recommend_continuous(
    spec: &StudyRequest,
    candidate: &StudyCandidate,
    result: &ContinuousEvaluation,
    provenance: &Provenance,
    context: ContinuousContext,
    minimum_mean_r: f64,
) -> Result<Option<AdaptiveRecommendationSnapshot>, StudyError> {
    validate_continuous_request(spec, context)?;
    validate_analytics(&candidate.analytics)?;
    if !minimum_mean_r.is_finite() {
        return Err(invalid("mean R threshold must be finite"));
    }
    if !FrequencyGate::new(spec)?.accepts(result.evaluation.summary.selected_setup_count) {
        return Ok(None);
    }
    let Some(cell) = select_robust_cell_for_mode(
        &result.evaluation.matrix,
        &spec.tp_levels,
        &spec.sl_levels,
        spec.min_closed_per_cell,
        spec.trend_mode,
    )?
    else {
        return Ok(None);
    };
    let metric_r = cell.blend_matrix_r.or(cell.forecast_matrix_r).unwrap_or(cell.mean_r);
    if if spec.trend_mode.is_mean() { metric_r < minimum_mean_r } else { metric_r <= spec.min_forecast_expectancy } {
        return Ok(None);
    }
    Ok(Some(AdaptiveRecommendationSnapshot {
        forecast: spec.forecast,
        direction: match spec.direction {
            TradeDirection::Short => RecommendationDirection::Short,
            TradeDirection::Long => RecommendationDirection::Long,
        },
        tp_pct: cell.tp_pct,
        sl_pct: cell.sl_pct,
        raw_r: cell.raw_r,
        mean_r: cell.mean_r,
        robust_score: cell.robust_score,
        search_forecast: cell.forecast_r.zip(cell.forecast_matrix_r).map(|(cell_r, matrix_r)| setups_domain::SearchForecast {
            bend_days: crate::trend::effective_bend(spec), cell_r, matrix_r,
            blend: cell.blend_r.zip(cell.blend_matrix_r).map(|(cell_r, matrix_r)| setups_domain::SearchBlend {
                lambda: spec.trend_blend_lambda, cell_r, matrix_r,
            }),
        }),
        time_shift_minutes: candidate.analytics.time_shift_minutes,
        price_change_pct: candidate.analytics.price_change_pct,
        cluster_enabled: candidate.analytics.cluster_enabled,
        cluster_gap_minutes: candidate
            .analytics
            .cluster_enabled
            .then_some(candidate.analytics.cluster_gap_minutes),
        provenance: RecommendationProvenance {
            algorithm_id: spec.algorithm_id.clone(),
            algorithm_version: spec.algorithm_version.clone(),
            optimizer_version: provenance.optimizer_version.clone(),
            seed: provenance.seed,
            snapshot_hash: provenance.snapshot_hash.clone(),
            captured_at: provenance.captured_at.clone(),
            from_unix: spec.from_unix,
            to_unix: spec.to_unix,
            observation_cutoff: context.observation_cutoff,
        },
    }))
}
