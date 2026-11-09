use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;
use std::time::Duration;

pub mod source;

pub use source::ApiCandleSource;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogInstrument {
    pub id: i64,
    pub exchange: String,
    pub category: String,
    pub symbol: String,
    pub status: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstrumentsResponse {
    pub instruments: Vec<CatalogInstrument>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CandleDto {
    pub open_time_unix: i64,
    pub open: String,
    pub high: String,
    pub low: String,
    pub close: String,
    pub volume: String,
    pub quote_turnover: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CandlesResponse {
    pub candles: Vec<CandleDto>,
    pub next_cursor_unix: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CandlesQuery {
    pub instrument_id: i64,
    pub from_unix: i64,
    pub to_unix: i64,
    pub after_unix: Option<i64>,
    pub limit: Option<u32>,
}

#[derive(Debug)]
pub enum MdClientError {
    Http { status: u16, body: String },
    Network(String),
    Decode(String),
}

impl fmt::Display for MdClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MdClientError::Http { status, body } => {
                write!(f, "market data api {status}: {body}")
            }
            MdClientError::Network(message) => write!(f, "market data network error: {message}"),
            MdClientError::Decode(message) => write!(f, "market data decode error: {message}"),
        }
    }
}

impl std::error::Error for MdClientError {}

pub struct MarketDataClient {
    base_url: String,
    api_key: String,
    http: reqwest::Client,
}

impl MarketDataClient {
    pub fn new(base_url: impl Into<String>, api_key: impl Into<String>) -> Self {
        MarketDataClient {
            base_url: base_url.into(),
            api_key: api_key.into(),
            http: reqwest::Client::new(),
        }
    }

    pub async fn instruments(&self) -> Result<Vec<CatalogInstrument>, MdClientError> {
        let response = self
            .http
            .get(format!("{}/v1/instruments", self.base_url))
            .header("x-api-key", &self.api_key)
            .timeout(Duration::from_secs(10))
            .send()
            .await
            .map_err(|error| MdClientError::Network(error.to_string()))?;
        let body: InstrumentsResponse = Self::decode(response).await?;
        Ok(body.instruments)
    }

    pub async fn candles(
        &self,
        query: &CandlesQuery,
        timeout: Duration,
    ) -> Result<CandlesResponse, MdClientError> {
        let mut request = self
            .http
            .get(format!("{}/v1/candles", self.base_url))
            .header("x-api-key", &self.api_key)
            .timeout(timeout)
            .query(&[
                ("instrumentId", query.instrument_id.to_string()),
                ("fromUnix", query.from_unix.to_string()),
                ("toUnix", query.to_unix.to_string()),
            ]);
        if let Some(limit) = query.limit {
            request = request.query(&[("limit", limit.to_string())]);
        }
        if let Some(after) = query.after_unix {
            request = request.query(&[("afterUnix", after.to_string())]);
        }
        let response = request
            .send()
            .await
            .map_err(|error| MdClientError::Network(error.to_string()))?;
        Self::decode(response).await
    }

    pub async fn ready(&self) -> Result<bool, MdClientError> {
        let response = self
            .http
            .get(format!("{}/ready", self.base_url))
            .timeout(Duration::from_secs(5))
            .send()
            .await
            .map_err(|error| MdClientError::Network(error.to_string()))?;
        Ok(response.status().is_success())
    }

    pub async fn proxy(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
    ) -> Result<(u16, Option<Value>), MdClientError> {
        let url = format!("{}/v1{path}", self.base_url);
        let method = reqwest::Method::from_bytes(method.as_bytes())
            .map_err(|error| MdClientError::Network(error.to_string()))?;
        let mut request = self
            .http
            .request(method, url)
            .header("x-api-key", &self.api_key)
            .timeout(Duration::from_secs(30));
        if let Some(payload) = body {
            request = request
                .header("content-type", "application/json")
                .json(payload);
        }
        let response = request
            .send()
            .await
            .map_err(|error| MdClientError::Network(error.to_string()))?;
        let status = response.status().as_u16();
        if !response.status().is_success() {
            return Ok((status, None));
        }
        let payload = response.json::<Value>().await.ok();
        Ok((status, payload))
    }

    async fn decode<T: serde::de::DeserializeOwned>(
        response: reqwest::Response,
    ) -> Result<T, MdClientError> {
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(MdClientError::Http {
                status: status.as_u16(),
                body: body.chars().take(200).collect(),
            });
        }
        response
            .json::<T>()
            .await
            .map_err(|error| MdClientError::Decode(error.to_string()))
    }
}
