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
