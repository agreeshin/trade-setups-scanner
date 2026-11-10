use serde::Deserialize;
use serde_json::{Value, json};
use setups_domain::{AlgorithmParams, SpikeParams};
use setups_replay::{ArrayCandleSource, CandleBarDto, ReplayRunOptions, replay_run};

#[derive(Deserialize)]
struct Fixture {
    replay: ReplaySection,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReplaySection {
    params: SpikeParams,
    from_unix: i64,
    to_unix: i64,
    bars: Vec<CandleBarDto>,
    result: Value,
}

fn canon(value: Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.into_iter().map(canon).collect()),
        Value::Object(entries) => Value::Object(
            entries
                .into_iter()
                .map(|(key, value)| (key, canon(value)))
                .collect(),
        ),
        Value::Number(number) => match number.as_f64() {
            Some(float) => serde_json::Number::from_f64(float)
                .map(Value::Number)
                .unwrap_or(Value::Null),
            None => Value::Null,
        },
        other => other,
    }
}

#[tokio::test]
async fn replay_runner_parity() {
    let fixture: Fixture =
        serde_json::from_str(include_str!("../../../fixtures/parity.json")).expect("fixture");
    let section = &fixture.replay;

    let source = ArrayCandleSource::new(section.bars.clone());
    let result = replay_run(ReplayRunOptions {
        source: &source,
        from_unix: section.from_unix,
        to_unix: section.to_unix,
        params: Some(AlgorithmParams::Spike(section.params.clone())),
        last_setup_unix: None,
        warmup_unix: None,
        cancel: None,
    })
    .await
    .expect("replay run succeeds");

    let actual = json!(result);
    let expected = section.result.clone();
    assert_eq!(
        canon(actual),
        canon(expected),
        "replay runner result must match TypeScript bit-for-bit"
    );
}

#[tokio::test]
async fn replay_runner_cancel_stops_midway() {
    let fixture: Fixture =
        serde_json::from_str(include_str!("../../../fixtures/parity.json")).expect("fixture");
    let section = &fixture.replay;

    let source = ArrayCandleSource::new(section.bars.clone());
    let token = setups_replay::CancelToken::new();
    token.cancel();
    let result = replay_run(ReplayRunOptions {
        source: &source,
        from_unix: section.from_unix,
        to_unix: section.to_unix,
        params: Some(AlgorithmParams::Spike(section.params.clone())),
        last_setup_unix: None,
        warmup_unix: None,
        cancel: Some(&token),
    })
    .await
    .expect("cancelled run still succeeds");
    assert!(result.cancelled);
    assert!(result.setups.is_empty());
    assert_eq!(result.processed_minutes, 0);
}
