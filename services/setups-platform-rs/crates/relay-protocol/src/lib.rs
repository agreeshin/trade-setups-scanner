//! Bounded Telegram relay v1 wire contract. HTTP adapters must reject duplicate auth
//! headers and bound streamed bodies before buffering; signatures cover raw bytes.
use hmac::{Hmac, Mac};
use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest, Sha256};
use std::{fmt, fs::File, io::Read, path::Path};

pub const VERSION: u8 = 1;
pub const VERSION_HEADER_VALUE: &str = "1";
pub const MESSAGES_PATH: &str = "/v1/messages";
pub const STATUS_PATH: &str = "/v1/status";
pub const HEADER_VERSION: &str = "x-relay-version";
pub const HEADER_TIMESTAMP: &str = "x-relay-timestamp";
pub const HEADER_NONCE: &str = "x-relay-nonce";
pub const HEADER_SIGNATURE: &str = "x-relay-signature";
pub const REQUEST_DOMAIN: &str = "setups-telegram-relay/request";
pub const RESPONSE_DOMAIN: &str = "setups-telegram-relay/response";
pub const MAX_BODY_BYTES: usize = 32 * 1024;
pub const MAX_ID_BYTES: usize = 128;
pub const MAX_TEXT_CHARS: usize = 4096;
pub const MAX_CLOCK_SKEW_SECONDS: u64 = 60;
pub const MAX_EXPIRY_SECONDS: i64 = 86_400;
pub const MAX_RETRY_AFTER_SECONDS: u32 = 86_400;
pub const MAX_ERROR_CODE_BYTES: usize = 64;
pub const CAPABILITY_TEXT: &str = "text";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtocolError {
    BodyTooLarge,
    InvalidJson,
    UnsupportedVersion,
    InvalidId,
    InvalidText,
    UnsupportedAttachment,
    Expired,
    InvalidExpiry,
    InvalidTimestamp,
    ClockSkew,
    InvalidNonce,
    InvalidSignature,
    InvalidRoute,
    InvalidStatus,
    InvalidErrorCode,
    InvalidRetryAfter,
    InvalidKey,
    KeyFileUnavailable,
    InsecureKeyFile,
}
impl ProtocolError {
    pub const fn code(self) -> &'static str {
        match self {
            Self::BodyTooLarge => "body_too_large",
            Self::InvalidJson => "invalid_json",
            Self::UnsupportedVersion => "unsupported_version",
            Self::InvalidId => "invalid_id",
            Self::InvalidText => "invalid_text",
            Self::UnsupportedAttachment => "unsupported_attachment",
            Self::Expired => "expired",
            Self::InvalidExpiry => "invalid_expiry",
            Self::InvalidTimestamp => "invalid_timestamp",
            Self::ClockSkew => "clock_skew",
            Self::InvalidNonce => "invalid_nonce",
            Self::InvalidSignature => "invalid_signature",
            Self::InvalidRoute => "invalid_route",
            Self::InvalidStatus => "invalid_status",
            Self::InvalidErrorCode => "invalid_error_code",
            Self::InvalidRetryAfter => "invalid_retry_after",
            Self::InvalidKey => "invalid_key",
            Self::KeyFileUnavailable => "key_file_unavailable",
            Self::InsecureKeyFile => "insecure_key_file",
        }
    }
}
impl fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code())
    }
}
impl std::error::Error for ProtocolError {}

/// Owned key material, intentionally neither Debug nor Serialize/Clone.
/// Long-lived ownership belongs to the caller's configuration, commonly via Arc.
pub struct SecretKey([u8; 32]);
impl SecretKey {
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Parse exactly 64 ASCII hexadecimal digits (without a trailing newline).
    pub fn from_hex(hex: &str) -> Result<Self, ProtocolError> {
        let mut bytes = [0_u8; 32];
        if hex.len() != 64 {
            return Err(ProtocolError::InvalidKey);
        }
        for (out, pair) in bytes.iter_mut().zip(hex.as_bytes().chunks(2)) {
            *out = (hex_digit(pair[0]).ok_or(ProtocolError::InvalidKey)? << 4)
                | hex_digit(pair[1]).ok_or(ProtocolError::InvalidKey)?;
        }
        Ok(Self(bytes))
    }

    /// Load a regular, non-symlink file with no group/other permissions and exactly
    /// 64 hex bytes plus LF. Metadata is checked on the opened descriptor too;
    /// paths and I/O details are deliberately excluded from errors.
    pub fn from_private_file(path: impl AsRef<Path>) -> Result<Self, ProtocolError> {
        let path = path.as_ref();
        let before =
            std::fs::symlink_metadata(path).map_err(|_| ProtocolError::KeyFileUnavailable)?;
        check_key_metadata(&before)?;
        let file = File::open(path).map_err(|_| ProtocolError::KeyFileUnavailable)?;
        let opened = file
            .metadata()
            .map_err(|_| ProtocolError::KeyFileUnavailable)?;
        check_key_metadata(&opened)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if before.dev() != opened.dev() || before.ino() != opened.ino() {
                return Err(ProtocolError::InsecureKeyFile);
            }
        }
        let mut encoded = [0_u8; 66];
        let length = file
            .take(66)
            .read(&mut encoded)
            .map_err(|_| ProtocolError::KeyFileUnavailable)?;
        let result = if length == 65 && encoded[64] == b'\n' {
            std::str::from_utf8(&encoded[..64])
                .map_err(|_| ProtocolError::InvalidKey)
                .and_then(Self::from_hex)
        } else {
            Err(ProtocolError::InvalidKey)
        };
        encoded.fill(0);
        result
    }
}
impl Drop for SecretKey {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}
fn check_key_metadata(metadata: &std::fs::Metadata) -> Result<(), ProtocolError> {
    if !metadata.file_type().is_file() || metadata.len() != 65 {
        return Err(ProtocolError::InsecureKeyFile);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(ProtocolError::InsecureKeyFile);
        }
    }
    #[cfg(not(unix))]
    return Err(ProtocolError::InsecureKeyFile);
    #[cfg(unix)]
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RelayMessage {
    pub version: u8,
    pub id: String,
    pub text: String,
    pub expires_at: i64,
    // deserialize_with makes this field required, unlike a default Option field.
    #[serde(deserialize_with = "required_attachment")]
    pub attachment: Option<serde_json::Value>,
}
fn required_attachment<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<serde_json::Value>, D::Error> {
    Option::<serde_json::Value>::deserialize(deserializer)
}
impl RelayMessage {
    pub fn parse(body: &[u8], now: i64) -> Result<Self, ProtocolError> {
        check_body(body)?;
        let message: Self = serde_json::from_slice(body).map_err(|_| ProtocolError::InvalidJson)?;
        message.validate(now)?;
        Ok(message)
    }
    pub fn encode(&self, now: i64) -> Result<Vec<u8>, ProtocolError> {
        self.validate(now)?;
        let body = serde_json::to_vec(self).map_err(|_| ProtocolError::InvalidJson)?;
        check_body(&body)?;
        Ok(body)
    }
    pub fn validate(&self, now: i64) -> Result<(), ProtocolError> {
        check_version(self.version)?;
        check_id(&self.id)?;
        if self.text.is_empty() || self.text.chars().count() > MAX_TEXT_CHARS {
            return Err(ProtocolError::InvalidText);
        }
        if self.attachment.is_some() {
            return Err(ProtocolError::UnsupportedAttachment);
        }
        self.validate_expiry(now)
    }
    /// Repeat immediately before each Telegram call, including retries.
    pub fn validate_expiry(&self, now: i64) -> Result<(), ProtocolError> {
        if now < 0 || self.expires_at < 0 {
            return Err(ProtocolError::InvalidExpiry);
        }
        if self.expires_at < now {
            return Err(ProtocolError::Expired);
        }
        if self.expires_at.saturating_sub(now) > MAX_EXPIRY_SECONDS {
            return Err(ProtocolError::InvalidExpiry);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RelayStatus {
    pub version: u8,
    pub capabilities: Vec<String>,
    pub telegram_ready: bool,
    pub checked_at: i64,
    pub error: Option<String>,
}
impl RelayStatus {
    pub fn validate(&self) -> Result<(), ProtocolError> {
        check_version(self.version)?;
        if self.capabilities != [CAPABILITY_TEXT] || self.checked_at < 0 {
            return Err(ProtocolError::InvalidStatus);
        }
        if let Some(error) = &self.error {
            check_error_code(error)?;
        }
        Ok(())
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelayOutcome {
    Sent,
    Duplicate,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RelayMessageResponse {
    pub version: u8,
    pub id: String,
    pub outcome: RelayOutcome,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub telegram_message_id: Option<i64>,
}
impl RelayMessageResponse {
    pub fn validate(&self) -> Result<(), ProtocolError> {
        check_version(self.version)?;
        check_id(&self.id)?;
        if self.telegram_message_id.is_some_and(|id| id <= 0) {
            return Err(ProtocolError::InvalidStatus);
        }
        Ok(())
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RelayErrorResponse {
    pub version: u8,
    pub code: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_after: Option<u32>,
}
impl RelayErrorResponse {
    pub fn validate(&self) -> Result<(), ProtocolError> {
        check_version(self.version)?;
        check_error_code(&self.code)?;
        if self
            .retry_after
            .is_some_and(|retry| retry > MAX_RETRY_AFTER_SECONDS)
        {
            return Err(ProtocolError::InvalidRetryAfter);
        }
        Ok(())
    }
}
fn check_version(version: u8) -> Result<(), ProtocolError> {
    if version != VERSION {
        return Err(ProtocolError::UnsupportedVersion);
    }
    Ok(())
}
fn check_id(id: &str) -> Result<(), ProtocolError> {
    if id.is_empty() || id.len() > MAX_ID_BYTES || !id.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(ProtocolError::InvalidId);
    }
    Ok(())
}
fn check_error_code(code: &str) -> Result<(), ProtocolError> {
    if code.is_empty()
        || code.len() > MAX_ERROR_CODE_BYTES
        || !code
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
    {
        return Err(ProtocolError::InvalidErrorCode);
    }
    Ok(())
}
fn check_body(body: &[u8]) -> Result<(), ProtocolError> {
    if body.len() > MAX_BODY_BYTES {
        return Err(ProtocolError::BodyTooLarge);
    }
    Ok(())
}

/// Header values are ASCII strings. Adapters must require exactly one of each.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestAuth {
    pub version: String,
    pub timestamp: String,
    pub nonce: String,
    pub signature: String,
}
impl RequestAuth {
    pub fn headers(&self) -> [(&'static str, &str); 4] {
        [
            (HEADER_VERSION, &self.version),
            (HEADER_TIMESTAMP, &self.timestamp),
            (HEADER_NONCE, &self.nonce),
            (HEADER_SIGNATURE, &self.signature),
        ]
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResponseAuth {
    pub version: String,
    pub nonce: String,
    pub signature: String,
}
impl ResponseAuth {
    pub fn headers(&self) -> [(&'static str, &str); 3] {
        [
            (HEADER_VERSION, &self.version),
            (HEADER_NONCE, &self.nonce),
            (HEADER_SIGNATURE, &self.signature),
        ]
    }
}

pub fn body_sha256(body: &[u8]) -> String {
    lower_hex(&Sha256::digest(body))
}

/// Exact UTF-8/ASCII canonical bytes, with LF separators and no trailing LF.
pub fn canonical_request(
    version: &str,
    method: &str,
    path: &str,
    timestamp: &str,
    nonce: &str,
    body: &[u8],
) -> Result<Vec<u8>, ProtocolError> {
    validate_auth_version(version)?;
    validate_route(method, path, body)?;
    parse_timestamp(timestamp)?;
    validate_nonce(nonce)?;
    check_body(body)?;
    Ok(format!(
        "{REQUEST_DOMAIN}\n{version}\n{method}\n{path}\n{timestamp}\n{nonce}\n{}",
        body_sha256(body)
    )
    .into_bytes())
}
pub fn sign_request(
    key: &SecretKey,
    method: &str,
    path: &str,
    timestamp: i64,
    nonce: &str,
    body: &[u8],
) -> Result<RequestAuth, ProtocolError> {
    let timestamp = timestamp.to_string();
    let bytes = canonical_request(VERSION_HEADER_VALUE, method, path, &timestamp, nonce, body)?;
    Ok(RequestAuth {
        version: VERSION_HEADER_VALUE.into(),
        timestamp,
        nonce: nonce.into(),
        signature: sign(key, &bytes),
    })
}
pub fn verify_request(
    key: &SecretKey,
    method: &str,
    path: &str,
    auth: &RequestAuth,
    body: &[u8],
    now: i64,
) -> Result<(), ProtocolError> {
    let bytes = canonical_request(
        &auth.version,
        method,
        path,
        &auth.timestamp,
        &auth.nonce,
        body,
    )?;
    let timestamp = parse_timestamp(&auth.timestamp)?;
    if now < 0 || timestamp.abs_diff(now) > MAX_CLOCK_SKEW_SECONDS {
        return Err(ProtocolError::ClockSkew);
    }
    verify(key, &bytes, &auth.signature)
}
pub fn canonical_response(
    version: &str,
    request_nonce: &str,
    status: u16,
    body: &[u8],
) -> Result<Vec<u8>, ProtocolError> {
    validate_auth_version(version)?;
    validate_nonce(request_nonce)?;
    if !(100..=599).contains(&status) {
        return Err(ProtocolError::InvalidStatus);
    }
    check_body(body)?;
    Ok(format!(
        "{RESPONSE_DOMAIN}\n{version}\n{request_nonce}\n{status}\n{}",
        body_sha256(body)
    )
    .into_bytes())
}
pub fn sign_response(
    key: &SecretKey,
    request_nonce: &str,
    status: u16,
    body: &[u8],
) -> Result<ResponseAuth, ProtocolError> {
    let bytes = canonical_response(VERSION_HEADER_VALUE, request_nonce, status, body)?;
    Ok(ResponseAuth {
        version: VERSION_HEADER_VALUE.into(),
        nonce: request_nonce.into(),
        signature: sign(key, &bytes),
    })
}
pub fn verify_response(
    key: &SecretKey,
    request_nonce: &str,
    status: u16,
    auth: &ResponseAuth,
    body: &[u8],
) -> Result<(), ProtocolError> {
    validate_nonce(request_nonce)?;
    if auth.nonce != request_nonce {
        return Err(ProtocolError::InvalidNonce);
    }
    let bytes = canonical_response(&auth.version, request_nonce, status, body)?;
    verify(key, &bytes, &auth.signature)
}
fn validate_auth_version(version: &str) -> Result<(), ProtocolError> {
    if version != VERSION_HEADER_VALUE {
        return Err(ProtocolError::UnsupportedVersion);
    }
    Ok(())
}
fn validate_route(method: &str, path: &str, body: &[u8]) -> Result<(), ProtocolError> {
    if (method == "POST" && path == MESSAGES_PATH)
        || (method == "GET" && path == STATUS_PATH && body.is_empty())
    {
        Ok(())
    } else {
        Err(ProtocolError::InvalidRoute)
    }
}
fn parse_timestamp(timestamp: &str) -> Result<i64, ProtocolError> {
    if timestamp.is_empty()
        || timestamp.len() > 19
        || !timestamp.bytes().all(|b| b.is_ascii_digit())
        || (timestamp.len() > 1 && timestamp.starts_with('0'))
    {
        return Err(ProtocolError::InvalidTimestamp);
    }
    timestamp
        .parse()
        .map_err(|_| ProtocolError::InvalidTimestamp)
}
fn validate_nonce(nonce: &str) -> Result<(), ProtocolError> {
    if nonce.len() != 32 || !nonce.bytes().all(is_lower_hex) {
        return Err(ProtocolError::InvalidNonce);
    }
    Ok(())
}
fn sign(key: &SecretKey, bytes: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(&key.0).expect("HMAC accepts a 32-byte key");
    mac.update(bytes);
    lower_hex(&mac.finalize().into_bytes())
}
fn verify(key: &SecretKey, bytes: &[u8], signature: &str) -> Result<(), ProtocolError> {
    if signature.len() != 64 || !signature.bytes().all(is_lower_hex) {
        return Err(ProtocolError::InvalidSignature);
    }
    let mut decoded = [0_u8; 32];
    for (out, pair) in decoded.iter_mut().zip(signature.as_bytes().chunks(2)) {
        *out = (hex_digit(pair[0]).ok_or(ProtocolError::InvalidSignature)? << 4)
            | hex_digit(pair[1]).ok_or(ProtocolError::InvalidSignature)?;
    }
    let mut mac = Hmac::<Sha256>::new_from_slice(&key.0).expect("HMAC accepts a 32-byte key");
    mac.update(bytes);
    mac.verify_slice(&decoded)
        .map_err(|_| ProtocolError::InvalidSignature)
}
fn is_lower_hex(byte: u8) -> bool {
    byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)
}
fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}
fn lower_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut result = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        result.push(DIGITS[(byte >> 4) as usize] as char);
        result.push(DIGITS[(byte & 15) as usize] as char);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    const NONCE: &str = "000102030405060708090a0b0c0d0e0f";
    fn message() -> RelayMessage {
        RelayMessage {
            version: VERSION,
            id: "setup:42".into(),
            text: "Сетап 🧠".into(),
            expires_at: 1060,
            attachment: None,
        }
    }
    #[test]
    fn canonical_bytes_and_headers_match_independent_known_vectors() {
        // Cross-checked against Python hashlib/hmac, not generated by this implementation.
        let key = SecretKey::from_bytes(std::array::from_fn(|i| i as u8));
        let request = b"setups-telegram-relay/request\n1\nGET\n/v1/status\n1760000000\n000102030405060708090a0b0c0d0e0f\ne3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        assert_eq!(
            canonical_request("1", "GET", STATUS_PATH, "1760000000", NONCE, b"").unwrap(),
            request
        );
        let auth = sign_request(&key, "GET", STATUS_PATH, 1760000000, NONCE, b"").unwrap();
        assert_eq!(
            auth.headers(),
            [
                ("x-relay-version", "1"),
                ("x-relay-timestamp", "1760000000"),
                ("x-relay-nonce", NONCE),
                (
                    "x-relay-signature",
                    "55328551207c19efe83109a0327576f631d9f7fe742eaad9b5b1e9f12f3b6aa3"
                ),
            ]
        );
        let body = br#"{"version":1}"#;
        let response = b"setups-telegram-relay/response\n1\n000102030405060708090a0b0c0d0e0f\n200\n2430f1a2ad2982d0067885488a4c89e21ad1d7c83b115ba8f1b20acc88dfaea8";
        assert_eq!(canonical_response("1", NONCE, 200, body).unwrap(), response);
        let auth = sign_response(&key, NONCE, 200, body).unwrap();
        assert_eq!(
            auth.headers(),
            [
                ("x-relay-version", "1"),
                ("x-relay-nonce", NONCE),
                (
                    "x-relay-signature",
                    "e46f5b552d99092c9fcb1480b7bd1f8e60d66fbe91d9a03827fb77e7bd4c7ca0"
                ),
            ]
        );
    }
    #[test]
    fn noncanonical_headers_and_routes_are_rejected() {
        for timestamp in [
            "",
            "01",
            "+1",
            "-1",
            " 1",
            "1\n",
            "１",
            "9223372036854775808",
        ] {
            assert_eq!(
                canonical_request("1", "GET", STATUS_PATH, timestamp, NONCE, b""),
                Err(ProtocolError::InvalidTimestamp)
            );
        }
        for nonce in [
            "",
            "000102030405060708090A0B0C0D0E0F",
            "000102030405060708090a0b0c0d0e0g",
            "000102030405060708090a0b0c0d0e0f\n",
        ] {
            assert_eq!(
                canonical_request("1", "GET", STATUS_PATH, "1000", nonce, b""),
                Err(ProtocolError::InvalidNonce)
            );
        }
        for path in [
            "/v1/status/",
            "/v1/status?x=1",
            "/v1/messages",
            "/v2/status",
        ] {
            assert_eq!(
                canonical_request("1", "GET", path, "1000", NONCE, b""),
                Err(ProtocolError::InvalidRoute)
            );
        }
        assert_eq!(
            canonical_request("1", "get", STATUS_PATH, "1000", NONCE, b""),
            Err(ProtocolError::InvalidRoute)
        );
        assert_eq!(
            canonical_request("1", "GET", STATUS_PATH, "1000", NONCE, b"{}"),
            Err(ProtocolError::InvalidRoute)
        );
        let key = SecretKey::from_bytes([42; 32]);
        let auth = sign_request(&key, "GET", STATUS_PATH, 1000, NONCE, b"").unwrap();
        for signature in [
            "",
            "gggg",
            &auth.signature.to_uppercase(),
            &format!("{}\n", auth.signature),
        ] {
            let mut changed = auth.clone();
            changed.signature = signature.into();
            assert_eq!(
                verify_request(&key, "GET", STATUS_PATH, &changed, b"", 1000),
                Err(ProtocolError::InvalidSignature)
            );
        }
    }
    #[test]
    fn private_key_hex_is_exact_and_errors_are_redacted() {
        let key = SecretKey::from_hex(&"aB".repeat(32)).unwrap();
        let expected = SecretKey::from_bytes([0xab; 32]);
        assert_eq!(
            sign_request(&key, "GET", STATUS_PATH, 1000, NONCE, b"").unwrap(),
            sign_request(&expected, "GET", STATUS_PATH, 1000, NONCE, b"").unwrap()
        );
        for invalid in [
            String::new(),
            "a".repeat(63),
            "a".repeat(65),
            "a".repeat(64) + "\n",
            "gg".repeat(32),
            "é".repeat(32),
        ] {
            assert_eq!(
                SecretKey::from_hex(&invalid).err(),
                Some(ProtocolError::InvalidKey)
            );
        }
        assert_eq!(ProtocolError::InvalidKey.to_string(), "invalid_key");
        assert_eq!(
            format!("{:?}", ProtocolError::KeyFileUnavailable),
            "KeyFileUnavailable"
        );
    }
    #[cfg(unix)]
    #[test]
    fn private_key_file_rejects_public_permissions_symlinks_and_invalid_contents() {
        use std::{
            fs::{self, OpenOptions},
            io::Write,
            os::unix::fs::{OpenOptionsExt, PermissionsExt, symlink},
        };
        struct Fixture(std::path::PathBuf);
        impl Drop for Fixture {
            fn drop(&mut self) {
                fs::remove_dir_all(&self.0).expect("remove private-key fixture");
            }
        }
        let directory = std::env::temp_dir().join(format!(
            "setups-relay-key-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&directory).unwrap();
        let fixture = Fixture(directory);
        fs::set_permissions(&fixture.0, fs::Permissions::from_mode(0o700)).unwrap();
        let path = fixture.0.join("key-with-private-path-fragment");
        let encoded = "aB".repeat(32) + "\n";
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&path)
            .unwrap();
        file.write_all(encoded.as_bytes()).unwrap();
        drop(file);
        let expected = SecretKey::from_bytes([0xab; 32]);
        for mode in [0o600, 0o400] {
            fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
            let loaded = SecretKey::from_private_file(&path).unwrap();
            assert_eq!(
                sign_request(&loaded, "GET", STATUS_PATH, 1000, NONCE, b"").unwrap(),
                sign_request(&expected, "GET", STATUS_PATH, 1000, NONCE, b"").unwrap()
            );
        }
        for mode in [0o640, 0o604, 0o644, 0o660, 0o666] {
            fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
            assert_eq!(
                SecretKey::from_private_file(&path).err(),
                Some(ProtocolError::InsecureKeyFile)
            );
        }
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let link = fixture.0.join("symlink");
        symlink(&path, &link).unwrap();
        assert_eq!(
            SecretKey::from_private_file(&link).err(),
            Some(ProtocolError::InsecureKeyFile)
        );
        assert_eq!(
            SecretKey::from_private_file(&fixture.0).err(),
            Some(ProtocolError::InsecureKeyFile)
        );
        for invalid_length in [
            String::new(),
            "a".repeat(64),
            "a".repeat(64) + "\r\n",
            "a".repeat(4096),
        ] {
            fs::write(&path, invalid_length).unwrap();
            assert_eq!(
                SecretKey::from_private_file(&path).err(),
                Some(ProtocolError::InsecureKeyFile)
            );
        }
        for invalid_contents in [
            "g".repeat(64) + "\n",
            "a".repeat(64) + "x",
            "é".repeat(32) + "\n",
        ] {
            fs::write(&path, invalid_contents).unwrap();
            assert_eq!(
                SecretKey::from_private_file(&path).err(),
                Some(ProtocolError::InvalidKey)
            );
        }
        fs::remove_file(&path).unwrap();
        let error = SecretKey::from_private_file(&path).err().unwrap();
        assert_eq!(error, ProtocolError::KeyFileUnavailable);
        assert_eq!(error.to_string(), "key_file_unavailable");
        assert!(!format!("{error:?}").contains("private-path-fragment"));
        assert!(!error.to_string().contains(encoded.trim()));
    }
    #[test]
    fn message_id_text_version_and_time_edges_are_rejected() {
        let mut value = message();
        value.id = "a".repeat(MAX_ID_BYTES);
        assert!(value.encode(1000).is_ok());
        for invalid in [
            String::new(),
            "a".repeat(MAX_ID_BYTES + 1),
            "has space".into(),
            "new\nline".into(),
            "tab\t".into(),
            "nul\0".into(),
            "delete\u{7f}".into(),
            "юникод".into(),
        ] {
            value.id = invalid;
            assert_eq!(value.validate(1000), Err(ProtocolError::InvalidId));
        }
        value = message();
        value.text.clear();
        assert_eq!(value.validate(1000), Err(ProtocolError::InvalidText));
        for version in [0, 2, u8::MAX] {
            value = message();
            value.version = version;
            assert_eq!(value.validate(1000), Err(ProtocolError::UnsupportedVersion));
        }
        value = message();
        value.expires_at = -1;
        assert_eq!(value.validate(1000), Err(ProtocolError::InvalidExpiry));
        value.expires_at = i64::MAX;
        assert_eq!(value.validate(1000), Err(ProtocolError::InvalidExpiry));
        assert_eq!(message().validate(-1), Err(ProtocolError::InvalidExpiry));
        value.expires_at = i64::MAX;
        assert_eq!(value.validate(i64::MAX), Ok(()));
    }
    #[test]
    fn malformed_json_duplicate_fields_and_nonnull_attachments_are_rejected() {
        for body in [
            br#"{"version":1,"id":"x","id":"y","text":"x","expiresAt":1060,"attachment":null}"#
                .as_slice(),
            br#"{"version":1,"id":"x","text":"x","expiresAt":1060.0,"attachment":null}"#,
            br#"{"version":1,"id":"x","text":"x","expiresAt":"1060","attachment":null}"#,
            br#"{"version":1,"id":42,"text":"x","expiresAt":1060,"attachment":null}"#,
            br#"{"version":256,"id":"x","text":"x","expiresAt":1060,"attachment":null}"#,
            b"\xff",
        ] {
            assert_eq!(
                RelayMessage::parse(body, 1000),
                Err(ProtocolError::InvalidJson)
            );
        }
        for attachment in [
            serde_json::json!(false),
            serde_json::json!(0),
            serde_json::json!("https://example.invalid/image"),
            serde_json::json!([]),
            serde_json::json!({}),
        ] {
            let mut value = message();
            value.attachment = Some(attachment);
            let raw = serde_json::to_vec(&value).unwrap();
            assert_eq!(
                RelayMessage::parse(&raw, 1000),
                Err(ProtocolError::UnsupportedAttachment)
            );
        }
    }
    #[test]
    fn response_dto_edges_and_serialization_are_bounded() {
        let status = RelayStatus {
            version: VERSION,
            capabilities: vec![CAPABILITY_TEXT.into()],
            telegram_ready: true,
            checked_at: 1000,
            error: None,
        };
        assert_eq!(status.validate(), Ok(()));
        assert_eq!(
            serde_json::to_string(&status).unwrap(),
            r#"{"version":1,"capabilities":["text"],"telegramReady":true,"checkedAt":1000,"error":null}"#
        );
        for capabilities in [
            vec![],
            vec!["image".into()],
            vec!["text".into(), "text".into()],
        ] {
            let mut bad = status.clone();
            bad.capabilities = capabilities;
            assert_eq!(bad.validate(), Err(ProtocolError::InvalidStatus));
        }
        let mut bad = status.clone();
        bad.checked_at = -1;
        assert_eq!(bad.validate(), Err(ProtocolError::InvalidStatus));
        bad = status;
        bad.error = Some("bot_permission_denied".into());
        assert_eq!(bad.validate(), Ok(()));
        bad.error = Some("upstream token https://example.invalid".into());
        assert_eq!(bad.validate(), Err(ProtocolError::InvalidErrorCode));
        let response = RelayMessageResponse {
            version: VERSION,
            id: "setup:42".into(),
            outcome: RelayOutcome::Duplicate,
            telegram_message_id: None,
        };
        assert_eq!(response.validate(), Ok(()));
        assert_eq!(
            serde_json::to_string(&response).unwrap(),
            r#"{"version":1,"id":"setup:42","outcome":"duplicate"}"#
        );
        for id in [0, -1] {
            let mut bad = response.clone();
            bad.telegram_message_id = Some(id);
            assert_eq!(bad.validate(), Err(ProtocolError::InvalidStatus));
        }
        let mut success = response;
        success.telegram_message_id = Some(i64::MAX);
        assert_eq!(success.validate(), Ok(()));
        let mut error = RelayErrorResponse {
            version: VERSION,
            code: "rate_limited".into(),
            retry_after: Some(MAX_RETRY_AFTER_SECONDS),
        };
        assert_eq!(error.validate(), Ok(()));
        error.retry_after = Some(MAX_RETRY_AFTER_SECONDS + 1);
        assert_eq!(error.validate(), Err(ProtocolError::InvalidRetryAfter));
        error.retry_after = None;
        for code in [
            String::new(),
            "a".repeat(MAX_ERROR_CODE_BYTES + 1),
            "TOKEN".into(),
            "bad\ncode".into(),
            "ошибка".into(),
        ] {
            error.code = code;
            assert_eq!(error.validate(), Err(ProtocolError::InvalidErrorCode));
        }
        assert!(serde_json::from_str::<RelayStatus>(r#"{"version":1,"capabilities":["text"],"telegramReady":true,"checkedAt":1000,"extra":null}"#).is_err());
        assert!(
            serde_json::from_str::<RelayMessageResponse>(
                r#"{"version":1,"id":"x","outcome":"sent","extra":null}"#
            )
            .is_err()
        );
        assert!(
            serde_json::from_str::<RelayErrorResponse>(
                r#"{"version":1,"code":"error","extra":null}"#
            )
            .is_err()
        );
    }
    #[test]
    fn signatures_bound_maximum_raw_body_and_reject_response_role_confusion() {
        let key = SecretKey::from_bytes([42; 32]);
        let max_body = vec![b'a'; MAX_BODY_BYTES];
        let auth = sign_request(&key, "POST", MESSAGES_PATH, 1000, NONCE, &max_body).unwrap();
        assert_eq!(
            verify_request(&key, "POST", MESSAGES_PATH, &auth, &max_body, 1000),
            Ok(())
        );
        let oversized = vec![b'a'; MAX_BODY_BYTES + 1];
        assert_eq!(
            sign_request(&key, "POST", MESSAGES_PATH, 1000, NONCE, &oversized),
            Err(ProtocolError::BodyTooLarge)
        );
        assert_eq!(
            sign_response(&key, NONCE, 200, &oversized),
            Err(ProtocolError::BodyTooLarge)
        );
        for status in [0, 99, 600, u16::MAX] {
            assert_eq!(
                sign_response(&key, NONCE, status, b""),
                Err(ProtocolError::InvalidStatus)
            );
        }
        let request = sign_request(&key, "GET", STATUS_PATH, 1000, NONCE, b"").unwrap();
        let forged = ResponseAuth {
            version: request.version,
            nonce: request.nonce,
            signature: request.signature,
        };
        assert_eq!(
            verify_response(&key, NONCE, 200, &forged, b""),
            Err(ProtocolError::InvalidSignature)
        );
    }
    #[test]
    fn message_roundtrip_and_required_null_attachment() {
        let value = message();
        let body = value.encode(1000).unwrap();
        assert_eq!(RelayMessage::parse(&body, 1000).unwrap(), value);
        assert!(
            std::str::from_utf8(&body)
                .unwrap()
                .contains("\"expiresAt\":1060,\"attachment\":null")
        );
        for bad in [
            r#"{"version":1,"id":"a","text":"x","expiresAt":1060}"#,
            r#"{"version":1,"id":"a","text":"x","expiresAt":1060,"attachment":null,"extra":1}"#,
        ] {
            assert_eq!(
                RelayMessage::parse(bad.as_bytes(), 1000),
                Err(ProtocolError::InvalidJson)
            );
        }
        let mut attachment = value;
        attachment.attachment = Some(serde_json::json!({"url":"https://example.invalid/image"}));
        assert_eq!(
            attachment.validate(1000),
            Err(ProtocolError::UnsupportedAttachment)
        );
    }
    #[test]
    fn expiry_boundaries_preserve_inclusive_ttl() {
        let value = message();
        assert_eq!(value.validate_expiry(1060), Ok(()));
        assert_eq!(value.validate_expiry(1061), Err(ProtocolError::Expired));
        let mut value = value;
        value.expires_at = 1000 + MAX_EXPIRY_SECONDS;
        assert_eq!(value.validate_expiry(1000), Ok(()));
        value.expires_at += 1;
        assert_eq!(
            value.validate_expiry(1000),
            Err(ProtocolError::InvalidExpiry)
        );
    }
    #[test]
    fn unicode_and_raw_body_budgets_are_independent() {
        let mut value = message();
        value.text = "я".repeat(MAX_TEXT_CHARS);
        assert!(value.encode(1000).is_ok());
        value.text.push('я');
        assert_eq!(value.encode(1000), Err(ProtocolError::InvalidText));
        value.text = "🧠".repeat(MAX_TEXT_CHARS);
        value.id = "\\".repeat(MAX_ID_BYTES);
        assert_eq!(value.validate(1000), Ok(()));
        let body = value.encode(1000).unwrap();
        assert!(
            body.len() > 16 * 1024,
            "four-byte text and envelope exceed the old cap"
        );
        assert!(body.len() <= MAX_BODY_BYTES);
        assert_eq!(RelayMessage::parse(&body, 1000).unwrap(), value);
        let mut padded = body;
        padded.resize(MAX_BODY_BYTES, b' ');
        assert_eq!(RelayMessage::parse(&padded, 1000).unwrap(), value);
        padded.push(b' ');
        assert_eq!(
            RelayMessage::parse(&padded, 1000),
            Err(ProtocolError::BodyTooLarge)
        );
        value.text.push('🧠');
        assert_eq!(value.encode(1000), Err(ProtocolError::InvalidText));
        assert_eq!(
            RelayMessage::parse(&vec![b' '; MAX_BODY_BYTES + 1], 1000),
            Err(ProtocolError::BodyTooLarge)
        );
    }
    #[test]
    fn request_tampering_and_clock_boundaries() {
        let key = SecretKey::from_bytes([42; 32]);
        let body = message().encode(1000).unwrap();
        let auth = sign_request(&key, "POST", MESSAGES_PATH, 1000, NONCE, &body).unwrap();
        for now in [940, 1000, 1060] {
            assert_eq!(
                verify_request(&key, "POST", MESSAGES_PATH, &auth, &body, now),
                Ok(())
            );
        }
        for now in [939, 1061] {
            assert_eq!(
                verify_request(&key, "POST", MESSAGES_PATH, &auth, &body, now),
                Err(ProtocolError::ClockSkew)
            );
        }
        assert_eq!(
            verify_request(
                &SecretKey::from_bytes([43; 32]),
                "POST",
                MESSAGES_PATH,
                &auth,
                &body,
                1000
            ),
            Err(ProtocolError::InvalidSignature)
        );
        let mut changed = body.clone();
        changed.push(b' ');
        assert_eq!(
            verify_request(&key, "POST", MESSAGES_PATH, &auth, &changed, 1000),
            Err(ProtocolError::InvalidSignature)
        );
        assert_eq!(
            verify_request(&key, "GET", MESSAGES_PATH, &auth, &body, 1000),
            Err(ProtocolError::InvalidRoute)
        );
        let mut changed_auth = auth.clone();
        changed_auth.timestamp = "1001".into();
        assert_eq!(
            verify_request(&key, "POST", MESSAGES_PATH, &changed_auth, &body, 1000),
            Err(ProtocolError::InvalidSignature)
        );
        changed_auth = auth.clone();
        changed_auth.version = "2".into();
        assert_eq!(
            verify_request(&key, "POST", MESSAGES_PATH, &changed_auth, &body, 1000),
            Err(ProtocolError::UnsupportedVersion)
        );
        changed_auth = auth;
        changed_auth.nonce = "111102030405060708090a0b0c0d0e0f".into();
        assert_eq!(
            verify_request(&key, "POST", MESSAGES_PATH, &changed_auth, &body, 1000),
            Err(ProtocolError::InvalidSignature)
        );
    }
    #[test]
    fn response_proves_key_nonce_status_and_raw_body() {
        let key = SecretKey::from_bytes([42; 32]);
        let body = br#"{"version":1,"capabilities":["text"],"telegramReady":true,"checkedAt":1000,"error":null}"#;
        let auth = sign_response(&key, NONCE, 200, body).unwrap();
        assert_eq!(verify_response(&key, NONCE, 200, &auth, body), Ok(()));
        assert_eq!(
            verify_response(&key, NONCE, 503, &auth, body),
            Err(ProtocolError::InvalidSignature)
        );
        assert_eq!(
            verify_response(&key, NONCE, 200, &auth, b"{}"),
            Err(ProtocolError::InvalidSignature)
        );
        assert_eq!(
            verify_response(&SecretKey::from_bytes([43; 32]), NONCE, 200, &auth, body),
            Err(ProtocolError::InvalidSignature)
        );
        assert_eq!(
            verify_response(&key, "111102030405060708090a0b0c0d0e0f", 200, &auth, body),
            Err(ProtocolError::InvalidNonce)
        );
        let mut auth = auth;
        auth.version = "2".into();
        assert_eq!(
            verify_response(&key, NONCE, 200, &auth, body),
            Err(ProtocolError::UnsupportedVersion)
        );
    }
}
