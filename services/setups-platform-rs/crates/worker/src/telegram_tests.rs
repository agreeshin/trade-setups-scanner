use super::*;
use protocol::{RelayMessage, RelayMessageResponse, RelayOutcome, VERSION};
use reqwest::header::HeaderValue;

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
        raw_close_unix: now,
        expires_at_unix: now + 3600,
        attempts: 1,
        request_body: None,
        payload: serde_json::json!({"instrument":{"exchange":"bybit","category":"linear","symbol":"TESTUSDT"},
            "raw":raw,"recommendation":null,"externalName":"Captured name"}),
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
        (Some("telegram_invalid_response"), "telegram_invalid_response"),
        (Some("telegram_permission_denied"), "telegram_permission_denied"),
        (Some("telegram_rejected"), "telegram_rejected"),
        (Some("telegram_rate_limited"), "telegram_rate_limited"),
        (None, "none"),
        (Some("secret_0123456789abcdef"), "other"),
    ] {
        status.error = error.map(str::to_owned);
        // The secret-shaped unknown code deliberately passes wire validation.
        status.validate().unwrap();
        let line = readiness_diagnostic(&job, &status, 120).unwrap();
        assert_eq!(line, format!(
            "[telegram-delivery] phase=readiness result=unready job=7 setup=86 attempt=3 settings_revision=9 signal_unix=100 expiry_unix=160 observed_unix=120 relay_checked_unix=118 remaining_s=40 upstream_code={expected}"
        ));
        assert!(!line.contains("secret_0123456789abcdef"));
        check_line(&line);
    }
    for error in ["untrusted_secret_".repeat(100), "secret\n\r\u{1b}非ASCII".into()] {
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
        let body = axum::body::to_bytes(request.into_body(), MAX_BODY_BYTES)
            .await
            .unwrap();
        let auth = protocol::RequestAuth {
            version: headers[HEADER_VERSION].to_str().unwrap().into(),
            timestamp: headers[HEADER_TIMESTAMP].to_str().unwrap().into(),
            nonce: headers[HEADER_NONCE].to_str().unwrap().into(),
            signature: headers[HEADER_SIGNATURE].to_str().unwrap().into(),
        };
        protocol::verify_request(
            &SecretKey::from_bytes([7; 32]),
            &method,
            &path,
            &auth,
            &body,
            chrono::Utc::now().timestamp(),
        )
        .unwrap();
        {
            let mut calls = relay.calls.lock().unwrap();
            assert!(calls.len() < 8);
            calls.push((method.clone(), body.to_vec(), auth.nonce.clone()));
        }
        let mode = relay.mode.load(Ordering::SeqCst);
        if method == "GET" && mode == 4 {
            relay.cancel.store(true, Ordering::SeqCst);
        }
        let (status, body) = if method == "GET" {
            (
                200,
                serde_json::to_vec(&protocol::RelayStatus {
                    version: 1,
                    capabilities: vec!["text".into()],
                    telegram_ready: mode != 2,
                    checked_at: chrono::Utc::now().timestamp(),
                    error: None,
                })
                .unwrap(),
            )
        } else if mode == 3 {
            (
                503,
                br#"{"version":1,"code":"telegram_transport_unknown"}"#.to_vec(),
            )
        } else {
            let message = RelayMessage::parse(&body, chrono::Utc::now().timestamp()).unwrap();
            (
                200,
                serde_json::to_vec(&RelayMessageResponse {
                    version: 1,
                    id: message.id,
                    outcome: RelayOutcome::Sent,
                    telegram_message_id: Some(42),
                })
                .unwrap(),
            )
        };
        let key = SecretKey::from_bytes([if mode == 1 { 8 } else { 7 }; 32]);
        let auth = protocol::sign_response(&key, &auth.nonce, status, &body).unwrap();
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
        calls: Arc::default(),
    };
    let app = Router::new()
        .route(protocol::STATUS_PATH, get(handle))
        .route(MESSAGES_PATH, post(handle))
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
    db::finish(&pool, worker, &job, outcome).await.unwrap();
    tokio::time::sleep(Duration::from_millis(2100)).await;
    relay.mode.store(0, Ordering::SeqCst);
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
