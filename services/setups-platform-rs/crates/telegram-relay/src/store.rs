//! Metadata only. Every SQLite operation finishes before any Telegram await.
//! A process-wide single connection has bounded admitted callers and no queue of
//! payloads. DELETE journal avoids an independently growing WAL file.
use crate::RelayError;
use rusqlite::{Connection, OptionalExtension, params};
use std::{
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

#[derive(Clone, Copy)]
pub(crate) struct StoreLimits {
    pub max_receipts: usize,
    pub max_bytes: usize,
    pub retention_seconds: i64,
    pub lease_seconds: i64,
}
#[derive(Clone)]
pub(crate) struct Store {
    connection: Arc<Mutex<Connection>>,
    slots: Arc<tokio::sync::Semaphore>,
    limits: StoreLimits,
}
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Claim {
    Acquired(i64),
    Duplicate(i64),
    Conflict,
    Busy(u32),
    Retry(&'static str, u32),
    Denied,
    Full,
    AttemptFull,
    Replay,
}
pub(crate) struct PostAttempt {
    pub nonce: String,
    pub signed_timestamp: i64,
}
pub(crate) enum Completion {
    Sent(i64),
    Retry {
        code: &'static str,
        after: u32,
        rate_limited: bool,
    },
    Denied,
}
impl Store {
    pub fn open(
        path: &Path,
        bot_id: i64,
        chat_id: i64,
        limits: StoreLimits,
    ) -> Result<Self, RelayError> {
        if limits.max_receipts == 0
            || limits.max_bytes < 1024 * 1024
            || limits.retention_seconds < 60
            || !(15..=30).contains(&limits.lease_seconds)
        {
            return Err(RelayError::Config("store_limits_invalid"));
        }
        let connection = Connection::open(path).map_err(|_| RelayError::Storage)?;
        connection
            .busy_timeout(Duration::from_millis(500))
            .map_err(|_| RelayError::Storage)?;
        connection.execute_batch("PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL; PRAGMA auto_vacuum=INCREMENTAL; PRAGMA trusted_schema=OFF; PRAGMA temp_store=MEMORY;").map_err(|_|RelayError::Storage)?;
        let pages = limits.max_bytes / 4096;
        let count: i64 = connection
            .query_row("PRAGMA page_count", [], |r| r.get(0))
            .map_err(|_| RelayError::Storage)?;
        let page_size: i64 = connection
            .query_row("PRAGMA page_size", [], |r| r.get(0))
            .map_err(|_| RelayError::Storage)?;
        if page_size != 4096 || count > pages as i64 {
            return Err(RelayError::Config("state_database_exceeds_budget"));
        }
        connection
            .pragma_update(None, "max_page_count", pages)
            .map_err(|_| RelayError::Storage)?;
        connection.execute_batch("CREATE TABLE IF NOT EXISTS relay_meta(singleton INTEGER PRIMARY KEY CHECK(singleton=1),version INTEGER NOT NULL,bot_id INTEGER NOT NULL,chat_id INTEGER NOT NULL);
            CREATE TABLE IF NOT EXISTS receipts(id TEXT PRIMARY KEY CHECK(length(id) BETWEEN 1 AND 128),body_hash TEXT NOT NULL CHECK(length(body_hash)=64),expires_at INTEGER NOT NULL,retain_until INTEGER NOT NULL,state INTEGER NOT NULL CHECK(state BETWEEN 0 AND 3),epoch INTEGER NOT NULL,lease_until INTEGER NOT NULL,retry_at INTEGER NOT NULL DEFAULT 0,telegram_id INTEGER);
            CREATE INDEX IF NOT EXISTS receipts_retention ON receipts(retain_until);
            CREATE TABLE IF NOT EXISTS post_attempts(nonce TEXT PRIMARY KEY CHECK(length(nonce)=32),id TEXT NOT NULL CHECK(length(id) BETWEEN 1 AND 128),body_hash TEXT NOT NULL CHECK(length(body_hash)=64),valid_until INTEGER NOT NULL);
            CREATE INDEX IF NOT EXISTS post_attempts_expiry ON post_attempts(valid_until);").map_err(|_|RelayError::Storage)?;
        let binding: Option<(i64, i64, i64)> = connection
            .query_row(
                "SELECT version,bot_id,chat_id FROM relay_meta WHERE singleton=1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()
            .map_err(|_| RelayError::Storage)?;
        match binding {
            None => {
                connection
                    .execute(
                        "INSERT INTO relay_meta VALUES(1,1,?1,?2)",
                        params![bot_id, chat_id],
                    )
                    .map_err(|_| RelayError::Storage)?;
            }
            Some((1, bot, chat)) if bot == bot_id && chat == chat_id => {}
            _ => return Err(RelayError::Config("state_destination_mismatch")),
        }
        Ok(Self {
            connection: Arc::new(Mutex::new(connection)),
            slots: Arc::new(tokio::sync::Semaphore::new(4)),
            limits,
        })
    }
    async fn operation<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Connection) -> Result<T, RelayError> + Send + 'static,
    ) -> Result<T, RelayError> {
        let connection = self.connection.clone();
        // The permit stays inside the blocking closure even if an HTTP client
        // disconnects and drops its awaiting future. No orphan blocking queue.
        let permit = self
            .slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| RelayError::Storage)?;
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let mut connection = connection.lock().map_err(|_| RelayError::Storage)?;
            f(&mut connection)
        })
        .await
        .map_err(|_| RelayError::Storage)?
    }
    pub async fn claim_attempt(
        &self,
        id: String,
        body_hash: String,
        expires_at: i64,
        now: i64,
        attempt: PostAttempt,
    ) -> Result<Claim, RelayError> {
        let limits = self.limits;
        self.operation(move|connection|{
            let tx=connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate).map_err(|_|RelayError::Storage)?;
            type Row=(String,i64,i64,i64,i64,Option<i64>);
            let row:Option<Row>=tx.query_row("SELECT body_hash,state,epoch,lease_until,retry_at,telegram_id FROM receipts WHERE id=?1",[&id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?))).optional().map_err(|_|RelayError::Storage)?;
            let seen:Option<(String,String)>=tx.query_row("SELECT id,body_hash FROM post_attempts WHERE nonce=?1",[&attempt.nonce],|r|Ok((r.get(0)?,r.get(1)?))).optional().map_err(|_|RelayError::Storage)?;
            if let Some((old_id,old_hash))=seen {
                if old_id!=id||old_hash!=body_hash{return Ok(Claim::Conflict);}
                return Ok(match row {
                    Some((hash,1,_,_,_,Some(telegram_id))) if hash==body_hash=>Claim::Duplicate(telegram_id),
                    Some((_,0,_,lease_until,_,_)) if lease_until>now=>Claim::Busy((lease_until-now).min(86400) as u32),
                    Some((_,3,_,_,_,_))=>Claim::Denied,
                    Some((_,state,_,_,retry_at,_)) if retry_at>now=>Claim::Retry(if state==2{"telegram_rate_limited"}else{"telegram_unavailable"},(retry_at-now).min(86400) as u32),
                    _=>Claim::Replay,
                });
            }
            // Record accepted POST attempts, including busy/conflict/backoff
            // responses. A captured old error must never become fresh admission.
            tx.execute("DELETE FROM post_attempts WHERE nonce IN(SELECT nonce FROM post_attempts WHERE valid_until < ?1 LIMIT 256)",[now]).map_err(|_|RelayError::Storage)?;
            let count:i64=tx.query_row("SELECT count(*) FROM post_attempts",[],|r|r.get(0)).map_err(|_|RelayError::Storage)?;
            if count>=limits.max_receipts.saturating_mul(4) as i64{return Ok(Claim::AttemptFull);}
            tx.execute("INSERT INTO post_attempts(nonce,id,body_hash,valid_until) VALUES(?1,?2,?3,?4)",params![attempt.nonce,id,body_hash,attempt.signed_timestamp.saturating_add(60)]).map_err(|_|RelayError::Storage)?;
            let claim=(||->Result<Claim,RelayError>{
                let epoch=if let Some((hash,state,epoch,lease_until,retry_at,telegram_id))=row {
                    if hash!=body_hash{return Ok(Claim::Conflict);}
                    if state==1{return telegram_id.map(Claim::Duplicate).ok_or(RelayError::Storage);}
                    if state==3{return Ok(Claim::Denied);}
                    if state==0&&lease_until>now{return Ok(Claim::Busy((lease_until-now) as u32));}
                    if retry_at>now{return Ok(Claim::Retry(if state==2{"telegram_rate_limited"}else{"telegram_unavailable"},(retry_at-now).min(86400) as u32));}
                    epoch.checked_add(1).ok_or(RelayError::Storage)?
                }else{
                    tx.execute("DELETE FROM receipts WHERE id IN(SELECT id FROM receipts WHERE retain_until < ?1 AND lease_until < ?1 LIMIT 256)",[now]).map_err(|_|RelayError::Storage)?;
                    let count:i64=tx.query_row("SELECT count(*) FROM receipts",[],|r|r.get(0)).map_err(|_|RelayError::Storage)?;
                    if count>=limits.max_receipts as i64{return Ok(Claim::Full);}1
                };
                tx.execute("INSERT INTO receipts(id,body_hash,expires_at,retain_until,state,epoch,lease_until,retry_at) VALUES(?1,?2,?3,?4,0,?5,?6,0)
                    ON CONFLICT(id) DO UPDATE SET state=0,epoch=excluded.epoch,lease_until=excluded.lease_until,retry_at=0",
                    params![id,body_hash,expires_at,expires_at.saturating_add(60).saturating_add(limits.retention_seconds),epoch,now.saturating_add(limits.lease_seconds)]).map_err(|_|RelayError::Storage)?;
                Ok(Claim::Acquired(epoch))
            })()?;
            tx.commit().map_err(|_|RelayError::Storage)?;Ok(claim)
        }).await
    }
    pub async fn finish(
        &self,
        id: String,
        epoch: i64,
        completion: Completion,
        now: i64,
    ) -> Result<(), RelayError> {
        self.operation(move|connection|{
            let (state,retry_at,telegram_id)=match completion {
                Completion::Sent(id)=>(1,0,Some(id)),
                Completion::Denied=>(3,0,None),
                Completion::Retry{code,after,rate_limited}=>{let _=code;(if rate_limited{2}else{0},now.saturating_add(after as i64),None)},
            };
            let changed=connection.execute("UPDATE receipts SET state=?3,retry_at=?4,telegram_id=?5,lease_until=0 WHERE id=?1 AND epoch=?2 AND state=0",params![id,epoch,state,retry_at,telegram_id]).map_err(|_|RelayError::Storage)?;
            if changed!=1{return Err(RelayError::StaleLease);}Ok(())
        }).await
    }
    pub async fn cleanup(&self, now: i64) -> Result<usize, RelayError> {
        self.operation(move|connection|{
            let deleted=connection.execute("DELETE FROM receipts WHERE id IN(SELECT id FROM receipts WHERE retain_until < ?1 AND lease_until < ?1 LIMIT 256)",[now]).map_err(|_|RelayError::Storage)?;
            let attempts=connection.execute("DELETE FROM post_attempts WHERE nonce IN(SELECT nonce FROM post_attempts WHERE valid_until < ?1 LIMIT 256)",[now]).map_err(|_|RelayError::Storage)?;
            connection.execute_batch("PRAGMA incremental_vacuum(256)").map_err(|_|RelayError::Storage)?;Ok(deleted+attempts)
        }).await
    }
    pub async fn health(&self) -> Result<(), RelayError> {
        self.operation(|connection| {
            connection
                .query_row("SELECT 1", [], |_| Ok(()))
                .map_err(|_| RelayError::Storage)
        })
        .await
    }
    #[cfg(test)]
    pub async fn claim(
        &self,
        id: String,
        hash: String,
        expires: i64,
        now: i64,
    ) -> Result<Claim, RelayError> {
        let nonce = setups_relay_protocol::body_sha256(format!("{id}/{hash}/{now}").as_bytes())
            [..32]
            .to_owned();
        self.claim_attempt(
            id,
            hash,
            expires,
            now,
            PostAttempt {
                nonce,
                signed_timestamp: now,
            },
        )
        .await
    }
    #[cfg(test)]
    pub async fn attempt_count(&self) -> i64 {
        self.operation(|c| {
            c.query_row("SELECT count(*) FROM post_attempts", [], |r| r.get(0))
                .map_err(|_| RelayError::Storage)
        })
        .await
        .unwrap()
    }
    #[cfg(test)]
    pub async fn disable_writes_for_test(&self) {
        self.operation(|c| {
            c.pragma_update(None, "query_only", true)
                .map_err(|_| RelayError::Storage)
        })
        .await
        .unwrap();
    }
    #[cfg(test)]
    pub fn limits_for_test(&self) -> StoreLimits {
        self.limits
    }
    #[cfg(test)]
    pub async fn stats(&self) -> (i64, i64, i64) {
        self.operation(|c| {
            Ok((
                c.query_row("SELECT count(*) FROM receipts", [], |r| r.get(0))
                    .unwrap(),
                c.query_row("PRAGMA page_count", [], |r| r.get(0)).unwrap(),
                c.query_row("PRAGMA freelist_count", [], |r| r.get(0))
                    .unwrap(),
            ))
        })
        .await
        .unwrap()
    }
}
