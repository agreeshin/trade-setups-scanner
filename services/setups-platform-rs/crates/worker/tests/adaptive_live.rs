//! Isolated PostgreSQL + local HTTP source; no production or external delivery.
use axum::{
    Json, Router,
    extract::{Query, State},
    routing::get,
};
use serde::Deserialize;
use serde_json::{Value, json};
use setups_db::{continuous, settings_set};
use setups_domain::{AlgorithmParams, SpikeParams};
use setups_optimizer::{ContinuousReplay, StudyRequest, continuous_bootstrap};
use setups_worker::adaptive::{
    AdaptiveCheckpoint, AdaptiveFence, AdaptiveInstrument, AdaptiveStore, PgAdaptiveStore,
    adaptive_live_tick,
};
use sqlx::{
    Connection, PgConnection, PgPool, Row,
    postgres::{PgConnectOptions, PgPoolOptions},
};
use std::{
    str::FromStr,
    sync::{Arc, Mutex, atomic::AtomicBool},
};

struct Database {
    pool: PgPool,
    admin: PgConnection,
    name: String,
}
impl Database {
    async fn new(label: &str) -> Option<Self> {
        let Ok(url) = std::env::var("TEST_DATABASE_URL") else {
            eprintln!("TEST_DATABASE_URL absent; isolated worker test skipped");
            return None;
        };
        let options = PgConnectOptions::from_str(&url).unwrap();
        assert!(
            options.get_database().unwrap_or("").ends_with("_test"),
            "test database must use _test suffix"
        );
        let name = format!("adaptive_worker_{label}_{}_test", std::process::id());
        let mut admin = PgConnection::connect_with(&options).await.unwrap();
        sqlx::query(&format!("CREATE DATABASE {name}"))
            .execute(&mut admin)
            .await
            .unwrap();
        let pool = PgPoolOptions::new()
            .max_connections(5)
            .connect_with(options.database(&name))
            .await
            .unwrap();
        // The legacy baseline is a pg_dump script with psql-only \restrict
        // metadata, not raw SQL. Bootstrap this isolated fixture from the same
        // canonical dump without those client directives, then exercise the
        // actual additive migration runner. Production migration code is not
        // changed by this worker-owned test fixture.
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
        sqlx::raw_sql("set search_path=public; create table kysely_migration(name varchar(255) primary key, timestamp varchar(255) not null); insert into kysely_migration values('0010_baseline.sql','fixture');")
            .execute(&mut *connection).await.unwrap();
        drop(connection);
        setups_db::migrate::ensure_migrations(&pool).await.unwrap();
        Some(Self { pool, admin, name })
    }
    async fn close(mut self) {
        self.pool.close().await;
        // Only this fixture's fresh *_test database; pool close can race the
        // server observing the last TCP disconnect by a few milliseconds.
        sqlx::query(&format!("DROP DATABASE {} WITH (FORCE)", self.name))
            .execute(&mut self.admin)
            .await
            .unwrap();
    }
}

fn params() -> AlgorithmParams {
    AlgorithmParams::Spike(SpikeParams {
        baseline_long_days: 1,
        baseline_short_days: 1,
        window_minutes: 1,
        price_growth_pct: "0".into(),
        volume_multiplier: "0.5".into(),
        cooldown_minutes: 1,
        ..SpikeParams::default()
    })
}

fn profile(now: i64) -> (Value, Value) {
    let params = params();
    let params = serde_json::to_value(params).unwrap();
    let end = now - 600;
    let next = end - 60;
    let instruments = json!([
        {"exchange":"bybit","category":"linear","symbol":"BTCUSDT"},
        {"exchange":"bybit","category":"linear","symbol":"DELISTUSDT"},
    ]);
    let spec = json!({"algorithmId":"price_volume_spike","algorithmVersion":"1.1.1",
        "algorithmParams":params,"analyticsParams":{"clusterEnabled":false,"clusterGapMinutes":480,"timeShiftMinutes":3,"priceChangePct":1000},
        "instruments":instruments,"fromUnix":end-86400,"toUnix":end,"direction":"short","seed":42,"minSetupsPerDay":0});
    let provenance = json!({"optimizerVersion":setups_optimizer::CONTINUOUS_OPTIMIZER_VERSION,"seed":42,"snapshotHash":"frozen-worker-fixture","capturedAt":"2026-11-12T00:00:00Z"});
    let raw = json!({"algorithmId":"price_volume_spike","algorithmVersion":"1.1.1","openTimeUnix":next-60,"closeTimeUnix":next,"closePrice":"100","params":params});
    let replay:ContinuousReplay=serde_json::from_value(json!({
        "fromUnix":end-86400,"toUnix":end,"context":{"observationCutoff":end},"params":params,"provenance":provenance,
        "instruments":instruments.as_array().unwrap().iter().map(|instrument|json!({"instrument":instrument,"setups":[raw],"previousRawCloseUnix":null,"lastRawCloseUnix":next,"evaluatedMinutes":1,"warmupMinutes":1440})).collect::<Vec<_>>(),
    })).unwrap();
    let request: StudyRequest = serde_json::from_value(spec.clone()).unwrap();
    let bootstrap = continuous_bootstrap(&request, &replay).unwrap();
    let recommendation = json!({"direction":"short","tpPct":14,"slPct":4,"rawR":0.32,"meanR":0.21,"robustScore":0.29,
        "timeShiftMinutes":3,"priceChangePct":1000,"clusterEnabled":false,"clusterGapMinutes":null,
        "provenance":{"algorithmId":"price_volume_spike","algorithmVersion":"1.1.1","optimizerVersion":setups_optimizer::CONTINUOUS_OPTIMIZER_VERSION,"seed":42,
        "snapshotHash":"frozen-worker-fixture","capturedAt":"2026-11-12T00:00:00Z","fromUnix":end-86400,"toUnix":end,"observationCutoff":end}});
    let result = json!({"best":{"params":params,"analytics":{"clusterEnabled":false,"clusterGapMinutes":null,"timeShiftMinutes":3,"priceChangePct":1000}},
        "selectedCell":{"tpPct":14,"slPct":4,"rawR":0.32,"meanR":0.21,"robustScore":0.29},"recommendation":recommendation,"bootstrap":bootstrap});
    (spec, result)
}

async fn configure(pool: &PgPool) {
    continuous::configure(
        pool,
        "admin",
        "price_volume_spike",
        &json!({"adaptiveEnabled":true}),
        &[0.0, 1.0],
    )
    .await
    .unwrap();
    continuous::start(pool, "admin", "price_volume_spike")
        .await
        .unwrap();
    settings_set(pool, "live", &json!({"enabled":true}))
        .await
        .unwrap();
    settings_set(pool, "notifications", &json!({"maxAgeSeconds":0}))
        .await
        .unwrap();
    settings_set(
        pool,
        "algorithm:price_volume_spike",
        &json!({"enabled":false,"externalName":"Saved <plain> name"}),
    )
    .await
    .unwrap();
}

// Test-only accepted generation fixture. All production persistence goes through
// backend exports, not worker-owned SQL or a shadow schema.
async fn activate(pool: &PgPool, slot_id: i64, spec: &Value, result: &Value) {
    let study:i64=sqlx::query_scalar("insert into optimization_studies(algorithm_id,status,spec,progress,origin) values('price_volume_spike','completed',$1,'{}','continuous') returning id")
        .bind(spec).fetch_one(pool).await.unwrap();
    let generation:i64=sqlx::query_scalar("insert into continuous_generations(owner_id,algorithm_id,slot_id,config_revision,lease_token,status,phase,spec,execution_started_at,observation_cutoff,study_id,result,accepted) values('admin','price_volume_spike',$1,1,1,'completed','completed',$2,now(),$3,$4,$5,true) returning id")
        .bind(slot_id).bind(spec).bind(spec["toUnix"].as_i64().unwrap()).bind(study).bind(result).fetch_one(pool).await.unwrap();
    sqlx::query("update continuous_slots set active_generation_id=$2,study_id=$3,mean_expectancy=0.21,selected_cell=$4,status='active' where id=$1")
        .bind(slot_id).bind(generation).bind(study).bind(&result["selectedCell"]).execute(pool).await.unwrap();
}

#[derive(Clone)]
struct Market {
    now: i64,
    reads: Arc<Mutex<Vec<i64>>>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CandleQuery {
    instrument_id: i64,
    from_unix: i64,
    to_unix: i64,
    after_unix: Option<i64>,
    limit: Option<usize>,
}
async fn candles(State(state): State<Market>, Query(query): Query<CandleQuery>) -> Json<Value> {
    state.reads.lock().unwrap().push(query.instrument_id);
    let start = query.from_unix.max(state.now - 3 * 86400);
    let mut candles:Vec<_>=(start..query.to_unix.min(state.now)).step_by(60).filter(|time|query.after_unix.is_none_or(|after|*time>after))
        .take(query.limit.unwrap_or(256)+1).map(|time|json!({"openTimeUnix":time,"open":"100","high":"100","low":"100","close":"100","volume":"1","quoteTurnover":"1"})).collect();
    let more = candles.len() > query.limit.unwrap_or(256);
    if more {
        candles.pop();
    }
    let next = more.then(|| candles.last().unwrap()["openTimeUnix"].as_i64().unwrap());
    Json(json!({"candles":candles,"nextCursorUnix":next}))
}
async fn catalog() -> Json<Value> {
    Json(json!({"instruments":[
        {"id":1,"exchange":"bybit","category":"linear","symbol":"BTCUSDT","status":"active"},
        {"id":2,"exchange":"bybit","category":"linear","symbol":"DELISTUSDT","status":"inactive"},
        {"id":3,"exchange":"bybit","category":"linear","symbol":"NOTINSTUDYUSDT","status":"active"}
    ]}))
}

#[tokio::test]
async fn actual_tick_uses_frozen_seeds_listing_intersection_and_independent_live_permission() {
    let Some(db) = Database::new("runtime").await else {
        return;
    };
    configure(&db.pool).await;
    let original_name = "策".repeat(120);
    assert_eq!(original_name.chars().count(), 120);
    assert_eq!(original_name.len(), 360);
    settings_set(
        &db.pool,
        "algorithm:price_volume_spike",
        &json!({"enabled":false,"externalName":original_name}),
    )
    .await
    .unwrap();
    let now = chrono::Utc::now().timestamp().div_euclid(60) * 60;
    let (spec, result) = profile(now);
    let ids: Vec<i64> = sqlx::query_scalar("select id from continuous_slots order by id")
        .fetch_all(&db.pool)
        .await
        .unwrap();
    activate(&db.pool, ids[0], &spec, &result).await;
    let state = Market {
        now,
        reads: Arc::default(),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new()
        .route("/v1/instruments", get(catalog))
        .route("/v1/candles", get(candles))
        .with_state(state.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let mut last = 0;
    adaptive_live_tick(
        &db.pool,
        &url,
        "fixture",
        &AtomicBool::new(false),
        &mut last,
    )
    .await
    .unwrap();
    let rows=sqlx::query("select recommendation,external_name,extract(epoch from close_time)::bigint raw_close from live_setups order by close_time").fetch_all(&db.pool).await.unwrap();
    assert_eq!(
        rows.len(),
        10,
        "raw events after cutoff publish immediately, not three minutes later or after a future-price gate"
    );
    for row in &rows {
        let recommendation: Value = row.get("recommendation");
        let close: i64 = row.get("raw_close");
        assert_eq!(recommendation["rawSignalAt"], close);
        assert_eq!(recommendation["recommendedEntryAt"], close + 180);
        assert_eq!(recommendation["externalName"], original_name);
        assert!(serde_json::to_vec(&recommendation).unwrap().len() <= 16 * 1024);
        assert_eq!(recommendation["priceChangePct"].as_f64(), Some(1000.0));
        assert_eq!(recommendation["rawR"], 0.32);
        assert_eq!(
            recommendation["provenance"]["snapshotHash"],
            "frozen-worker-fixture"
        );
        assert_eq!(row.get::<String, _>("external_name"), original_name);
    }
    assert!(state.reads.lock().unwrap().iter().all(|id| *id == 1));
    let checkpoint = continuous::get_adaptive_checkpoint(&db.pool, ids[0], "bybit/linear/BTCUSDT")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(checkpoint.state["nextOpenUnix"], now);
    let before: Vec<Value> = rows.iter().map(|row| row.get("recommendation")).collect();
    let renamed_name = "\u{10400}".repeat(120);
    assert_eq!(renamed_name.chars().count(), 120);
    assert_eq!(renamed_name.len(), 480);
    settings_set(
        &db.pool,
        "algorithm:price_volume_spike",
        &json!({"enabled":false,"externalName":renamed_name}),
    )
    .await
    .unwrap();
    activate(&db.pool, ids[0], &spec, &result).await;
    last = 0;
    adaptive_live_tick(
        &db.pool,
        &url,
        "fixture",
        &AtomicBool::new(false),
        &mut last,
    )
    .await
    .unwrap();
    let after: Vec<Value> =
        sqlx::query_scalar("select recommendation from live_setups order by close_time")
            .fetch_all(&db.pool)
            .await
            .unwrap();
    assert_eq!(
        before, after,
        "generation replacement and renaming cannot duplicate or rewrite old signals"
    );
    activate(&db.pool, ids[1], &spec, &result).await;
    last = 0;
    adaptive_live_tick(
        &db.pool,
        &url,
        "fixture",
        &AtomicBool::new(false),
        &mut last,
    )
    .await
    .unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("select count(*) from live_setups")
            .fetch_one(&db.pool)
            .await
            .unwrap(),
        20,
        "other slots have independent identity/cooldown"
    );
    let renamed: Vec<Value> = sqlx::query_scalar(
        "select recommendation from live_setups where external_name=$1 order by close_time",
    )
    .bind(&renamed_name)
    .fetch_all(&db.pool)
    .await
    .unwrap();
    assert_eq!(renamed.len(), 10);
    for recommendation in renamed {
        assert_eq!(recommendation["externalName"], renamed_name);
        assert!(serde_json::to_vec(&recommendation).unwrap().len() <= 16 * 1024);
    }
    let active = continuous::active_adaptive_slots(&db.pool, "admin")
        .await
        .unwrap();
    settings_set(&db.pool, "live", &json!({"enabled":false}))
        .await
        .unwrap();
    assert!(
        !continuous::adaptive_notification_permitted(&db.pool, &active[0])
            .await
            .unwrap()
    );
    let reads = state.reads.lock().unwrap().len();
    last = 0;
    adaptive_live_tick(
        &db.pool,
        &url,
        "fixture",
        &AtomicBool::new(false),
        &mut last,
    )
    .await
    .unwrap();
    assert_eq!(reads, state.reads.lock().unwrap().len());
    continuous::stop(&db.pool, "admin", "price_volume_spike")
        .await
        .unwrap();
    assert!(
        continuous::active_adaptive_slots(&db.pool, "admin")
            .await
            .unwrap()
            .is_empty()
    );
    server.abort();
    let _ = server.await;
    db.close().await;
}

#[tokio::test]
async fn store_cas_fences_competing_checkpoint_and_stop_without_local_advance() {
    let Some(db) = Database::new("cas").await else {
        return;
    };
    configure(&db.pool).await;
    let now = chrono::Utc::now().timestamp().div_euclid(60) * 60;
    let (spec, result) = profile(now);
    let id: i64 = sqlx::query_scalar("select min(id) from continuous_slots")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    activate(&db.pool, id, &spec, &result).await;
    let slot = continuous::active_adaptive_slots(&db.pool, "admin")
        .await
        .unwrap()
        .remove(0);
    let store = PgAdaptiveStore {
        pool: &db.pool,
        slot: &slot,
    };
    let fence = AdaptiveFence {
        owner_id: slot.owner_id.clone(),
        algorithm_id: slot.algorithm_id.clone(),
        slot_id: id,
        generation_id: slot.generation_id,
        config_revision: slot.config_revision,
    };
    let instrument = AdaptiveInstrument {
        exchange: "bybit".into(),
        category: "linear".into(),
        symbol: "BTCUSDT".into(),
    };
    let checkpoint = AdaptiveCheckpoint {
        generation_id: slot.generation_id,
        next_open_unix: now,
        last_raw_open_unix: None,
        last_raw_cluster_time: None,
    };
    for invalid_name in [
        "a".repeat(121),
        "策".repeat(121),
        "\u{10400}".repeat(121),
        String::new(),
        " \t ".into(),
        "bad\nname".into(),
        "bad\u{7f}name".into(),
    ] {
        settings_set(
            &db.pool,
            "algorithm:price_volume_spike",
            &json!({"enabled":false,"externalName":invalid_name}),
        )
        .await
        .unwrap();
        assert!(
            store
                .commit_page(&fence, &instrument, None, &checkpoint, &mut [])
                .await
                .is_err(),
            "invalid names must fail before checkpoint persistence"
        );
        assert!(
            continuous::get_adaptive_checkpoint(&db.pool, id, "bybit/linear/BTCUSDT")
                .await
                .unwrap()
                .is_none()
        );
    }
    settings_set(
        &db.pool,
        "algorithm:price_volume_spike",
        &json!({"enabled":false,"externalName":slot.external_name}),
    )
    .await
    .unwrap();
    assert!(
        store
            .commit_page(&fence, &instrument, None, &checkpoint, &mut [])
            .await
            .unwrap()
            .is_some()
    );
    let mut newer = checkpoint.clone();
    newer.next_open_unix += 60;
    assert!(
        store
            .commit_page(&fence, &instrument, None, &newer, &mut [])
            .await
            .unwrap()
            .is_none(),
        "absent-checkpoint CAS must not overwrite a competing scan"
    );
    assert!(
        store
            .commit_page(&fence, &instrument, Some(&checkpoint), &newer, &mut [])
            .await
            .unwrap()
            .is_some()
    );
    continuous::stop(&db.pool, "admin", "price_volume_spike")
        .await
        .unwrap();
    let mut stale = newer.clone();
    stale.next_open_unix += 60;
    assert!(
        store
            .commit_page(&fence, &instrument, Some(&newer), &stale, &mut [])
            .await
            .unwrap()
            .is_none()
    );
    let durable = continuous::get_adaptive_checkpoint(&db.pool, id, "bybit/linear/BTCUSDT")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(durable.state, serde_json::to_value(newer).unwrap());
    assert!(!store.current(&fence).await.unwrap());
    db.close().await;
}
