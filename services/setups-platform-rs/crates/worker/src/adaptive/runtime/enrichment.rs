//! Scalar copies belong to one NEW setup, never to a research result or an old
//! outbox job. Daily series stay in the bounded research artifact, not live RAM.
use super::*;
use setups_domain::expectancy::{
    EXPECTANCY_CALCULATION_VERSION, ExpectancyArtifact, ExpectancyImageKey, ExpectancyTimezone,
    ExpectancyUnavailableReason, MetricAvailability,
};

pub(super) const MAX_EXPECTANCY_SNAPSHOT_BYTES: usize = 8 * 1024;
const IMAGE_RENDER_VERSION: u32 = 1;

/// Actual historical input is separate from the immutable research reference.
/// Publication time, not the old market cutoff, bounds eligible raw signals.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HistoricalExpectancySnapshot {
    pub original_artifact_id: i64,
    pub revision_key: String,
    pub available_from_unix: i64,
    pub provenance: setups_db::expectancy::HistoricalRecalculationProvenance,
}

impl HistoricalExpectancySnapshot {
    fn validate(&self, cutoff: i64) -> Result<(), WorkerError> {
        self.provenance
            .validate()
            .map_err(|_| invalid("invalid historical expectancy provenance"))?;
        if self.original_artifact_id <= 0
            || self.revision_key != self.provenance.input_hash
            || self.available_from_unix < self.provenance.recalculated_at_unix
            || self.provenance.read_to_unix > cutoff
        {
            return Err(invalid(
                "historical expectancy identity or availability mismatch",
            ));
        }
        Ok(())
    }
}

/// Optional `expectancy` field on the saved recommendation. Availability is
/// explicit, unknown counts are null rather than invented zeroes, and imageKey
/// is a study asset identity (never a URL, PNG bytes or daily series).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AdaptiveExpectancySnapshot {
    pub calculation_version: u32,
    pub timezone: ExpectancyTimezone,
    pub study_id: i64,
    pub generation_id: i64,
    pub slot_id: i64,
    pub artifact_id: Option<i64>,
    pub derived_snapshot_id: Option<String>,
    pub source_snapshot_hash: String,
    pub source_cutoff: i64,
    pub from_unix: i64,
    pub to_unix: i64,
    pub period_days: f64,
    pub selected_mean_r: MetricAvailability<f64>,
    pub tp_count: Option<u64>,
    pub sl_count: Option<u64>,
    pub closed_count: Option<u64>,
    pub win_rate_pct: MetricAvailability<f64>,
    pub matrix_mean_r: MetricAvailability<f64>,
    pub trend_delta_r: MetricAvailability<f64>,
    pub forecast_r: MetricAvailability<f64>,
    pub forecast_at_unix: i64,
    pub image_key: Option<ExpectancyImageKey>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub historical_recalculation: Option<HistoricalExpectancySnapshot>,
}

fn unavailable(reason: ExpectancyUnavailableReason) -> MetricAvailability<f64> {
    MetricAvailability::Unavailable { reason }
}

fn period_days(envelope: &RecommendationEnvelope) -> Result<f64, WorkerError> {
    let p = &envelope.snapshot.provenance;
    let seconds = p
        .to_unix
        .checked_sub(p.from_unix)
        .filter(|seconds| *seconds > 0)
        .ok_or_else(|| invalid("invalid recommendation research period"))?;
    Ok(seconds as f64 / 86400.0)
}

impl AdaptiveExpectancySnapshot {
    /// Checks the scalar envelope only. Forecast is frozen once; formatting and
    /// retries must not re-evaluate it using a new artifact, clock or winner.
    pub(super) fn validate_for(
        &self,
        envelope: &RecommendationEnvelope,
        raw: i64,
    ) -> Result<(), WorkerError> {
        let p = &envelope.snapshot.provenance;
        if self.calculation_version != EXPECTANCY_CALCULATION_VERSION
            || self.timezone != ExpectancyTimezone::UTC
            || (self.study_id, self.generation_id, self.slot_id)
                != (envelope.study_id, envelope.generation_id, envelope.slot_id)
            || self.forecast_at_unix != raw
            || self.source_cutoff != p.observation_cutoff
            || self.source_cutoff > raw
            || self.source_snapshot_hash != p.snapshot_hash
            || self.from_unix != p.from_unix
            || self.to_unix != p.to_unix
            || self.period_days != period_days(envelope)?
            || self.selected_mean_r.as_ref().copied() != Some(envelope.snapshot.raw_r)
            || self.matrix_mean_r.as_ref().copied() != Some(envelope.snapshot.mean_r)
            || self
                .artifact_id
                .is_some_and(|id| id <= 0 || self.derived_snapshot_id.is_none())
        {
            return Err(invalid(
                "saved expectancy identity or raw-signal timestamp mismatch",
            ));
        }
        for metric in [
            &self.selected_mean_r,
            &self.matrix_mean_r,
            &self.win_rate_pct,
            &self.trend_delta_r,
            &self.forecast_r,
        ] {
            if metric.as_ref().is_some_and(|value| !value.is_finite()) {
                return Err(invalid("non-finite saved expectancy metric"));
            }
        }
        if self
            .win_rate_pct
            .as_ref()
            .is_some_and(|value| !(0.0..=100.0).contains(value))
        {
            return Err(invalid("saved expectancy win rate outside bounds"));
        }
        match (self.tp_count, self.sl_count, self.closed_count) {
            (Some(tp), Some(sl), Some(closed)) if tp.checked_add(sl) == Some(closed) => {}
            (None, None, None) if self.win_rate_pct.as_ref().is_none() => {}
            _ => return Err(invalid("saved expectancy counts inconsistent")),
        }
        if let Some(historical) = &self.historical_recalculation {
            historical.validate(self.source_cutoff)?;
            if historical.available_from_unix > raw
                || self.artifact_id.is_none()
                || self.artifact_id == Some(historical.original_artifact_id)
                || self.derived_snapshot_id.is_none()
            {
                return Err(invalid(
                    "saved historical expectancy is not eligible for this signal",
                ));
            }
        }
        if let Some(key) = &self.image_key {
            if key.study_id != self.study_id.to_string()
                || Some(&key.derived_snapshot_id) != self.derived_snapshot_id.as_ref()
                || key.input_snapshot_hash != self.source_snapshot_hash
                || key.tp_pct != envelope.snapshot.tp_pct
                || key.sl_pct != envelope.snapshot.sl_pct
                || key.calculation_version != self.calculation_version
                || key.timezone != self.timezone
                || key.render_version != IMAGE_RENDER_VERSION
            {
                return Err(invalid("saved expectancy image identity mismatch"));
            }
        }
        Ok(())
    }
}

/// The shared domain artifact is the sole forecast implementation. A missing
/// artifact is expected legacy data, not an excuse to fabricate outcomes.
pub(super) fn freeze(
    envelope: &RecommendationEnvelope,
    raw_signal_at: i64,
    artifact: Option<&ExpectancyArtifact>,
) -> Result<AdaptiveExpectancySnapshot, WorkerError> {
    let snapshot = &envelope.snapshot;
    let p = &snapshot.provenance;
    if p.observation_cutoff > raw_signal_at {
        return Err(invalid("expectancy source cutoff is after raw signal"));
    }
    let mut saved = AdaptiveExpectancySnapshot {
        calculation_version: EXPECTANCY_CALCULATION_VERSION,
        timezone: ExpectancyTimezone::UTC,
        study_id: envelope.study_id,
        generation_id: envelope.generation_id,
        slot_id: envelope.slot_id,
        artifact_id: None,
        derived_snapshot_id: None,
        source_snapshot_hash: p.snapshot_hash.clone(),
        source_cutoff: p.observation_cutoff,
        from_unix: p.from_unix,
        to_unix: p.to_unix,
        period_days: period_days(envelope)?,
        selected_mean_r: MetricAvailability::Available {
            value: snapshot.raw_r,
        },
        tp_count: None,
        sl_count: None,
        closed_count: None,
        win_rate_pct: unavailable(ExpectancyUnavailableReason::MissingSelectedCell),
        matrix_mean_r: MetricAvailability::Available {
            value: snapshot.mean_r,
        },
        trend_delta_r: unavailable(ExpectancyUnavailableReason::MissingFrozenOutcomes),
        forecast_r: unavailable(ExpectancyUnavailableReason::MissingFrozenOutcomes),
        forecast_at_unix: raw_signal_at,
        image_key: None,
        historical_recalculation: None,
    };
    if let Some(artifact) = artifact {
        let forecast = artifact
            .forecast_at_raw_signal(raw_signal_at)
            .map_err(|error| invalid(&error.to_string()))?;
        let source = &artifact.source;
        if source.study_id != envelope.study_id.to_string()
            || source.generation_id.as_deref() != Some(envelope.generation_id.to_string().as_str())
            || source.profile_id.as_deref() != Some(envelope.slot_id.to_string().as_str())
            || source.provenance != *p
            || source.direction != snapshot.direction
            || source.tp_pct != snapshot.tp_pct
            || source.sl_pct != snapshot.sl_pct
            || source.analytics.cluster_enabled != snapshot.cluster_enabled
            || source.analytics.cluster_gap_minutes != snapshot.cluster_gap_minutes
            || source.analytics.time_shift_minutes != snapshot.time_shift_minutes
            || source.analytics.price_change_pct != snapshot.price_change_pct
            || artifact
                .matrix_mean_r
                .as_ref()
                .is_some_and(|mean| *mean != snapshot.mean_r)
        {
            return Err(invalid(
                "expectancy artifact differs from immutable recommendation",
            ));
        }
        saved.derived_snapshot_id = Some(source.derived_snapshot_id.clone());
        if let Some(cell) = artifact.selected_cell.as_ref() {
            if cell
                .raw_r
                .as_ref()
                .is_some_and(|mean| *mean != snapshot.raw_r)
            {
                return Err(invalid("expectancy selected cell differs from saved raw R"));
            }
            saved.tp_count = Some(cell.tp_count);
            saved.sl_count = Some(cell.sl_count);
            saved.closed_count = Some(cell.closed_count);
            saved.win_rate_pct = cell.win_rate_pct.clone();
        } else if let MetricAvailability::Unavailable { reason } = artifact.selected_cell {
            saved.win_rate_pct = unavailable(reason);
        }
        saved.trend_delta_r = artifact.trend_delta_r.clone();
        saved.forecast_r = forecast;
        if artifact
            .daily
            .as_ref()
            .is_some_and(|days| days.iter().any(|day| day.mean_r.is_some()))
        {
            saved.image_key = Some(artifact.image_key(IMAGE_RENDER_VERSION));
        }
    }
    saved.validate_for(envelope, raw_signal_at)?;
    if serde_json::to_vec(&saved)?.len() > MAX_EXPECTANCY_SNAPSHOT_BYTES {
        return Err(invalid("expectancy scalar snapshot exceeds byte budget"));
    }
    Ok(saved)
}

/// Only called on speculative NEW setup writes. Existing scalar bytes win on
/// an ambiguous-commit retry, even if a newer derived artifact is now available.
/// `true` reports optional evidence degradation for one bounded commit summary;
/// strict recipe/floor/commit failures still return errors, never success.
pub(super) fn attach(
    slot: &ActiveAdaptiveSlot,
    instrument: &AdaptiveInstrument,
    setup: &mut AdaptiveSetup,
    evidence: Option<&CapturedArtifact>,
    artifact_id: Option<i64>,
) -> Result<bool, WorkerError> {
    let envelope: RecommendationEnvelope = serde_json::from_value(setup.recommendation.clone())?;
    if (
        envelope.study_id,
        envelope.generation_id,
        envelope.slot_id,
        envelope.owner_id.as_str(),
    ) != (
        slot.study_id,
        slot.generation_id,
        slot.slot_id,
        slot.owner_id.as_str(),
    ) || envelope.params != setup.raw.params
        || setup.raw.algorithm_id != slot.algorithm_id
        || setup.recommendation["rawSignalAt"].as_i64() != Some(setup.raw.close_time_unix)
        || envelope.snapshot.provenance.observation_cutoff > setup.raw.close_time_unix
    {
        return Err(invalid(
            "new setup expectancy envelope differs from generation",
        ));
    }
    // Do not introduce a second publication policy: enforce the already saved
    // generation/instrument floors, while the existing DB CAS owns live fences.
    if let Some(init) = &slot.live_initialization {
        if setup.raw.close_time_unix < init.emit_from_close_unix {
            return Err(invalid(
                "new setup expectancy precedes generation publication floor",
            ));
        }
    }
    let key = instrument_key(instrument);
    if let Some(readiness) = slot
        .live_readiness
        .iter()
        .find(|entry| entry.instrument_key == key)
    {
        if setup.raw.close_time_unix <= readiness.publication_floor_close_unix {
            return Err(invalid(
                "new setup expectancy precedes instrument publication floor",
            ));
        }
    }
    if let Some(value) = setup.recommendation.get("expectancy") {
        // Do not rewrite an already materialized intent on retry. A corrupt
        // optional field is isolated by the Telegram formatter as unavailable.
        let valid = serde_json::from_value::<AdaptiveExpectancySnapshot>(value.clone()).is_ok_and(
            |saved| {
                saved
                    .validate_for(&envelope, setup.raw.close_time_unix)
                    .is_ok()
            },
        );
        return Ok(!valid);
    }
    // Establish the strict scientific envelope first. Optional artifact failures
    // cannot change admission/readiness or suppress an otherwise valid setup.
    let fallback = freeze(&envelope, setup.raw.close_time_unix, None)?;
    let preferred = evidence.and_then(|evidence| {
        evidence.preferred.as_ref().filter(|preferred| {
            setup.raw.close_time_unix >= preferred.historical.available_from_unix
        })
    });
    let artifact = preferred
        .map(|preferred| &preferred.artifact)
        .or_else(|| evidence.map(|evidence| &evidence.original));
    let artifact_id = preferred
        .map(|preferred| preferred.artifact_id)
        .or(artifact_id);
    let (mut saved, degraded) = match artifact {
        None => (fallback, false),
        Some(artifact) => {
            let matches_horizon = slot.spec["horizonMinutes"].as_i64()
                == Some(artifact.source.analytics.horizon_minutes);
            match matches_horizon
                .then(|| freeze(&envelope, setup.raw.close_time_unix, Some(artifact)))
                .transpose()
            {
                Ok(Some(saved)) => (saved, false),
                _ => {
                    let mut saved = fallback;
                    saved.win_rate_pct =
                        unavailable(ExpectancyUnavailableReason::UnprovenOutcomeProvenance);
                    saved.trend_delta_r =
                        unavailable(ExpectancyUnavailableReason::UnprovenOutcomeProvenance);
                    saved.forecast_r =
                        unavailable(ExpectancyUnavailableReason::UnprovenOutcomeProvenance);
                    (saved, true)
                }
            }
        }
    };
    if !degraded && artifact.is_some() {
        saved.artifact_id = artifact_id;
        saved.historical_recalculation = preferred.map(|preferred| preferred.historical.clone());
        saved.validate_for(&envelope, setup.raw.close_time_unix)?;
    }
    if serde_json::to_vec(&saved)?.len() > MAX_EXPECTANCY_SNAPSHOT_BYTES {
        return Err(invalid("expectancy scalar snapshot exceeds byte budget"));
    }
    let value = serde_json::to_value(saved)?;
    // Bound the completed recommendation before changing the caller's copy.
    let mut candidate = setup.recommendation.clone();
    candidate["expectancy"] = value;
    if serde_json::to_vec(&candidate)?.len() > MAX_SNAPSHOT_BYTES {
        return Err(invalid(
            "enriched recommendation exceeds snapshot byte budget",
        ));
    }
    setup.recommendation = candidate;
    Ok(degraded)
}

/// Owned by this one commit attempt, never retained on a scientific LiveProfile.
/// Every OPTIONAL lookup failure degrades here. This function never executes or
/// catches the authoritative setup/checkpoint/outbox CAS that follows it.
pub(super) struct CapturedArtifact {
    original: ExpectancyArtifact,
    preferred: Option<PreferredExpectancy>,
}

struct PreferredExpectancy {
    artifact: ExpectancyArtifact,
    artifact_id: i64,
    historical: HistoricalExpectancySnapshot,
}

pub(super) struct CapturedExpectancy {
    pub artifact: Option<CapturedArtifact>,
    pub artifact_id: Option<i64>,
    pub invalid_evidence: bool,
}

pub(super) async fn load(pool: &PgPool, slot: &ActiveAdaptiveSlot) -> CapturedExpectancy {
    let row = setups_db::expectancy::get_for_generation(
        pool,
        &slot.owner_id,
        slot.generation_id,
        &EXPECTANCY_CALCULATION_VERSION.to_string(),
    )
    .await;
    let mut captured = capture_result(row, slot);
    // Native post-chain capture stays on the original one-query path. Both
    // copies below belong to this commit only, with no live/profile RAM cache.
    if captured
        .artifact
        .as_ref()
        .is_some_and(|artifact| artifact.original.daily.as_ref().is_none())
    {
        let preferred = setups_db::expectancy::get_published_recalculation(
            pool,
            &slot.owner_id,
            slot.generation_id,
            &EXPECTANCY_CALCULATION_VERSION.to_string(),
        )
        .await;
        capture_preferred(&mut captured, preferred, slot);
    }
    captured
}

fn capture_result(
    result: Result<Option<setups_db::expectancy::Artifact>, setups_db::DbError>,
    slot: &ActiveAdaptiveSlot,
) -> CapturedExpectancy {
    let row = match result {
        Ok(row) => row,
        Err(_) => {
            return CapturedExpectancy {
                artifact: None,
                artifact_id: None,
                invalid_evidence: true,
            };
        }
    };
    match row {
        None => CapturedExpectancy {
            artifact: None,
            artifact_id: None,
            invalid_evidence: false,
        },
        Some(row) => decode_record(row, slot),
    }
}

fn decode_record(
    row: setups_db::expectancy::Artifact,
    slot: &ActiveAdaptiveSlot,
) -> CapturedExpectancy {
    let id = row.id;
    match decode_artifact(row, slot, None) {
        Some(original) => CapturedExpectancy {
            artifact: Some(CapturedArtifact {
                original,
                preferred: None,
            }),
            artifact_id: Some(id),
            invalid_evidence: false,
        },
        None => CapturedExpectancy {
            artifact: None,
            artifact_id: None,
            invalid_evidence: true,
        },
    }
}

fn decode_artifact(
    row: setups_db::expectancy::Artifact,
    slot: &ActiveAdaptiveSlot,
    historical_input_hash: Option<&str>,
) -> Option<ExpectancyArtifact> {
    let valid = row.id > 0
        && row.identity.owner_id == slot.owner_id
        && row.identity.generation_id == slot.generation_id
        && row.identity.study_id == slot.study_id
        && row.identity.slot_id == slot.slot_id
        && row.identity.calculation_version == EXPECTANCY_CALCULATION_VERSION.to_string()
        && row.identity.tp_pct == slot.selected_cell["tpPct"].as_f64().unwrap_or(f64::NAN)
        && row.identity.sl_pct == slot.selected_cell["slPct"].as_f64().unwrap_or(f64::NAN)
        && row.identity.effective_analytics == slot.analytics
        && row.identity.historical_input_hash.as_deref() == historical_input_hash
        && setups_db::expectancy::artifact_key(&row.identity).is_ok_and(|key| key == row.key);
    let artifact = valid
        .then(|| serde_json::from_value::<ExpectancyArtifact>(row.numerical_snapshot))
        .transpose();
    match artifact {
        Ok(Some(artifact)) if artifact.source.derived_snapshot_id == row.key => Some(artifact),
        _ => None,
    }
}

fn capture_preferred(
    captured: &mut CapturedExpectancy,
    result: Result<Option<setups_db::expectancy::PublishedRecalculation>, setups_db::DbError>,
    slot: &ActiveAdaptiveSlot,
) {
    let preferred = match result {
        Ok(None) => return,
        Ok(Some(preferred)) => preferred,
        Err(_) => {
            captured.invalid_evidence = true;
            return;
        }
    };
    let Some(evidence) = captured.artifact.as_mut() else {
        captured.invalid_evidence = true;
        return;
    };
    let original = &evidence.original;
    let parsed = serde_json::from_value(preferred.provenance)
        .ok()
        .map(|provenance| HistoricalExpectancySnapshot {
            original_artifact_id: preferred.original_artifact_id,
            revision_key: preferred.revision_key,
            available_from_unix: preferred.available_from_unix,
            provenance,
        });
    let Some(historical) = parsed.filter(|historical| {
        Some(historical.original_artifact_id) == captured.artifact_id
            && historical
                .validate(original.source.provenance.observation_cutoff)
                .is_ok()
    }) else {
        captured.invalid_evidence = true;
        return;
    };
    let artifact_id = preferred.artifact.id;
    let Some(artifact) = decode_artifact(
        preferred.artifact,
        slot,
        Some(&historical.provenance.input_hash),
    ) else {
        captured.invalid_evidence = true;
        return;
    };
    let mut expected_source = original.source.clone();
    expected_source.derived_snapshot_id = artifact.source.derived_snapshot_id.clone();
    if artifact_id == historical.original_artifact_id
        || artifact.source != expected_source
        || artifact.selected_cell != original.selected_cell
        || artifact.matrix_mean_r != original.matrix_mean_r
        || artifact.daily.as_ref().is_none()
        || artifact.validate().is_err()
    {
        captured.invalid_evidence = true;
        return;
    }
    evidence.preferred = Some(PreferredExpectancy {
        artifact,
        artifact_id,
        historical,
    });
}

pub(super) fn report_degraded(slot: &ActiveAdaptiveSlot, affected_setups: usize) {
    // One bounded summary per commit, not per field/trade; no arbitrary owner,
    // external name, source payload or untrusted parse error enters the log.
    eprintln!(
        "[adaptive-expectancy] unavailable generation={} slot={} affected_setups={} code=invalid_optional_enrichment",
        slot.generation_id, slot.slot_id, affected_setups
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use setups_domain::{
        AlgorithmParams,
        expectancy::{ExpectancyInput, build_expectancy_artifact},
    };

    fn original(artifact: &ExpectancyArtifact) -> CapturedArtifact {
        CapturedArtifact {
            original: artifact.clone(),
            preferred: None,
        }
    }

    fn record(
        mut artifact: ExpectancyArtifact,
        slot: &ActiveAdaptiveSlot,
        id: i64,
        historical_input_hash: Option<String>,
    ) -> setups_db::expectancy::Artifact {
        let identity = setups_db::expectancy::ArtifactIdentity {
            owner_id: slot.owner_id.clone(),
            generation_id: slot.generation_id,
            study_id: slot.study_id,
            slot_id: slot.slot_id,
            calculation_version: EXPECTANCY_CALCULATION_VERSION.to_string(),
            tp_pct: artifact.source.tp_pct,
            sl_pct: artifact.source.sl_pct,
            from_unix: artifact.from_unix,
            to_unix: artifact.to_unix,
            observation_cutoff: artifact.source.provenance.observation_cutoff,
            provenance: serde_json::to_value(&artifact.source.provenance).unwrap(),
            effective_analytics: slot.analytics.clone(),
            historical_input_hash,
        };
        let key = setups_db::expectancy::artifact_key(&identity).unwrap();
        artifact.source.derived_snapshot_id = key.clone();
        setups_db::expectancy::Artifact {
            id,
            key,
            identity,
            numerical_snapshot: serde_json::to_value(artifact).unwrap(),
        }
    }

    fn historical_fixture(
        artifact: &ExpectancyArtifact,
        slot: &ActiveAdaptiveSlot,
        available_from_unix: i64,
    ) -> (
        CapturedExpectancy,
        setups_db::expectancy::PublishedRecalculation,
    ) {
        use setups_domain::expectancy::SelectedCellInput;
        let cell = artifact.selected_cell.as_ref().unwrap();
        let aggregate = build_expectancy_artifact(ExpectancyInput {
            source: artifact.source.clone(),
            selected_cell: Some(SelectedCellInput {
                raw_r: cell.raw_r.as_ref().copied(),
                tp_count: cell.tp_count,
                sl_count: cell.sl_count,
            }),
            matrix_mean_r: artifact.matrix_mean_r.as_ref().copied(),
            daily: MetricAvailability::Unavailable {
                reason: ExpectancyUnavailableReason::MissingFrozenOutcomes,
            },
        })
        .unwrap();
        let captured = decode_record(record(aggregate, slot, 23, None), slot);
        assert!(!captured.invalid_evidence);
        let provenance = setups_db::expectancy::HistoricalRecalculationProvenance {
            revision: setups_db::expectancy::HISTORICAL_RECALCULATION_REVISION,
            input_hash: "b".repeat(64),
            raw_setups_hash: "c".repeat(64),
            recalculated_at_unix: available_from_unix - 1,
            data_source: "existing_market_data_history".into(),
            read_from_unix: artifact.from_unix,
            read_to_unix: artifact.source.provenance.observation_cutoff,
            candle_count: 120,
            missing_minutes: 0,
        };
        let published = setups_db::expectancy::PublishedRecalculation {
            artifact: record(
                artifact.clone(),
                slot,
                24,
                Some(provenance.input_hash.clone()),
            ),
            original_artifact_id: 23,
            revision_key: provenance.input_hash.clone(),
            provenance: serde_json::to_value(provenance).unwrap(),
            available_from_unix,
        };
        (captured, published)
    }

    pub(super) fn fixture() -> (
        RecommendationEnvelope,
        ExpectancyArtifact,
        AdaptiveInstrument,
        AdaptiveSetup,
        ActiveAdaptiveSlot,
    ) {
        let fixture: Value = serde_json::from_str(include_str!(
            "../../../../domain/tests/fixtures/expectancy_utc_v2.json"
        ))
        .unwrap();
        let mut input: ExpectancyInput = serde_json::from_value(fixture["input"].clone()).unwrap();
        input.source.study_id = "42".into();
        input.source.generation_id = Some("3".into());
        input.source.profile_id = Some("7".into());
        let artifact = build_expectancy_artifact(input).unwrap();
        let source = &artifact.source;
        let params = AlgorithmParams::default();
        let envelope = RecommendationEnvelope {
            snapshot: setups_domain::AdaptiveRecommendationSnapshot {
                direction: source.direction,
                tp_pct: source.tp_pct,
                sl_pct: source.sl_pct,
                raw_r: *artifact
                    .selected_cell
                    .as_ref()
                    .unwrap()
                    .raw_r
                    .as_ref()
                    .unwrap(),
                mean_r: *artifact.matrix_mean_r.as_ref().unwrap(),
                robust_score: 0.2,
                time_shift_minutes: source.analytics.time_shift_minutes,
                price_change_pct: source.analytics.price_change_pct,
                cluster_enabled: source.analytics.cluster_enabled,
                cluster_gap_minutes: source.analytics.cluster_gap_minutes,
                provenance: source.provenance.clone(),
                search_forecast: None,
            },
            study_id: 42,
            generation_id: 3,
            slot_id: 7,
            owner_id: "admin".into(),
            external_name: "Saved <b>name</b> 😀".into(),
            params: params.clone(),
        };
        let at = fixture["expected"]["rawSignalAt"].as_i64().unwrap();
        let raw = SpikeSetup {
            algorithm_id: params.algorithm_id().into(),
            algorithm_version: params.algorithm_version().into(),
            open_time_unix: at - 60,
            close_time_unix: at,
            close_price: "100".into(),
            params: params.clone(),
            details: None,
        };
        let mut recommendation = serde_json::to_value(&envelope).unwrap();
        recommendation["rawSignalAt"] = json!(at);
        recommendation["recommendedEntryAt"] = json!(at + 180);
        let setup = AdaptiveSetup {
            identity: "fixture-identity".into(),
            raw,
            recommendation,
        };
        let instrument = AdaptiveInstrument {
            exchange: "bybit".into(),
            category: "linear".into(),
            symbol: "SKYUSDT".into(),
        };
        let slot = ActiveAdaptiveSlot {
            owner_id: envelope.owner_id.clone(),
            algorithm_id: params.algorithm_id().into(),
            slot_id: 7,
            generation_id: 3,
            study_id: 42,
            config_revision: 1,
            min_setups_per_day: 0.0,
            spec: json!({"horizonMinutes":source.analytics.horizon_minutes}),
            result: json!({"recommendation":envelope.snapshot}),
            params: serde_json::to_value(params).unwrap(),
            analytics: serde_json::to_value(&source.analytics).unwrap(),
            selected_cell: json!({"tpPct":source.tp_pct,"slPct":source.sl_pct,"rawR":envelope.snapshot.raw_r}),
            mean_expectancy: envelope.snapshot.mean_r,
            direction: "short".into(),
            external_name: envelope.external_name.clone(),
            live_initialization: None,
            live_readiness: Vec::new(),
        };
        (envelope, artifact, instrument, setup, slot)
    }

    #[test]
    fn same_generation_scalars_use_shared_forecast_at_raw_close_and_leave_recipe_and_artifact_unchanged()
     {
        let (envelope, artifact, instrument, mut setup, slot) = fixture();
        let old_result = slot.result.clone();
        let old_artifact = artifact.clone();
        let at = setup.raw.close_time_unix;
        let expected = artifact.forecast_at_raw_signal(at).unwrap();
        attach(
            &slot,
            &instrument,
            &mut setup,
            Some(&original(&artifact)),
            Some(23),
        )
        .unwrap();
        let saved: AdaptiveExpectancySnapshot =
            serde_json::from_value(setup.recommendation["expectancy"].clone()).unwrap();
        assert_eq!(saved.forecast_r, expected);
        assert_eq!(saved.forecast_at_unix, at);
        assert_eq!(saved.artifact_id, Some(23));
        assert_eq!(saved.tp_count, Some(32));
        assert_eq!(saved.sl_count, Some(13));
        assert_eq!(saved.closed_count, Some(45));
        assert_eq!(
            saved.selected_mean_r.as_ref(),
            Some(&envelope.snapshot.raw_r)
        );
        assert_eq!(
            saved.matrix_mean_r.as_ref(),
            Some(&envelope.snapshot.mean_r)
        );
        assert_eq!(
            saved.image_key,
            Some(artifact.image_key(IMAGE_RENDER_VERSION))
        );
        assert_eq!(slot.result, old_result);
        assert_eq!(artifact, old_artifact);
        assert!(serde_json::to_vec(&setup.recommendation).unwrap().len() < MAX_SNAPSHOT_BYTES);
        let later = freeze(&envelope, at + 86400, Some(&artifact)).unwrap();
        assert_ne!(later.forecast_r, saved.forecast_r);
        // The caption reuses the scalar snapshot, never current time or a fit.
        let caption = super::super::caption::recommendation_telegram_caption(
            &instrument,
            &setup.raw,
            &setup.recommendation,
        )
        .unwrap();
        assert!(caption.text.contains("📈 Прогноз матожидание торговли: +2.11 R"));
        assert!(!caption.text.contains("винрейт"));
        assert!(!caption.text.contains("Динамика МО"));
        assert!(!caption.text.contains("Среднее МО"));
    }

    #[test]
    fn missing_artifact_keeps_known_saved_means_and_unknown_counts_unavailable_not_zero() {
        let (envelope, _, instrument, mut setup, slot) = fixture();
        let legacy = setup.recommendation.clone();
        let saved = freeze(&envelope, setup.raw.close_time_unix, None).unwrap();
        assert_eq!(
            (saved.tp_count, saved.sl_count, saved.closed_count),
            (None, None, None)
        );
        assert!(saved.win_rate_pct.as_ref().is_none());
        assert!(saved.forecast_r.as_ref().is_none());
        assert!(saved.trend_delta_r.as_ref().is_none());
        assert!(saved.image_key.is_none());
        assert!(saved.selected_mean_r.as_ref().is_some());
        assert!(saved.matrix_mean_r.as_ref().is_some());
        let before = super::super::caption::recommendation_telegram_caption(
            &instrument,
            &setup.raw,
            &legacy,
        )
        .unwrap();
        assert!(!before.text.contains("винрейт") && !before.text.contains("Прогноз"));
        attach(&slot, &instrument, &mut setup, None, None).unwrap();
        assert_eq!(legacy.get("expectancy"), None); // source value is not mutated
        assert_eq!(setup.recommendation["expectancy"]["tpCount"], Value::Null);
    }

    #[test]
    fn ambiguous_retry_preserves_first_scalar_bytes_even_after_artifact_becomes_available() {
        let (_, artifact, instrument, mut setup, slot) = fixture();
        attach(&slot, &instrument, &mut setup, None, None).unwrap();
        let first = serde_json::to_vec(&setup.recommendation).unwrap();
        attach(
            &slot,
            &instrument,
            &mut setup,
            Some(&original(&artifact)),
            Some(23),
        )
        .unwrap();
        assert_eq!(serde_json::to_vec(&setup.recommendation).unwrap(), first);
        let mut tampered = setup.recommendation.clone();
        tampered["expectancy"]["forecastAtUnix"] = json!(setup.raw.close_time_unix + 60);
        let caption = super::super::caption::recommendation_telegram_caption(
            &instrument,
            &setup.raw,
            &tampered,
        )
        .unwrap();
        assert!(!caption.text.contains("Прогноз"));
        assert_eq!(
            caption.enrichment_unavailable_reason,
            Some("invalid_saved_enrichment")
        );
        assert_eq!(serde_json::to_vec(&setup.recommendation).unwrap(), first);
    }

    #[test]
    fn optional_artifact_mismatch_degrades_but_scientific_cutoff_generation_and_floors_remain_strict()
     {
        let (mut envelope, mut artifact, instrument, setup, mut slot) = fixture();
        let at = setup.raw.close_time_unix;
        envelope.snapshot.provenance.observation_cutoff = at + 1;
        assert!(freeze(&envelope, at, None).is_err());
        artifact.source.generation_id = Some("different".into());
        let mut next = setup.clone();
        assert!(
            attach(
                &slot,
                &instrument,
                &mut next,
                Some(&original(&artifact)),
                Some(23)
            )
            .unwrap()
        );
        assert!(next.recommendation["expectancy"]["tpCount"].is_null());
        assert_eq!(
            next.recommendation["expectancy"]["forecastR"]["status"],
            "unavailable"
        );
        next = setup.clone();
        slot.generation_id += 1;
        assert!(attach(&slot, &instrument, &mut next, None, None).is_err());
        slot.generation_id -= 1;
        slot.live_initialization = Some(live::LiveInitialization {
            version: 1,
            detector_from_open_unix: 0,
            anchor_close_unix: at,
            emit_from_close_unix: at + 60,
        });
        assert!(attach(&slot, &instrument, &mut next, None, None).is_err());
        assert_eq!(next.recommendation, setup.recommendation);
    }

    #[test]
    fn optional_record_corruption_is_isolated_from_recipe_and_setup_creation() {
        let (_, mut artifact, instrument, setup, slot) = fixture();
        let identity = setups_db::expectancy::ArtifactIdentity {
            owner_id: slot.owner_id.clone(),
            generation_id: slot.generation_id,
            study_id: slot.study_id,
            slot_id: slot.slot_id,
            calculation_version: EXPECTANCY_CALCULATION_VERSION.to_string(),
            tp_pct: artifact.source.tp_pct,
            sl_pct: artifact.source.sl_pct,
            from_unix: artifact.from_unix,
            to_unix: artifact.to_unix,
            observation_cutoff: artifact.source.provenance.observation_cutoff,
            provenance: serde_json::to_value(&artifact.source.provenance).unwrap(),
            effective_analytics: slot.analytics.clone(),
            historical_input_hash: None,
        };
        let key = setups_db::expectancy::artifact_key(&identity).unwrap();
        artifact.source.derived_snapshot_id = key.clone();
        let row = setups_db::expectancy::Artifact {
            id: 23,
            key,
            identity,
            numerical_snapshot: serde_json::to_value(&artifact).unwrap(),
        };
        let good = decode_record(row.clone(), &slot);
        assert!(!good.invalid_evidence);
        assert_eq!(good.artifact_id, Some(23));
        for bad in 0..3 {
            let mut row = row.clone();
            match bad {
                0 => row.identity.generation_id += 1,
                1 => row.numerical_snapshot = json!({"corrupt":true}),
                _ => row.key = "different-key".into(),
            }
            let captured = decode_record(row, &slot);
            assert!(captured.invalid_evidence);
            assert!(captured.artifact.is_none());
            let mut next = setup.clone();
            attach(
                &slot,
                &instrument,
                &mut next,
                captured.artifact.as_ref(),
                captured.artifact_id,
            )
            .unwrap();
            assert_eq!(next.raw, setup.raw);
            assert!(next.recommendation["expectancy"]["tpCount"].is_null());
            assert_eq!(
                next.recommendation["expectancy"]["selectedMeanR"]["value"],
                json!(artifact.selected_cell.as_ref().unwrap().raw_r.as_ref())
            );
        }
    }

    #[test]
    fn optional_json_and_sql_lookup_failures_both_degrade_before_the_authoritative_commit() {
        let (_, _, _, _, slot) = fixture();
        let invalid = capture_result(
            Err(setups_db::DbError::Json("invalid optional record".into())),
            &slot,
        );
        assert!(invalid.invalid_evidence);
        assert!(invalid.artifact.is_none());
        let absent = capture_result(Ok(None), &slot);
        assert!(!absent.invalid_evidence);
        assert!(absent.artifact.is_none());
        let failed = capture_result(
            Err(setups_db::DbError::Sqlx(sqlx::Error::RowNotFound)),
            &slot,
        );
        assert!(failed.invalid_evidence);
        assert!(failed.artifact.is_none());
    }

    #[test]
    fn legacy_aggregate_counts_keep_win_rate_without_inventing_a_daily_trend() {
        use setups_domain::expectancy::SelectedCellInput;
        let (_, artifact, instrument, mut setup, slot) = fixture();
        let cell = artifact.selected_cell.as_ref().unwrap();
        let legacy = build_expectancy_artifact(ExpectancyInput {
            source: artifact.source.clone(),
            selected_cell: Some(SelectedCellInput {
                raw_r: cell.raw_r.as_ref().copied(),
                tp_count: cell.tp_count,
                sl_count: cell.sl_count,
            }),
            matrix_mean_r: artifact.matrix_mean_r.as_ref().copied(),
            daily: MetricAvailability::Unavailable {
                reason: ExpectancyUnavailableReason::MissingFrozenOutcomes,
            },
        })
        .unwrap();
        assert!(
            !attach(
                &slot,
                &instrument,
                &mut setup,
                Some(&original(&legacy)),
                Some(23)
            )
            .unwrap()
        );
        let saved: AdaptiveExpectancySnapshot =
            serde_json::from_value(setup.recommendation["expectancy"].clone()).unwrap();
        assert_eq!(saved.tp_count, Some(32));
        assert_eq!(saved.sl_count, Some(13));
        assert_eq!(saved.win_rate_pct, cell.win_rate_pct);
        assert!(saved.forecast_r.as_ref().is_none());
        assert!(saved.trend_delta_r.as_ref().is_none());
        assert!(saved.image_key.is_none());
        let caption = super::super::caption::recommendation_telegram_caption(
            &instrument,
            &setup.raw,
            &setup.recommendation,
        )
        .unwrap();
        assert!(!caption.text.contains("винрейт") && !caption.text.contains("Прогноз"));
    }

    #[test]
    fn published_revision_freezes_actual_provenance_and_exact_artifact_without_changing_research() {
        let (envelope, artifact, instrument, mut setup, slot) = fixture();
        let before = setup.recommendation.clone();
        let result = slot.result.clone();
        let at = setup.raw.close_time_unix;
        let (mut captured, published) = historical_fixture(&artifact, &slot, at);
        let revision_key = published.artifact.key.clone();
        let published_before = serde_json::to_value(&published).unwrap();
        capture_preferred(&mut captured, Ok(Some(published.clone())), &slot);
        assert!(!captured.invalid_evidence);
        assert!(
            !attach(
                &slot,
                &instrument,
                &mut setup,
                captured.artifact.as_ref(),
                captured.artifact_id
            )
            .unwrap()
        );
        let saved: AdaptiveExpectancySnapshot =
            serde_json::from_value(setup.recommendation["expectancy"].clone()).unwrap();
        assert_eq!(saved.calculation_version, 2);
        assert_eq!(
            saved.artifact_id,
            Some(24),
            "commit must persist captured revision A, not base/pointer"
        );
        assert_eq!(
            saved.derived_snapshot_id.as_deref(),
            Some(revision_key.as_str())
        );
        assert_eq!(
            saved.source_snapshot_hash,
            envelope.snapshot.provenance.snapshot_hash
        );
        assert_eq!(
            saved.source_cutoff,
            envelope.snapshot.provenance.observation_cutoff
        );
        assert_eq!(
            saved.forecast_r,
            artifact.forecast_at_raw_signal(at).unwrap()
        );
        let historical = saved.historical_recalculation.as_ref().unwrap();
        assert_eq!(historical.original_artifact_id, 23);
        assert_eq!(historical.provenance.input_hash, "b".repeat(64));
        assert_ne!(historical.provenance.input_hash, saved.source_snapshot_hash);
        assert_eq!(
            saved.image_key.as_ref().unwrap().derived_snapshot_id,
            revision_key
        );
        let mut unchanged = setup.recommendation.clone();
        unchanged.as_object_mut().unwrap().remove("expectancy");
        assert_eq!(unchanged, before);
        assert_eq!(slot.result, result);
        assert_eq!(serde_json::to_value(&published).unwrap(), published_before);
        let caption = super::super::caption::recommendation_telegram_caption(
            &instrument,
            &setup.raw,
            &setup.recommendation,
        )
        .unwrap();
        assert!(caption.text.contains("📈 Прогноз матожидание торговли: +2.11 R"));
        assert!(serde_json::to_vec(&saved).unwrap().len() < MAX_EXPECTANCY_SNAPSHOT_BYTES);
    }

    #[test]
    fn one_captured_revision_applies_availability_per_raw_close_not_old_cutoff_or_commit_clock() {
        let (_, artifact, instrument, setup, slot) = fixture();
        let at = setup.raw.close_time_unix;
        let (mut captured, published) = historical_fixture(&artifact, &slot, at);
        capture_preferred(&mut captured, Ok(Some(published)), &slot);
        for raw in [at - 60, at, at + 60] {
            let mut next = setup.clone();
            next.raw.open_time_unix = raw - 60;
            next.raw.close_time_unix = raw;
            next.recommendation["rawSignalAt"] = json!(raw);
            next.recommendation["recommendedEntryAt"] = json!(raw + 180);
            assert!(
                !attach(
                    &slot,
                    &instrument,
                    &mut next,
                    captured.artifact.as_ref(),
                    captured.artifact_id
                )
                .unwrap()
            );
            let saved: AdaptiveExpectancySnapshot =
                serde_json::from_value(next.recommendation["expectancy"].clone()).unwrap();
            assert!(
                saved.source_cutoff < raw,
                "old cutoff alone does not establish revision availability"
            );
            assert!(saved.win_rate_pct.as_ref().is_some());
            if raw < at {
                assert_eq!(saved.artifact_id, Some(23));
                assert!(saved.historical_recalculation.is_none());
                assert!(saved.forecast_r.as_ref().is_none());
                assert!(saved.image_key.is_none());
            } else {
                assert_eq!(saved.artifact_id, Some(24));
                assert!(saved.historical_recalculation.is_some());
                assert!(saved.forecast_r.as_ref().is_some());
                assert!(saved.image_key.is_some());
            }
        }
    }

    #[test]
    fn preferred_revision_changes_never_rewrite_materialized_missing_base_or_revision_snapshots() {
        let (_, artifact, instrument, setup, slot) = fixture();
        let at = setup.raw.close_time_unix;
        let (mut captured_a, published_a) = historical_fixture(&artifact, &slot, at);
        capture_preferred(&mut captured_a, Ok(Some(published_a)), &slot);
        let (mut captured_b, mut published_b) = historical_fixture(&artifact, &slot, at);
        published_b.provenance["inputHash"] = json!("d".repeat(64));
        published_b.revision_key = "d".repeat(64);
        published_b.artifact = record(artifact.clone(), &slot, 25, Some("d".repeat(64)));
        capture_preferred(&mut captured_b, Ok(Some(published_b)), &slot);
        assert!(!captured_b.invalid_evidence);
        for first in [None, captured_a.artifact.as_ref()] {
            let mut next = setup.clone();
            attach(&slot, &instrument, &mut next, first, first.map(|_| 23)).unwrap();
            let frozen = serde_json::to_vec(&next.recommendation).unwrap();
            attach(
                &slot,
                &instrument,
                &mut next,
                captured_b.artifact.as_ref(),
                captured_b.artifact_id,
            )
            .unwrap();
            assert_eq!(serde_json::to_vec(&next.recommendation).unwrap(), frozen);
        }
        let (captured_base, _) = historical_fixture(&artifact, &slot, at);
        let mut next = setup;
        attach(
            &slot,
            &instrument,
            &mut next,
            captured_base.artifact.as_ref(),
            captured_base.artifact_id,
        )
        .unwrap();
        let frozen = serde_json::to_vec(&next.recommendation).unwrap();
        attach(
            &slot,
            &instrument,
            &mut next,
            captured_b.artifact.as_ref(),
            captured_b.artifact_id,
        )
        .unwrap();
        assert_eq!(serde_json::to_vec(&next.recommendation).unwrap(), frozen);
    }

    #[test]
    fn invalid_or_unreadable_preferred_revision_keeps_original_aggregate_evidence() {
        let (_, artifact, instrument, setup, slot) = fixture();
        let at = setup.raw.close_time_unix;
        for bad in 0..13 {
            let (mut captured, mut published) = historical_fixture(&artifact, &slot, at);
            match bad {
                0 => published.original_artifact_id += 1,
                1 => published.artifact.identity.generation_id += 1,
                2 => published.artifact.identity.owner_id = "another-owner".into(),
                3 => published.artifact.identity.tp_pct += 1.0,
                4 => published.revision_key = "e".repeat(64),
                5 => {
                    published.provenance["readToUnix"] =
                        json!(artifact.source.provenance.observation_cutoff + 60)
                }
                6 => published.provenance["recalculatedAtUnix"] = json!(at + 1),
                7 => {
                    published.artifact.numerical_snapshot["source"]["provenance"]["snapshotHash"] =
                        json!("another-input")
                }
                8 => {
                    published.artifact.numerical_snapshot["selectedCell"]["value"]["tpCount"] =
                        json!(999)
                }
                9 => published.artifact.numerical_snapshot["matrixMeanR"]["value"] = json!(99),
                10 => published.artifact.numerical_snapshot = json!({"corrupt":true}),
                11 => published.artifact.identity.historical_input_hash = Some("f".repeat(64)),
                _ => {
                    published.artifact.numerical_snapshot["daily"]["value"][0]["meanR"] = json!(99)
                }
            }
            capture_preferred(&mut captured, Ok(Some(published)), &slot);
            assert!(
                captured.invalid_evidence,
                "bad preferred evidence case {bad}"
            );
            assert!(captured.artifact.as_ref().unwrap().preferred.is_none());
            let mut next = setup.clone();
            assert!(
                !attach(
                    &slot,
                    &instrument,
                    &mut next,
                    captured.artifact.as_ref(),
                    captured.artifact_id
                )
                .unwrap()
            );
            assert_eq!(next.recommendation["expectancy"]["artifactId"], 23);
            assert_eq!(
                next.recommendation["expectancy"]["winRatePct"]["status"],
                "available"
            );
            assert_eq!(
                next.recommendation["expectancy"]["forecastR"]["status"],
                "unavailable"
            );
        }
        let (mut captured, _) = historical_fixture(&artifact, &slot, at);
        capture_preferred(
            &mut captured,
            Err(setups_db::DbError::Json("optional lookup failure".into())),
            &slot,
        );
        assert!(captured.invalid_evidence);
        assert!(captured.artifact.is_some());
        assert_eq!(captured.artifact_id, Some(23));
    }

    #[test]
    fn historical_snapshot_rejects_future_availability_and_invalid_actual_provenance() {
        let (envelope, artifact, instrument, mut setup, slot) = fixture();
        let at = setup.raw.close_time_unix;
        let (mut captured, published) = historical_fixture(&artifact, &slot, at);
        capture_preferred(&mut captured, Ok(Some(published)), &slot);
        attach(
            &slot,
            &instrument,
            &mut setup,
            captured.artifact.as_ref(),
            captured.artifact_id,
        )
        .unwrap();
        let saved: AdaptiveExpectancySnapshot =
            serde_json::from_value(setup.recommendation["expectancy"].clone()).unwrap();
        for bad in 0..4 {
            let mut tampered = saved.clone();
            match bad {
                0 => {
                    tampered
                        .historical_recalculation
                        .as_mut()
                        .unwrap()
                        .available_from_unix = at + 1
                }
                1 => {
                    tampered
                        .historical_recalculation
                        .as_mut()
                        .unwrap()
                        .provenance
                        .input_hash = "other".into()
                }
                2 => {
                    tampered
                        .historical_recalculation
                        .as_mut()
                        .unwrap()
                        .provenance
                        .read_to_unix = saved.source_cutoff + 60
                }
                _ => tampered.artifact_id = Some(23),
            }
            assert!(tampered.validate_for(&envelope, at).is_err());
        }
        let native = freeze(&envelope, at, Some(&artifact)).unwrap();
        let old_wire = serde_json::to_value(&native).unwrap();
        assert!(old_wire.get("historicalRecalculation").is_none());
        let restored: AdaptiveExpectancySnapshot = serde_json::from_value(old_wire).unwrap();
        assert_eq!(
            serde_json::to_vec(&native).unwrap(),
            serde_json::to_vec(&restored).unwrap()
        );
    }

    #[tokio::test]
    async fn postgres_lookup_only_failure_still_commits_both_paths_but_real_cas_failure_is_not_adopted()
     {
        use futures::FutureExt;
        use sqlx::Connection;
        let Ok(url) = std::env::var("TEST_DATABASE_URL") else {
            return;
        };
        let options: sqlx::postgres::PgConnectOptions = url.parse().unwrap();
        assert!(options.get_database().unwrap_or("").ends_with("_test"));
        let name = format!("adaptive_optional_lookup_{}_test", std::process::id());
        let mut admin = sqlx::PgConnection::connect_with(&options).await.unwrap();
        sqlx::query(&format!("create database {name}"))
            .execute(&mut admin)
            .await
            .unwrap();
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(3)
            .connect_with(options.database(&name))
            .await
            .unwrap();
        let outcome=std::panic::AssertUnwindSafe(async {
            let baseline=include_str!("../../../../db/migrations/0010_baseline.sql").lines()
                .filter(|line|!line.starts_with('\\') && !line.starts_with("SET transaction_timeout")).collect::<Vec<_>>().join("\n");
            let mut connection=pool.acquire().await.unwrap();
            sqlx::raw_sql(&baseline).execute(&mut *connection).await.unwrap();
            sqlx::raw_sql("set search_path=public; create table kysely_migration(name varchar(255) primary key,timestamp varchar(255) not null); insert into kysely_migration values('0010_baseline.sql','fixture');")
                .execute(&mut *connection).await.unwrap();drop(connection);
            setups_db::migrate::ensure_migrations(&pool).await.unwrap();
            continuous::configure(&pool,"admin","price_volume_spike",&json!({"adaptiveEnabled":true}),&[0.0]).await.unwrap();
            continuous::profiles::set_permission(&pool,"admin","price_volume_spike",true).await.unwrap();
            continuous::start(&pool,"admin","price_volume_spike").await.unwrap();
            setups_db::settings_set(&pool,"live",&json!({"enabled":true})).await.unwrap();
            setups_db::settings_set(&pool,"notifications",&json!({"maxAgeSeconds":86400})).await.unwrap();
            let (_,_,instrument,mut base,mut slot)=fixture();
            slot.slot_id=sqlx::query_scalar("select min(id) from continuous_slots").fetch_one(&pool).await.unwrap();
            slot.spec["algorithmVersion"]=json!(base.raw.algorithm_version);
            slot.generation_id=sqlx::query_scalar("insert into continuous_generations(owner_id,algorithm_id,slot_id,config_revision,lease_token,status,phase,spec,result,accepted,execution_started_at,observation_cutoff) values('admin','price_volume_spike',$1,1,1,'completed','completed',$2,$3,true,now(),$4) returning id")
                .bind(slot.slot_id).bind(&slot.spec).bind(&slot.result).bind(base.recommendation["provenance"]["observationCutoff"].as_i64().unwrap())
                .fetch_one(&pool).await.unwrap();
            slot.study_id=sqlx::query_scalar("insert into optimization_studies(algorithm_id,status,spec,progress,result,owner_id,origin,continuous_generation_id) values('price_volume_spike','completed',$1,'{}',$2,'admin','continuous',$3) returning id")
                .bind(&slot.spec).bind(&slot.result).bind(slot.generation_id).fetch_one(&pool).await.unwrap();
            sqlx::query("update continuous_generations set study_id=$2 where id=$1").bind(slot.generation_id).bind(slot.study_id).execute(&pool).await.unwrap();
            sqlx::query("update continuous_slots set active_generation_id=$2 where id=$1").bind(slot.slot_id).bind(slot.generation_id).execute(&pool).await.unwrap();
            sqlx::query("insert into adaptive_profiles(id,owner_id,algorithm_id,min_setups_per_day,accepted_generation_id) select id,owner_id,algorithm_id,min_setups_per_day,$2 from continuous_slots where id=$1")
                .bind(slot.slot_id).bind(slot.generation_id).execute(&pool).await.unwrap();
            let at=chrono::Utc::now().timestamp().div_euclid(60)*60;
            base.raw.open_time_unix=at-60;base.raw.close_time_unix=at;
            base.recommendation["rawSignalAt"]=json!(at);base.recommendation["recommendedEntryAt"]=json!(at+180);
            base.recommendation["generationId"]=json!(slot.generation_id);base.recommendation["studyId"]=json!(slot.study_id);
            base.recommendation["slotId"]=json!(slot.slot_id);
            let fence=AdaptiveFence {owner_id:slot.owner_id.clone(),algorithm_id:slot.algorithm_id.clone(),slot_id:slot.slot_id,
                generation_id:slot.generation_id,config_revision:slot.config_revision};
            let checkpoint=|next|AdaptiveCheckpoint {generation_id:slot.generation_id,next_open_unix:next,
                last_raw_open_unix:Some(next-60),last_raw_cluster_time:Some(next)};
            // Fault only the OPTIONAL SELECT. All authoritative tables, FKs and
            // the original CAS/outbox transaction remain real and healthy.
            sqlx::query("alter table study_expectancy_artifacts rename to fixture_hidden_expectancy_artifacts").execute(&pool).await.unwrap();
            let pg=super::super::store::PgAdaptiveStore {pool:&pool,slot:&slot};
            let mut first=vec![base.clone()];let first_checkpoint=checkpoint(at);
            assert_eq!(pg.commit_page(&fence,&instrument,None,&first_checkpoint,&mut first).await.unwrap().unwrap().len(),1);
            assert!(first[0].recommendation["expectancy"]["tpCount"].is_null());
            assert_eq!(first[0].recommendation["expectancy"]["forecastR"]["status"],"unavailable");
            // Exercise the batched entry point too, not a separate fake commit.
            let mut stream=crate::adaptive::state::tests::fixture();
            stream.runtime.install(&stream.epoch,std::slice::from_ref(&stream.spec)).unwrap();
            let mut prepared=stream.runtime.prepare(&stream.epoch,&stream.instrument,&[],stream.frontier,true).unwrap().pop().unwrap();
            let mut second=base.clone();second.raw.open_time_unix+=60;second.raw.close_time_unix+=60;
            second.recommendation["rawSignalAt"]=json!(at+60);second.recommendation["recommendedEntryAt"]=json!(at+240);
            let second_checkpoint=checkpoint(at+60);
            prepared.fence=fence.clone();prepared.expected=Some(first_checkpoint);prepared.checkpoint=second_checkpoint.clone();prepared.setups=vec![second];
            let mut scans=vec![super::super::batch::Scan {epoch:stream.epoch.clone(),instrument:instrument.clone(),slot:Arc::new(slot.clone()),prepared}];
            use super::super::batch::BatchStore;
            assert_eq!(super::super::batch::PgBatchStore {pool:&pool}.attempt(&slot,&mut scans,&[0]).await.unwrap(),vec![true]);
            assert_eq!(sqlx::query_scalar::<_,i64>("select count(*) from live_setups").fetch_one(&pool).await.unwrap(),2);
            assert_eq!(sqlx::query_scalar::<_,i64>("select count(*) from adaptive_notification_outbox").fetch_one(&pool).await.unwrap(),2);
            let key=instrument_key(&instrument);
            assert_eq!(super::super::store::load_checkpoint(&pool,slot.slot_id,&instrument).await.unwrap(),Some(second_checkpoint.clone()));
            let payload:Value=sqlx::query_scalar("select payload from adaptive_notification_outbox order by id limit 1").fetch_one(&pool).await.unwrap();
            assert_eq!(payload["recommendation"]["expectancy"]["forecastR"]["status"],"unavailable");
            // A real authoritative SQL failure must remain an error after
            // bounded recovery, with no successor/RAM adoption or partial rows.
            sqlx::raw_sql("create function fixture_reject_setup() returns trigger language plpgsql as $$ begin raise exception 'fixture_authoritative_cas_failure'; end $$; create trigger fixture_reject_setup before insert on live_setups for each row execute function fixture_reject_setup();")
                .execute(&pool).await.unwrap();
            let mut failed=base;failed.raw.open_time_unix+=120;failed.raw.close_time_unix+=120;
            failed.recommendation["rawSignalAt"]=json!(at+120);failed.recommendation["recommendedEntryAt"]=json!(at+300);
            let mut prepared=stream.runtime.prepare(&stream.epoch,&stream.instrument,&[],stream.frontier,true).unwrap().pop().unwrap();
            prepared.fence=fence;prepared.expected=Some(second_checkpoint.clone());prepared.checkpoint=checkpoint(at+120);prepared.setups=vec![failed];
            assert!(super::super::store::persist(&pg,&instrument,&mut prepared).await.is_err());
            assert!(stream.runtime.cached_checkpoint(&stream.epoch,&stream.spec.key()).unwrap().is_none());
            let durable:Value=sqlx::query_scalar("select state from adaptive_live_checkpoints where slot_id=$1 and instrument_key=$2")
                .bind(slot.slot_id).bind(key).fetch_one(&pool).await.unwrap();
            assert_eq!(durable,serde_json::to_value(second_checkpoint).unwrap());
            assert_eq!(sqlx::query_scalar::<_,i64>("select count(*) from live_setups").fetch_one(&pool).await.unwrap(),2);
            assert_eq!(sqlx::query_scalar::<_,i64>("select count(*) from adaptive_notification_outbox").fetch_one(&pool).await.unwrap(),2);
        }).catch_unwind().await;
        pool.close().await;
        sqlx::query(&format!("drop database {name} with (force)"))
            .execute(&mut admin)
            .await
            .unwrap();
        if let Err(panic) = outcome {
            std::panic::resume_unwind(panic);
        }
    }
}
