// Shared raw-data golden for unit projection and PostgreSQL HTTP parity tests.
// Expected buckets/weights/decision come from the real optimizer, not a copy
// of the display SQL or projection's merging arithmetic.
use serde_json::{Value, json};
use setups_analytics::OhlcCandle;
use setups_domain::{AlgorithmParams, SpikeParams, SpikeSetup, expectancy::*, forecast::ForecastRecipe, trend::*};
use setups_optimizer::*;

pub struct Golden {
    pub spec: StudyRequest,
    pub original: ExpectancyArtifact,
    pub actual_daily: Vec<UtcDailyInput>,
    pub actual_rows: Vec<Value>,
    pub one_shot: ExpectancyArtifact,
    pub objective: ExpectancyObjective,
    pub canonical_decision: f64,
    pub split: i64,
    pub cutoff: i64,
}

pub fn golden(mode: TrendMode, recipe: Option<ForecastRecipe>) -> Golden {
    let origin = 1790812800; // UTC midnight; never depends on the wall clock.
    let split = origin + 17 * 86400 + 12 * 3600;
    let cutoff = origin + 36 * 86400;
    let bend = match recipe { Some(ForecastRecipe::Trend { bend_days, .. }) => bend_days, _ => TrendBendDays::Linear };
    let spec: StudyRequest = serde_json::from_value(json!({
        "algorithmId":"price_volume_spike","algorithmVersion":"1.1.1","algorithmParams":SpikeParams::default(),
        "analyticsParams":{"clusterEnabled":false,"clusterGapMinutes":480,"timeShiftMinutes":1,"priceChangePct":1.0},
        "instruments":[{"exchange":"fixture","category":"linear","symbol":"FIX"}],
        "fromUnix":origin,"toUnix":cutoff,"direction":"short","seed":7,"tpLevels":[10.0],"slLevels":[5.0],
        "horizonMinutes":3,"minClosedPerCell":1,"trendMode":mode,"trendBendDays":bend,"trendBlendLambda":0.3,"forecast":recipe
    })).unwrap();
    let objective = ExpectancyObjective { version: 1, trend_mode: mode, trend_bend_days: bend,
        trend_blend_lambda: spec.trend_blend_lambda, forecast: recipe };
    let mut rows = Vec::new();
    let mut candles = Vec::new();
    for day in 0..36 {
        if [1, 4, 19, 30].contains(&day) { continue; } // truly absent days
        let statuses: &[&str] = if day == 9 || day == 27 { &["unclosed", "no_data"] }
            else if day == 11 || day == 23 { &["gate_failed"] }
            else if day % 3 == 0 { &["tp", "sl", "no_data", "gate_failed", "gate_failed"] }
            else if day % 3 == 1 { &["sl", "unclosed", "gate_failed"] }
            else { &["tp", "tp", "gate_failed", "no_data"] };
        for (index, status) in statuses.iter().enumerate() {
            // Every ordinary day straddles noon; split day has both cohorts.
            let signal = origin + day * 86400 + 10 * 3600 + index as i64 * 2 * 3600;
            rows.push(json!({"setupTimeUnix":signal,"entryTimeUnix":signal,"status":status,
                "riskReturn":match *status { "tp"=>Some(2.0),"sl"=>Some(-1.0),_=>None }}));
            if *status == "no_data" { continue; }
            let entry = if *status == "gate_failed" { 100.0 } else { 102.0 };
            for minute in 0..4 {
                candles.push(OhlcCandle { open_time_unix: signal + minute * 60, open: entry, close: entry,
                    high: if minute == 1 && *status == "sl" { 110.0 } else { entry },
                    low: if minute == 1 && *status == "tp" { 90.0 } else { entry } });
            }
        }
    }
    let capture = |from: i64, to: i64| {
        let mut part = spec.clone(); part.from_unix = from; part.to_unix = to;
        let selected: Vec<_> = rows.iter().filter(|r| (from..to).contains(&r["setupTimeUnix"].as_i64().unwrap())).collect();
        let input = StudyInput { instruments: vec![InstrumentInput { instrument: spec.instruments[0].clone(), detector_bars: vec![],
            candles: candles.iter().copied().filter(|c| c.open_time_unix >= from && c.open_time_unix + 60 <= to).collect() }],
            snapshot_hash: "golden-frozen".into(), captured_at: "2026-11-01T00:00:00Z".into() };
        let replay = ContinuousReplay { from_unix: from, to_unix: to, context: ContinuousContext { observation_cutoff: to },
            params: part.algorithm_params.clone(), provenance: Provenance { optimizer_version: CONTINUOUS_OPTIMIZER_VERSION.into(),
                seed: spec.seed, snapshot_hash: input.snapshot_hash.clone(), captured_at: input.captured_at.clone() },
            instruments: vec![ContinuousInstrumentReplay { instrument: spec.instruments[0].clone(),
                setups: selected.iter().map(|r| { let at = r["setupTimeUnix"].as_i64().unwrap(); SpikeSetup {
                    algorithm_id: spec.algorithm_id.clone(), algorithm_version: spec.algorithm_version.clone(),
                    open_time_unix: at - 60, close_time_unix: at, close_price: "100".into(),
                    params: AlgorithmParams::Spike(SpikeParams::default()), details: None } }).collect(),
                previous_raw_close_unix: None, last_raw_close_unix: selected.last().map(|r| r["setupTimeUnix"].as_i64().unwrap()),
                evaluated_minutes: 0, warmup_minutes: 0 }] };
        let captured = analyze_continuous_with_daily(&part, &input, &replay,
            &StudyCandidate { params: part.algorithm_params.clone(), analytics: part.analytics_params.clone() },
            &StudyLimits::default(), &CancelToken::new(), Some(ContinuousCaptureCell { tp_pct: 10.0, sl_pct: 5.0 })).unwrap().unwrap();
        let cell = &captured.evaluation.evaluation.matrix[0];
        for (status, count) in [("tp",cell.tp_count),("sl",cell.sl_count),("gate_failed",cell.price_change_filtered_count),
            ("unclosed",cell.unclosed_count),("no_data",cell.no_data_count)] {
            assert_eq!(selected.iter().filter(|r|r["status"]==status).count(), count, "canonical classifier {status}");
        }
        let daily = captured.selected_daily.unwrap().points;
        let artifact = build_expectancy_artifact(ExpectancyInput { source: ExpectancySource {
            study_id: "golden".into(), derived_snapshot_id: "golden".into(), generation_id: None, profile_id: None,
            provenance: RecommendationProvenance { algorithm_id: spec.algorithm_id.clone(), algorithm_version: spec.algorithm_version.clone(),
                optimizer_version: CONTINUOUS_OPTIMIZER_VERSION.into(), seed: spec.seed, snapshot_hash: "golden-frozen".into(),
                captured_at: input.captured_at, from_unix: from, to_unix: to, observation_cutoff: to },
            direction: RecommendationDirection::Short, tp_pct: 10.0, sl_pct: 5.0, forecast: recipe,
            analytics: ExpectancyAnalytics { cluster_enabled: false, cluster_gap_minutes: None, time_shift_minutes: 1,
                price_change_pct: Some(1.0), horizon_minutes: 3 } },
            selected_cell: Some(SelectedCellInput { raw_r: Some((2.0*cell.tp_count as f64-cell.sl_count as f64)/cell.closed_count as f64),
                tp_count: cell.tp_count as u64, sl_count: cell.sl_count as u64 }), matrix_mean_r: None,
            daily: MetricAvailability::Available { value: daily.clone() } }).unwrap();
        (artifact, daily, cell.risk_expectancy.unwrap())
    };
    let (original, _, _) = capture(origin, split);
    let (_, actual_daily, _) = capture(split, cutoff);
    let (one_shot, _, canonical_decision) = capture(origin, cutoff);
    Golden { spec, original, actual_daily, actual_rows: rows.into_iter().filter(|r|r["setupTimeUnix"].as_i64().unwrap()>=split).collect(),
        one_shot, objective, canonical_decision, split, cutoff }
}
