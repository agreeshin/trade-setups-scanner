//! Explicit operator command only. No web handler/GET/opening invokes it.
use serde_json::{Value,json};
use sqlx::PgPool;
use super::legacy::{LegacyCalculation,LegacyFidelity,LegacyLimits};
use super::legacy_runtime::{self,TrustedAuditFacts};
use setups_db::study_display::{self as db,OriginalMinute,OriginalPublication,VerifiedOriginalIdentity};

pub const USAGE:&str="materialize-study-original --study-id <ID> --generation-id <ID> --artifact-id <ID> --expected-source-identity <MD5> --expected-artifact-key <SHA256> --writer-audit-hash <SHA256> --verify-first-canonical-add-only --verify-canonical-ohlc-preserved --verify-original-close-kernel [--publish]\nDefault: bounded read-only calculation. --publish writes only the verified original-outcome sidecar; no research, refresh, profile update, or server startup.";

#[derive(Debug,Clone,PartialEq,Eq)]
pub struct Options {
    pub study_id:i64, pub generation_id:i64, pub artifact_id:i64, pub expected_source_identity:String, pub expected_artifact_key:String,
    pub writer_audit_hash:String, pub publish:bool,
}
pub fn parse(args:&[String])->Result<Options,&'static str> {
    let mut fields=std::collections::BTreeMap::<&str,&str>::new();let mut flags=std::collections::BTreeSet::new();let mut i=0;
    while i<args.len() {
        let flag=args[i].as_str();
        match flag {
            "--study-id"|"--generation-id"|"--artifact-id"|"--expected-source-identity"|"--expected-artifact-key"|"--writer-audit-hash"=>{
                i+=1;let value=args.get(i).ok_or("missing materialization argument value")?.as_str();
                if value.starts_with("--") || fields.insert(flag,value).is_some(){return Err("invalid or duplicate materialization argument");}
            },
            "--publish"|"--verify-first-canonical-add-only"|"--verify-canonical-ohlc-preserved"|"--verify-original-close-kernel"=>{
                if !flags.insert(flag){return Err("duplicate materialization flag");}
            },
            _=>return Err("unknown materialization argument"),
        }
        i+=1;
    }
    for required in ["--verify-first-canonical-add-only","--verify-canonical-ohlc-preserved","--verify-original-close-kernel"] {
        if !flags.contains(required){return Err("explicit first-canonical/OHLC/original-kernel verification is required");}
    }
    let required=|key|fields.get(key).copied().ok_or("missing required materialization argument");
    let id=|key|required(key)?.parse::<i64>().ok().filter(|id|*id>0).ok_or("materialization IDs must be positive");
    let hash=|key,len|{let v=required(key)?;if v.len()!=len || !v.bytes().all(|b|b.is_ascii_digit()||(b'a'..=b'f').contains(&b)){Err("invalid materialization identity/hash")}else{Ok(v.to_string())}};
    Ok(Options{study_id:id("--study-id")?,generation_id:id("--generation-id")?,artifact_id:id("--artifact-id")?,expected_source_identity:hash("--expected-source-identity",32)?,expected_artifact_key:hash("--expected-artifact-key",64)?,writer_audit_hash:hash("--writer-audit-hash",64)?,publish:flags.contains("--publish")})
}

/// A trusted runtime prepare() result, not a deserialized user certificate.
/// Publication independently rechecks source fingerprints and all ledger counts.
pub async fn publish(pool:&PgPool,options:&Options,calculation:&LegacyCalculation,facts:&TrustedAuditFacts)->Result<OriginalPublication,String> {
    if !options.publish || options.generation_id!=facts.generation_id || options.artifact_id!=facts.original_artifact_id
        || facts.writer_audit_hash!=options.writer_audit_hash || !matches!(calculation.fidelity,LegacyFidelity::VerifiedOriginalReconstruction)
        || !calculation.conservation_matched || !facts.first_canonical_add_only_verified || !facts.canonical_ohlc_preserved_verified
        || !facts.historical_close_kernel_compatible_verified || calculation.selected_minutes.version!=setups_optimizer::CONTINUOUS_SELECTED_MINUTES_VERSION
        || calculation.selected_minutes.evaluator!=setups_optimizer::CONTINUOUS_SELECTED_MINUTES_EVALUATOR {return Err("legacy_verified_publication_not_admitted".into());}
    let identity=VerifiedOriginalIdentity{owner_id:"admin".into(),study_id:options.study_id,artifact_id:options.artifact_id,
        artifact_key:options.expected_artifact_key.clone(),study_identity:options.expected_source_identity.clone(),original_snapshot_hash:calculation.original_snapshot_hash.clone(),
        source_hash:calculation.source_hash.clone(),input_hash:calculation.input_hash.clone(),proof_hash:calculation.proof_hash.clone().ok_or("legacy_verified_proof_missing")?,
        tp_pct:calculation.selected_minutes.tp_pct,sl_pct:calculation.selected_minutes.sl_pct};
    let evidence=json!({"fidelity":"verified_original_reconstruction","scope":"closed_outcomes_only","proofBasis":"add_only_closed_set_monotonicity",
        "sourceHash":identity.source_hash,"inputHash":identity.input_hash,"proofHash":identity.proof_hash,"originalSnapshotHash":identity.original_snapshot_hash,
        "originalArtifactKey":identity.artifact_key,"studyIdentity":identity.study_identity,"conservationMatched":true,
        "evaluator":calculation.selected_minutes.evaluator,"writerAuditHash":facts.writer_audit_hash,
        "firstCanonicalAddOnlyVerified":facts.first_canonical_add_only_verified,"canonicalOhlcPreservedVerified":facts.canonical_ohlc_preserved_verified,
        "historicalCloseKernelCompatibleVerified":facts.historical_close_kernel_compatible_verified,
        "instrumentCount":calculation.inputs.len(),"utcDayCount":calculation.daily.len(),"closedCount":calculation.selected_minutes.points.iter().map(|p|p.closed_count).sum::<u64>(),
        "tpCount":calculation.selected_minutes.points.iter().map(|p|p.tp_count).sum::<u64>(),"slCount":calculation.selected_minutes.points.iter().map(|p|p.sl_count).sum::<u64>()});
    let points:Vec<_>=calculation.selected_minutes.points.iter().map(|p|OriginalMinute{signal_close_unix:p.signal_close_unix,sum_r:p.sum_r,
        closed_count:p.closed_count,tp_count:p.tp_count,sl_count:p.sl_count}).collect();
    db::publish_verified_original_reconstruction(pool,&identity,&points,&evidence).await.map_err(|e|e.to_string())
}

pub async fn run(pool:&PgPool,client:&setups_mdclient::MarketDataClient,options:&Options)->Result<Value,String> {
    // Reject stale/foreign intent BEFORE any market IO; prepare() then loads and
    // fences the retained scientific source itself. No duplicate loader here.
    let matches:bool=sqlx::query_scalar("select exists(select 1 from optimization_studies s join study_expectancy_artifacts a on a.source_study_id=s.id and a.owner_id=s.owner_id where s.owner_id='admin' and s.id=$1 and s.status='completed' and s.origin='continuous' and s.continuous_generation_id=$2 and a.id=$3 and a.source_generation_id=$2 and a.artifact_key=$4 and a.revision_key='original' and md5(s.spec::text||coalesce(s.result::text,'null'))=$5)")
        .bind(options.study_id).bind(options.generation_id).bind(options.artifact_id).bind(&options.expected_artifact_key).bind(&options.expected_source_identity).fetch_one(pool).await.map_err(|_|"legacy_preflight_storage_unavailable")?;
    if !matches {return Err("legacy_expected_source_changed_or_missing".into());}
    let facts=TrustedAuditFacts{generation_id:options.generation_id,original_artifact_id:options.artifact_id,writer_audit_hash:options.writer_audit_hash.clone(),
        first_canonical_add_only_verified:true,canonical_ohlc_preserved_verified:true,historical_close_kernel_compatible_verified:true};
    let limits=LegacyLimits::default();let cancel=setups_optimizer::CancelToken::new();
    let calculation=tokio::select! {
        result=legacy_runtime::prepare(pool,"admin",options.study_id,client,&limits,&facts,&cancel)=>result.map_err(|e|e.to_string())?,
        _=tokio::signal::ctrl_c()=>{cancel.cancel();return Err("legacy_interrupted".into());},
    };
    let publication=if options.publish {Some(publish(pool,options,&calculation,&facts).await?)}else{None};
    Ok(summary(&calculation,publication.as_ref()))
}

pub fn summary(calculation:&LegacyCalculation,publication:Option<&OriginalPublication>)->Value {
    json!({"mode":if publication.is_some(){"publish"}else{"dry_run"},"fidelity":calculation.fidelity,"conservationMatched":calculation.conservation_matched,
        "scope":"closed_outcomes_only","acquisitionHashMatch":"not_claimed","sourceHash":calculation.source_hash,"scopedInputHash":calculation.input_hash,
        "originalSnapshotHash":calculation.original_snapshot_hash,"proofHash":calculation.proof_hash,"instrumentCount":calculation.inputs.len(),
        "candleCount":calculation.inputs.iter().map(|p|p.candle_count).sum::<u64>(),"missingMinutes":calculation.inputs.iter().map(|p|p.missing_minutes).sum::<u64>(),
        "minuteCount":calculation.selected_minutes.points.len(),"utcDayCount":calculation.daily.len(),
        "closedCount":calculation.selected_minutes.points.iter().map(|p|p.closed_count).sum::<u64>(),
        "tpCount":calculation.selected_minutes.points.iter().map(|p|p.tp_count).sum::<u64>(),"slCount":calculation.selected_minutes.points.iter().map(|p|p.sl_count).sum::<u64>(),"publication":publication})
}

#[cfg(test)] mod tests {
    use super::*;
    fn args()->Vec<String>{vec!["--study-id".into(),"36".into(),"--generation-id".into(),"1".into(),"--artifact-id".into(),"1".into(),"--expected-source-identity".into(),"a".repeat(32),"--expected-artifact-key".into(),"b".repeat(64),
        "--writer-audit-hash".into(),"c".repeat(64),"--verify-first-canonical-add-only".into(),"--verify-canonical-ohlc-preserved".into(),"--verify-original-close-kernel".into()]}
    #[test]fn defaults_to_dry_run_and_publication_requires_explicit_flag(){let mut a=args();assert!(!parse(&a).unwrap().publish);a.push("--publish".into());assert!(parse(&a).unwrap().publish);}
    #[test]fn rejects_missing_verification_changed_identity_and_duplicate_flags(){let mut a=args();a.pop();assert!(parse(&a).is_err());let mut a=args();a[3]="unsafe".into();assert!(parse(&a).is_err());let mut a=args();a.push("--verify-original-close-kernel".into());assert!(parse(&a).is_err());assert!(parse(&[]).is_err());}
    #[tokio::test]async fn default_dry_run_refuses_publication_before_any_db_connection(){
        let options=parse(&args()).unwrap();let facts=TrustedAuditFacts{generation_id:options.generation_id,original_artifact_id:options.artifact_id,writer_audit_hash:options.writer_audit_hash.clone(),
            first_canonical_add_only_verified:true,canonical_ohlc_preserved_verified:true,historical_close_kernel_compatible_verified:true};
        let calculation=LegacyCalculation{fidelity:LegacyFidelity::VerifiedOriginalReconstruction,source_hash:"1".repeat(64),input_hash:"2".repeat(64),original_snapshot_hash:"3".repeat(64),proof_hash:Some("4".repeat(64)),proof:None,
            selected_minutes:setups_optimizer::ContinuousSelectedMinutes{version:1,evaluator:setups_optimizer::CONTINUOUS_SELECTED_MINUTES_EVALUATOR.into(),tp_pct:1.0,sl_pct:1.0,points:vec![]},daily:vec![],conservation:vec![],conservation_matched:true,inputs:vec![]};
        let pool=sqlx::postgres::PgPoolOptions::new().connect_lazy("postgres://fixture@127.0.0.1:1/never_connected_test").unwrap();
        assert_eq!(publish(&pool,&options,&calculation,&facts).await.unwrap_err(),"legacy_verified_publication_not_admitted");pool.close().await;
    }
}
