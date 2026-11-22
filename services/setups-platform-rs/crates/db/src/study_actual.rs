//! Durable ownership for replaceable study diagnostics; never reads/writes live outcomes.
use crate::{
    DbError,
    admission::{ReplayAdmissionLimits, active_reservation_bytes},
};
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use sqlx::{Connection, PgConnection, PgPool, Postgres, Row, Transaction};
#[path = "study_actual_continuous.rs"]
pub mod continuous;

pub const QUEUE_LOCK: i64 = 72_423_412;
pub const RUNTIME_LOCK: i64 = 72_423_437;
pub const MAX_OUTPUT_BYTES: i64 = 64 * 1024 * 1024;
// Payload cap plus conservative tuple/index/TOAST, source and report overhead.
pub const RESERVED_BYTES: i64 = MAX_OUTPUT_BYTES * 4 + 8 * 1024 * 1024;
pub const MAX_ROWS: i64 = 100_000;

/// Scalar projection for a `study_actualizations a` head. The page index serves
/// MAX without transferring trades; staging and other owners/revisions stay out.
pub const LAST_SETUP_CLOSE_UNIX_SQL: &str = "(select max(s.signal_close_unix) from study_actual_setups s where s.owner_id=a.owner_id and s.study_id=a.study_id and s.revision=a.revision and a.status='completed' and a.batch_id is not null)";

pub(crate) fn head_sql() -> String {
    format!(
        "select (to_jsonb(a)-'source')||jsonb_build_object('sourceFacts',a.source-'artifact','last_setup_close_unix',{LAST_SETUP_CLOSE_UNIX_SQL}) from study_actualizations a where owner_id=$1 and study_id=$2"
    )
}

#[derive(Debug)]
pub enum Admission {
    Created(Value),
    Busy(Value),
    Resource,
    SourceChanged,
    Deferred,
}

/// One compact wire shape for both history rows and the actual detail endpoint.
/// The history query supplies only scalar/coarse eligibility; POST revalidates
/// the complete immutable recipe. It never loads the original chart to decide.
pub fn compact_summary(row: &Value) -> Value {
    let completed = row["status"] == "completed";
    let reason = row["eligibility_reason"].as_str();
    let last_setup_at = if completed && row["totals"]["total"] != 0 {
        row["last_setup_close_unix"]
            .as_i64()
            .and_then(|at| DateTime::<Utc>::from_timestamp(at, 0))
    } else {
        None
    };
    json!({"studyId":row["study_id"],"eligibility":{"eligible":reason.is_none(),"reason":reason},
        "jobId":row["job_id"],"requestOrigin":row["request_origin"],"revision":row["revision"].as_i64().unwrap_or(0),"status":row["status"].as_str().unwrap_or("not_calculated"),"stage":row["stage"],
        "observedThroughUnix":row["observed_through_unix"],"counts":if completed{row["totals"].clone()}else{Value::Null},
        "meanR":if completed{row["totals"]["meanR"].clone()}else{Value::Null},"lastSetupAt":last_setup_at,"error":row["error"]})
}

pub async fn get(pool: &PgPool, owner: &str, study: i64) -> Result<Option<Value>, DbError> {
    Ok(sqlx::query_scalar(&head_sql())
        .bind(owner)
        .bind(study)
        .fetch_optional(pool)
        .await?)
}

async fn cleanup(
    tx: &mut Transaction<'_, Postgres>,
    owner: &str,
    study: i64,
    revision: i64,
) -> Result<(), DbError> {
    // Only the explicit current owned ID, with all provenance guards, authorizes deletion.
    let owned:Option<(Option<i32>,String)>=sqlx::query_as("select batch_id,job_id from study_actualizations where owner_id=$1 and study_id=$2 and revision=$3 for update")
        .bind(owner).bind(study).bind(revision).fetch_optional(&mut **tx).await?;
    let Some((batch, job_id)) = owned else {
        return Ok(());
    };
    sqlx::query("update study_actualizations set batch_id=null where owner_id=$1 and study_id=$2 and revision=$3")
        .bind(owner).bind(study).bind(revision).execute(&mut **tx).await?;
    sqlx::query(
        "delete from study_actual_setups where owner_id=$1 and study_id=$2 and revision=$3",
    )
    .bind(owner)
    .bind(study)
    .bind(revision)
    .execute(&mut **tx)
    .await?;
    if let Some(batch) = batch {
        let owned: bool=sqlx::query_scalar("select exists(select 1 from replay_batches where id=$1 and owner_id=$2 and actual_study_id=$3 and actual_revision=$4 and origin='study_actual' and continuous_generation_id is null and actual_job_id=$5) and not exists(select 1 from replay_runs where batch_id=$1 and (owner_id,origin,actual_study_id,actual_revision,actual_job_id) is distinct from ($2::text,'study_actual'::text,$3::bigint,$4::bigint,$5::text))")
            .bind(batch).bind(owner).bind(study).bind(revision).bind(job_id).fetch_one(&mut **tx).await?;
        if !owned {
            return Err(DbError::Json("study_actual_owned_pair_mismatch".into()));
        }
        sqlx::query("delete from replay_quality_intervals where run_id in (select id from replay_runs where batch_id=$1 and owner_id=$2 and origin='study_actual' and actual_study_id=$3 and actual_revision=$4)")
            .bind(batch).bind(owner).bind(study).bind(revision).execute(&mut **tx).await?;
        sqlx::query("delete from replay_runs where batch_id=$1 and owner_id=$2 and origin='study_actual' and actual_study_id=$3 and actual_revision=$4")
            .bind(batch).bind(owner).bind(study).bind(revision).execute(&mut **tx).await?;
        sqlx::query("delete from replay_batches where id=$1 and owner_id=$2 and origin='study_actual' and actual_study_id=$3 and actual_revision=$4")
            .bind(batch).bind(owner).bind(study).bind(revision).execute(&mut **tx).await?;
    }
    Ok(())
}

/// The caller already locked the source study. Preserve manual history deletion:
/// active derived work is referenced; terminal owned diagnostics are removed in
/// the SAME transaction as the original manual study, never via a broad cascade.
pub(crate) async fn cleanup_for_study_delete(
    tx: &mut Transaction<'_, Postgres>,
    owner: &str,
    study: i64,
) -> Result<bool, DbError> {
    // Delete owns the source row. An active writer owns the head before its FK
    // insert needs KEY SHARE on that source. Do not wait for its head here.
    let active:bool=sqlx::query_scalar("select exists(select 1 from study_actualizations where owner_id=$1 and study_id=$2 and status in ('queued','running'))")
        .bind(owner).bind(study).fetch_one(&mut **tx).await?;
    if active {
        return Ok(false);
    }
    let head:Option<(i64,String)>=sqlx::query_as("select revision,status from study_actualizations where owner_id=$1 and study_id=$2 for update")
        .bind(owner).bind(study).fetch_optional(&mut **tx).await?;
    let Some((revision, status)) = head else {
        return Ok(true);
    };
    if matches!(status.as_str(), "queued" | "running") {
        return Ok(false);
    }
    cleanup(tx, owner, study, revision).await?;
    sqlx::query("delete from study_actualizations where owner_id=$1 and study_id=$2 and revision=$3 and status in ('completed','failed')")
        .bind(owner).bind(study).bind(revision).execute(&mut **tx).await?;
    Ok(true)
}

pub struct Request<'a> {
    pub owner: &'a str,
    pub study: i64,
    pub job: &'a str,
    pub start: DateTime<Utc>,
    pub cutoff: i64,
    pub source: &'a Value,
    pub identity: &'a str,
    pub work: i64,
    pub limits: &'a ReplayAdmissionLimits,
}

pub async fn admit(pool: &PgPool, request: Request<'_>) -> Result<Admission, DbError> {
    admit_inner(pool, request, None).await
}

async fn admit_inner(
    pool: &PgPool,
    request: Request<'_>,
    automatic: Option<&continuous::Ticket>,
) -> Result<Admission, DbError> {
    let Request {
        owner,
        study,
        job,
        start,
        cutoff,
        source,
        identity,
        work,
        limits,
    } = request;
    let mut tx = pool.begin().await?;
    // Same lock order as continuous history purge: owner -> queue -> rows.
    crate::continuous::owner_lock(&mut tx, owner).await?;
    sqlx::query("select pg_advisory_xact_lock($1)")
        .bind(QUEUE_LOCK)
        .execute(&mut *tx)
        .await?;
    if let Some(ticket) = automatic {
        if ticket.settings.owner_id != owner
            || source["algorithmId"] != ticket.settings.algorithm_id
        {
            return Ok(Admission::SourceChanged);
        }
        let allowed:bool=sqlx::query_scalar("select exists(select 1 from study_actual_continuous c join optimization_studies s on s.owner_id=c.owner_id and s.algorithm_id=c.algorithm_id left join continuous_generations g on g.id=s.continuous_generation_id and g.owner_id=s.owner_id where c.owner_id=$1 and c.algorithm_id=$2 and c.config_revision=$3 and c.enabled and s.id=$4 and s.status='completed' and coalesce(s.result#>'{best,partial}','false')<>'true'::jsonb and coalesce(extract(epoch from g.execution_started_at),extract(epoch from s.execution_started_at),study_actual_execution_start(s.events))=$5::text::numeric and $5::text::numeric>=extract(epoch from c.from_boundary)) and not exists(select 1 from continuous_maintenance where owner_id=$1 and admission_paused) and not exists(select 1 from study_actualizations where owner_id=$1 and status in ('queued','running'))")
            .bind(owner).bind(&ticket.settings.algorithm_id).bind(ticket.settings.revision).bind(study).bind(continuous::start_key(start)).fetch_one(&mut *tx).await?;
        if !allowed {
            return Ok(Admission::Deferred);
        }
        let last:Option<i64>=sqlx::query_scalar("select last_automatic_cutoff from study_actualizations where owner_id=$1 and study_id=$2").bind(owner).bind(study).fetch_optional(&mut *tx).await?.flatten();
        if last.is_some_and(|c| c >= cutoff) {
            sqlx::query("update study_actual_continuous set next_due_at=greatest(clock_timestamp()+interval '1 second',to_timestamp($4+60)) where owner_id=$1 and algorithm_id=$2 and config_revision=$3")
                .bind(owner).bind(&ticket.settings.algorithm_id).bind(ticket.settings.revision).bind(last.unwrap()).execute(&mut *tx).await?;
            tx.commit().await?;
            return Ok(Admission::Deferred);
        }
    }
    let current: Option<String>=sqlx::query_scalar("select md5(spec::text||coalesce(result::text,'null')) from optimization_studies where id=$1 and owner_id=$2 and status in ('completed','cancelled','failed') for share")
        .bind(study).bind(owner).fetch_optional(&mut *tx).await?;
    if current.as_deref() != Some(identity) {
        return Ok(Admission::SourceChanged);
    }
    let previous=sqlx::query("select revision,status from study_actualizations where owner_id=$1 and study_id=$2 for update")
        .bind(owner).bind(study).fetch_optional(&mut *tx).await?;
    if previous
        .as_ref()
        .is_some_and(|r| matches!(r.get::<String, _>("status").as_str(), "queued" | "running"))
    {
        let value = sqlx::query_scalar(&head_sql())
            .bind(owner)
            .bind(study)
            .fetch_one(&mut *tx)
            .await?;
        return Ok(Admission::Busy(value));
    }
    let reserved = active_reservation_bytes(&mut tx, limits, None)
        .await
        .map_err(|e| DbError::Json(e.to_string()))?;
    let used: i64 = sqlx::query_scalar("select pg_database_size(current_database())::bigint")
        .fetch_one(&mut *tx)
        .await?;
    let (pending,actual_work):(i64,i64)=sqlx::query_as("select count(*),coalesce(sum(work_minutes),0)::bigint from study_actualizations where status in ('queued','running')")
        .fetch_one(&mut *tx).await?;
    let manual_pending:i64=sqlx::query_scalar("select count(*) from replay_runs where origin='manual' and status in ('queued','running','cancel_requested')").fetch_one(&mut *tx).await?;
    if pending + manual_pending >= limits.max_pending_runs
        || work.saturating_add(actual_work) > limits.max_work_minutes
        || used.saturating_add(reserved).saturating_add(RESERVED_BYTES) > limits.max_database_bytes
    {
        return Ok(Admission::Resource);
    }
    let revision = previous
        .as_ref()
        .map(|r| r.get::<i64, _>("revision") + 1)
        .unwrap_or(1);
    if revision > 1 {
        cleanup(&mut tx, owner, study, revision - 1).await?;
    }
    sqlx::query("insert into study_actualizations(owner_id,study_id,job_id,revision,execution_started_at,observed_through_unix,source,source_identity,status,stage,work_minutes,reserved_bytes,request_origin,automatic_config_revision,automatic_cursor_generation,last_automatic_cutoff) values($1,$2,$3,$4,$5,$6,$7,$8,'queued','queued',$9,$10,$11,$12,$13,case when $11='automatic' then $6 end) on conflict(owner_id,study_id) do update set job_id=excluded.job_id,revision=excluded.revision,token=study_actualizations.token+1,execution_started_at=excluded.execution_started_at,observed_through_unix=excluded.observed_through_unix,source=excluded.source,source_identity=excluded.source_identity,status='queued',stage='queued',progress='{}',report=null,totals=null,error=null,batch_id=null,work_minutes=excluded.work_minutes,reserved_bytes=excluded.reserved_bytes,requested_at=clock_timestamp(),started_at=null,completed_at=null,request_origin=excluded.request_origin,automatic_config_revision=excluded.automatic_config_revision,automatic_cursor_generation=excluded.automatic_cursor_generation,last_automatic_cutoff=coalesce(excluded.last_automatic_cutoff,study_actualizations.last_automatic_cutoff),queue_sequence=nextval(pg_get_serial_sequence('study_actualizations','queue_sequence'))")
        .bind(owner).bind(study).bind(job).bind(revision).bind(start).bind(cutoff).bind(source).bind(identity).bind(work).bind(RESERVED_BYTES).bind(if automatic.is_some(){"automatic"}else{"manual"}).bind(automatic.map(|t|t.settings.revision)).bind(automatic.map(|t|t.settings.cursor_generation)).execute(&mut *tx).await?;
    if let Some(ticket) = automatic {
        sqlx::query("update study_actual_continuous set last_admitted_at=clock_timestamp() where owner_id=$1 and algorithm_id=$2 and config_revision=$3").bind(owner).bind(&ticket.settings.algorithm_id).bind(ticket.settings.revision).execute(&mut *tx).await?;
    }
    let value = sqlx::query_scalar(&head_sql())
        .bind(owner)
        .bind(study)
        .fetch_one(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Admission::Created(value))
}

#[derive(Debug, Clone)]
pub struct Job {
    pub owner: String,
    pub study: i64,
    pub revision: i64,
    pub token: i64,
    pub source: Value,
    pub identity: String,
    pub cutoff: i64,
    pub job_id: String,
}

pub async fn claim(pool: &PgPool, capacity: i64) -> Result<Option<Job>, DbError> {
    let mut connection = pool.acquire().await?;
    claim_connection(&mut connection, capacity, false).await
}

/// Production claims execute on the session that actually holds the runtime
/// advisory lock. A terminated/lost session cannot claim through another pool.
pub async fn claim_owned(
    connection: &mut PgConnection,
    capacity: i64,
) -> Result<Option<Job>, DbError> {
    claim_connection(connection, capacity, true).await
}

pub async fn owns_runtime(connection: &mut PgConnection) -> Result<bool, DbError> {
    Ok(sqlx::query_scalar("select exists(select 1 from pg_locks where locktype='advisory' and pid=pg_backend_pid() and classid=0 and objid=$1::bigint::oid and objsubid=1 and granted)")
        .bind(RUNTIME_LOCK).fetch_one(connection).await?)
}

async fn claim_connection(
    connection: &mut PgConnection,
    capacity: i64,
    require_owner: bool,
) -> Result<Option<Job>, DbError> {
    if require_owner && !owns_runtime(connection).await? {
        return Err(DbError::Json("study_actual_runtime_ownership_lost".into()));
    }
    let owner:Option<String>=sqlx::query_scalar("select a.owner_id from study_actualizations a where a.status='queued' and not exists(select 1 from study_actualizations r where r.owner_id=a.owner_id and r.status='running') and (a.request_origin='manual' or not exists(select 1 from continuous_maintenance m where m.owner_id=a.owner_id and m.admission_paused)) order by (a.request_origin='manual') desc,a.queue_sequence,a.study_id limit 1").fetch_optional(&mut *connection).await?;
    let Some(owner) = owner else {
        return Ok(None);
    };
    let mut tx = connection.begin().await?;
    crate::continuous::owner_lock(&mut tx, &owner).await?;
    sqlx::query("select pg_advisory_xact_lock($1)")
        .bind(QUEUE_LOCK)
        .execute(&mut *tx)
        .await?;
    let active:i64=sqlx::query_scalar("select (select count(*) from replay_runs where origin='manual' and status in ('running','cancel_requested'))+(select count(*) from study_actualizations where status='running')").fetch_one(&mut *tx).await?;
    if active >= capacity {
        return Ok(None);
    }
    // The shared queue lock serializes coordinators, not just tasks in one
    // process. No owner can claim while its previous job is still running.
    let candidate=sqlx::query("select a.* from study_actualizations a where a.owner_id=$1 and a.status='queued' and not exists(select 1 from study_actualizations r where r.owner_id=a.owner_id and r.status='running') and (a.request_origin='manual' or not exists(select 1 from continuous_maintenance m where m.owner_id=a.owner_id and m.admission_paused)) order by (a.request_origin='manual') desc,a.queue_sequence,a.study_id limit 1 for update skip locked").bind(&owner).fetch_optional(&mut *tx).await?;
    let Some(candidate) = candidate else {
        return Ok(None);
    };
    if candidate.get::<String, _>("request_origin") == "automatic" {
        let paused:bool=sqlx::query_scalar("select exists(select 1 from continuous_maintenance where owner_id=$1 and admission_paused)").bind(&owner).fetch_one(&mut *tx).await?;
        if paused {
            return Ok(None);
        }
        let allowed:bool=sqlx::query_scalar("select exists(select 1 from study_actualizations a join study_actual_continuous c on c.owner_id=a.owner_id and c.algorithm_id=a.source->>'algorithmId' join optimization_studies s on s.id=a.study_id and s.owner_id=a.owner_id left join continuous_generations g on g.id=s.continuous_generation_id and g.owner_id=s.owner_id where a.owner_id=$1 and a.study_id=$2 and c.enabled and c.config_revision=a.automatic_config_revision and s.status='completed' and a.source_identity=md5(s.spec::text||coalesce(s.result::text,'null')) and coalesce(extract(epoch from g.execution_started_at),extract(epoch from s.execution_started_at),study_actual_execution_start(s.events))=coalesce(study_actual_execution_start(jsonb_build_array(jsonb_build_object('kind','started','at',a.source->>'executionStartedAt'))),extract(epoch from a.execution_started_at)) and a.execution_started_at>=c.from_boundary)")
            .bind(&owner).bind(candidate.get::<i64,_>("study_id")).fetch_one(&mut *tx).await?;
        if !allowed {
            sqlx::query("update study_actualizations set status='failed',stage='failed',token=token+1,reserved_bytes=0,completed_at=clock_timestamp(),error='{\"code\":\"study_actual_automatic_admission_changed\"}' where owner_id=$1 and study_id=$2 and status='queued'")
                .bind(&owner).bind(candidate.get::<i64,_>("study_id")).execute(&mut *tx).await?;
            let stale = Job {
                owner: owner.clone(),
                study: candidate.try_get("study_id")?,
                revision: candidate.try_get("revision")?,
                token: candidate.try_get("token")?,
                source: Value::Null,
                identity: candidate.try_get("source_identity")?,
                cutoff: candidate.try_get("observed_through_unix")?,
                job_id: candidate.try_get("job_id")?,
            };
            continuous::terminal(&mut tx, &stale).await?;
            tx.commit().await?;
            return Ok(None);
        }
    }
    let row=sqlx::query("update study_actualizations set status='running',stage='acquiring',token=token+1,started_at=clock_timestamp() where owner_id=$1 and study_id=$2 and status='queued' returning *")
        .bind(owner).bind(candidate.get::<i64,_>("study_id")).fetch_optional(&mut *tx).await?;
    let job = row
        .map(|r| {
            Ok::<_, DbError>(Job {
                owner: r.try_get("owner_id")?,
                study: r.try_get("study_id")?,
                revision: r.try_get("revision")?,
                token: r.try_get("token")?,
                source: r.try_get("source")?,
                identity: r.try_get("source_identity")?,
                cutoff: r.try_get("observed_through_unix")?,
                job_id: r.try_get("job_id")?,
            })
        })
        .transpose()?;
    tx.commit().await?;
    Ok(job)
}

async fn fence(tx: &mut Transaction<'_, Postgres>, job: &Job) -> Result<bool, DbError> {
    Ok(sqlx::query_scalar("select exists(select 1 from study_actualizations a join optimization_studies s on s.id=a.study_id and s.owner_id=a.owner_id where a.owner_id=$1 and a.study_id=$2 and a.revision=$3 and a.token=$4 and a.status='running' and a.source_identity=$5 and a.source_identity=md5(s.spec::text||coalesce(s.result::text,'null')))")
        .bind(&job.owner).bind(job.study).bind(job.revision).bind(job.token).bind(&job.identity).fetch_one(&mut **tx).await?)
}

pub async fn progress(
    pool: &PgPool,
    job: &Job,
    stage: &str,
    progress: &Value,
) -> Result<bool, DbError> {
    Ok(sqlx::query("update study_actualizations set stage=$5,progress=$6 where owner_id=$1 and study_id=$2 and revision=$3 and token=$4 and status='running'")
        .bind(&job.owner).bind(job.study).bind(job.revision).bind(job.token).bind(stage).bind(progress).execute(pool).await?.rows_affected()==1)
}

pub async fn create_batch(pool: &PgPool, job: &Job) -> Result<Option<i32>, DbError> {
    let mut tx = pool.begin().await?;
    sqlx::query("select 1 from study_actualizations where owner_id=$1 and study_id=$2 for update")
        .bind(&job.owner)
        .bind(job.study)
        .execute(&mut *tx)
        .await?;
    if !fence(&mut tx, job).await? {
        return Ok(None);
    }
    let existing: Option<i32> = sqlx::query_scalar(
        "select batch_id from study_actualizations where owner_id=$1 and study_id=$2",
    )
    .bind(&job.owner)
    .bind(job.study)
    .fetch_one(&mut *tx)
    .await?;
    if let Some(id) = existing {
        let owned:bool=sqlx::query_scalar("select exists(select 1 from replay_batches where id=$1 and owner_id=$2 and actual_study_id=$3 and actual_revision=$4 and actual_job_id=$5 and origin='study_actual')")
            .bind(id).bind(&job.owner).bind(job.study).bind(job.revision).bind(&job.job_id).fetch_one(&mut *tx).await?;
        if !owned {
            return Err(DbError::Json("study_actual_owned_pair_mismatch".into()));
        }
        tx.commit().await?;
        return Ok(Some(id));
    }
    let id:i32=sqlx::query_scalar("insert into replay_batches(exchange_code,category,algorithm_id,algorithm_version,params,from_ts,to_ts,instrument_count,name,origin,owner_id,actual_study_id,actual_revision,actual_job_id) values('mixed','mixed',$1,$2,$3,to_timestamp($4),to_timestamp($5),$6,$7,'study_actual',$8,$9,$10,$11) returning id")
        .bind(job.source["algorithmId"].as_str()).bind(job.source["algorithmVersion"].as_str()).bind(&job.source["params"])
        .bind(job.source["startUnix"].as_f64().unwrap()).bind(job.cutoff as f64).bind(job.source["instruments"].as_array().unwrap().len() as i32)
        .bind(format!("После исследования · {}",job.study)).bind(&job.owner).bind(job.study).bind(job.revision).bind(&job.job_id).fetch_one(&mut *tx).await?;
    sqlx::query("update study_actualizations set batch_id=$3 where owner_id=$1 and study_id=$2")
        .bind(&job.owner)
        .bind(job.study)
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Some(id))
}

pub struct InstrumentOutput<'a> {
    pub exchange: &'a str,
    pub category: &'a str,
    pub symbol: &'a str,
    pub emitted: &'a Value,
    pub rows: &'a [Value],
    pub quality: &'a Value,
}

pub async fn append(
    pool: &PgPool,
    job: &Job,
    batch: i32,
    out: InstrumentOutput<'_>,
) -> Result<bool, DbError> {
    let mut tx = pool.begin().await?;
    sqlx::query("select 1 from study_actualizations where owner_id=$1 and study_id=$2 for update")
        .bind(&job.owner)
        .bind(job.study)
        .execute(&mut *tx)
        .await?;
    if !fence(&mut tx, job).await? {
        return Ok(false);
    }
    let owns_batch:bool=sqlx::query_scalar("select exists(select 1 from study_actualizations a join replay_batches b on b.id=a.batch_id where a.owner_id=$1 and a.study_id=$2 and a.batch_id=$3 and b.owner_id=a.owner_id and b.actual_study_id=a.study_id and b.actual_revision=a.revision and b.actual_job_id=a.job_id and b.origin='study_actual')")
        .bind(&job.owner).bind(job.study).bind(batch).fetch_one(&mut *tx).await?;
    if !owns_batch {
        return Ok(false);
    }
    let (n,bytes):(i64,i64)=sqlx::query_as("select count(*),coalesce(sum(octet_length(row_data::text)),0)::bigint from study_actual_setups where owner_id=$1 and study_id=$2")
        .bind(&job.owner).bind(job.study).fetch_one(&mut *tx).await?;
    let added = serde_json::to_vec(out.emitted)
        .map_err(crate::json_error)?
        .len()
        + serde_json::to_vec(out.rows)
            .map_err(crate::json_error)?
            .len();
    let replay_bytes:i64=sqlx::query_scalar("select coalesce(sum(octet_length(setups::text)),0)::bigint from replay_runs where batch_id=$1").bind(batch).fetch_one(&mut *tx).await?;
    if n + out.rows.len() as i64 > MAX_ROWS
        || bytes + replay_bytes + added as i64 > MAX_OUTPUT_BYTES
    {
        return Err(DbError::Json("study_actual_output_budget_exceeded".into()));
    }
    let run:i32=sqlx::query_scalar("insert into replay_runs(exchange_code,category,symbol,from_ts,to_ts,algorithm_id,algorithm_version,params,status,cache_hit,setups,batch_id,name,origin,owner_id,actual_study_id,actual_revision,actual_job_id,quality) values($1,$2,$3,to_timestamp($4),to_timestamp($5),$6,$7,$8,'done',false,$9,$10,$11,'study_actual',$12,$13,$14,$15,$16) returning id")
        .bind(out.exchange).bind(out.category).bind(out.symbol).bind(job.source["startUnix"].as_f64().unwrap()).bind(job.cutoff as f64)
        .bind(job.source["algorithmId"].as_str()).bind(job.source["algorithmVersion"].as_str()).bind(&job.source["params"]).bind(out.emitted).bind(batch).bind(format!("После исследования · {}",job.study)).bind(&job.owner).bind(job.study).bind(job.revision).bind(&job.job_id).bind(out.quality).fetch_one(&mut *tx).await?;
    for row in out.rows {
        sqlx::query("insert into study_actual_setups(owner_id,study_id,revision,run_id,signal_close_unix,instrument_key,row_data) values($1,$2,$3,$4,$5,$6,$7)")
            .bind(&job.owner).bind(job.study).bind(job.revision).bind(run).bind(row["setupTimeUnix"].as_i64().unwrap()).bind(format!("{}:{}:{}",out.exchange,out.category,out.symbol)).bind(row).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(true)
}

pub async fn finish(
    pool: &PgPool,
    job: &Job,
    report: &Value,
    totals: &Value,
) -> Result<bool, DbError> {
    let mut tx = pool.begin().await?;
    crate::continuous::owner_lock(&mut tx, &job.owner).await?;
    sqlx::query("select 1 from study_actualizations where owner_id=$1 and study_id=$2 for update")
        .bind(&job.owner)
        .bind(job.study)
        .execute(&mut *tx)
        .await?;
    if !fence(&mut tx, job).await? {
        return Ok(false);
    }
    sqlx::query("update study_actualizations set status='completed',stage='completed',report=$5,totals=$6,reserved_bytes=0,completed_at=clock_timestamp() where owner_id=$1 and study_id=$2 and revision=$3 and token=$4")
        .bind(&job.owner).bind(job.study).bind(job.revision).bind(job.token).bind(report).bind(totals).execute(&mut *tx).await?;
    continuous::terminal(&mut tx, job).await?;
    tx.commit().await?;
    Ok(true)
}

pub async fn fail(pool: &PgPool, job: &Job, code: &str) -> Result<(), DbError> {
    let mut tx = pool.begin().await?;
    fail_transaction(&mut tx, job, code).await?;
    tx.commit().await?;
    Ok(())
}

async fn fail_transaction(
    tx: &mut Transaction<'_, Postgres>,
    job: &Job,
    code: &str,
) -> Result<(), DbError> {
    crate::continuous::owner_lock(tx, &job.owner).await?;
    sqlx::query("select 1 from study_actualizations where owner_id=$1 and study_id=$2 for update")
        .bind(&job.owner)
        .bind(job.study)
        .execute(&mut **tx)
        .await?;
    let owned:bool=sqlx::query_scalar("select exists(select 1 from study_actualizations where owner_id=$1 and study_id=$2 and revision=$3 and token=$4 and status='running')")
        .bind(&job.owner).bind(job.study).bind(job.revision).bind(job.token).fetch_one(&mut **tx).await?;
    if !owned {
        return Ok(());
    }
    cleanup(tx, &job.owner, job.study, job.revision).await?;
    sqlx::query("update study_actualizations set status='failed',stage='failed',token=token+1,error=$5,reserved_bytes=0,completed_at=clock_timestamp() where owner_id=$1 and study_id=$2 and revision=$3 and token=$4 and status='running'")
        .bind(&job.owner).bind(job.study).bind(job.revision).bind(job.token).bind(json!({"code":code,"stage":"compute","message":code})).execute(&mut **tx).await?;
    continuous::terminal(tx, job).await?;
    Ok(())
}

pub async fn recover_owned(connection: &mut PgConnection) -> Result<(), DbError> {
    loop {
        if !owns_runtime(connection).await? {
            return Err(DbError::Json("study_actual_runtime_ownership_lost".into()));
        }
        let mut tx = connection.begin().await?;
        let row=sqlx::query("select * from study_actualizations where status='running' order by requested_at limit 1")
            .fetch_optional(&mut *tx).await?;
        let Some(r) = row else {
            tx.commit().await?;
            return Ok(());
        };
        let job = Job {
            owner: r.try_get("owner_id")?,
            study: r.try_get("study_id")?,
            revision: r.try_get("revision")?,
            token: r.try_get("token")?,
            source: Value::Null,
            identity: r.try_get("source_identity")?,
            cutoff: r.try_get("observed_through_unix")?,
            job_id: r.try_get("job_id")?,
        };
        fail_transaction(&mut tx, &job, "study_actual_interrupted_by_restart").await?;
        tx.commit().await?;
    }
}

pub async fn recover(pool: &PgPool) -> Result<(), DbError> {
    // Dispatcher holds its singleton advisory ownership before calling this;
    // never interrupts another server's active work. Queued jobs are untouched.
    loop {
        let row=sqlx::query("select * from study_actualizations where status='running' order by requested_at limit 1").fetch_optional(pool).await?;
        let Some(r) = row else { break };
        let job = Job {
            owner: r.try_get("owner_id")?,
            study: r.try_get("study_id")?,
            revision: r.try_get("revision")?,
            token: r.try_get("token")?,
            source: Value::Null,
            identity: r.try_get("source_identity")?,
            cutoff: r.try_get("observed_through_unix")?,
            job_id: r.try_get("job_id")?,
        };
        fail(pool, &job, "study_actual_interrupted_by_restart").await?;
    }
    Ok(())
}

pub async fn page(
    pool: &PgPool,
    owner: &str,
    study: i64,
    revision: i64,
    offset: i64,
    limit: i64,
) -> Result<Option<Vec<Value>>, DbError> {
    let mut tx = pool.begin().await?;
    sqlx::query("set transaction isolation level repeatable read, read only")
        .execute(&mut *tx)
        .await?;
    let valid:bool=sqlx::query_scalar("select exists(select 1 from study_actualizations where owner_id=$1 and study_id=$2 and revision=$3 and status='completed')")
        .bind(owner).bind(study).bind(revision).fetch_one(&mut *tx).await?;
    if !valid {
        return Ok(None);
    }
    let rows=sqlx::query_scalar("select row_data||jsonb_build_object('id',id) from study_actual_setups where owner_id=$1 and study_id=$2 and revision=$3 order by signal_close_unix desc,instrument_key,id limit $4 offset $5")
        .bind(owner).bind(study).bind(revision).bind(limit.clamp(1,100)).bind(offset.max(0)).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(Some(rows))
}
