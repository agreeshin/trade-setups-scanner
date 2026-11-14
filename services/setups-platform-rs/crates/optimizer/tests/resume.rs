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
        max_trials:48,max_seconds:120,seed:168};
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
