//! Durable continuous ownership. All mutating transitions fence by owner/revision/lease.
pub mod scheduler;
pub mod live;
pub mod outbox;
pub mod cleanup;
pub mod maintenance;
pub mod recovery;
pub mod frozen;
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
    pub study_id: Option<i64>,
    pub mean_expectancy: Option<f64>,
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
    pub active_study_id: Option<i64>,
    pub phase: Option<String>,
    pub error: Option<Value>,
    pub slots: Vec<ContinuousSlot>,
}

fn process(row: &PgRow) -> Result<ContinuousProcess, DbError> {
    Ok(ContinuousProcess {
        owner_id: row.try_get("owner_id")?, algorithm_id: row.try_get("algorithm_id")?,
        status: row.try_get("status")?, config_revision: row.try_get("config_revision")?,
        config: row.try_get("config")?, active_study_id: row.try_get("active_study_id")?,
        phase: row.try_get("phase")?, error: row.try_get("error")?, slots: Vec::new(),
    })
}

pub async fn get(pool: &PgPool, owner: &str, algorithm: &str) -> Result<Option<ContinuousProcess>, DbError> {
    let mut tx = pool.begin().await?;
    sqlx::query("set transaction isolation level repeatable read, read only").execute(&mut *tx).await?;
    let row = sqlx::query("select * from continuous_processes where owner_id=$1 and algorithm_id=$2")
        .bind(owner).bind(algorithm).fetch_optional(&mut *tx).await?;
    let Some(row) = row else { return Ok(None); };
    let mut result = process(&row)?;
    let slots = sqlx::query("select s.*,s.min_setups_per_day::double precision as frequency,a.spec active_spec,a.live_initialization,coalesce(a.live_readiness,'[]'::jsonb) live_readiness,n.live_initialization pending_initialization,coalesce(n.live_init_attempts,0) pending_attempts,n.live_init_next_attempt_at pending_next_attempt_at,n.live_init_deadline pending_attempt_deadline from continuous_slots s left join continuous_generations a on a.id=s.active_generation_id left join continuous_generations n on n.id=s.pending_generation_id where s.owner_id=$1 and s.algorithm_id=$2 and s.enabled order by s.id")
        .bind(owner).bind(algorithm).fetch_all(&mut *tx).await?;
    result.slots = slots.into_iter().map(|row| Ok(ContinuousSlot {
        id: row.try_get("id")?, min_setups_per_day: row.try_get("frequency")?,
        status: row.try_get("status")?, active_generation_id: row.try_get("active_generation_id")?,
        active_status: if row.try_get::<Option<i64>,_>("active_generation_id")?.is_some() {"active"} else {"inactive"}.into(),
        pending_generation_id: row.try_get("pending_generation_id")?, pending_status: row.try_get("pending_status")?,
        readiness_frontier: row.try_get("readiness_frontier")?, pending_error: row.try_get("pending_error")?,
        live_watermark: row.try_get("live_watermark")?,
        live_initialization:live::decode_live_metadata(&row)?.0,
        live_readiness_summary:row.try_get::<Option<i64>,_>("active_generation_id")?.map(|generation|live::readiness_summary(generation,&row.try_get::<Value,_>("active_spec")?,&live::decode_live_metadata(&row)?.1)).transpose()?,
        pending_initialization:row.try_get::<Option<Value>,_>("pending_initialization")?.map(serde_json::from_value).transpose().map_err(crate::json_error)?,
        pending_attempts:row.try_get("pending_attempts")?,
        pending_next_attempt_at:row.try_get("pending_next_attempt_at")?,
        pending_attempt_deadline:row.try_get("pending_attempt_deadline")?,
        study_id: row.try_get("study_id")?, mean_expectancy: row.try_get("mean_expectancy")?,
        selected_cell: row.try_get("selected_cell")?, updated_at: row.try_get("updated_at")?,
    })).collect::<Result<_,DbError>>()?;
    tx.commit().await?;
    Ok(Some(result))
}

pub async fn list(pool: &PgPool, owner: &str, algorithm: Option<&str>) -> Result<Vec<ContinuousProcess>, DbError> {
    // Registered algorithms are bounded by API metadata, not user-created arbitrary keys.
    let ids: Vec<String> = sqlx::query_scalar("select algorithm_id from continuous_processes where owner_id=$1 and ($2::text is null or algorithm_id=$2) order by algorithm_id")
        .bind(owner).bind(algorithm).fetch_all(pool).await?;
    let mut out = Vec::with_capacity(ids.len());
    for id in ids { if let Some(p) = get(pool, owner, &id).await? { out.push(p); } }
    Ok(out)
}

async fn owner_lock(tx: &mut Transaction<'_,Postgres>, owner: &str) -> Result<(),DbError> {
    // Domain mutations and lease renewal deliberately use different locks.
    // Takeover must acquire this SAME advisory lock before changing its token.
    sqlx::query("select pg_advisory_xact_lock(hashtextextended($1,72423415))")
        .bind(owner).execute(&mut **tx).await?;
    sqlx::query("insert into continuous_owner_lanes(owner_id) values($1) on conflict do nothing")
        .bind(owner).execute(&mut **tx).await?;
    Ok(())
}

/// Editing a process is fenced and intentionally refused until its current chain drains.
pub async fn configure(pool: &PgPool, owner: &str, algorithm: &str, config: &Value, thresholds: &[f64]) -> Result<bool,DbError> {
    let mut tx = pool.begin().await?;
    owner_lock(&mut tx, owner).await?;
    let current = sqlx::query("select config,desired_running,status from continuous_processes where owner_id=$1 and algorithm_id=$2 for update")
        .bind(owner).bind(algorithm).fetch_optional(&mut *tx).await?;
    if let Some(current) = current {
        let previous: Value = current.try_get("config")?;
        // Live permission is independent of research identity and accepted generation.
        if without_live_permission(&previous) == without_live_permission(config) {
            sqlx::query("update continuous_processes set config=jsonb_set(config,'{adaptiveEnabled}',$3),updated_at=now() where owner_id=$1 and algorithm_id=$2")
                .bind(owner).bind(algorithm).bind(&config["adaptiveEnabled"]).execute(&mut *tx).await?;
            tx.commit().await?;
            return Ok(true);
        }
        if current.try_get::<bool,_>("desired_running")? || current.try_get::<String,_>("status")?=="stopping" { return Ok(false); }
    }
    sqlx::query("insert into continuous_processes(owner_id,algorithm_id,config) values($1,$2,$3) on conflict(owner_id,algorithm_id) do update set config=excluded.config,config_revision=continuous_processes.config_revision+1,status='stopped',phase=null,error=null,next_slot=0,updated_at=now()")
        .bind(owner).bind(algorithm).bind(config).execute(&mut *tx).await?;
    sqlx::query("update continuous_slots set enabled=false,active_generation_id=null,pending_generation_id=null,pending_status=null,pending_error=null,readiness_frontier=null,live_watermark=null,status='stopped',updated_at=now() where owner_id=$1 and algorithm_id=$2")
        .bind(owner).bind(algorithm).execute(&mut *tx).await?;
    live::retire_inactive_checkpoints(&mut tx,owner,algorithm,None).await?;
    live::invalidate_warmup(&mut tx,owner,algorithm).await?;
    for frequency in thresholds {
        sqlx::query("insert into continuous_slots(owner_id,algorithm_id,min_setups_per_day) values($1,$2,$3::text::numeric) on conflict(owner_id,algorithm_id,min_setups_per_day) do update set enabled=true,status='idle',updated_at=now()")
            .bind(owner).bind(algorithm).bind(frequency.to_string()).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(true)
}

pub async fn start(pool: &PgPool, owner: &str, algorithm: &str) -> Result<bool,DbError> {
    let mut tx = pool.begin().await?;
    owner_lock(&mut tx, owner).await?;
    let changed = sqlx::query("update continuous_processes set desired_running=true,status='running',error=null,retry_count=0,next_attempt_at=now(),phase=coalesce(phase,'queued'),updated_at=now() where owner_id=$1 and algorithm_id=$2 and status<>'stopping'")
        .bind(owner).bind(algorithm).execute(&mut *tx).await?.rows_affected()>0;
    // Also repair inactive cursors persisted before retirement was implemented;
    // retrying a technical failure must leave every accepted incumbent intact.
    if changed {
        sqlx::query("update continuous_generations g set recovery_pending=false,error=null from continuous_processes p where p.owner_id=$1 and p.algorithm_id=$2 and p.active_generation_id=g.id and g.status='running'")
            .bind(owner).bind(algorithm).execute(&mut *tx).await?;
        live::retire_inactive_checkpoints(&mut tx,owner,algorithm,None).await?;
    }
    tx.commit().await?;
    Ok(changed)
}

/// Capture exact unfinished IDs BEFORE cancellation changes statuses. Never sweep terminal history.
pub async fn stop(pool: &PgPool, owner: &str, algorithm: &str) -> Result<bool,DbError> {
    let mut tx = pool.begin().await?;
    owner_lock(&mut tx, owner).await?;
    let row = sqlx::query("select status from continuous_processes where owner_id=$1 and algorithm_id=$2 for update")
        .bind(owner).bind(algorithm).fetch_optional(&mut *tx).await?;
    let Some(row) = row else { return Ok(false); };
    if row.try_get::<String,_>("status")? == "stopping" { tx.commit().await?; return Ok(true); }
    let studies: Vec<i64> = sqlx::query_scalar("select s.id from optimization_studies s join continuous_generations g on g.id=s.continuous_generation_id where s.owner_id=$1 and s.origin='continuous' and g.algorithm_id=$2 and s.status in ('queued','running','cancel_requested') for update of s")
        .bind(owner).bind(algorithm).fetch_all(&mut *tx).await?;
    let runs: Vec<i32> = sqlx::query_scalar("select r.id from replay_runs r join continuous_generations g on g.id=r.continuous_generation_id where r.owner_id=$1 and r.origin='continuous' and g.algorithm_id=$2 and r.status in ('queued','running','cancel_requested') for update of r")
        .bind(owner).bind(algorithm).fetch_all(&mut *tx).await?;
    let batches: Vec<i32> = sqlx::query_scalar("select b.id from replay_batches b join continuous_generations g on g.id=b.continuous_generation_id where b.owner_id=$1 and b.origin='continuous' and g.algorithm_id=$2 and g.status='running' and not exists(select 1 from replay_runs r where r.batch_id=b.id and r.status='done')")
        .bind(owner).bind(algorithm).fetch_all(&mut *tx).await?;
    sqlx::query("update continuous_processes set desired_running=false,status='stopping',config_revision=config_revision+1,phase='stopping',cleanup_targets=$3,updated_at=now() where owner_id=$1 and algorithm_id=$2")
        .bind(owner).bind(algorithm).bind(json!({"studies":studies,"runs":runs,"batches":batches})).execute(&mut *tx).await?;
    sqlx::query("update continuous_slots set active_generation_id=null,pending_generation_id=null,pending_status=null,pending_error=null,readiness_frontier=null,status='stopped',updated_at=now() where owner_id=$1 and algorithm_id=$2")
        .bind(owner).bind(algorithm).execute(&mut *tx).await?;
    live::retire_inactive_checkpoints(&mut tx,owner,algorithm,None).await?;
    live::invalidate_warmup(&mut tx,owner,algorithm).await?;
    sqlx::query("update optimization_studies set status='cancel_requested',updated_at=now() where id=any($1)")
        .bind(&studies).execute(&mut *tx).await?;
    sqlx::query("update replay_runs set status='cancel_requested',updated_at=now() where id=any($1)")
        .bind(&runs).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(true)
}

/// Caller must have joined its in-flight CPU/I/O tasks, or acquired the expired owner's lease.
pub async fn cleanup_stopped(pool: &PgPool, owner: &str, algorithm: &str, lease:&scheduler::OwnerLease) -> Result<(),DbError> {
    cleanup_stopped_inner(pool,owner,algorithm,lease,true).await
}

async fn cleanup_stopped_inner(pool:&PgPool,owner:&str,algorithm:&str,lease:&scheduler::OwnerLease,wait:bool)->Result<(),DbError> {
    if owner!=lease.owner_id {return Ok(());}
    let mut tx = pool.begin().await?;
    if !wait {
        let free:bool=sqlx::query_scalar("select pg_try_advisory_xact_lock(hashtextextended($1,72423415))").bind(owner).fetch_one(&mut *tx).await?;
        if !free {return Ok(());}
    }
    owner_lock(&mut tx,owner).await?;
    // Revalidate UNDER the mutation lock, BEFORE reading potentially newer Stop targets.
    if !scheduler::owns(&mut tx,lease).await? {return Ok(());}
    let row = sqlx::query("select cleanup_targets from continuous_processes where owner_id=$1 and algorithm_id=$2 and status='stopping' for update")
        .bind(owner).bind(algorithm).fetch_optional(&mut *tx).await?;
    let Some(row) = row else { return Ok(()); };
    let targets: Value = row.try_get("cleanup_targets")?;
    let studies: Vec<i64> = serde_json::from_value(targets["studies"].clone()).map_err(crate::json_error)?;
    let runs: Vec<i32> = serde_json::from_value(targets["runs"].clone()).map_err(crate::json_error)?;
    let batches: Vec<i32> = serde_json::from_value(targets["batches"].clone()).map_err(crate::json_error)?;
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
    if !scheduler::owns(&mut tx,lease).await? {return Ok(());}
    tx.commit().await?;
    Ok(())
}

/// Live integration contract: generation identity is immutable; active pointer is the write fence.
#[derive(Debug,Clone,Serialize)]
#[serde(rename_all="camelCase")]
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

pub async fn active_adaptive_slots(pool: &PgPool, owner: &str) -> Result<Vec<ActiveAdaptiveSlot>,DbError> {
    let rows = sqlx::query("select s.id slot_id,s.owner_id,s.algorithm_id,s.min_setups_per_day::double precision frequency,g.id generation_id,g.study_id,g.config_revision,g.spec,g.result,g.live_initialization,g.live_readiness,g.result#>'{best,params}' as params,g.result#>'{best,analytics}' as analytics,g.result->'selectedCell' as selected_cell,s.mean_expectancy,g.spec->>'direction' as direction,n.value->>'externalName' external_name from continuous_slots s join continuous_generations g on g.id=s.active_generation_id join continuous_processes p on p.owner_id=s.owner_id and p.algorithm_id=s.algorithm_id left join settings n on n.key='algorithm:'||s.algorithm_id where s.owner_id=$1 and s.enabled and p.desired_running and coalesce((p.config->>'adaptiveEnabled')::boolean,false) and g.accepted=true and g.status='completed' order by s.id")
        .bind(owner).fetch_all(pool).await?;
    rows.into_iter().map(|r|Ok(ActiveAdaptiveSlot {
        owner_id:r.try_get("owner_id")?,algorithm_id:r.try_get("algorithm_id")?,
        slot_id:r.try_get("slot_id")?,generation_id:r.try_get("generation_id")?,
        study_id:r.try_get("study_id")?,
        config_revision:r.try_get("config_revision")?,min_setups_per_day:r.try_get("frequency")?,
        spec:r.try_get("spec")?,result:r.try_get("result")?,
        params:r.try_get("params")?,analytics:r.try_get("analytics")?,
        selected_cell:r.try_get("selected_cell")?,mean_expectancy:r.try_get("mean_expectancy")?,
        direction:r.try_get("direction")?,
        live_initialization:live::decode_live_metadata(&r)?.0,
        live_readiness:live::decode_live_metadata(&r)?.1,
        external_name:r.try_get::<Option<String>,_>("external_name")?.unwrap_or_else(||crate::algorithm_name(&r.get::<String,_>("algorithm_id"))),
    })).collect()
}

#[derive(Debug,Clone,PartialEq)]
pub struct AdaptiveCheckpoint {
    /// May be the preceding generation: caller must preserve slot signal dedup/cooldown.
    pub generation_id: i64,
    pub state: Value,
}

pub async fn get_adaptive_checkpoint(pool: &PgPool, slot_id: i64, instrument_key: &str) -> Result<Option<AdaptiveCheckpoint>,DbError> {
    sqlx::query("select generation_id,state from adaptive_live_checkpoints where slot_id=$1 and instrument_key=$2")
        .bind(slot_id).bind(instrument_key).fetch_optional(pool).await?
        .map(|row|Ok(AdaptiveCheckpoint{generation_id:row.try_get("generation_id")?,state:row.try_get("state")?})).transpose()
}

#[derive(Debug,Clone)]
pub struct AdaptiveSetupWrite {
    pub exchange_code: String,
    pub category: String,
    pub symbol: String,
    pub setup: setups_domain::SpikeSetup,
    /// Fully materialized immutable per-signal recommendation (including future entry time).
    pub recommendation: Value,
}

#[derive(Debug,Clone,PartialEq,Eq)]
pub struct AdaptiveInserted {
    pub index: usize,
    pub id: i64,
}

#[derive(Debug,Clone,PartialEq,Eq)]
pub struct AdaptiveCommit {
    /// False is an expected stale/stopped/disabled result, not a database failure.
    pub applied: bool,
    /// Only newly inserted rows are eligible for notification, never duplicates.
    pub inserted: Vec<AdaptiveInserted>,
}

fn validate_scan(instrument_key: &str, checkpoint: &Value, setups: &[AdaptiveSetupWrite]) -> Result<(),DbError> {
    if instrument_key.is_empty() || instrument_key.len()>400 || instrument_key.chars().any(char::is_control)
        || serde_json::to_vec(checkpoint).map_err(crate::json_error)?.len()>16384 || setups.len()>256 {
        return Err(DbError::Json("adaptive scan exceeds bounded identity/state/setup limits".into()));
    }
    for row in setups {
        if format!("{}/{}/{}",row.exchange_code,row.category,row.symbol)!=instrument_key
            || serde_json::to_vec(&row.recommendation).map_err(crate::json_error)?.len()>16384
            || row.setup.close_time_unix<row.setup.open_time_unix {
            return Err(DbError::Json("invalid adaptive setup identity/recommendation/time".into()));
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
) -> Result<AdaptiveCommit,DbError> {
    commit_adaptive_scan_cas(pool,expected,instrument_key,previous,checkpoint,setups).await
}

/// CAS against the checkpoint actually used by the detector, including an absent first cursor.
pub async fn commit_adaptive_scan_cas(
    pool: &PgPool,
    expected: &ActiveAdaptiveSlot,
    instrument_key: &str,
    previous: Option<&AdaptiveCheckpoint>,
    checkpoint: &Value,
    setups: &[AdaptiveSetupWrite],
) -> Result<AdaptiveCommit,DbError> {
    validate_scan(instrument_key,checkpoint,setups)?;
    let mut tx = pool.begin().await?;
    // Same process -> slot lock order as atomic promotion; no locks are held during CPU/I/O.
    let process = sqlx::query("select desired_running,config,config_revision from continuous_processes where owner_id=$1 and algorithm_id=$2 for share")
        .bind(&expected.owner_id).bind(&expected.algorithm_id).fetch_optional(&mut *tx).await?;
    let slot = sqlx::query("select active_generation_id,enabled from continuous_slots where id=$1 and owner_id=$2 and algorithm_id=$3 for update")
        .bind(expected.slot_id).bind(&expected.owner_id).bind(&expected.algorithm_id).fetch_optional(&mut *tx).await?;
    let global = sqlx::query("select value from settings where key='live' for share").fetch_optional(&mut *tx).await?;
    let enabled = match (process,slot,global) {
        (Some(p),Some(s),Some(g)) => p.try_get::<bool,_>("desired_running")?
            && p.try_get::<i64,_>("config_revision")?==expected.config_revision
            && p.try_get::<Value,_>("config")?["adaptiveEnabled"]==json!(true)
            && s.try_get::<bool,_>("enabled")?
            && s.try_get::<Option<i64>,_>("active_generation_id")?==Some(expected.generation_id)
            && g.try_get::<Value,_>("value")?["enabled"]==json!(true),
        _ => false,
    };
    if !enabled { return Ok(AdaptiveCommit{applied:false,inserted:Vec::new()}); }
    // Read the current durable projection, not the caller's possibly stale copy.
    // Waiting instruments may join only through the real-state fenced join API.
    let readiness:Value=sqlx::query_scalar("select live_readiness from continuous_generations where id=$1")
        .bind(expected.generation_id).fetch_one(&mut *tx).await?;
    let readiness:Vec<live::LiveInstrumentReadiness>=serde_json::from_value(readiness).map_err(crate::json_error)?;
    let publication_floor=if !readiness.is_empty() {
        let Some(entry)=readiness.iter().find(|entry|entry.instrument_key==instrument_key && entry.status=="ready") else {
            return Ok(AdaptiveCommit{applied:false,inserted:Vec::new()});
        };
        Some(entry.publication_floor_close_unix)
    }else{None};
    let current = sqlx::query("select generation_id,state from adaptive_live_checkpoints where slot_id=$1 and instrument_key=$2 for update")
        .bind(expected.slot_id).bind(instrument_key).fetch_optional(&mut *tx).await?
        .map(|r|Ok::<_,DbError>(AdaptiveCheckpoint{generation_id:r.try_get("generation_id")?,state:r.try_get("state")?})).transpose()?;
    if current.as_ref()!=previous { return Ok(AdaptiveCommit{applied:false,inserted:Vec::new()}); }
    let mut inserted = Vec::new();
    for (index,row) in setups.iter().enumerate() {
        if row.setup.algorithm_id!=expected.algorithm_id
            || Some(row.setup.algorithm_version.as_str())!=expected.spec["algorithmVersion"].as_str() {
            return Err(DbError::Json("adaptive setup differs from generation algorithm/version".into()));
        }
        let open = DateTime::<Utc>::from_timestamp(row.setup.open_time_unix,0)
            .ok_or_else(||DbError::Json("invalid adaptive setup open timestamp".into()))?;
        let close = DateTime::<Utc>::from_timestamp(row.setup.close_time_unix,0)
            .ok_or_else(||DbError::Json("invalid adaptive setup close timestamp".into()))?;
        if publication_floor.is_some_and(|floor|row.setup.close_time_unix<=floor) {
            let duplicate:bool=sqlx::query_scalar("select exists(select 1 from live_setups where adaptive_slot_id=$1 and exchange_code=$2 and category=$3 and symbol=$4 and open_time=$5)")
                .bind(expected.slot_id).bind(&row.exchange_code).bind(&row.category).bind(&row.symbol).bind(open).fetch_one(&mut *tx).await?;
            if duplicate {continue;}
            return Err(DbError::Json("adaptive emission precedes instrument publication floor".into()));
        }
        let id: Option<i32> = sqlx::query_scalar("insert into live_setups(exchange_code,category,symbol,algorithm_id,algorithm_version,params,open_time,close_time,close_price,details,adaptive_slot_id,adaptive_generation_id,recommendation,external_name) values($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14) on conflict(adaptive_slot_id,exchange_code,category,symbol,open_time) where adaptive_slot_id is not null do nothing returning id")
            .bind(&row.exchange_code).bind(&row.category).bind(&row.symbol).bind(&row.setup.algorithm_id)
            .bind(&row.setup.algorithm_version).bind(serde_json::to_value(&row.setup.params).map_err(crate::json_error)?)
            .bind(open).bind(close).bind(&row.setup.close_price)
            .bind(serde_json::to_value(&row.setup.details).map_err(crate::json_error)?)
            .bind(expected.slot_id).bind(expected.generation_id).bind(&row.recommendation).bind(&expected.external_name)
            .fetch_optional(&mut *tx).await?;
        if let Some(id)=id {
            outbox::enqueue(&mut tx,expected,id,row).await?;
            inserted.push(AdaptiveInserted{index,id:i64::from(id)});
        }
    }
    sqlx::query("insert into adaptive_live_checkpoints(slot_id,instrument_key,generation_id,state) values($1,$2,$3,$4) on conflict(slot_id,instrument_key) do update set generation_id=excluded.generation_id,state=excluded.state,updated_at=now()")
        .bind(expected.slot_id).bind(instrument_key).bind(expected.generation_id).bind(checkpoint)
        .execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(AdaptiveCommit{applied:true,inserted})
}

fn without_live_permission(value: &Value) -> Value {
    let mut value=value.clone();
    if let Some(object)=value.as_object_mut() { object.remove("adaptiveEnabled"); }
    value
}

/// Recheck immediately before each external notification. No historical row is rewritten.
pub async fn adaptive_notification_allowed(pool: &PgPool, expected: &ActiveAdaptiveSlot) -> Result<bool,DbError> {
    Ok(sqlx::query_scalar("select exists(select 1 from continuous_slots s join continuous_processes p on p.owner_id=s.owner_id and p.algorithm_id=s.algorithm_id join continuous_generations g on g.id=s.active_generation_id where s.id=$1 and s.owner_id=$2 and s.algorithm_id=$3 and s.active_generation_id=$4 and p.config_revision=$5 and s.enabled and p.desired_running and p.status in ('running','error') and p.config->'adaptiveEnabled'='true'::jsonb and g.status='completed' and g.accepted=true and exists(select 1 from settings where key='live' and value->'enabled'='true'::jsonb))")
        .bind(expected.slot_id).bind(&expected.owner_id).bind(&expected.algorithm_id).bind(expected.generation_id).bind(expected.config_revision).fetch_one(pool).await?)
}

pub async fn adaptive_notification_permitted(pool: &PgPool, expected: &ActiveAdaptiveSlot) -> Result<bool,DbError> {
    adaptive_notification_allowed(pool,expected).await
}

pub async fn is_service_run(pool:&PgPool,id:i64)->Result<bool,DbError> {
    Ok(sqlx::query_scalar("select exists(select 1 from replay_runs where id=$1 and origin='continuous')").bind(id as i32).fetch_one(pool).await?)
}
pub async fn is_service_batch(pool:&PgPool,id:i64)->Result<bool,DbError> {
    Ok(sqlx::query_scalar("select exists(select 1 from replay_batches where id=$1 and origin='continuous')").bind(id as i32).fetch_one(pool).await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_scan_rejects_identity_state_and_batch_overflow_before_database() {
        assert!(validate_scan("bybit/linear/BTCUSDT",&json!({"nextUnix":60}),&[]).is_ok());
        assert!(validate_scan("",&json!({}),&[]).is_err());
        assert!(validate_scan("bad\nkey",&json!({}),&[]).is_err());
        assert!(validate_scan("bybit/linear/BTCUSDT",&json!({"data":"x".repeat(16384)}),&[]).is_err());
    }

    #[test]
    fn migration_separates_manual_lane_and_slot_dedup_without_rewriting_history() {
        let sql=include_str!("../migrations/0013_continuous_studies.sql");
        assert!(sql.contains("WHERE origin = 'manual'"));
        assert!(sql.contains("WHERE adaptive_slot_id IS NULL"));
        assert!(sql.contains("WHERE adaptive_slot_id IS NOT NULL"));
        assert!(!sql.contains("DELETE FROM"));
    }
}
