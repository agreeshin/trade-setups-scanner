//! Request-local, bounded history metadata. Full source validation belongs to POST.
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use setups_db::optimization::StudyPage;
use setups_domain::{SPIKE_ALGORITHM_ID, SPIKE_ALGORITHM_VERSION, SPIKE_DROP_ALGORITHM_ID, SPIKE_DROP_ALGORITHM_VERSION};
use sqlx::{PgPool, Row, postgres::PgRow};
use std::collections::HashMap;

const SUMMARY_KEYS: [&str; 11] = ["studyId", "eligibility", "jobId", "revision", "status", "stage", "observedThroughUnix", "counts", "meanR", "lastSetupAt", "error"];

fn compact(row: &Value) -> Value {
    // Share the actual GET producer, then positively allowlist table fields.
    // No report/source/archive can escape even if the detail DTO grows later.
    let detail = crate::study_actual::dto(row);
    Value::Object(SUMMARY_KEYS.into_iter().map(|key| (key.to_owned(), detail[key].clone())).collect())
}

fn idle(id: i64, reason: Option<&str>) -> Value {
    compact(&json!({"study_id":id,"status":"not_calculated","eligibility_reason":reason}))
}

fn eligibility(row: &PgRow, owner: &str) -> Result<Option<&'static str>, sqlx::Error> {
    if row.try_get::<String, _>("owner_id")? != owner { return Ok(Some("study_not_found")); }
    if !matches!(row.try_get::<String, _>("status")?.as_str(), "completed" | "cancelled" | "failed") {
        return Ok(Some("study_active"));
    }
    if !row.try_get::<bool, _>("has_winner")? { return Ok(Some("winner_missing")); }
    let algorithm: String = row.try_get("algorithm_id")?;
    let version: Option<String> = row.try_get("algorithm_version")?;
    let supported = match algorithm.as_str() {
        SPIKE_ALGORITHM_ID => version.as_deref() == Some(SPIKE_ALGORITHM_VERSION),
        SPIKE_DROP_ALGORITHM_ID => version.as_deref() == Some(SPIKE_DROP_ALGORITHM_VERSION),
        _ => false,
    };
    if !supported || !row.try_get::<bool, _>("algorithm_matches")? { return Ok(Some("algorithm_version_unavailable")); }
    let start: Option<DateTime<Utc>> = row.try_get("execution_start")?;
    let marker: Option<String> = row.try_get("started_marker")?;
    if start.is_none() && !marker.as_deref().is_some_and(|at| DateTime::parse_from_rfc3339(at).is_ok()) {
        return Ok(Some("execution_start_unavailable"));
    }
    if !row.try_get::<bool, _>("has_params")? { return Ok(Some("winning_parameters_unavailable")); }
    if !row.try_get::<bool, _>("has_universe")? { return Ok(Some("study_universe_unavailable")); }
    if !row.try_get::<bool, _>("has_trade_recipe")? { return Ok(Some("trade_conditions_unavailable")); }
    if !row.try_get::<bool, _>("has_closed_cell")? { return Ok(Some("recommended_pair_unavailable")); }
    Ok(None)
}

pub(crate) async fn enrich(pool: &PgPool, owner: &str, page: &mut StudyPage) -> Result<(), sqlx::Error> {
    if page.studies.len() > 100 { return Err(sqlx::Error::Protocol("study summary page exceeds 100 rows".into())); }
    if page.studies.is_empty() { return Ok(()); }
    let ids: Vec<i64> = page.studies.iter().map(|study| study.id).collect();
    // Only scalars/booleans and the compact head cross the SQL boundary. The
    // bounded legacy event scan never transfers events/matrices/frozen models.
    let rows = sqlx::query(&[r#"
        select s.id,s.owner_id,s.status,s.algorithm_id,s.spec->>'algorithmVersion' algorithm_version,
          coalesce(s.spec->>'algorithmId'=s.algorithm_id,false) algorithm_matches,
          coalesce(g.execution_started_at,s.execution_started_at) execution_start,
          (select case when length(e.value->>'at')<=128 then e.value->>'at' end
           from jsonb_array_elements(s.events) with ordinality e(value,position)
           where e.value->>'kind'='started' order by e.position limit 1) started_marker,
          coalesce(jsonb_typeof(s.result->'best')='object',false) has_winner,
          coalesce(jsonb_typeof(s.result#>'{best,params}')='object' and s.result#>'{best,params}'<>'{}'::jsonb,false) has_params,
          coalesce(case when jsonb_typeof(s.spec->'instruments')='array'
            then jsonb_array_length(s.spec->'instruments') between 1 and 1000 else false end,false) has_universe,
          coalesce(jsonb_typeof(s.result#>'{best,analytics}')='object' and s.spec->>'direction' in ('short','long')
            and jsonb_path_exists(s.result,'$.best.analytics.horizonMinutes ? (@ > 0 && @ <= 525600)'),false) has_trade_recipe,
          coalesce(jsonb_typeof(s.result#>'{best,matrix}')='array' and jsonb_path_exists(s.result,
            '$.best.matrix[*] ? (@.closedCount > 0 && @.tpPct > 0 && @.tpPct <= 100 && @.slPct > 0 && @.slPct <= 100 && @.riskExpectancy.type() == "number")'),false) has_closed_cell,
          case when a.study_id is not null then jsonb_build_object(
            'study_id',a.study_id,'job_id',a.job_id,'revision',a.revision,'status',a.status,'stage',a.stage,
            'observed_through_unix',a.observed_through_unix,'totals',a.totals,'error',a.error,'last_setup_close_unix',"#,
        setups_db::study_actual::LAST_SETUP_CLOSE_UNIX_SQL,
        r#") end head
        from optimization_studies s
        left join continuous_generations g on g.id=s.continuous_generation_id and g.owner_id=s.owner_id
        left join study_actualizations a on a.study_id=s.id and a.owner_id=s.owner_id and s.owner_id=$2
        where s.id=any($1)
    "#].concat()).bind(&ids).bind(owner).fetch_all(pool).await?;
    let mut summaries = HashMap::with_capacity(rows.len());
    for row in rows {
        let id: i64 = row.try_get("id")?;
        let head: Option<Value> = row.try_get("head")?;
        let summary = match head {
            Some(head) => compact(&head),
            None => idle(id, eligibility(&row, owner)?),
        };
        summaries.insert(id, summary);
    }
    for study in &mut page.studies {
        // A concurrent delete after the original page snapshot is an ordinary
        // unavailable row, not null metadata or a silently enabled refresh.
        study.actual = Some(summaries.remove(&study.id).unwrap_or_else(|| idle(study.id, Some("study_not_found"))));
    }
    Ok(())
}
