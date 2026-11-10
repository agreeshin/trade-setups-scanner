use crate::runs::replay_batch_result_page;
use crate::{DbError, PgPool};
use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::Row;

pub const ANALYTICS_CACHE_ALGORITHM_VERSION: i32 = 1;

pub type CachedPayload = Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetKind {
    Batch,
    Run,
}

impl SetKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            SetKind::Batch => "batch",
            SetKind::Run => "run",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetRef {
    pub kind: SetKind,
    pub id: i64,
}

pub fn parse_set_id(value: &str) -> Option<SetRef> {
    let (kind, digits) = if let Some(rest) = value.strip_prefix("batch-") {
        (SetKind::Batch, rest)
    } else {
        let rest = value.strip_prefix("run-")?;
        (SetKind::Run, rest)
    };
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    if digits.starts_with('0') {
        return None;
    }
    let id: i64 = digits.parse().ok()?;
    if id < 1 {
        return None;
    }
    Some(SetRef { kind, id })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetScope {
    pub set_kind: String,
    pub set_id: i64,
    pub setup_count: i64,
    pub first_setup_unix: Option<i64>,
    pub last_setup_unix: Option<i64>,
    pub last_setup_last_candle_open_time_unix: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ComputeParams {
    pub direction: String,
    pub tp_pct: f64,
    pub sl_pct: f64,
    pub horizon_minutes: i64,
    pub time_shift_minutes: i64,
    pub price_change_ppm: i64,
    pub maturity_tenths: i64,
    pub cluster_gap_minutes: i64,
}

pub fn price_change_ppm_of(pct: Option<f64>) -> i64 {
    match pct {
        Some(value) => (value * 10_000.0).round() as i64,
        None => -1,
    }
}

pub fn maturity_tenths_of(maturity: Option<f64>) -> i64 {
    match maturity {
        Some(value) => (value * 10.0).round() as i64,
        None => -1,
    }
}

pub fn format_pct(value: f64) -> String {
    format!("{value}")
}

pub fn matrix_scope_key(scope: &SetScope) -> String {
    format!(
        "{}:{}:{}:{}:{}:{}",
        scope.set_kind,
        scope.set_id,
        scope.setup_count,
        scope.first_setup_unix.unwrap_or(-1),
        scope.last_setup_unix.unwrap_or(-1),
        scope.last_setup_last_candle_open_time_unix
    )
}

pub fn selection_scope_key(
    set_kind: &str,
    set_id: i64,
    setup_count: i64,
    first_setup_unix: Option<i64>,
    last_setup_unix: Option<i64>,
) -> String {
    format!(
        "{set_kind}:{set_id}:{setup_count}:{}:{}",
        first_setup_unix.unwrap_or(-1),
        last_setup_unix.unwrap_or(-1)
    )
}

fn matrix_cell_where<'a>(
    query: sqlx::query::Query<'a, sqlx::Postgres, sqlx::postgres::PgArguments>,
    scope_key: &'a str,
    params: &'a ComputeParams,
    month_key: &'a str,
) -> sqlx::query::Query<'a, sqlx::Postgres, sqlx::postgres::PgArguments> {
    query
        .bind(scope_key)
        .bind(&params.direction)
        .bind(params.maturity_tenths)
        .bind(params.cluster_gap_minutes)
        .bind(month_key)
        .bind(params.tp_pct)
        .bind(params.sl_pct)
        .bind(params.horizon_minutes)
        .bind(params.time_shift_minutes)
        .bind(params.price_change_ppm)
        .bind(ANALYTICS_CACHE_ALGORITHM_VERSION)
}

pub async fn get_matrix_cell(
    pool: &PgPool,
    scope_key: &str,
    params: &ComputeParams,
    month_key: &str,
) -> Result<Option<CachedPayload>, DbError> {
    let row = matrix_cell_where(
        sqlx::query(
            "select result_json from analytics_matrix_cache where scope_key = $1 \
             and direction = $2 and maturity_tenths = $3 and cluster_gap_minutes = $4 \
             and month_key = $5 and tp_pct = $6 and sl_pct = $7 and horizon_minutes = $8 \
             and time_shift_minutes = $9 and price_change_ppm = $10 and algorithm_version = $11",
        ),
        scope_key,
        params,
        month_key,
    )
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|row| row.try_get("result_json")).transpose()?)
}

pub async fn save_matrix_cell(
    pool: &PgPool,
    scope_key: &str,
    params: &ComputeParams,
    month_key: &str,
    result: &CachedPayload,
) -> Result<(), DbError> {
    sqlx::query(
        "insert into analytics_matrix_cache (scope_key, direction, maturity_tenths, cluster_gap_minutes, \
         month_key, tp_pct, sl_pct, horizon_minutes, time_shift_minutes, price_change_ppm, \
         algorithm_version, result_json) values ($1, $2, $3, $4, $5, $6, $7, \
         $8, $9, $10, $11, $12) \
         on conflict (scope_key, direction, maturity_tenths, cluster_gap_minutes, month_key, tp_pct, \
         sl_pct, horizon_minutes, time_shift_minutes, price_change_ppm, algorithm_version) \
         do update set result_json = excluded.result_json, created_at = now()",
    )
    .bind(scope_key)
    .bind(&params.direction)
    .bind(params.maturity_tenths)
    .bind(params.cluster_gap_minutes)
    .bind(month_key)
    .bind(params.tp_pct)
    .bind(params.sl_pct)
    .bind(params.horizon_minutes)
    .bind(params.time_shift_minutes)
    .bind(params.price_change_ppm)
    .bind(ANALYTICS_CACHE_ALGORITHM_VERSION)
    .bind(result)
    .execute(pool)
    .await?;
    Ok(())
}

pub struct MatrixCellRow {
    pub tp_pct: f64,
    pub sl_pct: f64,
    pub payload: CachedPayload,
}

pub async fn list_matrix_cells(
    pool: &PgPool,
    scope_key: &str,
    params: &ComputeParams,
    month_key: &str,
) -> Result<Vec<MatrixCellRow>, DbError> {
    let rows = sqlx::query(
        "select tp_pct, sl_pct, result_json from analytics_matrix_cache where scope_key = $1 \
         and direction = $2 and maturity_tenths = $3 and cluster_gap_minutes = $4 \
         and month_key = $5 and horizon_minutes = $6 and time_shift_minutes = $7 \
         and price_change_ppm = $8 and algorithm_version = $9",
    )
    .bind(scope_key)
    .bind(&params.direction)
    .bind(params.maturity_tenths)
    .bind(params.cluster_gap_minutes)
    .bind(month_key)
    .bind(params.horizon_minutes)
    .bind(params.time_shift_minutes)
    .bind(params.price_change_ppm)
    .bind(ANALYTICS_CACHE_ALGORITHM_VERSION)
    .fetch_all(pool)
    .await?;
    rows.iter()
        .map(|row| {
            Ok(MatrixCellRow {
                tp_pct: row
                    .try_get::<String, _>("tp_pct")?
                    .parse()
                    .unwrap_or_default(),
                sl_pct: row
                    .try_get::<String, _>("sl_pct")?
                    .parse()
                    .unwrap_or_default(),
                payload: row.try_get("result_json")?,
            })
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TradeSetupIdentity {
    pub exchange_code: String,
    pub category: String,
    pub symbol: String,
    pub close_time_unix: i64,
}

pub async fn get_trade_row(
    pool: &PgPool,
    scope_key: &str,
    params: &ComputeParams,
    identity: &TradeSetupIdentity,
) -> Result<Option<CachedPayload>, DbError> {
    let row = sqlx::query(
        "select result_json from analytics_trades_cache where scope_key = $1 \
         and direction = $2 and maturity_tenths = $3 and cluster_gap_minutes = $4 \
         and exchange_code = $5 and category = $6 and symbol = $7 and close_time_unix = $8 \
         and tp_pct = $9 and sl_pct = $10 and horizon_minutes = $11 \
         and time_shift_minutes = $12 and price_change_ppm = $13 and algorithm_version = $14",
    )
    .bind(scope_key)
    .bind(&params.direction)
    .bind(params.maturity_tenths)
    .bind(params.cluster_gap_minutes)
    .bind(&identity.exchange_code)
    .bind(&identity.category)
    .bind(&identity.symbol)
    .bind(identity.close_time_unix)
    .bind(params.tp_pct)
    .bind(params.sl_pct)
    .bind(params.horizon_minutes)
    .bind(params.time_shift_minutes)
    .bind(params.price_change_ppm)
    .bind(ANALYTICS_CACHE_ALGORITHM_VERSION)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|row| row.try_get("result_json")).transpose()?)
}

pub async fn save_trade_row(
    pool: &PgPool,
    scope_key: &str,
    params: &ComputeParams,
    identity: &TradeSetupIdentity,
    result: &CachedPayload,
) -> Result<(), DbError> {
    sqlx::query(
        "insert into analytics_trades_cache (scope_key, direction, maturity_tenths, cluster_gap_minutes, \
         exchange_code, category, symbol, close_time_unix, tp_pct, sl_pct, horizon_minutes, \
         time_shift_minutes, price_change_ppm, algorithm_version, result_json) \
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15) \
         on conflict (scope_key, direction, maturity_tenths, cluster_gap_minutes, exchange_code, \
         category, symbol, close_time_unix, tp_pct, sl_pct, horizon_minutes, time_shift_minutes, \
         price_change_ppm, algorithm_version) do update set result_json = excluded.result_json, created_at = now()",
    )
    .bind(scope_key)
    .bind(&params.direction)
    .bind(params.maturity_tenths)
    .bind(params.cluster_gap_minutes)
    .bind(&identity.exchange_code)
    .bind(&identity.category)
    .bind(&identity.symbol)
    .bind(identity.close_time_unix)
    .bind(params.tp_pct)
    .bind(params.sl_pct)
    .bind(params.horizon_minutes)
    .bind(params.time_shift_minutes)
    .bind(params.price_change_ppm)
    .bind(ANALYTICS_CACHE_ALGORITHM_VERSION)
    .bind(result)
    .execute(pool)
    .await?;
    Ok(())
}

pub struct TradeCacheRow {
    pub identity: TradeSetupIdentity,
    pub payload: CachedPayload,
}

pub async fn list_trade_rows(
    pool: &PgPool,
    scope_key: &str,
    params: &ComputeParams,
) -> Result<Vec<TradeCacheRow>, DbError> {
    let rows = sqlx::query(
        "select exchange_code, category, symbol, close_time_unix, result_json \
         from analytics_trades_cache where scope_key = $1 and direction = $2 \
         and maturity_tenths = $3 and cluster_gap_minutes = $4 and tp_pct = $5 \
         and sl_pct = $6 and horizon_minutes = $7 and time_shift_minutes = $8 \
         and price_change_ppm = $9 and algorithm_version = $10 order by close_time_unix asc",
    )
    .bind(scope_key)
    .bind(&params.direction)
    .bind(params.maturity_tenths)
    .bind(params.cluster_gap_minutes)
    .bind(params.tp_pct)
    .bind(params.sl_pct)
    .bind(params.horizon_minutes)
    .bind(params.time_shift_minutes)
    .bind(params.price_change_ppm)
    .bind(ANALYTICS_CACHE_ALGORITHM_VERSION)
    .fetch_all(pool)
    .await?;
    rows.iter()
        .map(|row| {
            Ok(TradeCacheRow {
                identity: TradeSetupIdentity {
                    exchange_code: row.try_get("exchange_code")?,
                    category: row.try_get("category")?,
                    symbol: row.try_get("symbol")?,
                    close_time_unix: row
                        .try_get::<String, _>("close_time_unix")?
                        .parse()
                        .unwrap_or_default(),
                },
                payload: row.try_get("result_json")?,
            })
        })
        .collect()
}

pub async fn get_selection(
    pool: &PgPool,
    scope_key: &str,
    maturity_tenths: i64,
    cluster_gap_minutes: i64,
    month_key: &str,
) -> Result<Option<CachedPayload>, DbError> {
    let row = sqlx::query(
        "select result_json from analytics_selection_cache where scope_key = $1 \
         and maturity_tenths = $2 and cluster_gap_minutes = $3 and month_key = $4 \
         and algorithm_version = $5",
    )
    .bind(scope_key)
    .bind(maturity_tenths)
    .bind(cluster_gap_minutes)
    .bind(month_key)
    .bind(ANALYTICS_CACHE_ALGORITHM_VERSION)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|row| row.try_get("result_json")).transpose()?)
}

pub async fn save_selection(
    pool: &PgPool,
    scope_key: &str,
    maturity_tenths: i64,
    cluster_gap_minutes: i64,
    month_key: &str,
    result: &CachedPayload,
) -> Result<(), DbError> {
    sqlx::query(
        "insert into analytics_selection_cache (scope_key, maturity_tenths, cluster_gap_minutes, \
         month_key, algorithm_version, result_json) values ($1, $2, $3, $4, $5, $6) \
         on conflict (scope_key, maturity_tenths, cluster_gap_minutes, month_key, algorithm_version) \
         do update set result_json = excluded.result_json, created_at = now()",
    )
    .bind(scope_key)
    .bind(maturity_tenths)
    .bind(cluster_gap_minutes)
    .bind(month_key)
    .bind(ANALYTICS_CACHE_ALGORITHM_VERSION)
    .bind(result)
    .execute(pool)
    .await?;
    Ok(())
}

pub struct CacheTableCount {
    pub name: &'static str,
    pub count: i64,
}

pub async fn analytics_cache_summary(pool: &PgPool) -> Result<Vec<CacheTableCount>, DbError> {
    let names = [
        "analytics_matrix_cache",
        "analytics_trades_cache",
        "analytics_selection_cache",
    ];
    let mut tables = Vec::new();
    for name in names {
        let row = sqlx::query(&format!("select count(*)::int as count from {name}"))
            .fetch_one(pool)
            .await?;
        tables.push(CacheTableCount {
            name,
            count: i64::from(row.try_get::<i32, _>("count")?),
        });
    }
    Ok(tables)
}

pub async fn clear_analytics_cache(pool: &PgPool) -> Result<Vec<CacheTableCount>, DbError> {
    let names = [
        "analytics_matrix_cache",
        "analytics_trades_cache",
        "analytics_selection_cache",
    ];
    let mut deleted = Vec::new();
    for name in names {
        let result = sqlx::query(&format!("delete from {name}"))
            .execute(pool)
            .await?;
        deleted.push(CacheTableCount {
            name,
            count: result.rows_affected() as i64,
        });
    }
    Ok(deleted)
}

#[derive(Debug, Clone, PartialEq)]
pub struct AnalyticsSetupRow {
    pub exchange_code: String,
    pub category: String,
    pub symbol: String,
    pub open_time_unix: i64,
    pub close_time_unix: i64,
    pub close_price: String,
}

pub async fn load_analytics_setups(
    pool: &PgPool,
    set_ref: &SetRef,
) -> Result<Option<Vec<AnalyticsSetupRow>>, DbError> {
    match set_ref.kind {
        SetKind::Run => {
            let row = sqlx::query(
                "select setups, exchange_code, category, symbol from replay_runs where id = $1",
            )
            .bind(set_ref.id as i32)
            .fetch_optional(pool)
            .await?;
            let Some(row) = row else {
                return Ok(None);
            };
            let setups: Value = row.try_get("setups")?;
            let Some(items) = setups.as_array() else {
                return Ok(None);
            };
            let exchange: String = row.try_get("exchange_code")?;
            let category: String = row.try_get("category")?;
            let symbol: String = row.try_get("symbol")?;
            let result: Vec<AnalyticsSetupRow> = items
                .iter()
                .map(|setup| AnalyticsSetupRow {
                    exchange_code: exchange.clone(),
                    category: category.clone(),
                    symbol: symbol.clone(),
                    open_time_unix: setup
                        .get("openTimeUnix")
                        .and_then(Value::as_i64)
                        .unwrap_or_default(),
                    close_time_unix: setup
                        .get("closeTimeUnix")
                        .and_then(Value::as_i64)
                        .unwrap_or_default(),
                    close_price: setup
                        .get("closePrice")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                })
                .collect();
            Ok(Some(result))
        }
        SetKind::Batch => {
            let first = replay_batch_result_page(pool, set_ref.id, 0, 1).await?;
            let Some((first_rows, total)) = first else {
                return Ok(None);
            };
            let mut rows = first_rows;
            let mut offset = rows.len() as i64;
            while offset < total {
                match replay_batch_result_page(pool, set_ref.id, offset, (total - offset).min(500))
                    .await?
                {
                    Some((page, _)) => {
                        offset += page.len() as i64;
                        rows.extend(page);
                    }
                    None => return Ok(None),
                }
            }
            let mut setups: Vec<AnalyticsSetupRow> = Vec::new();
            let mut seen = std::collections::HashSet::new();
            for row in rows {
                let identity = format!(
                    "{}\0{}\0{}\0{}",
                    row.exchange_code,
                    row.category,
                    row.symbol,
                    row.setup
                        .get("closeTimeUnix")
                        .and_then(Value::as_i64)
                        .unwrap_or_default()
                );
                if !seen.insert(identity) {
                    continue;
                }
                setups.push(AnalyticsSetupRow {
                    exchange_code: row.exchange_code,
                    category: row.category,
                    symbol: row.symbol,
                    open_time_unix: row
                        .setup
                        .get("openTimeUnix")
                        .and_then(Value::as_i64)
                        .unwrap_or_default(),
                    close_time_unix: row
                        .setup
                        .get("closeTimeUnix")
                        .and_then(Value::as_i64)
                        .unwrap_or_default(),
                    close_price: row
                        .setup
                        .get("closePrice")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                });
            }
            setups.sort_by_key(|setup| setup.close_time_unix);
            Ok(Some(setups))
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct AnalyticsSetInfo {
    pub set_id: String,
    pub kind: SetKind,
    pub numeric_id: i64,
    pub label: String,
    pub algorithm_id: String,
    pub algorithm_version: Option<String>,
    pub params: Value,
    pub exchange: String,
    pub category: String,
    pub symbol: Option<String>,
    pub setup_count: i64,
    pub from_unix: Option<i64>,
    pub to_unix: Option<i64>,
    pub created_at: String,
}

pub async fn list_analytics_sets(pool: &PgPool) -> Result<Vec<AnalyticsSetInfo>, DbError> {
    let batches = crate::runs::list_replay_batches(pool, 200).await?;
    let standalone = sqlx::query(
        "select id, name, exchange_code, category, symbol, algorithm_id, algorithm_version, params, \
         from_ts, to_ts, created_at, setups from replay_runs where batch_id is null \
         and status = 'done' order by id desc limit 200",
    )
    .fetch_all(pool)
    .await?;
    let mut sets: Vec<AnalyticsSetInfo> = Vec::new();
    for batch in batches.iter().filter(|batch| batch.status == "done") {
        sets.push(AnalyticsSetInfo {
            set_id: format!("batch-{}", batch.id),
            kind: SetKind::Batch,
            numeric_id: batch.id,
            label: format!("#{} · {}", batch.id, batch.name),
            algorithm_id: batch.algorithm_id.clone(),
            algorithm_version: batch.algorithm_version.clone(),
            params: batch.params.clone(),
            exchange: batch.exchange_code.clone(),
            category: batch.category.clone(),
            symbol: None,
            setup_count: batch.setup_count,
            from_unix: Some(batch.from_ts.timestamp()),
            to_unix: Some(batch.to_ts.timestamp()),
            created_at: batch
                .created_at
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        });
    }
    for row in &standalone {
        let setups: Value = row.try_get("setups")?;
        let setup_count = setups.as_array().map_or(0, Vec::len) as i64;
        let id: i32 = row.try_get("id")?;
        let algorithm_version: Option<String> = row.try_get("algorithm_version")?;
        let from_ts: Option<DateTime<Utc>> = row.try_get("from_ts")?;
        let to_ts: Option<DateTime<Utc>> = row.try_get("to_ts")?;
        let created_at: DateTime<Utc> = row.try_get("created_at")?;
        let algorithm_id: String = row.try_get("algorithm_id")?;
        let name: Option<String> = row.try_get("name")?;
        let run_name = crate::replay_name(name.as_deref(), &algorithm_id);
        sets.push(AnalyticsSetInfo {
            set_id: format!("run-{id}"),
            kind: SetKind::Run,
            numeric_id: i64::from(id),
            label: format!("#{id} · {run_name}"),
            algorithm_id,
            algorithm_version,
            params: row.try_get("params")?,
            exchange: row.try_get("exchange_code")?,
            category: row.try_get("category")?,
            symbol: Some(row.try_get("symbol")?),
            setup_count,
            from_unix: from_ts.map(|value| value.timestamp()),
            to_unix: to_ts.map(|value| value.timestamp()),
            created_at: created_at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        });
    }
    sets.sort_by(|a, b| {
        b.created_at
            .cmp(&a.created_at)
            .then_with(|| a.set_id.cmp(&b.set_id))
    });
    Ok(sets)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_set_id_matches_ts_pattern() {
        assert_eq!(
            parse_set_id("batch-12"),
            Some(SetRef {
                kind: SetKind::Batch,
                id: 12
            })
        );
        assert_eq!(
            parse_set_id("run-1"),
            Some(SetRef {
                kind: SetKind::Run,
                id: 1
            })
        );
        assert_eq!(parse_set_id("run-0"), None);
        assert_eq!(parse_set_id("run-01"), None);
        assert_eq!(parse_set_id("batch-"), None);
        assert_eq!(parse_set_id("plan-1"), None);
        assert_eq!(parse_set_id("run-1x"), None);
    }

    #[test]
    fn ppm_and_tenths_use_sentinels() {
        assert_eq!(price_change_ppm_of(None), -1);
        assert_eq!(price_change_ppm_of(Some(0.5)), 5_000);
        assert_eq!(maturity_tenths_of(None), -1);
        assert_eq!(maturity_tenths_of(Some(0.7)), 7);
    }

    #[test]
    fn scope_keys_join_like_ts() {
        let scope = SetScope {
            set_kind: "batch".into(),
            set_id: 4,
            setup_count: 12,
            first_setup_unix: Some(100),
            last_setup_unix: None,
            last_setup_last_candle_open_time_unix: -1,
        };
        assert_eq!(matrix_scope_key(&scope), "batch:4:12:100:-1:-1");
        assert_eq!(
            selection_scope_key("batch", 4, 12, Some(100), None),
            "batch:4:12:100:-1"
        );
    }
}
