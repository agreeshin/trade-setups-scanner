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
/// Do not serialize SQL statements, parameters, remote response bodies or URLs.
pub fn db_error_code(error: &crate::DbError) -> &'static str {
    match error {
        crate::DbError::Json(_) => "invalid_state",
        crate::DbError::Sqlx(sqlx::Error::PoolTimedOut) => "pool_timeout",
        crate::DbError::Sqlx(sqlx::Error::PoolClosed) => "pool_closed",
        crate::DbError::Sqlx(
            sqlx::Error::Io(_) | sqlx::Error::Tls(_) | sqlx::Error::Protocol(_),
        ) => "database_transport",
        crate::DbError::Sqlx(sqlx::Error::Database(error)) => match error.code().as_deref() {
            Some("55P03") => "lock_timeout",
            Some("57014") => "statement_timeout_or_cancel",
            Some("40P01") => "deadlock",
            Some("40001") => "serialization_failure",
            Some("23505") => "unique_violation",
            Some("23503") => "foreign_key_violation",
            Some("23514") => "check_violation",
            _ => "database_statement",
        },
        crate::DbError::Sqlx(_) => "database_decode",
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
}
