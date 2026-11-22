//! Presentation only. Reads persisted outcomes; never fits a model or starts replay.
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use setups_db::study_display::{self as db, DisplayDay};
use sqlx::PgPool;
use super::model::{self, Source};

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
    value["originalReport"] = match source {
        Ok(source) => original_report(&source, &original_display),
        Err(reason) => { value["originalReportUnavailableReason"] = json!(reason); Value::Null },
    };
    if value["status"] == "completed" && value["report"].is_object() {
        let actual = DisplaySource::available("persisted_post_study", format!("{}:{}:{}", study, value["revision"], value["jobId"]), true);
        let display = StudyDisplay { time_zone: zone.into(), start_day_unix,
            daily: snapshot.combined_daily,
            historical, actual };
        value["report"]["display"] = serde_json::to_value(display).map_err(|_|"study_display_invalid_outcomes")?;
    }
    Ok(value)
}

fn original_report(source: &Source, display: &StudyDisplay) -> Value {
    // Frozen model's original through/cutoff, not now or a refresh admission window.
    let through = source.artifact.as_ref().map(|a| a.to_unix)
        .or_else(|| source.original_endpoint["atUnix"].as_i64()).unwrap_or(source.execution_started_at.timestamp());
    let mut report = model::chart(source, through, vec![]);
    report["display"] = json!(display);
    report
}
