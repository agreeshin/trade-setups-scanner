//! Real, mandatory PostgreSQL checks for the NEW-setup captured-artifact FK.
//! Each test owns and removes a unique database; no market/network calls.
use serde_json::{Value, json};
use setups_db::{continuous as db, expectancy as artifacts};
use setups_domain::{
    AlgorithmParams, RecommendationDirection, RecommendationProvenance, SpikeParams, SpikeSetup,
    expectancy::*,
};
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
        &std::env::var("TEST_DATABASE_URL").expect("isolated TEST_DATABASE_URL required"),
    )
    .unwrap();
    assert!(
        options
            .get_database()
            .is_some_and(|name| name.ends_with("_test"))
    );
    let admin = PgPoolOptions::new()
        .max_connections(2)
        .connect_with(options.clone())
        .await
        .unwrap();
    let name = format!(
        "historical_reference_{}_{}_test",
        std::process::id(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap()
    );
    sqlx::query(&format!("create database {name}"))
        .execute(&admin)
        .await
        .unwrap();
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect_with(options.database(&name))
        .await
        .unwrap();
    let child = pool.clone();
    let result=tokio::task::LocalSet::new().run_until(async move {tokio::task::spawn_local(async move {
        let baseline=include_str!("../migrations/0010_baseline.sql").lines()
            .filter(|line|!line.starts_with('\\') && !line.starts_with("SET transaction_timeout") && !line.contains("set_config('search_path'"))
            .collect::<Vec<_>>().join("\n");
        sqlx::raw_sql(&baseline).execute(&child).await.unwrap();
        sqlx::raw_sql("set search_path to public;create table kysely_migration(name varchar(255) primary key,timestamp varchar(255) not null);insert into kysely_migration values('0009_legacy','fixture')").execute(&child).await.unwrap();
        setups_db::migrate::ensure_migrations(&child).await.unwrap();
        body(child).await;
    }).await}).await;
    pool.close().await;
    sqlx::query(&format!("drop database {name} with(force)"))
        .execute(&admin)
        .await
        .unwrap();
    assert!(
        !sqlx::query_scalar::<_, bool>("select exists(select 1 from pg_database where datname=$1)")
            .bind(&name)
            .fetch_one(&admin)
            .await
            .unwrap()
    );
    admin.close().await;
    if let Err(error) = result {
        std::panic::resume_unwind(error.into_panic());
    }
}

struct Fixture {
    slot: db::ActiveAdaptiveSlot,
    base: i64,
    a: i64,
    b: i64,
    operation_a: i64,
    operation_b: i64,
    key_a: String,
    actual_a: Value,
    provenance: RecommendationProvenance,
    raw: i64,
}

async fn insert_artifact(pool: &PgPool, identity: &artifacts::ArtifactIdentity) -> (i64, String) {
    let key = artifacts::artifact_key(identity).unwrap();
    let numerical = build_expectancy_artifact(ExpectancyInput {
        source: ExpectancySource {
            forecast: None,
            study_id: identity.study_id.to_string(),
            derived_snapshot_id: key.clone(),
            generation_id: Some(identity.generation_id.to_string()),
            profile_id: Some(identity.slot_id.to_string()),
            provenance: RecommendationProvenance {
                algorithm_id: "price_volume_spike".into(),
                algorithm_version: "1.1.1".into(),
                optimizer_version: "fixture-v1".into(),
                seed: 42,
                snapshot_hash: "original-frozen-input".into(),
                captured_at: "2026-11-12T00:00:00Z".into(),
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
        },
        selected_cell: Some(SelectedCellInput {
            raw_r: Some(0.5),
            tp_count: 1,
            sl_count: 1,
        }),
        matrix_mean_r: Some(0.5),
        daily: if identity.historical_input_hash.is_none() {
            MetricAvailability::Unavailable {
                reason: ExpectancyUnavailableReason::MissingFrozenOutcomes,
            }
        } else {
            let first = utc_midnight_unix(identity.from_unix).unwrap() + 86400;
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
                        day_start_unix: first + 86400,
                        row_count: 1,
                        tp_count: 0,
                        sl_count: 1,
                        mean_r: Some(-1.0),
                    },
                ],
            }
        },
    })
    .unwrap();
    let id=sqlx::query_scalar("insert into study_expectancy_artifacts(artifact_key,owner_id,source_generation_id,source_study_id,source_slot_id,calculation_version,tp_pct,sl_pct,identity,numerical_snapshot,revision_key) values($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11) returning id")
        .bind(&key).bind(&identity.owner_id).bind(identity.generation_id).bind(identity.study_id).bind(identity.slot_id).bind(&identity.calculation_version)
        .bind(identity.tp_pct).bind(identity.sl_pct).bind(serde_json::to_value(identity).unwrap()).bind(serde_json::to_value(numerical).unwrap())
        .bind(identity.historical_input_hash.as_deref().unwrap_or("original")).fetch_one(pool).await.unwrap();
    (id, key)
}

async fn fixture(pool: &PgPool) -> Fixture {
    db::profiles::set_permission(pool, "admin", "price_volume_spike", true)
        .await
        .unwrap();
    db::configure(
        pool,
        "admin",
        "price_volume_spike",
        &json!({"adaptiveEnabled":true}),
        &[0.0],
    )
    .await
    .unwrap();
    db::start(pool, "admin", "price_volume_spike")
        .await
        .unwrap();
    setups_db::settings_set(pool, "live", &json!({"enabled":true}))
        .await
        .unwrap();
    let slot: i64 = sqlx::query_scalar("select min(id) from continuous_slots")
        .fetch_one(pool)
        .await
        .unwrap();
    let raw = chrono::Utc::now().timestamp().div_euclid(60) * 60;
    let provenance = RecommendationProvenance {
        algorithm_id: "price_volume_spike".into(),
        algorithm_version: "1.1.1".into(),
        optimizer_version: "fixture-v1".into(),
        seed: 42,
        snapshot_hash: "original-frozen-input".into(),
        captured_at: "2026-11-12T00:00:00Z".into(),
        from_unix: raw - 4 * 86400,
        to_unix: raw - 86400,
        observation_cutoff: raw - 86400,
    };
    let analytics = json!({"clusterEnabled":false,"clusterGapMinutes":null,"timeShiftMinutes":0,"priceChangePct":null,"horizonMinutes":60});
    let spec = json!({"algorithmId":"price_volume_spike","algorithmVersion":"1.1.1","direction":"short","horizonMinutes":60,"fromUnix":provenance.from_unix,"toUnix":provenance.to_unix});
    let selected = json!({"tpPct":2.0,"slPct":1.0,"rawR":0.5});
    let result = json!({"best":{"params":SpikeParams::default(),"analytics":analytics},"selectedCell":selected,"scienceSentinel":1.2345678901234567});
    let generation:i64=sqlx::query_scalar("insert into continuous_generations(owner_id,algorithm_id,slot_id,config_revision,lease_token,status,phase,spec,result,accepted,execution_started_at,observation_cutoff) values('admin','price_volume_spike',$1,1,1,'completed','completed',$2,$3,true,now(),$4) returning id")
        .bind(slot).bind(&spec).bind(&result).bind(provenance.observation_cutoff).fetch_one(pool).await.unwrap();
    let study:i64=sqlx::query_scalar("insert into optimization_studies(algorithm_id,status,spec,progress,result,owner_id,origin,continuous_generation_id,continuous_slot_id) values('price_volume_spike','completed',$1,'{}',$2,'admin','continuous',$3,$4) returning id")
        .bind(&spec).bind(&result).bind(generation).bind(slot).fetch_one(pool).await.unwrap();
    sqlx::query("update continuous_generations set study_id=$2 where id=$1")
        .bind(generation)
        .bind(study)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("update continuous_slots set active_generation_id=$2,study_id=$3,mean_expectancy=0.5,selected_cell=$4,status='active' where id=$1").bind(slot).bind(generation).bind(study).bind(selected).execute(pool).await.unwrap();
    sqlx::query("insert into adaptive_profiles(id,owner_id,algorithm_id,min_setups_per_day,accepted_generation_id) select id,owner_id,algorithm_id,min_setups_per_day,$2 from continuous_slots where id=$1")
        .bind(slot).bind(generation).execute(pool).await.unwrap();
    let identity = artifacts::ArtifactIdentity {
        owner_id: "admin".into(),
        generation_id: generation,
        study_id: study,
        slot_id: slot,
        calculation_version: EXPECTANCY_CALCULATION_VERSION.to_string(),
        tp_pct: 2.0,
        sl_pct: 1.0,
        from_unix: provenance.from_unix,
        to_unix: provenance.to_unix,
        observation_cutoff: provenance.observation_cutoff,
        provenance: serde_json::to_value(&provenance).unwrap(),
        effective_analytics: analytics,
        historical_input_hash: None,
        forecast: None,
        outcome_hash: None,
    };
    let (base, _) = insert_artifact(pool, &identity).await;
    sqlx::query("update continuous_generations set expectancy_artifact_id=$2 where id=$1")
        .bind(generation)
        .bind(base)
        .execute(pool)
        .await
        .unwrap();
    let mut revised = identity.clone();
    revised.historical_input_hash = Some("a".repeat(64));
    let (a, key_a) = insert_artifact(pool, &revised).await;
    revised.historical_input_hash = Some("b".repeat(64));
    let (b, _) = insert_artifact(pool, &revised).await;
    let actual_a = json!({"revision":1,"inputHash":"a".repeat(64),"rawSetupsHash":"c".repeat(64),"recalculatedAtUnix":raw-120,"dataSource":"existing_market_data_history","readFromUnix":provenance.from_unix,"readToUnix":provenance.observation_cutoff,"candleCount":120,"missingMinutes":0});
    let mut operations = Vec::new();
    for (id, hash) in [(a, "a".repeat(64)), (b, "b".repeat(64))] {
        let mut actual = actual_a.clone();
        actual["inputHash"] = json!(hash);
        let operation:i64=sqlx::query_scalar("insert into study_expectancy_recalculations(owner_id,generation_id,study_id,slot_id,config_revision,revision_key,original_artifact_id,artifact_id,provenance,comparison,matched) values('admin',$1,$2,$3,1,$4,$5,$6,$7,'{\"expected\":{},\"actual\":{}}',true) returning id")
            .bind(generation).bind(study).bind(slot).bind(hash).bind(base).bind(id).bind(actual).fetch_one(pool).await.unwrap();
        operations.push(operation);
    }
    sqlx::query("insert into study_expectancy_publications(owner_id,generation_id,slot_id,recalculation_id,artifact_id,available_from_unix) values('admin',$1,$2,$3,$4,$5)")
        .bind(generation).bind(slot).bind(operations[0]).bind(a).bind(raw-60).execute(pool).await.unwrap();
    Fixture {
        slot: db::active_adaptive_slots(pool, "admin")
            .await
            .unwrap()
            .remove(0),
        base,
        a,
        b,
        operation_a: operations[0],
        operation_b: operations[1],
        key_a,
        actual_a,
        provenance,
        raw,
    }
}

fn setup(f: &Fixture, symbol: &str) -> db::AdaptiveSetupWrite {
    db::AdaptiveSetupWrite {
        exchange_code: "bybit".into(),
        category: "linear".into(),
        symbol: symbol.into(),
        setup: SpikeSetup {
            algorithm_id: "price_volume_spike".into(),
            algorithm_version: "1.1.1".into(),
            params: AlgorithmParams::Spike(SpikeParams::default()),
            open_time_unix: f.raw - 60,
            close_time_unix: f.raw,
            close_price: "100".into(),
            details: None,
        },
        recommendation: json!({"ownerId":"admin","studyId":f.slot.study_id,"generationId":f.slot.generation_id,"slotId":f.slot.slot_id,"direction":"short","tpPct":2.0,"slPct":1.0,"rawR":0.5,"meanR":0.5,"timeShiftMinutes":0,"priceChangePct":null,"clusterEnabled":false,"clusterGapMinutes":null,"provenance":f.provenance,"rawSignalAt":f.raw,
            "expectancy":{"calculationVersion":2,"studyId":f.slot.study_id,"generationId":f.slot.generation_id,"slotId":f.slot.slot_id,"artifactId":f.a,"derivedSnapshotId":f.key_a,"sourceSnapshotHash":f.provenance.snapshot_hash,"sourceCutoff":f.provenance.observation_cutoff,"fromUnix":f.provenance.from_unix,"toUnix":f.provenance.to_unix,"forecastAtUnix":f.raw,
                "historicalRecalculation":{"originalArtifactId":f.base,"revisionKey":"a".repeat(64),"availableFromUnix":f.raw-60,"provenance":f.actual_a}}}),
    }
}

#[tokio::test]
async fn captured_a_survives_preferred_b_for_single_batch_retry_and_real_fk_retention() {
    isolated(|pool|async move {
        let f=fixture(&pool).await;
        let single=setup(&f,"SINGLE");
        let frozen=single.recommendation.clone();
        // Test-only preference promotion AFTER A was captured, before canonical
        // NEW setup insertion. Never update an artifact or an existing setup.
        sqlx::query("update study_expectancy_publications set artifact_id=$2,recalculation_id=$3 where generation_id=$1")
            .bind(f.slot.generation_id).bind(f.b).bind(f.operation_b).execute(&pool).await.unwrap();
        let checkpoint=json!({"nextOpenUnix":f.raw});
        let result=db::commit_adaptive_scan_cas(&pool,&f.slot,"bybit/linear/SINGLE",None,&checkpoint,std::slice::from_ref(&single)).await.unwrap();
        assert!(result.applied);assert_eq!(result.inserted.len(),1);
        let scans:Vec<_>=["BATCH1","BATCH2"].into_iter().map(|symbol|db::AdaptiveScanWrite {instrument_key:format!("bybit/linear/{symbol}"),previous:None,checkpoint:checkpoint.clone(),setups:vec![setup(&f,symbol)]}).collect();
        assert!(db::commit_adaptive_scans_cas(&pool,&f.slot,&scans).await.unwrap().iter().all(|result|result.applied && result.inserted.len()==1));
        let references:Vec<(i64,Value)>=sqlx::query_as("select expectancy_artifact_id,recommendation from live_setups order by id").fetch_all(&pool).await.unwrap();
        assert_eq!(references.len(),3);
        assert!(references.iter().all(|(id,snapshot)|*id==f.a && *snapshot==frozen));
        let before:String=sqlx::query_scalar("select result::text from continuous_generations where id=$1").bind(f.slot.generation_id).fetch_one(&pool).await.unwrap();
        let previous=db::AdaptiveCheckpoint {generation_id:f.slot.generation_id,state:checkpoint};
        let retry=db::commit_adaptive_scan_cas(&pool,&f.slot,"bybit/linear/SINGLE",Some(&previous),&json!({"nextOpenUnix":f.raw+60}),&[single]).await.unwrap();
        assert!(retry.applied && retry.inserted.is_empty());
        assert_eq!(sqlx::query_scalar::<_,Value>("select recommendation from live_setups where symbol='SINGLE'").fetch_one(&pool).await.unwrap(),frozen);
        assert_eq!(sqlx::query_scalar::<_,Value>("select payload->'recommendation' from adaptive_notification_outbox where setup_id=$1").bind(result.inserted[0].id as i32).fetch_one(&pool).await.unwrap(),frozen);
        assert_eq!(sqlx::query_scalar::<_,String>("select result::text from continuous_generations where id=$1").bind(f.slot.generation_id).fetch_one(&pool).await.unwrap(),before);
        assert_eq!(artifacts::collect_unreferenced(&pool,100).await.unwrap(),0);
        assert!(sqlx::query("delete from study_expectancy_artifacts where id=$1").bind(f.a).execute(&pool).await.is_err(),"real live FK must retain A even though B is preferred");
        assert_eq!(sqlx::query_scalar::<_,i64>("select original_artifact_id from study_expectancy_recalculations where id=$1").bind(f.operation_a).fetch_one(&pool).await.unwrap(),f.base);
    }).await;
}

#[tokio::test]
async fn absent_or_null_id_keeps_legacy_base_reference_without_rewriting_recommendations() {
    isolated(|pool| async move {
        let f = fixture(&pool).await;
        for (index, kind) in ["absent", "null"].into_iter().enumerate() {
            let mut row = setup(&f, kind);
            if index == 0 {
                row.recommendation
                    .as_object_mut()
                    .unwrap()
                    .remove("expectancy");
            } else {
                row.recommendation["expectancy"]["artifactId"] = Value::Null;
            }
            let original = row.recommendation.clone();
            let commit = db::commit_adaptive_scan_cas(
                &pool,
                &f.slot,
                &format!("bybit/linear/{kind}"),
                None,
                &json!({"nextOpenUnix":f.raw}),
                &[row],
            )
            .await
            .unwrap();
            let saved: (i64, Value) = sqlx::query_as(
                "select expectancy_artifact_id,recommendation from live_setups where id=$1",
            )
            .bind(commit.inserted[0].id as i32)
            .fetch_one(&pool)
            .await
            .unwrap();
            assert_eq!(saved, (f.base, original));
        }
    })
    .await;
}

#[tokio::test]
async fn invalid_explicit_reference_never_falls_back_and_batch_rolls_back_all_siblings() {
    isolated(|pool|async move {
        let f=fixture(&pool).await;
        for bad in 0..11 {
            let mut invalid=setup(&f,"INVALID");
            match bad {
                0=>invalid.recommendation["expectancy"]["artifactId"]=json!(i64::MAX),
                1=>invalid.recommendation["expectancy"]["artifactId"]=json!("not-an-id"),
                2=>invalid.recommendation["expectancy"]["artifactId"]=json!(-1),
                3=>invalid.recommendation["expectancy"]["derivedSnapshotId"]=json!("b".repeat(64)),
                4=>invalid.recommendation["ownerId"]=json!("another-owner"),
                5=>invalid.recommendation["expectancy"]["generationId"]=json!(f.slot.generation_id+1),
                6=>invalid.recommendation["expectancy"]["slotId"]=json!(f.slot.slot_id+1),
                7=>invalid.recommendation["expectancy"]["sourceSnapshotHash"]=json!("another-frozen-input"),
                8=>invalid.recommendation["expectancy"]["historicalRecalculation"]["availableFromUnix"]=json!(f.raw+1),
                9=>invalid.recommendation["expectancy"]["historicalRecalculation"]["provenance"]["inputHash"]=json!("b".repeat(64)),
                _=>invalid.recommendation["expectancy"]["historicalRecalculation"]["originalArtifactId"]=json!(f.b),
            }
            let error=db::commit_adaptive_scan_cas(&pool,&f.slot,"bybit/linear/INVALID",None,&json!({"nextOpenUnix":f.raw}),std::slice::from_ref(&invalid)).await.unwrap_err();
            assert!(error.to_string().contains("adaptive_expectancy_reference_invalid"),"case {bad}: {error}");
            let scans=[db::AdaptiveScanWrite {instrument_key:"bybit/linear/AAA_VALID".into(),previous:None,checkpoint:json!({"nextOpenUnix":f.raw}),setups:vec![setup(&f,"AAA_VALID")]},
                db::AdaptiveScanWrite {instrument_key:"bybit/linear/INVALID".into(),previous:None,checkpoint:json!({"nextOpenUnix":f.raw}),setups:vec![invalid]}];
            assert!(db::commit_adaptive_scans_cas(&pool,&f.slot,&scans).await.unwrap_err().to_string().contains("adaptive_expectancy_reference_invalid"));
            assert_eq!(sqlx::query_scalar::<_,i64>("select count(*) from live_setups").fetch_one(&pool).await.unwrap(),0);
            assert_eq!(sqlx::query_scalar::<_,i64>("select count(*) from adaptive_live_checkpoints").fetch_one(&pool).await.unwrap(),0);
            assert_eq!(sqlx::query_scalar::<_,i64>("select count(*) from adaptive_notification_outbox").fetch_one(&pool).await.unwrap(),0);
        }
    }).await;
}
