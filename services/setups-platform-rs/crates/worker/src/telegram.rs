//! Independent, bounded Telegram relay sender. No bot credentials belong here.
use crate::WorkerError;
use futures::{Stream, StreamExt};
use reqwest::{Client, Method, Url, header::HeaderMap};
use setups_db::telegram::{
    self as db, TelegramConfig, TelegramJob, TelegramOutcome, TelegramReason, TelegramStatusUpdate,
    TelegramTestAttempt, TelegramTestResult,
};
use setups_relay_protocol::v2;
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
    async fn authenticate_v2(&self) -> Result<AuthenticatedRelayV2<'_, 'key>, ClientFailure> {
        let reply = self
            .request(Method::GET, v2::STATUS_PATH, &[], None)
            .await
            .map_err(|error| ClientFailure::before(error.code))?;
        if reply.status != 200 {
            return Err(ClientFailure::before("relay_v2_unsupported"));
        }
        let status: v2::RelayStatus = serde_json::from_slice(&reply.body)
            .map_err(|_| ClientFailure::before("invalid_relay_v2_status"))?;
        status
            .validate()
            .map_err(|_| ClientFailure::before("invalid_relay_v2_status"))?;
        if !status.capabilities.iter().any(|value| value == "text")
            || !status
                .capabilities
                .iter()
                .any(|value| value == "text_entities")
        {
            return Err(ClientFailure::before("relay_v2_unsupported"));
        }
        Ok(AuthenticatedRelayV2 {
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
        let is_v2 = matches!(path, v2::STATUS_PATH | v2::MESSAGES_PATH);
        let body_limit = if is_v2 {
            v2::MAX_BODY_BYTES
        } else {
            MAX_BODY_BYTES
        };
        if body.len() > body_limit {
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
        let auth = if is_v2 {
            v2::sign_request(self.key, method.as_str(), path, now, &nonce, body)
                .map_err(|_| ClientFailure::before("invalid_relay_request"))
        } else {
            protocol::sign_request(self.key, method.as_str(), path, now, &nonce, body)
                .map_err(|_| ClientFailure::before("invalid_relay_request"))
        }?;
        let mut request = self.client.request(method, url);
        for (name, value) in auth.headers() {
            request = request.header(name, value);
        }
        if path == MESSAGES_PATH || path == v2::MESSAGES_PATH {
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
        let verified = if is_v2 {
            v2::verify_response(self.key, &nonce, status, &auth, &body)
                .map_err(|_| ClientFailure::after("relay_response_auth"))
        } else {
            protocol::verify_response(self.key, &nonce, status, &auth, &body)
                .map_err(|_| ClientFailure::after("relay_response_auth"))
        };
        verified?;
        Ok(SignedReply { status, body })
    }
}
struct AuthenticatedRelayV2<'a, 'key> {
    session: &'a RelaySession<'key>,
    status: v2::RelayStatus,
}
impl AuthenticatedRelayV2<'_, '_> {
    async fn post(&self, body: &[u8], expires_at: i64) -> Result<SignedReply, ClientFailure> {
        if !self.status.telegram_ready {
            return Err(ClientFailure::before("telegram_not_ready"));
        }
        // Reached only through a signed empty v2 GET on this same pinned client.
        self.session
            .request(Method::POST, v2::MESSAGES_PATH, body, Some(expires_at))
            .await
    }
}

/// Presentation-only seam: immutable saved metrics belong to the formatter and
/// study artifact owner, never to the relay sender. Owned buffers live for one
/// attempt and are dropped after freeze/send; no global image or payload cache.
pub struct TelegramV2Presentation {
    pub text: String,
    pub entities: Vec<v2::TextEntity>,
    pub png: Option<Vec<u8>>,
    pub unavailable_reason: Option<&'static str>,
}

struct RenderedV2 {
    body: Vec<u8>,
    diagnostic: Option<TelegramReason>,
}

fn render_v2_message(
    job: &TelegramJob,
    now: i64,
    presentation: TelegramV2Presentation,
    photo_supported: bool,
) -> Result<RenderedV2, ClientFailure> {
    // Validate full text/entities first. A bad PNG must not turn invalid text
    // into a successful fallback or silently remove required trading terms.
    let mut message = v2::RelayMessage {
        version: v2::VERSION,
        id: job.delivery_id.clone(),
        text: presentation.text,
        entities: presentation.entities,
        expires_at: job.expires_at_unix,
        attachment: None,
    };
    let text_body = message
        .encode(now)
        .map_err(|_| ClientFailure::before("invalid_v2_text"))?;
    // Fixed WATCH is ordinary text, not a missing expected adaptive chart.
    // Only an explicit unavailable reason (or a rejected supplied image) marks
    // a text-only publication as a chart fallback.
    if presentation.png.is_none() && presentation.unavailable_reason.is_none() {
        return Ok(RenderedV2 {
            body: text_body,
            diagnostic: None,
        });
    }
    let mut unavailable = presentation.unavailable_reason.unwrap_or("png_unavailable");
    if let Some(png) = presentation.png {
        if !photo_supported {
            unavailable = "relay_photo_unsupported";
        } else if v2::utf16_len(&message.text) > v2::MAX_CAPTION_UTF16_UNITS {
            unavailable = "caption_overflow";
        } else {
            // The shared protocol validates canonical base64, static RGB/RGBA
            // PNG structure, the decoded byte cap and exact 900x320 dimensions.
            // Artifact bytes are not fetched from a URL or re-rendered here.
            match v2::Attachment::from_png(&png) {
                Ok(attachment) => {
                    message.attachment = Some(attachment);
                    if let Ok(body) = message.encode(now) {
                        return Ok(RenderedV2 {
                            body,
                            diagnostic: None,
                        });
                    }
                    unavailable = "png_invalid";
                }
                Err(_) => unavailable = "png_invalid",
            }
        }
    }
    let code = match unavailable {
        "caption_overflow" => "caption_overflow",
        "relay_photo_unsupported" => "relay_photo_unsupported",
        "png_invalid" => "png_invalid",
        "png_identity_mismatch" => "png_identity_mismatch",
        "png_hash_mismatch" => "png_hash_mismatch",
        "png_lookup_unavailable" => "png_lookup_unavailable",
        "png_objective_unavailable" => "png_objective_unavailable",
        _ => "png_unavailable",
    };
    Ok(RenderedV2 {
        body: text_body,
        diagnostic: Some(TelegramReason::new(
            code,
            "Chart unavailable; delivery contains one complete formatted text message.",
        )),
    })
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
    async fn presentation(
        &self,
        pool: &PgPool,
        job: &TelegramJob,
    ) -> Result<TelegramV2Presentation, ClientFailure> {
        saved_presentation(pool, job).await
    }
}

async fn saved_presentation(
    pool: &PgPool,
    job: &TelegramJob,
) -> Result<TelegramV2Presentation, ClientFailure> {
    saved_presentation_with_lane(pool, job, None).await
}

async fn saved_presentation_with_lane(
    pool: &PgPool,
    job: &TelegramJob,
    charts: Option<&ChartRenderLane>,
) -> Result<TelegramV2Presentation, ClientFailure> {
    // Validate saved identity/name/params using the unchanged v1 path too;
    // rendering rich text must not weaken the original snapshot fence.
    let body = render_message(job, chrono::Utc::now().timestamp())?;
    let legacy = protocol::RelayMessage::parse(&body, chrono::Utc::now().timestamp())
        .map_err(|_| ClientFailure::before("invalid_saved_payload"))?;
    if job.slot_id.is_some() {
        let instrument = serde_json::from_value(job.payload["instrument"].clone())
            .map_err(|_| ClientFailure::before("invalid_saved_payload"))?;
        let raw = serde_json::from_value(job.payload["raw"].clone())
            .map_err(|_| ClientFailure::before("invalid_saved_payload"))?;
        let caption = crate::adaptive::recommendation_telegram_caption(
            &instrument,
            &raw,
            &job.payload["recommendation"],
        )
        .map_err(|_| ClientFailure::before("invalid_saved_payload"))?;
        let chart = if caption.enrichment_unavailable_reason == Some("invalid_saved_enrichment") {
            Err("png_identity_mismatch")
        } else {
            saved_chart_png(pool, job, charts).await
        };
        let (png, unavailable_reason) = match chart {
            Ok(png) => (png, None),
            Err(code) => (None, Some(code)),
        };
        return Ok(TelegramV2Presentation {
            text: caption.text,
            entities: caption.entities,
            png,
            unavailable_reason,
        });
    }
    Ok(TelegramV2Presentation {
        text: legacy.text,
        entities: Vec::new(),
        png: None,
        unavailable_reason: None,
    })
}

/// Read only the artifact named by this immutable recommendation. All decoded
/// numerical JSON (<=1 MiB) and PNG bytes (<=256 KiB) belong to this one attempt
/// and are released before its result; no current-profile lookup or cache here.
async fn saved_chart_png(
    pool: &PgPool,
    job: &TelegramJob,
    charts: Option<&ChartRenderLane>,
) -> Result<Option<Vec<u8>>, &'static str> {
    use setups_domain::{AdaptiveRecommendationSnapshot, expectancy::ExpectancyArtifact};
    let observed_presentation=match job.payload.get("chartPresentation") {
        None=>false, // Preexisting queued payloads retain their exact old contract.
        Some(serde_json::Value::String(value)) if value==setups_db::continuous::outbox::OBSERVED_CHART_PRESENTATION=>true,
        _=>return Err("png_identity_mismatch"),
    };
    let recommendation = &job.payload["recommendation"];
    let saved = &recommendation["expectancy"];
    let Some(id) = saved["artifactId"].as_i64().filter(|id| *id > 0) else {
        return Err("png_unavailable");
    };
    if saved["imageKey"].is_null() {
        return Err("png_unavailable");
    }
    let key: setups_domain::expectancy::ExpectancyImageKey =
        serde_json::from_value(saved["imageKey"].clone()).map_err(|_| "png_identity_mismatch")?;
    let snapshot: AdaptiveRecommendationSnapshot =
        serde_json::from_value(recommendation.clone()).map_err(|_| "png_identity_mismatch")?;
    let fence = ChartSourceFence::from_job(job)?;
    if saved.get("objective").is_some_and(|objective| !objective.is_null()) {
        let charts = charts.ok_or("png_objective_unavailable")?;
        let saved: crate::adaptive::AdaptiveExpectancySnapshot = serde_json::from_value(saved.clone()).map_err(|_| "png_identity_mismatch")?;
        let objective = saved.objective.as_ref().ok_or("png_identity_mismatch")?;
        let expected = setups_domain::expectancy::ExpectancyObjective::from_recommendation(&snapshot).map_err(|_| "png_identity_mismatch")?;
        let value = objective.decision_r.as_ref().copied().ok_or("png_objective_unavailable")?;
        let overlay = crate::adaptive::ExpectancyOverlayKey::for_objective(&key, job.raw_close_unix, value, expected, snapshot.provenance.observation_cutoff)
            .map_err(|_| "png_identity_mismatch")?;
        if objective.settings != expected || saved.image_key.as_ref() != Some(&key)
            || saved.overlay_key.as_ref() != Some(&overlay) || saved.forecast_at_unix != job.raw_close_unix
            || saved.artifact_id != Some(id) { return Err("png_identity_mismatch"); }
        let raw = job.raw_close_unix;
        let pool = pool.clone();
        return charts.render(move || {
            let mut artifact = tokio::runtime::Handle::current().block_on(setups_db::expectancy::get(&pool, &fence.owner_id, id))
                .map_err(|_| "png_lookup_unavailable")?.ok_or("png_objective_unavailable")?;
            let numerical: ExpectancyArtifact = serde_json::from_value(std::mem::take(&mut artifact.numerical_snapshot))
                .map_err(|_| "png_identity_mismatch")?;
            validate_chart_source(&fence, id, &key, &snapshot, &artifact, &numerical)?;
            let observed=if observed_presentation {tokio::runtime::Handle::current().block_on(chart_observed(&pool,&fence,id,&numerical))?}else{None};
            let rendered=match observed.as_deref() {
                Some(points)=>setups_analytics::expectancy_chart::render_png_objective_observed(&numerical,expected,raw,value,points),
                None=>setups_analytics::expectancy_chart::render_png_with_objective(&numerical,expected,raw,value),
            };
            rendered
                .map(Some).map_err(|_| "png_objective_unavailable")
        }).await;
    }
    if let Some(recipe) = snapshot.forecast {
        let charts = charts.ok_or("png_unavailable")?;
        let saved: crate::adaptive::AdaptiveExpectancySnapshot =
            serde_json::from_value(saved.clone()).map_err(|_| "png_identity_mismatch")?;
        let forecast = saved
            .forecast_r
            .as_ref()
            .copied()
            .ok_or("png_unavailable")?;
        let expected_overlay =
            crate::adaptive::ExpectancyOverlayKey::new(&key, job.raw_close_unix, forecast)
                .map_err(|_| "png_identity_mismatch")?;
        if saved.forecast != Some(recipe)
            || saved.image_key.as_ref() != Some(&key)
            || saved.overlay_key.as_ref() != Some(&expected_overlay)
            || saved.forecast_at_unix != job.raw_close_unix
            || saved.artifact_id != Some(id)
        {
            return Err("png_identity_mismatch");
        }
        let raw_signal_at = job.raw_close_unix;
        let pool = pool.clone();
        // The sole sender-owned lane admits one blocking render. Cancellation
        // drops a waiting closure/buffers; an already running closure keeps its
        // permit until it actually exits, so timeouts cannot multiply work.
        return charts
            .render(move || {
                // The validating DB decode also belongs to this CPU admission,
                // not the async sender or a second concurrently admitted lane.
                let mut artifact = tokio::runtime::Handle::current()
                    .block_on(setups_db::expectancy::get(&pool, &fence.owner_id, id))
                    .map_err(|_| "png_lookup_unavailable")?
                    .ok_or("png_unavailable")?;
                let numerical: ExpectancyArtifact =
                    serde_json::from_value(std::mem::take(&mut artifact.numerical_snapshot))
                        .map_err(|_| "png_identity_mismatch")?;
                validate_chart_source(&fence, id, &key, &snapshot, &artifact, &numerical)?;
                let observed=if observed_presentation {tokio::runtime::Handle::current().block_on(chart_observed(&pool,&fence,id,&numerical))?}else{None};
                let rendered=match observed.as_deref() {
                    Some(points)=>setups_analytics::expectancy_chart::render_png_model_observed(&numerical,raw_signal_at,forecast,points),
                    None=>setups_analytics::expectancy_chart::render_png_with_model(&numerical,raw_signal_at,forecast),
                };
                rendered
                .map(Some)
                .map_err(|_| "png_unavailable")
            })
            .await;
    }
    let mut artifact = setups_db::expectancy::get(pool, &job.owner_id, id)
        .await
        .map_err(|_| "png_lookup_unavailable")?
        .ok_or("png_unavailable")?;
    let numerical: ExpectancyArtifact =
        serde_json::from_value(std::mem::take(&mut artifact.numerical_snapshot))
            .map_err(|_| "png_identity_mismatch")?;
    validate_chart_source(&fence, id, &key, &snapshot, &artifact, &numerical)?;
    let observed=if observed_presentation {chart_observed(pool,&fence,id,&numerical).await?}else{None};
    if let Some(points)=observed {
        let charts=charts.ok_or("png_unavailable")?;
        // Same immutable generation fields used by the old asset renderer.
        // Render only this unfrozen future message: no version bump, cache
        // rewrite, scanner admission or requeue of historical PNG assets.
        let saved:serde_json::Value=sqlx::query_scalar("select jsonb_strip_nulls(jsonb_build_object('trendMode',g.spec->'trendMode','trendBendDays',g.spec->'trendBendDays','trendBlendLambda',g.spec->'trendBlendLambda')) from study_expectancy_artifacts a left join continuous_generations g on g.id=a.source_generation_id and g.owner_id=a.owner_id where a.id=$1 and a.owner_id=$2")
            .bind(id).bind(&fence.owner_id).fetch_one(pool).await.map_err(|_|"png_lookup_unavailable")?;
        let trend=serde_json::from_value(saved).map_err(|_|"png_identity_mismatch")?;
        return charts.render(move||setups_analytics::expectancy_chart::render_png_trend_observed(&numerical,trend,&points)
            .map(Some).map_err(|_|"png_unavailable")).await;
    }
    let version = key.render_version.to_string();
    let png = setups_db::expectancy::load_png_by_image_key(pool, &job.owner_id, &key)
        .await
        .map_err(|_| "png_lookup_unavailable")?
        .ok_or("png_unavailable")?;
    if png.len() > v2::MAX_PNG_BYTES {
        return Err("png_invalid");
    }
    if !db::chart_png_checksum_matches(pool, &job.owner_id, id, &version, &png)
        .await
        .map_err(|_| "png_lookup_unavailable")?
    {
        return Err("png_hash_mismatch");
    }
    Ok(Some(png))
}

async fn chart_observed(pool:&PgPool,fence:&ChartSourceFence,id:i64,numerical:&setups_domain::expectancy::ExpectancyArtifact)
    ->Result<Option<Vec<setups_domain::expectancy::UtcDailyPoint>>,&'static str> {
    let archive=setups_db::study_display::original_observed(pool,&fence.owner_id,fence.study_id,Some(id),"UTC").await
        .map_err(|error| {
            let message:String=error.to_string().chars().take(300).map(|c|if c.is_control(){' '}else{c}).collect();
            setups_db::diagnostics::emit(setups_db::diagnostics::Level::Error,"telegram_original_source_invalid",
                serde_json::json!({"studyId":fence.study_id,"artifactId":id,"message":message}));
            "png_identity_mismatch"
        })?;
    archive.map(|archive|archive.utc_points(numerical.origin_unix).map_err(|_|"png_identity_mismatch")).transpose()
}

/// Only bounded scalar source fences cross to the CPU lane, not the job's full
/// recommendation, outbox body or PNG. No live/current-profile lookup occurs.
struct ChartSourceFence {
    owner_id: String,
    generation_id: i64,
    slot_id: i64,
    study_id: i64,
}

impl ChartSourceFence {
    fn from_job(job: &TelegramJob) -> Result<Self, &'static str> {
        let recommendation = &job.payload["recommendation"];
        if recommendation["ownerId"].as_str() != Some(job.owner_id.as_str()) {
            return Err("png_identity_mismatch");
        }
        Ok(Self {
            owner_id: job.owner_id.clone(),
            generation_id: job.generation_id.ok_or("png_identity_mismatch")?,
            slot_id: job.slot_id.ok_or("png_identity_mismatch")?,
            study_id: recommendation["studyId"]
                .as_i64()
                .ok_or("png_identity_mismatch")?,
        })
    }
}

fn validate_chart_source(
    fence: &ChartSourceFence,
    id: i64,
    key: &setups_domain::expectancy::ExpectancyImageKey,
    snapshot: &setups_domain::AdaptiveRecommendationSnapshot,
    artifact: &setups_db::expectancy::Artifact,
    numerical: &setups_domain::expectancy::ExpectancyArtifact,
) -> Result<(), &'static str> {
    // This private adapter only consumes the very same owned snapshot returned
    // by db::get, whose read boundary already validates/rebuilds numerical JSON.
    let identity = &artifact.identity;
    let source = &numerical.source;
    let provenance = &snapshot.provenance;
    if artifact.id != id
        || identity.owner_id != fence.owner_id
        || identity.generation_id != fence.generation_id
        || identity.slot_id != fence.slot_id
        || identity.study_id != fence.study_id
        || identity.calculation_version != key.calculation_version.to_string()
        || identity.tp_pct != snapshot.tp_pct
        || identity.sl_pct != snapshot.sl_pct
        || (
            identity.from_unix,
            identity.to_unix,
            identity.observation_cutoff,
        ) != (
            provenance.from_unix,
            provenance.to_unix,
            provenance.observation_cutoff,
        )
        || setups_db::expectancy::artifact_key(identity).map_err(|_| "png_identity_mismatch")?
            != artifact.key
        || key.derived_snapshot_id != artifact.key
        || numerical.image_key(key.render_version) != *key
        || (key.render_version != setups_analytics::expectancy_chart::OBJECTIVE_RENDER_VERSION && key.render_version
            != if snapshot.forecast.is_some() {
                setups_analytics::expectancy_chart::PROBABILITY_RENDER_VERSION
            } else {
                1
            })
        || source.forecast != snapshot.forecast
        || identity.forecast != snapshot.forecast
        || numerical.calculation_version
            != if snapshot.forecast.is_some() {
                setups_domain::expectancy::EXPECTANCY_MODEL_CALCULATION_VERSION
            } else {
                setups_domain::expectancy::EXPECTANCY_CALCULATION_VERSION
            }
        || source.generation_id.as_deref() != Some(identity.generation_id.to_string().as_str())
        || source.profile_id.as_deref() != Some(identity.slot_id.to_string().as_str())
        || source.provenance != *provenance
        || source.direction != snapshot.direction
        || source.tp_pct != snapshot.tp_pct
        || source.sl_pct != snapshot.sl_pct
        || source.analytics.cluster_enabled != snapshot.cluster_enabled
        || source.analytics.cluster_gap_minutes != snapshot.cluster_gap_minutes
        || source.analytics.time_shift_minutes != snapshot.time_shift_minutes
        || source.analytics.price_change_pct != snapshot.price_change_pct
        || numerical
            .matrix_mean_r
            .as_ref()
            .is_some_and(|value| *value != snapshot.mean_r)
        || numerical
            .selected_cell
            .as_ref()
            .and_then(|cell| cell.raw_r.as_ref())
            .is_some_and(|value| *value != snapshot.raw_r)
        || !numerical
            .daily
            .as_ref()
            .is_some_and(|days| days.iter().any(|day| day.mean_r.is_some()))
    {
        return Err("png_identity_mismatch");
    }
    if let Some(expected) = &identity.outcome_hash {
        let daily = numerical
            .daily
            .as_ref()
            .ok_or("png_identity_mismatch")?
            .iter()
            .map(|p| setups_domain::expectancy::UtcDailyInput {
                day_start_unix: p.day_start_unix,
                row_count: p.row_count,
                tp_count: p.tp_count,
                sl_count: p.sl_count,
                mean_r: p.mean_r,
            })
            .collect::<Vec<_>>();
        if setups_db::expectancy::captured_outcome_hash(&daily)
            .map_err(|_| "png_identity_mismatch")?
            != *expected
        {
            return Err("png_identity_mismatch");
        }
    }
    Ok(())
}

struct ChartRenderLane {
    permit: Arc<tokio::sync::Semaphore>,
}

impl Default for ChartRenderLane {
    fn default() -> Self {
        Self {
            permit: Arc::new(tokio::sync::Semaphore::new(1)),
        }
    }
}

impl ChartRenderLane {
    async fn render<T: Send + 'static>(
        &self,
        render: impl FnOnce() -> Result<T, &'static str> + Send + 'static,
    ) -> Result<T, &'static str> {
        let permit = self
            .permit
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| "png_unavailable")?;
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            render()
        })
        .await
        .map_err(|_| "png_unavailable")?
    }
}

#[derive(Default)]
struct SecureRelayAccess {
    charts: ChartRenderLane,
}
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
    async fn presentation(
        &self,
        pool: &PgPool,
        job: &TelegramJob,
    ) -> Result<TelegramV2Presentation, ClientFailure> {
        saved_presentation_with_lane(pool, job, Some(&self.charts)).await
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
    probe_via(config, &SecureRelayAccess::default()).await
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
fn classify_v2_reply(reply: SignedReply, expected_id: &str) -> DeliveryResult {
    if reply.status == 200 {
        return match serde_json::from_slice::<v2::RelayMessageResponse>(&reply.body) {
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
    // Error codes and retry policy are identical, but response version must be
    // validated as v2 before adapting into the unchanged legacy classifier.
    let parsed = serde_json::from_slice::<v2::RelayErrorResponse>(&reply.body)
        .ok()
        .filter(|value| value.validate().is_ok());
    let body = parsed.map(|error| serde_json::json!({"version":1,"code":error.code,"retryAfter":error.retry_after}))
        .and_then(|body| serde_json::to_vec(&body).ok()).unwrap_or_default();
    classify_reply(
        SignedReply {
            status: reply.status,
            body,
        },
        expected_id,
    )
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
    test_via(attempt, &SecureRelayAccess::default()).await
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

/// Pure saved-WATCH presentation, also used by the read-only setup detail API.
/// No delivery, clock, settings or current algorithm values are consulted.
pub fn fixed_watch_text(symbol: &str, name: &str, exchange: &str, close_unix: i64) -> Result<String, &'static str> {
    if name.trim().is_empty() || name.chars().count() > 120 || name.len() > 480 || name.chars().any(char::is_control) {
        return Err("invalid_saved_payload");
    }
    let time = chrono::DateTime::from_timestamp(close_unix, 0).ok_or("invalid_saved_payload")?;
    Ok(format!("WATCH {symbol} · {name}\n{exchange} · {}", time.format("%Y-%m-%d %H:%M:%S UTC")))
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
        fixed_watch_text(&instrument.symbol, name, &instrument.exchange, raw.close_time_unix)
            .map_err(ClientFailure::before)?
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
    access: &SecureRelayAccess,
) -> Result<TelegramOutcome, WorkerError> {
    send_job_via(pool, worker, job, stopped, access).await
}
async fn send_job_via(
    pool: &PgPool,
    worker: &str,
    job: &TelegramJob,
    stopped: &AtomicBool,
    access: &impl RelayAccess,
) -> Result<TelegramOutcome, WorkerError> {
    match job.wire_version {
        1 => send_v1_job_via(pool, worker, job, stopped, access).await,
        2 => send_v2_job_via(pool, worker, job, stopped, access).await,
        _ => Ok(TelegramOutcome::Failed {
            reason: reason("unsupported_wire_version"),
        }),
    }
}

async fn send_v1_job_via(
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

async fn send_v2_job_via(
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
    let key = match access.key() {
        Ok(key) => key,
        Err(error) => return Ok(retry_outcome(job, error.code, None, now)),
    };
    let session = match access.connect(&job.endpoint_url, &key).await {
        Ok(session) => session,
        Err(error) => return Ok(retry_outcome(job, error.code, None, now)),
    };
    // Prove exact v2 route/version/capabilities before selecting an unfrozen
    // photo/text form. Unsupported relays never receive confidential content;
    // existing v1 work uses the separate unchanged route, not a v2 downgrade.
    let authenticated = match session.authenticate_v2().await {
        Ok(value) => value,
        Err(error) => {
            db::record_status(pool, job.settings_revision, &failed_status(error.code)).await?;
            return Ok(retry_outcome(job, error.code, None, now));
        }
    };
    let status = TelegramStatusUpdate {
        relay_authenticated: true,
        telegram_ready: Some(authenticated.status.telegram_ready),
        reason: (!authenticated.status.telegram_ready).then(|| reason("telegram_not_ready")),
    };
    db::record_status(pool, job.settings_revision, &status).await?;
    let (raw_body, diagnostic) = if let Some(body) = &job.request_body {
        (body.clone(), None)
    } else {
        let content = match access.presentation(pool, job).await {
            Ok(content) => content,
            Err(error) => {
                return Ok(TelegramOutcome::Failed {
                    reason: reason(error.code),
                });
            }
        };
        let photo_supported = authenticated
            .status
            .capabilities
            .iter()
            .any(|value| value == "photo_png");
        match render_v2_message(
            job,
            chrono::Utc::now().timestamp(),
            content,
            photo_supported,
        ) {
            Ok(rendered) => (rendered.body, rendered.diagnostic),
            Err(error) => {
                return Ok(TelegramOutcome::Failed {
                    reason: reason(error.code),
                });
            }
        }
    };
    let Some(body) =
        db::freeze_message_with_diagnostic(pool, worker, job, &raw_body, diagnostic.as_ref())
            .await?
    else {
        return Ok(disabled("delivery_fenced"));
    };
    let message = match v2::RelayMessage::parse(&body, chrono::Utc::now().timestamp()) {
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
    if message.attachment.is_some()
        && !authenticated
            .status
            .capabilities
            .iter()
            .any(|value| value == "photo_png")
    {
        return Ok(retry_outcome(job, "relay_photo_unsupported", None, now));
    }
    if stopped.load(Ordering::SeqCst)
        || !db::permitted(pool, worker, job).await?
        || !db::renew(pool, worker, job).await?
    {
        return Ok(disabled("delivery_fenced"));
    }
    let result = match authenticated.post(&body, message.expires_at).await {
        Ok(reply) => classify_v2_reply(reply, &job.delivery_id),
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

async fn process_job(
    pool: &PgPool,
    worker: &str,
    job: Option<TelegramJob>,
    stopped: &AtomicBool,
    access: &SecureRelayAccess,
) {
    let Some(job) = job else {
        return;
    };
    let started = std::time::Instant::now();
    let outcome = match tokio::time::timeout(
        Duration::from_secs(22),
        send_job(pool, worker, &job, stopped, access),
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
    let access = SecureRelayAccess::default();
    while !stopped.load(Ordering::SeqCst) {
        match tokio::time::timeout(Duration::from_secs(3), db::claim(&pool, &worker, 2)).await {
            Ok(Ok(jobs)) => {
                let mut jobs = jobs.into_iter();
                tokio::join!(
                    process_job(&pool, &worker, jobs.next(), &stopped, &access),
                    process_job(&pool, &worker, jobs.next(), &stopped, &access)
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
