use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use setups_api::{SessionAuth, session_router};
use std::sync::Arc;
use tower::ServiceExt;

fn app() -> Router {
    let auth = Arc::new(SessionAuth::new(&["test-key".to_string()]));
    session_router(auth)
}

async fn body_text(response: axum::response::Response) -> String {
    String::from_utf8(
        response
            .into_body()
            .collect()
            .await
            .expect("body collects")
            .to_bytes()
            .to_vec(),
    )
    .expect("utf-8 body")
}

#[tokio::test]
async fn session_lifecycle_matches_ts_contract() {
    let app = app();

    let forbidden = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/session")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"apiKey":"test-key"}"#))
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(forbidden.status(), StatusCode::FORBIDDEN);
    assert!(
        body_text(forbidden)
            .await
            .contains("invalid_request_origin")
    );

    let created = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/session")
                .header("x-platform-request", "1")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"apiKey":"test-key"}"#))
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(created.status(), StatusCode::OK);
    let set_cookie = created
        .headers()
        .get("set-cookie")
        .expect("set-cookie present")
        .to_str()
        .expect("cookie ascii")
        .to_string();
    assert!(set_cookie.starts_with("platform_session="));
    assert!(set_cookie.contains("Path=/v1; HttpOnly; SameSite=Strict; Max-Age=86400"));
    assert_eq!(
        created
            .headers()
            .get("cache-control")
            .and_then(|v| v.to_str().ok()),
        Some("no-store")
    );
    assert!(body_text(created).await.contains("\"authenticated\":true"));
    let token = set_cookie
        .split(';')
        .next()
        .expect("cookie pair")
        .trim_start_matches("platform_session=")
        .to_string();

    let ok = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/session")
                .header("cookie", format!("platform_session={token}"))
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(ok.status(), StatusCode::OK);

    let deleted = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/v1/session")
                .header("x-platform-request", "1")
                .header("cookie", format!("platform_session={token}"))
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(deleted.status(), StatusCode::NO_CONTENT);
    assert!(
        deleted
            .headers()
            .get("set-cookie")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|cookie| cookie.contains("Max-Age=0"))
    );

    let after = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/session")
                .header("cookie", format!("platform_session={token}"))
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(after.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn login_rate_limit_after_ten_failures() {
    let app = app();
    for _ in 0..10 {
        let bad = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/session")
                    .header("x-platform-request", "1")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"apiKey":"wrong"}"#))
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(bad.status(), StatusCode::UNAUTHORIZED);
    }
    let limited = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/session")
                .header("x-platform-request", "1")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"apiKey":"test-key"}"#))
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(limited.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(limited.headers().contains_key("retry-after"));
    assert!(body_text(limited).await.contains("too_many_attempts"));
}
