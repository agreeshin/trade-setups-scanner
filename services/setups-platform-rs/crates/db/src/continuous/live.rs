//! Live readiness is independent of research ownership. No worker types enter this crate.
use super::{ActiveAdaptiveSlot, AdaptiveCheckpoint, owner_lock};
use crate::DbError;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{PgPool, Postgres, Row, Transaction};
use std::collections::{BTreeMap, BTreeSet};

pub const WARMUP_LEASE_SECONDS: i64 = 30;
pub const MAX_PROMOTION_INSTRUMENTS: usize = 1000;
pub const MAX_INITIALIZATION_ATTEMPTS: i32 = 3;
/// Full-universe 30..90 day cold reads can take hours. Independent of 30s lease.
pub const DEFAULT_INITIALIZATION_ATTEMPT_SECONDS: i64 = 6 * 60 * 60;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveInitialization {
    pub version: u32,
    pub anchor_close_unix: i64,
    pub detector_from_open_unix: i64,
    pub emit_from_close_unix: i64,
}

#[derive(Debug, Clone)]
pub struct WaitingInstrument {
    pub instrument_key: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveInstrumentReadiness {
    pub instrument_key: String,
    pub status: String,
    pub reason: Option<String>,
    pub publication_floor_close_unix: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all="camelCase")]
pub struct WaitingReasonCount {pub reason:String,pub count:usize}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all="camelCase")]
pub struct LiveReadinessSummary {
    pub generation_id:i64,
    pub total_instruments:usize,
    pub ready_instruments:usize,
    pub waiting_instruments:usize,
    pub waiting_reasons:Vec<WaitingReasonCount>,
    pub other_waiting_instruments:usize,
}

pub(super) fn readiness_summary(generation:i64,spec:&Value,readiness:&[LiveInstrumentReadiness])->Result<LiveReadinessSummary,DbError> {
    let scope=scope_keys(spec)?;
    let mut reasons:BTreeMap<String,usize>=BTreeMap::new();
    let mut ready=0;
    for key in &scope {
        match readiness.iter().find(|entry|&entry.instrument_key==key) {
            Some(entry) if entry.status=="ready"=>ready+=1,
            entry=>{let reason=entry.and_then(|entry|entry.reason.as_deref()).filter(|reason|!reason.is_empty()).unwrap_or("unknown");
                *reasons.entry(reason.chars().take(240).collect()).or_default()+=1;},
        }
    }
    let mut reasons:Vec<_>=reasons.into_iter().map(|(reason,count)|WaitingReasonCount {reason,count}).collect();
    reasons.sort_by(|a,b|b.count.cmp(&a.count).then(a.reason.cmp(&b.reason)));
    let other=reasons.iter().skip(8).map(|reason|reason.count).sum();
    reasons.truncate(8);
    Ok(LiveReadinessSummary {generation_id:generation,total_instruments:scope.len(),ready_instruments:ready,
        waiting_instruments:scope.len()-ready,waiting_reasons:reasons,other_waiting_instruments:other})
}

pub(super) fn decode_live_metadata(row: &sqlx::postgres::PgRow) -> Result<(Option<LiveInitialization>, Vec<LiveInstrumentReadiness>), DbError> {
    let initialization: Option<Value> = row.try_get("live_initialization")?;
    let readiness: Value = row.try_get("live_readiness")?;
    Ok((initialization.map(serde_json::from_value).transpose().map_err(crate::json_error)?,
        serde_json::from_value(readiness).map_err(crate::json_error)?))
}

fn initialization_at(profile: &ActiveAdaptiveSlot, anchor: i64) -> Result<LiveInitialization, DbError> {
    let params: setups_domain::AlgorithmParams = serde_json::from_value(profile.params.clone()).map_err(crate::json_error)?;
    let causal = i64::from(params.cooldown_minutes()).max(if profile.analytics["clusterEnabled"] == true {
        profile.analytics["clusterGapMinutes"].as_i64().filter(|v| (1..=10080).contains(v))
            .ok_or_else(|| DbError::Json("invalid persisted live causal gap".into()))?
    } else { 0 });
    let minutes = i64::from(params.baseline_long_days().max(params.baseline_short_days())) * 1440
        + i64::from(params.window_minutes()) + causal + 1;
    let from = anchor.checked_sub(minutes * 60).filter(|v| *v >= 0)
        .ok_or_else(|| DbError::Json("invalid live initialization boundary".into()))?;
    Ok(LiveInitialization { version: 1, anchor_close_unix: anchor,
        detector_from_open_unix: from, emit_from_close_unix: anchor + 60 })
}

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
    pub initialization: LiveInitialization,
    pub attempt: i32,
    pub attempt_deadline_unix: i64,
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
    let rows = sqlx::query("select s.id slot_id,s.owner_id,s.algorithm_id,s.min_setups_per_day::double precision frequency,s.active_generation_id,s.pending_status,s.readiness_frontier,g.id generation_id,g.study_id,g.config_revision,g.spec,g.result,g.live_initialization,g.live_readiness,g.result#>'{best,params}' params,g.result#>'{best,analytics}' analytics,g.result->'selectedCell' selected_cell,(g.result->>'meanExpectancy')::double precision mean_expectancy,g.spec->>'direction' direction,n.value->>'externalName' external_name from continuous_slots s join continuous_generations g on g.id=s.pending_generation_id join continuous_processes p on p.owner_id=s.owner_id and p.algorithm_id=s.algorithm_id left join settings n on n.key='algorithm:'||s.algorithm_id where s.owner_id=$1 and s.enabled and p.desired_running and p.config_revision=g.config_revision and p.config->'adaptiveEnabled'='true'::jsonb and g.accepted=true and g.status='completed' and g.live_init_status is distinct from 'exhausted' and g.live_init_status is distinct from 'superseded' order by s.id limit 257")
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
            live_initialization:decode_live_metadata(&r)?.0,
            live_readiness:decode_live_metadata(&r)?.1,
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
    Ok(sqlx::query_scalar("select exists(select 1 from adaptive_live_warmup_lane w join continuous_generations g on g.id=w.generation_id where w.id=1 and w.slot_id=$1 and w.generation_id=$2 and w.lease_owner=$3 and w.lease_token=$4 and w.lease_until>clock_timestamp() and (g.live_init_deadline is null or g.live_init_deadline>clock_timestamp()))")
        .bind(fence.slot_id).bind(fence.generation_id).bind(&fence.worker).bind(fence.token).fetch_one(&mut **tx).await?)
}

pub async fn claim_pending_warmup(pool: &PgPool, pending: &PendingAdaptiveSlot, worker: &str) -> Result<Option<WarmupLease>, DbError> {
    let anchor: i64 = sqlx::query_scalar("select floor(extract(epoch from clock_timestamp())/60)::bigint*60")
        .fetch_one(pool).await?;
    claim_pending_warmup_at(pool,pending,worker,anchor).await
}

pub async fn claim_pending_warmup_at(pool: &PgPool, pending: &PendingAdaptiveSlot, worker: &str, anchor_close_unix: i64) -> Result<Option<WarmupLease>, DbError> {
    claim_pending_warmup_with_budget(pool,pending,worker,anchor_close_unix,DEFAULT_INITIALIZATION_ATTEMPT_SECONDS).await
}

/// Explicit 1 minute..24 hour wall budget, independent of renewable lease TTL.
pub async fn claim_pending_warmup_with_budget(pool: &PgPool, pending: &PendingAdaptiveSlot, worker: &str, anchor_close_unix: i64, attempt_seconds: i64) -> Result<Option<WarmupLease>, DbError> {
    valid_worker(worker)?;
    if anchor_close_unix < 0 || anchor_close_unix % 60 != 0 || !(60..=86400).contains(&attempt_seconds) {
        return Err(DbError::Json("invalid live initialization anchor/attempt budget".into()));
    }
    let p = &pending.profile;
    recover_pending_warmups(pool,&p.owner_id).await?;
    let initialization = p.live_initialization.clone().map(Ok).unwrap_or_else(|| initialization_at(p,anchor_close_unix))?;
    let mut fence = WarmupLease {owner_id:p.owner_id.clone(),algorithm_id:p.algorithm_id.clone(),slot_id:p.slot_id,
        generation_id:p.generation_id,config_revision:p.config_revision,worker:worker.into(),token:0,
        initialization,attempt:0,attempt_deadline_unix:0};
    let mut tx = pool.begin().await?;
    if !lock_pending(&mut tx,&fence).await? { return Ok(None); }
    // Process -> slot -> global lane -> generation: no long reads under locks.
    sqlx::query("select id from adaptive_live_warmup_lane where id=1 for update").execute(&mut *tx).await?;
    let eligible: bool = sqlx::query_scalar("select exists(select 1 from continuous_generations where id=$1 and status='completed' and accepted=true and live_init_attempts<3 and live_init_status is distinct from 'exhausted' and live_init_status is distinct from 'superseded' and (live_init_next_attempt_at is null or live_init_next_attempt_at<=clock_timestamp()))")
        .bind(p.generation_id).fetch_one(&mut *tx).await?;
    if !eligible { return Ok(None); }
    let token: Option<i64> = sqlx::query_scalar("update adaptive_live_warmup_lane set slot_id=$1,generation_id=$2,lease_owner=$3,lease_token=lease_token+1,lease_until=clock_timestamp()+$4::bigint*interval '1 second' where id=1 and (lease_owner is null or lease_until<=clock_timestamp()) returning lease_token")
        .bind(p.slot_id).bind(p.generation_id).bind(worker).bind(WARMUP_LEASE_SECONDS).fetch_optional(&mut *tx).await?;
    let Some(token) = token else { return Ok(None); };
    fence.token = token;
    let row=sqlx::query("update continuous_generations set live_initialization=coalesce(live_initialization,$2),live_init_attempts=live_init_attempts+1,live_init_status='running',live_init_deadline=clock_timestamp()+$3::bigint*interval '1 second',live_init_next_attempt_at=null where id=$1 returning live_initialization,live_init_attempts,extract(epoch from live_init_deadline)::bigint deadline")
        .bind(p.generation_id).bind(json!(fence.initialization)).bind(attempt_seconds).fetch_one(&mut *tx).await?;
    fence.initialization=serde_json::from_value(row.try_get("live_initialization")?).map_err(crate::json_error)?;
    fence.attempt=row.try_get("live_init_attempts")?;
    fence.attempt_deadline_unix=row.try_get("deadline")?;
    // Keep last error visible during retries instead of erasing a diagnostic.
    sqlx::query("update continuous_slots set pending_status='warming',readiness_frontier=null,updated_at=now() where id=$1")
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
    if status=="error" {
        return report_warmup_failure(pool,fence,error.unwrap_or(&json!({"code":"adaptive_warmup_failed"})),true).await;
    }
    let mut tx = pool.begin().await?;
    if !lock_pending(&mut tx,fence).await? { return Ok(false); }
    sqlx::query("select id from adaptive_live_warmup_lane where id=1 for update").execute(&mut *tx).await?;
    if !lease_current(&mut tx,fence).await? { return Ok(false); }
    sqlx::query("update continuous_slots set pending_status=$2,readiness_frontier=$3,pending_error=case when $2='ready' then $4 else coalesce($4,pending_error) end,updated_at=now() where id=$1")
        .bind(fence.slot_id).bind(status).bind(readiness_frontier).bind(error).execute(&mut *tx).await?;
    sqlx::query("update adaptive_live_warmup_lane set lease_until=least(clock_timestamp()+$1::bigint*interval '1 second',(select live_init_deadline from continuous_generations where id=adaptive_live_warmup_lane.generation_id)) where id=1")
        .bind(WARMUP_LEASE_SECONDS).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(true)
}

async fn finish_failed_attempt(tx: &mut Transaction<'_,Postgres>, generation: i64, slot: i64, error: &Value, retryable: bool) -> Result<(),DbError> {
    let exhausted:bool=sqlx::query_scalar("update continuous_generations set live_init_status=case when not $3 or live_init_attempts>=3 then 'exhausted' else 'retry' end,live_init_error=$2,live_init_deadline=null,live_init_next_attempt_at=clock_timestamp()+(case when live_init_attempts<=1 then 5 else 30 end)*interval '1 second' where id=$1 returning live_init_status='exhausted'")
        .bind(generation).bind(error).bind(retryable).fetch_one(&mut **tx).await?;
    sqlx::query("update continuous_slots set pending_generation_id=case when $3 then null else pending_generation_id end,pending_status='error',pending_error=$4,readiness_frontier=null,status=case when active_generation_id is not null then 'active' else 'error' end,updated_at=now() where id=$1 and pending_generation_id=$2")
        .bind(slot).bind(generation).bind(exhausted).bind(error).execute(&mut **tx).await?;
    sqlx::query("update adaptive_live_warmup_lane set slot_id=null,generation_id=null,lease_owner=null,lease_until=null,lease_token=lease_token+1 where id=1 and generation_id=$1")
        .bind(generation).execute(&mut **tx).await?;
    Ok(())
}

/// Caller has drained this attempt's CPU/I/O before releasing the global lane.
pub async fn report_warmup_failure(pool:&PgPool,fence:&WarmupLease,error:&Value,retryable:bool)->Result<bool,DbError> {
    if !error.is_object() || error.to_string().len()>4096 {return Err(DbError::Json("invalid bounded live initialization failure".into()));}
    let mut tx=pool.begin().await?;
    if !lock_pending(&mut tx,fence).await? {return Ok(false);}
    sqlx::query("select id from adaptive_live_warmup_lane where id=1 for update").execute(&mut *tx).await?;
    if !lease_current(&mut tx,fence).await? {return Ok(false);}
    finish_failed_attempt(&mut tx,fence.generation_id,fence.slot_id,error,retryable).await?;
    tx.commit().await?;
    Ok(true)
}

/// Expected cohort/CAS changes may yield without classifying the candidate as bad.
pub async fn release_pending_warmup(pool:&PgPool,fence:&WarmupLease)->Result<bool,DbError> {
    let mut tx=pool.begin().await?;
    if !lock_pending(&mut tx,fence).await? {return Ok(false);}
    sqlx::query("select id from adaptive_live_warmup_lane where id=1 for update").execute(&mut *tx).await?;
    if !lease_current(&mut tx,fence).await? {return Ok(false);}
    sqlx::query("update continuous_generations set live_init_attempts=greatest(live_init_attempts-1,0),live_init_status='retry',live_init_deadline=null,live_init_next_attempt_at=clock_timestamp() where id=$1")
        .bind(fence.generation_id).execute(&mut *tx).await?;
    sqlx::query("update adaptive_live_warmup_lane set slot_id=null,generation_id=null,lease_owner=null,lease_until=null,lease_token=lease_token+1 where id=1")
        .execute(&mut *tx).await?;
    tx.commit().await?;Ok(true)
}

/// Recovers abandoned claims, including the last allowed attempt. Read APIs stay pure.
pub async fn recover_pending_warmups(pool:&PgPool,owner:&str)->Result<u64,DbError> {
    let mut tx=pool.begin().await?;
    owner_lock(&mut tx,owner).await?;
    sqlx::query("select owner_id from continuous_processes where owner_id=$1 order by algorithm_id for update").bind(owner).execute(&mut *tx).await?;
    let rows=sqlx::query("select s.id slot_id,g.id generation_id from continuous_slots s join continuous_generations g on g.id=s.pending_generation_id where s.owner_id=$1 and g.live_init_status='running' order by s.id for update of s")
        .bind(owner).fetch_all(&mut *tx).await?;
    sqlx::query("select id from adaptive_live_warmup_lane where id=1 for update").execute(&mut *tx).await?;
    let mut count=0;
    for row in rows {
        let generation:i64=row.try_get("generation_id")?;
        let expired:bool=sqlx::query_scalar("select exists(select 1 from continuous_generations g where g.id=$1 and (g.live_init_deadline<=clock_timestamp() or not exists(select 1 from adaptive_live_warmup_lane w where w.generation_id=g.id and w.lease_owner is not null and w.lease_until>clock_timestamp())))")
            .bind(generation).fetch_one(&mut *tx).await?;
        if expired {
            finish_failed_attempt(&mut tx,generation,row.try_get("slot_id")?,&json!({"code":"live_initialization_abandoned","message":"Initialization lease or fixed attempt budget expired"}),true).await?;
            count+=1;
        }
    }
    tx.commit().await?;Ok(count)
}

fn validate_promotion(expected: &[CheckpointExpectation], frontier: i64, initial: &[InitialAdaptiveCheckpoint], generation: i64, waiting: &[WaitingInstrument], retired_unavailable:&[WaitingInstrument]) -> Result<(),DbError> {
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
    let mut waiting_keys=BTreeSet::new();
    for entry in waiting.iter().chain(retired_unavailable) {
        if !valid_instrument_key(&entry.instrument_key) || !waiting_keys.insert(&entry.instrument_key)
            || keys.contains(&entry.instrument_key) || entry.reason.is_empty() || entry.reason.len()>240
            || entry.reason.chars().any(char::is_control) {
            return Err(DbError::Json("invalid explicit waiting instrument disposition".into()));
        }
    }
    if initial.len()+waiting.len()>MAX_PROMOTION_INSTRUMENTS || retired_unavailable.len()>MAX_PROMOTION_INSTRUMENTS {return Err(DbError::Json("live disposition scope exceeds admission".into()));}
    let mut keys = BTreeSet::new();
    for entry in expected {
        if entry.instrument_key.is_empty() || entry.instrument_key.len()>400 || !keys.insert(&entry.instrument_key)
            || entry.checkpoint.as_ref().is_some_and(|c| c.state.to_string().len()>16384 || match c.state["nextOpenUnix"].as_i64() {
                Some(next) if waiting_keys.contains(&entry.instrument_key)=>next<0 || next%60!=0 || next>frontier,
                next=>next!=Some(frontier),
            }) {
            return Err(DbError::Json("old live checkpoint has not reached the promotion frontier".into()));
        }
    }
    if initial.iter().any(|entry| !keys.contains(&entry.instrument_key)) {
        return Err(DbError::Json("promotion must explicitly compare even absent scoped checkpoints".into()));
    }
    Ok(())
}

/// A short transactional CAS only: caller must finish independent live warmup outside this call.
/// `expected_checkpoints` describes the complete old durable cursor set (plus optional absent keys).
pub async fn promote_pending(pool: &PgPool, fence: &WarmupLease, expected_active_generation_id: Option<i64>, expected_checkpoints: &[CheckpointExpectation], readiness_frontier: i64, initial_new_checkpoints: &[InitialAdaptiveCheckpoint]) -> Result<bool, DbError> {
    promote_pending_with_waiting(pool,fence,expected_active_generation_id,expected_checkpoints,readiness_frontier,initial_new_checkpoints,&[]).await
}

#[allow(clippy::too_many_arguments)]
pub async fn promote_pending_with_waiting(pool: &PgPool, fence: &WarmupLease, expected_active_generation_id: Option<i64>, expected_checkpoints: &[CheckpointExpectation], readiness_frontier: i64, initial_new_checkpoints: &[InitialAdaptiveCheckpoint], waiting: &[WaitingInstrument]) -> Result<bool, DbError> {
    promote_pending_with_dispositions(pool,fence,expected_active_generation_id,expected_checkpoints,readiness_frontier,initial_new_checkpoints,waiting,&[]).await
}

/// Retired unavailable keys are a separate, explicit old-only disposition;
/// never count them as candidate readiness or invent cursor progress for them.
#[allow(clippy::too_many_arguments)]
pub async fn promote_pending_with_dispositions(pool: &PgPool, fence: &WarmupLease, expected_active_generation_id: Option<i64>, expected_checkpoints: &[CheckpointExpectation], readiness_frontier: i64, initial_new_checkpoints: &[InitialAdaptiveCheckpoint], waiting: &[WaitingInstrument], retired_unavailable:&[WaitingInstrument]) -> Result<bool, DbError> {
    if initial_new_checkpoints.is_empty() {return Ok(false);}
    validate_promotion(expected_checkpoints,readiness_frontier,initial_new_checkpoints,fence.generation_id,waiting,retired_unavailable)?;
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
        || expected_checkpoints.iter().any(|entry|!scope.contains(&entry.instrument_key))
        || initial_new_checkpoints.iter().any(|entry| !candidate_scope.contains(&entry.instrument_key))
        || waiting.iter().any(|entry|!candidate_scope.contains(&entry.instrument_key))
        || retired_unavailable.iter().any(|entry|!scope.contains(&entry.instrument_key) || candidate_scope.contains(&entry.instrument_key))
        || candidate_scope.len()!=initial_new_checkpoints.len()+waiting.len() {
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
    let readiness:Vec<_>=candidate_scope.iter().map(|key| {
        let missing=waiting.iter().find(|entry| &entry.instrument_key==key);
        LiveInstrumentReadiness {instrument_key:key.clone(),status:if missing.is_some(){"waiting"}else{"ready"}.into(),
            reason:missing.map(|entry|entry.reason.clone()),publication_floor_close_unix:readiness_frontier}
    }).collect();
    sqlx::query("update continuous_generations set live_readiness=$2,live_init_status='active',live_init_next_attempt_at=null,live_init_error=null where id=$1")
        .bind(fence.generation_id).bind(json!(readiness)).execute(&mut *tx).await?;
    sqlx::query("update continuous_slots s set active_generation_id=s.pending_generation_id,pending_generation_id=null,pending_status=null,pending_error=null,readiness_frontier=null,live_watermark=$2,status='active',study_id=g.study_id,mean_expectancy=(g.result->>'meanExpectancy')::double precision,selected_cell=g.result->'selectedCell',updated_at=now() from continuous_generations g where s.id=$1 and g.id=s.pending_generation_id")
        .bind(fence.slot_id).bind(readiness_frontier).execute(&mut *tx).await?;
    if !lease_current(&mut tx,fence).await? {return Ok(false);}
    sqlx::query("update continuous_generations set live_init_deadline=null where id=$1")
        .bind(fence.generation_id).execute(&mut *tx).await?;
    sqlx::query("update adaptive_live_warmup_lane set slot_id=null,generation_id=null,lease_owner=null,lease_until=null where id=1").execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(true)
}

fn valid_instrument_key(key:&str)->bool {
    !key.is_empty() && key.len()<=400 && !key.chars().any(char::is_control)
        && key.split('/').count()==3 && key.split('/').all(|part|!part.is_empty())
}

/// A waiting instrument joins using real computed state; no historical emission.
pub async fn join_waiting_adaptive_instrument(pool:&PgPool,expected:&ActiveAdaptiveSlot,instrument_key:&str,previous:Option<&AdaptiveCheckpoint>,publication_floor_close_unix:i64,initial_state:&Value)->Result<bool,DbError> {
    if !valid_instrument_key(instrument_key) || publication_floor_close_unix<0 || publication_floor_close_unix%60!=0
        || initial_state.to_string().len()>16384 || initial_state["generationId"].as_i64()!=Some(expected.generation_id)
        || initial_state["nextOpenUnix"].as_i64()!=Some(publication_floor_close_unix) {
        return Err(DbError::Json("invalid waiting instrument join state/frontier".into()));
    }
    let mut tx=pool.begin().await?;
    let process=sqlx::query("select desired_running,config_revision,config from continuous_processes where owner_id=$1 and algorithm_id=$2 for share")
        .bind(&expected.owner_id).bind(&expected.algorithm_id).fetch_optional(&mut *tx).await?;
    let slot=sqlx::query("select active_generation_id,enabled from continuous_slots where id=$1 and owner_id=$2 and algorithm_id=$3 for update")
        .bind(expected.slot_id).bind(&expected.owner_id).bind(&expected.algorithm_id).fetch_optional(&mut *tx).await?;
    let global:bool=sqlx::query_scalar("select exists(select 1 from settings where key='live' and value->'enabled'='true'::jsonb)").fetch_one(&mut *tx).await?;
    let valid=match (process,slot) {
        (Some(p),Some(s))=>global && p.try_get::<bool,_>("desired_running")? && p.try_get::<i64,_>("config_revision")?==expected.config_revision
            && p.try_get::<Value,_>("config")?["adaptiveEnabled"]==true && s.try_get::<bool,_>("enabled")?
            && s.try_get::<Option<i64>,_>("active_generation_id")?==Some(expected.generation_id),
        _=>false,
    };
    if !valid {return Ok(false);}
    let raw:Value=sqlx::query_scalar("select live_readiness from continuous_generations where id=$1 for update")
        .bind(expected.generation_id).fetch_one(&mut *tx).await?;
    let mut readiness:Vec<LiveInstrumentReadiness>=serde_json::from_value(raw).map_err(crate::json_error)?;
    let Some(entry)=readiness.iter_mut().find(|entry|entry.instrument_key==instrument_key && entry.status=="waiting") else {return Ok(false);};
    if publication_floor_close_unix<entry.publication_floor_close_unix {return Ok(false);}
    let actual=sqlx::query("select generation_id,state from adaptive_live_checkpoints where slot_id=$1 and instrument_key=$2 for update")
        .bind(expected.slot_id).bind(instrument_key).fetch_optional(&mut *tx).await?
        .map(|r|Ok::<_,DbError>(AdaptiveCheckpoint {generation_id:r.try_get("generation_id")?,state:r.try_get("state")?})).transpose()?;
    if actual.as_ref()!=previous {return Ok(false);}
    entry.status="ready".into();entry.reason=None;entry.publication_floor_close_unix=publication_floor_close_unix;
    sqlx::query("insert into adaptive_live_checkpoints(slot_id,instrument_key,generation_id,state) values($1,$2,$3,$4) on conflict(slot_id,instrument_key) do update set generation_id=excluded.generation_id,state=excluded.state,updated_at=now()")
        .bind(expected.slot_id).bind(instrument_key).bind(expected.generation_id).bind(initial_state).execute(&mut *tx).await?;
    sqlx::query("update continuous_generations set live_readiness=$2 where id=$1")
        .bind(expected.generation_id).bind(json!(readiness)).execute(&mut *tx).await?;
    tx.commit().await?;Ok(true)
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

/// Research never waits for live. A newer complete result fences an older
/// pending candidate, but does not touch the active incumbent or research data.
pub(super) async fn supersede_pending(tx:&mut Transaction<'_,Postgres>,slot:i64)->Result<(),DbError> {
    let previous:Option<i64>=sqlx::query_scalar("select pending_generation_id from continuous_slots where id=$1 for update")
        .bind(slot).fetch_one(&mut **tx).await?;
    if let Some(previous)=previous {
        sqlx::query("update continuous_generations set live_init_status='superseded',live_init_deadline=null,live_init_next_attempt_at=null where id=$1")
            .bind(previous).execute(&mut **tx).await?;
        // Fence the old worker now. Keep its occupied 30s lease until expiry;
        // no second process may overlap work that has not observed the fence.
        sqlx::query("update adaptive_live_warmup_lane set lease_token=lease_token+1 where id=1 and generation_id=$1")
            .bind(previous).execute(&mut **tx).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_readiness_summary_counts_entire_declared_scope_and_bounds_reasons() {
        let spec=json!({"instruments":(0..12).map(|n|json!({"exchange":"bybit","category":"linear","symbol":format!("S{n}")})).collect::<Vec<_>>()});
        let mut readiness=vec![LiveInstrumentReadiness {instrument_key:"bybit/linear/S0".into(),status:"ready".into(),reason:None,publication_floor_close_unix:60}];
        readiness.extend((1..11).map(|n|LiveInstrumentReadiness {instrument_key:format!("bybit/linear/S{n}"),status:"waiting".into(),reason:Some(format!("reason_{n}")),publication_floor_close_unix:60}));
        let summary=readiness_summary(42,&spec,&readiness).unwrap();
        assert_eq!(summary.generation_id,42);
        assert_eq!(summary.total_instruments,12);
        assert_eq!(summary.ready_instruments,1);
        assert_eq!(summary.waiting_instruments,11,"missing readiness must not count ready");
        assert_eq!(summary.waiting_reasons.len(),8);
        assert_eq!(summary.other_waiting_instruments,3);
        assert_eq!(summary.waiting_reasons.iter().map(|r|r.count).sum::<usize>()+summary.other_waiting_instruments,summary.waiting_instruments);
        let missing=readiness_summary(99,&spec,&[]).unwrap();
        assert_eq!(missing.ready_instruments,0);
        assert_eq!(missing.waiting_reasons,vec![WaitingReasonCount {reason:"unknown".into(),count:12}]);
    }
}
