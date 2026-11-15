//! Independent bounded Telegram delivery. No network or secrets enter this module.
use crate::DbError;
use chrono::{DateTime,Utc};
use serde::{Deserialize,Serialize};
use serde_json::{Value,json};
use sha2::{Digest,Sha256};
use sqlx::{PgPool,Postgres,Row,Transaction};

pub const MAX_ATTEMPTS:i32=5;
pub const LEASE_SECONDS:i64=30;
pub const MAX_BODY_BYTES:usize=32768;

#[derive(Debug,Clone,PartialEq,Eq,Serialize)]
#[serde(rename_all="camelCase")]
pub struct TelegramConfig {pub enabled:bool,pub endpoint_url:String,pub revision:i64}
#[derive(Debug,Clone,PartialEq,Eq,Serialize,Deserialize)]
pub struct TelegramReason {pub code:String,pub message:String}
impl TelegramReason {
    pub fn new(code:&str,message:&str)->Self {Self {code:code.into(),message:message.into()}}
    fn validate(&self)->Result<(),DbError> {
        if self.code.is_empty() || self.code.len()>64 || !self.code.bytes().all(|b|b.is_ascii_alphanumeric()||matches!(b,b'_'|b'-'|b'.'))
            || self.message.len()>512 || self.message.chars().any(char::is_control) {return Err(DbError::Json("invalid bounded Telegram reason".into()));}
        Ok(())
    }
}
#[derive(Debug,Clone)]
pub struct TelegramJob {
    pub id:i64,pub setup_id:i64,pub delivery_id:String,pub endpoint_url:String,pub settings_revision:i64,
    pub owner_id:String,pub algorithm_id:String,pub slot_id:Option<i64>,pub generation_id:Option<i64>,
    pub config_revision:Option<i64>,pub fixed_config:Option<Value>,pub lease_token:i64,
    pub raw_close_unix:i64,pub expires_at_unix:i64,pub attempts:i32,pub payload:Value,pub request_body:Option<Vec<u8>>,
}
#[derive(Debug,Clone)]
pub enum TelegramOutcome {
    Sent,Disabled {reason:TelegramReason},Retry {delay_seconds:i64,reason:TelegramReason},Failed {reason:TelegramReason},
}
#[derive(Debug,Clone)]
pub struct TelegramStatusUpdate {
    /// Trusted transport observation: true only after response-MAC verification.
    pub relay_authenticated:bool,pub telegram_ready:Option<bool>,pub reason:Option<TelegramReason>,
}
#[derive(Debug,Clone,Serialize)]
#[serde(rename_all="camelCase")]
pub struct TelegramStatus {
    pub endpoint_url:String,pub settings_revision:i64,pub secret_configured:bool,pub state:String,
    pub telegram_channel:String,pub checked_at:Option<DateTime<Utc>>,pub reason:Option<TelegramReason>,
}
#[derive(Debug,Clone,Serialize)]
pub struct TelegramEnvelope {pub settings:TelegramConfig,pub status:TelegramStatus}

#[derive(Debug)]
pub enum ConfigUpdate {Updated(TelegramConfig),Stale,EndpointBusy}
/// API additionally canonicalizes through its URL parser; transport separately
/// validates/pins public DNS addresses. No network occurs in this transaction.
pub async fn set_config(pool:&PgPool,expected_revision:i64,enabled:bool,endpoint_url:&str)->Result<ConfigUpdate,DbError> {
    let bare=endpoint_url.strip_suffix('/').unwrap_or(endpoint_url);
    let normalized=if bare.is_empty(){String::new()}else{format!("{bare}/")};
    let endpoint=normalized.as_str();
    let valid_origin=bare.strip_prefix("https://").is_some_and(|authority|!authority.is_empty()
        &&authority.bytes().all(|byte|byte.is_ascii_alphanumeric()||matches!(byte,b'.'|b'-'|b':'|b'['|b']')));
    if expected_revision<0 || endpoint.len()>2048 || ((!endpoint.is_empty()||enabled)&&!valid_origin) {
        return Err(DbError::Json("invalid Telegram HTTPS origin".into()));
    }
    let mut tx=pool.begin().await?;lock_budget(&mut tx).await?;cleanup(&mut tx).await?;
    let current=sqlx::query("select enabled,endpoint_url,revision from telegram_relay_config where id=1 for update").fetch_one(&mut *tx).await?;
    let revision:i64=current.try_get("revision")?;
    if revision!=expected_revision {return Ok(ConfigUpdate::Stale);}
    let old_endpoint:String=current.try_get("endpoint_url")?;
    if old_endpoint==endpoint&&current.try_get::<bool,_>("enabled")?==enabled {
        tx.commit().await?;return Ok(ConfigUpdate::Updated(TelegramConfig {enabled,endpoint_url:endpoint.into(),revision}));
    }
    if old_endpoint!=endpoint {
        let busy:bool=sqlx::query_scalar("select exists(select 1 from telegram_delivery_outbox where lease_owner is not null or attempts>0) or exists(select 1 from telegram_relay_tests where in_flight_until>=date_trunc('second',clock_timestamp()))").fetch_one(&mut *tx).await?;
        if busy {return Ok(ConfigUpdate::EndpointBusy);}
    }
    let next=revision.checked_add(1).ok_or_else(||DbError::Json("Telegram settings revision overflow".into()))?;
    sqlx::query("update telegram_relay_config set enabled=$1,endpoint_url=$2,revision=$3,state='unknown',telegram_channel='unknown',checked_at=null,reason=null,updated_at=clock_timestamp() where id=1")
        .bind(enabled).bind(endpoint).bind(next).execute(&mut *tx).await?;
    if enabled {
        // Never reroute attempted work. Untouched jobs retain identity/raw body/TTL.
        sqlx::query("update telegram_delivery_outbox set endpoint_url=$1,settings_revision=$2 where attempts=0 and lease_owner is null")
            .bind(endpoint).bind(next).execute(&mut *tx).await?;
    }
    cleanup(&mut tx).await?;tx.commit().await?;
    Ok(ConfigUpdate::Updated(TelegramConfig {enabled,endpoint_url:endpoint.into(),revision:next}))
}

#[derive(Debug,Clone)]
pub struct TelegramTestAttempt {pub config:TelegramConfig,pub request_id:String,pub delivery_id:String,pub expires_at_unix:i64,pub request_body:Vec<u8>}
#[derive(Debug,Clone,Serialize)]
#[serde(rename_all="camelCase")]
pub struct TelegramTestResult {pub request_id:String,pub state:String,pub reason:Option<TelegramReason>}
#[derive(Debug)]
pub enum ProbeAdmission {Admitted(TelegramConfig),Stale,RateLimited,NotConfigured}
#[derive(Debug)]
pub enum TestAdmission {Admitted(TelegramTestAttempt),Existing(TelegramTestResult),Stale,RateLimited,NotConfigured,KeyConflict,Capacity}

pub fn valid_test_request_id(value:&str)->bool {
    value.len()==36&&value.bytes().enumerate().all(|(index,byte)|if [8,13,18,23].contains(&index){byte==b'-'}else{byte.is_ascii_hexdigit()&& !byte.is_ascii_uppercase()})
}
pub async fn begin_probe(pool:&PgPool,expected_revision:i64)->Result<ProbeAdmission,DbError> {
    let mut tx=pool.begin().await?;
    let row=sqlx::query("select *,probe_started_at>clock_timestamp()-interval '15 seconds' limited from telegram_relay_config where id=1 for update").fetch_one(&mut *tx).await?;
    let config=TelegramConfig {enabled:row.try_get("enabled")?,endpoint_url:row.try_get("endpoint_url")?,revision:row.try_get("revision")?};
    if config.revision!=expected_revision {return Ok(ProbeAdmission::Stale);}
    if config.endpoint_url.is_empty() {return Ok(ProbeAdmission::NotConfigured);}
    if row.try_get::<Option<bool>,_>("limited")?==Some(true) {return Ok(ProbeAdmission::RateLimited);}
    sqlx::query("update telegram_relay_config set probe_started_at=clock_timestamp(),state='checking',reason=null where id=1").execute(&mut *tx).await?;
    tx.commit().await?;Ok(ProbeAdmission::Admitted(config))
}
pub async fn begin_test(pool:&PgPool,expected_revision:i64,request_id:&str)->Result<TestAdmission,DbError> {
    if !valid_test_request_id(request_id) {return Err(DbError::Json("Telegram test requestId must be a canonical UUID".into()));}
    let mut tx=pool.begin().await?;lock_budget(&mut tx).await?;cleanup(&mut tx).await?;
    let row=sqlx::query("select *,test_started_at>clock_timestamp()-interval '15 seconds' limited from telegram_relay_config where id=1 for update").fetch_one(&mut *tx).await?;
    let config=TelegramConfig {enabled:row.try_get("enabled")?,endpoint_url:row.try_get("endpoint_url")?,revision:row.try_get("revision")?};
    if config.revision!=expected_revision {return Ok(TestAdmission::Stale);}
    if let Some(existing)=sqlx::query("select * from telegram_relay_tests where request_id=$1").bind(request_id).fetch_optional(&mut *tx).await? {
        if existing.try_get::<i64,_>("settings_revision")?!=config.revision {return Ok(TestAdmission::KeyConflict);}
        let reason:Option<Value>=existing.try_get("reason")?;
        return Ok(TestAdmission::Existing(TelegramTestResult {request_id:request_id.into(),state:existing.try_get("state")?,reason:reason.map(serde_json::from_value).transpose().map_err(crate::json_error)?}));
    }
    if config.endpoint_url.is_empty() {return Ok(TestAdmission::NotConfigured);}
    if row.try_get::<Option<bool>,_>("limited")?==Some(true) {return Ok(TestAdmission::RateLimited);}
    let count:i64=sqlx::query_scalar("select count(*) from telegram_relay_tests").fetch_one(&mut *tx).await?;
    if count>=128 {return Ok(TestAdmission::Capacity);}
    let instance:String=sqlx::query_scalar("select instance_id::text from telegram_delivery_budget where id=1").fetch_one(&mut *tx).await?;
    let now:i64=sqlx::query_scalar("select floor(extract(epoch from clock_timestamp()))::bigint").fetch_one(&mut *tx).await?;
    let delivery_id=format!("test:{instance}:{:x}",Sha256::digest(request_id.as_bytes()));let expires_at_unix=now+60;
    let request_body=serde_json::to_vec(&json!({"version":1,"id":delivery_id,"text":"Telegram relay delivery test.","expiresAt":expires_at_unix,"attachment":null})).map_err(crate::json_error)?;
    // Admission and origin changes share budget/config locks. The original
    // expiry protects a delayed diagnostic without retaining a SQL transaction
    // across its network attempt or blocking the origin for receipt retention.
    sqlx::query("insert into telegram_relay_tests(request_id,settings_revision,endpoint_url,delivery_id,request_body,in_flight_until) values($1,$2,$3,$4,$5,to_timestamp($6))")
        .bind(request_id).bind(config.revision).bind(&config.endpoint_url).bind(&delivery_id).bind(&request_body).bind(expires_at_unix as f64).execute(&mut *tx).await?;
    sqlx::query("update telegram_relay_config set test_started_at=clock_timestamp() where id=1").execute(&mut *tx).await?;
    tx.commit().await?;Ok(TestAdmission::Admitted(TelegramTestAttempt {config,request_id:request_id.into(),delivery_id,expires_at_unix,request_body}))
}
pub async fn record_test(pool:&PgPool,attempt:&TelegramTestAttempt,result:&TelegramTestResult)->Result<bool,DbError> {
    if result.request_id!=attempt.request_id||!["sent","unknown","error"].contains(&result.state.as_str()) {return Err(DbError::Json("invalid Telegram test result".into()));}
    if let Some(reason)=&result.reason {reason.validate()?;}
    sqlx::query("update telegram_relay_tests set state=$3,reason=$4,in_flight_until=null where request_id=$1 and settings_revision=$2")
        .bind(&attempt.request_id).bind(attempt.config.revision).bind(&result.state).bind(result.reason.as_ref().map(|reason|json!(reason))).execute(pool).await?;
    Ok(get_config(pool).await?.revision==attempt.config.revision)
}

pub async fn get_config(pool:&PgPool)->Result<TelegramConfig,DbError> {
    let row=sqlx::query("select enabled,endpoint_url,revision from telegram_relay_config where id=1").fetch_one(pool).await?;
    Ok(TelegramConfig {enabled:row.try_get("enabled")?,endpoint_url:row.try_get("endpoint_url")?,revision:row.try_get("revision")?})
}
pub async fn get_envelope(pool:&PgPool,secret_configured:bool)->Result<TelegramEnvelope,DbError> {
    let row=sqlx::query("select *,probe_started_at>=clock_timestamp()-interval '15 seconds' checking_fresh from telegram_relay_config where id=1").fetch_one(pool).await?;
    let settings=TelegramConfig {enabled:row.try_get("enabled")?,endpoint_url:row.try_get("endpoint_url")?,revision:row.try_get("revision")?};
    let mut state=if !settings.enabled {"disabled".into()}else if !secret_configured||settings.endpoint_url.is_empty() {"not_configured".into()}else{row.try_get("state")?};
    if state=="checking"&&row.try_get::<Option<bool>,_>("checking_fresh")?!=Some(true) {state="unknown".into();}
    let reason:Option<Value>=row.try_get("reason")?;
    let status=TelegramStatus {endpoint_url:settings.endpoint_url.clone(),settings_revision:settings.revision,secret_configured,state,
        telegram_channel:row.try_get("telegram_channel")?,checked_at:row.try_get("checked_at")?,
        reason:reason.map(serde_json::from_value).transpose().map_err(crate::json_error)?};
    Ok(TelegramEnvelope {settings,status})
}
pub async fn record_status(pool:&PgPool,expected_revision:i64,update:&TelegramStatusUpdate)->Result<bool,DbError> {
    if let Some(reason)=&update.reason {reason.validate()?;}
    let state=if update.relay_authenticated {"connected"}else{"error"};
    let channel=if !update.relay_authenticated {"unknown"}else{match update.telegram_ready {Some(true)=>"ready",Some(false)=>"error",None=>"unknown"}};
    Ok(sqlx::query("update telegram_relay_config set state=$2,telegram_channel=$3,reason=$4,checked_at=clock_timestamp() where id=1 and revision=$1")
        .bind(expected_revision).bind(state).bind(channel).bind(update.reason.as_ref().map(|reason|json!(reason))).execute(pool).await?.rows_affected()==1)
}
fn valid_worker(worker:&str)->Result<(),DbError> {
    if worker.is_empty()||worker.len()>200||worker.chars().any(char::is_control) {return Err(DbError::Json("invalid Telegram worker".into()));}Ok(())
}
fn job(row:sqlx::postgres::PgRow)->Result<TelegramJob,DbError> {
    Ok(TelegramJob {id:row.try_get("id")?,setup_id:i64::from(row.try_get::<i32,_>("setup_id")?),delivery_id:row.try_get("delivery_id")?,
        endpoint_url:row.try_get("endpoint_url")?,settings_revision:row.try_get("settings_revision")?,owner_id:row.try_get("owner_id")?,algorithm_id:row.try_get("algorithm_id")?,
        slot_id:row.try_get("slot_id")?,generation_id:row.try_get("generation_id")?,config_revision:row.try_get("config_revision")?,fixed_config:row.try_get("fixed_config")?,
        lease_token:row.try_get("lease_token")?,raw_close_unix:row.try_get("raw_close_unix")?,expires_at_unix:row.try_get("expires_at_unix")?,
        attempts:row.try_get("attempts")?,payload:row.try_get("payload")?,request_body:row.try_get("request_body")?})
}
async fn lock_budget(tx:&mut Transaction<'_,Postgres>)->Result<(),DbError> {
    sqlx::query("select id from telegram_delivery_budget where id=1 for update").fetch_one(&mut **tx).await?;Ok(())
}
async fn cleanup(tx:&mut Transaction<'_,Postgres>)->Result<(),DbError> {
    // Relay expiry is inclusive integer Unix seconds; only expiry clocks are
    // truncated. Lease deadlines, backoff and rate limits remain precise.
    // Keep unexpired active leases until the worker finishes/loses its lease.
    // Expired jobs never receive a new TTL or target, including on idle ticks.
    sqlx::query("with terminal as (delete from telegram_delivery_outbox o using telegram_relay_config c where c.id=1 and (o.expires_at<date_trunc('second',clock_timestamp()) or ((o.lease_owner is null or o.lease_until<=clock_timestamp()) and (not c.enabled or o.settings_revision<>c.revision or o.attempts>=5))) returning o.setup_id,case when o.expires_at<date_trunc('second',clock_timestamp()) then 'expired' when not c.enabled or o.settings_revision<>c.revision then 'disabled' else 'failed' end status) update live_setups s set telegram_delivery_status=t.status,telegram_delivery_updated_at=clock_timestamp() from terminal t where s.id=t.setup_id")
        .execute(&mut **tx).await?;
    sqlx::query("update telegram_relay_tests set in_flight_until=null where in_flight_until<date_trunc('second',clock_timestamp())").execute(&mut **tx).await?;
    sqlx::query("delete from telegram_relay_tests where created_at<clock_timestamp()-interval '24 hours'").execute(&mut **tx).await?;Ok(())
}
pub async fn claim(pool:&PgPool,worker:&str,limit:usize)->Result<Vec<TelegramJob>,DbError> {
    valid_worker(worker)?;if !(1..=32).contains(&limit) {return Err(DbError::Json("Telegram claim batch must be 1..=32".into()));}
    let mut tx=pool.begin().await?;lock_budget(&mut tx).await?;cleanup(&mut tx).await?;
    let rows=sqlx::query("with due as (select o.id from telegram_delivery_outbox o join telegram_relay_config c on c.id=1 where c.enabled and o.settings_revision=c.revision and o.endpoint_url=c.endpoint_url and o.not_before<=clock_timestamp() and o.expires_at>=date_trunc('second',clock_timestamp()) and o.attempts<5 and (o.lease_owner is null or o.lease_until<=clock_timestamp()) order by o.not_before,o.id for update of o skip locked limit $1) update telegram_delivery_outbox o set lease_owner=$2,lease_token=lease_token+1,lease_until=clock_timestamp()+$3::bigint*interval '1 second',attempts=attempts+1 from due where o.id=due.id returning o.*,extract(epoch from o.expires_at)::bigint expires_at_unix")
        .bind(limit as i64).bind(worker).bind(LEASE_SECONDS).fetch_all(&mut *tx).await?;
    let jobs=rows.into_iter().map(job).collect::<Result<Vec<_>,_>>()?;tx.commit().await?;Ok(jobs)
}
pub async fn permitted(pool:&PgPool,worker:&str,job:&TelegramJob)->Result<bool,DbError> {
    valid_worker(worker)?;
    let leased:bool=sqlx::query_scalar("select exists(select 1 from telegram_delivery_outbox o join telegram_relay_config c on c.id=1 where o.id=$1 and o.lease_owner=$2 and o.lease_token=$3 and o.settings_revision=$4 and o.endpoint_url=$5 and o.lease_until>clock_timestamp() and o.expires_at>=date_trunc('second',clock_timestamp()) and c.enabled and c.revision=o.settings_revision and c.endpoint_url=o.endpoint_url)")
        .bind(job.id).bind(worker).bind(job.lease_token).bind(job.settings_revision).bind(&job.endpoint_url).fetch_one(pool).await?;
    if !leased {return Ok(false);}
    if let Some(fence)=&job.fixed_config {
        let row=sqlx::query("select (select value from settings where key='live') global,(select value from settings where key=$1) algorithm")
            .bind(format!("algorithm:{}",job.algorithm_id)).fetch_one(pool).await?;
        let global:Option<Value>=row.try_get("global")?;let algorithm:Option<Value>=row.try_get("algorithm")?;
        let params=crate::runs::algorithm_params_from_settings(&job.algorithm_id,algorithm.as_ref());
        return Ok(global.as_ref().is_some_and(|value|value["enabled"]==true)&&algorithm.as_ref().is_none_or(|value|value["enabled"]!=false)
            &&fence["algorithmVersion"]==params.algorithm_version()&&fence["params"]==serde_json::to_value(params).map_err(crate::json_error)?);
    }
    Ok(sqlx::query_scalar("select exists(select 1 from continuous_slots s join continuous_processes p on p.owner_id=s.owner_id and p.algorithm_id=s.algorithm_id where s.id=$1 and s.owner_id=$2 and s.algorithm_id=$3 and s.enabled and s.active_generation_id is not null and p.desired_running and p.config_revision=$4 and p.config->'adaptiveEnabled'='true'::jsonb and exists(select 1 from settings where key='live' and value->'enabled'='true'::jsonb))")
        .bind(job.slot_id).bind(&job.owner_id).bind(&job.algorithm_id).bind(job.config_revision).fetch_one(pool).await?)
}
pub async fn renew(pool:&PgPool,worker:&str,job:&TelegramJob)->Result<bool,DbError> {
    valid_worker(worker)?;
    Ok(sqlx::query("update telegram_delivery_outbox set lease_until=clock_timestamp()+$4::bigint*interval '1 second' where id=$1 and lease_owner=$2 and lease_token=$3 and lease_until>clock_timestamp() and expires_at>=date_trunc('second',clock_timestamp())")
        .bind(job.id).bind(worker).bind(job.lease_token).bind(LEASE_SECONDS).execute(pool).await?.rows_affected()==1)
}
pub async fn freeze_message(pool:&PgPool,worker:&str,job:&TelegramJob,raw_body:&[u8])->Result<Option<Vec<u8>>,DbError> {
    valid_worker(worker)?;
    let mut tx=pool.begin().await?;
    let row=sqlx::query("select request_body from telegram_delivery_outbox where id=$1 and lease_owner=$2 and lease_token=$3 and lease_until>clock_timestamp() and expires_at>=date_trunc('second',clock_timestamp()) for update")
        .bind(job.id).bind(worker).bind(job.lease_token).fetch_optional(&mut *tx).await?;
    let Some(row)=row else{return Ok(None);};
    if let Some(body)=row.try_get::<Option<Vec<u8>>,_>("request_body")? {return Ok(Some(body));}
    if raw_body.len()>MAX_BODY_BYTES {return Err(DbError::Json("Telegram body exceeds 32 KiB".into()));}
    let body:Value=serde_json::from_slice(raw_body).map_err(crate::json_error)?;
    if body.as_object().is_none_or(|object|object.len()!=5||!["version","id","text","expiresAt","attachment"].iter().all(|key|object.contains_key(*key)))
        ||body["version"]!=1||body["id"]!=job.delivery_id||body["expiresAt"]!=job.expires_at_unix||!body["attachment"].is_null()
        ||body["text"].as_str().is_none_or(|text|text.is_empty()||text.chars().count()>4096) {
        return Err(DbError::Json("Telegram body differs from immutable delivery identity/expiry".into()));
    }
    let hash=format!("{:x}",Sha256::digest(raw_body));
    sqlx::query("update telegram_delivery_outbox set request_body=$2,request_body_hash=$3 where id=$1")
        .bind(job.id).bind(raw_body).bind(hash).execute(&mut *tx).await?;
    tx.commit().await?;Ok(Some(raw_body.to_vec()))
}
pub async fn finish(pool:&PgPool,worker:&str,job:&TelegramJob,outcome:TelegramOutcome)->Result<bool,DbError> {
    valid_worker(worker)?;
    let (status,reason,retry)=match outcome {
        TelegramOutcome::Sent=>("sent",None,None),
        TelegramOutcome::Disabled {reason}=>("disabled",Some(reason),None),
        TelegramOutcome::Failed {reason}=>("failed",Some(reason),None),
        TelegramOutcome::Retry {delay_seconds,reason}=>{
            if !(1..=86400).contains(&delay_seconds) {return Err(DbError::Json("invalid Telegram retry delay".into()));}
            if job.attempts<MAX_ATTEMPTS {("pending",Some(reason),Some(delay_seconds))}else{("failed",Some(reason),None)}
        },
    };
    if let Some(reason)=&reason {reason.validate()?;}
    let error=reason.map(|reason|json!(reason));let mut tx=pool.begin().await?;lock_budget(&mut tx).await?;
    if let Some(delay)=retry {
        let changed=sqlx::query("update telegram_delivery_outbox set lease_owner=null,lease_until=null,not_before=clock_timestamp()+$4::bigint*interval '1 second',last_error=$5 where id=$1 and lease_owner=$2 and lease_token=$3 and lease_until>clock_timestamp() and expires_at>=date_trunc('second',clock_timestamp()) and attempts<5")
            .bind(job.id).bind(worker).bind(job.lease_token).bind(delay).bind(&error).execute(&mut *tx).await?.rows_affected();
        if changed==1 {
            sqlx::query("update live_setups set telegram_delivery_status='pending',telegram_delivery_error=$2,telegram_delivery_updated_at=clock_timestamp() where id::bigint=$1")
                .bind(job.setup_id).bind(error).execute(&mut *tx).await?;tx.commit().await?;return Ok(true);
        }
    }
    let terminal=if retry.is_some(){"expired"}else{status};
    let deleted=sqlx::query("delete from telegram_delivery_outbox where id=$1 and lease_owner=$2 and lease_token=$3 and lease_until>clock_timestamp() returning setup_id")
        .bind(job.id).bind(worker).bind(job.lease_token).fetch_optional(&mut *tx).await?;
    if deleted.is_some() {
        sqlx::query("update live_setups set telegram_delivery_status=$2,telegram_delivery_error=$3,telegram_delivery_updated_at=clock_timestamp() where id::bigint=$1")
            .bind(job.setup_id).bind(terminal).bind(error).execute(&mut *tx).await?;
    }
    tx.commit().await?;Ok(deleted.is_some())
}
pub async fn configure_limits(pool:&PgPool,max_jobs:i64,max_bytes:i64)->Result<(),DbError> {
    if !(1..=100000).contains(&max_jobs)||!(65536..=1073741824).contains(&max_bytes) {return Err(DbError::Json("invalid Telegram count/byte budgets".into()));}
    sqlx::query("update telegram_delivery_budget set max_jobs=$1,max_bytes=$2 where id=1").bind(max_jobs).bind(max_bytes).execute(pool).await?;Ok(())
}

pub(crate) struct Enqueue<'a> {
    pub setup_id:i32,pub owner_id:&'a str,pub algorithm_id:&'a str,pub slot_id:Option<i64>,pub generation_id:Option<i64>,
    pub config_revision:Option<i64>,pub fixed_config:Option<&'a Value>,pub raw_close_unix:i64,pub expires_at_unix:i64,pub payload:&'a Value,
}
pub(crate) async fn enqueue(tx:&mut Transaction<'_,Postgres>,source:Enqueue<'_>)->Result<(),DbError> {
    // Existing live path holds WebPush budget first; Telegram never acquires it.
    // Config updates and Telegram claims take this same Telegram-budget-first order.
    lock_budget(tx).await?;
    let config=sqlx::query("select enabled,endpoint_url,revision from telegram_relay_config where id=1").fetch_one(&mut **tx).await?;
    if !config.try_get::<bool,_>("enabled")? {
        sqlx::query("update live_setups set telegram_delivery_status='disabled',telegram_delivery_updated_at=clock_timestamp() where id=$1")
            .bind(source.setup_id).execute(&mut **tx).await?;return Ok(());
    }
    sqlx::query("insert into telegram_delivery_outbox(setup_id,delivery_id,endpoint_url,settings_revision,owner_id,algorithm_id,slot_id,generation_id,config_revision,fixed_config,raw_close_unix,expires_at,payload) select $1,'live:'||instance_id::text||':'||$1::text,$2,$3,$4,$5,$6,$7,$8,$9,$10,to_timestamp($11),$12 from telegram_delivery_budget where id=1")
        .bind(source.setup_id).bind(config.try_get::<String,_>("endpoint_url")?).bind(config.try_get::<i64,_>("revision")?).bind(source.owner_id).bind(source.algorithm_id)
        .bind(source.slot_id).bind(source.generation_id).bind(source.config_revision).bind(source.fixed_config).bind(source.raw_close_unix)
        .bind(source.expires_at_unix as f64).bind(source.payload).execute(&mut **tx).await?;Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn public_reason_message_byte_bound_also_fits_web_utf16_budget() {
        for text in ["a".repeat(512),"я".repeat(256),"\u{1d11e}".repeat(128)] {
            assert_eq!(text.len(),512);assert!(text.encode_utf16().count()<=512);
            assert!(TelegramReason::new("bounded",&text).validate().is_ok());
        }
        for text in ["a".repeat(513),"я".repeat(257),"\u{1d11e}".repeat(129)] {
            assert!(text.len()>512);
            assert!(TelegramReason::new("bounded",&text).validate().is_err());
        }
    }
}
