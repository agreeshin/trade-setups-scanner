// Persisted-issued adapter fixture backed by Decision50's optimizer golden.
#[allow(dead_code, unused_imports)]
#[path = "combined_projection.rs"]
pub mod canonical;
use serde_json::{Value, json};
use setups_domain::{expectancy::ExpectancyArtifact, forecast::ForecastRecipe, trend::TrendMode};

pub struct Fixture {
    pub golden: canonical::Golden,
    pub spec: Value,
    pub result: Value,
    pub artifact: ExpectancyArtifact,
    pub rows: Vec<Value>,
}
pub fn issued(mode: TrendMode, recipe: Option<ForecastRecipe>) -> Fixture {
    let g = canonical::golden(mode, recipe);
    let mut artifact = g.original.clone();
    artifact.source.study_id = "77".into();
    artifact.source.analytics.horizon_minutes = 10080;
    let mut spec = json!(g.spec);
    spec["toUnix"] = json!(g.split);
    spec["horizonMinutes"] = json!(10080);
    let cell = artifact.selected_cell.as_ref().unwrap();
    let series = artifact.objective_series(g.objective).unwrap();
    let result = json!({"best":{"score":0.5,"partial":false,"params":spec["algorithmParams"],
        "analytics":{"clusterEnabled":false,"clusterGapMinutes":null,"timeShiftMinutes":1,"priceChangePct":1.0,"setupMaturity":0.0,"horizonMinutes":10080},
        "matrix":[{"tpPct":10.0,"slPct":5.0,"riskExpectancy":series.decision_r.as_ref(),"meanRiskExpectancy":cell.raw_r.as_ref(),
            "forecastRiskExpectancy":series.forecast_r.as_ref(),"avgMovePct":cell.raw_r.as_ref().map(|r|r*5.0),"closedCount":cell.closed_count,
            "tpCount":cell.tp_count,"slCount":cell.sl_count,"unclosedCount":0,"noDataCount":0,"priceChangeFilteredCount":0}],
        "provenance":{"optimizerVersion":"plan-a-3","seed":7,"snapshotHash":artifact.source.provenance.snapshot_hash,"capturedAt":"2026-11-01T00:00:00Z"},
        "conclusions":[],"support":{"score":0.5,"setupCount":100,"selectedSetupCount":90,"evaluatedMinutes":1000,"warmupMinutes":1440},
        "trialIndex":0,"observationCutoff":g.split}});
    let rows=g.actual_rows.iter().enumerate().map(|(i,row)| {
        let signal=row["setupTimeUnix"].as_i64().unwrap();
        let status=row["status"].as_str().unwrap();
        let closed=matches!(status,"tp"|"sl");
        let entry=(status!="no_data").then_some(signal+60);
        let rec=json!({"direction":"short","tpPct":10.0,"slPct":5.0,"rawR":cell.raw_r.as_ref(),"meanR":cell.raw_r.as_ref(),"robustScore":0.5,
            "timeShiftMinutes":1,"priceChangePct":1.0,"clusterEnabled":false,"clusterGapMinutes":null,
            "provenance":artifact.source.provenance,"forecast":recipe,"studyId":77,"ownerId":"admin",
            "searchForecast":if mode==TrendMode::Mean {Value::Null}else{json!({"bendDays":g.objective.trend_bend_days,"cellR":0.5,"matrixR":0.5,
                "blend":if mode==TrendMode::Blend {json!({"lambda":g.objective.trend_blend_lambda,"cellR":0.5,"matrixR":0.5})}else{Value::Null}})}});
        json!({"id":i+1,"signal":signal,"recommendation":rec,"outcome":{"status":status,"entryTimeUnix":entry,
            "entryPrice":entry.map(|_|102.0),"gateThreshold":102.0,"tpPrice":null,"slPrice":null,
            "closeTimeUnix":closed.then_some(signal+120),"closePrice":null,"riskReturn":row["riskReturn"],
            "evaluatedAtUnix":g.cutoff,"observedThroughUnix":g.cutoff,"isFinal":closed || status=="gate_failed"}})
    }).collect();
    Fixture {
        golden: g,
        spec,
        result,
        artifact,
        rows,
    }
}
