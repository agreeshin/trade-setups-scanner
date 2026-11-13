use super::*;
use serde_json::json;
use setups_db::continuous::{self, ActiveAdaptiveSlot, AdaptiveSetupWrite};
use setups_db::push::{
    delete_push_subscription, get_or_create_vapid_keys, list_push_subscriptions,
    valid_push_endpoint,
};
use setups_db::settings_get;
use setups_domain::{AdaptiveRecommendationSnapshot, RecommendationDirection};
use setups_mdclient::{ApiCandleSource, CatalogInstrument, MarketDataClient};
pub use setups_optimizer::ContinuousBootstrap as AdaptiveLiveBootstrap;
use setups_optimizer::{ContinuousInstrumentBootstrap, StudyRequest};
use sqlx::PgPool;
use std::collections::{BTreeSet, HashSet};
use web_push::{
    ContentEncoding, HyperWebPushClient, SubscriptionInfo, VapidSignatureBuilder, WebPushClient,
    WebPushError, WebPushMessageBuilder,
};

mod text;
pub use text::recommendation_message;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RecommendationEnvelope {
    #[serde(flatten)]
    snapshot: AdaptiveRecommendationSnapshot,
    study_id: i64,
    generation_id: i64,
    slot_id: i64,
    owner_id: String,
    external_name: String,
    params: AlgorithmParams,
}

struct LiveProfile {
    fence: AdaptiveFence,
    params: AlgorithmParams,
    bootstrap: AdaptiveLiveBootstrap,
    recommendation: Value,
    cluster_gap_minutes: Option<u32>,
    live_from_close_unix: i64,
}

impl LiveProfile {
    fn parse(slot: &ActiveAdaptiveSlot) -> Result<Self, WorkerError> {
        let params: AlgorithmParams = serde_json::from_value(slot.params.clone())?;
        let snapshot: AdaptiveRecommendationSnapshot = serde_json::from_value(
            slot.result
                .get("recommendation")
                .cloned()
                .ok_or_else(|| invalid("missing result.recommendation"))?,
        )?;
        let bootstrap: AdaptiveLiveBootstrap = serde_json::from_value(
            slot.result
                .get("bootstrap")
                .cloned()
                .ok_or_else(|| invalid("missing result.bootstrap"))?,
        )?;
        let spec: StudyRequest = serde_json::from_value(slot.spec.clone())?;
        let expected: BTreeSet<_> = spec.instruments.iter().cloned().collect();
        let actual: BTreeSet<_> = bootstrap
            .instruments
            .iter()
            .map(|raw| raw.instrument.clone())
            .collect();
        if params.algorithm_id() != slot.algorithm_id
            || params.algorithm_version() != spec.algorithm_version
            || snapshot.provenance.algorithm_id != slot.algorithm_id
            || snapshot.provenance.algorithm_version != spec.algorithm_version
            || snapshot.provenance.from_unix != spec.from_unix
            || snapshot.provenance.to_unix != spec.to_unix
            || snapshot.provenance.observation_cutoff != bootstrap.observation_cutoff
            || snapshot.provenance.snapshot_hash != bootstrap.provenance.snapshot_hash
            || snapshot.provenance.optimizer_version != bootstrap.provenance.optimizer_version
            || snapshot.provenance.seed != bootstrap.provenance.seed
            || snapshot.provenance.captured_at != bootstrap.provenance.captured_at
            || snapshot.provenance.seed != spec.seed
            || serde_json::to_value(snapshot.direction)?.as_str() != Some(slot.direction.as_str())
            || slot.analytics["clusterEnabled"].as_bool() != Some(snapshot.cluster_enabled)
            || slot.analytics["timeShiftMinutes"].as_i64() != Some(snapshot.time_shift_minutes)
            || slot.analytics["priceChangePct"].as_f64() != snapshot.price_change_pct
            || (snapshot.cluster_enabled
                && slot.analytics["clusterGapMinutes"].as_i64() != snapshot.cluster_gap_minutes)
            || slot.selected_cell["tpPct"].as_f64() != Some(snapshot.tp_pct)
            || slot.selected_cell["slPct"].as_f64() != Some(snapshot.sl_pct)
            || slot.selected_cell["rawR"].as_f64() != Some(snapshot.raw_r)
            || bootstrap.export_from_unix != spec.from_unix
            || bootstrap.export_to_unix != spec.to_unix
            || snapshot.mean_r != slot.mean_expectancy
            || ![
                snapshot.tp_pct,
                snapshot.sl_pct,
                snapshot.raw_r,
                snapshot.mean_r,
                snapshot.robust_score,
            ]
            .iter()
            .all(|number| number.is_finite())
            || !(0.0 < snapshot.tp_pct && snapshot.tp_pct <= 100.0)
            || !(0.0 < snapshot.sl_pct && snapshot.sl_pct <= 100.0)
            || !(0..=43200).contains(&snapshot.time_shift_minutes)
            || snapshot
                .price_change_pct
                .is_some_and(|pct| !pct.is_finite() || !(-100.0..=1000.0).contains(&pct))
            || slot.external_name.trim().is_empty()
            || slot.external_name.chars().count() > 120
            || slot.external_name.len() > 480
            || slot.external_name.chars().any(char::is_control)
            || expected != actual
            || actual.len() != bootstrap.instruments.len()
            || actual.len() > 1000
            || bootstrap.next_open_unix.rem_euclid(60) != 0
            || bootstrap.next_open_unix < 0
            || bootstrap.next_open_unix > bootstrap.observation_cutoff
            || bootstrap.detector_warmup_from_unix.rem_euclid(60) != 0
            || bootstrap.emit_from_close_unix != bootstrap.observation_cutoff + 60
            || bootstrap.instruments.iter().any(|raw| {
                raw.last_raw_cluster_time.is_some_and(|time| {
                    time < 60 || time > bootstrap.next_open_unix || time % 60 != 0
                }) || raw.last_raw_open_unix.map(|time| time + 60) != raw.last_raw_cluster_time
            })
        {
            return Err(invalid(
                "accepted profile/bootstrap identity or values mismatch",
            ));
        }
        let cluster_gap_minutes = if snapshot.cluster_enabled {
            Some(
                u32::try_from(
                    snapshot
                        .cluster_gap_minutes
                        .filter(|gap| (1..=10080).contains(gap))
                        .ok_or_else(|| invalid("missing causal cluster gap"))?,
                )
                .map_err(|_| invalid("invalid causal gap"))?,
            )
        } else {
            None
        };
        let recommendation = serde_json::to_value(RecommendationEnvelope {
            snapshot,
            study_id: slot.study_id,
            generation_id: slot.generation_id,
            slot_id: slot.slot_id,
            owner_id: slot.owner_id.clone(),
            external_name: slot.external_name.clone(),
            params: params.clone(),
        })?;
        let live_from_close_unix = bootstrap.emit_from_close_unix;
        Ok(Self {
            fence: AdaptiveFence {
                owner_id: slot.owner_id.clone(),
                algorithm_id: slot.algorithm_id.clone(),
                slot_id: slot.slot_id,
                generation_id: slot.generation_id,
                config_revision: slot.config_revision,
            },
            params,
            bootstrap,
            recommendation,
            cluster_gap_minutes,
            live_from_close_unix,
        })
    }
}

pub struct PgAdaptiveStore<'a> {
    pub pool: &'a PgPool,
    pub slot: &'a ActiveAdaptiveSlot,
}

impl AdaptiveStore for PgAdaptiveStore<'_> {
    async fn current(&self, _: &AdaptiveFence) -> Result<bool, WorkerError> {
        Ok(continuous::adaptive_notification_permitted(self.pool, self.slot).await?)
    }

    async fn commit_page(
        &self,
        _: &AdaptiveFence,
        instrument: &AdaptiveInstrument,
        expected: Option<&AdaptiveCheckpoint>,
        checkpoint: &AdaptiveCheckpoint,
        setups: &mut [AdaptiveSetup],
    ) -> Result<Option<Vec<AdaptiveInserted>>, WorkerError> {
        // Capture the editable name at creation/persistence, not when a long
        // multi-instrument tick started. The same materialized value is used
        // for the DB row, immutable JSON and subsequent notification.
        let mut captured = self.slot.clone();
        let settings =
            settings_get(self.pool, &format!("algorithm:{}", self.slot.algorithm_id)).await?;
        captured.external_name = settings
            .as_ref()
            .and_then(|value| value["externalName"].as_str())
            .map(str::to_owned)
            .unwrap_or_else(|| match self.slot.algorithm_id.as_str() {
                setups_domain::SPIKE_ALGORITHM_ID => "Всплеск цены и оборота".into(),
                setups_domain::SPIKE_DROP_ALGORITHM_ID => "Падение цены и всплеск оборота".into(),
                _ => self.slot.external_name.clone(),
            });
        if captured.external_name.trim().is_empty()
            || captured.external_name.chars().count() > 120
            || captured.external_name.len() > 480
            || captured.external_name.chars().any(char::is_control)
        {
            return Err(invalid("invalid external algorithm name at setup creation"));
        }
        for setup in setups.iter_mut() {
            setup.recommendation["externalName"] = captured.external_name.clone().into();
        }
        let writes: Vec<_> = setups
            .iter()
            .map(|setup| AdaptiveSetupWrite {
                exchange_code: instrument.exchange.clone(),
                category: instrument.category.clone(),
                symbol: instrument.symbol.clone(),
                setup: setup.raw.clone(),
                recommendation: setup.recommendation.clone(),
            })
            .collect();
        let expected = expected
            .map(|state| {
                Ok::<_, WorkerError>(continuous::AdaptiveCheckpoint {
                    generation_id: state.generation_id,
                    state: serde_json::to_value(state)?,
                })
            })
            .transpose()?;
        let commit = continuous::commit_adaptive_scan_cas(
            self.pool,
            &captured,
            &instrument_key(instrument),
            expected.as_ref(),
            &serde_json::to_value(checkpoint)?,
            &writes,
        )
        .await?;
        Ok(commit.applied.then_some(commit.inserted))
    }

    async fn notify(
        &self,
        _: &AdaptiveFence,
        setup_id: i64,
        instrument: &AdaptiveInstrument,
        setup: &AdaptiveSetup,
        max_age_seconds: i64,
    ) -> Result<(), WorkerError> {
        send_push(
            self.pool,
            self.slot,
            setup_id,
            instrument,
            setup,
            max_age_seconds,
        )
        .await
    }
}

fn instrument_key(instrument: &AdaptiveInstrument) -> String {
    format!(
        "{}/{}/{}",
        instrument.exchange, instrument.category, instrument.symbol
    )
}

pub(crate) async fn adaptive_live_loop(
    pool: PgPool,
    market_url: String,
    market_key: String,
    stopped: std::sync::Arc<AtomicBool>,
    notify: std::sync::Arc<tokio::sync::Notify>,
) {
    let mut last_minute = 0;
    loop {
        tokio::select! {
            _=notify.notified()=> { if stopped.load(Ordering::SeqCst) { break; } },
            _=tokio::time::sleep(std::time::Duration::from_secs(15))=> {
                if stopped.load(Ordering::SeqCst) { break; }
                if let Err(error)=adaptive_live_tick(&pool,&market_url,&market_key,&stopped,&mut last_minute).await {
                    eprintln!("[adaptive-live] tick failed: {}",error.to_string().chars().take(240).collect::<String>());
                }
            }
        }
    }
}

fn listed_study_instruments<'a>(
    bootstrap: &'a AdaptiveLiveBootstrap,
    catalog: &'a [CatalogInstrument],
) -> Result<Vec<(&'a ContinuousInstrumentBootstrap, &'a CatalogInstrument)>, WorkerError> {
    let mut seen = HashSet::new();
    let mut selected = Vec::new();
    for raw in &bootstrap.instruments {
        let key = (
            &raw.instrument.exchange,
            &raw.instrument.category,
            &raw.instrument.symbol,
        );
        if !seen.insert(key) {
            return Err(invalid("duplicate bootstrap instrument"));
        }
        let mut matches = catalog.iter().filter(|item| {
            item.status == "active"
                && item.exchange == raw.instrument.exchange
                && item.category == raw.instrument.category
                && item.symbol == raw.instrument.symbol
        });
        if let Some(item) = matches.next() {
            if matches.next().is_some() {
                return Err(invalid("ambiguous active listing"));
            }
            selected.push((raw, item));
        }
    }
    selected.sort_by(|(a, _), (b, _)| a.instrument.cmp(&b.instrument));
    Ok(selected)
}

/// Called by the existing live owner even when every fixed detector is disabled.
/// Separate minute/checkpoint namespaces keep fixed and other slots independent.
pub async fn adaptive_live_tick(
    pool: &PgPool,
    market_url: &str,
    market_key: &str,
    stopped: &AtomicBool,
    last_minute: &mut i64,
) -> Result<(), WorkerError> {
    if stopped.load(Ordering::SeqCst)
        || settings_get(pool, "live").await?.unwrap_or(json!({}))["enabled"] != json!(true)
    {
        return Ok(());
    }
    let now_unix = chrono::Utc::now().timestamp().div_euclid(60) * 60;
    if now_unix == *last_minute {
        return Ok(());
    }
    let slots = continuous::active_adaptive_slots(pool, continuous::ADMIN_OWNER).await?;
    if slots.is_empty() {
        *last_minute = now_unix;
        return Ok(());
    }
    let catalog = MarketDataClient::new(market_url, market_key)
        .instruments()
        .await
        .map_err(|error| WorkerError::Source(error.to_string()))?;
    let max_age = settings_get(pool, "notifications")
        .await?
        .unwrap_or(json!({}))["maxAgeSeconds"]
        .as_i64()
        .unwrap_or(60);
    for slot in slots {
        if stopped.load(Ordering::SeqCst) {
            break;
        }
        if let Err(error) = scan_slot(
            pool, market_url, market_key, &slot, &catalog, now_unix, max_age, stopped,
        )
        .await
        {
            // One malformed/stale profile must not starve other adaptive slots.
            eprintln!(
                "[adaptive-live] slot={} generation={} failed: {}",
                slot.slot_id,
                slot.generation_id,
                error.to_string().chars().take(240).collect::<String>()
            );
        }
    }
    *last_minute = now_unix;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn scan_slot(
    pool: &PgPool,
    market_url: &str,
    market_key: &str,
    slot: &ActiveAdaptiveSlot,
    catalog: &[CatalogInstrument],
    now_unix: i64,
    max_age_seconds: i64,
    stopped: &AtomicBool,
) -> Result<(), WorkerError> {
    let profile = LiveProfile::parse(slot)?;
    let store = PgAdaptiveStore { pool, slot };
    for (seed, item) in listed_study_instruments(&profile.bootstrap, catalog)? {
        if stopped.load(Ordering::SeqCst) || !store.current(&profile.fence).await? {
            break;
        }
        let instrument = AdaptiveInstrument {
            exchange: item.exchange.clone(),
            category: item.category.clone(),
            symbol: item.symbol.clone(),
        };
        let stored =
            continuous::get_adaptive_checkpoint(pool, slot.slot_id, &instrument_key(&instrument))
                .await?;
        let checkpoint = stored
            .map(|row| {
                let value: AdaptiveCheckpoint = serde_json::from_value(row.state)?;
                if value.generation_id != row.generation_id {
                    return Err(invalid("checkpoint generation differs from stored fence"));
                }
                Ok(value)
            })
            .transpose()?;
        let initial = AdaptiveCheckpoint {
            generation_id: slot.generation_id,
            next_open_unix: profile.bootstrap.next_open_unix,
            last_raw_open_unix: seed.last_raw_open_unix,
            last_raw_cluster_time: seed.last_raw_cluster_time,
        };
        let from = checkpoint
            .as_ref()
            .filter(|c| c.generation_id == slot.generation_id)
            .unwrap_or(&initial)
            .next_open_unix;
        if from >= now_unix {
            continue;
        }
        let source = ApiCandleSource::new(market_url, market_key, item.id);
        let result = scan_adaptive_pages(
            &source,
            &store,
            AdaptiveScanOptions {
                fence: &profile.fence,
                instrument: &instrument,
                params: &profile.params,
                recommendation: &profile.recommendation,
                bootstrap_from_unix: profile.bootstrap.detector_warmup_from_unix,
                emit_from_close_unix: profile.live_from_close_unix,
                observation_close_unix: now_unix.min(from + 86400),
                cluster_gap_minutes: profile.cluster_gap_minutes,
                initial_state: Some(initial),
                checkpoint,
                max_age_seconds,
                stopped,
            },
        )
        .await?;
        if result.fenced {
            break;
        }
    }
    Ok(())
}

async fn send_push(
    pool: &PgPool,
    slot: &ActiveAdaptiveSlot,
    setup_id: i64,
    instrument: &AdaptiveInstrument,
    setup: &AdaptiveSetup,
    max_age_seconds: i64,
) -> Result<(), WorkerError> {
    let close = setup.raw.close_time_unix;
    if max_age_seconds <= 0
        || !crate::live::evaluate_delivery_age(
            close,
            chrono::Utc::now().timestamp(),
            max_age_seconds,
        )
        .1
    {
        return Ok(());
    }
    let message = recommendation_message(instrument, &setup.raw, &setup.recommendation)?;
    let payload = serde_json::to_vec(&json!({
        "symbol":instrument.symbol, "setupId":setup_id,"slotId":slot.slot_id,"tag":format!("setup-{setup_id}"),
        "timestamp":close*1000,"expiresAt":(close+max_age_seconds)*1000,
        "recommendation":{"direction":setup.recommendation["direction"]},"message":message,
    }))?;
    // RFC8291 single-record payload budget. Never truncate a recommendation into
    // a different financial condition; the full saved snapshot remains in UI.
    if payload.len() > 3800 {
        return Err(invalid("adaptive notification exceeds push payload budget"));
    }
    let (_, private_key) = get_or_create_vapid_keys(pool).await?;
    let subscriptions = list_push_subscriptions(pool, 100).await?;
    let client = HyperWebPushClient::new();
    for row in subscriptions {
        if !valid_push_endpoint(&row.endpoint) {
            continue;
        }
        let info = SubscriptionInfo::new(row.endpoint.clone(), row.p256dh, row.auth);
        let mut signature = VapidSignatureBuilder::from_base64(&private_key, &info)
            .map_err(|_| invalid("invalid push signing keys"))?;
        signature.add_claim("sub", "mailto:admin@example.invalid");
        let signature = signature
            .build()
            .map_err(|_| invalid("invalid push signature"))?;
        // Fresh owner/generation/global permission and age for each subscriber,
        // after signing and immediately before handing the message to transport.
        if !continuous::adaptive_notification_permitted(pool, slot).await? {
            break;
        }
        let (age, eligible) = crate::live::evaluate_delivery_age(
            close,
            chrono::Utc::now().timestamp(),
            max_age_seconds,
        );
        if !eligible {
            break;
        }
        let mut builder = WebPushMessageBuilder::new(&info);
        builder.set_payload(ContentEncoding::Aes128Gcm, &payload);
        builder.set_ttl((max_age_seconds - age.unwrap_or(0)).min(28 * 24 * 60 * 60) as u32);
        builder.set_vapid_signature(signature);
        let notification = builder
            .build()
            .map_err(|_| invalid("invalid push message"))?;
        match client.send(notification).await {
            Ok(()) => {}
            Err(WebPushError::EndpointNotFound(_)) | Err(WebPushError::EndpointNotValid(_)) => {
                delete_push_subscription(pool, &row.endpoint).await?;
            }
            Err(_) => eprintln!("[adaptive-live] setup={setup_id} push delivery failed: network"),
        }
    }
    Ok(())
}
