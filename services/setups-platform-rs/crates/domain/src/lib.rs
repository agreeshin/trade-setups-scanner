pub mod decimal;
pub mod parameter_metadata;
pub use parameter_metadata::algorithm_fields;

use serde::{Deserialize, Serialize};

pub const SPIKE_ALGORITHM_ID: &str = "price_volume_spike";
pub const SPIKE_ALGORITHM_VERSION: &str = "1.1.1";

pub const SPIKE_DROP_ALGORITHM_ID: &str = "price_drop_volume_spike";
pub const SPIKE_DROP_ALGORITHM_VERSION: &str = "1.1.1";

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
pub struct SpikeDropParams {
    pub price_drop_pct: String,
    pub window_minutes: u32,
    pub baseline_long_days: u32,
    pub baseline_short_days: u32,
    pub volume_multiplier: String,
    pub cooldown_minutes: u32,
    pub max_gap_percent: String,
    pub max_consecutive_gap_minutes: u32,
    pub required_recent_minutes: u32,
}

impl Default for SpikeDropParams {
    fn default() -> Self {
        SpikeDropParams {
            price_drop_pct: "12".to_string(),
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

/// Parameters of any registered algorithm. Serialized/deserialized as the
/// concrete struct of the variant (`untagged`): spike params carry
/// `priceGrowthPct`, drop params carry `priceDropPct`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AlgorithmParams {
    Spike(SpikeParams),
    SpikeDrop(SpikeDropParams),
}

impl Default for AlgorithmParams {
    fn default() -> Self {
        AlgorithmParams::Spike(SpikeParams::default())
    }
}

impl AlgorithmParams {
    pub fn algorithm_id(&self) -> &'static str {
        match self {
            AlgorithmParams::Spike(_) => SPIKE_ALGORITHM_ID,
            AlgorithmParams::SpikeDrop(_) => SPIKE_DROP_ALGORITHM_ID,
        }
    }

    pub fn algorithm_version(&self) -> &'static str {
        match self {
            AlgorithmParams::Spike(_) => SPIKE_ALGORITHM_VERSION,
            AlgorithmParams::SpikeDrop(_) => SPIKE_DROP_ALGORITHM_VERSION,
        }
    }

    /// Price threshold in percent: growth for the spike algorithm,
    /// drop for the mirror algorithm.
    pub fn price_pct(&self) -> &str {
        match self {
            AlgorithmParams::Spike(params) => &params.price_growth_pct,
            AlgorithmParams::SpikeDrop(params) => &params.price_drop_pct,
        }
    }

    pub fn window_minutes(&self) -> u32 {
        match self {
            AlgorithmParams::Spike(params) => params.window_minutes,
            AlgorithmParams::SpikeDrop(params) => params.window_minutes,
        }
    }

    pub fn baseline_long_days(&self) -> u32 {
        match self {
            AlgorithmParams::Spike(params) => params.baseline_long_days,
            AlgorithmParams::SpikeDrop(params) => params.baseline_long_days,
        }
    }

    pub fn baseline_short_days(&self) -> u32 {
        match self {
            AlgorithmParams::Spike(params) => params.baseline_short_days,
            AlgorithmParams::SpikeDrop(params) => params.baseline_short_days,
        }
    }

    pub fn volume_multiplier(&self) -> &str {
        match self {
            AlgorithmParams::Spike(params) => &params.volume_multiplier,
            AlgorithmParams::SpikeDrop(params) => &params.volume_multiplier,
        }
    }

    pub fn cooldown_minutes(&self) -> u32 {
        match self {
            AlgorithmParams::Spike(params) => params.cooldown_minutes,
            AlgorithmParams::SpikeDrop(params) => params.cooldown_minutes,
        }
    }

    pub fn max_gap_percent(&self) -> &str {
        match self {
            AlgorithmParams::Spike(params) => &params.max_gap_percent,
            AlgorithmParams::SpikeDrop(params) => &params.max_gap_percent,
        }
    }

    pub fn max_consecutive_gap_minutes(&self) -> u32 {
        match self {
            AlgorithmParams::Spike(params) => params.max_consecutive_gap_minutes,
            AlgorithmParams::SpikeDrop(params) => params.max_consecutive_gap_minutes,
        }
    }

    pub fn required_recent_minutes(&self) -> u32 {
        match self {
            AlgorithmParams::Spike(params) => params.required_recent_minutes,
            AlgorithmParams::SpikeDrop(params) => params.required_recent_minutes,
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
    pub params: AlgorithmParams,
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn algorithm_params_roundtrip_keeps_variant_shape() {
        let spike = AlgorithmParams::Spike(SpikeParams::default());
        let value = serde_json::to_value(&spike).expect("spike serializes");
        assert_eq!(value["priceGrowthPct"], json!("12"));
        assert!(value.get("priceDropPct").is_none());
        let parsed: AlgorithmParams = serde_json::from_value(value).expect("spike parses back");
        assert_eq!(parsed, spike);
        assert_eq!(parsed.algorithm_id(), SPIKE_ALGORITHM_ID);

        let drop = AlgorithmParams::SpikeDrop(SpikeDropParams::default());
        let value = serde_json::to_value(&drop).expect("drop serializes");
        assert_eq!(value["priceDropPct"], json!("12"));
        assert!(value.get("priceGrowthPct").is_none());
        let parsed: AlgorithmParams = serde_json::from_value(value).expect("drop parses back");
        assert_eq!(parsed, drop);
        assert_eq!(parsed.algorithm_id(), SPIKE_DROP_ALGORITHM_ID);
        assert_eq!(parsed.algorithm_version(), SPIKE_DROP_ALGORITHM_VERSION);
        assert_eq!(parsed.price_pct(), "12");
        assert_eq!(parsed.window_minutes(), 5);
        assert_eq!(parsed.baseline_long_days(), 30);
        assert_eq!(parsed.baseline_short_days(), 7);
        assert_eq!(parsed.volume_multiplier(), "3");
    }
}
