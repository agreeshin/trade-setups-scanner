//! Bounded durable delivery executor, independent of detector/fetch progress.
use super::*;
use setups_db::continuous::outbox::{self, NotificationJob, NotificationOutcome};
use setups_db::push::{
    delete_push_subscription, get_or_create_vapid_keys, list_push_subscriptions,
    valid_push_endpoint, PushSubscriptionRow,
};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;
use web_push::{
    ContentEncoding, HyperWebPushClient, SubscriptionInfo, VapidSignatureBuilder, WebPushClient,
    WebPushError, WebPushMessageBuilder,
};

const DELIVERY_WORKERS: usize = 4;
const JOB_TIMEOUT: Duration = Duration::from_secs(45);
const SEND_TIMEOUT: Duration = Duration::from_secs(3);
const SUBSCRIBER_CONCURRENCY: usize = 8;

pub(super) async fn run(pool: PgPool, stopped: Arc<AtomicBool>, notify: Arc<Notify>) {
    let mut tasks = tokio::task::JoinSet::new();
    for index in 0..DELIVERY_WORKERS {
        let pool = pool.clone();
        let stopped = stopped.clone();
        let notify = notify.clone();
        tasks.spawn(async move {
            let worker = format!("adaptive-delivery-{}-{index}", std::process::id());
            while !stopped.load(Ordering::SeqCst) {
                let result = deliver_once(&pool, &worker, &stopped).await;
                if let Err(error) = result {
                    eprintln!("[adaptive-delivery] failed: {}", error.to_string().chars().take(240).collect::<String>());
                }
                tokio::select! {
                    _ = notify.notified() => {},
                    _ = tokio::time::sleep(Duration::from_millis(500)) => {},
                }
            }
        });
    }
    while let Some(result) = tasks.join_next().await {
        if let Err(error) = result {
            eprintln!("[adaptive-delivery] executor failed: {error}");
        }
    }
}

async fn deliver_once(pool: &PgPool, worker: &str, stopped: &Arc<AtomicBool>) -> Result<(), WorkerError> {
    // Claim only work that this executor can start immediately; no RAM queue.
    for job in outbox::claim_notifications(pool, worker, 1).await? {
        let outcome = if stopped.load(Ordering::SeqCst) {
            NotificationOutcome::Retry { delay_seconds: 1, error: "executor stopping".into() }
        } else {
            match tokio::time::timeout(JOB_TIMEOUT, send_job(pool, worker, &job, stopped.clone())).await {
                Ok(Ok(outcome)) => outcome,
                Ok(Err(error)) => NotificationOutcome::Retry {
                    delay_seconds: 2_i64.pow(job.attempts.clamp(0, 5) as u32),
                    error: error.to_string().chars().take(240).collect(),
                },
                Err(_) => NotificationOutcome::Retry { delay_seconds: 2, error: "delivery timeout".into() },
            }
        };
        outbox::finish_notification(pool, worker, &job, outcome).await?;
    }
    Ok(())
}

fn payload(job: &NotificationJob) -> Result<Vec<u8>, WorkerError> {
    let instrument: AdaptiveInstrument = serde_json::from_value(job.payload["instrument"].clone())?;
    let raw: SpikeSetup = serde_json::from_value(job.payload["raw"].clone())?;
    let recommendation = &job.payload["recommendation"];
    let external=job.payload["externalName"].as_str().ok_or_else(||invalid("outbox external name missing"))?;
    if raw.close_time_unix != job.raw_close_unix || raw.algorithm_id!=job.algorithm_id
        || external.trim().is_empty() || external.chars().count()>120 || external.len()>480 || external.chars().any(char::is_control)
        || serde_json::to_vec(recommendation)?.len() > MAX_SNAPSHOT_BYTES
    {
        return Err(invalid("outbox payload differs from saved setup"));
    }
    let message=if job.slot_id.is_none() && job.generation_id.is_none() && job.config_revision.is_none()
        && recommendation.is_null() && let Some(fence)=job.fixed_config.as_ref() {
        if fence["algorithmVersion"]!=raw.algorithm_version || fence["params"]!=serde_json::to_value(&raw.params)? {
            return Err(invalid("fixed delivery config differs from raw setup"));
        }
        format!("WATCH {} · {}\n{} · {}",instrument.symbol,external,instrument.exchange,
            chrono::DateTime::from_timestamp(raw.close_time_unix,0).ok_or_else(||invalid("invalid raw close"))?.format("%Y-%m-%d %H:%M:%S UTC"))
    } else if job.slot_id.is_some() && job.generation_id.is_some() && job.config_revision.is_some()
        && job.fixed_config.is_none() && recommendation["externalName"]==job.payload["externalName"] {
        recommendation_message(&instrument,&raw,recommendation)?
    } else {return Err(invalid("outbox delivery mode is inconsistent"));};
    let payload = serde_json::to_vec(&json!({
        "symbol":instrument.symbol,"setupId":job.setup_id,"slotId":job.slot_id,
        "tag":format!("setup-{}",job.setup_id),"timestamp":job.raw_close_unix*1000,
        "expiresAt":job.expires_at_unix*1000,
        "recommendation":if recommendation.is_null(){Value::Null}else{json!({"direction":recommendation["direction"]})},"message":message,
    }))?;
    if payload.len() > 3800 {
        return Err(invalid("adaptive notification exceeds push payload budget"));
    }
    Ok(payload)
}

async fn send_job(
    pool: &PgPool,
    worker: &str,
    job: &NotificationJob,
    stopped: Arc<AtomicBool>,
) -> Result<NotificationOutcome, WorkerError> {
    if chrono::Utc::now().timestamp() >= job.expires_at_unix
        || !outbox::notification_permitted(pool, worker, job).await?
    {
        return Ok(NotificationOutcome::Skipped);
    }
    let payload = match payload(job) {
        Ok(payload) => payload,
        Err(_) => return Ok(NotificationOutcome::Failed { error: "invalid saved delivery payload".into() }),
    };
    let subscriptions = list_push_subscriptions(pool, 100).await?;
    if subscriptions.is_empty() {
        return Ok(NotificationOutcome::Skipped);
    }
    let (_, private_key) = get_or_create_vapid_keys(pool).await?;
    let context=Arc::new(SendContext {pool:pool.clone(),worker:worker.into(),job:job.clone(),payload,
        private_key,stopped,client:HyperWebPushClient::new()});
    let mut tasks=tokio::task::JoinSet::new();
    for row in subscriptions {
        if !valid_push_endpoint(&row.endpoint) {continue;}
        let context=context.clone();
        tasks.spawn(async move {context.send(row).await});
        if tasks.len()>=SUBSCRIBER_CONCURRENCY && !send_result(tasks.join_next().await.unwrap())? {
            return Ok(NotificationOutcome::Skipped);
        }
    }
    while let Some(result)=tasks.join_next().await {
        if !send_result(result)? {return Ok(NotificationOutcome::Skipped);}
    }
    Ok(NotificationOutcome::Sent)
}

fn send_result(result:Result<Result<bool,WorkerError>,tokio::task::JoinError>)->Result<bool,WorkerError> {
    result.map_err(|_|invalid("push subscriber task failed"))?
}
struct SendContext {
    pool:PgPool,worker:String,job:NotificationJob,payload:Vec<u8>,private_key:String,
    stopped:Arc<AtomicBool>,client:HyperWebPushClient,
}
impl SendContext {
    async fn send(&self,row:PushSubscriptionRow)->Result<bool,WorkerError> {
        let info=SubscriptionInfo::new(row.endpoint.clone(),row.p256dh,row.auth);
        let mut signature=VapidSignatureBuilder::from_base64(&self.private_key,&info).map_err(|_|invalid("invalid push signing keys"))?;
        signature.add_claim("sub","mailto:admin@example.invalid");
        let signature=signature.build().map_err(|_|invalid("invalid push signature"))?;
        if self.stopped.load(Ordering::SeqCst) || chrono::Utc::now().timestamp()>=self.job.expires_at_unix
            || !outbox::renew_notification(&self.pool,&self.worker,&self.job).await?
            || !outbox::notification_permitted(&self.pool,&self.worker,&self.job).await? {return Ok(false);}
        let remaining=self.job.expires_at_unix-chrono::Utc::now().timestamp();
        if remaining<=0 {return Ok(false);}
        let mut builder=WebPushMessageBuilder::new(&info);
        builder.set_payload(ContentEncoding::Aes128Gcm,&self.payload);
        builder.set_ttl(remaining.min(28*24*60*60) as u32);builder.set_vapid_signature(signature);
        let notification=builder.build().map_err(|_|invalid("invalid push message"))?;
        match tokio::time::timeout(SEND_TIMEOUT,self.client.send(notification)).await {
            Ok(Ok(()))=>Ok(true),
            Ok(Err(WebPushError::EndpointNotFound(_))) | Ok(Err(WebPushError::EndpointNotValid(_)))=>{
                delete_push_subscription(&self.pool,&row.endpoint).await?;Ok(true)
            },
            _=>Err(invalid("push transport failed or timed out")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixed()->NotificationJob {
        let raw=SpikeSetup {algorithm_id:"price_volume_spike".into(),algorithm_version:"1.1.1".into(),
            open_time_unix:86340,close_time_unix:86400,close_price:"100".into(),params:AlgorithmParams::default(),details:None};
        NotificationJob {id:1,setup_id:2,owner_id:"admin".into(),algorithm_id:raw.algorithm_id.clone(),slot_id:None,
            generation_id:None,config_revision:None,fixed_config:Some(json!({"algorithmVersion":raw.algorithm_version,"params":raw.params})),
            lease_token:1,raw_close_unix:raw.close_time_unix,expires_at_unix:86460,attempts:1,
            payload:json!({"instrument":{"exchange":"bybit","category":"linear","symbol":"BTCUSDT"},"raw":raw,
                "recommendation":null,"externalName":"Saved fixed name"})}
    }
    fn adaptive()->NotificationJob {
        let mut job=fixed();job.slot_id=Some(7);job.generation_id=Some(3);job.config_revision=Some(1);job.fixed_config=None;
        job.payload["externalName"]="Saved adaptive name".into();
        job.payload["recommendation"]=json!({"direction":"short","tpPct":14,"slPct":4,"rawR":0.32,"meanR":0.21,"robustScore":0.29,
            "timeShiftMinutes":3,"priceChangePct":1,"clusterEnabled":false,"clusterGapMinutes":null,
            "provenance":{"algorithmId":"price_volume_spike","algorithmVersion":"1.1.1","optimizerVersion":"plan-a-continuous-1","seed":42,
                "snapshotHash":"fixture","capturedAt":"2026-11-12T00:00:00Z","fromUnix":0,"toUnix":86400,"observationCutoff":86400},
            "studyId":42,"generationId":3,"slotId":7,"ownerId":"admin","externalName":"Saved adaptive name","params":job.payload["raw"]["params"],
            "rawSignalAt":86400,"recommendedEntryAt":86580});
        job
    }
    #[test]
    fn fixed_job_with_null_recommendation_builds_watch_not_adaptive_payload() {
        let value:Value=serde_json::from_slice(&payload(&fixed()).unwrap()).unwrap();
        assert!(value["message"].as_str().unwrap().starts_with("WATCH BTCUSDT"));
        assert!(value["message"].as_str().unwrap().contains("1970-01-02 00:00:00 UTC"));
        assert!(value["recommendation"].is_null());assert!(value["slotId"].is_null());assert_eq!(value["expiresAt"],86460000);
    }
    #[test]
    fn adaptive_job_uses_immutable_saved_recommendation_and_seconds() {
        let value:Value=serde_json::from_slice(&payload(&adaptive()).unwrap()).unwrap();
        assert_eq!(value["recommendation"]["direction"],"short");assert_eq!(value["slotId"],7);
        let text=value["message"].as_str().unwrap();
        assert!(text.contains("SHORT"));assert!(text.contains("1970-01-02 00:03:00 UTC"));assert!(text.contains("Saved adaptive name"));
    }
    #[test]
    fn mixed_delivery_modes_mismatched_snapshots_and_oversized_payloads_fail() {
        let mut job=fixed();job.slot_id=Some(1);assert!(payload(&job).is_err());
        let mut job=fixed();job.fixed_config.as_mut().unwrap()["algorithmVersion"]="wrong".into();assert!(payload(&job).is_err());
        let mut job=adaptive();job.payload["externalName"]="renamed".into();assert!(payload(&job).is_err());
        let mut job=adaptive();job.raw_close_unix+=60;assert!(payload(&job).is_err());
        let mut job=adaptive();job.payload["recommendation"]["padding"]="x".repeat(MAX_SNAPSHOT_BYTES).into();assert!(payload(&job).is_err());
    }
}
