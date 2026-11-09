use hmac::{Hmac, Mac};
use rand::RngCore;
use sha2::Sha256;
use std::sync::Mutex;

pub const API_KEY_HEADER: &str = "x-api-key";
pub const COOKIE_NAME: &str = "platform_session";
pub const SESSION_SECONDS: i64 = 24 * 60 * 60;

const MAX_SESSIONS: usize = 128;
const MAX_FAILURES: u32 = 10;
const RATE_WINDOW_MS: i64 = 60_000;
const NONCE_BYTES: usize = 16;
const SIGNING_KEY_BYTES: usize = 32;

type HmacSha256 = Hmac<Sha256>;

#[derive(Default)]
struct SessionsState {
    entries: Vec<(String, i64)>,
}

#[derive(Default)]
struct RateState {
    failures: u32,
    window_end: i64,
}

#[derive(Debug)]
pub enum PostSessionResult {
    Issued { token: String, expires_at: i64 },
    InvalidKey,
    RateLimited { retry_after_seconds: u32 },
}

pub struct SessionAuth {
    keys: Vec<String>,
    signing_key: [u8; SIGNING_KEY_BYTES],
    sessions: Mutex<SessionsState>,
    rate: Mutex<RateState>,
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock after unix epoch")
        .as_millis() as i64
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from_digit(u32::from(byte >> 4), 16).expect("hex digit"));
        out.push(char::from_digit(u32::from(byte & 0x0f), 16).expect("hex digit"));
    }
    out
}

fn timing_safe_equal(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in left.iter().zip(right.iter()) {
        diff |= a ^ b;
    }
    diff == 0
}

fn is_lower_hex(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn is_digits(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit())
}

pub fn cookie_token(cookie_header: Option<&str>) -> Option<&str> {
    let header = cookie_header?;
    header
        .split(';')
        .map(str::trim)
        .find(|part| {
            part.starts_with(COOKIE_NAME) && part.as_bytes().get(COOKIE_NAME.len()) == Some(&b'=')
        })
        .map(|part| &part[COOKIE_NAME.len() + 1..])
}

pub fn cookie_string(token: &str, max_age: i64, secure: bool) -> String {
    format!(
        "{COOKIE_NAME}={token}; Path=/v1; HttpOnly; SameSite=Strict; Max-Age={max_age}{}",
        if secure { "; Secure" } else { "" }
    )
}

impl SessionAuth {
    pub fn new(valid_keys: &[String]) -> Self {
        let mut signing_key = [0u8; SIGNING_KEY_BYTES];
        rand::rng().fill_bytes(&mut signing_key);
        Self::with_signing_key(valid_keys, signing_key)
    }

    pub fn with_signing_key(valid_keys: &[String], signing_key: [u8; SIGNING_KEY_BYTES]) -> Self {
        SessionAuth {
            keys: valid_keys
                .iter()
                .filter(|key| !key.is_empty())
                .cloned()
                .collect(),
            signing_key,
            sessions: Mutex::new(SessionsState::default()),
            rate: Mutex::new(RateState::default()),
        }
    }

    pub fn valid_key(&self, value: Option<&str>) -> bool {
        let Some(value) = value else {
            return false;
        };
        self.keys
            .iter()
            .any(|key| timing_safe_equal(value.as_bytes(), key.as_bytes()))
    }

    pub fn sign(&self, value: &str) -> String {
        let mut mac =
            HmacSha256::new_from_slice(&self.signing_key).expect("hmac accepts any key length");
        mac.update(value.as_bytes());
        hex_encode(&mac.finalize().into_bytes())
    }

    fn session_get(&self, nonce: &str) -> Option<i64> {
        self.sessions
            .lock()
            .expect("sessions lock")
            .entries
            .iter()
            .find(|(entry, _)| entry == nonce)
            .map(|(_, expiry)| *expiry)
    }

    fn session_remove(&self, nonce: &str) {
        self.sessions
            .lock()
            .expect("sessions lock")
            .entries
            .retain(|(entry, _)| entry != nonce);
    }

    fn session_insert(&self, nonce: String, expires_at: i64) {
        let mut sessions = self.sessions.lock().expect("sessions lock");
        if sessions.entries.len() >= MAX_SESSIONS {
            sessions.entries.remove(0);
        }
        sessions.entries.push((nonce, expires_at));
    }

    pub fn cleanup(&self, now: i64) {
        self.sessions
            .lock()
            .expect("sessions lock")
            .entries
            .retain(|(_, expiry)| *expiry > now);
    }

    pub fn session_expiry(&self, cookie_header: Option<&str>, now: i64) -> Option<i64> {
        let token = cookie_token(cookie_header)?;
        if token.len() > 200 {
            return None;
        }
        let parts: Vec<&str> = token.split('.').collect();
        if parts.len() != 3 {
            return None;
        }
        let [expiry, nonce, signature] = parts.as_slice() else {
            return None;
        };
        if !is_digits(expiry)
            || nonce.len() != 32
            || !is_lower_hex(nonce)
            || signature.len() != 64
            || !is_lower_hex(signature)
        {
            return None;
        }
        let expected = self.sign(&format!("{expiry}.{nonce}"));
        if !timing_safe_equal(signature.as_bytes(), expected.as_bytes()) {
            return None;
        }
        let expires_at: i64 = expiry.parse().ok()?;
        if expires_at <= now || expires_at > now + SESSION_SECONDS * 1000 {
            return None;
        }
        if self.session_get(nonce) != Some(expires_at) {
            return None;
        }
        Some(expires_at)
    }

    pub fn issue_session(&self, now: i64) -> (String, i64) {
        let expires_at = now + SESSION_SECONDS * 1000;
        let mut nonce_bytes = [0u8; NONCE_BYTES];
        rand::rng().fill_bytes(&mut nonce_bytes);
        let nonce = hex_encode(&nonce_bytes);
        self.session_insert(nonce.clone(), expires_at);
        let payload = format!("{expires_at}.{nonce}");
        let token = format!("{payload}.{}", self.sign(&payload));
        (token, expires_at)
    }

    pub fn post_session(&self, api_key: Option<&str>, now: i64) -> PostSessionResult {
        {
            let mut rate = self.rate.lock().expect("rate lock");
            if now >= rate.window_end {
                rate.failures = 0;
                rate.window_end = now + RATE_WINDOW_MS;
            }
            if rate.failures >= MAX_FAILURES {
                let retry_after_seconds = ((rate.window_end - now) as f64 / 1000.0).ceil() as u32;
                return PostSessionResult::RateLimited {
                    retry_after_seconds,
                };
            }
        }
        if !self.valid_key(api_key) {
            self.rate.lock().expect("rate lock").failures += 1;
            return PostSessionResult::InvalidKey;
        }
        let (token, expires_at) = self.issue_session(now);
        PostSessionResult::Issued { token, expires_at }
    }

    pub fn logout(&self, cookie_header: Option<&str>) {
        if let Some(token) = cookie_token(cookie_header)
            && let Some(nonce) = token.split('.').nth(1)
        {
            self.session_remove(nonce);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: [u8; SIGNING_KEY_BYTES] = [7u8; SIGNING_KEY_BYTES];

    fn auth() -> SessionAuth {
        let keys = vec!["test-key".to_string()];
        SessionAuth::with_signing_key(&keys, KEY)
    }

    fn valid_token(auth: &SessionAuth, now: i64) -> String {
        let nonce = "0123456789abcdef0123456789abcdef".to_string();
        let expires_at = now + 60_000;
        auth.session_insert(nonce.clone(), expires_at);
        let payload = format!("{expires_at}.{nonce}");
        format!("{}.{}", payload, auth.sign(&payload))
    }

    #[test]
    fn sign_is_deterministic_lowercase_hex() {
        let auth = auth();
        let signature = auth.sign("1700000000.0123456789abcdef0123456789abcdef");
        assert_eq!(signature.len(), 64);
        assert!(is_lower_hex(&signature));
        assert_eq!(
            signature,
            auth.sign("1700000000.0123456789abcdef0123456789abcdef")
        );
        assert_ne!(
            signature,
            auth.sign("1700000001.0123456789abcdef0123456789abcdef")
        );
    }

    #[test]
    fn session_roundtrip_and_validation_matrix() {
        let auth = auth();
        let now = 1_700_000_000_000i64;
        let token = valid_token(&auth, now);
        let header = format!("{COOKIE_NAME}={token}");
        assert_eq!(auth.session_expiry(Some(&header), now), Some(now + 60_000));
        assert_eq!(auth.session_expiry(Some(&header), now + 61_000), None);
        assert_eq!(auth.session_expiry(Some("other=1"), now), None);
        assert_eq!(auth.session_expiry(None, now), None);

        let bad_signature = format!("{}.{}", &token[..token.len() - 1], "f");
        assert_eq!(
            auth.session_expiry(Some(&format!("{COOKIE_NAME}={bad_signature}")), now),
            None
        );

        let unknown_nonce = {
            let nonce = "ffffffffffffffffffffffffffffffff".to_string();
            let expires_at = now + 60_000;
            let payload = format!("{expires_at}.{nonce}");
            format!("{}.{}", payload, auth.sign(&payload))
        };
        assert_eq!(
            auth.session_expiry(Some(&format!("{COOKIE_NAME}={unknown_nonce}")), now),
            None
        );

        let beyond_day = {
            let nonce = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string();
            let expires_at = now + SESSION_SECONDS * 1000 + 1_000;
            auth.session_insert(nonce.clone(), expires_at);
            let payload = format!("{expires_at}.{nonce}");
            format!("{}.{}", payload, auth.sign(&payload))
        };
        assert_eq!(
            auth.session_expiry(Some(&format!("{COOKIE_NAME}={beyond_day}")), now),
            None
        );

        let long_token = format!("{COOKIE_NAME}={}", "a".repeat(201));
        assert_eq!(auth.session_expiry(Some(&long_token), now), None);

        let extra_dot = format!("{token}.x");
        assert_eq!(
            auth.session_expiry(Some(&format!("{COOKIE_NAME}={extra_dot}")), now),
            None
        );

        let short_nonce = format!("{}.{}", now + 60_000, auth.sign("x"));
        assert_eq!(
            auth.session_expiry(Some(&format!("{COOKIE_NAME}={short_nonce}")), now),
            None
        );
    }

    #[test]
    fn post_session_rate_limit_matches_ts_window() {
        let auth = auth();
        let now = 1_700_000_000_000i64;
        for _ in 0..10 {
            assert!(matches!(
                auth.post_session(Some("wrong"), now),
                PostSessionResult::InvalidKey
            ));
        }
        match auth.post_session(Some("test-key"), now) {
            PostSessionResult::RateLimited {
                retry_after_seconds,
            } => assert_eq!(retry_after_seconds, 60),
            other => panic!("expected rate limit, got {other:?}"),
        }
        match auth.post_session(Some("test-key"), now + RATE_WINDOW_MS) {
            PostSessionResult::Issued { .. } => {}
            other => panic!("expected fresh window to issue, got {other:?}"),
        }
    }

    #[test]
    fn cookie_and_logout_formats_match_ts() {
        let auth = auth();
        assert_eq!(
            cookie_string("tok", 86400, false),
            "platform_session=tok; Path=/v1; HttpOnly; SameSite=Strict; Max-Age=86400"
        );
        assert_eq!(
            cookie_string("tok", 86400, true),
            "platform_session=tok; Path=/v1; HttpOnly; SameSite=Strict; Max-Age=86400; Secure"
        );
        let now = 1_700_000_000_000i64;
        let token = valid_token(&auth, now);
        let header = format!("{COOKIE_NAME}={token}");
        auth.logout(Some(&header));
        assert_eq!(auth.session_expiry(Some(&header), now), None);
    }

    #[test]
    fn sessions_evict_oldest_beyond_capacity() {
        let auth = auth();
        let now = 1_700_000_000_000i64;
        let first = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee".to_string();
        auth.session_insert(first.clone(), now + 60_000);
        for index in 0..MAX_SESSIONS {
            let nonce = format!("{index:032x}");
            auth.session_insert(nonce, now + 60_000);
        }
        assert_eq!(auth.session_get(&first), None);
        assert_eq!(
            auth.session_get(&format!("{:032x}", MAX_SESSIONS - 1)),
            Some(now + 60_000)
        );
    }
}
