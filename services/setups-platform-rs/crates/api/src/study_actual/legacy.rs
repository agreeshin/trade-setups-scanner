//! Explicit owned legacy calculation, never a GET side effect or publication.
//! The provider uses the normal MD boundary and owns pagination/read guards.
//! No DB, detector, search, new replay, profile enrichment, or native seal here.
use chrono::{DateTime, Utc};
use serde::Serialize;
use setups_analytics::{CausalCluster, OhlcCandle};
use setups_domain::expectancy::{EXPECTANCY_MAX_POINTS, UtcDailyInput};
use setups_mdclient::CandleDto;
use setups_optimizer::{self as core, ContinuousSelectedMinute, ContinuousSelectedMinutes};
use sha2::{Digest, Sha256};
use std::{collections::{BTreeMap, BTreeSet}, future::Future, io::Write, time::{Duration, Instant}};

#[derive(Debug)]
pub enum LegacyError {
    Invalid(&'static str),
    Kernel(core::StudyError),
    Interrupted,
}
impl std::fmt::Display for LegacyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self { Self::Invalid(code) => f.write_str(code), Self::Kernel(error) => error.fmt(f), Self::Interrupted => f.write_str("legacy_interrupted") }
    }
}
impl std::error::Error for LegacyError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> { match self { Self::Kernel(error) => Some(error), _ => None } }
}
impl From<core::StudyError> for LegacyError { fn from(error: core::StudyError) -> Self { Self::Kernel(error) } }
pub type Result<T> = std::result::Result<T, LegacyError>;
fn invalid(code: &'static str) -> LegacyError { LegacyError::Invalid(code) }
fn valid_hash(value: &str) -> bool { value.len() == 64 && value.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) }

/// Loaded from immutable retained linkage by the caller, never current defaults.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LegacySource {
    pub owner_id: String,
    pub study_id: i64,
    pub generation_id: i64,
    pub original_artifact_id: i64,
    pub original_artifact_key: String,
    pub original_snapshot_hash: String,
    pub spec: core::StudyRequest,
    pub candidate: core::StudyCandidate,
    pub original_effective_analytics: core::EffectiveAnalytics,
    pub replay: core::ContinuousReplay,
    pub pair: core::ContinuousCaptureCell,
    pub expected_daily: Vec<UtcDailyInput>,
}

#[derive(Debug, Clone)]
pub struct LegacyLimits {
    pub kernel: core::StudyLimits,
    pub max_source_bytes: usize,
    pub max_instrument_candles: usize,
    pub max_total_candles: u64,
    pub max_input_bytes: usize,
    pub max_output_bytes: usize,
    pub max_seconds: u64,
}
impl Default for LegacyLimits {
    fn default() -> Self {
        Self { kernel: core::StudyLimits::default(), max_source_bytes: 64 * 1024 * 1024,
            max_instrument_candles: 300_000, max_total_candles: 5_000_000,
            max_input_bytes: 32 * 1024 * 1024, max_output_bytes: 32 * 1024 * 1024, max_seconds: 600 }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TradeWindow { pub from_unix: i64, pub to_unix: i64 }
#[derive(Debug, Clone)]
pub struct ReadRequest {
    pub instrument: core::StudyInstrument,
    pub windows: Vec<TradeWindow>,
    pub windows_hash: String,
    pub max_candles: usize,
    pub max_bytes: usize,
}
/// Provider must enforce request caps while reading, not after an unbounded read.
/// Missing minutes remain missing; no synthetic candles or gap repair.
#[derive(Debug, Clone)]
pub struct CandleInput {
    pub instrument_id: i64,
    pub read_identity: String,
    pub candles: Vec<CandleDto>,
}

/// A TRUSTED read-only verifier's attestation, not user/HTTP JSON. No Deserialize
/// intentionally: hashing/field validation does not authenticate the verifier.
/// The verifier must audit import/restore/deletion paths as well as DO NOTHING.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VerifiedRowsProof {
    pub version: u32,
    pub source_hash: String,
    pub original_capture: DateTime<Utc>,
    pub evaluator: String,
    pub writer_audit_hash: String,
    pub first_canonical_add_only_verified: bool,
    pub canonical_ohlc_preserved_verified: bool,
    pub historical_close_kernel_compatible_verified: bool,
    pub instruments: Vec<InstrumentProof>,
}
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstrumentProof {
    pub instrument: core::StudyInstrument,
    pub instrument_id: i64,
    pub windows_hash: String,
    pub candle_hash: String,
    pub read_identity: String,
    pub candle_count: u64,
    /// Diagnostic only. DEFAULT now() is transaction-start time, not MVCC proof;
    /// late gap fills are permitted under the monotonic closed-set argument.
    pub latest_ingested_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LegacyFidelity { ReconstructedUnproven, VerifiedOriginalReconstruction }
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstrumentFingerprint {
    pub instrument: core::StudyInstrument,
    pub instrument_id: i64,
    pub windows_hash: String,
    pub candle_hash: String,
    pub read_identity: String,
    pub candle_count: u64,
    pub missing_minutes: u64,
}
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DayConservation {
    pub day_start_unix: i64,
    pub expected: Option<UtcDailyInput>,
    pub actual: Option<UtcDailyInput>,
    pub matched: bool,
}
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LegacyCalculation {
    pub fidelity: LegacyFidelity,
    pub source_hash: String,
    pub input_hash: String,
    pub original_snapshot_hash: String,
    pub proof_hash: Option<String>,
    pub proof: Option<VerifiedRowsProof>,
    pub selected_minutes: ContinuousSelectedMinutes,
    pub daily: Vec<UtcDailyInput>,
    pub conservation: Vec<DayConservation>,
    pub conservation_matched: bool,
    pub inputs: Vec<InstrumentFingerprint>,
}

// Streaming hash prevents another complete copy of retained replay/MD payloads.
struct HashWriter { hash: Sha256, bytes: usize, cap: usize }
impl Write for HashWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > self.cap.saturating_sub(self.bytes) { return Err(std::io::Error::other("legacy_byte_budget")); }
        self.hash.update(bytes); self.bytes += bytes.len(); Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
}
pub(crate) fn fingerprint<T: Serialize + ?Sized>(domain: &[u8], value: &T, cap: usize) -> Result<(String, usize)> {
    let mut writer = HashWriter { hash: Sha256::new(), bytes: 0, cap };
    writer.hash.update(domain);
    serde_json::to_writer(&mut writer, value).map_err(|_| invalid("legacy_serialization_or_byte_budget"))?;
    Ok((format!("{:x}", writer.hash.finalize()), writer.bytes))
}
pub fn source_hash(source: &LegacySource, cap: usize) -> Result<String> {
    Ok(fingerprint(b"legacy-original-source-v1\0", source, cap)?.0)
}
pub fn candle_hash(candles: &[CandleDto], cap: usize) -> Result<String> {
    Ok(fingerprint(b"legacy-trade-candles-v1\0", candles, cap)?.0)
}

/// All original selected-cell trade windows, not detector/acquisition warmup.
/// Keeps pre-period raw seed and causal chaining BEFORE entry/gate evaluation.
pub fn trade_windows(source: &LegacySource, raw: &core::ContinuousInstrumentReplay) -> Result<Vec<TradeWindow>> {
    let a = &source.candidate.analytics;
    let mut cluster = CausalCluster::new(a.cluster_gap_minutes.checked_mul(60).ok_or_else(|| invalid("legacy_time_overflow"))?, raw.previous_raw_close_unix)
        .map_err(|_| invalid("legacy_raw_seed_or_order"))?;
    let bounds = core::required_continuous_input_bounds(&source.spec, source.replay.context)?;
    let mut windows: Vec<TradeWindow> = Vec::new();
    for setup in &raw.setups {
        let first = cluster.observe_raw(setup.close_time_unix).map_err(|_| invalid("legacy_raw_order"))?;
        if a.cluster_enabled && !first { continue; }
        let from = setup.open_time_unix.checked_add(a.time_shift_minutes.checked_mul(60).ok_or_else(|| invalid("legacy_time_overflow"))?)
            .ok_or_else(|| invalid("legacy_time_overflow"))?;
        let start = from.checked_add(60).ok_or_else(|| invalid("legacy_time_overflow"))?;
        if start > source.replay.context.observation_cutoff { continue; }
        let to = start.checked_add(source.spec.horizon_minutes.checked_mul(60).ok_or_else(|| invalid("legacy_time_overflow"))?)
            .ok_or_else(|| invalid("legacy_time_overflow"))?.min(source.replay.context.observation_cutoff);
        if from < bounds.candles_from_unix || to > bounds.candles_to_unix || from >= to { return Err(invalid("legacy_window_outside_original_bounds")); }
        if let Some(last) = windows.last_mut() && from <= last.to_unix { last.to_unix = last.to_unix.max(to); continue; }
        windows.push(TradeWindow { from_unix: from, to_unix: to });
    }
    Ok(windows)
}

fn check(cancel: &core::CancelToken, deadline: Instant) -> Result<()> {
    if cancel.is_cancelled() || Instant::now() >= deadline { Err(LegacyError::Interrupted) } else { Ok(()) }
}
fn sum_r(point: &UtcDailyInput) -> Option<f64> { point.mean_r.map(|mean| mean * (point.tp_count + point.sl_count) as f64) }
fn matches_day(expected: &UtcDailyInput, actual: &UtcDailyInput, pair: core::ContinuousCaptureCell) -> bool {
    let absolute = expected.tp_count as f64 * (pair.tp_pct / pair.sl_pct) + expected.sl_count as f64;
    expected.tp_count == actual.tp_count && expected.sl_count == actual.sl_count
        && match (sum_r(expected), sum_r(actual)) { (None, None) => true, (Some(a), Some(b)) => a.is_finite() && b.is_finite() && (a - b).abs() <= 1e-10 * absolute.max(1.0), _ => false }
}

/// Reconstruct using existing CLOSE-entry kernel, one bounded instrument at a
/// time. The trusted caller owns scheduling, stable MD read guards and explicit
/// invocation. No publication occurs, and matching UTC totals alone never grants
/// verified fidelity. Invalid supplied proof fails closed instead of downgrading.
pub async fn calculate<P, F>(source: &LegacySource, limits: &LegacyLimits, cancel: &core::CancelToken,
    proof: Option<&VerifiedRowsProof>, mut provider: P) -> Result<LegacyCalculation>
where P: FnMut(ReadRequest) -> F, F: Future<Output = Result<CandleInput>> {
    if limits.max_seconds == 0 || limits.max_seconds > 1800 { return Err(invalid("legacy_time_budget")); }
    let deadline = Instant::now() + Duration::from_secs(limits.max_seconds);
    check(cancel, deadline)?;
    let capture = DateTime::parse_from_rfc3339(&source.replay.provenance.captured_at).map_err(|_| invalid("legacy_original_capture_missing"))?.with_timezone(&Utc);
    core::validate_continuous_request(&source.spec, source.replay.context)?;
    let effective = &source.original_effective_analytics;
    if effective.horizon_minutes != source.spec.horizon_minutes || effective.setup_maturity.unwrap_or(0.0) != 0.0
        || effective.cluster_enabled != source.candidate.analytics.cluster_enabled
        || effective.cluster_enabled && effective.cluster_gap_minutes != Some(source.candidate.analytics.cluster_gap_minutes)
        || effective.time_shift_minutes != source.candidate.analytics.time_shift_minutes
        || effective.price_change_pct != source.candidate.analytics.price_change_pct
    { return Err(invalid("legacy_original_effective_conditions_mismatch")); }
    if source.owner_id.is_empty() || source.owner_id.len() > 120 || source.study_id <= 0 || source.generation_id <= 0 || source.original_artifact_id <= 0
        || !valid_hash(&source.original_artifact_key) || source.original_snapshot_hash != source.replay.provenance.snapshot_hash
        || !valid_hash(&source.original_snapshot_hash) || source.replay.params != source.candidate.params
        || source.replay.from_unix != source.spec.from_unix || source.replay.to_unix != source.spec.to_unix
        || source.replay.provenance.seed != source.spec.seed || source.replay.provenance.optimizer_version != core::CONTINUOUS_OPTIMIZER_VERSION
        || source.replay.instruments.len() != source.spec.instruments.len()
        || source.replay.instruments.iter().zip(&source.spec.instruments).any(|(raw, expected)| &raw.instrument != expected)
        || source.expected_daily.len() > EXPECTANCY_MAX_POINTS
        || !source.spec.tp_levels.contains(&source.pair.tp_pct) || !source.spec.sl_levels.contains(&source.pair.sl_pct)
    { return Err(invalid("legacy_original_source_mismatch")); }
    let raw_count = source.replay.instruments.iter().try_fold(0usize, |n, raw| n.checked_add(raw.setups.len())).ok_or_else(|| invalid("legacy_setup_overflow"))?;
    if raw_count > limits.kernel.max_setups_per_candidate { return Err(invalid("legacy_setup_budget")); }
    let (source_hash, source_bytes) = fingerprint(b"legacy-original-source-v1\0", source, limits.max_source_bytes)?;
    // Conservative BTree node + compact output allowance, bounded by original
    // setup count; one input and one kernel result coexist, never all OHLC sets.
    let output_reserve = raw_count.saturating_mul(768).saturating_add(source.expected_daily.len().saturating_mul(512)).saturating_add(4096);
    let working = source_bytes.saturating_mul(2).saturating_add(raw_count.saturating_mul(192))
        .saturating_add(limits.max_input_bytes.saturating_mul(2)).saturating_add(output_reserve);
    if output_reserve > limits.max_output_bytes || working > limits.kernel.max_working_bytes { return Err(invalid("legacy_working_or_output_budget")); }
    let mut proof_by_instrument = BTreeMap::new();
    if let Some(proof) = proof {
        if proof.version != 1 || proof.source_hash != source_hash || proof.original_capture != capture
            || proof.evaluator != core::CONTINUOUS_SELECTED_MINUTES_EVALUATOR || !valid_hash(&proof.writer_audit_hash)
            || !proof.first_canonical_add_only_verified || !proof.canonical_ohlc_preserved_verified
            || !proof.historical_close_kernel_compatible_verified
            || proof.instruments.len() > source.spec.instruments.len()
        { return Err(invalid("legacy_proof_header_invalid")); }
        for item in &proof.instruments {
            if proof_by_instrument.insert(item.instrument.clone(), item).is_some() { return Err(invalid("legacy_proof_duplicate_instrument")); }
        }
    }
    let mut minutes: BTreeMap<i64, ContinuousSelectedMinute> = BTreeMap::new();
    let mut days: BTreeMap<i64, (u64, u64, u64, f64)> = BTreeMap::new();
    let mut inputs = Vec::with_capacity(source.spec.instruments.len());
    let mut ids = BTreeSet::new();
    let mut total_candles = 0u64;
    let mut total_window_minutes = 0u64;
    for raw in &source.replay.instruments {
        check(cancel, deadline)?;
        let windows = trade_windows(source, raw)?;
        let windows_hash = fingerprint(b"legacy-trade-windows-v1\0", &(raw.instrument.clone(), &windows), limits.max_source_bytes)?.0;
        let expected_minutes: u64 = windows.iter().map(|w| ((w.to_unix - w.from_unix) / 60) as u64).sum();
        if expected_minutes > limits.max_instrument_candles as u64 { return Err(invalid("legacy_instrument_window_budget")); }
        total_window_minutes = total_window_minutes.checked_add(expected_minutes).ok_or_else(|| invalid("legacy_window_overflow"))?;
        if total_window_minutes > limits.max_total_candles { return Err(invalid("legacy_total_window_budget")); }
        let input = if windows.is_empty() { CandleInput { instrument_id: 0, read_identity: "no_trade_windows".into(), candles: Vec::new() } }
            else {
                tokio::time::timeout(deadline.saturating_duration_since(Instant::now()), provider(ReadRequest {
                    instrument: raw.instrument.clone(), windows: windows.clone(), windows_hash: windows_hash.clone(),
                    max_candles: limits.max_instrument_candles, max_bytes: limits.max_input_bytes,
                })).await.map_err(|_| LegacyError::Interrupted)??
            };
        check(cancel, deadline)?;
        if input.candles.len() > limits.max_instrument_candles || input.read_identity.is_empty() || input.read_identity.len() > 256
            || (!windows.is_empty() && (input.instrument_id <= 0 || !ids.insert(input.instrument_id))) { return Err(invalid("legacy_input_id_or_budget")); }
        let (candle_hash, _) = fingerprint(b"legacy-trade-candles-v1\0", &input.candles, limits.max_input_bytes)?;
        total_candles = total_candles.checked_add(input.candles.len() as u64).ok_or_else(|| invalid("legacy_candle_overflow"))?;
        if total_candles > limits.max_total_candles { return Err(invalid("legacy_total_candle_budget")); }
        let mut candles = Vec::with_capacity(input.candles.len());
        let mut previous = None;
        let mut window_index = 0;
        for (index, candle) in input.candles.iter().enumerate() {
            if index % 128 == 0 { check(cancel, deadline)?; }
            let at = candle.open_time_unix;
            while window_index < windows.len() && at >= windows[window_index].to_unix { window_index += 1; }
            if at % 60 != 0 || previous.is_some_and(|last| at <= last) || !windows.get(window_index).is_some_and(|w| at >= w.from_unix && at < w.to_unix)
                || [&candle.open, &candle.high, &candle.low, &candle.close, &candle.volume, &candle.quote_turnover].iter().any(|s| s.len() > 128)
            { return Err(invalid("legacy_candle_order_scope_or_size")); }
            previous = Some(at);
            let number = |text: &str| text.parse::<f64>().ok().filter(|n| n.is_finite() && *n > 0.0).ok_or_else(|| invalid("legacy_ohlc_invalid"));
            candles.push(OhlcCandle { open_time_unix: at, open: number(&candle.open)?, high: number(&candle.high)?, low: number(&candle.low)?, close: number(&candle.close)? });
        }
        let missing = expected_minutes.checked_sub(candles.len() as u64).ok_or_else(|| invalid("legacy_candle_duplicates"))?;
        if let Some(_) = proof && !windows.is_empty() {
            let item = proof_by_instrument.remove(&raw.instrument).ok_or_else(|| invalid("legacy_proof_instrument_missing"))?;
            if item.instrument_id != input.instrument_id || item.windows_hash != windows_hash || item.candle_hash != candle_hash
                || item.read_identity != input.read_identity || item.candle_count != input.candles.len() as u64
            { return Err(invalid("legacy_proof_rows_mismatch")); }
        }
        inputs.push(InstrumentFingerprint { instrument: raw.instrument.clone(), instrument_id: input.instrument_id,
            windows_hash, candle_hash: candle_hash.clone(), read_identity: input.read_identity, candle_count: candles.len() as u64, missing_minutes: missing });
        drop(input.candles);
        let mut spec = source.spec.clone(); spec.instruments = vec![raw.instrument.clone()];
        spec.max_seconds = deadline.saturating_duration_since(Instant::now()).as_secs().max(1);
        let replay = core::ContinuousReplay { from_unix: source.replay.from_unix, to_unix: source.replay.to_unix,
            context: source.replay.context, params: source.replay.params.clone(), provenance: source.replay.provenance.clone(), instruments: vec![raw.clone()] };
        let data = core::StudyInput { instruments: vec![core::InstrumentInput { instrument: raw.instrument.clone(), detector_bars: Vec::new(), candles }],
            snapshot_hash: candle_hash, captured_at: Utc::now().to_rfc3339() };
        let result = core::analyze_historical_continuous_with_daily(&spec, &data, &replay, &source.candidate, &limits.kernel, cancel, Some(source.pair))?
            .ok_or(LegacyError::Interrupted)?;
        drop(data);
        drop(replay);
        for point in result.selected_daily.ok_or_else(|| invalid("legacy_daily_capture_missing"))?.points {
            let day = days.entry(point.day_start_unix).or_default();
            day.0 += point.row_count; day.1 += point.tp_count; day.2 += point.sl_count; day.3 += sum_r(&point).unwrap_or(0.0);
        }
        for point in result.selected_minutes.ok_or_else(|| invalid("legacy_minute_capture_missing"))?.points {
            let value = minutes.entry(point.signal_close_unix).or_insert(ContinuousSelectedMinute { signal_close_unix: point.signal_close_unix, sum_r: 0.0, closed_count: 0, tp_count: 0, sl_count: 0 });
            value.sum_r += point.sum_r; value.closed_count += point.closed_count; value.tp_count += point.tp_count; value.sl_count += point.sl_count;
        }
        if minutes.len() > limits.kernel.max_setups_per_candidate || days.len() > EXPECTANCY_MAX_POINTS { return Err(invalid("legacy_result_budget")); }
    }
    check(cancel, deadline)?;
    if !proof_by_instrument.is_empty() { return Err(invalid("legacy_proof_unconsumed_scope")); }
    let daily: Vec<_> = days.iter().map(|(&at, &(rows, tp, sl, r))| UtcDailyInput { day_start_unix: at, row_count: rows, tp_count: tp, sl_count: sl, mean_r: (tp + sl > 0).then(|| r / (tp + sl) as f64) }).collect();
    let mut expected = BTreeMap::new();
    for point in &source.expected_daily {
        let closed = point.tp_count.checked_add(point.sl_count).ok_or_else(|| invalid("legacy_expected_count_overflow"))?;
        if point.day_start_unix % 86400 != 0 || point.row_count < closed
            || point.mean_r.is_some_and(|n| !n.is_finite()) || (closed > 0) != point.mean_r.is_some()
            || expected.insert(point.day_start_unix, point.clone()).is_some() { return Err(invalid("legacy_expected_days_invalid")); }
    }
    if days.values().any(|day| !day.3.is_finite()) || minutes.values().any(|point| !point.sum_r.is_finite() || point.closed_count != point.tp_count + point.sl_count) {
        return Err(invalid("legacy_nonfinite_or_invalid_aggregate"));
    }
    let mut conservation = Vec::with_capacity(expected.len().max(days.len()));
    for at in expected.keys().chain(days.keys()).copied().collect::<BTreeSet<_>>() {
        let e = expected.get(&at).cloned(); let a = daily.iter().find(|p| p.day_start_unix == at).cloned();
        let matched = match (&e, &a) {
            (Some(e), Some(a)) => matches_day(e, a, source.pair),
            (Some(e), None) => e.tp_count == 0 && e.sl_count == 0,
            (None, Some(a)) => a.tp_count == 0 && a.sl_count == 0,
            (None, None) => true,
        };
        conservation.push(DayConservation { day_start_unix: at, expected: e, actual: a, matched });
    }
    let matched = conservation.iter().all(|day| day.matched);
    if proof.is_some() && !matched { return Err(invalid("legacy_verified_conservation_mismatch")); }
    let input_hash = fingerprint(b"legacy-actual-input-v1\0", &(&source_hash, &inputs), limits.max_output_bytes)?.0;
    let proof_hash = proof.map(|p| fingerprint(b"legacy-verified-proof-v1\0", p, limits.max_source_bytes).map(|x| x.0)).transpose()?;
    // With preserved old OHLC and the same first-gap-stopping CLOSE kernel,
    // append-only gap fills cannot alter an already closed outcome, only add
    // closures. Equality of CLOSED counts proves the same closed set; TP/SL and
    // per-day R additionally guard scope/implementation. Warmup hash and ingest
    // timestamps are neither substituted nor required for this bounded proof.
    let result = LegacyCalculation { fidelity: if proof.is_some() { LegacyFidelity::VerifiedOriginalReconstruction } else { LegacyFidelity::ReconstructedUnproven },
        source_hash, input_hash, original_snapshot_hash: source.original_snapshot_hash.clone(), proof_hash, proof: proof.cloned(),
        selected_minutes: ContinuousSelectedMinutes { version: core::CONTINUOUS_SELECTED_MINUTES_VERSION, evaluator: core::CONTINUOUS_SELECTED_MINUTES_EVALUATOR.into(),
            tp_pct: source.pair.tp_pct, sl_pct: source.pair.sl_pct, points: minutes.into_values().collect() }, daily, conservation, conservation_matched: matched, inputs };
    fingerprint(b"legacy-output-budget-v1\0", &result, limits.max_output_bytes)?;
    Ok(result)
}

/// Trusted SAME-invocation promotion, not a deserialized/imported result API.
/// Runtime supplies its audited facts after its one guarded read; this validates
/// the completed calculation's bindings without fetching/evaluating OHLC twice.
pub fn promote_same_read(source: &LegacySource, mut result: LegacyCalculation,
    proof: VerifiedRowsProof, limits: &LegacyLimits) -> Result<LegacyCalculation> {
    let source_key = source_hash(source, limits.max_source_bytes)?;
    let captured = DateTime::parse_from_rfc3339(&source.replay.provenance.captured_at)
        .map_err(|_| invalid("legacy_original_capture_missing"))?.with_timezone(&Utc);
    if !matches!(result.fidelity, LegacyFidelity::ReconstructedUnproven) || result.proof.is_some() || result.proof_hash.is_some()
        || result.source_hash != source_key || result.original_snapshot_hash != source.original_snapshot_hash
        || result.input_hash != fingerprint(b"legacy-actual-input-v1\0", &(&source_key, &result.inputs), limits.max_output_bytes)?.0
        || proof.version != 1 || proof.source_hash != source_key || proof.original_capture != captured
        || proof.evaluator != core::CONTINUOUS_SELECTED_MINUTES_EVALUATOR || !valid_hash(&proof.writer_audit_hash)
        || !proof.first_canonical_add_only_verified || !proof.canonical_ohlc_preserved_verified || !proof.historical_close_kernel_compatible_verified
        || result.selected_minutes.version != core::CONTINUOUS_SELECTED_MINUTES_VERSION
        || result.selected_minutes.evaluator != core::CONTINUOUS_SELECTED_MINUTES_EVALUATOR
        || (result.selected_minutes.tp_pct, result.selected_minutes.sl_pct) != (source.pair.tp_pct, source.pair.sl_pct)
        || result.inputs.len() != source.replay.instruments.len()
        || proof.instruments.len() > source.spec.instruments.len() || result.selected_minutes.points.len() > limits.kernel.max_setups_per_candidate
    { return Err(invalid("legacy_same_read_binding_invalid")); }
    let mut proofs = BTreeMap::new();
    for item in &proof.instruments {
        if proofs.insert(&item.instrument, item).is_some() { return Err(invalid("legacy_proof_duplicate_instrument")); }
    }
    for (input, raw) in result.inputs.iter().zip(&source.replay.instruments) {
        let windows = trade_windows(source, raw)?;
        let hash = fingerprint(b"legacy-trade-windows-v1\0", &(raw.instrument.clone(), &windows), limits.max_source_bytes)?.0;
        if input.instrument != raw.instrument || input.windows_hash != hash || !valid_hash(&input.candle_hash) { return Err(invalid("legacy_same_read_scope_invalid")); }
        if windows.is_empty() { if input.candle_count != 0 || input.instrument_id != 0 { return Err(invalid("legacy_same_read_empty_scope_invalid")); } continue; }
        let p = proofs.remove(&raw.instrument).ok_or_else(|| invalid("legacy_proof_instrument_missing"))?;
        if p.instrument_id != input.instrument_id || p.windows_hash != input.windows_hash || p.candle_hash != input.candle_hash
            || p.read_identity != input.read_identity || p.candle_count != input.candle_count { return Err(invalid("legacy_proof_rows_mismatch")); }
    }
    if !proofs.is_empty() { return Err(invalid("legacy_proof_unconsumed_scope")); }
    // Re-establish conservation from the bounded ledger, not a caller-set flag.
    let mut ledger = BTreeMap::<i64, UtcDailyInput>::new();
    let mut previous = None;
    for p in &result.selected_minutes.points {
        if p.signal_close_unix % 60 != 0 || p.signal_close_unix < source.spec.from_unix || p.signal_close_unix >= source.spec.to_unix
            || p.signal_close_unix > source.replay.context.observation_cutoff || previous.is_some_and(|at| at >= p.signal_close_unix)
            || p.closed_count == 0 || p.tp_count.checked_add(p.sl_count) != Some(p.closed_count) || !p.sum_r.is_finite()
        { return Err(invalid("legacy_same_read_ledger_invalid")); }
        previous = Some(p.signal_close_unix);
        let at = p.signal_close_unix.div_euclid(86400) * 86400;
        let day = ledger.entry(at).or_insert(UtcDailyInput { day_start_unix: at, row_count: 0, tp_count: 0, sl_count: 0, mean_r: None });
        let sum = sum_r(day).unwrap_or(0.0) + p.sum_r;
        day.row_count += p.closed_count; day.tp_count += p.tp_count; day.sl_count += p.sl_count;
        day.mean_r = Some(sum / (day.tp_count + day.sl_count) as f64);
    }
    for day in &source.expected_daily {
        let empty = UtcDailyInput {day_start_unix: day.day_start_unix, row_count: 0, tp_count: 0, sl_count: 0, mean_r: None};
        if !matches_day(day, &ledger.remove(&day.day_start_unix).unwrap_or(empty), source.pair) { return Err(invalid("legacy_verified_conservation_mismatch")); }
    }
    if !ledger.is_empty() || !result.conservation_matched || result.conservation.iter().any(|d| !d.matched) { return Err(invalid("legacy_verified_conservation_mismatch")); }
    result.proof_hash = Some(fingerprint(b"legacy-verified-proof-v1\0", &proof, limits.max_source_bytes)?.0);
    result.proof = Some(proof);
    result.fidelity = LegacyFidelity::VerifiedOriginalReconstruction;
    fingerprint(b"legacy-output-budget-v1\0", &result, limits.max_output_bytes)?;
    Ok(result)
}
