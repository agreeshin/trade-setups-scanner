use serde_json::Value;
use setups_db::{DbError, ReplayRun, settings_get};
use setups_detector::PriceVolumeSpikeDetector;
use setups_detector::quality::{QualityInterval, ReplayQuality, ReplayQualityTracker};
use setups_domain::decimal::DecimalError;
use setups_domain::{DetectorBar, PushStatus};
use setups_replay::{CandleSource, CandleSourceQuery, PAGE_LIMIT};
use sqlx::Row;
use sqlx::postgres::PgPool;
use std::fmt;
use std::time::Duration;

const CACHE_LOCK_KEY: i64 = 72_423_411;
const PAUSE_POLL: Duration = Duration::from_secs(1);

type IntervalColumns = (Vec<i64>, Vec<i64>, Vec<String>);

#[derive(Debug)]
pub enum WorkerError {
    Db(DbError),
    Detector(DecimalError),
    Source(String),
    PageLimitExceeded(usize),
    Json(String),
}

impl fmt::Display for WorkerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WorkerError::Db(error) => write!(f, "{error}"),
            WorkerError::Detector(error) => write!(f, "detector error: {error}"),
            WorkerError::Source(message) => write!(f, "replay source error: {message}"),
            WorkerError::PageLimitExceeded(size) => {
                write!(f, "replay source exceeded page limit: {size}")
            }
            WorkerError::Json(message) => write!(f, "json error: {message}"),
        }
    }
}

impl std::error::Error for WorkerError {}

impl From<DbError> for WorkerError {
    fn from(error: DbError) -> Self {
        WorkerError::Db(error)
    }
}

impl From<DecimalError> for WorkerError {
    fn from(error: DecimalError) -> Self {
        WorkerError::Detector(error)
    }
}

impl From<sqlx::Error> for WorkerError {
    fn from(error: sqlx::Error) -> Self {
        WorkerError::Db(DbError::Sqlx(error))
    }
}

impl From<serde_json::Error> for WorkerError {
    fn from(error: serde_json::Error) -> Self {
        WorkerError::Json(error.to_string())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunOutcome {
    Completed,
    Cancelled,
}

pub async fn run_one<S: CandleSource>(
    pool: &PgPool,
    run: &ReplayRun,
    source: &S,
) -> Result<RunOutcome, WorkerError> {
    let result = run_one_inner(pool, run, source).await;
    if let Err(error) = &result {
        let message = error.to_string();
        let truncated: String = message.chars().take(300).collect();
        let _ = mark_run_failed(pool, run.id, &truncated).await;
    }
    result
}

async fn run_one_inner<S: CandleSource>(
    pool: &PgPool,
    run: &ReplayRun,
    source: &S,
) -> Result<RunOutcome, WorkerError> {
    let generation = current_generation(pool).await?;
    let from_unix = run.from_ts.timestamp();
    let to_unix = run.to_ts.timestamp();
    let required_minutes = run.params.baseline_long_days as u64 * 1440;
    let mut detector = PriceVolumeSpikeDetector::new(run.params.clone(), None)?;
    let mut tracker = ReplayQualityTracker::new(from_unix, to_unix, required_minutes);
    let warmup_from = from_unix - required_minutes as i64 * 60;
    let mut warmup_minutes: i64 = 0;
    let mut processed_minutes: i64 = 0;
    let mut cursor: Option<i64> = None;

    loop {
        if run_status(pool, run.id).await? == "cancel_requested" {
            return Ok(RunOutcome::Cancelled);
        }
        if replay_paused(pool).await? {
            tokio::time::sleep(PAUSE_POLL).await;
            continue;
        }
        let page = source
            .read(&CandleSourceQuery {
                from_unix: warmup_from,
                to_unix,
                after_unix: cursor,
                limit: Some(PAGE_LIMIT as u32),
            })
            .await
            .map_err(WorkerError::Source)?;
        if page.bars.is_empty() {
            break;
        }
        if page.bars.len() > PAGE_LIMIT {
            return Err(WorkerError::PageLimitExceeded(page.bars.len()));
        }
        let mut setups_page: Vec<Value> = Vec::new();
        for bar in &page.bars {
            let result = detector.push(&DetectorBar {
                open_time_unix: bar.open_time_unix,
                close: bar.close.clone(),
                quote_turnover: bar.quote_turnover.clone(),
            })?;
            tracker.observe(bar.open_time_unix, &result);
            match result.status {
                PushStatus::Warmup => warmup_minutes += 1,
                PushStatus::Evaluated => processed_minutes += 1,
            }
            if let Some(setup) = &result.setup
                && bar.open_time_unix >= from_unix
            {
                setups_page.push(serde_json::to_value(setup)?);
            }
        }
        let intervals = tracker.drain();
        save_page(
            pool,
            run.id,
            &setups_page,
            &intervals,
            warmup_minutes,
            processed_minutes,
            tracker.quality(),
        )
        .await?;
        let Some(next) = page.next_cursor_unix else {
            break;
        };
        if let Some(current) = cursor
            && next <= current
        {
            return Err(WorkerError::Source(
                "replay source cursor did not advance".into(),
            ));
        }
        cursor = Some(next);
    }

    tracker.finish();
    let intervals = tracker.drain();
    publish_run(
        pool,
        run,
        &generation,
        tracker.quality(),
        warmup_minutes,
        processed_minutes,
        &intervals,
    )
    .await?;
    Ok(RunOutcome::Completed)
}

async fn run_status(pool: &PgPool, run_id: i64) -> Result<String, WorkerError> {
    let row = sqlx::query("select status from replay_runs where id = $1")
        .bind(run_id as i32)
        .fetch_one(pool)
        .await?;
    Ok(row.try_get::<String, _>("status")?)
}

async fn replay_paused(pool: &PgPool) -> Result<bool, WorkerError> {
    Ok(settings_get(pool, "replay")
        .await?
        .and_then(|value| value.get("paused").cloned())
        .is_some_and(|paused| paused == Value::Bool(true)))
}

async fn current_generation(pool: &PgPool) -> Result<String, WorkerError> {
    Ok(settings_get(pool, "replay-cache-generation")
        .await?
        .and_then(|value| {
            value
                .get("generation")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| "initial".to_string()))
}

fn interval_columns(intervals: &[QualityInterval]) -> Result<IntervalColumns, WorkerError> {
    let mut from_unix = Vec::with_capacity(intervals.len());
    let mut to_unix = Vec::with_capacity(intervals.len());
    let mut reasons = Vec::with_capacity(intervals.len());
    for interval in intervals {
        from_unix.push(interval.from_unix);
        to_unix.push(interval.to_unix);
        reasons.push(
            serde_json::to_value(interval.reason)?
                .as_str()
                .map(str::to_string)
                .unwrap_or_default(),
        );
    }
    Ok((from_unix, to_unix, reasons))
}

async fn save_page(
    pool: &PgPool,
    run_id: i64,
    setups: &[Value],
    intervals: &[QualityInterval],
    warmup_minutes: i64,
    processed_minutes: i64,
    quality: &ReplayQuality,
) -> Result<(), WorkerError> {
    let setups_json = Value::Array(setups.to_vec());
    let quality_json = serde_json::to_value(quality)?;
    let (from_unix, to_unix, reasons) = interval_columns(intervals)?;
    let mut tx = pool.begin().await?;
    if !intervals.is_empty() {
        sqlx::query(
            "insert into replay_quality_intervals (run_id, from_unix, to_unix, reason) \
             select $1, f, t, r from unnest($2::bigint[], $3::bigint[], $4::text[]) as x(f, t, r)",
        )
        .bind(run_id as i32)
        .bind(&from_unix)
        .bind(&to_unix)
        .bind(&reasons)
        .execute(&mut *tx)
        .await?;
    }
    sqlx::query(
        "update replay_runs set setups = coalesce(setups, '[]'::jsonb) || $2::jsonb, \
         warmup_minutes = $3, processed_minutes = $4, quality = $5, updated_at = now() \
         where id = $1 and status in ('running', 'cancel_requested')",
    )
    .bind(run_id as i32)
    .bind(&setups_json)
    .bind(warmup_minutes as i32)
    .bind(processed_minutes as i32)
    .bind(&quality_json)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

async fn publish_run(
    pool: &PgPool,
    run: &ReplayRun,
    generation: &str,
    quality: &ReplayQuality,
    warmup_minutes: i64,
    processed_minutes: i64,
    intervals: &[QualityInterval],
) -> Result<(), WorkerError> {
    let quality_json = serde_json::to_value(quality)?;
    let cache_key = setups_db::canonical_cache_key(&setups_db::run_query(run));
    let (from_unix, to_unix, reasons) = interval_columns(intervals)?;
    let mut tx = pool.begin().await?;
    let row = sqlx::query("select status from replay_runs where id = $1 for update")
        .bind(run.id as i32)
        .fetch_one(&mut *tx)
        .await?;
    if row.try_get::<String, _>("status")? != "running" {
        tx.commit().await?;
        return Ok(());
    }
    if !intervals.is_empty() {
        sqlx::query(
            "insert into replay_quality_intervals (run_id, from_unix, to_unix, reason) \
             select $1, f, t, r from unnest($2::bigint[], $3::bigint[], $4::text[]) as x(f, t, r)",
        )
        .bind(run.id as i32)
        .bind(&from_unix)
        .bind(&to_unix)
        .bind(&reasons)
        .execute(&mut *tx)
        .await?;
    }
    sqlx::query(
        "update replay_runs set status = 'done', quality = $2, quality_source_run_id = id, \
         quality_computed_at = now(), updated_at = now() where id = $1",
    )
    .bind(run.id as i32)
    .bind(&quality_json)
    .execute(&mut *tx)
    .await?;
    sqlx::query("select pg_advisory_xact_lock($1)")
        .bind(CACHE_LOCK_KEY)
        .execute(&mut *tx)
        .await?;
    let settings_row = settings_get_tx(&mut tx).await?;
    let current = settings_row
        .as_ref()
        .and_then(|value| value.get("generation").and_then(Value::as_str))
        .unwrap_or("initial");
    if current == generation {
        sqlx::query("savepoint replay_cache_publish")
            .execute(&mut *tx)
            .await?;
        let publish = sqlx::query(
            "insert into replay_cache \
             (cache_key, exchange_code, category, symbol, algorithm_id, algorithm_version, params, from_ts, to_ts, setups, warmup_minutes, processed_minutes, quality, quality_source_run_id, computed_at) \
             select $2, exchange_code, category, symbol, algorithm_id, algorithm_version, params, from_ts, to_ts, \
             coalesce(setups, '[]'::jsonb), $3, $4, quality, quality_source_run_id, now() \
             from replay_runs where id = $1 \
             on conflict (cache_key) do update set \
             setups = excluded.setups, warmup_minutes = excluded.warmup_minutes, \
             processed_minutes = excluded.processed_minutes, quality = excluded.quality, \
             quality_source_run_id = excluded.quality_source_run_id, computed_at = excluded.computed_at",
        )
        .bind(run.id as i32)
        .bind(&cache_key)
        .bind(warmup_minutes as i32)
        .bind(processed_minutes as i32)
        .execute(&mut *tx)
        .await;
        if let Err(error) = publish {
            sqlx::query("rollback to savepoint replay_cache_publish")
                .execute(&mut *tx)
                .await?;
            let message: String = error.to_string().chars().take(300).collect();
            sqlx::query("update replay_runs set error = $2 where id = $1")
                .bind(run.id as i32)
                .bind(&message)
                .execute(&mut *tx)
                .await?;
        }
    }
    tx.commit().await?;
    Ok(())
}

async fn settings_get_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
) -> Result<Option<Value>, WorkerError> {
    let row = sqlx::query("select value from settings where key = 'replay-cache-generation'")
        .fetch_optional(&mut **tx)
        .await?;
    row.map(|row| row.try_get::<Value, _>("value"))
        .transpose()
        .map_err(|error| WorkerError::Db(DbError::Sqlx(error)))
}

async fn mark_run_failed(pool: &PgPool, run_id: i64, message: &str) -> Result<(), WorkerError> {
    sqlx::query(
        "update replay_runs set status = 'failed', error = $2, updated_at = now() where id = $1",
    )
    .bind(run_id as i32)
    .bind(message)
    .execute(pool)
    .await?;
    Ok(())
}
