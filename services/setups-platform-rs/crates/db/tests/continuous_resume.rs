//! Isolated contracts for private state, same-generation ownership, and maintenance.
use serde_json::{Value,json};
use setups_db::continuous::{self as db,scheduler,recovery,maintenance};
use sqlx::{PgPool,postgres::{PgConnectOptions,PgPoolOptions}};
use std::{str::FromStr,future::Future};
const OWNER:&str="admin";
const ALGORITHM:&str="price_volume_spike";

async fn isolated<F,Fut>(body:F) where F:FnOnce(PgPool)->Fut+'static,Fut:Future<Output=()>+'static {
    let options=PgConnectOptions::from_str(&std::env::var("TEST_DATABASE_URL").expect("isolated *_test URL required")).unwrap();
    assert!(options.get_database().unwrap().ends_with("_test"));
    let admin=PgPoolOptions::new().max_connections(2).connect_with(options.clone()).await.unwrap();
    let name=format!("continuous_resume_{}_{}_test",std::process::id(),chrono::Utc::now().timestamp_nanos_opt().unwrap());
    sqlx::query(&format!("create database {name}")).execute(&admin).await.unwrap();
    let pool=PgPoolOptions::new().max_connections(6).connect_with(options.database(&name)).await.unwrap();
    let child=pool.clone();
    let result=tokio::task::LocalSet::new().run_until(async move {tokio::task::spawn_local(async move {
        let baseline=include_str!("../migrations/0010_baseline.sql").lines().filter(|line|!line.starts_with('\\')&&!line.starts_with("SET transaction_timeout")&&!line.contains("set_config('search_path'")).collect::<Vec<_>>().join("\n");
        sqlx::raw_sql(&baseline).execute(&child).await.unwrap();
        sqlx::raw_sql("set search_path to public;create table kysely_migration(name varchar(255) primary key,timestamp varchar(255) not null);insert into kysely_migration values('0009_legacy','legacy')").execute(&child).await.unwrap();
        setups_db::migrate::ensure_migrations(&child).await.unwrap();
        body(child).await;
    }).await}).await;
    pool.close().await;
    sqlx::query(&format!("drop database {name} with(force)")).execute(&admin).await.unwrap();admin.close().await;
    if let Err(error)=result {std::panic::resume_unwind(error.into_panic());}
}
async fn job(pool:&PgPool)->scheduler::ChainJob {
    let config=json!({"adaptiveEnabled":true,"lookbackDays":1,"minSetupsPerDay":[0],"minMeanExpectancy":0.15,"study":{"algorithmId":ALGORITHM,"algorithmVersion":"1.1.1","maxSeconds":3600,"maxTrials":8,"seed":42}});
    assert!(db::configure(pool,OWNER,ALGORITHM,&config,&[0.0]).await.unwrap());
    assert!(db::start(pool,OWNER,ALGORITHM).await.unwrap());
    let lease=scheduler::claim_owner(pool,"first-executor").await.unwrap().unwrap();
    scheduler::prepare_next(pool,&lease).await.unwrap().unwrap()
}
fn identity()->Value {json!({"version":1,"requestHash":"fixture"})}
async fn snapshot(pool:&PgPool,job:&scheduler::ChainJob) {
    sqlx::query("insert into continuous_runtime_artifacts(generation_id,identity,reserved_bytes,snapshot_manifest,snapshot_bytes) values($1,$2,1048576,'{}',1)").bind(job.generation_id).bind(identity()).execute(pool).await.unwrap();
    sqlx::query("insert into continuous_frozen_chunks values($1,0,decode('01','hex'),repeat('0',64))").bind(job.generation_id).execute(pool).await.unwrap();
}
async fn expire(pool:&PgPool) {sqlx::query("update continuous_owner_lanes set lease_until=now()-interval '1 second'").execute(pool).await.unwrap();}

#[tokio::test]
#[ignore="requires isolated TEST_DATABASE_URL ending _test with CREATEDB"]
async fn maintenance_drain_recovers_same_generation_then_finishes_without_start_or_retry_reset() {
    isolated(|pool|async move {
        assert_eq!(maintenance::status(&pool,OWNER).await.unwrap().state,"running");
        let old=job(&pool).await;snapshot(&pool,&old).await;
        recovery::save_checkpoint(&pool,&old,&identity(),b"first-boundary",2*1024*1024).await.unwrap();
        sqlx::query("update continuous_processes set retry_count=2").execute(&pool).await.unwrap();
        let drained=maintenance::set_paused(&pool,OWNER,true).await.unwrap();
        assert_eq!(drained.state,"draining");assert_eq!(drained.active_chains,1);assert_eq!(drained.resumable_chains,1);
        assert_eq!(maintenance::set_paused(&pool,OWNER,true).await.unwrap().updated_at,drained.updated_at);
        assert!(scheduler::prepare_next(&pool,&old.lease).await.unwrap().is_none());
        expire(&pool).await;
        let lease=scheduler::claim_owner(&pool,"replacement").await.unwrap().expect("active recovery must be admitted under drain");
        scheduler::recover_owner(&pool,&lease).await.unwrap();
        let same=recovery::active_job(&pool,&lease).await.unwrap().unwrap();
        assert_eq!((same.generation_id,same.study_id,same.observation_cutoff), (old.generation_id,old.study_id,old.observation_cutoff));
        assert_eq!(same.research_deadline.unwrap().timestamp_micros(),old.research_deadline.unwrap().timestamp_micros());
        assert_eq!(same.execution_started_at.timestamp_micros(),old.execution_started_at.timestamp_micros());
        assert_eq!(same.spec,old.spec);
        assert_eq!(sqlx::query_scalar::<_,i32>("select retry_count from continuous_processes").fetch_one(&pool).await.unwrap(),3,"abandoned executor charges the third recovery at claim, which must still run");
        assert!(recovery::save_checkpoint(&pool,&old,&identity(),b"stale",2*1024*1024).await.is_err());
        let restored=recovery::load(&pool,&same,&identity()).await.unwrap();assert_eq!(restored.checkpoint.unwrap(),b"first-boundary");
        scheduler::finish_search(&pool,&same,&json!({"progress":{},"best":null}),true).await.unwrap();
        let gate=recovery::Gate {result:json!({"best":null,"reason":"no_winner"}),accepted:false,mean:None,selected:None};
        assert!(recovery::save_gate(&pool,&same,&identity(),&gate).await.unwrap());
        assert!(scheduler::finalize(&pool,&same,&gate.result,false,None,None).await.unwrap());
        assert!(scheduler::finalize(&pool,&same,&gate.result,false,None,None).await.unwrap(),"exact lost-response retry is idempotent");
        assert_eq!(maintenance::status(&pool,OWNER).await.unwrap().state,"drained");
        assert!(scheduler::claim_owner(&pool,"drained").await.unwrap().is_none());
        assert_eq!(sqlx::query_scalar::<_,i64>("select count(*) from continuous_runtime_artifacts").fetch_one(&pool).await.unwrap(),0);
        assert_eq!(sqlx::query_scalar::<_,i64>("select count(*) from continuous_frozen_chunks").fetch_one(&pool).await.unwrap(),0);
        assert_eq!(setups_db::optimization::get(&pool,same.study_id).await.unwrap().unwrap().status,"completed");
        assert_eq!(maintenance::set_paused(&pool,OWNER,false).await.unwrap().state,"running");
        assert!(sqlx::query_scalar::<_,bool>("select desired_running from continuous_processes").fetch_one(&pool).await.unwrap());
    }).await;
}

#[tokio::test]
#[ignore="requires isolated TEST_DATABASE_URL ending _test with CREATEDB"]
async fn checkpoint_replacement_is_bounded_detects_corruption_and_stop_reclaims_only_private_data() {
    isolated(|pool|async move {
        let job=job(&pool).await;snapshot(&pool,&job).await;
        let first=recovery::save_checkpoint(&pool,&job,&identity(),b"first",2*1024*1024).await.unwrap();
        assert_eq!(recovery::save_checkpoint(&pool,&job,&identity(),b"first",2*1024*1024).await.unwrap(),first);
        assert_eq!(recovery::save_checkpoint(&pool,&job,&identity(),b"second",2*1024*1024).await.unwrap(),first+1);
        assert!(recovery::save_checkpoint(&pool,&job,&identity(),&vec![0;recovery::MAX_CHECKPOINT_BYTES+1],2*1024*1024).await.is_err());
        assert_eq!(sqlx::query_scalar::<_,i64>("select count(*) from continuous_runtime_artifacts").fetch_one(&pool).await.unwrap(),1);
        assert!(recovery::load(&pool,&job,&json!({"version":2})).await.is_err());
        sqlx::query("update continuous_runtime_artifacts set checkpoint=decode('00','hex')").execute(&pool).await.unwrap();
        assert!(recovery::load(&pool,&job,&identity()).await.err().unwrap().to_string().contains("corrupt"));
        scheduler::finish_search(&pool,&job,&json!({"progress":{},"best":null}),true).await.unwrap();
        db::stop(&pool,OWNER,ALGORITHM).await.unwrap();
        assert!(recovery::save_checkpoint(&pool,&job,&identity(),b"after-stop",2*1024*1024).await.is_err());
        db::cleanup_stopped(&pool,OWNER,ALGORITHM,&job.lease).await.unwrap();
        assert_eq!(sqlx::query_scalar::<_,i64>("select count(*) from continuous_frozen_chunks").fetch_one(&pool).await.unwrap(),0);
        assert_eq!(setups_db::optimization::get(&pool,job.study_id).await.unwrap().unwrap().status,"completed");
        assert_eq!(recovery::reclaim_orphans(&pool).await.unwrap(),0);
    }).await;
}

#[tokio::test]
#[ignore="requires isolated TEST_DATABASE_URL ending _test with CREATEDB"]
async fn repeated_same_generation_storage_reservation_replaces_itself_under_tight_quota() {
    isolated(|pool|async move {
        let job=job(&pool).await;
        let bytes:i64=sqlx::query_scalar("select pg_database_size(current_database())::bigint").fetch_one(&pool).await.unwrap();
        let estimate=1024*1024;let limit=bytes+estimate+256*1024;
        for _ in 0..8 {
            let admission=scheduler::reserve_storage(&pool,&job,estimate,limit,&Default::default()).await.unwrap().unwrap();
            assert!(admission.admitted,"same reservation double-counted: requested={} limit={}",admission.requested,limit);
            assert!(sqlx::query_scalar::<_,i64>("select reserved_bytes from continuous_generations").fetch_one(&pool).await.unwrap()<=estimate);
        }
        scheduler::finish_search(&pool,&job,&json!({"progress":{},"best":null,"fixture":"x".repeat(65536)}),true).await.unwrap();
        assert!(scheduler::reserve_storage(&pool,&job,estimate,limit,&Default::default()).await.unwrap().unwrap().admitted);
    }).await;
}

#[tokio::test]
#[ignore="requires isolated TEST_DATABASE_URL ending _test with CREATEDB"]
async fn uncommitted_acquisition_recovers_same_window_and_original_expired_deadline_without_reset() {
    isolated(|pool|async move {
        let old=job(&pool).await;
        sqlx::query("update continuous_generations set research_deadline=execution_started_at-interval '1 second'").execute(&pool).await.unwrap();
        expire(&pool).await;
        let lease=scheduler::claim_owner(&pool,"after-acquisition-crash").await.unwrap().unwrap();
        scheduler::recover_owner(&pool,&lease).await.unwrap();
        let same=recovery::active_job(&pool,&lease).await.unwrap().unwrap();
        assert_eq!((same.generation_id,same.study_id,&same.spec),(old.generation_id,old.study_id,&old.spec));
        assert!(same.research_deadline.unwrap()<chrono::Utc::now());
        assert!(!recovery::load(&pool,&same,&identity()).await.unwrap().snapshot_committed);
        assert_eq!(sqlx::query_scalar::<_,i32>("select retry_count from continuous_processes").fetch_one(&pool).await.unwrap(),1,"crashed acquisition is charged once at takeover, not a new generation");
    }).await;
}

#[tokio::test]
#[ignore="requires isolated TEST_DATABASE_URL ending _test with CREATEDB"]
async fn incompatible_generation_fails_visibly_preserves_completed_child_and_reclaims_runtime_orphans() {
    isolated(|pool|async move {
        let old=job(&pool).await;snapshot(&pool,&old).await;
        let result=json!({"progress":{},"best":null,"historicalSentinel":1.2345678901234567});
        scheduler::finish_search(&pool,&old,&result,true).await.unwrap();
        sqlx::query("update continuous_generations set resume_version=999").execute(&pool).await.unwrap();
        maintenance::set_paused(&pool,OWNER,true).await.unwrap();expire(&pool).await;
        let lease=scheduler::claim_owner(&pool,"unsupported").await.unwrap().unwrap();
        scheduler::recover_owner(&pool,&lease).await.unwrap();
        assert!(recovery::active_job(&pool,&lease).await.unwrap().is_none());
        let error:Value=sqlx::query_scalar("select error from continuous_generations").fetch_one(&pool).await.unwrap();
        assert_eq!(error["code"],"resume_version_incompatible");
        let completed=setups_db::optimization::get(&pool,old.study_id).await.unwrap().unwrap();
        assert_eq!(completed.status,"completed");assert_eq!(completed.result,Some(result));
        assert_eq!(sqlx::query_scalar::<_,i64>("select count(*) from continuous_runtime_artifacts").fetch_one(&pool).await.unwrap(),0);
        assert_eq!(sqlx::query_scalar::<_,i32>("select retry_count from continuous_processes").fetch_one(&pool).await.unwrap(),1);
        maintenance::set_paused(&pool,OWNER,false).await.unwrap();
        assert_eq!(sqlx::query_scalar::<_,i32>("select retry_count from continuous_processes").fetch_one(&pool).await.unwrap(),1);
        // Emulate an orphan left after a committed terminal transition; only the
        // private generation-owned row/chunks may be reclaimed by startup GC.
        sqlx::query("insert into continuous_runtime_artifacts(generation_id,identity) values($1,'{}')").bind(old.generation_id).execute(&pool).await.unwrap();
        assert_eq!(recovery::reclaim_orphans(&pool).await.unwrap(),1);
        assert_eq!(setups_db::optimization::get(&pool,old.study_id).await.unwrap().unwrap().status,"completed");
    }).await;
}

#[tokio::test]
#[ignore="requires isolated TEST_DATABASE_URL ending _test with CREATEDB"]
async fn uncertain_recovery_claims_are_bounded_keep_head_and_explicit_start_retries_same_ids() {
    isolated(|pool|async move {
        let mut current=job(&pool).await;snapshot(&pool,&current).await;
        recovery::save_checkpoint(&pool,&current,&identity(),b"committed",2*1024*1024).await.unwrap();
        let original=current.clone();
        let error=json!({"code":"persistence_unconfirmed","message":"lost durable acknowledgement"});
        for attempts in 0..=3 {
            assert!(recovery::defer(&pool,&current,&error).await.unwrap());
            assert!(recovery::defer(&pool,&current,&error).await.unwrap(),"lost defer ACK must be idempotent");
            assert_eq!(sqlx::query_scalar::<_,i32>("select retry_count from continuous_processes").fetch_one(&pool).await.unwrap(),attempts);
            assert!(recovery::active_job(&pool,&current.lease).await.unwrap().is_none(),"backoff applies even with active IDs");
            scheduler::release(&pool,&current.lease).await.unwrap();
            assert!(scheduler::claim_owner(&pool,"too-early").await.unwrap().is_none());
            sqlx::query("update continuous_processes set next_attempt_at=now()-interval '1 second'").execute(&pool).await.unwrap();
            if attempts==3 {break;}
            let lease=scheduler::claim_owner(&pool,"bounded-recovery").await.unwrap().unwrap();
            scheduler::recover_owner(&pool,&lease).await.unwrap();
            current=recovery::active_job(&pool,&lease).await.unwrap().expect("third charged claim must actually run");
            assert_eq!((current.generation_id,current.study_id,current.research_deadline),(original.generation_id,original.study_id,original.research_deadline));
            // Retrying an identical checkpoint or writing phase/progress is not
            // evidence of a new frontier and must not reset failed recoveries.
            recovery::save_checkpoint(&pool,&current,&identity(),b"committed",2*1024*1024).await.unwrap();
            scheduler::phase(&pool,&current,"searching",Some(&json!({"evaluatedCandidates":1}))).await.unwrap();
            assert_eq!(sqlx::query_scalar::<_,i32>("select retry_count from continuous_processes").fetch_one(&pool).await.unwrap(),attempts+1);
        }
        assert!(scheduler::claim_owner(&pool,"no-fourth").await.unwrap().is_none());
        assert_eq!(db::get(&pool,OWNER,ALGORITHM).await.unwrap().unwrap().error.unwrap()["code"],"durable_resume_retry_exhausted");
        assert_eq!(sqlx::query_scalar::<_,String>("select status from continuous_generations").fetch_one(&pool).await.unwrap(),"running");
        assert_eq!(sqlx::query_scalar::<_,Vec<u8>>("select checkpoint from continuous_runtime_artifacts").fetch_one(&pool).await.unwrap(),b"committed");
        maintenance::set_paused(&pool,OWNER,true).await.unwrap();maintenance::set_paused(&pool,OWNER,false).await.unwrap();
        assert!(scheduler::claim_owner(&pool,"maintenance-is-not-start").await.unwrap().is_none());
        assert!(db::start(&pool,OWNER,ALGORITHM).await.unwrap());
        let lease=scheduler::claim_owner(&pool,"explicit-same-head-retry").await.unwrap().unwrap();scheduler::recover_owner(&pool,&lease).await.unwrap();
        let same=recovery::active_job(&pool,&lease).await.unwrap().unwrap();
        assert_eq!((same.generation_id,same.study_id,same.research_deadline),(original.generation_id,original.study_id,original.research_deadline));
        assert_eq!(sqlx::query_scalar::<_,i64>("select count(*) from continuous_generations").fetch_one(&pool).await.unwrap(),1);
        db::stop(&pool,OWNER,ALGORITHM).await.unwrap();scheduler::drain_stops(&pool,&lease).await.unwrap();
        assert_eq!(sqlx::query_scalar::<_,i64>("select count(*) from continuous_runtime_artifacts").fetch_one(&pool).await.unwrap(),0);
    }).await;
}

#[tokio::test]
#[ignore="requires isolated TEST_DATABASE_URL ending _test with CREATEDB"]
async fn queued_or_stopping_other_algorithm_cannot_bypass_active_backoff_or_exhaustion() {
    isolated(|pool|async move {
        const OTHER:&str="price_drop_volume_spike";
        let original=job(&pool).await;snapshot(&pool,&original).await;
        recovery::save_checkpoint(&pool,&original,&identity(),b"retained-head",2*1024*1024).await.unwrap();
        let mut other=original.config.clone();other["study"]["algorithmId"]=OTHER.into();
        db::configure(&pool,OWNER,OTHER,&other,&[0.0]).await.unwrap();db::start(&pool,OWNER,OTHER).await.unwrap();
        recovery::defer(&pool,&original,&json!({"code":"persistence_unconfirmed"})).await.unwrap();
        scheduler::release(&pool,&original.lease).await.unwrap();
        assert!(scheduler::claim_owner(&pool,"queued-b-cannot-skip-a-backoff").await.unwrap().is_none());
        sqlx::query("update continuous_processes set retry_count=3,error='{\"code\":\"durable_resume_retry_exhausted\"}',next_attempt_at=now()-interval '1 second' where algorithm_id=$1").bind(ALGORITHM).execute(&pool).await.unwrap();
        assert!(scheduler::claim_owner(&pool,"queued-b-cannot-authorize-fourth-a").await.unwrap().is_none());
        db::stop(&pool,OWNER,OTHER).await.unwrap();
        let housekeeping=scheduler::claim_owner(&pool,"stop-b-only").await.unwrap().expect("Stop B must remain drainable");
        scheduler::recover_owner(&pool,&housekeeping).await.unwrap();
        assert!(recovery::active_job(&pool,&housekeeping).await.unwrap().is_none(),"housekeeping lease must not resume latched A");
        assert!(scheduler::prepare_next(&pool,&housekeeping).await.unwrap().is_none());
        assert_eq!(db::get(&pool,OWNER,OTHER).await.unwrap().unwrap().status,"stopped");
        assert_eq!(db::get(&pool,OWNER,ALGORITHM).await.unwrap().unwrap().error.unwrap()["code"],"durable_resume_retry_exhausted");
        assert_eq!(sqlx::query_scalar::<_,String>("select status from continuous_generations").fetch_one(&pool).await.unwrap(),"running");
        assert_eq!(sqlx::query_scalar::<_,Vec<u8>>("select checkpoint from continuous_runtime_artifacts").fetch_one(&pool).await.unwrap(),b"retained-head");
        scheduler::release(&pool,&housekeeping).await.unwrap();db::start(&pool,OWNER,OTHER).await.unwrap();
        assert!(scheduler::claim_owner(&pool,"restarted-b-still-cannot-bypass-a").await.unwrap().is_none());
        db::start(&pool,OWNER,ALGORITHM).await.unwrap();
        let lease=scheduler::claim_owner(&pool,"explicitly-retry-a").await.unwrap().unwrap();scheduler::recover_owner(&pool,&lease).await.unwrap();
        let same=recovery::active_job(&pool,&lease).await.unwrap().unwrap();
        assert_eq!((same.generation_id,same.study_id,same.research_deadline),(original.generation_id,original.study_id,original.research_deadline));
    }).await;
}

async fn third_charged_attempt(pool:&PgPool)->scheduler::ChainJob {
    let mut current=job(pool).await;snapshot(pool,&current).await;
    recovery::save_checkpoint(pool,&current,&identity(),b"third-attempt-head",2*1024*1024).await.unwrap();
    for count in 1..=3 {
        recovery::defer(pool,&current,&json!({"code":"persistence_unconfirmed"})).await.unwrap();scheduler::release(pool,&current.lease).await.unwrap();
        sqlx::query("update continuous_processes set next_attempt_at=now()-interval '1 second'").execute(pool).await.unwrap();
        let lease=scheduler::claim_owner(pool,"charged-attempt").await.unwrap().unwrap();scheduler::recover_owner(pool,&lease).await.unwrap();
        current=recovery::active_job(pool,&lease).await.unwrap().expect("each of the three authorized attempts must execute");
        assert_eq!(sqlx::query_scalar::<_,i32>("select retry_count from continuous_processes").fetch_one(pool).await.unwrap(),count);
    }
    current
}

#[tokio::test]
#[ignore="requires isolated TEST_DATABASE_URL ending _test with CREATEDB"]
async fn third_recovery_crash_before_defer_gets_metadata_only_latch_not_a_fourth_attempt() {
    for stop_other in [true,false] {
        isolated(move|pool|async move {
            const OTHER:&str="price_drop_volume_spike";
            let old=third_charged_attempt(&pool).await;
            if stop_other {
                let mut config=old.config.clone();config["study"]["algorithmId"]=OTHER.into();
                db::configure(&pool,OWNER,OTHER,&config,&[0.0]).await.unwrap();db::start(&pool,OWNER,OTHER).await.unwrap();db::stop(&pool,OWNER,OTHER).await.unwrap();
            }
            expire(&pool).await; // Crash: no defer/error write and no clean release.
            let latch=scheduler::claim_owner(&pool,"metadata-only-exhaustion").await.unwrap().expect("third crash must become visible even without Stop B");
            scheduler::recover_owner(&pool,&latch).await.unwrap();
            assert!(recovery::active_job(&pool,&latch).await.unwrap().is_none(),"housekeeping/refencing cannot authorize a fourth execution");
            assert_eq!(db::get(&pool,OWNER,ALGORITHM).await.unwrap().unwrap().error.unwrap()["code"],"durable_resume_retry_exhausted");
            if stop_other {assert_eq!(db::get(&pool,OWNER,OTHER).await.unwrap().unwrap().status,"stopped");}
            assert_eq!(sqlx::query_scalar::<_,Vec<u8>>("select checkpoint from continuous_runtime_artifacts").fetch_one(&pool).await.unwrap(),b"third-attempt-head");
            scheduler::release(&pool,&latch).await.unwrap();assert!(scheduler::claim_owner(&pool,"no-metadata-spin").await.unwrap().is_none());
            db::start(&pool,OWNER,ALGORITHM).await.unwrap();
            let retry=scheduler::claim_owner(&pool,"explicit-after-crash").await.unwrap().unwrap();scheduler::recover_owner(&pool,&retry).await.unwrap();
            let same=recovery::active_job(&pool,&retry).await.unwrap().unwrap();
            assert_eq!((same.generation_id,same.study_id,same.research_deadline),(old.generation_id,old.study_id,old.research_deadline));
        }).await;
    }
}

#[tokio::test]
#[ignore="requires isolated TEST_DATABASE_URL ending _test with CREATEDB"]
async fn clean_suspension_of_charged_third_attempt_continues_without_recharge_or_new_head() {
    isolated(|pool|async move {
        let mut current=third_charged_attempt(&pool).await;let original=current.clone();
        for _ in 0..3 {
            scheduler::release(&pool,&current.lease).await.unwrap(); // joined graceful pause, no durable advancement
            let lease=scheduler::claim_owner(&pool,"clean-third-continuation").await.unwrap().expect("clean suspension must not exhaust its already charged attempt");
            scheduler::recover_owner(&pool,&lease).await.unwrap();
            current=recovery::active_job(&pool,&lease).await.unwrap().unwrap();
            assert_eq!((current.generation_id,current.study_id,current.research_deadline),(original.generation_id,original.study_id,original.research_deadline));
            scheduler::phase(&pool,&current,"searching",Some(&json!({"evaluatedCandidates":1}))).await.unwrap();
            scheduler::heartbeat(&pool,&lease).await.unwrap();
            recovery::save_checkpoint(&pool,&current,&identity(),b"third-attempt-head",2*1024*1024).await.unwrap();
            assert_eq!(sqlx::query_scalar::<_,i32>("select retry_count from continuous_processes").fetch_one(&pool).await.unwrap(),3,"phase/heartbeat/exact checkpoint do not reset or recharge");
        }
        recovery::defer(&pool,&current,&json!({"code":"persistence_unconfirmed"})).await.unwrap();scheduler::release(&pool,&current.lease).await.unwrap();
        assert_eq!(db::get(&pool,OWNER,ALGORITHM).await.unwrap().unwrap().error.unwrap()["code"],"durable_resume_retry_exhausted");
        assert!(scheduler::claim_owner(&pool,"failed-third-is-not-clean-pause").await.unwrap().is_none());
    }).await;
}
