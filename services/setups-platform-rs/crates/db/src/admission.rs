use crate::{CacheQuery, DbError, ReplayRun, canonical_cache_key, replay_run_from_row, unix_to_ts};
use chrono::{DateTime, Utc};
use setups_domain::AlgorithmParams;
use sqlx::postgres::{PgPool, PgRow};
use sqlx::{PgConnection, Row};
use std::collections::HashSet;
use std::fmt;

const QUEUE_LOCK_KEY: i64 = 72_423_412;
const PENDING: [&str; 3] = ["queued", "running", "cancel_requested"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReplayAdmissionLimits {
    pub max_pending_runs: i64,
    pub max_work_minutes: i64,
    pub max_database_bytes: i64,
    pub estimated_bytes_per_work_minute: i64,
}

impl Default for ReplayAdmissionLimits {
    fn default() -> Self {
        ReplayAdmissionLimits {
            max_pending_runs: 10_000,
            max_work_minutes: 5_270_400_000,
            max_database_bytes: 50 * 1024 * 1024 * 1024,
            estimated_bytes_per_work_minute: 128,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdmissionResource {
    QueueJobs,
    WorkMinutes,
    DatabaseBytes,
}

impl AdmissionResource {
    pub fn as_str(self) -> &'static str {
        match self {
            AdmissionResource::QueueJobs => "queue_jobs",
            AdmissionResource::WorkMinutes => "work_minutes",
            AdmissionResource::DatabaseBytes => "database_bytes",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReplayAdmissionError {
    pub resource: AdmissionResource,
    pub limit: i64,
    pub requested: i64,
}

impl fmt::Display for ReplayAdmissionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "replay {} limit exceeded: requested {}, limit {}",
            self.resource.as_str(),
            self.requested,
            self.limit
        )
    }
}

impl std::error::Error for ReplayAdmissionError {}

#[derive(Debug)]
pub enum QueueError {
    Db(DbError),
    Admission(ReplayAdmissionError),
    Json(String),
}

impl fmt::Display for QueueError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            QueueError::Db(error) => write!(f, "{error}"),
            QueueError::Admission(error) => write!(f, "{error}"),
            QueueError::Json(message) => write!(f, "json error: {message}"),
        }
    }
}

impl std::error::Error for QueueError {}

impl From<DbError> for QueueError {
    fn from(error: DbError) -> Self {
        QueueError::Db(error)
    }
}

impl From<sqlx::Error> for QueueError {
    fn from(error: sqlx::Error) -> Self {
        QueueError::Db(DbError::Sqlx(error))
    }
}

impl From<serde_json::Error> for QueueError {
    fn from(error: serde_json::Error) -> Self {
        QueueError::Json(error.to_string())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdmissionInstrument {
    pub exchange: String,
    pub category: String,
    pub symbol: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ReplayAdmissionInput {
    pub instruments: Vec<AdmissionInstrument>,
    pub from_unix: i64,
    pub to_unix: i64,
    pub params: AlgorithmParams,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReserveStatus {
    Queued,
    Running,
}

impl ReserveStatus {
    fn as_str(self) -> &'static str {
        match self {
            ReserveStatus::Queued => "queued",
            ReserveStatus::Running => "running",
        }
    }
}

pub fn replay_work_minutes(from_unix: i64, to_unix: i64, params: &AlgorithmParams) -> i64 {
    (to_unix - from_unix + 59) / 60 + i64::from(params.baseline_long_days()) * 24 * 60
}

fn admission(resource: AdmissionResource, limit: i64, requested: i64) -> QueueError {
    QueueError::Admission(ReplayAdmissionError {
        resource,
        limit,
        requested,
    })
}

fn work_minutes_of(row: &PgRow) -> Result<i64, QueueError> {
    let from = row.try_get::<DateTime<Utc>, _>("from_ts")?.timestamp();
    let to = row.try_get::<DateTime<Utc>, _>("to_ts")?.timestamp();
    let params: serde_json::Value = row.try_get("params")?;
    let params: AlgorithmParams =
        serde_json::from_value(params).map_err(|error| QueueError::Json(error.to_string()))?;
    Ok(replay_work_minutes(from, to, &params))
}

pub(crate) async fn active_reservation_bytes(
    tx: &mut PgConnection,
    limits: &ReplayAdmissionLimits,
    exclude_run_id: Option<i64>,
) -> Result<i64, QueueError> {
    let rows = sqlx::query(
        "select from_ts, to_ts, params from replay_runs \
         where origin='manual' and status = any($1) and cache_hit = false and ($2::int4 is null or id <> $2)",
    )
    .bind(PENDING.as_slice())
    .bind(exclude_run_id.map(|id| id as i32))
    .fetch_all(&mut *tx)
    .await?;
    let mut total: i64 = 0;
    for row in rows {
        total = total
            .saturating_add(work_minutes_of(&row)? * limits.estimated_bytes_per_work_minute + 4096);
    }
    let continuous:i64=sqlx::query_scalar("select coalesce(sum(g.reserved_bytes),0)::bigint + coalesce(sum(greatest(coalesce(a.reserved_bytes,0)-coalesce(a.snapshot_bytes,0)-coalesce(octet_length(a.checkpoint),0),0)),0)::bigint from continuous_generations g left join continuous_runtime_artifacts a on a.generation_id=g.id where g.status='running'").fetch_one(tx).await?;
    total=total.saturating_add(continuous);
    Ok(total)
}

async fn database_bytes(tx: &mut PgConnection) -> Result<i64, QueueError> {
    let row = sqlx::query("select pg_database_size(current_database())::bigint as bytes")
        .fetch_one(tx)
        .await?;
    Ok(row.try_get("bytes")?)
}

pub async fn reserve_replay_runs(
    pool: &PgPool,
    input: &ReplayAdmissionInput,
    limits: &ReplayAdmissionLimits,
    initial_status: ReserveStatus,
) -> Result<Vec<ReplayRun>, QueueError> {
    let mut tx = pool.begin().await?;
    sqlx::query("select pg_advisory_xact_lock($1)")
        .bind(QUEUE_LOCK_KEY)
        .execute(&mut *tx)
        .await?;
    let count_row =
        sqlx::query("select count(*)::bigint as n from replay_runs where origin='manual' and status = any($1)")
            .bind(PENDING.as_slice())
            .fetch_one(&mut *tx)
            .await?;
    let pending_count: i64 = count_row.try_get("n")?;
    let requested_runs = pending_count + input.instruments.len() as i64;
    if requested_runs > limits.max_pending_runs {
        return Err(admission(
            AdmissionResource::QueueJobs,
            limits.max_pending_runs,
            requested_runs,
        ));
    }
    let algorithm_id = input.params.algorithm_id().to_string();
    let algorithm_version = input.params.algorithm_version().to_string();
    let cache_keys: Vec<String> = input
        .instruments
        .iter()
        .map(|instrument| {
            canonical_cache_key(&CacheQuery {
                exchange_code: instrument.exchange.clone(),
                category: instrument.category.clone(),
                symbol: instrument.symbol.clone(),
                algorithm_id: algorithm_id.clone(),
                algorithm_version: algorithm_version.clone(),
                params: input.params.clone(),
                from_unix: input.from_unix,
                to_unix: input.to_unix,
            })
        })
        .collect();
    let cached_rows = sqlx::query("select cache_key from replay_cache where cache_key = any($1)")
        .bind(&cache_keys)
        .fetch_all(&mut *tx)
        .await?;
    let cached: HashSet<String> = cached_rows
        .iter()
        .map(|row| row.try_get::<String, _>("cache_key"))
        .collect::<Result<HashSet<_>, _>>()?;
    let missing_runs = cache_keys
        .iter()
        .filter(|key| !cached.contains(*key))
        .count() as i64;
    let work_minutes =
        replay_work_minutes(input.from_unix, input.to_unix, &input.params) * missing_runs;
    if work_minutes > limits.max_work_minutes {
        return Err(admission(
            AdmissionResource::WorkMinutes,
            limits.max_work_minutes,
            work_minutes,
        ));
    }
    let database_bytes_now = database_bytes(&mut tx).await?;
    let reserved_bytes = active_reservation_bytes(&mut tx, limits, None).await?;
    let estimated_bytes =
        work_minutes * limits.estimated_bytes_per_work_minute + missing_runs * 4096;
    let requested_bytes = database_bytes_now
        .saturating_add(reserved_bytes)
        .saturating_add(estimated_bytes);
    if requested_bytes > limits.max_database_bytes {
        return Err(admission(
            AdmissionResource::DatabaseBytes,
            limits.max_database_bytes,
            requested_bytes,
        ));
    }
    let batch_id: Option<i32> = if input.instruments.len() > 1 {
        let first = &input.instruments[0];
        let row = sqlx::query(
            "insert into replay_batches \
             (exchange_code, category, algorithm_id, algorithm_version, params, from_ts, to_ts, instrument_count) \
             values ($1, $2, $3, $4, $5, $6, $7, $8) returning id",
        )
        .bind(&first.exchange)
        .bind(&first.category)
        .bind(&algorithm_id)
        .bind(&algorithm_version)
        .bind(serde_json::to_value(&input.params)?)
        .bind(unix_to_ts(input.from_unix))
        .bind(unix_to_ts(input.to_unix))
        .bind(input.instruments.len() as i32)
        .fetch_one(&mut *tx)
        .await?;
        Some(row.try_get("id")?)
    } else {
        None
    };
    let mut runs = Vec::with_capacity(input.instruments.len());
    for (instrument, key) in input.instruments.iter().zip(&cache_keys) {
        let row = sqlx::query(
            "insert into replay_runs \
             (exchange_code, category, symbol, from_ts, to_ts, algorithm_id, algorithm_version, params, status, cache_hit, batch_id) \
             values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11) \
             returning id, batch_id, exchange_code, category, symbol, from_ts, to_ts, algorithm_id, algorithm_version, params, status, cache_hit",
        )
        .bind(&instrument.exchange)
        .bind(&instrument.category)
        .bind(&instrument.symbol)
        .bind(unix_to_ts(input.from_unix))
        .bind(unix_to_ts(input.to_unix))
        .bind(&algorithm_id)
        .bind(&algorithm_version)
        .bind(serde_json::to_value(&input.params)?)
        .bind(initial_status.as_str())
        .bind(cached.contains(key))
        .bind(batch_id)
        .fetch_one(&mut *tx)
        .await?;
        runs.push(replay_run_from_row(&row)?);
    }
    tx.commit().await?;
    Ok(runs)
}

pub async fn enqueue_replay_runs(
    pool: &PgPool,
    input: &ReplayAdmissionInput,
    limits: &ReplayAdmissionLimits,
) -> Result<Vec<ReplayRun>, QueueError> {
    reserve_replay_runs(pool, input, limits, ReserveStatus::Queued).await
}

pub async fn admit_replay_cache_miss(
    pool: &PgPool,
    run: &ReplayRun,
    limits: &ReplayAdmissionLimits,
) -> Result<(), QueueError> {
    let mut tx = pool.begin().await?;
    sqlx::query("select pg_advisory_xact_lock($1)")
        .bind(QUEUE_LOCK_KEY)
        .execute(&mut *tx)
        .await?;
    let row = sqlx::query("select cache_hit, status from replay_runs where id = $1 for update")
        .bind(run.id as i32)
        .fetch_one(&mut *tx)
        .await?;
    let cache_hit: bool = row.try_get("cache_hit")?;
    let status: String = row.try_get("status")?;
    if !cache_hit || !PENDING.contains(&status.as_str()) {
        tx.commit().await?;
        return Ok(());
    }
    let work_minutes =
        replay_work_minutes(run.from_ts.timestamp(), run.to_ts.timestamp(), &run.params);
    if work_minutes > limits.max_work_minutes {
        return Err(admission(
            AdmissionResource::WorkMinutes,
            limits.max_work_minutes,
            work_minutes,
        ));
    }
    let database_bytes_now = database_bytes(&mut tx).await?;
    let reserved_bytes = active_reservation_bytes(&mut tx, limits, Some(run.id)).await?;
    let estimated_bytes = work_minutes * limits.estimated_bytes_per_work_minute + 4096;
    let requested_bytes = database_bytes_now
        .saturating_add(reserved_bytes)
        .saturating_add(estimated_bytes);
    if requested_bytes > limits.max_database_bytes {
        return Err(admission(
            AdmissionResource::DatabaseBytes,
            limits.max_database_bytes,
            requested_bytes,
        ));
    }
    sqlx::query("update replay_runs set cache_hit = false, updated_at = now() where id = $1")
        .bind(run.id as i32)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn cancel_replay_run(
    pool: &PgPool,
    run_id: i64,
) -> Result<Option<ReplayRun>, QueueError> {
    let mut tx = pool.begin().await?;
    let row = sqlx::query(
        "select id, batch_id, exchange_code, category, symbol, from_ts, to_ts, algorithm_id, algorithm_version, params, status, cache_hit, origin \
         from replay_runs where id = $1 for update",
    )
    .bind(run_id as i32)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(row) = row else {
        tx.commit().await?;
        return Ok(None);
    };
    let run = replay_run_from_row(&row)?;
    if row.try_get::<String,_>("origin")?=="continuous" {return Ok(Some(run));}
    if !PENDING.contains(&run.status.as_str()) {
        tx.commit().await?;
        return Ok(Some(run));
    }
    let new_status = if run.status == "queued" {
        "cancelled"
    } else {
        "cancel_requested"
    };
    let updated = sqlx::query(
        "update replay_runs set status = $2, updated_at = now() where id = $1 \
         returning id, batch_id, exchange_code, category, symbol, from_ts, to_ts, algorithm_id, algorithm_version, params, status, cache_hit",
    )
    .bind(run_id as i32)
    .bind(new_status)
    .fetch_one(&mut *tx)
    .await?;
    let updated = replay_run_from_row(&updated)?;
    tx.commit().await?;
    Ok(Some(updated))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PurgeOutcome {
    Purged {
        runs_deleted: u64,
        batches_deleted: u64,
        cache_deleted: u64,
        quality_intervals_deleted: u64,
    },
    Active {
        active: i64,
        queued_cancelled: u64,
        running_cancel_requested: u64,
    },
}

pub async fn purge_replay_runs(pool: &PgPool) -> Result<PurgeOutcome, QueueError> {
    let mut tx = pool.begin().await?;
    sqlx::query("select pg_advisory_xact_lock($1)")
        .bind(QUEUE_LOCK_KEY)
        .execute(&mut *tx)
        .await?;
    let count_row =
        sqlx::query("select count(*)::bigint as n from replay_runs where origin='manual' and status = any($1)")
            .bind(["running", "cancel_requested"].as_slice())
            .fetch_one(&mut *tx)
            .await?;
    let active: i64 = count_row.try_get("n")?;
    if active > 0 {
        let queued = sqlx::query(
            "update replay_runs set status = 'cancelled', updated_at = now() where origin='manual' and status = 'queued'",
        )
        .execute(&mut *tx)
        .await?
        .rows_affected();
        let running = sqlx::query(
            "update replay_runs set status = 'cancel_requested', updated_at = now() where origin='manual' and status = 'running'",
        )
        .execute(&mut *tx)
        .await?
        .rows_affected();
        tx.commit().await?;
        return Ok(PurgeOutcome::Active {
            active,
            queued_cancelled: queued,
            running_cancel_requested: running,
        });
    }
    let quality_intervals_deleted = sqlx::query("delete from replay_quality_intervals where run_id in (select id from replay_runs where origin='manual')")
        .execute(&mut *tx)
        .await?
        .rows_affected();
    let cache_deleted = sqlx::query("delete from replay_cache")
        .execute(&mut *tx)
        .await?
        .rows_affected();
    let runs_deleted = sqlx::query("delete from replay_runs where origin='manual'")
        .execute(&mut *tx)
        .await?
        .rows_affected();
    let batches_deleted = sqlx::query("delete from replay_batches where origin='manual'")
        .execute(&mut *tx)
        .await?
        .rows_affected();
    tx.commit().await?;
    Ok(PurgeOutcome::Purged {
        runs_deleted,
        batches_deleted,
        cache_deleted,
        quality_intervals_deleted,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteRunOutcome {
    Deleted { quality_intervals_deleted: u64 },
    Active,
    Referenced,
}

pub async fn delete_replay_run(
    pool: &PgPool,
    run_id: i64,
) -> Result<Option<DeleteRunOutcome>, QueueError> {
    let mut tx = pool.begin().await?;
    sqlx::query("select pg_advisory_xact_lock($1)")
        .bind(QUEUE_LOCK_KEY)
        .execute(&mut *tx)
        .await?;
    let row = sqlx::query("select status,continuous_generation_id from replay_runs where id = $1")
        .bind(run_id as i32)
        .fetch_optional(&mut *tx)
        .await?;
    let Some(row) = row else {
        tx.commit().await?;
        return Ok(None);
    };
    let status: String = row.try_get("status")?;
    if row.try_get::<Option<i64>,_>("continuous_generation_id")?.is_some() {return Ok(Some(DeleteRunOutcome::Referenced));}
    if PENDING.contains(&status.as_str()) {
        tx.commit().await?;
        return Ok(Some(DeleteRunOutcome::Active));
    }
    let quality_intervals_deleted =
        sqlx::query("delete from replay_quality_intervals where run_id = $1")
            .bind(run_id as i32)
            .execute(&mut *tx)
            .await?
            .rows_affected();
    sqlx::query("delete from replay_runs where id = $1")
        .bind(run_id as i32)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Some(DeleteRunOutcome::Deleted {
        quality_intervals_deleted,
    }))
}

pub async fn interrupt_queued_runs(pool: &PgPool) -> Result<u64, QueueError> {
    let result = sqlx::query(
        "update replay_runs set status = 'cancelled', updated_at = now() where origin='manual' and status = 'queued'",
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn work_minutes_ceil_and_baseline_match_ts() {
        let params = AlgorithmParams::Spike(setups_domain::SpikeParams::default());
        assert_eq!(replay_work_minutes(0, 60, &params), 1 + 30 * 1440);
        assert_eq!(replay_work_minutes(0, 61, &params), 2 + 30 * 1440);
        assert_eq!(replay_work_minutes(0, 59, &params), 1 + 30 * 1440);
        let AlgorithmParams::Spike(mut small) = params else {
            unreachable!();
        };
        small.baseline_long_days = 1;
        assert_eq!(
            replay_work_minutes(0, 3600, &AlgorithmParams::Spike(small)),
            60 + 1440
        );
    }

    #[test]
    fn default_limits_match_ts_constants() {
        let limits = ReplayAdmissionLimits::default();
        assert_eq!(limits.max_pending_runs, 10_000);
        assert_eq!(limits.max_work_minutes, 5_270_400_000);
        assert_eq!(limits.max_database_bytes, 50 * 1024 * 1024 * 1024);
        assert_eq!(limits.estimated_bytes_per_work_minute, 128);
    }

    #[test]
    fn admission_error_message_matches_ts() {
        let error = ReplayAdmissionError {
            resource: AdmissionResource::QueueJobs,
            limit: 10_000,
            requested: 10_001,
        };
        assert_eq!(
            error.to_string(),
            "replay queue_jobs limit exceeded: requested 10001, limit 10000"
        );
    }
}
