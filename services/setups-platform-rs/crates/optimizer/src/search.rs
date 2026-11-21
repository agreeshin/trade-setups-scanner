use crate::checkpoint::{BoundedBytes, SearchRng};
use crate::evaluation::{Control, evaluate_group, limit, validate_input};
use crate::validation::{analytics_with_value, invalid, normalized_decimal, parse_params};
use crate::*;
use rand::{Rng, seq::SliceRandom};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashSet, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

// Packet-owned results are bounded by batch_size and never serialized. Keep
// candidate infeasibility distinct from cancellation/deadline interruption.
enum TrialEvaluation {
    Evaluated(CandidateEvaluation),
    SetupLimitExceeded { limit: usize, requested: usize },
    Interrupted,
}
type PacketEvaluation = Result<Vec<(usize, TrialEvaluation)>, StudyError>;

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Dimension {
    key: String,
    algorithm: bool,
    values: Vec<Value>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    coordinates: Vec<usize>,
    score: Option<f64>,
    trial: usize,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Proposal {
    candidate: StudyCandidate,
    coordinates: Option<Vec<usize>>,
    comparison: Option<(usize, usize, Option<f64>)>,
    phase: StudyPhase,
    packet_child: bool,
}
#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Influence {
    total_difference: f64,
    comparisons: usize,
    contexts: BTreeSet<usize>,
}

/// All retained state is bounded by dimensions/domain values and maxTrials.
/// It stores coordinates/scores and canonical identities, never trial matrices.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Search {
    baseline: StudyCandidate,
    baseline_coordinates: Option<Vec<usize>>,
    dimensions: Vec<Dimension>,
    initial: Vec<Vec<usize>>,
    initial_index: usize,
    baseline_sent: bool,
    sensitivity_step: usize,
    refinement_step: usize,
    records: Vec<Record>,
    influence: Vec<Influence>,
    #[serde(with = "crate::checkpoint::sorted_strings")]
    seen: HashSet<String>,
    pending: VecDeque<Proposal>,
    random: SearchRng,
    enumeration: Vec<usize>,
    enumeration_finished: bool,
    attempts: usize,
    invalid: usize,
    max_attempts: usize,
    max_trials: usize,
    algorithm_id: String,
}

impl Search {
    fn new(spec: &StudyRequest) -> Result<Self, StudyError> {
        Self::with_rng(spec, false)
    }

    fn with_rng(spec: &StudyRequest, durable: bool) -> Result<Self, StudyError> {
        let baseline = StudyCandidate {
            params: spec.algorithm_params.clone(),
            analytics: spec.analytics_params.clone(),
        };
        let mut dimensions = Vec::new();
        for (algorithm, domains) in [
            (true, &spec.algorithm_search),
            (false, &spec.analytics_search),
        ] {
            for (key, domain) in domains {
                // Remove provably inactive dimensions before mixed-radix
                // enumeration, not by one-context sensitivity observations.
                if !algorithm
                    && key == "clusterGapMinutes"
                    && !spec.analytics_params.cluster_enabled
                    && spec
                        .analytics_search
                        .get("clusterEnabled")
                        .is_none_or(|values| values.iter().all(|v| v == &Value::Bool(false)))
                {
                    continue;
                }
                let mut values = Vec::new();
                let mut seen = BTreeSet::new();
                for value in domain {
                    let mut value = value.clone();
                    if algorithm && let Value::String(text) = &mut value {
                        *text = normalized_decimal(text)?;
                    }
                    let identity = if !algorithm
                        && key == "priceChangePct"
                        && spec.analytics_params.time_shift_minutes == 0
                        && spec
                            .analytics_search
                            .get("timeShiftMinutes")
                            .is_none_or(|values| values.iter().all(|v| v.as_i64() == Some(0)))
                    {
                        if value.as_f64().is_some_and(|v| v > 0.0) {
                            // Positive gates are not all equivalent in the
                            // scalar f64 kernel (e.g. 1e-20 can pass at shift 0).
                            value.to_string()
                        } else {
                            "pass".to_string()
                        }
                    } else {
                        value.to_string()
                    };
                    if seen.insert(identity) {
                        values.push(value);
                    }
                }
                values.sort_by(|a, b| {
                    let number = |v: &Value| {
                        v.as_f64()
                            .or_else(|| v.as_str().and_then(|s| s.parse::<f64>().ok()))
                    };
                    match (number(a), number(b)) {
                        (Some(a_num), Some(b_num)) => a_num
                            .total_cmp(&b_num)
                            .then_with(|| a.to_string().cmp(&b.to_string())),
                        _ => a.to_string().cmp(&b.to_string()),
                    }
                });
                dimensions.push(Dimension {
                    key: key.clone(),
                    algorithm,
                    values,
                });
            }
        }
        let baseline_analytics = serde_json::to_value(&baseline.analytics)
            .map_err(|error| invalid(error.to_string()))?;
        let baseline_coordinates: Option<Vec<usize>> = dimensions
            .iter()
            .map(|dimension| {
                let mut value = if dimension.algorithm {
                    baseline.params[&dimension.key].clone()
                } else {
                    baseline_analytics[&dimension.key].clone()
                };
                if dimension.algorithm
                    && let Value::String(text) = &mut value
                {
                    *text = normalized_decimal(text).expect("validated baseline decimal");
                }
                dimension
                    .values
                    .iter()
                    .position(|candidate| candidate == &value)
            })
            .collect();
        let mut random = SearchRng::new(spec.seed, durable);
        let initial_count = spec
            .max_trials
            .saturating_sub(1)
            .min((dimensions.len() * 2).clamp(4, 64))
            .min((spec.max_trials / 8).max(4));
        let mut initial = vec![vec![0; dimensions.len()]; initial_count];
        if initial_count > 0 {
            // Discrete Latin hypercube: each coordinate covers its strata,
            // independently permuted, rather than clustered random starts.
            for (dim, dimension) in dimensions.iter().enumerate() {
                let mut strata: Vec<_> = (0..initial_count).collect();
                strata.shuffle(&mut random);
                for row in 0..initial_count {
                    initial[row][dim] = ((2 * strata[row] + 1) * dimension.values.len()
                        / (2 * initial_count))
                        .min(dimension.values.len() - 1);
                }
            }
        }
        Ok(Self {
            influence: (0..dimensions.len())
                .map(|_| Influence::default())
                .collect(),
            enumeration: vec![0; dimensions.len()],
            dimensions,
            baseline,
            baseline_coordinates,
            initial,
            initial_index: 0,
            baseline_sent: false,
            sensitivity_step: 0,
            refinement_step: 0,
            records: Vec::new(),
            seen: HashSet::new(),
            pending: VecDeque::new(),
            random,
            enumeration_finished: false,
            attempts: 0,
            invalid: 0,
            max_attempts: spec.max_trials.saturating_mul(128).saturating_add(4096),
            max_trials: spec.max_trials,
            algorithm_id: spec.algorithm_id.clone(),
        })
    }

    fn build(
        &self,
        coordinates: Vec<usize>,
        phase: StudyPhase,
        comparison: Option<(usize, usize, Option<f64>)>,
    ) -> Result<Proposal, StudyError> {
        let mut candidate = self.baseline.clone();
        for (dim, &index) in self.dimensions.iter().zip(&coordinates) {
            if dim.algorithm {
                candidate
                    .params
                    .insert(dim.key.clone(), dim.values[index].clone());
            } else {
                analytics_with_value(&mut candidate.analytics, &dim.key, &dim.values[index])?;
            }
        }
        Ok(Proposal {
            candidate,
            coordinates: Some(coordinates),
            comparison,
            phase,
            packet_child: false,
        })
    }

    fn contexts(&self) -> Vec<usize> {
        let mut indexes: Vec<_> = (0..self.records.len()).collect();
        indexes.sort_by(|&a, &b| {
            let left = &self.records[a];
            let right = &self.records[b];
            right
                .score
                .unwrap_or(f64::NEG_INFINITY)
                .total_cmp(&left.score.unwrap_or(f64::NEG_INFINITY))
                .then(left.trial.cmp(&right.trial))
        });
        indexes.truncate(4);
        indexes
    }

    fn random_coordinates(&mut self) -> Vec<usize> {
        self.dimensions
            .iter()
            .map(|dimension| self.random.random_range(0..dimension.values.len()))
            .collect()
    }

    fn next_enumerated(&mut self) -> Option<Vec<usize>> {
        if self.enumeration_finished {
            return None;
        }
        let coordinates = self.enumeration.clone();
        let mut carry = true;
        for (index, dimension) in self.dimensions.iter().enumerate().rev() {
            self.enumeration[index] += 1;
            if self.enumeration[index] < dimension.values.len() {
                carry = false;
                break;
            }
            self.enumeration[index] = 0;
        }
        self.enumeration_finished = carry;
        Some(coordinates)
    }

    fn root(&mut self) -> Result<Option<Proposal>, StudyError> {
        if !self.baseline_sent {
            self.baseline_sent = true;
            return Ok(Some(Proposal {
                candidate: self.baseline.clone(),
                coordinates: self.baseline_coordinates.clone(),
                comparison: None,
                phase: StudyPhase::Initial,
                packet_child: false,
            }));
        }
        if self.dimensions.is_empty() {
            return Ok(None);
        }
        if self.initial_index < self.initial.len() {
            let coordinates = self.initial[self.initial_index].clone();
            self.initial_index += 1;
            return self.build(coordinates, StudyPhase::Initial, None).map(Some);
        }
        let contexts = self.contexts();
        let dims = self.dimensions.len();
        // Two directions, at least three contexts where budget permits. The
        // resulting effects only prioritize moves; no dimension is pruned.
        let sensitivity_count = (dims * 3 * 2).min(self.max_trials / 6);
        if !contexts.is_empty() && self.sensitivity_step < sensitivity_count {
            let step = self.sensitivity_step;
            self.sensitivity_step += 1;
            let context = contexts[(step / dims) % contexts.len()];
            let record = &self.records[context];
            let dim = step % dims;
            let mut coordinates = record.coordinates.clone();
            let width = self.dimensions[dim].values.len();
            let stride = (width / 2).max(1);
            coordinates[dim] = if (step / (dims * contexts.len())).is_multiple_of(2) {
                (coordinates[dim] + stride) % width
            } else {
                (coordinates[dim] + width - stride.min(width)) % width
            };
            return self
                .build(
                    coordinates,
                    StudyPhase::Sensitivity,
                    Some((record.trial, dim, record.score)),
                )
                .map(Some);
        }
        let step = self.refinement_step;
        self.refinement_step += 1;
        if contexts.is_empty() || step % 4 == 3 {
            let coordinates = self.random_coordinates();
            return self
                .build(coordinates, StudyPhase::Refinement, None)
                .map(Some);
        }
        let context = contexts[(step / 4) % contexts.len()];
        let mut coordinates = self.records[context].coordinates.clone();
        let mut priority: Vec<_> = (0..dims).collect();
        priority.sort_by(|&a, &b| {
            let effect = |i: usize| {
                self.influence[i].total_difference / self.influence[i].comparisons.max(1) as f64
            };
            effect(b).total_cmp(&effect(a)).then(a.cmp(&b))
        });
        let count = match step % 4 {
            0 => 1,
            1 => 2.min(dims),
            _ => dims,
        };
        let stage = (self.records.len() * 4 / self.max_trials.max(1)).min(3);
        for offset in 0..count {
            let dim = priority[(step / 4 + offset) % dims];
            let width = self.dimensions[dim].values.len();
            let stride = ((width / 2) >> stage).max(1);
            coordinates[dim] = if self.random.random_bool(0.5) {
                (coordinates[dim] + stride) % width
            } else {
                (coordinates[dim] + width - stride.min(width)) % width
            };
        }
        self.build(coordinates, StudyPhase::Refinement, None)
            .map(Some)
    }

    fn packet_siblings(&mut self, proposal: &Proposal) -> Result<(), StudyError> {
        if proposal.packet_child {
            return Ok(());
        }
        let Some(coordinates) = &proposal.coordinates else {
            return Ok(());
        };
        let analytics_dims: Vec<_> = self
            .dimensions
            .iter()
            .enumerate()
            .filter_map(|(i, d)| (!d.algorithm && d.values.len() > 1).then_some(i))
            .collect();
        // At most two siblings per root, all using exactly its algorithm.
        for offset in 0..analytics_dims.len().min(2) {
            let dim = analytics_dims[(self.attempts + offset) % analytics_dims.len()];
            let mut sibling = coordinates.clone();
            sibling[dim] = (sibling[dim] + 1 + offset) % self.dimensions[dim].values.len();
            let mut child = self.build(sibling, proposal.phase, None)?;
            child.packet_child = true;
            self.pending.push_back(child);
        }
        Ok(())
    }

    #[cfg(test)]
    fn next(&mut self) -> Result<Option<Proposal>, StudyError> {
        self.next_controlled(|| false)
    }

    fn next_controlled(
        &mut self,
        stopped: impl Fn() -> bool,
    ) -> Result<Option<Proposal>, StudyError> {
        let mut consecutive_misses = 0;
        loop {
            if stopped() {
                return Ok(None);
            }
            if self.attempts >= self.max_attempts {
                return Err(StudyError::Execution(format!(
                    "proposal_attempt_limit: {} attempts, {} invalid combinations",
                    self.attempts, self.invalid
                )));
            }
            self.attempts += 1;
            // A deterministic fallback advances even with invalid coupled
            // combinations/conditional duplicates. Never retry indefinitely.
            let proposal = if consecutive_misses >= 16 || self.attempts.is_multiple_of(5) {
                match self.next_enumerated() {
                    Some(coords) => self.build(coords, StudyPhase::Refinement, None)?,
                    None if consecutive_misses >= 16 => return Ok(None),
                    None => match self
                        .pending
                        .pop_front()
                        .map(Some)
                        .map(Ok)
                        .unwrap_or_else(|| self.root())?
                    {
                        Some(p) => p,
                        None => return Ok(None),
                    },
                }
            } else {
                match self.pending.pop_front() {
                    Some(proposal) => proposal,
                    None => match self.root()? {
                        Some(proposal) => proposal,
                        None => return Ok(None),
                    },
                }
            };
            if parse_params(&self.algorithm_id, &proposal.candidate.params).is_err() {
                self.invalid += 1;
                consecutive_misses += 1;
                continue;
            }
            let key = canonical_candidate_key(&proposal.candidate)?;
            if !self.seen.insert(key) {
                consecutive_misses += 1;
                continue;
            }
            self.packet_siblings(&proposal)?;
            return Ok(Some(proposal));
        }
    }

    fn observe(&mut self, proposal: &Proposal, score: Option<f64>, trial: usize) {
        if let Some((context, dim, Some(parent_score))) = proposal.comparison
            && let Some(score) = score
        {
            let influence = &mut self.influence[dim];
            influence.total_difference += (score - parent_score).abs();
            influence.comparisons += 1;
            influence.contexts.insert(context);
        }
        if let Some(coordinates) = &proposal.coordinates {
            self.records.push(Record {
                coordinates: coordinates.clone(),
                score,
                trial,
            });
        }
    }

    fn conclusions(&self, completed: usize) -> Vec<String> {
        let mut conclusions = vec![format!(
            "Лучший полностью проверенный кандидат среди {completed} завершённых оценок; глобальный оптимум не доказан."
        )];
        if self.invalid > 0 {
            conclusions.push(format!("Пропущено {} предложений, нарушающих составные ограничения модуля; некорректные кандидаты не оценивались.", self.invalid));
        }
        let mut effects: Vec<_> = self
            .influence
            .iter()
            .enumerate()
            .filter(|(_, effect)| effect.comparisons > 0)
            .collect();
        effects.sort_by(|(_, a), (_, b)| {
            (b.total_difference / b.comparisons as f64)
                .total_cmp(&(a.total_difference / a.comparisons as f64))
        });
        for (dim, effect) in effects.into_iter().take(4) {
            conclusions.push(format!("Наблюдение для {}: среднее абсолютное изменение оценки {:.6} по {} сравнениям в {} контекстах. Это наблюдаемая связь, не доказательство причинности или общего влияния.",
                self.dimensions[dim].key, effect.total_difference / effect.comparisons as f64, effect.comparisons, effect.contexts.len()));
        }
        conclusions
    }
}

fn make_best(
    spec: &StudyRequest,
    input: &StudyInput,
    candidate: &StudyCandidate,
    evaluation: CandidateEvaluation,
    trial: usize,
    continuous: bool,
    observation_cutoff: Option<i64>,
) -> StudyBest {
    StudyBest {
        score: evaluation.summary.score.expect("scorable evaluation"),
        mean_score: (!spec.trend_mode.is_mean()).then(|| score_period_mean(&evaluation.matrix, spec.min_closed_per_cell)).flatten(),
        partial: true,
        params: candidate.params.clone(),
        analytics: EffectiveAnalytics {
            cluster_enabled: candidate.analytics.cluster_enabled,
            cluster_gap_minutes: candidate
                .analytics
                .cluster_enabled
                .then_some(candidate.analytics.cluster_gap_minutes),
            time_shift_minutes: candidate.analytics.time_shift_minutes,
            price_change_pct: candidate.analytics.price_change_pct,
            setup_maturity: candidate
                .analytics
                .cluster_enabled
                .then_some(spec.setup_maturity),
            horizon_minutes: spec.horizon_minutes,
        },
        matrix: evaluation.matrix,
        support: evaluation.summary,
        trial_index: trial,
        trend_mode: spec.trend_mode,
        trend_bend_days: if spec.trend_mode.is_mean() { TrendBendDays::Linear } else { crate::trend::effective_bend(spec) },
        trend_blend_lambda: if spec.trend_mode == TrendMode::Blend { spec.trend_blend_lambda } else { TrendBlendLambda::default() },
        forecast: spec.forecast,
        observation_cutoff,
        provenance: Provenance {
            optimizer_version: if continuous {
                CONTINUOUS_OPTIMIZER_VERSION
            } else {
                OPTIMIZER_VERSION
            }
            .into(),
            seed: spec.seed,
            snapshot_hash: input.snapshot_hash.clone(),
            captured_at: input.captured_at.clone(),
        },
        conclusions: Vec::new(),
    }
}

pub fn run_study(
    spec: &StudyRequest,
    input: &StudyInput,
    limits: &StudyLimits,
    cancel: &CancelToken,
    progress: impl FnMut(StudyUpdate),
) -> Result<StudyOutcome, StudyError> {
    run_study_mode(spec, input, limits, cancel, progress, None)
}

/// Manual runtime entry point with an independently owned immutable archive.
/// Captures only a prospective incumbent, within the SAME deadline/cancel token,
/// before best+ledger are promoted together. A failed capture keeps the previous
/// complete pair. The scientific result/progress/checkpoint schemas are unchanged.
pub fn run_study_with_original_minutes(
    spec:&StudyRequest,input:&StudyInput,limits:&StudyLimits,cancel:&CancelToken,
    progress:impl FnMut(StudyUpdate),
)->Result<(StudyOutcome,Option<ManualOriginalArchive>),StudyError> {
    let observation_cutoff=crate::evaluation::manual_forecast_observation_cutoff(spec,input)?;
    let mut minutes=None;
    let exit=run_engine(spec,input,limits,cancel,progress,EngineMode{
        continuous:None,resume:None,manual_minutes:Some(&mut minutes),
        #[cfg(test)] manual_capture_observer:None,
    })?;
    match exit {
        ContinuousSearchExit::Completed(outcome)=>Ok((outcome,minutes.map(|minutes|ManualOriginalArchive{observation_cutoff,minutes}))),
        ContinuousSearchExit::Suspended(_)=>Err(StudyError::Execution("unexpected manual suspension".into())),
    }
}

pub(crate) fn run_study_mode(
    spec: &StudyRequest,
    input: &StudyInput,
    limits: &StudyLimits,
    cancel: &CancelToken,
    progress: impl FnMut(StudyUpdate),
    continuous: Option<ContinuousContext>,
) -> Result<StudyOutcome, StudyError> {
    match run_engine(
        spec,
        input,
        limits,
        cancel,
        progress,
        EngineMode {
            continuous,
            resume: None,
            manual_minutes:None,
            #[cfg(test)] manual_capture_observer:None,
        },
    )? {
        ContinuousSearchExit::Completed(outcome) => Ok(outcome),
        ContinuousSearchExit::Suspended(_) => {
            Err(StudyError::Execution("unexpected manual suspension".into()))
        }
    }
}

type CheckpointCallback<'a> =
    dyn FnMut(&ContinuousCheckpointInfo, &[u8]) -> Result<CheckpointAction, StudyError> + 'a;
struct ResumeHooks<'a> {
    control: ContinuousSearchControl<'a>,
    checkpoint: &'a mut CheckpointCallback<'a>,
}
struct EngineMode<'a> {
    continuous: Option<ContinuousContext>,
    resume: Option<ResumeHooks<'a>>,
    manual_minutes:Option<&'a mut Option<ContinuousSelectedMinutes>>,
    // A scoped unit-test probe, never compiled into the native runtime or DTOs.
    #[cfg(test)]
    manual_capture_observer:Option<&'a mut dyn FnMut(usize,usize,Instant)>,
}

/// Continuous-only private recovery. Callback MUST durably commit/ack before
/// returning. Maintenance drain does not pause admitted work; `pause` is only an
/// explicit process-shutdown boundary. Original wall deadline belongs to runtime.
pub fn search_continuous_resumable(
    spec: &StudyRequest,
    input: &StudyInput,
    context: ContinuousContext,
    limits: &StudyLimits,
    control: ContinuousSearchControl<'_>,
    progress: impl FnMut(StudyUpdate),
    mut checkpoint: impl FnMut(&ContinuousCheckpointInfo, &[u8]) -> Result<CheckpointAction, StudyError>,
) -> Result<ContinuousSearchExit, StudyError> {
    let cancel = control.cancel;
    run_engine(
        spec,
        input,
        limits,
        cancel,
        progress,
        EngineMode {
            continuous: Some(context),
            manual_minutes:None,
            #[cfg(test)] manual_capture_observer:None,
            resume: Some(ResumeHooks {
                control,
                checkpoint: &mut checkpoint,
            }),
        },
    )
}

fn run_engine(
    spec: &StudyRequest,
    input: &StudyInput,
    limits: &StudyLimits,
    cancel: &CancelToken,
    mut progress: impl FnMut(StudyUpdate),
    mut mode: EngineMode<'_>,
) -> Result<ContinuousSearchExit, StudyError> {
    let continuous = mode.continuous;
    if let Some(context) = continuous {
        validate_continuous_request(spec, context)?;
    } else {
        validate_request(spec, &default_parameter_descriptors(&spec.algorithm_id)?)?;
    }
    let forecast_cutoff = if spec.forecast.is_some() {
        Some(match continuous {
            Some(context) => context.observation_cutoff,
            None => crate::evaluation::manual_forecast_observation_cutoff(spec,input)?,
        })
    } else { None };
    if limits.cpu_threads == 0
        || limits.cpu_threads > 64
        || limits.batch_size == 0
        || limits.batch_size > 64
        || limits.max_setups_per_candidate == 0
        || limits.max_working_bytes == 0
        || limits.max_snapshot_bytes == 0
    {
        return Err(invalid("invalid execution limits"));
    }
    let domain_values: usize = spec
        .algorithm_search
        .values()
        .chain(spec.analytics_search.values())
        .map(Vec::len)
        .sum();
    limit("domain_values", domain_values, limits.max_domain_values)?;
    let serialized_bytes = serde_json::to_vec(spec)
        .map_err(|error| invalid(error.to_string()))?
        .len();
    let manual_archive_bytes = if mode.manual_minutes.is_some() {
        crate::continuous::SelectedMinuteCapture::budget_bytes(limits.max_setups_per_candidate.min(100_000))
    } else { 0 };
    // Include bounded search identities/coordinates plus a retained incumbent
    // matrix in the working reservation, before allocating a Rayon pool.
    let search_bytes = serialized_bytes
        .saturating_mul(4)
        .saturating_add(limits.cpu_threads * (512 * 1024 + 16 * 1024))
        .saturating_add(spec.max_trials.saturating_mul(
            2048 + (spec.algorithm_search.len() + spec.analytics_search.len()) * 16,
        ))
        .saturating_add(
            spec.tp_levels.len()
                * spec.sl_levels.len()
                * std::mem::size_of::<MatrixCell>()
                * (limits.batch_size + 2),
        )
        // The current incumbent's scalar vector plus one temporary promotion
        // capture/encode reservation. No input/setup references or trial cache.
        .saturating_add(manual_archive_bytes.saturating_mul(2));
    limit(
        "search_working_bytes",
        search_bytes,
        limits.max_working_bytes,
    )?;
    let start = Instant::now();
    let restored = if let Some(hooks) = &mode.resume {
        if hooks.control.remaining_budget > Duration::from_secs(spec.max_seconds)
            || hooks.control.max_checkpoint_bytes == 0
        {
            return Err(checkpoint_error("invalid remaining/checkpoint budget"));
        }
        hooks
            .control
            .resume_checkpoint
            .map(|bytes| {
                decode_checkpoint(
                    bytes,
                    spec,
                    input,
                    continuous.expect("continuous resume"),
                    limits,
                    hooks.control.max_checkpoint_bytes,
                )
            })
            .transpose()?
    } else {
        None
    };
    let (
        mut search,
        mut snapshot,
        mut best,
        mut baseline,
        mut baseline_infeasible,
        mut submitted,
        mut sequence,
        elapsed_before,
        prior_passes,
    ) = match restored {
        Some(state) => (
            state.search,
            state.progress,
            state.best,
            state.baseline,
            state.baseline_infeasible,
            state.submitted,
            state.sequence,
            Duration::from_nanos(state.elapsed_nanos),
            state.detector_passes,
        ),
        None => (
            if mode.resume.is_some() {
                Search::with_rng(spec, true)?
            } else {
                Search::new(spec)?
            },
            StudyProgress {
                phase: StudyPhase::Initial,
                evaluated_candidates: 0,
                scorable_candidates: 0,
                max_trials: spec.max_trials,
                elapsed_seconds: 0.0,
                best_score: None,
            },
            None,
            None,
            false,
            0,
            0,
            Duration::ZERO,
            0,
        ),
    };
    let elapsed = || elapsed_before.saturating_add(start.elapsed());
    let passes = AtomicUsize::new(prior_passes);
    let control = Control {
        cancel,
        deadline: start
            + mode
                .resume
                .as_ref()
                .map_or(Duration::from_secs(spec.max_seconds), |h| {
                    h.control.remaining_budget
                }),
        detector_passes: &passes,
    };
    progress(StudyUpdate {
        progress: snapshot.clone(),
        incumbent: None,
        event: None,
    });
    let stats = if mode.resume.is_some() && control.stopped() {
        None
    } else {
        validate_input(spec, input, limits, cancel)?
    };
    if let Some(context) = continuous {
        crate::continuous::validate_frozen_cutoff(input, context, cancel)?;
    }
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(limits.cpu_threads)
        .stack_size(512 * 1024)
        .build()
        .map_err(|error| StudyError::Execution(error.to_string()))?;
    if let Some(hooks) = &mut mode.resume {
        if hooks.control.resume_checkpoint.is_none() {
            snapshot.elapsed_seconds = elapsed().as_secs_f64();
            let info = ContinuousCheckpointInfo {
                sequence,
                progress: snapshot.clone(),
            };
            let bytes = encode_checkpoint(
                &search,
                spec,
                input,
                continuous.unwrap(),
                limits,
                &info,
                &best,
                &baseline,
                baseline_infeasible,
                submitted,
                elapsed(),
                passes.load(Ordering::Relaxed),
                hooks.control.max_checkpoint_bytes,
            )?;
            let action = (hooks.checkpoint)(&info, &bytes)?;
            if !cancel.is_cancelled()
                && !control.stopped()
                && (action == CheckpointAction::Suspend || hooks.control.pause.is_cancelled())
            {
                return Ok(ContinuousSearchExit::Suspended(info));
            }
        } else if hooks.control.pause.is_cancelled() && !cancel.is_cancelled() && !control.stopped()
        {
            return Ok(ContinuousSearchExit::Suspended(ContinuousCheckpointInfo {
                sequence,
                progress: snapshot.clone(),
            }));
        }
    }
    let reason = 'search: loop {
        if cancel.is_cancelled() {
            break TerminationReason::Cancelled;
        }
        if control.stopped() || stats.is_none() {
            break TerminationReason::TimeBudget;
        }
        if snapshot.evaluated_candidates >= spec.max_trials {
            break TerminationReason::TrialBudget;
        }
        let available = limits
            .batch_size
            .min(spec.max_trials - snapshot.evaluated_candidates);
        let mut batch = Vec::with_capacity(available);
        for _ in 0..available {
            if control.stopped() {
                break;
            }
            match search.next_controlled(|| control.stopped())? {
                Some(proposal) => batch.push(proposal),
                None => break,
            }
        }
        if batch.is_empty() {
            break if cancel.is_cancelled() {
                TerminationReason::Cancelled
            } else if control.stopped() {
                TerminationReason::TimeBudget
            } else {
                TerminationReason::SpaceExhausted
            };
        }
        snapshot.phase = batch[0].phase;
        snapshot.elapsed_seconds = elapsed().as_secs_f64();
        progress(StudyUpdate {
            progress: snapshot.clone(),
            incumbent: None,
            event: None,
        });
        let mut groups: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        for (index, proposal) in batch.iter().enumerate() {
            let key = serde_json::to_string(&proposal.candidate.params)
                .map_err(|error| invalid(error.to_string()))?;
            groups.entry(key).or_default().push(index);
        }
        let groups: Vec<_> = groups.into_values().collect();
        let mut worker_limits = limits.clone();
        worker_limits.max_working_bytes =
            (limits.max_working_bytes - search_bytes) / groups.len().min(limits.cpu_threads).max(1);
        let results: Vec<PacketEvaluation> = pool.install(|| {
            groups
                .par_iter()
                .map(|indexes| {
                    let candidates: Vec<_> = indexes
                        .iter()
                        .map(|&index| &batch[index].candidate)
                        .collect();
                    let evaluated = if let Some(context) = continuous {
                        let mut continuous_limits = worker_limits.clone();
                        let retained_matrices = candidates.len().saturating_sub(1)
                            * spec.tp_levels.len()
                            * spec.sl_levels.len()
                            * std::mem::size_of::<MatrixCell>();
                        continuous_limits.max_working_bytes = worker_limits
                            .max_working_bytes
                            .saturating_sub(retained_matrices);
                        let raw = match crate::continuous::materialize_inner(
                            spec,
                            input,
                            candidates[0],
                            context,
                            &continuous_limits,
                            stats.as_ref().expect("validated stats"),
                            &control,
                        ) {
                            // This raw-set bound is independent of analytics. Only
                            // proposals sharing this detector pass are infeasible;
                            // every other error, and final materialization, stays fatal.
                            Err(StudyError::ResourceLimit { resource, limit, requested })
                                if resource == "setups_per_candidate" => {
                                return Ok(indexes.iter().map(|&index| (
                                    index, TrialEvaluation::SetupLimitExceeded { limit, requested },
                                )).collect());
                            }
                            result => result?,
                        };
                        match raw {
                            None => vec![None; candidates.len()],
                            Some(raw) => {
                                let mut evaluations = Vec::with_capacity(candidates.len());
                                limit(
                                    "continuous_packet_bytes",
                                    candidates.len()
                                        * spec.tp_levels.len()
                                        * spec.sl_levels.len()
                                        * std::mem::size_of::<MatrixCell>(),
                                    worker_limits.max_working_bytes,
                                )?;
                                for candidate in &candidates {
                                    evaluations.push(
                                        crate::continuous::analyze_inner(
                                            spec,
                                            input,
                                            &raw,
                                            candidate,
                                            &continuous_limits,
                                            &control,
                                        )?
                                        .map(|result| result.evaluation),
                                    );
                                }
                                evaluations
                            }
                        }
                    } else {
                        evaluate_group(
                            spec,
                            input,
                            &candidates,
                            &worker_limits,
                            stats.as_ref().expect("validated stats"),
                            &control,
                        )?
                    };
                    Ok(indexes.iter().copied().zip(evaluated).map(|(index, value)| {
                        (index, value.map_or(TrialEvaluation::Interrupted, TrialEvaluation::Evaluated))
                    }).collect())
                })
                .collect()
        });
        let mut completed = Vec::new();
        for result in results {
            completed.extend(result?);
        }
        completed.sort_by_key(|(index, _)| *index);
        let mut interrupted = false;
        for (index, evaluation) in completed {
            let trial = submitted + index;
            let evaluation = match evaluation {
                TrialEvaluation::Interrupted => {
                    interrupted = true;
                    continue;
                }
                TrialEvaluation::SetupLimitExceeded { limit, requested } => {
                    // A completed feasibility decision consumes this proposal once,
                    // without a fabricated matrix/baseline or a scorable record.
                    snapshot.evaluated_candidates += 1;
                    if trial == 0 { baseline_infeasible = true; }
                    search.observe(&batch[index], None, trial);
                    snapshot.elapsed_seconds = elapsed().as_secs_f64();
                    progress(StudyUpdate {
                        progress: snapshot.clone(),
                        incumbent: None,
                        event: Some(StudyEvent {
                            at: chrono::Utc::now().to_rfc3339(),
                            kind: "candidate_infeasible".into(),
                            message: format!("Кандидат trial={trial} непригоден: setups_per_candidate requested={requested}, limit={limit}."),
                        }),
                    });
                    continue;
                }
                TrialEvaluation::Evaluated(evaluation) => evaluation,
            };
            if trial == 0 {
                baseline = Some(evaluation.summary.clone());
            }
            snapshot.evaluated_candidates += 1;
            if evaluation.summary.score.is_some() {
                snapshot.scorable_candidates += 1;
            }
            search.observe(&batch[index], evaluation.summary.score, trial);
            let improves = evaluation
                .summary
                .score
                .is_some_and(|score| (continuous.is_some() || spec.trend_mode.is_mean() || score > spec.min_forecast_expectancy)
                    && best.as_ref().is_none_or(|old| score > old.score));
            let incumbent = if improves {
                if let Some(archive)=mode.manual_minutes.as_deref_mut() {
                    let pair=if spec.forecast.is_some() {
                        select_robust_cell_for_mode(&evaluation.matrix,&spec.tp_levels,&spec.sl_levels,spec.min_closed_per_cell,spec.trend_mode)
                    }else{select_robust_cell(&evaluation.matrix,&spec.tp_levels,&spec.sl_levels,spec.min_closed_per_cell)}?;
                    let captured=if let Some(pair)=pair {
                        let mut promotion_limits=limits.clone();
                        // The evaluator charges its collector itself. Reuse the
                        // temporary allowance already held in search_bytes;
                        // the incumbent reservation remains unavailable to it.
                        promotion_limits.max_working_bytes=limits.max_working_bytes
                            .checked_sub(search_bytes)
                            .and_then(|remaining|remaining.checked_add(manual_archive_bytes))
                            .ok_or_else(||StudyError::Execution("manual promotion working reservation is inconsistent".into()))?;
                        #[cfg(test)]
                        let mut closed=0;
                        #[cfg(test)]
                        let mut observe=|| {
                            closed+=1;
                            if let Some(observer)=mode.manual_capture_observer.as_deref_mut() {
                                observer(trial,closed,control.deadline);
                            }
                        };
                        let Some((verified,minutes))=crate::evaluation::evaluate_manual_with_minutes(
                            spec,input,&batch[index].candidate,ContinuousCaptureCell{tp_pct:pair.tp_pct,sl_pct:pair.sl_pct},
                            evaluation.summary.setup_count,&promotion_limits,stats.as_ref().expect("validated stats"),&control,
                            #[cfg(test)] Some(&mut observe),
                        )? else {interrupted=true;continue;};
                        if verified!=evaluation {return Err(StudyError::Execution("manual incumbent capture differs from completed candidate".into()));}
                        Some(minutes)
                    }else{None};
                    if control.stopped() {interrupted=true;continue;}
                    *archive=captured;
                }
                best = Some(make_best(
                    spec,
                    input,
                    &batch[index].candidate,
                    evaluation,
                    trial,
                    continuous.is_some(),
                    forecast_cutoff,
                ));
                best.clone()
            } else {
                None
            };
            snapshot.best_score = best.as_ref().map(|value| value.score);
            snapshot.elapsed_seconds = elapsed().as_secs_f64();
            progress(StudyUpdate {
                progress: snapshot.clone(),
                incumbent,
                event: None,
            });
        }
        submitted += batch.len();
        if interrupted {
            break 'search if cancel.is_cancelled() {
                TerminationReason::Cancelled
            } else {
                TerminationReason::TimeBudget
            };
        }
        // NO checkpoint after an interrupted/partially observed batch: Search
        // has already removed pending proposals/inserted seen identities.
        if let Some(hooks) = &mut mode.resume {
            sequence += 1;
            snapshot.elapsed_seconds = elapsed().as_secs_f64();
            let info = ContinuousCheckpointInfo {
                sequence,
                progress: snapshot.clone(),
            };
            let bytes = encode_checkpoint(
                &search,
                spec,
                input,
                continuous.unwrap(),
                limits,
                &info,
                &best,
                &baseline,
                baseline_infeasible,
                submitted,
                elapsed(),
                passes.load(Ordering::Relaxed),
                hooks.control.max_checkpoint_bytes,
            )?;
            let action = (hooks.checkpoint)(&info, &bytes)?;
            if !cancel.is_cancelled()
                && !control.stopped()
                && (action == CheckpointAction::Suspend || hooks.control.pause.is_cancelled())
            {
                return Ok(ContinuousSearchExit::Suspended(info));
            }
        }
    };
    snapshot.phase = StudyPhase::Cleanup;
    snapshot.elapsed_seconds = elapsed().as_secs_f64();
    if let Some(best) = &mut best {
        best.partial = reason == TerminationReason::Cancelled;
        best.conclusions = search.conclusions(snapshot.evaluated_candidates);
        let threshold =
            if spec.min_setups_per_day != 0.0 && !(1e-6..1e9).contains(&spec.min_setups_per_day) {
                format!("{:.6e}", spec.min_setups_per_day)
            } else {
                spec.min_setups_per_day.to_string()
            };
        best.conclusions.insert(1, format!(
            "Допущено после фильтров {} сетапов за {:.6} суток: {:.6} сетапов/сутки; порог {}, требуется не менее {} сетапов.",
            best.support.selected_setup_count, (spec.to_unix - spec.from_unix) as f64 / 86400.0,
            best.support.avg_setups_per_day.expect("new completed summary has frequency"), threshold,
            best.support.required_setup_count.expect("a qualified winner has a representable required count")
        ));
        best.conclusions.truncate(6);
    }
    let event = (search.invalid > 0).then(|| StudyEvent {
        at: chrono::Utc::now().to_rfc3339(),
        kind: "invalid_combinations".into(),
        message: format!(
            "Пропущено {} предложений с некорректными сочетаниями параметров.",
            search.invalid
        ),
    });
    progress(StudyUpdate {
        progress: snapshot.clone(),
        incumbent: None,
        event,
    });
    Ok(ContinuousSearchExit::Completed(StudyOutcome {
        progress: snapshot,
        best,
        reason,
        baseline,
        invalid_proposals: search.invalid,
        detector_passes: passes.load(Ordering::Relaxed),
    }))
}

fn checkpoint_error(message: impl std::fmt::Display) -> StudyError {
    StudyError::InvalidInput(format!("continuous checkpoint: {message}"))
}
fn is_false(value: &bool) -> bool { !value }

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Checkpoint<S> {
    version: u32,
    optimizer_version: String,
    rng_version: String,
    word_bits: u32,
    request_hash: String,
    input_hash: String,
    captured_at: String,
    observation_cutoff: i64,
    execution_limits: [usize; 6],
    sequence: u64,
    submitted: usize,
    progress: StudyProgress,
    best: Option<StudyBest>,
    baseline: Option<TrialSummary>,
    #[serde(default, skip_serializing_if = "is_false")]
    baseline_infeasible: bool,
    detector_passes: usize,
    elapsed_nanos: u64,
    search: S,
}
fn execution_limits(limits: &StudyLimits) -> [usize; 6] {
    [
        limits.cpu_threads,
        limits.batch_size,
        limits.max_snapshot_bytes,
        limits.max_working_bytes,
        limits.max_setups_per_candidate,
        limits.max_domain_values,
    ]
}
fn request_hash(spec: &StudyRequest) -> Result<String, StudyError> {
    use sha2::{Digest, Sha256};
    let bytes = serde_json::to_vec(spec).map_err(checkpoint_error)?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}
fn checkpoint_cap(limits: &StudyLimits, requested: usize) -> usize {
    // Reserve room for decoded collections, encoding, validation and incumbent
    // copies. The frozen input is under its SEPARATE max_snapshot_bytes budget.
    requested.min(limits.max_working_bytes / 32)
}
#[allow(clippy::too_many_arguments)]
fn encode_checkpoint(
    search: &Search,
    spec: &StudyRequest,
    input: &StudyInput,
    context: ContinuousContext,
    limits: &StudyLimits,
    info: &ContinuousCheckpointInfo,
    best: &Option<StudyBest>,
    baseline: &Option<TrialSummary>,
    baseline_infeasible: bool,
    submitted: usize,
    elapsed: Duration,
    detector_passes: usize,
    max_bytes: usize,
) -> Result<Vec<u8>, StudyError> {
    let checkpoint = Checkpoint {
        version: crate::checkpoint::FORMAT_VERSION,
        optimizer_version: CONTINUOUS_OPTIMIZER_VERSION.into(),
        rng_version: crate::checkpoint::RNG_VERSION.into(),
        word_bits: usize::BITS,
        request_hash: request_hash(spec)?,
        input_hash: input.snapshot_hash.clone(),
        captured_at: input.captured_at.clone(),
        observation_cutoff: context.observation_cutoff,
        execution_limits: execution_limits(limits),
        sequence: info.sequence,
        submitted,
        progress: info.progress.clone(),
        best: best.clone(),
        baseline: baseline.clone(),
        baseline_infeasible,
        detector_passes,
        elapsed_nanos: u64::try_from(elapsed.as_nanos()).map_err(checkpoint_error)?,
        search,
    };
    let mut writer = BoundedBytes {
        bytes: Vec::new(),
        limit: checkpoint_cap(limits, max_bytes),
    };
    ciborium::ser::into_writer(&checkpoint, &mut writer).map_err(checkpoint_error)?;
    Ok(writer.bytes)
}

fn decode_checkpoint(
    bytes: &[u8],
    spec: &StudyRequest,
    input: &StudyInput,
    context: ContinuousContext,
    limits: &StudyLimits,
    max_bytes: usize,
) -> Result<Checkpoint<Search>, StudyError> {
    limit(
        "checkpoint_bytes",
        bytes.len(),
        checkpoint_cap(limits, max_bytes),
    )?;
    let mut reader = std::io::Cursor::new(bytes);
    let state: Checkpoint<Search> =
        ciborium::de::from_reader(&mut reader).map_err(checkpoint_error)?;
    if reader.position() != bytes.len() as u64 {
        return Err(checkpoint_error("trailing checkpoint bytes"));
    }
    if state.version != crate::checkpoint::FORMAT_VERSION
        || state.optimizer_version != CONTINUOUS_OPTIMIZER_VERSION
        || state.rng_version != crate::checkpoint::RNG_VERSION
        || state.word_bits != usize::BITS
        || state.execution_limits != execution_limits(limits)
        || state.observation_cutoff != context.observation_cutoff
        || state.request_hash != request_hash(spec)?
        || state.input_hash != input.snapshot_hash
        || state.captured_at != input.captured_at
    {
        return Err(checkpoint_error(
            "incompatible version/request/input/context/limits",
        ));
    }
    let expected = Search::with_rng(spec, true)?;
    let search = &state.search;
    let dimensions = search.dimensions.len();
    if search.baseline != expected.baseline
        || search.baseline_coordinates != expected.baseline_coordinates
        || search.dimensions != expected.dimensions
        || search.initial != expected.initial
        || search.max_attempts != expected.max_attempts
        || search.max_trials != spec.max_trials
        || search.algorithm_id != spec.algorithm_id
        || search.initial_index > search.initial.len()
        || search.attempts > search.max_attempts
        || search.invalid > search.attempts
        || search.sensitivity_step > search.attempts
        || search.refinement_step > search.attempts
    {
        return Err(checkpoint_error("invalid frontier/configuration/cursors"));
    }
    let valid_coords = |coords: &[usize]| {
        coords.len() == dimensions
            && coords
                .iter()
                .zip(&search.dimensions)
                .all(|(&i, d)| i < d.values.len())
    };
    if !valid_coords(&search.enumeration)
        || search
            .baseline_coordinates
            .as_ref()
            .is_some_and(|c| !valid_coords(c))
        || search.initial.iter().any(|c| !valid_coords(c))
    {
        return Err(checkpoint_error("invalid coordinate dimensions/index"));
    }
    if state.submitted > spec.max_trials
        || state.progress.evaluated_candidates != state.submitted
        || search.seen.len() != state.submitted
        || state.progress.scorable_candidates > state.submitted
        || state.progress.max_trials != spec.max_trials
        || !state.progress.elapsed_seconds.is_finite()
        || state.progress.elapsed_seconds < 0.0
        || state.progress.best_score.is_some_and(|v| !v.is_finite())
        || state.sequence > state.submitted as u64
        || (state.submitted == 0) != (state.sequence == 0)
        || search.baseline_sent != (state.submitted > 0)
        || (if state.submitted == 0 {
            state.baseline.is_some() || state.baseline_infeasible
        } else { state.baseline.is_some() == state.baseline_infeasible })
        || search.records.len()
            != state
                .submitted
                .saturating_sub(usize::from(search.baseline_coordinates.is_none()))
        || search.pending.len() > spec.max_trials.saturating_mul(2).saturating_add(2)
        || search.influence.len() != dimensions
    {
        return Err(checkpoint_error("inconsistent committed batch counters"));
    }
    if !matches!(
        state.progress.phase,
        StudyPhase::Initial | StudyPhase::Sensitivity | StudyPhase::Refinement
    ) {
        return Err(checkpoint_error("non-search phase"));
    }
    let mut previous = None;
    for record in &search.records {
        if record.trial >= state.submitted
            || previous.is_some_and(|old| old >= record.trial)
            || !valid_coords(&record.coordinates)
            || record.score.is_some_and(|v| !v.is_finite())
        {
            return Err(checkpoint_error("invalid record/index/score"));
        }
        let proposal = search.build(record.coordinates.clone(), StudyPhase::Initial, None)?;
        if !search
            .seen
            .contains(&canonical_candidate_key(&proposal.candidate)?)
        {
            return Err(checkpoint_error("observed record absent from seen"));
        }
        previous = Some(record.trial);
    }
    for proposal in &search.pending {
        let Some(coords) = &proposal.coordinates else {
            return Err(checkpoint_error("pending coordinates absent"));
        };
        if !valid_coords(coords)
            || proposal.comparison.is_some_and(|(trial, dim, score)| {
                trial >= state.submitted
                    || dim >= dimensions
                    || score.is_some_and(|v| !v.is_finite())
            })
        {
            return Err(checkpoint_error("invalid pending coordinates/comparison"));
        }
        if search
            .build(coords.clone(), proposal.phase, proposal.comparison)?
            .candidate
            != proposal.candidate
        {
            return Err(checkpoint_error(
                "pending candidate differs from coordinates",
            ));
        }
    }
    for effect in &search.influence {
        if !effect.total_difference.is_finite()
            || effect.total_difference < 0.0
            || effect.comparisons > state.submitted
            || effect.contexts.len() > effect.comparisons
            || effect.contexts.iter().any(|&i| i >= state.submitted)
        {
            return Err(checkpoint_error("invalid influence"));
        }
    }
    let finite_summary = |summary: &TrialSummary| {
        summary.score.is_none_or(f64::is_finite)
            && summary
                .avg_setups_per_day
                .is_none_or(|v| v.is_finite() && v >= 0.0)
    };
    if state.baseline_infeasible {
        let record = search.records.iter().find(|record| record.trial == 0);
        // Out-of-domain baselines have never had coordinate records. Preserve
        // that layout; their existing sent/seen identity still proves trial0.
        let record_valid = match &search.baseline_coordinates {
            Some(coordinates) => record.is_some_and(|r| r.score.is_none() && &r.coordinates == coordinates),
            None => record.is_none(),
        };
        if !record_valid || state.best.as_ref().is_some_and(|best| best.trial_index == 0)
            || !search.seen.contains(&canonical_candidate_key(&search.baseline)?)
        {
            return Err(checkpoint_error("inconsistent infeasible baseline marker"));
        }
    }
    if state.baseline.as_ref().is_some_and(|b| !finite_summary(b)) {
        return Err(checkpoint_error("invalid baseline"));
    }
    let scorable = search.records.iter().filter(|r| r.score.is_some()).count()
        + usize::from(
            search.baseline_coordinates.is_none()
                && state.baseline.as_ref().is_some_and(|b| b.score.is_some()),
        );
    if scorable != state.progress.scorable_candidates
        || state.best.is_some() != (scorable > 0)
        || state.best.as_ref().map(|b| b.score.to_bits())
            != state.progress.best_score.map(f64::to_bits)
    {
        return Err(checkpoint_error("incumbent/scorable mismatch"));
    }
    if let Some(best) = &state.best {
        let provenance = &best.provenance;
        if !best.score.is_finite()
            || best.trend_mode != spec.trend_mode
            || best.forecast != spec.forecast
            || best.observation_cutoff != spec.forecast.map(|_| context.observation_cutoff)
            || (!spec.trend_mode.is_mean() && best.trend_bend_days != crate::trend::effective_bend(spec))
            || (spec.trend_mode == TrendMode::Blend && best.trend_blend_lambda != spec.trend_blend_lambda)
            || best.mean_score != (!spec.trend_mode.is_mean()).then(|| score_period_mean(&best.matrix, spec.min_closed_per_cell)).flatten()
            || best.trial_index >= state.submitted
            || !finite_summary(&best.support)
            || best.support.score != Some(best.score)
            || best.matrix.len() != spec.tp_levels.len() * spec.sl_levels.len()
            || best.conclusions.len() > 6
            || provenance.optimizer_version != CONTINUOUS_OPTIMIZER_VERSION
            || provenance.seed != spec.seed
            || provenance.snapshot_hash != input.snapshot_hash
            || provenance.captured_at != input.captured_at
            || best
                .analytics
                .price_change_pct
                .is_some_and(|v| !v.is_finite())
            || best
                .analytics
                .setup_maturity
                .is_some_and(|v| !v.is_finite())
            || best.matrix.iter().any(|cell| {
                !cell.tp_pct.is_finite()
                    || !cell.sl_pct.is_finite()
                    || cell.risk_expectancy.is_some_and(|v| !v.is_finite())
                    || cell.avg_move_pct.is_some_and(|v| !v.is_finite())
                    || cell.mean_risk_expectancy.is_some_and(|v| !v.is_finite())
                    || cell.forecast_risk_expectancy.is_some_and(|v| !v.is_finite())
                    || (if spec.forecast.is_some() {
                        (!spec.trend_mode.is_mean() != cell.forecast_risk_expectancy.is_some())
                            || (spec.trend_mode == TrendMode::Forecast && cell.risk_expectancy != cell.forecast_risk_expectancy)
                    } else { (spec.trend_mode == TrendMode::Blend) != cell.forecast_risk_expectancy.is_some() })
                    || (spec.trend_mode == TrendMode::Blend && cell.risk_expectancy != cell.mean_risk_expectancy.zip(cell.forecast_risk_expectancy).map(|(mean,forecast)|spec.trend_blend_lambda.combine(mean,forecast)))
                    || (spec.trend_mode.is_mean() == cell.mean_risk_expectancy.is_some())
            })
            || score_matrix(&best.matrix, spec.min_closed_per_cell) != Some(best.score)
        {
            return Err(checkpoint_error("invalid incumbent/matrix/provenance"));
        }
        parse_params(&spec.algorithm_id, &best.params).map_err(checkpoint_error)?;
    }
    Ok(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::{SeedableRng, rngs::StdRng};
    use serde_json::json;

    fn manual_fixture() -> (StudyRequest,StudyInput) {
        const T0:i64=1_699_999_980;
        let minute=|n:i64|T0+n*60;
        let params=setups_domain::SpikeParams{baseline_long_days:1,baseline_short_days:1,..Default::default()};
        let spec:StudyRequest=serde_json::from_value(json!({
            "algorithmId":"price_volume_spike","algorithmVersion":"1.1.1","algorithmParams":params,
            "analyticsParams":{"clusterEnabled":false,"clusterGapMinutes":480,"timeShiftMinutes":0,"priceChangePct":null},
            "analyticsSearch":{"clusterEnabled":[true]},
            "instruments":[{"exchange":"bybit","category":"linear","symbol":"FIXTUREUSDT"}],
            "fromUnix":minute(1440),"toUnix":minute(1500),"direction":"long","seed":42,
            "tpLevels":[20.0],"slLevels":[1.0],"horizonMinutes":30,"minClosedPerCell":1,
            "minSetupsPerDay":0.0,"maxTrials":2,"maxSeconds":30
        })).unwrap();
        let mut bars=Vec::new();let mut candles=Vec::new();
        for n in 0i64..1560 {
            let exponent=((n.min(1464)-1434)/5).max(0) as i32;
            let close=format!("{:.8}",100.0*1.13f64.powi(exponent));
            let price=close.parse::<f64>().unwrap();
            bars.push(setups_domain::DetectorBar{open_time_unix:minute(n),close,
                quote_turnover:if (1435..1465).contains(&n) {"3000"} else {"10"}.into()});
            candles.push(setups_analytics::OhlcCandle{open_time_unix:minute(n),open:price,close:price,
                high:price*1.25,low:if n==1465 {price*0.5} else {price}});
        }
        let input=StudyInput{instruments:vec![InstrumentInput{instrument:spec.instruments[0].clone(),detector_bars:bars,candles}],
            snapshot_hash:"manual-promotion-fixture".into(),captured_at:"2026-11-21T00:00:00Z".into()};
        (spec,input)
    }

    #[test]
    fn manual_newer_promotion_interruption_keeps_complete_incumbent_and_archive() {
        let (spec,input)=manual_fixture();
        let limits=StudyLimits{cpu_threads:1,batch_size:1,max_setups_per_candidate:6,..Default::default()};
        let mut baseline_spec=spec.clone();baseline_spec.max_trials=1;
        let (baseline,baseline_archive)=run_study_with_original_minutes(&baseline_spec,&input,&limits,&CancelToken::new(),|_|{}).unwrap();
        let baseline=baseline.best.unwrap();let baseline_archive=baseline_archive.unwrap().minutes;
        let (complete,_)=run_study_with_original_minutes(&spec,&input,&limits,&CancelToken::new(),|_|{}).unwrap();
        let legacy=run_study(&spec,&input,&limits,&CancelToken::new(),|_|{}).unwrap();
        assert_eq!(complete.best,legacy.best,"completed multi-promotion science is unchanged");
        assert_eq!(complete.baseline,legacy.baseline);assert_eq!(complete.reason,legacy.reason);
        assert!(complete.best.as_ref().unwrap().score>baseline.score,"fixture must really promote a newer candidate");
        assert_eq!(complete.best.unwrap().trial_index,1);
        for timeout in [false,true] {
            let mut request=spec.clone();if timeout {request.max_seconds=1;}
            let token=CancelToken::new();let mut archive=None;let mut reached=None;let mut promotions=Vec::new();
            let mut observer=|trial,closed,deadline:Instant| {
                if trial==1 && closed==1 {
                    reached=Some((trial,closed));
                    if timeout {std::thread::sleep(deadline.saturating_duration_since(Instant::now()));}
                    else {token.cancel();}
                }
            };
            let exit=run_engine(&request,&input,&limits,&token,|update| {
                if let Some(best)=update.incumbent {promotions.push(best.trial_index);}
            },EngineMode{continuous:None,resume:None,manual_minutes:Some(&mut archive),manual_capture_observer:Some(&mut observer)}).unwrap();
            let ContinuousSearchExit::Completed(outcome)=exit else {panic!("manual study cannot suspend");};
            assert_eq!(reached,Some((1,1)),"interruption is inside the real newer capture");
            assert_eq!(outcome.reason,if timeout {TerminationReason::TimeBudget} else {TerminationReason::Cancelled});
            assert_eq!(promotions,vec![0],"incomplete newer candidate must never be published");
            let best=outcome.best.unwrap();assert_eq!(best.trial_index,baseline.trial_index);
            assert_eq!(best.score,baseline.score);assert_eq!(best.params,baseline.params);
            assert_eq!(best.analytics,baseline.analytics);assert_eq!(best.matrix,baseline.matrix);
            assert_eq!(archive.unwrap(),baseline_archive,"prior ledger survives byte-for-byte");
            assert_eq!(outcome.detector_passes,4,"two evaluations and two captures, no post-stop pass");
        }
    }

    #[test]
    fn manual_archive_point_and_byte_boundaries_and_empty_pair_are_explicit() {
        use crate::continuous::SelectedMinuteCapture;
        let (spec,_)=manual_fixture();let pair=ContinuousCaptureCell{tp_pct:20.0,sl_pct:1.0};
        let bytes=SelectedMinuteCapture::budget_bytes(6);
        let mut limits=StudyLimits{max_setups_per_candidate:6,max_working_bytes:bytes-1,..Default::default()};
        assert_eq!(SelectedMinuteCapture::manual(&spec,pair,6,&limits).err(),Some(StudyError::ResourceLimit{
            resource:"manual_minute_working_bytes".into(),limit:bytes-1,requested:bytes}));
        limits.max_working_bytes=bytes;
        assert!(SelectedMinuteCapture::manual(&spec,pair,6,&limits).is_ok());
        assert!(matches!(SelectedMinuteCapture::manual(&spec,pair,7,&limits),Err(StudyError::ResourceLimit{resource,limit:6,requested:7}) if resource=="selected_minute_points"));
        let unlimited=StudyLimits{max_setups_per_candidate:100_001,..Default::default()};
        assert!(matches!(SelectedMinuteCapture::manual(&spec,pair,100_001,&unlimited),Err(StudyError::ResourceLimit{resource,limit:100_000,requested:100_001}) if resource=="selected_minute_points"));
        let token=CancelToken::new();let passes=AtomicUsize::new(0);
        let control=Control{cancel:&token,deadline:Instant::now()+Duration::from_secs(30),detector_passes:&passes};
        let mut matrix=crate::evaluation::empty_matrix(&spec);
        let empty=SelectedMinuteCapture::manual(&spec,pair,0,&limits).unwrap().finish(&matrix,&control).unwrap().unwrap();
        assert!(empty.points.is_empty(),"selected-but-empty capture is Some, not interrupted None");
        assert_eq!(empty.evaluator,MANUAL_SELECTED_MINUTES_EVALUATOR);
        assert!(select_robust_cell(&matrix,&spec.tp_levels,&spec.sl_levels,1).unwrap().is_none(),"existing selector cannot admit an empty winner");
        let mut capture=SelectedMinuteCapture::manual(&spec,pair,6,&limits).unwrap();
        for n in 0..6 {matrix[0].tp_count+=1;capture.record(spec.from_unix,(n,0),&matrix[0]).unwrap();}
        matrix[0].closed_count=6;
        assert!(matches!(capture.record(spec.from_unix,(5,0),&matrix[0]),Err(StudyError::ResourceLimit{limit:6,requested:7,..})));
        let archive=capture.finish(&matrix,&control).unwrap().unwrap();
        assert_eq!(archive.points.len(),1);assert_eq!(archive.points.capacity(),6);
        assert_eq!((archive.points[0].closed_count,archive.points[0].tp_count,archive.points[0].sum_r),(6,6,120.0));
        assert!(serde_json::to_vec(&archive).unwrap().len()<=256+6*256,"encoding stays within reserved output bytes");
    }

    #[test]
    fn manual_search_reserves_both_archives_and_returns_an_owned_frozen_snapshot() {
        use crate::continuous::SelectedMinuteCapture;
        let (mut spec,mut input)=manual_fixture();spec.max_trials=1;
        let limits=StudyLimits{cpu_threads:1,batch_size:1,max_setups_per_candidate:6,max_working_bytes:1,..Default::default()};
        let requested=|error:StudyError|match error {StudyError::ResourceLimit{resource,requested,..}=>{assert_eq!(resource,"search_working_bytes");requested},error=>panic!("unexpected {error:?}")};
        let old=requested(run_study(&spec,&input,&limits,&CancelToken::new(),|_|{}).unwrap_err());
        let new=requested(run_study_with_original_minutes(&spec,&input,&limits,&CancelToken::new(),|_|{}).unwrap_err());
        assert_eq!(new-old,2*SelectedMinuteCapture::budget_bytes(6));
        let tight=StudyLimits{max_working_bytes:new,..limits.clone()};
        assert!(matches!(run_study_with_original_minutes(&spec,&input,&tight,&CancelToken::new(),|_|{}),Err(StudyError::ResourceLimit{resource,..}) if resource=="candidate_working_bytes"));
        let limits=StudyLimits{max_working_bytes:128*1024*1024,..limits};
        let before=format!("{input:?}");
        let (outcome,archive)=run_study_with_original_minutes(&spec,&input,&limits,&CancelToken::new(),|_|{}).unwrap();
        assert_eq!(format!("{input:?}"),before);
        let archive=archive.unwrap();let saved=archive.clone();
        assert!(archive.observation_cutoff>spec.to_unix,"manual observation retains the post-signal full horizon");
        assert_eq!(archive.observation_cutoff,required_input_bounds(&spec).unwrap().candles_to_unix);
        assert!(!serde_json::to_string(&outcome).unwrap().contains("original-manual-close-1"));
        drop(outcome);input.instruments.clear();drop(input);drop(spec);
        assert_eq!(archive,saved,"archive owns only scalar values and outlives all search/input owners");
    }

    #[test]
    fn manual_zero_closed_matrix_cannot_manufacture_a_winner_or_archive() {
        let (mut spec,input)=manual_fixture();spec.max_trials=1;
        // With zero shift the unchanged positive gate rejects the zero move.
        spec.analytics_params.price_change_pct=Some(1.0);
        let limits=StudyLimits{cpu_threads:1,batch_size:1,max_setups_per_candidate:6,..Default::default()};
        let candidate=StudyCandidate{params:spec.algorithm_params.clone(),analytics:spec.analytics_params.clone()};
        let evaluated=crate::evaluate_candidate(&spec,&input,&candidate,&limits,&CancelToken::new()).unwrap().unwrap();
        assert_eq!(evaluated.matrix[0].closed_count,0);
        assert_eq!(evaluated.matrix[0].price_change_filtered_count,6);
        assert!(select_robust_cell(&evaluated.matrix,&spec.tp_levels,&spec.sl_levels,spec.min_closed_per_cell).unwrap().is_none());
        let (outcome,archive)=run_study_with_original_minutes(&spec,&input,&limits,&CancelToken::new(),|_|{}).unwrap();
        assert!(outcome.best.is_none()&&archive.is_none());
        assert_eq!(outcome.baseline,Some(evaluated.summary));
    }

    #[test]
    fn manual_promotion_charges_the_reserved_temporary_archive_only_once() {
        use crate::continuous::SelectedMinuteCapture;
        let (spec,input)=manual_fixture();
        let limits=StudyLimits{cpu_threads:1,batch_size:1,max_setups_per_candidate:6,..Default::default()};
        let candidate=StudyCandidate{params:spec.algorithm_params.clone(),analytics:spec.analytics_params.clone()};
        let token=CancelToken::new();
        let requested=|error:StudyError,expected:&str|match error {
            StudyError::ResourceLimit{resource,requested,..}=>{assert_eq!(resource,expected);requested},
            error=>panic!("unexpected {error:?}"),
        };
        // Measure the unchanged standalone evaluator's real admission boundary
        // from its base and first setup. All six fixture setups have equal size;
        // exact success and one-byte-below failure below independently check T.
        let tiny=StudyLimits{max_working_bytes:1,..limits.clone()};
        let base=requested(crate::evaluate_candidate(&spec,&input,&candidate,&tiny,&token).unwrap_err(),"candidate_working_bytes");
        let base_limit=StudyLimits{max_working_bytes:base,..limits.clone()};
        let first=requested(crate::evaluate_candidate(&spec,&input,&candidate,&base_limit,&token).unwrap_err(),"candidate_working_bytes");
        let evaluator=base+(first-base)*6;
        let exact=StudyLimits{max_working_bytes:evaluator,..limits.clone()};
        let evaluated=crate::evaluate_candidate(&spec,&input,&candidate,&exact,&token).unwrap().unwrap();
        assert_eq!(evaluated.summary.setup_count,6);
        let below=StudyLimits{max_working_bytes:evaluator-1,..limits.clone()};
        assert_eq!(crate::evaluate_candidate(&spec,&input,&candidate,&below,&token).unwrap_err(),StudyError::ResourceLimit{
            resource:"candidate_working_bytes".into(),limit:evaluator-1,requested:evaluator});
        let search=requested(run_study(&spec,&input,&tiny,&token,|_|{}).unwrap_err(),"search_working_bytes");
        let archive=SelectedMinuteCapture::budget_bytes(6);
        let needed=search+2*archive+evaluator;
        let cap=StudyLimits{max_working_bytes:needed,..limits.clone()};
        let mut promotions=Vec::new();
        let result=run_study_with_original_minutes(&spec,&input,&cap,&token,|update| {
            if let Some(best)=update.incumbent {promotions.push(best.trial_index);}
        });
        assert!(result.is_ok(),"S0={search}, A={archive}, T={evaluator}, global={needed} fits two archives, not three: {result:?}");
        let (outcome,ledger)=result.unwrap();
        assert_eq!(promotions,vec![0,1],"the boundary must allow real replacement of an existing incumbent");
        assert_eq!(outcome.detector_passes,4);
        let legacy=run_study(&spec,&input,&limits,&token,|_|{}).unwrap();
        assert_eq!(outcome.best,legacy.best);assert_eq!(outcome.baseline,legacy.baseline);
        let ledger=ledger.unwrap().minutes;let best=outcome.best.unwrap();
        assert_eq!(ledger.points.iter().map(|point|point.closed_count).sum::<u64>(),best.matrix[0].closed_count as u64);
        let below=StudyLimits{max_working_bytes:needed-1,..limits};
        assert_eq!(run_study_with_original_minutes(&spec,&input,&below,&token,|_|{}).unwrap_err(),StudyError::ResourceLimit{
            resource:"candidate_working_bytes".into(),limit:evaluator-1,requested:evaluator});
        eprintln!("manual promotion boundary: S0={search}, A={archive}, T={evaluator}, accepted={needed}, rejected={}",needed-1);
    }

    fn toy_spec() -> StudyRequest {
        serde_json::from_value(json!({
            "algorithmId":"price_volume_spike", "algorithmVersion":"1.1.1",
            "algorithmParams":setups_domain::SpikeParams::default(), "analyticsParams":AnalyticsParams::default(),
            "instruments":[{"exchange":"bybit","category":"linear","symbol":"BTCUSDT"}],
            "fromUnix":120,"toUnix":180,"direction":"short","seed":42,"maxTrials":128,
            "algorithmSearch":{"priceGrowthPct":["4","6","8","10","12","14","16","18","20"],
                "windowMinutes":[1,2,3,4,5,6,7,8,9],"cooldownMinutes":[1,2,3,4,5,6,7,8,9]}
        })).unwrap()
    }

    #[test]
    fn setup_cap_is_inclusive_at_ten_thousand() {
        assert_eq!(limit("setups_per_candidate", 10_000, 10_000), Ok(()));
        assert_eq!(
            limit("setups_per_candidate", 10_001, 10_000),
            Err(StudyError::ResourceLimit {
                resource: "setups_per_candidate".into(),
                limit: 10_000,
                requested: 10_001,
            })
        );
    }

    #[test]
    fn in_domain_baseline_seeds_contexts_and_has_a_refinable_better_neighbor() {
        let spec = toy_spec();
        let mut search = Search::new(&spec).unwrap();
        let baseline = search.next().unwrap().unwrap();
        let coordinates = baseline
            .coordinates
            .clone()
            .expect("baseline lies in every searched domain");
        search.observe(&baseline, Some(10.0), 0);
        assert_eq!(search.records[0].coordinates, coordinates);
        assert_eq!(search.contexts(), vec![0]);
        search.initial_index = search.initial.len();
        let dim = search
            .dimensions
            .iter()
            .position(|d| d.key == "priceGrowthPct")
            .unwrap();
        search.sensitivity_step = dim;
        let next = search.root().unwrap().unwrap();
        assert_eq!(next.comparison, Some((0, dim, Some(10.0))));
        assert_ne!(next.coordinates.as_ref().unwrap()[dim], coordinates[dim]);
        search.observe(&next, Some(11.0), 1);
        assert_eq!(search.records[search.contexts()[0]].trial, 1);
        let mut outside = spec;
        outside
            .algorithm_params
            .insert("priceGrowthPct".into(), json!("21"));
        assert!(
            Search::new(&outside)
                .unwrap()
                .baseline_coordinates
                .is_none()
        );
    }

    #[test]
    fn multi_context_sensitivity_and_all_refinement_kinds_remain_available() {
        let spec = toy_spec();
        let mut search = Search::new(&spec).unwrap();
        for (trial, coords) in [vec![0, 0, 0], vec![4, 4, 4], vec![8, 8, 8]]
            .into_iter()
            .enumerate()
        {
            let proposal = search.build(coords, StudyPhase::Initial, None).unwrap();
            search.observe(&proposal, Some(trial as f64), trial);
        }
        search.baseline_sent = true;
        search.initial_index = search.initial.len();
        let mut compared = BTreeSet::new();
        for _ in 0..18 {
            let proposal = search.root().unwrap().unwrap();
            if let Some((context, dim, _)) = proposal.comparison {
                compared.insert((context, dim));
            }
        }
        assert!(compared.len() >= search.dimensions.len() * 3);
        search.sensitivity_step = usize::MAX;
        let base = search.records[search.contexts()[0]].coordinates.clone();
        let mut changed = Vec::new();
        for _ in 0..4 {
            let proposal = search.root().unwrap().unwrap();
            changed.push(
                proposal
                    .coordinates
                    .unwrap()
                    .iter()
                    .zip(&base)
                    .filter(|(a, b)| a != b)
                    .count(),
            );
        }
        assert_eq!(&changed[..3], &[1, 2, 3]); // fourth is continuing global exploration
    }

    #[test]
    fn toy_landscape_equal_budget_comparison_against_seeded_random() {
        let mut guided_total = 0.0;
        let mut random_total = 0.0;
        for seed in 0..8 {
            let mut spec = toy_spec();
            spec.seed = seed;
            let mut search = Search::new(&spec).unwrap();
            let objective = |coords: &[usize]| {
                let target = [6.0, 2.0, 5.0];
                -coords
                    .iter()
                    .zip(target)
                    .map(|(&x, t)| (x as f64 - t).powi(2))
                    .sum::<f64>()
            };
            let mut best = f64::NEG_INFINITY;
            for trial in 0..spec.max_trials {
                let proposal = search.next().unwrap().unwrap();
                let value = objective(proposal.coordinates.as_ref().unwrap());
                search.observe(&proposal, Some(value), trial);
                best = best.max(value);
            }
            guided_total += best;
            let mut random = StdRng::seed_from_u64(seed);
            let mut random_best = objective(search.baseline_coordinates.as_ref().unwrap());
            for _ in 1..spec.max_trials {
                random_best = random_best.max(objective(&[
                    random.random_range(0..9),
                    random.random_range(0..9),
                    random.random_range(0..9),
                ]));
            }
            random_total += random_best;
        }
        eprintln!(
            "8-seed sphere landscape, 128 trials: Plan A total={guided_total}, random total={random_total}; optimum=0"
        );
        assert!(
            guided_total >= random_total,
            "bounded heuristic should beat this fixed random reference, not a universal guarantee"
        );
        assert!(guided_total >= -8.0);
    }
}
