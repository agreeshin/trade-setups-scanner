//! Opt-in DB-contract tests, not an API/executor end-to-end test. Each case
//! creates/drops its own child database; the supplied *_test database is only
//! the administrative connection. Lifecycle fixture SQL deliberately avoids IO.
use serde_json::{Value, json};
use setups_db::continuous::scheduler;
use setups_db::{continuous as db, optimization};
use setups_domain::{AlgorithmParams, SpikeParams, SpikeSetup};
use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions};
use std::{collections::BTreeSet, future::Future, str::FromStr, time::Duration};

const OWNER: &str = "admin";
const ALGORITHM: &str = "price_volume_spike";
const OTHER_ALGORITHM: &str = "price_drop_volume_spike";
const CUTOFF: i64 = 1_700_000_040;
const INSTRUMENT: &str = "bybit/linear/BTCUSDT";

struct TestDb {
    admin: PgPool,
    pool: PgPool,
    name: String,
}

impl TestDb {
    async fn new() -> Self {
        let options = PgConnectOptions::from_str(
            &std::env::var("TEST_DATABASE_URL").expect("TEST_DATABASE_URL required"),
        )
        .unwrap();
        assert!(
            options
                .get_database()
                .is_some_and(|name| name.ends_with("_test")),
            "refusing a database URL not ending in _test"
        );
        let admin = PgPoolOptions::new()
            .max_connections(2)
            .connect_with(options.clone())
            .await
            .unwrap();
        let name = format!(
            "continuous_it_{}_{}_test",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap()
        );
        assert!(name.len() < 63 && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'));
        sqlx::query(&format!("create database \"{name}\""))
            .execute(&admin)
            .await
            .unwrap();
        let pool = match PgPoolOptions::new()
            .max_connections(4)
            .connect_with(options.database(&name))
            .await
        {
            Ok(pool) => pool,
            Err(error) => {
                sqlx::query(&format!("drop database \"{name}\" with (force)"))
                    .execute(&admin)
                    .await
                    .unwrap();
                panic!("could not connect to isolated child database: {error}");
            }
        };
        Self { admin, pool, name }
    }

    async fn cleanup(self) {
        self.pool.close().await;
        sqlx::query(&format!("drop database \"{}\" with (force)", self.name))
            .execute(&self.admin)
            .await
            .unwrap();
        let exists: bool =
            sqlx::query_scalar("select exists(select 1 from pg_database where datname=$1)")
                .bind(&self.name)
                .fetch_one(&self.admin)
                .await
                .unwrap();
        assert!(!exists, "owned child database must be removed");
        self.admin.close().await;
    }
}

async fn isolated<F, Fut>(body: F)
where
    F: FnOnce(PgPool) -> Fut + 'static,
    Fut: Future<Output = ()> + 'static,
{
    let db = TestDb::new().await;
    let pool = db.pool.clone();
    // JoinError retains assertion panics so cleanup also runs on test failure,
    // including errors while applying the baseline or migrations.
    let local = tokio::task::LocalSet::new();
    let result = local.run_until(async move { tokio::task::spawn_local(async move {
        let baseline = include_str!("../migrations/0010_baseline.sql").lines()
            .filter(|line| !line.starts_with('\\')
                && !line.starts_with("SET transaction_timeout")
                && !line.contains("set_config('search_path'"))
            .collect::<Vec<_>>().join("\n");
        sqlx::raw_sql(&baseline).execute(&pool).await.unwrap();
        sqlx::raw_sql("set search_path to public; create table kysely_migration (name varchar(255) primary key, timestamp varchar(255) not null); insert into kysely_migration values ('0009_legacy','legacy')")
            .execute(&pool).await.unwrap();
        // Establish the actual pre-0013 schema, allowing sentinels to exist
        // BEFORE ensure_migrations applies the continuous migration.
        sqlx::raw_sql(include_str!("../migrations/0011_run_names.sql"))
            .execute(&pool).await.unwrap();
        sqlx::raw_sql(include_str!("../migrations/0012_optimization_studies.sql"))
            .execute(&pool).await.unwrap();
        sqlx::query("insert into kysely_migration values ('0011_run_names.sql','fixture'),('0012_optimization_studies.sql','fixture')")
            .execute(&pool).await.unwrap();
        body(pool).await;
    }).await }).await;
    db.cleanup().await;
    if let Err(error) = result {
        if error.is_panic() {
            std::panic::resume_unwind(error.into_panic());
        }
        panic!("isolated test task cancelled: {error}");
    }
}

fn spec(algorithm: &str) -> Value {
    json!({"algorithmId":algorithm,"algorithmVersion":"1.1.1","direction":"short",
        "instruments":[{"exchange":"bybit","category":"linear","symbol":"BTCUSDT"}],
        "fromUnix":CUTOFF-86400,"toUnix":CUTOFF+17,"seed":42,
        "algorithmParams":SpikeParams::default(),"analyticsParams":{"clusterEnabled":true,
        "clusterGapMinutes":480,"timeShiftMinutes":5,"priceChangePct":1}})
}

fn config(algorithm: &str) -> Value {
    json!({"adaptiveEnabled":true,"lookbackDays":1,"minSetupsPerDay":[4],
        "meanExpectancyThreshold":0.15,"study":spec(algorithm)})
}

async fn configured(pool: &PgPool, owner: &str, algorithm: &str) -> i64 {
    assert!(
        db::configure(pool, owner, algorithm, &config(algorithm), &[4.0])
            .await
            .unwrap()
    );
    assert!(db::start(pool, owner, algorithm).await.unwrap());
    db::get(pool, owner, algorithm)
        .await
        .unwrap()
        .unwrap()
        .slots[0]
        .id
}

async fn generation(pool: &PgPool, owner: &str, algorithm: &str, slot: i64, status: &str) -> i64 {
    sqlx::query_scalar("insert into continuous_generations(owner_id,algorithm_id,slot_id,config_revision,lease_token,status,spec,execution_started_at,observation_cutoff) values($1,$2,$3,1,1,$4,$5,to_timestamp($6),$7) returning id")
        .bind(owner).bind(algorithm).bind(slot).bind(status).bind(spec(algorithm))
        .bind((CUTOFF+17) as f64).bind(CUTOFF).fetch_one(pool).await.unwrap()
}

async fn study(
    pool: &PgPool,
    owner: &str,
    algorithm: &str,
    status: &str,
    generation: Option<i64>,
    slot: Option<i64>,
) -> i64 {
    sqlx::query_scalar("insert into optimization_studies(algorithm_id,status,spec,progress,result,error,origin,owner_id,continuous_generation_id,continuous_slot_id) values($1,$2,$3,'{}',$4,$5,$6,$7,$8,$9) returning id")
        .bind(algorithm).bind(status).bind(spec(algorithm))
        .bind((status=="completed").then(||json!({"best":null,"reason":"nonpass sentinel"})))
        .bind((status=="failed").then(||json!({"code":"previous_failure","message":"retain"})))
        .bind(if generation.is_some() { "continuous" } else { "manual" })
        .bind(owner).bind(generation).bind(slot).fetch_one(pool).await.unwrap()
}

async fn batch(pool: &PgPool, algorithm: &str, generation: Option<i64>) -> i32 {
    sqlx::query_scalar("insert into replay_batches(exchange_code,category,algorithm_id,algorithm_version,params,from_ts,to_ts,instrument_count,origin,owner_id,continuous_generation_id) values('bybit','linear',$1,'1.1.1','{}',to_timestamp(0),to_timestamp(3600),1,$2,$3,$4) returning id")
        .bind(algorithm).bind(if generation.is_some() { "continuous" } else { "manual" })
        .bind(OWNER).bind(generation).fetch_one(pool).await.unwrap()
}

async fn run(
    pool: &PgPool,
    batch: i32,
    algorithm: &str,
    status: &str,
    generation: Option<i64>,
) -> i32 {
    sqlx::query_scalar("insert into replay_runs(exchange_code,category,symbol,algorithm_id,algorithm_version,params,from_ts,to_ts,status,setups,batch_id,origin,owner_id,continuous_generation_id) values('bybit','linear','BTCUSDT',$1,'1.1.1','{}',to_timestamp(0),to_timestamp(3600),$2,$3,$4,$5,$6,$7) returning id")
        .bind(algorithm).bind(status).bind(json!([{"sentinel":status}])).bind(batch)
        .bind(if generation.is_some() { "continuous" } else { "manual" })
        .bind(OWNER).bind(generation).fetch_one(pool).await.unwrap()
}

async fn fixed_setup(pool: &PgPool, open: i64) -> i32 {
    sqlx::query_scalar("insert into live_setups(exchange_code,category,symbol,algorithm_id,algorithm_version,params,open_time,close_time,close_price,details,notified_at) values('bybit','linear','BTCUSDT',$1,'1.1.1',$2,to_timestamp($3),to_timestamp($4),'100',$5,to_timestamp(123)) returning id")
        .bind(ALGORITHM).bind(json!(SpikeParams::default())).bind(open as f64)
        .bind((open+60) as f64).bind(json!({"fixed":"retain"})).fetch_one(pool).await.unwrap()
}

async fn accepted(pool: &PgPool) -> db::ActiveAdaptiveSlot {
    let slot = configured(pool, OWNER, ALGORITHM).await;
    let generation = generation(pool, OWNER, ALGORITHM, slot, "completed").await;
    let study = study(
        pool,
        OWNER,
        ALGORITHM,
        "completed",
        Some(generation),
        Some(slot),
    )
    .await;
    let selected = json!({"tpPct":14,"slPct":4,"rawR":0.32,"robustScore":0.29});
    let result = json!({"best":{"params":SpikeParams::default(),"analytics":{"clusterEnabled":true,
        "clusterGapMinutes":480,"timeShiftMinutes":5,"priceChangePct":1}},
        "selectedCell":selected,"immutable":"accepted-profile"});
    sqlx::query(
        "update continuous_generations set study_id=$2,accepted=true,result=$3 where id=$1",
    )
    .bind(generation)
    .bind(study)
    .bind(result)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query("update continuous_slots set active_generation_id=$2,study_id=$3,mean_expectancy=0.21,selected_cell=$4,status='ready' where id=$1")
        .bind(slot).bind(generation).bind(study).bind(selected).execute(pool).await.unwrap();
    sqlx::query("update continuous_processes set active_generation_id=$3,active_study_id=$4 where owner_id=$1 and algorithm_id=$2")
        .bind(OWNER).bind(ALGORITHM).bind(generation).bind(study).execute(pool).await.unwrap();
    sqlx::query("insert into settings(key,value) values('live','{\"enabled\":true}'),($1,$2) on conflict(key) do update set value=excluded.value")
        .bind(format!("algorithm:{ALGORITHM}")).bind(json!({"externalName":"accepted external"}))
        .execute(pool).await.unwrap();
    db::active_adaptive_slots(pool, OWNER)
        .await
        .unwrap()
        .remove(0)
}

fn adaptive_write(open: i64) -> db::AdaptiveSetupWrite {
    db::AdaptiveSetupWrite {
        exchange_code: "bybit".into(),
        category: "linear".into(),
        symbol: "BTCUSDT".into(),
        setup: SpikeSetup {
            algorithm_id: ALGORITHM.into(),
            algorithm_version: "1.1.1".into(),
            open_time_unix: open,
            close_time_unix: open + 60,
            close_price: "100".into(),
            params: AlgorithmParams::Spike(SpikeParams::default()),
            details: None,
        },
        recommendation: json!({"direction":"short","tpPct":14,"slPct":4,"rawR":0.32,
            "meanR":0.21,"timeShiftMinutes":5,"priceChangePct":1,"rawSignalAt":open+60,
            "recommendedEntryAt":open+360,"externalName":"original recommendation"}),
    }
}

fn ids(value: &Value) -> BTreeSet<i64> {
    serde_json::from_value::<Vec<i64>>(value.clone())
        .unwrap()
        .into_iter()
        .collect()
}

async fn cleanup_owned(
    pool: &PgPool,
    lease: &scheduler::OwnerLease,
    algorithm: &str,
) -> Result<(), setups_db::DbError> {
    // The deleting transaction must validate this exact token before reading targets.
    db::cleanup_stopped(pool, &lease.owner_id, algorithm, lease)
        .await
        .map(|_| ())
}

struct PurgeFixture {
    active: db::ActiveAdaptiveSlot,
    batch: i32,
    run: i32,
    standalone: i32,
    signal: i64,
    lease: scheduler::OwnerLease,
}

async fn purge_fixture(pool:&PgPool)->PurgeFixture {
    setups_db::migrate::ensure_migrations(pool).await.unwrap();
    let active=accepted(pool).await;
    let b=batch(pool,ALGORITHM,Some(active.generation_id)).await;
    let r=run(pool,b,ALGORITHM,"done",Some(active.generation_id)).await;
    let standalone=run(pool,b,ALGORITHM,"done",Some(active.generation_id)).await;
    sqlx::query("update replay_runs set batch_id=null where id=$1").bind(standalone).execute(pool).await.unwrap();
    sqlx::query("update continuous_generations set replay_batch_id=$2,replay_manifest=$3,result=result||$4 where id=$1")
        .bind(active.generation_id).bind(b).bind(json!({"provenance":"frozen sentinel","instruments":[{"runId":r}]}))
        .bind(json!({"analytics":{"evaluation":{"matrix":[{"riskExpectancy":0.32}],"summary":{"score":0.32}},"sentinel":"independent post-chain"}})).execute(pool).await.unwrap();
    let second_slot=configured(pool,OWNER,OTHER_ALGORITHM).await;
    let second_generation=generation(pool,OWNER,OTHER_ALGORITHM,second_slot,"failed").await;
    let second_study=study(pool,OWNER,OTHER_ALGORITHM,"failed",Some(second_generation),Some(second_slot)).await;
    sqlx::query("update continuous_generations set study_id=$2 where id=$1").bind(second_generation).bind(second_study).execute(pool).await.unwrap();
    study(pool,OWNER,ALGORITHM,"completed",None,None).await;
    let manual_batch=batch(pool,ALGORITHM,None).await;
    run(pool,manual_batch,ALGORITHM,"done",None).await;
    let other_slot=configured(pool,"tenant-two",ALGORITHM).await;
    let other_generation=generation(pool,"tenant-two",ALGORITHM,other_slot,"completed").await;
    study(pool,"tenant-two",ALGORITHM,"completed",Some(other_generation),Some(other_slot)).await;
    sqlx::query("insert into adaptive_live_checkpoints(slot_id,instrument_key,generation_id,state) values($1,'other/sentinel',$2,'{}')").bind(other_slot).bind(other_generation).execute(pool).await.unwrap();
    sqlx::query("update adaptive_live_warmup_lane set slot_id=$1,generation_id=$2,lease_token=7,lease_owner='other-worker',lease_until=now()+interval '1 hour'").bind(other_slot).bind(other_generation).execute(pool).await.unwrap();
    setups_db::settings_set(pool,"notifications",&json!({"maxAgeSeconds":3600})).await.unwrap();
    setups_db::settings_set(pool,"optimization-draft:price_volume_spike",&json!({"keep":"shared draft"})).await.unwrap();
    let now=chrono::Utc::now().timestamp().div_euclid(60)*60;
    let signal=db::commit_adaptive_scan_cas(pool,&active,INSTRUMENT,None,&promotion_state(active.generation_id,now),&[adaptive_write(now-60)]).await.unwrap().inserted[0].id;
    db::outbox::commit_fixed_setup(pool,"bybit","linear","BTCUSDT",&adaptive_write(now-60).setup).await.unwrap().unwrap();
    sqlx::query("insert into replay_quality_intervals(run_id,from_unix,to_unix,reason) values($1,0,60,'sentinel'),($2,0,60,'sentinel')").bind(r).bind(standalone).execute(pool).await.unwrap();
    sqlx::query("insert into replay_cache(cache_key,exchange_code,category,symbol,algorithm_id,algorithm_version,params,from_ts,to_ts,setups,warmup_minutes,processed_minutes,quality_source_run_id) values('shared','bybit','linear','BTCUSDT',$1,'1.1.1','{}',to_timestamp(0),to_timestamp(3600),'[]',1,1,$2)").bind(ALGORITHM).bind(r).execute(pool).await.unwrap();
    sqlx::raw_sql("insert into analytics_selection_cache(scope_key,maturity_tenths,month_key,algorithm_version,result_json) values('batch:sentinel',0,'all',1,'{\"keep\":true}');insert into analytics_matrix_cache(scope_key,direction,maturity_tenths,month_key,tp_pct,sl_pct,horizon_minutes,time_shift_minutes,price_change_ppm,algorithm_version,result_json) values('batch:sentinel','short',0,'all',1,1,1,0,-1,1,'{\"keep\":true}');insert into analytics_trades_cache(scope_key,direction,maturity_tenths,exchange_code,category,symbol,close_time_unix,tp_pct,sl_pct,horizon_minutes,time_shift_minutes,price_change_ppm,algorithm_version,result_json) values('batch:sentinel','short',0,'bybit','linear','BTCUSDT',60,1,1,1,0,-1,1,'{\"keep\":true}')").execute(pool).await.unwrap();
    for algorithm in [ALGORITHM,OTHER_ALGORITHM] {db::stop(pool,OWNER,algorithm).await.unwrap();}
    let lease=scheduler::claim_owner(pool,"purge-fixture").await.unwrap().unwrap();
    assert_eq!(lease.owner_id,OWNER);
    for algorithm in [ALGORITHM,OTHER_ALGORITHM] {cleanup_owned(pool,&lease,algorithm).await.unwrap();}
    PurgeFixture {active,batch:b,run:r,standalone,signal,lease}
}

async fn purge_preserved(pool:&PgPool)->Value {
    sqlx::query_scalar("select jsonb_build_object('studies',(select jsonb_agg(to_jsonb(s) order by id) from optimization_studies s where origin='manual' or owner_id<>'admin'),'settings',(select jsonb_agg(to_jsonb(s) order by key) from settings s),'cache',(select jsonb_agg(to_jsonb(c) order by cache_key) from replay_cache c),'selection',(select jsonb_agg(to_jsonb(c)) from analytics_selection_cache c),'matrix',(select jsonb_agg(to_jsonb(c)) from analytics_matrix_cache c),'trades',(select jsonb_agg(to_jsonb(c)) from analytics_trades_cache c),'quality',(select jsonb_agg(to_jsonb(q) order by run_id) from replay_quality_intervals q),'foreignSlots',(select jsonb_agg(to_jsonb(s) order by id) from continuous_slots s where owner_id<>'admin'),'foreignCheckpoints',(select jsonb_agg(to_jsonb(c) order by slot_id) from adaptive_live_checkpoints c join continuous_slots s on s.id=c.slot_id where s.owner_id<>'admin'),'warmup',(select to_jsonb(w) from adaptive_live_warmup_lane w),'fixedOutbox',(select jsonb_agg(to_jsonb(o) order by id) from adaptive_notification_outbox o where slot_id is null or owner_id<>'admin'))").fetch_one(pool).await.unwrap()
}

#[tokio::test]
#[ignore="requires isolated TEST_DATABASE_URL ending in _test with CREATEDB"]
async fn history_purge_preserves_mixed_outputs_analytics_identity_and_supports_restart_and_row_delete() {
    use db::cleanup::*;
    isolated(|pool|async move {
        let f=purge_fixture(&pool).await;
        let original=purge_preserved(&pool).await;
        let run_before=setups_db::runs::get_run_full(&pool,i64::from(f.run)).await.unwrap().unwrap();
        let batch_before=setups_db::runs::get_replay_batch(&pool,i64::from(f.batch)).await.unwrap().unwrap();
        let live_before=setups_db::runs::live_setups_search(&pool,i64::from(i32::MAX),None,0,100).await.unwrap();
        let config_before=db::get(&pool,OWNER,ALGORITHM).await.unwrap().unwrap();
        let p=preview(&pool,OWNER).await.unwrap();
        assert!(p.eligible,"{:?}",p.blockers);
        assert_eq!((p.counts.studies,p.counts.generations,p.counts.notification_jobs),(2,2,1));
        assert_eq!((p.retained.runs,p.retained.batches,p.retained.analytics_results),(2,1,1));
        assert!(matches!(optimization::delete(&pool,f.active.study_id).await.unwrap(),Some(optimization::DeleteStudyOutcome::Referenced)));
        assert!(matches!(setups_db::runs::delete_replay_batch(&pool,i64::from(f.batch)).await.unwrap(),Some(setups_db::runs::BatchDeleteOutcome::Referenced)));
        let Outcome::Deleted(done)=purge(&pool,OWNER,&p.fingerprint).await.unwrap() else {panic!("purge rejected")};
        assert_eq!(done.deleted,p.counts);
        assert_eq!(purge_preserved(&pool).await,original);
        assert_eq!(setups_db::runs::get_run_full(&pool,i64::from(f.run)).await.unwrap().unwrap(),run_before);
        let batch_after=setups_db::runs::get_replay_batch(&pool,i64::from(f.batch)).await.unwrap().unwrap();
        assert_eq!((batch_after.continuous_study_id,batch_after.continuous_generation_id,batch_after.setup_count),(batch_before.continuous_study_id,batch_before.continuous_generation_id,batch_before.setup_count));
        assert_eq!(setups_db::runs::live_setups_search(&pool,i64::from(i32::MAX),None,0,100).await.unwrap(),live_before);
        let archives:Value=sqlx::query_scalar("select jsonb_build_object('meta',continuous_archive,'analytics',continuous_analytics_archive,'replay',continuous_replay_archive) from replay_batches where id=$1").bind(f.batch).fetch_one(&pool).await.unwrap();
        assert_eq!(archives["meta"]["generationId"],f.active.generation_id);
        assert_eq!(archives["analytics"]["sentinel"],"independent post-chain");
        assert_eq!(archives["replay"]["provenance"],"frozen sentinel");
        assert!(archives["meta"].get("best").is_none() && archives["meta"].get("bootstrap").is_none());
        assert_eq!(sqlx::query_scalar::<_,i64>("select count(*) from continuous_generations where owner_id='admin'").fetch_one(&pool).await.unwrap(),0);
        assert_eq!(sqlx::query_scalar::<_,String>("select notification_delivery_status from live_setups where id::bigint=$1").bind(f.signal).fetch_one(&pool).await.unwrap(),"skipped");
        let budget:Value=sqlx::query_scalar("select jsonb_build_object('jobs',pending_jobs,'actual',(select count(*) from adaptive_notification_outbox),'bytes',pending_bytes,'actualBytes',(select coalesce(sum(octet_length(payload::text)),0) from adaptive_notification_outbox)) from adaptive_notification_budget").fetch_one(&pool).await.unwrap();
        assert_eq!(budget["jobs"],budget["actual"]);assert_eq!(budget["bytes"],budget["actualBytes"]);
        let after=db::get(&pool,OWNER,ALGORITHM).await.unwrap().unwrap();
        assert_eq!(after.config,config_before.config);assert!(after.config_revision>config_before.config_revision);
        assert_eq!(after.slots[0].id,config_before.slots[0].id);assert!(after.slots[0].study_id.is_none()&&after.slots[0].selected_cell.is_none());
        let again=preview(&pool,OWNER).await.unwrap();assert_eq!(again.counts,Counts::default());
        assert!(matches!(purge(&pool,OWNER,&again.fingerprint).await.unwrap(),Outcome::Deleted(Completed{deleted,..}) if deleted==Counts::default()));
        assert!(!scheduler::heartbeat(&pool,&f.lease).await.unwrap());
        assert!(db::start(&pool,OWNER,ALGORITHM).await.unwrap());
        let lease=scheduler::claim_owner(&pool,"after-purge").await.unwrap().unwrap();
        let next=scheduler::prepare_next(&pool,&lease).await.unwrap().unwrap();
        assert!(next.generation_id>f.active.generation_id);assert_eq!(next.slot_id,f.active.slot_id);
        // Publish under the new fence but with an old signal identity: slot dedup
        // retains the existing signal and creates no replacement outbox job.
        sqlx::query("update continuous_generations set status='completed',accepted=true,result=$2 where id=$1").bind(next.generation_id).bind(&f.active.result).execute(&pool).await.unwrap();
        sqlx::query("update continuous_slots set active_generation_id=$2 where id=$1").bind(next.slot_id).bind(next.generation_id).execute(&pool).await.unwrap();
        let mut new_profile=f.active.clone();new_profile.generation_id=next.generation_id;new_profile.study_id=next.study_id;new_profile.config_revision=next.config_revision;
        let old_open=live_before.1.iter().find(|s|s.id==f.signal).unwrap().open_time.timestamp();
        let replayed=db::commit_adaptive_scan_cas(&pool,&new_profile,INSTRUMENT,None,&promotion_state(next.generation_id,old_open+60),&[adaptive_write(old_open)]).await.unwrap();
        assert!(replayed.applied&&replayed.inserted.is_empty());
        assert!(!db::commit_adaptive_scan_cas(&pool,&f.active,INSTRUMENT,None,&promotion_state(f.active.generation_id,old_open+60),&[]).await.unwrap().applied);
        assert!(matches!(setups_db::admission::delete_replay_run(&pool,i64::from(f.standalone)).await.unwrap(),Some(setups_db::admission::DeleteRunOutcome::Deleted{..})));
        assert!(matches!(setups_db::runs::delete_replay_batch(&pool,i64::from(f.batch)).await.unwrap(),Some(setups_db::runs::BatchDeleteOutcome::Deleted{runs_deleted:1,..})));
        assert_eq!(sqlx::query_scalar::<_,i64>("select count(*) from replay_cache").fetch_one(&pool).await.unwrap(),1);
        assert_eq!(sqlx::query_scalar::<_,i64>("select count(*) from analytics_matrix_cache").fetch_one(&pool).await.unwrap(),1);
    }).await;
}

#[tokio::test]
#[ignore="requires isolated TEST_DATABASE_URL ending in _test with CREATEDB"]
async fn history_purge_guards_active_work_claimed_delivery_stale_preview_and_rolls_back_failure() {
    use db::cleanup::*;
    isolated(|pool|async move {
        let f=purge_fixture(&pool).await;
        let p=preview(&pool,OWNER).await.unwrap();
        db::start(&pool,OWNER,ALGORITHM).await.unwrap();
        assert!(matches!(purge(&pool,OWNER,&p.fingerprint).await.unwrap(),Outcome::Blocked(_)));
        db::stop(&pool,OWNER,ALGORITHM).await.unwrap();cleanup_owned(&pool,&f.lease,ALGORITHM).await.unwrap();
        assert!(matches!(purge(&pool,OWNER,&p.fingerprint).await.unwrap(),Outcome::Stale(_)));
        sqlx::query("update adaptive_notification_outbox set lease_owner='in-flight',lease_until=now()+interval '1 minute' where slot_id is not null").execute(&pool).await.unwrap();
        let claimed=preview(&pool,OWNER).await.unwrap();assert!(!claimed.eligible);
        sqlx::query("update adaptive_notification_outbox set lease_until=now()-interval '1 second' where slot_id is not null").execute(&pool).await.unwrap();
        let p=preview(&pool,OWNER).await.unwrap();assert!(p.eligible);
        let all_before:Value=sqlx::query_scalar("select jsonb_build_object('processes',(select jsonb_agg(to_jsonb(p) order by owner_id,algorithm_id) from continuous_processes p),'slots',(select jsonb_agg(to_jsonb(s) order by id) from continuous_slots s),'generations',(select jsonb_agg(to_jsonb(g) order by id) from continuous_generations g),'live',(select jsonb_agg(to_jsonb(l) order by id) from live_setups l),'outbox',(select jsonb_agg(to_jsonb(o) order by id) from adaptive_notification_outbox o),'budget',(select to_jsonb(b) from adaptive_notification_budget b),'batches',(select jsonb_agg(to_jsonb(b) order by id) from replay_batches b))").fetch_one(&pool).await.unwrap();
        sqlx::raw_sql("create function reject_history_purge() returns trigger language plpgsql as $$begin raise exception 'history_purge_rollback_sentinel';end$$;create trigger reject_history_purge before delete on continuous_generations for each row execute function reject_history_purge()") .execute(&pool).await.unwrap();
        assert!(purge(&pool,OWNER,&p.fingerprint).await.err().unwrap().to_string().contains("history_purge_rollback_sentinel"));
        let all_after:Value=sqlx::query_scalar("select jsonb_build_object('processes',(select jsonb_agg(to_jsonb(p) order by owner_id,algorithm_id) from continuous_processes p),'slots',(select jsonb_agg(to_jsonb(s) order by id) from continuous_slots s),'generations',(select jsonb_agg(to_jsonb(g) order by id) from continuous_generations g),'live',(select jsonb_agg(to_jsonb(l) order by id) from live_setups l),'outbox',(select jsonb_agg(to_jsonb(o) order by id) from adaptive_notification_outbox o),'budget',(select to_jsonb(b) from adaptive_notification_budget b),'batches',(select jsonb_agg(to_jsonb(b) order by id) from replay_batches b))").fetch_one(&pool).await.unwrap();
        assert_eq!(all_after,all_before);
        assert_eq!(preview(&pool,OWNER).await.unwrap(),p);
    }).await;
}

#[tokio::test]
#[ignore="requires isolated TEST_DATABASE_URL ending in _test with CREATEDB"]
async fn history_purge_preserves_analytics_null_missing_and_rejects_cross_owner_or_collapsed_results() {
    use db::cleanup::*;
    isolated(|pool|async move {
        let f=purge_fixture(&pool).await;
        sqlx::query("update continuous_generations set result=jsonb_set(result,'{analytics}','null') where id=$1").bind(f.active.generation_id).execute(&pool).await.unwrap();
        let p=preview(&pool,OWNER).await.unwrap();assert_eq!(p.retained.analytics_results,1);
        assert!(matches!(purge(&pool,OWNER,&p.fingerprint).await.unwrap(),Outcome::Deleted(_)));
        assert_eq!(sqlx::query_scalar::<_,Option<Value>>("select continuous_analytics_archive from replay_batches where id=$1").bind(f.batch).fetch_one(&pool).await.unwrap(),Some(Value::Null));
        let slot=f.active.slot_id;
        let g=generation(&pool,OWNER,ALGORITHM,slot,"completed").await;
        let b=batch(&pool,ALGORITHM,Some(g)).await;
        sqlx::query("update continuous_generations set replay_batch_id=$2,result='{}',spec='{}' where id=$1").bind(g).bind(b).execute(&pool).await.unwrap();
        let p=preview(&pool,OWNER).await.unwrap();assert_eq!(p.retained.analytics_results,1);
        assert!(matches!(purge(&pool,OWNER,&p.fingerprint).await.unwrap(),Outcome::Deleted(_)));
        assert_eq!(sqlx::query_scalar::<_,Option<Value>>("select continuous_analytics_archive from replay_batches where id=$1").bind(b).fetch_one(&pool).await.unwrap(),None);
        assert_eq!(setups_db::runs::replay_batch_archive(&pool,i64::from(b)).await.unwrap().unwrap()["context"],json!({"effectiveAnalytics":null,"direction":null}));
        let g=generation(&pool,OWNER,ALGORITHM,slot,"completed").await;
        sqlx::query("update continuous_generations set replay_batch_id=$2,result='{\"analytics\":{\"different\":true}}' where id=$1").bind(g).bind(f.batch).execute(&pool).await.unwrap();
        assert!(preview(&pool,OWNER).await.unwrap().blockers.contains(&"ownership_conflict".into()),"cannot overwrite/collapse another generation's archived result");
        sqlx::query("update continuous_generations set result='{}',replay_batch_id=null where id=$1").bind(g).execute(&pool).await.unwrap();
        let bad=study(&pool,"tenant-two",ALGORITHM,"completed",Some(g),None).await;
        let p=preview(&pool,OWNER).await.unwrap();assert!(!p.eligible);
        assert!(matches!(purge(&pool,OWNER,&p.fingerprint).await.unwrap(),Outcome::Blocked(_)));
        assert!(optimization::get(&pool,bad).await.unwrap().is_some());
    }).await;
}

#[tokio::test]
#[ignore="requires isolated TEST_DATABASE_URL ending in _test with CREATEDB"]
async fn history_purge_owner_fence_serializes_new_algorithm_insert() {
    use db::cleanup::*;
    isolated(|pool|async move {
        purge_fixture(&pool).await;
        let p=preview(&pool,OWNER).await.unwrap();
        let mut gate=pool.begin().await.unwrap();
        sqlx::query("select pg_advisory_xact_lock(4938261)").execute(&mut *gate).await.unwrap();
        sqlx::raw_sql("create function gate_history_purge() returns trigger language plpgsql as $$begin perform pg_advisory_xact_lock(4938261);return old;end$$;create trigger gate_history_purge before delete on continuous_generations for each row execute function gate_history_purge()") .execute(&pool).await.unwrap();
        let task_pool=pool.clone();
        let purge_task=tokio::spawn(async move {purge(&task_pool,OWNER,&p.fingerprint).await});
        let reached=tokio::time::timeout(Duration::from_secs(3),async {loop {
            if sqlx::query_scalar::<_,bool>("select exists(select 1 from pg_locks where locktype='advisory' and objid=4938261 and not granted)").fetch_one(&pool).await.unwrap(){break;}
            tokio::time::sleep(Duration::from_millis(10)).await;
        }}).await;
        let configure_pool=pool.clone();
        let configure_task=tokio::spawn(async move {db::configure(&configure_pool,OWNER,"new-algorithm",&config("new-algorithm"),&[4.0]).await});
        tokio::time::sleep(Duration::from_millis(50)).await;
        let serialized=!configure_task.is_finished();
        // Release and join before assertions, including an unexpected old-code failure.
        gate.rollback().await.unwrap();
        let purge_result=purge_task.await.unwrap().unwrap();
        let configured=configure_task.await.unwrap().unwrap();
        assert!(reached.is_ok()&&serialized,"new algorithm insertion escaped the existing owner fence");
        assert!(matches!(purge_result,Outcome::Deleted(_))&&configured);
        let saved=db::get(&pool,OWNER,"new-algorithm").await.unwrap().unwrap();
        assert_eq!(saved.config,config("new-algorithm"));assert_eq!(saved.slots.len(),1);
    }).await;
}

#[tokio::test]
#[ignore="requires isolated TEST_DATABASE_URL ending in _test with CREATEDB"]
async fn history_purge_nonpass_archives_exact_effective_analytics_once_without_live_fallback() {
    use db::cleanup::*;
    isolated(|pool|async move {
        setups_db::migrate::ensure_migrations(&pool).await.unwrap();
        assert!(db::configure(&pool,OWNER,ALGORITHM,&config(ALGORITHM),&[4.0]).await.unwrap());
        let slot=db::get(&pool,OWNER,ALGORITHM).await.unwrap().unwrap().slots[0].id;
        let g=generation(&pool,OWNER,ALGORITHM,slot,"completed").await;
        let s=study(&pool,OWNER,ALGORITHM,"completed",Some(g),Some(slot)).await;
        let b=batch(&pool,ALGORITHM,Some(g)).await;
        run(&pool,b,ALGORITHM,"done",Some(g)).await;
        run(&pool,b,ALGORITHM,"done",Some(g)).await;
        let effective=json!({"clusterEnabled":true,"clusterGapMinutes":17,"timeShiftMinutes":11,"priceChangePct":-2.5,"setupMaturity":0.0,"horizonMinutes":123});
        let analytics=json!({"evaluation":{"matrix":[{"tpPct":2,"slPct":3,"score":-0.7}],"summary":{"score":-0.7}},"context":{"observationCutoff":CUTOFF}});
        let result=json!({"best":{"analytics":effective,"params":{"doNotArchive":"research winner"}},"analytics":analytics,"bootstrap":{"doNotArchive":true}});
        sqlx::query("update continuous_generations set study_id=$2,replay_batch_id=$3,result=$4,spec=jsonb_set(spec,'{direction}','\"long\"'),accepted=false where id=$1").bind(g).bind(s).bind(b).bind(result).execute(&pool).await.unwrap();
        assert_eq!(sqlx::query_scalar::<_,i64>("select count(*) from live_setups").fetch_one(&pool).await.unwrap(),0);
        let p=preview(&pool,OWNER).await.unwrap();assert!(p.eligible);
        assert_eq!((p.retained.runs,p.retained.analytics_results),(2,1));
        assert!(matches!(purge(&pool,OWNER,&p.fingerprint).await.unwrap(),Outcome::Deleted(_)));
        let archived=setups_db::runs::replay_batch_archive(&pool,i64::from(b)).await.unwrap().unwrap();
        assert_eq!(archived["analytics"],analytics);
        assert_eq!(archived["context"],json!({"effectiveAnalytics":effective,"direction":"long"}));
        assert_eq!(archived["analyticsPresent"],true);
        assert_eq!(archived["provenance"]["studyId"],s);
        assert!(archived.get("best").is_none()&&archived.get("spec").is_none()&&archived.get("bootstrap").is_none());
        assert_eq!(sqlx::query_scalar::<_,i64>("select count(*) from replay_batches where continuous_analytics_archive is not null").fetch_one(&pool).await.unwrap(),1);
        assert_eq!(sqlx::query_scalar::<_,i64>("select count(*) from replay_runs where continuous_archive ? 'analytics' or continuous_archive ? 'effectiveAnalytics'").fetch_one(&pool).await.unwrap(),0);
    }).await;
}

#[tokio::test]
#[ignore = "requires isolated TEST_DATABASE_URL ending in _test with CREATEDB"]
async fn migration_preserves_fixed_manual_history_and_separates_active_lanes() {
    isolated(|pool| async move {
        let fixed = fixed_setup(&pool, CUTOFF-60).await;
        let manual: i64 = sqlx::query_scalar("insert into optimization_studies(algorithm_id,spec,progress) values($1,$2,$3) returning id")
            .bind(ALGORITHM).bind(json!({"legacy":"spec untouched"})).bind(json!({"phase":"queued","bestScore":null}))
            .fetch_one(&pool).await.unwrap();
        let fixed_before: Value = sqlx::query_scalar("select to_jsonb(s) from live_setups s where id=$1")
            .bind(fixed).fetch_one(&pool).await.unwrap();
        let manual_before: Value = sqlx::query_scalar("select to_jsonb(s) from optimization_studies s where id=$1")
            .bind(manual).fetch_one(&pool).await.unwrap();
        setups_db::migrate::ensure_migrations(&pool).await.unwrap();
        assert_eq!(setups_db::migrate::ensure_migrations(&pool).await.unwrap(), "0 applied, 8 up to date");
        let fixed_after: Value = sqlx::query_scalar("select to_jsonb(s)-array['adaptive_slot_id','adaptive_generation_id','recommendation','external_name','notification_delivery_status','notification_delivery_error','notification_delivery_updated_at','adaptive_generation_archive'] from live_setups s where id=$1")
            .bind(fixed).fetch_one(&pool).await.unwrap();
        let manual_after: Value = sqlx::query_scalar("select to_jsonb(s)-array['origin','owner_id','continuous_generation_id','continuous_slot_id'] from optimization_studies s where id=$1")
            .bind(manual).fetch_one(&pool).await.unwrap();
        assert_eq!(fixed_after, fixed_before);
        assert_eq!(manual_after, manual_before);
        let slot = configured(&pool, OWNER, ALGORITHM).await;
        let generation = generation(&pool, OWNER, ALGORITHM, slot, "running").await;
        let continuous = study(&pool, OWNER, ALGORITHM, "running", Some(generation), Some(slot)).await;
        assert_eq!(optimization::get(&pool, manual).await.unwrap().unwrap().origin, "manual");
        assert_eq!(optimization::get(&pool, continuous).await.unwrap().unwrap().origin, "continuous");
        assert_eq!(sqlx::query_scalar::<_,i64>("select count(*) from optimization_studies where status in ('queued','running')")
            .fetch_one(&pool).await.unwrap(), 2);
        assert!(matches!(optimization::create(&pool, ALGORITHM, &spec(ALGORITHM), &json!({})).await.unwrap(), optimization::Admission::Busy(id) if id==manual));
        let error = sqlx::query("insert into continuous_slots(owner_id,algorithm_id,min_setups_per_day) values($1,$2,-1)")
            .bind(OWNER).bind(ALGORITHM).execute(&pool).await.unwrap_err();
        assert_eq!(error.as_database_error().unwrap().code().as_deref(), Some("23514"));
        let error = sqlx::query("insert into adaptive_live_checkpoints(slot_id,instrument_key,generation_id,state) values($1,$2,$3,'{}')")
            .bind(i64::MAX).bind(INSTRUMENT).bind(generation).execute(&pool).await.unwrap_err();
        assert_eq!(error.as_database_error().unwrap().code().as_deref(), Some("23503"));
        let error = sqlx::query("insert into continuous_generations(owner_id,algorithm_id,slot_id,config_revision,lease_token,status,spec,execution_started_at,observation_cutoff) values($1,$2,$3,1,1,'failed','{}',to_timestamp($4),$5)")
            .bind(OWNER).bind(ALGORITHM).bind(slot).bind(CUTOFF as f64).bind(CUTOFF+60)
            .execute(&pool).await.unwrap_err();
        assert_eq!(error.as_database_error().unwrap().code().as_deref(), Some("23514"));
    }).await;
}

#[tokio::test]
#[ignore = "requires isolated TEST_DATABASE_URL ending in _test with CREATEDB"]
async fn permission_only_configure_preserves_revision_profile_but_disables_live() {
    isolated(|pool| async move {
        setups_db::migrate::ensure_migrations(&pool).await.unwrap();
        let profile = accepted(&pool).await;
        assert!(
            db::adaptive_notification_allowed(&pool, &profile)
                .await
                .unwrap()
        );
        let before = db::get(&pool, OWNER, ALGORITHM).await.unwrap().unwrap();
        let mut disabled = before.config.clone();
        disabled["adaptiveEnabled"] = json!(false);
        assert!(
            db::configure(&pool, OWNER, ALGORITHM, &disabled, &[4.0])
                .await
                .unwrap()
        );
        let after = db::get(&pool, OWNER, ALGORITHM).await.unwrap().unwrap();
        assert_eq!(after.config_revision, before.config_revision);
        assert_eq!(after.status, before.status);
        assert_eq!(
            after.slots[0].active_generation_id,
            Some(profile.generation_id)
        );
        assert_eq!(after.slots[0].selected_cell, before.slots[0].selected_cell);
        assert!(
            db::active_adaptive_slots(&pool, OWNER)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            !db::adaptive_notification_allowed(&pool, &profile)
                .await
                .unwrap()
        );
        let blocked = db::commit_adaptive_scan_cas(
            &pool,
            &profile,
            INSTRUMENT,
            None,
            &json!({"nextOpenUnix":CUTOFF}),
            &[adaptive_write(CUTOFF - 60)],
        )
        .await
        .unwrap();
        assert!(!blocked.applied && blocked.inserted.is_empty());
        assert!(
            db::get_adaptive_checkpoint(&pool, profile.slot_id, INSTRUMENT)
                .await
                .unwrap()
                .is_none()
        );
        let retained: Value =
            sqlx::query_scalar("select result from continuous_generations where id=$1")
                .bind(profile.generation_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(retained, profile.result);
        let mut changed = disabled;
        changed["lookbackDays"] = json!(2);
        assert!(
            !db::configure(&pool, OWNER, ALGORITHM, &changed, &[4.0])
                .await
                .unwrap()
        );
        assert!(
            db::configure(&pool, OWNER, ALGORITHM, &before.config, &[4.0])
                .await
                .unwrap()
        );
        assert_eq!(
            db::get(&pool, OWNER, ALGORITHM)
                .await
                .unwrap()
                .unwrap()
                .config_revision,
            before.config_revision
        );
        assert_eq!(
            db::active_adaptive_slots(&pool, OWNER).await.unwrap()[0].generation_id,
            profile.generation_id
        );
        assert!(
            db::adaptive_notification_allowed(&pool, &profile)
                .await
                .unwrap()
        );
    })
    .await;
}

#[tokio::test]
#[ignore = "requires isolated TEST_DATABASE_URL ending in _test with CREATEDB"]
async fn adaptive_cas_is_atomic_fenced_and_deduplicates_across_generations() {
    isolated(|pool| async move {
        setups_db::migrate::ensure_migrations(&pool).await.unwrap();
        let profile = accepted(&pool).await;
        let open = CUTOFF-60;
        let fixed = fixed_setup(&pool, open).await;
        let row = adaptive_write(open);
        let original_recommendation = row.recommendation.clone();
        let checkpoint = json!({"nextOpenUnix":CUTOFF,"lastRawOpenUnix":open,"lastRawClusterTime":CUTOFF});
        let bogus = db::AdaptiveCheckpoint { generation_id:profile.generation_id, state:json!({"stale":true}) };
        assert!(!db::commit_adaptive_scan_cas(&pool, &profile, INSTRUMENT, Some(&bogus), &checkpoint, std::slice::from_ref(&row)).await.unwrap().applied);
        let mut stale = profile.clone();
        stale.generation_id += 1000;
        assert!(!db::commit_adaptive_scan_cas(&pool, &stale, INSTRUMENT, None, &checkpoint, std::slice::from_ref(&row)).await.unwrap().applied);
        sqlx::query("update settings set value='{\"enabled\":false}' where key='live'").execute(&pool).await.unwrap();
        assert!(!db::commit_adaptive_scan_cas(&pool, &profile, INSTRUMENT, None, &checkpoint, std::slice::from_ref(&row)).await.unwrap().applied);
        assert_eq!(sqlx::query_scalar::<_,i64>("select count(*) from live_setups where adaptive_slot_id is not null").fetch_one(&pool).await.unwrap(), 0);
        assert!(db::get_adaptive_checkpoint(&pool, profile.slot_id, INSTRUMENT).await.unwrap().is_none());
        sqlx::query("update settings set value='{\"enabled\":true}' where key='live'").execute(&pool).await.unwrap();
        let committed = db::commit_adaptive_scan_cas(&pool, &profile, INSTRUMENT, None, &checkpoint, std::slice::from_ref(&row)).await.unwrap();
        assert!(committed.applied);
        assert_eq!(committed.inserted.len(), 1);
        assert_eq!(committed.inserted[0].index, 0);
        let setup_id = committed.inserted[0].id;
        let previous = db::get_adaptive_checkpoint(&pool, profile.slot_id, INSTRUMENT).await.unwrap().unwrap();
        assert_eq!(previous, db::AdaptiveCheckpoint { generation_id:profile.generation_id, state:checkpoint.clone() });
        let stored: Value = sqlx::query_scalar("select to_jsonb(s) from live_setups s where id::bigint=$1")
            .bind(setup_id).fetch_one(&pool).await.unwrap();
        assert_eq!(stored["recommendation"], original_recommendation);
        assert_eq!(stored["params"], json!(SpikeParams::default()));
        assert_eq!(stored["adaptive_generation_id"], profile.generation_id);
        assert_eq!(stored["external_name"], "accepted external");
        assert_eq!(sqlx::query_scalar::<_,i64>("select count(*) from live_setups").fetch_one(&pool).await.unwrap(), 2);
        assert_eq!(sqlx::query_scalar::<_,String>("select close_price from live_setups where id=$1").bind(fixed).fetch_one(&pool).await.unwrap(), "100");
        // Valid first row would be inserted, but invalid second algorithm must
        // roll back BOTH that insertion and the proposed checkpoint advance.
        let valid_new = adaptive_write(CUTOFF+60);
        let mut invalid_second = adaptive_write(CUTOFF+120);
        invalid_second.setup.algorithm_id = "wrong_algorithm".into();
        assert!(db::commit_adaptive_scan_cas(&pool, &profile, INSTRUMENT, Some(&previous), &json!({"nextOpenUnix":CUTOFF+180}), &[valid_new,invalid_second]).await.is_err());
        assert_eq!(db::get_adaptive_checkpoint(&pool, profile.slot_id, INSTRUMENT).await.unwrap().unwrap(), previous);
        assert_eq!(sqlx::query_scalar::<_,i64>("select count(*) from live_setups").fetch_one(&pool).await.unwrap(), 2);
        // Existing cursor cannot be overwritten by an absent or stale cursor.
        assert!(!db::commit_adaptive_scan_cas(&pool, &profile, INSTRUMENT, None, &json!({"bad":true}), &[]).await.unwrap().applied);
        let next_generation = generation(&pool, OWNER, ALGORITHM, profile.slot_id, "completed").await;
        sqlx::query("update continuous_generations set study_id=$2,result=$3,accepted=true where id=$1")
            .bind(next_generation).bind(profile.study_id).bind(&profile.result).execute(&pool).await.unwrap();
        sqlx::query("update continuous_slots set active_generation_id=$2 where id=$1")
            .bind(profile.slot_id).bind(next_generation).execute(&pool).await.unwrap();
        sqlx::query("update continuous_processes set active_generation_id=$3 where owner_id=$1 and algorithm_id=$2")
            .bind(OWNER).bind(ALGORITHM).bind(next_generation).execute(&pool).await.unwrap();
        let promoted = db::active_adaptive_slots(&pool, OWNER).await.unwrap().remove(0);
        let mut duplicate = row;
        duplicate.recommendation["externalName"] = json!("must not rewrite prior signal");
        let next_checkpoint = json!({"nextOpenUnix":CUTOFF+60});
        let duplicate_commit = db::commit_adaptive_scan_cas(&pool, &promoted, INSTRUMENT, Some(&previous), &next_checkpoint, &[duplicate]).await.unwrap();
        assert!(duplicate_commit.applied && duplicate_commit.inserted.is_empty());
        let persisted: Value = sqlx::query_scalar("select to_jsonb(s) from live_setups s where id::bigint=$1")
            .bind(setup_id).fetch_one(&pool).await.unwrap();
        assert_eq!(persisted, stored, "generation replacement must not rewrite raw/recommendation/history");
        let advanced = db::get_adaptive_checkpoint(&pool, profile.slot_id, INSTRUMENT).await.unwrap().unwrap();
        assert_eq!(advanced.generation_id, next_generation);
        assert_eq!(advanced.state, next_checkpoint);
        assert!(!db::commit_adaptive_scan_cas(&pool, &profile, INSTRUMENT, Some(&advanced), &json!({}), &[]).await.unwrap().applied);
    }).await;
}

async fn pending_fixture(pool: &PgPool, active: &db::ActiveAdaptiveSlot) -> db::live::PendingAdaptiveSlot {
    let generation = generation(pool,OWNER,ALGORITHM,active.slot_id,"completed").await;
    let mut result = active.result.clone();
    result["meanExpectancy"]=json!(0.21);
    result["bootstrap"]=json!({"frozenPrefix":{"version":1}});
    sqlx::query("update continuous_generations g set study_id=$2,accepted=true,result=$3,config_revision=p.config_revision,spec=p.config->'study' from continuous_processes p where g.id=$1 and p.owner_id=g.owner_id and p.algorithm_id=g.algorithm_id")
        .bind(generation).bind(active.study_id).bind(result).execute(pool).await.unwrap();
    sqlx::query("update continuous_slots set pending_generation_id=$2,pending_status='warming' where id=$1")
        .bind(active.slot_id).bind(generation).execute(pool).await.unwrap();
    db::live::pending_adaptive_slots(pool,OWNER).await.unwrap().remove(0)
}

#[tokio::test]
#[ignore = "requires isolated TEST_DATABASE_URL ending in _test with CREATEDB"]
async fn independent_initialization_retries_keep_anchor_bound_attempts_and_recover_abandoned_final_claim() {
    use db::live::*;
    isolated(|pool| async move {
        setups_db::migrate::ensure_migrations(&pool).await.unwrap();
        let old=accepted(&pool).await;
        let pending=pending_fixture(&pool,&old).await;
        let id=pending.profile.generation_id;
        let immutable:Value=sqlx::query_scalar("select jsonb_build_object('spec',spec,'result',result,'accepted',accepted,'status',status,'study',study_id) from continuous_generations where id=$1")
            .bind(id).fetch_one(&pool).await.unwrap();
        let config_before:Value=sqlx::query_scalar("select config from continuous_processes where owner_id=$1 and algorithm_id=$2")
            .bind(OWNER).bind(ALGORITHM).fetch_one(&pool).await.unwrap();
        let first=claim_pending_warmup_at(&pool,&pending,"first",CUTOFF).await.unwrap().unwrap();
        assert_eq!(first.attempt,1);
        assert_eq!(first.initialization.anchor_close_unix,CUTOFF);
        assert!(first.initialization.detector_from_open_unix<CUTOFF-30*86400);
        assert_eq!(first.initialization.emit_from_close_unix,CUTOFF+60);
        let remaining=first.attempt_deadline_unix-chrono::Utc::now().timestamp();
        assert!((21590..=21601).contains(&remaining),"default must cover full cold universe, not a lease TTL");
        assert!(report_warmup_failure(&pool,&first,&json!({"code":"data_unavailable"}),true).await.unwrap());
        let occupied:bool=sqlx::query_scalar("select lease_owner is not null from adaptive_live_warmup_lane").fetch_one(&pool).await.unwrap();
        assert!(!occupied,"drained failure must release lane immediately");
        assert!(claim_pending_warmup_at(&pool,&pending,"too-soon",CUTOFF+60).await.unwrap().is_none());
        sqlx::query("update continuous_generations set live_init_next_attempt_at=clock_timestamp()-interval '1 second' where id=$1")
            .bind(id).execute(&pool).await.unwrap();
        let second=claim_pending_warmup_at(&pool,&pending,"second",CUTOFF+600).await.unwrap().unwrap();
        assert_eq!(second.initialization,first.initialization,"stale caller cannot move durable initialization");
        assert_eq!(second.attempt,2);
        assert_ne!(second.token,first.token);
        assert!(!report_warmup(&pool,&first,"ready",Some(CUTOFF),None).await.unwrap());
        sqlx::query("update continuous_generations set live_init_deadline=clock_timestamp()-interval '1 second' where id=$1")
            .bind(id).execute(&pool).await.unwrap();
        assert!(!report_warmup(&pool,&second,"warming",None,None).await.unwrap(),"heartbeat cannot extend fixed attempt deadline");
        assert_eq!(recover_pending_warmups(&pool,OWNER).await.unwrap(),1);
        sqlx::query("update continuous_generations set live_init_next_attempt_at=clock_timestamp()-interval '1 second' where id=$1")
            .bind(id).execute(&pool).await.unwrap();
        let third=claim_pending_warmup_at(&pool,&pending,"third",CUTOFF+1200).await.unwrap().unwrap();
        assert_eq!(third.initialization,first.initialization);assert_eq!(third.attempt,3);
        // Simulate process loss, not a cooperative final failure report.
        sqlx::query("update adaptive_live_warmup_lane set lease_until=clock_timestamp()-interval '1 second'").execute(&pool).await.unwrap();
        assert_eq!(recover_pending_warmups(&pool,OWNER).await.unwrap(),1);
        assert_eq!(recover_pending_warmups(&pool,OWNER).await.unwrap(),0,"recovery is idempotent");
        assert!(claim_pending_warmup_at(&pool,&pending,"fourth",CUTOFF+1800).await.unwrap().is_none());
        assert!(!report_warmup(&pool,&third,"ready",Some(CUTOFF),None).await.unwrap());
        let slot=db::get(&pool,OWNER,ALGORITHM).await.unwrap().unwrap().slots.remove(0);
        assert_eq!(slot.active_generation_id,Some(old.generation_id));
        assert_eq!(slot.pending_generation_id,None);
        assert_eq!(slot.pending_error.unwrap()["code"],"live_initialization_abandoned");
        let terminal:Value=sqlx::query_scalar("select jsonb_build_object('status',live_init_status,'attempts',live_init_attempts,'error',live_init_error) from continuous_generations where id=$1")
            .bind(id).fetch_one(&pool).await.unwrap();
        assert_eq!(terminal["status"],"exhausted");assert_eq!(terminal["attempts"],3);
        let after:Value=sqlx::query_scalar("select jsonb_build_object('spec',spec,'result',result,'accepted',accepted,'status',status,'study',study_id) from continuous_generations where id=$1")
            .bind(id).fetch_one(&pool).await.unwrap();
        assert_eq!(after,immutable);
        assert_eq!(sqlx::query_scalar::<_,Value>("select config from continuous_processes where owner_id=$1 and algorithm_id=$2")
            .bind(OWNER).bind(ALGORITHM).fetch_one(&pool).await.unwrap(),config_before);
        assert_eq!(sqlx::query_scalar::<_,i32>("select retry_count from continuous_processes where owner_id=$1 and algorithm_id=$2")
            .bind(OWNER).bind(ALGORITHM).fetch_one(&pool).await.unwrap(),0);
    }).await;
}

#[tokio::test]
#[ignore = "requires isolated TEST_DATABASE_URL ending in _test with CREATEDB"]
async fn independent_live_migration_keeps_legacy_anchor_and_immutable_research_across_restart() {
    isolated(|pool| async move {
        setups_db::migrate::ensure_migrations(&pool).await.unwrap();
        let old=accepted(&pool).await;
        let bootstrap=json!({"observationCutoff":CUTOFF,"detectorWarmupFromUnix":CUTOFF-45*86400,"emitFromCloseUnix":CUTOFF+60});
        sqlx::query("update continuous_generations set result=jsonb_set(result,'{bootstrap}',$2) where id=$1")
            .bind(old.generation_id).bind(bootstrap).execute(&pool).await.unwrap();
        sqlx::query("update continuous_generations set spec=jsonb_set(spec,'{instruments}',spec->'instruments'||'[ {\"exchange\":\"bybit\",\"category\":\"linear\",\"symbol\":\"MISSINGUSDT\"} ]'::jsonb) where id=$1")
            .bind(old.generation_id).execute(&pool).await.unwrap();
        let cursor=promotion_state(old.generation_id,CUTOFF+600);
        sqlx::query("insert into adaptive_live_checkpoints(slot_id,instrument_key,generation_id,state) values($1,$2,$3,$4)")
            .bind(old.slot_id).bind(INSTRUMENT).bind(old.generation_id).bind(&cursor).execute(&pool).await.unwrap();
        let immutable:Value=sqlx::query_scalar("select jsonb_build_object('spec',spec,'result',result,'accepted',accepted,'status',status,'updatedAt',updated_at) from continuous_generations where id=$1")
            .bind(old.generation_id).fetch_one(&pool).await.unwrap();
        // Emulate the actual pre-0016 schema in this disposable child database.
        sqlx::raw_sql("alter table continuous_generations drop column live_initialization,drop column live_readiness,drop column live_init_attempts,drop column live_init_status,drop column live_init_deadline,drop column live_init_next_attempt_at,drop column live_init_error; delete from kysely_migration where name='0016_independent_live_initialization.sql';")
            .execute(&pool).await.unwrap();
        assert_eq!(setups_db::migrate::ensure_migrations(&pool).await.unwrap(),"1 applied, 7 up to date");
        let active=db::active_adaptive_slots(&pool,OWNER).await.unwrap().remove(0);
        let anchor=active.live_initialization.unwrap();
        assert_eq!(active.live_readiness.len(),2,"legacy full scope needs explicit disposition");
        assert_eq!(active.live_readiness[0].instrument_key,INSTRUMENT);
        assert_eq!(active.live_readiness[0].status,"ready","only an actual retained cursor justifies legacy ready");
        assert_eq!(active.live_readiness[0].publication_floor_close_unix,CUTOFF);
        assert_eq!(active.live_readiness[1].status,"waiting");
        assert_eq!(active.live_readiness[1].reason.as_deref(),Some("unknown"));
        assert_eq!(active.live_readiness[1].publication_floor_close_unix,CUTOFF);
        assert_eq!(anchor.anchor_close_unix,CUTOFF);
        assert_eq!(anchor.detector_from_open_unix,CUTOFF-45*86400);
        assert_eq!(anchor.emit_from_close_unix,CUTOFF+60);
        setups_db::migrate::ensure_migrations(&pool).await.unwrap();
        assert_eq!(db::active_adaptive_slots(&pool,OWNER).await.unwrap()[0].live_initialization.as_ref(),Some(&anchor));
        assert_eq!(db::get_adaptive_checkpoint(&pool,old.slot_id,INSTRUMENT).await.unwrap().unwrap().state,cursor);
        assert_eq!(sqlx::query_scalar::<_,Value>("select jsonb_build_object('spec',spec,'result',result,'accepted',accepted,'status',status,'updatedAt',updated_at) from continuous_generations where id=$1")
            .bind(old.generation_id).fetch_one(&pool).await.unwrap(),immutable);
    }).await;
}

#[tokio::test]
#[ignore = "requires isolated TEST_DATABASE_URL ending in _test with CREATEDB"]
async fn pending_candidate_never_blocks_research_and_new_complete_round_supersedes_with_fences() {
    use db::live::*;
    isolated(|pool| async move {
        setups_db::migrate::ensure_migrations(&pool).await.unwrap();
        let old=accepted(&pool).await;
        let pending=pending_fixture(&pool,&old).await;
        let fence=claim_pending_warmup_at(&pool,&pending,"old-warmup",CUTOFF).await.unwrap().unwrap();
        let lease=scheduler::claim_owner(&pool,"next-round").await.unwrap().unwrap();
        let job=scheduler::prepare_next(&pool,&lease).await.unwrap().unwrap();
        assert_eq!(job.slot_id,old.slot_id,"single-threshold process must not starve behind pending live");
        let result=json!({"best":pending.profile.result["best"],"analytics":{"frozenProof":"retained"},
            "recommendation":{"retained":true},"meanExpectancy":0.21,"selectedCell":{"tpPct":14,"slPct":4}});
        let completed_batch=batch(&pool,ALGORITHM,Some(job.generation_id)).await;
        run(&pool,completed_batch,ALGORITHM,"done",Some(job.generation_id)).await;
        sqlx::query("update optimization_studies set status='completed',result='{}' where id=$1")
            .bind(job.study_id).execute(&pool).await.unwrap();
        sqlx::query("update continuous_generations set replay_batch_id=$2,replay_manifest='{}' where id=$1")
            .bind(job.generation_id).bind(completed_batch).execute(&pool).await.unwrap();
        assert!(scheduler::finalize(&pool,&job,&result,true,Some(0.21),Some(&result["selectedCell"])).await.unwrap(),
            "live-only frozenPrefix presence is not research admission");
        let state=db::get(&pool,OWNER,ALGORITHM).await.unwrap().unwrap();
        assert_eq!(state.slots[0].active_generation_id,Some(old.generation_id));
        assert_eq!(state.slots[0].pending_generation_id,Some(job.generation_id));
        assert!(!report_warmup(&pool,&fence,"ready",Some(CUTOFF),None).await.unwrap());
        assert!(!report_warmup_failure(&pool,&fence,&json!({"code":"late"}),false).await.unwrap());
        assert_eq!(sqlx::query_scalar::<_,String>("select live_init_status from continuous_generations where id=$1")
            .bind(pending.profile.generation_id).fetch_one(&pool).await.unwrap(),"superseded");
        assert_eq!(sqlx::query_scalar::<_,Value>("select result from continuous_generations where id=$1")
            .bind(pending.profile.generation_id).fetch_one(&pool).await.unwrap(),pending.profile.result);
    }).await;
}

#[tokio::test]
#[ignore = "requires isolated TEST_DATABASE_URL ending in _test with CREATEDB"]
async fn waiting_subset_promotion_keeps_real_cursor_cas_and_late_join_has_own_publication_floor() {
    use db::live::*;
    isolated(|pool| async move {
        setups_db::migrate::ensure_migrations(&pool).await.unwrap();
        let old=accepted(&pool).await;
        let pending=pending_fixture(&pool,&old).await;
        let second="bybit/linear/NEWUSDT";
        let scope=json!([{"exchange":"bybit","category":"linear","symbol":"BTCUSDT"},
            {"exchange":"bybit","category":"linear","symbol":"NEWUSDT"}]);
        sqlx::query("update continuous_generations set spec=jsonb_set(spec,'{instruments}',$2) where id=$1")
            .bind(pending.profile.generation_id).bind(scope).execute(&pool).await.unwrap();
        let pending=pending_adaptive_slots(&pool,OWNER).await.unwrap().remove(0);
        let fence=claim_pending_warmup_at(&pool,&pending,"subset",CUTOFF).await.unwrap().unwrap();
        let stale=promotion_state(old.generation_id,CUTOFF-600);
        sqlx::query("insert into adaptive_live_checkpoints(slot_id,instrument_key,generation_id,state) values($1,$2,$3,$4)")
            .bind(old.slot_id).bind(second).bind(old.generation_id).bind(&stale).execute(&pool).await.unwrap();
        let expected=vec![CheckpointExpectation {instrument_key:INSTRUMENT.into(),checkpoint:None},
            CheckpointExpectation {instrument_key:second.into(),checkpoint:Some(db::AdaptiveCheckpoint {generation_id:old.generation_id,state:stale.clone()})}];
        let initial=vec![InitialAdaptiveCheckpoint {instrument_key:INSTRUMENT.into(),state:promotion_state(fence.generation_id,CUTOFF)}];
        let waiting=vec![WaitingInstrument {instrument_key:second.into(),reason:"insufficient_baseline".into()}];
        assert!(report_warmup(&pool,&fence,"ready",Some(CUTOFF),None).await.unwrap());
        assert!(promote_pending(&pool,&fence,Some(old.generation_id),&expected,CUTOFF,&initial).await.is_err(),"no blanket relaxation");
        assert!(!promote_pending_with_waiting(&pool,&fence,Some(old.generation_id),&expected,CUTOFF,&[],&waiting).await.unwrap(),"zero ready is not healthy active");
        let mut wrong=expected.clone();wrong[1].checkpoint.as_mut().unwrap().state["nextOpenUnix"]=json!(CUTOFF-660);
        assert!(!promote_pending_with_waiting(&pool,&fence,Some(old.generation_id),&wrong,CUTOFF,&initial,&waiting).await.unwrap(),"waiting does not weaken exact old CAS");
        assert!(promote_pending_with_waiting(&pool,&fence,Some(old.generation_id),&expected,CUTOFF,&initial,&waiting).await.unwrap());
        assert!(db::get_adaptive_checkpoint(&pool,old.slot_id,second).await.unwrap().is_none(),"never synthesize waiting cursor at W");
        let active=db::active_adaptive_slots(&pool,OWNER).await.unwrap().remove(0);
        assert_eq!(active.live_readiness.len(),2);
        assert_eq!(active.live_initialization,Some(fence.initialization.clone()));
        let summary=db::get(&pool,OWNER,ALGORITHM).await.unwrap().unwrap().slots.remove(0).live_readiness_summary.unwrap();
        assert_eq!((summary.total_instruments,summary.ready_instruments,summary.waiting_instruments),(2,1,1));
        assert_eq!(summary.generation_id,fence.generation_id);
        assert_eq!(summary.waiting_reasons[0].reason,"insufficient_baseline");
        let join=CUTOFF+600;
        assert!(!join_waiting_adaptive_instrument(&pool,&old,second,None,join,&promotion_state(old.generation_id,join)).await.unwrap(),"old active fence lost");
        assert!(join_waiting_adaptive_instrument(&pool,&active,second,None,join,&promotion_state(active.generation_id,join)).await.unwrap());
        let after=db::active_adaptive_slots(&pool,OWNER).await.unwrap().remove(0);
        assert_eq!(after.live_readiness.iter().find(|e|e.instrument_key==second).unwrap().publication_floor_close_unix,join);
        assert!(!join_waiting_adaptive_instrument(&pool,&after,second,None,join,&promotion_state(active.generation_id,join)).await.unwrap(),"lost-response retry cannot overwrite live cursor");
        assert_eq!(db::get(&pool,OWNER,ALGORITHM).await.unwrap().unwrap().slots[0].live_readiness_summary.as_ref().unwrap().ready_instruments,2);
        assert!(db::stop(&pool,OWNER,ALGORITHM).await.unwrap());
        assert!(!join_waiting_adaptive_instrument(&pool,&after,second,None,join,&promotion_state(active.generation_id,join)).await.unwrap());
        assert!(db::get(&pool,OWNER,ALGORITHM).await.unwrap().unwrap().slots[0].live_readiness_summary.is_none());
    }).await;
}

#[tokio::test]
#[ignore = "requires isolated TEST_DATABASE_URL ending in _test with CREATEDB"]
async fn retired_unavailable_old_only_cursor_is_explicit_and_cas_exact_without_candidate_scope_relaxation() {
    use db::live::*;
    isolated(|pool| async move {
        setups_db::migrate::ensure_migrations(&pool).await.unwrap();
        let old=accepted(&pool).await;
        let second="bybit/linear/DELISTEDUSDT";
        sqlx::query("update continuous_generations set spec=jsonb_set(spec,'{instruments}',spec->'instruments'||'[ {\"exchange\":\"bybit\",\"category\":\"linear\",\"symbol\":\"DELISTEDUSDT\"} ]'::jsonb) where id=$1")
            .bind(old.generation_id).execute(&pool).await.unwrap();
        let pending=pending_fixture(&pool,&old).await;
        let fence=claim_pending_warmup_at(&pool,&pending,"retired",CUTOFF).await.unwrap().unwrap();
        let stale=promotion_state(old.generation_id,CUTOFF-600);
        sqlx::query("insert into adaptive_live_checkpoints(slot_id,instrument_key,generation_id,state) values($1,$2,$3,$4)")
            .bind(old.slot_id).bind(second).bind(old.generation_id).bind(&stale).execute(&pool).await.unwrap();
        let expected=vec![CheckpointExpectation {instrument_key:INSTRUMENT.into(),checkpoint:None},
            CheckpointExpectation {instrument_key:second.into(),checkpoint:Some(db::AdaptiveCheckpoint {generation_id:old.generation_id,state:stale})}];
        let initial=vec![InitialAdaptiveCheckpoint {instrument_key:INSTRUMENT.into(),state:promotion_state(fence.generation_id,CUTOFF)}];
        let retired=vec![WaitingInstrument {instrument_key:second.into(),reason:"not_listed".into()}];
        assert!(report_warmup(&pool,&fence,"ready",Some(CUTOFF),None).await.unwrap());
        assert!(promote_pending_with_waiting(&pool,&fence,Some(old.generation_id),&expected,CUTOFF,&initial,&[]).await.is_err());
        let mut wrong=expected.clone();wrong[1].checkpoint.as_mut().unwrap().state["nextOpenUnix"]=json!(CUTOFF-660);
        assert!(!promote_pending_with_dispositions(&pool,&fence,Some(old.generation_id),&wrong,CUTOFF,&initial,&[],&retired).await.unwrap());
        assert!(promote_pending_with_dispositions(&pool,&fence,Some(old.generation_id),&expected,CUTOFF,&initial,&[],
            &[WaitingInstrument {instrument_key:INSTRUMENT.into(),reason:"not_listed".into()}]).await.is_err(),"new-scope instrument cannot be labeled retired");
        assert!(promote_pending_with_dispositions(&pool,&fence,Some(old.generation_id),&expected,CUTOFF,&initial,&[],&retired).await.unwrap());
        let active=db::active_adaptive_slots(&pool,OWNER).await.unwrap().remove(0);
        assert_eq!(active.live_readiness.len(),1,"retired symbols never inflate candidate scope");
        assert_eq!(active.live_readiness[0].instrument_key,INSTRUMENT);
        assert!(db::get_adaptive_checkpoint(&pool,old.slot_id,second).await.unwrap().is_none());
    }).await;
}

fn promotion_state(generation: i64, frontier: i64) -> Value {
    json!({"generationId":generation,"nextOpenUnix":frontier,"lastRawOpenUnix":null,"lastRawClusterTime":null})
}

async fn retirement_fixture(pool: &PgPool) -> (db::ActiveAdaptiveSlot, i64) {
    setups_db::migrate::ensure_migrations(pool).await.unwrap();
    let old = accepted(pool).await;
    let frontier = chrono::Utc::now().timestamp().div_euclid(60) * 60;
    setups_db::settings_set(pool,"notifications",&json!({"maxAgeSeconds":3600})).await.unwrap();
    assert!(db::commit_adaptive_scan_cas(pool,&old,INSTRUMENT,None,
        &promotion_state(old.generation_id,frontier),&[adaptive_write(frontier-60)]).await.unwrap().applied);
    assert_eq!(sqlx::query_scalar::<_,i64>("select count(*) from adaptive_notification_outbox").fetch_one(pool).await.unwrap(),1);
    fixed_setup(pool,frontier-60).await;
    study(pool,OWNER,OTHER_ALGORITHM,"completed",None,None).await;
    let historical_batch = batch(pool,ALGORITHM,Some(old.generation_id)).await;
    run(pool,historical_batch,ALGORITHM,"done",Some(old.generation_id)).await;
    (old,frontier)
}

async fn retained_history(pool: &PgPool, old: &db::ActiveAdaptiveSlot) -> Value {
    sqlx::query_scalar("select jsonb_build_object('generation',(select to_jsonb(g) from continuous_generations g where id=$1),'studies',(select jsonb_agg(to_jsonb(s) order by id) from optimization_studies s where continuous_generation_id=$1 or origin='manual'),'live',(select jsonb_agg(to_jsonb(s) order by id) from live_setups s),'outbox',(select jsonb_agg(to_jsonb(j) order by id) from adaptive_notification_outbox j),'budget',(select to_jsonb(b) from adaptive_notification_budget b where id=1),'runs',(select jsonb_agg(to_jsonb(r) order by id) from replay_runs r where status='done'),'batches',(select jsonb_agg(to_jsonb(b) order by id) from replay_batches b))")
        .bind(old.generation_id).fetch_one(pool).await.unwrap()
}

async fn activate_after_retirement(pool: &PgPool, old: &db::ActiveAdaptiveSlot, key: &str, frontier: i64) {
    use db::live::*;
    let pending = pending_fixture(pool,old).await;
    assert_eq!(pending.expected_active_generation_id,None);
    let fence = claim_pending_warmup(pool,&pending,"fresh-after-retirement").await.unwrap().unwrap();
    assert!(report_warmup(pool,&fence,"ready",Some(frontier),None).await.unwrap());
    // Exactly the worker contract: compare every currently declared instrument,
    // including absent cursors. No obsolete/out-of-scope predecessor is invented.
    let expected = [CheckpointExpectation {instrument_key:key.into(),
        checkpoint:db::get_adaptive_checkpoint(pool,old.slot_id,key).await.unwrap()}];
    let initial = [InitialAdaptiveCheckpoint {instrument_key:key.into(),
        state:promotion_state(pending.profile.generation_id,frontier)}];
    assert!(promote_pending(pool,&fence,None,&expected,frontier,&initial).await
        .expect("obsolete inactive cursor must not block the new promotion frontier"),
        "obsolete cursor outside the new scope must not block fresh activation");
    let cursor = db::get_adaptive_checkpoint(pool,old.slot_id,key).await.unwrap().unwrap();
    assert_eq!(cursor.state,initial[0].state);
    assert_eq!(cursor.generation_id,pending.profile.generation_id);
    assert_eq!(sqlx::query_scalar::<_,i64>("select count(*) from adaptive_live_checkpoints where slot_id=$1")
        .bind(old.slot_id).fetch_one(pool).await.unwrap(),1);
    assert!(!db::commit_adaptive_scan_cas(pool,old,INSTRUMENT,None,
        &promotion_state(old.generation_id,frontier+60),&[adaptive_write(frontier)]).await.unwrap().applied);
}

#[tokio::test]
#[ignore = "requires isolated TEST_DATABASE_URL ending in _test with CREATEDB"]
async fn retired_cursors_stop_restart_can_promote_without_rewriting_history() {
    isolated(|pool| async move {
        let (old,frontier) = retirement_fixture(&pool).await;
        let history = retained_history(&pool,&old).await;
        let lease = scheduler::claim_owner(&pool,"retirement-stop").await.unwrap().unwrap();
        assert!(db::stop(&pool,OWNER,ALGORITHM).await.unwrap());
        assert!(db::get_adaptive_checkpoint(&pool,old.slot_id,INSTRUMENT).await.unwrap().is_none());
        assert!(!db::start(&pool,OWNER,ALGORITHM).await.unwrap(),"Stop must drain before restart");
        cleanup_owned(&pool,&lease,ALGORITHM).await.unwrap();
        assert!(db::start(&pool,OWNER,ALGORITHM).await.unwrap());
        activate_after_retirement(&pool,&old,INSTRUMENT,frontier+600).await;
        // Durable slot-level signal dedup survives retirement of detector cursors.
        let new = db::active_adaptive_slots(&pool,OWNER).await.unwrap().remove(0);
        let previous = db::get_adaptive_checkpoint(&pool,new.slot_id,INSTRUMENT).await.unwrap().unwrap();
        let duplicate = db::commit_adaptive_scan_cas(&pool,&new,INSTRUMENT,Some(&previous),
            &promotion_state(new.generation_id,frontier+660),&[adaptive_write(frontier-60)]).await.unwrap();
        assert!(duplicate.applied && duplicate.inserted.is_empty());
        assert_eq!(retained_history(&pool,&old).await,history);
    }).await;
}

#[tokio::test]
#[ignore = "requires isolated TEST_DATABASE_URL ending in _test with CREATEDB"]
async fn retired_cursors_reconfigure_removes_old_scope_but_not_other_owners_or_algorithms() {
    isolated(|pool| async move {
        let (old,frontier) = retirement_fixture(&pool).await;
        let mut foreign = Vec::new();
        for (owner,algorithm) in [(OWNER,OTHER_ALGORITHM),("other-owner",ALGORITHM)] {
            let slot = configured(&pool,owner,algorithm).await;
            let generation = generation(&pool,owner,algorithm,slot,"completed").await;
            sqlx::query("update continuous_slots set active_generation_id=$2 where id=$1")
                .bind(slot).bind(generation).execute(&pool).await.unwrap();
            let state = promotion_state(generation,frontier);
            sqlx::query("insert into adaptive_live_checkpoints(slot_id,instrument_key,generation_id,state) values($1,$2,$3,$4)")
                .bind(slot).bind(INSTRUMENT).bind(generation).bind(&state).execute(&pool).await.unwrap();
            foreign.push((slot,db::AdaptiveCheckpoint {generation_id:generation,state}));
        }
        let history = retained_history(&pool,&old).await;
        // Persisted pre-fix Stop state isolates configuration reset from the now-
        // corrected Stop path: an inactive old BTC cursor still exists on disk.
        sqlx::query("update continuous_processes set desired_running=false,status='stopped',active_generation_id=null where owner_id=$1 and algorithm_id=$2")
            .bind(OWNER).bind(ALGORITHM).execute(&pool).await.unwrap();
        sqlx::query("update continuous_slots set active_generation_id=null where id=$1")
            .bind(old.slot_id).execute(&pool).await.unwrap();
        let mut changed = config(ALGORITHM);
        changed["study"]["instruments"][0]["symbol"] = json!("ETHUSDT");
        assert!(db::configure(&pool,OWNER,ALGORITHM,&changed,&[4.0]).await.unwrap());
        assert!(db::get_adaptive_checkpoint(&pool,old.slot_id,INSTRUMENT).await.unwrap().is_none(),
            "configuration reset must retire old scope before Start can repair it");
        assert!(db::start(&pool,OWNER,ALGORITHM).await.unwrap());
        activate_after_retirement(&pool,&old,"bybit/linear/ETHUSDT",frontier+600).await;
        assert!(db::get_adaptive_checkpoint(&pool,old.slot_id,INSTRUMENT).await.unwrap().is_none());
        for (slot,expected) in foreign {
            assert_eq!(db::get_adaptive_checkpoint(&pool,slot,INSTRUMENT).await.unwrap(),Some(expected));
        }
        assert_eq!(retained_history(&pool,&old).await,history);
    }).await;
}

#[tokio::test]
#[ignore = "requires isolated TEST_DATABASE_URL ending in _test with CREATEDB"]
async fn retired_cursors_start_repairs_legacy_inactive_state_without_a_new_configuration() {
    isolated(|pool| async move {
        let (old,frontier) = retirement_fixture(&pool).await;
        let history = retained_history(&pool,&old).await;
        sqlx::query("update continuous_processes set desired_running=false,status='stopped',active_generation_id=null where owner_id=$1 and algorithm_id=$2")
            .bind(OWNER).bind(ALGORITHM).execute(&pool).await.unwrap();
        sqlx::query("update continuous_slots set active_generation_id=null where id=$1")
            .bind(old.slot_id).execute(&pool).await.unwrap();
        assert!(db::start(&pool,OWNER,ALGORITHM).await.unwrap());
        activate_after_retirement(&pool,&old,INSTRUMENT,frontier+600).await;
        assert_eq!(retained_history(&pool,&old).await,history);
    }).await;
}

#[tokio::test]
#[ignore = "requires isolated TEST_DATABASE_URL ending in _test with CREATEDB"]
async fn retired_cursors_nonpass_retires_only_after_technical_retry_preserves_incumbent() {
    isolated(|pool| async move {
        let (old,frontier) = retirement_fixture(&pool).await;
        let history = retained_history(&pool,&old).await;
        let previous = db::get_adaptive_checkpoint(&pool,old.slot_id,INSTRUMENT).await.unwrap();
        let lease = scheduler::claim_owner(&pool,"retirement-nonpass").await.unwrap().unwrap();
        let failed = scheduler::prepare_next(&pool,&lease).await.unwrap().unwrap();
        scheduler::fail(&pool,&failed,&json!({"code":"technical_failure"})).await.unwrap();
        assert!(db::start(&pool,OWNER,ALGORITHM).await.unwrap());
        let mut permission = config(ALGORITHM);
        permission["adaptiveEnabled"] = json!(false);
        assert!(db::configure(&pool,OWNER,ALGORITHM,&permission,&[4.0]).await.unwrap());
        assert_eq!(db::get_adaptive_checkpoint(&pool,old.slot_id,INSTRUMENT).await.unwrap(),previous);
        permission["adaptiveEnabled"] = json!(true);
        assert!(db::configure(&pool,OWNER,ALGORITHM,&permission,&[4.0]).await.unwrap());
        assert_eq!(db::active_adaptive_slots(&pool,OWNER).await.unwrap()[0].generation_id,old.generation_id);
        let nonpass = scheduler::prepare_next(&pool,&lease).await.unwrap().unwrap();
        scheduler::finish_search(&pool,&nonpass,&json!({"best":null,"progress":{}}),true).await.unwrap();
        assert!(scheduler::finalize(&pool,&nonpass,&json!({"best":null,"reason":"no winner"}),false,None,None).await.unwrap());
        activate_after_retirement(&pool,&old,INSTRUMENT,frontier+600).await;
        assert_eq!(retained_history(&pool,&old).await,history);
        assert_eq!(optimization::get(&pool,nonpass.study_id).await.unwrap().unwrap().status,"completed");
    }).await;
}

#[tokio::test]
#[ignore = "requires isolated TEST_DATABASE_URL ending in _test with CREATEDB"]
async fn retired_cursors_stop_rolls_back_pointer_and_cursor_on_retirement_error() {
    isolated(|pool| async move {
        let (old,_) = retirement_fixture(&pool).await;
        let history = retained_history(&pool,&old).await;
        let before = serde_json::to_value(db::get(&pool,OWNER,ALGORITHM).await.unwrap()).unwrap();
        let cursor = db::get_adaptive_checkpoint(&pool,old.slot_id,INSTRUMENT).await.unwrap();
        sqlx::raw_sql("create function reject_test_retirement() returns trigger language plpgsql as $$begin raise exception 'retirement rollback sentinel';end$$;create trigger reject_test_retirement before delete on adaptive_live_checkpoints for each row execute function reject_test_retirement()")
            .execute(&pool).await.unwrap();
        assert!(db::stop(&pool,OWNER,ALGORITHM).await.is_err());
        assert_eq!(serde_json::to_value(db::get(&pool,OWNER,ALGORITHM).await.unwrap()).unwrap(),before);
        assert_eq!(db::get_adaptive_checkpoint(&pool,old.slot_id,INSTRUMENT).await.unwrap(),cursor);
        assert_eq!(retained_history(&pool,&old).await,history);
        sqlx::raw_sql("drop trigger reject_test_retirement on adaptive_live_checkpoints").execute(&pool).await.unwrap();
        assert!(db::stop(&pool,OWNER,ALGORITHM).await.unwrap());
        assert!(db::get_adaptive_checkpoint(&pool,old.slot_id,INSTRUMENT).await.unwrap().is_none());
        assert_eq!(retained_history(&pool,&old).await,history);
    }).await;
}

#[tokio::test]
#[ignore = "requires isolated TEST_DATABASE_URL ending in _test with CREATEDB"]
async fn retired_cursors_stop_fences_a_concurrent_old_scan_before_removing_ephemeral_state() {
    isolated(|pool| async move {
        let (old,frontier) = retirement_fixture(&pool).await;
        let history = retained_history(&pool,&old).await;
        let previous = db::get_adaptive_checkpoint(&pool,old.slot_id,INSTRUMENT).await.unwrap();
        const GATE: i64 = 81_330_902;
        sqlx::raw_sql(&format!("create function gate_test_retirement() returns trigger language plpgsql as $$begin perform pg_advisory_xact_lock({GATE}::bigint);return old;end$$;create trigger gate_test_retirement before delete on adaptive_live_checkpoints for each row execute function gate_test_retirement()"))
            .execute(&pool).await.unwrap();
        let mut gate = pool.begin().await.unwrap();
        sqlx::query("select pg_advisory_xact_lock($1)").bind(GATE).execute(&mut *gate).await.unwrap();
        let stop_pool = pool.clone();
        let stop = tokio::task::spawn_local(async move {db::stop(&stop_pool,OWNER,ALGORITHM).await});
        let reached = tokio::time::timeout(Duration::from_secs(2),async {
            loop {
                let waiting: bool = sqlx::query_scalar("select exists(select 1 from pg_locks where database=(select oid from pg_database where datname=current_database()) and locktype='advisory' and classid=0 and objid=$1::bigint::oid and not granted)")
                    .bind(GATE).fetch_one(&pool).await.unwrap();
                if waiting {break;}
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }).await;
        let scan_pool = pool.clone();
        let scan_old = old.clone();
        let mut scan = tokio::task::spawn_local(async move {
            db::commit_adaptive_scan_cas(&scan_pool,&scan_old,INSTRUMENT,previous.as_ref(),
                &promotion_state(scan_old.generation_id,frontier+60),&[adaptive_write(frontier)]).await
        });
        let observed = tokio::time::timeout(Duration::from_millis(150),&mut scan).await;
        let blocked = observed.is_err();
        // Release and join even on old-code failure: no transaction/task may
        // outlive the fixture and interfere with owned child-database cleanup.
        gate.rollback().await.unwrap();
        assert!(tokio::time::timeout(Duration::from_secs(5),stop).await.unwrap().unwrap().unwrap());
        let result = match observed {
            Ok(result) => result.unwrap().unwrap(),
            Err(_) => tokio::time::timeout(Duration::from_secs(5),scan).await.unwrap().unwrap().unwrap(),
        };
        assert!(reached.is_ok(),"Stop did not retire cursors under the controlled transaction");
        assert!(blocked,"old scan did not wait for Stop's process fence");
        assert!(!result.applied && result.inserted.is_empty());
        assert!(db::get_adaptive_checkpoint(&pool,old.slot_id,INSTRUMENT).await.unwrap().is_none());
        assert_eq!(retained_history(&pool,&old).await,history);
    }).await;
}

#[tokio::test]
#[ignore = "requires isolated TEST_DATABASE_URL ending in _test with CREATEDB"]
async fn pending_promotion_cas_compares_absent_and_all_old_cursors_and_survives_lost_response() {
    use db::live::*;
    isolated(|pool| async move {
        setups_db::migrate::ensure_migrations(&pool).await.unwrap();
        let old = accepted(&pool).await;
        let pending = pending_fixture(&pool,&old).await;
        assert_eq!(db::active_adaptive_slots(&pool,OWNER).await.unwrap()[0].generation_id,old.generation_id);
        let fence = claim_pending_warmup(&pool,&pending,"warmup-a").await.unwrap().unwrap();
        assert!(claim_pending_warmup(&pool,&pending,"warmup-b").await.unwrap().is_none());
        let initial = vec![InitialAdaptiveCheckpoint {instrument_key:INSTRUMENT.into(),state:promotion_state(pending.profile.generation_id,CUTOFF)}];
        assert!(report_warmup(&pool,&fence,"ready",Some(CUTOFF),None).await.unwrap());
        assert!(promote_pending(&pool,&fence,Some(old.generation_id),&[],CUTOFF,&initial).await.is_err(),"absent scoped cursor must be explicitly included");
        let absent = vec![CheckpointExpectation {instrument_key:INSTRUMENT.into(),checkpoint:None}];
        // Old commit wins while the candidate warms. A captured absent cursor is now stale.
        let old_state = promotion_state(old.generation_id,CUTOFF);
        assert!(db::commit_adaptive_scan_cas(&pool,&old,INSTRUMENT,None,&old_state,&[]).await.unwrap().applied);
        assert!(!promote_pending(&pool,&fence,Some(old.generation_id),&absent,CUTOFF,&initial).await.unwrap());
        let previous = db::get_adaptive_checkpoint(&pool,old.slot_id,INSTRUMENT).await.unwrap().unwrap();
        let expected = vec![CheckpointExpectation {instrument_key:INSTRUMENT.into(),checkpoint:Some(previous.clone())}];
        assert!(promote_pending(&pool,&fence,Some(old.generation_id),&expected,CUTOFF,&initial).await.unwrap());
        let restored = db::active_adaptive_slots(&pool,OWNER).await.unwrap().remove(0);
        assert_eq!(restored.generation_id,pending.profile.generation_id);
        assert!(db::live::pending_adaptive_slots(&pool,OWNER).await.unwrap().is_empty());
        let cursor = db::get_adaptive_checkpoint(&pool,old.slot_id,INSTRUMENT).await.unwrap().unwrap();
        assert_eq!(cursor.generation_id,restored.generation_id);
        assert_eq!(cursor.state,initial[0].state);
        // Promotion wins first: the old detector cannot commit after it, nor can a
        // retry after a lost response overwrite the new durable cursor.
        assert!(!db::commit_adaptive_scan_cas(&pool,&old,INSTRUMENT,Some(&previous),&promotion_state(old.generation_id,CUTOFF+60),&[]).await.unwrap().applied);
        assert!(!promote_pending(&pool,&fence,Some(old.generation_id),&expected,CUTOFF,&initial).await.unwrap());
        assert_eq!(db::get_adaptive_checkpoint(&pool,old.slot_id,INSTRUMENT).await.unwrap().unwrap(),cursor);
    }).await;
}

#[tokio::test]
#[ignore = "requires isolated TEST_DATABASE_URL ending in _test with CREATEDB"]
async fn warmup_expiry_restart_and_stop_fence_pending_without_replacing_incumbent() {
    use db::live::*;
    isolated(|pool| async move {
        setups_db::migrate::ensure_migrations(&pool).await.unwrap();
        let old = accepted(&pool).await;
        let pending = pending_fixture(&pool,&old).await;
        let expired = claim_pending_warmup(&pool,&pending,"old-runtime").await.unwrap().unwrap();
        assert!(report_warmup(&pool,&expired,"error",None,Some(&json!({"code":"prefix_mismatch"}))).await.unwrap());
        assert_eq!(db::active_adaptive_slots(&pool,OWNER).await.unwrap()[0].generation_id,old.generation_id);
        sqlx::query("update adaptive_live_warmup_lane set lease_until=clock_timestamp()-interval '1 second'").execute(&pool).await.unwrap();
        sqlx::query("update continuous_generations set live_init_next_attempt_at=clock_timestamp()-interval '1 second' where id=$1")
            .bind(pending.profile.generation_id).execute(&pool).await.unwrap();
        let restarted = claim_pending_warmup(&pool,&pending,"new-runtime").await.unwrap().unwrap();
        assert_ne!(expired.token,restarted.token);
        assert!(!report_warmup(&pool,&expired,"ready",Some(CUTOFF),None).await.unwrap());
        assert!(report_warmup(&pool,&restarted,"ready",Some(CUTOFF),None).await.unwrap());
        assert!(db::stop(&pool,OWNER,ALGORITHM).await.unwrap());
        let process = db::get(&pool,OWNER,ALGORITHM).await.unwrap().unwrap();
        assert!(process.slots[0].active_generation_id.is_none());
        assert!(process.slots[0].pending_generation_id.is_none());
        assert!(!report_warmup(&pool,&restarted,"ready",Some(CUTOFF),None).await.unwrap());
        let initial = vec![InitialAdaptiveCheckpoint {instrument_key:INSTRUMENT.into(),state:promotion_state(pending.profile.generation_id,CUTOFF)}];
        let expected = vec![CheckpointExpectation {instrument_key:INSTRUMENT.into(),checkpoint:None}];
        assert!(!promote_pending(&pool,&restarted,Some(old.generation_id),&expected,CUTOFF,&initial).await.unwrap());
    }).await;
}

#[tokio::test]
#[ignore = "requires isolated TEST_DATABASE_URL ending in _test with CREATEDB"]
async fn outbox_is_atomic_bounded_raw_close_ttl_and_reclaims_expired_delivery_leases() {
    use db::outbox::*;
    isolated(|pool| async move {
        setups_db::migrate::ensure_migrations(&pool).await.unwrap();
        let active = accepted(&pool).await;
        setups_db::settings_set(&pool,"notifications",&json!({"maxAgeSeconds":3600})).await.unwrap();
        configure_limits(&pool,1,65536).await.unwrap();
        let now = chrono::Utc::now().timestamp().div_euclid(60)*60;
        let state = promotion_state(active.generation_id,now);
        let committed = db::commit_adaptive_scan_cas(&pool,&active,INSTRUMENT,None,&state,&[adaptive_write(now-60)]).await.unwrap();
        assert_eq!(committed.inserted.len(),1);
        let previous = db::get_adaptive_checkpoint(&pool,active.slot_id,INSTRUMENT).await.unwrap().unwrap();
        let next_state=promotion_state(active.generation_id,now+60);
        let saturated = db::commit_adaptive_scan_cas(&pool,&active,INSTRUMENT,Some(&previous),&next_state,&[adaptive_write(now-120)]).await.unwrap();
        assert!(saturated.applied && saturated.inserted.len()==1);
        assert_eq!(sqlx::query_scalar::<_,i64>("select count(*) from live_setups").fetch_one(&pool).await.unwrap(),2);
        let advanced=db::get_adaptive_checkpoint(&pool,active.slot_id,INSTRUMENT).await.unwrap().unwrap();
        assert_eq!(advanced.state,next_state);
        assert_eq!(sqlx::query_scalar::<_,String>("select notification_delivery_status from live_setups where id::bigint=$1").bind(saturated.inserted[0].id).fetch_one(&pool).await.unwrap(),"skipped_capacity");
        assert_eq!(sqlx::query_scalar::<_,i64>("select capacity_skipped from adaptive_notification_budget").fetch_one(&pool).await.unwrap(),1);
        assert_eq!(sqlx::query_scalar::<_,i64>("select count(*) from adaptive_notification_outbox").fetch_one(&pool).await.unwrap(),1);
        // Saturation is the only non-error outcome. Unexpected SQL failures must
        // still roll back the setup and proposed cursor, never default-success.
        sqlx::raw_sql("create function reject_test_outbox() returns trigger language plpgsql as $$begin raise exception 'unexpected_outbox_test_error';end$$;create trigger a_reject_test_outbox before insert on adaptive_notification_outbox for each row execute function reject_test_outbox()")
            .execute(&pool).await.unwrap();
        assert!(db::commit_adaptive_scan_cas(&pool,&active,INSTRUMENT,Some(&advanced),&promotion_state(active.generation_id,now+120),&[adaptive_write(now-180)]).await.unwrap_err().to_string().contains("unexpected_outbox_test_error"));
        assert_eq!(db::get_adaptive_checkpoint(&pool,active.slot_id,INSTRUMENT).await.unwrap().unwrap(),advanced);
        assert_eq!(sqlx::query_scalar::<_,i64>("select count(*) from live_setups").fetch_one(&pool).await.unwrap(),2);
        sqlx::query("drop trigger a_reject_test_outbox on adaptive_notification_outbox").execute(&pool).await.unwrap();
        let job = claim_notifications(&pool,"delivery-old",1).await.unwrap().remove(0);
        assert_eq!(job.raw_close_unix,now);
        assert_eq!(job.expires_at_unix,now+3600);
        assert!(notification_permitted(&pool,"delivery-old",&job).await.unwrap());
        assert!(claim_notifications(&pool,"delivery-new",1).await.unwrap().is_empty());
        sqlx::query("update adaptive_notification_outbox set lease_until=clock_timestamp()-interval '1 second'").execute(&pool).await.unwrap();
        let retried = claim_notifications(&pool,"delivery-new",1).await.unwrap().remove(0);
        assert_ne!(retried.lease_token,job.lease_token);
        assert_eq!(retried.expires_at_unix,job.expires_at_unix);
        assert!(!finish_notification(&pool,"delivery-old",&job,NotificationOutcome::Sent).await.unwrap());
        assert!(db::stop(&pool,OWNER,ALGORITHM).await.unwrap());
        assert!(!notification_permitted(&pool,"delivery-new",&retried).await.unwrap());
        assert!(finish_notification(&pool,"delivery-new",&retried,NotificationOutcome::Skipped).await.unwrap());
        assert_eq!(sqlx::query_scalar::<_,i64>("select pending_jobs from adaptive_notification_budget").fetch_one(&pool).await.unwrap(),0);
        assert_eq!(sqlx::query_scalar::<_,i64>("select pending_bytes from adaptive_notification_budget").fetch_one(&pool).await.unwrap(),0);
    }).await;
}

#[tokio::test]
#[ignore = "requires isolated TEST_DATABASE_URL ending in _test with CREATEDB"]
async fn fixed_setup_and_outbox_use_config_and_global_fences_without_breaking_reference_insert() {
    use db::outbox::*;
    isolated(|pool| async move {
        setups_db::migrate::ensure_migrations(&pool).await.unwrap();
        let _active = accepted(&pool).await;
        setups_db::settings_set(&pool,"notifications",&json!({"maxAgeSeconds":3600})).await.unwrap();
        let now = chrono::Utc::now().timestamp().div_euclid(60)*60;
        let setup = adaptive_write(now-60).setup;
        assert!(commit_fixed_setup(&pool,"bybit","linear","BTCUSDT",&setup).await.unwrap().is_some());
        assert!(commit_fixed_setup(&pool,"bybit","linear","BTCUSDT",&setup).await.unwrap().is_none());
        let job = claim_notifications(&pool,"fixed-delivery",1).await.unwrap().remove(0);
        assert!(job.slot_id.is_none() && job.fixed_config.is_some());
        assert_eq!(job.payload["externalName"],"accepted external");
        assert!(notification_permitted(&pool,"fixed-delivery",&job).await.unwrap());
        setups_db::settings_set(&pool,&format!("algorithm:{ALGORITHM}"),&json!({"enabled":true,"params":{"windowMinutes":10},"externalName":"changed"})).await.unwrap();
        assert!(!notification_permitted(&pool,"fixed-delivery",&job).await.unwrap());
        assert!(commit_fixed_setup(&pool,"bybit","linear","BTCUSDT",&adaptive_write(now-120).setup).await.unwrap().is_none());
        setups_db::settings_set(&pool,&format!("algorithm:{ALGORITHM}"),&json!({"enabled":true,"externalName":"changed"})).await.unwrap();
        assert!(notification_permitted(&pool,"fixed-delivery",&job).await.unwrap());
        setups_db::settings_set(&pool,"live",&json!({"enabled":false})).await.unwrap();
        assert!(!notification_permitted(&pool,"fixed-delivery",&job).await.unwrap());
        assert!(finish_notification(&pool,"fixed-delivery",&job,NotificationOutcome::Skipped).await.unwrap());
        assert!(setups_db::runs::insert_live_setup(&pool,"bybit","linear","BTCUSDT",&adaptive_write(now-180).setup).await.unwrap().is_some());
        assert_eq!(sqlx::query_scalar::<_,i64>("select count(*) from adaptive_notification_outbox").fetch_one(&pool).await.unwrap(),0);
    }).await;
}

#[tokio::test]
#[ignore = "requires isolated TEST_DATABASE_URL ending in _test with CREATEDB"]
async fn research_materialization_does_not_exclude_incumbent_live_checkpoint_commit() {
    isolated(|pool| async move {
        setups_db::migrate::ensure_migrations(&pool).await.unwrap();
        let active=accepted(&pool).await;
        let lease=scheduler::claim_owner(&pool,"materialization-isolation").await.unwrap().unwrap();
        let job=scheduler::prepare_next(&pool,&lease).await.unwrap().unwrap();
        sqlx::raw_sql("create function delay_test_materialization() returns trigger language plpgsql as $$begin perform pg_sleep(1);return new;end$$;create trigger delay_test_materialization before insert on replay_runs for each row execute function delay_test_materialization()")
            .execute(&pool).await.unwrap();
        let task_pool=pool.clone(); let task_job=job.clone();
        let task=tokio::spawn(async move {
            scheduler::persist_replay(&task_pool,&task_job,&json!(SpikeParams::default()),json!({"instruments":[{}]}),&[scheduler::ServiceReplayRun {
                exchange:"bybit".into(),category:"linear".into(),symbol:"BTCUSDT".into(),setups:json!([]),warmup_minutes:1,evaluated_minutes:1,
            }]).await
        });
        tokio::time::timeout(Duration::from_secs(2),async {
            loop {
                let sleeping:bool=sqlx::query_scalar("select exists(select 1 from pg_stat_activity where datname=current_database() and wait_event='PgSleep')").fetch_one(&pool).await.unwrap();
                if sleeping {break;}
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }).await.expect("materialization did not reach fixture delay");
        let result=tokio::time::timeout(Duration::from_millis(300),db::commit_adaptive_scan_cas(&pool,&active,INSTRUMENT,None,&promotion_state(active.generation_id,CUTOFF),&[])).await;
        assert!(task.await.unwrap().unwrap().is_some());
        assert!(result.expect("live checkpoint waited for research bulk transaction").unwrap().applied);
        scheduler::release(&pool,&lease).await.unwrap();
    }).await;
}

#[tokio::test]
#[ignore = "requires isolated TEST_DATABASE_URL ending in _test with CREATEDB"]
async fn stop_cleanup_targets_unfinished_ids_and_retains_terminal_mixed_children() {
    isolated(|pool| async move {
        setups_db::migrate::ensure_migrations(&pool).await.unwrap();
        let slot = configured(&pool, OWNER, ALGORITHM).await;
        let cleanup_lease = scheduler::claim_owner(&pool, "stop-cleanup").await.unwrap().unwrap();
        assert_eq!(cleanup_lease.owner_id, OWNER);
        let current_generation = generation(&pool, OWNER, ALGORITHM, slot, "running").await;
        let active_study = study(&pool, OWNER, ALGORITHM, "running", Some(current_generation), Some(slot)).await;
        let old_generation = generation(&pool, OWNER, ALGORITHM, slot, "completed").await;
        let nonpass = study(&pool, OWNER, ALGORITHM, "completed", Some(old_generation), Some(slot)).await;
        let old_failed = study(&pool, OWNER, ALGORITHM, "failed", Some(old_generation), Some(slot)).await;
        let mixed = batch(&pool, ALGORITHM, Some(current_generation)).await;
        let completed_child = run(&pool, mixed, ALGORITHM, "done", Some(current_generation)).await;
        let failed_child = run(&pool, mixed, ALGORITHM, "failed", Some(current_generation)).await;
        let active_child = run(&pool, mixed, ALGORITHM, "running", Some(current_generation)).await;
        let racing_child = run(&pool, mixed, ALGORITHM, "running", Some(current_generation)).await;
        let unfinished_batch = batch(&pool, ALGORITHM, Some(current_generation)).await;
        let queued_child = run(&pool, unfinished_batch, ALGORITHM, "queued", Some(current_generation)).await;
        sqlx::query("update continuous_generations set study_id=$2,replay_batch_id=$3 where id=$1")
            .bind(current_generation).bind(active_study).bind(mixed).execute(&pool).await.unwrap();
        sqlx::query("insert into replay_quality_intervals(run_id,from_unix,to_unix,reason) values($1,0,60,'sentinel'),($2,0,60,'sentinel')")
            .bind(completed_child).bind(active_child).execute(&pool).await.unwrap();
        let manual = study(&pool, OWNER, OTHER_ALGORITHM, "queued", None, None).await;
        let manual_batch = batch(&pool, OTHER_ALGORITHM, None).await;
        let manual_run = run(&pool, manual_batch, OTHER_ALGORITHM, "queued", None).await;
        let other_slot = configured(&pool, OWNER, OTHER_ALGORITHM).await;
        let other_generation = generation(&pool, OWNER, OTHER_ALGORITHM, other_slot, "completed").await;
        let other_study = study(&pool, OWNER, OTHER_ALGORITHM, "failed", Some(other_generation), Some(other_slot)).await;
        let tenant_slot = configured(&pool, "other-owner", ALGORITHM).await;
        let tenant_generation = generation(&pool, "other-owner", ALGORITHM, tenant_slot, "running").await;
        let tenant_study = study(&pool, "other-owner", ALGORITHM, "running", Some(tenant_generation), Some(tenant_slot)).await;
        assert!(db::stop(&pool, OWNER, ALGORITHM).await.unwrap());
        let targets: Value = sqlx::query_scalar("select cleanup_targets from continuous_processes where owner_id=$1 and algorithm_id=$2")
            .bind(OWNER).bind(ALGORITHM).fetch_one(&pool).await.unwrap();
        assert_eq!(ids(&targets["studies"]), BTreeSet::from([active_study]));
        assert_eq!(ids(&targets["runs"]), BTreeSet::from([i64::from(active_child),i64::from(racing_child),i64::from(queued_child)]));
        assert_eq!(ids(&targets["batches"]), BTreeSet::from([i64::from(unfinished_batch)]));
        assert_eq!(optimization::get(&pool, active_study).await.unwrap().unwrap().status, "cancel_requested");
        assert!(db::stop(&pool, OWNER, ALGORITHM).await.unwrap());
        let repeated: Value = sqlx::query_scalar("select cleanup_targets from continuous_processes where owner_id=$1 and algorithm_id=$2")
            .bind(OWNER).bind(ALGORITHM).fetch_one(&pool).await.unwrap();
        assert_eq!(repeated, targets, "second Stop must not lose IDs after status transitions");
        // Simulated joined/drained executor: one selected child wins completion.
        sqlx::query("update replay_runs set status='cancelled' where id=any($1)")
            .bind(vec![active_child,queued_child]).execute(&pool).await.unwrap();
        sqlx::query("update replay_runs set status='done' where id=$1")
            .bind(racing_child).execute(&pool).await.unwrap();
        cleanup_owned(&pool, &cleanup_lease, ALGORITHM).await.unwrap();
        assert!(optimization::get(&pool, active_study).await.unwrap().is_none());
        for (id, status) in [(nonpass,"completed"),(old_failed,"failed"),(manual,"queued"),(other_study,"failed"),(tenant_study,"running")] {
            assert_eq!(optimization::get(&pool,id).await.unwrap().unwrap().status,status);
        }
        for (id, status) in [(completed_child,"done"),(racing_child,"done"),(failed_child,"failed"),(manual_run,"queued")] {
            assert_eq!(sqlx::query_scalar::<_,String>("select status from replay_runs where id=$1").bind(id).fetch_one(&pool).await.unwrap(),status);
        }
        for id in [active_child,queued_child] {
            assert!(!sqlx::query_scalar::<_,bool>("select exists(select 1 from replay_runs where id=$1)").bind(id).fetch_one(&pool).await.unwrap());
        }
        assert!(sqlx::query_scalar::<_,bool>("select exists(select 1 from replay_batches where id=$1)").bind(mixed).fetch_one(&pool).await.unwrap());
        assert!(!sqlx::query_scalar::<_,bool>("select exists(select 1 from replay_batches where id=$1)").bind(unfinished_batch).fetch_one(&pool).await.unwrap());
        assert_eq!(sqlx::query_scalar::<_,i64>("select count(*) from replay_quality_intervals where run_id=$1").bind(completed_child).fetch_one(&pool).await.unwrap(),1);
        assert_eq!(sqlx::query_scalar::<_,i64>("select count(*) from replay_quality_intervals where run_id=$1").bind(active_child).fetch_one(&pool).await.unwrap(),0);
        assert_eq!(db::get(&pool,OWNER,ALGORITHM).await.unwrap().unwrap().status,"stopped");
        assert_eq!(db::get(&pool,OWNER,OTHER_ALGORITHM).await.unwrap().unwrap().status,"running");
        assert_eq!(db::get(&pool,"other-owner",ALGORITHM).await.unwrap().unwrap().status,"running");
        assert_eq!(sqlx::query_scalar::<_,String>("select status from continuous_generations where id=$1").bind(current_generation).fetch_one(&pool).await.unwrap(),"cancelled");
    }).await;
}

#[tokio::test]
#[ignore = "requires isolated TEST_DATABASE_URL ending in _test with CREATEDB"]
async fn heartbeat_does_not_wait_behind_held_replay_persistence_mutation() {
    isolated(|pool| async move {
        setups_db::migrate::ensure_migrations(&pool).await.unwrap();
        configured(&pool, OWNER, ALGORITHM).await;
        let lease = scheduler::claim_owner(&pool, "persist-owner").await.unwrap().unwrap();
        let job = scheduler::prepare_next(&pool, &lease).await.unwrap().unwrap();
        // Stall ACTUAL persist_replay after it acquired its mutation/fence lock,
        // rather than artificially taking the legacy owner row lock in this test.
        const GATE: i64 = 81_330_901;
        sqlx::raw_sql(&format!("create function hold_test_persist() returns trigger language plpgsql as $$ begin perform pg_advisory_xact_lock({GATE}::bigint); return new; end $$; create trigger hold_test_persist before insert on replay_batches for each row execute function hold_test_persist()"))
            .execute(&pool).await.unwrap();
        let mut gate = pool.begin().await.unwrap();
        sqlx::query("select pg_advisory_xact_lock($1)").bind(GATE).execute(&mut *gate).await.unwrap();
        let persist_pool = pool.clone();
        let persist = tokio::task::spawn_local(async move {
            scheduler::persist_replay(&persist_pool, &job, &json!(SpikeParams::default()),
                json!({"instruments":[{"exchange":"bybit","category":"linear","symbol":"BTCUSDT"}]}),
                &[scheduler::ServiceReplayRun { exchange:"bybit".into(), category:"linear".into(),
                    symbol:"BTCUSDT".into(), setups:json!([]), warmup_minutes:1, evaluated_minutes:1 }]).await
        });
        let reached_gate = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let waiting: bool = sqlx::query_scalar("select exists(select 1 from pg_locks where database=(select oid from pg_database where datname=current_database()) and locktype='advisory' and classid=0 and objid=$1::bigint::oid and not granted)")
                    .bind(GATE).fetch_one(&pool).await.unwrap();
                if waiting { break; }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }).await;
        let heartbeat = if reached_gate.is_ok() {
            Some(tokio::time::timeout(Duration::from_secs(1), scheduler::heartbeat(&pool, &lease)).await)
        } else { None };
        // Always release/join before asserting, including the expected old-code
        // red path, so no blocked task/transaction can escape child DB cleanup.
        gate.rollback().await.unwrap();
        let persisted = tokio::time::timeout(Duration::from_secs(5), persist).await.unwrap().unwrap().unwrap();
        assert!(persisted.is_some(), "the controlled real persist operation must succeed");
        assert!(reached_gate.is_ok(), "persist never reached the controlled held transaction");
        assert!(matches!(heartbeat, Some(Ok(Ok(true)))), "heartbeat waited behind held owner mutation or lost lease: {heartbeat:?}");
        assert!(scheduler::heartbeat(&pool,&lease).await.unwrap());
    }).await;
}

#[tokio::test]
#[ignore = "requires isolated TEST_DATABASE_URL ending in _test with CREATEDB"]
async fn stale_cleanup_after_takeover_cannot_read_or_delete_new_stop_targets() {
    isolated(|pool| async move {
        setups_db::migrate::ensure_migrations(&pool).await.unwrap();
        configured(&pool, OWNER, ALGORITHM).await;
        let stale = scheduler::claim_owner(&pool, "old-cleaner").await.unwrap().unwrap();
        let first = scheduler::prepare_next(&pool, &stale).await.unwrap().unwrap();
        assert!(db::stop(&pool,OWNER,ALGORITHM).await.unwrap());
        sqlx::query("update continuous_owner_lanes set lease_until=clock_timestamp()-interval '1 second' where owner_id=$1")
            .bind(OWNER).execute(&pool).await.unwrap();
        let current = scheduler::claim_owner(&pool, "new-cleaner").await.unwrap().unwrap();
        assert!(current.token > stale.token);
        assert!(!scheduler::heartbeat(&pool,&stale).await.unwrap());
        cleanup_owned(&pool,&current,ALGORITHM).await.unwrap();
        assert!(optimization::get(&pool,first.study_id).await.unwrap().is_none());
        assert!(db::start(&pool,OWNER,ALGORITHM).await.unwrap());
        let newer = scheduler::prepare_next(&pool,&current).await.unwrap().unwrap();
        let new_batch = batch(&pool,ALGORITHM,Some(newer.generation_id)).await;
        let new_run = run(&pool,new_batch,ALGORITHM,"running",Some(newer.generation_id)).await;
        assert!(db::stop(&pool,OWNER,ALGORITHM).await.unwrap());
        let targets: Value = sqlx::query_scalar("select cleanup_targets from continuous_processes where owner_id=$1 and algorithm_id=$2")
            .bind(OWNER).bind(ALGORITHM).fetch_one(&pool).await.unwrap();
        assert!(ids(&targets["studies"]).contains(&newer.study_id));
        assert!(ids(&targets["runs"]).contains(&i64::from(new_run)));
        // Valid but locked new targets: a rejected old token must not even wait
        // to read/lock these, let alone consume/delete another Stop's work.
        let mut held_targets = pool.begin().await.unwrap();
        sqlx::query("select algorithm_id from continuous_processes where owner_id=$1 and algorithm_id=$2 for update")
            .bind(OWNER).bind(ALGORITHM).fetch_one(&mut *held_targets).await.unwrap();
        let stale_pool = pool.clone();
        let mut cleanup = tokio::task::spawn_local(async move { cleanup_owned(&stale_pool,&stale,ALGORITHM).await });
        let immediate = tokio::time::timeout(Duration::from_secs(1), &mut cleanup).await;
        let rejected_before_target_access = immediate.is_ok();
        held_targets.rollback().await.unwrap();
        let stale_result = match immediate {
            Ok(result) => result.unwrap(),
            Err(_) => tokio::time::timeout(Duration::from_secs(5),cleanup).await.unwrap().unwrap(),
        };
        let remains = optimization::get(&pool,newer.study_id).await.unwrap().is_some();
        let run_remains: bool = sqlx::query_scalar("select exists(select 1 from replay_runs where id=$1)")
            .bind(new_run).fetch_one(&pool).await.unwrap();
        let after: Value = sqlx::query_scalar("select cleanup_targets from continuous_processes where owner_id=$1 and algorithm_id=$2")
            .bind(OWNER).bind(ALGORITHM).fetch_one(&pool).await.unwrap();
        assert!(rejected_before_target_access && remains && run_remains && after==targets,
            "stale cleanup accessed targets: rejected_before_lock={rejected_before_target_access}, study_remains={remains}, run_remains={run_remains}, targets_unchanged={}, result={stale_result:?}",after==targets);
        // Positive control: the current token can drain the same target set.
        cleanup_owned(&pool,&current,ALGORITHM).await.unwrap();
        assert!(optimization::get(&pool,newer.study_id).await.unwrap().is_none());
        assert!(!sqlx::query_scalar::<_,bool>("select exists(select 1 from replay_runs where id=$1)")
            .bind(new_run).fetch_one(&pool).await.unwrap());
    }).await;
}
