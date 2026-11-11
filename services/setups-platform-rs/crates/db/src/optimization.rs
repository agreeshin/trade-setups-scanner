//! Independent optimizer metadata. No raw candles, trial archive or shared caches.
use crate::DbError;
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::{Value, json};
use sqlx::{PgPool, Row, postgres::PgRow};

const ADMISSION_LOCK: i64 = 72_423_413;
pub const RUNTIME_LOCK: i64 = 72_423_414;

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
}

impl StudyRecord {
    pub fn active(&self) -> bool {
        matches!(
            self.status.as_str(),
            "queued" | "running" | "cancel_requested"
        )
    }
}

fn record(row: PgRow) -> Result<StudyRecord, DbError> {
    Ok(StudyRecord {
        id: row.try_get("id")?,
        status: row.try_get("status")?,
        spec: row.try_get("spec")?,
        progress: row.try_get("progress")?,
        result: row.try_get("result")?,
        error: row.try_get("error")?,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
        events: row.try_get("events")?,
    })
}

pub enum Admission {
    Created(Box<StudyRecord>),
    Busy(i64),
}

pub async fn create(
    pool: &PgPool,
    algorithm_id: &str,
    spec: &Value,
    progress: &Value,
) -> Result<Admission, DbError> {
    let mut tx = pool.begin().await?;
    sqlx::query("select pg_advisory_xact_lock($1)")
        .bind(ADMISSION_LOCK)
        .execute(&mut *tx)
        .await?;
    let active: Option<i64> = sqlx::query_scalar(
        "select id from optimization_studies where status in ('queued','running','cancel_requested') limit 1",
    ).fetch_optional(&mut *tx).await?;
    if let Some(id) = active {
        tx.commit().await?;
        return Ok(Admission::Busy(id));
    }
    let row = sqlx::query(
        "insert into optimization_studies (algorithm_id,spec,progress) values ($1,$2,$3) returning *",
    ).bind(algorithm_id).bind(spec).bind(progress).fetch_one(&mut *tx).await?;
    tx.commit().await?;
    Ok(Admission::Created(Box::new(record(row)?)))
}

pub async fn get(pool: &PgPool, id: i64) -> Result<Option<StudyRecord>, DbError> {
    sqlx::query("select * from optimization_studies where id=$1")
        .bind(id)
        .fetch_optional(pool)
        .await?
        .map(record)
        .transpose()
}

pub async fn list(
    pool: &PgPool,
    algorithm: Option<&str>,
    limit: i64,
) -> Result<Vec<StudyRecord>, DbError> {
    sqlx::query(
        "select * from optimization_studies where ($1::text is null or algorithm_id=$1) \
         order by (status in ('queued','running','cancel_requested')) desc, id desc limit $2",
    )
    .bind(algorithm)
    .bind(limit.clamp(1, 100))
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(record)
    .collect()
}

pub async fn claim(pool: &PgPool) -> Result<Option<StudyRecord>, DbError> {
    sqlx::query(
        "update optimization_studies set status='running',updated_at=now() \
         where id=(select id from optimization_studies where status='queued' order by id \
         for update skip locked limit 1) and status='queued' returning *",
    )
    .fetch_optional(pool)
    .await?
    .map(record)
    .transpose()
}

pub async fn cancel(pool: &PgPool, id: i64) -> Result<Option<(StudyRecord, bool)>, DbError> {
    // Queued work has no retained input or admitted CPU work. Running work keeps
    // its active slot until its owner drains and calls finish.
    let mut tx = pool.begin().await?;
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
    assert!(matches!(status, "completed" | "cancelled" | "failed"));
    let mut tx = pool.begin().await?;
    let previous: Option<String> =
        sqlx::query_scalar("select status from optimization_studies where id=$1 for update")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
    if !previous
        .as_deref()
        .is_some_and(|status| matches!(status, "running" | "cancel_requested"))
    {
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
         progress=$3,result=$4,error=$5,events=$6,updated_at=now() \
         where id=$1 and status in ('running','cancel_requested')",
    )
    .bind(id)
    .bind(status)
    .bind(progress)
    .bind(result)
    .bind(error)
    .bind(&events)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Call only while owning RUNTIME_LOCK. The previous process's RAM is gone.
pub async fn interrupt(pool: &PgPool) -> Result<u64, DbError> {
    let error = json!({"code":"interrupted_by_restart","message":"Исследование прервано перезапуском сервера; запустите новое исследование."});
    Ok(sqlx::query(
        "update optimization_studies set status='failed',result=null,error=$1,updated_at=now() \
         where status in ('queued','running','cancel_requested')",
    )
    .bind(error)
    .execute(pool)
    .await?
    .rows_affected())
}
