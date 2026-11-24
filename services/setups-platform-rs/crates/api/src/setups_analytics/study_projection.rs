//! Issued-only study modal. GET reads sealed science and saved outcomes; an
//! explicit POST evaluates a fingerprinted, fixed-cutoff diagnostic in RAM.
//! No actualization/replay tables, market reads, writes, jobs or invented revisions.
//! Timezone is presentation-only: daily buckets and scientific cohorts stay UTC.
//! All wire timestamps are integer Unix seconds (sealed curve samples rounded
//! only at this display boundary; the shared evaluator retains full precision).
use super::*;
use serde::Serialize;
use setups_domain::{AdaptiveRecommendationSnapshot, expectancy::*, trend::TrendMode};
use setups_optimizer::{StudyBest, StudyRequest};
use sha2::{Digest, Sha256};

const MAX_INPUT_BYTES: i64 = 8 * 1024 * 1024;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct Filter {
    from_unix: Option<i64>,
    to_unix: Option<i64>,
    q: Option<String>,
}
impl Filter {
    fn validate(&self) -> bool {
        matches!(
            (self.from_unix, self.to_unix),
            (None, None) | (Some(0..), Some(0..))
        ) && self.from_unix.zip(self.to_unix).is_none_or(|(a, b)| a < b)
            && self.q.as_ref().is_none_or(|q| q.len() <= 200)
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Snapshot {
    snapshot_id: i64,
    observation_cutoff: i64,
    fingerprint: String,
}
#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct ReadQuery {
    from_unix: Option<i64>,
    to_unix: Option<i64>,
    q: Option<String>,
    snapshot_id: Option<i64>,
    observation_cutoff: Option<i64>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct EvaluateRequest {
    filter: Filter,
    snapshot: Snapshot,
}

#[derive(Debug, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum Availability {
    Available,
    Unavailable { reason: String },
}
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Model {
    availability: Availability,
    method: Option<String>,
    points: Vec<ModelPoint>,
    mean_r: Option<f64>,
    forecast_r: Option<f64>,
    decision_r: Option<f64>,
    fitted_on_issued_data: bool,
}
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ModelPoint {
    at_unix: i64,
    value: Option<f64>,
}
impl Model {
    fn unavailable(reason: &str, issued: bool) -> Self {
        Self {
            availability: Availability::Unavailable {
                reason: reason.into(),
            },
            method: None,
            points: vec![],
            mean_r: None,
            forecast_r: None,
            decision_r: None,
            // Population identity, not a claim that an unavailable fit ran.
            fitted_on_issued_data: issued,
        }
    }
    fn series(series: ExpectancyObjectiveSeries, issued: bool) -> Self {
        if series.decision_r.as_ref().is_none() {
            let reason = serde_json::to_value(&series.decision_r)
                .ok()
                .and_then(|v| v["reason"].as_str().map(str::to_owned))
                .unwrap_or_else(|| "objective_unavailable".into());
            return Self::unavailable(&reason, issued);
        }
        Self {
            availability: Availability::Available,
            method: Some(series.method.into()),
            mean_r: series.mean_r.as_ref().copied(),
            forecast_r: series.forecast_r.as_ref().copied(),
            decision_r: series.decision_r.as_ref().copied(),
            fitted_on_issued_data: issued,
            points: series
                .curve
                .into_iter()
                .map(|(at_unix, value)| ModelPoint {
                    at_unix: at_unix.round() as i64,
                    value,
                })
                .collect(),
        }
    }
}
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
struct Day {
    day_unix: i64,
    tp_count: u64,
    sl_count: u64,
    closed_count: u64,
    sum_r: Option<f64>,
    mean_r: Option<f64>,
    historical_closed_count: u64,
    issued_closed_count: u64,
}
fn observed(artifact: &ExpectancyArtifact, issued: &[UtcDailyInput]) -> Vec<Day> {
    let mut days = BTreeMap::<i64, Day>::new();
    let q = artifact.source.tp_pct / artifact.source.sl_pct;
    if let Some(original) = artifact.daily.as_ref() {
        for p in original {
            let day = days.entry(p.day_start_unix).or_insert_with(|| Day {
                day_unix: p.day_start_unix,
                ..Default::default()
            });
            day.tp_count += p.tp_count;
            day.sl_count += p.sl_count;
            day.historical_closed_count += p.tp_count + p.sl_count;
        }
    }
    for p in issued {
        let day = days.entry(p.day_start_unix).or_insert_with(|| Day {
            day_unix: p.day_start_unix,
            ..Default::default()
        });
        day.tp_count += p.tp_count;
        day.sl_count += p.sl_count;
        day.issued_closed_count += p.tp_count + p.sl_count;
    }
    days.into_values()
        .map(|mut day| {
            day.closed_count = day.tp_count + day.sl_count;
            day.sum_r =
                (day.closed_count > 0).then(|| q * day.tp_count as f64 - day.sl_count as f64);
            day.mean_r = day.sum_r.map(|sum| sum / day.closed_count as f64);
            day
        })
        .collect()
}
struct Source {
    spec: StudyRequest,
    best: StudyBest,
    objective: ExpectancyObjective,
    tp: f64,
    sl: f64,
    identity: String,
    created_at: i64,
}
fn resolve(
    spec: Value,
    result: Value,
    identity: String,
    created_at: i64,
) -> Result<Source, &'static str> {
    let spec: StudyRequest =
        serde_json::from_value(spec).map_err(|_| "study_recipe_unavailable")?;
    let best: StudyBest =
        serde_json::from_value(result["best"].clone()).map_err(|_| "winner_recipe_unavailable")?;
    let pair = if spec.forecast.is_some() {
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
    let objective = crate::study_actual::model::source_objective(&spec)?;
    Ok(Source {
        spec,
        best,
        objective,
        tp: pair.tp_pct,
        sl: pair.sl_pct,
        identity,
        created_at,
    })
}
fn artifact_matches(
    a: &ExpectancyArtifact,
    source: &Source,
    study: i64,
) -> Result<(), &'static str> {
    a.validate().map_err(|_| "original_model_invalid")?;
    if a.source.study_id != study.to_string()
        || a.source.provenance.snapshot_hash != source.best.provenance.snapshot_hash
    {
        return Err("original_source_mismatch");
    }
    if a.source.tp_pct != source.tp || a.source.sl_pct != source.sl {
        return Err("original_pair_mismatch");
    }
    if a.source.forecast != source.objective.forecast {
        return Err("original_recipe_mismatch");
    }
    let direction = match source.spec.direction {
        TradeDirection::Short => "short",
        TradeDirection::Long => "long",
    };
    if json!(a.source.direction) != direction {
        return Err("original_trade_conditions_mismatch");
    }
    let analytics = &source.best.analytics;
    if a.source.analytics.time_shift_minutes != analytics.time_shift_minutes
        || a.source.analytics.price_change_pct != analytics.price_change_pct
        || a.source.analytics.cluster_enabled != analytics.cluster_enabled
        || a.source.analytics.horizon_minutes != analytics.horizon_minutes
    {
        return Err("original_trade_conditions_mismatch");
    }
    Ok(())
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct IssuedRow {
    id: i64,
    signal: i64,
    recommendation: Value,
    outcome: Option<Value>,
}
struct Cohorts {
    scientific: Vec<UtcDailyInput>,
    observed: Vec<UtcDailyInput>,
    closed: usize,
}
fn cohorts(
    rows: &[IssuedRow],
    source: &Source,
    study: i64,
    cutoff: i64,
) -> Result<Cohorts, &'static str> {
    let mut science = BTreeMap::<i64, UtcDailyInput>::new();
    let mut display = BTreeMap::<i64, UtcDailyInput>::new();
    let mut closed_count = 0;
    let q = source.tp / source.sl;
    for row in rows {
        let rec: AdaptiveRecommendationSnapshot =
            serde_json::from_value(row.recommendation.clone())
                .map_err(|_| "issued_recommendation_invalid")?;
        if id_of(&row.recommendation.get("studyId").cloned()).is_some_and(|id| id != study) {
            return Err("issued_study_mismatch");
        }
        if rec.tp_pct != source.tp || rec.sl_pct != source.sl {
            return Err("issued_pair_mismatch");
        }
        if rec.forecast != source.objective.forecast {
            return Err("issued_recipe_mismatch");
        }
        let direction = match source.spec.direction {
            TradeDirection::Short => "short",
            TradeDirection::Long => "long",
        };
        if json!(rec.direction) != direction
            || rec.provenance.snapshot_hash != source.best.provenance.snapshot_hash
            || rec.provenance.algorithm_id != source.spec.algorithm_id
            || rec.provenance.algorithm_version != source.spec.algorithm_version
        {
            return Err("issued_recommendation_source_mismatch");
        }
        let a = &source.best.analytics;
        if rec.time_shift_minutes != a.time_shift_minutes
            || rec.price_change_pct != a.price_change_pct
            || rec.cluster_enabled != a.cluster_enabled
            || (a.cluster_enabled && rec.cluster_gap_minutes != a.cluster_gap_minutes)
            || a.horizon_minutes != live_outcomes::HORIZON_MINUTES
        {
            return Err("issued_trade_conditions_mismatch");
        }
        let saved = ExpectancyObjective::from_recommendation(&rec)
            .map_err(|_| "issued_objective_invalid")?;
        if saved.trend_mode != source.objective.trend_mode
            || (source.objective.trend_mode != TrendMode::Mean
                && source.objective.forecast.is_none()
                && saved.trend_bend_days != source.objective.trend_bend_days)
            || (source.objective.trend_mode == TrendMode::Blend
                && saved.trend_blend_lambda != source.objective.trend_blend_lambda)
        {
            return Err("issued_objective_mismatch");
        }
        let outcome: SetupOutcome =
            serde_json::from_value(row.outcome.clone().ok_or("issued_outcomes_missing")?)
                .map_err(|_| "issued_outcome_invalid")?;
        if row.signal > cutoff || outcome.observed_through_unix > cutoff {
            return Err("issued_outcome_after_cutoff");
        }
        let closed = matches!(outcome.status.as_str(), "tp" | "sl");
        if !matches!(
            outcome.status.as_str(),
            "tp" | "sl" | "unclosed" | "no_data" | "gate_failed"
        ) {
            return Err("issued_outcome_invalid");
        }
        if closed {
            let expected = if outcome.status == "tp" { q } else { -1.0 };
            if !outcome.is_final
                || outcome.risk_return.is_none_or(|r| {
                    !r.is_finite() || (r - expected).abs() > 1e-9 * expected.abs().max(1.0)
                })
                || outcome.entry_time_unix != row.signal.checked_add(rec.time_shift_minutes * 60)
                || outcome
                    .entry_time_unix
                    .is_none_or(|at| at < 0 || at > cutoff)
                || outcome.close_time_unix.is_none_or(|at| {
                    at < outcome.entry_time_unix.unwrap_or(i64::MAX)
                        || at.checked_add(60).is_none_or(|end| end > cutoff)
                })
            {
                return Err("issued_outcome_invalid");
            }
            closed_count += 1;
            add(
                &mut display,
                outcome.entry_time_unix.unwrap(),
                &outcome.status,
                q,
            );
        } else if outcome.risk_return.is_some() {
            return Err("issued_outcome_invalid");
        }
        // Exactly Decision50's canonical selection population. In legacy mean
        // gate-only rows retain support; all forecast recipes exclude gate failures.
        if outcome.status != "gate_failed"
            || (source.objective.trend_mode == TrendMode::Mean
                && source.objective.forecast.is_none())
        {
            add(&mut science, row.signal, &outcome.status, q);
        }
    }
    Ok(Cohorts {
        scientific: science.into_values().collect(),
        observed: display.into_values().collect(),
        closed: closed_count,
    })
}
fn add(days: &mut BTreeMap<i64, UtcDailyInput>, at: i64, status: &str, q: f64) {
    let day = at.div_euclid(86400) * 86400;
    let p = days.entry(day).or_insert(UtcDailyInput {
        day_start_unix: day,
        row_count: 0,
        tp_count: 0,
        sl_count: 0,
        mean_r: None,
    });
    p.row_count += 1;
    p.tp_count += u64::from(status == "tp");
    p.sl_count += u64::from(status == "sl");
    let closed = p.tp_count + p.sl_count;
    p.mean_r = (closed > 0).then(|| (q * p.tp_count as f64 - p.sl_count as f64) / closed as f64);
}

fn reply(value: Value) -> Response {
    ([("cache-control", "no-store")], Json(value)).into_response()
}
fn valid_bounds(snapshot: Option<i64>, cutoff: Option<i64>) -> bool {
    snapshot.is_none_or(|id| (0..=i64::from(i32::MAX)).contains(&id))
        && cutoff.is_none_or(|c| c >= 0 && c % 60 == 0 && c <= chrono::Utc::now().timestamp())
}
pub(super) async fn read(
    State(state): State<Arc<AnalyticsState>>,
    headers: HeaderMap,
    Path(study): Path<i64>,
    query: Result<Query<ReadQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    if let Err(e) = guard_auth(&state.auth, &headers, false) {
        return e;
    }
    let Ok(Query(query)) = query else {
        return error_response(StatusCode::BAD_REQUEST, "invalid_query");
    };
    let filter = Filter {
        from_unix: query.from_unix,
        to_unix: query.to_unix,
        q: query.q,
    };
    if study <= 0
        || !filter.validate()
        || !valid_bounds(query.snapshot_id, query.observation_cutoff)
    {
        return error_response(StatusCode::BAD_REQUEST, "invalid_query");
    }
    respond(
        state,
        study,
        filter,
        query.snapshot_id,
        query.observation_cutoff,
        None,
    )
    .await
}
pub(super) async fn evaluate(
    State(state): State<Arc<AnalyticsState>>,
    headers: HeaderMap,
    Path(study): Path<i64>,
    body: Result<Json<EvaluateRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(e) = guard_auth(&state.auth, &headers, true) {
        return e;
    }
    let Ok(Json(request)) = body else {
        return error_response(StatusCode::BAD_REQUEST, "invalid_body");
    };
    if study <= 0
        || !request.filter.validate()
        || !valid_bounds(
            Some(request.snapshot.snapshot_id),
            Some(request.snapshot.observation_cutoff),
        )
        || request.snapshot.fingerprint.len() != 64
        || !request
            .snapshot
            .fingerprint
            .bytes()
            .all(|b| b.is_ascii_hexdigit())
    {
        return error_response(StatusCode::BAD_REQUEST, "invalid_body");
    }
    respond(
        state,
        study,
        request.filter,
        Some(request.snapshot.snapshot_id),
        Some(request.snapshot.observation_cutoff),
        Some(request.snapshot.fingerprint),
    )
    .await
}
async fn respond(
    state: Arc<AnalyticsState>,
    study: i64,
    filter: Filter,
    max_id: Option<i64>,
    cutoff: Option<i64>,
    expected: Option<String>,
) -> Response {
    match load(&state.pool, study, &filter, max_id, cutoff).await {
        Err(e) => e,
        Ok((source, artifact, artifact_id, artifact_key, rows, issue, snapshot)) => {
            if expected
                .as_ref()
                .is_some_and(|hash| hash != &snapshot.fingerprint)
            {
                return error_response(
                    StatusCode::CONFLICT,
                    "setups_study_projection_snapshot_changed",
                );
            }
            let evaluate = expected.is_some();
            let _permit = if evaluate {
                match state.forecast_cpu.clone().try_acquire_owned() {
                    Ok(p) => Some(p),
                    Err(_) => return error_response(StatusCode::CONFLICT, "setups_forecast_busy"),
                }
            } else {
                None
            };
            let work = move || {
                let original = match &artifact {
                    Ok(a) => a
                        .objective_series(source.objective)
                        .map(|s| Model::series(s, false))
                        .unwrap_or_else(|_| {
                            Model::unavailable("original_objective_unavailable", false)
                        }),
                    Err(reason) => Model::unavailable(reason, false),
                };
                let cohorts = issue.map_or_else(
                    || cohorts(&rows, &source, study, snapshot.observation_cutoff),
                    Err,
                );
                let combined = match (&artifact, &cohorts) {
                    (Err(reason), _) => Model::unavailable(reason, true),
                    (_, Err(reason)) => Model::unavailable(reason, true),
                    (Ok(_), Ok(c)) if c.closed == 0 => {
                        Model::unavailable("issued_no_closed_outcomes", true)
                    }
                    (Ok(_), Ok(_)) if !evaluate => {
                        Model::unavailable("issued_projection_not_evaluated", true)
                    }
                    (Ok(a), Ok(c)) => match crate::expectancy_projection::evaluate(
                        a,
                        source.objective,
                        &c.scientific,
                        snapshot.observation_cutoff,
                    ) {
                        Ok(s) => Model::series(s, true),
                        Err(reason) => Model::unavailable(reason, true),
                    },
                };
                let original_daily = artifact
                    .as_ref()
                    .ok()
                    .map(|a| observed(a, &[]))
                    .unwrap_or_default();
                let daily = match (&artifact, &cohorts) {
                    (Ok(a), Ok(c)) => observed(a, &c.observed),
                    _ => original_daily.clone(),
                };
                json!({"version":1,"studyId":study,"sourceKind":"persisted_issued_setups","retrospective":false,
                    "objective":source.objective,"recommendedPair":{"tpPct":source.tp,"slPct":source.sl},
                    "originalSource":{"studyIdentity":source.identity,"artifactId":artifact_id,"artifactKey":artifact_key,"revisionKey":"original",
                        "observationCutoff":artifact.as_ref().ok().map(|a|a.source.provenance.observation_cutoff)},
                    "issuedSource":{"availability":match &cohorts {Ok(_)=>Availability::Available,Err(reason)=>Availability::Unavailable{reason:(*reason).into()}},
                        "outcomePolicy":"issued_open_entry_sl_first_10080m","setupCount":issue.is_none().then_some(rows.len()),"closedCount":cohorts.as_ref().ok().map(|c|c.closed),
                        "scientificDay":"raw_signal_close_utc","observedDay":"entry_utc"},
                    "filter":filter,"snapshot":snapshot,"startDayUnix":source.created_at.div_euclid(86400)*86400,
                    "originalProjection":original,"combinedProjection":combined,"originalDaily":original_daily,"daily":daily})
            };
            // Even GET's sealed curve materialization/validation is bounded and
            // off the I/O executor. GET never enters the diagnostic evaluator.
            match tokio::task::spawn_blocking(work).await {
                Ok(value) => reply(value),
                Err(_) => read_error(),
            }
        }
    }
}

type Loaded = (
    Source,
    Result<ExpectancyArtifact, &'static str>,
    Option<i64>,
    Option<String>,
    Vec<IssuedRow>,
    Option<&'static str>,
    Snapshot,
);
async fn load(
    pool: &sqlx::PgPool,
    study: i64,
    filter: &Filter,
    max_id: Option<i64>,
    cutoff: Option<i64>,
) -> Result<Loaded, Response> {
    let mut tx = pool.begin().await.map_err(|_| read_error())?;
    sqlx::query("set transaction isolation level repeatable read, read only")
        .execute(&mut *tx)
        .await
        .map_err(|_| read_error())?;
    sqlx::query("set local statement_timeout='5000'")
        .execute(&mut *tx)
        .await
        .map_err(|_| read_error())?;
    let row=sqlx::query("select status,case when pg_column_size(spec)+coalesce(pg_column_size(result),0)<=8388608 then spec end spec,case when pg_column_size(spec)+coalesce(pg_column_size(result),0)<=8388608 then result end result,floor(extract(epoch from created_at))::bigint created_at,md5(spec::text||coalesce(result::text,'null')) identity from optimization_studies where id=$1 and owner_id='admin'")
        .bind(study).fetch_optional(&mut *tx).await.map_err(|_|read_error())?.ok_or_else(||error_response(StatusCode::NOT_FOUND,"study_not_found"))?;
    if !matches!(
        row.get::<String, _>("status").as_str(),
        "completed" | "cancelled" | "failed"
    ) {
        return Err(error_response(StatusCode::CONFLICT, "study_active"));
    }
    let source = resolve(
        row.get::<Option<Value>, _>("spec").ok_or_else(|| {
            error_response(StatusCode::BAD_REQUEST, "study_source_budget_exceeded")
        })?,
        row.get::<Option<Value>, _>("result").unwrap_or(Value::Null),
        row.get("identity"),
        row.get("created_at"),
    )
    .map_err(|reason| error_response(StatusCode::CONFLICT, reason))?;
    let artifact_row=sqlx::query("select id,artifact_key,case when pg_column_size(numerical_snapshot)<=1048576 then numerical_snapshot end numerical_snapshot from study_expectancy_artifacts where owner_id='admin' and source_study_id=$1 and revision_key='original' and tp_pct=$2 and sl_pct=$3 order by id desc limit 1")
        .bind(study).bind(source.tp).bind(source.sl).fetch_optional(&mut *tx).await.map_err(|_|read_error())?;
    let artifact_id = artifact_row.as_ref().map(|r| r.get::<i64, _>("id"));
    let artifact_key = artifact_row
        .as_ref()
        .map(|r| r.get::<String, _>("artifact_key"));
    let artifact = artifact_row
        .as_ref()
        .ok_or("original_model_unavailable")
        .and_then(|r| {
            serde_json::from_value::<ExpectancyArtifact>(
                r.get::<Option<Value>, _>("numerical_snapshot")
                    .ok_or("original_model_budget_exceeded")?,
            )
            .map_err(|_| "original_model_invalid")
        })
        .and_then(|a| {
            artifact_matches(&a, &source, study)?;
            Ok(a)
        });
    // Generation identity is authoritative. Archived owner/study and then the
    // saved envelope are fallbacks, never the profile's current winner.
    let mut query = sqlx::QueryBuilder::new(
        "with selected as (select id,extract(epoch from close_time)::bigint signal,recommendation,trade_outcome from live_setups l where adaptive_slot_id is not null and recommendation is not null and exists(select 1 from adaptive_profiles p where p.id=l.adaptive_slot_id and p.owner_id='admin') and coalesce((select g.study_id from continuous_generations g where g.id=coalesce(l.adaptive_generation_id,case when (l.adaptive_generation_archive->>'generationId')~'^[0-9]{1,18}$' then (l.adaptive_generation_archive->>'generationId')::bigint end) and g.owner_id='admin'),case when l.adaptive_generation_archive->>'ownerId'='admin' and (l.adaptive_generation_archive->>'studyId')~'^[0-9]{1,18}$' then (l.adaptive_generation_archive->>'studyId')::bigint end,case when (recommendation->>'studyId')~'^[0-9]{1,18}$' then (recommendation->>'studyId')::bigint end)=",
    );
    query.push_bind(study);
    setups_db::runs::push_live_setup_filters(
        &mut query,
        &setups_db::runs::LiveSetupFilter {
            algorithm_id: Some(source.spec.algorithm_id.clone()),
            from_unix: filter.from_unix,
            to_unix: filter.to_unix,
            q: filter.q.clone(),
            studies: None,
        },
    );
    if let Some(id) = max_id {
        query.push(" and id<=").push_bind(id as i32);
    }
    if let Some(c) = cutoff {
        query
            .push(" and close_time<=to_timestamp(")
            .push_bind(c as f64)
            .push(")");
    }
    query.push(" order by id limit 5001) select id,signal,(sum(pg_column_size(recommendation)::bigint+coalesce(pg_column_size(trade_outcome),0)) over())::bigint input_bytes,case when sum(pg_column_size(recommendation)::bigint+coalesce(pg_column_size(trade_outcome),0)) over()<=8388608 then recommendation end recommendation,case when sum(pg_column_size(recommendation)::bigint+coalesce(pg_column_size(trade_outcome),0)) over()<=8388608 then trade_outcome end trade_outcome from selected order by id");
    let saved = query
        .build()
        .fetch_all(&mut *tx)
        .await
        .map_err(|_| read_error())?;
    let issue = if saved.len() > MAX_ANALYTICS_SETUPS as usize {
        Some("issued_setups_limit_exceeded")
    } else if saved
        .first()
        .is_some_and(|r| r.get::<i64, _>("input_bytes") > MAX_INPUT_BYTES)
    {
        Some("issued_source_budget_exceeded")
    } else {
        None
    };
    let upper = max_id.unwrap_or_else(|| {
        saved
            .last()
            .map(|r| i64::from(r.get::<i32, _>("id")))
            .unwrap_or(0)
    });
    let mut rows = Vec::new();
    if issue.is_none() {
        for row in saved {
            rows.push(IssuedRow {
                id: i64::from(row.get::<i32, _>("id")),
                signal: row.get("signal"),
                recommendation: row.get("recommendation"),
                outcome: row.get("trade_outcome"),
            });
        }
    }
    let original_cutoff = artifact
        .as_ref()
        .ok()
        .map(|a| a.source.provenance.observation_cutoff)
        .unwrap_or(0);
    let cutoff = cutoff.unwrap_or_else(|| {
        rows.iter()
            .map(|r| {
                r.outcome
                    .as_ref()
                    .and_then(|v| v.get("observedThroughUnix"))
                    .and_then(Value::as_i64)
                    .unwrap_or(r.signal)
                    .max(r.signal)
            })
            .max()
            .unwrap_or(original_cutoff)
            .max(original_cutoff)
    });
    let mut hash = Sha256::new();
    hash.update(b"issued-study-projection-v1\0");
    hash.update(serde_json::to_vec(&json!({"study":source.identity,"artifact":artifact_key,"originalSnapshot":artifact.as_ref().ok(),"rows":rows,"filter":filter,"snapshotId":upper,"cutoff":cutoff,"issue":issue})).map_err(|_|read_error())?);
    let snapshot = Snapshot {
        snapshot_id: upper,
        observation_cutoff: cutoff,
        fingerprint: format!("{:x}", hash.finalize()),
    };
    tx.commit().await.map_err(|_| read_error())?;
    Ok((
        source,
        artifact,
        artifact_id,
        artifact_key,
        rows,
        issue,
        snapshot,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use setups_domain::{forecast::ForecastRecipe, trend::TrendBendDays};
    use tower::ServiceExt;
    #[allow(dead_code, unused_imports)]
    mod fixture {
        include!("../../tests/fixtures/issued_projection.rs");
    }

    fn source(f: &fixture::Fixture) -> Source {
        resolve(
            f.spec.clone(),
            f.result.clone(),
            "saved-study-identity".into(),
            f.golden.split,
        )
        .unwrap()
    }
    fn rows(f: &fixture::Fixture) -> Vec<IssuedRow> {
        f.rows
            .iter()
            .cloned()
            .map(|r| serde_json::from_value(r).unwrap())
            .collect()
    }
    #[test]
    fn issued_adapter_uses_canonical_cohorts_weights_and_objectives_without_mutation() {
        for recipe in [
            None,
            Some(ForecastRecipe::engineering_baseline()),
            Some(ForecastRecipe::Trend {
                version: 1,
                bend_days: TrendBendDays::Linear,
            }),
            Some(ForecastRecipe::Trend {
                version: 1,
                bend_days: TrendBendDays::Seven,
            }),
        ] {
            for mode in [TrendMode::Mean, TrendMode::Forecast, TrendMode::Blend] {
                let f = fixture::issued(mode, recipe);
                let s = source(&f);
                let rows = rows(&f);
                artifact_matches(&f.artifact, &s, 77).unwrap();
                let before = f.artifact.to_json().unwrap();
                let c = cohorts(&rows, &s, 77, f.golden.cutoff).unwrap();
                assert_eq!(c.scientific, f.golden.actual_daily, "{mode:?}/{recipe:?}");
                let model = crate::expectancy_projection::evaluate(
                    &f.artifact,
                    s.objective,
                    &c.scientific,
                    f.golden.cutoff,
                )
                .unwrap();
                assert!(
                    (model.decision_r.as_ref().unwrap() - f.golden.canonical_decision).abs()
                        < 1e-10
                );
                let wire = json!(Model::series(model, true));
                assert!(
                    wire["points"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .all(|p| p["atUnix"].as_i64().is_some())
                );
                let merged = observed(&f.artifact, &c.observed);
                let split = f.golden.split.div_euclid(86400) * 86400;
                let shared = merged.iter().find(|d| d.day_unix == split).unwrap();
                assert!(shared.historical_closed_count > 0 && shared.issued_closed_count > 0);
                assert_eq!(
                    shared.closed_count,
                    shared.historical_closed_count + shared.issued_closed_count
                );
                assert_eq!(
                    shared.mean_r,
                    shared.sum_r.map(|r| r / shared.closed_count as f64)
                );
                assert!(
                    !merged
                        .iter()
                        .any(|d| d.day_unix == f.artifact.origin_unix + 19 * 86400),
                    "absent days stay absent"
                );
                assert_eq!(f.artifact.to_json().unwrap(), before);
            }
        }
    }
    #[test]
    fn issued_fences_pair_recipe_objective_conditions_missing_and_future_outcomes() {
        let f = fixture::issued(
            TrendMode::Blend,
            Some(ForecastRecipe::engineering_baseline()),
        );
        let s = source(&f);
        let original = rows(&f);
        for (field, value, reason) in [
            ("tpPct", json!(20.0), "issued_pair_mismatch"),
            ("studyId", json!(78), "issued_study_mismatch"),
            (
                "forecast",
                json!({"method":"decayed_probability","version":1,"halfLifeDays":7.0,"kappa":10.0,"priorPolicy":"gross_break_even"}),
                "issued_recipe_mismatch",
            ),
            (
                "timeShiftMinutes",
                json!(2),
                "issued_trade_conditions_mismatch",
            ),
            (
                "searchForecast",
                json!({"bendDays":"linear","cellR":0.1,"matrixR":0.1,"blend":{"lambda":0.8,"cellR":0.1,"matrixR":0.1}}),
                "issued_objective_mismatch",
            ),
        ] {
            let mut rows = original.clone();
            rows[0].recommendation[field] = value;
            assert!(
                matches!(cohorts(&rows,&s,77,f.golden.cutoff),Err(r) if r==reason),
                "{field}"
            );
        }
        let mut rows = original.clone();
        rows[0].outcome = None;
        assert!(matches!(
            cohorts(&rows, &s, 77, f.golden.cutoff),
            Err("issued_outcomes_missing")
        ));
        let mut rows = original.clone();
        rows[0].outcome.as_mut().unwrap()["observedThroughUnix"] = json!(f.golden.cutoff + 60);
        assert!(matches!(
            cohorts(&rows, &s, 77, f.golden.cutoff),
            Err("issued_outcome_after_cutoff")
        ));
        let mut a = f.artifact.clone();
        a.source.tp_pct = 20.0;
        a.source.sl_pct = 10.0;
        assert_eq!(artifact_matches(&a, &s, 77), Err("original_pair_mismatch"));
    }
    #[tokio::test]
    async fn both_projection_methods_authenticate_before_storage_or_market() {
        let state = Arc::new(AnalyticsState {
            pool: sqlx::postgres::PgPoolOptions::new()
                .connect_lazy("postgres://unused@127.0.0.1:1/unused_test")
                .unwrap(),
            auth: Arc::new(crate::SessionAuth::new(&["fixture".into()])),
            reader: crate::analytics::AnalyticsCandleReader::new("http://127.0.0.1:1", "fixture"),
            market_url: "http://127.0.0.1:1".into(),
            market_key: "fixture".into(),
            forecast_cpu: Arc::new(tokio::sync::Semaphore::new(1)),
            outcome_compute: Arc::new(tokio::sync::Semaphore::new(1)),
        });
        let app = setups_analytics_router(state);
        for method in ["GET", "POST"] {
            let response = app
                .clone()
                .oneshot(
                    axum::http::Request::builder()
                        .method(method)
                        .uri("/v1/setups/analytics/study-expectancy/77/projection")
                        .header("content-type", "application/json")
                        .body(Body::from("{}"))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        }
    }
    #[test]
    fn post_rejects_legacy_live_wrappers_timezone_and_unknown_inputs() {
        for input in [
            json!({"live":[]}),
            json!({"filter":{},"snapshot":{},"timeZone":"UTC"}),
            json!({"filter":{"timeZone":"UTC"},"snapshot":{"snapshotId":1,"observationCutoff":60,"fingerprint":"a".repeat(64)}}),
        ] {
            assert!(serde_json::from_value::<EvaluateRequest>(input).is_err());
        }
        assert!(serde_json::from_value::<EvaluateRequest>(json!({"filter":{},"snapshot":{"snapshotId":1,"observationCutoff":60,"fingerprint":"a".repeat(64)}})).is_ok());
    }
}
