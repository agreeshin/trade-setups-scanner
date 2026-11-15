//! Real PostgreSQL persistence contracts. Each case owns an isolated *_test DB;
//! no silent URL skip, network/science rerun or market source is involved.
use serde_json::{Value, json};
use setups_db::{
    continuous::{self as continuous, recovery, scheduler},
    expectancy as db,
};
use setups_domain::{RecommendationDirection, RecommendationProvenance, expectancy::*};
use sqlx::{
    PgPool,
    postgres::{PgConnectOptions, PgPoolOptions},
};
use std::{future::Future, str::FromStr};

async fn isolated<F, Fut>(body: F)
where
    F: FnOnce(PgPool) -> Fut + 'static,
    Fut: Future<Output = ()> + 'static,
{
    let options = PgConnectOptions::from_str(
        &std::env::var("TEST_DATABASE_URL").expect("isolated *_test URL required"),
    )
    .unwrap();
    assert!(options.get_database().unwrap().ends_with("_test"));
    let admin = PgPoolOptions::new()
        .max_connections(2)
        .connect_with(options.clone())
        .await
        .unwrap();
    let name = format!(
        "expectancy_{}_{}_test",
        std::process::id(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap()
    );
    sqlx::query(&format!("create database {name}"))
        .execute(&admin)
        .await
        .unwrap();
    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect_with(options.database(&name))
        .await
        .unwrap();
    let child = pool.clone();
    let result = tokio::task::LocalSet::new().run_until(async move { tokio::task::spawn_local(async move {
        let baseline = include_str!("../migrations/0010_baseline.sql").lines()
            .filter(|line| !line.starts_with('\\') && !line.starts_with("SET transaction_timeout") && !line.contains("set_config('search_path'"))
            .collect::<Vec<_>>().join("\n");
        sqlx::raw_sql(&baseline).execute(&child).await.unwrap();
        sqlx::raw_sql("set search_path to public;create table kysely_migration(name varchar(255) primary key,timestamp varchar(255) not null);insert into kysely_migration values('0009_legacy','fixture')").execute(&child).await.unwrap();
        setups_db::migrate::ensure_migrations(&child).await.unwrap();
        body(child).await;
    }).await }).await;
    pool.close().await;
    sqlx::query(&format!("drop database {name} with(force)"))
        .execute(&admin)
        .await
        .unwrap();
    admin.close().await;
    if let Err(error) = result {
        std::panic::resume_unwind(error.into_panic());
    }
}

async fn chain(pool: &PgPool) -> (scheduler::ChainJob, db::ArtifactIdentity, Value) {
    let config = json!({"adaptiveEnabled":true,"lookbackDays":3,"minSetupsPerDay":[0],"minMeanExpectancy":0.15,
        "study":{"algorithmId":"price_volume_spike","algorithmVersion":"1.1.1","direction":"short","horizonMinutes":60,"maxSeconds":3600,"maxTrials":8,"seed":42}});
    continuous::configure(pool, "admin", "price_volume_spike", &config, &[0.0])
        .await
        .unwrap();
    continuous::start(pool, "admin", "price_volume_spike")
        .await
        .unwrap();
    let lease = scheduler::claim_owner(pool, "first-executor")
        .await
        .unwrap()
        .unwrap();
    let job = scheduler::prepare_next(pool, &lease)
        .await
        .unwrap()
        .unwrap();
    let provenance = json!({"snapshotHash":"frozen-fixture","optimizerVersion":"plan-a-continuous-1","seed":42,"capturedAt":"2026-11-15T00:00:00Z"});
    let analytics = json!({"clusterEnabled":false,"clusterGapMinutes":null,"timeShiftMinutes":0,"priceChangePct":null,"horizonMinutes":60,"setupMaturity":null});
    let matrix: Vec<_> = (1..=9).flat_map(|tp| (1..=9).map(move |sl| json!({"tpPct":tp as f64,"slPct":sl as f64,
        "tpCount":2,"slCount":1,"closedCount":3,"riskExpectancy":(2.0*tp as f64-sl as f64)/3.0/sl as f64}))).collect();
    let original = json!({"progress":{},"best":{"score":1.0,"matrix":matrix,"analytics":analytics,"provenance":provenance},"scienceSentinel":1.2345678901234567});
    scheduler::finish_search(pool, &job, &original, true)
        .await
        .unwrap();
    let manifest = json!({"provenance":provenance,"instruments":[{"instrument":{"exchange":"bybit","category":"linear","symbol":"BTCUSDT"}}]});
    scheduler::persist_replay(
        pool,
        &job,
        &json!({}),
        manifest,
        &[scheduler::ServiceReplayRun {
            exchange: "bybit".into(),
            category: "linear".into(),
            symbol: "BTCUSDT".into(),
            setups: json!([]),
            warmup_minutes: 0,
            evaluated_minutes: 1,
        }],
    )
    .await
    .unwrap();
    let identity = db::ArtifactIdentity {
        owner_id: "admin".into(),
        generation_id: job.generation_id,
        study_id: job.study_id,
        slot_id: job.slot_id,
        calculation_version: EXPECTANCY_CALCULATION_VERSION.to_string(),
        historical_input_hash: None,
        tp_pct: 2.0,
        sl_pct: 1.0,
        from_unix: job.spec["fromUnix"].as_i64().unwrap(),
        to_unix: job.spec["toUnix"].as_i64().unwrap(),
        observation_cutoff: job.observation_cutoff,
        provenance,
        effective_analytics: analytics,
    };
    (job, identity, original)
}

fn artifact(identity: &db::ArtifactIdentity, legacy: bool) -> Value {
    let first = utc_midnight_unix(identity.from_unix).unwrap();
    let source = ExpectancySource {
        study_id: identity.study_id.to_string(),
        derived_snapshot_id: db::artifact_key(identity).unwrap(),
        generation_id: Some(identity.generation_id.to_string()),
        profile_id: Some(identity.slot_id.to_string()),
        provenance: RecommendationProvenance {
            algorithm_id: "price_volume_spike".into(),
            algorithm_version: "1.1.1".into(),
            optimizer_version: "plan-a-continuous-1".into(),
            seed: 42,
            snapshot_hash: "frozen-fixture".into(),
            captured_at: "2026-11-15T00:00:00Z".into(),
            from_unix: identity.from_unix,
            to_unix: identity.to_unix,
            observation_cutoff: identity.observation_cutoff,
        },
        direction: RecommendationDirection::Short,
        tp_pct: 2.0,
        sl_pct: 1.0,
        analytics: ExpectancyAnalytics {
            cluster_enabled: false,
            cluster_gap_minutes: None,
            time_shift_minutes: 0,
            price_change_pct: None,
            horizon_minutes: 60,
        },
    };
    let daily = if legacy {
        MetricAvailability::Unavailable {
            reason: ExpectancyUnavailableReason::MissingFrozenOutcomes,
        }
    } else {
        MetricAvailability::Available {
            value: vec![
                UtcDailyInput {
                    day_start_unix: first,
                    row_count: 1,
                    tp_count: 1,
                    sl_count: 0,
                    mean_r: Some(2.0),
                },
                UtcDailyInput {
                    day_start_unix: first + 2 * 86400,
                    row_count: 2,
                    tp_count: 1,
                    sl_count: 1,
                    mean_r: Some(0.5),
                },
            ],
        }
    };
    serde_json::to_value(
        build_expectancy_artifact(ExpectancyInput {
            source,
            selected_cell: Some(SelectedCellInput {
                raw_r: Some(1.0),
                tp_count: 2,
                sl_count: 1,
            }),
            matrix_mean_r: Some(1.0),
            daily,
        })
        .unwrap(),
    )
    .unwrap()
}

async fn finish(pool: &PgPool, job: &scheduler::ChainJob, original: &Value) {
    let result = json!({"best":original["best"],"selectedCell":{"tpPct":2.0,"slPct":1.0,"rawR":1.0},"meanExpectancy":1.0,
        "analytics":{"evaluation":{"matrix":original["best"]["matrix"]}},"unchangedSentinel":"original"});
    assert!(
        scheduler::finalize(
            pool,
            job,
            &result,
            false,
            Some(1.0),
            Some(&result["selectedCell"])
        )
        .await
        .unwrap()
    );
}

#[tokio::test]
#[ignore = "requires isolated TEST_DATABASE_URL ending _test with CREATEDB"]
async fn numerical_seal_survives_owner_restart_gate_and_frozen_cleanup_without_science_mutation() {
    isolated(|pool| async move {
        let (old, identity, original) = chain(&pool).await;
        let original_text: String = sqlx::query_scalar("select result::text from optimization_studies where id=$1").bind(old.study_id).fetch_one(&pool).await.unwrap();
        sqlx::query("insert into continuous_runtime_artifacts(generation_id,identity,snapshot_manifest,reserved_bytes,snapshot_bytes) values($1,'{}','{}',1048576,1)").bind(old.generation_id).execute(&pool).await.unwrap();
        sqlx::query("insert into continuous_frozen_chunks values($1,0,decode('01','hex'),repeat('0',64))").bind(old.generation_id).execute(&pool).await.unwrap();
        let numerical = artifact(&identity, false);
        let id = db::seal_for_chain(&pool, &old, &identity, &numerical).await.unwrap().unwrap();
        assert_eq!(db::seal_for_chain(&pool, &old, &identity, &numerical).await.unwrap(), Some(id), "lost ACK must not duplicate or recalculate");
        assert!(db::seal_for_chain(&pool, &old, &identity, &json!({"different":true})).await.is_err());
        sqlx::query("update continuous_owner_lanes set lease_until=clock_timestamp()-interval '1 second'").execute(&pool).await.unwrap();
        let lease = scheduler::claim_owner(&pool, "after-numerical-seal-crash").await.unwrap().unwrap();
        scheduler::recover_owner(&pool, &lease).await.unwrap();
        let same = recovery::active_job(&pool, &lease).await.unwrap().unwrap();
        assert_eq!((same.generation_id,same.study_id,same.observation_cutoff),(old.generation_id,old.study_id,old.observation_cutoff));
        assert_eq!(same.research_deadline, old.research_deadline);
        assert!(db::seal_for_chain(&pool, &old, &identity, &numerical).await.unwrap().is_none(), "old epoch is fenced");
        assert_eq!(db::seal_for_chain(&pool, &same, &identity, &numerical).await.unwrap(),Some(id));
        let loaded = recovery::load(&pool, &same, &json!({})).await.unwrap();
        assert!(loaded.snapshot_committed && loaded.replay_committed);
        let gate = recovery::Gate {result:json!({"best":original["best"],"analytics":{},"selectedCell":{},"meanExpectancy":1.0}),accepted:false,mean:Some(1.0),selected:Some(json!({"tpPct":2.0,"slPct":1.0}))};
        assert!(recovery::save_gate_with_expectancy(&pool, &same, &json!({}), &gate,Some((&identity,&numerical))).await.unwrap());
        // An enriched gate cannot release frozen inputs without its seal.
        sqlx::query("update continuous_generations set expectancy_artifact_id=null where id=$1").bind(same.generation_id).execute(&pool).await.unwrap();
        assert!(recovery::load(&pool,&same,&json!({})).await.err().unwrap().to_string().contains("resume_gate_expectancy_missing"));
        assert!(scheduler::finalize(&pool,&same,&gate.result,false,gate.mean,gate.selected.as_ref()).await.is_err());
        assert_eq!(sqlx::query_scalar::<_,i64>("select count(*) from continuous_frozen_chunks").fetch_one(&pool).await.unwrap(),1);
        sqlx::query("update continuous_generations set expectancy_artifact_id=$2 where id=$1").bind(same.generation_id).bind(id).execute(&pool).await.unwrap();
        // Crash after the atomic gate+artifact COMMIT. The replacement must
        // adopt the gate directly, with no market/science work or TTL renewal.
        sqlx::query("update continuous_owner_lanes set lease_until=clock_timestamp()-interval '1 second'").execute(&pool).await.unwrap();
        let lease=scheduler::claim_owner(&pool,"after-gate-seal-crash").await.unwrap().unwrap();
        scheduler::recover_owner(&pool,&lease).await.unwrap();
        let same=recovery::active_job(&pool,&lease).await.unwrap().unwrap();
        let recovered_gate = recovery::load(&pool, &same, &json!({})).await.unwrap().gate.unwrap();
        assert_eq!(recovered_gate.result,gate.result);
        assert!(scheduler::finalize(&pool, &same, &recovered_gate.result,false,recovered_gate.mean,recovered_gate.selected.as_ref()).await.unwrap());
        assert_eq!(sqlx::query_scalar::<_,i64>("select count(*) from continuous_frozen_chunks").fetch_one(&pool).await.unwrap(),0);
        assert_eq!(sqlx::query_scalar::<_,i64>("select count(*) from continuous_runtime_artifacts").fetch_one(&pool).await.unwrap(),0);
        assert_eq!(db::get(&pool,"admin",id).await.unwrap().unwrap().numerical_snapshot,numerical);
        assert_eq!(db::collect_unreferenced(&pool,100).await.unwrap(),0);
        assert_eq!(sqlx::query_scalar::<_,String>("select result::text from optimization_studies where id=$1").bind(old.study_id).fetch_one(&pool).await.unwrap(),original_text);
        assert_eq!(sqlx::query_scalar::<_,Value>("select spec from continuous_generations where id=$1").bind(old.generation_id).fetch_one(&pool).await.unwrap(),old.spec);
        assert!(db::get(&pool,"other-owner",id).await.unwrap().is_none());
        assert!(sqlx::query("update study_expectancy_artifacts set numerical_snapshot='{}' where id=$1").bind(id).execute(&pool).await.is_err(),"SQL cannot mutate an immutable version");
    }).await;
}

#[tokio::test]
#[ignore = "requires isolated TEST_DATABASE_URL ending _test with CREATEDB"]
async fn legacy_mean_and_winrate_remain_available_without_inventing_daily_outcomes_and_history_survives_purge()
 {
    isolated(|pool| async move {
        let (job, identity, original) = chain(&pool).await;
        finish(&pool,&job,&original).await;
        let before: (String,String) = sqlx::query_as("select g.result::text,s.result::text from continuous_generations g join optimization_studies s on s.id=g.study_id where g.id=$1").bind(job.generation_id).fetch_one(&pool).await.unwrap();
        let source = db::saved_source(&pool,"admin",job.generation_id).await.unwrap().unwrap();
        assert_eq!(source.result["best"]["matrix"].as_array().unwrap().len(),81);
        assert!(source.replay_manifest.unwrap()["instruments"][0]["runId"].is_number(),"saved raw replay is NOT individual closed outcomes");
        let numerical = artifact(&identity,true);
        let id = db::enrich_saved_generation(&pool,"admin",job.generation_id).await.unwrap().unwrap();
        assert_eq!(db::enrich_saved_generation(&pool,"admin",job.generation_id).await.unwrap(),Some(id));
        assert_eq!(db::seal_existing(&pool,&identity,&numerical).await.unwrap(),id);
        let loaded: ExpectancyArtifact = serde_json::from_value(db::get(&pool,"admin",id).await.unwrap().unwrap().numerical_snapshot).unwrap();
        assert_eq!(loaded.selected_cell.as_ref().unwrap().raw_r.as_ref(),Some(&1.0));
        assert!((loaded.selected_cell.as_ref().unwrap().win_rate_pct.as_ref().unwrap()-100.0*2.0/3.0).abs()<1e-12);
        assert!(loaded.daily.as_ref().is_none() && loaded.trend.is_none());
        assert!(matches!(loaded.forecast_at_raw_signal(identity.to_unix+60).unwrap(),MetricAvailability::Unavailable {reason:ExpectancyUnavailableReason::MissingFrozenOutcomes}));
        let after: (String,String) = sqlx::query_as("select g.result::text,s.result::text from continuous_generations g join optimization_studies s on s.id=g.study_id where g.id=$1").bind(job.generation_id).fetch_one(&pool).await.unwrap();
        assert_eq!(before,after,"enrichment never rewrites generation or original scientific result");
        assert!(db::store_png(&pool,"admin",id,"1",&vec![0;db::MAX_PNG_BYTES+1]).await.is_err());
        continuous::stop(&pool,"admin","price_volume_spike").await.unwrap();
        continuous::cleanup_stopped(&pool,"admin","price_volume_spike",&job.lease).await.unwrap();
        let preview = continuous::cleanup::preview(&pool,"admin").await.unwrap();
        assert!(preview.eligible,"{:?}",preview.blockers);
        assert!(matches!(continuous::cleanup::purge(&pool,"admin",&preview.fingerprint).await.unwrap(),continuous::cleanup::Outcome::Deleted(_)));
        assert!(db::get(&pool,"admin",id).await.unwrap().is_some(),"retained completed replay owns numerical history after generation purge");
        assert_eq!(db::collect_unreferenced(&pool,100).await.unwrap(),0);
        sqlx::query("update replay_batches set expectancy_artifact_id=null where expectancy_artifact_id=$1").bind(id).execute(&pool).await.unwrap();
        assert_eq!(db::collect_unreferenced(&pool,0).await.unwrap(),0);
        assert_eq!(db::collect_unreferenced(&pool,1).await.unwrap(),1);
        assert!(db::get(&pool,"admin",id).await.unwrap().is_none());
    }).await;
}

fn png_fixture() -> Vec<u8> {
    fn hex(text: &str) -> Vec<u8> {
        text.as_bytes()
            .as_chunks::<2>().0.iter()
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect()
    }
    // Valid synthetic 900x320 RGB PNG, no new renderer dependency or binary asset.
    let mut png = hex(
        "89504e470d0a1a0a0000000d49484452000003840000014008020000006bca25e70000035c4944415478daedc18100000000c3a0f9531fe00a5501",
    );
    png.extend(std::iter::repeat_n(0, 836));
    png.extend(hex("c01b310300011fc9248e0000000049454e44ae426082"));
    png
}

#[tokio::test]
#[ignore = "requires isolated TEST_DATABASE_URL ending _test with CREATEDB"]
async fn png_exact_key_claim_recovery_retry_limit_and_live_history_reference_are_bounded() {
    isolated(|pool|async move {
        let (job,identity,original)=chain(&pool).await;
        let numerical=artifact(&identity,false);
        let id=db::seal_for_chain(&pool,&job,&identity,&numerical).await.unwrap().unwrap();
        assert!(db::claim_render(&pool,"before-finalize",1).await.unwrap().is_none());
        finish(&pool,&job,&original).await;
        let canonical=ExpectancyArtifact::from_json(&serde_json::to_vec(&numerical).unwrap()).unwrap();
        let key=canonical.image_key(1);
        assert!(db::load_png_by_image_key(&pool,"admin",&key).await.unwrap().is_none());
        let first=db::claim_render(&pool,"renderer-one",1).await.unwrap().unwrap();
        assert!(db::claim_render(&pool,"competing-renderer",1).await.unwrap().is_none());
        // Process restart: reclaim the expired attempt with the same immutable input.
        sqlx::query("update study_expectancy_render_attempts set lease_until=clock_timestamp()-interval '1 second'").execute(&pool).await.unwrap();
        let replacement=db::claim_render(&pool,"renderer-restarted",1).await.unwrap().unwrap();
        assert_eq!(replacement.artifact.id,id);
        assert_eq!(replacement.artifact.numerical_snapshot,numerical);
        db::finish_render(&pool,&first,None,Some("stale_failure")).await.unwrap();
        assert_eq!(sqlx::query_scalar::<_,String>("select lease_owner from study_expectancy_render_attempts").fetch_one(&pool).await.unwrap(),"renderer-restarted");
        let png=png_fixture();
        db::finish_render(&pool,&replacement,Some(&png),None).await.unwrap();
        db::finish_render(&pool,&replacement,Some(&png),None).await.unwrap();
        assert_eq!(db::load_png_by_image_key(&pool,"admin",&key).await.unwrap(),Some(png.clone()));
        assert!(db::load_png_by_image_key(&pool,"other-owner",&key).await.unwrap().is_none());
        assert!(db::claim_render(&pool,"already-cached",1).await.unwrap().is_none());
        assert_eq!(sqlx::query_scalar::<_,i64>("select count(*) from study_expectancy_pngs").fetch_one(&pool).await.unwrap(),1);
        assert_eq!(sqlx::query_scalar::<_,i64>("select count(*) from study_expectancy_render_attempts").fetch_one(&pool).await.unwrap(),0);
        let mut wrong_pair=key.clone();wrong_pair.tp_pct+=1.0;
        let mut wrong_study=key.clone();wrong_study.study_id.push('x');
        let mut wrong_calculation=key.clone();wrong_calculation.calculation_version+=1;
        for tampered in [wrong_pair,wrong_study,wrong_calculation] {
            assert!(db::load_png_by_image_key(&pool,"admin",&tampered).await.is_err());
        }
        let mut other_render=key.clone();other_render.render_version=2;
        assert!(db::load_png_by_image_key(&pool,"admin",&other_render).await.unwrap().is_none());
        let mut tampered=key.clone();tampered.input_snapshot_hash.push('x');
        assert!(db::load_png_by_image_key(&pool,"admin",&tampered).await.is_err());
        let mut missing=key.clone();missing.derived_snapshot_id="0".repeat(64);
        assert!(db::load_png_by_image_key(&pool,"admin",&missing).await.unwrap().is_none());
        let mut changed=png.clone();*changed.last_mut().unwrap()^=1;
        assert!(db::store_png(&pool,"admin",id,"1",&changed).await.is_err());
        for attempt in 1..=3 {
            let claim=db::claim_render(&pool,"bounded-failure",2).await.unwrap().unwrap();
            db::finish_render(&pool,&claim,None,Some("injected_render_failure")).await.unwrap();
            assert!(db::claim_render(&pool,"backoff",2).await.unwrap().is_none());
            assert_eq!(sqlx::query_scalar::<_,i16>("select attempts from study_expectancy_render_attempts where render_version=2").fetch_one(&pool).await.unwrap(),attempt);
            sqlx::query("update study_expectancy_render_attempts set next_attempt_at=clock_timestamp()-interval '1 second'").execute(&pool).await.unwrap();
        }
        assert!(db::claim_render(&pool,"retry-exhausted",2).await.unwrap().is_none());
        let setup:i32=sqlx::query_scalar("insert into live_setups(exchange_code,category,symbol,algorithm_id,algorithm_version,params,open_time,close_time,close_price,details,adaptive_slot_id,adaptive_generation_id) values('bybit','linear','BTCUSDT','price_volume_spike','1.1.1','{}',to_timestamp(0),to_timestamp(60),'100','{}',$1,$2) returning id")
            .bind(job.slot_id).bind(job.generation_id).fetch_one(&pool).await.unwrap();
        assert_eq!(sqlx::query_scalar::<_,i64>("select expectancy_artifact_id from live_setups where id=$1").bind(setup).fetch_one(&pool).await.unwrap(),id);
        sqlx::query("update continuous_generations set expectancy_artifact_id=null where id=$1").bind(job.generation_id).execute(&pool).await.unwrap();
        sqlx::query("update replay_batches set expectancy_artifact_id=null where expectancy_artifact_id=$1").bind(id).execute(&pool).await.unwrap();
        assert_eq!(db::collect_unreferenced(&pool,100).await.unwrap(),0,"published live row alone preserves image and numbers for history/pending jobs");
        assert_eq!(db::load_png_by_image_key(&pool,"admin",&key).await.unwrap(),Some(png));
        sqlx::query("delete from live_setups where id=$1").bind(setup).execute(&pool).await.unwrap();
        assert_eq!(db::collect_unreferenced(&pool,1).await.unwrap(),1);
        assert_eq!(sqlx::query_scalar::<_,i64>("select count(*) from study_expectancy_pngs").fetch_one(&pool).await.unwrap(),0);
        assert_eq!(sqlx::query_scalar::<_,i64>("select count(*) from study_expectancy_render_attempts").fetch_one(&pool).await.unwrap(),0);
    }).await;
}

#[test]
fn identity_key_is_order_independent_and_bounded() {
    let mut identity = db::ArtifactIdentity {
        owner_id: "admin".into(),
        generation_id: 1,
        study_id: 2,
        slot_id: 3,
        calculation_version: "2".into(),
        historical_input_hash: None,
        tp_pct: 2.0,
        sl_pct: 1.0,
        from_unix: 86400,
        to_unix: 172800,
        observation_cutoff: 172800,
        provenance: json!({"a":1,"b":2}),
        effective_analytics: json!({}),
    };
    let key = db::artifact_key(&identity).unwrap();
    let old_json=serde_json::to_value(&identity).unwrap();
    assert!(old_json.get("historicalInputHash").is_none(),"new None must not alter old identity bytes/hash");
    let old_identity:db::ArtifactIdentity=serde_json::from_value(old_json).unwrap();
    assert_eq!(db::artifact_key(&old_identity).unwrap(),key);
    identity.provenance = serde_json::from_str("{\"b\":2,\"a\":1}").unwrap();
    assert_eq!(db::artifact_key(&identity).unwrap(), key);
    identity.provenance = json!({"huge":"x".repeat(db::MAX_IDENTITY_BYTES)});
    assert!(db::artifact_key(&identity).is_err());
}

#[tokio::test]
#[ignore = "requires isolated TEST_DATABASE_URL ending _test with CREATEDB"]
async fn historical_revision_stages_without_rewriting_history_then_publishes_cas_and_is_idempotent() {
    isolated(|pool|async move {
        let (job,identity,original)=chain(&pool).await;finish(&pool,&job,&original).await;
        let original_id=db::seal_existing(&pool,&identity,&artifact(&identity,true)).await.unwrap();
        sqlx::query("update continuous_slots set active_generation_id=$2,status='active' where id=$1").bind(job.slot_id).bind(job.generation_id).execute(&pool).await.unwrap();
        let admission=db::admit_historical_recalculation(&pool,"admin",job.generation_id).await.unwrap().unwrap();
        let before:(String,String,String)=sqlx::query_as("select g.result::text,s.result::text,a.numerical_snapshot::text from continuous_generations g join optimization_studies s on s.id=g.study_id join study_expectancy_artifacts a on a.id=g.expectancy_artifact_id where g.id=$1").bind(job.generation_id).fetch_one(&pool).await.unwrap();
        let provenance=db::HistoricalRecalculationProvenance {revision:1,input_hash:"a".repeat(64),raw_setups_hash:"b".repeat(64),
            recalculated_at_unix:chrono::Utc::now().timestamp()-60,data_source:"existing_market_data_history".into(),
            read_from_unix:identity.from_unix,read_to_unix:identity.observation_cutoff,candle_count:3,missing_minutes:0};
        let mut revised=identity.clone();revised.historical_input_hash=Some(provenance.input_hash.clone());
        let numerical=artifact(&revised,false);
        let expected=db::historical_expected_comparison(&admission.source,2.0,1.0).unwrap();
        let comparison=json!({"expected":expected,"actual":expected});
        let stage=db::stage_historical_recalculation(&pool,&admission,&provenance,&comparison,Some(&numerical)).await.unwrap().unwrap();
        assert!(stage.matched && stage.artifact_id!=Some(original_id));
        assert_eq!(db::get_for_generation(&pool,"admin",job.generation_id,"2").await.unwrap().unwrap().id,original_id);
        assert!(db::get_published_recalculation(&pool,"admin",job.generation_id,"2").await.unwrap().is_none());
        let mut retry_provenance=provenance.clone();retry_provenance.recalculated_at_unix+=10;
        let retry=db::stage_historical_recalculation(&pool,&admission,&retry_provenance,&comparison,Some(&numerical)).await.unwrap().unwrap();
        assert_eq!(stage.id,retry.id);assert_eq!(retry.recalculation,provenance,"lost ACK preserves original operation/time");
        assert_eq!(db::collect_unreferenced(&pool,100).await.unwrap(),0,"staging and original anchors own both rows");
        let published=db::publish_historical_recalculation(&pool,"admin",job.generation_id,&provenance.input_hash).await.unwrap().unwrap();
        assert_eq!(published.artifact.id,stage.artifact_id.unwrap());assert_eq!(published.original_artifact_id,original_id);
        assert!(published.available_from_unix>=chrono::Utc::now().timestamp()-1);
        let again=db::publish_historical_recalculation(&pool,"admin",job.generation_id,&provenance.input_hash).await.unwrap().unwrap();
        assert_eq!(again.available_from_unix,published.available_from_unix);
        assert_eq!(db::get_for_generation(&pool,"admin",job.generation_id,"2").await.unwrap().unwrap().id,original_id);
        let after:(String,String,String)=sqlx::query_as("select g.result::text,s.result::text,a.numerical_snapshot::text from continuous_generations g join optimization_studies s on s.id=g.study_id join study_expectancy_artifacts a on a.id=g.expectancy_artifact_id where g.id=$1").bind(job.generation_id).fetch_one(&pool).await.unwrap();
        assert_eq!(before,after);
        assert!(sqlx::query("update study_expectancy_recalculations set matched=false where id=$1").bind(stage.id).execute(&pool).await.is_err());
        sqlx::query("update continuous_processes set desired_running=false where owner_id='admin'").execute(&pool).await.unwrap();
        assert!(db::publish_historical_recalculation(&pool,"admin",job.generation_id,&provenance.input_hash).await.unwrap().is_none());
        assert!(db::stage_historical_recalculation(&pool,&admission,&provenance,&comparison,Some(&numerical)).await.unwrap().is_none());
    }).await;
}

#[tokio::test]
#[ignore = "requires isolated TEST_DATABASE_URL ending _test with CREATEDB"]
async fn historical_mismatch_is_immutable_evidence_not_a_publishable_artifact() {
    isolated(|pool|async move {
        let (job,identity,original)=chain(&pool).await;finish(&pool,&job,&original).await;
        db::seal_existing(&pool,&identity,&artifact(&identity,true)).await.unwrap();
        sqlx::query("update continuous_slots set active_generation_id=$2,status='active' where id=$1").bind(job.slot_id).bind(job.generation_id).execute(&pool).await.unwrap();
        let admission=db::admit_historical_recalculation(&pool,"admin",job.generation_id).await.unwrap().unwrap();
        let provenance=db::HistoricalRecalculationProvenance {revision:1,input_hash:"c".repeat(64),raw_setups_hash:"d".repeat(64),
            recalculated_at_unix:chrono::Utc::now().timestamp(),data_source:"existing_market_data_history".into(),
            read_from_unix:identity.from_unix,read_to_unix:identity.observation_cutoff,candle_count:2,missing_minutes:1};
        let expected=db::historical_expected_comparison(&admission.source,2.0,1.0).unwrap();
        let mut actual=expected.clone();actual["selectedCell"]["tpCount"]=json!(1);
        let comparison=json!({"expected":expected,"actual":actual});
        let stage=db::stage_historical_recalculation(&pool,&admission,&provenance,&comparison,None).await.unwrap().unwrap();
        assert!(!stage.matched && stage.artifact_id.is_none());
        assert!(db::publish_historical_recalculation(&pool,"admin",job.generation_id,&provenance.input_hash).await.is_err());
        assert_eq!(sqlx::query_scalar::<_,i64>("select count(*) from study_expectancy_artifacts").fetch_one(&pool).await.unwrap(),1);
        assert_eq!(db::get_historical_stage(&pool,"admin",job.generation_id,&provenance.input_hash).await.unwrap().unwrap().comparison,comparison);
        sqlx::query("update continuous_slots set active_generation_id=null where id=$1").bind(job.slot_id).execute(&pool).await.unwrap();
        assert!(db::publish_historical_recalculation(&pool,"admin",job.generation_id,&provenance.input_hash).await.unwrap().is_none());
    }).await;
}
