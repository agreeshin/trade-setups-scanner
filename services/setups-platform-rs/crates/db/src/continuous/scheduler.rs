//! Short transactional transitions; raw frozen inputs belong exclusively to the executor.
use super::{owner_lock, cleanup_stopped,cleanup_stopped_inner};
use crate::DbError;
use chrono::Utc;
use serde_json::{Value, json};
use sqlx::{PgPool, Postgres, Row, Transaction};

pub const LEASE_SECONDS: i64 = 30;
pub const MAX_TECHNICAL_ATTEMPTS: i32 = 3;

#[derive(Debug, Clone)]
pub struct OwnerLease {
    pub owner_id: String,
    pub worker: String,
    pub token: i64,
}

#[derive(Debug, Clone)]
pub struct ChainJob {
    pub lease: OwnerLease,
    pub algorithm_id: String,
    pub generation_id: i64,
    pub slot_id: i64,
    pub study_id: i64,
    pub config_revision: i64,
    pub observation_cutoff: i64,
    pub config: Value,
    pub spec: Value,
    pub execution_started_at: chrono::DateTime<Utc>,
    pub research_deadline: Option<chrono::DateTime<Utc>>,
}

pub async fn claim_owner(pool: &PgPool, worker: &str) -> Result<Option<OwnerLease>, DbError> {
    let mut tx=pool.begin().await?;
    let owner: Option<String>=sqlx::query_scalar(r#"select l.owner_id from continuous_owner_lanes l
        where (l.lease_owner is null or l.lease_until<=now()) and exists(
            select 1 from continuous_processes p
            left join continuous_generations g on g.id=p.active_generation_id and g.status='running'
            where p.owner_id=l.owner_id and (p.status='stopping' or (
                p.desired_running and p.next_attempt_at<=now()
                and coalesce(p.error->>'code','')<>'durable_resume_retry_exhausted'
                and exists(select 1 from continuous_slots s where s.owner_id=p.owner_id and s.algorithm_id=p.algorithm_id and s.enabled)
                and ((g.id is not null and (p.retry_count<3 or (g.resume_version=1 and p.retry_count=3)))
                    or (g.id is null and p.retry_count<3
                        and not exists(select 1 from continuous_generations a where a.owner_id=l.owner_id and a.status='running')
                        and not coalesce((select admission_paused from continuous_maintenance where owner_id=l.owner_id),false)))
            ))) order by l.lease_until nulls first,l.owner_id for update skip locked limit 1"#)
        .fetch_optional(&mut *tx).await?;
    let Some(owner_id)=owner else { return Ok(None); };
    // try-lock avoids inversion with owner_lock's advisory -> lease-row ordering.
    // No new epoch may win during an older protected commit.
    let mutation_free:bool=sqlx::query_scalar("select pg_try_advisory_xact_lock(hashtextextended($1,72423415))")
        .bind(&owner_id).fetch_one(&mut *tx).await?;
    if !mutation_free {return Ok(None);}
    let abandoned:bool=sqlx::query_scalar("select lease_owner is not null from continuous_owner_lanes where owner_id=$1").bind(&owner_id).fetch_one(&mut *tx).await?;
    let token:Option<i64>=sqlx::query_scalar("update continuous_owner_lanes set lease_owner=$2,lease_token=lease_token+1,lease_until=clock_timestamp()+$3::bigint*interval '1 second' where owner_id=$1 and (lease_owner is null or lease_until<=clock_timestamp()) returning lease_token")
        .bind(&owner_id).bind(worker).bind(LEASE_SECONDS).fetch_optional(&mut *tx).await?;
    let Some(token)=token else{return Ok(None);};
    // A third attempt that died before its failure handler still needs a single
    // metadata-only latch. This claim must not authorize a fourth execution.
    sqlx::query("update continuous_processes p set status='error',error=jsonb_build_object('code','durable_resume_retry_exhausted','message','Durable recovery retries exhausted; explicit Start retries the retained study without changing its input or deadline'),updated_at=now() from continuous_generations g where p.owner_id=$1 and p.active_generation_id=g.id and g.status='running' and g.resume_version=1 and p.desired_running and p.status<>'stopping' and p.retry_count>=3 and coalesce(p.error->>'code','')<>'durable_resume_retry_exhausted' and ($2 or g.recovery_pending or g.recovery_lease_token is null)")
        .bind(&owner_id).bind(abandoned).execute(&mut *tx).await?;
    sqlx::query("update continuous_generations g set error=p.error from continuous_processes p where g.owner_id=$1 and p.active_generation_id=g.id and g.status='running' and p.error->>'code'='durable_resume_retry_exhausted'")
        .bind(&owner_id).execute(&mut *tx).await?;
    // Only an eligible active chain gets an execution epoch. A clean release
    // continues its already-charged attempt (including attempt three), whereas
    // pending retry or an abandoned non-null owner charges the next attempt.
    let active=sqlx::query("select g.id,p.algorithm_id,g.recovery_pending from continuous_processes p join continuous_generations g on g.id=p.active_generation_id where p.owner_id=$1 and p.desired_running and p.status<>'stopping' and p.config_revision=g.config_revision and g.status='running' and g.resume_version=1 and p.next_attempt_at<=now() and coalesce(p.error->>'code','')<>'durable_resume_retry_exhausted' and (p.retry_count<3 or (p.retry_count=3 and not $2 and not g.recovery_pending and g.recovery_lease_token is not null)) for update of p")
        .bind(&owner_id).bind(abandoned).fetch_optional(&mut *tx).await?;
    if let Some(active)=active {
        let generation:i64=active.try_get("id")?;
        if abandoned || active.try_get::<bool,_>("recovery_pending")? {
            sqlx::query("update continuous_processes set retry_count=retry_count+1,status='error',error=jsonb_build_object('code','persistence_unconfirmed','message','Recovering the retained durable head after an unavailable or abandoned executor'),updated_at=now() where owner_id=$1 and algorithm_id=$2")
                .bind(&owner_id).bind(active.try_get::<String,_>("algorithm_id")?).execute(&mut *tx).await?;
        }
        sqlx::query("update continuous_generations set recovery_lease_token=$2,recovery_pending=false where id=$1")
            .bind(generation).bind(token).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(Some(OwnerLease{owner_id,worker:worker.into(),token}))
}

pub(super) async fn owns(tx: &mut Transaction<'_,Postgres>, lease: &OwnerLease) -> Result<bool,DbError> {
    Ok(sqlx::query_scalar("select exists(select 1 from continuous_owner_lanes where owner_id=$1 and lease_owner=$2 and lease_token=$3 and lease_until>clock_timestamp())")
        .bind(&lease.owner_id).bind(&lease.worker).bind(lease.token).fetch_one(&mut **tx).await?)
}

pub(super) async fn fenced(tx: &mut Transaction<'_,Postgres>, job: &ChainJob) -> Result<bool,DbError> {
    owner_lock(tx,&job.lease.owner_id).await?;
    if !owns(tx,&job.lease).await? { return Ok(false); }
    // Protect revision/Stop without excluding live scanners during bulk replay
    // persistence. Transitions updating slots upgrade the process lock FIRST.
    Ok(sqlx::query_scalar::<_,i64>("select p.config_revision from continuous_processes p join continuous_generations g on g.id=p.active_generation_id where p.owner_id=$1 and p.algorithm_id=$2 and p.desired_running and p.config_revision=$3 and p.active_generation_id=$4 and g.lease_token=$5 and (g.resume_version is distinct from 1 or g.recovery_lease_token=$5) and g.status='running' for share of p")
        .bind(&job.lease.owner_id).bind(&job.algorithm_id).bind(job.config_revision).bind(job.generation_id).bind(job.lease.token).fetch_optional(&mut **tx).await?.is_some())
}

pub async fn heartbeat<'e>(executor: impl sqlx::PgExecutor<'e>, lease: &OwnerLease) -> Result<bool,DbError> {
    Ok(sqlx::query("update continuous_owner_lanes set lease_until=clock_timestamp()+$4::bigint*interval '1 second' where owner_id=$1 and lease_owner=$2 and lease_token=$3 and lease_until>clock_timestamp()")
        .bind(&lease.owner_id).bind(&lease.worker).bind(lease.token).bind(LEASE_SECONDS).execute(executor).await?.rows_affected()==1)
}

pub async fn current<'e>(executor: impl sqlx::PgExecutor<'e>, job: &ChainJob) -> Result<bool,DbError> {
    // Advisory liveness only: never wait behind this executor's own materialization lock.
    // Every mutation still takes owner_lock and checks fenced() authoritatively.
    Ok(sqlx::query_scalar("select exists(select 1 from continuous_owner_lanes l join continuous_processes p using(owner_id) where l.owner_id=$1 and l.lease_owner=$2 and l.lease_token=$3 and l.lease_until>clock_timestamp() and p.algorithm_id=$4 and p.desired_running and p.config_revision=$5 and p.active_generation_id=$6)")
        .bind(&job.lease.owner_id).bind(&job.lease.worker).bind(job.lease.token).bind(&job.algorithm_id).bind(job.config_revision).bind(job.generation_id).fetch_one(executor).await?)
}

pub async fn release(pool: &PgPool, lease: &OwnerLease) -> Result<(),DbError> {
    let mut tx=pool.begin().await?;
    owner_lock(&mut tx,&lease.owner_id).await?;
    sqlx::query("update continuous_owner_lanes set lease_owner=null,lease_until=now() where owner_id=$1 and lease_owner=$2 and lease_token=$3")
        .bind(&lease.owner_id).bind(&lease.worker).bind(lease.token).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

/// A newly acquired token fences the expired executor before recovery touches metadata.
pub async fn recover_owner(pool: &PgPool, lease: &OwnerLease) -> Result<(),DbError> {
    let mut tx=pool.begin().await?;
    owner_lock(&mut tx,&lease.owner_id).await?;
    if !owns(&mut tx,lease).await? { return Err(DbError::Json("continuous lease lost during recovery".into())); }
    // Compatible execution is re-fenced atomically before legacy interruption.
    // Snapshot validation is performed by the executor before it uses any input.
    sqlx::query("update continuous_generations g set lease_token=$2 from continuous_processes p where g.owner_id=$1 and p.owner_id=g.owner_id and p.algorithm_id=g.algorithm_id and p.active_generation_id=g.id and p.desired_running and p.status<>'stopping' and p.config_revision=g.config_revision and g.status='running' and g.resume_version=1 and g.research_deadline is not null and g.study_id is not null")
        .bind(&lease.owner_id).bind(lease.token).execute(&mut *tx).await?;
    let error=json!({"code":"interrupted_by_restart","message":"Frozen input утрачен; новая попытка получит собственный actual-start и снимок"});
    sqlx::query("update continuous_generations set error=jsonb_build_object('code','resume_version_incompatible','message','Unsupported private continuous resume format') where owner_id=$1 and lease_token<>$2 and status='running' and resume_version is not null and resume_version<>1")
        .bind(&lease.owner_id).bind(lease.token).execute(&mut *tx).await?;
    sqlx::query("update optimization_studies s set status='failed',result=null,error=coalesce(g.error,$3),updated_at=now() from continuous_generations g where s.continuous_generation_id=g.id and g.owner_id=$1 and g.lease_token<>$2 and g.status='running' and s.status in ('queued','running','cancel_requested')")
        .bind(&lease.owner_id).bind(lease.token).bind(&error).execute(&mut *tx).await?;
    sqlx::query("update replay_runs r set status='failed',error='interrupted_by_restart',updated_at=now() from continuous_generations g where r.continuous_generation_id=g.id and g.owner_id=$1 and g.lease_token<>$2 and g.status='running' and r.status in ('queued','running','cancel_requested')")
        .bind(&lease.owner_id).bind(lease.token).execute(&mut *tx).await?;
    sqlx::query("update continuous_processes p set active_generation_id=null,active_study_id=null,phase='queued',error=(select coalesce(g.error,$3) from continuous_generations g where g.id=p.active_generation_id),status='error',retry_count=least(retry_count+1,3),next_attempt_at=now()+interval '5 seconds',updated_at=now() where p.owner_id=$1 and p.status<>'stopping' and exists(select 1 from continuous_generations g where g.id=p.active_generation_id and g.lease_token<>$2 and g.status='running')")
        .bind(&lease.owner_id).bind(lease.token).bind(&error).execute(&mut *tx).await?;
    sqlx::query("update continuous_generations set status='failed',phase='failed',error=coalesce(error,$3),updated_at=now() where owner_id=$1 and lease_token<>$2 and status='running'")
        .bind(&lease.owner_id).bind(lease.token).bind(error).execute(&mut *tx).await?;
    super::recovery::reclaim_terminal_in(&mut tx,&lease.owner_id).await?;
    tx.commit().await?;
    drain_stops(pool,lease).await
}

/// Only called after a joined chain, or by a newly acquired owner after recovery.
pub async fn drain_stops(pool: &PgPool, lease: &OwnerLease) -> Result<(),DbError> {
    let algorithms: Vec<String>=sqlx::query_scalar("select p.algorithm_id from continuous_processes p join continuous_owner_lanes l using(owner_id) where p.owner_id=$1 and p.status='stopping' and l.lease_owner=$2 and l.lease_token=$3 and l.lease_until>now() order by p.algorithm_id")
        .bind(&lease.owner_id).bind(&lease.worker).bind(lease.token).fetch_all(pool).await?;
    for algorithm in algorithms { cleanup_stopped(pool,&lease.owner_id,&algorithm,lease).await?; }
    Ok(())
}

/// Idle B has nothing owned by running A to drain. Never clean A before its join.
pub async fn drain_idle_stops(pool:&PgPool,lease:&OwnerLease,active_algorithm:&str)->Result<(),DbError> {
    let algorithms:Vec<String>=sqlx::query_scalar("select p.algorithm_id from continuous_processes p where p.owner_id=$1 and p.status='stopping' and p.algorithm_id<>$2 and not exists(select 1 from continuous_generations g where g.owner_id=p.owner_id and g.algorithm_id=p.algorithm_id and g.status='running') order by p.algorithm_id")
        .bind(&lease.owner_id).bind(active_algorithm).fetch_all(pool).await?;
    // Never park the heartbeat monitor behind its own current generation's mutation.
    for algorithm in algorithms {cleanup_stopped_inner(pool,&lease.owner_id,&algorithm,lease,false).await?;}
    Ok(())
}

pub async fn prepare_next(pool: &PgPool, lease: &OwnerLease) -> Result<Option<ChainJob>,DbError> {
    let mut tx=pool.begin().await?;
    owner_lock(&mut tx,&lease.owner_id).await?;
    if !owns(&mut tx,lease).await? { return Ok(None); }
    if sqlx::query_scalar::<_,bool>("select exists(select 1 from continuous_generations where owner_id=$1 and status='running')").bind(&lease.owner_id).fetch_one(&mut *tx).await? {return Ok(None);}
    if sqlx::query_scalar::<_,bool>("select coalesce((select admission_paused from continuous_maintenance where owner_id=$1),false)").bind(&lease.owner_id).fetch_one(&mut *tx).await? {return Ok(None);}
    let row=sqlx::query("select p.* from continuous_processes p join continuous_owner_lanes l using(owner_id) where p.owner_id=$1 and p.desired_running and p.retry_count<3 and p.next_attempt_at<=now() and exists(select 1 from continuous_slots s where s.owner_id=p.owner_id and s.algorithm_id=p.algorithm_id and s.enabled) order by (p.algorithm_id>l.last_algorithm_id) desc,p.algorithm_id for update of p limit 1")
        .bind(&lease.owner_id).fetch_optional(&mut *tx).await?;
    let Some(row)=row else { return Ok(None); };
    let algorithm: String=row.try_get("algorithm_id")?;
    let config: Value=row.try_get("config")?;
    let revision: i64=row.try_get("config_revision")?;
    let cursor: i32=row.try_get("next_slot")?;
    let thresholds=config["minSetupsPerDay"].as_array().ok_or_else(||DbError::Json("invalid persisted continuous thresholds".into()))?;
    if thresholds.is_empty() {return Err(DbError::Json("empty persisted continuous thresholds".into()));}
    let mut selected_slot = None;
    for offset in 0..thresholds.len() {
        let index = (cursor as usize+offset)%thresholds.len();
        let frequency=thresholds[index].as_f64().ok_or_else(||DbError::Json("invalid persisted continuous frequency".into()))?;
        let slot: Option<i64>=sqlx::query_scalar("select id from continuous_slots where owner_id=$1 and algorithm_id=$2 and enabled and min_setups_per_day=$3::text::numeric for update")
            .bind(&lease.owner_id).bind(&algorithm).bind(frequency.to_string()).fetch_optional(&mut *tx).await?;
        if let Some(slot_id)=slot {selected_slot=Some((slot_id,frequency,index));break;}
    }
    let Some((slot_id,frequency,slot_index))=selected_slot else {return Ok(None);};
    sqlx::query("update continuous_processes set next_slot=$3 where owner_id=$1 and algorithm_id=$2")
        .bind(&lease.owner_id).bind(&algorithm).bind(slot_index as i32).execute(&mut *tx).await?;
    // Actual execution start, never enqueue time. This timestamp and cutoff cannot move.
    let start=Utc::now();
    // PostgreSQL timestamptz stores microseconds. Initial and restored identities
    // must use the same precision, not local nanoseconds versus restored micros.
    let start=chrono::DateTime::from_timestamp_micros(start.timestamp_micros()).expect("execution timestamp fits PostgreSQL precision");
    let to=start.timestamp();
    let days=config["lookbackDays"].as_f64().ok_or_else(||DbError::Json("invalid lookbackDays".into()))?;
    let cutoff=to.div_euclid(60)*60;
    let mut spec=config["study"].clone();
    spec["fromUnix"]=json!(to-(days*86400.0) as i64);
    spec["toUnix"]=json!(to);
    spec["minSetupsPerDay"]=json!(frequency);
    spec["setupMaturity"]=json!(0);
    let seconds=spec.get("maxSeconds").and_then(Value::as_i64).unwrap_or(3600);
    if !(1..=86400).contains(&seconds) {return Err(DbError::Json("invalid persisted search deadline".into()));}
    let deadline=start+chrono::Duration::seconds(seconds);
    let generation_id: i64=sqlx::query_scalar("insert into continuous_generations(owner_id,algorithm_id,slot_id,config_revision,lease_token,spec,execution_started_at,observation_cutoff,resume_version,research_deadline,recovery_lease_token) values($1,$2,$3,$4,$5,$6,$7,$8,1,$9,$5) returning id")
        .bind(&lease.owner_id).bind(&algorithm).bind(slot_id).bind(revision).bind(lease.token).bind(&spec).bind(start).bind(cutoff).bind(deadline).fetch_one(&mut *tx).await?;
    let progress=json!({"phase":"acquiring","evaluatedCandidates":0,"scorableCandidates":0,"maxTrials":spec["maxTrials"],"elapsedSeconds":0.0,"bestScore":null});
    let study_id: i64=sqlx::query_scalar("insert into optimization_studies(algorithm_id,status,spec,progress,origin,owner_id,continuous_generation_id,continuous_slot_id) values($1,'running',$2,$3,'continuous',$4,$5,$6) returning id")
        .bind(&algorithm).bind(&spec).bind(progress).bind(&lease.owner_id).bind(generation_id).bind(slot_id).fetch_one(&mut *tx).await?;
    sqlx::query("update continuous_generations set study_id=$2 where id=$1").bind(generation_id).bind(study_id).execute(&mut *tx).await?;
    sqlx::query("update continuous_processes set status='running',active_generation_id=$3,active_study_id=$4,phase='acquiring',error=null,updated_at=now() where owner_id=$1 and algorithm_id=$2")
        .bind(&lease.owner_id).bind(&algorithm).bind(generation_id).bind(study_id).execute(&mut *tx).await?;
    sqlx::query("update continuous_slots set status='acquiring',study_id=$2,updated_at=now() where id=$1").bind(slot_id).bind(study_id).execute(&mut *tx).await?;
    sqlx::query("update continuous_owner_lanes set last_algorithm_id=$2 where owner_id=$1").bind(&lease.owner_id).bind(&algorithm).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(Some(ChainJob{lease:lease.clone(),algorithm_id:algorithm,generation_id,slot_id,study_id,config_revision:revision,observation_cutoff:cutoff,config,spec,execution_started_at:start,research_deadline:Some(deadline)}))
}

pub async fn phase(pool: &PgPool, job: &ChainJob, phase: &str, progress: Option<&Value>) -> Result<bool,DbError> {
    let mut tx=pool.begin().await?;
    if !fenced(&mut tx,job).await? { return Ok(false); }
    sqlx::query("update continuous_processes set phase=$3,updated_at=now() where owner_id=$1 and algorithm_id=$2")
        .bind(&job.lease.owner_id).bind(&job.algorithm_id).bind(phase).execute(&mut *tx).await?;
    sqlx::query("update continuous_generations set phase=$2,updated_at=now() where id=$1").bind(job.generation_id).bind(phase).execute(&mut *tx).await?;
    sqlx::query("update continuous_slots set status=$2,updated_at=now() where id=$1").bind(job.slot_id).bind(phase).execute(&mut *tx).await?;
    if let Some(progress)=progress {
        sqlx::query("update optimization_studies set progress=$2,updated_at=now() where id=$1 and status='running'").bind(job.study_id).bind(progress).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(true)
}

/// Fully completed search may win a Stop race; preserve it, but never activate its slot here.
pub async fn finish_search(pool: &PgPool, job: &ChainJob, outcome: &Value, completed: bool) -> Result<(),DbError> {
    let mut tx=pool.begin().await?;
    owner_lock(&mut tx,&job.lease.owner_id).await?;
    if !owns(&mut tx,&job.lease).await? { return Err(DbError::Json("continuous search lease lost".into())); }
    let advanced=sqlx::query("update optimization_studies set status=$2,progress=$3,result=$4,updated_at=now() where id=$1 and origin='continuous' and continuous_generation_id=$5 and status in ('running','cancel_requested')")
        .bind(job.study_id).bind(if completed {"completed"} else {"cancelled"}).bind(&outcome["progress"]).bind(outcome).bind(job.generation_id).execute(&mut *tx).await?.rows_affected();
    if advanced>0 {super::recovery::acknowledge_boundary(&mut tx,job).await?;}
    tx.commit().await?;
    Ok(())
}

pub struct ServiceReplayRun {
    pub exchange: String,
    pub category: String,
    pub symbol: String,
    pub setups: Value,
    pub warmup_minutes: i64,
    pub evaluated_minutes: i64,
}

pub struct StorageAdmission {
    pub admitted: bool,
    pub limit: i64,
    pub requested: i64,
}

/// Same short storage-admission lock as ordinary replay, but independent pending/CPU lanes.
/// History is never silently deleted to make space; an exhausted budget is technical failure.
pub async fn reserve_storage(pool:&PgPool,job:&ChainJob,estimated_bytes:i64,limit:i64,manual:&crate::admission::ReplayAdmissionLimits)->Result<Option<StorageAdmission>,DbError> {
    if estimated_bytes<0 || limit<=0 {return Err(DbError::Json("invalid continuous storage admission budget".into()));}
    let mut tx=pool.begin().await?;
    if !fenced(&mut tx,job).await? {return Ok(None);}
    sqlx::query("select pg_advisory_xact_lock(72423412)").execute(&mut *tx).await?;
    let current:i64=sqlx::query_scalar("select pg_database_size(current_database())::bigint").fetch_one(&mut *tx).await?;
    let reserved=crate::admission::active_reservation_bytes(&mut tx,manual,None).await.map_err(|error|match error {
        crate::admission::QueueError::Db(error)=>error,
        error=>DbError::Json(error.to_string()),
    })?;
    let previous:i64=sqlx::query_scalar("select reserved_bytes from continuous_generations where id=$1").bind(job.generation_id).fetch_one(&mut *tx).await?;
    // Physical output is already included in pg_database_size. A resumed chain
    // replaces its own remaining reservation, never adds a second full estimate.
    let materialized:i64=sqlx::query_scalar("select coalesce((select sum(pg_column_size(setups))::bigint from replay_runs where continuous_generation_id=$1),0)+coalesce((select pg_column_size(replay_manifest)::bigint from continuous_generations where id=$1),0)+coalesce((select pg_column_size(result)::bigint from optimization_studies where id=$2),0)")
        .bind(job.generation_id).bind(job.study_id).fetch_one(&mut *tx).await?;
    let remaining=estimated_bytes.saturating_sub(materialized).max(0);
    let requested=current.saturating_add(reserved.saturating_sub(previous)).saturating_add(remaining);
    let admitted=requested<=limit;
    if admitted {sqlx::query("update continuous_generations set reserved_bytes=$2 where id=$1").bind(job.generation_id).bind(remaining).execute(&mut *tx).await?;}
    tx.commit().await?;
    Ok(Some(StorageAdmission{admitted,limit,requested}))
}

pub async fn persist_replay(pool: &PgPool, job: &ChainJob, params: &Value, mut manifest: Value, runs: &[ServiceReplayRun]) -> Result<Option<i64>,DbError> {
    if runs.is_empty() || manifest["instruments"].as_array().map(Vec::len)!=Some(runs.len()) { return Err(DbError::Json("invalid continuous replay scope".into())); }
    let mut tx=pool.begin().await?;
    if !fenced(&mut tx,job).await? { return Ok(None); }
    let existing:Option<i32>=sqlx::query_scalar("select replay_batch_id from continuous_generations where id=$1").bind(job.generation_id).fetch_one(&mut *tx).await?;
    if let Some(id)=existing {return Ok(Some(i64::from(id)));}
    super::recovery::acknowledge_boundary(&mut tx,job).await?;
    let batch: i32=sqlx::query_scalar("insert into replay_batches(exchange_code,category,algorithm_id,algorithm_version,params,from_ts,to_ts,instrument_count,name,origin,owner_id,continuous_generation_id) values($1,$2,$3,$4,$5,to_timestamp($6),to_timestamp($7),$8,$9,'continuous',$10,$11) returning id")
        .bind(if runs.iter().all(|r|r.exchange==runs[0].exchange){runs[0].exchange.as_str()}else{"mixed"})
        .bind(if runs.iter().all(|r|r.category==runs[0].category){runs[0].category.as_str()}else{"mixed"})
        .bind(&job.algorithm_id).bind(job.spec["algorithmVersion"].as_str().ok_or_else(||DbError::Json("missing algorithmVersion".into()))?).bind(params)
        .bind(job.spec["fromUnix"].as_i64().unwrap() as f64).bind(job.spec["toUnix"].as_i64().unwrap() as f64)
        .bind(runs.len() as i32).bind(format!("Continuous · {} · {}",job.algorithm_id,job.study_id)).bind(&job.lease.owner_id).bind(job.generation_id).fetch_one(&mut *tx).await?;
    for (i,run) in runs.iter().enumerate() {
        let id: i32=sqlx::query_scalar("insert into replay_runs(exchange_code,category,symbol,from_ts,to_ts,algorithm_id,algorithm_version,params,status,cache_hit,setups,warmup_minutes,processed_minutes,batch_id,name,origin,owner_id,continuous_generation_id) values($1,$2,$3,to_timestamp($4),to_timestamp($5),$6,$7,$8,'done',false,$9,$10,$11,$12,$13,'continuous',$14,$15) returning id")
            .bind(&run.exchange).bind(&run.category).bind(&run.symbol).bind(job.spec["fromUnix"].as_i64().unwrap() as f64).bind(job.spec["toUnix"].as_i64().unwrap() as f64)
            .bind(&job.algorithm_id).bind(job.spec["algorithmVersion"].as_str().unwrap()).bind(params).bind(&run.setups)
            .bind(run.warmup_minutes as i32).bind(run.evaluated_minutes as i32).bind(batch).bind(format!("Continuous · {}",job.study_id))
            .bind(&job.lease.owner_id).bind(job.generation_id).fetch_one(&mut *tx).await?;
        manifest["instruments"][i]["runId"]=json!(id);
    }
    sqlx::query("update continuous_generations set replay_batch_id=$2,replay_manifest=$3,phase='analytics',updated_at=now() where id=$1")
        .bind(job.generation_id).bind(batch).bind(manifest).execute(&mut *tx).await?;
    if !owns(&mut tx,&job.lease).await? {return Ok(None);}
    tx.commit().await?;
    Ok(Some(i64::from(batch)))
}

/// Read the actual durable completed setup set. No market-data/cache reread is involved.
pub async fn load_replay(pool: &PgPool, job: &ChainJob, max_bytes: usize) -> Result<Value,DbError> {
    let mut tx=pool.begin().await?;
    let mut manifest: Value=sqlx::query_scalar("select replay_manifest from continuous_generations where id=$1 and owner_id=$2")
        .bind(job.generation_id).bind(&job.lease.owner_id).fetch_one(&mut *tx).await?;
    let bytes: i64=sqlx::query_scalar("select coalesce(sum(octet_length(setups::text)),0)::bigint from replay_runs where continuous_generation_id=$1")
        .bind(job.generation_id).fetch_one(&mut *tx).await?;
    if bytes<0 || bytes as u64>max_bytes as u64 { return Err(DbError::Json("persisted replay exceeds continuous memory budget".into())); }
    let items=manifest["instruments"].as_array_mut().ok_or_else(||DbError::Json("missing persisted replay manifest".into()))?;
    for item in items {
        let id=item["runId"].as_i64().ok_or_else(||DbError::Json("missing service run identity".into()))?;
        item["setups"]=sqlx::query_scalar::<_,Value>("select setups from replay_runs where id=$1 and continuous_generation_id=$2 and owner_id=$3 and origin='continuous' and status='done'")
            .bind(id as i32).bind(job.generation_id).bind(&job.lease.owner_id).fetch_one(&mut *tx).await?;
        item.as_object_mut().unwrap().remove("runId");
    }
    tx.commit().await?;
    Ok(manifest)
}

pub async fn finalize(pool: &PgPool, job: &ChainJob, result: &Value, accepted: bool, mean: Option<f64>, selected: Option<&Value>) -> Result<bool,DbError> {
    let mut tx=pool.begin().await?;
    // A lost response may retry finalization, including after a newer owner won.
    // An exact already-completed result acknowledges success without any writes.
    let completed:bool=sqlx::query_scalar("select exists(select 1 from continuous_generations where id=$1 and owner_id=$2 and config_revision=$3 and status='completed' and result=$4 and accepted=$5)")
        .bind(job.generation_id).bind(&job.lease.owner_id).bind(job.config_revision).bind(result).bind(accepted).fetch_one(&mut *tx).await?;
    if completed {return Ok(true);}
    if !fenced(&mut tx,job).await? { return Ok(false); }
    // Process -> slot, like scan commit and promotion. No market/CPU work here.
    sqlx::query("select owner_id from continuous_processes where owner_id=$1 and algorithm_id=$2 for update")
        .bind(&job.lease.owner_id).bind(&job.algorithm_id).execute(&mut *tx).await?;
    let ready: bool=sqlx::query_scalar("select exists(select 1 from continuous_generations g join optimization_studies s on s.id=g.study_id where g.id=$1 and s.status='completed' and (not $2 or (g.replay_batch_id is not null and g.replay_manifest is not null and exists(select 1 from replay_runs r where r.batch_id=g.replay_batch_id) and not exists(select 1 from replay_runs r where r.batch_id=g.replay_batch_id and r.status<>'done'))))")
        .bind(job.generation_id).bind(accepted).fetch_one(&mut *tx).await?;
    if !ready || (accepted && (!result["recommendation"].is_object() || !result["analytics"].is_object() || mean.is_none() || selected.is_none())) {
        return Err(DbError::Json("continuous generation is not fully ready for atomic finalization".into()));
    }
    sqlx::query("update continuous_generations set status='completed',phase='completed',result=$2,accepted=$3,error=null,updated_at=now() where id=$1")
        .bind(job.generation_id).bind(result).bind(accepted).execute(&mut *tx).await?;
    if accepted {
        // Frozen search/replay/analytics proof is complete before this call. A
        // research-prefix manifest is not an independent-live prerequisite.
        super::live::supersede_pending(&mut tx,job.slot_id).await?;
        let installed=sqlx::query("update continuous_slots set pending_generation_id=$2,pending_status='warming',pending_error=null,readiness_frontier=null,status='warming',study_id=$3,updated_at=now() where id=$1")
            .bind(job.slot_id).bind(job.generation_id).bind(job.study_id).execute(&mut *tx).await?.rows_affected();
        if installed!=1 {return Err(DbError::Json("continuous candidate slot disappeared during finalization".into()));}
    } else {
        sqlx::query("update continuous_slots set active_generation_id=null,pending_generation_id=null,pending_status=null,pending_error=null,readiness_frontier=null,status='rejected',study_id=$2,mean_expectancy=$3,selected_cell=$4,updated_at=now() where id=$1")
            .bind(job.slot_id).bind(job.study_id).bind(mean).bind(selected).execute(&mut *tx).await?;
        super::live::retire_inactive_checkpoints(&mut tx,&job.lease.owner_id,&job.algorithm_id,Some(job.slot_id)).await?;
        sqlx::query("update adaptive_live_warmup_lane set slot_id=null,generation_id=null,lease_owner=null,lease_until=null,lease_token=lease_token+1 where slot_id=$1")
            .bind(job.slot_id).execute(&mut *tx).await?;
    }
    sqlx::query("update continuous_processes set active_generation_id=null,active_study_id=null,phase='queued',status='running',retry_count=0,error=null,next_slot=(next_slot+1)%jsonb_array_length(config->'minSetupsPerDay'),next_attempt_at=now()+interval '1 second',updated_at=now() where owner_id=$1 and algorithm_id=$2")
        .bind(&job.lease.owner_id).bind(&job.algorithm_id).execute(&mut *tx).await?;
    // Final result includes compact bootstrap/proof; independent live needs no raw snapshot.
    sqlx::query("delete from continuous_runtime_artifacts where generation_id=$1").bind(job.generation_id).execute(&mut *tx).await?;
    if !owns(&mut tx,&job.lease).await? {return Ok(false);}
    tx.commit().await?;
    Ok(true)
}

pub async fn fail(pool: &PgPool, job: &ChainJob, error: &Value) -> Result<(),DbError> {
    let mut tx=pool.begin().await?;
    if !fenced(&mut tx,job).await? { return Ok(()); }
    sqlx::query("update optimization_studies set status='failed',result=null,error=$2,updated_at=now() where id=$1 and status in ('running','cancel_requested')")
        .bind(job.study_id).bind(error).execute(&mut *tx).await?;
    sqlx::query("update continuous_generations set status='failed',phase='failed',error=$2,updated_at=now() where id=$1")
        .bind(job.generation_id).bind(error).execute(&mut *tx).await?;
    // Accepted slot data stays intact on every technical failure. Three attempts, then explicit restart.
    sqlx::query("update continuous_processes set status='error',phase=null,error=$3,retry_count=least(retry_count+1,3),next_attempt_at=now()+(case when retry_count=0 then 1 else 5 end)*interval '1 second',active_generation_id=null,active_study_id=null,updated_at=now() where owner_id=$1 and algorithm_id=$2")
        .bind(&job.lease.owner_id).bind(&job.algorithm_id).bind(error).execute(&mut *tx).await?;
    sqlx::query("update continuous_slots set status='error',updated_at=now() where id=$1").bind(job.slot_id).execute(&mut *tx).await?;
    sqlx::query("delete from continuous_runtime_artifacts where generation_id=$1").bind(job.generation_id).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}
