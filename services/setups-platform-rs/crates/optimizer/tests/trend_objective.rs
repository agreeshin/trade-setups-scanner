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

#[test]
fn probability_recipe_matches_closed_cohorts_and_changes_rank_without_a_visual_fit() {
    use setups_domain::forecast::{ProbabilityDay,evaluate_probability};
    let (mut spec,input,candidate)=fixture();let limits=StudyLimits::default();let cancel=CancelToken::new();
    let mean=evaluate_candidate(&spec,&input,&candidate,&limits,&cancel).unwrap().unwrap();
    spec.forecast=Some(ForecastRecipe::engineering_baseline());spec.trend_mode=TrendMode::Forecast;
    let forecast=evaluate_candidate(&spec,&input,&candidate,&limits,&cancel).unwrap().unwrap();
    assert_eq!(forecast,evaluate_candidate(&spec,&input,&candidate,&limits,&cancel).unwrap().unwrap());
    let context=ContinuousContext{observation_cutoff:spec.to_unix};
    assert_eq!(manual_forecast_observation_cutoff(&spec,&input).unwrap(),context.observation_cutoff);
    let replay=materialize_continuous(&spec,&input,&candidate,context,&limits,&cancel).unwrap().unwrap();
    let captured=analyze_continuous_with_daily(&spec,&input,&replay,&candidate,&limits,&cancel,
        Some(ContinuousCaptureCell{tp_pct:8.0,sl_pct:4.0})).unwrap().unwrap();
    assert_eq!(captured.evaluation.evaluation,forecast);
    let daily:Vec<_>=captured.selected_daily.unwrap().points.into_iter().map(|p|ProbabilityDay{
        day_start_unix:p.day_start_unix,tp_count:p.tp_count,sl_count:p.sl_count}).collect();
    let canonical=evaluate_probability(spec.forecast.unwrap(),2.0,context.observation_cutoff,&daily).unwrap();
    assert_eq!(forecast.matrix[1].forecast_risk_expectancy,canonical.as_ref().map(|p|p.forecast_r));
    for (new,old) in forecast.matrix.iter().zip(&mean.matrix) {
        assert_eq!(new.mean_risk_expectancy,old.risk_expectancy);
        assert_eq!(new.risk_expectancy,new.forecast_risk_expectancy);
    }
    let old_pair=select_robust_cell(&mean.matrix,&spec.tp_levels,&spec.sl_levels,1).unwrap().unwrap();
    let new_pair=select_robust_cell_for_mode(&forecast.matrix,&spec.tp_levels,&spec.sl_levels,1,TrendMode::Forecast).unwrap().unwrap();
    assert_eq!(old_pair.tp_pct,8.0);assert_eq!(new_pair.tp_pct,2.0);assert!(new_pair.blend_r.is_none());
    let recommendation=recommend_continuous(&spec,&candidate,&captured.evaluation,&replay.provenance,context,100.0).unwrap().unwrap();
    assert_eq!(recommendation.forecast,spec.forecast);assert!(recommendation.search_forecast.unwrap().blend.is_none());
    let best=run_study(&spec,&input,&limits,&cancel,|_|{}).unwrap().best.unwrap();
    assert_eq!(best.forecast,spec.forecast);assert_eq!(best.observation_cutoff,Some(context.observation_cutoff));
    let json=serde_json::to_value(&best).unwrap();assert_eq!(json["forecast"]["method"],"decayed_probability");
    assert_eq!(json["observationCutoff"],context.observation_cutoff);
}

#[test]
fn explicit_blend_keeps_pure_forecast_distinct_and_preserves_strict_forward_gate() {
    let (mut spec,input,candidate)=fixture();spec.forecast=Some(ForecastRecipe::engineering_baseline());
    let limits=StudyLimits::default();let cancel=CancelToken::new();let context=ContinuousContext{observation_cutoff:spec.to_unix};
    spec.trend_mode=TrendMode::Forecast;
    let pure=evaluate_continuous(&spec,&input,&candidate,context,&limits,&cancel).unwrap().unwrap();
    spec.trend_mode=TrendMode::Blend;
    let mixed=evaluate_continuous(&spec,&input,&candidate,context,&limits,&cancel).unwrap().unwrap();
    for (b,f) in mixed.evaluation.matrix.iter().zip(&pure.evaluation.matrix) {
        assert_eq!(b.forecast_risk_expectancy,f.risk_expectancy);
        assert_eq!(b.risk_expectancy,Some(spec.trend_blend_lambda.combine(b.mean_risk_expectancy.unwrap(),f.risk_expectancy.unwrap())));
    }
    let provenance=Provenance{optimizer_version:CONTINUOUS_OPTIMIZER_VERSION.into(),seed:spec.seed,
        snapshot_hash:input.snapshot_hash.clone(),captured_at:input.captured_at.clone()};
    let accepted=recommend_continuous(&spec,&candidate,&mixed,&provenance,context,100.0).unwrap().unwrap();
    assert!(accepted.search_forecast.unwrap().blend.is_some());
    spec.min_forecast_expectancy=mixed.evaluation.summary.score.unwrap();
    assert!(recommend_continuous(&spec,&candidate,&mixed,&provenance,context,-100.0).unwrap().is_none());
    assert!(run_study(&spec,&input,&limits,&cancel,|_|{}).unwrap().best.is_none());
}

#[test]
fn matrix_forward_gate_uses_arithmetic_mean_not_sum_in_manual_and_continuous() {
    let (mut spec,input,candidate)=fixture();
    spec.forecast=Some(ForecastRecipe::engineering_baseline());
    spec.max_trials=1; // Only the fixed baseline: no other candidate can clear the gate.
    let limits=StudyLimits::default();let cancel=CancelToken::new();
    let context=ContinuousContext{observation_cutoff:spec.to_unix};
    let provenance=Provenance{optimizer_version:CONTINUOUS_OPTIMIZER_VERSION.into(),seed:spec.seed,
        snapshot_hash:input.snapshot_hash.clone(),captured_at:input.captured_at.clone()};
    for mode in [TrendMode::Forecast,TrendMode::Blend] {
        spec.trend_mode=mode;spec.min_forecast_expectancy=0.0;
        let evaluated=evaluate_continuous(&spec,&input,&candidate,context,&limits,&cancel).unwrap().unwrap();
        let matrix=&evaluated.evaluation.matrix;
        assert_eq!(matrix.len(),spec.tp_levels.len()*spec.sl_levels.len());
        assert!(matrix.len()>1);
        let sum: f64=matrix.iter().map(|cell|cell.risk_expectancy.unwrap()).sum();
        let average=sum/matrix.len() as f64;
        let forecast_average=matrix.iter().map(|cell|cell.forecast_risk_expectancy.unwrap()).sum::<f64>()/matrix.len() as f64;
        assert!(average>0.0 && sum>average,"fixture must distinguish sum from mean");
        assert_eq!(evaluated.evaluation.summary.score,Some(average));
        // Repeating the same cell population cannot multiply the objective by K.
        for repetitions in [1,2,4] {
            let repeated: Vec<_>=(0..repetitions).flat_map(|_|matrix.iter().cloned()).collect();
            assert!((score_matrix(&repeated,spec.min_closed_per_cell).unwrap()-average).abs()<1e-12);
        }
        for (threshold,accepted) in [(average/2.0,true),(average,false),((average+sum)/2.0,false)] {
            spec.min_forecast_expectancy=threshold;
            let recommendation=recommend_continuous(&spec,&candidate,&evaluated,&provenance,context,99.0).unwrap();
            let best=run_study(&spec,&input,&limits,&cancel,|_|{}).unwrap().best;
            assert_eq!(recommendation.is_some(),accepted,"continuous {mode:?}, threshold={threshold}, mean={average}, sum={sum}");
            assert_eq!(best.is_some(),accepted,"manual {mode:?}, threshold={threshold}, mean={average}, sum={sum}");
            if let Some(best)=best {assert_eq!(best.score,average);}
            if let Some(recommendation)=recommendation {
                let forecast=recommendation.search_forecast.unwrap();
                assert_eq!(forecast.matrix_r,forecast_average,"pure F remains separate from blend");
                match mode {
                    TrendMode::Forecast=>assert!(forecast.blend.is_none()),
                    TrendMode::Blend=>assert_eq!(forecast.blend.unwrap().matrix_r,average),
                    TrendMode::Mean=>unreachable!(),
                }
            }
        }
        spec.min_forecast_expectancy=0.0;
        for invalid in [None,Some(f64::NAN),Some(f64::INFINITY)] {
            let mut incomplete=evaluated.clone();
            incomplete.evaluation.matrix[0].risk_expectancy=invalid;
            assert!(recommend_continuous(&spec,&candidate,&incomplete,&provenance,context,99.0).unwrap().is_none(),
                "an unavailable selected cell must not disappear from the denominator");
        }
    }
}

#[test]
fn probability_one_day_is_available_but_missing_cells_frequency_and_real_minimum_still_reject() {
    let (mut spec,input,candidate)=fixture();let limits=StudyLimits::default();let cancel=CancelToken::new();
    spec.from_unix=30*86400;spec.trend_mode=TrendMode::Forecast;
    let legacy=evaluate_candidate(&spec,&input,&candidate,&limits,&cancel).unwrap().unwrap();
    assert!(legacy.summary.score.is_none());
    spec.forecast=Some(ForecastRecipe::engineering_baseline());
    let new=evaluate_candidate(&spec,&input,&candidate,&limits,&cancel).unwrap().unwrap();
    assert!(new.summary.score.is_some());assert!(new.matrix.iter().all(|c|c.closed_count==1 && c.forecast_risk_expectancy.is_some()));
    spec.min_closed_per_cell=2;
    assert!(evaluate_candidate(&spec,&input,&candidate,&limits,&cancel).unwrap().unwrap().summary.score.is_none());
    spec.min_closed_per_cell=1;spec.min_setups_per_day=1.001;
    let limited=evaluate_candidate(&spec,&input,&candidate,&limits,&cancel).unwrap().unwrap();
    assert_eq!(limited.summary.selected_setup_count,1);assert!(limited.summary.score.is_none());
    spec.min_setups_per_day=0.0;spec.tp_levels=vec![2.0,100.0];spec.sl_levels=vec![4.0,100.0];
    let incomplete=evaluate_candidate(&spec,&input,&candidate,&limits,&cancel).unwrap().unwrap();
    assert_eq!(incomplete.matrix.len(),4);assert!(incomplete.matrix.iter().any(|c|c.closed_count==0 && c.forecast_risk_expectancy.is_none()));
    assert!(incomplete.summary.score.is_none());
    assert!(run_study(&spec,&input,&limits,&cancel,|_|{}).unwrap().best.is_none(),"manual search cannot discard an unavailable selected cell");
}

#[test]
fn explicit_mean_persists_recipe_without_computing_forward_metric_or_changing_default_science() {
    let (mut spec,input,candidate)=fixture();let limits=StudyLimits::default();let cancel=CancelToken::new();
    let legacy=evaluate_candidate(&spec,&input,&candidate,&limits,&cancel).unwrap().unwrap();
    let before=serde_json::to_vec(&spec).unwrap();assert!(serde_json::to_value(&spec).unwrap().get("forecast").is_none());
    assert_eq!(before,serde_json::to_vec(&serde_json::from_slice::<StudyRequest>(&before).unwrap()).unwrap());
    spec.forecast=Some(ForecastRecipe::engineering_baseline());
    let same=evaluate_candidate(&spec,&input,&candidate,&limits,&cancel).unwrap().unwrap();assert_eq!(same,legacy);
    assert!(same.matrix.iter().all(|c|c.forecast_risk_expectancy.is_none()));
    let best=run_study(&spec,&input,&limits,&cancel,|_|{}).unwrap().best.unwrap();
    assert_eq!(best.trend_mode,TrendMode::Mean);assert_eq!(best.forecast,spec.forecast);assert!(best.observation_cutoff.is_some());
    let mut invalid=spec.clone();invalid.analytics_search.insert("halfLifeDays".into(),vec![json!(7)]);
    assert!(validate_request(&invalid,&default_parameter_descriptors(&spec.algorithm_id).unwrap()).is_err());
}
