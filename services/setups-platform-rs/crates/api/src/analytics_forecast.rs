//! Pure probability graph adapter. Counts and buffers belong to one request;
//! no market reads, cache, clock-based refit or retained per-study collection.
use super::*;
use setups_domain::forecast::{
    ForecastError, ForecastRecipe, MAX_FORECAST_DAYS, ProbabilityCurveRequest, ProbabilityDay,
    decision_r, probability_curve,
};
use setups_domain::trend::TrendBlendLambda;

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct ForecastRequest {
    pub forecast: ForecastRecipe,
    #[serde(default)]
    pub trend_mode: TrendMode,
    #[serde(default)]
    pub trend_blend_lambda: TrendBlendLambda,
    #[serde(deserialize_with = "deserialize_payoff")]
    pub q: f64,
    pub from_unix: i64,
    pub to_unix: i64,
    pub observation_cutoff: i64,
    pub at_unix: Option<i64>,
    pub daily: Vec<ProbabilityDay>,
}

fn deserialize_payoff<'de, D: serde::Deserializer<'de>>(decoder: D) -> Result<f64, D::Error> {
    use serde::Deserialize;
    serde_json::Number::deserialize(decoder)?
        .as_f64()
        .ok_or_else(|| serde::de::Error::custom("non-finite forecast payoff"))
}

pub(super) fn validate_period(from: i64, to: i64, cutoff: i64) -> Result<(), String> {
    let duration = to.checked_sub(from).ok_or("forecast: invalid period")?;
    if duration <= 0
        || duration > 3660 * 86_400
        || cutoff < from
        || cutoff > chrono::Utc::now().timestamp()
        || [from, to, cutoff]
            .iter()
            .any(|&at| chrono::DateTime::from_timestamp(at, 0).is_none())
    {
        return Err("forecast: invalid period/cutoff".into());
    }
    Ok(())
}

pub(super) fn compute(request: ForecastRequest) -> Result<Value, String> {
    validate_period(
        request.from_unix,
        request.to_unix,
        request.observation_cutoff,
    )?;
    if request.daily.len() > MAX_FORECAST_DAYS {
        return Err("forecast: TooManyDays".into());
    }
    let at = request.at_unix.unwrap_or(request.observation_cutoff);
    if at < request.observation_cutoff || chrono::DateTime::from_timestamp(at, 0).is_none() {
        return Err("forecast: BeforeCutoff".into());
    }
    for day in &request.daily {
        let end = day
            .day_start_unix
            .checked_add(86_400)
            .ok_or("forecast: invalid day")?;
        if day.day_start_unix >= request.to_unix || end <= request.from_unix {
            return Err("forecast: day outside signal period".into());
        }
    }
    let model = probability_curve(&ProbabilityCurveRequest {
        recipe: request.forecast,
        q: request.q,
        observation_cutoff: request.observation_cutoff,
        daily: request.daily.clone(),
    })
    .map_err(|error| error.to_string())?;
    let estimate = model.estimate;
    let forecast_r = estimate.at(at).map_err(|error| error.to_string())?;
    // Period mean remains meaningful if extreme valid decay underflows all W.
    // This is count-summary arithmetic, not a separate forecast model.
    let (tp, sl) = request
        .daily
        .iter()
        .try_fold((0_u64, 0_u64), |(tp, sl), day| {
            Some((tp.checked_add(day.tp_count)?, sl.checked_add(day.sl_count)?))
        })
        .ok_or_else(|| ForecastError::CountOverflow.to_string())?;
    let n = tp
        .checked_add(sl)
        .ok_or_else(|| ForecastError::CountOverflow.to_string())?;
    let mean_r = estimate
        .as_ref()
        .map(|value| value.mean_r)
        .or_else(|| (n > 0).then(|| request.q * (tp as f64 / n as f64) - sl as f64 / n as f64));
    if mean_r.is_some_and(|mean| !mean.is_finite()) {
        return Err(ForecastError::NonFiniteNumber.to_string());
    }
    let decision = decision_r(
        request.trend_mode,
        mean_r,
        forecast_r,
        request.trend_blend_lambda,
    )
    .map_err(|error| error.to_string())?;
    let blend_curve = if request.trend_mode == TrendMode::Blend {
        Some(
            model
                .curve
                .iter()
                .map(|point| {
                    decision_r(
                        request.trend_mode,
                        mean_r,
                        point.forecast_r,
                        request.trend_blend_lambda,
                    )
                    .map(|decision| json!({"atUnix": point.at_unix, "decisionR": decision}))
                })
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| error.to_string())?,
        )
    } else {
        None
    };
    let mut response = json!({
        "forecast": request.forecast, "trendMode": request.trend_mode, "q": request.q,
        "fromUnix": request.from_unix, "toUnix": request.to_unix,
        "observationCutoff": request.observation_cutoff, "retrospective": true,
        "estimate": estimate, "meanR": mean_r, "forecastR": forecast_r, "decisionR": decision,
        "curve": model.curve,
        "flatFuture": {"fromUnix": request.observation_cutoff, "toUnix": at,
            "forecastR": forecast_r, "decisionR": decision},
        "provenance": {"kind": "supplied_closed_outcomes", "availability": "asserted_at_cutoff"}
    });
    if let Some(curve) = blend_curve {
        response["blendCurve"] = json!(curve);
        response["trendBlendLambda"] = json!(request.trend_blend_lambda);
    }
    Ok(response)
}

pub(super) async fn handle(
    State(state): State<Arc<AnalyticsState>>,
    headers: HeaderMap,
    Json(request): Json<ForecastRequest>,
) -> Response {
    if let Err(response) = guard_auth(&state.auth, &headers, true) {
        return response;
    }
    // Reject an oversized vector before submitting bounded request-owned CPU work.
    if request.daily.len() > MAX_FORECAST_DAYS {
        return error_response(StatusCode::BAD_REQUEST, "forecast: TooManyDays");
    }
    let Ok(permit) = state.forecast_cpu.clone().try_acquire_owned() else {
        return error_response(StatusCode::TOO_MANY_REQUESTS, "forecast_busy");
    };
    match tokio::task::spawn_blocking(move || {
        let _permit = permit;
        compute(request)
    })
    .await
    {
        Ok(Ok(value)) => Json(value).into_response(),
        Ok(Err(error)) => error_response(StatusCode::BAD_REQUEST, &error),
        Err(_) => internal_error(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn request() -> ForecastRequest {
        serde_json::from_value(json!({
            "forecast": ForecastRecipe::engineering_baseline(), "trendMode": "blend",
            "trendBlendLambda": 0.7, "q": 2.5, "fromUnix": 0, "toUnix": 172800,
            "observationCutoff": 172800, "atUnix": 173100,
            "daily": [{"dayStartUnix": 0, "tpCount": 4, "slCount": 0},
                {"dayStartUnix": 86400, "tpCount": 0, "slCount": 2}]
        }))
        .unwrap()
    }
    #[test]
    fn graph_uses_core_curve_scalar_counts_and_frozen_future() {
        let req = request();
        let core = probability_curve(&ProbabilityCurveRequest {
            recipe: req.forecast,
            q: req.q,
            observation_cutoff: req.observation_cutoff,
            daily: req.daily.clone(),
        })
        .unwrap();
        let actual = compute(req).unwrap();
        assert_eq!(actual["estimate"], json!(core.estimate));
        assert_eq!(actual["curve"], json!(core.curve));
        assert_eq!(actual["forecastR"], actual["flatFuture"]["forecastR"]);
        assert_eq!(
            actual["forecastR"],
            actual["curve"].as_array().unwrap().last().unwrap()["forecastR"]
        );
        assert_eq!(actual["estimate"]["value"]["tpCount"], 4);
        assert_eq!(actual["estimate"]["value"]["slCount"], 2);
        assert_eq!(actual["meanR"], 8.0 / 6.0);
        assert_ne!(actual["decisionR"], actual["forecastR"]);
        assert_eq!(actual["blendCurve"].as_array().unwrap().len(), 2);
        assert_eq!(actual["retrospective"], true);
    }
    #[test]
    fn graph_rejects_bad_period_future_cohorts_and_unknown_contract_fields() {
        let mut req = request();
        req.at_unix = Some(1);
        assert!(compute(req).is_err());
        let mut req = request();
        req.to_unix = req.from_unix;
        assert!(compute(req).is_err());
        let mut req = request();
        req.daily[0].day_start_unix = -86400;
        assert!(compute(req).is_err());
        let mut req = request();
        req.daily = vec![
            ProbabilityDay {
                day_start_unix: 0,
                tp_count: 1,
                sl_count: 0
            };
            MAX_FORECAST_DAYS + 1
        ];
        assert!(compute(req).is_err());
        assert!(serde_json::from_value::<ForecastRequest>(json!({"points": []})).is_err());
        let mut value = json!({"forecast": ForecastRecipe::engineering_baseline(), "q": 1,
            "fromUnix": 0, "toUnix": 86400, "observationCutoff": 86400, "daily": [], "points": []});
        assert!(serde_json::from_value::<ForecastRequest>(value.clone()).is_err());
        value.as_object_mut().unwrap().remove("points");
        value["trendBlendLambda"] = json!(0);
        assert!(serde_json::from_value::<ForecastRequest>(value).is_err());
    }
    #[test]
    fn empty_graph_is_unavailable_not_prior_only_zero() {
        let mut req = request();
        req.daily.clear();
        let actual = compute(req).unwrap();
        assert_eq!(actual["estimate"]["reason"], "no_closed_outcomes");
        for key in ["meanR", "forecastR", "decisionR"] {
            assert!(actual[key].is_null());
        }
    }

    #[test]
    fn graph_json_q_preserves_rust_payoff_and_domain_curve_bits() {
        for (tp, sl) in [
            (2.0_f64, 3.0),
            (20.0, 8.0),
            (8.0, 6.0),
            (14.0, 3.0),
            (1.0000000000000002, 1.0),
            (0.8455124082255701, 1.0),
        ]
        .into_iter()
        .chain((1..=40).flat_map(|tp| (1..=40).map(move |sl| (tp as f64 / 10.0, sl as f64 / 10.0))))
        {
            let q = tp / sl;
            let req = request();
            let expected = probability_curve(&ProbabilityCurveRequest {
                recipe: req.forecast,
                q,
                observation_cutoff: req.observation_cutoff,
                daily: req.daily.clone(),
            })
            .unwrap();
            let body = serde_json::to_string(&json!({"forecast": req.forecast, "q": q,
                "trendMode": "forecast", "fromUnix": req.from_unix, "toUnix": req.to_unix,
                "observationCutoff": req.observation_cutoff, "daily": req.daily}))
            .unwrap();
            let decoded: ForecastRequest = serde_json::from_str(&body).unwrap();
            assert_eq!(decoded.q.to_bits(), q.to_bits(), "payoff {tp}/{sl}");
            let response = compute(decoded).unwrap();
            assert_eq!(
                response["forecastR"].as_f64().unwrap().to_bits(),
                expected.estimate.as_ref().unwrap().forecast_r.to_bits(),
                "forecast {tp}/{sl}"
            );
            assert_eq!(response["curve"], json!(expected.curve));
        }
    }

    #[test]
    fn underflow_keeps_period_mean_without_claiming_predictive_evidence() {
        let mut req = request();
        req.forecast = ForecastRecipe::DecayedProbability {
            version: 1,
            half_life_days: f64::MIN_POSITIVE,
            kappa: 10.0,
            prior_policy: setups_domain::forecast::PriorPolicy::GrossBreakEven,
        };
        let result = compute(req).unwrap();
        assert_eq!(result["estimate"]["reason"], "no_weighted_evidence");
        assert_eq!(result["meanR"], 8.0 / 6.0);
        assert!(result["forecastR"].is_null());
        assert!(result["decisionR"].is_null());
        assert_eq!(result["provenance"]["availability"], "asserted_at_cutoff");
        assert_eq!(result["retrospective"], true);
    }
}
