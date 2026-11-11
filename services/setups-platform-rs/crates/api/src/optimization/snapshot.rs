use super::JobError;
use crate::config::OptimizationConfig;
use futures::StreamExt;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use setups_analytics::OhlcCandle;
use setups_domain::DetectorBar;
use setups_mdclient::InstrumentsResponse;
use setups_optimizer::{InstrumentInput, StudyInput, StudyRequest};
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

fn resource(limit: usize) -> JobError {
    JobError::new(
        "resource_limit",
        format!(
            "Снимок превышает OPTIMIZATION_MAX_SNAPSHOT_BYTES={limit}; уменьшите число инструментов/период/прогрев или увеличьте локальный лимит памяти."
        ),
    )
}

async fn read<T: DeserializeOwned>(
    request: reqwest::RequestBuilder,
    cap: usize,
    snapshot_cap: usize,
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
        return Err(resource(snapshot_cap));
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
            return Err(resource(snapshot_cap));
        }
        bytes
            .try_reserve_exact(chunk.len())
            .map_err(|_| resource(snapshot_cap))?;
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
    let client = reqwest::Client::new();
    let url = url.trim_end_matches('/');
    let cap = config.max_snapshot_bytes;
    // Account for the bounded wire buffer and decoded page simultaneously.
    let catalog: InstrumentsResponse = read(
        client
            .get(format!("{url}/v1/instruments"))
            .header("x-api-key", key),
        MAX_HTTP_BYTES.min(cap / 4),
        cap,
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
                return Err(resource(cap));
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
            let page: SnapshotPage = read(request, MAX_HTTP_BYTES.min(remaining / 4), cap).await?;
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
                return Err(resource(cap));
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
                return Err(resource(cap));
            }
            item.detector_bars
                .try_reserve_exact(detector_count)
                .map_err(|_| resource(cap))?;
            item.candles
                .try_reserve_exact(candle_count)
                .map_err(|_| resource(cap))?;
            let after = item.detector_bars.capacity() * size_of::<DetectorBar>()
                + item.candles.capacity() * size_of::<OhlcCandle>();
            retained = retained.saturating_add(after - before);
            if retained.saturating_add(page_bytes) > cap {
                return Err(resource(cap));
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
                    return Err(resource(cap));
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
