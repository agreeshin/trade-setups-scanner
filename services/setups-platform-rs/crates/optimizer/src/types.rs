use serde::{Deserialize, Serialize};
use serde_json::Value;
use setups_analytics::{OhlcCandle, TradeDirection};
use setups_domain::DetectorBar;
use std::collections::BTreeMap;

pub type ParamMap = BTreeMap<String, Value>;
pub type SearchMap = BTreeMap<String, Vec<Value>>;

pub fn default_levels() -> Vec<f64> {
    (2..=10).map(|n| f64::from(n * 2)).collect()
}
fn default_horizon() -> i64 {
    10_080
}
fn default_min_closed() -> usize {
    1
}
fn default_trials() -> usize {
    256
}
fn default_seconds() -> u64 {
    3600
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StudyRequest {
    pub algorithm_id: String,
    pub algorithm_version: String,
    pub algorithm_params: ParamMap,
    #[serde(default)]
    pub algorithm_search: SearchMap,
    pub analytics_params: AnalyticsParams,
    #[serde(default)]
    pub analytics_search: SearchMap,
    pub instruments: Vec<StudyInstrument>,
    pub from_unix: i64,
    pub to_unix: i64,
    pub direction: TradeDirection,
    #[serde(default)]
    pub setup_maturity: f64,
    #[serde(default = "default_levels")]
    pub tp_levels: Vec<f64>,
    #[serde(default = "default_levels")]
    pub sl_levels: Vec<f64>,
    #[serde(default = "default_horizon")]
    pub horizon_minutes: i64,
    #[serde(default = "default_min_closed")]
    pub min_closed_per_cell: usize,
    #[serde(default = "default_trials")]
    pub max_trials: usize,
    #[serde(default = "default_seconds")]
    pub max_seconds: u64,
    pub seed: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AnalyticsParams {
    pub cluster_enabled: bool,
    pub cluster_gap_minutes: i64,
    pub time_shift_minutes: i64,
    pub price_change_pct: Option<f64>,
}

impl Default for AnalyticsParams {
    fn default() -> Self {
        Self {
            cluster_enabled: true,
            cluster_gap_minutes: 480,
            time_shift_minutes: 0,
            price_change_pct: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StudyInstrument {
    pub exchange: String,
    pub category: String,
    pub symbol: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParameterKind {
    Integer,
    Decimal,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ParameterDescriptor {
    pub kind: ParameterKind,
    pub min: Value,
    pub max: Value,
}

/// Half-open ranges on the candle OPEN-time axis. `fromUnix..toUnix` is
/// their union, suitable for one strict frozen OHLC/turnover fetch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InputBounds {
    pub from_unix: i64,
    pub to_unix: i64,
    pub detector_from_unix: i64,
    pub detector_to_unix: i64,
    pub candles_from_unix: i64,
    pub candles_to_unix: i64,
    pub max_warmup_minutes: i64,
    pub max_time_shift_minutes: i64,
}

#[derive(Debug, Clone)]
pub struct StudyInput {
    pub instruments: Vec<InstrumentInput>,
    pub snapshot_hash: String,
    pub captured_at: String,
}

#[derive(Debug, Clone)]
pub struct InstrumentInput {
    pub instrument: StudyInstrument,
    pub detector_bars: Vec<DetectorBar>,
    pub candles: Vec<OhlcCandle>,
}

/// One study owns all retained input/search data. CPU work lives in a dedicated
/// Rayon pool; no process-wide cache or unbounded candidate queue is created.
#[derive(Debug, Clone)]
pub struct StudyLimits {
    pub cpu_threads: usize,
    pub batch_size: usize,
    pub max_snapshot_bytes: usize,
    pub max_working_bytes: usize,
    pub max_setups_per_candidate: usize,
    pub max_domain_values: usize,
}

impl Default for StudyLimits {
    fn default() -> Self {
        Self {
            cpu_threads: 2,
            batch_size: 4,
            max_snapshot_bytes: 256 * 1024 * 1024,
            max_working_bytes: 128 * 1024 * 1024,
            max_setups_per_candidate: 100_000,
            max_domain_values: 4096,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StudyPhase {
    Acquiring,
    Initial,
    Sensitivity,
    Refinement,
    Cleanup,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StudyProgress {
    pub phase: StudyPhase,
    pub evaluated_candidates: usize,
    pub scorable_candidates: usize,
    pub max_trials: usize,
    pub elapsed_seconds: f64,
    pub best_score: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StudyEvent {
    pub at: String,
    pub kind: String,
    pub message: String,
}

#[derive(Debug, Clone)]
pub struct StudyUpdate {
    pub progress: StudyProgress,
    pub incumbent: Option<StudyBest>,
    pub event: Option<StudyEvent>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminationReason {
    TrialBudget,
    TimeBudget,
    SpaceExhausted,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StudyOutcome {
    pub progress: StudyProgress,
    pub best: Option<StudyBest>,
    pub reason: TerminationReason,
    pub baseline: Option<TrialSummary>,
    pub invalid_proposals: usize,
    pub detector_passes: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrialSummary {
    pub score: Option<f64>,
    pub setup_count: usize,
    pub selected_setup_count: usize,
    pub evaluated_minutes: u64,
    pub warmup_minutes: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EffectiveAnalytics {
    pub cluster_enabled: bool,
    pub cluster_gap_minutes: Option<i64>,
    pub time_shift_minutes: i64,
    pub price_change_pct: Option<f64>,
    pub setup_maturity: Option<f64>,
    pub horizon_minutes: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StudyBest {
    pub score: f64,
    pub partial: bool,
    pub params: ParamMap,
    pub analytics: EffectiveAnalytics,
    pub matrix: Vec<MatrixCell>,
    pub provenance: Provenance,
    pub conclusions: Vec<String>,
    pub support: TrialSummary,
    pub trial_index: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Provenance {
    pub optimizer_version: String,
    pub seed: u64,
    pub snapshot_hash: String,
    pub captured_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MatrixCell {
    pub tp_pct: f64,
    pub sl_pct: f64,
    pub risk_expectancy: Option<f64>,
    pub avg_move_pct: Option<f64>,
    pub closed_count: usize,
    pub tp_count: usize,
    pub sl_count: usize,
    pub unclosed_count: usize,
    pub no_data_count: usize,
    pub price_change_filtered_count: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StudyCandidate {
    pub params: ParamMap,
    pub analytics: AnalyticsParams,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CandidateEvaluation {
    pub summary: TrialSummary,
    pub matrix: Vec<MatrixCell>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StudyError {
    InvalidRequest(String),
    InvalidInput(String),
    ResourceLimit {
        resource: String,
        limit: usize,
        requested: usize,
    },
    Detector(String),
    Execution(String),
}

impl std::fmt::Display for StudyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidRequest(message) => write!(f, "invalid optimizer request: {message}"),
            Self::InvalidInput(message) => write!(f, "invalid optimizer input: {message}"),
            Self::ResourceLimit {
                resource,
                limit,
                requested,
            } => write!(
                f,
                "optimizer {resource} limit exceeded: requested {requested}, limit {limit}"
            ),
            Self::Detector(message) => write!(f, "optimizer detector error: {message}"),
            Self::Execution(message) => write!(f, "optimizer execution error: {message}"),
        }
    }
}
impl std::error::Error for StudyError {}
