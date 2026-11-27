use super::*;
use axum::{body::Body,http::{Request,StatusCode}};
use futures::FutureExt;
use http_body_util::BodyExt;
use setups_db::{optimization,study_actual as actual};
use std::sync::{Arc,atomic::AtomicBool};
use tower::ServiceExt;
const DAY:i64=86400;
const START:i64=1_795_478_400; // fixed UTC midnight, not render/view time

async fn isolated(body:impl AsyncFnOnce(PgPool)) {
    let Ok(url)=std::env::var("TEST_DATABASE_URL")else{return;};
    let options:sqlx::postgres::PgConnectOptions=url.parse().unwrap();
    assert!(options.get_database().unwrap_or("").ends_with("_test"));
    assert!(matches!(options.get_host(),"localhost"|"127.0.0.1"|"::1"));
    let admin=sqlx::postgres::PgPoolOptions::new().max_connections(1).connect_with(options.clone()).await.unwrap();
    let name=format!("current_api_{}_{}_test",std::process::id(),chrono::Utc::now().timestamp_nanos_opt().unwrap());
    sqlx::query(&format!("create database {name}")).execute(&admin).await.unwrap();
    // Two connections are enough even for full initialization; no hidden
    // nested checkout is allowed inside a held render/admission transaction.
    let pool=sqlx::postgres::PgPoolOptions::new().max_connections(2).acquire_timeout(std::time::Duration::from_secs(2)).connect_with(options.database(&name)).await.unwrap();
    let child=pool.clone();
    let result=std::panic::AssertUnwindSafe(async move {
        let baseline=include_str!("../../db/migrations/0010_baseline.sql").lines().filter(|line|!line.starts_with('\\')&&!line.starts_with("SET transaction_timeout")&&!line.contains("set_config('search_path'")).collect::<Vec<_>>().join("\n");
        sqlx::raw_sql(&baseline).execute(&child).await.unwrap();
        sqlx::raw_sql("set search_path=public;create table kysely_migration(name varchar(255) primary key,timestamp varchar(255) not null);insert into kysely_migration values('0009_legacy','fixture')").execute(&child).await.unwrap();
        setups_db::migrate::ensure_migrations(&child).await.unwrap();
        assert!(setups_db::migrate::ensure_migrations(&child).await.unwrap().starts_with("0 applied"));
        body(child).await;
    }).catch_unwind().await;
    pool.close().await;sqlx::query(&format!("drop database {name} with(force)")).execute(&admin).await.unwrap();admin.close().await;
    if let Err(panic)=result{std::panic::resume_unwind(panic);}
}

async fn manual(pool:&PgPool,owner:&str)->(i64,String) {
    let params=json!(setups_domain::SpikeParams::default());
    let spec=json!({"algorithmId":"price_volume_spike","algorithmVersion":"1.1.1","algorithmParams":params,
        "analyticsParams":{"clusterEnabled":false,"clusterGapMinutes":480,"timeShiftMinutes":1,"priceChangePct":null},
        "instruments":[{"exchange":"fixture","category":"linear","symbol":"FIX"}],"fromUnix":START-2*DAY,"toUnix":START,
        "direction":"short","seed":7,"tpLevels":[8.0],"slLevels":[4.0],"minClosedPerCell":1});
    let result=json!({"best":{"score":1.0,"partial":false,"params":params,"analytics":{"clusterEnabled":false,"clusterGapMinutes":null,"timeShiftMinutes":1,"priceChangePct":null,"setupMaturity":null,"horizonMinutes":10080},
        "matrix":[{"tpPct":8.0,"slPct":4.0,"riskExpectancy":1.0,"avgMovePct":4.0,"closedCount":3,"tpCount":2,"slCount":1,"unclosedCount":1,"noDataCount":0,"priceChangeFilteredCount":0}],
        "provenance":{"optimizerVersion":"plan-a-3","seed":7,"snapshotHash":"immutable-manual-fixture","capturedAt":"2026-11-20T00:00:00Z"},"conclusions":[],
        "support":{"score":1.0,"setupCount":4,"selectedSetupCount":4,"evaluatedMinutes":1000,"warmupMinutes":1440},"trialIndex":0}});
    let id:i64=sqlx::query_scalar("insert into optimization_studies(owner_id,algorithm_id,status,spec,progress,result,execution_started_at) values($1,'price_volume_spike','completed',$2,'{}',$4,to_timestamp($3)) returning id")
        .bind(owner).bind(spec).bind(START as f64).bind(&result).fetch_one(pool).await.unwrap();
    let archive=study_display::ManualArchive {observation_cutoff:START,tp_pct:8.0,sl_pct:4.0,kernel:"original-manual-close-1".into(),
        model_daily:Some(vec![UtcDailyInput{day_start_unix:START-2*DAY,row_count:1,tp_count:1,sl_count:0,mean_r:Some(2.0)},
            UtcDailyInput{day_start_unix:START-DAY,row_count:3,tp_count:1,sl_count:1,mean_r:Some(0.5)}]),
        minutes:vec![study_display::OriginalMinute{signal_close_unix:START-2*DAY+60,sum_r:2.0,closed_count:1,tp_count:1,sl_count:0},
            study_display::OriginalMinute{signal_close_unix:START-DAY+60,sum_r:2.0,closed_count:1,tp_count:1,sl_count:0},
            study_display::OriginalMinute{signal_close_unix:START-DAY+120,sum_r:-1.0,closed_count:1,tp_count:0,sl_count:1}]};
    optimization::finish_with_original_archive(pool,id,"completed",&json!({}),Some(&result),None,&json!([]),Some(&archive)).await.unwrap();
    let identity=sqlx::query_scalar("select md5(spec::text||coalesce(result::text,'null')) from optimization_studies where id=$1").bind(id).fetch_one(pool).await.unwrap();
    (id,identity)
}

fn router(pool:&PgPool)->axum::Router {
    crate::optimization::optimization_router(Arc::new(crate::optimization::OptimizationState{pool:pool.clone(),auth:Arc::new(crate::SessionAuth::new(&["fixture-key".into()])),config:Default::default(),notify:Arc::new(tokio::sync::Notify::new()),available:Arc::new(AtomicBool::new(true))}))
}
async fn get(app:&axum::Router,path:&str,auth:bool)->(StatusCode,axum::http::HeaderMap,Vec<u8>) {
    let mut request=Request::builder().uri(path);if auth{request=request.header("x-api-key","fixture-key");}
    let response=app.clone().oneshot(request.body(Body::empty()).unwrap()).await.unwrap();
    let status=response.status();let headers=response.headers().clone();let body=response.into_body().collect().await.unwrap().to_bytes().to_vec();(status,headers,body)
}
async fn json_get(app:&axum::Router,path:&str)->Value{let(status,_,body)=get(app,path,true).await;assert_eq!(status,StatusCode::OK);serde_json::from_slice(&body).unwrap()}

async fn continuous_binding(pool:&PgPool,slot:i64)->(i64,i64) {
    use setups_db::expectancy::{self,ArtifactIdentity};
    let(study,_)=manual(pool,"admin").await;
    let source=model::original_source(pool,study,"admin").await.unwrap();
    let(mut artifact,_,_)=original(pool,"admin",study,&source).await.unwrap();
    let(spec,result):(Value,Value)=sqlx::query_as("select spec,result from optimization_studies where id=$1").bind(study).fetch_one(pool).await.unwrap();
    let generation:i64=sqlx::query_scalar("insert into continuous_generations(owner_id,algorithm_id,slot_id,config_revision,lease_token,status,phase,spec,execution_started_at,observation_cutoff,study_id,accepted) values('admin','price_volume_spike',$1,1,1,'completed','completed',$2,to_timestamp($3),$3,$4,true) returning id")
        .bind(slot).bind(&spec).bind(START).bind(study).fetch_one(pool).await.unwrap();
    sqlx::query("update optimization_studies set origin='continuous',continuous_generation_id=$2,continuous_slot_id=$3 where id=$1").bind(study).bind(generation).bind(slot).execute(pool).await.unwrap();
    artifact.source.generation_id=Some(generation.to_string());artifact.source.profile_id=Some(slot.to_string());
    let identity=ArtifactIdentity{owner_id:"admin".into(),generation_id:generation,study_id:study,slot_id:slot,
        calculation_version:artifact.calculation_version.to_string(),tp_pct:8.0,sl_pct:4.0,from_unix:artifact.from_unix,to_unix:artifact.to_unix,
        observation_cutoff:START,provenance:json!(artifact.source.provenance),effective_analytics:json!(source.analytics),historical_input_hash:None,forecast:None,outcome_hash:None};
    let key=expectancy::artifact_key(&identity).unwrap();artifact.source.derived_snapshot_id=key.clone();
    let numerical=expectancy::encode_artifact(&artifact).unwrap();
    let id:i64=sqlx::query_scalar("insert into study_expectancy_artifacts(artifact_key,owner_id,source_generation_id,source_study_id,source_slot_id,calculation_version,tp_pct,sl_pct,identity,numerical_snapshot) values($1,'admin',$2,$3,$4,$5,8,4,$6,$7) returning id")
        .bind(key).bind(generation).bind(study).bind(slot).bind(artifact.calculation_version.to_string()).bind(json!(identity)).bind(numerical).fetch_one(pool).await.unwrap();
    let recommendation=json!({"direction":"short","tpPct":8.0,"slPct":4.0,"rawR":1.0,"meanR":1.0,"robustScore":1.0,
        "clusterEnabled":false,"clusterGapMinutes":null,"timeShiftMinutes":1,"priceChangePct":null,"provenance":artifact.source.provenance});
    sqlx::query("update continuous_generations set expectancy_artifact_id=$2,result=$3 where id=$1")
        .bind(generation).bind(id).bind(json!({"best":result["best"],"recommendation":recommendation,"meanExpectancy":1.0,"selectedCell":{"tpPct":8.0,"slPct":4.0,"rawR":1.0,"meanR":1.0,"robustScore":1.0}})).execute(pool).await.unwrap();
    // This fixture proves the permitted saved UTC source; no synthetic manual
    // ledger is borrowed after changing the owner type to a real generation.
    sqlx::query("delete from study_manual_original_outcomes where study_id=$1").bind(study).execute(pool).await.unwrap();
    (study,generation)
}

#[tokio::test]
async fn startup_initializes_standalone_accepted_and_incumbent_before_inactive_backlog() {
    isolated(async |pool| {
        for _ in 0..20 {sqlx::query("insert into optimization_studies(algorithm_id,status,spec,progress,result) values('price_volume_spike','completed','{}','{}','{\"best\":{}}')").execute(&pool).await.unwrap();}
        setups_db::continuous::configure(&pool,"admin","price_volume_spike",&json!({"adaptiveEnabled":true}),&[0.0]).await.unwrap();
        setups_db::continuous::profiles::set_permission(&pool,"admin","price_volume_spike",true).await.unwrap();
        let slot:i64=sqlx::query_scalar("select id from continuous_slots where owner_id='admin' limit 1").fetch_one(&pool).await.unwrap();
        let(old,incumbent)=continuous_binding(&pool,slot).await;
        let(new,accepted)=continuous_binding(&pool,slot).await;
        sqlx::query("update continuous_slots set active_generation_id=$2,pending_generation_id=null where id=$1").bind(slot).bind(incumbent).execute(&pool).await.unwrap();
        sqlx::query("insert into adaptive_profiles(id,owner_id,algorithm_id,min_setups_per_day,accepted_generation_id) values($1,'admin','price_volume_spike',0,$2)").bind(slot).bind(accepted).execute(&pool).await.unwrap();
        let page=db::active_initial_page(&pool,0,new).await.unwrap();assert_eq!(page.iter().map(|p|p.0).collect::<Vec<_>>(),vec![old,new]);
        initialize_active_presentations(&pool).await.unwrap();
        for(study,generation)in[(old,incumbent),(new,accepted)] {
            let saved=db::summary(&pool,"admin",study).await.unwrap();assert_eq!(saved["status"],"available","{saved}");assert_eq!(saved["snapshot"]["generationId"],generation);
        }
        assert_eq!(sqlx::query_scalar::<_,i64>("select count(*) from study_current_presentations").fetch_one(&pool).await.unwrap(),2,"startup does not drain unrelated history before live");
        // The first delayed setup can take the latest fixed snapshot even if
        // its raw-close is earlier than C. One transaction/connection suffices.
        let recommendation:Value=sqlx::query_scalar("select result->'recommendation' from continuous_generations where id=$1").bind(accepted).fetch_one(&pool).await.unwrap();
        let mut transaction=pool.begin().await.unwrap();
        let frozen=db::freeze_in(&mut transaction,"admin",new,accepted,&recommendation,START-60).await.unwrap();assert_eq!(frozen["status"],"available","{frozen}");
        assert_eq!(frozen["snapshot"]["observationCutoff"],START);transaction.commit().await.unwrap();
    }).await;
}

#[tokio::test]
async fn initial_actual_http_png_pair_owner_revision_and_legacy_recovery_are_saved_only() {
    isolated(async |pool| {
        let (id,identity)=manual(&pool,"admin").await;
        let before:Value=sqlx::query_scalar("select jsonb_build_object('spec',spec,'result',result) from optimization_studies where id=$1").bind(id).fetch_one(&pool).await.unwrap();
        initialize_study(&pool,"admin",id,&identity).await.unwrap();
        let initial=db::summary(&pool,"admin",id).await.unwrap();assert_eq!(initial["status"],"available","{initial}");
        assert_eq!(initial["snapshot"]["source"],"initial");assert_eq!(initial["snapshot"]["observationCutoff"],START);
        assert_eq!(initial["snapshot"]["counts"],json!({"total":4,"closed":3,"tp":2,"sl":1,"gateFailed":0,"unclosed":1,"noData":0}));
        assert_eq!(initial["snapshot"]["meanR"].as_f64(),Some(1.0));
        initialize_study(&pool,"admin",id,&identity).await.unwrap();assert_eq!(db::summary(&pool,"admin",id).await.unwrap(),initial,"initial publisher is idempotent");
        let app=router(&pool);let path=format!("/v1/optimization-studies/{id}");
        let detail=json_get(&app,&path).await;assert_eq!(detail["currentPresentation"],initial);
        let page=json_get(&app,"/v1/optimization-studies?limit=10").await;assert_eq!(page["studies"][0]["currentPresentation"],initial);
        let png_path=format!("{path}/presentation.png?revision=1");
        let(status,headers,png)=get(&app,&png_path,true).await;assert_eq!(status,StatusCode::OK);assert_eq!(headers["content-type"],"image/png");assert_eq!(headers["cache-control"],"no-store");
        assert_eq!(png.get(..8),Some(b"\x89PNG\r\n\x1a\n".as_slice()));assert!(png.len()<=256*1024);
        assert_eq!(&png[16..24],&[0,0,3,132,0,0,1,64]); // 900x320
        assert_eq!(get(&app,&png_path,false).await.0,StatusCode::UNAUTHORIZED);
        assert_eq!(get(&app,&format!("{path}/presentation.png?revision=0"),true).await.0,StatusCode::BAD_REQUEST);
        let source=model::original_source(&pool,id,"admin").await.unwrap();
        let(original,observed,_)=original(&pool,"admin",id,&source).await.unwrap();
        let admitted=actual::admit(&pool,actual::Request{owner:"admin",study:id,job:"current-test",start:source.execution_started_at,cutoff:START+DAY,
            source:&json!(source),identity:&identity,work:60,limits:&Default::default()}).await.unwrap();assert!(matches!(admitted,actual::Admission::Created(_)));
        assert_eq!(db::summary(&pool,"admin",id).await.unwrap(),initial,"queued refresh retains current success");
        let job=actual::claim(&pool,2).await.unwrap().unwrap();let batch=actual::create_batch(&pool,&job).await.unwrap().unwrap();
        assert!(actual::append(&pool,&job,batch,actual::InstrumentOutput{exchange:"fixture",category:"linear",symbol:"FIX",emitted:&json!([]),rows:&[json!({"setupTimeUnix":START+60,"status":"sl","riskReturn":-1.0})],quality:&json!({})}).await.unwrap());
        let additional=vec![UtcDailyInput{day_start_unix:START,row_count:1,tp_count:0,sl_count:1,mean_r:Some(-1.0)}];
        let actual_days=vec![DisplayDay{day_unix:START,date:utc_date(START).unwrap(),sum_r:-1.0,mean_r:Some(-1.0),closed_count:1,tp_count:0,sl_count:1}];
        let counts=json!({"total":1,"closed":1,"tp":0,"sl":1,"gateFailed":0,"unclosed":0,"noData":0,"meanR":-1.0});
        let prepared=prepare("admin",id,&source,&original,observed,&additional,actual_days,job.cutoff,Some(job.revision),Some(&counts)).unwrap();
        let report=json!({"daily":[{"dayUnix":START,"meanR":-1.0,"closedCount":1,"tpCount":0,"slCount":1}]});
        assert!(actual::finish_with_presentation(&pool,&job,&report,&counts,Some(&prepared)).await.unwrap());
        let current=db::summary(&pool,"admin",id).await.unwrap();assert_eq!(current["snapshot"]["source"],"actual");assert_eq!(current["snapshot"]["revision"],2);
        assert_eq!(current["snapshot"]["counts"]["closed"],4);assert_eq!(current["snapshot"]["counts"]["tp"],2);assert_eq!(current["snapshot"]["counts"]["sl"],2);
        assert_eq!(current["snapshot"]["meanR"].as_f64(),Some(0.5));assert_eq!(current["snapshot"]["observationCutoff"],START+DAY);
        let(status,_,body)=get(&app,&png_path,true).await;assert_eq!(status,StatusCode::CONFLICT);assert_eq!(serde_json::from_slice::<Value>(&body).unwrap()["error"],"study_presentation_revision_changed");
        let(status,_,actual_png)=get(&app,&format!("{path}/presentation.png?revision=2"),true).await;assert_eq!(status,StatusCode::OK);assert_ne!(actual_png,png);
        let actual_detail=json_get(&app,&path).await;assert_eq!(actual_detail["currentPresentation"],current);
        let (other,other_identity)=manual(&pool,"other-owner").await;initialize_study(&pool,"other-owner",other,&other_identity).await.unwrap();
        assert_eq!(get(&app,&format!("/v1/optimization-studies/{other}/presentation.png?revision=1"),true).await.0,StatusCode::NOT_FOUND);
        // Simulate upgrading a legacy study which already has a successful
        // actual result, but no new current asset. Recover newest saved data.
        sqlx::query("delete from study_current_presentations where study_id=$1").bind(id).execute(&pool).await.unwrap();
        initialize_study(&pool,"admin",id,&identity).await.unwrap();let recovered=db::summary(&pool,"admin",id).await.unwrap();
        assert_eq!(recovered["snapshot"]["source"],"actual","{recovered}");assert_eq!(recovered["snapshot"]["sourceRevision"],job.revision);assert_eq!(recovered["snapshot"]["meanR"].as_f64(),Some(0.5));
        let after:Value=sqlx::query_scalar("select jsonb_build_object('spec',spec,'result',result) from optimization_studies where id=$1").bind(id).fetch_one(&pool).await.unwrap();assert_eq!(before,after);
        // Optional requested local evidence; generated bytes/DTOs only.
        if let Ok(directory)=std::env::var("STUDY_PRESENTATION_TEST_ARTIFACT_DIR") {
            let directory=std::path::Path::new(&directory);assert!(directory.is_dir());
            std::fs::write(directory.join("study-current-initial.png"),png).unwrap();std::fs::write(directory.join("study-current-actual.png"),actual_png).unwrap();
            std::fs::write(directory.join("study-current-initial.json"),serde_json::to_vec_pretty(&detail).unwrap()).unwrap();
            std::fs::write(directory.join("study-current-actual.json"),serde_json::to_vec_pretty(&actual_detail).unwrap()).unwrap();
        }
    }).await;
}
