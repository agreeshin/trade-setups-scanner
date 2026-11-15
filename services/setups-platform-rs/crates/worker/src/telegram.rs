//! Independent, bounded Telegram relay sender. No bot credentials belong here.
use crate::WorkerError;
use futures::{Stream, StreamExt};
use reqwest::{Client, Method, Url, header::HeaderMap};
use setups_db::telegram::{
    self as db, TelegramConfig, TelegramJob, TelegramOutcome, TelegramReason, TelegramStatusUpdate,
    TelegramTestAttempt, TelegramTestResult,
};
use setups_relay_protocol::{
    self as protocol, HEADER_NONCE, HEADER_SIGNATURE, HEADER_TIMESTAMP, HEADER_VERSION,
    MAX_BODY_BYTES, MESSAGES_PATH, ResponseAuth, SecretKey,
};
use sqlx::PgPool;
use std::{
    io::Write,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

pub const TELEGRAM_RELAY_KEY_FILE: &str = "TELEGRAM_RELAY_KEY_FILE";
const DNS_TIMEOUT: Duration = Duration::from_secs(2);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const HTTP_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_DNS_ADDRESSES: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ClientFailure {
    code: &'static str,
    // A failed POST after starting transport is not evidence of no delivery.
    uncertain: bool,
}
impl ClientFailure {
    fn before(code: &'static str) -> Self {
        Self {
            code,
            uncertain: false,
        }
    }
    fn after(code: &'static str) -> Self {
        Self {
            code,
            uncertain: true,
        }
    }
}

fn signing_key() -> Result<SecretKey, ClientFailure> {
    let path = std::env::var_os(TELEGRAM_RELAY_KEY_FILE)
        .ok_or_else(|| ClientFailure::before("key_not_configured"))?;
    SecretKey::from_private_file(path).map_err(|error| ClientFailure::before(error.code()))
}

/// A relay is an HTTPS origin, never an arbitrary URL or a forward proxy.
fn relay_origin(value: &str) -> Result<Url, ClientFailure> {
    if value.len() > 2048 {
        return Err(ClientFailure::before("invalid_relay_origin"));
    }
    let url = Url::parse(value).map_err(|_| ClientFailure::before("invalid_relay_origin"))?;
    let authority = value
        .split_once("://")
        .map(|(_, tail)| tail.split('/').next().unwrap_or(""));
    if url.scheme() != "https"
        || authority.is_none_or(|authority| authority.contains('@'))
        || url.port() == Some(0)
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() != "/"
        || url.port_or_known_default().is_none()
    {
        return Err(ClientFailure::before("invalid_relay_origin"));
    }
    Ok(url)
}

fn in_v4(address: Ipv4Addr, network: [u8; 4], bits: u32) -> bool {
    u32::from(address) >> (32 - bits) == u32::from(Ipv4Addr::from(network)) >> (32 - bits)
}
fn in_v6(address: Ipv6Addr, network: Ipv6Addr, bits: u32) -> bool {
    u128::from(address) >> (128 - bits) == u128::from(network) >> (128 - bits)
}

/// Conservative public-unicast policy, including mapped IPv6 and cloud metadata.
/// Special-purpose/transition ranges are not supported relay deployment targets.
fn public_address(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(ip) => {
            let forbidden = [
                ([0, 0, 0, 0], 8),
                ([10, 0, 0, 0], 8),
                ([100, 64, 0, 0], 10),
                ([127, 0, 0, 0], 8),
                ([169, 254, 0, 0], 16),
                ([172, 16, 0, 0], 12),
                ([192, 0, 0, 0], 24),
                ([192, 0, 2, 0], 24),
                ([192, 31, 196, 0], 24),
                ([192, 52, 193, 0], 24),
                ([192, 88, 99, 0], 24),
                ([192, 168, 0, 0], 16),
                ([192, 175, 48, 0], 24),
                ([198, 18, 0, 0], 15),
                ([198, 51, 100, 0], 24),
                ([203, 0, 113, 0], 24),
                ([224, 0, 0, 0], 4),
                ([240, 0, 0, 0], 4),
                ([168, 63, 129, 16], 32),
            ];
            !forbidden
                .into_iter()
                .any(|(network, bits)| in_v4(ip, network, bits))
        }
        IpAddr::V6(ip) => {
            // Excludes unspecified, loopback, ULA, link/site-local, multicast,
            // mapped/compatible IPv4, NAT64, and unallocated address space.
            in_v6(ip, Ipv6Addr::new(0x2000, 0, 0, 0, 0, 0, 0, 0), 3)
                && !in_v6(ip, Ipv6Addr::new(0x2001, 0, 0, 0, 0, 0, 0, 0), 23)
                && !in_v6(ip, Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 0), 32)
                && !in_v6(ip, Ipv6Addr::new(0x2002, 0, 0, 0, 0, 0, 0, 0), 16)
                && !in_v6(ip, Ipv6Addr::new(0x2620, 0x4f, 0x8000, 0, 0, 0, 0, 0), 48)
                && !in_v6(ip, Ipv6Addr::new(0x3fff, 0, 0, 0, 0, 0, 0, 0), 20)
        }
    }
}

fn checked_addresses(
    addresses: impl IntoIterator<Item = SocketAddr>,
) -> Result<Vec<SocketAddr>, ClientFailure> {
    let mut checked = Vec::new();
    let mut seen = 0;
    for address in addresses {
        seen += 1;
        if seen > MAX_DNS_ADDRESSES {
            return Err(ClientFailure::before("dns_address_limit"));
        }
        if !public_address(address.ip()) {
            return Err(ClientFailure::before("relay_address_forbidden"));
        }
        if !checked.contains(&address) {
            checked.push(address);
        }
    }
    if checked.is_empty() {
        return Err(ClientFailure::before("dns_empty"));
    }
    Ok(checked)
}

struct PinnedRelay {
    origin: Url,
    addresses: Vec<SocketAddr>,
}
impl PinnedRelay {
    async fn resolve(value: &str) -> Result<Self, ClientFailure> {
        let origin = relay_origin(value)?;
        let host = origin
            .host_str()
            .ok_or_else(|| ClientFailure::before("invalid_relay_origin"))?;
        let port = origin
            .port_or_known_default()
            .ok_or_else(|| ClientFailure::before("invalid_relay_origin"))?;
        let addresses = if let Ok(ip) = host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<IpAddr>()
        {
            checked_addresses([SocketAddr::new(ip, port)])?
        } else {
            let resolved = tokio::time::timeout(DNS_TIMEOUT, tokio::net::lookup_host((host, port)))
                .await
                .map_err(|_| ClientFailure::before("dns_timeout"))?
                .map_err(|_| ClientFailure::before("dns_failed"))?;
            checked_addresses(resolved)?
        };
        Ok(Self { origin, addresses })
    }
    fn client(&self) -> Result<Client, ClientFailure> {
        let host = self
            .origin
            .host_str()
            .ok_or_else(|| ClientFailure::before("invalid_relay_origin"))?;
        Client::builder()
            .https_only(true)
            .http1_only()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(HTTP_TIMEOUT)
            .pool_max_idle_per_host(1)
            .pool_idle_timeout(Duration::from_secs(5))
            // Validate ALL answers, but pin ONE IP for the authenticated empty
            // challenge and subsequent text POST. No fallback to an unproved
            // address, even if several public DNS answers were returned.
            .resolve_to_addrs(host, &self.addresses[..1])
            .build()
            .map_err(|_| ClientFailure::before("relay_client_unavailable"))
    }
}

fn nonce() -> String {
    rand::random::<[u8; 16]>()
        .into_iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
fn single_header(headers: &HeaderMap, name: &'static str) -> Result<String, ClientFailure> {
    let mut values = headers.get_all(name).iter();
    let value = values
        .next()
        .ok_or_else(|| ClientFailure::after("relay_response_auth"))?;
    if values.next().is_some() {
        return Err(ClientFailure::after("relay_response_auth"));
    }
    value
        .to_str()
        .map(str::to_owned)
        .map_err(|_| ClientFailure::after("relay_response_auth"))
}
fn response_auth(headers: &HeaderMap) -> Result<ResponseAuth, ClientFailure> {
    if headers.get_all(HEADER_TIMESTAMP).iter().count() > 1 {
        return Err(ClientFailure::after("relay_response_auth"));
    }
    Ok(ResponseAuth {
        version: single_header(headers, HEADER_VERSION)?,
        nonce: single_header(headers, HEADER_NONCE)?,
        signature: single_header(headers, HEADER_SIGNATURE)?,
    })
}
async fn bounded_body<S, B, E>(stream: S) -> Result<Vec<u8>, ClientFailure>
where
    S: Stream<Item = Result<B, E>>,
    B: AsRef<[u8]>,
{
    futures::pin_mut!(stream);
    let mut body = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| ClientFailure::after("relay_response_transport"))?;
        if body
            .len()
            .checked_add(chunk.as_ref().len())
            .is_none_or(|size| size > MAX_BODY_BYTES)
        {
            return Err(ClientFailure::after("relay_response_too_large"));
        }
        body.extend_from_slice(chunk.as_ref());
    }
    Ok(body)
}

struct SignedReply {
    status: u16,
    body: Vec<u8>,
}

/// One short-lived client/IP per attempt, shared by the empty challenge and POST.
struct RelaySession<'key> {
    origin: Url,
    client: Client,
    key: &'key SecretKey,
}
struct AuthenticatedRelay<'a, 'key> {
    session: &'a RelaySession<'key>,
    status: protocol::RelayStatus,
}
impl<'key> RelaySession<'key> {
    async fn connect(origin: &str, key: &'key SecretKey) -> Result<Self, ClientFailure> {
        let relay = PinnedRelay::resolve(origin).await?;
        let client = relay.client()?;
        Ok(Self {
            origin: relay.origin,
            client,
            key,
        })
    }
    async fn authenticate(&self) -> Result<AuthenticatedRelay<'_, 'key>, ClientFailure> {
        let reply = self
            .request(Method::GET, protocol::STATUS_PATH, &[], None)
            .await
            .map_err(|error| ClientFailure::before(error.code))?;
        if reply.status != 200 {
            return Err(ClientFailure::before("relay_status_unavailable"));
        }
        let status: protocol::RelayStatus = serde_json::from_slice(&reply.body)
            .map_err(|_| ClientFailure::before("invalid_relay_status"))?;
        status
            .validate()
            .map_err(|_| ClientFailure::before("invalid_relay_status"))?;
        Ok(AuthenticatedRelay {
            session: self,
            status,
        })
    }
    async fn request(
        &self,
        method: Method,
        path: &'static str,
        body: &[u8],
        expires_at: Option<i64>,
    ) -> Result<SignedReply, ClientFailure> {
        if body.len() > MAX_BODY_BYTES {
            return Err(ClientFailure::before("relay_body_too_large"));
        }
        let mut url = self.origin.clone();
        url.set_path(path);
        let now = chrono::Utc::now().timestamp();
        // Inclusive Telegram TTL; existing Web Push semantics stay unchanged.
        if expires_at.is_some_and(|expiry| now > expiry) {
            return Err(ClientFailure::before("expired"));
        }
        let nonce = nonce();
        let auth = protocol::sign_request(self.key, method.as_str(), path, now, &nonce, body)
            .map_err(|_| ClientFailure::before("invalid_relay_request"))?;
        let mut request = self.client.request(method, url);
        for (name, value) in auth.headers() {
            request = request.header(name, value);
        }
        if path == MESSAGES_PATH {
            request = request
                .header("content-type", "application/json")
                .body(body.to_vec());
        }
        let response = request
            .send()
            .await
            .map_err(|_| ClientFailure::after("relay_transport"))?;
        let status = response.status().as_u16();
        let auth = response_auth(response.headers())?;
        if response
            .content_length()
            .is_some_and(|size| size > MAX_BODY_BYTES as u64)
        {
            return Err(ClientFailure::after("relay_response_too_large"));
        }
        let body = bounded_body(response.bytes_stream()).await?;
        protocol::verify_response(self.key, &nonce, status, &auth, &body)
            .map_err(|_| ClientFailure::after("relay_response_auth"))?;
        Ok(SignedReply { status, body })
    }
}
impl AuthenticatedRelay<'_, '_> {
    async fn post(&self, body: &[u8], expires_at: i64) -> Result<SignedReply, ClientFailure> {
        if !self.status.telegram_ready {
            return Err(ClientFailure::before("telegram_not_ready"));
        }
        // This capability only exists after a valid MAC to an empty GET: a
        // merely valid HTTPS host cannot receive confidential setup/test text.
        self.session
            .request(Method::POST, MESSAGES_PATH, body, Some(expires_at))
            .await
    }
}

trait RelayAccess {
    fn key(&self) -> Result<SecretKey, ClientFailure>;
    async fn connect<'key>(
        &self,
        origin: &str,
        key: &'key SecretKey,
    ) -> Result<RelaySession<'key>, ClientFailure>;
}
struct SecureRelayAccess;
impl RelayAccess for SecureRelayAccess {
    fn key(&self) -> Result<SecretKey, ClientFailure> {
        signing_key()
    }
    async fn connect<'key>(
        &self,
        origin: &str,
        key: &'key SecretKey,
    ) -> Result<RelaySession<'key>, ClientFailure> {
        RelaySession::connect(origin, key).await
    }
}

/// Local-only key presence/validity check; never resolves DNS or contacts relay.
pub fn key_presence() -> bool {
    signing_key().is_ok()
}

fn reason(code: &str) -> TelegramReason {
    // Only bounded local codes are passed here, never transport errors/URLs.
    TelegramReason::new(code, "Telegram relay operation could not be completed.")
}
fn failed_status(code: &str) -> TelegramStatusUpdate {
    TelegramStatusUpdate {
        relay_authenticated: false,
        telegram_ready: None,
        reason: Some(reason(code)),
    }
}
fn authenticated_status(status: &protocol::RelayStatus) -> TelegramStatusUpdate {
    TelegramStatusUpdate {
        relay_authenticated: true,
        telegram_ready: Some(status.telegram_ready),
        reason: (!status.telegram_ready).then(|| reason("telegram_not_ready")),
    }
}

fn readiness_upstream_code(error: Option<&str>) -> &'static str {
    // Even authenticated, protocol-valid strings must not enter logs verbatim.
    match error {
        Some("telegram_timeout") => "telegram_timeout",
        Some("telegram_unavailable") => "telegram_unavailable",
        Some("telegram_invalid_response") => "telegram_invalid_response",
        Some("telegram_permission_denied") => "telegram_permission_denied",
        Some("telegram_rejected") => "telegram_rejected",
        Some("telegram_rate_limited") => "telegram_rate_limited",
        Some(_) => "other",
        None => "none",
    }
}

fn readiness_diagnostic(
    job: &TelegramJob,
    status: &protocol::RelayStatus,
    observed_unix: i64,
) -> Option<String> {
    if status.telegram_ready {
        return None;
    }
    // Fixed ASCII labels, numeric scalars and static codes fit one <=512-byte
    // line. The temporary string is dropped after this attempt's observation.
    let remaining_s = job.expires_at_unix.saturating_sub(observed_unix);
    let upstream_code = readiness_upstream_code(status.error.as_deref());
    Some(format!(
        "[telegram-delivery] phase=readiness result=unready job={} setup={} attempt={} settings_revision={} signal_unix={} expiry_unix={} observed_unix={} relay_checked_unix={} remaining_s={remaining_s} upstream_code={upstream_code}",
        job.id,
        job.setup_id,
        job.attempts,
        job.settings_revision,
        job.raw_close_unix,
        job.expires_at_unix,
        observed_unix,
        status.checked_at,
    ))
}

/// Transport-only API adapter. Admission and revision-CAS persistence belong to
/// the API's db::begin_probe / db::record_status boundary. Never sends text.
pub async fn probe_telegram_relay(config: &TelegramConfig) -> TelegramStatusUpdate {
    probe_via(config, &SecureRelayAccess).await
}
async fn probe_via(config: &TelegramConfig, access: &impl RelayAccess) -> TelegramStatusUpdate {
    let attempt = async {
        let key = access.key()?;
        let session = access.connect(&config.endpoint_url, &key).await?;
        let authenticated = session.authenticate().await?;
        Ok::<_, ClientFailure>(authenticated_status(&authenticated.status))
    };
    match tokio::time::timeout(Duration::from_secs(8), attempt).await {
        Ok(Ok(status)) => status,
        Ok(Err(error)) => failed_status(error.code),
        Err(_) => failed_status("probe_timeout"),
    }
}

#[derive(Debug, PartialEq, Eq)]
enum DeliveryResult {
    Sent,
    Retry {
        code: &'static str,
        after: Option<u32>,
        uncertain: bool,
    },
    Failed(&'static str),
}
fn classify_reply(reply: SignedReply, expected_id: &str) -> DeliveryResult {
    if reply.status == 200 {
        let parsed = serde_json::from_slice::<protocol::RelayMessageResponse>(&reply.body);
        return match parsed {
            Ok(response) if response.validate().is_ok() && response.id == expected_id => {
                DeliveryResult::Sent
            }
            _ => DeliveryResult::Retry {
                code: "invalid_relay_receipt",
                after: None,
                uncertain: true,
            },
        };
    }
    let error = serde_json::from_slice::<protocol::RelayErrorResponse>(&reply.body)
        .ok()
        .filter(|value| value.validate().is_ok());
    let after = error.as_ref().and_then(|value| value.retry_after);
    match reply.status {
        403 => DeliveryResult::Failed("telegram_permission_denied"),
        400 | 401 | 413 | 422 => DeliveryResult::Failed("relay_message_rejected"),
        409 if error.as_ref().is_some_and(|value| {
            matches!(
                value.code.as_str(),
                "id_conflict"
                    | "id_body_conflict"
                    | "body_conflict"
                    | "receipt_conflict"
                    | "message_conflict"
            )
        }) =>
        {
            DeliveryResult::Failed("relay_message_conflict")
        }
        429 => DeliveryResult::Retry {
            code: "telegram_rate_limited",
            after,
            uncertain: false,
        },
        409 => DeliveryResult::Retry {
            code: "relay_busy",
            after,
            uncertain: true,
        },
        _ => DeliveryResult::Retry {
            code: "relay_unavailable",
            after,
            uncertain: true,
        },
    }
}
fn test_result(
    request_id: &str,
    result: Result<DeliveryResult, ClientFailure>,
) -> TelegramTestResult {
    let (state, code) = match result {
        Ok(DeliveryResult::Sent) => ("sent", None),
        Ok(DeliveryResult::Failed(code)) => ("error", Some(code)),
        Ok(DeliveryResult::Retry {
            code, uncertain, ..
        }) => (if uncertain { "unknown" } else { "error" }, Some(code)),
        Err(error) => (
            if error.uncertain { "unknown" } else { "error" },
            Some(error.code),
        ),
    };
    TelegramTestResult {
        request_id: request_id.into(),
        state: state.into(),
        reason: code.map(reason),
    }
}

/// Exactly one explicit test attempt. Uses the DB's frozen body, ID and expiry;
/// callers own admission/idempotency and record_test's settings-revision fence.
pub async fn test_telegram_relay(attempt: &TelegramTestAttempt) -> TelegramTestResult {
    test_via(attempt, &SecureRelayAccess).await
}
async fn test_via(attempt: &TelegramTestAttempt, access: &impl RelayAccess) -> TelegramTestResult {
    let send = async {
        let message =
            protocol::RelayMessage::parse(&attempt.request_body, chrono::Utc::now().timestamp())
                .map_err(|error| ClientFailure::before(error.code()))?;
        if message.id != attempt.delivery_id || message.expires_at != attempt.expires_at_unix {
            return Err(ClientFailure::before("invalid_test_identity"));
        }
        let key = access.key()?;
        let session = access.connect(&attempt.config.endpoint_url, &key).await?;
        let authenticated = session.authenticate().await?;
        let reply = authenticated
            .post(&attempt.request_body, attempt.expires_at_unix)
            .await?;
        Ok(classify_reply(reply, &attempt.delivery_id))
    };
    let result = tokio::time::timeout(Duration::from_secs(13), send)
        .await
        .unwrap_or_else(|_| Err(ClientFailure::after("delivery_unconfirmed")));
    test_result(&attempt.request_id, result)
}

fn render_message(job: &TelegramJob, now: i64) -> Result<Vec<u8>, ClientFailure> {
    let instrument: crate::adaptive::AdaptiveInstrument =
        serde_json::from_value(job.payload["instrument"].clone())
            .map_err(|_| ClientFailure::before("invalid_saved_payload"))?;
    let raw: setups_domain::SpikeSetup = serde_json::from_value(job.payload["raw"].clone())
        .map_err(|_| ClientFailure::before("invalid_saved_payload"))?;
    let name = job.payload["externalName"]
        .as_str()
        .filter(|name| {
            !name.trim().is_empty()
                && name.chars().count() <= 120
                && name.len() <= 480
                && !name.chars().any(char::is_control)
        })
        .ok_or_else(|| ClientFailure::before("invalid_saved_payload"))?;
    if raw.close_time_unix != job.raw_close_unix || raw.algorithm_id != job.algorithm_id {
        return Err(ClientFailure::before("invalid_saved_payload"));
    }
    let recommendation = &job.payload["recommendation"];
    let text = if job.slot_id.is_none()
        && job.generation_id.is_none()
        && job.config_revision.is_none()
        && recommendation.is_null()
        && let Some(fence) = &job.fixed_config
    {
        if fence["algorithmVersion"] != raw.algorithm_version
            || fence["params"]
                != serde_json::to_value(&raw.params)
                    .map_err(|_| ClientFailure::before("invalid_saved_payload"))?
        {
            return Err(ClientFailure::before("invalid_saved_payload"));
        }
        let time = chrono::DateTime::from_timestamp(raw.close_time_unix, 0)
            .ok_or_else(|| ClientFailure::before("invalid_saved_payload"))?;
        format!(
            "WATCH {} · {}\n{} · {}",
            instrument.symbol,
            name,
            instrument.exchange,
            time.format("%Y-%m-%d %H:%M:%S UTC")
        )
    } else if job.slot_id.is_some()
        && job.generation_id.is_some()
        && job.config_revision.is_some()
        && job.fixed_config.is_none()
        && recommendation["externalName"] == job.payload["externalName"]
    {
        crate::adaptive::recommendation_message(&instrument, &raw, recommendation)
            .map_err(|_| ClientFailure::before("invalid_saved_payload"))?
    } else {
        return Err(ClientFailure::before("invalid_saved_payload"));
    };
    protocol::RelayMessage {
        version: protocol::VERSION,
        id: job.delivery_id.clone(),
        text,
        expires_at: job.expires_at_unix,
        attachment: None,
    }
    .encode(now)
    .map_err(|error| ClientFailure::before(error.code()))
}
fn retry_outcome(
    job: &TelegramJob,
    code: &'static str,
    after: Option<u32>,
    _now: i64,
) -> TelegramOutcome {
    let backoff = 2_u32.pow(job.attempts.clamp(0, 5) as u32).min(60);
    // Honor a verified server delay even when it outlives this job. The durable
    // expiry is never extended; idle cleanup expires it instead of retrying
    // earlier than Telegram allowed just to squeeze in before the deadline.
    let delay = after.map_or(backoff, |seconds| {
        seconds.clamp(1, protocol::MAX_RETRY_AFTER_SECONDS)
    });
    TelegramOutcome::Retry {
        delay_seconds: i64::from(delay),
        reason: reason(code),
    }
}
fn disabled(code: &str) -> TelegramOutcome {
    TelegramOutcome::Disabled {
        reason: reason(code),
    }
}

async fn send_job(
    pool: &PgPool,
    worker: &str,
    job: &TelegramJob,
    stopped: &AtomicBool,
) -> Result<TelegramOutcome, WorkerError> {
    send_job_via(pool, worker, job, stopped, &SecureRelayAccess).await
}
async fn send_job_via(
    pool: &PgPool,
    worker: &str,
    job: &TelegramJob,
    stopped: &AtomicBool,
    access: &impl RelayAccess,
) -> Result<TelegramOutcome, WorkerError> {
    let now = chrono::Utc::now().timestamp();
    if now > job.expires_at_unix {
        return Ok(retry_outcome(job, "expired", Some(1), now));
    }
    if stopped.load(Ordering::SeqCst) || !db::permitted(pool, worker, job).await? {
        return Ok(disabled("delivery_fenced"));
    }
    let raw_body = if let Some(body) = &job.request_body {
        body.clone()
    } else {
        match render_message(job, now) {
            Ok(body) => body,
            Err(error) => {
                return Ok(TelegramOutcome::Failed {
                    reason: reason(error.code),
                });
            }
        }
    };
    // Freeze before DNS/HTTP. Always send the CAS's returned bytes, not the
    // renderer input: another claimant or older binary may own the body.
    let Some(body) = db::freeze_message(pool, worker, job, &raw_body).await? else {
        return Ok(disabled("delivery_fenced"));
    };
    let message = match protocol::RelayMessage::parse(&body, chrono::Utc::now().timestamp()) {
        Ok(message)
            if message.id == job.delivery_id && message.expires_at == job.expires_at_unix =>
        {
            message
        }
        _ => {
            return Ok(TelegramOutcome::Failed {
                reason: reason("invalid_frozen_message"),
            });
        }
    };
    let key = match access.key() {
        Ok(key) => key,
        Err(error) => return Ok(retry_outcome(job, error.code, None, now)),
    };
    let session = match access.connect(&job.endpoint_url, &key).await {
        Ok(session) => session,
        Err(error) => return Ok(retry_outcome(job, error.code, None, now)),
    };
    let authenticated = match session.authenticate().await {
        Ok(authenticated) => authenticated,
        Err(error) => {
            db::record_status(pool, job.settings_revision, &failed_status(error.code)).await?;
            return Ok(retry_outcome(
                job,
                error.code,
                None,
                chrono::Utc::now().timestamp(),
            ));
        }
    };
    let observed_unix = chrono::Utc::now().timestamp();
    if let Some(line) = readiness_diagnostic(job, &authenticated.status, observed_unix) {
        // Same lifecycle sink, but diagnostics must not fail delivery on I/O.
        let _ = writeln!(std::io::stderr().lock(), "{line}");
    }
    db::record_status(
        pool,
        job.settings_revision,
        &authenticated_status(&authenticated.status),
    )
    .await?;
    // Stop, settings/config and normal-promotion semantics stay DB-owned. The
    // second check is AFTER the challenge and immediately before the text POST.
    if stopped.load(Ordering::SeqCst)
        || !db::permitted(pool, worker, job).await?
        || !db::renew(pool, worker, job).await?
    {
        return Ok(disabled("delivery_fenced"));
    }
    let result = match authenticated.post(&body, message.expires_at).await {
        Ok(reply) => classify_reply(reply, &job.delivery_id),
        Err(error) => DeliveryResult::Retry {
            code: error.code,
            after: None,
            uncertain: error.uncertain,
        },
    };
    Ok(match result {
        DeliveryResult::Sent => TelegramOutcome::Sent,
        DeliveryResult::Failed(code) => TelegramOutcome::Failed {
            reason: reason(code),
        },
        DeliveryResult::Retry { code, after, .. } => {
            retry_outcome(job, code, after, chrono::Utc::now().timestamp())
        }
    })
}

async fn process_job(pool: &PgPool, worker: &str, job: Option<TelegramJob>, stopped: &AtomicBool) {
    let Some(job) = job else {
        return;
    };
    let started = std::time::Instant::now();
    let outcome = match tokio::time::timeout(
        Duration::from_secs(22),
        send_job(pool, worker, &job, stopped),
    )
    .await
    {
        Ok(Ok(outcome)) => outcome,
        Ok(Err(_)) => retry_outcome(
            &job,
            "delivery_database_error",
            None,
            chrono::Utc::now().timestamp(),
        ),
        Err(_) => retry_outcome(
            &job,
            "delivery_unconfirmed",
            None,
            chrono::Utc::now().timestamp(),
        ),
    };
    let code = match &outcome {
        TelegramOutcome::Sent => "sent",
        TelegramOutcome::Disabled { .. } => "disabled",
        TelegramOutcome::Retry { .. } => "retry",
        TelegramOutcome::Failed { .. } => "failed",
    };
    match tokio::time::timeout(
        Duration::from_secs(3),
        db::finish(pool, worker, &job, outcome),
    )
    .await
    {
        Ok(Ok(true)) => eprintln!(
            "[telegram-delivery] job={} outcome={code} elapsed_ms={}",
            job.id,
            started.elapsed().as_millis()
        ),
        Ok(Ok(false)) => eprintln!("[telegram-delivery] job={} outcome=lease_lost", job.id),
        _ => eprintln!("[telegram-delivery] job={} code=finish_unconfirmed", job.id),
    }
}

/// Two borrowed in-flight jobs, no detached job tasks or RAM queue. Every job,
/// raw body, key, client, DNS result and response is dropped at its turn's end.
/// DNS answers, payload and bodies are bounded per job. Parsing, DB readback and
/// transport can retain additional bounded copies; this is not a two-buffer peak.
pub(crate) async fn run(pool: PgPool, stopped: Arc<AtomicBool>, notify: Arc<tokio::sync::Notify>) {
    let worker = format!("telegram-delivery-{}", std::process::id());
    while !stopped.load(Ordering::SeqCst) {
        match tokio::time::timeout(Duration::from_secs(3), db::claim(&pool, &worker, 2)).await {
            Ok(Ok(jobs)) => {
                let mut jobs = jobs.into_iter();
                tokio::join!(
                    process_job(&pool, &worker, jobs.next(), &stopped),
                    process_job(&pool, &worker, jobs.next(), &stopped)
                );
            }
            _ => eprintln!("[telegram-delivery] code=claim_unavailable"),
        }
        let jitter = u64::from(rand::random::<u8>()) % 101;
        tokio::select! {_=notify.notified()=>{},_=tokio::time::sleep(Duration::from_millis(500+jitter))=>{}}
    }
}

#[cfg(test)]
#[path = "telegram_tests.rs"]
mod tests;
