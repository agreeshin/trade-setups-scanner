use serde::Deserialize;
use serde_json::Value;
use setups_detector::PriceVolumeSpikeDetector;
use setups_detector::quality::ReplayQualityTracker;
use setups_domain::{DetectorBar, PushStatus, SkipReason, SpikeParams};

#[derive(Deserialize)]
struct Fixture {
    scenarios: Vec<Scenario>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Scenario {
    name: String,
    params: SpikeParams,
    last_setup_unix: Option<i64>,
    track_from_unix: i64,
    track_to_unix: i64,
    bars: Vec<(i64, String, String)>,
    digest: Digest,
    pushes: Vec<Value>,
    quality: Value,
    intervals: Vec<Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Digest {
    push_count: usize,
    warmup_count: usize,
    evaluated_count: usize,
    setup_count: usize,
    interesting_count: usize,
}

fn is_interesting(result: &setups_domain::PushResult) -> bool {
    if result.setup.is_some() || result.status == PushStatus::Evaluated {
        return true;
    }
    matches!(
        result.skip_reason,
        Some(SkipReason::RecentGap)
            | Some(SkipReason::PriceReferenceMissing)
            | Some(SkipReason::GapToleranceExceeded)
    )
}

#[test]
fn parity_with_typescript_detector() {
    let fixture: Fixture = serde_json::from_str(include_str!("../../../fixtures/parity.json"))
        .expect("fixture parses");
    assert_eq!(fixture.scenarios.len(), 9, "expected all parity scenarios");

    for scenario in &fixture.scenarios {
        let mut detector =
            PriceVolumeSpikeDetector::new(scenario.params.clone(), scenario.last_setup_unix)
                .expect("detector constructs");
        let mut tracker = ReplayQualityTracker::new(
            scenario.track_from_unix,
            scenario.track_to_unix,
            scenario.params.baseline_long_days as u64 * 1440,
        );

        let mut warmup_count = 0usize;
        let mut evaluated_count = 0usize;
        let mut setup_count = 0usize;
        let mut interesting: Vec<Value> = Vec::new();

        for (open_time_unix, close, quote_turnover) in &scenario.bars {
            let bar = DetectorBar {
                open_time_unix: *open_time_unix,
                close: close.clone(),
                quote_turnover: quote_turnover.clone(),
            };
            let result = detector.push(&bar).expect("push succeeds");
            tracker.observe(*open_time_unix, &result);
            match result.status {
                PushStatus::Warmup => warmup_count += 1,
                PushStatus::Evaluated => evaluated_count += 1,
            }
            if result.setup.is_some() {
                setup_count += 1;
            }
            if is_interesting(&result) {
                interesting.push(serde_json::to_value(&result).expect("serialize push"));
            }
        }
        tracker.finish();
        let intervals: Vec<Value> = tracker
            .drain()
            .iter()
            .map(|interval| serde_json::to_value(interval).expect("serialize interval"))
            .collect();

        let digest = &scenario.digest;
        assert_eq!(
            digest.push_count,
            scenario.bars.len(),
            "{}: push count",
            scenario.name
        );
        assert_eq!(
            warmup_count, digest.warmup_count,
            "{}: warmup count",
            scenario.name
        );
        assert_eq!(
            evaluated_count, digest.evaluated_count,
            "{}: evaluated count",
            scenario.name
        );
        assert_eq!(
            setup_count, digest.setup_count,
            "{}: setup count",
            scenario.name
        );
        assert_eq!(
            interesting.len(),
            digest.interesting_count,
            "{}: interesting count",
            scenario.name
        );
        assert_eq!(
            interesting, scenario.pushes,
            "{}: interesting pushes must match TypeScript bit-for-bit",
            scenario.name
        );
        assert_eq!(
            serde_json::to_value(tracker.quality()).expect("serialize quality"),
            scenario.quality,
            "{}: quality report must match TypeScript",
            scenario.name
        );
        assert_eq!(
            intervals, scenario.intervals,
            "{}: quality intervals must match TypeScript",
            scenario.name
        );
    }
}
