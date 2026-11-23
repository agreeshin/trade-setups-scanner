use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use setups_domain::{
    AlgorithmParams,
    expectancy::{ExpectancyArtifact, ExpectancyObjective, MetricAvailability},
};
use setups_optimizer::{EffectiveAnalytics, StudyBest, StudyInstrument, StudyRequest};
use sqlx::{PgPool, Row};

pub const MAX_CANDLES: i64 = 200_000;
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Source {
    pub algorithm_id: String,
    pub algorithm_version: String,
    pub params: AlgorithmParams,
    pub instruments: Vec<StudyInstrument>,
    pub start_unix: f64,
    pub execution_started_at: DateTime<Utc>,
    pub direction: setups_analytics::TradeDirection,
    pub analytics: EffectiveAnalytics,
    pub tp_pct: f64,
    pub sl_pct: f64,
    pub continuous: bool,
    pub objective: ExpectancyObjective,
    pub original_endpoint: Value,
    pub source_artifact_id: Option<i64>,
    pub artifact: Option<ExpectancyArtifact>,
    pub source_identity: String,
    pub selection_from_open_unix: i64,
}

pub async fn source(
    pool: &PgPool,
    study: i64,
    owner: &str,
    cutoff: i64,
) -> Result<Source, &'static str> {
    load_source(pool, study, owner, cutoff, true).await
}

/// Original viewing never inherits a refresh job's current-period admission budget.
pub async fn original_source(
    pool: &PgPool,
    study: i64,
    owner: &str,
) -> Result<Source, &'static str> {
    load_source(pool, study, owner, 0, false).await
}

async fn load_source(
    pool: &PgPool,
    study: i64,
    owner: &str,
    cutoff: i64,
    refresh_budget: bool,
) -> Result<Source, &'static str> {
    let row=sqlx::query("select s.spec,s.result,s.events,s.status,s.origin,coalesce(g.execution_started_at,s.execution_started_at) execution_start,md5(s.spec::text||coalesce(s.result::text,'null')) identity from optimization_studies s left join continuous_generations g on g.id=s.continuous_generation_id and g.owner_id=s.owner_id where s.id=$1 and s.owner_id=$2")
        .bind(study).bind(owner).fetch_optional(pool).await.map_err(|_|"study_actual_storage_unavailable")?.ok_or("study_not_found")?;
    if !matches!(
        row.get::<String, _>("status").as_str(),
        "completed" | "cancelled" | "failed"
    ) {
        return Err("study_active");
    }
    let spec: StudyRequest =
        serde_json::from_value(row.get("spec")).map_err(|_| "study_recipe_unavailable")?;
    let result: Value = row
        .get::<Option<Value>, _>("result")
        .ok_or("winner_missing")?;
    let best: StudyBest =
        serde_json::from_value(result.get("best").cloned().ok_or("winner_missing")?)
            .map_err(|_| "winner_recipe_unavailable")?;
    let start = row
        .get::<Option<DateTime<Utc>>, _>("execution_start")
        .or_else(|| {
            // Preserve legacy RFC3339 nanoseconds; malformed markers do not hide a
            // later valid retained execution event. Never substitute created_at.
            let events: Value = row.get("events");
            events
                .as_array()?
                .iter()
                .filter(|e| e["kind"] == "started")
                .find_map(|e| DateTime::parse_from_rfc3339(e["at"].as_str()?).ok())
                .map(|t| t.with_timezone(&Utc))
        })
        .ok_or("execution_start_unavailable")?;
    let params = setups_optimizer::saved_algorithm_params(&spec.algorithm_id, &best.params)
        .map_err(|_| "winning_parameters_unavailable")?;
    if params.algorithm_id() != spec.algorithm_id
        || params.algorithm_version() != spec.algorithm_version
    {
        return Err("algorithm_version_unavailable");
    }
    if crate::replay::validate_algorithm_params(&params).is_some() {
        return Err("winning_parameters_unavailable");
    }
    if spec.instruments.is_empty() || spec.instruments.len() > 1000 {
        return Err("study_universe_unavailable");
    }
    let mut identities = std::collections::BTreeSet::new();
    if spec.instruments.iter().any(|i| {
        i.exchange.is_empty()
            || i.category.is_empty()
            || i.symbol.is_empty()
            || i.exchange.len() + i.category.len() + i.symbol.len() > 390
            || !identities.insert((i.exchange.clone(), i.category.clone(), i.symbol.clone()))
    }) {
        return Err("study_universe_unavailable");
    }
    let a = &best.analytics;
    if a.horizon_minutes <= 0
        || a.horizon_minutes > 525_600
        || a.time_shift_minutes.abs() > 525_600
        || a.price_change_pct.is_some_and(|p| !p.is_finite())
        || a.cluster_enabled && a.cluster_gap_minutes.is_none_or(|g| g <= 0 || g > 525_600)
    {
        return Err("trade_conditions_unavailable");
    }
    let continuous = row.get::<String, _>("origin") == "continuous";
    let maturity = a.setup_maturity.unwrap_or(spec.setup_maturity);
    if !maturity.is_finite() || !(0.0..=1.0).contains(&maturity) || continuous && maturity != 0.0 {
        return Err("selection_recipe_unavailable");
    }
    let selected = if spec.forecast.is_some() {
        setups_optimizer::select_robust_cell_for_mode(
            &best.matrix,
            &spec.tp_levels,
            &spec.sl_levels,
            spec.min_closed_per_cell,
            spec.trend_mode,
        )
    } else {
        setups_optimizer::select_robust_cell(
            &best.matrix,
            &spec.tp_levels,
            &spec.sl_levels,
            spec.min_closed_per_cell,
        )
    }
    .map_err(|_| "recommended_pair_unavailable")?
    .ok_or("recommended_pair_unavailable")?;
    let objective = source_objective(&spec)?;
    let cell = best
        .matrix
        .iter()
        .find(|c| c.tp_pct == selected.tp_pct && c.sl_pct == selected.sl_pct)
        .ok_or("recommended_pair_unavailable")?;
    let available = |v: Option<f64>| match v.filter(|v| v.is_finite()) {
        Some(value) => json!({"status":"available","value":value}),
        None => json!({"status":"unavailable","reason":"saved_metric_unavailable"}),
    };
    let endpoint = json!({"meanR":available(cell.mean_risk_expectancy.or(cell.risk_expectancy.filter(|_|spec.trend_mode.is_mean()))),"forecastR":available(cell.forecast_risk_expectancy.or(cell.risk_expectancy.filter(|_|spec.trend_mode==setups_domain::trend::TrendMode::Forecast))),"decisionR":available(cell.risk_expectancy),"atUnix":best.observation_cutoff});
    let artifact_row=sqlx::query("select id,numerical_snapshot from study_expectancy_artifacts where owner_id=$1 and source_study_id=$2 and revision_key='original' and tp_pct=$3 and sl_pct=$4 order by id desc limit 1")
        .bind(owner).bind(study).bind(selected.tp_pct).bind(selected.sl_pct).fetch_optional(pool).await.map_err(|_|"study_actual_storage_unavailable")?;
    let artifact = artifact_row
        .as_ref()
        .and_then(|r| {
            serde_json::from_value::<ExpectancyArtifact>(r.get("numerical_snapshot")).ok()
        })
        .filter(|a| {
            a.validate().is_ok()
                && a.source.study_id == study.to_string()
                && a.source.provenance.snapshot_hash == best.provenance.snapshot_hash
                && a.source.forecast == spec.forecast
        });
    let artifact_id = artifact
        .as_ref()
        .and_then(|_| artifact_row.as_ref().map(|r| r.get::<i64, _>("id")));
    let context_open = first_signal_open(start)
        - a.cluster_gap_minutes
            .filter(|_| a.cluster_enabled)
            .unwrap_or(0)
            * 60
        - i64::from(params.cooldown_minutes()) * 60;
    let mut analytics = best.analytics;
    analytics.setup_maturity = Some(maturity);
    // Nonzero manual maturity depends on both ends of a cluster. Retain its
    // original detector interval as context, not a truncated first cluster.
    let selection_from_open_unix = if !continuous && analytics.cluster_enabled && maturity > 0.0 {
        spec.from_unix.min(context_open)
    } else {
        context_open
    };
    let value = Source {
        algorithm_id: spec.algorithm_id,
        algorithm_version: spec.algorithm_version,
        params,
        instruments: spec.instruments,
        start_unix: start.timestamp() as f64 + start.timestamp_subsec_nanos() as f64 / 1e9,
        execution_started_at: start,
        direction: spec.direction,
        analytics,
        tp_pct: selected.tp_pct,
        sl_pct: selected.sl_pct,
        continuous,
        objective,
        original_endpoint: endpoint,
        source_artifact_id: artifact_id,
        artifact,
        source_identity: row.get("identity"),
        selection_from_open_unix,
    };
    if refresh_budget {
        value.refresh_eligibility(cutoff)?;
    }
    Ok(value)
}

fn source_objective(spec: &StudyRequest) -> Result<ExpectancyObjective, &'static str> {
    // Same method dispatch as optimizer::trend::effective_bend: the legacy
    // spec bend is inactive for explicit recipes. Never rewrite the saved spec.
    let bend = match spec.forecast {
        Some(setups_domain::forecast::ForecastRecipe::TrendSumR { .. }) => return Err("unsupported_study_forecast"),
        Some(setups_domain::forecast::ForecastRecipe::Trend { bend_days, .. }) => bend_days,
        Some(setups_domain::forecast::ForecastRecipe::DecayedProbability { .. }) => {
            setups_domain::trend::TrendBendDays::Linear
        }
        None => spec.trend_bend_days,
    };
    let objective = ExpectancyObjective {
        version: 1,
        trend_mode: spec.trend_mode,
        trend_bend_days: bend,
        trend_blend_lambda: spec.trend_blend_lambda,
        forecast: spec.forecast,
    };
    objective
        .validate()
        .map_err(|_| "objective_recipe_invalid")?;
    Ok(objective)
}

pub fn first_signal_open(start: DateTime<Utc>) -> i64 {
    let first_close = if start.timestamp() % 60 == 0 && start.timestamp_subsec_nanos() == 0 {
        start.timestamp()
    } else {
        start.timestamp().div_euclid(60) * 60 + 60
    };
    first_close - 60
}
impl Source {
    pub fn refresh_eligibility(&self, cutoff: i64) -> Result<(), &'static str> {
        let input_minutes = (cutoff - self.selection_from_open_unix).max(0) / 60
            + i64::from(self.params.baseline_long_days()) * 1440;
        if cutoff < 0
            || input_minutes > MAX_CANDLES
            || cutoff - self.execution_started_at.timestamp() > 3660 * 86400
        {
            return Err("study_actual_period_budget_exceeded");
        }
        if serde_json::to_vec(self)
            .map_err(|_| "source_invalid")?
            .len()
            > 1024 * 1024
        {
            return Err("study_actual_source_budget_exceeded");
        }
        Ok(())
    }
    pub fn work_minutes(&self, cutoff: i64) -> i64 {
        ((cutoff - self.selection_from_open_unix).max(0) / 60
            + i64::from(self.params.baseline_long_days()) * 1440)
            .saturating_mul(self.instruments.len() as i64)
    }
}

pub fn chart(source: &Source, cutoff: i64, actual: Vec<Value>) -> Value {
    let unavailable = json!({"status":"unavailable","reason":"original_series_unavailable"});
    let mut report = json!({"sourceArtifactId":source.source_artifact_id,"retrospective":true,"executionStartedAt":source.execution_started_at,"observedThroughUnix":cutoff,
        "actual":{"daily":actual},"historical":{"availability":unavailable,"daily":[]},"originalObjective":source.objective,"originalEndpoint":source.original_endpoint,
        "projection":{"availability":unavailable,"method":null,"points":[],"fittedOnPostStudyData":false}});
    if let Some(artifact) = &source.artifact {
        if let MetricAvailability::Available { value } = &artifact.daily {
            report["historical"] = json!({"availability":{"status":"available"},"daily":value.iter().map(|p|json!({"dayUnix":p.day_start_unix,"plotAtUnix":(p.day_start_unix as f64).min(source.start_unix),"meanR":p.mean_r,"closedCount":p.tp_count+p.sl_count,"tpCount":p.tp_count,"slCount":p.sl_count})).collect::<Vec<_>>()});
        }
        // b1 owns the additive typed projection helper; no combined/live-series fitting here.
        if let Ok(series) = artifact.objective_series_through(source.objective, cutoff) {
            if series.decision_r.as_ref().is_some() {
                report["projection"] = json!({"availability":{"status":"available"},"method":series.method,"points":series.curve.iter().map(|(at,value)|json!({"atUnix":at,"value":value})).collect::<Vec<_>>(),"fittedOnPostStudyData":false});
            }
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use setups_domain::{
        forecast::ForecastRecipe,
        trend::{TrendBendDays, TrendBlendLambda, TrendMode},
    };

    fn objective_spec(mode: TrendMode, forecast: Option<ForecastRecipe>) -> StudyRequest {
        serde_json::from_value(json!({
            "algorithmId":"price_volume_spike","algorithmVersion":"1.1.1",
            "algorithmParams":setups_domain::SpikeParams::default(),
            "analyticsParams":setups_optimizer::AnalyticsParams::default(),
            "instruments":[{"exchange":"fixture","category":"linear","symbol":"FIX"}],
            "fromUnix":0,"toUnix":30*86400,"direction":"short","seed":42,
            "trendMode":mode,"trendBendDays":"30","trendBlendLambda":0.6,"forecast":forecast
        }))
        .unwrap()
    }

    #[test]
    fn source_objective_probability_ignores_retained_trend_bend_without_mutating_spec() {
        for mode in [TrendMode::Forecast, TrendMode::Blend, TrendMode::Mean] {
            let spec = objective_spec(mode, Some(ForecastRecipe::engineering_baseline()));
            let before = serde_json::to_vec(&spec).unwrap();
            let objective =
                source_objective(&spec).expect("valid saved probability recipe must stay eligible");
            assert_eq!(objective.trend_bend_days, TrendBendDays::Linear);
            assert_eq!(objective.trend_mode, mode);
            assert_eq!(objective.trend_blend_lambda, spec.trend_blend_lambda);
            assert_eq!(objective.forecast, spec.forecast);
            assert_eq!(objective.version, 1);
            assert_eq!(serde_json::to_vec(&spec).unwrap(), before);
            let mean = MetricAvailability::Available { value: 0.25 };
            let forecast = MetricAvailability::Available { value: 0.75 };
            let expected = match mode {
                TrendMode::Mean => 0.25,
                TrendMode::Forecast => 0.75,
                TrendMode::Blend => spec.trend_blend_lambda.combine(0.25, 0.75),
            };
            assert_eq!(
                objective.decision(&mean, &forecast).unwrap().as_ref(),
                Some(&expected)
            );
        }
    }

    #[test]
    fn source_objective_explicit_trend_uses_its_own_bend_not_retained_legacy_bend() {
        for mode in [TrendMode::Forecast, TrendMode::Blend] {
            let recipe = ForecastRecipe::Trend {
                version: 1,
                bend_days: TrendBendDays::Seven,
            };
            let spec = objective_spec(mode, Some(recipe));
            let before = serde_json::to_vec(&spec).unwrap();
            let objective = source_objective(&spec).unwrap();
            assert_eq!(objective.trend_bend_days, TrendBendDays::Seven);
            assert_eq!(objective.trend_mode, mode);
            assert_eq!(objective.forecast, Some(recipe));
            assert_eq!(serde_json::to_vec(&spec).unwrap(), before);
        }
    }

    #[test]
    fn source_objective_preserves_legacy_bends_modes_lambda_and_missing_defaults() {
        for mode in [TrendMode::Mean, TrendMode::Forecast, TrendMode::Blend] {
            let spec = objective_spec(mode, None);
            let objective = source_objective(&spec).unwrap();
            assert_eq!(objective.trend_bend_days, TrendBendDays::Thirty);
            assert_eq!(objective.trend_mode, mode);
            assert_eq!(objective.trend_blend_lambda, spec.trend_blend_lambda);
            assert_eq!(objective.forecast, None);
        }
        let mut value = serde_json::to_value(objective_spec(TrendMode::Mean, None)).unwrap();
        for key in ["trendMode", "trendBendDays", "trendBlendLambda", "forecast"] {
            value.as_object_mut().unwrap().remove(key);
        }
        let spec: StudyRequest = serde_json::from_value(value).unwrap();
        let objective = source_objective(&spec).unwrap();
        assert_eq!(objective.trend_mode, TrendMode::Mean);
        assert_eq!(objective.trend_bend_days, TrendBendDays::Linear);
        assert_eq!(objective.trend_blend_lambda, TrendBlendLambda::default());
        assert_eq!(objective.forecast, None);
    }

    #[test]
    fn source_objective_still_rejects_invalid_or_unsupported_explicit_recipes() {
        let mut invalid = ForecastRecipe::engineering_baseline();
        if let ForecastRecipe::DecayedProbability { version, .. } = &mut invalid {
            *version = 2;
        }
        let mut invalid_parameters = ForecastRecipe::engineering_baseline();
        if let ForecastRecipe::DecayedProbability { half_life_days, .. } = &mut invalid_parameters {
            *half_life_days = 0.0;
        }
        for recipe in [
            invalid,
            invalid_parameters,
            ForecastRecipe::Trend {
                version: 2,
                bend_days: TrendBendDays::Seven,
            },
        ] {
            for mode in [TrendMode::Mean, TrendMode::Forecast, TrendMode::Blend] {
                let mut spec = objective_spec(mode, None);
                spec.forecast = Some(recipe);
                assert_eq!(source_objective(&spec), Err("objective_recipe_invalid"));
            }
        }
        let mut unknown = serde_json::to_value(objective_spec(
            TrendMode::Forecast,
            Some(ForecastRecipe::engineering_baseline()),
        ))
        .unwrap();
        unknown["forecast"]["method"] = json!("unknown_method");
        assert!(serde_json::from_value::<StudyRequest>(unknown).is_err());
    }

    #[test]
    fn fractional_start_keeps_first_legal_signal_and_exact_boundary() {
        let start = DateTime::from_timestamp(600, 500_000_000).unwrap();
        assert_eq!(first_signal_open(start), 600);
        assert_eq!(
            first_signal_open(DateTime::from_timestamp(600, 0).unwrap()),
            540
        );
    }
}
