//! Isolated PostgreSQL + local HTTP source; no production or external delivery.
use axum::{
    Json, Router,
    extract::{Query, State},
    routing::{get,post},
};
use serde::Deserialize;
use serde_json::{Value, json};
use setups_db::{continuous, settings_set};
use setups_domain::{AlgorithmParams, SpikeParams};
use setups_optimizer::{ContinuousReplay, StudyRequest, continuous_bootstrap};
use setups_worker::adaptive::{
    AdaptiveCheckpoint, AdaptiveFence, AdaptiveInstrument, AdaptiveStore, PgAdaptiveStore,
    adaptive_live_tick, AdaptiveLiveRuntime, AdaptiveRuntimeLimits,
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

fn profile(now: i64) -> (Value, Value) {profile_with_gap(now,None)}
fn profile_with_gap(now:i64,gap:Option<i64>)->(Value,Value) {
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
    let mut bootstrap = continuous_bootstrap(&request, &replay).unwrap();
    // Canonical digest of this fixture's immutable ORIGINAL detector input,
    // which is exactly the saved sequence served by `candles` below.
    let frozen_bars:Vec<_>=(bootstrap.detector_warmup_from_unix..bootstrap.next_open_unix)
        .step_by(60).filter(|open|Some(*open)!=gap).map(|open_time_unix|setups_domain::DetectorBar {open_time_unix,close:"100".into(),quote_turnover:"1".into()}).collect();
    let digest=setups_optimizer::detector_prefix_fingerprint(&frozen_bars,bootstrap.detector_warmup_from_unix,bootstrap.next_open_unix).unwrap();
    bootstrap.frozen_prefix=Some(setups_optimizer::ContinuousFrozenPrefix {
        version:1,from_open_unix:bootstrap.detector_warmup_from_unix,to_open_unix:bootstrap.next_open_unix,
        instruments:bootstrap.instruments.iter().map(|entry|setups_optimizer::ContinuousInstrumentPrefix {
            instrument:entry.instrument.clone(),bar_count:digest.bar_count,sha256:digest.sha256.clone(),
        }).collect(),
    });
    let recommendation = json!({"direction":"short","tpPct":14,"slPct":4,"rawR":0.32,"meanR":0.21,"robustScore":0.29,
        "timeShiftMinutes":3,"priceChangePct":1000,"clusterEnabled":false,"clusterGapMinutes":null,
        "provenance":{"algorithmId":"price_volume_spike","algorithmVersion":"1.1.1","optimizerVersion":setups_optimizer::CONTINUOUS_OPTIMIZER_VERSION,"seed":42,
        "snapshotHash":"frozen-worker-fixture","capturedAt":"2026-11-12T00:00:00Z","fromUnix":end-86400,"toUnix":end,"observationCutoff":end}});
    let result = json!({"meanExpectancy":0.21,"best":{"params":params,"analytics":{"clusterEnabled":false,"clusterGapMinutes":null,"timeShiftMinutes":3,"priceChangePct":1000}},
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
    settings_set(pool, "notifications", &json!({"maxAgeSeconds":60}))
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
    let generation:i64=sqlx::query_scalar("insert into continuous_generations(owner_id,algorithm_id,slot_id,config_revision,lease_token,status,phase,spec,execution_started_at,observation_cutoff,study_id,result,accepted) values('admin','price_volume_spike',$1,(select config_revision from continuous_processes where owner_id='admin' and algorithm_id='price_volume_spike'),1,'completed','completed',$2,now(),$3,$4,$5,true) returning id")
        .bind(slot_id).bind(spec).bind(spec["toUnix"].as_i64().unwrap()).bind(study).bind(result).fetch_one(pool).await.unwrap();
    sqlx::query("update continuous_slots set active_generation_id=$2,study_id=$3,mean_expectancy=0.21,selected_cell=$4,status='active' where id=$1")
        .bind(slot_id).bind(generation).bind(study).bind(&result["selectedCell"]).execute(pool).await.unwrap();
}

fn scoped_profile(now:i64,symbols:&[&str])->(Value,Value) {
    let (mut spec,mut result)=profile(now);
    let scope:Vec<_>=symbols.iter().map(|symbol|json!({"exchange":"bybit","category":"linear","symbol":symbol})).collect();
    spec["instruments"]=json!(scope);
    let seed=result["bootstrap"]["instruments"][0].clone();
    let prefix=result["bootstrap"]["frozenPrefix"]["instruments"][0].clone();
    result["bootstrap"]["instruments"]=json!(scope.iter().map(|instrument| {let mut seed=seed.clone();seed["instrument"]=instrument.clone();seed}).collect::<Vec<_>>());
    result["bootstrap"]["frozenPrefix"]["instruments"]=json!(scope.iter().map(|instrument| {let mut prefix=prefix.clone();prefix["instrument"]=instrument.clone();prefix}).collect::<Vec<_>>());
    (spec,result)
}
async fn stage(pool:&PgPool,slot:i64,spec:&Value,result:&Value)->i64 {
    let study:i64=sqlx::query_scalar("insert into optimization_studies(algorithm_id,status,spec,progress,origin) values('price_volume_spike','completed',$1,'{}','continuous') returning id")
        .bind(spec).fetch_one(pool).await.unwrap();
    let generation:i64=sqlx::query_scalar("insert into continuous_generations(owner_id,algorithm_id,slot_id,config_revision,lease_token,status,phase,spec,execution_started_at,observation_cutoff,study_id,result,accepted) values('admin','price_volume_spike',$1,(select config_revision from continuous_processes where owner_id='admin' and algorithm_id='price_volume_spike'),1,'completed','completed',$2,now(),$3,$4,$5,true) returning id")
        .bind(slot).bind(spec).bind(spec["toUnix"].as_i64().unwrap()).bind(study).bind(result).fetch_one(pool).await.unwrap();
    sqlx::query("update continuous_slots set pending_generation_id=$2,pending_status='warming',pending_error=null where id=$1")
        .bind(slot).bind(generation).execute(pool).await.unwrap();generation
}

#[derive(Clone)]
struct Market {
    now: i64,
    reads: Arc<Mutex<Vec<i64>>>,
    journal:Arc<Mutex<Journal>>,
}
#[derive(Default)]
struct Journal {
    revision:i64,
    floor:i64,
    available_through:Option<i64>,
    gaps:std::collections::BTreeSet<i64>,
    quiet:std::collections::BTreeSet<i64>,
    events:Vec<(i64,i64)>,
    omit_guard:bool,
    missing_evidence:bool,
    repair_during_history:Option<i64>,
    arrival_before_guard:Option<(usize,i64)>,
    guard_requests:Vec<Value>,
}
impl Journal {
    fn repair(&mut self,open:i64) {
        self.gaps.remove(&open);self.revision+=1;self.events.push((self.revision,open));
    }
}
impl Market {
    fn new(now:i64)->Self {Self {now,reads:Arc::default(),journal:Arc::default()}}
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
    let mut journal=state.journal.lock().unwrap();
    let mut candles:Vec<_>=(start..query.to_unix.min(journal.available_through.unwrap_or(state.now))).step_by(60).filter(|time|query.after_unix.is_none_or(|after|*time>after) && !journal.gaps.contains(time))
        .take(query.limit.unwrap_or(256)+1).map(|time|json!({"openTimeUnix":time,"open":"100","high":"100","low":"100","close":"100","volume":"1","quoteTurnover":if journal.quiet.contains(&time){"0"}else{"1"}})).collect();
    let more = candles.len() > query.limit.unwrap_or(256);
    if more {
        candles.pop();
    }
    let next = more.then(|| candles.last().unwrap()["openTimeUnix"].as_i64().unwrap());
    // The legacy page is an earlier snapshot; mutate canonical history before
    // its post-read guard response to reproduce a warmup epoch race exactly.
    if let Some(open)=journal.repair_during_history.take() {journal.repair(open);}
    Json(json!({"candles":candles,"nextCursorUnix":next}))
}
async fn tail(State(state):State<Market>,Json(request):Json<Value>)->Json<Value> {
    let from=request["fromUnix"].as_i64().unwrap();let to=request["toUnix"].as_i64().unwrap();
    let guards:Vec<setups_mdclient::TailReadGuardQuery>=serde_json::from_value(request.get("readGuards").cloned().unwrap_or(json!([]))).unwrap();
    let mut journal=state.journal.lock().unwrap();
    if let Some((ordinal,open))=journal.arrival_before_guard
        && journal.guard_requests.len()+1==ordinal {
        journal.arrival_before_guard=None;journal.available_through=Some(open+60);journal.repair(open);
    }
    if journal.guard_requests.len()==128 {journal.guard_requests.remove(0);}
    journal.guard_requests.push(request.clone());
    let instruments:Vec<_>=request["instrumentIds"].as_array().unwrap().iter().map(|id| {
        let id=id.as_i64().unwrap();let mut missing=Vec::new();let mut cursor=from;let mut contiguous=None;
        let candles:Vec<_>=(from..to.min(journal.available_through.unwrap_or(state.now))).step_by(60).filter(|open|!journal.gaps.contains(open)).map(|open| {
            if cursor<open {missing.push(json!({"fromUnix":cursor,"toUnix":open}));}
            if missing.is_empty() {contiguous=Some(open);}
            cursor=open+60;
            json!({"instrumentId":id,"openTimeUnix":open,"open":"100","high":"100","low":"100","close":"100","volume":"1","quoteTurnover":if journal.quiet.contains(&open){"0"}else{"1"}})
        }).collect();
        if cursor<to {missing.push(json!({"fromUnix":cursor,"toUnix":to}));}
        let mut result=json!({"instrumentId":id,"status":"known","latestStoredOpenTimeUnix":journal.available_through.unwrap_or(state.now)-60,
            "contiguousThroughOpenTimeUnix":contiguous,"candles":candles,"missingRanges":missing});
        if !journal.omit_guard && let Some(guard)=guards.iter().find(|guard|guard.instrument_id==id) {
            let token=json!({"epoch":"worker-market-fixture","revision":journal.revision.to_string()});
            let status=if journal.missing_evidence {"unknown"} else if let Some(previous)=&guard.previous {
                let previous_revision=previous.revision.parse::<i64>().unwrap();
                if previous.epoch!="worker-market-fixture" || previous_revision<journal.floor || previous_revision>journal.revision {"unknown"}
                else if journal.events.iter().any(|(revision,open)|*revision>previous_revision && *open>=guard.from_unix && *open<guard.to_unix) {"invalid"}
                else {"valid"}
            } else {"valid"};
            result["readGuard"]=json!({"status":status,"token":if journal.missing_evidence {Value::Null}else{token}});
        }
        result
    }).collect();
    Json(json!({"checkedAt":chrono::Utc::now().to_rfc3339(),"instruments":instruments}))
}
async fn spawn_market(state:Market)->(String,tokio::task::JoinHandle<()>) {
    let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url=format!("http://{}",listener.local_addr().unwrap());
    let app=Router::new().route("/v1/instruments",get(catalog)).route("/v1/candles",get(candles))
        .route("/v1/candles/tail-batch",post(tail)).with_state(state);
    let server=tokio::spawn(async move {axum::serve(listener,app).await.unwrap();});
    (url,server)
}
async fn catalog() -> Json<Value> {
    Json(json!({"instruments":[
        {"id":1,"exchange":"bybit","category":"linear","symbol":"BTCUSDT","status":"active"},
        {"id":2,"exchange":"bybit","category":"linear","symbol":"DELISTUSDT","status":"inactive"},
        {"id":3,"exchange":"bybit","category":"linear","symbol":"NOTINSTUDYUSDT","status":"active"},
        {"id":4,"exchange":"bybit","category":"linear","symbol":"EXTRA4USDT","status":"active"},
        {"id":5,"exchange":"bybit","category":"linear","symbol":"EXTRA5USDT","status":"active"},
        {"id":6,"exchange":"bybit","category":"linear","symbol":"EXTRA6USDT","status":"active"},
        {"id":7,"exchange":"bybit","category":"linear","symbol":"EXTRA7USDT","status":"active"},
        {"id":8,"exchange":"bybit","category":"linear","symbol":"EXTRA8USDT","status":"active"},
        {"id":9,"exchange":"bybit","category":"linear","symbol":"EXTRA9USDT","status":"active"}
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
    let state = Market::new(now);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new()
        .route("/v1/instruments", get(catalog))
        .route("/v1/candles", get(candles))
        .route("/v1/candles/tail-batch", post(tail))
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
    assert!(continuous::get_adaptive_checkpoint(&db.pool, id, "bybit/linear/BTCUSDT").await.unwrap().is_none(),
        "Stop retires inactive detector cursors and a stale writer must not recreate them");
    assert!(!store.current(&fence).await.unwrap());
    db.close().await;
}

struct LiveFixture {
    db:Database,
    market:Market,
    runtime:AdaptiveLiveRuntime,
    client:setups_mdclient::MarketDataClient,
    server:tokio::task::JoinHandle<()>,
    slot:i64,
    generation:i64,
    anchor:i64,
    now:i64,
}
impl LiveFixture {
    async fn new(label:&str,original_gap:bool)->Option<Self> {
        let db=Database::new(label).await?;configure(&db.pool).await;
        let now=chrono::Utc::now().timestamp().div_euclid(60)*60;
        let (_,initial)=profile(now);
        let anchor=initial["bootstrap"]["detectorWarmupFromUnix"].as_i64().unwrap();
        let gap=original_gap.then_some(anchor+60);
        let (spec,result)=profile_with_gap(now,gap);
        let slot=sqlx::query_scalar("select min(id) from continuous_slots").fetch_one(&db.pool).await.unwrap();
        activate(&db.pool,slot,&spec,&result).await;
        let generation=continuous::active_adaptive_slots(&db.pool,"admin").await.unwrap()[0].generation_id;
        let market=Market::new(now);
        if let Some(gap)=gap {market.journal.lock().unwrap().gaps.insert(gap);}
        let (url,server)=spawn_market(market.clone()).await;
        Some(Self {db,market,runtime:AdaptiveLiveRuntime::new(AdaptiveRuntimeLimits {max_history_bytes:32*1024*1024,
            max_symbols:10,max_profile_bytes:4*1024*1024,..AdaptiveRuntimeLimits::default()}),
            client:setups_mdclient::MarketDataClient::new(url,"fixture"),server,slot,generation,anchor,now})
    }
    async fn tick(&mut self)->Result<(),setups_worker::WorkerError> {
        self.runtime.tick(&self.db.pool,&self.client,&AtomicBool::new(false)).await
    }
    async fn durable(&self)->(Vec<Value>,Option<continuous::AdaptiveCheckpoint>) {
        (sqlx::query_scalar("select recommendation from live_setups order by id").fetch_all(&self.db.pool).await.unwrap(),
            continuous::get_adaptive_checkpoint(&self.db.pool,self.slot,"bybit/linear/BTCUSDT").await.unwrap())
    }
    async fn close(self) {self.server.abort();let _=self.server.await;self.db.close().await;}
}

#[tokio::test]
async fn causal_repair_older_than_retained_baseline_invalidates_before_effects_and_preserves_frozen_mismatch() {
    let Some(mut f)=LiveFixture::new("oldrepair",true).await else {return;};
    f.tick().await.unwrap();assert!(f.runtime.retained_history_bytes()>0);
    assert!(f.anchor+60<f.now-86400,"repair lies OUTSIDE the one-day retained baseline but inside the causal prefix");
    let before=f.durable().await;let reads=f.market.reads.lock().unwrap().len();
    f.market.journal.lock().unwrap().repair(f.anchor+60);
    let error=f.tick().await.unwrap_err();assert!(error.to_string().contains("causal history changed"));
    assert_eq!(f.runtime.retained_history_bytes(),0);assert_eq!(f.runtime.retained_profile_bytes(),0);
    assert_eq!(f.durable().await,before);assert_eq!(f.market.reads.lock().unwrap().len(),reads);
    assert_eq!(f.market.journal.lock().unwrap().guard_requests.last().unwrap()["readGuards"][0]["fromUnix"],f.anchor);
    // Rebuilding cannot substitute journal evidence for the original manifest:
    // the now-filled ORIGINAL gap changes the frozen prefix and stays an error.
    let error=f.tick().await.unwrap_err();assert!(error.to_string().contains("frozen detector prefix fingerprint mismatch"));
    assert_eq!(f.durable().await,before);
    assert_eq!(continuous::active_adaptive_slots(&f.db.pool,"admin").await.unwrap()[0].generation_id,f.generation);
    f.close().await;
}

#[tokio::test]
async fn original_prefix_repair_requires_explicit_stop_start_and_new_verified_candidate() {
    let Some(mut f)=LiveFixture::new("explicitrecovery",true).await else {return;};
    f.runtime.tick_at(&f.db.pool,&f.client,&AtomicBool::new(false),f.now).await.unwrap();
    let history:Vec<Value>=sqlx::query_scalar("select to_jsonb(s) from live_setups s order by id").fetch_all(&f.db.pool).await.unwrap();
    let original:Value=sqlx::query_scalar("select result->'bootstrap'->'frozenPrefix' from continuous_generations where id=$1")
        .bind(f.generation).fetch_one(&f.db.pool).await.unwrap();
    f.market.journal.lock().unwrap().repair(f.anchor+60);
    assert!(f.runtime.tick_at(&f.db.pool,&f.client,&AtomicBool::new(false),f.now+5).await.unwrap_err().to_string().contains("causal history changed"));
    let (spec,result)=scoped_profile(f.now,&["BTCUSDT","NOTINSTUDYUSDT"]);
    let blocked=stage(&f.db.pool,f.slot,&spec,&result).await;
    let error=f.runtime.tick_at(&f.db.pool,&f.client,&AtomicBool::new(false),f.now+6).await.unwrap_err();
    assert!(error.to_string().contains("frozen detector prefix fingerprint mismatch"),"a valid candidate cannot shadow an invalid incumbent manifest: {error}");
    assert_eq!(sqlx::query_scalar::<_,i64>("select active_generation_id from continuous_slots where id=$1").bind(f.slot).fetch_one(&f.db.pool).await.unwrap(),f.generation);
    assert_eq!(sqlx::query_scalar::<_,i64>("select count(*) from live_setups where adaptive_generation_id=$1").bind(blocked).fetch_one(&f.db.pool).await.unwrap(),0);
    let lease=continuous::scheduler::claim_owner(&f.db.pool,"worker-explicit-recovery").await.unwrap().unwrap();
    assert!(continuous::stop(&f.db.pool,"admin","price_volume_spike").await.unwrap());
    assert!(continuous::get_adaptive_checkpoint(&f.db.pool,f.slot,"bybit/linear/BTCUSDT").await.unwrap().is_none());
    assert!(!continuous::start(&f.db.pool,"admin","price_volume_spike").await.unwrap());
    continuous::cleanup_stopped(&f.db.pool,"admin","price_volume_spike",&lease).await.unwrap();
    assert!(continuous::start(&f.db.pool,"admin","price_volume_spike").await.unwrap());
    f.runtime.tick_at(&f.db.pool,&f.client,&AtomicBool::new(false),f.now+7).await.unwrap();
    let candidate=stage(&f.db.pool,f.slot,&spec,&result).await;
    f.runtime.tick_at(&f.db.pool,&f.client,&AtomicBool::new(false),f.now+8).await.unwrap();
    assert_eq!(sqlx::query_scalar::<_,i64>("select active_generation_id from continuous_slots where id=$1").bind(f.slot).fetch_one(&f.db.pool).await.unwrap(),candidate);
    assert_eq!(sqlx::query_scalar::<_,Value>("select result->'bootstrap'->'frozenPrefix' from continuous_generations where id=$1")
        .bind(f.generation).fetch_one(&f.db.pool).await.unwrap(),original,"never rewrite or synthesize the original proof");
    assert_eq!(sqlx::query_scalar::<_,Value>("select result->'bootstrap'->'frozenPrefix' from continuous_generations where id=$1")
        .bind(candidate).fetch_one(&f.db.pool).await.unwrap(),result["bootstrap"]["frozenPrefix"]);
    assert_eq!(sqlx::query_scalar::<_,Value>("select to_jsonb(g)->'accepted' from continuous_generations g where id=$1").bind(f.generation).fetch_one(&f.db.pool).await.unwrap(),true);
    assert_eq!(sqlx::query_scalar::<_,Value>("select to_jsonb(s) from optimization_studies s where id=(select study_id from continuous_generations where id=$1)")
        .bind(f.generation).fetch_one(&f.db.pool).await.unwrap()["status"],"completed");
    assert_eq!(sqlx::query_scalar::<_,Value>("select to_jsonb(s) from live_setups s order by id limit 1").fetch_one(&f.db.pool).await.unwrap(),history[0]);
    assert_eq!(sqlx::query_scalar::<_,Value>("select coalesce(jsonb_agg(to_jsonb(s) order by id),'[]'::jsonb) from live_setups s").fetch_one(&f.db.pool).await.unwrap(),json!(history),
        "verified warmup/activation must not republish any history, including the newly scoped instrument");
    {let mut journal=f.market.journal.lock().unwrap();journal.available_through=Some(f.now+60);journal.repair(f.now);}
    f.runtime.tick_at(&f.db.pool,&f.client,&AtomicBool::new(false),f.now+65).await.unwrap();
    let generations:Vec<i64>=sqlx::query_scalar("select adaptive_generation_id from live_setups where adaptive_slot_id=$1 and extract(epoch from close_time)::bigint=$2")
        .bind(f.slot).bind(f.now+60).fetch_all(&f.db.pool).await.unwrap();assert_eq!(generations,vec![candidate,candidate]);
    f.close().await;
}

#[tokio::test]
async fn irrelevant_backfill_before_original_anchor_is_acknowledged_without_rewarming() {
    let Some(mut f)=LiveFixture::new("irrelevant",false).await else {return;};
    f.market.journal.lock().unwrap().gaps.insert(f.anchor-60);
    f.tick().await.unwrap();let bytes=f.runtime.retained_history_bytes();let before=f.durable().await;
    let reads=f.market.reads.lock().unwrap().len();
    f.market.journal.lock().unwrap().repair(f.anchor-60);
    f.tick().await.unwrap();assert_eq!(f.runtime.retained_history_bytes(),bytes);
    assert_eq!(f.market.reads.lock().unwrap().len(),reads);assert_eq!(f.durable().await,before);
    f.tick().await.unwrap();
    assert_eq!(f.market.journal.lock().unwrap().guard_requests.last().unwrap()["readGuards"][0]["previous"]["revision"],"1");
    assert_eq!(f.market.reads.lock().unwrap().len(),reads);
    f.close().await;
}

#[tokio::test]
async fn warmup_repair_after_legacy_page_snapshot_rejects_before_materialization_or_publication() {
    let Some(mut f)=LiveFixture::new("warmrepair",true).await else {return;};
    f.market.journal.lock().unwrap().repair_during_history=Some(f.anchor+60);
    assert!(f.tick().await.unwrap_err().to_string().contains("causal history changed"));
    assert_eq!(f.runtime.retained_history_bytes(),0);assert_eq!(f.durable().await,(Vec::new(),None));
    {
        let journal=f.market.journal.lock().unwrap();
        assert!(journal.guard_requests[0]["readGuards"][0]["previous"].is_null());
        assert_eq!(journal.guard_requests[1]["readGuards"][0]["previous"]["revision"],"0");
    }
    f.close().await;
}

#[tokio::test]
async fn floor_unknown_null_evidence_and_omitted_guard_all_fail_closed_without_cursor_advance() {
    for (label,mode) in [("guardfloor",0),("guardevidence",1),("guardomit",2)] {
        let Some(mut f)=LiveFixture::new(label,false).await else {return;};
        f.tick().await.unwrap();let before=f.durable().await;
        {
            let mut journal=f.market.journal.lock().unwrap();
            match mode {0=>{journal.revision=2048;journal.floor=1024;},1=>journal.missing_evidence=true,_=>journal.omit_guard=true}
        }
        assert!(f.tick().await.is_err());assert_eq!(f.runtime.retained_history_bytes(),0);
        assert_eq!(f.runtime.retained_profile_bytes(),0);assert_eq!(f.durable().await,before);
        f.close().await;
    }
}

#[tokio::test]
async fn legacy_market_omission_does_not_turn_capture_into_proof_or_fetch_unverified_history() {
    let Some(mut f)=LiveFixture::new("legacyguard",false).await else {return;};
    f.market.journal.lock().unwrap().omit_guard=true;
    assert!(f.tick().await.is_err());assert!(f.market.reads.lock().unwrap().is_empty());
    assert_eq!(f.runtime.retained_history_bytes(),0);assert_eq!(f.durable().await,(Vec::new(),None));
    f.close().await;
}

async fn assert_delayed_final(restart:bool) {
    let Some(mut f)=LiveFixture::new(if restart {"delayedrestart"}else{"delayedfinal"},false).await else {return;};
    let t=f.now;
    f.market.journal.lock().unwrap().available_through=Some(t-60);
    f.runtime.tick_at(&f.db.pool,&f.client,&AtomicBool::new(false),t-60).await.unwrap();
    // At close T the store still ends at OPEN T-120. The pending final OPEN
    // T-60 is absent, not a processed historical gap. Exercise the real tick.
    f.runtime.tick_at(&f.db.pool,&f.client,&AtomicBool::new(false),t).await.unwrap();
    let before=f.durable().await;let reads=f.market.reads.lock().unwrap().len();
    let before_next=before.1.as_ref().unwrap().state["nextOpenUnix"].as_i64().unwrap();
    if restart {f.runtime=AdaptiveLiveRuntime::new(AdaptiveRuntimeLimits {max_history_bytes:32*1024*1024,max_symbols:10,
        max_profile_bytes:4*1024*1024,..AdaptiveRuntimeLimits::default()});}
    {
        let mut journal=f.market.journal.lock().unwrap();journal.available_through=Some(t);journal.repair(t-60);
    }
    let arrived=f.runtime.tick_at(&f.db.pool,&f.client,&AtomicBool::new(false),t+5).await;
    assert!(arrived.is_ok(),"ordinary delayed final was suppressed: restart={restart} checkpoint_after_empty_poll={before_next} error={arrived:?}");
    assert_eq!(before_next,t-60,"observing absence must not advance the durable processed frontier");
    let after=f.durable().await;assert_eq!(after.0.len(),before.0.len()+1);
    assert_eq!(after.1.as_ref().unwrap().state["nextOpenUnix"],t);
    assert_eq!(f.market.reads.lock().unwrap().len(),reads+usize::from(restart),"ordinary arrival must not cause an extra full rewarm");
    let (_,result)=profile(t);let bootstrap=&result["bootstrap"];
    let mut reference=setups_detector::PriceVolumeSpikeDetector::with_params(params(),None).unwrap();
    let mut expected=Vec::new();
    for open in (f.anchor..t).step_by(60) {
        let bar=setups_domain::DetectorBar {open_time_unix:open,close:"100".into(),quote_turnover:"1".into()};
        if let Some(raw)=reference.push(&bar).unwrap().setup
            && raw.open_time_unix>=bootstrap["nextOpenUnix"].as_i64().unwrap()
            && raw.close_time_unix>=bootstrap["emitFromCloseUnix"].as_i64().unwrap() {expected.push(serde_json::to_value(raw).unwrap());}
    }
    let actual:Vec<Value>=sqlx::query_scalar("select jsonb_build_object('algorithmId',algorithm_id,'algorithmVersion',algorithm_version,'openTimeUnix',extract(epoch from open_time)::bigint,'closeTimeUnix',extract(epoch from close_time)::bigint,'closePrice',close_price,'params',params,'details',details) from live_setups order by close_time")
        .fetch_all(&f.db.pool).await.unwrap();
    assert_eq!(actual,expected,"guarded incremental/restarted publication must match a fresh reference replay exactly");
    for second in [6,10,20] {f.runtime.tick_at(&f.db.pool,&f.client,&AtomicBool::new(false),t+second).await.unwrap();}
    assert_eq!(f.durable().await,after,"the delayed eligible final must publish exactly once");
    f.close().await;
}
#[tokio::test]
async fn ordinary_delayed_final_is_published_once_without_rewarming() {assert_delayed_final(false).await;}
#[tokio::test]
async fn ordinary_delayed_final_is_published_once_after_restart_between_poll_and_arrival() {assert_delayed_final(true).await;}

#[tokio::test]
async fn resumed_accepted_profile_joins_existing_history_via_one_verified_cohort_reconstruction() {
    let Some(mut f)=LiveFixture::new("profilejoin",false).await else {return;};
    f.runtime.tick_at(&f.db.pool,&f.client,&AtomicBool::new(false),f.now).await.unwrap();
    let before=f.durable().await;let reads=f.market.reads.lock().unwrap().len();
    let sibling:i64=sqlx::query_scalar("select max(id) from continuous_slots").fetch_one(&f.db.pool).await.unwrap();
    let (spec,result)=scoped_profile(f.now,&["BTCUSDT"]);activate(&f.db.pool,sibling,&spec,&result).await;
    let generation:i64=sqlx::query_scalar("select active_generation_id from continuous_slots where id=$1").bind(sibling).fetch_one(&f.db.pool).await.unwrap();
    let mut state=before.1.unwrap().state;state["generationId"]=generation.into();
    // An accepted profile resumed after a prior promotion has a durable W.
    sqlx::query("insert into adaptive_live_checkpoints(slot_id,instrument_key,generation_id,state) values($1,'bybit/linear/BTCUSDT',$2,$3)")
        .bind(sibling).bind(generation).bind(state).execute(&f.db.pool).await.unwrap();
    f.runtime.tick_at(&f.db.pool,&f.client,&AtomicBool::new(false),f.now+5).await.unwrap();
    assert_eq!(f.durable().await.0,before.0);assert_eq!(f.runtime.retained_history_epochs(),1);
    assert_eq!(f.market.reads.lock().unwrap().len(),reads+1,"rejoin is one cold read for the complete cohort, not one per profile");
    {let mut journal=f.market.journal.lock().unwrap();journal.available_through=Some(f.now+60);journal.repair(f.now);}
    f.runtime.tick_at(&f.db.pool,&f.client,&AtomicBool::new(false),f.now+65).await.unwrap();
    assert_eq!(sqlx::query_scalar::<_,i64>("select count(*) from live_setups where symbol='BTCUSDT' and extract(epoch from close_time)::bigint=$1")
        .bind(f.now+60).fetch_one(&f.db.pool).await.unwrap(),2);
    f.close().await;
}

#[tokio::test]
async fn cluster_enabled_repair_restart_preserves_chain_and_emits_the_real_next_first_member() {
    let Some(mut f)=LiveFixture::new("clusterrepair",false).await else {return;};
    let (mut spec,mut result)=scoped_profile(f.now,&["BTCUSDT"]);
    spec["analyticsParams"]["clusterEnabled"]=true.into();spec["analyticsParams"]["clusterGapMinutes"]=3.into();
    result["best"]["analytics"]["clusterEnabled"]=true.into();result["best"]["analytics"]["clusterGapMinutes"]=3.into();
    result["recommendation"]["clusterEnabled"]=true.into();result["recommendation"]["clusterGapMinutes"]=3.into();
    sqlx::query("update continuous_generations set spec=$2,result=$3 where id=$1").bind(f.generation).bind(&spec).bind(&result).execute(&f.db.pool).await.unwrap();
    let repaired=f.now-4*60;assert!(repaired>result["bootstrap"]["nextOpenUnix"].as_i64().unwrap());
    f.market.journal.lock().unwrap().gaps.insert(repaired);
    f.runtime.tick_at(&f.db.pool,&f.client,&AtomicBool::new(false),f.now).await.unwrap();
    assert!(f.durable().await.0.is_empty(),"the prehistory raw chain must suppress non-first members");
    f.market.journal.lock().unwrap().repair(repaired);
    assert!(f.runtime.tick_at(&f.db.pool,&f.client,&AtomicBool::new(false),f.now+5).await.unwrap_err().to_string().contains("causal history changed"));
    f.runtime=AdaptiveLiveRuntime::new(AdaptiveRuntimeLimits {max_history_bytes:32*1024*1024,max_symbols:10,max_profile_bytes:4*1024*1024,..AdaptiveRuntimeLimits::default()});
    {let mut journal=f.market.journal.lock().unwrap();journal.available_through=Some(f.now+60);journal.repair(f.now);}
    f.runtime.tick_at(&f.db.pool,&f.client,&AtomicBool::new(false),f.now+65).await.unwrap();
    assert!(f.durable().await.0.is_empty(),"the first fresh raw after restart is STILL not a cluster first");
    assert_eq!(f.durable().await.1.unwrap().state["lastRawClusterTime"],f.now+60);
    let quiet:std::collections::BTreeSet<_>=(f.now+60..f.now+240).step_by(60).collect();
    {
        let mut journal=f.market.journal.lock().unwrap();journal.quiet.extend(&quiet);journal.available_through=Some(f.now+240);
        for open in &quiet {journal.repair(*open);}
    }
    f.runtime.tick_at(&f.db.pool,&f.client,&AtomicBool::new(false),f.now+245).await.unwrap();
    assert_eq!(f.durable().await.1.unwrap().state["lastRawClusterTime"],f.now+60);
    for target in [f.now+300,f.now+360] {
        {let mut journal=f.market.journal.lock().unwrap();journal.available_through=Some(target);journal.repair(target-60);}
        f.runtime.tick_at(&f.db.pool,&f.client,&AtomicBool::new(false),target+5).await.unwrap();
    }
    let mut reference=setups_detector::PriceVolumeSpikeDetector::with_params(params(),None).unwrap();
    let mut cluster=setups_analytics::CausalCluster::new(180,None).unwrap();let mut expected=Vec::new();
    for open in (f.anchor..f.now+360).step_by(60) {
        let bar=setups_domain::DetectorBar {open_time_unix:open,close:"100".into(),quote_turnover:if quiet.contains(&open){"0"}else{"1"}.into()};
        if let Some(raw)=reference.push(&bar).unwrap().setup
            && cluster.observe_raw(raw.close_time_unix).unwrap()
            && raw.open_time_unix>=result["bootstrap"]["nextOpenUnix"].as_i64().unwrap()
            && raw.close_time_unix>=result["bootstrap"]["emitFromCloseUnix"].as_i64().unwrap() {expected.push(raw.close_time_unix);}
    }
    assert_eq!(expected,vec![f.now+300],"cross a real >3-minute raw-cluster boundary with nonzero first-member output");
    let actual:Vec<i64>=sqlx::query_scalar("select extract(epoch from close_time)::bigint from live_setups where adaptive_slot_id=$1 order by close_time")
        .bind(f.slot).fetch_all(&f.db.pool).await.unwrap();assert_eq!(actual,expected);
    let after=f.durable().await;assert_eq!(after.1.unwrap().state["lastRawClusterTime"],f.now+360,"suppressed raw members still advance the chain");
    assert_eq!(after.0.len(),1);assert_eq!(after.0[0]["clusterEnabled"],true);assert_eq!(after.0[0]["clusterGapMinutes"],3);
    f.close().await;
}

#[tokio::test]
async fn repaired_post_frozen_gap_recomputes_durable_cooldown_phase_and_future_matches_reference() {
    let Some(mut f)=LiveFixture::new("phaserepair",false).await else {return;};
    let (mut spec,mut result)=scoped_profile(f.now,&["BTCUSDT"]);
    let mut parameters=params();if let AlgorithmParams::Spike(params)=&mut parameters {params.cooldown_minutes=7;}
    spec["algorithmParams"]=serde_json::to_value(&parameters).unwrap();result["best"]["params"]=spec["algorithmParams"].clone();
    let boundary=result["bootstrap"]["nextOpenUnix"].as_i64().unwrap();
    let mut reference=setups_detector::PriceVolumeSpikeDetector::with_params(parameters.clone(),None).unwrap();
    let mut seed=None;let mut gap=None;
    for open in (f.anchor..f.now).step_by(60) {
        let bar=setups_domain::DetectorBar {open_time_unix:open,close:"100".into(),quote_turnover:"1".into()};
        if let Some(raw)=reference.push(&bar).unwrap().setup {
            if open<boundary {seed=Some(raw.open_time_unix);} else if open<f.now-60 && gap.is_none() {gap=Some(open);}
        }
    }
    result["bootstrap"]["instruments"][0]["lastRawOpenUnix"]=json!(seed);
    result["bootstrap"]["instruments"][0]["lastRawClusterTime"]=json!(seed.map(|open|open+60));
    sqlx::query("update continuous_generations set spec=$2,result=$3 where id=$1").bind(f.generation).bind(&spec).bind(&result).execute(&f.db.pool).await.unwrap();
    let gap=gap.unwrap();f.market.journal.lock().unwrap().gaps.insert(gap);
    f.runtime.tick_at(&f.db.pool,&f.client,&AtomicBool::new(false),f.now).await.unwrap();let before=f.durable().await;
    f.market.journal.lock().unwrap().repair(gap);
    assert!(f.runtime.tick_at(&f.db.pool,&f.client,&AtomicBool::new(false),f.now+5).await.is_err());
    f.runtime.tick_at(&f.db.pool,&f.client,&AtomicBool::new(false),f.now+6).await.unwrap();
    assert_eq!(f.durable().await.0,before.0,"source repair must not rewrite or republish historical setups");
    assert_ne!(f.durable().await.1.unwrap().state,before.1.unwrap().state,"recomputed cooldown phase must replace the stale derived seed");
    let target=f.now+15*60;f.market.journal.lock().unwrap().available_through=Some(target);
    let mut expected=Vec::new();
    for open in (f.now..target).step_by(60) {
        let bar=setups_domain::DetectorBar {open_time_unix:open,close:"100".into(),quote_turnover:"1".into()};
        if let Some(raw)=reference.push(&bar).unwrap().setup {expected.push(raw.close_time_unix);}
    }
    f.runtime.tick_at(&f.db.pool,&f.client,&AtomicBool::new(false),target+5).await.unwrap();
    let actual:Vec<i64>=sqlx::query_scalar("select extract(epoch from close_time)::bigint from live_setups where adaptive_slot_id=$1 and extract(epoch from open_time)::bigint >= $2 order by close_time")
        .bind(f.slot).bind(f.now).fetch_all(&f.db.pool).await.unwrap();assert_eq!(actual,expected);
    f.close().await;
}

#[tokio::test]
async fn final_arriving_between_active_hot_and_staging_snapshot_preserves_both_epoch_proofs() {
    let Some(mut f)=LiveFixture::new("interleavedfinal",false).await else {return;};
    let (spec,result)=scoped_profile(f.now,&["BTCUSDT"]);
    sqlx::query("update continuous_generations set spec=$2,result=$3 where id=$1").bind(f.generation).bind(&spec).bind(&result).execute(&f.db.pool).await.unwrap();
    {let mut journal=f.market.journal.lock().unwrap();journal.available_through=Some(f.now-60);journal.revision=1;}
    f.runtime.tick_at(&f.db.pool,&f.client,&AtomicBool::new(false),f.now-60).await.unwrap();
    let candidate=stage(&f.db.pool,f.slot,&spec,&result).await;
    {
        let mut journal=f.market.journal.lock().unwrap();journal.guard_requests.clear();
        // Request 1 is the active hot observation, processed before cold work.
        // Request 2 is staging's first source capture. Insert immediately
        // before that snapshot, not before the tick or active observation.
        journal.arrival_before_guard=Some((2,f.now-60));
    }
    f.runtime.tick_at(&f.db.pool,&f.client,&AtomicBool::new(false),f.now+5).await.unwrap();
    {
        let journal=f.market.journal.lock().unwrap();assert!(journal.arrival_before_guard.is_none());
        assert_eq!(journal.guard_requests[0]["readGuards"][0]["previous"]["revision"],"1");
        assert_eq!(journal.revision,2);
    }
    assert_eq!(f.durable().await.1.unwrap().state["nextOpenUnix"],f.now-60);
    let baseline_reads=f.market.reads.lock().unwrap().len();
    // Canonical still owns the shorter interval at revision 1; staging has
    // consumed the new final at revision 2. Both individual proofs are valid.
    let resumed=f.runtime.tick_at(&f.db.pool,&f.client,&AtomicBool::new(false),f.now+6).await;
    assert!(resumed.is_ok(),"independent valid snapshots must not invalidate their union: {resumed:?}");
    assert_eq!(f.market.reads.lock().unwrap().len(),baseline_reads,"no incumbent or candidate baseline reread");
    assert_eq!(sqlx::query_scalar::<_,i64>("select active_generation_id from continuous_slots where id=$1").bind(f.slot).fetch_one(&f.db.pool).await.unwrap(),candidate);
    assert_eq!(sqlx::query_scalar::<_,i64>("select adaptive_generation_id from live_setups where adaptive_slot_id=$1 and extract(epoch from close_time)::bigint=$2")
        .bind(f.slot).bind(f.now).fetch_one(&f.db.pool).await.unwrap(),f.generation);
    {let mut journal=f.market.journal.lock().unwrap();journal.available_through=Some(f.now+60);journal.repair(f.now);}
    f.runtime.tick_at(&f.db.pool,&f.client,&AtomicBool::new(false),f.now+65).await.unwrap();
    assert_eq!(sqlx::query_scalar::<_,i64>("select adaptive_generation_id from live_setups where adaptive_slot_id=$1 and extract(epoch from close_time)::bigint=$2")
        .bind(f.slot).bind(f.now+60).fetch_one(&f.db.pool).await.unwrap(),candidate);
    assert_eq!(f.market.reads.lock().unwrap().len(),baseline_reads);assert_eq!(f.runtime.retained_history_epochs(),1);
    f.close().await;
}

#[tokio::test]
async fn pending_late_final_preserves_incumbent_through_watermark_and_new_generation_starts_after_it() {
    let Some(mut f)=LiveFixture::new("latepromotion",false).await else {return;};
    // Define this fixture's original accepted scope before any materialization.
    let (spec,result)=scoped_profile(f.now,&["BTCUSDT"]);
    sqlx::query("update continuous_generations set spec=$2,result=$3 where id=$1").bind(f.generation).bind(&spec).bind(&result).execute(&f.db.pool).await.unwrap();
    f.market.journal.lock().unwrap().available_through=Some(f.now-60);
    f.runtime.tick_at(&f.db.pool,&f.client,&AtomicBool::new(false),f.now-60).await.unwrap();
    let candidate=stage(&f.db.pool,f.slot,&spec,&result).await;
    f.runtime.tick_at(&f.db.pool,&f.client,&AtomicBool::new(false),f.now).await.unwrap();
    assert_eq!(sqlx::query_scalar::<_,i64>("select active_generation_id from continuous_slots where id=$1").bind(f.slot).fetch_one(&f.db.pool).await.unwrap(),f.generation);
    assert_eq!(f.durable().await.1.unwrap().state["nextOpenUnix"],f.now-60);
    {let mut journal=f.market.journal.lock().unwrap();journal.available_through=Some(f.now);journal.repair(f.now-60);}
    f.runtime.tick_at(&f.db.pool,&f.client,&AtomicBool::new(false),f.now+5).await.unwrap();
    assert_eq!(sqlx::query_scalar::<_,i64>("select active_generation_id from continuous_slots where id=$1").bind(f.slot).fetch_one(&f.db.pool).await.unwrap(),candidate);
    assert_eq!(sqlx::query_scalar::<_,i64>("select adaptive_generation_id from live_setups where adaptive_slot_id=$1 and extract(epoch from close_time)::bigint=$2").bind(f.slot).bind(f.now).fetch_one(&f.db.pool).await.unwrap(),f.generation);
    {let mut journal=f.market.journal.lock().unwrap();journal.available_through=Some(f.now+60);journal.repair(f.now);}
    f.runtime.tick_at(&f.db.pool,&f.client,&AtomicBool::new(false),f.now+65).await.unwrap();
    assert_eq!(sqlx::query_scalar::<_,i64>("select adaptive_generation_id from live_setups where adaptive_slot_id=$1 and extract(epoch from close_time)::bigint=$2").bind(f.slot).bind(f.now+60).fetch_one(&f.db.pool).await.unwrap(),candidate);
    assert_eq!(f.runtime.retained_history_epochs(),1);f.close().await;
}

#[tokio::test]
async fn candidate_prefix_failure_reports_error_and_keeps_incumbent_hot_and_history_immutable() {
    let Some(mut f)=LiveFixture::new("badcandidate",false).await else {return;};
    f.tick().await.unwrap();let before=f.durable().await.0;
    let (spec,mut result)=scoped_profile(f.now,&["BTCUSDT"]);
    result["bootstrap"]["frozenPrefix"]["instruments"][0]["sha256"]="0".repeat(64).into();
    let candidate=stage(&f.db.pool,f.slot,&spec,&result).await;
    assert!(f.tick().await.unwrap_err().to_string().contains("fingerprint mismatch"));
    let row=sqlx::query("select active_generation_id,pending_generation_id,pending_status,pending_error from continuous_slots where id=$1").bind(f.slot).fetch_one(&f.db.pool).await.unwrap();
    assert_eq!(row.get::<i64,_>("active_generation_id"),f.generation);assert_eq!(row.get::<i64,_>("pending_generation_id"),candidate);
    assert_eq!(row.get::<String,_>("pending_status"),"error");assert!(row.get::<Value,_>("pending_error")["message"].as_str().unwrap().contains("fingerprint mismatch"));
    assert_eq!(f.durable().await.0,before);
    {let mut journal=f.market.journal.lock().unwrap();journal.available_through=Some(f.now+60);journal.repair(f.now);}
    f.runtime.tick_at(&f.db.pool,&f.client,&AtomicBool::new(false),f.now+65).await.unwrap();
    assert_eq!(sqlx::query_scalar::<_,i64>("select adaptive_generation_id from live_setups where adaptive_slot_id=$1 and extract(epoch from close_time)::bigint=$2").bind(f.slot).bind(f.now+60).fetch_one(&f.db.pool).await.unwrap(),f.generation);
    assert_eq!(f.runtime.retained_history_epochs(),1);f.close().await;
}

#[tokio::test]
async fn promotion_cas_conflict_is_reread_and_never_blindly_activates_candidate() {
    let Some(mut f)=LiveFixture::new("promotioncas",false).await else {return;};
    f.tick().await.unwrap();let old=f.durable().await.1.unwrap();
    let (spec,result)=scoped_profile(f.now,&["BTCUSDT","NOTINSTUDYUSDT"]);let candidate=stage(&f.db.pool,f.slot,&spec,&result).await;
    sqlx::raw_sql("create function fixture_cursor_race() returns trigger language plpgsql as $$ begin update adaptive_live_checkpoints set state=jsonb_set(state,'{lastRawClusterTime}','1'::jsonb) where slot_id=new.id and instrument_key='bybit/linear/BTCUSDT'; return new; end $$; create trigger fixture_cursor_race after update of pending_status on continuous_slots for each row when (new.pending_status='ready') execute function fixture_cursor_race();")
        .execute(&f.db.pool).await.unwrap();
    f.tick().await.unwrap();
    assert_eq!(sqlx::query_scalar::<_,i64>("select active_generation_id from continuous_slots where id=$1").bind(f.slot).fetch_one(&f.db.pool).await.unwrap(),f.generation);
    sqlx::raw_sql("drop trigger fixture_cursor_race on continuous_slots; drop function fixture_cursor_race();").execute(&f.db.pool).await.unwrap();
    sqlx::query("update adaptive_live_checkpoints set state=$2 where slot_id=$1 and instrument_key='bybit/linear/BTCUSDT'").bind(f.slot).bind(&old.state).execute(&f.db.pool).await.unwrap();
    f.tick().await.unwrap();
    assert_eq!(sqlx::query_scalar::<_,i64>("select active_generation_id from continuous_slots where id=$1").bind(f.slot).fetch_one(&f.db.pool).await.unwrap(),candidate);
    f.close().await;
}

#[tokio::test]
async fn pending_promotes_explicit_candidate_with_union_absence_and_reuses_one_epoch_across_replacements() {
    let Some(mut f)=LiveFixture::new("promotion",false).await else {return;};
    let sibling:i64=sqlx::query_scalar("select max(id) from continuous_slots").fetch_one(&f.db.pool).await.unwrap();
    let (spec,result)=scoped_profile(f.now,&["BTCUSDT"]);activate(&f.db.pool,sibling,&spec,&result).await;
    f.tick().await.unwrap();let original=f.durable().await.0;
    let mut final_generation=0;
    for round in 0..4 {
        let scope=if round%2==0 {vec!["BTCUSDT","EXTRA4USDT","EXTRA5USDT","EXTRA6USDT","EXTRA7USDT","EXTRA8USDT","EXTRA9USDT"]}
            else {vec!["BTCUSDT","NOTINSTUDYUSDT"]};
        let (spec,result)=scoped_profile(f.now,&scope);
        final_generation=stage(&f.db.pool,f.slot,&spec,&result).await;
        for _ in 0..10 {
            f.runtime.tick_at(&f.db.pool,&f.client,&AtomicBool::new(false),f.now+5).await.unwrap();
            let active:i64=sqlx::query_scalar("select active_generation_id from continuous_slots where id=$1").bind(f.slot).fetch_one(&f.db.pool).await.unwrap();
            if active==final_generation {break;}
        }
        let row=sqlx::query("select active_generation_id,pending_generation_id,live_watermark from continuous_slots where id=$1").bind(f.slot).fetch_one(&f.db.pool).await.unwrap();
        assert_eq!(row.get::<i64,_>("active_generation_id"),final_generation);
        assert!(row.get::<Option<i64>,_>("pending_generation_id").is_none());assert_eq!(row.get::<i64,_>("live_watermark"),f.now);
        assert_eq!(f.runtime.retained_history_epochs(),scope.len(),"drained old-only rings and all duplicate staging epochs must be released");
        assert!(f.runtime.peak_staging_epochs()<=4);assert!(f.runtime.retained_history_bytes()<4*1024*1024);
        assert_eq!(f.durable().await.0,original,"muted warmup/replacement must never republish history");
        let checkpoints:Vec<String>=sqlx::query_scalar("select instrument_key from adaptive_live_checkpoints where slot_id=$1 order by instrument_key")
            .bind(f.slot).fetch_all(&f.db.pool).await.unwrap();
        let mut wanted:Vec<_>=scope.iter().map(|symbol|format!("bybit/linear/{symbol}")).collect();wanted.sort();assert_eq!(checkpoints,wanted);
    }
    // Old owns <=W; only the promoted generation owns the next real close.
    {let mut journal=f.market.journal.lock().unwrap();journal.available_through=Some(f.now+60);journal.repair(f.now);}
    f.runtime.tick_at(&f.db.pool,&f.client,&AtomicBool::new(false),f.now+65).await.unwrap();
    let generations:Vec<i64>=sqlx::query_scalar("select adaptive_generation_id from live_setups where adaptive_slot_id=$1 and extract(epoch from close_time)::bigint=$2")
        .bind(f.slot).bind(f.now+60).fetch_all(&f.db.pool).await.unwrap();
    assert_eq!(generations,vec![final_generation,final_generation]);
    assert_eq!(continuous::get_adaptive_checkpoint(&f.db.pool,sibling,"bybit/linear/BTCUSDT").await.unwrap().unwrap().state["nextOpenUnix"],f.now+60);
    f.close().await;
}
