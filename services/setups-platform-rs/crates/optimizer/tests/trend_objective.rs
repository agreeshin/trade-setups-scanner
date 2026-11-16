use serde_json::json;
use setups_analytics::{OhlcCandle, TradeDirection};
use setups_domain::{DetectorBar, SpikeParams};
use setups_optimizer::*;

fn fixture() -> (StudyRequest, StudyInput, StudyCandidate) {
    let params = SpikeParams { baseline_long_days: 1, baseline_short_days: 1,
        window_minutes: 1, cooldown_minutes: 1, price_growth_pct: "10".into(), ..SpikeParams::default() };
    let spec: StudyRequest = serde_json::from_value(json!({
        "algorithmId":"price_volume_spike", "algorithmVersion":"1.1.1", "algorithmParams":params,
        "analyticsParams":{"clusterEnabled":false,"clusterGapMinutes":480,"timeShiftMinutes":0,"priceChangePct":null},
        "instruments":[{"exchange":"bybit","category":"linear","symbol":"TRENDUSDT"}],
        "fromUnix":86400,"toUnix":31*86400,"direction":"short","seed":42,
        "tpLevels":[2,8],"slLevels":[4],"horizonMinutes":3,"minSetupsPerDay":0,"maxTrials":8,"maxSeconds":120
    })).unwrap();
    let mut bars = Vec::new();
    let mut candles = Vec::new();
    for minute in 0..31 * 1440 {
        let day = minute / 1440;
        let clock = minute % 1440;
        let plateau = (20..=24).contains(&clock);
        let close = if plateau { 113.0 } else { 100.0 };
        let (high, low) = match (day, clock) {
            (1..=20, 21) => (113.1, 100.0),
            (21..=30, 21) => (113.1, 109.0),
            (21..=30, 22) => (119.0, 112.9),
            _ => (close, close),
        };
        bars.push(DetectorBar { open_time_unix: minute * 60, close: close.to_string(), quote_turnover: if clock==20 { "10000" } else { "10" }.into() });
        candles.push(OhlcCandle { open_time_unix: minute * 60, open: close, close, high, low });
    }
    let input = StudyInput { instruments: vec![InstrumentInput { instrument: spec.instruments[0].clone(), detector_bars: bars, candles }],
        snapshot_hash: "n6-declining-series".into(), captured_at: "2026-11-16T00:00:00Z".into() };
    let candidate = StudyCandidate { params: spec.algorithm_params.clone(), analytics: spec.analytics_params.clone() };
    (spec, input, candidate)
}

#[test]
fn manual_and_continuous_rank_declining_cells_by_the_chosen_metric_deterministically() {
    let (mut spec, input, candidate) = fixture();
    let limits = StudyLimits::default();
    let cancel = CancelToken::new();
    let mean = evaluate_candidate(&spec, &input, &candidate, &limits, &cancel).unwrap().unwrap();
    assert_eq!(mean.summary.selected_setup_count, 30);
    let selected = select_robust_cell(&mean.matrix, &spec.tp_levels, &spec.sl_levels, 1).unwrap().unwrap();
    assert_eq!(selected.tp_pct, 8.0);
    assert!(mean.matrix.iter().all(|cell| cell.mean_risk_expectancy.is_none()));
    spec.trend_mode = TrendMode::Forecast;
    spec.trend_bend_days = TrendBendDays::Seven;
    let forecast = evaluate_candidate(&spec, &input, &candidate, &limits, &cancel).unwrap().unwrap();
    assert_eq!(forecast, evaluate_candidate(&spec, &input, &candidate, &limits, &cancel).unwrap().unwrap());
    let selected = select_robust_cell(&forecast.matrix, &spec.tp_levels, &spec.sl_levels, 1).unwrap().unwrap();
    assert_eq!(selected.tp_pct, 2.0);
    assert_eq!(forecast.matrix[1].mean_risk_expectancy, mean.matrix[1].risk_expectancy);
    assert!(forecast.matrix[1].risk_expectancy.unwrap() < 0.0);
    assert_ne!(forecast.summary.score, mean.summary.score);

    let context = ContinuousContext { observation_cutoff: spec.to_unix };
    let replay = materialize_continuous(&spec, &input, &candidate, context, &limits, &cancel).unwrap().unwrap();
    let captured = analyze_continuous_with_daily(&spec, &input, &replay, &candidate, &limits, &cancel,
        Some(ContinuousCaptureCell { tp_pct: 8.0, sl_pct: 4.0 })).unwrap().unwrap();
    assert_eq!(captured.evaluation.evaluation, forecast);
    let daily = captured.selected_daily.unwrap();
    let analytics = setups_analytics::manual_trend::fit_daily_outcomes(daily.points.iter().map(|point| (point.day_start_unix, point.mean_r)), spec.trend_bend_days).unwrap().unwrap();
    assert_eq!(analytics.endpoint().value.to_bits(), forecast.matrix[1].risk_expectancy.unwrap().to_bits());
    let mut first = run_study(&spec, &input, &limits, &cancel, |_| {}).unwrap();
    let mut second = run_study(&spec, &input, &limits, &cancel, |_| {}).unwrap();
    first.progress.elapsed_seconds=0.0; second.progress.elapsed_seconds=0.0;
    assert_eq!(first, second);
    assert!(first.best.is_none(), "negative forecast does not pass the default forward gate");
}

#[test]
fn blend_ranking_is_deterministic_and_forward_gate_is_configurable() {
    let (mut spec,input,candidate)=fixture();let limits=StudyLimits::default();let cancel=CancelToken::new();
    spec.trend_mode=TrendMode::Forecast;spec.trend_bend_days=TrendBendDays::Seven;
    let forecast=evaluate_candidate(&spec,&input,&candidate,&limits,&cancel).unwrap().unwrap();
    spec.trend_mode=TrendMode::Blend;
    let blend=evaluate_candidate(&spec,&input,&candidate,&limits,&cancel).unwrap().unwrap();
    assert_eq!(blend,evaluate_candidate(&spec,&input,&candidate,&limits,&cancel).unwrap().unwrap());
    for (mixed,pure) in blend.matrix.iter().zip(&forecast.matrix) {
        assert_eq!(mixed.forecast_risk_expectancy,pure.risk_expectancy);
        let mean=mixed.mean_risk_expectancy.unwrap();let pure=pure.risk_expectancy.unwrap();
        assert_eq!(mixed.risk_expectancy,Some(spec.trend_blend_lambda.combine(mean,pure)));
    }
    spec.min_forecast_expectancy=100.0;
    assert!(run_study(&spec,&input,&limits,&cancel,|_|{}).unwrap().best.is_none());
    let mut invalid=spec.clone(); invalid.min_forecast_expectancy = -0.1;
    assert!(evaluate_candidate(&invalid,&input,&candidate,&limits,&cancel).is_err());
    for mode in [TrendMode::Mean,TrendMode::Forecast] {
        spec.trend_mode=mode;spec.min_forecast_expectancy=0.0;
        let before=serde_json::to_vec(&spec).unwrap();
        assert!(!String::from_utf8(before.clone()).unwrap().contains("trendBlendLambda"));
        assert_eq!(before,serde_json::to_vec(&serde_json::from_slice::<StudyRequest>(&before).unwrap()).unwrap());
    }
}

#[test]
#[ignore = "manual paired wall-time benchmark; not a timing-sensitive CI gate"]
fn benchmark_blend_vs_forecast_same_seed_trials() {
    use std::time::Instant;
    let (mut spec,input,_)=fixture();spec.max_trials=8;spec.max_seconds=120;
    spec.algorithm_search=serde_json::from_value(json!({"priceGrowthPct":["8","9","10","11","12","13","14","15"]})).unwrap();
    spec.trend_bend_days=TrendBendDays::Seven;
    let limits=StudyLimits {cpu_threads:1,..StudyLimits::default()};let cancel=CancelToken::new();
    let mut forecast=Vec::new();let mut blend=Vec::new();let mut trials=None;
    for pair in 0..5 {
        for mode in if pair%2==0 {[TrendMode::Forecast,TrendMode::Blend]}else{[TrendMode::Blend,TrendMode::Forecast]} {
            spec.trend_mode=mode;let started=Instant::now();
            let result=run_study(&spec,&input,&limits,&cancel,|_|{}).unwrap();
            let elapsed=started.elapsed().as_secs_f64();
            assert_eq!(*trials.get_or_insert(result.progress.evaluated_candidates),result.progress.evaluated_candidates);
            if mode==TrendMode::Blend {blend.push(elapsed);}else{forecast.push(elapsed);}
        }
    }
    forecast.sort_by(f64::total_cmp);blend.sort_by(f64::total_cmp);
    let ratio=blend[2]/forecast[2];
    println!("N7 paired benchmark: seed={} trials/run={} pairs=5 forecast_median_s={:.6} blend_median_s={:.6} overhead_pct={:.2} forecast_samples={forecast:?} blend_samples={blend:?}",spec.seed,trials.unwrap(),forecast[2],blend[2],(ratio-1.0)*100.0);
    assert!(ratio<=1.20,"blend exceeds the owner's +20% acceptance bound");
}

#[test]
fn legacy_settings_roundtrip_byte_identically_and_mean_does_not_bucket_days() {
    let (mut spec, input, candidate) = fixture();
    let legacy_bytes = serde_json::to_vec(&spec).unwrap();
    assert!(!String::from_utf8(legacy_bytes.clone()).unwrap().contains("trend"));
    let old: StudyRequest = serde_json::from_slice(&legacy_bytes).unwrap();
    assert_eq!(old.trend_mode, TrendMode::Mean);
    assert_eq!(serde_json::to_vec(&old).unwrap(), legacy_bytes);
    let limits = StudyLimits { max_working_bytes: 512 * 1024, ..StudyLimits::default() };
    let mean = evaluate_candidate(&old, &input, &candidate, &limits, &CancelToken::new()).unwrap().unwrap();
    spec.trend_bend_days = TrendBendDays::Seven; // Ignored in mean mode.
    assert_eq!(evaluate_candidate(&spec, &input, &candidate, &limits, &CancelToken::new()).unwrap().unwrap(), mean);
    spec.trend_mode = TrendMode::Forecast;
    assert!(matches!(evaluate_candidate(&spec, &input, &candidate, &limits, &CancelToken::new()), Err(StudyError::ResourceLimit { .. })));
    assert_eq!(spec.direction, TradeDirection::Short);
}
