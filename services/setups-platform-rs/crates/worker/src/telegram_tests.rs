use super::*;
use protocol::{RelayMessage, RelayMessageResponse, RelayOutcome, VERSION};
use reqwest::header::HeaderValue;
use sha2::Digest;

#[test]
fn origin_policy_rejects_arbitrary_urls_without_exposing_them() {
    for value in [
        "http://relay.example",
        "https://user:password@relay.example",
        "https://relay.example/a",
        "https://relay.example/?token=secret",
        "https://relay.example/#secret",
        "file:///etc/passwd",
        "https://relay.example/%2f",
        "https://relay.example:invalid",
        "not a URL",
    ] {
        assert_eq!(
            relay_origin(value).unwrap_err().code,
            "invalid_relay_origin"
        );
    }
    for value in [
        "https://relay.example",
        "https://relay.example/",
        "https://relay.example:8443/",
        "https://8.8.8.8/",
        "https://[2606:4700:4700::1111]/",
    ] {
        assert!(relay_origin(value).is_ok());
    }
}

#[test]
fn public_unicast_policy_covers_metadata_mapped_transition_and_special_ranges() {
    for value in [
        "0.0.0.0",
        "0.1.2.3",
        "10.0.0.1",
        "100.64.0.1",
        "100.100.100.200",
        "100.127.255.255",
        "127.0.0.1",
        "169.254.169.254",
        "172.16.0.1",
        "172.31.255.255",
        "192.0.0.9",
        "192.0.2.1",
        "192.31.196.1",
        "192.52.193.1",
        "192.88.99.1",
        "192.168.0.1",
        "192.175.48.1",
        "198.18.0.1",
        "198.19.255.255",
        "198.51.100.1",
        "203.0.113.1",
        "224.0.0.1",
        "239.255.255.255",
        "240.0.0.1",
        "255.255.255.255",
        "168.63.129.16",
        "::",
        "::1",
        "::ffff:127.0.0.1",
        "::ffff:10.0.0.1",
        "::ffff:8.8.8.8",
        "::8.8.8.8",
        "64:ff9b::a00:1",
        "64:ff9b:1::1",
        "100::1",
        "2001::1",
        "2001:2::1",
        "2001:10::1",
        "2001:20::1",
        "2001:db8::1",
        "2002:7f00:1::1",
        "3fff::1",
        "3fff:fff::1",
        "5f00::1",
        "2620:4f:8000::1",
        "fc00::1",
        "fd00:ec2::254",
        "fe80::1",
        "fec0::1",
        "ff02::1",
    ] {
        assert!(
            !public_address(value.parse().unwrap()),
            "unexpected public classification: {value}"
        );
    }
    for value in [
        "1.1.1.1",
        "8.8.8.8",
        "100.63.255.255",
        "100.128.0.1",
        "172.15.255.255",
        "172.32.0.1",
        "93.184.216.34",
        "2606:4700:4700::1111",
        "2001:4860:4860::8888",
        "2a00:1450:4001::1",
    ] {
        assert!(
            public_address(value.parse().unwrap()),
            "unexpected rejection: {value}"
        );
    }
}

#[test]
fn all_resolved_addresses_must_pass_before_the_connection_is_pinned() {
    let good: SocketAddr = "8.8.8.8:443".parse().unwrap();
    let bad: SocketAddr = "127.0.0.1:443".parse().unwrap();
    assert_eq!(checked_addresses([good, good]).unwrap(), vec![good]);
    for addresses in [vec![good, bad], vec![bad, good], vec![bad]] {
        assert_eq!(
            checked_addresses(addresses).unwrap_err().code,
            "relay_address_forbidden"
        );
    }
    assert_eq!(checked_addresses([]).unwrap_err().code, "dns_empty");
    assert_eq!(
        checked_addresses(std::iter::repeat_n(good, MAX_DNS_ADDRESSES + 1))
            .unwrap_err()
            .code,
        "dns_address_limit"
    );
    let pinned = PinnedRelay {
        origin: relay_origin("https://relay.example:8443/").unwrap(),
        addresses: vec![good],
    };
    assert!(
        pinned.client().is_ok(),
        "the checked override builds on the actual no-proxy/no-redirect client"
    );
}

#[tokio::test]
async fn private_ip_origins_are_rejected_before_any_network_request() {
    let key = SecretKey::from_bytes([7; 32]);
    for origin in [
        "https://127.0.0.1/",
        "https://[::1]/",
        "https://0x7f000001/",
        "https://2130706433/",
        "https://[::ffff:127.0.0.1]/",
        "https://169.254.169.254/",
    ] {
        let result = RelaySession::connect(origin, &key).await;
        let error = result.err().expect("must reject, not connect");
        assert_eq!(error.code, "relay_address_forbidden");
        assert!(!error.uncertain);
    }
}

#[tokio::test]
async fn streamed_response_cap_is_checked_before_extension_and_stops_polling() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let polls = AtomicUsize::new(0);
    let chunks = futures::stream::iter([
        Ok::<_, ()>(vec![0_u8; MAX_BODY_BYTES]),
        Ok(vec![1]),
        Ok(vec![2; MAX_BODY_BYTES]),
    ])
    .inspect(|_| {
        polls.fetch_add(1, Ordering::SeqCst);
    });
    assert_eq!(
        bounded_body(chunks).await.unwrap_err().code,
        "relay_response_too_large"
    );
    assert_eq!(
        polls.load(Ordering::SeqCst),
        2,
        "must not consume the remaining unbounded stream"
    );
    assert_eq!(
        bounded_body(futures::stream::iter([Ok::<_, ()>(vec![
            0_u8;
            MAX_BODY_BYTES
        ])]))
        .await
        .unwrap()
        .len(),
        MAX_BODY_BYTES
    );
    assert_eq!(
        bounded_body(futures::stream::iter([Err::<Vec<u8>, _>(())]))
            .await
            .unwrap_err()
            .code,
        "relay_response_transport"
    );
}

#[test]
fn signed_response_adapter_rejects_duplicate_missing_tampered_and_wrong_nonce_headers() {
    let key = SecretKey::from_bytes([7; 32]);
    let nonce = "0123456789abcdef0123456789abcdef";
    let body=b"{\"version\":1,\"capabilities\":[\"text\"],\"telegramReady\":true,\"checkedAt\":100,\"error\":null}";
    let auth = protocol::sign_response(&key, nonce, 200, body).unwrap();
    let mut headers = HeaderMap::new();
    for (name, value) in auth.headers() {
        headers.insert(name, HeaderValue::from_str(value).unwrap());
    }
    let parsed = response_auth(&headers).unwrap();
    protocol::verify_response(&key, nonce, 200, &parsed, body).unwrap();
    for name in [HEADER_VERSION, HEADER_NONCE, HEADER_SIGNATURE] {
        let mut duplicate = headers.clone();
        duplicate.append(name, duplicate[name].clone());
        assert_eq!(
            response_auth(&duplicate).unwrap_err().code,
            "relay_response_auth"
        );
        let mut missing = headers.clone();
        missing.remove(name);
        assert_eq!(
            response_auth(&missing).unwrap_err().code,
            "relay_response_auth"
        );
    }
    assert!(protocol::verify_response(&key, nonce, 503, &parsed, body).is_err());
    assert!(protocol::verify_response(&key, nonce, 200, &parsed, b"{}").is_err());
    assert!(
        protocol::verify_response(&key, "1123456789abcdef0123456789abcdef", 200, &parsed, body)
            .is_err()
    );
}

#[test]
fn retries_keep_identical_message_bytes_but_use_new_authentication_nonces() {
    let message = RelayMessage {
        version: VERSION,
        id: "source:setup:42".into(),
        text: "Saved immutable recommendation".into(),
        expires_at: 160,
        attachment: None,
    };
    let original = message.encode(100).unwrap();
    assert_eq!(message.encode(150).unwrap(), original);
    assert!(
        message.validate_expiry(160).is_ok(),
        "Telegram's expiry boundary is inclusive"
    );
    assert!(message.validate_expiry(161).is_err());
    let first = nonce();
    let second = nonce();
    assert_eq!(first.len(), 32);
    assert_ne!(first, second);
    let key = SecretKey::from_bytes([7; 32]);
    let one = protocol::sign_request(&key, "POST", MESSAGES_PATH, 100, &first, &original).unwrap();
    let two = protocol::sign_request(&key, "POST", MESSAGES_PATH, 150, &second, &original).unwrap();
    assert_ne!(one.signature, two.signature);
    for outcome in [RelayOutcome::Sent, RelayOutcome::Duplicate] {
        let response = RelayMessageResponse {
            version: VERSION,
            id: message.id.clone(),
            outcome,
            telegram_message_id: Some(42),
        };
        response.validate().unwrap();
    }
}

fn fixed_job(now: i64) -> TelegramJob {
    let params = setups_domain::AlgorithmParams::default();
    let raw = setups_domain::SpikeSetup {
        algorithm_id: params.algorithm_id().into(),
        algorithm_version: params.algorithm_version().into(),
        open_time_unix: now - 60,
        close_time_unix: now,
        close_price: "100".into(),
        params: params.clone(),
        details: None,
    };
    TelegramJob {
        id: 1,
        setup_id: 42,
        delivery_id: "source:setup:42".into(),
        endpoint_url: "https://relay.example".into(),
        settings_revision: 1,
        owner_id: "admin".into(),
        algorithm_id: raw.algorithm_id.clone(),
        slot_id: None,
        generation_id: None,
        config_revision: None,
        fixed_config: Some(
            serde_json::json!({"algorithmVersion":raw.algorithm_version,"params":params}),
        ),
        lease_token: 1,
        wire_version: 1,
        raw_close_unix: now,
        expires_at_unix: now + 3600,
        attempts: 1,
        request_body: None,
        payload: serde_json::json!({"instrument":{"exchange":"bybit","category":"linear","symbol":"TESTUSDT"},
            "raw":raw,"recommendation":null,"externalName":"Captured name"}),
    }
}

fn png_fixture() -> Vec<u8> {
    fn hex(value: &str) -> Vec<u8> {
        value
            .as_bytes()
            .chunks_exact(2)
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect()
    }
    // A synthetic 900x320 static RGB image, compressed from zero scanlines.
    // No binary test asset, renderer dependency or retained decoded pixel cache.
    let mut png = hex(
        "89504e470d0a1a0a0000000d49484452000003840000014008020000006bca25e70000035c4944415478daedc18100000000c3a0f9531fe00a5501",
    );
    png.extend(std::iter::repeat_n(0, 836));
    png.extend(hex("c01b310300011fc9248e0000000049454e44ae426082"));
    v2::validate_png(&png).unwrap();
    png
}

fn presentation(text: &str, png: Option<Vec<u8>>) -> TelegramV2Presentation {
    TelegramV2Presentation {
        text: text.into(),
        entities: Vec::new(),
        png,
        unavailable_reason: None,
    }
}

#[test]
fn v2_png_and_caption_use_one_complete_body_and_utf16_entities() {
    let mut job = fixed_job(100);
    job.wire_version = 2;
    let text = "🔴 BTCUSDT · <literal>*name*";
    let mut content = presentation(text, Some(png_fixture()));
    content.entities.push(v2::TextEntity {
        kind: v2::EntityKind::Code,
        offset: 3,
        length: 7,
    });
    let rendered = render_v2_message(&job, 100, content, true).unwrap();
    assert!(rendered.diagnostic.is_none());
    let message = v2::RelayMessage::parse(&rendered.body, 100).unwrap();
    assert_eq!(message.text, text);
    assert_eq!(message.id, job.delivery_id);
    assert_eq!(message.expires_at, job.expires_at_unix);
    assert_eq!(
        message.attachment.unwrap().decode_png().unwrap(),
        png_fixture()
    );
    assert_eq!(message.entities[0].offset, 3);
    for (count, photo) in [(512, true), (513, false)] {
        let text = "😀".repeat(count);
        let rendered =
            render_v2_message(&job, 100, presentation(&text, Some(png_fixture())), true).unwrap();
        let message = v2::RelayMessage::parse(&rendered.body, 100).unwrap();
        assert_eq!(message.text, text);
        assert_eq!(message.attachment.is_some(), photo);
        if !photo {
            assert_eq!(rendered.diagnostic.unwrap().code, "caption_overflow");
        }
    }
}

#[test]
fn v2_missing_invalid_oversize_or_unsupported_png_falls_back_before_freeze() {
    let job = fixed_job(100);
    for (png, supported, reason) in [
        (None, true, "png_unavailable"),
        (Some(b"not png".to_vec()), true, "png_invalid"),
        (Some(vec![0; v2::MAX_PNG_BYTES + 1]), true, "png_invalid"),
        (Some(png_fixture()), false, "relay_photo_unsupported"),
    ] {
        let mut content = presentation("Complete trading terms, no truncation.", png);
        if content.png.is_none() {
            content.unavailable_reason = Some("png_unavailable");
        }
        let rendered = render_v2_message(&job, 100, content, supported).unwrap();
        let message = v2::RelayMessage::parse(&rendered.body, 100).unwrap();
        assert_eq!(message.text, "Complete trading terms, no truncation.");
        assert!(message.attachment.is_none());
        assert_eq!(rendered.diagnostic.unwrap().code, reason);
    }
    let mut missing = presentation("Available terms", None);
    missing.unavailable_reason = Some("private_artifact_detail_must_not_escape");
    assert_eq!(
        render_v2_message(&job, 100, missing, true)
            .unwrap()
            .diagnostic
            .unwrap()
            .code,
        "png_unavailable"
    );
}

#[test]
fn v2_ordinary_text_without_expected_chart_has_no_fallback_diagnostic() {
    let job = fixed_job(100);
    let ordinary =
        render_v2_message(&job, 100, presentation("WATCH literal text", None), true).unwrap();
    assert!(ordinary.diagnostic.is_none());
    assert!(
        v2::RelayMessage::parse(&ordinary.body, 100)
            .unwrap()
            .attachment
            .is_none()
    );
    let mut expected = presentation("Complete adaptive terms", None);
    expected.unavailable_reason = Some("png_unavailable");
    assert_eq!(
        render_v2_message(&job, 100, expected, true)
            .unwrap()
            .diagnostic
            .unwrap()
            .code,
        "png_unavailable"
    );
}

#[test]
fn v2_full_text_and_entity_errors_are_errors_not_truncation_or_two_messages() {
    let job = fixed_job(100);
    assert!(render_v2_message(&job, 100, presentation(&"😀".repeat(2048), None), true).is_ok());
    assert_eq!(
        render_v2_message(
            &job,
            100,
            presentation(&"😀".repeat(2049), Some(png_fixture())),
            true
        )
        .err()
        .unwrap()
        .code,
        "invalid_v2_text"
    );
    let mut content = presentation("😀 literal", None);
    content.entities.push(v2::TextEntity {
        kind: v2::EntityKind::Code,
        offset: 1,
        length: 1,
    });
    assert_eq!(
        render_v2_message(&job, 100, content, true)
            .err()
            .unwrap()
            .code,
        "invalid_v2_text"
    );
}

#[tokio::test]
async fn v2_requires_its_own_signed_empty_capability_probe_on_the_same_client() {
    for mode in [0, 1, 2, 5] {
        let (relay, server) = local_relay(mode).await;
        let key = relay.key().unwrap();
        let session = relay.connect("unused", &key).await.unwrap();
        let authenticated = session.authenticate_v2().await;
        if mode == 0 {
            let job = fixed_job(chrono::Utc::now().timestamp());
            let rendered = render_v2_message(
                &job,
                chrono::Utc::now().timestamp(),
                presentation("literal body", None),
                true,
            )
            .unwrap();
            let reply = authenticated
                .unwrap()
                .post(&rendered.body, job.expires_at_unix)
                .await
                .unwrap();
            assert_eq!(
                classify_v2_reply(reply, &job.delivery_id),
                DeliveryResult::Sent
            );
        } else if mode == 2 {
            assert!(
                authenticated
                    .unwrap()
                    .post(b"confidential", i64::MAX)
                    .await
                    .is_err()
            );
        } else {
            assert!(authenticated.is_err());
        }
        {
            let calls = relay.calls.lock().unwrap();
            assert_eq!(calls.len(), if mode == 0 { 2 } else { 1 });
            assert_eq!(calls[0].0, "GET");
            assert!(calls[0].1.is_empty());
            if mode == 0 {
                assert_eq!(calls[1].0, "POST");
            }
        }
        server.abort();
        let _ = server.await;
    }
}

#[test]
fn readiness_failure_diagnostic_is_bounded_and_redacted() {
    let mut job = fixed_job(100);
    job.id = 7;
    job.setup_id = 86;
    job.attempts = 3;
    job.settings_revision = 9;
    job.expires_at_unix = 160;
    job.endpoint_url = "https://endpoint_secret_sentinel.invalid/".into();
    job.delivery_id = "delivery_identity_secret_sentinel".into();
    job.owner_id = "owner_identity_secret_sentinel".into();
    job.algorithm_id = "algorithm_identity_secret_sentinel".into();
    job.fixed_config = Some(serde_json::json!({"private":"config_secret_sentinel"}));
    job.payload = serde_json::json!({"private":"payload_secret_sentinel"});
    job.request_body = Some(b"request_body_secret_sentinel".to_vec());
    let mut status = protocol::RelayStatus {
        version: protocol::VERSION,
        capabilities: vec![protocol::CAPABILITY_TEXT.into()],
        telegram_ready: false,
        checked_at: 118,
        error: None,
    };
    let check_line = |line: &str| {
        assert!(line.is_ascii());
        assert!(line.len() <= 512, "diagnostic exceeded its byte budget");
        assert!(!line.contains(['\r', '\n']));
        for excluded in [
            "endpoint_secret_sentinel",
            "delivery_identity_secret_sentinel",
            "owner_identity_secret_sentinel",
            "algorithm_identity_secret_sentinel",
            "config_secret_sentinel",
            "payload_secret_sentinel",
            "request_body_secret_sentinel",
        ] {
            assert!(!line.contains(excluded));
        }
    };
    for (error, expected) in [
        (Some("telegram_timeout"), "telegram_timeout"),
        (Some("telegram_unavailable"), "telegram_unavailable"),
        (
            Some("telegram_invalid_response"),
            "telegram_invalid_response",
        ),
        (
            Some("telegram_permission_denied"),
            "telegram_permission_denied",
        ),
        (Some("telegram_rejected"), "telegram_rejected"),
        (Some("telegram_rate_limited"), "telegram_rate_limited"),
        (None, "none"),
        (Some("secret_0123456789abcdef"), "other"),
    ] {
        status.error = error.map(str::to_owned);
        // The secret-shaped unknown code deliberately passes wire validation.
        status.validate().unwrap();
        let line = readiness_diagnostic(&job, &status, 120).unwrap();
        assert_eq!(
            line,
            format!(
                "[telegram-delivery] phase=readiness result=unready job=7 setup=86 attempt=3 settings_revision=9 signal_unix=100 expiry_unix=160 observed_unix=120 relay_checked_unix=118 remaining_s=40 upstream_code={expected}"
            )
        );
        assert!(!line.contains("secret_0123456789abcdef"));
        check_line(&line);
    }
    for error in [
        "untrusted_secret_".repeat(100),
        "secret\n\r\u{1b}非ASCII".into(),
    ] {
        status.error = Some(error.clone());
        assert!(status.validate().is_err());
        let line = readiness_diagnostic(&job, &status, 120).unwrap();
        assert!(line.ends_with("upstream_code=other"));
        assert!(!line.contains(&error));
        check_line(&line);
    }
    let update = authenticated_status(&status);
    assert!(update.relay_authenticated);
    assert_eq!(update.telegram_ready, Some(false));
    assert_eq!(update.reason.unwrap().code, "telegram_not_ready");
    status.telegram_ready = true;
    assert_eq!(readiness_diagnostic(&job, &status, 120), None);
    status.telegram_ready = false;
    let expired = readiness_diagnostic(&job, &status, 161).unwrap();
    assert!(expired.contains("remaining_s=-1 "));
    check_line(&expired);

    // Formatting stays bounded defensively even outside normal DB scalar ranges.
    for (value, observed, remaining, attempts) in [
        (i64::MIN, i64::MAX, i64::MIN, i32::MIN),
        (i64::MAX, i64::MIN, i64::MAX, i32::MAX),
    ] {
        job.id = value;
        job.setup_id = value;
        job.settings_revision = value;
        job.raw_close_unix = value;
        job.expires_at_unix = value;
        job.attempts = attempts;
        status.checked_at = value;
        status.error = Some("telegram_permission_denied".into());
        let line = readiness_diagnostic(&job, &status, observed).unwrap();
        assert!(line.contains(&format!("remaining_s={remaining} ")));
        check_line(&line);
    }
}

#[test]
fn saved_fixed_and_adaptive_rendering_use_immutable_names_params_and_expiry() {
    let now = 180_000;
    let mut job = fixed_job(now);
    let bytes = render_message(&job, now).unwrap();
    let message = RelayMessage::parse(&bytes, now).unwrap();
    assert_eq!(message.id, job.delivery_id);
    assert_eq!(message.expires_at, job.expires_at_unix);
    assert!(
        message
            .text
            .starts_with("WATCH TESTUSDT · Captured name\nbybit · ")
    );
    job.slot_id = Some(2);
    job.generation_id = Some(3);
    job.config_revision = Some(1);
    job.fixed_config = None;
    job.payload["recommendation"] = serde_json::json!({"direction":"short","tpPct":14,"slPct":4,"rawR":0.32,"meanR":0.21,"robustScore":0.29,
        "timeShiftMinutes":3,"priceChangePct":1,"clusterEnabled":false,"clusterGapMinutes":null,
        "provenance":{"algorithmId":"price_volume_spike","algorithmVersion":"1.1.1","optimizerVersion":"plan-a-continuous-1","seed":42,
            "snapshotHash":"fixture","capturedAt":"2026-11-12T00:00:00Z","fromUnix":0,"toUnix":now,"observationCutoff":now},
        "studyId":21,"generationId":3,"slotId":2,"ownerId":"admin","externalName":"Captured name","params":job.payload["raw"]["params"]});
    let bytes = render_message(&job, now).unwrap();
    let message = RelayMessage::parse(&bytes, now).unwrap();
    assert!(message.text.contains("SHORT"));
    assert!(message.text.contains("101,00 USDT"));
    assert!(message.text.contains("Исследование №21"));
    assert!(message.text.contains("Алгоритм: Captured name"));
    job.payload["externalName"] = "Renamed after persistence".into();
    assert_eq!(
        render_message(&job, now).unwrap_err().code,
        "invalid_saved_payload"
    );
}

#[test]
fn authenticated_actual_core_conflict_is_permanent_and_retry_after_is_never_shortened() {
    let body = br#"{"version":1,"code":"id_body_conflict"}"#.to_vec();
    let key = SecretKey::from_bytes([7; 32]);
    let n = "0123456789abcdef0123456789abcdef";
    let auth = protocol::sign_response(&key, n, 409, &body).unwrap();
    protocol::verify_response(&key, n, 409, &auth, &body).unwrap();
    assert_eq!(
        classify_reply(SignedReply { status: 409, body }, "source:setup:42"),
        DeliveryResult::Failed("relay_message_conflict")
    );
    let mut job = fixed_job(100);
    job.expires_at_unix = 3700;
    let TelegramOutcome::Retry { delay_seconds, .. } =
        retry_outcome(&job, "telegram_rate_limited", Some(120), 100)
    else {
        panic!("retry expected")
    };
    assert_eq!(delay_seconds, 120);
    assert_eq!(job.expires_at_unix, 3700, "TTL is not renewed by backoff");
    job.expires_at_unix = 160;
    let TelegramOutcome::Retry { delay_seconds, .. } =
        retry_outcome(&job, "telegram_rate_limited", Some(120), 100)
    else {
        panic!("retry expected")
    };
    assert_eq!(delay_seconds, 120);
    assert!(
        100 + delay_seconds > job.expires_at_unix,
        "must expire, not send earlier at its boundary"
    );
    let TelegramOutcome::Retry { delay_seconds, .. } =
        retry_outcome(&job, "rate_limited", Some(u32::MAX), 100)
    else {
        panic!("retry expected")
    };
    assert_eq!(delay_seconds, 86400);
}

type RecordedCall = (String, Vec<u8>, String);
#[derive(Clone)]
struct LocalRelay {
    origin: String,
    mode: Arc<std::sync::atomic::AtomicU8>,
    cancel: Arc<AtomicBool>,
    photo: Arc<AtomicBool>,
    presentation_calls: Arc<std::sync::atomic::AtomicUsize>,
    calls: Arc<std::sync::Mutex<Vec<RecordedCall>>>,
}
impl RelayAccess for LocalRelay {
    fn key(&self) -> Result<SecretKey, ClientFailure> {
        Ok(SecretKey::from_bytes([7; 32]))
    }
    async fn connect<'key>(
        &self,
        _origin: &str,
        key: &'key SecretKey,
    ) -> Result<RelaySession<'key>, ClientFailure> {
        // A test transport dependency only: production always uses SecureRelayAccess
        // and its unchanged public-IP/HTTPS connector, without any bypass flag.
        Ok(RelaySession {
            origin: Url::parse(&self.origin).unwrap(),
            key,
            client: Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .retry(reqwest::retry::never())
                .build()
                .unwrap(),
        })
    }
    async fn presentation(
        &self,
        pool: &PgPool,
        job: &TelegramJob,
    ) -> Result<TelegramV2Presentation, ClientFailure> {
        assert!(self.presentation_calls.fetch_add(1, Ordering::SeqCst) < 8);
        let mut content = saved_presentation(pool, job).await?;
        if self.photo.load(Ordering::SeqCst) {
            content.png = Some(png_fixture());
        }
        Ok(content)
    }
}
async fn local_relay(mode: u8) -> (LocalRelay, tokio::task::JoinHandle<()>) {
    use axum::{
        Router,
        extract::State,
        response::IntoResponse,
        routing::{get, post},
    };
    async fn handle(
        State(relay): State<LocalRelay>,
        request: axum::extract::Request,
    ) -> axum::response::Response {
        let method = request.method().to_string();
        let path = request.uri().path().to_string();
        let headers = request.headers().clone();
        let body = axum::body::to_bytes(request.into_body(), v2::MAX_BODY_BYTES)
            .await
            .unwrap();
        let auth = protocol::RequestAuth {
            version: headers[HEADER_VERSION].to_str().unwrap().into(),
            timestamp: headers[HEADER_TIMESTAMP].to_str().unwrap().into(),
            nonce: headers[HEADER_NONCE].to_str().unwrap().into(),
            signature: headers[HEADER_SIGNATURE].to_str().unwrap().into(),
        };
        let is_v2 = path.starts_with("/v2/");
        if is_v2 {
            v2::verify_request(
                &SecretKey::from_bytes([7; 32]),
                &method,
                &path,
                &auth,
                &body,
                chrono::Utc::now().timestamp(),
            )
            .unwrap();
        } else {
            protocol::verify_request(
                &SecretKey::from_bytes([7; 32]),
                &method,
                &path,
                &auth,
                &body,
                chrono::Utc::now().timestamp(),
            )
            .unwrap();
        }
        {
            let mut calls = relay.calls.lock().unwrap();
            assert!(calls.len() < 8);
            calls.push((method.clone(), body.to_vec(), auth.nonce.clone()));
        }
        let mode = relay.mode.load(Ordering::SeqCst);
        if method == "GET" && mode == 4 {
            relay.cancel.store(true, Ordering::SeqCst);
        }
        let (status, body) = if is_v2 && mode == 5 {
            (
                404,
                br#"{"version":2,"code":"unsupported_version"}"#.to_vec(),
            )
        } else if method == "GET" {
            (
                200,
                serde_json::to_vec(&serde_json::json!({
                    "version": if is_v2 {2}else{1},
                    "capabilities":if is_v2 {vec!["text","text_entities","photo_png"]}else{vec!["text"]},
                    "telegramReady":mode != 2,
                    "checkedAt":chrono::Utc::now().timestamp(),
                    "error":null,
                }))
                .unwrap(),
            )
        } else if mode == 3 {
            (
                503,
                serde_json::to_vec(&serde_json::json!({"version":if is_v2 {2}else{1},"code":"telegram_transport_unknown"})).unwrap(),
            )
        } else {
            let id = if is_v2 {
                v2::RelayMessage::parse(&body, chrono::Utc::now().timestamp())
                    .unwrap()
                    .id
            } else {
                RelayMessage::parse(&body, chrono::Utc::now().timestamp())
                    .unwrap()
                    .id
            };
            (
                200,
                serde_json::to_vec(&serde_json::json!({"version":if is_v2 {2}else{1},"id":id,"outcome":"sent","telegramMessageId":42}))
                .unwrap(),
            )
        };
        let key = SecretKey::from_bytes([if mode == 1 { 8 } else { 7 }; 32]);
        let auth = if is_v2 {
            v2::sign_response(&key, &auth.nonce, status, &body).unwrap()
        } else {
            protocol::sign_response(&key, &auth.nonce, status, &body).unwrap()
        };
        let mut headers = HeaderMap::new();
        for (name, value) in auth.headers() {
            headers.insert(name, HeaderValue::from_str(value).unwrap());
        }
        (
            axum::http::StatusCode::from_u16(status).unwrap(),
            headers,
            body,
        )
            .into_response()
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let relay = LocalRelay {
        origin: format!("http://{}", listener.local_addr().unwrap()),
        mode: Arc::new(std::sync::atomic::AtomicU8::new(mode)),
        cancel: Arc::new(AtomicBool::new(false)),
        photo: Arc::new(AtomicBool::new(false)),
        presentation_calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        calls: Arc::default(),
    };
    let app = Router::new()
        .route(protocol::STATUS_PATH, get(handle))
        .route(MESSAGES_PATH, post(handle))
        .route(v2::STATUS_PATH, get(handle))
        .route(v2::MESSAGES_PATH, post(handle))
        .with_state(relay.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (relay, server)
}
fn test_attempt(now: i64) -> TelegramTestAttempt {
    let request_id = "11111111-1111-4111-8111-111111111111".to_string();
    let job = fixed_job(now);
    TelegramTestAttempt {
        config: TelegramConfig {
            enabled: true,
            endpoint_url: job.endpoint_url.clone(),
            revision: 1,
        },
        request_id,
        delivery_id: job.delivery_id.clone(),
        expires_at_unix: job.expires_at_unix,
        request_body: render_message(&job, now).unwrap(),
    }
}

#[tokio::test]
async fn empty_challenge_authenticates_before_any_text_and_test_has_one_post_no_auto_retry() {
    for (mode, state, count) in [
        (0, "sent", 2),
        (1, "error", 1),
        (2, "error", 1),
        (3, "unknown", 2),
    ] {
        let (relay, server) = local_relay(mode).await;
        let attempt = test_attempt(chrono::Utc::now().timestamp());
        let result = test_via(&attempt, &relay).await;
        assert_eq!(result.state, state, "mode={mode}");
        {
            let calls = relay.calls.lock().unwrap();
            assert_eq!(calls.len(), count);
            assert_eq!(calls[0].0, "GET");
            assert!(calls[0].1.is_empty());
            if count == 2 {
                assert_eq!(calls[1].0, "POST");
                assert_eq!(calls[1].1, attempt.request_body);
                assert_ne!(calls[0].2, calls[1].2);
            }
        }
        server.abort();
        let _ = server.await;
    }
}

#[tokio::test]
async fn postgres_sender_freezes_exact_retry_bytes_rechecks_stop_and_never_waits_for_webpush() {
    use sqlx::{
        Connection, Row,
        postgres::{PgConnectOptions, PgPoolOptions},
    };
    let Ok(url) = std::env::var("TEST_DATABASE_URL") else {
        eprintln!("TEST_DATABASE_URL absent; PostgreSQL sender test skipped");
        return;
    };
    let options: PgConnectOptions = url.parse().unwrap();
    assert!(options.get_database().unwrap_or("").ends_with("_test"));
    let name = format!("telegram_sender_{}_test", std::process::id());
    let mut admin = sqlx::PgConnection::connect_with(&options).await.unwrap();
    sqlx::query(&format!("create database {name}"))
        .execute(&mut admin)
        .await
        .unwrap();
    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect_with(options.database(&name))
        .await
        .unwrap();
    let baseline = include_str!("../../db/migrations/0010_baseline.sql")
        .lines()
        .filter(|line| !line.starts_with('\\') && !line.starts_with("SET transaction_timeout"))
        .collect::<Vec<_>>()
        .join("\n");
    let mut connection = pool.acquire().await.unwrap();
    sqlx::raw_sql(&baseline)
        .execute(&mut *connection)
        .await
        .unwrap();
    sqlx::raw_sql("set search_path=public; create table kysely_migration(name varchar(255) primary key,timestamp varchar(255) not null); insert into kysely_migration values('0010_baseline.sql','fixture');")
        .execute(&mut *connection).await.unwrap();
    drop(connection);
    setups_db::migrate::ensure_migrations(&pool).await.unwrap();
    setups_db::settings_set(&pool, "live", &serde_json::json!({"enabled":true}))
        .await
        .unwrap();
    setups_db::settings_set(
        &pool,
        "notifications",
        &serde_json::json!({"maxAgeSeconds":3600}),
    )
    .await
    .unwrap();
    setups_db::settings_set(
        &pool,
        "algorithm:price_volume_spike",
        &serde_json::json!({"enabled":true,"externalName":"Captured name"}),
    )
    .await
    .unwrap();
    let config = db::get_config(&pool).await.unwrap();
    db::set_config(&pool, config.revision, true, "https://relay.example")
        .await
        .unwrap();
    let now = chrono::Utc::now().timestamp().div_euclid(60) * 60;
    let raw: setups_domain::SpikeSetup =
        serde_json::from_value(fixed_job(now).payload["raw"].clone()).unwrap();
    setups_db::continuous::outbox::commit_fixed_setup(&pool, "bybit", "linear", "TESTUSDT", &raw)
        .await
        .unwrap()
        .unwrap();
    let worker = "sender-fixture";
    let job = db::claim(&pool, worker, 2).await.unwrap().remove(0);
    let (relay, server) = local_relay(3).await;
    relay.photo.store(true, Ordering::SeqCst);
    let outcome = send_job_via(&pool, worker, &job, &AtomicBool::new(false), &relay)
        .await
        .unwrap();
    assert!(
        matches!(outcome, TelegramOutcome::Retry { .. }),
        "signed uncertain ACK must retry durably"
    );
    let frozen: Vec<u8> =
        sqlx::query_scalar("select request_body from telegram_delivery_outbox where id=$1")
            .bind(job.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(relay.calls.lock().unwrap()[1].1, frozen);
    assert_eq!(job.wire_version, 2);
    assert!(
        v2::RelayMessage::parse(&frozen, chrono::Utc::now().timestamp())
            .unwrap()
            .attachment
            .is_some()
    );
    db::finish(&pool, worker, &job, outcome).await.unwrap();
    tokio::time::sleep(Duration::from_millis(2100)).await;
    relay.mode.store(0, Ordering::SeqCst);
    relay.photo.store(false, Ordering::SeqCst);
    let mut retry = db::claim(&pool, worker, 2).await.unwrap().remove(0);
    assert_eq!(retry.attempts, 2);
    assert_eq!(retry.request_body.as_ref(), Some(&frozen));
    assert_eq!(retry.delivery_id, job.delivery_id);
    assert_eq!(retry.expires_at_unix, job.expires_at_unix);
    // Simulate unavailable/new formatting after a deploy: frozen bytes, not a
    // reconstructed JSONB body or changed renderer, remain the retry authority.
    retry.payload = serde_json::Value::Null;
    let outcome = send_job_via(&pool, worker, &retry, &AtomicBool::new(false), &relay)
        .await
        .unwrap();
    assert!(matches!(outcome, TelegramOutcome::Sent));
    db::finish(&pool, worker, &retry, outcome).await.unwrap();
    {
        let calls = relay.calls.lock().unwrap();
        assert_eq!(calls.len(), 4);
        assert_eq!(calls[3].1, frozen);
        assert_ne!(calls[1].2, calls[3].2);
    }
    let row = sqlx::query(
        "select telegram_delivery_status,notification_delivery_status from live_setups where id=$1",
    )
    .bind(job.setup_id as i32)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(row.get::<String, _>("telegram_delivery_status"), "sent");
    assert_eq!(
        row.get::<String, _>("notification_delivery_status"),
        "pending"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("select count(*) from adaptive_notification_outbox")
            .fetch_one(&pool)
            .await
            .unwrap(),
        1,
        "pending Web Push never blocks Telegram"
    );

    let mut second = raw.clone();
    second.open_time_unix -= 120;
    second.close_time_unix -= 120;
    setups_db::continuous::outbox::commit_fixed_setup(
        &pool, "bybit", "linear", "TESTUSDT", &second,
    )
    .await
    .unwrap()
    .unwrap();
    let job = db::claim(&pool, worker, 2).await.unwrap().remove(0);
    relay.mode.store(4, Ordering::SeqCst);
    let outcome = send_job_via(&pool, worker, &job, &relay.cancel, &relay)
        .await
        .unwrap();
    assert!(matches!(outcome, TelegramOutcome::Disabled { .. }));
    assert_eq!(
        relay.calls.lock().unwrap().len(),
        5,
        "Stop after the empty challenge must prevent text POST"
    );
    db::finish(&pool, worker, &job, outcome).await.unwrap();

    setups_db::settings_set(
        &pool,
        "notifications",
        &serde_json::json!({"maxAgeSeconds":180}),
    )
    .await
    .unwrap();
    let mut third = raw;
    third.open_time_unix -= 60;
    third.close_time_unix -= 60;
    setups_db::continuous::outbox::commit_fixed_setup(&pool, "bybit", "linear", "TESTUSDT", &third)
        .await
        .unwrap()
        .unwrap();
    let job = db::claim(&pool, worker, 2).await.unwrap().remove(0);
    assert!(job.expires_at_unix - chrono::Utc::now().timestamp() < 120);
    let outcome = retry_outcome(
        &job,
        "telegram_rate_limited",
        Some(120),
        chrono::Utc::now().timestamp(),
    );
    db::finish(&pool, worker, &job, outcome).await.unwrap();
    assert!(
        sqlx::query_scalar::<_, bool>(
            "select not_before>expires_at from telegram_delivery_outbox where id=$1"
        )
        .bind(job.id)
        .fetch_one(&pool)
        .await
        .unwrap()
    );
    assert!(
        db::claim(&pool, worker, 2).await.unwrap().is_empty(),
        "server retry_after beyond TTL must never produce an early attempt"
    );
    assert_eq!(relay.calls.lock().unwrap().len(), 5);
    server.abort();
    let _ = server.await;
    pool.close().await;
    sqlx::query(&format!("drop database {name} with (force)"))
        .execute(&mut admin)
        .await
        .unwrap();
}

#[tokio::test]
async fn probe_is_empty_get_only_and_authenticated_readiness_is_independent() {
    for (mode, authenticated, ready) in [
        (0, true, Some(true)),
        (1, false, None),
        (2, true, Some(false)),
    ] {
        let (relay, server) = local_relay(mode).await;
        let result = probe_via(
            &TelegramConfig {
                enabled: true,
                endpoint_url: "https://relay.example".into(),
                revision: 1,
            },
            &relay,
        )
        .await;
        assert_eq!(result.relay_authenticated, authenticated);
        assert_eq!(result.telegram_ready, ready);
        assert_eq!(relay.calls.lock().unwrap().len(), 1);
        assert!(relay.calls.lock().unwrap()[0].1.is_empty());
        server.abort();
        let _ = server.await;
    }
}

#[tokio::test]
async fn postgres_saved_png_lookup_is_exact_validated_and_never_repeated_for_frozen_retries() {
    use sqlx::{
        Connection,
        postgres::{PgConnectOptions, PgPoolOptions},
    };
    let Ok(url) = std::env::var("TEST_DATABASE_URL") else {
        eprintln!("TEST_DATABASE_URL absent; PostgreSQL chart lookup test skipped");
        return;
    };
    let options: PgConnectOptions = url.parse().unwrap();
    assert!(options.get_database().unwrap_or("").ends_with("_test"));
    assert!(matches!(
        options.get_host(),
        "127.0.0.1" | "localhost" | "::1"
    ));
    let name = format!("telegram_chart_{}_test", std::process::id());
    let mut admin = sqlx::PgConnection::connect_with(&options).await.unwrap();
    sqlx::query(&format!("create database {name}"))
        .execute(&mut admin)
        .await
        .unwrap();
    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect_with(options.database(&name))
        .await
        .unwrap();
    let fixture = pool.clone();
    let (relay, server) = local_relay(3).await;
    let local = tokio::task::LocalSet::new();
    let result=local.run_until(async move {tokio::task::spawn_local(async move {
        let baseline=include_str!("../../db/migrations/0010_baseline.sql").lines()
            .filter(|line|!line.starts_with('\\')&&!line.starts_with("SET transaction_timeout"))
            .collect::<Vec<_>>().join("\n");
        let mut connection=fixture.acquire().await.unwrap();
        sqlx::raw_sql(&baseline).execute(&mut *connection).await.unwrap();
        sqlx::raw_sql("set search_path=public; create table kysely_migration(name varchar(255) primary key,timestamp varchar(255) not null); insert into kysely_migration values('0010_baseline.sql','fixture')").execute(&mut *connection).await.unwrap();
        drop(connection);
        setups_db::migrate::ensure_migrations(&fixture).await.unwrap();
        saved_png_regression(&fixture,&relay).await;
    }).await}).await;
    server.abort();
    let _ = server.await;
    pool.close().await;
    sqlx::query(&format!("drop database {name} with (force)"))
        .execute(&mut admin)
        .await
        .unwrap();
    result.unwrap();
}

async fn saved_png_regression(pool: &PgPool, relay: &LocalRelay) {
    use serde_json::json;
    use setups_db::expectancy::{self as artifacts, ArtifactIdentity};
    use setups_domain::{AdaptiveRecommendationSnapshot, RecommendationDirection, expectancy::*};
    let now = chrono::Utc::now().timestamp().div_euclid(60) * 60;
    setups_db::settings_set(pool, "live", &json!({"enabled":true}))
        .await
        .unwrap();
    db::set_config(pool, 0, true, "https://relay.example")
        .await
        .unwrap();
    setups_db::settings_set(pool, "notifications", &json!({"maxAgeSeconds":3600}))
        .await
        .unwrap();
    let worker = "chart-worker";
    let fixed_raw: setups_domain::SpikeSetup =
        serde_json::from_value(fixed_job(now).payload["raw"].clone()).unwrap();
    let fixed_id = setups_db::continuous::outbox::commit_fixed_setup(
        pool,
        "bybit",
        "linear",
        "FIXEDUSDT",
        &fixed_raw,
    )
    .await
    .unwrap()
    .unwrap();
    let fixed = db::claim(pool, worker, 1).await.unwrap().remove(0);
    assert_eq!(fixed.wire_version, 2);
    assert!(fixed.slot_id.is_none());
    relay.mode.store(0, Ordering::SeqCst);
    let outcome = send_job_via(pool, worker, &fixed, &AtomicBool::new(false), relay)
        .await
        .unwrap();
    assert!(matches!(outcome, TelegramOutcome::Sent));
    let diagnostic: Option<serde_json::Value> =
        sqlx::query_scalar("select render_diagnostic from telegram_delivery_outbox where id=$1")
            .bind(fixed.id)
            .fetch_one(pool)
            .await
            .unwrap();
    assert!(
        diagnostic.is_none(),
        "ordinary fixed WATCH must not acquire a chart warning"
    );
    db::finish(pool, worker, &fixed, outcome).await.unwrap();
    let fixed_result:(String,Option<serde_json::Value>)=sqlx::query_as("select telegram_delivery_status,telegram_delivery_error from live_setups where id::bigint=$1").bind(fixed_id).fetch_one(pool).await.unwrap();
    assert_eq!(fixed_result, ("sent".into(), None));
    relay.mode.store(3, Ordering::SeqCst);
    sqlx::raw_sql("insert into continuous_owner_lanes(owner_id) values('admin'); insert into continuous_processes(owner_id,algorithm_id,status,desired_running,config) values('admin','price_volume_spike','running',true,'{\"adaptiveEnabled\":true}')").execute(pool).await.unwrap();
    let slot:i64=sqlx::query_scalar("insert into continuous_slots(owner_id,algorithm_id,min_setups_per_day,status) values('admin','price_volume_spike',4,'ready') returning id").fetch_one(pool).await.unwrap();
    let generation:i64=sqlx::query_scalar("insert into continuous_generations(owner_id,algorithm_id,slot_id,config_revision,lease_token,status,phase,spec,execution_started_at,observation_cutoff) values('admin','price_volume_spike',$1,1,1,'completed','active','{}',clock_timestamp(),$2) returning id")
        .bind(slot).bind(now-3600).fetch_one(pool).await.unwrap();
    let study:i64=sqlx::query_scalar("insert into optimization_studies(algorithm_id,status,spec,progress,result,origin,owner_id,continuous_generation_id,continuous_slot_id) values('price_volume_spike','completed','{}','{}','{}','continuous','admin',$1,$2) returning id")
        .bind(generation).bind(slot).fetch_one(pool).await.unwrap();
    sqlx::query("update continuous_generations set study_id=$2 where id=$1")
        .bind(generation)
        .bind(study)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("update continuous_slots set active_generation_id=$2,study_id=$3 where id=$1")
        .bind(slot)
        .bind(generation)
        .bind(study)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("update continuous_processes set active_generation_id=$1,active_study_id=$2 where owner_id='admin'").bind(generation).bind(study).execute(pool).await.unwrap();
    let mut job = fixed_job(now);
    job.wire_version = 2;
    job.slot_id = Some(slot);
    job.generation_id = Some(generation);
    job.config_revision = Some(1);
    job.fixed_config = None;
    job.payload["recommendation"] = json!({"direction":"short","tpPct":14,"slPct":4,"rawR":1.25,"meanR":0.21,"robustScore":0.29,
        "timeShiftMinutes":3,"priceChangePct":1,"clusterEnabled":false,"clusterGapMinutes":null,
        "provenance":{"algorithmId":"price_volume_spike","algorithmVersion":"1.1.1","optimizerVersion":"fixture-v1","seed":42,
            "snapshotHash":"exact-frozen-source","capturedAt":"2026-11-12T00:00:00Z","fromUnix":now-3*86400,"toUnix":now-3600,"observationCutoff":now-3600},
        "studyId":study,"generationId":generation,"slotId":slot,"ownerId":"admin","externalName":"Captured name","params":job.payload["raw"]["params"],
        "rawSignalAt":now,"recommendedEntryAt":now+180});
    let snapshot: AdaptiveRecommendationSnapshot =
        serde_json::from_value(job.payload["recommendation"].clone()).unwrap();
    let identity = ArtifactIdentity {
        owner_id: "admin".into(),
        generation_id: generation,
        study_id: study,
        slot_id: slot,
        calculation_version: EXPECTANCY_CALCULATION_VERSION.to_string(),
        tp_pct: 14.,
        sl_pct: 4.,
        from_unix: snapshot.provenance.from_unix,
        to_unix: snapshot.provenance.to_unix,
        observation_cutoff: snapshot.provenance.observation_cutoff,
        provenance: serde_json::to_value(&snapshot.provenance).unwrap(),
        effective_analytics: json!({"clusterEnabled":false,"clusterGapMinutes":null,"timeShiftMinutes":3,"priceChangePct":1,"horizonMinutes":60}),
        historical_input_hash: None,
    };
    let key = artifacts::artifact_key(&identity).unwrap();
    let first = utc_midnight_unix(identity.from_unix).unwrap() + 86400;
    let numeric = build_expectancy_artifact(ExpectancyInput {
        source: ExpectancySource {
            study_id: study.to_string(),
            derived_snapshot_id: key.clone(),
            generation_id: Some(generation.to_string()),
            profile_id: Some(slot.to_string()),
            provenance: snapshot.provenance.clone(),
            direction: RecommendationDirection::Short,
            tp_pct: 14.,
            sl_pct: 4.,
            analytics: ExpectancyAnalytics {
                cluster_enabled: false,
                cluster_gap_minutes: None,
                time_shift_minutes: 3,
                price_change_pct: Some(1.),
                horizon_minutes: 60,
            },
        },
        selected_cell: Some(SelectedCellInput {
            raw_r: Some(1.25),
            tp_count: 1,
            sl_count: 1,
        }),
        matrix_mean_r: Some(0.21),
        daily: MetricAvailability::Available {
            value: vec![
                UtcDailyInput {
                    day_start_unix: first,
                    row_count: 1,
                    tp_count: 1,
                    sl_count: 0,
                    mean_r: Some(3.5),
                },
                UtcDailyInput {
                    day_start_unix: first + 86400,
                    row_count: 1,
                    tp_count: 0,
                    sl_count: 1,
                    mean_r: Some(-1.),
                },
            ],
        },
    })
    .unwrap();
    let id = insert_chart_artifact(pool, &identity, &numeric).await;
    artifacts::store_png(pool, "admin", id, "1", &png_fixture())
        .await
        .unwrap();
    let selected = numeric.selected_cell.as_ref().unwrap();
    let saved = crate::adaptive::AdaptiveExpectancySnapshot {
        calculation_version: numeric.calculation_version,
        timezone: numeric.timezone,
        study_id: study,
        generation_id: generation,
        slot_id: slot,
        artifact_id: Some(id),
        derived_snapshot_id: Some(key.clone()),
        source_snapshot_hash: snapshot.provenance.snapshot_hash.clone(),
        source_cutoff: snapshot.provenance.observation_cutoff,
        from_unix: numeric.from_unix,
        to_unix: numeric.to_unix,
        period_days: numeric.period_days,
        selected_mean_r: selected.raw_r.clone(),
        tp_count: Some(1),
        sl_count: Some(1),
        closed_count: Some(2),
        win_rate_pct: selected.win_rate_pct.clone(),
        matrix_mean_r: numeric.matrix_mean_r.clone(),
        trend_delta_r: numeric.trend_delta_r.clone(),
        forecast_r: numeric.forecast_at_raw_signal(now).unwrap(),
        forecast_at_unix: now,
        image_key: Some(numeric.image_key(1)),
        historical_recalculation: None,
    };
    job.payload["recommendation"]["expectancy"] = serde_json::to_value(saved).unwrap();
    let setup:i32=sqlx::query_scalar("insert into live_setups(exchange_code,category,symbol,algorithm_id,algorithm_version,params,open_time,close_time,close_price,adaptive_slot_id,adaptive_generation_id,recommendation,external_name,expectancy_artifact_id) values('bybit','linear','TESTUSDT','price_volume_spike','1.1.1',$1,to_timestamp($2),to_timestamp($3),'100',$4,$5,$6,'Captured name',$7) returning id")
        .bind(job.payload["raw"]["params"].clone()).bind((now-60)as f64).bind(now as f64).bind(slot).bind(generation).bind(job.payload["recommendation"].clone()).bind(id).fetch_one(pool).await.unwrap();
    sqlx::query("insert into telegram_delivery_outbox(setup_id,delivery_id,endpoint_url,settings_revision,owner_id,algorithm_id,slot_id,generation_id,config_revision,raw_close_unix,expires_at,payload,wire_version) values($1,'exact:chart','https://relay.example/',1,'admin','price_volume_spike',$2,$3,1,$4,to_timestamp($5),$6,2)")
        .bind(setup).bind(slot).bind(generation).bind(now).bind(job.expires_at_unix as f64).bind(&job.payload).execute(pool).await.unwrap();
    let job = db::claim(pool, worker, 1).await.unwrap().remove(0);
    assert!(db::permitted(pool, worker, &job).await.unwrap());
    let outcome = send_job_via(pool, worker, &job, &AtomicBool::new(false), relay)
        .await
        .unwrap();
    assert!(matches!(outcome, TelegramOutcome::Retry { .. }));
    let frozen: Vec<u8> =
        sqlx::query_scalar("select request_body from telegram_delivery_outbox where id=$1")
            .bind(job.id)
            .fetch_one(pool)
            .await
            .unwrap();
    let parsed = v2::RelayMessage::parse(&frozen, now).unwrap();
    assert_eq!(
        parsed.attachment.unwrap().decode_png().unwrap(),
        png_fixture(),
        "PNG came from exact persisted id/version, not the test photo override"
    );
    assert!(!relay.photo.load(Ordering::SeqCst));
    assert_eq!(relay.presentation_calls.load(Ordering::SeqCst), 2);
    db::finish(pool, worker, &job, outcome).await.unwrap();

    let mut other_identity = identity.clone();
    other_identity.study_id += 1;
    other_identity.provenance["snapshotHash"] = "different-source".into();
    let mut other_numeric = numeric.clone();
    other_numeric.source.study_id = other_identity.study_id.to_string();
    other_numeric.source.provenance.snapshot_hash = "different-source".into();
    other_numeric.source.derived_snapshot_id = artifacts::artifact_key(&other_identity).unwrap();
    let other = insert_chart_artifact(pool, &other_identity, &other_numeric).await;
    artifacts::store_png(pool, "admin", other, "1", &png_fixture())
        .await
        .unwrap();
    let mut wrong_source = job.clone();
    wrong_source.payload["recommendation"]["expectancy"]["artifactId"] = other.into();
    let content = saved_presentation(pool, &wrong_source).await.unwrap();
    let rendered = render_v2_message(&wrong_source, now, content, true).unwrap();
    assert_eq!(rendered.diagnostic.unwrap().code, "png_identity_mismatch");
    assert!(
        v2::RelayMessage::parse(&rendered.body, now)
            .unwrap()
            .attachment
            .is_none()
    );

    for (png, hash, expected) in [
        (png_fixture(), "0".repeat(64), "png_hash_mismatch"),
        (
            {
                let mut png = png_fixture();
                png[16..20].copy_from_slice(&901_u32.to_be_bytes());
                png
            },
            String::new(),
            "png_invalid",
        ),
    ] {
        sqlx::query("delete from study_expectancy_pngs where artifact_id=$1")
            .bind(id)
            .execute(pool)
            .await
            .unwrap();
        let hash = if hash.is_empty() {
            format!("{:x}", sha2::Sha256::digest(&png))
        } else {
            hash
        };
        // Fault injection is confined to this disposable DB. Production writes
        // use the backend's immutable store_png and cannot update a cache row.
        sqlx::query("insert into study_expectancy_pngs(artifact_id,rendering_version,image_key,png,sha256) values($1,'1',$2,$3,$4)")
            .bind(id).bind(serde_json::to_value(numeric.image_key(1)).unwrap()).bind(png).bind(hash).execute(pool).await.unwrap();
        let content = saved_presentation(pool, &job).await.unwrap();
        let rendered = render_v2_message(&job, now, content, true).unwrap();
        assert_eq!(rendered.diagnostic.unwrap().code, expected);
        assert!(
            v2::RelayMessage::parse(&rendered.body, now)
                .unwrap()
                .attachment
                .is_none()
        );
    }
    sqlx::query("delete from study_expectancy_pngs where artifact_id=$1")
        .bind(id)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("insert into study_expectancy_pngs(artifact_id,rendering_version,image_key,png,sha256) values($1,'1',$2,$3,$4)")
        .bind(id).bind(serde_json::to_value(other_numeric.image_key(1)).unwrap()).bind(png_fixture())
        .bind(format!("{:x}",sha2::Sha256::digest(png_fixture()))).execute(pool).await.unwrap();
    let wrong_key = saved_presentation(pool, &job).await.unwrap();
    assert!(
        wrong_key.png.is_none(),
        "a cached PNG with another full image key is never accepted"
    );
    assert_eq!(wrong_key.unavailable_reason, Some("png_unavailable"));
    sqlx::query("delete from study_expectancy_pngs where artifact_id=$1")
        .bind(id)
        .execute(pool)
        .await
        .unwrap();
    artifacts::store_png(pool, "admin", id, "2", &png_fixture())
        .await
        .unwrap();
    let absent = saved_presentation(pool, &job).await.unwrap();
    assert!(
        absent.png.is_none(),
        "a different renderer version must never substitute for the frozen image key"
    );
    assert_eq!(absent.unavailable_reason, Some("png_unavailable"));
    sqlx::query(
        "update telegram_delivery_outbox set not_before=clock_timestamp()-interval '1 second'",
    )
    .execute(pool)
    .await
    .unwrap();
    let mut retry = db::claim(pool, worker, 1).await.unwrap().remove(0);
    retry.payload = serde_json::Value::Null;
    relay.mode.store(0, Ordering::SeqCst);
    let outcome = send_job_via(pool, worker, &retry, &AtomicBool::new(false), relay)
        .await
        .unwrap();
    assert!(matches!(outcome, TelegramOutcome::Sent));
    assert_eq!(
        relay.presentation_calls.load(Ordering::SeqCst),
        2,
        "frozen retry must not look up artifact/PNG, even after cache loss"
    );
    assert_eq!(relay.calls.lock().unwrap().last().unwrap().1, frozen);
    db::finish(pool, worker, &retry, outcome).await.unwrap();

    let mut missing = job.clone();
    missing.payload["instrument"]["symbol"] = "ABSENTUSDT".into();
    let setup:i32=sqlx::query_scalar("insert into live_setups(exchange_code,category,symbol,algorithm_id,algorithm_version,params,open_time,close_time,close_price,adaptive_slot_id,adaptive_generation_id,recommendation,external_name,expectancy_artifact_id) values('bybit','linear','ABSENTUSDT','price_volume_spike','1.1.1',$1,to_timestamp($2),to_timestamp($3),'100',$4,$5,$6,'Captured name',$7) returning id")
        .bind(missing.payload["raw"]["params"].clone()).bind((now-60)as f64).bind(now as f64).bind(slot).bind(generation).bind(missing.payload["recommendation"].clone()).bind(id).fetch_one(pool).await.unwrap();
    sqlx::query("insert into telegram_delivery_outbox(setup_id,delivery_id,endpoint_url,settings_revision,owner_id,algorithm_id,slot_id,generation_id,config_revision,raw_close_unix,expires_at,payload,wire_version) values($1,'missing:chart','https://relay.example/',1,'admin','price_volume_spike',$2,$3,1,$4,to_timestamp($5),$6,2)")
        .bind(setup).bind(slot).bind(generation).bind(now).bind(missing.expires_at_unix as f64).bind(&missing.payload).execute(pool).await.unwrap();
    let missing = db::claim(pool, worker, 1).await.unwrap().remove(0);
    let outcome = send_job_via(pool, worker, &missing, &AtomicBool::new(false), relay)
        .await
        .unwrap();
    assert!(matches!(outcome, TelegramOutcome::Sent));
    let body: Vec<u8> =
        sqlx::query_scalar("select request_body from telegram_delivery_outbox where id=$1")
            .bind(missing.id)
            .fetch_one(pool)
            .await
            .unwrap();
    assert!(
        v2::RelayMessage::parse(&body, now)
            .unwrap()
            .attachment
            .is_none()
    );
    db::finish(pool, worker, &missing, outcome).await.unwrap();
    let missing_result: (String, Option<serde_json::Value>) = sqlx::query_as(
        "select telegram_delivery_status,telegram_delivery_error from live_setups where id=$1",
    )
    .bind(setup)
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(missing_result.0, "sent");
    assert_eq!(
        missing_result.1.unwrap()["code"],
        "png_unavailable",
        "expected adaptive chart retains its reason after successful text-only delivery"
    );
}

async fn insert_chart_artifact(
    pool: &PgPool,
    identity: &setups_db::expectancy::ArtifactIdentity,
    numeric: &setups_domain::expectancy::ExpectancyArtifact,
) -> i64 {
    numeric.validate().unwrap();
    sqlx::query_scalar("insert into study_expectancy_artifacts(artifact_key,owner_id,source_generation_id,source_study_id,source_slot_id,calculation_version,tp_pct,sl_pct,identity,numerical_snapshot) values($1,$2,$3,$4,$5,$6,$7,$8,$9,$10) returning id")
        .bind(setups_db::expectancy::artifact_key(identity).unwrap()).bind(&identity.owner_id).bind(identity.generation_id).bind(identity.study_id).bind(identity.slot_id).bind(&identity.calculation_version)
        .bind(identity.tp_pct).bind(identity.sl_pct).bind(serde_json::to_value(identity).unwrap()).bind(serde_json::to_value(numeric).unwrap()).fetch_one(pool).await.unwrap()
}
