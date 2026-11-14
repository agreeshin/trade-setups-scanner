use super::*;
use std::{collections::BTreeSet, sync::Arc};
use setups_domain::AdaptiveRecommendationSnapshot;
use setups_optimizer::{StudyInstrument, StudyRequest};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct RecommendationEnvelope {
    #[serde(flatten)]
    pub snapshot: AdaptiveRecommendationSnapshot,
    pub study_id: i64,
    pub generation_id: i64,
    pub slot_id: i64,
    pub owner_id: String,
    pub external_name: String,
    pub params: AlgorithmParams,
}

#[derive(Clone)]
pub(super) struct LiveProfile {
    pub fence: AdaptiveFence,
    pub params: AlgorithmParams,
    pub instruments: Vec<StudyInstrument>,
    pub initialization: Option<live::LiveInitialization>,
    pub readiness: Vec<live::LiveInstrumentReadiness>,
    pub recommendation: Arc<Value>,
    pub cluster_gap_minutes: Option<u32>,
}
impl LiveProfile {
    pub fn parse(slot:&ActiveAdaptiveSlot)->Result<Self,WorkerError> {
        let params:AlgorithmParams=serde_json::from_value(slot.params.clone())?;
        let snapshot:AdaptiveRecommendationSnapshot=serde_json::from_value(slot.result.get("recommendation").cloned().ok_or_else(||invalid("missing result.recommendation"))?)?;
        let spec:StudyRequest=serde_json::from_value(slot.spec.clone())?;
        let expected:BTreeSet<_>=spec.instruments.iter().cloned().collect();
        if params.algorithm_id()!=slot.algorithm_id || params.algorithm_version()!=spec.algorithm_version
            || snapshot.provenance.algorithm_id!=slot.algorithm_id || snapshot.provenance.algorithm_version!=spec.algorithm_version
            || snapshot.provenance.from_unix!=spec.from_unix || snapshot.provenance.to_unix!=spec.to_unix
            || snapshot.provenance.seed!=spec.seed
            || serde_json::to_value(snapshot.direction)?.as_str()!=Some(slot.direction.as_str())
            || slot.analytics["clusterEnabled"].as_bool()!=Some(snapshot.cluster_enabled)
            || slot.analytics["timeShiftMinutes"].as_i64()!=Some(snapshot.time_shift_minutes)
            || slot.analytics["priceChangePct"].as_f64()!=snapshot.price_change_pct
            || (snapshot.cluster_enabled && slot.analytics["clusterGapMinutes"].as_i64()!=snapshot.cluster_gap_minutes)
            || slot.selected_cell["tpPct"].as_f64()!=Some(snapshot.tp_pct) || slot.selected_cell["slPct"].as_f64()!=Some(snapshot.sl_pct)
            || slot.selected_cell["rawR"].as_f64()!=Some(snapshot.raw_r)
            || snapshot.mean_r!=slot.mean_expectancy
            || ![snapshot.tp_pct,snapshot.sl_pct,snapshot.raw_r,snapshot.mean_r,snapshot.robust_score].iter().all(|number|number.is_finite())
            || !(0.0<snapshot.tp_pct && snapshot.tp_pct<=100.0) || !(0.0<snapshot.sl_pct && snapshot.sl_pct<=100.0)
            || !(0..=43200).contains(&snapshot.time_shift_minutes)
            || snapshot.price_change_pct.is_some_and(|pct|!pct.is_finite() || !(-100.0..=1000.0).contains(&pct))
            || slot.external_name.trim().is_empty() || slot.external_name.chars().count()>120
            || slot.external_name.len()>480 || slot.external_name.chars().any(char::is_control)
            || expected.is_empty() || expected.len()!=spec.instruments.len() || expected.len()>1000
        {return Err(invalid("accepted recipe identity or values mismatch"));}
        if let Some(init)=&slot.live_initialization
            && (init.version!=1 || init.detector_from_open_unix<0 || init.detector_from_open_unix%60!=0
                || init.anchor_close_unix%60!=0 || init.anchor_close_unix<init.detector_from_open_unix
                || init.emit_from_close_unix!=init.anchor_close_unix.checked_add(60).ok_or_else(||invalid("live initialization overflow"))?) {
            return Err(invalid("invalid live-owned initialization"));
        }
        let cluster_gap_minutes=if snapshot.cluster_enabled {Some(u32::try_from(snapshot.cluster_gap_minutes.filter(|gap|(1..=10080).contains(gap)).ok_or_else(||invalid("missing causal cluster gap"))?).map_err(|_|invalid("invalid causal gap"))?)}else{None};
        let recommendation=Arc::new(serde_json::to_value(RecommendationEnvelope {snapshot,study_id:slot.study_id,generation_id:slot.generation_id,
            slot_id:slot.slot_id,owner_id:slot.owner_id.clone(),external_name:slot.external_name.clone(),params:params.clone()})?);
        Ok(Self {fence:AdaptiveFence {owner_id:slot.owner_id.clone(),algorithm_id:slot.algorithm_id.clone(),slot_id:slot.slot_id,
            generation_id:slot.generation_id,config_revision:slot.config_revision},params,instruments:spec.instruments,
            initialization:slot.live_initialization.clone(),readiness:slot.live_readiness.clone(),recommendation,cluster_gap_minutes})
    }
    pub fn fence_key(&self)->String {
        serde_json::json!([self.fence.owner_id,self.fence.algorithm_id,self.fence.slot_id,self.fence.generation_id,self.fence.config_revision]).to_string()
    }
    pub fn spec(&self,instrument:&StudyInstrument,checkpoint:Option<AdaptiveCheckpoint>,pending:bool)->Result<crate::adaptive::state::ProfileSpec,WorkerError> {
        let init=self.initialization.as_ref().ok_or_else(||invalid("generation requires durable live initialization"))?;
        let key=format!("{}/{}/{}",instrument.exchange,instrument.category,instrument.symbol);
        let disposition=self.readiness.iter().find(|entry|entry.instrument_key==key);
        if !pending && disposition.is_none() {return Err(invalid("active instrument has no explicit live readiness disposition"));}
        if let Some(entry)=disposition
            && (!matches!(entry.status.as_str(),"ready"|"waiting") || entry.publication_floor_close_unix<init.anchor_close_unix
                || entry.publication_floor_close_unix%60!=0) {return Err(invalid("invalid live instrument disposition"));}
        let floor=disposition.map_or(init.emit_from_close_unix,|entry|entry.publication_floor_close_unix.saturating_add(60).max(init.emit_from_close_unix));
        Ok(crate::adaptive::state::ProfileSpec {fence:self.fence.clone(),params:self.params.clone(),recommendation:self.recommendation.clone(),
            cluster_gap_minutes:self.cluster_gap_minutes,from_open:init.detector_from_open_unix,
            warmup_through:init.anchor_close_unix,emit_from_close:floor,checkpoint,
            pending,waiting:disposition.is_some_and(|entry|entry.status=="waiting"),publish_enabled:true})
    }
}
