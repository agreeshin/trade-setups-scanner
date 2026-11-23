//! Isolated DB transition acceptance for PRD 47 / analytics 10.4.1.
//! Marker snapshot/checkpoint bytes below prove DB ownership/acknowledgement
//! only, NOT optimizer codec validity or scientific/original-ledger parity.
use serde_json::{Value, json};
use setups_db::{
    continuous::{self as continuous, frozen, maintenance, recovery, scheduler},
    optimization::{self as studies, manual},
};
use sqlx::{PgPool, postgres::{PgConnectOptions, PgPoolOptions}};
use std::{future::Future, str::FromStr};

const OWNER: &str = "admin";
const ALGORITHM: &str = "price_volume_spike";

async fn isolated<F, Fut>(body: F)
where
    F: FnOnce(PgPool) -> Fut + 'static,
    Fut: Future<Output = ()> + 'static,
{
    let options = PgConnectOptions::from_str(
        &std::env::var("TEST_DATABASE_URL").expect("isolated *_test URL with CREATEDB required"),
    ).unwrap();
    assert!(options.get_database().is_some_and(|name| name.ends_with("_test")));
    let admin = PgPoolOptions::new().max_connections(2)
        .connect_with(options.clone()).await.unwrap();
    let backend: i32 = sqlx::query_scalar("select pg_backend_pid()")
        .fetch_one(&admin).await.unwrap();
    let name = format!("manual_queue_acceptance_{}_{}_test", std::process::id(), backend);
    sqlx::query(&format!("create database {name}"))
        .execute(&admin).await.unwrap();
    let pool = PgPoolOptions::new().max_connections(8)
        .connect_with(options.database(&name)).await.unwrap();
    let child = pool.clone();
    // Retain the existing isolated-test panic boundary so even a failing
    // acceptance assertion closes connections and drops only its own database.
    let result = tokio::task::LocalSet::new().run_until(async move {
        tokio::task::spawn_local(async move {
            let baseline = include_str!("../migrations/0010_baseline.sql").lines()
                .filter(|line| !line.starts_with('\\') && !line.starts_with("SET transaction_timeout")
                    && !line.contains("set_config('search_path'"))
                .collect::<Vec<_>>().join("\n");
            sqlx::raw_sql(&baseline).execute(&child).await.unwrap();
            sqlx::raw_sql("set search_path to public;create table kysely_migration(name varchar(255) primary key,timestamp varchar(255) not null);insert into kysely_migration values('0009_legacy','legacy')")
                .execute(&child).await.unwrap();
            setups_db::migrate::ensure_migrations(&child).await.unwrap();
            assert!(sqlx::query_scalar::<_, bool>("select to_regclass('manual_runtime_artifacts') is not null")
                .fetch_one(&child).await.unwrap(), "fresh migration 0031 must be present");
            // recover() is legal only for the detached native runtime owner.
            let mut runtime = child.acquire().await.unwrap();
            sqlx::query("select pg_advisory_lock($1)").bind(studies::RUNTIME_LOCK)
                .execute(&mut *runtime).await.unwrap();
            body(child).await;
            sqlx::query("select pg_advisory_unlock($1)").bind(studies::RUNTIME_LOCK)
                .execute(&mut *runtime).await.unwrap();
        }).await
    }).await;
    pool.close().await;
    sqlx::query(&format!("drop database {name} with(force)"))
        .execute(&admin).await.unwrap();
    admin.close().await;
    if let Err(error) = result { std::panic::resume_unwind(error.into_panic()); }
}

fn progress(elapsed: f64) -> Value {
    json!({"phase":"initial","evaluatedCandidates":0,"scorableCandidates":0,
        "maxTrials":8,"elapsedSeconds":elapsed,"bestScore":null})
}
async fn create(pool: &PgPool, owner: &str, label: &str) -> studies::StudyRecord {
    let spec = json!({"algorithmId":ALGORITHM,"algorithmVersion":"1.1.1",
        "maxSeconds":30,"maxTrials":8,"seed":42,"fixtureLabel":label});
    match manual::create(pool, owner, ALGORITHM, &spec, &progress(0.0), Some(label),
        &manual::Limits::default()).await.unwrap() {
        studies::Admission::Created(study) => *study,
        _ => panic!("bounded manual fixture admission rejected"),
    }
}
async fn finish(pool: &PgPool, job: &manual::Job, status: &str) {
    let progress = progress(job.elapsed_ms() as f64 / 1000.0);
    let result = json!({"progress":progress,"best":null,
        "reason":if status == "cancelled" {"cancelled"} else {"trial_budget"}});
    studies::finish_fenced(pool, job.study.id, Some(job.epoch), status, &progress,
        Some(&result), None, &json!([]), None).await.unwrap();
}
async fn queue_sequence(pool: &PgPool, id: i64) -> i64 {
    sqlx::query_scalar("select manual_queue_sequence from optimization_studies where id=$1")
        .bind(id).fetch_one(pool).await.unwrap()
}
fn revision(study: &studies::StudyRecord) -> i64 {
    study.manual_control.as_ref().unwrap().revision
}
fn changed(control: manual::Control) -> studies::StudyRecord {
    match control {
        manual::Control::Changed(study) => *study,
        _ => panic!("expected accepted manual control transition"),
    }
}
async fn committed_manual_input(pool: &PgPool, job: &manual::Job, checkpoint: bool) {
    let limits = manual::Limits::default();
    let bytes = b"snapshot";
    manual::reserve(pool, job, &json!({"version":1,"studyId":job.study.id,"fixture":"DB-only"}),
        1024, &limits).await.unwrap();
    manual::write_chunk(pool, job, 0, bytes, &limits).await.unwrap();
    let manifest = frozen::SnapshotManifest { version:1, codec:"framed-cbor-1".into(), chunks:1,
        encoded_bytes:bytes.len() as u64, sha256:frozen::checksum(bytes),
        snapshot_hash:"db-transition-marker".into(), captured_at:"2026-11-23T00:00:00Z".into() };
    manual::seal(pool, job, &manifest, 120, &limits).await.unwrap();
    if checkpoint {
        manual::checkpoint(pool, job, 0, b"DB-only-acknowledged-boundary", &limits).await.unwrap();
    }
}
async fn configure(pool: &PgPool) -> Value {
    let config = json!({"adaptiveEnabled":true,"lookbackDays":1,"minSetupsPerDay":[0,4,8],
        "minMeanExpectancy":0.15,"study":{"algorithmId":ALGORITHM,"algorithmVersion":"1.1.1",
        "maxSeconds":3600,"maxTrials":8,"seed":42}});
    assert!(continuous::configure(pool, OWNER, ALGORITHM, &config, &[0.0,4.0,8.0]).await.unwrap());
    assert!(continuous::start(pool, OWNER, ALGORITHM).await.unwrap());
    config
}
async fn job(pool: &PgPool) -> scheduler::ChainJob {
    configure(pool).await;
    let lease = scheduler::claim_owner(pool, "acceptance-first").await.unwrap().unwrap();
    scheduler::prepare_next(pool, &lease).await.unwrap().unwrap()
}
fn identity() -> Value { json!({"version":1,"requestHash":"DB-only-continuous-fixture"}) }
async fn continuous_snapshot(pool: &PgPool, job: &scheduler::ChainJob) {
    // Same compact transition-only fixture as continuous_resume.rs. The
    // executor, not these arbitration tests, validates the frozen input codec.
    sqlx::query("insert into continuous_runtime_artifacts(generation_id,identity,reserved_bytes,snapshot_manifest,snapshot_bytes) values($1,$2,1048576,'{}',1)")
        .bind(job.generation_id).bind(identity()).execute(pool).await.unwrap();
    sqlx::query("insert into continuous_frozen_chunks values($1,0,decode('01','hex'),repeat('0',64))")
        .bind(job.generation_id).execute(pool).await.unwrap();
    recovery::save_checkpoint(pool, job, &identity(), b"DB-only-continuous-boundary", 2*1024*1024)
        .await.unwrap();
}
async fn continuous_state(pool: &PgPool) -> Value {
    // Exclude executor lease metadata only; all process fields, rotation,
    // retries, user intent, config and maintenance remain byte-value compared.
    sqlx::query_scalar("select jsonb_build_object('processes',(select jsonb_agg(to_jsonb(p) order by p.algorithm_id) from continuous_processes p where owner_id=$1),'lastAlgorithm',(select last_algorithm_id from continuous_owner_lanes where owner_id=$1),'maintenance',(select to_jsonb(m) from continuous_maintenance m where owner_id=$1))")
        .bind(OWNER).fetch_one(pool).await.unwrap()
}

#[tokio::test]
#[ignore = "requires isolated TEST_DATABASE_URL ending _test with CREATEDB"]
async fn full_continuous_chain_then_manual_fifo_then_original_rotation_and_recovery() {
    isolated(|pool| async move {
        let first = job(&pool).await;
        continuous_snapshot(&pool, &first).await;
        let a = create(&pool, OWNER, "A").await;
        let b = create(&pool, OWNER, "B").await;
        assert!(queue_sequence(&pool,a.id).await < queue_sequence(&pool,b.id).await);
        assert!(manual::claim(&pool,None).await.unwrap().is_none(), "running continuous search owns the full lane");
        assert!(scheduler::prepare_next(&pool,&first.lease).await.unwrap().is_none());

        // Pending manual work may not suppress admission of the already-owned
        // continuous head after a clean executor release/recovery.
        scheduler::release(&pool,&first.lease).await.unwrap();
        let lease = scheduler::claim_owner(&pool,"acceptance-recovery").await.unwrap()
            .expect("active continuous recovery remains admissible with manual pending");
        scheduler::recover_owner(&pool,&lease).await.unwrap();
        let same = recovery::active_job(&pool,&lease).await.unwrap().unwrap();
        assert_eq!((same.study_id,same.generation_id,same.slot_id,same.config_revision),
            (first.study_id,first.generation_id,first.slot_id,first.config_revision));
        assert_eq!(same.execution_started_at,first.execution_started_at);
        assert_eq!(same.research_deadline,first.research_deadline);
        assert_eq!(same.spec,first.spec);
        assert_eq!(recovery::load(&pool,&same,&identity()).await.unwrap().checkpoint.unwrap(),b"DB-only-continuous-boundary");
        assert!(manual::claim(&pool,None).await.unwrap().is_none());

        scheduler::finish_search(&pool,&same,&json!({"progress":{},"best":null}),true).await.unwrap();
        assert_eq!(studies::get(&pool,same.study_id).await.unwrap().unwrap().status,"completed");
        assert!(manual::claim(&pool,None).await.unwrap().is_none(), "completed search is not completed continuous chain");
        scheduler::persist_replay(&pool,&same,&json!({}),json!({"instruments":[{}]}),
            &[scheduler::ServiceReplayRun { exchange:"bybit".into(),category:"linear".into(),
                symbol:"QUEUEUSDT".into(),setups:json!([]),warmup_minutes:0,evaluated_minutes:1 }])
            .await.unwrap().unwrap();
        assert!(manual::claim(&pool,None).await.unwrap().is_none(), "durable replay still requires gate/finalize");
        let gate = recovery::Gate { result:json!({"best":null,"reason":"no_winner"}),
            accepted:false,mean:None,selected:None };
        assert!(recovery::save_gate(&pool,&same,&identity(),&gate).await.unwrap());
        assert!(manual::claim(&pool,None).await.unwrap().is_none(), "saved gate still owns continuous lane");
        assert!(scheduler::finalize(&pool,&same,&gate.result,false,None,None).await.unwrap());

        // Make the normal next slot immediately due, so no backoff/pause flag
        // can accidentally masquerade as successful manual arbitration.
        sqlx::query("update continuous_processes set next_attempt_at=to_timestamp(123),retry_count=1 where owner_id=$1")
            .bind(OWNER).execute(&pool).await.unwrap();
        let after_chain = continuous_state(&pool).await;
        assert_eq!(after_chain["processes"][0]["next_slot"],1);
        assert!(scheduler::prepare_next(&pool,&lease).await.unwrap().is_none(), "queued A/B block new continuous admission");
        let claimed_a = manual::claim(&pool,None).await.unwrap().unwrap();
        assert_eq!(claimed_a.study.id,a.id);
        assert!(scheduler::prepare_next(&pool,&lease).await.unwrap().is_none());
        finish(&pool,&claimed_a,"completed").await;
        let claimed_b = manual::claim(&pool,None).await.unwrap().unwrap();
        assert_eq!(claimed_b.study.id,b.id);
        assert!(scheduler::prepare_next(&pool,&lease).await.unwrap().is_none(), "reserved manual blocks even without queued rows");
        scheduler::release(&pool,&lease).await.unwrap();
        assert!(scheduler::claim_owner(&pool,"no-hot-loop").await.unwrap().is_none(), "idle owner must not spin on a manual holder");
        finish(&pool,&claimed_b,"completed").await;
        assert_eq!(continuous_state(&pool).await,after_chain,"manual drain does not rewrite continuous state");
        let next_lease = scheduler::claim_owner(&pool,"normal-continuation").await.unwrap().unwrap();
        let next = scheduler::prepare_next(&pool,&next_lease).await.unwrap().unwrap();
        assert_eq!(next.algorithm_id,first.algorithm_id);
        assert_eq!(next.spec["minSetupsPerDay"],4.0,"continue original next slot, not restarted slot zero");
        assert_eq!(next.config,first.config);
        assert_eq!(next.config_revision,first.config_revision);
        assert_ne!(next.study_id,first.study_id);
        assert_ne!(next.slot_id,first.slot_id);
        scheduler::release(&pool,&next_lease).await.unwrap();
    }).await;
}

#[tokio::test]
#[ignore = "requires isolated TEST_DATABASE_URL ending _test with CREATEDB"]
async fn pause_releases_lane_play_appends_same_id_once_and_preserves_active_budget() {
    isolated(|pool| async move {
        let a = create(&pool,OWNER,"A").await;
        let b = create(&pool,OWNER,"B").await;
        sqlx::query("update optimization_studies set created_at=clock_timestamp()-interval '1 day' where id in ($1,$2)")
            .bind(a.id).bind(b.id).execute(&pool).await.unwrap();
        let pins = json!({"fixtureExecutionLimits":"immutable"});
        let first_a = manual::claim(&pool,Some(&pins)).await.unwrap().unwrap();
        assert_eq!(first_a.study.id,a.id);
        assert_eq!(first_a.spent_ms,0,"queued day does not consume maxSeconds");
        let started:Value = sqlx::query_scalar("select to_jsonb(execution_started_at) from optimization_studies where id=$1")
            .bind(a.id).fetch_one(&pool).await.unwrap();
        committed_manual_input(&pool,&first_a,true).await;
        sqlx::query("update optimization_studies set manual_spent_ms=2000,manual_segment_started_at=clock_timestamp()-interval '3 seconds' where id=$1")
            .bind(a.id).execute(&pool).await.unwrap();
        assert_eq!(studies::delete(&pool,a.id).await.unwrap(),Some(studies::DeleteStudyOutcome::Active));
        let pausing = changed(manual::control(&pool,OWNER,a.id,revision(&a),false).await.unwrap());
        assert_eq!(pausing.status,"pausing");
        assert_eq!(studies::delete(&pool,a.id).await.unwrap(),Some(studies::DeleteStudyOutcome::Active));
        assert!(manual::pause_ack(&pool,&first_a,&progress(5.0),&json!([])).await.unwrap());
        let paused = studies::get(&pool,a.id).await.unwrap().unwrap();
        assert_eq!(paused.status,"paused");
        assert_eq!(paused.progress["elapsedSeconds"],5.0);
        assert_eq!(studies::delete(&pool,a.id).await.unwrap(),Some(studies::DeleteStudyOutcome::Active));
        let spent:i64 = sqlx::query_scalar("select manual_spent_ms from optimization_studies where id=$1")
            .bind(a.id).fetch_one(&pool).await.unwrap();
        assert!(spent>=5000,"active acquisition/compute segment was charged");
        sqlx::query("update optimization_studies set updated_at=clock_timestamp()-interval '12 hours' where id=$1")
            .bind(a.id).execute(&pool).await.unwrap();

        let claimed_b = manual::claim(&pool,None).await.unwrap().unwrap();
        assert_eq!(claimed_b.study.id,b.id,"user-paused A releases the lane");
        assert_eq!(claimed_b.spent_ms,0,"queued B's day is still not active time");
        let c = create(&pool,OWNER,"C").await;
        let deleted = create(&pool,OWNER,"queued-delete").await;
        assert_eq!(studies::delete(&pool,deleted.id).await.unwrap(),Some(studies::DeleteStudyOutcome::Deleted));
        assert!(studies::get(&pool,deleted.id).await.unwrap().is_none());
        let queued_a = changed(manual::control(&pool,OWNER,a.id,revision(&paused),true).await.unwrap());
        assert_eq!((queued_a.id,queued_a.status.as_str()),(a.id,"queued"));
        let tail = queue_sequence(&pool,a.id).await;
        assert!(tail>queue_sequence(&pool,c.id).await,"Play appends behind current C, not original position A");
        let duplicate = changed(manual::control(&pool,OWNER,a.id,revision(&queued_a),true).await.unwrap());
        assert_eq!(revision(&duplicate),revision(&queued_a));
        assert!(matches!(manual::control(&pool,OWNER,a.id,revision(&paused),true).await.unwrap(),manual::Control::Stale));
        assert_eq!(queue_sequence(&pool,a.id).await,tail,"neither duplicate nor stale Play repositions A");
        assert_eq!(studies::get(&pool,b.id).await.unwrap().unwrap().status,"running","Play cannot interrupt B");
        finish(&pool,&claimed_b,"completed").await;
        let claimed_c = manual::claim(&pool,None).await.unwrap().unwrap();
        assert_eq!(claimed_c.study.id,c.id);
        finish(&pool,&claimed_c,"completed").await;
        let resumed_a = manual::claim(&pool,None).await.unwrap().unwrap();
        assert_eq!(resumed_a.study.id,a.id);
        assert_eq!(resumed_a.study.spec,a.spec);
        assert!(resumed_a.epoch>first_a.epoch);
        assert_eq!(resumed_a.spent_ms,spent,"paused/queued wall time never resets or increases spent budget");
        assert!(resumed_a.segment_started_at.is_some());
        assert_eq!(resumed_a.limits,Some(pins));
        assert_eq!(sqlx::query_scalar::<_,Value>("select to_jsonb(execution_started_at) from optimization_studies where id=$1")
            .bind(a.id).fetch_one(&pool).await.unwrap(),started,"Play preserves original actual start");
        let artifact = manual::artifact(&pool,&resumed_a,&manual::Limits::default()).await.unwrap().unwrap();
        assert_eq!(artifact.checkpoint.unwrap(),b"DB-only-acknowledged-boundary");
        assert!(manual::save_progress(&pool,&first_a,&progress(0.0),&json!([])).await.is_err(),"stale callback is fenced");
        finish(&pool,&resumed_a,"completed").await;
        assert_eq!(sqlx::query_scalar::<_,i64>("select count(*) from manual_runtime_artifacts").fetch_one(&pool).await.unwrap(),0);
        assert_eq!(sqlx::query_scalar::<_,i64>("select count(*) from manual_frozen_chunks").fetch_one(&pool).await.unwrap(),0);
        assert_eq!(studies::delete(&pool,a.id).await.unwrap(),Some(studies::DeleteStudyOutcome::Deleted));
    }).await;
}

#[tokio::test]
#[ignore = "requires isolated TEST_DATABASE_URL ending _test with CREATEDB"]
async fn queued_delete_and_claim_are_atomic_and_never_delete_running_work() {
    isolated(|pool| async move {
        for attempt in 0..8 {
            let study = create(&pool,&format!("race-owner-{attempt}"),"race").await;
            let (deleted,claimed) = tokio::join!(studies::delete(&pool,study.id),manual::claim(&pool,None));
            match (deleted.unwrap(),claimed.unwrap()) {
                (Some(studies::DeleteStudyOutcome::Deleted),None) => {
                    assert!(studies::get(&pool,study.id).await.unwrap().is_none());
                }
                (Some(studies::DeleteStudyOutcome::Active),Some(job)) => {
                    assert_eq!(job.study.id,study.id);
                    assert_eq!(studies::get(&pool,study.id).await.unwrap().unwrap().status,"running");
                    finish(&pool,&job,"cancelled").await;
                    assert_eq!(studies::delete(&pool,study.id).await.unwrap(),Some(studies::DeleteStudyOutcome::Deleted));
                }
                _ => panic!("queued delete/claim must have exactly one winner; running rows cannot disappear"),
            }
        }
    }).await;
}

#[tokio::test]
#[ignore = "requires isolated TEST_DATABASE_URL ending _test with CREATEDB"]
async fn restart_keeps_queued_specs_fails_unacknowledged_pause_and_never_autoplays_user_pause() {
    isolated(|pool| async move {
        let mut rows = Vec::new();
        for stage in 0..5 {
            let owner = format!("restart-owner-{stage}");
            let active = create(&pool,&owner,"active").await;
            let active_job = manual::claim(&pool,None).await.unwrap().unwrap();
            assert_eq!(active_job.study.id,active.id);
            let queued = create(&pool,&owner,"queued").await;
            let sequence = queue_sequence(&pool,queued.id).await;
            sqlx::query("update optimization_studies set manual_spent_ms=2000,manual_segment_started_at=clock_timestamp()-interval '3 seconds' where id=$1")
                .bind(active.id).execute(&pool).await.unwrap();
            if stage==0 {
                sqlx::query("update optimization_studies set manual_format_version=null where id=$1")
                    .bind(active.id).execute(&pool).await.unwrap();
            } else {
                changed(manual::control(&pool,&owner,active.id,revision(&active),false).await.unwrap());
                if stage>=2 { committed_manual_input(&pool,&active_job,stage>=3).await; }
                if stage==4 {
                    assert!(manual::pause_ack(&pool,&active_job,&progress(5.0),&json!([])).await.unwrap());
                }
            }
            rows.push((stage,active,queued,sequence));
        }
        assert_eq!(manual::recover(&pool).await.unwrap(),4,"already user-paused state has no runtime holder to recover");
        for (stage,active,queued,sequence) in rows {
            let recovered = studies::get(&pool,active.id).await.unwrap().unwrap();
            assert_eq!(recovered.status,if stage>=3 {"paused"} else {"failed"});
            if stage<3 {
                assert_eq!(recovered.error.unwrap()["code"],if stage==0 {"interrupted_by_restart"} else {"manual_pause_checkpoint_incomplete"});
            }
            let (spent,open,reserved):(i64,bool,bool) = sqlx::query_as("select manual_spent_ms,manual_segment_started_at is not null,manual_execution_reserved from optimization_studies where id=$1")
                .bind(active.id).fetch_one(&pool).await.unwrap();
            assert!(spent>=5000,"restart charges the previously open active segment");
            assert!(!open && !reserved);
            let same_queued = studies::get(&pool,queued.id).await.unwrap().unwrap();
            assert_eq!(same_queued.status,"queued","waiting is not an interrupted RAM-only execution");
            assert_eq!(same_queued.spec,queued.spec);
            assert_eq!(revision(&same_queued),revision(&queued));
            assert_eq!(queue_sequence(&pool,queued.id).await,sequence);
            assert!(same_queued.result.is_none() && same_queued.error.is_none());
            if stage>=3 {
                assert!(recovered.result.is_none(),"paused is not a fabricated terminal result");
            } else {
                assert_eq!(sqlx::query_scalar::<_,i64>("select count(*) from manual_runtime_artifacts where study_id=$1")
                    .bind(active.id).fetch_one(&pool).await.unwrap(),0,"failed private state is reclaimed");
            }
        }
        assert_eq!(manual::recover(&pool).await.unwrap(),0,"restart recovery is idempotent, not implicit Play");
    }).await;
}

#[tokio::test]
#[ignore = "requires isolated TEST_DATABASE_URL ending _test with CREATEDB"]
async fn manual_controls_preserve_continuous_user_pause_maintenance_config_retries_and_cursor() {
    isolated(|pool| async move {
        configure(&pool).await;
        let a = create(&pool,OWNER,"A").await;
        let b = create(&pool,OWNER,"B").await;
        assert!(scheduler::claim_owner(&pool,"queued-no-hot-loop").await.unwrap().is_none(),"queued manual blocks idle continuous lease admission");
        assert!(continuous::pause(&pool,OWNER,ALGORITHM).await.unwrap());
        maintenance::set_paused(&pool,OWNER,true).await.unwrap();
        sqlx::query("update continuous_processes set retry_count=2,next_slot=2,next_attempt_at=to_timestamp(123),error=$1 where owner_id=$2")
            .bind(json!({"code":"fixture-retained-retry","message":"retain exactly"})).bind(OWNER).execute(&pool).await.unwrap();
        let before = continuous_state(&pool).await;
        let claimed_a = manual::claim(&pool,None).await.unwrap().unwrap();
        assert_eq!(claimed_a.study.id,a.id,"continuous user-pause/maintenance are not manual Stop");
        committed_manual_input(&pool,&claimed_a,true).await;
        let pausing = changed(manual::control(&pool,OWNER,a.id,revision(&a),false).await.unwrap());
        assert_eq!(pausing.status,"pausing");
        assert!(manual::pause_ack(&pool,&claimed_a,&progress(0.0),&json!([])).await.unwrap());
        let claimed_b = manual::claim(&pool,None).await.unwrap().unwrap();
        assert_eq!(claimed_b.study.id,b.id);
        let paused = studies::get(&pool,a.id).await.unwrap().unwrap();
        changed(manual::control(&pool,OWNER,a.id,revision(&paused),true).await.unwrap());
        finish(&pool,&claimed_b,"completed").await;
        let resumed_a = manual::claim(&pool,None).await.unwrap().unwrap();
        assert_eq!(resumed_a.study.id,a.id);
        finish(&pool,&resumed_a,"completed").await;
        assert_eq!(continuous_state(&pool).await,before,"manual controls never call continuous Pause/Play/Start/Stop or reset its state");
        assert!(maintenance::status(&pool,OWNER).await.unwrap().admission_paused);
        assert!(continuous::get(&pool,OWNER,ALGORITHM).await.unwrap().unwrap().pause_requested);
        assert!(scheduler::claim_owner(&pool,"still-paused-after-manual").await.unwrap().is_none());
    }).await;
}

#[tokio::test]
#[ignore = "requires isolated TEST_DATABASE_URL ending _test with CREATEDB"]
async fn repeated_manual_seal_is_idempotent_and_changed_cutoff_is_a_typed_immutable_error() {
    isolated(|pool| async move {
        create(&pool, OWNER, "repeat-seal").await;
        let job = manual::claim(&pool, None).await.unwrap().unwrap();
        committed_manual_input(&pool, &job, true).await;
        let artifact = manual::artifact(&pool, &job, &manual::Limits::default()).await.unwrap().unwrap();
        let manifest = artifact.manifest.unwrap();
        let before: Value = sqlx::query_scalar("select to_jsonb(a) from manual_runtime_artifacts a where study_id=$1")
            .bind(job.study.id).fetch_one(&pool).await.unwrap();
        // A second acknowledgement of precisely the same immutable input must
        // return Ok, never panic because a selected column was omitted.
        manual::seal(&pool, &job, &manifest, 120, &manual::Limits::default()).await.unwrap();
        let unchanged: Value = sqlx::query_scalar("select to_jsonb(a) from manual_runtime_artifacts a where study_id=$1")
            .bind(job.study.id).fetch_one(&pool).await.unwrap();
        assert_eq!(unchanged, before);
        let error = manual::seal(&pool, &job, &manifest, 180, &manual::Limits::default()).await.unwrap_err();
        assert!(matches!(error, setups_db::DbError::Json(ref message) if message.contains("immutable")), "expected typed immutable-cutoff error, got {error:?}");
        let unchanged: Value = sqlx::query_scalar("select to_jsonb(a) from manual_runtime_artifacts a where study_id=$1")
            .bind(job.study.id).fetch_one(&pool).await.unwrap();
        assert_eq!(unchanged, before, "rejected cutoff cannot replace manifest, checkpoint, or any private state");
    }).await;
}
