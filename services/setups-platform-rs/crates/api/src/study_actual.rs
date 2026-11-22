//! Durable manual/automatic post-start diagnostics. No live outcome or replay cache.
pub mod display;
pub mod legacy;
pub mod legacy_cli;
pub mod legacy_runtime;
pub(crate) mod model;
mod runtime;
use crate::replay::{ReplayState, guard_auth};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
pub use runtime::{ActualHandle, start_actual_runtime};
use serde::Deserialize;
use serde_json::{Value, json};
use setups_db::study_actual as db;
use std::{collections::HashMap, sync::Arc};

fn reply(status: StatusCode, value: Value) -> Response {
    (status, [("cache-control", "no-store")], Json(value)).into_response()
}
fn error(status: StatusCode, code: &str) -> Response {
    reply(status, json!({"error":code}))
}
pub fn study_actual_router(state: Arc<ReplayState>) -> Router {
    Router::new()
        .route("/v1/optimization-studies/{id}/actual", get(detail))
        .route(
            "/v1/optimization-studies/actual/continuous",
            get(continuous_get).put(continuous_put),
        )
        .route(
            "/v1/optimization-studies/{id}/actual/refresh",
            post(refresh),
        )
        .route("/v1/optimization-studies/{id}/actual/setups", get(setups))
        .layer(DefaultBodyLimit::max(256))
        .with_state(state)
}

pub fn dto(row: &Value) -> Value {
    let completed = row["status"] == "completed";
    let totals = &row["totals"];
    let extra = json!({"studyId":row["study_id"],"jobId":row["job_id"],"revision":row["revision"],"status":row["status"],"stage":row["stage"],
        "eligibility":{"eligible":true,"reason":null},"sourceIdentity":row["source_identity"],"source":row["sourceFacts"],"executionStartedAt":row["execution_started_at"],"fromUnix":row["execution_started_at"].as_str().and_then(|s|chrono::DateTime::parse_from_rfc3339(s).ok()).map(|s|s.timestamp()),
        "observedThroughUnix":row["observed_through_unix"],"requestedAt":row["requested_at"],"startedAt":row["started_at"],"completedAt":row["completed_at"],
        "replayBatchId":if completed {row["batch_id"].clone()}else{Value::Null},"progress":row["progress"],"counts":if completed{totals.clone()}else{Value::Null},
        "meanR":if completed{totals["meanR"].clone()}else{Value::Null},"error":row["error"],"report":if completed{row["report"].clone()}else{Value::Null}});
    let mut value = extra;
    value
        .as_object_mut()
        .unwrap()
        .extend(db::compact_summary(row).as_object().unwrap().clone());
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower::ServiceExt;
    #[tokio::test]
    async fn every_actual_route_authenticates_before_any_database_or_market_read() {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://unused@127.0.0.1:1/unused_test")
            .unwrap();
        let app = study_actual_router(Arc::new(ReplayState {
            pool,
            auth: Arc::new(crate::SessionAuth::new(&["fixture-key".into()])),
            market_url: "http://127.0.0.1:1".into(),
            market_key: "fixture".into(),
            limits: Default::default(),
        }));
        for (method, url) in [
            ("GET", "/v1/optimization-studies/1/actual"),
            ("POST", "/v1/optimization-studies/1/actual/refresh"),
            ("GET", "/v1/optimization-studies/1/actual/setups?revision=1"),
            (
                "GET",
                "/v1/optimization-studies/actual/continuous?algorithmId=price_volume_spike",
            ),
            (
                "PUT",
                "/v1/optimization-studies/actual/continuous?algorithmId=price_volume_spike",
            ),
        ] {
            let response = app
                .clone()
                .oneshot(
                    axum::http::Request::builder()
                        .method(method)
                        .uri(url)
                        .header("content-type", "application/json")
                        .body(axum::body::Body::from("{}"))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{url}");
        }
    }
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ContinuousQuery {
    algorithm_id: String,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ContinuousPut {
    enabled: bool,
    from_date: String,
    revision: i64,
    time_zone: Option<String>,
}
type ContinuousQueryResult =
    Result<Query<ContinuousQuery>, axum::extract::rejection::QueryRejection>;
async fn continuous_get(
    State(state): State<Arc<ReplayState>>,
    headers: HeaderMap,
    query: ContinuousQueryResult,
) -> Response {
    if let Err(e) = guard_auth(&state.auth, &headers, false) {
        return e;
    }
    let Ok(Query(q)) = query else {
        return error(StatusCode::BAD_REQUEST, "invalid_query");
    };
    if !db::continuous::valid_algorithm(&q.algorithm_id) {
        return error(StatusCode::BAD_REQUEST, "invalid_algorithm_id");
    }
    match db::continuous::state(&state.pool, "admin", &q.algorithm_id).await {
        Ok(v) => reply(StatusCode::OK, v),
        Err(_) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "study_actual_storage_unavailable",
        ),
    }
}
async fn continuous_put(
    State(state): State<Arc<ReplayState>>,
    headers: HeaderMap,
    query: ContinuousQueryResult,
    body: Result<Json<ContinuousPut>, axum::extract::rejection::JsonRejection>,
) -> Response {
    // HTTP principal remains the existing single admin; persistence is owner-aware.
    if let Err(e) = guard_auth(&state.auth, &headers, true) {
        return e;
    }
    let Ok(Query(q)) = query else {
        return error(StatusCode::BAD_REQUEST, "invalid_query");
    };
    let Ok(Json(b)) = body else {
        return error(StatusCode::BAD_REQUEST, "invalid_body");
    };
    if !db::continuous::valid_algorithm(&q.algorithm_id) {
        return error(StatusCode::BAD_REQUEST, "invalid_algorithm_id");
    }
    if b.revision <= 0 {
        return error(StatusCode::BAD_REQUEST, "invalid_revision");
    }
    let zone = match b.time_zone {
        Some(zone) => zone,
        None => match db::continuous::ensure(&state.pool, "admin", &q.algorithm_id).await {
            Ok(s) => s.time_zone,
            Err(_) => {
                return error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "study_actual_storage_unavailable",
                );
            }
        },
    };
    match db::continuous::configure(
        &state.pool,
        "admin",
        &q.algorithm_id,
        b.revision,
        b.enabled,
        &b.from_date,
        &zone,
    )
    .await
    {
        Ok(db::continuous::Configure::Saved(_)) => {
            match db::continuous::state(&state.pool, "admin", &q.algorithm_id).await {
                Ok(v) => reply(StatusCode::OK, v),
                Err(_) => error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "study_actual_storage_unavailable",
                ),
            }
        }
        Ok(db::continuous::Configure::Conflict(current)) => reply(
            StatusCode::CONFLICT,
            json!({"error":"study_actual_configuration_changed","current":current}),
        ),
        Ok(db::continuous::Configure::Invalid(code)) => error(StatusCode::BAD_REQUEST, code),
        Err(_) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "study_actual_storage_unavailable",
        ),
    }
}
async fn owns(state: &ReplayState, id: i64) -> Result<(), Response> {
    match setups_db::optimization::get(&state.pool, id).await {
        Ok(Some(s)) if s.owner_id == "admin" => Ok(()),
        Ok(_) => Err(error(StatusCode::NOT_FOUND, "study_not_found")),
        Err(_) => Err(error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "study_actual_storage_unavailable",
        )),
    }
}
async fn detail(
    State(state): State<Arc<ReplayState>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    if let Err(e) = guard_auth(&state.auth, &headers, false) {
        return e;
    }
    if let Err(e) = owns(&state, id).await {
        return e;
    }
    if let Some(zone) = query.get("timeZone") {
        return match display::detail(&state.pool, "admin", id, zone).await {
            Ok(value) => reply(StatusCode::OK, value),
            Err("invalid_time_zone") => error(StatusCode::BAD_REQUEST, "invalid_time_zone"),
            Err(reason) => error(StatusCode::INTERNAL_SERVER_ERROR, reason),
        };
    }
    match db::get(&state.pool, "admin", id).await {
        Ok(Some(row)) => reply(StatusCode::OK, dto(&row)),
        Ok(None) => {
            let eligibility = match model::source(
                &state.pool,
                id,
                "admin",
                chrono::Utc::now().timestamp().div_euclid(60) * 60,
            )
            .await
            {
                Ok(_) => json!({"eligible":true,"reason":null}),
                Err(reason) => json!({"eligible":false,"reason":reason}),
            };
            reply(
                StatusCode::OK,
                json!({"studyId":id,"eligibility":eligibility,"jobId":null,"revision":0,"status":"not_calculated","stage":null,"executionStartedAt":null,"fromUnix":null,"observedThroughUnix":null,"requestedAt":null,"startedAt":null,"completedAt":null,"replayBatchId":null,"progress":{},"counts":null,"meanR":null,"lastSetupAt":null,"error":null,"report":null}),
            )
        }
        Err(_) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "study_actual_storage_unavailable",
        ),
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Empty {}
async fn refresh(
    State(state): State<Arc<ReplayState>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    body: Result<Json<Empty>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(e) = guard_auth(&state.auth, &headers, true) {
        return e;
    }
    if let Err(e) = owns(&state, id).await {
        return e;
    }
    if body.is_err() {
        return error(StatusCode::BAD_REQUEST, "invalid_body");
    }
    // Read-only busy reconciliation before potentially expensive provenance loading.
    if let Ok(Some(row)) = db::get(&state.pool, "admin", id).await {
        if matches!(row["status"].as_str(), Some("queued" | "running")) {
            return reply(
                StatusCode::CONFLICT,
                json!({"error":"study_actual_busy","actual":dto(&row)}),
            );
        }
    }
    let cutoff = chrono::Utc::now().timestamp().div_euclid(60) * 60;
    let source = match model::source(&state.pool, id, "admin", cutoff).await {
        Ok(v) => v,
        Err(reason) => {
            return reply(
                StatusCode::UNPROCESSABLE_ENTITY,
                json!({"error":"study_actual_ineligible","reason":reason}),
            );
        }
    };
    let job = format!("{:032x}", rand::random::<u128>());
    let value = json!(source);
    match db::admit(
        &state.pool,
        db::Request {
            owner: "admin",
            study: id,
            job: &job,
            start: source.execution_started_at,
            cutoff,
            source: &value,
            identity: &source.source_identity,
            work: source.work_minutes(cutoff),
            limits: &state.limits,
        },
    )
    .await
    {
        Ok(db::Admission::Created(row)) => reply(StatusCode::ACCEPTED, dto(&row)),
        Ok(db::Admission::Busy(row)) => reply(
            StatusCode::CONFLICT,
            json!({"error":"study_actual_busy","actual":dto(&row)}),
        ),
        Ok(db::Admission::Resource) => error(StatusCode::TOO_MANY_REQUESTS, "resource_limit"),
        Ok(db::Admission::SourceChanged) => {
            error(StatusCode::CONFLICT, "study_actual_source_changed")
        }
        Ok(db::Admission::Deferred) => {
            error(StatusCode::CONFLICT, "study_actual_admission_changed")
        }
        Err(e) => {
            eprintln!(
                "study_actual outcome=error stage=admission error_class=storage study_id={id} cause={e}"
            );
            error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "study_actual_storage_unavailable",
            )
        }
    }
}
async fn setups(
    State(state): State<Arc<ReplayState>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    if let Err(e) = guard_auth(&state.auth, &headers, false) {
        return e;
    }
    if let Err(e) = owns(&state, id).await {
        return e;
    }
    let parse =
        |key: &str, default: i64| q.get(key).map(|s| s.parse::<i64>()).unwrap_or(Ok(default));
    let (Ok(revision), Ok(offset), Ok(limit)) =
        (parse("revision", 0), parse("offset", 0), parse("limit", 50))
    else {
        return error(StatusCode::BAD_REQUEST, "invalid_query");
    };
    if revision <= 0 || offset < 0 || !(1..=100).contains(&limit) {
        return error(StatusCode::BAD_REQUEST, "invalid_query");
    }
    // Page metadata and rows share a repeatable snapshot inside DB; total comes from the same revision.
    match db::page(&state.pool, "admin", id, revision, offset, limit).await {
        Ok(Some(rows)) => {
            let head = match db::get(&state.pool, "admin", id).await {
                Ok(Some(v)) if v["revision"] == revision && v["status"] == "completed" => v,
                _ => return error(StatusCode::CONFLICT, "study_actual_revision_changed"),
            };
            reply(
                StatusCode::OK,
                json!({"studyId":id,"jobId":head["job_id"],"revision":revision,"retrospective":true,"total":head["totals"]["total"],"limit":limit,"offset":offset,"setups":rows}),
            )
        }
        Ok(None) => error(StatusCode::CONFLICT, "study_actual_revision_changed"),
        Err(_) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "study_actual_storage_unavailable",
        ),
    }
}
