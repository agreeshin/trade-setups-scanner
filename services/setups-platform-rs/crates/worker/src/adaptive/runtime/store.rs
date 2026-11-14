//! PostgreSQL boundary for adaptive scans. CPU/history ownership stays outside.
use super::*;
use setups_db::continuous::AdaptiveSetupWrite;
use crate::adaptive::state::PreparedProfile;
use sqlx::Row;

#[derive(Clone,Copy,Debug,PartialEq,Eq)]
pub(super) enum JoinDurable {Successor,Predecessor,Changed}
#[allow(async_fn_in_trait)]
pub(super) trait JoinStore {
    async fn attempt(&self)->Result<bool,WorkerError>;
    async fn reread(&self)->Result<JoinDurable,WorkerError>;
}
/// An uncertain join never unmutes RAM based only on generation identity.
pub(super) async fn join(store:&impl JoinStore)->Result<bool,WorkerError> {
    let mut last_error=None;
    for _ in 0..2 {
        match store.attempt().await {Ok(true)=>return Ok(true),Ok(false)=>{},Err(error)=>last_error=Some(error)}
        match store.reread().await? {
            JoinDurable::Successor=>return Ok(true),
            JoinDurable::Changed=>return Ok(false),
            JoinDurable::Predecessor=>{},
        }
    }
    last_error.map_or(Ok(false),Err)
}
pub(super) struct PgJoin<'a> {
    pub pool:&'a PgPool,
    pub slot:&'a ActiveAdaptiveSlot,
    pub instrument_key:&'a str,
    pub previous:Option<&'a continuous::AdaptiveCheckpoint>,
    pub frontier:i64,
    pub state:&'a Value,
}
impl JoinStore for PgJoin<'_> {
    async fn attempt(&self)->Result<bool,WorkerError> {
        Ok(live::join_waiting_adaptive_instrument(self.pool,self.slot,self.instrument_key,self.previous,self.frontier,self.state).await?)
    }
    async fn reread(&self)->Result<JoinDurable,WorkerError> {
        // One MVCC statement observes permission, generation, readiness/floor
        // and the checkpoint together. A later cohort must never be adopted.
        let row=sqlx::query("select s.active_generation_id,s.enabled,p.desired_running,p.config_revision,p.config,g.live_readiness,c.generation_id checkpoint_generation,c.state,exists(select 1 from settings where key='live' and value->'enabled'='true'::jsonb) permitted from continuous_slots s join continuous_processes p on p.owner_id=s.owner_id and p.algorithm_id=s.algorithm_id left join continuous_generations g on g.id=s.active_generation_id left join adaptive_live_checkpoints c on c.slot_id=s.id and c.instrument_key=$4 where s.id=$1 and s.owner_id=$2 and s.algorithm_id=$3")
            .bind(self.slot.slot_id).bind(&self.slot.owner_id).bind(&self.slot.algorithm_id).bind(self.instrument_key).fetch_optional(self.pool).await?;
        let Some(row)=row else {return Ok(JoinDurable::Changed);};
        if !row.try_get::<bool,_>("enabled")? || !row.try_get::<bool,_>("desired_running")? || !row.try_get::<bool,_>("permitted")?
            || row.try_get::<Option<i64>,_>("active_generation_id")?!=Some(self.slot.generation_id)
            || row.try_get::<i64,_>("config_revision")?!=self.slot.config_revision || row.try_get::<Value,_>("config")?["adaptiveEnabled"]!=true {
            return Ok(JoinDurable::Changed);
        }
        let readiness:Vec<live::LiveInstrumentReadiness>=serde_json::from_value(row.try_get("live_readiness")?)?;
        let Some(entry)=readiness.iter().find(|entry|entry.instrument_key==self.instrument_key) else {return Ok(JoinDurable::Changed);};
        let actual=row.try_get::<Option<i64>,_>("checkpoint_generation")?.map(|generation_id|
            Ok::<_,WorkerError>(continuous::AdaptiveCheckpoint {generation_id,state:row.try_get("state")?})).transpose()?;
        if entry.status=="ready" && entry.reason.is_none() && entry.publication_floor_close_unix==self.frontier
            && actual.as_ref().is_some_and(|state|state.generation_id==self.slot.generation_id && &state.state==self.state) {
            return Ok(JoinDurable::Successor);
        }
        Ok(if entry.status=="waiting" && entry.publication_floor_close_unix<=self.frontier && actual.as_ref()==self.previous {
            JoinDurable::Predecessor
        } else {JoinDurable::Changed})
    }
}

pub(super) async fn load_checkpoint(pool:&PgPool,slot:i64,instrument:&AdaptiveInstrument)->Result<Option<AdaptiveCheckpoint>,WorkerError> {
    continuous::get_adaptive_checkpoint(pool,slot,&instrument_key(instrument)).await?
        .map(|row| {
            let state:AdaptiveCheckpoint=serde_json::from_value(row.state)?;
            if state.generation_id!=row.generation_id {return Err(invalid("checkpoint generation differs from durable row"));}
            Ok(state)
        }).transpose()
}

/// An uncertain response is not evidence of failure. Resolve against the exact
/// durable predecessor/successor before adopting speculative detector progress.
pub(super) async fn persist<D:AdaptiveStore>(store:&D,instrument:&AdaptiveInstrument,prepared:&mut PreparedProfile)->Result<bool,WorkerError> {
    let mut last_error=None;
    for _ in 0..2 {
        match store.commit_page(&prepared.fence,instrument,prepared.expected.as_ref(),&prepared.checkpoint,&mut prepared.setups).await {
            Ok(Some(_))=>return Ok(true),
            Ok(None)=>{},
            Err(error)=>last_error=Some(error),
        }
        let durable=store.checkpoint(&prepared.fence,instrument).await?;
        if durable.as_ref()==Some(&prepared.checkpoint) && store.current(&prepared.fence).await? {return Ok(true);}
        if durable!=prepared.expected || !store.current(&prepared.fence).await? {return Ok(false);}
    }
    match last_error {Some(error)=>Err(error),None=>Ok(false)}
}


pub struct PgAdaptiveStore<'a> {
    pub pool: &'a PgPool,
    pub slot: &'a ActiveAdaptiveSlot,
}

impl AdaptiveStore for PgAdaptiveStore<'_> {
    async fn current(&self, _: &AdaptiveFence) -> Result<bool, WorkerError> {
        Ok(continuous::adaptive_notification_permitted(self.pool, self.slot).await?)
    }

    async fn checkpoint(
        &self,
        _: &AdaptiveFence,
        instrument: &AdaptiveInstrument,
    ) -> Result<Option<AdaptiveCheckpoint>, WorkerError> {
        load_checkpoint(self.pool, self.slot.slot_id, instrument).await
    }

    async fn commit_page(
        &self,
        _: &AdaptiveFence,
        instrument: &AdaptiveInstrument,
        expected: Option<&AdaptiveCheckpoint>,
        checkpoint: &AdaptiveCheckpoint,
        setups: &mut [AdaptiveSetup],
    ) -> Result<Option<Vec<AdaptiveInserted>>, WorkerError> {
        // Capture the editable name at persistence, not at tick start. The same
        // value belongs to the DB row, immutable snapshot and delivery payload.
        let mut captured = self.slot.clone();
        let settings =
            settings_get(self.pool, &format!("algorithm:{}", self.slot.algorithm_id)).await?;
        captured.external_name = settings
            .as_ref()
            .and_then(|value| value["externalName"].as_str())
            .map(str::to_owned)
            .unwrap_or_else(|| match self.slot.algorithm_id.as_str() {
                setups_domain::SPIKE_ALGORITHM_ID => "Всплеск цены и оборота".into(),
                setups_domain::SPIKE_DROP_ALGORITHM_ID => "Падение цены и всплеск оборота".into(),
                _ => self.slot.external_name.clone(),
            });
        if captured.external_name.trim().is_empty()
            || captured.external_name.chars().count() > 120
            || captured.external_name.len() > 480
            || captured.external_name.chars().any(char::is_control)
        {
            return Err(invalid("invalid external algorithm name at setup creation"));
        }
        for setup in setups.iter_mut() {
            setup.recommendation["externalName"] = captured.external_name.clone().into();
        }
        let writes: Vec<_> = setups
            .iter()
            .map(|setup| AdaptiveSetupWrite {
                exchange_code: instrument.exchange.clone(),
                category: instrument.category.clone(),
                symbol: instrument.symbol.clone(),
                setup: setup.raw.clone(),
                recommendation: setup.recommendation.clone(),
            })
            .collect();
        let expected = expected
            .map(|state| {
                Ok::<_, WorkerError>(continuous::AdaptiveCheckpoint {
                    generation_id: state.generation_id,
                    state: serde_json::to_value(state)?,
                })
            })
            .transpose()?;
        let commit = continuous::commit_adaptive_scan_cas(
            self.pool,
            &captured,
            &instrument_key(instrument),
            expected.as_ref(),
            &serde_json::to_value(checkpoint)?,
            &writes,
        )
        .await?;
        Ok(commit.applied.then_some(commit.inserted))
    }

    async fn notify(
        &self,
        _: &AdaptiveFence,
        _: i64,
        _: &AdaptiveInstrument,
        _: &AdaptiveSetup,
        _: i64,
    ) -> Result<(), WorkerError> {
        // commit_adaptive_scan_cas atomically registered durable delivery.
        // The independent executor owns transport; no local RAM notification queue.
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adaptive::state::tests::{fixture,feed};
    use std::sync::{Mutex,atomic::{AtomicUsize,Ordering}};

    #[derive(Clone,Copy)]
    enum Mode {Committed,Unchanged,Other,AlwaysFails,Unreadable}
    struct FakeStore {
        mode:Mode,
        durable:Mutex<Option<AdaptiveCheckpoint>>,
        calls:AtomicUsize,
        allowed:bool,
    }
    impl AdaptiveStore for FakeStore {
        async fn current(&self,_:&AdaptiveFence)->Result<bool,WorkerError> {Ok(self.allowed)}
        async fn checkpoint(&self,_:&AdaptiveFence,_:&AdaptiveInstrument)->Result<Option<AdaptiveCheckpoint>,WorkerError> {
            if matches!(self.mode,Mode::Unreadable) {return Err(invalid("simulated checkpoint read failure"));}
            Ok(self.durable.lock().unwrap().clone())
        }
        async fn commit_page(&self,_:&AdaptiveFence,_:&AdaptiveInstrument,_:Option<&AdaptiveCheckpoint>,checkpoint:&AdaptiveCheckpoint,_:&mut[AdaptiveSetup])
            ->Result<Option<Vec<AdaptiveInserted>>,WorkerError> {
            let call=self.calls.fetch_add(1,Ordering::SeqCst);
            if call==0 || matches!(self.mode,Mode::AlwaysFails) {
                match self.mode {
                    Mode::Committed=>*self.durable.lock().unwrap()=Some(checkpoint.clone()),
                    Mode::Other=>{let mut other=checkpoint.clone();other.next_open_unix+=60;*self.durable.lock().unwrap()=Some(other);},
                    _=>{},
                }
                return Err(invalid("simulated ambiguous commit response"));
            }
            *self.durable.lock().unwrap()=Some(checkpoint.clone());Ok(Some(Vec::new()))
        }
        async fn notify(&self,_:&AdaptiveFence,_:i64,_:&AdaptiveInstrument,_:&AdaptiveSetup,_:i64)->Result<(),WorkerError> {
            panic!("scan CAS/recovery must never perform transport; delivery is durable outbox work")
        }
    }
    fn next_page()->(crate::adaptive::state::tests::Fixture,PreparedProfile) {
        let mut f=fixture();f.runtime.install(&f.epoch,std::slice::from_ref(&f.spec)).unwrap();
        feed(&mut f.runtime,&f.epoch,&f.instrument,&f.bars,f.frontier).unwrap();
        let bar=DetectorBar {open_time_unix:f.frontier,close:"100".into(),quote_turnover:"100".into()};
        let prepared=f.runtime.prepare(&f.epoch,&f.instrument,&[bar],f.frontier+60,true).unwrap().remove(0);
        assert!(prepared.should_commit());assert_eq!(prepared.setups.len(),1);
        (f,prepared)
    }
    #[tokio::test]
    async fn ambiguous_commits_adopt_successor_retry_unchanged_and_discard_other_durable_state() {
        for (mode,applied,calls) in [(Mode::Committed,true,1),(Mode::Unchanged,true,2),(Mode::Other,false,1)] {
            let (mut f,mut prepared)=next_page();let successor=prepared.checkpoint.clone();
            let store=FakeStore {mode,durable:Mutex::new(prepared.expected.clone()),calls:AtomicUsize::new(0),allowed:true};
            assert_eq!(persist(&store,&f.instrument,&mut prepared).await.unwrap(),applied);
            assert_eq!(store.calls.load(Ordering::SeqCst),calls);
            if applied {
                f.runtime.accept(&f.epoch,prepared,true);
                assert_eq!(f.runtime.checkpoint_for(&f.epoch,&f.spec.key()),Some(successor));
            } else {
                f.runtime.invalidate(&f.epoch,None);assert_eq!(f.runtime.retained_history_bytes(),0);
            }
        }
    }
    #[tokio::test]
    async fn uncertain_recovery_remains_bounded_and_does_not_adopt_without_read_or_live_fence() {
        for (mode,allowed,calls) in [(Mode::AlwaysFails,true,2),(Mode::Unreadable,true,1),(Mode::Committed,false,1)] {
            let (f,mut prepared)=next_page();
            let store=FakeStore {mode,durable:Mutex::new(prepared.expected.clone()),calls:AtomicUsize::new(0),allowed};
            let result=persist(&store,&f.instrument,&mut prepared).await;
            if allowed {assert!(result.is_err());}else{assert!(!result.unwrap());}
            assert_eq!(store.calls.load(Ordering::SeqCst),calls);
            assert_eq!(f.runtime.cached_checkpoint(&f.epoch,&f.spec.key()),Some(prepared.expected));
        }
    }
}
