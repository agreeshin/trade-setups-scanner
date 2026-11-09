use crate::{DbError, settings_get};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::{Value, json};
use sqlx::Row;
use sqlx::postgres::PgPool;

pub const VAPID_SETTING_KEY: &str = "internal.web-push.vapid";
const MAX_SUBSCRIPTIONS: i64 = 100;
const PUSH_LOCK_KEY: i64 = 730_031;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushSubscriptionRow {
    pub endpoint: String,
    pub p256dh: String,
    pub auth: String,
}

pub async fn save_push_subscription(
    pool: &PgPool,
    endpoint: &str,
    p256dh: &str,
    auth: &str,
) -> Result<bool, DbError> {
    let mut tx = pool.begin().await?;
    sqlx::query("select pg_advisory_xact_lock($1)")
        .bind(PUSH_LOCK_KEY)
        .execute(&mut *tx)
        .await?;
    let existing = sqlx::query("select endpoint from push_subscriptions where endpoint = $1")
        .bind(endpoint)
        .fetch_optional(&mut *tx)
        .await?;
    if existing.is_none() {
        let count = sqlx::query("select count(*)::int as count from push_subscriptions")
            .fetch_one(&mut *tx)
            .await?;
        if i64::from(count.try_get::<i32, _>("count")?) >= MAX_SUBSCRIPTIONS {
            tx.commit().await?;
            return Ok(false);
        }
    }
    sqlx::query(
        "insert into push_subscriptions (endpoint, p256dh, auth) values ($1, $2, $3) \
         on conflict (endpoint) do update set p256dh = excluded.p256dh, auth = excluded.auth",
    )
    .bind(endpoint)
    .bind(p256dh)
    .bind(auth)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(true)
}

pub async fn delete_push_subscription(pool: &PgPool, endpoint: &str) -> Result<(), DbError> {
    sqlx::query("delete from push_subscriptions where endpoint = $1")
        .bind(endpoint)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn count_push_subscriptions(pool: &PgPool) -> Result<i64, DbError> {
    let row = sqlx::query("select count(*)::int as count from push_subscriptions")
        .fetch_one(pool)
        .await?;
    Ok(i64::from(row.try_get::<i32, _>("count")?))
}

pub async fn list_push_subscriptions(
    pool: &PgPool,
    limit: i64,
) -> Result<Vec<PushSubscriptionRow>, DbError> {
    let rows = sqlx::query("select endpoint, p256dh, auth from push_subscriptions limit $1")
        .bind(limit as i32)
        .fetch_all(pool)
        .await?;
    rows.iter()
        .map(|row| {
            Ok(PushSubscriptionRow {
                endpoint: row.try_get("endpoint")?,
                p256dh: row.try_get("p256dh")?,
                auth: row.try_get("auth")?,
            })
        })
        .collect()
}

pub fn generate_vapid_keypair() -> Result<(String, String), String> {
    use p256::ecdsa::SigningKey;
    use p256::elliptic_curve::rand_core::OsRng;
    let signing = SigningKey::random(&mut OsRng);
    let public = signing.verifying_key().to_encoded_point(false);
    let public_b64 = URL_SAFE_NO_PAD.encode(public.as_bytes());
    let private_b64 = URL_SAFE_NO_PAD.encode(signing.to_bytes());
    Ok((public_b64, private_b64))
}

async fn read_vapid_keys(pool: &PgPool) -> Result<Option<(String, String)>, DbError> {
    let Some(stored) = settings_get(pool, VAPID_SETTING_KEY).await? else {
        return Ok(None);
    };
    match (
        stored.get("publicKey").and_then(Value::as_str),
        stored.get("privateKey").and_then(Value::as_str),
    ) {
        (Some(public_key), Some(private_key)) => {
            Ok(Some((public_key.to_string(), private_key.to_string())))
        }
        _ => Err(DbError::Json(
            "invalid stored Web Push signing configuration".to_string(),
        )),
    }
}

pub async fn get_or_create_vapid_keys(pool: &PgPool) -> Result<(String, String), DbError> {
    if let Some(stored) = read_vapid_keys(pool).await? {
        return Ok(stored);
    }
    let (public_key, private_key) = generate_vapid_keypair().map_err(DbError::Json)?;
    let value = json!({ "publicKey": public_key, "privateKey": private_key });
    sqlx::query("insert into settings (key, value) values ($1, $2) on conflict (key) do nothing")
        .bind(VAPID_SETTING_KEY)
        .bind(&value)
        .execute(pool)
        .await?;
    read_vapid_keys(pool)
        .await?
        .ok_or_else(|| DbError::Json("vapid keys missing after insert".to_string()))
}

pub fn valid_push_endpoint(endpoint: &str) -> bool {
    if endpoint.len() > 2048 || !endpoint.starts_with("https://") {
        return false;
    }
    let rest = &endpoint[8..];
    if rest.contains('@') || rest.contains('#') {
        return false;
    }
    let authority_end = rest.find(['/', '?']).unwrap_or(rest.len());
    let host = &rest[..authority_end];
    if host.is_empty() || host.contains(':') {
        return false;
    }
    host == "fcm.googleapis.com"
        || host == "updates.push.services.mozilla.com"
        || host == "push.apple.com"
        || host.ends_with(".push.apple.com")
        || host == "notify.windows.com"
        || host.ends_with(".notify.windows.com")
}

fn base64url_part(value: &str) -> Option<(&str, usize)> {
    let trimmed = value.trim_end_matches('=');
    let pads = value.len() - trimmed.len();
    if pads > 2 || trimmed.is_empty() {
        return None;
    }
    if !trimmed
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    {
        return None;
    }
    let decoded_len = match trimmed.len() % 4 {
        0 => trimmed.len() / 4 * 3,
        2 => trimmed.len() / 4 * 3 + 1,
        3 => trimmed.len() / 4 * 3 + 2,
        _ => return None,
    };
    Some((trimmed, decoded_len))
}

pub fn valid_push_keys(p256dh: &str, auth: &str) -> bool {
    let Some((p256dh_trimmed, p256dh_len)) = base64url_part(p256dh) else {
        return false;
    };
    if !(86..=88).contains(&p256dh_trimmed.len()) || p256dh_len != 65 {
        return false;
    }
    match URL_SAFE_NO_PAD.decode(p256dh_trimmed) {
        Ok(bytes) if bytes.len() == 65 && bytes.first() == Some(&4) => {}
        _ => return false,
    }
    let Some((auth_trimmed, auth_len)) = base64url_part(auth) else {
        return false;
    };
    if auth_trimmed.len() != 22 || auth_len != 16 {
        return false;
    }
    URL_SAFE_NO_PAD
        .decode(auth_trimmed)
        .is_ok_and(|bytes| bytes.len() == 16)
}

pub fn valid_push_subscription(endpoint: &str, p256dh: &str, auth: &str) -> bool {
    valid_push_endpoint(endpoint) && valid_push_keys(p256dh, auth)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_allowlist_matches_ts() {
        assert!(valid_push_endpoint(
            "https://fcm.googleapis.com/fcm/send/abc"
        ));
        assert!(valid_push_endpoint(
            "https://updates.push.services.mozilla.com/wpush/v2/glyph"
        ));
        assert!(valid_push_endpoint("https://push.apple.com/x"));
        assert!(valid_push_endpoint("https://x.push.apple.com/x"));
        assert!(valid_push_endpoint("https://notify.windows.com/w"));
        assert!(valid_push_endpoint("https://x.notify.windows.com/w"));
        assert!(!valid_push_endpoint("http://fcm.googleapis.com/x"));
        assert!(!valid_push_endpoint(
            "https://user:pass@fcm.googleapis.com/x"
        ));
        assert!(!valid_push_endpoint("https://fcm.googleapis.com:443/x"));
        assert!(!valid_push_endpoint("https://fcm.googleapis.com/x#frag"));
        assert!(!valid_push_endpoint("https://evil.example.com/x"));
        assert!(!valid_push_endpoint("not-a-url"));
    }

    #[test]
    fn push_keys_require_65_byte_uncompressed_point_and_16_byte_auth() {
        let auth = ["A"; 22].concat();
        // 86 chars decode to 64 bytes, so a valid key needs 87 chars.
        let public = ["A"; 86].concat();
        assert!(!valid_push_keys(&public, &auth));
        // 'B' (sextet 1) followed by 'A' (sextet 0) decodes to first byte 0x04.
        let public = format!("B{}", ["A"; 86].concat());
        assert!(valid_push_keys(&public, &auth));
        let auth_short = ["A"; 21].concat();
        assert!(!valid_push_keys(&public, &auth_short));
    }

    #[test]
    fn vapid_keypair_shape_matches_web_push_format() {
        let (public, private) = generate_vapid_keypair().expect("keypair");
        let (public_trimmed, public_len) = base64url_part(&public).expect("public b64url");
        assert_eq!(public_trimmed.len(), 87);
        assert_eq!(public_len, 65);
        let public_bytes = URL_SAFE_NO_PAD.decode(public_trimmed).expect("decode");
        assert_eq!(public_bytes.first(), Some(&4u8));
        let (private_trimmed, private_len) = base64url_part(&private).expect("private b64url");
        assert_eq!(private_trimmed.len(), 43);
        assert_eq!(private_len, 32);
    }
}
