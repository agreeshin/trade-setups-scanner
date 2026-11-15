use crate::{
    RelayError,
    store::{Claim, Completion, PostAttempt, Store},
    telegram::{Telegram, TelegramError},
};
use axum::{
    Router,
    body::{Body, to_bytes},
    extract::{Request, State},
    http::{HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::any,
};
use protocol::{
    RelayErrorResponse, RelayMessage, RelayMessageResponse, RelayOutcome, RelayStatus, RequestAuth,
    SecretKey,
};
use serde::Serialize;
use setups_relay_protocol as protocol;
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::{Mutex, Semaphore};

type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

/// Wire versions share durable receipt/nonce state, never reinterpret one body.
/// A request owns its bounded message until the Telegram call completes; no
/// second payload queue or process-wide image cache is retained by the relay.
enum DeliveryMessage {
    V1(RelayMessage),
    V2(protocol::v2::RelayMessage, Option<Vec<u8>>),
}
impl DeliveryMessage {
    fn rich(message: protocol::v2::RelayMessage) -> Result<Self, protocol::v2::Error> {
        // Decode/validate before receipt admission and the final TTL check, not
        // between that check and Telegram egress. The one <=256KiB buffer is
        // moved into multipart and released on completion/error/cancellation.
        let png = message
            .attachment
            .as_ref()
            .map(protocol::v2::Attachment::decode_png)
            .transpose()?;
        Ok(Self::V2(message, png))
    }
    fn id(&self) -> &str {
        match self {
            Self::V1(message) => &message.id,
            Self::V2(message, _) => &message.id,
        }
    }
    fn version(&self) -> u8 {
        match self {
            Self::V1(_) => 1,
            Self::V2(..) => 2,
        }
    }
    fn expires_at(&self) -> i64 {
        match self {
            Self::V1(message) => message.expires_at,
            Self::V2(message, _) => message.expires_at,
        }
    }
    fn validate_expiry(&self, now: i64) -> Result<(), &'static str> {
        match self {
            Self::V1(message) => message.validate_expiry(now).map_err(|error| error.code()),
            Self::V2(message, _) => message.validate_expiry(now).map_err(|error| error.code()),
        }
    }
    async fn send(self, telegram: &Telegram) -> Result<i64, TelegramError> {
        match self {
            Self::V1(message) => telegram.send(&message.text).await,
            Self::V2(message, png) => telegram.send_rich(&message, png).await,
        }
    }
}
struct Probe {
    cached: Option<(Instant, RelayStatus)>,
}
pub(crate) struct App {
    key: SecretKey,
    store: Store,
    telegram: Telegram,
    concurrency: Semaphore,
    channel: Semaphore,
    clock: Clock,
    probe: Mutex<Probe>,
    pub stopping: AtomicBool,
}
impl App {
    pub fn new(
        key: SecretKey,
        store: Store,
        telegram: Telegram,
        concurrency: usize,
        clock: Clock,
    ) -> Arc<Self> {
        Arc::new(Self {
            key,
            store,
            telegram,
            concurrency: Semaphore::new(concurrency),
            channel: Semaphore::new(1),
            clock,
            probe: Mutex::new(Probe { cached: None }),
            stopping: AtomicBool::new(false),
        })
    }
    pub fn router(self: &Arc<Self>) -> Router {
        Router::new()
            .route(protocol::MESSAGES_PATH, any(handle))
            .route(protocol::STATUS_PATH, any(handle))
            .route(protocol::v2::MESSAGES_PATH, any(handle))
            .route(protocol::v2::STATUS_PATH, any(handle))
            .with_state(self.clone())
    }
    async fn status(&self) -> Result<RelayStatus, RelayError> {
        self.store.health().await?;
        let mut probe = self.probe.lock().await;
        if let Some((checked, status)) = &probe.cached {
            let ttl = if status.telegram_ready { 30 } else { 5 };
            if checked.elapsed() < Duration::from_secs(ttl) {
                return Ok(status.clone());
            }
        }
        let result = self.telegram.permission().await;
        let status = RelayStatus {
            version: 1,
            capabilities: vec![protocol::CAPABILITY_TEXT.into()],
            telegram_ready: result.is_ok(),
            checked_at: (self.clock)(),
            error: result.err().map(|e| e.code().into()),
        };
        probe.cached = Some((Instant::now(), status.clone()));
        Ok(status)
    }
    fn actual_send_status(&self, result: Result<(), TelegramError>) {
        // Never wait behind an external status probe after committing a receipt.
        if let Ok(mut probe) = self.probe.try_lock() {
            probe.cached = Some((
                Instant::now(),
                RelayStatus {
                    version: 1,
                    capabilities: vec![protocol::CAPABILITY_TEXT.into()],
                    telegram_ready: result.is_ok(),
                    checked_at: (self.clock)(),
                    error: result.err().map(|e| e.code().into()),
                },
            ));
        }
    }
    async fn message(
        &self,
        message: DeliveryMessage,
        hash: String,
        attempt: PostAttempt,
    ) -> Result<RelayMessageResponse, Failure> {
        let now = (self.clock)();
        let id = message.id().to_owned();
        let version = message.version();
        let claim = self
            .store
            .claim_attempt(id.clone(), hash, message.expires_at(), now, attempt)
            .await
            .map_err(Failure::storage)?;
        let epoch = match claim {
            Claim::Duplicate(telegram_id) => {
                return Ok(RelayMessageResponse {
                    version,
                    id,
                    outcome: RelayOutcome::Duplicate,
                    telegram_message_id: Some(telegram_id),
                });
            }
            Claim::Conflict => return Err(Failure::new(409, "id_body_conflict", None)),
            Claim::Busy(after) => return Err(Failure::new(409, "delivery_in_flight", Some(after))),
            Claim::Retry(code, after) => {
                return Err(Failure::new(
                    if code == "telegram_rate_limited" {
                        429
                    } else {
                        503
                    },
                    code,
                    Some(after),
                ));
            }
            Claim::Denied => return Err(Failure::new(403, "telegram_permission_denied", None)),
            Claim::Full => return Err(Failure::new(503, "receipt_capacity", Some(30))),
            Claim::AttemptFull => return Err(Failure::new(503, "attempt_capacity", Some(30))),
            Claim::Replay => return Err(Failure::new(409, "post_replay", None)),
            Claim::Acquired(epoch) => epoch,
        };
        // One external publisher for the one configured channel, without a
        // second in-process payload queue. Main retains/retries busy work.
        let Ok(_channel) = self.channel.try_acquire() else {
            self.store
                .finish(
                    id.clone(),
                    epoch,
                    Completion::Retry {
                        code: "channel_busy",
                        after: 1,
                        rate_limited: false,
                    },
                    (self.clock)(),
                )
                .await
                .map_err(Failure::storage)?;
            return Err(Failure::new(409, "channel_busy", Some(1)));
        };
        // Time spent acquiring SQLite/the HTTP permit never grants fresh TTL.
        if let Err(error) = message.validate_expiry((self.clock)()) {
            self.store
                .finish(
                    id.clone(),
                    epoch,
                    Completion::Retry {
                        code: "expired",
                        after: 0,
                        rate_limited: false,
                    },
                    (self.clock)(),
                )
                .await
                .map_err(Failure::storage)?;
            return Err(Failure::new(400, error, None));
        }
        match message.send(&self.telegram).await {
            Ok(telegram_id) => {
                self.store
                    .finish(
                        id.clone(),
                        epoch,
                        Completion::Sent(telegram_id),
                        (self.clock)(),
                    )
                    .await
                    .map_err(Failure::storage)?;
                self.actual_send_status(Ok(()));
                Ok(RelayMessageResponse {
                    version,
                    id,
                    outcome: RelayOutcome::Sent,
                    telegram_message_id: Some(telegram_id),
                })
            }
            Err(error) => {
                let (status, completion, retry) = match error {
                    TelegramError::Permission | TelegramError::Rejected => {
                        (403, Completion::Denied, None)
                    }
                    TelegramError::RateLimited(after) => (
                        429,
                        Completion::Retry {
                            code: error.code(),
                            after,
                            rate_limited: true,
                        },
                        Some(after),
                    ),
                    _ => (
                        503,
                        Completion::Retry {
                            code: error.code(),
                            after: 1,
                            rate_limited: false,
                        },
                        Some(1),
                    ),
                };
                self.store
                    .finish(id, epoch, completion, (self.clock)())
                    .await
                    .map_err(Failure::storage)?;
                self.actual_send_status(Err(error));
                Err(Failure::new(status, error.code(), retry))
            }
        }
    }
}
struct Failure {
    status: u16,
    code: &'static str,
    retry: Option<u32>,
}
impl Failure {
    fn new(status: u16, code: &'static str, retry: Option<u32>) -> Self {
        Self {
            status,
            code,
            retry,
        }
    }
    fn storage(error: RelayError) -> Self {
        Self::new(503, error.code(), Some(1))
    }
}
fn one_header(headers: &HeaderMap, name: &str) -> Option<String> {
    let mut values = headers.get_all(name).iter();
    let value = values.next()?.to_str().ok()?;
    if values.next().is_some() {
        return None;
    }
    Some(value.to_owned())
}
fn auth_headers(headers: &HeaderMap) -> Option<RequestAuth> {
    Some(RequestAuth {
        version: one_header(headers, protocol::HEADER_VERSION)?,
        timestamp: one_header(headers, protocol::HEADER_TIMESTAMP)?,
        nonce: one_header(headers, protocol::HEADER_NONCE)?,
        signature: one_header(headers, protocol::HEADER_SIGNATURE)?,
    })
}
async fn handle(State(app): State<Arc<App>>, request: Request<Body>) -> Response {
    let start = Instant::now();
    let version = if matches!(
        request.uri().path(),
        protocol::v2::STATUS_PATH | protocol::v2::MESSAGES_PATH
    ) {
        2
    } else {
        1
    };
    let Some(auth) = auth_headers(request.headers()) else {
        return unsigned_error(version, 401, "unauthorized");
    };
    let Ok(_permit) = app.concurrency.try_acquire() else {
        return unsigned_error(version, 503, "relay_busy");
    };
    let method = request.method().as_str().to_owned();
    let path = request
        .uri()
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or("")
        .to_owned();
    let body = match tokio::time::timeout(
        Duration::from_secs(2),
        to_bytes(
            request.into_body(),
            if version == 2 {
                protocol::v2::MAX_BODY_BYTES
            } else {
                protocol::MAX_BODY_BYTES
            },
        ),
    )
    .await
    {
        Err(_) => return unsigned_error(version, 503, "body_timeout"),
        Ok(Err(_)) => return unsigned_error(version, 413, "body_too_large"),
        Ok(Ok(body)) => body,
    };
    let verified = if version == 2 {
        protocol::v2::verify_request(&app.key, &method, &path, &auth, &body, (app.clock)()).is_ok()
    } else {
        protocol::verify_request(&app.key, &method, &path, &auth, &body, (app.clock)()).is_ok()
    };
    if !verified {
        return unsigned_error(version, 401, "unauthorized");
    }
    let response = if app.stopping.load(Ordering::Acquire) {
        signed_error(&app, &auth, Failure::new(503, "relay_stopping", Some(1)))
    } else if matches!(
        path.as_str(),
        protocol::STATUS_PATH | protocol::v2::STATUS_PATH
    ) {
        match app.status().await {
            Ok(mut status) => {
                // Cache readiness only; capability/version views are exact per route.
                // A v1 probe must never advertise v2 support to an old sender.
                status.version = version;
                if version == 2 {
                    status.capabilities = protocol::v2::CAPABILITIES
                        .into_iter()
                        .map(str::to_owned)
                        .collect();
                }
                signed(&app, &auth, 200, &status)
            }
            Err(error) => signed_error(&app, &auth, Failure::storage(error)),
        }
    } else {
        let parsed = if version == 2 {
            protocol::v2::RelayMessage::parse(&body, (app.clock)())
                .and_then(DeliveryMessage::rich)
                .map_err(|error| error.code())
        } else {
            RelayMessage::parse(&body, (app.clock)())
                .map(DeliveryMessage::V1)
                .map_err(|error| error.code())
        };
        // Hash the original authenticated bytes. The wire version, entities and
        // PNG stay immutable across retries; Store needs no wire schema bump.
        let hash = protocol::body_sha256(&body);
        drop(body);
        match parsed {
            Err(code) => signed_error(&app, &auth, Failure::new(400, code, None)),
            Ok(message) => match app
                .message(
                    message,
                    hash,
                    PostAttempt {
                        nonce: auth.nonce.clone(),
                        signed_timestamp: auth.timestamp.parse().expect("verified timestamp"),
                    },
                )
                .await
            {
                Ok(receipt) => signed(&app, &auth, 200, &receipt),
                Err(error) => signed_error(&app, &auth, error),
            },
        }
    };
    eprintln!(
        "relay_request status={} duration_ms={}",
        response.status().as_u16(),
        start.elapsed().as_millis()
    );
    response
}
fn signed_error(app: &App, auth: &RequestAuth, error: Failure) -> Response {
    signed(
        app,
        auth,
        error.status,
        &RelayErrorResponse {
            version: if auth.version == "2" { 2 } else { 1 },
            code: error.code.into(),
            retry_after: error.retry,
        },
    )
}
fn signed(app: &App, auth: &RequestAuth, status: u16, value: &impl Serialize) -> Response {
    let version = if auth.version == "2" { 2 } else { 1 };
    let Ok(body) = serde_json::to_vec(value) else {
        return unsigned_error(version, 500, "internal_error");
    };
    // Both wire versions keep the original 32KiB response cap.
    if body.len() > protocol::MAX_BODY_BYTES {
        return unsigned_error(version, 500, "internal_error");
    }
    let mac = if version == 2 {
        protocol::v2::sign_response(&app.key, &auth.nonce, status, &body).ok()
    } else {
        protocol::sign_response(&app.key, &auth.nonce, status, &body).ok()
    };
    let Some(mac) = mac else {
        return unsigned_error(version, 500, "internal_error");
    };
    let mut response = (
        StatusCode::from_u16(status).expect("static response status"),
        [("content-type", "application/json")],
        body,
    )
        .into_response();
    for (name, value) in mac.headers() {
        response.headers_mut().insert(
            name,
            HeaderValue::from_str(value).expect("validated MAC header"),
        );
    }
    response
}
fn unsigned_error(version: u8, status: u16, code: &'static str) -> Response {
    (
        StatusCode::from_u16(status).expect("static error status"),
        axum::Json(RelayErrorResponse {
            version,
            code: code.into(),
            retry_after: None,
        }),
    )
        .into_response()
}

#[cfg(test)]
pub(crate) mod tests;
