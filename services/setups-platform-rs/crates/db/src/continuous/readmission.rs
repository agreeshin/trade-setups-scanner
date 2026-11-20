//! Explicit authenticated-owner grants; never a research retry or direct activation.
use super::{live::MAX_INITIALIZATION_ATTEMPTS, owner_lock};
use crate::DbError;
use crate::diagnostics::{self, Level};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{PgPool, Row};

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LiveReadmissionRequest {
    pub expected_generation_id: i64,
    pub expected_config_revision: i64,
    pub request_id: String,
}
impl LiveReadmissionRequest {
    pub fn validate(&self) -> Result<(), DbError> {
        if self.expected_generation_id <= 0
            || self.expected_config_revision <= 0
            || !(16..=128).contains(&self.request_id.len())
            || !self
                .request_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            return Err(DbError::Json(
                "invalid live readmission identity/idempotency key".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveReadmissionReceipt {
    pub grant_id: i64,
    pub request_id: String,
    pub slot_id: i64,
    pub generation_id: i64,
    pub config_revision: i64,
    pub attempts_before: i32,
    pub limit_before: i32,
    pub attempt_limit: i32,
    pub granted_at: DateTime<Utc>,
    /// A replay acknowledges the original grant, not current admission/activation.
    pub duplicate: bool,
}
#[derive(Debug)]
pub enum LiveReadmissionOutcome {
    Granted(LiveReadmissionReceipt),
    NotFound,
    Conflict(&'static str),
}
fn receipt(
    row: &sqlx::postgres::PgRow,
    duplicate: bool,
) -> Result<LiveReadmissionReceipt, DbError> {
    Ok(LiveReadmissionReceipt {
        grant_id: row.try_get("id")?,
        request_id: row.try_get("request_id")?,
        slot_id: row.try_get("slot_id")?,
        generation_id: row.try_get("generation_id")?,
        config_revision: row.try_get("config_revision")?,
        attempts_before: row.try_get("attempts_before")?,
        limit_before: row.try_get("limit_before")?,
        attempt_limit: row.try_get("attempt_limit")?,
        granted_at: row.try_get("granted_at")?,
        duplicate,
    })
}

/// The owner is supplied by the authenticated caller, never by request JSON.
/// Repeating a receipt key after exhaustion/Stop returns evidence, not a new transition.
pub async fn grant(
    pool: &PgPool,
    owner: &str,
    algorithm: &str,
    slot: i64,
    request: &LiveReadmissionRequest,
) -> Result<LiveReadmissionOutcome, DbError> {
    let started = std::time::Instant::now();
    let mut fields = readmission_diagnostics(algorithm, slot, request);
    diagnostics::emit(Level::Info, "live_readmission_started", fields.clone());
    let result = grant_inner(pool, owner, algorithm, slot, request, &mut fields).await;
    let level = readmission_outcome_diagnostics(&mut fields, &result);
    fields["duration_ms"] = (started.elapsed().as_millis() as u64).into();
    diagnostics::emit(level, "live_readmission_finished", fields);
    result
}
fn readmission_diagnostics(algorithm: &str, slot: i64, request: &LiveReadmissionRequest) -> Value {
    // Never emit the idempotency key, authenticated owner, prior_state or raw errors.
    json!({"component":"continuous.readmission","stage":"readmission","profile_id":slot,"slot_id":slot,
        "algorithm_id":algorithm,"generation_id":request.expected_generation_id,"config_revision":request.expected_config_revision})
}
fn readmission_outcome_diagnostics(
    fields: &mut Value,
    result: &Result<LiveReadmissionOutcome, DbError>,
) -> Level {
    match result {
        Ok(LiveReadmissionOutcome::Granted(receipt)) => {
            fields["outcome"] = if receipt.duplicate {
                "already_granted"
            } else {
                "granted"
            }
            .into();
            fields["grant_id"] = receipt.grant_id.into();
            fields["attempt"] = receipt.attempts_before.into();
            fields["limit_before"] = receipt.limit_before.into();
            fields["attempt_limit"] = receipt.attempt_limit.into();
            fields["admission_state"] = if receipt.duplicate {
                "original_receipt_only"
            } else {
                "awaiting_claim"
            }
            .into();
            Level::Info
        }
        Ok(LiveReadmissionOutcome::NotFound) => {
            fields["outcome"] = "not_found".into();
            Level::Info
        }
        Ok(LiveReadmissionOutcome::Conflict(reason)) => {
            fields["outcome"] = "conflict".into();
            fields["reason"] = (*reason).into();
            Level::Info
        }
        Err(error) => {
            fields["outcome"] = "error".into();
            fields["error_code"] = diagnostics::db_error_code(error).into();
            Level::Error
        }
    }
}
async fn grant_inner(
    pool: &PgPool,
    owner: &str,
    algorithm: &str,
    slot: i64,
    request: &LiveReadmissionRequest,
    fields: &mut Value,
) -> Result<LiveReadmissionOutcome, DbError> {
    request.validate()?;
    if slot <= 0 {
        return Err(DbError::Json("invalid live readmission slot".into()));
    }
    let mut tx = pool.begin().await?;
    sqlx::query(
        "select set_config('statement_timeout','2000',true),set_config('lock_timeout','1000',true)",
    )
    .execute(&mut *tx)
    .await?;
    owner_lock(&mut tx, owner).await?;
    if let Some(prior)=sqlx::query("select r.*,g.study_id from continuous_live_readmissions r left join continuous_generations g on g.id=r.generation_id where r.owner_id=$1 and r.request_id=$2")
        .bind(owner).bind(&request.request_id).fetch_optional(&mut *tx).await? {
        if prior.try_get::<String,_>("algorithm_id")?!=algorithm || prior.try_get::<i64,_>("slot_id")?!=slot
            || prior.try_get::<i64,_>("generation_id")?!=request.expected_generation_id
            || prior.try_get::<i64,_>("config_revision")?!=request.expected_config_revision {
            return Ok(LiveReadmissionOutcome::Conflict("live_readmission_key_conflict"));
        }
        fields["study_id"]=json!(prior.try_get::<Option<i64>,_>("study_id")?);
        return Ok(LiveReadmissionOutcome::Granted(receipt(&prior,true)?));
    }
    let process=sqlx::query("select desired_running,config_revision,config,accepted_generation_id from adaptive_live_controls where owner_id=$1 and algorithm_id=$2 and id=$3 for share")
        .bind(owner).bind(algorithm).bind(slot).fetch_optional(&mut *tx).await?;
    let target_slot=sqlx::query("select enabled,active_generation_id,pending_generation_id,pending_error,updated_at from continuous_slots where id=$1 and owner_id=$2 and algorithm_id=$3 for update")
        .bind(slot).bind(owner).bind(algorithm).fetch_optional(&mut *tx).await?;
    let (Some(process), Some(target_slot)) = (process, target_slot) else {
        return Ok(LiveReadmissionOutcome::NotFound);
    };
    if !process.try_get::<bool, _>("desired_running")?
        || process.try_get::<i64, _>("config_revision")? != request.expected_config_revision
        || process.try_get::<Value, _>("config")?["adaptiveEnabled"] != true
        || process.try_get::<i64, _>("accepted_generation_id")? != request.expected_generation_id
    {
        return Ok(LiveReadmissionOutcome::Conflict(
            "live_readmission_stale_process",
        ));
    }
    let global = sqlx::query("select value from settings where key='live' for share")
        .fetch_optional(&mut *tx)
        .await?;
    if !global.is_some_and(|row| row.get::<Value, _>("value")["enabled"] == true) {
        return Ok(LiveReadmissionOutcome::Conflict(
            "live_readmission_disabled",
        ));
    }
    let lane=sqlx::query("select slot_id,generation_id,lease_owner from adaptive_live_warmup_lane where id=1 for update")
        .fetch_one(&mut *tx).await?;
    let generation=sqlx::query("select config_revision,status,accepted,study_id,live_initialization,live_init_attempts,live_init_attempt_limit,live_init_status,live_init_error,live_init_deadline,live_init_next_attempt_at,updated_at from continuous_generations where id=$1 and owner_id=$2 and algorithm_id=$3 and slot_id=$4 for update")
        .bind(request.expected_generation_id).bind(owner).bind(algorithm).bind(slot).fetch_optional(&mut *tx).await?;
    let Some(generation) = generation else {
        return Ok(LiveReadmissionOutcome::NotFound);
    };
    fields["study_id"] = json!(generation.try_get::<Option<i64>, _>("study_id")?);
    let attempts: i32 = generation.try_get("live_init_attempts")?;
    let limit: i32 = generation.try_get("live_init_attempt_limit")?;
    fields["attempt"] = attempts.into();
    fields["attempt_limit"] = limit.into();
    if generation.try_get::<String, _>("status")? != "completed"
        || generation.try_get::<Option<bool>, _>("accepted")? != Some(true)
        || generation.try_get::<Option<i64>, _>("study_id")?.is_none()
        || generation
            .try_get::<Option<Value>, _>("live_initialization")?
            .is_none()
        || generation
            .try_get::<Option<String>, _>("live_init_status")?
            .as_deref()
            != Some("exhausted")
        || attempts != limit
    {
        return Ok(LiveReadmissionOutcome::Conflict(
            "live_readmission_ineligible",
        ));
    }
    if target_slot
        .try_get::<Option<i64>, _>("pending_generation_id")?
        .is_some()
        || target_slot.try_get::<Option<i64>, _>("active_generation_id")?
            == Some(request.expected_generation_id)
    {
        return Ok(LiveReadmissionOutcome::Conflict(
            "live_readmission_pending_conflict",
        ));
    }
    if lane.try_get::<Option<i64>, _>("slot_id")?.is_some()
        || lane.try_get::<Option<i64>, _>("generation_id")?.is_some()
        || lane.try_get::<Option<String>, _>("lease_owner")?.is_some()
    {
        return Ok(LiveReadmissionOutcome::Conflict(
            "live_readmission_lane_busy",
        ));
    }
    let next_limit = limit
        .checked_add(MAX_INITIALIZATION_ATTEMPTS)
        .ok_or_else(|| DbError::Json("live initialization attempt budget overflow".into()))?;
    // Snapshot only observed operational evidence; no invented per-attempt history.
    let prior_state = json!({"liveInitStatus":generation.try_get::<Option<String>,_>("live_init_status")?,
        "attempts":attempts,"attemptLimit":limit,"lastError":generation.try_get::<Option<Value>,_>("live_init_error")?,
        "attemptDeadline":generation.try_get::<Option<DateTime<Utc>>,_>("live_init_deadline")?,
        "nextAttemptAt":generation.try_get::<Option<DateTime<Utc>>,_>("live_init_next_attempt_at")?,
        "generationUpdatedAt":generation.try_get::<DateTime<Utc>,_>("updated_at")?,
        "slotError":target_slot.try_get::<Option<Value>,_>("pending_error")?,"slotUpdatedAt":target_slot.try_get::<DateTime<Utc>,_>("updated_at")?});
    let saved=sqlx::query("insert into continuous_live_readmissions(owner_id,algorithm_id,slot_id,generation_id,config_revision,request_id,attempts_before,limit_before,attempt_limit,prior_state) values($1,$2,$3,$4,$5,$6,$7,$8,$9,$10) returning *")
        .bind(owner).bind(algorithm).bind(slot).bind(request.expected_generation_id).bind(request.expected_config_revision)
        .bind(&request.request_id).bind(attempts).bind(limit).bind(next_limit).bind(prior_state).fetch_one(&mut *tx).await?;
    sqlx::query("update continuous_generations set live_init_attempt_limit=$2,live_init_status='retry',live_init_deadline=null,live_init_next_attempt_at=clock_timestamp() where id=$1")
        .bind(request.expected_generation_id).bind(next_limit).execute(&mut *tx).await?;
    sqlx::query("update continuous_slots set pending_generation_id=$2,pending_status='warming',readiness_frontier=null,status=case when active_generation_id is null then 'warming' else status end,updated_at=now() where id=$1")
        .bind(slot).bind(request.expected_generation_id).execute(&mut *tx).await?;
    let result = receipt(&saved, false)?;
    tx.commit().await?;
    Ok(LiveReadmissionOutcome::Granted(result))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn readmission_log_projection_does_not_expose_request_keys_or_raw_errors() {
        let request = LiveReadmissionRequest {
            expected_generation_id: 31,
            expected_config_revision: 2,
            request_id: "private-idempotency-key".into(),
        };
        let receipt = LiveReadmissionReceipt {
            grant_id: 7,
            request_id: request.request_id.clone(),
            slot_id: 4,
            generation_id: 31,
            config_revision: 2,
            attempts_before: 3,
            limit_before: 3,
            attempt_limit: 6,
            granted_at: Utc::now(),
            duplicate: false,
        };
        let mut fields = readmission_diagnostics("price_volume_spike", 4, &request);
        assert_eq!(
            readmission_outcome_diagnostics(
                &mut fields,
                &Ok(LiveReadmissionOutcome::Granted(receipt.clone()))
            ),
            Level::Info
        );
        assert_eq!(fields["outcome"], "granted");
        assert_eq!(fields["attempt"], 3);
        assert_eq!(fields["attempt_limit"], 6);
        assert_eq!(fields["admission_state"], "awaiting_claim");
        let mut fields = readmission_diagnostics("price_volume_spike", 4, &request);
        readmission_outcome_diagnostics(
            &mut fields,
            &Ok(LiveReadmissionOutcome::Granted(LiveReadmissionReceipt {
                duplicate: true,
                ..receipt
            })),
        );
        assert_eq!(fields["outcome"], "already_granted");
        assert_eq!(fields["admission_state"], "original_receipt_only");
        for outcome in [
            Ok(LiveReadmissionOutcome::Conflict(
                "live_readmission_lane_busy",
            )),
            Ok(LiveReadmissionOutcome::NotFound),
            Err(DbError::Json(
                "private-raw-error-with-credentials".repeat(1000),
            )),
        ] {
            let mut fields = readmission_diagnostics("price_volume_spike", 4, &request);
            readmission_outcome_diagnostics(&mut fields, &outcome);
            let record = diagnostics::record(Level::Info, "live_readmission_finished", &fields);
            let text = record.to_string();
            assert!(text.len() + 1 <= diagnostics::MAX_RECORD_BYTES);
            assert!(!text.contains("private-"));
            assert!(record.get("request_id").is_none());
            if outcome.is_err() {
                assert_eq!(record["error_code"], "invalid_state");
                assert_eq!(record["outcome"], "error");
            }
        }
    }
}
