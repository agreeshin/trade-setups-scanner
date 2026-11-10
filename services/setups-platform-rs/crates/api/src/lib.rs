pub mod analytics;
pub mod auth;
pub mod backup;

pub use auth::{API_KEY_HEADER, COOKIE_NAME, SESSION_SECONDS, SessionAuth, cookie_string, now_ms};

use auth::PostSessionResult;
pub mod batches;
pub mod config;
pub mod controls;
pub mod replay;
pub mod restore;

use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

#[derive(Deserialize)]
struct LoginBody {
    #[serde(default, rename = "apiKey")]
    api_key: Option<String>,
}

pub fn browser_request(headers: &HeaderMap) -> bool {
    let platform = headers
        .get("x-platform-request")
        .and_then(|value| value.to_str().ok());
    if platform != Some("1") {
        return false;
    }
    if let Some(site) = headers
        .get("sec-fetch-site")
        .and_then(|value| value.to_str().ok())
        && site != "same-origin"
        && site != "none"
    {
        return false;
    }
    if let Some(origin) = headers.get("origin").and_then(|value| value.to_str().ok()) {
        let Some((scheme, origin_host)) = origin.split_once("://") else {
            return false;
        };
        if scheme != "http" && scheme != "https" {
            return false;
        }
        let Some(host) = headers.get("host").and_then(|value| value.to_str().ok()) else {
            return false;
        };
        if origin_host != host {
            return false;
        }
    }
    true
}

fn secure_request(headers: &HeaderMap) -> bool {
    headers
        .get("origin")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|origin| origin.starts_with("https://"))
}

fn error_response(status: StatusCode, message: &str) -> Response {
    let mut response = (status, Json(json!({ "error": message }))).into_response();
    response
        .headers_mut()
        .insert("cache-control", HeaderValue::from_static("no-store"));
    response
}

fn cookie_header(headers: &HeaderMap) -> Option<String> {
    headers
        .get(axum::http::header::COOKIE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}

pub fn session_router(state: Arc<SessionAuth>) -> Router {
    Router::new()
        .route(
            "/v1/session",
            post(create_session).get(get_session).delete(delete_session),
        )
        .with_state(state)
}

async fn create_session(
    State(auth): State<Arc<SessionAuth>>,
    headers: HeaderMap,
    Json(body): Json<LoginBody>,
) -> Response {
    if !browser_request(&headers) {
        return error_response(StatusCode::FORBIDDEN, "invalid_request_origin");
    }
    let now = now_ms();
    let secure = secure_request(&headers);
    match auth.post_session(body.api_key.as_deref(), now) {
        PostSessionResult::RateLimited {
            retry_after_seconds,
        } => {
            let mut response = error_response(StatusCode::TOO_MANY_REQUESTS, "too_many_attempts");
            if let Ok(value) = HeaderValue::from_str(&retry_after_seconds.to_string()) {
                response.headers_mut().insert("retry-after", value);
            }
            response
        }
        PostSessionResult::InvalidKey => {
            error_response(StatusCode::UNAUTHORIZED, "invalid_api_key")
        }
        PostSessionResult::Issued { token, expires_at } => {
            let mut response = (
                StatusCode::OK,
                Json(json!({ "authenticated": true, "expiresAt": expires_at })),
            )
                .into_response();
            response
                .headers_mut()
                .insert("cache-control", HeaderValue::from_static("no-store"));
            if let Ok(value) =
                HeaderValue::from_str(&cookie_string(&token, SESSION_SECONDS, secure))
            {
                response.headers_mut().insert("set-cookie", value);
            }
            response
        }
    }
}

async fn get_session(State(auth): State<Arc<SessionAuth>>, headers: HeaderMap) -> Response {
    let cookie = cookie_header(&headers);
    match auth.session_expiry(cookie.as_deref(), now_ms()) {
        Some(expires_at) => {
            let mut response = (
                StatusCode::OK,
                Json(json!({ "authenticated": true, "expiresAt": expires_at })),
            )
                .into_response();
            response
                .headers_mut()
                .insert("cache-control", HeaderValue::from_static("no-store"));
            response
        }
        None => error_response(StatusCode::UNAUTHORIZED, "unauthorized"),
    }
}

async fn delete_session(State(auth): State<Arc<SessionAuth>>, headers: HeaderMap) -> Response {
    if !browser_request(&headers) {
        return error_response(StatusCode::FORBIDDEN, "invalid_request_origin");
    }
    let cookie = cookie_header(&headers);
    auth.logout(cookie.as_deref());
    let secure = secure_request(&headers);
    let mut response = StatusCode::NO_CONTENT.into_response();
    response
        .headers_mut()
        .insert("cache-control", HeaderValue::from_static("no-store"));
    if let Ok(value) = HeaderValue::from_str(&cookie_string("", 0, secure)) {
        response.headers_mut().insert("set-cookie", value);
    }
    response
}
