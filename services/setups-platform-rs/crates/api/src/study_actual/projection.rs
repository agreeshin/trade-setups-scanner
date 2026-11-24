//! Request-owned diagnostic projection. Reuses the sealed-objective evaluator;
//! never writes an artifact, selected cell, profile, recommendation or delivery.
use serde::Serialize;
use setups_domain::expectancy::{
    EXPECTANCY_MAX_POINTS, ExpectancyArtifact, ExpectancyInput, ExpectancyObjective,
    MetricAvailability, SelectedCellInput, UtcDailyInput, build_expectancy_artifact,
};
use std::collections::BTreeMap;

use super::display::DisplayAvailability;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CombinedProjection {
    pub availability: DisplayAvailability,
    pub method: Option<String>,
    pub points: Vec<ModelPoint>,
    pub fitted_on_post_study_data: bool,
    pub observation_cutoff: Option<i64>,
    pub revision: Option<i64>,
    pub job_id: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelPoint { pub at_unix: f64, pub value: Option<f64> }

pub struct PublishedActual<'a> {
    pub daily: &'a [UtcDailyInput],
    pub cutoff: i64,
    pub revision: i64,
    pub job_id: &'a str,
}

impl CombinedProjection {
    pub fn unavailable(reason: &str) -> Self {
        Self { availability: DisplayAvailability::Unavailable { reason: reason.into() },
            method: None, points: vec![], fitted_on_post_study_data: true,
            observation_cutoff: None, revision: None, job_id: None }
    }
}

pub fn project(
    artifact: Option<&ExpectancyArtifact>,
    objective: ExpectancyObjective,
    actual: Result<PublishedActual<'_>, &str>,
) -> CombinedProjection {
    let actual = match actual { Ok(actual) => actual, Err(reason) => return CombinedProjection::unavailable(reason) };
    let Some(artifact) = artifact else { return CombinedProjection::unavailable("original_model_unavailable"); };
    match evaluate(artifact, objective, &actual) {
        Ok((method, points)) => CombinedProjection {
            availability: DisplayAvailability::Available, method: Some(method), points,
            fitted_on_post_study_data: true, observation_cutoff: Some(actual.cutoff),
            revision: Some(actual.revision), job_id: Some(actual.job_id.into()),
        },
        Err(reason) => CombinedProjection::unavailable(reason),
    }
}

fn evaluate(artifact: &ExpectancyArtifact, objective: ExpectancyObjective, actual: &PublishedActual<'_>)
    -> Result<(String, Vec<ModelPoint>), &'static str>
{
    artifact.validate().map_err(|_| "original_model_invalid")?;
    objective.validate().map_err(|_| "original_objective_invalid")?;
    if artifact.source.forecast != objective.forecast { return Err("original_recipe_mismatch"); }
    if actual.revision <= 0 || actual.job_id.is_empty() || actual.cutoff < artifact.source.provenance.observation_cutoff {
        return Err("post_study_stale");
    }
    if actual.daily.len() > EXPECTANCY_MAX_POINTS { return Err("combined_series_budget_exceeded"); }
    if actual.daily.windows(2).any(|days| days[0].day_start_unix >= days[1].day_start_unix) { return Err("combined_daily_invalid"); }
    if !actual.daily.iter().any(|p| p.tp_count > 0 || p.sl_count > 0) { return Err("post_study_no_closed_outcomes"); }
    let original = artifact.daily.as_ref().ok_or("original_series_unavailable")?;
    // Keep original scientific setup-count weights (not display archive's closed
    // counts). New cohorts use the same recipe-dependent price-gate selection;
    // unclosed/no_data rows still carry weight, never a fabricated zero R.
    // Merge a shared UTC day BEFORE fitting, never fit two independent curves or
    // average their means. IANA observed geometry is an independent projection.
    let mut days = BTreeMap::<i64, UtcDailyInput>::new();
    let q = artifact.source.tp_pct / artifact.source.sl_pct;
    for p in original.iter().map(|p| UtcDailyInput { day_start_unix: p.day_start_unix,
        row_count: p.row_count, tp_count: p.tp_count, sl_count: p.sl_count, mean_r: p.mean_r })
        .chain(actual.daily.iter().copied())
    {
        let closed = p.tp_count.checked_add(p.sl_count).ok_or("combined_count_overflow")?;
        if p.day_start_unix % 86400 != 0 || p.day_start_unix > actual.cutoff || closed > p.row_count
            || p.mean_r.is_some() != (closed > 0)
            || p.mean_r.is_some_and(|value| !value.is_finite() || (value - (q*p.tp_count as f64-p.sl_count as f64)/closed as f64).abs() > 1e-9*value.abs().max(1.0)) {
            return Err("combined_daily_invalid");
        }
        let day = days.entry(p.day_start_unix).or_insert(UtcDailyInput { day_start_unix: p.day_start_unix,
            row_count: 0, tp_count: 0, sl_count: 0, mean_r: None });
        day.row_count = day.row_count.checked_add(p.row_count).ok_or("combined_count_overflow")?;
        day.tp_count = day.tp_count.checked_add(p.tp_count).ok_or("combined_count_overflow")?;
        day.sl_count = day.sl_count.checked_add(p.sl_count).ok_or("combined_count_overflow")?;
        let closed = day.tp_count.checked_add(day.sl_count).ok_or("combined_count_overflow")?;
        day.mean_r = (closed > 0).then(|| (q*day.tp_count as f64-day.sl_count as f64)/closed as f64);
        if days.len() > EXPECTANCY_MAX_POINTS { return Err("combined_series_budget_exceeded"); }
    }
    let (tp, sl) = days.values().try_fold((0u64, 0u64), |(tp, sl), p|
        Some((tp.checked_add(p.tp_count)?, sl.checked_add(p.sl_count)?))).ok_or("combined_count_overflow")?;
    let closed = tp.checked_add(sl).ok_or("combined_count_overflow")?;
    // Ephemeral evaluator input only. The clone is never persisted or returned
    // as original provenance; all arrays die with this bounded detail request.
    let mut source = artifact.source.clone();
    source.provenance.observation_cutoff = actual.cutoff;
    // Actual signal closes include C (runtime::emitted uses <= C), whereas
    // artifact provenance uses [from,to). C+1 includes the UTC bucket at an
    // exact-midnight C without advancing the model's observation cutoff.
    source.provenance.to_unix = actual.cutoff.checked_add(1).ok_or("combined_cutoff_invalid")?.max(source.provenance.to_unix);
    let diagnostic = build_expectancy_artifact(ExpectancyInput {
        source, selected_cell: Some(SelectedCellInput { raw_r: Some((q*tp as f64-sl as f64)/closed as f64), tp_count: tp, sl_count: sl }),
        matrix_mean_r: None, daily: MetricAvailability::Available { value: days.into_values().collect() },
    }).map_err(|_| "combined_evaluation_unavailable")?;
    let series = diagnostic.objective_series(objective).map_err(|_| "combined_evaluation_unavailable")?;
    if let MetricAvailability::Unavailable { reason } = series.decision_r {
        return Err(match reason {
            setups_domain::expectancy::ExpectancyUnavailableReason::InsufficientFiniteDays => "combined_insufficient_finite_days",
            _ => "combined_objective_unavailable",
        });
    }
    Ok((series.method.into(), series.curve.into_iter().map(|(at_unix, value)| ModelPoint { at_unix, value }).collect()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use setups_domain::{forecast::ForecastRecipe, trend::{TrendBendDays, TrendBlendLambda, TrendMode}};

    fn fixture(recipe: Option<ForecastRecipe>) -> ExpectancyArtifact {
        let value: serde_json::Value = serde_json::from_str(include_str!("../../../domain/tests/fixtures/expectancy_utc_v2.json")).unwrap();
        let mut input: ExpectancyInput = serde_json::from_value(value["input"].clone()).unwrap();
        input.source.forecast = recipe;
        build_expectancy_artifact(input).unwrap()
    }
    fn objective(mode: TrendMode, recipe: Option<ForecastRecipe>) -> ExpectancyObjective {
        let bend = match recipe { Some(ForecastRecipe::Trend { bend_days, .. }) => bend_days, _ => TrendBendDays::Linear };
        ExpectancyObjective { version: 1, trend_mode: mode, trend_bend_days: bend,
            trend_blend_lambda: TrendBlendLambda::try_from(0.3).unwrap(), forecast: recipe }
    }
    #[allow(dead_code, unused_imports)]
    mod golden_fixture { include!("../../tests/fixtures/combined_projection.rs"); }

    #[test]
    fn combined_split_matches_one_shot_canonical_evaluator_for_every_objective() {
        for recipe in [None, Some(ForecastRecipe::engineering_baseline()),
            Some(ForecastRecipe::Trend { version: 1, bend_days: TrendBendDays::Linear }),
            Some(ForecastRecipe::Trend { version: 1, bend_days: TrendBendDays::Seven })] {
            for mode in [TrendMode::Mean, TrendMode::Forecast, TrendMode::Blend] {
                let g = golden_fixture::golden(mode, recipe);
                let before = g.original.to_json().unwrap();
                let model = project(Some(&g.original), g.objective, Ok(PublishedActual {
                    daily: &g.actual_daily, cutoff: g.cutoff, revision: 1, job_id: "golden" }));
                assert!(matches!(model.availability, DisplayAvailability::Available), "{recipe:?}/{mode:?}: {model:?}");
                let expected = g.one_shot.objective_series(g.objective).unwrap();
                assert_eq!(model.method.as_deref(), Some(expected.method));
                assert_eq!(model.points.len(), expected.curve.len());
                for (point, (at, value)) in model.points.iter().zip(expected.curve) {
                    assert_eq!(point.at_unix, at);
                    match (point.value, value) {
                        (Some(actual), Some(expected)) => assert!((actual-expected).abs()<1e-10, "{recipe:?}/{mode:?} at {at}"),
                        (None, None) => {}, _ => panic!("availability differs"),
                    }
                }
                assert!((model.points.last().unwrap().value.unwrap()-g.canonical_decision).abs()<1e-10,
                    "diagnostic endpoint must match optimizer matrix decision, not just another artifact");
                assert_eq!(g.original.to_json().unwrap(), before);
                let split_day = g.split.div_euclid(86400)*86400;
                assert!(g.original.daily.as_ref().unwrap().iter().any(|p|p.day_start_unix==split_day));
                assert!(g.actual_daily.iter().any(|p|p.day_start_unix==split_day));
                // This fixture must catch the former count(*) bug, not merely
                // exercise a dataset where gate weights happen to cancel.
                if mode == TrendMode::Forecast && recipe.is_none() {
                    let mut wrong = g.actual_daily.clone();
                    for row in g.actual_rows.iter().filter(|r|r["status"]=="gate_failed") {
                        let day = row["setupTimeUnix"].as_i64().unwrap().div_euclid(86400)*86400;
                        if let Some(p) = wrong.iter_mut().find(|p|p.day_start_unix==day) { p.row_count += 1; }
                    }
                    let wrong = project(Some(&g.original), g.objective, Ok(PublishedActual {
                        daily: &wrong, cutoff: g.cutoff, revision: 1, job_id: "wrong-weights" }));
                    assert!((wrong.points.last().unwrap().value.unwrap()-g.canonical_decision).abs()>1e-5);
                }
            }
        }
    }

    #[test]
    fn combined_inclusive_midnight_cutoff_keeps_boundary_bucket_without_advancing_endpoint() {
        let artifact = fixture(None);
        let cutoff = (artifact.source.provenance.observation_cutoff.div_euclid(86400)+1)*86400;
        let at_boundary = UtcDailyInput { day_start_unix: cutoff, row_count: 1, tp_count: 1, sl_count: 0,
            mean_r: Some(artifact.source.tp_pct/artifact.source.sl_pct) };
        let model = project(Some(&artifact), objective(TrendMode::Mean, None), Ok(PublishedActual {
            daily: &[at_boundary], cutoff, revision: 1, job_id: "midnight" }));
        assert!(matches!(model.availability, DisplayAvailability::Available), "{model:?}");
        assert_eq!(model.observation_cutoff, Some(cutoff));
        assert_eq!(model.points.last().unwrap().at_unix, cutoff as f64);
        let after = UtcDailyInput { day_start_unix: cutoff+86400, ..at_boundary };
        assert!(matches!(project(Some(&artifact), objective(TrendMode::Mean, None), Ok(PublishedActual {
            daily: &[after], cutoff, revision: 1, job_id: "after" })).availability,
            DisplayAvailability::Unavailable { reason } if reason=="combined_daily_invalid"));
    }

    fn actual_day(artifact: &ExpectancyArtifact) -> UtcDailyInput {
        let day = artifact.daily.as_ref().unwrap().last().unwrap().day_start_unix;
        let q = artifact.source.tp_pct / artifact.source.sl_pct;
        UtcDailyInput { day_start_unix: day, row_count: 9, tp_count: 1, sl_count: 3, mean_r: Some((q-3.0)/4.0) }
    }
    #[test]
    fn combined_same_day_is_weighted_and_mean_is_horizontal_without_mutating_original() {
        let artifact = fixture(None); let before = artifact.to_json().unwrap(); let day = actual_day(&artifact);
        let cutoff = artifact.source.provenance.observation_cutoff + 86400;
        let model = project(Some(&artifact), objective(TrendMode::Mean, None), Ok(PublishedActual { daily: &[day], cutoff, revision: 2, job_id: "job" }));
        assert!(matches!(model.availability, DisplayAvailability::Available));
        let cell = artifact.selected_cell.as_ref().unwrap(); let q = artifact.source.tp_pct/ artifact.source.sl_pct;
        let expected = (q*(cell.tp_count+1) as f64-(cell.sl_count+3) as f64)/(cell.closed_count+4) as f64;
        assert!(model.points.len() >= 2); assert!(model.points.iter().all(|p| (p.value.unwrap()-expected).abs()<1e-12));
        assert_eq!(model.points.last().unwrap().at_unix, cutoff as f64);
        assert_eq!(artifact.to_json().unwrap(), before);
    }
    #[test]
    fn combined_probability_weighted_trend_legacy_and_blend_use_the_shared_objective_evaluator() {
        for recipe in [None, Some(ForecastRecipe::engineering_baseline()), Some(ForecastRecipe::Trend { version: 1, bend_days: TrendBendDays::Linear }), Some(ForecastRecipe::Trend { version: 1, bend_days: TrendBendDays::Seven })] {
            let artifact = fixture(recipe); let day = actual_day(&artifact);
            let cutoff = artifact.source.provenance.observation_cutoff + 86400;
            let run = |mode| project(Some(&artifact), objective(mode, recipe), Ok(PublishedActual { daily: &[day], cutoff, revision: 1, job_id: "job" }));
            let mean = run(TrendMode::Mean); let forecast = run(TrendMode::Forecast); let blend = run(TrendMode::Blend);
            for model in [&mean, &forecast, &blend] { assert!(matches!(model.availability, DisplayAvailability::Available), "{model:?}"); }
            let m = mean.points.last().unwrap().value.unwrap(); let f = forecast.points.last().unwrap().value.unwrap();
            assert!((blend.points.last().unwrap().value.unwrap() - (0.7*m+0.3*f)).abs()<1e-12);
            assert_ne!(f, artifact.objective_series(objective(TrendMode::Forecast, recipe)).unwrap().decision_r.as_ref().copied().unwrap());
            assert_eq!(forecast.points.last().unwrap().at_unix, cutoff as f64);
            if !matches!(recipe, Some(ForecastRecipe::DecayedProbability { .. })) {
                assert_eq!(forecast.points.last().unwrap().value, forecast.points[forecast.points.len()-2].value);
            }
        }
    }
    #[test]
    fn combined_missing_days_are_not_zero_filled_and_invalid_or_empty_actual_is_unavailable() {
        let artifact = fixture(Some(ForecastRecipe::engineering_baseline()));
        let mut day = actual_day(&artifact); day.day_start_unix += 4*86400;
        let cutoff = day.day_start_unix + 3600;
        let run = |days: &[UtcDailyInput]| project(Some(&artifact), objective(TrendMode::Forecast, artifact.source.forecast), Ok(PublishedActual { daily: days, cutoff, revision: 1, job_id: "job" }));
        let sparse = run(&[day]); assert!(matches!(sparse.availability, DisplayAvailability::Available));
        assert!(!sparse.points.iter().any(|p| p.at_unix == (day.day_start_unix-86400) as f64 && p.value.is_some()));
        assert!(matches!(run(&[]).availability, DisplayAvailability::Unavailable { .. }));
        let unclosed = UtcDailyInput { row_count: 3, tp_count: 0, sl_count: 0, mean_r: None, ..day };
        assert!(matches!(run(&[unclosed]).availability, DisplayAvailability::Unavailable { reason } if reason=="post_study_no_closed_outcomes"));
        assert!(matches!(run(&[day,day]).availability, DisplayAvailability::Unavailable { .. }));
        assert!(matches!(run(&vec![day; EXPECTANCY_MAX_POINTS+1]).availability, DisplayAvailability::Unavailable { reason } if reason=="combined_series_budget_exceeded"));
        day.mean_r = Some(99.0); assert!(matches!(run(&[day]).availability, DisplayAvailability::Unavailable { .. }));
        for reason in ["post_study_queued", "post_study_running", "post_study_failed", "post_study_stale", "post_study_not_calculated"] {
            let model = project(Some(&artifact), objective(TrendMode::Mean, None), Err(reason));
            assert!(model.points.is_empty());
            assert!(matches!(model.availability, DisplayAvailability::Unavailable { reason: r } if r==reason));
        }
        assert!(matches!(project(None, objective(TrendMode::Mean, None), Ok(PublishedActual { daily: &[day], cutoff, revision: 1, job_id: "job" })).availability,
            DisplayAvailability::Unavailable { reason } if reason=="original_model_unavailable"));
    }
}
