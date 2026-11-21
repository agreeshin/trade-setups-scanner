#[path = "../src/study_actual/legacy.rs"] mod legacy;
#[path = "../src/study_actual/legacy_runtime.rs"] mod legacy_runtime;
use legacy_runtime::*;
use axum::{Json, Router, extract::{Query, State}, routing::{get, post}};
use serde_json::{Value, json};
use setups_db::{expectancy as db, optimization::StudyRecord};
use setups_domain::{AlgorithmParams, SpikeParams, expectancy::*};
use setups_mdclient::MarketDataClient;
use setups_optimizer as core;
use std::{collections::BTreeMap, sync::{Arc, atomic::{AtomicUsize, Ordering}}};

fn facts() -> TrustedAuditFacts { TrustedAuditFacts { generation_id:32,original_artifact_id:16,writer_audit_hash:"c".repeat(64),
    first_canonical_add_only_verified:true,canonical_ohlc_preserved_verified:true,historical_close_kernel_compatible_verified:true } }
fn parts() -> (StudyRecord, db::SavedSource, db::Artifact, core::ContinuousReplay) {
    let instrument=core::StudyInstrument {exchange:"fixture".into(),category:"linear".into(),symbol:"TEST".into()};
    let params:core::ParamMap=serde_json::from_value(json!(SpikeParams::default())).unwrap();
    let spec:core::StudyRequest=serde_json::from_value(json!({"algorithmId":"price_volume_spike","algorithmVersion":"1.1.1","algorithmParams":params,
        "analyticsParams":{"clusterEnabled":false,"clusterGapMinutes":1,"timeShiftMinutes":0,"priceChangePct":null},"instruments":[instrument],
        "fromUnix":60,"toUnix":360,"horizonMinutes":3,"tpLevels":[4],"slLevels":[4],"minClosedPerCell":1,"minSetupsPerDay":0,"direction":"short","seed":42,"maxSeconds":30,"maxTrials":1})).unwrap();
    let provenance=core::Provenance {optimizer_version:core::CONTINUOUS_OPTIMIZER_VERSION.into(),seed:42,snapshot_hash:"a".repeat(64),captured_at:"2026-11-17T07:22:54.988468946Z".into()};
    let effective=json!({"clusterEnabled":false,"clusterGapMinutes":null,"timeShiftMinutes":0,"priceChangePct":null,"horizonMinutes":3,"setupMaturity":0});
    let best=json!({"params":params,"analytics":effective,"provenance":provenance});
    let epoch=chrono::DateTime::from_timestamp(0,0).unwrap();
    let record=StudyRecord {id:36,status:"completed".into(),spec:json!(spec),progress:json!({}),result:Some(json!({"best":best})),error:None,created_at:epoch,updated_at:epoch,events:json!([]),origin:"continuous".into(),owner_id:"fixture".into(),continuous_generation_id:Some(32),continuous_slot_id:Some(6)};
    let saved=db::SavedSource {generation_id:32,study_id:36,slot_id:6,observation_cutoff:360,spec:json!(spec),
        result:json!({"best":best,"selectedCell":{"tpPct":4,"slPct":4},"analytics":{"evaluation":{"matrix":[{"tpPct":4,"slPct":4,"tpCount":1,"slCount":0,"closedCount":1}]}}}),replay_manifest:Some(json!({"retained":"immutable-manifest"}))};
    let identity=db::ArtifactIdentity {owner_id:"fixture".into(),generation_id:32,study_id:36,slot_id:6,calculation_version:"2".into(),tp_pct:4.0,sl_pct:4.0,
        from_unix:60,to_unix:360,observation_cutoff:360,provenance:json!(provenance),effective_analytics:effective,historical_input_hash:None,forecast:None,outcome_hash:None};
    let key=db::artifact_key(&identity).unwrap();
    let source:ExpectancySource=serde_json::from_value(json!({"studyId":"36","derivedSnapshotId":key,"generationId":"32","profileId":"6","direction":"short","tpPct":4,"slPct":4,
        "provenance":{"algorithmId":"price_volume_spike","algorithmVersion":"1.1.1","optimizerVersion":provenance.optimizer_version,"seed":42,"snapshotHash":provenance.snapshot_hash,"capturedAt":provenance.captured_at,"fromUnix":60,"toUnix":360,"observationCutoff":360},
        "analytics":{"clusterEnabled":false,"clusterGapMinutes":null,"timeShiftMinutes":0,"priceChangePct":null,"horizonMinutes":3}})).unwrap();
    let numerical=build_expectancy_artifact(ExpectancyInput {source,selected_cell:Some(SelectedCellInput {raw_r:Some(1.0),tp_count:1,sl_count:0}),matrix_mean_r:Some(1.0),
        daily:MetricAvailability::Available {value:vec![UtcDailyInput {day_start_unix:0,row_count:2,tp_count:1,sl_count:0,mean_r:Some(1.0)}]}}).unwrap();
    let artifact=db::Artifact {id:16,key,identity,numerical_snapshot:json!(numerical)};
    let replay=core::ContinuousReplay {from_unix:60,to_unix:360,context:core::ContinuousContext {observation_cutoff:360},params,provenance,
        instruments:vec![core::ContinuousInstrumentReplay {instrument,setups:[60,300].into_iter().map(|at|setups_domain::SpikeSetup {algorithm_id:"price_volume_spike".into(),algorithm_version:"1.1.1".into(),open_time_unix:at-60,close_time_unix:at,close_price:"100".into(),params:AlgorithmParams::Spike(SpikeParams::default()),details:None}).collect(),previous_raw_close_unix:None,last_raw_close_unix:Some(300),evaluated_minutes:5,warmup_minutes:0}]};
    (record,saved,artifact,replay)
}
fn source() -> legacy::LegacySource { let (r,s,a,raw)=parts();assemble_source("fixture",36,&r,&s,&a,raw,&facts()).unwrap() }

#[test]
fn retained_loader_checks_identity_recipe_counts_and_metadata_fence_without_active_profile() {
    let (mut record,saved,artifact,replay)=parts();
    let source=assemble_source("fixture",36,&record,&saved,&artifact,replay.clone(),&facts()).unwrap();
    assert_eq!((source.study_id,source.generation_id,source.original_artifact_id),(36,32,16));
    assert_eq!(source.replay.instruments[0].previous_raw_close_unix,None);
    let fence=metadata_fence(&record,&saved,&artifact,64*1024*1024).unwrap();
    record.progress=json!({"ui":"changed"});assert_eq!(metadata_fence(&record,&saved,&artifact,64*1024*1024).unwrap(),fence);
    record.spec["seed"]=json!(43);assert_ne!(metadata_fence(&record,&saved,&artifact,64*1024*1024).unwrap(),fence);
    assert!(assemble_source("fixture",36,&record,&saved,&artifact,replay.clone(),&facts()).is_err());
    let (record,mut saved,artifact,replay)=parts();saved.observation_cutoff=300;
    assert!(assemble_source("fixture",36,&record,&saved,&artifact,replay,&facts()).is_err());
    let (record,saved,artifact,replay)=parts();assert!(assemble_source("other",36,&record,&saved,&artifact,replay,&facts()).is_err());
}

struct Mock { bad_cursor:bool, changed:bool, catalog:AtomicUsize, candles:AtomicUsize, guards:AtomicUsize }
async fn catalog(State(s):State<Arc<Mock>>)->Json<Value> {s.catalog.fetch_add(1,Ordering::Relaxed);Json(json!({"instruments":[{"id":7,"exchange":"fixture","category":"linear","symbol":"TEST","status":"delisted"}]}))}
async fn guard(State(s):State<Arc<Mock>>,Json(body):Json<Value>)->Json<Value> {
    s.guards.fetch_add(1,Ordering::Relaxed);let previous=body["readGuards"][0]["previous"].is_object();
    Json(json!({"checkedAt":"2026-11-17T08:00:00Z","instruments":[{"instrumentId":7,"status":"known","latestStoredOpenTimeUnix":null,"contiguousThroughOpenTimeUnix":null,
        "candles":[],"missingRanges":[{"fromUnix":body["fromUnix"],"toUnix":body["toUnix"]}],"readGuard":{"status":if previous&&s.changed{"invalid"}else{"valid"},"token":{"epoch":"fixture-source","revision":if previous{"2"}else{"1"}}}}]}))
}
async fn candles(State(s):State<Arc<Mock>>,Query(q):Query<BTreeMap<String,String>>)->Json<Value> {
    s.candles.fetch_add(1,Ordering::Relaxed);let after=q.get("afterUnix").map(|s|s.parse::<i64>().unwrap());
    let from=q["fromUnix"].parse::<i64>().unwrap();let to=q["toUnix"].parse::<i64>().unwrap();
    let mut times:Vec<_>=(0..6).map(|i|i*60).filter(|&at|at>=from&&at<to&&after.is_none_or(|last|at>last)).collect();
    let more=after.is_none()&&times.len()>2;if more {times.truncate(2);}
    let next=more.then(||if s.bad_cursor {times[0]}else{*times.last().unwrap()});
    Json(json!({"candles":times.into_iter().map(|at|json!({"openTimeUnix":at,"open":"100","high":"100","low":if at==120{"90"}else{"100"},"close":"100","volume":"10","quoteTurnover":"1000"})).collect::<Vec<_>>(),"nextCursorUnix":next}))
}
async fn server(bad_cursor:bool,changed:bool)->(MarketDataClient,Arc<Mock>,tokio::task::JoinHandle<()>) {
    let s=Arc::new(Mock {bad_cursor,changed,catalog:AtomicUsize::new(0),candles:AtomicUsize::new(0),guards:AtomicUsize::new(0)});
    let app=Router::new().route("/v1/instruments",get(catalog)).route("/v1/candles",get(candles)).route("/v1/candles/tail-batch",post(guard)).with_state(s.clone());
    let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();let address=listener.local_addr().unwrap();
    let task=tokio::spawn(async move {axum::serve(listener,app).await.unwrap()});
    (MarketDataClient::new(format!("http://{address}"),"fixture-only"),s,task)
}

#[tokio::test]
async fn guarded_provider_promotes_same_read_once_with_distinct_proof_metadata() {
    let (client,calls,server)=server(false,false).await;let source=source();
    let result=prepare_from_source(&source,&client,&legacy::LegacyLimits::default(),&facts(),&core::CancelToken::new()).await.unwrap();
    assert!(matches!(result.fidelity,legacy::LegacyFidelity::VerifiedOriginalReconstruction));
    assert_eq!(result.selected_minutes.points[0].signal_close_unix,60);assert_eq!(result.selected_minutes.points[0].sum_r,1.0);
    assert!(result.conservation_matched);assert!(result.proof_hash.is_some());
    let proof=result.proof.as_ref().unwrap();assert_eq!(proof.instruments[0].candle_hash,result.inputs[0].candle_hash);
    assert_eq!(proof.instruments[0].latest_ingested_at,None);assert_ne!(result.input_hash,result.original_snapshot_hash);
    assert_eq!(calls.catalog.load(Ordering::Relaxed),1);assert_eq!(calls.guards.load(Ordering::Relaxed),2);
    assert_eq!(calls.candles.load(Ordering::Relaxed),2,"one two-page OHLC read, never a second promotion read");
    let mut altered=result;altered.selected_minutes.points[0].tp_count=0;
    let proof=altered.proof.take().unwrap();altered.proof_hash=None;altered.fidelity=legacy::LegacyFidelity::ReconstructedUnproven;
    assert!(legacy::promote_same_read(&source,altered,proof,&legacy::LegacyLimits::default()).is_err());
    server.abort();let _=server.await;
}

#[tokio::test]
async fn guarded_provider_rejects_changed_prefix_or_invalid_page_cursor() {
    for (cursor,changed) in [(true,false),(false,true)] {
        let (client,_,server)=server(cursor,changed).await;
        assert!(prepare_from_source(&source(),&client,&legacy::LegacyLimits::default(),&facts(),&core::CancelToken::new()).await.is_err());
        server.abort();let _=server.await;
    }
}

#[tokio::test]
async fn trusted_flags_identity_and_cancellation_admit_before_db_or_market_reads() {
    let (client,calls,server)=server(false,false).await;let mut audit=facts();audit.canonical_ohlc_preserved_verified=false;
    let pool=sqlx::postgres::PgPoolOptions::new().connect_lazy("postgres://unused@127.0.0.1:1/unused").unwrap();
    assert!(prepare(&pool,"fixture",36,&client,&legacy::LegacyLimits::default(),&audit,&core::CancelToken::new()).await.is_err());
    let mut audit=facts();audit.original_artifact_id=99;
    assert!(prepare_from_source(&source(),&client,&legacy::LegacyLimits::default(),&audit,&core::CancelToken::new()).await.is_err());
    let cancel=core::CancelToken::new();cancel.cancel();
    assert!(prepare(&pool,"fixture",36,&client,&legacy::LegacyLimits::default(),&facts(),&cancel).await.is_err());
    assert_eq!(calls.catalog.load(Ordering::Relaxed),0);assert_eq!(calls.candles.load(Ordering::Relaxed),0);
    server.abort();let _=server.await;
}
