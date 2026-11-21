// Standalone inclusion keeps this helper testable before Backend8b mounts its
// module. No API wiring, database, network, detector or production data here.
#[path = "../src/study_actual/legacy.rs"]
mod legacy;

use legacy::*;
use serde_json::json;
use setups_domain::{AlgorithmParams, SpikeParams, expectancy::UtcDailyInput};
use setups_mdclient::CandleDto;
use setups_optimizer as core;
use std::future::ready;

fn fixture() -> (LegacySource, Vec<CandleDto>) {
    let params: core::ParamMap = serde_json::from_value(json!(SpikeParams::default())).unwrap();
    let instrument = core::StudyInstrument { exchange: "fixture".into(), category: "linear".into(), symbol: "TEST".into() };
    let analytics = core::AnalyticsParams { cluster_enabled: false, cluster_gap_minutes: 1, time_shift_minutes: 0, price_change_pct: None };
    let spec: core::StudyRequest = serde_json::from_value(json!({
        "algorithmId":"price_volume_spike","algorithmVersion":"1.1.1","algorithmParams":params,
        "analyticsParams":analytics,"instruments":[instrument],"fromUnix":60,"toUnix":360,"direction":"short",
        "horizonMinutes":3,"tpLevels":[4],"slLevels":[4],"minClosedPerCell":1,"minSetupsPerDay":0,
        "seed":42,"maxTrials":1,"maxSeconds":30
    })).unwrap();
    let raw = core::ContinuousInstrumentReplay { instrument: instrument.clone(), setups: [60,300].into_iter().map(|at| setups_domain::SpikeSetup {
        algorithm_id: spec.algorithm_id.clone(), algorithm_version: spec.algorithm_version.clone(), open_time_unix:at-60,
        close_time_unix:at, close_price:"100".into(), params:AlgorithmParams::Spike(SpikeParams::default()), details:None,
    }).collect(), previous_raw_close_unix:None,last_raw_close_unix:Some(300),evaluated_minutes:5,warmup_minutes:0 };
    let replay = core::ContinuousReplay { from_unix:60,to_unix:360,context:core::ContinuousContext {observation_cutoff:360},params:params.clone(),
        provenance:core::Provenance {optimizer_version:core::CONTINUOUS_OPTIMIZER_VERSION.into(),seed:42,snapshot_hash:"a".repeat(64),captured_at:"2026-11-17T07:22:54.988468946Z".into()},instruments:vec![raw] };
    let source = LegacySource { owner_id:"fixture".into(),study_id:36,generation_id:32,original_artifact_id:16,
        original_artifact_key:"b".repeat(64),original_snapshot_hash:"a".repeat(64),spec,
        candidate:core::StudyCandidate {params,analytics},replay,
        original_effective_analytics:core::EffectiveAnalytics {cluster_enabled:false,cluster_gap_minutes:None,time_shift_minutes:0,price_change_pct:None,horizon_minutes:3,setup_maturity:Some(0.0)},
        pair:core::ContinuousCaptureCell {tp_pct:4.0,sl_pct:4.0},
        expected_daily:vec![UtcDailyInput {day_start_unix:0,row_count:2,tp_count:1,sl_count:0,mean_r:Some(1.0)}] };
    let candles = (0..6).map(|i| CandleDto {open_time_unix:i*60,open:"100".into(),high:"100".into(),
        low:if i==2 {"90"}else{"100"}.into(),close:"100".into(),volume:"10".into(),quote_turnover:"1000".into()}).collect();
    (source,candles)
}
fn read(request: ReadRequest, candles: &[CandleDto]) -> CandleInput {
    assert!(!request.instrument.symbol.is_empty());
    assert!(request.max_candles > 0 && request.max_bytes > 0);
    assert_eq!(request.windows_hash.len(),64);
    CandleInput {instrument_id:7,read_identity:"verified-current-read-epoch".into(),candles:candles.iter()
        .filter(|c|request.windows.iter().any(|w|c.open_time_unix>=w.from_unix && c.open_time_unix<w.to_unix)).cloned().collect()}
}
fn proof(source:&LegacySource,result:&LegacyCalculation)->VerifiedRowsProof {
    VerifiedRowsProof {version:1,source_hash:result.source_hash.clone(),original_capture:chrono::DateTime::parse_from_rfc3339(&source.replay.provenance.captured_at).unwrap().with_timezone(&chrono::Utc),
        evaluator:core::CONTINUOUS_SELECTED_MINUTES_EVALUATOR.into(),writer_audit_hash:"c".repeat(64),first_canonical_add_only_verified:true,
        canonical_ohlc_preserved_verified:true,historical_close_kernel_compatible_verified:true,instruments:result.inputs.iter().filter(|i|i.instrument_id>0).map(|i|InstrumentProof {
            instrument:i.instrument.clone(),instrument_id:i.instrument_id,windows_hash:i.windows_hash.clone(),candle_hash:i.candle_hash.clone(),
            read_identity:i.read_identity.clone(),candle_count:i.candle_count,latest_ingested_at:Some(chrono::DateTime::parse_from_rfc3339("2026-11-16T00:00:00Z").unwrap().with_timezone(&chrono::Utc)),
        }).collect()}
}
async fn run(source:&LegacySource,candles:&[CandleDto],proof:Option<&VerifiedRowsProof>)->Result<LegacyCalculation> {
    calculate(source,&LegacyLimits::default(),&core::CancelToken::new(),proof,|request|ready(Ok(read(request,candles)))).await
}

#[tokio::test]
async fn conserved_utc_totals_are_unproven_without_row_attestation_and_signal_time_is_not_exit() {
    let (source,candles)=fixture();let before=serde_json::to_vec(&source).unwrap();
    let result=run(&source,&candles,None).await.unwrap();
    assert!(matches!(result.fidelity,LegacyFidelity::ReconstructedUnproven));
    assert!(result.conservation_matched);assert!(result.proof_hash.is_none());
    assert_eq!(result.daily,source.expected_daily);
    assert_eq!(result.selected_minutes.points,vec![core::ContinuousSelectedMinute {signal_close_unix:60,sum_r:1.0,closed_count:1,tp_count:1,sl_count:0}]);
    assert_eq!(result.selected_minutes.evaluator,"original-historical-close-1");
    assert_ne!(result.input_hash,source.original_snapshot_hash);
    assert_eq!(result.source_hash,source_hash(&source,64*1024*1024).unwrap());
    assert_eq!(result.inputs[0].candle_hash,candle_hash(&candles,32*1024*1024).unwrap());
    assert_eq!(serde_json::to_vec(&source).unwrap(),before);
}

#[tokio::test]
async fn verified_reconstruction_binds_exact_rows_not_just_same_utc_mean_or_prefix_close() {
    let (source,mut candles)=fixture();let unproven=run(&source,&candles,None).await.unwrap();let proof=proof(&source,&unproven);
    let verified=run(&source,&candles,Some(&proof)).await.unwrap();
    assert!(matches!(verified.fidelity,LegacyFidelity::VerifiedOriginalReconstruction));assert!(verified.proof_hash.is_some());
    // Change low only: detector-prefix close/turnover and observed TP/UTC mean
    // remain identical, but the exact necessary OHLC row proof MUST fail.
    candles[2].low="89".into();
    let changed=run(&source,&candles,None).await.unwrap();assert!(changed.conservation_matched);
    assert_ne!(changed.input_hash,unproven.input_hash);
    assert!(matches!(run(&source,&candles,Some(&proof)).await,Err(LegacyError::Invalid("legacy_proof_rows_mismatch"))));
}

#[tokio::test]
async fn proof_allows_late_ingestion_but_rejects_unpreserved_ohlc_wrong_kernel_source_and_scope() {
    let (source,candles)=fixture();let result=run(&source,&candles,None).await.unwrap();let base=proof(&source,&result);
    let mut late=base.clone();late.instruments[0].latest_ingested_at=Some(base.original_capture+chrono::Duration::seconds(1));
    assert!(matches!(run(&source,&candles,Some(&late)).await.unwrap().fidelity,LegacyFidelity::VerifiedOriginalReconstruction));
    let mut restored=base.clone();restored.canonical_ohlc_preserved_verified=false;
    assert!(run(&source,&candles,Some(&restored)).await.is_err());
    let mut incompatible=base.clone();incompatible.historical_close_kernel_compatible_verified=false;
    assert!(run(&source,&candles,Some(&incompatible)).await.is_err());
    let mut wrong=base.clone();wrong.source_hash="d".repeat(64);assert!(run(&source,&candles,Some(&wrong)).await.is_err());
    let mut missing=base.clone();missing.instruments.clear();assert!(run(&source,&candles,Some(&missing)).await.is_err());
    let mut altered=source.clone();altered.candidate.analytics.time_shift_minutes=1;
    assert!(matches!(run(&altered,&candles,None).await,Err(LegacyError::Invalid("legacy_original_effective_conditions_mismatch"))));
}

#[tokio::test]
async fn gaps_preserve_unknown_outcomes_and_conservation_mismatch_cannot_gain_verified_fidelity() {
    let (source,mut candles)=fixture();candles.remove(1);
    let result=run(&source,&candles,None).await.unwrap();assert!(!result.conservation_matched);
    assert!(result.selected_minutes.points.is_empty());assert_eq!(result.inputs[0].missing_minutes,1);
    assert_eq!(result.daily[0].mean_r,None);
    let proof=proof(&source,&result);
    assert!(matches!(run(&source,&candles,Some(&proof)).await,Err(LegacyError::Invalid("legacy_verified_conservation_mismatch"))));
}

#[tokio::test]
async fn original_raw_seed_drives_causal_windows_without_detector_or_new_open_entry() {
    let (mut source,candles)=fixture();source.candidate.analytics.cluster_enabled=true;
    source.original_effective_analytics.cluster_enabled=true;source.original_effective_analytics.cluster_gap_minutes=Some(1);
    source.replay.instruments[0].previous_raw_close_unix=Some(0);
    source.expected_daily=vec![UtcDailyInput {day_start_unix:0,row_count:1,tp_count:0,sl_count:0,mean_r:None}];
    assert_eq!(trade_windows(&source,&source.replay.instruments[0]).unwrap(),vec![TradeWindow {from_unix:240,to_unix:360}]);
    let result=run(&source,&candles,None).await.unwrap();assert!(result.conservation_matched);assert!(result.selected_minutes.points.is_empty());
}

#[tokio::test]
async fn verified_closed_set_ignores_nonclosed_row_count_changes_but_rejects_added_closures() {
    let (mut source,candles)=fixture();
    source.expected_daily[0].row_count=1;
    let result=run(&source,&candles,None).await.unwrap();
    assert_eq!(result.daily[0].row_count,2);
    assert!(result.conservation_matched);
    let valid=proof(&source,&result);
    assert!(matches!(run(&source,&candles,Some(&valid)).await.unwrap().fidelity,LegacyFidelity::VerifiedOriginalReconstruction));
    // The original first gap could have left this trade unclosed. A later fill
    // can add its TP but cannot remove old closures. Strict CLOSED parity is the
    // guard that refuses to publish those extra new outcomes as original.
    source.expected_daily[0].tp_count=0;source.expected_daily[0].mean_r=None;
    let extra=run(&source,&candles,None).await.unwrap();
    assert!(!extra.conservation_matched);
    let invalid=proof(&source,&extra);
    assert!(matches!(run(&source,&candles,Some(&invalid)).await,Err(LegacyError::Invalid("legacy_verified_conservation_mismatch"))));
}

#[tokio::test]
async fn cancellation_caps_order_and_future_scope_fail_without_partial_results() {
    let (source,candles)=fixture();let limits=LegacyLimits::default();let cancel=core::CancelToken::new();cancel.cancel();
    let mut called=false;
    let stopped=calculate(&source,&limits,&cancel,None,|request| {called=true;ready(Ok(read(request,&candles)))}).await;
    assert!(matches!(stopped,Err(LegacyError::Interrupted)));assert!(!called);
    let tiny=LegacyLimits {max_instrument_candles:1,..limits.clone()};
    assert!(calculate(&source,&tiny,&core::CancelToken::new(),None,|r|ready(Ok(read(r,&candles)))).await.is_err());
    let tiny=LegacyLimits {max_output_bytes:1,..limits.clone()};assert!(calculate(&source,&tiny,&core::CancelToken::new(),None,|r|ready(Ok(read(r,&candles)))).await.is_err());
    let reversed=calculate(&source,&limits,&core::CancelToken::new(),None,|r| {let mut input=read(r,&candles);input.candles.reverse();ready(Ok(input))}).await;
    assert!(matches!(reversed,Err(LegacyError::Invalid("legacy_candle_order_scope_or_size"))));
    let future=calculate(&source,&limits,&core::CancelToken::new(),None,|r| {let mut input=read(r,&candles);let mut c=candles[0].clone();c.open_time_unix=360;input.candles.push(c);ready(Ok(input))}).await;
    assert!(matches!(future,Err(LegacyError::Invalid("legacy_candle_order_scope_or_size"))));
    let during_read=core::CancelToken::new();
    let stopped=calculate(&source,&limits,&during_read,None,|r| {during_read.cancel();ready(Ok(read(r,&candles)))}).await;
    assert!(matches!(stopped,Err(LegacyError::Interrupted)));
}
