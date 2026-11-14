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
};
pub use checkpoint::{
    FORMAT_VERSION as CONTINUOUS_CHECKPOINT_VERSION,
    RNG_VERSION as CONTINUOUS_CHECKPOINT_RNG_VERSION,
};
pub use continuous::*;
pub use evaluation::{canonical_candidate_key, evaluate_candidate, score_matrix, snapshot_bytes};
pub use input_codec::{
    FrozenInputCodecLimits, FrozenInputDigest, read_frozen_input, write_frozen_input,
};
pub use search::run_study;
pub use search::search_continuous_resumable;
pub use setups_replay::CancelToken;
pub use types::*;
pub use validation::{default_parameter_descriptors, required_input_bounds, validate_request};

pub const OPTIMIZER_VERSION: &str = "plan-a-3";
pub const CONTINUOUS_OPTIMIZER_VERSION: &str = "plan-a-continuous-1";
