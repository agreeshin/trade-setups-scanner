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
mod schedule;
mod batch;
mod promotion;
mod text;
mod caption;
mod enrichment;
mod diagnostics;
mod supervision;
use setups_db::diagnostics::{self as lifecycle,Level};
use profile::{LiveProfile,RecommendationEnvelope};
pub use store::PgAdaptiveStore;
pub use text::recommendation_message;
pub use caption::{AdaptiveTelegramCaption, recommendation_telegram_caption};
pub use enrichment::{AdaptiveExpectancySnapshot, ExpectancyOverlayKey};

const HISTORY_PAGE_LIMIT:u32=5000;
const IO_CONCURRENCY:usize=4;
const COMMIT_CONCURRENCY:usize=16;
fn cold_commit_concurrency(pool:&PgPool)->Result<usize,WorkerError> {
    // This pool is data-only. Control owns a separate connection in the same
    // configured live total; pool2 therefore gives us a valid max1 data pool.
    let capacity=pool.options().get_max_connections();
    if capacity==0 {return Err(invalid("cold persistence requires a nonempty data pool"));}
    Ok(COMMIT_CONCURRENCY.min(capacity as usize))
}
const WARMUP_TIME_SLICE:Duration=Duration::from_secs(1);
const JOIN_MAX_PER_TURN:usize=8;
const JOIN_TIME_SLICE:Duration=Duration::from_millis(100);

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
fn current_preparation_requirements<'a>(plans:impl Iterator<Item=&'a SymbolPlan>)->BTreeMap<i64,setups_mdclient::HistoryDemand> {
    let mut required:BTreeMap<i64,setups_mdclient::HistoryDemand>=BTreeMap::new();
    for plan in plans.filter(|plan|plan.catalog_id>0 && plan.specs.iter().any(|spec|spec.publish_enabled)) {
        for spec in &plan.specs {
            required.entry(plan.catalog_id).and_modify(|range| {
                range.from_unix=range.from_unix.min(spec.from_open);
                range.to_unix=range.to_unix.max(spec.warmup_through);
            }).or_insert(setups_mdclient::HistoryDemand {instrument_id:plan.catalog_id,from_unix:spec.from_open,to_unix:spec.warmup_through});
        }
    }
    required
}
fn preparation_source_ready<'a>(state:&AdaptiveRuntime,plans:impl Iterator<Item=&'a SymbolPlan>,id:i64,target:i64)->bool {
    let mut found=false;
    for plan in plans.filter(|plan|plan.catalog_id==id) {
        for spec in plan.specs.iter().filter(|spec|spec.publish_enabled) {
            found=true;
            if !state.ready(&plan.epoch,&spec.key(),target) {return false;}
        }
    }
    found
}
struct Warmup {
    pending:live::PendingAdaptiveSlot,
    lease:live::WarmupLease,
    supervisor:supervision::Supervisor,
    plans:Vec<SymbolPlan>,
    candidate_key:String,
    cohort:BTreeSet<String>,
    barrier_attempts:u32,
    // At most one bounded disposition per declared old/new scope member.
    waiting:BTreeMap<String,String>,
    retired:BTreeMap<String,String>,
    started:Instant,
    progress_reported_at:Instant,
}
pub struct AdaptiveLiveRuntime {
    state:AdaptiveRuntime,
    warmup:Option<Warmup>,
    worker:String,
    next_pending:usize,
    cold_schedule:schedule::ColdSchedule,
    join_cursor:usize,
    peak_staging_epochs:usize,
    preparation:crate::preparation::Preparation,
    diagnostic_summary_at:Instant,
    admission_wait_reported:bool,
}
impl AdaptiveLiveRuntime {
    pub fn new(limits:AdaptiveRuntimeLimits)->Self {
        Self {state:AdaptiveRuntime::new(limits),warmup:None,
            worker:format!("adaptive-live-{}-{:016x}",std::process::id(),rand::random::<u64>()),next_pending:0,cold_schedule:schedule::ColdSchedule::default(),join_cursor:0,peak_staging_epochs:0,preparation:Default::default(),diagnostic_summary_at:Instant::now(),admission_wait_reported:false}
    }
    pub fn peak_staging_epochs(&self)->usize {self.peak_staging_epochs}
    pub fn retained_history_epochs(&self)->usize {self.state.retained_history_epochs()}
    pub fn retained_history_bytes(&self)->usize {self.state.retained_history_bytes()}
    pub fn retained_profile_bytes(&self)->usize {self.state.retained_profile_bytes()}
    pub fn preparation_status(&self)->Value {self.preparation.status()}
    async fn active_plans(&mut self,pool:&PgPool,slots:&[Arc<ActiveAdaptiveSlot>],catalog:&[CatalogInstrument])->Result<Vec<SymbolPlan>,WorkerError> {
        if slots.len()>self.state.limits.max_profiles {return Err(invalid("active live slot count exceeds admission budget"));}
        let mut plans:BTreeMap<String,SymbolPlan>=BTreeMap::new();
        for slot in slots {
            let profile=LiveProfile::parse(slot)?;
            for scoped in &profile.instruments {
                let matches:Vec<_>=catalog.iter().filter(|c|c.status=="active" && c.exchange==scoped.exchange
                    && c.category==scoped.category && c.symbol==scoped.symbol).collect();
                if matches.len()>1 {return Err(invalid("ambiguous active listing"));}
                let Some(item)=matches.first() else {continue;};
                let instrument=instrument(item);
                let key=profile.fence_key();
                let epoch=self.state.profile_epoch(&instrument,&key).unwrap_or_else(||AdaptiveRuntime::epoch_key(&instrument,None));
                let checkpoint=match self.state.cached_checkpoint(&epoch,&key) {
                    Some(checkpoint)=>checkpoint,
                    None=>store::load_checkpoint(pool,slot.slot_id,&instrument).await?,
                };
                let spec=profile.spec(scoped,checkpoint,false)?;
                let plan=plans.entry(epoch.clone()).or_insert_with(||SymbolPlan {epoch,instrument,catalog_id:item.id,specs:Vec::new(),slots:BTreeMap::new()});
                plan.slots.insert(spec.key(),slot.clone());plan.specs.push(spec);
            }
        }
        Ok(plans.into_values().collect())
    }
    async fn build_warmup_plans(&self,pool:&PgPool,candidate:&live::PendingAdaptiveSlot,active:&[SymbolPlan],slots:&[Arc<ActiveAdaptiveSlot>],catalog:&[CatalogInstrument],permit:&supervision::Permit)->Result<(Vec<SymbolPlan>,String),WorkerError> {
        let profile=LiveProfile::parse(&candidate.profile)?;
        let candidate_key=profile.fence_key();
        let incumbent=slots.iter().find(|slot|slot.slot_id==candidate.profile.slot_id).cloned();
        let old=incumbent.as_ref().map(|slot|LiveProfile::parse(slot)).transpose()?;
        let mut scope:BTreeSet<_>=profile.instruments.iter().cloned().collect();
        if let Some(old)=&old {scope.extend(old.instruments.iter().cloned());}
        let mut plans=Vec::new();
        for scoped in scope {
            permit.check()?;
            let matches:Vec<_>=catalog.iter().filter(|item|item.exchange==scoped.exchange && item.category==scoped.category && item.symbol==scoped.symbol).collect();
            if matches.len()>1 {return Err(invalid("ambiguous pending source catalog identity"));}
            let item=matches.first().copied();
            let instrument=AdaptiveInstrument {exchange:scoped.exchange.clone(),category:scoped.category.clone(),symbol:scoped.symbol.clone()};
            let listed=item.is_some_and(|item|item.status=="active");
            let candidate_seed=profile.instruments.iter().find(|seed|**seed==scoped);
            let epoch=AdaptiveRuntime::epoch_key(&instrument,if candidate_seed.is_none() && listed {None}else{Some(candidate.profile.generation_id)});
            let mut specs:BTreeMap<String,ProfileSpec>=BTreeMap::new();let mut stores=BTreeMap::new();
            for plan in active.iter().filter(|plan|plan.instrument==instrument) {
                for spec in &plan.specs {let mut shadow=spec.clone();shadow.pending=epoch!=AdaptiveRuntime::epoch_key(&instrument,None);specs.insert(shadow.key(),shadow);}
                stores.extend(plan.slots.clone());
            }
            if let (Some(old),Some(slot))=(&old,&incumbent)
                && let Some(seed)=old.instruments.iter().find(|seed|**seed==scoped)
                && !specs.contains_key(&old.fence_key()) {
                let checkpoint=store::load_checkpoint(pool,slot.slot_id,&instrument).await?;
                let mut shadow=old.spec(seed,checkpoint,true)?;shadow.publish_enabled=listed;
                stores.insert(shadow.key(),slot.clone());specs.insert(shadow.key(),shadow);
            }
            if let Some(seed)=candidate_seed {
                let mut pending=profile.spec(seed,None,true)?;pending.publish_enabled=listed;
                specs.insert(pending.key(),pending);
            }
            plans.push(SymbolPlan {epoch,instrument,catalog_id:item.map_or(0,|item|item.id),specs:specs.into_values().collect(),slots:stores});
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
    async fn refresh_warmup(&mut self,pool:&PgPool,data_pool:&PgPool,pending:Vec<live::PendingAdaptiveSlot>,catalog:&[CatalogInstrument],active:&[SymbolPlan],slots:&[Arc<ActiveAdaptiveSlot>],target:i64)->Result<(),WorkerError> {
        let cohort:BTreeSet<_>=slots.iter().map(|slot|json!([slot.owner_id,slot.algorithm_id,slot.slot_id,slot.generation_id,slot.config_revision]).to_string()).collect();
        if let Some(warmup)=&self.warmup {
            let current=warmup.cohort==cohort && pending.iter().any(|p|p.profile.slot_id==warmup.lease.slot_id && p.profile.generation_id==warmup.lease.generation_id
                && p.profile.config_revision==warmup.lease.config_revision);
            warmup.supervisor.permit().check()?;
            if current {
                // A waiting sibling may have joined while this candidate was
                // warming. Refresh only durable control fields, NEVER origins
                // or reconstructed scalar state from the candidate's recipe.
                let warmup=self.warmup.as_mut().unwrap();
                let all_waiting=warmup.plans.iter().filter(|p|p.specs.iter().any(|s|s.key()==warmup.candidate_key))
                    .all(|p|warmup.waiting.contains_key(&instrument_key(&p.instrument)));
                if all_waiting {
                    // Recheck recoverable quality/tail waits on the next turn,
                    // but only after every declared candidate had a fair turn.
                    warmup.waiting.retain(|_,reason|matches!(reason.as_str(),"not_listed"|"unavailable_catalog"));
                }
                for plan in &mut warmup.plans {
                    for spec in plan.specs.iter_mut().filter(|spec|spec.key()!=warmup.candidate_key) {
                        if let Some((fresh,slot))=active.iter().filter(|p|p.instrument==plan.instrument)
                            .find_map(|p|p.specs.iter().find(|s|s.key()==spec.key()).map(|s|(s,p.slots.get(&s.key())))) {
                            spec.waiting=fresh.waiting;spec.emit_from_close=fresh.emit_from_close;
                            if let Some(slot)=slot {plan.slots.insert(spec.key(),slot.clone());}
                        }
                    }
                    if self.state.has_epoch(&plan.epoch) {self.state.install(&plan.epoch,&plan.specs)?;}
                }
                return Ok(());
            }
            live::release_pending_warmup(pool,&warmup.lease).await?;
            self.discard_warmup("cohort_or_candidate_changed").await?;
        }
        if pending.is_empty() {self.admission_wait_reported=false;return Ok(());}
        for offset in 0..pending.len() {
            let index=(self.next_pending+offset)%pending.len();
            let mut candidate=pending[index].clone();
            let Some(lease)=live::claim_pending_warmup_at(pool,&candidate,&self.worker,target).await? else {continue;};
            let mut supervisor=match supervision::Supervisor::start(pool,&lease) {
                Ok(supervisor)=>supervisor,
                Err(error)=>{
                    let report=live::report_warmup_failure(pool,&lease,&json!({"code":"warmup_supervisor_start_failed"}),true).await;
                    let (error,fields)=diagnostics::failure_report(error,&report,live::warmup_diagnostic_fields(&lease));
                    lifecycle::emit(Level::Error,"adaptive_warmup_failure_report",fields);return Err(error);
                },
            };
            // The claim, not the pre-claim pending DTO, owns the stable anchor.
            candidate.profile.live_initialization=Some(lease.initialization.clone());
            self.next_pending=(index+1)%pending.len();
            self.admission_wait_reported=false;
            let started=Instant::now();
            let mut fields=live::warmup_diagnostic_fields(&lease);
            fields["study_id"]=candidate.profile.study_id.into();fields["stage"]="plan_build".into();
            fields["outcome"]="started".into();lifecycle::emit(Level::Info,"adaptive_warmup_started",fields);
            let build=self.build_warmup_plans(data_pool,&candidate,active,slots,catalog,supervisor.permit()).await.and_then(|built| {
                supervisor.permit().check()?;Ok(built)
            });
            match build {
                Ok((plans,candidate_key))=>{
                    let waiting=plans.iter().filter(|p|p.specs.iter().any(|s|s.key()==candidate_key) && !p.specs.iter().any(|s|s.publish_enabled))
                        .map(|p|(instrument_key(&p.instrument),if p.catalog_id==0 {"unavailable_catalog".into()}else{"not_listed".into()})).collect();
                    let retired=plans.iter().filter(|p|!p.specs.iter().any(|s|s.key()==candidate_key) && !p.specs.iter().any(|s|s.publish_enabled))
                        .map(|p|(instrument_key(&p.instrument),if p.catalog_id==0 {"unavailable_catalog".into()}else{"not_listed".into()})).collect();
                    self.warmup=Some(Warmup {pending:candidate,lease,supervisor,plans,candidate_key,cohort:cohort.clone(),barrier_attempts:0,waiting,retired,started,progress_reported_at:started});
                    let mut fields=diagnostics::warmup_fields(self.warmup.as_ref().unwrap(),&self.state,target);
                    fields["stage"]="plan_build".into();fields["outcome"]="admitted".into();
                    lifecycle::emit(Level::Info,"adaptive_warmup_plan_finished",fields);
                },
                Err(error)=>{
                    if let Err(cleanup)=supervisor.stop().await {
                        lifecycle::emit(Level::Error,"live_supervision_join_failed",json!({"runtime_instance":self.worker,"error_code":diagnostics::error_code(&cleanup)}));
                    }
                    let retryable=matches!(error,WorkerError::WarmupControl(_));
                    let report=live::report_warmup_failure(pool,&lease,&json!({"code":if retryable {"warmup_supervision_lost"}else{"invalid_live_candidate"},"message":error.to_string().chars().take(240).collect::<String>()}),retryable).await;
                    let mut fields=live::warmup_diagnostic_fields(&lease);
                    fields["study_id"]=candidate.profile.study_id.into();fields["stage"]="plan_build".into();
                    fields["duration_ms"]=(started.elapsed().as_millis() as u64).into();
                    let (error,fields)=diagnostics::failure_report(error,&report,fields);
                    lifecycle::emit(Level::Error,"adaptive_warmup_failure_report",fields);return Err(error);
                }
            }
            break;
        }
        if self.warmup.is_none() && !self.admission_wait_reported {
            self.admission_wait_reported=true;
            lifecycle::emit(Level::Info,"adaptive_warmup_waiting",json!({"runtime_instance":self.worker,"stage":"admission",
                "outcome":"no_candidate_claimed","pending_candidates":pending.len()}));
        }
        Ok(())
    }
    fn check_supervision(&self)->Result<(),WorkerError> {
        self.warmup.as_ref().map_or(Ok(()),|w|w.supervisor.permit().check())
    }
    fn candidate_cursor(&self,plan:&SymbolPlan)->Option<i64> {
        let w=self.warmup.as_ref()?;
        if !plan.specs.iter().any(|s|s.key()==w.candidate_key) {return None;}
        self.state.checkpoint_for(&plan.epoch,&w.candidate_key).map(|checkpoint|checkpoint.next_open_unix)
    }
    fn candidate_progress(&self,plan:&SymbolPlan,previous:Option<i64>) {
        if let Some(w)=&self.warmup && let (Some(previous),Some(next))=(previous,self.candidate_cursor(plan))
            && next>previous {w.supervisor.permit().progress();}
    }
    async fn discard_warmup(&mut self,reason:&'static str)->Result<(),WorkerError> {
        if let Some(mut warmup)=self.warmup.take() {
            let mut fields=diagnostics::warmup_fields(&warmup,&self.state,chrono::Utc::now().timestamp().div_euclid(60)*60);
            fields["stage"]="cleanup".into();fields["reason"]=reason.into();
            let stopped=warmup.supervisor.stop().await;
            for plan in warmup.plans {
                if plan.epoch==AdaptiveRuntime::epoch_key(&plan.instrument,None) {self.state.remove_profile(&plan.epoch,&warmup.candidate_key);}
                else {self.state.invalidate(&plan.epoch,None);}
            }
            fields["outcome"]=if stopped.is_ok(){"drained"}else{"supervisor_join_error"}.into();
            fields["history_bytes_after"]=self.retained_history_bytes().into();
            lifecycle::emit(if stopped.is_ok(){Level::Info}else{Level::Error},"adaptive_warmup_finished",fields);
            stopped?;
        }
        Ok(())
    }
    /// The caller has drained its tick/read/commit futures. This only releases
    /// the pending claim; incumbent checkpoints/generation and delivery survive.
    pub async fn shutdown_warmup(&mut self,pool:&PgPool)->Result<bool,WorkerError> {
        let Some(warmup)=self.warmup.as_mut() else {return Ok(false);};
        warmup.supervisor.stop().await?;
        let released=live::release_pending_warmup(pool,&warmup.lease).await?;
        let mut fields=live::warmup_diagnostic_fields(&warmup.lease);
        fields["release_outcome"]=if released {"released_and_refunded"}else{"expired_or_fenced_not_refunded"}.into();
        lifecycle::emit(Level::Info,"adaptive_shutdown_claim_finished",fields);
        self.discard_warmup("graceful_shutdown").await?;Ok(released)
    }
    fn report_warmup_progress(&mut self,target:i64) {
        if let Some(warmup)=self.warmup.as_mut() && warmup.progress_reported_at.elapsed()>=Duration::from_secs(30) {
            warmup.progress_reported_at=Instant::now();
            let mut fields=diagnostics::warmup_fields(warmup,&self.state,target);fields["outcome"]="in_progress".into();
            lifecycle::emit(Level::Info,"adaptive_warmup_progress",fields);
        }
    }
    fn cold_plans(&self,active:&[SymbolPlan],target:i64,completed:&BTreeSet<String>)->Vec<SymbolPlan> {
        let mut cold:BTreeMap<String,SymbolPlan>=active.iter().filter(|p|!self.state.is_current(&p.epoch,target-1800) && !completed.contains(&p.epoch))
            .map(|p|(p.epoch.clone(),p.clone())).collect();
        if let Some(warmup)=&self.warmup {
            let mut resident=warmup.plans.iter().filter(|p|p.epoch!=AdaptiveRuntime::epoch_key(&p.instrument,None) && self.state.has_epoch(&p.epoch)).count();
            for plan in &warmup.plans {
                if !plan.specs.iter().any(|s|s.publish_enabled) {continue;}
                if warmup.waiting.contains_key(&instrument_key(&plan.instrument)) {continue;}
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
    fn park_waiting(&mut self,plan:&SymbolPlan,frontier:i64) {
        let Some(warmup)=self.warmup.as_mut() else {return;};
        if !plan.specs.iter().any(|spec|spec.key()==warmup.candidate_key) {return;}
        let Some(reason)=self.state.waiting_reason(&plan.epoch,&warmup.candidate_key,frontier) else {return;};
        warmup.waiting.insert(instrument_key(&plan.instrument),reason.into());
        lifecycle::emit(Level::Debug,"adaptive_warmup_instrument_waiting",json!({"study_id":warmup.pending.profile.study_id,
            "generation_id":warmup.lease.generation_id,"profile_id":warmup.lease.slot_id,"attempt":warmup.lease.attempt,
            "instrument_key":instrument_key(&plan.instrument),"history_epoch":plan.epoch,"reason":reason,"frontier":frontier}));
        // Release duplicate staging rings so waiting instruments cannot block
        // admission. An already adopted candidate instead owns bounded scalar
        // state in canonical history: keep its verified origin/proof and muted
        // pending flag. Removing it would make refresh attach a new profile to
        // populated incumbent history. Promotion/join activates it; discard and
        // reconcile release it without rebuilding the healthy incumbent.
        if plan.epoch!=AdaptiveRuntime::epoch_key(&plan.instrument,None) {
            self.state.invalidate(&plan.epoch,None);
        }
    }
    async fn join_ready_waiting(&mut self,pool:&PgPool,plans:&[SymbolPlan],frontier:i64)->Result<(),WorkerError> {
        let eligible:Vec<_>=plans.iter().flat_map(|plan|plan.specs.iter().filter(|spec|spec.waiting)
            .filter(|spec|self.state.ready(&plan.epoch,&spec.key(),frontier)).map(move |spec|(plan,spec))).collect();
        let started=Instant::now();
        for _ in 0..JOIN_MAX_PER_TURN.min(eligible.len()) {
                if started.elapsed()>=JOIN_TIME_SLICE {break;}
                let (plan,spec)=eligible[self.join_cursor%eligible.len()];
                // Advance before IO: one failed join cannot reset the next turn
                // to the first instrument. The slice does not abort owned IO.
                self.join_cursor=(self.join_cursor+1)%eligible.len();
                if !self.state.has_epoch(&plan.epoch) {continue;}
                let slot=plan.slots.get(&spec.key()).ok_or_else(||invalid("waiting instrument store missing"))?;
                let key=instrument_key(&plan.instrument);
                let previous=continuous::get_adaptive_checkpoint(pool,slot.slot_id,&key).await?;
                let checkpoint=self.state.checkpoint_for(&plan.epoch,&spec.key()).ok_or_else(||invalid("waiting live state missing"))?;
                if store::join(&store::PgJoin {pool,slot,instrument_key:&key,previous:previous.as_ref(),frontier,state:&serde_json::to_value(&checkpoint)?}).await? {
                    // Preserve the detector/cluster just warmed. Only its
                    // publication floor and durable ownership change.
                    self.state.activate(&plan.epoch,&spec.key(),frontier)?;
                } else {self.state.invalidate(&plan.epoch,None);break;}
        }
        Ok(())
    }
    async fn adopt_ready_staging(&mut self,pool:&PgPool,frontier:i64)->Result<(),WorkerError> {
        self.check_supervision()?;
        let Some(warmup)=&self.warmup else {return Ok(());};
        let candidate=warmup.candidate_key.clone();
        let ready:Vec<_>=warmup.plans.iter().filter(|p|p.epoch!=AdaptiveRuntime::epoch_key(&p.instrument,None)
            && self.state.is_current(&p.epoch,frontier)).cloned().collect();
        for plan in ready {
            self.check_supervision()?;
            let mut expected=BTreeMap::new();let mut caught_up=true;
            for spec in plan.specs.iter().filter(|spec|spec.key()!=candidate) {
                let staged=self.state.checkpoint_for(&plan.epoch,&spec.key()).ok_or_else(||invalid("staged incumbent checkpoint missing"))?;
                let durable=store::load_checkpoint(pool,spec.fence.slot_id,&plan.instrument).await?;
                if spec.waiting {
                    let production=AdaptiveRuntime::epoch_key(&plan.instrument,None);
                    // A waiting sibling has no publication authority yet.
                    // Require exact canonical scalar AND durable predecessor
                    // parity, without fabricating a committed checkpoint.
                    if self.state.checkpoint_for(&production,&spec.key()).as_ref()!=Some(&staged)
                        || self.state.cached_checkpoint(&production,&spec.key())!=Some(durable) {caught_up=false;break;}
                    expected.insert(spec.key(),staged);continue;
                }
                if durable.as_ref()!=Some(&staged) {
                    caught_up=false;break;
                }
                expected.insert(spec.key(),staged);
            }
            if !caught_up {continue;}
            self.check_supervision()?;
            let production=AdaptiveRuntime::epoch_key(&plan.instrument,None);
            if !self.state.adopt_staging(&plan.epoch,&production,&candidate,&expected)? {
                if let Some(warmup)=&self.warmup {live::release_pending_warmup(pool,&warmup.lease).await?;}
                self.discard_warmup("staging_cohort_changed").await?;break;
            }
            lifecycle::emit(Level::Debug,"adaptive_warmup_history_adopted",json!({"instrument_key":instrument_key(&plan.instrument),
                "history_epoch":production,"stage":"staging_adoption","frontier":frontier}));
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
    /// Isolated single-pool compatibility; production uses tick_at_on_pools
    /// with distinct data/control pools and the actual UTC observation clock.
    pub async fn tick_at(&mut self,pool:&PgPool,client:&MarketDataClient,stopped:&AtomicBool,observation_unix:i64)->Result<(),WorkerError> {
        // Compatibility for isolated callers/tests. Production always supplies
        // distinct pools through tick_at_on_pools, never this shared-pool path.
        self.tick_at_on_pools(pool,pool,client,stopped,observation_unix).await
    }
    pub async fn tick_at_on_pools(&mut self,data_pool:&PgPool,control_pool:&PgPool,client:&MarketDataClient,stopped:&AtomicBool,observation_unix:i64)->Result<(),WorkerError> {
        let pool=control_pool;
        let mut trace=diagnostics::TickTrace::new(&self.worker);
        let result=self.tick_observed(pool,data_pool,client,stopped,observation_unix,&mut trace).await;
        let target=observation_unix.div_euclid(60)*60;
        let level=if result.is_err() {Level::Error}
            else if trace.started.elapsed()>=Duration::from_secs(5) || self.diagnostic_summary_at.elapsed()>=Duration::from_secs(30) {Level::Info}
            else {Level::Debug};
        trace.finish(self,target,level,result.as_ref().err());
        if level!=Level::Debug {self.diagnostic_summary_at=Instant::now();}
        let Err(error)=result else {return Ok(());};
        if let Some(warmup)=&self.warmup {
            let report=json!({"code":"adaptive_warmup_failed","message":error.to_string().chars().take(240).collect::<String>()});
            let reported=live::report_warmup_failure(pool,&warmup.lease,&report,true).await;
            let (error,fields)=diagnostics::failure_report(error,&reported,trace.fields(self,target));
            lifecycle::emit(Level::Error,"adaptive_warmup_failure_report",fields);
            if let Err(cleanup)=self.discard_warmup("tick_failed").await {
                lifecycle::emit(Level::Error,"live_supervision_join_failed",json!({"runtime_instance":self.worker,"error_code":diagnostics::error_code(&cleanup)}));
            }
            return Err(error);
        }
        Err(error)
    }
    // `pool` is exclusively serial control: no data task receives/clones it.
    async fn tick_observed(&mut self,pool:&PgPool,data_pool:&PgPool,client:&MarketDataClient,stopped:&AtomicBool,observation_unix:i64,trace:&mut diagnostics::TickTrace)->Result<(),WorkerError> {
        if stopped.load(Ordering::SeqCst) {return Ok(());}
        self.check_supervision()?;
        continuous::profiles::accept_handoffs(pool,continuous::ADMIN_OWNER).await?;
        trace.enter("settings");
        if settings_get(pool,"live").await?.unwrap_or(json!({}))["enabled"]!=true {
            self.shutdown_warmup(pool).await?;
            self.state.clear();self.preparation.clear();
            continuous::profiles::acknowledge_stops(pool,continuous::ADMIN_OWNER).await?;return Ok(());
        }
        trace.enter("recovery");
        // The independent supervisor owns our claim. Never reap it ahead of
        // renewal or queue its control plane behind research's owner lock.
        if self.warmup.is_none() {live::recover_pending_warmups(pool,continuous::ADMIN_OWNER).await?;}
        let target=observation_unix.div_euclid(60)*60;
        trace.enter("projection");
        let slots:Vec<_>=continuous::active_adaptive_slots(pool,continuous::ADMIN_OWNER).await?.into_iter().map(Arc::new).collect();
        let pending=live::pending_adaptive_slots(pool,continuous::ADMIN_OWNER).await?;
        if slots.is_empty() && pending.is_empty() {self.discard_warmup("no_enabled_profiles").await?;self.state.clear();self.preparation.clear();self.admission_wait_reported=false;continuous::profiles::acknowledge_stops(pool,continuous::ADMIN_OWNER).await?;return Ok(());}
        trace.enter("catalog");
        let catalog=client.instruments().await.map_err(|e|invalid(&e.to_string()))?;
        trace.enter("plan_refresh");
        self.check_supervision()?;
        let active=self.active_plans(data_pool,&slots,&catalog).await?;
        self.refresh_warmup(pool,data_pool,pending,&catalog,&active,&slots,target).await?;
        let mut wanted:BTreeMap<String,BTreeSet<String>>=BTreeMap::new();
        for plan in active.iter().chain(self.warmup.iter().flat_map(|w|w.plans.iter())) {
            wanted.entry(plan.epoch.clone()).or_default().extend(plan.specs.iter().map(ProfileSpec::key));
        }
        self.state.reconcile(&wanted);
        continuous::profiles::acknowledge_stops(pool,continuous::ADMIN_OWNER).await?;
        // One current range per SOURCE, never alternating per-epoch origins.
        // Recomputed from the actual cohort so departed profiles do not leave
        // a min-ever origin or max-ever initialization boundary behind.
        let preparation_requirements=current_preparation_requirements(active.iter().chain(self.warmup.iter().flat_map(|w|w.plans.iter())));
        self.preparation.reconcile_requirements(&preparation_requirements);
        for plan in &active {
            // Re-enabling an already accepted profile has no pending research
            // lease. Never attach it to unchecked existing scalar history:
            // cold-reconstruct the complete current cohort once. Pending
            // generations use the non-disruptive staged path below instead.
            if self.state.needs_reconstruction(&plan.epoch,&plan.specs) {self.state.invalidate(&plan.epoch,None);}
            self.state.install(&plan.epoch,&plan.specs)?;
        }
        let mut hot=active.clone();
        if let Some(warmup)=&self.warmup {hot.extend(warmup.plans.iter().filter(|p|!warmup.waiting.contains_key(&instrument_key(&p.instrument)) && self.state.is_current(&p.epoch,target-1800)).cloned());}
        trace.enter("hot_tail");
        self.hot_tail(data_pool,client,&hot,target,stopped).await?;
        for id in preparation_requirements.keys() {
            if preparation_source_ready(&self.state,active.iter().chain(self.warmup.iter().flat_map(|warmup|warmup.plans.iter())),*id,target) {
                if let Err(code)=self.preparation.retire_ready(*id) {eprintln!("[live-preparation] instrument_id={id} code={code}");}
            }
        }
        // No preparation HTTP is awaited by this owner; hot traffic went first.
        self.preparation.poll(client,stopped.load(Ordering::SeqCst));
        self.adopt_ready_staging(pool,target).await?;
        // Active hot work wins; at most four duplicate staging rings exist.
        // A completed empty observation is retried next tick, not in a loop.
        trace.enter("cold_history");
        let mut completed=BTreeSet::new();
        let mut cold=self.cold_plans(&active,target,&completed);
        let started=Instant::now();
        let mut pages=0;
        while !cold.is_empty() && pages<self.state.limits.warmup_pages_per_turn && started.elapsed()<WARMUP_TIME_SLICE && !stopped.load(Ordering::SeqCst) {
            self.check_supervision()?;
            let count=IO_CONCURRENCY.min(self.state.limits.warmup_pages_per_turn-pages).min(cold.len());
            let mut reads=tokio::task::JoinSet::new();
            let mut first_error=None;
            for index in self.cold_schedule.select(&cold,count) {
                if let Err(error)=self.check_supervision() {first_error=Some(error);break;}
                let mut plan=cold[index].clone();
                if !self.state.has_epoch(&plan.epoch) {
                    for spec in plan.specs.iter_mut().filter(|spec|!spec.pending) {
                        spec.checkpoint=store::load_checkpoint(data_pool,spec.fence.slot_id,&plan.instrument).await?;
                    }
                }
                if let Err(error)=self.check_supervision() {first_error=Some(error);break;}
                self.state.install(&plan.epoch,&plan.specs)?;
                let resident=self.warmup.as_ref().map_or(0,|w|w.plans.iter().filter(|p|p.epoch!=AdaptiveRuntime::epoch_key(&p.instrument,None) && self.state.has_epoch(&p.epoch)).count());
                self.peak_staging_epochs=self.peak_staging_epochs.max(resident);
                let client=client.clone();
                let (from,_)=self.state.fetch_bounds(&plan.epoch,target)?;
                let guard=self.state.source_guard(&plan.epoch,plan.catalog_id)?;
                let warmup_boundary=self.state.warmup_boundary(&plan.epoch)?;
                reads.spawn(async move {
                    let result=source::read_history(&client,plan.catalog_id,from,target,guard,warmup_boundary).await;
                    (plan,result)
                });
            }
            while let Some(result)=reads.join_next().await {
                let (plan,response)=result.map_err(|e|invalid(&e.to_string()))?;
                if let Err(error)=self.check_supervision() {first_error.get_or_insert(error);continue;}
                let page=match response {
                    Ok(page)=>page,
                    Err(failure)=>{
                        if !failure.rebuild {diagnostics::source_failure(&plan,&failure.error,"cold_history",self.warmup.as_ref());}
                        if failure.invalidate {self.state.invalidate(&plan.epoch,None);}
                        // Drain the owned batch before releasing a lease. A
                        // repair invalidation retries from the same live
                        // origin; transport/schema failures stay errors.
                        if !failure.rebuild {first_error.get_or_insert(failure.error);}
                        completed.insert(plan.epoch.clone());continue;
                    }
                };
                let source_empty=page.missing_source;
                trace.pages+=1;trace.bars+=page.bars.len();
                self.state.acknowledge_source(&plan.epoch,page.token)?;
                let progress=self.warmup.as_ref().filter(|w|plan.specs.iter().any(|s|s.key()==w.candidate_key))
                    .map(|w|(w.supervisor.permit(),w.candidate_key.as_str()));
                if let Err(error)=process_page(&mut self.state,data_pool,&plan,&page.bars,target,page.complete,progress).await {
                    first_error.get_or_insert(error);continue;
                }
                if plan.specs.iter().any(|spec|spec.publish_enabled) {
                    if let Some(required)=preparation_requirements.get(&plan.catalog_id) {
                        let ready=preparation_source_ready(&self.state,active.iter().chain(self.warmup.iter().flat_map(|warmup|warmup.plans.iter())),plan.catalog_id,target);
                        // Nonempty restart recovery uses the immutable live
                        // initialization boundary, not a later observation
                        // clock that would expand the persisted required job.
                        if let Err(code)=self.preparation.observe_required_cold(*required,source_empty,ready,target) {
                            eprintln!("[live-preparation] instrument_id={} code={code}",plan.catalog_id);
                        }
                    }
                }
                if page.complete {
                    completed.insert(plan.epoch.clone());
                    self.park_waiting(&plan,target);
                }
                pages+=1;
            }
            if let Some(error)=first_error {return Err(error);}
            self.adopt_ready_staging(pool,target).await?;
            cold=self.cold_plans(&active,target,&completed);
            tokio::task::yield_now().await;
        }
        trace.enter("waiting_join");
        self.check_supervision()?;
        self.join_ready_waiting(pool,&active,target).await?;
        trace.enter("promotion");
        self.report_warmup_progress(target);
        self.try_promote(pool,&slots,&catalog,target,stopped).await?;
        self.preparation.poll(client,stopped.load(Ordering::SeqCst));
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
            if stopped.load(Ordering::SeqCst) {break;}
            if let Err(error)=self.check_supervision() {first_error.get_or_insert(error);break;}
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
                if let Err(error)=self.check_supervision() {first_error.get_or_insert(error);continue;}
                let response=match response {
                    Ok(response)=>response,
                    Err(error)=>{
                        if matches!(&error,setups_mdclient::MdClientError::Decode(_)) {
                            for plan in &batch {self.state.invalidate(&plan.epoch,None);}
                        }
                        first_error.get_or_insert_with(||invalid(&error.to_string()));continue;
                    }
                };
                let batch_store=batch::PgBatchStore {pool};
                let previous:BTreeMap<_,_>=batch.iter().map(|p|(p.epoch.clone(),self.candidate_cursor(p))).collect();
                let mut window=batch::Window::new(&mut self.state,&batch_store);
                let mut response_failed=false;
                for item in response.instruments {
                    if stopped.load(Ordering::SeqCst) {response_failed=true;break;}
                    let plan=batch.iter().find(|plan|plan.catalog_id==item.instrument_id).ok_or_else(||invalid("requested source epoch missing"))?;
                    let guard=guards.iter().find(|guard|guard.instrument_id==item.instrument_id).ok_or_else(||invalid("requested source guard missing"))?;
                    let token=guard.validated_token(item.read_guard.as_ref());
                    if item.status!="known" || token.is_none() {
                        let error=invalid("source causal history changed or repair coverage is unknown");
                        if item.status!="known" || !item.read_guard.as_ref().is_some_and(|guard|guard.status==setups_mdclient::TailReadGuardStatus::Invalid) {
                            diagnostics::source_failure(plan,&error,"hot_tail_guard",self.warmup.as_ref());
                        }
                        window.reject(&plan.epoch);
                        if item.status!="known" || !item.read_guard.as_ref().is_some_and(|guard|guard.status==setups_mdclient::TailReadGuardStatus::Invalid) {
                            first_error.get_or_insert_with(||invalid("source causal history changed or repair coverage is unknown"));
                        }
                        continue;
                    }
                    let bars:Vec<_>=item.candles.into_iter().map(|row|DetectorBar {open_time_unix:row.candle.open_time_unix,close:row.candle.close,quote_turnover:row.candle.quote_turnover}).collect();
                    window.state.acknowledge_source(&plan.epoch,token.unwrap())?;
                    if let Err(error)=window.prepare(plan,&bars,target).await {
                        first_error.get_or_insert(error);
                        response_failed=true;
                        // Drop invalidates every speculative epoch in this
                        // response. Other source batches are still drained.
                        break;
                    }
                }
                if response_failed {
                    drop(window);
                } else {
                    if let Err(error)=window.finish().await {first_error.get_or_insert(error);}
                    for plan in &batch {self.candidate_progress(plan,previous[&plan.epoch]);}
                    for plan in &batch {self.park_waiting(plan,target);}
                }
            }
        }
        first_error.map_or(Ok(()),Err)
    }
    async fn try_promote(&mut self,pool:&PgPool,_active:&[Arc<ActiveAdaptiveSlot>],_catalog:&[CatalogInstrument],frontier:i64,stopped:&AtomicBool)->Result<(),WorkerError> {
        self.check_supervision()?;
        let Some(warmup)=self.warmup.as_ref() else {return Ok(());};
        let promotion_started=Instant::now();
        if stopped.load(Ordering::SeqCst) {return Ok(());}
        for plan in &warmup.plans {
            if !plan.specs.iter().any(|s|s.publish_enabled) {continue;}
            if warmup.waiting.contains_key(&instrument_key(&plan.instrument)) {continue;}
            if plan.epoch!=AdaptiveRuntime::epoch_key(&plan.instrument,None) || !self.state.is_current(&plan.epoch,frontier) {return Ok(());}
            if plan.specs.iter().any(|spec|spec.key()==warmup.candidate_key)
                && !self.state.ready(&plan.epoch,&warmup.candidate_key,frontier) {return Ok(());}
        }
        // The owner has joined all active commits. This short CAS is the slot
        // barrier; no historical fetch or detector-frame borrow enters it.
        let mut expected=Vec::new();
        for plan in &warmup.plans {
            let key=instrument_key(&plan.instrument);
            let checkpoint=continuous::get_adaptive_checkpoint(pool,warmup.lease.slot_id,&key).await?;
            if !warmup.waiting.contains_key(&key) && !warmup.retired.contains_key(&key)
                && checkpoint.as_ref().is_some_and(|row|row.state["nextOpenUnix"].as_i64()!=Some(frontier)) {return Ok(());}
            expected.push(live::CheckpointExpectation {instrument_key:key,checkpoint});
        }
        let mut initial=Vec::new();
        for plan in warmup.plans.iter().filter(|plan|!warmup.waiting.contains_key(&instrument_key(&plan.instrument))
            && plan.specs.iter().any(|spec|spec.key()==warmup.candidate_key)) {
            let checkpoint=self.state.checkpoint_for(&plan.epoch,&warmup.candidate_key).ok_or_else(||invalid("ready candidate checkpoint missing"))?;
            if checkpoint.next_open_unix!=frontier {return Ok(());}
            initial.push(live::InitialAdaptiveCheckpoint {instrument_key:instrument_key(&plan.instrument),state:serde_json::to_value(checkpoint)?});
        }
        if initial.is_empty() {return Ok(());}
        let waiting:Vec<_>=warmup.waiting.iter().map(|(key,reason)|live::WaitingInstrument {instrument_key:key.clone(),reason:reason.clone()}).collect();
        let retired:Vec<_>=warmup.retired.iter().map(|(key,reason)|live::WaitingInstrument {instrument_key:key.clone(),reason:reason.clone()}).collect();
        warmup.supervisor.permit().check()?;
        let mut fields=diagnostics::warmup_fields(warmup,&self.state,frontier);
        fields["stage"]="promotion".into();fields["outcome"]="started".into();fields["ready_instruments"]=initial.len().into();
        lifecycle::emit(Level::Info,"adaptive_promotion_started",fields);
        if !live::report_warmup(pool,&warmup.lease,"ready",Some(frontier),None).await? {
            return Err(WorkerError::WarmupControl(WarmupControlOutcome::RenewalRejected));
        }
        let result=promotion::persist(&promotion::PgPromotion {pool,lease:&warmup.lease,
            predecessor:warmup.pending.expected_active_generation_id,expected:&expected,frontier,initial:&initial,waiting:&waiting,retired:&retired,
            supervision:Some(warmup.supervisor.permit())}).await;
        let mut fields=live::warmup_diagnostic_fields(&warmup.lease);
        fields["study_id"]=warmup.pending.profile.study_id.into();
        fields["frontier"]=frontier.into();fields["ready_instruments"]=initial.len().into();fields["waiting_instruments"]=waiting.len().into();
        fields["duration_ms"]=(promotion_started.elapsed().as_millis() as u64).into();
        fields["outcome"]=match &result {Ok(promotion::Outcome::Adopt)=>"adopted",Ok(promotion::Outcome::Rebuild)=>"rebuild",
            Ok(promotion::Outcome::Lost)=>"lost",Ok(promotion::Outcome::Retry)=>"retry",Err(_)=>"error"}.into();
        if let Err(error)=&result {fields["error_code"]=diagnostics::error_code(error).into();}
        lifecycle::emit(if result.is_err(){Level::Error}else if matches!(&result,Ok(promotion::Outcome::Retry)){Level::Debug}else{Level::Info},"adaptive_promotion_finished",fields);
        if !matches!(&result,Ok(promotion::Outcome::Adopt|promotion::Outcome::Rebuild)) {self.check_supervision()?;}
        match result {
            Ok(promotion::Outcome::Adopt)=>{
                let mut fields=diagnostics::warmup_fields(self.warmup.as_ref().unwrap(),&self.state,frontier);
                fields["outcome"]="active".into();fields["ready_instruments"]=initial.len().into();
                lifecycle::emit(Level::Info,"adaptive_warmup_finished",fields);
                let mut warmup=self.warmup.take().unwrap();warmup.supervisor.stop().await?;
                for plan in &warmup.plans {
                    if initial.iter().any(|entry|entry.instrument_key==instrument_key(&plan.instrument)) {self.state.activate(&plan.epoch,&warmup.candidate_key,frontier)?;}
                    for spec in plan.specs.iter().filter(|spec|spec.fence.slot_id==warmup.lease.slot_id && spec.key()!=warmup.candidate_key) {
                        self.state.remove_profile(&plan.epoch,&spec.key());
                    }
                }
            },
            Ok(promotion::Outcome::Rebuild)=>{
                let mut fields=diagnostics::warmup_fields(self.warmup.as_ref().unwrap(),&self.state,frontier);
                fields["outcome"]="committed_rebuild".into();lifecycle::emit(Level::Info,"adaptive_warmup_finished",fields);
                let mut warmup=self.warmup.take().unwrap();warmup.supervisor.stop().await?;
                for plan in &warmup.plans {self.state.invalidate(&plan.epoch,None);}
            },
            Ok(promotion::Outcome::Lost)=>self.discard_warmup("promotion_fenced").await?,
            Ok(promotion::Outcome::Retry)=>{
                let warmup=self.warmup.as_mut().unwrap();warmup.barrier_attempts+=1;
                if warmup.barrier_attempts>=3 {
                    lifecycle::emit(Level::Info,"adaptive_promotion_yielded",live::warmup_diagnostic_fields(&warmup.lease));
                    live::release_pending_warmup(pool,&warmup.lease).await?;
                    self.discard_warmup("promotion_barrier_retry_limit").await?;
                }
            },
            Err(error)=>{
                // tick_at drains/discards only candidate staging. Preserve the
                // incumbent's canonical state; if COMMIT was actually applied,
                // next tick's authoritative generation projection reconciles it.
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

async fn process_page(state:&mut AdaptiveRuntime,pool:&PgPool,plan:&SymbolPlan,bars:&[DetectorBar],target:i64,complete:bool,progress:Option<(&supervision::Permit,&str)>)->Result<(),WorkerError> {
    if bars.is_empty() {process_chunk(state,pool,plan,&[],target,complete).await?;}
    for (index,chunk) in bars.chunks(ADAPTIVE_PAGE_LIMIT).enumerate() {
        if let Some((permit,_))=progress {permit.check()?;}
        let previous=progress.and_then(|(_,key)|state.checkpoint_for(&plan.epoch,key));
        process_chunk(state,pool,plan,chunk,target,complete && (index+1)*ADAPTIVE_PAGE_LIMIT>=bars.len()).await?;
        if let Some((permit,key))=progress && let (Some(before),Some(after))=(previous,state.checkpoint_for(&plan.epoch,key))
            && after.next_open_unix>before.next_open_unix {permit.progress();}
    }
    Ok(())
}
async fn process_chunk(state:&mut AdaptiveRuntime,pool:&PgPool,plan:&SymbolPlan,bars:&[DetectorBar],target:i64,complete:bool)->Result<(),WorkerError> {
    let commit_concurrency=cold_commit_concurrency(pool)?;
    let prepared=match state.prepare(&plan.epoch,&plan.instrument,bars,target,complete) {
        Ok(prepared)=>prepared,
        Err(error)=>{state.invalidate(&plan.epoch,None);return Err(error);}
    };
    let mut tasks=tokio::task::JoinSet::new();
    let mut first_error=None;
    for prepared in prepared {
        if !state.has_epoch(&plan.epoch) {continue;}
        if !prepared.should_commit() {state.accept(&plan.epoch,prepared,false);continue;}
        let Some(slot)=plan.slots.get(&prepared.key).cloned() else {
            first_error.get_or_insert_with(||invalid("active store missing"));state.invalidate(&plan.epoch,None);continue;
        };
        let pool=pool.clone();let instrument=plan.instrument.clone();
        tasks.spawn(async move {
            let mut prepared=prepared;
            let store=PgAdaptiveStore {pool:&pool,slot:&slot};
            let result=store::persist(&store,&instrument,&mut prepared).await;
            (prepared,result)
        });
        if tasks.len()>=commit_concurrency
            && let Err(error)=accept_result(state,&plan.epoch,tasks.join_next().await.unwrap()) {first_error.get_or_insert(error);}
    }
    while let Some(result)=tasks.join_next().await {
        if let Err(error)=accept_result(state,&plan.epoch,result) {first_error.get_or_insert(error);}
    }
    if let Some(error)=first_error {state.invalidate(&plan.epoch,None);return Err(error);}
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

pub(crate) async fn adaptive_live_loop(data_pool:PgPool,control_pool:PgPool,market_url:String,market_key:String,stopped:Arc<AtomicBool>,notify:Arc<tokio::sync::Notify>) {
    let scan_pool=control_pool;let scan_data_pool=data_pool.clone();let scan_stopped=stopped.clone();let scan_notify=notify.clone();
    let scan=async move {
        let client=MarketDataClient::new(market_url,market_key);
        let mut owner=AdaptiveLiveRuntime::new(AdaptiveRuntimeLimits::default());
        lifecycle::emit(Level::Info,"adaptive_runtime_started",json!({"runtime_instance":owner.worker}));
        while !scan_stopped.load(Ordering::SeqCst) {
            if let Err(error)=owner.tick_at_on_pools(&scan_data_pool,&scan_pool,&client,&scan_stopped,chrono::Utc::now().timestamp()).await {
                if let Some(warmup)=&owner.warmup {
                    let report=json!({"code":"adaptive_warmup_failed","message":error.to_string().chars().take(240).collect::<String>()});
                    let _=live::report_warmup(&scan_pool,&warmup.lease,"error",None,Some(&report)).await;
                    if let Err(error)=owner.discard_warmup("loop_failed").await {lifecycle::emit(Level::Error,"live_supervision_join_failed",json!({"error_code":diagnostics::error_code(&error)}));}
                }
                lifecycle::emit(Level::Error,"adaptive_tick_error",json!({"runtime_instance":owner.worker,"error_code":diagnostics::error_code(&error)}));
            }
            tokio::select! {_=scan_notify.notified()=>{},_=tokio::time::sleep(Duration::from_secs(2))=>{}}
        }
        let shutdown_started=Instant::now();
        let mut fields=owner.warmup.as_ref().map_or_else(||json!({"runtime_instance":owner.worker}),|w|live::warmup_diagnostic_fields(&w.lease));
        fields["warmup_in_progress"]=owner.warmup.is_some().into();
        fields["history_bytes_before"]=owner.retained_history_bytes().into();fields["profile_bytes_before"]=owner.retained_profile_bytes().into();
        lifecycle::emit(Level::Info,"adaptive_shutdown_started",fields.clone());
        let release=owner.shutdown_warmup(&scan_pool).await;
        fields["lease_disposition"]=match &release {Ok(true)=>"released_and_refunded",Ok(false)=>"no_current_claim",Err(_)=>"release_error_not_refunded"}.into();
        if let Err(error)=&release {fields["error_code"]=diagnostics::error_code(error).into();}
        owner.state.clear();drop(owner);
        fields["duration_ms"]=(shutdown_started.elapsed().as_millis() as u64).into();
        fields["outcome"]=if release.is_ok(){"drained"}else{"release_error"}.into();
        fields["history_bytes"]=0.into();fields["profile_bytes"]=0.into();
        lifecycle::emit(if release.is_ok(){Level::Info}else{Level::Error},"adaptive_shutdown_finished",fields);
    };
    tokio::join!(scan,delivery::run(data_pool,stopped,notify));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adaptive::state::tests::{fixture,feed};
    #[tokio::test]
    async fn cold_persistence_uses_only_data_pool_capacity() {
        for (connections,expected) in [(1,1),(2,2),(3,3),(17,16),(64,16)] {
            let pool=sqlx::postgres::PgPoolOptions::new().max_connections(connections)
                .connect_lazy("postgres://test:test@127.0.0.1:1/unreachable_test").unwrap();
            assert_eq!(cold_commit_concurrency(&pool).unwrap(),expected);
        }
    }
    #[tokio::test]
    #[ignore = "requires isolated TEST_DATABASE_URL ending in _test with CREATEDB"]
    async fn cold_chunk_six_profiles_reserve_fourth_connection_and_drain_failed_tasks() {
        use futures::FutureExt;
        use sqlx::Connection;
        let options:sqlx::postgres::PgConnectOptions=std::env::var("TEST_DATABASE_URL").unwrap().parse().unwrap();
        assert!(options.get_database().unwrap_or("").ends_with("_test"));
        let name=format!("cold_chunk_{}_{}_test",std::process::id(),rand::random::<u32>());
        let mut admin=sqlx::PgConnection::connect_with(&options).await.unwrap();
        sqlx::query(&format!("create database {name}")).execute(&mut admin).await.unwrap();
        let options=options.database(&name);
        let pool=sqlx::postgres::PgPoolOptions::new().max_connections(3).acquire_timeout(Duration::from_secs(5))
            .connect_with(options.clone()).await.unwrap();
        let control_pool=sqlx::postgres::PgPoolOptions::new().max_connections(1).acquire_timeout(Duration::from_secs(5))
            .connect_with(options.clone()).await.unwrap();
        let result=std::panic::AssertUnwindSafe(async {
            // Hold every real settings read, then let the deliberately absent
            // persistence schema fail so task draining can be observed.
            sqlx::query("create table settings(key text primary key,value jsonb)").execute(&pool).await.unwrap();
            let mut blocker=sqlx::PgConnection::connect_with(&options).await.unwrap();
            let mut held=blocker.begin().await.unwrap();
            sqlx::query("lock table settings in access exclusive mode").execute(&mut *held).await.unwrap();
            let f=fixture();
            let mut state=AdaptiveRuntime::new(AdaptiveRuntimeLimits {max_profiles:6,..f.runtime.limits.clone()});
            let mut specs=Vec::new();let mut slots=BTreeMap::new();
            for id in 1..=6 {
                let mut spec=f.spec.clone();spec.fence.slot_id=id;
                slots.insert(spec.key(),Arc::new(ActiveAdaptiveSlot {
                    owner_id:spec.fence.owner_id.clone(),algorithm_id:spec.fence.algorithm_id.clone(),slot_id:id,
                    generation_id:1,study_id:id,config_revision:1,min_setups_per_day:0.0,
                    spec:json!({}),result:json!({}),params:json!({}),analytics:json!({}),selected_cell:json!({}),
                    mean_expectancy:0.2,direction:"short".into(),external_name:"fixture".into(),
                    live_initialization:None,live_readiness:Vec::new(),
                }));
                specs.push(spec);
            }
            state.install(&f.epoch,&specs).unwrap();
            feed(&mut state,&f.epoch,&f.instrument,&f.bars,f.frontier).unwrap();
            let plan=SymbolPlan {epoch:f.epoch,instrument:f.instrument,catalog_id:1,specs,slots};
            let mut next=f.bars.last().unwrap().clone();next.open_time_unix=f.frontier;
            let work_pool=pool.clone();
            let work=tokio::spawn(async move {
                let result=process_chunk(&mut state,&work_pool,&plan,&[next],f.frontier+60,true).await;
                (state,result)
            });
            let observed=tokio::time::timeout(Duration::from_secs(3),async {
                loop {
                    sqlx::query("select pg_stat_clear_snapshot()").execute(&mut *held).await.unwrap();
                    let count:i64=sqlx::query_scalar("select count(*) from pg_stat_activity where datname=current_database() and wait_event_type='Lock' and query like 'select value from settings%'")
                        .fetch_one(&mut *held).await.unwrap();
                    if count>=3 {break;}
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
                sqlx::query("select pg_stat_clear_snapshot()").execute(&mut *held).await.unwrap();
                sqlx::query_scalar::<_,i64>("select count(*) from pg_stat_activity where datname=current_database() and wait_event_type='Lock' and query like 'select value from settings%'")
                    .fetch_one(&mut *held).await.unwrap()
            }).await;
            let control=tokio::time::timeout(Duration::from_millis(500),sqlx::query_scalar::<_,i32>("select 1").fetch_one(&control_pool)).await;
            // Release before asserting so a failed admission assertion cannot
            // leave real blocked SQL/tasks holding the owned test database.
            held.rollback().await.unwrap();
            let (state,outcome)=tokio::time::timeout(Duration::from_secs(5),work).await.unwrap().unwrap();
            assert_eq!(observed.unwrap(),3,"six profiles must own at most three cold operations on data3/control1");
            assert_eq!(control.unwrap().unwrap(),1,"control query must use the reserved fourth slot");
            assert!(outcome.is_err(),"missing persistence schema deliberately fails the writes");
            assert_eq!(state.retained_history_epochs(),0,"failed speculative chunk is invalidated");
            let mut returned=Vec::new();
            for _ in 0..3 {returned.push(tokio::time::timeout(Duration::from_secs(1),pool.acquire()).await.unwrap().unwrap());}
            drop(returned);blocker.close().await.unwrap();
        }).catch_unwind().await;
        pool.close().await;
        control_pool.close().await;
        sqlx::query(&format!("drop database {name} with (force)")).execute(&mut admin).await.unwrap();
        admin.close().await.unwrap();
        if let Err(error)=result {std::panic::resume_unwind(error);}
    }
    #[tokio::test]
    async fn supervision_loss_drains_owned_reads_stops_admission_and_preserves_incumbent_ram() {
        use axum::{Json,Router,routing::post};
        use std::sync::atomic::AtomicUsize;
        let f=fixture();let mut candidate=f.spec.clone();candidate.fence.generation_id=2;candidate.pending=true;
        let candidate_key=candidate.key();let incumbent_key=f.spec.key();
        let lease=live::WarmupLease {owner_id:f.spec.fence.owner_id.clone(),algorithm_id:f.spec.fence.algorithm_id.clone(),slot_id:1,
            generation_id:2,config_revision:1,worker:"owned-drain".into(),token:1,attempt:1,attempt_deadline_unix:chrono::Utc::now().timestamp()+21600,
            initialization:live::LiveInitialization {version:1,anchor_close_unix:f.frontier,detector_from_open_unix:0,emit_from_close_unix:f.frontier+60},
            lease_valid_until:Instant::now()+Duration::from_secs(30)};
        let profile=ActiveAdaptiveSlot {owner_id:lease.owner_id.clone(),algorithm_id:lease.algorithm_id.clone(),slot_id:1,generation_id:2,study_id:2,
            config_revision:1,min_setups_per_day:0.0,spec:json!({}),result:json!({}),params:json!({}),analytics:json!({}),selected_cell:json!({}),
            mean_expectancy:0.21,direction:"short".into(),external_name:"fixture".into(),live_initialization:Some(lease.initialization.clone()),live_readiness:Vec::new()};
        let plan=SymbolPlan {epoch:f.epoch.clone(),instrument:f.instrument.clone(),catalog_id:1,specs:vec![f.spec.clone(),candidate],slots:BTreeMap::new()};
        let mut owner=AdaptiveLiveRuntime::new(f.runtime.limits.clone());
        owner.state.install(&f.epoch,&plan.specs).unwrap();feed(&mut owner.state,&f.epoch,&f.instrument,&f.bars,f.frontier).unwrap();
        let checkpoint=owner.state.checkpoint_for(&f.epoch,&incumbent_key).unwrap();let history_bytes=owner.retained_history_bytes();
        let supervisor=supervision::Supervisor::fixture(&lease);let permit=supervisor.permit().clone();
        owner.warmup=Some(Warmup {pending:live::PendingAdaptiveSlot {profile,expected_active_generation_id:Some(1),warmup_status:"warming".into(),readiness_frontier:None},lease,supervisor,
             plans:vec![plan.clone()],candidate_key:candidate_key.clone(),cohort:BTreeSet::new(),barrier_attempts:0,waiting:BTreeMap::new(),retired:BTreeMap::new(),started:Instant::now(),progress_reported_at:Instant::now()});
        let trace=diagnostics::TickTrace::new("owned-drain-test");
        let (_,fields)=diagnostics::failure_report(invalid("history_lookup_evicted: fixture"),&Ok(true),trace.fields(&owner,f.frontier));
        let record=lifecycle::record(Level::Error,"adaptive_warmup_failure_report",&fields);
        assert_eq!(record["failure_report"],"persisted");assert_eq!(record["original_error_code"],"history_lookup_evicted");
        assert_eq!(record["study_id"],2);assert_eq!(record["generation_id"],2);assert_eq!(record["attempt"],1);
        assert_eq!(record["stage"],"profile_handoff");assert!(record.get("fields_truncated").is_none(),"{record}");
        let canonical=SymbolPlan {specs:vec![f.spec.clone()],..plan.clone()};
        let fields=diagnostics::source_failure_fields(&canonical,&invalid("source unavailable"),"hot_tail_guard",owner.warmup.as_ref());
        let record=lifecycle::record(Level::Error,"adaptive_source_failed",&fields);
        assert_eq!(record["candidate_in_plan"],false);assert_eq!(record["context_role"],"source_epoch");
        assert_eq!(record["warmup_generation_id"],2);assert_eq!(record["warmup_study_id"],2);assert_eq!(record["warmup_attempt"],1);
        for key in ["profile_id","generation_id","study_id"] {assert!(record.get(key).is_none(),"unrelated warmup is not culprit {key}");}
        assert_eq!(record["history_epoch"],canonical.epoch);assert!(record.get("fields_truncated").is_none());
        let entered=Arc::new(AtomicUsize::new(0));let observed=entered.clone();let gate=Arc::new(tokio::sync::Semaphore::new(0));let blocked=gate.clone();
        let app=Router::new().route("/v1/candles/tail-batch",post(move || {
            let entered=observed.clone();let gate=blocked.clone();async move {
                entered.fetch_add(1,Ordering::SeqCst);gate.acquire().await.unwrap().forget();Json(json!({}))
            }
        }));
        let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();let url=format!("http://{}",listener.local_addr().unwrap());
        let server=tokio::spawn(async move {axum::serve(listener,app).await.unwrap();});
        let pool=sqlx::postgres::PgPoolOptions::new().connect_lazy("postgres://test:test@127.0.0.1:1/unreachable_test").unwrap();
        let batches=vec![vec![plan];IO_CONCURRENCY+1];let target=f.frontier;
        let work=tokio::spawn(async move {
            let outcome=owner.hot_tail_snapshots(&pool,&MarketDataClient::new(url,"fixture"),batches,target,&AtomicBool::new(false)).await;(owner,outcome)
        });
        tokio::time::timeout(Duration::from_secs(2),async {while entered.load(Ordering::SeqCst)<IO_CONCURRENCY {tokio::time::sleep(Duration::from_millis(1)).await;}}).await.unwrap();
        permit.lose(WarmupControlOutcome::RenewalRejected);
        gate.add_permits(IO_CONCURRENCY-1);tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!work.is_finished(),"loss must drain every already-owned request, not abort its remaining sibling");
        gate.add_permits(1);let (mut owner,outcome)=tokio::time::timeout(Duration::from_secs(2),work).await.unwrap().unwrap();
        assert!(matches!(outcome,Err(WorkerError::WarmupControl(WarmupControlOutcome::RenewalRejected))));
        assert_eq!(entered.load(Ordering::SeqCst),IO_CONCURRENCY,"no next-wave admission after supervision loss");
        owner.discard_warmup("test_supervision_lost").await.unwrap();assert!(owner.warmup.is_none());
        assert_eq!(owner.retained_history_bytes(),history_bytes);
        assert_eq!(owner.state.checkpoint_for(&f.epoch,&incumbent_key),Some(checkpoint));
        assert!(owner.state.checkpoint_for(&f.epoch,&candidate_key).is_none());
        let mut next=f.bars.last().unwrap().clone();next.open_time_unix=f.frontier;next.quote_turnover="100".into();
        feed(&mut owner.state,&f.epoch,&f.instrument,std::slice::from_ref(&next),f.frontier+60).unwrap();
        let mut reference=fixture();reference.runtime.install(&reference.epoch,std::slice::from_ref(&reference.spec)).unwrap();
        let mut bars=f.bars;bars.push(next);feed(&mut reference.runtime,&reference.epoch,&reference.instrument,&bars,f.frontier+60).unwrap();
        assert_eq!(owner.state.checkpoint_for(&f.epoch,&incumbent_key),reference.runtime.checkpoint_for(&reference.epoch,&incumbent_key),"incumbent continues the original detector/cluster, not a reset profile");
        server.abort();let _=server.await;
    }
    #[test]
    fn cold_preparation_uses_frozen_scope_bounds_not_restart_observation_clock() {
        let f=fixture();let mut sibling=f.spec.clone();sibling.from_open=60;sibling.warmup_through+=60;
        let frozen_end=sibling.warmup_through;
        let plan=SymbolPlan {epoch:f.epoch,instrument:f.instrument,catalog_id:820,specs:vec![f.spec,sibling],slots:BTreeMap::new()};
        let later=frozen_end+86400;
        let required=current_preparation_requirements(std::iter::once(&plan));let demand=required[&820];
        assert_eq!(demand,setups_mdclient::HistoryDemand {instrument_id:820,from_unix:0,to_unix:frozen_end});
        assert!(demand.to_unix<later);
        assert_eq!(current_preparation_requirements(std::iter::once(&plan))[&820],demand);
    }
    #[tokio::test]
    async fn alternating_epoch_origins_share_one_task_and_departure_reconciles_actual_cohort_once() {
        use crate::preparation::{Preparation,tests::{Fake,settle,posts}};
        let f=fixture();let mut second=f.spec.clone();second.from_open=60;second.warmup_through+=60;
        let mut first=f.spec.clone();first.warmup_through+=120;
        let plans=vec![SymbolPlan {epoch:f.epoch.clone(),instrument:f.instrument.clone(),catalog_id:820,specs:vec![first],slots:BTreeMap::new()},
            SymbolPlan {epoch:AdaptiveRuntime::epoch_key(&f.instrument,Some(2)),instrument:f.instrument,catalog_id:820,specs:vec![second],slots:BTreeMap::new()}];
        let required=current_preparation_requirements(plans.iter());let combined=required[&820];
        assert_eq!((combined.from_unix,combined.to_unix),(0,f.frontier+120));
        let (backend,gate)=Fake::gated();let mut preparation=Preparation::default();
        preparation.reconcile_requirements(&required);
        preparation.observe_required_cold(combined,false,false,f.frontier+86400).unwrap();preparation.poll(&backend,false);tokio::task::yield_now().await;
        let initial=preparation.operation_snapshot(820).unwrap();assert!(initial.2.is_some());
        for index in 0..100 {
            let plan=&plans[index%2];let ready=preparation_source_ready(&f.runtime,plans.iter(),plan.catalog_id,f.frontier);
            preparation.reconcile_requirements(&required);
            preparation.observe_required_cold(required[&plan.catalog_id],false,ready,f.frontier+86400).unwrap();preparation.poll(&backend,false);
            tokio::task::yield_now().await;
            assert_eq!(preparation.operation_snapshot(820).unwrap(),initial,"siblings must not abort/version/reset the same source operation");
        }
        assert_eq!(backend.call_count("state:"),1);
        gate.notify_one();settle(&mut preparation,&backend).await;assert_eq!(posts(&backend),1);
        let accepted=preparation.operation_snapshot(820).unwrap();assert!(accepted.3);
        // Drop the earliest/longest profile: both ends now come from B, not a
        // min-ever/max-ever cache. Exactly one new operation uses that cohort.
        let remaining=current_preparation_requirements(std::iter::once(&plans[1]));
        assert_eq!((remaining[&820].from_unix,remaining[&820].to_unix),(60,f.frontier+60));
        preparation.reconcile_requirements(&remaining);assert!(preparation.operation_snapshot(820).is_none());
        preparation.observe_required_cold(remaining[&820],false,false,f.frontier+172800).unwrap();preparation.poll(&backend,false);tokio::task::yield_now().await;
        let changed=preparation.operation_snapshot(820).unwrap();assert_ne!(changed.0,accepted.0);
        for _ in 0..50 {
            preparation.reconcile_requirements(&remaining);preparation.observe_required_cold(remaining[&820],false,false,f.frontier+172800).unwrap();preparation.poll(&backend,false);
            assert_eq!(preparation.operation_snapshot(820).unwrap(),changed);
        }
        gate.notify_one();settle(&mut preparation,&backend).await;
        assert_eq!(posts(&backend),1,"persisted original covering job is reused after real cohort shrink");
        assert_eq!(backend.call_count("state:"),2);assert_eq!(preparation.operation_snapshot(820).unwrap().1,remaining[&820]);
    }
    #[tokio::test]
    async fn ready_epoch_does_not_retire_nonready_sibling_source_preparation() {
        use crate::preparation::{Preparation,tests::{Fake,settle,posts}};
        let mut f=fixture();f.runtime.install(&f.epoch,std::slice::from_ref(&f.spec)).unwrap();
        feed(&mut f.runtime,&f.epoch,&f.instrument,&f.bars,f.frontier).unwrap();
        assert!(f.runtime.ready(&f.epoch,&f.spec.key(),f.frontier));
        let mut sibling=f.spec.clone();sibling.from_open=60;sibling.fence.generation_id=2;sibling.pending=true;
        let epoch=AdaptiveRuntime::epoch_key(&f.instrument,Some(2));f.runtime.install(&epoch,std::slice::from_ref(&sibling)).unwrap();
        feed(&mut f.runtime,&epoch,&f.instrument,&f.bars[1..2],f.frontier).unwrap();
        assert!(!f.runtime.ready(&epoch,&sibling.key(),f.frontier));
        let plans=vec![SymbolPlan {epoch:f.epoch,instrument:f.instrument.clone(),catalog_id:820,specs:vec![f.spec],slots:BTreeMap::new()},
            SymbolPlan {epoch,instrument:f.instrument,catalog_id:820,specs:vec![sibling],slots:BTreeMap::new()}];
        let required=current_preparation_requirements(plans.iter());let (backend,_gate)=Fake::gated();let mut preparation=Preparation::default();
        preparation.reconcile_requirements(&required);preparation.observe_required_cold(required[&820],false,false,f.frontier).unwrap();preparation.poll(&backend,false);tokio::task::yield_now().await;
        let initial=preparation.operation_snapshot(820).unwrap();
        for index in 0..20 {
            let plan=&plans[index%2];let ready=preparation_source_ready(&f.runtime,plans.iter(),plan.catalog_id,f.frontier);
            assert!(!ready,"ready A is not whole-source readiness while B remains cold");
            preparation.observe_required_cold(required[&820],false,ready,f.frontier).unwrap();preparation.poll(&backend,false);tokio::task::yield_now().await;
            assert_eq!(preparation.operation_snapshot(820).unwrap(),initial);
        }
        assert_eq!(preparation.status()["collectionOnly"],0);
        preparation.clear();settle(&mut preparation,&backend).await;assert_eq!(posts(&backend),0);
    }
    #[tokio::test]
    async fn independent_epoch_proofs_keep_long_revision_two_and_short_revision_one_valid() {
        use axum::{Json,Router,routing::post};
        use setups_mdclient::TailReadToken;
        let mut f=fixture();f.spec.warmup_through=60;f.spec.emit_from_close=120;
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
        let other_spec=f.spec.clone();
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
