//! Independent optimizer metadata. No raw candles, trial archive or shared caches.
use crate::DbError;
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::{Value, json};
use sqlx::{PgPool, Row, postgres::PgRow};

pub const RUNTIME_LOCK: i64 = 72_423_414;
pub mod manual;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ManualControl {
    pub revision: i64,
    pub supports_pause: bool,
}

fn manual_control(row: &PgRow) -> Result<Option<ManualControl>, DbError> {
    Ok((row.try_get::<String, _>("origin")? == "manual").then(|| ManualControl {
        revision: row.get("manual_control_revision"),
        supports_pause: row.get::<Option<i32>, _>("manual_format_version") == Some(1),
    }))
}

pub async fn runtime_owned(pool: &PgPool) -> Result<bool, DbError> {
    Ok(sqlx::query_scalar("select exists(select 1 from pg_locks where locktype='advisory' and database=(select oid from pg_database where datname=current_database()) and classid=0 and objid=$1::bigint::oid and objsubid=1 and granted)")
        .bind(RUNTIME_LOCK).fetch_one(pool).await?)
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StudyRecord {
    pub id: i64,
    pub status: String,
    pub spec: Value,
    pub progress: Value,
    pub result: Option<Value>,
    pub error: Option<Value>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub events: Value,
    pub origin: String,
    pub owner_id: String,
    pub continuous_generation_id: Option<i64>,
    pub continuous_slot_id: Option<i64>,
    pub manual_control: Option<ManualControl>,
}

impl StudyRecord {
    pub fn active(&self) -> bool {
        matches!(
            self.status.as_str(),
            "queued" | "running" | "pausing" | "paused" | "cancel_requested"
        )
    }
}

fn record(row: PgRow) -> Result<StudyRecord, DbError> {
    Ok(StudyRecord {
        manual_control: manual_control(&row)?,
        id: row.try_get("id")?,
        status: row.try_get("status")?,
        spec: row.try_get("spec")?,
        progress: row.try_get("progress")?,
        result: row.try_get("result")?,
        error: row.try_get("error")?,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
        events: row.try_get("events")?,
        origin: row.try_get("origin")?,
        owner_id: row.try_get("owner_id")?,
        continuous_generation_id: row.try_get("continuous_generation_id")?,
        continuous_slot_id: row.try_get("continuous_slot_id")?,
    })
}

pub enum Admission {
    Created(Box<StudyRecord>),
    Busy(i64),
    IdempotencyConflict,
    Resource { resource: &'static str, limit: i64, requested: i64 },
}

pub async fn create(
    pool: &PgPool,
    algorithm_id: &str,
    spec: &Value,
    progress: &Value,
) -> Result<Admission, DbError> {
    manual::create(pool, "admin", algorithm_id, spec, progress, None, &Default::default()).await
}

pub async fn get(pool: &PgPool, id: i64) -> Result<Option<StudyRecord>, DbError> {
    sqlx::query("select * from optimization_studies where id=$1")
        .bind(id)
        .fetch_optional(pool)
        .await?
        .map(record)
        .transpose()
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ForecastBestPair {
    pub forecast_r: f64,
    pub mean_r: f64,
    pub tp_pct: f64,
    pub sl_pct: f64,
}

fn forecast_mode<'a>(best: &'a Value, spec: &'a Value) -> Option<&'a str> {
    // Only persisted inputs belong to this result; never use a current config.
    best.get("trendMode")
        .or_else(|| spec.get("trendMode"))?
        .as_str()
}

fn forecast_pair(cell: &Value, mode: Option<&str>) -> Option<ForecastBestPair> {
    let finite = |value: &Value| value.as_f64().filter(|value| value.is_finite());
    let forecast = match cell.get("forecastRiskExpectancy") {
        Some(value) => value,
        None if mode == Some("forecast") => cell.get("riskExpectancy")?,
        None => return None,
    };
    let pair = ForecastBestPair {
        forecast_r: finite(forecast)?,
        mean_r: finite(cell.get("meanRiskExpectancy")?)?,
        tp_pct: finite(cell.get("tpPct")?)?,
        sl_pct: finite(cell.get("slPct")?)?,
    };
    (pair.tp_pct > 0.0 && pair.tp_pct <= 100.0 && pair.sl_pct > 0.0 && pair.sl_pct <= 100.0)
        .then_some(pair)
}

/// Read-only projection of the frozen winner, not a new optimization or fit.
/// O(matrix) time, O(1) scratch; ties keep the first persisted cell.
pub fn forecast_best_pair(result: Option<&Value>, spec: &Value) -> Option<ForecastBestPair> {
    let best = result?.get("best")?;
    let mode = forecast_mode(best, spec);
    best.get("matrix")?
        .as_array()?
        .iter()
        .filter_map(|cell| forecast_pair(cell, mode))
        .fold(None::<ForecastBestPair>, |winner, pair| match winner {
            Some(current) if current.forecast_r >= pair.forecast_r => Some(current),
            _ => Some(pair),
        })
}

/// Detail-only scalar projection of the persisted selectedCell. `meanR` here
/// means the pair's period mean (rawR), never selectedCell.meanR (matrix mean).
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecommendedPair {
    pub tp_pct: f64,
    pub sl_pct: f64,
    pub mean_r: f64,
    pub forecast_r: Option<f64>,
    pub decision_r: Option<f64>,
    /// Original generation admission, not the mutable current profile status.
    pub adaptive_accepted: Option<bool>,
}

fn recommended_pair(result: &Value, spec: &Value) -> Result<Option<RecommendedPair>, DbError> {
    let invalid = || DbError::Json("recommended_pair_identity_mismatch".into());
    let Some(cell) = result.get("selectedCell").filter(|value| !value.is_null()) else {
        return Ok(None); // No persisted choice: do not select a new legacy winner.
    };
    let finite = |value: &Value| value.as_f64().filter(|value| value.is_finite());
    let optional = |key: &str| -> Result<Option<f64>, DbError> {
        match cell.get(key).filter(|value| !value.is_null()) {
            None => Ok(None),
            Some(value) => finite(value).map(Some).ok_or_else(invalid),
        }
    };
    let tp_pct = finite(&cell["tpPct"]).ok_or_else(invalid)?;
    let sl_pct = finite(&cell["slPct"]).ok_or_else(invalid)?;
    let mean_r = finite(&cell["rawR"]).ok_or_else(invalid)?;
    if !(0.0 < tp_pct && tp_pct <= 100.0 && 0.0 < sl_pct && sl_pct <= 100.0) {
        return Err(invalid());
    }
    let forecast_r = optional("forecastR")?;
    let blend_r = optional("blendR")?;
    let mode = spec
        .get("trendMode")
        .and_then(Value::as_str)
        .unwrap_or("mean");
    let decision_r = match mode {
        "mean" => Some(mean_r),
        "forecast" => forecast_r,
        "blend" => blend_r,
        _ => return Err(invalid()),
    };
    if let Some(handoff) = result
        .get("recommendation")
        .filter(|value| !value.is_null())
    {
        if finite(&handoff["tpPct"]) != Some(tp_pct)
            || finite(&handoff["slPct"]) != Some(sl_pct)
            || finite(&handoff["rawR"]) != Some(mean_r)
            || finite(&handoff["searchForecast"]["cellR"]) != forecast_r
            || finite(&handoff["searchForecast"]["blend"]["cellR"]) != blend_r
        {
            return Err(invalid());
        }
    }
    Ok(Some(RecommendedPair {
        tp_pct,
        sl_pct,
        mean_r,
        forecast_r,
        decision_r,
        adaptive_accepted: None,
    }))
}

/// Read only the exact original generation, not current profile pointers. The
/// SQL projection retains no matrix/candles; scratch lives for this request.
pub async fn get_recommended_pair(
    pool: &PgPool,
    study: &StudyRecord,
) -> Result<Option<RecommendedPair>, DbError> {
    let Some(result) = study.result.as_ref() else {
        return Ok(None);
    };
    let Some(generation) = study.continuous_generation_id else {
        return recommended_pair(result, &study.spec);
    };
    let row = sqlx::query("select g.accepted, g.spec=$4 and g.result->'best'=$5 as same_source, jsonb_build_object('selectedCell',g.result->'selectedCell','recommendation',g.result->'recommendation') as selection from continuous_generations g where g.id=$1 and g.study_id=$2 and g.owner_id=$3 and g.status='completed'")
        .bind(generation).bind(study.id).bind(&study.owner_id).bind(&study.spec).bind(&result["best"])
        .fetch_optional(pool).await?;
    let Some(row) = row else {
        return Ok(None);
    };
    if row.try_get::<Option<bool>, _>("same_source")? != Some(true) {
        return Err(DbError::Json(
            "recommended_pair_frozen_source_mismatch".into(),
        ));
    }
    let mut pair = recommended_pair(&row.try_get::<Value, _>("selection")?, &study.spec)?;
    if let Some(pair) = &mut pair {
        pair.adaptive_accepted = row.try_get("accepted")?;
    }
    Ok(pair)
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StudySummary {
    pub id: i64,
    pub status: String,
    pub spec: Value,
    pub progress: Value,
    pub error: Option<Value>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub best_score: Option<f64>,
    pub forecast_best: Option<ForecastBestPair>,
    pub origin: String,
    pub owner_id: String,
    pub continuous_generation_id: Option<i64>,
    pub continuous_slot_id: Option<i64>,
    pub actual: Option<Value>,
    pub current_presentation: Value,
    pub manual_control: Option<ManualControl>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StudyPage {
    pub studies: Vec<StudySummary>,
    pub total: i64,
    pub limit: i64,
    pub offset: i64,
}

pub async fn list(
    pool: &PgPool,
    algorithm: Option<&str>,
    limit: i64,
    offset: i64,
) -> Result<StudyPage, DbError> {
    let limit = limit.clamp(1, 100);
    let offset = offset.max(0);
    let mut tx = pool.begin().await?;
    // Count and page must describe the same history even during admission/deletion.
    sqlx::query("set transaction isolation level repeatable read, read only")
        .execute(&mut *tx)
        .await?;
    let total = sqlx::query_scalar(
        "select count(*) from optimization_studies where ($1::text is null or algorithm_id=$1)",
    )
    .bind(algorithm)
    .fetch_one(&mut *tx)
    .await?;
    // Page BEFORE inspecting matrices. PostgreSQL returns at most one cell per
    // row, never full result matrices/events to API memory (at most 100 rows).
    // TODO(study_actual): remove the redundant idle-eligibility CASE/version
    // binds below during integration cleanup. The API bulk summary projection
    // is authoritative and replaces this bounded metadata; keeping it for the
    // current handoff does not change client eligibility or add SQL round-trips.
    let rows = sqlx::query(&[
        "select page.id,page.status,page.spec,page.progress,page.error,page.created_at,page.updated_at, \
         page.origin,page.owner_id,page.continuous_generation_id,page.continuous_slot_id,page.manual_format_version,page.manual_control_revision, \
         (case when page.status in ('queued','running','pausing','paused','cancel_requested') \
          then page.progress->>'bestScore' else page.result#>>'{best,score}' end)::double precision as best_score, \
         coalesce(page.result#>'{best,trendMode}',page.spec->'trendMode') as forecast_mode, \
         winner.cell as best_forecast_cell, \
         (select jsonb_build_object('snapshot',cp.snapshot,'error',cp.error) from study_current_presentations cp where cp.study_id=page.id and cp.owner_id=page.owner_id) current_presentation, \
          coalesce((select jsonb_build_object('study_id',page.id,'job_id',a.job_id,'request_origin',a.request_origin,'revision',a.revision,'status',a.status,'stage',a.stage,'totals',a.totals,'observed_through_unix',a.observed_through_unix,'error',a.error,'last_setup_close_unix',",
        crate::study_actual::LAST_SETUP_CLOSE_UNIX_SQL,
        ") from study_actualizations a where a.owner_id=page.owner_id and a.study_id=page.id), \
          jsonb_build_object('study_id',page.id,'status','not_calculated','eligibility_reason',case \
           when page.status not in ('completed','cancelled','failed') then 'study_active' \
           when jsonb_typeof(page.result->'best') is distinct from 'object' then 'winner_missing' \
           when not coalesce(((page.algorithm_id=$4 and page.spec->>'algorithmVersion'=$5) or (page.algorithm_id=$6 and page.spec->>'algorithmVersion'=$7)),false) then 'algorithm_version_unavailable' \
           when page.execution_started_at is null and not exists(select 1 from continuous_generations g where g.id=page.continuous_generation_id and g.owner_id=page.owner_id) \
            and not exists(select 1 from jsonb_array_elements(case when jsonb_typeof(page.events)='array' then page.events else '[]'::jsonb end) e where e->>'kind'='started' and jsonb_typeof(e->'at')='string') then 'execution_start_unavailable' \
           when not coalesce((jsonb_typeof(page.result#>'{best,params}')='object' and jsonb_typeof(page.result#>'{best,analytics}')='object' \
            and jsonb_typeof(page.result#>'{best,matrix}')='array' and page.spec->>'direction' in ('short','long') \
            and jsonb_typeof(page.spec->'instruments')='array' and page.spec->'instruments'<>'[]'::jsonb),false) then 'trade_conditions_unavailable' \
           else null end)) as actual \
         from (select * from optimization_studies where ($1::text is null or algorithm_id=$1) \
          order by (status in ('queued','running','pausing','paused','cancel_requested')) desc, id desc limit $2 offset $3) page \
         left join lateral ( \
          select entry.cell from jsonb_array_elements(case when jsonb_typeof(page.result#>'{best,matrix}')='array' \
           then page.result#>'{best,matrix}' else '[]'::jsonb end) with ordinality entry(cell,position) \
          cross join lateral (select case when entry.cell ? 'forecastRiskExpectancy' then entry.cell->'forecastRiskExpectancy' \
           when coalesce(page.result#>'{best,trendMode}',page.spec->'trendMode')='\"forecast\"'::jsonb \
           then entry.cell->'riskExpectancy' end as forecast) metric \
          where case when jsonb_typeof(metric.forecast)='number' then abs((metric.forecast#>>'{}')::numeric) <= 1.7976931348623157e308 else false end \
           and case when jsonb_typeof(entry.cell->'meanRiskExpectancy')='number' \
            then abs((entry.cell->>'meanRiskExpectancy')::numeric) <= 1.7976931348623157e308 else false end \
           and case when jsonb_typeof(entry.cell->'tpPct')='number' then (entry.cell->>'tpPct')::numeric > 0 \
            and (entry.cell->>'tpPct')::numeric <= 100 else false end \
           and case when jsonb_typeof(entry.cell->'slPct')='number' then (entry.cell->>'slPct')::numeric > 0 \
            and (entry.cell->>'slPct')::numeric <= 100 else false end \
          order by (metric.forecast#>>'{}')::numeric desc,entry.position limit 1 \
         ) winner on true \
         order by (page.status in ('queued','running','pausing','paused','cancel_requested')) desc,page.id desc",
    ].concat())
    .bind(algorithm)
    .bind(limit)
    .bind(offset)
    .bind(setups_domain::SPIKE_ALGORITHM_ID).bind(setups_domain::SPIKE_ALGORITHM_VERSION)
    .bind(setups_domain::SPIKE_DROP_ALGORITHM_ID).bind(setups_domain::SPIKE_DROP_ALGORITHM_VERSION)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    let studies = rows
        .into_iter()
        .map(|row| {
            let cell: Option<Value> = row.try_get("best_forecast_cell")?;
            let mode: Option<Value> = row.try_get("forecast_mode")?;
            Ok(StudySummary {
                manual_control: manual_control(&row)?,
                id: row.try_get("id")?,
                status: row.try_get("status")?,
                spec: row.try_get("spec")?,
                progress: row.try_get("progress")?,
                error: row.try_get("error")?,
                created_at: row.try_get("created_at")?,
                updated_at: row.try_get("updated_at")?,
                best_score: row.try_get("best_score")?,
                forecast_best: cell
                    .as_ref()
                    .and_then(|cell| forecast_pair(cell, mode.as_ref().and_then(Value::as_str))),
                origin: row.try_get("origin")?,
                owner_id: row.try_get("owner_id")?,
                continuous_generation_id: row.try_get("continuous_generation_id")?,
                continuous_slot_id: row.try_get("continuous_slot_id")?,
                actual: Some(crate::study_actual::compact_summary(
                    &row.try_get::<Value, _>("actual")?,
                )),
                current_presentation: {
                    let saved:Option<Value>=row.try_get("current_presentation")?;
                    match saved {Some(saved)=>crate::study_presentation::metadata(saved.get("snapshot").filter(|v|!v.is_null()).cloned(),saved["error"].as_str().map(str::to_owned)),
                        None=>crate::study_presentation::unavailable("study_presentation_not_initialized")}
                },
            })
        })
        .collect::<Result<_, DbError>>()?;
    Ok(StudyPage {
        studies,
        total,
        limit,
        offset,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteStudyOutcome {
    Deleted,
    Active,
    Referenced,
}

pub async fn delete(pool: &PgPool, id: i64) -> Result<Option<DeleteStudyOutcome>, DbError> {
    let mut tx = pool.begin().await?;
    let owner: Option<String> = sqlx::query_scalar("select owner_id from optimization_studies where id=$1")
        .bind(id).fetch_optional(&mut *tx).await?;
    let Some(owner) = owner else { return Ok(None); };
    crate::continuous::owner_lock(&mut tx, &owner).await?;
    let head = sqlx::query(
        "select status,owner_id,origin from optimization_studies where id=$1 for no key update",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(head) = head else {
        tx.commit().await?;
        return Ok(None);
    };
    let status: String = head.try_get("status")?;
    let owner: String = head.try_get("owner_id")?;
    if !matches!(status.as_str(), "completed" | "cancelled" | "failed")
        && !(status == "queued" && head.get::<String, _>("origin") == "manual") {
        tx.commit().await?;
        return Ok(Some(DeleteStudyOutcome::Active));
    }
    if sqlx::query_scalar::<_, bool>(
        "select exists(select 1 from continuous_generations where study_id=$1)",
    )
    .bind(id)
    .fetch_one(&mut *tx)
    .await?
    {
        return Ok(Some(DeleteStudyOutcome::Referenced));
    }
    if !crate::study_actual::cleanup_for_study_delete(&mut tx, &owner, id).await? {
        return Ok(Some(DeleteStudyOutcome::Referenced));
    }
    sqlx::query("delete from optimization_studies where id=$1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Some(DeleteStudyOutcome::Deleted))
}

pub async fn claim(pool: &PgPool) -> Result<Option<StudyRecord>, DbError> {
    Ok(manual::claim(pool, None).await?.map(|job| job.study))
}

pub async fn cancel(pool: &PgPool, id: i64) -> Result<Option<(StudyRecord, bool)>, DbError> {
    manual::cancel(pool, id).await
}

#[allow(dead_code)]
async fn legacy_cancel(pool: &PgPool, id: i64) -> Result<Option<(StudyRecord, bool)>, DbError> {
    // Queued work has no retained input or admitted CPU work. Running work keeps
    // its active slot until its owner drains and calls finish.
    let mut tx = pool.begin().await?;
    if let Some(row) = sqlx::query(
        "select * from optimization_studies where id=$1 and origin='continuous' for update",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?
    {
        return Ok(Some((record(row)?, false)));
    }
    let previous: Option<String> =
        sqlx::query_scalar("select status from optimization_studies where id=$1 for update")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
    let Some(previous) = previous else {
        tx.commit().await?;
        return Ok(None);
    };
    let accepted = matches!(previous.as_str(), "queued" | "running" | "cancel_requested");
    let row = sqlx::query(
        "update optimization_studies set \
         status=case when status='queued' then 'cancelled' when status='running' then 'cancel_requested' else status end, \
         updated_at=case when status in ('queued','running') then now() else updated_at end \
         where id=$1 returning *",
    ).bind(id).fetch_one(&mut *tx).await?;
    tx.commit().await?;
    Ok(Some((record(row)?, accepted)))
}

pub async fn save_progress(
    pool: &PgPool,
    id: i64,
    progress: &Value,
    events: &Value,
) -> Result<(), DbError> {
    sqlx::query(
        "update optimization_studies set progress=$2,events=$3,updated_at=now() \
         where id=$1 and status in ('running','cancel_requested')",
    )
    .bind(id)
    .bind(progress)
    .bind(events)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn cancellation_requested(pool: &PgPool, id: i64) -> Result<bool, DbError> {
    let status =
        sqlx::query_scalar::<_, String>("select status from optimization_studies where id=$1")
            .bind(id)
            .fetch_one(pool)
            .await?;
    match status.as_str() {
        "running" => Ok(false),
        "cancel_requested" => Ok(true),
        _ => Err(DbError::Json(
            "optimization study ownership changed during execution".into(),
        )),
    }
}

pub async fn finish(
    pool: &PgPool,
    id: i64,
    status: &str,
    progress: &Value,
    result: Option<&Value>,
    error: Option<&Value>,
    events: &Value,
) -> Result<(), DbError> {
    finish_with_original_archive(pool, id, status, progress, result, error, events, None).await
}

/// New manual runtime path. Terminal science and the separate bounded archive
/// commit together. Old callers retain their historical no-archive behavior.
#[allow(clippy::too_many_arguments)]
pub async fn finish_with_original_archive(
    pool: &PgPool,
    id: i64,
    status: &str,
    progress: &Value,
    result: Option<&Value>,
    error: Option<&Value>,
    events: &Value,
    archive: Option<&crate::study_display::ManualArchive>,
) -> Result<(), DbError> {
    finish_fenced(pool,id,None,status,progress,result,error,events,archive).await
}

#[allow(clippy::too_many_arguments)]
pub async fn finish_fenced(
    pool:&PgPool,id:i64,epoch:Option<i64>,status:&str,progress:&Value,
    result:Option<&Value>,error:Option<&Value>,events:&Value,
    archive:Option<&crate::study_display::ManualArchive>,
) -> Result<(),DbError> {
    assert!(matches!(status, "completed" | "cancelled" | "failed"));
    let mut tx = pool.begin().await?;
    let owner:Option<String>=sqlx::query_scalar("select owner_id from optimization_studies where id=$1").bind(id).fetch_optional(&mut *tx).await?;
    let Some(owner)=owner else {return Err(DbError::Json("manual study missing during finalization".into()));};
    crate::continuous::owner_lock(&mut tx,&owner).await?;
    if let Some(epoch)=epoch {
        let current:i64=sqlx::query_scalar("select manual_execution_epoch from optimization_studies where id=$1 for update").bind(id).fetch_one(&mut *tx).await?;
        if current!=epoch {return Err(DbError::Json("manual execution epoch lost".into()));}
    }
    let previous: Option<String> =
        sqlx::query_scalar("select status from optimization_studies where id=$1 for update")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
    if !previous
        .as_deref()
        .is_some_and(|status| matches!(status, "running" | "pausing" | "cancel_requested"))
    {
        if archive.is_some() {
            // A lost COMMIT ACK must verify what was committed, not turn an
            // already-terminal study into an unconditional default-success.
            let mut expected = result.cloned();
            if previous.as_deref() == Some("cancelled")
                && let Some(value) = &mut expected
            {
                value["reason"] = json!("cancelled");
                if value.get("best").is_some_and(Value::is_object) {
                    value["best"]["partial"] = json!(true);
                }
            }
            // JSONB normalizes numeric notation; compare in its value domain,
            // not arbitrary_precision Number strings decoded back into Rust.
            let same:bool=sqlx::query_scalar("select result is not distinct from $2::jsonb from optimization_studies where id=$1")
                .bind(id).bind(&expected).fetch_one(&mut *tx).await?;
            if !same {
                return Err(DbError::Json(
                    "study_display_manual_terminal_conflict".into(),
                ));
            }
            crate::study_display::seal_manual_in(
                &mut tx,
                id,
                archive.expect("archive presence checked"),
            )
            .await?;
        }
        tx.commit().await?;
        return Ok(());
    }
    let cancelled = previous.as_deref() == Some("cancel_requested") && status == "completed";
    let status = if cancelled { "cancelled" } else { status };
    let mut result = result.cloned();
    if cancelled && let Some(result) = &mut result {
        result["reason"] = json!("cancelled");
        if result.get("best").is_some_and(Value::is_object) {
            result["best"]["partial"] = json!(true);
        }
    }
    let mut events = events.clone();
    if cancelled && let Some(events) = events.as_array_mut() {
        for event in events
            .iter_mut()
            .filter(|event| event["kind"] == "completed")
        {
            event["kind"] = json!("cancelled");
            event["message"] =
                json!("Исследование остановлено; сохранён только завершённый результат");
        }
    }
    sqlx::query(
        "update optimization_studies set status=$2, \
         progress=$3,result=$4,error=$5,events=$6,manual_execution_reserved=false, \
         manual_spent_ms=manual_spent_ms+greatest(0,coalesce((extract(epoch from(clock_timestamp()-manual_segment_started_at))*1000)::bigint,0)), \
         manual_segment_started_at=null,updated_at=now() \
         where id=$1 and status in ('running','pausing','cancel_requested')",
    )
    .bind(id)
    .bind(status)
    .bind(progress)
    .bind(&result)
    .bind(error)
    .bind(&events)
    .execute(&mut *tx)
    .await?;
    if let Some(archive) = archive {
        crate::study_display::seal_manual_in(&mut tx, id, archive).await?;
    }
    // Private state is detached only AFTER science + paired original archive
    // have succeeded in this same transaction, and callers have joined work.
    sqlx::query("delete from manual_runtime_artifacts where study_id=$1").bind(id).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

/// Call only while owning RUNTIME_LOCK. The previous process's RAM is gone.
pub async fn interrupt(pool: &PgPool) -> Result<u64, DbError> {
    manual::recover(pool).await
}

#[cfg(test)]
mod forecast_tests {
    use super::*;

    fn cell(forecast: f64, mean: f64, tp: f64) -> Value {
        json!({"forecastRiskExpectancy":forecast,"meanRiskExpectancy":mean,
            "riskExpectancy":99,"tpPct":tp,"slPct":4})
    }

    #[test]
    fn recommended_pair_projects_selected_handoff_not_forecast_max_or_matrix_mean() {
        let result = json!({"selectedCell":{"tpPct":20,"slPct":6,"rawR":0.42,"meanR":0.1,
            "forecastR":1.02,"blendR":0.84,"robustScore":0.7},
            "recommendation":{"tpPct":20,"slPct":6,"rawR":0.42,
                "searchForecast":{"cellR":1.02,"blend":{"cellR":0.84}}}});
        let before = result.clone();
        let pair = recommended_pair(&result, &json!({"trendMode":"blend"})).unwrap().unwrap();
        assert_eq!(json!(pair), json!({"tpPct":20.0,"slPct":6.0,"meanR":0.42,"forecastR":1.02,"decisionR":0.84,"adaptiveAccepted":null}));
        assert_eq!(result, before);
        let mut mismatched = result.clone();
        mismatched["recommendation"]["tpPct"] = json!(18);
        assert!(recommended_pair(&mismatched, &json!({"trendMode":"blend"})).is_err());
        mismatched = result.clone();
        mismatched["recommendation"]["rawR"] = json!(0.53);
        assert!(recommended_pair(&mismatched, &json!({"trendMode":"blend"})).is_err());
    }

    #[test]
    fn recommended_pair_preserves_missing_legacy_metrics_and_mode_specific_decision() {
        assert!(recommended_pair(&json!({"best":{"matrix":[cell(1.08,0.53,18.0)]}}), &json!({})).unwrap().is_none());
        for (mode, expected) in [("mean",Some(-0.2)),("forecast",Some(0.0)),("blend",None)] {
            let pair = recommended_pair(&json!({"selectedCell":{"tpPct":20,"slPct":6,"rawR":-0.2,"forecastR":0}}),
                &json!({"trendMode":mode})).unwrap().unwrap();
            assert_eq!(pair.mean_r, -0.2);
            assert_eq!(pair.forecast_r, Some(0.0));
            assert_eq!(pair.decision_r, expected);
        }
        let pair = recommended_pair(&json!({"selectedCell":{"tpPct":20,"slPct":6,"rawR":0.42}}), &json!({"trendMode":"forecast"})).unwrap().unwrap();
        assert_eq!((pair.forecast_r,pair.decision_r),(None,None));
        assert!(recommended_pair(&json!({"selectedCell":{"tpPct":0,"slPct":6,"rawR":0.42}}),&json!({})).is_err());
    }

    #[test]
    fn forecast_best_keeps_mean_and_levels_of_the_maximum_pure_forecast() {
        let result = json!({"best":{"score":99,"trendMode":"blend","matrix":[
            cell(0.2, 5.0, 4.0),cell(0.30001, -0.7, 6.0),cell(0.3, 9.0, 8.0)
        ]}});
        let before = result.clone();
        assert_eq!(
            forecast_best_pair(Some(&result), &json!({})),
            Some(ForecastBestPair {
                forecast_r: 0.30001,
                mean_r: -0.7,
                tp_pct: 6.0,
                sl_pct: 4.0,
            })
        );
        assert_eq!(result, before, "projection must not rewrite saved results");
    }

    #[test]
    fn forecast_best_handles_negative_zero_and_ties_without_rounding() {
        for (values, expected, tp) in [
            (vec![cell(-0.8, 1.0, 4.0), cell(-0.2, -3.0, 6.0)], -0.2, 6.0),
            (vec![cell(-0.1, 1.0, 4.0), cell(0.0, -3.0, 6.0)], 0.0, 6.0),
            (vec![cell(0.5, 1.0, 4.0), cell(0.5, -3.0, 6.0)], 0.5, 4.0),
        ] {
            let pair =
                forecast_best_pair(Some(&json!({"best":{"matrix":values}})), &json!({})).unwrap();
            assert_eq!((pair.forecast_r, pair.tp_pct), (expected, tp));
        }
    }

    #[test]
    fn n6_forecast_uses_frozen_mode_but_never_mistakes_blend_or_mean_for_forecast() {
        let mut result = json!({"best":{"matrix":[{"riskExpectancy":0.5,
            "meanRiskExpectancy":-0.3,"tpPct":4,"slPct":6}]}});
        assert!(forecast_best_pair(Some(&result), &json!({})).is_none());
        assert!(forecast_best_pair(Some(&result), &json!({"trendMode":"blend"})).is_none());
        assert_eq!(
            forecast_best_pair(Some(&result), &json!({"trendMode":"forecast"}))
                .unwrap()
                .forecast_r,
            0.5
        );
        result["best"]["trendMode"] = json!("mean");
        assert!(forecast_best_pair(Some(&result), &json!({"trendMode":"forecast"})).is_none());
        result["best"]["trendMode"] = json!("forecast");
        assert_eq!(
            forecast_best_pair(Some(&result), &json!({"trendMode":"mean"}))
                .unwrap()
                .mean_r,
            -0.3
        );
        result["best"]["matrix"][0]["forecastRiskExpectancy"] = Value::Null;
        assert!(
            forecast_best_pair(Some(&result), &json!({})).is_none(),
            "explicit null is not an absent field"
        );
    }

    #[test]
    fn legacy_empty_missing_and_incomplete_results_have_no_invented_forecast() {
        assert!(forecast_best_pair(None, &json!({})).is_none());
        for result in [
            Value::Null,
            json!({"best":null}),
            json!({"best":{"matrix":[]}}),
            json!({"best":{"matrix":{}}}),
            json!({"best":{"matrix":[{"tpPct":4,"slPct":4,"riskExpectancy":9}]}}),
        ] {
            assert!(forecast_best_pair(Some(&result), &json!({})).is_none());
        }
        for (field, value) in [
            ("forecastRiskExpectancy", json!("NaN")),
            ("meanRiskExpectancy", Value::Null),
            ("meanRiskExpectancy", json!("Infinity")),
            ("tpPct", json!(0)),
            ("slPct", json!(101)),
        ] {
            let mut invalid = cell(2.0, 3.0, 4.0);
            invalid[field] = value;
            assert!(
                forecast_best_pair(Some(&json!({"best":{"matrix":[invalid]}})), &json!({}))
                    .is_none()
            );
        }
    }
}
