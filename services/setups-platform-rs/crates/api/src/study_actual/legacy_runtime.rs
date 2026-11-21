//! Explicit owned preparation only: retained DB source + saved-only MD reads.
//! No publication, profile binding, jobs, detector, actual refresh or HTTP mount.
use super::legacy::{self, CandleInput, InstrumentProof, LegacyCalculation, LegacyError, LegacyLimits, LegacySource, ReadRequest, VerifiedRowsProof};
use serde_json::Value;
use setups_db::{expectancy as db, optimization::{self, StudyRecord}};
use setups_domain::expectancy::{ExpectancyArtifact, MetricAvailability, UtcDailyInput};
use setups_mdclient::{MarketDataClient, CandlesQuery, TailBatchQuery, TailReadGuardQuery, TailReadGuardStatus, TailReadToken};
use setups_optimizer as core;
use sqlx::PgPool;
use std::{collections::{BTreeMap, BTreeSet}, sync::{Arc, atomic::{AtomicUsize, Ordering}}, time::{Duration, Instant}};

/// Privileged CLI/main facts, never HTTP-deserialized. Main must establish the
/// real deployment lineage before setting these assertions for production.
#[derive(Debug, Clone)]
pub struct TrustedAuditFacts {
    pub generation_id: i64,
    pub original_artifact_id: i64,
    pub writer_audit_hash: String,
    pub first_canonical_add_only_verified: bool,
    pub canonical_ohlc_preserved_verified: bool,
    pub historical_close_kernel_compatible_verified: bool,
}
impl TrustedAuditFacts {
    fn validate(&self) -> Result<()> {
        if self.generation_id <= 0 || self.original_artifact_id <= 0 || self.writer_audit_hash.len() != 64
            || !self.writer_audit_hash.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            || !self.first_canonical_add_only_verified || !self.canonical_ohlc_preserved_verified || !self.historical_close_kernel_compatible_verified
        { return Err(Error::Invalid("legacy_trusted_audit_required")); } Ok(())
    }
}
#[derive(Debug)]
pub enum Error { Invalid(&'static str), Storage(setups_db::DbError), Calculation(LegacyError), Interrupted }
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self { Self::Invalid(code) => f.write_str(code), Self::Storage(_) => f.write_str("legacy_storage_unavailable"),
            Self::Calculation(error) => error.fmt(f), Self::Interrupted => f.write_str("legacy_interrupted") }
    }
}
impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> { match self { Self::Storage(e) => Some(e), Self::Calculation(e) => Some(e), _ => None } }
}
impl From<setups_db::DbError> for Error { fn from(e: setups_db::DbError) -> Self { Self::Storage(e) } }
impl From<LegacyError> for Error { fn from(e: LegacyError) -> Self { Self::Calculation(e) } }
pub type Result<T> = std::result::Result<T, Error>;
fn decode<T: serde::de::DeserializeOwned>(value: Value) -> Result<T> { serde_json::from_value(value).map_err(|_| Error::Invalid("legacy_retained_source_invalid")) }

/// Pure decoder, shared by the DB loader and isolated tests. All IDs, original
/// conditions, selected pair and cutoff are checked before any market read.
pub fn assemble_source(owner: &str, study: i64, record: &StudyRecord, saved: &db::SavedSource,
    artifact: &db::Artifact, replay: core::ContinuousReplay, facts: &TrustedAuditFacts) -> Result<LegacySource> {
    facts.validate()?;
    let spec: core::StudyRequest = decode(saved.spec.clone())?;
    let identity = &artifact.identity;
    if record.id != study || record.owner_id != owner || record.status != "completed" || record.origin != "continuous"
        || record.continuous_generation_id != Some(facts.generation_id) || saved.generation_id != facts.generation_id
        || saved.study_id != study || record.continuous_slot_id != Some(saved.slot_id) || record.spec != saved.spec
        || artifact.id != facts.original_artifact_id || identity.owner_id != owner || identity.study_id != study
        || identity.generation_id != saved.generation_id || identity.slot_id != saved.slot_id || identity.historical_input_hash.is_some()
        || artifact.key != db::artifact_key(identity)? || identity.from_unix != spec.from_unix || identity.to_unix != spec.to_unix
        || identity.observation_cutoff != saved.observation_cutoff || identity.forecast != spec.forecast
        || replay.context.observation_cutoff != saved.observation_cutoff || replay.from_unix != spec.from_unix || replay.to_unix != spec.to_unix
    { return Err(Error::Invalid("legacy_fixed_source_mismatch")); }
    let best = &saved.result["best"];
    let study_best = record.result.as_ref().and_then(|r| r.get("best")).ok_or(Error::Invalid("legacy_winner_missing"))?;
    for key in ["params", "analytics", "provenance"] {
        if best.get(key).is_none() || best[key] != study_best[key] { return Err(Error::Invalid("legacy_winner_source_mismatch")); }
    }
    for key in ["clusterEnabled", "clusterGapMinutes", "timeShiftMinutes", "priceChangePct", "horizonMinutes", "setupMaturity"] {
        if best["analytics"].get(key).is_none() { return Err(Error::Invalid("legacy_effective_conditions_missing")); }
    }
    let effective: core::EffectiveAnalytics = decode(best["analytics"].clone())?;
    if identity.effective_analytics != best["analytics"] || identity.provenance != best["provenance"]
        || identity.provenance != serde_json::to_value(&replay.provenance).map_err(|_| Error::Invalid("legacy_provenance_invalid"))?
        || replay.params != decode::<core::ParamMap>(best["params"].clone())?
        || saved.result["selectedCell"]["tpPct"].as_f64() != Some(identity.tp_pct)
        || saved.result["selectedCell"]["slPct"].as_f64() != Some(identity.sl_pct)
    { return Err(Error::Invalid("legacy_recipe_or_pair_mismatch")); }
    let numerical: ExpectancyArtifact = decode(artifact.numerical_snapshot.clone())?;
    numerical.validate().map_err(|_| Error::Invalid("legacy_original_artifact_invalid"))?;
    if numerical.source.study_id != study.to_string() || numerical.source.derived_snapshot_id != artifact.key
        || numerical.source.provenance.snapshot_hash != replay.provenance.snapshot_hash
        || numerical.source.provenance.observation_cutoff != saved.observation_cutoff
    { return Err(Error::Invalid("legacy_original_artifact_mismatch")); }
    let MetricAvailability::Available { value: daily } = numerical.daily else { return Err(Error::Invalid("legacy_original_daily_missing")); };
    let expected_daily: Vec<_> = daily.into_iter().map(|p| UtcDailyInput { day_start_unix: p.day_start_unix, row_count: p.row_count,
        tp_count: p.tp_count, sl_count: p.sl_count, mean_r: p.mean_r }).collect();
    let comparison = db::historical_expected_comparison(saved, identity.tp_pct, identity.sl_pct)?;
    let expected = &comparison["selectedCell"];
    if expected["tpCount"].as_u64() != Some(expected_daily.iter().map(|p| p.tp_count).sum())
        || expected["slCount"].as_u64() != Some(expected_daily.iter().map(|p| p.sl_count).sum())
    { return Err(Error::Invalid("legacy_original_closed_counts_mismatch")); }
    let candidate = core::StudyCandidate { params: replay.params.clone(), analytics: core::AnalyticsParams {
        cluster_enabled: effective.cluster_enabled, cluster_gap_minutes: if effective.cluster_enabled { effective.cluster_gap_minutes.ok_or(Error::Invalid("legacy_cluster_gap_missing"))? } else { spec.analytics_params.cluster_gap_minutes },
        time_shift_minutes: effective.time_shift_minutes, price_change_pct: effective.price_change_pct } };
    Ok(LegacySource { owner_id: owner.into(), study_id: study, generation_id: saved.generation_id,
        original_artifact_id: artifact.id, original_artifact_key: artifact.key.clone(), original_snapshot_hash: replay.provenance.snapshot_hash.clone(),
        spec, candidate, original_effective_analytics: effective, replay, pair: core::ContinuousCaptureCell {tp_pct: identity.tp_pct, sl_pct: identity.sl_pct}, expected_daily })
}

/// Scientific/source metadata only: ignore mutable UI/progress timestamps.
pub fn metadata_fence(record: &StudyRecord, saved: &db::SavedSource, artifact: &db::Artifact, cap: usize) -> Result<String> {
    Ok(legacy::fingerprint(b"legacy-retained-fence-v1\0", &(
        (record.id, &record.owner_id, &record.status, &record.origin, record.continuous_generation_id, record.continuous_slot_id, &record.spec, &record.result),
        (saved.generation_id, saved.study_id, saved.slot_id, saved.observation_cutoff, &saved.spec, &saved.result, &saved.replay_manifest),
        (artifact.id, &artifact.key, &artifact.identity, &artifact.numerical_snapshot),
    ), cap)?.0)
}
async fn metadata(pool: &PgPool, owner: &str, study: i64, facts: &TrustedAuditFacts) -> Result<(StudyRecord, db::SavedSource, db::Artifact)> {
    let record = optimization::get(pool, study).await?.ok_or(Error::Invalid("legacy_study_missing"))?;
    if record.owner_id != owner || record.status != "completed" || record.origin != "continuous" || record.continuous_generation_id != Some(facts.generation_id) {
        return Err(Error::Invalid("legacy_completed_continuous_source_required"));
    }
    let saved = db::saved_source(pool, owner, facts.generation_id).await?.ok_or(Error::Invalid("legacy_saved_source_missing"))?;
    let artifact = db::get(pool, owner, facts.original_artifact_id).await?.ok_or(Error::Invalid("legacy_original_artifact_missing"))?;
    Ok((record, saved, artifact))
}

/// One owned provider; bounded ID map/counter are dropped with the invocation.
/// Clone shares only invocation-local context, never a global cache or queue.
#[derive(Clone)]
pub struct GuardedMdProvider<'a> {
    client: MarketDataClient, ids: Arc<BTreeMap<core::StudyInstrument, i64>>,
    calls: Arc<AtomicUsize>, cancel: &'a core::CancelToken, deadline: Instant,
}
impl<'a> GuardedMdProvider<'a> {
    pub async fn new(client: &MarketDataClient, instruments: &[core::StudyInstrument], cancel: &'a core::CancelToken, deadline: Instant) -> legacy::Result<Self> {
        if cancel.is_cancelled() { return Err(LegacyError::Interrupted); }
        let catalog = tokio::time::timeout(deadline.saturating_duration_since(Instant::now()), client.instruments()).await
            .map_err(|_| LegacyError::Interrupted)?.map_err(|_| LegacyError::Invalid("legacy_md_catalog_unavailable"))?;
        if catalog.len() > 10_000 || instruments.len() > 1000 { return Err(LegacyError::Invalid("legacy_md_catalog_budget")); }
        let wanted: BTreeSet<_> = instruments.iter().cloned().collect(); let mut ids = BTreeMap::new(); let mut distinct = BTreeSet::new();
        for item in catalog {
            let key = core::StudyInstrument {exchange: item.exchange, category: item.category, symbol: item.symbol};
            if wanted.contains(&key) && (item.id <= 0 || item.id > i64::from(i32::MAX) || !distinct.insert(item.id) || ids.insert(key, item.id).is_some()) {
                return Err(LegacyError::Invalid("legacy_md_catalog_identity_ambiguous"));
            }
        }
        if ids.len() != wanted.len() { return Err(LegacyError::Invalid("legacy_md_saved_universe_missing")); }
        Ok(Self { client: client.clone(), ids: Arc::new(ids), calls: Arc::new(AtomicUsize::new(1)), cancel, deadline })
    }
    fn admit_call(&self) -> legacy::Result<()> {
        if self.cancel.is_cancelled() || Instant::now() >= self.deadline { return Err(LegacyError::Interrupted); }
        if self.calls.fetch_add(1, Ordering::Relaxed) >= 20_000 { return Err(LegacyError::Invalid("legacy_md_callback_budget")); } Ok(())
    }
    async fn guard(&self, id: i64, from: i64, to: i64, previous: Option<TailReadToken>) -> legacy::Result<TailReadToken> {
        self.admit_call()?;
        let query = TailBatchQuery {instrument_ids: vec![id], from_unix: from, to_unix: (from + 60).min(to)};
        let guard = TailReadGuardQuery {instrument_id: id, from_unix: from, to_unix: to, previous};
        let response = tokio::time::timeout(self.deadline.saturating_duration_since(Instant::now()), self.client.tail_batch_guarded(&query, std::slice::from_ref(&guard))).await
            .map_err(|_| LegacyError::Interrupted)?.map_err(|_| LegacyError::Invalid("legacy_md_guard_unavailable"))?;
        let item = response.instruments.first().filter(|i| i.instrument_id == id && i.status == "known").ok_or(LegacyError::Invalid("legacy_md_guard_instrument_missing"))?;
        let proof = item.read_guard.as_ref().ok_or(LegacyError::Invalid("legacy_md_guard_missing"))?;
        if guard.previous.as_ref().is_some_and(|old| !proof.validates(old)) || proof.status != TailReadGuardStatus::Valid { return Err(LegacyError::Invalid("legacy_md_prefix_changed")); }
        let token = proof.token.clone().ok_or(LegacyError::Invalid("legacy_md_guard_token_missing"))?;
        token.validate().map_err(|_| LegacyError::Invalid("legacy_md_guard_token_invalid"))?; Ok(token)
    }
    pub async fn read(&self, request: ReadRequest) -> legacy::Result<CandleInput> {
        let id = *self.ids.get(&request.instrument).ok_or(LegacyError::Invalid("legacy_md_instrument_missing"))?;
        let first = request.windows.first().ok_or(LegacyError::Invalid("legacy_md_empty_read"))?;
        let last = request.windows.last().expect("nonempty");
        let before = self.guard(id, first.from_unix, last.to_unix, None).await?;
        let mut candles = Vec::new(); let mut bytes = 2usize; let mut previous = None;
        for window in &request.windows {
            let mut after = None;
            loop {
                self.admit_call()?;
                let page = self.client.candles(&CandlesQuery {instrument_id: id, from_unix: window.from_unix, to_unix: window.to_unix, after_unix: after, limit: Some(5000)},
                    Duration::from_secs(15).min(self.deadline.saturating_duration_since(Instant::now()))).await
                    .map_err(|_| LegacyError::Invalid("legacy_md_candle_page_unavailable"))?;
                if page.candles.len() > 5000 || candles.len() + page.candles.len() > request.max_candles { return Err(LegacyError::Invalid("legacy_md_candle_budget")); }
                bytes = bytes.checked_add(legacy::fingerprint(b"legacy-page-budget-v1\0", &page.candles, request.max_bytes)?.1)
                    .ok_or(LegacyError::Invalid("legacy_md_byte_budget"))?;
                if bytes > request.max_bytes { return Err(LegacyError::Invalid("legacy_md_byte_budget")); }
                for candle in &page.candles {
                    if candle.open_time_unix % 60 != 0 || candle.open_time_unix < window.from_unix || candle.open_time_unix >= window.to_unix
                        || previous.is_some_and(|last| candle.open_time_unix <= last)
                        || [&candle.open,&candle.high,&candle.low,&candle.close,&candle.volume,&candle.quote_turnover].iter().any(|s| s.len() > 128)
                    { return Err(LegacyError::Invalid("legacy_md_candle_scope_order")); }
                    previous = Some(candle.open_time_unix);
                }
                let empty = page.candles.is_empty(); candles.extend(page.candles);
                match page.next_cursor_unix {
                    None => break,
                    Some(cursor) if !empty && previous == Some(cursor) && after.is_none_or(|old| cursor > old) => after = Some(cursor),
                    _ => return Err(LegacyError::Invalid("legacy_md_cursor_invalid")),
                }
            }
        }
        let after = self.guard(id, first.from_unix, last.to_unix, Some(before.clone())).await?;
        let read_identity = legacy::fingerprint(b"legacy-guarded-read-v1\0", &(id, first.from_unix, last.to_unix, before, after), 4096)?.0;
        Ok(CandleInput {instrument_id: id, read_identity, candles})
    }
}

/// Testable calculation/provider half. No DB fence here; production callers use
/// prepare below, which fences retained metadata before returning to a publisher.
pub async fn prepare_from_source(source: &LegacySource, client: &MarketDataClient, limits: &LegacyLimits,
    facts: &TrustedAuditFacts, cancel: &core::CancelToken) -> Result<LegacyCalculation> {
    facts.validate()?;
    if source.generation_id != facts.generation_id || source.original_artifact_id != facts.original_artifact_id { return Err(Error::Invalid("legacy_expected_identity_mismatch")); }
    if limits.max_seconds == 0 || limits.max_seconds > 1800 { return Err(Error::Invalid("legacy_time_budget")); }
    let deadline = Instant::now() + Duration::from_secs(limits.max_seconds);
    let run = async {
        let provider = GuardedMdProvider::new(client, &source.spec.instruments, cancel, deadline).await?;
        let result = legacy::calculate(source, limits, cancel, None, move |request| { let provider = provider.clone(); async move {provider.read(request).await} }).await?;
        let proof = VerifiedRowsProof {version: 1, source_hash: result.source_hash.clone(),
            original_capture: chrono::DateTime::parse_from_rfc3339(&source.replay.provenance.captured_at).map_err(|_| Error::Invalid("legacy_original_capture_invalid"))?.with_timezone(&chrono::Utc),
            evaluator: core::CONTINUOUS_SELECTED_MINUTES_EVALUATOR.into(), writer_audit_hash: facts.writer_audit_hash.clone(),
            first_canonical_add_only_verified: facts.first_canonical_add_only_verified, canonical_ohlc_preserved_verified: facts.canonical_ohlc_preserved_verified,
            historical_close_kernel_compatible_verified: facts.historical_close_kernel_compatible_verified,
            instruments: result.inputs.iter().filter(|i| i.instrument_id > 0).map(|i| InstrumentProof {instrument: i.instrument.clone(), instrument_id: i.instrument_id,
                windows_hash: i.windows_hash.clone(), candle_hash: i.candle_hash.clone(), read_identity: i.read_identity.clone(), candle_count: i.candle_count, latest_ingested_at: None}).collect() };
        Ok(legacy::promote_same_read(source, result, proof, limits)?)
    };
    tokio::time::timeout(deadline.saturating_duration_since(Instant::now()), run).await.map_err(|_| Error::Interrupted)?
}

pub async fn prepare(pool: &PgPool, owner: &str, study: i64, client: &MarketDataClient, limits: &LegacyLimits,
    facts: &TrustedAuditFacts, cancel: &core::CancelToken) -> Result<LegacyCalculation> {
    facts.validate()?;
    if owner.is_empty() || owner.len() > 120 || study <= 0 || limits.max_seconds == 0 || limits.max_seconds > 1800 { return Err(Error::Invalid("legacy_prepare_request_invalid")); }
    if cancel.is_cancelled() { return Err(Error::Interrupted); }
    let run = async {
        let (record, saved, artifact) = metadata(pool, owner, study, facts).await?;
        let fence = metadata_fence(&record, &saved, &artifact, limits.max_source_bytes)?;
        let raw = db::load_historical_replay(pool, owner, &saved, limits.max_source_bytes).await?;
        let source = assemble_source(owner, study, &record, &saved, &artifact, decode(raw)?, facts)?;
        let result = prepare_from_source(&source, client, limits, facts, cancel).await?;
        let (record, saved, artifact) = metadata(pool, owner, study, facts).await?;
        if fence != metadata_fence(&record, &saved, &artifact, limits.max_source_bytes)? { return Err(Error::Invalid("legacy_retained_source_changed")); }
        if cancel.is_cancelled() { return Err(Error::Interrupted); } Ok(result)
    };
    tokio::time::timeout(Duration::from_secs(limits.max_seconds), run).await.map_err(|_| Error::Interrupted)?
}
