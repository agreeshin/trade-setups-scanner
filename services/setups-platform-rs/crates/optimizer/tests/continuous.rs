use serde_json::json;
use setups_analytics::{CausalCluster, OhlcCandle, TradeDirection};
use setups_detector::PriceVolumeSpikeDetector;
use setups_domain::{AlgorithmParams, DetectorBar, SpikeParams, SpikeSetup};
use setups_optimizer::*;
use std::collections::BTreeMap;

const T0: i64 = 1_699_999_980;
fn minute(index: i64) -> i64 {
    T0 + index * 60
}
fn identity() -> StudyInstrument {
    StudyInstrument {
        exchange: "bybit".into(),
        category: "linear".into(),
        symbol: "FIXTUREUSDT".into(),
    }
}
fn request() -> StudyRequest {
    StudyRequest {
        algorithm_id: "price_volume_spike".into(),
        algorithm_version: "1.1.1".into(),
        algorithm_params: serde_json::from_value(json!(SpikeParams {
            baseline_long_days: 1,
            baseline_short_days: 1,
            ..SpikeParams::default()
        }))
        .unwrap(),
        algorithm_search: BTreeMap::new(),
        analytics_search: BTreeMap::new(),
        analytics_params: AnalyticsParams {
            cluster_enabled: false,
            ..AnalyticsParams::default()
        },
        instruments: vec![identity()],
        from_unix: minute(1450),
        to_unix: minute(1500),
        direction: TradeDirection::Short,
        setup_maturity: 0.0,
        tp_levels: vec![4.0, 8.0],
        sl_levels: vec![4.0, 8.0],
        horizon_minutes: 30,
        min_closed_per_cell: 1,
        min_setups_per_day: 0.0,
        max_trials: 8,
        max_seconds: 30,
        seed: 42,
        trend_mode: TrendMode::Mean,
        trend_bend_days: TrendBendDays::Linear,
        trend_blend_lambda: TrendBlendLambda::default(), min_forecast_expectancy: 0.0,
        forecast: None,
    }
}
fn candidate(spec: &StudyRequest) -> StudyCandidate {
    StudyCandidate {
        params: spec.algorithm_params.clone(),
        analytics: spec.analytics_params.clone(),
    }
}
fn snapshot(last: i64) -> StudyInput {
    let mut detector_bars = Vec::new();
    let mut candles = Vec::new();
    for index in 0..last {
        let close = if index < 1435 {
            "100.00000000".to_string()
        } else {
            format!("{:.8}", 100.0 * 1.13f64.powi(((index - 1434) / 5) as i32))
        };
        let value: f64 = close.parse().unwrap();
        detector_bars.push(DetectorBar {
            open_time_unix: minute(index),
            close,
            quote_turnover: if index < 1435 {
                "10".into()
            } else {
                "3000".into()
            },
        });
        candles.push(OhlcCandle {
            open_time_unix: minute(index),
            open: value,
            high: value * 1.02,
            low: value * 0.98,
            close: value,
        });
    }
    StudyInput {
        instruments: vec![InstrumentInput {
            instrument: identity(),
            detector_bars,
            candles,
        }],
        snapshot_hash: "fixture-frozen".into(),
        captured_at: "2026-11-12T00:00:00Z".into(),
    }
}
fn context(spec: &StudyRequest) -> ContinuousContext {
    ContinuousContext {
        observation_cutoff: spec.to_unix / 60 * 60,
    }
}

#[test]
fn frozen_prefix_fingerprint_binds_exact_values_bounds_and_gaps_not_raw_seed_only() {
    let bars = vec![DetectorBar {open_time_unix:0,close:"100.000".into(),quote_turnover:"10.0".into()},
        DetectorBar {open_time_unix:120,close:"100.000000000000000001".into(),quote_turnover:"20".into()}];
    let original = detector_prefix_fingerprint(&bars,0,180).unwrap();
    let mut normalized = bars.clone(); normalized[0].close="100".into(); normalized[0].quote_turnover="10.000000".into();
    assert_eq!(detector_prefix_fingerprint(&normalized,0,180).unwrap(),original);
    let mut changed = bars.clone(); changed[1].close="100.000000000000000002".into();
    assert_ne!(detector_prefix_fingerprint(&changed,0,180).unwrap(),original);
    changed=bars.clone(); changed[0].quote_turnover="10.000000000000000001".into();
    assert_ne!(detector_prefix_fingerprint(&changed,0,180).unwrap(),original);
    changed=bars.clone(); changed.insert(1,DetectorBar {open_time_unix:60,close:"100".into(),quote_turnover:"0".into()});
    assert_ne!(detector_prefix_fingerprint(&changed,0,180).unwrap(),original);
    assert_ne!(detector_prefix_fingerprint(&bars,0,240).unwrap(),original);
    changed=bars.clone(); changed.reverse();
    assert!(detector_prefix_fingerprint(&changed,0,180).is_err());
    changed=bars.clone(); changed.push(bars[1].clone());
    assert!(detector_prefix_fingerprint(&changed,0,180).is_err());
    assert_eq!(original.bar_count,2);
    assert_eq!(original.sha256.len(),64);
}

#[test]
fn frozen_prefix_manifest_is_compact_roundtrips_and_legacy_is_explicitly_unverified() {
    let spec=request(); let input=snapshot(1500); let context=context(&spec);
    let replay=materialize_continuous(&spec,&input,&candidate(&spec),context,&StudyLimits::default(),&CancelToken::new()).unwrap().unwrap();
    let legacy=continuous_bootstrap(&spec,&replay).unwrap();
    assert!(legacy.frozen_prefix.is_none());
    let bootstrap=continuous_bootstrap_with_frozen_prefix(&spec,&replay,&input).unwrap();
    let prefix=bootstrap.frozen_prefix.as_ref().unwrap();
    assert_eq!(prefix.version,1);
    assert_eq!(prefix.from_open_unix,bootstrap.detector_warmup_from_unix);
    assert_eq!(prefix.to_open_unix,bootstrap.next_open_unix);
    assert_eq!(prefix.instruments.len(),1);
    let fingerprint=detector_prefix_fingerprint(&input.instruments[0].detector_bars,prefix.from_open_unix,prefix.to_open_unix).unwrap();
    assert_eq!(prefix.instruments[0].sha256,fingerprint.sha256);
    assert_eq!(prefix.instruments[0].bar_count,fingerprint.bar_count);
    let json=serde_json::to_vec(&bootstrap).unwrap(); assert!(json.len()<2048);
    assert_eq!(serde_json::from_slice::<ContinuousBootstrap>(&json).unwrap(),bootstrap);
    let mut wrong=input.clone(); wrong.snapshot_hash="another-input".into();
    assert!(continuous_bootstrap_with_frozen_prefix(&spec,&replay,&wrong).is_err());
}

#[test]
fn continuous_search_materialization_and_reanalysis_preserve_exact_scope_and_axes() {
    let mut spec = request();
    spec.from_unix += 17;
    spec.to_unix += 17;
    spec.tp_levels.reverse();
    let data = snapshot(1500);
    let context = context(&spec);
    let limits = StudyLimits::default();
    let cancel = CancelToken::new();
    let bounds = required_continuous_input_bounds(&spec, context).unwrap();
    assert_eq!(bounds.to_unix, context.observation_cutoff);
    assert_eq!(bounds.candles_to_unix, context.observation_cutoff);
    assert_eq!(bounds.detector_from_unix, minute(5));
    assert!(
        validate_request(
            &spec,
            &default_parameter_descriptors(&spec.algorithm_id).unwrap()
        )
        .is_err()
    );
    let evaluated = evaluate_continuous(&spec, &data, &candidate(&spec), context, &limits, &cancel)
        .unwrap()
        .unwrap();
    let outcome = search_continuous(&spec, &data, context, &limits, &cancel, |_| {}).unwrap();
    let best = outcome.best.unwrap();
    assert_eq!(best.matrix, evaluated.evaluation.matrix);
    assert_eq!(
        best.provenance.optimizer_version,
        CONTINUOUS_OPTIMIZER_VERSION
    );
    assert_eq!(OPTIMIZER_VERSION, "plan-a-3");
    let raw = materialize_continuous(&spec, &data, &candidate(&spec), context, &limits, &cancel)
        .unwrap()
        .unwrap();
    assert_eq!((raw.from_unix, raw.to_unix), (spec.from_unix, spec.to_unix));
    assert!(!raw.instruments[0].setups.is_empty());
    assert!(
        raw.instruments[0]
            .setups
            .iter()
            .all(|setup| setup.close_time_unix >= spec.from_unix
                && setup.close_time_unix < spec.to_unix
                && setup.close_time_unix <= context.observation_cutoff)
    );
    // Represents persistence and reconstruction of the ACTUAL raw replay set.
    let persisted: ContinuousReplay =
        serde_json::from_value(serde_json::to_value(raw).unwrap()).unwrap();
    let analyzed = analyze_continuous(
        &spec,
        &data,
        &persisted,
        &candidate(&spec),
        &limits,
        &cancel,
    )
    .unwrap()
    .unwrap();
    assert_eq!(analyzed, evaluated);
    assert_eq!(analyzed.evaluation.matrix[0].tp_pct, spec.tp_levels[0]);
    let mut changed = data.clone();
    changed.snapshot_hash = "different-frozen-input".into();
    assert!(
        analyze_continuous(
            &spec,
            &changed,
            &persisted,
            &candidate(&spec),
            &limits,
            &cancel
        )
        .is_err()
    );
}

#[test]
fn causal_cluster_carries_prehistory_and_every_raw_signal_not_last_publication() {
    let mut spec = request();
    spec.analytics_params.cluster_enabled = true;
    spec.analytics_params.cluster_gap_minutes = 8;
    spec.analytics_search
        .insert("clusterEnabled".into(), vec![json!(false), json!(true)]);
    let data = snapshot(1500);
    let context = context(&spec);
    let limits = StudyLimits::default();
    let cancel = CancelToken::new();
    let raw = materialize_continuous(&spec, &data, &candidate(&spec), context, &limits, &cancel)
        .unwrap()
        .unwrap();
    assert!(raw.instruments[0].previous_raw_close_unix.is_some());
    assert!(raw.instruments[0].setups.len() > 1);
    let on = analyze_continuous(&spec, &data, &raw, &candidate(&spec), &limits, &cancel)
        .unwrap()
        .unwrap();
    assert_eq!(on.evaluation.summary.selected_setup_count, 0);
    let mut off = candidate(&spec);
    off.analytics.cluster_enabled = false;
    let off = analyze_continuous(&spec, &data, &raw, &off, &limits, &cancel)
        .unwrap()
        .unwrap();
    assert!(off.evaluation.summary.selected_setup_count > 1);
    let manual = evaluate_candidate(&spec, &data, &candidate(&spec), &limits, &cancel)
        .unwrap()
        .unwrap();
    assert_eq!(
        manual.summary.selected_setup_count, 1,
        "one-shot still clusters after period filtering"
    );
    let mut cluster = CausalCluster::new(120, Some(60)).unwrap();
    assert!(!cluster.observe_raw(180).unwrap());
    assert!(!cluster.observe_raw(300).unwrap());
    assert!(cluster.observe_raw(480).unwrap());
    assert!(cluster.observe_raw(480).is_err());
}

fn observed_fixture() -> (StudyRequest, StudyInput, ContinuousReplay) {
    let mut spec = request();
    spec.from_unix = 60;
    spec.to_unix = 360;
    spec.horizon_minutes = 3;
    spec.tp_levels = vec![4.0];
    spec.sl_levels = vec![4.0];
    let data = StudyInput {
        instruments: vec![InstrumentInput {
            instrument: identity(),
            detector_bars: (0..6)
                .map(|index| DetectorBar {
                    open_time_unix: index * 60,
                    close: "100".into(),
                    quote_turnover: "10".into(),
                })
                .collect(),
            candles: (0..6)
                .map(|index| OhlcCandle {
                    open_time_unix: index * 60,
                    open: 100.0,
                    high: 100.0,
                    low: 100.0,
                    close: 100.0,
                })
                .collect(),
        }],
        snapshot_hash: "observed".into(),
        captured_at: "fixture".into(),
    };
    let params: SpikeParams =
        serde_json::from_value(serde_json::to_value(&spec.algorithm_params).unwrap()).unwrap();
    let replay = ContinuousReplay {
        from_unix: spec.from_unix,
        to_unix: spec.to_unix,
        context: context(&spec),
        params: spec.algorithm_params.clone(),
        provenance: Provenance {
            optimizer_version: CONTINUOUS_OPTIMIZER_VERSION.into(),
            seed: spec.seed,
            snapshot_hash: data.snapshot_hash.clone(),
            captured_at: data.captured_at.clone(),
        },
        instruments: vec![ContinuousInstrumentReplay {
            instrument: identity(),
            setups: [60, 300]
                .into_iter()
                .map(|close| SpikeSetup {
                    algorithm_id: spec.algorithm_id.clone(),
                    algorithm_version: spec.algorithm_version.clone(),
                    open_time_unix: close - 60,
                    close_time_unix: close,
                    close_price: "100".into(),
                    params: AlgorithmParams::Spike(params.clone()),
                    details: None,
                })
                .collect(),
            previous_raw_close_unix: None,
            last_raw_close_unix: Some(300),
            evaluated_minutes: 5,
            warmup_minutes: 0,
        }],
    };
    (spec, data, replay)
}

#[test]
fn saved_replay_capture_needs_no_detector_bars_and_is_instrument_bounded() {
    let (mut spec, mut historical, mut saved) = observed_fixture();
    spec.analytics_params.cluster_enabled = true;
    spec.analytics_params.cluster_gap_minutes = 1;
    // A failed frequency score must not prevent evaluating these saved rows.
    spec.min_setups_per_day = 100_000.0;
    let mut second = identity();
    second.symbol = "SECONDUSDT".into();
    spec.instruments.push(second.clone());
    saved.instruments[0].previous_raw_close_unix = Some(0);
    let mut second_raw = saved.instruments[0].clone();
    second_raw.instrument = second.clone();
    second_raw.previous_raw_close_unix = None;
    saved.instruments.push(second_raw);
    historical.instruments[0].detector_bars.clear();
    let mut second_data = historical.instruments[0].clone();
    second_data.instrument = second;
    // First instrument's pre-period seed suppresses its first raw signal.
    // The second has a known-empty prehistory and takes the first signal's SL.
    historical.instruments[0].candles[5].low = 90.0;
    second_data.candles[1].high = 110.0;
    historical.instruments.push(second_data);
    let saved_bytes = serde_json::to_vec(&saved).unwrap();
    let restored: ContinuousReplay = serde_json::from_slice(&saved_bytes).unwrap();
    assert_ne!(restored.provenance.snapshot_hash, "available-history-v1");

    // The saved raw-set anchor and actual historical-input proof are distinct.
    // Do not relabel either one to make the native frozen entry accept them.
    let canonical = analyze_continuous_with_daily(
        &spec, &historical, &restored, &candidate(&spec), &StudyLimits::default(),
        &CancelToken::new(), Some(ContinuousCaptureCell { tp_pct: 4.0, sl_pct: 4.0 }),
    ).unwrap().unwrap();
    historical.snapshot_hash = "available-history-v1".into();
    historical.captured_at = "2026-11-15T12:00:00Z".into();
    let limits = StudyLimits::default();
    let cancel = CancelToken::new();
    let pair = ContinuousCaptureCell { tp_pct: 4.0, sl_pct: 4.0 };
    assert!(matches!(analyze_continuous_with_daily(
        &spec, &historical, &restored, &candidate(&spec), &limits, &cancel, Some(pair),
    ), Err(StudyError::InvalidInput(message)) if message.contains("frozen scope/provenance")));
    let whole = analyze_historical_continuous_with_daily(
        &spec, &historical, &restored, &candidate(&spec), &limits, &cancel, Some(pair),
    ).unwrap().unwrap();
    assert_eq!(whole, canonical);
    assert_eq!(serde_json::to_vec(&whole).unwrap(), serde_json::to_vec(&canonical).unwrap());
    assert_eq!(whole.evaluation.evaluation.summary.setup_count, 4);
    assert_eq!(whole.evaluation.evaluation.summary.selected_setup_count, 3);
    assert_eq!(whole.evaluation.evaluation.summary.score, None);
    let cell = &whole.evaluation.evaluation.matrix[0];
    assert_eq!((cell.closed_count, cell.tp_count, cell.sl_count), (2, 1, 1));
    assert_eq!(cell.risk_expectancy, Some(0.0));
    assert_eq!(whole.evaluation.diagnostics[0].right_censored_count, 1);

    // A budget that rejects the whole input still admits each individual
    // instrument. Production callers load/evaluate/drop in this same loop;
    // no input reference or candle is retained in the compact output.
    let probe_limits = StudyLimits { max_snapshot_bytes: 0, ..limits.clone() };
    let requested = match analyze_historical_continuous_with_daily(
        &spec, &historical, &restored, &candidate(&spec), &probe_limits, &cancel, Some(pair),
    ).unwrap_err() {
        StudyError::ResourceLimit { resource, requested, .. } if resource == "snapshot_bytes" => requested,
        other => panic!("unexpected input budget failure: {other}"),
    };
    let bounded = StudyLimits { max_snapshot_bytes: requested - 1, ..limits };
    assert!(matches!(analyze_historical_continuous_with_daily(
        &spec, &historical, &restored, &candidate(&spec), &bounded, &cancel, Some(pair),
    ), Err(StudyError::ResourceLimit { resource, .. }) if resource == "snapshot_bytes"));
    let (mut raw_count, mut eligible, mut closed, mut tp, mut sl, mut censored) = (0, 0, 0, 0, 0, 0);
    let mut daily_counts = BTreeMap::new();
    for (raw, data) in restored.instruments.iter().zip(historical.instruments) {
        let mut one_spec = spec.clone();
        one_spec.instruments = vec![raw.instrument.clone()];
        let one_input = StudyInput {
            instruments: vec![data],
            snapshot_hash: historical.snapshot_hash.clone(),
            captured_at: historical.captured_at.clone(),
        };
        let mut one_replay = restored.clone();
        one_replay.instruments = vec![raw.clone()];
        let result = analyze_historical_continuous_with_daily(
            &one_spec, &one_input, &one_replay, &candidate(&spec), &bounded, &cancel, Some(pair),
        ).unwrap().unwrap();
        raw_count += result.evaluation.evaluation.summary.setup_count;
        eligible += result.evaluation.evaluation.summary.selected_setup_count;
        let cell = &result.evaluation.evaluation.matrix[0];
        closed += cell.closed_count;
        tp += cell.tp_count;
        sl += cell.sl_count;
        censored += result.evaluation.diagnostics[0].right_censored_count;
        for point in result.selected_daily.unwrap().points {
            let counters = daily_counts.entry(point.day_start_unix).or_insert((0, 0, 0));
            counters.0 += point.row_count;
            counters.1 += point.tp_count;
            counters.2 += point.sl_count;
        }
    }
    assert_eq!((raw_count, eligible, closed, tp, sl, censored), (4, 3, 2, 1, 1, 1));
    let whole_daily: BTreeMap<_, _> = whole.selected_daily.unwrap().points.into_iter()
        .map(|p| (p.day_start_unix, (p.row_count, p.tp_count, p.sl_count))).collect();
    assert_eq!(daily_counts, whole_daily);
    assert_eq!(serde_json::to_vec(&restored).unwrap(), saved_bytes);
    assert_eq!(serde_json::to_vec(&saved).unwrap(), saved_bytes);
}

#[test]
fn saved_replay_capture_rejects_post_cutoff_candles_and_preserves_missing_history() {
    let (mut spec, mut historical, raw) = observed_fixture();
    spec.analytics_params.time_shift_minutes = 2;
    spec.analytics_params.price_change_pct = Some(1.0);
    historical.instruments[0].detector_bars.clear();
    historical.snapshot_hash = "available-history-with-shift".into();
    historical.captured_at = "2026-11-15T12:00:00Z".into();
    historical.instruments[0].candles[2].close = 102.0;
    historical.instruments[0].candles[2].high = 102.0;
    historical.instruments[0].candles[3].low = 90.0;
    let pair = ContinuousCaptureCell { tp_pct: 4.0, sl_pct: 4.0 };
    let limits = StudyLimits::default();
    let cancel = CancelToken::new();
    let result = analyze_historical_continuous_with_daily(
        &spec, &historical, &raw, &candidate(&spec), &limits, &cancel, Some(pair),
    ).unwrap().unwrap();
    assert_eq!(result.evaluation.evaluation.summary.selected_setup_count, 1);
    assert_eq!(result.evaluation.evaluation.matrix[0].tp_count, 1);
    assert_eq!(result.evaluation.diagnostics[0].future_entry_count, 1);

    let mut beyond = historical.instruments[0].candles[5];
    beyond.open_time_unix = raw.context.observation_cutoff;
    historical.instruments[0].candles.push(beyond);
    assert!(matches!(analyze_historical_continuous_with_daily(
        &spec, &historical, &raw, &candidate(&spec), &limits, &cancel, Some(pair),
    ), Err(StudyError::InvalidInput(message)) if message.contains("after observationCutoff")));

    historical.instruments[0].candles.clear();
    let missing = analyze_historical_continuous_with_daily(
        &spec, &historical, &raw, &candidate(&spec), &limits, &cancel, Some(pair),
    ).unwrap().unwrap();
    assert_eq!(missing.evaluation.evaluation.summary.setup_count, 2);
    assert_eq!(missing.evaluation.evaluation.summary.selected_setup_count, 0);
    assert_eq!(missing.evaluation.evaluation.matrix[0].closed_count, 0);
    assert_eq!(missing.evaluation.evaluation.matrix[0].risk_expectancy, None);
    assert_eq!(missing.evaluation.diagnostics[0].missing_entry_count, 1);
    assert_eq!(missing.evaluation.diagnostics[0].future_entry_count, 1);
    let point = missing.selected_daily.unwrap().points.remove(0);
    assert_eq!((point.row_count, point.tp_count, point.sl_count, point.mean_r), (2, 0, 0, None));
}

#[test]
fn saved_replay_capture_requires_explicit_prehistory_not_end_of_period_bootstrap() {
    let (mut spec, mut historical, mut saved) = observed_fixture();
    spec.analytics_params.cluster_enabled = true;
    spec.analytics_params.cluster_gap_minutes = 1;
    historical.instruments[0].detector_bars.clear();
    historical.snapshot_hash = "separate-historical-read".into();
    historical.captured_at = "2026-11-15T12:00:00Z".into();
    let pair = Some(ContinuousCaptureCell { tp_pct: 4.0, sl_pct: 4.0 });
    let limits = StudyLimits::default();
    let mut serialized = serde_json::to_value(&saved).unwrap();
    assert!(serialized["instruments"][0]["previousRawCloseUnix"].is_null());
    let known_empty: ContinuousReplay = serde_json::from_value(serialized.clone()).unwrap();
    let empty = analyze_historical_continuous_with_daily(
        &spec, &historical, &known_empty, &candidate(&spec), &limits, &CancelToken::new(), pair,
    ).unwrap().unwrap();
    assert_eq!(empty.selected_daily.unwrap().points[0].row_count, 2);
    serialized["instruments"][0].as_object_mut().unwrap().remove("previousRawCloseUnix");
    let missing = serde_json::from_value::<ContinuousReplay>(serialized).unwrap_err();
    assert!(missing.to_string().contains("previousRawCloseUnix"));

    saved.instruments[0].previous_raw_close_unix = Some(0);
    let present: ContinuousReplay = serde_json::from_slice(&serde_json::to_vec(&saved).unwrap()).unwrap();
    let seeded = analyze_historical_continuous_with_daily(
        &spec, &historical, &present, &candidate(&spec), &limits, &CancelToken::new(), pair,
    ).unwrap().unwrap();
    assert_eq!(seeded.selected_daily.unwrap().points[0].row_count, 1);
    saved.instruments[0].previous_raw_close_unix = saved.instruments[0].last_raw_close_unix;
    assert!(matches!(analyze_historical_continuous_with_daily(
        &spec, &historical, &saved, &candidate(&spec), &limits, &CancelToken::new(), pair,
    ), Err(StudyError::InvalidInput(message)) if message.contains("prehistory raw seed")));

    saved.instruments[0].previous_raw_close_unix = Some(0);
    let cancel = CancelToken::new();
    cancel.cancel();
    assert!(analyze_historical_continuous_with_daily(
        &spec, &historical, &saved, &candidate(&spec), &limits, &cancel, pair,
    ).unwrap().is_none());
    saved.provenance.snapshot_hash.clear();
    assert!(matches!(analyze_historical_continuous_with_daily(
        &spec, &historical, &saved, &candidate(&spec), &limits, &CancelToken::new(), pair,
    ), Err(StudyError::InvalidInput(message)) if message.contains("original scope/provenance")));
}

#[test]
fn observed_outcomes_distinguish_censoring_expiry_future_entry_and_holes_without_frequency_bias() {
    let (spec, mut data, raw) = observed_fixture();
    let limits = StudyLimits::default();
    let cancel = CancelToken::new();
    let evaluated = analyze_continuous(&spec, &data, &raw, &candidate(&spec), &limits, &cancel)
        .unwrap()
        .unwrap();
    assert_eq!(evaluated.evaluation.summary.selected_setup_count, 2);
    assert_eq!(evaluated.evaluation.matrix[0].unclosed_count, 1);
    assert_eq!(evaluated.diagnostics[0].horizon_expired_count, 1);
    assert_eq!(evaluated.diagnostics[0].right_censored_count, 1);
    assert_eq!(evaluated.evaluation.summary.score, None);
    let mut shifted = candidate(&spec);
    shifted.analytics.time_shift_minutes = 2;
    let shifted = analyze_continuous(&spec, &data, &raw, &shifted, &limits, &cancel)
        .unwrap()
        .unwrap();
    assert_eq!(shifted.evaluation.summary.selected_setup_count, 1);
    assert_eq!(shifted.diagnostics[0].future_entry_count, 1);
    data.instruments[0].candles[1].low = 80.0;
    let closed = analyze_continuous(&spec, &data, &raw, &candidate(&spec), &limits, &cancel)
        .unwrap()
        .unwrap();
    assert_eq!(closed.evaluation.summary.selected_setup_count, 2);
    assert_eq!(closed.evaluation.matrix[0].closed_count, 1);
    assert_eq!(
        closed.evaluation.summary.score,
        Some(1.0),
        "censored entry must not dilute R"
    );
    data.instruments[0].candles[1].low = 100.0;
    data.instruments[0].candles.remove(2);
    data.instruments[0].candles[2].low = 80.0;
    let gap = analyze_continuous(&spec, &data, &raw, &candidate(&spec), &limits, &cancel)
        .unwrap()
        .unwrap();
    assert_eq!(gap.evaluation.summary.selected_setup_count, 2);
    assert_eq!(gap.diagnostics[0].observation_gap_count, 1);
    assert_eq!(
        gap.evaluation.matrix[0].closed_count, 0,
        "a later TP cannot bridge unknown earlier outcomes"
    );
}

#[test]
fn selected_daily_capture_preserves_whole_analysis_and_only_records_requested_pair() {
    let (mut spec, mut data, raw) = observed_fixture();
    spec.tp_levels = vec![4.0, 8.0]; spec.sl_levels = vec![4.0, 8.0];
    // TP4 observes a TP before SL4, whereas TP8/SL4 observes the later SL.
    data.instruments[0].candles[1].low = 95.0;
    data.instruments[0].candles[2].high = 105.0;
    let pair = ContinuousCaptureCell { tp_pct: 8.0, sl_pct: 4.0 };
    let captured = capture_and_assert_identical(&spec, &data, &raw, &candidate(&spec), pair);
    let daily = captured.selected_daily.unwrap();
    assert_eq!((daily.tp_pct, daily.sl_pct), (8.0, 4.0));
    assert_eq!(daily.points.len(), 1);
    assert_eq!((daily.points[0].row_count, daily.points[0].tp_count, daily.points[0].sl_count), (2,0,1));
    assert_eq!(daily.points[0].mean_r, Some(-1.0));
    assert_eq!(captured.evaluation.evaluation.matrix[0].tp_count, 1);
    assert_eq!(captured.evaluation.evaluation.matrix[2].sl_count, 1);
}

fn capture_and_assert_identical(spec: &StudyRequest, data: &StudyInput, replay: &ContinuousReplay,
    candidate: &StudyCandidate, pair: ContinuousCaptureCell) -> CapturedContinuousAnalysis {
    let limits = StudyLimits::default(); let cancel = CancelToken::new();
    let original = analyze_continuous(spec, data, replay, candidate, &limits, &cancel).unwrap().unwrap();
    let off = analyze_continuous_with_daily(spec, data, replay, candidate, &limits, &cancel, None).unwrap().unwrap();
    assert!(off.selected_daily.is_none()); assert!(off.selected_minutes.is_none()); assert_eq!(off.evaluation, original);
    assert!(serde_json::to_value(&off).unwrap().get("selectedMinutes").is_none());
    let captured = analyze_continuous_with_daily(spec, data, replay, candidate, &limits, &cancel, Some(pair)).unwrap().unwrap();
    // All scientific fields, original axis order, diagnostics and exact float
    // serialization stay identical, not merely approximately equal averages.
    assert_eq!(captured.evaluation, original);
    assert_eq!(serde_json::to_vec(&captured.evaluation).unwrap(), serde_json::to_vec(&original).unwrap());
    let daily = captured.selected_daily.as_ref().unwrap();
    let cell = original.evaluation.matrix.iter().find(|cell| cell.tp_pct == pair.tp_pct && cell.sl_pct == pair.sl_pct).unwrap();
    assert_eq!(daily.points.iter().map(|p|p.tp_count).sum::<u64>(), cell.tp_count as u64);
    assert_eq!(daily.points.iter().map(|p|p.sl_count).sum::<u64>(), cell.sl_count as u64);
    assert!(daily.points.len() <= setups_domain::expectancy::EXPECTANCY_MAX_POINTS);
    let minutes = captured.selected_minutes.as_ref().unwrap();
    assert_eq!(minutes.version, CONTINUOUS_SELECTED_MINUTES_VERSION);
    assert_eq!(minutes.evaluator, CONTINUOUS_SELECTED_MINUTES_EVALUATOR);
    assert_eq!((minutes.tp_pct, minutes.sl_pct), (pair.tp_pct, pair.sl_pct));
    assert_eq!(minutes.points.iter().map(|p| p.closed_count).sum::<u64>(), cell.closed_count as u64);
    assert_eq!(minutes.points.iter().map(|p| p.tp_count).sum::<u64>(), cell.tp_count as u64);
    assert_eq!(minutes.points.iter().map(|p| p.sl_count).sum::<u64>(), cell.sl_count as u64);
    assert!(minutes.points.iter().all(|p| p.closed_count > 0 && p.closed_count == p.tp_count + p.sl_count));
    assert!(minutes.points.windows(2).all(|p| p[0].signal_close_unix < p[1].signal_close_unix));
    assert!(minutes.points.len() <= limits.max_setups_per_candidate);
    let expected = cell.tp_count as f64 * (pair.tp_pct / pair.sl_pct) - cell.sl_count as f64;
    let tolerance = 1e-10 * (cell.tp_count as f64 * (pair.tp_pct / pair.sl_pct) + cell.sl_count as f64).max(1.0);
    assert!((minutes.points.iter().map(|p| p.sum_r).sum::<f64>() - expected).abs() <= tolerance);
    let output_bound = replay.instruments.iter().map(|r| r.setups.len()).sum::<usize>() * 256 + 256;
    assert!(serde_json::to_vec(minutes).unwrap().len() <= output_bound);
    captured
}

#[test]
fn selected_minutes_preserve_historical_close_entry_and_raw_signal_time_not_exit() {
    let (mut spec, mut data, raw) = observed_fixture();
    spec.analytics_params.time_shift_minutes = 1;
    // CLOSE-entry is 100 at minute 60, with SL at minute 120. An OPEN-entry
    // evaluation would instead enter at 200 and TP inside its entry candle.
    data.instruments[0].candles[1].open = 200.0;
    data.instruments[0].candles[1].high = 200.0;
    data.instruments[0].candles[2].high = 105.0;
    let result = capture_and_assert_identical(
        &spec, &data, &raw, &candidate(&spec), ContinuousCaptureCell { tp_pct: 4.0, sl_pct: 4.0 },
    );
    assert_eq!(result.evaluation.evaluation.matrix[0].sl_count, 1);
    let minutes = result.selected_minutes.unwrap();
    assert_eq!(minutes.evaluator, "original-historical-close-1");
    assert_eq!(minutes.points, vec![ContinuousSelectedMinute {
        signal_close_unix: 60, sum_r: -1.0, closed_count: 1, tp_count: 0, sl_count: 1,
    }]);
    let wire = serde_json::to_value(&minutes).unwrap();
    assert_eq!(wire["points"][0], json!({
        "signalCloseUnix": 60, "sumR": -1.0, "closedCount": 1, "tpCount": 0, "slCount": 1,
    }));
    assert_eq!(serde_json::from_value::<ContinuousSelectedMinutes>(wire).unwrap(), minutes);
}

#[test]
fn selected_minutes_fold_same_instant_across_instruments_and_keep_zero_sum_closed_points() {
    let (mut spec, mut data, mut raw) = observed_fixture();
    let mut second = identity();
    second.symbol = "SECONDUSDT".into();
    spec.instruments.push(second.clone());
    let mut second_data = data.instruments[0].clone();
    second_data.instrument = second.clone();
    second_data.candles[1].high = 110.0;
    let mut second_raw = raw.instruments[0].clone();
    second_raw.instrument = second;
    data.instruments[0].candles[1].low = 90.0;
    data.instruments[0].candles[5].low = 90.0;
    data.instruments.push(second_data);
    raw.instruments.push(second_raw);
    // Capture order is 60, 300, 60, not globally timestamp-sorted. The second
    // instrument's censored minute 300 contributes no fake closed/zero outcome.
    let result = capture_and_assert_identical(
        &spec, &data, &raw, &candidate(&spec), ContinuousCaptureCell { tp_pct: 4.0, sl_pct: 4.0 },
    );
    assert_eq!(result.selected_minutes.unwrap().points, vec![
        ContinuousSelectedMinute { signal_close_unix: 60, sum_r: 0.0, closed_count: 2, tp_count: 1, sl_count: 1 },
        ContinuousSelectedMinute { signal_close_unix: 300, sum_r: 1.0, closed_count: 1, tp_count: 1, sl_count: 0 },
    ]);
    // The old daily payload is byte-identical, including all eligible rows.
    let expected = ContinuousSelectedDaily {
        tp_pct: 4.0, sl_pct: 4.0,
        points: vec![setups_domain::expectancy::UtcDailyInput {
            day_start_unix: 0, row_count: 4, tp_count: 2, sl_count: 1, mean_r: Some(1.0 / 3.0),
        }],
    };
    assert_eq!(serde_json::to_vec(&result.selected_daily.unwrap()).unwrap(), serde_json::to_vec(&expected).unwrap());
}

#[test]
fn selected_minutes_empty_capture_is_complete_and_not_a_missing_or_zero_outcome() {
    let (spec, data, mut raw) = observed_fixture();
    let pair = ContinuousCaptureCell { tp_pct: 4.0, sl_pct: 4.0 };
    // Existing horizon-expired/right-censored rows still appear in daily input,
    // but the separate closed-only ledger has no fabricated zero-R minute.
    let result = capture_and_assert_identical(&spec, &data, &raw, &candidate(&spec), pair);
    assert!(result.selected_minutes.unwrap().points.is_empty());
    assert_eq!(result.selected_daily.unwrap().points[0].mean_r, None);
    raw.instruments[0].setups.clear();
    raw.instruments[0].last_raw_close_unix = None;
    let result = capture_and_assert_identical(&spec, &data, &raw, &candidate(&spec), pair);
    assert!(result.selected_minutes.as_ref().unwrap().points.is_empty());
    assert!(result.selected_daily.as_ref().unwrap().points.is_empty());
    let mut legacy_wire = serde_json::to_value(&result).unwrap();
    legacy_wire.as_object_mut().unwrap().remove("selectedMinutes");
    let legacy: CapturedContinuousAnalysis = serde_json::from_value(legacy_wire).unwrap();
    assert!(legacy.selected_minutes.is_none());
    assert_eq!(legacy.evaluation, result.evaluation);
}

#[test]
fn selected_minutes_conserve_fractional_r_without_changing_daily_or_scientific_arithmetic() {
    let (mut spec, mut data, mut raw) = observed_fixture();
    spec.sl_levels = vec![12.0];
    let template_data = data.instruments[0].clone();
    let template_raw = raw.instruments[0].clone();
    spec.instruments.clear();
    data.instruments.clear();
    raw.instruments.clear();
    for index in 0..30 {
        let mut instrument = identity();
        instrument.symbol = format!("FRACTIONAL{index}USDT");
        let mut input = template_data.clone();
        input.instrument = instrument.clone();
        if index < 21 { input.candles[1].low = 90.0; }
        else { input.candles[1].high = 120.0; }
        let mut replay = template_raw.clone();
        replay.instrument = instrument.clone();
        spec.instruments.push(instrument);
        data.instruments.push(input);
        raw.instruments.push(replay);
    }
    let result = capture_and_assert_identical(
        &spec, &data, &raw, &candidate(&spec), ContinuousCaptureCell { tp_pct: 4.0, sl_pct: 12.0 },
    );
    let minutes = result.selected_minutes.unwrap();
    assert_eq!(minutes.points.len(), 1);
    let point = minutes.points[0];
    assert_eq!((point.closed_count, point.tp_count, point.sl_count), (30, 21, 9));
    assert!((point.sum_r - (21.0 * (4.0 / 12.0) - 9.0)).abs() <= 1e-10 * 16.0);
}

#[test]
fn selected_minutes_obey_native_setup_and_byte_caps_and_cancel_without_partial_results() {
    let (spec, mut data, raw) = observed_fixture();
    data.instruments[0].candles[1].low = 90.0;
    data.instruments[0].candles[5].high = 110.0;
    let pair = ContinuousCaptureCell { tp_pct: 4.0, sl_pct: 4.0 };
    assert_eq!(StudyLimits::default().max_setups_per_candidate, 100_000);
    let limits = StudyLimits { max_setups_per_candidate: 2, ..StudyLimits::default() };
    let run = |limits: &StudyLimits, cancel: &CancelToken| analyze_continuous_with_daily(
        &spec, &data, &raw, &candidate(&spec), limits, cancel, Some(pair),
    );
    let result = run(&limits, &CancelToken::new()).unwrap().unwrap();
    assert_eq!(result.selected_minutes.unwrap().points.len(), 2);
    let below = StudyLimits { max_setups_per_candidate: 1, ..limits.clone() };
    assert!(matches!(run(&below, &CancelToken::new()),
        Err(StudyError::ResourceLimit { resource, .. }) if resource == "setups_per_candidate"));
    let tiny = StudyLimits { max_working_bytes: 0, ..limits.clone() };
    let daily_only_budget = match run(&tiny, &CancelToken::new()).unwrap_err() {
        StudyError::ResourceLimit { resource, requested, .. } if resource == "continuous_working_bytes" => requested,
        other => panic!("unexpected capture admission error: {other}"),
    };
    // The first admission above accounts for the unchanged daily capture. Its
    // exact byte budget must NOT also admit the extra event/output allocation.
    let daily_only = StudyLimits { max_working_bytes: daily_only_budget, ..limits.clone() };
    let full_budget = match run(&daily_only, &CancelToken::new()).unwrap_err() {
        StudyError::ResourceLimit { resource, requested, .. } if resource == "continuous_working_bytes" => requested,
        other => panic!("unexpected minute admission error: {other}"),
    };
    assert!(full_budget > daily_only_budget);
    let exact = StudyLimits { max_working_bytes: full_budget, ..limits.clone() };
    assert!(run(&exact, &CancelToken::new()).unwrap().is_some());
    for _ in 0..8 {
        let cancel = CancelToken::new();
        cancel.cancel();
        assert!(run(&limits, &cancel).unwrap().is_none());
        let completed = run(&limits, &CancelToken::new()).unwrap().unwrap();
        assert_eq!(completed.selected_minutes.unwrap().points.len(), 2,
            "a cancelled/failed call must not leak partial counters into the next call");
    }
}

#[test]
fn selected_daily_capture_keeps_censored_gap_filtered_and_missing_rows_null() {
    let (spec, data, raw) = observed_fixture();
    let pair = ContinuousCaptureCell { tp_pct: 4.0, sl_pct: 4.0 };
    let result = capture_and_assert_identical(&spec, &data, &raw, &candidate(&spec), pair);
    assert_eq!(result.evaluation.diagnostics[0].horizon_expired_count, 1);
    assert_eq!(result.evaluation.diagnostics[0].right_censored_count, 1);
    assert_eq!(result.selected_daily.unwrap().points[0].mean_r, None);

    let mut shifted = candidate(&spec); shifted.analytics.time_shift_minutes = 2;
    let result = capture_and_assert_identical(&spec, &data, &raw, &shifted, pair);
    assert_eq!(result.evaluation.diagnostics[0].future_entry_count, 1);
    assert_eq!(result.selected_daily.unwrap().points[0].mean_r, None);

    let mut missing = data.clone(); missing.instruments[0].candles.remove(0);
    let result = capture_and_assert_identical(&spec, &missing, &raw, &candidate(&spec), pair);
    assert_eq!(result.evaluation.diagnostics[0].missing_entry_count, 1);
    assert_eq!(result.selected_daily.unwrap().points[0].mean_r, None);

    let mut filtered = candidate(&spec); filtered.analytics.price_change_pct = Some(1.0);
    let result = capture_and_assert_identical(&spec, &data, &raw, &filtered, pair);
    assert_eq!(result.evaluation.evaluation.summary.selected_setup_count, 0);
    assert_eq!(result.evaluation.evaluation.matrix[0].price_change_filtered_count, 2);
    let point = result.selected_daily.unwrap().points.remove(0);
    assert_eq!((point.row_count,point.tp_count,point.sl_count,point.mean_r),(2,0,0,None));

    let mut gap = data; gap.instruments[0].candles.remove(2);
    gap.instruments[0].candles[2].low = 80.0;
    let result = capture_and_assert_identical(&spec, &gap, &raw, &candidate(&spec), pair);
    assert_eq!(result.evaluation.diagnostics[0].observation_gap_count, 1);
    assert_eq!(result.selected_daily.unwrap().points[0].mean_r, None,
        "a later TP cannot bridge an unknown earlier minute");
}

#[test]
fn selected_daily_capture_uses_existing_simultaneous_touch_sl_classifier() {
    let (spec, mut data, raw) = observed_fixture();
    data.instruments[0].candles[1].low = 80.0;
    data.instruments[0].candles[1].high = 120.0;
    let result = capture_and_assert_identical(&spec, &data, &raw, &candidate(&spec),
        ContinuousCaptureCell { tp_pct:4.0,sl_pct:4.0 });
    let point = result.selected_daily.unwrap().points.remove(0);
    assert_eq!((point.tp_count,point.sl_count,point.mean_r),(0,1,Some(-1.0)));
}

#[test]
fn selected_daily_capture_follows_causal_cluster_prehistory_and_raw_chaining() {
    let mut spec = request(); spec.analytics_params.cluster_enabled = true;
    spec.analytics_params.cluster_gap_minutes = 8;
    let data = snapshot(1500); let limits = StudyLimits::default(); let cancel = CancelToken::new();
    let raw = materialize_continuous(&spec,&data,&candidate(&spec),context(&spec),&limits,&cancel).unwrap().unwrap();
    assert!(raw.instruments[0].previous_raw_close_unix.is_some());
    let pair = ContinuousCaptureCell {tp_pct:4.0,sl_pct:4.0};
    let result = capture_and_assert_identical(&spec,&data,&raw,&candidate(&spec),pair);
    assert!(result.selected_daily.unwrap().points.is_empty(),
        "pre-period raw seed suppresses the whole chained cluster, not only its first row");
    let mut off = candidate(&spec); off.analytics.cluster_enabled = false;
    let result = capture_and_assert_identical(&spec,&data,&raw,&off,pair);
    assert_eq!(result.selected_daily.unwrap().points.iter().map(|p|p.row_count).sum::<u64>(), raw.instruments[0].setups.len() as u64);
}

#[test]
fn selected_daily_capture_groups_raw_close_utc_not_exit_and_omits_absent_days() {
    let (mut spec, mut data, mut raw) = observed_fixture();
    spec.from_unix=86_340; spec.to_unix=864_180; spec.horizon_minutes=1;
    raw.from_unix=spec.from_unix; raw.to_unix=spec.to_unix; raw.context=context(&spec);
    let template=raw.instruments[0].setups[0].clone();
    raw.instruments[0].setups=[86_340,86_400,864_060].into_iter().map(|close| {
        let mut setup=template.clone(); setup.open_time_unix=close-60; setup.close_time_unix=close; setup
    }).collect();
    raw.instruments[0].last_raw_close_unix=Some(864_060);
    data.instruments[0].candles=[86_280,86_340,86_400,864_000,864_060].into_iter().map(|open| OhlcCandle {
        open_time_unix:open,open:100.0,close:100.0,
        high:if open==86_400 {110.0} else {100.0},
        low:if open==86_340 || open==864_060 {90.0} else {100.0},
    }).collect();
    let result=capture_and_assert_identical(&spec,&data,&raw,&candidate(&spec),ContinuousCaptureCell {tp_pct:4.0,sl_pct:4.0});
    let points=result.selected_daily.unwrap().points;
    assert_eq!(points.iter().map(|p|p.day_start_unix).collect::<Vec<_>>(),[0,86_400,864_000]);
    assert_eq!(points.iter().map(|p|p.mean_r).collect::<Vec<_>>(),[Some(1.0),Some(-1.0),Some(1.0)]);
    assert_eq!(points.len(),3,"no fake zero buckets for days 2..9");
}

#[test]
fn selected_daily_capture_is_admitted_bounded_cancelled_and_released_per_call() {
    let (spec,data,raw)=observed_fixture(); let pair=ContinuousCaptureCell {tp_pct:4.0,sl_pct:4.0};
    assert!(analyze_continuous_with_daily(&spec,&data,&raw,&candidate(&spec),&StudyLimits::default(),&CancelToken::new(),
        Some(ContinuousCaptureCell {tp_pct:99.0,sl_pct:4.0})).is_err());
    let cancel=CancelToken::new();cancel.cancel();
    assert!(analyze_continuous_with_daily(&spec,&data,&raw,&candidate(&spec),&StudyLimits::default(),&cancel,Some(pair)).unwrap().is_none());
    let mut long_spec=spec.clone(); long_spec.to_unix=long_spec.from_unix+3660*86_400;
    let mut long_raw=raw.clone(); long_raw.to_unix=long_spec.to_unix;
    let limits=StudyLimits {max_working_bytes:200_000,..StudyLimits::default()};
    assert!(analyze_continuous(&long_spec,&data,&long_raw,&candidate(&long_spec),&limits,&CancelToken::new()).unwrap().is_some());
    assert!(matches!(analyze_continuous_with_daily(&long_spec,&data,&long_raw,&candidate(&long_spec),&limits,&CancelToken::new(),Some(pair)),
        Err(StudyError::ResourceLimit {resource,..}) if resource=="continuous_working_bytes"));
    for _ in 0..8 {capture_and_assert_identical(&spec,&data,&raw,&candidate(&spec),pair);}
}

#[test]
fn cutoff_maturity_cancellation_and_resource_limits_fail_closed() {
    let mut spec = request();
    let limits = StudyLimits::default();
    let context = context(&spec);
    assert!(matches!(
        evaluate_continuous(
            &spec,
            &snapshot(1501),
            &candidate(&spec),
            context,
            &limits,
            &CancelToken::new()
        ),
        Err(StudyError::InvalidInput(_))
    ));
    spec.setup_maturity = 0.1;
    assert!(validate_continuous_request(&spec, context).is_err());
    spec.setup_maturity = 0.0;
    let cancel = CancelToken::new();
    cancel.cancel();
    assert!(
        materialize_continuous(
            &spec,
            &snapshot(1500),
            &candidate(&spec),
            context,
            &limits,
            &cancel
        )
        .unwrap()
        .is_none()
    );
    let tiny = StudyLimits {
        max_setups_per_candidate: 1,
        ..limits
    };
    assert!(matches!(
        materialize_continuous(
            &spec,
            &snapshot(1500),
            &candidate(&spec),
            context,
            &tiny,
            &CancelToken::new()
        ),
        Err(StudyError::ResourceLimit { .. })
    ));
}

fn cell(tp: f64, sl: f64, risk: f64) -> MatrixCell {
    MatrixCell {
        tp_pct: tp,
        sl_pct: sl,
        risk_expectancy: Some(risk),
        avg_move_pct: Some(risk * sl),
        closed_count: 1,
        tp_count: 1,
        sl_count: 0,
        unclosed_count: 0,
        no_data_count: 0,
        price_change_filtered_count: 0,
        mean_risk_expectancy: None,
        forecast_risk_expectancy: None,
    }
}

#[test]
fn robust_selection_sorts_axes_clips_windows_and_rejects_any_invalid_cell() {
    let sl = [7.0, 1.0, 6.0, 2.0, 5.0, 3.0, 4.0];
    let matrix: Vec<_> = sl
        .iter()
        .map(|&sl| {
            cell(
                2.0,
                sl,
                match sl as i32 {
                    1 | 3 => 0.0,
                    2 => 10.0,
                    _ => 2.0,
                },
            )
        })
        .collect();
    let selected = select_robust_cell(&matrix, &[2.0], &sl, 1)
        .unwrap()
        .unwrap();
    assert_eq!(
        (
            selected.tp_pct,
            selected.sl_pct,
            selected.raw_r,
            selected.robust_score
        ),
        (2.0, 4.0, 2.0, 2.0)
    );
    let edge = select_robust_cell(
        &[cell(2.0, 1.0, 4.0), cell(2.0, 2.0, 2.0)],
        &[2.0],
        &[2.0, 1.0],
        1,
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        (edge.raw_r, edge.robust_score, edge.mean_r),
        (4.0, 3.0, 3.0)
    );
    let single = select_robust_cell(&[cell(2.0, 1.0, -0.2)], &[2.0], &[1.0], 1)
        .unwrap()
        .unwrap();
    assert_eq!(single.raw_r, single.robust_score);
    let mut invalid = matrix.clone();
    invalid[0].risk_expectancy = None;
    assert!(
        select_robust_cell(&invalid, &[2.0], &sl, 1)
            .unwrap()
            .is_none()
    );
    invalid[0].risk_expectancy = Some(f64::NAN);
    assert!(
        select_robust_cell(&invalid, &[2.0], &sl, 1)
            .unwrap()
            .is_none()
    );
    assert!(
        select_robust_cell(
            &[cell(2.0, 1.0, 1.0), cell(2.0, 1.0, 1.0)],
            &[2.0],
            &[1.0, 2.0],
            1
        )
        .is_err()
    );
}

#[test]
fn recommendation_gate_is_unrounded_whole_mean_and_payload_keeps_direction_and_raw_r() {
    let (mut spec, data, raw) = observed_fixture();
    spec.direction = TradeDirection::Long;
    let mut result = analyze_continuous(
        &spec,
        &data,
        &raw,
        &candidate(&spec),
        &StudyLimits::default(),
        &CancelToken::new(),
    )
    .unwrap()
    .unwrap();
    result.evaluation.matrix = vec![cell(4.0, 4.0, 0.1499999999)];
    assert!(
        select_robust_cell(
            &result.evaluation.matrix,
            &spec.tp_levels,
            &spec.sl_levels,
            1
        )
        .unwrap()
        .is_some()
    );
    assert!(
        recommend_continuous(
            &spec,
            &candidate(&spec),
            &result,
            &raw.provenance,
            raw.context,
            DEFAULT_CONTINUOUS_MEAN_R
        )
        .unwrap()
        .is_none()
    );
    result.evaluation.matrix[0] = cell(4.0, 4.0, 0.15);
    let recommendation = recommend_continuous(
        &spec,
        &candidate(&spec),
        &result,
        &raw.provenance,
        raw.context,
        DEFAULT_CONTINUOUS_MEAN_R,
    )
    .unwrap()
    .unwrap();
    assert_eq!(recommendation.direction, RecommendationDirection::Long);
    assert_eq!(recommendation.raw_r, 0.15);
    let json = serde_json::to_value(recommendation).unwrap();
    assert_eq!(json["direction"], "long");
    assert_eq!(json["rawR"], 0.15);
    assert_eq!(json["provenance"]["observationCutoff"], 360);
    spec.min_setups_per_day = 100_000.0;
    assert!(
        recommend_continuous(
            &spec,
            &candidate(&spec),
            &result,
            &raw.provenance,
            raw.context,
            0.15
        )
        .unwrap()
        .is_none()
    );
}

#[test]
fn forecast_gate_uses_chosen_metric_not_source_mean_and_keeps_period_provenance() {
    let (mut spec, data, raw) = observed_fixture();
    let mut result = analyze_continuous(&spec, &data, &raw, &candidate(&spec), &StudyLimits::default(), &CancelToken::new()).unwrap().unwrap();
    spec.trend_mode = TrendMode::Forecast;
    spec.trend_bend_days = TrendBendDays::Seven;
    for (forecast, period_mean, threshold, accepted) in [
        (0.1499999999, 2.0, 0.15, false), (0.15, -0.4, 0.15, false),
        (0.1500000001, -0.4, 0.15, true), (-0.1, 2.0, 0.0, false),
        (0.0, 2.0, 0.0, false), (0.01, -0.5, 0.0, true),
    ] {
        spec.min_forecast_expectancy = threshold;
        let mut value = cell(4.0, 4.0, forecast);
        value.mean_risk_expectancy = Some(period_mean);
        result.evaluation.matrix = vec![value];
        let recommendation = recommend_continuous(&spec, &candidate(&spec), &result, &raw.provenance, raw.context, 99.0).unwrap();
        assert_eq!(recommendation.is_some(), accepted);
        if let Some(recommendation) = recommendation {
            assert_eq!(recommendation.raw_r, period_mean);
            assert_eq!(recommendation.mean_r, period_mean);
            let metric = recommendation.search_forecast.unwrap();
            assert_eq!(metric.cell_r, forecast);
            assert_eq!(metric.matrix_r, forecast);
            assert_eq!(metric.bend_days, TrendBendDays::Seven);
        }
    }
    spec.trend_mode = TrendMode::Mean;
    result.evaluation.matrix = vec![cell(4.0, 4.0, -0.1)];
    assert!(recommend_continuous(&spec, &candidate(&spec), &result, &raw.provenance, raw.context, -1.0).unwrap().unwrap().search_forecast.is_none());
}

#[test]
fn blend_gate_uses_the_forward_threshold_and_not_mean_or_pure_forecast() {
    let (mut spec,data,raw)=observed_fixture();
    let mut result=analyze_continuous(&spec,&data,&raw,&candidate(&spec),&StudyLimits::default(),&CancelToken::new()).unwrap().unwrap();
    spec.trend_mode=TrendMode::Blend;
    for (score,threshold,accepted) in [(0.0,0.0,false),(0.01,0.0,true),(0.2,0.2,false),(0.20000001,0.2,true)] {
        spec.min_forecast_expectancy=threshold;
        let mut mixed=cell(4.0,4.0,score);mixed.mean_risk_expectancy=Some(-0.5);mixed.forecast_risk_expectancy=Some(0.8);
        result.evaluation.matrix=vec![mixed];
        let next=recommend_continuous(&spec,&candidate(&spec),&result,&raw.provenance,raw.context,99.0).unwrap();
        assert_eq!(next.is_some(),accepted);
        if let Some(next)=next {
            assert_eq!(next.raw_r,-0.5);let forecast=next.search_forecast.unwrap();assert_eq!(forecast.cell_r,0.8);
            let blend=forecast.blend.unwrap();assert_eq!(blend.lambda,spec.trend_blend_lambda);assert_eq!(blend.matrix_r,score);
        }
    }
}

#[test]
fn compact_bootstrap_preserves_exact_scope_and_does_not_serialize_raw_sets() {
    let spec = request();
    let raw = materialize_continuous(
        &spec,
        &snapshot(1500),
        &candidate(&spec),
        context(&spec),
        &StudyLimits::default(),
        &CancelToken::new(),
    )
    .unwrap()
    .unwrap();
    let bootstrap = continuous_bootstrap(&spec, &raw).unwrap();
    assert_eq!(bootstrap.export_from_unix, spec.from_unix);
    assert_eq!(bootstrap.export_to_unix, spec.to_unix);
    assert_eq!(
        bootstrap.observation_cutoff,
        context(&spec).observation_cutoff
    );
    assert_eq!(bootstrap.next_open_unix, minute(1499));
    assert_eq!(bootstrap.emit_from_close_unix, minute(1501));
    assert_eq!(
        bootstrap.instruments[0].last_raw_cluster_time,
        raw.instruments[0].last_raw_close_unix
    );
    assert_eq!(
        bootstrap.instruments[0]
            .last_raw_open_unix
            .map(|time| time + 60),
        raw.instruments[0].last_raw_close_unix
    );
    let json = serde_json::to_value(&bootstrap).unwrap();
    assert_eq!(json["nextOpenUnix"], minute(1499));
    assert_eq!(json["detectorWarmupFromUnix"], minute(4));
    assert!(json["instruments"][0].get("setups").is_none());
    assert!(serde_json::to_vec(&bootstrap).unwrap().len() < 2048);
    assert_eq!(
        serde_json::from_value::<ContinuousBootstrap>(json).unwrap(),
        bootstrap
    );
    let mut wrong_scope = spec.clone();
    wrong_scope.from_unix += 60;
    assert!(continuous_bootstrap(&wrong_scope, &raw).is_err());
    let mut wrong_seed = raw;
    wrong_seed.instruments[0].last_raw_close_unix = Some(spec.to_unix + 60);
    assert!(continuous_bootstrap(&spec, &wrong_seed).is_err());
}

#[test]
fn bootstrap_seeded_worker_style_continuation_matches_cold_detector_and_cluster() {
    for offset in [0, 17] {
        for cluster_enabled in [false, true] {
            let mut spec = request();
            spec.to_unix += offset;
            spec.analytics_params.cluster_enabled = cluster_enabled;
            spec.analytics_params.cluster_gap_minutes = 8;
            let params = AlgorithmParams::Spike(SpikeParams {
                baseline_long_days: 1,
                baseline_short_days: 1,
                window_minutes: 1,
                price_growth_pct: "0".into(),
                volume_multiplier: "1".into(),
                cooldown_minutes: 3,
                ..SpikeParams::default()
            });
            spec.algorithm_params =
                serde_json::from_value(serde_json::to_value(&params).unwrap()).unwrap();
            // The actual winner needs one day, not this maximum two-day envelope.
            spec.algorithm_search
                .insert("baselineLongDays".into(), vec![json!(1), json!(2)]);
            let mut complete = snapshot(1530);
            // Preserve a known historical hole during reconstruction as well.
            complete.instruments[0]
                .detector_bars
                .retain(|bar| bar.open_time_unix != minute(1480));
            complete.instruments[0]
                .candles
                .retain(|bar| bar.open_time_unix != minute(1480));
            let context = context(&spec);
            let mut frozen = complete.clone();
            frozen.instruments[0]
                .detector_bars
                .retain(|bar| bar.open_time_unix + 60 <= context.observation_cutoff);
            frozen.instruments[0]
                .candles
                .retain(|bar| bar.open_time_unix + 60 <= context.observation_cutoff);
            let raw = materialize_continuous(
                &spec,
                &frozen,
                &candidate(&spec),
                context,
                &StudyLimits::default(),
                &CancelToken::new(),
            )
            .unwrap()
            .unwrap();
            let bootstrap = continuous_bootstrap(&spec, &raw).unwrap();
            let envelope = required_continuous_input_bounds(&spec, context).unwrap();
            assert_eq!(
                bootstrap.detector_warmup_from_unix - envelope.detector_from_unix,
                1440 * 60
            );
            assert_eq!(
                bootstrap.next_open_unix,
                if offset == 0 {
                    minute(1499)
                } else {
                    minute(1500)
                }
            );
            assert_eq!(bootstrap.export_to_unix, spec.to_unix);
            let bars = &complete.instruments[0].detector_bars;
            let mut cold = PriceVolumeSpikeDetector::with_params(params.clone(), None).unwrap();
            let mut all_raw = Vec::new();
            for bar in bars
                .iter()
                .filter(|bar| bar.open_time_unix >= bootstrap.detector_warmup_from_unix)
            {
                if let Some(setup) = cold.push(bar).unwrap().setup {
                    all_raw.push(setup);
                }
            }
            let seed = &bootstrap.instruments[0];
            let mut resumed =
                PriceVolumeSpikeDetector::with_params(params.clone(), seed.last_raw_open_unix)
                    .unwrap();
            // Exact reconstruction formula used by scan_adaptive_pages:
            // N-1 preceding calendar minutes, then the first unprocessed minute.
            let read_from =
                (bootstrap.next_open_unix - i64::from(params.baseline_long_days()) * 86400 + 60)
                    .max(bootstrap.detector_warmup_from_unix);
            let mut continued_raw = Vec::new();
            for bar in bars.iter().filter(|bar| bar.open_time_unix >= read_from) {
                let pushed = resumed.push(bar).unwrap();
                if bar.open_time_unix >= bootstrap.next_open_unix
                    && let Some(setup) = pushed.setup
                {
                    continued_raw.push(setup);
                }
            }
            let expected_raw: Vec<_> = all_raw
                .iter()
                .filter(|setup| setup.open_time_unix >= bootstrap.next_open_unix)
                .cloned()
                .collect();
            assert!(!continued_raw.is_empty());
            assert_eq!(continued_raw, expected_raw);
            if offset == 0 {
                assert_eq!(
                    continued_raw[0].close_time_unix, context.observation_cutoff,
                    "excluded export endpoint must be processed once, not skipped as already observed"
                );
            }
            let mut full_cluster = CausalCluster::new(8 * 60, None).unwrap();
            let mut expected_live = Vec::new();
            for setup in &all_raw {
                let first = full_cluster.observe_raw(setup.close_time_unix).unwrap();
                if (first || !cluster_enabled)
                    && setup.close_time_unix >= bootstrap.emit_from_close_unix
                {
                    expected_live.push(setup.close_time_unix);
                }
            }
            let mut resumed_cluster =
                CausalCluster::new(8 * 60, seed.last_raw_cluster_time).unwrap();
            let mut continued_live = Vec::new();
            for setup in continued_raw {
                let first = resumed_cluster.observe_raw(setup.close_time_unix).unwrap();
                if (first || !cluster_enabled)
                    && setup.close_time_unix >= bootstrap.emit_from_close_unix
                {
                    continued_live.push(setup.close_time_unix);
                }
            }
            assert_eq!(continued_live, expected_live);
        }
    }
}
