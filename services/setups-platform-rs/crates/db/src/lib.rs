use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use setups_detector::quality::ReplayQuality;
use setups_domain::{SpikeParams, SpikeSetup};
use sqlx::Row;
use sqlx::postgres::{PgPool, PgPoolOptions};

pub mod admission;
pub mod runs;

#[derive(Debug)]
pub enum DbError {
    Sqlx(sqlx::Error),
    Json(String),
}

impl std::fmt::Display for DbError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DbError::Sqlx(error) => write!(f, "database error: {error}"),
            DbError::Json(message) => write!(f, "json decode error: {message}"),
        }
    }
}

impl std::error::Error for DbError {}

impl From<sqlx::Error> for DbError {
    fn from(error: sqlx::Error) -> Self {
        DbError::Sqlx(error)
    }
}

fn json_error(error: serde_json::Error) -> DbError {
    DbError::Json(error.to_string())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CacheQuery {
    pub exchange_code: String,
    pub category: String,
    pub symbol: String,
    pub algorithm_id: String,
    pub algorithm_version: String,
    pub params: SpikeParams,
    pub from_unix: i64,
    pub to_unix: i64,
}

pub fn ts_stringify(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Number(number) => number.to_string(),
        Value::Bool(flag) => flag.to_string(),
        Value::Null => "null".to_string(),
        Value::Array(items) => items.iter().map(ts_stringify).collect::<Vec<_>>().join(","),
        Value::Object(_) => "[object Object]".to_string(),
    }
}

pub fn canonical_params(params: &Value) -> String {
    let Some(object) = params.as_object() else {
        return String::new();
    };
    let mut keys: Vec<&String> = object.keys().collect();
    keys.sort();
    let mut parts: Vec<String> = Vec::with_capacity(keys.len());
    for key in keys {
        let value = &object[key];
        if let Value::Object(_) = value {
            parts.push(format!("{key}:{{{}}}", canonical_params(value)));
        } else {
            parts.push(format!("{key}:{}", ts_stringify(value)));
        }
    }
    parts.join(",")
}

pub fn canonical_cache_key(query: &CacheQuery) -> String {
    let params = serde_json::to_value(&query.params).expect("params serialize");
    format!(
        "{}@{}|{}/{}/{}|{}-{}|{}",
        query.algorithm_id,
        query.algorithm_version,
        query.exchange_code,
        query.category,
        query.symbol,
        query.from_unix,
        query.to_unix,
        canonical_params(&params)
    )
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CachedReplayResult {
    pub cache_key: String,
    pub quality: Option<ReplayQuality>,
    pub quality_source_run_id: Option<i64>,
    pub setups: Vec<SpikeSetup>,
    pub warmup_minutes: i64,
    pub processed_minutes: i64,
    pub computed_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FinishedReplay {
    pub quality: Option<ReplayQuality>,
    pub quality_source_run_id: Option<i64>,
    pub setups: Vec<SpikeSetup>,
    pub warmup_minutes: i64,
    pub processed_minutes: i64,
    pub computed_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ReplayRun {
    pub id: i64,
    pub batch_id: Option<i64>,
    pub exchange_code: String,
    pub category: String,
    pub symbol: String,
    pub from_ts: DateTime<Utc>,
    pub to_ts: DateTime<Utc>,
    pub algorithm_id: String,
    pub algorithm_version: String,
    pub params: SpikeParams,
    pub status: String,
    pub cache_hit: bool,
}

pub(crate) fn unix_to_ts(unix: i64) -> DateTime<Utc> {
    DateTime::from_timestamp(unix, 0).expect("unix seconds within range")
}

fn ts_to_unix(timestamp: DateTime<Utc>) -> i64 {
    timestamp.timestamp()
}

pub async fn connect(url: &str) -> Result<PgPool, DbError> {
    Ok(PgPoolOptions::new().connect(url).await?)
}

pub async fn cache_lookup(
    pool: &PgPool,
    query: &CacheQuery,
) -> Result<Option<CachedReplayResult>, DbError> {
    let key = canonical_cache_key(query);
    let row = sqlx::query(
        "select cache_key, quality, quality_source_run_id, setups, warmup_minutes, processed_minutes, computed_at \
         from replay_cache where cache_key = $1",
    )
    .bind(&key)
    .fetch_optional(pool)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let quality: Option<Value> = row.try_get("quality")?;
    let setups: Value = row.try_get("setups")?;
    Ok(Some(CachedReplayResult {
        cache_key: row.try_get("cache_key")?,
        quality: quality
            .map(serde_json::from_value)
            .transpose()
            .map_err(json_error)?,
        quality_source_run_id: row
            .try_get::<Option<i32>, _>("quality_source_run_id")?
            .map(i64::from),
        setups: serde_json::from_value(setups).map_err(json_error)?,
        warmup_minutes: i64::from(row.try_get::<i32, _>("warmup_minutes")?),
        processed_minutes: i64::from(row.try_get::<i32, _>("processed_minutes")?),
        computed_at: row.try_get("computed_at")?,
    }))
}

pub async fn cache_store(
    pool: &PgPool,
    query: &CacheQuery,
    result: &FinishedReplay,
) -> Result<(), DbError> {
    let key = canonical_cache_key(query);
    let params = serde_json::to_value(&query.params).map_err(json_error)?;
    let setups = serde_json::to_value(&result.setups).map_err(json_error)?;
    let quality = match &result.quality {
        Some(quality) => Some(serde_json::to_value(quality).map_err(json_error)?),
        None => None,
    };
    sqlx::query(
        "insert into replay_cache \
         (cache_key, exchange_code, category, symbol, algorithm_id, algorithm_version, params, from_ts, to_ts, setups, warmup_minutes, processed_minutes, quality, quality_source_run_id, computed_at) \
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15) \
         on conflict (cache_key) do update set \
         setups = excluded.setups, warmup_minutes = excluded.warmup_minutes, \
         processed_minutes = excluded.processed_minutes, quality = excluded.quality, \
         quality_source_run_id = excluded.quality_source_run_id, computed_at = excluded.computed_at",
    )
    .bind(&key)
    .bind(&query.exchange_code)
    .bind(&query.category)
    .bind(&query.symbol)
    .bind(&query.algorithm_id)
    .bind(&query.algorithm_version)
    .bind(&params)
    .bind(unix_to_ts(query.from_unix))
    .bind(unix_to_ts(query.to_unix))
    .bind(&setups)
    .bind(result.warmup_minutes as i32)
    .bind(result.processed_minutes as i32)
    .bind(&quality)
    .bind(result.quality_source_run_id.map(|value| value as i32))
    .bind(result.computed_at.unwrap_or_else(Utc::now))
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn settings_get(pool: &PgPool, key: &str) -> Result<Option<Value>, DbError> {
    let row = sqlx::query("select value from settings where key = $1")
        .bind(key)
        .fetch_optional(pool)
        .await?;
    row.map(|row| row.try_get("value"))
        .transpose()
        .map_err(DbError::Sqlx)
}

pub async fn settings_set(pool: &PgPool, key: &str, value: &Value) -> Result<(), DbError> {
    sqlx::query(
        "insert into settings (key, value, updated_at) values ($1, $2, now()) \
         on conflict (key) do update set value = excluded.value, updated_at = now()",
    )
    .bind(key)
    .bind(value)
    .execute(pool)
    .await?;
    Ok(())
}

pub(crate) fn replay_run_from_row(row: &sqlx::postgres::PgRow) -> Result<ReplayRun, DbError> {
    let params: Value = row.try_get("params")?;
    Ok(ReplayRun {
        id: i64::from(row.try_get::<i32, _>("id")?),
        batch_id: row.try_get::<Option<i32>, _>("batch_id")?.map(i64::from),
        exchange_code: row.try_get("exchange_code")?,
        category: row.try_get("category")?,
        symbol: row.try_get("symbol")?,
        from_ts: row.try_get("from_ts")?,
        to_ts: row.try_get("to_ts")?,
        algorithm_id: row.try_get("algorithm_id")?,
        algorithm_version: row.try_get("algorithm_version")?,
        params: serde_json::from_value(params).map_err(json_error)?,
        status: row.try_get("status")?,
        cache_hit: row.try_get("cache_hit")?,
    })
}

pub async fn claim_next_queued_run(pool: &PgPool) -> Result<Option<ReplayRun>, DbError> {
    let mut tx = pool.begin().await?;
    let row = sqlx::query(
        "select id, batch_id, exchange_code, category, symbol, from_ts, to_ts, algorithm_id, algorithm_version, params, status, cache_hit \
         from replay_runs where status = 'queued' order by id for update skip locked limit 1",
    )
    .fetch_optional(&mut *tx)
    .await?;
    let Some(row) = row else {
        tx.commit().await?;
        return Ok(None);
    };
    let run = replay_run_from_row(&row)?;
    sqlx::query("update replay_runs set status = 'running', updated_at = now() where id = $1")
        .bind(run.id as i32)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Some(run))
}

pub async fn fail_interrupted_runs(pool: &PgPool) -> Result<u64, DbError> {
    let result = sqlx::query(
        "update replay_runs set status = 'failed', error = 'interrupted by application restart', updated_at = now() \
         where status = 'running'",
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

pub fn run_query(run: &ReplayRun) -> CacheQuery {
    CacheQuery {
        exchange_code: run.exchange_code.clone(),
        category: run.category.clone(),
        symbol: run.symbol.clone(),
        algorithm_id: run.algorithm_id.clone(),
        algorithm_version: run.algorithm_version.clone(),
        params: run.params.clone(),
        from_unix: ts_to_unix(run.from_ts),
        to_unix: ts_to_unix(run.to_ts),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_params_sorts_keys_and_matches_ts_format() {
        let value: Value =
            serde_json::from_str(r#"{"b":"x","a":30,"c":null,"d":true,"nested":{"z":1,"y":2}}"#)
                .unwrap();
        assert_eq!(
            canonical_params(&value),
            "a:30,b:x,c:null,d:true,nested:{y:2,z:1}"
        );
    }

    #[test]
    fn canonical_cache_key_composes_expected_shape() {
        let query = CacheQuery {
            exchange_code: "bybit".into(),
            category: "linear".into(),
            symbol: "BTCUSDT".into(),
            algorithm_id: "price_volume_spike".into(),
            algorithm_version: "1.1.1".into(),
            params: SpikeParams::default(),
            from_unix: 1_699_920_000,
            to_unix: 1_700_006_400,
        };
        assert_eq!(
            canonical_cache_key(&query),
            "price_volume_spike@1.1.1|bybit/linear/BTCUSDT|1699920000-1700006400|baselineLongDays:30,baselineShortDays:7,cooldownMinutes:5,maxConsecutiveGapMinutes:5,maxGapPercent:1,priceGrowthPct:12,requiredRecentMinutes:10,volumeMultiplier:3,windowMinutes:5"
        );
    }
}
