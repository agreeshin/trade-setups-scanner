//! Streaming adaptive live execution. A page and one detector are owned by a
//! single instrument scan; no signal history or pending future entries is kept.
use crate::WorkerError;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use setups_analytics::CausalCluster;
use setups_db::continuous::AdaptiveInserted;
use setups_detector::PriceVolumeSpikeDetector;
use setups_domain::{AlgorithmParams, DetectorBar, SpikeSetup};
use setups_replay::{CandleSource, CandleSourceQuery};
use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicBool, Ordering};

mod runtime;
mod state;
pub use state::{AdaptiveRuntime, AdaptiveRuntimeLimits};
pub(crate) use runtime::adaptive_live_loop;
pub use runtime::{AdaptiveLiveBootstrap, AdaptiveLiveRuntime, PgAdaptiveStore, adaptive_live_tick};

pub const ADAPTIVE_PAGE_LIMIT: usize = 256;
const MAX_SNAPSHOT_BYTES: usize = 16 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdaptiveFence {
    pub owner_id: String,
    pub algorithm_id: String,
    pub slot_id: i64,
    pub generation_id: i64,
    pub config_revision: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdaptiveInstrument {
    pub exchange: String,
    pub category: String,
    pub symbol: String,
}

/// Detector cooldown uses OPEN times; raw cluster gaps use CLOSE times. State
/// is per slot/instrument, never fixed-live state or last *published* signal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AdaptiveCheckpoint {
    pub generation_id: i64,
    pub next_open_unix: i64,
    pub last_raw_open_unix: Option<i64>,
    pub last_raw_cluster_time: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AdaptiveSetup {
    pub identity: String,
    pub raw: SpikeSetup,
    /// Already validated immutable recommendation/provenance. Never rebuilt
    /// from current settings during retries or after a profile replacement.
    pub recommendation: Value,
}

/// Persistence implementations must atomically compare the generation/revision
/// fence, insert setups (unique slot + instrument + raw close, NOT generation),
/// and save this page's checkpoint. `None` means stale/disabled, not an error.
/// Notification implementations must recheck live/adaptive/owner/generation
/// fences and original-close age immediately before EACH external send.
#[allow(async_fn_in_trait)]
pub trait AdaptiveStore: Send + Sync {
    async fn current(&self, fence: &AdaptiveFence) -> Result<bool, WorkerError>;
    async fn checkpoint(
        &self,
        _: &AdaptiveFence,
        _: &AdaptiveInstrument,
    ) -> Result<Option<AdaptiveCheckpoint>, WorkerError> {
        Err(invalid("store does not support uncertain-commit checkpoint recovery"))
    }
    async fn commit_page(
        &self,
        fence: &AdaptiveFence,
        instrument: &AdaptiveInstrument,
        expected: Option<&AdaptiveCheckpoint>,
        checkpoint: &AdaptiveCheckpoint,
        setups: &mut [AdaptiveSetup],
    ) -> Result<Option<Vec<AdaptiveInserted>>, WorkerError>;
    async fn notify(
        &self,
        fence: &AdaptiveFence,
        setup_id: i64,
        instrument: &AdaptiveInstrument,
        setup: &AdaptiveSetup,
        max_age_seconds: i64,
    ) -> Result<(), WorkerError>;
}

pub struct AdaptiveScanOptions<'a> {
    pub fence: &'a AdaptiveFence,
    pub instrument: &'a AdaptiveInstrument,
    pub params: &'a AlgorithmParams,
    pub recommendation: &'a Value,
    /// Immutable original detector initialization boundary supplied by the
    /// accepted chain. Prehistory advances cooldown and cluster state.
    pub bootstrap_from_unix: i64,
    pub emit_from_close_unix: i64,
    pub observation_close_unix: i64,
    pub cluster_gap_minutes: Option<u32>,
    /// Frozen post-chain raw seeds, independent of an old generation's durable
    /// checkpoint. None is used only for an explicitly unseeded bootstrap.
    pub initial_state: Option<AdaptiveCheckpoint>,
    pub checkpoint: Option<AdaptiveCheckpoint>,
    pub max_age_seconds: i64,
    pub stopped: &'a AtomicBool,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct AdaptiveScanResult {
    pub processed_minutes: u64,
    pub raw_signals: u64,
    pub inserted: u64,
    pub fenced: bool,
}

/// Stable across generation/parameter/name changes, distinct from fixed live
/// and other owners/slots. JSON tuple encoding prevents separator collisions.
pub fn adaptive_setup_identity(
    fence: &AdaptiveFence,
    instrument: &AdaptiveInstrument,
    raw_close_unix: i64,
) -> String {
    let key = serde_json::json!([
        "adaptive",
        fence.owner_id,
        fence.algorithm_id,
        fence.slot_id,
        instrument.exchange,
        instrument.category,
        instrument.symbol,
        raw_close_unix,
    ]);
    format!("adaptive:{:x}", Sha256::digest(key.to_string().as_bytes()))
}

fn invalid(message: &str) -> WorkerError {
    WorkerError::Source(format!("adaptive: {message}"))
}

/// Scan only closed candles, with immediate publication of cluster-selected
/// raw signals. Shift/price conditions are recommendation data, not live gates.
pub async fn scan_adaptive_pages<S: CandleSource, D: AdaptiveStore>(
    source: &S,
    store: &D,
    options: AdaptiveScanOptions<'_>,
) -> Result<AdaptiveScanResult, WorkerError> {
    if options.params.algorithm_id() != options.fence.algorithm_id
        || options.bootstrap_from_unix.rem_euclid(60) != 0
        || options.observation_close_unix.rem_euclid(60) != 0
        || options.cluster_gap_minutes == Some(0)
        || serde_json::to_vec(options.recommendation)?.len() > MAX_SNAPSHOT_BYTES
    {
        return Err(invalid("invalid profile or snapshot budget exceeded"));
    }
    let mut result = AdaptiveScanResult::default();
    let mut expected = options.checkpoint;
    let matching = expected
        .as_ref()
        .filter(|c| c.generation_id == options.fence.generation_id)
        .or(options.initial_state.as_ref());
    if matching.is_some_and(|state| {
        state.generation_id != options.fence.generation_id
            || state.next_open_unix.rem_euclid(60) != 0
            || state.last_raw_open_unix.map(|time| time + 60) != state.last_raw_cluster_time
            || state
                .last_raw_open_unix
                .is_some_and(|time| time >= state.next_open_unix)
    }) {
        return Err(invalid("invalid causal checkpoint seed"));
    }
    let resume_from = matching.map(|c| c.next_open_unix);
    let required_seconds = i64::from(options.params.baseline_long_days()) * 86400;
    let rebuild_from = resume_from.map_or(options.bootstrap_from_unix, |from| {
        (from - required_seconds + 60).max(options.bootstrap_from_unix)
    });
    // A missing first minute cannot be represented by starting at the next
    // observed candle: the detector only fills INTERIOR gaps. Find one actual
    // predecessor within the maximum permitted leading gap plus its anchor.
    // No predecessor means no invented prelisting/history candles. A larger
    // gap would reset the existing detector series anyway.
    let read_from = if resume_from.is_some() {
        (rebuild_from - (i64::from(options.params.max_consecutive_gap_minutes()) + 1) * 60)
            .max(options.bootstrap_from_unix)
    } else {
        rebuild_from
    };
    let mut state = matching.cloned().unwrap_or(AdaptiveCheckpoint {
        generation_id: options.fence.generation_id,
        next_open_unix: options.bootstrap_from_unix,
        last_raw_open_unix: None,
        last_raw_cluster_time: None,
    });
    let mut detector =
        PriceVolumeSpikeDetector::with_params(options.params.clone(), state.last_raw_open_unix)?;
    let mut cluster = CausalCluster::new(
        i64::from(options.cluster_gap_minutes.unwrap_or(1)) * 60,
        state.last_raw_cluster_time,
    )
    .map_err(|error| invalid(&error.to_string()))?;
    let mut cursor = None;
    let mut predecessor = None;
    let mut prefix_started = false;
    loop {
        if options.stopped.load(Ordering::SeqCst) || !store.current(options.fence).await? {
            result.fenced = true;
            break;
        }
        let page = source
            .read(&CandleSourceQuery {
                from_unix: read_from,
                to_unix: options.observation_close_unix,
                after_unix: cursor,
                limit: Some(ADAPTIVE_PAGE_LIMIT as u32),
            })
            .await
            .map_err(WorkerError::Source)?;
        if page.bars.len() > ADAPTIVE_PAGE_LIMIT {
            return Err(WorkerError::PageLimitExceeded(page.bars.len()));
        }
        let mut previous = cursor;
        for bar in &page.bars {
            if bar.open_time_unix < read_from
                || bar.open_time_unix >= options.observation_close_unix
                || bar.open_time_unix.rem_euclid(60) != 0
                || previous.is_some_and(|time| bar.open_time_unix <= time)
            {
                return Err(invalid("unordered, open, or out-of-range candle"));
            }
            if bar.close.len() > 128
                || bar.quote_turnover.len() > 128
                || setups_domain::decimal::Decimal::parse(&bar.close)?.int <= 0
                || setups_domain::decimal::Decimal::parse(&bar.quote_turnover)?.int < 0
            {
                return Err(invalid("invalid source price or turnover"));
            }
            previous = Some(bar.open_time_unix);
        }
        if page.next_cursor_unix.is_some()
            && (page.next_cursor_unix != previous || page.bars.is_empty())
        {
            return Err(invalid("incomplete or nonadvancing pagination"));
        }
        if page.bars.is_empty() {
            break;
        }
        let mut setups = Vec::new();
        let mut advanced = false;
        for bar in &page.bars {
            if options.stopped.load(Ordering::SeqCst) {
                result.fenced = true;
                return Ok(result);
            }
            if bar.open_time_unix < rebuild_from {
                predecessor = Some(DetectorBar {
                    open_time_unix: bar.open_time_unix,
                    close: bar.close.clone(),
                    quote_turnover: bar.quote_turnover.clone(),
                });
                continue;
            }
            if !prefix_started {
                if let Some(anchor) = predecessor.take() {
                    detector.push(&anchor)?;
                }
                prefix_started = true;
            }
            let pushed = detector.push(&DetectorBar {
                open_time_unix: bar.open_time_unix,
                close: bar.close.clone(),
                quote_turnover: bar.quote_turnover.clone(),
            })?;
            // On resume these bars rebuild rolling windows only. The durable
            // cooldown seed suppresses pre-checkpoint raw signals.
            if resume_from.is_some_and(|from| bar.open_time_unix < from) {
                // cooldown=0 may re-observe the saved raw minute itself. It
                // leaves the seed unchanged; any different prefix raw would
                // change saved cooldown and must fail without committing.
                if pushed
                    .setup
                    .as_ref()
                    .is_some_and(|raw| Some(raw.open_time_unix) != state.last_raw_open_unix)
                {
                    return Err(invalid(
                        "checkpoint raw seed differs from reconstructed prefix",
                    ));
                }
                continue;
            }
            state.next_open_unix = bar.open_time_unix + 60;
            advanced = true;
            result.processed_minutes += 1;
            if let Some(raw) = pushed.setup {
                result.raw_signals += 1;
                let first = cluster
                    .observe_raw(raw.close_time_unix)
                    .map_err(|error| invalid(&error.to_string()))?;
                // Every RAW signal moves both states, including suppressed
                // cluster members and signals before the export boundary.
                state.last_raw_open_unix = Some(raw.open_time_unix);
                state.last_raw_cluster_time = Some(raw.close_time_unix);
                if (first || options.cluster_gap_minutes.is_none())
                    && raw.close_time_unix >= options.emit_from_close_unix
                {
                    let mut recommendation = options.recommendation.clone();
                    if let Some(shift) = recommendation
                        .get("timeShiftMinutes")
                        .and_then(Value::as_i64)
                    {
                        recommendation["rawSignalAt"] = raw.close_time_unix.into();
                        recommendation["recommendedEntryAt"] =
                            (raw.close_time_unix + shift * 60).into();
                    }
                    setups.push(AdaptiveSetup {
                        identity: adaptive_setup_identity(
                            options.fence,
                            options.instrument,
                            raw.close_time_unix,
                        ),
                        raw,
                        recommendation,
                    });
                }
            }
        }
        if advanced {
            if options.stopped.load(Ordering::SeqCst) {
                result.fenced = true;
                break;
            }
            let Some(inserted) = store
                .commit_page(
                    options.fence,
                    options.instrument,
                    expected.as_ref(),
                    &state,
                    &mut setups,
                )
                .await?
            else {
                result.fenced = true;
                break;
            };
            result.inserted += inserted.len() as u64;
            expected = Some(state.clone());
            for inserted in inserted {
                let setup = setups
                    .get(inserted.index)
                    .ok_or_else(|| invalid("database returned an invalid inserted index"))?;
                if options.stopped.load(Ordering::SeqCst) || !store.current(options.fence).await? {
                    result.fenced = true;
                    return Ok(result);
                }
                store
                    .notify(
                        options.fence,
                        inserted.id,
                        options.instrument,
                        setup,
                        options.max_age_seconds,
                    )
                    .await?;
            }
        }
        let Some(next) = page.next_cursor_unix else {
            break;
        };
        cursor = Some(next);
    }
    // A long read/empty final page must not report a stopped generation as live.
    result.fenced |=
        options.stopped.load(Ordering::SeqCst) || !store.current(options.fence).await?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use setups_domain::SpikeParams;
    use setups_replay::{ArrayCandleSource, CandleBarDto, SourceResult};
    use std::collections::HashSet;
    use std::sync::Mutex;

    #[derive(Default)]
    struct MemoryState {
        checkpoint: Option<AdaptiveCheckpoint>,
        identities: HashSet<String>,
        setups: Vec<AdaptiveSetup>,
        notified: Vec<i64>,
        max_page: usize,
    }

    struct MemoryStore {
        active: AtomicBool,
        stop_after_commit: bool,
        state: Mutex<MemoryState>,
    }
    impl Default for MemoryStore {
        fn default() -> Self {
            Self {
                active: AtomicBool::new(true),
                stop_after_commit: false,
                state: Mutex::default(),
            }
        }
    }
    impl AdaptiveStore for MemoryStore {
        async fn current(&self, _: &AdaptiveFence) -> Result<bool, WorkerError> {
            Ok(self.active.load(Ordering::SeqCst))
        }
        async fn commit_page(
            &self,
            _: &AdaptiveFence,
            _: &AdaptiveInstrument,
            expected: Option<&AdaptiveCheckpoint>,
            checkpoint: &AdaptiveCheckpoint,
            setups: &mut [AdaptiveSetup],
        ) -> Result<Option<Vec<AdaptiveInserted>>, WorkerError> {
            if !self.active.load(Ordering::SeqCst) {
                return Ok(None);
            }
            let mut state = self.state.lock().unwrap();
            if state.checkpoint.as_ref() != expected {
                return Ok(None);
            }
            state.max_page = state.max_page.max(setups.len());
            let mut inserted = Vec::new();
            for (index, setup) in setups.iter().enumerate() {
                if state.identities.insert(setup.identity.clone()) {
                    state.setups.push(setup.clone());
                    inserted.push(AdaptiveInserted {
                        index,
                        id: state.setups.len() as i64,
                    });
                }
            }
            state.checkpoint = Some(checkpoint.clone());
            if self.stop_after_commit && !inserted.is_empty() {
                self.active.store(false, Ordering::SeqCst);
            }
            Ok(Some(inserted))
        }
        async fn notify(
            &self,
            _: &AdaptiveFence,
            id: i64,
            _: &AdaptiveInstrument,
            _: &AdaptiveSetup,
            _: i64,
        ) -> Result<(), WorkerError> {
            self.state.lock().unwrap().notified.push(id);
            Ok(())
        }
    }

    fn fence() -> AdaptiveFence {
        AdaptiveFence {
            owner_id: "owner".into(),
            algorithm_id: "price_volume_spike".into(),
            slot_id: 7,
            generation_id: 3,
            config_revision: 2,
        }
    }
    fn instrument() -> AdaptiveInstrument {
        AdaptiveInstrument {
            exchange: "bybit".into(),
            category: "linear".into(),
            symbol: "BTCUSDT".into(),
        }
    }
    fn params() -> AlgorithmParams {
        AlgorithmParams::Spike(SpikeParams {
            baseline_long_days: 1,
            baseline_short_days: 1,
            window_minutes: 1,
            price_growth_pct: "0".into(),
            volume_multiplier: "0".into(),
            cooldown_minutes: 1,
            ..SpikeParams::default()
        })
    }
    fn bars(minutes: i64) -> ArrayCandleSource {
        ArrayCandleSource::new(
            (0..minutes)
                .map(|i| CandleBarDto {
                    open_time_unix: i * 60,
                    close: "100".into(),
                    volume: "1".into(),
                    quote_turnover: "1".into(),
                })
                .collect(),
        )
    }
    fn options<'a>(
        fence: &'a AdaptiveFence,
        item: &'a AdaptiveInstrument,
        params: &'a AlgorithmParams,
        recommendation: &'a Value,
        stopped: &'a AtomicBool,
    ) -> AdaptiveScanOptions<'a> {
        AdaptiveScanOptions {
            fence,
            instrument: item,
            params,
            recommendation,
            bootstrap_from_unix: 0,
            emit_from_close_unix: 86400,
            observation_close_unix: 87000,
            cluster_gap_minutes: Some(2),
            initial_state: None,
            checkpoint: None,
            max_age_seconds: 60,
            stopped,
        }
    }

    #[test]
    fn identity_is_generation_name_and_parameter_independent_but_slot_scoped() {
        let mut f = fence();
        let key = adaptive_setup_identity(&f, &instrument(), 86400);
        f.generation_id += 1;
        f.config_revision += 1;
        assert_eq!(key, adaptive_setup_identity(&f, &instrument(), 86400));
        f.slot_id += 1;
        assert_ne!(key, adaptive_setup_identity(&f, &instrument(), 86400));
        f.slot_id -= 1;
        f.owner_id = "other".into();
        assert_ne!(key, adaptive_setup_identity(&f, &instrument(), 86400));
    }

    #[tokio::test]
    async fn raw_chain_not_last_publication_controls_cluster_and_preserves_snapshot() {
        let store = MemoryStore::default();
        let f = fence();
        let item = instrument();
        let params = params();
        let stop = AtomicBool::new(false);
        let snapshot = json!({"studyId":42,"generationId":3,"externalName":"<plain>","direction":"short",
            "timeShiftMinutes":300,"priceChangePct":1000,"tpPct":14,"slPct":4,"selectedCellRawR":0.32,"meanR":0.21});
        let result = scan_adaptive_pages(
            &bars(1450),
            &store,
            options(&f, &item, &params, &snapshot, &stop),
        )
        .await
        .unwrap();
        assert_eq!(result.raw_signals, 11);
        assert_eq!(result.inserted, 1); // not another first every two minutes
        let state = store.state.lock().unwrap();
        assert_eq!(state.setups[0].raw.close_time_unix, 86400);
        assert_eq!(
            state.setups[0].recommendation["externalName"],
            snapshot["externalName"]
        );
        assert_eq!(state.setups[0].recommendation["rawSignalAt"], 86400);
        assert_eq!(
            state.setups[0].recommendation["recommendedEntryAt"],
            86400 + 300 * 60
        );
        assert_eq!(state.notified, vec![1]); // future time/price never delays publication
        assert_eq!(
            state.checkpoint.as_ref().unwrap().last_raw_cluster_time,
            Some(87000)
        );
    }

    #[tokio::test]
    async fn prehistory_advances_raw_cluster_and_disabling_clustering_emits_all() {
        let f = fence();
        let item = instrument();
        let params = params();
        let stop = AtomicBool::new(false);
        let snapshot = json!({});
        let store = MemoryStore::default();
        let mut opts = options(&f, &item, &params, &snapshot, &stop);
        opts.emit_from_close_unix = 86460;
        let result = scan_adaptive_pages(&bars(1450), &store, opts)
            .await
            .unwrap();
        assert_eq!(result.inserted, 0); // first raw exists BEFORE export boundary
        let store = MemoryStore::default();
        let mut opts = options(&f, &item, &params, &snapshot, &stop);
        opts.cluster_gap_minutes = None;
        assert_eq!(
            scan_adaptive_pages(&bars(1450), &store, opts)
                .await
                .unwrap()
                .inserted,
            11
        );
    }

    #[tokio::test]
    async fn restart_restores_raw_cooldown_and_cluster_without_duplicate_publication() {
        let f = fence();
        let item = instrument();
        let params = params();
        let stop = AtomicBool::new(false);
        let snapshot = json!({});
        let store = MemoryStore::default();
        let mut opts = options(&f, &item, &params, &snapshot, &stop);
        opts.observation_close_unix = 86700;
        scan_adaptive_pages(&bars(1450), &store, opts)
            .await
            .unwrap();
        let checkpoint = store.state.lock().unwrap().checkpoint.clone();
        let mut opts = options(&f, &item, &params, &snapshot, &stop);
        opts.checkpoint = checkpoint;
        assert_eq!(
            scan_adaptive_pages(&bars(1450), &store, opts)
                .await
                .unwrap()
                .inserted,
            0
        );
        assert_eq!(store.state.lock().unwrap().setups.len(), 1);
    }

    #[tokio::test]
    async fn split_scan_with_missing_first_rebuild_minute_matches_uninterrupted() {
        let source = ArrayCandleSource::new(
            (0..1501)
                .filter(|index| *index != 61)
                .map(|index| CandleBarDto {
                    open_time_unix: index * 60,
                    close: "100".into(),
                    volume: "1".into(),
                    quote_turnover: "1".into(),
                })
                .collect(),
        );
        let f = fence();
        let item = instrument();
        let params = params();
        let snapshot = json!({});
        let stop = AtomicBool::new(false);
        let uninterrupted = MemoryStore::default();
        let mut opts = options(&f, &item, &params, &snapshot, &stop);
        opts.cluster_gap_minutes = None;
        opts.observation_close_unix = 1501 * 60;
        scan_adaptive_pages(&source, &uninterrupted, opts)
            .await
            .unwrap();
        let split = MemoryStore::default();
        let mut opts = options(&f, &item, &params, &snapshot, &stop);
        opts.cluster_gap_minutes = None;
        opts.observation_close_unix = 1500 * 60;
        scan_adaptive_pages(&source, &split, opts).await.unwrap();
        let checkpoint = split.state.lock().unwrap().checkpoint.clone();
        assert_eq!(checkpoint.as_ref().unwrap().next_open_unix, 1500 * 60);
        let mut opts = options(&f, &item, &params, &snapshot, &stop);
        opts.cluster_gap_minutes = None;
        opts.observation_close_unix = 1501 * 60;
        opts.checkpoint = checkpoint;
        scan_adaptive_pages(&source, &split, opts).await.unwrap();
        let uninterrupted = uninterrupted.state.lock().unwrap();
        let split = split.state.lock().unwrap();
        assert_eq!(
            uninterrupted.setups.last().unwrap().raw.close_time_unix,
            1501 * 60
        );
        assert!(
            split.setups == uninterrupted.setups,
            "a leading permitted gap must not lose the first resumed signal: split={} uninterrupted={}",
            split.setups.len(),
            uninterrupted.setups.len()
        );
        assert_eq!(split.checkpoint, uninterrupted.checkpoint);
    }

    #[tokio::test]
    async fn split_gap_rebuild_respects_gap_limits_zero_cooldown_and_listing_start() {
        for (missing, listing_start, cooldown) in
            [(0, 0, 0), (1, 0, 0), (5, 0, 1), (6, 0, 1), (0, 62, 1)]
        {
            let source = ArrayCandleSource::new(
                (listing_start..1510)
                    .filter(|index| !(*index >= 61 && *index < 61 + missing))
                    .map(|index| CandleBarDto {
                        open_time_unix: index * 60,
                        close: "100".into(),
                        volume: "1".into(),
                        quote_turnover: "1".into(),
                    })
                    .collect(),
            );
            let f = fence();
            let item = instrument();
            let mut params = params();
            let AlgorithmParams::Spike(ref mut values) = params else {
                unreachable!()
            };
            values.cooldown_minutes = cooldown;
            let snapshot = json!({});
            let stop = AtomicBool::new(false);
            let full = MemoryStore::default();
            let split = MemoryStore::default();
            let mut opts = options(&f, &item, &params, &snapshot, &stop);
            opts.cluster_gap_minutes = None;
            opts.observation_close_unix = 1510 * 60;
            scan_adaptive_pages(&source, &full, opts).await.unwrap();
            let mut opts = options(&f, &item, &params, &snapshot, &stop);
            opts.cluster_gap_minutes = None;
            opts.observation_close_unix = 1500 * 60;
            scan_adaptive_pages(&source, &split, opts).await.unwrap();
            let checkpoint = split.state.lock().unwrap().checkpoint.clone();
            let mut opts = options(&f, &item, &params, &snapshot, &stop);
            opts.cluster_gap_minutes = None;
            opts.observation_close_unix = 1510 * 60;
            opts.checkpoint = checkpoint;
            scan_adaptive_pages(&source, &split, opts).await.unwrap();
            let full = full.state.lock().unwrap();
            let split = split.state.lock().unwrap();
            assert!(
                full.setups == split.setups,
                "gap={missing} listing_start={listing_start} cooldown={cooldown}: full={} split={}",
                full.setups.len(),
                split.setups.len()
            );
            assert_eq!(full.checkpoint, split.checkpoint);
        }
    }

    #[tokio::test]
    async fn changed_generation_replays_bootstrap_but_same_candle_does_not_duplicate() {
        let mut f = fence();
        let item = instrument();
        let params = params();
        let stop = AtomicBool::new(false);
        let snapshot = json!({"externalName":"old"});
        let store = MemoryStore::default();
        scan_adaptive_pages(
            &bars(1450),
            &store,
            options(&f, &item, &params, &snapshot, &stop),
        )
        .await
        .unwrap();
        let checkpoint = store.state.lock().unwrap().checkpoint.clone();
        f.generation_id += 1;
        let newer = json!({"externalName":"new"});
        let mut opts = options(&f, &item, &params, &newer, &stop);
        opts.checkpoint = checkpoint;
        assert_eq!(
            scan_adaptive_pages(&bars(1450), &store, opts)
                .await
                .unwrap()
                .inserted,
            0
        );
        assert_eq!(
            store.state.lock().unwrap().setups[0].recommendation,
            snapshot
        );
    }

    #[tokio::test]
    async fn stop_between_persistence_and_notification_fences_delivery() {
        let store = MemoryStore {
            stop_after_commit: true,
            ..MemoryStore::default()
        };
        let f = fence();
        let item = instrument();
        let params = params();
        let stop = AtomicBool::new(false);
        let snapshot = json!({});
        let result = scan_adaptive_pages(
            &bars(1450),
            &store,
            options(&f, &item, &params, &snapshot, &stop),
        )
        .await
        .unwrap();
        assert!(result.fenced);
        assert_eq!(result.inserted, 1);
        assert!(store.state.lock().unwrap().notified.is_empty());
    }

    struct BrokenSource;
    impl CandleSource for BrokenSource {
        async fn read(&self, _: &CandleSourceQuery) -> SourceResult {
            Ok(setups_replay::CandlePage {
                bars: Vec::new(),
                next_cursor_unix: Some(1),
            })
        }
    }
    #[tokio::test]
    async fn malformed_pagination_is_error_not_empty_success() {
        let store = MemoryStore::default();
        let f = fence();
        let item = instrument();
        let params = params();
        let stop = AtomicBool::new(false);
        let snapshot = json!({});
        assert!(
            scan_adaptive_pages(
                &BrokenSource,
                &store,
                options(&f, &item, &params, &snapshot, &stop)
            )
            .await
            .is_err()
        );
        assert!(store.state.lock().unwrap().checkpoint.is_none());
    }
}
