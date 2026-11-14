//! Promotion errors may occur after COMMIT. Resolve from a coherent durable
//! snapshot before deciding whether to adopt, retry, or rebuild RAM.
use super::*;
use sqlx::Row;

#[derive(Debug,Clone,Copy,PartialEq,Eq)]
pub(super) enum Outcome {Adopt,Rebuild,Retry,Lost}
#[derive(Debug,Clone,Copy,PartialEq,Eq)]
pub(super) enum Durable {Successor,Advanced,Predecessor,Conflict,Lost}
#[allow(async_fn_in_trait)]
pub(super) trait PromotionStore {
    async fn attempt(&self)->Result<bool,WorkerError>;
    async fn reread(&self)->Result<Durable,WorkerError>;
}
pub(super) async fn persist(store:&impl PromotionStore)->Result<Outcome,WorkerError> {
    let mut last_error=None;
    for _ in 0..2 {
        let attempt=tokio::time::timeout(Duration::from_secs(1),store.attempt()).await
            .unwrap_or_else(|_|Err(invalid("promotion CAS timed out; durable outcome requires reread")));
        match attempt {
            Ok(true)=>return Ok(Outcome::Adopt),
            Ok(false)=>{},
            Err(error)=>last_error=Some(error),
        }
        let durable=tokio::time::timeout(Duration::from_secs(1),store.reread()).await
            .map_err(|_|invalid("promotion durable reread timed out"))??;
        match durable {
            Durable::Successor=>return Ok(Outcome::Adopt),
            Durable::Advanced=>return Ok(Outcome::Rebuild),
            Durable::Conflict=>return Ok(Outcome::Retry),
            Durable::Lost=>return Ok(Outcome::Lost),
            Durable::Predecessor=>{},
        }
    }
    last_error.map_or(Ok(Outcome::Retry),Err)
}
pub(super) struct PgPromotion<'a> {
    pub pool:&'a PgPool,
    pub lease:&'a live::WarmupLease,
    pub predecessor:Option<i64>,
    pub expected:&'a[live::CheckpointExpectation],
    pub frontier:i64,
    pub initial:&'a[live::InitialAdaptiveCheckpoint],
    pub waiting:&'a[live::WaitingInstrument],
    pub retired:&'a[live::WaitingInstrument],
}
impl PromotionStore for PgPromotion<'_> {
    async fn attempt(&self)->Result<bool,WorkerError> {
        Ok(live::promote_pending_with_dispositions(self.pool,self.lease,self.predecessor,self.expected,self.frontier,self.initial,self.waiting,self.retired).await?)
    }
    async fn reread(&self)->Result<Durable,WorkerError> {
        let mut tx=self.pool.begin().await?;
        sqlx::query("set transaction isolation level repeatable read read only").execute(&mut *tx).await?;
        let slot=sqlx::query("select s.active_generation_id,s.pending_generation_id,s.enabled,p.desired_running,p.config_revision,p.config,g.live_readiness from continuous_slots s join continuous_processes p on p.owner_id=s.owner_id and p.algorithm_id=s.algorithm_id left join continuous_generations g on g.id=s.active_generation_id where s.id=$1 and s.owner_id=$2 and s.algorithm_id=$3")
            .bind(self.lease.slot_id).bind(&self.lease.owner_id).bind(&self.lease.algorithm_id).fetch_optional(&mut *tx).await?;
        let Some(slot)=slot else {return Ok(Durable::Lost);};
        if !slot.try_get::<bool,_>("enabled")? || !slot.try_get::<bool,_>("desired_running")?
            || slot.try_get::<i64,_>("config_revision")?!=self.lease.config_revision
            || slot.try_get::<Value,_>("config")?["adaptiveEnabled"]!=true {return Ok(Durable::Lost);}
        let active=slot.try_get::<Option<i64>,_>("active_generation_id")?;
        let pending=slot.try_get::<Option<i64>,_>("pending_generation_id")?;
        let readiness:Vec<live::LiveInstrumentReadiness>=slot.try_get::<Option<Value>,_>("live_readiness")?.map(serde_json::from_value).transpose()?.unwrap_or_default();
        let rows=sqlx::query("select instrument_key,generation_id,state from adaptive_live_checkpoints where slot_id=$1 order by instrument_key limit 2001")
            .bind(self.lease.slot_id).fetch_all(&mut *tx).await?;
        if rows.len()>2000 {return Err(invalid("promotion recovery checkpoint scope exceeds admission"));}
        let mut actual=BTreeMap::new();
        for row in rows {actual.insert(row.try_get::<String,_>("instrument_key")?,continuous::AdaptiveCheckpoint {
            generation_id:row.try_get("generation_id")?,state:row.try_get("state")?});}
        tx.commit().await?;
        if active==Some(self.lease.generation_id) {
            let exact=actual.len()==self.initial.len() && readiness.len()==self.initial.len()+self.waiting.len()
                && self.initial.iter().all(|entry|actual.get(&entry.instrument_key)
                .is_some_and(|row|row.generation_id==self.lease.generation_id && row.state==entry.state)
                && readiness.iter().any(|r|r.instrument_key==entry.instrument_key && r.status=="ready" && r.publication_floor_close_unix==self.frontier))
                && self.waiting.iter().all(|entry|readiness.iter().any(|r|r.instrument_key==entry.instrument_key
                    && r.status=="waiting" && r.reason.as_ref()==Some(&entry.reason) && r.publication_floor_close_unix==self.frontier));
            return Ok(if exact {Durable::Successor}else{Durable::Advanced});
        }
        if active!=self.predecessor {return Ok(Durable::Advanced);}
        if pending!=Some(self.lease.generation_id) {return Ok(Durable::Lost);}
        let exact=actual.len()==self.expected.iter().filter(|entry|entry.checkpoint.is_some()).count()
            && self.expected.iter().all(|entry|actual.get(&entry.instrument_key)==entry.checkpoint.as_ref());
        Ok(if exact {Durable::Predecessor}else{Durable::Conflict})
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize,Ordering};
    struct Fake {state:Durable,calls:AtomicUsize,retry_succeeds:bool}
    impl PromotionStore for Fake {
        async fn attempt(&self)->Result<bool,WorkerError> {
            if self.calls.fetch_add(1,Ordering::SeqCst)>0 && self.retry_succeeds {return Ok(true);}
            Err(invalid("simulated lost promotion response"))
        }
        async fn reread(&self)->Result<Durable,WorkerError> {Ok(self.state)}
    }
    #[tokio::test]
    async fn postgres_lost_successful_promotion_reply_is_recovered_from_coherent_successor() {
        use sqlx::Connection;
        let Ok(url)=std::env::var("TEST_DATABASE_URL") else {return;};
        let options:sqlx::postgres::PgConnectOptions=url.parse().unwrap();
        assert!(options.get_database().unwrap_or("").ends_with("_test"));
        let name=format!("adaptive_promotion_recovery_{}_test",std::process::id());
        let mut admin=sqlx::PgConnection::connect_with(&options).await.unwrap();
        sqlx::query(&format!("create database {name}")).execute(&mut admin).await.unwrap();
        let pool=sqlx::postgres::PgPoolOptions::new().max_connections(3).connect_with(options.database(&name)).await.unwrap();
        let baseline=include_str!("../../../../db/migrations/0010_baseline.sql").lines()
            .filter(|line|!line.starts_with('\\') && !line.starts_with("SET transaction_timeout")).collect::<Vec<_>>().join("\n");
        let mut connection=pool.acquire().await.unwrap();
        sqlx::raw_sql(&baseline).execute(&mut *connection).await.unwrap();
        sqlx::raw_sql("set search_path=public; create table kysely_migration(name varchar(255) primary key,timestamp varchar(255) not null); insert into kysely_migration values('0010_baseline.sql','fixture');")
            .execute(&mut *connection).await.unwrap();drop(connection);
        setups_db::migrate::ensure_migrations(&pool).await.unwrap();
        continuous::configure(&pool,"admin","price_volume_spike",&json!({"adaptiveEnabled":true}),&[0.0,1.0]).await.unwrap();
        continuous::start(&pool,"admin","price_volume_spike").await.unwrap();
        setups_db::settings_set(&pool,"live",&json!({"enabled":true})).await.unwrap();
        let slot:i64=sqlx::query_scalar("select min(id) from continuous_slots").fetch_one(&pool).await.unwrap();
        let mut generations=Vec::new();
        for _ in 0..2 {
            let generation:i64=sqlx::query_scalar("insert into continuous_generations(owner_id,algorithm_id,slot_id,config_revision,lease_token,status,phase,spec,result,accepted,execution_started_at,observation_cutoff) values('admin','price_volume_spike',$1,1,1,'completed','completed',$2,$3,true,now(),180) returning id")
                .bind(slot).bind(json!({"instruments":[{"exchange":"test","category":"linear","symbol":"TESTUSDT"},{"exchange":"test","category":"linear","symbol":"WAITUSDT"}]}))
                .bind(json!({"meanExpectancy":0.21,"selectedCell":{}})).fetch_one(&pool).await.unwrap();generations.push(generation);
        }
        let lease=live::WarmupLease {owner_id:"admin".into(),algorithm_id:"price_volume_spike".into(),slot_id:slot,
            generation_id:generations[1],config_revision:1,worker:"fixture".into(),token:42,
            initialization:live::LiveInitialization {version:1,anchor_close_unix:180,detector_from_open_unix:0,emit_from_close_unix:240},
            attempt:1,attempt_deadline_unix:chrono::Utc::now().timestamp()+3600};
        sqlx::query("update continuous_slots set active_generation_id=$2,pending_generation_id=$3,pending_status='ready',readiness_frontier=180 where id=$1")
            .bind(slot).bind(generations[0]).bind(generations[1]).execute(&pool).await.unwrap();
        sqlx::query("update adaptive_live_warmup_lane set slot_id=$1,generation_id=$2,lease_owner='fixture',lease_token=42,lease_until=now()+interval '30 seconds' where id=1")
            .bind(slot).bind(generations[1]).execute(&pool).await.unwrap();
        let state=|generation|json!({"generationId":generation,"nextOpenUnix":180,"lastRawOpenUnix":null,"lastRawClusterTime":null});
        let key="test/linear/TESTUSDT";
        sqlx::query("insert into adaptive_live_checkpoints(slot_id,instrument_key,generation_id,state) values($1,$2,$3,$4)")
            .bind(slot).bind(key).bind(generations[0]).bind(state(generations[0])).execute(&pool).await.unwrap();
        let expected=vec![live::CheckpointExpectation {instrument_key:key.into(),checkpoint:Some(continuous::AdaptiveCheckpoint {generation_id:generations[0],state:state(generations[0])})},
            live::CheckpointExpectation {instrument_key:"test/linear/WAITUSDT".into(),checkpoint:None}];
        let initial=vec![live::InitialAdaptiveCheckpoint {instrument_key:key.into(),state:state(generations[1])}];
        let waiting=vec![live::WaitingInstrument {instrument_key:"test/linear/WAITUSDT".into(),reason:"insufficient_history".into()}];
        let pg=PgPromotion {pool:&pool,lease:&lease,predecessor:Some(generations[0]),expected:&expected,frontier:180,initial:&initial,waiting:&waiting,retired:&[]};
        assert_eq!(pg.reread().await.unwrap(),Durable::Predecessor);
        struct LostReply<'a>(PgPromotion<'a>,AtomicUsize);
        impl PromotionStore for LostReply<'_> {
            async fn attempt(&self)->Result<bool,WorkerError> {
                self.1.fetch_add(1,Ordering::SeqCst);assert!(self.0.attempt().await?);
                Err(invalid("simulated lost reply AFTER real successful COMMIT"))
            }
            async fn reread(&self)->Result<Durable,WorkerError> {self.0.reread().await}
        }
        let store=LostReply(pg,AtomicUsize::new(0));
        assert_eq!(persist(&store).await.unwrap(),Outcome::Adopt);assert_eq!(store.1.load(Ordering::SeqCst),1);
        let profile=ActiveAdaptiveSlot {owner_id:"admin".into(),algorithm_id:"price_volume_spike".into(),slot_id:slot,
            generation_id:generations[1],study_id:0,config_revision:1,min_setups_per_day:0.0,spec:json!({}),result:json!({}),
            params:json!({}),analytics:json!({}),selected_cell:json!({}),mean_expectancy:0.21,direction:"short".into(),external_name:"fixture".into(),
            live_initialization:Some(lease.initialization.clone()),live_readiness:Vec::new()};
        let mut joined=state(generations[1]);joined["nextOpenUnix"]=240.into();
        let pg_join=super::super::store::PgJoin {pool:&pool,slot:&profile,instrument_key:"test/linear/WAITUSDT",previous:None,frontier:240,state:&joined};
        struct LostJoin<'a>(super::super::store::PgJoin<'a>,AtomicUsize);
        impl super::super::store::JoinStore for LostJoin<'_> {
            async fn attempt(&self)->Result<bool,WorkerError> {
                self.1.fetch_add(1,Ordering::SeqCst);assert!(self.0.attempt().await?);
                Err(invalid("lost reply AFTER actual waiting join COMMIT"))
            }
            async fn reread(&self)->Result<super::super::store::JoinDurable,WorkerError> {
                self.0.reread().await
            }
        }
        let lost_join=LostJoin(pg_join,AtomicUsize::new(0));
        assert!(super::super::store::join(&lost_join).await.unwrap());assert_eq!(lost_join.1.load(Ordering::SeqCst),1);
        // A later floor with unchanged generation/checkpoint is NOT the exact
        // successor and cannot silently unmute speculative RAM.
        sqlx::query("update continuous_generations set live_readiness=jsonb_set(live_readiness,'{1,publicationFloorCloseUnix}','300') where id=$1")
            .bind(generations[1]).execute(&pool).await.unwrap();
        use super::super::store::JoinStore;
        assert_eq!(lost_join.0.reread().await.unwrap(),super::super::store::JoinDurable::Changed);
        sqlx::query("update adaptive_live_checkpoints set state=jsonb_set(state,'{nextOpenUnix}','240'::jsonb) where slot_id=$1").bind(slot).execute(&pool).await.unwrap();
        assert_eq!(store.0.reread().await.unwrap(),Durable::Advanced);
        continuous::stop(&pool,"admin","price_volume_spike").await.unwrap();assert_eq!(store.0.reread().await.unwrap(),Durable::Lost);
        pool.close().await;sqlx::query(&format!("drop database {name} with (force)")).execute(&mut admin).await.unwrap();
    }

    #[tokio::test]
    async fn ambiguous_promotion_adopts_exact_successor_rebuilds_advanced_and_retries_unchanged() {
        for (state,retry_succeeds,want,calls) in [(Durable::Successor,false,Outcome::Adopt,1),
            (Durable::Advanced,false,Outcome::Rebuild,1),(Durable::Predecessor,true,Outcome::Adopt,2),
            (Durable::Conflict,false,Outcome::Retry,1),(Durable::Lost,false,Outcome::Lost,1)] {
            let store=Fake {state,calls:AtomicUsize::new(0),retry_succeeds};
            assert_eq!(persist(&store).await.unwrap(),want);assert_eq!(store.calls.load(Ordering::SeqCst),calls);
        }
        let store=Fake {state:Durable::Predecessor,calls:AtomicUsize::new(0),retry_succeeds:false};
        assert!(persist(&store).await.is_err());assert_eq!(store.calls.load(Ordering::SeqCst),2);
    }
}
