use serde_json::{Value, json};
use setups_analytics::{
    AnalyticsSetup, EvaluateLimitOptions, OhlcCandle, TradeDirection, aggregate_limit_outcomes,
    evaluate_limit, passes_price_change, select_by_maturity,
};
use setups_detector::PriceVolumeSpikeDetector;
use setups_domain::{AlgorithmParams, DetectorBar, SpikeDropParams, SpikeParams};
use setups_optimizer::*;
use std::collections::BTreeMap;

const T0: i64 = 1_699_999_980;
fn minute(index: i64) -> i64 {
    T0 + index * 60
}
fn instrument() -> StudyInstrument {
    StudyInstrument {
        exchange: "bybit".into(),
        category: "linear".into(),
        symbol: "FIXTUREUSDT".into(),
    }
}

fn spec() -> StudyRequest {
    let params = SpikeParams {
        baseline_long_days: 1,
        baseline_short_days: 1,
        ..SpikeParams::default()
    };
    StudyRequest {
        algorithm_id: "price_volume_spike".into(),
        algorithm_version: "1.1.1".into(),
        algorithm_params: serde_json::from_value(json!(params)).unwrap(),
        algorithm_search: BTreeMap::new(),
        analytics_params: AnalyticsParams {
            cluster_enabled: false,
            ..AnalyticsParams::default()
        },
        analytics_search: BTreeMap::new(),
        instruments: vec![instrument()],
        from_unix: minute(1440),
        to_unix: minute(1500),
        direction: TradeDirection::Short,
        setup_maturity: 0.0,
        tp_levels: default_levels(),
        sl_levels: default_levels(),
        horizon_minutes: 30,
        min_closed_per_cell: 1,
        min_setups_per_day: 4.0,
        max_trials: 32,
        max_seconds: 30,
        seed: 42,
    }
}

fn input() -> StudyInput {
    let mut detector_bars = Vec::new();
    let mut candles = Vec::new();
    for index in 0..1560 {
        let close = if index < 1435 {
            "100.00000000".to_string()
        } else {
            format!("{:.8}", 100.0 * 1.13f64.powi((index - 1434) / 5))
        };
        let value: f64 = close.parse().unwrap();
        detector_bars.push(DetectorBar {
            open_time_unix: minute(i64::from(index)),
            close,
            quote_turnover: if index < 1435 {
                "10".into()
            } else {
                "3000".into()
            },
        });
        let (high, low) = if index % 7 == 0 {
            (value * 1.31, value * 0.69)
        } else {
            (value * 1.02, value * 0.98)
        };
        candles.push(OhlcCandle {
            open_time_unix: minute(i64::from(index)),
            open: value,
            high,
            low,
            close: value,
        });
    }
    StudyInput {
        instruments: vec![InstrumentInput {
            instrument: instrument(),
            detector_bars,
            candles,
        }],
        snapshot_hash: "fixture-sha256".into(),
        captured_at: "2026-11-11T00:00:00Z".into(),
    }
}

fn candidate(spec: &StudyRequest) -> StudyCandidate {
    StudyCandidate {
        params: spec.algorithm_params.clone(),
        analytics: spec.analytics_params.clone(),
    }
}

fn six_setups_over_ten_days() -> (StudyRequest, StudyInput) {
    let mut spec = spec();
    spec.to_unix = spec.from_unix + 10 * 86400;
    spec.direction = TradeDirection::Long;
    spec.tp_levels = vec![20.0];
    spec.sl_levels = vec![1.0];
    let mut data = input();
    let flat_close = data.instruments[0].detector_bars[1464].close.clone();
    for bar in &mut data.instruments[0].detector_bars {
        if bar.open_time_unix >= minute(1465) {
            bar.close = flat_close.clone();
            bar.quote_turnover = "10".into();
        }
    }
    let instrument = &mut data.instruments[0];
    for (bar, candle) in instrument.detector_bars.iter().zip(&mut instrument.candles) {
        let close: f64 = bar.close.parse().unwrap();
        candle.open = close;
        candle.close = close;
        candle.low = close;
        candle.high = close * 1.25;
    }
    (spec, data)
}

#[test]
fn default_frequency_gate_rejects_six_selected_setups_over_ten_days_even_with_high_r() {
    let (spec, data) = six_setups_over_ten_days();
    let evaluated = evaluate_candidate(
        &spec,
        &data,
        &candidate(&spec),
        &StudyLimits::default(),
        &CancelToken::new(),
    )
    .unwrap()
    .unwrap();
    assert_eq!(evaluated.summary.setup_count, 6);
    assert_eq!(evaluated.summary.selected_setup_count, 6);
    assert_eq!(evaluated.matrix[0].risk_expectancy, Some(20.0));
    assert_eq!(evaluated.summary.score, None);
    assert_eq!(evaluated.summary.avg_setups_per_day, Some(0.6));
    assert_eq!(evaluated.summary.required_setup_count, Some(40));
    let outcome = run_study(
        &spec,
        &data,
        &StudyLimits::default(),
        &CancelToken::new(),
        |_| {},
    )
    .unwrap();
    assert!(outcome.best.is_none());
    assert_eq!(outcome.progress.scorable_candidates, 0);
    assert_eq!(outcome.baseline.unwrap(), evaluated.summary);
}

#[test]
fn frequency_exact_boundary_disabled_zero_and_closed_cell_gate_are_independent() {
    let (mut spec, data) = six_setups_over_ten_days();
    for (threshold, score, required) in [
        (0.6, Some(20.0), 6),
        (0.6000000000000001, None, 7),
        (0.0, Some(20.0), 0),
    ] {
        spec.min_setups_per_day = threshold;
        let evaluated = evaluate_candidate(
            &spec,
            &data,
            &candidate(&spec),
            &StudyLimits::default(),
            &CancelToken::new(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(evaluated.summary.score, score, "threshold={threshold}");
        assert_eq!(evaluated.summary.required_setup_count, Some(required));
        assert_eq!(evaluated.summary.avg_setups_per_day, Some(0.6));
    }
    spec.min_setups_per_day = 0.0;
    spec.min_closed_per_cell = 7;
    let evaluated = evaluate_candidate(
        &spec,
        &data,
        &candidate(&spec),
        &StudyLimits::default(),
        &CancelToken::new(),
    )
    .unwrap()
    .unwrap();
    assert_eq!(evaluated.matrix[0].closed_count, 6);
    assert_eq!(
        evaluated.summary.score, None,
        "disabled frequency gate must not disable minClosedPerCell"
    );
}

#[test]
fn frequency_uses_exact_signal_range_including_fractional_days_and_reports_required_count() {
    let (mut spec, data) = six_setups_over_ten_days();
    spec.to_unix += 60;
    let evaluated = evaluate_candidate(
        &spec,
        &data,
        &candidate(&spec),
        &StudyLimits::default(),
        &CancelToken::new(),
    )
    .unwrap()
    .unwrap();
    assert_eq!(spec.to_unix - spec.from_unix, 864060);
    assert_eq!(evaluated.summary.required_setup_count, Some(41));
    assert_eq!(
        evaluated.summary.avg_setups_per_day,
        Some(6.0 * 86400.0 / 864060.0)
    );
    assert_eq!(evaluated.summary.score, None);

    spec.to_unix = spec.from_unix + 129600; // 1.5 days, not two calendar days.
    let result = run_study(
        &spec,
        &data,
        &StudyLimits::default(),
        &CancelToken::new(),
        |_| {},
    )
    .unwrap();
    let best = result.best.unwrap();
    assert_eq!(best.support.avg_setups_per_day, Some(4.0));
    assert_eq!(best.support.required_setup_count, Some(6));
    assert_eq!(best.score, 20.0);
    assert!(
        best.conclusions
            .iter()
            .any(|line| line.contains("4.000000 сетапов/сутки") && line.contains("6 сетапов"))
    );
}

#[test]
fn frequency_counts_clustering_selection_before_price_gate_and_no_data_outcomes() {
    let (mut spec, data) = six_setups_over_ten_days();
    spec.min_setups_per_day = 0.2;
    spec.analytics_params.cluster_enabled = true;
    let evaluated = evaluate_candidate(
        &spec,
        &data,
        &candidate(&spec),
        &StudyLimits::default(),
        &CancelToken::new(),
    )
    .unwrap()
    .unwrap();
    assert_eq!(evaluated.summary.setup_count, 6);
    assert_eq!(evaluated.summary.selected_setup_count, 1);
    assert_eq!(evaluated.summary.avg_setups_per_day, Some(0.1));
    assert_eq!(evaluated.summary.required_setup_count, Some(2));
    assert_eq!(evaluated.matrix[0].risk_expectancy, Some(20.0));
    assert_eq!(
        evaluated.summary.score, None,
        "raw pre-clustering count must not pass frequency gate"
    );

    spec.analytics_params.cluster_enabled = false;
    spec.min_setups_per_day = 0.6;
    for (shift, price_gate) in [(0, Some(1.0)), (43200, None)] {
        let mut candidate = candidate(&spec);
        candidate.analytics.time_shift_minutes = shift;
        candidate.analytics.price_change_pct = price_gate;
        let evaluated = evaluate_candidate(
            &spec,
            &data,
            &candidate,
            &StudyLimits::default(),
            &CancelToken::new(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(evaluated.summary.selected_setup_count, 6);
        assert_eq!(evaluated.summary.avg_setups_per_day, Some(0.6));
        assert_eq!(evaluated.summary.required_setup_count, Some(6));
        assert_eq!(
            evaluated.matrix[0].price_change_filtered_count + evaluated.matrix[0].no_data_count,
            6
        );
        assert_eq!(
            evaluated.summary.score, None,
            "per-cell support still required after frequency passes"
        );
    }
}

#[test]
fn frequency_request_validation_defaults_and_legacy_support_are_explicit() {
    let mut spec = spec();
    let fields = default_parameter_descriptors(&spec.algorithm_id).unwrap();
    for threshold in [-1.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        spec.min_setups_per_day = threshold;
        assert!(validate_request(&spec, &fields).is_err());
    }
    for threshold in [0.0, 0.6, 4.0, f64::MAX] {
        spec.min_setups_per_day = threshold;
        validate_request(&spec, &fields).unwrap();
    }
    spec.min_setups_per_day = 4.0;
    spec.analytics_search
        .insert("minSetupsPerDay".into(), vec![json!(1)]);
    assert!(validate_request(&spec, &fields).is_err());
    spec.analytics_search.clear();
    spec.algorithm_search
        .insert("minSetupsPerDay".into(), vec![json!(1)]);
    assert!(validate_request(&spec, &fields).is_err());
    let legacy: TrialSummary = serde_json::from_value(json!({
        "score":0.47681,"setupCount":6,"selectedSetupCount":6,"evaluatedMinutes":10,"warmupMinutes":0
    })).unwrap();
    assert_eq!(legacy.score, Some(0.47681));
    assert_eq!(legacy.avg_setups_per_day, None);
    assert_eq!(legacy.required_setup_count, None);
}

fn scalar_oracle(
    spec: &StudyRequest,
    input: &StudyInput,
    candidate: &StudyCandidate,
) -> (Vec<MatrixCell>, usize, usize) {
    let params = if spec.algorithm_id == "price_drop_volume_spike" {
        AlgorithmParams::SpikeDrop(serde_json::from_value(json!(candidate.params)).unwrap())
    } else {
        AlgorithmParams::Spike(serde_json::from_value(json!(candidate.params)).unwrap())
    };
    let mut selected = Vec::new();
    let mut setup_count = 0;
    for data in &input.instruments {
        let mut detector = PriceVolumeSpikeDetector::with_params(params.clone(), None).unwrap();
        let mut setups = Vec::new();
        for bar in &data.detector_bars {
            if bar.open_time_unix
                < spec.from_unix - 60 - i64::from(params.baseline_long_days()) * 1440 * 60
            {
                continue;
            }
            if bar.open_time_unix >= spec.to_unix - 60 {
                break;
            }
            if let Some(setup) = detector.push(bar).unwrap().setup
                && setup.close_time_unix >= spec.from_unix
                && setup.close_time_unix < spec.to_unix
            {
                setups.push(AnalyticsSetup {
                    exchange_code: data.instrument.exchange.clone(),
                    category: data.instrument.category.clone(),
                    symbol: data.instrument.symbol.clone(),
                    open_time_unix: setup.open_time_unix,
                    close_time_unix: setup.close_time_unix,
                    close_price: setup.close_price,
                });
            }
        }
        setup_count += setups.len();
        if candidate.analytics.cluster_enabled {
            setups = select_by_maturity(
                &setups,
                spec.setup_maturity,
                candidate.analytics.cluster_gap_minutes * 60,
            )
            .rows;
        }
        selected.extend(setups);
    }
    let matrix = scalar_matrix_from_setups(spec, input, candidate, &selected);
    (matrix, setup_count, selected.len())
}

fn scalar_matrix_from_setups(
    spec: &StudyRequest,
    input: &StudyInput,
    candidate: &StudyCandidate,
    selected: &[AnalyticsSetup],
) -> Vec<MatrixCell> {
    let mut matrix = Vec::new();
    for &tp in &spec.tp_levels {
        for &sl in &spec.sl_levels {
            let mut evaluations = Vec::new();
            let mut filtered = 0;
            for setup in selected {
                let data = input
                    .instruments
                    .iter()
                    .find(|data| data.instrument.symbol == setup.symbol)
                    .unwrap();
                if let Some(pct) = candidate.analytics.price_change_pct
                    && let Some((entry, _)) = setups_analytics::resolve_entry(
                        &data.candles,
                        setup,
                        candidate.analytics.time_shift_minutes,
                    )
                    && !passes_price_change(
                        setup.close_price.parse().unwrap(),
                        entry,
                        spec.direction,
                        pct,
                    )
                {
                    filtered += 1;
                    continue;
                }
                evaluations.push(evaluate_limit(EvaluateLimitOptions {
                    candles: &data.candles,
                    setup,
                    direction: spec.direction,
                    tp_pct: tp,
                    sl_pct: sl,
                    time_shift_minutes: candidate.analytics.time_shift_minutes,
                    horizon_minutes: spec.horizon_minutes,
                }));
            }
            let aggregate = aggregate_limit_outcomes(&evaluations, sl);
            matrix.push(MatrixCell {
                tp_pct: tp,
                sl_pct: sl,
                risk_expectancy: aggregate.risk_expectancy,
                avg_move_pct: aggregate.avg_move_pct,
                closed_count: aggregate.closed_count,
                tp_count: aggregate.tp_count,
                sl_count: aggregate.sl_count,
                unclosed_count: aggregate.unclosed_count,
                no_data_count: aggregate.no_data_count,
                price_change_filtered_count: filtered,
            });
        }
    }
    matrix
}

#[test]
fn all_81_cells_match_scalar_exactly_for_both_directions_shifts_gates_and_maturities() {
    let mut spec = spec();
    let mut input = input();
    // An actual sparse snapshot; neither detector nor analytics fill this gap.
    input.instruments[0]
        .detector_bars
        .retain(|bar| bar.open_time_unix != minute(1460));
    input.instruments[0]
        .candles
        .retain(|bar| bar.open_time_unix != minute(1460));
    for direction in [TradeDirection::Short, TradeDirection::Long] {
        spec.direction = direction;
        for (shift, gate, cluster, maturity) in [
            (0, None, false, 0.0),
            (1, Some(0.0), false, 0.0),
            (3, Some(-2.0), true, 0.5),
            (0, Some(1.0), false, 0.0),
            (5, None, true, 1.0),
        ] {
            spec.setup_maturity = maturity;
            let mut candidate = candidate(&spec);
            candidate.analytics = AnalyticsParams {
                cluster_enabled: cluster,
                cluster_gap_minutes: 10,
                time_shift_minutes: shift,
                price_change_pct: gate,
            };
            let actual = evaluate_candidate(
                &spec,
                &input,
                &candidate,
                &StudyLimits::default(),
                &CancelToken::new(),
            )
            .unwrap()
            .unwrap();
            let (expected, count, selected) = scalar_oracle(&spec, &input, &candidate);
            assert_eq!(actual.matrix.len(), 81);
            assert_eq!(
                actual.matrix, expected,
                "{direction:?}, shift={shift}, gate={gate:?}, cluster={cluster}"
            );
            assert_eq!(actual.summary.setup_count, count);
            assert_eq!(actual.summary.selected_setup_count, selected);
            assert!(count > 0);
        }
    }
}

#[test]
fn mirror_algorithm_uses_the_same_metadata_and_scalar_engine() {
    let mut spec = spec();
    spec.algorithm_id = "price_drop_volume_spike".into();
    spec.direction = TradeDirection::Long;
    spec.algorithm_params = serde_json::from_value(json!(SpikeDropParams {
        baseline_long_days: 1,
        baseline_short_days: 1,
        ..SpikeDropParams::default()
    }))
    .unwrap();
    // Inverting a +13% move gives an 11.50% fall, not a 13% fall.
    spec.algorithm_params
        .insert("priceDropPct".into(), json!("10"));
    let mut input = input();
    let data = &mut input.instruments[0];
    for (bar, candle) in data.detector_bars.iter_mut().zip(&mut data.candles) {
        let previous: f64 = bar.close.parse().unwrap();
        let price = 10_000.0 / previous;
        bar.close = format!("{price:.8}");
        let price: f64 = bar.close.parse().unwrap();
        *candle = OhlcCandle {
            open_time_unix: bar.open_time_unix,
            open: price,
            high: price * 1.3,
            low: price * 0.7,
            close: price,
        };
    }
    let actual = evaluate_candidate(
        &spec,
        &input,
        &candidate(&spec),
        &StudyLimits::default(),
        &CancelToken::new(),
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        actual.matrix,
        scalar_oracle(&spec, &input, &candidate(&spec)).0
    );
    assert!(actual.summary.setup_count > 0);
}

#[test]
fn score_is_equal_weight_raw_r_and_requires_every_cell() {
    let mut matrix = scalar_oracle(&spec(), &input(), &candidate(&spec())).0;
    matrix.truncate(2);
    matrix[0].risk_expectancy = Some(0.12345678901234566);
    matrix[0].closed_count = 1;
    matrix[1].risk_expectancy = Some(0.7654321098765432);
    matrix[1].closed_count = 1000;
    assert_eq!(
        score_matrix(&matrix, 1),
        Some((0.12345678901234566 + 0.7654321098765432) / 2.0)
    );
    assert_eq!(score_matrix(&matrix, 2), None);
    matrix[0].closed_count = 0;
    assert_eq!(score_matrix(&matrix, 1), None);
    matrix[0].closed_count = 1;
    matrix[1].risk_expectancy = None;
    assert_eq!(score_matrix(&matrix, 1), None);
    assert_eq!(score_matrix(&[], 1), None);
}

#[test]
fn schema_defaults_metadata_decimal_precision_and_fixed_fields() {
    let spec = spec();
    let mut wire = json!(spec);
    for key in [
        "setupMaturity",
        "tpLevels",
        "slLevels",
        "horizonMinutes",
        "minClosedPerCell",
        "minSetupsPerDay",
        "maxTrials",
        "maxSeconds",
    ] {
        wire.as_object_mut().unwrap().remove(key);
    }
    let defaults: StudyRequest = serde_json::from_value(wire).unwrap();
    assert_eq!(defaults.tp_levels, default_levels());
    assert_eq!(defaults.max_trials, 256);
    assert_eq!(defaults.horizon_minutes, 10080);
    assert_eq!(defaults.min_setups_per_day, 4.0);
    assert!(
        json!(StudyProgress {
            phase: StudyPhase::Initial,
            evaluated_candidates: 0,
            scorable_candidates: 0,
            max_trials: 1,
            elapsed_seconds: 0.0,
            best_score: None
        })
        .get("bestScore")
        .unwrap()
        .is_null()
    );
    let mut spec = spec;
    let fields = default_parameter_descriptors(&spec.algorithm_id).unwrap();
    spec.algorithm_params
        .insert("volumeMultiplier".into(), json!("0.000001"));
    validate_request(&spec, &fields).unwrap();
    spec.algorithm_search.insert(
        "priceGrowthPct".into(),
        vec![json!("12.000000000000000001")],
    );
    validate_request(&spec, &fields).unwrap();
    spec.analytics_search
        .insert("setupMaturity".into(), vec![json!(0.5)]);
    assert!(validate_request(&spec, &fields).is_err());
    spec.analytics_search.clear();
    spec.algorithm_search
        .insert("priceDropPct".into(), vec![json!("12")]);
    assert!(validate_request(&spec, &fields).is_err());
    spec.algorithm_search.remove("priceDropPct");
    spec.algorithm_params
        .insert("volumeMultiplier".into(), json!(3));
    assert!(validate_request(&spec, &fields).is_err());
}

#[test]
fn bounds_include_baseline_outside_domains_and_full_shift_horizon() {
    let mut spec = spec();
    spec.algorithm_params
        .insert("baselineLongDays".into(), json!(30));
    spec.algorithm_search
        .insert("baselineLongDays".into(), vec![json!(1), json!(7)]);
    spec.analytics_params.time_shift_minutes = 10;
    spec.analytics_search
        .insert("timeShiftMinutes".into(), vec![json!(0), json!(3)]);
    let bounds = required_input_bounds(&spec).unwrap();
    assert_eq!(
        bounds.detector_from_unix,
        spec.from_unix - 60 - 30 * 1440 * 60
    );
    assert_eq!(bounds.detector_to_unix, spec.to_unix - 60);
    assert_eq!(
        bounds.candles_to_unix,
        spec.to_unix - 60 + (10 + spec.horizon_minutes) * 60
    );
    assert_eq!(bounds.from_unix, bounds.detector_from_unix);
    spec.from_unix += 1;
    assert!(required_input_bounds(&spec).is_err());
}

#[test]
fn conditional_keys_are_deduplicated_but_gate_zero_differs_after_shift() {
    let spec = spec();
    let mut first = candidate(&spec);
    first.analytics.cluster_enabled = false;
    let mut second = first.clone();
    second.analytics.cluster_gap_minutes = 1;
    second.analytics.price_change_pct = Some(-1.0);
    assert_eq!(
        canonical_candidate_key(&first).unwrap(),
        canonical_candidate_key(&second).unwrap()
    );
    first.analytics.time_shift_minutes = 5;
    second = first.clone();
    second.analytics.price_change_pct = Some(0.0);
    assert_ne!(
        canonical_candidate_key(&first).unwrap(),
        canonical_candidate_key(&second).unwrap()
    );
    second = first.clone();
    second
        .params
        .insert("priceGrowthPct".into(), json!("12.000"));
    assert_eq!(
        canonical_candidate_key(&first).unwrap(),
        canonical_candidate_key(&second).unwrap()
    );
}

#[test]
fn tiny_positive_zero_shift_gate_is_not_deduplicated_with_rejecting_positive_gate() {
    for direction in [TradeDirection::Short, TradeDirection::Long] {
        assert!(passes_price_change(100.0, 100.0, direction, 1e-20));
        assert!(!passes_price_change(100.0, 100.0, direction, 1.0));
        // Check baseline-vs-domain keys AND preservation inside the domain.
        for gates in [vec![json!(1e-20)], vec![json!(1.0), json!(1e-20)]] {
            let mut spec = spec();
            spec.direction = direction;
            spec.analytics_params.price_change_pct = Some(1.0);
            spec.analytics_search.insert("priceChangePct".into(), gates);
            let result = run_study(
                &spec,
                &input(),
                &StudyLimits::default(),
                &CancelToken::new(),
                |_| {},
            )
            .unwrap();
            assert_eq!(result.baseline.as_ref().unwrap().score, None);
            assert_eq!(result.progress.evaluated_candidates, 2);
            let best = result
                .best
                .expect("tiny positive gate passes the unchanged scalar kernel");
            assert_eq!(best.analytics.price_change_pct, Some(1e-20));
            assert_eq!(best.trial_index, 1);
            assert!(best.matrix.iter().all(|cell| cell.closed_count > 0));
        }
    }
}

#[test]
fn baseline_trial_zero_is_evaluated_even_outside_domains_and_budget_one() {
    let mut spec = spec();
    spec.algorithm_search
        .insert("priceGrowthPct".into(), vec![json!("50")]);
    spec.max_trials = 1;
    let result = run_study(
        &spec,
        &input(),
        &StudyLimits::default(),
        &CancelToken::new(),
        |_| {},
    )
    .unwrap();
    assert_eq!(result.reason, TerminationReason::TrialBudget);
    assert_eq!(result.progress.evaluated_candidates, 1);
    let best = result.best.unwrap();
    assert_eq!(best.trial_index, 0);
    assert_eq!(best.params["priceGrowthPct"], json!("12"));
    assert!(!best.partial);
}

#[test]
fn search_is_deterministic_with_parallelism_and_reuses_detector_packets() {
    let mut spec = spec();
    spec.analytics_params.time_shift_minutes = 1;
    spec.analytics_search.insert(
        "timeShiftMinutes".into(),
        vec![json!(0), json!(1), json!(2), json!(3)],
    );
    spec.analytics_search
        .insert("clusterEnabled".into(), vec![json!(false), json!(true)]);
    spec.algorithm_search.insert(
        "priceGrowthPct".into(),
        vec![json!("8"), json!("12"), json!("16")],
    );
    spec.max_trials = 16;
    let input = input();
    let run = |threads| {
        run_study(
            &spec,
            &input,
            &StudyLimits {
                cpu_threads: threads,
                batch_size: 8,
                ..StudyLimits::default()
            },
            &CancelToken::new(),
            |_| {},
        )
        .unwrap()
    };
    let first = run(1);
    let second = run(3);
    assert_eq!(first.progress.evaluated_candidates, 16);
    assert_eq!(first.best, second.best);
    assert_eq!(first.baseline, second.baseline);
    assert_eq!(first.invalid_proposals, second.invalid_proposals);
    assert!(first.detector_passes < first.progress.evaluated_candidates);
    assert_eq!(
        first.best.as_ref().map(|best| best.score),
        first.progress.best_score
    );
}

#[test]
fn inactive_domains_exhaust_without_redundant_trials() {
    let mut spec = spec();
    spec.analytics_search.insert(
        "clusterGapMinutes".into(),
        vec![json!(1), json!(60), json!(480)],
    );
    spec.analytics_search.insert(
        "priceChangePct".into(),
        vec![Value::Null, json!(0), json!(-1)],
    );
    let result = run_study(
        &spec,
        &input(),
        &StudyLimits::default(),
        &CancelToken::new(),
        |_| {},
    )
    .unwrap();
    assert_eq!(result.reason, TerminationReason::SpaceExhausted);
    assert_eq!(result.progress.evaluated_candidates, 1);
}

#[test]
fn invalid_coupled_search_combinations_are_bounded_and_reported() {
    let mut spec = spec();
    spec.algorithm_search
        .insert("baselineLongDays".into(), vec![json!(1)]);
    spec.algorithm_search
        .insert("baselineShortDays".into(), vec![json!(2)]);
    let result = run_study(
        &spec,
        &input(),
        &StudyLimits::default(),
        &CancelToken::new(),
        |_| {},
    )
    .unwrap();
    assert_eq!(result.reason, TerminationReason::SpaceExhausted);
    assert_eq!(result.progress.evaluated_candidates, 1);
    assert!(result.invalid_proposals > 0 && result.invalid_proposals < 100);
}

#[test]
fn cancellation_preserves_only_complete_incumbent_and_all_limits_fail_explicitly() {
    let mut spec = spec();
    spec.algorithm_search.insert(
        "priceGrowthPct".into(),
        vec![json!("8"), json!("12"), json!("16")],
    );
    let token = CancelToken::new();
    let result = run_study(
        &spec,
        &input(),
        &StudyLimits {
            batch_size: 1,
            ..StudyLimits::default()
        },
        &token,
        |update| {
            if update.progress.evaluated_candidates == 1 {
                token.cancel();
            }
        },
    )
    .unwrap();
    assert_eq!(result.reason, TerminationReason::Cancelled);
    assert_eq!(result.progress.evaluated_candidates, 1);
    assert!(result.best.unwrap().partial);
    for limits in [
        StudyLimits {
            max_snapshot_bytes: 1,
            ..StudyLimits::default()
        },
        StudyLimits {
            max_working_bytes: 1,
            ..StudyLimits::default()
        },
        StudyLimits {
            max_setups_per_candidate: 1,
            ..StudyLimits::default()
        },
    ] {
        assert!(matches!(
            run_study(&spec, &input(), &limits, &CancelToken::new(), |_| {}),
            Err(StudyError::ResourceLimit { .. })
        ));
    }
    let cancelled = CancelToken::new();
    cancelled.cancel();
    let result = run_study(&spec, &input(), &StudyLimits::default(), &cancelled, |_| {}).unwrap();
    assert!(result.best.is_none());
    assert_eq!(result.progress.evaluated_candidates, 0);
}

#[test]
fn invalid_snapshots_and_decimal_overflows_do_not_become_no_signal_success() {
    let spec = spec();
    let mut data = input();
    data.instruments[0].detector_bars[0].quote_turnover =
        "99999999999999999999999999999999999999".into();
    assert!(matches!(
        run_study(
            &spec,
            &data,
            &StudyLimits::default(),
            &CancelToken::new(),
            |_| {}
        ),
        Err(StudyError::Detector(_))
    ));
    let mut data = input();
    data.instruments[0].candles[0].low = f64::NAN;
    assert!(matches!(
        run_study(
            &spec,
            &data,
            &StudyLimits::default(),
            &CancelToken::new(),
            |_| {}
        ),
        Err(StudyError::InvalidInput(_))
    ));
}

#[tokio::test]
async fn fresh_replay_own_warmup_and_cooldown_with_longer_shared_snapshot() {
    let mut spec = spec();
    // Real signals precede the emission boundary: starting a short candidate
    // at the shared maximum warmup would produce a different cooldown phase.
    spec.from_unix = minute(1450);
    let mut data = input();
    // Extend the shared snapshot to cover a second, longer candidate domain.
    let earlier: Vec<_> = (-1440..0)
        .map(|i| DetectorBar {
            open_time_unix: minute(i),
            close: "100".into(),
            quote_turnover: "10".into(),
        })
        .collect();
    data.instruments[0].detector_bars.splice(0..0, earlier);
    spec.algorithm_search
        .insert("baselineLongDays".into(), vec![json!(1), json!(2)]);
    for days in [1, 2] {
        let mut candidate = candidate(&spec);
        candidate
            .params
            .insert("baselineLongDays".into(), json!(days));
        let params: SpikeParams = serde_json::from_value(json!(candidate.params)).unwrap();
        let source = setups_replay::ArrayCandleSource::new(
            data.instruments[0]
                .detector_bars
                .iter()
                .map(|bar| setups_replay::CandleBarDto {
                    open_time_unix: bar.open_time_unix,
                    close: bar.close.clone(),
                    volume: "1".into(),
                    quote_turnover: bar.quote_turnover.clone(),
                })
                .collect(),
        );
        let replay = setups_replay::replay_run(setups_replay::ReplayRunOptions {
            source: &source,
            from_unix: spec.from_unix - 60,
            to_unix: spec.to_unix - 60,
            params: Some(AlgorithmParams::Spike(params)),
            last_setup_unix: None,
            warmup_unix: None,
            cancel: None,
        })
        .await
        .unwrap();
        let reference_setups: Vec<AnalyticsSetup> = replay
            .setups
            .iter()
            .map(|setup| {
                assert_eq!(setup.close_time_unix, setup.open_time_unix + 60);
                let signal_bar = data.instruments[0]
                    .detector_bars
                    .iter()
                    .find(|bar| bar.open_time_unix == setup.open_time_unix)
                    .expect("replay signal identity belongs to the frozen source");
                assert_eq!(setup.close_price, signal_bar.close);
                AnalyticsSetup {
                    exchange_code: data.instruments[0].instrument.exchange.clone(),
                    category: data.instruments[0].instrument.category.clone(),
                    symbol: data.instruments[0].instrument.symbol.clone(),
                    open_time_unix: setup.open_time_unix,
                    close_time_unix: setup.close_time_unix,
                    close_price: setup.close_price.clone(),
                }
            })
            .collect();
        let evaluated = evaluate_candidate(
            &spec,
            &data,
            &candidate,
            &StudyLimits::default(),
            &CancelToken::new(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(evaluated.summary.setup_count, replay.setups.len());
        assert_eq!(
            evaluated.matrix,
            scalar_matrix_from_setups(&spec, &data, &candidate, &reference_setups)
        );
        assert!(
            replay
                .setups
                .iter()
                .all(|setup| setup.close_time_unix >= spec.from_unix
                    && setup.close_time_unix < spec.to_unix)
        );
    }
}

#[test]
fn impossible_min_closed_returns_no_winner_not_zero_score() {
    let mut spec = spec();
    spec.min_closed_per_cell = 1000;
    let result = run_study(
        &spec,
        &input(),
        &StudyLimits::default(),
        &CancelToken::new(),
        |_| {},
    )
    .unwrap();
    assert!(result.best.is_none());
    assert_eq!(result.progress.best_score, None);
    assert_eq!(result.progress.scorable_candidates, 0);
}
