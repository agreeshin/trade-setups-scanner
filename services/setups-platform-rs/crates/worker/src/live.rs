use crate::WorkerError;
use serde_json::{Value, json};
use setups_db::controls::instrument_list_by_kind;
use setups_db::push::{
    delete_push_subscription, get_or_create_vapid_keys, list_push_subscriptions,
    valid_push_endpoint,
};
use setups_db::runs::{get_replay_params, insert_live_setup, last_live_setup_open_time};
use setups_db::{settings_get, settings_set};
use setups_domain::{
    PushStatus, SPIKE_ALGORITHM_ID, SPIKE_ALGORITHM_VERSION, SpikeParams, SpikeSetup,
};
use setups_mdclient::{ApiCandleSource, CatalogInstrument, MarketDataClient};
use setups_replay::{CandleSource, CandleSourceQuery, ReplayRunOptions, replay_run};
use sha2::{Digest, Sha256};
use sqlx::postgres::PgPool;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::Notify;
use web_push::{
    ContentEncoding, HyperWebPushClient, SubscriptionInfo, VapidSignatureBuilder, WebPushClient,
    WebPushError, WebPushMessageBuilder,
};

pub fn evaluate_delivery_age(
    close_time_unix: i64,
    now_unix: i64,
    max_age_seconds: i64,
) -> (Option<i64>, bool) {
    let age = now_unix - close_time_unix;
    if age < 0 {
        return (None, false);
    }
    (Some(age), age <= max_age_seconds)
}

pub struct LiveScanResult {
    pub new_setups: Vec<SpikeSetup>,
    pub notified: u64,
    pub last_processed_unix: Option<i64>,
    pub ready: bool,
}

#[allow(clippy::too_many_arguments)]
pub async fn live_scan_once<S: CandleSource>(
    pool: &PgPool,
    source: &S,
    instruments: &[(String, String, String)],
    now_unix: i64,
    from_unix: Option<i64>,
    delivery_now_unix: Option<i64>,
    params: &SpikeParams,
    max_age_seconds: i64,
    stopped: Option<&AtomicBool>,
) -> Result<LiveScanResult, WorkerError> {
    let required_minutes = i64::from(params.baseline_long_days) * 1440;
    let from_unix = from_unix.unwrap_or(now_unix - 600);
    // Align the warmup start down to a whole minute like the TS scanner.
    let warmup_unix = (from_unix - required_minutes * 60).div_euclid(60) * 60;
    let params_value = serde_json::to_value(params).unwrap_or(Value::Null);
    let mut result = LiveScanResult {
        new_setups: Vec::new(),
        notified: 0,
        last_processed_unix: None,
        ready: false,
    };
    for (exchange, category, symbol) in instruments {
        if stopped.is_some_and(|flag| flag.load(Ordering::SeqCst)) {
            break;
        }
        let previous = last_live_setup_open_time(
            pool,
            exchange,
            category,
            symbol,
            SPIKE_ALGORITHM_ID,
            SPIKE_ALGORITHM_VERSION,
            &params_value,
        )
        .await?;
        let run = replay_run(ReplayRunOptions {
            source,
            from_unix,
            to_unix: now_unix,
            params: Some(params.clone()),
            last_setup_unix: previous,
            warmup_unix: Some(warmup_unix),
            cancel: None,
        })
        .await
        .map_err(|error| WorkerError::Source(error.to_string()))?;
        if run.cancelled {
            break;
        }
        result.last_processed_unix = run.last_processed_unix;
        result.ready = run.last_status == Some(PushStatus::Evaluated);
        for setup in &run.setups {
            let inserted = insert_live_setup(pool, exchange, category, symbol, setup).await?;
            if inserted.is_some() {
                let (_, eligible) = evaluate_delivery_age(
                    setup.close_time_unix,
                    delivery_now_unix.unwrap_or(now_unix),
                    max_age_seconds,
                );
                if eligible {
                    send_web_push_notifications(
                        pool,
                        symbol,
                        setup.close_time_unix,
                        max_age_seconds,
                    )
                    .await;
                    result.notified += 1;
                }
                result.new_setups.push(setup.clone());
            }
        }
    }
    Ok(result)
}

pub struct LiveHandle {
    stopped: Arc<AtomicBool>,
    notify: Arc<Notify>,
    task: tokio::task::JoinHandle<()>,
}

impl LiveHandle {
    pub async fn stop(self) {
        self.stopped.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
        let _ = self.task.await;
    }
}

pub fn start_live_runtime(pool: PgPool, market_url: String, market_key: String) -> LiveHandle {
    let stopped = Arc::new(AtomicBool::new(false));
    let notify = Arc::new(Notify::new());
    let task = tokio::spawn(live_loop(
        pool,
        market_url,
        market_key,
        stopped.clone(),
        notify.clone(),
    ));
    LiveHandle {
        stopped,
        notify,
        task,
    }
}

async fn live_loop(
    pool: PgPool,
    market_url: String,
    market_key: String,
    stopped: Arc<AtomicBool>,
    notify: Arc<Notify>,
) {
    let mut last_minute: i64 = 0;
    loop {
        tokio::select! {
            _ = notify.notified() => {
                if stopped.load(Ordering::SeqCst) {
                    break;
                }
            }
            _ = tokio::time::sleep(Duration::from_secs(15)) => {
                if stopped.load(Ordering::SeqCst) {
                    break;
                }
                live_tick(&pool, &market_url, &market_key, &stopped, &mut last_minute).await;
            }
        }
    }
    let _ = settings_set(
        &pool,
        "live-status",
        &json!({
            "running": false,
            "lastScanAt": Value::Null,
            "lastError": Value::Null,
        }),
    )
    .await;
}

async fn live_tick(
    pool: &PgPool,
    market_url: &str,
    market_key: &str,
    stopped: &AtomicBool,
    last_minute: &mut i64,
) {
    if live_tick_inner(pool, market_url, market_key, stopped, last_minute)
        .await
        .is_err()
    {
        let _ = settings_set(
            pool,
            "live-status",
            &json!({
                "running": false,
                "lastScanAt": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                "lastError": "Ошибка сканирования; проверьте сервис данных",
            }),
        )
        .await;
    }
}

async fn live_tick_inner(
    pool: &PgPool,
    market_url: &str,
    market_key: &str,
    stopped: &AtomicBool,
    last_minute: &mut i64,
) -> Result<(), WorkerError> {
    let live = settings_get(pool, "live").await?.unwrap_or(json!({}));
    if live.get("enabled") != Some(&Value::Bool(true)) {
        return Ok(());
    }
    let algorithm_stored = settings_get(pool, "algorithm:price_volume_spike")
        .await?
        .unwrap_or(json!({}));
    if algorithm_stored.get("enabled") == Some(&Value::Bool(false)) {
        return Ok(());
    }
    let params = get_replay_params(pool).await?;
    let now_unix = chrono::Utc::now().timestamp().div_euclid(60) * 60;
    if now_unix == *last_minute {
        return Ok(());
    }
    let started_at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let config_hash = {
        let payload = json!([
            SPIKE_ALGORITHM_VERSION,
            serde_json::to_value(&params).unwrap_or(Value::Null),
        ]);
        let mut hasher = Sha256::new();
        hasher.update(payload.to_string());
        format!("{:x}", hasher.finalize())
    };
    settings_set(
        pool,
        "live-status",
        &json!({
            "running": true,
            "lastScanAt": started_at.clone(),
            "lastError": Value::Null,
        }),
    )
    .await?;
    let list = instrument_list_by_kind(pool, "live").await?;
    let configured = settings_get(pool, "instrument-list:live")
        .await?
        .unwrap_or(json!({}))
        .get("configured")
        == Some(&Value::Bool(true));
    let selected: HashSet<String> = list
        .iter()
        .map(|row| format!("{}/{}/{}", row.exchange_code, row.category, row.symbol))
        .collect();
    let catalog = MarketDataClient::new(market_url, market_key)
        .instruments()
        .await
        .map_err(|error| WorkerError::Source(error.to_string()))?;
    let instruments: Vec<&CatalogInstrument> = catalog
        .iter()
        .filter(|item| {
            item.status == "active"
                && (!configured
                    || selected.contains(&format!(
                        "{}/{}/{}",
                        item.exchange, item.category, item.symbol
                    )))
        })
        .collect();
    if instruments.len() > 1000 {
        return Err(WorkerError::Source(
            "select at most 1000 live instruments".to_string(),
        ));
    }
    let notifications = settings_get(pool, "notifications")
        .await?
        .unwrap_or(json!({}));
    let max_age_seconds = notifications
        .get("maxAgeSeconds")
        .and_then(Value::as_i64)
        .unwrap_or(60);
    let mut instrument_states: Vec<Value> = Vec::new();
    let mut scanned: u64 = 0;
    let mut setup_count: u64 = 0;
    let mut stale: u64 = 0;
    let mut failed: u64 = 0;
    for item in &instruments {
        if stopped.load(Ordering::SeqCst) {
            break;
        }
        let still_enabled = settings_get(pool, "live")
            .await?
            .unwrap_or(json!({}))
            .get("enabled")
            == Some(&Value::Bool(true));
        if !still_enabled {
            break;
        }
        match scan_instrument(
            pool,
            market_url,
            market_key,
            item,
            &params,
            &config_hash,
            now_unix,
            max_age_seconds,
        )
        .await
        {
            ScanOutcome::WaitingData => {
                stale += 1;
                instrument_states.push(json!({
                    "symbol": item.symbol,
                    "state": "waiting_data",
                }));
            }
            ScanOutcome::Skipped {
                is_stale,
                last_closed,
            } => {
                if is_stale {
                    stale += 1;
                }
                let state = if is_stale { "stale" } else { "waiting_data" };
                instrument_states.push(json!({
                    "symbol": item.symbol,
                    "state": state,
                    "lastClosedUnix": last_closed,
                }));
            }
            ScanOutcome::Scanned {
                ready,
                is_stale,
                last_closed,
                new_setups,
                checkpoint_next,
            } => {
                scanned += 1;
                setup_count += new_setups;
                if is_stale {
                    stale += 1;
                }
                let state = if !ready {
                    "insufficient_history"
                } else if is_stale {
                    "stale"
                } else {
                    "ready"
                };
                instrument_states.push(json!({
                    "symbol": item.symbol,
                    "state": state,
                    "lastClosedUnix": last_closed,
                }));
                if let Some(next) = checkpoint_next {
                    let _ = settings_set(
                        pool,
                        &format!("live-checkpoint:{}", item.id),
                        &json!({ "configHash": config_hash, "nextUnix": next }),
                    )
                    .await;
                }
            }
            ScanOutcome::Failed => {
                failed += 1;
                instrument_states.push(json!({
                    "symbol": item.symbol,
                    "state": "error",
                    "error": "Ошибка чтения или расчёта",
                }));
            }
        }
    }
    *last_minute = now_unix;
    let last_error = if failed > 0 {
        json!(format!("Не обработано инструментов: {failed}"))
    } else {
        Value::Null
    };
    settings_set(
        pool,
        "live-status",
        &json!({
            "running": false,
            "lastScanAt": started_at,
            "lastError": last_error,
            "scanned": scanned,
            "setupCount": setup_count,
            "stale": stale,
            "instruments": instrument_states,
        }),
    )
    .await?;
    Ok(())
}

enum ScanOutcome {
    WaitingData,
    Skipped {
        is_stale: bool,
        last_closed: Option<i64>,
    },
    Scanned {
        ready: bool,
        is_stale: bool,
        last_closed: Option<i64>,
        new_setups: u64,
        checkpoint_next: Option<i64>,
    },
    Failed,
}

#[allow(clippy::too_many_arguments)]
async fn scan_instrument(
    pool: &PgPool,
    market_url: &str,
    market_key: &str,
    item: &CatalogInstrument,
    params: &SpikeParams,
    config_hash: &str,
    now_unix: i64,
    max_age_seconds: i64,
) -> ScanOutcome {
    let source = ApiCandleSource::new(market_url, market_key, item.id);
    let recent = match source
        .read(&CandleSourceQuery {
            from_unix: now_unix - 86400,
            to_unix: now_unix,
            after_unix: None,
            limit: Some(1440),
        })
        .await
    {
        Ok(page) => page,
        Err(_) => return ScanOutcome::Failed,
    };
    let Some(last_bar) = recent.bars.last() else {
        return ScanOutcome::WaitingData;
    };
    let latest_close = last_bar.open_time_unix + 60;
    let is_stale = now_unix - latest_close > 60;
    let checkpoint_key = format!("live-checkpoint:{}", item.id);
    let checkpoint = settings_get(pool, &checkpoint_key)
        .await
        .ok()
        .flatten()
        .unwrap_or(json!({}));
    let matching_next = checkpoint
        .get("configHash")
        .and_then(Value::as_str)
        .filter(|stored| *stored == config_hash)
        .and_then(|_| checkpoint.get("nextUnix").and_then(Value::as_i64));
    let from_unix = matching_next.unwrap_or(latest_close - 600);
    if from_unix >= latest_close {
        return ScanOutcome::Skipped {
            is_stale,
            last_closed: Some(latest_close),
        };
    }
    let to_unix = latest_close.min(from_unix + 86400);
    match live_scan_once(
        pool,
        &source,
        &[(
            item.exchange.clone(),
            item.category.clone(),
            item.symbol.clone(),
        )],
        to_unix,
        Some(from_unix),
        Some(now_unix),
        params,
        max_age_seconds,
        None,
    )
    .await
    {
        Ok(result) => ScanOutcome::Scanned {
            ready: result.ready,
            is_stale,
            last_closed: Some(latest_close),
            new_setups: result.new_setups.len() as u64,
            checkpoint_next: result.last_processed_unix.map(|value| value + 60),
        },
        Err(_) => ScanOutcome::Failed,
    }
}

pub async fn send_web_push_notifications(
    pool: &PgPool,
    symbol: &str,
    close_time_unix: i64,
    max_age_seconds: i64,
) {
    if max_age_seconds <= 0 {
        return;
    }
    let now = chrono::Utc::now().timestamp();
    if now < close_time_unix || max_age_seconds - (now - close_time_unix) < 0 {
        return;
    }
    let Ok((_, private_key)) = get_or_create_vapid_keys(pool).await else {
        eprintln!("[web-push] delivery unavailable");
        return;
    };
    let Ok(subscriptions) = list_push_subscriptions(pool, 100).await else {
        eprintln!("[web-push] delivery unavailable");
        return;
    };
    if subscriptions.is_empty() {
        return;
    }
    let payload = serde_json::to_vec(&json!({
        "symbol": symbol.chars().take(80).collect::<String>(),
        "timestamp": close_time_unix * 1000,
        "expiresAt": (close_time_unix + max_age_seconds) * 1000,
    }))
    .unwrap_or_default();
    let client = HyperWebPushClient::new();
    for row in &subscriptions {
        let remaining = max_age_seconds - (chrono::Utc::now().timestamp() - close_time_unix);
        if remaining < 0 || !valid_push_endpoint(&row.endpoint) {
            continue;
        }
        let info =
            SubscriptionInfo::new(row.endpoint.clone(), row.p256dh.clone(), row.auth.clone());
        let Ok(mut signature_builder) = VapidSignatureBuilder::from_base64(&private_key, &info)
        else {
            eprintln!("[web-push] delivery failed: invalid_keys");
            continue;
        };
        signature_builder.add_claim("sub", "mailto:admin@example.invalid");
        let Ok(signature) = signature_builder.build() else {
            continue;
        };
        let mut builder = WebPushMessageBuilder::new(&info);
        builder.set_payload(ContentEncoding::Aes128Gcm, &payload);
        builder.set_ttl(remaining.min(28 * 24 * 60 * 60) as u32);
        builder.set_vapid_signature(signature);
        let Ok(message) = builder.build() else {
            continue;
        };
        match client.send(message).await {
            Ok(()) => {}
            Err(WebPushError::EndpointNotFound(_)) | Err(WebPushError::EndpointNotValid(_)) => {
                let _ = delete_push_subscription(pool, &row.endpoint).await;
            }
            Err(_) => eprintln!("[web-push] delivery failed: network"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delivery_age_boundaries_match_ts() {
        assert_eq!(evaluate_delivery_age(1000, 900, 60), (None, false));
        assert_eq!(evaluate_delivery_age(1000, 1000, 60), (Some(0), true));
        assert_eq!(evaluate_delivery_age(1000, 1060, 60), (Some(60), true));
        assert_eq!(evaluate_delivery_age(1000, 1061, 60), (Some(61), false));
    }

    #[test]
    fn warmup_start_aligns_down_to_whole_minute() {
        let aligned = (61i64 - 60).div_euclid(60) * 60;
        assert_eq!(aligned, 0);
        let exact = (120i64 - 60).div_euclid(60) * 60;
        assert_eq!(exact, 60);
        let negative = (1i64 - 120).div_euclid(60) * 60;
        assert_eq!(negative, -120);
    }
}
