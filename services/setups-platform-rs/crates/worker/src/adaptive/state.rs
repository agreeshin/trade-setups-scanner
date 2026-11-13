//! Runtime-owned parsed history, scalar profile state and speculative effects.
//! Nothing in this module performs IO or keeps a borrowed frame across await.
use super::*;
use setups_detector::shared::{HistoryIdentity, HistoryLimits, SharedHistory, SharedProfile};
use setups_domain::{PushStatus, decimal::Decimal};
use setups_optimizer::ContinuousInstrumentPrefix;
use setups_mdclient::{TailReadGuardQuery,TailReadToken};
use std::{collections::{BTreeMap, BTreeSet}, mem::size_of, sync::Arc};

#[derive(Debug, Clone)]
pub struct AdaptiveRuntimeLimits {
    pub max_history_bytes: usize,
    pub max_symbols: usize,
    pub max_profiles: usize,
    pub max_profile_bytes: usize,
    pub warmup_pages_per_turn: usize,
    pub max_close_bytes_per_minute: usize,
}
impl Default for AdaptiveRuntimeLimits {
    fn default() -> Self {
        Self { max_history_bytes: 8 * 1024 * 1024 * 1024, max_symbols: 1000,
            max_profiles: 64, max_profile_bytes: 512 * 1024 * 1024,
            warmup_pages_per_turn: 32, max_close_bytes_per_minute: 48 }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ProfileSpec {
    pub fence: AdaptiveFence,
    pub params: AlgorithmParams,
    pub recommendation: Arc<Value>,
    pub cluster_gap_minutes: Option<u32>,
    pub from_open: i64,
    pub bootstrap_next: i64,
    pub bootstrap_raw_open: Option<i64>,
    pub emit_from_close: i64,
    pub checkpoint: Option<AdaptiveCheckpoint>,
    pub prefix: ContinuousInstrumentPrefix,
    pub pending: bool,
    // Catalog eligibility only. Pending muting and live permission fences are
    // independent; false is reserved for an explicitly not-listed instrument.
    pub publish_enabled: bool,
}
impl ProfileSpec {
    pub fn key(&self) -> String {
        serde_json::json!([self.fence.owner_id,self.fence.algorithm_id,self.fence.slot_id,
            self.fence.generation_id,self.fence.config_revision]).to_string()
    }
}

#[derive(Clone)]
struct PrefixDigest {
    hash: Sha256,
    count: u64,
    expected: ContinuousInstrumentPrefix,
    from: i64,
    to: i64,
}
impl PrefixDigest {
    fn new(spec: &ProfileSpec) -> Self {
        let mut hash=Sha256::new();
        hash.update(b"continuous-detector-prefix-v1\0");
        hash.update(spec.from_open.to_be_bytes()); hash.update(spec.bootstrap_next.to_be_bytes());
        hash.update(spec.prefix.bar_count.to_be_bytes());
        Self { hash,count:0,expected:spec.prefix.clone(),from:spec.from_open,to:spec.bootstrap_next }
    }
    fn observe(&mut self, open: i64, close: Decimal, turnover: Decimal) {
        if open<self.from || open>=self.to { return; }
        self.hash.update(open.to_be_bytes());
        for mut value in [close,turnover] {
            while value.scale>0 && value.int%10==0 {value.int/=10;value.scale-=1;}
            self.hash.update(value.int.to_be_bytes());self.hash.update(value.scale.to_be_bytes());
        }
        self.count+=1;
    }
    fn verify(&self) -> Result<(), WorkerError> {
        if self.count!=self.expected.bar_count || format!("{:x}",self.hash.clone().finalize())!=self.expected.sha256 {
            return Err(invalid("frozen detector prefix fingerprint mismatch"));
        }
        Ok(())
    }
}

#[derive(Clone)]
struct CachedProfile {
    spec: ProfileSpec,
    detector: Option<SharedProfile>,
    cluster: CausalCluster,
    checkpoint: AdaptiveCheckpoint,
    expected: Option<AdaptiveCheckpoint>,
    digest: PrefixDigest,
    prefix_verified: bool,
    checkpoint_verified: bool,
    last_status: Option<PushStatus>,
}
impl CachedProfile {
    fn new(spec: ProfileSpec) -> Result<Self, WorkerError> {
        let cluster=CausalCluster::new(i64::from(spec.cluster_gap_minutes.unwrap_or(1))*60,None)
            .map_err(|error|invalid(&error.to_string()))?;
        let digest=PrefixDigest::new(&spec);
        let checkpoint=AdaptiveCheckpoint {generation_id:spec.fence.generation_id,next_open_unix:spec.from_open,
            last_raw_open_unix:None,last_raw_cluster_time:None};
        let expected=spec.checkpoint.clone();
        Ok(Self {spec,detector:None,cluster,checkpoint,expected,digest,prefix_verified:false,
            checkpoint_verified:false,last_status:None})
    }
    fn check_boundaries(&mut self, through_open: i64) -> Result<(),WorkerError> {
        if !self.prefix_verified && through_open>=self.spec.bootstrap_next {
            self.digest.verify()?;
            if self.checkpoint.last_raw_open_unix!=self.spec.bootstrap_raw_open {
                return Err(invalid("frozen prefix raw seed differs from original replay"));
            }
            self.prefix_verified=true;
        }
        if !self.checkpoint_verified && let Some(expected)=&self.expected
            && expected.generation_id==self.spec.fence.generation_id && through_open>=expected.next_open_unix {
            // The durable cursor is the publication boundary, not proof that
            // its derived cooldown/cluster seeds still match canonical data.
            // Reconstruct them from the original frozen prefix and guarded
            // input, suppress pre-cursor effects, then CAS the fresh state.
            // A genuine post-frozen repair can change cooldown indefinitely.
            self.checkpoint_verified=true;
        }
        Ok(())
    }
    fn retained_bytes(&self) -> usize {
        size_of::<Self>()+self.detector.as_ref().map_or(0,SharedProfile::retained_bytes)
            + self.spec.key().len()+self.spec.prefix.sha256.capacity()
    }
}

pub(crate) struct PreparedProfile {
    pub key: String,
    pub fence: AdaptiveFence,
    pub expected: Option<AdaptiveCheckpoint>,
    pub checkpoint: AdaptiveCheckpoint,
    pub setups: Vec<AdaptiveSetup>,
    next: CachedProfile,
}
impl PreparedProfile {
    pub(crate) fn should_commit(&self)->bool {
        !self.next.spec.pending && self.next.prefix_verified
            && (self.expected.as_ref()!=Some(&self.checkpoint) || !self.setups.is_empty())
            && self.checkpoint.next_open_unix>=self.next.spec.bootstrap_next
            && self.expected.as_ref().filter(|c|c.generation_id==self.fence.generation_id)
                .is_none_or(|c|self.next.checkpoint_verified && self.checkpoint.next_open_unix>=c.next_open_unix)
    }
}

struct SymbolState {
    history: SharedHistory,
    profiles: BTreeMap<String,CachedProfile>,
    read_from: i64,
    observed_through: i64,
    source_token: Option<TailReadToken>,
    capacity_minutes: usize,
}

/// Sole owner of retained adaptive RAM. Drop/reconcile releases actual history
/// allocations; there is no process-global map, timer cache or hidden singleton.
pub struct AdaptiveRuntime {
    pub limits: AdaptiveRuntimeLimits,
    symbols: BTreeMap<String,SymbolState>,
    locations: BTreeMap<(String,String),String>,
    epoch: u64,
}
impl AdaptiveRuntime {
    pub fn new(limits: AdaptiveRuntimeLimits) -> Self {Self {limits,symbols:BTreeMap::new(),locations:BTreeMap::new(),epoch:0}}
    pub fn retained_history_epochs(&self)->usize {self.symbols.len()}
    pub fn retained_history_bytes(&self)->usize {self.symbols.values().map(|s|s.history.retained_bytes()).sum()}
    pub fn retained_profile_bytes(&self)->usize {self.symbols.values().flat_map(|s|s.profiles.values()).map(CachedProfile::retained_bytes).sum()}
    pub fn clear(&mut self) {self.symbols.clear();self.locations.clear();}
    pub(crate) fn profile_epoch(&self,instrument:&AdaptiveInstrument,profile:&str)->Option<String> {
        let symbol=serde_json::json!([instrument.exchange,instrument.category,instrument.symbol]).to_string();
        self.locations.get(&(symbol,profile.into())).cloned()
    }
    pub(crate) fn has_epoch(&self,epoch:&str)->bool {self.symbols.contains_key(epoch)}
    pub(crate) fn cached_checkpoint(&self,epoch:&str,profile:&str)->Option<Option<AdaptiveCheckpoint>> {
        self.symbols.get(epoch).and_then(|s|s.profiles.get(profile)).map(|p|p.expected.clone())
    }
    pub(crate) fn checkpoint_for(&self,epoch:&str,profile:&str)->Option<AdaptiveCheckpoint> {
        self.symbols.get(epoch).and_then(|s|s.profiles.get(profile)).map(|p|p.checkpoint.clone())
    }
    pub(crate) fn source_guard(&self,epoch:&str,instrument_id:i64)->Result<TailReadGuardQuery,WorkerError> {
        let symbol=self.symbols.get(epoch).ok_or_else(||invalid("missing source history owner"))?;
        // This is the ORIGINAL reconstruction anchor, not the rolling ring
        // anchor: old input can change the phase of an indefinite cooldown.
        let to=symbol.profiles.values().map(|p|p.checkpoint.next_open_unix.max(if p.prefix_verified {p.spec.bootstrap_next}else{p.spec.from_open})).max().unwrap_or(symbol.read_from);
        let from=symbol.profiles.values().map(|p|p.spec.from_open).min().unwrap_or(symbol.read_from);
        Ok(TailReadGuardQuery {instrument_id,from_unix:from,to_unix:to,previous:symbol.source_token.clone()})
    }
    pub(crate) fn frozen_boundary(&self,epoch:&str)->Result<i64,WorkerError> {
        self.symbols.get(epoch).and_then(|s|s.profiles.values().map(|p|p.spec.bootstrap_next).max()).ok_or_else(||invalid("missing frozen boundary"))
    }
    pub(crate) fn acknowledge_source(&mut self,epoch:&str,token:TailReadToken)->Result<(),WorkerError> {
        token.validate().map_err(shared_error)?;
        self.symbols.get_mut(epoch).ok_or_else(||invalid("missing source history owner"))?.source_token=Some(token);
        Ok(())
    }
    pub(crate) fn is_current(&self,epoch:&str,through:i64)->bool {
        self.symbols.get(epoch).is_some_and(|s|s.source_token.is_some()
            && s.profiles.values().all(|p|p.prefix_verified && p.checkpoint.next_open_unix>=through))
    }
    pub(crate) fn account_not_listed(&mut self,epoch:&str,frontier:i64)->Result<(),WorkerError> {
        let symbol=self.symbols.get_mut(epoch).ok_or_else(||invalid("not-listed history owner missing"))?;
        if symbol.source_token.is_none() || symbol.observed_through<frontier
            || symbol.profiles.values().any(|p|p.spec.publish_enabled || !p.prefix_verified) {
            return Err(invalid("not-listed accounting requires verified source absence and an inactive catalog listing"));
        }
        // This is explicit not-listed accounting, NOT an ordinary active-tail
        // observation. The caller has proved the entire interval through W.
        // No real frame, rolling sum or raw seed is fabricated.
        for profile in symbol.profiles.values_mut() {profile.checkpoint.next_open_unix=frontier;}
        Ok(())
    }
    pub(crate) fn activate(&mut self,epoch:&str,profile:&str,frontier:i64)->Result<(),WorkerError> {
        let p=self.symbols.get_mut(epoch).and_then(|s|s.profiles.get_mut(profile)).ok_or_else(||invalid("promoted profile RAM missing"))?;
        if !p.prefix_verified || p.checkpoint.next_open_unix!=frontier {return Err(invalid("promoted profile not ready at watermark"));}
        p.spec.pending=false;p.spec.emit_from_close=frontier+60;p.expected=Some(p.checkpoint.clone());p.checkpoint_verified=true;
        let parts:Vec<Value>=serde_json::from_str(epoch)?;
        self.locations.insert((serde_json::to_string(&parts[..3])?,profile.into()),epoch.into());
        Ok(())
    }
    pub(crate) fn epoch_key(instrument:&AdaptiveInstrument,pending:Option<i64>)->String {
        serde_json::json!([instrument.exchange,instrument.category,instrument.symbol,pending]).to_string()
    }
    pub(crate) fn reconcile(&mut self, wanted:&BTreeMap<String,BTreeSet<String>>) {
        self.symbols.retain(|key,symbol| {
            if let Some(keep)=wanted.get(key) {symbol.profiles.retain(|key,_|keep.contains(key));!symbol.profiles.is_empty()}
            else {false}
        });
        self.locations.retain(|(_,profile),epoch|self.symbols.get(epoch).is_some_and(|s|s.profiles.contains_key(profile)));
    }
    pub(crate) fn needs_reconstruction(&self,key:&str,specs:&[ProfileSpec])->bool {
        self.symbols.get(key).is_some_and(|symbol|specs.iter().any(|spec|
            spec.from_open<symbol.read_from || (!symbol.history.is_empty() && !symbol.profiles.contains_key(&spec.key()))
                || symbol.capacity_minutes<spec.params.baseline_long_days() as usize*1440
                    +spec.params.window_minutes() as usize+spec.params.max_consecutive_gap_minutes() as usize+2))
    }
    pub(crate) fn install(&mut self,key:&str,specs:&[ProfileSpec]) -> Result<(),WorkerError> {
        if specs.is_empty() {return Ok(());}
        let distinct:BTreeSet<_>=specs.iter().map(ProfileSpec::key).collect();
        if distinct.len()>self.limits.max_profiles {return Err(invalid("live profile count exceeds admission budget"));}
        let capacity_minutes=specs.iter().map(|s|s.params.baseline_long_days() as usize*1440
            +s.params.window_minutes() as usize+s.params.max_consecutive_gap_minutes() as usize+2).max().unwrap();
        if !self.symbols.contains_key(key) {
            if self.symbols.len()>=self.limits.max_symbols.saturating_mul(2) {return Err(invalid("live symbol count exceeds admission budget"));}
            let close_bytes=capacity_minutes.checked_mul(self.limits.max_close_bytes_per_minute).ok_or_else(||invalid("history size overflow"))?;
            let required=SharedHistory::required_bytes(capacity_minutes,close_bytes).map_err(shared_error)?;
            let remaining=self.limits.max_history_bytes.saturating_sub(self.retained_history_bytes());
            if required>remaining {return Err(invalid("live shared history exceeds byte admission budget"));}
            self.epoch=self.epoch.checked_add(1).ok_or_else(||invalid("history epoch exhausted"))?;
            let identity=HistoryIdentity(Sha256::digest(serde_json::to_vec(&(key,self.epoch))?).into());
            let history=SharedHistory::new(identity,HistoryLimits {capacity_minutes,max_close_bytes:close_bytes,max_bytes:remaining}).map_err(shared_error)?;
            let read_from=specs.iter().map(|s|s.from_open).min().unwrap();
            self.symbols.insert(key.into(),SymbolState {history,profiles:BTreeMap::new(),read_from,observed_through:read_from,source_token:None,capacity_minutes});
        }
        let symbol=self.symbols.get_mut(key).unwrap();
        let epoch_parts:Vec<Value>=serde_json::from_str(key)?;
        let symbol_key=serde_json::to_string(&epoch_parts[..3])?;
        if symbol.capacity_minutes<capacity_minutes {return Err(invalid("larger live profile requires staged history epoch"));}
        for spec in specs {
            let profile_key=spec.key();
            if let Some(profile)=symbol.profiles.get_mut(&profile_key) {
                profile.spec.recommendation=spec.recommendation.clone();
                profile.spec.publish_enabled=spec.publish_enabled;
                continue;
            }
            // A newly attached profile must verify its entire frozen prefix in
            // this epoch, not inherit another generation's unchecked identity.
            if !symbol.history.is_empty() {return Err(invalid("new profile requires verified staged history epoch"));}
            symbol.profiles.insert(profile_key.clone(),CachedProfile::new(spec.clone())?);
            if !spec.pending {self.locations.insert((symbol_key.clone(),profile_key),key.into());}
        }
        if self.retained_profile_bytes()>self.limits.max_profile_bytes {return Err(invalid("live profile state exceeds byte admission budget"));}
        Ok(())
    }
    pub(crate) fn fetch_bounds(&self,key:&str,target:i64)->Result<(i64,i64),WorkerError> {
        let s=self.symbols.get(key).ok_or_else(||invalid("missing history owner"))?;
        let consumed=s.profiles.values().map(|p|p.checkpoint.next_open_unix).min().unwrap_or(s.read_from);
        Ok((consumed.max(s.read_from),target))
    }
    pub(crate) fn adoption_page(&self,epoch:&str,profile:&str,expected:Option<AdaptiveCheckpoint>)->Result<PreparedProfile,WorkerError> {
        let mut next=self.symbols.get(epoch).and_then(|s|s.profiles.get(profile)).ok_or_else(||invalid("staging consumer missing"))?.clone();
        next.spec.pending=false;next.expected=expected.clone();next.checkpoint_verified=true;
        Ok(PreparedProfile {key:profile.into(),fence:next.spec.fence.clone(),expected,checkpoint:next.checkpoint.clone(),setups:Vec::new(),next})
    }
    pub(crate) fn adopt_staging(&mut self,staging:&str,production:&str,candidate:&str,expected:&BTreeMap<String,AdaptiveCheckpoint>)->Result<(),WorkerError> {
        let staged=self.symbols.get(staging).ok_or_else(||invalid("staging epoch missing"))?;
        if staged.source_token.is_none() || staged.profiles.values().any(|p|!p.prefix_verified) {
            return Err(invalid("staging epoch is not fully verified"));
        }
        if let Some(current)=self.symbols.get(production)
            && current.profiles.keys().any(|key|key!=candidate && !staged.profiles.contains_key(key)) {
            return Err(invalid("active consumer cohort changed while staging"));
        }
        for (key,profile) in &staged.profiles {
            if key!=candidate && expected.get(key)!=Some(&profile.checkpoint) {
                return Err(invalid("incumbent checkpoint differs from staged reconstruction"));
            }
        }
        let mut staged=self.symbols.remove(staging).unwrap();
        if let Some(current)=self.symbols.remove(production) {
            for (key,profile) in &mut staged.profiles {
                if let Some(old)=current.profiles.get(key) {profile.spec.recommendation=old.spec.recommendation.clone();}
            }
        }
        self.locations.retain(|_,epoch|epoch!=staging && epoch!=production);
        let parts:Vec<Value>=serde_json::from_str(production)?;
        let symbol=serde_json::to_string(&parts[..3])?;
        for (key,profile) in &mut staged.profiles {
            if key!=candidate {
                profile.spec.pending=false;profile.expected=Some(profile.checkpoint.clone());profile.checkpoint_verified=true;
                self.locations.insert((symbol.clone(),key.clone()),production.into());
            }
        }
        self.symbols.insert(production.into(),staged);
        Ok(())
    }
    pub(crate) fn remove_profile(&mut self,epoch:&str,candidate:&str) {
        if let Some(symbol)=self.symbols.get_mut(epoch) {symbol.profiles.remove(candidate);}
        self.locations.retain(|(_,profile),owner|owner!=epoch || profile!=candidate);
        if self.symbols.get(epoch).is_some_and(|s|s.profiles.is_empty()) {self.symbols.remove(epoch);}
    }
    pub(crate) fn invalidate(&mut self,key:&str,_profile:Option<&str>) {
        // A lost profile CAS may have retained an anchor while sibling states
        // advanced. Rebuild this symbol's epoch, never reuse that partial ring.
        self.symbols.remove(key);
        self.locations.retain(|_,epoch|epoch!=key);
    }
    pub(crate) fn ready(&self,key:&str,profile:&str,frontier:i64)->bool {
        self.symbols.get(key).and_then(|s|s.profiles.get(profile)).is_some_and(|p|
            p.prefix_verified && p.checkpoint.next_open_unix==frontier && p.last_status==Some(PushStatus::Evaluated))
    }
    /// Build owned effects and speculative scalar clones. History borrows end
    /// here, BEFORE the caller performs persistence or promotion IO.
    pub(crate) fn prepare(&mut self,key:&str,instrument:&AdaptiveInstrument,bars:&[DetectorBar],target:i64,complete:bool)
        ->Result<Vec<PreparedProfile>,WorkerError> {
        if bars.len()>ADAPTIVE_PAGE_LIMIT {return Err(WorkerError::PageLimitExceeded(bars.len()));}
        let symbol=self.symbols.get_mut(key).ok_or_else(||invalid("missing history owner"))?;
        // Source completion can advance a consumer through trailing holes
        // beyond the ring's last REAL bar. Core sees a later fill as an append,
        // but that consumer has already accounted for the minute as missing.
        // Preflight the whole page before mutating history or speculative sums;
        // process_chunk owns invalidation of this symbol epoch on rejection.
        let last_real_open=symbol.history.latest_position()
            .map(|position|symbol.history.open_time(position)).transpose().map_err(shared_error)?;
        if bars.iter().any(|bar|last_real_open.is_none_or(|last|bar.open_time_unix>last)
            && symbol.profiles.values().any(|profile|profile.spec.from_open<=bar.open_time_unix
                && bar.open_time_unix<profile.checkpoint.next_open_unix)) {
            return Err(invalid("trailing consumed gap repair requires history rebuild"));
        }
        let mut next=symbol.profiles.clone();
        let mut setups:BTreeMap<String,Vec<AdaptiveSetup>>=BTreeMap::new();
        for bar in bars {
            if bar.close.len()>128 || bar.quote_turnover.len()>128 {return Err(invalid("source decimal exceeds byte budget"));}
            let anchor=next.values().filter_map(|p|p.detector.as_ref().map(SharedProfile::retention_anchor)).min();
            let position=match anchor {
                Some(anchor)=>symbol.history.push_retaining(bar,target,anchor),
                None=>symbol.history.push(bar,target),
            }.map_err(shared_error)?;
            let frame=symbol.history.frame(position).map_err(shared_error)?.ok_or_else(||invalid("ingested frame missing"))?;
            for (key,profile) in &mut next {
                if bar.open_time_unix<profile.spec.from_open || bar.open_time_unix<profile.checkpoint.next_open_unix {continue;}
                profile.check_boundaries(bar.open_time_unix)?;
                profile.digest.observe(bar.open_time_unix,frame.close(),frame.quote_turnover());
                let detector=match profile.detector.as_mut() {
                    Some(detector)=>detector,
                    None=>profile.detector.insert(SharedProfile::new(profile.spec.params.clone(),symbol.history.identity(),position,None).map_err(shared_error)?),
                };
                let pushed=detector.push(&symbol.history,position).map_err(shared_error)?;
                profile.last_status=Some(pushed.status);
                profile.checkpoint.next_open_unix=bar.open_time_unix+60;
                if let Some(raw)=pushed.setup {
                    let first=profile.cluster.observe_raw(raw.close_time_unix).map_err(|e|invalid(&e.to_string()))?;
                    profile.checkpoint.last_raw_open_unix=Some(raw.open_time_unix);
                    profile.checkpoint.last_raw_cluster_time=Some(raw.close_time_unix);
                    let emit_after=profile.expected.as_ref().filter(|c|c.generation_id==profile.spec.fence.generation_id)
                        .map_or(profile.spec.bootstrap_next,|c|c.next_open_unix);
                    if !profile.spec.pending && profile.spec.publish_enabled && profile.prefix_verified && raw.open_time_unix>=emit_after
                        && raw.close_time_unix>=profile.spec.emit_from_close && (first || profile.spec.cluster_gap_minutes.is_none()) {
                        let mut recommendation=(*profile.spec.recommendation).clone();
                        let shift=recommendation["timeShiftMinutes"].as_i64().ok_or_else(||invalid("missing recommended shift"))?;
                        recommendation["rawSignalAt"]=raw.close_time_unix.into();
                        recommendation["recommendedEntryAt"]=(raw.close_time_unix+shift*60).into();
                        let events=setups.entry(key.clone()).or_default();
                        if events.len()>=ADAPTIVE_PAGE_LIMIT {return Err(invalid("live effect count exceeds page budget"));}
                        events.push(AdaptiveSetup {identity:adaptive_setup_identity(&profile.spec.fence,instrument,raw.close_time_unix),raw,recommendation});
                    }
                }
            }
        }
        let through=if complete {target}else{bars.last().map_or(symbol.observed_through,|bar|bar.open_time_unix+60)};
        let mut prepared=Vec::new();
        for (key,mut profile) in next {
            profile.check_boundaries(through)?;
            // Observation of trailing absence is not detector consumption.
            // Only a real push advances this checkpoint; a later real push
            // accounts for interior gaps according to the detector's rules.
            prepared.push(PreparedProfile {key:key.clone(),fence:profile.spec.fence.clone(),expected:profile.expected.clone(),
                checkpoint:profile.checkpoint.clone(),setups:setups.remove(&key).unwrap_or_default(),next:profile});
        }
        symbol.observed_through=through;
        Ok(prepared)
    }
    pub(crate) fn accept(&mut self,key:&str,mut prepared:PreparedProfile,committed:bool) {
        if committed {prepared.next.expected=Some(prepared.checkpoint.clone());}
        if let Some(symbol)=self.symbols.get_mut(key) {symbol.profiles.insert(prepared.key,prepared.next);}
    }
}
impl Default for AdaptiveRuntime {fn default()->Self {Self::new(AdaptiveRuntimeLimits::default())}}

fn shared_error(error:impl std::fmt::Display)->WorkerError {invalid(&error.to_string())}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use setups_domain::SpikeParams;
    use setups_optimizer::{StudyInstrument,detector_prefix_fingerprint};

    pub(crate) struct Fixture {
        pub runtime:AdaptiveRuntime,
        pub epoch:String,
        pub instrument:AdaptiveInstrument,
        pub spec:ProfileSpec,
        pub bars:Vec<DetectorBar>,
        pub frontier:i64,
    }
    pub(crate) fn fixture()->Fixture {
        let instrument=AdaptiveInstrument {exchange:"test".into(),category:"linear".into(),symbol:"TESTUSDT".into()};
        let frontier=1445*60;
        let bars:Vec<_>=(0..frontier).step_by(60).map(|open_time_unix|DetectorBar {open_time_unix,close:"100".into(),quote_turnover:"1".into()}).collect();
        let digest=detector_prefix_fingerprint(&bars,0,frontier).unwrap();
        let params=AlgorithmParams::Spike(SpikeParams {baseline_long_days:1,baseline_short_days:1,window_minutes:1,
            price_growth_pct:"0".into(),volume_multiplier:"2".into(),cooldown_minutes:1,required_recent_minutes:0,..SpikeParams::default()});
        let spec=ProfileSpec {fence:AdaptiveFence {owner_id:"test".into(),algorithm_id:params.algorithm_id().into(),slot_id:1,generation_id:1,config_revision:1},
            params,recommendation:Arc::new(serde_json::json!({"timeShiftMinutes":3})),cluster_gap_minutes:None,
            from_open:0,bootstrap_next:frontier,bootstrap_raw_open:None,emit_from_close:frontier+60,checkpoint:None,
            prefix:ContinuousInstrumentPrefix {instrument:StudyInstrument {exchange:instrument.exchange.clone(),category:instrument.category.clone(),symbol:instrument.symbol.clone()},
                bar_count:digest.bar_count,sha256:digest.sha256},pending:false,publish_enabled:true};
        let epoch=AdaptiveRuntime::epoch_key(&instrument,None);
        let runtime=AdaptiveRuntime::new(AdaptiveRuntimeLimits {max_history_bytes:8*1024*1024,max_symbols:2,max_profiles:4,
            max_profile_bytes:1024*1024,..AdaptiveRuntimeLimits::default()});
        Fixture {runtime,epoch,instrument,spec,bars,frontier}
    }
    pub(crate) fn feed(runtime:&mut AdaptiveRuntime,epoch:&str,instrument:&AdaptiveInstrument,bars:&[DetectorBar],target:i64)->Result<Vec<AdaptiveSetup>,WorkerError> {
        let mut effects=Vec::new();
        if bars.is_empty() {
            for prepared in runtime.prepare(epoch,instrument,&[],target,true)? {
                let committed=prepared.should_commit();effects.extend(prepared.setups.clone());runtime.accept(epoch,prepared,committed);
            }
        }
        for (index,chunk) in bars.chunks(ADAPTIVE_PAGE_LIMIT).enumerate() {
            for prepared in runtime.prepare(epoch,instrument,chunk,target,(index+1)*ADAPTIVE_PAGE_LIMIT>=bars.len())? {
                let committed=prepared.should_commit();effects.extend(prepared.setups.clone());runtime.accept(epoch,prepared,committed);
            }
        }
        Ok(effects)
    }
    pub(crate) fn mark_legacy_consumed(runtime:&mut AdaptiveRuntime,epoch:&str,through:i64) {
        // Explicit legacy/corrupt-state fixture. Ordinary empty polls MUST NOT
        // manufacture this already-consumed trailing frontier anymore.
        let symbol=runtime.symbols.get_mut(epoch).unwrap();symbol.observed_through=through;
        for profile in symbol.profiles.values_mut() {
            profile.checkpoint.next_open_unix=through;profile.expected=Some(profile.checkpoint.clone());
        }
    }
    fn profile_snapshot(profile:&CachedProfile)->(AdaptiveCheckpoint,Option<AdaptiveCheckpoint>,String,u64) {
        (profile.checkpoint.clone(),profile.expected.clone(),format!("{:?}",profile.detector),profile.digest.count)
    }
    type HistorySnapshot=(u64,u64,Option<u64>,usize,Vec<(i64,Decimal,Decimal,String)>);
    fn history_snapshot(history:&SharedHistory)->HistorySnapshot {
        (history.oldest_position().0,history.end_position().0,history.latest_position().map(|p|p.0),history.len(),
            history.frames_from(history.oldest_position()).unwrap().map(|f|(f.open_time_unix(),f.close(),f.quote_turnover(),f.original_close())).collect())
    }

    #[test]
    fn changed_active_cohort_rejects_staging_adoption_without_replacing_either_epoch() {
        let mut f=fixture();let mut sibling=f.spec.clone();sibling.fence.slot_id=3;sibling.fence.generation_id=3;
        f.runtime.install(&f.epoch,&[f.spec.clone(),sibling.clone()]).unwrap();
        feed(&mut f.runtime,&f.epoch,&f.instrument,&f.bars,f.frontier).unwrap();
        let staged=AdaptiveRuntime::epoch_key(&f.instrument,Some(2));
        let mut shadow=f.spec.clone();shadow.pending=true;
        let mut candidate=shadow.clone();candidate.fence.slot_id=2;candidate.fence.generation_id=2;
        f.runtime.install(&staged,&[shadow,candidate.clone()]).unwrap();
        feed(&mut f.runtime,&staged,&f.instrument,&f.bars,f.frontier).unwrap();
        f.runtime.acknowledge_source(&staged,TailReadToken {epoch:"fixture".into(),revision:"0".into()}).unwrap();
        let production=history_snapshot(&f.runtime.symbols[&f.epoch].history);
        let staging=history_snapshot(&f.runtime.symbols[&staged].history);
        let expected=BTreeMap::from([(f.spec.key(),f.runtime.checkpoint_for(&f.epoch,&f.spec.key()).unwrap())]);
        assert!(f.runtime.adopt_staging(&staged,&f.epoch,&candidate.key(),&expected).unwrap_err().to_string().contains("cohort changed"));
        assert_eq!(history_snapshot(&f.runtime.symbols[&f.epoch].history),production);
        assert_eq!(history_snapshot(&f.runtime.symbols[&staged].history),staging);
        assert_eq!(f.runtime.retained_history_epochs(),2);
    }

    #[test]
    fn trailing_consumed_hole_is_rejected_before_any_shared_history_or_profile_mutation() {
        let mut f=fixture();
        let mut second=f.spec.clone();second.fence.slot_id=2;second.fence.generation_id=2;
        f.runtime.install(&f.epoch,&[f.spec.clone(),second.clone()]).unwrap();
        feed(&mut f.runtime,&f.epoch,&f.instrument,&f.bars,f.frontier).unwrap();
        feed(&mut f.runtime,&f.epoch,&f.instrument,&[],f.frontier+120).unwrap();
        mark_legacy_consumed(&mut f.runtime,&f.epoch,f.frontier+120);
        // Only the SECOND consumer has accounted for this trailing hole. It is
        // unsafe to append to their shared ring just because the first has not.
        let first=f.runtime.symbols.get_mut(&f.epoch).unwrap().profiles.get_mut(&f.spec.key()).unwrap();
        first.checkpoint.next_open_unix=f.frontier;first.expected=Some(first.checkpoint.clone());
        let before_history=history_snapshot(&f.runtime.symbols[&f.epoch].history);
        let before_profiles:Vec<_>=f.runtime.symbols[&f.epoch].profiles.values().map(profile_snapshot).collect();
        let late=DetectorBar {open_time_unix:f.frontier+60,close:"99".into(),quote_turnover:"50".into()};
        let result=f.runtime.prepare(&f.epoch,&f.instrument,&[late],f.frontier+120,true);
        assert!(result.is_err(),"a trailing consumed hole must invalidate, not append then skip profile sums");
        assert_eq!(history_snapshot(&f.runtime.symbols[&f.epoch].history),before_history);
        assert_eq!(f.runtime.symbols[&f.epoch].profiles.values().map(profile_snapshot).collect::<Vec<_>>(),before_profiles);
    }

    #[test]
    fn identical_consumed_real_duplicate_remains_idempotent_after_trailing_holes() {
        let mut f=fixture();f.runtime.install(&f.epoch,&[f.spec.clone()]).unwrap();
        feed(&mut f.runtime,&f.epoch,&f.instrument,&f.bars,f.frontier).unwrap();
        feed(&mut f.runtime,&f.epoch,&f.instrument,&[],f.frontier+120).unwrap();
        mark_legacy_consumed(&mut f.runtime,&f.epoch,f.frontier+120);
        let before_history=history_snapshot(&f.runtime.symbols[&f.epoch].history);
        let before_profile=profile_snapshot(&f.runtime.symbols[&f.epoch].profiles[&f.spec.key()]);
        let prepared=f.runtime.prepare(&f.epoch,&f.instrument,&[f.bars.last().unwrap().clone()],f.frontier+120,true).unwrap();
        assert_eq!(prepared.len(),1);assert!(prepared[0].setups.is_empty());assert!(!prepared[0].should_commit());
        for profile in prepared {f.runtime.accept(&f.epoch,profile,false);}
        assert_eq!(history_snapshot(&f.runtime.symbols[&f.epoch].history),before_history);
        assert_eq!(profile_snapshot(&f.runtime.symbols[&f.epoch].profiles[&f.spec.key()]),before_profile);
    }

    #[test]
    fn clean_rebuild_after_trailing_repair_matches_scalar_state_and_reference_future_outputs() {
        let mut f=fixture();f.runtime.install(&f.epoch,&[f.spec.clone()]).unwrap();
        feed(&mut f.runtime,&f.epoch,&f.instrument,&f.bars,f.frontier).unwrap();
        feed(&mut f.runtime,&f.epoch,&f.instrument,&[],f.frontier+120).unwrap();
        mark_legacy_consumed(&mut f.runtime,&f.epoch,f.frontier+120);
        let durable=f.runtime.checkpoint_for(&f.epoch,&f.spec.key()).unwrap();
        let late=DetectorBar {open_time_unix:f.frontier+60,close:"99".into(),quote_turnover:"50".into()};
        assert!(f.runtime.prepare(&f.epoch,&f.instrument,std::slice::from_ref(&late),f.frontier+120,true).is_err());
        f.runtime.invalidate(&f.epoch,None);
        assert_eq!(f.runtime.retained_history_bytes(),0);assert_eq!(f.runtime.retained_profile_bytes(),0);
        assert!(f.runtime.profile_epoch(&f.instrument,&f.spec.key()).is_none());
        f.spec.checkpoint=Some(durable.clone());f.runtime.install(&f.epoch,&[f.spec.clone()]).unwrap();
        let mut repaired=f.bars.clone();repaired.push(late);
        assert!(feed(&mut f.runtime,&f.epoch,&f.instrument,&repaired,f.frontier+120).unwrap().is_empty());
        assert_eq!(f.runtime.checkpoint_for(&f.epoch,&f.spec.key()),Some(durable));
        let symbol=&f.runtime.symbols[&f.epoch];
        let mut history=SharedHistory::new(symbol.history.identity(),HistoryLimits {capacity_minutes:symbol.capacity_minutes,
            max_close_bytes:symbol.capacity_minutes*48,max_bytes:8*1024*1024}).unwrap();
        let mut scalar=SharedProfile::new(f.spec.params.clone(),history.identity(),setups_detector::shared::HistoryPosition(0),None).unwrap();
        let mut reference=PriceVolumeSpikeDetector::with_params(f.spec.params.clone(),None).unwrap();
        for bar in &repaired {
            let position=history.push(bar,f.frontier+120).unwrap();scalar.push(&history,position).unwrap();reference.push(bar).unwrap();
        }
        assert_eq!(format!("{:?}",symbol.profiles[&f.spec.key()].detector.as_ref().unwrap()),format!("{scalar:?}"),
            "all rolling sums and scalar cursors must equal an independent clean materialization");
        for minute in 0..16 {
            let bar=DetectorBar {open_time_unix:f.frontier+120+minute*60,close:"100".into(),
                quote_turnover:["2.05","100","1","3"][minute as usize%4].into()};
            let expected=reference.push(&bar).unwrap();
            let effects=feed(&mut f.runtime,&f.epoch,&f.instrument,std::slice::from_ref(&bar),bar.open_time_unix+60).unwrap();
            assert_eq!(effects.into_iter().map(|effect|effect.raw).collect::<Vec<_>>(),expected.setup.into_iter().collect::<Vec<_>>());
        }
    }
}
