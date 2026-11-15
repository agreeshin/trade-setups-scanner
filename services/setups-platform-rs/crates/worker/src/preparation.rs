//! Selected cold sources only: proven empty, or nonempty with ABSENT target.
//! Existing enabled populated sources never enter a general gap-repair loop.
use setups_mdclient::{BackfillHistory, BackfillJob, BackfillStatus, HistoryDemand, MarketDataClient, MdClientError, TailBatchQuery};
use std::{collections::{BTreeMap, BTreeSet}, future::Future, time::{Duration, Instant}};
use tokio::task::{AbortHandle, JoinSet};

const MAX_ITEMS: usize = 1000;
const MAX_IN_FLIGHT: usize = 2;
const OPERATION_TIMEOUT: Duration = Duration::from_secs(5);
const RECHECK: Duration = Duration::from_secs(300);

pub(crate) trait Backend: Clone + Send + Sync + 'static {
    fn state(&self, id: i64) -> impl Future<Output=Result<Option<bool>, MdClientError>> + Send;
    fn collection(&self, id: i64) -> impl Future<Output=Result<bool, MdClientError>> + Send;
    fn missing(&self, demand: HistoryDemand) -> impl Future<Output=Result<i64, MdClientError>> + Send;
    fn jobs(&self, id: i64) -> impl Future<Output=Result<BackfillHistory, MdClientError>> + Send;
    fn admit(&self, demand: HistoryDemand) -> impl Future<Output=Result<BackfillJob, MdClientError>> + Send;
}
impl Backend for MarketDataClient {
    async fn state(&self,id:i64)->Result<Option<bool>,MdClientError> {Ok(self.collection_state(id).await?.map(|row|row.enabled))}
    async fn collection(&self, id:i64)->Result<bool,MdClientError> { Ok(self.ensure_collection_if_missing(id).await?.enabled) }
    async fn missing(&self, demand:HistoryDemand)->Result<i64,MdClientError> { Ok(self.preparation_coverage(demand).await?.missing_minutes) }
    async fn jobs(&self, id:i64)->Result<BackfillHistory,MdClientError> { self.preparation_jobs(id).await }
    async fn admit(&self, demand:HistoryDemand)->Result<BackfillJob,MdClientError> { Ok(self.admit_preparation(demand).await?.job) }
}
#[derive(Debug, Clone, PartialEq, Eq)]
enum Outcome { Covered, CollectionReady, NoWork, Disabled, DisabledJob(i64,BackfillStatus), Job(i64, BackfillStatus), Failed(&'static str) }
#[derive(Clone,Copy,PartialEq,Eq)]
enum DiscoveryReason { EmptySource, MissingTarget }
#[derive(Clone,Copy)]
pub(crate) struct ColdDiscovery {pub demand:HistoryDemand,pub source_empty:bool}
#[derive(Clone,Copy)]
enum Receipt { Covered, Job(i64) }
#[cfg(test)]
async fn execute<B:Backend>(backend:&B,demand:HistoryDemand)->Result<Outcome,MdClientError> {
    execute_for(backend,demand,DiscoveryReason::EmptySource).await
}
async fn execute_for<B:Backend>(backend:&B,demand:HistoryDemand,reason:DiscoveryReason)->Result<Outcome,MdClientError> {
    match backend.state(demand.instrument_id).await? {
        Some(false)=>return Ok(Outcome::Disabled),
        Some(true) if reason==DiscoveryReason::MissingTarget=>return Ok(Outcome::NoWork),
        _=>{},
    }
    let outcome=if backend.missing(demand).await?==0 {Outcome::Covered} else {
    let history=backend.jobs(demand.instrument_id).await?;
    // requestBackfill itself only deduplicates ACTIVE covering jobs. Reuse
    // persisted terminal evidence too, even if empty/no_trade cannot warm a
    // detector; never turn a terminal failure into another POST every tick.
    if let Some(job)=history.jobs.iter().find(|job|job.covers(demand) && job.status.active())
        .or_else(||history.jobs.iter().find(|job|job.covers(demand))) {
        Outcome::Job(job.id,job.status)
    } else {
        if history.next_before_id.is_some() {return Ok(Outcome::Failed("preparation_job_history_incomplete"));}
    // TODO: producer-owned persistent operation key is needed to close the
    // cross-instance race where another admission becomes terminal AFTER this
    // lookup but BEFORE POST. Active-only producer reuse cannot prove that.
        let job=backend.admit(demand).await?;
        Outcome::Job(job.id,job.status)
    }};
    // Admit/recover the historical job BEFORE creating a fresh collector. A
    // first fresh candle must not hide an unadmitted month on cold restart.
    // An ambiguous POST leaves collection untouched; persisted job evidence is
    // checked on retry. Conditional insert + actual-state reread preserves a
    // concurrent disable; already admitted additive history is not cancelled.
    if !backend.collection(demand.instrument_id).await? {
        return Ok(match outcome {Outcome::Job(id,status)=>Outcome::DisabledJob(id,status),_=>Outcome::Disabled});
    }
    Ok(outcome)
}
async fn refresh<B:Backend>(backend:&B,demand:HistoryDemand,receipt:Option<Receipt>,reason:DiscoveryReason)->Result<Outcome,MdClientError> {
    let Some(receipt)=receipt else {return execute_for(backend,demand,reason).await;};
    if !backend.collection(demand.instrument_id).await? {return Ok(Outcome::Disabled);}
    match receipt {
        Receipt::Covered=>Ok(Outcome::Covered),
        Receipt::Job(id)=>{
            let history=backend.jobs(demand.instrument_id).await?;
            Ok(history.jobs.into_iter().find(|job|job.id==id && job.covers(demand))
                .map_or(Outcome::Failed("accepted_backfill_status_unavailable"),|job|Outcome::Job(id,job.status)))
        }
    }
}

/// Extra fixed cold probe only. Catalog membership and an empty short range
/// alone do not prove absence of a stored series; stale/gapped series must NOT
/// become a general historical repair project.
pub(crate) async fn source_is_empty(client:&MarketDataClient,id:i64,closed:i64)->Result<bool,MdClientError> {
    let response=client.tail_batch(&TailBatchQuery {instrument_ids:vec![id],from_unix:closed-60,to_unix:closed}).await?;
    let item=response.instruments.first().ok_or_else(||MdClientError::Decode("cold source result missing".into()))?;
    if item.status!="known" {return Err(MdClientError::Decode("cold selected instrument not found".into()));}
    Ok(item.latest_stored_open_time_unix.is_none())
}
struct Entry {
    demand: HistoryDemand,
    // Adaptive's current whole-source cohort requirement, distinct from an
    // empty-source operation's frozen observation-time right edge.
    requirement: Option<HistoryDemand>,
    collection_only: bool,
    reason: DiscoveryReason,
    version: u64,
    next: Instant,
    failures: u8,
    outcome: Option<Outcome>,
    // Keep the admission receipt even across disabled/error observations. Once
    // source admission succeeded, this operation NEVER creates another job.
    receipt: Option<Receipt>,
    task: Option<AbortHandle>,
}
/// Owned by ONE live runtime. At most 1000 scalar entries, two tasks, no candle
/// payloads. Scope eviction/Stop/drop abort pending I/O; source jobs stay owned
/// by the producer (never cancelled just because a consumer disappears).
#[derive(Default)]
pub(crate) struct Preparation {
    entries: BTreeMap<i64,Entry>,
    tasks: JoinSet<(i64,u64,Outcome)>,
    version: u64,
    cursor: usize,
}
impl Preparation {
    pub fn clear(&mut self) { self.tasks=JoinSet::new(); self.entries.clear(); self.cursor=0; }
    pub fn retain_scope(&mut self, ids:&BTreeSet<i64>) {
        self.entries.retain(|id,entry| {
            let keep=ids.contains(id);
            if !keep { if let Some(task)=&entry.task { task.abort(); } }
            keep
        });
    }
    pub fn reconcile_requirements(&mut self,current:&BTreeMap<i64,HistoryDemand>) {
        self.entries.retain(|id,entry| {
            if entry.collection_only {
                if let Some(required)=current.get(id) {
                    // Registration depends on selected source identity, not on
                    // a departing profile's historical reconstruction bounds.
                    entry.requirement=Some(*required);entry.demand=*required;
                    return true;
                }
            }
            let keep=current.get(id).is_some_and(|required|
                entry.demand.from_unix==required.from_unix && entry.requirement.is_none_or(|old|old==*required));
            if !keep {if let Some(task)=&entry.task {task.abort();}}
            keep
        });
    }
    pub fn observe_required_cold(&mut self,required:HistoryDemand,source_empty:bool,ready:bool,target:i64)->Result<(),&'static str> {
        let demand=if source_empty {HistoryDemand {to_unix:target,..required}} else {required};
        self.observe_cold(demand,source_empty,ready)?;
        if let Some(entry)=self.entries.get_mut(&required.instrument_id) {entry.requirement=Some(required);}
        Ok(())
    }
    /// Call only after a saved read proved the source has NO stored candles.
    /// Freeze the right edge on first admission; normal collection owns future
    /// closes. Never resubmit a full month just because wall clock advanced.
    pub fn observe_empty(&mut self,demand:HistoryDemand)->Result<(),&'static str> {
        self.observe(demand,DiscoveryReason::EmptySource)
    }
    fn observe(&mut self,mut demand:HistoryDemand,reason:DiscoveryReason)->Result<(),&'static str> {
        demand.validate().map_err(|_|"invalid cold preparation range")?;
        let mut receipt=None;
        if let Some(entry)=self.entries.get(&demand.instrument_id).filter(|entry|entry.demand.from_unix==demand.from_unix) {
            if entry.reason==DiscoveryReason::EmptySource {return Ok(());}
            if reason==DiscoveryReason::MissingTarget {
                // This end is an immutable initialization/checkpoint boundary,
                // not observation time. A larger admitted cold requirement is
                // a new range; shorter sibling observations must not oscillate
                // and cancel its in-flight recovery on every pass.
                if demand.to_unix<=entry.demand.to_unix {return Ok(());}
            } else {
            // A later proven-empty observation can upgrade a parked nonempty
            // discovery. Preserve this operation's frozen end and receipt.
                demand=entry.demand;receipt=entry.receipt;
            }
        }
        if self.entries.len()>=MAX_ITEMS && !self.entries.contains_key(&demand.instrument_id) { return Err("cold preparation item budget exceeded"); }
        if let Some(entry)=self.entries.remove(&demand.instrument_id) { if let Some(task)=entry.task {task.abort();} }
        self.version=self.version.checked_add(1).ok_or("cold preparation version overflow")?;
        self.entries.insert(demand.instrument_id,Entry {demand,requirement:None,collection_only:false,reason,version:self.version,next:Instant::now(),failures:0,outcome:None,receipt,task:None});
        Ok(())
    }
    pub fn observe_cold(&mut self,demand:HistoryDemand,source_empty:bool,ready:bool)->Result<(),&'static str> {
        if ready {
            return self.retire_ready(demand.instrument_id);
        }
        if source_empty {self.observe_empty(demand)} else {self.observe(demand,DiscoveryReason::MissingTarget)}
    }
    pub fn retire_ready(&mut self,id:i64)->Result<(),&'static str> {
        // Readiness forbids late history admission, but must not orphan an
        // admitted job whose first candles arrived before collection insertion.
        // Cancel the old future and fence its result; continue COLLECTION ONLY.
        // A ready source with no existing cold operation never creates one.
        if self.entries.get(&id).is_some_and(|entry|entry.reason==DiscoveryReason::MissingTarget && !entry.collection_only) {
            self.version=self.version.checked_add(1).ok_or("cold preparation version overflow")?;
            let entry=self.entries.get_mut(&id).unwrap();
            if let Some(task)=entry.task.take() {task.abort();}
            entry.version=self.version;entry.collection_only=true;
            entry.next=Instant::now();entry.outcome=None;
        }
        Ok(())
    }
    /// Nonblocking: harvest completed I/O and start at most two operations.
    /// Call after ready/hot work. No HTTP future is awaited by the hot owner.
    pub fn poll<B:Backend>(&mut self,backend:&B,stopped:bool) {
        if stopped {self.clear();return;}
        while let Some(result)=self.tasks.try_join_next() {
            let (id,version,outcome)=match result {
                Ok(result)=>result,
                Err(error)=>{
                    // Removed/replaced scopes abort their tasks normally. A
                    // panic for a STILL selected operation must release its
                    // slot and become a visible, retryable failure.
                    let Some((&id,entry))=self.entries.iter().find(|(_,entry)|entry.task.as_ref().is_some_and(|task|task.id()==error.id())) else {continue;};
                    (id,entry.version,Outcome::Failed("preparation_task_failed"))
                }
            };
            let Some(entry)=self.entries.get_mut(&id).filter(|entry|entry.version==version) else {continue;};
            entry.task=None;
            let failed=matches!(outcome,Outcome::Failed(_));
            entry.failures=if failed {entry.failures.saturating_add(1)} else {0};
            let delay=if failed {Duration::from_secs((5u64 << entry.failures.min(8)).min(900))} else {RECHECK};
            entry.next=Instant::now()+delay;
            match outcome {Outcome::Covered=>entry.receipt=Some(Receipt::Covered),Outcome::Job(id,_)|Outcome::DisabledJob(id,_)=>entry.receipt=Some(Receipt::Job(id)),_=>{}}
            if entry.outcome.as_ref()!=Some(&outcome) {
                eprintln!("[live-preparation] instrument_id={id} outcome={outcome:?}");
            }
            entry.outcome=Some(outcome);
        }
        let ids:Vec<_>=self.entries.keys().copied().collect();
        for _ in 0..ids.len() {
            if self.tasks.len()>=MAX_IN_FLIGHT {break;}
            let id=ids[self.cursor%ids.len()]; self.cursor=(self.cursor+1)%ids.len();
            let entry=self.entries.get_mut(&id).unwrap();
            if entry.task.is_some() || entry.next>Instant::now() || entry.outcome==Some(Outcome::NoWork) {continue;}
            let backend=backend.clone(); let demand=entry.demand; let version=entry.version; let receipt=entry.receipt; let reason=entry.reason; let collection_only=entry.collection_only;
            entry.task=Some(self.tasks.spawn(async move {
                let operation=async {
                    if collection_only {
                        return backend.collection(demand.instrument_id).await.map(|enabled|
                            if enabled {Outcome::CollectionReady} else {Outcome::Disabled});
                    }
                    refresh(&backend,demand,receipt,reason).await
                };
                let outcome=match tokio::time::timeout(OPERATION_TIMEOUT,operation).await {
                    Ok(Ok(outcome))=>outcome,
                    Ok(Err(error))=>Outcome::Failed(match error {MdClientError::Http {status:429,..}=>"admission_resource_limit",MdClientError::Http {..}=>"preparation_http_failed",MdClientError::Network(_)=>"preparation_transport_failed",MdClientError::Decode(code) if code=="conditional_collection_unsupported"=>"conditional_collection_unsupported",MdClientError::Decode(_)=>"preparation_invalid_response"}),
                    Err(_)=>Outcome::Failed("preparation_timeout"),
                };
                (id,version,outcome)
            }));
        }
    }
    pub fn status(&self)->serde_json::Value {
        let problems:Vec<_>=self.entries.iter().filter_map(|(id,entry)|match &entry.outcome {
            Some(Outcome::Disabled | Outcome::DisabledJob(..))=>Some(serde_json::json!({"instrumentId":id,"code":"collection_disabled"})),
            Some(Outcome::Failed(code))=>Some(serde_json::json!({"instrumentId":id,"code":code})),
            Some(Outcome::Job(job,BackfillStatus::Failed | BackfillStatus::Cancelled))=>Some(serde_json::json!({"instrumentId":id,"code":"backfill_terminal_failure","jobId":job})),
            _=>None,
        }).take(10).collect();
        serde_json::json!({"tracked":self.entries.len(),"inFlight":self.tasks.len(),
            "pending":self.entries.values().filter(|entry|entry.outcome.is_none()).count(),
            "collectionOnly":self.entries.values().filter(|entry|entry.collection_only).count(),
            "noWork":self.entries.values().filter(|entry|entry.outcome==Some(Outcome::NoWork)).count(),"problems":problems})
    }
    #[cfg(test)]
    pub fn operation_snapshot(&self,id:i64)->Option<(u64,HistoryDemand,Option<tokio::task::Id>,bool)> {
        self.entries.get(&id).map(|entry|(entry.version,entry.demand,entry.task.as_ref().map(AbortHandle::id),entry.receipt.is_some()))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::{Arc,Mutex,atomic::{AtomicUsize,Ordering}};
    #[derive(Clone,Default)]
    pub(crate) struct Fake {
        calls:Arc<Mutex<Vec<String>>>, disabled:bool, jobs:Arc<Mutex<Vec<BackfillJob>>>,
        covered:bool, incomplete:bool, fail:Option<i64>, panic:Option<i64>, block:bool,
        dropped:Arc<AtomicUsize>, fail_after_enable:bool, disable_after_admit:bool, legacy:bool,
        existing:Option<bool>, fail_before_enable:bool, block_state:bool,
        state_gate:Option<Arc<tokio::sync::Notify>>,
        collection_gate:Option<Arc<tokio::sync::Notify>>, registered:Arc<AtomicUsize>,
    }
    impl Fake {
        pub fn gated()->(Self,Arc<tokio::sync::Notify>) {
            let gate=Arc::new(tokio::sync::Notify::new());
            (Self {state_gate:Some(gate.clone()),..Default::default()},gate)
        }
        pub fn collection_gated()->(Self,Arc<tokio::sync::Notify>) {
            let gate=Arc::new(tokio::sync::Notify::new());
            (Self {collection_gate:Some(gate.clone()),..Default::default()},gate)
        }
        pub fn call_count(&self,prefix:&str)->usize {self.calls.lock().unwrap().iter().filter(|call|call.starts_with(prefix)).count()}
    }
    struct Dropped(Arc<AtomicUsize>);
    impl Drop for Dropped {fn drop(&mut self){self.0.fetch_add(1,Ordering::SeqCst);}}
    impl Backend for Fake {
        async fn state(&self,id:i64)->Result<Option<bool>,MdClientError> {
            self.calls.lock().unwrap().push(format!("state:{id}"));
            if self.legacy {return Err(MdClientError::Decode("conditional_collection_unsupported".into()));}
            if self.fail==Some(id) {return Err(MdClientError::Network("synthetic".into()));}
            if self.block_state {let _drop=Dropped(self.dropped.clone());std::future::pending::<()>().await;}
            if let Some(gate)=&self.state_gate {gate.notified().await;}
            Ok(if self.disabled {Some(false)} else {self.existing})
        }
        async fn collection(&self,id:i64)->Result<bool,MdClientError> {
            self.calls.lock().unwrap().push(format!("target:{id}"));
            if self.fail_before_enable {return Err(MdClientError::Network("synthetic interruption before target insert".into()));}
            if self.panic==Some(id) {panic!("synthetic preparation panic");}
            if self.fail==Some(id) {return Err(MdClientError::Network("synthetic".into()));}
            if self.block {let _drop=Dropped(self.dropped.clone());std::future::pending::<()>().await;}
            if let Some(gate)=&self.collection_gate {let _drop=Dropped(self.dropped.clone());gate.notified().await;}
            if self.fail_after_enable {return Err(MdClientError::Network("synthetic interruption after enable".into()));}
            if !self.disabled && !self.disable_after_admit {self.registered.fetch_add(1,Ordering::SeqCst);}
            Ok(!self.disabled && !self.disable_after_admit)
        }
        async fn missing(&self,_:HistoryDemand)->Result<i64,MdClientError> {self.calls.lock().unwrap().push("coverage".into());Ok(if self.covered {0}else{2})}
        async fn jobs(&self,id:i64)->Result<BackfillHistory,MdClientError> {self.calls.lock().unwrap().push("jobs".into());Ok(BackfillHistory {jobs:self.jobs.lock().unwrap().iter().filter(|job|job.instrument_id==id).cloned().collect(),next_before_id:self.incomplete.then_some(1)})}
        async fn admit(&self,d:HistoryDemand)->Result<BackfillJob,MdClientError> {
            self.calls.lock().unwrap().push(format!("post:{}:{}:{}",d.instrument_id,d.from_unix,d.to_unix));
            let job=BackfillJob {id:42,instrument_id:d.instrument_id,from_unix:d.from_unix,to_unix:d.to_unix,status:BackfillStatus::Queued};
            self.jobs.lock().unwrap().push(job.clone());Ok(job)
        }
    }
    fn demand()->HistoryDemand {HistoryDemand {instrument_id:820,from_unix:120,to_unix:240}}
    #[tokio::test]
    async fn cold_selected_exact_range_is_admitted_and_disabled_is_preserved() {
        let backend=Fake::default();
        assert_eq!(execute(&backend,demand()).await.unwrap(),Outcome::Job(42,BackfillStatus::Queued));
        assert_eq!(*backend.calls.lock().unwrap(),vec!["state:820","coverage","jobs","post:820:120:240","target:820"]);
        let disabled=Fake {disabled:true,..Default::default()};
        assert_eq!(execute(&disabled,demand()).await.unwrap(),Outcome::Disabled);
        assert_eq!(*disabled.calls.lock().unwrap(),vec!["state:820"]);
    }
    pub(crate) async fn settle(preparation:&mut Preparation,backend:&Fake) {
        for _ in 0..20 {tokio::task::yield_now().await;preparation.poll(backend,false);}
    }
    pub(crate) fn posts(backend:&Fake)->usize {backend.calls.lock().unwrap().iter().filter(|call|call.starts_with("post:")).count()}
    #[tokio::test]
    async fn ticks_reconnect_and_terminal_job_evidence_do_not_multiply_posts() {
        let backend=Fake::default(); let mut preparation=Preparation::default();
        preparation.observe_empty(demand()).unwrap();preparation.poll(&backend,false);settle(&mut preparation,&backend).await;
        for _ in 0..1000 {preparation.observe_empty(HistoryDemand {to_unix:300,..demand()}).unwrap();preparation.poll(&backend,false);}
        assert_eq!(posts(&backend),1);
        assert_eq!(preparation.entries[&820].demand.to_unix,240,"freeze the operation, not wall-clock ticks");
        for status in [BackfillStatus::Done,BackfillStatus::Failed,BackfillStatus::Cancelled] {
            backend.jobs.lock().unwrap()[0].status=status;
            let mut restarted=Preparation::default();restarted.observe_empty(demand()).unwrap();restarted.poll(&backend,false);settle(&mut restarted,&backend).await;
            assert_eq!(restarted.entries[&820].outcome,Some(Outcome::Job(42,status)));
        }
        assert_eq!(posts(&backend),1);
        assert!(!backend.calls.lock().unwrap().iter().any(|call|call.contains("821")));
    }
    #[tokio::test]
    async fn coverage_and_incomplete_history_do_not_create_empty_job_storms() {
        let covered=Fake {covered:true,..Default::default()};
        assert_eq!(execute(&covered,demand()).await.unwrap(),Outcome::Covered);
        assert_eq!(posts(&covered),0);
        let incomplete=Fake {incomplete:true,..Default::default()};
        assert_eq!(execute(&incomplete,demand()).await.unwrap(),Outcome::Failed("preparation_job_history_incomplete"));
        assert_eq!(posts(&incomplete),0);
        let overlapping=Fake::default();
        overlapping.jobs.lock().unwrap().push(BackfillJob {id:9,instrument_id:820,from_unix:180,to_unix:240,status:BackfillStatus::Done});
        assert_eq!(execute(&overlapping,demand()).await.unwrap(),Outcome::Job(42,BackfillStatus::Queued));
        assert_eq!(posts(&overlapping),1,"terminal [180,240) must not veto required missing [120,180)");
        assert_eq!(execute(&overlapping,demand()).await.unwrap(),Outcome::Job(42,BackfillStatus::Queued));
        assert_eq!(posts(&overlapping),1);
    }
    #[tokio::test]
    async fn failure_is_visible_and_does_not_block_other_selected_work_or_ignore_disable() {
        let backend=Fake {fail:Some(820),..Default::default()};let mut preparation=Preparation::default();
        preparation.observe_empty(demand()).unwrap();preparation.observe_empty(HistoryDemand {instrument_id:821,..demand()}).unwrap();
        preparation.poll(&backend,false);settle(&mut preparation,&backend).await;
        assert_eq!(preparation.entries[&820].outcome,Some(Outcome::Failed("preparation_transport_failed")));
        assert_eq!(preparation.entries[&821].outcome,Some(Outcome::Job(42,BackfillStatus::Queued)));
        assert_eq!(preparation.status()["problems"][0]["instrumentId"],820);
        let disabled=Fake {disabled:true,..backend.clone()};
        preparation.entries.get_mut(&821).unwrap().next=Instant::now();preparation.poll(&disabled,false);settle(&mut preparation,&disabled).await;
        assert_eq!(preparation.entries[&821].outcome,Some(Outcome::Disabled));
        assert_eq!(posts(&backend),1);
    }
    #[tokio::test]
    async fn task_panic_releases_entry_reports_failure_and_can_retry() {
        let backend=Fake {panic:Some(820),..Default::default()};let mut preparation=Preparation::default();
        preparation.observe_empty(demand()).unwrap();preparation.poll(&backend,false);settle(&mut preparation,&backend).await;
        let entry=&preparation.entries[&820];
        assert!(entry.task.is_none());assert_eq!(entry.outcome,Some(Outcome::Failed("preparation_task_failed")));assert!(entry.next>Instant::now());
        preparation.entries.get_mut(&820).unwrap().next=Instant::now();
        let recovered=Fake {panic:None,..backend};preparation.poll(&recovered,false);settle(&mut preparation,&recovered).await;
        assert_eq!(posts(&recovered),1);
    }
    #[tokio::test]
    async fn concurrency_item_budget_scope_eviction_and_stop_release_resources() {
        let backend=Fake {block:true,..Default::default()};let mut preparation=Preparation::default();
        for id in 1..=1000 {preparation.observe_empty(HistoryDemand {instrument_id:id,..demand()}).unwrap();}
        assert!(preparation.observe_empty(HistoryDemand {instrument_id:1001,..demand()}).is_err());
        preparation.poll(&backend,false);tokio::task::yield_now().await;
        assert_eq!(preparation.tasks.len(),2);
        assert_eq!(backend.calls.lock().unwrap().iter().filter(|call|call.starts_with("target:")).count(),2);
        preparation.retain_scope(&BTreeSet::from([3]));settle(&mut preparation,&backend).await;
        assert_eq!(preparation.entries.len(),1);assert_eq!(backend.dropped.load(Ordering::SeqCst),2);
        preparation.poll(&backend,true);tokio::task::yield_now().await;
        assert!(preparation.entries.is_empty());assert!(preparation.tasks.is_empty());
        assert_eq!(backend.dropped.load(Ordering::SeqCst),3);
        for _ in 0..100 {preparation.observe_empty(demand()).unwrap();preparation.clear();}
        assert!(preparation.entries.is_empty());assert!(preparation.tasks.is_empty());
    }
    #[tokio::test]
    async fn changed_origin_replaces_only_its_operation_and_releases_stale_task() {
        let backend=Fake {block:true,..Default::default()};let mut preparation=Preparation::default();
        preparation.observe_empty(demand()).unwrap();preparation.poll(&backend,false);tokio::task::yield_now().await;
        preparation.observe_empty(HistoryDemand {from_unix:60,..demand()}).unwrap();
        settle(&mut preparation,&backend).await;
        assert_eq!(backend.dropped.load(Ordering::SeqCst),1);
        assert_eq!(preparation.entries[&820].demand.from_unix,60);
        assert_eq!(preparation.entries[&820].outcome,None,"expected stale abort is not an error on the replacement");
        preparation.clear();tokio::task::yield_now().await;
    }
    #[tokio::test]
    async fn accepted_receipt_survives_disable_and_missing_job_history_without_repairing_again() {
        let backend=Fake::default();let mut preparation=Preparation::default();
        preparation.observe_empty(demand()).unwrap();preparation.poll(&backend,false);settle(&mut preparation,&backend).await;
        preparation.entries.get_mut(&820).unwrap().next=Instant::now();
        let disabled=Fake {disabled:true,..backend.clone()};preparation.poll(&disabled,false);settle(&mut preparation,&disabled).await;
        assert_eq!(preparation.entries[&820].outcome,Some(Outcome::Disabled));
        backend.jobs.lock().unwrap().clear();
        preparation.entries.get_mut(&820).unwrap().next=Instant::now();preparation.poll(&backend,false);settle(&mut preparation,&backend).await;
        assert_eq!(preparation.entries[&820].outcome,Some(Outcome::Failed("accepted_backfill_status_unavailable")));
        assert_eq!(posts(&backend),1,"no new repair after the initial cold preparation, even if the receipt's source job was retired");
    }
    #[tokio::test]
    async fn interruption_after_fresh_enable_already_has_persisted_historical_job() {
        let interrupted=Fake {fail_after_enable:true,..Default::default()};
        assert!(execute(&interrupted,demand()).await.is_err());
        assert_eq!(interrupted.jobs.lock().unwrap().len(),1);
        assert!(interrupted.jobs.lock().unwrap()[0].covers(demand()));
        let recovered=Fake {fail_after_enable:false,..interrupted};
        assert_eq!(execute(&recovered,demand()).await.unwrap(),Outcome::Job(42,BackfillStatus::Queued));
        assert_eq!(posts(&recovered),1);
        let calls=recovered.calls.lock().unwrap();
        assert!(calls.iter().position(|call|call.starts_with("post:")).unwrap()<calls.iter().position(|call|call.starts_with("target:")).unwrap());
    }
    #[tokio::test]
    async fn concurrent_disable_after_admission_does_not_cancel_shared_history_or_report_enabled() {
        let backend=Fake {disable_after_admit:true,..Default::default()};
        assert_eq!(execute(&backend,demand()).await.unwrap(),Outcome::DisabledJob(42,BackfillStatus::Queued));
        assert_eq!(posts(&backend),1);assert_eq!(backend.jobs.lock().unwrap().len(),1);
    }
    #[tokio::test]
    async fn legacy_capability_preflight_is_actionable_and_performs_no_put_or_post() {
        let backend=Fake {legacy:true,..Default::default()};let mut preparation=Preparation::default();
        preparation.observe_empty(demand()).unwrap();preparation.poll(&backend,false);settle(&mut preparation,&backend).await;
        assert_eq!(preparation.entries[&820].outcome,Some(Outcome::Failed("conditional_collection_unsupported")));
        assert_eq!(*backend.calls.lock().unwrap(),vec!["state:820"]);
        assert_eq!(posts(&backend),0);
    }
    #[tokio::test]
    async fn nonempty_cold_restart_recovers_admitted_history_before_target_insert_without_duplicate_post() {
        let interrupted=Fake {fail_before_enable:true,..Default::default()};
        assert!(execute(&interrupted,demand()).await.is_err());
        assert_eq!(posts(&interrupted),1);
        // Producer persists the job and its first candle. Consumer RAM is lost
        // before collection insertion; the reconstructed scope is NONEMPTY but
        // still cold with the same immutable required origin/range.
        let restarted=Fake {fail_before_enable:false,..interrupted};
        let mut preparation=Preparation::default();
        preparation.observe_cold(demand(),false,false).unwrap();
        preparation.poll(&restarted,false);settle(&mut preparation,&restarted).await;
        assert_eq!(restarted.calls.lock().unwrap().iter().filter(|call|call.starts_with("target:")).count(),2);
        assert_eq!(posts(&restarted),1);
        assert_eq!(preparation.entries[&820].outcome,Some(Outcome::Job(42,BackfillStatus::Queued)));
    }
    #[tokio::test]
    async fn nonempty_existing_target_true_is_read_only_no_work_and_not_periodic_repair() {
        let backend=Fake {existing:Some(true),..Default::default()};let mut preparation=Preparation::default();
        preparation.observe_cold(demand(),false,false).unwrap();preparation.poll(&backend,false);settle(&mut preparation,&backend).await;
        assert_eq!(preparation.entries[&820].outcome,Some(Outcome::NoWork));
        for _ in 0..1000 {
            preparation.entries.get_mut(&820).unwrap().next=Instant::now();
            preparation.observe_cold(demand(),false,false).unwrap();
            preparation.poll(&backend,false);
        }
        assert_eq!(*backend.calls.lock().unwrap(),vec!["state:820"]);
        assert_eq!(preparation.status()["noWork"],1);
        assert_eq!(posts(&backend),0);
    }
    #[tokio::test]
    async fn nonempty_existing_disabled_and_unsupported_targets_never_write() {
        for backend in [Fake {existing:Some(false),..Default::default()},Fake {legacy:true,..Default::default()}] {
            let mut preparation=Preparation::default();preparation.observe_cold(demand(),false,false).unwrap();
            preparation.poll(&backend,false);settle(&mut preparation,&backend).await;
            assert_eq!(*backend.calls.lock().unwrap(),vec!["state:820"]);
            assert_eq!(posts(&backend),0);
            assert_eq!(preparation.entries[&820].outcome,Some(if backend.legacy {Outcome::Failed("conditional_collection_unsupported")} else {Outcome::Disabled}));
        }
    }
    #[tokio::test]
    async fn initially_nonempty_cold_missing_target_admits_only_its_frozen_range() {
        let backend=Fake::default();let mut preparation=Preparation::default();
        preparation.observe_cold(demand(),false,false).unwrap();preparation.poll(&backend,false);settle(&mut preparation,&backend).await;
        assert_eq!(*backend.calls.lock().unwrap(),vec!["state:820","coverage","jobs","post:820:120:240","target:820"]);
        for _ in 0..10 {preparation.observe_cold(demand(),false,false).unwrap();preparation.poll(&backend,false);}
        assert_eq!(preparation.entries[&820].demand,demand());assert_eq!(posts(&backend),1);
    }
    #[tokio::test]
    async fn ready_sources_are_not_discovered_and_existing_probe_becomes_collection_only() {
        let backend=Fake {block_state:true,..Default::default()};let mut preparation=Preparation::default();
        preparation.observe_cold(demand(),false,true).unwrap();preparation.poll(&backend,false);
        assert!(preparation.entries.is_empty());assert!(backend.calls.lock().unwrap().is_empty());
        preparation.observe_cold(demand(),false,false).unwrap();preparation.poll(&backend,false);tokio::task::yield_now().await;
        preparation.observe_cold(demand(),false,true).unwrap();settle(&mut preparation,&backend).await;
        assert!(preparation.entries[&820].collection_only);assert!(preparation.tasks.is_empty());
        assert_eq!(preparation.entries[&820].outcome,Some(Outcome::CollectionReady));
        assert_eq!(backend.dropped.load(Ordering::SeqCst),1);assert_eq!(posts(&backend),0);
    }
    #[tokio::test]
    async fn proven_empty_discovery_upgrades_no_work_without_losing_empty_source_behavior() {
        let backend=Fake {existing:Some(true),..Default::default()};let mut preparation=Preparation::default();
        preparation.observe_cold(demand(),false,false).unwrap();preparation.poll(&backend,false);settle(&mut preparation,&backend).await;
        assert_eq!(preparation.entries[&820].outcome,Some(Outcome::NoWork));
        preparation.observe_cold(HistoryDemand {to_unix:300,..demand()},true,false).unwrap();
        preparation.poll(&backend,false);settle(&mut preparation,&backend).await;
        assert_eq!(preparation.entries[&820].demand,demand());assert_eq!(posts(&backend),1);
        assert_eq!(preparation.entries[&820].outcome,Some(Outcome::Job(42,BackfillStatus::Queued)));
    }
    #[tokio::test]
    async fn larger_frozen_cold_requirement_is_admitted_once_without_sibling_range_oscillation() {
        let interrupted=Fake {fail_before_enable:true,..Default::default()};let mut preparation=Preparation::default();
        preparation.observe_cold(demand(),false,false).unwrap();preparation.poll(&interrupted,false);settle(&mut preparation,&interrupted).await;
        let larger=HistoryDemand {to_unix:300,..demand()};
        preparation.observe_cold(larger,false,false).unwrap();
        let recovered=Fake {fail_before_enable:false,..interrupted};preparation.poll(&recovered,false);settle(&mut preparation,&recovered).await;
        assert_eq!(posts(&recovered),2,"one original job and one genuinely larger required range");
        for _ in 0..100 {preparation.observe_cold(demand(),false,false).unwrap();preparation.observe_cold(larger,false,false).unwrap();preparation.poll(&recovered,false);}
        assert_eq!(preparation.entries[&820].demand,larger);assert_eq!(posts(&recovered),2);
    }
    #[tokio::test]
    async fn ready_transition_finishes_blocked_collection_without_any_more_history_admission() {
        let (backend,gate)=Fake::collection_gated();let mut preparation=Preparation::default();
        let required=demand();let cohort=BTreeMap::from([(820,required)]);
        preparation.reconcile_requirements(&cohort);
        preparation.observe_required_cold(required,false,false,required.to_unix).unwrap();preparation.poll(&backend,false);
        tokio::task::yield_now().await;
        assert_eq!(posts(&backend),1);assert_eq!(backend.call_count("target:"),1);assert_eq!(backend.registered.load(Ordering::SeqCst),0);
        let old_version=preparation.entries[&820].version;
        preparation.observe_required_cold(required,false,true,required.to_unix).unwrap();settle(&mut preparation,&backend).await;
        assert!(preparation.entries[&820].collection_only);assert_ne!(preparation.entries[&820].version,old_version);
        assert_eq!(backend.call_count("target:"),2,"old HTTP was aborted; registration-only continuation started");
        assert_eq!(posts(&backend),1);assert_eq!(backend.call_count("coverage"),1);
        // A departing profile changes historical bounds, but cannot orphan a
        // source-identity-only collection continuation while still selected.
        let narrower=HistoryDemand {from_unix:180,..required};
        preparation.reconcile_requirements(&BTreeMap::from([(820,narrower)]));
        assert_eq!(preparation.entries[&820].demand,narrower);
        gate.notify_one();settle(&mut preparation,&backend).await;
        assert_eq!(preparation.entries[&820].outcome,Some(Outcome::CollectionReady));
        assert_eq!(backend.registered.load(Ordering::SeqCst),1);assert_eq!(posts(&backend),1);
        preparation.entries.get_mut(&820).unwrap().next=Instant::now();
        let disabled=Fake {disabled:true,collection_gate:None,..backend.clone()};
        preparation.poll(&disabled,false);settle(&mut preparation,&disabled).await;
        assert_eq!(preparation.entries[&820].outcome,Some(Outcome::Disabled));assert_eq!(posts(&backend),1);
    }
    #[tokio::test]
    async fn stop_and_deselection_abort_collection_only_continuations_without_cancelling_history() {
        for stop in [false,true] {
            let (backend,_gate)=Fake::collection_gated();let mut preparation=Preparation::default();
            preparation.observe_cold(demand(),false,false).unwrap();preparation.poll(&backend,false);tokio::task::yield_now().await;
            assert_eq!(posts(&backend),1);
            preparation.observe_cold(demand(),false,true).unwrap();settle(&mut preparation,&backend).await;
            if stop {preparation.poll(&backend,true);} else {preparation.reconcile_requirements(&BTreeMap::new());}
            settle(&mut preparation,&backend).await;
            assert!(preparation.entries.is_empty());assert!(preparation.tasks.is_empty());
            assert_eq!(backend.registered.load(Ordering::SeqCst),0);assert_eq!(backend.jobs.lock().unwrap().len(),1);
        }
    }
    #[tokio::test]
    async fn current_cohort_reconciliation_keeps_empty_operation_right_edge_frozen() {
        let backend=Fake::default();let mut preparation=Preparation::default();let required=demand();
        let cohort=BTreeMap::from([(820,required)]);
        preparation.reconcile_requirements(&cohort);preparation.observe_required_cold(required,true,false,300).unwrap();
        preparation.poll(&backend,false);settle(&mut preparation,&backend).await;
        let before=preparation.operation_snapshot(820).unwrap();
        for _ in 0..100 {
            preparation.reconcile_requirements(&cohort);preparation.observe_required_cold(required,true,false,360).unwrap();preparation.poll(&backend,false);
        }
        assert_eq!(preparation.operation_snapshot(820).unwrap(),before);
        assert_eq!(before.1.to_unix,300);assert_eq!(posts(&backend),1);
    }

    type ContractError=Box<dyn std::error::Error+Send+Sync>;
    #[derive(serde::Deserialize)]
    #[serde(rename_all="camelCase")]
    struct ProducerFixture {base_url:String,api_key:String,control_prefix:String,pid:u32}
    #[derive(Debug,serde::Deserialize,serde::Serialize)]
    #[serde(rename_all="camelCase")]
    struct ProducerInspection {
        target_enabled:Option<bool>,job_count:u64,history_post_count:u64,collection_put_count:u64,candle_count:u64,
    }
    struct ProducerContractReport {path:std::path::PathBuf,archive:std::path::PathBuf,value:serde_json::Value}
    impl ProducerContractReport {
        fn start(path:std::path::PathBuf,authority:&str,pid:u32)->Self {
            let archive=path.with_file_name(format!("rust-result-{pid}.json"));
            Self {path,archive,value:serde_json::json!({"status":"running","driver":"preparation::tests::real_producer_preparation_http_db_contract",
                "fixtureAuthority":authority,"fixturePid":pid,"cases":[],"currentCase":null,"scope":"localhost real Fastify/PG; no exchange workers"})}
        }
        fn begin(&mut self,name:&str) {self.value["currentCase"]=name.into();}
        fn passed(&mut self,evidence:serde_json::Value) {
            let name=self.value["currentCase"].clone();let cases=self.value["cases"].as_array_mut().unwrap();
            assert!(cases.len()<6);cases.push(serde_json::json!({"name":name,"status":"passed","evidence":evidence}));
            self.value["currentCase"]=serde_json::Value::Null;
        }
        fn finish(&mut self)->Result<(),ContractError> {
            assert_eq!(self.value["cases"].as_array().unwrap().len(),6);
            self.value["status"]="passed".into();self.save()
        }
        fn save(&self)->Result<(),ContractError> {
            let bytes=serde_json::to_vec_pretty(&self.value)?;
            std::fs::write(&self.archive,&bytes)?;std::fs::write(&self.path,&bytes)?;Ok(())
        }
    }
    impl Drop for ProducerContractReport {
        fn drop(&mut self) {
            if self.value["status"]=="running" {self.value["status"]="failed".into();}
            if self.save().is_err() {eprintln!("[preparation-contract] result artifact write failed");}
        }
    }
    fn loopback_fixture_authority(value:&str)->Result<reqwest::Url,ContractError> {
        let url=reqwest::Url::parse(value)?;
        let host=url.host_str().ok_or("fixture host absent")?;
        let port=url.port().ok_or("fixture requires an explicit ephemeral HTTP port")?;
        if url.scheme()!="http" || !matches!(host,"127.0.0.1"|"localhost") || port<1024
            || !url.username().is_empty() || url.password().is_some() || url.query().is_some() || url.fragment().is_some()
            || url.path()!="/" || value.trim_end_matches('/')!=format!("http://{host}:{port}") {
            return Err("fixture must use a literal localhost/127.0.0.1 HTTP authority with an explicit unprivileged port".into());
        }
        Ok(url)
    }
    #[test]
    fn real_producer_fixture_guard_rejects_external_or_nonliteral_authorities() {
        for url in ["http://127.0.0.1:43210","http://localhost:43210/"] {assert!(loopback_fixture_authority(url).is_ok());}
        for url in ["http://fixture.invalid:43210","http://127.0.0.1.evil.invalid:43210","http://0.0.0.0:43210",
            "http://2130706433:43210","https://127.0.0.1:43210","http://127.0.0.1","http://127.0.0.1:80",
            "http://user@127.0.0.1:43210","http://127.0.0.1:43210/v1","http://127.0.0.1:43210?external=yes"] {
            assert!(loopback_fixture_authority(url).is_err(),"unsafe fixture authority accepted");
        }
    }
    // Raw reqwest is deliberately restricted to the TEST-ONLY control prefix.
    // All public management/candle traffic below uses the actual mdclient.
    async fn producer_control(http:&reqwest::Client,fixture:&ProducerFixture,method:reqwest::Method,path:&str,body:Option<serde_json::Value>)->Result<serde_json::Value,ContractError> {
        if !path.starts_with('/') || path.contains("://") {return Err("invalid fixture control path".into());}
        let mut request=http.request(method,format!("{}{}{}",fixture.base_url.trim_end_matches('/'),fixture.control_prefix,path))
            .header("x-api-key",&fixture.api_key);
        if let Some(body)=body {request=request.json(&body);}
        let mut response=request.send().await.map_err(|_|"fixture control transport failed")?;
        if !response.status().is_success() {return Err(format!("fixture control returned {}",response.status()).into());}
        let mut bytes=Vec::new();
        while let Some(chunk)=response.chunk().await? {
            if bytes.len()+chunk.len()>64*1024 {return Err("fixture control response exceeds 64 KiB".into());}
            bytes.extend_from_slice(&chunk);
        }
        Ok(serde_json::from_slice(&bytes)?)
    }
    async fn producer_scenario(http:&reqwest::Client,fixture:&ProducerFixture,name:&str)->Result<HistoryDemand,ContractError> {
        let demand:HistoryDemand=serde_json::from_value(producer_control(http,fixture,reqwest::Method::POST,"/scenario",Some(serde_json::json!({"name":name}))).await?)?;
        demand.validate()?;assert_eq!(demand.to_unix-demand.from_unix,120,"fixture range must be exactly two closed minutes");Ok(demand)
    }
    async fn producer_inspect(http:&reqwest::Client,fixture:&ProducerFixture,id:i64)->Result<ProducerInspection,ContractError> {
        let value=producer_control(http,fixture,reqwest::Method::GET,&format!("/inspect?instrumentId={id}"),None).await?;
        assert!(value.get("targetEnabled").is_some(),"fixture inspection must distinguish absent from false");
        Ok(serde_json::from_value(value)?)
    }
    async fn settle_producer(preparation:&mut Preparation,client:&MarketDataClient,id:i64)->Result<Outcome,ContractError> {
        let deadline=Instant::now()+Duration::from_secs(7);
        loop {
            preparation.poll(client,false);
            let entry=preparation.entries.get(&id).ok_or("expected controller operation absent")?;
            if entry.task.is_none() {if let Some(outcome)=&entry.outcome {return Ok(outcome.clone());}}
            if Instant::now()>=deadline {return Err(format!("controller settle timed out: {}",preparation.status()).into());}
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
    async fn public_contract_json(client:&MarketDataClient,method:&str,path:&str,body:Option<&serde_json::Value>)->Result<serde_json::Value,ContractError> {
        let (status,value)=tokio::time::timeout(Duration::from_secs(4),client.proxy(method,path,body)).await??;
        assert!((200..300).contains(&status),"public contract HTTP status {status}");
        Ok(value.ok_or("public contract response was not JSON")?)
    }

    /// Opt-in actual Rust client/controller -> current Node Fastify -> isolated
    /// PostgreSQL regression. Node owns startup/seeding/cleanup. Missing fixture
    /// is an error on explicit execution, never a successful skipped smoke.
    #[tokio::test]
    #[ignore="requires Node-owned isolated localhost fixture; set SETUPS_LIVE_CONTRACT_FIXTURE and use --ignored --exact"]
    async fn real_producer_preparation_http_db_contract()->Result<(),ContractError> {
        for name in ["HTTP_PROXY","HTTPS_PROXY","ALL_PROXY","http_proxy","https_proxy","all_proxy"] {
            if std::env::var_os(name).is_some() {return Err("unset HTTP(S)/ALL_PROXY before running the localhost-only contract test".into());}
        }
        let path=std::path::PathBuf::from(std::env::var_os("SETUPS_LIVE_CONTRACT_FIXTURE").ok_or("SETUPS_LIVE_CONTRACT_FIXTURE is required for explicit execution")?);
        if std::fs::metadata(&path)?.len()>64*1024 {return Err("fixture manifest exceeds 64 KiB".into());}
        let fixture:ProducerFixture=serde_json::from_slice(&std::fs::read(&path)?)?;
        let authority=loopback_fixture_authority(&fixture.base_url)?;
        if fixture.control_prefix!="/__fixture" || fixture.api_key.is_empty() || fixture.api_key.len()>1024 || fixture.pid==0 {
            return Err("invalid synthetic fixture control/auth identity".into());
        }
        let addresses:Vec<_>=tokio::net::lookup_host((authority.host_str().unwrap(),authority.port().unwrap())).await?.collect();
        if addresses.is_empty() || addresses.iter().any(|address|!address.ip().is_loopback()) {return Err("fixture host did not resolve exclusively to loopback".into());}
        let http=reqwest::Client::builder().no_proxy().redirect(reqwest::redirect::Policy::none()).timeout(Duration::from_secs(3)).build()?;
        let client=MarketDataClient::new(&fixture.base_url,&fixture.api_key);
        let mut report=ProducerContractReport::start(path.parent().ok_or("fixture parent absent")?.join("rust-result.json"),&fixture.base_url,fixture.pid);

        report.begin("happy_exact_range_single_job_conditional_collection_and_repeat_reconstruction");
        let happy=producer_scenario(&http,&fixture,"happy").await?;
        let before=producer_inspect(&http,&fixture,happy.instrument_id).await?;
        assert_eq!(before.target_enabled,None);assert_eq!(before.job_count,0);assert_eq!(before.candle_count,0);
        assert!(source_is_empty(&client,happy.instrument_id,happy.to_unix).await?);
        let mut preparation=Preparation::default();preparation.observe_empty(happy)?;
        let outcome=settle_producer(&mut preparation,&client,happy.instrument_id).await?;
        let Outcome::Job(happy_job,_) = outcome else {return Err(format!("expected happy job, got {outcome:?}").into());};
        for _ in 0..3 {
            preparation.observe_empty(HistoryDemand {to_unix:happy.to_unix+60,..happy})?;
            preparation.entries.get_mut(&happy.instrument_id).unwrap().next=Instant::now();
            assert!(matches!(settle_producer(&mut preparation,&client,happy.instrument_id).await?,Outcome::Job(id,_) if id==happy_job));
        }
        assert_eq!(preparation.entries[&happy.instrument_id].demand,happy);drop(preparation);
        let mut reconstructed=Preparation::default();reconstructed.observe_empty(happy)?;
        assert!(matches!(settle_producer(&mut reconstructed,&client,happy.instrument_id).await?,Outcome::Job(id,_) if id==happy_job));
        let jobs=client.preparation_jobs(happy.instrument_id).await?;assert_eq!(jobs.jobs.len(),1);assert!(jobs.next_before_id.is_none());
        assert_eq!((jobs.jobs[0].from_unix,jobs.jobs[0].to_unix),(happy.from_unix,happy.to_unix),"real persisted public job must retain the EXCLUSIVE right edge");
        let after=producer_inspect(&http,&fixture,happy.instrument_id).await?;
        assert_eq!(after.target_enabled,Some(true));assert_eq!((after.job_count,after.history_post_count,after.collection_put_count),(1,1,1));
        let raw_job=public_contract_json(&client,"GET",&format!("/backfill-jobs?instrumentId={}&limit=1",happy.instrument_id),None).await?;
        assert_eq!(raw_job["jobs"][0]["fromUnix"],happy.from_unix);assert_eq!(raw_job["jobs"][0]["toUnix"],happy.to_unix);
        report.passed(serde_json::json!({"demand":happy,"jobId":happy_job,"before":before,"after":after,"serializedJobs":raw_job}));

        report.begin("terminal_persisted_job_reused_for_same_frozen_range");
        producer_control(&http,&fixture,reqwest::Method::POST,"/terminal",Some(serde_json::json!({"instrumentId":happy.instrument_id,"status":"done"}))).await?;
        drop(reconstructed);let mut terminal=Preparation::default();terminal.observe_empty(happy)?;
        assert_eq!(settle_producer(&mut terminal,&client,happy.instrument_id).await?,Outcome::Job(happy_job,BackfillStatus::Done));
        let inspection=producer_inspect(&http,&fixture,happy.instrument_id).await?;
        assert_eq!((inspection.job_count,inspection.history_post_count,inspection.collection_put_count),(1,1,1));
        report.passed(serde_json::json!({"demand":happy,"jobId":happy_job,"inspection":inspection}));

        report.begin("orphan_persisted_history_first_candle_absent_target_recovers_without_post");
        let orphan=producer_scenario(&http,&fixture,"orphan").await?;
        let before=producer_inspect(&http,&fixture,orphan.instrument_id).await?;
        assert_eq!(before.target_enabled,None);assert_eq!((before.job_count,before.candle_count),(1,1));
        assert!(!source_is_empty(&client,orphan.instrument_id,orphan.to_unix).await?);
        let coverage=client.preparation_coverage(orphan).await?;
        assert_eq!((coverage.present_minutes,coverage.missing_minutes),(1,1));
        let seed_job=client.preparation_jobs(orphan.instrument_id).await?.jobs[0].id;
        let mut recovered=Preparation::default();recovered.observe_cold(orphan,false,false)?;
        assert!(matches!(settle_producer(&mut recovered,&client,orphan.instrument_id).await?,Outcome::Job(id,_) if id==seed_job));
        let after=producer_inspect(&http,&fixture,orphan.instrument_id).await?;
        assert_eq!(after.target_enabled,Some(true));assert_eq!((after.job_count,after.history_post_count,after.collection_put_count),(1,0,1));
        let mut existing=Preparation::default();existing.observe_cold(orphan,false,false)?;
        assert_eq!(settle_producer(&mut existing,&client,orphan.instrument_id).await?,Outcome::NoWork);
        report.passed(serde_json::json!({"demand":orphan,"seedJobId":seed_job,"before":before,"after":after,"existingEnabled":"no_work"}));

        report.begin("explicit_false_preserved_controller_and_client_perform_no_mutation");
        let disabled=producer_scenario(&http,&fixture,"disabled").await?;
        let mut stopped=Preparation::default();stopped.observe_empty(disabled)?;
        assert_eq!(settle_producer(&mut stopped,&client,disabled.instrument_id).await?,Outcome::Disabled);
        assert!(!client.ensure_collection_if_missing(disabled.instrument_id).await?.enabled);
        let inspection=producer_inspect(&http,&fixture,disabled.instrument_id).await?;
        assert_eq!(inspection.target_enabled,Some(false));assert_eq!((inspection.job_count,inspection.history_post_count,inspection.collection_put_count),(0,0,0));
        report.passed(serde_json::json!({"demand":disabled,"inspection":inspection}));

        report.begin("real_serialization_pagination_put_echo_actual_state_and_exclusive_backfill_end");
        let mut cursor=happy.instrument_id.min(orphan.instrument_id).min(disabled.instrument_id)-1;
        let mut pages=Vec::new();let mut seen=BTreeMap::new();let last_id=happy.instrument_id.max(orphan.instrument_id).max(disabled.instrument_id);
        for _ in 0..16 {
            let page=public_contract_json(&client,"GET",&format!("/collection-targets?limit=1&afterInstrumentId={cursor}"),None).await?;
            assert_eq!(page["onlyMissingSupported"],true);assert!(page.get("nextAfterInstrumentId").is_some());
            let rows=page["targets"].as_array().ok_or("target rows absent")?;assert!(rows.len()<=1);
            for row in rows {let id=row["instrumentId"].as_i64().ok_or("target identity absent")?;assert!(id>cursor);seen.insert(id,row["enabled"].as_bool().ok_or("target state absent")?);}
            let next=page["nextAfterInstrumentId"].as_i64();pages.push(page);
            match next {Some(next)=>{assert!(next>cursor);cursor=next;if cursor>=last_id {break;}},None=>break}
        }
        assert!(pages.len()>=2,"exercise a REAL serialized pagination boundary");
        assert_eq!(seen.get(&happy.instrument_id),Some(&true));assert_eq!(seen.get(&orphan.instrument_id),Some(&true));assert_eq!(seen.get(&disabled.instrument_id),Some(&false));
        // Intentional wire-only conditional no-op, AFTER proving controller
        // emitted zero PUTs for false. The response echoes true, actual=false.
        let echo=public_contract_json(&client,"PUT","/collection-targets",Some(&serde_json::json!({"instrumentIds":[disabled.instrument_id],"enabled":true,"onlyMissing":true}))).await?;
        assert_eq!(echo["enabled"],true);assert_eq!(echo["updated"],0);
        assert!(!client.collection_state(disabled.instrument_id).await?.ok_or("disabled target disappeared")?.enabled);
        let after_echo=producer_inspect(&http,&fixture,disabled.instrument_id).await?;
        assert_eq!(after_echo.target_enabled,Some(false));assert_eq!((after_echo.job_count,after_echo.history_post_count,after_echo.collection_put_count),(0,0,1));
        let serialized_job=public_contract_json(&client,"GET",&format!("/backfill-jobs?instrumentId={}&limit=1",happy.instrument_id),None).await?;
        assert_eq!(serialized_job["jobs"][0]["fromUnix"],happy.from_unix);
        assert_eq!(serialized_job["jobs"][0]["toUnix"],happy.to_unix,"persisted inclusive internal end must serialize as the original exclusive public end, NOT end-60/end+60");
        report.passed(serde_json::json!({"pages":pages,"conditionalPutEcho":echo,"afterIntentionalNoOpProbe":after_echo,"controllerDisabledPuts":0,"intentionalWireProbePuts":1,"exclusiveBackfillJobResponse":serialized_job}));

        report.begin("legacy_serialized_capability_missing_actionable_fail_closed_before_mutation");
        let legacy=producer_scenario(&http,&fixture,"legacy").await?;
        let raw=public_contract_json(&client,"GET","/collection-targets?limit=1",None).await?;
        assert!(raw.get("onlyMissingSupported").is_none(),"legacy fixture must omit capability on the ACTUAL serialized response");
        let mut unsupported=Preparation::default();unsupported.observe_empty(legacy)?;
        assert_eq!(settle_producer(&mut unsupported,&client,legacy.instrument_id).await?,Outcome::Failed("conditional_collection_unsupported"));
        let inspection=producer_inspect(&http,&fixture,legacy.instrument_id).await?;
        assert_eq!(inspection.target_enabled,None);assert_eq!((inspection.job_count,inspection.history_post_count,inspection.collection_put_count),(0,0,0));
        report.passed(serde_json::json!({"demand":legacy,"inspection":inspection,"serializedLegacyTargets":raw,"controllerStatus":unsupported.status()}));
        report.finish()?;
        // Server/DB cleanup is owned by main, after independent readback.
        Ok(())
    }
}
