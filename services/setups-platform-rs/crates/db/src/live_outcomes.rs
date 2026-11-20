//! Integral live_setup outcome writes. No cache identity or recommendation matching.
use crate::{DbError, PgPool};
use serde_json::Value;

/// Returns the durable winner, including when a concurrent calculation already won.
/// No transaction or row lock is held while callers read candles or calculate.
pub async fn save(
    pool: &PgPool,
    owner: &str,
    setup_id: i64,
    outcome: &Value,
    refresh: bool,
    previous: Option<&Value>,
) -> Result<(Value, bool), DbError> {
    let observed = outcome["observedThroughUnix"]
        .as_i64()
        .ok_or_else(|| DbError::Json("live_outcome_observation_missing".into()))?;
    let evaluated = outcome["evaluatedAtUnix"]
        .as_i64()
        .ok_or_else(|| DbError::Json("live_outcome_timestamp_missing".into()))?;
    let updated: Option<Value> = sqlx::query_scalar(
        "update live_setups l set trade_outcome=$3 where l.id=$1 \
         and exists(select 1 from adaptive_profiles p where p.id=l.adaptive_slot_id and p.owner_id=$2) \
         and l.trade_outcome is not distinct from $7::jsonb \
         and (l.trade_outcome is null or ($4 and l.trade_outcome->>'status' in ('unclosed','no_data') \
           and (l.trade_outcome->>'observedThroughUnix')::bigint <= $5 \
           and (l.trade_outcome->>'evaluatedAtUnix')::bigint <= $6 \
           and not (l.trade_outcome->>'status'='unclosed' and $3->>'status'='no_data' \
             and (l.trade_outcome->>'observedThroughUnix')::bigint=$5))) returning l.trade_outcome")
        .bind(setup_id as i32).bind(owner).bind(outcome).bind(refresh).bind(observed).bind(evaluated).bind(previous)
        .fetch_optional(pool).await?;
    if let Some(value) = updated {
        return Ok((value, true));
    }
    let winner: Option<Value> = sqlx::query_scalar(
        "select trade_outcome from live_setups l where l.id=$1 and trade_outcome is not null \
         and exists(select 1 from adaptive_profiles p where p.id=l.adaptive_slot_id and p.owner_id=$2)")
        .bind(setup_id as i32).bind(owner).fetch_optional(pool).await?;
    winner
        .map(|value| (value, false))
        .ok_or_else(|| DbError::Json("live_outcome_setup_unavailable".into()))
}
