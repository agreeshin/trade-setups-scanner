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
            stage: "settings",
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
            |w| {
                let mut fields = live::warmup_diagnostic_fields(&w.lease);
                fields["study_id"] = w.pending.profile.study_id.into();
                let candidate: Vec<_> = w
                    .plans
                    .iter()
                    .filter(|p| p.specs.iter().any(|s| s.key() == w.candidate_key))
                    .collect();
                fields["ready_instruments"] = candidate
                    .iter()
                    .filter(|p| owner.state.ready(&p.epoch, &w.candidate_key, target))
                    .count()
                    .into();
                fields["waiting_instruments"] = w.waiting.len().into();
                fields["total_instruments"] = candidate.len().into();
                fields
            },
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
