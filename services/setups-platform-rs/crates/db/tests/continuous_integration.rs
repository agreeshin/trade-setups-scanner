//! Opt-in DB-contract tests, not an API/executor end-to-end test. Each case
//! creates/drops its own child database; the supplied *_test database is only
//! the administrative connection. Lifecycle fixture SQL deliberately avoids IO.
use serde_json::{Value, json};
use setups_db::continuous::scheduler;
use setups_db::{continuous as db, optimization};
use setups_domain::{AlgorithmParams, SpikeParams, SpikeSetup};
use sqlx::Row;
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
    db::profiles::set_permission(pool, owner, algorithm, true)
        .await
        .unwrap();
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
        "selectedCell":selected,"meanExpectancy":0.21,"immutable":"accepted-profile"});
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
    sqlx::query("insert into adaptive_profiles(id,owner_id,algorithm_id,min_setups_per_day,accepted_generation_id) select id,owner_id,algorithm_id,min_setups_per_day,$2 from continuous_slots where id=$1")
        .bind(slot).bind(generation).execute(pool).await.unwrap();
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

#[tokio::test]
#[ignore = "requires isolated TEST_DATABASE_URL ending in _test"]
async fn n8_migration_preserves_six_incumbents_17_to_22_and_all_durable_links() {
    isolated(|pool|async move {
        for (name,source) in [
            ("0013_continuous_studies.sql",include_str!("../migrations/0013_continuous_studies.sql")),
            ("0014_adaptive_live_readiness.sql",include_str!("../migrations/0014_adaptive_live_readiness.sql")),
            ("0015_continuous_history_cleanup.sql",include_str!("../migrations/0015_continuous_history_cleanup.sql")),
            ("0016_independent_live_initialization.sql",include_str!("../migrations/0016_independent_live_initialization.sql")),
            ("0017_continuous_resume.sql",include_str!("../migrations/0017_continuous_resume.sql")),
            ("0018_live_readmission.sql",include_str!("../migrations/0018_live_readmission.sql")),
            ("0019_telegram_relay.sql",include_str!("../migrations/0019_telegram_relay.sql")),
            ("0020_study_expectancy.sql",include_str!("../migrations/0020_study_expectancy.sql")),
            ("0021_telegram_v2_queue.sql",include_str!("../migrations/0021_telegram_v2_queue.sql")),
            ("0022_historical_expectancy.sql",include_str!("../migrations/0022_historical_expectancy.sql")),
        ] {
            sqlx::raw_sql(source).execute(&pool).await.unwrap();
            sqlx::query("insert into kysely_migration values($1,'fixture')").bind(name).execute(&pool).await.unwrap();
        }
        sqlx::query("insert into continuous_owner_lanes(owner_id) values('admin')").execute(&pool).await.unwrap();
        sqlx::query("insert into continuous_processes(owner_id,algorithm_id,status,desired_running,config_revision,config) values('admin',$1,'running',true,7,$2)")
            .bind(ALGORITHM).bind(config(ALGORITHM)).execute(&pool).await.unwrap();
        for generation in 17i64..=22 {
            let slot:i64=sqlx::query_scalar("insert into continuous_slots(owner_id,algorithm_id,min_setups_per_day,status) values('admin',$1,$2::text::numeric,'active') returning id")
                .bind(ALGORITHM).bind((generation-16).to_string()).fetch_one(&pool).await.unwrap();
            sqlx::query("insert into continuous_generations(id,owner_id,algorithm_id,slot_id,config_revision,lease_token,status,phase,spec,result,accepted,execution_started_at,observation_cutoff,live_initialization,live_readiness,live_init_attempts,live_init_status) overriding system value values($1,'admin',$2,$3,7,1,'completed','completed',$4,$5,true,now(),$6,$7,$8,2,'active')")
                .bind(generation).bind(ALGORITHM).bind(slot).bind(spec(ALGORITHM)).bind(json!({"immutable":generation,"meanExpectancy":0.21,"selectedCell":{"tpPct":14,"slPct":4}})).bind(CUTOFF)
                .bind(json!({"version":1,"anchorCloseUnix":CUTOFF,"detectorFromOpenUnix":CUTOFF-86400,"emitFromCloseUnix":CUTOFF+60}))
                .bind(json!([{"instrumentKey":INSTRUMENT,"status":"ready","reason":null,"publicationFloorCloseUnix":CUTOFF}])).execute(&pool).await.unwrap();
            let study=study(&pool,OWNER,ALGORITHM,"completed",Some(generation),Some(slot)).await;
            sqlx::query("update continuous_generations set study_id=$2 where id=$1").bind(generation).bind(study).execute(&pool).await.unwrap();
            sqlx::query("update continuous_slots set active_generation_id=$2,study_id=$3,live_watermark=$4 where id=$1")
                .bind(slot).bind(generation).bind(study).bind(CUTOFF).execute(&pool).await.unwrap();
            sqlx::query("insert into adaptive_live_checkpoints(slot_id,instrument_key,generation_id,state) values($1,$2,$3,$4)")
                .bind(slot).bind(INSTRUMENT).bind(generation).bind(json!({"generationId":generation,"nextOpenUnix":CUTOFF+600,"lastRawOpenUnix":CUTOFF})).execute(&pool).await.unwrap();
            let now=chrono::Utc::now().timestamp();
            let setup:i32=sqlx::query_scalar("insert into live_setups(exchange_code,category,symbol,algorithm_id,algorithm_version,params,open_time,close_time,close_price,adaptive_slot_id,adaptive_generation_id,recommendation) values('bybit','linear','BTCUSDT',$1,'1.1.1','{}',to_timestamp($2),to_timestamp($3),'100',$4,$5,$6) returning id")
                .bind(ALGORITHM).bind((now-60)as f64).bind(now as f64).bind(slot).bind(generation).bind(json!({"snapshot":generation})).fetch_one(&pool).await.unwrap();
            sqlx::query("insert into adaptive_notification_outbox(setup_id,owner_id,algorithm_id,slot_id,generation_id,config_revision,raw_close_unix,expires_at,payload) values($1,'admin',$2,$3,$4,7,$5,to_timestamp($6),'{}')")
                .bind(setup).bind(ALGORITHM).bind(slot).bind(generation).bind(now).bind((now+3600)as f64).execute(&pool).await.unwrap();
        }
        async fn snapshot(pool:&PgPool)->Value {
            sqlx::query_scalar("select jsonb_build_object('slots',(select jsonb_agg(to_jsonb(s) order by id) from continuous_slots s),'generations',(select jsonb_agg(to_jsonb(g) order by id) from continuous_generations g),'cursors',(select jsonb_agg(to_jsonb(c) order by slot_id,instrument_key) from adaptive_live_checkpoints c),'signals',(select jsonb_agg(to_jsonb(l) order by id) from live_setups l),'outbox',(select jsonb_agg(to_jsonb(o) order by id) from adaptive_notification_outbox o))")
                .fetch_one(pool).await.unwrap()
        }
        let before=snapshot(&pool).await;
        setups_db::migrate::ensure_migrations(&pool).await.unwrap();
        assert_eq!(snapshot(&pool).await,before,"migration must not rewrite any incumbent data");
        assert_eq!(sqlx::query_scalar::<_,Vec<i64>>("select array_agg(accepted_generation_id order by accepted_generation_id) from adaptive_profiles").fetch_one(&pool).await.unwrap(),vec![17,18,19,20,21,22]);
        assert_eq!(sqlx::query_scalar::<_,i64>("select count(*) from adaptive_profiles where desired_running and control_revision=7").fetch_one(&pool).await.unwrap(),6);
        assert_eq!(sqlx::query_scalar::<_,i64>("select count(*) from adaptive_notification_outbox o join adaptive_live_controls p on p.id=o.slot_id where p.desired_running and p.config_revision=o.config_revision").fetch_one(&pool).await.unwrap(),6);
        setups_db::migrate::ensure_migrations(&pool).await.unwrap();assert_eq!(snapshot(&pool).await,before);
        assert_eq!(db::profiles::limits(&pool,OWNER).await.unwrap(),db::profiles::ProfileLimits{total_profiles:50,running_profiles:30});
    }).await;
}

#[tokio::test]
#[ignore = "requires isolated TEST_DATABASE_URL ending in _test"]
async fn maintenance_drained_configure_preserves_running_research_and_independent_live() {
    isolated(|pool| async move {
        setups_db::migrate::ensure_migrations(&pool).await.unwrap();
        let active = accepted(&pool).await;
        let checkpoint = json!({"generationId":active.generation_id,"nextOpenUnix":CUTOFF});
        assert!(db::commit_adaptive_scan(&pool, &active, INSTRUMENT, None, &checkpoint, &[]).await.unwrap().applied);
        sqlx::query("update continuous_processes set active_generation_id=null,active_study_id=null,status='running',phase='queued',error='{\"code\":\"retained_backoff\"}',retry_count=2,next_attempt_at=to_timestamp(123),next_slot=7 where owner_id=$1 and algorithm_id=$2")
            .bind(OWNER).bind(ALGORITHM).execute(&pool).await.unwrap();
        assert_eq!(db::maintenance::set_paused(&pool, OWNER, true).await.unwrap().state, "drained");
        let before: Value = sqlx::query_scalar("select to_jsonb(p) from continuous_processes p where owner_id=$1 and algorithm_id=$2")
            .bind(OWNER).bind(ALGORITHM).fetch_one(&pool).await.unwrap();
        let live_before = maintenance_live_snapshot(&pool).await;
        let slot_before: Value = sqlx::query_scalar("select to_jsonb(s)-'enabled' from continuous_slots s where id=$1")
            .bind(active.slot_id).fetch_one(&pool).await.unwrap();
        let mut next = before["config"].clone();
        next["minSetupsPerDay"] = json!([8, 16]);
        next["adaptiveEnabled"] = json!(false);
        next["study"]["trendMode"] = json!("forecast");
        next["study"]["forecast"] = json!({"version":1,"method":"decayed_probability","halfLifeDays":14,"kappa":10,"priorPolicy":"gross_break_even"});
        assert!(db::configure(&pool, OWNER, ALGORITHM, &next, &[8.0, 16.0]).await.unwrap());
        let after: Value = sqlx::query_scalar("select to_jsonb(p) from continuous_processes p where owner_id=$1 and algorithm_id=$2")
            .bind(OWNER).bind(ALGORITHM).fetch_one(&pool).await.unwrap();
        let mut expected_config = next.clone();
        expected_config["adaptiveEnabled"] = json!(true);
        assert_eq!(after["config"], expected_config, "persist exact research config with the independent permission projection");
        assert_eq!(after["config_revision"], before["config_revision"].as_i64().unwrap() + 1);
        let mut expected_process = before.clone();
        for key in ["config", "config_revision", "updated_at"] {
            expected_process[key] = after[key].clone();
        }
        assert_eq!(after, expected_process, "configuration must preserve all operational/retry/scheduling fields");
        assert_eq!(maintenance_live_snapshot(&pool).await, live_before);
        assert_eq!(sqlx::query_scalar::<_, Value>("select to_jsonb(s)-'enabled' from continuous_slots s where id=$1")
            .bind(active.slot_id).fetch_one(&pool).await.unwrap(), slot_before);
        assert_eq!(sqlx::query_as::<_, (f64, bool)>("select min_setups_per_day::double precision,enabled from continuous_slots where owner_id=$1 and algorithm_id=$2 order by min_setups_per_day")
            .bind(OWNER).bind(ALGORITHM).fetch_all(&pool).await.unwrap(), vec![(4.0, false), (8.0, true), (16.0, true)]);
        assert!(db::adaptive_notification_allowed(&pool, &active).await.unwrap(), "queue removal cannot fence the live profile");
        assert!(db::configure(&pool, OWNER, ALGORITHM, &next, &[8.0, 16.0]).await.unwrap());
        assert_eq!(sqlx::query_scalar::<_, Value>("select to_jsonb(p) from continuous_processes p where owner_id=$1 and algorithm_id=$2")
            .bind(OWNER).bind(ALGORITHM).fetch_one(&pool).await.unwrap(), after, "same research config does not increment the revision again");
        assert!(scheduler::claim_owner(&pool, "paused-config").await.unwrap().is_none());
        db::maintenance::set_paused(&pool, OWNER, false).await.unwrap();
        assert_eq!(sqlx::query_scalar::<_, i32>("select retry_count from continuous_processes where owner_id=$1 and algorithm_id=$2")
            .bind(OWNER).bind(ALGORITHM).fetch_one(&pool).await.unwrap(), 2);
        let lease = scheduler::claim_owner(&pool, "new-config").await.unwrap().unwrap();
        let job = scheduler::prepare_next(&pool, &lease).await.unwrap().unwrap();
        assert_eq!(job.config, expected_config);
        assert_eq!(job.config_revision, after["config_revision"].as_i64().unwrap());
        assert_eq!(job.spec["trendMode"], "forecast");
        assert_eq!(job.spec["forecast"], next["study"]["forecast"]);
        assert_eq!(job.spec["minSetupsPerDay"], 16.0, "scheduler modulo-normalizes the preserved next_slot");
        assert_ne!(job.generation_id, active.generation_id);
        assert_ne!(job.study_id, active.study_id);
        assert_eq!(sqlx::query_scalar::<_, Value>("select spec from optimization_studies where id=$1").bind(job.study_id).fetch_one(&pool).await.unwrap(), job.spec);
        assert_eq!(sqlx::query_scalar::<_, i32>("select retry_count from continuous_processes where owner_id=$1 and algorithm_id=$2")
            .bind(OWNER).bind(ALGORITHM).fetch_one(&pool).await.unwrap(), 2, "admission is not a hidden Start/retry reset");
        assert_eq!(maintenance_live_snapshot(&pool).await, live_before);
        scheduler::release(&pool, &lease).await.unwrap();
    }).await;
}

async fn maintenance_live_snapshot(pool: &PgPool) -> Value {
    sqlx::query_scalar("select jsonb_build_object('profiles',(select jsonb_agg(to_jsonb(p) order by id) from adaptive_profiles p),'controls',(select jsonb_agg(to_jsonb(c) order by id) from adaptive_live_controls c),'permissions',(select jsonb_agg(to_jsonb(p) order by owner_id,algorithm_id) from adaptive_live_permissions p),'checkpoints',(select jsonb_agg(to_jsonb(c) order by slot_id,instrument_key) from adaptive_live_checkpoints c),'generations',(select jsonb_agg(to_jsonb(g) order by id) from continuous_generations g where exists(select 1 from adaptive_profiles p where p.accepted_generation_id=g.id)),'settings',(select jsonb_agg(to_jsonb(s) order by key) from settings s))")
        .fetch_one(pool).await.unwrap()
}

#[tokio::test]
#[ignore = "requires isolated TEST_DATABASE_URL ending in _test"]
async fn configure_defers_while_research_may_be_in_flight_and_applies_on_start() {
    isolated(|pool| async move {
        setups_db::migrate::ensure_migrations(&pool).await.unwrap();
        let slot = configured(&pool, OWNER, ALGORITHM).await;
        let mut next = config(ALGORITHM);
        next["study"]["trendMode"] = json!("forecast");
        next["minSetupsPerDay"] = json!([8.0]);
        async fn snapshot(pool: &PgPool) -> (Value, i64, Option<Value>, Vec<f64>) {
            let row = sqlx::query("select config,config_revision,pending_config from continuous_processes where owner_id=$1 and algorithm_id=$2")
                .bind(OWNER).bind(ALGORITHM).fetch_one(pool).await.unwrap();
            let enabled: Vec<f64> = sqlx::query_scalar("select s.min_setups_per_day::double precision from continuous_slots s where s.owner_id=$1 and s.algorithm_id=$2 and s.enabled order by s.min_setups_per_day")
                .bind(OWNER).bind(ALGORITHM).fetch_all(pool).await.unwrap();
            (row.try_get("config").unwrap(), row.try_get("config_revision").unwrap(), row.try_get("pending_config").unwrap(), enabled)
        }
        let (baseline, revision, pending, enabled) = snapshot(&pool).await;
        assert_eq!(pending, None);
        assert_eq!(enabled, vec![4.0]);
        async fn deferred(pool: &PgPool, next: &Value, baseline: &Value, revision: i64) {
            assert!(db::configure(pool, OWNER, ALGORITHM, next, &[8.0]).await.unwrap());
            let (config, revision_now, pending, enabled) = snapshot(pool).await;
            assert_eq!(pending.as_ref(), Some(next), "save must land in pending_config");
            assert_eq!(config, *baseline, "active chain keeps its parameters");
            assert_eq!(revision_now, revision, "revision fences stay untouched");
            assert_eq!(enabled, vec![4.0], "slot membership is not rebuilt while research runs");
        }
        deferred(&pool, &next, &baseline, revision).await; // No durable maintenance row.
        db::maintenance::set_paused(&pool, OWNER, false).await.unwrap();
        deferred(&pool, &next, &baseline, revision).await;
        db::maintenance::set_paused(&pool, OWNER, true).await.unwrap();
        let other_slot = configured(&pool, OWNER, OTHER_ALGORITHM).await;
        let other = generation(&pool, OWNER, OTHER_ALGORITHM, other_slot, "running").await;
        assert_eq!(db::maintenance::status(&pool, OWNER).await.unwrap().state, "draining");
        deferred(&pool, &next, &baseline, revision).await; // Owner-wide, even when another algorithm owns the chain.
        sqlx::query("update continuous_generations set status='completed' where id=$1").bind(other).execute(&pool).await.unwrap();
        sqlx::query("update continuous_processes set status='stopping' where owner_id=$1 and algorithm_id=$2").bind(OWNER).bind(ALGORITHM).execute(&pool).await.unwrap();
        assert!(!db::configure(&pool, OWNER, ALGORITHM, &next, &[8.0]).await.unwrap(), "stopping still rejects");
        let (config, revision_now, pending_now, enabled_now) = snapshot(&pool).await;
        assert_eq!((config, revision_now, pending_now, enabled_now), (baseline.clone(), revision, Some(next.clone()), vec![4.0]), "rejection must leave state unchanged");
        sqlx::query("update continuous_processes set status='running' where owner_id=$1 and algorithm_id=$2").bind(OWNER).bind(ALGORITHM).execute(&pool).await.unwrap();
        let terminal = generation(&pool, OWNER, ALGORITHM, slot, "completed").await;
        sqlx::query("update continuous_processes set active_generation_id=$3 where owner_id=$1 and algorithm_id=$2").bind(OWNER).bind(ALGORITHM).bind(terminal).execute(&pool).await.unwrap();
        assert_eq!(db::maintenance::status(&pool, OWNER).await.unwrap().state, "drained");
        deferred(&pool, &next, &baseline, revision).await; // Stale active head defers instead of reconfiguring.
        let terminal_study = study(&pool, OWNER, ALGORITHM, "completed", Some(terminal), Some(slot)).await;
        sqlx::query("update continuous_processes set active_generation_id=null,active_study_id=$3 where owner_id=$1 and algorithm_id=$2").bind(OWNER).bind(ALGORITHM).bind(terminal_study).execute(&pool).await.unwrap();
        deferred(&pool, &next, &baseline, revision).await;
        // A stopped process applies the deferred configuration on start: the next
        // admitted generation runs with the saved parameters.
        sqlx::query("update continuous_processes set desired_running=false,status='stopped',active_generation_id=null,active_study_id=null where owner_id=$1 and algorithm_id=$2").bind(OWNER).bind(ALGORITHM).execute(&pool).await.unwrap();
        assert!(db::start(&pool, OWNER, ALGORITHM).await.unwrap());
        let (config, revision_now, pending, enabled) = snapshot(&pool).await;
        assert_eq!(config, next, "start applies the deferred configuration");
        assert_eq!(revision_now, revision + 1);
        assert_eq!(pending, None);
        assert_eq!(enabled, vec![8.0], "slot membership is rebuilt from the deferred thresholds");
    }).await;
}

#[tokio::test]
#[ignore = "requires isolated TEST_DATABASE_URL ending in _test"]
async fn maintenance_configure_locks_durable_pause_and_serializes_concurrent_resume() {
    isolated(|pool| async move {
        setups_db::migrate::ensure_migrations(&pool).await.unwrap();
        configured(&pool, OWNER, ALGORITHM).await;
        db::maintenance::set_paused(&pool, OWNER, true)
            .await
            .unwrap();
        let mut next = config(ALGORITHM);
        next["study"]["trendMode"] = json!("forecast");
        let mut blocker = pool.begin().await.unwrap();
        sqlx::query(
            "select admission_paused from continuous_maintenance where owner_id=$1 for update",
        )
        .bind(OWNER)
        .fetch_one(&mut *blocker)
        .await
        .unwrap();
        let configured_pool = pool.clone();
        let configured = tokio::spawn(async move {
            db::configure(&configured_pool, OWNER, ALGORITHM, &next, &[4.0]).await
        });
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let free: bool = sqlx::query_scalar(
                    "select pg_try_advisory_xact_lock(hashtextextended($1,72423415))",
                )
                .bind(OWNER)
                .fetch_one(&pool)
                .await
                .unwrap();
                if !free {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect(
            "configure must hold the owner mutation lock while waiting for the maintenance row",
        );
        assert!(
            !configured.is_finished(),
            "durable maintenance row must be locked, not read optimistically"
        );
        let resumed_pool = pool.clone();
        let mut resumed =
            tokio::spawn(
                async move { db::maintenance::set_paused(&resumed_pool, OWNER, false).await },
            );
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut resumed)
                .await
                .is_err(),
            "resume must wait behind the same owner mutation fence"
        );
        blocker.commit().await.unwrap();
        assert!(configured.await.unwrap().unwrap());
        assert!(!resumed.await.unwrap().unwrap().admission_paused);
        let lease = scheduler::claim_owner(&pool, "after-fenced-resume")
            .await
            .unwrap()
            .unwrap();
        let job = scheduler::prepare_next(&pool, &lease)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(job.config_revision, 2);
        assert_eq!(job.spec["trendMode"], "forecast");
        scheduler::release(&pool, &lease).await.unwrap();
    })
    .await;
}

#[tokio::test]
#[ignore = "requires isolated TEST_DATABASE_URL ending in _test"]
async fn n8_independent_lifecycle_research_stop_new_id_profile_play_grants_and_fences() {
    isolated(|pool|async move {
        use db::profiles::{self,ProfileCommand,CommandOutcome};
        setups_db::migrate::ensure_migrations(&pool).await.unwrap();
        let active=accepted(&pool).await;
        let checkpoint=json!({"generationId":active.generation_id,"nextOpenUnix":CUTOFF});
        assert!(db::commit_adaptive_scan(&pool,&active,INSTRUMENT,None,&checkpoint,&[]).await.unwrap().applied);
        db::stop(&pool,OWNER,ALGORITHM).await.unwrap();
        assert!(db::adaptive_notification_allowed(&pool,&active).await.unwrap(),"research Stop cannot fence live");
        let lease=scheduler::claim_owner(&pool,"n8-cleanup").await.unwrap().unwrap();
        scheduler::drain_stops(&pool,&lease).await.unwrap();
        let mut next=config(ALGORITHM);next["minSetupsPerDay"]=json!([8]);next["adaptiveEnabled"]=json!(false);
        assert!(db::configure(&pool,OWNER,ALGORITHM,&next,&[8.0]).await.unwrap());
        assert!(profiles::permission(&pool,OWNER,ALGORITHM).await.unwrap(),"research config cannot change the live permission");
        assert_eq!(db::active_adaptive_slots(&pool,OWNER).await.unwrap()[0].generation_id,active.generation_id);
        assert_eq!(profiles::list(&pool,OWNER,None,0,50).await.unwrap()["profiles"][0]["inResearchQueue"],false);
        assert!(db::start(&pool,OWNER,ALGORITHM).await.unwrap());
        let job=scheduler::prepare_next(&pool,&lease).await.unwrap().unwrap();
        assert_ne!(job.study_id,active.study_id);assert_ne!(job.generation_id,active.generation_id);
        let request=ProfileCommand{expected_revision:1,expected_generation_id:active.generation_id,request_id:"n8-stop-00000001".into()};
        let stopped=match profiles::command(&pool,OWNER,active.slot_id,false,&request).await.unwrap() {CommandOutcome::Applied(v)=>v,_=>panic!("Stop rejected")};
        assert!(!db::adaptive_notification_allowed(&pool,&active).await.unwrap());
        assert_eq!(db::get_adaptive_checkpoint(&pool,active.slot_id,INSTRUMENT).await.unwrap().unwrap().state,checkpoint);
        assert!(scheduler::current(&pool,&job).await.unwrap(),"profile Stop cannot cancel research");
        assert_eq!(stopped["status"],"stopping");
        profiles::acknowledge_stops(&pool,OWNER).await.unwrap();
        let play=ProfileCommand{expected_revision:stopped["controlRevision"].as_i64().unwrap(),request_id:"n8-play-00000001".into(),..request.clone()};
        assert!(matches!(profiles::command(&pool,OWNER,active.slot_id,true,&play).await.unwrap(),CommandOutcome::Applied(_)));
        let pending=db::live::pending_adaptive_slots(&pool,OWNER).await.unwrap().remove(0);
        assert_eq!(pending.profile.generation_id,active.generation_id);assert_eq!(pending.profile.config_revision,3);
        let warm=db::live::claim_pending_warmup_at(&pool,&pending,"n8-warm",CUTOFF).await.unwrap().unwrap();
        assert!(db::live::renew_pending_warmup(&pool,&warm).await.unwrap()==db::live::WarmupRenewal::Renewed);
        assert!(db::live::report_warmup(&pool,&warm,"ready",Some(CUTOFF),None).await.unwrap());
        assert!(db::live::promote_pending(&pool,&warm,None,&[db::live::CheckpointExpectation{instrument_key:INSTRUMENT.into(),checkpoint:None}],CUTOFF,
            &[db::live::InitialAdaptiveCheckpoint{instrument_key:INSTRUMENT.into(),state:checkpoint.clone()}]).await.unwrap());
        assert_eq!(sqlx::query_scalar::<_,Value>("select state from adaptive_profile_stopped_checkpoints where profile_id=$1").bind(active.slot_id).fetch_one(&pool).await.unwrap(),checkpoint);
        assert!(!db::commit_adaptive_scan(&pool,&active,INSTRUMENT,None,&checkpoint,&[]).await.unwrap().applied,"old epoch remains fenced");
        sqlx::query("update continuous_generations set live_init_attempts=3,live_init_attempt_limit=3,live_init_status='exhausted' where id=$1").bind(active.generation_id).execute(&pool).await.unwrap();
        let grant=ProfileCommand{expected_revision:3,request_id:"n8-grant-00000001".into(),..request};
        let first=match profiles::command(&pool,OWNER,active.slot_id,true,&grant).await.unwrap() {CommandOutcome::Applied(v)=>v,_=>panic!("Play must grant")};
        assert_eq!(first["grantedAttempts"],3);assert_eq!(first["attemptsBefore"],3);assert_eq!(first["attemptLimit"],6);
        assert_eq!(match profiles::command(&pool,OWNER,active.slot_id,true,&grant).await.unwrap(){CommandOutcome::Applied(v)=>v,_=>panic!()},first);
        assert!(matches!(profiles::command(&pool,"another-user",active.slot_id,true,&grant).await.unwrap(),CommandOutcome::NotFound));
        assert!(db::cleanup::preview(&pool,OWNER).await.unwrap().blockers.contains(&"accepted_profile_references".into()));
        scheduler::release(&pool,&lease).await.unwrap();
    }).await;
}

#[tokio::test]
#[ignore = "requires isolated TEST_DATABASE_URL ending in _test"]
async fn n8_handoff_negative_outcomes_sticky_stop_ordering_and_configured_limits() {
    isolated(|pool|async move {
        use db::profiles::{self,ProfileCommand,CommandOutcome,ProfileLimits};
        setups_db::migrate::ensure_migrations(&pool).await.unwrap();
        profiles::configure_default_limits(&pool,ProfileLimits{total_profiles:2,running_profiles:1}).await.unwrap();
        let active=accepted(&pool).await;
        async fn handoff(pool:&PgPool,slot:i64)->i64 {
            let generation=generation(pool,OWNER,ALGORITHM,slot,"completed").await;
            let study=study(pool,OWNER,ALGORITHM,"completed",Some(generation),Some(slot)).await;
            sqlx::query("update continuous_generations set accepted=true,study_id=$2,result=$3 where id=$1").bind(generation).bind(study)
                .bind(json!({"meanExpectancy":0.2,"selectedCell":{"tpPct":14,"slPct":4},"best":{"params":SpikeParams::default(),"analytics":{"clusterEnabled":true,"clusterGapMinutes":480}}})).execute(pool).await.unwrap();
            sqlx::query("insert into adaptive_profile_handoffs(generation_id,owner_id,slot_id) values($1,'admin',$2)").bind(generation).bind(slot).execute(pool).await.unwrap();generation
        }
        let old=handoff(&pool,active.slot_id).await;let latest=handoff(&pool,active.slot_id).await;
        assert_eq!(profiles::accept_handoffs(&pool,OWNER).await.unwrap(),2);
        assert_eq!(profiles::detail(&pool,OWNER,active.slot_id).await.unwrap().unwrap()["acceptedGenerationId"],latest);
        let negative=generation(&pool,OWNER,ALGORITHM,active.slot_id,"completed").await;
        sqlx::query("update continuous_generations set accepted=false where id=$1").bind(negative).execute(&pool).await.unwrap();
        assert_eq!(profiles::accept_handoffs(&pool,OWNER).await.unwrap(),0);
        assert_eq!(db::live::pending_adaptive_slots(&pool,OWNER).await.unwrap()[0].profile.generation_id,latest,"newer nonpass must not fence accepted pending");
        sqlx::query("update adaptive_profile_handoffs set status='pending' where generation_id=$1").bind(old).execute(&pool).await.unwrap();
        profiles::accept_handoffs(&pool,OWNER).await.unwrap();
        assert_eq!(profiles::detail(&pool,OWNER,active.slot_id).await.unwrap().unwrap()["acceptedGenerationId"],latest);
        let stop=ProfileCommand{expected_revision:1,expected_generation_id:latest,request_id:"n8-sticky-stop-01".into()};
        assert!(matches!(profiles::command(&pool,OWNER,active.slot_id,false,&stop).await.unwrap(),CommandOutcome::Applied(_)));
        let replacement=handoff(&pool,active.slot_id).await;profiles::accept_handoffs(&pool,OWNER).await.unwrap();
        let stopped=profiles::detail(&pool,OWNER,active.slot_id).await.unwrap().unwrap();assert_eq!(stopped["desiredRunning"],false);assert_eq!(stopped["acceptedGenerationId"],replacement);
        assert!(db::live::pending_adaptive_slots(&pool,OWNER).await.unwrap().is_empty());
        profiles::acknowledge_stops(&pool,OWNER).await.unwrap();
        // Resume first to fill the active admission, then accept a second pairing.
        let play=ProfileCommand{expected_revision:2,expected_generation_id:replacement,request_id:"n8-cap-play-00001".into()};
        assert!(matches!(profiles::command(&pool,OWNER,active.slot_id,true,&play).await.unwrap(),CommandOutcome::Applied(_)));
        let mut slots=Vec::new();
        for frequency in [8,12] {
            let id:i64=sqlx::query_scalar("insert into continuous_slots(owner_id,algorithm_id,min_setups_per_day) values('admin',$1,$2::text::numeric) returning id").bind(ALGORITHM).bind(frequency.to_string()).fetch_one(&pool).await.unwrap();
            handoff(&pool,id).await;slots.push(id);
        }
        profiles::accept_handoffs(&pool,OWNER).await.unwrap();
        let listed=profiles::list(&pool,OWNER,None,0,50).await.unwrap();assert_eq!(listed["total"],2);
        assert_eq!(listed["profiles"][1]["error"]["code"],"profile_running_limit");
        assert_eq!(listed["blockedHandoffs"][0]["error"]["code"],"profile_total_limit");
        let second=profiles::detail(&pool,OWNER,slots[0]).await.unwrap().unwrap();
        let play=ProfileCommand{expected_revision:1,expected_generation_id:second["acceptedGenerationId"].as_i64().unwrap(),request_id:"n8-cap-play-00002".into()};
        assert!(matches!(profiles::command(&pool,OWNER,slots[0],true,&play).await.unwrap(),CommandOutcome::Limited{limit:1,..}));
        assert_eq!(profiles::detail(&pool,OWNER,active.slot_id).await.unwrap().unwrap()["desiredRunning"],true,"cap cannot evict incumbent");
    }).await;
}

#[tokio::test]
#[ignore = "requires isolated TEST_DATABASE_URL ending in _test"]
async fn continuous_handoff_lifecycle_records_follow_commit_and_hide_payloads() {
    const CHILD: &str = "SETUPS_HANDOFF_LOG_TEST_CHILD";
    if std::env::var_os(CHILD).is_none() {
        // Capture the real shared stderr sink without adding a process-global
        // collector/test hook to production logging. The child owns its test DB.
        let output = tokio::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "continuous_handoff_lifecycle_records_follow_commit_and_hide_payloads",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env("RUST_LOG", "info")
            .output()
            .await
            .unwrap();
        assert!(
            output.status.success(),
            "log fixture failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(
            !stderr.contains("private-"),
            "private fixture snapshots, idempotency keys and raw DB errors must not be logged"
        );
        let records: Vec<Value> = stderr
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect();
        for record in &records {
            assert!(record.to_string().len() + 1 <= setups_db::diagnostics::MAX_RECORD_BYTES);
        }
        let outcomes = |event: &str| {
            records
                .iter()
                .filter(|r| r["event"] == event)
                .map(|r| r["outcome"].as_str().unwrap())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            outcomes("continuous_finalization_finished"),
            [
                "error",
                "finalized",
                "already_completed",
                "finalized",
                "finalized"
            ]
        );
        let nonpasses: Vec<_> = records
            .iter()
            .filter(|r| r["event"] == "continuous_finalization_finished" && r["accepted"] == false)
            .collect();
        assert_eq!(
            nonpasses
                .iter()
                .map(|r| r["reason"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["no_winner", "quality_gate_nonpass"]
        );
        assert!(
            nonpasses
                .iter()
                .all(|r| r["handoff_state"] == "not_created")
        );
        assert_eq!(
            outcomes("continuous_profile_handoff_finished"),
            ["error", "accepted"],
            "idle polls must not add INFO records"
        );
        let failed_handoff = records
            .iter()
            .find(|r| {
                r["event"] == "continuous_profile_handoff_finished" && r["outcome"] == "error"
            })
            .unwrap();
        assert_eq!(failed_handoff["admission_state"], "not_confirmed");
        for field in ["reason", "desired_running"] {
            assert!(
                failed_handoff.get(field).is_none(),
                "unconfirmed COMMIT must not retain tentative {field}"
            );
        }
        assert!(
            !failed_handoff.to_string().contains("rolled_back"),
            "an unconfirmed acknowledgement does not prove rollback"
        );
        assert_eq!(
            outcomes("live_initialization_claimed"),
            ["claimed", "claimed", "claimed"]
        );
        assert!(
            outcomes("live_initialization_claim_finished").is_empty(),
            "ordinary claim misses are DEBUG only"
        );
        assert_eq!(
            outcomes("live_readmission_finished"),
            [
                "error",
                "granted",
                "already_granted",
                "conflict",
                "not_found"
            ]
        );
        for record in records.iter().filter(|r| {
            matches!(
                r["event"].as_str(),
                Some(
                    "continuous_finalization_finished"
                        | "continuous_profile_handoff_finished"
                        | "live_initialization_claimed"
                )
            )
        }) {
            assert!(record["study_id"].is_i64());
            assert!(record["generation_id"].is_i64());
            assert!(record["profile_id"].is_i64());
            assert!(record["duration_ms"].is_u64());
        }
        for record in records.iter().filter(|r| {
            r["event"] == "live_readmission_finished"
                && matches!(r["outcome"].as_str(), Some("granted" | "already_granted"))
        }) {
            assert!(record["study_id"].is_i64());
            assert_eq!(record["attempt"], 3);
            assert_eq!(record["attempt_limit"], 6);
        }
        return;
    }
    isolated(|pool|async move {
        setups_db::migrate::ensure_migrations(&pool).await.unwrap();
        configured(&pool,OWNER,ALGORITHM).await;
        setups_db::settings_set(&pool,"live",&json!({"enabled":true})).await.unwrap();
        let lease=scheduler::claim_owner(&pool,"lifecycle-fixture").await.unwrap().unwrap();
        let job=scheduler::prepare_next(&pool,&lease).await.unwrap().unwrap();
        let result=json!({"best":{"params":SpikeParams::default(),"analytics":{"clusterEnabled":false}},
            "analytics":{"payload":"private-candle-array-secret"},"recommendation":{"snapshot":"private-parameters-secret"},
            "meanExpectancy":0.21,"selectedCell":{"tpPct":14,"slPct":4}});
        let replay=batch(&pool,ALGORITHM,Some(job.generation_id)).await;
        run(&pool,replay,ALGORITHM,"done",Some(job.generation_id)).await;
        sqlx::query("update optimization_studies set status='completed',result=$2 where id=$1")
            .bind(job.study_id).bind(&result).execute(&pool).await.unwrap();
        sqlx::query("update continuous_generations set replay_batch_id=$2,replay_manifest='{}' where id=$1")
            .bind(job.generation_id).bind(replay).execute(&pool).await.unwrap();
        sqlx::raw_sql("create function reject_lifecycle_commit() returns trigger language plpgsql as $$begin raise exception 'private-db-error-sentinel';end$$;
            create constraint trigger reject_lifecycle_commit after insert on adaptive_profile_handoffs deferrable initially deferred for each row execute function reject_lifecycle_commit()")
            .execute(&pool).await.unwrap();
        assert!(scheduler::finalize(&pool,&job,&result,true,Some(0.21),Some(&result["selectedCell"])).await.is_err());
        assert_eq!(sqlx::query_scalar::<_,String>("select status from continuous_generations where id=$1").bind(job.generation_id).fetch_one(&pool).await.unwrap(),"running");
        sqlx::query("drop trigger reject_lifecycle_commit on adaptive_profile_handoffs").execute(&pool).await.unwrap();
        assert!(scheduler::finalize(&pool,&job,&result,true,Some(0.21),Some(&result["selectedCell"])).await.unwrap());
        assert!(scheduler::finalize(&pool,&job,&result,true,Some(0.21),Some(&result["selectedCell"])).await.unwrap());

        sqlx::raw_sql("create constraint trigger reject_lifecycle_commit after insert on adaptive_profiles deferrable initially deferred for each row execute function reject_lifecycle_commit()")
            .execute(&pool).await.unwrap();
        assert!(db::profiles::accept_handoffs(&pool,OWNER).await.is_err());
        assert_eq!(sqlx::query_scalar::<_,String>("select status from adaptive_profile_handoffs where generation_id=$1").bind(job.generation_id).fetch_one(&pool).await.unwrap(),"pending");
        assert_eq!(sqlx::query_scalar::<_,i64>("select count(*) from adaptive_profiles").fetch_one(&pool).await.unwrap(),0);
        sqlx::query("drop trigger reject_lifecycle_commit on adaptive_profiles").execute(&pool).await.unwrap();
        assert_eq!(db::profiles::accept_handoffs(&pool,OWNER).await.unwrap(),1);
        for _ in 0..3 {assert_eq!(db::profiles::accept_handoffs(&pool,OWNER).await.unwrap(),0);}
        let pending=db::live::pending_adaptive_slots(&pool,OWNER).await.unwrap().remove(0);
        for attempt in 1..=3 {
            sqlx::query("update continuous_generations set live_init_next_attempt_at=null where id=$1").bind(job.generation_id).execute(&pool).await.unwrap();
            let fence=db::live::claim_pending_warmup_at(&pool,&pending,"lifecycle-fixture",CUTOFF).await.unwrap().unwrap();
            assert_eq!(fence.attempt,attempt);
            for _ in 0..3 {assert!(db::live::claim_pending_warmup_at(&pool,&pending,"contender",CUTOFF).await.unwrap().is_none());}
            assert!(db::live::report_warmup_failure(&pool,&fence,&json!({"code":"test_failure","message":"private-candle-array-secret"}),true).await.unwrap());
        }
        let request=live_readmission_request(job.generation_id,"private-idempotency-key");
        sqlx::raw_sql("create constraint trigger reject_lifecycle_commit after insert on continuous_live_readmissions deferrable initially deferred for each row execute function reject_lifecycle_commit()")
            .execute(&pool).await.unwrap();
        assert!(db::readmission::grant(&pool,OWNER,ALGORITHM,job.slot_id,&request).await.is_err());
        assert_eq!(sqlx::query_scalar::<_,i32>("select live_init_attempt_limit from continuous_generations where id=$1").bind(job.generation_id).fetch_one(&pool).await.unwrap(),3);
        sqlx::query("drop trigger reject_lifecycle_commit on continuous_live_readmissions").execute(&pool).await.unwrap();
        assert!(!granted_live(&pool,job.slot_id,&request).await.duplicate);
        assert!(granted_live(&pool,job.slot_id,&request).await.duplicate);
        let stale=db::readmission::LiveReadmissionRequest{expected_config_revision:99,..request.clone()};
        assert!(matches!(db::readmission::grant(&pool,OWNER,ALGORITHM,job.slot_id,&stale).await.unwrap(),db::readmission::LiveReadmissionOutcome::Conflict(_)));
        assert!(matches!(db::readmission::grant(&pool,"missing-owner",ALGORITHM,job.slot_id,&request).await.unwrap(),db::readmission::LiveReadmissionOutcome::NotFound));
        assert_eq!(sqlx::query_scalar::<_,Value>("select result from continuous_generations where id=$1").bind(job.generation_id).fetch_one(&pool).await.unwrap(),result);
        for result in [json!({"best":null,"reason":"no_winner","private":"private-payload"}),json!({"best":{},"analytics":{"private":"private-payload"}})] {
            sqlx::query("update continuous_processes set next_attempt_at=now() where owner_id=$1 and algorithm_id=$2").bind(OWNER).bind(ALGORITHM).execute(&pool).await.unwrap();
            let next=scheduler::prepare_next(&pool,&lease).await.unwrap().unwrap();
            scheduler::finish_search(&pool,&next,&json!({"progress":{}}),true).await.unwrap();
            assert!(scheduler::finalize(&pool,&next,&result,false,None,None).await.unwrap());
        }
        assert_eq!(sqlx::query_scalar::<_,i64>("select count(*) from adaptive_profile_handoffs").fetch_one(&pool).await.unwrap(),1,"nonpass results create no handoff");
        scheduler::release(&pool,&lease).await.unwrap();
    }).await;
}

#[tokio::test]
#[ignore = "requires isolated TEST_DATABASE_URL ending in _test"]
async fn telegram_explicit_legacy_v1_keeps_original_utf8_body_and_32k_reservation() {
    isolated(|pool|async move {
        use setups_db::telegram as tg;
        setups_db::migrate::ensure_migrations(&pool).await.unwrap();
        tg::set_config(&pool,0,true,"https://relay.example").await.unwrap();
        let now=chrono::Utc::now().timestamp();
        let setup=fixed_setup(&pool,now-60).await;
        tg::configure_limits(&pool,1,65536).await.unwrap();
        // Omitted version deliberately exercises the legacy default, rather
        // than upgrading new v2 work or bypassing its immutable-version guard.
        sqlx::query("insert into telegram_delivery_outbox(setup_id,delivery_id,endpoint_url,settings_revision,owner_id,algorithm_id,fixed_config,raw_close_unix,expires_at,payload) values($1,'legacy:unicode','https://relay.example/',1,'admin',$2,'{}',$3,to_timestamp($4),'{}')")
            .bind(setup).bind(ALGORITHM).bind(now).bind((now+3600)as f64).execute(&pool).await.unwrap();
        let job=tg::claim(&pool,"legacy",1).await.unwrap().remove(0);
        assert_eq!(job.wire_version,1);
        assert_eq!(sqlx::query_scalar::<_,i64>("select reserved_bytes-octet_length(payload::text) from telegram_delivery_outbox").fetch_one(&pool).await.unwrap(),32768);
        let body=format!(" {} \n",json!({"version":1,"id":job.delivery_id,"text":"\u{1d11e}".repeat(4096),"expiresAt":job.expires_at_unix,"attachment":null}));
        assert!(body.len()>16384&&body.len()<32768,"v1 retains scalar-character semantics, not the v2 UTF-16 limit");
        assert_eq!(tg::freeze_message(&pool,"legacy",&job,body.as_bytes()).await.unwrap().unwrap(),body.as_bytes());
        assert_eq!(tg::freeze_message(&pool,"legacy",&job,b"different renderer").await.unwrap().unwrap(),body.as_bytes());
        assert!(tg::finish(&pool,"legacy",&job,tg::TelegramOutcome::Sent).await.unwrap());
        assert_eq!(sqlx::query_scalar::<_,i64>("select pending_jobs+pending_bytes from telegram_delivery_budget").fetch_one(&pool).await.unwrap(),0);
    }).await;
}

#[tokio::test]
#[ignore = "requires isolated TEST_DATABASE_URL ending in _test"]
async fn telegram_fixed_independent_enqueue_raw_bytes_fences_capacity_expiry_and_rollback() {
    isolated(|pool|async move {
        use setups_db::telegram as tg;
        let legacy=fixed_setup(&pool,CUTOFF-60).await;
        setups_db::migrate::ensure_migrations(&pool).await.unwrap();
        assert_eq!(sqlx::query_scalar::<_,String>("select telegram_delivery_status from live_setups where id=$1").bind(legacy).fetch_one(&pool).await.unwrap(),"not_requested");
        assert!(tg::claim(&pool,"tg",1).await.unwrap().is_empty());
        setups_db::settings_set(&pool,"live",&json!({"enabled":true})).await.unwrap();
        let now=chrono::Utc::now().timestamp().div_euclid(60)*60;
        let raw=adaptive_write(now-60).setup;
        let disabled=db::outbox::commit_fixed_setup(&pool,"bybit","linear","DISABLED",&raw).await.unwrap().unwrap();
        assert_eq!(sqlx::query_scalar::<_,String>("select telegram_delivery_status from live_setups where id::bigint=$1").bind(disabled).fetch_one(&pool).await.unwrap(),"disabled");
        assert!(matches!(tg::set_config(&pool,0,true,"https://relay.example").await.unwrap(),tg::ConfigUpdate::Updated(_)));
        let reference=fixed_setup(&pool,now+60).await;
        assert!(tg::claim(&pool,"tg",1).await.unwrap().is_empty(),"enable never backfills old or reference inserts");
        assert_eq!(sqlx::query_scalar::<_,String>("select telegram_delivery_status from live_setups where id=$1").bind(reference).fetch_one(&pool).await.unwrap(),"not_requested");
        db::outbox::configure_limits(&pool,1,65536).await.unwrap();
        let id=db::outbox::commit_fixed_setup(&pool,"bybit","linear","FRESH",&raw).await.unwrap().unwrap();
        assert_eq!(sqlx::query_scalar::<_,String>("select notification_delivery_status from live_setups where id::bigint=$1").bind(id).fetch_one(&pool).await.unwrap(),"skipped_capacity","WebPush capacity must not suppress Telegram");
        let before:Value=sqlx::query_scalar("select to_jsonb(o)-'endpoint_url'-'settings_revision' from telegram_delivery_outbox o").fetch_one(&pool).await.unwrap();
        assert!(matches!(tg::set_config(&pool,1,true,"https://other.example").await.unwrap(),tg::ConfigUpdate::Updated(_)));
        assert_eq!(sqlx::query_scalar::<_,Value>("select to_jsonb(o)-'endpoint_url'-'settings_revision' from telegram_delivery_outbox o").fetch_one(&pool).await.unwrap(),before);
        let job=tg::claim(&pool,"tg",1).await.unwrap().remove(0);assert_eq!(job.setup_id,id);assert_eq!(job.endpoint_url,"https://other.example/");assert_eq!(job.wire_version,2);
        assert_eq!(sqlx::query_scalar::<_,i64>("select reserved_bytes-octet_length(payload::text) from telegram_delivery_outbox where id=$1").bind(job.id).fetch_one(&pool).await.unwrap(),tg::MAX_V2_BODY_BYTES as i64);
        assert!(tg::permitted(&pool,"tg",&job).await.unwrap());assert!(!tg::permitted(&pool,"wrong",&job).await.unwrap());
        assert!(matches!(tg::set_config(&pool,2,true,"https://third.example").await.unwrap(),tg::ConfigUpdate::EndpointBusy));
        let body=format!(" {} {}\n",json!({"version":2,"id":job.delivery_id,"text":"\u{1d11e}".repeat(2048),"entities":[],"expiresAt":job.expires_at_unix,"attachment":null})," ".repeat(32768));
        assert!(body.len()>32768&&body.len()<=tg::MAX_V2_BODY_BYTES);
        let quota:i64=sqlx::query_scalar("select pending_bytes from telegram_delivery_budget").fetch_one(&pool).await.unwrap();
        assert_eq!(tg::freeze_message(&pool,"tg",&job,body.as_bytes()).await.unwrap().unwrap(),body.as_bytes());
        assert_eq!(tg::freeze_message(&pool,"tg",&job,b"changed renderer bytes").await.unwrap().unwrap(),body.as_bytes());
        assert_eq!(sqlx::query_scalar::<_,i64>("select pending_bytes from telegram_delivery_budget").fetch_one(&pool).await.unwrap(),quota);
        assert_eq!(sqlx::query_scalar::<_,String>("select request_body_hash from telegram_delivery_outbox").fetch_one(&pool).await.unwrap().len(),64);
        assert!(!tg::finish(&pool,"wrong",&job,tg::TelegramOutcome::Sent).await.unwrap());
        assert!(tg::finish(&pool,"tg",&job,tg::TelegramOutcome::Retry {delay_seconds:1,reason:tg::TelegramReason::new("transient","Retry")}).await.unwrap());
        assert!(matches!(tg::set_config(&pool,2,true,"https://third.example").await.unwrap(),tg::ConfigUpdate::EndpointBusy),"attempted but unleased work blocks changing origin");
        for attempt in 2..=5 {
            sqlx::query("update telegram_delivery_outbox set not_before=clock_timestamp()-interval '1 second'").execute(&pool).await.unwrap();
            let next=tg::claim(&pool,"tg",1).await.unwrap().remove(0);assert_eq!(next.attempts,attempt);assert_eq!(next.request_body.as_deref(),Some(body.as_bytes()));
            assert!(tg::finish(&pool,"tg",&next,tg::TelegramOutcome::Retry {delay_seconds:1,reason:tg::TelegramReason::new("unconfirmed","Delivery unconfirmed")}).await.unwrap());
        }
        assert!(tg::claim(&pool,"tg",1).await.unwrap().is_empty());
        assert_eq!(sqlx::query_scalar::<_,String>("select telegram_delivery_status from live_setups where id::bigint=$1").bind(id).fetch_one(&pool).await.unwrap(),"failed");
        tg::configure_limits(&pool,1,65536).await.unwrap();
        let byte_full=db::outbox::commit_fixed_setup(&pool,"bybit","linear","BYTE_FULL",&raw).await.unwrap().unwrap();
        assert_eq!(sqlx::query_scalar::<_,String>("select telegram_delivery_status from live_setups where id::bigint=$1").bind(byte_full).fetch_one(&pool).await.unwrap(),"skipped_capacity","a free job slot cannot bypass the full v2 reservation");
        assert_eq!(sqlx::query_scalar::<_,i64>("select pending_jobs from telegram_delivery_budget").fetch_one(&pool).await.unwrap(),0);
        tg::configure_limits(&pool,1,524288).await.unwrap();
        let one=db::outbox::commit_fixed_setup(&pool,"bybit","linear","ONE",&raw).await.unwrap().unwrap();
        assert_eq!(sqlx::query_scalar::<_,String>("select telegram_delivery_status from live_setups where id::bigint=$1").bind(one).fetch_one(&pool).await.unwrap(),"pending");
        let full=db::outbox::commit_fixed_setup(&pool,"bybit","linear","FULL",&raw).await.unwrap().unwrap();
        assert_eq!(sqlx::query_scalar::<_,String>("select telegram_delivery_status from live_setups where id::bigint=$1").bind(full).fetch_one(&pool).await.unwrap(),"skipped_capacity");
        sqlx::query("update telegram_delivery_outbox set expires_at=clock_timestamp()-interval '1 second'").execute(&pool).await.unwrap();assert!(tg::claim(&pool,"tg",1).await.unwrap().is_empty());
        assert_eq!(sqlx::query_scalar::<_,i64>("select pending_bytes+pending_jobs from telegram_delivery_budget").fetch_one(&pool).await.unwrap(),0);
        let expired=db::outbox::commit_fixed_setup(&pool,"bybit","linear","OLD",&adaptive_write(CUTOFF-60).setup).await.unwrap().unwrap();
        assert_eq!(sqlx::query_scalar::<_,String>("select telegram_delivery_status from live_setups where id::bigint=$1").bind(expired).fetch_one(&pool).await.unwrap(),"expired");
        let count:i64=sqlx::query_scalar("select count(*) from live_setups").fetch_one(&pool).await.unwrap();
        sqlx::raw_sql("create function telegram_fault() returns trigger language plpgsql as $$ begin raise exception 'injected telegram transaction failure'; end $$;create trigger zz_telegram_fault before insert on telegram_delivery_outbox for each row execute function telegram_fault()").execute(&pool).await.unwrap();
        assert!(db::outbox::commit_fixed_setup(&pool,"bybit","linear","ROLLBACK",&raw).await.is_err());
        assert_eq!(sqlx::query_scalar::<_,i64>("select count(*) from live_setups").fetch_one(&pool).await.unwrap(),count);
        assert_eq!(sqlx::query_scalar::<_,i64>("select pending_jobs from telegram_delivery_budget").fetch_one(&pool).await.unwrap(),0);
    }).await;
}

#[tokio::test]
#[ignore = "requires isolated TEST_DATABASE_URL ending in _test"]
async fn telegram_adaptive_atomic_checkpoint_promotion_stop_and_confirmed_cleanup_preserve_publication()
 {
    isolated(|pool|async move {
        use setups_db::telegram as tg;
        setups_db::migrate::ensure_migrations(&pool).await.unwrap();tg::set_config(&pool,0,true,"https://relay.example").await.unwrap();
        let fixture=purge_fixture(&pool).await;
        let rows=tg::claim(&pool,"tg",2).await.unwrap();assert_eq!(rows.len(),2,"both fixed and adaptive new writes registered independently");
        let adaptive=rows.iter().find(|job|job.slot_id.is_some()).unwrap();
        let output:Value=sqlx::query_scalar("select jsonb_build_object('id',id,'params',params,'details',details,'close_time',close_time,'external_name',external_name) from live_setups where id::bigint=$1").bind(fixture.signal).fetch_one(&pool).await.unwrap();
        assert!(!tg::permitted(&pool,"tg",adaptive).await.unwrap(),"Stop blocks delivery");
        let preview=db::cleanup::preview(&pool,OWNER).await.unwrap();assert!(!preview.eligible,"active Telegram lease must block research purge");
        assert!(tg::finish(&pool,"tg",adaptive,tg::TelegramOutcome::Retry {delay_seconds:1,reason:tg::TelegramReason::new("stopped","Stopped")}).await.unwrap());
        let preview=db::cleanup::preview(&pool,OWNER).await.unwrap();assert!(preview.eligible);
        assert!(matches!(db::cleanup::purge(&pool,OWNER,&preview.fingerprint).await.unwrap(),db::cleanup::Outcome::Deleted(_)));
        assert_eq!(sqlx::query_scalar::<_,Value>("select jsonb_build_object('id',id,'params',params,'details',details,'close_time',close_time,'external_name',external_name) from live_setups where id::bigint=$1").bind(fixture.signal).fetch_one(&pool).await.unwrap(),output);
        assert_eq!(sqlx::query_scalar::<_,String>("select telegram_delivery_status from live_setups where id::bigint=$1").bind(fixture.signal).fetch_one(&pool).await.unwrap(),"disabled");
        assert_eq!(sqlx::query_scalar::<_,i64>("select pending_jobs from telegram_delivery_budget").fetch_one(&pool).await.unwrap(),1,"fixed Telegram job survives adaptive cleanup");
    }).await;
}

#[tokio::test]
#[ignore = "requires isolated TEST_DATABASE_URL ending in _test"]
async fn telegram_pre_promotion_source_remains_deliverable_but_nonpass_and_stop_block() {
    isolated(|pool|async move {
        use setups_db::telegram as tg;
        setups_db::migrate::ensure_migrations(&pool).await.unwrap();tg::set_config(&pool,0,true,"https://relay.example").await.unwrap();
        let active=accepted(&pool).await;let now=chrono::Utc::now().timestamp().div_euclid(60)*60;
        let saved=db::commit_adaptive_scan_cas(&pool,&active,INSTRUMENT,None,&promotion_state(active.generation_id,now),&[adaptive_write(now-60)]).await.unwrap();assert_eq!(saved.inserted.len(),1);
        let new=tg::claim(&pool,"new",1).await.unwrap().remove(0);assert!(tg::permitted(&pool,"new",&new).await.unwrap());
        let counts="select jsonb_build_array((select count(*) from live_setups),(select count(*) from adaptive_live_checkpoints),(select count(*) from adaptive_notification_outbox),(select pending_jobs from telegram_delivery_budget),(select pending_bytes from telegram_delivery_budget))";
        let before:Value=sqlx::query_scalar(counts).fetch_one(&pool).await.unwrap();
        sqlx::raw_sql("create function telegram_fault() returns trigger language plpgsql as $$ begin raise exception 'injected Telegram failure after checkpoint/setup'; end $$;create trigger zz_telegram_fault before insert on telegram_delivery_outbox for each row execute function telegram_fault()").execute(&pool).await.unwrap();
        let mut second=adaptive_write(now-60);second.symbol="ETHUSDT".into();
        assert!(db::commit_adaptive_scan_cas(&pool,&active,"bybit/linear/ETHUSDT",None,&promotion_state(active.generation_id,now),&[second]).await.is_err());
        assert_eq!(sqlx::query_scalar::<_,Value>(counts).fetch_one(&pool).await.unwrap(),before,"checkpoint/setup and both queues roll back atomically");
        sqlx::raw_sql("drop trigger zz_telegram_fault on telegram_delivery_outbox;drop function telegram_fault()").execute(&pool).await.unwrap();
        let replacement=generation(&pool,OWNER,ALGORITHM,active.slot_id,"completed").await;
        sqlx::query("update continuous_slots set active_generation_id=$2 where id=$1").bind(active.slot_id).bind(replacement).execute(&pool).await.unwrap();
        assert!(tg::permitted(&pool,"new",&new).await.unwrap(),"pre-normal-promotion committed work remains deliverable");
        sqlx::query("update continuous_slots set active_generation_id=null where id=$1").bind(active.slot_id).execute(&pool).await.unwrap();assert!(!tg::permitted(&pool,"new",&new).await.unwrap(),"nonpass disables delivery");
    }).await;
}

#[tokio::test]
#[ignore = "requires isolated TEST_DATABASE_URL ending in _test"]
async fn telegram_explicit_test_markers_use_original_expiry_clear_all_results_and_expire_without_rearming()
 {
    isolated(|pool|async move {
        use setups_db::telegram as tg;
        setups_db::migrate::ensure_migrations(&pool).await.unwrap();tg::set_config(&pool,0,true,"https://a.example/").await.unwrap();
        let boundary:(bool,bool,bool)=sqlx::query_as("select to_timestamp($1)>=date_trunc('second',to_timestamp($2)),to_timestamp($1)<date_trunc('second',to_timestamp($3)),to_timestamp($1)>to_timestamp($2)")
            .bind(CUTOFF as f64).bind(CUTOFF as f64+0.875).bind(CUTOFF as f64+1.0).fetch_one(&pool).await.unwrap();
        assert_eq!(boundary,(true,true,false),"fixed PG timestamps prove inclusive last-second expiry and next-second rejection without relaxing precise leases");
        let trigger:String=sqlx::query_scalar("select pg_get_functiondef('telegram_delivery_account()'::regprocedure)").fetch_one(&pool).await.unwrap();
        assert!(trigger.contains("NEW.expires_at<date_trunc('second',clock_timestamp())"));assert!(trigger.contains("telegram_delivery_updated_at=clock_timestamp()"));
        for (state,key) in [("sent","aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"),("unknown","bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb"),("error","cccccccc-cccc-4ccc-8ccc-cccccccccccc")] {
            sqlx::query("update telegram_relay_config set test_started_at=null").execute(&pool).await.unwrap();
            let config=tg::get_config(&pool).await.unwrap();
            let tg::TestAdmission::Admitted(attempt)=tg::begin_test(&pool,config.revision,key).await.unwrap() else {panic!("test admission failed")};
            let deadline:i64=sqlx::query_scalar("select extract(epoch from in_flight_until)::bigint from telegram_relay_tests where request_id=$1").bind(key).fetch_one(&pool).await.unwrap();
            assert_eq!(deadline,attempt.expires_at_unix);assert_eq!(serde_json::from_slice::<Value>(&attempt.request_body).unwrap()["expiresAt"],deadline);
            assert!(matches!(tg::begin_test(&pool,config.revision,key).await.unwrap(),tg::TestAdmission::Existing(_)));
            assert_eq!(sqlx::query_scalar::<_,i64>("select extract(epoch from in_flight_until)::bigint from telegram_relay_tests where request_id=$1").bind(key).fetch_one(&pool).await.unwrap(),deadline,"replay never extends the original deadline");
            assert!(matches!(tg::set_config(&pool,config.revision,true,"https://b.example/").await.unwrap(),tg::ConfigUpdate::EndpointBusy));
            assert!(matches!(tg::set_config(&pool,config.revision,!config.enabled,&config.endpoint_url).await.unwrap(),tg::ConfigUpdate::Updated(_)),"enable/disable remains available while a diagnostic runs");
            let result=tg::TelegramTestResult {request_id:key.into(),state:state.into(),reason:None};
            assert!(!tg::record_test(&pool,&attempt,&result).await.unwrap(),"the completion is stale after a settings revision change");
            assert!(sqlx::query_scalar::<_,bool>("select in_flight_until is null and state=$2 from telegram_relay_tests where request_id=$1").bind(key).bind(state).fetch_one(&pool).await.unwrap(),"all finished outcomes release the guard even when revision CAS is stale");
        }
        let config=tg::get_config(&pool).await.unwrap();let key="dddddddd-dddd-4ddd-8ddd-dddddddddddd";
        let expired=chrono::Utc::now().timestamp()-1;
        let body=serde_json::to_vec(&json!({"version":1,"id":"test:abandoned","text":"Telegram relay delivery test.","expiresAt":expired,"attachment":null})).unwrap();
        sqlx::query("insert into telegram_relay_tests(request_id,settings_revision,endpoint_url,delivery_id,request_body,in_flight_until,created_at) values($1,$2,$3,'test:abandoned',$4,to_timestamp($5),to_timestamp($5)-interval '60 seconds')")
            .bind(key).bind(config.revision).bind(&config.endpoint_url).bind(&body).bind(expired as f64).execute(&pool).await.unwrap();
        assert!(tg::claim(&pool,"idle",1).await.unwrap().is_empty(),"idle cleanup also expires abandoned diagnostics");
        assert!(matches!(tg::begin_test(&pool,config.revision,key).await.unwrap(),tg::TestAdmission::Existing(_)),"an abandoned unknown receipt is never automatically retried");
        assert!(sqlx::query_scalar::<_,bool>("select in_flight_until is null and request_body=$2 from telegram_relay_tests where request_id=$1").bind(key).bind(&body).fetch_one(&pool).await.unwrap(),"expiry clears only the marker, not the frozen original body/expiry");
        assert!(matches!(tg::set_config(&pool,config.revision,true,"https://b.example/").await.unwrap(),tg::ConfigUpdate::Updated(_)));
        assert_eq!(sqlx::query_scalar::<_,i64>("select count(*) from telegram_relay_tests").fetch_one(&pool).await.unwrap(),4,"24-hour receipts do not hold the origin for 24 hours");
    }).await;
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

async fn purge_fixture(pool: &PgPool) -> PurgeFixture {
    setups_db::migrate::ensure_migrations(pool).await.unwrap();
    let active = accepted(pool).await;
    let b = batch(pool, ALGORITHM, Some(active.generation_id)).await;
    let r = run(pool, b, ALGORITHM, "done", Some(active.generation_id)).await;
    let standalone = run(pool, b, ALGORITHM, "done", Some(active.generation_id)).await;
    sqlx::query("update replay_runs set batch_id=null where id=$1")
        .bind(standalone)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("update continuous_generations set replay_batch_id=$2,replay_manifest=$3,result=result||$4 where id=$1")
        .bind(active.generation_id).bind(b).bind(json!({"provenance":"frozen sentinel","instruments":[{"runId":r}]}))
        .bind(json!({"analytics":{"evaluation":{"matrix":[{"riskExpectancy":0.32}],"summary":{"score":0.32}},"sentinel":"independent post-chain"}})).execute(pool).await.unwrap();
    let second_slot = configured(pool, OWNER, OTHER_ALGORITHM).await;
    let second_generation = generation(pool, OWNER, OTHER_ALGORITHM, second_slot, "failed").await;
    let second_study = study(
        pool,
        OWNER,
        OTHER_ALGORITHM,
        "failed",
        Some(second_generation),
        Some(second_slot),
    )
    .await;
    sqlx::query("update continuous_generations set study_id=$2 where id=$1")
        .bind(second_generation)
        .bind(second_study)
        .execute(pool)
        .await
        .unwrap();
    study(pool, OWNER, ALGORITHM, "completed", None, None).await;
    let manual_batch = batch(pool, ALGORITHM, None).await;
    run(pool, manual_batch, ALGORITHM, "done", None).await;
    let other_slot = configured(pool, "tenant-two", ALGORITHM).await;
    let other_generation = generation(pool, "tenant-two", ALGORITHM, other_slot, "completed").await;
    study(
        pool,
        "tenant-two",
        ALGORITHM,
        "completed",
        Some(other_generation),
        Some(other_slot),
    )
    .await;
    sqlx::query("insert into adaptive_live_checkpoints(slot_id,instrument_key,generation_id,state) values($1,'other/sentinel',$2,'{}')").bind(other_slot).bind(other_generation).execute(pool).await.unwrap();
    sqlx::query("update adaptive_live_warmup_lane set slot_id=$1,generation_id=$2,lease_token=7,lease_owner='other-worker',lease_until=now()+interval '1 hour'").bind(other_slot).bind(other_generation).execute(pool).await.unwrap();
    setups_db::settings_set(pool, "notifications", &json!({"maxAgeSeconds":3600}))
        .await
        .unwrap();
    setups_db::settings_set(
        pool,
        "optimization-draft:price_volume_spike",
        &json!({"keep":"shared draft"}),
    )
    .await
    .unwrap();
    let now = chrono::Utc::now().timestamp().div_euclid(60) * 60;
    let signal = db::commit_adaptive_scan_cas(
        pool,
        &active,
        INSTRUMENT,
        None,
        &promotion_state(active.generation_id, now),
        &[adaptive_write(now - 60)],
    )
    .await
    .unwrap()
    .inserted[0]
        .id;
    db::outbox::commit_fixed_setup(
        pool,
        "bybit",
        "linear",
        "BTCUSDT",
        &adaptive_write(now - 60).setup,
    )
    .await
    .unwrap()
    .unwrap();
    sqlx::query("insert into replay_quality_intervals(run_id,from_unix,to_unix,reason) values($1,0,60,'sentinel'),($2,0,60,'sentinel')").bind(r).bind(standalone).execute(pool).await.unwrap();
    sqlx::query("insert into replay_cache(cache_key,exchange_code,category,symbol,algorithm_id,algorithm_version,params,from_ts,to_ts,setups,warmup_minutes,processed_minutes,quality_source_run_id) values('shared','bybit','linear','BTCUSDT',$1,'1.1.1','{}',to_timestamp(0),to_timestamp(3600),'[]',1,1,$2)").bind(ALGORITHM).bind(r).execute(pool).await.unwrap();
    sqlx::raw_sql("insert into analytics_selection_cache(scope_key,maturity_tenths,month_key,algorithm_version,result_json) values('batch:sentinel',0,'all',1,'{\"keep\":true}');insert into analytics_matrix_cache(scope_key,direction,maturity_tenths,month_key,tp_pct,sl_pct,horizon_minutes,time_shift_minutes,price_change_ppm,algorithm_version,result_json) values('batch:sentinel','short',0,'all',1,1,1,0,-1,1,'{\"keep\":true}');insert into analytics_trades_cache(scope_key,direction,maturity_tenths,exchange_code,category,symbol,close_time_unix,tp_pct,sl_pct,horizon_minutes,time_shift_minutes,price_change_ppm,algorithm_version,result_json) values('batch:sentinel','short',0,'bybit','linear','BTCUSDT',60,1,1,1,0,-1,1,'{\"keep\":true}')").execute(pool).await.unwrap();
    for algorithm in [ALGORITHM, OTHER_ALGORITHM] {
        db::stop(pool, OWNER, algorithm).await.unwrap();
    }
    let lease = scheduler::claim_owner(pool, "purge-fixture")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(lease.owner_id, OWNER);
    for algorithm in [ALGORITHM, OTHER_ALGORITHM] {
        cleanup_owned(pool, &lease, algorithm).await.unwrap();
    }
    PurgeFixture {
        active,
        batch: b,
        run: r,
        standalone,
        signal,
        lease,
    }
}

async fn purge_preserved(pool: &PgPool) -> Value {
    sqlx::query_scalar("select jsonb_build_object('studies',(select jsonb_agg(to_jsonb(s) order by id) from optimization_studies s where origin='manual' or owner_id<>'admin'),'settings',(select jsonb_agg(to_jsonb(s) order by key) from settings s),'cache',(select jsonb_agg(to_jsonb(c) order by cache_key) from replay_cache c),'selection',(select jsonb_agg(to_jsonb(c)) from analytics_selection_cache c),'matrix',(select jsonb_agg(to_jsonb(c)) from analytics_matrix_cache c),'trades',(select jsonb_agg(to_jsonb(c)) from analytics_trades_cache c),'quality',(select jsonb_agg(to_jsonb(q) order by run_id) from replay_quality_intervals q),'foreignSlots',(select jsonb_agg(to_jsonb(s) order by id) from continuous_slots s where owner_id<>'admin'),'foreignCheckpoints',(select jsonb_agg(to_jsonb(c) order by slot_id) from adaptive_live_checkpoints c join continuous_slots s on s.id=c.slot_id where s.owner_id<>'admin'),'warmup',(select to_jsonb(w) from adaptive_live_warmup_lane w),'fixedOutbox',(select jsonb_agg(to_jsonb(o) order by id) from adaptive_notification_outbox o where slot_id is null or owner_id<>'admin'))").fetch_one(pool).await.unwrap()
}

#[tokio::test]
#[ignore = "requires isolated TEST_DATABASE_URL ending in _test with CREATEDB"]
async fn history_purge_preserves_mixed_outputs_analytics_identity_and_supports_restart_and_row_delete()
 {
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
#[ignore = "requires isolated TEST_DATABASE_URL ending in _test with CREATEDB"]
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
#[ignore = "requires isolated TEST_DATABASE_URL ending in _test with CREATEDB"]
async fn history_purge_preserves_analytics_null_missing_and_rejects_cross_owner_or_collapsed_results()
 {
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
#[ignore = "requires isolated TEST_DATABASE_URL ending in _test with CREATEDB"]
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
#[ignore = "requires isolated TEST_DATABASE_URL ending in _test with CREATEDB"]
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
        assert_eq!(setups_db::migrate::ensure_migrations(&pool).await.unwrap(), "0 applied, 13 up to date");
        let fixed_after: Value = sqlx::query_scalar("select to_jsonb(s)-array['adaptive_slot_id','adaptive_generation_id','recommendation','external_name','notification_delivery_status','notification_delivery_error','notification_delivery_updated_at','telegram_delivery_status','telegram_delivery_error','telegram_delivery_updated_at','adaptive_generation_archive','expectancy_artifact_id'] from live_setups s where id=$1")
            .bind(fixed).fetch_one(&pool).await.unwrap();
        let manual_after: Value = sqlx::query_scalar("select to_jsonb(s)-array['origin','owner_id','continuous_generation_id','continuous_slot_id'] from optimization_studies s where id=$1")
            .bind(manual).fetch_one(&pool).await.unwrap();
        assert_eq!(fixed_after, fixed_before);
        assert_eq!(sqlx::query_scalar::<_,Option<i64>>("select expectancy_artifact_id from live_setups where id=$1")
            .bind(fixed).fetch_one(&pool).await.unwrap(),None,"new nullable provenance must not fabricate a legacy artifact");
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

async fn adaptive_batch_profile(pool: &PgPool, close: i64) -> db::ActiveAdaptiveSlot {
    setups_db::migrate::ensure_migrations(pool).await.unwrap();
    let profile = accepted(pool).await;
    let symbols = ["BTCUSDT", "ETHUSDT", "ZZZUSDT", "WAITUSDT", "UNSEENUSDT"];
    let instruments: Vec<_> = symbols
        .iter()
        .map(|symbol| json!({"exchange":"bybit","category":"linear","symbol":symbol}))
        .collect();
    let readiness:Vec<_>=symbols[..4].iter().map(|symbol|json!({"instrumentKey":format!("bybit/linear/{symbol}"),
        "status":if *symbol=="WAITUSDT" {"waiting"}else{"ready"},"reason":if *symbol=="WAITUSDT" {json!("waiting_tail")}else{Value::Null},
        "publicationFloorCloseUnix":close-180})).collect();
    sqlx::query("update continuous_generations set spec=jsonb_set(spec,'{instruments}',$2),live_readiness=$3 where id=$1")
        .bind(profile.generation_id).bind(json!(instruments)).bind(json!(readiness)).execute(pool).await.unwrap();
    setups_db::settings_set(pool, "notifications", &json!({"maxAgeSeconds":3600}))
        .await
        .unwrap();
    db::active_adaptive_slots(pool, OWNER)
        .await
        .unwrap()
        .remove(0)
}

fn adaptive_batch_scan(
    profile: &db::ActiveAdaptiveSlot,
    symbol: &str,
    close: i64,
) -> db::AdaptiveScanWrite {
    let mut row = adaptive_write(close - 60);
    row.symbol = symbol.into();
    db::AdaptiveScanWrite {
        instrument_key: format!("bybit/linear/{symbol}"),
        previous: None,
        checkpoint: promotion_state(profile.generation_id, close),
        setups: vec![row],
    }
}

async fn adaptive_batch_state(pool: &PgPool) -> Value {
    sqlx::query_scalar("select jsonb_build_object('setups',(select jsonb_agg(to_jsonb(s) order by id) from live_setups s),'checkpoints',(select jsonb_agg(to_jsonb(c) order by slot_id,instrument_key) from adaptive_live_checkpoints c),'outbox',(select jsonb_agg(to_jsonb(o) order by id) from adaptive_notification_outbox o),'budget',(select to_jsonb(b) from adaptive_notification_budget b))")
        .fetch_one(pool).await.unwrap()
}

#[tokio::test]
#[ignore = "requires isolated TEST_DATABASE_URL ending in _test with CREATEDB"]
async fn adaptive_batch_atomic_order_local_indices_exact_ack_and_single_wrapper_parity() {
    isolated(|pool|async move {
        let close=chrono::Utc::now().timestamp().div_euclid(60)*60;
        let profile=adaptive_batch_profile(&pool,close).await;
        let seed=adaptive_batch_scan(&profile,"BTCUSDT",close-60);
        assert!(db::commit_adaptive_scan(&pool,&profile,&seed.instrument_key,None,&seed.checkpoint,&seed.setups).await.unwrap().applied);
        let previous=db::get_adaptive_checkpoint(&pool,profile.slot_id,&seed.instrument_key).await.unwrap().unwrap();
        let mut scans=vec![adaptive_batch_scan(&profile,"ZZZUSDT",close),adaptive_batch_scan(&profile,"BTCUSDT",close),adaptive_batch_scan(&profile,"ETHUSDT",close)];
        scans[1].previous=Some(previous);scans[1].setups.insert(0,seed.setups[0].clone());
        // Captured operation name is authoritative, not a later settings lookup.
        setups_db::settings_set(&pool,&format!("algorithm:{ALGORITHM}"),&json!({"externalName":"renamed later"})).await.unwrap();
        let results=db::commit_adaptive_scans_cas(&pool,&profile,&scans).await.unwrap();
        assert!(results.iter().all(|result|result.applied));
        assert_eq!(results.iter().map(|r|r.inserted.iter().map(|i|i.index).collect::<Vec<_>>()).collect::<Vec<_>>(),vec![vec![0],vec![1],vec![0]]);
        assert!(results[1].inserted[0].id<results[2].inserted[0].id && results[2].inserted[0].id<results[0].inserted[0].id,"writes lock/order by key, results remain in input order");
        let ids:Vec<_>=results.iter().flat_map(|result|result.inserted.iter().map(|row|row.id as i32)).collect();
        for (scan,result) in scans.iter().zip(&results) {
            let stored:Value=sqlx::query_scalar("select to_jsonb(s) from live_setups s where id=$1").bind(result.inserted[0].id as i32).fetch_one(&pool).await.unwrap();
            assert_eq!(format!("bybit/linear/{}",stored["symbol"].as_str().unwrap()),scan.instrument_key);
            assert_eq!(stored["external_name"],profile.external_name);
            assert_eq!(db::get_adaptive_checkpoint(&pool,profile.slot_id,&scan.instrument_key).await.unwrap(),Some(db::AdaptiveCheckpoint {generation_id:profile.generation_id,state:scan.checkpoint.clone()}));
        }
        let transaction_count:i64=sqlx::query_scalar("select count(distinct xid) from (select xmin::text xid from live_setups where id=any($1) union all select xmin::text from adaptive_notification_outbox where setup_id=any($1) union all select xmin::text from adaptive_live_checkpoints where slot_id=$2) writes")
            .bind(&ids).bind(profile.slot_id).fetch_one(&pool).await.unwrap();
        assert_eq!(transaction_count,1,"all new setup/outbox/checkpoint writes share one transaction");
        assert_eq!(sqlx::query_scalar::<_,i64>("select count(*) from adaptive_notification_outbox where setup_id=any($1)").bind(&ids).fetch_one(&pool).await.unwrap(),3);
        let committed=adaptive_batch_state(&pool).await;
        // Model a lost ACK: an exact retry may be stale, but exact durable state
        // above permits worker adoption without inventing newly inserted IDs.
        let retried=db::commit_adaptive_scans_cas(&pool,&profile,&scans).await.unwrap();
        assert!(retried.iter().all(|result|!result.applied&&result.inserted.is_empty()));
        assert_eq!(adaptive_batch_state(&pool).await,committed);
    }).await;
}

#[tokio::test]
#[ignore = "requires isolated TEST_DATABASE_URL ending in _test with CREATEDB"]
async fn adaptive_batch_partial_cas_durable_readiness_global_fences_promotion_and_stop() {
    isolated(|pool|async move {
        let close=chrono::Utc::now().timestamp().div_euclid(60)*60;
        let mut profile=adaptive_batch_profile(&pool,close).await;
        let seed=adaptive_batch_scan(&profile,"ETHUSDT",close-60);
        assert!(db::commit_adaptive_scan_cas(&pool,&profile,&seed.instrument_key,None,&seed.checkpoint,&seed.setups).await.unwrap().applied);
        for entry in &mut profile.live_readiness {entry.status="ready".into();}
        let mut scans:Vec<_>=["ETHUSDT","BTCUSDT","WAITUSDT","UNSEENUSDT"].iter().map(|symbol|adaptive_batch_scan(&profile,symbol,close)).collect();
        let result=db::commit_adaptive_scans_cas(&pool,&profile,&scans).await.unwrap();
        assert_eq!(result.iter().map(|r|r.applied).collect::<Vec<_>>(),vec![false,true,false,false],"stale or caller-forged readiness cannot exclude a valid sibling or apply waiting/absent metadata");
        assert!(result[0].inserted.is_empty()&&result[2].inserted.is_empty()&&result[3].inserted.is_empty());
        scans[0].previous=db::get_adaptive_checkpoint(&pool,profile.slot_id,&scans[0].instrument_key).await.unwrap();
        scans[1].previous=Some(db::AdaptiveCheckpoint {generation_id:profile.generation_id,state:json!({"stale":true})});
        assert_eq!(db::commit_adaptive_scans_cas(&pool,&profile,&scans).await.unwrap().iter().map(|r|r.applied).collect::<Vec<_>>(),vec![true,false,false,false]);
        for (disable,enable) in [
            ("update settings set value='{}' where key='live'","update settings set value='{\"enabled\":true}' where key='live'"),
            ("update continuous_processes set config_revision=config_revision+1","update continuous_processes set config_revision=config_revision-1"),
            ("update continuous_slots set enabled=false","update continuous_slots set enabled=true"),
            ("update continuous_processes set config=jsonb_set(config,'{adaptiveEnabled}','false')","update continuous_processes set config=jsonb_set(config,'{adaptiveEnabled}','true')"),
        ] {
            sqlx::query(disable).execute(&pool).await.unwrap();let before=adaptive_batch_state(&pool).await;
            assert!(db::commit_adaptive_scans_cas(&pool,&profile,&scans).await.unwrap().iter().all(|r|!r.applied&&r.inserted.is_empty()));
            assert_eq!(adaptive_batch_state(&pool).await,before);sqlx::query(enable).execute(&pool).await.unwrap();
        }
        let pending=pending_fixture(&pool,&profile).await;
        let fence=db::live::claim_pending_warmup_at(&pool,&pending,"batch-promotion",close).await.unwrap().unwrap();
        assert!(db::live::report_warmup(&pool,&fence,"ready",Some(close),None).await.unwrap());
        let mut expected=Vec::new();
        for symbol in ["BTCUSDT","ETHUSDT","ZZZUSDT","WAITUSDT","UNSEENUSDT"] {
            let key=format!("bybit/linear/{symbol}");let checkpoint=db::get_adaptive_checkpoint(&pool,profile.slot_id,&key).await.unwrap();
            expected.push(db::live::CheckpointExpectation {instrument_key:key,checkpoint});
        }
        let initial=[db::live::InitialAdaptiveCheckpoint {instrument_key:INSTRUMENT.into(),state:promotion_state(fence.generation_id,close)}];
        assert!(db::live::promote_pending(&pool,&fence,Some(profile.generation_id),&expected,close,&initial).await.unwrap());
        let before=adaptive_batch_state(&pool).await;
        assert!(db::commit_adaptive_scans_cas(&pool,&profile,&scans).await.unwrap().iter().all(|r|!r.applied));
        assert_eq!(adaptive_batch_state(&pool).await,before);
        let promoted=db::active_adaptive_slots(&pool,OWNER).await.unwrap().remove(0);
        assert!(db::stop(&pool,OWNER,ALGORITHM).await.unwrap());let stopped=adaptive_batch_state(&pool).await;
        let scan=adaptive_batch_scan(&promoted,"BTCUSDT",close+60);
        assert!(!db::commit_adaptive_scans_cas(&pool,&promoted,&[scan]).await.unwrap()[0].applied);
        assert_eq!(adaptive_batch_state(&pool).await,stopped);
    }).await;
}

#[tokio::test]
#[ignore = "requires isolated TEST_DATABASE_URL ending in _test with CREATEDB"]
async fn adaptive_batch_late_validation_outbox_failure_and_statement_timeout_roll_back_every_item()
{
    isolated(|pool|async move {
        let close=chrono::Utc::now().timestamp().div_euclid(60)*60;
        let profile=adaptive_batch_profile(&pool,close).await;
        let mut scans=vec![adaptive_batch_scan(&profile,"ZZZUSDT",close),adaptive_batch_scan(&profile,"BTCUSDT",close)];
        sqlx::raw_sql("create sequence fixture_outbox_attempts; create function fixture_batch_outbox() returns trigger language plpgsql as $$begin perform nextval('fixture_outbox_attempts'); if new.payload#>>'{instrument,symbol}'='ZZZUSDT' then raise exception 'batch_outbox_rollback_sentinel'; end if; return new; end$$; create trigger fixture_batch_outbox before insert on adaptive_notification_outbox for each row execute function fixture_batch_outbox();")
            .execute(&pool).await.unwrap();
        let original=adaptive_batch_state(&pool).await;
        scans[0].setups[0].setup.algorithm_id="wrong_algorithm".into();
        assert!(db::commit_adaptive_scans_cas(&pool,&profile,&scans).await.err().unwrap().to_string().contains("differs from generation"));
        assert_eq!(sqlx::query_scalar::<_,i64>("select last_value from fixture_outbox_attempts").fetch_one(&pool).await.unwrap(),1,"the earlier sorted item really reached outbox before validation failed");
        assert_eq!(adaptive_batch_state(&pool).await,original);
        scans[0].setups[0].setup.algorithm_id=ALGORITHM.into();
        assert!(db::commit_adaptive_scans_cas(&pool,&profile,&scans).await.err().unwrap().to_string().contains("batch_outbox_rollback_sentinel"));
        assert_eq!(sqlx::query_scalar::<_,i64>("select last_value from fixture_outbox_attempts").fetch_one(&pool).await.unwrap(),3);
        assert_eq!(adaptive_batch_state(&pool).await,original,"outbox budgets, setups and checkpoints all roll back");
        sqlx::raw_sql("drop trigger fixture_batch_outbox on adaptive_notification_outbox; create function fixture_batch_timeout() returns trigger language plpgsql as $$begin if new.instrument_key='bybit/linear/ZZZUSDT' then perform pg_sleep(3); end if; return new; end$$; create trigger fixture_batch_timeout before insert on adaptive_live_checkpoints for each row execute function fixture_batch_timeout();")
            .execute(&pool).await.unwrap();
        let error=db::commit_adaptive_scans_cas(&pool,&profile,&scans).await.err().unwrap();
        assert!(error.to_string().contains("statement timeout"),"{error}");
        assert_eq!(adaptive_batch_state(&pool).await,original,"timed-out bulk checkpoint cannot leave earlier setup/outbox writes");
        // SET LOCAL does not impose the new batch timeout on a legacy scan.
        let scan=&scans[0];
        assert!(db::commit_adaptive_scan_cas(&pool,&profile,&scan.instrument_key,None,&scan.checkpoint,&scan.setups).await.unwrap().applied);
    }).await;
}

#[tokio::test]
#[ignore = "requires isolated TEST_DATABASE_URL ending in _test with CREATEDB"]
async fn saved_matrix_payload_projects_own_mode_for_mean_forecast_and_legacy() {
    isolated(|pool| async move {
        setups_db::migrate::ensure_migrations(&pool).await.unwrap();
        let slot = configured(&pool, OWNER, ALGORITHM).await;
        for (index, (mode, lambda)) in [(Some("mean"),None),(Some("forecast"),None),(None,None),(Some("blend"),Some(0.6)),(Some("blend"),None)].into_iter().enumerate() {
            let id = generation(&pool, OWNER, ALGORITHM, slot, "completed").await;
            let mut saved_spec = spec(ALGORITHM);
            if let Some(mode) = mode { saved_spec["trendMode"] = json!(mode); }
            if let Some(lambda) = lambda { saved_spec["trendBlendLambda"] = json!(lambda); }
            let selected = json!({"tpPct":14,"slPct":4,"rawR":0.32,"fixture":index});
            let result = json!({"meanExpectancy":0.21,"selectedCell":selected});
            sqlx::query("update continuous_generations set spec=$2,result=$3 where id=$1")
                .bind(id).bind(saved_spec).bind(result).execute(&pool).await.unwrap();
            sqlx::query("update continuous_slots set active_generation_id=$2,mean_expectancy=0.21,selected_cell=$3 where id=$1")
                .bind(slot).bind(id).bind(&selected).execute(&pool).await.unwrap();
            // A newly configured mode and a newer completed generation must not
            // relabel the matrix still belonging to the incumbent.
            let opposite = if mode == Some("forecast") { "mean" } else { "forecast" };
            sqlx::query("update continuous_processes set config=jsonb_set(config,'{study,trendMode}',$3) where owner_id=$1 and algorithm_id=$2")
                .bind(OWNER).bind(ALGORITHM).bind(json!(opposite)).execute(&pool).await.unwrap();
            let newer = generation(&pool, OWNER, ALGORITHM, slot, "completed").await;
            let mut newer_spec = spec(ALGORITHM); newer_spec["trendMode"] = json!(opposite);
            sqlx::query("update continuous_generations set spec=$2,result=$3 where id=$1")
                .bind(newer).bind(newer_spec).bind(json!({"meanExpectancy":0.99,"selectedCell":selected}))
                .execute(&pool).await.unwrap();
            let expected = mode.unwrap_or("mean");
            let detail = serde_json::to_value(db::get(&pool, OWNER, ALGORITHM).await.unwrap().unwrap()).unwrap();
            assert_eq!(detail["slots"][0]["trendMode"], expected);
            assert_eq!(detail["slots"][0]["meanExpectancy"], 0.21);
            let expected_lambda = if mode == Some("blend") { json!(lambda.unwrap_or(0.7)) } else { Value::Null };
            assert_eq!(detail["slots"][0]["trendBlendLambda"], expected_lambda);
            let list = serde_json::to_value(db::list(&pool, OWNER, Some(ALGORITHM)).await.unwrap()).unwrap();
            assert_eq!(list[0]["slots"][0]["trendMode"], expected);
            assert_eq!(list[0]["slots"][0]["trendBlendLambda"], expected_lambda);
            // Stopping/reconfiguration may clear live ownership while retaining
            // the saved mean: recover only from its matching frozen result.
            sqlx::query("update continuous_slots set active_generation_id=null where id=$1")
                .bind(slot).execute(&pool).await.unwrap();
            let saved = serde_json::to_value(db::get(&pool, OWNER, ALGORITHM).await.unwrap().unwrap()).unwrap();
            assert_eq!(saved["slots"][0]["trendMode"], expected);
            assert_eq!(saved["slots"][0]["trendBlendLambda"], expected_lambda);
        }
        sqlx::query("update continuous_slots set mean_expectancy=9.99 where id=$1")
            .bind(slot).execute(&pool).await.unwrap();
        let unknown = serde_json::to_value(db::get(&pool, OWNER, ALGORITHM).await.unwrap().unwrap()).unwrap();
        assert!(unknown["slots"][0]["trendMode"].is_null(), "no proven persisted source means unknown, not a guessed default");
    }).await;
}

#[tokio::test]
#[ignore = "requires isolated TEST_DATABASE_URL ending in _test with CREATEDB"]
async fn new_research_search_completion_and_failure_preserve_finalized_profile_provenance() {
    isolated(|pool| async move {
        setups_db::migrate::ensure_migrations(&pool).await.unwrap();
        let old = accepted(&pool).await;
        let lease = scheduler::claim_owner(&pool, "provenance-regression")
            .await
            .unwrap()
            .unwrap();
        let job = scheduler::prepare_next(&pool, &lease)
            .await
            .unwrap()
            .unwrap();
        let projection = |state: db::ContinuousProcess| {
            state
                .slots
                .into_iter()
                .find(|s| s.id == old.slot_id)
                .unwrap()
        };
        let started = projection(db::get(&pool, OWNER, ALGORITHM).await.unwrap().unwrap());
        let serialized = serde_json::to_value(&started).unwrap();
        assert_eq!(serialized["runningStudyId"], job.study_id);
        assert_eq!(serialized["finalizedGenerationId"], old.generation_id);
        assert_eq!(serialized["activeStudyId"], old.study_id);
        assert_eq!(started.study_id, Some(old.study_id));
        assert_eq!(started.finalized_generation_id, Some(old.generation_id));
        assert_eq!(started.running_study_id, Some(job.study_id));
        assert_eq!(started.active_study_id, Some(old.study_id));
        assert_eq!(started.active_generation_id, Some(old.generation_id));
        assert_eq!(
            sqlx::query_scalar::<_, Option<i64>>(
                "select study_id from continuous_slots where id=$1"
            )
            .bind(old.slot_id)
            .fetch_one(&pool)
            .await
            .unwrap(),
            Some(old.study_id),
            "admission must not overwrite the saved-result binding"
        );

        // Repair old deployments' cached reference without mutating history or
        // mistaking search completion for the full post-chain boundary.
        sqlx::query("update continuous_slots set study_id=$2 where id=$1")
            .bind(old.slot_id)
            .bind(job.study_id)
            .execute(&pool)
            .await
            .unwrap();
        scheduler::finish_search(&pool, &job, &json!({"progress":{},"best":null}), true)
            .await
            .unwrap();
        scheduler::phase(&pool, &job, "analytics", None)
            .await
            .unwrap();
        let post_chain = projection(db::get(&pool, OWNER, ALGORITHM).await.unwrap().unwrap());
        assert_eq!(post_chain.study_id, Some(old.study_id));
        assert_eq!(post_chain.finalized_generation_id, Some(old.generation_id));
        assert_eq!(post_chain.running_study_id, Some(job.study_id));
        scheduler::fail(&pool, &job, &json!({"code":"post_chain_mismatch"}))
            .await
            .unwrap();
        let failed = projection(db::get(&pool, OWNER, ALGORITHM).await.unwrap().unwrap());
        assert_eq!(failed.running_study_id, None);
        assert_eq!(failed.study_id, Some(old.study_id));
        assert_eq!(failed.finalized_generation_id, Some(old.generation_id));
        assert_eq!(failed.active_study_id, Some(old.study_id));
        assert_eq!(failed.active_generation_id, Some(old.generation_id));
        assert_eq!(failed.mean_expectancy, started.mean_expectancy);
        assert_eq!(failed.selected_cell, started.selected_cell);
        assert_eq!(
            db::active_adaptive_slots(&pool, OWNER).await.unwrap()[0].params,
            old.params
        );
    })
    .await;
}

async fn pending_fixture(
    pool: &PgPool,
    active: &db::ActiveAdaptiveSlot,
) -> db::live::PendingAdaptiveSlot {
    let generation = generation(pool, OWNER, ALGORITHM, active.slot_id, "completed").await;
    let mut result = active.result.clone();
    result["meanExpectancy"] = json!(0.21);
    result["bootstrap"] = json!({"frozenPrefix":{"version":1}});
    sqlx::query("update continuous_generations g set study_id=$2,accepted=true,result=$3,config_revision=p.config_revision,spec=p.config->'study' from continuous_processes p where g.id=$1 and p.owner_id=g.owner_id and p.algorithm_id=g.algorithm_id")
        .bind(generation).bind(active.study_id).bind(result).execute(pool).await.unwrap();
    sqlx::query(
        "update continuous_slots set pending_generation_id=$2,pending_status='warming' where id=$1",
    )
    .bind(active.slot_id)
    .bind(generation)
    .execute(pool)
    .await
    .unwrap();
    db::live::pending_adaptive_slots(pool, OWNER)
        .await
        .unwrap()
        .remove(0)
}

#[tokio::test]
#[ignore = "requires isolated TEST_DATABASE_URL ending in _test with CREATEDB"]
async fn independent_heartbeat_survives_slow_tick_owner_row_locks_and_saturated_data_pool() {
    use db::live::*;
    isolated(|pool|async move {
        setups_db::migrate::ensure_migrations(&pool).await.unwrap();
        let old=accepted(&pool).await;let pending=pending_fixture(&pool,&old).await;
        let fence=claim_pending_warmup_at(&pool,&pending,"slow-data-plane",CUTOFF).await.unwrap().unwrap();
        let control=PgPoolOptions::new().max_connections(1).connect_with(pool.connect_options().as_ref().clone()).await.unwrap();
        let deadline:Value=sqlx::query_scalar("select to_jsonb(live_init_deadline) from continuous_generations where id=$1")
            .bind(fence.generation_id).fetch_one(&pool).await.unwrap();
        // Scale only the initial lease, not the fixed attempt deadline. The
        // successful simulated tick below outlives this TTL several times.
        sqlx::query("update adaptive_live_warmup_lane set lease_until=clock_timestamp()+interval '400 milliseconds'").execute(&pool).await.unwrap();
        let mut held=pool.begin().await.unwrap();
        sqlx::query("select pg_advisory_xact_lock(hashtextextended($1,72423415))").bind(OWNER).execute(&mut *held).await.unwrap();
        sqlx::query("select owner_id from continuous_processes where owner_id=$1 for update").bind(OWNER).execute(&mut *held).await.unwrap();
        sqlx::query("select id from continuous_slots where id=$1 for update").bind(old.slot_id).execute(&mut *held).await.unwrap();
        let a=pool.acquire().await.unwrap();let b=pool.acquire().await.unwrap();let c=pool.acquire().await.unwrap();
        let beating=async {
            for _ in 0..30 {
                assert_eq!(tokio::time::timeout(Duration::from_millis(300),renew_pending_warmup(&control,&fence)).await.unwrap().unwrap(),WarmupRenewal::Renewed);
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        };
        tokio::join!(beating,tokio::time::sleep(Duration::from_secs(6)));
        drop((a,b,c));held.rollback().await.unwrap();
        assert_eq!(recover_pending_warmups(&pool,OWNER).await.unwrap(),0,"renewed slow successful tick must not reap itself");
        assert_eq!(sqlx::query_scalar::<_,Value>("select to_jsonb(live_init_deadline) from continuous_generations where id=$1")
            .bind(fence.generation_id).fetch_one(&pool).await.unwrap(),deadline,"heartbeats never extend the fixed attempt deadline");
        assert_eq!(db::active_adaptive_slots(&pool,OWNER).await.unwrap()[0].generation_id,old.generation_id);
        assert_eq!(sqlx::query_scalar::<_,i32>("select live_init_attempts from continuous_generations where id=$1")
            .bind(fence.generation_id).fetch_one(&pool).await.unwrap(),1,"held data-plane locks must not consume another attempt");
        assert!(release_pending_warmup(&pool,&fence).await.unwrap());control.close().await;
    }).await;
}

#[tokio::test]
#[ignore = "requires isolated TEST_DATABASE_URL ending in _test with CREATEDB"]
async fn heartbeat_rechecks_expiry_after_lane_lock_wait_without_resurrecting_the_claim() {
    use db::live::*;
    isolated(|pool|async move {
        setups_db::migrate::ensure_migrations(&pool).await.unwrap();
        let old=accepted(&pool).await;let pending=pending_fixture(&pool,&old).await;
        let fence=claim_pending_warmup_at(&pool,&pending,"expiry-during-renewal",CUTOFF).await.unwrap().unwrap();
        sqlx::query("update adaptive_live_warmup_lane set lease_until=clock_timestamp()+interval '200 milliseconds'")
            .execute(&pool).await.unwrap();
        let mut held=pool.begin().await.unwrap();
        sqlx::query("select id from adaptive_live_warmup_lane where id=1 for update").execute(&mut *held).await.unwrap();
        let (renewal,())=tokio::join!(renew_pending_warmup_until(&pool,&fence),async {
            tokio::time::sleep(Duration::from_millis(300)).await;held.rollback().await.unwrap();
        });
        assert!(renewal.unwrap().is_none(),"expiry must be checked AFTER acquiring the lane row lock");
        assert_eq!(recover_pending_warmups(&pool,OWNER).await.unwrap(),1);
        assert_eq!(db::active_adaptive_slots(&pool,OWNER).await.unwrap()[0].generation_id,old.generation_id);
    }).await;
}

#[tokio::test]
#[ignore = "requires isolated TEST_DATABASE_URL ending in _test with CREATEDB"]
async fn heartbeat_statement_cancels_are_short_retryable_and_do_not_spend_attempts_or_extend_credit()
 {
    use db::live::*;
    isolated(|pool|async move {
        setups_db::migrate::ensure_migrations(&pool).await.unwrap();
        let old=accepted(&pool).await;let pending=pending_fixture(&pool,&old).await;
        let fence=claim_pending_warmup_at(&pool,&pending,"statement-cancel-storm",CUTOFF).await.unwrap().unwrap();
        let before:Value=sqlx::query_scalar("select to_jsonb(lease_until) from adaptive_live_warmup_lane").fetch_one(&pool).await.unwrap();
        sqlx::raw_sql("create function slow_warmup_renewal() returns trigger language plpgsql as $$begin perform pg_sleep(2); return new; end$$; create trigger slow_warmup_renewal before update of lease_until on adaptive_live_warmup_lane for each row execute function slow_warmup_renewal();")
            .execute(&pool).await.unwrap();
        for _ in 0..3 {
            let started=std::time::Instant::now();
            let error=renew_pending_warmup_until(&pool,&fence).await.unwrap_err();
            assert_eq!(setups_db::diagnostics::db_error_code(&error),"statement_timeout_or_cancel");
            assert!(started.elapsed()<Duration::from_millis(1500),"heartbeat statement timeout must be sub-second, below the 3s I/O budget");
        }
        assert_eq!(sqlx::query_scalar::<_,Value>("select to_jsonb(lease_until) from adaptive_live_warmup_lane").fetch_one(&pool).await.unwrap(),before);
        sqlx::query("drop trigger slow_warmup_renewal on adaptive_live_warmup_lane").execute(&pool).await.unwrap();
        let started=std::time::Instant::now();
        let until=renew_pending_warmup_until(&pool,&fence).await.unwrap().unwrap();
        assert!(until<=started+Duration::from_secs(30),"latency cannot manufacture extra lease credit");
        assert!(until>std::time::Instant::now()+Duration::from_secs(25));
        assert_eq!(sqlx::query_scalar::<_,i32>("select live_init_attempts from continuous_generations where id=$1").bind(fence.generation_id).fetch_one(&pool).await.unwrap(),1);
        assert_eq!(recover_pending_warmups(&pool,OWNER).await.unwrap(),0);
        assert_eq!(db::active_adaptive_slots(&pool,OWNER).await.unwrap()[0].generation_id,old.generation_id);
        assert!(release_pending_warmup(&pool,&fence).await.unwrap());
    }).await;
}

#[tokio::test]
#[ignore = "requires isolated TEST_DATABASE_URL ending in _test with CREATEDB"]
async fn clean_shutdown_refunds_only_current_claim_and_restart_gets_fresh_window_without_incumbent_reset()
 {
    use db::live::*;
    isolated(|pool|async move {
        setups_db::migrate::ensure_migrations(&pool).await.unwrap();
        let old=accepted(&pool).await;let pending=pending_fixture(&pool,&old).await;
        sqlx::query("insert into adaptive_live_checkpoints(slot_id,instrument_key,generation_id,state) values($1,$2,$3,$4)")
            .bind(old.slot_id).bind(INSTRUMENT).bind(old.generation_id).bind(json!({"nextOpenUnix":CUTOFF})).execute(&pool).await.unwrap();
        let incumbent:Value=sqlx::query_scalar("select jsonb_build_object('generation',s.active_generation_id,'study',a.study_id,'params',a.result,'mean',s.mean_expectancy,'checkpoint',(select to_jsonb(c) from adaptive_live_checkpoints c where c.slot_id=s.id)) from continuous_slots s join continuous_generations a on a.id=s.active_generation_id where s.id=$1")
            .bind(old.slot_id).fetch_one(&pool).await.unwrap();
        let first=claim_pending_warmup_at(&pool,&pending,"before-clean-shutdown",CUTOFF).await.unwrap().unwrap();
        sqlx::query("update continuous_generations set live_init_deadline=clock_timestamp()+interval '30 minutes' where id=$1")
            .bind(first.generation_id).execute(&pool).await.unwrap();
        let mut research=pool.begin().await.unwrap();
        sqlx::query("select pg_advisory_xact_lock(hashtextextended($1,72423415))").bind(OWNER).execute(&mut *research).await.unwrap();
        assert!(tokio::time::timeout(Duration::from_millis(500),release_pending_warmup(&pool,&first)).await.unwrap().unwrap(),"clean release is not queued behind unrelated research's owner lock");
        research.rollback().await.unwrap();
        assert_eq!(sqlx::query_scalar::<_,i32>("select live_init_attempts from continuous_generations where id=$1").bind(first.generation_id).fetch_one(&pool).await.unwrap(),0);
        assert_eq!(renew_pending_warmup(&pool,&first).await.unwrap(),WarmupRenewal::Rejected);
        let second=claim_pending_warmup_at(&pool,&pending,"after-clean-restart",CUTOFF+86400).await.unwrap().unwrap();
        assert_eq!(second.attempt,1);assert_ne!(second.token,first.token);assert_eq!(second.initialization,first.initialization);
        assert!((21590..=21601).contains(&(second.attempt_deadline_unix-chrono::Utc::now().timestamp())));
        assert_eq!(sqlx::query_scalar::<_,Value>("select jsonb_build_object('generation',s.active_generation_id,'study',a.study_id,'params',a.result,'mean',s.mean_expectancy,'checkpoint',(select to_jsonb(c) from adaptive_live_checkpoints c where c.slot_id=s.id)) from continuous_slots s join continuous_generations a on a.id=s.active_generation_id where s.id=$1")
            .bind(old.slot_id).fetch_one(&pool).await.unwrap(),incumbent);
        assert!(!release_pending_warmup(&pool,&first).await.unwrap(),"old worker cannot refund the new claim");
        assert!(report_warmup_failure(&pool,&second,&json!({"code":"real_source_failure"}),true).await.unwrap());
        // Neutral shutdown refunds exactly the current charge, not previous
        // real failures. Repeated restarts never manufacture readmission.
        for attempt in 2..=3 {
            sqlx::query("update continuous_generations set live_init_next_attempt_at=null where id=$1").bind(second.generation_id).execute(&pool).await.unwrap();
            let current=claim_pending_warmup_at(&pool,&pending,"real-retry",CUTOFF).await.unwrap().unwrap();assert_eq!(current.attempt,attempt);
            assert!(release_pending_warmup(&pool,&current).await.unwrap());
            let current=claim_pending_warmup_at(&pool,&pending,"after-another-restart",CUTOFF).await.unwrap().unwrap();assert_eq!(current.attempt,attempt);
            assert!(report_warmup_failure(&pool,&current,&json!({"code":"real_source_failure"}),true).await.unwrap());
        }
        assert!(claim_pending_warmup_at(&pool,&pending,"must-not-auto-replenish",CUTOFF).await.unwrap().is_none());
        let request=live_readmission_request(second.generation_id,"explicit-after-real-failures");
        assert_eq!(granted_live(&pool,old.slot_id,&request).await.attempt_limit,6);
        let pending=pending_adaptive_slots(&pool,OWNER).await.unwrap().remove(0);
        assert_eq!(claim_pending_warmup_at(&pool,&pending,"readmitted",CUTOFF).await.unwrap().unwrap().attempt,4);
    }).await;
}

#[tokio::test]
#[ignore = "requires isolated TEST_DATABASE_URL ending in _test with CREATEDB"]
async fn expired_shutdown_never_refunds_and_heartbeat_never_resurrects_lease_or_fixed_deadline() {
    use db::live::*;
    isolated(|pool|async move {
        setups_db::migrate::ensure_migrations(&pool).await.unwrap();let old=accepted(&pool).await;let pending=pending_fixture(&pool,&old).await;
        for attempt in 1..=2 {
            let fence=claim_pending_warmup_at(&pool,&pending,"expired-shutdown",CUTOFF).await.unwrap().unwrap();assert_eq!(fence.attempt,attempt);
            if attempt==1 {
                sqlx::query("update adaptive_live_warmup_lane set lease_until=clock_timestamp()+interval '200 milliseconds'").execute(&pool).await.unwrap();
                let mut held=pool.begin().await.unwrap();sqlx::query("select id from continuous_generations where id=$1 for update").bind(fence.generation_id).execute(&mut *held).await.unwrap();
                let (release,())=tokio::join!(release_pending_warmup(&pool,&fence),async move {tokio::time::sleep(Duration::from_millis(300)).await;held.rollback().await.unwrap();});
                assert!(!release.unwrap(),"a lease expiring while shutdown waits for its generation cannot be refunded");
            }
            else {sqlx::query("update continuous_generations set live_init_deadline=clock_timestamp()-interval '1 second' where id=$1").bind(fence.generation_id).execute(&pool).await.unwrap();}
            assert_eq!(renew_pending_warmup(&pool,&fence).await.unwrap(),WarmupRenewal::Rejected);
            assert!(!release_pending_warmup(&pool,&fence).await.unwrap());
            assert_eq!(sqlx::query_scalar::<_,i32>("select live_init_attempts from continuous_generations where id=$1").bind(fence.generation_id).fetch_one(&pool).await.unwrap(),attempt);
            assert_eq!(recover_pending_warmups(&pool,OWNER).await.unwrap(),1);
            sqlx::query("update continuous_generations set live_init_next_attempt_at=null where id=$1").bind(fence.generation_id).execute(&pool).await.unwrap();
        }
        assert_eq!(db::active_adaptive_slots(&pool,OWNER).await.unwrap()[0].generation_id,old.generation_id);
    }).await;
}

#[tokio::test]
#[ignore = "requires isolated TEST_DATABASE_URL ending in _test with CREATEDB"]
async fn supervision_loss_during_promotion_owner_wait_or_final_commit_gate_preserves_incumbent() {
    use db::live::*;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };
    isolated(|pool| async move {
        setups_db::migrate::ensure_migrations(&pool).await.unwrap();
        let old = accepted(&pool).await;
        let pending = pending_fixture(&pool, &old).await;
        let fence = claim_pending_warmup_at(&pool, &pending, "guarded-promotion", CUTOFF)
            .await
            .unwrap()
            .unwrap();
        assert!(
            report_warmup(&pool, &fence, "ready", Some(CUTOFF), None)
                .await
                .unwrap()
        );
        let expected = [CheckpointExpectation {
            instrument_key: INSTRUMENT.into(),
            checkpoint: None,
        }];
        let initial = [InitialAdaptiveCheckpoint {
            instrument_key: INSTRUMENT.into(),
            state: promotion_state(fence.generation_id, CUTOFF),
        }];
        let mut held = pool.begin().await.unwrap();
        sqlx::query("select pg_advisory_xact_lock(hashtextextended($1,72423415))")
            .bind(OWNER)
            .execute(&mut *held)
            .await
            .unwrap();
        let permitted = Arc::new(AtomicBool::new(true));
        let lost = permitted.clone();
        let (outcome, ()) = tokio::join!(
            promote_pending_with_dispositions_guarded(
                &pool,
                &fence,
                Some(old.generation_id),
                &expected,
                CUTOFF,
                &initial,
                &[],
                &[],
                || permitted.load(Ordering::SeqCst)
            ),
            async move {
                tokio::time::sleep(Duration::from_millis(30)).await;
                lost.store(false, Ordering::SeqCst);
                held.rollback().await.unwrap();
            }
        );
        assert!(!outcome.unwrap());
        let calls = AtomicUsize::new(0);
        assert!(
            !promote_pending_with_dispositions_guarded(
                &pool,
                &fence,
                Some(old.generation_id),
                &expected,
                CUTOFF,
                &initial,
                &[],
                &[],
                || calls.fetch_add(1, Ordering::SeqCst) < 2
            )
            .await
            .unwrap(),
            "loss at the last pre-COMMIT gate rolls back staged pointer/cursor changes"
        );
        assert_eq!(
            db::active_adaptive_slots(&pool, OWNER).await.unwrap()[0].generation_id,
            old.generation_id
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("select count(*) from adaptive_live_checkpoints")
                .fetch_one(&pool)
                .await
                .unwrap(),
            0
        );
        assert_eq!(
            renew_pending_warmup(&pool, &fence).await.unwrap(),
            WarmupRenewal::Renewed,
            "rolled-back promotion retains the original claim fence"
        );
    })
    .await;
}

#[tokio::test]
#[ignore = "requires isolated TEST_DATABASE_URL ending in _test with CREATEDB"]
async fn independent_initialization_retries_keep_anchor_bound_attempts_and_recover_abandoned_final_claim()
 {
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
        assert_eq!(sqlx::query_scalar::<_,Value>("select live_init_error from continuous_generations where id=$1")
            .bind(id).fetch_one(&pool).await.unwrap()["reason"],"deadline");
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
        let error=slot.pending_error.unwrap();
        assert_eq!(error["code"],"live_initialization_abandoned");
        assert_eq!(error["reason"],"lease");
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

async fn exhausted_live_candidate(
    pool: &PgPool,
) -> (
    db::ActiveAdaptiveSlot,
    db::live::PendingAdaptiveSlot,
    db::live::LiveInitialization,
) {
    setups_db::migrate::ensure_migrations(pool).await.unwrap();
    let old = accepted(pool).await;
    let pending = pending_fixture(pool, &old).await;
    let mut initialization = None;
    for attempt in 1..=3 {
        sqlx::query("update continuous_generations set live_init_next_attempt_at=null where id=$1")
            .bind(pending.profile.generation_id)
            .execute(pool)
            .await
            .unwrap();
        let fence = db::live::claim_pending_warmup_at(pool, &pending, "exhaust-fixture", CUTOFF)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(fence.attempt, attempt);
        initialization = Some(fence.initialization.clone());
        assert!(
            db::live::report_warmup_failure(
                pool,
                &fence,
                &json!({"code":"observed_terminal_error","attempt":attempt}),
                true
            )
            .await
            .unwrap()
        );
    }
    (old, pending, initialization.unwrap())
}
fn live_readmission_request(generation: i64, key: &str) -> db::readmission::LiveReadmissionRequest {
    db::readmission::LiveReadmissionRequest {
        expected_generation_id: generation,
        expected_config_revision: 1,
        request_id: key.into(),
    }
}
async fn readmission_snapshot(pool: &PgPool) -> Value {
    sqlx::query_scalar("select jsonb_build_object('processes',(select jsonb_agg(to_jsonb(p) order by owner_id,algorithm_id) from continuous_processes p),'generations',(select jsonb_agg(to_jsonb(g) order by id) from continuous_generations g),'slots',(select jsonb_agg(to_jsonb(s) order by id) from continuous_slots s),'lane',(select to_jsonb(w) from adaptive_live_warmup_lane w),'grants',(select jsonb_agg(to_jsonb(r) order by id) from continuous_live_readmissions r))")
        .fetch_one(pool).await.unwrap()
}
async fn granted_live(
    pool: &PgPool,
    slot: i64,
    request: &db::readmission::LiveReadmissionRequest,
) -> db::readmission::LiveReadmissionReceipt {
    match db::readmission::grant(pool, OWNER, ALGORITHM, slot, request)
        .await
        .unwrap()
    {
        db::readmission::LiveReadmissionOutcome::Granted(receipt) => receipt,
        outcome => panic!("expected grant: {outcome:?}"),
    }
}

#[tokio::test]
#[ignore = "requires isolated TEST_DATABASE_URL ending in _test with CREATEDB"]
async fn live_readmission_preserves_scientific_origin_ongoing_other_slot_research_and_promotes_normally()
 {
    isolated(|pool|async move {
        let (old,pending,origin)=exhausted_live_candidate(&pool).await;let id=pending.profile.generation_id;
        let other_slot:i64=sqlx::query_scalar("insert into continuous_slots(owner_id,algorithm_id,min_setups_per_day) values($1,$2,5) returning id")
            .bind(OWNER).bind(ALGORITHM).fetch_one(&pool).await.unwrap();
        let research=generation(&pool,OWNER,ALGORITHM,other_slot,"running").await;
        let study=study(&pool,OWNER,ALGORITHM,"running",Some(research),Some(other_slot)).await;
        sqlx::query("update continuous_generations set research_deadline=now()+interval '10 minutes',resume_version=1 where id=$1").bind(research).execute(&pool).await.unwrap();
        sqlx::query("update continuous_processes set active_generation_id=$1,active_study_id=$2,retry_count=2,next_slot=1,phase='optimizing' where owner_id=$3 and algorithm_id=$4")
            .bind(research).bind(study).bind(OWNER).bind(ALGORITHM).execute(&pool).await.unwrap();
        sqlx::query("update continuous_owner_lanes set lease_owner='research22',lease_token=19,lease_until=now()+interval '10 minutes' where owner_id=$1").bind(OWNER).execute(&pool).await.unwrap();
        sqlx::query("insert into continuous_runtime_artifacts(generation_id,identity) values($1,'{\"keep\":true}')").bind(research).execute(&pool).await.unwrap();
        sqlx::query("insert into continuous_maintenance(owner_id,admission_paused) values($1,true)").bind(OWNER).execute(&pool).await.unwrap();
        let preserved=sqlx::query_scalar::<_,Value>("select jsonb_build_object('process',(select to_jsonb(p) from continuous_processes p),'research',(select to_jsonb(g) from continuous_generations g where id=$1),'study',(select to_jsonb(s) from optimization_studies s where id=$2),'lease',(select to_jsonb(l) from continuous_owner_lanes l),'artifact',(select to_jsonb(a) from continuous_runtime_artifacts a),'maintenance',(select to_jsonb(m) from continuous_maintenance m))")
            .bind(research).bind(study).fetch_one(&pool).await.unwrap();
        let immutable:Value=sqlx::query_scalar("select jsonb_build_object('spec',spec,'result',result,'accepted',accepted,'status',status,'study',study_id,'updatedAt',updated_at,'cutoff',observation_cutoff) from continuous_generations where id=$1")
            .bind(id).fetch_one(&pool).await.unwrap();
        let request=live_readmission_request(id,"restore-study-21-original");let receipt=granted_live(&pool,old.slot_id,&request).await;
        assert!(!receipt.duplicate);assert_eq!(receipt.attempts_before,3);assert_eq!(receipt.limit_before,3);assert_eq!(receipt.attempt_limit,6);
        let state=readmission_snapshot(&pool).await;
        let mut duplicate=receipt.clone();duplicate.duplicate=true;
        assert_eq!(granted_live(&pool,old.slot_id,&request).await,duplicate);assert_eq!(readmission_snapshot(&pool).await,state);
        let evidence:Value=sqlx::query_scalar("select prior_state from continuous_live_readmissions where id=$1").bind(receipt.grant_id).fetch_one(&pool).await.unwrap();
        assert_eq!(evidence["attempts"],3);assert_eq!(evidence["lastError"]["attempt"],3);assert_eq!(evidence["liveInitStatus"],"exhausted");assert!(evidence["slotUpdatedAt"].is_string());
        let pending=db::live::pending_adaptive_slots(&pool,OWNER).await.unwrap().remove(0);
        let fence=db::live::claim_pending_warmup_at(&pool,&pending,"ordinary-worker",CUTOFF+3600).await.unwrap().unwrap();
        assert_eq!(fence.attempt,4);assert_eq!(fence.initialization,origin);
        assert!(db::live::report_warmup(&pool,&fence,"ready",Some(CUTOFF+600),None).await.unwrap());
        let expected=[db::live::CheckpointExpectation {instrument_key:INSTRUMENT.into(),checkpoint:None}];
        let initial=[db::live::InitialAdaptiveCheckpoint {instrument_key:INSTRUMENT.into(),state:promotion_state(id,CUTOFF+600)}];
        assert!(db::live::promote_pending(&pool,&fence,Some(old.generation_id),&expected,CUTOFF+600,&initial).await.unwrap());
        assert_eq!(db::active_adaptive_slots(&pool,OWNER).await.unwrap()[0].generation_id,id);
        assert_eq!(sqlx::query_scalar::<_,Value>("select jsonb_build_object('spec',spec,'result',result,'accepted',accepted,'status',status,'study',study_id,'updatedAt',updated_at,'cutoff',observation_cutoff) from continuous_generations where id=$1").bind(id).fetch_one(&pool).await.unwrap(),immutable);
        assert_eq!(sqlx::query_scalar::<_,Value>("select jsonb_build_object('process',(select to_jsonb(p) from continuous_processes p),'research',(select to_jsonb(g) from continuous_generations g where id=$1),'study',(select to_jsonb(s) from optimization_studies s where id=$2),'lease',(select to_jsonb(l) from continuous_owner_lanes l),'artifact',(select to_jsonb(a) from continuous_runtime_artifacts a),'maintenance',(select to_jsonb(m) from continuous_maintenance m))").bind(research).bind(study).fetch_one(&pool).await.unwrap(),preserved);
        assert_eq!(sqlx::query_scalar::<_,Value>("select prior_state from continuous_live_readmissions where id=$1").bind(receipt.grant_id).fetch_one(&pool).await.unwrap(),evidence,"promotion cannot erase observed prior failure evidence");
    }).await;
}

#[tokio::test]
#[ignore = "requires isolated TEST_DATABASE_URL ending in _test with CREATEDB"]
async fn live_readmission_idempotency_races_three_attempt_cycle_and_neutral_yield_preserve_charged_budget()
 {
    isolated(|pool|async move {
        let (old,pending,origin)=exhausted_live_candidate(&pool).await;let id=pending.profile.generation_id;
        let request=live_readmission_request(id,"readmit-concurrent-key-1");
        let (first,second)=tokio::join!(granted_live(&pool,old.slot_id,&request),granted_live(&pool,old.slot_id,&request));
        assert_eq!(first.grant_id,second.grant_id);assert_ne!(first.duplicate,second.duplicate);
        let new_key=live_readmission_request(id,"readmit-concurrent-key-2");
        assert!(matches!(db::readmission::grant(&pool,OWNER,ALGORITHM,old.slot_id,&new_key).await.unwrap(),db::readmission::LiveReadmissionOutcome::Conflict("live_readmission_ineligible")));
        for attempt in 4..=6 {
            sqlx::query("update continuous_generations set live_init_next_attempt_at=null where id=$1").bind(id).execute(&pool).await.unwrap();
            let mut fence=db::live::claim_pending_warmup_at(&pool,&pending,"new-cycle",CUTOFF+3600).await.unwrap().unwrap();
            assert_eq!(fence.attempt,attempt);assert_eq!(fence.initialization,origin);
            if attempt==4 {
                assert!(db::live::release_pending_warmup(&pool,&fence).await.unwrap());
                let charged:(i32,i32)=sqlx::query_as("select live_init_attempts,live_init_attempt_limit from continuous_generations where id=$1").bind(id).fetch_one(&pool).await.unwrap();
                assert_eq!(charged,(3,6),"benign fourth claim refunds exactly one charged attempt");
                assert!(!db::live::report_warmup_failure(&pool,&fence,&json!({"code":"stale-yielded-worker"}),true).await.unwrap());
                fence=db::live::claim_pending_warmup_at(&pool,&pending,"reclaim-fourth",CUTOFF+3600).await.unwrap().unwrap();
                assert_eq!(fence.attempt,4);
            }
            if attempt<6 {assert!(db::live::report_warmup_failure(&pool,&fence,&json!({"code":"new_cycle_failure"}),true).await.unwrap());}
            else {sqlx::query("update adaptive_live_warmup_lane set lease_until=clock_timestamp()-interval '1 second'").execute(&pool).await.unwrap();assert_eq!(db::live::recover_pending_warmups(&pool,OWNER).await.unwrap(),1);}
        }
        assert!(db::live::claim_pending_warmup_at(&pool,&pending,"no-seventh-auto",CUTOFF+7200).await.unwrap().is_none());
        let before=readmission_snapshot(&pool).await;let replay=granted_live(&pool,old.slot_id,&request).await;
        assert!(replay.duplicate);assert_eq!(replay.attempt_limit,6);assert_eq!(readmission_snapshot(&pool).await,before);
        let racing_key=live_readmission_request(id,"readmit-concurrent-key-3");
        let (left,right)=tokio::join!(db::readmission::grant(&pool,OWNER,ALGORITHM,old.slot_id,&new_key),db::readmission::grant(&pool,OWNER,ALGORITHM,old.slot_id,&racing_key));
        let fresh=match (left.unwrap(),right.unwrap()) {
            (db::readmission::LiveReadmissionOutcome::Granted(receipt),db::readmission::LiveReadmissionOutcome::Conflict(_))
            | (db::readmission::LiveReadmissionOutcome::Conflict(_),db::readmission::LiveReadmissionOutcome::Granted(receipt))=>receipt,
            outcomes=>panic!("distinct concurrent keys must grant only once per exhaustion: {outcomes:?}"),
        };
        assert_eq!(fresh.attempts_before,6);assert_eq!(fresh.attempt_limit,9);
        let fence=db::live::claim_pending_warmup_at(&pool,&pending,"neutral-yield",CUTOFF+7200).await.unwrap().unwrap();assert_eq!(fence.attempt,7);
        assert!(db::live::release_pending_warmup(&pool,&fence).await.unwrap());
        let row=sqlx::query("select live_init_attempts,live_init_attempt_limit from continuous_generations where id=$1").bind(id).fetch_one(&pool).await.unwrap();
        assert_eq!(sqlx::Row::get::<i32,_>(&row,"live_init_attempts"),6);assert_eq!(sqlx::Row::get::<i32,_>(&row,"live_init_attempt_limit"),9);
        let next=db::live::claim_pending_warmup_at(&pool,&pending,"after-yield",CUTOFF+7200).await.unwrap().unwrap();assert_eq!(next.attempt,7);
        assert!(db::live::report_warmup_failure(&pool,&next,&json!({"code":"first_charged_failure"}),true).await.unwrap());
        let wait:i64=sqlx::query_scalar("select ceil(extract(epoch from live_init_next_attempt_at-clock_timestamp()))::bigint from continuous_generations where id=$1").bind(id).fetch_one(&pool).await.unwrap();assert!((29..=30).contains(&wait),"later charged failures retain ordinary 30s backoff");
        assert_eq!(sqlx::query_scalar::<_,i64>("select count(*) from continuous_live_readmissions").fetch_one(&pool).await.unwrap(),2);
    }).await;
}

#[tokio::test]
#[ignore = "requires isolated TEST_DATABASE_URL ending in _test with CREATEDB"]
async fn live_readmission_guards_stale_controls_pending_lane_and_slot_scoped_authority() {
    isolated(|pool|async move {
        let (old,pending,_)=exhausted_live_candidate(&pool).await;let id=pending.profile.generation_id;
        let request=live_readmission_request(id,"guarded-readmission-key");
        assert!(matches!(db::readmission::grant(&pool,"foreign",ALGORITHM,old.slot_id,&request).await.unwrap(),db::readmission::LiveReadmissionOutcome::NotFound));
        let mut stale=request.clone();stale.expected_config_revision+=1;
        assert!(matches!(db::readmission::grant(&pool,OWNER,ALGORITHM,old.slot_id,&stale).await.unwrap(),db::readmission::LiveReadmissionOutcome::Conflict(_)));
        for (disable,restore) in [
            ("update continuous_processes set desired_running=false","update continuous_processes set desired_running=true"),
            ("update continuous_processes set config_revision=config_revision+1","update continuous_processes set config_revision=config_revision-1"),
            ("update continuous_processes set config=jsonb_set(config,'{adaptiveEnabled}','false')","update continuous_processes set config=jsonb_set(config,'{adaptiveEnabled}','true')"),
            ("update continuous_slots set enabled=false","update continuous_slots set enabled=true"),
            ("update settings set value='{}' where key='live'","update settings set value='{\"enabled\":true}' where key='live'"),
        ] {sqlx::query(disable).execute(&pool).await.unwrap();let before=readmission_snapshot(&pool).await;
            assert!(matches!(db::readmission::grant(&pool,OWNER,ALGORITHM,old.slot_id,&request).await.unwrap(),db::readmission::LiveReadmissionOutcome::Conflict(_)));
            assert_eq!(readmission_snapshot(&pool).await,before);sqlx::query(restore).execute(&pool).await.unwrap();}
        sqlx::query("update continuous_slots set pending_generation_id=$2 where id=$1").bind(old.slot_id).bind(old.generation_id).execute(&pool).await.unwrap();
        assert!(matches!(db::readmission::grant(&pool,OWNER,ALGORITHM,old.slot_id,&request).await.unwrap(),db::readmission::LiveReadmissionOutcome::Conflict("live_readmission_pending_conflict")));
        sqlx::query("update continuous_slots set pending_generation_id=null where id=$1").bind(old.slot_id).execute(&pool).await.unwrap();
        sqlx::query("update adaptive_live_warmup_lane set slot_id=$1,generation_id=$2,lease_owner='competing-worker',lease_until=clock_timestamp()+interval '30 seconds'").bind(old.slot_id).bind(old.generation_id).execute(&pool).await.unwrap();
        assert!(matches!(db::readmission::grant(&pool,OWNER,ALGORITHM,old.slot_id,&request).await.unwrap(),db::readmission::LiveReadmissionOutcome::Conflict("live_readmission_lane_busy")));
        sqlx::query("update adaptive_live_warmup_lane set slot_id=null,generation_id=null,lease_owner=null,lease_until=null").execute(&pool).await.unwrap();
        for accepted in [Some(true),Some(false),None] {
            let newer=generation(&pool,OWNER,ALGORITHM,old.slot_id,"completed").await;
            sqlx::query("update continuous_generations set accepted=$2 where id=$1").bind(newer).bind(accepted).execute(&pool).await.unwrap();
            assert!(matches!(db::readmission::grant(&pool,OWNER,ALGORITHM,old.slot_id,&request).await.unwrap(),db::readmission::LiveReadmissionOutcome::Conflict("live_readmission_superseded")));
            sqlx::query("delete from continuous_generations where id=$1").bind(newer).execute(&pool).await.unwrap();
        }
        sqlx::query("update continuous_generations set live_init_status='superseded' where id=$1").bind(id).execute(&pool).await.unwrap();
        assert!(matches!(db::readmission::grant(&pool,OWNER,ALGORITHM,old.slot_id,&request).await.unwrap(),db::readmission::LiveReadmissionOutcome::Conflict("live_readmission_ineligible")));
        sqlx::query("update continuous_generations set live_init_status='exhausted' where id=$1").bind(id).execute(&pool).await.unwrap();
        let receipt=granted_live(&pool,old.slot_id,&request).await;
        let mut changed=request.clone();changed.expected_generation_id=old.generation_id;
        assert!(matches!(db::readmission::grant(&pool,OWNER,ALGORITHM,old.slot_id,&changed).await.unwrap(),db::readmission::LiveReadmissionOutcome::Conflict("live_readmission_key_conflict")));
        assert!(db::stop(&pool,OWNER,ALGORITHM).await.unwrap());let stopped=readmission_snapshot(&pool).await;
        let replay=granted_live(&pool,old.slot_id,&request).await;assert!(replay.duplicate);assert_eq!(replay.grant_id,receipt.grant_id);
        assert_eq!(readmission_snapshot(&pool).await,stopped,"a receipt replay after Stop is evidence, not resurrection");
        let fresh=live_readmission_request(id,"after-stop-new-operation");
        assert!(matches!(db::readmission::grant(&pool,OWNER,ALGORITHM,old.slot_id,&fresh).await.unwrap(),db::readmission::LiveReadmissionOutcome::Conflict("live_readmission_stale_process")));
    }).await;
}

#[tokio::test]
#[ignore = "requires isolated TEST_DATABASE_URL ending in _test with CREATEDB"]
async fn live_readmission_ordinary_claim_and_promotion_reject_newer_same_slot_but_not_other_slot_outcomes()
 {
    isolated(|pool|async move {
        let (old,pending,_)=exhausted_live_candidate(&pool).await;let id=pending.profile.generation_id;
        let other_slot:i64=sqlx::query_scalar("insert into continuous_slots(owner_id,algorithm_id,min_setups_per_day) values($1,$2,5) returning id").bind(OWNER).bind(ALGORITHM).fetch_one(&pool).await.unwrap();
        let other=generation(&pool,OWNER,ALGORITHM,other_slot,"completed").await;
        sqlx::query("update continuous_generations set accepted=true where id=$1").bind(other).execute(&pool).await.unwrap();
        granted_live(&pool,old.slot_id,&live_readmission_request(id,"slot-scoped-readmission")).await;
        let newer=generation(&pool,OWNER,ALGORITHM,old.slot_id,"completed").await;
        assert!(db::live::claim_pending_warmup_at(&pool,&pending,"fenced-claim",CUTOFF).await.unwrap().is_none());
        sqlx::query("delete from continuous_generations where id=$1").bind(newer).execute(&pool).await.unwrap();
        let fence=db::live::claim_pending_warmup_at(&pool,&pending,"ordinary-claim",CUTOFF).await.unwrap().unwrap();
        assert!(db::live::report_warmup(&pool,&fence,"ready",Some(CUTOFF+600),None).await.unwrap());
        let newer=generation(&pool,OWNER,ALGORITHM,old.slot_id,"completed").await;
        let expected=[db::live::CheckpointExpectation {instrument_key:INSTRUMENT.into(),checkpoint:None}];
        let initial=[db::live::InitialAdaptiveCheckpoint {instrument_key:INSTRUMENT.into(),state:promotion_state(id,CUTOFF+600)}];
        assert!(!db::live::promote_pending(&pool,&fence,Some(old.generation_id),&expected,CUTOFF+600,&initial).await.unwrap());
        sqlx::query("delete from continuous_generations where id=$1").bind(newer).execute(&pool).await.unwrap();
        assert!(db::live::promote_pending(&pool,&fence,Some(old.generation_id),&expected,CUTOFF+600,&initial).await.unwrap());
    }).await;
}

#[tokio::test]
#[ignore = "requires isolated TEST_DATABASE_URL ending in _test with CREATEDB"]
async fn live_readmission_additive_migration_and_failed_pending_write_preserve_prior_state() {
    isolated(|pool|async move {
        let (old,pending,origin)=exhausted_live_candidate(&pool).await;let id=pending.profile.generation_id;
        let before=readmission_snapshot(&pool).await;
        sqlx::raw_sql("drop table continuous_live_readmissions; drop index continuous_generations_completed_slot; alter table continuous_generations drop constraint continuous_generations_live_init_budget_check,drop column live_init_attempt_limit; alter table continuous_generations add constraint continuous_generations_live_init_attempts_check check(live_init_attempts between 0 and 3); delete from kysely_migration where name='0018_live_readmission.sql';").execute(&pool).await.unwrap();
        assert_eq!(setups_db::migrate::ensure_migrations(&pool).await.unwrap(),"1 applied, 12 up to date");assert_eq!(readmission_snapshot(&pool).await,before);
        sqlx::raw_sql("create function fixture_reject_readmission() returns trigger language plpgsql as $$begin raise exception 'readmission_rollback_sentinel'; end$$; create trigger fixture_reject_readmission before update on continuous_slots for each row execute function fixture_reject_readmission();").execute(&pool).await.unwrap();
        let request=live_readmission_request(id,"readmission-rollback-key");
        assert!(db::readmission::grant(&pool,OWNER,ALGORITHM,old.slot_id,&request).await.err().unwrap().to_string().contains("readmission_rollback_sentinel"));
        assert_eq!(readmission_snapshot(&pool).await,before,"receipt and budget cannot survive a failed pending write");
        sqlx::query("drop trigger fixture_reject_readmission on continuous_slots").execute(&pool).await.unwrap();
        assert!(!granted_live(&pool,old.slot_id,&request).await.duplicate);
        assert_eq!(db::live::pending_adaptive_slots(&pool,OWNER).await.unwrap()[0].profile.live_initialization.as_ref(),Some(&origin));
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
        assert_eq!(setups_db::migrate::ensure_migrations(&pool).await.unwrap(),"1 applied, 12 up to date");
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
        assert_eq!(state.slots[0].study_id,Some(job.study_id));
        assert_eq!(state.slots[0].finalized_generation_id,Some(job.generation_id));
        assert_eq!(state.slots[0].pending_study_id,Some(job.study_id));
        assert_eq!(state.slots[0].active_study_id,Some(old.study_id));
        assert_eq!(state.slots[0].running_study_id,None);
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
async fn retired_unavailable_old_only_cursor_is_explicit_and_cas_exact_without_candidate_scope_relaxation()
 {
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
    setups_db::settings_set(pool, "notifications", &json!({"maxAgeSeconds":3600}))
        .await
        .unwrap();
    assert!(
        db::commit_adaptive_scan_cas(
            pool,
            &old,
            INSTRUMENT,
            None,
            &promotion_state(old.generation_id, frontier),
            &[adaptive_write(frontier - 60)]
        )
        .await
        .unwrap()
        .applied
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("select count(*) from adaptive_notification_outbox")
            .fetch_one(pool)
            .await
            .unwrap(),
        1
    );
    fixed_setup(pool, frontier - 60).await;
    study(pool, OWNER, OTHER_ALGORITHM, "completed", None, None).await;
    let historical_batch = batch(pool, ALGORITHM, Some(old.generation_id)).await;
    run(
        pool,
        historical_batch,
        ALGORITHM,
        "done",
        Some(old.generation_id),
    )
    .await;
    (old, frontier)
}

async fn retained_history(pool: &PgPool, old: &db::ActiveAdaptiveSlot) -> Value {
    sqlx::query_scalar("select jsonb_build_object('generation',(select to_jsonb(g) from continuous_generations g where id=$1),'studies',(select jsonb_agg(to_jsonb(s) order by id) from optimization_studies s where continuous_generation_id=$1 or origin='manual'),'live',(select jsonb_agg(to_jsonb(s) order by id) from live_setups s),'outbox',(select jsonb_agg(to_jsonb(j) order by id) from adaptive_notification_outbox j),'budget',(select to_jsonb(b) from adaptive_notification_budget b where id=1),'runs',(select jsonb_agg(to_jsonb(r) order by id) from replay_runs r where status='done'),'batches',(select jsonb_agg(to_jsonb(b) order by id) from replay_batches b))")
        .bind(old.generation_id).fetch_one(pool).await.unwrap()
}

async fn activate_after_retirement(
    pool: &PgPool,
    old: &db::ActiveAdaptiveSlot,
    key: &str,
    frontier: i64,
) {
    use db::live::*;
    let pending = pending_fixture(pool, old).await;
    assert_eq!(pending.expected_active_generation_id, None);
    let fence = claim_pending_warmup(pool, &pending, "fresh-after-retirement")
        .await
        .unwrap()
        .unwrap();
    assert!(
        report_warmup(pool, &fence, "ready", Some(frontier), None)
            .await
            .unwrap()
    );
    // Exactly the worker contract: compare every currently declared instrument,
    // including absent cursors. No obsolete/out-of-scope predecessor is invented.
    let expected = [CheckpointExpectation {
        instrument_key: key.into(),
        checkpoint: db::get_adaptive_checkpoint(pool, old.slot_id, key)
            .await
            .unwrap(),
    }];
    let initial = [InitialAdaptiveCheckpoint {
        instrument_key: key.into(),
        state: promotion_state(pending.profile.generation_id, frontier),
    }];
    assert!(
        promote_pending(pool, &fence, None, &expected, frontier, &initial)
            .await
            .expect("obsolete inactive cursor must not block the new promotion frontier"),
        "obsolete cursor outside the new scope must not block fresh activation"
    );
    let cursor = db::get_adaptive_checkpoint(pool, old.slot_id, key)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(cursor.state, initial[0].state);
    assert_eq!(cursor.generation_id, pending.profile.generation_id);
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "select count(*) from adaptive_live_checkpoints where slot_id=$1"
        )
        .bind(old.slot_id)
        .fetch_one(pool)
        .await
        .unwrap(),
        1
    );
    assert!(
        !db::commit_adaptive_scan_cas(
            pool,
            old,
            INSTRUMENT,
            None,
            &promotion_state(old.generation_id, frontier + 60),
            &[adaptive_write(frontier)]
        )
        .await
        .unwrap()
        .applied
    );
}

#[tokio::test]
#[ignore = "requires isolated TEST_DATABASE_URL ending in _test with CREATEDB"]
async fn retired_cursors_stop_restart_can_promote_without_rewriting_history() {
    isolated(|pool| async move {
        let (old, frontier) = retirement_fixture(&pool).await;
        let history = retained_history(&pool, &old).await;
        let lease = scheduler::claim_owner(&pool, "retirement-stop")
            .await
            .unwrap()
            .unwrap();
        assert!(db::stop(&pool, OWNER, ALGORITHM).await.unwrap());
        assert!(
            db::get_adaptive_checkpoint(&pool, old.slot_id, INSTRUMENT)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            !db::start(&pool, OWNER, ALGORITHM).await.unwrap(),
            "Stop must drain before restart"
        );
        cleanup_owned(&pool, &lease, ALGORITHM).await.unwrap();
        assert!(db::start(&pool, OWNER, ALGORITHM).await.unwrap());
        activate_after_retirement(&pool, &old, INSTRUMENT, frontier + 600).await;
        // Durable slot-level signal dedup survives retirement of detector cursors.
        let new = db::active_adaptive_slots(&pool, OWNER)
            .await
            .unwrap()
            .remove(0);
        let previous = db::get_adaptive_checkpoint(&pool, new.slot_id, INSTRUMENT)
            .await
            .unwrap()
            .unwrap();
        let duplicate = db::commit_adaptive_scan_cas(
            &pool,
            &new,
            INSTRUMENT,
            Some(&previous),
            &promotion_state(new.generation_id, frontier + 660),
            &[adaptive_write(frontier - 60)],
        )
        .await
        .unwrap();
        assert!(duplicate.applied && duplicate.inserted.is_empty());
        assert_eq!(retained_history(&pool, &old).await, history);
    })
    .await;
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
        let (old, frontier) = retirement_fixture(&pool).await;
        let history = retained_history(&pool, &old).await;
        let previous = db::get_adaptive_checkpoint(&pool, old.slot_id, INSTRUMENT)
            .await
            .unwrap();
        let lease = scheduler::claim_owner(&pool, "retirement-nonpass")
            .await
            .unwrap()
            .unwrap();
        let failed = scheduler::prepare_next(&pool, &lease)
            .await
            .unwrap()
            .unwrap();
        scheduler::fail(&pool, &failed, &json!({"code":"technical_failure"}))
            .await
            .unwrap();
        assert!(db::start(&pool, OWNER, ALGORITHM).await.unwrap());
        let mut permission = config(ALGORITHM);
        permission["adaptiveEnabled"] = json!(false);
        assert!(
            db::configure(&pool, OWNER, ALGORITHM, &permission, &[4.0])
                .await
                .unwrap()
        );
        assert_eq!(
            db::get_adaptive_checkpoint(&pool, old.slot_id, INSTRUMENT)
                .await
                .unwrap(),
            previous
        );
        permission["adaptiveEnabled"] = json!(true);
        assert!(
            db::configure(&pool, OWNER, ALGORITHM, &permission, &[4.0])
                .await
                .unwrap()
        );
        assert_eq!(
            db::active_adaptive_slots(&pool, OWNER).await.unwrap()[0].generation_id,
            old.generation_id
        );
        let nonpass = scheduler::prepare_next(&pool, &lease)
            .await
            .unwrap()
            .unwrap();
        scheduler::finish_search(&pool, &nonpass, &json!({"best":null,"progress":{}}), true)
            .await
            .unwrap();
        assert!(
            scheduler::finalize(
                &pool,
                &nonpass,
                &json!({"best":null,"reason":"no winner"}),
                false,
                None,
                None
            )
            .await
            .unwrap()
        );
        activate_after_retirement(&pool, &old, INSTRUMENT, frontier + 600).await;
        assert_eq!(retained_history(&pool, &old).await, history);
        assert_eq!(
            optimization::get(&pool, nonpass.study_id)
                .await
                .unwrap()
                .unwrap()
                .status,
            "completed"
        );
    })
    .await;
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
        let pending = pending_fixture(&pool, &old).await;
        assert_eq!(
            db::active_adaptive_slots(&pool, OWNER).await.unwrap()[0].generation_id,
            old.generation_id
        );
        let fence = claim_pending_warmup(&pool, &pending, "warmup-a")
            .await
            .unwrap()
            .unwrap();
        assert!(
            claim_pending_warmup(&pool, &pending, "warmup-b")
                .await
                .unwrap()
                .is_none()
        );
        let initial = vec![InitialAdaptiveCheckpoint {
            instrument_key: INSTRUMENT.into(),
            state: promotion_state(pending.profile.generation_id, CUTOFF),
        }];
        assert!(
            report_warmup(&pool, &fence, "ready", Some(CUTOFF), None)
                .await
                .unwrap()
        );
        assert!(
            promote_pending(
                &pool,
                &fence,
                Some(old.generation_id),
                &[],
                CUTOFF,
                &initial
            )
            .await
            .is_err(),
            "absent scoped cursor must be explicitly included"
        );
        let absent = vec![CheckpointExpectation {
            instrument_key: INSTRUMENT.into(),
            checkpoint: None,
        }];
        // Old commit wins while the candidate warms. A captured absent cursor is now stale.
        let old_state = promotion_state(old.generation_id, CUTOFF);
        assert!(
            db::commit_adaptive_scan_cas(&pool, &old, INSTRUMENT, None, &old_state, &[])
                .await
                .unwrap()
                .applied
        );
        assert!(
            !promote_pending(
                &pool,
                &fence,
                Some(old.generation_id),
                &absent,
                CUTOFF,
                &initial
            )
            .await
            .unwrap()
        );
        let previous = db::get_adaptive_checkpoint(&pool, old.slot_id, INSTRUMENT)
            .await
            .unwrap()
            .unwrap();
        let expected = vec![CheckpointExpectation {
            instrument_key: INSTRUMENT.into(),
            checkpoint: Some(previous.clone()),
        }];
        assert!(
            promote_pending(
                &pool,
                &fence,
                Some(old.generation_id),
                &expected,
                CUTOFF,
                &initial
            )
            .await
            .unwrap()
        );
        let restored = db::active_adaptive_slots(&pool, OWNER)
            .await
            .unwrap()
            .remove(0);
        assert_eq!(restored.generation_id, pending.profile.generation_id);
        assert!(
            db::live::pending_adaptive_slots(&pool, OWNER)
                .await
                .unwrap()
                .is_empty()
        );
        let cursor = db::get_adaptive_checkpoint(&pool, old.slot_id, INSTRUMENT)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(cursor.generation_id, restored.generation_id);
        assert_eq!(cursor.state, initial[0].state);
        // Promotion wins first: the old detector cannot commit after it, nor can a
        // retry after a lost response overwrite the new durable cursor.
        assert!(
            !db::commit_adaptive_scan_cas(
                &pool,
                &old,
                INSTRUMENT,
                Some(&previous),
                &promotion_state(old.generation_id, CUTOFF + 60),
                &[]
            )
            .await
            .unwrap()
            .applied
        );
        assert!(
            !promote_pending(
                &pool,
                &fence,
                Some(old.generation_id),
                &expected,
                CUTOFF,
                &initial
            )
            .await
            .unwrap()
        );
        assert_eq!(
            db::get_adaptive_checkpoint(&pool, old.slot_id, INSTRUMENT)
                .await
                .unwrap()
                .unwrap(),
            cursor
        );
    })
    .await;
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
        setups_db::settings_set(&pool, "notifications", &json!({"maxAgeSeconds":3600}))
            .await
            .unwrap();
        let now = chrono::Utc::now().timestamp().div_euclid(60) * 60;
        let setup = adaptive_write(now - 60).setup;
        assert!(
            commit_fixed_setup(&pool, "bybit", "linear", "BTCUSDT", &setup)
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            commit_fixed_setup(&pool, "bybit", "linear", "BTCUSDT", &setup)
                .await
                .unwrap()
                .is_none()
        );
        let job = claim_notifications(&pool, "fixed-delivery", 1)
            .await
            .unwrap()
            .remove(0);
        assert!(job.slot_id.is_none() && job.fixed_config.is_some());
        assert_eq!(job.payload["externalName"], "accepted external");
        assert!(
            notification_permitted(&pool, "fixed-delivery", &job)
                .await
                .unwrap()
        );
        setups_db::settings_set(
            &pool,
            &format!("algorithm:{ALGORITHM}"),
            &json!({"enabled":true,"params":{"windowMinutes":10},"externalName":"changed"}),
        )
        .await
        .unwrap();
        assert!(
            !notification_permitted(&pool, "fixed-delivery", &job)
                .await
                .unwrap()
        );
        assert!(
            commit_fixed_setup(
                &pool,
                "bybit",
                "linear",
                "BTCUSDT",
                &adaptive_write(now - 120).setup
            )
            .await
            .unwrap()
            .is_none()
        );
        setups_db::settings_set(
            &pool,
            &format!("algorithm:{ALGORITHM}"),
            &json!({"enabled":true,"externalName":"changed"}),
        )
        .await
        .unwrap();
        assert!(
            notification_permitted(&pool, "fixed-delivery", &job)
                .await
                .unwrap()
        );
        setups_db::settings_set(&pool, "live", &json!({"enabled":false}))
            .await
            .unwrap();
        assert!(
            !notification_permitted(&pool, "fixed-delivery", &job)
                .await
                .unwrap()
        );
        assert!(
            finish_notification(&pool, "fixed-delivery", &job, NotificationOutcome::Skipped)
                .await
                .unwrap()
        );
        assert!(
            setups_db::runs::insert_live_setup(
                &pool,
                "bybit",
                "linear",
                "BTCUSDT",
                &adaptive_write(now - 180).setup
            )
            .await
            .unwrap()
            .is_some()
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("select count(*) from adaptive_notification_outbox")
                .fetch_one(&pool)
                .await
                .unwrap(),
            0
        );
    })
    .await;
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
