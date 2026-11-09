pub mod decimal;

use serde::{Deserialize, Serialize};

pub const SPIKE_ALGORITHM_ID: &str = "price_volume_spike";
pub const SPIKE_ALGORITHM_VERSION: &str = "1.1.1";

pub const MINUTE_SECONDS: i64 = 60;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SpikeParams {
    pub price_growth_pct: String,
    pub window_minutes: u32,
    pub baseline_long_days: u32,
    pub baseline_short_days: u32,
    pub volume_multiplier: String,
    pub cooldown_minutes: u32,
    pub max_gap_percent: String,
    pub max_consecutive_gap_minutes: u32,
    pub required_recent_minutes: u32,
}

impl Default for SpikeParams {
    fn default() -> Self {
        SpikeParams {
            price_growth_pct: "12".to_string(),
            window_minutes: 5,
            baseline_long_days: 30,
            baseline_short_days: 7,
            volume_multiplier: "3".to_string(),
            cooldown_minutes: 5,
            max_gap_percent: "1".to_string(),
            max_consecutive_gap_minutes: 5,
            required_recent_minutes: 10,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DetectorBar {
    pub open_time_unix: i64,
    pub close: String,
    pub quote_turnover: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetupDetails {
    pub growth_pct: Option<String>,
    pub volume_long_ratio: Option<String>,
    pub volume_short_ratio: Option<String>,
    pub over_long: bool,
    pub over_short: bool,
    pub missing_minutes: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SpikeSetup {
    pub algorithm_id: String,
    pub algorithm_version: String,
    pub open_time_unix: i64,
    pub close_time_unix: i64,
    pub close_price: String,
    pub params: SpikeParams,
    pub details: Option<SetupDetails>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PushStatus {
    Warmup,
    Evaluated,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    InsufficientWarmup,
    RecentGap,
    PriceReferenceMissing,
    GapToleranceExceeded,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PushResult {
    pub status: PushStatus,
    pub continuous_minutes: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub setup: Option<SpikeSetup>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skip_reason: Option<SkipReason>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub missing_minutes: Option<u32>,
}
