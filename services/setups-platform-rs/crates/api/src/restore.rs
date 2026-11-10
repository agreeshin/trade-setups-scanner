use crate::config::PlatformConfig;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Совпадает со scope BackupManager в main.rs: маркеры restore-маршрутов
/// пишутся как {scope}.restore.pending в BACKUP_DIR.
const RESTORE_SCOPE: &str = "platform";

fn write_status(path: &Path, body: &str) {
    let _ = std::fs::write(path, body);
}

fn pg_dump_custom(database_url: &str, out: &Path) -> bool {
    let Ok(file) = std::fs::File::create(out) else {
        return false;
    };
    Command::new("pg_dump")
        .args([
            "--dbname",
            database_url,
            "--format=custom",
            "--compress=6",
            "--no-owner",
            "--no-privileges",
        ])
        .stdout(Stdio::from(file))
        .status()
        .is_ok_and(|status| status.success())
}

fn restore_dump(database_url: &str, dump: &Path) -> bool {
    let mut restore = match Command::new("pg_restore")
        .args([
            "--file=-",
            "--clean",
            "--if-exists",
            "--no-owner",
            "--no-privileges",
        ])
        .arg(dump)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(_) => return false,
    };
    let Some(pipe) = restore.stdout.take() else {
        let _ = restore.kill();
        let _ = restore.wait();
        return false;
    };
    let mut psql = match Command::new("psql")
        .args([
            "--dbname",
            database_url,
            "--quiet",
            "--set",
            "ON_ERROR_STOP=1",
        ])
        .stdin(pipe)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(_) => {
            let _ = restore.kill();
            let _ = restore.wait();
            return false;
        }
    };
    restore.wait().is_ok_and(|status| status.success())
        && psql.wait().is_ok_and(|status| status.success())
}

fn rollback_retention() -> usize {
    std::env::var("BACKUP_ROLLBACK_RETENTION")
        .ok()
        .and_then(|value| value.trim().parse().ok())
        .filter(|value| *value > 0)
        .unwrap_or(3)
}

fn cleanup_old_rollbacks(dir: &Path, retention: usize) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let prefix = format!("{RESTORE_SCOPE}.pre-restore-");
    let mut dumps: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(&prefix) && name.ends_with(".dump"))
        })
        .collect();
    dumps.sort();
    dumps.reverse();
    for path in dumps.iter().skip(retention) {
        let _ = std::fs::remove_file(path);
    }
}

/// Порт restore-on-startup из infra/container/service-entrypoint.sh:
/// применяет отложенный restore-дамп по маркеру до подключения к БД.
pub fn apply_pending_restore(config: &PlatformConfig) -> Result<(), String> {
    let dir = Path::new(&config.backup_dir);
    let marker = dir.join(format!("{RESTORE_SCOPE}.restore.pending"));
    if !marker.exists() {
        return Ok(());
    }
    let pending = dir.join(format!("{RESTORE_SCOPE}.restore.pending.dump"));
    let status = dir.join(format!("{RESTORE_SCOPE}.restore.last.json"));
    if !pending.exists() {
        write_status(
            &status,
            r#"{"status":"failed","error":"pending dump is missing"}"#,
        );
        return Err("pending dump is missing".to_string());
    }
    let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");
    let rollback = dir.join(format!("{RESTORE_SCOPE}.pre-restore-{stamp}.dump"));
    let rollback_partial = dir.join(format!("{RESTORE_SCOPE}.pre-restore-{stamp}.dump.partial"));
    if !pg_dump_custom(&config.database_url, &rollback_partial) {
        let _ = std::fs::remove_file(&rollback_partial);
        write_status(
            &status,
            r#"{"status":"failed","error":"pre-restore backup failed"}"#,
        );
        return Err("pre-restore backup failed".to_string());
    }
    if let Err(error) = std::fs::rename(&rollback_partial, &rollback) {
        return Err(format!("rollback rename failed: {error}"));
    }
    let rollback_name = rollback
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_string();
    if restore_dump(&config.database_url, &pending) {
        let applied_at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        write_status(
            &status,
            &format!(
                r#"{{"status":"applied","appliedAt":"{applied_at}","rollback":"{rollback_name}"}}"#
            ),
        );
        let _ = std::fs::remove_file(&marker);
        let _ = std::fs::remove_file(&pending);
    } else if restore_dump(&config.database_url, &rollback) {
        let failed = dir.join(format!("{RESTORE_SCOPE}.restore-failed-{stamp}.dump"));
        let _ = std::fs::rename(&pending, &failed);
        let _ = std::fs::remove_file(&marker);
        let failed_at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        write_status(
            &status,
            &format!(
                r#"{{"status":"failed_rolled_back","failedAt":"{failed_at}","failedDump":"{RESTORE_SCOPE}.restore-failed-{stamp}.dump","rollback":"{rollback_name}"}}"#
            ),
        );
    } else {
        let failed_at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        write_status(
            &status,
            &format!(
                r#"{{"status":"failed_rollback","failedAt":"{failed_at}","rollback":"{rollback_name}"}}"#
            ),
        );
        return Err("restore failed and rollback failed".to_string());
    }
    cleanup_old_rollbacks(dir, rollback_retention());
    Ok(())
}
