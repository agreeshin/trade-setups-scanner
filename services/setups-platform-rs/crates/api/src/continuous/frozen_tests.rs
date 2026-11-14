//! Focused frozen storage/codec tests; only isolated child databases are mutated.
use super::*;
use setups_analytics::OhlcCandle;
use setups_db::admission::{
    self as admission, AdmissionInstrument, ReplayAdmissionInput, ReplayAdmissionLimits,
};
use setups_db::continuous::{self as db, recovery, scheduler};
use setups_domain::DetectorBar;
use setups_optimizer::{InstrumentInput, StudyInstrument};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use std::{future::Future, str::FromStr, time::Duration};

const SNAPSHOT: usize = 16 * storage::CHUNK_BYTES;
const RUNTIME: i64 = 256 * 1024 * 1024;
const DATABASE: i64 = 1024 * 1024 * 1024;
const ALGORITHM: &str = "price_volume_spike";

fn replay_limits() -> ReplayAdmissionLimits {
    ReplayAdmissionLimits {
        max_database_bytes: DATABASE,
        ..Default::default()
    }
}

#[test]
fn database_converter_preserves_transport_uncertainty_but_not_corrupt_state() {
    let transport = setups_db::DbError::Sqlx(sqlx::Error::Io(io::Error::new(
        io::ErrorKind::ConnectionReset,
        "frozen test connection reset",
    )));
    assert_eq!(database(transport).code, "persistence_unconfirmed");
    assert_eq!(
        database(setups_db::DbError::Sqlx(sqlx::Error::PoolTimedOut)).code,
        "persistence_unconfirmed"
    );
    assert_eq!(
        database(setups_db::DbError::Json("frozen input checksum corrupt".into())).code,
        "frozen_input_storage"
    );
}

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
            "refusing non-test database"
        );
        let admin = PgPoolOptions::new()
            .max_connections(2)
            .connect_with(options.clone())
            .await
            .unwrap();
        let name = format!(
            "frozen_{}_{}_test",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap()
        );
        assert!(name.len() < 63 && name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_'));
        sqlx::query(&format!("create database {name}"))
            .execute(&admin)
            .await
            .unwrap();
        let pool = PgPoolOptions::new()
            .max_connections(6)
            .connect_with(options.database(&name))
            .await
            .unwrap();
        Self { admin, pool, name }
    }
    async fn cleanup(self) {
        self.pool.close().await;
        sqlx::query(&format!("drop database {} with(force)", self.name))
            .execute(&self.admin)
            .await
            .unwrap();
        let remains: bool =
            sqlx::query_scalar("select exists(select 1 from pg_database where datname=$1)")
                .bind(&self.name)
                .fetch_one(&self.admin)
                .await
                .unwrap();
        assert!(!remains);
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
    // sqlx raw migration futures need not be Send; keep assertion capture and
    // deterministic cleanup without forcing them through tokio::spawn.
    let local = tokio::task::LocalSet::new();
    let result = local.run_until(async move { tokio::task::spawn_local(async move {
        // Apply schema inside the joined task, so a failed migration also cleans up.
        let baseline = include_str!("../../../db/migrations/0010_baseline.sql").lines()
            .filter(|line| !line.starts_with('\\') && !line.starts_with("SET transaction_timeout") && !line.contains("set_config('search_path'"))
            .collect::<Vec<_>>().join("\n");
        sqlx::raw_sql(&baseline).execute(&pool).await.unwrap();
        sqlx::raw_sql("set search_path to public; create table kysely_migration(name varchar(255) primary key,timestamp varchar(255) not null); insert into kysely_migration values('0009_legacy','fixture')")
            .execute(&pool).await.unwrap();
        setups_db::migrate::ensure_migrations(&pool).await.unwrap();
        body(pool).await;
    }).await }).await;
    db.cleanup().await;
    if let Err(error) = result {
        std::panic::resume_unwind(error.into_panic());
    }
}
fn config() -> Value {
    json!({"lookbackDays":1,"minSetupsPerDay":[0],"minMeanExpectancy":0.15,"adaptiveEnabled":true,"study":{
        "algorithmId":ALGORITHM,"algorithmVersion":"1.1.1","algorithmParams":setups_domain::SpikeParams::default(),
        "algorithmSearch":{},"analyticsParams":{"clusterEnabled":false,"clusterGapMinutes":480,"timeShiftMinutes":0,"priceChangePct":null},
        "analyticsSearch":{},"instruments":[{"exchange":"bybit","category":"linear","symbol":"BTCUSDT"}],
        "direction":"short","tpLevels":[1],"slLevels":[1],"horizonMinutes":2,"minClosedPerCell":1,"maxTrials":1,"maxSeconds":3600,"seed":42}})
}
async fn job(pool: &PgPool, owner: &str) -> ChainJob {
    assert!(
        db::configure(pool, owner, ALGORITHM, &config(), &[0.0])
            .await
            .unwrap()
    );
    assert!(db::start(pool, owner, ALGORITHM).await.unwrap());
    let lease = scheduler::claim_owner(pool, &format!("frozen-{owner}"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(lease.owner_id, owner);
    scheduler::prepare_next(pool, &lease)
        .await
        .unwrap()
        .unwrap()
}
fn input(rows: usize) -> StudyInput {
    let mut detector_bars = Vec::with_capacity(rows);
    let mut candles = Vec::with_capacity(rows);
    for index in 0..rows {
        // Deliberate minute gaps; do not fill them on restore.
        let time = index as i64 * 120;
        detector_bars.push(DetectorBar {
            open_time_unix: time,
            close: "100.00000000000000000001".into(),
            quote_turnover: "0001.2300".into(),
        });
        candles.push(OhlcCandle {
            open_time_unix: time,
            open: f64::from_bits(0x3ff0_0000_0000_0001),
            high: f64::MAX,
            low: -0.0,
            close: f64::from_bits(1),
        });
    }
    StudyInput {
        instruments: vec![
            InstrumentInput {
                instrument: StudyInstrument {
                    exchange: "bybit".into(),
                    category: "linear".into(),
                    symbol: "BTCUSDT".into(),
                },
                detector_bars,
                candles,
            },
            InstrumentInput {
                instrument: StudyInstrument {
                    exchange: "bybit".into(),
                    category: "linear".into(),
                    symbol: "EMPTYUSDT".into(),
                },
                detector_bars: vec![],
                candles: vec![],
            },
        ],
        snapshot_hash: "original-acquisition-hash".into(),
        captured_at: "2026-11-14T00:00:00Z".into(),
    }
}
fn exact(left: &StudyInput, right: &StudyInput) {
    assert_eq!(left.snapshot_hash, right.snapshot_hash);
    assert_eq!(left.captured_at, right.captured_at);
    assert_eq!(left.instruments.len(), right.instruments.len());
    for (left, right) in left.instruments.iter().zip(&right.instruments) {
        assert_eq!(left.instrument, right.instrument);
        assert_eq!(left.detector_bars, right.detector_bars);
        assert_eq!(left.candles.len(), right.candles.len());
        for (left, right) in left.candles.iter().zip(&right.candles) {
            assert_eq!(left.open_time_unix, right.open_time_unix);
            assert_eq!(
                [
                    left.open.to_bits(),
                    left.high.to_bits(),
                    left.low.to_bits(),
                    left.close.to_bits()
                ],
                [
                    right.open.to_bits(),
                    right.high.to_bits(),
                    right.low.to_bits(),
                    right.close.to_bits()
                ]
            );
        }
    }
}
async fn save(pool: &PgPool, job: &ChainJob, input: Arc<StudyInput>) {
    persist(
        pool,
        job,
        input,
        SNAPSHOT,
        RUNTIME,
        DATABASE,
        &replay_limits(),
        &CancellationToken::new(),
    )
    .await
    .unwrap();
}

#[test]
fn canonical_objects_preserve_array_order_and_numeric_values() {
    let first: Value = serde_json::from_str(r#"{"z":[{"b":2,"a":1},3],"a":-0.0}"#).unwrap();
    let second: Value = serde_json::from_str(r#"{"a":-0.0,"z":[{"a":1,"b":2},3]}"#).unwrap();
    assert_eq!(json_hash(&first), json_hash(&second));
    assert_eq!(
        serde_json::to_string(&canonical(&first)).unwrap(),
        serde_json::to_string(&canonical(&second)).unwrap()
    );
    let mut reversed = first.clone();
    reversed["z"].as_array_mut().unwrap().reverse();
    assert_ne!(json_hash(&first), json_hash(&reversed));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires isolated TEST_DATABASE_URL ending in _test with CREATEDB"]
async fn permission_only_toggle_under_drain_keeps_research_identity_and_restores_actual_database_job()
 {
    isolated(|pool| async move {
        let job = job(&pool, "admin").await;
        let original = Arc::new(input(4));
        save(&pool, &job, original.clone()).await;
        let identity_before = identity(&job);
        let mut toggled = job.clone(); toggled.config["adaptiveEnabled"] = false.into();
        assert_eq!(identity(&toggled), identity_before);
        for field in ["lookbackDays", "minMeanExpectancy", "minSetupsPerDay", "study", "someFutureResearchCoordinate"] {
            let mut research_change = toggled.clone(); research_change.config[field] = "changed".into();
            assert_ne!(identity(&research_change), identity_before, "only adaptiveEnabled is excluded: {field}");
        }
        let drained = db::maintenance::set_paused(&pool, &job.lease.owner_id, true).await.unwrap();
        assert_eq!(drained.state, "draining");
        assert!(db::configure(&pool, &job.lease.owner_id, ALGORITHM, &toggled.config, &[0.0]).await.unwrap());
        let restored_job = recovery::active_job(&pool, &job.lease).await.unwrap().unwrap();
        assert_eq!(restored_job.generation_id, job.generation_id);
        assert_eq!(restored_job.study_id, job.study_id);
        assert_eq!(restored_job.config_revision, job.config_revision);
        assert_eq!(identity(&restored_job), identity_before, "database timestamp precision and live-only permission changes must both survive recovery");
        exact(&original, &load(&pool, &restored_job, SNAPSHOT, &CancellationToken::new()).await.unwrap());
        assert_eq!(db::maintenance::status(&pool, &job.lease.owner_id).await.unwrap().state, "draining");
    }).await;
}

fn manual_request() -> ReplayAdmissionInput {
    ReplayAdmissionInput {
        instruments: vec![AdmissionInstrument {
            exchange: "bybit".into(),
            category: "linear".into(),
            symbol: "MANUALUSDT".into(),
        }],
        from_unix: 0,
        to_unix: 3600,
        params: setups_domain::AlgorithmParams::default(),
    }
}
fn manual_reservation(request: &ReplayAdmissionInput, limits: &ReplayAdmissionLimits) -> i64 {
    admission::replay_work_minutes(request.from_unix, request.to_unix, &request.params)
        * limits.estimated_bytes_per_work_minute
        + 4096
}
async fn physical_bytes(pool: &PgPool) -> i64 {
    sqlx::query_scalar("select pg_database_size(current_database())")
        .fetch_one(pool)
        .await
        .unwrap()
}
async fn history_sentinel(pool: &PgPool) -> i32 {
    sqlx::query_scalar("insert into replay_runs(exchange_code,category,symbol,from_ts,to_ts,algorithm_id,algorithm_version,params,status,setups) values('bybit','linear','HISTORYUSDT',to_timestamp(0),to_timestamp(60),'price_volume_spike','1.1.1',$1,'done',$2) returning id")
        .bind(json!(setups_domain::AlgorithmParams::default())).bind(json!([{"history":"unchanged"}])).fetch_one(pool).await.unwrap()
}
async fn assert_history(pool: &PgPool, id: i32) {
    let row: (String, Value) = sqlx::query_as("select status,setups from replay_runs where id=$1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(row, ("done".into(), json!([{"history":"unchanged"}])));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires isolated TEST_DATABASE_URL ending in _test with CREATEDB"]
async fn manual_then_frozen_shared_quota_counts_actual_manual_policy_and_continuous_reservation_once()
 {
    isolated(|pool| async move {
        let job = job(&pool, "admin").await;
        let history = history_sentinel(&pool).await;
        // Deliberately non-default policy: substituting ReplayAdmissionLimits::default
        // would admit a snapshot that overbooks this already accepted manual run.
        let policy = ReplayAdmissionLimits {
            estimated_bytes_per_work_minute: 2048,
            ..replay_limits()
        };
        let request = manual_request();
        let runs = admission::enqueue_replay_runs(&pool, &request, &policy)
            .await
            .unwrap();
        let continuous = 7 * 1024 * 1024;
        assert!(
            scheduler::reserve_storage(&pool, &job, continuous, DATABASE, &policy)
                .await
                .unwrap()
                .unwrap()
                .admitted
        );
        let private = storage::CHUNK_BYTES as i64 + storage::STORAGE_HEADROOM;
        let tight = physical_bytes(&pool).await
            + manual_reservation(&request, &policy)
            + continuous
            + private / 2;
        assert!(
            storage::reserve(
                &pool,
                &job,
                &identity(&job),
                storage::CHUNK_BYTES,
                RUNTIME,
                tight,
                &policy
            )
            .await
            .is_err()
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("select count(*) from continuous_runtime_artifacts")
                .fetch_one(&pool)
                .await
                .unwrap(),
            0
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "select reserved_bytes from continuous_generations where id=$1"
            )
            .bind(job.generation_id)
            .fetch_one(&pool)
            .await
            .unwrap(),
            continuous
        );
        assert_eq!(
            sqlx::query_scalar::<_, String>("select status from replay_runs where id=$1")
                .bind(runs[0].id as i32)
                .fetch_one(&pool)
                .await
                .unwrap(),
            "queued"
        );
        assert_history(&pool, history).await;

        // Removing only that known reservation makes the same quota sufficient;
        // no history deletion or release of another lane happens on rejection.
        sqlx::query("update replay_runs set status='cancelled' where id=$1")
            .bind(runs[0].id as i32)
            .execute(&pool)
            .await
            .unwrap();
        let no_double_charge = physical_bytes(&pool).await + continuous + private + 256 * 1024;
        storage::reserve(
            &pool,
            &job,
            &identity(&job),
            storage::CHUNK_BYTES,
            RUNTIME,
            no_double_charge,
            &policy,
        )
        .await
        .unwrap();
        assert_history(&pool, history).await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires isolated TEST_DATABASE_URL ending in _test with CREATEDB"]
async fn frozen_then_manual_and_continuous_admission_reject_without_mutating_prior_reservations() {
    isolated(|pool| async move {
        let job = job(&pool, "admin").await;
        let history = history_sentinel(&pool).await;
        let policy = ReplayAdmissionLimits { estimated_bytes_per_work_minute: 2048, ..replay_limits() };
        storage::reserve(&pool, &job, &identity(&job), storage::CHUNK_BYTES, RUNTIME, DATABASE, &policy).await.unwrap();
        let prior: (Value, i64, i64) = sqlx::query_as("select identity,reserved_bytes,snapshot_bytes from continuous_runtime_artifacts where generation_id=$1").bind(job.generation_id).fetch_one(&pool).await.unwrap();
        let request = manual_request();
        let tight = ReplayAdmissionLimits { max_database_bytes: physical_bytes(&pool).await + manual_reservation(&request, &policy) + prior.1 / 2, ..policy };
        assert!(matches!(admission::enqueue_replay_runs(&pool, &request, &tight).await, Err(admission::QueueError::Admission(error)) if error.resource==admission::AdmissionResource::DatabaseBytes));
        assert_eq!(sqlx::query_scalar::<_,i64>("select count(*) from replay_runs where status='queued'").fetch_one(&pool).await.unwrap(), 0);
        let wanted = 8 * 1024 * 1024;
        let tight_continuous = physical_bytes(&pool).await + prior.1 + wanted / 2;
        assert!(!scheduler::reserve_storage(&pool, &job, wanted, tight_continuous, &policy).await.unwrap().unwrap().admitted);
        assert_eq!(sqlx::query_scalar::<_,i64>("select reserved_bytes from continuous_generations where id=$1").bind(job.generation_id).fetch_one(&pool).await.unwrap(), 0);
        let after: (Value, i64, i64) = sqlx::query_as("select identity,reserved_bytes,snapshot_bytes from continuous_runtime_artifacts where generation_id=$1").bind(job.generation_id).fetch_one(&pool).await.unwrap();
        assert_eq!(prior, after);
        assert_history(&pool, history).await;
        let fits_once = ReplayAdmissionLimits { max_database_bytes: physical_bytes(&pool).await + prior.1 + manual_reservation(&request, &policy) + 256 * 1024, ..policy };
        assert_eq!(admission::enqueue_replay_runs(&pool, &request, &fits_once).await.unwrap().len(), 1);
        assert_history(&pool, history).await;
    }).await;
}

#[test]
fn representative_wire_sizes_use_a_separate_checked_encoded_budget() {
    let mixed = input(1000);
    let retained = core::snapshot_bytes(&mixed);
    let digest =
        core::write_frozen_input(&mixed, &mut io::sink(), &limits(retained, retained * 4)).unwrap();
    let mut candle_only = input(1000);
    candle_only.instruments[0].detector_bars.clear();
    candle_only.instruments[0].detector_bars.shrink_to_fit();
    let candle_retained = core::snapshot_bytes(&candle_only);
    let candle_digest = core::write_frozen_input(
        &candle_only,
        &mut io::sink(),
        &limits(candle_retained, candle_retained * 4),
    )
    .unwrap();
    println!(
        "frozen mixed: decoded={retained} encoded={} ratio={:.3}; candle-heavy: decoded={candle_retained} encoded={} ratio={:.3}",
        digest.encoded_bytes,
        digest.encoded_bytes as f64 / retained as f64,
        candle_digest.encoded_bytes,
        candle_digest.encoded_bytes as f64 / candle_retained as f64
    );
    assert!(
        digest.encoded_bytes > retained as u64,
        "fixture reproduces why RAM is not a wire byte budget"
    );
    assert!(digest.encoded_bytes <= encoded_cap(retained).unwrap() as u64);
    assert_eq!(limits(retained, retained * 2).max_decoded_bytes, retained);
    assert_eq!(
        persistence_cap(retained, storage::STORAGE_HEADROOM + 1234).unwrap(),
        1234
    );
    assert!(encoded_cap(usize::MAX).is_err());
    assert!(persistence_cap(retained, storage::STORAGE_HEADROOM).is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires isolated TEST_DATABASE_URL ending in _test with CREATEDB"]
async fn near_ram_cap_input_persists_with_independent_encoded_cap_and_unchanged_decoded_limit() {
    isolated(|pool| async move {
        let job = job(&pool, "admin").await;
        let original = Arc::new(input(1000));
        let retained = core::snapshot_bytes(&original);
        persist(
            &pool,
            &job,
            original.clone(),
            retained,
            RUNTIME,
            DATABASE,
            &replay_limits(),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        let encoded: i64 = sqlx::query_scalar(
            "select snapshot_bytes from continuous_runtime_artifacts where generation_id=$1",
        )
        .bind(job.generation_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(encoded as usize > retained);
        exact(
            &original,
            &load(&pool, &job, retained, &CancellationToken::new())
                .await
                .unwrap(),
        );
        let candle_job = self::job(&pool, "candles").await;
        let mut candle_input = input(1000);
        candle_input.instruments[0].detector_bars.clear();
        candle_input.instruments[0].detector_bars.shrink_to_fit();
        let candle_input = Arc::new(candle_input);
        let retained = core::snapshot_bytes(&candle_input);
        persist(
            &pool,
            &candle_job,
            candle_input.clone(),
            retained,
            RUNTIME,
            DATABASE,
            &replay_limits(),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        let encoded: i64 = sqlx::query_scalar(
            "select snapshot_bytes from continuous_runtime_artifacts where generation_id=$1",
        )
        .bind(candle_job.generation_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(
            encoded as usize > 2 * retained,
            "a two-times wire cap is insufficient for this admitted RAM input"
        );
        exact(
            &candle_input,
            &load(&pool, &candle_job, retained, &CancellationToken::new())
                .await
                .unwrap(),
        );
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires isolated TEST_DATABASE_URL ending in _test with CREATEDB"]
async fn exact_cbor_roundtrip_keeps_empty_scope_gaps_decimal_strings_and_f64_bits() {
    isolated(|pool| async move {
        let job = job(&pool, "admin").await;
        let original = Arc::new(input(3));
        save(&pool, &job, original.clone()).await;
        let restored = load(&pool, &job, SNAPSHOT, &CancellationToken::new())
            .await
            .unwrap();
        exact(&original, &restored);
        assert_eq!(
            Arc::strong_count(&original),
            1,
            "encoder releases its owned input before returning"
        );
        assert_eq!(
            load(&pool, &job, 1, &CancellationToken::new())
                .await
                .unwrap_err()
                .code,
            "frozen_input_storage"
        );
        let mut changed = job.clone();
        changed.lease.worker = "new-worker".into();
        changed.lease.token += 10;
        assert_eq!(
            identity(&job),
            identity(&changed),
            "lease identity is replaceable, computation identity is not"
        );
        changed = job.clone();
        changed.research_deadline = changed
            .research_deadline
            .map(|time| time + chrono::Duration::seconds(1));
        assert_ne!(identity(&job), identity(&changed));
        changed = job.clone();
        changed.spec["algorithmVersion"] = "new".into();
        assert_ne!(identity(&job), identity(&changed));
        let result: Option<Value> =
            sqlx::query_scalar("select result from optimization_studies where id=$1")
                .bind(job.study_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(
            result.is_none(),
            "frozen state never becomes public running StudyOutcome"
        );
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires isolated TEST_DATABASE_URL ending in _test with CREATEDB"]
async fn partial_replacement_and_lost_response_preserve_committed_head_checkpoint_and_gate() {
    isolated(|pool| async move {
        let job = job(&pool, "admin").await; let id = identity(&job); let original = Arc::new(input(7));
        assert!(storage::reserve(&pool, &job, &id, SNAPSHOT, RUNTIME, DATABASE, &replay_limits()).await.unwrap().is_none());
        storage::write_chunk(&pool, &job, &id, 0, b"partial old input", SNAPSHOT).await.unwrap();
        assert!(load(&pool, &job, SNAPSHOT, &CancellationToken::new()).await.is_err());
        save(&pool, &job, original.clone()).await;
        recovery::save_checkpoint(&pool, &job, &id, b"private checkpoint", RUNTIME).await.unwrap();
        let gate = recovery::Gate { result: json!({"frozenSentinel":true}), accepted: false, mean: None, selected: None };
        recovery::save_gate(&pool, &job, &id, &gate).await.unwrap();
        let before: (Value, Vec<u8>, i64, Value, i64) = sqlx::query_as("select snapshot_manifest,checkpoint,checkpoint_sequence,gate_result,reserved_bytes from continuous_runtime_artifacts where generation_id=$1")
            .bind(job.generation_id).fetch_one(&pool).await.unwrap();
        for _ in 0..3 { save(&pool, &job, original.clone()).await; }
        let after: (Value, Vec<u8>, i64, Value, i64) = sqlx::query_as("select snapshot_manifest,checkpoint,checkpoint_sequence,gate_result,reserved_bytes from continuous_runtime_artifacts where generation_id=$1")
            .bind(job.generation_id).fetch_one(&pool).await.unwrap();
        assert_eq!(before, after);
        let different = Arc::new(input(8));
        assert!(persist(&pool, &job, different, SNAPSHOT, RUNTIME, DATABASE, &replay_limits(), &CancellationToken::new()).await.is_err());
        exact(&original, &load(&pool, &job, SNAPSHOT, &CancellationToken::new()).await.unwrap());
        let bytes: i64 = sqlx::query_scalar("select snapshot_bytes from continuous_runtime_artifacts where generation_id=$1").bind(job.generation_id).fetch_one(&pool).await.unwrap();
        assert_eq!(after.4, bytes + storage::STORAGE_HEADROOM, "seal releases unused snapshot reservation");
        assert_eq!(sqlx::query_scalar::<_, i64>("select count(*) from continuous_frozen_chunks where generation_id=$1").bind(job.generation_id).fetch_one(&pool).await.unwrap(), 1);
    }).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires isolated TEST_DATABASE_URL ending in _test with CREATEDB"]
async fn committed_corruption_missing_chunks_and_versions_fail_without_replacement_or_network_fallback()
 {
    isolated(|pool| async move {
        let job = job(&pool, "admin").await; let original = Arc::new(input(3)); save(&pool, &job, original.clone()).await;
        let manifest: Value = sqlx::query_scalar("select snapshot_manifest from continuous_runtime_artifacts where generation_id=$1").bind(job.generation_id).fetch_one(&pool).await.unwrap();
        for changed in [json!({"version":2}), json!({"sha256":"f".repeat(64)})] {
            let mut broken = manifest.clone(); for (key, value) in changed.as_object().unwrap() { broken[key] = value.clone(); }
            sqlx::query("update continuous_runtime_artifacts set snapshot_manifest=$2 where generation_id=$1").bind(job.generation_id).bind(&broken).execute(&pool).await.unwrap();
            assert!(load(&pool, &job, SNAPSHOT, &CancellationToken::new()).await.is_err());
            assert!(persist(&pool, &job, original.clone(), SNAPSHOT, RUNTIME, DATABASE, &replay_limits(), &CancellationToken::new()).await.is_err());
            let retained: Value = sqlx::query_scalar("select snapshot_manifest from continuous_runtime_artifacts where generation_id=$1").bind(job.generation_id).fetch_one(&pool).await.unwrap(); assert_eq!(broken, retained);
        }
        sqlx::query("update continuous_runtime_artifacts set snapshot_manifest=$2 where generation_id=$1").bind(job.generation_id).bind(&manifest).execute(&pool).await.unwrap();
        sqlx::query("update continuous_frozen_chunks set checksum=$2 where generation_id=$1").bind(job.generation_id).bind("0".repeat(64)).execute(&pool).await.unwrap();
        assert!(load(&pool, &job, SNAPSHOT, &CancellationToken::new()).await.unwrap_err().message.contains("checksum"));
        sqlx::query("delete from continuous_frozen_chunks where generation_id=$1").bind(job.generation_id).execute(&pool).await.unwrap();
        assert!(load(&pool, &job, SNAPSHOT, &CancellationToken::new()).await.is_err());
        assert!(persist(&pool, &job, original, SNAPSHOT, RUNTIME, DATABASE, &replay_limits(), &CancellationToken::new()).await.is_err());
        assert_eq!(sqlx::query_scalar::<_, i64>("select count(*) from continuous_frozen_chunks").fetch_one(&pool).await.unwrap(), 0);
    }).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires isolated TEST_DATABASE_URL ending in _test with CREATEDB"]
async fn fences_reject_owner_revision_epoch_and_stop_but_compatible_new_epoch_restores_same_input()
{
    isolated(|pool| async move {
        let job = job(&pool, "admin").await;
        let original = Arc::new(input(2));
        save(&pool, &job, original.clone()).await;
        for field in 0..3 {
            let mut stale = job.clone();
            match field {
                0 => stale.lease.owner_id = "other".into(),
                1 => stale.config_revision += 1,
                _ => stale.lease.token += 1,
            }
            assert!(
                load(&pool, &stale, SNAPSHOT, &CancellationToken::new())
                    .await
                    .is_err()
            );
            assert!(
                persist(
                    &pool,
                    &stale,
                    original.clone(),
                    SNAPSHOT,
                    RUNTIME,
                    DATABASE,
                    &replay_limits(),
                    &CancellationToken::new()
                )
                .await
                .is_err()
            );
        }
        // Exercise an authorized new execution epoch, not a metadata-only
        // refence that could also belong to Stop/exhaustion housekeeping.
        scheduler::release(&pool, &job.lease).await.unwrap();
        let lease = scheduler::claim_owner(&pool, "compatible-frozen-recovery")
            .await.unwrap().unwrap();
        scheduler::recover_owner(&pool, &lease).await.unwrap();
        let resumed = recovery::active_job(&pool, &lease).await.unwrap().unwrap();
        assert!(resumed.lease.token > job.lease.token);
        assert_eq!(resumed.generation_id, job.generation_id);
        assert_eq!(resumed.study_id, job.study_id);
        assert_eq!(resumed.research_deadline, job.research_deadline);
        assert!(
            load(&pool, &job, SNAPSHOT, &CancellationToken::new())
                .await
                .is_err()
        );
        exact(
            &original,
            &load(&pool, &resumed, SNAPSHOT, &CancellationToken::new())
                .await
                .unwrap(),
        );
        sqlx::query("update continuous_processes set desired_running=false where owner_id=$1")
            .bind(&job.lease.owner_id)
            .execute(&pool)
            .await
            .unwrap();
        assert!(
            load(&pool, &resumed, SNAPSHOT, &CancellationToken::new())
                .await
                .is_err()
        );
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires isolated TEST_DATABASE_URL ending in _test with CREATEDB"]
async fn per_chain_aggregate_database_caps_and_short_chunk_order_are_enforced_transactionally() {
    isolated(|pool| async move {
        let first = job(&pool, "admin").await;
        let id = identity(&first);
        assert!(
            storage::reserve(
                &pool,
                &first,
                &id,
                SNAPSHOT,
                storage::STORAGE_HEADROOM,
                DATABASE,
                &replay_limits()
            )
            .await
            .is_err()
        );
        assert!(
            storage::reserve(&pool, &first, &id, SNAPSHOT, RUNTIME, 1, &replay_limits())
                .await
                .is_err()
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("select count(*) from continuous_runtime_artifacts")
                .fetch_one(&pool)
                .await
                .unwrap(),
            0
        );
        storage::reserve(
            &pool,
            &first,
            &id,
            SNAPSHOT,
            RUNTIME,
            DATABASE,
            &replay_limits(),
        )
        .await
        .unwrap();
        assert!(
            storage::write_chunk(
                &pool,
                &first,
                &id,
                0,
                &vec![1; storage::CHUNK_BYTES + 1],
                SNAPSHOT
            )
            .await
            .is_err()
        );
        assert!(
            storage::write_chunk(&pool, &first, &id, 1, b"out of order", SNAPSHOT)
                .await
                .is_err()
        );
        storage::write_chunk(&pool, &first, &id, 0, b"short final", SNAPSHOT)
            .await
            .unwrap();
        storage::write_chunk(&pool, &first, &id, 0, b"short final", SNAPSHOT)
            .await
            .unwrap();
        assert!(
            storage::write_chunk(&pool, &first, &id, 1, b"after short", SNAPSHOT)
                .await
                .is_err()
        );
        let actual: i64 = sqlx::query_scalar(
            "select snapshot_bytes from continuous_runtime_artifacts where generation_id=$1",
        )
        .bind(first.generation_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(actual, 11);
        let second = job(&pool, "second").await;
        let one_reservation = SNAPSHOT as i64 + storage::STORAGE_HEADROOM;
        assert!(
            storage::reserve(
                &pool,
                &second,
                &identity(&second),
                SNAPSHOT,
                one_reservation + 1,
                DATABASE,
                &replay_limits()
            )
            .await
            .is_err()
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("select count(*) from continuous_runtime_artifacts")
                .fetch_one(&pool)
                .await
                .unwrap(),
            1
        );
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires isolated TEST_DATABASE_URL ending in _test with CREATEDB"]
async fn cancellation_joins_blocked_codec_releases_input_and_replaces_only_partial_chunks() {
    isolated(|pool| async move {
        let job = job(&pool, "admin").await; let original = Arc::new(input(25000));
        sqlx::raw_sql("create function frozen_test_slow_chunk() returns trigger language plpgsql as $$ begin perform pg_sleep(0.025); return new; end $$; create trigger frozen_test_slow before insert on continuous_frozen_chunks for each row execute function frozen_test_slow_chunk()")
            .execute(&pool).await.unwrap();
        let cancel = CancellationToken::new(); let child = cancel.clone(); let p = pool.clone(); let j = job.clone(); let owned = original.clone();
        let task = tokio::spawn(async move { persist(&p, &j, owned, SNAPSHOT, RUNTIME, DATABASE, &replay_limits(), &child).await });
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let bytes: Option<i64> = sqlx::query_scalar("select snapshot_bytes from continuous_runtime_artifacts where generation_id=$1").bind(job.generation_id).fetch_optional(&pool).await.unwrap();
                if bytes.is_some_and(|n| n >= storage::CHUNK_BYTES as i64) { break; }
                assert!(!task.is_finished(), "fixture must span multiple chunks");
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }).await.unwrap();
        cancel.cancel();
        assert!(tokio::time::timeout(Duration::from_secs(5), task).await.unwrap().unwrap().is_err());
        assert_eq!(Arc::strong_count(&original), 1, "cancelled codec task was joined, not detached");
        assert!(load(&pool, &job, SNAPSHOT, &CancellationToken::new()).await.is_err());
        save(&pool, &job, original.clone()).await;
        let restored = load(&pool, &job, SNAPSHOT, &CancellationToken::new()).await.unwrap(); exact(&original, &restored);
        let maximum: i32 = sqlx::query_scalar("select max(octet_length(payload)) from continuous_frozen_chunks").fetch_one(&pool).await.unwrap();
        assert_eq!(maximum as usize, storage::CHUNK_BYTES);
        let metadata: (i64, i64, i64) = sqlx::query_as("select count(*)::bigint,min(chunk_index)::bigint,max(chunk_index)::bigint from continuous_frozen_chunks").fetch_one(&pool).await.unwrap();
        assert!(metadata.0 > 1); assert_eq!(metadata.1, 0); assert_eq!(metadata.2 + 1, metadata.0, "replacement leaves no stale tail chunks");
    }).await;
}
