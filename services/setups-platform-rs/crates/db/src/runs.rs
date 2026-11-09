use crate::{DbError, unix_to_ts};
use chrono::{DateTime, Utc};
use serde_json::Value;
use setups_domain::SpikeParams;
use sqlx::Row;
use sqlx::postgres::{PgPool, PgRow};

const CACHE_LOCK_KEY: i64 = 72_423_411;

#[derive(Debug, Clone, PartialEq)]
pub struct ReplayRunFull {
    pub id: i64,
    pub batch_id: Option<i64>,
    pub exchange_code: String,
    pub category: String,
    pub symbol: String,
    pub status: String,
    pub cache_hit: bool,
    pub setups: Value,
    pub setup_count: i64,
    pub warmup_minutes: Option<i64>,
    pub processed_minutes: Option<i64>,
    pub error: Option<String>,
    pub algorithm_id: String,
    pub algorithm_version: String,
    pub params: Value,
    pub from_ts: DateTime<Utc>,
    pub to_ts: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
    pub quality: Option<Value>,
    pub quality_source_run_id: Option<i64>,
    pub quality_computed_at: Option<DateTime<Utc>>,
}

const RUN_COLUMNS: &str = "id, batch_id, exchange_code, category, symbol, status, cache_hit, setups, warmup_minutes, \
     processed_minutes, error, algorithm_id, algorithm_version, from_ts, to_ts, created_at, params, \
     quality, quality_source_run_id, quality_computed_at";

fn run_full_from_row(row: &PgRow) -> Result<ReplayRunFull, DbError> {
    Ok(ReplayRunFull {
        id: i64::from(row.try_get::<i32, _>("id")?),
        batch_id: row.try_get::<Option<i32>, _>("batch_id")?.map(i64::from),
        exchange_code: row.try_get("exchange_code")?,
        category: row.try_get("category")?,
        symbol: row.try_get("symbol")?,
        status: row.try_get("status")?,
        cache_hit: row.try_get("cache_hit")?,
        setups: row.try_get("setups")?,
        setup_count: i64::from(row.try_get::<i32, _>("setup_count")?),
        warmup_minutes: row
            .try_get::<Option<i32>, _>("warmup_minutes")?
            .map(i64::from),
        processed_minutes: row
            .try_get::<Option<i32>, _>("processed_minutes")?
            .map(i64::from),
        error: row.try_get("error")?,
        algorithm_id: row.try_get("algorithm_id")?,
        algorithm_version: row.try_get("algorithm_version")?,
        params: row.try_get("params")?,
        from_ts: row.try_get("from_ts")?,
        to_ts: row.try_get("to_ts")?,
        created_at: row.try_get("created_at")?,
        quality: row.try_get("quality")?,
        quality_source_run_id: row
            .try_get::<Option<i32>, _>("quality_source_run_id")?
            .map(i64::from),
        quality_computed_at: row.try_get("quality_computed_at")?,
    })
}

pub async fn list_recent_runs(pool: &PgPool, limit: i64) -> Result<Vec<ReplayRunFull>, DbError> {
    let rows = sqlx::query(&format!(
        "select {RUN_COLUMNS}, coalesce(jsonb_array_length(setups), 0) as setup_count \
         from replay_runs where batch_id is null order by id desc limit $1"
    ))
    .bind(limit as i32)
    .fetch_all(pool)
    .await?;
    rows.iter()
        .map(run_full_from_row)
        .collect::<Result<Vec<_>, _>>()
}

pub async fn get_run_full(pool: &PgPool, run_id: i64) -> Result<Option<ReplayRunFull>, DbError> {
    let row = sqlx::query(&format!(
        "select {RUN_COLUMNS}, coalesce(jsonb_array_length(setups), 0) as setup_count \
         from replay_runs where id = $1"
    ))
    .bind(run_id as i32)
    .fetch_optional(pool)
    .await?;
    row.as_ref().map(run_full_from_row).transpose()
}

#[derive(Debug, Clone, PartialEq)]
pub struct RunPage {
    pub run: ReplayRunFull,
    pub setups_page: Value,
}

pub async fn get_run_page(
    pool: &PgPool,
    run_id: i64,
    offset: i64,
    limit: i64,
) -> Result<Option<RunPage>, DbError> {
    let path = format!("$[{offset} to {}]", (offset + limit - 1).min(2_147_483_647));
    let query = if offset > 2_147_483_647 {
        format!(
            "select {RUN_COLUMNS}, coalesce(jsonb_array_length(setups), 0) as setup_count, '[]'::jsonb as setups_page from replay_runs where id = $1"
        )
    } else {
        format!(
            "select {RUN_COLUMNS}, coalesce(jsonb_array_length(setups), 0) as setup_count, \
             jsonb_path_query_array(coalesce(setups, '[]'::jsonb), $2::jsonpath) as setups_page \
             from replay_runs where id = $1"
        )
    };
    let mut statement = sqlx::query(&query).bind(run_id as i32);
    if offset <= 2_147_483_647 {
        statement = statement.bind(path);
    }
    let row = statement.fetch_optional(pool).await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let setups_page: Value = row.try_get("setups_page")?;
    Ok(Some(RunPage {
        run: run_full_from_row(&row)?,
        setups_page,
    }))
}

#[derive(Debug, Clone, PartialEq)]
pub struct BatchSummary {
    pub id: i64,
    pub exchange_code: String,
    pub category: String,
    pub algorithm_id: String,
    pub algorithm_version: Option<String>,
    pub params: Value,
    pub from_ts: DateTime<Utc>,
    pub to_ts: DateTime<Utc>,
    pub instrument_count: i64,
    pub created_at: DateTime<Utc>,
    pub status: String,
    pub cache_hit: bool,
    pub setup_count: i64,
    pub active_instruments: i64,
    pub done_instruments: i64,
    pub failed_instruments: i64,
    pub cancelled_instruments: i64,
}

const BATCH_SELECT: &str = "select b.id, b.exchange_code, b.category, b.algorithm_id, b.algorithm_version, b.params, b.from_ts, b.to_ts, b.instrument_count, b.created_at, \
     count(*) filter (where r.status in ('queued','running','cancel_requested')) as active_instruments, \
     count(*) filter (where r.status = 'done') as done_instruments, \
     count(*) filter (where r.status = 'failed') as failed_instruments, \
     count(*) filter (where r.status = 'cancelled') as cancelled_instruments, \
     coalesce(sum(case when jsonb_typeof(r.setups) = 'array' then jsonb_array_length(r.setups) else 0 end), 0) as setup_count, \
     bool_and(r.cache_hit) as cache_hit";

fn batch_from_row(row: &PgRow) -> BatchSummary {
    let active: i64 = row.try_get("active_instruments").unwrap_or(0);
    let done: i64 = row.try_get("done_instruments").unwrap_or(0);
    let failed: i64 = row.try_get("failed_instruments").unwrap_or(0);
    BatchSummary {
        id: i64::from(row.try_get::<i32, _>("id").unwrap_or(0)),
        exchange_code: row.try_get("exchange_code").unwrap_or_default(),
        category: row.try_get("category").unwrap_or_default(),
        algorithm_id: row.try_get("algorithm_id").unwrap_or_default(),
        algorithm_version: row.try_get("algorithm_version").unwrap(),
        params: row.try_get("params").unwrap_or(Value::Null),
        from_ts: row.try_get("from_ts").unwrap_or_else(|_| unix_to_ts(0)),
        to_ts: row.try_get("to_ts").unwrap_or_else(|_| unix_to_ts(0)),
        instrument_count: i64::from(row.try_get::<i32, _>("instrument_count").unwrap_or(0)),
        created_at: row.try_get("created_at").unwrap_or_else(|_| unix_to_ts(0)),
        status: if active > 0 {
            "running"
        } else if done > 0 {
            "done"
        } else if failed > 0 {
            "failed"
        } else {
            "cancelled"
        }
        .to_string(),
        cache_hit: row.try_get::<Option<bool>, _>("cache_hit").unwrap_or(None) == Some(true),
        setup_count: row.try_get("setup_count").unwrap_or(0),
        active_instruments: active,
        done_instruments: done,
        failed_instruments: failed,
        cancelled_instruments: row.try_get("cancelled_instruments").unwrap_or(0),
    }
}

pub async fn list_replay_batches(pool: &PgPool, limit: i64) -> Result<Vec<BatchSummary>, DbError> {
    let rows = sqlx::query(&format!(
        "{BATCH_SELECT} from replay_batches b join replay_runs r on r.batch_id = b.id \
         group by b.id order by b.id desc limit $1"
    ))
    .bind(limit as i32)
    .fetch_all(pool)
    .await?;
    Ok(rows.iter().map(batch_from_row).collect())
}

#[derive(Debug, Clone, PartialEq)]
pub struct RunIntervals {
    pub quality: Option<Value>,
    pub quality_source_run_id: Option<i64>,
    pub intervals: Vec<(i64, i64, String)>,
    pub total: i64,
    pub available: bool,
}

pub async fn get_run_intervals(
    pool: &PgPool,
    run_id: i64,
    offset: i64,
    limit: i64,
) -> Result<Option<RunIntervals>, DbError> {
    let mut tx = pool.begin().await?;
    sqlx::query("set transaction isolation level repeatable read")
        .execute(&mut *tx)
        .await?;
    let run = sqlx::query("select quality, quality_source_run_id from replay_runs where id = $1")
        .bind(run_id as i32)
        .fetch_optional(&mut *tx)
        .await?;
    let Some(run) = run else {
        tx.commit().await?;
        return Ok(None);
    };
    let quality: Option<Value> = run.try_get("quality")?;
    let quality_source_run_id: Option<i64> = run
        .try_get::<Option<i32>, _>("quality_source_run_id")?
        .map(i64::from);
    if quality.is_none() || quality_source_run_id.is_none() {
        tx.commit().await?;
        return Ok(Some(RunIntervals {
            quality,
            quality_source_run_id,
            intervals: Vec::new(),
            total: 0,
            available: false,
        }));
    }
    let count_row =
        sqlx::query("select count(*)::bigint as n from replay_quality_intervals where run_id = $1")
            .bind(quality_source_run_id)
            .fetch_one(&mut *tx)
            .await?;
    let total: i64 = count_row.try_get("n")?;
    let rows = sqlx::query(
        "select from_unix, to_unix, reason from replay_quality_intervals where run_id = $1 \
         order by from_unix offset $2 limit $3",
    )
    .bind(quality_source_run_id)
    .bind(offset)
    .bind(limit)
    .fetch_all(&mut *tx)
    .await?;
    let intervals = rows
        .iter()
        .map(|row| {
            let from: i64 = match row.try_get::<i64, _>("from_unix") {
                Ok(value) => value,
                Err(_) => i64::from(row.try_get::<i32, _>("from_unix").unwrap_or(0)),
            };
            let to: i64 = match row.try_get::<i64, _>("to_unix") {
                Ok(value) => value,
                Err(_) => i64::from(row.try_get::<i32, _>("to_unix").unwrap_or(0)),
            };
            let reason: String = row.try_get("reason").unwrap_or_default();
            (from, to, reason)
        })
        .collect();
    tx.commit().await?;
    Ok(Some(RunIntervals {
        quality,
        quality_source_run_id,
        intervals,
        total,
        available: true,
    }))
}

pub async fn cache_stats(pool: &PgPool) -> Result<(i64, i64), DbError> {
    let count_row = sqlx::query("select count(*)::bigint as n from replay_cache")
        .fetch_one(pool)
        .await?;
    let size_row = sqlx::query("select pg_total_relation_size('replay_cache')::bigint as size")
        .fetch_one(pool)
        .await?;
    Ok((count_row.try_get("n")?, size_row.try_get("size")?))
}

async fn advance_generation(tx: &mut sqlx::Transaction<'_, sqlx::Postgres>) -> Result<(), DbError> {
    let generation = uuid_string();
    let value = serde_json::json!({ "generation": generation });
    sqlx::query(
        "insert into settings (key, value, updated_at) values ('replay-cache-generation', $1, now()) \
         on conflict (key) do update set value = excluded.value, updated_at = now()",
    )
    .bind(&value)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

fn uuid_string() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 16];
    rand::rng().fill_bytes(&mut bytes);
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

pub async fn cache_clear_by_instrument(
    pool: &PgPool,
    exchange_code: &str,
    category: &str,
    symbol: &str,
) -> Result<u64, DbError> {
    let mut tx = pool.begin().await?;
    sqlx::query("select pg_advisory_xact_lock($1)")
        .bind(CACHE_LOCK_KEY)
        .execute(&mut *tx)
        .await?;
    advance_generation(&mut tx).await?;
    let removed = sqlx::query(
        "delete from replay_cache where exchange_code = $1 and category = $2 and symbol = $3",
    )
    .bind(exchange_code)
    .bind(category)
    .bind(symbol)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    tx.commit().await?;
    Ok(removed)
}

pub async fn cache_clear_all(pool: &PgPool) -> Result<u64, DbError> {
    let mut tx = pool.begin().await?;
    sqlx::query("select pg_advisory_xact_lock($1)")
        .bind(CACHE_LOCK_KEY)
        .execute(&mut *tx)
        .await?;
    advance_generation(&mut tx).await?;
    let removed = sqlx::query("delete from replay_cache")
        .execute(&mut *tx)
        .await?
        .rows_affected();
    tx.commit().await?;
    Ok(removed)
}

pub struct CacheCompletion {
    pub quality: Option<Value>,
    pub quality_source_run_id: Option<i64>,
    pub computed_at: DateTime<Utc>,
    pub setups: Value,
    pub warmup_minutes: i64,
    pub processed_minutes: i64,
}

pub async fn complete_run_from_cache(
    pool: &PgPool,
    run_id: i64,
    cached: &CacheCompletion,
) -> Result<(), DbError> {
    let mut tx = pool.begin().await?;
    let current = sqlx::query("select status from replay_runs where id = $1 for update")
        .bind(run_id as i32)
        .fetch_one(&mut *tx)
        .await?;
    let status: String = current.try_get("status")?;
    let new_status = if status == "cancel_requested" {
        "cancelled"
    } else {
        "done"
    };
    sqlx::query(
        "update replay_runs set status = $2, cache_hit = true, quality = $3, quality_source_run_id = $4, \
         quality_computed_at = $5, setups = $6, warmup_minutes = $7, processed_minutes = $8, updated_at = now() \
         where id = $1",
    )
    .bind(run_id as i32)
    .bind(new_status)
    .bind(&cached.quality)
    .bind(cached.quality_source_run_id.map(|id| id as i32))
    .bind(cached.computed_at)
    .bind(&cached.setups)
    .bind(cached.warmup_minutes as i32)
    .bind(cached.processed_minutes as i32)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn mark_run_instrument_not_found(
    pool: &PgPool,
    run_id: i64,
) -> Result<ReplayRunFull, DbError> {
    let row = sqlx::query(&format!(
        "update replay_runs set status = 'failed', error = 'instrument not found in market data catalog', updated_at = now() \
         where id = $1 returning {RUN_COLUMNS}, coalesce(jsonb_array_length(setups), 0) as setup_count"
    ))
    .bind(run_id as i32)
    .fetch_one(pool)
    .await?;
    run_full_from_row(&row)
}

pub async fn get_replay_params(pool: &PgPool) -> Result<SpikeParams, DbError> {
    let stored = crate::settings_get(pool, "algorithm:price_volume_spike").await?;
    let mut params = SpikeParams::default();
    if let Some(Value::Object(fields)) = stored
        && let Ok(merged) = serde_json::from_value::<SpikeParams>(Value::Object(fields))
    {
        params = merged;
    }
    Ok(params)
}

pub async fn get_replay_batch(
    pool: &PgPool,
    batch_id: i64,
) -> Result<Option<BatchSummary>, DbError> {
    let rows = sqlx::query(&format!(
        "{BATCH_SELECT} from replay_batches b join replay_runs r on r.batch_id = b.id \
         where b.id = $1 group by b.id"
    ))
    .bind(batch_id as i32)
    .fetch_optional(pool)
    .await?;
    Ok(rows.as_ref().map(batch_from_row))
}

const QUEUE_LOCK_KEY: i64 = 72_423_412;

pub async fn cancel_replay_batch(
    pool: &PgPool,
    batch_id: i64,
) -> Result<Option<(u64, u64)>, DbError> {
    let mut tx = pool.begin().await?;
    sqlx::query("select pg_advisory_xact_lock($1)")
        .bind(QUEUE_LOCK_KEY)
        .execute(&mut *tx)
        .await?;
    let batch = sqlx::query("select id from replay_batches where id = $1")
        .bind(batch_id as i32)
        .fetch_optional(&mut *tx)
        .await?;
    if batch.is_none() {
        tx.commit().await?;
        return Ok(None);
    }
    let cancelled = sqlx::query(
        "update replay_runs set status = 'cancelled', updated_at = now() \
         where batch_id = $1 and status = 'queued'",
    )
    .bind(batch_id as i32)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    let cancel_requested = sqlx::query(
        "update replay_runs set status = 'cancel_requested', updated_at = now() \
         where batch_id = $1 and status = 'running'",
    )
    .bind(batch_id as i32)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    tx.commit().await?;
    Ok(Some((cancelled, cancel_requested)))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchDeleteOutcome {
    Deleted {
        runs_deleted: u64,
        quality_intervals_deleted: u64,
    },
    Active,
}

pub async fn delete_replay_batch(
    pool: &PgPool,
    batch_id: i64,
) -> Result<Option<BatchDeleteOutcome>, DbError> {
    let mut tx = pool.begin().await?;
    sqlx::query("select pg_advisory_xact_lock($1)")
        .bind(QUEUE_LOCK_KEY)
        .execute(&mut *tx)
        .await?;
    let batch = sqlx::query("select id from replay_batches where id = $1")
        .bind(batch_id as i32)
        .fetch_optional(&mut *tx)
        .await?;
    if batch.is_none() {
        tx.commit().await?;
        return Ok(None);
    }
    let children = sqlx::query("select status from replay_runs where batch_id = $1")
        .bind(batch_id as i32)
        .fetch_all(&mut *tx)
        .await?;
    if children.iter().any(|row| {
        row.try_get::<String, _>("status")
            .is_ok_and(|status| status != "done" && status != "failed" && status != "cancelled")
    }) {
        tx.commit().await?;
        return Ok(Some(BatchDeleteOutcome::Active));
    }
    let quality_intervals_deleted = sqlx::query(
        "delete from replay_quality_intervals where run_id in \
         (select id from replay_runs where batch_id = $1)",
    )
    .bind(batch_id as i32)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    let runs_deleted = sqlx::query("delete from replay_runs where batch_id = $1")
        .bind(batch_id as i32)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    sqlx::query("delete from replay_batches where id = $1")
        .bind(batch_id as i32)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Some(BatchDeleteOutcome::Deleted {
        runs_deleted,
        quality_intervals_deleted,
    }))
}

#[derive(Debug, Clone, PartialEq)]
pub struct BatchSetupRow {
    pub setup: Value,
    pub exchange_code: String,
    pub category: String,
    pub symbol: String,
    pub algorithm_id: String,
    pub algorithm_version: Option<String>,
    pub params: Value,
}

fn batch_setup_from_row(row: &PgRow) -> BatchSetupRow {
    BatchSetupRow {
        setup: row.try_get("setup").unwrap_or(Value::Null),
        exchange_code: row.try_get("exchange_code").unwrap_or_default(),
        category: row.try_get("category").unwrap_or_default(),
        symbol: row.try_get("symbol").unwrap_or_default(),
        algorithm_id: row.try_get("algorithm_id").unwrap_or_default(),
        algorithm_version: row.try_get("algorithm_version").unwrap(),
        params: row.try_get("params").unwrap_or(Value::Null),
    }
}

pub async fn replay_batch_result_page(
    pool: &PgPool,
    batch_id: i64,
    offset: i64,
    limit: i64,
) -> Result<Option<(Vec<BatchSetupRow>, i64)>, DbError> {
    let batch = sqlx::query("select id from replay_batches where id = $1")
        .bind(batch_id as i32)
        .fetch_optional(pool)
        .await?;
    if batch.is_none() {
        return Ok(None);
    }
    let total_row = sqlx::query(
        "select coalesce(sum(case when jsonb_typeof(setups) = 'array' \
         then jsonb_array_length(setups) else 0 end), 0)::bigint as total \
         from replay_runs where batch_id = $1",
    )
    .bind(batch_id as i32)
    .fetch_one(pool)
    .await?;
    let total: i64 = total_row.try_get("total")?;
    let rows = sqlx::query(
        "select s.value as setup, r.exchange_code, r.category, r.symbol, r.algorithm_id, r.algorithm_version, r.params \
         from replay_runs r \
         cross join lateral jsonb_array_elements(case when jsonb_typeof(r.setups) = 'array' \
         then r.setups else '[]'::jsonb end) with ordinality as s(value, ordinality) \
         where r.batch_id = $1 \
         order by (s.value->>'closeTimeUnix')::bigint desc, r.id asc, s.ordinality asc \
         limit $2 offset $3",
    )
    .bind(batch_id as i32)
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await?;
    let setups = rows.iter().map(batch_setup_from_row).collect();
    Ok(Some((setups, total)))
}

#[derive(Debug, Clone, PartialEq)]
pub struct LiveSetupRow {
    pub id: i64,
    pub exchange_code: String,
    pub category: String,
    pub symbol: String,
    pub algorithm_id: String,
    pub algorithm_version: Option<String>,
    pub open_time: DateTime<Utc>,
    pub close_time: DateTime<Utc>,
    pub close_price: String,
}

pub async fn live_setups_snapshot(pool: &PgPool) -> Result<i64, DbError> {
    let row = sqlx::query("select coalesce(max(id), 0)::int as id from live_setups")
        .fetch_one(pool)
        .await?;
    Ok(i64::from(row.try_get::<i32, _>("id")?))
}

pub async fn live_setups_page(
    pool: &PgPool,
    snapshot_id: i64,
    after: Option<(DateTime<Utc>, i64)>,
    limit: i64,
) -> Result<Vec<LiveSetupRow>, DbError> {
    let query = if after.is_some() {
        "select id, exchange_code, category, symbol, algorithm_id, algorithm_version, open_time, close_time, close_price \
         from live_setups where id <= $1 and (close_time, id) > ($3, $4) \
         order by close_time asc, id asc limit $2"
    } else {
        "select id, exchange_code, category, symbol, algorithm_id, algorithm_version, open_time, close_time, close_price \
         from live_setups where id <= $1 \
         order by close_time asc, id asc limit $2"
    };
    let mut statement = sqlx::query(query)
        .bind(snapshot_id as i32)
        .bind(limit as i32);
    if let Some((close_time, id)) = after {
        statement = statement.bind(close_time).bind(id as i32);
    }
    let rows = statement.fetch_all(pool).await?;
    rows.iter()
        .map(|row| {
            Ok(LiveSetupRow {
                id: i64::from(row.try_get::<i32, _>("id")?),
                exchange_code: row.try_get("exchange_code")?,
                category: row.try_get("category")?,
                symbol: row.try_get("symbol")?,
                algorithm_id: row.try_get("algorithm_id")?,
                algorithm_version: row.try_get("algorithm_version")?,
                open_time: row.try_get("open_time")?,
                close_time: row.try_get("close_time")?,
                close_price: row.try_get("close_price")?,
            })
        })
        .collect()
}
