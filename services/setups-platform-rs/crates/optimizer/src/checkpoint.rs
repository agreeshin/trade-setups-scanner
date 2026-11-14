//! Private continuous checkpoints, never public in-progress study results.
use crate::*;
use rand::{RngCore, SeedableRng, rngs::StdRng};
use rand_chacha::ChaCha12Rng;
use serde::{Deserialize, Serialize};
use std::time::Duration;

pub const FORMAT_VERSION: u32 = 1;
// Distribution/shuffle implementation and usize width also affect proposals.
pub const RNG_VERSION: &str = "chacha12-0.9.0/rand-0.9.5-v1";

pub struct ContinuousSearchControl<'a> {
    pub cancel: &'a CancelToken,
    /// Optional explicit shutdown suspension, checked ONLY at joined boundaries.
    /// Maintenance drain must NOT set this: it lets the whole chain finish.
    pub pause: &'a CancelToken,
    /// Runtime derives this from the ORIGINAL persisted absolute deadline.
    pub remaining_budget: Duration,
    pub resume_checkpoint: Option<&'a [u8]>,
    pub max_checkpoint_bytes: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckpointAction {
    Continue,
    Suspend,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ContinuousCheckpointInfo {
    pub sequence: u64,
    pub progress: StudyProgress,
}

#[derive(Debug, Clone, PartialEq)]
// One owned exit value, never a retained queue/collection. Keep the additive
// public API's StudyOutcome ownership identical to existing search entry points.
#[allow(clippy::large_enum_variant)]
pub enum ContinuousSearchExit {
    Completed(StudyOutcome),
    Suspended(ContinuousCheckpointInfo),
}

pub(crate) enum SearchRng {
    Legacy(StdRng),
    Durable(ChaCha12Rng),
}
impl SearchRng {
    pub fn new(seed: u64, durable: bool) -> Self {
        if durable {
            Self::Durable(ChaCha12Rng::seed_from_u64(seed))
        } else {
            Self::Legacy(StdRng::seed_from_u64(seed))
        }
    }
}
impl RngCore for SearchRng {
    fn next_u32(&mut self) -> u32 {
        match self {
            Self::Legacy(r) => r.next_u32(),
            Self::Durable(r) => r.next_u32(),
        }
    }
    fn next_u64(&mut self) -> u64 {
        match self {
            Self::Legacy(r) => r.next_u64(),
            Self::Durable(r) => r.next_u64(),
        }
    }
    fn fill_bytes(&mut self, dst: &mut [u8]) {
        match self {
            Self::Legacy(r) => r.fill_bytes(dst),
            Self::Durable(r) => r.fill_bytes(dst),
        }
    }
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RngState {
    seed: [u8; 32],
    stream: u64,
    word_position: u128,
}
impl Serialize for SearchRng {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let Self::Durable(rng) = self else {
            return Err(serde::ser::Error::custom(
                "manual RNG is not checkpointable",
            ));
        };
        RngState {
            seed: rng.get_seed(),
            stream: rng.get_stream(),
            word_position: rng.get_word_pos(),
        }
        .serialize(serializer)
    }
}
impl<'de> Deserialize<'de> for SearchRng {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let state = RngState::deserialize(deserializer)?;
        if state.word_position >= 1u128 << 68 {
            return Err(serde::de::Error::custom("RNG position out of range"));
        }
        let mut rng = ChaCha12Rng::from_seed(state.seed);
        rng.set_stream(state.stream);
        rng.set_word_pos(state.word_position);
        Ok(Self::Durable(rng))
    }
}

pub(crate) mod sorted_strings {
    use serde::{Deserialize, Serialize};
    use std::collections::HashSet;
    pub fn serialize<S: serde::Serializer>(v: &HashSet<String>, s: S) -> Result<S::Ok, S::Error> {
        let mut ordered: Vec<_> = v.iter().collect();
        ordered.sort();
        ordered.serialize(s)
    }
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(
        d: D,
    ) -> Result<HashSet<String>, D::Error> {
        let ordered = Vec::<String>::deserialize(d)?;
        if ordered.windows(2).any(|w| w[0] >= w[1]) {
            return Err(serde::de::Error::custom(
                "seen identities not strictly ordered",
            ));
        }
        Ok(ordered.into_iter().collect())
    }
}

/// Serialization stops BEFORE exceeding the byte cap; never builds an unbounded
/// temporary buffer. Input storage has its own separate snapshot budget.
pub(crate) struct BoundedBytes {
    pub bytes: Vec<u8>,
    pub limit: usize,
}
impl std::io::Write for BoundedBytes {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        if data.len() > self.limit.saturating_sub(self.bytes.len()) {
            return Err(std::io::Error::other("codec byte limit exceeded"));
        }
        self.bytes.extend_from_slice(data);
        Ok(data.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::{Rng, seq::SliceRandom};

    #[test]
    fn explicit_rng_matches_legacy_mixed_operations_and_restores_exact_word_position() {
        for seed in [0, 1, 42, 168, u64::MAX] {
            let mut old = StdRng::seed_from_u64(seed);
            let mut durable = SearchRng::new(seed, true);
            for step in 0..2000 {
                assert_eq!(
                    old.random_range(0..7919usize),
                    durable.random_range(0..7919usize)
                );
                assert_eq!(old.random_bool(0.5), durable.random_bool(0.5));
                if step % 7 == 0 {
                    let mut left = [0; 17];
                    let mut right = [0; 17];
                    old.fill_bytes(&mut left);
                    durable.fill_bytes(&mut right);
                    assert_eq!(left, right);
                }
                if step % 13 == 0 {
                    let mut left: Vec<_> = (0..19).collect();
                    let mut right = left.clone();
                    left.shuffle(&mut old);
                    right.shuffle(&mut durable);
                    assert_eq!(left, right);
                    let mut bytes = Vec::new();
                    ciborium::ser::into_writer(&durable, &mut bytes).unwrap();
                    durable = ciborium::de::from_reader(bytes.as_slice()).unwrap();
                }
            }
        }
    }

    #[test]
    fn cbor_preserves_ieee_bits_and_arbitrary_precision_value_lexemes() {
        #[derive(Serialize, Deserialize)]
        struct Floats {
            values: Vec<f64>,
            parameter: serde_json::Value,
        }
        let value = Floats {
            values: vec![
                0.0,
                -0.0,
                0.10000000000000002,
                f64::from_bits(1),
                f64::MIN_POSITIVE,
                f64::MAX,
            ],
            parameter: serde_json::from_str(
                "[0.10000000000000002,1.000000000000000001,184467440737095516160]",
            )
            .unwrap(),
        };
        let mut bytes = Vec::new();
        ciborium::ser::into_writer(&value, &mut bytes).unwrap();
        let decoded: Floats = ciborium::de::from_reader(bytes.as_slice()).unwrap();
        assert_eq!(
            decoded
                .values
                .iter()
                .map(|v| v.to_bits())
                .collect::<Vec<_>>(),
            value.values.iter().map(|v| v.to_bits()).collect::<Vec<_>>()
        );
        assert_eq!(decoded.parameter.to_string(), value.parameter.to_string());
    }
}
