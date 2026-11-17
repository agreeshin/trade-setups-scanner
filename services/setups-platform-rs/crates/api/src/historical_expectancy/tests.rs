use super::*;
use axum::{Json, Router, extract::{Query, State}, routing::{get,post}};
use std::sync::Arc;

#[test]
fn legacy_unaligned_period_preserves_continuous_bounds_and_original_cutoff() {
    let spec=StudyRequest {
        algorithm_id:"price_volume_spike".into(),algorithm_version:"1.1.1".into(),
        algorithm_params:serde_json::from_value(json!(setups_domain::SpikeParams::default())).unwrap(),
        algorithm_search:BTreeMap::new(),analytics_search:BTreeMap::new(),
        analytics_params:core::AnalyticsParams {cluster_enabled:true,cluster_gap_minutes:480,time_shift_minutes:6,price_change_pct:Some(1.0)},
        instruments:vec![core::StudyInstrument {exchange:"bybit".into(),category:"linear".into(),symbol:"FIXTUREUSDT".into()}],
        from_unix:1793168289,to_unix:1794637089,direction:setups_analytics::TradeDirection::Short,
        setup_maturity:0.0,tp_levels:vec![16.0],sl_levels:vec![4.0],horizon_minutes:10080,
        min_closed_per_cell:1,min_setups_per_day:0.0,max_trials:8,max_seconds:30,seed:168,
        trend_mode:core::TrendMode::Mean,trend_bend_days:core::TrendBendDays::Linear,
        trend_blend_lambda:core::TrendBlendLambda::default(),min_forecast_expectancy:0.0,forecast:None,
    };
    let context=core::ContinuousContext {observation_cutoff:1794637080};
    assert!(core::required_input_bounds(&spec).is_err(),"manual validation is not the continuous contract");
    let bounds=core::required_continuous_input_bounds(&spec,context).unwrap();
    assert_eq!(bounds.candles_from_unix%60,0);
    assert_eq!(bounds.candles_to_unix,context.observation_cutoff);
    assert!(bounds.candles_from_unix<spec.from_unix);
    assert_eq!((spec.from_unix,spec.to_unix),(1793168289,1794637089));
}

#[test]
fn exact_counter_projection_matches_and_one_causal_or_outcome_change_fails() {
    let mut totals=Totals::new(ContinuousCaptureCell{tp_pct:20.0,sl_pct:4.0});
    totals.setup_count=1766;totals.selected_setup_count=106;
    totals.cell.tp_count=29;totals.cell.sl_count=64;totals.cell.unclosed_count=13;
    totals.cell.price_change_filtered_count=385;
    totals.days.insert(86400,Day{rows:491,tp:29,sl:64});totals.finish();
    let actual=totals.comparison().unwrap();
    let source=db::SavedSource {generation_id:18,study_id:22,slot_id:3,observation_cutoff:172800,
        spec:json!({}),replay_manifest:None,result:json!({"analytics":{"evaluation":{"summary":{
            "setupCount":1766,"selectedSetupCount":106,"evaluatedMinutes":999999,"warmupMinutes":777777},
            "matrix":[totals.cell]},"diagnostics":[totals.diagnostics]}})};
    let expected=db::historical_expected_comparison(&source,20.0,4.0).unwrap();
    assert_eq!(expected["causalEligibleCount"],491);assert_eq!(expected["causalExcludedCount"],1275);
    assert!(expected.get("evaluatedMinutes").is_none() && expected.get("warmupMinutes").is_none());
    assert_eq!(project_saved_shape(&expected,&actual),expected);
    for key in ["causalEligibleCount","causalExcludedCount","selectedSetupCount"] {
        let mut changed=actual.clone();changed[key]=json!(0);assert_ne!(project_saved_shape(&expected,&changed),expected);
    }
    let mut changed=actual.clone();changed["selectedCell"]["tpCount"]=json!(28);
    assert_ne!(project_saved_shape(&expected,&changed),expected);
    assert_eq!(totals.cell.risk_expectancy,Some(0.8709677419354839));
}

#[test]
fn daily_merge_uses_outcome_counts_and_preserves_nonclosed_line_break() {
    let mut totals=Totals::new(ContinuousCaptureCell{tp_pct:16.0,sl_pct:4.0});
    totals.days.insert(86400,Day{rows:12,tp:9,sl:1});
    totals.days.insert(172800,Day{rows:2,tp:0,sl:0});
    let points=totals.points();
    assert_eq!(points[0].mean_r,Some(3.5));assert_eq!(points[0].row_count,12);
    assert_eq!(points[1].mean_r,None);assert_eq!(points[1].row_count,2);
    totals.setup_count=1;
    assert!(totals.comparison().is_err(),"causal underflow must not wrap or silently manufacture counts");
}

#[derive(Clone)]
struct Mock {repair:bool,bad_cursor:bool}

async fn mock_guard(State(state):State<Arc<Mock>>,Json(body):Json<Value>)->Json<Value> {
    let from=body["fromUnix"].as_i64().unwrap();let to=body["toUnix"].as_i64().unwrap();
    let previous=body["readGuards"][0]["previous"].is_object();
    Json(json!({"checkedAt":"2026-11-15T12:00:00Z","instruments":[{"instrumentId":1,"status":"known",
        "latestStoredOpenTimeUnix":null,"contiguousThroughOpenTimeUnix":null,"candles":[],"missingRanges":[{"fromUnix":from,"toUnix":to}],
        "readGuard":{"status":if previous&&state.repair{"invalid"}else{"valid"},"token":{"epoch":"test-source","revision":if previous{"2"}else{"1"}}}}]}))
}

async fn mock_candles(State(state):State<Arc<Mock>>,Query(query):Query<BTreeMap<String,String>>)->Json<Value> {
    let after=query.get("afterUnix");
    let times=if after.is_none(){vec![600,660]}else{vec![780]};
    Json(json!({"candles":times.into_iter().map(|at|json!({"openTimeUnix":at,"open":"100","high":"101","low":"99","close":"100","volume":"1","quoteTurnover":"100"})).collect::<Vec<_>>(),
        "nextCursorUnix":if after.is_none(){Some(if state.bad_cursor{600}else{660})}else{None}}))
}

async fn reader_case(repair:bool,bad_cursor:bool)->Result<(usize,u64),String> {
    let app=Router::new().route("/v1/candles/tail-batch",post(mock_guard)).route("/v1/candles",get(mock_candles))
        .with_state(Arc::new(Mock{repair,bad_cursor}));
    let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();let address=listener.local_addr().unwrap();
    let server=tokio::spawn(async move{axum::serve(listener,app).await.unwrap()});
    let client=MarketDataClient::new(format!("http://{address}"),"fixture");let mut hash=Sha256::new();
    let result=read_instrument(&client,1,&[Window{from:600,to:960}],960,Instant::now()+Duration::from_secs(5),&mut hash).await
        .map(|(candles,missing)|(candles.len(),missing)).map_err(|error|error.to_string());
    server.abort();let _=server.await;result
}

#[tokio::test]
async fn paginated_reader_fingerprints_real_gaps_and_allows_unrelated_live_head_advancement() {
    assert_eq!(reader_case(false,false).await.unwrap(),(3,3));
}

#[tokio::test]
async fn paginated_reader_rejects_changed_prefix_and_nonforward_or_skipped_cursor() {
    assert!(reader_case(true,false).await.unwrap_err().contains("prefix_changed_or_unproven"));
    assert!(reader_case(false,true).await.unwrap_err().contains("pagination_incomplete_or_nonforward"));
}
