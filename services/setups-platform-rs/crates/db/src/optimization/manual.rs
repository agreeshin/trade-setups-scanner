//! Owner-serialized manual FIFO, execution epochs and private resumable state.
//! No input is retained in this module: callers own one chunk/checkpoint at a time.
use super::{Admission, StudyRecord, record};
use crate::{DbError, continuous::frozen::{SnapshotManifest, checksum, CHUNK_BYTES}};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::{PgPool, Postgres, Row, Transaction};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Limits {
    pub max_pending: i64,
    pub max_spec_bytes: i64,
    pub max_checkpoint_bytes: i64,
    pub max_runtime_bytes: i64,
    pub max_database_bytes: i64,
}
impl Default for Limits {
    fn default() -> Self { Self {
        max_pending:64, max_spec_bytes:8*1024*1024, max_checkpoint_bytes:16*1024*1024,
        max_runtime_bytes:16*1024*1024*1024, max_database_bytes:48*1024*1024*1024,
    }}
}
impl Limits {
    pub fn validate(&self) -> bool {
        (1..=64).contains(&self.max_pending) && (1..=8*1024*1024).contains(&self.max_spec_bytes)
            && (1..=16*1024*1024).contains(&self.max_checkpoint_bytes)
            && self.max_runtime_bytes > self.headroom() && self.max_runtime_bytes<=16*1024*1024*1024
            && self.max_database_bytes>self.max_runtime_bytes
    }
    pub fn headroom(&self)->i64 { 2*self.max_checkpoint_bytes+2*CHUNK_BYTES as i64 }
}
pub fn valid_key(key:&str)->bool {
    !key.is_empty() && key.len()<=128 && key.bytes().all(|c|c.is_ascii_graphic())
}
pub async fn receipt(pool:&PgPool,owner:&str,key:&str,spec:&Value)->Result<Option<Admission>,DbError> {
    let row=sqlx::query("select *,spec=$3::jsonb same from optimization_studies where owner_id=$1 and origin='manual' and manual_idempotency_key=$2")
        .bind(owner).bind(key).bind(spec).fetch_optional(pool).await?;
    row.map(|row|if row.get::<bool,_>("same"){record(row).map(|study|Admission::Created(Box::new(study)))}else{Ok(Admission::IdempotencyConflict)}).transpose()
}
fn invalid(message:&str)->DbError { DbError::Json(format!("manual_resume: {message}")) }

pub async fn create(pool:&PgPool,owner:&str,algorithm:&str,spec:&Value,progress:&Value,key:Option<&str>,limits:&Limits)->Result<Admission,DbError> {
    if !limits.validate() || key.is_some_and(|key|!valid_key(key)) {return Err(invalid("invalid admission limits/key"));}
    let mut tx=pool.begin().await?;
    crate::continuous::owner_lock(&mut tx,owner).await?;
    if let Some(key)=key {
        if let Some(row)=sqlx::query("select *,spec=$3::jsonb as same from optimization_studies where owner_id=$1 and origin='manual' and manual_idempotency_key=$2")
            .bind(owner).bind(key).bind(spec).fetch_optional(&mut *tx).await? {
            if !row.get::<bool,_>("same") {return Ok(Admission::IdempotencyConflict);}
            let study=record(row)?;tx.commit().await?;return Ok(Admission::Created(Box::new(study)));
        }
    }
    let (count,bytes):(i64,i64)=sqlx::query_as("select count(*),coalesce(sum(octet_length(spec::text)),0)::bigint from optimization_studies where owner_id=$1 and origin='manual' and status not in ('completed','cancelled','failed')")
        .bind(owner).fetch_one(&mut *tx).await?;
    let spec_bytes:i64=sqlx::query_scalar("select octet_length($1::jsonb::text)::bigint").bind(spec).fetch_one(&mut *tx).await?;
    for (resource,limit,requested) in [("manual_pending",limits.max_pending,count+1),("manual_spec_bytes",limits.max_spec_bytes,bytes+spec_bytes)] {
        if requested>limit {return Ok(Admission::Resource{resource,limit,requested});}
    }
    let row=sqlx::query("insert into optimization_studies(algorithm_id,owner_id,spec,progress,manual_format_version,manual_idempotency_key) values($1,$2,$3,$4,1,$5) returning *")
        .bind(algorithm).bind(owner).bind(spec).bind(progress).bind(key).fetch_one(&mut *tx).await?;
    tx.commit().await?;Ok(Admission::Created(Box::new(record(row)?)))
}

#[derive(Debug, Clone)]
pub struct Job {
    pub study:StudyRecord,
    pub epoch:i64,
    pub spent_ms:i64,
    pub segment_started_at:Option<DateTime<Utc>>,
    pub limits:Option<Value>,
}
fn job(row:sqlx::postgres::PgRow)->Result<Job,DbError> {
    Ok(Job {epoch:row.try_get("manual_execution_epoch")?,spent_ms:row.try_get("manual_spent_ms")?,
        segment_started_at:row.try_get("manual_segment_started_at")?,limits:row.try_get("manual_limits")?,study:record(row)?})
}
impl Job {
    pub fn elapsed_ms(&self)->i64 {
        self.spent_ms.saturating_add(self.segment_started_at.map_or(0,|at|(Utc::now()-at).num_milliseconds().max(0)))
    }
}

/// Single authoritative research arbitration. Caller holds owner mutation lock.
/// A logical holder blocks admission even if its executor lease has expired.
pub async fn manual_blocks_continuous(tx:&mut Transaction<'_,Postgres>,owner:&str)->Result<bool,DbError> {
    Ok(sqlx::query_scalar("select exists(select 1 from optimization_studies where owner_id=$1 and origin='manual' and (status='queued' or manual_execution_reserved))")
        .bind(owner).fetch_one(&mut **tx).await?)
}
pub async fn claim(pool:&PgPool,pins:Option<&Value>)->Result<Option<Job>,DbError> {
    // No SKIP LOCKED: it would let another worker skip the FIFO head. A short
    // owner transaction rechecks eligibility before reserving this exact head.
    let owner:Option<String>=sqlx::query_scalar("select q.owner_id from optimization_studies q where q.origin='manual' and q.status='queued' and not exists(select 1 from continuous_generations g where g.owner_id=q.owner_id and g.status='running') and not exists(select 1 from optimization_studies a where a.owner_id=q.owner_id and a.origin='manual' and a.manual_execution_reserved) order by q.manual_queue_sequence limit 1")
        .fetch_optional(pool).await?;
    let Some(owner)=owner else{return Ok(None);};
    let mut tx=pool.begin().await?;crate::continuous::owner_lock(&mut tx,&owner).await?;
    let busy:bool=sqlx::query_scalar("select exists(select 1 from continuous_generations where owner_id=$1 and status='running') or exists(select 1 from optimization_studies where owner_id=$1 and origin='manual' and manual_execution_reserved)")
        .bind(&owner).fetch_one(&mut *tx).await?;
    if busy {return Ok(None);}
    let row=sqlx::query("update optimization_studies set status='running',manual_execution_reserved=true,manual_execution_epoch=manual_execution_epoch+1,manual_format_version=1,manual_limits=coalesce(manual_limits,$2),manual_segment_started_at=clock_timestamp(),execution_started_at=coalesce(execution_started_at,clock_timestamp()),updated_at=now() where id=(select id from optimization_studies where owner_id=$1 and origin='manual' and status='queued' order by manual_queue_sequence limit 1 for update) returning *")
        .bind(&owner).bind(pins).fetch_optional(&mut *tx).await?;
    tx.commit().await?;row.map(job).transpose()
}
pub async fn pending_stop(pool:&PgPool)->Result<Option<Job>,DbError> {
    sqlx::query("select * from optimization_studies where origin='manual' and status='cancel_requested' and not manual_execution_reserved order by manual_queue_sequence limit 1")
        .fetch_optional(pool).await?.map(job).transpose()
}

pub enum Control { Changed(Box<StudyRecord>), Stale, Invalid, Missing }
pub async fn control(pool:&PgPool,owner:&str,id:i64,revision:i64,play:bool)->Result<Control,DbError> {
    let mut tx=pool.begin().await?;crate::continuous::owner_lock(&mut tx,owner).await?;
    let row=sqlx::query("select * from optimization_studies where id=$1 and owner_id=$2 for update").bind(id).bind(owner).fetch_optional(&mut *tx).await?;
    let Some(row)=row else{return Ok(Control::Missing);};
    if row.get::<String,_>("origin")!="manual" || row.get::<Option<i32>,_>("manual_format_version")!=Some(1) {return Ok(Control::Invalid);}
    if row.get::<i64,_>("manual_control_revision")!=revision {return Ok(Control::Stale);}
    let status:String=row.get("status");
    if (!play && matches!(status.as_str(),"pausing"|"paused")) || (play && status=="queued") {
        return Ok(Control::Changed(Box::new(record(row)?)));
    }
    if (!play && status!="running") || (play && status!="paused") {return Ok(Control::Invalid);}
    let row=sqlx::query("update optimization_studies set status=case when $2 then 'queued' else 'pausing' end,manual_control_revision=manual_control_revision+1,manual_queue_sequence=case when $2 then nextval('manual_study_queue_sequence') else manual_queue_sequence end,updated_at=now() where id=$1 returning *")
        .bind(id).bind(play).fetch_one(&mut *tx).await?;
    tx.commit().await?;Ok(Control::Changed(Box::new(record(row)?)))
}
pub async fn cancel(pool:&PgPool,id:i64)->Result<Option<(StudyRecord,bool)>,DbError> {
    let mut tx=pool.begin().await?;
    let owner:Option<String>=sqlx::query_scalar("select owner_id from optimization_studies where id=$1").bind(id).fetch_optional(&mut *tx).await?;
    let Some(owner)=owner else{return Ok(None);};crate::continuous::owner_lock(&mut tx,&owner).await?;
    let row=sqlx::query("select * from optimization_studies where id=$1 for update").bind(id).fetch_one(&mut *tx).await?;
    let status:String=row.get("status");
    if row.get::<String,_>("origin")!="manual" || matches!(status.as_str(),"completed"|"cancelled"|"failed") {return Ok(Some((record(row)?,false)));}
    let input:bool=sqlx::query_scalar("select exists(select 1 from manual_runtime_artifacts where study_id=$1 and checkpoint is not null)").bind(id).fetch_one(&mut *tx).await?;
    let immediate=status=="queued" && !input;
    let row=sqlx::query("update optimization_studies set status=$2,manual_control_revision=manual_control_revision+case when status='cancel_requested' then 0 else 1 end,updated_at=now() where id=$1 returning *")
        .bind(id).bind(if immediate {"cancelled"} else {"cancel_requested"}).fetch_one(&mut *tx).await?;
    if immediate {sqlx::query("delete from manual_runtime_artifacts where study_id=$1").bind(id).execute(&mut *tx).await?;}
    tx.commit().await?;Ok(Some((record(row)?,true)))
}

pub async fn fence(tx:&mut Transaction<'_,Postgres>,job:&Job)->Result<String,DbError> {
    crate::continuous::owner_lock(tx,&job.study.owner_id).await?;
    sqlx::query_scalar("select status from optimization_studies where id=$1 and owner_id=$2 and origin='manual' and manual_execution_epoch=$3 and status in ('running','pausing','cancel_requested') for update")
        .bind(job.study.id).bind(&job.study.owner_id).bind(job.epoch).fetch_optional(&mut **tx).await?.ok_or_else(||invalid("execution epoch lost"))
}
pub async fn status(pool:&PgPool,job:&Job)->Result<String,DbError> {
    sqlx::query_scalar("select status from optimization_studies where id=$1 and owner_id=$2 and manual_execution_epoch=$3 and status in ('running','pausing','cancel_requested')")
        .bind(job.study.id).bind(&job.study.owner_id).bind(job.epoch).fetch_optional(pool).await?.ok_or_else(||invalid("execution epoch lost"))
}
pub async fn save_progress(pool:&PgPool,job:&Job,progress:&Value,events:&Value)->Result<(),DbError> {
    let mut tx=pool.begin().await?;fence(&mut tx,job).await?;
    sqlx::query("update optimization_studies set progress=$2,events=$3,updated_at=now() where id=$1")
        .bind(job.study.id).bind(progress).bind(events).execute(&mut *tx).await?;tx.commit().await?;Ok(())
}
pub async fn pause_ack(pool:&PgPool,job:&Job,progress:&Value,events:&Value)->Result<bool,DbError> {
    let mut tx=pool.begin().await?;
    crate::continuous::owner_lock(&mut tx,&job.study.owner_id).await?;
    let acknowledged:bool=sqlx::query_scalar("select exists(select 1 from optimization_studies where id=$1 and owner_id=$2 and manual_execution_epoch=$3 and status='paused' and not manual_execution_reserved)")
        .bind(job.study.id).bind(&job.study.owner_id).bind(job.epoch).fetch_one(&mut *tx).await?;
    if acknowledged {tx.commit().await?;return Ok(true);}
    if fence(&mut tx,job).await?!="pausing" {return Ok(false);}
    let valid:bool=sqlx::query_scalar("select exists(select 1 from manual_runtime_artifacts where study_id=$1 and snapshot_manifest is not null and checkpoint is not null)")
        .bind(job.study.id).fetch_one(&mut *tx).await?;
    if !valid {return Err(invalid("pause without committed input/checkpoint"));}
    sqlx::query("update optimization_studies set status='paused',manual_execution_reserved=false,manual_spent_ms=manual_spent_ms+greatest(0,coalesce((extract(epoch from(clock_timestamp()-manual_segment_started_at))*1000)::bigint,0)),manual_segment_started_at=null,progress=$2,events=$3,updated_at=now() where id=$1")
        .bind(job.study.id).bind(progress).bind(events).execute(&mut *tx).await?;tx.commit().await?;Ok(true)
}

/// Only the owner of the detached RUNTIME_LOCK connection calls this. Expiry of
/// a continuous lease is deliberately not permission to clear a manual holder.
pub async fn recover(pool:&PgPool)->Result<u64,DbError> {
    let owners:Vec<String>=sqlx::query_scalar("select distinct owner_id from optimization_studies where origin='manual' and (manual_execution_reserved or status='pausing')").fetch_all(pool).await?;
    let mut changed=0;
    for owner in owners {
        let mut tx=pool.begin().await?;crate::continuous::owner_lock(&mut tx,&owner).await?;
        // A pending user Pause may survive a crash, but a successful-looking
        // paused state requires an intact acknowledged private boundary. Page
        // checkpoints one at a time; never retain 64 * 16 MiB in recovery RAM.
        let mut cursor=0;
        loop {
            let row=sqlx::query("select * from optimization_studies where owner_id=$1 and origin='manual' and status='pausing' and id>$2 order by id limit 1")
                .bind(&owner).bind(cursor).fetch_optional(&mut *tx).await?;
            let Some(row)=row else {break;};let head=job(row)?;cursor=head.study.id;
            let valid=match artifact(pool,&head,&Limits::default()).await {
                Ok(Some(a))=>if let (Some(manifest),Some(_))=(a.manifest,a.checkpoint) {
                    if manifest.validate(Limits::default().max_runtime_bytes as usize).is_err(){false}else{
                        let (count,bytes):(i64,i64)=sqlx::query_as("select count(*),coalesce(sum(octet_length(payload)),0)::bigint from manual_frozen_chunks where study_id=$1")
                            .bind(cursor).fetch_one(&mut *tx).await?;
                        count==i64::from(manifest.chunks) && bytes==manifest.encoded_bytes as i64
                    }
                }else{false},
                Ok(None)=>false,
                Err(DbError::Json(_))=>false,
                Err(error)=>return Err(error),
            };
            if !valid {sqlx::query("update manual_runtime_artifacts set checkpoint=null,checkpoint_hash=null where study_id=$1").bind(cursor).execute(&mut *tx).await?;}
        }
        changed+=sqlx::query("update optimization_studies s set status=case when manual_format_version is null then 'failed' when status='pausing' then case when exists(select 1 from manual_runtime_artifacts a where a.study_id=s.id and a.snapshot_manifest is not null and a.checkpoint is not null and a.checkpoint_hash is not null) then 'paused' else 'failed' end when status='cancel_requested' then 'cancel_requested' else 'queued' end,error=case when manual_format_version is null then jsonb_build_object('code','interrupted_by_restart','message','Legacy RAM-only input was lost') when status='pausing' and not exists(select 1 from manual_runtime_artifacts a where a.study_id=s.id and a.snapshot_manifest is not null and a.checkpoint is not null and a.checkpoint_hash is not null) then jsonb_build_object('code','manual_pause_checkpoint_incomplete','message','Restart interrupted Pause before a committed snapshot/checkpoint; start a new study') else error end,manual_execution_reserved=false,manual_execution_epoch=manual_execution_epoch+1,manual_spent_ms=manual_spent_ms+greatest(0,coalesce((extract(epoch from(clock_timestamp()-manual_segment_started_at))*1000)::bigint,0)),manual_segment_started_at=null,updated_at=now() where owner_id=$1 and origin='manual' and (manual_execution_reserved or status='pausing')")
            .bind(&owner).execute(&mut *tx).await?.rows_affected();
        sqlx::query("delete from manual_runtime_artifacts a using optimization_studies s where a.study_id=s.id and s.owner_id=$1 and s.status in ('completed','cancelled','failed')").bind(&owner).execute(&mut *tx).await?;
        tx.commit().await?;
    }
    Ok(changed)
}

#[derive(Debug)]
pub struct Artifact { pub identity:Value,pub manifest:Option<SnapshotManifest>,pub checkpoint:Option<Vec<u8>>,pub max_encoded:usize,pub observation_cutoff:Option<i64> }
pub async fn artifact(pool:&PgPool,job:&Job,limits:&Limits)->Result<Option<Artifact>,DbError> {
    let row=sqlx::query("select * from manual_runtime_artifacts where study_id=$1").bind(job.study.id).fetch_optional(pool).await?;
    row.map(|row| {
        let checkpoint:Option<Vec<u8>>=row.try_get("checkpoint")?;
        if let Some(bytes)=&checkpoint {if bytes.len()>limits.max_checkpoint_bytes as usize || row.get::<Option<String>,_>("checkpoint_hash").as_deref()!=Some(checksum(bytes).as_str()) {return Err(invalid("checkpoint checksum/limit"));}}
        let manifest:Option<Value>=row.try_get("snapshot_manifest")?;
        Ok(Artifact{identity:row.try_get("identity")?,observation_cutoff:row.try_get("observation_cutoff")?,manifest:manifest.map(serde_json::from_value).transpose().map_err(crate::json_error)?,checkpoint,
            max_encoded:row.get::<i64,_>("reserved_bytes").saturating_sub(limits.headroom()) as usize})
    }).transpose()
}
pub async fn reserve(pool:&PgPool,job:&Job,identity:&Value,desired_encoded:usize,limits:&Limits)->Result<usize,DbError> {
    if !limits.validate() || serde_json::to_vec(identity).map_err(crate::json_error)?.len()>4096 {return Err(invalid("identity/storage limits"));}
    let mut tx=pool.begin().await?;fence(&mut tx,job).await?;
    sqlx::query("select pg_advisory_xact_lock(72423412)").execute(&mut *tx).await?;
    if let Some(row)=sqlx::query("select identity,snapshot_manifest,reserved_bytes from manual_runtime_artifacts where study_id=$1 for update").bind(job.study.id).fetch_optional(&mut *tx).await? {
        if row.get::<Value,_>("identity")!=*identity {return Err(invalid("identity incompatible"));}
        if row.get::<Option<Value>,_>("snapshot_manifest").is_some() {
            let cap=row.get::<i64,_>("reserved_bytes")-limits.headroom();tx.commit().await?;return Ok(cap as usize);
        }
        sqlx::query("delete from manual_runtime_artifacts where study_id=$1").bind(job.study.id).execute(&mut *tx).await?;
    }
    let other:i64=sqlx::query_scalar("select coalesce(sum(reserved_bytes),0)::bigint from manual_runtime_artifacts").fetch_one(&mut *tx).await?;
    let encoded=(desired_encoded.min(i64::MAX as usize) as i64).min(limits.max_runtime_bytes-other-limits.headroom());
    if encoded<=0 {return Err(invalid("runtime storage resource_limit"));}
    sqlx::query("insert into manual_runtime_artifacts(study_id,identity,reserved_bytes) values($1,$2,$3)").bind(job.study.id).bind(identity).bind(encoded+limits.headroom()).execute(&mut *tx).await?;
    let pending=crate::admission::active_reservation_bytes(&mut tx,&Default::default(),None).await.map_err(|e|invalid(&e.to_string()))?;
    let actual:i64=sqlx::query_scalar("select pg_database_size(current_database())").fetch_one(&mut *tx).await?;
    if actual.saturating_add(pending)>limits.max_database_bytes {return Err(invalid("database storage resource_limit"));}
    tx.commit().await?;Ok(encoded as usize)
}
pub async fn write_chunk(pool:&PgPool,job:&Job,index:u32,payload:&[u8],limits:&Limits)->Result<(),DbError> {
    if payload.is_empty() || payload.len()>CHUNK_BYTES || index>i32::MAX as u32 {return Err(invalid("chunk limit"));}
    let mut tx=pool.begin().await?;
    if fence(&mut tx,job).await?=="cancel_requested" {return Err(invalid("manual_snapshot_cancelled"));}
    let row=sqlx::query("select snapshot_manifest,snapshot_bytes,reserved_bytes,observation_cutoff from manual_runtime_artifacts where study_id=$1 for update").bind(job.study.id).fetch_one(&mut *tx).await?;
    if row.get::<Option<Value>,_>("snapshot_manifest").is_some() {return Err(invalid("committed input immutable"));}
    if let Some(old)=sqlx::query_scalar::<_,String>("select checksum from manual_frozen_chunks where study_id=$1 and chunk_index=$2").bind(job.study.id).bind(index as i32).fetch_optional(&mut *tx).await? {
        if old!=checksum(payload) {return Err(invalid("different repeated chunk"));}return Ok(());
    }
    let previous:i64=row.get("snapshot_bytes");let total=previous+payload.len() as i64;
    if previous!=i64::from(index)*CHUNK_BYTES as i64 || total+limits.headroom()>row.get::<i64,_>("reserved_bytes") {return Err(invalid("chunk order/storage resource_limit"));}
    sqlx::query("insert into manual_frozen_chunks(study_id,chunk_index,payload,checksum) values($1,$2,$3,$4)").bind(job.study.id).bind(index as i32).bind(payload).bind(checksum(payload)).execute(&mut *tx).await?;
    sqlx::query("update manual_runtime_artifacts set snapshot_bytes=$2 where study_id=$1").bind(job.study.id).bind(total).execute(&mut *tx).await?;tx.commit().await?;Ok(())
}
pub async fn seal(pool:&PgPool,job:&Job,manifest:&SnapshotManifest,observation_cutoff:i64,limits:&Limits)->Result<(),DbError> {
    manifest.validate(limits.max_runtime_bytes as usize)?;
    let mut tx=pool.begin().await?;fence(&mut tx,job).await?;
    let row=sqlx::query("select snapshot_manifest,snapshot_bytes,reserved_bytes,observation_cutoff from manual_runtime_artifacts where study_id=$1 for update").bind(job.study.id).fetch_one(&mut *tx).await?;
    let value=serde_json::to_value(manifest).map_err(crate::json_error)?;
    if let Some(old)=row.get::<Option<Value>,_>("snapshot_manifest") {if old!=value || row.get::<Option<i64>,_>("observation_cutoff")!=Some(observation_cutoff) {return Err(invalid("committed manifest/cutoff immutable"));}}
    let (count,bytes,short):(i64,i64,i64)=sqlx::query_as("select count(*),coalesce(sum(octet_length(payload)),0)::bigint,count(*) filter(where chunk_index<>$2 and octet_length(payload)<>1048576) from manual_frozen_chunks where study_id=$1").bind(job.study.id).bind(manifest.chunks as i32-1).fetch_one(&mut *tx).await?;
    if count!=i64::from(manifest.chunks) || bytes!=manifest.encoded_bytes as i64 || bytes!=row.get::<i64,_>("snapshot_bytes") || short!=0 || bytes+limits.headroom()>row.get::<i64,_>("reserved_bytes") {return Err(invalid("incomplete snapshot"));}
    sqlx::query("update manual_runtime_artifacts set snapshot_manifest=$2,reserved_bytes=snapshot_bytes+$3,observation_cutoff=$4 where study_id=$1").bind(job.study.id).bind(value).bind(limits.headroom()).bind(observation_cutoff).execute(&mut *tx).await?;tx.commit().await?;Ok(())
}
pub async fn read_chunk(pool:&PgPool,job:&Job,manifest:&SnapshotManifest,index:u32)->Result<Vec<u8>,DbError> {
    if index>=manifest.chunks {return Err(invalid("chunk outside manifest"));}
    let mut tx=pool.begin().await?;
    if fence(&mut tx,job).await?=="cancel_requested" {return Err(invalid("manual_snapshot_cancelled"));}
    let row=sqlx::query("select c.payload,c.checksum from manual_frozen_chunks c join manual_runtime_artifacts a using(study_id) where c.study_id=$1 and c.chunk_index=$2 and a.snapshot_manifest=$3")
        .bind(job.study.id).bind(index as i32).bind(serde_json::to_value(manifest).map_err(crate::json_error)?).fetch_optional(&mut *tx).await?.ok_or_else(||invalid("missing chunk/changed manifest"))?;
    let bytes:Vec<u8>=row.get("payload");let expected=if index+1==manifest.chunks {((manifest.encoded_bytes-1)%CHUNK_BYTES as u64+1) as usize}else{CHUNK_BYTES};
    if bytes.len()!=expected || checksum(&bytes)!=row.get::<String,_>("checksum") {return Err(invalid("chunk checksum/length"));}
    tx.commit().await?;Ok(bytes)
}
pub async fn checkpoint(pool:&PgPool,job:&Job,sequence:u64,bytes:&[u8],limits:&Limits)->Result<String,DbError> {
    if bytes.is_empty() || bytes.len()>limits.max_checkpoint_bytes as usize || sequence>i64::MAX as u64 {return Err(invalid("checkpoint resource_limit"));}
    let mut tx=pool.begin().await?;let status=fence(&mut tx,job).await?;
    let row=sqlx::query("select snapshot_manifest,checkpoint_sequence,checkpoint_hash from manual_runtime_artifacts where study_id=$1 for update").bind(job.study.id).fetch_one(&mut *tx).await?;
    if row.get::<Option<Value>,_>("snapshot_manifest").is_none() {return Err(invalid("checkpoint before snapshot"));}
    let old:i64=row.get("checkpoint_sequence");let hash=checksum(bytes);
    let old_hash:Option<String>=row.get("checkpoint_hash");
    if (sequence as i64)<old || ((sequence as i64)==old && old_hash.as_ref().is_some_and(|old|*old!=hash)) {return Err(invalid("checkpoint sequence/hash conflict"));}
    if sequence as i64==old && old_hash.as_ref()==Some(&hash) {tx.commit().await?;return Ok(status);}
    sqlx::query("update manual_runtime_artifacts set checkpoint=$2,checkpoint_hash=$3,checkpoint_sequence=$4 where study_id=$1").bind(job.study.id).bind(bytes).bind(hash).bind(sequence as i64).execute(&mut *tx).await?;
    tx.commit().await?;Ok(status)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn finite_manual_budgets_and_bounded_idempotency_keys() {
        assert!(Limits::default().validate());
        assert!(!Limits{max_pending:65,..Default::default()}.validate());
        assert!(!Limits{max_checkpoint_bytes:16*1024*1024+1,..Default::default()}.validate());
        assert!(!Limits{max_runtime_bytes:1,..Default::default()}.validate());
        assert!(!Limits{max_database_bytes:1,..Default::default()}.validate());
        assert!(valid_key("request-42"));assert!(valid_key(&"a".repeat(128)));
        for key in ["", "white space", "\n", "кириллица", &"x".repeat(129)] {assert!(!valid_key(key));}
    }
}
