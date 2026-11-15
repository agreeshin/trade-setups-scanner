use crate::RelayError;
use serde::Deserialize;
use serde_json::{Value, json};
use std::time::Duration;

pub(crate) const TELEGRAM_TIMEOUT: Duration = Duration::from_secs(4);
const RESPONSE_LIMIT: usize = 32 * 1024;
pub(crate) struct Telegram {
    client: reqwest::Client,
    token: String,
    bot_id: i64,
    chat_id: i64,
    origin: String,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TelegramError {
    Timeout,
    Transport,
    InvalidResponse,
    Permission,
    Rejected,
    RateLimited(u32),
}
impl TelegramError {
    pub fn code(self) -> &'static str {
        match self {
            Self::Timeout => "telegram_timeout",
            Self::Transport => "telegram_unavailable",
            Self::InvalidResponse => "telegram_invalid_response",
            Self::Permission => "telegram_permission_denied",
            Self::Rejected => "telegram_rejected",
            Self::RateLimited(_) => "telegram_rate_limited",
        }
    }
}
#[derive(Deserialize)]
struct Envelope {
    ok: bool,
    result: Option<Value>,
    error_code: Option<u16>,
    parameters: Option<Parameters>,
}
#[derive(Deserialize)]
struct Parameters {
    retry_after: Option<u32>,
}
impl Telegram {
    pub fn new(token: String, bot_id: i64, chat_id: i64) -> Result<Self, RelayError> {
        let client = reqwest::Client::builder()
            .https_only(true)
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(2))
            .timeout(TELEGRAM_TIMEOUT)
            .pool_max_idle_per_host(2)
            .pool_idle_timeout(Duration::from_secs(30))
            .build()
            .map_err(|_| RelayError::Config("telegram_client_invalid"))?;
        Ok(Self {
            client,
            token,
            bot_id,
            chat_id,
            origin: "https://api.telegram.org".into(),
        })
    }
    fn request(&self, method: &'static str) -> reqwest::RequestBuilder {
        // Neither URL nor reqwest error/Telegram description may leave this module.
        self.client
            .post(format!("{}/bot{}/{method}", self.origin, self.token))
    }
    async fn call(&self, method: &'static str, body: Value) -> Result<Value, TelegramError> {
        self.response(self.request(method).json(&body)).await
    }
    async fn response(&self, request: reqwest::RequestBuilder) -> Result<Value, TelegramError> {
        let mut response = request.send().await.map_err(classify_transport)?;
        let status = response.status().as_u16();
        if (300..400).contains(&status) {
            return Err(TelegramError::InvalidResponse);
        }
        if response
            .content_length()
            .is_some_and(|size| size > RESPONSE_LIMIT as u64)
        {
            return Err(TelegramError::InvalidResponse);
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(classify_transport)? {
            if chunk.len() > RESPONSE_LIMIT.saturating_sub(bytes.len()) {
                return Err(TelegramError::InvalidResponse);
            }
            bytes.extend_from_slice(&chunk);
        }
        let envelope: Envelope =
            serde_json::from_slice(&bytes).map_err(|_| TelegramError::InvalidResponse)?;
        if status == 429 || envelope.error_code == Some(429) {
            return Err(TelegramError::RateLimited(
                envelope
                    .parameters
                    .and_then(|p| p.retry_after)
                    .unwrap_or(1)
                    .clamp(1, 86400),
            ));
        }
        if matches!(status, 401 | 403) || matches!(envelope.error_code, Some(401 | 403)) {
            return Err(TelegramError::Permission);
        }
        if status >= 500 {
            return Err(TelegramError::Transport);
        }
        if !(200..300).contains(&status) || !envelope.ok {
            return Err(TelegramError::Rejected);
        }
        envelope.result.ok_or(TelegramError::InvalidResponse)
    }
    pub async fn send(&self, text: &str) -> Result<i64, TelegramError> {
        let value=self.call("sendMessage",json!({"chat_id":self.chat_id,"text":text,"link_preview_options":{"is_disabled":true}})).await?;
        message_id(value)
    }
    pub async fn send_rich(
        &self,
        message: &setups_relay_protocol::v2::RelayMessage,
        png: Option<Vec<u8>>,
    ) -> Result<i64, TelegramError> {
        // Only the authenticated protocol's validated inline bytes reach this
        // boundary. No URL/file_id, parse_mode, second message or photo fallback.
        let value = if let Some(png) = png {
            let entities = serde_json::to_string(&message.entities)
                .map_err(|_| TelegramError::InvalidResponse)?;
            let photo = reqwest::multipart::Part::bytes(png)
                .file_name("expectancy.png")
                .mime_str("image/png")
                .map_err(|_| TelegramError::InvalidResponse)?;
            let form = reqwest::multipart::Form::new()
                .text("chat_id", self.chat_id.to_string())
                .text("caption", message.text.clone())
                .text("caption_entities", entities)
                .part("photo", photo);
            self.response(self.request("sendPhoto").multipart(form))
                .await?
        } else {
            self.call(
                "sendMessage",
                json!({
                    "chat_id": self.chat_id,
                    "text": message.text,
                    "entities": message.entities,
                    "link_preview_options": {"is_disabled": true},
                }),
            )
            .await?
        };
        message_id(value)
    }
    pub async fn permission(&self) -> Result<(), TelegramError> {
        let value = self
            .call(
                "getChatMember",
                json!({"chat_id":self.chat_id,"user_id":self.bot_id}),
            )
            .await?;
        if value.pointer("/user/id").and_then(Value::as_i64) != Some(self.bot_id)
            || value.get("status").and_then(Value::as_str) != Some("administrator")
            || value.get("can_post_messages").and_then(Value::as_bool) != Some(true)
        {
            return Err(TelegramError::Permission);
        }
        Ok(())
    }
    #[cfg(test)]
    pub fn for_test(origin: String, timeout: Duration) -> Self {
        Self {
            client: reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(timeout)
                .build()
                .unwrap(),
            token: "12345:TEST_SENTINEL_TOKEN_NEVER_LOG".into(),
            bot_id: 12345,
            chat_id: -10012345,
            origin,
        }
    }
}
fn message_id(value: Value) -> Result<i64, TelegramError> {
    value
        .get("message_id")
        .and_then(Value::as_i64)
        .filter(|id| *id > 0)
        .ok_or(TelegramError::InvalidResponse)
}
fn classify_transport(error: reqwest::Error) -> TelegramError {
    if error.is_timeout() {
        TelegramError::Timeout
    } else {
        TelegramError::Transport
    }
}
