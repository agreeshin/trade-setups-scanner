use super::*;
use std::{collections::BTreeSet, sync::Arc};
use setups_domain::AdaptiveRecommendationSnapshot;
use setups_optimizer::{ContinuousInstrumentBootstrap, StudyRequest};

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
    pub bootstrap: AdaptiveLiveBootstrap,
    pub recommendation: Arc<Value>,
    pub cluster_gap_minutes: Option<u32>,
}
impl LiveProfile {
    pub fn parse(slot:&ActiveAdaptiveSlot)->Result<Self,WorkerError> {
        let params:AlgorithmParams=serde_json::from_value(slot.params.clone())?;
        let snapshot:AdaptiveRecommendationSnapshot=serde_json::from_value(slot.result.get("recommendation").cloned().ok_or_else(||invalid("missing result.recommendation"))?)?;
        let bootstrap:AdaptiveLiveBootstrap=serde_json::from_value(slot.result.get("bootstrap").cloned().ok_or_else(||invalid("missing result.bootstrap"))?)?;
        let spec:StudyRequest=serde_json::from_value(slot.spec.clone())?;
        let expected:BTreeSet<_>=spec.instruments.iter().cloned().collect();
        let actual:BTreeSet<_>=bootstrap.instruments.iter().map(|raw|raw.instrument.clone()).collect();
        if params.algorithm_id()!=slot.algorithm_id || params.algorithm_version()!=spec.algorithm_version
            || snapshot.provenance.algorithm_id!=slot.algorithm_id || snapshot.provenance.algorithm_version!=spec.algorithm_version
            || snapshot.provenance.from_unix!=spec.from_unix || snapshot.provenance.to_unix!=spec.to_unix
            || snapshot.provenance.observation_cutoff!=bootstrap.observation_cutoff
            || snapshot.provenance.snapshot_hash!=bootstrap.provenance.snapshot_hash
            || snapshot.provenance.optimizer_version!=bootstrap.provenance.optimizer_version
            || snapshot.provenance.seed!=bootstrap.provenance.seed || snapshot.provenance.captured_at!=bootstrap.provenance.captured_at
            || snapshot.provenance.seed!=spec.seed
            || serde_json::to_value(snapshot.direction)?.as_str()!=Some(slot.direction.as_str())
            || slot.analytics["clusterEnabled"].as_bool()!=Some(snapshot.cluster_enabled)
            || slot.analytics["timeShiftMinutes"].as_i64()!=Some(snapshot.time_shift_minutes)
            || slot.analytics["priceChangePct"].as_f64()!=snapshot.price_change_pct
            || (snapshot.cluster_enabled && slot.analytics["clusterGapMinutes"].as_i64()!=snapshot.cluster_gap_minutes)
            || slot.selected_cell["tpPct"].as_f64()!=Some(snapshot.tp_pct) || slot.selected_cell["slPct"].as_f64()!=Some(snapshot.sl_pct)
            || slot.selected_cell["rawR"].as_f64()!=Some(snapshot.raw_r)
            || bootstrap.export_from_unix!=spec.from_unix || bootstrap.export_to_unix!=spec.to_unix
            || snapshot.mean_r!=slot.mean_expectancy
            || ![snapshot.tp_pct,snapshot.sl_pct,snapshot.raw_r,snapshot.mean_r,snapshot.robust_score].iter().all(|number|number.is_finite())
            || !(0.0<snapshot.tp_pct && snapshot.tp_pct<=100.0) || !(0.0<snapshot.sl_pct && snapshot.sl_pct<=100.0)
            || !(0..=43200).contains(&snapshot.time_shift_minutes)
            || snapshot.price_change_pct.is_some_and(|pct|!pct.is_finite() || !(-100.0..=1000.0).contains(&pct))
            || slot.external_name.trim().is_empty() || slot.external_name.chars().count()>120
            || slot.external_name.len()>480 || slot.external_name.chars().any(char::is_control)
            || expected!=actual || actual.len()!=bootstrap.instruments.len() || actual.len()>1000
            || bootstrap.next_open_unix.rem_euclid(60)!=0 || bootstrap.next_open_unix<0
            || bootstrap.next_open_unix>bootstrap.observation_cutoff || bootstrap.detector_warmup_from_unix.rem_euclid(60)!=0
            || bootstrap.emit_from_close_unix!=bootstrap.observation_cutoff+60
            || bootstrap.instruments.iter().any(|raw|raw.last_raw_cluster_time.is_some_and(|time|time<60 || time>bootstrap.next_open_unix || time%60!=0)
                || raw.last_raw_open_unix.map(|time|time+60)!=raw.last_raw_cluster_time)
        {return Err(invalid("accepted profile/bootstrap identity or values mismatch"));}
        if let Some(prefix)=&bootstrap.frozen_prefix {
            let scope:BTreeSet<_>=prefix.instruments.iter().map(|p|p.instrument.clone()).collect();
            if prefix.version!=1 || prefix.from_open_unix!=bootstrap.detector_warmup_from_unix
                || prefix.to_open_unix!=bootstrap.next_open_unix || prefix.from_open_unix>prefix.to_open_unix
                || scope!=actual || scope.len()!=prefix.instruments.len()
                || prefix.instruments.iter().any(|p|p.sha256.len()!=64 || !p.sha256.bytes().all(|b|b.is_ascii_hexdigit())
                    || p.bar_count>((prefix.to_open_unix-prefix.from_open_unix)/60) as u64)
            {return Err(invalid("invalid frozen detector prefix manifest"));}
        }
        let cluster_gap_minutes=if snapshot.cluster_enabled {Some(u32::try_from(snapshot.cluster_gap_minutes.filter(|gap|(1..=10080).contains(gap)).ok_or_else(||invalid("missing causal cluster gap"))?).map_err(|_|invalid("invalid causal gap"))?)}else{None};
        let recommendation=Arc::new(serde_json::to_value(RecommendationEnvelope {snapshot,study_id:slot.study_id,generation_id:slot.generation_id,
            slot_id:slot.slot_id,owner_id:slot.owner_id.clone(),external_name:slot.external_name.clone(),params:params.clone()})?);
        Ok(Self {fence:AdaptiveFence {owner_id:slot.owner_id.clone(),algorithm_id:slot.algorithm_id.clone(),slot_id:slot.slot_id,
            generation_id:slot.generation_id,config_revision:slot.config_revision},params,bootstrap,recommendation,cluster_gap_minutes})
    }
    pub fn fence_key(&self)->String {
        serde_json::json!([self.fence.owner_id,self.fence.algorithm_id,self.fence.slot_id,self.fence.generation_id,self.fence.config_revision]).to_string()
    }
    pub fn spec(&self,seed:&ContinuousInstrumentBootstrap,checkpoint:Option<AdaptiveCheckpoint>,pending:bool)->Result<crate::adaptive::state::ProfileSpec,WorkerError> {
        let prefix=self.bootstrap.frozen_prefix.as_ref().ok_or_else(||invalid("generation has no frozen prefix manifest; cannot share unchecked history"))?
            .instruments.iter().find(|p|p.instrument==seed.instrument).ok_or_else(||invalid("missing instrument prefix fingerprint"))?.clone();
        Ok(crate::adaptive::state::ProfileSpec {fence:self.fence.clone(),params:self.params.clone(),recommendation:self.recommendation.clone(),
            cluster_gap_minutes:self.cluster_gap_minutes,from_open:self.bootstrap.detector_warmup_from_unix,
            bootstrap_next:self.bootstrap.next_open_unix,bootstrap_raw_open:seed.last_raw_open_unix,
            emit_from_close:self.bootstrap.emit_from_close_unix,checkpoint,prefix,pending,publish_enabled:true})
    }
}
