//! Durable automatic admission policy for the existing actual executor.
//! Only compact candidate pages live in RAM; settings own scan/cursor lifetime.
use super::*;
use chrono::NaiveDate;
use serde::Serialize;

pub const CANDIDATE_PAGE: i64 = 16;
pub const EXECUTION_START_SQL: &str = "coalesce(extract(epoch from g.execution_started_at),extract(epoch from s.execution_started_at),study_actual_execution_start(s.events))";
pub(super) fn start_key(start: DateTime<Utc>) -> String {
    format!(
        "{}.{:09}",
        start.timestamp(),
        start.timestamp_subsec_nanos()
    )
}
fn parse_start(key: String) -> Result<DateTime<Utc>, DbError> {
    let (seconds, fraction) = key.split_once('.').unwrap_or((&key, ""));
    let seconds = seconds
        .parse::<i64>()
        .map_err(|e| DbError::Json(format!("study_actual_start_key: {e}")))?;
    let fraction = format!("{fraction:0<9}");
    let nanos = fraction[..9]
        .parse::<u32>()
        .map_err(|e| DbError::Json(format!("study_actual_start_key: {e}")))?;
    DateTime::from_timestamp(seconds, nanos)
        .ok_or_else(|| DbError::Json("study_actual_execution_start_invalid".into()))
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    pub owner_id: String,
    pub algorithm_id: String,
    pub enabled: bool,
    pub from_date: NaiveDate,
    pub from_boundary: DateTime<Utc>,
    pub time_zone: String,
    pub revision: i64,
    pub cursor_generation: i64,
    pub cursor_study_id: Option<i64>,
    pub cursor_execution_started_at: Option<DateTime<Utc>>,
    pub reset_pending: bool,
    pub next_due_at: DateTime<Utc>,
    pub last_skip_reason: Option<String>,
}
fn decode(r: &sqlx::postgres::PgRow) -> Result<Settings, DbError> {
    Ok(Settings {
        owner_id: r.try_get("owner_id")?,
        algorithm_id: r.try_get("algorithm_id")?,
        enabled: r.try_get("enabled")?,
        from_date: r.try_get("from_date")?,
        from_boundary: r.try_get("from_boundary")?,
        time_zone: r.try_get("time_zone")?,
        revision: r.try_get("config_revision")?,
        cursor_generation: r.try_get("cursor_generation")?,
        cursor_study_id: r.try_get("cursor_study_id")?,
        cursor_execution_started_at: r.try_get("cursor_execution_started_at")?,
        reset_pending: r.try_get("reset_pending")?,
        next_due_at: r.try_get("next_due_at")?,
        last_skip_reason: r.try_get("last_skip_reason")?,
    })
}
pub fn valid_algorithm(id: &str) -> bool {
    matches!(id, "price_volume_spike" | "price_drop_volume_spike")
}

/// Read-only effective defaults shared by GET and durable initialization.
/// Global UI settings belong to the legacy admin only; never borrow another
/// owner's timezone. Auto/invalid => UTC. Capture the clock once for this view.
async fn effective_default(
    connection: &mut PgConnection,
    owner: &str,
    algorithm: &str,
) -> Result<Settings, DbError> {
    let row = sqlx::query("with instant as (select clock_timestamp() as now_at), zone as (select coalesce((select name from pg_timezone_names where name=(select value#>>'{interface,timezone}' from settings where key='ui' and $1='admin') and length(name)<=128),'UTC') name), d as (select name,now_at,timezone(name,now_at)::date as from_date from zone cross join instant) select name,from_date,to_timestamp(study_local_day_start(name,from_date)) from_boundary,now_at next_due_at from d")
        .bind(owner).fetch_one(connection).await?;
    Ok(Settings {
        owner_id: owner.into(),
        algorithm_id: algorithm.into(),
        enabled: true,
        from_date: row.try_get("from_date")?,
        from_boundary: row.try_get("from_boundary")?,
        time_zone: row.try_get("name")?,
        revision: 1,
        cursor_generation: 1,
        cursor_study_id: None,
        cursor_execution_started_at: None,
        reset_pending: false,
        next_due_at: row.try_get("next_due_at")?,
        last_skip_reason: None,
    })
}

/// Durable initialization belongs to explicit PUT and headless bootstrap only.
pub async fn ensure(pool: &PgPool, owner: &str, algorithm: &str) -> Result<Settings, DbError> {
    if !valid_algorithm(algorithm) {
        return Err(DbError::Json("invalid_algorithm_id".into()));
    }
    let mut tx = pool.begin().await?;
    crate::continuous::owner_lock(&mut tx, owner).await?;
    if let Some(row) =
        sqlx::query("select * from study_actual_continuous where owner_id=$1 and algorithm_id=$2")
            .bind(owner)
            .bind(algorithm)
            .fetch_optional(&mut *tx)
            .await?
    {
        let settings = decode(&row)?;
        tx.commit().await?;
        return Ok(settings);
    }
    let defaults = effective_default(&mut tx, owner, algorithm).await?;
    sqlx::query("insert into study_actual_continuous(owner_id,algorithm_id,from_date,from_boundary,time_zone,next_due_at) values($1,$2,$3,$4,$5,$6) on conflict do nothing")
        .bind(owner).bind(algorithm).bind(defaults.from_date).bind(defaults.from_boundary)
        .bind(defaults.time_zone).bind(defaults.next_due_at).execute(&mut *tx).await?;
    let r =
        sqlx::query("select * from study_actual_continuous where owner_id=$1 and algorithm_id=$2")
            .bind(owner)
            .bind(algorithm)
            .fetch_one(&mut *tx)
            .await?;
    let value = decode(&r)?;
    tx.commit().await?;
    Ok(value)
}

/// Missing defaults are initialized in bounded pages, independently of visits.
pub async fn bootstrap(pool: &PgPool) -> Result<(), DbError> {
    let pairs:Vec<(String,String)>=sqlx::query_as("select owner_id,algorithm_id from (select 'admin'::text owner_id,unnest(array['price_volume_spike','price_drop_volume_spike']) algorithm_id union select owner_id,algorithm_id from optimization_studies where algorithm_id in ('price_volume_spike','price_drop_volume_spike')) p where not exists(select 1 from study_actual_continuous c where c.owner_id=p.owner_id and c.algorithm_id=p.algorithm_id) order by owner_id,algorithm_id limit 32").fetch_all(pool).await?;
    for (owner, algorithm) in pairs {
        ensure(pool, &owner, &algorithm).await?;
    }
    Ok(())
}

pub enum Configure {
    Saved(Settings),
    Conflict(Settings),
    Invalid(&'static str),
}
pub async fn configure(
    pool: &PgPool,
    owner: &str,
    algorithm: &str,
    revision: i64,
    enabled: bool,
    date: &str,
    zone: &str,
) -> Result<Configure, DbError> {
    let Ok(day) = NaiveDate::parse_from_str(date, "%Y-%m-%d") else {
        return Ok(Configure::Invalid("invalid_from_date"));
    };
    if date.len() != 10 || day.to_string() != date || !("1970-01-01"..="9999-12-31").contains(&date)
    {
        return Ok(Configure::Invalid("invalid_from_date"));
    }
    if zone.is_empty() || zone.len() > 128 || !zone.is_ascii() {
        return Ok(Configure::Invalid("invalid_time_zone"));
    }
    ensure(pool, owner, algorithm).await?;
    let mut tx = pool.begin().await?;
    crate::continuous::owner_lock(&mut tx, owner).await?;
    let valid: bool =
        sqlx::query_scalar("select exists(select 1 from pg_timezone_names where name=$1)")
            .bind(zone)
            .fetch_one(&mut *tx)
            .await?;
    if !valid {
        return Ok(Configure::Invalid("invalid_time_zone"));
    }
    let row = sqlx::query(
        "select * from study_actual_continuous where owner_id=$1 and algorithm_id=$2 for update",
    )
    .bind(owner)
    .bind(algorithm)
    .fetch_one(&mut *tx)
    .await?;
    let old = decode(&row)?;
    if old.revision != revision {
        return Ok(Configure::Conflict(old));
    }
    // Preserve the absolute boundary if only enablement changes, even when UI
    // timezone changed. A date edit explicitly normalizes a new boundary.
    let changed_date = old.from_date != day;
    if !changed_date && old.enabled == enabled {
        // A same-date save retains the effective stored timezone/boundary.
        // No semantic edit means no new fence, scan reset, or due-time churn.
        tx.commit().await?;
        return Ok(Configure::Saved(old));
    }
    let r=sqlx::query("update study_actual_continuous set enabled=$3,from_date=$4,from_boundary=case when $6 then to_timestamp(study_local_day_start($5,$4)) else from_boundary end,time_zone=case when $6 then $5 else time_zone end,config_revision=config_revision+1,cursor_generation=cursor_generation+case when $6 then 1 else 0 end,cursor_study_id=case when $6 then null else cursor_study_id end,cursor_execution_started_at=case when $6 then null else cursor_execution_started_at end,reset_pending=reset_pending or $6,scan_study_id=null,scan_execution_started_at=null,next_due_at=clock_timestamp(),last_skip_reason=null,updated_at=clock_timestamp() where owner_id=$1 and algorithm_id=$2 returning *")
        .bind(owner).bind(algorithm).bind(enabled).bind(day).bind(zone).bind(changed_date).fetch_one(&mut *tx).await?;
    // Running remains fenced to its own revision and may publish. Stale queued
    // autos are retired by claim (without ever entering a worker).
    let saved = decode(&r)?;
    tx.commit().await?;
    Ok(Configure::Saved(saved))
}

pub async fn state(pool: &PgPool, owner: &str, algorithm: &str) -> Result<Value, DbError> {
    if !valid_algorithm(algorithm) {
        return Err(DbError::Json("invalid_algorithm_id".into()));
    }
    let mut connection = pool.acquire().await?;
    let existing =
        sqlx::query("select * from study_actual_continuous where owner_id=$1 and algorithm_id=$2")
            .bind(owner)
            .bind(algorithm)
            .fetch_optional(&mut *connection)
            .await?;
    let settings = if let Some(row) = existing {
        decode(&row)?
    } else {
        effective_default(&mut connection, owner, algorithm).await?
    };
    let queue:Value=sqlx::query_scalar("select jsonb_build_object('manualQueued',count(*) filter(where status='queued' and request_origin='manual'),'automaticQueued',count(*) filter(where status='queued' and request_origin='automatic'),'running', (select jsonb_build_object('studyId',study_id,'jobId',job_id,'requestOrigin',request_origin,'status',status,'algorithmId',source->>'algorithmId') from study_actualizations where owner_id=$1 and status='running' order by queue_sequence limit 1)) from study_actualizations where owner_id=$1 and status in ('queued','running')")
        .bind(owner).fetch_one(&mut *connection).await?;
    let mut result = serde_json::to_value(settings).map_err(crate::json_error)?;
    result["queue"] = queue;
    Ok(result)
}

#[derive(Debug, Clone)]
pub struct Candidate {
    pub study: i64,
    pub start: DateTime<Utc>,
}
#[derive(Debug, Clone)]
pub struct Ticket {
    pub settings: Settings,
    pub candidates: Vec<Candidate>,
    pub cutoff: i64,
    pub wrapped: bool,
}

/// Reserve only a small scan turn, not compute or a full cycle. Stable due order
/// and last_checked_at prevent one always-due algorithm from starving another.
pub async fn next(pool: &PgPool) -> Result<Option<Ticket>, DbError> {
    let mut tx = pool.begin().await?;
    let pair:Option<(String,String)>=sqlx::query_as("select c.owner_id,c.algorithm_id from study_actual_continuous c where c.enabled and c.next_due_at<=clock_timestamp() and not exists(select 1 from continuous_maintenance m where m.owner_id=c.owner_id and m.admission_paused) and not exists(select 1 from study_actualizations a where a.owner_id=c.owner_id and a.status in ('queued','running')) order by c.next_due_at,c.last_checked_at nulls first,c.owner_id,c.algorithm_id limit 1").fetch_optional(&mut *tx).await?;
    let Some((owner, algorithm)) = pair else {
        return Ok(None);
    };
    crate::continuous::owner_lock(&mut tx, &owner).await?;
    sqlx::query("select pg_advisory_xact_lock($1)")
        .bind(QUEUE_LOCK)
        .execute(&mut *tx)
        .await?;
    let row=sqlx::query("select c.* from study_actual_continuous c where c.owner_id=$1 and c.algorithm_id=$2 and c.enabled and c.next_due_at<=clock_timestamp() and not exists(select 1 from continuous_maintenance m where m.owner_id=c.owner_id and m.admission_paused) and not exists(select 1 from study_actualizations a where a.owner_id=c.owner_id and a.status in ('queued','running')) for update")
        .bind(&owner).bind(&algorithm).fetch_optional(&mut *tx).await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let mut settings = decode(&row)?;
    if settings.reset_pending {
        sqlx::query("update study_actual_continuous set cursor_study_id=null,cursor_execution_started_at=null,scan_study_id=null,scan_execution_started_at=null,reset_pending=false where owner_id=$1 and algorithm_id=$2")
            .bind(&settings.owner_id).bind(&settings.algorithm_id).execute(&mut *tx).await?;
        settings.cursor_study_id = None;
        settings.cursor_execution_started_at = None;
    }
    let scan_id: Option<i64> = if settings.reset_pending {
        None
    } else {
        row.try_get("scan_study_id")?
    };
    let after_id = scan_id.or(settings.cursor_study_id);
    settings.reset_pending = false;
    let prefix = format!(
        "with candidates as (select s.id,{EXECUTION_START_SQL} started from optimization_studies s left join continuous_generations g on g.id=s.continuous_generation_id and g.owner_id=s.owner_id where s.owner_id=$1 and s.algorithm_id=$2 and s.status='completed' and s.result->'best' is not null and s.result->'best'<>'null'::jsonb and coalesce(s.result#>'{{best,partial}}','false')<>'true'::jsonb), eligible as (select * from candidates where started>=extract(epoch from $3::timestamptz)) select id,started::text from eligible"
    );
    let forward = format!(
        "{prefix} where $4::bigint is null or not exists(select 1 from eligible where id=$4) or (started,id)>((select started from eligible where id=$4),$4) order by started::numeric,id limit {CANDIDATE_PAGE}"
    );
    let mut rows: Vec<(i64, String)> = sqlx::query_as(&forward)
        .bind(&settings.owner_id)
        .bind(&settings.algorithm_id)
        .bind(settings.from_boundary)
        .bind(after_id)
        .fetch_all(&mut *tx)
        .await?;
    let wrapped = rows.is_empty() && after_id.is_some();
    if wrapped {
        rows = sqlx::query_as(&format!(
            "{prefix} order by started::numeric,id limit {CANDIDATE_PAGE}"
        ))
        .bind(&settings.owner_id)
        .bind(&settings.algorithm_id)
        .bind(settings.from_boundary)
        .fetch_all(&mut *tx)
        .await?;
    }
    let cutoff: i64 =
        sqlx::query_scalar("select floor(extract(epoch from clock_timestamp())/60)::bigint*60")
            .fetch_one(&mut *tx)
            .await?;
    sqlx::query("update study_actual_continuous set last_checked_at=clock_timestamp(),next_due_at=clock_timestamp()+interval '2 seconds' where owner_id=$1 and algorithm_id=$2").bind(&settings.owner_id).bind(&settings.algorithm_id).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(Some(Ticket {
        settings,
        candidates: rows
            .into_iter()
            .map(|(study, start)| {
                Ok(Candidate {
                    study,
                    start: parse_start(start)?,
                })
            })
            .collect::<Result<_, DbError>>()?,
        cutoff,
        wrapped,
    }))
}

/// A shared resource shortage is not evidence that later candidates were read.
pub async fn backoff(pool: &PgPool, ticket: &Ticket, reason: &str) -> Result<(), DbError> {
    sqlx::query("update study_actual_continuous set next_due_at=clock_timestamp()+interval '30 seconds',last_skip_reason=$4 where owner_id=$1 and algorithm_id=$2 and config_revision=$3")
        .bind(&ticket.settings.owner_id).bind(&ticket.settings.algorithm_id).bind(ticket.settings.revision).bind(reason.chars().take(160).collect::<String>()).execute(pool).await?;
    Ok(())
}

/// Invalid saved sources do not consume an actual revision or delete artifacts.
/// Scan progress is distinct from the terminal-only automatic execution cursor.
pub async fn skipped(pool: &PgPool, ticket: &Ticket, reason: &str) -> Result<(), DbError> {
    let last = ticket.candidates.last();
    let mut tx = pool.begin().await?;
    crate::continuous::owner_lock(&mut tx, &ticket.settings.owner_id).await?;
    sqlx::query("update study_actual_continuous c set scan_study_id=$4,scan_execution_started_at=$5,last_skip_reason=$6,next_due_at=clock_timestamp()+case when $7 then interval '30 seconds' else interval '2 seconds' end where c.owner_id=$1 and c.algorithm_id=$2 and c.config_revision=$3 and not exists(select 1 from study_actualizations a where a.owner_id=c.owner_id and a.status in ('queued','running'))")
        .bind(&ticket.settings.owner_id).bind(&ticket.settings.algorithm_id).bind(ticket.settings.revision).bind(last.map(|c|c.study)).bind(last.map(|c|c.start)).bind(reason.chars().take(160).collect::<String>()).bind(ticket.wrapped||ticket.candidates.len()<CANDIDATE_PAGE as usize).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

pub async fn admit(
    pool: &PgPool,
    ticket: &Ticket,
    request: Request<'_>,
) -> Result<Admission, DbError> {
    super::admit_inner(pool, request, Some(ticket)).await
}

/// Called INSIDE the publication/failure transaction, never on worker return.
/// Revision fences make disable/date edits independent of old in-flight work.
pub(super) async fn terminal(tx: &mut Transaction<'_, Postgres>, job: &Job) -> Result<(), DbError> {
    sqlx::query("update study_actual_continuous c set cursor_study_id=a.study_id,cursor_execution_started_at=a.execution_started_at,scan_study_id=null,scan_execution_started_at=null,last_skip_reason=null,next_due_at=case when exists(select 1 from optimization_studies s left join continuous_generations g on g.id=s.continuous_generation_id and g.owner_id=s.owner_id where s.owner_id=c.owner_id and s.algorithm_id=c.algorithm_id and s.status='completed' and s.result->'best' is not null and s.result->'best'<>'null'::jsonb and coalesce(s.result#>'{best,partial}','false')<>'true'::jsonb and (coalesce(extract(epoch from g.execution_started_at),extract(epoch from s.execution_started_at),study_actual_execution_start(s.events)),s.id)>(coalesce(study_actual_execution_start(jsonb_build_array(jsonb_build_object('kind','started','at',a.source->>'executionStartedAt'))),extract(epoch from a.execution_started_at)),a.study_id)) then clock_timestamp() else greatest(clock_timestamp(),to_timestamp(a.observed_through_unix+60)) end from study_actualizations a where a.owner_id=$1 and a.study_id=$2 and a.revision=$3 and a.request_origin='automatic' and a.status in ('completed','failed') and c.owner_id=a.owner_id and c.algorithm_id=a.source->>'algorithmId' and c.cursor_generation=a.automatic_cursor_generation")
        .bind(&job.owner).bind(job.study).bind(job.revision).execute(&mut **tx).await?;
    Ok(())
}
