//! Pure, bounded original + diagnostic evaluator shared by retrospective and
//! actually-issued adapters. No job/revision identities, clock, storage or I/O.
use setups_domain::expectancy::{
    EXPECTANCY_MAX_POINTS, ExpectancyArtifact, ExpectancyInput, ExpectancyObjective,
    ExpectancyObjectiveSeries, MetricAvailability, SelectedCellInput, UtcDailyInput,
    build_expectancy_artifact,
};
use std::collections::BTreeMap;

pub(crate) fn merge_daily(
    artifact: &ExpectancyArtifact,
    additional: &[UtcDailyInput],
    cutoff: i64,
) -> Result<Vec<UtcDailyInput>, &'static str> {
    if additional.len() > EXPECTANCY_MAX_POINTS {
        return Err("combined_series_budget_exceeded");
    }
    if additional
        .windows(2)
        .any(|p| p[0].day_start_unix >= p[1].day_start_unix)
    {
        return Err("combined_daily_invalid");
    }
    let original = artifact
        .daily
        .as_ref()
        .ok_or("original_series_unavailable")?;
    let mut days = BTreeMap::<i64, UtcDailyInput>::new();
    let q = artifact.source.tp_pct / artifact.source.sl_pct;
    // Merge counts, not means. Keep scientific setup-count weights; a cohort
    // without closed outcomes has None, never a fabricated zero observation.
    for p in original
        .iter()
        .map(|p| UtcDailyInput {
            day_start_unix: p.day_start_unix,
            row_count: p.row_count,
            tp_count: p.tp_count,
            sl_count: p.sl_count,
            mean_r: p.mean_r,
        })
        .chain(additional.iter().copied())
    {
        let closed = p
            .tp_count
            .checked_add(p.sl_count)
            .ok_or("combined_count_overflow")?;
        if p.day_start_unix % 86400 != 0
            || p.day_start_unix > cutoff
            || closed > p.row_count
            || p.mean_r.is_some() != (closed > 0)
            || p.mean_r.is_some_and(|value| {
                !value.is_finite()
                    || (value - (q * p.tp_count as f64 - p.sl_count as f64) / closed as f64).abs()
                        > 1e-9 * value.abs().max(1.0)
            })
        {
            return Err("combined_daily_invalid");
        }
        let day = days.entry(p.day_start_unix).or_insert(UtcDailyInput {
            day_start_unix: p.day_start_unix,
            row_count: 0,
            tp_count: 0,
            sl_count: 0,
            mean_r: None,
        });
        day.row_count = day
            .row_count
            .checked_add(p.row_count)
            .ok_or("combined_count_overflow")?;
        day.tp_count = day
            .tp_count
            .checked_add(p.tp_count)
            .ok_or("combined_count_overflow")?;
        day.sl_count = day
            .sl_count
            .checked_add(p.sl_count)
            .ok_or("combined_count_overflow")?;
        let closed = day
            .tp_count
            .checked_add(day.sl_count)
            .ok_or("combined_count_overflow")?;
        day.mean_r =
            (closed > 0).then(|| (q * day.tp_count as f64 - day.sl_count as f64) / closed as f64);
        if days.len() > EXPECTANCY_MAX_POINTS {
            return Err("combined_series_budget_exceeded");
        }
    }
    Ok(days.into_values().collect())
}

pub(crate) fn evaluate(
    artifact: &ExpectancyArtifact,
    objective: ExpectancyObjective,
    additional: &[UtcDailyInput],
    cutoff: i64,
) -> Result<ExpectancyObjectiveSeries, &'static str> {
    artifact.validate().map_err(|_| "original_model_invalid")?;
    objective
        .validate()
        .map_err(|_| "original_objective_invalid")?;
    if artifact.source.forecast != objective.forecast {
        return Err("original_recipe_mismatch");
    }
    if cutoff < artifact.source.provenance.observation_cutoff {
        return Err("combined_cutoff_invalid");
    }
    let days = merge_daily(artifact, additional, cutoff)?;
    let (tp, sl) = days
        .iter()
        .try_fold((0u64, 0u64), |(tp, sl), p| {
            Some((tp.checked_add(p.tp_count)?, sl.checked_add(p.sl_count)?))
        })
        .ok_or("combined_count_overflow")?;
    let closed = tp.checked_add(sl).ok_or("combined_count_overflow")?;
    let q = artifact.source.tp_pct / artifact.source.sl_pct;
    // Ephemeral input only: never published as original provenance or saved.
    let mut source = artifact.source.clone();
    source.provenance.observation_cutoff = cutoff;
    source.provenance.to_unix = cutoff
        .checked_add(1)
        .ok_or("combined_cutoff_invalid")?
        .max(source.provenance.to_unix);
    let diagnostic = build_expectancy_artifact(ExpectancyInput {
        source,
        selected_cell: Some(SelectedCellInput {
            raw_r: (closed > 0).then(|| (q * tp as f64 - sl as f64) / closed as f64),
            tp_count: tp,
            sl_count: sl,
        }),
        matrix_mean_r: None,
        daily: MetricAvailability::Available { value: days },
    })
    .map_err(|_| "combined_evaluation_unavailable")?;
    let series = diagnostic
        .objective_series(objective)
        .map_err(|_| "combined_evaluation_unavailable")?;
    if let MetricAvailability::Unavailable { reason } = series.decision_r {
        return Err(match reason {
            setups_domain::expectancy::ExpectancyUnavailableReason::InsufficientFiniteDays => {
                "combined_insufficient_finite_days"
            }
            _ => "combined_objective_unavailable",
        });
    }
    Ok(series)
}
