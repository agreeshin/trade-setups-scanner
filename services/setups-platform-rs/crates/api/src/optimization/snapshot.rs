use super::JobError;
use crate::config::OptimizationConfig;
use futures::StreamExt;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use setups_analytics::OhlcCandle;
use setups_domain::DetectorBar;
use setups_mdclient::InstrumentsResponse;
use setups_optimizer::{ContinuousContext, InputBounds, InstrumentInput, StudyInput, StudyRequest};
use sha2::{Digest, Sha256};
use std::{mem::size_of, time::Duration};

const MAX_HTTP_BYTES: usize = 8 * 1024 * 1024;
const PAGE_SIZE: usize = 5000;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SnapshotPage {
    candles: Vec<SnapshotCandle>,
    // A plain Option accepts omission; deserialize_with requires the property.
    #[serde(deserialize_with = "required_cursor")]
    next_cursor_unix: Option<i64>,
}
fn required_cursor<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<i64>, D::Error> {
    Option::<i64>::deserialize(deserializer)
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct SnapshotCandle {
    instrument_id: i64,
    open_time_unix: i64,
    open: String,
    high: String,
    low: String,
    close: String,
    volume: String,
    quote_turnover: String,
}

#[derive(Clone, Copy)]
struct SnapshotBudget {
    limit: usize,
    setting: &'static str,
}

impl SnapshotBudget {
    fn error(self) -> JobError {
        let Self { limit, setting } = self;
        JobError::new(
            "resource_limit",
            format!(
                "Снимок превышает {setting}={limit}; уменьшите число инструментов/период/прогрев или увеличьте локальный лимит памяти."
            ),
        )
    }
}

async fn read<T: DeserializeOwned>(
    request: reqwest::RequestBuilder,
    cap: usize,
    budget: SnapshotBudget,
) -> Result<T, JobError> {
    let response = request
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(|_| {
            JobError::new(
                "market_data_unavailable",
                "Не удалось прочитать API рыночных данных",
            )
        })?;
    if !response.status().is_success() {
        return Err(JobError::new(
            "market_data_unavailable",
            format!(
                "API рыночных данных вернул HTTP {}",
                response.status().as_u16()
            ),
        ));
    }
    if response
        .content_length()
        .is_some_and(|len| len > cap as u64)
    {
        return Err(budget.error());
    }
    let mut stream = response.bytes_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| {
            JobError::new(
                "market_data_unavailable",
                "Прервано чтение API рыночных данных",
            )
        })?;
        if bytes.len().saturating_add(chunk.len()) > cap {
            return Err(budget.error());
        }
        bytes
            .try_reserve_exact(chunk.len())
            .map_err(|_| budget.error())?;
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| {
        JobError::new(
            "invalid_market_data",
            "Неверный JSON ответ API рыночных данных",
        )
    })
}

fn value(text: &str, positive: bool) -> Result<f64, JobError> {
    let n = text
        .parse::<f64>()
        .map_err(|_| JobError::new("invalid_market_data", "Неверное числовое значение свечи"))?;
    if !n.is_finite() || if positive { n <= 0.0 } else { n < 0.0 } {
        return Err(JobError::new(
            "invalid_market_data",
            "Недопустимое числовое значение свечи",
        ));
    }
    Ok(n)
}

pub(super) async fn acquire(
    spec: &StudyRequest,
    url: &str,
    key: &str,
    config: &OptimizationConfig,
) -> Result<StudyInput, JobError> {
    let bounds = setups_optimizer::required_input_bounds(spec)
        .map_err(|error| JobError::new("invalid_request", error.to_string()))?;
    acquire_bounds(
        spec,
        url,
        key,
        SnapshotBudget {
            limit: config.max_snapshot_bytes,
            setting: "OPTIMIZATION_MAX_SNAPSHOT_BYTES",
        },
        bounds,
    )
    .await
}

pub(crate) async fn acquire_continuous(
    spec: &StudyRequest,
    context: ContinuousContext,
    url: &str,
    key: &str,
    config: &OptimizationConfig,
) -> Result<StudyInput, JobError> {
    let bounds = setups_optimizer::required_continuous_input_bounds(spec, context)
        .map_err(|error| JobError::new("invalid_request", error.to_string()))?;
    acquire_bounds(
        spec,
        url,
        key,
        SnapshotBudget {
            limit: config.max_snapshot_bytes,
            setting: "CONTINUOUS_MAX_SNAPSHOT_BYTES",
        },
        bounds,
    )
    .await
}

async fn acquire_bounds(
    spec: &StudyRequest,
    url: &str,
    key: &str,
    budget: SnapshotBudget,
    bounds: InputBounds,
) -> Result<StudyInput, JobError> {
    let client = reqwest::Client::new();
    let url = url.trim_end_matches('/');
    let cap = budget.limit;
    // Account for the bounded wire buffer and decoded page simultaneously.
    let catalog: InstrumentsResponse = read(
        client
            .get(format!("{url}/v1/instruments"))
            .header("x-api-key", key),
        MAX_HTTP_BYTES.min(cap / 4),
        budget,
    )
    .await?;
    let mut input = StudyInput {
        instruments: Vec::with_capacity(spec.instruments.len()),
        snapshot_hash: String::new(),
        captured_at: chrono::Utc::now().to_rfc3339(),
    };
    // Resolve only the fixed scope, then release the potentially much larger
    // catalog before retaining candles. Delisted instruments are not filtered.
    let mut ids = Vec::with_capacity(spec.instruments.len());
    for instrument in &spec.instruments {
        let id = catalog
            .instruments
            .iter()
            .find(|row| {
                row.exchange == instrument.exchange
                    && row.category == instrument.category
                    && row.symbol == instrument.symbol
            })
            .ok_or_else(|| {
                JobError::new(
                    "instrument_not_found",
                    "Инструмент отсутствует в каталоге рыночных данных",
                )
            })?
            .id;
        ids.push(id);
    }
    drop(catalog);
    let mut retained = size_of::<StudyInput>()
        + input.instruments.capacity() * size_of::<InstrumentInput>()
        + input.captured_at.capacity()
        + 64
        + ids.capacity() * size_of::<i64>();
    let mut hash = Sha256::new();
    // Bounds and ordered timestamps encode absent minutes, including completely
    // empty instruments, without a second inconsistent coverage API read.
    hash.update(serde_json::to_vec(&bounds).expect("input bounds serialize"));
    for (instrument, instrument_id) in spec.instruments.iter().zip(ids.iter()) {
        hash.update(serde_json::to_vec(instrument).expect("instrument serialize"));
        let mut item = InstrumentInput {
            instrument: instrument.clone(),
            detector_bars: Vec::new(),
            candles: Vec::new(),
        };
        retained += item.instrument.exchange.capacity()
            + item.instrument.category.capacity()
            + item.instrument.symbol.capacity();
        let mut cursor = None;
        loop {
            let remaining = cap.saturating_sub(retained);
            if remaining < 4096 {
                return Err(budget.error());
            }
            let page_size = PAGE_SIZE.min(remaining / 2048).max(1);
            let mut request = client
                .get(format!("{url}/v1/candles"))
                .header("x-api-key", key)
                .query(&[
                    ("instrumentId", instrument_id.to_string()),
                    ("fromUnix", bounds.from_unix.to_string()),
                    ("toUnix", bounds.to_unix.to_string()),
                    ("limit", page_size.to_string()),
                ]);
            if let Some(after) = cursor {
                request = request.query(&[("afterUnix", after)]);
            }
            let page: SnapshotPage =
                read(request, MAX_HTTP_BYTES.min(remaining / 4), budget).await?;
            let page_bytes = page.candles.capacity() * size_of::<SnapshotCandle>()
                + page
                    .candles
                    .iter()
                    .map(|c| {
                        c.open.capacity()
                            + c.high.capacity()
                            + c.low.capacity()
                            + c.close.capacity()
                            + c.volume.capacity()
                            + c.quote_turnover.capacity()
                    })
                    .sum::<usize>();
            if retained.saturating_add(page_bytes) > cap {
                return Err(budget.error());
            }
            if page.candles.len() > page_size
                || (page.candles.is_empty() && page.next_cursor_unix.is_some())
            {
                return Err(JobError::new(
                    "invalid_market_data",
                    "Неверный размер/курсор страницы свечей",
                ));
            }
            let detector_count = page
                .candles
                .iter()
                .filter(|c| {
                    c.open_time_unix >= bounds.detector_from_unix
                        && c.open_time_unix < bounds.detector_to_unix
                })
                .count();
            let candle_count = page
                .candles
                .iter()
                .filter(|c| {
                    c.open_time_unix >= bounds.candles_from_unix
                        && c.open_time_unix < bounds.candles_to_unix
                })
                .count();
            let before = item.detector_bars.capacity() * size_of::<DetectorBar>()
                + item.candles.capacity() * size_of::<OhlcCandle>();
            let additional =
                detector_count * size_of::<DetectorBar>() + candle_count * size_of::<OhlcCandle>();
            if retained
                .saturating_add(page_bytes)
                .saturating_add(additional)
                > cap
            {
                return Err(budget.error());
            }
            item.detector_bars
                .try_reserve_exact(detector_count)
                .map_err(|_| budget.error())?;
            item.candles
                .try_reserve_exact(candle_count)
                .map_err(|_| budget.error())?;
            let after = item.detector_bars.capacity() * size_of::<DetectorBar>()
                + item.candles.capacity() * size_of::<OhlcCandle>();
            retained = retained.saturating_add(after - before);
            if retained.saturating_add(page_bytes) > cap {
                return Err(budget.error());
            }
            let mut last = cursor;
            for candle in page.candles {
                let time = candle.open_time_unix;
                if time % 60 != 0
                    || candle.instrument_id != *instrument_id
                    || time < bounds.from_unix
                    || time >= bounds.to_unix
                    || last.is_some_and(|prev| time <= prev)
                {
                    return Err(JobError::new(
                        "invalid_market_data",
                        "Свечи не упорядочены, дублируются или выходят за границы запроса",
                    ));
                }
                let open = value(&candle.open, true)?;
                let high = value(&candle.high, true)?;
                let low = value(&candle.low, true)?;
                let close = value(&candle.close, true)?;
                value(&candle.volume, false)?;
                value(&candle.quote_turnover, false)?;
                if low > open || low > close || high < open || high < close || low > high {
                    return Err(JobError::new(
                        "invalid_market_data",
                        "Нарушены OHLC границы свечи",
                    ));
                }
                hash.update(serde_json::to_vec(&candle).expect("candle serialize"));
                if time >= bounds.detector_from_unix && time < bounds.detector_to_unix {
                    retained = retained
                        .saturating_add(candle.close.capacity() + candle.quote_turnover.capacity());
                    item.detector_bars.push(DetectorBar {
                        open_time_unix: time,
                        close: candle.close,
                        quote_turnover: candle.quote_turnover,
                    });
                }
                if time >= bounds.candles_from_unix && time < bounds.candles_to_unix {
                    item.candles.push(OhlcCandle {
                        open_time_unix: time,
                        open,
                        high,
                        low,
                        close,
                    });
                }
                if retained > cap {
                    return Err(budget.error());
                }
                last = Some(time);
            }
            match page.next_cursor_unix {
                None => break,
                Some(next)
                    if last == Some(next) && cursor.is_none_or(|previous| next > previous) =>
                {
                    cursor = Some(next)
                }
                _ => {
                    return Err(JobError::new(
                        "invalid_market_data",
                        "Пагинация свечей не продвигается",
                    ));
                }
            }
        }
        input.instruments.push(item);
    }
    input.snapshot_hash = format!("{:x}", hash.finalize());
    Ok(input)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> StudyRequest {
        serde_json::from_value(serde_json::json!({
            "algorithmId": "price_volume_spike",
            "algorithmVersion": "1.1.1",
            "algorithmParams": setups_domain::SpikeParams::default(),
            "algorithmSearch": {},
            "analyticsParams": {"clusterEnabled": false, "clusterGapMinutes": 480, "timeShiftMinutes": 0, "priceChangePct": null},
            "analyticsSearch": {},
            "instruments": [{"exchange": "bybit", "category": "linear", "symbol": "BTCUSDT"}],
            "fromUnix": 1_700_000_040, "toUnix": 1_700_003_640,
            "direction": "short", "setupMaturity": 0,
            "horizonMinutes": 60, "tpLevels": [4], "slLevels": [4],
            "minClosedPerCell": 1, "minSetupsPerDay": 0,
            "seed": 42, "maxTrials": 1, "maxSeconds": 10
        })).unwrap()
    }

    async fn acquisition_errors(cap: usize) -> (JobError, JobError) {
        let app = axum::Router::new().route("/v1/instruments", axum::routing::get(|| async {
            axum::Json(serde_json::json!({"instruments": [{"id": 42, "exchange": "bybit", "category": "linear", "symbol": "BTCUSDT", "status": "active"}]}))
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let config = OptimizationConfig {
            max_snapshot_bytes: cap,
            ..Default::default()
        };
        let spec = request();
        let manual = acquire(&spec, &url, "fixture-key", &config).await;
        let continuous = acquire_continuous(
            &spec,
            ContinuousContext {
                observation_cutoff: spec.to_unix,
            },
            &url,
            "fixture-key",
            &config,
        )
        .await;
        server.abort();
        let _ = server.await;
        (manual.unwrap_err(), continuous.unwrap_err())
    }

    fn assert_lane_errors(manual: JobError, continuous: JobError, cap: usize) {
        assert_eq!(manual.code, "resource_limit");
        assert_eq!(continuous.code, "resource_limit");
        assert!(
            manual
                .message
                .contains(&format!("OPTIMIZATION_MAX_SNAPSHOT_BYTES={cap}"))
        );
        assert!(!manual.message.contains("CONTINUOUS_MAX_SNAPSHOT_BYTES"));
        assert!(
            continuous
                .message
                .contains(&format!("CONTINUOUS_MAX_SNAPSHOT_BYTES={cap}"))
        );
        assert!(
            !continuous
                .message
                .contains("OPTIMIZATION_MAX_SNAPSHOT_BYTES")
        );
    }

    #[tokio::test]
    async fn snapshot_http_body_limit_errors_name_the_owning_lane_setting() {
        let (manual, continuous) = acquisition_errors(1).await;
        assert_lane_errors(manual, continuous, 1);
    }

    #[tokio::test]
    async fn snapshot_retained_budget_errors_name_the_owning_lane_setting() {
        let (manual, continuous) = acquisition_errors(4096).await;
        assert_lane_errors(manual, continuous, 4096);
    }

    #[test]
    fn cursor_is_required_but_explicit_null_is_valid() {
        assert!(
            serde_json::from_value::<SnapshotPage>(
                serde_json::json!({"candles":[],"nextCursorUnix":null})
            )
            .is_ok()
        );
        assert!(serde_json::from_value::<SnapshotPage>(serde_json::json!({"candles":[]})).is_err());
    }
}
