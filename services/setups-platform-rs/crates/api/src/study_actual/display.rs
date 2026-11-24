//! Read-only presentation of persisted outcomes and diagnostic model projections.
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use setups_db::study_display::{self as db, DisplayDay};
use sqlx::PgPool;
use super::model::{self, Source};
use super::projection::{self, PublishedActual};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DisplaySource {
    pub availability: DisplayAvailability,
    pub fidelity: String,
    pub source_identity: Option<String>,
    pub complete: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub raw_replay_available: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag="status",rename_all="snake_case")]
pub enum DisplayAvailability { Available, Unavailable { reason:String } }
impl DisplaySource {
    fn unavailable(reason: &str, raw: bool) -> Self {
        Self { availability: DisplayAvailability::Unavailable{reason:reason.into()}, fidelity: "unavailable".into(),
            source_identity: None, complete: false, reason: Some(reason.into()), raw_replay_available: raw }
    }
    fn available(fidelity: &str, identity: String, raw: bool) -> Self {
        Self { availability: DisplayAvailability::Available, fidelity: fidelity.into(), source_identity: Some(identity),
            complete: true, reason: None, raw_replay_available: raw }
    }
}

/// Additive report.display and originalReport.display wire contract for ?timeZone.
/// dayUnix/startDayUnix are UTC instants of local midnight; exact S stays in report.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StudyDisplay {
    pub time_zone: String,
    pub start_day_unix: i64,
    pub daily: Vec<DisplayDay>,
    pub historical: DisplaySource,
    pub actual: DisplaySource,
}

pub async fn detail(pool: &PgPool, owner: &str, study: i64, zone: &str) -> Result<Value, &'static str> {
    let source = model::original_source(pool, study, owner).await;
    let (start, artifact_id, identity) = match &source {
        Ok(s) => (Some(s.execution_started_at), s.source_artifact_id, Some(s.source_identity.as_str())),
        Err(_) => (None, None, None),
    };
    let snapshot = db::display_snapshot(pool, owner, study, zone, start, artifact_id, identity).await
        .map_err(|e| {
            if e.to_string().contains("invalid_time_zone") {return "invalid_time_zone";}
            let message:String=e.to_string().chars().take(512).map(|c|if c.is_control(){' '}else{c}).collect();
            setups_db::diagnostics::emit(setups_db::diagnostics::Level::Error,"study_display_read_failed",
                json!({"studyId":study,"code":"study_display_storage_unavailable","message":message}));
            "study_display_storage_unavailable"
        })?;
    let eligibility=match source.as_ref() {Ok(s)=>s.refresh_eligibility(chrono::Utc::now().timestamp()),Err(reason)=>Err(*reason)};
    let mut value = match snapshot.actual_row.as_ref() {
        Some(row) => super::dto(row),
        None => json!({"studyId":study,"eligibility":{"eligible":eligibility.is_ok(),"reason":eligibility.err()},
            "jobId":null,"revision":0,"status":"not_calculated","stage":null,"executionStartedAt":null,"fromUnix":null,
            "observedThroughUnix":null,"requestedAt":null,"startedAt":null,"completedAt":null,"replayBatchId":null,
            "progress":{},"counts":null,"meanR":null,"lastSetupAt":null,"error":null,"report":null}),
    };
    let Some(start_day_unix) = snapshot.start_day_unix else {
        value["originalReport"] = Value::Null;
        value["originalReportUnavailableReason"] = json!(source.err().unwrap_or("execution_start_unavailable"));
        return Ok(value);
    };
    let historical = match snapshot.original_identity {
        Some(id) => DisplaySource::available(snapshot.original_fidelity.as_deref().ok_or("study_display_invalid_outcomes")?, id, snapshot.raw_replay_available),
        None => DisplaySource::unavailable("original_outcomes_not_persisted", snapshot.raw_replay_available),
    };
    let original_display = StudyDisplay { time_zone: zone.into(), start_day_unix, daily: snapshot.original_daily.clone(),
        historical: historical.clone(), actual: DisplaySource::unavailable("post_study_not_included", false) };
    value["originalReport"] = match &source {
        Ok(source) => {
            let mut report=original_report(source, &original_display);
            // Independent original report is stable through every actual state.
            // The actual envelope owns queued/running/failed availability reasons.
            report["combinedProjection"]=json!(projection::CombinedProjection::unavailable("post_study_not_included"));
            report
        },
        Err(reason) => { value["originalReportUnavailableReason"] = json!(reason); Value::Null },
    };
    if value["status"] == "completed" && value["report"].is_object() {
        let metadata=source.as_ref().ok().map(|source|combined_metadata(source,snapshot.actual_row.as_ref(),&value));
        // Objective incompatibility prevents refitting but does not change the
        // factual TP/SL outcomes. Pair/source/revision incompatibility does:
        // never merge observed rows produced under different trade conditions.
        let observed_error=metadata.as_ref().and_then(|result|result.as_ref().err().copied())
            .filter(|reason|!matches!(*reason,"post_study_objective_unavailable"|"post_study_objective_mismatch"));
        let actual = if let Some(reason)=observed_error {DisplaySource::unavailable(reason,false)} else {
            DisplaySource::available("persisted_post_study",format!("{}:{}:{}",study,value["revision"],value["jobId"]),true)
        };
        let combined=match (&source,metadata) {
            (Ok(source),Some(metadata))=>{
                let artifact=source.artifact.clone();let objective=source.objective;
                let daily=snapshot.actual_utc_daily;
                // Numerical work owns bounded inputs off the async I/O executor.
                match tokio::task::spawn_blocking(move||projection::project(artifact.as_ref(),objective,
                    metadata.as_ref().map(|(cutoff,revision,job_id)|PublishedActual{daily:&daily,cutoff:*cutoff,revision:*revision,job_id}).map_err(|reason|*reason))).await {
                    Ok(model)=>model,
                    Err(_)=>{
                        setups_db::diagnostics::emit(setups_db::diagnostics::Level::Error,"study_combined_projection_failed",json!({"studyId":study,"code":"combined_evaluation_failed"}));
                        projection::CombinedProjection::unavailable("combined_evaluation_failed")
                    },
                }
            },
            (Err(reason),_)=>projection::CombinedProjection::unavailable(reason),
            _=>unreachable!("source and metadata availability must agree"),
        };
        value["report"]["combinedProjection"]=json!(combined);
        let display = StudyDisplay { time_zone: zone.into(), start_day_unix,
            daily: if observed_error.is_some(){snapshot.original_daily}else{snapshot.combined_daily},
            historical, actual };
        value["report"]["display"] = serde_json::to_value(display).map_err(|_|"study_display_invalid_outcomes")?;
    }
    Ok(value)
}

fn combined_metadata(source: &Source, row: Option<&Value>, summary: &Value) -> Result<(i64, i64, String), &'static str> {
    // The row and UTC cohorts share display_snapshot's REPEATABLE READ. Derive
    // labels here, not from any separately read/assembled outer summary.
    let row = row.ok_or("post_study_identity_unavailable")?;
    let (Some(cutoff), Some(revision), Some(job)) = (row["observed_through_unix"].as_i64(), row["revision"].as_i64(), row["job_id"].as_str()) else {
        return Err("post_study_identity_unavailable");
    };
    if row["status"] != "completed" || summary["status"] != row["status"]
        || summary["observedThroughUnix"] != cutoff || summary["revision"] != revision || summary["jobId"] != job {
        return Err("post_study_revision_changed");
    }
    if row["source_identity"].as_str() != Some(&source.source_identity) { return Err("post_study_stale"); }
    if source.artifact.as_ref().is_some_and(|a| a.source.tp_pct != source.tp_pct || a.source.sl_pct != source.sl_pct) {
        return Err("original_pair_mismatch");
    }
    // source_identity hashes spec/result, NOT the admitted derived Source. An
    // older selector can choose a different pair under the same study hash.
    // Equal payoff ratios do not establish equal trade outcomes.
    let saved = &row["sourceFacts"];
    if saved["tpPct"].as_f64() != Some(source.tp_pct) || saved["slPct"].as_f64() != Some(source.sl_pct) {
        return Err("post_study_pair_mismatch");
    }
    let objective: setups_domain::expectancy::ExpectancyObjective = serde_json::from_value(saved["objective"].clone())
        .map_err(|_| "post_study_objective_unavailable")?;
    if objective != source.objective { return Err("post_study_objective_mismatch"); }
    Ok((cutoff, revision, job.to_owned()))
}

fn original_report(source: &Source, display: &StudyDisplay) -> Value {
    // Both bounds are sealed, never taken from actual. Explicit forecast sources
    // may observe exits after their signal-selection to_unix. Their sealed curve
    // already ends at C, and objective_series_through rejects a bound before C;
    // max keeps that curve available rather than extending/refitting its points.
    let through = source.artifact.as_ref().map(|a| a.to_unix.max(a.source.provenance.observation_cutoff))
        .or_else(|| source.original_endpoint["atUnix"].as_i64()).unwrap_or(source.execution_started_at.timestamp());
    let mut report = model::chart(source, through, vec![]);
    report["display"] = json!(display);
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use setups_domain::{expectancy::*, forecast::ForecastRecipe, trend::*};

    fn source() -> Source {
        serde_json::from_value(json!({
            "algorithmId":"price_volume_spike","algorithmVersion":"1.1.1","params":setups_domain::AlgorithmParams::Spike(setups_domain::SpikeParams::default()),
            "instruments":[{"exchange":"fixture","category":"linear","symbol":"FIX"}],"startUnix":600.0,
            "executionStartedAt":"1970-01-01T00:10:00Z","direction":"short",
            "analytics":{"clusterEnabled":false,"clusterGapMinutes":null,"timeShiftMinutes":1,"priceChangePct":null,"setupMaturity":0.0,"horizonMinutes":10080},
            "tpPct":10.0,"slPct":3.0,"continuous":true,
            "objective":{"version":1,"trendMode":"mean","trendBendDays":"linear","trendBlendLambda":0.3,"forecast":null},
            "originalEndpoint":{},"sourceArtifactId":null,"artifact":null,"sourceIdentity":"same-study-hash","selectionFromOpenUnix":540
        })).unwrap()
    }
    fn row(source: &Source) -> Value {
        json!({"status":"completed","revision":7,"job_id":"job-seven","observed_through_unix":86400,
            "source_identity":source.source_identity,"sourceFacts":{"tpPct":source.tp_pct,"slPct":source.sl_pct,"objective":source.objective}})
    }
    #[test]
    fn combined_never_labels_snapshot_seven_with_an_independently_replaced_head() {
        let source = source(); let row = row(&source); let summary = super::super::dto(&row);
        assert_eq!(combined_metadata(&source, Some(&row), &summary), Ok((86400, 7, "job-seven".into())));
        for (key, value) in [("revision",json!(8)),("jobId",json!("job-eight")),("observedThroughUnix",json!(172800)),("status",json!("queued"))] {
            let mut replaced = summary.clone(); replaced[key] = value;
            let metadata = combined_metadata(&source, Some(&row), &replaced);
            assert_eq!(metadata, Err("post_study_revision_changed"));
            let projection = projection::project(None, source.objective, metadata.as_ref()
                .map(|(cutoff,revision,job_id)|PublishedActual { daily: &[], cutoff:*cutoff, revision:*revision, job_id })
                .map_err(|reason|*reason));
            assert!(matches!(projection.availability, DisplayAvailability::Unavailable { reason } if reason=="post_study_revision_changed"));
            assert!(projection.points.is_empty()); assert!(projection.revision.is_none()); assert!(projection.job_id.is_none());
        }
    }
    #[test]
    fn combined_exact_pair_and_objective_are_bound_even_with_identical_study_hash() {
        let source = source(); let mut row = row(&source); let summary = super::super::dto(&row);
        row["sourceFacts"]["tpPct"] = json!(20.0); row["sourceFacts"]["slPct"] = json!(6.0);
        assert_eq!(20.0/6.0, source.tp_pct/source.sl_pct);
        assert_eq!(combined_metadata(&source, Some(&row), &summary), Err("post_study_pair_mismatch"));
        row["sourceFacts"]["tpPct"] = json!(source.tp_pct); row["sourceFacts"]["slPct"] = json!(source.sl_pct);
        for (key, value) in [("trendMode",json!("blend")),("trendBendDays",json!("7")),("trendBlendLambda",json!(0.7)),
            ("forecast",json!(ForecastRecipe::engineering_baseline()))] {
            let mut changed = row.clone(); changed["sourceFacts"]["objective"][key] = value;
            assert_eq!(combined_metadata(&source, Some(&changed), &summary), Err("post_study_objective_mismatch"));
        }
        row["sourceFacts"].as_object_mut().unwrap().remove("objective");
        assert_eq!(combined_metadata(&source, Some(&row), &summary), Err("post_study_objective_unavailable"));
    }
    #[test]
    fn original_report_preserves_sealed_range_even_when_cutoff_is_later() {
        let input: Value = serde_json::from_str(include_str!("../../../domain/tests/fixtures/expectancy_utc_v2.json")).unwrap();
        for cutoff_after_range in [false, true] {
            let mut input: ExpectancyInput = serde_json::from_value(input["input"].clone()).unwrap();
            input.source.forecast = Some(ForecastRecipe::engineering_baseline());
            if cutoff_after_range { input.source.provenance.observation_cutoff += 3600; }
            let artifact = build_expectancy_artifact(input).unwrap();
            let mut source = source(); source.objective = ExpectancyObjective { version:1, trend_mode:TrendMode::Forecast,
                trend_bend_days:TrendBendDays::Linear, trend_blend_lambda:TrendBlendLambda::default(), forecast:artifact.source.forecast };
            let through = artifact.to_unix.max(artifact.source.provenance.observation_cutoff);
            if cutoff_after_range {
                assert!(artifact.objective_series_through(source.objective,artifact.to_unix).is_err(),
                    "a valid explicit forecast's selection end can precede its sealed observation cutoff");
            }
            let sealed = artifact.objective_series(source.objective).unwrap();
            let before = artifact.to_json().unwrap(); source.artifact = Some(artifact);
            let display = StudyDisplay { time_zone:"UTC".into(), start_day_unix:0, daily:vec![],
                historical:DisplaySource::unavailable("fixture",false), actual:DisplaySource::unavailable("fixture",false) };
            let report = original_report(&source, &display);
            assert_eq!(report["observedThroughUnix"], through);
            assert_eq!(report["projection"]["availability"]["status"], "available");
            assert_eq!(report["projection"]["points"],json!(sealed.curve.iter().map(|(at,value)|json!({"atUnix":at,"value":value})).collect::<Vec<_>>()),
                "compatibility bound must preserve the sealed curve exactly, not append new points");
            let actual_report = model::chart(&source, through+7*86400, vec![]);
            assert_ne!(actual_report["projection"]["points"],report["projection"]["points"],"fixture must have a later actual window");
            assert_eq!(source.artifact.as_ref().unwrap().to_json().unwrap(), before);
            assert_eq!(original_report(&source, &display), report, "actual publication/window must not extend this original window");
        }
    }
}
