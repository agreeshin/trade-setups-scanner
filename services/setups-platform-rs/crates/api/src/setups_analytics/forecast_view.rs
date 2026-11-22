use super::*;
use serde::Serialize;
use setups_domain::forecast::{ForecastRecipe, ProbabilityDay, decision_r, evaluate_probability};
use setups_domain::trend::{
    MAX_TREND_DAYS, TrendBlendLambda, TrendInput, TrendMode, fit_weighted_trend,
};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct Snapshot {
    snapshot_id: i64,
    observation_cutoff: i64,
    fingerprint: String,
}

pub(super) fn snapshot(
    rows: &[(AnalyticsSetupRow, ParsedRecommendation)],
    outcomes: &[Option<SetupOutcome>],
    cutoff: i64,
) -> Snapshot {
    let mut hash = Sha256::new();
    hash.update(cutoff.to_be_bytes());
    for ((row, _), outcome) in rows.iter().zip(outcomes) {
        let bytes = serde_json::to_vec(&json!([
            row.id,
            row.algorithm_id,
            row.exchange_code,
            row.category,
            row.symbol,
            row.close_unix,
            row.generation_id,
            row.recommendation,
            outcome
        ]))
        .expect("stored JSON and finite outcomes serialize");
        hash.update((bytes.len() as u64).to_be_bytes());
        hash.update(bytes);
    }
    Snapshot {
        snapshot_id: rows.iter().map(|(row, _)| row.id).max().unwrap_or(0),
        observation_cutoff: cutoff,
        fingerprint: format!("{:x}", hash.finalize()),
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct ForecastViewRequest {
    snapshot: Snapshot,
    forecast: ForecastRecipe,
    #[serde(default)]
    trend_mode: TrendMode,
    #[serde(default)]
    trend_blend_lambda: TrendBlendLambda,
    time_zone: String,
}

impl ForecastViewRequest {
    fn validate(&self, now: i64) -> Result<(), Response> {
        if !(0..=i64::from(i32::MAX)).contains(&self.snapshot.snapshot_id)
            || self.snapshot.observation_cutoff < 0
            || self.snapshot.observation_cutoff > now
            || self.snapshot.observation_cutoff % 60 != 0
            || self.snapshot.fingerprint.len() != 64
            || !self
                .snapshot
                .fingerprint
                .bytes()
                .all(|c| c.is_ascii_hexdigit())
            || self.time_zone.is_empty()
            || self.time_zone.len() > 100
            || !self
                .time_zone
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"/_-+".contains(&c))
            || self.forecast.validate().is_err()
        {
            return Err(error_response(
                StatusCode::BAD_REQUEST,
                "invalid_forecast_view",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy)]
struct CalendarDay {
    day: i64,
    at: i64,
    next: i64,
}

async fn calendar(
    pool: &sqlx::PgPool,
    zone: &str,
    entries: &[i64],
) -> Result<Vec<CalendarDay>, Response> {
    let valid: bool =
        sqlx::query_scalar("select exists(select 1 from pg_timezone_names where name=$1)")
            .bind(zone)
            .fetch_one(pool)
            .await
            .map_err(|_| read_error())?;
    if !valid {
        return Err(error_response(StatusCode::BAD_REQUEST, "invalid_time_zone"));
    }
    let (Some(first), Some(last)) = (entries.iter().min(), entries.iter().max()) else {
        return Ok(Vec::new());
    };
    let days = sqlx::query("with bounds as (select timezone($1,to_timestamp($2))::date lo, timezone($1,to_timestamp($3))::date hi), days as (select lo+i as day from bounds cross join lateral generate_series(0,case when hi-lo <= 3660 then hi-lo else 3661 end) i) select (day-date '1970-01-01')::bigint AS day,study_local_day_start($1,day) at,study_local_day_start($1,day+1) next from days order by day limit 3662")
        .bind(zone).bind(*first as f64).bind(*last as f64).fetch_all(pool).await.map_err(|_| read_error())?;
    if days.len() > MAX_TREND_DAYS {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "setups_forecast_period_limit_exceeded",
        ));
    }
    days.iter()
        .map(|row| {
            Ok(CalendarDay {
                day: row.try_get("day").map_err(|_| read_error())?,
                at: row.try_get("at").map_err(|_| read_error())?,
                next: row.try_get("next").map_err(|_| read_error())?,
            })
        })
        .collect()
}

fn day_for(calendar: &[CalendarDay], at: i64) -> Option<CalendarDay> {
    calendar
        .get(
            calendar
                .partition_point(|day| day.at <= at)
                .checked_sub(1)?,
        )
        .copied()
        .filter(|day| at < day.next)
}

fn unix_for(calendar: &[CalendarDay], day: f64) -> Option<i64> {
    let index = calendar
        .binary_search_by_key(&(day.floor() as i64), |day| day.day)
        .ok()?;
    let date = calendar[index];
    Some(date.at + ((date.next - date.at) as f64 * day.fract()).round() as i64)
}

fn eligible_entry(outcome: &SetupOutcome, cutoff: i64) -> Option<i64> {
    matches!(outcome.status.as_str(), "tp" | "sl" | "unclosed")
        .then_some(outcome.entry_time_unix)
        .flatten()
        .filter(|at| (0..=cutoff).contains(at))
}

fn closed_r(
    row: &AnalyticsSetupRow,
    outcome: &SetupOutcome,
    cutoff: i64,
) -> Result<Option<f64>, String> {
    if !matches!(outcome.status.as_str(), "tp" | "sl") {
        return Ok(None);
    }
    let valid = row.close_unix <= cutoff
        && eligible_entry(outcome, cutoff).is_some()
        && outcome
            .close_time_unix
            .and_then(|at| at.checked_add(60))
            .is_some_and(|available| available <= cutoff);
    if !valid {
        return Ok(None);
    }
    outcome
        .risk_return
        .filter(|r| r.is_finite())
        .map(Some)
        .ok_or_else(|| "setups_forecast_invalid_saved_outcome".to_owned())
}

#[derive(Debug, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum Availability {
    Available,
    Unavailable { reason: String },
}

fn availability(value: Option<f64>, reason: &str) -> Availability {
    match value {
        Some(_) => Availability::Available,
        None => Availability::Unavailable {
            reason: reason.to_owned(),
        },
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ModelPoint {
    at_unix: i64,
    forecast_r: Option<f64>,
    decision_r: Option<f64>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ForecastView {
    snapshot: Snapshot,
    snapshot_id: i64,
    projection_identity: String,
    selected: usize,
    closed: u64,
    availability: Availability,
    forecast_availability: Availability,
    mean_r: Option<f64>,
    forecast_r: Option<f64>,
    decision_r: Option<f64>,
    trend_mode: TrendMode,
    trend_blend_lambda: TrendBlendLambda,
    forecast: ForecastRecipe,
    time_zone: String,
    observation_cutoff: i64,
    now_unix: i64,
    retrospective: bool,
    model: Model,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Model {
    target: &'static str,
    method: &'static str,
    points: Vec<ModelPoint>,
    endpoint: ModelPoint,
}

struct ProbabilityGroup {
    q: f64,
    days: BTreeMap<i64, ProbabilityDay>,
}

type ProbabilityGroups = BTreeMap<(String, String), ProbabilityGroup>;

fn probability_at(
    groups: &ProbabilityGroups,
    recipe: ForecastRecipe,
    at: i64,
) -> Result<(Option<f64>, &'static str), String> {
    let mut weighted = 0.0;
    let mut closed = 0_u64;
    for group in groups.values() {
        let daily: Vec<_> = group
            .days
            .values()
            .copied()
            .take_while(|d| d.day_start_unix <= at)
            .collect();
        let count: u64 = daily.iter().map(|day| day.tp_count + day.sl_count).sum();
        if count == 0 {
            continue;
        }
        let estimate =
            evaluate_probability(recipe, group.q, at, &daily).map_err(|error| error.to_string())?;
        let value = match estimate {
            setups_domain::forecast::ProbabilityForecast::Available { value } => value.forecast_r,
            setups_domain::forecast::ProbabilityForecast::Unavailable { .. } => {
                return Ok((None, "group_forecast_unavailable"));
            }
        };
        weighted += value * count as f64;
        closed += count;
    }
    Ok((
        (closed > 0).then(|| weighted / closed as f64),
        "no_closed_outcomes",
    ))
}

fn compute(
    request: ForecastViewRequest,
    parsed: &[(AnalyticsSetupRow, ParsedRecommendation)],
    outcomes: &[Option<SetupOutcome>],
    calendar: &[CalendarDay],
    now: i64,
    projection_identity: String,
) -> Result<ForecastView, String> {
    if parsed.len() > MAX_ANALYTICS_SETUPS as usize || outcomes.len() != parsed.len() {
        return Err("setups_analytics_limit_exceeded".into());
    }
    let cutoff = request.snapshot.observation_cutoff;
    let mut sum_r = 0.0;
    let mut closed = 0_u64;
    let mut daily = BTreeMap::<i64, (u32, u32, f64)>::new();
    let mut groups = ProbabilityGroups::new();
    for ((row, rec), outcome) in parsed.iter().zip(outcomes) {
        let outcome = outcome
            .as_ref()
            .ok_or("setups_analytics_outcomes_missing")?;
        if row.close_unix > cutoff {
            continue;
        }
        let risk = closed_r(row, outcome, cutoff)?;
        if let Some(at) = eligible_entry(outcome, cutoff) {
            let day = day_for(calendar, at).ok_or("setups_forecast_calendar_mismatch")?;
            let bucket = daily.entry(day.day).or_default();
            bucket.0 += 1;
            if let Some(risk) = risk {
                bucket.1 += 1;
                bucket.2 += risk;
            }
        }
        if let Some(risk) = risk {
            sum_r += risk;
            closed += 1;
            let cohort = row.close_unix.div_euclid(86400) * 86400;
            let group = groups
                .entry((rec.tp_pct.to_string(), rec.sl_pct.to_string()))
                .or_insert_with(|| ProbabilityGroup {
                    q: rec.tp_pct / rec.sl_pct,
                    days: BTreeMap::new(),
                });
            let day = group.days.entry(cohort).or_insert(ProbabilityDay {
                day_start_unix: cohort,
                tp_count: 0,
                sl_count: 0,
            });
            day.tp_count += u64::from(outcome.status == "tp");
            day.sl_count += u64::from(outcome.status == "sl");
        }
    }
    let mean = (closed > 0).then(|| sum_r / closed as f64);
    let mut forecast_points = Vec::new();
    let (forecast, reason, method, target) = match request.forecast {
        recipe @ ForecastRecipe::DecayedProbability { .. } => {
            let (value, reason) = probability_at(&groups, recipe, cutoff)?;
            let mut times: Vec<i64> = groups
                .values()
                .flat_map(|group| group.days.keys())
                .map(|day| (day + 86400).min(cutoff))
                .collect();
            times.sort_unstable();
            times.dedup();
            let step = times.len().div_ceil(256).max(1);
            for at in times.into_iter().step_by(step) {
                forecast_points.push((at, probability_at(&groups, recipe, at)?.0));
            }
            if closed > 0 {
                forecast_points.push((cutoff, value));
            }
            (value, reason, "decayed_probability", "closed_trade_r")
        }
        ForecastRecipe::Trend { bend_days, .. } => {
            let input: Vec<_> = daily
                .into_iter()
                .map(|(day, (weight, count, sum))| TrendInput {
                    day: day as f64,
                    value: (count > 0).then_some(sum),
                    weight,
                })
                .collect();
            match fit_weighted_trend(&input, bend_days).map_err(|error| error.to_string())? {
                Some(fit) => {
                    let summary = fit.summary();
                    for point in summary.curve {
                        let at = unix_for(calendar, point.day)
                            .ok_or("setups_forecast_calendar_mismatch")?;
                        forecast_points.push((at, Some(point.value)));
                    }
                    (
                        Some(summary.end.value),
                        "insufficient_finite_days",
                        summary.method,
                        "daily_sum_r",
                    )
                }
                None => (None, "insufficient_finite_days", "trend", "daily_sum_r"),
            }
        }
    };
    let decision = decision_r(
        request.trend_mode,
        mean,
        forecast,
        request.trend_blend_lambda,
    )
    .map_err(|error| error.to_string())?;
    let point = |at, value| -> Result<ModelPoint, String> {
        Ok(ModelPoint {
            at_unix: at,
            forecast_r: value,
            decision_r: decision_r(request.trend_mode, mean, value, request.trend_blend_lambda)
                .map_err(|error| error.to_string())?,
        })
    };
    if request.trend_mode == TrendMode::Mean
        && forecast_points.is_empty()
        && let Some(first) = calendar.first()
        && mean.is_some()
    {
        forecast_points.push((first.at, None));
    }
    let mut points = forecast_points
        .into_iter()
        .filter(|(at, _)| *at <= cutoff)
        .map(|(at, value)| point(at, value))
        .collect::<Result<Vec<_>, _>>()?;
    points.dedup_by_key(|point| point.at_unix);
    if decision.is_some() {
        if points.last().is_none_or(|p| p.at_unix < cutoff) {
            points.push(point(cutoff, forecast)?);
        }
        if points.last().is_none_or(|p| p.at_unix < now) {
            points.push(point(now, forecast)?);
        }
    }
    Ok(ForecastView {
        snapshot_id: request.snapshot.snapshot_id,
        projection_identity,
        selected: parsed.len(),
        closed,
        snapshot: request.snapshot,
        availability: availability(
            decision,
            if mean.is_none() {
                "no_closed_outcomes"
            } else {
                reason
            },
        ),
        forecast_availability: availability(forecast, reason),
        mean_r: mean,
        forecast_r: forecast,
        decision_r: decision,
        trend_mode: request.trend_mode,
        trend_blend_lambda: request.trend_blend_lambda,
        forecast: request.forecast,
        time_zone: request.time_zone,
        observation_cutoff: cutoff,
        now_unix: now,
        retrospective: true,
        model: Model {
            target,
            method,
            points,
            endpoint: point(now, forecast)?,
        },
    })
}

fn projection_identity(
    request: &ForecastViewRequest,
    filter: &setups_db::runs::LiveSetupFilter,
) -> String {
    let mut studies = filter.studies.clone();
    if let Some(studies) = studies.as_mut() {
        studies.sort_unstable();
        studies.dedup();
    }
    let identity = json!({"version":1,"snapshot":request.snapshot,"algorithmId":filter.algorithm_id,
        "fromUnix":filter.from_unix,"toUnix":filter.to_unix,"q":filter.q,"studies":studies,
        "forecast":request.forecast,"trendMode":request.trend_mode,
        "trendBlendLambda":if request.trend_mode == TrendMode::Blend {Some(request.trend_blend_lambda)} else {None},
        "timeZone":request.time_zone});
    format!(
        "setups-forecast-view-v1:{:x}",
        Sha256::digest(
            serde_json::to_vec(&identity).expect("typed projection identity serializes")
        )
    )
}

pub(super) async fn handle(
    State(state): State<Arc<AnalyticsState>>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
    Json(request): Json<ForecastViewRequest>,
) -> Response {
    if let Err(response) = guard_auth(&state.auth, &headers, true) {
        return response;
    }
    let now = chrono::Utc::now().timestamp();
    if let Err(response) = request.validate(now) {
        return response;
    }
    let filter = match calculation_filter(&params) {
        Ok(filter) => filter,
        Err(response) => return response,
    };
    let from = filter.from_unix.expect("validated from");
    let to = filter.to_unix.expect("validated to");
    if from < 0
        || to.checked_sub(from).is_none_or(|span| span > 3660 * 86400)
        || chrono::DateTime::from_timestamp(to, 0).is_none()
    {
        return error_response(
            StatusCode::BAD_REQUEST,
            "setups_forecast_period_limit_exceeded",
        );
    }
    let Ok(permit) = state.forecast_cpu.clone().try_acquire_owned() else {
        return error_response(StatusCode::TOO_MANY_REQUESTS, "forecast_busy");
    };
    let rows = match load_setup_rows(
        &state.pool,
        filter.algorithm_id.as_deref().expect("validated algorithm"),
        filter.from_unix,
        filter.to_unix,
        filter.studies.as_deref(),
        true,
        filter.q.as_deref(),
        Some(request.snapshot.snapshot_id),
    )
    .await
    {
        Ok(rows) => rows,
        Err(response) => return response,
    };
    let mut parsed = Vec::with_capacity(rows.len());
    let mut outcomes = Vec::with_capacity(rows.len());
    for row in rows {
        let rec = match parse_recommendation(&row.recommendation) {
            Ok(rec) => rec,
            Err(_) => return read_error(),
        };
        let outcome = match row
            .trade_outcome
            .as_ref()
            .map(|v| serde_json::from_value::<SetupOutcome>(v.clone()))
        {
            Some(Ok(outcome)) => outcome,
            None => {
                return error_response(StatusCode::CONFLICT, "setups_analytics_outcomes_missing");
            }
            _ => return read_error(),
        };
        parsed.push((row, rec));
        outcomes.push(Some(outcome));
    }
    if snapshot(&parsed, &outcomes, request.snapshot.observation_cutoff) != request.snapshot {
        return error_response(StatusCode::CONFLICT, "setups_analytics_snapshot_changed");
    }
    let entries: Vec<_> = outcomes
        .iter()
        .flatten()
        .filter_map(|o| eligible_entry(o, request.snapshot.observation_cutoff))
        .collect();
    let calendar = match calendar(&state.pool, &request.time_zone, &entries).await {
        Ok(calendar) => calendar,
        Err(response) => return response,
    };
    let identity = projection_identity(&request, &filter);
    match tokio::task::spawn_blocking(move || {
        let _permit = permit;
        compute(request, &parsed, &outcomes, &calendar, now, identity)
    })
    .await
    {
        Ok(Ok(value)) => Json(value).into_response(),
        Ok(Err(error)) => error_response(StatusCode::BAD_REQUEST, &error),
        Err(_) => read_error(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use setups_domain::forecast::{FORECAST_RECIPE_VERSION, PriorPolicy};
    use setups_domain::trend::TrendBendDays;

    const DAY: i64 = 86_400;
    const BASE: i64 = 10 * DAY;

    fn row(
        id: i64,
        close_unix: i64,
        tp_pct: f64,
        sl_pct: f64,
    ) -> (AnalyticsSetupRow, ParsedRecommendation) {
        (
            AnalyticsSetupRow {
                id,
                exchange_code: "bybit".into(),
                category: "linear".into(),
                symbol: format!("TEST{id}"),
                algorithm_id: "test".into(),
                close_unix,
                close_price: "100".into(),
                slot_id: 1,
                generation_id: Some(1),
                recommendation: json!({"tpPct":tp_pct,"slPct":sl_pct}),
                artifact_id: None,
                trade_outcome: None,
            },
            ParsedRecommendation {
                direction: TradeDirection::Short,
                direction_raw: "short",
                tp_pct,
                sl_pct,
                time_shift_minutes: 0,
                price_change_pct: None,
                envelope_study_id: Some(1),
                external_name: None,
            },
        )
    }

    fn outcome(
        status: &str,
        entry_time_unix: i64,
        risk_return: Option<f64>,
    ) -> Option<SetupOutcome> {
        Some(SetupOutcome {
            status: status.into(),
            entry_time_unix: Some(entry_time_unix),
            entry_price: Some(100.0),
            gate_threshold: None,
            tp_price: Some(102.0),
            sl_price: Some(99.0),
            close_time_unix: risk_return.map(|_| entry_time_unix + 60),
            close_price: risk_return.map(|_| 102.0),
            risk_return,
            evaluated_at_unix: entry_time_unix + 120,
            observed_through_unix: entry_time_unix + 120,
            is_final: risk_return.is_some(),
        })
    }

    fn request(
        snapshot: Snapshot,
        forecast: ForecastRecipe,
        trend_mode: TrendMode,
    ) -> ForecastViewRequest {
        ForecastViewRequest {
            snapshot,
            forecast,
            trend_mode,
            trend_blend_lambda: TrendBlendLambda::default(),
            time_zone: "UTC".into(),
        }
    }

    #[test]
    fn snapshot_fingerprint_changes_when_a_saved_outcome_changes() {
        let parsed = vec![row(1, BASE, 2.0, 1.0)];
        let first = vec![outcome("tp", BASE + 60, Some(2.0))];
        let second = vec![outcome("sl", BASE + 60, Some(-1.0))];
        let cutoff = BASE + DAY;
        let first_snapshot = snapshot(&parsed, &first, cutoff);
        let second_snapshot = snapshot(&parsed, &second, cutoff);
        assert_eq!(first_snapshot.snapshot_id, second_snapshot.snapshot_id);
        assert_ne!(first_snapshot.fingerprint, second_snapshot.fingerprint);
    }

    #[test]
    fn trend_uses_daily_sum_r_while_mean_stays_per_closed_trade() {
        let entries = [
            (BASE + 60, 3.0),
            (BASE + 120, -1.0),
            (BASE + DAY + 60, 1.0),
            (BASE + 2 * DAY + 60, 3.0),
            (BASE + 2 * DAY + 120, 3.0),
        ];
        let parsed: Vec<_> = entries
            .iter()
            .enumerate()
            .map(|(index, (entry, _))| row(index as i64 + 1, entry - 60, 3.0, 1.0))
            .collect();
        let outcomes: Vec<_> = entries
            .iter()
            .map(|(entry, risk)| outcome("tp", *entry, Some(*risk)))
            .collect();
        let calendar = (0..3)
            .map(|offset| CalendarDay {
                day: 10 + offset,
                at: BASE + offset * DAY,
                next: BASE + (offset + 1) * DAY,
            })
            .collect::<Vec<_>>();
        let cutoff = BASE + 3 * DAY;
        let snapshot = snapshot(&parsed, &outcomes, cutoff);
        let view = compute(
            request(
                snapshot,
                ForecastRecipe::Trend {
                    version: FORECAST_RECIPE_VERSION,
                    bend_days: TrendBendDays::Linear,
                },
                TrendMode::Forecast,
            ),
            &parsed,
            &outcomes,
            &calendar,
            cutoff + 60,
            "fixture".into(),
        )
        .unwrap();

        assert!((view.mean_r.unwrap() - 1.8).abs() < 1e-12);
        assert!((view.forecast_r.unwrap() - 5.4).abs() < 1e-12);
        assert_eq!(view.model.target, "daily_sum_r");
        assert_eq!(view.closed, 5);
    }

    #[test]
    fn probability_groups_use_their_own_q_and_closed_count_weight() {
        let recipe = ForecastRecipe::DecayedProbability {
            version: FORECAST_RECIPE_VERSION,
            half_life_days: 14.0,
            kappa: 10.0,
            prior_policy: PriorPolicy::GrossBreakEven,
        };
        let at = BASE + DAY;
        let first_day = ProbabilityDay {
            day_start_unix: BASE,
            tp_count: 2,
            sl_count: 0,
        };
        let second_day = ProbabilityDay {
            day_start_unix: BASE,
            tp_count: 0,
            sl_count: 1,
        };
        let first = match evaluate_probability(recipe, 2.0, at, &[first_day]).unwrap() {
            setups_domain::forecast::ProbabilityForecast::Available { value } => value.forecast_r,
            other => panic!("unexpected first group: {other:?}"),
        };
        let second = match evaluate_probability(recipe, 1.0, at, &[second_day]).unwrap() {
            setups_domain::forecast::ProbabilityForecast::Available { value } => value.forecast_r,
            other => panic!("unexpected second group: {other:?}"),
        };
        let groups = BTreeMap::from([
            (
                ("2".into(), "1".into()),
                ProbabilityGroup {
                    q: 2.0,
                    days: BTreeMap::from([(BASE, first_day)]),
                },
            ),
            (
                ("1".into(), "1".into()),
                ProbabilityGroup {
                    q: 1.0,
                    days: BTreeMap::from([(BASE, second_day)]),
                },
            ),
        ]);
        let combined = probability_at(&groups, recipe, at).unwrap().0.unwrap();
        assert!((combined - (first * 2.0 + second) / 3.0).abs() < 1e-12);

        let underflow_recipe = ForecastRecipe::DecayedProbability {
            version: FORECAST_RECIPE_VERSION,
            half_life_days: 0.000_001,
            kappa: 10.0,
            prior_policy: PriorPolicy::GrossBreakEven,
        };
        let mixed = BTreeMap::from([
            (
                ("1".into(), "1".into()),
                ProbabilityGroup {
                    q: 1.0,
                    days: BTreeMap::from([(
                        at,
                        ProbabilityDay {
                            day_start_unix: at,
                            tp_count: 1,
                            sl_count: 0,
                        },
                    )]),
                },
            ),
            (
                ("2".into(), "1".into()),
                ProbabilityGroup {
                    q: 2.0,
                    days: BTreeMap::from([(BASE, second_day)]),
                },
            ),
        ]);
        assert_eq!(
            probability_at(&mixed, underflow_recipe, at).unwrap().0,
            None
        );
    }
}
