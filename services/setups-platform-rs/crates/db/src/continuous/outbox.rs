//! Bounded durable post-commit delivery. No network runs in a scan transaction.
use super::{ActiveAdaptiveSlot, AdaptiveSetupWrite};
use crate::DbError;
use serde_json::{Value, json};
use sqlx::{PgPool, Postgres, Row, Transaction};

pub const NOTIFICATION_LEASE_SECONDS: i64 = 30;
pub const MAX_NOTIFICATION_ATTEMPTS: i32 = 5;

#[derive(Debug, Clone)]
pub struct NotificationJob {
    pub id: i64,
    pub setup_id: i64,
    pub owner_id: String,
    pub algorithm_id: String,
    pub slot_id: Option<i64>,
    pub generation_id: Option<i64>,
    pub config_revision: Option<i64>,
    pub fixed_config: Option<Value>,
    pub lease_token: i64,
    pub raw_close_unix: i64,
    pub expires_at_unix: i64,
    pub attempts: i32,
    pub payload: Value,
}

#[derive(Debug, Clone)]
pub enum NotificationOutcome {
    Sent,
    Skipped,
    Retry { delay_seconds: i64, error: String },
    Failed { error: String },
}

pub async fn configure_limits(pool: &PgPool, max_jobs: i64, max_bytes: i64) -> Result<(),DbError> {
    if !(1..=100000).contains(&max_jobs) || !(32768..=1073741824).contains(&max_bytes) {
        return Err(DbError::Json("invalid notification outbox count/byte budgets".into()));
    }
    sqlx::query("update adaptive_notification_budget set max_jobs=$1,max_bytes=$2 where id=1")
        .bind(max_jobs).bind(max_bytes).execute(pool).await?;
    Ok(())
}

async fn notification_expiry(tx: &mut Transaction<'_,Postgres>, raw_close: i64) -> Result<i64,DbError> {
    let settings: Option<Value> = sqlx::query_scalar("select value from settings where key='notifications'").fetch_optional(&mut **tx).await?;
    let max_age = match settings.as_ref().and_then(|v| v.get("maxAgeSeconds")) {
        None => 60,
        Some(value) => value.as_i64().filter(|v| (1..=86400).contains(v)).ok_or_else(||DbError::Json("invalid persisted notification TTL".into()))?,
    };
    raw_close.checked_add(max_age).ok_or_else(||DbError::Json("notification TTL overflow".into()))
}

pub(super) async fn enqueue(tx: &mut Transaction<'_,Postgres>, profile: &ActiveAdaptiveSlot, setup_id: i32, setup: &AdaptiveSetupWrite) -> Result<(),DbError> {
    let expiry = notification_expiry(tx,setup.setup.close_time_unix).await?;
    let payload = json!({"instrument":{"exchange":setup.exchange_code,"category":setup.category,"symbol":setup.symbol},
        "raw":setup.setup,"recommendation":setup.recommendation,"externalName":profile.external_name});
    // The trigger persists explicit expired/skipped_capacity outcomes without a
    // job or rollback; every other SQL error still fails this scan transaction.
    sqlx::query("insert into adaptive_notification_outbox(setup_id,owner_id,algorithm_id,slot_id,generation_id,config_revision,raw_close_unix,expires_at,payload) values($1,$2,$3,$4,$5,$6,$7,to_timestamp($8),$9)")
        .bind(setup_id).bind(&profile.owner_id).bind(&profile.algorithm_id).bind(profile.slot_id).bind(profile.generation_id)
        .bind(profile.config_revision).bind(setup.setup.close_time_unix).bind(expiry as f64).bind(payload).execute(&mut **tx).await?;
    Ok(())
}

/// Fixed scanner entry point; the historical/reference insert_live_setup API is unchanged.
/// The setup's effective parameters/version are the config fence, not an opaque worker hash.
pub async fn commit_fixed_setup(pool: &PgPool, exchange: &str, category: &str, symbol: &str, setup: &setups_domain::SpikeSetup) -> Result<Option<i64>,DbError> {
    let key = format!("{exchange}/{category}/{symbol}");
    if key.len()>400 || [exchange,category,symbol].iter().any(|s| s.is_empty() || s.chars().any(char::is_control))
        || setup.open_time_unix%60!=0 || setup.open_time_unix.checked_add(60)!=Some(setup.close_time_unix)
        || setup.algorithm_id!=setup.params.algorithm_id() || setup.algorithm_version!=setup.params.algorithm_version() {
        return Err(DbError::Json("invalid fixed setup identity/time/algorithm".into()));
    }
    let mut tx = pool.begin().await?;
    let setting_key = format!("algorithm:{}",setup.algorithm_id);
    // Materialize the default row so concurrent first configuration is fenced too.
    sqlx::query("insert into settings(key,value) values($1,'{}') on conflict do nothing").bind(&setting_key).execute(&mut *tx).await?;
    let algorithm: Value = sqlx::query_scalar("select value from settings where key=$1 for share").bind(&setting_key).fetch_one(&mut *tx).await?;
    let global: Option<Value> = sqlx::query_scalar("select value from settings where key='live' for share").fetch_optional(&mut *tx).await?;
    let current = crate::runs::algorithm_params_from_settings(&setup.algorithm_id,Some(&algorithm));
    if global.as_ref().is_none_or(|v| v["enabled"]!=true) || algorithm["enabled"]==false || current!=setup.params {return Ok(None);}
    let external_name = algorithm["externalName"].as_str().map(str::to_owned).unwrap_or_else(||crate::algorithm_name(&setup.algorithm_id));
    if external_name.is_empty() || external_name.chars().count()>120 || external_name.len()>480 || external_name.chars().any(char::is_control) {
        return Err(DbError::Json("invalid fixed setup external name".into()));
    }
    let params = serde_json::to_value(&setup.params).map_err(crate::json_error)?;
    let id: Option<i32> = sqlx::query_scalar("insert into live_setups(exchange_code,category,symbol,algorithm_id,algorithm_version,params,open_time,close_time,close_price,details,external_name) values($1,$2,$3,$4,$5,$6,to_timestamp($7),to_timestamp($8),$9,$10,$11) on conflict(exchange_code,category,symbol,algorithm_id,algorithm_version,open_time) where adaptive_slot_id is null do nothing returning id")
        .bind(exchange).bind(category).bind(symbol).bind(&setup.algorithm_id).bind(&setup.algorithm_version).bind(&params)
        .bind(setup.open_time_unix as f64).bind(setup.close_time_unix as f64).bind(&setup.close_price)
        .bind(serde_json::to_value(&setup.details).map_err(crate::json_error)?).bind(&external_name).fetch_optional(&mut *tx).await?;
    let Some(id)=id else {return Ok(None);};
    let expiry = notification_expiry(&mut tx,setup.close_time_unix).await?;
    let payload = json!({"instrument":{"exchange":exchange,"category":category,"symbol":symbol},"raw":setup,"recommendation":null,"externalName":external_name});
    let fence = json!({"algorithmVersion":setup.algorithm_version,"params":params});
    sqlx::query("insert into adaptive_notification_outbox(setup_id,owner_id,algorithm_id,fixed_config,raw_close_unix,expires_at,payload) values($1,'admin',$2,$3,$4,to_timestamp($5),$6)")
        .bind(id).bind(&setup.algorithm_id).bind(fence).bind(setup.close_time_unix).bind(expiry as f64).bind(payload).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(Some(i64::from(id)))
}

fn validate_worker(worker: &str) -> Result<(),DbError> {
    if worker.is_empty() || worker.len()>200 || worker.chars().any(char::is_control) {return Err(DbError::Json("invalid notification worker".into()));}
    Ok(())
}

/// Also performs actual expiry cleanup when there are no subscriptions or no new setups.
pub async fn claim_notifications(pool: &PgPool, worker: &str, limit: usize) -> Result<Vec<NotificationJob>,DbError> {
    validate_worker(worker)?;
    if !(1..=32).contains(&limit) {return Err(DbError::Json("notification claim batch must be in 1..=32".into()));}
    let mut tx = pool.begin().await?;
    // Match insert-trigger accounting order; expiry/finish must not hold job
    // rows while waiting for a budget row owned by another deleting batch.
    sqlx::query("select id from adaptive_notification_budget where id=1 for update").execute(&mut *tx).await?;
    sqlx::query("with terminal as (delete from adaptive_notification_outbox where expires_at<=clock_timestamp() or (attempts>=5 and (lease_until is null or lease_until<=clock_timestamp())) returning setup_id,case when expires_at<=clock_timestamp() then 'expired' else 'failed' end status) update live_setups s set notification_delivery_status=t.status,notification_delivery_updated_at=clock_timestamp() from terminal t where s.id=t.setup_id").execute(&mut *tx).await?;
    let rows = sqlx::query("with due as (select id from adaptive_notification_outbox where not_before<=clock_timestamp() and expires_at>clock_timestamp() and attempts<5 and (lease_owner is null or lease_until<=clock_timestamp()) order by not_before,id for update skip locked limit $1) update adaptive_notification_outbox o set lease_owner=$2,lease_token=lease_token+1,lease_until=clock_timestamp()+$3::bigint*interval '1 second',attempts=attempts+1 from due where o.id=due.id returning o.*,extract(epoch from o.expires_at)::bigint expires_at_unix")
        .bind(limit as i64).bind(worker).bind(NOTIFICATION_LEASE_SECONDS).fetch_all(&mut *tx).await?;
    let jobs = rows.into_iter().map(|r| Ok(NotificationJob {
        id:r.try_get("id")?,setup_id:i64::from(r.try_get::<i32,_>("setup_id")?),owner_id:r.try_get("owner_id")?,algorithm_id:r.try_get("algorithm_id")?,
        slot_id:r.try_get("slot_id")?,generation_id:r.try_get("generation_id")?,config_revision:r.try_get("config_revision")?,fixed_config:r.try_get("fixed_config")?,
        lease_token:r.try_get("lease_token")?,raw_close_unix:r.try_get("raw_close_unix")?,expires_at_unix:r.try_get("expires_at_unix")?,
        attempts:r.try_get("attempts")?,payload:r.try_get("payload")?,
    })).collect::<Result<Vec<_>,DbError>>()?;
    tx.commit().await?;
    Ok(jobs)
}

/// A committed pre-watermark setup stays deliverable after a normal promotion, but not Stop/nonpass.
pub async fn notification_permitted(pool: &PgPool, worker: &str, job: &NotificationJob) -> Result<bool,DbError> {
    if let Some(fence) = &job.fixed_config {
        let current: bool = sqlx::query_scalar("select exists(select 1 from adaptive_notification_outbox where id=$1 and lease_owner=$2 and lease_token=$3 and lease_until>clock_timestamp() and expires_at>clock_timestamp())")
            .bind(job.id).bind(worker).bind(job.lease_token).fetch_one(pool).await?;
        if !current {return Ok(false);}
        let row = sqlx::query("select (select value from settings where key='live') global,(select value from settings where key=$1) algorithm")
            .bind(format!("algorithm:{}",job.algorithm_id)).fetch_one(pool).await?;
        let global: Option<Value> = row.try_get("global")?;
        let algorithm: Option<Value> = row.try_get("algorithm")?;
        let params = crate::runs::algorithm_params_from_settings(&job.algorithm_id,algorithm.as_ref());
        return Ok(global.as_ref().is_some_and(|v| v["enabled"]==true)
            && algorithm.as_ref().is_none_or(|v| v["enabled"]!=false)
            && fence["algorithmVersion"]==params.algorithm_version()
            && fence["params"]==serde_json::to_value(params).map_err(crate::json_error)?);
    }
    Ok(sqlx::query_scalar("select exists(select 1 from adaptive_notification_outbox o join continuous_slots s on s.id=o.slot_id join continuous_processes p on p.owner_id=s.owner_id and p.algorithm_id=s.algorithm_id where o.id=$1 and o.lease_owner=$2 and o.lease_token=$3 and o.lease_until>clock_timestamp() and o.expires_at>clock_timestamp() and s.enabled and s.active_generation_id is not null and p.desired_running and p.config_revision=o.config_revision and p.config->'adaptiveEnabled'='true'::jsonb and exists(select 1 from settings where key='live' and value->'enabled'='true'::jsonb))")
        .bind(job.id).bind(worker).bind(job.lease_token).fetch_one(pool).await?)
}

pub async fn renew_notification(pool: &PgPool, worker: &str, job: &NotificationJob) -> Result<bool,DbError> {
    Ok(sqlx::query("update adaptive_notification_outbox set lease_until=clock_timestamp()+$4::bigint*interval '1 second' where id=$1 and lease_owner=$2 and lease_token=$3 and lease_until>clock_timestamp() and expires_at>clock_timestamp()")
        .bind(job.id).bind(worker).bind(job.lease_token).bind(NOTIFICATION_LEASE_SECONDS).execute(pool).await?.rows_affected()==1)
}

pub async fn finish_notification(pool: &PgPool, worker: &str, job: &NotificationJob, outcome: NotificationOutcome) -> Result<bool,DbError> {
    validate_worker(worker)?;
    let terminal_status = match &outcome {
        NotificationOutcome::Sent => "sent",
        NotificationOutcome::Skipped => "skipped",
        NotificationOutcome::Retry{..} if job.attempts<MAX_NOTIFICATION_ATTEMPTS => "pending",
        _ => "failed",
    };
    let terminal_error = match &outcome {
        NotificationOutcome::Retry{error,..} | NotificationOutcome::Failed{error} => Some(error.clone()),
        _ => None,
    };
    let retry = match outcome {
        NotificationOutcome::Retry{delay_seconds,error} if job.attempts<MAX_NOTIFICATION_ATTEMPTS => {
            if !(1..=86400).contains(&delay_seconds) || error.len()>1024 {return Err(DbError::Json("invalid bounded notification retry".into()));}
            Some((delay_seconds,error))
        },
        NotificationOutcome::Failed{error} | NotificationOutcome::Retry{error,..} => {
            if error.len()>1024 {return Err(DbError::Json("notification error exceeds budget".into()));}
            None
        },
        _ => None,
    };
    let mut tx = pool.begin().await?;
    sqlx::query("select id from adaptive_notification_budget where id=1 for update").execute(&mut *tx).await?;
    let changed = if let Some((delay,error)) = retry {
        sqlx::query("update adaptive_notification_outbox set lease_owner=null,lease_until=null,not_before=clock_timestamp()+$4::bigint*interval '1 second',last_error=$5 where id=$1 and lease_owner=$2 and lease_token=$3 and lease_until>clock_timestamp()")
            .bind(job.id).bind(worker).bind(job.lease_token).bind(delay).bind(error).execute(&mut *tx).await?.rows_affected()
    } else {
        sqlx::query("delete from adaptive_notification_outbox where id=$1 and lease_owner=$2 and lease_token=$3 and lease_until>clock_timestamp()")
            .bind(job.id).bind(worker).bind(job.lease_token).execute(&mut *tx).await?.rows_affected()
    };
    if changed==1 {
        sqlx::query("update live_setups set notification_delivery_status=case when $2='skipped' and to_timestamp($4)<=clock_timestamp() then 'expired' else $2 end,notification_delivery_error=$3,notification_delivery_updated_at=clock_timestamp() where id::bigint=$1")
            .bind(job.setup_id).bind(terminal_status).bind(terminal_error).bind(job.expires_at_unix as f64).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(changed==1)
}
