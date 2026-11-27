//! Durable current study presentation owner. Initialization reads saved sources
//! only; no market/replay or rendering is performed by an HTTP GET or sender.
use crate::study_actual::model::{self,Source};
use serde_json::{Value,json};
use setups_db::{study_display::{self,DisplayDay},study_presentation::{self as db,Prepared}};
use setups_domain::expectancy::*;
use sqlx::{PgPool,Row};

fn fail(_:impl std::fmt::Display)->&'static str {"study_presentation_source_unavailable"}

pub(crate) async fn original(pool:&PgPool,owner:&str,study:i64,source:&Source)->Result<(ExpectancyArtifact,Vec<DisplayDay>,String),&'static str> {
    let observed=study_display::original_observed(pool,owner,study,source.source_artifact_id,"UTC").await.map_err(fail)?;
    if let Some(artifact)=&source.artifact {
        artifact.validate().map_err(fail)?;
        let days=match observed {
            Some(observed)=>observed.daily,
            None=>artifact.daily.as_ref().ok_or("original_series_unavailable")?.iter().map(|p|DisplayDay {
                day_unix:p.day_start_unix,date:p.date.clone(),sum_r:p.mean_r.unwrap_or(0.0)*(p.tp_count+p.sl_count) as f64,
                mean_r:p.mean_r,closed_count:p.tp_count+p.sl_count,tp_count:p.tp_count,sl_count:p.sl_count,
            }).collect(),
        };
        return Ok((artifact.clone(),days,source.source_identity.clone()));
    }
    // Manual archive is a real retained-study owner, not a synthetic generation.
    let observed=observed.ok_or("original_outcomes_not_persisted")?;
    let row=sqlx::query("select s.spec,s.result,o.source from optimization_studies s join study_manual_original_outcomes o on o.study_id=s.id and o.owner_id=s.owner_id where s.id=$1 and s.owner_id=$2")
        .bind(study).bind(owner).fetch_optional(pool).await.map_err(fail)?.ok_or("original_outcomes_not_persisted")?;
    let spec:Value=row.try_get("spec").map_err(fail)?;let result:Value=row.try_get("result").map_err(fail)?;
    let saved:Value=row.try_get("source").map_err(fail)?;
    if saved["tpPct"].as_f64()!=Some(source.tp_pct) || saved["slPct"].as_f64()!=Some(source.sl_pct) {return Err("original_pair_mismatch");}
    let cutoff=saved["observationCutoff"].as_i64().ok_or("original_cutoff_unavailable")?;
    let p=&result["best"]["provenance"];
    let tp=observed.daily.iter().map(|p|p.tp_count).sum::<u64>();let sl=observed.daily.iter().map(|p|p.sl_count).sum::<u64>();let closed=tp+sl;
    let scientific=match saved.get("modelDaily") {
        Some(value)=>serde_json::from_value::<Vec<UtcDailyInput>>(value.clone()).map_err(fail)?,
        None=>{
            if !source.objective.trend_mode.is_mean() && !matches!(source.objective.forecast,Some(setups_domain::forecast::ForecastRecipe::DecayedProbability{..}))
                && source.original_endpoint["counts"]["unclosed"].as_u64().unwrap_or(0)+source.original_endpoint["counts"]["noData"].as_u64().unwrap_or(0)>0 {
                return Err("manual_original_weights_unavailable");
            }
            observed.daily.iter().filter(|p|p.closed_count>0).map(|p|UtcDailyInput{day_start_unix:p.day_unix,row_count:p.closed_count,tp_count:p.tp_count,sl_count:p.sl_count,mean_r:p.mean_r}).collect()
        },
    };
    let artifact=build_expectancy_artifact(ExpectancyInput{
        source:ExpectancySource{study_id:study.to_string(),derived_snapshot_id:format!("current-manual-{study}-{}",source.source_identity),generation_id:None,profile_id:None,
            provenance:setups_domain::RecommendationProvenance {algorithm_id:source.algorithm_id.clone(),algorithm_version:source.algorithm_version.clone(),
                optimizer_version:p["optimizerVersion"].as_str().ok_or("original_provenance_invalid")?.into(),seed:p["seed"].as_u64().ok_or("original_provenance_invalid")?,
                snapshot_hash:p["snapshotHash"].as_str().ok_or("original_provenance_invalid")?.into(),captured_at:p["capturedAt"].as_str().ok_or("original_provenance_invalid")?.into(),
                from_unix:spec["fromUnix"].as_i64().ok_or("original_period_invalid")?,to_unix:spec["toUnix"].as_i64().ok_or("original_period_invalid")?,observation_cutoff:cutoff},
            direction:serde_json::from_value(json!(source.direction)).map_err(fail)?,tp_pct:source.tp_pct,sl_pct:source.sl_pct,forecast:source.objective.forecast,
            analytics:ExpectancyAnalytics{cluster_enabled:source.analytics.cluster_enabled,cluster_gap_minutes:source.analytics.cluster_gap_minutes,time_shift_minutes:source.analytics.time_shift_minutes,
                price_change_pct:source.analytics.price_change_pct,horizon_minutes:source.analytics.horizon_minutes}},
        selected_cell:Some(SelectedCellInput{raw_r:(closed>0).then(||(tp as f64*source.tp_pct/source.sl_pct-sl as f64)/closed as f64),tp_count:tp,sl_count:sl}),
        matrix_mean_r:None,daily:MetricAvailability::Available{value:scientific},
    }).map_err(fail)?;
    Ok((artifact,observed.daily,source.source_identity.clone()))
}

pub(crate) fn prepare(
    owner:&str,study:i64,source:&Source,original:&ExpectancyArtifact,observed:Vec<DisplayDay>,
    actual:&[UtcDailyInput],actual_observed:Vec<DisplayDay>,cutoff:i64,revision:Option<i64>,actual_counts:Option<&Value>,
) -> Result<Prepared,&'static str> {
    let artifact=if revision.is_some() {crate::expectancy_projection::combined_artifact(original,source.objective,actual,cutoff)?}else{original.clone()};
    if artifact.source.provenance.observation_cutoff!=cutoff {return Err("study_presentation_cutoff_mismatch");}
    let observed=study_display::merge_days(observed,actual_observed).map_err(fail)?;
    let points=observed.iter().filter(|p|p.closed_count>0).map(|p|UtcDailyPoint{date:p.date.clone(),day_start_unix:p.day_unix,
        x_utc_days:utc_day_coordinate(artifact.origin_unix,p.day_unix),row_count:p.closed_count,tp_count:p.tp_count,sl_count:p.sl_count,mean_r:p.mean_r}).collect::<Vec<_>>();
    let series=artifact.objective_series(source.objective).map_err(fail)?;
    let cell=artifact.selected_cell.as_ref().ok_or("study_presentation_metrics_unavailable")?;
    let (tp,sl)=observed.iter().try_fold((0u64,0u64),|(tp,sl),p|Some((tp.checked_add(p.tp_count)?,sl.checked_add(p.sl_count)?))).ok_or("combined_count_overflow")?;
    if (tp,sl)!=(cell.tp_count,cell.sl_count) {return Err("study_presentation_count_mismatch");}
    let decision=series.decision_r.as_ref().copied().ok_or("study_presentation_objective_unavailable")?;
    let png=setups_analytics::expectancy_chart::render_png_current(&artifact,source.objective,&points).map_err(|e|e.code())?;
    let numerical=setups_db::expectancy::encode_artifact(&artifact).map_err(fail)?;
    let counts=source.original_endpoint.get("counts").filter(|c|c.is_object()).map(|original| {
        let mut counts=original.clone();
        for key in ["total","closed","tp","sl","gateFailed","unclosed","noData"] {
            counts[key]=json!(original[key].as_u64().unwrap_or(0)+actual_counts.and_then(|a|a[key].as_u64()).unwrap_or(0));
        }
        counts
    });
    Ok(Prepared{owner:owner.into(),study,source_identity:source.source_identity.clone(),numerical,png,
        snapshot:json!({"revision":0,"source":if revision.is_some(){"actual"}else{"initial"},"sourceRevision":revision,
            "studyId":study,"generationId":original.source.generation_id.as_ref().and_then(|g|g.parse::<i64>().ok()),
            "observationCutoff":cutoff,"meanR":cell.raw_r.as_ref(),"decisionR":decision,"selectedObjectiveR":decision,"forecastR":series.forecast_r.as_ref(),
            "winRate":cell.win_rate_pct.as_ref(),"counts":counts,"closedCount":cell.closed_count,"tpCount":tp,"slCount":sl,
            "objective":source.objective,"tpPct":source.tp_pct,"slPct":source.sl_pct,"originalSnapshotHash":original.source.provenance.snapshot_hash,
            "sourceIdentity":source.source_identity,"sourceArtifactId":source.source_artifact_id,"renderVersion":setups_analytics::expectancy_chart::CURRENT_RENDER_VERSION})})
}

pub(crate) async fn render_owned<T>(pool:&PgPool,render:impl FnOnce()->Result<T,&'static str>)->Result<T,&'static str> {
    let mut lane=pool.begin().await.map_err(fail)?;
    sqlx::query("select pg_advisory_xact_lock($1)").bind(db::RENDER_LOCK).execute(&mut *lane).await.map_err(fail)?;
    // No nested checkout while the render lane owns this connection. There is
    // no asynchronous cancellation point inside the CPU/render closure.
    let result=if tokio::runtime::Handle::current().runtime_flavor()==tokio::runtime::RuntimeFlavor::MultiThread {tokio::task::block_in_place(render)}else{render()};
    lane.commit().await.map_err(fail)?;
    result
}

/// Uses the existing single asset lane's connection, not a second pool checkout
/// while holding a transaction. Synchronous bounded render cannot detach on job cancellation.
pub(crate) async fn initialize_next(pool:&PgPool)->Result<bool,setups_db::DbError> {
    let Some((study,owner,identity))=db::next_initial(pool).await? else{return Ok(false);};
    initialize_study(pool,&owner,study,&identity).await?;
    Ok(true)
}

pub(crate) async fn initialize_study(pool:&PgPool,owner:&str,study:i64,identity:&str)->Result<(),setups_db::DbError> {
    if db::summary(pool,owner,study).await?["status"]=="available" {return Ok(());}
    let result=async {
        let source=model::original_source(pool,study,owner).await?;
        let (original,observed,_)=original(pool,owner,study,&source).await?;
        let cutoff=original.source.provenance.observation_cutoff;
        // Legacy initialization may already have a successful post-start
        // publication. Consume its saved cohorts, never trigger a new replay.
        let saved=study_display::display_snapshot(pool,owner,study,"UTC",Some(source.execution_started_at),source.source_artifact_id,Some(&source.source_identity)).await.map_err(fail)?;
        let published=saved.actual_row.as_ref().filter(|r|r["status"]=="completed");
        let prepared=if let Some(row)=published {
            let cutoff=row["observed_through_unix"].as_i64().ok_or("post_study_identity_unavailable")?;
            let revision=row["revision"].as_i64().ok_or("post_study_identity_unavailable")?;
            let facts=&row["sourceFacts"];
            if row["source_identity"]!=source.source_identity || facts["tpPct"].as_f64()!=Some(source.tp_pct)
                || facts["slPct"].as_f64()!=Some(source.sl_pct) || facts["objective"]!=json!(source.objective) {return Err("post_study_source_mismatch");}
            render_owned(pool,||prepare(owner,study,&source,&original,vec![],&saved.actual_utc_daily,saved.combined_daily,cutoff,Some(revision),Some(&row["totals"]))).await?
        }else{
            render_owned(pool,||prepare(owner,study,&source,&original,observed,&[],vec![],cutoff,None,None)).await?
        };
        if !db::publish_initial(pool,&prepared).await.map_err(fail)? && db::summary(pool,owner,study).await.map_err(fail)?["status"]!="available" {
            return Err("study_presentation_source_unavailable");
        }
        Ok::<_,&'static str>(())
    }.await;
    if let Err(code)=result {
        // Transient pool/SQL outages must not become permanent unavailable
        // metadata. Retry this saved source on the next bounded renderer tick.
        if code=="study_actual_storage_unavailable" || code=="study_presentation_source_unavailable" {return Err(setups_db::DbError::Json(code.into()));}
        db::initialization_failed(pool,owner,study,identity,code).await?;
        eprintln!("study_presentation event=initial_unavailable study_id={study} code={code}");
    }
    Ok(())
}

/// Before starting live on upgrade, initialize only active/pending bindings.
/// Fixed high-water + 16 IDs/page, no market work or full history backlog. New
/// generations initialize before finalize/handoff in continuous::runtime.
pub async fn initialize_active_presentations(pool:&PgPool)->Result<(),setups_db::DbError> {
    let through:i64=sqlx::query_scalar("select coalesce(max(id),0) from optimization_studies").fetch_one(pool).await?;
    let mut after=0;
    loop {
        let page=db::active_initial_page(pool,after,through).await?;
        if page.is_empty() {return Ok(());}
        for (study,owner,identity) in page {
            after=study;
            initialize_study(pool,&owner,study,&identity).await?;
        }
    }
}

#[cfg(test)]
#[path="study_presentation_tests.rs"]
mod tests;
