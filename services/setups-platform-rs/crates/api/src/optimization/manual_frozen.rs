//! Acknowledged one-chunk pipes. Every exit closes the pipe and joins its codec;
//! no full encoded snapshot or detached worker survives pause/cancel/failure.
use super::JobError;
use setups_db::optimization::manual::{self as db,Job,Limits};
use setups_db::continuous::frozen::{SnapshotManifest,CHUNK_BYTES};
use setups_optimizer::{self as core,StudyInput,FrozenInputCodecLimits};
use serde_json::Value;
use sqlx::PgPool;
use std::{io::{self,Read,Write},sync::Arc};
use tokio::sync::{mpsc,oneshot};

fn error(error:impl std::fmt::Display)->JobError {JobError::new("manual_frozen_input",error.to_string())}
fn database(error:setups_db::DbError)->JobError {
    JobError::new(if matches!(&error,setups_db::DbError::Json(message) if message.contains("manual_snapshot_cancelled")){"manual_snapshot_cancelled"}
        else if setups_db::continuous::recovery::is_transient(&error){"persistence_unconfirmed"}
        else if error.to_string().contains("resource_limit"){"resource_limit"}else{"manual_frozen_input"},error.to_string())
}
fn codec_limits(decoded:usize,encoded:usize)->FrozenInputCodecLimits {
    FrozenInputCodecLimits{max_decoded_bytes:decoded,max_encoded_bytes:encoded as u64,max_record_bytes:CHUNK_BYTES.min(encoded),max_records:(encoded as u64/8).max(1)}
}
struct Chunk {index:u32,bytes:Vec<u8>,ack:oneshot::Sender<()>}
struct Writer {sender:mpsc::Sender<Chunk>,buffer:Vec<u8>,next:u32}
impl Writer {
    fn flush_chunk(&mut self)->io::Result<()> {
        if self.buffer.is_empty(){return Ok(());}
        let bytes=std::mem::replace(&mut self.buffer,Vec::with_capacity(CHUNK_BYTES));
        let (ack,received)=oneshot::channel();
        self.sender.blocking_send(Chunk{index:self.next,bytes,ack}).map_err(|_|io::Error::other("manual encoder pipe closed"))?;
        received.blocking_recv().map_err(|_|io::Error::other("manual chunk not committed"))?;
        self.next=self.next.checked_add(1).ok_or_else(||io::Error::other("chunk index overflow"))?;Ok(())
    }
}
impl Write for Writer {
    fn write(&mut self,mut bytes:&[u8])->io::Result<usize> {
        let len=bytes.len();while !bytes.is_empty(){let n=bytes.len().min(CHUNK_BYTES-self.buffer.len());self.buffer.extend_from_slice(&bytes[..n]);bytes=&bytes[n..];if self.buffer.len()==CHUNK_BYTES {self.flush_chunk()?;}}Ok(len)
    }
    fn flush(&mut self)->io::Result<()> {self.flush_chunk()}
}
pub(super) async fn persist(pool:&PgPool,job:&Job,input:Arc<StudyInput>,identity:&Value,observation_cutoff:i64,max_decoded:usize,limits:&Limits,stop:&tokio_util::sync::CancellationToken)->Result<(),JobError> {
    let cap=db::reserve(pool,job,identity,max_decoded.saturating_mul(3),limits).await.map_err(database)?;
    let hash=input.snapshot_hash.clone();let captured_at=input.captured_at.clone();
    let codec=codec_limits(max_decoded,cap);let(sender,mut receiver)=mpsc::channel::<Chunk>(1);
    let worker=tokio::task::spawn_blocking(move|| {
        let mut writer=Writer{sender,buffer:Vec::with_capacity(CHUNK_BYTES),next:0};
        let digest=core::write_frozen_input(&input,&mut writer,&codec)?;
        writer.flush().map_err(|e|core::StudyError::Execution(e.to_string()))?;
        Ok::<_,core::StudyError>((digest,writer.next))
    });
    let pumped=async {
        loop {
            let chunk=tokio::select!{biased;_=stop.cancelled()=>return Err(error("manual_shutdown_deferred")),chunk=receiver.recv()=>match chunk{Some(chunk)=>chunk,None=>break}};
            db::write_chunk(pool,job,chunk.index,&chunk.bytes,limits).await.map_err(database)?;
            let _=chunk.ack.send(());
        }Ok::<_,JobError>(())
    }.await;
    drop(receiver);let encoded=worker.await.map_err(error);pumped?;
    let(digest,chunks)=encoded?.map_err(error)?;
    let snapshot=SnapshotManifest{version:1,codec:"framed-cbor-1".into(),chunks,encoded_bytes:digest.encoded_bytes,
        sha256:digest.sha256.iter().map(|b|format!("{b:02x}")).collect(),snapshot_hash:hash,captured_at};
    db::seal(pool,job,&snapshot,observation_cutoff,limits).await.map_err(database)
}
struct Reader {receiver:mpsc::Receiver<Chunk>,current:Option<Chunk>,position:usize}
impl Read for Reader {
    fn read(&mut self,target:&mut[u8])->io::Result<usize> {
        if target.is_empty(){return Ok(0);}
        if self.current.is_none(){self.current=self.receiver.blocking_recv();self.position=0;}
        let Some(chunk)=&self.current else{return Ok(0);};
        let n=target.len().min(chunk.bytes.len()-self.position);target[..n].copy_from_slice(&chunk.bytes[self.position..self.position+n]);self.position+=n;
        if self.position==chunk.bytes.len(){if let Some(chunk)=self.current.take(){let _=chunk.ack.send(());}}
        Ok(n)
    }
}
pub(super) async fn load(pool:&PgPool,job:&Job,snapshot:&SnapshotManifest,max_decoded:usize,stop:&tokio_util::sync::CancellationToken)->Result<StudyInput,JobError> {
    let encoded=usize::try_from(snapshot.encoded_bytes).map_err(error)?;snapshot.validate(max_decoded.saturating_mul(3)).map_err(error)?;
    let codec=codec_limits(max_decoded,encoded);let(sender,receiver)=mpsc::channel::<Chunk>(1);
    let worker=tokio::task::spawn_blocking(move||core::read_frozen_input(&mut Reader{receiver,current:None,position:0},&codec));
    let pumped=async {
        for index in 0..snapshot.chunks {
            if stop.is_cancelled(){return Err(error("manual_shutdown_deferred"));}
            let bytes=db::read_chunk(pool,job,snapshot,index).await.map_err(database)?;let(ack,received)=oneshot::channel();
            sender.send(Chunk{index,bytes,ack}).await.map_err(error)?;received.await.map_err(error)?;
        }Ok::<_,JobError>(())
    }.await;
    drop(sender);let decoded=worker.await.map_err(error);pumped?;
    let(input,digest)=decoded?.map_err(error)?;
    let hash:String=digest.sha256.iter().map(|b|format!("{b:02x}")).collect();
    if digest.encoded_bytes!=snapshot.encoded_bytes || hash!=snapshot.sha256 || input.snapshot_hash!=snapshot.snapshot_hash || input.captured_at!=snapshot.captured_at {return Err(error("snapshot checksum/provenance mismatch"));}
    db::status(pool,job).await.map_err(database)?;Ok(input)
}
