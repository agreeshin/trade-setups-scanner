//! Bounded speculative hot window. No history borrow or task survives a commit.
//! Dropping the future invalidates every touched epoch, including siblings.
use super::*;
use setups_db::continuous::{AdaptiveScanWrite,AdaptiveSetupWrite,ADAPTIVE_COMMIT_MAX_BYTES,ADAPTIVE_COMMIT_MAX_INSTRUMENTS};

const COMMIT_WAVE_SIZE:usize=4;
const _:()=assert!(COMMIT_WAVE_SIZE==4,"commit wave must match borrowed tokio::join! arity");

pub(super) struct Scan {
    pub epoch:String,
    pub instrument:AdaptiveInstrument,
    pub slot:Arc<ActiveAdaptiveSlot>,
    pub prepared:PreparedProfile,
}
#[allow(async_fn_in_trait)]
pub(super) trait BatchStore {
    async fn attempt(&self,slot:&ActiveAdaptiveSlot,scans:&mut[Scan],indices:&[usize])->Result<Vec<bool>,WorkerError>;
    async fn checkpoint(&self,slot:&ActiveAdaptiveSlot,instrument:&AdaptiveInstrument)->Result<Option<AdaptiveCheckpoint>,WorkerError>;
    async fn current(&self,slot:&ActiveAdaptiveSlot)->Result<bool,WorkerError>;
}
pub(super) struct PgBatchStore<'a> {pub pool:&'a PgPool}
impl BatchStore for PgBatchStore<'_> {
    async fn attempt(&self,slot:&ActiveAdaptiveSlot,scans:&mut[Scan],indices:&[usize])->Result<Vec<bool>,WorkerError> {
        // Most hot pages have no setup. No name query or full recipe clone is
        // needed in that case; otherwise capture once for this bounded commit.
        let captured=if indices.iter().any(|&i|!scans[i].prepared.setups.is_empty()) {
            Some(store::capture_slot(self.pool,slot).await?)
        }else{None};
        let slot=captured.as_ref().unwrap_or(slot);
        // Optional evidence is loaded once for this commit, never into the
        // scientific profile/readiness state or a process-wide cache.
        let evidence=if indices.iter().any(|&i|scans[i].prepared.setups.iter().any(|setup|setup.recommendation.get("expectancy").is_none())) {
            Some(super::enrichment::load(self.pool,slot).await)
        }else{None};
        if evidence.as_ref().is_some_and(|captured|captured.retryable_failure) {
            return Err(invalid("expectancy storage temporarily unavailable; retry live admission"));
        }
        let mut degraded=0usize;
        let mut writes=Vec::with_capacity(indices.len());
        for &index in indices {
            let scan=&mut scans[index];
            let mut setups=Vec::with_capacity(scan.prepared.setups.len());
            for setup in &mut scan.prepared.setups {
                setup.recommendation["externalName"]=slot.external_name.clone().into();
                let invalid=super::enrichment::attach(slot,&scan.instrument,setup,
                    evidence.as_ref().and_then(|captured|captured.artifact.as_ref()),
                    evidence.as_ref().and_then(|captured|captured.artifact_id))?;
                if invalid || evidence.as_ref().is_some_and(|captured|captured.invalid_evidence) {degraded+=1;}
                setups.push(AdaptiveSetupWrite {exchange_code:scan.instrument.exchange.clone(),category:scan.instrument.category.clone(),
                    symbol:scan.instrument.symbol.clone(),setup:setup.raw.clone(),recommendation:setup.recommendation.clone()});
            }
            writes.push(AdaptiveScanWrite {instrument_key:instrument_key(&scan.instrument),
                previous:scan.prepared.expected.as_ref().map(|state|Ok::<_,WorkerError>(continuous::AdaptiveCheckpoint {
                    generation_id:state.generation_id,state:serde_json::to_value(state)?})).transpose()?,
                checkpoint:serde_json::to_value(&scan.prepared.checkpoint)?,setups});
        }
        if degraded>0 {super::enrichment::report_degraded(slot,degraded);}
        let bytes=writes.iter().try_fold(0usize,|sum,write|Ok::<_,WorkerError>(sum.saturating_add(
            continuous::adaptive_scan_write_bytes(write)?)))?;
        if bytes>ADAPTIVE_COMMIT_MAX_BYTES {return Err(invalid("adaptive constructed writes exceed batch byte budget"));}
        Ok(continuous::commit_adaptive_scans_cas(self.pool,slot,&writes).await?.into_iter().map(|commit|commit.applied).collect())
    }
    async fn checkpoint(&self,slot:&ActiveAdaptiveSlot,instrument:&AdaptiveInstrument)->Result<Option<AdaptiveCheckpoint>,WorkerError> {
        store::load_checkpoint(self.pool,slot.slot_id,instrument).await
    }
    async fn current(&self,slot:&ActiveAdaptiveSlot)->Result<bool,WorkerError> {
        Ok(continuous::adaptive_notification_permitted(self.pool,slot).await?)
    }
}

/// Only ambiguous ACKs read back. A confirmed stale/fenced item never advances
/// RAM; other items may have committed in the same transaction.
pub(super) async fn persist(store:&impl BatchStore,scans:&mut[Scan])->Result<Vec<bool>,WorkerError> {
    let Some(first)=scans.first() else{return Ok(Vec::new());};
    let slot=first.slot.clone();
    let key=first.prepared.key.clone();
    if scans.iter().any(|scan|scan.prepared.key!=key) {return Err(invalid("mixed adaptive batch fences"));}
    let mut resolved=vec![None;scans.len()];
    let mut uncertain=vec![false;scans.len()];
    let mut last_error=None;
    for _ in 0..2 {
        let indices:Vec<_>=resolved.iter().enumerate().filter_map(|(i,result)|result.is_none().then_some(i)).collect();
        if indices.is_empty() {break;}
        match store.attempt(&slot,scans,&indices).await {
            Ok(applied)=>{
                if applied.len()!=indices.len() {return Err(invalid("adaptive batch result length mismatch"));}
                let mut verify_fence=false;
                for (index,applied) in indices.into_iter().zip(applied) {
                    if !applied && uncertain[index] {
                        // A delayed original COMMIT can win AFTER the first
                        // predecessor read, making this retry's CAS return false.
                        let durable=store.checkpoint(&slot,&scans[index].instrument).await?;
                        if durable.as_ref()==Some(&scans[index].prepared.checkpoint) {resolved[index]=Some(true);}
                        else if durable!=scans[index].prepared.expected {resolved[index]=Some(false);}
                        verify_fence=true;
                    }else{resolved[index]=Some(applied);}
                }
                if verify_fence && !store.current(&slot).await? {return Ok(vec![false;scans.len()]);}
            },
            Err(error)=>{
                last_error=Some(error);
                for &index in &indices {
                    uncertain[index]=true;
                    let durable=store.checkpoint(&slot,&scans[index].instrument).await?;
                    if durable.as_ref()==Some(&scans[index].prepared.checkpoint) {resolved[index]=Some(true);}
                    else if durable!=scans[index].prepared.expected {resolved[index]=Some(false);}
                }
                // Exact successor AND the original permission/fence are required.
                if !store.current(&slot).await? {return Ok(vec![false;scans.len()]);}
            },
        }
    }
    if resolved.iter().any(Option::is_none) {return Err(last_error.unwrap_or_else(||invalid("adaptive batch acknowledgement unresolved")));}
    Ok(resolved.into_iter().map(Option::unwrap).collect())
}
async fn persist_optional(store:&impl BatchStore,scans:Option<&mut Vec<Scan>>)->Option<Result<Vec<bool>,WorkerError>> {
    match scans {Some(scans) if !scans.is_empty()=>Some(persist(store,scans).await),_=>None}
}

struct CountBytes(usize);
impl std::io::Write for CountBytes {
    fn write(&mut self,bytes:&[u8])->std::io::Result<usize> {
        self.0=self.0.checked_add(bytes.len()).ok_or_else(||std::io::Error::other("adaptive write size overflow"))?;
        Ok(bytes.len())
    }
    fn flush(&mut self)->std::io::Result<()> {Ok(())}
}
fn wire_bytes(prepared:&PreparedProfile,instrument:&AdaptiveInstrument)->Result<usize,WorkerError> {
    // Conservative JSON field/envelope and editable-name headroom; counting
    // never stages a serialized copy. Actual DB admission remains authoritative.
    let mut bytes=CountBytes(1024+instrument_key(instrument).len()*2);
    serde_json::to_writer(&mut bytes,&prepared.expected)?;
    serde_json::to_writer(&mut bytes,&prepared.checkpoint)?;
    for setup in &prepared.setups {
        serde_json::to_writer(&mut bytes,&setup.raw)?;
        serde_json::to_writer(&mut bytes,&setup.recommendation)?;
        bytes.0=bytes.0.checked_add(1536+super::enrichment::MAX_EXPECTANCY_SNAPSHOT_BYTES).ok_or_else(||invalid("adaptive write size overflow"))?;
    }
    Ok(bytes.0)
}

pub(super) struct Window<'a,S:BatchStore> {
    pub state:&'a mut AdaptiveRuntime,
    store:&'a S,
    pending:Vec<Scan>,
    counts:BTreeMap<String,usize>,
    bytes:usize,
    scalar_bytes:usize,
    dirty:BTreeSet<String>,
    rejected:BTreeSet<String>,
}
impl<'a,S:BatchStore> Window<'a,S> {
    pub fn new(state:&'a mut AdaptiveRuntime,store:&'a S)->Self {
        Self {state,store,pending:Vec::new(),counts:BTreeMap::new(),bytes:0,scalar_bytes:0,dirty:BTreeSet::new(),rejected:BTreeSet::new()}
    }
    pub fn reject(&mut self,epoch:&str) {self.rejected.insert(epoch.into());self.state.invalidate(epoch,None);}
    pub async fn prepare(&mut self,plan:&SymbolPlan,bars:&[DetectorBar],target:i64)->Result<(),WorkerError> {
        if self.rejected.contains(&plan.epoch) {return Ok(());}
        self.dirty.insert(plan.epoch.clone());
        let prepared=self.state.prepare(&plan.epoch,&plan.instrument,bars,target,true)?;
        // All scalar clones of this one symbol are owned locally until queued.
        let local_bytes=prepared.iter().map(PreparedProfile::retained_scalar_bytes).sum::<usize>();
        let local_wire=prepared.iter().try_fold(0usize,|total,item|Ok::<_,WorkerError>(
            total.saturating_add(wire_bytes(item,&plan.instrument)?)))?;
        self.check_memory(local_bytes,local_wire)?;
        for prepared in prepared {
            if !prepared.should_commit() {self.state.accept(&plan.epoch,prepared,false);continue;}
            let bytes=wire_bytes(&prepared,&plan.instrument)?;
            if bytes>ADAPTIVE_COMMIT_MAX_BYTES {return Err(invalid("adaptive prepared item exceeds batch byte budget"));}
            let scalar=prepared.retained_scalar_bytes();
            if !self.pending.is_empty() && (self.counts.get(&prepared.key).copied().unwrap_or(0)>=ADAPTIVE_COMMIT_MAX_INSTRUMENTS
                || self.pending.len()>=ADAPTIVE_COMMIT_MAX_INSTRUMENTS.saturating_mul(self.state.limits.max_profiles)
                || self.bytes.saturating_add(bytes)>ADAPTIVE_COMMIT_MAX_BYTES
                || self.memory_exceeded(scalar,bytes)) {self.flush().await?;}
            if self.rejected.contains(&plan.epoch) {continue;}
            self.check_memory(scalar,bytes)?;
            let slot=plan.slots.get(&prepared.key).cloned().ok_or_else(||invalid("active batch store missing"))?;
            self.bytes+=bytes;self.scalar_bytes+=scalar;
            *self.counts.entry(prepared.key.clone()).or_default()+=1;
            self.pending.push(Scan {epoch:plan.epoch.clone(),instrument:plan.instrument.clone(),slot,prepared});
        }
        Ok(())
    }
    fn memory_exceeded(&self,scalar:usize,wire:usize)->bool {
        self.state.retained_profile_bytes().saturating_add(self.scalar_bytes).saturating_add(scalar)
            .saturating_add(self.bytes.saturating_add(wire).saturating_mul(2))>self.state.limits.max_profile_bytes
    }
    fn check_memory(&self,scalar:usize,wire:usize)->Result<(),WorkerError> {
        if self.memory_exceeded(scalar,wire) {return Err(invalid("adaptive prepared window exceeds profile byte budget"));} Ok(())
    }
    async fn flush(&mut self)->Result<(),WorkerError> {
        let mut groups:BTreeMap<String,Vec<Scan>>=BTreeMap::new();
        for scan in std::mem::take(&mut self.pending) {
            if !self.rejected.contains(&scan.epoch) {groups.entry(scan.prepared.key.clone()).or_default().push(scan);}
        }
        let mut groups:Vec<_>=groups.into_values().collect();
        for wave in groups.chunks_mut(COMMIT_WAVE_SIZE) {
            for scans in wave.iter_mut() {scans.retain(|scan|!self.rejected.contains(&scan.epoch));}
            // Name capture clones at most one accepted recipe per launched
            // slot. Reserve conservative JSON-DOM/allocator headroom as well as
            // prepared scalars and both effect/write representations. Source
            // responses retain their separate existing 4 x 8 MiB client bound.
            let capture_bytes=wave.iter().try_fold(0usize,|total,scans| {
                let mut bytes=CountBytes(0);
                if scans.iter().any(|scan|!scan.prepared.setups.is_empty()) {
                    serde_json::to_writer(&mut bytes,&*scans[0].slot)?;
                }
                Ok::<_,WorkerError>(total.saturating_add(bytes.0.saturating_mul(16)).saturating_add(4096))
            })?;
            self.check_memory(capture_bytes,0)?;
            // Distinct captured slots have independent DB fences. Borrowed
            // futures preserve the former cross-profile overlap without tasks,
            // detached work, history clones, or extra Send requirements.
            let mut iter=wave.iter_mut();
            let (a,b,c,d)=tokio::join!(persist_optional(self.store,iter.next()),persist_optional(self.store,iter.next()),
                persist_optional(self.store,iter.next()),persist_optional(self.store,iter.next()));
            let outcomes:Vec<_>=[a,b,c,d].into_iter().flatten().collect();
            // Empty rejected groups did not launch; align outcomes with exactly
            // the remaining groups. Classify EVERY result before RAM adoption.
            for (scans,result) in wave.iter().filter(|scans|!scans.is_empty()).zip(&outcomes) {
                match result {
                    Ok(applied)=>for (scan,applied) in scans.iter().zip(applied) {if !applied {self.reject(&scan.epoch);}},
                    Err(_)=>for scan in scans {self.reject(&scan.epoch);},
                }
            }
            let mut first_error=None;
            for (scans,result) in wave.iter_mut().filter(|scans|!scans.is_empty()).zip(outcomes) {
                match result {
                    Ok(applied)=>for (scan,applied) in std::mem::take(scans).into_iter().zip(applied) {
                        if applied && !self.rejected.contains(&scan.epoch) && self.state.has_epoch(&scan.epoch) {
                            self.state.accept(&scan.epoch,scan.prepared,true);
                        }
                    },
                    Err(error)=>{first_error.get_or_insert(error);},
                }
            }
            if let Some(error)=first_error {return Err(error);}
        }
        self.bytes=0;self.scalar_bytes=0;self.counts.clear();
        Ok(())
    }
    pub async fn finish(mut self)->Result<(),WorkerError> {
        self.flush().await?;
        self.dirty.clear();
        Ok(())
    }
}
impl<S:BatchStore> Drop for Window<'_,S> {
    fn drop(&mut self) {
        for epoch in &self.dirty {self.state.invalidate(epoch,None);}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adaptive::state::tests::fixture;
    use std::sync::{Mutex,atomic::{AtomicUsize,Ordering}};
    #[derive(Clone,Copy,Default)]
    enum Mode {#[default] Success,LostAck,Unchanged,Changed,Stopped,Unreadable,Blocked,DelayedFalse,DelayedStopped,Concurrent}
    #[derive(Default)]
    struct Fake {
        mode:Mode,
        calls:AtomicUsize,
        sizes:Mutex<Vec<usize>>,
        durable:Mutex<BTreeMap<String,AdaptiveCheckpoint>>,
        delayed:Mutex<BTreeMap<String,AdaptiveCheckpoint>>,
        writes:AtomicUsize,
        inflight:AtomicUsize,
        peak:AtomicUsize,
        reject_slot:Option<i64>,
    }
    impl BatchStore for Fake {
        async fn attempt(&self,slot:&ActiveAdaptiveSlot,scans:&mut[Scan],indices:&[usize])->Result<Vec<bool>,WorkerError> {
            let call=self.calls.fetch_add(1,Ordering::SeqCst);
            self.sizes.lock().unwrap().push(indices.len());
            if matches!(self.mode,Mode::Concurrent) {
                let active=self.inflight.fetch_add(1,Ordering::SeqCst)+1;self.peak.fetch_max(active,Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(1)).await;self.inflight.fetch_sub(1,Ordering::SeqCst);
            }
            if matches!(self.mode,Mode::Blocked) {std::future::pending::<()>().await;}
            if matches!(self.mode,Mode::DelayedFalse|Mode::DelayedStopped) {
                if call==0 {
                    for &index in indices {self.delayed.lock().unwrap().insert(instrument_key(&scans[index].instrument),scans[index].prepared.checkpoint.clone());}
                    return Err(invalid("original commit still completing"));
                }
                return Ok(vec![false;indices.len()]);
            }
            if call==0 && matches!(self.mode,Mode::Unchanged|Mode::Unreadable) {return Err(invalid("unconfirmed ACK"));}
            for &index in indices {
                let mut checkpoint=scans[index].prepared.checkpoint.clone();
                if matches!(self.mode,Mode::Changed) {checkpoint.next_open_unix+=60;}
                self.durable.lock().unwrap().insert(instrument_key(&scans[index].instrument),checkpoint);
                self.writes.fetch_add(1,Ordering::SeqCst);
            }
            if call==0 && matches!(self.mode,Mode::LostAck|Mode::Changed|Mode::Stopped) {return Err(invalid("lost commit ACK"));}
            Ok(indices.iter().map(|_|self.reject_slot!=Some(slot.slot_id)).collect())
        }
        async fn checkpoint(&self,_:&ActiveAdaptiveSlot,instrument:&AdaptiveInstrument)->Result<Option<AdaptiveCheckpoint>,WorkerError> {
            if matches!(self.mode,Mode::Unreadable) {return Err(invalid("readback unavailable"));}
            let key=instrument_key(instrument);
            let observed=self.durable.lock().unwrap().get(&key).cloned();
            if let Some(late)=self.delayed.lock().unwrap().remove(&key) {
                self.durable.lock().unwrap().insert(key,late);self.writes.fetch_add(1,Ordering::SeqCst);
            }
            Ok(observed)
        }
        async fn current(&self,_:&ActiveAdaptiveSlot)->Result<bool,WorkerError> {
            Ok(!matches!(self.mode,Mode::Stopped) && !(matches!(self.mode,Mode::DelayedStopped) && self.calls.load(Ordering::SeqCst)>=2))
        }
    }
    fn slot(spec:&ProfileSpec)->Arc<ActiveAdaptiveSlot> {
        Arc::new(ActiveAdaptiveSlot {owner_id:spec.fence.owner_id.clone(),algorithm_id:spec.fence.algorithm_id.clone(),
            slot_id:spec.fence.slot_id,generation_id:spec.fence.generation_id,study_id:1,config_revision:spec.fence.config_revision,
            min_setups_per_day:1.0,spec:json!({}),result:json!({}),params:json!({}),analytics:json!({}),selected_cell:json!({}),
            mean_expectancy:1.0,direction:"long".into(),external_name:"fixture".into(),live_initialization:None,live_readiness:Vec::new()})
    }
    fn plan(state:&mut AdaptiveRuntime,index:usize,specs:&[ProfileSpec])->SymbolPlan {
        let instrument=AdaptiveInstrument {exchange:"test".into(),category:"linear".into(),symbol:format!("HOT{index}USDT")};
        let epoch=AdaptiveRuntime::epoch_key(&instrument,None);
        state.install(&epoch,specs).unwrap();
        SymbolPlan {epoch,instrument,catalog_id:index as i64+1,specs:specs.to_vec(),
            slots:specs.iter().map(|spec|(spec.key(),slot(spec))).collect()}
    }
    fn spec()->ProfileSpec {let mut spec=fixture().spec;spec.warmup_through=0;spec.emit_from_close=60;spec}
    fn bar()->DetectorBar {DetectorBar {open_time_unix:0,close:"100".into(),quote_turnover:"1".into()}}

    #[tokio::test]
    async fn full_805_symbol_hot_burst_uses_29_bounded_batches_not_805_commits() {
        let mut state=AdaptiveRuntime::default();let spec=spec();
        let plans:Vec<_>=(0..805).map(|index|plan(&mut state,index,std::slice::from_ref(&spec))).collect();
        let store=Fake::default();
        for response in plans.chunks(200) {
            let mut window=Window::new(&mut state,&store);
            for plan in response {window.prepare(plan,&[bar()],60).await.unwrap();}
            window.finish().await.unwrap();
        }
        assert_eq!(store.calls.load(Ordering::SeqCst),29);
        assert!(store.sizes.lock().unwrap().iter().all(|&count|count<=ADAPTIVE_COMMIT_MAX_INSTRUMENTS));
        for plan in plans {assert_eq!(state.cached_checkpoint(&plan.epoch,&spec.key()).unwrap().unwrap().next_open_unix,60);}
        state.clear();assert_eq!(state.retained_history_bytes(),0);assert_eq!(state.retained_profile_bytes(),0);
    }
    #[tokio::test]
    async fn ten_profiles_batch_32_instruments_per_slot_not_32_total_profile_scans() {
        let mut state=AdaptiveRuntime::default();state.limits.max_profiles=10;
        let specs:Vec<_>=(1..=10).map(|slot|{let mut spec=spec();spec.fence.slot_id=slot;spec.fence.generation_id=slot;spec}).collect();
        let plans:Vec<_>=(0..64).map(|index|plan(&mut state,index,&specs)).collect();
        let store=Fake {mode:Mode::Concurrent,..Fake::default()};let mut window=Window::new(&mut state,&store);
        for plan in &plans {
            window.prepare(plan,&[bar()],60).await.unwrap();
            assert!(window.pending.len()<=32*10);
            assert!(window.bytes<=ADAPTIVE_COMMIT_MAX_BYTES);
            assert!(window.counts.values().all(|&count|count<=32));
        }
        window.finish().await.unwrap();
        assert_eq!(store.calls.load(Ordering::SeqCst),20,"64 instruments must use two commits per slot, not about 22 per slot");
        assert!(store.sizes.lock().unwrap().iter().all(|&count|count==32));
        assert_eq!(store.peak.load(Ordering::SeqCst),COMMIT_WAVE_SIZE,"independent slots must overlap in bounded waves");
        assert_eq!(store.inflight.load(Ordering::SeqCst),0);
        for plan in plans {for spec in &specs {assert_eq!(state.cached_checkpoint(&plan.epoch,&spec.key()).unwrap().unwrap().next_open_unix,60);}}
    }
    #[tokio::test]
    async fn ambiguity_requires_exact_successor_and_fence_and_retries_only_predecessors() {
        for (mode,accepted,calls) in [(Mode::LostAck,true,1),(Mode::Unchanged,true,2),(Mode::Changed,false,1),(Mode::Stopped,false,1)] {
            let mut state=AdaptiveRuntime::default();let spec=spec();let plan=plan(&mut state,0,std::slice::from_ref(&spec));
            let store=Fake {mode,..Fake::default()};
            let mut window=Window::new(&mut state,&store);window.prepare(&plan,&[bar()],60).await.unwrap();
            assert!(window.state.cached_checkpoint(&plan.epoch,&spec.key()).unwrap().is_none(),"speculation must not be accepted before commit");
            window.finish().await.unwrap();
            assert_eq!(store.calls.load(Ordering::SeqCst),calls);
            assert_eq!(state.has_epoch(&plan.epoch),accepted);
            if accepted {assert_eq!(state.cached_checkpoint(&plan.epoch,&spec.key()).unwrap().unwrap().next_open_unix,60);}
        }
    }
    #[tokio::test]
    async fn delayed_original_commit_then_false_retry_reconciles_successor_without_duplicate_effects() {
        for mode in [Mode::DelayedFalse,Mode::DelayedStopped] {
            let mut state=AdaptiveRuntime::default();let spec=spec();let plan=plan(&mut state,0,std::slice::from_ref(&spec));
            let store=Fake {mode,..Fake::default()};
            let mut window=Window::new(&mut state,&store);window.prepare(&plan,&[bar()],60).await.unwrap();window.finish().await.unwrap();
            assert_eq!(store.calls.load(Ordering::SeqCst),2);
            assert_eq!(store.writes.load(Ordering::SeqCst),1,"only the delayed original commit writes effects");
            assert_eq!(state.has_epoch(&plan.epoch),matches!(mode,Mode::DelayedFalse));
            if state.has_epoch(&plan.epoch) {assert_eq!(state.cached_checkpoint(&plan.epoch,&spec.key()).unwrap().unwrap().next_open_unix,60);}
        }
    }
    #[tokio::test]
    async fn epoch_rejection_discards_later_siblings_and_never_resurrects_shared_history() {
        let mut state=AdaptiveRuntime::default();let first=spec();let mut second=first.clone();second.fence.slot_id=2;
        let mut third=first.clone();third.fence.slot_id=3;
        let mut fourth=first.clone();fourth.fence.slot_id=4;
        let mut fifth=first.clone();fifth.fence.slot_id=5;
        let plan=plan(&mut state,0,&[first,second,third,fourth,fifth]);
        let store=Fake {reject_slot:Some(2),..Fake::default()};
        let mut window=Window::new(&mut state,&store);window.prepare(&plan,&[bar()],60).await.unwrap();window.finish().await.unwrap();
        assert!(!state.has_epoch(&plan.epoch));assert_eq!(state.retained_history_bytes(),0);
        assert_eq!(store.calls.load(Ordering::SeqCst),4,"the next wave must not persist a fifth sibling after whole-epoch invalidation");
    }
    #[tokio::test]
    async fn unreadable_ack_and_cancelled_commit_release_every_speculative_epoch() {
        for mode in [Mode::Unreadable,Mode::Blocked] {
            let mut state=AdaptiveRuntime::default();let plan=plan(&mut state,0,&[spec()]);let store=Fake {mode,..Fake::default()};
            let mut window=Window::new(&mut state,&store);window.prepare(&plan,&[bar()],60).await.unwrap();
            let result=tokio::time::timeout(Duration::from_millis(5),window.finish()).await;
            match mode {Mode::Blocked=>assert!(result.is_err()),_=>assert!(result.unwrap().is_err())}
            assert_eq!(state.retained_history_epochs(),0);assert_eq!(state.retained_profile_bytes(),0);
        }
    }
    #[tokio::test]
    async fn byte_admission_failure_drops_prepared_state_without_issuing_a_commit() {
        let mut state=AdaptiveRuntime::default();let spec=spec();let plan=plan(&mut state,0,std::slice::from_ref(&spec));
        state.limits.max_profile_bytes=state.retained_profile_bytes()+1;
        let store=Fake::default();
        {let mut window=Window::new(&mut state,&store);assert!(window.prepare(&plan,&[bar()],60).await.is_err());}
        assert_eq!(store.calls.load(Ordering::SeqCst),0);assert_eq!(state.retained_history_bytes(),0);
    }
}
