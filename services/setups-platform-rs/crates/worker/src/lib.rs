use serde_json::Value;
use setups_db::admission::{ReplayAdmissionLimits, admit_replay_cache_miss, interrupt_queued_runs};
use setups_db::runs::complete_run_from_cache_by_key;
use setups_db::{DbError, ReplayRun, settings_get};
use setups_db::{canonical_cache_key, claim_next_queued_run, fail_interrupted_runs, run_query};
use setups_detector::PriceVolumeSpikeDetector;
use setups_detector::quality::{QualityInterval, ReplayQuality, ReplayQualityTracker};
use setups_domain::decimal::DecimalError;
use setups_domain::{
    DetectorBar, PushStatus, SPIKE_ALGORITHM_ID, SPIKE_ALGORITHM_VERSION, SPIKE_DROP_ALGORITHM_ID,
    SPIKE_DROP_ALGORITHM_VERSION,
};
use setups_mdclient::{ApiCandleSource, MarketDataClient};
use setups_replay::{CandleSource, CandleSourceQuery, PAGE_LIMIT};
use sqlx::Row;
use sqlx::postgres::PgPool;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::Notify;

pub mod adaptive;
pub mod live;

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

impl From<setups_db::admission::QueueError> for WorkerError {
    fn from(error: setups_db::admission::QueueError) -> Self {
        match error {
            setups_db::admission::QueueError::Db(db) => WorkerError::Db(db),
            other => WorkerError::Source(other.to_string()),
        }
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
    let required_minutes = run.params.baseline_long_days() as u64 * 1440;
    let mut detector = PriceVolumeSpikeDetector::with_params(run.params.clone(), None)?;
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

/// Persist drained quality intervals with TS `saveQualityPage` semantics: if the
/// run's last stored interval is adjacent to the first drained one and carries
/// the same reason, extend it instead of inserting a new row.
async fn insert_intervals_merged(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    run_id: i64,
    intervals: &[QualityInterval],
) -> Result<(), WorkerError> {
    if intervals.is_empty() {
        return Ok(());
    }
    let first = &intervals[0];
    let first_reason = serde_json::to_value(first.reason)?
        .as_str()
        .map(str::to_string)
        .unwrap_or_default();
    let last = sqlx::query(
        "select from_unix::bigint as from_unix, to_unix::bigint as to_unix, reason \
         from replay_quality_intervals where run_id = $1 \
         order by from_unix desc limit 1",
    )
    .bind(run_id as i32)
    .fetch_optional(&mut **tx)
    .await?;
    let mut start = 0;
    if let Some(last) = last {
        let last_from: i64 = last.try_get("from_unix")?;
        let last_to: i64 = last.try_get("to_unix")?;
        let last_reason: String = last.try_get("reason")?;
        if last_reason == first_reason && last_to == first.from_unix {
            sqlx::query(
                "update replay_quality_intervals set to_unix = $2::bigint \
                 where run_id = $1 and from_unix = $3::bigint",
            )
            .bind(run_id as i32)
            .bind(first.to_unix)
            .bind(last_from)
            .execute(&mut **tx)
            .await?;
            start = 1;
        }
    }
    if start < intervals.len() {
        let (from_unix, to_unix, reasons) = interval_columns(&intervals[start..])?;
        sqlx::query(
            "insert into replay_quality_intervals (run_id, from_unix, to_unix, reason) \
             select $1, f, t, r from unnest($2::bigint[], $3::bigint[], $4::text[]) as x(f, t, r)",
        )
        .bind(run_id as i32)
        .bind(&from_unix)
        .bind(&to_unix)
        .bind(&reasons)
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
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
    let mut tx = pool.begin().await?;
    insert_intervals_merged(&mut tx, run_id, intervals).await?;
    sqlx::query(
        "update replay_runs set setups = coalesce(setups, '[]'::jsonb) || $2::jsonb, \
         warmup_minutes = $3, processed_minutes = $4, quality = $5, \
         quality_source_run_id = $1, updated_at = now() \
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
    let mut tx = pool.begin().await?;
    let row = sqlx::query("select status from replay_runs where id = $1 for update")
        .bind(run.id as i32)
        .fetch_one(&mut *tx)
        .await?;
    if row.try_get::<String, _>("status")? != "running" {
        tx.commit().await?;
        return Ok(());
    }
    insert_intervals_merged(&mut tx, run.id, intervals).await?;
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

const SHUTDOWN_ERROR: &str = "interrupted by application shutdown";
const UNSUPPORTED_ALGORITHM: &str =
    "unsupported_algorithm_version: create a new run using the current algorithm";

pub struct JobsHandle {
    stopped: Arc<AtomicBool>,
    notify: Arc<Notify>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl JobsHandle {
    pub async fn stop(mut self) {
        self.stopped.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
        for task in self.tasks.drain(..) {
            let _ = task.await;
        }
    }
}

pub async fn start_replay_jobs(
    pool: PgPool,
    market_url: String,
    market_key: String,
    workers: usize,
    poll: Duration,
) -> Result<JobsHandle, WorkerError> {
    if workers == 0 {
        // Shadow mode: serve the API without claiming replay work or
        // mutating run state during startup recovery.
        let stopped = Arc::new(AtomicBool::new(true));
        let notify = Arc::new(Notify::new());
        return Ok(JobsHandle {
            stopped,
            notify,
            tasks: Vec::new(),
        });
    }
    fail_interrupted_runs(&pool).await?;
    interrupt_queued_runs(&pool).await?;
    sqlx::query(
        "update replay_runs set status = 'cancelled', updated_at = now() \
         where origin='manual' and status = 'cancel_requested'",
    )
    .execute(&pool)
    .await?;
    let stopped = Arc::new(AtomicBool::new(false));
    let notify = Arc::new(Notify::new());
    let mut tasks = Vec::new();
    for _ in 0..workers.max(1) {
        let worker_pool = pool.clone();
        let worker_market_url = market_url.clone();
        let worker_market_key = market_key.clone();
        let worker_stopped = stopped.clone();
        let worker_notify = notify.clone();
        tasks.push(tokio::spawn(async move {
            worker_loop(
                worker_pool,
                worker_market_url,
                worker_market_key,
                worker_stopped,
                worker_notify,
                poll,
            )
            .await;
        }));
    }
    Ok(JobsHandle {
        stopped,
        notify,
        tasks,
    })
}

async fn worker_loop(
    pool: PgPool,
    market_url: String,
    market_key: String,
    stopped: Arc<AtomicBool>,
    notify: Arc<Notify>,
    poll: Duration,
) {
    loop {
        if stopped.load(Ordering::SeqCst) {
            break;
        }
        let paused = replay_paused(&pool).await.unwrap_or(false);
        let claimed = if paused {
            None
        } else {
            claim_next_queued_run(&pool).await.ok().flatten()
        };
        if let Some(run) = claimed {
            execute_queued_run(&pool, &run, &market_url, &market_key, &stopped).await;
            continue;
        }
        tokio::select! {
            _ = notify.notified() => {}
            _ = tokio::time::sleep(poll) => {}
        }
    }
}

pub async fn execute_queued_run(
    pool: &PgPool,
    run: &ReplayRun,
    market_url: &str,
    market_key: &str,
    stopped: &AtomicBool,
) {
    let outcome = execute_inner(pool, run, market_url, market_key).await;
    if let Err(message) = &outcome {
        let truncated: String = message.chars().take(500).collect();
        let _ = sqlx::query(
            "update replay_runs set status = 'failed', error = $2, updated_at = now() \
             where id = $1 and status = 'running'",
        )
        .bind(run.id as i32)
        .bind(&truncated)
        .execute(pool)
        .await;
    }
    let _ = sqlx::query(
        "update replay_runs set status = 'cancelled', updated_at = now() \
         where id = $1 and status = 'cancel_requested'",
    )
    .bind(run.id as i32)
    .execute(pool)
    .await;
    if stopped.load(Ordering::SeqCst) {
        let _ = sqlx::query(
            "update replay_runs set status = 'failed', error = $2, updated_at = now() \
             where id = $1 and status = 'running'",
        )
        .bind(run.id as i32)
        .bind(SHUTDOWN_ERROR)
        .execute(pool)
        .await;
    }
}

async fn execute_inner(
    pool: &PgPool,
    run: &ReplayRun,
    market_url: &str,
    market_key: &str,
) -> Result<(), String> {
    let cache_key = canonical_cache_key(&run_query(run));
    if complete_run_from_cache_by_key(pool, run.id, &cache_key)
        .await
        .map_err(|error| error.to_string())?
    {
        return Ok(());
    }
    if run.cache_hit {
        admit_replay_cache_miss(pool, run, &ReplayAdmissionLimits::default())
            .await
            .map_err(|error| error.to_string())?;
    }
    let algorithm_supported = matches!(
        (run.algorithm_id.as_str(), run.algorithm_version.as_str()),
        (SPIKE_ALGORITHM_ID, SPIKE_ALGORITHM_VERSION)
            | (SPIKE_DROP_ALGORITHM_ID, SPIKE_DROP_ALGORITHM_VERSION)
    );
    if !algorithm_supported {
        return Err(UNSUPPORTED_ALGORITHM.to_string());
    }
    let catalog = MarketDataClient::new(market_url, market_key)
        .instruments()
        .await
        .map_err(|error| error.to_string())?;
    let instrument = catalog
        .iter()
        .find(|item| {
            item.exchange == run.exchange_code
                && item.category == run.category
                && item.symbol == run.symbol
        })
        .ok_or_else(|| "instrument not found in market data catalog".to_string())?;
    let source = ApiCandleSource::new(market_url, market_key, instrument.id);
    run_one(pool, run, &source)
        .await
        .map(|_| ())
        .map_err(|error| error.to_string())
}
