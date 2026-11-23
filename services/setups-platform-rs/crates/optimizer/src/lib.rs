//! Isolated, bounded optimizer computations. No network, database or shared caches.
mod checkpoint;
mod continuous;
mod evaluation;
mod frequency;
mod input_codec;
mod search;
mod types;
mod validation;

pub use checkpoint::{
    CheckpointAction, ContinuousCheckpointInfo, ContinuousSearchControl, ContinuousSearchExit,
    ManualCheckpointIdentity, ManualCheckpointInfo, ManualSearchControl, ManualSearchExit,
};
pub use checkpoint::{
    FORMAT_VERSION as CONTINUOUS_CHECKPOINT_VERSION,
    RNG_VERSION as CONTINUOUS_CHECKPOINT_RNG_VERSION,
    MANUAL_FORMAT_VERSION as MANUAL_CHECKPOINT_VERSION,
};
pub use continuous::*;
pub use evaluation::{canonical_candidate_key, evaluate_candidate, score_matrix, snapshot_bytes};
pub use evaluation::manual_forecast_observation_cutoff;
mod trend;
pub use trend::score_period_mean;
pub use input_codec::{
    FrozenInputCodecLimits, FrozenInputDigest, read_frozen_input, write_frozen_input,
};
pub use search::run_study;
pub use search::run_study_with_original_minutes;
pub use search::search_continuous_resumable;
pub use search::{search_manual_resumable, export_manual_checkpoint_partial};
pub use setups_replay::CancelToken;
pub use types::*;
pub use validation::{default_parameter_descriptors, required_input_bounds, validate_request};
pub use validation::parse_params as saved_algorithm_params;

pub const OPTIMIZER_VERSION: &str = "plan-a-3";
pub const CONTINUOUS_OPTIMIZER_VERSION: &str = "plan-a-continuous-1";
