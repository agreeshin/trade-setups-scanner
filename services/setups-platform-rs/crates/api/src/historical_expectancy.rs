//! Operator-only bounded historical calculation. Not mounted in the HTTP API.
//! Original raw rows are inputs; detector/search/gate and current defaults are
//! never invoked. Each instrument's candles are dropped before the next read.
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use setups_analytics::{CausalCluster, OhlcCandle};
use setups_db::expectancy::{self as db, HistoricalAdmission, HistoricalRecalculationProvenance};
use setups_domain::expectancy::*;
use setups_mdclient::{CandlesQuery, MarketDataClient, TailBatchQuery, TailReadGuardQuery, TailReadToken};
use setups_optimizer::{self as core, ContinuousCaptureCell, ContinuousReplay, StudyRequest};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use std::{collections::BTreeMap, time::{Duration, Instant}};

pub const MAX_CANDIDATE_BYTES: usize = 2 * 1024 * 1024;
const MAX_RAW_BYTES: usize = 64 * 1024 * 1024;
const MAX_INSTRUMENT_CANDLES: usize = 300_000;
const MAX_TOTAL_CANDLES: u64 = 5_000_000;

type Error = Box<dyn std::error::Error + Send + Sync>;
#[cfg(test)]
mod tests;
fn invalid(code: &str) -> Error {std::io::Error::other(code).into()}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all="camelCase",deny_unknown_fields)]
pub struct HistoricalCandidate {
    pub owner: String,
    pub generation_id: i64,
    pub original_artifact_id: i64,
    pub config_revision: i64,
    pub recalculation: HistoricalRecalculationProvenance,
    pub comparison: Value,
    pub numerical: Option<Value>,
}

impl HistoricalCandidate {
    pub async fn stage(&self,pool:&PgPool)->Result<Option<db::HistoricalStage>,Error> {
        let Some(admission)=db::admit_historical_recalculation(pool,&self.owner,self.generation_id).await? else {return Ok(None);};
        if admission.original_artifact.id!=self.original_artifact_id || admission.config_revision!=self.config_revision {return Ok(None);}
        Ok(db::stage_historical_recalculation(pool,&admission,&self.recalculation,&self.comparison,self.numerical.as_ref()).await?)
    }
}

#[derive(Debug,Clone,Copy,PartialEq,Eq)]
struct Window {from:i64,to:i64}

fn windows(spec:&StudyRequest,raw:&core::ContinuousInstrumentReplay,candidate:&core::StudyCandidate,
    bounds:core::InputBounds,cutoff:i64)->Result<Vec<Window>,Error> {
    let mut cluster=CausalCluster::new(candidate.analytics.cluster_gap_minutes*60,raw.previous_raw_close_unix)?;
    let mut windows:Vec<Window>=Vec::new();
    for setup in &raw.setups {
        let first=cluster.observe_raw(setup.close_time_unix)?;
        if candidate.analytics.cluster_enabled && !first {continue;}
        let from=setup.open_time_unix.checked_add(candidate.analytics.time_shift_minutes*60).ok_or_else(||invalid("historical_entry_overflow"))?;
        if from+60>cutoff {continue;}
        let to=from.checked_add((spec.horizon_minutes+1)*60).ok_or_else(||invalid("historical_horizon_overflow"))?
            .min(cutoff).min(bounds.candles_to_unix);
        if from<bounds.candles_from_unix || from>=to {return Err(invalid("historical_window_outside_original_bounds"));}
        if let Some(last)=windows.last_mut() && from<=last.to {last.to=last.to.max(to);continue;}
        windows.push(Window{from,to});
    }
    Ok(windows)
}

async fn guard(client:&MarketDataClient,id:i64,scope:Window,previous:Option<TailReadToken>)->Result<TailReadToken,Error> {
    let query=TailBatchQuery {instrument_ids:vec![id],from_unix:scope.from,to_unix:(scope.from+60).min(scope.to)};
    let request=TailReadGuardQuery {instrument_id:id,from_unix:scope.from,to_unix:scope.to,previous};
    let response=client.tail_batch_guarded(&query,std::slice::from_ref(&request)).await
        .map_err(|_|invalid("historical_read_guard_transport_failed"))?;
    let item=response.instruments.first().filter(|item|item.status=="known").ok_or_else(||invalid("historical_instrument_missing"))?;
    let proof=item.read_guard.as_ref().ok_or_else(||invalid("historical_read_guard_missing"))?;
    if let Some(previous)=&request.previous {
        if !proof.validates(previous) {return Err(invalid("historical_read_prefix_changed_or_unproven"));}
    } else if proof.status!=setups_mdclient::TailReadGuardStatus::Valid {return Err(invalid("historical_read_guard_unavailable"));}
    proof.token.clone().ok_or_else(||invalid("historical_read_guard_token_missing"))
}

fn check_deadline(deadline:Instant)->Result<(),Error> {
    if Instant::now()>=deadline {Err(invalid("historical_operation_time_limit"))} else {Ok(())}
}

async fn read_instrument(client:&MarketDataClient,id:i64,windows:&[Window],cutoff:i64,deadline:Instant,
    fingerprint:&mut Sha256)->Result<(Vec<OhlcCandle>,u64),Error> {
    let Some(first)=windows.first() else {fingerprint.update(0u64.to_be_bytes());return Ok((Vec::new(),0));};
    let scope=Window {from:first.from,to:windows.last().expect("nonempty").to};
    let token=guard(client,id,scope,None).await?;
    // The stable epoch is source identity; moving head revisions are NOT data
    // identity. The scoped guard below permits unrelated ongoing live inserts.
    fingerprint.update(token.epoch.as_bytes());
    let mut candles=Vec::new();let mut expected=0u64;
    for window in windows {
        fingerprint.update(window.from.to_be_bytes());fingerprint.update(window.to.to_be_bytes());
        expected=expected.checked_add(((window.to-window.from)/60) as u64).ok_or_else(||invalid("historical_count_overflow"))?;
        let mut after=None;
        loop {
            check_deadline(deadline)?;
            let response=client.candles(&CandlesQuery {instrument_id:id,from_unix:window.from,to_unix:window.to,after_unix:after,limit:Some(5000)},Duration::from_secs(15)).await
                .map_err(|_|invalid("historical_candle_page_failed"))?;
            if response.candles.len()>5000 {return Err(invalid("historical_page_resource_limit"));}
            let mut previous=after;
            for candle in &response.candles {
                let at=candle.open_time_unix;
                if at<window.from || at>=window.to || at%60!=0 || at+60>cutoff || previous.is_some_and(|last|at<=last)
                    || [&candle.open,&candle.high,&candle.low,&candle.close,&candle.volume,&candle.quote_turnover].iter().any(|value|value.len()>128) {
                    return Err(invalid("historical_candle_scope_order_or_size_invalid"));
                }
                previous=Some(at);
                fingerprint.update(serde_json::to_vec(candle)?);
                candles.push(OhlcCandle {open_time_unix:at,open:candle.open.parse()?,high:candle.high.parse()?,low:candle.low.parse()?,close:candle.close.parse()?});
                if candles.len()>MAX_INSTRUMENT_CANDLES {return Err(invalid("historical_instrument_memory_limit"));}
            }
            match response.next_cursor_unix {
                None=>break,
                Some(cursor) if previous==Some(cursor) && after.is_none_or(|old|cursor>old) && !response.candles.is_empty()=>after=Some(cursor),
                _=>return Err(invalid("historical_pagination_incomplete_or_nonforward")),
            }
        }
    }
    check_deadline(deadline)?;
    guard(client,id,scope,Some(token)).await?;
    fingerprint.update((candles.len() as u64).to_be_bytes());
    let missing=expected.checked_sub(candles.len() as u64).ok_or_else(||invalid("historical_duplicate_candles"))?;
    Ok((candles,missing))
}

#[derive(Default)]
struct Day {rows:u64,tp:u64,sl:u64}

struct Totals {
    cell:core::MatrixCell,
    diagnostics:core::ContinuousCellDiagnostics,
    setup_count:usize,selected_setup_count:usize,evaluated_minutes:u64,warmup_minutes:u64,
    days:BTreeMap<i64,Day>,
}

impl Totals {
    fn new(pair:ContinuousCaptureCell)->Self {
        Self {cell:core::MatrixCell {tp_pct:pair.tp_pct,sl_pct:pair.sl_pct,risk_expectancy:None,avg_move_pct:None,closed_count:0,tp_count:0,sl_count:0,unclosed_count:0,no_data_count:0,price_change_filtered_count:0,mean_risk_expectancy:None,forecast_risk_expectancy:None},
            diagnostics:core::ContinuousCellDiagnostics {tp_pct:pair.tp_pct,sl_pct:pair.sl_pct,future_entry_count:0,missing_entry_count:0,right_censored_count:0,observation_gap_count:0,horizon_expired_count:0},
            setup_count:0,selected_setup_count:0,evaluated_minutes:0,warmup_minutes:0,days:BTreeMap::new()}
    }
    fn add(&mut self,result:core::CapturedContinuousAnalysis)->Result<(),Error> {
        let evaluation=result.evaluation;let summary=evaluation.evaluation.summary;
        let cell=evaluation.evaluation.matrix.iter().find(|cell|cell.tp_pct==self.cell.tp_pct && cell.sl_pct==self.cell.sl_pct)
            .ok_or_else(||invalid("historical_selected_cell_missing"))?;
        let diagnostic=evaluation.diagnostics.iter().find(|cell|cell.tp_pct==self.cell.tp_pct && cell.sl_pct==self.cell.sl_pct)
            .ok_or_else(||invalid("historical_diagnostics_missing"))?;
        self.setup_count+=summary.setup_count;self.selected_setup_count+=summary.selected_setup_count;
        self.evaluated_minutes+=summary.evaluated_minutes;self.warmup_minutes+=summary.warmup_minutes;
        self.cell.tp_count+=cell.tp_count;self.cell.sl_count+=cell.sl_count;self.cell.unclosed_count+=cell.unclosed_count;
        self.cell.no_data_count+=cell.no_data_count;self.cell.price_change_filtered_count+=cell.price_change_filtered_count;
        self.diagnostics.future_entry_count+=diagnostic.future_entry_count;self.diagnostics.missing_entry_count+=diagnostic.missing_entry_count;
        self.diagnostics.right_censored_count+=diagnostic.right_censored_count;self.diagnostics.observation_gap_count+=diagnostic.observation_gap_count;
        self.diagnostics.horizon_expired_count+=diagnostic.horizon_expired_count;
        let daily=result.selected_daily.ok_or_else(||invalid("historical_capture_missing"))?;
        for point in daily.points {
            let day=self.days.entry(point.day_start_unix).or_default();day.rows+=point.row_count;
            day.tp+=point.tp_count;day.sl+=point.sl_count;
        }
        if self.days.len()>EXPECTANCY_MAX_POINTS {return Err(invalid("historical_daily_resource_limit"));}
        Ok(())
    }
    fn finish(&mut self) {
        self.cell.closed_count=self.cell.tp_count+self.cell.sl_count;
        if self.cell.closed_count>0 {
            // Same ordered f64 count reduction as the canonical kernel, not
            // weighted averages of per-instrument R and not fixed decimals.
            let mut sum=0.0;
            for _ in 0..self.cell.tp_count {sum+=self.cell.tp_pct;}
            for _ in 0..self.cell.sl_count {sum-=self.cell.sl_pct;}
            self.cell.avg_move_pct=Some(sum/self.cell.closed_count as f64);
            self.cell.risk_expectancy=self.cell.avg_move_pct.map(|mean|mean/self.cell.sl_pct);
        }
    }
    fn comparison(&self)->Result<Value,Error> {
        let eligible=self.days.values().try_fold(0u64,|sum,day|sum.checked_add(day.rows))
            .ok_or_else(||invalid("historical_causal_counter_overflow"))?;
        let excluded=(self.setup_count as u64).checked_sub(eligible).ok_or_else(||invalid("historical_causal_counters_invalid"))?;
        Ok(json!({"selectedCell":self.cell,"diagnostics":self.diagnostics,"setupCount":self.setup_count,
            "causalEligibleCount":eligible,"causalExcludedCount":excluded,"selectedSetupCount":self.selected_setup_count}))
    }
    fn points(&self)->Vec<UtcDailyInput> {self.days.iter().map(|(at,day)| {
        // Merge weighted OUTCOME COUNTS, never means of instrument/day means.
        let mut sum=0.0;for _ in 0..day.tp {sum+=self.cell.tp_pct/self.cell.sl_pct;}for _ in 0..day.sl {sum-=1.0;}
        UtcDailyInput {day_start_unix:*at,row_count:day.rows,tp_count:day.tp,sl_count:day.sl,
            mean_r:(day.tp+day.sl>0).then(||sum/(day.tp+day.sl) as f64)}
    }).collect()}
}

fn project_saved_shape(expected:&Value,actual:&Value)->Value {
    match expected {
        Value::Object(fields)=>Value::Object(fields.iter().map(|(key,value)|(key.clone(),project_saved_shape(value,&actual[key]))).collect()),
        _=>actual.clone(),
    }
}

fn source_candidate(admission:&HistoricalAdmission)->Result<(StudyRequest,core::StudyCandidate),Error> {
    let spec:StudyRequest=serde_json::from_value(admission.source.spec.clone())?;
    let original=&admission.source.result["best"];
    let analytics=&original["analytics"];
    for key in ["clusterEnabled","clusterGapMinutes","timeShiftMinutes","priceChangePct","horizonMinutes","setupMaturity"] {
        if analytics.get(key).is_none() {return Err(invalid("historical_original_effective_analytics_missing"));}
    }
    let effective:core::EffectiveAnalytics=serde_json::from_value(analytics.clone())?;
    if effective.horizon_minutes!=spec.horizon_minutes || spec.setup_maturity!=0.0 {
        return Err(invalid("historical_original_analytics_incompatible"));
    }
    let candidate=core::StudyCandidate {params:serde_json::from_value(original["params"].clone())?,analytics:core::AnalyticsParams {
        cluster_enabled:effective.cluster_enabled,
        cluster_gap_minutes:if effective.cluster_enabled {effective.cluster_gap_minutes.ok_or_else(||invalid("historical_cluster_seed_incompatible"))?}
            else {spec.analytics_params.cluster_gap_minutes},
        time_shift_minutes:effective.time_shift_minutes,price_change_pct:effective.price_change_pct}};
    Ok((spec,candidate))
}

/// READ ONLY compute: works against deployed 0020/0021 databases, before new
/// schema/readers. The caller persists the bounded candidate in a private file.
pub async fn compute(pool:&PgPool,client:&MarketDataClient,owner:&str,generation:i64,max_seconds:u64)->Result<HistoricalCandidate,Error> {
    if owner.is_empty() || owner.len()>120 || generation<=0 || !(1..=1800).contains(&max_seconds) {return Err(invalid("historical_request_invalid"));}
    let deadline=Instant::now()+Duration::from_secs(max_seconds);
    let admission=db::admit_historical_recalculation(pool,owner,generation).await?.ok_or_else(||invalid("historical_not_current_active_legacy"))?;
    let (spec,candidate)=source_candidate(&admission)?;
    let stored=db::load_historical_replay(pool,owner,&admission.source,MAX_RAW_BYTES).await?;
    let raw_setups_hash=format!("{:x}",Sha256::digest(serde_json::to_vec(&stored)?));
    // Legacy generation 15 stores this optional end-of-period document only in
    // result.bootstrap. The REQUIRED causal initialization is each manifest
    // instrument's explicitly present previousRawCloseUnix, checked by DB load.
    let bootstrap=stored.get("bootstrap").filter(|value|value.is_object())
        .or_else(||admission.source.result.get("bootstrap").filter(|value|value.is_object()))
        .map(|value|serde_json::from_value::<core::ContinuousBootstrap>(value.clone())).transpose()?;
    let replay:ContinuousReplay=serde_json::from_value(stored)?;
    if bootstrap.as_ref().is_some_and(|bootstrap|bootstrap.provenance!=replay.provenance || bootstrap.observation_cutoff!=admission.source.observation_cutoff)
        || replay.context.observation_cutoff!=admission.source.observation_cutoff || replay.params!=candidate.params
        || replay.from_unix!=spec.from_unix || replay.to_unix!=spec.to_unix || replay.instruments.len()!=spec.instruments.len()
        || replay.instruments.iter().zip(&spec.instruments).any(|(raw,instrument)|&raw.instrument!=instrument)
        || serde_json::to_value(&replay.provenance)?!=admission.original_artifact.identity.provenance {
        return Err(invalid("historical_original_replay_or_bootstrap_incompatible"));
    }
    let bounds=core::required_continuous_input_bounds(&spec,replay.context)?;
    let identity=&admission.original_artifact.identity;
    let pair=ContinuousCaptureCell {tp_pct:identity.tp_pct,sl_pct:identity.sl_pct};
    if !spec.tp_levels.contains(&pair.tp_pct) || !spec.sl_levels.contains(&pair.sl_pct) {return Err(invalid("historical_selected_pair_outside_original_axes"));}
    let catalog=client.instruments().await.map_err(|_|invalid("historical_catalog_read_failed"))?;
    if catalog.len()>10_000 {return Err(invalid("historical_catalog_resource_limit"));}
    let mut ids=BTreeMap::new();
    for item in catalog {
        if ids.insert(core::StudyInstrument{exchange:item.exchange,category:item.category,symbol:item.symbol},item.id).is_some() {
            return Err(invalid("historical_catalog_ambiguous"));
        }
    }
    if spec.instruments.iter().any(|instrument|!ids.contains_key(instrument)) {return Err(invalid("historical_saved_universe_unmapped"));}
    let mut fingerprint=Sha256::new();fingerprint.update(b"historical-expectancy-input-v1\0");fingerprint.update(raw_setups_hash.as_bytes());
    let mut totals=Totals::new(pair);let(mut count,mut missing)=(0u64,0u64);
    let(mut read_from,mut read_to)=(admission.source.observation_cutoff,0i64);
    for (index,raw) in replay.instruments.iter().enumerate() {
        check_deadline(deadline)?;
        fingerprint.update(serde_json::to_vec(&raw.instrument)?);
        let windows=windows(&spec,raw,&candidate,bounds,replay.context.observation_cutoff)?;
        if let (Some(first),Some(last))=(windows.first(),windows.last()) {read_from=read_from.min(first.from);read_to=read_to.max(last.to);}
        let (candles,gaps)=read_instrument(client,ids[&raw.instrument],&windows,replay.context.observation_cutoff,deadline,&mut fingerprint).await?;
        count+=candles.len() as u64;missing+=gaps;
        if count>MAX_TOTAL_CANDLES {return Err(invalid("historical_total_read_limit"));}
        let mut local_spec=spec.clone();local_spec.instruments=vec![raw.instrument.clone()];
        local_spec.max_seconds=deadline.saturating_duration_since(Instant::now()).as_secs().max(1);
        let local_replay=ContinuousReplay {from_unix:replay.from_unix,to_unix:replay.to_unix,context:replay.context,
            params:replay.params.clone(),provenance:replay.provenance.clone(),instruments:vec![raw.clone()]};
        let actual_input=core::StudyInput {instruments:vec![core::InstrumentInput {instrument:raw.instrument.clone(),detector_bars:Vec::new(),candles}],
            snapshot_hash:format!("{:x}",fingerprint.clone().finalize()),captured_at:chrono::Utc::now().to_rfc3339()};
        let limits=core::StudyLimits {cpu_threads:1,batch_size:1,max_snapshot_bytes:64*1024*1024,max_working_bytes:64*1024*1024,..Default::default()};
        let cancel=core::CancelToken::new();
        let result=core::analyze_historical_continuous_with_daily(&local_spec,&actual_input,&local_replay,&candidate,&limits,&cancel,Some(pair))?
            .ok_or_else(||invalid("historical_canonical_analysis_interrupted"))?;
        totals.add(result)?;
        if !raw.setups.is_empty() {eprintln!("historical_compute generation={generation} instrument_index={} instruments={} raw={} candle_total={count}",index+1,spec.instruments.len(),raw.setups.len());}
    }
    totals.finish();
    let expected=db::historical_expected_comparison(&admission.source,pair.tp_pct,pair.sl_pct)?;
    let actual=project_saved_shape(&expected,&totals.comparison()?);
    let matched=expected==actual;let comparison=json!({"expected":expected,"actual":actual,
        "operationalCoverage":{"status":"not_comparable_operational_metadata",
            "originalEvaluatedMinutes":admission.source.result["analytics"]["evaluation"]["summary"]["evaluatedMinutes"],
            "originalWarmupMinutes":admission.source.result["analytics"]["evaluation"]["summary"]["warmupMinutes"],
            "savedReplayEvaluatedMinutes":totals.evaluated_minutes,"savedReplayWarmupMinutes":totals.warmup_minutes}});
    let input_hash=format!("{:x}",fingerprint.finalize());
    let provenance=HistoricalRecalculationProvenance {revision:1,input_hash:input_hash.clone(),raw_setups_hash,
        recalculated_at_unix:chrono::Utc::now().timestamp(),data_source:"existing_market_data_history".into(),
        read_from_unix:read_from.min(read_to),read_to_unix:read_to,candle_count:count,missing_minutes:missing};
    let numerical=if matched {
        let original:ExpectancyArtifact=serde_json::from_value(admission.original_artifact.numerical_snapshot.clone())?;
        let mut revised_identity=identity.clone();revised_identity.historical_input_hash=Some(input_hash);
        let mut source=original.source;source.derived_snapshot_id=db::artifact_key(&revised_identity)?;
        Some(db::encode_artifact(&build_expectancy_artifact(ExpectancyInput {source,
            selected_cell:Some(SelectedCellInput {raw_r:totals.cell.risk_expectancy,tp_count:totals.cell.tp_count as u64,sl_count:totals.cell.sl_count as u64}),
            matrix_mean_r:original.matrix_mean_r.as_ref().copied(),daily:MetricAvailability::Available {value:totals.points()}})?)?)
    } else {None};
    let candidate=HistoricalCandidate {owner:owner.into(),generation_id:generation,original_artifact_id:admission.original_artifact.id,
        config_revision:admission.config_revision,recalculation:provenance,comparison,numerical};
    if serde_json::to_vec(&candidate)?.len()>MAX_CANDIDATE_BYTES {return Err(invalid("historical_candidate_resource_limit"));}
    // No staging/publication on a changed binding, even though all input was
    // historical. This check neither restarts nor reactivates any generation.
    let current=db::admit_historical_recalculation(pool,owner,generation).await?.ok_or_else(||invalid("historical_binding_changed"))?;
    if current.original_artifact.id!=candidate.original_artifact_id || current.config_revision!=candidate.config_revision {return Err(invalid("historical_binding_changed"));}
    Ok(candidate)
}
