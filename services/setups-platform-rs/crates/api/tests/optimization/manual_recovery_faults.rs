//! Targeted frozen-adapter transport failure, not an all-pool outage.
use super::*;
use futures::FutureExt;

fn long_spec() -> Value {
    let mut value = spec();
    // Enough joined batches to request Pause, but final continuation fits the
    // bounded HTTP test window even under concurrent local build/test load.
    value["maxTrials"] = json!(128);
    value["maxSeconds"] = json!(30);
    // At the mock candle's next minute TP 4 is observed without touching SL 8:
    // the acknowledged incumbent/ledger must be genuinely positive, not null.
    value["slLevels"] = json!([8]);
    value["algorithmSearch"] = json!({
        "windowMinutes":(1..=32).collect::<Vec<_>>(),
        "priceGrowthPct":(1..=32).map(|n| n.to_string()).collect::<Vec<_>>()
    });
    value
}
async fn create(router: &Router) -> i64 {
    let (status, body) = request(router, "POST", "/v1/optimization-studies", long_spec()).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    body["id"].as_i64().unwrap()
}
async fn observe(pool: &PgPool, id: i64, stage: &str) -> Value {
    // Bounded lifecycle evidence only: never print raw input/checkpoint/result.
    let state: Value = sqlx::query_scalar("select jsonb_build_object('id',s.id,'status',s.status,'errorCode',s.error->'code','epoch',s.manual_execution_epoch,'reserved',s.manual_execution_reserved,'controlRevision',s.manual_control_revision,'spentMs',s.manual_spent_ms,'phase',s.progress->'phase','evaluated',s.progress->'evaluatedCandidates','scorable',s.progress->'scorableCandidates','bestScore',s.progress->'bestScore','checkpointSequence',a.checkpoint_sequence,'checkpointBytes',octet_length(a.checkpoint)) from optimization_studies s left join manual_runtime_artifacts a on a.study_id=s.id where s.id=$1")
        .bind(id).fetch_one(pool).await.unwrap();
    eprintln!("manual_frozen_read_test stage={stage} state={state}");
    state
}
async fn transition(pool: &PgPool, id: i64, stage: &str,
    predicate: impl Fn(&db::StudyRecord) -> bool) -> db::StudyRecord {
    observe(pool, id, stage).await;
    match tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let row = db::get(pool, id).await.unwrap().unwrap();
            if predicate(&row) { return row; }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }).await {
        Ok(row) => row,
        Err(_) => {
            let state = observe(pool, id, "transition_timeout").await;
            panic!("{stage} timed out: {state}");
        }
    }
}
async fn pause_with_winner(router: &Router, pool: &PgPool, id: i64) -> db::StudyRecord {
    let running = transition(pool, id, "first completed positive trial", |row| row.status == "running"
        && row.progress["evaluatedCandidates"].as_u64().unwrap_or(0) > 0).await;
    assert!(running.progress["bestScore"].as_f64().is_some_and(|score| score > 0.0));
    assert_eq!(request(router, "POST", &format!("/v1/optimization-studies/{id}/pause"),
        json!({"expectedControlRevision":running.manual_control.unwrap().revision})).await.0,
        StatusCode::ACCEPTED);
    transition(pool, id, "acknowledged pause", |row| row.status == "paused").await
}
type Boundary = (Value, Value, Vec<u8>, String, i64, i64);
async fn boundary(pool: &PgPool, id: i64) -> Boundary {
    sqlx::query_as("select identity,snapshot_manifest,checkpoint,checkpoint_hash,observation_cutoff,checkpoint_sequence from manual_runtime_artifacts where study_id=$1")
        .bind(id).fetch_one(pool).await.unwrap()
}
async fn blocked_chunk_reader(pool: &PgPool, database: &str, lock_pid: i32, id: i64, exclude: Option<i32>) -> i32 {
    observe(pool, id, if exclude.is_some() { "await_retry_chunk_read" } else { "await_first_chunk_read" }).await;
    tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            if let Some(pid) = sqlx::query_scalar::<_, i32>(
                "select pid from pg_stat_activity where datname=$1 and pid<>pg_backend_pid() and ($3::integer is null or pid<>$3) and wait_event_type='Lock' and query like 'select c.payload,c.checksum from manual_frozen_chunks%' and $2=any(pg_blocking_pids(pid)) order by pid limit 1"
            ).bind(database).bind(lock_pid).bind(exclude).fetch_optional(pool).await.unwrap() {
                return pid;
            }
            let row = db::get(pool, id).await.unwrap().unwrap();
            assert!(row.active(), "frozen read uncertainty became terminal: status={} error={:?}", row.status, row.error);
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }).await.expect("runtime must attempt/retry the frozen-chunk read in this test database")
}

#[tokio::test]
#[ignore = "requires isolated TEST_DATABASE_URL ending _test with CREATEDB"]
async fn manual_play_retries_terminated_frozen_chunk_read_without_losing_immutable_boundary() {
    let database = TestDb::new().await;
    assert!(database.name.starts_with(&format!("optimization_test_{}_", std::process::id())));
    let (url, requests, server) = market(Duration::from_millis(5), 0).await;
    let config = OptimizationConfig { cpu_threads:1, batch_size:1, ..Default::default() };
    let runtime = start_optimization_runtime(database.pool.clone(), url, "key".into(), config.clone()).await.unwrap();
    let router = app(database.pool.clone(), config);
    let mut table_lock = None;
    // All assertion failures still release the owned lock, join the runtime,
    // abort the mock, and drop this exact TestDb before propagating the failure.
    let result = std::panic::AssertUnwindSafe(async {
        let id = create(&router).await;
        let paused = pause_with_winner(&router, &database.pool, id).await;
        let before = boundary(&database.pool, id).await;
        assert!(!before.2.is_empty());
        assert_eq!(setups_db::continuous::frozen::checksum(&before.2), before.3);
        assert!(before.1["snapshotHash"].is_string());
        let calls = requests.load(Ordering::SeqCst);
        assert!(calls > 0);
        let chunks: Value = sqlx::query_scalar("select jsonb_agg(jsonb_build_object('index',chunk_index,'checksum',checksum,'bytes',octet_length(payload)) order by chunk_index) from manual_frozen_chunks where study_id=$1")
            .bind(id).fetch_one(&database.pool).await.unwrap();

        let mut lock = database.pool.begin().await.unwrap();
        sqlx::query("set local lock_timeout='2s'").execute(&mut *lock).await.unwrap();
        sqlx::query("lock table manual_frozen_chunks in access exclusive mode")
            .execute(&mut *lock).await.unwrap();
        let lock_pid: i32 = sqlx::query_scalar("select pg_backend_pid()")
            .fetch_one(&mut *lock).await.unwrap();
        table_lock = Some(lock);
        let (status, played) = request(&router, "POST", &format!("/v1/optimization-studies/{id}/play"),
            json!({"expectedControlRevision":paused.manual_control.unwrap().revision})).await;
        assert_eq!(status, StatusCode::ACCEPTED, "{played}");
        assert_eq!(played["id"], id);
        let first_reader = blocked_chunk_reader(&database.pool, &database.name, lock_pid, id, None).await;
        assert_ne!(first_reader, lock_pid);
        // Scope the destructive injection to a currently blocked chunk SELECT
        // in THIS freshly created database, blocked by OUR lock transaction.
        let killed: Option<bool> = sqlx::query_scalar("select pg_terminate_backend(pid) from pg_stat_activity where datname=$1 and pid=$2 and pid<>pg_backend_pid() and wait_event_type='Lock' and query like 'select c.payload,c.checksum from manual_frozen_chunks%' and $3=any(pg_blocking_pids(pid))")
            .bind(&database.name).bind(first_reader).bind(lock_pid)
            .fetch_optional(&database.pool).await.unwrap();
        assert_eq!(killed, Some(true), "the targeted frozen read must actually receive transport termination");
        let second_reader = blocked_chunk_reader(&database.pool, &database.name, lock_pid, id, Some(first_reader)).await;
        assert_ne!(second_reader, first_reader, "a new DB connection must retry, not silently succeed");
        eprintln!("manual_frozen_read_test injected=backend_termination study={id} first_reader={first_reader} retry_reader={second_reader}");
        let retained = db::get(&database.pool, id).await.unwrap().unwrap();
        assert_eq!(retained.status, "running", "transient load error cannot become failed");
        assert!(retained.error.is_none() && retained.result.is_none());
        assert_eq!(boundary(&database.pool, id).await, before, "input identity/manifest/checkpoint/cutoff must survive the failed read exactly");
        let retained_chunks: Value = sqlx::query_scalar("select jsonb_agg(jsonb_build_object('index',chunk_index,'checksum',checksum,'bytes',octet_length(payload)) order by chunk_index) from manual_frozen_chunks where study_id=$1")
            .bind(id).fetch_one(&mut **table_lock.as_mut().unwrap()).await.unwrap();
        assert_eq!(retained_chunks, chunks, "only our lock connection reads chunks while holding ACCESS EXCLUSIVE; no private input was replaced");
        assert_eq!(requests.load(Ordering::SeqCst), calls, "retry may not reacquire market input");
        table_lock.take().unwrap().rollback().await.unwrap();

        let completed = transition(&database.pool, id, "resumed completion", |row| !row.active()).await;
        assert_eq!(completed.status, "completed", "{:?}", completed.error);
        assert_eq!(completed.spec, retained.spec);
        let outcome = completed.result.unwrap();
        assert!(outcome["best"]["score"].as_f64().is_some_and(|score| score > 0.0));
        assert!(outcome["best"]["score"].as_f64().unwrap() >= paused.progress["bestScore"].as_f64().unwrap());
        assert_eq!(outcome["best"]["provenance"]["snapshotHash"], before.1["snapshotHash"]);
        assert_eq!(outcome["best"]["provenance"]["capturedAt"], before.1["capturedAt"]);
        assert!(outcome["progress"]["evaluatedCandidates"].as_u64().unwrap()
            > paused.progress["evaluatedCandidates"].as_u64().unwrap(), "the saved frontier must actually continue");
        assert_eq!(requests.load(Ordering::SeqCst), calls);
        assert!(runtime.available.load(Ordering::SeqCst));
        assert!(chunks.is_array() && !chunks.as_array().unwrap().is_empty());
        assert_eq!(sqlx::query_scalar::<_, i64>("select count(*) from manual_runtime_artifacts where study_id=$1")
            .bind(id).fetch_one(&database.pool).await.unwrap(), 0, "normal terminal completion performs cleanup, not transient load failure");
    }).catch_unwind().await;
    if let Some(lock) = table_lock { let _ = lock.rollback().await; }
    runtime.stop().await;
    server.abort();
    let _ = server.await;
    database.cleanup().await;
    if let Err(panic) = result { std::panic::resume_unwind(panic); }
}
