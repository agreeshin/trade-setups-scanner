//! Bounded original native ledger and exact IANA-day presentation. No market reads.
use crate::DbError;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value,json};
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Postgres, Row, Transaction};
use std::collections::BTreeMap;
use setups_domain::expectancy::{ExpectancyArtifact, MetricAvailability};

pub const MAX_MINUTES: usize = 100_000;
// Covers core's 256*100_000+256 serialized allowance, including JSONB spacing.
pub const MAX_LEDGER_BYTES: usize = 32 * 1024 * 1024;
pub const MAX_DISPLAY_DAYS: usize = 7322;
const MAX_LEDGERS: i64 = crate::expectancy::MAX_ARTIFACTS;
// A sidecar may not exhaust a smaller chart-only quota before its source
// artifact capacity. Existing pg_database_size admission remains the disk gate;
// deleting an actually unreferenced source artifact cascades this sidecar.
const MAX_TOTAL_BYTES: i64 = MAX_LEDGERS * MAX_LEDGER_BYTES as i64;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all="camelCase", deny_unknown_fields)]
pub struct OriginalMinute {
    pub signal_close_unix: i64, pub sum_r: f64, pub closed_count: u64, pub tp_count: u64, pub sl_count: u64,
}

/// The manual CPU task transfers only this bounded scalar payload after frozen
/// input cleanup. It is not serialized into the 8-MiB scientific result.
#[derive(Debug)]
pub struct ManualArchive {
    pub observation_cutoff:i64,
    pub tp_pct:f64,
    pub sl_pct:f64,
    pub kernel:String,
    pub minutes:Vec<OriginalMinute>,
}

fn payload_hash(points:&[OriginalMinute])->Result<String,DbError> {
    if points.len()>MAX_MINUTES {return Err(invalid("ledger_resource_limit"));}
    let mut digest=LedgerDigest{hash:Sha256::new(),bytes:0};
    serde_json::to_writer(&mut digest,points).map_err(|_|invalid("ledger_resource_limit"))?;
    Ok(format!("{:x}",digest.hash.finalize()))
}

fn source_key(source:&Value)->Result<String,DbError> {
    fn canonical(value:&Value)->Value {
        match value {
            Value::Object(object)=>Value::Object(object.iter().collect::<BTreeMap<_,_>>().into_iter().map(|(k,v)|(k.clone(),canonical(v))).collect()),
            Value::Array(items)=>Value::Array(items.iter().map(canonical).collect()),
            _=>value.clone(),
        }
    }
    let bytes=serde_json::to_vec(&canonical(source)).map_err(crate::json_error)?;
    if bytes.len()>65536 {return Err(invalid("manual_source_resource_limit"));}
    Ok(format!("{:x}",Sha256::digest(bytes)))
}

fn manual_source(owner:&str,study:i64,identity:&str,start:DateTime<Utc>,spec:&Value,result:&Value,archive:&ManualArchive)->Result<Value,DbError> {
    manual_source_checked(owner,study,identity,start,spec,result,archive,true)
}

fn manual_source_checked(owner:&str,study:i64,identity:&str,start:DateTime<Utc>,spec:&Value,result:&Value,archive:&ManualArchive,check_minutes:bool)->Result<Value,DbError> {
    let best=result.get("best").filter(|v|v.is_object()).ok_or_else(||invalid("manual_winner_missing"))?;
    let from=spec["fromUnix"].as_i64().ok_or_else(||invalid("manual_period_invalid"))?;
    let to=spec["toUnix"].as_i64().ok_or_else(||invalid("manual_period_invalid"))?;
    let a=&archive;
    if owner.is_empty() || study<=0 || identity.len()!=32 || from>=to || a.observation_cutoff%60!=0
        || a.kernel!="original-manual-close-1" || !a.tp_pct.is_finite() || !a.sl_pct.is_finite()
        || a.tp_pct<=0.0 || a.sl_pct<=0.0 || !(a.tp_pct/a.sl_pct).is_finite()
        || !best["provenance"].is_object() || !best["analytics"].is_object() {return Err(invalid("manual_source_invalid"));}
    let cell=best["matrix"].as_array().and_then(|cells|cells.iter().find(|c|c["tpPct"].as_f64()==Some(a.tp_pct)&&c["slPct"].as_f64()==Some(a.sl_pct)))
        .ok_or_else(||invalid("manual_pair_missing"))?;
    let expected_tp=cell["tpCount"].as_u64().ok_or_else(||invalid("manual_counts_invalid"))?;
    let expected_sl=cell["slCount"].as_u64().ok_or_else(||invalid("manual_counts_invalid"))?;
    if expected_tp.checked_add(expected_sl)!=cell["closedCount"].as_u64() {return Err(invalid("manual_counts_invalid"));}
    if check_minutes {
    let (mut tp,mut sl,mut last)=(0u64,0u64,None);
    if a.minutes.len()>MAX_MINUTES {return Err(invalid("ledger_resource_limit"));}
    for point in &a.minutes {
        if point.signal_close_unix%60!=0 || point.signal_close_unix<from || point.signal_close_unix>=to
            || point.signal_close_unix>a.observation_cutoff || last.is_some_and(|old|old>=point.signal_close_unix)
            || point.closed_count==0 || point.tp_count.checked_add(point.sl_count)!=Some(point.closed_count)
            || !conserved(point.sum_r,point.tp_count,point.sl_count,a.tp_pct/a.sl_pct) {return Err(invalid("manual_minute_invalid"));}
        tp=tp.checked_add(point.tp_count).ok_or_else(||invalid("count_overflow"))?;
        sl=sl.checked_add(point.sl_count).ok_or_else(||invalid("count_overflow"))?;
        last=Some(point.signal_close_unix);
    }
    if (tp,sl)!=(expected_tp,expected_sl) {return Err(invalid("manual_count_mismatch"));}
    }
    Ok(json!({"version":1,"ownerId":owner,"studyId":study,"studyIdentity":identity,"executionStartedAt":start,
        "observationCutoff":a.observation_cutoff,"fromUnix":from,"toUnix":to,"tpPct":a.tp_pct,"slPct":a.sl_pct,
        "kernel":a.kernel,"provenance":best["provenance"],"analytics":best["analytics"],
        "trendMode":best["trendMode"],"trendBendDays":best["trendBendDays"],"trendBlendLambda":best["trendBlendLambda"],"forecast":best["forecast"]}))
}

/// Same transaction as the manual terminal scientific result. Re-entry after an
/// uncertain ACK validates the existing immutable source and payload, not a new
/// version. No clock/market read, model fit or continuous owner is synthesized.
pub(crate) async fn seal_manual_in(tx:&mut Transaction<'_,Postgres>,study:i64,archive:&ManualArchive)->Result<(),DbError> {
    let row=sqlx::query("select owner_id,origin,status,spec,result,execution_started_at,md5(spec::text||coalesce(result::text,'null')) identity from optimization_studies where id=$1 for key share")
        .bind(study).fetch_optional(&mut **tx).await?.ok_or_else(||invalid("manual_study_missing"))?;
    if row.get::<String,_>("origin")!="manual" || !matches!(row.get::<String,_>("status").as_str(),"completed"|"cancelled"|"failed") {return Err(invalid("manual_terminal_required"));}
    let owner:String=row.get("owner_id");let identity:String=row.get("identity");
    let start:DateTime<Utc>=row.get::<Option<DateTime<Utc>>,_>("execution_started_at").ok_or_else(||invalid("manual_execution_start_missing"))?;
    let source=manual_source(&owner,study,&identity,start,&row.get::<Value,_>("spec"),&row.get::<Option<Value>,_>("result").ok_or_else(||invalid("manual_winner_missing"))?,archive)?;
    let key=source_key(&source)?;let hash=payload_hash(&archive.minutes)?;
    let stored_bytes:i64=sqlx::query_scalar("select octet_length($1::jsonb::text)::bigint").bind(sqlx::types::Json(&archive.minutes)).fetch_one(&mut **tx).await?;
    if stored_bytes>MAX_LEDGER_BYTES as i64 {return Err(invalid("ledger_resource_limit"));}
    // The same lock and shared quotas cover both genuine ownership variants.
    sqlx::query("select pg_advisory_xact_lock(72423439)").execute(&mut **tx).await?;
    let existing:Option<bool>=sqlx::query_scalar("select owner_id=$2 and source_key=$3 and study_identity=$4 and source=$5 and kernel=$6 and payload_sha256=$7 and minutes=$8::jsonb from study_manual_original_outcomes where study_id=$1")
        .bind(study).bind(&owner).bind(&key).bind(&identity).bind(&source).bind(&archive.kernel).bind(&hash).bind(sqlx::types::Json(&archive.minutes)).fetch_optional(&mut **tx).await?;
    if let Some(same)=existing {return if same {Ok(())}else{Err(invalid("immutable_ledger_conflict"))};}
    let (count,total)=ledger_capacity_in(tx).await?;
    if !storage_admitted(count,total,stored_bytes) {return Err(invalid("ledger_resource_limit"));}
    let utc=utc_days(&archive.minutes)?;let utc_hash=utc_projection_hash(&hash,&utc)?;
    sqlx::query("insert into study_manual_original_outcomes(study_id,owner_id,source_key,study_identity,kernel,source,minutes,payload_sha256,payload_bytes,utc_daily,utc_daily_sha256,utc_source_payload_sha256) values($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$8)")
        .bind(study).bind(owner).bind(key).bind(identity).bind(&archive.kernel).bind(source).bind(sqlx::types::Json(&archive.minutes)).bind(hash).bind(stored_bytes)
        .bind(sqlx::types::Json(&utc)).bind(utc_hash).execute(&mut **tx).await?;
    Ok(())
}

async fn ledger_capacity_in(tx:&mut Transaction<'_,Postgres>)->Result<(i64,i64),DbError> {
    Ok(sqlx::query_as("select count(*),coalesce(sum(payload_bytes),0)::bigint from (select payload_bytes from study_original_outcomes union all select payload_bytes from study_manual_original_outcomes) owned")
        .fetch_one(&mut **tx).await?)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all="camelCase")]
pub struct DisplayDay {
    pub day_unix: i64, pub date: String, pub sum_r: f64, pub mean_r: Option<f64>,
    pub closed_count: u64, pub tp_count: u64, pub sl_count: u64,
}

/// Trusted explicit materializer supplies these immutable identities, not HTTP
/// JSON. The caller verifies MD/kernel proof; this publication boundary rechecks
/// current ownership/source and complete closed-outcome conservation atomically.
#[derive(Debug,Clone)]
pub struct VerifiedOriginalIdentity {
    pub owner_id:String, pub study_id:i64, pub artifact_id:i64, pub artifact_key:String,
    pub study_identity:String, pub original_snapshot_hash:String,
    pub source_hash:String, pub input_hash:String, pub proof_hash:String,
    pub tp_pct:f64, pub sl_pct:f64,
}
#[derive(Debug,Clone,Serialize)]
#[serde(rename_all="camelCase")]
pub struct OriginalPublication {
    pub artifact_id:i64, pub source_identity:String, pub fidelity:String, pub created:bool,
}
fn invalid(code: &str) -> DbError { DbError::Json(format!("study_display_{code}")) }
fn conserved(sum:f64,tp:u64,sl:u64,payoff:f64)->bool {
    let positive=tp as f64*payoff;
    sum.is_finite() && (sum-(positive-sl as f64)).abs()<=1e-10*(positive+sl as f64).max(1.0)
}

/// Call only from native post-chain capture in the same fenced gate transaction.
/// UTC-bucket parity is an invariant check, NOT authorization for legacy reconstruction.
pub(crate) async fn seal_native_in(tx: &mut Transaction<'_,Postgres>, artifact: i64, points: &[OriginalMinute]) -> Result<(),DbError> {
    let row=sqlx::query("select artifact_key,numerical_snapshot,revision_key from study_expectancy_artifacts where id=$1 for key share")
        .bind(artifact).fetch_one(&mut **tx).await?;
    if row.get::<String,_>("revision_key")!="original" {return Err(invalid("not_original_artifact"));}
    let numerical:ExpectancyArtifact=serde_json::from_value(row.get("numerical_snapshot")).map_err(crate::json_error)?;
    validate_minutes(&numerical,points)?;
    let key:String=row.get("artifact_key");
    insert_ledger_in(tx,artifact,&key,points,"native_original",&json!({})).await.map(|_|())
}

/// Separate from the native-only sealer: never claims a reconstructed acquisition
/// hash matches the original. Does not update study/result/model/profile/replay.
pub async fn publish_verified_original_reconstruction(pool:&PgPool,source:&VerifiedOriginalIdentity,points:&[OriginalMinute],evidence:&Value)->Result<OriginalPublication,DbError> {
    validate_verified_evidence(source,evidence)?;
    publish_verified_in_storage(pool,source,points,evidence).await
}

fn validate_verified_evidence(source:&VerifiedOriginalIdentity,evidence:&Value)->Result<(),DbError> {
    let hash=|s:&str,n|s.len()==n&&s.bytes().all(|b|b.is_ascii_digit()||(b'a'..=b'f').contains(&b));
    if source.owner_id.is_empty() || source.owner_id.len()>120 || source.study_id<=0 || source.artifact_id<=0
        || !hash(&source.artifact_key,64) || !hash(&source.study_identity,32) || !hash(&source.original_snapshot_hash,64)
        || !hash(&source.source_hash,64) || !hash(&source.input_hash,64) || !hash(&source.proof_hash,64)
        || !source.tp_pct.is_finite() || !source.sl_pct.is_finite() || source.tp_pct<=0.0 || source.sl_pct<=0.0 {
        return Err(invalid("verified_identity_invalid"));
    }
    let object=evidence.as_object().ok_or_else(||invalid("verified_evidence_invalid"))?;
    if object.len()>32 || object.iter().any(|(k,v)|k.len()>64 || !matches!(v,Value::String(s) if s.len()<=512) && !v.is_number() && !v.is_boolean() && !v.is_null())
        || evidence["fidelity"]!="verified_original_reconstruction" || evidence["conservationMatched"]!=true
        || evidence["scope"]!="closed_outcomes_only" || evidence["firstCanonicalAddOnlyVerified"]!=true
        || evidence["canonicalOhlcPreservedVerified"]!=true || evidence["historicalCloseKernelCompatibleVerified"]!=true
        || !evidence["writerAuditHash"].as_str().is_some_and(|s|hash(s,64))
        || evidence["evaluator"]!="original-historical-close-1" || evidence["sourceHash"]!=source.source_hash
        || evidence["inputHash"]!=source.input_hash || evidence["proofHash"]!=source.proof_hash
        || evidence["originalSnapshotHash"]!=source.original_snapshot_hash || evidence["originalArtifactKey"]!=source.artifact_key
        || evidence["studyIdentity"]!=source.study_identity {return Err(invalid("verified_evidence_invalid"));}
    Ok(())
}

async fn publish_verified_in_storage(pool:&PgPool,source:&VerifiedOriginalIdentity,points:&[OriginalMinute],evidence:&Value)->Result<OriginalPublication,DbError> {
    let mut tx=pool.begin().await?;
    sqlx::query("set local statement_timeout='15000'").execute(&mut *tx).await?;
    let row=sqlx::query("select a.artifact_key,a.numerical_snapshot,a.tp_pct,a.sl_pct,a.revision_key,md5(s.spec::text||coalesce(s.result::text,'null')) study_identity,s.result->'best'->'provenance'->>'snapshotHash' snapshot_hash,s.status from study_expectancy_artifacts a join optimization_studies s on s.id=a.source_study_id and s.owner_id=a.owner_id where a.id=$1 and a.owner_id=$2 and a.source_study_id=$3 for share of a,s")
        .bind(source.artifact_id).bind(&source.owner_id).bind(source.study_id).fetch_optional(&mut *tx).await?.ok_or_else(||invalid("verified_source_missing"))?;
    if row.get::<String,_>("artifact_key")!=source.artifact_key || row.get::<String,_>("study_identity")!=source.study_identity
        || row.get::<Option<String>,_>("snapshot_hash").as_deref()!=Some(&source.original_snapshot_hash)
        || row.get::<String,_>("revision_key")!="original" || row.get::<f64,_>("tp_pct")!=source.tp_pct || row.get::<f64,_>("sl_pct")!=source.sl_pct
        || !matches!(row.get::<String,_>("status").as_str(),"completed"|"cancelled"|"failed") {return Err(invalid("verified_source_changed"));}
    let artifact:ExpectancyArtifact=serde_json::from_value(row.get("numerical_snapshot")).map_err(crate::json_error)?;
    if artifact.source.provenance.snapshot_hash!=source.original_snapshot_hash || artifact.source.study_id!=source.study_id.to_string()
        || artifact.source.derived_snapshot_id!=source.artifact_key {return Err(invalid("verified_artifact_mismatch"));}
    validate_minute_ledger(&artifact,points,false)?;
    let published=insert_ledger_in(&mut tx,source.artifact_id,&source.artifact_key,points,"verified_original_reconstruction",evidence).await?;
    tx.commit().await?;Ok(published)
}

async fn insert_ledger_in(tx:&mut Transaction<'_,Postgres>,artifact:i64,key:&str,points:&[OriginalMinute],fidelity:&str,evidence:&Value)->Result<OriginalPublication,DbError> {
    // Hash without retaining a second serialized payload or a per-point JSON
    // object graph. SQLx owns only its one bounded encode buffer per query.
    let mut digest=LedgerDigest{hash:Sha256::new(),bytes:0};
    serde_json::to_writer(&mut digest,points).map_err(|_|invalid("ledger_resource_limit"))?;
    let hash=format!("{:x}",digest.hash.finalize());
    let stored_bytes:i64=sqlx::query_scalar("select octet_length($1::jsonb::text)::bigint").bind(sqlx::types::Json(points)).fetch_one(&mut **tx).await?;
    if stored_bytes>MAX_LEDGER_BYTES as i64 {return Err(invalid("ledger_resource_limit"));}
    let evidence_bytes=serde_json::to_vec(evidence).map_err(crate::json_error)?;
    if evidence_bytes.len()>32768 {return Err(invalid("verified_evidence_resource_limit"));}
    let proof_hash=if fidelity=="native_original" {None}else{Some(source_key(evidence)?)};
    let source_identity=format!("{key}:{hash}{}",proof_hash.as_ref().map(|h|format!(":{h}")).unwrap_or_default());
    sqlx::query("select pg_advisory_xact_lock(72423439)").execute(&mut **tx).await?;
    let existing:Option<(bool,Option<String>)>=sqlx::query_as("select source_key=$2 and payload_sha256=$3 and minutes=$4::jsonb and fidelity=$5 and evidence=$6,evidence_sha256 from study_original_outcomes where artifact_id=$1")
        .bind(artifact).bind(key).bind(&hash).bind(sqlx::types::Json(points)).bind(fidelity).bind(evidence).fetch_optional(&mut **tx).await?;
    if let Some((matches,stored_proof))=existing {
        let valid_proof=match &stored_proof {Some(digest)=>evidence_digest_matches(evidence,digest)?,None=>fidelity=="native_original"};
        let source_identity=format!("{key}:{hash}{}",stored_proof.map(|h|format!(":{h}")).unwrap_or_default());
        return if matches&&valid_proof {Ok(OriginalPublication{artifact_id:artifact,source_identity,fidelity:fidelity.into(),created:false})}else{Err(invalid("immutable_ledger_conflict"))};
    }
    let (count,total)=ledger_capacity_in(tx).await?;
    if !storage_admitted(count,total,stored_bytes) {return Err(invalid("ledger_resource_limit"));}
    let utc=utc_days(points)?;let utc_hash=utc_projection_hash(&hash,&utc)?;
    sqlx::query("insert into study_original_outcomes(artifact_id,source_key,kernel,minutes,payload_sha256,payload_bytes,fidelity,evidence,evidence_sha256,utc_daily,utc_daily_sha256,utc_source_payload_sha256) values($1,$2,'original-historical-close-1',$3,$4,$5,$6,$7,$8,$9,$10,$4)")
        .bind(artifact).bind(key).bind(sqlx::types::Json(points)).bind(hash).bind(stored_bytes).bind(fidelity).bind(evidence).bind(proof_hash)
        .bind(sqlx::types::Json(&utc)).bind(utc_hash).execute(&mut **tx).await?;
    Ok(OriginalPublication{artifact_id:artifact,source_identity,fidelity:fidelity.into(),created:true})
}

fn evidence_digest_matches(evidence:&Value,expected:&str)->Result<bool,DbError> {
    if source_key(evidence)?==expected {return Ok(true);}
    // 1c5 wrote this bounded CLI header in field order before JSONB reordered
    // keys. Preserve its historical digest/identity; never rewrite old proofs.
    const FIELDS:&[&str]=&["fidelity","scope","proofBasis","sourceHash","inputHash","proofHash","originalSnapshotHash","originalArtifactKey","studyIdentity","conservationMatched","evaluator","writerAuditHash","firstCanonicalAddOnlyVerified","canonicalOhlcPreservedVerified","historicalCloseKernelCompatibleVerified","instrumentCount","utcDayCount","closedCount","tpCount","slCount"];
    let object=evidence.as_object().ok_or_else(||invalid("verified_evidence_invalid"))?;
    if object.len()!=FIELDS.len() {return Ok(false);}
    let mut ordered=serde_json::Map::new();
    for &key in FIELDS {
        let Some(value)=object.get(key) else {return Ok(false);};ordered.insert(key.into(),value.clone());
    }
    let bytes=serde_json::to_vec(&ordered).map_err(crate::json_error)?;
    Ok(format!("{:x}",Sha256::digest(bytes))==expected)
}

fn storage_admitted(count:i64,total:i64,requested:i64)->bool {
    count>=0 && count<MAX_LEDGERS && total>=0 && requested>=0 && requested<=MAX_LEDGER_BYTES as i64
        && total.saturating_add(requested)<=MAX_TOTAL_BYTES
}
struct LedgerDigest {hash:Sha256,bytes:usize}
impl std::io::Write for LedgerDigest {
    fn write(&mut self,bytes:&[u8])->std::io::Result<usize> {
        self.bytes=self.bytes.checked_add(bytes.len()).filter(|n|*n<=MAX_LEDGER_BYTES)
            .ok_or_else(||std::io::Error::other("original ledger byte limit"))?;
        self.hash.update(bytes);Ok(bytes.len())
    }
    fn flush(&mut self)->std::io::Result<()> {Ok(())}
}

pub fn validate_minutes(artifact:&ExpectancyArtifact, points:&[OriginalMinute])->Result<(),DbError> {
    validate_minute_ledger(artifact,points,true)
}
fn validate_minute_ledger(artifact:&ExpectancyArtifact, points:&[OriginalMinute],require_daily:bool)->Result<(),DbError> {
    artifact.validate().map_err(|_|invalid("original_artifact_invalid"))?;
    if points.len()>MAX_MINUTES {return Err(invalid("ledger_resource_limit"));}
    let mut days:BTreeMap<i64,(u64,u64,f64)>=BTreeMap::new();let mut last=None;
    for p in points {
        if p.signal_close_unix%60!=0 || p.signal_close_unix<artifact.from_unix || p.signal_close_unix>=artifact.to_unix
            || p.signal_close_unix>artifact.source.provenance.observation_cutoff
            || last.is_some_and(|v|v>=p.signal_close_unix) || !p.sum_r.is_finite()
            || p.tp_count.checked_add(p.sl_count)!=Some(p.closed_count)
            || (p.closed_count==0 && p.sum_r!=0.0)
            || !conserved(p.sum_r,p.tp_count,p.sl_count,artifact.source.tp_pct/artifact.source.sl_pct) {
            return Err(invalid("original_minute_invalid"));
        }
        last=Some(p.signal_close_unix);
        let day=days.entry(p.signal_close_unix.div_euclid(86400)*86400).or_default();
        day.0=day.0.checked_add(p.tp_count).ok_or_else(||invalid("count_overflow"))?;
        day.1=day.1.checked_add(p.sl_count).ok_or_else(||invalid("count_overflow"))?;day.2+=p.sum_r;
        if !day.2.is_finite(){return Err(invalid("sum_overflow"));}
    }
    let metrics=artifact.selected_cell.as_ref().ok_or_else(||invalid("original_selected_metrics_missing"))?;
    let (tp,sl)=days.values().try_fold((0u64,0u64),|(tp,sl),d|Some((tp.checked_add(d.0)?,sl.checked_add(d.1)?))).ok_or_else(||invalid("count_overflow"))?;
    if tp!=metrics.tp_count || sl!=metrics.sl_count || tp.checked_add(sl)!=Some(metrics.closed_count) {return Err(invalid("original_count_mismatch"));}
    let MetricAvailability::Available{value:daily}=&artifact.daily else {
        return if require_daily {Err(invalid("original_daily_missing"))}else{Ok(())};
    };
    for p in daily {
        let (tp,sl,sum)=days.remove(&p.day_start_unix).unwrap_or_default();
        let gross=tp as f64*(artifact.source.tp_pct/artifact.source.sl_pct)+sl as f64;
        if tp!=p.tp_count || sl!=p.sl_count || p.mean_r.is_some_and(|mean|(sum-mean*(tp+sl) as f64).abs()>1e-10*gross.max(1.0)) {return Err(invalid("original_daily_mismatch"));}
    }
    if !days.is_empty() {return Err(invalid("original_daily_mismatch"));}
    Ok(())
}

pub struct DisplaySnapshot {
    pub actual_row:Option<Value>, pub start_day_unix:Option<i64>, pub original_identity:Option<String>,
    pub original_fidelity:Option<String>,
    pub raw_replay_available:bool, pub original_daily:Vec<DisplayDay>, pub combined_daily:Vec<DisplayDay>,
}

/// Canonical observed projection, independent of scientific daily row weights,
/// model fitting and actual-refresh eligibility. None means legacy absence;
/// present-but-invalid is an error, never a sealed-daily fallback.
#[derive(Debug)]
pub struct ObservedArchive {
    pub source_identity:String,
    pub fidelity:String,
    pub time_zone:String,
    pub daily:Vec<DisplayDay>,
}

impl ObservedArchive {
    /// Display-only adapter. row_count is CLOSED support here and must never
    /// replace the sealed scientific daily weights or be fed to a model fit.
    pub fn utc_points(&self,origin:i64)->Result<Vec<setups_domain::expectancy::UtcDailyPoint>,DbError> {
        if self.time_zone!="UTC" {return Err(invalid("utc_projection_zone_mismatch"));}
        Ok(self.daily.iter().map(|p|setups_domain::expectancy::UtcDailyPoint {
            day_start_unix:p.day_unix,date:p.date.clone(),x_utc_days:(i128::from(p.day_unix)-i128::from(origin)) as f64/86400.0,
            row_count:p.closed_count,tp_count:p.tp_count,sl_count:p.sl_count,mean_r:p.mean_r,
        }).collect())
    }
}

fn utc_projection_hash(payload:&str,days:&[DisplayDay])->Result<String,DbError> {
    if days.len()>MAX_DISPLAY_DAYS {return Err(invalid("daily_resource_limit"));}
    let mut writer=LedgerDigest{hash:Sha256::new(),bytes:0};
    writer.hash.update(b"original-utc-projection-v1\0");writer.hash.update(payload.as_bytes());
    serde_json::to_writer(&mut writer,days).map_err(|_|invalid("daily_resource_limit"))?;
    if writer.bytes>4*1024*1024 {return Err(invalid("daily_resource_limit"));}
    Ok(format!("{:x}",writer.hash.finalize()))
}

fn utc_days(points:&[OriginalMinute])->Result<Vec<DisplayDay>,DbError> {
    let mut days=BTreeMap::<i64,DisplayDay>::new();
    for p in points {
        let at=p.signal_close_unix.div_euclid(86400)*86400;
        let day=days.entry(at).or_insert(DisplayDay{day_unix:at,date:String::new(),sum_r:0.0,mean_r:None,closed_count:0,tp_count:0,sl_count:0});
        day.sum_r+=p.sum_r;
        day.tp_count=day.tp_count.checked_add(p.tp_count).ok_or_else(||invalid("count_overflow"))?;
        day.sl_count=day.sl_count.checked_add(p.sl_count).ok_or_else(||invalid("count_overflow"))?;
        day.closed_count=day.closed_count.checked_add(p.closed_count).ok_or_else(||invalid("count_overflow"))?;
        if !day.sum_r.is_finite() {return Err(invalid("invalid_sum"));}
    }
    let (Some(&first),Some(&last))=(days.keys().next(),days.keys().next_back()) else {return Ok(vec![]);};
    let count=usize::try_from((last-first)/86400+1).map_err(|_|invalid("daily_resource_limit"))?;
    if count>MAX_DISPLAY_DAYS {return Err(invalid("daily_resource_limit"));}
    let mut result=Vec::with_capacity(count);
    for i in 0..count {
        let at=first+i as i64*86400;
        let mut day=days.remove(&at).unwrap_or(DisplayDay{day_unix:at,date:String::new(),sum_r:0.0,mean_r:None,closed_count:0,tp_count:0,sl_count:0});
        day.date=DateTime::<Utc>::from_timestamp(at,0).ok_or_else(||invalid("invalid_day"))?.format("%Y-%m-%d").to_string();
        day.mean_r=(day.closed_count>0).then(||day.sum_r/day.closed_count as f64);result.push(day);
    }
    Ok(result)
}

async fn valid_zone_in(tx:&mut Transaction<'_,Postgres>,zone:&str)->Result<(),DbError> {
    if zone.is_empty() || zone.len()>128 || !zone.is_ascii() {return Err(invalid("invalid_time_zone"));}
    let valid:bool=sqlx::query_scalar("select exists(select 1 from pg_timezone_names where name=$1)").bind(zone).fetch_one(&mut **tx).await?;
    if valid {Ok(())}else{Err(invalid("invalid_time_zone"))}
}

pub async fn original_observed(pool:&PgPool,owner:&str,study:i64,artifact:Option<i64>,zone:&str)->Result<Option<ObservedArchive>,DbError> {
    let mut tx=pool.begin().await?;
    sqlx::query("set transaction isolation level repeatable read, read only").execute(&mut *tx).await?;
    sqlx::query("set local statement_timeout='5000'").execute(&mut *tx).await?;
    valid_zone_in(&mut tx,zone).await?;
    let result=original_observed_in(&mut tx,owner,study,artifact,zone).await?;
    tx.commit().await?;Ok(result)
}

async fn original_observed_in(tx:&mut Transaction<'_,Postgres>,owner:&str,study:i64,artifact:Option<i64>,zone:&str)->Result<Option<ObservedArchive>,DbError> {
    // SQL caps BEFORE decoding JSON. Cached UTC reads retain only <=7322 day
    // scalars; IANA/legacy reads own <=32 MiB input and <=100k scalar minutes,
    // then release them before aggregation. No request-global cache/MD work.
    let row=if let Some(artifact)=artifact {
        sqlx::query("select o.source_key,o.payload_sha256,o.evidence_sha256,o.evidence,o.fidelity,o.kernel,a.artifact_key,a.numerical_snapshot,md5(s.spec::text||coalesce(s.result::text,'null')) current_identity,o.utc_daily,o.utc_daily_sha256,o.utc_source_payload_sha256,case when $4='UTC' and o.utc_daily is not null then null when o.payload_bytes between 0 and 33554432 and jsonb_array_length(o.minutes)<=100000 then o.minutes else null end minutes from study_original_outcomes o join study_expectancy_artifacts a on a.id=o.artifact_id left join optimization_studies s on s.id=a.source_study_id and s.owner_id=a.owner_id where o.artifact_id=$1 and a.owner_id=$2 and a.source_study_id=$3 and a.revision_key='original'")
            .bind(artifact).bind(owner).bind(study).bind(zone).fetch_optional(&mut **tx).await?
    }else{
        sqlx::query("select o.source_key,o.payload_sha256,null::text evidence_sha256,'native_original'::text fidelity,o.kernel,o.source,o.study_identity,md5(s.spec::text||coalesce(s.result::text,'null')) current_identity,s.spec,s.result,s.execution_started_at,o.utc_daily,o.utc_daily_sha256,o.utc_source_payload_sha256,case when $4='UTC' and o.utc_daily is not null then null when o.payload_bytes between 0 and 33554432 and jsonb_array_length(o.minutes)<=100000 then o.minutes else null end minutes from study_manual_original_outcomes o join optimization_studies s on s.id=o.study_id and s.owner_id=o.owner_id where o.study_id=$1 and o.owner_id=$2 and s.id=$3 and s.origin='manual'")
            .bind(study).bind(owner).bind(study).bind(zone).fetch_optional(&mut **tx).await?
    };
    let Some(row)=row else {return Ok(None);};
    let key:String=row.try_get("source_key")?;let hash:String=row.try_get("payload_sha256")?;
    let fidelity:String=row.try_get("fidelity")?;let evidence:Option<String>=row.try_get("evidence_sha256")?;
    let identity=format!("{key}:{hash}{}",evidence.as_ref().map(|v|format!(":{v}")).unwrap_or_default());
    let kernel:String=row.try_get("kernel")?;
    let numerical=if artifact.is_some() {
        if key!=row.try_get::<String,_>("artifact_key")? || kernel!="original-historical-close-1" {return Err(invalid("archive_source_mismatch"));}
        let value:ExpectancyArtifact=serde_json::from_value(row.try_get("numerical_snapshot")?).map_err(crate::json_error)?;
        value.validate().map_err(|_|invalid("original_artifact_invalid"))?;
        if value.source.study_id!=study.to_string() || value.source.derived_snapshot_id!=key {return Err(invalid("archive_source_mismatch"));}
        let proof:Value=row.try_get("evidence")?;
        if fidelity=="native_original" {
            if proof!=json!({}) || evidence.is_some() {return Err(invalid("verified_evidence_invalid"));}
        }else if fidelity=="verified_original_reconstruction" {
            let text=|field:&str|proof[field].as_str().map(str::to_owned).ok_or_else(||invalid("verified_evidence_invalid"));
            let source=VerifiedOriginalIdentity{owner_id:owner.into(),study_id:study,artifact_id:artifact.expect("continuous artifact"),
                artifact_key:key.clone(),study_identity:row.try_get::<Option<String>,_>("current_identity")?.unwrap_or(text("studyIdentity")?),
                original_snapshot_hash:value.source.provenance.snapshot_hash.clone(),source_hash:text("sourceHash")?,input_hash:text("inputHash")?,
                proof_hash:text("proofHash")?,tp_pct:value.source.tp_pct,sl_pct:value.source.sl_pct};
            validate_verified_evidence(&source,&proof)?;
            if !evidence_digest_matches(&proof,evidence.as_deref().ok_or_else(||invalid("verified_evidence_invalid"))?)? {return Err(invalid("verified_evidence_corrupt"));}
        }else{return Err(invalid("verified_evidence_invalid"));}
        Some(value)
    }else{None};
    let manual=if artifact.is_none() {
        let source:Value=row.try_get("source")?;
        if row.try_get::<String,_>("study_identity")?!=row.try_get::<String,_>("current_identity")? || source_key(&source)?!=key
            || kernel!="original-manual-close-1" {return Err(invalid("archive_source_mismatch"));}
        let archive=ManualArchive{observation_cutoff:source["observationCutoff"].as_i64().ok_or_else(||invalid("manual_source_invalid"))?,
            tp_pct:source["tpPct"].as_f64().ok_or_else(||invalid("manual_source_invalid"))?,sl_pct:source["slPct"].as_f64().ok_or_else(||invalid("manual_source_invalid"))?,kernel:kernel.clone(),minutes:vec![]};
        let check=manual_source_checked(owner,study,&row.try_get::<String,_>("current_identity")?,
            row.try_get::<Option<DateTime<Utc>>,_>("execution_started_at")?.ok_or_else(||invalid("manual_execution_start_missing"))?,
            &row.try_get::<Value,_>("spec")?,&row.try_get::<Option<Value>,_>("result")?.ok_or_else(||invalid("manual_winner_missing"))?,&archive,false)?;
        if check!=source {return Err(invalid("archive_source_mismatch"));}
        Some(source)
    }else{None};
    let (expected_tp,expected_sl,payoff)=if let Some(value)=&numerical {
        let cell=value.selected_cell.as_ref().ok_or_else(||invalid("original_selected_metrics_missing"))?;
        (cell.tp_count,cell.sl_count,value.source.tp_pct/value.source.sl_pct)
    }else{
        let source=manual.as_ref().expect("validated manual source");
        let result:Value=row.try_get("result")?;
        let cell=result["best"]["matrix"].as_array().and_then(|cells|cells.iter().find(|c|c["tpPct"]==source["tpPct"]&&c["slPct"]==source["slPct"]))
            .ok_or_else(||invalid("manual_pair_missing"))?;
        (cell["tpCount"].as_u64().ok_or_else(||invalid("manual_counts_invalid"))?,cell["slCount"].as_u64().ok_or_else(||invalid("manual_counts_invalid"))?,
            source["tpPct"].as_f64().expect("validated manual TP")/source["slPct"].as_f64().expect("validated manual SL"))
    };
    let cached:Option<sqlx::types::Json<Vec<DisplayDay>>>=row.try_get("utc_daily")?;
    let daily=if zone=="UTC" && cached.is_some() {
        let days=cached.expect("cached UTC projection present").0;
        if row.try_get::<Option<String>,_>("utc_source_payload_sha256")?.as_deref()!=Some(&hash)
            || row.try_get::<Option<String>,_>("utc_daily_sha256")?.as_deref()!=Some(&utc_projection_hash(&hash,&days)?) {return Err(invalid("archive_projection_corrupt"));}
        // Digest is bound to the immutable canonical payload, not model daily
        // weights. Also reject malformed counts/means/order, including nulls.
        let mut previous=None;
        for p in &days {
            if p.day_unix%86400!=0 || previous.is_some_and(|at|at>=p.day_unix) || p.tp_count.checked_add(p.sl_count)!=Some(p.closed_count)
                || !p.sum_r.is_finite() || p.mean_r!=(p.closed_count>0).then(||p.sum_r/p.closed_count as f64)
                || DateTime::<Utc>::from_timestamp(p.day_unix,0).map(|at|at.format("%Y-%m-%d").to_string()).as_deref()!=Some(&p.date) {return Err(invalid("archive_projection_corrupt"));}
            previous=Some(p.day_unix);
        }
        days
    }else{
        let points=row.try_get::<Option<sqlx::types::Json<Vec<OriginalMinute>>>,_>("minutes")?.ok_or_else(||invalid("ledger_resource_limit"))?.0;
        if payload_hash(&points)?!=hash {return Err(invalid("archive_payload_corrupt"));}
        if let Some(numerical)=&numerical {validate_minute_ledger(numerical,&points,fidelity=="native_original")?;}
        if let Some(source)=manual {
            let archive=ManualArchive{observation_cutoff:source["observationCutoff"].as_i64().ok_or_else(||invalid("manual_source_invalid"))?,tp_pct:source["tpPct"].as_f64().ok_or_else(||invalid("manual_source_invalid"))?,sl_pct:source["slPct"].as_f64().ok_or_else(||invalid("manual_source_invalid"))?,kernel,minutes:points};
            let start=row.try_get::<Option<DateTime<Utc>>,_>("execution_started_at")?.ok_or_else(||invalid("manual_execution_start_missing"))?;
            let current=row.try_get::<String,_>("current_identity")?;
            let check=manual_source(owner,study,&current,start,&row.try_get::<Value,_>("spec")?,&row.try_get::<Option<Value>,_>("result")?.ok_or_else(||invalid("manual_winner_missing"))?,&archive)?;
            if check!=source {return Err(invalid("archive_source_mismatch"));}
        }
        let rows=sqlx::query("with owned as (select minutes from study_original_outcomes where artifact_id=$1::bigint union all select minutes from study_manual_original_outcomes where $1::bigint is null and study_id=$2 and owner_id=$3), p as (select (x->>'signalCloseUnix')::bigint signal,(x->>'sumR')::float8 sum_r,(x->>'closedCount')::bigint closed,(x->>'tpCount')::bigint tp,(x->>'slCount')::bigint sl from owned cross join lateral jsonb_array_elements(minutes) x), d as (select timezone($4,to_timestamp(signal))::date as day,sum(sum_r)::float8 sum_r,sum(closed)::bigint closed,sum(tp)::bigint tp,sum(sl)::bigint sl from p group by 1) select day::text date,study_local_day_start($4,day) day_unix,sum_r,closed,tp,sl from d order by day limit 7323")
            .bind(artifact).bind(study).bind(owner).bind(zone).fetch_all(&mut **tx).await?;
        fill_calendar(tx,zone,decode_days(rows)?).await?
    };
    let (mut tp,mut sl)=(0u64,0u64);
    for point in &daily {
        if point.tp_count.checked_add(point.sl_count)!=Some(point.closed_count) || !conserved(point.sum_r,point.tp_count,point.sl_count,payoff) {
            return Err(invalid("archive_projection_corrupt"));
        }
        tp=tp.checked_add(point.tp_count).ok_or_else(||invalid("count_overflow"))?;
        sl=sl.checked_add(point.sl_count).ok_or_else(||invalid("count_overflow"))?;
    }
    if (tp,sl)!=(expected_tp,expected_sl) {return Err(invalid("archive_projection_count_mismatch"));}
    Ok(Some(ObservedArchive{source_identity:identity,fidelity,time_zone:zone.into(),daily}))
}

pub async fn display_snapshot(pool:&PgPool,owner:&str,study:i64,zone:&str,start:Option<DateTime<Utc>>,artifact:Option<i64>,source_identity:Option<&str>)->Result<DisplaySnapshot,DbError> {
    if zone.is_empty() || zone.len()>128 || !zone.is_ascii() {return Err(invalid("invalid_time_zone"));}
    let mut tx=pool.begin().await?;
    sqlx::query("set transaction isolation level repeatable read, read only").execute(&mut *tx).await?;
    sqlx::query("set local statement_timeout='5000'").execute(&mut *tx).await?;
    let valid:bool=sqlx::query_scalar("select exists(select 1 from pg_timezone_names where name=$1)").bind(zone).fetch_one(&mut *tx).await?;
    if !valid {return Err(invalid("invalid_time_zone"));}
    let actual_row=sqlx::query_scalar::<_,Value>("select (to_jsonb(a)-'source')||jsonb_build_object('sourceFacts',a.source-'artifact') from study_actualizations a where owner_id=$1 and study_id=$2")
        .bind(owner).bind(study).fetch_optional(&mut *tx).await?;
    let start=start.or_else(||actual_row.as_ref()?.get("execution_started_at")?.as_str()?.parse::<DateTime<Utc>>().ok());
    let start_day_unix=match start {
        Some(s)=>Some(sqlx::query_scalar::<_,i64>("select study_local_day_start($1,timezone($1,$2::timestamptz)::date)")
            .bind(zone).bind(s).fetch_one(&mut *tx).await?),None=>None,
    };
    if let Some(expected)=source_identity {
        let matches:bool=sqlx::query_scalar("select exists(select 1 from optimization_studies where owner_id=$1 and id=$2 and md5(spec::text||coalesce(result::text,'null'))=$3)")
            .bind(owner).bind(study).bind(expected).fetch_one(&mut *tx).await?;
        if !matches {return Err(invalid("source_changed"));}
    }
    let original=original_observed_in(&mut tx,owner,study,artifact,zone).await?;
    let (original_identity,original_fidelity,original_daily)=match original {
        Some(source)=>(Some(source.source_identity),Some(source.fidelity),source.daily),
        None=>(None,None,vec![]),
    };
    let raw_replay_available:bool=sqlx::query_scalar("select exists(select 1 from replay_runs r join replay_batches b on b.id=r.batch_id where r.owner_id=$1 and b.owner_id=$1 and r.origin='continuous' and (b.expectancy_artifact_id=$3 or r.continuous_generation_id=(select continuous_generation_id from optimization_studies where id=$2 and owner_id=$1)))")
        .bind(owner).bind(study).bind(artifact).fetch_one(&mut *tx).await?;
    let actual_daily=if let Some(row)=actual_row.as_ref().filter(|r|r["status"]=="completed") {
        if source_identity.is_some_and(|id|row["source_identity"].as_str()!=Some(id)) {return Err(invalid("source_changed"));}
        let bad:bool=sqlx::query_scalar("select exists(select 1 from study_actual_setups where owner_id=$1 and study_id=$2 and revision=$3 and (signal_close_unix<extract(epoch from $4::timestamptz) or signal_close_unix>$5 or row_data->>'status' not in ('tp','sl','gate_failed','unclosed','no_data') or row_data->>'status' in ('tp','sl') and (row_data->>'riskReturn') is null))")
            .bind(owner).bind(study).bind(row["revision"].as_i64().ok_or_else(||invalid("invalid_revision"))?).bind(start)
            .bind(row["observed_through_unix"].as_i64().ok_or_else(||invalid("invalid_cutoff"))?).fetch_one(&mut *tx).await?;
        if bad {return Err(invalid("actual_outcome_invariant"));}
        let rows=sqlx::query("with d as (select timezone($4,to_timestamp(signal_close_unix))::date as day,coalesce(sum((row_data->>'riskReturn')::float8) filter(where row_data->>'status' in ('tp','sl')),0)::float8 sum_r,count(*) filter(where row_data->>'status' in ('tp','sl'))::bigint closed,count(*) filter(where row_data->>'status'='tp')::bigint tp,count(*) filter(where row_data->>'status'='sl')::bigint sl from study_actual_setups where owner_id=$1 and study_id=$2 and revision=$3 group by 1) select day::text date,study_local_day_start($4,day) day_unix,sum_r,closed,tp,sl from d order by day limit 7323")
            .bind(owner).bind(study).bind(row["revision"].as_i64().ok_or_else(||invalid("invalid_revision"))?).bind(zone).fetch_all(&mut *tx).await?;
        decode_days(rows)?
    }else{vec![]};
    let combined_daily=fill_calendar(&mut tx,zone,merge_days(original_daily.clone(),actual_daily)?).await?;
    let original_daily=fill_calendar(&mut tx,zone,original_daily).await?;
    tx.commit().await?;
    Ok(DisplaySnapshot{actual_row,start_day_unix,original_identity,original_fidelity,raw_replay_available,original_daily,combined_daily})
}

async fn fill_calendar(tx:&mut Transaction<'_,Postgres>,zone:&str,points:Vec<DisplayDay>)->Result<Vec<DisplayDay>,DbError> {
    let (Some(first),Some(last))=(points.first(),points.last()) else {return Ok(points);};
    // Generate LOCAL dates, not fixed 86400-second increments. LIMIT bounds even a malformed long interval.
    let rows=sqlx::query("select d::date::text date,study_local_day_start($1,d::date) day_unix from generate_series($2::text::date::timestamp,$3::text::date::timestamp,interval '1 day') d limit 7323")
        .bind(zone).bind(&first.date).bind(&last.date).fetch_all(&mut **tx).await?;
    if rows.len()>MAX_DISPLAY_DAYS {return Err(invalid("daily_resource_limit"));}
    let mut points:BTreeMap<_,_>=points.into_iter().map(|p|(p.day_unix,p)).collect();
    rows.into_iter().map(|row| {
        let at:i64=row.try_get("day_unix")?;
        Ok(points.remove(&at).unwrap_or(DisplayDay{day_unix:at,date:row.try_get("date")?,sum_r:0.0,mean_r:None,closed_count:0,tp_count:0,sl_count:0}))
    }).collect()
}

fn decode_days(rows:Vec<sqlx::postgres::PgRow>)->Result<Vec<DisplayDay>,DbError> {
    if rows.len()>MAX_DISPLAY_DAYS {return Err(invalid("daily_resource_limit"));}
    rows.into_iter().map(|r| {
        let closed=u64::try_from(r.try_get::<i64,_>("closed")?).map_err(|_|invalid("invalid_counts"))?;
        let sum:f64=r.try_get("sum_r")?;
        if !sum.is_finite(){return Err(invalid("invalid_sum"));}
        Ok(DisplayDay{day_unix:r.try_get("day_unix")?,date:r.try_get("date")?,sum_r:sum,mean_r:if closed>0{Some(sum/closed as f64)}else{None},
            closed_count:closed,tp_count:u64::try_from(r.try_get::<i64,_>("tp")?).map_err(|_|invalid("invalid_counts"))?,
            sl_count:u64::try_from(r.try_get::<i64,_>("sl")?).map_err(|_|invalid("invalid_counts"))?})
    }).collect()
}

pub fn merge_days(original:Vec<DisplayDay>,actual:Vec<DisplayDay>)->Result<Vec<DisplayDay>,DbError> {
    let mut days:BTreeMap<i64,DisplayDay>=BTreeMap::new();
    for p in original.into_iter().chain(actual) {
        let day=days.entry(p.day_unix).or_insert_with(||DisplayDay{day_unix:p.day_unix,date:p.date.clone(),sum_r:0.0,mean_r:None,closed_count:0,tp_count:0,sl_count:0});
        if day.date!=p.date || !p.sum_r.is_finite() || p.tp_count.checked_add(p.sl_count)!=Some(p.closed_count) {return Err(invalid("invalid_day"));}
        day.sum_r+=p.sum_r;day.closed_count=day.closed_count.checked_add(p.closed_count).ok_or_else(||invalid("count_overflow"))?;
        day.tp_count=day.tp_count.checked_add(p.tp_count).ok_or_else(||invalid("count_overflow"))?;
        day.sl_count=day.sl_count.checked_add(p.sl_count).ok_or_else(||invalid("count_overflow"))?;
        if !day.sum_r.is_finite() || days.len()>MAX_DISPLAY_DAYS {return Err(invalid("daily_resource_limit"));}
    }
    Ok(days.into_values().map(|mut p|{p.mean_r=if p.closed_count>0{Some(p.sum_r/p.closed_count as f64)}else{None};p}).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn chart_storage_does_not_saturate_before_admitted_source_artifact_capacity() {
        assert!(MAX_LEDGER_BYTES>=256*MAX_MINUTES+256);
        assert!(storage_admitted(8,64*1024*1024,8*1024*1024),"ninth historical ledger is not a scientific gate failure");
        assert!(storage_admitted(MAX_LEDGERS-1,(MAX_LEDGERS-1)*MAX_LEDGER_BYTES as i64,MAX_LEDGER_BYTES as i64));
        assert!(!storage_admitted(MAX_LEDGERS,MAX_TOTAL_BYTES,0));
        assert!(!storage_admitted(0,0,MAX_LEDGER_BYTES as i64+1));
    }
    #[test]
    fn merge_is_weighted_and_zero_is_not_a_gap() {
        let day=|sum,closed,tp,sl|DisplayDay{day_unix:123,date:"2026-11-21".into(),sum_r:sum,mean_r:None,closed_count:closed,tp_count:tp,sl_count:sl};
        let merged=merge_days(vec![day(2.0,4,3,1)],vec![day(-1.0,1,0,1)]).unwrap();
        assert_eq!((merged[0].sum_r,merged[0].closed_count,merged[0].mean_r),(1.0,5,Some(0.2)));
        let zero=merge_days(vec![day(0.0,2,1,1)],vec![]).unwrap();assert_eq!(zero[0].mean_r,Some(0.0));
        let gap=merge_days(vec![day(0.0,0,0,0)],vec![]).unwrap();assert_eq!(gap[0].mean_r,None);
        assert!(merge_days(vec![day(f64::NAN,1,1,0)],vec![]).is_err());
        assert!(merge_days(vec![day(1.0,2,1,0)],vec![]).is_err());
    }
}
