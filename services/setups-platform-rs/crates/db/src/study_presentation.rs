//! A single atomically published current PNG/metrics bundle per study. No fit,
//! market data or GET-time initialization. Frozen deliveries own their copies.
use crate::DbError;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Postgres, Row, Transaction};

pub const PRESENTATION: &str = "study_current_v1";
pub const RENDER_LOCK: i64 = 72_423_442;
const STORAGE_LOCK: i64 = 72_423_443;
pub const MAX_ROWS: i64 = 8192;
pub const MAX_TOTAL_BYTES: i64 = 128 * 1024 * 1024;

pub struct Prepared {
    pub owner: String,
    pub study: i64,
    pub source_identity: String,
    pub snapshot: Value,
    pub numerical: Value,
    pub png: Vec<u8>,
}

pub fn unavailable(reason: &str) -> Value {
    json!({"status":"unavailable","reason":reason})
}
pub fn metadata(snapshot: Option<Value>, error: Option<String>) -> Value {
    snapshot.map_or_else(|| unavailable(error.as_deref().unwrap_or("study_presentation_not_initialized")),
        |snapshot| json!({"status":"available","snapshot":snapshot}))
}
pub async fn summary(pool: &PgPool, owner: &str, study: i64) -> Result<Value, DbError> {
    let row = sqlx::query("select snapshot,error from study_current_presentations where owner_id=$1 and study_id=$2")
        .bind(owner).bind(study).fetch_optional(pool).await?;
    Ok(match row { Some(row) => metadata(row.try_get("snapshot")?, row.try_get("error")?), None => unavailable("study_presentation_not_initialized") })
}

pub enum Png { Available(Vec<u8>), Changed, Unavailable }
pub async fn png(pool: &PgPool, owner: &str, study: i64, revision: i64) -> Result<Png, DbError> {
    let row = sqlx::query("select (snapshot->>'revision')::bigint revision,case when (snapshot->>'revision')::bigint=$3 then png end png,png_sha256 from study_current_presentations where owner_id=$1 and study_id=$2 and snapshot is not null")
        .bind(owner).bind(study).bind(revision).fetch_optional(pool).await?;
    let Some(row) = row else { return Ok(Png::Unavailable); };
    if row.try_get::<i64,_>("revision")? != revision { return Ok(Png::Changed); }
    let png: Vec<u8> = row.try_get("png")?;
    if png.len()>crate::expectancy::MAX_PNG_BYTES || format!("{:x}",Sha256::digest(&png))!=row.try_get::<String,_>("png_sha256")? {
        return Err(DbError::Json("study_presentation_png_corrupt".into()));
    }
    Ok(Png::Available(png))
}

/// The caller holds the owner lock and its publication fence. All fields are
/// switched in this transaction, or none are. Re-entry after a lost ACK is
/// idempotent and never allocates a second revision/cutoff.
pub async fn publish_in(tx: &mut Transaction<'_,Postgres>, prepared: &Prepared) -> Result<bool, DbError> {
    let invalid=||DbError::Json("study_presentation_invalid_bundle".into());
    if prepared.png.is_empty() || prepared.png.len()>crate::expectancy::MAX_PNG_BYTES
        || serde_json::to_vec(&prepared.snapshot).map_err(crate::json_error)?.len()>32768
        || serde_json::to_vec(&prepared.numerical).map_err(crate::json_error)?.len()>crate::expectancy::MAX_NUMERICAL_BYTES
        || !prepared.numerical.is_object() { return Err(invalid()); }
    let cutoff=prepared.snapshot["observationCutoff"].as_i64().filter(|n|*n>=0).ok_or_else(invalid)?;
    let source=prepared.snapshot["source"].as_str().ok_or_else(invalid)?;
    if !matches!(source,"initial"|"actual") { return Err(invalid()); }
    let valid:bool=sqlx::query_scalar("select exists(select 1 from optimization_studies where id=$1 and owner_id=$2 and status='completed' and md5(spec::text||coalesce(result::text,'null'))=$3)")
        .bind(prepared.study).bind(&prepared.owner).bind(&prepared.source_identity).fetch_one(&mut **tx).await?;
    if !valid { return Ok(false); }
    sqlx::query("select pg_advisory_xact_lock($1)").bind(STORAGE_LOCK).execute(&mut **tx).await?;
    let previous:Option<Value>=sqlx::query_scalar("select snapshot from study_current_presentations where study_id=$1 and owner_id=$2 for update")
        .bind(prepared.study).bind(&prepared.owner).fetch_optional(&mut **tx).await?.flatten();
    if let Some(previous)=&previous {
        if source=="initial" { return Ok(false); }
        if previous["source"]=="actual" && previous["sourceRevision"]==prepared.snapshot["sourceRevision"] {
            let same:bool=sqlx::query_scalar("select numerical=$3 and png=$4 and source_identity=$5 from study_current_presentations where study_id=$1 and owner_id=$2")
                .bind(prepared.study).bind(&prepared.owner).bind(&prepared.numerical).bind(&prepared.png).bind(&prepared.source_identity).fetch_one(&mut **tx).await?;
            return if same {Ok(true)} else {Err(DbError::Json("study_presentation_publication_conflict".into()))};
        }
        if previous["observationCutoff"].as_i64().is_none_or(|old|old>cutoff)
            || previous["sourceRevision"].as_i64().zip(prepared.snapshot["sourceRevision"].as_i64()).is_some_and(|(old,new)|old>=new) { return Ok(false); }
    }
    let bytes=(serde_json::to_vec(&prepared.numerical).map_err(crate::json_error)?.len()+prepared.png.len()+32768) as i64;
    let (count,total):(i64,i64)=sqlx::query_as("select count(*),coalesce(sum(coalesce(octet_length(numerical::text),0)+coalesce(octet_length(png),0)+32768),0)::bigint from study_current_presentations where study_id<>$1")
        .bind(prepared.study).fetch_one(&mut **tx).await?;
    if count>=MAX_ROWS || total.saturating_add(bytes)>MAX_TOTAL_BYTES { return Err(DbError::Json("study_presentation_storage_limit".into())); }
    let revision=previous.as_ref().and_then(|p|p["revision"].as_i64()).unwrap_or(0).checked_add(1).ok_or_else(invalid)?;
    let hash=format!("{:x}",Sha256::digest(&prepared.png));
    let mut snapshot=prepared.snapshot.clone();snapshot["revision"]=json!(revision);snapshot["pngSha256"]=json!(hash);
    let published:chrono::DateTime<chrono::Utc>=sqlx::query_scalar("select clock_timestamp()").fetch_one(&mut **tx).await?;
    snapshot["publishedAt"]=json!(published);
    sqlx::query("insert into study_current_presentations(study_id,owner_id,source_identity,snapshot,numerical,png,png_sha256) values($1,$2,$3,$4,$5,$6,$7) on conflict(study_id) do update set source_identity=excluded.source_identity,snapshot=excluded.snapshot,numerical=excluded.numerical,png=excluded.png,png_sha256=excluded.png_sha256,error=null")
        .bind(prepared.study).bind(&prepared.owner).bind(&prepared.source_identity).bind(snapshot).bind(&prepared.numerical).bind(&prepared.png).bind(hash).execute(&mut **tx).await?;
    Ok(true)
}

pub async fn publish_initial(pool:&PgPool, prepared:&Prepared)->Result<bool,DbError> {
    let mut tx=pool.begin().await?;crate::continuous::owner_lock(&mut tx,&prepared.owner).await?;
    if prepared.snapshot["source"]=="actual" {
        let current:bool=sqlx::query_scalar("select exists(select 1 from study_actualizations where owner_id=$1 and study_id=$2 and status='completed' and revision=$3 and observed_through_unix=$4 and source_identity=$5)")
            .bind(&prepared.owner).bind(prepared.study).bind(prepared.snapshot["sourceRevision"].as_i64()).bind(prepared.snapshot["observationCutoff"].as_i64()).bind(&prepared.source_identity).fetch_one(&mut *tx).await?;
        if !current {return Ok(false);}
    }
    let result=publish_in(&mut tx,prepared).await?;tx.commit().await?;Ok(result)
}
pub async fn initialization_failed(pool:&PgPool,owner:&str,study:i64,identity:&str,code:&str)->Result<(),DbError> {
    if code.len()>128 {return Err(DbError::Json("study_presentation_invalid_reason".into()));}
    sqlx::query("insert into study_current_presentations(study_id,owner_id,source_identity,error) select id,owner_id,$3,$4 from optimization_studies where id=$1 and owner_id=$2 and md5(spec::text||coalesce(result::text,'null'))=$3 on conflict(study_id) do nothing")
        .bind(study).bind(owner).bind(identity).bind(code).execute(pool).await?;Ok(())
}

/// One tiny candidate per renderer iteration, not a history/result preload.
pub async fn next_initial(pool:&PgPool)->Result<Option<(i64,String,String)>,DbError> {
    Ok(sqlx::query_as("select s.id,s.owner_id,md5(s.spec::text||coalesce(s.result::text,'null')) from optimization_studies s where s.status='completed' and jsonb_typeof(s.result->'best')='object' and coalesce(s.result#>'{best,partial}','false')<>'true'::jsonb and (select count(*) from study_current_presentations)<8192 and not exists(select 1 from study_current_presentations p where p.study_id=s.id) and (s.origin='manual' or exists(select 1 from continuous_generations g where g.id=s.continuous_generation_id and g.status='completed')) order by s.id limit 1")
        .fetch_optional(pool).await?)
}

pub async fn active_initial_page(pool:&PgPool,after:i64,through:i64)->Result<Vec<(i64,String,String)>,DbError> {
    Ok(sqlx::query_as("select s.id,s.owner_id,md5(s.spec::text||coalesce(s.result::text,'null')) from optimization_studies s join continuous_generations g on g.id=s.continuous_generation_id and g.owner_id=s.owner_id where s.id>$1 and s.id<=$2 and s.status='completed' and g.status='completed' and (exists(select 1 from continuous_slots b where b.owner_id=s.owner_id and (b.active_generation_id=g.id or b.pending_generation_id=g.id)) or exists(select 1 from adaptive_profiles p where p.owner_id=s.owner_id and p.accepted_generation_id=g.id) or exists(select 1 from adaptive_profile_handoffs h where h.generation_id=g.id and h.status='pending')) order by s.id limit 16")
        .bind(after).bind(through).fetch_all(pool).await?)
}

/// New Telegram admissions copy this row in the live transaction. No pointer
/// survives a refresh; base64 bytes are owned by the bounded existing outbox.
pub async fn freeze_in(tx:&mut Transaction<'_,Postgres>,owner:&str,study:i64,generation:i64,recommendation:&Value,_raw:i64)->Result<Value,DbError> {
    let row=sqlx::query("select p.snapshot,p.png,p.png_sha256,p.source_identity=md5(s.spec::text||coalesce(s.result::text,'null')) same_source from study_current_presentations p join optimization_studies s on s.id=p.study_id and s.owner_id=p.owner_id where p.owner_id=$1 and p.study_id=$2 and s.continuous_generation_id=$3 and p.snapshot is not null")
        .bind(owner).bind(study).bind(generation).fetch_optional(&mut **tx).await?;
    let Some(row)=row else {return Ok(unavailable("study_presentation_unavailable"));};
    let snapshot:Value=row.try_get("snapshot")?;
    let expected: setups_domain::AdaptiveRecommendationSnapshot=serde_json::from_value(recommendation.clone()).map_err(crate::json_error)?;
    let objective=setups_domain::expectancy::ExpectancyObjective::from_recommendation(&expected).map_err(|e|DbError::Json(e.to_string()))?;
    if !row.try_get::<bool,_>("same_source")? || snapshot["tpPct"].as_f64()!=Some(expected.tp_pct) || snapshot["slPct"].as_f64()!=Some(expected.sl_pct)
        || snapshot["objective"]!=json!(objective) || snapshot["originalSnapshotHash"]!=expected.provenance.snapshot_hash
        || snapshot["observationCutoff"].as_i64().is_none_or(|cutoff|cutoff<expected.provenance.observation_cutoff) {
        return Ok(unavailable("study_presentation_identity_mismatch"));
    }
    let png:Vec<u8>=row.try_get("png")?;let hash:String=row.try_get("png_sha256")?;
    if png.len()>crate::expectancy::MAX_PNG_BYTES || format!("{:x}",Sha256::digest(&png))!=hash {
        crate::diagnostics::emit(crate::diagnostics::Level::Warn,"study_presentation_optional_unavailable",json!({"study_id":study,"generation_id":generation,"code":"study_presentation_png_corrupt"}));
        return Ok(unavailable("study_presentation_png_corrupt"));
    }
    Ok(json!({"status":"available","snapshot":snapshot,"pngBase64":STANDARD.encode(png),"pngSha256":hash}))
}
