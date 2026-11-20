//! Real PostgreSQL contracts for the study_actual public DB API, not HTTP or
//! numerical outcome end-to-end tests. Every ignored case owns a disposable DB.
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use setups_db::{
    DbError,
    admission::{self, AdmissionInstrument, ReplayAdmissionInput, ReplayAdmissionLimits, ReserveStatus},
    optimization::{self, DeleteStudyOutcome},
    runs,
    study_actual::{self as actual, Admission, InstrumentOutput, Job, Request},
};
use setups_domain::AlgorithmParams;
use sqlx::{PgPool, postgres::{PgConnectOptions, PgPoolOptions}};
use std::{future::Future, str::FromStr};

const START: i64 = 1_794_000_000;
const CUTOFF: i64 = START + 600;

async fn isolated<F, Fut>(body: F)
where
    F: FnOnce(PgPool) -> Fut + 'static,
    Fut: Future<Output = ()> + 'static,
{
    let options = PgConnectOptions::from_str(
        &std::env::var("TEST_DATABASE_URL").expect("explicit local *_test URL required"),
    ).unwrap();
    assert!(options.get_database().unwrap_or("").ends_with("_test"));
    assert!(matches!(options.get_host(), "localhost" | "127.0.0.1" | "::1"));
    let options = options.options([("statement_timeout", "15000"), ("lock_timeout", "5000")]);
    let admin = PgPoolOptions::new().max_connections(2)
        .connect_with(options.clone()).await.unwrap();
    let name = format!("actual_lifecycle_{}_{}_test", std::process::id(),
        Utc::now().timestamp_nanos_opt().unwrap());
    sqlx::query(&format!("create database {name}")).execute(&admin).await.unwrap();
    let connected = PgPoolOptions::new().max_connections(6)
        .connect_with(options.database(&name)).await;
    // Keep the pool outside the child task: assertion/migration panics still
    // close connections and drop only this generated database before unwinding.
    let outcome = if let Ok(pool) = &connected {
        let child = pool.clone();
        let result = tokio::task::LocalSet::new().run_until(async move {
            tokio::task::spawn_local(async move {
                let baseline = include_str!("../migrations/0010_baseline.sql").lines()
                    .filter(|line| !line.starts_with('\\')
                        && !line.starts_with("SET transaction_timeout")
                        && !line.contains("set_config('search_path'"))
                    .collect::<Vec<_>>().join("\n");
                sqlx::raw_sql(&baseline).execute(&child).await.unwrap();
                sqlx::raw_sql("set search_path to public; create table kysely_migration(name varchar(255) primary key,timestamp varchar(255) not null); insert into kysely_migration values('0009_legacy','fixture')")
                    .execute(&child).await.unwrap();
                setups_db::migrate::ensure_migrations(&child).await.unwrap();
                let repeated = setups_db::migrate::ensure_migrations(&child).await.unwrap();
                assert!(repeated.starts_with("0 applied"), "migration replay: {repeated}");
                body(child).await;
            }).await
        }).await;
        pool.close().await;
        Some(result)
    } else {
        None
    };
    let cleanup = sqlx::query(&format!("drop database {name} with(force)"))
        .execute(&admin).await;
    admin.close().await;
    cleanup.expect("owned disposable DB cleanup failed");
    let result = outcome.unwrap_or_else(|| panic!("child DB connection failed: {:?}", connected.err()));
    if let Err(error) = result {
        std::panic::resume_unwind(error.into_panic());
    }
}

#[derive(Clone)]
struct Study {
    owner: &'static str,
    id: i64,
    identity: String,
    source: Value,
}

async fn seed(pool: &PgPool, owner: &'static str, label: &str) -> Study {
    let params = serde_json::to_value(AlgorithmParams::default()).unwrap();
    let source = json!({"algorithmId":"price_volume_spike", "algorithmVersion":"1.1.1",
        "params":params, "startUnix":START as f64 + 30.25,
        "instruments":[{"exchange":"bybit","category":"linear","symbol":"BTCUSDT"}]});
    let spec = json!({"algorithmId":"price_volume_spike","label":label,"owner":owner});
    let result = json!({"frozenSentinel":label,"science":1.2345678901234567});
    let id: i64 = sqlx::query_scalar("insert into optimization_studies(algorithm_id,status,spec,progress,result,owner_id,execution_started_at) values('price_volume_spike','completed',$1,'{}',$2,$3,to_timestamp($4)) returning id")
        .bind(spec).bind(result).bind(owner).bind(START as f64 + 30.25)
        .fetch_one(pool).await.unwrap();
    let identity: String = sqlx::query_scalar("select md5(spec::text||coalesce(result::text,'null')) from optimization_studies where id=$1")
        .bind(id).fetch_one(pool).await.unwrap();
    Study { owner, id, identity, source }
}

async fn admit(pool: &PgPool, study: &Study, name: &str, limits: ReplayAdmissionLimits) -> Result<Admission, DbError> {
    actual::admit(pool, Request { owner: study.owner, study: study.id, job: name,
        start: DateTime::from_timestamp(START + 30, 250_000_000).unwrap(),
        cutoff: CUTOFF, source: &study.source, identity: &study.identity, work: 60, limits: &limits,
    }).await
}

fn created(admission: Admission) -> Value {
    match admission {
        Admission::Created(value) => value,
        other => panic!("expected Created, got {other:?}"),
    }
}

async fn head(pool: &PgPool, study: &Study) -> Value {
    actual::get(pool, study.owner, study.id).await.unwrap().unwrap()
}

async fn claim(pool: &PgPool) -> Job {
    actual::claim(pool, 2).await.unwrap().expect("queued job should be claimable")
}

fn rows(count: usize) -> Vec<Value> {
    let statuses = ["tp", "sl", "gate_failed", "unclosed", "no_data"];
    (0..count).map(|i| json!({"setupTimeUnix":START + (i as i64 + 1) * 60,
        "status":statuses[i % statuses.len()], "sentinel":i})).collect()
}

async fn append(pool: &PgPool, job: &Job, batch: i32, symbol: &str, rows: &[Value]) -> Result<bool, DbError> {
    let emitted = json!(rows);
    actual::append(pool, job, batch, InstrumentOutput { exchange: "bybit", category: "linear",
        symbol, emitted: &emitted, rows, quality: &json!({"fixture":"quality"}),
    }).await
}

async fn complete(pool: &PgPool, study: &Study, name: &str) -> (Job, i32) {
    created(admit(pool, study, name, ReplayAdmissionLimits::default()).await.unwrap());
    let job = claim(pool).await;
    assert_eq!(job.study, study.id);
    let batch = actual::create_batch(pool, &job).await.unwrap().unwrap();
    assert!(append(pool, &job, batch, "BTCUSDT", &rows(5)).await.unwrap());
    sqlx::query("insert into replay_quality_intervals(run_id,from_unix,to_unix,reason) select id,$2,$3,'fixture' from replay_runs where batch_id=$1")
        .bind(batch).bind(START).bind(START + 60).execute(pool).await.unwrap();
    assert!(actual::finish(pool, &job, &json!({"fixture":"sealed-report"}),
        &json!({"artifactSetupCount":5,"closedCount":2})).await.unwrap());
    (job, batch)
}

async fn count(pool: &PgPool, query: &str, id: i64) -> i64 {
    sqlx::query_scalar(query).bind(id).fetch_one(pool).await.unwrap()
}

async fn assert_pair_absent(pool: &PgPool, study: i64, batch: i32) {
    assert_eq!(count(pool, "select count(*) from study_actual_setups where study_id=$1", study).await, 0);
    assert_eq!(count(pool, "select count(*) from replay_runs where batch_id=$1::bigint", batch.into()).await, 0);
    assert_eq!(count(pool, "select count(*) from replay_batches where id=$1::bigint", batch.into()).await, 0);
}

async fn seed_sentinels(pool: &PgPool) {
    for (origin, symbol) in [("manual", "ORIGINAL"), ("continuous", "CONTINUOUS")] {
        let batch: i32 = sqlx::query_scalar("insert into replay_batches(exchange_code,category,algorithm_id,algorithm_version,params,from_ts,to_ts,instrument_count,origin,owner_id,name) values('bybit','linear','price_volume_spike','1.1.1',$1,to_timestamp($2),to_timestamp($3),1,$4,'admin',$5) returning id")
            .bind(serde_json::to_value(AlgorithmParams::default()).unwrap()).bind(START as f64)
            .bind(CUTOFF as f64).bind(origin).bind(symbol).fetch_one(pool).await.unwrap();
        let run: i32 = sqlx::query_scalar("insert into replay_runs(exchange_code,category,symbol,algorithm_id,algorithm_version,params,from_ts,to_ts,status,origin,owner_id,batch_id,setups) values('bybit','linear',$1,'price_volume_spike','1.1.1',$2,to_timestamp($3),to_timestamp($4),'done',$5,'admin',$6,$7) returning id")
            .bind(symbol).bind(serde_json::to_value(AlgorithmParams::default()).unwrap())
            .bind(START as f64).bind(CUTOFF as f64).bind(origin).bind(batch)
            .bind(json!([{"protected":symbol}])).fetch_one(pool).await.unwrap();
        sqlx::query("insert into replay_quality_intervals(run_id,from_unix,to_unix,reason) values($1,$2,$3,'protected')")
            .bind(run).bind(START).bind(START + 60).execute(pool).await.unwrap();
        sqlx::query("insert into replay_cache(cache_key,exchange_code,category,symbol,algorithm_id,algorithm_version,params,from_ts,to_ts,setups,warmup_minutes,processed_minutes,quality_source_run_id) values($1,'bybit','linear',$1,'price_volume_spike','1.1.1',$2,to_timestamp($3),to_timestamp($4),$5,0,1,$6)")
            .bind(symbol).bind(serde_json::to_value(AlgorithmParams::default()).unwrap())
            .bind(START as f64).bind(CUTOFF as f64).bind(json!([{"protected":symbol}]))
            .bind(run).execute(pool).await.unwrap();
    }
    sqlx::query("insert into analytics_selection_cache(scope_key,maturity_tenths,month_key,algorithm_version,result_json) values('protected-research',0,'all',1,'{\"science\":42}')")
        .execute(pool).await.unwrap();
}

// Exclude only the target admin actual artifacts. Compare full protected rows,
// including cache source-run links and timestamps, not just aggregate counts.
async fn protected(pool: &PgPool) -> Value {
    sqlx::query_scalar("select jsonb_build_object(\
        'studies',(select jsonb_agg(to_jsonb(s) order by id) from optimization_studies s where owner_id<>'admin'),\
        'runs',(select jsonb_agg(to_jsonb(r) order by id) from replay_runs r where origin<>'study_actual' or owner_id<>'admin'),\
        'batches',(select jsonb_agg(to_jsonb(b) order by id) from replay_batches b where origin<>'study_actual' or owner_id<>'admin'),\
        'quality',(select jsonb_agg(to_jsonb(q) order by run_id,from_unix) from replay_quality_intervals q join replay_runs r on r.id=q.run_id where r.origin<>'study_actual' or r.owner_id<>'admin'),\
        'cache',(select jsonb_agg(to_jsonb(c) order by cache_key) from replay_cache c),\
        'selection',(select jsonb_agg(to_jsonb(c) order by scope_key) from analytics_selection_cache c),\
        'foreignRows',(select jsonb_agg(to_jsonb(a) order by id) from study_actual_setups a where owner_id<>'admin'),\
        'foreignHeads',(select jsonb_agg(to_jsonb(a) order by study_id) from study_actualizations a where owner_id<>'admin'))")
        .fetch_one(pool).await.unwrap()
}

async fn continuous_reservation(pool: &PgPool, bytes: i64) -> i64 {
    sqlx::query("insert into continuous_owner_lanes(owner_id) values('reservation')").execute(pool).await.unwrap();
    sqlx::query("insert into continuous_processes(owner_id,algorithm_id,config) values('reservation','price_volume_spike','{}')").execute(pool).await.unwrap();
    let slot: i64 = sqlx::query_scalar("insert into continuous_slots(owner_id,algorithm_id,min_setups_per_day) values('reservation','price_volume_spike',0) returning id")
        .fetch_one(pool).await.unwrap();
    sqlx::query_scalar("insert into continuous_generations(owner_id,algorithm_id,slot_id,config_revision,lease_token,spec,execution_started_at,observation_cutoff,reserved_bytes) values('reservation','price_volume_spike',$1,1,1,'{}',to_timestamp($2),$3,$4) returning id")
        .bind(slot).bind(CUTOFF as f64).bind(CUTOFF).bind(bytes).fetch_one(pool).await.unwrap()
}

#[tokio::test]
#[ignore = "explicit loopback TEST_DATABASE_URL and CREATEDB required"]
async fn concurrent_same_study_admission_and_claim_have_one_current_job() {
    isolated(|pool| async move {
        let study = seed(&pool, "admin", "same-study").await;
        let foreign = seed(&pool, "foreign", "foreign").await;
        let limits = ReplayAdmissionLimits::default();
        let (left, right) = tokio::join!(admit(&pool, &study, "left", limits), admit(&pool, &study, "right", limits));
        let (accepted, busy) = match (left.unwrap(), right.unwrap()) {
            (Admission::Created(a), Admission::Busy(b)) | (Admission::Busy(b), Admission::Created(a)) => (a, b),
            other => panic!("exactly one Created and one Busy required: {other:?}"),
        };
        assert_eq!(accepted["revision"], 1);
        assert_eq!(accepted, busy, "Busy must return the same current queued head");
        assert!(accepted.get("source").is_none(), "private frozen source must not be exposed");
        let (left, right) = tokio::join!(actual::claim(&pool, 2), actual::claim(&pool, 2));
        assert_eq!(usize::from(left.unwrap().is_some()) + usize::from(right.unwrap().is_some()), 1);
        assert_eq!(head(&pool, &study).await["status"], "running");
        assert!(actual::get(&pool, foreign.owner, study.id).await.unwrap().is_none());
        let mut impostor = study.clone();
        impostor.owner = foreign.owner;
        assert!(matches!(admit(&pool, &impostor, "impostor", limits).await.unwrap(), Admission::SourceChanged));
        let mut changed = foreign.clone();
        changed.identity = "wrong-frozen-source".into();
        assert!(matches!(admit(&pool, &changed, "changed", limits).await.unwrap(), Admission::SourceChanged));
        assert!(actual::get(&pool, foreign.owner, foreign.id).await.unwrap().is_none());
    }).await;
}

#[tokio::test]
#[ignore = "explicit loopback TEST_DATABASE_URL and CREATEDB required"]
async fn independent_jobs_share_capacity_and_manual_continuous_reservations() {
    isolated(|pool| async move {
        let first = seed(&pool, "admin", "first").await;
        let second = seed(&pool, "foreign", "second").await;
        let third = seed(&pool, "admin", "third").await;
        let limits = ReplayAdmissionLimits::default();
        let input = ReplayAdmissionInput { instruments: vec![AdmissionInstrument {
            exchange: "bybit".into(), category: "linear".into(), symbol: "MANUAL".into(),
        }], from_unix: START, to_unix: CUTOFF, params: AlgorithmParams::default() };
        let manual = admission::reserve_replay_runs(&pool, &input, &limits, ReserveStatus::Running).await.unwrap();
        created(admit(&pool, &first, "first", limits).await.unwrap());
        created(admit(&pool, &second, "second", limits).await.unwrap());
        let a = claim(&pool).await;
        assert!(actual::claim(&pool, 2).await.unwrap().is_none(), "manual worker owns the other slot");
        sqlx::query("update replay_runs set status='done' where id=$1").bind(manual[0].id as i32).execute(&pool).await.unwrap();
        let b = claim(&pool).await;
        assert_ne!(a.study, b.study, "both independent studies must run at capacity two");
        assert!(actual::claim(&pool, 2).await.unwrap().is_none());
        assert!(matches!(admit(&pool, &third, "full", ReplayAdmissionLimits { max_pending_runs: 2, ..limits }).await.unwrap(), Admission::Resource));
        actual::fail(&pool, &a, "fixture-done").await.unwrap();
        actual::fail(&pool, &b, "fixture-done").await.unwrap();
        let queued = admission::reserve_replay_runs(&pool, &input, &limits, ReserveStatus::Queued).await.unwrap();
        assert!(matches!(admit(&pool, &third, "manual-pending", ReplayAdmissionLimits { max_pending_runs: 1, ..limits }).await.unwrap(), Admission::Resource));
        let used: i64 = sqlx::query_scalar("select pg_database_size(current_database())::bigint").fetch_one(&pool).await.unwrap();
        assert!(matches!(admit(&pool, &third, "manual-bytes", ReplayAdmissionLimits {
            max_database_bytes: used + actual::MAX_OUTPUT_BYTES + 512 * 1024, ..limits
        }).await.unwrap(), Admission::Resource), "manual reservation bytes cannot be spent twice");
        sqlx::query("update replay_runs set status='done' where id=$1").bind(queued[0].id as i32).execute(&pool).await.unwrap();
        let work_limits = ReplayAdmissionLimits { max_work_minutes: 119, ..limits };
        created(admit(&pool, &first, "work-first", work_limits).await.unwrap());
        assert!(matches!(admit(&pool, &second, "work-second", work_limits).await.unwrap(), Admission::Resource));
        let work = claim(&pool).await;
        actual::fail(&pool, &work, "fixture-done").await.unwrap();
        continuous_reservation(&pool, 16 * 1024 * 1024).await;
        let used: i64 = sqlx::query_scalar("select pg_database_size(current_database())::bigint").fetch_one(&pool).await.unwrap();
        assert!(matches!(admit(&pool, &third, "continuous-bytes", ReplayAdmissionLimits {
            max_database_bytes: used + actual::MAX_OUTPUT_BYTES + 8 * 1024 * 1024, ..limits
        }).await.unwrap(), Admission::Resource), "continuous reservation must reduce actual admission headroom");
        created(admit(&pool, &third, "room-after-reject", limits).await.unwrap());
        let room = claim(&pool).await;
        actual::fail(&pool, &room, "fixture-done").await.unwrap();

        // Keep all three audit regressions independently observable: if an
        // unexpected admission/claim succeeds, settle only that owned fixture
        // before collecting the next result, then report every violation.
        let mut violations = Vec::new();
        let one_pending = ReplayAdmissionLimits { max_pending_runs: 1, ..limits };
        created(admit(&pool, &first, "actual-arrives-first", one_pending).await.unwrap());
        let manual_after_actual = admission::reserve_replay_runs(&pool, &input, &one_pending, ReserveStatus::Queued).await;
        if !matches!(&manual_after_actual, Err(admission::QueueError::Admission(e))
            if e.resource == admission::AdmissionResource::QueueJobs && e.limit == 1 && e.requested == 2) {
            violations.push(format!("manual-after-actual must reject QueueJobs(limit=1, requested=2), got {manual_after_actual:?}"));
        }
        if let Ok(unexpected) = manual_after_actual {
            for run in unexpected {
                sqlx::query("update replay_runs set status='done' where id=$1")
                    .bind(run.id as i32).execute(&pool).await.unwrap();
            }
        }
        let actual_first = claim(&pool).await;
        actual::fail(&pool, &actual_first, "fixture-done").await.unwrap();

        let manual_first = admission::reserve_replay_runs(&pool, &input, &one_pending, ReserveStatus::Running).await.unwrap();
        let actual_after_manual = admit(&pool, &second, "manual-arrives-first", one_pending).await.unwrap();
        match actual_after_manual {
            Admission::Resource => { created(admit(&pool, &second, "capacity-fixture", limits).await.unwrap()); }
            Admission::Created(_) => violations.push("actual-after-manual must reject Resource at max_pending_runs=1".into()),
            other => panic!("unexpected actual-after-manual admission: {other:?}"),
        }
        sqlx::query("update replay_runs set status='cancel_requested' where id=$1")
            .bind(manual_first[0].id as i32).execute(&pool).await.unwrap();
        let queued_before = head(&pool, &second).await;
        let during_cancel = actual::claim(&pool, 1).await.unwrap();
        let blocked = during_cancel.is_none();
        if let Some(unexpected) = during_cancel {
            violations.push("capacity=1 actual claim admitted while manual cancel_requested was still executing".into());
            actual::fail(&pool, &unexpected, "fixture-done").await.unwrap();
        } else {
            assert_eq!(head(&pool, &second).await, queued_before, "blocked claim must retain queued job/fence/cutoff");
        }
        sqlx::query("update replay_runs set status='done' where id=$1")
            .bind(manual_first[0].id as i32).execute(&pool).await.unwrap();
        if blocked {
            let after_settlement = actual::claim(&pool, 1).await.unwrap().expect("manual settlement releases capacity");
            assert_eq!(after_settlement.study, second.id);
            assert_eq!(after_settlement.cutoff, CUTOFF);
            actual::fail(&pool, &after_settlement, "fixture-done").await.unwrap();
        }
        assert!(violations.is_empty(), "shared manual/actual admission violations: {}", violations.join("; "));
    }).await;
}

#[tokio::test]
#[ignore = "explicit loopback TEST_DATABASE_URL and CREATEDB required"]
async fn append_publish_pages_and_output_bounds_are_transactional() {
    isolated(|pool| async move {
        let mut study = seed(&pool, "admin", "output").await;
        study.source["instruments"] = json!([
            {"exchange":"bybit","category":"linear","symbol":"BTCUSDT"},
            {"exchange":"bybit","category":"linear","symbol":"ETHUSDT"}
        ]);
        seed(&pool, "foreign", "foreign").await;
        created(admit(&pool, &study, "output", ReplayAdmissionLimits::default()).await.unwrap());
        let job = claim(&pool).await;
        let batch = actual::create_batch(&pool, &job).await.unwrap().unwrap();
        let bad_rows = vec![rows(1).remove(0), json!({"setupTimeUnix":START + 120,"padding":"x".repeat(16_385)})];
        assert!(append(&pool, &job, batch, "BAD", &bad_rows).await.is_err());
        assert_eq!(count(&pool, "select count(*) from replay_runs where batch_id=$1::bigint", batch.into()).await, 0);
        assert_eq!(count(&pool, "select count(*) from study_actual_setups where study_id=$1", study.id).await, 0);
        assert!(append(&pool, &job, batch, "BTCUSDT", &rows(2)).await.unwrap());
        assert!(append(&pool, &job, batch, "ETHUSDT", &rows(3)).await.unwrap());
        assert!(actual::page(&pool, study.owner, study.id, 1, 0, 10).await.unwrap().is_none(), "staging is not published");
        let run_ids: Vec<i32> = sqlx::query_scalar("select id from replay_runs where batch_id=$1 order by id")
            .bind(batch).fetch_all(&pool).await.unwrap();
        assert_eq!(run_ids.len(), 2, "both instrument staging runs really exist");
        assert_eq!(count(&pool, "select count(*) from replay_runs where batch_id=$1::bigint and status='done'", batch.into()).await, 2);
        for run in &run_ids {
            assert!(runs::get_run_full(&pool, (*run).into()).await.unwrap().is_none(), "generic detail must hide staging");
            assert!(runs::get_run_page(&pool, (*run).into(), 0, 10).await.unwrap().is_none(), "generic paging must hide staging");
        }
        assert!(runs::get_replay_batch(&pool, batch.into()).await.unwrap().is_none());
        assert!(runs::list_replay_batches(&pool, 100).await.unwrap().iter().all(|b| b.id != i64::from(batch)));
        let report = json!({"fixture":"report","meanRiskExpectancy":null});
        let totals = json!({"artifactSetupCount":5,"closedCount":4,"gateFailedCount":1});
        assert!(actual::finish(&pool, &job, &report, &totals).await.unwrap());
        let published = head(&pool, &study).await;
        assert_eq!(published["status"], "completed");
        assert_eq!(published["report"], report);
        assert_eq!(published["totals"], totals);
        assert_eq!(published["reserved_bytes"], 0);
        let dataset_total = count(&pool, "select count(*) from study_actual_setups where study_id=$1", study.id).await;
        let artifact_total = published["totals"]["artifactSetupCount"].as_i64().unwrap();
        assert_eq!(dataset_total, artifact_total);
        let mut run_setup_total = 0;
        for run_id in &run_ids {
            let full = runs::get_run_full(&pool, (*run_id).into()).await.unwrap().unwrap();
            let expected = match full.symbol.as_str() { "BTCUSDT" => 2, "ETHUSDT" => 3, other => panic!("unexpected symbol {other}") };
            assert_eq!(full.origin, "study_actual");
            assert_eq!(full.actual_study_id, Some(study.id));
            assert_eq!(full.actual_revision, Some(1));
            assert_eq!(full.actual_job_id.as_deref(), Some(job.job_id.as_str()));
            assert_eq!(full.setup_count, expected);
            run_setup_total += full.setup_count;
            assert_eq!(full.setups.as_array().unwrap().len(), expected as usize);
            let page = runs::get_run_page(&pool, (*run_id).into(), 1, 1).await.unwrap().unwrap();
            assert_eq!(page.run, full);
            assert_eq!(page.setups_page, json!([full.setups[1]]));
            assert_eq!(admission::delete_replay_run(&pool, (*run_id).into()).await.unwrap(),
                Some(admission::DeleteRunOutcome::Referenced));
        }
        let detail = runs::get_replay_batch(&pool, batch.into()).await.unwrap().unwrap();
        assert_eq!(detail.origin, "study_actual");
        assert_eq!(detail.actual_study_id, Some(study.id));
        assert_eq!(detail.actual_revision, Some(1));
        assert_eq!(detail.actual_job_id.as_deref(), Some(job.job_id.as_str()));
        assert_eq!(detail.setup_count, 5);
        assert_eq!(detail.setup_count, dataset_total);
        assert_eq!(detail.setup_count, artifact_total);
        assert_eq!(run_setup_total, dataset_total);
        assert_eq!(detail.instrument_count, 2);
        assert_eq!(detail.done_instruments, 2);
        assert_eq!(runs::list_replay_batches(&pool, 100).await.unwrap(), vec![detail]);
        assert_eq!(runs::delete_replay_batch(&pool, batch.into()).await.unwrap(), Some(runs::BatchDeleteOutcome::Referenced));
        let all = actual::page(&pool, study.owner, study.id, 1, 0, 1000).await.unwrap().unwrap();
        assert_eq!(all.len(), 5);
        assert!(all.windows(2).all(|p| p[0]["setupTimeUnix"].as_i64() >= p[1]["setupTimeUnix"].as_i64()));
        let page0 = actual::page(&pool, study.owner, study.id, 1, 0, 2).await.unwrap().unwrap();
        let page1 = actual::page(&pool, study.owner, study.id, 1, 2, 2).await.unwrap().unwrap();
        let page2 = actual::page(&pool, study.owner, study.id, 1, 4, 2).await.unwrap().unwrap();
        assert_eq!([page0, page1, page2].concat(), all);
        assert_eq!(actual::page(&pool, study.owner, study.id, 1, -9, 0).await.unwrap().unwrap(), all[..1]);
        assert!(actual::page(&pool, study.owner, study.id, 1, 500, 2).await.unwrap().unwrap().is_empty());
        assert!(actual::page(&pool, "foreign", study.id, 1, 0, 2).await.unwrap().is_none());
        assert!(actual::page(&pool, study.owner, study.id, 2, 0, 2).await.unwrap().is_none());
        created(admit(&pool, &study, "row-limit", ReplayAdmissionLimits::default()).await.unwrap());
        for run in &run_ids {
            assert!(runs::get_run_full(&pool, (*run).into()).await.unwrap().is_none());
            assert!(runs::get_run_page(&pool, (*run).into(), 0, 10).await.unwrap().is_none());
        }
        assert!(runs::get_replay_batch(&pool, batch.into()).await.unwrap().is_none());
        assert!(runs::list_replay_batches(&pool, 100).await.unwrap().iter().all(|b| b.id != i64::from(batch)));
        let bounded = claim(&pool).await;
        let bounded_batch = actual::create_batch(&pool, &bounded).await.unwrap().unwrap();
        assert!(append(&pool, &bounded, bounded_batch, "BOUND", &[]).await.unwrap());
        // Seed at the real constant in SQL, not a 100k-element Rust allocation.
        sqlx::query("insert into study_actual_setups(owner_id,study_id,revision,run_id,signal_close_unix,instrument_key,row_data) select $1,$2,$3,r.id,$4,'bybit:linear:BOUND',jsonb_build_object('setupTimeUnix',$4::bigint) from replay_runs r cross join generate_series(1,$5::bigint) where r.batch_id=$6")
            .bind(study.owner).bind(study.id).bind(bounded.revision).bind(START + 60)
            .bind(actual::MAX_ROWS).bind(bounded_batch).execute(&pool).await.unwrap();
        let error = append(&pool, &bounded, bounded_batch, "OVER", &rows(1)).await.unwrap_err();
        assert!(error.to_string().contains("study_actual_output_budget_exceeded"), "{error}");
        assert_eq!(count(&pool, "select count(*) from study_actual_setups where study_id=$1", study.id).await, actual::MAX_ROWS);
        assert_eq!(count(&pool, "select count(*) from replay_runs where batch_id=$1::bigint", bounded_batch.into()).await, 1);
        assert!(actual::finish(&pool, &bounded, &json!({"fixture":"row-bound"}),
            &json!({"artifactSetupCount":actual::MAX_ROWS})).await.unwrap());
        assert_eq!(actual::page(&pool, study.owner, study.id, bounded.revision, 0, actual::MAX_ROWS).await.unwrap().unwrap().len(), 100,
            "page limit is clamped without returning the entire retained dataset");
    }).await;
}

#[tokio::test]
#[ignore = "explicit loopback TEST_DATABASE_URL and CREATEDB required"]
async fn replacement_only_deletes_owned_pair_and_fences_every_old_writer() {
    isolated(|pool| async move {
        let study = seed(&pool, "admin", "replace").await;
        let foreign = seed(&pool, "foreign", "foreign").await;
        seed_sentinels(&pool).await;
        let (old, old_batch) = complete(&pool, &study, "old").await;
        complete(&pool, &foreign, "foreign").await;
        let before = protected(&pool).await;
        let replacement = created(admit(&pool, &study, "new", ReplayAdmissionLimits::default()).await.unwrap());
        assert_eq!(replacement["revision"], 2);
        assert_eq!(replacement["status"], "queued");
        assert!(replacement["report"].is_null() && replacement["batch_id"].is_null());
        assert_pair_absent(&pool, study.id, old_batch).await;
        assert_eq!(protected(&pool).await, before);
        let new = claim(&pool).await;
        let new_batch = actual::create_batch(&pool, &new).await.unwrap().unwrap();
        assert_ne!(old_batch, new_batch);
        assert!(append(&pool, &new, new_batch, "NEW", &rows(2)).await.unwrap());
        let current = head(&pool, &study).await;
        for stale in [old.clone(), Job { token: new.token - 1, ..new.clone() }] {
            assert!(!actual::progress(&pool, &stale, "stale", &json!({"wrong":true})).await.unwrap());
            assert!(actual::create_batch(&pool, &stale).await.unwrap().is_none());
            assert!(!append(&pool, &stale, new_batch, "STALE", &rows(1)).await.unwrap());
            assert!(!actual::finish(&pool, &stale, &json!({"wrong":true}), &json!({})).await.unwrap());
            actual::fail(&pool, &stale, "stale-failure").await.unwrap();
            assert_eq!(head(&pool, &study).await, current);
        }
        assert!(actual::progress(&pool, &new, "finalizing", &json!({"instruments":1})).await.unwrap());
        assert!(actual::finish(&pool, &new, &json!({"revision":2}), &json!({"artifactSetupCount":2})).await.unwrap());
        assert!(actual::page(&pool, study.owner, study.id, 1, 0, 100).await.unwrap().is_none());
        let page = actual::page(&pool, study.owner, study.id, 2, 0, 100).await.unwrap().unwrap();
        assert_eq!(page.len(), 2);
        assert!(page.iter().all(|row| row["sentinel"].as_u64().unwrap() < 2));
        assert_eq!(protected(&pool).await, before);
        let identity: String = sqlx::query_scalar("select md5(spec::text||coalesce(result::text,'null')) from optimization_studies where id=$1")
            .bind(study.id).fetch_one(&pool).await.unwrap();
        assert_eq!(identity, study.identity);
    }).await;
}

#[tokio::test]
#[ignore = "explicit loopback TEST_DATABASE_URL and CREATEDB required"]
async fn recovery_preserves_queued_and_fails_fenced_running_with_owned_cleanup() {
    isolated(|pool| async move {
        let running = seed(&pool, "admin", "running").await;
        let queued = seed(&pool, "foreign", "queued").await;
        seed_sentinels(&pool).await;
        created(admit(&pool, &running, "running", ReplayAdmissionLimits::default()).await.unwrap());
        let job = claim(&pool).await;
        let batch = actual::create_batch(&pool, &job).await.unwrap().unwrap();
        assert!(append(&pool, &job, batch, "STAGING", &rows(3)).await.unwrap());
        created(admit(&pool, &queued, "queued", ReplayAdmissionLimits::default()).await.unwrap());
        let queued_before = head(&pool, &queued).await;
        let protected_before = protected(&pool).await;
        actual::recover(&pool).await.unwrap();
        actual::recover(&pool).await.unwrap();
        assert_eq!(head(&pool, &queued).await, queued_before);
        let failed = head(&pool, &running).await;
        assert_eq!(failed["status"], "failed");
        assert_eq!(failed["error"]["code"], "study_actual_interrupted_by_restart");
        assert_eq!(failed["reserved_bytes"], 0);
        assert!(failed["report"].is_null() && failed["batch_id"].is_null());
        assert!(failed["token"].as_i64().unwrap() > job.token);
        assert_pair_absent(&pool, running.id, batch).await;
        assert!(!actual::progress(&pool, &job, "late", &json!({})).await.unwrap());
        assert!(!append(&pool, &job, batch, "LATE", &rows(1)).await.unwrap());
        assert!(!actual::finish(&pool, &job, &json!({}), &json!({})).await.unwrap());
        actual::fail(&pool, &job, "late").await.unwrap();
        assert_eq!(head(&pool, &running).await, failed);
        assert_eq!(protected(&pool).await, protected_before);
        let recovered = claim(&pool).await;
        assert_eq!(recovered.study, queued.id);
        assert_eq!(recovered.job_id, "queued");
        assert_eq!(recovered.cutoff, CUTOFF);
        assert_eq!(recovered.revision, 1);
    }).await;
}

#[tokio::test]
#[ignore = "explicit loopback TEST_DATABASE_URL and CREATEDB required"]
async fn ownership_mismatch_rolls_back_cleanup_without_deleting_foreign_pair() {
    isolated(|pool| async move {
        let study = seed(&pool, "admin", "mismatch").await;
        let foreign = seed(&pool, "foreign", "foreign").await;
        seed_sentinels(&pool).await;
        let (_, owned_batch) = complete(&pool, &study, "owned").await;
        let (_, foreign_batch) = complete(&pool, &foreign, "foreign").await;
        // Deliberate DB fault injection: a pointer is not deletion authority.
        sqlx::query("update study_actualizations set batch_id=$2 where study_id=$1")
            .bind(study.id).bind(foreign_batch).execute(&pool).await.unwrap();
        let before = head(&pool, &study).await;
        let protected_before = protected(&pool).await;
        let error = admit(&pool, &study, "must-rollback", ReplayAdmissionLimits::default()).await.unwrap_err();
        assert!(error.to_string().contains("study_actual_owned_pair_mismatch"), "{error}");
        assert_eq!(head(&pool, &study).await, before, "batch pointer update must roll back");
        assert_eq!(count(&pool, "select count(*) from study_actual_setups where study_id=$1", study.id).await, 5, "row deletion must roll back");
        assert_eq!(count(&pool, "select count(*) from replay_batches where id=$1::bigint", owned_batch.into()).await, 1);
        assert_eq!(count(&pool, "select count(*) from replay_runs where batch_id=$1::bigint", owned_batch.into()).await, 1);
        assert_eq!(protected(&pool).await, protected_before);
    }).await;
}

#[tokio::test]
#[ignore = "explicit loopback TEST_DATABASE_URL and CREATEDB required"]
async fn explicit_manual_study_delete_blocks_active_but_cleans_terminal_actual_only() {
    isolated(|pool| async move {
        let study = seed(&pool, "admin", "delete-completed").await;
        let failed_study = seed(&pool, "admin", "delete-failed").await;
        let foreign = seed(&pool, "foreign", "foreign").await;
        seed_sentinels(&pool).await;
        complete(&pool, &foreign, "foreign").await;
        let protected_before = protected(&pool).await;
        created(admit(&pool, &study, "delete", ReplayAdmissionLimits::default()).await.unwrap());
        assert!(matches!(optimization::delete(&pool, study.id).await.unwrap(),
            Some(DeleteStudyOutcome::Active | DeleteStudyOutcome::Referenced)), "queued actual must block deletion");
        let job = claim(&pool).await;
        let batch = actual::create_batch(&pool, &job).await.unwrap().unwrap();
        assert!(append(&pool, &job, batch, "DELETE", &rows(3)).await.unwrap());
        assert!(matches!(optimization::delete(&pool, study.id).await.unwrap(),
            Some(DeleteStudyOutcome::Active | DeleteStudyOutcome::Referenced)), "running actual must block deletion");
        let running_head = head(&pool, &study).await;
        let mut held = pool.begin().await.unwrap();
        sqlx::query("select 1 from study_actualizations where owner_id=$1 and study_id=$2 for update")
            .bind(study.owner).bind(study.id).execute(&mut *held).await.unwrap();
        // A worker can own this FK key-share lock while appending its child run.
        // Delete must read active status before requesting either conflicting lock.
        sqlx::query("select 1 from optimization_studies where id=$1 for key share")
            .bind(study.id).execute(&mut *held).await.unwrap();
        let prompt_delete = tokio::time::timeout(std::time::Duration::from_secs(1),
            optimization::delete(&pool, study.id)).await;
        // Release before inspecting/asserting even when timeout cancelled delete.
        held.rollback().await.unwrap();
        assert!(matches!(prompt_delete.expect("active actual deletion waited on the locked head/source-study FK for over 1s").unwrap(),
            Some(DeleteStudyOutcome::Active | DeleteStudyOutcome::Referenced)));
        assert_eq!(head(&pool, &study).await, running_head);
        assert!(actual::finish(&pool, &job, &json!({"fixture":"terminal"}), &json!({})).await.unwrap());
        assert_eq!(optimization::delete(&pool, study.id).await.unwrap(), Some(DeleteStudyOutcome::Deleted),
            "a terminal owned actual head must not permanently block historical manual study deletion");
        assert!(actual::get(&pool, study.owner, study.id).await.unwrap().is_none());
        assert_eq!(count(&pool, "select count(*) from optimization_studies where id=$1", study.id).await, 0);
        assert_pair_absent(&pool, study.id, batch).await;
        created(admit(&pool, &failed_study, "failed", ReplayAdmissionLimits::default()).await.unwrap());
        let failed = claim(&pool).await;
        let failed_batch = actual::create_batch(&pool, &failed).await.unwrap().unwrap();
        assert!(append(&pool, &failed, failed_batch, "FAILED", &rows(1)).await.unwrap());
        actual::fail(&pool, &failed, "fixture-failure").await.unwrap();
        assert_eq!(optimization::delete(&pool, failed_study.id).await.unwrap(), Some(DeleteStudyOutcome::Deleted));
        assert!(actual::get(&pool, failed_study.owner, failed_study.id).await.unwrap().is_none());
        assert_pair_absent(&pool, failed_study.id, failed_batch).await;
        let continuous_study = seed(&pool, "admin", "continuous-protected").await;
        let generation = continuous_reservation(&pool, 0).await;
        sqlx::query("update optimization_studies set origin='continuous',continuous_generation_id=$2 where id=$1")
            .bind(continuous_study.id).bind(generation).execute(&pool).await.unwrap();
        sqlx::query("update continuous_generations set study_id=$2 where id=$1")
            .bind(generation).bind(continuous_study.id).execute(&pool).await.unwrap();
        assert_eq!(optimization::delete(&pool, continuous_study.id).await.unwrap(), Some(DeleteStudyOutcome::Referenced));
        assert_eq!(protected(&pool).await, protected_before);
    }).await;
}
