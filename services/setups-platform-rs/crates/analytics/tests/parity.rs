use serde::Deserialize;
use serde_json::{Value, json};
use setups_analytics::{
    AnalyticsSetup, EvaluateLimitOptions, LimitStatus, MaturitySelection, OhlcCandle,
    TradeDirection, aggregate_limit_outcomes, evaluate_limit, filter_by_day_range, filter_by_month,
    passes_price_change, risk_return, select_by_maturity, setup_clusters_count, setup_months,
};

#[derive(Deserialize)]
struct Fixture {
    analytics: AnalyticsSection,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AnalyticsSection {
    clustering: Clustering,
    evaluation: Evaluation,
    price_gate: PriceGate,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Clustering {
    setups: Vec<AnalyticsSetup>,
    months: Vec<String>,
    cluster_counts: Vec<ClusterCount>,
    selections: Vec<SelectionCase>,
    month_filters: Vec<MonthFilter>,
    day_range_filters: Vec<DayFilter>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClusterCount {
    max_gap_seconds: i64,
    count: usize,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SelectionCase {
    maturity: f64,
    max_gap_seconds: i64,
    result: MaturitySelection,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MonthFilter {
    month: String,
    rows: Vec<AnalyticsSetup>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DayFilter {
    from_day: String,
    to_day: String,
    rows: Vec<AnalyticsSetup>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Evaluation {
    cases: Vec<EvalCase>,
    aggregate: Value,
    risk_returns: Vec<Option<f64>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct EvalCase {
    name: String,
    setup: AnalyticsSetup,
    direction: TradeDirection,
    tp_pct: f64,
    sl_pct: f64,
    time_shift_minutes: i64,
    horizon_minutes: i64,
    candles: Vec<OhlcCandle>,
    evaluation: Value,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PriceGate {
    cases: Vec<GateCase>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GateCase {
    direction: TradeDirection,
    pct: f64,
    shifted: f64,
    setup_close: f64,
    passed: bool,
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

fn assert_same(actual: Value, expected: Value, context: &str) {
    assert_eq!(canon(actual), canon(expected), "parity mismatch: {context}");
}

#[test]
fn analytics_engine_parity() {
    let fixture: Fixture =
        serde_json::from_str(include_str!("../../../fixtures/parity.json")).expect("fixture");

    let clustering = &fixture.analytics.clustering;
    let setups = &clustering.setups;

    assert_same(
        json!(setup_months(setups)),
        json!(clustering.months),
        "months",
    );

    for case in &clustering.cluster_counts {
        let actual = setup_clusters_count(setups, case.max_gap_seconds);
        assert_eq!(
            actual, case.count,
            "cluster count at gap {}",
            case.max_gap_seconds
        );
    }

    for case in &clustering.selections {
        let actual = select_by_maturity(setups, case.maturity, case.max_gap_seconds);
        assert_same(
            json!(actual),
            json!(case.result),
            &format!("maturity {} gap {}", case.maturity, case.max_gap_seconds),
        );
    }

    for case in &clustering.month_filters {
        let rows = filter_by_month(setups, &case.month).expect("month filter");
        assert_same(
            json!(rows),
            json!(case.rows),
            &format!("month {}", case.month),
        );
    }

    for case in &clustering.day_range_filters {
        let rows = filter_by_day_range(setups, &case.from_day, &case.to_day).expect("day range");
        assert_same(
            json!(rows),
            json!(case.rows),
            &format!("days {}..{}", case.from_day, case.to_day),
        );
    }

    let evaluation = &fixture.analytics.evaluation;
    let mut evaluations = Vec::with_capacity(evaluation.cases.len());
    for case in &evaluation.cases {
        let actual = evaluate_limit(EvaluateLimitOptions {
            candles: &case.candles,
            setup: &case.setup,
            direction: case.direction,
            tp_pct: case.tp_pct,
            sl_pct: case.sl_pct,
            time_shift_minutes: case.time_shift_minutes,
            horizon_minutes: case.horizon_minutes,
        });
        evaluations.push(actual);
        assert_same(
            json!(evaluations.last().unwrap()),
            case.evaluation.clone(),
            &case.name,
        );
    }

    let aggregate = aggregate_limit_outcomes(&evaluations, 10.0);
    assert_same(json!(aggregate), evaluation.aggregate.clone(), "aggregate");

    let risk_returns = vec![
        risk_return(LimitStatus::Tp, 2.0, 1.0),
        risk_return(LimitStatus::Sl, 2.0, 1.0),
        risk_return(LimitStatus::Unclosed, 2.0, 1.0),
        risk_return(LimitStatus::NoData, 2.0, 1.0),
    ];
    assert_same(
        json!(risk_returns),
        json!(evaluation.risk_returns),
        "risk returns",
    );

    for case in &fixture.analytics.price_gate.cases {
        let actual = passes_price_change(case.setup_close, case.shifted, case.direction, case.pct);
        assert_eq!(
            actual, case.passed,
            "gate {:?} pct {} shifted {}",
            case.direction, case.pct, case.shifted
        );
    }
}
