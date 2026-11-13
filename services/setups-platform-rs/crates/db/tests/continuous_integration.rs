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
        assert_eq!(setups_db::migrate::ensure_migrations(&pool).await.unwrap(), "0 applied, 4 up to date");
        let fixed_after: Value = sqlx::query_scalar("select to_jsonb(s)-array['adaptive_slot_id','adaptive_generation_id','recommendation','external_name'] from live_setups s where id=$1")
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
