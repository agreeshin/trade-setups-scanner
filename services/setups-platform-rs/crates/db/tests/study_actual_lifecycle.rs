//! Real PostgreSQL contracts for the study_actual public DB API, not HTTP or
//! numerical outcome end-to-end tests. Every ignored case owns a disposable DB.
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use setups_db::{
    DbError,
    admission::{
        self, AdmissionInstrument, ReplayAdmissionInput, ReplayAdmissionLimits, ReserveStatus,
    },
    optimization::{self, DeleteStudyOutcome},
    runs,
    study_actual::{self as actual, Admission, InstrumentOutput, Job, Request},
};
use setups_domain::AlgorithmParams;
use sqlx::{
    PgPool,
    postgres::{PgConnectOptions, PgPoolOptions},
};
use std::{future::Future, str::FromStr};

const START: i64 = 1_794_000_000;
const CUTOFF: i64 = START + 600;

#[test]
fn compact_last_setup_at_is_nullable_utc_and_visible_without_closed_trades() {
    let mut row = json!({"study_id":1,"status":"completed","totals":{"total":23,"closed":0,"meanR":null},"last_setup_close_unix":START});
    let summary = actual::compact_summary(&row);
    assert_eq!(
        summary["lastSetupAt"],
        json!(DateTime::from_timestamp(START, 0).unwrap())
    );
    assert!(
        summary["meanR"].is_null(),
        "last setup is not restricted to closed trades"
    );
    for status in ["not_calculated", "queued", "running", "failed"] {
        row["status"] = json!(status);
        assert!(
            actual::compact_summary(&row)["lastSetupAt"].is_null(),
            "{status} cannot expose a previous timestamp"
        );
    }
    row["status"] = json!("completed");
    row["totals"]["total"] = json!(0);
    assert!(actual::compact_summary(&row)["lastSetupAt"].is_null());
    row["totals"]["total"] = json!(23);
    row.as_object_mut().unwrap().remove("last_setup_close_unix");
    let legacy = actual::compact_summary(&row);
    assert!(
        legacy.get("lastSetupAt").is_some_and(Value::is_null),
        "legacy rows get an additive nullable field"
    );
}

async fn isolated<F, Fut>(body: F)
where
    F: FnOnce(PgPool) -> Fut + 'static,
    Fut: Future<Output = ()> + 'static,
{
    isolated_upgrade(body, false).await;
}

async fn isolated_upgrade<F, Fut>(body: F, legacy_heads: bool)
where
    F: FnOnce(PgPool) -> Fut + 'static,
    Fut: Future<Output = ()> + 'static,
{
    let options = PgConnectOptions::from_str(
        &std::env::var("TEST_DATABASE_URL").expect("explicit local *_test URL required"),
    )
    .unwrap();
    assert!(options.get_database().unwrap_or("").ends_with("_test"));
    assert!(matches!(
        options.get_host(),
        "localhost" | "127.0.0.1" | "::1"
    ));
    let options = options.options([("statement_timeout", "15000"), ("lock_timeout", "5000")]);
    let admin = PgPoolOptions::new()
        .max_connections(2)
        .connect_with(options.clone())
        .await
        .unwrap();
    let name = format!(
        "actual_lifecycle_{}_{}_test",
        std::process::id(),
        Utc::now().timestamp_nanos_opt().unwrap()
    );
    sqlx::query(&format!("create database {name}"))
        .execute(&admin)
        .await
        .unwrap();
    let connected = PgPoolOptions::new()
        .max_connections(6)
        .connect_with(options.database(&name))
        .await;
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
                if legacy_heads {
                    sqlx::query("insert into kysely_migration values('0029_study_actual_continuous.sql','pre-upgrade-fixture')").execute(&child).await.unwrap();
                    setups_db::migrate::ensure_migrations(&child).await.unwrap();
                    sqlx::raw_sql("insert into optimization_studies(algorithm_id,status,spec,progress,owner_id,execution_started_at) values('price_volume_spike','completed','{}','{}','upgrade',now()),('price_volume_spike','completed','{}','{}','upgrade',now()),('price_volume_spike','completed','{}','{}','upgrade',now()); insert into study_actualizations(owner_id,study_id,job_id,revision,status,stage,execution_started_at,observed_through_unix,source,source_identity,work_minutes,reserved_bytes,requested_at) select 'upgrade',id,'upgrade-'||id,1,case when id=3 then 'running' else 'queued' end,'fixture',now(),0,'{}','fixture',0,1,to_timestamp(100-id) from optimization_studies order by id; delete from kysely_migration where name='0029_study_actual_continuous.sql';")
                        .execute(&child).await.unwrap();
                }
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
        .execute(&admin)
        .await;
    admin.close().await;
    cleanup.expect("owned disposable DB cleanup failed");
    let result =
        outcome.unwrap_or_else(|| panic!("child DB connection failed: {:?}", connected.err()));
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
    Study {
        owner,
        id,
        identity,
        source,
    }
}

async fn admit(
    pool: &PgPool,
    study: &Study,
    name: &str,
    limits: ReplayAdmissionLimits,
) -> Result<Admission, DbError> {
    actual::admit(
        pool,
        Request {
            owner: study.owner,
            study: study.id,
            job: name,
            start: DateTime::from_timestamp(START + 30, 250_000_000).unwrap(),
            cutoff: CUTOFF,
            source: &study.source,
            identity: &study.identity,
            work: 60,
            limits: &limits,
        },
    )
    .await
}

fn created(admission: Admission) -> Value {
    match admission {
        Admission::Created(value) => value,
        other => panic!("expected Created, got {other:?}"),
    }
}

async fn head(pool: &PgPool, study: &Study) -> Value {
    actual::get(pool, study.owner, study.id)
        .await
        .unwrap()
        .unwrap()
}

async fn claim(pool: &PgPool) -> Job {
    actual::claim(pool, 2)
        .await
        .unwrap()
        .expect("queued job should be claimable")
}

fn rows(count: usize) -> Vec<Value> {
    let statuses = ["tp", "sl", "gate_failed", "unclosed", "no_data"];
    (0..count)
        .map(|i| {
            json!({"setupTimeUnix":START + (i as i64 + 1) * 60,
        "status":statuses[i % statuses.len()], "sentinel":i})
        })
        .collect()
}

async fn append(
    pool: &PgPool,
    job: &Job,
    batch: i32,
    symbol: &str,
    rows: &[Value],
) -> Result<bool, DbError> {
    let emitted = json!(rows);
    actual::append(
        pool,
        job,
        batch,
        InstrumentOutput {
            exchange: "bybit",
            category: "linear",
            symbol,
            emitted: &emitted,
            rows,
            quality: &json!({"fixture":"quality"}),
        },
    )
    .await
}

async fn complete(pool: &PgPool, study: &Study, name: &str) -> (Job, i32) {
    created(
        admit(pool, study, name, ReplayAdmissionLimits::default())
            .await
            .unwrap(),
    );
    let job = claim(pool).await;
    assert_eq!(job.study, study.id);
    let batch = actual::create_batch(pool, &job).await.unwrap().unwrap();
    assert!(
        append(pool, &job, batch, "BTCUSDT", &rows(5))
            .await
            .unwrap()
    );
    sqlx::query("insert into replay_quality_intervals(run_id,from_unix,to_unix,reason) select id,$2,$3,'fixture' from replay_runs where batch_id=$1")
        .bind(batch).bind(START).bind(START + 60).execute(pool).await.unwrap();
    assert!(
        actual::finish(
            pool,
            &job,
            &json!({"fixture":"sealed-report"}),
            &json!({"artifactSetupCount":5,"closedCount":2})
        )
        .await
        .unwrap()
    );
    (job, batch)
}

async fn count(pool: &PgPool, query: &str, id: i64) -> i64 {
    sqlx::query_scalar(query)
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn assert_pair_absent(pool: &PgPool, study: i64, batch: i32) {
    assert_eq!(
        count(
            pool,
            "select count(*) from study_actual_setups where study_id=$1",
            study
        )
        .await,
        0
    );
    assert_eq!(
        count(
            pool,
            "select count(*) from replay_runs where batch_id=$1::bigint",
            batch.into()
        )
        .await,
        0
    );
    assert_eq!(
        count(
            pool,
            "select count(*) from replay_batches where id=$1::bigint",
            batch.into()
        )
        .await,
        0
    );
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
    sqlx::query("insert into continuous_owner_lanes(owner_id) values('reservation')")
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("insert into continuous_processes(owner_id,algorithm_id,config) values('reservation','price_volume_spike','{}')").execute(pool).await.unwrap();
    let slot: i64 = sqlx::query_scalar("insert into continuous_slots(owner_id,algorithm_id,min_setups_per_day) values('reservation','price_volume_spike',0) returning id")
        .fetch_one(pool).await.unwrap();
    sqlx::query_scalar("insert into continuous_generations(owner_id,algorithm_id,slot_id,config_revision,lease_token,spec,execution_started_at,observation_cutoff,reserved_bytes) values('reservation','price_volume_spike',$1,1,1,'{}',to_timestamp($2),$3,$4) returning id")
        .bind(slot).bind(CUTOFF as f64).bind(CUTOFF).bind(bytes).fetch_one(pool).await.unwrap()
}

#[tokio::test]
#[ignore = "explicit loopback TEST_DATABASE_URL and CREATEDB required"]
async fn manual_original_archive_terminal_is_atomic_idempotent_owned_and_shared_by_utc_and_iana() {
    use setups_db::study_display::{self, ManualArchive, OriginalMinute};
    async fn finish(
        pool: &PgPool,
        id: i64,
        result: &Value,
        archive: &ManualArchive,
    ) -> Result<(), DbError> {
        optimization::finish_with_original_archive(
            pool,
            id,
            "completed",
            &json!({}),
            Some(result),
            None,
            &json!([]),
            Some(archive),
        )
        .await
    }
    isolated(|pool| async move {
        let spec=json!({"fromUnix":0,"toUnix":172800});
        let scientific=json!({"reason":"trial_budget","best":{"score":0.5,"partial":false,
            "matrix":[{"tpPct":8.0,"slPct":4.0,"tpCount":1,"slCount":1,"closedCount":2}],
            "provenance":{"snapshotHash":"immutable-manual-input","capturedAt":"2026-11-21T00:00:00Z"},
            "analytics":{"clusterEnabled":true,"setupMaturity":1.0,"timeShiftMinutes":2,"horizonMinutes":3}}});
        let id:i64=sqlx::query_scalar("insert into optimization_studies(algorithm_id,status,spec,progress,owner_id,execution_started_at) values('price_volume_spike','running',$1,'{}','admin',to_timestamp($2)) returning id")
            .bind(&spec).bind(START as f64+30.25).fetch_one(&pool).await.unwrap();
        let mut archive=ManualArchive{observation_cutoff:172800,tp_pct:8.0,sl_pct:4.0,kernel:"original-manual-close-1".into(),minutes:vec![
            OriginalMinute{signal_close_unix:86340,sum_r:2.0,closed_count:1,tp_count:1,sl_count:0},
            OriginalMinute{signal_close_unix:86460,sum_r:-1.0,closed_count:1,tp_count:0,sl_count:1},
        ]};
        // A rejected archive rolls back the terminal result as well.
        archive.minutes[0].sum_r=3.0;
        assert!(finish(&pool,id,&scientific,&archive).await.is_err());
        let state:Value=sqlx::query_scalar("select to_jsonb(s) from optimization_studies s where id=$1").bind(id).fetch_one(&pool).await.unwrap();
        assert_eq!(state["status"],"running");assert!(state["result"].is_null());
        archive.minutes[0].sum_r=2.0;
        finish(&pool,id,&scientific,&archive).await.unwrap();
        let sealed:Value=sqlx::query_scalar("select to_jsonb(o) from study_manual_original_outcomes o where study_id=$1").bind(id).fetch_one(&pool).await.unwrap();
        finish(&pool,id,&scientific,&archive).await.unwrap();
        assert_eq!(sqlx::query_scalar::<_,Value>("select to_jsonb(o) from study_manual_original_outcomes o where study_id=$1").bind(id).fetch_one(&pool).await.unwrap(),sealed,"ACK retry cannot rewrite a source, digest, timestamp or projection");
        let result:Value=sqlx::query_scalar("select result from optimization_studies where id=$1").bind(id).fetch_one(&pool).await.unwrap();
        assert_eq!(result,scientific,"no archive blob or fake artifact IDs in science");
        let artifacts:i64=sqlx::query_scalar("select count(*) from study_expectancy_artifacts").fetch_one(&pool).await.unwrap();assert_eq!(artifacts,0);
        let generations:i64=sqlx::query_scalar("select count(*) from continuous_generations").fetch_one(&pool).await.unwrap();assert_eq!(generations,0);
        assert!(study_display::original_observed(&pool,"foreign",id,None,"UTC").await.unwrap().is_none());
        let utc=study_display::original_observed(&pool,"admin",id,None,"UTC").await.unwrap().unwrap();
        assert_eq!(utc.daily.len(),2);assert_eq!(utc.daily[0].mean_r,Some(2.0));assert_eq!(utc.daily[1].mean_r,Some(-1.0));
        let local=study_display::original_observed(&pool,"admin",id,None,"Asia/Yekaterinburg").await.unwrap().unwrap();
        assert_eq!(local.source_identity,utc.source_identity);assert_eq!(local.daily.len(),1);
        assert_eq!((local.daily[0].sum_r,local.daily[0].closed_count,local.daily[0].mean_r),(1.0,2,Some(0.5)));
        let display=study_display::display_snapshot(&pool,"admin",id,"UTC",Some(DateTime::from_timestamp(START,0).unwrap()),None,None).await.unwrap();
        assert!(display.actual_row.is_none());assert_eq!(display.original_identity.as_deref(),Some(utc.source_identity.as_str()));
        assert_eq!(serde_json::to_value(display.original_daily).unwrap(),serde_json::to_value(&utc.daily).unwrap());
        // The genuine retained owner survives dropping/reopening a connection.
        let reopened=PgPoolOptions::new().max_connections(1).connect_with((*pool.connect_options()).clone()).await.unwrap();
        assert_eq!(study_display::original_observed(&reopened,"admin",id,None,"UTC").await.unwrap().unwrap().source_identity,utc.source_identity);reopened.close().await;
        archive.minutes[0].signal_close_unix=86280;
        assert!(finish(&pool,id,&scientific,&archive).await.is_err(),"different assignments with the same totals are not an ACK retry");
        // Quota is global, not per requesting owner. An ACK retry is admitted
        // at capacity, but a new result/archive transaction rolls back.
        sqlx::query("with owners as (insert into optimization_studies(algorithm_id,status,spec,progress,owner_id) select 'price_volume_spike','completed','{}','{}','foreign' from generate_series(1,8191) returning id) insert into study_manual_original_outcomes(study_id,owner_id,source_key,study_identity,kernel,source,minutes,payload_sha256,payload_bytes) select id,'foreign',repeat('1',64),repeat('2',32),'original-manual-close-1','{}','[]',repeat('3',64),2 from owners")
            .execute(&pool).await.unwrap();
        archive.minutes[0].signal_close_unix=86340;
        finish(&pool,id,&scientific,&archive).await.unwrap();
        let second:i64=sqlx::query_scalar("insert into optimization_studies(algorithm_id,status,spec,progress,owner_id,execution_started_at) values('price_volume_spike','running',$1,'{}','admin',to_timestamp($2)) returning id")
            .bind(&spec).bind(START as f64+30.25).fetch_one(&pool).await.unwrap();
        assert!(finish(&pool,second,&scientific,&archive).await.unwrap_err().to_string().contains("ledger_resource_limit"));
        assert_eq!(sqlx::query_as::<_,(String,Option<Value>)>("select status,result from optimization_studies where id=$1").bind(second).fetch_one(&pool).await.unwrap(),("running".into(),None));
        assert_eq!(sqlx::query_scalar::<_,i64>("select count(*) from study_manual_original_outcomes where study_id=$1").bind(second).fetch_one(&pool).await.unwrap(),0);
        optimization::finish(&pool,second,"failed",&json!({}),None,Some(&json!({"code":"original_archive_persistence_failed"})),&json!([])).await.unwrap();
        assert_eq!(sqlx::query_scalar::<_,String>("select status from optimization_studies where id=$1").bind(second).fetch_one(&pool).await.unwrap(),"failed");
        sqlx::query("delete from optimization_studies where id=$1").bind(id).execute(&pool).await.unwrap();
        assert!(study_display::original_observed(&pool,"admin",id,None,"UTC").await.unwrap().is_none());
    }).await;
}

#[tokio::test]
#[ignore = "explicit loopback TEST_DATABASE_URL and CREATEDB required"]
async fn manual_original_archive_ack_uses_jsonb_numeric_equality_without_rewriting_science() {
    use setups_db::study_display::{ManualArchive, OriginalMinute};
    isolated(|pool| async move {
        let exponent=json!(1e-7_f64);
        let decoded:Value=sqlx::query_scalar("select $1::jsonb").bind(&exponent).fetch_one(&pool).await.unwrap();
        let equal:bool=sqlx::query_scalar("select $1::jsonb is not distinct from $2::jsonb")
            .bind(&exponent).bind(&decoded).fetch_one(&pool).await.unwrap();
        eprintln!("JSONB numeric roundtrip: original={exponent}, decoded={decoded}, Rust equality={}, PostgreSQL equality={equal}",exponent==decoded);
        assert_ne!(exponent,decoded,"fixture must exercise arbitrary_precision representation drift");
        assert!(equal);assert_eq!(exponent.as_f64(),decoded.as_f64());
        for elapsed in [1e-7_f64,0.125] {
            for cancelled in [false,true] {
                let progress=json!({"phase":"cleanup","evaluatedCandidates":1,"scorableCandidates":1,
                    "maxTrials":1,"elapsedSeconds":elapsed,"bestScore":0.5});
                let scientific=json!({"progress":progress,"reason":"trial_budget","baseline":null,
                    "invalidProposals":0,"detectorPasses":1,"best":{"score":0.5,"partial":false,
                    "params":setups_domain::SpikeParams::default(),"analytics":{"clusterEnabled":false,"clusterGapMinutes":null,
                        "setupMaturity":null,"timeShiftMinutes":0,"priceChangePct":null,"horizonMinutes":3},
                    "matrix":[{"tpPct":8.0,"slPct":4.0,"riskExpectancy":0.5,"avgMovePct":2.0,
                        "tpCount":1,"slCount":1,"closedCount":2,"unclosedCount":0,"noDataCount":0,"priceChangeFilteredCount":0}],
                    "provenance":{"optimizerVersion":"plan-a-3","seed":42,"snapshotHash":"immutable-manual-input","capturedAt":"2026-11-21T00:00:00Z"},
                    "conclusions":[],"trialIndex":0,"support":{"score":0.5,"setupCount":2,"selectedSetupCount":2,
                        "avgSetupsPerDay":1.0,"requiredSetupCount":0,"evaluatedMinutes":2880,"warmupMinutes":1440}}});
                let spec=json!({"fromUnix":0,"toUnix":172800});
                let id:i64=sqlx::query_scalar("insert into optimization_studies(algorithm_id,status,spec,progress,owner_id,execution_started_at) values('price_volume_spike',$1,$2,'{}','admin',to_timestamp($3)) returning id")
                    .bind(if cancelled {"cancel_requested"}else{"running"}).bind(&spec).bind(START as f64+30.25).fetch_one(&pool).await.unwrap();
                let archive=ManualArchive {observation_cutoff:172800,tp_pct:8.0,sl_pct:4.0,kernel:"original-manual-close-1".into(),minutes:vec![
                    OriginalMinute {signal_close_unix:86340,sum_r:2.0,closed_count:1,tp_count:1,sl_count:0},
                    OriginalMinute {signal_close_unix:86460,sum_r:-1.0,closed_count:1,tp_count:0,sl_count:1},
                ]};
                let events=json!([{ "kind":"completed","message":"fixture","at":"2026-11-21T00:00:00Z" }]);
                optimization::finish_with_original_archive(&pool,id,"completed",&progress,Some(&scientific),None,&events,Some(&archive)).await.unwrap();
                let stored:Value=sqlx::query_scalar("select to_jsonb(s) from optimization_studies s where id=$1").bind(id).fetch_one(&pool).await.unwrap();
                let sealed:Value=sqlx::query_scalar("select to_jsonb(o) from study_manual_original_outcomes o where study_id=$1").bind(id).fetch_one(&pool).await.unwrap();
                assert_eq!(stored["status"],if cancelled {"cancelled"}else{"completed"});
                assert_eq!(stored["result"]["best"]["partial"],cancelled);
                assert_eq!(stored["result"]["reason"],if cancelled {"cancelled"}else{"trial_budget"});
                let retry=optimization::finish_with_original_archive(&pool,id,"completed",&progress,Some(&scientific),None,&events,Some(&archive)).await;
                assert!(retry.is_ok(),"identical ACK retry failed: elapsed={elapsed}, cancelled={cancelled}, result={retry:?}");
                let mut changed=scientific.clone();changed["progress"]["elapsedSeconds"]=json!(elapsed*2.0);
                assert!(optimization::finish_with_original_archive(&pool,id,"completed",&progress,Some(&changed),None,&events,Some(&archive)).await.unwrap_err().to_string().contains("study_display_manual_terminal_conflict"));
                assert_eq!(sqlx::query_scalar::<_,Value>("select to_jsonb(s) from optimization_studies s where id=$1").bind(id).fetch_one(&pool).await.unwrap(),stored,"retry/conflict must not rewrite science or timestamps");
                assert_eq!(sqlx::query_scalar::<_,Value>("select to_jsonb(o) from study_manual_original_outcomes o where study_id=$1").bind(id).fetch_one(&pool).await.unwrap(),sealed,"retry/conflict must preserve archive source, hashes and createdAt");
            }
        }
    }).await;
}

#[tokio::test]
#[ignore = "explicit loopback TEST_DATABASE_URL and CREATEDB required"]
async fn concurrent_same_study_admission_and_claim_have_one_current_job() {
    isolated(|pool| async move {
        let study = seed(&pool, "admin", "same-study").await;
        let foreign = seed(&pool, "foreign", "foreign").await;
        let limits = ReplayAdmissionLimits::default();
        let (left, right) = tokio::join!(
            admit(&pool, &study, "left", limits),
            admit(&pool, &study, "right", limits)
        );
        let (accepted, busy) = match (left.unwrap(), right.unwrap()) {
            (Admission::Created(a), Admission::Busy(b))
            | (Admission::Busy(b), Admission::Created(a)) => (a, b),
            other => panic!("exactly one Created and one Busy required: {other:?}"),
        };
        assert_eq!(accepted["revision"], 1);
        assert_eq!(
            accepted, busy,
            "Busy must return the same current queued head"
        );
        assert!(
            accepted.get("source").is_none(),
            "private frozen source must not be exposed"
        );
        let (left, right) = tokio::join!(actual::claim(&pool, 2), actual::claim(&pool, 2));
        assert_eq!(
            usize::from(left.unwrap().is_some()) + usize::from(right.unwrap().is_some()),
            1
        );
        assert_eq!(head(&pool, &study).await["status"], "running");
        assert!(
            actual::get(&pool, foreign.owner, study.id)
                .await
                .unwrap()
                .is_none()
        );
        let mut impostor = study.clone();
        impostor.owner = foreign.owner;
        assert!(matches!(
            admit(&pool, &impostor, "impostor", limits).await.unwrap(),
            Admission::SourceChanged
        ));
        let mut changed = foreign.clone();
        changed.identity = "wrong-frozen-source".into();
        assert!(matches!(
            admit(&pool, &changed, "changed", limits).await.unwrap(),
            Admission::SourceChanged
        ));
        assert!(
            actual::get(&pool, foreign.owner, foreign.id)
                .await
                .unwrap()
                .is_none()
        );
    })
    .await;
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
async fn rejected_or_rolled_back_replacement_preserves_the_entire_published_pair() {
    async fn snapshot(pool: &PgPool, study: i64) -> Value {
        sqlx::query_scalar("select jsonb_build_object(\
            'study',(select to_jsonb(s) from optimization_studies s where id=$1),\
            'head',(select to_jsonb(a) from study_actualizations a where study_id=$1 and owner_id='admin'),\
            'rows',(select jsonb_agg(to_jsonb(s) order by id) from study_actual_setups s where study_id=$1 and owner_id='admin'),\
            'runs',(select jsonb_agg(to_jsonb(r) order by id) from replay_runs r where actual_study_id=$1 and owner_id='admin'),\
            'batches',(select jsonb_agg(to_jsonb(b) order by id) from replay_batches b where actual_study_id=$1 and owner_id='admin'),\
            'quality',(select jsonb_agg(to_jsonb(q) order by run_id,from_unix) from replay_quality_intervals q join replay_runs r on r.id=q.run_id where r.actual_study_id=$1 and r.owner_id='admin'))")
            .bind(study).fetch_one(pool).await.unwrap()
    }
    isolated(|pool| async move {
        let study = seed(&pool, "admin", "rejected-replacement").await;
        let foreign = seed(&pool, "foreign", "protected-foreign").await;
        seed_sentinels(&pool).await;
        let (_, old_batch) = complete(&pool, &study, "published").await;
        complete(&pool, &foreign, "reserved-foreign-job").await;
        let published = snapshot(&pool, study.id).await;
        let protected_before = protected(&pool).await;
        assert!(matches!(
            admit(
                &pool,
                &study,
                "no-capacity",
                ReplayAdmissionLimits {
                    max_pending_runs: 0,
                    ..Default::default()
                }
            )
            .await
            .unwrap(),
            Admission::Resource
        ));
        assert_eq!(snapshot(&pool, study.id).await, published);
        let mut invalid = study.clone();
        invalid.identity = "changed-source".into();
        assert!(matches!(
            admit(&pool, &invalid, "invalid", Default::default())
                .await
                .unwrap(),
            Admission::SourceChanged
        ));
        assert_eq!(snapshot(&pool, study.id).await, published);
        // The unique job collision occurs AFTER cleanup has deleted the old
        // rows, runs, quality and batch. The complete transaction must roll back.
        assert!(
            admit(&pool, &study, "reserved-foreign-job", Default::default())
                .await
                .is_err()
        );
        assert_eq!(
            snapshot(&pool, study.id).await,
            published,
            "late admission error must restore every published byte and pointer"
        );
        assert_eq!(protected(&pool).await, protected_before);
        let accepted = created(
            admit(&pool, &study, "accepted", Default::default())
                .await
                .unwrap(),
        );
        assert_eq!(accepted["revision"], 2);
        for key in ["batch_id", "report", "totals"] {
            assert!(
                accepted[key].is_null(),
                "accepted replacement must hide {key}"
            );
        }
        assert_pair_absent(&pool, study.id, old_batch).await;
        let job = claim(&pool).await;
        let batch = actual::create_batch(&pool, &job).await.unwrap().unwrap();
        assert!(
            append(&pool, &job, batch, "CURRENT", &rows(3))
                .await
                .unwrap()
        );
        let staging = snapshot(&pool, study.id).await;
        assert!(matches!(
            admit(&pool, &study, "busy", Default::default())
                .await
                .unwrap(),
            Admission::Busy(_)
        ));
        assert_eq!(
            snapshot(&pool, study.id).await,
            staging,
            "busy cannot clean current staging"
        );
        assert!(
            actual::page(&pool, study.owner, study.id, 2, 0, 100)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            actual::finish(&pool, &job, &json!({"revision":2}), &json!({"total":3}))
                .await
                .unwrap()
        );
        assert_eq!(
            actual::page(&pool, study.owner, study.id, 2, 0, 100)
                .await
                .unwrap()
                .unwrap()
                .len(),
            3
        );
        assert_eq!(protected(&pool).await, protected_before);
        assert_eq!(
            snapshot(&pool, study.id).await["study"],
            published["study"],
            "original study/spec/result/model/history are never refresh-owned"
        );
    })
    .await;
}

#[tokio::test]
#[ignore = "explicit loopback TEST_DATABASE_URL and CREATEDB required"]
async fn recovery_preserves_queued_and_fails_fenced_running_with_owned_cleanup() {
    isolated(|pool| async move {
        let running = seed(&pool, "admin", "running").await;
        let queued = seed(&pool, "foreign", "queued").await;
        seed_sentinels(&pool).await;
        created(
            admit(&pool, &running, "running", ReplayAdmissionLimits::default())
                .await
                .unwrap(),
        );
        let job = claim(&pool).await;
        let batch = actual::create_batch(&pool, &job).await.unwrap().unwrap();
        assert!(
            append(&pool, &job, batch, "STAGING", &rows(3))
                .await
                .unwrap()
        );
        created(
            admit(&pool, &queued, "queued", ReplayAdmissionLimits::default())
                .await
                .unwrap(),
        );
        let queued_before = head(&pool, &queued).await;
        let protected_before = protected(&pool).await;
        actual::recover(&pool).await.unwrap();
        actual::recover(&pool).await.unwrap();
        assert_eq!(head(&pool, &queued).await, queued_before);
        let failed = head(&pool, &running).await;
        assert_eq!(failed["status"], "failed");
        assert_eq!(
            failed["error"]["code"],
            "study_actual_interrupted_by_restart"
        );
        assert_eq!(failed["reserved_bytes"], 0);
        assert!(failed["report"].is_null() && failed["batch_id"].is_null());
        assert!(failed["token"].as_i64().unwrap() > job.token);
        assert_pair_absent(&pool, running.id, batch).await;
        assert!(
            !actual::progress(&pool, &job, "late", &json!({}))
                .await
                .unwrap()
        );
        assert!(!append(&pool, &job, batch, "LATE", &rows(1)).await.unwrap());
        assert!(
            !actual::finish(&pool, &job, &json!({}), &json!({}))
                .await
                .unwrap()
        );
        actual::fail(&pool, &job, "late").await.unwrap();
        assert_eq!(head(&pool, &running).await, failed);
        assert_eq!(protected(&pool).await, protected_before);
        let recovered = claim(&pool).await;
        assert_eq!(recovered.study, queued.id);
        assert_eq!(recovered.job_id, "queued");
        assert_eq!(recovered.cutoff, CUTOFF);
        assert_eq!(recovered.revision, 1);
    })
    .await;
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
            .bind(study.id)
            .bind(foreign_batch)
            .execute(&pool)
            .await
            .unwrap();
        let before = head(&pool, &study).await;
        let protected_before = protected(&pool).await;
        let error = admit(
            &pool,
            &study,
            "must-rollback",
            ReplayAdmissionLimits::default(),
        )
        .await
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("study_actual_owned_pair_mismatch"),
            "{error}"
        );
        assert_eq!(
            head(&pool, &study).await,
            before,
            "batch pointer update must roll back"
        );
        assert_eq!(
            count(
                &pool,
                "select count(*) from study_actual_setups where study_id=$1",
                study.id
            )
            .await,
            5,
            "row deletion must roll back"
        );
        assert_eq!(
            count(
                &pool,
                "select count(*) from replay_batches where id=$1::bigint",
                owned_batch.into()
            )
            .await,
            1
        );
        assert_eq!(
            count(
                &pool,
                "select count(*) from replay_runs where batch_id=$1::bigint",
                owned_batch.into()
            )
            .await,
            1
        );
        assert_eq!(protected(&pool).await, protected_before);
    })
    .await;
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

use actual::continuous as scheduler;
async fn enabled(pool: &PgPool, owner: &str, algorithm: &str) -> scheduler::Settings {
    let settings = scheduler::ensure(pool, owner, algorithm).await.unwrap();
    match scheduler::configure(
        pool,
        owner,
        algorithm,
        settings.revision,
        true,
        "1970-01-01",
        "UTC",
    )
    .await
    .unwrap()
    {
        scheduler::Configure::Saved(s) => s,
        _ => panic!("configuration rejected"),
    }
}
async fn due(pool: &PgPool, owner: &str) {
    sqlx::query("update study_actual_continuous set next_due_at=clock_timestamp()-interval '1 second' where owner_id=$1").bind(owner).execute(pool).await.unwrap();
}
async fn auto(pool: &PgPool, ticket: &scheduler::Ticket, study: &Study, job: &str) -> Admission {
    scheduler::admit(
        pool,
        ticket,
        Request {
            owner: study.owner,
            study: study.id,
            job,
            start: ticket
                .candidates
                .iter()
                .find(|c| c.study == study.id)
                .unwrap()
                .start,
            cutoff: ticket.cutoff,
            source: &study.source,
            identity: &study.identity,
            work: 60,
            limits: &Default::default(),
        },
    )
    .await
    .unwrap()
}

#[tokio::test]
#[ignore = "explicit loopback TEST_DATABASE_URL and CREATEDB required"]
async fn continuous_migration_applies_to_real_0028_and_preserves_durable_legacy_heads() {
    isolated_upgrade(|pool|async move {
        let rows:Vec<(i64,String,i64,String)>=sqlx::query_as("select study_id,status,queue_sequence,request_origin from study_actualizations order by queue_sequence").fetch_all(&pool).await.unwrap();
        assert_eq!(rows.iter().map(|r|r.0).collect::<Vec<_>>(),vec![3,2,1],"bigserial upgrade must restore requested FIFO, not physical insertion order");
        assert_eq!(rows.iter().map(|r|r.1.as_str()).collect::<Vec<_>>(),vec!["running","queued","queued"]);
        assert!(rows.iter().all(|r|r.3=="manual"));
        let retained:Vec<(String,i64,i64,Value,i64)>=sqlx::query_as("select job_id,revision,observed_through_unix,source,reserved_bytes from study_actualizations order by study_id").fetch_all(&pool).await.unwrap();
        assert_eq!(retained,(1..=3).map(|id|(format!("upgrade-{id}"),1,0,json!({}),1)).collect::<Vec<_>>(),"upgrade must not rewrite accepted identity/revision/C/source/reservation");
        let has_exact_column:bool=sqlx::query_scalar("select exists(select 1 from information_schema.columns where table_name='optimization_studies' and column_name='execution_started_at')").fetch_one(&pool).await.unwrap();
        assert!(has_exact_column,"0026 adds the exact clock before 0029 indexes it");
        assert!(sqlx::query("update study_actualizations set status='running' where study_id=2").execute(&pool).await.is_err(),"DB trigger also protects old claim queries during rolling overlap");
        actual::recover(&pool).await.unwrap();
        let job=claim(&pool).await;assert_eq!(job.study,2);actual::fail(&pool,&job,"fixture").await.unwrap();
        let second=claim(&pool).await;assert_eq!(second.study,1);
        let max:i64=sqlx::query_scalar("select max(queue_sequence) from study_actualizations").fetch_one(&pool).await.unwrap();
        let next:i64=sqlx::query_scalar("select nextval(pg_get_serial_sequence('study_actualizations','queue_sequence'))").fetch_one(&pool).await.unwrap();assert!(next>max);
    },true).await;
}

#[tokio::test]
#[ignore = "explicit loopback TEST_DATABASE_URL and CREATEDB required"]
async fn continuous_defaults_are_persistent_owner_isolated_and_share_api_headless_resolver() {
    isolated(|pool|async move {
        setups_db::settings_set(&pool,"ui",&json!({"interface":{"timezone":"Asia/Kathmandu"}})).await.unwrap();
        scheduler::bootstrap(&pool).await.unwrap();
        let first=scheduler::ensure(&pool,"admin","price_volume_spike").await.unwrap();
        assert!(first.enabled);assert_eq!(first.time_zone,"Asia/Kathmandu");
        let expected:(chrono::NaiveDate,chrono::DateTime<Utc>)=sqlx::query_as("select timezone('Asia/Kathmandu',clock_timestamp())::date,to_timestamp(study_local_day_start('Asia/Kathmandu',timezone('Asia/Kathmandu',clock_timestamp())::date))").fetch_one(&pool).await.unwrap();
        assert_eq!((first.from_date,first.from_boundary),expected);
        setups_db::settings_set(&pool,"ui",&json!({"interface":{"timezone":"America/New_York"}})).await.unwrap();
        scheduler::bootstrap(&pool).await.unwrap();
        let repeated=scheduler::ensure(&pool,"admin","price_volume_spike").await.unwrap();
        assert_eq!(serde_json::to_value(&first).unwrap(),serde_json::to_value(&repeated).unwrap());
        let foreign=scheduler::ensure(&pool,"foreign","price_volume_spike").await.unwrap();assert_eq!(foreign.time_zone,"UTC");
        let configured=enabled(&pool,"admin","price_volume_spike").await;
        assert_ne!(configured.from_boundary,first.from_boundary);
        assert_eq!(scheduler::ensure(&pool,"foreign","price_volume_spike").await.unwrap().revision,1);
        assert_eq!(scheduler::ensure(&pool,"admin","price_drop_volume_spike").await.unwrap().revision,1);
        assert!(matches!(scheduler::configure(&pool,"admin","price_volume_spike",1,false,"1970-01-01","UTC").await.unwrap(),scheduler::Configure::Conflict(_)));
        for (day,zone) in [("2026-02-30","UTC"),("2026-2-01","UTC"),("1969-12-31","UTC"),("2026-11-22","Mars/Olympus")] {
            assert!(matches!(scheduler::configure(&pool,"admin","price_volume_spike",configured.revision,true,day,zone).await.unwrap(),scheduler::Configure::Invalid(_)));
        }
    }).await;
}

#[tokio::test]
#[ignore = "explicit loopback TEST_DATABASE_URL and CREATEDB required"]
async fn automatic_completed_exact_date_order_ties_new_study_wrap_and_cutoff_backoff() {
    isolated(|pool|async move {
        let a=seed(&pool,"admin","a").await;let b=seed(&pool,"admin","b").await;
        let cancelled=seed(&pool,"admin","cancelled").await;let partial=seed(&pool,"admin","partial").await;
        let before=seed(&pool,"admin","before").await;let malformed=seed(&pool,"admin","malformed").await;
        sqlx::query("update optimization_studies set status='cancelled' where id=$1").bind(cancelled.id).execute(&pool).await.unwrap();
        sqlx::query("update optimization_studies set result=jsonb_set(result,'{best}','{\"partial\":true}') where id=$1").bind(partial.id).execute(&pool).await.unwrap();
        sqlx::query("update optimization_studies set execution_started_at=to_timestamp(0) where id=$1").bind(before.id).execute(&pool).await.unwrap();
        sqlx::query("update optimization_studies set execution_started_at=null,events='[{\"kind\":\"started\",\"at\":\"bad\"}]' where id=$1").bind(malformed.id).execute(&pool).await.unwrap();
        // Seed helper's scientific result intentionally has no best; add just
        // the DB coarse eligibility marker, then recompute exact identity.
        for study in [&a,&b,&cancelled,&before,&malformed] {sqlx::query("update optimization_studies set result=result||'{\"best\":{\"partial\":false}}' where id=$1").bind(study.id).execute(&pool).await.unwrap();}
        let refresh_identity=|study:Study,pool:PgPool|async move {let mut study=study;study.identity=sqlx::query_scalar("select md5(spec::text||coalesce(result::text,'null')) from optimization_studies where id=$1").bind(study.id).fetch_one(&pool).await.unwrap();study};
        let a=refresh_identity(a,pool.clone()).await;let b=refresh_identity(b,pool.clone()).await;
        let config=enabled(&pool,"admin","price_volume_spike").await;
        let day=DateTime::from_timestamp(START,0).unwrap().date_naive().to_string();
        assert!(matches!(scheduler::configure(&pool,"admin","price_volume_spike",config.revision,true,&day,"UTC").await.unwrap(),scheduler::Configure::Saved(_)));
        let t=scheduler::next(&pool).await.unwrap().unwrap();assert_eq!(t.candidates.iter().map(|c|c.study).collect::<Vec<_>>(),vec![a.id,b.id]);
        let limited=setups_db::admission::ReplayAdmissionLimits{max_pending_runs:0,..Default::default()};
        assert!(matches!(scheduler::admit(&pool,&t,Request{owner:"admin",study:a.id,job:"global-shortage",start:t.candidates[0].start,cutoff:t.cutoff,source:&a.source,identity:&a.identity,work:60,limits:&limited}).await.unwrap(),Admission::Resource));
        scheduler::backoff(&pool,&t,"resource_limit").await.unwrap();
        let scan:Option<i64>=sqlx::query_scalar("select scan_study_id from study_actual_continuous where owner_id='admin'").fetch_one(&pool).await.unwrap();assert!(scan.is_none(),"global shortage cannot advance through uninspected candidates");
        assert!(scheduler::next(&pool).await.unwrap().is_none());
        due(&pool,"admin").await;
        created(auto(&pool,&t,&a,"auto-a").await);let job=claim(&pool).await;actual::fail(&pool,&job,"expected").await.unwrap();
        let state=scheduler::ensure(&pool,"admin","price_volume_spike").await.unwrap();assert_eq!(state.cursor_study_id,Some(a.id));
        let t=scheduler::next(&pool).await.unwrap().unwrap();assert_eq!(t.candidates[0].study,b.id);
        created(auto(&pool,&t,&b,"auto-b").await);let job=claim(&pool).await;actual::fail(&pool,&job,"expected").await.unwrap();
        assert!(scheduler::next(&pool).await.unwrap().is_none(),"completed last study persists next-minute backoff");
        // A newly completed later study is discovered before wrap, regardless
        // of created/updated clocks and without materializing a cycle.
        let mut c=seed(&pool,"admin","new").await;
        sqlx::query("update optimization_studies set result=result||'{\"best\":{\"partial\":false}}',execution_started_at=to_timestamp($2),created_at=to_timestamp(0) where id=$1").bind(c.id).bind((START+31) as f64).execute(&pool).await.unwrap();
        c.identity=sqlx::query_scalar("select md5(spec::text||coalesce(result::text,'null')) from optimization_studies where id=$1").bind(c.id).fetch_one(&pool).await.unwrap();
        due(&pool,"admin").await;let t=scheduler::next(&pool).await.unwrap().unwrap();assert_eq!(t.candidates[0].study,c.id);
        created(auto(&pool,&t,&c,"auto-c").await);let job=claim(&pool).await;actual::fail(&pool,&job,"expected").await.unwrap();
        due(&pool,"admin").await;let t=scheduler::next(&pool).await.unwrap().unwrap();assert!(t.wrapped);assert_eq!(t.candidates[0].study,a.id);
        let old=head(&pool,&a).await;assert!(matches!(auto(&pool,&t,&a,"duplicate-cutoff").await,Admission::Deferred));assert_eq!(head(&pool,&a).await,old);
        assert!(scheduler::next(&pool).await.unwrap().is_none());
        // Same-cutoff suppression is automatic only.
        created(admit(&pool,&a,"manual-same-cutoff",Default::default()).await.unwrap());let manual=claim(&pool).await;
        actual::fail(&pool,&manual,"manual").await.unwrap();assert_eq!(scheduler::ensure(&pool,"admin","price_volume_spike").await.unwrap().cursor_study_id,Some(c.id));
    }).await;
}

async fn automatic_seed(pool: &PgPool, label: &str) -> Study {
    let mut study = seed(pool, "admin", label).await;
    sqlx::query("update optimization_studies set result=result||'{\"best\":{\"partial\":false}}' where id=$1").bind(study.id).execute(pool).await.unwrap();
    study.identity=sqlx::query_scalar("select md5(spec::text||coalesce(result::text,'null')) from optimization_studies where id=$1").bind(study.id).fetch_one(pool).await.unwrap();
    study
}

#[tokio::test]
#[ignore = "explicit loopback TEST_DATABASE_URL and CREATEDB required"]
async fn owner_lane_two_coordinators_manual_fifo_priority_and_atomic_terminal_cursor() {
    isolated(|pool|async move {
        let a=automatic_seed(&pool,"a").await;let b=automatic_seed(&pool,"b").await;let c=automatic_seed(&pool,"c").await;
        enabled(&pool,"admin","price_volume_spike").await;
        let t=scheduler::next(&pool).await.unwrap().unwrap();created(auto(&pool,&t,&a,"automatic-first").await);
        // A later manual request outranks an already committed/unclaimed auto.
        created(admit(&pool,&b,"manual-first",Default::default()).await.unwrap());
        let (left,right)=tokio::join!(actual::claim(&pool,2),actual::claim(&pool,2));
        let claims:Vec<_>=[left.unwrap(),right.unwrap()].into_iter().flatten().collect();assert_eq!(claims.len(),1);assert_eq!(claims[0].study,b.id);
        actual::fail(&pool,&claims[0],"manual").await.unwrap();
        assert!(scheduler::ensure(&pool,"admin","price_volume_spike").await.unwrap().cursor_study_id.is_none());
        let running=claim(&pool).await;assert_eq!(running.study,a.id);
        created(admit(&pool,&b,"fifo-1",Default::default()).await.unwrap());created(admit(&pool,&c,"fifo-2",Default::default()).await.unwrap());
        assert!(actual::claim(&pool,2).await.unwrap().is_none(),"running auto is not interrupted by manual");
        assert!(scheduler::next(&pool).await.unwrap().is_none());
        let batch=actual::create_batch(&pool,&running).await.unwrap().unwrap();append(&pool,&running,batch,"FIX",&rows(1)).await.unwrap();
        // Publication and cursor advancement MUST roll back together on a
        // fault after the completed-head update. Retry is the same job/C.
        sqlx::raw_sql("create function cursor_fixture() returns trigger language plpgsql as $$begin if new.cursor_study_id is not null then raise exception 'cursor fixture';end if;return new;end$$;create trigger cursor_fixture before update on study_actual_continuous for each row execute function cursor_fixture();").execute(&pool).await.unwrap();
        assert!(actual::finish(&pool,&running,&json!({}),&json!({})).await.is_err());assert_eq!(head(&pool,&a).await["status"],"running");
        sqlx::query("drop trigger cursor_fixture on study_actual_continuous").execute(&pool).await.unwrap();
        assert!(actual::finish(&pool,&running,&json!({}),&json!({})).await.unwrap());
        let sealed=serde_json::to_value(scheduler::ensure(&pool,"admin","price_volume_spike").await.unwrap()).unwrap();assert_eq!(sealed["cursorStudyId"],a.id);
        actual::fail(&pool,&running,"uncertain-ack").await.unwrap();assert!(!actual::finish(&pool,&running,&json!({}),&json!({})).await.unwrap());
        assert_eq!(serde_json::to_value(scheduler::ensure(&pool,"admin","price_volume_spike").await.unwrap()).unwrap(),sealed);
        for expected in [b.id,c.id] {let job=claim(&pool).await;assert_eq!(job.study,expected);actual::fail(&pool,&job,"manual").await.unwrap();}
        assert_eq!(serde_json::to_value(scheduler::ensure(&pool,"admin","price_volume_spike").await.unwrap()).unwrap(),sealed);
        due(&pool,"admin").await;assert_eq!(scheduler::next(&pool).await.unwrap().unwrap().candidates[0].study,b.id);
    }).await;
}

#[tokio::test]
#[ignore = "explicit loopback TEST_DATABASE_URL and CREATEDB required"]
async fn config_revision_fences_loading_queued_and_running_without_interrupting_manual() {
    isolated(|pool| async move {
        let a = automatic_seed(&pool, "a").await;
        let b = automatic_seed(&pool, "b").await;
        enabled(&pool, "admin", "price_volume_spike").await;
        let t = scheduler::next(&pool).await.unwrap().unwrap();
        scheduler::configure(
            &pool,
            "admin",
            "price_volume_spike",
            t.settings.revision,
            false,
            "1970-01-01",
            "UTC",
        )
        .await
        .unwrap();
        assert!(matches!(
            auto(&pool, &t, &a, "stale-loading").await,
            Admission::Deferred
        ));
        assert!(actual::get(&pool, "admin", a.id).await.unwrap().is_none());
        enabled(&pool, "admin", "price_volume_spike").await;
        let t = scheduler::next(&pool).await.unwrap().unwrap();
        created(auto(&pool, &t, &a, "stale-queued").await);
        scheduler::configure(
            &pool,
            "admin",
            "price_volume_spike",
            t.settings.revision,
            false,
            "1970-01-01",
            "UTC",
        )
        .await
        .unwrap();
        assert!(actual::claim(&pool, 2).await.unwrap().is_none());
        assert_eq!(head(&pool, &a).await["status"], "failed");
        enabled(&pool, "admin", "price_volume_spike").await;
        // Previous queued cutoff remains deduped; a different eligible study runs.
        let t = scheduler::next(&pool).await.unwrap().unwrap();
        created(auto(&pool, &t, &b, "running-config").await);
        let job = claim(&pool).await;
        let old = scheduler::ensure(&pool, "admin", "price_volume_spike")
            .await
            .unwrap();
        scheduler::configure(
            &pool,
            "admin",
            "price_volume_spike",
            old.revision,
            true,
            "1970-01-02",
            "Asia/Kathmandu",
        )
        .await
        .unwrap();
        created(
            admit(&pool, &a, "manual-survives", Default::default())
                .await
                .unwrap(),
        );
        actual::finish(&pool, &job, &json!({}), &json!({}))
            .await
            .unwrap();
        assert_eq!(head(&pool, &b).await["status"], "completed");
        let changed = scheduler::ensure(&pool, "admin", "price_volume_spike")
            .await
            .unwrap();
        assert!(changed.cursor_study_id.is_none());
        assert!(changed.reset_pending);
        let manual = claim(&pool).await;
        assert_eq!(manual.study, a.id);
        actual::fail(&pool, &manual, "fixture").await.unwrap();
        assert!(
            scheduler::ensure(&pool, "admin", "price_volume_spike")
                .await
                .unwrap()
                .reset_pending
        );
        due(&pool, "admin").await;
        let _ = scheduler::next(&pool).await.unwrap();
        assert!(
            !scheduler::ensure(&pool, "admin", "price_volume_spike")
                .await
                .unwrap()
                .reset_pending
        );
        assert!(
            scheduler::ensure(&pool, "admin", "price_volume_spike")
                .await
                .unwrap()
                .cursor_study_id
                .is_none(),
            "the new date resets the cursor only before the next automatic scan"
        );
    })
    .await;
}

#[tokio::test]
#[ignore = "explicit loopback TEST_DATABASE_URL and CREATEDB required"]
async fn maintenance_preserves_queued_auto_across_recovery_resume_and_waits_for_running() {
    isolated(|pool| async move {
        use setups_db::continuous::maintenance;
        let a = automatic_seed(&pool, "a").await;
        let b = automatic_seed(&pool, "b").await;
        enabled(&pool, "admin", "price_volume_spike").await;
        let t = scheduler::next(&pool).await.unwrap().unwrap();
        created(auto(&pool, &t, &a, "durable-auto").await);
        let queued = head(&pool, &a).await;
        let status = maintenance::set_paused(&pool, "admin", true).await.unwrap();
        assert_eq!(status.state, "drained");
        assert!(actual::claim(&pool, 2).await.unwrap().is_none());
        assert!(scheduler::next(&pool).await.unwrap().is_none());
        actual::recover(&pool).await.unwrap();
        assert_eq!(
            head(&pool, &a).await,
            queued,
            "pause/restart preserve job, revision, cutoff, fence and reserved bytes"
        );
        created(
            admit(&pool, &b, "manual-paused", Default::default())
                .await
                .unwrap(),
        );
        let status = maintenance::status(&pool, "admin").await.unwrap();
        assert_eq!(status.state, "drained");
        assert_eq!(status.active_manual_actual, 1);
        let manual = claim(&pool).await;
        assert_eq!(manual.study, b.id);
        actual::fail(&pool, &manual, "manual").await.unwrap();
        assert_eq!(head(&pool, &a).await, queued);
        maintenance::set_paused(&pool, "admin", false)
            .await
            .unwrap();
        let running = claim(&pool).await;
        assert_eq!(running.job_id, "durable-auto");
        assert_eq!(
            running.cutoff,
            queued["observed_through_unix"].as_i64().unwrap()
        );
        let status = maintenance::set_paused(&pool, "admin", true).await.unwrap();
        assert_eq!(status.state, "draining");
        assert_eq!(status.running_automatic_actual, 1);
        actual::recover(&pool).await.unwrap();
        let failed = head(&pool, &a).await;
        assert_eq!(failed["status"], "failed");
        assert_eq!(
            scheduler::ensure(&pool, "admin", "price_volume_spike")
                .await
                .unwrap()
                .cursor_study_id,
            Some(a.id)
        );
        assert_eq!(
            maintenance::status(&pool, "admin").await.unwrap().state,
            "drained"
        );
        assert!(
            !actual::finish(&pool, &running, &json!({}), &json!({}))
                .await
                .unwrap()
        );
        maintenance::set_paused(&pool, "admin", false)
            .await
            .unwrap();
        due(&pool, "admin").await;
        let ticket = scheduler::next(&pool).await.unwrap().unwrap();
        let (paused, admission) = tokio::join!(
            maintenance::set_paused(&pool, "admin", true),
            auto(&pool, &ticket, &b, "drain-race")
        );
        paused.unwrap();
        assert!(matches!(
            admission,
            Admission::Created(_) | Admission::Deferred
        ));
        assert!(
            actual::claim(&pool, 2).await.unwrap().is_none(),
            "no auto can acquire ownership after committed drain"
        );
        assert_eq!(
            maintenance::status(&pool, "admin").await.unwrap().state,
            "drained"
        );
    })
    .await;
}

#[tokio::test]
#[ignore = "explicit loopback TEST_DATABASE_URL and CREATEDB required"]
async fn enable_only_noop_and_restart_terminal_preserve_automatic_position() {
    isolated(|pool| async move {
        let a = automatic_seed(&pool, "a").await;
        let b = automatic_seed(&pool, "b").await;
        let c = automatic_seed(&pool, "c").await;
        let d = automatic_seed(&pool, "d").await;
        enabled(&pool, "admin", "price_volume_spike").await;
        let t = scheduler::next(&pool).await.unwrap().unwrap();
        created(auto(&pool, &t, &a, "a").await);
        let job = claim(&pool).await;
        actual::fail(&pool, &job, "a").await.unwrap();
        let t = scheduler::next(&pool).await.unwrap().unwrap();
        created(auto(&pool, &t, &b, "b").await);
        let job = claim(&pool).await;
        scheduler::configure(
            &pool,
            "admin",
            "price_volume_spike",
            t.settings.revision,
            false,
            "1970-01-01",
            "UTC",
        )
        .await
        .unwrap();
        actual::finish(&pool, &job, &json!({}), &json!({}))
            .await
            .unwrap();
        assert_eq!(
            scheduler::ensure(&pool, "admin", "price_volume_spike")
                .await
                .unwrap()
                .cursor_study_id,
            Some(b.id)
        );
        let reenabled = enabled(&pool, "admin", "price_volume_spike").await;
        assert_eq!(reenabled.cursor_study_id, Some(b.id));
        let t = scheduler::next(&pool).await.unwrap().unwrap();
        assert_eq!(t.candidates[0].study, c.id);
        created(auto(&pool, &t, &c, "c").await);
        let job = claim(&pool).await;
        scheduler::configure(
            &pool,
            "admin",
            "price_volume_spike",
            t.settings.revision,
            true,
            "1970-01-01",
            "UTC",
        )
        .await
        .unwrap();
        actual::fail(&pool, &job, "noop").await.unwrap();
        assert_eq!(
            scheduler::ensure(&pool, "admin", "price_volume_spike")
                .await
                .unwrap()
                .cursor_study_id,
            Some(c.id)
        );
        let t = scheduler::next(&pool).await.unwrap().unwrap();
        created(auto(&pool, &t, &d, "d").await);
        let _job = claim(&pool).await;
        scheduler::configure(
            &pool,
            "admin",
            "price_volume_spike",
            t.settings.revision,
            false,
            "1970-01-01",
            "UTC",
        )
        .await
        .unwrap();
        actual::recover(&pool).await.unwrap();
        assert_eq!(
            scheduler::ensure(&pool, "admin", "price_volume_spike")
                .await
                .unwrap()
                .cursor_study_id,
            Some(d.id)
        );
    })
    .await;
}

#[tokio::test]
#[ignore = "explicit loopback TEST_DATABASE_URL and CREATEDB required"]
async fn two_algorithm_turns_are_fair_and_exact_event_start_keys_preserve_nanoseconds() {
    isolated(|pool| async move {
        let a = automatic_seed(&pool, "a").await;
        let mut b = automatic_seed(&pool, "b").await;
        sqlx::query(
            "update optimization_studies set algorithm_id='price_drop_volume_spike' where id=$1",
        )
        .bind(b.id)
        .execute(&pool)
        .await
        .unwrap();
        b.source["algorithmId"] = json!("price_drop_volume_spike");
        enabled(&pool, "admin", "price_volume_spike").await;
        enabled(&pool, "admin", "price_drop_volume_spike").await;
        let t = scheduler::next(&pool).await.unwrap().unwrap();
        assert_eq!(t.settings.algorithm_id, "price_volume_spike");
        created(auto(&pool, &t, &a, "a").await);
        let job = claim(&pool).await;
        actual::fail(&pool, &job, "a").await.unwrap();
        let t = scheduler::next(&pool).await.unwrap().unwrap();
        assert_eq!(t.settings.algorithm_id, "price_drop_volume_spike");
        created(auto(&pool, &t, &b, "b").await);
        let job = claim(&pool).await;
        actual::fail(&pool, &job, "b").await.unwrap();
        let settings = scheduler::ensure(&pool, "admin", "price_drop_volume_spike")
            .await
            .unwrap();
        scheduler::configure(
            &pool,
            "admin",
            "price_drop_volume_spike",
            settings.revision,
            false,
            "1970-01-01",
            "UTC",
        )
        .await
        .unwrap();
        for timestamp in [
            "2026-11-22T00:00:59.999999999Z",
            "2026-11-22T05:00:00.000000001+05:00",
            "2026-11-22T00:01:00.250Z",
        ] {
            let parsed = DateTime::parse_from_rfc3339(timestamp)
                .unwrap()
                .with_timezone(&Utc);
            let events =
                json!([{"kind":"started","at":"malformed"},{"kind":"started","at":timestamp}]);
            let value: String = sqlx::query_scalar("select study_actual_execution_start($1)::text")
                .bind(&events)
                .fetch_one(&pool)
                .await
                .unwrap();
            assert_eq!(
                value.parse::<f64>().unwrap(),
                parsed.timestamp() as f64 + parsed.timestamp_subsec_nanos() as f64 / 1e9
            );
            let expected = format!(
                "{}.{:09}",
                parsed.timestamp(),
                parsed.timestamp_subsec_nanos()
            );
            let exact: bool =
                sqlx::query_scalar("select study_actual_execution_start($1)=$2::text::numeric")
                    .bind(&events)
                    .bind(&expected)
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            assert!(exact, "fractional precision changed: {value} vs {expected}");
            sqlx::query(
                "update optimization_studies set execution_started_at=null,events=$2 where id=$1",
            )
            .bind(a.id)
            .bind(events)
            .execute(&pool)
            .await
            .unwrap();
            due(&pool, "admin").await;
            let t = scheduler::next(&pool).await.unwrap().unwrap();
            assert_eq!(t.candidates[0].start, parsed);
            scheduler::skipped(&pool, &t, "fixture").await.unwrap();
        }
    })
    .await;
}

#[tokio::test]
#[ignore = "explicit loopback TEST_DATABASE_URL and CREATEDB required"]
async fn owner_then_queue_order_allows_cleanup_admit_and_claim_to_progress() {
    isolated(|pool| async move {
        let a = automatic_seed(&pool, "a").await;
        enabled(&pool, "admin", "price_volume_spike").await;
        let t = scheduler::next(&pool).await.unwrap().unwrap();
        // Hold purge's first lock. The admitting connection must wait BEFORE
        // owning QUEUE_LOCK, otherwise acquiring it below reproduces inversion.
        let mut guard = pool.begin().await.unwrap();
        sqlx::query("select pg_advisory_xact_lock(hashtextextended('admin',72423415))")
            .execute(&mut *guard)
            .await
            .unwrap();
        let child = pool.clone();
        let source = a.clone();
        let task = tokio::spawn(async move { auto(&child, &t, &source, "concurrent").await });
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            sqlx::query("select pg_advisory_xact_lock($1)")
                .bind(actual::QUEUE_LOCK)
                .execute(&mut *guard),
        )
        .await
        .expect("owner/queue lock inversion")
        .unwrap();
        guard.rollback().await.unwrap();
        created(task.await.unwrap());
        let preview = setups_db::continuous::cleanup::preview(&pool, "admin")
            .await
            .unwrap();
        let (purge, claimed) = tokio::time::timeout(std::time::Duration::from_secs(3), async {
            tokio::join!(
                setups_db::continuous::cleanup::purge(&pool, "admin", &preview.fingerprint),
                actual::claim(&pool, 2)
            )
        })
        .await
        .expect("claim and purge deadlocked");
        purge.unwrap();
        let job = claimed.unwrap().unwrap();
        actual::fail(&pool, &job, "fixture").await.unwrap();
        created(
            admit(&pool, &a, "manual-progress", Default::default())
                .await
                .unwrap(),
        );
        assert_eq!(claim(&pool).await.job_id, "manual-progress");
    })
    .await;
}
