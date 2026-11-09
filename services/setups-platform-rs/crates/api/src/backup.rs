use crate::auth::SessionAuth;
use crate::replay::guard_auth;
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio_util::io::ReaderStream;

pub struct BackupState {
    pub auth: Arc<SessionAuth>,
    pub manager: BackupManager,
    pub market_url: String,
    pub market_key: String,
    pub market_max_upload_bytes: i64,
    pub http: reqwest::Client,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupManifest {
    pub format: String,
    pub scope: String,
    pub created_at: String,
    pub schema_version: String,
    pub postgres_client: String,
    pub payload_bytes: i64,
    pub payload_sha256: String,
    pub staged_at: Option<String>,
}

pub struct BackupManager {
    pub scope: String,
    pub max_upload_bytes: i64,
    pub database_url: String,
    pub backup_dir: String,
    pub script_path: String,
}

fn random_uuid() -> String {
    let mut bytes = [0u8; 16];
    rand::rng().fill_bytes(&mut bytes);
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: Vec<String> = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!(
        "{}{}{}{}-{}{}-{}{}-{}{}-{}{}{}{}{}{}",
        hex[0],
        hex[1],
        hex[2],
        hex[3],
        hex[4],
        hex[5],
        hex[6],
        hex[7],
        hex[8],
        hex[9],
        hex[10],
        hex[11],
        hex[12],
        hex[13],
        hex[14],
        hex[15],
    )
}

fn tail_chars(text: &str, limit: usize) -> String {
    text.chars()
        .rev()
        .take(limit)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
}

fn ensure_private_dir(path: &Path) -> Result<(), String> {
    std::fs::create_dir_all(path).map_err(|error| error.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700));
    }
    Ok(())
}

async fn write_private_file(path: &Path, payload: &[u8]) -> Result<(), String> {
    let mut file = tokio::fs::File::create(path)
        .await
        .map_err(|error| error.to_string())?;
    file.write_all(payload)
        .await
        .map_err(|error| error.to_string())?;
    file.sync_all().await.map_err(|error| error.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

impl BackupManager {
    async fn run_helper(&self, args: &[&str]) -> Result<BackupManifest, String> {
        let output = tokio::process::Command::new("node")
            .arg(&self.script_path)
            .args(args)
            .env("DATABASE_URL", &self.database_url)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output()
            .await
            .map_err(|error| error.to_string())?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let trimmed = tail_chars(stderr.trim(), 4_000);
            return Err(if trimmed.is_empty() {
                format!(
                    "backup helper failed ({})",
                    output.status.code().unwrap_or(-1)
                )
            } else {
                trimmed
            });
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        serde_json::from_str(&tail_chars(stdout.trim(), 16_000))
            .map_err(|error| format!("backup helper produced an invalid manifest: {error}"))
    }

    pub async fn create(&self) -> Result<(PathBuf, BackupManifest), String> {
        ensure_private_dir(Path::new(&self.backup_dir))?;
        let path =
            Path::new(&self.backup_dir).join(format!("{}-{}.tsb", self.scope, random_uuid()));
        let manifest = match self
            .run_helper(&[
                "create",
                "--scope",
                &self.scope,
                "--output",
                &path.to_string_lossy(),
            ])
            .await
        {
            Ok(value) => value,
            Err(error) => {
                let _ = tokio::fs::remove_file(&path).await;
                return Err(error);
            }
        };
        Ok((path, manifest))
    }

    pub async fn stage(&self, payload: &[u8]) -> Result<BackupManifest, String> {
        if payload.len() as i64 > self.max_upload_bytes {
            return Err(format!(
                "backup exceeds {} byte upload limit",
                self.max_upload_bytes
            ));
        }
        ensure_private_dir(Path::new(&self.backup_dir))?;
        let upload =
            Path::new(&self.backup_dir).join(format!("{}-{}.upload", self.scope, random_uuid()));
        let staged = async {
            write_private_file(&upload, payload).await?;
            self.run_helper(&[
                "stage",
                "--scope",
                &self.scope,
                "--input",
                &upload.to_string_lossy(),
                "--backup-dir",
                &self.backup_dir,
            ])
            .await
        }
        .await;
        let _ = tokio::fs::remove_file(&upload).await;
        staged
    }

    pub async fn status(&self) -> (bool, Value) {
        let marker = Path::new(&self.backup_dir).join(format!("{}.restore.pending", self.scope));
        let pending = marker.exists();
        let last = Path::new(&self.backup_dir).join(format!("{}.restore.last.json", self.scope));
        let last = match tokio::fs::read_to_string(&last).await {
            Ok(text) => serde_json::from_str(&text).unwrap_or(Value::Null),
            Err(_) => Value::Null,
        };
        (pending, last)
    }
}

fn map_stage_error(message: &str) -> Option<(&'static str, StatusCode)> {
    if message.contains("pending") || message.contains("already in progress") {
        return Some(("restore_already_pending", StatusCode::CONFLICT));
    }
    if message.contains("insufficient free space") {
        return Some((
            "insufficient_restore_space",
            StatusCode::INSUFFICIENT_STORAGE,
        ));
    }
    const INVALID_BACKUP_NEEDLES: [&str; 7] = [
        "truncated",
        "unsupported backup format",
        "invalid backup manifest",
        "scope mismatch",
        "payload size mismatch",
        "checksum mismatch",
        "pg_restore:",
    ];
    if INVALID_BACKUP_NEEDLES
        .iter()
        .any(|needle| message.contains(needle))
    {
        return Some(("invalid_backup", StatusCode::BAD_REQUEST));
    }
    None
}

fn error_response(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}

fn internal_error() -> Response {
    error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal_error")
}

struct CleanupStream {
    inner: ReaderStream<tokio::fs::File>,
    path: PathBuf,
}

impl futures_core::Stream for CleanupStream {
    type Item = <ReaderStream<tokio::fs::File> as futures_core::Stream>::Item;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        std::pin::Pin::new(&mut self.inner).poll_next(cx)
    }
}

impl Drop for CleanupStream {
    fn drop(&mut self) {
        let path = std::mem::take(&mut self.path);
        tokio::spawn(async move {
            let _ = tokio::fs::remove_file(path).await;
        });
    }
}

async fn download_backup(State(state): State<Arc<BackupState>>, headers: HeaderMap) -> Response {
    if let Err(response) = guard_auth(&state.auth, &headers, true) {
        return response;
    }
    let (path, manifest) = match state.manager.create().await {
        Ok(value) => value,
        Err(_) => return internal_error(),
    };
    let size = match tokio::fs::metadata(&path).await {
        Ok(meta) => meta.len(),
        Err(_) => {
            let _ = tokio::fs::remove_file(&path).await;
            return internal_error();
        }
    };
    let file = match tokio::fs::File::open(&path).await {
        Ok(file) => file,
        Err(_) => {
            let _ = tokio::fs::remove_file(&path).await;
            return internal_error();
        }
    };
    let filename = format!(
        "{}-{}.tsb",
        state.manager.scope,
        manifest.created_at.replace(':', "-")
    );
    let stream = CleanupStream {
        inner: ReaderStream::new(file),
        path,
    };
    let mut response = Response::new(Body::from_stream(stream));
    let response_headers = response.headers_mut();
    response_headers.insert("cache-control", HeaderValue::from_static("no-store"));
    response_headers.insert(
        "content-type",
        HeaderValue::from_static("application/octet-stream"),
    );
    if let Ok(value) = HeaderValue::from_str(&size.to_string()) {
        response_headers.insert("content-length", value);
    }
    if let Ok(value) = HeaderValue::from_str(&format!("attachment; filename=\"{filename}\"")) {
        response_headers.insert("content-disposition", value);
    }
    response
}

async fn upload_restore(
    State(state): State<Arc<BackupState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err(response) = guard_auth(&state.auth, &headers, true) {
        return response;
    }
    let expected = format!("RESTORE {}", state.manager.scope);
    if headers
        .get("x-restore-confirmation")
        .and_then(|value| value.to_str().ok())
        != Some(expected.as_str())
    {
        return error_response(
            StatusCode::BAD_REQUEST,
            &format!("confirmation_required: RESTORE {}", state.manager.scope),
        );
    }
    if body.is_empty() {
        return error_response(StatusCode::BAD_REQUEST, "binary_backup_required");
    }
    match state.manager.stage(&body).await {
        Ok(manifest) => (
            StatusCode::ACCEPTED,
            Json(json!({
                "status": "staged",
                "scope": state.manager.scope,
                "restartRequired": true,
                "manifest": manifest,
            })),
        )
            .into_response(),
        Err(message) => {
            if let Some((code, status)) = map_stage_error(&message) {
                return error_response(status, code);
            }
            internal_error()
        }
    }
}

async fn restore_status(State(state): State<Arc<BackupState>>, headers: HeaderMap) -> Response {
    if let Err(response) = guard_auth(&state.auth, &headers, false) {
        return response;
    }
    let (pending, last) = state.manager.status().await;
    Json(json!({ "pending": pending, "last": last })).into_response()
}

async fn market_backup(State(state): State<Arc<BackupState>>, headers: HeaderMap) -> Response {
    if let Err(response) = guard_auth(&state.auth, &headers, false) {
        return response;
    }
    let response = match state
        .http
        .get(format!("{}/v1/admin/backup", state.market_url))
        .header("x-api-key", &state.market_key)
        .send()
        .await
    {
        Ok(value) => value,
        Err(_) => return internal_error(),
    };
    let upstream_status = response.status();
    if !upstream_status.is_success() {
        return error_response(
            StatusCode::from_u16(upstream_status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY),
            &format!("market_backup_failed:{}", upstream_status.as_u16()),
        );
    }
    let mut builder = Response::builder();
    for name in [
        "content-type",
        "content-length",
        "content-disposition",
        "cache-control",
    ] {
        if let Some(value) = response.headers().get(name) {
            builder = builder.header(name, value);
        }
    }
    builder
        .body(Body::from_stream(response.bytes_stream()))
        .unwrap_or_else(|_| internal_error())
}

async fn market_restore_upload(
    State(state): State<Arc<BackupState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err(response) = guard_auth(&state.auth, &headers, true) {
        return response;
    }
    if body.is_empty() {
        return error_response(StatusCode::BAD_REQUEST, "binary_backup_required");
    }
    let confirmation = headers
        .get("x-restore-confirmation")
        .and_then(|value| value.to_str().ok());
    let mut request = state
        .http
        .post(format!("{}/v1/admin/restore", state.market_url))
        .header("x-api-key", &state.market_key)
        .header("content-type", "application/octet-stream");
    if let Some(value) = confirmation {
        request = request.header("x-restore-confirmation", value);
    }
    let response = match request.body(body.to_vec()).send().await {
        Ok(value) => value,
        Err(_) => return internal_error(),
    };
    let status = response.status();
    let content_type = response.headers().get("content-type").cloned();
    let text = response.text().await.unwrap_or_default();
    let mut builder = Response::builder()
        .status(StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY));
    match content_type {
        Some(value) => {
            builder = builder.header("content-type", value);
        }
        None => {
            builder = builder.header("content-type", "application/json");
        }
    }
    builder
        .body(Body::from(text))
        .unwrap_or_else(|_| internal_error())
}

async fn market_restore_status(
    State(state): State<Arc<BackupState>>,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = guard_auth(&state.auth, &headers, false) {
        return response;
    }
    let response = match state
        .http
        .get(format!("{}/v1/admin/restore", state.market_url))
        .header("x-api-key", &state.market_key)
        .send()
        .await
    {
        Ok(value) => value,
        Err(_) => return internal_error(),
    };
    let status = response.status();
    let content_type = response.headers().get("content-type").cloned();
    let text = response.text().await.unwrap_or_default();
    let mut builder = Response::builder()
        .status(StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY));
    match content_type {
        Some(value) => {
            builder = builder.header("content-type", value);
        }
        None => {
            builder = builder.header("content-type", "application/json");
        }
    }
    builder
        .body(Body::from(text))
        .unwrap_or_else(|_| internal_error())
}

pub fn backup_router(state: Arc<BackupState>) -> Router {
    let max = state.manager.max_upload_bytes.max(0) as usize;
    let market_max = state.market_max_upload_bytes.max(0) as usize;
    Router::new()
        .route("/v1/admin/backup", get(download_backup))
        .route(
            "/v1/admin/restore",
            get(restore_status)
                .post(upload_restore)
                .layer(axum::extract::DefaultBodyLimit::max(max)),
        )
        .route("/v1/admin/market-backup", get(market_backup))
        .route(
            "/v1/admin/market-restore",
            get(market_restore_status)
                .post(market_restore_upload)
                .layer(axum::extract::DefaultBodyLimit::max(market_max)),
        )
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuid_matches_v4_shape() {
        let value = random_uuid();
        assert_eq!(value.len(), 36);
        let parts: Vec<&str> = value.split('-').collect();
        assert_eq!(
            parts.iter().map(|part| part.len()).collect::<Vec<_>>(),
            vec![8, 4, 4, 4, 12]
        );
        assert!(
            value
                .chars()
                .all(|char| char.is_ascii_hexdigit() || char == '-')
        );
        assert!(value.starts_with(|char| char != '0') || value.starts_with('0'));
        assert_eq!(&value[14..15], "4");
    }

    #[test]
    fn stage_error_mapping_matches_ts() {
        assert_eq!(
            map_stage_error("restore already in progress"),
            Some(("restore_already_pending", StatusCode::CONFLICT))
        );
        assert_eq!(
            map_stage_error("target has pending restore"),
            Some(("restore_already_pending", StatusCode::CONFLICT))
        );
        assert_eq!(
            map_stage_error("insufficient free space on device"),
            Some((
                "insufficient_restore_space",
                StatusCode::INSUFFICIENT_STORAGE
            ))
        );
        assert_eq!(
            map_stage_error("checksum mismatch after stage"),
            Some(("invalid_backup", StatusCode::BAD_REQUEST))
        );
        assert_eq!(
            map_stage_error("pg_restore: error detected"),
            Some(("invalid_backup", StatusCode::BAD_REQUEST))
        );
        assert_eq!(map_stage_error("node not found"), None);
    }

    #[test]
    fn tail_keeps_last_chars_only() {
        assert_eq!(tail_chars("abcdef", 3), "def");
        assert_eq!(tail_chars("ab", 5), "ab");
        assert_eq!(tail_chars("повтор", 3), "тор");
    }
}
