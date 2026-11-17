//! Runtime-owned parsed history, scalar profile state and speculative effects.
//! Nothing in this module performs IO or keeps a borrowed frame across await.
use super::*;
use setups_detector::shared::{HistoryIdentity, HistoryLimits, HistoryPosition, ProfileRetention, SharedDetectorError, SharedHistory, SharedProfile};
use setups_domain::{PushStatus, SkipReason};
#[cfg(test)]
use setups_domain::decimal::Decimal;
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
    /// Stable live-owned warmup target, unrelated to the research bootstrap.
    pub warmup_through: i64,
    pub emit_from_close: i64,
    pub checkpoint: Option<AdaptiveCheckpoint>,
    pub pending: bool,
    /// Durable per-instrument readiness, independent of a temporary shadow mute.
    pub waiting: bool,
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
struct CachedProfile {
    spec: ProfileSpec,
    detector: Option<SharedProfile>,
    cluster: CausalCluster,
    checkpoint: AdaptiveCheckpoint,
    expected: Option<AdaptiveCheckpoint>,
    initialized: bool,
    checkpoint_verified: bool,
    last_status: Option<PushStatus>,
    last_skip: Option<SkipReason>,
}
impl CachedProfile {
    fn new(spec: ProfileSpec) -> Result<Self, WorkerError> {
        let cluster=CausalCluster::new(i64::from(spec.cluster_gap_minutes.unwrap_or(1))*60,None)
            .map_err(|error|invalid(&error.to_string()))?;
        let checkpoint=AdaptiveCheckpoint {generation_id:spec.fence.generation_id,next_open_unix:spec.from_open,
            last_raw_open_unix:None,last_raw_cluster_time:None};
        let expected=spec.checkpoint.clone();
        Ok(Self {spec,detector:None,cluster,checkpoint,expected,initialized:false,
            checkpoint_verified:false,last_status:None,last_skip:None})
    }
    fn check_boundaries(&mut self, through_open: i64) -> Result<(),WorkerError> {
        // Only the current live-owned interval matters. Research fingerprints
        // and raw seeds are immutable evidence, never operational live state.
        if through_open>=self.spec.warmup_through {self.initialized=true;}
        if !self.checkpoint_verified && let Some(expected)=&self.expected
            && expected.generation_id==self.spec.fence.generation_id && through_open>=expected.next_open_unix {
            // The durable cursor is the publication boundary, not proof that
            // its derived cooldown/cluster seeds still match canonical data.
            // Reconstruct them from this profile's live origin and guarded
            // input, suppress pre-cursor effects, then CAS the fresh state.
            // A current-source repair can change cooldown indefinitely.
            self.checkpoint_verified=true;
        }
        Ok(())
    }
    fn retained_bytes(&self, key_len:usize) -> usize {
        size_of::<Self>()+self.detector.as_ref().map_or(0,SharedProfile::retained_bytes)
            + key_len
    }
    fn retention_for_bar(&self,bar:&DetectorBar,position:HistoryPosition)->Option<ProfileRetention> {
        self.detector.as_ref().map(|detector| {
            if bar.open_time_unix>=self.spec.from_open && bar.open_time_unix>=self.checkpoint.next_open_unix {
                detector.retention_for_next(position)
            }else{
                // A consumer skipped by this page still owns its departure
                // samples. Never release a held/CAS predecessor's anchor.
                ProfileRetention {anchor:detector.retention_anchor(),resets_for_gap:false}
            }
        })
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
    pub(crate) fn retained_scalar_bytes(&self)->usize { self.next.retained_bytes(self.key.len())+self.key.len() }
    pub(crate) fn should_commit(&self)->bool {
        !self.next.spec.pending && !self.next.spec.waiting && self.next.initialized
            && (self.expected.as_ref()!=Some(&self.checkpoint) || !self.setups.is_empty())
            && self.checkpoint.next_open_unix>=self.next.spec.warmup_through
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
impl SymbolState {
    fn history_failure_fields(&self,epoch:&str,instrument:&AdaptiveInstrument,profile:Option<&CachedProfile>,
        bar:&DetectorBar,position:Option<HistoryPosition>,stage:&str,error:&SharedDetectorError,duration_ms:u64)->Value {
        let (code,requested)=match error {
            SharedDetectorError::RetentionRejected {requested,..}=>("history_retention_rejected",Some(requested.0)),
            SharedDetectorError::Evicted {requested,..}=>("history_lookup_evicted",Some(requested.0)),
            SharedDetectorError::Sequence {..}=>("history_consumer_sequence",None),
            SharedDetectorError::Future(_)=>("history_future_position",None),
            SharedDetectorError::Capacity {..}=>("history_capacity",None),
            SharedDetectorError::WrongHistory=>("history_identity",None),
            SharedDetectorError::Decimal(_)=>("history_decimal",None),
            SharedDetectorError::Invalid(_)=>("history_validation",None),
        };
        let previous=profile.and_then(|p|p.detector.as_ref()).and_then(SharedProfile::last_position);
        let retention=profile.and_then(|p|position.and_then(|position|p.retention_for_bar(bar,position)));
        let projected_end=position.map(|position|position.0.saturating_add(1).max(self.history.end_position().0));
        let projected_oldest=projected_end.map(|end|end.saturating_sub(self.capacity_minutes as u64).max(self.history.oldest_position().0));
        serde_json::json!({"component":"adaptive_history","stage":stage,"error_code":code,
            "instrument_key":format!("{}/{}/{}",instrument.exchange,instrument.category,instrument.symbol),"history_epoch":epoch,
            "owner_id":profile.map(|p|p.spec.fence.owner_id.as_str()),"algorithm_id":profile.map(|p|p.spec.fence.algorithm_id.as_str()),
            "profile_id":profile.map(|p|p.spec.fence.slot_id),"generation_id":profile.map(|p|p.spec.fence.generation_id),
            "study_id":profile.and_then(|p|p.spec.recommendation["studyId"].as_i64()),"control_revision":profile.map(|p|p.spec.fence.config_revision),
            "anchor_owner":if stage=="history_ingest" {"profile_id/generation_id"}else{"failing_consumer"},
            "previous_position":previous.map(|p|p.0),"previous_open_unix":previous.and_then(|p|self.history.open_time(p).ok()),
            "incoming_open_unix":bar.open_time_unix,"incoming_position":position.map(|p|p.0),
            "current_oldest":self.history.oldest_position().0,"current_end":self.history.end_position().0,
            "projected_oldest":projected_oldest,"projected_end":projected_end,"capacity_minutes":self.capacity_minutes,
            "requested_position":requested,"required_anchor":retention.map(|r|r.anchor.0),"reset_eligible":retention.map(|r|r.resets_for_gap),
            "pending":profile.map(|p|p.spec.pending),"duration_ms":duration_ms})
    }
    fn history_failure(&self,epoch:&str,instrument:&AdaptiveInstrument,profile:Option<&CachedProfile>,
        bar:&DetectorBar,position:Option<HistoryPosition>,stage:&str,error:SharedDetectorError,started:std::time::Instant)->WorkerError {
        let fields=self.history_failure_fields(epoch,instrument,profile,bar,position,stage,&error,started.elapsed().as_millis() as u64);
        setups_db::diagnostics::emit(setups_db::diagnostics::Level::Error,"adaptive_history_failed",fields.clone());
        // The durable error has a short actionable identity; the common log
        // carries bounded numeric context, never source decimals/payloads.
        let detail=if matches!(error,SharedDetectorError::Decimal(_)) {"detector decimal validation/arithmetic failed".into()}else{error.to_string()};
        invalid(&format!("{}: symbol={} profile={} generation={} stage={stage}: {detail}",
            fields["error_code"].as_str().unwrap_or("history_validation"),instrument.symbol.chars().take(48).collect::<String>(),
            fields["profile_id"],fields["generation_id"]))
    }
}

/// Sole owner of retained adaptive RAM. Drop/reconcile releases actual history
/// allocations; there is no process-global map, timer cache or hidden singleton.
pub struct AdaptiveRuntime {
    pub limits: AdaptiveRuntimeLimits,
    symbols: BTreeMap<String,SymbolState>,
    locations: BTreeMap<(String,String),String>,
    epoch: u64,
    // Valid only until an accounted profile footprint changes; the public limit
    // must also match. Exact accounting remains independent of this admission proof.
    profile_budget_validated_at_limit: Option<usize>,
    #[cfg(test)]
    profile_budget_checks: usize,
}
impl AdaptiveRuntime {
    pub fn new(limits: AdaptiveRuntimeLimits) -> Self {Self {limits,symbols:BTreeMap::new(),locations:BTreeMap::new(),epoch:0,
        profile_budget_validated_at_limit:None,#[cfg(test)] profile_budget_checks:0}}
    pub fn retained_history_epochs(&self)->usize {self.symbols.len()}
    pub fn retained_history_bytes(&self)->usize {self.symbols.values().map(|s|s.history.retained_bytes()).sum()}
    pub fn retained_profile_bytes(&self)->usize {self.symbols.values().flat_map(|s|s.profiles.iter()).map(|(key,p)|p.retained_bytes(key.len())).sum()}
    pub fn clear(&mut self) {self.profile_budget_validated_at_limit=None;self.symbols.clear();self.locations.clear();}
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
        let to=symbol.profiles.values().map(|p|p.checkpoint.next_open_unix).max().unwrap_or(symbol.read_from);
        let from=symbol.profiles.values().map(|p|p.spec.from_open).min().unwrap_or(symbol.read_from);
        Ok(TailReadGuardQuery {instrument_id,from_unix:from,to_unix:to,previous:symbol.source_token.clone()})
    }
    pub(crate) fn warmup_boundary(&self,epoch:&str)->Result<i64,WorkerError> {
        self.symbols.get(epoch).and_then(|s|s.profiles.values().map(|p|p.spec.warmup_through).max()).ok_or_else(||invalid("missing live warmup boundary"))
    }
    pub(crate) fn acknowledge_source(&mut self,epoch:&str,token:TailReadToken)->Result<(),WorkerError> {
        token.validate().map_err(shared_error)?;
        self.symbols.get_mut(epoch).ok_or_else(||invalid("missing source history owner"))?.source_token=Some(token);
        Ok(())
    }
    pub(crate) fn is_current(&self,epoch:&str,through:i64)->bool {
        self.symbols.get(epoch).is_some_and(|s|s.source_token.is_some()
            && s.profiles.values().all(|p|p.initialized && p.checkpoint.next_open_unix>=through))
    }
    pub(crate) fn activate(&mut self,epoch:&str,profile:&str,frontier:i64)->Result<(),WorkerError> {
        let p=self.symbols.get_mut(epoch).and_then(|s|s.profiles.get_mut(profile)).ok_or_else(||invalid("promoted profile RAM missing"))?;
        if !p.initialized || p.checkpoint.next_open_unix!=frontier {return Err(invalid("promoted profile not ready at watermark"));}
        p.spec.pending=false;p.spec.waiting=false;p.spec.emit_from_close=frontier+60;p.expected=Some(p.checkpoint.clone());p.checkpoint_verified=true;
        let parts:Vec<Value>=serde_json::from_str(epoch)?;
        self.locations.insert((serde_json::to_string(&parts[..3])?,profile.into()),epoch.into());
        Ok(())
    }
    pub(crate) fn epoch_key(instrument:&AdaptiveInstrument,pending:Option<i64>)->String {
        serde_json::json!([instrument.exchange,instrument.category,instrument.symbol,pending]).to_string()
    }
    pub(crate) fn reconcile(&mut self, wanted:&BTreeMap<String,BTreeSet<String>>) {
        let validated=&mut self.profile_budget_validated_at_limit;
        self.symbols.retain(|key,symbol| {
            if let Some(keep)=wanted.get(key) {
                symbol.profiles.retain(|key,_| {
                    let retain=keep.contains(key);if !retain {*validated=None;} retain
                });
                if symbol.profiles.is_empty() {*validated=None;false}else{true}
            }else {*validated=None;false}
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
                profile.spec.waiting=spec.waiting;
                profile.spec.emit_from_close=spec.emit_from_close;
                continue;
            }
            // A newly attached profile must replay from its OWN live origin;
            // it cannot inherit another generation's cooldown/cluster state.
            if !symbol.history.is_empty() {return Err(invalid("new profile requires verified staged history epoch"));}
            self.profile_budget_validated_at_limit=None;
            symbol.profiles.insert(profile_key.clone(),CachedProfile::new(spec.clone())?);
            if !spec.pending {self.locations.insert((symbol_key.clone(),profile_key),key.into());}
        }
        if self.profile_budget_validated_at_limit!=Some(self.limits.max_profile_bytes) {
            self.profile_budget_validated_at_limit=None;
            #[cfg(test)] {self.profile_budget_checks+=1;}
            if self.retained_profile_bytes()>self.limits.max_profile_bytes {return Err(invalid("live profile state exceeds byte admission budget"));}
            self.profile_budget_validated_at_limit=Some(self.limits.max_profile_bytes);
        }
        Ok(())
    }
    pub(crate) fn fetch_bounds(&self,key:&str,target:i64)->Result<(i64,i64),WorkerError> {
        let s=self.symbols.get(key).ok_or_else(||invalid("missing history owner"))?;
        let consumed=s.profiles.values().map(|p|p.checkpoint.next_open_unix).min().unwrap_or(s.read_from);
        Ok((consumed.max(s.read_from),target))
    }
    pub(crate) fn adopt_staging(&mut self,staging:&str,production:&str,candidate:&str,expected:&BTreeMap<String,AdaptiveCheckpoint>)->Result<bool,WorkerError> {
        let staged=self.symbols.get(staging).ok_or_else(||invalid("staging epoch missing"))?;
        if staged.source_token.is_none() || staged.profiles.values().any(|p|!p.initialized) {
            return Err(invalid("staging epoch is not fully verified"));
        }
        if let Some(current)=self.symbols.get(production)
            && current.profiles.keys().any(|key|key!=candidate && !staged.profiles.contains_key(key)) {
            return Ok(false);
        }
        for (key,profile) in &staged.profiles {
            if key!=candidate && expected.get(key)!=Some(&profile.checkpoint) {
                return Ok(false);
            }
        }
        self.profile_budget_validated_at_limit=None;
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
                profile.spec.pending=false;
                if !profile.spec.waiting {profile.expected=Some(profile.checkpoint.clone());profile.checkpoint_verified=true;}
                self.locations.insert((symbol.clone(),key.clone()),production.into());
            }
        }
        self.symbols.insert(production.into(),staged);
        Ok(true)
    }
    pub(crate) fn remove_profile(&mut self,epoch:&str,candidate:&str) {
        if let Some(symbol)=self.symbols.get_mut(epoch) && symbol.profiles.contains_key(candidate) {
            self.profile_budget_validated_at_limit=None;symbol.profiles.remove(candidate);
        }
        self.locations.retain(|(_,profile),owner|owner!=epoch || profile!=candidate);
        if self.symbols.get(epoch).is_some_and(|s|s.profiles.is_empty()) {
            self.profile_budget_validated_at_limit=None;self.symbols.remove(epoch);
        }
    }
    pub(crate) fn invalidate(&mut self,key:&str,_profile:Option<&str>) {
        // A lost profile CAS may have retained an anchor while sibling states
        // advanced. Rebuild this symbol's epoch, never reuse that partial ring.
        if self.symbols.contains_key(key) {self.profile_budget_validated_at_limit=None;self.symbols.remove(key);}
        self.locations.retain(|_,epoch|epoch!=key);
    }
    pub(crate) fn ready(&self,key:&str,profile:&str,frontier:i64)->bool {
        self.symbols.get(key).and_then(|s|s.profiles.get(profile)).is_some_and(|p|
            p.initialized && p.checkpoint.next_open_unix==frontier && p.last_status==Some(PushStatus::Evaluated)
            && !matches!(p.last_skip,Some(SkipReason::GapToleranceExceeded | SkipReason::PriceReferenceMissing)))
    }
    pub(crate) fn waiting_reason(&self,key:&str,profile:&str,frontier:i64)->Option<&'static str> {
        if self.ready(key,profile,frontier) {return None;}
        let p=self.symbols.get(key)?.profiles.get(profile)?;
        Some(if p.checkpoint.next_open_unix<frontier {"waiting_tail"} else {
            match p.last_skip {
                Some(SkipReason::RecentGap)=>"recent_gap",
                Some(SkipReason::GapToleranceExceeded)=>"gap_tolerance",
                Some(SkipReason::PriceReferenceMissing)=>"missing_price_reference",
                _=>"insufficient_history",
            }
        })
    }
    /// Build owned effects and speculative scalar clones. History borrows end
    /// here, BEFORE the caller performs persistence or promotion IO.
    pub(crate) fn prepare(&mut self,key:&str,instrument:&AdaptiveInstrument,bars:&[DetectorBar],target:i64,complete:bool)
        ->Result<Vec<PreparedProfile>,WorkerError> {
        let started=std::time::Instant::now();
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
            let incoming=if symbol.history.is_empty() {HistoryPosition(0)}else{
                symbol.history.position_for_open(bar.open_time_unix).map_err(|error|
                    symbol.history_failure(key,instrument,None,bar,None,"history_position",error,started))?
            };
            let anchor=next.values().filter_map(|p|p.retention_for_bar(bar,incoming).map(|retention|(p,retention)))
                .min_by_key(|(_,retention)|retention.anchor);
            if let Some((profile,retention))=anchor.filter(|(_,retention)|retention.resets_for_gap)
                && setups_db::diagnostics::debug_enabled() {
                setups_db::diagnostics::emit(setups_db::diagnostics::Level::Debug,"adaptive_history_gap_reset",serde_json::json!({
                    "stage":"history_ingest","instrument_key":format!("{}/{}/{}",instrument.exchange,instrument.category,instrument.symbol),
                    "history_epoch":key,"profile_id":profile.spec.fence.slot_id,"generation_id":profile.spec.fence.generation_id,
                    "study_id":profile.spec.recommendation["studyId"],"previous_position":profile.detector.as_ref().and_then(SharedProfile::last_position).map(|p|p.0),
                    "incoming_position":incoming.0,"incoming_open_unix":bar.open_time_unix,"released_anchor":profile.detector.as_ref().map(SharedProfile::retention_anchor).map(|p|p.0),
                    "required_anchor":retention.anchor.0,"capacity_minutes":symbol.capacity_minutes}));
            }
            let position=match anchor {
                Some((_,retention))=>symbol.history.push_retaining(bar,target,retention.anchor),
                None=>symbol.history.push(bar,target),
            }.map_err(|error|symbol.history_failure(key,instrument,anchor.map(|(profile,_)|profile),bar,Some(incoming),"history_ingest",error,started))?;
            for (profile_key,profile) in &mut next {
                if bar.open_time_unix<profile.spec.from_open || bar.open_time_unix<profile.checkpoint.next_open_unix {continue;}
                profile.check_boundaries(bar.open_time_unix)?;
                let detector=match profile.detector.as_mut() {
                    Some(detector)=>detector,
                    None=>profile.detector.insert(SharedProfile::new(profile.spec.params.clone(),symbol.history.identity(),position,None).map_err(shared_error)?),
                };
                let pushed=detector.push(&symbol.history,position).map_err(|error|
                    symbol.history_failure(key,instrument,Some(profile),bar,Some(position),"detector_advance",error,started))?;
                profile.last_status=Some(pushed.status);
                profile.last_skip=pushed.skip_reason;
                profile.checkpoint.next_open_unix=bar.open_time_unix+60;
                if let Some(raw)=pushed.setup {
                    let first=profile.cluster.observe_raw(raw.close_time_unix).map_err(|e|invalid(&e.to_string()))?;
                    profile.checkpoint.last_raw_open_unix=Some(raw.open_time_unix);
                    profile.checkpoint.last_raw_cluster_time=Some(raw.close_time_unix);
                    let emit_after=profile.expected.as_ref().filter(|c|c.generation_id==profile.spec.fence.generation_id)
                        .map_or(profile.spec.warmup_through,|c|c.next_open_unix);
                    if !profile.spec.pending && !profile.spec.waiting && profile.spec.publish_enabled && profile.initialized && raw.open_time_unix>=emit_after
                        && raw.close_time_unix>=profile.spec.emit_from_close && (first || profile.spec.cluster_gap_minutes.is_none()) {
                        let mut recommendation=(*profile.spec.recommendation).clone();
                        let shift=recommendation["timeShiftMinutes"].as_i64().ok_or_else(||invalid("missing recommended shift"))?;
                        recommendation["rawSignalAt"]=raw.close_time_unix.into();
                        recommendation["recommendedEntryAt"]=(raw.close_time_unix+shift*60).into();
                        let events=setups.entry(profile_key.clone()).or_default();
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
        if let Some(symbol)=self.symbols.get_mut(key) {
            let bytes=prepared.next.retained_bytes(prepared.key.len());
            if symbol.profiles.get(&prepared.key).is_none_or(|old|old.retained_bytes(prepared.key.len())!=bytes) {
                self.profile_budget_validated_at_limit=None;
            }
            symbol.profiles.insert(prepared.key,prepared.next);
        }
    }
}
impl Default for AdaptiveRuntime {fn default()->Self {Self::new(AdaptiveRuntimeLimits::default())}}

fn shared_error(error:impl std::fmt::Display)->WorkerError {invalid(&error.to_string())}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use setups_domain::SpikeParams;

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
        let params=AlgorithmParams::Spike(SpikeParams {baseline_long_days:1,baseline_short_days:1,window_minutes:1,
            price_growth_pct:"0".into(),volume_multiplier:"2".into(),cooldown_minutes:1,required_recent_minutes:0,..SpikeParams::default()});
        let spec=ProfileSpec {fence:AdaptiveFence {owner_id:"test".into(),algorithm_id:params.algorithm_id().into(),slot_id:1,generation_id:1,config_revision:1},
            params,recommendation:Arc::new(serde_json::json!({"timeShiftMinutes":3})),cluster_gap_minutes:None,
            from_open:0,warmup_through:frontier,emit_from_close:frontier+60,checkpoint:None,pending:false,waiting:false,publish_enabled:true};
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
    fn profile_snapshot(profile:&CachedProfile)->(AdaptiveCheckpoint,Option<AdaptiveCheckpoint>,String,bool) {
        (profile.checkpoint.clone(),profile.expected.clone(),format!("{:?}",profile.detector),profile.initialized)
    }
    type HistorySnapshot=(u64,u64,Option<u64>,usize,Vec<(i64,Decimal,Decimal,String)>);
    fn history_snapshot(history:&SharedHistory)->HistorySnapshot {
        (history.oldest_position().0,history.end_position().0,history.latest_position().map(|p|p.0),history.len(),
            history.frames_from(history.oldest_position()).unwrap().map(|f|(f.open_time_unix(),f.close(),f.quote_turnover(),f.original_close())).collect())
    }

    #[test]
    fn history_failure_records_keep_real_epoch_anchor_owner_and_bounds_within_sink_budget() {
        let mut f=fixture();f.instrument.symbol="10000SATSUSDT".into();
        f.epoch=AdaptiveRuntime::epoch_key(&f.instrument,Some(30));
        f.spec.fence.owner_id="diagnostic-owner".into();f.spec.fence.slot_id=4;f.spec.fence.generation_id=17;
        f.spec.fence.config_revision=5;f.spec.recommendation=Arc::new(serde_json::json!({"studyId":21}));
        f.runtime.install(&f.epoch,std::slice::from_ref(&f.spec)).unwrap();
        feed(&mut f.runtime,&f.epoch,&f.instrument,&f.bars,f.frontier).unwrap();
        let symbol=&f.runtime.symbols[&f.epoch];let profile=&symbol.profiles[&f.spec.key()];
        let input=DetectorBar {open_time_unix:3000*60,close:"not-for-logs".into(),quote_turnover:"not-for-logs".into()};
        for (error,stage,code) in [
            (SharedDetectorError::RetentionRejected {requested:HistoryPosition(2761),projected_oldest:HistoryPosition(2921)},"history_ingest","history_retention_rejected"),
            (SharedDetectorError::Evicted {requested:HistoryPosition(2761),oldest:HistoryPosition(2921)},"detector_advance","history_lookup_evicted"),
        ] {
            let fields=symbol.history_failure_fields(&f.epoch,&f.instrument,Some(profile),&input,Some(HistoryPosition(3000)),stage,&error,123);
            let record=setups_db::diagnostics::record(setups_db::diagnostics::Level::Error,"adaptive_history_failed",&fields);
            assert_eq!(record["history_epoch"],f.epoch);assert_ne!(record["history_epoch"],f.spec.key());
            assert_eq!(record["error_code"],code);assert_eq!(record["profile_id"],4);assert_eq!(record["generation_id"],17);
            assert_eq!(record["study_id"],21);assert_eq!(record["control_revision"],5);
            for key in ["owner_id","incoming_open_unix","incoming_position","previous_position","previous_open_unix",
                "current_oldest","current_end","projected_oldest","projected_end","capacity_minutes","reset_eligible","required_anchor"] {
                assert_eq!(record[key],fields[key],"critical history field {key} was lost");assert!(!record[key].is_null(),"{key}");
            }
            assert!(record.get("fields_truncated").is_none(),"{record}");
            assert!(record.to_string().len()<setups_db::diagnostics::MAX_RECORD_BYTES);
            assert!(!record.to_string().contains("not-for-logs"));
        }
        let mut held=profile.clone();held.checkpoint.next_open_unix=input.open_time_unix+60;
        let retained=held.retention_for_bar(&input,HistoryPosition(3000)).unwrap();
        assert!(!retained.resets_for_gap);assert_eq!(retained.anchor,held.detector.as_ref().unwrap().retention_anchor());
    }

    #[test]
    fn incident_gap_with_six_incumbent_shadows_and_each_candidate_preserves_parity_muting_and_cleanup() {
        // Compact deterministic reconstruction of the Nov17 incident: gen17
        // owns the 30-day ring; its anchor2761 must not block a 194-minute reset.
        const ORIGIN:i64=1792045920;
        let instrument=AdaptiveInstrument {exchange:"bybit".into(),category:"linear".into(),symbol:"10000SATSUSDT".into()};
        let bars:Vec<_>=(0..47500).filter(|minute|!matches!(minute,1000|1100|45961..=46154)).map(|minute|DetectorBar {
            open_time_unix:ORIGIN+minute*60,close:if minute%43==0 {"125.000001"}else{"100"}.into(),
            quote_turnover:if minute%43==0 {"1000.000001"}else{"1.000001"}.into()}).collect();
        for candidate in [30,31] {
            let rows=[(17,4,30,6,18,"9","2",1792045920,Some(480)),(18,5,14,5,12,"4","20",1793394600,Some(1320)),
                (19,6,14,12,4,"6","9",1793477340,Some(480)),(20,1,25,7,9,"8","10",1792490880,Some(1320)),
                (21,2,18,12,18,"9","2",1793184480,Some(240)),(22,3,20,6,12,"6","10",1792965300,Some(1320)),
                if candidate==30 {(30,4,14,12,3,"4","7",1793675520,None)}else{(31,5,25,3,12,"5","7",1792739040,None)}];
            let target=ORIGIN+47500*60;
            let specs:Vec<_>=rows.iter().map(|&(generation,slot,long,short,window,growth,multiplier,from,cluster)|ProfileSpec {
                fence:AdaptiveFence {owner_id:"incident-fixture".into(),algorithm_id:"price_volume_spike".into(),slot_id:slot,generation_id:generation,config_revision:5},
                params:AlgorithmParams::Spike(SpikeParams {baseline_long_days:long,baseline_short_days:short,window_minutes:window,
                    price_growth_pct:growth.into(),volume_multiplier:multiplier.into(),cooldown_minutes:5,max_gap_percent:"10".into(),
                    max_consecutive_gap_minutes:15,required_recent_minutes:30}),
                recommendation:Arc::new(serde_json::json!({"timeShiftMinutes":0})),cluster_gap_minutes:cluster,from_open:from,
                warmup_through:target-60,emit_from_close:target,checkpoint:None,pending:true,waiting:false,publish_enabled:true,
            }).collect();
            let mut f=fixture();let healthy_key=f.spec.key();
            f.runtime.install(&f.epoch,std::slice::from_ref(&f.spec)).unwrap();feed(&mut f.runtime,&f.epoch,&f.instrument,&f.bars,f.frontier).unwrap();
            let healthy=profile_snapshot(&f.runtime.symbols[&f.epoch].profiles[&healthy_key]);
            let healthy_identity=f.runtime.symbols[&f.epoch].history.identity();
            f.runtime.limits.max_profiles=8;
            let epoch=AdaptiveRuntime::epoch_key(&instrument,Some(candidate));f.runtime.install(&epoch,&specs).unwrap();
            assert_eq!(f.runtime.symbols[&epoch].capacity_minutes,43235);
            let bytes=f.runtime.retained_history_bytes();
            assert!(feed(&mut f.runtime,&epoch,&instrument,&bars,target).unwrap().is_empty(),"warmup must not publish old signals");
            assert_eq!(f.runtime.retained_history_bytes(),bytes);
            for spec in &specs {
                let mut reference=PriceVolumeSpikeDetector::with_params(spec.params.clone(),None).unwrap();
                let mut last=None;let mut result=None;
                for bar in bars.iter().filter(|bar|bar.open_time_unix>=spec.from_open) {
                    let pushed=reference.push(bar).unwrap();if let Some(raw)=&pushed.setup {last=Some(raw.open_time_unix);}result=Some(pushed);
                }
                let cached=&f.runtime.symbols[&epoch].profiles[&spec.key()];let result=result.unwrap();
                assert_eq!(cached.checkpoint.last_raw_open_unix,last,"generation {} cooldown",spec.fence.generation_id);
                assert_eq!(cached.detector.as_ref().unwrap().last_raw_open_unix(),last);
                assert_eq!(cached.last_status,Some(result.status));assert_eq!(cached.last_skip,result.skip_reason);
                assert_eq!(cached.checkpoint.next_open_unix,target);
                assert_eq!(f.runtime.waiting_reason(&epoch,&spec.key(),target),Some("insufficient_history"));
            }
            assert_eq!(profile_snapshot(&f.runtime.symbols[&f.epoch].profiles[&healthy_key]),healthy);
            assert_eq!(f.runtime.symbols[&f.epoch].history.identity(),healthy_identity);
            f.runtime.invalidate(&epoch,None);assert!(!f.runtime.has_epoch(&epoch));assert_eq!(f.runtime.retained_history_epochs(),1);
            f.runtime.clear();assert_eq!(f.runtime.retained_history_bytes(),0);assert_eq!(f.runtime.retained_profile_bytes(),0);assert!(f.runtime.locations.is_empty());
        }
    }

    #[test]
    fn repeated_install_and_unchanged_reconcile_refresh_controls_without_rescanning_or_resetting() {
        let mut f=fixture();let key=f.spec.key();
        f.runtime.install(&f.epoch,std::slice::from_ref(&f.spec)).unwrap();
        feed(&mut f.runtime,&f.epoch,&f.instrument,&f.bars,f.frontier).unwrap();
        f.runtime.install(&f.epoch,std::slice::from_ref(&f.spec)).unwrap();
        let checks=f.runtime.profile_budget_checks;
        let before=profile_snapshot(&f.runtime.symbols[&f.epoch].profiles[&key]);
        let history=history_snapshot(&f.runtime.symbols[&f.epoch].history);
        let identity=f.runtime.symbols[&f.epoch].history.identity();
        let bytes=f.runtime.retained_profile_bytes();
        let wanted=BTreeMap::from([(f.epoch.clone(),BTreeSet::from([key.clone()]))]);
        let mut fresh=f.spec.clone();fresh.waiting=true;fresh.publish_enabled=false;fresh.emit_from_close+=600;
        fresh.recommendation=Arc::new(serde_json::json!({"timeShiftMinutes":7,"refreshed":true}));
        for _ in 0..32 {
            f.runtime.reconcile(&wanted);
            f.runtime.install(&f.epoch,std::slice::from_ref(&fresh)).unwrap();
        }
        assert_eq!(f.runtime.profile_budget_checks,checks);
        let profile=&f.runtime.symbols[&f.epoch].profiles[&key];
        assert!(Arc::ptr_eq(&profile.spec.recommendation,&fresh.recommendation));
        assert!(profile.spec.waiting);assert!(!profile.spec.publish_enabled);
        assert_eq!(profile.spec.emit_from_close,fresh.emit_from_close);
        assert_eq!(profile_snapshot(profile),before);
        assert_eq!(profile.spec.from_open,f.spec.from_open);assert_eq!(profile.spec.params,f.spec.params);
        assert_eq!(history_snapshot(&f.runtime.symbols[&f.epoch].history),history);
        assert_eq!(f.runtime.symbols[&f.epoch].history.identity(),identity);
        assert_eq!(f.runtime.retained_profile_bytes(),bytes);
        let prepared=f.runtime.prepare(&f.epoch,&f.instrument,&[],f.frontier,true).unwrap().pop().unwrap();
        let old_definition=size_of::<CachedProfile>()
            +prepared.next.detector.as_ref().map_or(0,SharedProfile::retained_bytes)+prepared.next.spec.key().len();
        assert_eq!(prepared.retained_scalar_bytes(),old_definition+prepared.key.len());
        f.runtime.accept(&f.epoch,prepared,false);
        f.runtime.install(&f.epoch,std::slice::from_ref(&fresh)).unwrap();
        assert_eq!(f.runtime.profile_budget_checks,checks,"scalar-only accept preserves admission");
    }

    #[test]
    fn owned_key_accounting_matches_original_sum_across_epochs_and_materialization() {
        fn original_sum(runtime:&AdaptiveRuntime)->usize {
            runtime.symbols.values().flat_map(|s|s.profiles.values()).map(|p|
                size_of::<CachedProfile>()+p.detector.as_ref().map_or(0,SharedProfile::retained_bytes)
                    +p.spec.key().len()).sum()
        }
        let mut f=fixture();let mut sibling=f.spec.clone();
        sibling.fence.owner_id="another-owner-Ж".into();sibling.fence.generation_id=12345;
        let staged=AdaptiveRuntime::epoch_key(&f.instrument,Some(2));
        for epoch in [&f.epoch,&staged] {
            f.runtime.install(epoch,&[f.spec.clone(),sibling.clone()]).unwrap();
            assert_eq!(f.runtime.retained_profile_bytes(),original_sum(&f.runtime));
            for prepared in f.runtime.prepare(epoch,&f.instrument,&f.bars[..1],60,false).unwrap() {
                assert_eq!(prepared.retained_scalar_bytes(),size_of::<CachedProfile>()
                    +prepared.next.detector.as_ref().map_or(0,SharedProfile::retained_bytes)
                    +prepared.next.spec.key().len()+prepared.key.len());
                f.runtime.accept(epoch,prepared,false);
                assert_eq!(f.runtime.retained_profile_bytes(),original_sum(&f.runtime));
            }
        }
        f.runtime.remove_profile(&staged,&sibling.key());
        assert_eq!(f.runtime.retained_profile_bytes(),original_sum(&f.runtime));
        f.runtime.invalidate(&staged,None);
        assert_eq!(f.runtime.retained_profile_bytes(),original_sum(&f.runtime));
        f.runtime.clear();assert_eq!(f.runtime.retained_profile_bytes(),original_sum(&f.runtime));
    }

    #[test]
    fn detector_materialization_and_changed_limits_force_exact_recheck_including_retries() {
        let mut f=fixture();
        f.runtime.install(&f.epoch,std::slice::from_ref(&f.spec)).unwrap();
        let cold=f.runtime.retained_profile_bytes();f.runtime.limits.max_profile_bytes=cold;
        f.runtime.install(&f.epoch,std::slice::from_ref(&f.spec)).unwrap();
        let checks=f.runtime.profile_budget_checks;
        feed(&mut f.runtime,&f.epoch,&f.instrument,&f.bars[..1],60).unwrap();
        let materialized=f.runtime.retained_profile_bytes();assert!(materialized>cold);
        assert_eq!(f.runtime.profile_budget_validated_at_limit,None);
        for retry in 1..=2 {
            assert!(f.runtime.install(&f.epoch,std::slice::from_ref(&f.spec)).is_err());
            assert_eq!(f.runtime.profile_budget_checks,checks+retry);
            assert_eq!(f.runtime.profile_budget_validated_at_limit,None);
            assert_eq!(f.runtime.retained_profile_bytes(),materialized);
        }
        f.runtime.limits.max_profile_bytes=materialized;
        f.runtime.install(&f.epoch,std::slice::from_ref(&f.spec)).unwrap();
        let checks=f.runtime.profile_budget_checks;
        feed(&mut f.runtime,&f.epoch,&f.instrument,&f.bars[1..2],120).unwrap();
        f.runtime.install(&f.epoch,std::slice::from_ref(&f.spec)).unwrap();
        assert_eq!(f.runtime.profile_budget_checks,checks,"initialized detector updates do not grow");
        f.runtime.limits.max_profile_bytes=materialized-1;
        assert!(f.runtime.install(&f.epoch,std::slice::from_ref(&f.spec)).is_err());
        assert_eq!(f.runtime.profile_budget_checks,checks+1);
        assert_eq!(f.runtime.profile_budget_validated_at_limit,None);
    }

    #[test]
    fn failed_insertion_and_partial_install_error_cannot_certify_an_identical_retry() {
        let mut f=fixture();let mut sibling=f.spec.clone();sibling.fence.slot_id=2;
        f.runtime.install(&f.epoch,std::slice::from_ref(&f.spec)).unwrap();
        let bytes=f.runtime.retained_profile_bytes();f.runtime.limits.max_profile_bytes=bytes;
        f.runtime.install(&f.epoch,std::slice::from_ref(&f.spec)).unwrap();
        for _ in 0..2 {
            let checks=f.runtime.profile_budget_checks;
            assert!(f.runtime.install(&f.epoch,&[f.spec.clone(),sibling.clone()]).is_err());
            assert_eq!(f.runtime.profile_budget_checks,checks+1);
            assert_eq!(f.runtime.profile_budget_validated_at_limit,None);
            assert_eq!(f.runtime.symbols[&f.epoch].profiles.len(),2,"no silent eviction on rejection");
        }
        f.runtime.remove_profile(&f.epoch,&sibling.key());
        f.runtime.install(&f.epoch,std::slice::from_ref(&f.spec)).unwrap();
        let mut invalid=sibling.clone();invalid.fence.slot_id=3;invalid.cluster_gap_minutes=Some(0);
        let checks=f.runtime.profile_budget_checks;
        assert!(f.runtime.install(&f.epoch,&[sibling,invalid]).is_err());
        assert_eq!(f.runtime.profile_budget_checks,checks,"constructor failed before final admission");
        assert_eq!(f.runtime.profile_budget_validated_at_limit,None);
        assert!(f.runtime.install(&f.epoch,std::slice::from_ref(&f.spec)).is_err(),
            "earlier insertion survived the constructor error and must be counted");
        assert_eq!(f.runtime.profile_budget_checks,checks+1);
    }

    #[test]
    fn accept_of_missing_profile_invalidates_a_newer_admission_proof() {
        let mut f=fixture();let mut sibling=f.spec.clone();sibling.fence.slot_id=2;
        f.runtime.install(&f.epoch,&[f.spec.clone(),sibling.clone()]).unwrap();
        let prepared=f.runtime.prepare(&f.epoch,&f.instrument,&[],0,false).unwrap().into_iter()
            .find(|p|p.key==sibling.key()).unwrap();
        f.runtime.remove_profile(&f.epoch,&sibling.key());
        f.runtime.install(&f.epoch,std::slice::from_ref(&f.spec)).unwrap();
        let bytes=f.runtime.retained_profile_bytes();let checks=f.runtime.profile_budget_checks;
        f.runtime.accept(&f.epoch,prepared,false);
        assert!(f.runtime.retained_profile_bytes()>bytes);
        assert_eq!(f.runtime.profile_budget_validated_at_limit,None);
        f.runtime.install(&f.epoch,std::slice::from_ref(&f.spec)).unwrap();
        assert_eq!(f.runtime.profile_budget_checks,checks+1);
    }

    #[test]
    fn removals_reconcile_invalidation_and_clear_release_memory_and_owned_references() {
        let mut f=fixture();
        f.runtime.install(&f.epoch,std::slice::from_ref(&f.spec)).unwrap();
        let base_bytes=f.runtime.retained_profile_bytes();
        let wanted=BTreeMap::from([(f.epoch.clone(),BTreeSet::from([f.spec.key()]))]);
        let identity=f.runtime.symbols[&f.epoch].history.identity();
        for explicit_remove in [true,false,true,false] {
            let mut sibling=f.spec.clone();sibling.fence.slot_id=2;
            sibling.recommendation=Arc::new(serde_json::json!({"owned":"candidate"}));
            let weak=Arc::downgrade(&sibling.recommendation);let key=sibling.key();
            f.runtime.install(&f.epoch,std::slice::from_ref(&sibling)).unwrap();drop(sibling);
            assert!(weak.upgrade().is_some());assert!(f.runtime.retained_profile_bytes()>base_bytes);
            let checks=f.runtime.profile_budget_checks;
            f.runtime.remove_profile(&f.epoch,"absent");f.runtime.invalidate("absent",None);
            assert!(f.runtime.profile_budget_validated_at_limit.is_some());
            if explicit_remove {f.runtime.remove_profile(&f.epoch,&key);}else{f.runtime.reconcile(&wanted);}
            assert_eq!(f.runtime.profile_budget_validated_at_limit,None);assert!(weak.upgrade().is_none());
            assert_eq!(f.runtime.retained_profile_bytes(),base_bytes);
            assert!(f.runtime.profile_epoch(&f.instrument,&key).is_none());
            f.runtime.install(&f.epoch,std::slice::from_ref(&f.spec)).unwrap();
            assert_eq!(f.runtime.profile_budget_checks,checks+1);
        }
        f.runtime.invalidate(&f.epoch,None);
        assert_eq!(f.runtime.retained_profile_bytes(),0);assert_eq!(f.runtime.retained_history_bytes(),0);
        assert!(f.runtime.locations.is_empty());assert_eq!(f.runtime.profile_budget_validated_at_limit,None);
        f.runtime.install(&f.epoch,std::slice::from_ref(&f.spec)).unwrap();
        assert_ne!(f.runtime.symbols[&f.epoch].history.identity(),identity);
        f.runtime.remove_profile(&f.epoch,&f.spec.key());
        assert_eq!(f.runtime.retained_history_epochs(),0);assert_eq!(f.runtime.retained_profile_bytes(),0);
        f.runtime.install(&f.epoch,std::slice::from_ref(&f.spec)).unwrap();
        f.runtime.reconcile(&BTreeMap::new());
        assert_eq!(f.runtime.retained_history_bytes(),0);assert!(f.runtime.locations.is_empty());
        f.runtime.install(&f.epoch,std::slice::from_ref(&f.spec)).unwrap();
        f.runtime.clear();assert_eq!(f.runtime.profile_budget_validated_at_limit,None);
        assert_eq!(f.runtime.retained_history_bytes(),0);assert_eq!(f.runtime.retained_profile_bytes(),0);
        assert!(f.runtime.locations.is_empty());
    }

    #[test]
    fn certified_install_still_enforces_profile_counts_capacity_and_revision_reconstruction() {
        let mut f=fixture();f.runtime.install(&f.epoch,std::slice::from_ref(&f.spec)).unwrap();
        let mut revision=f.spec.clone();revision.fence.config_revision+=1;
        f.runtime.limits.max_profiles=1;
        assert!(f.runtime.install(&f.epoch,&[f.spec.clone(),revision.clone()]).is_err());
        f.runtime.limits.max_profiles=4;
        feed(&mut f.runtime,&f.epoch,&f.instrument,&f.bars[..1],60).unwrap();
        f.runtime.install(&f.epoch,std::slice::from_ref(&f.spec)).unwrap();
        let checks=f.runtime.profile_budget_checks;let bytes=f.runtime.retained_profile_bytes();
        assert!(f.runtime.needs_reconstruction(&f.epoch,std::slice::from_ref(&revision)));
        assert!(f.runtime.install(&f.epoch,&[revision]).is_err());
        let mut larger=f.spec.clone();if let AlgorithmParams::Spike(p)=&mut larger.params {p.baseline_long_days+=1;}
        assert!(f.runtime.install(&f.epoch,&[larger]).is_err());
        assert_eq!(f.runtime.retained_profile_bytes(),bytes);assert_eq!(f.runtime.profile_budget_checks,checks);
        f.runtime.limits.max_history_bytes=0;
        let other=AdaptiveRuntime::epoch_key(&f.instrument,Some(2));
        assert!(f.runtime.install(&other,std::slice::from_ref(&f.spec)).is_err());
        assert!(!f.runtime.has_epoch(&other));
        f.runtime.limits.max_history_bytes=8*1024*1024;f.runtime.limits.max_symbols=0;
        assert!(f.runtime.install(&other,std::slice::from_ref(&f.spec)).is_err());
        assert!(!f.runtime.has_epoch(&other));
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
        f.runtime.install(&staged,std::slice::from_ref(&candidate)).unwrap();
        let admission=f.runtime.profile_budget_validated_at_limit;
        assert!(!f.runtime.adopt_staging(&staged,&f.epoch,&candidate.key(),&expected).unwrap());
        assert_eq!(f.runtime.profile_budget_validated_at_limit,admission);
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

    #[test]
    fn staging_preserves_distinct_live_origins_and_promotion_preserves_causal_phase() {
        let mut f=fixture();
        if let AlgorithmParams::Spike(params)=&mut f.spec.params {params.volume_multiplier="0.5".into();params.cooldown_minutes=7;}
        let target=f.frontier+1200;
        f.bars=(0..target).step_by(60).map(|open_time_unix|DetectorBar {open_time_unix,close:"100".into(),quote_turnover:"1".into()}).collect();
        f.runtime.install(&f.epoch,&[f.spec.clone()]).unwrap();
        feed(&mut f.runtime,&f.epoch,&f.instrument,&f.bars,target).unwrap();
        let incumbent=f.runtime.checkpoint_for(&f.epoch,&f.spec.key()).unwrap();
        let mut shadow=f.spec.clone();shadow.pending=true;shadow.checkpoint=Some(incumbent.clone());
        let mut candidate=f.spec.clone();candidate.fence.generation_id=2;candidate.from_open=600;
        candidate.recommendation=Arc::new(serde_json::json!({"timeShiftMinutes":3,"generationId":2}));
        candidate.warmup_through=target;candidate.emit_from_close=target+60;candidate.pending=true;
        let staging=AdaptiveRuntime::epoch_key(&f.instrument,Some(2));
        f.runtime.install(&staging,&[shadow,candidate.clone()]).unwrap();
        assert!(feed(&mut f.runtime,&staging,&f.instrument,&f.bars,target).unwrap().is_empty());
        let candidate_before=f.runtime.checkpoint_for(&staging,&candidate.key()).unwrap();
        assert_ne!(candidate_before.last_raw_open_unix,incumbent.last_raw_open_unix,"distinct origins must not borrow another profile's cooldown phase");
        assert_eq!(f.runtime.checkpoint_for(&staging,&f.spec.key()),Some(incumbent.clone()));
        f.runtime.acknowledge_source(&staging,TailReadToken {epoch:"fixture".into(),revision:"1".into()}).unwrap();
        f.runtime.install(&staging,std::slice::from_ref(&candidate)).unwrap();
        assert!(f.runtime.profile_budget_validated_at_limit.is_some());
        assert!(f.runtime.adopt_staging(&staging,&f.epoch,&candidate.key(),&BTreeMap::from([(f.spec.key(),incumbent)])).unwrap());
        assert_eq!(f.runtime.profile_budget_validated_at_limit,None);
        let checks=f.runtime.profile_budget_checks;
        f.runtime.install(&f.epoch,std::slice::from_ref(&candidate)).unwrap();
        assert_eq!(f.runtime.profile_budget_checks,checks+1);
        f.runtime.activate(&f.epoch,&candidate.key(),target).unwrap();
        assert_eq!(f.runtime.checkpoint_for(&f.epoch,&candidate.key()),Some(candidate_before),"promotion changes ownership, not raw state");
        let mut reference=PriceVolumeSpikeDetector::with_params(candidate.params.clone(),None).unwrap();
        for bar in f.bars.iter().filter(|bar|bar.open_time_unix>=candidate.from_open) {reference.push(bar).unwrap();}
        for open in (target..target+1200).step_by(60) {
            let bar=DetectorBar {open_time_unix:open,close:"100".into(),quote_turnover:"1".into()};
            let expected=reference.push(&bar).unwrap().setup;
            let effects=feed(&mut f.runtime,&f.epoch,&f.instrument,&[bar],open+60).unwrap();
            assert_eq!(effects.iter().filter(|effect|effect.recommendation["generationId"]==2)
                .map(|effect|effect.raw.clone()).collect::<Vec<_>>(),expected.into_iter().collect::<Vec<_>>());
        }
        f.runtime.clear();assert_eq!(f.runtime.retained_history_bytes(),0);assert_eq!(f.runtime.retained_profile_bytes(),0);
    }

    #[test]
    fn adopting_shadow_does_not_unmute_a_durably_waiting_sibling() {
        let mut f=fixture();let mut sibling=f.spec.clone();sibling.fence.slot_id=3;sibling.waiting=true;
        f.runtime.install(&f.epoch,&[f.spec.clone(),sibling.clone()]).unwrap();
        feed(&mut f.runtime,&f.epoch,&f.instrument,&f.bars,f.frontier).unwrap();
        let current=f.runtime.checkpoint_for(&f.epoch,&f.spec.key()).unwrap();
        let waiting=f.runtime.checkpoint_for(&f.epoch,&sibling.key()).unwrap();
        let mut shadow=f.spec.clone();shadow.pending=true;shadow.checkpoint=Some(current.clone());
        let mut waiting_shadow=sibling.clone();waiting_shadow.pending=true;
        let mut candidate=f.spec.clone();candidate.fence.generation_id=2;candidate.pending=true;
        let staging=AdaptiveRuntime::epoch_key(&f.instrument,Some(2));
        f.runtime.install(&staging,&[shadow,waiting_shadow,candidate.clone()]).unwrap();
        feed(&mut f.runtime,&staging,&f.instrument,&f.bars,f.frontier).unwrap();
        f.runtime.acknowledge_source(&staging,TailReadToken {epoch:"fixture".into(),revision:"1".into()}).unwrap();
        assert!(f.runtime.adopt_staging(&staging,&f.epoch,&candidate.key(),&BTreeMap::from([(f.spec.key(),current),(sibling.key(),waiting)])).unwrap());
        assert_eq!(f.runtime.cached_checkpoint(&f.epoch,&sibling.key()),Some(None),"staging cannot invent durable waiting state");
        let bar=DetectorBar {open_time_unix:f.frontier,close:"100".into(),quote_turnover:"100".into()};
        let pages=f.runtime.prepare(&f.epoch,&f.instrument,&[bar],f.frontier+60,true).unwrap();
        let waiting=pages.iter().find(|page|page.key==sibling.key()).unwrap();
        assert!(waiting.setups.is_empty());assert!(!waiting.should_commit());
        assert_eq!(pages.iter().filter(|page|page.should_commit()).count(),1);
    }
}
