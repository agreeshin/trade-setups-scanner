//! Decision52 persistence contracts only: no HTTP, renderer, market reads or
//! Telegram admissions. Every ignored case owns and drops a unique local DB.
use base64::{Engine as _, engine::general_purpose::STANDARD};
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use setups_db::{
    DbError,
    expectancy::{MAX_NUMERICAL_BYTES, MAX_PNG_BYTES},
    study_presentation::{self as presentation, Png, Prepared},
};
use sha2::{Digest, Sha256};
use sqlx::{PgPool, postgres::{PgConnectOptions, PgPoolOptions}};
use std::{future::Future, str::FromStr};

const START: i64 = 1_794_000_000;
// Minute-aligned original C=10 and refresh C=20, not publication/send times.
const INITIAL_CUTOFF: i64 = START + 10 * 60;
const ACTUAL_CUTOFF: i64 = START + 20 * 60;
// Valid deterministic 1x1 RGBA PNGs. This layer checks bytes/hash, not the
// renderer's separate 900x320 visual contract; no analytics dependency needed.
const INITIAL_PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGNgYGD4DwABBAEAX+XDSwAAAABJRU5ErkJggg==";
const ACTUAL_PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAAC0lEQVR4nGP4DwQACfsD/fteaysAAAAASUVORK5CYII=";
const INITIAL_PNG_HASH: &str = "abc58d5127d7cdf313beb9ec8ee839860a9c6bfbc48c8b8eb6a3f7d8bb63de6f";
const ACTUAL_PNG_HASH: &str = "7ac2abfce2be8b46dd9826d597d94edeefc1570b4a992622721b90a1850e3ee9";

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
    let admin = PgPoolOptions::new().max_connections(1)
        .connect_with(options.clone()).await.unwrap();
    let name = format!("presentation_{}_{}_test", std::process::id(),
        Utc::now().timestamp_nanos_opt().unwrap());
    sqlx::query(&format!("create database {name}")).execute(&admin).await.unwrap();
    let connected = PgPoolOptions::new().max_connections(3)
        .connect_with(options.database(&name)).await;
    // Keep the pool outside the child: assertion/migration panics must still
    // close its connections and drop only this case's generated database.
    let outcome = if let Ok(pool) = &connected {
        let child = pool.clone();
        let outcome = tokio::task::LocalSet::new().run_until(async move {
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
                assert!(setups_db::migrate::ensure_migrations(&child).await.unwrap()
                    .starts_with("0 applied"));
                assert_eq!(sqlx::query_scalar::<_, i64>("select count(*) from kysely_migration where name='0032_study_current_presentation.sql'")
                    .fetch_one(&child).await.unwrap(), 1);
                body(child).await;
            }).await
        }).await;
        pool.close().await;
        Some(outcome)
    } else {
        None
    };
    let cleanup = sqlx::query(&format!("drop database {name} with(force)"))
        .execute(&admin).await;
    admin.close().await;
    cleanup.expect("owned disposable DB cleanup failed");
    let outcome = outcome.unwrap_or_else(|| panic!("child DB connection failed: {:?}", connected.err()));
    if let Err(error) = outcome {
        std::panic::resume_unwind(error.into_panic());
    }
}

struct Study {
    owner: String,
    id: i64,
    identity: String,
    spec: Value,
    result: Value,
}

async fn seed(pool: &PgPool, owner: &str) -> Study {
    let params = json!(setups_domain::SpikeParams::default());
    let analytics = json!({"clusterEnabled":false,"clusterGapMinutes":null,
        "timeShiftMinutes":0,"priceChangePct":null,"setupMaturity":0.0,"horizonMinutes":60});
    // Complete retained StudyRequest/StudyOutcome-shaped science, not '{}' or
    // a presentation hash fabricated independently of PostgreSQL's JSONB text.
    let spec = json!({"algorithmId":"price_volume_spike","algorithmVersion":"1.1.1",
        "algorithmParams":params,"algorithmSearch":{},
        "analyticsParams":{"clusterEnabled":false,"clusterGapMinutes":480,
            "timeShiftMinutes":0,"priceChangePct":null},"analyticsSearch":{},
        "instruments":[{"exchange":"bybit","category":"linear","symbol":"BTCUSDT"}],
        "fromUnix":START-86400,"toUnix":INITIAL_CUTOFF,"direction":"short",
        "setupMaturity":0.0,"tpLevels":[2.0],"slLevels":[1.0],"horizonMinutes":60,
        "minClosedPerCell":1,"minSetupsPerDay":0.0,"maxTrials":1,"maxSeconds":30,"seed":42});
    let progress = json!({"phase":"cleanup","evaluatedCandidates":1,"scorableCandidates":1,
        "maxTrials":1,"elapsedSeconds":0.125,"bestScore":1.25});
    let result = json!({"progress":progress,"reason":"trial_budget","baseline":null,
        "invalidProposals":0,"detectorPasses":1,"best":{
            "score":1.25,"partial":false,"params":params,"analytics":analytics,
            "matrix":[{"tpPct":2.0,"slPct":1.0,"riskExpectancy":1.25,"avgMovePct":1.0,
                "closedCount":4,"tpCount":3,"slCount":1,"unclosedCount":0,
                "noDataCount":0,"priceChangeFilteredCount":0}],
            "provenance":{"optimizerVersion":"plan-a-3","seed":42,
                "snapshotHash":"original-frozen-input","capturedAt":DateTime::from_timestamp(INITIAL_CUTOFF,0).unwrap()},
            "conclusions":[],"support":{"score":1.25,"setupCount":4,"selectedSetupCount":4,
                "avgSetupsPerDay":4.0,"requiredSetupCount":1,"evaluatedMinutes":1440,"warmupMinutes":60},
            "trialIndex":0,"observationCutoff":INITIAL_CUTOFF}});
    let id = sqlx::query_scalar("insert into optimization_studies(algorithm_id,status,spec,progress,result,owner_id,execution_started_at) values('price_volume_spike','completed',$1,$2,$3,$4,to_timestamp($5)) returning id")
        .bind(&spec).bind(&progress).bind(&result).bind(owner)
        .bind(INITIAL_CUTOFF as f64 + 30.25).fetch_one(pool).await.unwrap();
    let identity = sqlx::query_scalar("select md5(spec::text||coalesce(result::text,'null')) from optimization_studies where id=$1")
        .bind(id).fetch_one(pool).await.unwrap();
    Study { owner:owner.into(), id, identity, spec, result }
}

fn prepared(study: &Study, source_revision: Option<i64>, cutoff: i64) -> Prepared {
    let actual = source_revision.is_some();
    let (tp, sl, mean, win_rate) = if actual { (5, 3, 0.875, 62.5) } else { (3, 1, 1.25, 75.0) };
    let counts = json!({"total":tp+sl,"closed":tp+sl,"tp":tp,"sl":sl,
        "gateFailed":0,"unclosed":0,"noData":0});
    let png = STANDARD.decode(if actual { ACTUAL_PNG } else { INITIAL_PNG }).unwrap();
    assert_eq!(format!("{:x}", Sha256::digest(&png)),
        if actual { ACTUAL_PNG_HASH } else { INITIAL_PNG_HASH });
    Prepared {
        owner:study.owner.clone(), study:study.id, source_identity:study.identity.clone(), png,
        snapshot:json!({"revision":0,"studyId":study.id,"generationId":null,
            "source":if actual {"actual"} else {"initial"},"sourceRevision":source_revision,
            "observationCutoff":cutoff,"meanR":mean,"decisionR":mean,"selectedObjectiveR":mean,
            "forecastR":null,"winRate":win_rate,"counts":counts,
            "closedCount":tp+sl,"tpCount":tp,"slCount":sl,
            "objective":{"trendMode":"mean","trendBendDays":0,"trendBlendLambda":0.7},
            "tpPct":2.0,"slPct":1.0,"originalSnapshotHash":"original-frozen-input",
            "sourceIdentity":study.identity,"sourceArtifactId":null,"renderVersion":"db-fixture-1"}),
        // The persistence API owns an opaque bounded numerical object. Its
        // evaluator/decoder contract is deliberately tested by the core owner.
        numerical:json!({"version":1,"source":{"studyId":study.id,"sourceRevision":source_revision,
                "observationCutoff":cutoff},"selectedCell":{"rawR":mean,"tpCount":tp,"slCount":sl,
                "closedCount":tp+sl,"winRatePct":win_rate}}),
    }
}

async fn stored(pool: &PgPool, study: i64) -> Option<Value> {
    sqlx::query_scalar("select to_jsonb(p) from study_current_presentations p where study_id=$1")
        .bind(study).fetch_optional(pool).await.unwrap()
}

async fn assert_science_unchanged(pool: &PgPool, study: &Study) {
    let (spec, result): (Value, Value) = sqlx::query_as("select spec,result from optimization_studies where id=$1")
        .bind(study.id).fetch_one(pool).await.unwrap();
    assert_eq!(spec, study.spec);
    assert_eq!(result, study.result);
}

async fn assert_bundle(pool: &PgPool, prepared: &Prepared, revision: i64) -> Value {
    let row = stored(pool, prepared.study).await.unwrap();
    assert_eq!(row["owner_id"], prepared.owner);
    assert_eq!(row["source_identity"], prepared.source_identity);
    assert_eq!(row["numerical"], prepared.numerical);
    assert!(row["error"].is_null());
    let hash = format!("{:x}", Sha256::digest(&prepared.png));
    assert_eq!(row["png_sha256"], hash);
    let mut snapshot = prepared.snapshot.clone();
    snapshot["revision"] = json!(revision);
    snapshot["pngSha256"] = json!(hash);
    let published = row["snapshot"]["publishedAt"].as_str().expect("publication timestamp required");
    DateTime::parse_from_rfc3339(published).unwrap();
    snapshot["publishedAt"] = json!(published);
    assert_eq!(row["snapshot"], snapshot);
    assert_eq!(presentation::summary(pool, &prepared.owner, prepared.study).await.unwrap(),
        json!({"status":"available","snapshot":snapshot}));
    match presentation::png(pool, &prepared.owner, prepared.study, revision).await.unwrap() {
        Png::Available(png) => assert_eq!(png, prepared.png),
        _ => panic!("current revision must return its matching PNG bytes"),
    }
    row
}

async fn set_actual_head(pool: &PgPool, study: &Study, revision: i64, cutoff: i64, status: &str) {
    let source = json!({"algorithmId":"price_volume_spike","algorithmVersion":"1.1.1",
        "params":study.result["best"]["params"],"instruments":study.spec["instruments"],
        "startUnix":INITIAL_CUTOFF as f64+30.25,"tpPct":2.0,"slPct":1.0});
    let report = (status == "completed").then(|| json!({"sourceFacts":source,"revision":revision}));
    sqlx::query("insert into study_actualizations(owner_id,study_id,job_id,revision,status,stage,execution_started_at,observed_through_unix,source,source_identity,report,work_minutes,reserved_bytes) values($1,$2,$3,$4,$5,$5,to_timestamp($6),$7,$8,$9,$10,1,$11) on conflict(owner_id,study_id) do update set job_id=excluded.job_id,revision=excluded.revision,status=excluded.status,stage=excluded.stage,observed_through_unix=excluded.observed_through_unix,source=excluded.source,source_identity=excluded.source_identity,report=excluded.report,reserved_bytes=excluded.reserved_bytes")
        .bind(&study.owner).bind(study.id).bind(format!("presentation-{}-{revision}",study.id))
        .bind(revision).bind(status).bind(INITIAL_CUTOFF as f64+30.25).bind(cutoff)
        .bind(source).bind(&study.identity).bind(report)
        .bind(if status == "running" {1_i64} else {0_i64}).execute(pool).await.unwrap();
}

async fn owner_lock(tx: &mut sqlx::Transaction<'_, sqlx::Postgres>, owner: &str) {
    // publish_in requires its caller's owner fence; the production helper is
    // crate-private, so acquire the same advisory lock without widening APIs.
    sqlx::query("select pg_advisory_xact_lock(hashtextextended($1,72423415))")
        .bind(owner).execute(&mut **tx).await.unwrap();
    sqlx::query("insert into continuous_owner_lanes(owner_id) values($1) on conflict do nothing")
        .bind(owner).execute(&mut **tx).await.unwrap();
}

async fn publish_fenced(pool: &PgPool, candidate: &Prepared) -> Result<bool, DbError> {
    let mut tx = pool.begin().await.unwrap();
    owner_lock(&mut tx, &candidate.owner).await;
    let result = presentation::publish_in(&mut tx, candidate).await;
    if result.is_ok() { tx.commit().await.unwrap(); } else { tx.rollback().await.unwrap(); }
    result
}

#[tokio::test]
#[ignore = "explicit loopback TEST_DATABASE_URL and CREATEDB required"]
async fn initial_publication_is_one_complete_bundle_with_revision_qualified_png() {
    isolated(|pool| async move {
        let study = seed(&pool, "alice").await;
        assert_eq!(presentation::summary(&pool, &study.owner, study.id).await.unwrap(),
            json!({"status":"unavailable","reason":"study_presentation_not_initialized"}));
        assert!(matches!(presentation::png(&pool, &study.owner, study.id, 1).await.unwrap(), Png::Unavailable));
        let candidate = prepared(&study, None, INITIAL_CUTOFF);
        assert!(presentation::publish_initial(&pool, &candidate).await.unwrap());
        assert_bundle(&pool, &candidate, 1).await;
        for revision in [0, 2] {
            assert!(matches!(presentation::png(&pool, &study.owner, study.id, revision).await.unwrap(), Png::Changed));
        }
        assert_eq!(sqlx::query_scalar::<_, i64>("select count(*) from study_current_presentations")
            .fetch_one(&pool).await.unwrap(), 1);
        assert_science_unchanged(&pool, &study).await;
    }).await;
}

#[tokio::test]
#[ignore = "explicit loopback TEST_DATABASE_URL and CREATEDB required"]
async fn actual_successor_switches_png_metrics_cutoff_and_completed_refresh_together() {
    isolated(|pool| async move {
        let study = seed(&pool, "alice").await;
        let initial = prepared(&study, None, INITIAL_CUTOFF);
        assert!(presentation::publish_initial(&pool, &initial).await.unwrap());
        let prior = assert_bundle(&pool, &initial, 1).await;
        set_actual_head(&pool, &study, 2, ACTUAL_CUTOFF, "running").await;
        let successor = prepared(&study, Some(2), ACTUAL_CUTOFF);
        let mut tx = pool.begin().await.unwrap();
        owner_lock(&mut tx, &study.owner).await;
        sqlx::query("update study_actualizations set status='completed',stage='completed',report=$2,reserved_bytes=0 where study_id=$1")
            .bind(study.id).bind(json!({"revision":2,"closed":8})).execute(&mut *tx).await.unwrap();
        assert!(presentation::publish_in(&mut tx, &successor).await.unwrap());
        // An independent connection cannot observe any staged component.
        assert_eq!(stored(&pool, study.id).await.unwrap(), prior);
        assert_eq!(sqlx::query_scalar::<_, String>("select status from study_actualizations where study_id=$1")
            .bind(study.id).fetch_one(&pool).await.unwrap(), "running");
        tx.commit().await.unwrap();
        let current = assert_bundle(&pool, &successor, 2).await;
        assert_eq!(current["snapshot"]["sourceRevision"], 2);
        assert_eq!(current["snapshot"]["observationCutoff"], ACTUAL_CUTOFF);
        assert_eq!(current["snapshot"]["meanR"], 0.875);
        assert_eq!(current["snapshot"]["closedCount"], 8);
        assert_ne!(current["png"], prior["png"]);
        assert_eq!(sqlx::query_scalar::<_, String>("select status from study_actualizations where study_id=$1")
            .bind(study.id).fetch_one(&pool).await.unwrap(), "completed");
        assert!(matches!(presentation::png(&pool, &study.owner, study.id, 1).await.unwrap(), Png::Changed));
        assert_science_unchanged(&pool, &study).await;
    }).await;
}

#[tokio::test]
#[ignore = "explicit loopback TEST_DATABASE_URL and CREATEDB required"]
async fn rolled_back_successor_preserves_the_previous_complete_bundle_and_job() {
    isolated(|pool| async move {
        let study = seed(&pool, "alice").await;
        let initial = prepared(&study, None, INITIAL_CUTOFF);
        assert!(presentation::publish_initial(&pool, &initial).await.unwrap());
        let prior = assert_bundle(&pool, &initial, 1).await;
        set_actual_head(&pool, &study, 2, ACTUAL_CUTOFF, "running").await;
        let successor = prepared(&study, Some(2), ACTUAL_CUTOFF);
        let mut tx = pool.begin().await.unwrap();
        owner_lock(&mut tx, &study.owner).await;
        sqlx::query("update study_actualizations set status='completed',report=$2,reserved_bytes=0 where study_id=$1")
            .bind(study.id).bind(json!({"revision":2})).execute(&mut *tx).await.unwrap();
        assert!(presentation::publish_in(&mut tx, &successor).await.unwrap());
        let staged: Value = sqlx::query_scalar("select snapshot from study_current_presentations where study_id=$1")
            .bind(study.id).fetch_one(&mut *tx).await.unwrap();
        assert_eq!(staged["revision"], 2);
        assert_eq!(staged["observationCutoff"], ACTUAL_CUTOFF);
        assert_eq!(stored(&pool, study.id).await.unwrap(), prior);
        tx.rollback().await.unwrap();
        assert_eq!(stored(&pool, study.id).await.unwrap(), prior);
        assert_bundle(&pool, &initial, 1).await;
        assert_eq!(sqlx::query_as::<_, (String, Option<Value>)>("select status,report from study_actualizations where study_id=$1")
            .bind(study.id).fetch_one(&pool).await.unwrap(), ("running".into(), None));
        assert_science_unchanged(&pool, &study).await;
    }).await;
}

#[tokio::test]
#[ignore = "explicit loopback TEST_DATABASE_URL and CREATEDB required"]
async fn stale_identity_lower_cutoff_revision_and_ack_retry_cannot_rewrite_current() {
    isolated(|pool| async move {
        let study = seed(&pool, "alice").await;
        assert!(presentation::publish_initial(&pool, &prepared(&study, None, INITIAL_CUTOFF)).await.unwrap());
        set_actual_head(&pool, &study, 2, ACTUAL_CUTOFF, "completed").await;
        let current = prepared(&study, Some(2), ACTUAL_CUTOFF);
        assert!(presentation::publish_initial(&pool, &current).await.unwrap());
        let committed = assert_bundle(&pool, &current, 2).await;
        // Simulate the caller losing the successful COMMIT acknowledgement.
        assert!(presentation::publish_initial(&pool, &current).await.unwrap());
        assert_eq!(stored(&pool, study.id).await.unwrap(), committed,
            "ACK re-entry preserves revision, fixed C, publication time, hash and bytes");
        let mut stale = prepared(&study, Some(3), ACTUAL_CUTOFF+60);
        stale.source_identity = "0".repeat(32);
        assert!(!publish_fenced(&pool, &stale).await.unwrap());
        for candidate in [prepared(&study, Some(3), ACTUAL_CUTOFF-60),
            prepared(&study, Some(1), ACTUAL_CUTOFF+60)] {
            assert!(!publish_fenced(&pool, &candidate).await.unwrap());
            assert_eq!(stored(&pool, study.id).await.unwrap(), committed);
        }
        let mut conflicting = prepared(&study, Some(2), ACTUAL_CUTOFF);
        conflicting.numerical["selectedCell"]["rawR"] = json!(99.0);
        let error = publish_fenced(&pool, &conflicting).await.unwrap_err();
        assert!(error.to_string().contains("study_presentation_publication_conflict"));
        assert_eq!(stored(&pool, study.id).await.unwrap(), committed);
        assert_science_unchanged(&pool, &study).await;
    }).await;
}

#[tokio::test]
#[ignore = "explicit loopback TEST_DATABASE_URL and CREATEDB required"]
async fn saved_actual_initialization_requires_the_current_completed_source_head() {
    isolated(|pool| async move {
        let study = seed(&pool, "alice").await;
        let initial = prepared(&study, None, INITIAL_CUTOFF);
        assert!(presentation::publish_initial(&pool, &initial).await.unwrap());
        let prior = assert_bundle(&pool, &initial, 1).await;
        let successor = prepared(&study, Some(2), ACTUAL_CUTOFF);
        assert!(!presentation::publish_initial(&pool, &successor).await.unwrap(), "no saved actual head");
        set_actual_head(&pool, &study, 2, ACTUAL_CUTOFF, "running").await;
        assert!(!presentation::publish_initial(&pool, &successor).await.unwrap(), "running is not completed");
        set_actual_head(&pool, &study, 3, ACTUAL_CUTOFF, "completed").await;
        assert!(!presentation::publish_initial(&pool, &successor).await.unwrap(), "superseded actual revision");
        set_actual_head(&pool, &study, 2, ACTUAL_CUTOFF+60, "completed").await;
        assert!(!presentation::publish_initial(&pool, &successor).await.unwrap(), "different admission cutoff");
        assert_eq!(stored(&pool, study.id).await.unwrap(), prior);
        set_actual_head(&pool, &study, 2, ACTUAL_CUTOFF, "completed").await;
        assert!(presentation::publish_initial(&pool, &successor).await.unwrap());
        assert_bundle(&pool, &successor, 2).await;
    }).await;
}

#[tokio::test]
#[ignore = "explicit loopback TEST_DATABASE_URL and CREATEDB required"]
async fn initialization_failure_and_replacement_never_erase_a_previous_success() {
    isolated(|pool| async move {
        let study = seed(&pool, "alice").await;
        presentation::initialization_failed(&pool, &study.owner, study.id, &study.identity,
            "original_outcomes_not_persisted").await.unwrap();
        assert_eq!(presentation::summary(&pool, &study.owner, study.id).await.unwrap(),
            json!({"status":"unavailable","reason":"original_outcomes_not_persisted"}));
        assert!(matches!(presentation::png(&pool, &study.owner, study.id, 1).await.unwrap(), Png::Unavailable));
        let failed = stored(&pool, study.id).await.unwrap();
        for field in ["snapshot", "numerical", "png", "png_sha256"] { assert!(failed[field].is_null()); }
        let initial = prepared(&study, None, INITIAL_CUTOFF);
        assert!(presentation::publish_initial(&pool, &initial).await.unwrap());
        let prior = assert_bundle(&pool, &initial, 1).await;
        presentation::initialization_failed(&pool, &study.owner, study.id, &study.identity,
            "new_render_failed").await.unwrap();
        assert_eq!(stored(&pool, study.id).await.unwrap(), prior);
        let mut replacement = prepared(&study, None, INITIAL_CUTOFF+60);
        replacement.png = STANDARD.decode(ACTUAL_PNG).unwrap();
        replacement.numerical["selectedCell"]["rawR"] = json!(99.0);
        assert!(!presentation::publish_initial(&pool, &replacement).await.unwrap());
        assert_eq!(stored(&pool, study.id).await.unwrap(), prior);
        assert_science_unchanged(&pool, &study).await;
    }).await;
}

#[tokio::test]
#[ignore = "explicit loopback TEST_DATABASE_URL and CREATEDB required"]
async fn ownership_fences_reads_publication_failures_and_cascaded_cleanup() {
    isolated(|pool| async move {
        let alice = seed(&pool, "alice").await;
        let bob = seed(&pool, "bob").await;
        let alice_bundle = prepared(&alice, None, INITIAL_CUTOFF);
        let bob_bundle = prepared(&bob, None, INITIAL_CUTOFF);
        assert!(presentation::publish_initial(&pool, &alice_bundle).await.unwrap());
        assert!(presentation::publish_initial(&pool, &bob_bundle).await.unwrap());
        let alice_row = assert_bundle(&pool, &alice_bundle, 1).await;
        let bob_row = assert_bundle(&pool, &bob_bundle, 1).await;
        for (owner, study) in [(&alice.owner, &bob), (&bob.owner, &alice)] {
            assert_eq!(presentation::summary(&pool, owner, study.id).await.unwrap()["status"], "unavailable");
            assert!(matches!(presentation::png(&pool, owner, study.id, 1).await.unwrap(), Png::Unavailable));
            let mut unauthorized = prepared(study, Some(2), ACTUAL_CUTOFF);
            unauthorized.owner = owner.clone();
            assert!(!publish_fenced(&pool, &unauthorized).await.unwrap(), "knowing the source hash grants no ownership");
            presentation::initialization_failed(&pool, owner, study.id, &study.identity, "foreign_failure")
                .await.unwrap();
        }
        assert_eq!(stored(&pool, alice.id).await.unwrap(), alice_row);
        assert_eq!(stored(&pool, bob.id).await.unwrap(), bob_row);
        assert_science_unchanged(&pool, &alice).await;
        assert_science_unchanged(&pool, &bob).await;
        sqlx::query("delete from optimization_studies where id=$1").bind(bob.id).execute(&pool).await.unwrap();
        assert!(stored(&pool, bob.id).await.is_none());
        assert_eq!(stored(&pool, alice.id).await.unwrap(), alice_row);
    }).await;
}

#[tokio::test]
#[ignore = "explicit loopback TEST_DATABASE_URL and CREATEDB required"]
async fn bounded_candidates_and_repeated_refresh_keep_one_owned_current_row() {
    isolated(|pool| async move {
        let study = seed(&pool, "alice").await;
        let initial = prepared(&study, None, INITIAL_CUTOFF);
        assert!(presentation::publish_initial(&pool, &initial).await.unwrap());
        let prior = assert_bundle(&pool, &initial, 1).await;
        for invalid in 0..5 {
            let mut candidate = prepared(&study, Some(2), ACTUAL_CUTOFF);
            match invalid {
                0 => candidate.png.clear(),
                1 => candidate.png = vec![0; MAX_PNG_BYTES+1],
                2 => candidate.numerical = json!([]),
                3 => candidate.numerical = json!({"oversize":"x".repeat(MAX_NUMERICAL_BYTES)}),
                4 => candidate.snapshot["oversize"] = json!("x".repeat(32768)),
                _ => unreachable!(),
            }
            assert!(publish_fenced(&pool, &candidate).await.unwrap_err().to_string()
                .contains("study_presentation_invalid_bundle"));
            assert_eq!(stored(&pool, study.id).await.unwrap(), prior);
        }
        for revision in 2..=9 {
            let cutoff = ACTUAL_CUTOFF+(revision-2)*60;
            set_actual_head(&pool, &study, revision, cutoff, "completed").await;
            let successor = prepared(&study, Some(revision), cutoff);
            assert!(presentation::publish_initial(&pool, &successor).await.unwrap());
            assert_bundle(&pool, &successor, revision).await;
            let (rows, bytes): (i64, i64) = sqlx::query_as("select count(*),coalesce(sum(octet_length(snapshot::text)+octet_length(numerical::text)+octet_length(png)),0)::bigint from study_current_presentations")
                .fetch_one(&pool).await.unwrap();
            assert_eq!(rows, 1, "refresh replaces the row, not a retained history of bundles");
            assert!(bytes < 8192, "tiny bundles must not retain superseded payloads: {bytes}");
        }
        assert!(matches!(presentation::png(&pool, &study.owner, study.id, 8).await.unwrap(), Png::Changed));
        assert_science_unchanged(&pool, &study).await;
    }).await;
}
