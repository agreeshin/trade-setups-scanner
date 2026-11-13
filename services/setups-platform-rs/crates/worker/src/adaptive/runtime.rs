use super::*;
use serde_json::json;
use setups_db::continuous::{self, ActiveAdaptiveSlot, live};
use setups_db::settings_get;
use setups_domain::RecommendationDirection;
use setups_mdclient::{CandlesQuery, CatalogInstrument, MarketDataClient, TailBatchQuery};
pub use setups_optimizer::ContinuousBootstrap as AdaptiveLiveBootstrap;
use sqlx::PgPool;
use std::{collections::{BTreeMap,BTreeSet},sync::Arc,time::{Duration,Instant}};
use crate::adaptive::state::{ProfileSpec,PreparedProfile};

mod delivery;
mod profile;
mod store;
mod source;
mod promotion;
mod text;
use profile::{LiveProfile,RecommendationEnvelope};
pub use store::PgAdaptiveStore;
pub use text::recommendation_message;

const HISTORY_PAGE_LIMIT:u32=5000;
const IO_CONCURRENCY:usize=4;
const COMMIT_CONCURRENCY:usize=16;
const WARMUP_TIME_SLICE:Duration=Duration::from_secs(1);

fn instrument_key(instrument:&AdaptiveInstrument)->String {format!("{}/{}/{}",instrument.exchange,instrument.category,instrument.symbol)}
fn instrument(item:&CatalogInstrument)->AdaptiveInstrument {AdaptiveInstrument {exchange:item.exchange.clone(),category:item.category.clone(),symbol:item.symbol.clone()}}

#[derive(Clone)]
struct SymbolPlan {
    epoch:String,
    instrument:AdaptiveInstrument,
    catalog_id:i64,
    specs:Vec<ProfileSpec>,
    slots:BTreeMap<String,Arc<ActiveAdaptiveSlot>>,
}
struct Warmup {
    pending:live::PendingAdaptiveSlot,
    lease:live::WarmupLease,
    plans:Vec<SymbolPlan>,
    candidate_key:String,
    cohort:BTreeSet<String>,
    barrier_attempts:u32,
}
pub struct AdaptiveLiveRuntime {
    state:AdaptiveRuntime,
    warmup:Option<Warmup>,
    worker:String,
    next_pending:usize,
    warm_cursor:usize,
    peak_staging_epochs:usize,
}
impl AdaptiveLiveRuntime {
    pub fn new(limits:AdaptiveRuntimeLimits)->Self {
        Self {state:AdaptiveRuntime::new(limits),warmup:None,
            worker:format!("adaptive-live-{}",std::process::id()),next_pending:0,warm_cursor:0,peak_staging_epochs:0}
    }
    pub fn peak_staging_epochs(&self)->usize {self.peak_staging_epochs}
    pub fn retained_history_epochs(&self)->usize {self.state.retained_history_epochs()}
    pub fn retained_history_bytes(&self)->usize {self.state.retained_history_bytes()}
    pub fn retained_profile_bytes(&self)->usize {self.state.retained_profile_bytes()}
    async fn active_plans(&mut self,pool:&PgPool,slots:&[Arc<ActiveAdaptiveSlot>],catalog:&[CatalogInstrument])->Result<Vec<SymbolPlan>,WorkerError> {
        if slots.len()>self.state.limits.max_profiles {return Err(invalid("active live slot count exceeds admission budget"));}
        let mut plans:BTreeMap<String,SymbolPlan>=BTreeMap::new();
        for slot in slots {
            let profile=LiveProfile::parse(slot)?;
            for seed in &profile.bootstrap.instruments {
                let matches:Vec<_>=catalog.iter().filter(|c|c.status=="active" && c.exchange==seed.instrument.exchange
                    && c.category==seed.instrument.category && c.symbol==seed.instrument.symbol).collect();
                if matches.len()>1 {return Err(invalid("ambiguous active listing"));}
                let Some(item)=matches.first() else {continue;};
                let instrument=instrument(item);
                let key=profile.fence_key();
                let epoch=self.state.profile_epoch(&instrument,&key).unwrap_or_else(||AdaptiveRuntime::epoch_key(&instrument,None));
                let checkpoint=match self.state.cached_checkpoint(&epoch,&key) {
                    Some(checkpoint)=>checkpoint,
                    None=>store::load_checkpoint(pool,slot.slot_id,&instrument).await?,
                };
                let spec=profile.spec(seed,checkpoint,false)?;
                let plan=plans.entry(epoch.clone()).or_insert_with(||SymbolPlan {epoch,instrument,catalog_id:item.id,specs:Vec::new(),slots:BTreeMap::new()});
                plan.slots.insert(spec.key(),slot.clone());plan.specs.push(spec);
            }
        }
        Ok(plans.into_values().collect())
    }
    async fn build_warmup_plans(&self,pool:&PgPool,candidate:&live::PendingAdaptiveSlot,active:&[SymbolPlan],slots:&[Arc<ActiveAdaptiveSlot>],catalog:&[CatalogInstrument])->Result<(Vec<SymbolPlan>,String),WorkerError> {
        let profile=LiveProfile::parse(&candidate.profile)?;
        let candidate_key=profile.fence_key();
        let incumbent=slots.iter().find(|slot|slot.slot_id==candidate.profile.slot_id).cloned();
        let old=incumbent.as_ref().map(|slot|LiveProfile::parse(slot)).transpose()?;
        let mut scope:BTreeSet<_>=profile.bootstrap.instruments.iter().map(|seed|seed.instrument.clone()).collect();
        if let Some(old)=&old {scope.extend(old.bootstrap.instruments.iter().map(|seed|seed.instrument.clone()));}
        let mut plans=Vec::new();
        for scoped in scope {
            let matches:Vec<_>=catalog.iter().filter(|item|item.exchange==scoped.exchange && item.category==scoped.category && item.symbol==scoped.symbol).collect();
            if matches.len()!=1 {return Err(invalid("pending old/new scope missing or ambiguous in source catalog"));}
            let item=matches[0];let instrument=instrument(item);
            let candidate_seed=profile.bootstrap.instruments.iter().find(|seed|seed.instrument==scoped);
            let epoch=AdaptiveRuntime::epoch_key(&instrument,if candidate_seed.is_none() && item.status=="active" {None}else{Some(candidate.profile.generation_id)});
            let mut specs:BTreeMap<String,ProfileSpec>=BTreeMap::new();let mut stores=BTreeMap::new();
            for plan in active.iter().filter(|plan|plan.catalog_id==item.id) {
                for spec in &plan.specs {let mut shadow=spec.clone();shadow.pending=epoch!=AdaptiveRuntime::epoch_key(&instrument,None);specs.insert(shadow.key(),shadow);}
                stores.extend(plan.slots.clone());
            }
            if let (Some(old),Some(slot))=(&old,&incumbent)
                && let Some(seed)=old.bootstrap.instruments.iter().find(|seed|seed.instrument==scoped)
                && !specs.contains_key(&old.fence_key()) {
                let checkpoint=store::load_checkpoint(pool,slot.slot_id,&instrument).await?;
                let mut shadow=old.spec(seed,checkpoint,true)?;shadow.publish_enabled=item.status=="active";
                stores.insert(shadow.key(),slot.clone());specs.insert(shadow.key(),shadow);
            }
            if let Some(seed)=profile.bootstrap.instruments.iter().find(|seed|seed.instrument==scoped) {
                let mut pending=profile.spec(seed,None,true)?;pending.publish_enabled=item.status=="active";
                specs.insert(pending.key(),pending);
            }
            plans.push(SymbolPlan {epoch,instrument,catalog_id:item.id,specs:specs.into_values().collect(),slots:stores});
        }
        if plans.is_empty() {return Err(invalid("pending universe is empty"));}
        self.check_warmup_admission(active,&plans)?;
        Ok((plans,candidate_key))
    }
    fn check_warmup_admission(&self,active:&[SymbolPlan],pending:&[SymbolPlan])->Result<(),WorkerError> {
        let mut allocations:BTreeMap<String,usize>=BTreeMap::new();
        for plan in active.iter().chain(pending) {
            let capacity=plan.specs.iter().map(|spec|spec.params.baseline_long_days() as usize*1440
                +spec.params.window_minutes() as usize+spec.params.max_consecutive_gap_minutes() as usize+2).max().unwrap_or(0);
            let close=capacity.checked_mul(self.state.limits.max_close_bytes_per_minute).ok_or_else(||invalid("history admission overflow"))?;
            let bytes=setups_detector::shared::SharedHistory::required_bytes(capacity,close).map_err(|error|invalid(&error.to_string()))?;
            allocations.entry(AdaptiveRuntime::epoch_key(&plan.instrument,None)).and_modify(|current|*current=(*current).max(bytes)).or_insert(bytes);
        }
        let mut peaks:Vec<_>=allocations.values().copied().collect();peaks.sort_unstable_by(|a,b|b.cmp(a));
        let total=allocations.values().chain(peaks.iter().take(IO_CONCURRENCY)).try_fold(0usize,|sum,bytes|sum.checked_add(*bytes))
            .ok_or_else(||invalid("history admission overflow"))?;
        if total>self.state.limits.max_history_bytes {return Err(invalid("complete pending universe plus staging exceeds history byte admission budget"));}
        Ok(())
    }
    async fn refresh_warmup(&mut self,pool:&PgPool,pending:Vec<live::PendingAdaptiveSlot>,catalog:&[CatalogInstrument],active:&[SymbolPlan],slots:&[Arc<ActiveAdaptiveSlot>])->Result<(),WorkerError> {
        let cohort:BTreeSet<_>=slots.iter().map(|slot|json!([slot.owner_id,slot.algorithm_id,slot.slot_id,slot.generation_id,slot.config_revision]).to_string()).collect();
        if let Some(warmup)=&self.warmup {
            let current=warmup.cohort==cohort && pending.iter().any(|p|p.profile.slot_id==warmup.lease.slot_id && p.profile.generation_id==warmup.lease.generation_id
                && p.profile.config_revision==warmup.lease.config_revision);
            if current && live::report_warmup(pool,&warmup.lease,"warming",None,None).await? {return Ok(());}
            self.discard_warmup();
        }
        if pending.is_empty() {return Ok(());}
        for offset in 0..pending.len() {
            let index=(self.next_pending+offset)%pending.len();
            let candidate=&pending[index];
            let Some(lease)=live::claim_pending_warmup(pool,candidate,&self.worker).await? else {continue;};
            self.next_pending=(index+1)%pending.len();
            let build=self.build_warmup_plans(pool,candidate,active,slots,catalog).await;
            match build {
                Ok((plans,candidate_key))=>self.warmup=Some(Warmup {pending:candidate.clone(),lease,plans,candidate_key,cohort:cohort.clone(),barrier_attempts:0}),
                Err(error)=>{live::report_warmup(pool,&lease,"error",None,Some(&json!({"code":"invalid_live_candidate","message":error.to_string().chars().take(240).collect::<String>()}))).await?;}
            }
            break;
        }
        Ok(())
    }
    fn discard_warmup(&mut self) {
        if let Some(warmup)=self.warmup.take() {
            for plan in warmup.plans {
                if plan.epoch==AdaptiveRuntime::epoch_key(&plan.instrument,None) {self.state.remove_profile(&plan.epoch,&warmup.candidate_key);}
                else {self.state.invalidate(&plan.epoch,None);}
            }
        }
    }
    fn cold_plans(&self,active:&[SymbolPlan],target:i64,completed:&BTreeSet<String>)->Vec<SymbolPlan> {
        let mut cold:BTreeMap<String,SymbolPlan>=active.iter().filter(|p|!self.state.is_current(&p.epoch,target-1800) && !completed.contains(&p.epoch))
            .map(|p|(p.epoch.clone(),p.clone())).collect();
        if let Some(warmup)=&self.warmup {
            let mut resident=warmup.plans.iter().filter(|p|p.epoch!=AdaptiveRuntime::epoch_key(&p.instrument,None) && self.state.has_epoch(&p.epoch)).count();
            for plan in &warmup.plans {
                let canonical=plan.epoch==AdaptiveRuntime::epoch_key(&plan.instrument,None);
                let exists=self.state.has_epoch(&plan.epoch);
                if !canonical && !exists {
                    if resident>=IO_CONCURRENCY {continue;}
                    resident+=1;
                }
                if !self.state.is_current(&plan.epoch,target-1800) && !completed.contains(&plan.epoch) {
                    let mut merged=plan.clone();
                    if let Some(active)=cold.remove(&plan.epoch) {
                        let mut specs:BTreeMap<_,_>=merged.specs.into_iter().map(|spec|(spec.key(),spec)).collect();
                        specs.extend(active.specs.into_iter().map(|spec|(spec.key(),spec)));
                        merged.specs=specs.into_values().collect();merged.slots.extend(active.slots);
                    }
                    cold.insert(plan.epoch.clone(),merged);
                }
            }
        }
        cold.into_values().collect()
    }
    async fn adopt_ready_staging(&mut self,pool:&PgPool,frontier:i64)->Result<(),WorkerError> {
        let Some(warmup)=&self.warmup else {return Ok(());};
        let candidate=warmup.candidate_key.clone();
        let ready:Vec<_>=warmup.plans.iter().filter(|p|p.epoch!=AdaptiveRuntime::epoch_key(&p.instrument,None)
            && self.state.is_current(&p.epoch,frontier)).cloned().collect();
        for plan in ready {
            let mut expected=BTreeMap::new();let mut caught_up=true;
            for spec in plan.specs.iter().filter(|spec|spec.key()!=candidate) {
                let staged=self.state.checkpoint_for(&plan.epoch,&spec.key()).ok_or_else(||invalid("staged incumbent checkpoint missing"))?;
                let durable=store::load_checkpoint(pool,spec.fence.slot_id,&plan.instrument).await?;
                if durable.as_ref()!=Some(&staged) {
                    if spec.publish_enabled {caught_up=false;break;}
                    // A delisted incumbent has no live emissions. Its explicit
                    // not-listed source accounting may drain a control cursor.
                    let slot=plan.slots.get(&spec.key()).ok_or_else(||invalid("inactive incumbent store missing"))?;
                    let mut page=self.state.adoption_page(&plan.epoch,&spec.key(),durable)?;
                    if !store::persist(&PgAdaptiveStore {pool,slot},&plan.instrument,&mut page).await? {caught_up=false;break;}
                }
                expected.insert(spec.key(),staged);
            }
            if !caught_up {continue;}
            let production=AdaptiveRuntime::epoch_key(&plan.instrument,None);
            self.state.adopt_staging(&plan.epoch,&production,&candidate,&expected)?;
            if let Some(warmup)=&mut self.warmup
                && let Some(staged)=warmup.plans.iter_mut().find(|p|p.epoch==plan.epoch) {
                    staged.epoch=production;
                    for spec in &mut staged.specs {if spec.key()!=candidate {spec.pending=false;}}
                }
        }
        Ok(())
    }
    pub async fn tick(&mut self,pool:&PgPool,client:&MarketDataClient,stopped:&AtomicBool)->Result<(),WorkerError> {
        self.tick_at(pool,client,stopped,chrono::Utc::now().timestamp()).await
    }
    /// Explicit observation clock for deterministic guarded-tick integration.
    /// The production loop always uses tick(), which supplies the actual UTC clock.
    pub async fn tick_at(&mut self,pool:&PgPool,client:&MarketDataClient,stopped:&AtomicBool,observation_unix:i64)->Result<(),WorkerError> {
        let result=self.tick_observed(pool,client,stopped,observation_unix).await;
        if let Err(error)=&result && let Some(warmup)=&self.warmup {
            let report=json!({"code":"adaptive_warmup_failed","message":error.to_string().chars().take(240).collect::<String>()});
            if let Err(report_error)=live::report_warmup(pool,&warmup.lease,"error",None,Some(&report)).await {
                eprintln!("[adaptive-live] warmup error report failed: {}",report_error.to_string().chars().take(200).collect::<String>());
            }
            self.discard_warmup();
        }
        result
    }
    async fn tick_observed(&mut self,pool:&PgPool,client:&MarketDataClient,stopped:&AtomicBool,observation_unix:i64)->Result<(),WorkerError> {
        if stopped.load(Ordering::SeqCst) || settings_get(pool,"live").await?.unwrap_or(json!({}))["enabled"]!=true {
            self.state.clear();self.warmup=None;return Ok(());
        }
        let slots:Vec<_>=continuous::active_adaptive_slots(pool,continuous::ADMIN_OWNER).await?.into_iter().map(Arc::new).collect();
        let pending=live::pending_adaptive_slots(pool,continuous::ADMIN_OWNER).await?;
        if slots.is_empty() && pending.is_empty() {self.state.clear();self.warmup=None;return Ok(());}
        let catalog=client.instruments().await.map_err(|e|invalid(&e.to_string()))?;
        let active=self.active_plans(pool,&slots,&catalog).await?;
        self.refresh_warmup(pool,pending,&catalog,&active,&slots).await?;
        let mut wanted:BTreeMap<String,BTreeSet<String>>=BTreeMap::new();
        for plan in active.iter().chain(self.warmup.iter().flat_map(|w|w.plans.iter())) {
            wanted.entry(plan.epoch.clone()).or_default().extend(plan.specs.iter().map(ProfileSpec::key));
        }
        self.state.reconcile(&wanted);
        for plan in &active {
            // Re-enabling an already accepted profile has no pending research
            // lease. Never attach it to unchecked existing scalar history:
            // cold-reconstruct the complete current cohort once. Pending
            // generations use the non-disruptive staged path below instead.
            if self.state.needs_reconstruction(&plan.epoch,&plan.specs) {self.state.invalidate(&plan.epoch,None);}
            self.state.install(&plan.epoch,&plan.specs)?;
        }
        let target=observation_unix.div_euclid(60)*60;
        let mut hot=active.clone();
        if let Some(warmup)=&self.warmup {hot.extend(warmup.plans.iter().filter(|p|self.state.is_current(&p.epoch,target-1800)).cloned());}
        self.hot_tail(pool,client,&hot,target,stopped).await?;
        self.adopt_ready_staging(pool,target).await?;
        // Active hot work wins; at most four duplicate staging rings exist.
        // A completed empty observation is retried next tick, not in a loop.
        let mut completed=BTreeSet::new();
        let mut cold=self.cold_plans(&active,target,&completed);
        let started=Instant::now();
        let mut pages=0;
        while !cold.is_empty() && pages<self.state.limits.warmup_pages_per_turn && started.elapsed()<WARMUP_TIME_SLICE && !stopped.load(Ordering::SeqCst) {
            let count=IO_CONCURRENCY.min(self.state.limits.warmup_pages_per_turn-pages).min(cold.len());
            let mut reads=tokio::task::JoinSet::new();
            for offset in 0..count {
                let index=(self.warm_cursor+offset)%cold.len();
                let mut plan=cold[index].clone();
                if !self.state.has_epoch(&plan.epoch) {
                    for spec in plan.specs.iter_mut().filter(|spec|!spec.pending) {
                        spec.checkpoint=store::load_checkpoint(pool,spec.fence.slot_id,&plan.instrument).await?;
                    }
                }
                self.state.install(&plan.epoch,&plan.specs)?;
                let resident=self.warmup.as_ref().map_or(0,|w|w.plans.iter().filter(|p|p.epoch!=AdaptiveRuntime::epoch_key(&p.instrument,None) && self.state.has_epoch(&p.epoch)).count());
                self.peak_staging_epochs=self.peak_staging_epochs.max(resident);
                let client=client.clone();
                let (from,_)=self.state.fetch_bounds(&plan.epoch,target)?;
                let guard=self.state.source_guard(&plan.epoch,plan.catalog_id)?;
                let frozen_boundary=if plan.specs.iter().any(|spec|spec.publish_enabled) {self.state.frozen_boundary(&plan.epoch)?}else{target};
                reads.spawn(async move {
                    let result=source::read_history(&client,plan.catalog_id,from,target,guard,frozen_boundary).await;
                    (plan,result)
                });
            }
            self.warm_cursor=(self.warm_cursor+count)%cold.len();
            while let Some(result)=reads.join_next().await {
                let (plan,response)=result.map_err(|e|invalid(&e.to_string()))?;
                let page=match response {
                    Ok(page)=>page,
                    Err(failure)=>{
                        if failure.invalidate {self.state.invalidate(&plan.epoch,None);}
                        return Err(failure.error);
                    }
                };
                self.state.acknowledge_source(&plan.epoch,page.token)?;
                process_page(&mut self.state,pool,&plan,&page.bars,target,page.complete).await?;
                if page.complete {
                    completed.insert(plan.epoch.clone());
                    if !plan.specs.iter().any(|spec|spec.publish_enabled) {self.state.account_not_listed(&plan.epoch,target)?;}
                }
                pages+=1;
            }
            self.adopt_ready_staging(pool,target).await?;
            cold=self.cold_plans(&active,target,&completed);
            tokio::task::yield_now().await;
        }
        self.try_promote(pool,&slots,&catalog,target,stopped).await?;
        Ok(())
    }
    async fn hot_tail(&mut self,pool:&PgPool,client:&MarketDataClient,plans:&[SymbolPlan],target:i64,stopped:&AtomicBool)->Result<(),WorkerError> {
        let mut by_epoch:BTreeMap<String,SymbolPlan>=BTreeMap::new();
        for plan in plans.iter().filter(|plan|self.state.is_current(&plan.epoch,target-1800)) {
            if let Some(existing)=by_epoch.get_mut(&plan.epoch) {
                existing.slots.extend(plan.slots.clone());
                let specs:BTreeMap<_,_>=existing.specs.iter().chain(&plan.specs).map(|s|(s.key(),s.clone())).collect();
                existing.specs=specs.into_values().collect();
            } else {by_epoch.insert(plan.epoch.clone(),plan.clone());}
        }
        let mut canonical=BTreeMap::new();let mut staging=Vec::new();
        for plan in by_epoch.into_values() {
            if plan.epoch==AdaptiveRuntime::epoch_key(&plan.instrument,None) {
                if canonical.insert(plan.catalog_id,plan).is_some() {return Err(invalid("multiple canonical epochs for one source instrument"));}
            } else {staging.push(vec![plan]);}
        }
        if staging.len()>IO_CONCURRENCY {return Err(invalid("resident staging epochs exceed admission budget"));}
        let canonical:Vec<_>=canonical.into_values().collect();
        let batches=canonical.chunks(200).map(|batch|batch.to_vec()).collect();
        // A proof belongs to ONE consumed interval in ONE epoch. Taking the
        // oldest revision and union of intervals is not equivalent: an ordinary
        // insert can already be consumed by one epoch and still be fresh input
        // for another. Finish canonical snapshots/effects before observing the
        // at-most-four staging epochs, each in its own coherent request.
        self.hot_tail_snapshots(pool,client,batches,target,stopped).await?;
        self.hot_tail_snapshots(pool,client,staging,target,stopped).await
    }
    async fn hot_tail_snapshots(&mut self,pool:&PgPool,client:&MarketDataClient,batches:Vec<Vec<SymbolPlan>>,target:i64,stopped:&AtomicBool)->Result<(),WorkerError> {
        let mut batches=batches.into_iter();let mut first_error=None;
        loop {
            let mut reads=tokio::task::JoinSet::new();
            for batch in batches.by_ref().take(IO_CONCURRENCY) {
                let ids:Vec<_>=batch.iter().map(|plan|plan.catalog_id).collect();
                if ids.iter().copied().collect::<BTreeSet<_>>().len()!=ids.len() {return Err(invalid("one coherent tail request cannot contain two epochs of one instrument"));}
                let from=batch.iter().map(|plan|self.state.fetch_bounds(&plan.epoch,target).map(|(from,_)|from))
                    .collect::<Result<Vec<_>,_>>()?.into_iter().min().unwrap_or(target).min(target-60).max(target-1800);
                let guards=batch.iter().map(|plan|self.state.source_guard(&plan.epoch,plan.catalog_id)).collect::<Result<Vec<_>,_>>()?;
                let client=client.clone();
                reads.spawn(async move {
                    let response=client.tail_batch_guarded(&TailBatchQuery {instrument_ids:ids,from_unix:from,to_unix:target},&guards).await;
                    (batch,guards,response)
                });
            }
            if reads.is_empty() {break;}
            while let Some(response)=reads.join_next().await {
                let (batch,guards,response)=response.map_err(|e|invalid(&e.to_string()))?;
                let response=match response {
                    Ok(response)=>response,
                    Err(error)=>{
                        if matches!(&error,setups_mdclient::MdClientError::Decode(_)) {
                            for plan in &batch {self.state.invalidate(&plan.epoch,None);}
                        }
                        first_error.get_or_insert_with(||invalid(&error.to_string()));continue;
                    }
                };
                for item in response.instruments {
                    if stopped.load(Ordering::SeqCst) {return Ok(());}
                    let plan=batch.iter().find(|plan|plan.catalog_id==item.instrument_id).ok_or_else(||invalid("requested source epoch missing"))?;
                    let guard=guards.iter().find(|guard|guard.instrument_id==item.instrument_id).ok_or_else(||invalid("requested source guard missing"))?;
                    let token=guard.validated_token(item.read_guard.as_ref());
                    if item.status!="known" || token.is_none() {
                        self.state.invalidate(&plan.epoch,None);
                        first_error.get_or_insert_with(||invalid("source causal history changed or repair coverage is unknown"));continue;
                    }
                    let bars:Vec<_>=item.candles.into_iter().map(|row|DetectorBar {open_time_unix:row.candle.open_time_unix,close:row.candle.close,quote_turnover:row.candle.quote_turnover}).collect();
                    self.state.acknowledge_source(&plan.epoch,token.unwrap())?;
                    if let Err(error)=process_page(&mut self.state,pool,plan,&bars,target,true).await {first_error.get_or_insert(error);continue;}
                    if !plan.specs.iter().any(|spec|spec.publish_enabled) {
                        self.state.account_not_listed(&plan.epoch,target)?;
                        if let Err(error)=process_chunk(&mut self.state,pool,plan,&[],target,false).await {first_error.get_or_insert(error);}
                    }
                }
            }
        }
        first_error.map_or(Ok(()),Err)
    }
    async fn try_promote(&mut self,pool:&PgPool,_active:&[Arc<ActiveAdaptiveSlot>],catalog:&[CatalogInstrument],frontier:i64,stopped:&AtomicBool)->Result<(),WorkerError> {
        let Some(warmup)=self.warmup.as_ref() else {return Ok(());};
        if stopped.load(Ordering::SeqCst) {return Ok(());}
        for plan in &warmup.plans {
            if plan.epoch!=AdaptiveRuntime::epoch_key(&plan.instrument,None) || !self.state.is_current(&plan.epoch,frontier) {return Ok(());}
            if plan.specs.iter().any(|spec|spec.key()==warmup.candidate_key)
                && catalog.iter().any(|item|item.id==plan.catalog_id && item.status=="active")
                && !self.state.ready(&plan.epoch,&warmup.candidate_key,frontier) {return Ok(());}
        }
        // The owner has joined all active commits. This short CAS is the slot
        // barrier; no historical fetch or detector-frame borrow enters it.
        let mut expected=Vec::new();
        for plan in &warmup.plans {
            let key=instrument_key(&plan.instrument);
            let checkpoint=continuous::get_adaptive_checkpoint(pool,warmup.lease.slot_id,&key).await?;
            if checkpoint.as_ref().is_some_and(|row|row.state["nextOpenUnix"].as_i64()!=Some(frontier)) {return Ok(());}
            expected.push(live::CheckpointExpectation {instrument_key:key,checkpoint});
        }
        let mut initial=Vec::new();
        for plan in warmup.plans.iter().filter(|plan|plan.specs.iter().any(|spec|spec.key()==warmup.candidate_key)) {
            let checkpoint=self.state.checkpoint_for(&plan.epoch,&warmup.candidate_key).ok_or_else(||invalid("ready candidate checkpoint missing"))?;
            if checkpoint.next_open_unix!=frontier {return Ok(());}
            initial.push(live::InitialAdaptiveCheckpoint {instrument_key:instrument_key(&plan.instrument),state:serde_json::to_value(checkpoint)?});
        }
        if !live::report_warmup(pool,&warmup.lease,"ready",Some(frontier),None).await? {self.discard_warmup();return Ok(());}
        let result=promotion::persist(&promotion::PgPromotion {pool,lease:&warmup.lease,
            predecessor:warmup.pending.expected_active_generation_id,expected:&expected,frontier,initial:&initial}).await;
        match result {
            Ok(promotion::Outcome::Adopt)=>{
                let warmup=self.warmup.take().unwrap();
                for plan in &warmup.plans {
                    if plan.specs.iter().any(|spec|spec.key()==warmup.candidate_key) {self.state.activate(&plan.epoch,&warmup.candidate_key,frontier)?;}
                    for spec in plan.specs.iter().filter(|spec|spec.fence.slot_id==warmup.lease.slot_id && spec.key()!=warmup.candidate_key) {
                        self.state.remove_profile(&plan.epoch,&spec.key());
                    }
                }
            },
            Ok(promotion::Outcome::Rebuild)=>{
                let warmup=self.warmup.take().unwrap();
                for plan in &warmup.plans {self.state.invalidate(&plan.epoch,None);}
            },
            Ok(promotion::Outcome::Lost)=>self.discard_warmup(),
            Ok(promotion::Outcome::Retry)=>{
                let warmup=self.warmup.as_mut().unwrap();warmup.barrier_attempts+=1;
                if warmup.barrier_attempts>=3 {self.discard_warmup();}
            },
            Err(error)=>{
                // Do not leave a possibly committed candidate missing from a
                // nonempty production ring. Reconstruct from durable fences.
                if let Some(warmup)=&self.warmup {for plan in &warmup.plans {self.state.invalidate(&plan.epoch,None);}}
                return Err(error);
            },
        }
        Ok(())
    }
}

fn validate_history_page(bars:&[DetectorBar],from:i64,to:i64,next:Option<i64>)->Result<(),WorkerError> {
    if bars.len()>HISTORY_PAGE_LIMIT as usize {return Err(WorkerError::PageLimitExceeded(bars.len()));}
    let mut previous=None;
    for bar in bars {
        if bar.open_time_unix<from || bar.open_time_unix>=to || bar.open_time_unix%60!=0
            || previous.is_some_and(|time|bar.open_time_unix<=time) {return Err(invalid("malformed history pagination"));}
        previous=Some(bar.open_time_unix);
    }
    if next.is_some() && (next!=previous || bars.is_empty()) {return Err(invalid("nonadvancing history pagination"));}
    Ok(())
}

async fn process_page(state:&mut AdaptiveRuntime,pool:&PgPool,plan:&SymbolPlan,bars:&[DetectorBar],target:i64,complete:bool)->Result<(),WorkerError> {
    if bars.is_empty() {process_chunk(state,pool,plan,&[],target,complete).await?;}
    for (index,chunk) in bars.chunks(ADAPTIVE_PAGE_LIMIT).enumerate() {
        process_chunk(state,pool,plan,chunk,target,complete && (index+1)*ADAPTIVE_PAGE_LIMIT>=bars.len()).await?;
    }
    Ok(())
}
async fn process_chunk(state:&mut AdaptiveRuntime,pool:&PgPool,plan:&SymbolPlan,bars:&[DetectorBar],target:i64,complete:bool)->Result<(),WorkerError> {
    let prepared=match state.prepare(&plan.epoch,&plan.instrument,bars,target,complete) {
        Ok(prepared)=>prepared,
        Err(error)=>{state.invalidate(&plan.epoch,None);return Err(error);}
    };
    let mut tasks=tokio::task::JoinSet::new();
    for prepared in prepared {
        if !prepared.should_commit() {state.accept(&plan.epoch,prepared,false);continue;}
        let slot=plan.slots.get(&prepared.key).cloned().ok_or_else(||invalid("active store missing"))?;
        let pool=pool.clone();let instrument=plan.instrument.clone();
        tasks.spawn(async move {
            let mut prepared=prepared;
            let store=PgAdaptiveStore {pool:&pool,slot:&slot};
            let result=store::persist(&store,&instrument,&mut prepared).await;
            (prepared,result)
        });
        if tasks.len()>=COMMIT_CONCURRENCY {accept_result(state,&plan.epoch,tasks.join_next().await.unwrap())?;}
    }
    while let Some(result)=tasks.join_next().await {accept_result(state,&plan.epoch,result)?;}
    Ok(())
}
fn accept_result(state:&mut AdaptiveRuntime,epoch:&str,result:Result<(PreparedProfile,Result<bool,WorkerError>),tokio::task::JoinError>)->Result<(),WorkerError> {
    let (prepared,result)=result.map_err(|e|invalid(&e.to_string()))?;
    match result {
        Ok(true)=>state.accept(epoch,prepared,true),
        Ok(false)=>state.invalidate(epoch,Some(&prepared.key)),
        Err(error)=>{state.invalidate(epoch,Some(&prepared.key));return Err(error);}
    }
    Ok(())
}

/// Compatibility helper for isolated tests/callers; production keeps AdaptiveLiveRuntime
/// across turns. `last_minute` is observational only, never a suppression gate.
pub async fn adaptive_live_tick(pool:&PgPool,market_url:&str,market_key:&str,stopped:&AtomicBool,last_minute:&mut i64)->Result<(),WorkerError> {
    let mut owner=AdaptiveLiveRuntime::new(AdaptiveRuntimeLimits::default());
    owner.tick(pool,&MarketDataClient::new(market_url,market_key),stopped).await?;
    *last_minute=chrono::Utc::now().timestamp().div_euclid(60)*60;
    Ok(())
}

pub(crate) async fn adaptive_live_loop(pool:PgPool,market_url:String,market_key:String,stopped:Arc<AtomicBool>,notify:Arc<tokio::sync::Notify>) {
    let scan_pool=pool.clone();let scan_stopped=stopped.clone();let scan_notify=notify.clone();
    let scan=async move {
        let client=MarketDataClient::new(market_url,market_key);
        let mut owner=AdaptiveLiveRuntime::new(AdaptiveRuntimeLimits::default());
        while !scan_stopped.load(Ordering::SeqCst) {
            if let Err(error)=owner.tick(&scan_pool,&client,&scan_stopped).await {
                if let Some(warmup)=&owner.warmup {
                    let report=json!({"code":"adaptive_warmup_failed","message":error.to_string().chars().take(240).collect::<String>()});
                    let _=live::report_warmup(&scan_pool,&warmup.lease,"error",None,Some(&report)).await;
                    owner.discard_warmup();
                }
                eprintln!("[adaptive-live] tick failed: {}",error.to_string().chars().take(240).collect::<String>());
            }
            tokio::select! {_=scan_notify.notified()=>{},_=tokio::time::sleep(Duration::from_secs(2))=>{}}
        }
        owner.state.clear();
    };
    tokio::join!(scan,delivery::run(pool,stopped,notify));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adaptive::state::tests::{fixture,feed};
    #[tokio::test]
    async fn independent_epoch_proofs_keep_long_revision_two_and_short_revision_one_valid() {
        use axum::{Json,Router,routing::post};
        use setups_mdclient::TailReadToken;
        let mut f=fixture();f.spec.bootstrap_next=60;f.spec.emit_from_close=120;
        let digest=setups_optimizer::detector_prefix_fingerprint(&f.bars[..1],0,60).unwrap();
        f.spec.prefix.bar_count=digest.bar_count;f.spec.prefix.sha256=digest.sha256;
        f.runtime.install(&f.epoch,std::slice::from_ref(&f.spec)).unwrap();
        feed(&mut f.runtime,&f.epoch,&f.instrument,&f.bars[..2],120).unwrap();
        f.runtime.acknowledge_source(&f.epoch,TailReadToken {epoch:"proof-fixture".into(),revision:"2".into()}).unwrap();
        let staging=AdaptiveRuntime::epoch_key(&f.instrument,Some(2));let mut candidate=f.spec.clone();
        candidate.fence.generation_id=2;candidate.pending=true;
        f.runtime.install(&staging,std::slice::from_ref(&candidate)).unwrap();
        feed(&mut f.runtime,&staging,&f.instrument,&f.bars[..1],60).unwrap();
        f.runtime.acknowledge_source(&staging,TailReadToken {epoch:"proof-fixture".into(),revision:"1".into()}).unwrap();
        let active=f.runtime.source_guard(&f.epoch,1).unwrap();let pending=f.runtime.source_guard(&staging,1).unwrap();
        assert_eq!((active.from_unix,active.to_unix,active.previous.unwrap().revision),(0,120,"2".into()));
        assert_eq!((pending.from_unix,pending.to_unix,pending.previous.unwrap().revision),(0,60,"1".into()));
        let requests=Arc::new(std::sync::Mutex::new(Vec::new()));let observed=requests.clone();
        let app=Router::new().route("/v1/candles/tail-batch",post(move |Json(request):Json<Value>| {
            let observed=observed.clone();async move {
                let guard=&request["readGuards"][0];let revision=guard["previous"]["revision"].as_str().unwrap().parse::<i64>().unwrap();
                let from=guard["fromUnix"].as_i64().unwrap();let to=guard["toUnix"].as_i64().unwrap();
                let head={let mut requests=observed.lock().unwrap();requests.push((from,to,revision));if requests.len()==1 {2}else{3}};
                // Revision 2 inserts OPEN 60. After canonical processing,
                // revision 3 corrects that same bar before staging observes it.
                let status=if revision<head && from<=60 && 60<to {"invalid"}else{"valid"};
                Json(json!({"checkedAt":chrono::Utc::now().to_rfc3339(),"instruments":[{"instrumentId":1,"status":"known",
                    "latestStoredOpenTimeUnix":60,"contiguousThroughOpenTimeUnix":60,"missingRanges":[],
                    "candles":[{"instrumentId":1,"openTimeUnix":60,"open":"100","high":"100","low":"100","close":"100","volume":"1","quoteTurnover":if head==2 {"1"}else{"2"}}],
                    "readGuard":{"status":status,"token":{"epoch":"proof-fixture","revision":head.to_string()}}}]}))
            }
        }));
        let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();let url=format!("http://{}",listener.local_addr().unwrap());
        let server=tokio::spawn(async move {axum::serve(listener,app).await.unwrap();});
        let mut owner=AdaptiveLiveRuntime::new(AdaptiveRuntimeLimits::default());owner.state=f.runtime;
        let plans=vec![SymbolPlan {epoch:f.epoch.clone(),instrument:f.instrument.clone(),catalog_id:1,specs:vec![f.spec],slots:BTreeMap::new()},
            SymbolPlan {epoch:staging.clone(),instrument:f.instrument.clone(),catalog_id:1,specs:vec![candidate],slots:BTreeMap::new()}];
        let pool=sqlx::postgres::PgPoolOptions::new().connect_lazy("postgres://test:test@127.0.0.1:1/unreachable_test").unwrap();
        let result=tokio::time::timeout(Duration::from_millis(500),owner.hot_tail(&pool,&MarketDataClient::new(url,"fixture"),&plans,120,&AtomicBool::new(false))).await.unwrap();
        assert!(result.is_ok(),"both original proofs are valid; their oldest-token/union-range merge is not: {result:?}");
        assert_eq!(*requests.lock().unwrap(),vec![(0,120,2),(0,60,1)]);
        assert_eq!(owner.state.source_guard(&f.epoch,1).unwrap().previous.unwrap().revision,"2",
            "staging's later snapshot did NOT prove canonical's consumed OPEN 60 and must never acknowledge its revision");
        assert_eq!(owner.state.source_guard(&staging,1).unwrap().previous.unwrap().revision,"3");
        assert_eq!(owner.state.retained_history_epochs(),2);
        server.abort();let _=server.await;pool.close().await;
    }

    #[tokio::test]
    async fn trailing_repair_rejection_invalidates_only_affected_symbol_before_database_io() {
        let mut f=fixture();f.runtime.install(&f.epoch,std::slice::from_ref(&f.spec)).unwrap();
        feed(&mut f.runtime,&f.epoch,&f.instrument,&f.bars,f.frontier).unwrap();
        feed(&mut f.runtime,&f.epoch,&f.instrument,&[],f.frontier+120).unwrap();
        crate::adaptive::state::tests::mark_legacy_consumed(&mut f.runtime,&f.epoch,f.frontier+120);
        let mut other=f.instrument.clone();other.symbol="OTHERUSDT".into();
        let other_epoch=AdaptiveRuntime::epoch_key(&other,None);
        let mut other_spec=f.spec.clone();other_spec.prefix.instrument.symbol=other.symbol.clone();
        f.runtime.install(&other_epoch,&[other_spec]).unwrap();
        feed(&mut f.runtime,&other_epoch,&other,&f.bars,f.frontier).unwrap();
        let other_bytes=f.runtime.retained_history_bytes();
        let pool=sqlx::postgres::PgPoolOptions::new().connect_lazy("postgres://test:test@127.0.0.1:1/unreachable_test").unwrap();
        let plan=SymbolPlan {epoch:f.epoch.clone(),instrument:f.instrument.clone(),catalog_id:1,specs:Vec::new(),slots:BTreeMap::new()};
        let late=DetectorBar {open_time_unix:f.frontier+60,close:"99".into(),quote_turnover:"50".into()};
        let result=tokio::time::timeout(Duration::from_millis(250),process_chunk(&mut f.runtime,&pool,&plan,&[late],f.frontier+120,true))
            .await.expect("must reject before acquiring any database connection");
        assert!(result.is_err());assert!(f.runtime.profile_epoch(&f.instrument,&f.spec.key()).is_none());
        assert!(f.runtime.retained_history_bytes()>0 && f.runtime.retained_history_bytes()<other_bytes);
        assert_eq!(f.runtime.profile_epoch(&other,&f.spec.key()),Some(other_epoch));
        pool.close().await;
    }
}
