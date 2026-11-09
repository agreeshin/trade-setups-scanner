use crate::DbError;
use sqlx::Row;
use sqlx::postgres::PgPool;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstrumentListRow {
    pub list_type: String,
    pub exchange_code: String,
    pub category: String,
    pub symbol: String,
}

const LIST_COLUMNS: &str = "list_type, exchange_code, category, symbol";

fn row_from_row(row: &sqlx::postgres::PgRow) -> InstrumentListRow {
    InstrumentListRow {
        list_type: row.try_get("list_type").unwrap_or_default(),
        exchange_code: row.try_get("exchange_code").unwrap_or_default(),
        category: row.try_get("category").unwrap_or_default(),
        symbol: row.try_get("symbol").unwrap_or_default(),
    }
}

pub async fn list_instrument_lists(
    pool: &PgPool,
    limit: i64,
) -> Result<Vec<InstrumentListRow>, DbError> {
    let rows = sqlx::query(&format!(
        "select {LIST_COLUMNS} from instrument_lists order by id limit $1"
    ))
    .bind(limit as i32)
    .fetch_all(pool)
    .await?;
    Ok(rows.iter().map(row_from_row).collect())
}

pub async fn instrument_list_by_kind(
    pool: &PgPool,
    kind: &str,
) -> Result<Vec<InstrumentListRow>, DbError> {
    let rows = sqlx::query(&format!(
        "select {LIST_COLUMNS} from instrument_lists where list_type = $1 order by id"
    ))
    .bind(kind)
    .fetch_all(pool)
    .await?;
    Ok(rows.iter().map(row_from_row).collect())
}

pub async fn replace_instrument_list(
    pool: &PgPool,
    kind: &str,
    instruments: &[(String, String, String)],
) -> Result<(), DbError> {
    let mut tx = pool.begin().await?;
    sqlx::query("delete from instrument_lists where list_type = $1")
        .bind(kind)
        .execute(&mut *tx)
        .await?;
    for (exchange, category, symbol) in instruments {
        sqlx::query(
            "insert into instrument_lists (list_type, exchange_code, category, symbol) values ($1, $2, $3, $4)",
        )
        .bind(kind)
        .bind(exchange)
        .bind(category)
        .bind(symbol)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}
