//! Owned codec tasks and acknowledged 1 MiB pipes; no snapshot-sized staging buffer.
//! Callers must await these functions through cancellation (never drop/abort the
//! future). The cancellation token closes the pipe and every exit joins its worker.
use crate::optimization::JobError;
use serde_json::{Value, json};
use setups_db::continuous::{frozen as storage, scheduler::ChainJob};
use setups_optimizer::{self as core, FrozenInputCodecLimits, FrozenInputDigest, StudyInput};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use std::{
    io::{self, Read, Write},
    sync::Arc,
};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

fn error(code: &'static str, message: impl std::fmt::Display) -> JobError {
    JobError::new(
        code,
        message.to_string().chars().take(600).collect::<String>(),
    )
}
fn database(e: setups_db::DbError) -> JobError {
    // Preserve typed transport/ACK uncertainty for the owner's retry bridge
    // before converting the error into its bounded diagnostic text.
    let code = if setups_db::continuous::recovery::is_transient(&e) {
        "persistence_unconfirmed"
    } else {
        "frozen_input_storage"
    };
    error(code, e)
}
fn cancelled() -> JobError {
    error(
        "chain_interrupted",
        "frozen input operation cancelled after joined codec cleanup",
    )
}

fn canonical(value: &Value) -> Value {
    match value {
        Value::Object(object) => {
            let mut keys: Vec<_> = object.keys().collect();
            keys.sort();
            Value::Object(
                keys.into_iter()
                    .map(|key| (key.clone(), canonical(&object[key])))
                    .collect(),
            )
        }
        Value::Array(values) => Value::Array(values.iter().map(canonical).collect()),
        _ => value.clone(),
    }
}
struct HashWriter(Sha256);
impl Write for HashWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.update(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
fn json_hash(value: &Value) -> String {
    let mut writer = HashWriter(Sha256::new());
    // JSON Value serialization into an infallible hash sink cannot fail.
    serde_json::to_writer(&mut writer, &canonical(value)).expect("JSON hash writer is infallible");
    format!("{:x}", writer.0.finalize())
}

/// Lease epoch/worker are intentionally absent: a compatible recovery changes
/// them. Large universe/spec/config values are bound by canonical hashes, so the
/// identity remains under the schema's 4 KiB cap. Array order remains meaningful.
pub fn identity(job: &ChainJob) -> Value {
    // Match continuous::without_live_permission exactly: this top-level flag is
    // a live permission, not a research coordinate, and may change without a
    // revision bump. Every other config field remains identity-bearing.
    let mut research_config = job.config.clone();
    if let Some(object) = research_config.as_object_mut() {
        object.remove("adaptiveEnabled");
    }
    canonical(&json!({
        "version": 1, "ownerId": job.lease.owner_id, "algorithmId": job.algorithm_id,
        "algorithmVersion": job.spec.get("algorithmVersion"),
        "optimizerVersion": core::CONTINUOUS_OPTIMIZER_VERSION,
        "checkpointVersion": core::CONTINUOUS_CHECKPOINT_VERSION,
        "checkpointRngVersion": core::CONTINUOUS_CHECKPOINT_RNG_VERSION,
        "wordBits": usize::BITS, "codec": "framed-cbor-1", "codecVersion": storage::FORMAT_VERSION,
        "generationId": job.generation_id, "slotId": job.slot_id, "studyId": job.study_id,
        "configRevision": job.config_revision, "specHash": json_hash(&job.spec), "configHash": json_hash(&research_config),
        // PostgreSQL timestamps retain microseconds; the initially constructed
        // ChainJob may still carry Utc::now() nanoseconds. Normalize both paths
        // so a genuine DB reload does not invalidate unchanged durable state.
        "executionStartedAt": job.execution_started_at.to_rfc3339_opts(chrono::SecondsFormat::Micros, true), "observationCutoff": job.observation_cutoff,
        "researchDeadline": job.research_deadline.map(|at| at.to_rfc3339_opts(chrono::SecondsFormat::Micros, true)),
    }))
}
fn encoded_cap(max_decoded_bytes: usize) -> Result<usize, JobError> {
    // Framed named-field CBOR is 2.087x retained bytes for the focused
    // candle-heavy fixture; 2x would still reject legitimate near-RAM-cap input.
    // This wire ceiling never changes decoded RAM or the aggregate disk quota.
    max_decoded_bytes
        .checked_mul(3)
        .filter(|cap| *cap > 0)
        .ok_or_else(|| error("resource_limit", "frozen encoded byte cap overflow/zero"))
}
fn persistence_cap(max_decoded_bytes: usize, max_runtime_bytes: i64) -> Result<usize, JobError> {
    let available = max_runtime_bytes
        .checked_sub(storage::STORAGE_HEADROOM)
        .and_then(|n| usize::try_from(n).ok())
        .filter(|n| *n > 0)
        .ok_or_else(|| {
            error(
                "resource_limit",
                "frozen runtime storage has no encoded snapshot headroom",
            )
        })?;
    Ok(encoded_cap(max_decoded_bytes)?.min(available))
}
fn limits(max_decoded_bytes: usize, max_encoded_bytes: usize) -> FrozenInputCodecLimits {
    FrozenInputCodecLimits {
        max_encoded_bytes: max_encoded_bytes as u64,
        max_decoded_bytes,
        max_record_bytes: storage::CHUNK_BYTES.min(max_encoded_bytes),
        max_records: (max_encoded_bytes as u64 / 8).max(1),
    }
}

struct Chunk {
    index: u32,
    bytes: Vec<u8>,
    ack: oneshot::Sender<()>,
}
struct ChunkWriter {
    sender: mpsc::Sender<Chunk>,
    buffer: Vec<u8>,
    next: u32,
}
impl ChunkWriter {
    fn flush_chunk(&mut self) -> io::Result<()> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        let bytes = std::mem::replace(&mut self.buffer, Vec::with_capacity(storage::CHUNK_BYTES));
        let (ack, received) = oneshot::channel();
        self.sender
            .blocking_send(Chunk {
                index: self.next,
                bytes,
                ack,
            })
            .map_err(|_| io::Error::other("frozen writer pipe closed"))?;
        received
            .blocking_recv()
            .map_err(|_| io::Error::other("frozen chunk not committed"))?;
        self.next = self
            .next
            .checked_add(1)
            .ok_or_else(|| io::Error::other("chunk index overflow"))?;
        Ok(())
    }
}
impl Write for ChunkWriter {
    fn write(&mut self, mut bytes: &[u8]) -> io::Result<usize> {
        let original = bytes.len();
        while !bytes.is_empty() {
            let count = bytes.len().min(storage::CHUNK_BYTES - self.buffer.len());
            self.buffer.extend_from_slice(&bytes[..count]);
            bytes = &bytes[count..];
            if self.buffer.len() == storage::CHUNK_BYTES {
                self.flush_chunk()?;
            }
        }
        Ok(original)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.flush_chunk()
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn persist(
    pool: &PgPool,
    job: &ChainJob,
    input: Arc<StudyInput>,
    max_snapshot_bytes: usize,
    max_runtime_bytes: i64,
    max_database_bytes: i64,
    replay_admission: &setups_db::admission::ReplayAdmissionLimits,
    cancel: &CancellationToken,
) -> Result<(), JobError> {
    if input.snapshot_hash.is_empty()
        || input.snapshot_hash.len() > 256
        || input.captured_at.is_empty()
        || input.captured_at.len() > 64
    {
        return Err(error(
            "frozen_input_encode",
            "invalid bounded input provenance",
        ));
    }
    let max_encoded_bytes = persistence_cap(max_snapshot_bytes, max_runtime_bytes)?;
    let identity = identity(job);
    let existing = tokio::select! {
        biased;
        _ = cancel.cancelled() => return Err(cancelled()),
        result = storage::reserve(pool, job, &identity, max_encoded_bytes, max_runtime_bytes, max_database_bytes, replay_admission) => result.map_err(database)?,
    };
    let snapshot_hash = input.snapshot_hash.clone();
    let captured_at = input.captured_at.clone();
    let codec_limits = limits(max_snapshot_bytes, max_encoded_bytes);
    let (sender, mut receiver) = mpsc::channel::<Chunk>(1);
    let worker = tokio::task::spawn_blocking(move || {
        let mut writer = ChunkWriter {
            sender,
            buffer: Vec::with_capacity(storage::CHUNK_BYTES),
            next: 0,
        };
        let digest = core::write_frozen_input(&input, &mut writer, &codec_limits)?;
        writer
            .flush()
            .map_err(|e| core::StudyError::InvalidInput(e.to_string()))?;
        Ok::<_, core::StudyError>((digest, writer.next))
    });
    let pumped = async {
        loop {
            let chunk = tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(cancelled()),
                next = receiver.recv() => match next { Some(chunk) => chunk, None => break },
            };
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(cancelled()),
                result = async {
                    if let Some(snapshot) = &existing {
                        let stored = storage::read_chunk(pool, job, &identity, snapshot, chunk.index).await?;
                        if stored != chunk.bytes { return Err(setups_db::DbError::Json("committed frozen input differs".into())); }
                        Ok(())
                    } else {
                        storage::write_chunk(pool, job, &identity, chunk.index, &chunk.bytes, max_encoded_bytes).await
                    }
                } => result.map_err(database)?,
            }
            // The codec cannot advance to another chunk before this commit/read.
            let _ = chunk.ack.send(());
        }
        Ok::<_, JobError>(())
    }.await;
    // Close queued acknowledgements as well as the receive endpoint BEFORE join.
    // A codec blocked on send/ack then exits without retaining its Arc input.
    drop(receiver);
    let encoded = worker
        .await
        .map_err(|e| error("frozen_input_codec_join", e));
    pumped?;
    let (digest, chunks) = encoded?.map_err(|e| error("frozen_input_encode", e))?;
    let snapshot = storage::SnapshotManifest {
        version: storage::FORMAT_VERSION,
        codec: "framed-cbor-1".into(),
        chunks,
        encoded_bytes: digest.encoded_bytes,
        sha256: hex_digest(&digest),
        snapshot_hash,
        captured_at,
    };
    if existing.as_ref().is_some_and(|old| old != &snapshot) {
        return Err(error(
            "frozen_input_corrupt",
            "committed frozen manifest differs from supplied input",
        ));
    }
    tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(cancelled()),
        result = storage::seal(pool, job, &identity, &snapshot, max_encoded_bytes, max_runtime_bytes, max_database_bytes, replay_admission) => result.map_err(database),
    }
}
fn hex_digest(digest: &FrozenInputDigest) -> String {
    digest
        .sha256
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

struct ChunkReader {
    receiver: mpsc::Receiver<Chunk>,
    current: Option<Chunk>,
    position: usize,
}
impl Read for ChunkReader {
    fn read(&mut self, target: &mut [u8]) -> io::Result<usize> {
        if target.is_empty() {
            return Ok(0);
        }
        if self.current.is_none() {
            self.current = self.receiver.blocking_recv();
            self.position = 0;
        }
        let Some(chunk) = &self.current else {
            return Ok(0);
        };
        let count = target.len().min(chunk.bytes.len() - self.position);
        target[..count].copy_from_slice(&chunk.bytes[self.position..self.position + count]);
        self.position += count;
        if self.position == chunk.bytes.len() {
            let completed = self.current.take().expect("current chunk exists");
            let _ = completed.ack.send(());
        }
        Ok(count)
    }
}

pub(crate) async fn load(
    pool: &PgPool,
    job: &ChainJob,
    max_snapshot_bytes: usize,
    cancel: &CancellationToken,
) -> Result<StudyInput, JobError> {
    let max_encoded_bytes = encoded_cap(max_snapshot_bytes)?;
    let identity = identity(job);
    let snapshot = tokio::select! {
        biased;
        _ = cancel.cancelled() => return Err(cancelled()),
        result = storage::load_manifest(pool, job, &identity, max_encoded_bytes) => result.map_err(database)?,
    };
    let codec_limits = limits(max_snapshot_bytes, max_encoded_bytes);
    let (sender, receiver) = mpsc::channel::<Chunk>(1);
    let worker = tokio::task::spawn_blocking(move || {
        let mut reader = ChunkReader {
            receiver,
            current: None,
            position: 0,
        };
        core::read_frozen_input(&mut reader, &codec_limits)
    });
    let pumped = async {
        for index in 0..snapshot.chunks {
            let bytes = tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(cancelled()),
                result = storage::read_chunk(pool, job, &identity, &snapshot, index) => result.map_err(database)?,
            };
            let (ack, received) = oneshot::channel();
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(cancelled()),
                result = sender.send(Chunk { index, bytes, ack }) => result.map_err(|_| error("frozen_input_corrupt", "decoder closed before snapshot end"))?,
            }
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(cancelled()),
                result = received => result.map_err(|_| error("frozen_input_corrupt", "decoder rejected snapshot chunk"))?,
            }
        }
        Ok::<_, JobError>(())
    }.await;
    drop(sender);
    let decoded = worker
        .await
        .map_err(|e| error("frozen_input_codec_join", e));
    pumped?;
    let (input, digest) = decoded?.map_err(|e| error("frozen_input_decode", e))?;
    if digest.encoded_bytes != snapshot.encoded_bytes
        || hex_digest(&digest) != snapshot.sha256
        || input.snapshot_hash != snapshot.snapshot_hash
        || input.captured_at != snapshot.captured_at
    {
        return Err(error(
            "frozen_input_corrupt",
            "snapshot checksum/provenance mismatch",
        ));
    }
    // Recheck the authoritative fence/manifest after the last chunk, before
    // returning any decoded input to evaluation (also covers Stop during decode).
    tokio::select! {
        biased;
        _ = cancel.cancelled() => return Err(cancelled()),
        result = storage::load_manifest(pool, job, &identity, max_encoded_bytes) => {
            if result.map_err(database)? != snapshot { return Err(error("frozen_input_corrupt", "snapshot changed during restore")); }
        }
    }
    Ok(input)
}

#[cfg(test)]
#[path = "frozen_tests.rs"]
mod tests;
