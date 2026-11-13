//! Live readiness is independent of research ownership. No worker types enter this crate.
use super::{ActiveAdaptiveSlot, AdaptiveCheckpoint, owner_lock};
use crate::DbError;
use serde::Serialize;
use serde_json::Value;
use sqlx::{PgPool, Postgres, Row, Transaction};
use std::collections::BTreeSet;

pub const WARMUP_LEASE_SECONDS: i64 = 30;
pub const MAX_PROMOTION_INSTRUMENTS: usize = 1000;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingAdaptiveSlot {
    pub profile: ActiveAdaptiveSlot,
    pub expected_active_generation_id: Option<i64>,
    pub warmup_status: String,
    pub readiness_frontier: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct WarmupLease {
    pub owner_id: String,
    pub algorithm_id: String,
    pub slot_id: i64,
    pub generation_id: i64,
    pub config_revision: i64,
    pub worker: String,
    pub token: i64,
}

#[derive(Debug, Clone)]
pub struct CheckpointExpectation {
    pub instrument_key: String,
    pub checkpoint: Option<AdaptiveCheckpoint>,
}

#[derive(Debug, Clone)]
pub struct InitialAdaptiveCheckpoint {
    pub instrument_key: String,
    pub state: Value,
}

pub async fn pending_adaptive_slots(pool: &PgPool, owner: &str) -> Result<Vec<PendingAdaptiveSlot>, DbError> {
    let rows = sqlx::query("select s.id slot_id,s.owner_id,s.algorithm_id,s.min_setups_per_day::double precision frequency,s.active_generation_id,s.pending_status,s.readiness_frontier,g.id generation_id,g.study_id,g.config_revision,g.spec,g.result,g.result#>'{best,params}' params,g.result#>'{best,analytics}' analytics,g.result->'selectedCell' selected_cell,(g.result->>'meanExpectancy')::double precision mean_expectancy,g.spec->>'direction' direction,n.value->>'externalName' external_name from continuous_slots s join continuous_generations g on g.id=s.pending_generation_id join continuous_processes p on p.owner_id=s.owner_id and p.algorithm_id=s.algorithm_id left join settings n on n.key='algorithm:'||s.algorithm_id where s.owner_id=$1 and s.enabled and p.desired_running and p.config_revision=g.config_revision and p.config->'adaptiveEnabled'='true'::jsonb and g.accepted=true and g.status='completed' order by s.id limit 257")
        .bind(owner).fetch_all(pool).await?;
    if rows.len() > 256 { return Err(DbError::Json("pending live slot admission exceeds 256 profiles".into())); }
    rows.into_iter().map(|r| Ok(PendingAdaptiveSlot {
        expected_active_generation_id: r.try_get("active_generation_id")?,
        warmup_status: r.try_get("pending_status")?,
        readiness_frontier: r.try_get("readiness_frontier")?,
        profile: ActiveAdaptiveSlot {
            owner_id:r.try_get("owner_id")?, algorithm_id:r.try_get("algorithm_id")?,
            slot_id:r.try_get("slot_id")?, generation_id:r.try_get("generation_id")?,
            study_id:r.try_get("study_id")?, config_revision:r.try_get("config_revision")?,
            min_setups_per_day:r.try_get("frequency")?, spec:r.try_get("spec")?, result:r.try_get("result")?,
            params:r.try_get("params")?, analytics:r.try_get("analytics")?, selected_cell:r.try_get("selected_cell")?,
            mean_expectancy:r.try_get("mean_expectancy")?, direction:r.try_get("direction")?,
            external_name:r.try_get::<Option<String>,_>("external_name")?.unwrap_or_else(|| crate::algorithm_name(&r.get::<String,_>("algorithm_id"))),
        },
    })).collect()
}

fn valid_worker(worker: &str) -> Result<(), DbError> {
    if worker.is_empty() || worker.len() > 200 || worker.chars().any(char::is_control) {
        return Err(DbError::Json("invalid live warmup worker identity".into()));
    }
    Ok(())
}

// Always process -> slot -> warmup lane, shared with Stop and scan commit.
async fn lock_pending(tx: &mut Transaction<'_, Postgres>, fence: &WarmupLease) -> Result<bool, DbError> {
    owner_lock(tx, &fence.owner_id).await?;
    let process = sqlx::query("select desired_running,config_revision,config from continuous_processes where owner_id=$1 and algorithm_id=$2 for share")
        .bind(&fence.owner_id).bind(&fence.algorithm_id).fetch_optional(&mut **tx).await?;
    let slot = sqlx::query("select pending_generation_id,enabled from continuous_slots where id=$1 and owner_id=$2 and algorithm_id=$3 for update")
        .bind(fence.slot_id).bind(&fence.owner_id).bind(&fence.algorithm_id).fetch_optional(&mut **tx).await?;
    Ok(match (process,slot) {
        (Some(p),Some(s)) => p.try_get::<bool,_>("desired_running")?
            && p.try_get::<i64,_>("config_revision")? == fence.config_revision
            && p.try_get::<Value,_>("config")?["adaptiveEnabled"] == true
            && s.try_get::<bool,_>("enabled")?
            && s.try_get::<Option<i64>,_>("pending_generation_id")? == Some(fence.generation_id),
        _ => false,
    })
}

async fn lease_current(tx: &mut Transaction<'_, Postgres>, fence: &WarmupLease) -> Result<bool, DbError> {
    Ok(sqlx::query_scalar("select exists(select 1 from adaptive_live_warmup_lane where id=1 and slot_id=$1 and generation_id=$2 and lease_owner=$3 and lease_token=$4 and lease_until>clock_timestamp())")
        .bind(fence.slot_id).bind(fence.generation_id).bind(&fence.worker).bind(fence.token).fetch_one(&mut **tx).await?)
}

pub async fn claim_pending_warmup(pool: &PgPool, pending: &PendingAdaptiveSlot, worker: &str) -> Result<Option<WarmupLease>, DbError> {
    valid_worker(worker)?;
    let p = &pending.profile;
    let mut fence = WarmupLease {owner_id:p.owner_id.clone(),algorithm_id:p.algorithm_id.clone(),slot_id:p.slot_id,
        generation_id:p.generation_id,config_revision:p.config_revision,worker:worker.into(),token:0};
    let mut tx = pool.begin().await?;
    if !lock_pending(&mut tx,&fence).await? { return Ok(None); }
    let token: Option<i64> = sqlx::query_scalar("update adaptive_live_warmup_lane set slot_id=$1,generation_id=$2,lease_owner=$3,lease_token=lease_token+1,lease_until=clock_timestamp()+$4::bigint*interval '1 second' where id=1 and (lease_owner is null or lease_until<=clock_timestamp()) returning lease_token")
        .bind(p.slot_id).bind(p.generation_id).bind(worker).bind(WARMUP_LEASE_SECONDS).fetch_optional(&mut *tx).await?;
    let Some(token) = token else { return Ok(None); };
    fence.token = token;
    sqlx::query("update continuous_slots set pending_status='warming',pending_error=null,readiness_frontier=null,updated_at=now() where id=$1")
        .bind(p.slot_id).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(Some(fence))
}

pub async fn report_warmup(pool: &PgPool, fence: &WarmupLease, status: &str, readiness_frontier: Option<i64>, error: Option<&Value>) -> Result<bool, DbError> {
    if !matches!(status,"warming"|"ready"|"error") || (status=="ready" && readiness_frontier.is_none())
        || readiness_frontier.is_some_and(|v| v<0 || v%60!=0)
        || error.is_some_and(|v| !v.is_object() || v.to_string().len()>4096) {
        return Err(DbError::Json("invalid bounded live warmup report".into()));
    }
    let mut tx = pool.begin().await?;
    if !lock_pending(&mut tx,fence).await? { return Ok(false); }
    sqlx::query("select id from adaptive_live_warmup_lane where id=1 for update").execute(&mut *tx).await?;
    if !lease_current(&mut tx,fence).await? { return Ok(false); }
    sqlx::query("update continuous_slots set pending_status=$2,readiness_frontier=$3,pending_error=$4,updated_at=now() where id=$1")
        .bind(fence.slot_id).bind(status).bind(readiness_frontier).bind(error).execute(&mut *tx).await?;
    sqlx::query("update adaptive_live_warmup_lane set lease_until=clock_timestamp()+$1::bigint*interval '1 second' where id=1")
        .bind(WARMUP_LEASE_SECONDS).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(true)
}

fn validate_promotion(expected: &[CheckpointExpectation], frontier: i64, initial: &[InitialAdaptiveCheckpoint], generation: i64) -> Result<(),DbError> {
    if frontier<0 || frontier%60!=0 || initial.is_empty() || initial.len()>MAX_PROMOTION_INSTRUMENTS
        || expected.len()>MAX_PROMOTION_INSTRUMENTS*2 {
        return Err(DbError::Json("invalid live promotion frontier/scope".into()));
    }
    let mut keys = BTreeSet::new();
    for entry in initial {
        if entry.instrument_key.is_empty() || entry.instrument_key.len()>400 || entry.instrument_key.chars().any(char::is_control)
            || !keys.insert(&entry.instrument_key) || entry.state.to_string().len()>16384
            || entry.state["generationId"].as_i64()!=Some(generation)
            || entry.state["nextOpenUnix"].as_i64()!=Some(frontier) {
            return Err(DbError::Json("invalid initial live promotion checkpoint".into()));
        }
    }
    let mut keys = BTreeSet::new();
    for entry in expected {
        if entry.instrument_key.is_empty() || entry.instrument_key.len()>400 || !keys.insert(&entry.instrument_key)
            || entry.checkpoint.as_ref().is_some_and(|c| c.state.to_string().len()>16384 || c.state["nextOpenUnix"].as_i64()!=Some(frontier)) {
            return Err(DbError::Json("old live checkpoint has not reached the promotion frontier".into()));
        }
    }
    if initial.iter().any(|entry| !keys.contains(&entry.instrument_key)) {
        return Err(DbError::Json("promotion must explicitly compare even absent scoped checkpoints".into()));
    }
    Ok(())
}

/// A short transactional CAS only: caller must finish frozen/revalidated warmup outside this call.
/// `expected_checkpoints` describes the complete old durable cursor set (plus optional absent keys).
pub async fn promote_pending(pool: &PgPool, fence: &WarmupLease, expected_active_generation_id: Option<i64>, expected_checkpoints: &[CheckpointExpectation], readiness_frontier: i64, initial_new_checkpoints: &[InitialAdaptiveCheckpoint]) -> Result<bool, DbError> {
    validate_promotion(expected_checkpoints,readiness_frontier,initial_new_checkpoints,fence.generation_id)?;
    let mut tx = pool.begin().await?;
    if !lock_pending(&mut tx,fence).await? { return Ok(false); }
    sqlx::query("select id from adaptive_live_warmup_lane where id=1 for update").execute(&mut *tx).await?;
    if !lease_current(&mut tx,fence).await? { return Ok(false); }
    let ready: bool = sqlx::query_scalar("select exists(select 1 from continuous_slots s join continuous_generations g on g.id=s.pending_generation_id where s.id=$1 and s.active_generation_id is not distinct from $2 and s.pending_status='ready' and s.readiness_frontier=$3 and g.status='completed' and g.accepted=true and g.owner_id=$4 and g.algorithm_id=$5 and g.config_revision=$6 and exists(select 1 from settings where key='live' and value->'enabled'='true'::jsonb))")
        .bind(fence.slot_id).bind(expected_active_generation_id).bind(readiness_frontier).bind(&fence.owner_id).bind(&fence.algorithm_id).bind(fence.config_revision).fetch_one(&mut *tx).await?;
    if !ready { return Ok(false); }
    let candidate_spec: Value=sqlx::query_scalar("select spec from continuous_generations where id=$1").bind(fence.generation_id).fetch_one(&mut *tx).await?;
    let candidate_scope=scope_keys(&candidate_spec)?;
    let mut scope=candidate_scope.clone();
    if let Some(id)=expected_active_generation_id {
        let old_spec: Value=sqlx::query_scalar("select spec from continuous_generations where id=$1").bind(id).fetch_one(&mut *tx).await?;
        scope.extend(scope_keys(&old_spec)?);
    }
    if scope.iter().any(|key| !expected_checkpoints.iter().any(|e| &e.instrument_key==key))
        || initial_new_checkpoints.iter().any(|entry| !candidate_scope.contains(&entry.instrument_key)) {
        return Err(DbError::Json("promotion must compare the complete old/new declared scope, including absent cursors".into()));
    }
    let actual = sqlx::query("select instrument_key,generation_id,state from adaptive_live_checkpoints where slot_id=$1 order by instrument_key limit 2001 for update")
        .bind(fence.slot_id).fetch_all(&mut *tx).await?;
    if actual.len()>MAX_PROMOTION_INSTRUMENTS*2 {return Err(DbError::Json("live promotion cursor count exceeds bounded admission".into()));}
    if actual.len()!=expected_checkpoints.iter().filter(|e| e.checkpoint.is_some()).count() {return Ok(false);}
    for row in &actual {
        let key: String = row.try_get("instrument_key")?;
        let checkpoint = AdaptiveCheckpoint {generation_id:row.try_get("generation_id")?,state:row.try_get("state")?};
        if !expected_checkpoints.iter().any(|e| e.instrument_key==key && e.checkpoint.as_ref()==Some(&checkpoint)) {return Ok(false);}
    }
    // Delete only these ephemeral old detector cursors, never historical setups or research.
    sqlx::query("delete from adaptive_live_checkpoints where slot_id=$1").bind(fence.slot_id).execute(&mut *tx).await?;
    for entry in initial_new_checkpoints {
        sqlx::query("insert into adaptive_live_checkpoints(slot_id,instrument_key,generation_id,state) values($1,$2,$3,$4)")
            .bind(fence.slot_id).bind(&entry.instrument_key).bind(fence.generation_id).bind(&entry.state).execute(&mut *tx).await?;
    }
    sqlx::query("update continuous_slots s set active_generation_id=s.pending_generation_id,pending_generation_id=null,pending_status=null,pending_error=null,readiness_frontier=null,live_watermark=$2,status='active',study_id=g.study_id,mean_expectancy=(g.result->>'meanExpectancy')::double precision,selected_cell=g.result->'selectedCell',updated_at=now() from continuous_generations g where s.id=$1 and g.id=s.pending_generation_id")
        .bind(fence.slot_id).bind(readiness_frontier).execute(&mut *tx).await?;
    if !lease_current(&mut tx,fence).await? {return Ok(false);}
    sqlx::query("update adaptive_live_warmup_lane set slot_id=null,generation_id=null,lease_owner=null,lease_until=null where id=1").execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(true)
}

fn scope_keys(spec: &Value) -> Result<BTreeSet<String>,DbError> {
    let instruments=spec["instruments"].as_array().filter(|a| !a.is_empty() && a.len()<=MAX_PROMOTION_INSTRUMENTS)
        .ok_or_else(||DbError::Json("invalid persisted live generation scope".into()))?;
    let mut keys=BTreeSet::new();
    for instrument in instruments {
        let parts=["exchange","category","symbol"].map(|key| instrument[key].as_str());
        let [Some(exchange),Some(category),Some(symbol)]=parts else {return Err(DbError::Json("invalid persisted live instrument identity".into()));};
        let key=format!("{exchange}/{category}/{symbol}");
        if key.len()>400 || [exchange,category,symbol].iter().any(|p| p.is_empty() || p.contains('/') || p.chars().any(char::is_control)) || !keys.insert(key) {
            return Err(DbError::Json("invalid duplicate/bounded live generation scope".into()));
        }
    }
    Ok(keys)
}

/// Caller holds the owner mutation lock and the process write lock. Scans share
/// that process lock, so a captured old generation cannot recreate these cursors
/// after deactivation. Only obsolete ephemeral state is retired: accepted live
/// incumbents, signal dedup/history, outbox and research are never deletion targets.
pub(super) async fn retire_inactive_checkpoints(tx: &mut Transaction<'_,Postgres>, owner: &str, algorithm: &str, slot: Option<i64>) -> Result<(),DbError> {
    sqlx::query("delete from adaptive_live_checkpoints c using continuous_slots s where c.slot_id=s.id and s.owner_id=$1 and s.algorithm_id=$2 and s.active_generation_id is null and ($3::bigint is null or s.id=$3)")
        .bind(owner).bind(algorithm).bind(slot).execute(&mut **tx).await?;
    Ok(())
}

pub(super) async fn invalidate_warmup(tx: &mut Transaction<'_,Postgres>, owner: &str, algorithm: &str) -> Result<(),DbError> {
    sqlx::query("update adaptive_live_warmup_lane w set slot_id=null,generation_id=null,lease_owner=null,lease_until=null,lease_token=lease_token+1 where w.slot_id in (select id from continuous_slots where owner_id=$1 and algorithm_id=$2)")
        .bind(owner).bind(algorithm).execute(&mut **tx).await?;
    Ok(())
}
