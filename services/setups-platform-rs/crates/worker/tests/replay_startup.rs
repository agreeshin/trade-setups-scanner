//! Startup recovery owns manual rows only. Every case uses a disposable child
//! database; the supplied *_test database is only its administrative connection.
use serde_json::{Value, json};
use setups_domain::{SPIKE_ALGORITHM_ID, SPIKE_ALGORITHM_VERSION, SpikeParams};
use setups_worker::start_replay_jobs;
use sqlx::{
    Connection, PgConnection, PgPool,
    postgres::{PgConnectOptions, PgPoolOptions},
};
use std::{str::FromStr, time::Duration};

const STATUSES: [&str; 6] = [
    "queued",
    "running",
    "cancel_requested",
    "done",
    "failed",
    "cancelled",
];

struct Database {
    pool: PgPool,
    admin: PgConnection,
    name: String,
}

impl Database {
    async fn new(workers: usize) -> Self {
        let url = std::env::var("TEST_DATABASE_URL").expect("isolated TEST_DATABASE_URL required");
        let options = PgConnectOptions::from_str(&url).unwrap();
        assert!(
            options
                .get_database()
                .is_some_and(|name| name.ends_with("_test")),
            "refusing a database URL not ending in _test"
        );
        let name = format!("replay_startup_{workers}_{}_test", std::process::id());
        let mut admin = PgConnection::connect_with(&options).await.unwrap();
        sqlx::query(&format!("CREATE DATABASE {name}"))
            .execute(&mut admin)
            .await
            .unwrap();
        let pool = match PgPoolOptions::new()
            .max_connections(2)
            .acquire_timeout(Duration::from_secs(5))
            .connect_with(options.database(&name))
            .await
        {
            Ok(pool) => pool,
            Err(error) => {
                sqlx::query(&format!("DROP DATABASE {name} WITH (FORCE)"))
                    .execute(&mut admin)
                    .await
                    .unwrap();
                panic!("could not connect to isolated startup database: {error}");
            }
        };
        Self { pool, admin, name }
    }

    async fn close(mut self) {
        self.pool.close().await;
        sqlx::query(&format!("DROP DATABASE {} WITH (FORCE)", self.name))
            .execute(&mut self.admin)
            .await
            .unwrap();
    }
}

async fn initialize(pool: &PgPool) {
    // The canonical baseline is a pg_dump script, so remove psql-only commands
    // before exercising the real additive migration runner in this child DB.
    let baseline = include_str!("../../db/migrations/0010_baseline.sql")
        .lines()
        .filter(|line| !line.starts_with('\\') && !line.starts_with("SET transaction_timeout"))
        .collect::<Vec<_>>()
        .join("\n");
    let mut connection = pool.acquire().await.unwrap();
    sqlx::raw_sql(&baseline)
        .execute(&mut *connection)
        .await
        .unwrap();
    sqlx::raw_sql("set search_path=public; create table kysely_migration(name varchar(255) primary key,timestamp varchar(255) not null); insert into kysely_migration values('0010_baseline.sql','fixture');")
        .execute(&mut *connection).await.unwrap();
    drop(connection);
    setups_db::migrate::ensure_migrations(pool).await.unwrap();

    // Pause execution only, NOT startup recovery: no market-data requests or
    // actual replay computations can escape this fixture.
    setups_db::settings_set(pool, "replay", &json!({"paused":true,"sentinel":"keep"}))
        .await
        .unwrap();
    let params = json!(SpikeParams::default());
    for owner in ["admin", "other-owner"] {
        sqlx::query("insert into continuous_owner_lanes(owner_id,lease_owner,lease_token) values($1,'startup-sentinel',41)")
            .bind(owner).execute(pool).await.unwrap();
        sqlx::query("insert into continuous_processes(owner_id,algorithm_id,status,desired_running,config_revision,config,phase) values($1,$2,'running',true,7,$3,'searching')")
            .bind(owner).bind(SPIKE_ALGORITHM_ID).bind(json!({"sentinel":owner})).execute(pool).await.unwrap();
        let slot: i64 = sqlx::query_scalar("insert into continuous_slots(owner_id,algorithm_id,min_setups_per_day) values($1,$2,1) returning id")
            .bind(owner).bind(SPIKE_ALGORITHM_ID).fetch_one(pool).await.unwrap();
        let generation: i64 = sqlx::query_scalar("insert into continuous_generations(owner_id,algorithm_id,slot_id,config_revision,lease_token,phase,spec,execution_started_at,observation_cutoff,replay_manifest) values($1,$2,$3,7,41,'searching',$4,to_timestamp(946684817),946684800,$5) returning id")
            .bind(owner).bind(SPIKE_ALGORITHM_ID).bind(slot).bind(json!({"sentinel":owner}))
            .bind(json!({"sentinel":"frozen replay manifest"})).fetch_one(pool).await.unwrap();
        sqlx::query("update continuous_processes set active_generation_id=$2 where owner_id=$1")
            .bind(owner)
            .bind(generation)
            .execute(pool)
            .await
            .unwrap();
        for origin in ["manual", "continuous"] {
            let generation = (origin == "continuous").then_some(generation);
            let batch: i32 = sqlx::query_scalar("insert into replay_batches(exchange_code,category,algorithm_id,algorithm_version,params,from_ts,to_ts,instrument_count,origin,owner_id,continuous_generation_id,name) values('fixture','linear',$1,$2,$3,to_timestamp(946684800),to_timestamp(946684860),6,$4,$5,$6,'startup batch sentinel') returning id")
                .bind(SPIKE_ALGORITHM_ID).bind(SPIKE_ALGORITHM_VERSION).bind(&params).bind(origin)
                .bind(owner).bind(generation).fetch_one(pool).await.unwrap();
            for status in STATUSES {
                let sentinel = format!("{owner}/{origin}/{status}");
                let id: i32 = sqlx::query_scalar("insert into replay_runs(exchange_code,category,symbol,from_ts,to_ts,algorithm_id,algorithm_version,params,status,cache_hit,setups,warmup_minutes,processed_minutes,error,created_at,updated_at,quality,quality_computed_at,batch_id,origin,owner_id,continuous_generation_id,name) values('fixture','linear',$1,to_timestamp(946684800),to_timestamp(946684860),$2,$3,$4,$5,true,$6,123,456,$1,to_timestamp(946684800),to_timestamp(946684800),$7,to_timestamp(946684800),$8,$9,$10,$11,'startup run sentinel') returning id")
                    .bind(&sentinel).bind(SPIKE_ALGORITHM_ID).bind(SPIKE_ALGORITHM_VERSION).bind(&params).bind(status)
                    .bind(json!([{"sentinel":sentinel,"price":"00100.0000","nested":[null,0,"keep"]}]))
                    .bind(json!({"sentinel":sentinel,"missingMinutes":13})).bind(batch).bind(origin).bind(owner).bind(generation)
                    .fetch_one(pool).await.unwrap();
                sqlx::query("update replay_runs set quality_source_run_id=id where id=$1")
                    .bind(id)
                    .execute(pool)
                    .await
                    .unwrap();
                sqlx::query("insert into replay_quality_intervals(run_id,from_unix,to_unix,reason) values($1,946684800,946684860,'startup quality sentinel')")
                    .bind(id).execute(pool).await.unwrap();
            }
        }
    }
}

struct Snapshot {
    runs: Vec<String>,
    sentinels: String,
}

async fn snapshot(pool: &PgPool) -> Snapshot {
    // PostgreSQL's canonical JSONB text includes every column, including source
    // linkage, timestamps and opaque payloads: unchanged means byte-identical.
    let runs = sqlx::query_scalar("select to_jsonb(r)::text from replay_runs r order by id")
        .fetch_all(pool)
        .await
        .unwrap();
    let sentinels = sqlx::query_scalar("select jsonb_build_object('batches',(select jsonb_agg(to_jsonb(b) order by id) from replay_batches b),'quality',(select jsonb_agg(to_jsonb(q) order by run_id,from_unix) from replay_quality_intervals q),'settings',(select jsonb_agg(to_jsonb(s) order by key) from settings s),'lanes',(select jsonb_agg(to_jsonb(l) order by owner_id) from continuous_owner_lanes l),'processes',(select jsonb_agg(to_jsonb(p) order by owner_id,algorithm_id) from continuous_processes p),'slots',(select jsonb_agg(to_jsonb(s) order by id) from continuous_slots s),'generations',(select jsonb_agg(to_jsonb(g) order by id) from continuous_generations g))::text")
        .fetch_one(pool).await.unwrap();
    Snapshot { runs, sentinels }
}

async fn startup_case(pool: &PgPool, workers: usize) {
    initialize(pool).await;
    let before = snapshot(pool).await;
    let jobs = start_replay_jobs(
        pool.clone(),
        "http://127.0.0.1:1".into(),
        "startup-fixture".into(),
        workers,
        Duration::from_millis(10),
    )
    .await
    .unwrap();
    // Stop before assertions so an assertion panic cannot detach a poller that
    // retains the child pool and prevents deterministic fixture cleanup.
    jobs.stop().await;
    let after = snapshot(pool).await;
    assert_eq!(before.runs.len(), 24);
    assert_eq!(after.runs.len(), before.runs.len());
    assert_eq!(after.sentinels, before.sentinels);
    for (before, after) in before.runs.iter().zip(&after.runs) {
        let original: Value = serde_json::from_str(before).unwrap();
        let status = original["status"].as_str().unwrap();
        let description = format!(
            "workers={workers}, owner={}, origin={}, status={status}, id={}",
            original["owner_id"], original["origin"], original["id"]
        );
        if workers == 0
            || original["origin"] == "continuous"
            || !["queued", "running", "cancel_requested"].contains(&status)
        {
            assert_eq!(after, before, "protected row changed: {description}");
            continue;
        }
        let actual: Value = serde_json::from_str(after).unwrap();
        let mut expected = original.clone();
        expected["status"] = json!(if status == "running" {
            "failed"
        } else {
            "cancelled"
        });
        if status == "running" {
            expected["error"] = json!("interrupted by application restart");
        }
        assert_ne!(
            actual["updated_at"], original["updated_at"],
            "{description}"
        );
        expected["updated_at"] = actual["updated_at"].clone();
        assert_eq!(actual, expected, "manual recovery changed: {description}");
    }
}

async fn isolated(workers: usize) {
    let db = Database::new(workers).await;
    let pool = db.pool.clone();
    // Cleanup also runs when schema setup or a regression assertion panics.
    let local = tokio::task::LocalSet::new();
    let result = local
        .run_until(async move {
            tokio::task::spawn_local(async move { startup_case(&pool, workers).await }).await
        })
        .await;
    db.close().await;
    if let Err(error) = result {
        if error.is_panic() {
            std::panic::resume_unwind(error.into_panic());
        }
        panic!("isolated startup test was cancelled: {error}");
    }
}

#[tokio::test]
#[ignore = "requires isolated TEST_DATABASE_URL ending in _test with CREATEDB"]
async fn one_worker_recovers_only_manual_rows_and_preserves_continuous_sentinels() {
    isolated(1).await;
}

#[tokio::test]
#[ignore = "requires isolated TEST_DATABASE_URL ending in _test with CREATEDB"]
async fn zero_workers_preserves_all_manual_and_continuous_rows_byte_for_byte() {
    isolated(0).await;
}
