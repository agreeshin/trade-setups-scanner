//! Immutable derived artifacts, not research results or a trade-cache backfill.
//! The core owns the versioned numerical JSON contract and validates its daily
//! series. This layer owns bounded durable storage and real history/live refs.
use crate::{
    DbError,
    continuous::scheduler::{self, ChainJob},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Postgres, Row, Transaction};

mod historical;
pub use historical::*;

pub const MAX_IDENTITY_BYTES: usize = 32 * 1024;
pub const MAX_NUMERICAL_BYTES: usize = setups_domain::expectancy::EXPECTANCY_MAX_ARTIFACT_BYTES;
pub const MAX_PNG_BYTES: usize = 256 * 1024;
pub const MAX_ARTIFACTS: i64 = 8192;
pub const MAX_TOTAL_NUMERICAL_BYTES: i64 = 64 * 1024 * 1024;
pub const MAX_TOTAL_PNG_BYTES: i64 = 64 * 1024 * 1024;
const STORAGE_LOCK: i64 = 72423418;

/// Private storage identity, separate from (and never inserted into) StudyOutcome.
/// Provenance/effective analytics must be copied from the original frozen chain.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ArtifactIdentity {
    pub owner_id: String,
    pub generation_id: i64,
    pub study_id: i64,
    pub slot_id: i64,
    pub calculation_version: String,
    pub tp_pct: f64,
    pub sl_pct: f64,
    pub from_unix: i64,
    pub to_unix: i64,
    pub observation_cutoff: i64,
    pub provenance: Value,
    pub effective_analytics: Value,
    /// Actual historical input identity; provenance above is the ORIGINAL reference.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub historical_input_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forecast: Option<setups_domain::forecast::ForecastRecipe>,
    /// Hash of the exact captured UTC buckets, not an aggregate/rounded-R guess.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome_hash: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Artifact {
    pub id: i64,
    pub key: String,
    pub identity: ArtifactIdentity,
    /// Canonical core type serialized unchanged; no math in persistence.
    pub numerical_snapshot: Value,
}

/// Only exact saved source documents. In particular this API intentionally does
/// NOT read analytics_trades_cache or assert that raw replay setups are outcomes.
/// A core caller can recover the selected pair's aggregate mean/win rate here;
/// absent proven complete original outcomes, legacy daily trend is unavailable.
#[derive(Debug, Clone)]
pub struct SavedSource {
    pub generation_id: i64,
    pub study_id: i64,
    pub slot_id: i64,
    pub observation_cutoff: i64,
    pub spec: Value,
    pub result: Value,
    pub replay_manifest: Option<Value>,
}

fn invalid(message: &str) -> DbError {
    DbError::Json(format!("study_expectancy_{message}"))
}

/// Artifacts retain period means even when search optimizes forecast/blend.
/// Only legacy/mean objectives are equivalent when the additive mean is absent;
/// an explicit null/invalid mean must not silently fall back to the objective.
fn period_mean(
    value: &Value,
    mean_field: &str,
    objective_field: &str,
    trend_mode: Option<&Value>,
) -> Option<f64> {
    match value.get(mean_field) {
        Some(mean) => mean.as_f64(),
        None if trend_mode.is_none_or(|mode| mode.as_str() == Some("mean")) => {
            value[objective_field].as_f64()
        }
        None => None,
    }
}

fn bounded_object(value: &Value, maximum: usize) -> Result<(), DbError> {
    if !value.is_object() || serde_json::to_vec(value).map_err(crate::json_error)?.len() > maximum {
        return Err(invalid("payload_resource_limit"));
    }
    Ok(())
}

fn identity_value(identity: &ArtifactIdentity) -> Result<Value, DbError> {
    if identity.owner_id.is_empty()
        || identity.owner_id.len() > 120
        || identity.generation_id <= 0
        || identity.study_id <= 0
        || identity.slot_id <= 0
        || identity.calculation_version.is_empty()
        || identity.calculation_version.len() > 64
        || !identity.tp_pct.is_finite()
        || identity.tp_pct <= 0.0
        || !identity.sl_pct.is_finite()
        || identity.sl_pct <= 0.0
        || identity.from_unix < 0
        || identity.to_unix <= identity.from_unix
        || identity.observation_cutoff < 0
        || (identity.forecast.is_none() && identity.observation_cutoff > identity.to_unix)
        || !identity.provenance.is_object()
        || !identity.effective_analytics.is_object()
        || identity
            .historical_input_hash
            .as_ref()
            .is_some_and(|hash| !historical::valid_hash(hash))
        || identity
            .outcome_hash
            .as_ref()
            .is_some_and(|hash| !historical::valid_hash(hash))
        || (identity.calculation_version
            == setups_domain::expectancy::EXPECTANCY_MODEL_CALCULATION_VERSION.to_string()
            && (identity.forecast.is_none() || identity.outcome_hash.is_none()))
        || (identity.forecast.is_some()
            && (identity.calculation_version
                != setups_domain::expectancy::EXPECTANCY_MODEL_CALCULATION_VERSION.to_string()
                || identity.outcome_hash.is_none()
                || identity
                    .forecast
                    .is_some_and(|recipe| recipe.identity_key().is_err())))
        || (identity.forecast.is_none() && identity.outcome_hash.is_some())
    {
        return Err(invalid("identity_invalid"));
    }
    let value = serde_json::to_value(identity).map_err(crate::json_error)?;
    bounded_object(&value, MAX_IDENTITY_BYTES)?;
    Ok(value)
}

pub fn artifact_key(identity: &ArtifactIdentity) -> Result<String, DbError> {
    let value = identity_value(identity)?;
    fn canonical(value: &Value) -> Value {
        match value {
            Value::Object(object) => {
                let sorted: std::collections::BTreeMap<_, _> = object.iter().collect();
                Value::Object(
                    sorted
                        .into_iter()
                        .map(|(key, value)| (key.clone(), canonical(value)))
                        .collect(),
                )
            }
            Value::Array(items) => Value::Array(items.iter().map(canonical).collect()),
            _ => value.clone(),
        }
    }
    // Canonical ordering is explicit even when serde_json preserve_order is on.
    // Prefix separates private storage hashes from scientific/checkpoint hashes.
    let mut hash = Sha256::new();
    hash.update(b"study-expectancy-artifact-v1\0");
    hash.update(serde_json::to_vec(&canonical(&value)).map_err(crate::json_error)?);
    Ok(format!("{:x}", hash.finalize()))
}

pub fn captured_outcome_hash(
    daily: &[setups_domain::expectancy::UtcDailyInput],
) -> Result<String, DbError> {
    if daily.len() > setups_domain::expectancy::EXPECTANCY_MAX_POINTS {
        return Err(invalid("outcome_resource_limit"));
    }
    let bytes = serde_json::to_vec(daily).map_err(crate::json_error)?;
    if bytes.len() > MAX_NUMERICAL_BYTES {
        return Err(invalid("outcome_resource_limit"));
    }
    let mut hash = Sha256::new();
    hash.update(b"study-expectancy-outcomes-v1\0");
    hash.update(bytes);
    Ok(format!("{:x}", hash.finalize()))
}

pub fn encode_artifact(
    artifact: &setups_domain::expectancy::ExpectancyArtifact,
) -> Result<Value, DbError> {
    let bytes = artifact
        .to_json()
        .map_err(|error| DbError::Json(error.to_string()))?;
    let roundtrip = setups_domain::expectancy::ExpectancyArtifact::from_json(&bytes)
        .map_err(|error| DbError::Json(error.to_string()))?;
    if &roundtrip != artifact {
        return Err(invalid("numerical_roundtrip_mismatch"));
    }
    serde_json::from_slice(&bytes).map_err(crate::json_error)
}

fn decode_numerical(
    value: &Value,
) -> Result<setups_domain::expectancy::ExpectancyArtifact, DbError> {
    bounded_object(value, MAX_NUMERICAL_BYTES)?;
    setups_domain::expectancy::ExpectancyArtifact::from_json(
        &serde_json::to_vec(value).map_err(crate::json_error)?,
    )
    .map_err(|error| DbError::Json(error.to_string()))
}

fn decode(row: sqlx::postgres::PgRow) -> Result<Artifact, DbError> {
    let numerical_snapshot: Value = row.try_get("numerical_snapshot")?;
    decode_numerical(&numerical_snapshot)?;
    Ok(Artifact {
        id: row.try_get("id")?,
        key: row.try_get("artifact_key")?,
        identity: serde_json::from_value(row.try_get("identity")?).map_err(crate::json_error)?,
        numerical_snapshot: row.try_get("numerical_snapshot")?,
    })
}

pub async fn get(pool: &PgPool, owner: &str, id: i64) -> Result<Option<Artifact>, DbError> {
    sqlx::query("select id,artifact_key,identity,numerical_snapshot from study_expectancy_artifacts where id=$1 and owner_id=$2")
        .bind(id).bind(owner).fetch_optional(pool).await?.map(decode).transpose()
}

pub async fn get_for_generation(
    pool: &PgPool,
    owner: &str,
    generation: i64,
    version: &str,
) -> Result<Option<Artifact>, DbError> {
    if version == setups_domain::expectancy::EXPECTANCY_MODEL_CALCULATION_VERSION.to_string() {
        return Err(invalid("model_exact_identity_required"));
    }
    sqlx::query("select a.id,a.artifact_key,a.identity,a.numerical_snapshot from study_expectancy_artifacts a where owner_id=$1 and source_generation_id=$2 and calculation_version=$3 and revision_key='original' order by id limit 1")
        .bind(owner).bind(generation).bind(version).fetch_optional(pool).await?.map(decode).transpose()
}

/// Exact recipe + cutoff + outcome identity. No mutable/latest selection.
pub async fn get_by_identity(
    pool: &PgPool,
    identity: &ArtifactIdentity,
) -> Result<Option<Artifact>, DbError> {
    let row = sqlx::query("select id,artifact_key,identity,numerical_snapshot from study_expectancy_artifacts where artifact_key=$1 and owner_id=$2 and source_generation_id=$3 and calculation_version=$4")
        .bind(artifact_key(identity)?).bind(&identity.owner_id).bind(identity.generation_id).bind(&identity.calculation_version)
        .fetch_optional(pool).await?;
    let record = row.map(decode).transpose()?;
    if record
        .as_ref()
        .is_some_and(|record| record.identity != *identity)
    {
        return Err(invalid("exact_identity_mismatch"));
    }
    Ok(record)
}

/// Startup-only, keyset-paged admission for CURRENT active bindings. A fixed
/// high-water slot id and <=16 tiny rows per page bound ownership/lifetime;
/// neither completed inactive history nor pending generations are scanned.
pub struct ActiveEnrichmentBinding {
    pub slot_id: i64,
    pub owner_id: String,
    pub generation_id: i64,
}

pub async fn active_enrichment_high_water(pool: &PgPool) -> Result<i64, DbError> {
    Ok(sqlx::query_scalar("select coalesce(max(s.id),0) from continuous_slots s join adaptive_profiles p on p.id=s.id where p.desired_running and s.active_generation_id is not null").fetch_one(pool).await?)
}

pub async fn active_enrichment_page(
    pool: &PgPool,
    after_slot: i64,
    through_slot: i64,
) -> Result<Vec<ActiveEnrichmentBinding>, DbError> {
    let rows=sqlx::query("select s.id,s.owner_id,s.active_generation_id from continuous_slots s join adaptive_profiles p on p.id=s.id join continuous_generations g on g.id=s.active_generation_id and g.owner_id=s.owner_id and g.slot_id=s.id where p.desired_running and s.id>$1 and s.id<=$2 and (g.spec->'forecast' is null or g.spec->'forecast'='null'::jsonb) and not exists(select 1 from study_expectancy_artifacts a where a.owner_id=s.owner_id and a.source_generation_id=s.active_generation_id and a.calculation_version=$3) order by s.id limit 16")
        .bind(after_slot).bind(through_slot).bind(setups_domain::expectancy::EXPECTANCY_CALCULATION_VERSION.to_string()).fetch_all(pool).await?;
    rows.into_iter()
        .map(|row| {
            Ok(ActiveEnrichmentBinding {
                slot_id: row.try_get("id")?,
                owner_id: row.try_get("owner_id")?,
                generation_id: row.try_get("active_generation_id")?,
            })
        })
        .collect()
}

pub async fn saved_source(
    pool: &PgPool,
    owner: &str,
    generation: i64,
) -> Result<Option<SavedSource>, DbError> {
    let row = sqlx::query("select g.id,g.study_id,g.slot_id,g.observation_cutoff,g.spec,g.result,g.replay_manifest from continuous_generations g join optimization_studies s on s.id=g.study_id and s.owner_id=g.owner_id and s.origin='continuous' and s.status='completed' where g.id=$1 and g.owner_id=$2 and g.status='completed' and g.result is not null")
        .bind(generation).bind(owner).fetch_optional(pool).await?;
    row.map(|row| {
        Ok(SavedSource {
            generation_id: row.try_get("id")?,
            study_id: row.try_get("study_id")?,
            slot_id: row.try_get("slot_id")?,
            observation_cutoff: row.try_get("observation_cutoff")?,
            spec: row.try_get("spec")?,
            result: row.try_get("result")?,
            replay_manifest: row.try_get("replay_manifest")?,
        })
    })
    .transpose()
}

/// Build a legacy copy only from this generation's saved selected cell and
/// frozen post-chain matrix. No current slot parameters or caches participate.
/// The old schema has no proven complete individual-outcome archive: that is an
/// explicit unavailable daily series, not an inferred zero or a candle rerun.
pub async fn enrich_saved_generation(
    pool: &PgPool,
    owner: &str,
    generation: i64,
) -> Result<Option<i64>, DbError> {
    use setups_domain::{RecommendationProvenance, expectancy::*};
    let version = EXPECTANCY_CALCULATION_VERSION.to_string();
    if let Some(artifact) = get_for_generation(pool, owner, generation, &version).await? {
        return Ok(Some(artifact.id));
    }
    let Some(saved) = saved_source(pool, owner, generation).await? else {
        return Ok(None);
    };
    if saved
        .spec
        .get("forecast")
        .is_some_and(|recipe| !recipe.is_null())
    {
        return Ok(None);
    }
    let selected = &saved.result["selectedCell"];
    if !selected.is_object() {
        return Ok(None);
    }
    let tp = selected["tpPct"]
        .as_f64()
        .ok_or_else(|| invalid("selected_cell_invalid"))?;
    let sl = selected["slPct"]
        .as_f64()
        .ok_or_else(|| invalid("selected_cell_invalid"))?;
    let matrix = saved.result["analytics"]["evaluation"]["matrix"]
        .as_array()
        .ok_or_else(|| invalid("original_matrix_missing"))?;
    let mut matching = matrix
        .iter()
        .filter(|cell| cell["tpPct"].as_f64() == Some(tp) && cell["slPct"].as_f64() == Some(sl));
    let cell = matching
        .next()
        .ok_or_else(|| invalid("selected_cell_missing"))?;
    if matching.next().is_some() {
        return Err(invalid("selected_cell_ambiguous"));
    }
    let best = &saved.result["best"];
    let trend_mode = best
        .get("trendMode")
        .or_else(|| saved.spec.get("trendMode"));
    let analytics = &best["analytics"];
    let from_unix = saved.spec["fromUnix"]
        .as_i64()
        .ok_or_else(|| invalid("period_invalid"))?;
    let to_unix = saved.spec["toUnix"]
        .as_i64()
        .ok_or_else(|| invalid("period_invalid"))?;
    let identity = ArtifactIdentity {
        owner_id: owner.into(),
        generation_id: generation,
        study_id: saved.study_id,
        slot_id: saved.slot_id,
        calculation_version: version,
        tp_pct: tp,
        sl_pct: sl,
        from_unix,
        to_unix,
        observation_cutoff: saved.observation_cutoff,
        provenance: best["provenance"].clone(),
        effective_analytics: analytics.clone(),
        historical_input_hash: None,
        forecast: None,
        outcome_hash: None,
    };
    let provenance: RecommendationProvenance = serde_json::from_value(serde_json::json!({
        "algorithmId":saved.spec["algorithmId"],"algorithmVersion":saved.spec["algorithmVersion"],
        "optimizerVersion":identity.provenance["optimizerVersion"],"seed":identity.provenance["seed"],
        "snapshotHash":identity.provenance["snapshotHash"],"capturedAt":identity.provenance["capturedAt"],
        "fromUnix":from_unix,"toUnix":to_unix,"observationCutoff":saved.observation_cutoff,
    })).map_err(crate::json_error)?;
    let source = ExpectancySource { forecast:None,study_id:saved.study_id.to_string(),derived_snapshot_id:artifact_key(&identity)?,
        generation_id:Some(generation.to_string()),profile_id:Some(saved.slot_id.to_string()),provenance,
        direction:serde_json::from_value(saved.spec["direction"].clone()).map_err(crate::json_error)?,tp_pct:tp,sl_pct:sl,
        analytics:serde_json::from_value(serde_json::json!({"clusterEnabled":analytics["clusterEnabled"],
            "clusterGapMinutes":analytics["clusterGapMinutes"],"timeShiftMinutes":analytics["timeShiftMinutes"],
            "priceChangePct":analytics["priceChangePct"],"horizonMinutes":analytics["horizonMinutes"]})).map_err(crate::json_error)? };
    let artifact = build_expectancy_artifact(ExpectancyInput {
        source,
        selected_cell: Some(SelectedCellInput {
            raw_r: period_mean(cell, "meanRiskExpectancy", "riskExpectancy", trend_mode),
            tp_count: cell["tpCount"]
                .as_u64()
                .ok_or_else(|| invalid("selected_counts_invalid"))?,
            sl_count: cell["slCount"]
                .as_u64()
                .ok_or_else(|| invalid("selected_counts_invalid"))?,
        }),
        matrix_mean_r: saved.result["meanExpectancy"].as_f64(),
        daily: MetricAvailability::Unavailable {
            reason: ExpectancyUnavailableReason::MissingFrozenOutcomes,
        },
    })
    .map_err(|error| DbError::Json(error.to_string()))?;
    Ok(Some(
        seal_existing(pool, &identity, &encode_artifact(&artifact)?).await?,
    ))
}

async fn insert_in(
    tx: &mut Transaction<'_, Postgres>,
    identity: &ArtifactIdentity,
    numerical: &Value,
) -> Result<i64, DbError> {
    let value = identity_value(identity)?;
    bounded_object(numerical, MAX_NUMERICAL_BYTES)?;
    let key = artifact_key(identity)?;
    let artifact = decode_numerical(numerical)?;
    let source = &artifact.source;
    if artifact.calculation_version.to_string() != identity.calculation_version
        || source.forecast != identity.forecast
        || source.derived_snapshot_id != key
        || source.study_id != identity.study_id.to_string()
        || source.generation_id.as_deref() != Some(identity.generation_id.to_string().as_str())
        || source.profile_id.as_deref() != Some(identity.slot_id.to_string().as_str())
        || source.tp_pct != identity.tp_pct
        || source.sl_pct != identity.sl_pct
        || artifact.from_unix != identity.from_unix
        || artifact.to_unix != identity.to_unix
        || source.provenance.observation_cutoff != identity.observation_cutoff
        || serde_json::to_value(&source.provenance.snapshot_hash).map_err(crate::json_error)?
            != identity.provenance["snapshotHash"]
        || serde_json::to_value(&source.provenance.optimizer_version).map_err(crate::json_error)?
            != identity.provenance["optimizerVersion"]
        || serde_json::to_value(source.provenance.seed).map_err(crate::json_error)?
            != identity.provenance["seed"]
        || serde_json::to_value(&source.provenance.captured_at).map_err(crate::json_error)?
            != identity.provenance["capturedAt"]
    {
        return Err(invalid("numerical_identity_mismatch"));
    }
    if let Some(expected) = &identity.outcome_hash {
        let points = artifact
            .daily
            .as_ref()
            .ok_or_else(|| invalid("model_outcomes_missing"))?;
        let daily: Vec<_> = points
            .iter()
            .map(|point| setups_domain::expectancy::UtcDailyInput {
                day_start_unix: point.day_start_unix,
                row_count: point.row_count,
                tp_count: point.tp_count,
                sl_count: point.sl_count,
                mean_r: point.mean_r,
            })
            .collect();
        if captured_outcome_hash(&daily)? != *expected {
            return Err(invalid("outcome_identity_mismatch"));
        }
    }
    sqlx::query("select pg_advisory_xact_lock($1)")
        .bind(STORAGE_LOCK)
        .execute(&mut **tx)
        .await?;
    let revision = identity
        .historical_input_hash
        .as_deref()
        .unwrap_or("original");
    if let Some(row) = sqlx::query("select id,identity,numerical_snapshot from study_expectancy_artifacts where owner_id=$1 and source_generation_id=$2 and source_study_id=$3 and source_slot_id=$4 and tp_pct=$5 and sl_pct=$6 and calculation_version=$7 and revision_key=$8")
        .bind(&identity.owner_id).bind(identity.generation_id).bind(identity.study_id).bind(identity.slot_id)
        .bind(identity.tp_pct).bind(identity.sl_pct).bind(&identity.calculation_version).bind(revision).fetch_optional(&mut **tx).await? {
        if row.try_get::<Value, _>("identity")? != value || row.try_get::<Value, _>("numerical_snapshot")? != *numerical {
            return Err(invalid("immutable_conflict"));
        }
        return Ok(row.try_get("id")?);
    }
    let (rows, bytes): (i64, i64) = sqlx::query_as("select count(*),coalesce(sum(octet_length(identity::text)+octet_length(numerical_snapshot::text)),0)::bigint from study_expectancy_artifacts")
        .fetch_one(&mut **tx).await?;
    let requested: i64 = sqlx::query_scalar(
        "select octet_length($1::jsonb::text)::bigint+octet_length($2::jsonb::text)::bigint",
    )
    .bind(&value)
    .bind(numerical)
    .fetch_one(&mut **tx)
    .await?;
    if rows >= MAX_ARTIFACTS || bytes.saturating_add(requested) > MAX_TOTAL_NUMERICAL_BYTES {
        // Never evict completed history/live/pending work to admit enrichment.
        return Err(invalid("storage_resource_limit"));
    }
    Ok(sqlx::query_scalar("insert into study_expectancy_artifacts(artifact_key,owner_id,source_generation_id,source_study_id,source_slot_id,calculation_version,tp_pct,sl_pct,identity,numerical_snapshot,revision_key) values($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11) returning id")
        .bind(key).bind(&identity.owner_id).bind(identity.generation_id).bind(identity.study_id).bind(identity.slot_id)
        .bind(&identity.calculation_version).bind(identity.tp_pct).bind(identity.sl_pct).bind(value).bind(numerical).bind(revision).fetch_one(&mut **tx).await?)
}

async fn validate_source_in(
    tx: &mut Transaction<'_, Postgres>,
    identity: &ArtifactIdentity,
    numerical: &Value,
) -> Result<i32, DbError> {
    let row = sqlx::query("select g.spec,g.observation_cutoff,g.replay_batch_id,g.replay_manifest,s.result search_result from continuous_generations g join optimization_studies s on s.id=g.study_id and s.owner_id=g.owner_id and s.origin='continuous' and s.status='completed' where g.id=$1 and g.owner_id=$2 and g.study_id=$3 and g.slot_id=$4 for update of g")
        .bind(identity.generation_id).bind(&identity.owner_id).bind(identity.study_id).bind(identity.slot_id).fetch_optional(&mut **tx).await?
        .ok_or_else(|| invalid("source_mismatch"))?;
    let spec: Value = row.try_get("spec")?;
    let search: Value = row.try_get("search_result")?;
    let replay: Option<Value> = row.try_get("replay_manifest")?;
    if search["best"]["provenance"] != identity.provenance
        || search["best"]["analytics"] != identity.effective_analytics
        || replay
            .as_ref()
            .is_none_or(|replay| replay["provenance"] != identity.provenance)
        || !search["best"]["matrix"].as_array().is_some_and(|cells| {
            cells.iter().any(|cell| {
                cell["tpPct"].as_f64() == Some(identity.tp_pct)
                    && cell["slPct"].as_f64() == Some(identity.sl_pct)
            })
        })
    {
        return Err(invalid("provenance_mismatch"));
    }
    let original_cell = search["best"]["matrix"]
        .as_array()
        .and_then(|cells| {
            cells.iter().find(|cell| {
                cell["tpPct"].as_f64() == Some(identity.tp_pct)
                    && cell["slPct"].as_f64() == Some(identity.sl_pct)
            })
        })
        .ok_or_else(|| invalid("selected_cell_missing"))?;
    let artifact = decode_numerical(numerical)?;
    let frozen_recipe =
        |value: &Value| -> Result<Option<setups_domain::forecast::ForecastRecipe>, DbError> {
            value
                .get("forecast")
                .filter(|recipe| !recipe.is_null())
                .cloned()
                .map(serde_json::from_value)
                .transpose()
                .map_err(crate::json_error)
        };
    if frozen_recipe(&spec)? != identity.forecast
        || frozen_recipe(&search["best"])? != identity.forecast
        || (identity.forecast.is_some()
            && search["best"]["observationCutoff"].as_i64() != Some(identity.observation_cutoff))
    {
        return Err(invalid("original_recipe_cutoff_mismatch"));
    }
    let metrics = artifact
        .selected_cell
        .as_ref()
        .ok_or_else(|| invalid("selected_metrics_missing"))?;
    let trend_mode = search["best"]
        .get("trendMode")
        .or_else(|| spec.get("trendMode"));
    if identity.forecast.is_some()
        && trend_mode.is_some_and(|mode| matches!(mode.as_str(), Some("forecast" | "blend")))
    {
        let forecast = artifact.forecast.as_ref().and_then(|model| {
            model
                .probability
                .as_ref()
                .and_then(|model| model.estimate.as_ref().map(|estimate| estimate.forecast_r))
                .or_else(|| model.trend.as_ref().map(|model| model.end.value))
        });
        if forecast != original_cell["forecastRiskExpectancy"].as_f64() {
            return Err(invalid("original_forecast_mismatch"));
        }
    }
    if Some(metrics.tp_count) != original_cell["tpCount"].as_u64()
        || Some(metrics.sl_count) != original_cell["slCount"].as_u64()
        || metrics.raw_r.as_ref().copied()
            != period_mean(
                original_cell,
                "meanRiskExpectancy",
                "riskExpectancy",
                trend_mode,
            )
        || artifact.matrix_mean_r.as_ref().copied()
            != period_mean(&search["best"], "meanScore", "score", trend_mode)
        || serde_json::to_value(&artifact.source.provenance.algorithm_id)
            .map_err(crate::json_error)?
            != spec["algorithmId"]
        || serde_json::to_value(&artifact.source.provenance.algorithm_version)
            .map_err(crate::json_error)?
            != spec["algorithmVersion"]
        || serde_json::to_value(artifact.source.direction).map_err(crate::json_error)?
            != spec["direction"]
    {
        return Err(invalid("original_metrics_mismatch"));
    }
    let analytics = serde_json::to_value(&artifact.source.analytics).map_err(crate::json_error)?;
    if !analytics.as_object().is_some_and(|object| {
        object
            .iter()
            .all(|(key, value)| identity.effective_analytics.get(key) == Some(value))
    }) {
        return Err(invalid("original_analytics_mismatch"));
    }
    if spec["fromUnix"].as_i64() != Some(identity.from_unix)
        || spec["toUnix"].as_i64() != Some(identity.to_unix)
        || row.try_get::<i64, _>("observation_cutoff")? != identity.observation_cutoff
    {
        return Err(invalid("source_mismatch"));
    }
    row.try_get::<Option<i32>, _>("replay_batch_id")?
        .ok_or_else(|| invalid("original_replay_missing"))
}

async fn link_in(
    tx: &mut Transaction<'_, Postgres>,
    identity: &ArtifactIdentity,
    batch: i32,
    id: i64,
) -> Result<(), DbError> {
    sqlx::query("update continuous_generations set expectancy_artifact_id=$2 where id=$1 and expectancy_artifact_id is null")
        .bind(identity.generation_id).bind(id).execute(&mut **tx).await?;
    let linked = sqlx::query("update replay_batches set expectancy_artifact_id=coalesce(expectancy_artifact_id,$2) where id=$1 and owner_id=$3 and origin='continuous' and continuous_generation_id=$4")
        .bind(batch).bind(id).bind(&identity.owner_id).bind(identity.generation_id).execute(&mut **tx).await?.rows_affected();
    if linked != 1 {
        return Err(invalid("original_replay_missing"));
    }
    Ok(())
}

/// Call after the canonical post-chain consistency check, BEFORE save_gate and
/// frozen cleanup. One short fenced transaction; no candle reads/science/render.
/// Lost ACK/restart returns the same id; a differing payload fails explicitly.
pub async fn seal_for_chain(
    pool: &PgPool,
    job: &ChainJob,
    identity: &ArtifactIdentity,
    numerical: &Value,
) -> Result<Option<i64>, DbError> {
    if identity.owner_id != job.lease.owner_id
        || identity.generation_id != job.generation_id
        || identity.study_id != job.study_id
        || identity.slot_id != job.slot_id
    {
        return Err(invalid("job_identity_mismatch"));
    }
    let mut tx = pool.begin().await?;
    if !scheduler::fenced(&mut tx, job).await? {
        return Ok(None);
    }
    let id = seal_for_chain_in(&mut tx, job, identity, numerical).await?;
    if !scheduler::owns(&mut tx, &job.lease).await? {
        return Ok(None);
    }
    tx.commit().await?;
    Ok(Some(id))
}

/// Caller already holds the authoritative continuous fence. Used to commit
/// the numerical seal and gate as ONE durable recovery boundary.
pub(crate) async fn seal_for_chain_in(
    tx: &mut Transaction<'_, Postgres>,
    job: &ChainJob,
    identity: &ArtifactIdentity,
    numerical: &Value,
) -> Result<i64, DbError> {
    if identity.historical_input_hash.is_some() {
        return Err(invalid("historical_requires_explicit_staging"));
    }
    if identity.owner_id != job.lease.owner_id
        || identity.generation_id != job.generation_id
        || identity.study_id != job.study_id
        || identity.slot_id != job.slot_id
    {
        return Err(invalid("job_identity_mismatch"));
    }
    let batch = validate_source_in(tx, identity, numerical).await?;
    let id = insert_in(tx, identity, numerical).await?;
    link_in(tx, identity, batch, id).await?;
    Ok(id)
}

/// Additive legacy enrichment. Core must mark missing/unproven daily history
/// unavailable; having arbitrary cache rows is never an admission predicate.
pub async fn seal_existing(
    pool: &PgPool,
    identity: &ArtifactIdentity,
    numerical: &Value,
) -> Result<i64, DbError> {
    if identity.historical_input_hash.is_some() {
        return Err(invalid("historical_requires_explicit_staging"));
    }
    let mut tx = pool.begin().await?;
    super::continuous::owner_lock(&mut tx, &identity.owner_id).await?;
    let completed: bool = sqlx::query_scalar("select exists(select 1 from continuous_generations where id=$1 and owner_id=$2 and status='completed')")
        .bind(identity.generation_id).bind(&identity.owner_id).fetch_one(&mut *tx).await?;
    if !completed {
        return Err(invalid("source_not_completed"));
    }
    let batch = validate_source_in(&mut tx, identity, numerical).await?;
    let id = insert_in(&mut tx, identity, numerical).await?;
    link_in(&mut tx, identity, batch, id).await?;
    tx.commit().await?;
    Ok(id)
}

/// The caller is the bounded core renderer. Header checks here are a storage
/// guard, not a substitute for the relay's independent full PNG validation.
pub async fn store_png(
    pool: &PgPool,
    owner: &str,
    artifact: i64,
    version: &str,
    png: &[u8],
) -> Result<(), DbError> {
    if version.is_empty()
        || version.len() > 64
        || png.len() < 33
        || png.len() > MAX_PNG_BYTES
        || png[..8] != *b"\x89PNG\r\n\x1a\n"
        || png[8..12] != 13_u32.to_be_bytes()
        || png[12..16] != *b"IHDR"
        || png[16..20] != 900_u32.to_be_bytes()
        || png[20..24] != 320_u32.to_be_bytes()
    {
        return Err(invalid("png_invalid"));
    }
    let record = get(pool, owner, artifact)
        .await?
        .ok_or_else(|| invalid("artifact_missing"))?;
    let render_version = version
        .parse::<u32>()
        .ok()
        .filter(|version| *version > 0)
        .ok_or_else(|| invalid("render_version_invalid"))?;
    if (record.identity.forecast.is_some() && render_version != 2)
        || (record.identity.forecast.is_none() && render_version == 2)
    {
        return Err(invalid("render_recipe_version_mismatch"));
    }
    let image_key = serde_json::to_value(
        decode_numerical(&record.numerical_snapshot)?.image_key(render_version),
    )
    .map_err(crate::json_error)?;
    let mut tx = pool.begin().await?;
    sqlx::query("select pg_advisory_xact_lock($1)")
        .bind(STORAGE_LOCK)
        .execute(&mut *tx)
        .await?;
    if !sqlx::query_scalar::<_, bool>(
        "select exists(select 1 from study_expectancy_artifacts where id=$1 and owner_id=$2)",
    )
    .bind(artifact)
    .bind(owner)
    .fetch_one(&mut *tx)
    .await?
    {
        return Err(invalid("artifact_missing"));
    }
    if let Some(existing) = sqlx::query_scalar::<_, Vec<u8>>(
        "select png from study_expectancy_pngs where artifact_id=$1 and rendering_version=$2",
    )
    .bind(artifact)
    .bind(version)
    .fetch_optional(&mut *tx)
    .await?
    {
        if existing != png {
            return Err(invalid("png_immutable_conflict"));
        }
        return Ok(());
    }
    let bytes: i64 = sqlx::query_scalar(
        "select coalesce(sum(octet_length(png)),0)::bigint from study_expectancy_pngs",
    )
    .fetch_one(&mut *tx)
    .await?;
    let rows: i64 = sqlx::query_scalar("select count(*) from study_expectancy_pngs")
        .fetch_one(&mut *tx)
        .await?;
    if rows >= MAX_ARTIFACTS || bytes.saturating_add(png.len() as i64) > MAX_TOTAL_PNG_BYTES {
        return Err(invalid("png_storage_resource_limit"));
    }
    sqlx::query("insert into study_expectancy_pngs(artifact_id,rendering_version,image_key,png,sha256) values($1,$2,$3,$4,$5)")
        .bind(artifact).bind(version).bind(image_key).bind(png).bind(format!("{:x}", Sha256::digest(png))).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

pub async fn load_png(
    pool: &PgPool,
    owner: &str,
    artifact: i64,
    version: &str,
) -> Result<Option<Vec<u8>>, DbError> {
    Ok(sqlx::query_scalar("select p.png from study_expectancy_pngs p join study_expectancy_artifacts a on a.id=p.artifact_id where p.artifact_id=$1 and p.rendering_version=$2 and a.owner_id=$3")
        .bind(artifact).bind(version).bind(owner).fetch_optional(pool).await?)
}

/// Resolve ALL immutable image-key fields, including the exact numerical copy,
/// selected pair, UTC/calculation/render versions and frozen snapshot hash.
/// Never route through a mutable current generation or study label alone.
pub async fn load_png_by_image_key(
    pool: &PgPool,
    owner: &str,
    key: &setups_domain::expectancy::ExpectancyImageKey,
) -> Result<Option<Vec<u8>>, DbError> {
    let row=sqlx::query("select id,artifact_key,identity,numerical_snapshot from study_expectancy_artifacts where artifact_key=$1 and owner_id=$2")
        .bind(&key.derived_snapshot_id).bind(owner).fetch_optional(pool).await?;
    let Some(record) = row.map(decode).transpose()? else {
        return Ok(None);
    };
    if decode_numerical(&record.numerical_snapshot)?.image_key(key.render_version) != *key {
        return Err(invalid("image_key_mismatch"));
    }
    Ok(sqlx::query_scalar("select png from study_expectancy_pngs where artifact_id=$1 and rendering_version=$2 and image_key=$3")
        .bind(record.id).bind(key.render_version.to_string()).bind(serde_json::to_value(key).map_err(crate::json_error)?).fetch_optional(pool).await?)
}

pub struct RenderJob {
    pub artifact: Artifact,
    /// Three fields from the artifact's original generation spec, not current
    /// research/profile settings. Orphan/legacy specs project an empty object.
    pub study_trend: Value,
    pub render_version: u32,
    worker: String,
    token: i64,
}

/// One bounded claim; caller owns at most one render/one numeric payload. Retry
/// state survives restart, expires after 30s, and stops after three attempts.
pub async fn claim_render(
    pool: &PgPool,
    worker: &str,
    version: u32,
) -> Result<Option<RenderJob>, DbError> {
    if worker.is_empty() || worker.len() > 200 || !matches!(version, 1 | 2) {
        return Err(invalid("render_claim_invalid"));
    }
    let mut tx = pool.begin().await?;
    sqlx::query("select pg_advisory_xact_lock($1)")
        .bind(STORAGE_LOCK)
        .execute(&mut *tx)
        .await?;
    let row=sqlx::query("select a.id,a.artifact_key,a.identity,a.numerical_snapshot,jsonb_strip_nulls(jsonb_build_object('trendMode',g.spec->'trendMode','trendBendDays',g.spec->'trendBendDays','trendBlendLambda',g.spec->'trendBlendLambda') || case when a.calculation_version='3' then jsonb_build_object('forecast',g.spec->'forecast','observationCutoff',g.observation_cutoff) else '{}'::jsonb end) study_trend from study_expectancy_artifacts a left join study_expectancy_render_attempts r on r.artifact_id=a.id and r.render_version=$1 left join continuous_generations g on g.id=a.source_generation_id and g.owner_id=a.owner_id where (($1=1 and a.calculation_version='2') or ($1=2 and a.calculation_version='3')) and (g.id is null or g.status='completed') and a.numerical_snapshot#>>'{daily,status}'='available' and jsonb_array_length(a.numerical_snapshot#>'{daily,value}')>0 and not exists(select 1 from study_expectancy_pngs p where p.artifact_id=a.id and p.rendering_version=$2) and (r.artifact_id is null or (r.attempts<3 and (r.lease_until is null or r.lease_until<=clock_timestamp()) and r.next_attempt_at<=clock_timestamp())) order by a.id limit 1")
        .bind(version as i32).bind(version.to_string()).fetch_optional(&mut *tx).await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let study_trend = row.try_get("study_trend")?;
    let artifact = decode(row)?;
    let count: i64 = sqlx::query_scalar("select count(*) from study_expectancy_render_attempts")
        .fetch_one(&mut *tx)
        .await?;
    let existing:bool=sqlx::query_scalar("select exists(select 1 from study_expectancy_render_attempts where artifact_id=$1 and render_version=$2)")
        .bind(artifact.id).bind(version as i32).fetch_one(&mut *tx).await?;
    if !existing && count >= MAX_ARTIFACTS {
        return Err(invalid("render_claim_resource_limit"));
    }
    let token=sqlx::query_scalar("insert into study_expectancy_render_attempts(artifact_id,render_version,attempts,lease_owner,lease_until) values($1,$2,1,$3,clock_timestamp()+interval '30 seconds') on conflict(artifact_id,render_version) do update set attempts=study_expectancy_render_attempts.attempts+1,lease_owner=excluded.lease_owner,lease_token=study_expectancy_render_attempts.lease_token+1,lease_until=excluded.lease_until returning lease_token")
        .bind(artifact.id).bind(version as i32).bind(worker).fetch_one(&mut *tx).await?;
    tx.commit().await?;
    Ok(Some(RenderJob {
        artifact,
        study_trend,
        render_version: version,
        worker: worker.into(),
        token,
    }))
}

/// No transaction spans the renderer. A deterministic PNG is immutable even
/// across a lost claim/ACK; stale failures cannot alter a newer owner's claim.
pub async fn finish_render(
    pool: &PgPool,
    job: &RenderJob,
    png: Option<&[u8]>,
    error_code: Option<&str>,
) -> Result<(), DbError> {
    if let Some(png) = png {
        store_png(
            pool,
            &job.artifact.identity.owner_id,
            job.artifact.id,
            &job.render_version.to_string(),
            png,
        )
        .await?;
        sqlx::query("delete from study_expectancy_render_attempts where artifact_id=$1 and render_version=$2 and lease_owner=$3 and lease_token=$4")
            .bind(job.artifact.id).bind(job.render_version as i32).bind(&job.worker).bind(job.token).execute(pool).await?;
    } else {
        let code = error_code
            .filter(|code| code.len() <= 256)
            .unwrap_or("expectancy_render_failed");
        sqlx::query("update study_expectancy_render_attempts set lease_owner=null,lease_until=null,error=$5,next_attempt_at=clock_timestamp()+interval '5 seconds' where artifact_id=$1 and render_version=$2 and lease_owner=$3 and lease_token=$4")
            .bind(job.artifact.id).bind(job.render_version as i32).bind(&job.worker).bind(job.token).bind(code).execute(pool).await?;
    }
    Ok(())
}

/// Bounded GC only for artifacts with NO real owner. Replay/history and live
/// rows (therefore pending delivery jobs) survive regardless of age; PNGs share
/// their artifact's lifetime. Call on startup/normal cleanup, never by age alone.
pub async fn collect_unreferenced(pool: &PgPool, limit: u32) -> Result<u64, DbError> {
    let mut tx = pool.begin().await?;
    sqlx::query("select pg_advisory_xact_lock($1)")
        .bind(STORAGE_LOCK)
        .execute(&mut *tx)
        .await?;
    let removed = sqlx::query("with orphan as (select a.id from study_expectancy_artifacts a where not exists(select 1 from continuous_generations g where g.expectancy_artifact_id=a.id) and not exists(select 1 from replay_batches b where b.expectancy_artifact_id=a.id) and not exists(select 1 from live_setups l where l.expectancy_artifact_id=a.id) and not exists(select 1 from study_expectancy_recalculations r where r.artifact_id=a.id or r.original_artifact_id=a.id) and not exists(select 1 from study_expectancy_publications p where p.artifact_id=a.id) order by a.id limit $1 for update of a skip locked) delete from study_expectancy_artifacts a using orphan o where a.id=o.id")
        .bind(i64::from(limit.min(100))).execute(&mut *tx).await?.rows_affected();
    tx.commit().await?;
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn period_mean_prefers_saved_mean_and_only_legacy_mean_can_fall_back() {
        for (mean_field, objective_field) in [
            ("meanRiskExpectancy", "riskExpectancy"),
            ("meanScore", "score"),
        ] {
            for mode in [
                None,
                Some(json!("mean")),
                Some(json!("forecast")),
                Some(json!("blend")),
            ] {
                let mut value = json!({});
                value[objective_field] = json!(1.7);
                value[mean_field] = json!(-0.25);
                assert_eq!(
                    period_mean(&value, mean_field, objective_field, mode.as_ref()),
                    Some(-0.25)
                );
                for invalid in [Value::Null, json!("invalid")] {
                    value[mean_field] = invalid;
                    assert_eq!(
                        period_mean(&value, mean_field, objective_field, mode.as_ref()),
                        None
                    );
                }
            }
            let mut legacy = json!({});
            legacy[objective_field] = json!(1.0);
            assert_eq!(
                period_mean(&legacy, mean_field, objective_field, None),
                Some(1.0)
            );
            assert_eq!(
                period_mean(&legacy, mean_field, objective_field, Some(&json!("mean"))),
                Some(1.0)
            );
            for mode in [
                json!("forecast"),
                json!("blend"),
                json!("invalid"),
                Value::Null,
            ] {
                assert_eq!(
                    period_mean(&legacy, mean_field, objective_field, Some(&mode)),
                    None
                );
            }
        }
    }
}
