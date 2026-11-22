use super::*;
use serde::Serialize;
use setups_db::optimization::ForecastBestPair;

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct BestCell {
    tp_pct: f64,
    sl_pct: f64,
    raw_r: f64,
}

pub(super) fn best_cell(value: &Value) -> Option<BestCell> {
    let cell = BestCell {
        tp_pct: value.get("tpPct")?.as_f64()?,
        sl_pct: value.get("slPct")?.as_f64()?,
        raw_r: value.get("rawR")?.as_f64()?,
    };
    (cell.tp_pct.is_finite()
        && cell.tp_pct > 0.0
        && cell.tp_pct <= 100.0
        && cell.sl_pct.is_finite()
        && cell.sl_pct > 0.0
        && cell.sl_pct <= 100.0
        && cell.raw_r.is_finite())
    .then_some(cell)
}

pub(super) struct StudyMetrics {
    pub status: String,
    pub best_score: Option<f64>,
    pub forecast_best: Option<ForecastBestPair>,
}

pub(super) fn compact(
    study_id: i64,
    external_name: String,
    setup_count: i64,
    generation_id: Option<i64>,
    artifact_id: Option<i64>,
    best_cell: Option<BestCell>,
    metric: Option<&StudyMetrics>,
) -> Value {
    let mut value = json!({
        "studyId": study_id,
        "externalName": external_name,
        "bestScore": metric.and_then(|value| value.best_score),
        "forecastBest": metric.and_then(|value| value.forecast_best),
        "bestCell": best_cell,
        "setupCount": setup_count,
        "generationId": generation_id,
        "artifactId": artifact_id,
    });
    // Missing owned metadata keeps the legacy optional status absent, not null
    // or a completion inferred from recommendations or numerical metrics.
    if let Some(metric) = metric {
        value["status"] = json!(metric.status);
    }
    value
}

pub(super) fn response(items: Vec<Value>) -> Response {
    Json(json!({"studies": items})).into_response()
}

pub(super) async fn load(
    pool: &sqlx::PgPool,
    ids: &[i64],
) -> Result<HashMap<i64, StudyMetrics>, Response> {
    if ids.len() > MAX_STUDY_OPTIONS {
        return Err(read_error());
    }
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = sqlx::query("select s.id,s.status,s.result#>'{best,score}' best_score,coalesce(s.result#>'{best,trendMode}',s.spec->'trendMode') forecast_mode,winner.cell from optimization_studies s left join lateral (
        select jsonb_build_object('tpPct',entry.cell->'tpPct','slPct',entry.cell->'slPct','meanRiskExpectancy',entry.cell->'meanRiskExpectancy','forecastRiskExpectancy',metric.forecast) cell
        from jsonb_array_elements(case when jsonb_typeof(s.result#>'{best,matrix}')='array' then s.result#>'{best,matrix}' else '[]'::jsonb end) with ordinality entry(cell,position)
        cross join lateral (select case when entry.cell ? 'forecastRiskExpectancy' then entry.cell->'forecastRiskExpectancy'
            when coalesce(s.result#>'{best,trendMode}',s.spec->'trendMode')='\"forecast\"'::jsonb then entry.cell->'riskExpectancy' end forecast) metric
        where case when jsonb_typeof(metric.forecast)='number' then abs((metric.forecast#>>'{}')::numeric)<=1.7976931348623157e308 else false end
            and case when jsonb_typeof(entry.cell->'meanRiskExpectancy')='number' then abs((entry.cell->>'meanRiskExpectancy')::numeric)<=1.7976931348623157e308 else false end
            and case when jsonb_typeof(entry.cell->'tpPct')='number' then (entry.cell->>'tpPct')::numeric>0 and (entry.cell->>'tpPct')::numeric<=100 else false end
            and case when jsonb_typeof(entry.cell->'slPct')='number' then (entry.cell->>'slPct')::numeric>0 and (entry.cell->>'slPct')::numeric<=100 else false end
        order by (metric.forecast#>>'{}')::numeric desc,entry.position limit 1) winner on true
        where s.owner_id='admin' and s.id=any($1)")
        .bind(ids).fetch_all(pool).await.map_err(|_| read_error())?;
    rows.iter()
        .map(|row| {
            let score: Option<Value> = row.try_get("best_score").map_err(|_| read_error())?;
            let mode: Option<Value> = row.try_get("forecast_mode").map_err(|_| read_error())?;
            let cell: Option<Value> = row.try_get("cell").map_err(|_| read_error())?;
            Ok((
                row.try_get("id").map_err(|_| read_error())?,
                StudyMetrics {
                    status: row.try_get("status").map_err(|_| read_error())?,
                    best_score: score
                        .as_ref()
                        .and_then(Value::as_f64)
                        .filter(|score| score.is_finite()),
                    forecast_best: cell.and_then(|cell| {
                        setups_db::optimization::forecast_best_pair(
                            Some(&json!({"best":{"matrix":[cell]}})),
                            &json!({"trendMode":mode}),
                        )
                    }),
                },
            ))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn compact_studies_response_projects_persisted_status_without_inference() {
        for persisted_status in [
            "queued",
            "running",
            "cancel_requested",
            "completed",
            "cancelled",
            "failed",
            "future_state",
        ] {
            let metric = StudyMetrics {
                status: persisted_status.into(),
                best_score: None,
                forecast_best: None,
            };
            let response = response(vec![compact(
                40,
                "Saved study".into(),
                23,
                Some(36),
                Some(7),
                None,
                Some(&metric),
            )]);
            assert_eq!(response.status(), StatusCode::OK);
            let bytes = axum::body::to_bytes(response.into_body(), 4096)
                .await
                .unwrap();
            let body: Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(
                body,
                json!({"studies":[{
                    "studyId":40,"externalName":"Saved study","setupCount":23,"generationId":36,"artifactId":7,
                    "bestScore":null,"forecastBest":null,"bestCell":null,"status":persisted_status,
                }]})
            );
        }
    }

    #[tokio::test]
    async fn compact_studies_response_omits_unknown_owned_study_status_for_legacy_compatibility() {
        let response = response(vec![compact(
            40,
            "Legacy study".into(),
            2,
            None,
            None,
            None,
            None,
        )]);
        let bytes = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            body,
            json!({"studies":[{
                "studyId":40,"externalName":"Legacy study","setupCount":2,"generationId":null,"artifactId":null,
                "bestScore":null,"forecastBest":null,"bestCell":null,
            }]})
        );
        assert!(
            body["studies"][0].get("status").is_none(),
            "web accepts an absent optional status, not null or an invented completion"
        );
    }

    #[test]
    fn selected_cell_is_original_raw_pair_not_robust_or_forecast_maximum() {
        let value = json!({"tpPct":14,"slPct":4,"rawR":-0.3,"robustScore":0.9,"forecastR":1.2});
        assert_eq!(
            json!(best_cell(&value)),
            json!({"tpPct":14.0,"slPct":4.0,"rawR":-0.3})
        );
        for value in [
            json!({}),
            json!({"tpPct":0,"slPct":4,"rawR":1}),
            json!({"tpPct":14,"slPct":4,"meanR":1}),
            json!({"tpPct":14,"slPct":4,"rawR":null}),
        ] {
            assert_eq!(best_cell(&value), None);
        }
    }
}
