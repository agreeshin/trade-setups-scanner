//! One bounded, explicitly authorized historical operation. No scheduler,
//! market reads, scientific mutation or automatic activation lives here.
use super::*;

pub const HISTORICAL_RECALCULATION_REVISION: u32 = 1;
pub const MAX_RECALCULATION_BYTES: usize = 256 * 1024;
const MAX_TOTAL_RECALCULATION_BYTES: i64 = 32 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HistoricalRecalculationProvenance {
    pub revision: u32,
    pub input_hash: String,
    pub raw_setups_hash: String,
    pub recalculated_at_unix: i64,
    /// Portable source label, never an endpoint, key or connection string.
    pub data_source: String,
    pub read_from_unix: i64,
    pub read_to_unix: i64,
    pub candle_count: u64,
    pub missing_minutes: u64,
}

impl HistoricalRecalculationProvenance {
    pub fn validate(&self) -> Result<(), DbError> {
        if self.revision != HISTORICAL_RECALCULATION_REVISION
            || !valid_hash(&self.input_hash)
            || !valid_hash(&self.raw_setups_hash)
            || self.recalculated_at_unix < 0
            || self.read_from_unix < 0
            || self.read_to_unix < self.read_from_unix
            || self.data_source != "existing_market_data_history"
        {
            return Err(invalid("historical_provenance_invalid"));
        }
        Ok(())
    }
}

pub(super) fn valid_hash(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[derive(Debug, Clone)]
pub struct HistoricalAdmission {
    pub source: SavedSource,
    pub original_artifact: Artifact,
    pub config_revision: i64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoricalStage {
    pub id: i64,
    pub revision_key: String,
    pub artifact_id: Option<i64>,
    pub matched: bool,
    pub comparison: Value,
    pub recalculation: HistoricalRecalculationProvenance,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PublishedRecalculation {
    pub artifact: Artifact,
    pub original_artifact_id: i64,
    pub provenance: Value,
    pub revision_key: String,
    /// NEW setup raw-close must be at/after this independent availability floor.
    pub available_from_unix: i64,
}

fn decode_stage(row: sqlx::postgres::PgRow) -> Result<HistoricalStage, DbError> {
    let recalculation: HistoricalRecalculationProvenance =
        serde_json::from_value(row.try_get("provenance")?).map_err(crate::json_error)?;
    recalculation.validate()?;
    let revision_key: String = row.try_get("revision_key")?;
    if revision_key != recalculation.input_hash {
        return Err(invalid("historical_revision_mismatch"));
    }
    Ok(HistoricalStage {
        id: row.try_get("id")?,
        revision_key,
        artifact_id: row.try_get("artifact_id")?,
        matched: row.try_get("matched")?,
        comparison: row.try_get("comparison")?,
        recalculation,
    })
}

/// Capture only a currently enabled legacy binding, never pending/inactive history.
pub async fn admit_historical_recalculation(
    pool: &PgPool,
    owner: &str,
    generation: i64,
) -> Result<Option<HistoricalAdmission>, DbError> {
    let row = sqlx::query("select p.config_revision,g.expectancy_artifact_id from continuous_slots s join adaptive_live_controls p on p.id=s.id join continuous_generations g on g.id=s.active_generation_id and g.owner_id=s.owner_id and g.slot_id=s.id where s.owner_id=$1 and s.active_generation_id=$2 and p.desired_running and p.config->>'adaptiveEnabled'='true' and g.status='completed'")
        .bind(owner).bind(generation).fetch_optional(pool).await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let id: Option<i64> = row.try_get("expectancy_artifact_id")?;
    let Some(original_artifact) = (match id {
        Some(id) => get(pool, owner, id).await?,
        None => None,
    }) else {
        return Err(invalid("historical_original_artifact_missing"));
    };
    let original = decode_numerical(&original_artifact.numerical_snapshot)?;
    if original_artifact.identity.historical_input_hash.is_some()
        || original.daily.as_ref().is_some()
    {
        return Ok(None);
    }
    let Some(source) = saved_source(pool, owner, generation).await? else {
        return Err(invalid("historical_source_missing"));
    };
    if (
        original_artifact.identity.generation_id,
        original_artifact.identity.study_id,
        original_artifact.identity.slot_id,
    ) != (generation, source.study_id, source.slot_id)
    {
        return Err(invalid("historical_original_identity_mismatch"));
    }
    Ok(Some(HistoricalAdmission {
        source,
        original_artifact,
        config_revision: row.try_get("config_revision")?,
    }))
}

/// Read bounded original raw rows by their exact manifest IDs. No detector or
/// ordinary trade cache. The manifest keeps original cluster/bootstrap seeds.
pub async fn load_historical_replay(
    pool: &PgPool,
    owner: &str,
    source: &SavedSource,
    max_bytes: usize,
) -> Result<Value, DbError> {
    let mut manifest = source
        .replay_manifest
        .clone()
        .ok_or_else(|| invalid("historical_replay_missing"))?;
    let mut tx = pool.begin().await?;
    let bytes:i64=sqlx::query_scalar("select coalesce(sum(octet_length(setups::text)),0)::bigint from replay_runs where owner_id=$1 and continuous_generation_id=$2 and origin='continuous'")
        .bind(owner).bind(source.generation_id).fetch_one(&mut *tx).await?;
    if bytes < 0 || bytes as u64 > max_bytes as u64 {
        return Err(invalid("historical_replay_resource_limit"));
    }
    let items = manifest["instruments"]
        .as_array_mut()
        .ok_or_else(|| invalid("historical_replay_scope_missing"))?;
    if items.is_empty() || items.len() > 1000 {
        return Err(invalid("historical_replay_scope_limit"));
    }
    let mut seen = std::collections::BTreeSet::new();
    for item in items {
        if !item
            .as_object()
            .is_some_and(|item| item.contains_key("previousRawCloseUnix"))
        {
            return Err(invalid("historical_causal_seed_missing"));
        }
        let id = item["runId"]
            .as_i64()
            .and_then(|id| i32::try_from(id).ok())
            .ok_or_else(|| invalid("historical_run_id_missing"))?;
        if !seen.insert(id) {
            return Err(invalid("historical_duplicate_run"));
        }
        item["setups"]=sqlx::query_scalar::<_,Value>("select setups from replay_runs where id=$1 and owner_id=$2 and continuous_generation_id=$3 and origin='continuous' and status='done' and exchange_code=$4 and category=$5 and symbol=$6")
            .bind(id).bind(owner).bind(source.generation_id).bind(item["instrument"]["exchange"].as_str())
            .bind(item["instrument"]["category"].as_str()).bind(item["instrument"]["symbol"].as_str())
            .fetch_one(&mut *tx).await?;
        item.as_object_mut()
            .ok_or_else(|| invalid("historical_manifest_invalid"))?
            .remove("runId");
    }
    tx.commit().await?;
    if serde_json::to_vec(&manifest)
        .map_err(crate::json_error)?
        .len()
        > max_bytes
    {
        return Err(invalid("historical_replay_resource_limit"));
    }
    Ok(manifest)
}

pub async fn get_historical_stage(
    pool: &PgPool,
    owner: &str,
    generation: i64,
    revision_key: &str,
) -> Result<Option<HistoricalStage>, DbError> {
    sqlx::query("select id,revision_key,artifact_id,matched,comparison,provenance from study_expectancy_recalculations where owner_id=$1 and generation_id=$2 and revision_key=$3")
        .bind(owner).bind(generation).bind(revision_key).fetch_optional(pool).await?.map(decode_stage).transpose()
}

/// Compare the full saved selected-cell counter vector and available causal
/// evidence. A legacy missing field is NOT silently replaced with today's zero.
pub fn historical_expected_comparison(
    source: &SavedSource,
    tp: f64,
    sl: f64,
) -> Result<Value, DbError> {
    let analytics = &source.result["analytics"];
    let cell = analytics["evaluation"]["matrix"]
        .as_array()
        .and_then(|cells| {
            cells.iter().find(|cell| {
                cell["tpPct"].as_f64() == Some(tp) && cell["slPct"].as_f64() == Some(sl)
            })
        })
        .ok_or_else(|| invalid("historical_original_cell_missing"))?;
    let mut expected = serde_json::Map::new();
    expected.insert("selectedCell".into(), cell.clone());
    for key in ["setupCount", "selectedSetupCount"] {
        let value = &analytics["evaluation"]["summary"][key];
        if !value.is_null() {
            expected.insert(key.into(), value.clone());
        }
    }
    if let (Some(selected), Some(no_data), Some(filtered)) = (
        expected.get("selectedSetupCount").and_then(Value::as_u64),
        cell["noDataCount"].as_u64(),
        cell["priceChangeFilteredCount"].as_u64(),
    ) {
        let eligible = selected
            .checked_add(no_data)
            .and_then(|value| value.checked_add(filtered))
            .ok_or_else(|| invalid("historical_original_counter_overflow"))?;
        expected.insert("causalEligibleCount".into(), Value::from(eligible));
        if let Some(raw) = expected.get("setupCount").and_then(Value::as_u64) {
            let excluded = raw
                .checked_sub(eligible)
                .ok_or_else(|| invalid("historical_original_causal_counters_invalid"))?;
            expected.insert("causalExcludedCount".into(), Value::from(excluded));
        }
    }
    if let Some(diagnostic) = analytics["diagnostics"].as_array().and_then(|items| {
        items
            .iter()
            .find(|item| item["tpPct"].as_f64() == Some(tp) && item["slPct"].as_f64() == Some(sl))
    }) {
        expected.insert("diagnostics".into(), diagnostic.clone());
    }
    Ok(Value::Object(expected))
}

async fn binding_matches(
    tx: &mut Transaction<'_, Postgres>,
    admission: &HistoricalAdmission,
    lock: bool,
) -> Result<bool, DbError> {
    let query = if lock {
        "select p.config_revision,g.study_id,g.expectancy_artifact_id from continuous_slots s join adaptive_live_controls p on p.id=s.id join continuous_generations g on g.id=s.active_generation_id and g.slot_id=s.id and g.owner_id=s.owner_id where s.owner_id=$1 and s.id=$2 and s.active_generation_id=$3 and p.desired_running and p.config->>'adaptiveEnabled'='true' and g.status='completed' for update of s"
    } else {
        "select p.config_revision,g.study_id,g.expectancy_artifact_id from continuous_slots s join adaptive_live_controls p on p.id=s.id join continuous_generations g on g.id=s.active_generation_id and g.slot_id=s.id and g.owner_id=s.owner_id where s.owner_id=$1 and s.id=$2 and s.active_generation_id=$3 and p.desired_running and p.config->>'adaptiveEnabled'='true' and g.status='completed'"
    };
    let row = sqlx::query(query)
        .bind(&admission.original_artifact.identity.owner_id)
        .bind(admission.source.slot_id)
        .bind(admission.source.generation_id)
        .fetch_optional(&mut **tx)
        .await?;
    row.map(|row| {
        Ok(
            row.try_get::<i64, _>("config_revision")? == admission.config_revision
                && row.try_get::<Option<i64>, _>("study_id")? == Some(admission.source.study_id)
                && row.try_get::<Option<i64>, _>("expectancy_artifact_id")?
                    == Some(admission.original_artifact.id),
        )
    })
    .transpose()
    .map(|matches| matches.unwrap_or(false))
}

/// Stage matched numbers OR an explicit mismatch. Same actual input returns
/// the original immutable operation/time; lost ACK does not create a new copy.
pub async fn stage_historical_recalculation(
    pool: &PgPool,
    admission: &HistoricalAdmission,
    provenance: &HistoricalRecalculationProvenance,
    comparison: &Value,
    numerical: Option<&Value>,
) -> Result<Option<HistoricalStage>, DbError> {
    provenance.validate()?;
    bounded_object(comparison, 65536)?;
    if provenance.read_to_unix > admission.source.observation_cutoff {
        return Err(invalid("historical_cutoff_exceeded"));
    }
    let expected = comparison
        .get("expected")
        .filter(|value| value.is_object())
        .ok_or_else(|| invalid("historical_comparison_missing"))?;
    let actual = comparison
        .get("actual")
        .filter(|value| value.is_object())
        .ok_or_else(|| invalid("historical_comparison_missing"))?;
    let original = &admission.original_artifact.identity;
    if *expected
        != historical_expected_comparison(&admission.source, original.tp_pct, original.sl_pct)?
    {
        return Err(invalid("historical_expected_evidence_mismatch"));
    }
    let matched = expected == actual;
    if matched != numerical.is_some() {
        return Err(invalid("historical_comparison_mismatch"));
    }
    let mut tx = pool.begin().await?;
    if !binding_matches(&mut tx, admission, false).await? {
        return Ok(None);
    }
    sqlx::query("select pg_advisory_xact_lock($1)")
        .bind(STORAGE_LOCK)
        .execute(&mut *tx)
        .await?;
    if let Some(row)=sqlx::query("select id,revision_key,artifact_id,matched,comparison,provenance from study_expectancy_recalculations where owner_id=$1 and generation_id=$2 and revision_key=$3")
        .bind(&admission.original_artifact.identity.owner_id).bind(admission.source.generation_id).bind(&provenance.input_hash).fetch_optional(&mut *tx).await? {
        let previous=decode_stage(row)?;
        if previous.comparison!=*comparison || previous.recalculation.raw_setups_hash!=provenance.raw_setups_hash {return Err(invalid("historical_immutable_conflict"));}
        tx.commit().await?;return Ok(Some(previous));
    }
    let metadata = serde_json::to_value(provenance).map_err(crate::json_error)?;
    bounded_object(&metadata, MAX_RECALCULATION_BYTES)?;
    let (rows,bytes):(i64,i64)=sqlx::query_as("select count(*),coalesce(sum(octet_length(provenance::text)+octet_length(comparison::text)),0)::bigint from study_expectancy_recalculations").fetch_one(&mut *tx).await?;
    let requested = serde_json::to_vec(&metadata)
        .map_err(crate::json_error)?
        .len()
        + serde_json::to_vec(comparison)
            .map_err(crate::json_error)?
            .len();
    if rows >= MAX_ARTIFACTS
        || bytes.saturating_add(requested as i64) > MAX_TOTAL_RECALCULATION_BYTES
    {
        return Err(invalid("historical_storage_resource_limit"));
    }
    let mut identity = admission.original_artifact.identity.clone();
    identity.historical_input_hash = Some(provenance.input_hash.clone());
    let artifact_id = if let Some(numerical) = numerical {
        validate_source_in(&mut tx, &identity, numerical).await?;
        let artifact = decode_numerical(numerical)?;
        if artifact.daily.as_ref().is_none() {
            return Err(invalid("historical_daily_missing"));
        }
        Some(insert_in(&mut tx, &identity, numerical).await?)
    } else {
        None
    };
    let row=sqlx::query("insert into study_expectancy_recalculations(owner_id,generation_id,study_id,slot_id,config_revision,revision_key,original_artifact_id,artifact_id,provenance,comparison,matched) values($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11) returning id,revision_key,artifact_id,matched,comparison,provenance")
        .bind(&identity.owner_id).bind(identity.generation_id).bind(identity.study_id).bind(identity.slot_id).bind(admission.config_revision)
        .bind(&provenance.input_hash).bind(admission.original_artifact.id).bind(artifact_id).bind(metadata).bind(comparison).bind(matched)
        .fetch_one(&mut *tx).await?;
    let staged = decode_stage(row)?;
    tx.commit().await?;
    Ok(Some(staged))
}

/// Explicit operator phase AFTER compatible readers deploy. No setup/replay/
/// science/outbox updates. A replaced or stopped binding returns None.
pub async fn publish_historical_recalculation(
    pool: &PgPool,
    owner: &str,
    generation: i64,
    revision_key: &str,
) -> Result<Option<PublishedRecalculation>, DbError> {
    let Some(admission) = admit_historical_recalculation(pool, owner, generation).await? else {
        return Ok(None);
    };
    let mut tx = pool.begin().await?;
    if !binding_matches(&mut tx, &admission, true).await? {
        return Ok(None);
    }
    let row=sqlx::query("select id,artifact_id,provenance,config_revision from study_expectancy_recalculations where owner_id=$1 and generation_id=$2 and revision_key=$3 and matched")
        .bind(owner).bind(generation).bind(revision_key).fetch_optional(&mut *tx).await?;
    let Some(row) = row else {
        return Err(invalid("historical_matched_revision_missing"));
    };
    if row.try_get::<i64, _>("config_revision")? != admission.config_revision {
        return Ok(None);
    }
    let provenance: HistoricalRecalculationProvenance =
        serde_json::from_value(row.try_get("provenance")?).map_err(crate::json_error)?;
    provenance.validate()?;
    let artifact_id: i64 = row.try_get("artifact_id")?;
    let prior=sqlx::query("select artifact_id from study_expectancy_publications where owner_id=$1 and generation_id=$2")
        .bind(owner).bind(generation).fetch_optional(&mut *tx).await?;
    if prior.is_some_and(|row| row.get::<i64, _>("artifact_id") != artifact_id) {
        return Err(invalid("historical_publication_conflict"));
    }
    sqlx::query("insert into study_expectancy_publications(owner_id,generation_id,slot_id,recalculation_id,artifact_id,available_from_unix) values($1,$2,$3,$4,$5,greatest(ceil(extract(epoch from clock_timestamp()))::bigint,$6)) on conflict(owner_id,generation_id) do nothing")
        .bind(owner).bind(generation).bind(admission.source.slot_id).bind(row.try_get::<i64,_>("id")?).bind(artifact_id)
        .bind(provenance.recalculated_at_unix).execute(&mut *tx).await?;
    tx.commit().await?;
    get_published_recalculation(
        pool,
        owner,
        generation,
        &setups_domain::expectancy::EXPECTANCY_CALCULATION_VERSION.to_string(),
    )
    .await
}

pub async fn get_published_recalculation(
    pool: &PgPool,
    owner: &str,
    generation: i64,
    version: &str,
) -> Result<Option<PublishedRecalculation>, DbError> {
    let row=sqlx::query("select a.id,a.artifact_key,a.identity,a.numerical_snapshot,r.provenance,r.original_artifact_id,r.revision_key,p.available_from_unix from study_expectancy_publications p join study_expectancy_artifacts a on a.id=p.artifact_id and a.owner_id=p.owner_id and a.source_generation_id=p.generation_id join study_expectancy_recalculations r on r.id=p.recalculation_id and r.artifact_id=a.id and r.matched where p.owner_id=$1 and p.generation_id=$2 and a.calculation_version=$3")
        .bind(owner).bind(generation).bind(version).fetch_optional(pool).await?;
    row.map(|row| {
        let recalculation: HistoricalRecalculationProvenance =
            serde_json::from_value(row.try_get("provenance")?).map_err(crate::json_error)?;
        recalculation.validate()?;
        let available_from_unix = row.try_get("available_from_unix")?;
        let original_artifact_id = row.try_get("original_artifact_id")?;
        let revision_key: String = row.try_get("revision_key")?;
        let artifact = decode(row)?;
        if artifact.identity.historical_input_hash.as_deref()
            != Some(recalculation.input_hash.as_str())
            || revision_key != recalculation.input_hash
            || available_from_unix < recalculation.recalculated_at_unix
        {
            return Err(invalid("historical_publication_identity_mismatch"));
        }
        Ok(PublishedRecalculation {
            artifact,
            original_artifact_id,
            revision_key,
            provenance: serde_json::to_value(recalculation).map_err(crate::json_error)?,
            available_from_unix,
        })
    })
    .transpose()
}
