//! Bounded lifecycle records in the application's existing chronological stderr
//! sink. No payload history or separate log files are retained by this module.
use serde_json::{Value, json};
use std::{
    io::Write,
    sync::atomic::{AtomicU64, Ordering},
};

pub const MAX_RECORD_BYTES: usize = 2048;
static LOST: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Debug,
    Info,
    Warn,
    Error,
}
impl Level {
    fn name(self) -> &'static str {
        match self {
            Self::Debug => "DEBUG",
            Self::Info => "INFO",
            Self::Warn => "WARN",
            Self::Error => "ERROR",
        }
    }
}
pub fn debug_enabled() -> bool {
    std::env::var("RUST_LOG").is_ok_and(|setting| {
        setting
            .split(',')
            .any(|part| matches!(part.trim().rsplit('=').next(), Some("debug" | "trace")))
    })
}

/// Only scalar metadata is admitted. Strings/field count/total bytes are bounded
/// even if a caller accidentally supplies a large diagnostic value.
pub fn record(level: Level, event: &str, fields: &Value) -> Value {
    let mut result = json!({"timestamp":chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis,true),
        "level":level.name(),"event":event.chars().take(80).collect::<String>()});
    let mut truncated = false;
    if let Some(fields) = fields.as_object() {
        for (index, (key, value)) in fields.iter().enumerate() {
            if index >= 32 {
                truncated = true;
                break;
            }
            if matches!(key.as_str(), "timestamp" | "level" | "event") {
                continue;
            }
            let value = match value {
                Value::String(text) => {
                    truncated |= text.chars().count() > 160;
                    Value::String(text.chars().take(160).collect())
                }
                Value::Null | Value::Bool(_) | Value::Number(_) => value.clone(),
                _ => {
                    truncated = true;
                    Value::String("[omitted]".into())
                }
            };
            let key: String = key.chars().take(48).collect();
            result[&key] = value;
            // Reserve room for truncation/loss metadata and the final LF.
            if result.to_string().len() > MAX_RECORD_BYTES - 128 {
                result.as_object_mut().unwrap().remove(&key);
                truncated = true;
                break;
            }
        }
    }
    if truncated {
        result["fields_truncated"] = true.into();
    }
    result
}
pub fn emit(level: Level, event: &str, fields: Value) {
    if level == Level::Debug && !debug_enabled() {
        return;
    }
    let mut value = record(level, event, &fields);
    let lost = LOST.swap(0, Ordering::Relaxed);
    if lost > 0 {
        value["diagnostic_events_lost"] = lost.into();
    }
    let mut bytes = value.to_string().into_bytes();
    bytes.push(b'\n');
    if std::io::stderr().lock().write_all(&bytes).is_err() {
        let _ = LOST.try_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
            Some(n.saturating_add(lost.saturating_add(1)))
        });
    }
}
/// Observe acquisition separately from execution, without SQL, keys, URLs or
/// error messages. Pool counters are instantaneous snapshots, not reservations.
pub async fn pool_operation<T>(
    pool: &sqlx::PgPool,
    operation: &'static str,
    phase: &'static str,
    future: impl std::future::Future<Output = Result<T, sqlx::Error>>,
) -> Result<T, sqlx::Error> {
    let started = std::time::Instant::now();
    let result = future.await;
    let elapsed = started.elapsed();
    let level = if result.is_err() {
        Level::Error
    } else if elapsed >= std::time::Duration::from_secs(1) {
        Level::Info
    } else {
        Level::Debug
    };
    if level != Level::Debug || debug_enabled() {
        emit(
            level,
            "database_pool_operation_finished",
            pool_operation_fields(pool, operation, phase, elapsed, result.as_ref().err()),
        );
    }
    result
}

fn pool_operation_fields(
    pool: &sqlx::PgPool,
    operation: &str,
    phase: &str,
    elapsed: std::time::Duration,
    error: Option<&sqlx::Error>,
) -> Value {
    json!({"component":"database_pool","operation":operation,"phase":phase,
        "outcome":if error.is_some() {"error"} else {"completed"},
        "error_code":error.map(sqlx_error_code),"duration_ms":elapsed.as_millis() as u64,
        "pool_size":pool.size(),"pool_idle":pool.num_idle(),
        "pool_max":pool.options().get_max_connections()})
}

/// Do not serialize SQL statements, parameters, remote response bodies or URLs.
pub fn db_error_code(error: &crate::DbError) -> &'static str {
    match error {
        crate::DbError::Json(_) => "invalid_state",
        crate::DbError::Sqlx(error) => sqlx_error_code(error),
    }
}
fn sqlx_error_code(error: &sqlx::Error) -> &'static str {
    match error {
        sqlx::Error::PoolTimedOut => "pool_timeout",
        sqlx::Error::PoolClosed => "pool_closed",
        sqlx::Error::Io(_) | sqlx::Error::Tls(_) | sqlx::Error::Protocol(_) => "database_transport",
        sqlx::Error::Database(error) => match error.code().as_deref() {
            Some("55P03") => "lock_timeout",
            Some("57014") => "statement_timeout_or_cancel",
            Some("40P01") => "deadlock",
            Some("40001") => "serialization_failure",
            Some("23505") => "unique_violation",
            Some("23503") => "foreign_key_violation",
            Some("23514") => "check_violation",
            _ => "database_statement",
        },
        _ => "database_decode",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn lifecycle_records_bound_bytes_fields_and_multibyte_strings_without_payload_dumps() {
        let mut fields = json!({"payload":{"secret":"not emitted"},"text":"я\n".repeat(5000)});
        for n in 0..100 {
            fields[format!("field_{n}")] = Value::String("x".repeat(5000));
        }
        let record = record(Level::Info, "warmup_claimed", &fields);
        assert!(record.to_string().len() + 1 <= MAX_RECORD_BYTES);
        assert!(record.as_object().unwrap().len() <= 36);
        assert_eq!(record["fields_truncated"], true);
        assert!(!record.to_string().contains("not emitted"));
        assert!(!record.to_string().contains('\n'));
    }

    #[tokio::test]
    async fn pool_operation_fields_attribute_timeout_without_query_or_credentials() {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(4)
            .connect_lazy("postgres://user:secret@127.0.0.1:1/signals")
            .unwrap();
        let fields = pool_operation_fields(
            &pool,
            "profile_handoff",
            "acquire_transaction",
            std::time::Duration::from_secs(5),
            Some(&sqlx::Error::PoolTimedOut),
        );
        assert_eq!(fields["operation"], "profile_handoff");
        assert_eq!(fields["phase"], "acquire_transaction");
        assert_eq!(fields["error_code"], "pool_timeout");
        assert_eq!(fields["pool_max"], 4);
        assert_eq!(fields["duration_ms"], 5000);
        let rendered_record = record(Level::Error, "database_pool_operation_finished", &fields);
        for key in [
            "operation",
            "phase",
            "error_code",
            "pool_size",
            "pool_idle",
            "pool_max",
            "duration_ms",
        ] {
            assert_eq!(rendered_record[key], fields[key], "rendering lost {key}");
        }
        assert!(rendered_record.get("fields_truncated").is_none());
        let settings = pool_operation_fields(
            &pool,
            "settings_get",
            "acquire",
            std::time::Duration::from_secs(5),
            Some(&sqlx::Error::PoolTimedOut),
        );
        let settings = record(Level::Error, "database_pool_operation_finished", &settings);
        assert_eq!(settings["operation"], "settings_get");
        assert_eq!(settings["phase"], "acquire");
        assert_eq!(settings["error_code"], rendered_record["error_code"]);
        let rendered = rendered_record.to_string();
        assert!(!rendered.contains("secret"));
        assert!(!rendered.contains("select"));
    }
}
