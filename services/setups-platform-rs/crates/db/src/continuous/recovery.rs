//! Private durable stage boundaries. No public running StudyOutcome is introduced.
use super::scheduler::{self,ChainJob,OwnerLease};
use crate::DbError;
use serde_json::Value;
use sha2::{Digest,Sha256};
use sqlx::{PgPool,Postgres,Row,Transaction};

pub const MAX_CHECKPOINT_BYTES:usize=16*1024*1024;

/// Classification precedes any display/string conversion. A retry is permitted
/// only for transport/availability/transaction uncertainty, never malformed
/// private state, an incompatible identity, or an authoritative fence rejection.
pub fn is_transient(error:&DbError)->bool {
    match error {
        DbError::Sqlx(sqlx::Error::Io(_) | sqlx::Error::Tls(_) | sqlx::Error::Protocol(_) | sqlx::Error::PoolTimedOut | sqlx::Error::PoolClosed)=>true,
        DbError::Sqlx(sqlx::Error::Database(error))=>error.code().as_deref().is_some_and(|code|code.starts_with("08") || code.starts_with("40") || matches!(code,"55P03"|"57014"|"53300"|"57P01"|"57P02"|"57P03")),
        _=>false,
    }
}

/// An unresolved acknowledgement is not a failed scientific result. Preserve the
/// current head and IDs. Only the next owner claim charges a recovery attempt,
/// including when this best-effort write itself cannot reach the database.
pub async fn defer(pool:&PgPool,job:&ChainJob,error:&Value)->Result<bool,DbError> {
    let mut tx=pool.begin().await?;
    if !scheduler::fenced(&mut tx,job).await? {return Ok(false);}
    let row=sqlx::query("select (g.error->>'deferredLeaseToken')::bigint deferred_token,p.retry_count from continuous_generations g join continuous_processes p on p.active_generation_id=g.id where g.id=$1")
        .bind(job.generation_id).fetch_one(&mut *tx).await?;
    let prior:Option<i64>=row.try_get("deferred_token")?;
    if prior==Some(job.lease.token) {return Ok(true);}
    let mut error=error.clone();error["deferredLeaseToken"]=job.lease.token.into();
    if row.try_get::<i32,_>("retry_count")?>=scheduler::MAX_TECHNICAL_ATTEMPTS {error["code"]="durable_resume_retry_exhausted".into();error["message"]="Durable recovery retries exhausted; explicit Start retries the retained study without changing its input or deadline".into();}
    sqlx::query("update continuous_generations set error=$2,recovery_pending=true,updated_at=now() where id=$1")
        .bind(job.generation_id).bind(&error).execute(&mut *tx).await?;
    sqlx::query("update continuous_processes set status='error',error=$3,next_attempt_at=clock_timestamp()+interval '5 seconds',updated_at=now() where owner_id=$1 and algorithm_id=$2")
        .bind(&job.lease.owner_id).bind(&job.algorithm_id).bind(error).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(true)
}

/// Clear consecutive recovery failures only when this transaction advances a
/// durable head. Never call for heartbeat, phase/progress, or an exact retry.
pub async fn acknowledge_boundary(tx:&mut Transaction<'_,Postgres>,job:&ChainJob)->Result<(),DbError> {
    let changed=sqlx::query("update continuous_processes set retry_count=0,error=null,status='running',updated_at=now() where owner_id=$1 and algorithm_id=$2 and config_revision=$3 and active_generation_id=$4 and desired_running and error->>'code'='persistence_unconfirmed'")
        .bind(&job.lease.owner_id).bind(&job.algorithm_id).bind(job.config_revision).bind(job.generation_id).execute(&mut **tx).await?.rows_affected();
    if changed>0 {sqlx::query("update continuous_generations set recovery_pending=false,error=case when error->>'code'='persistence_unconfirmed' then null else error end where id=$1").bind(job.generation_id).execute(&mut **tx).await?;}
    Ok(())
}

pub struct ChainState {
    pub snapshot_committed:bool,
    pub checkpoint:Option<Vec<u8>>,
    pub checkpoint_sequence:i64,
    pub search_result:Option<Value>,
    pub replay_committed:bool,
    pub gate:Option<Gate>,
}
pub struct Gate {pub result:Value,pub accepted:bool,pub mean:Option<f64>,pub selected:Option<Value>}

/// Called after recover_owner atomically installed the new epoch. Admission drain
/// never blocks this path: this is the already admitted chain, not a new study.
pub async fn active_job(pool:&PgPool,lease:&OwnerLease)->Result<Option<ChainJob>,DbError> {
    let mut tx=pool.begin().await?;
    super::owner_lock(&mut tx,&lease.owner_id).await?;
    if !scheduler::owns(&mut tx,lease).await? {return Ok(None);}
    let row=sqlx::query("select g.*,p.config from continuous_generations g join continuous_processes p on p.active_generation_id=g.id and p.owner_id=g.owner_id and p.algorithm_id=g.algorithm_id where g.owner_id=$1 and g.lease_token=$2 and g.recovery_lease_token=$2 and g.status='running' and g.resume_version=1 and not g.recovery_pending and p.desired_running and p.status<>'stopping' and p.config_revision=g.config_revision and p.retry_count<=3 and p.next_attempt_at<=clock_timestamp() and coalesce(p.error->>'code','')<>'durable_resume_retry_exhausted'")
        .bind(&lease.owner_id).bind(lease.token).fetch_optional(&mut *tx).await?;
    let result=row.map(|row|Ok::<_,DbError>(ChainJob {lease:lease.clone(),algorithm_id:row.try_get("algorithm_id")?,generation_id:row.try_get("id")?,slot_id:row.try_get("slot_id")?,study_id:row.try_get::<Option<i64>,_>("study_id")?.ok_or_else(||DbError::Json("resume study identity missing".into()))?,config_revision:row.try_get("config_revision")?,observation_cutoff:row.try_get("observation_cutoff")?,config:row.try_get("config")?,spec:row.try_get("spec")?,execution_started_at:row.try_get("execution_started_at")?,research_deadline:row.try_get("research_deadline")?})).transpose()?;
    tx.commit().await?;
    Ok(result)
}

pub async fn load(pool:&PgPool,job:&ChainJob,identity:&Value)->Result<ChainState,DbError> {
    let mut tx=pool.begin().await?;
    if !scheduler::fenced(&mut tx,job).await? {return Err(DbError::Json("resume fence lost".into()));}
    let row=sqlx::query("select a.identity,a.snapshot_manifest is not null committed,a.checkpoint,a.checkpoint_hash,a.checkpoint_sequence,s.status study_status,s.result search_result,g.replay_manifest is not null replay_committed,a.gate_result,a.gate_accepted,a.gate_mean,a.gate_selected from continuous_generations g join optimization_studies s on s.id=g.study_id left join continuous_runtime_artifacts a on a.generation_id=g.id where g.id=$1")
        .bind(job.generation_id).fetch_one(&mut *tx).await?;
    let stored:Option<Value>=row.try_get("identity")?;
    if stored.as_ref().is_some_and(|stored|stored!=identity) {return Err(DbError::Json("resume_identity_incompatible".into()));}
    let checkpoint:Option<Vec<u8>>=row.try_get("checkpoint")?;
    let checksum:Option<String>=row.try_get("checkpoint_hash")?;
    if let Some(bytes)=&checkpoint
        && (bytes.is_empty() || bytes.len()>MAX_CHECKPOINT_BYTES || checksum.as_deref()!=Some(format!("{:x}",Sha256::digest(bytes)).as_str())) {
        return Err(DbError::Json("resume_checkpoint_corrupt".into()));
    }
    let status:String=row.try_get("study_status")?;
    if !matches!(status.as_str(),"running"|"completed") {return Err(DbError::Json("resume_study_not_running_or_completed".into()));}
    let gate:Option<Value>=row.try_get("gate_result")?;
    let gate=gate.map(|result|Ok::<_,DbError>(Gate {result,accepted:row.try_get::<Option<bool>,_>("gate_accepted")?.ok_or_else(||DbError::Json("resume_gate_incomplete".into()))?,mean:row.try_get("gate_mean")?,selected:row.try_get("gate_selected")?})).transpose()?;
    let result=ChainState {snapshot_committed:row.try_get::<Option<bool>,_>("committed")?.unwrap_or(false),checkpoint,checkpoint_sequence:row.try_get::<Option<i64>,_>("checkpoint_sequence")?.unwrap_or(0),search_result:if status=="completed" {row.try_get("search_result")?}else{None},replay_committed:row.try_get("replay_committed")?,gate};
    if (result.checkpoint.is_some() || result.search_result.is_some() || result.replay_committed || result.gate.is_some()) && !result.snapshot_committed {return Err(DbError::Json("resume_committed_input_missing".into()));}
    tx.commit().await?;
    Ok(result)
}

pub async fn save_checkpoint(pool:&PgPool,job:&ChainJob,identity:&Value,bytes:&[u8],max_runtime_bytes:i64)->Result<i64,DbError> {
    if bytes.is_empty() || bytes.len()>MAX_CHECKPOINT_BYTES || max_runtime_bytes<=0 {return Err(DbError::Json("resume_checkpoint_resource_limit".into()));}
    let mut tx=pool.begin().await?;
    if !scheduler::fenced(&mut tx,job).await? {return Err(DbError::Json("resume checkpoint fence lost".into()));}
    sqlx::query("select pg_advisory_xact_lock(72423412)").execute(&mut *tx).await?;
    let row=sqlx::query("select identity,snapshot_manifest is not null committed,checkpoint_hash,checkpoint_sequence,reserved_bytes,snapshot_bytes from continuous_runtime_artifacts where generation_id=$1 for update")
        .bind(job.generation_id).fetch_optional(&mut *tx).await?.ok_or_else(||DbError::Json("resume checkpoint input absent".into()))?;
    if row.try_get::<Value,_>("identity")?!=*identity || !row.try_get::<bool,_>("committed")? {return Err(DbError::Json("resume checkpoint identity/input mismatch".into()));}
    if row.try_get::<i64,_>("reserved_bytes")?.saturating_sub(row.try_get::<i64,_>("snapshot_bytes")?) < bytes.len() as i64 {
        return Err(DbError::Json("resume checkpoint exceeds own storage reservation".into()));
    }
    let hash=format!("{:x}",Sha256::digest(bytes));
    if row.try_get::<Option<String>,_>("checkpoint_hash")?.as_deref()==Some(hash.as_str()) {return Ok(row.try_get("checkpoint_sequence")?);}
    // Reservation includes two bounded checkpoint copies/WAL headroom at acquisition.
    let reserved:i64=sqlx::query_scalar("select coalesce(sum(reserved_bytes),0)::bigint from continuous_runtime_artifacts").fetch_one(&mut *tx).await?;
    if reserved>max_runtime_bytes {return Err(DbError::Json("resume aggregate storage resource_limit".into()));}
    let sequence=sqlx::query_scalar("update continuous_runtime_artifacts set checkpoint=$2,checkpoint_hash=$3,checkpoint_sequence=checkpoint_sequence+1,updated_at=now() where generation_id=$1 returning checkpoint_sequence")
        .bind(job.generation_id).bind(bytes).bind(hash).fetch_one(&mut *tx).await?;
    acknowledge_boundary(&mut tx,job).await?;
    tx.commit().await?;
    Ok(sequence)
}

pub async fn save_gate(pool:&PgPool,job:&ChainJob,identity:&Value,gate:&Gate)->Result<bool,DbError> {
    let mut tx=pool.begin().await?;
    if !scheduler::fenced(&mut tx,job).await? {return Ok(false);}
    let written=sqlx::query("update continuous_runtime_artifacts set gate_result=$3,gate_accepted=$4,gate_mean=$5,gate_selected=$6,updated_at=now() where generation_id=$1 and identity=$2 and snapshot_manifest is not null and gate_result is null")
        .bind(job.generation_id).bind(identity).bind(&gate.result).bind(gate.accepted).bind(gate.mean).bind(&gate.selected).execute(&mut *tx).await?.rows_affected();
    if written==0 {
        let matches:bool=sqlx::query_scalar("select exists(select 1 from continuous_runtime_artifacts where generation_id=$1 and identity=$2 and gate_result=$3 and gate_accepted=$4 and gate_mean is not distinct from $5 and gate_selected is not distinct from $6)")
            .bind(job.generation_id).bind(identity).bind(&gate.result).bind(gate.accepted).bind(gate.mean).bind(&gate.selected).fetch_one(&mut *tx).await?;
        if !matches {return Err(DbError::Json("resume gate identity/immutable result mismatch".into()));}
    }else{
        acknowledge_boundary(&mut tx,job).await?;
    }
    tx.commit().await?;
    Ok(true)
}

/// Reclaim private runtime data only. Completed research/replay/live are never deleted.
pub(super) async fn reclaim_terminal_in(tx:&mut Transaction<'_,Postgres>,owner:&str)->Result<u64,DbError> {
    Ok(sqlx::query("delete from continuous_runtime_artifacts a using continuous_generations g where a.generation_id=g.id and g.owner_id=$1 and g.status<>'running'").bind(owner).execute(&mut **tx).await?.rows_affected())
}

pub async fn reclaim_orphans(pool:&PgPool)->Result<u64,DbError> {
    let mut tx=pool.begin().await?;
    // A terminal transition fences all writers before reclaim; foreign keys own true orphans.
    let count=sqlx::query("delete from continuous_runtime_artifacts a using continuous_generations g where a.generation_id=g.id and g.status<>'running'").execute(&mut *tx).await?.rows_affected();
    tx.commit().await?;
    Ok(count)
}
