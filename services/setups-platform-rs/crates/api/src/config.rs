use setups_db::admission::ReplayAdmissionLimits;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlatformConfig {
    pub database_url: String,
    pub api_keys: Vec<String>,
    pub market_data_url: String,
    pub market_data_api_key: String,
    pub port: u16,
    pub host: String,
    pub log_level: String,
    pub backup_dir: String,
    pub backup_max_upload_bytes: i64,
    pub market_backup_max_upload_bytes: i64,
    pub backup_script: String,
    pub replay_limits: ReplayAdmissionLimits,
    pub replay_workers: usize,
}

fn required(get: &mut dyn FnMut(&str) -> Option<String>, key: &str) -> Result<String, String> {
    let value = get(key);
    match value {
        Some(value) if !value.trim().is_empty() => Ok(value),
        _ => Err(format!("Missing required environment variable: {key}")),
    }
}

fn positive_integer(
    get: &mut dyn FnMut(&str) -> Option<String>,
    key: &str,
    fallback: i64,
) -> Result<i64, String> {
    let raw = get(key);
    let value = match raw {
        Some(text) => text
            .parse::<i64>()
            .map_err(|_| format!("{key} must be a positive safe integer"))?,
        None => fallback,
    };
    if value < 1 {
        return Err(format!("{key} must be a positive safe integer"));
    }
    Ok(value)
}

impl PlatformConfig {
    pub fn from_env() -> Result<Self, String> {
        Self::from_lookup(|key| std::env::var(key).ok())
    }

    pub fn from_lookup(mut get: impl FnMut(&str) -> Option<String>) -> Result<Self, String> {
        let database_url = required(&mut get, "DATABASE_URL")?;
        let api_keys = get("API_KEYS")
            .unwrap_or_default()
            .split(',')
            .map(str::trim)
            .filter(|key| !key.is_empty())
            .map(str::to_string)
            .collect();
        let market_data_url = required(&mut get, "MARKET_DATA_URL")?;
        let market_data_api_key = required(&mut get, "MARKET_DATA_API_KEY")?;
        let port = match get("PORT") {
            Some(text) if !text.trim().is_empty() => text
                .trim()
                .parse::<u16>()
                .map_err(|_| "PORT must be a positive safe integer".to_string())?,
            _ => 8082,
        };
        let host = get("HOST").unwrap_or_else(|| "127.0.0.1".to_string());
        let log_level = get("LOG_LEVEL").unwrap_or_else(|| "info".to_string());
        let backup_dir = get("BACKUP_DIR").unwrap_or_else(|| "./backups".to_string());
        let backup_script = get("BACKUP_SCRIPT_PATH")
            .unwrap_or_else(|| "./scripts/postgres-backup.mjs".to_string());
        let backup_max_upload_bytes =
            positive_integer(&mut get, "BACKUP_MAX_UPLOAD_BYTES", 60 * 1024 * 1024 * 1024)?;
        let market_backup_max_upload_bytes = positive_integer(
            &mut get,
            "MARKET_DATA_BACKUP_MAX_UPLOAD_BYTES",
            120 * 1024 * 1024 * 1024,
        )?;
        let defaults = ReplayAdmissionLimits::default();
        let replay_limits = ReplayAdmissionLimits {
            max_pending_runs: positive_integer(
                &mut get,
                "REPLAY_MAX_PENDING_RUNS",
                defaults.max_pending_runs,
            )?,
            max_work_minutes: positive_integer(
                &mut get,
                "REPLAY_MAX_WORK_MINUTES",
                defaults.max_work_minutes,
            )?,
            max_database_bytes: positive_integer(
                &mut get,
                "REPLAY_MAX_DATABASE_BYTES",
                defaults.max_database_bytes,
            )?,
            estimated_bytes_per_work_minute: positive_integer(
                &mut get,
                "REPLAY_ESTIMATED_BYTES_PER_MINUTE",
                defaults.estimated_bytes_per_work_minute,
            )?,
        };
        let replay_workers = match get("REPLAY_WORKERS") {
            Some(text) if !text.trim().is_empty() => {
                let value = text
                    .trim()
                    .parse::<usize>()
                    .map_err(|_| "REPLAY_WORKERS must be a positive safe integer".to_string())?;
                if value < 1 {
                    return Err("REPLAY_WORKERS must be a positive safe integer".to_string());
                }
                value
            }
            _ => 2,
        };
        Ok(PlatformConfig {
            database_url,
            api_keys,
            market_data_url,
            market_data_api_key,
            port,
            host,
            log_level,
            backup_dir,
            backup_max_upload_bytes,
            market_backup_max_upload_bytes,
            backup_script,
            replay_limits,
            replay_workers,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> impl FnMut(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect();
        move |key| map.get(key).cloned()
    }

    const BASE: &[(&str, &str)] = &[
        ("DATABASE_URL", "postgres://localhost/signals"),
        ("MARKET_DATA_URL", "http://localhost:8081"),
        ("MARKET_DATA_API_KEY", "secret"),
    ];

    #[test]
    fn missing_required_variable_reports_ts_message() {
        let error = PlatformConfig::from_lookup(env(&[])).expect_err("config required");
        assert_eq!(error, "Missing required environment variable: DATABASE_URL");
    }

    #[test]
    fn defaults_match_ts() {
        let config = PlatformConfig::from_lookup(env(BASE)).expect("config loads");
        assert_eq!(config.port, 8082);
        assert_eq!(config.host, "127.0.0.1");
        assert_eq!(config.log_level, "info");
        assert_eq!(config.backup_dir, "./backups");
        assert_eq!(config.backup_max_upload_bytes, 60 * 1024 * 1024 * 1024);
        assert_eq!(
            config.market_backup_max_upload_bytes,
            120 * 1024 * 1024 * 1024
        );
        assert!(config.api_keys.is_empty());
        assert_eq!(config.replay_limits, ReplayAdmissionLimits::default());
        assert_eq!(config.replay_workers, 2);
    }

    #[test]
    fn api_keys_split_trim_and_filter() {
        let config = PlatformConfig::from_lookup(env(&[
            ("DATABASE_URL", "postgres://x"),
            ("MARKET_DATA_URL", "http://x"),
            ("MARKET_DATA_API_KEY", "k"),
            ("API_KEYS", " one , two ,,  "),
        ]))
        .expect("config loads");
        assert_eq!(config.api_keys, vec!["one".to_string(), "two".to_string()]);
    }

    #[test]
    fn positive_integer_rejects_invalid_values() {
        let error = PlatformConfig::from_lookup(env(&[
            ("DATABASE_URL", "postgres://x"),
            ("MARKET_DATA_URL", "http://x"),
            ("MARKET_DATA_API_KEY", "k"),
            ("REPLAY_MAX_PENDING_RUNS", "0"),
        ]))
        .expect_err("zero rejected");
        assert_eq!(
            error,
            "REPLAY_MAX_PENDING_RUNS must be a positive safe integer"
        );
    }
}
