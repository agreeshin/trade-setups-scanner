use serde_json::json;
use setups_analytics::{OhlcCandle, TradeDirection};
use setups_domain::{DetectorBar, SpikeParams};
use setups_optimizer::*;
use std::{collections::BTreeMap, time::Duration};

const T0: i64 = 1_699_999_980;
fn fixture() -> (StudyRequest, StudyInput, ContinuousContext) {
    let instrument = StudyInstrument {
        exchange: "bybit".into(),
        category: "linear".into(),
        symbol: "RESUMEUSDT".into(),
    };
    let spec=StudyRequest{algorithm_id:"price_volume_spike".into(),algorithm_version:"1.1.1".into(),
        algorithm_params:serde_json::from_value(json!(SpikeParams{baseline_long_days:1,baseline_short_days:1,..SpikeParams::default()})).unwrap(),
        algorithm_search:serde_json::from_value(json!({"priceGrowthPct":["8","12","16","20"],"volumeMultiplier":["1.5","3","5"]})).unwrap(),
        analytics_params:AnalyticsParams{cluster_enabled:false,..AnalyticsParams::default()},
        analytics_search:serde_json::from_value(json!({"clusterEnabled":[false,true],"timeShiftMinutes":[0,1,3],"priceChangePct":[null,0.10000000000000002,-0.0]})).unwrap(),
        instruments:vec![instrument.clone()],from_unix:T0+1450*60,to_unix:T0+1530*60,direction:TradeDirection::Short,setup_maturity:0.0,
        tp_levels:vec![4.0,8.0],sl_levels:vec![4.0,8.0],horizon_minutes:30,min_closed_per_cell:1,min_setups_per_day:0.0,
        max_trials:48,max_seconds:120,seed:168,trend_mode:TrendMode::Mean,trend_bend_days:TrendBendDays::Linear,
        trend_blend_lambda:TrendBlendLambda::default(),min_forecast_expectancy:0.0,forecast:None};
    let mut bars = Vec::new();
    let mut candles = Vec::new();
    for minute in 0..1530 {
        let value = if minute < 1435 {
            100.0
        } else {
            100.0 * 1.13f64.powi((minute - 1434) / 5)
        };
        let close = format!("{value:.8}");
        let value = close.parse::<f64>().unwrap();
        bars.push(DetectorBar {
            open_time_unix: T0 + minute as i64 * 60,
            close,
            quote_turnover: if minute < 1435 { "10" } else { "3000" }.into(),
        });
        candles.push(OhlcCandle {
            open_time_unix: T0 + minute as i64 * 60,
            open: value,
            high: value * 1.02,
            low: value * 0.98,
            close: value,
        });
    }
    let input = StudyInput {
        instruments: vec![InstrumentInput {
            instrument,
            detector_bars: bars,
            candles,
        }],
        snapshot_hash: "original-wire-hash-not-derived-from-reduced-input".into(),
        captured_at: "2026-11-14T12:00:00Z".into(),
    };
    let context = ContinuousContext {
        observation_cutoff: spec.to_unix,
    };
    (spec, input, context)
}
fn finished(exit: ContinuousSearchExit) -> StudyOutcome {
    match exit {
        ContinuousSearchExit::Completed(outcome) => outcome,
        _ => panic!("unexpected suspension"),
    }
}
fn comparable(mut outcome: StudyOutcome) -> StudyOutcome {
    outcome.progress.elapsed_seconds = 0.0;
    outcome
}

fn assert_legacy_checkpoint_has_no_new_metric_fields(bytes: &[u8]) {
    fn check(value: &ciborium::value::Value) {
        use ciborium::value::Value;
        match value {
            Value::Map(entries) => for (key,value) in entries {
                if let Value::Text(key)=key {
                    assert!(!["trendMode","trendBendDays","meanRiskExpectancy","meanScore","searchForecast","forecastR","forecastMatrixR","trendBlendLambda","minForecastExpectancy","forecastRiskExpectancy","forecast","observationCutoff"].contains(&key.as_str()));
                }
                check(value);
            },
            Value::Array(values) => for value in values { check(value); },
            _ => {},
        }
    }
    let value:ciborium::value::Value=ciborium::de::from_reader(bytes).unwrap();
    check(&value);
}
fn control<'a>(
    cancel: &'a CancelToken,
    pause: &'a CancelToken,
    resume: Option<&'a [u8]>,
) -> ContinuousSearchControl<'a> {
    ContinuousSearchControl {
        cancel,
        pause,
        resume_checkpoint: resume,
        remaining_budget: Duration::from_secs(120),
        max_checkpoint_bytes: 4 * 1024 * 1024,
    }
}

fn setup_cap_fixture(baseline_cooldown: u32) -> (StudyRequest, StudyInput, ContinuousContext, StudyLimits) {
    let (mut spec, input, context) = fixture();
    spec.algorithm_params.insert("cooldownMinutes".into(), json!(baseline_cooldown));
    spec.algorithm_search = BTreeMap::from([("cooldownMinutes".into(), vec![json!(5), json!(10), json!(20)])]);
    spec.analytics_search = BTreeMap::from([("timeShiftMinutes".into(), vec![json!(0), json!(1)])]);
    spec.max_trials = 8;
    let cancel = CancelToken::new();
    let mut params = spec.algorithm_params.clone();
    params.insert("cooldownMinutes".into(), json!(10));
    let exact_cap = materialize_continuous(
        &spec, &input, &StudyCandidate { params, analytics: spec.analytics_params.clone() },
        context, &StudyLimits::default(), &cancel,
    ).unwrap().unwrap().instruments[0].setups.len();
    assert!(exact_cap > 1);
    let limits = StudyLimits { cpu_threads: 1, max_setups_per_candidate: exact_cap, ..StudyLimits::default() };
    (spec, input, context, limits)
}

fn stable_checkpoint(bytes: &[u8]) -> ciborium::Value {
    assert_legacy_checkpoint_has_no_new_metric_fields(bytes);
    let mut value = ciborium::de::from_reader(bytes).unwrap();
    replace_path(&mut value, &["elapsed_nanos"], ciborium::Value::Integer(0.into()));
    replace_path(&mut value, &["progress", "elapsedSeconds"], ciborium::Value::Float(0.0));
    value
}

#[test]
fn oversized_continuous_proposals_are_counted_once_and_resume_deterministically() {
    // Baseline may be infeasible in-domain, feasible, or infeasible outside the domain.
    for baseline_cooldown in [5, 10, 7] {
        for batch_size in [1, 4] {
            let (spec, input, context, mut limits) = setup_cap_fixture(baseline_cooldown);
            limits.batch_size = batch_size;
            let cancel = CancelToken::new();
            let pause = CancelToken::new();
            let mut overflowing = spec.algorithm_params.clone();
            overflowing.insert("cooldownMinutes".into(), json!(5));
            let oversized = StudyCandidate { params: overflowing, analytics: spec.analytics_params.clone() };
            // Actual detector/materialization guard, not an injected limit() error.
            assert!(matches!(materialize_continuous(&spec, &input, &oversized, context, &limits, &cancel),
                Err(StudyError::ResourceLimit { resource, requested, limit })
                    if resource == "setups_per_candidate" && limit == limits.max_setups_per_candidate && requested == limit + 1));
            let mut reference_states = Vec::new();
            let mut infeasible = Vec::new();
            let mut completed_counts = Vec::new();
            let reference = finished(search_continuous_resumable(
                &spec, &input, context, &limits, control(&cancel, &pause, None),
                |update| {
                    let count = update.progress.evaluated_candidates;
                    if count > completed_counts.last().copied().unwrap_or(0) { completed_counts.push(count); }
                    if let Some(event) = update.event.filter(|e| e.kind == "candidate_infeasible") {
                        assert!(event.message.contains("setups_per_candidate"));
                        assert!(event.message.contains(&format!("limit={}", limits.max_setups_per_candidate)));
                        infeasible.push((count, update.progress.scorable_candidates));
                    }
                },
                |info, bytes| { reference_states.push((info.sequence, stable_checkpoint(bytes))); Ok(CheckpointAction::Continue) },
            ).expect("one oversized proposal must not abort the continuous search"));
            let expected_trials = if baseline_cooldown == 7 { 7 } else { 6 };
            assert_eq!(reference.reason, TerminationReason::SpaceExhausted);
            assert_eq!(reference.progress.evaluated_candidates, expected_trials);
            assert_eq!(completed_counts, (1..=expected_trials).collect::<Vec<_>>());
            assert_eq!(reference.progress.scorable_candidates, 4);
            assert_eq!(infeasible.len(), expected_trials - 4, "only the over-cap detector group is infeasible");
            assert!(infeasible.windows(2).all(|w| w[0].0 < w[1].0));
            assert_eq!(reference.baseline.is_some(), baseline_cooldown == 10);
            if baseline_cooldown != 10 { assert_eq!(infeasible[0], (1, 0)); }
            let best = reference.best.as_ref().expect("later feasible proposals must still be evaluated");
            assert!(best.support.setup_count <= limits.max_setups_per_candidate);
            assert_ne!(best.params["cooldownMinutes"], json!(5));
            if baseline_cooldown != 10 { assert!(best.trial_index > 0); }
            let winner = StudyCandidate { params: best.params.clone(), analytics: AnalyticsParams {
                cluster_enabled: best.analytics.cluster_enabled,
                cluster_gap_minutes: best.analytics.cluster_gap_minutes.unwrap_or(spec.analytics_params.cluster_gap_minutes),
                time_shift_minutes: best.analytics.time_shift_minutes, price_change_pct: best.analytics.price_change_pct,
            } };
            let replay = materialize_continuous(&spec, &input, &winner, context, &limits, &cancel).unwrap().unwrap();
            let reanalyzed = analyze_continuous(&spec, &input, &replay, &winner, &limits, &cancel).unwrap().unwrap();
            assert_eq!(reanalyzed.evaluation.matrix, best.matrix);
            assert_eq!(reanalyzed.evaluation.summary, best.support);
            // Direct/final materialization is still strict after the search exception.
            assert!(matches!(materialize_continuous(&spec, &input, &oversized, context, &limits, &cancel),
                Err(StudyError::ResourceLimit { resource, .. }) if resource == "setups_per_candidate"));
            let mut checkpoint = None;
            let mut resumed_states = Vec::new();
            let recovered = loop {
                let mut committed = None;
                let exit = search_continuous_resumable(
                    &spec, &input, context, &limits, control(&cancel, &pause, checkpoint.as_deref()), |_| {},
                    |info, bytes| {
                        resumed_states.push((info.sequence, stable_checkpoint(bytes)));
                        committed = Some(bytes.to_vec());
                        Ok(if info.sequence > 0 { CheckpointAction::Suspend } else { CheckpointAction::Continue })
                    },
                ).expect("joined infeasible trials must be checkpoint-resumable without resampling");
                if let Some(bytes) = committed { checkpoint = Some(bytes); }
                assert!(resumed_states.len() <= reference_states.len());
                if let ContinuousSearchExit::Completed(outcome) = exit { break outcome; }
            };
            // Includes exact RNG/frontier/seen/records/influence/counters/incumbent, not just score.
            assert_eq!(resumed_states, reference_states);
            assert_eq!(comparable(recovered), comparable(reference));
        }
    }
}

#[test]
fn all_oversized_continuous_trials_finish_without_fake_baseline_or_winner() {
    let (mut spec, input, context, limits) = setup_cap_fixture(5);
    spec.algorithm_search.clear();
    spec.analytics_search.clear();
    spec.max_trials = 1;
    let cancel = CancelToken::new();
    let pause = CancelToken::new();
    let mut checkpoint = Vec::new();
    let exit = search_continuous_resumable(
        &spec, &input, context, &limits, control(&cancel, &pause, None), |_| {},
        |info, bytes| {
            checkpoint = bytes.to_vec();
            Ok(if info.sequence == 1 { CheckpointAction::Suspend } else { CheckpointAction::Continue })
        },
    ).expect("an infeasible baseline consumes trial0 without becoming a study error");
    let ContinuousSearchExit::Suspended(info) = exit else { panic!("expected joined trial0 checkpoint") };
    assert_eq!((info.progress.evaluated_candidates, info.progress.scorable_candidates, info.progress.best_score), (1, 0, None));
    let outcome = finished(search_continuous_resumable(
        &spec, &input, context, &limits, control(&cancel, &pause, Some(&checkpoint)), |_| {},
        |_, _| panic!("trial budget already consumed"),
    ).unwrap());
    assert_eq!(outcome.reason, TerminationReason::TrialBudget);
    assert!(outcome.baseline.is_none());
    assert!(outcome.best.is_none());
    assert_eq!((outcome.progress.evaluated_candidates, outcome.progress.scorable_candidates), (1, 0));
    let value:ciborium::Value=ciborium::de::from_reader(checkpoint.as_slice()).unwrap();
    let mut corruptions=Vec::new();
    let mut changed=value.clone();replace_path(&mut changed,&["baseline_infeasible"],ciborium::Value::Bool(false));corruptions.push(changed);
    let mut changed=value.clone();
    let ciborium::Value::Map(fields)=&mut changed else {panic!("checkpoint map")};
    fields.retain(|(key,_)|key.as_text()!=Some("baseline_infeasible"));corruptions.push(changed);
    let mut changed=value.clone();replace_path(&mut changed,&["search","records"],ciborium::Value::Array(vec![]));corruptions.push(changed);
    let mut changed=value.clone();
    let ciborium::Value::Map(fields)=&mut changed else {panic!("checkpoint map")};
    let (_,ciborium::Value::Map(search))=fields.iter_mut().find(|(key,_)|key.as_text()==Some("search")).unwrap() else {panic!("search map")};
    let (_,ciborium::Value::Array(records))=search.iter_mut().find(|(key,_)|key.as_text()==Some("records")).unwrap() else {panic!("records")};
    replace_path(&mut records[0],&["score"],ciborium::Value::Float(0.0));corruptions.push(changed);
    for value in corruptions {
        let mut bytes=Vec::new();ciborium::ser::into_writer(&value,&mut bytes).unwrap();
        assert!(matches!(search_continuous_resumable(&spec,&input,context,&limits,control(&cancel,&pause,Some(&bytes)),|_|{},|_,_|panic!("corruption must not commit")),Err(StudyError::InvalidInput(_))));
    }
}

#[test]
fn oversized_trial_handling_keeps_other_limits_corruption_and_commit_errors_fatal() {
    let (spec, input, context, limits) = setup_cap_fixture(5);
    let cancel = CancelToken::new();
    let pause = CancelToken::new();
    for (changed, expected_resource) in [
        (StudyLimits { max_snapshot_bytes: 1, ..limits.clone() }, "snapshot_bytes"),
        (StudyLimits { max_working_bytes: 700_000, ..limits.clone() }, "continuous_working_bytes"),
    ] {
        assert!(matches!(search_continuous_resumable(&spec, &input, context, &changed,
            control(&cancel, &pause, None), |_| {}, |_, _| Ok(CheckpointAction::Continue)),
            Err(StudyError::ResourceLimit { resource, .. }) if resource == expected_resource));
    }
    let mut corrupted = input.clone();
    corrupted.instruments[0].candles[0].low = f64::NAN;
    assert!(matches!(search_continuous_resumable(&spec, &corrupted, context, &limits,
        control(&cancel, &pause, None), |_| {}, |_, _| Ok(CheckpointAction::Continue)), Err(StudyError::InvalidInput(_))));
    let injected = StudyError::ResourceLimit { resource: "setups_per_candidate".into(), limit: 1, requested: 2 };
    let mut committed_sequence = None;
    let error = search_continuous_resumable(&spec, &input, context, &limits,
        control(&cancel, &pause, None), |_| {}, |info, _| {
            if info.sequence == 1 { return Err(injected.clone()); }
            committed_sequence = Some(info.sequence);
            Ok(CheckpointAction::Continue)
        }).unwrap_err();
    assert_eq!(error, injected, "even the same resource error from a checkpoint callback is fatal");
    assert_eq!(committed_sequence, Some(0));
}

#[test]
fn missing_unscorable_baseline_without_marker_fails_closed() {
    let (mut spec,input,context)=fixture();
    spec.algorithm_params.insert("priceGrowthPct".into(),json!("1000"));
    spec.algorithm_search.clear(); spec.analytics_search.clear(); spec.max_trials=1;
    let limits=StudyLimits::default(); let cancel=CancelToken::new(); let pause=CancelToken::new();
    let mut checkpoint=Vec::new();
    search_continuous_resumable(&spec,&input,context,&limits,control(&cancel,&pause,None),|_|{},|info,bytes| {
        checkpoint=bytes.to_vec();Ok(if info.sequence==1 {CheckpointAction::Suspend}else{CheckpointAction::Continue})
    }).unwrap();
    let mut value:ciborium::Value=ciborium::de::from_reader(checkpoint.as_slice()).unwrap();
    assert!(value.as_map().unwrap().iter().all(|(key,_)|key.as_text()!=Some("baseline_infeasible")),"false marker must preserve legacy bytes");
    let legacy=finished(search_continuous_resumable(&spec,&input,context,&limits,control(&cancel,&pause,Some(&checkpoint)),|_|{},|_,_|panic!("budget consumed")).unwrap());
    assert!(legacy.baseline.as_ref().is_some_and(|summary|summary.score.is_none()));
    let mut contradictory=value.clone();
    let ciborium::Value::Map(fields)=&mut contradictory else {panic!("checkpoint map")};
    fields.push((ciborium::Value::Text("baseline_infeasible".into()),ciborium::Value::Bool(true)));
    let mut corrupt=Vec::new();ciborium::ser::into_writer(&contradictory,&mut corrupt).unwrap();
    assert!(matches!(search_continuous_resumable(&spec,&input,context,&limits,control(&cancel,&pause,Some(&corrupt)),|_|{},|_,_|panic!("contradictory marker must not commit")),Err(StudyError::InvalidInput(_))));
    replace_path(&mut value,&["baseline"],ciborium::Value::Null);
    let mut corrupt=Vec::new();ciborium::ser::into_writer(&value,&mut corrupt).unwrap();
    assert!(matches!(search_continuous_resumable(&spec,&input,context,&limits,control(&cancel,&pause,Some(&corrupt)),|_|{},|_,_|panic!("corrupt baseline must not commit")),
        Err(StudyError::InvalidInput(_))),"a missing ordinary non-scorable baseline is corruption, not inferred infeasibility");
}

#[test]
fn infeasible_marker_rejects_initial_state_and_scored_trial_zero() {
    let (mut spec,input,context)=fixture();spec.max_trials=1;
    let limits=StudyLimits::default();let cancel=CancelToken::new();let pause=CancelToken::new();
    let mut snapshots=Vec::new();
    search_continuous_resumable(&spec,&input,context,&limits,control(&cancel,&pause,None),|_|{},|_,bytes| {
        snapshots.push(bytes.to_vec());Ok(CheckpointAction::Continue)
    }).unwrap();
    assert_eq!(snapshots.len(),2);
    for (index,bytes) in snapshots.iter().enumerate() {
        let mut value:ciborium::Value=ciborium::de::from_reader(bytes.as_slice()).unwrap();
        let ciborium::Value::Map(fields)=&mut value else {panic!("checkpoint map")};
        fields.push((ciborium::Value::Text("baseline_infeasible".into()),ciborium::Value::Bool(true)));
        if index==1 {replace_path(&mut value,&["baseline"],ciborium::Value::Null);}
        let mut corrupt=Vec::new();ciborium::ser::into_writer(&value,&mut corrupt).unwrap();
        assert!(matches!(search_continuous_resumable(&spec,&input,context,&limits,control(&cancel,&pause,Some(&corrupt)),|_|{},|_,_|panic!("invalid trial0 marker must not commit")),Err(StudyError::InvalidInput(_))));
    }
}

#[test]
fn joined_restarts_match_uninterrupted_and_legacy_manual_rng_proposals() {
    let (spec, input, context) = fixture();
    let limits = StudyLimits::default();
    let cancel = CancelToken::new();
    let pause = CancelToken::new();
    let legacy = search_continuous(&spec, &input, context, &limits, &cancel, |_| {}).unwrap();
    let mut expected_boundaries = Vec::new();
    let reference = finished(
        search_continuous_resumable(
            &spec,
            &input,
            context,
            &limits,
            control(&cancel, &pause, None),
            |_| {},
            |info, _| {
                expected_boundaries.push((
                    info.sequence,
                    info.progress.evaluated_candidates,
                    info.progress.best_score.map(f64::to_bits),
                ));
                Ok(CheckpointAction::Continue)
            },
        )
        .unwrap(),
    );
    assert_eq!(comparable(legacy), comparable(reference.clone()));
    let mut checkpoint: Option<Vec<u8>> = None;
    let mut actual_boundaries = Vec::new();
    let result = loop {
        let mut committed = None;
        let result = search_continuous_resumable(
            &spec,
            &input,
            context,
            &limits,
            control(&cancel, &pause, checkpoint.as_deref()),
            |_| {},
            |info, bytes| {
                assert_legacy_checkpoint_has_no_new_metric_fields(bytes);
                actual_boundaries.push((
                    info.sequence,
                    info.progress.evaluated_candidates,
                    info.progress.best_score.map(f64::to_bits),
                ));
                committed = Some(bytes.to_vec());
                Ok(if info.sequence > 0 && info.sequence % 3 == 0 {
                    CheckpointAction::Suspend
                } else {
                    CheckpointAction::Continue
                })
            },
        )
        .unwrap();
        if let Some(bytes) = committed {
            checkpoint = Some(bytes);
        }
        if let ContinuousSearchExit::Completed(outcome) = result {
            break outcome;
        }
    };
    assert_eq!(actual_boundaries, expected_boundaries);
    assert_eq!(comparable(result), comparable(reference));
}

#[test]
fn crash_before_commit_replays_only_uncommitted_batch_and_commit_errors_fail_closed() {
    let (spec, input, context) = fixture();
    let limits = StudyLimits::default();
    let cancel = CancelToken::new();
    let pause = CancelToken::new();
    let reference = finished(
        search_continuous_resumable(
            &spec,
            &input,
            context,
            &limits,
            control(&cancel, &pause, None),
            |_| {},
            |_, _| Ok(CheckpointAction::Continue),
        )
        .unwrap(),
    );
    let mut committed = Vec::new();
    let mut observations = Vec::new();
    let error = search_continuous_resumable(
        &spec,
        &input,
        context,
        &limits,
        control(&cancel, &pause, None),
        |_| {},
        |info, bytes| {
            if info.sequence == 4 {
                return Err(StudyError::Execution("injected commit failure".into()));
            }
            observations.push(info.sequence);
            committed = bytes.to_vec();
            Ok(CheckpointAction::Continue)
        },
    )
    .unwrap_err();
    assert!(matches!(error, StudyError::Execution(_)));
    assert_eq!(observations, vec![0, 1, 2, 3]);
    let recovered = finished(
        search_continuous_resumable(
            &spec,
            &input,
            context,
            &limits,
            control(&cancel, &pause, Some(&committed)),
            |_| {},
            |info, _| {
                observations.push(info.sequence);
                Ok(CheckpointAction::Continue)
            },
        )
        .unwrap(),
    );
    assert!(observations.windows(2).all(|w| w[1] == w[0] + 1));
    assert_eq!(comparable(recovered), comparable(reference));
}

#[test]
fn blend_checkpoint_binds_lambda_and_the_forward_gate_without_new_history_migration() {
    let (mut spec,input,context)=fixture();spec.trend_mode=TrendMode::Blend;
    let limits=StudyLimits::default();let cancel=CancelToken::new();let pause=CancelToken::new();let mut checkpoint=Vec::new();
    search_continuous_resumable(&spec,&input,context,&limits,control(&cancel,&pause,None),|_|{},|_,bytes| {
        checkpoint=bytes.to_vec();Ok(CheckpointAction::Suspend)
    }).unwrap();
    assert!(!checkpoint.is_empty());
    for changed in [StudyRequest {trend_blend_lambda:TrendBlendLambda::try_from(0.6).unwrap(),..spec.clone()},
        StudyRequest {min_forecast_expectancy:0.2,..spec.clone()}] {
        assert!(search_continuous_resumable(&changed,&input,context,&limits,control(&cancel,&pause,Some(&checkpoint)),|_|{},|_,_|Ok(CheckpointAction::Continue)).is_err());
    }
    assert!(search_continuous_resumable(&spec,&input,context,&limits,control(&cancel,&pause,Some(&checkpoint)),|_|{},|_,_|Ok(CheckpointAction::Suspend)).is_ok());
}

#[test]
fn explicit_recipes_resume_exactly_and_method_settings_cutoff_changes_fail_closed() {
    let (mut spec,input,context)=fixture();spec.max_trials=8;spec.forecast=Some(ForecastRecipe::engineering_baseline());
    let limits=StudyLimits::default();let cancel=CancelToken::new();let pause=CancelToken::new();
    for mode in [TrendMode::Mean,TrendMode::Forecast,TrendMode::Blend] {
        spec.trend_mode=mode;
        let reference=finished(search_continuous_resumable(&spec,&input,context,&limits,control(&cancel,&pause,None),|_|{},|_,_|Ok(CheckpointAction::Continue)).unwrap());
        let mut bytes=Vec::new();
        let suspended=search_continuous_resumable(&spec,&input,context,&limits,control(&cancel,&pause,None),|_|{},|info,encoded| {
            bytes=encoded.to_vec();Ok(if info.sequence==2 {CheckpointAction::Suspend}else{CheckpointAction::Continue})
        }).unwrap();assert!(matches!(suspended,ContinuousSearchExit::Suspended(_)));
        let recovered=finished(search_continuous_resumable(&spec,&input,context,&limits,control(&cancel,&pause,Some(&bytes)),|_|{},|_,_|Ok(CheckpointAction::Continue)).unwrap());
        assert_eq!(comparable(recovered),comparable(reference));
        for recipe in [None,Some(ForecastRecipe::Trend{version:1,bend_days:TrendBendDays::Linear}),
            Some(serde_json::from_value(json!({"method":"decayed_probability","version":1,"halfLifeDays":7,"kappa":10,"priorPolicy":"gross_break_even"})).unwrap()),
            Some(serde_json::from_value(json!({"method":"decayed_probability","version":1,"halfLifeDays":14,"kappa":20,"priorPolicy":"gross_break_even"})).unwrap())] {
            let changed=StudyRequest{forecast:recipe,..spec.clone()};
            assert!(search_continuous_resumable(&changed,&input,context,&limits,control(&cancel,&pause,Some(&bytes)),|_|{},|_,_|panic!("incompatible recipe must not restart")).is_err());
        }
        assert!(search_continuous_resumable(&spec,&input,ContinuousContext{observation_cutoff:context.observation_cutoff-60},&limits,
            control(&cancel,&pause,Some(&bytes)),|_|{},|_,_|panic!("changed cutoff must not restart")).is_err());
    }
}

#[test]
fn expired_budget_finalizes_committed_incumbent_without_new_observations() {
    let (spec, input, context) = fixture();
    let limits = StudyLimits::default();
    let cancel = CancelToken::new();
    let pause = CancelToken::new();
    let mut bytes = Vec::new();
    let mut last = None;
    search_continuous_resumable(
        &spec,
        &input,
        context,
        &limits,
        control(&cancel, &pause, None),
        |_| {},
        |info, encoded| {
            bytes = encoded.to_vec();
            last = Some(info.clone());
            Ok(if info.sequence == 2 {
                CheckpointAction::Suspend
            } else {
                CheckpointAction::Continue
            })
        },
    )
    .unwrap();
    let mut options = control(&cancel, &pause, Some(&bytes));
    options.remaining_budget = Duration::ZERO;
    let outcome = finished(
        search_continuous_resumable(
            &spec,
            &input,
            context,
            &limits,
            options,
            |_| {},
            |_, _| panic!("expired resume must not advance checkpoint"),
        )
        .unwrap(),
    );
    assert_eq!(outcome.reason, TerminationReason::TimeBudget);
    assert_eq!(
        outcome.progress.evaluated_candidates,
        last.as_ref().unwrap().progress.evaluated_candidates
    );
    assert_eq!(
        outcome.progress.best_score,
        last.unwrap().progress.best_score
    );
    assert!(outcome.best.as_ref().is_none_or(|b| !b.partial));
}

#[test]
fn initial_checkpoint_pause_and_cancelled_partial_batch_never_commit_emitted_search_state() {
    let (spec, input, context) = fixture();
    let limits = StudyLimits::default();
    let cancel = CancelToken::new();
    let pause = CancelToken::new();
    pause.cancel();
    let mut saved = Vec::new();
    let exit = search_continuous_resumable(
        &spec,
        &input,
        context,
        &limits,
        control(&cancel, &pause, None),
        |_| {},
        |info, bytes| {
            assert_eq!(info.sequence, 0);
            assert_eq!(info.progress.evaluated_candidates, 0);
            saved = bytes.to_vec();
            Ok(CheckpointAction::Continue)
        },
    )
    .unwrap();
    assert!(matches!(exit, ContinuousSearchExit::Suspended(_)));
    let pause = CancelToken::new();
    let mut committed = 0;
    let mut updates = 0;
    let outcome = finished(
        search_continuous_resumable(
            &spec,
            &input,
            context,
            &limits,
            control(&cancel, &pause, Some(&saved)),
            |update| {
                if update.progress.phase != StudyPhase::Cleanup {
                    updates += 1;
                    if updates == 2 {
                        cancel.cancel();
                    }
                }
            },
            |_, _| {
                committed += 1;
                Ok(CheckpointAction::Continue)
            },
        )
        .unwrap(),
    );
    assert_eq!(updates, 2);
    assert_eq!(outcome.reason, TerminationReason::Cancelled);
    assert_eq!(outcome.progress.evaluated_candidates, 0);
    assert_eq!(committed, 0);
    let cancel = CancelToken::new();
    let recovered = finished(
        search_continuous_resumable(
            &spec,
            &input,
            context,
            &limits,
            control(&cancel, &pause, Some(&saved)),
            |_| {},
            |_, _| Ok(CheckpointAction::Continue),
        )
        .unwrap(),
    );
    assert_eq!(recovered.progress.evaluated_candidates, spec.max_trials);
}

#[test]
fn cancellation_and_expired_deadline_take_priority_over_initial_shutdown_suspend() {
    let (spec, input, context) = fixture();
    let limits = StudyLimits::default();
    for stopped in [false, true] {
        let cancel = CancelToken::new();
        let pause = CancelToken::new();
        pause.cancel();
        if stopped {
            cancel.cancel();
        }
        let mut options = control(&cancel, &pause, None);
        options.remaining_budget = Duration::ZERO;
        let outcome = finished(
            search_continuous_resumable(
                &spec,
                &input,
                context,
                &limits,
                options,
                |_| {},
                |_, _| Ok(CheckpointAction::Suspend),
            )
            .unwrap(),
        );
        assert_eq!(
            outcome.reason,
            if stopped {
                TerminationReason::Cancelled
            } else {
                TerminationReason::TimeBudget
            }
        );
        assert_eq!(outcome.progress.evaluated_candidates, 0);
    }
}

#[test]
fn exact_zero_score_is_a_committed_winner_not_missing_data() {
    let (mut spec, mut input, context) = fixture();
    let limits = StudyLimits::default();
    spec.algorithm_search.clear();
    spec.analytics_search.clear();
    spec.max_trials = 1;
    spec.tp_levels = vec![20.0];
    spec.sl_levels = vec![20.0];
    let cancel = CancelToken::new();
    let pause = CancelToken::new();
    let candidate = StudyCandidate {
        params: spec.algorithm_params.clone(),
        analytics: spec.analytics_params.clone(),
    };
    let raw = materialize_continuous(&spec, &input, &candidate, context, &limits, &cancel)
        .unwrap()
        .unwrap();
    let signals: Vec<_> = raw.instruments[0]
        .setups
        .iter()
        .map(|s| s.close_time_unix)
        .collect();
    assert!(signals.len() >= 2);
    for candle in &mut input.instruments[0].candles {
        candle.open = candle.close;
        candle.high = candle.close;
        candle.low = candle.close;
    }
    // Alternating TP/SL at the signal's entry minute gives exactly zero R. If
    // odd, leave the final entry right-censored rather than biasing either side.
    for (index, &signal) in signals.iter().take(signals.len() / 2 * 2).enumerate() {
        let candle = input.instruments[0]
            .candles
            .iter_mut()
            .find(|c| c.open_time_unix == signal)
            .unwrap();
        if index % 2 == 0 {
            candle.low = candle.open * 0.7;
        } else {
            candle.high = candle.open * 1.5;
        }
    }
    let mut bytes = Vec::new();
    let exit = search_continuous_resumable(
        &spec,
        &input,
        context,
        &limits,
        control(&cancel, &pause, None),
        |_| {},
        |info, encoded| {
            bytes = encoded.to_vec();
            Ok(if info.sequence == 1 {
                CheckpointAction::Suspend
            } else {
                CheckpointAction::Continue
            })
        },
    )
    .unwrap();
    let ContinuousSearchExit::Suspended(info) = exit else {
        panic!("expected committed trial boundary")
    };
    assert_eq!(info.progress.best_score, Some(0.0));
    let mut options = control(&cancel, &pause, Some(&bytes));
    options.remaining_budget = Duration::ZERO;
    let outcome = finished(
        search_continuous_resumable(
            &spec,
            &input,
            context,
            &limits,
            options,
            |_| {},
            |_, _| panic!("expired zero incumbent must not be re-evaluated"),
        )
        .unwrap(),
    );
    assert_eq!(outcome.best.unwrap().score.to_bits(), 0.0f64.to_bits());
    assert_eq!(outcome.progress.scorable_candidates, 1);
}

fn replace_path(value: &mut ciborium::Value, path: &[&str], replacement: ciborium::Value) {
    let ciborium::Value::Map(fields) = value else {
        panic!("expected map")
    };
    let child = &mut fields
        .iter_mut()
        .find(|(k, _)| k.as_text() == Some(path[0]))
        .unwrap()
        .1;
    if path.len() == 1 {
        *child = replacement;
    } else {
        replace_path(child, &path[1..], replacement);
    }
}
#[test]
fn corrupt_checkpoint_indices_versions_floats_and_trailing_bytes_return_typed_errors() {
    let (spec, input, context) = fixture();
    let limits = StudyLimits::default();
    let cancel = CancelToken::new();
    let pause = CancelToken::new();
    let mut checkpoint = Vec::new();
    search_continuous_resumable(
        &spec,
        &input,
        context,
        &limits,
        control(&cancel, &pause, None),
        |_| {},
        |info, bytes| {
            checkpoint = bytes.to_vec();
            Ok(if info.sequence == 1 {
                CheckpointAction::Suspend
            } else {
                CheckpointAction::Continue
            })
        },
    )
    .unwrap();
    let cases: Vec<(&[&str], ciborium::Value)> = vec![
        // A missing infeasible-baseline summary is valid, but deleting the
        // summary of a scored trial0 must still fail closed.
        (&["baseline"], ciborium::Value::Null),
        (&["version"], ciborium::Value::Integer(99.into())),
        (
            &["search", "enumeration"],
            ciborium::Value::Array(vec![ciborium::Value::Integer(99999.into())]),
        ),
        (
            &["search", "initial_index"],
            ciborium::Value::Integer(99999.into()),
        ),
        (
            &["progress", "elapsedSeconds"],
            ciborium::Value::Float(f64::NAN),
        ),
    ];
    for (path, replacement) in cases {
        let mut value: ciborium::Value = ciborium::de::from_reader(checkpoint.as_slice()).unwrap();
        replace_path(&mut value, path, replacement);
        let mut bytes = Vec::new();
        ciborium::ser::into_writer(&value, &mut bytes).unwrap();
        assert!(matches!(
            search_continuous_resumable(
                &spec,
                &input,
                context,
                &limits,
                control(&cancel, &pause, Some(&bytes)),
                |_| {},
                |_, _| panic!("bad checkpoint committed")
            ),
            Err(StudyError::InvalidInput(_))
        ));
    }
    for bytes in [
        &checkpoint[..checkpoint.len() / 2],
        &[checkpoint.as_slice(), &[0]].concat(),
    ] {
        assert!(
            search_continuous_resumable(
                &spec,
                &input,
                context,
                &limits,
                control(&cancel, &pause, Some(bytes)),
                |_| {},
                |_, _| Ok(CheckpointAction::Continue)
            )
            .is_err()
        );
    }
    let mut changed = spec.clone();
    changed.seed += 1;
    assert!(
        search_continuous_resumable(
            &changed,
            &input,
            context,
            &limits,
            control(&cancel, &pause, Some(&checkpoint)),
            |_| {},
            |_, _| Ok(CheckpointAction::Continue)
        )
        .is_err()
    );
}

#[test]
fn frozen_codec_preserves_raw_strings_numeric_bits_empty_scope_and_same_admitted_capacity() {
    let (_, mut input, _) = fixture();
    input.instruments[0].detector_bars.truncate(1025);
    input.instruments[0].candles.truncate(1025);
    let floats = [
        f64::from_bits(0x3fd5555555555557),
        -0.0,
        f64::MIN_POSITIVE,
        f64::from_bits(1),
        f64::MAX,
    ];
    for (index, candle) in input.instruments[0].candles.iter_mut().enumerate() {
        let value = floats[index % floats.len()];
        candle.open = value;
        candle.high = value;
        candle.low = value;
        candle.close = value;
    }
    input.instruments[0].detector_bars[0].close = " 000100.00000000 ".into();
    input.instruments[0].detector_bars[0].quote_turnover = "00010.00000000".into();
    for data in &mut input.instruments {
        data.detector_bars.shrink_to_fit();
        data.candles.shrink_to_fit();
    }
    input.instruments.push(InstrumentInput {
        instrument: StudyInstrument {
            symbol: "EMPTYUSDT".into(),
            ..input.instruments[0].instrument.clone()
        },
        detector_bars: vec![],
        candles: vec![],
    });
    input.instruments.shrink_to_fit();
    let original_bytes = snapshot_bytes(&input);
    let limits = FrozenInputCodecLimits {
        max_encoded_bytes: 10 * 1024 * 1024,
        max_decoded_bytes: original_bytes + 512,
        max_record_bytes: 4096,
        max_records: 10000,
    };
    let mut bytes = Vec::new();
    let expected = write_frozen_input(&input, &mut bytes, &limits).unwrap();
    let (decoded, digest) = read_frozen_input(&mut bytes.as_slice(), &limits).unwrap();
    assert_eq!(digest, expected);
    assert_eq!(decoded.snapshot_hash, input.snapshot_hash);
    assert_eq!(decoded.captured_at, input.captured_at);
    assert_eq!(decoded.instruments.len(), 2);
    assert!(decoded.instruments[1].detector_bars.is_empty());
    assert!(snapshot_bytes(&decoded) <= limits.max_decoded_bytes);
    assert_eq!(
        decoded.instruments[0].detector_bars,
        input.instruments[0].detector_bars
    );
    for (actual, expected) in decoded.instruments[0]
        .candles
        .iter()
        .zip(&input.instruments[0].candles)
    {
        assert_eq!(actual.open.to_bits(), expected.open.to_bits());
        assert_eq!(actual.high.to_bits(), expected.high.to_bits());
        assert_eq!(actual.low.to_bits(), expected.low.to_bits());
        assert_eq!(actual.close.to_bits(), expected.close.to_bits());
    }
    assert!(read_frozen_input(&mut &bytes[..bytes.len() - 1], &limits).is_err());
    let mut tight = limits.clone();
    tight.max_decoded_bytes = 1024;
    assert!(read_frozen_input(&mut bytes.as_slice(), &tight).is_err());
    let mut trailing = bytes.clone();
    trailing.push(0);
    assert!(read_frozen_input(&mut trailing.as_slice(), &limits).is_err());
    let mut tiny = limits.clone();
    tiny.max_record_bytes = 16;
    assert!(write_frozen_input(&input, &mut Vec::new(), &tiny).is_err());
}

#[test]
fn no_winner_can_resume_and_expire_without_fabricating_zero_score() {
    let (mut spec, mut input, context) = fixture();
    spec.algorithm_search = BTreeMap::new();
    spec.analytics_search = BTreeMap::new();
    spec.algorithm_params
        .insert("priceGrowthPct".into(), json!("1000"));
    for bar in &mut input.instruments[0].detector_bars {
        bar.close = "100".into();
    }
    for candle in &mut input.instruments[0].candles {
        candle.open = 100.0;
        candle.high = 101.0;
        candle.low = 99.0;
        candle.close = 100.0;
    }
    let limits = StudyLimits::default();
    let cancel = CancelToken::new();
    let pause = CancelToken::new();
    let mut saved = Vec::new();
    search_continuous_resumable(
        &spec,
        &input,
        context,
        &limits,
        control(&cancel, &pause, None),
        |_| {},
        |info, bytes| {
            saved = bytes.to_vec();
            Ok(if info.sequence == 1 {
                CheckpointAction::Suspend
            } else {
                CheckpointAction::Continue
            })
        },
    )
    .unwrap();
    let mut options = control(&cancel, &pause, Some(&saved));
    options.remaining_budget = Duration::ZERO;
    let outcome = finished(
        search_continuous_resumable(
            &spec,
            &input,
            context,
            &limits,
            options,
            |_| {},
            |_, _| Ok(CheckpointAction::Continue),
        )
        .unwrap(),
    );
    assert!(outcome.best.is_none());
    assert_eq!(outcome.progress.best_score, None);
    assert_eq!(outcome.progress.scorable_candidates, 0);
}
