use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::json;
use setups_mdclient::ApiCandleSource;
use setups_replay::{CandleSource, CandleSourceQuery};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

type Captured = Arc<Mutex<Vec<(Option<String>, HashMap<String, String>)>>>;

async fn candles_handler(
    State(captured): State<Captured>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> (StatusCode, Json<serde_json::Value>) {
    let api_key = headers
        .get("x-api-key")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    captured
        .lock()
        .expect("capture lock")
        .push((api_key, params.clone()));
    if headers
        .get("x-api-key")
        .and_then(|value| value.to_str().ok())
        != Some("test-key")
    {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "invalid api key" })),
        );
    }
    if params.contains_key("afterUnix") {
        (
            StatusCode::OK,
            Json(json!({
                "candles": [
                    { "openTimeUnix": 200, "open": "1", "high": "2", "low": "0.5", "close": "1.5", "volume": "9", "quoteTurnover": "13.5" }
                ],
                "nextCursorUnix": null
            })),
        )
    } else {
        (
            StatusCode::OK,
            Json(json!({
                "candles": [
                    { "openTimeUnix": 100, "open": "1", "high": "2", "low": "0.5", "close": "1.25", "volume": "8", "quoteTurnover": "10" }
                ],
                "nextCursorUnix": 100
            })),
        )
    }
}

async fn spawn_server() -> (String, Captured) {
    let captured: Captured = Arc::new(Mutex::new(Vec::new()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local address");
    let app = Router::new()
        .route("/v1/candles", get(candles_handler))
        .with_state(captured.clone());
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("server serves");
    });
    (format!("http://{addr}"), captured)
}

#[tokio::test]
async fn api_candle_source_reads_pages_and_maps_bars() {
    let (base_url, captured) = spawn_server().await;
    let source = ApiCandleSource::new(base_url, "test-key", 42);

    let page_one = source
        .read(&CandleSourceQuery {
            from_unix: 0,
            to_unix: 1000,
            after_unix: None,
            limit: Some(5000),
        })
        .await
        .expect("first page");
    assert_eq!(page_one.bars.len(), 1);
    assert_eq!(page_one.bars[0].open_time_unix, 100);
    assert_eq!(page_one.bars[0].close, "1.25");
    assert_eq!(page_one.bars[0].volume, "8");
    assert_eq!(page_one.bars[0].quote_turnover, "10");
    assert_eq!(page_one.next_cursor_unix, Some(100));

    let page_two = source
        .read(&CandleSourceQuery {
            from_unix: 0,
            to_unix: 1000,
            after_unix: Some(100),
            limit: Some(5000),
        })
        .await
        .expect("second page");
    assert_eq!(page_two.bars[0].quote_turnover, "13.5");
    assert_eq!(page_two.next_cursor_unix, None);

    let calls = captured.lock().expect("capture lock");
    assert_eq!(calls.len(), 2);
    assert_eq!(
        calls[0].1.get("instrumentId").map(String::as_str),
        Some("42")
    );
    assert_eq!(calls[0].1.get("limit").map(String::as_str), Some("5000"));
    assert!(!calls[0].1.contains_key("afterUnix"));
    assert_eq!(calls[1].1.get("afterUnix").map(String::as_str), Some("100"));
}

#[tokio::test]
async fn api_candle_source_error_format_matches_ts() {
    let (base_url, _captured) = spawn_server().await;
    let source = ApiCandleSource::new(base_url, "wrong-key", 42);
    let error = source
        .read(&CandleSourceQuery {
            from_unix: 0,
            to_unix: 1000,
            after_unix: None,
            limit: None,
        })
        .await
        .expect_err("unauthorized request fails");
    assert!(
        error.starts_with("market data api 401"),
        "unexpected error text: {error}"
    );
}
