//! Isolated, bounded optimizer computations. No network, database or shared caches.
mod evaluation;
mod frequency;
mod search;
mod types;
mod validation;

pub use evaluation::{canonical_candidate_key, evaluate_candidate, score_matrix, snapshot_bytes};
pub use search::run_study;
pub use setups_replay::CancelToken;
pub use types::*;
pub use validation::{default_parameter_descriptors, required_input_bounds, validate_request};

pub const OPTIMIZER_VERSION: &str = "plan-a-2";
