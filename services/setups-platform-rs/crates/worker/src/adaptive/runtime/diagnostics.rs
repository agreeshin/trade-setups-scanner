//! Per-tick diagnostic scalars die with the tick; no history/payload is retained.
use super::*;
use setups_db::diagnostics::{self as log, Level};

pub(super) fn error_code(error: &WorkerError) -> &'static str {
    match error {
        WorkerError::WarmupFailure { cause, .. } => error_code(cause),
        WorkerError::WarmupControl(_)=>"warmup_supervision_lost",
        WorkerError::Db(error) => log::db_error_code(error),
        WorkerError::Detector(_) => "detector",
        WorkerError::Json(_) => "json_decode",
        WorkerError::PageLimitExceeded(_) => "page_limit",
        WorkerError::Source(message) if message.starts_with("adaptive: history_retention_rejected")=>"history_retention_rejected",
        WorkerError::Source(message) if message.starts_with("adaptive: history_lookup_evicted")=>"history_lookup_evicted",
        WorkerError::Source(message) if message.contains("market data network error") => {
            "source_transport"
        }
        WorkerError::Source(message) if message.contains("market data api ") => "source_http",
        WorkerError::Source(message) if message.contains("market data decode error") => {
            "source_decode"
        }
        WorkerError::Source(_) => "source_validation",
    }
}

/// One bounded scalar snapshot of the owned attempt, not a history of ticks.
pub(super) fn warmup_fields(warmup:&Warmup,state:&AdaptiveRuntime,target:i64)->Value {
    let mut fields=live::warmup_diagnostic_fields(&warmup.lease);
    let mut total=0;let mut resident=0;let mut ready=0;let mut uninspected=0;
    let mut cursor_min:Option<i64>=None;let mut cursor_max:Option<i64>=None;
    for plan in &warmup.plans {
        if !plan.specs.iter().any(|spec|spec.key()==warmup.candidate_key) {continue;}
        total+=1;
        if let Some(checkpoint)=state.checkpoint_for(&plan.epoch,&warmup.candidate_key) {
            resident+=1;
            cursor_min=Some(cursor_min.map_or(checkpoint.next_open_unix,|old|old.min(checkpoint.next_open_unix)));
            cursor_max=Some(cursor_max.map_or(checkpoint.next_open_unix,|old|old.max(checkpoint.next_open_unix)));
        }else if !warmup.waiting.contains_key(&instrument_key(&plan.instrument)) {uninspected+=1;}
        ready+=usize::from(state.ready(&plan.epoch,&warmup.candidate_key,target));
    }
    fields["study_id"]=warmup.pending.profile.study_id.into();
    fields["stage"]="warmup".into();fields["duration_ms"]=(warmup.started.elapsed().as_millis() as u64).into();
    fields["target_unix"]=target.into();fields["total_instruments"]=total.into();
    fields["resident_instruments"]=resident.into();fields["ready_instruments"]=ready.into();
    fields["waiting_instruments"]=warmup.waiting.len().into();fields["uninspected_instruments"]=uninspected.into();
    fields["cursor_min_unix"]=cursor_min.into();fields["cursor_max_unix"]=cursor_max.into();
    fields["initialization_from_unix"]=warmup.lease.initialization.detector_from_open_unix.into();
    fields["initialization_anchor_unix"]=warmup.lease.initialization.anchor_close_unix.into();
    fields
}

pub(super) fn source_failure_fields(plan:&SymbolPlan,error:&WorkerError,stage:&str,warmup:Option<&Warmup>)->Value {
    // A canonical incumbent epoch may fail while a different candidate owns the
    // warmup lane. The charged attempt is context, NOT the source/anchor owner.
    json!({"component":"adaptive_source","stage":stage,"outcome":"error","context_role":"source_epoch",
        "instrument_key":instrument_key(&plan.instrument),"source_instrument_id":plan.catalog_id,"history_epoch":plan.epoch,
        "error_code":error_code(error),"profile_count":plan.specs.len(),
        "runtime_instance":warmup.map(|w|w.lease.worker.as_str()),
        "candidate_in_plan":warmup.map(|w|plan.specs.iter().any(|spec|spec.key()==w.candidate_key)),
        "warmup_profile_id":warmup.map(|w|w.lease.slot_id),"warmup_generation_id":warmup.map(|w|w.lease.generation_id),
        "warmup_study_id":warmup.map(|w|w.pending.profile.study_id),"warmup_attempt":warmup.map(|w|w.lease.attempt),
        "warmup_control_revision":warmup.map(|w|w.lease.config_revision),"warmup_lease_token":warmup.map(|w|w.lease.token)})
}
pub(super) fn source_failure(plan:&SymbolPlan,error:&WorkerError,stage:&str,warmup:Option<&Warmup>) {
    log::emit(Level::Error,"adaptive_source_failed",source_failure_fields(plan,error,stage,warmup));
}

pub(super) fn failure_report(
    error: WorkerError,
    report: &Result<bool, setups_db::DbError>,
    mut fields: Value,
) -> (WorkerError, Value) {
    let outcome = match report {
        Ok(true) => WarmupFailureReportOutcome::Persisted,
        Ok(false) => WarmupFailureReportOutcome::RejectedStaleOrFenced,
        Err(_) => WarmupFailureReportOutcome::DatabaseError,
    };
    fields["failure_report"] = json!(outcome);
    fields["original_error_code"] = error_code(&error).into();
    // Failure outcome/cause must survive the common sink's 32-field ceiling.
    // Initialization bounds already have their admission/progress records.
    for key in ["initialization_from_unix","initialization_anchor_unix","resident_instruments","cursor_max_unix"] {
        fields.as_object_mut().unwrap().remove(key);
    }
    if let WorkerError::Source(message) = &error
        && let Some((_, status)) = message.split_once("market data api ")
    {
        let status: String = status.chars().take(3).collect();
        if status.len() == 3 && status.bytes().all(|b| b.is_ascii_digit()) {
            fields["source_http_status"] = status.into();
        }
    }
    if let Err(error) = report {
        fields["report_error_code"] = log::db_error_code(error).into();
    }
    (
        WorkerError::WarmupFailure {
            cause: Box::new(error),
            report: outcome,
        },
        fields,
    )
}

pub(super) struct TickTrace {
    worker: String,
    pub started: Instant,
    stage: &'static str,
    phase_started: Instant,
    pub pages: usize,
    pub bars: usize,
}
impl TickTrace {
    pub fn new(worker: &str) -> Self {
        let now = Instant::now();
        let trace = Self {
            worker: worker.into(),
            started: now,
            stage: "profile_handoff",
            phase_started: now,
            pages: 0,
            bars: 0,
        };
        trace.phase("started", Level::Debug);
        trace
    }
    fn phase(&self, outcome: &str, level: Level) {
        log::emit(
            level,
            "adaptive_tick_phase",
            json!({"runtime_instance":self.worker,"stage":self.stage,
            "outcome":outcome,"duration_ms":self.phase_started.elapsed().as_millis() as u64}),
        );
    }
    pub fn enter(&mut self, stage: &'static str) {
        self.phase("completed", Level::Debug);
        self.stage = stage;
        self.phase_started = Instant::now();
        self.phase("started", Level::Debug);
    }
    pub fn fields(&self, owner: &AdaptiveLiveRuntime, target: i64) -> Value {
        let mut fields = owner.warmup.as_ref().map_or_else(
            || json!({"runtime_instance":self.worker}),
            |w| warmup_fields(w,&owner.state,target),
        );
        fields["stage"] = self.stage.into();
        fields["duration_ms"] = (self.started.elapsed().as_millis() as u64).into();
        fields["stage_duration_ms"] = (self.phase_started.elapsed().as_millis() as u64).into();
        fields["history_pages_read"] = self.pages.into();
        fields["history_bars_read"] = self.bars.into();
        fields["history_epochs"] = owner.retained_history_epochs().into();
        fields["history_bytes"] = owner.retained_history_bytes().into();
        fields["profile_bytes"] = owner.retained_profile_bytes().into();
        fields
    }
    pub fn finish(
        &self,
        owner: &AdaptiveLiveRuntime,
        target: i64,
        level: Level,
        error: Option<&WorkerError>,
    ) {
        self.phase(
            if error.is_some() {
                "failed"
            } else {
                "completed"
            },
            Level::Debug,
        );
        if level == Level::Debug && !log::debug_enabled() {
            return;
        }
        let mut fields = self.fields(owner, target);
        fields["outcome"] = if error.is_some() {
            "error"
        } else {
            "completed"
        }
        .into();
        if let Some(error) = error {
            fields["error_code"] = error_code(error).into();
        }
        log::emit(level, "adaptive_tick_finished", fields);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error;
    #[test]
    fn stale_failure_report_is_visible_and_retains_original_technical_cause() {
        let original = invalid("market data api 503: do-not-log-raw-response");
        let original_text = original.to_string();
        let (error, fields) = failure_report(
            original,
            &Ok(false),
            json!({"stage":"cold_history","generation_id":25}),
        );
        assert!(matches!(
            error,
            WorkerError::WarmupFailure {
                report: WarmupFailureReportOutcome::RejectedStaleOrFenced,
                ..
            }
        ));
        assert_eq!(error.source().unwrap().to_string(), original_text);
        let record = log::record(Level::Error, "adaptive_warmup_failure_report", &fields);
        assert_eq!(record["failure_report"], "rejected_stale_or_fenced");
        assert_eq!(record["original_error_code"], "source_http");
        assert_eq!(record["source_http_status"], "503");
        assert_eq!(record["stage"], "cold_history");
        assert!(!record.to_string().contains("do-not-log-raw-response"));
    }
    #[test]
    fn persistence_and_report_database_failure_are_distinct_outcomes() {
        let (_, ok) = failure_report(invalid("original"), &Ok(true), json!({}));
        assert_eq!(ok["failure_report"], "persisted");
        let (_, failed) = failure_report(
            invalid("original"),
            &Err(setups_db::DbError::Sqlx(sqlx::Error::PoolTimedOut)),
            json!({}),
        );
        assert_eq!(failed["failure_report"], "database_error");
        assert_eq!(failed["original_error_code"], "source_validation");
        assert_eq!(failed["report_error_code"], "pool_timeout");
    }
}
