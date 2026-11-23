//! Durable continuous ownership. All mutating transitions fence by owner/revision/lease.
pub mod cleanup;
pub mod frozen;
pub mod live;
pub mod maintenance;
pub mod outbox;
pub mod profiles;
pub mod readmission;
pub mod recovery;
pub mod scheduler;
use crate::DbError;
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::{Value, json};
use sqlx::{PgPool, Postgres, Row, Transaction, postgres::PgRow};

pub const ADMIN_OWNER: &str = "admin";

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContinuousSlot {
    pub id: i64,
    pub min_setups_per_day: f64,
    pub status: String,
    pub active_generation_id: Option<i64>,
    pub active_status: String,
    pub pending_generation_id: Option<i64>,
    pub pending_status: Option<String>,
    pub readiness_frontier: Option<i64>,
    pub pending_error: Option<Value>,
    pub live_watermark: Option<i64>,
    pub live_initialization: Option<live::LiveInitialization>,
    pub live_readiness_summary: Option<live::LiveReadinessSummary>,
    pub pending_initialization: Option<live::LiveInitialization>,
    pub pending_attempts: i32,
    pub pending_next_attempt_at: Option<DateTime<Utc>>,
    pub pending_attempt_deadline: Option<DateTime<Utc>>,
    /// Research ownership is separate from finalized-result and live provenance.
    pub running_study_id: Option<i64>,
    pub finalized_generation_id: Option<i64>,
    pub active_study_id: Option<i64>,
    pub pending_study_id: Option<i64>,
    /// Last fully finalized continuous result, never merely a completed search.
    pub study_id: Option<i64>,
    pub mean_expectancy: Option<f64>,
    /// Objective of the saved matrix, never the currently editable process config.
    pub trend_mode: Option<setups_domain::trend::TrendMode>,
    pub trend_blend_lambda: Option<setups_domain::trend::TrendBlendLambda>,
    pub selected_cell: Option<Value>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContinuousProcess {
    pub owner_id: String,
    pub algorithm_id: String,
    pub status: String,
    pub config_revision: i64,
    pub config: Value,
    pub pending_config: Option<Value>,
    pub active_study_id: Option<i64>,
    pub phase: Option<String>,
    pub error: Option<Value>,
    pub slots: Vec<ContinuousSlot>,
}

fn process(row: &PgRow) -> Result<ContinuousProcess, DbError> {
    Ok(ContinuousProcess {
        owner_id: row.try_get("owner_id")?,
        algorithm_id: row.try_get("algorithm_id")?,
        status: row.try_get("status")?,
        config_revision: row.try_get("config_revision")?,
        config: row.try_get("config")?,
        pending_config: row.try_get("pending_config")?,
        active_study_id: row.try_get("active_study_id")?,
        phase: row.try_get("phase")?,
        error: row.try_get("error")?,
        slots: Vec::new(),
    })
}

fn saved_trend_mode(spec: Option<&Value>) -> Option<setups_domain::trend::TrendMode> {
    use setups_domain::trend::TrendMode;
    match spec?.as_object()?.get("trendMode") {
        // Before N6 this persisted study path could only optimize period means.
        None => Some(TrendMode::Mean),
        Some(value) => match value.as_str() {
            Some("mean") => Some(TrendMode::Mean),
            Some("forecast") => Some(TrendMode::Forecast),
            Some("blend") => Some(TrendMode::Blend),
            _ => None,
        },
    }
}

fn saved_blend_lambda(spec: Option<&Value>) -> Option<setups_domain::trend::TrendBlendLambda> {
    if saved_trend_mode(spec) != Some(setups_domain::trend::TrendMode::Blend) {
        return None;
    }
    match spec?.get("trendBlendLambda") {
        None => Some(Default::default()),
        Some(value) => value.as_f64().and_then(|value| value.try_into().ok()),
    }
}

pub async fn get(
    pool: &PgPool,
    owner: &str,
    algorithm: &str,
) -> Result<Option<ContinuousProcess>, DbError> {
    let mut tx = pool.begin().await?;
    sqlx::query("set transaction isolation level repeatable read, read only")
        .execute(&mut *tx)
        .await?;
    let row =
        sqlx::query("select * from continuous_processes where owner_id=$1 and algorithm_id=$2")
            .bind(owner)
            .bind(algorithm)
            .fetch_optional(&mut *tx)
            .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let mut result = process(&row)?;
    // Repair legacy display bindings at read time: prepare_next used to replace
    // s.study_id before post-chain/gate completed. Never trust that cached field.
    let slots = sqlx::query(r#"select s.*,s.min_setups_per_day::double precision as frequency,
        a.spec active_spec,coalesce(a.spec,m.spec) metric_spec,
        a.live_initialization,coalesce(a.live_readiness,'[]'::jsonb) live_readiness,
        a.study_id active_study_id,n.study_id pending_study_id,r.study_id running_study_id,
        f.id finalized_generation_id,f.study_id finalized_study_id,
        n.live_initialization pending_initialization,coalesce(n.live_init_attempts,0) pending_attempts,
        n.live_init_next_attempt_at pending_next_attempt_at,n.live_init_deadline pending_attempt_deadline
        from continuous_slots s
        join continuous_processes p on p.owner_id=s.owner_id and p.algorithm_id=s.algorithm_id
        left join continuous_generations a on a.id=s.active_generation_id
        left join continuous_generations n on n.id=s.pending_generation_id
        left join continuous_generations r on r.id=p.active_generation_id and r.slot_id=s.id and r.status='running'
        left join lateral (select g.id,g.study_id from continuous_generations g
            where g.owner_id=s.owner_id and g.algorithm_id=s.algorithm_id and g.slot_id=s.id
                and g.status='completed' order by g.id desc limit 1) f on true
        left join lateral (select g.spec from continuous_generations g
            where g.owner_id=s.owner_id and g.algorithm_id=s.algorithm_id and g.slot_id=s.id
                and g.status='completed' and g.result->'meanExpectancy'=to_jsonb(s.mean_expectancy)
                and g.result->'selectedCell' is not distinct from s.selected_cell
            order by g.id desc limit 1) m on true
        where s.owner_id=$1 and s.algorithm_id=$2 and s.enabled order by s.id"#)
        .bind(owner).bind(algorithm).fetch_all(&mut *tx).await?;
    result.slots = slots
        .into_iter()
        .map(|row| {
            Ok(ContinuousSlot {
                id: row.try_get("id")?,
                min_setups_per_day: row.try_get("frequency")?,
                status: row.try_get("status")?,
                active_generation_id: row.try_get("active_generation_id")?,
                active_status: if row
                    .try_get::<Option<i64>, _>("active_generation_id")?
                    .is_some()
                {
                    "active"
                } else {
                    "inactive"
                }
                .into(),
                pending_generation_id: row.try_get("pending_generation_id")?,
                pending_status: row.try_get("pending_status")?,
                readiness_frontier: row.try_get("readiness_frontier")?,
                pending_error: row.try_get("pending_error")?,
                live_watermark: row.try_get("live_watermark")?,
                live_initialization: live::decode_live_metadata(&row)?.0,
                live_readiness_summary: row
                    .try_get::<Option<i64>, _>("active_generation_id")?
                    .map(|generation| {
                        live::readiness_summary(
                            generation,
                            &row.try_get::<Value, _>("active_spec")?,
                            &live::decode_live_metadata(&row)?.1,
                        )
                    })
                    .transpose()?,
                pending_initialization: row
                    .try_get::<Option<Value>, _>("pending_initialization")?
                    .map(serde_json::from_value)
                    .transpose()
                    .map_err(crate::json_error)?,
                pending_attempts: row.try_get("pending_attempts")?,
                pending_next_attempt_at: row.try_get("pending_next_attempt_at")?,
                pending_attempt_deadline: row.try_get("pending_attempt_deadline")?,
                running_study_id: row.try_get("running_study_id")?,
                finalized_generation_id: row.try_get("finalized_generation_id")?,
                active_study_id: row.try_get("active_study_id")?,
                pending_study_id: row.try_get("pending_study_id")?,
                study_id: row.try_get("finalized_study_id")?,
                mean_expectancy: row.try_get("mean_expectancy")?,
                trend_mode: if row.try_get::<Option<f64>, _>("mean_expectancy")?.is_some() {
                    saved_trend_mode(row.try_get::<Option<Value>, _>("metric_spec")?.as_ref())
                } else {
                    None
                },
                trend_blend_lambda: if row.try_get::<Option<f64>, _>("mean_expectancy")?.is_some() {
                    saved_blend_lambda(row.try_get::<Option<Value>, _>("metric_spec")?.as_ref())
                } else {
                    None
                },
                selected_cell: row.try_get("selected_cell")?,
                updated_at: row.try_get("updated_at")?,
            })
        })
        .collect::<Result<_, DbError>>()?;
    tx.commit().await?;
    Ok(Some(result))
}

pub async fn list(
    pool: &PgPool,
    owner: &str,
    algorithm: Option<&str>,
) -> Result<Vec<ContinuousProcess>, DbError> {
    // Registered algorithms are bounded by API metadata, not user-created arbitrary keys.
    let ids: Vec<String> = sqlx::query_scalar("select algorithm_id from continuous_processes where owner_id=$1 and ($2::text is null or algorithm_id=$2) order by algorithm_id")
        .bind(owner).bind(algorithm).fetch_all(pool).await?;
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        if let Some(p) = get(pool, owner, &id).await? {
            out.push(p);
        }
    }
    Ok(out)
}

pub(crate) async fn owner_lock(
    tx: &mut Transaction<'_, Postgres>,
    owner: &str,
) -> Result<(), DbError> {
    // Domain mutations and lease renewal deliberately use different locks.
    // Takeover must acquire this SAME advisory lock before changing its token.
    sqlx::query("select pg_advisory_xact_lock(hashtextextended($1,72423415))")
        .bind(owner)
        .execute(&mut **tx)
        .await?;
    sqlx::query("insert into continuous_owner_lanes(owner_id) values($1) on conflict do nothing")
        .bind(owner)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// Research edits preserve running intent only under an owner-wide drained maintenance fence.
pub async fn configure(
    pool: &PgPool,
    owner: &str,
    algorithm: &str,
    config: &Value,
    thresholds: &[f64],
) -> Result<bool, DbError> {
    let mut tx = pool.begin().await?;
    owner_lock(&mut tx, owner).await?;
    let current = sqlx::query("select config,desired_running,status,active_generation_id,active_study_id from continuous_processes where owner_id=$1 and algorithm_id=$2 for update")
        .bind(owner).bind(algorithm).fetch_optional(&mut *tx).await?;
    let mut preserve_operational_state = false;
    if let Some(current) = current {
        let previous: Value = current.try_get("config")?;
        // Live permission is independent of research identity and accepted generation.
        if without_live_permission(&previous) == without_live_permission(config) {
            tx.commit().await?;
            return Ok(true);
        }
        if current.try_get::<String, _>("status")? == "stopping" {
            return Ok(false);
        }
        if current.try_get::<bool, _>("desired_running")? {
            // The owner mutation lock also serializes maintenance resume and
            // scheduler admission. Lock the durable row until this commit;
            // an absent row is not permission to edit a running process.
            let paused: Option<bool> = sqlx::query_scalar(
                "select admission_paused from continuous_maintenance where owner_id=$1 for update",
            )
            .bind(owner)
            .fetch_optional(&mut *tx)
            .await?;
            let idle = paused == Some(true)
                && current.try_get::<Option<i64>,_>("active_generation_id")?.is_none()
                && current.try_get::<Option<i64>,_>("active_study_id")?.is_none()
                && !sqlx::query_scalar("select exists(select 1 from continuous_generations where owner_id=$1 and status='running')")
                    .bind(owner).fetch_one(&mut *tx).await?;
            if !idle {
                // A chain may still be in flight: defer the save. The active
                // generation keeps its config/revision fences and finishes with
                // the parameters it was launched with; the pending config is
                // applied atomically when the generation ends or a stopped
                // process starts again.
                sqlx::query("update continuous_processes set pending_config=$3,updated_at=now() where owner_id=$1 and algorithm_id=$2")
                    .bind(owner).bind(algorithm).bind(config).execute(&mut *tx).await?;
                tx.commit().await?;
                return Ok(true);
            }
            preserve_operational_state = true;
        }
    }
    if preserve_operational_state {
        sqlx::query("update continuous_processes set config=$3,config_revision=config_revision+1,pending_config=null,updated_at=now() where owner_id=$1 and algorithm_id=$2")
            .bind(owner).bind(algorithm).bind(config).execute(&mut *tx).await?;
    } else {
        sqlx::query("insert into continuous_processes(owner_id,algorithm_id,config) values($1,$2,$3) on conflict(owner_id,algorithm_id) do update set config=excluded.config,config_revision=continuous_processes.config_revision+1,pending_config=null,status='stopped',phase=null,error=null,next_slot=0,updated_at=now()")
            .bind(owner).bind(algorithm).bind(config).execute(&mut *tx).await?;
    }
    sqlx::query("insert into adaptive_live_permissions(owner_id,algorithm_id,enabled) values($1,$2,false) on conflict(owner_id,algorithm_id) do nothing")
        .bind(owner).bind(algorithm).execute(&mut *tx).await?;
    sqlx::query("update continuous_processes set config=jsonb_set(config,'{adaptiveEnabled}',(select to_jsonb(enabled) from adaptive_live_permissions where owner_id=$1 and algorithm_id=$2)) where owner_id=$1 and algorithm_id=$2")
        .bind(owner).bind(algorithm).execute(&mut *tx).await?;
    sqlx::query("update continuous_slots set enabled=false where owner_id=$1 and algorithm_id=$2")
        .bind(owner)
        .bind(algorithm)
        .execute(&mut *tx)
        .await?;
    for frequency in thresholds {
        sqlx::query("insert into continuous_slots(owner_id,algorithm_id,min_setups_per_day) values($1,$2,$3::text::numeric) on conflict(owner_id,algorithm_id,min_setups_per_day) do update set enabled=true")
            .bind(owner).bind(algorithm).bind(frequency.to_string()).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(true)
}

pub async fn start(pool: &PgPool, owner: &str, algorithm: &str) -> Result<bool, DbError> {
    let mut tx = pool.begin().await?;
    owner_lock(&mut tx, owner).await?;
    if let Some(row)=sqlx::query("select desired_running,status from continuous_processes where owner_id=$1 and algorithm_id=$2 for update")
        .bind(owner).bind(algorithm).fetch_optional(&mut *tx).await?
        && row.try_get::<bool,_>("desired_running")? {
        let already_running=row.try_get::<String,_>("status")?=="running";
        tx.commit().await?;return Ok(already_running);
    }
    let changed = sqlx::query("update continuous_processes set desired_running=true,status='running',error=null,retry_count=0,next_attempt_at=now(),phase=coalesce(phase,'queued'),updated_at=now() where owner_id=$1 and algorithm_id=$2 and status<>'stopping' and not exists(select 1 from continuous_generations g where g.id=continuous_processes.active_generation_id and g.status='running' and continuous_processes.error->>'code'='durable_resume_retry_exhausted')")
        .bind(owner).bind(algorithm).execute(&mut *tx).await?.rows_affected()>0;
    // Research-only state; live control and retained detector cursors are independent.
    if changed {
        sqlx::query("update continuous_generations g set recovery_pending=false,error=null from continuous_processes p where p.owner_id=$1 and p.algorithm_id=$2 and p.active_generation_id=g.id and g.status='running'")
            .bind(owner).bind(algorithm).execute(&mut *tx).await?;
        scheduler::apply_pending_config(&mut tx, owner, algorithm).await?;
    }
    tx.commit().await?;
    Ok(changed)
}

/// Capture exact unfinished IDs BEFORE cancellation changes statuses. Never sweep terminal history.
pub async fn stop(pool: &PgPool, owner: &str, algorithm: &str) -> Result<bool, DbError> {
    let mut tx = pool.begin().await?;
    owner_lock(&mut tx, owner).await?;
    let row = sqlx::query(
        "select status from continuous_processes where owner_id=$1 and algorithm_id=$2 for update",
    )
    .bind(owner)
    .bind(algorithm)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(row) = row else {
        return Ok(false);
    };
    if row.try_get::<String, _>("status")? == "stopping" {
        tx.commit().await?;
        return Ok(true);
    }
    let studies: Vec<i64> = sqlx::query_scalar("select s.id from optimization_studies s join continuous_generations g on g.id=s.continuous_generation_id where s.owner_id=$1 and s.origin='continuous' and g.algorithm_id=$2 and s.status in ('queued','running','cancel_requested') for update of s")
        .bind(owner).bind(algorithm).fetch_all(&mut *tx).await?;
    let runs: Vec<i32> = sqlx::query_scalar("select r.id from replay_runs r join continuous_generations g on g.id=r.continuous_generation_id where r.owner_id=$1 and r.origin='continuous' and g.algorithm_id=$2 and r.status in ('queued','running','cancel_requested') for update of r")
        .bind(owner).bind(algorithm).fetch_all(&mut *tx).await?;
    let batches: Vec<i32> = sqlx::query_scalar("select b.id from replay_batches b join continuous_generations g on g.id=b.continuous_generation_id where b.owner_id=$1 and b.origin='continuous' and g.algorithm_id=$2 and g.status='running' and not exists(select 1 from replay_runs r where r.batch_id=b.id and r.status='done')")
        .bind(owner).bind(algorithm).fetch_all(&mut *tx).await?;
    sqlx::query("update continuous_processes set desired_running=false,status='stopping',config_revision=config_revision+1,phase='stopping',cleanup_targets=$3,updated_at=now() where owner_id=$1 and algorithm_id=$2")
        .bind(owner).bind(algorithm).bind(json!({"studies":studies,"runs":runs,"batches":batches})).execute(&mut *tx).await?;
    sqlx::query("update optimization_studies set status='cancel_requested',updated_at=now() where id=any($1)")
        .bind(&studies).execute(&mut *tx).await?;
    sqlx::query(
        "update replay_runs set status='cancel_requested',updated_at=now() where id=any($1)",
    )
    .bind(&runs)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(true)
}

/// Explicit recovery intent: never shared with the user's fresh Start.
pub async fn retry_recovery(pool: &PgPool, owner: &str, algorithm: &str) -> Result<bool, DbError> {
    let mut tx = pool.begin().await?;
    owner_lock(&mut tx, owner).await?;
    let changed=sqlx::query("update continuous_processes p set status='running',error=null,retry_count=0,next_attempt_at=now(),updated_at=now() where p.owner_id=$1 and p.algorithm_id=$2 and p.desired_running and p.error->>'code'='durable_resume_retry_exhausted' and exists(select 1 from continuous_generations g where g.id=p.active_generation_id and g.status='running' and g.resume_version=1)")
        .bind(owner).bind(algorithm).execute(&mut *tx).await?.rows_affected()==1;
    if changed {
        sqlx::query("update continuous_generations g set recovery_pending=true,error=null from continuous_processes p where p.owner_id=$1 and p.algorithm_id=$2 and p.active_generation_id=g.id")
        .bind(owner).bind(algorithm).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(changed)
}

/// Caller must have joined its in-flight CPU/I/O tasks, or acquired the expired owner's lease.
pub async fn cleanup_stopped(
    pool: &PgPool,
    owner: &str,
    algorithm: &str,
    lease: &scheduler::OwnerLease,
) -> Result<(), DbError> {
    cleanup_stopped_inner(pool, owner, algorithm, lease, true).await
}

async fn cleanup_stopped_inner(
    pool: &PgPool,
    owner: &str,
    algorithm: &str,
    lease: &scheduler::OwnerLease,
    wait: bool,
) -> Result<(), DbError> {
    if owner != lease.owner_id {
        return Ok(());
    }
    let mut tx = pool.begin().await?;
    if !wait {
        let free: bool =
            sqlx::query_scalar("select pg_try_advisory_xact_lock(hashtextextended($1,72423415))")
                .bind(owner)
                .fetch_one(&mut *tx)
                .await?;
        if !free {
            return Ok(());
        }
    }
    owner_lock(&mut tx, owner).await?;
    // Revalidate UNDER the mutation lock, BEFORE reading potentially newer Stop targets.
    if !scheduler::owns(&mut tx, lease).await? {
        return Ok(());
    }
    let row = sqlx::query("select cleanup_targets from continuous_processes where owner_id=$1 and algorithm_id=$2 and status='stopping' for update")
        .bind(owner).bind(algorithm).fetch_optional(&mut *tx).await?;
    let Some(row) = row else {
        return Ok(());
    };
    let targets: Value = row.try_get("cleanup_targets")?;
    let studies: Vec<i64> =
        serde_json::from_value(targets["studies"].clone()).map_err(crate::json_error)?;
    let runs: Vec<i32> =
        serde_json::from_value(targets["runs"].clone()).map_err(crate::json_error)?;
    let batches: Vec<i32> =
        serde_json::from_value(targets["batches"].clone()).map_err(crate::json_error)?;
    // Recheck successful completion after drain. Previous failed/cancelled rows were never selected.
    sqlx::query("delete from optimization_studies where id=any($1) and owner_id=$2 and origin='continuous' and status<>'completed'")
        .bind(studies).bind(owner).execute(&mut *tx).await?;
    sqlx::query("delete from replay_runs where id=any($1) and owner_id=$2 and origin='continuous' and status<>'done'")
        .bind(runs).bind(owner).execute(&mut *tx).await?;
    sqlx::query("update continuous_generations g set replay_batch_id=null where owner_id=$2 and replay_batch_id=any($1) and not exists(select 1 from replay_runs r where r.batch_id=g.replay_batch_id)")
        .bind(&batches).bind(owner).execute(&mut *tx).await?;
    sqlx::query("delete from replay_batches b where id=any($1) and owner_id=$2 and origin='continuous' and not exists(select 1 from replay_runs r where r.batch_id=b.id)")
        .bind(batches).bind(owner).execute(&mut *tx).await?;
    sqlx::query("update continuous_generations set status='cancelled',phase='cancelled',updated_at=now() where owner_id=$1 and algorithm_id=$2 and status='running'")
        .bind(owner).bind(algorithm).execute(&mut *tx).await?;
    sqlx::query("delete from continuous_runtime_artifacts a using continuous_generations g where a.generation_id=g.id and g.owner_id=$1 and g.algorithm_id=$2 and g.status<>'running'")
        .bind(owner).bind(algorithm).execute(&mut *tx).await?;
    sqlx::query("update continuous_processes set status='stopped',phase=null,active_generation_id=null,active_study_id=null,cleanup_targets='{\"studies\":[],\"runs\":[],\"batches\":[]}',updated_at=now() where owner_id=$1 and algorithm_id=$2")
        .bind(owner).bind(algorithm).execute(&mut *tx).await?;
    if !scheduler::owns(&mut tx, lease).await? {
        return Ok(());
    }
    tx.commit().await?;
    Ok(())
}

/// Live integration contract: generation identity is immutable; active pointer is the write fence.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActiveAdaptiveSlot {
    pub owner_id: String,
    pub algorithm_id: String,
    pub slot_id: i64,
    pub generation_id: i64,
    pub study_id: i64,
    pub config_revision: i64,
    pub min_setups_per_day: f64,
    pub spec: Value,
    pub result: Value,
    /// Winning detector parameters, never the baseline stored in spec.
    pub params: Value,
    /// Effective winning analytics, including causal cluster configuration.
    pub analytics: Value,
    pub selected_cell: Value,
    pub mean_expectancy: f64,
    pub direction: String,
    pub external_name: String,
    pub live_initialization: Option<live::LiveInitialization>,
    pub live_readiness: Vec<live::LiveInstrumentReadiness>,
}

pub async fn active_adaptive_slots(
    pool: &PgPool,
    owner: &str,
) -> Result<Vec<ActiveAdaptiveSlot>, DbError> {
    let rows = sqlx::query("select s.id slot_id,s.owner_id,s.algorithm_id,s.min_setups_per_day::double precision frequency,g.id generation_id,g.study_id,p.config_revision,g.spec,g.result,g.live_initialization,g.live_readiness,g.result#>'{best,params}' as params,g.result#>'{best,analytics}' as analytics,g.result->'selectedCell' as selected_cell,s.mean_expectancy,g.spec->>'direction' as direction,n.value->>'externalName' external_name from continuous_slots s join continuous_generations g on g.id=s.active_generation_id join adaptive_live_controls p on p.id=s.id left join settings n on n.key='algorithm:'||s.algorithm_id where s.owner_id=$1 and p.desired_running and coalesce((p.config->>'adaptiveEnabled')::boolean,false) and g.accepted=true and g.status='completed' order by s.id")
        .bind(owner).fetch_all(pool).await?;
    rows.into_iter()
        .map(|r| {
            Ok(ActiveAdaptiveSlot {
                owner_id: r.try_get("owner_id")?,
                algorithm_id: r.try_get("algorithm_id")?,
                slot_id: r.try_get("slot_id")?,
                generation_id: r.try_get("generation_id")?,
                study_id: r.try_get("study_id")?,
                config_revision: r.try_get("config_revision")?,
                min_setups_per_day: r.try_get("frequency")?,
                spec: r.try_get("spec")?,
                result: r.try_get("result")?,
                params: r.try_get("params")?,
                analytics: r.try_get("analytics")?,
                selected_cell: r.try_get("selected_cell")?,
                mean_expectancy: r.try_get("mean_expectancy")?,
                direction: r.try_get("direction")?,
                live_initialization: live::decode_live_metadata(&r)?.0,
                live_readiness: live::decode_live_metadata(&r)?.1,
                external_name: r
                    .try_get::<Option<String>, _>("external_name")?
                    .unwrap_or_else(|| crate::algorithm_name(&r.get::<String, _>("algorithm_id"))),
            })
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AdaptiveCheckpoint {
    /// May be the preceding generation: caller must preserve slot signal dedup/cooldown.
    pub generation_id: i64,
    pub state: Value,
}

pub async fn get_adaptive_checkpoint(
    pool: &PgPool,
    slot_id: i64,
    instrument_key: &str,
) -> Result<Option<AdaptiveCheckpoint>, DbError> {
    sqlx::query("select generation_id,state from adaptive_live_checkpoints where slot_id=$1 and instrument_key=$2")
        .bind(slot_id).bind(instrument_key).fetch_optional(pool).await?
        .map(|row|Ok(AdaptiveCheckpoint{generation_id:row.try_get("generation_id")?,state:row.try_get("state")?})).transpose()
}

#[derive(Debug, Clone, Serialize)]
pub struct AdaptiveSetupWrite {
    pub exchange_code: String,
    pub category: String,
    pub symbol: String,
    pub setup: setups_domain::SpikeSetup,
    /// Fully materialized immutable per-signal recommendation (including future entry time).
    pub recommendation: Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdaptiveInserted {
    pub index: usize,
    pub id: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdaptiveCommit {
    /// False is an expected stale/stopped/disabled result, not a database failure.
    pub applied: bool,
    /// Only newly inserted rows are eligible for notification, never duplicates.
    pub inserted: Vec<AdaptiveInserted>,
}

pub const ADAPTIVE_COMMIT_MAX_INSTRUMENTS: usize = 32;
/// Sum of serialized scan bytes, including predecessors, raw setups and recommendations.
/// Legacy single-scan entry points retain their existing per-item limits instead.
pub const ADAPTIVE_COMMIT_MAX_BYTES: usize = 8 * 1024 * 1024;
pub const ADAPTIVE_COMMIT_STATEMENT_TIMEOUT_MS: u64 = 2_000;
pub const ADAPTIVE_COMMIT_LOCK_TIMEOUT_MS: u64 = 1_000;
/// Checked between DB operations; an in-flight statement retains its server timeout.
pub const ADAPTIVE_COMMIT_TRANSACTION_BUDGET_MS: u64 = 10_000;

#[derive(Debug, Clone, Serialize)]
pub struct AdaptiveScanWrite {
    pub instrument_key: String,
    pub previous: Option<AdaptiveCheckpoint>,
    pub checkpoint: Value,
    pub setups: Vec<AdaptiveSetupWrite>,
}

#[derive(Clone, Copy, Serialize)]
struct AdaptiveScanRef<'a> {
    instrument_key: &'a str,
    previous: Option<&'a AdaptiveCheckpoint>,
    checkpoint: &'a Value,
    setups: &'a [AdaptiveSetupWrite],
}

impl<'a> From<&'a AdaptiveScanWrite> for AdaptiveScanRef<'a> {
    fn from(scan: &'a AdaptiveScanWrite) -> Self {
        Self {
            instrument_key: &scan.instrument_key,
            previous: scan.previous.as_ref(),
            checkpoint: &scan.checkpoint,
            setups: &scan.setups,
        }
    }
}

#[derive(Default)]
struct AdaptiveScanBytes(usize);
impl std::io::Write for AdaptiveScanBytes {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 = self
            .0
            .checked_add(bytes.len())
            .filter(|size| *size <= ADAPTIVE_COMMIT_MAX_BYTES)
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "adaptive commit exceeds byte limit",
                )
            })?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Allocation-free byte accounting for worker microbatch admission. Batch size is
/// the sum of this value for its scans, without array/separator overhead.
pub fn adaptive_scan_write_bytes(scan: &AdaptiveScanWrite) -> Result<usize, DbError> {
    let mut bytes = AdaptiveScanBytes::default();
    serde_json::to_writer(&mut bytes, scan).map_err(crate::json_error)?;
    Ok(bytes.0)
}

fn validate_scan(
    instrument_key: &str,
    checkpoint: &Value,
    setups: &[AdaptiveSetupWrite],
) -> Result<(), DbError> {
    if instrument_key.is_empty()
        || instrument_key.len() > 400
        || instrument_key.chars().any(char::is_control)
        || serde_json::to_vec(checkpoint)
            .map_err(crate::json_error)?
            .len()
            > 16384
        || setups.len() > 256
    {
        return Err(DbError::Json(
            "adaptive scan exceeds bounded identity/state/setup limits".into(),
        ));
    }
    for row in setups {
        if format!("{}/{}/{}", row.exchange_code, row.category, row.symbol) != instrument_key
            || serde_json::to_vec(&row.recommendation)
                .map_err(crate::json_error)?
                .len()
                > 16384
            || row.setup.close_time_unix < row.setup.open_time_unix
        {
            return Err(DbError::Json(
                "invalid adaptive setup identity/recommendation/time".into(),
            ));
        }
    }
    Ok(())
}

/// Atomically persist a bounded scan and checkpoint. Notifications happen AFTER this returns.
/// Locks serialize against promotion/Stop; an old captured slot cannot write after either wins.
/// instrument_key is exactly exchange/category/symbol, independent of host-local catalog IDs.
pub async fn commit_adaptive_scan(
    pool: &PgPool,
    expected: &ActiveAdaptiveSlot,
    instrument_key: &str,
    previous: Option<&AdaptiveCheckpoint>,
    checkpoint: &Value,
    setups: &[AdaptiveSetupWrite],
) -> Result<AdaptiveCommit, DbError> {
    commit_adaptive_scan_cas(pool, expected, instrument_key, previous, checkpoint, setups).await
}

/// CAS against the checkpoint actually used by the detector, including an absent first cursor.
pub async fn commit_adaptive_scan_cas(
    pool: &PgPool,
    expected: &ActiveAdaptiveSlot,
    instrument_key: &str,
    previous: Option<&AdaptiveCheckpoint>,
    checkpoint: &Value,
    setups: &[AdaptiveSetupWrite],
) -> Result<AdaptiveCommit, DbError> {
    // No full setup/checkpoint clone and no new aggregate cap for existing callers.
    let scan = AdaptiveScanRef {
        instrument_key,
        previous,
        checkpoint,
        setups,
    };
    Ok(commit_adaptive_scan_refs(pool, expected, &[scan], false)
        .await?
        .remove(0))
}

/// One fenced transaction for one captured slot/generation/revision. Results are
/// in INPUT order; inserted indices are local to each scan. Stale predecessors or
/// non-ready instruments return applied=false without excluding valid siblings.
/// Any validation/write/outbox error rolls back ALL siblings. Ambiguous COMMIT
/// errors must be reconciled against exact durable successors before RAM adoption.
pub async fn commit_adaptive_scans_cas(
    pool: &PgPool,
    expected: &ActiveAdaptiveSlot,
    scans: &[AdaptiveScanWrite],
) -> Result<Vec<AdaptiveCommit>, DbError> {
    if scans.is_empty() || scans.len() > ADAPTIVE_COMMIT_MAX_INSTRUMENTS {
        return Err(DbError::Json(
            "adaptive commit instrument count must be in 1..=32".into(),
        ));
    }
    let scans: Vec<_> = scans.iter().map(AdaptiveScanRef::from).collect();
    commit_adaptive_scan_refs(pool, expected, &scans, true).await
}

async fn commit_adaptive_scan_refs(
    pool: &PgPool,
    expected: &ActiveAdaptiveSlot,
    scans: &[AdaptiveScanRef<'_>],
    aggregate_byte_limit: bool,
) -> Result<Vec<AdaptiveCommit>, DbError> {
    // Single scans retain their existing per-item admission. Transaction/lock
    // budgets apply equally to cold reconstruction and hot microbatches.
    let keys = validate_adaptive_scan_batch(scans, aggregate_byte_limit)?;
    let deadline = Some(
        std::time::Instant::now()
            + std::time::Duration::from_millis(ADAPTIVE_COMMIT_TRANSACTION_BUDGET_MS),
    );
    let mut results = vec![
        AdaptiveCommit {
            applied: false,
            inserted: Vec::new()
        };
        scans.len()
    ];
    let mut tx = pool.begin().await?;
    // PG16 has no transaction_timeout. Bound server statements/locks, then
    // check the transaction budget between operations, never drop COMMIT.
    sqlx::query(
        "select set_config('statement_timeout',$1,true),set_config('lock_timeout',$2,true)",
    )
    .bind(ADAPTIVE_COMMIT_STATEMENT_TIMEOUT_MS.to_string())
    .bind(ADAPTIVE_COMMIT_LOCK_TIMEOUT_MS.to_string())
    .execute(&mut *tx)
    .await?;
    adaptive_commit_budget(deadline)?;
    // Same process -> slot lock order as atomic promotion; no locks are held during CPU/I/O.
    let process = sqlx::query("select desired_running,config,config_revision from adaptive_live_controls where id=$3 and owner_id=$1 and algorithm_id=$2 for share")
        .bind(&expected.owner_id).bind(&expected.algorithm_id).bind(expected.slot_id).fetch_optional(&mut *tx).await?;
    let slot = sqlx::query("select active_generation_id,enabled from continuous_slots where id=$1 and owner_id=$2 and algorithm_id=$3 for update")
        .bind(expected.slot_id).bind(&expected.owner_id).bind(&expected.algorithm_id).fetch_optional(&mut *tx).await?;
    let global = sqlx::query("select value from settings where key='live' for share")
        .fetch_optional(&mut *tx)
        .await?;
    let enabled = match (process, slot, global) {
        (Some(p), Some(s), Some(g)) => {
            p.try_get::<bool, _>("desired_running")?
                && p.try_get::<i64, _>("config_revision")? == expected.config_revision
                && p.try_get::<Value, _>("config")?["adaptiveEnabled"] == json!(true)
                && s.try_get::<Option<i64>, _>("active_generation_id")?
                    == Some(expected.generation_id)
                && g.try_get::<Value, _>("value")?["enabled"] == json!(true)
        }
        _ => false,
    };
    if !enabled {
        return Ok(results);
    }
    // Read the current durable projection, not the caller's possibly stale copy.
    // Waiting instruments may join only through the real-state fenced join API.
    let readiness: Value =
        sqlx::query_scalar("select live_readiness from continuous_generations where id=$1")
            .bind(expected.generation_id)
            .fetch_one(&mut *tx)
            .await?;
    let readiness: Vec<live::LiveInstrumentReadiness> =
        serde_json::from_value(readiness).map_err(crate::json_error)?;
    let current=sqlx::query("select instrument_key,generation_id,state from adaptive_live_checkpoints where slot_id=$1 and instrument_key=any($2) order by instrument_key for update")
        .bind(expected.slot_id).bind(&keys).fetch_all(&mut *tx).await?
        .into_iter().map(|row|Ok::<_,DbError>((row.try_get::<String,_>("instrument_key")?,AdaptiveCheckpoint {
            generation_id:row.try_get("generation_id")?,state:row.try_get("state")?,
        }))).collect::<Result<std::collections::BTreeMap<_,_>,_>>()?;
    let mut order: Vec<_> = (0..scans.len()).collect();
    order.sort_unstable_by_key(|&index| scans[index].instrument_key);
    let mut applied = Vec::with_capacity(scans.len());
    for index in order {
        adaptive_commit_budget(deadline)?;
        let scan = scans[index];
        let publication_floor = if !readiness.is_empty() {
            let Some(entry) = readiness.iter().find(|entry| {
                entry.instrument_key == scan.instrument_key && entry.status == "ready"
            }) else {
                continue;
            };
            Some(entry.publication_floor_close_unix)
        } else {
            None
        };
        if current.get(scan.instrument_key) != scan.previous {
            continue;
        }
        let inserted =
            persist_adaptive_setups(&mut tx, expected, publication_floor, scan.setups, deadline)
                .await?;
        results[index] = AdaptiveCommit {
            applied: true,
            inserted,
        };
        applied.push(scan);
    }
    if applied.is_empty() {
        return Ok(results);
    }
    adaptive_commit_budget(deadline)?;
    let mut checkpoints = sqlx::QueryBuilder::<Postgres>::new(
        "insert into adaptive_live_checkpoints(slot_id,instrument_key,generation_id,state) ",
    );
    checkpoints.push_values(applied, |mut row, scan| {
        row.push_bind(expected.slot_id)
            .push_bind(scan.instrument_key)
            .push_bind(expected.generation_id)
            .push_bind(scan.checkpoint);
    });
    checkpoints.push(" on conflict(slot_id,instrument_key) do update set generation_id=excluded.generation_id,state=excluded.state,updated_at=now()")
        .build().execute(&mut *tx).await?;
    adaptive_commit_budget(deadline)?;
    tx.commit().await?;
    Ok(results)
}

fn adaptive_commit_budget(deadline: Option<std::time::Instant>) -> Result<(), DbError> {
    if deadline.is_some_and(|deadline| std::time::Instant::now() >= deadline) {
        return Err(DbError::Json(
            "adaptive commit transaction budget exceeded".into(),
        ));
    }
    Ok(())
}

fn validate_adaptive_scan_batch<'a>(
    scans: &[AdaptiveScanRef<'a>],
    bounded: bool,
) -> Result<Vec<&'a str>, DbError> {
    if scans.is_empty() || scans.len() > ADAPTIVE_COMMIT_MAX_INSTRUMENTS {
        return Err(DbError::Json(
            "adaptive commit instrument count must be in 1..=32".into(),
        ));
    }
    let mut keys = std::collections::BTreeSet::new();
    let mut bytes = AdaptiveScanBytes::default();
    for scan in scans {
        if !keys.insert(scan.instrument_key) {
            return Err(DbError::Json(
                "duplicate adaptive commit instrument key".into(),
            ));
        }
        if bounded {
            serde_json::to_writer(&mut bytes, scan).map_err(crate::json_error)?;
        }
        validate_scan(scan.instrument_key, scan.checkpoint, scan.setups)?;
    }
    Ok(keys.into_iter().collect())
}

async fn persist_adaptive_setups(
    tx: &mut Transaction<'_, Postgres>,
    expected: &ActiveAdaptiveSlot,
    publication_floor: Option<i64>,
    setups: &[AdaptiveSetupWrite],
    deadline: Option<std::time::Instant>,
) -> Result<Vec<AdaptiveInserted>, DbError> {
    let mut inserted = Vec::new();
    for (index, row) in setups.iter().enumerate() {
        adaptive_commit_budget(deadline)?;
        if row.setup.algorithm_id != expected.algorithm_id
            || Some(row.setup.algorithm_version.as_str())
                != expected.spec["algorithmVersion"].as_str()
        {
            return Err(DbError::Json(
                "adaptive setup differs from generation algorithm/version".into(),
            ));
        }
        let open = DateTime::<Utc>::from_timestamp(row.setup.open_time_unix, 0)
            .ok_or_else(|| DbError::Json("invalid adaptive setup open timestamp".into()))?;
        let close = DateTime::<Utc>::from_timestamp(row.setup.close_time_unix, 0)
            .ok_or_else(|| DbError::Json("invalid adaptive setup close timestamp".into()))?;
        if publication_floor.is_some_and(|floor| row.setup.close_time_unix <= floor) {
            let duplicate:bool=sqlx::query_scalar("select exists(select 1 from live_setups where adaptive_slot_id=$1 and exchange_code=$2 and category=$3 and symbol=$4 and open_time=$5)")
                .bind(expected.slot_id).bind(&row.exchange_code).bind(&row.category).bind(&row.symbol).bind(open).fetch_one(&mut **tx).await?;
            if duplicate {
                continue;
            }
            return Err(DbError::Json(
                "adaptive emission precedes instrument publication floor".into(),
            ));
        }
        let expectancy_id = adaptive_expectancy_reference(tx, expected, row).await?;
        let id: Option<i32> = sqlx::query_scalar("insert into live_setups(exchange_code,category,symbol,algorithm_id,algorithm_version,params,open_time,close_time,close_price,details,adaptive_slot_id,adaptive_generation_id,recommendation,external_name,expectancy_artifact_id) values($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15) on conflict(adaptive_slot_id,exchange_code,category,symbol,open_time) where adaptive_slot_id is not null do nothing returning id")
            .bind(&row.exchange_code).bind(&row.category).bind(&row.symbol).bind(&row.setup.algorithm_id)
            .bind(&row.setup.algorithm_version).bind(serde_json::to_value(&row.setup.params).map_err(crate::json_error)?)
            .bind(open).bind(close).bind(&row.setup.close_price)
            .bind(serde_json::to_value(&row.setup.details).map_err(crate::json_error)?)
            .bind(expected.slot_id).bind(expected.generation_id).bind(&row.recommendation).bind(&expected.external_name)
            .bind(expectancy_id)
            .fetch_optional(&mut **tx).await?;
        adaptive_commit_budget(deadline)?;
        if let Some(id) = id {
            outbox::enqueue(tx, expected, id, row).await?;
            adaptive_commit_budget(deadline)?;
            inserted.push(AdaptiveInserted {
                index,
                id: i64::from(id),
            });
        }
    }
    Ok(inserted)
}

/// Pin the exact evidence already frozen by the worker, never the current
/// preferred pointer. A KEY SHARE lock protects captured A from concurrent GC
/// until this transaction installs its real FK, even if B is now preferred.
/// Absent/null IDs preserve the legacy trigger; malformed explicit IDs fail
/// this transition instead of silently retaining an unrelated base artifact.
async fn adaptive_expectancy_reference(
    tx: &mut Transaction<'_, Postgres>,
    expected: &ActiveAdaptiveSlot,
    setup: &AdaptiveSetupWrite,
) -> Result<Option<i64>, DbError> {
    let snapshot = &setup.recommendation["expectancy"];
    let Some(value) = snapshot.get("artifactId").filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    let invalid = || DbError::Json("adaptive_expectancy_reference_invalid".into());
    let id = value.as_i64().filter(|id| *id > 0).ok_or_else(invalid)?;
    let row=sqlx::query("select artifact_key,owner_id,source_generation_id,source_study_id,source_slot_id,identity from study_expectancy_artifacts where id=$1 for key share")
        .bind(id).fetch_optional(&mut **tx).await?.ok_or_else(invalid)?;
    let identity: super::expectancy::ArtifactIdentity =
        serde_json::from_value(row.try_get("identity")?).map_err(|_| invalid())?;
    let key: String = row.try_get("artifact_key")?;
    let provenance = &setup.recommendation["provenance"];
    if identity.owner_id != expected.owner_id
        || identity.generation_id != expected.generation_id
        || identity.study_id != expected.study_id
        || identity.slot_id != expected.slot_id
        || identity.tp_pct != expected.selected_cell["tpPct"].as_f64().unwrap_or(f64::NAN)
        || identity.sl_pct != expected.selected_cell["slPct"].as_f64().unwrap_or(f64::NAN)
        || identity.effective_analytics != expected.analytics
        || row.try_get::<String, _>("owner_id")? != identity.owner_id
        || row.try_get::<i64, _>("source_generation_id")? != identity.generation_id
        || row.try_get::<i64, _>("source_study_id")? != identity.study_id
        || row.try_get::<i64, _>("source_slot_id")? != identity.slot_id
        || super::expectancy::artifact_key(&identity).map_err(|_| invalid())? != key
        || snapshot["derivedSnapshotId"].as_str() != Some(key.as_str())
        || snapshot["studyId"].as_i64() != Some(expected.study_id)
        || snapshot["generationId"].as_i64() != Some(expected.generation_id)
        || snapshot["slotId"].as_i64() != Some(expected.slot_id)
        || setup.recommendation["ownerId"].as_str() != Some(expected.owner_id.as_str())
        || setup.recommendation["studyId"].as_i64() != Some(expected.study_id)
        || setup.recommendation["generationId"].as_i64() != Some(expected.generation_id)
        || setup.recommendation["slotId"].as_i64() != Some(expected.slot_id)
        || snapshot["calculationVersion"]
            .as_u64()
            .map(|version| version.to_string())
            .as_ref()
            != Some(&identity.calculation_version)
        || snapshot["sourceSnapshotHash"] != identity.provenance["snapshotHash"]
        || snapshot["sourceSnapshotHash"] != provenance["snapshotHash"]
        || identity.provenance["optimizerVersion"] != provenance["optimizerVersion"]
        || identity.provenance["seed"] != provenance["seed"]
        || identity.provenance["capturedAt"] != provenance["capturedAt"]
        || snapshot["sourceCutoff"].as_i64() != Some(identity.observation_cutoff)
        || provenance["observationCutoff"].as_i64() != Some(identity.observation_cutoff)
        || snapshot["fromUnix"].as_i64() != Some(identity.from_unix)
        || snapshot["toUnix"].as_i64() != Some(identity.to_unix)
        || provenance["fromUnix"].as_i64() != Some(identity.from_unix)
        || provenance["toUnix"].as_i64() != Some(identity.to_unix)
        || setup.recommendation["tpPct"].as_f64() != Some(identity.tp_pct)
        || setup.recommendation["slPct"].as_f64() != Some(identity.sl_pct)
        || snapshot["forecastAtUnix"].as_i64() != Some(setup.setup.close_time_unix)
        || identity.observation_cutoff > setup.setup.close_time_unix
    {
        return Err(invalid());
    }
    match (
        &identity.historical_input_hash,
        snapshot
            .get("historicalRecalculation")
            .filter(|value| !value.is_null()),
    ) {
        (None, None) => {}
        (Some(hash), Some(historical)) => {
            let actual: super::expectancy::HistoricalRecalculationProvenance =
                serde_json::from_value(historical["provenance"].clone()).map_err(|_| invalid())?;
            actual.validate().map_err(|_| invalid())?;
            let original_id = historical["originalArtifactId"]
                .as_i64()
                .filter(|original| *original > 0 && *original != id)
                .ok_or_else(invalid)?;
            let available = historical["availableFromUnix"]
                .as_i64()
                .ok_or_else(invalid)?;
            if historical["revisionKey"].as_str() != Some(hash.as_str())
                || &actual.input_hash != hash
                || actual.read_to_unix > identity.observation_cutoff
                || available < actual.recalculated_at_unix
                || available > setup.setup.close_time_unix
            {
                return Err(invalid());
            }
            // Validate immutable operation evidence for A, not B's publication.
            let matched:bool=sqlx::query_scalar("select exists(select 1 from study_expectancy_recalculations where artifact_id=$1 and original_artifact_id=$2 and owner_id=$3 and generation_id=$4 and study_id=$5 and slot_id=$6 and revision_key=$7 and provenance=$8 and matched)")
                .bind(id).bind(original_id).bind(&expected.owner_id).bind(expected.generation_id).bind(expected.study_id).bind(expected.slot_id)
                .bind(hash).bind(&historical["provenance"]).fetch_one(&mut **tx).await?;
            if !matched {
                return Err(invalid());
            }
        }
        _ => return Err(invalid()),
    }
    Ok(Some(id))
}

fn without_live_permission(value: &Value) -> Value {
    let mut value = value.clone();
    if let Some(object) = value.as_object_mut() {
        object.remove("adaptiveEnabled");
    }
    value
}

/// Recheck immediately before each external notification. No historical row is rewritten.
pub async fn adaptive_notification_allowed(
    pool: &PgPool,
    expected: &ActiveAdaptiveSlot,
) -> Result<bool, DbError> {
    Ok(sqlx::query_scalar("select exists(select 1 from continuous_slots s join adaptive_live_controls p on p.id=s.id join continuous_generations g on g.id=s.active_generation_id where s.id=$1 and s.owner_id=$2 and s.algorithm_id=$3 and s.active_generation_id=$4 and p.config_revision=$5 and p.desired_running and p.status in ('running','error') and p.config->'adaptiveEnabled'='true'::jsonb and g.status='completed' and g.accepted=true and exists(select 1 from settings where key='live' and value->'enabled'='true'::jsonb))")
        .bind(expected.slot_id).bind(&expected.owner_id).bind(&expected.algorithm_id).bind(expected.generation_id).bind(expected.config_revision).fetch_one(pool).await?)
}

pub async fn adaptive_notification_permitted(
    pool: &PgPool,
    expected: &ActiveAdaptiveSlot,
) -> Result<bool, DbError> {
    adaptive_notification_allowed(pool, expected).await
}

pub async fn is_service_run(pool: &PgPool, id: i64) -> Result<bool, DbError> {
    Ok(sqlx::query_scalar(
        "select exists(select 1 from replay_runs where id=$1 and origin='continuous')",
    )
    .bind(id as i32)
    .fetch_one(pool)
    .await?)
}
pub async fn is_service_batch(pool: &PgPool, id: i64) -> Result<bool, DbError> {
    Ok(sqlx::query_scalar(
        "select exists(select 1 from replay_batches where id=$1 and origin='continuous')",
    )
    .bind(id as i32)
    .fetch_one(pool)
    .await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saved_matrix_mode_defaults_only_proven_legacy_specs() {
        use setups_domain::trend::TrendMode;
        assert_eq!(
            saved_trend_mode(Some(&json!({"trendMode":"mean"}))),
            Some(TrendMode::Mean)
        );
        assert_eq!(
            saved_trend_mode(Some(&json!({"trendMode":"forecast"}))),
            Some(TrendMode::Forecast)
        );
        assert_eq!(
            saved_trend_mode(Some(&json!({"algorithmId":"legacy"}))),
            Some(TrendMode::Mean)
        );
        for unknown in [
            None,
            Some(json!(null)),
            Some(json!({"trendMode":null})),
            Some(json!({"trendMode":"unknown"})),
        ] {
            assert_eq!(saved_trend_mode(unknown.as_ref()), None);
        }
    }

    #[test]
    fn adaptive_batch_preflight_bounds_keys_items_and_exact_serialized_bytes() {
        let scan = AdaptiveScanWrite {
            instrument_key: "bybit/linear/BTCUSDT".into(),
            previous: Some(AdaptiveCheckpoint {
                generation_id: 7,
                state: json!({"nextOpenUnix":60}),
            }),
            checkpoint: json!({"nextOpenUnix":120}),
            setups: Vec::new(),
        };
        assert_eq!(
            adaptive_scan_write_bytes(&scan).unwrap(),
            serde_json::to_vec(&scan).unwrap().len()
        );
        let borrowed = AdaptiveScanRef::from(&scan);
        assert!(validate_adaptive_scan_batch(&[], true).is_err());
        assert!(
            validate_adaptive_scan_batch(&[borrowed; ADAPTIVE_COMMIT_MAX_INSTRUMENTS + 1], true)
                .is_err()
        );
        assert!(validate_adaptive_scan_batch(&[borrowed, borrowed], true).is_err());
        let writes: Vec<_> = (0..ADAPTIVE_COMMIT_MAX_INSTRUMENTS)
            .map(|index| AdaptiveScanWrite {
                instrument_key: format!("bybit/linear/S{index:02}"),
                ..scan.clone()
            })
            .collect();
        let refs: Vec<_> = writes.iter().rev().map(AdaptiveScanRef::from).collect();
        let keys = validate_adaptive_scan_batch(&refs, true).unwrap();
        assert_eq!(keys.first().copied(), Some("bybit/linear/S00"));
        assert_eq!(keys.len(), ADAPTIVE_COMMIT_MAX_INSTRUMENTS);
        let mut invalid = scan.clone();
        invalid.instrument_key.clear();
        assert!(validate_adaptive_scan_batch(&[AdaptiveScanRef::from(&invalid)], true).is_err());
        invalid = scan;
        invalid.checkpoint = json!({"oversize":"x".repeat(16384)});
        assert!(validate_adaptive_scan_batch(&[AdaptiveScanRef::from(&invalid)], true).is_err());
    }

    #[test]
    fn adaptive_batch_byte_limit_does_not_tighten_legacy_single_scan_validation() {
        let setup = AdaptiveSetupWrite {
            exchange_code: "bybit".into(),
            category: "linear".into(),
            symbol: "BTCUSDT".into(),
            setup: setups_domain::SpikeSetup {
                algorithm_id: "price_volume_spike".into(),
                algorithm_version: "1.1.1".into(),
                open_time_unix: 0,
                close_time_unix: 60,
                close_price: "x".repeat(ADAPTIVE_COMMIT_MAX_BYTES),
                params: setups_domain::AlgorithmParams::Spike(setups_domain::SpikeParams::default()),
                details: None,
            },
            recommendation: json!({}),
        };
        let scan = AdaptiveScanWrite {
            instrument_key: "bybit/linear/BTCUSDT".into(),
            previous: None,
            checkpoint: json!({}),
            setups: vec![setup],
        };
        assert!(validate_scan(&scan.instrument_key, &scan.checkpoint, &scan.setups).is_ok());
        assert!(adaptive_scan_write_bytes(&scan).is_err());
        assert!(validate_adaptive_scan_batch(&[AdaptiveScanRef::from(&scan)], true).is_err());
        assert!(validate_adaptive_scan_batch(&[AdaptiveScanRef::from(&scan)], false).is_ok());
        let mut small = scan.setups[0].clone();
        small.setup.close_price = "100".into();
        let oversized = vec![small; 257];
        assert!(validate_scan(&scan.instrument_key, &scan.checkpoint, &oversized).is_err());
        assert!(adaptive_commit_budget(Some(std::time::Instant::now())).is_err());
        assert!(adaptive_commit_budget(None).is_ok());
    }

    #[test]
    fn bounded_scan_rejects_identity_state_and_batch_overflow_before_database() {
        assert!(validate_scan("bybit/linear/BTCUSDT", &json!({"nextUnix":60}), &[]).is_ok());
        assert!(validate_scan("", &json!({}), &[]).is_err());
        assert!(validate_scan("bad\nkey", &json!({}), &[]).is_err());
        assert!(
            validate_scan(
                "bybit/linear/BTCUSDT",
                &json!({"data":"x".repeat(16384)}),
                &[]
            )
            .is_err()
        );
    }

    #[test]
    fn migration_separates_manual_lane_and_slot_dedup_without_rewriting_history() {
        let sql = include_str!("../migrations/0013_continuous_studies.sql");
        assert!(sql.contains("WHERE origin = 'manual'"));
        assert!(sql.contains("WHERE adaptive_slot_id IS NULL"));
        assert!(sql.contains("WHERE adaptive_slot_id IS NOT NULL"));
        assert!(!sql.contains("DELETE FROM"));
    }
}
