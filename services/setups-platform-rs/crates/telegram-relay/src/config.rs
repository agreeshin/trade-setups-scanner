use crate::{RelayError, store::StoreLimits};
use setups_relay_protocol::SecretKey;
use std::{
    fs::File,
    io::Read,
    net::SocketAddr,
    path::{Path, PathBuf},
};

/// Never Debug/Serialize configuration: it owns the only bot token and MAC key.
pub struct Config {
    pub(crate) key: SecretKey,
    pub(crate) token: String,
    pub(crate) bot_id: i64,
    pub(crate) chat_id: i64,
    pub(crate) state_path: PathBuf,
    pub(crate) listen: SocketAddr,
    pub(crate) concurrency: usize,
    pub(crate) limits: StoreLimits,
}
impl Config {
    pub fn from_env() -> Result<Self, RelayError> {
        Self::from_lookup(|name| std::env::var(name).ok())
    }
    fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Self, RelayError> {
        let key_path =
            get("RELAY_SIGNING_KEY_FILE").ok_or(RelayError::Config("signing_key_file_required"))?;
        let key = SecretKey::from_private_file(key_path)
            .map_err(|_| RelayError::Config("signing_key_file_invalid"))?;
        let token_path =
            get("RELAY_TOKEN_FILE").ok_or(RelayError::Config("token_file_required"))?;
        let token = read_token(Path::new(&token_path))?;
        let bot_id = token
            .split_once(':')
            .and_then(|(id, _)| id.parse::<i64>().ok())
            .filter(|id| *id > 0)
            .ok_or(RelayError::Config("token_file_invalid"))?;
        let chat_id = get("RELAY_CHAT_ID")
            .and_then(|v| v.parse::<i64>().ok())
            .filter(|id| *id < 0)
            .ok_or(RelayError::Config("private_channel_id_required"))?;
        let state_path = PathBuf::from(
            get("RELAY_STATE_PATH")
                .unwrap_or_else(|| "/var/lib/telegram-relay/relay.sqlite".into()),
        );
        if !state_path.is_absolute() {
            return Err(RelayError::Config("state_path_must_be_absolute"));
        }
        let listen = get("RELAY_LISTEN")
            .unwrap_or_else(|| "0.0.0.0:8080".into())
            .parse()
            .map_err(|_| RelayError::Config("listen_address_invalid"))?;
        let number =
            |name: &str, default: usize, min: usize, max: usize| -> Result<usize, RelayError> {
                let value = get(name)
                    .map(|v| v.parse::<usize>())
                    .transpose()
                    .map_err(|_| RelayError::Config("resource_limit_invalid"))?
                    .unwrap_or(default);
                if !(min..=max).contains(&value) {
                    return Err(RelayError::Config("resource_limit_invalid"));
                }
                Ok(value)
            };
        Ok(Self {
            key,
            token,
            bot_id,
            chat_id,
            state_path,
            listen,
            concurrency: number("RELAY_MAX_CONCURRENCY", 2, 1, 16)?,
            limits: StoreLimits {
                max_receipts: number("RELAY_MAX_RECEIPTS", 10000, 1, 100000)?,
                max_bytes: number(
                    "RELAY_MAX_DATABASE_BYTES",
                    16 * 1024 * 1024,
                    1024 * 1024,
                    256 * 1024 * 1024,
                )?,
                retention_seconds: number("RELAY_RECEIPT_RETENTION_SECONDS", 3600, 60, 86400)?
                    as i64,
                lease_seconds: 20,
            },
        })
    }
}
fn read_token(path: &Path) -> Result<String, RelayError> {
    let bad = || RelayError::Config("token_file_invalid");
    let before = std::fs::symlink_metadata(path).map_err(|_| bad())?;
    if !before.file_type().is_file() || before.len() == 0 || before.len() > 256 {
        return Err(bad());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if before.permissions().mode() & 0o077 != 0 {
            return Err(bad());
        }
    }
    #[cfg(not(unix))]
    return Err(bad());
    let file = File::open(path).map_err(|_| bad())?;
    let opened = file.metadata().map_err(|_| bad())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if before.dev() != opened.dev()
            || before.ino() != opened.ino()
            || opened.permissions().mode() & 0o077 != 0
        {
            return Err(bad());
        }
    }
    let mut bytes = Vec::new();
    file.take(257).read_to_end(&mut bytes).map_err(|_| bad())?;
    if bytes.len() > 256 {
        return Err(bad());
    }
    let text = std::str::from_utf8(&bytes).map_err(|_| bad())?;
    let text = text.strip_suffix('\n').unwrap_or(text);
    let Some((id, secret)) = text.split_once(':') else {
        return Err(bad());
    };
    if id.is_empty()
        || !id.bytes().all(|b| b.is_ascii_digit())
        || secret.len() < 20
        || !secret
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err(bad());
    }
    let token = text.to_owned();
    bytes.fill(0);
    Ok(token)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn missing_configuration_errors_are_literal_and_do_not_echo_values() {
        assert_eq!(
            Config::from_lookup(|_| None).err().unwrap().to_string(),
            "signing_key_file_required"
        );
        assert_eq!(
            Config::from_lookup(|_| Some("sensitive-value-never-echo".into()))
                .err()
                .unwrap()
                .to_string(),
            "signing_key_file_invalid"
        );
    }
}
