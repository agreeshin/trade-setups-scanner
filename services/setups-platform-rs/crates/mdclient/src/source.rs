use crate::{CandlesQuery, MarketDataClient};
use setups_replay::{CandleBarDto, CandlePage, CandleSource, CandleSourceQuery};
use std::time::Duration;

const READ_TIMEOUT: Duration = Duration::from_millis(15_000);

pub struct ApiCandleSource {
    client: MarketDataClient,
    instrument_id: i64,
}

impl ApiCandleSource {
    pub fn new(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        instrument_id: i64,
    ) -> Self {
        ApiCandleSource {
            client: MarketDataClient::new(base_url, api_key),
            instrument_id,
        }
    }
}

impl CandleSource for ApiCandleSource {
    async fn read(&self, query: &CandleSourceQuery) -> Result<CandlePage, String> {
        let response = self
            .client
            .candles(
                &CandlesQuery {
                    instrument_id: self.instrument_id,
                    from_unix: query.from_unix,
                    to_unix: query.to_unix,
                    after_unix: query.after_unix,
                    limit: query.limit,
                },
                READ_TIMEOUT,
            )
            .await
            .map_err(|error| error.to_string())?;
        Ok(CandlePage {
            bars: response
                .candles
                .into_iter()
                .map(|candle| CandleBarDto {
                    open_time_unix: candle.open_time_unix,
                    close: candle.close,
                    volume: candle.volume,
                    quote_turnover: candle.quote_turnover,
                })
                .collect(),
            next_cursor_unix: response.next_cursor_unix,
        })
    }
}
