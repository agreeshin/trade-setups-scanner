use super::*;
use axum::{Json, extract::Path, routing::post};
use http_body_util::BodyExt;
use protocol::{ResponseAuth, sign_request, verify_response};
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    sync::atomic::{AtomicI64, AtomicU8, AtomicUsize},
    time::Duration,
};
use tower::ServiceExt;

const NOW: i64 = 1_800_000_000;
const NONCE: &str = "0123456789abcdef0123456789abcdef";
static NEXT: AtomicUsize = AtomicUsize::new(0);
struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "setups-relay-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
struct MockState {
    send_mode: AtomicU8,
    permission_mode: AtomicU8,
    calls: std::sync::Mutex<Vec<(String, Value)>>,
}
struct Mock {
    origin: String,
    state: Arc<MockState>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Mock {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Mock {
    async fn new() -> Self {
        let state = Arc::new(MockState {
            send_mode: AtomicU8::new(0),
            permission_mode: AtomicU8::new(0),
            calls: std::sync::Mutex::new(Vec::new()),
        });
        let router = Router::new()
            .route(
                "/bot12345:TEST_SENTINEL_TOKEN_NEVER_LOG/{method}",
                post(mock_handler),
            )
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        Self {
            origin,
            state,
            task,
        }
    }
    fn calls(&self, method: &str) -> usize {
        self.state
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(name, _)| name == method)
            .count()
    }
}
async fn mock_handler(
    State(state): State<Arc<MockState>>,
    Path(method): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    {
        let mut calls = state.calls.lock().unwrap();
        assert!(calls.len() < 128);
        calls.push((method.clone(), body));
    }
    let mode = if method == "sendMessage" {
        state.send_mode.load(Ordering::Relaxed)
    } else {
        state.permission_mode.load(Ordering::Relaxed)
    };
    match mode {
        1=>(StatusCode::TOO_MANY_REQUESTS,Json(json!({"ok":false,"error_code":429,"description":"SECRET-UPSTREAM-DESCRIPTION","parameters":{"retry_after":7}}))).into_response(),
        2=>(StatusCode::FORBIDDEN,Json(json!({"ok":false,"error_code":403,"description":"SECRET-UPSTREAM-DESCRIPTION"}))).into_response(),
        3=>{tokio::time::sleep(Duration::from_millis(200)).await;Json(json!({"ok":true,"result":{"message_id":42}})).into_response()},
        4=>(StatusCode::OK,"not json SECRET-UPSTREAM-DESCRIPTION").into_response(),
        5=>(StatusCode::OK,"x".repeat(32769)).into_response(),
        6=>(StatusCode::TEMPORARY_REDIRECT,[("location","http://127.0.0.1:1/must-not-follow")],Json(json!({"ok":false}))).into_response(),
        _=>if method=="getChatMember" {Json(json!({"ok":true,"result":{"status":"administrator","can_post_messages":true,"user":{"id":12345}}})).into_response()}
        else{Json(json!({"ok":true,"result":{"message_id":42}})).into_response()},
    }
}
struct Fixture {
    app: Arc<App>,
    mock: Mock,
    now: Arc<AtomicI64>,
    directory: Directory,
}
impl Fixture {
    async fn new() -> Self {
        Self::with_limits(1000).await
    }
    async fn with_limits(max_receipts: usize) -> Self {
        let directory = Directory::new();
        let mock = Mock::new().await;
        let now = Arc::new(AtomicI64::new(NOW));
        let clock = now.clone();
        let store = Store::open(
            &directory.0.join("state.sqlite"),
            12345,
            -10012345,
            crate::store::StoreLimits {
                max_receipts,
                max_bytes: 1024 * 1024,
                retention_seconds: 60,
                lease_seconds: 20,
            },
        )
        .unwrap();
        let app = App::new(
            SecretKey::from_bytes([7; 32]),
            store,
            Telegram::for_test(mock.origin.clone(), Duration::from_millis(75)),
            2,
            Arc::new(move || clock.load(Ordering::Relaxed)),
        );
        Self {
            app,
            mock,
            now,
            directory,
        }
    }
}
fn message(id: &str) -> Vec<u8> {
    serde_json::to_vec(&json!({"version":1,"id":id,"text":"plain <b>text</b> https://invalid.example/private","expiresAt":NOW+60,"attachment":null})).unwrap()
}
fn request(method: &str, path: &str, body: Vec<u8>, timestamp: i64, key: u8) -> Request<Body> {
    request_nonce(
        method,
        path,
        body,
        timestamp,
        key,
        &format!("{:032x}", NEXT.fetch_add(1, Ordering::Relaxed)),
    )
}
fn request_nonce(
    method: &str,
    path: &str,
    body: Vec<u8>,
    timestamp: i64,
    key: u8,
    nonce: &str,
) -> Request<Body> {
    let auth = sign_request(
        &SecretKey::from_bytes([key; 32]),
        method,
        path,
        timestamp,
        nonce,
        &body,
    )
    .unwrap();
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json");
    for (name, value) in auth.headers() {
        request = request.header(name, value);
    }
    request.body(Body::from(body)).unwrap()
}
async fn execute(app: &Arc<App>, request: Request<Body>, authenticated: bool) -> (u16, Value) {
    let nonce = request.headers()[protocol::HEADER_NONCE]
        .to_str()
        .unwrap()
        .to_owned();
    let response = app.router().oneshot(request).await.unwrap();
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    if authenticated {
        let auth = ResponseAuth {
            version: headers[protocol::HEADER_VERSION].to_str().unwrap().into(),
            nonce: headers[protocol::HEADER_NONCE].to_str().unwrap().into(),
            signature: headers[protocol::HEADER_SIGNATURE].to_str().unwrap().into(),
        };
        verify_response(
            &SecretKey::from_bytes([7; 32]),
            &nonce,
            status,
            &auth,
            &bytes,
        )
        .unwrap();
    }
    assert!(!String::from_utf8_lossy(&bytes).contains("SECRET-UPSTREAM"));
    assert!(!String::from_utf8_lossy(&bytes).contains("TEST_SENTINEL_TOKEN"));
    (status, serde_json::from_slice(&bytes).unwrap())
}
async fn send(f: &Fixture, body: Vec<u8>) -> (u16, Value) {
    execute(
        &f.app,
        request(
            "POST",
            protocol::MESSAGES_PATH,
            body,
            f.now.load(Ordering::Relaxed),
            7,
        ),
        true,
    )
    .await
}

#[tokio::test]
async fn valid_plain_text_duplicate_raw_hash_conflict_and_restart_receipt() {
    let f = Fixture::new().await;
    let body = message("stable-id");
    let (status, sent) = send(&f, body.clone()).await;
    assert_eq!(status, 200);
    assert_eq!(sent["outcome"], "sent");
    assert_eq!(sent["telegramMessageId"], 42);
    assert_eq!(send(&f, body.clone()).await.1["outcome"], "duplicate");
    assert_eq!(f.mock.calls("sendMessage"), 1);
    let mut changed = body.clone();
    changed.push(b' ');
    assert_eq!(send(&f, changed).await.0, 409);
    let new_store = Store::open(
        &f.directory.0.join("state.sqlite"),
        12345,
        -10012345,
        f.app.store.limits_for_test(),
    )
    .unwrap();
    assert_eq!(
        new_store
            .claim(
                "stable-id".into(),
                protocol::body_sha256(&body),
                NOW + 60,
                NOW
            )
            .await
            .unwrap(),
        Claim::Duplicate(42)
    );
    let calls = f.mock.state.calls.lock().unwrap();
    let payload = &calls
        .iter()
        .find(|(name, _)| name == "sendMessage")
        .unwrap()
        .1;
    assert_eq!(payload["chat_id"], -10012345i64);
    assert!(payload.get("parse_mode").is_none());
    assert_eq!(payload["link_preview_options"]["is_disabled"], true);
    assert!(payload["text"].as_str().unwrap().contains("<b>text</b>"));
}
#[tokio::test]
async fn wrong_tampered_stale_duplicate_headers_and_routes_never_forward() {
    let f = Fixture::new().await;
    for (time, key) in [(NOW, 8), (NOW - 61, 7), (NOW + 61, 7)] {
        assert_eq!(
            execute(
                &f.app,
                request("POST", protocol::MESSAGES_PATH, message("bad"), time, key),
                false
            )
            .await
            .0,
            401
        );
    }
    let mut req = request("POST", protocol::MESSAGES_PATH, message("bad"), NOW, 7);
    *req.body_mut() = Body::from(message("tampered"));
    assert_eq!(execute(&f.app, req, false).await.0, 401);
    let mut req = request("POST", protocol::MESSAGES_PATH, message("bad"), NOW, 7);
    req.headers_mut()
        .append(protocol::HEADER_NONCE, HeaderValue::from_static(NONCE));
    assert_eq!(execute(&f.app, req, false).await.0, 401);
    let mut req = request("POST", protocol::MESSAGES_PATH, message("bad"), NOW, 7);
    *req.uri_mut() = "/v1/messages?x=1".parse().unwrap();
    assert_eq!(execute(&f.app, req, false).await.0, 401);
    let mut req = request("POST", protocol::MESSAGES_PATH, message("bad"), NOW, 7);
    *req.method_mut() = axum::http::Method::GET;
    assert_eq!(execute(&f.app, req, false).await.0, 401);
    assert_eq!(f.mock.calls("sendMessage"), 0);
}
#[tokio::test]
async fn bounds_attachment_unknown_fields_and_inclusive_expiry() {
    let f = Fixture::new().await;
    for patch in [
        json!({"attachment":[]}),
        json!({"attachment":{"url":"https://invalid.example/no-fetch"}}),
        json!({"text":"x".repeat(4097)}),
        json!({"expiresAt":NOW-1}),
        json!({"expiresAt":NOW+86401}),
        json!({"chat_id":-1}),
    ] {
        let mut body: Value = serde_json::from_slice(&message("invalid")).unwrap();
        for (key, value) in patch.as_object().unwrap() {
            body[key] = value.clone();
        }
        assert_eq!(send(&f, serde_json::to_vec(&body).unwrap()).await.0, 400);
    }
    let mut body: Value = serde_json::from_slice(&message("missing-attachment")).unwrap();
    body.as_object_mut().unwrap().remove("attachment");
    assert_eq!(send(&f, serde_json::to_vec(&body).unwrap()).await.0, 400);
    let mut req = request("POST", protocol::MESSAGES_PATH, message("large"), NOW, 7);
    *req.body_mut() = Body::from(vec![b'x'; protocol::MAX_BODY_BYTES + 1]);
    assert_eq!(execute(&f.app, req, false).await.0, 413);
    let mut exact: Value = serde_json::from_slice(&message("exact-boundary")).unwrap();
    exact["expiresAt"] = json!(NOW);
    exact["text"] = json!(char::from_u32(0x1f680).unwrap().to_string().repeat(4096));
    assert_eq!(send(&f, serde_json::to_vec(&exact).unwrap()).await.0, 200);
    assert_eq!(f.mock.calls("sendMessage"), 1);
}
#[tokio::test]
async fn expiry_rechecked_after_claim_before_external_call() {
    let mut f = Fixture::new().await;
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    Arc::get_mut(&mut f.app).unwrap().clock = Arc::new(move || {
        if counter.fetch_add(1, Ordering::Relaxed) >= 3 {
            NOW + 1
        } else {
            NOW
        }
    });
    let mut body: Value = serde_json::from_slice(&message("expires-while-admitting")).unwrap();
    body["expiresAt"] = json!(NOW);
    let (status, error) = send(&f, serde_json::to_vec(&body).unwrap()).await;
    assert_eq!(status, 400);
    assert_eq!(error["code"], "expired");
    assert_eq!(f.mock.calls("sendMessage"), 0);
}
#[tokio::test]
async fn signed_permission_probe_not_config_readiness_and_no_status_send() {
    let f = Fixture::new().await;
    f.mock.state.permission_mode.store(2, Ordering::Relaxed);
    let (status, value) = execute(
        &f.app,
        request("GET", protocol::STATUS_PATH, vec![], NOW, 7),
        true,
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(value["telegramReady"], false);
    assert_eq!(value["capabilities"], json!(["text"]));
    assert_eq!(f.mock.calls("sendMessage"), 0);
    f.mock.state.permission_mode.store(0, Ordering::Relaxed);
    f.app.probe.lock().await.cached = None;
    let value = execute(
        &f.app,
        request("GET", protocol::STATUS_PATH, vec![], NOW, 7),
        true,
    )
    .await
    .1;
    assert_eq!(value["telegramReady"], true);
    assert_eq!(value["checkedAt"], NOW);
    execute(
        &f.app,
        request("GET", protocol::STATUS_PATH, vec![], NOW, 7),
        true,
    )
    .await;
    assert_eq!(f.mock.calls("getChatMember"), 2);
    f.app.stopping.store(true, Ordering::Release);
    assert_eq!(
        execute(
            &f.app,
            request("GET", protocol::STATUS_PATH, vec![], NOW, 7),
            true
        )
        .await
        .0,
        503
    );
}
#[tokio::test]
async fn telegram_rate_limit_backoff_and_permanent_rejection_are_durable() {
    let f = Fixture::new().await;
    let body = message("rate-limited");
    f.mock.state.send_mode.store(1, Ordering::Relaxed);
    let (status, error) = send(&f, body.clone()).await;
    assert_eq!(status, 429);
    assert_eq!(error["retryAfter"], 7);
    assert_eq!(send(&f, body.clone()).await.0, 429);
    assert_eq!(f.mock.calls("sendMessage"), 1);
    f.now.store(NOW + 7, Ordering::Relaxed);
    f.mock.state.send_mode.store(0, Ordering::Relaxed);
    assert_eq!(send(&f, body).await.0, 200);
    f.mock.state.send_mode.store(2, Ordering::Relaxed);
    let body = message("denied");
    assert_eq!(send(&f, body.clone()).await.0, 403);
    assert_eq!(send(&f, body).await.0, 403);
    assert_eq!(f.mock.calls("sendMessage"), 3);
}
#[tokio::test]
async fn timeout_uncertainty_is_retryable_not_false_success_and_bad_upstream_is_safe() {
    let f = Fixture::new().await;
    f.mock.state.send_mode.store(3, Ordering::Relaxed);
    let body = message("ack-lost");
    let (status, error) = send(&f, body.clone()).await;
    assert_eq!(status, 503);
    assert_eq!(error["code"], "telegram_timeout");
    assert_eq!(send(&f, body.clone()).await.0, 503);
    assert_eq!(f.mock.calls("sendMessage"), 1);
    // Telegram may already have accepted attempt one. This documented uncertain
    // case may duplicate externally; only a durable success receipt prevents it.
    f.now.store(NOW + 1, Ordering::Relaxed);
    f.mock.state.send_mode.store(0, Ordering::Relaxed);
    assert_eq!(send(&f, body.clone()).await.0, 200);
    assert_eq!(send(&f, body).await.1["outcome"], "duplicate");
    assert_eq!(f.mock.calls("sendMessage"), 2);
    for mode in [4, 5, 6] {
        f.mock.state.send_mode.store(mode, Ordering::Relaxed);
        assert_eq!(
            send(&f, message(&format!("bad-upstream-{mode}"))).await.0,
            503
        );
    }
}
#[tokio::test]
async fn concurrent_id_is_busy_no_db_mutex_over_telegram_await_and_cancel_does_not_skip_lease() {
    let f = Fixture::new().await;
    f.mock.state.send_mode.store(3, Ordering::Relaxed);
    let body = message("concurrent");
    let app = f.app.clone();
    let first_body = body.clone();
    let first = tokio::spawn(async move {
        execute(
            &app,
            request("POST", protocol::MESSAGES_PATH, first_body, NOW, 7),
            true,
        )
        .await
    });
    for _ in 0..30 {
        if f.mock.calls("sendMessage") > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    assert_eq!(f.mock.calls("sendMessage"), 1);
    assert_eq!(
        tokio::time::timeout(Duration::from_millis(50), send(&f, body.clone()))
            .await
            .unwrap()
            .0,
        409
    );
    tokio::time::timeout(Duration::from_millis(50), f.app.store.health())
        .await
        .unwrap()
        .unwrap();
    first.abort();
    let _ = first.await;
    assert_eq!(send(&f, body).await.0, 409);
    assert_eq!(f.mock.calls("sendMessage"), 1);
}
#[tokio::test]
async fn capacity_expired_lease_fencing_destination_and_actual_cleanup() {
    let f = Fixture::with_limits(1).await;
    let first = message("one");
    assert_eq!(send(&f, first).await.0, 200);
    assert_eq!(send(&f, message("two")).await.0, 503);
    let limits = f.app.store.limits_for_test();
    let path = f.directory.0.join("state.sqlite");
    assert!(Store::open(&path, 12345, -10012345, limits).is_ok());
    assert!(Store::open(&path, 54321, -10012345, limits).is_err());
    assert!(Store::open(&path, 12345, -10099999, limits).is_err());
    assert!(f.app.store.cleanup(NOW + 181).await.unwrap() >= 1);
    assert_eq!(f.app.store.stats().await.0, 0);
    assert_eq!(
        f.app
            .store
            .claim("lease".into(), "f".repeat(64), NOW + 1000, NOW)
            .await
            .unwrap(),
        Claim::Acquired(1)
    );
    assert_eq!(
        f.app
            .store
            .claim("lease".into(), "f".repeat(64), NOW + 1000, NOW + 20)
            .await
            .unwrap(),
        Claim::Acquired(2)
    );
    assert_eq!(
        f.app
            .store
            .finish("lease".into(), 1, Completion::Sent(9), NOW + 20)
            .await,
        Err(RelayError::StaleLease)
    );
    f.app
        .store
        .finish("lease".into(), 2, Completion::Sent(10), NOW + 20)
        .await
        .unwrap();
}
#[tokio::test]
async fn physical_page_reclamation_under_repeated_operations() {
    let f = Fixture::with_limits(1000).await;
    for cycle in 0..3 {
        for index in 0..500 {
            let id = format!("{cycle}-{index}-{}", "x".repeat(100));
            let Claim::Acquired(epoch) = f
                .app
                .store
                .claim(id.clone(), "e".repeat(64), NOW + 60, NOW)
                .await
                .unwrap()
            else {
                panic!("unexpected claim")
            };
            f.app
                .store
                .finish(id, epoch, Completion::Sent(42), NOW)
                .await
                .unwrap();
        }
        let before = f.app.store.stats().await;
        assert_eq!(before.0, 500);
        while f.app.store.cleanup(NOW + 181).await.unwrap() > 0 {}
        let after = f.app.store.stats().await;
        assert_eq!(after.0, 0);
        assert!(after.1 < before.1);
        assert!(
            std::fs::metadata(f.directory.0.join("state.sqlite"))
                .unwrap()
                .len()
                <= 1024 * 1024
        );
    }
}

#[tokio::test]
async fn exact_post_replay_after_timeout_and_restart_never_creates_another_attempt() {
    let f = Fixture::new().await;
    let body = message("durable-post-nonce");
    f.mock.state.send_mode.store(3, Ordering::Relaxed);
    let same = || request_nonce("POST", protocol::MESSAGES_PATH, body.clone(), NOW, 7, NONCE);
    assert_eq!(execute(&f.app, same(), true).await.0, 503);
    assert_eq!(f.mock.calls("sendMessage"), 1);
    f.now.store(NOW + 1, Ordering::Relaxed);
    assert_eq!(execute(&f.app, same(), true).await.0, 409);
    assert_eq!(f.mock.calls("sendMessage"), 1);
    let store = Store::open(
        &f.directory.0.join("state.sqlite"),
        12345,
        -10012345,
        f.app.store.limits_for_test(),
    )
    .unwrap();
    let clock = f.now.clone();
    let restarted = App::new(
        SecretKey::from_bytes([7; 32]),
        store,
        Telegram::for_test(f.mock.origin.clone(), Duration::from_millis(75)),
        2,
        Arc::new(move || clock.load(Ordering::Relaxed)),
    );
    assert_eq!(execute(&restarted, same(), true).await.0, 409);
    assert_eq!(f.mock.calls("sendMessage"), 1);
    f.mock.state.send_mode.store(0, Ordering::Relaxed);
    assert_eq!(
        execute(
            &restarted,
            request("POST", protocol::MESSAGES_PATH, body, NOW + 1, 7),
            true
        )
        .await
        .0,
        200
    );
    assert_eq!(f.mock.calls("sendMessage"), 2);
    // Both old and new successful-ACK-loss replays now resolve to the receipt.
    let old = request_nonce(
        "POST",
        protocol::MESSAGES_PATH,
        message("durable-post-nonce"),
        NOW,
        7,
        NONCE,
    );
    assert_eq!(
        execute(&restarted, old, true).await.1["outcome"],
        "duplicate"
    );
    assert_eq!(f.mock.calls("sendMessage"), 2);
}

#[tokio::test]
async fn old_busy_nonce_after_lease_expiry_cannot_turn_into_new_send() {
    let f = Fixture::new().await;
    let body = message("busy-replay");
    f.app
        .store
        .claim_attempt(
            "busy-replay".into(),
            protocol::body_sha256(&body),
            NOW + 60,
            NOW,
            PostAttempt {
                nonce: "11111111111111111111111111111111".into(),
                signed_timestamp: NOW,
            },
        )
        .await
        .unwrap();
    let old = || request_nonce("POST", protocol::MESSAGES_PATH, body.clone(), NOW, 7, NONCE);
    assert_eq!(execute(&f.app, old(), true).await.0, 409);
    f.now.store(NOW + 20, Ordering::Relaxed);
    let (status, error) = execute(&f.app, old(), true).await;
    assert_eq!(status, 409);
    assert_eq!(error["code"], "post_replay");
    assert_eq!(f.mock.calls("sendMessage"), 0);
    assert_eq!(send(&f, body).await.0, 200);
    assert_eq!(f.mock.calls("sendMessage"), 1);
}

#[tokio::test]
async fn nonce_capacity_fails_closed_and_future_timestamp_retention_is_inclusive() {
    let f = Fixture::with_limits(1).await;
    let body = message("nonce-capacity");
    for _ in 0..4 {
        assert_eq!(send(&f, body.clone()).await.0, 200);
    }
    let (status, error) = send(&f, body).await;
    assert_eq!(status, 503);
    assert_eq!(error["code"], "attempt_capacity");
    assert_eq!(f.app.store.attempt_count().await, 4);
    assert_eq!(f.mock.calls("sendMessage"), 1);

    let f = Fixture::new().await;
    let mut value: Value = serde_json::from_slice(&message("future-window")).unwrap();
    value["expiresAt"] = json!(NOW + 300);
    let body = serde_json::to_vec(&value).unwrap();
    f.app
        .store
        .claim_attempt(
            "future-window".into(),
            protocol::body_sha256(&body),
            NOW + 300,
            NOW,
            PostAttempt {
                nonce: NONCE.into(),
                signed_timestamp: NOW + 60,
            },
        )
        .await
        .unwrap();
    f.app.store.cleanup(NOW + 120).await.unwrap();
    assert_eq!(f.app.store.attempt_count().await, 1);
    f.now.store(NOW + 120, Ordering::Relaxed);
    assert_eq!(
        execute(
            &f.app,
            request_nonce(
                "POST",
                protocol::MESSAGES_PATH,
                body.clone(),
                NOW + 60,
                7,
                NONCE
            ),
            true
        )
        .await
        .0,
        409
    );
    assert_eq!(f.mock.calls("sendMessage"), 0);
    f.app.store.cleanup(NOW + 121).await.unwrap();
    assert_eq!(f.app.store.attempt_count().await, 0);
    f.now.store(NOW + 121, Ordering::Relaxed);
    assert_eq!(
        execute(
            &f.app,
            request_nonce("POST", protocol::MESSAGES_PATH, body, NOW + 60, 7, NONCE),
            false
        )
        .await
        .0,
        401
    );
    assert_eq!(f.mock.calls("sendMessage"), 0);
}

#[tokio::test]
async fn cannot_persist_post_protection_fails_before_telegram() {
    let f = Fixture::new().await;
    f.app.store.disable_writes_for_test().await;
    let (status, error) = send(&f, message("storage-unavailable")).await;
    assert_eq!(status, 503);
    assert_eq!(error["code"], "storage_unavailable");
    assert_eq!(f.mock.calls("sendMessage"), 0);
    assert_eq!(f.app.store.attempt_count().await, 0);
    assert_eq!(f.app.store.stats().await.0, 0);
}
