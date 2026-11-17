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

#[cfg(test)]
mod tests {
    use super::*;
    use setups_domain::forecast::ForecastRecipe;
    use setups_domain::trend::TrendBendDays;
    use setups_optimizer::{ContinuousContext, ContinuousEvaluation, Provenance, StudyCandidate, recommend_continuous};

    #[test]
    fn producer_json_accepts_effective_recipe_bend_without_weakening_metric_identity() {
        for (recipe, legacy_bend, effective_bend) in [
            (Some(ForecastRecipe::engineering_baseline()), TrendBendDays::Seven, TrendBendDays::Linear),
            (Some(ForecastRecipe::Trend { version: 1, bend_days: TrendBendDays::Seven }), TrendBendDays::Linear, TrendBendDays::Seven),
            (None, TrendBendDays::Seven, TrendBendDays::Seven),
        ] {
            let (envelope, _, _, _, mut slot) = super::super::enrichment::tests::fixture();
            let p = &envelope.snapshot.provenance;
            let spec: StudyRequest = serde_json::from_value(json!({
                "algorithmId":p.algorithm_id,"algorithmVersion":p.algorithm_version,"algorithmParams":slot.params,
                "analyticsParams":{"clusterEnabled":envelope.snapshot.cluster_enabled,"clusterGapMinutes":envelope.snapshot.cluster_gap_minutes.unwrap_or(480),"timeShiftMinutes":envelope.snapshot.time_shift_minutes,"priceChangePct":envelope.snapshot.price_change_pct},
                "instruments":[{"exchange":"bybit","category":"linear","symbol":"TESTUSDT"}],
                "fromUnix":p.from_unix,"toUnix":p.to_unix,"direction":"short","seed":p.seed,
                "minSetupsPerDay":0,"tpLevels":[envelope.snapshot.tp_pct],"slLevels":[envelope.snapshot.sl_pct],
                "trendMode":"forecast","trendBendDays":legacy_bend,"forecast":recipe,
            })).unwrap();
            let candidate = StudyCandidate { params: spec.algorithm_params.clone(), analytics: spec.analytics_params.clone() };
            let result: ContinuousEvaluation = serde_json::from_value(json!({"evaluation":{
                "summary":{"score":0.5,"setupCount":45,"selectedSetupCount":45,"evaluatedMinutes":100,"warmupMinutes":0},
                "matrix":[{"tpPct":envelope.snapshot.tp_pct,"slPct":envelope.snapshot.sl_pct,
                    "riskExpectancy":0.5,"meanRiskExpectancy":envelope.snapshot.raw_r,"forecastRiskExpectancy":0.5,
                    "avgMovePct":null,"closedCount":45,"tpCount":32,"slCount":13,"unclosedCount":0,"noDataCount":0,"priceChangeFilteredCount":0}]
            },"diagnostics":[]})).unwrap();
            let producer = recommend_continuous(&spec, &candidate, &result, &Provenance {
                optimizer_version:p.optimizer_version.clone(), seed:p.seed, snapshot_hash:p.snapshot_hash.clone(), captured_at:p.captured_at.clone(),
            }, ContinuousContext { observation_cutoff: p.observation_cutoff }, 0.0).unwrap().unwrap();
            assert_eq!(producer.search_forecast.unwrap().bend_days, effective_bend);
            slot.spec = serde_json::to_value(spec).unwrap();
            slot.result["recommendation"] = serde_json::to_value(&producer).unwrap();
            slot.selected_cell = json!({"tpPct":producer.tp_pct,"slPct":producer.sl_pct,"rawR":producer.raw_r,"forecastR":0.5,"forecastMatrixR":0.5});
            slot.mean_expectancy = producer.mean_r;
            LiveProfile::parse(&slot).unwrap();
            slot.selected_cell["forecastR"] = json!(0.6);
            assert!(LiveProfile::parse(&slot).is_err(), "real metric tampering still fails closed");
        }
    }
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
        let forecast=spec.forecast;
        let effective_bend=match forecast {
            None=>spec.trend_bend_days,
            Some(setups_domain::forecast::ForecastRecipe::DecayedProbability {..})=>setups_domain::trend::TrendBendDays::Linear,
            Some(setups_domain::forecast::ForecastRecipe::Trend {bend_days,..})=>bend_days,
        };
        let expected:BTreeSet<_>=spec.instruments.iter().cloned().collect();
        if params.algorithm_id()!=slot.algorithm_id || params.algorithm_version()!=spec.algorithm_version
            || snapshot.provenance.algorithm_id!=slot.algorithm_id || snapshot.provenance.algorithm_version!=spec.algorithm_version
            || snapshot.provenance.from_unix!=spec.from_unix || snapshot.provenance.to_unix!=spec.to_unix
            || snapshot.provenance.seed!=spec.seed
            || snapshot.forecast!=forecast
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
        match (spec.trend_mode, snapshot.search_forecast) {
            (setups_optimizer::TrendMode::Mean, None) => {},
            (setups_optimizer::TrendMode::Forecast, Some(forecast))
                if forecast.bend_days==effective_bend && forecast.cell_r.is_finite()
                    && forecast.blend.is_none() && forecast.matrix_r.is_finite() && forecast.matrix_r>spec.min_forecast_expectancy
                    && slot.selected_cell["forecastR"].as_f64()==Some(forecast.cell_r)
                    && slot.selected_cell["forecastMatrixR"].as_f64()==Some(forecast.matrix_r) => {},
            (setups_optimizer::TrendMode::Blend, Some(forecast))
                if forecast.bend_days==effective_bend && forecast.cell_r.is_finite() && forecast.matrix_r.is_finite()
                    && slot.selected_cell["forecastR"].as_f64()==Some(forecast.cell_r)
                    && slot.selected_cell["forecastMatrixR"].as_f64()==Some(forecast.matrix_r)
                    && forecast.blend.is_some_and(|blend| blend.lambda==spec.trend_blend_lambda
                        && blend.cell_r.is_finite() && blend.matrix_r.is_finite() && blend.matrix_r>spec.min_forecast_expectancy
                        && slot.selected_cell["blendR"].as_f64()==Some(blend.cell_r)
                        && slot.selected_cell["blendMatrixR"].as_f64()==Some(blend.matrix_r)) => {},
            _ => return Err(invalid("accepted search metric identity mismatch")),
        }
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
