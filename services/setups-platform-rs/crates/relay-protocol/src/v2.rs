//! Rich-text and inline-PNG relay v2, independent of the unchanged v1 contract.
//!
//! HTTP owners must reject duplicate authentication headers and bound streamed
//! requests before buffering. Authenticate the raw body before parsing PNGs.
//! This module owns no nonce ledger, cache, network client or persistent state.
//! All decoded media/scratch buffers are bounded and dropped after validation.
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::{Deserialize, Deserializer, Serialize};
use std::{fmt, io::Cursor};

pub use crate::{
    CAPABILITY_TEXT, HEADER_NONCE, HEADER_SIGNATURE, HEADER_TIMESTAMP, HEADER_VERSION,
    MAX_CLOCK_SKEW_SECONDS, MAX_ERROR_CODE_BYTES, MAX_EXPIRY_SECONDS, MAX_ID_BYTES,
    MAX_RETRY_AFTER_SECONDS, RelayOutcome, RequestAuth, ResponseAuth, SecretKey, body_sha256,
};

pub const VERSION: u8 = 2;
pub const VERSION_HEADER_VALUE: &str = "2";
pub const MESSAGES_PATH: &str = "/v2/messages";
pub const STATUS_PATH: &str = "/v2/status";
/// Request limit only. Responses retain the v1 32-KiB limit.
pub const MAX_BODY_BYTES: usize = 384 * 1024;
pub const MAX_REQUEST_BODY_BYTES: usize = MAX_BODY_BYTES;
pub const MAX_RESPONSE_BODY_BYTES: usize = crate::MAX_BODY_BYTES;
pub const MAX_PNG_BYTES: usize = 256 * 1024;
pub const MAX_PNG_BASE64_BYTES: usize = MAX_PNG_BYTES.div_ceil(3) * 4;
pub const PNG_WIDTH: u32 = 900;
pub const PNG_HEIGHT: u32 = 320;
pub const MAX_PNG_DECODE_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_ENTITIES: usize = 32;
pub const MAX_CAPTION_UTF16_UNITS: usize = 1024;
pub const MAX_TEXT_UTF16_UNITS: usize = 4096;
pub const CAPABILITY_TEXT_ENTITIES: &str = "text_entities";
pub const CAPABILITY_PHOTO_PNG: &str = "photo_png";
pub const CAPABILITIES: [&str; 3] = [
    CAPABILITY_TEXT,
    CAPABILITY_TEXT_ENTITIES,
    CAPABILITY_PHOTO_PNG,
];

/// Common authentication/identity failures preserve their existing safe codes.
/// New media/formatting failures are separate so a sender can choose an explicit
/// text-only fallback BEFORE freezing a job, never after a publication attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Protocol(crate::ProtocolError),
    CaptionTooLong,
    InvalidEntities,
    InvalidBase64,
    PngTooLarge,
    InvalidPng,
}
impl Error {
    pub const fn code(self) -> &'static str {
        match self {
            Self::Protocol(error) => error.code(),
            Self::CaptionTooLong => "caption_too_long",
            Self::InvalidEntities => "invalid_entities",
            Self::InvalidBase64 => "invalid_base64",
            Self::PngTooLarge => "png_too_large",
            Self::InvalidPng => "invalid_png",
        }
    }
}
impl From<crate::ProtocolError> for Error {
    fn from(error: crate::ProtocolError) -> Self {
        Self::Protocol(error)
    }
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code())
    }
}
impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Protocol(error) => Some(error),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntityKind {
    Bold,
    Code,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TextEntity {
    #[serde(rename = "type")]
    pub kind: EntityKind,
    pub offset: u32,
    pub length: u32,
}

/// Both values use Telegram's UTF-16 entity coordinate system, NOT UTF-8 bytes
/// or Rust char indices. Limits on displayed v2 text are deliberately conservative.
pub fn utf16_len(text: &str) -> usize {
    text.encode_utf16().count()
}

/// Ranges must be sorted, nonempty and nonoverlapping; adjacent ranges are valid.
/// Neither end may split a surrogate pair. No links, HTML or Markdown are parsed.
pub fn validate_entities(text: &str, entities: &[TextEntity]) -> Result<(), Error> {
    if entities.len() > MAX_ENTITIES {
        return Err(Error::InvalidEntities);
    }
    let units = utf16_len(text);
    if units > MAX_TEXT_UTF16_UNITS {
        return Err(crate::ProtocolError::InvalidText.into());
    }
    if entities.is_empty() {
        return Ok(());
    }
    // At most 4097 u32 entries (16 KiB), released at the end of this call.
    let mut boundaries = Vec::with_capacity(units + 1);
    boundaries.push(0_u32);
    let mut position = 0_u32;
    for ch in text.chars() {
        position += ch.len_utf16() as u32;
        boundaries.push(position);
    }
    let mut previous_end = 0;
    for entity in entities {
        let end = entity
            .offset
            .checked_add(entity.length)
            .ok_or(Error::InvalidEntities)?;
        if entity.length == 0
            || entity.offset < previous_end
            || boundaries.binary_search(&entity.offset).is_err()
            || boundaries.binary_search(&end).is_err()
        {
            return Err(Error::InvalidEntities);
        }
        previous_end = end;
    }
    Ok(())
}

/// Only inline PNG bytes are representable: no URL, path, MIME override, Telegram
/// file_id or other opaque attachment. Debug intentionally omits the media bytes.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Attachment {
    Png {
        #[serde(rename = "dataBase64")]
        data_base64: String,
    },
}
impl fmt::Debug for Attachment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Png { data_base64 } => f
                .debug_struct("Png")
                .field("base64_bytes", &data_base64.len())
                .finish(),
        }
    }
}
impl Attachment {
    /// Validate a renderer's output before making its canonical wire encoding.
    pub fn from_png(bytes: &[u8]) -> Result<Self, Error> {
        validate_png(bytes)?;
        Ok(Self::Png {
            data_base64: STANDARD.encode(bytes),
        })
    }

    /// The returned buffer is owned by the caller's one publication attempt.
    /// No cache or persistent decoded copy is retained by the protocol crate.
    pub fn decode_png(&self) -> Result<Vec<u8>, Error> {
        let Self::Png { data_base64 } = self;
        if data_base64.len() > MAX_PNG_BASE64_BYTES {
            return Err(Error::PngTooLarge);
        }
        if !data_base64.len().is_multiple_of(4) {
            return Err(Error::InvalidBase64);
        }
        let padding = if data_base64.ends_with("==") {
            2
        } else if data_base64.ends_with('=') {
            1
        } else {
            0
        };
        let decoded_len = (data_base64.len() / 4 * 3)
            .checked_sub(padding)
            .ok_or(Error::InvalidBase64)?;
        if decoded_len > MAX_PNG_BYTES {
            return Err(Error::PngTooLarge);
        }
        // STANDARD requires exact padding and zero unused trailing bits. Decode
        // directly into a <=256-KiB slice, rather than an unbounded growing Vec.
        let mut bytes = vec![0_u8; decoded_len];
        let length = STANDARD
            .decode_slice(data_base64, &mut bytes)
            .map_err(|_| Error::InvalidBase64)?;
        if length != decoded_len {
            return Err(Error::InvalidBase64);
        }
        validate_png(&bytes)?;
        Ok(bytes)
    }

    pub fn validate(&self) -> Result<(), Error> {
        self.decode_png().map(|_| ())
    }
}

/// Validate the complete static PNG, not just its magic bytes/IHDR. Chunk scanning
/// rejects APNG, trailing bytes and hostile dimensions before invoking a decoder.
/// The decoder has a 2-MiB internal budget; its caller-owned output is at most
/// 900*320*4 = 1,152,000 bytes. No pixel buffer survives this call.
pub fn validate_png(bytes: &[u8]) -> Result<(), Error> {
    check_png_structure(bytes)?;
    // png defaults to skipping bad ancillary CRCs and Adler-32 verification;
    // wire validation must fail closed rather than silently repair those inputs.
    let mut options = png::DecodeOptions::default();
    options.set_ignore_checksums(false);
    options.set_skip_ancillary_crc_failures(false);
    options.set_ignore_text_chunk(true);
    options.set_ignore_iccp_chunk(true);
    let mut decoder = png::Decoder::new_with_options(Cursor::new(bytes), options);
    decoder.set_limits(png::Limits {
        bytes: MAX_PNG_DECODE_BYTES,
    });
    let mut reader = decoder.read_info().map_err(|_| Error::InvalidPng)?;
    let info = reader.info();
    if info.width != PNG_WIDTH
        || info.height != PNG_HEIGHT
        || info.bit_depth != png::BitDepth::Eight
        || !matches!(info.color_type, png::ColorType::Rgb | png::ColorType::Rgba)
        || info.animation_control.is_some()
    {
        return Err(Error::InvalidPng);
    }
    let channels = if info.color_type == png::ColorType::Rgb {
        3
    } else {
        4
    };
    let expected = PNG_WIDTH as usize * PNG_HEIGHT as usize * channels;
    if reader.output_buffer_size() != expected {
        return Err(Error::InvalidPng);
    }
    let mut pixels = vec![0_u8; expected];
    let output = reader
        .next_frame(&mut pixels)
        .map_err(|_| Error::InvalidPng)?;
    if output.width != PNG_WIDTH
        || output.height != PNG_HEIGHT
        || output.bit_depth != png::BitDepth::Eight
        || output.buffer_size() != expected
    {
        return Err(Error::InvalidPng);
    }
    reader.finish().map_err(|_| Error::InvalidPng)?;
    Ok(())
}

fn check_png_structure(bytes: &[u8]) -> Result<(), Error> {
    if bytes.len() > MAX_PNG_BYTES {
        return Err(Error::PngTooLarge);
    }
    if !bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Err(Error::InvalidPng);
    }
    let mut offset = 8;
    let mut seen_header = false;
    let mut seen_palette = false;
    let mut seen_data = false;
    let mut data_ended = false;
    let mut data_bytes = 0_usize;
    while offset < bytes.len() {
        let header = bytes.get(offset..offset + 8).ok_or(Error::InvalidPng)?;
        let length =
            u32::from_be_bytes(header[..4].try_into().expect("four length bytes")) as usize;
        let end = offset
            .checked_add(12)
            .and_then(|start| start.checked_add(length))
            .filter(|end| *end <= bytes.len())
            .ok_or(Error::InvalidPng)?;
        let kind = &header[4..8];
        if !kind.iter().all(u8::is_ascii_alphabetic) || !kind[2].is_ascii_uppercase() {
            return Err(Error::InvalidPng);
        }
        let data = &bytes[offset + 8..end - 4];
        if !seen_header && kind != b"IHDR" {
            return Err(Error::InvalidPng);
        }
        match kind {
            b"IHDR" => {
                if seen_header
                    || length != 13
                    || data[..4] != PNG_WIDTH.to_be_bytes()
                    || data[4..8] != PNG_HEIGHT.to_be_bytes()
                    || data[8] != 8
                    || !matches!(data[9], 2 | 6)
                    || data[10] != 0
                    || data[11] != 0
                    || data[12] > 1
                {
                    return Err(Error::InvalidPng);
                }
                seen_header = true;
            }
            b"PLTE" => {
                if seen_palette
                    || seen_data
                    || length == 0
                    || length > 768
                    || !length.is_multiple_of(3)
                {
                    return Err(Error::InvalidPng);
                }
                seen_palette = true;
            }
            b"IDAT" => {
                if data_ended {
                    return Err(Error::InvalidPng);
                }
                seen_data = true;
                data_bytes += length;
            }
            b"IEND" => {
                if length != 0 || !seen_data || data_bytes == 0 || end != bytes.len() {
                    return Err(Error::InvalidPng);
                }
                return Ok(());
            }
            b"acTL" | b"fcTL" | b"fdAT" => return Err(Error::InvalidPng),
            _ => {
                if kind[0].is_ascii_uppercase() {
                    return Err(Error::InvalidPng);
                }
            }
        }
        if seen_data && kind != b"IDAT" {
            data_ended = true;
        }
        offset = end;
    }
    Err(Error::InvalidPng)
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RelayMessage {
    pub version: u8,
    pub id: String,
    pub text: String,
    pub entities: Vec<TextEntity>,
    pub expires_at: i64,
    // An ordinary Option would silently accept a missing field; null is explicit.
    #[serde(deserialize_with = "required_attachment")]
    pub attachment: Option<Attachment>,
}
impl fmt::Debug for RelayMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RelayMessage")
            .field("version", &self.version)
            .field("id", &self.id)
            .field("text_utf16_units", &utf16_len(&self.text))
            .field("entities", &self.entities)
            .field("expires_at", &self.expires_at)
            .field("attachment", &self.attachment)
            .finish()
    }
}
fn required_attachment<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Attachment>, D::Error> {
    Option::<Attachment>::deserialize(deserializer)
}
impl RelayMessage {
    pub fn parse(body: &[u8], now: i64) -> Result<Self, Error> {
        check_body(body, MAX_REQUEST_BODY_BYTES)?;
        let message: Self =
            serde_json::from_slice(body).map_err(|_| crate::ProtocolError::InvalidJson)?;
        message.validate(now)?;
        Ok(message)
    }
    pub fn encode(&self, now: i64) -> Result<Vec<u8>, Error> {
        self.validate(now)?;
        let body = serde_json::to_vec(self).map_err(|_| crate::ProtocolError::InvalidJson)?;
        check_body(&body, MAX_REQUEST_BODY_BYTES)?;
        Ok(body)
    }
    pub fn validate(&self, now: i64) -> Result<(), Error> {
        check_version(self.version)?;
        crate::check_id(&self.id)?;
        let units = utf16_len(&self.text);
        if self.text.is_empty() || units > MAX_TEXT_UTF16_UNITS {
            return Err(crate::ProtocolError::InvalidText.into());
        }
        if self.attachment.is_some() && units > MAX_CAPTION_UTF16_UNITS {
            return Err(Error::CaptionTooLong);
        }
        validate_entities(&self.text, &self.entities)?;
        self.validate_expiry(now)?;
        if let Some(attachment) = &self.attachment {
            attachment.validate()?;
        }
        Ok(())
    }
    /// Repeat immediately before the one Telegram call; never renew the expiry.
    pub fn validate_expiry(&self, now: i64) -> Result<(), Error> {
        if now < 0 || self.expires_at < 0 {
            return Err(crate::ProtocolError::InvalidExpiry.into());
        }
        if self.expires_at < now {
            return Err(crate::ProtocolError::Expired.into());
        }
        if self.expires_at.saturating_sub(now) > MAX_EXPIRY_SECONDS {
            return Err(crate::ProtocolError::InvalidExpiry.into());
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
    pub fn validate(&self) -> Result<(), Error> {
        check_version(self.version)?;
        if self.capabilities != CAPABILITIES || self.checked_at < 0 {
            return Err(crate::ProtocolError::InvalidStatus.into());
        }
        if let Some(error) = &self.error {
            crate::check_error_code(error)?;
        }
        Ok(())
    }
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
    pub fn validate(&self) -> Result<(), Error> {
        check_version(self.version)?;
        crate::check_id(&self.id)?;
        if self.telegram_message_id.is_some_and(|id| id <= 0) {
            return Err(crate::ProtocolError::InvalidStatus.into());
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
    pub fn validate(&self) -> Result<(), Error> {
        check_version(self.version)?;
        crate::check_error_code(&self.code)?;
        if self
            .retry_after
            .is_some_and(|retry| retry > MAX_RETRY_AFTER_SECONDS)
        {
            return Err(crate::ProtocolError::InvalidRetryAfter.into());
        }
        Ok(())
    }
}

fn check_version(version: u8) -> Result<(), Error> {
    if version != VERSION {
        return Err(crate::ProtocolError::UnsupportedVersion.into());
    }
    Ok(())
}
fn check_body(body: &[u8], limit: usize) -> Result<(), Error> {
    if body.len() > limit {
        return Err(crate::ProtocolError::BodyTooLarge.into());
    }
    Ok(())
}
fn check_auth_version(version: &str) -> Result<(), Error> {
    if version != VERSION_HEADER_VALUE {
        return Err(crate::ProtocolError::UnsupportedVersion.into());
    }
    Ok(())
}

/// Same domain separators and LF layout as v1, with an explicit authenticated 2.
/// It authenticates arbitrary raw bytes; DTO/body-version validation is a separate
/// mandatory step after authentication, just as for v1.
pub fn canonical_request(
    version: &str,
    method: &str,
    path: &str,
    timestamp: &str,
    nonce: &str,
    body: &[u8],
) -> Result<Vec<u8>, Error> {
    check_auth_version(version)?;
    if !((method == "POST" && path == MESSAGES_PATH)
        || (method == "GET" && path == STATUS_PATH && body.is_empty()))
    {
        return Err(crate::ProtocolError::InvalidRoute.into());
    }
    crate::parse_timestamp(timestamp)?;
    crate::validate_nonce(nonce)?;
    check_body(body, MAX_REQUEST_BODY_BYTES)?;
    Ok(format!(
        "{}\n{version}\n{method}\n{path}\n{timestamp}\n{nonce}\n{}",
        crate::REQUEST_DOMAIN,
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
) -> Result<RequestAuth, Error> {
    let timestamp = timestamp.to_string();
    let bytes = canonical_request(VERSION_HEADER_VALUE, method, path, &timestamp, nonce, body)?;
    Ok(RequestAuth {
        version: VERSION_HEADER_VALUE.into(),
        timestamp,
        nonce: nonce.into(),
        signature: crate::sign(key, &bytes),
    })
}
pub fn verify_request(
    key: &SecretKey,
    method: &str,
    path: &str,
    auth: &RequestAuth,
    body: &[u8],
    now: i64,
) -> Result<(), Error> {
    let bytes = canonical_request(
        &auth.version,
        method,
        path,
        &auth.timestamp,
        &auth.nonce,
        body,
    )?;
    let timestamp = crate::parse_timestamp(&auth.timestamp)?;
    if now < 0 || timestamp.abs_diff(now) > MAX_CLOCK_SKEW_SECONDS {
        return Err(crate::ProtocolError::ClockSkew.into());
    }
    crate::verify(key, &bytes, &auth.signature)?;
    Ok(())
}
pub fn canonical_response(
    version: &str,
    request_nonce: &str,
    status: u16,
    body: &[u8],
) -> Result<Vec<u8>, Error> {
    check_auth_version(version)?;
    crate::validate_nonce(request_nonce)?;
    if !(100..=599).contains(&status) {
        return Err(crate::ProtocolError::InvalidStatus.into());
    }
    check_body(body, MAX_RESPONSE_BODY_BYTES)?;
    Ok(format!(
        "{}\n{version}\n{request_nonce}\n{status}\n{}",
        crate::RESPONSE_DOMAIN,
        body_sha256(body)
    )
    .into_bytes())
}
pub fn sign_response(
    key: &SecretKey,
    request_nonce: &str,
    status: u16,
    body: &[u8],
) -> Result<ResponseAuth, Error> {
    let bytes = canonical_response(VERSION_HEADER_VALUE, request_nonce, status, body)?;
    Ok(ResponseAuth {
        version: VERSION_HEADER_VALUE.into(),
        nonce: request_nonce.into(),
        signature: crate::sign(key, &bytes),
    })
}
pub fn verify_response(
    key: &SecretKey,
    request_nonce: &str,
    status: u16,
    auth: &ResponseAuth,
    body: &[u8],
) -> Result<(), Error> {
    crate::validate_nonce(request_nonce)?;
    if auth.nonce != request_nonce {
        return Err(crate::ProtocolError::InvalidNonce.into());
    }
    let bytes = canonical_response(&auth.version, request_nonce, status, body)?;
    crate::verify(key, &bytes, &auth.signature)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ProtocolError;
    const NONCE: &str = "000102030405060708090a0b0c0d0e0f";

    fn message() -> RelayMessage {
        RelayMessage {
            version: VERSION,
            id: "setup:42".into(),
            text: "Сетап 🧠".into(),
            entities: vec![TextEntity {
                kind: EntityKind::Bold,
                offset: 0,
                length: 5,
            }],
            expires_at: 1060,
            attachment: None,
        }
    }
    fn png_fixture(color: png::ColorType) -> Vec<u8> {
        let channels = match color {
            png::ColorType::Rgb => 3,
            png::ColorType::Rgba => 4,
            _ => 1,
        };
        let mut bytes = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut bytes, PNG_WIDTH, PNG_HEIGHT);
            encoder.set_color(color);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().unwrap();
            writer
                .write_image_data(&vec![
                    25;
                    PNG_WIDTH as usize * PNG_HEIGHT as usize * channels
                ])
                .unwrap();
        }
        bytes
    }
    fn photo() -> RelayMessage {
        RelayMessage {
            attachment: Some(Attachment::from_png(&png_fixture(png::ColorType::Rgb)).unwrap()),
            ..message()
        }
    }
    fn entity(offset: u32, length: u32) -> TextEntity {
        TextEntity {
            kind: EntityKind::Code,
            offset,
            length,
        }
    }
    // Fixture-only CRC permits changing structural fields without failing solely
    // on a stale checksum. Production CRC/decompression comes from the PNG codec.
    fn crc(bytes: &[u8]) -> u32 {
        let mut value = !0_u32;
        for byte in bytes {
            value ^= u32::from(*byte);
            for _ in 0..8 {
                value = (value >> 1) ^ (0xedb88320 & 0_u32.wrapping_sub(value & 1));
            }
        }
        !value
    }
    fn chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&(data.len() as u32).to_be_bytes());
        bytes.extend_from_slice(kind);
        bytes.extend_from_slice(data);
        bytes.extend_from_slice(&crc(&bytes[4..]).to_be_bytes());
        bytes
    }
    fn replace_header(bytes: &mut [u8], start: usize, value: &[u8]) {
        bytes[start..start + value.len()].copy_from_slice(value);
        let checksum = crc(&bytes[12..29]);
        bytes[29..33].copy_from_slice(&checksum.to_be_bytes());
    }

    #[test]
    fn v2_canonical_bytes_and_headers_match_independent_known_vectors() {
        // Independently computed with Python hashlib/hmac; not self-generated.
        let key = SecretKey::from_bytes(std::array::from_fn(|i| i as u8));
        let request = b"setups-telegram-relay/request\n2\nGET\n/v2/status\n1760000000\n000102030405060708090a0b0c0d0e0f\ne3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        assert_eq!(
            canonical_request("2", "GET", STATUS_PATH, "1760000000", NONCE, b"").unwrap(),
            request
        );
        let auth = sign_request(&key, "GET", STATUS_PATH, 1760000000, NONCE, b"").unwrap();
        assert_eq!(
            auth.headers(),
            [
                (HEADER_VERSION, "2"),
                (HEADER_TIMESTAMP, "1760000000"),
                (HEADER_NONCE, NONCE),
                (
                    HEADER_SIGNATURE,
                    "48873a6d2bf3806e5393e60b0ac05245e7c748fa9177ebfa14f392477263ded7"
                ),
            ]
        );
        let body = br#"{"version":2}"#;
        let response = b"setups-telegram-relay/response\n2\n000102030405060708090a0b0c0d0e0f\n200\n732322c128ed67841ef5aa42c7cd04ffcd74a61b2d17d710cd502b713d62930d";
        assert_eq!(canonical_response("2", NONCE, 200, body).unwrap(), response);
        let auth = sign_response(&key, NONCE, 200, body).unwrap();
        assert_eq!(
            auth.headers(),
            [
                (HEADER_VERSION, "2"),
                (HEADER_NONCE, NONCE),
                (
                    HEADER_SIGNATURE,
                    "c933f639e8a0c6b09b8c5ba3ce808d8701a1398588acdf178877887722acb7a1"
                ),
            ]
        );
    }

    #[test]
    fn roundtrip_has_required_null_and_strict_typed_attachment() {
        let value = message();
        let body = value.encode(1000).unwrap();
        assert_eq!(RelayMessage::parse(&body, 1000).unwrap(), value);
        assert_eq!(
            String::from_utf8(body).unwrap(),
            "{\"version\":2,\"id\":\"setup:42\",\"text\":\"Сетап 🧠\",\"entities\":[{\"type\":\"bold\",\"offset\":0,\"length\":5}],\"expiresAt\":1060,\"attachment\":null}"
        );
        let photo = photo();
        assert_eq!(
            RelayMessage::parse(&photo.encode(1000).unwrap(), 1000).unwrap(),
            photo
        );
        for attachment in [
            serde_json::json!("https://example.invalid/a.png"),
            serde_json::json!({"url":"https://example.invalid/a.png"}),
            serde_json::json!({"kind":"jpeg","dataBase64":"AAAA"}),
            serde_json::json!({"kind":"png","dataBase64":"AAAA","url":"https://example.invalid"}),
            serde_json::json!({"kind":"png","fileId":"opaque"}),
        ] {
            let mut value = serde_json::to_value(message()).unwrap();
            value["attachment"] = attachment;
            assert_eq!(
                RelayMessage::parse(&serde_json::to_vec(&value).unwrap(), 1000),
                Err(Error::Protocol(ProtocolError::InvalidJson))
            );
        }
        for field in ["attachment", "entities"] {
            let mut value = serde_json::to_value(message()).unwrap();
            value.as_object_mut().unwrap().remove(field);
            assert!(RelayMessage::parse(&serde_json::to_vec(&value).unwrap(), 1000).is_err());
        }
        assert!(
            serde_json::from_str::<Attachment>(
                r#"{"kind":"png","kind":"png","dataBase64":"AAAA"}"#
            )
            .is_err()
        );
    }

    #[test]
    fn entities_use_utf16_boundaries_and_sorted_disjoint_spans() {
        let text = "Я🧠e\u{301}<b>&x";
        assert_eq!(utf16_len(text), 10);
        assert_eq!(
            validate_entities(
                text,
                &[entity(0, 1), entity(1, 2), entity(3, 2), entity(5, 5)]
            ),
            Ok(())
        );
        for entities in [
            vec![entity(2, 1)],
            vec![entity(1, 1)],
            vec![entity(0, 0)],
            vec![entity(0, 3), entity(1, 2)],
            vec![entity(3, 1), entity(0, 1)],
            vec![entity(0, 12)],
            vec![entity(11, 1)],
            vec![entity(u32::MAX, 2)],
            vec![entity(0, 1); MAX_ENTITIES + 1],
        ] {
            assert_eq!(
                validate_entities(text, &entities),
                Err(Error::InvalidEntities)
            );
        }
        let thirty_two = "a".repeat(MAX_ENTITIES);
        let entities: Vec<_> = (0..MAX_ENTITIES as u32).map(|i| entity(i, 1)).collect();
        assert_eq!(validate_entities(&thirty_two, &entities), Ok(()));
        assert!(
            serde_json::from_str::<TextEntity>(
                r#"{"type":"text_link","offset":0,"length":1,"url":"https://example.invalid"}"#
            )
            .is_err()
        );
        assert!(
            serde_json::from_str::<TextEntity>(r#"{"type":"bold","offset":-1,"length":1}"#)
                .is_err()
        );
        assert!(
            serde_json::from_str::<TextEntity>(
                r#"{"type":"code","offset":0,"length":1,"language":"sh"}"#
            )
            .is_err()
        );
    }

    #[test]
    fn caption_and_text_limits_are_utf16_not_scalars_or_utf8() {
        let mut value = photo();
        value.entities.clear();
        value.text = "🧠".repeat(MAX_CAPTION_UTF16_UNITS / 2);
        assert!(value.validate(1000).is_ok());
        value.text.push('a');
        assert_eq!(value.validate(1000), Err(Error::CaptionTooLong));
        // Explicit pre-freeze fallback can preserve the full text, not truncate.
        value.attachment = None;
        assert!(value.validate(1000).is_ok());
        value.text = "🧠".repeat(MAX_TEXT_UTF16_UNITS / 2);
        assert!(value.encode(1000).is_ok());
        value.text.push('a');
        assert_eq!(
            value.validate(1000),
            Err(Error::Protocol(ProtocolError::InvalidText))
        );
        value.text.clear();
        assert_eq!(
            value.validate(1000),
            Err(Error::Protocol(ProtocolError::InvalidText))
        );
        let name = "🧠".repeat(120);
        assert_eq!(name.len(), 480);
        assert_eq!(utf16_len(&name), 240);
        value.text = format!(
            "{name}\nSHORT\nTP = 14%, SL = 4% (от фактической цены входа)\nЦена не ниже 99,00 USDT"
        );
        value.entities = vec![TextEntity {
            kind: EntityKind::Bold,
            offset: 0,
            length: 240,
        }];
        value.attachment = photo().attachment;
        assert!(value.validate(1000).is_ok());
    }

    #[test]
    fn png_rgb_and_rgba_are_fully_decoded_and_corruption_rejected() {
        for color in [png::ColorType::Rgb, png::ColorType::Rgba] {
            let bytes = png_fixture(color);
            assert_eq!(validate_png(&bytes), Ok(()));
            assert_eq!(
                Attachment::from_png(&bytes).unwrap().decode_png().unwrap(),
                bytes
            );
            let mut corrupt = bytes.clone();
            corrupt[45] ^= 1;
            assert_eq!(validate_png(&corrupt), Err(Error::InvalidPng));
            let mut trailing = bytes.clone();
            trailing.push(0);
            assert_eq!(validate_png(&trailing), Err(Error::InvalidPng));
            assert_eq!(
                validate_png(&bytes[..bytes.len() - 1]),
                Err(Error::InvalidPng)
            );
            let mut bad_end = bytes;
            let index = bad_end.len() - 1;
            bad_end[index] ^= 1;
            assert_eq!(validate_png(&bad_end), Err(Error::InvalidPng));
        }
        assert_eq!(
            validate_png(&png_fixture(png::ColorType::Grayscale)),
            Err(Error::InvalidPng)
        );
        assert_eq!(validate_png(b"not a PNG"), Err(Error::InvalidPng));
        assert_eq!(
            validate_png(&vec![0; MAX_PNG_BYTES + 1]),
            Err(Error::PngTooLarge)
        );
    }

    #[test]
    fn png_structure_rejects_hostile_sizes_animation_and_invalid_idat() {
        let bytes = png_fixture(png::ColorType::Rgb);
        for (start, replacement) in [
            (16, (PNG_WIDTH + 1).to_be_bytes().to_vec()),
            (20, 0_u32.to_be_bytes().to_vec()),
            (16, u32::MAX.to_be_bytes().to_vec()),
            (24, vec![16]),
            (25, vec![3]),
            (26, vec![1]),
            (27, vec![1]),
            (28, vec![2]),
        ] {
            let mut invalid = bytes.clone();
            replace_header(&mut invalid, start, &replacement);
            assert_eq!(validate_png(&invalid), Err(Error::InvalidPng));
        }
        for kind in [b"acTL", b"fcTL", b"fdAT", b"ABCD", b"te1t"] {
            let mut invalid = bytes.clone();
            invalid.splice(33..33, chunk(kind, &[0; 8]));
            assert_eq!(validate_png(&invalid), Err(Error::InvalidPng));
        }
        let mut invalid = bytes[..33].to_vec();
        invalid.extend(chunk(b"IDAT", b"not a deflate stream"));
        invalid.extend(chunk(b"IEND", b""));
        assert_eq!(validate_png(&invalid), Err(Error::InvalidPng));
        let mut huge_chunk = bytes.clone();
        huge_chunk[33..37].copy_from_slice(&u32::MAX.to_be_bytes());
        assert_eq!(validate_png(&huge_chunk), Err(Error::InvalidPng));
    }

    #[test]
    fn png_checks_ancillary_crc_and_zlib_checksum_instead_of_repairing_them() {
        let bytes = png_fixture(png::ColorType::Rgb);
        let mut ancillary = bytes.clone();
        let mut metadata = chunk(b"raNd", b"test");
        let last = metadata.len() - 1;
        metadata[last] ^= 1;
        ancillary.splice(33..33, metadata);
        assert_eq!(validate_png(&ancillary), Err(Error::InvalidPng));

        // Corrupt the Adler-32 while repairing the IDAT CRC, so checksum
        // verification (not structural or CRC checks) must reject the image.
        let mut adler = bytes;
        assert_eq!(&adler[37..41], b"IDAT");
        let length = u32::from_be_bytes(adler[33..37].try_into().unwrap()) as usize;
        let end = 41 + length;
        adler[end - 1] ^= 1;
        let checksum = crc(&adler[37..end]);
        adler[end..end + 4].copy_from_slice(&checksum.to_be_bytes());
        assert_eq!(validate_png(&adler), Err(Error::InvalidPng));
    }

    #[test]
    fn canonical_base64_and_png_encoded_decoded_boundaries() {
        let bytes = png_fixture(png::ColorType::Rgb);
        let good = STANDARD.encode(&bytes);
        for invalid in [
            format!("{good}\n"),
            format!(" {good}"),
            good.replace('+', "-") + "=",
            format!("data:image/png;base64,{good}"),
            "AAAA=".into(),
            "!!!!".into(),
            "Zh==".into(),
            "Zg".into(),
            "Zg=".into(),
            "Zg==AAAA".into(),
        ] {
            assert!(
                Attachment::Png {
                    data_base64: invalid
                }
                .decode_png()
                .is_err()
            );
        }
        assert_eq!(
            Attachment::Png {
                data_base64: "a".repeat(MAX_PNG_BASE64_BYTES + 1)
            }
            .decode_png(),
            Err(Error::PngTooLarge)
        );
        assert_eq!(
            Attachment::Png {
                data_base64: STANDARD.encode(vec![0; MAX_PNG_BYTES + 1])
            }
            .decode_png(),
            Err(Error::PngTooLarge)
        );
        // A bounded ancillary chunk makes a genuine image hit exactly 256 KiB.
        let mut maximum = bytes.clone();
        let padding = MAX_PNG_BYTES - maximum.len() - 12;
        maximum.splice(33..33, chunk(b"raNd", &vec![0; padding]));
        assert_eq!(maximum.len(), MAX_PNG_BYTES);
        assert_eq!(validate_png(&maximum), Ok(()));
        let attachment = Attachment::from_png(&maximum).unwrap();
        let Attachment::Png { data_base64 } = &attachment;
        assert_eq!(data_base64.len(), MAX_PNG_BASE64_BYTES);
        assert_eq!(attachment.decode_png().unwrap(), maximum);
        let mut oversized = bytes;
        oversized.splice(33..33, chunk(b"raNd", &vec![0; padding + 1]));
        assert_eq!(validate_png(&oversized), Err(Error::PngTooLarge));
    }

    #[test]
    fn request_body_and_response_budgets_are_separate_and_inclusive() {
        let key = SecretKey::from_bytes([42; 32]);
        let maximum = vec![b'a'; MAX_REQUEST_BODY_BYTES];
        let auth = sign_request(&key, "POST", MESSAGES_PATH, 1000, NONCE, &maximum).unwrap();
        assert_eq!(
            verify_request(&key, "POST", MESSAGES_PATH, &auth, &maximum, 1000),
            Ok(())
        );
        assert_eq!(
            sign_request(
                &key,
                "POST",
                MESSAGES_PATH,
                1000,
                NONCE,
                &vec![0; MAX_REQUEST_BODY_BYTES + 1]
            ),
            Err(Error::Protocol(ProtocolError::BodyTooLarge))
        );
        let response = vec![0; MAX_RESPONSE_BODY_BYTES];
        assert!(sign_response(&key, NONCE, 200, &response).is_ok());
        assert_eq!(
            sign_response(&key, NONCE, 200, &vec![0; MAX_RESPONSE_BODY_BYTES + 1]),
            Err(Error::Protocol(ProtocolError::BodyTooLarge))
        );
        let value = message();
        let mut body = value.encode(1000).unwrap();
        body.resize(MAX_REQUEST_BODY_BYTES, b' ');
        assert_eq!(RelayMessage::parse(&body, 1000).unwrap(), value);
        body.push(b' ');
        assert_eq!(
            RelayMessage::parse(&body, 1000),
            Err(Error::Protocol(ProtocolError::BodyTooLarge))
        );
    }

    #[test]
    fn versions_routes_headers_and_expiry_are_fail_closed() {
        let key = SecretKey::from_bytes([42; 32]);
        for path in [
            crate::STATUS_PATH,
            "/v2/status/",
            "/v2/status?x=1",
            MESSAGES_PATH,
        ] {
            assert_eq!(
                sign_request(&key, "GET", path, 1000, NONCE, b""),
                Err(Error::Protocol(ProtocolError::InvalidRoute))
            );
        }
        assert_eq!(
            sign_request(&key, "GET", STATUS_PATH, 1000, NONCE, b"{}"),
            Err(Error::Protocol(ProtocolError::InvalidRoute))
        );
        assert_eq!(
            canonical_request("1", "GET", STATUS_PATH, "1000", NONCE, b""),
            Err(Error::Protocol(ProtocolError::UnsupportedVersion))
        );
        for timestamp in ["01", "-1", "+1", "1\n", "9223372036854775808"] {
            assert!(canonical_request("2", "GET", STATUS_PATH, timestamp, NONCE, b"").is_err());
        }
        for nonce in ["", "000102030405060708090A0B0C0D0E0F"] {
            assert!(sign_request(&key, "GET", STATUS_PATH, 1000, nonce, b"").is_err());
        }
        let mut value = message();
        for version in [0, 1, 3] {
            value.version = version;
            assert_eq!(
                value.validate(1000),
                Err(Error::Protocol(ProtocolError::UnsupportedVersion))
            );
        }
        value.version = VERSION;
        assert_eq!(value.validate_expiry(1060), Ok(()));
        assert_eq!(
            value.validate_expiry(1061),
            Err(Error::Protocol(ProtocolError::Expired))
        );
        assert_eq!(
            value.validate_expiry(-1),
            Err(Error::Protocol(ProtocolError::InvalidExpiry))
        );
        value.expires_at = 1000 + MAX_EXPIRY_SECONDS;
        assert_eq!(value.validate_expiry(1000), Ok(()));
        value.expires_at += 1;
        assert_eq!(
            value.validate_expiry(1000),
            Err(Error::Protocol(ProtocolError::InvalidExpiry))
        );
    }

    #[test]
    fn raw_body_entity_and_png_tampering_and_cross_version_replay_fail() {
        let key = SecretKey::from_bytes([42; 32]);
        let value = photo();
        let body = value.encode(1000).unwrap();
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
                Err(Error::Protocol(ProtocolError::ClockSkew))
            );
        }
        let mut changed = value.clone();
        changed.entities[0].kind = EntityKind::Code;
        let entities = changed.encode(1000).unwrap();
        changed = value;
        let Some(Attachment::Png { data_base64 }) = &mut changed.attachment else {
            panic!("fixture photo");
        };
        let replacement = if data_base64.as_bytes()[45] == b'A' {
            "B"
        } else {
            "A"
        };
        data_base64.replace_range(45..46, replacement);
        let png = serde_json::to_vec(&changed).unwrap();
        for changed in [entities, png, [body.as_slice(), b" "].concat()] {
            assert_eq!(
                verify_request(&key, "POST", MESSAGES_PATH, &auth, &changed, 1000),
                Err(Error::Protocol(ProtocolError::InvalidSignature))
            );
        }
        assert_eq!(
            crate::verify_request(&key, "POST", crate::MESSAGES_PATH, &auth, &body, 1000),
            Err(ProtocolError::UnsupportedVersion)
        );
        let v1 = crate::sign_request(&key, "GET", crate::STATUS_PATH, 1000, NONCE, b"").unwrap();
        assert_eq!(
            verify_request(&key, "GET", STATUS_PATH, &v1, b"", 1000),
            Err(Error::Protocol(ProtocolError::UnsupportedVersion))
        );
        let mut changed_auth = auth;
        changed_auth.timestamp = "1001".into();
        assert!(verify_request(&key, "POST", MESSAGES_PATH, &changed_auth, &body, 1000).is_err());
    }

    #[test]
    fn responses_bind_version_nonce_status_body_and_role() {
        let key = SecretKey::from_bytes([42; 32]);
        let body = br#"{"version":2}"#;
        let auth = sign_response(&key, NONCE, 200, body).unwrap();
        assert_eq!(verify_response(&key, NONCE, 200, &auth, body), Ok(()));
        assert!(verify_response(&key, NONCE, 503, &auth, body).is_err());
        assert!(verify_response(&key, NONCE, 200, &auth, b"{}").is_err());
        assert!(
            verify_response(&SecretKey::from_bytes([43; 32]), NONCE, 200, &auth, body).is_err()
        );
        assert!(
            verify_response(&key, "111102030405060708090a0b0c0d0e0f", 200, &auth, body).is_err()
        );
        assert!(crate::verify_response(&key, NONCE, 200, &auth, body).is_err());
        let v1 = crate::sign_response(&key, NONCE, 200, body).unwrap();
        assert!(verify_response(&key, NONCE, 200, &v1, body).is_err());
        let request = sign_request(&key, "GET", STATUS_PATH, 1000, NONCE, b"").unwrap();
        let forged = ResponseAuth {
            version: request.version,
            nonce: request.nonce,
            signature: request.signature,
        };
        assert!(verify_response(&key, NONCE, 200, &forged, b"").is_err());
    }

    #[test]
    fn exact_v2_capabilities_cannot_be_mistaken_for_legacy_support() {
        let mut status = RelayStatus {
            version: VERSION,
            capabilities: CAPABILITIES.map(str::to_owned).to_vec(),
            telegram_ready: true,
            checked_at: 1000,
            error: None,
        };
        assert_eq!(status.validate(), Ok(()));
        assert_eq!(
            serde_json::to_string(&status).unwrap(),
            r#"{"version":2,"capabilities":["text","text_entities","photo_png"],"telegramReady":true,"checkedAt":1000,"error":null}"#
        );
        for capabilities in [
            vec!["text".into()],
            vec![],
            vec!["text".into(), "photo_png".into(), "text_entities".into()],
        ] {
            status.capabilities = capabilities;
            assert_eq!(
                status.validate(),
                Err(Error::Protocol(ProtocolError::InvalidStatus))
            );
        }
        let v1 = crate::RelayStatus {
            version: 1,
            capabilities: vec!["text".into()],
            telegram_ready: true,
            checked_at: 1000,
            error: None,
        };
        assert!(v1.validate().is_ok());
        let mut invalid_v1 = v1;
        invalid_v1.capabilities = CAPABILITIES.map(str::to_owned).to_vec();
        assert!(invalid_v1.validate().is_err());
        let mut response = RelayMessageResponse {
            version: VERSION,
            id: "setup:42".into(),
            outcome: RelayOutcome::Duplicate,
            telegram_message_id: Some(42),
        };
        assert!(response.validate().is_ok());
        response.telegram_message_id = Some(0);
        assert!(response.validate().is_err());
        let mut error = RelayErrorResponse {
            version: VERSION,
            code: "caption_too_long".into(),
            retry_after: Some(MAX_RETRY_AFTER_SECONDS),
        };
        assert!(error.validate().is_ok());
        error.retry_after = Some(MAX_RETRY_AFTER_SECONDS + 1);
        assert!(error.validate().is_err());
        error.retry_after = None;
        error.code = "unsafe upstream value\n".into();
        assert!(error.validate().is_err());
    }

    #[test]
    fn diagnostics_do_not_dump_text_or_encoded_png() {
        let value = photo();
        let debug = format!("{value:?}");
        assert!(!debug.contains(&value.text));
        let Some(Attachment::Png { data_base64 }) = &value.attachment else {
            panic!("fixture");
        };
        assert!(!debug.contains(data_base64));
        assert!(debug.len() < 512);
        assert_eq!(Error::InvalidPng.to_string(), "invalid_png");
    }
}
