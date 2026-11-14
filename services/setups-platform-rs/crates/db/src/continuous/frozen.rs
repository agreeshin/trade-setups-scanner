//! Generation-owned frozen bytes. Every transaction is short and rechecks the lease.
use super::{
    recovery::MAX_CHECKPOINT_BYTES,
    scheduler::{self, ChainJob},
};
use crate::{
    DbError,
    admission::{QueueError, ReplayAdmissionLimits},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Postgres, Row, Transaction};

pub const CHUNK_BYTES: usize = 1024 * 1024;
pub const FORMAT_VERSION: u32 = 1;
// One current checkpoint plus its replacement/WAL allowance and two chunk buffers.
// This is bounded headroom, not an indefinitely retained second full snapshot.
pub const STORAGE_HEADROOM: i64 = (2 * MAX_CHECKPOINT_BYTES + 2 * CHUNK_BYTES) as i64;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SnapshotManifest {
    pub version: u32,
    pub codec: String,
    pub chunks: u32,
    pub encoded_bytes: u64,
    pub sha256: String,
    pub snapshot_hash: String,
    pub captured_at: String,
}

fn invalid(message: &str) -> DbError {
    DbError::Json(format!("frozen_input: {message}"))
}
pub fn checksum(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

impl SnapshotManifest {
    pub fn validate(&self, max_encoded_bytes: usize) -> Result<(), DbError> {
        if self.version != FORMAT_VERSION
            || self.codec != "framed-cbor-1"
            || self.encoded_bytes == 0
            || self.encoded_bytes > max_encoded_bytes as u64
            || self.chunks > i32::MAX as u32
            || u64::from(self.chunks) != self.encoded_bytes.div_ceil(CHUNK_BYTES as u64)
            || self.sha256.len() != 64
            || !self
                .sha256
                .bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
            || self.snapshot_hash.is_empty()
            || self.snapshot_hash.len() > 256
            || self.captured_at.is_empty()
            || self.captured_at.len() > 64
        {
            return Err(invalid("invalid or incompatible snapshot manifest"));
        }
        Ok(())
    }
}

async fn fence(tx: &mut Transaction<'_, Postgres>, job: &ChainJob) -> Result<(), DbError> {
    if !scheduler::fenced(tx, job).await? {
        return Err(invalid("lease/revision/owner fence lost"));
    }
    Ok(())
}
fn validate_identity(identity: &Value) -> Result<(), DbError> {
    if !identity.is_object()
        || serde_json::to_vec(identity)
            .map_err(crate::json_error)?
            .len()
            > 4096
    {
        return Err(invalid("identity exceeds bounded object budget"));
    }
    Ok(())
}
async fn storage_lock(tx: &mut Transaction<'_, Postgres>) -> Result<(), DbError> {
    sqlx::query("select pg_advisory_xact_lock(72423412)")
        .execute(&mut **tx)
        .await?;
    Ok(())
}
async fn admission(
    tx: &mut Transaction<'_, Postgres>,
    max_runtime: i64,
    max_database: i64,
    replay_admission: &ReplayAdmissionLimits,
) -> Result<(), DbError> {
    let reserved: i64 = sqlx::query_scalar(
        "select coalesce(sum(reserved_bytes),0)::bigint from continuous_runtime_artifacts",
    )
    .fetch_one(&mut **tx)
    .await?;
    // The shared aggregate ALREADY includes outstanding private bytes, all
    // continuous replay reservations, and manual replay estimates. Counting our
    // private remainder again here would double-charge the same admitted work.
    let remaining = crate::admission::active_reservation_bytes(&mut *tx, replay_admission, None)
        .await
        .map_err(|error| match error {
            QueueError::Db(error) => error,
            error => DbError::Json(error.to_string()),
        })?;
    let actual: i64 = sqlx::query_scalar("select pg_database_size(current_database())")
        .fetch_one(&mut **tx)
        .await?;
    if max_runtime <= 0
        || max_database <= 0
        || reserved > max_runtime
        || actual
            .checked_add(remaining)
            .is_none_or(|size| size > max_database)
    {
        return Err(invalid("runtime/database storage resource_limit"));
    }
    Ok(())
}
fn manifest(value: Value, limit: usize) -> Result<SnapshotManifest, DbError> {
    let value: SnapshotManifest =
        serde_json::from_value(value).map_err(|_| invalid("malformed snapshot manifest"))?;
    value.validate(limit)?;
    Ok(value)
}

/// Reserve before encoding. Only an uncommitted acquisition may be discarded.
/// Existing committed bytes/checkpoint/gate are never replaced by this operation.
pub async fn reserve(
    pool: &PgPool,
    job: &ChainJob,
    identity: &Value,
    max_encoded_bytes: usize,
    max_runtime_bytes: i64,
    max_database_bytes: i64,
    replay_admission: &ReplayAdmissionLimits,
) -> Result<Option<SnapshotManifest>, DbError> {
    validate_identity(identity)?;
    let reserved = i64::try_from(max_encoded_bytes)
        .ok()
        .and_then(|n| n.checked_add(STORAGE_HEADROOM))
        .filter(|_| max_encoded_bytes > 0)
        .ok_or_else(|| invalid("snapshot reservation resource_limit"))?;
    let mut tx = pool.begin().await?;
    fence(&mut tx, job).await?;
    storage_lock(&mut tx).await?;
    let existing = sqlx::query("select identity,snapshot_manifest,checkpoint is not null or gate_result is not null derived from continuous_runtime_artifacts where generation_id=$1 for update")
        .bind(job.generation_id).fetch_optional(&mut *tx).await?;
    if let Some(row) = existing {
        if row.try_get::<Value, _>("identity")? != *identity {
            return Err(invalid("identity incompatible"));
        }
        if let Some(value) = row.try_get::<Option<Value>, _>("snapshot_manifest")? {
            let result = manifest(value, max_encoded_bytes)?;
            admission(
                &mut tx,
                max_runtime_bytes,
                max_database_bytes,
                replay_admission,
            )
            .await?;
            tx.commit().await?;
            return Ok(Some(result));
        }
        if row.try_get::<bool, _>("derived")? {
            return Err(invalid("derived state without committed input"));
        }
        sqlx::query("delete from continuous_frozen_chunks where generation_id=$1")
            .bind(job.generation_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("update continuous_runtime_artifacts set reserved_bytes=$2,snapshot_bytes=0,updated_at=now() where generation_id=$1")
            .bind(job.generation_id).bind(reserved).execute(&mut *tx).await?;
    } else {
        sqlx::query("insert into continuous_runtime_artifacts(generation_id,identity,reserved_bytes) values($1,$2,$3)")
            .bind(job.generation_id).bind(identity).bind(reserved).execute(&mut *tx).await?;
    }
    admission(
        &mut tx,
        max_runtime_bytes,
        max_database_bytes,
        replay_admission,
    )
    .await?;
    tx.commit().await?;
    Ok(None)
}

/// Acknowledged only after commit; exact repeated chunks do not double-account.
pub async fn write_chunk(
    pool: &PgPool,
    job: &ChainJob,
    identity: &Value,
    index: u32,
    payload: &[u8],
    max_encoded_bytes: usize,
) -> Result<(), DbError> {
    if payload.is_empty() || payload.len() > CHUNK_BYTES || index > i32::MAX as u32 {
        return Err(invalid("chunk resource_limit"));
    }
    let hash = checksum(payload);
    let mut tx = pool.begin().await?;
    fence(&mut tx, job).await?;
    let row = sqlx::query("select identity,snapshot_manifest,snapshot_bytes,reserved_bytes from continuous_runtime_artifacts where generation_id=$1 for update")
        .bind(job.generation_id).fetch_optional(&mut *tx).await?.ok_or_else(|| invalid("reservation missing"))?;
    if row.try_get::<Value, _>("identity")? != *identity
        || row
            .try_get::<Option<Value>, _>("snapshot_manifest")?
            .is_some()
    {
        return Err(invalid(
            "identity mismatch or committed snapshot is immutable",
        ));
    }
    if let Some(old) = sqlx::query("select checksum,octet_length(payload) bytes from continuous_frozen_chunks where generation_id=$1 and chunk_index=$2")
        .bind(job.generation_id).bind(index as i32).fetch_optional(&mut *tx).await? {
        if old.try_get::<String, _>("checksum")? != hash || old.try_get::<i32, _>("bytes")? != payload.len() as i32 {
            return Err(invalid("different repeated chunk"));
        }
        tx.commit().await?;
        return Ok(());
    }
    let previous: i64 = row.try_get("snapshot_bytes")?;
    let total = previous
        .checked_add(payload.len() as i64)
        .ok_or_else(|| invalid("snapshot byte overflow"))?;
    if previous != i64::from(index) * CHUNK_BYTES as i64
        || total as u64 > max_encoded_bytes as u64
        || total
            .checked_add(STORAGE_HEADROOM)
            .is_none_or(|n| n > row.get::<i64, _>("reserved_bytes"))
    {
        return Err(invalid("chunk order or snapshot resource_limit"));
    }
    sqlx::query("insert into continuous_frozen_chunks(generation_id,chunk_index,payload,checksum) values($1,$2,$3,$4)")
        .bind(job.generation_id).bind(index as i32).bind(payload).bind(hash).execute(&mut *tx).await?;
    sqlx::query("update continuous_runtime_artifacts set snapshot_bytes=$2,updated_at=now() where generation_id=$1")
        .bind(job.generation_id).bind(total).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub async fn seal(
    pool: &PgPool,
    job: &ChainJob,
    identity: &Value,
    snapshot: &SnapshotManifest,
    max_encoded_bytes: usize,
    max_runtime_bytes: i64,
    max_database_bytes: i64,
    replay_admission: &ReplayAdmissionLimits,
) -> Result<(), DbError> {
    snapshot.validate(max_encoded_bytes)?;
    let mut tx = pool.begin().await?;
    fence(&mut tx, job).await?;
    storage_lock(&mut tx).await?;
    let row = sqlx::query("select identity,snapshot_manifest,snapshot_bytes from continuous_runtime_artifacts where generation_id=$1 for update")
        .bind(job.generation_id).fetch_optional(&mut *tx).await?.ok_or_else(|| invalid("reservation missing"))?;
    if row.try_get::<Value, _>("identity")? != *identity {
        return Err(invalid("seal identity mismatch"));
    }
    if let Some(old) = row.try_get::<Option<Value>, _>("snapshot_manifest")? {
        if manifest(old, max_encoded_bytes)? != *snapshot {
            return Err(invalid("committed snapshot is immutable"));
        }
    } else {
        let chunks = sqlx::query("select count(*)::bigint count,coalesce(sum(octet_length(payload)),0)::bigint bytes,min(chunk_index) first,max(chunk_index) last,count(*) filter(where octet_length(payload)<>1048576 and chunk_index<>$2)::bigint short from continuous_frozen_chunks where generation_id=$1")
            .bind(job.generation_id).bind(snapshot.chunks as i32 - 1).fetch_one(&mut *tx).await?;
        if chunks.try_get::<i64, _>("count")? != i64::from(snapshot.chunks)
            || chunks.try_get::<i64, _>("bytes")? as u64 != snapshot.encoded_bytes
            || row.try_get::<i64, _>("snapshot_bytes")? as u64 != snapshot.encoded_bytes
            || chunks.try_get::<Option<i32>, _>("first")? != Some(0)
            || chunks.try_get::<Option<i32>, _>("last")? != Some(snapshot.chunks as i32 - 1)
            || chunks.try_get::<i64, _>("short")? != 0
        {
            return Err(invalid("incomplete snapshot chunks"));
        }
        sqlx::query("update continuous_runtime_artifacts set snapshot_manifest=$2,reserved_bytes=snapshot_bytes+$3,updated_at=now() where generation_id=$1")
            .bind(job.generation_id).bind(serde_json::to_value(snapshot).map_err(crate::json_error)?)
            .bind(STORAGE_HEADROOM).execute(&mut *tx).await?;
        super::recovery::acknowledge_boundary(&mut tx, job).await?;
    }
    admission(
        &mut tx,
        max_runtime_bytes,
        max_database_bytes,
        replay_admission,
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn load_manifest(
    pool: &PgPool,
    job: &ChainJob,
    identity: &Value,
    max_encoded_bytes: usize,
) -> Result<SnapshotManifest, DbError> {
    let mut tx = pool.begin().await?;
    fence(&mut tx, job).await?;
    let row = sqlx::query("select identity,snapshot_manifest,snapshot_bytes from continuous_runtime_artifacts where generation_id=$1")
        .bind(job.generation_id).fetch_optional(&mut *tx).await?.ok_or_else(|| invalid("committed snapshot missing"))?;
    if row.try_get::<Value, _>("identity")? != *identity {
        return Err(invalid("identity incompatible"));
    }
    let result = manifest(
        row.try_get::<Option<Value>, _>("snapshot_manifest")?
            .ok_or_else(|| invalid("snapshot incomplete"))?,
        max_encoded_bytes,
    )?;
    if row.try_get::<i64, _>("snapshot_bytes")? as u64 != result.encoded_bytes {
        return Err(invalid("snapshot length mismatch"));
    }
    let chunks: i64 = sqlx::query_scalar(
        "select count(*)::bigint from continuous_frozen_chunks where generation_id=$1",
    )
    .bind(job.generation_id)
    .fetch_one(&mut *tx)
    .await?;
    if chunks != i64::from(result.chunks) {
        return Err(invalid("snapshot chunk count mismatch"));
    }
    tx.commit().await?;
    Ok(result)
}

pub async fn read_chunk(
    pool: &PgPool,
    job: &ChainJob,
    identity: &Value,
    snapshot: &SnapshotManifest,
    index: u32,
) -> Result<Vec<u8>, DbError> {
    if index >= snapshot.chunks {
        return Err(invalid("chunk index outside manifest"));
    }
    let mut tx = pool.begin().await?;
    fence(&mut tx, job).await?;
    let row = sqlx::query("select c.payload,c.checksum from continuous_frozen_chunks c join continuous_runtime_artifacts a using(generation_id) where c.generation_id=$1 and c.chunk_index=$2 and a.identity=$3 and a.snapshot_manifest=$4 and a.snapshot_bytes=$5")
        .bind(job.generation_id).bind(index as i32).bind(identity)
        .bind(serde_json::to_value(snapshot).map_err(crate::json_error)?)
        .bind(snapshot.encoded_bytes as i64).fetch_optional(&mut *tx).await?.ok_or_else(|| invalid("chunk missing or manifest/identity changed"))?;
    let bytes: Vec<u8> = row.try_get("payload")?;
    let expected = if index + 1 == snapshot.chunks {
        ((snapshot.encoded_bytes - 1) % CHUNK_BYTES as u64 + 1) as usize
    } else {
        CHUNK_BYTES
    };
    if bytes.len() != expected || checksum(&bytes) != row.try_get::<String, _>("checksum")? {
        return Err(invalid("chunk checksum/length mismatch"));
    }
    tx.commit().await?;
    Ok(bytes)
}
