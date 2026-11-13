use super::*;
use setups_domain::{SpikeDropParams, SpikeParams};
use setups_replay::{ArrayCandleSource, CandleBarDto};
use sqlx::{
    Connection, PgConnection,
    postgres::{PgConnectOptions, PgPoolOptions},
};
use std::str::FromStr;

#[path = "guard_tests.rs"]
mod journal_guards;

fn token(revision: u64) -> TailReadToken {
    TailReadToken {
        epoch: "fixture".into(),
        revision: revision.to_string(),
    }
}

fn algorithms() -> Vec<LiveAlgorithm> {
    [
        AlgorithmParams::Spike(SpikeParams {
            baseline_long_days: 1,
            baseline_short_days: 1,
            window_minutes: 1,
            price_growth_pct: "0".into(),
            volume_multiplier: "0.5".into(),
            cooldown_minutes: 3,
            max_gap_percent: "2".into(),
            ..SpikeParams::default()
        }),
        AlgorithmParams::SpikeDrop(SpikeDropParams {
            baseline_long_days: 2,
            baseline_short_days: 1,
            window_minutes: 1,
            price_drop_pct: "0".into(),
            volume_multiplier: "0.5".into(),
            cooldown_minutes: 5,
            max_gap_percent: "2".into(),
            ..SpikeDropParams::default()
        }),
    ]
    .into_iter()
    .map(|params| LiveAlgorithm {
        config_hash: live_config_hash(
            params.algorithm_version(),
            &serde_json::to_value(&params).unwrap(),
        ),
        checkpoint_suffix: if matches!(params, AlgorithmParams::Spike(_)) {
            ""
        } else {
            ":price_drop_volume_spike"
        },
        stored: Some(Arc::new(json!({"params":params}))),
        params,
    })
    .collect()
}

fn item(id: i64) -> CatalogInstrument {
    CatalogInstrument {
        id,
        exchange: "bybit".into(),
        category: "linear".into(),
        symbol: format!("FIXED{id}USDT"),
        status: "active".into(),
    }
}

fn symbol(algorithms: &[LiveAlgorithm], emit_from: i64) -> Symbol {
    let profiles: Vec<_> = algorithms
        .iter()
        .map(|a| Profile {
            algorithm: a.clone(),
            checkpoint: None,
            emit_from,
            warmup_from: emit_from - i64::from(a.params.baseline_long_days()) * 86400,
            previous_setup: None,
            rolling: None,
            ready: false,
        })
        .collect();
    let read_from = profiles.iter().map(|p| p.warmup_from).min().unwrap();
    Symbol {
        capacity_minutes: limits(algorithms).unwrap().capacity_minutes,
        read_from,
        profiles,
        history: SharedHistory::new(HistoryIdentity([42; 32]), limits(algorithms).unwrap())
            .unwrap(),
        after: None,
        caught_up: false,
        last_closed: None,
        read_token: None,
        observed_to: read_from,
        reconstruction_to: None,
        needs_rebuild: false,
    }
}

fn bars(from: i64, count: usize) -> Vec<CandleBarDto> {
    (0..count)
        .map(|n| CandleBarDto {
            open_time_unix: from + n as i64 * 60,
            close: if n % 79 == 0 {
                "100.0000".into()
            } else {
                "100.00".into()
            },
            volume: "1".into(),
            quote_turnover: if n % 29 == 0 {
                "140.001".into()
            } else {
                "100.00".into()
            },
        })
        .collect()
}

#[tokio::test]
async fn two_profiles_match_reference_replay_through_eviction_gaps_and_retries() {
    let algorithms = algorithms();
    let emit = 2880 * 60;
    let now = 8000 * 60;
    let input = bars(0, 8000)
        .into_iter()
        .enumerate()
        .filter(|(n, _)| !matches!(*n,3100..=3102|4700..=4705|6200))
        .map(|(_, b)| b)
        .collect();
    let source = ArrayCandleSource::new(input);
    let mut symbol = symbol(&algorithms, emit);
    let allocated = symbol.history.retained_bytes();
    let mut actual = Vec::new();
    let stopped = AtomicBool::new(false);
    let mut pages = 0;
    loop {
        let page = source.read(&symbol.query(now)).await.unwrap();
        if pages % 3 == 0 {
            let before = symbol.after;
            let tentative = symbol.prepare(&page, now, &stopped).unwrap();
            assert_eq!(
                symbol.after, before,
                "failed persistence cannot advance committed cursor"
            );
            drop(tentative);
        }
        let batch = symbol.prepare(&page, now, &stopped).unwrap();
        actual.extend(batch.setups.clone());
        let done = batch.caught_up;
        symbol.accept(batch);
        assert_eq!(symbol.history.retained_bytes(), allocated);
        assert!(symbol.history.len() <= limits(&algorithms).unwrap().capacity_minutes);
        pages += 1;
        if done {
            break;
        }
        assert!(pages < 100);
    }
    let mut expected = Vec::new();
    for algorithm in &algorithms {
        let run = replay_run(ReplayRunOptions {
            source: &source,
            from_unix: emit,
            to_unix: now,
            params: Some(algorithm.params.clone()),
            last_setup_unix: None,
            warmup_unix: Some(emit - i64::from(algorithm.params.baseline_long_days()) * 86400),
            cancel: None,
        })
        .await
        .unwrap();
        expected.extend(run.setups);
    }
    actual.sort_by_key(|s| (s.algorithm_id.clone(), s.open_time_unix));
    expected.sort_by_key(|s| (s.algorithm_id.clone(), s.open_time_unix));
    assert!(!expected.is_empty());
    assert_eq!(
        serde_json::to_value(actual).unwrap(),
        serde_json::to_value(expected).unwrap(),
        "exact raw close strings, decimal details, cooldown, and export times"
    );
    assert_eq!(symbol.query(now).after_unix, Some(now - 60));
    assert!(
        symbol.history.oldest_position().0 > 0,
        "test exercised real eviction"
    );
}

#[test]
fn cancelled_and_invalid_pages_never_consume_profile_progress_or_grow_history() {
    let algorithms = algorithms();
    let mut symbol = symbol(&algorithms, 2880 * 60);
    let stopped = AtomicBool::new(true);
    let page = CandlePage {
        bars: bars(0, 1000),
        next_cursor_unix: Some(999 * 60),
    };
    assert!(symbol.prepare(&page, 4000 * 60, &stopped).is_err());
    assert!(symbol.history.is_empty());
    stopped.store(false, Ordering::SeqCst);
    let mut corrupt = page.clone();
    corrupt.bars[100].quote_turnover = "invalid".into();
    let bytes = symbol.history.retained_bytes();
    for _ in 0..30 {
        assert!(symbol.prepare(&corrupt, 4000 * 60, &stopped).is_err());
        assert_eq!(symbol.after, None);
        assert!(symbol.profiles.iter().all(|p| p.rolling.is_none()));
        assert_eq!(symbol.history.len(), 100);
        assert_eq!(symbol.history.retained_bytes(), bytes);
    }
    assert!(symbol.prepare(&page, 4000 * 60, &stopped).is_ok());
}

#[tokio::test]
async fn later_acceptance_in_same_wall_minute_is_not_suppressed() {
    let algorithms = algorithms();
    let mut symbol = symbol(&algorithms, 2880 * 60);
    let stopped = AtomicBool::new(false);
    let source = ArrayCandleSource::new(bars(0, 3000));
    while !symbol.caught_up {
        let page = source.read(&symbol.query(3100 * 60)).await.unwrap();
        let batch = symbol.prepare(&page, 3100 * 60, &stopped).unwrap();
        symbol.accept(batch);
    }
    let empty = source.read(&symbol.query(3100 * 60)).await.unwrap();
    assert!(empty.bars.is_empty());
    let batch = symbol.prepare(&empty, 3100 * 60, &stopped).unwrap();
    symbol.accept(batch);
    let previous = symbol.after;
    let late = CandlePage {
        bars: bars(3000 * 60, 1),
        next_cursor_unix: None,
    };
    let batch = symbol.prepare(&late, 3100 * 60, &stopped).unwrap();
    assert_ne!(batch.after, previous);
    assert!(
        batch.profiles.iter().all(
            |p| p.rolling.as_ref().unwrap().last_position() == symbol.history.latest_position()
        )
    );
}

#[test]
fn resource_admission_round_robin_and_cleanup_are_explicit() {
    let algorithms = algorithms();
    let mut runtime = Runtime::default();
    assert!(
        runtime
            .sync(vec![item(1), item(2)], &algorithms, 1)
            .unwrap_err()
            .to_string()
            .contains("not truncated")
    );
    assert!(runtime.entries.is_empty());
    runtime
        .sync(vec![item(1), item(2)], &algorithms, usize::MAX)
        .unwrap();
    let (id, mut entry) = runtime.take(true).unwrap();
    assert_eq!(id, 1);
    assert_eq!(runtime.status()["selected"], 2);
    assert_eq!(runtime.status()["inFlight"], 1);
    entry.symbol = Some(symbol(&algorithms, 2880 * 60));
    entry.symbol.as_mut().unwrap().caught_up = true;
    entry.prefetched = Some(Prefetched {
        page: CandlePage {
            bars: vec![],
            next_cursor_unix: None,
        },
        token: token(1),
        observed_to: 2880 * 60,
    });
    runtime.put(id, entry);
    assert_eq!(runtime.take(true).unwrap().0, 2);
    let (id, entry) = runtime.take(false).unwrap();
    assert_eq!(id, 1);
    runtime.put(id, entry);
    for _ in 0..10 {
        runtime.clear();
        runtime
            .sync(vec![item(1)], &algorithms, usize::MAX)
            .unwrap();
    }
    runtime.clear();
    assert!(runtime.entries.is_empty() && runtime.ready.is_empty() && runtime.cold.is_empty());
    assert!(runtime.signature.is_none());
    assert_eq!(checkpoint_key(3, &algorithms[0]), "live-checkpoint:3");
    assert_eq!(
        checkpoint_key(3, &algorithms[1]),
        "live-checkpoint:3:price_drop_volume_spike"
    );
}

#[tokio::test]
async fn shared_tail_batch_reads_both_profiles_once_and_surfaces_transport_errors() {
    use axum::{Json, Router, routing::post};
    use std::sync::atomic::AtomicUsize;
    let calls = Arc::new(AtomicUsize::new(0));
    let failed = Arc::new(AtomicBool::new(false));
    let server_calls = calls.clone();
    let server_failed = failed.clone();
    let app=Router::new().route("/v1/candles/tail-batch",post(move |Json(query):Json<Value>|{
        let calls=server_calls.clone();let failed=server_failed.clone();
        async move {
            calls.fetch_add(1,Ordering::SeqCst);
            if failed.load(Ordering::SeqCst) {return Err(axum::http::StatusCode::SERVICE_UNAVAILABLE);}
            let open=query["toUnix"].as_i64().unwrap()-60;
            let rows=query["instrumentIds"].as_array().unwrap().iter().map(|id|json!({
                "instrumentId":id,"status":"known","latestStoredOpenTimeUnix":open,"contiguousThroughOpenTimeUnix":open,
                "missingRanges":[],"candles":[{"instrumentId":id,"openTimeUnix":open,"open":"100","high":"100","low":"100","close":"100.0000","volume":"1","quoteTurnover":"100"}],
                "readGuard":{"status":"valid","token":{"epoch":"fixture","revision":"1"}}
            })).collect::<Vec<_>>();
            Ok(Json(json!({"checkedAt":"test","instruments":rows})))
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let client = Arc::new(MarketDataClient::new(url, "fixture"));
    let algorithms = algorithms();
    let mut runtime = Runtime::default();
    runtime
        .sync(vec![item(1), item(2), item(3)], &algorithms, usize::MAX)
        .unwrap();
    let now = chrono::Utc::now().timestamp().div_euclid(60) * 60;
    for id in [1, 2, 3] {
        let mut entry = runtime.entries.remove(&id).unwrap();
        runtime.cold.retain(|v| *v != id);
        let mut state = symbol(&algorithms, now - 600);
        state.after = Some(now - 120);
        state.caught_up = true;
        state.read_token = Some(token(1));
        state.observed_to = now - 60;
        entry.symbol = Some(state);
        runtime.put(id, entry);
    }
    let stopped = AtomicBool::new(false);
    prefetch(&mut runtime, &client, &stopped).await.unwrap();
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "three instruments/two profiles use ONE source request"
    );
    assert!(
        runtime
            .entries
            .values()
            .all(|e| e.prefetched.as_ref().unwrap().page.bars.len() == 1)
    );
    prefetch(&mut runtime, &client, &stopped).await.unwrap();
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "unconsumed pages cannot accumulate"
    );
    for entry in runtime.entries.values_mut() {
        entry.prefetched = None;
    }
    failed.store(true, Ordering::SeqCst);
    prefetch(&mut runtime, &client, &stopped).await.unwrap();
    assert!(
        runtime
            .entries
            .values()
            .all(|e| e.error.as_ref().unwrap().contains("503")
                && e.symbol.as_ref().unwrap().after == Some(now - 120))
    );
    stopped.store(true, Ordering::SeqCst);
    assert!(prefetch(&mut runtime, &client, &stopped).await.is_err());
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    runtime.clear();
    assert!(runtime.entries.is_empty());
    server.abort();
    let _ = server.await;
}

struct Database {
    pool: PgPool,
    admin: PgConnection,
    name: String,
}
impl Database {
    async fn new() -> Option<Self> {
        let Ok(url) = std::env::var("TEST_DATABASE_URL") else {
            eprintln!("TEST_DATABASE_URL absent; fixed DB test skipped");
            return None;
        };
        let options = PgConnectOptions::from_str(&url).unwrap();
        assert!(options.get_database().unwrap_or("").ends_with("_test"));
        static SERIAL: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let name = format!(
            "fixed_live_{}_{}_test",
            std::process::id(),
            SERIAL.fetch_add(1, Ordering::SeqCst)
        );
        let mut admin = PgConnection::connect_with(&options).await.unwrap();
        sqlx::query(&format!("CREATE DATABASE {name}"))
            .execute(&mut admin)
            .await
            .unwrap();
        let pool = PgPoolOptions::new()
            .max_connections(4)
            .connect_with(options.database(&name))
            .await
            .unwrap();
        let baseline = include_str!("../../../../db/migrations/0010_baseline.sql")
            .lines()
            .filter(|s| !s.starts_with('\\') && !s.starts_with("SET transaction_timeout"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut connection = pool.acquire().await.unwrap();
        sqlx::raw_sql(&baseline)
            .execute(&mut *connection)
            .await
            .unwrap();
        sqlx::raw_sql("set search_path=public;create table kysely_migration(name varchar(255) primary key,timestamp varchar(255) not null);insert into kysely_migration values('0010_baseline.sql','fixture');")
            .execute(&mut *connection).await.unwrap();
        drop(connection);
        setups_db::migrate::ensure_migrations(&pool).await.unwrap();
        Some(Self { pool, admin, name })
    }
    async fn close(mut self) {
        self.pool.close().await;
        sqlx::query(&format!("DROP DATABASE {} WITH (FORCE)", self.name))
            .execute(&mut self.admin)
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn postgres_fixed_batch_rolls_back_errors_stop_config_and_checkpoint_conflicts() {
    let Some(db) = Database::new().await else {
        return;
    };
    settings_set(&db.pool, "live", &json!({"enabled":true}))
        .await
        .unwrap();
    settings_set(&db.pool, "notifications", &json!({"maxAgeSeconds":3600}))
        .await
        .unwrap();
    let algorithms = algorithms();
    for a in &algorithms {
        settings_set(
            &db.pool,
            &format!("algorithm:{}", a.params.algorithm_id()),
            a.stored.as_ref().unwrap(),
        )
        .await
        .unwrap();
    }
    let item = item(41);
    let now = chrono::Utc::now().timestamp().div_euclid(60) * 60;
    let mut symbol = Symbol::load(&db.pool, &item, &algorithms, now, now)
        .await
        .unwrap();
    let source = ArrayCandleSource::new(bars(symbol.read_from, 2900));
    let stopped = AtomicBool::new(false);
    let mut pending = loop {
        let page = source.read(&symbol.query(now)).await.unwrap();
        let mut batch = symbol.prepare(&page, now, &stopped).unwrap();
        batch.read_token = Some(token(1));
        if !batch.setups.is_empty() {
            break batch;
        }
        if symbol.reconstruction_to.is_none() || batch.caught_up {
            persist(&db.pool, &item, &mut batch, &stopped)
                .await
                .unwrap();
        }
        symbol.accept(batch);
    };
    let before = symbol.after;
    sqlx::raw_sql("create function fixed_test_reject() returns trigger language plpgsql as $$begin raise exception 'fixed test injected failure';end$$;create trigger fixed_test_reject before insert on live_setups for each row execute function fixed_test_reject();")
        .execute(&db.pool).await.unwrap();
    for _ in 0..3 {
        assert!(
            persist(&db.pool, &item, &mut pending, &stopped)
                .await
                .is_err()
        );
        assert_eq!(symbol.after, before);
    }
    assert_eq!(
        sqlx::query_scalar::<_, i64>("select count(*) from live_setups")
            .fetch_one(&db.pool)
            .await
            .unwrap(),
        0
    );
    sqlx::raw_sql(
        "drop trigger fixed_test_reject on live_setups;drop function fixed_test_reject();",
    )
    .execute(&db.pool)
    .await
    .unwrap();
    stopped.store(true, Ordering::SeqCst);
    assert!(
        persist(&db.pool, &item, &mut pending, &stopped)
            .await
            .is_err()
    );
    stopped.store(false, Ordering::SeqCst);
    let key = checkpoint_key(item.id, &algorithms[0]);
    settings_set(
        &db.pool,
        &key,
        &json!({"nextUnix":1,"configHash":"racing writer"}),
    )
    .await
    .unwrap();
    assert!(
        persist(&db.pool, &item, &mut pending, &stopped)
            .await
            .unwrap_err()
            .to_string()
            .contains("CAS")
    );
    settings_set(
        &db.pool,
        &key,
        symbol.profiles[0].checkpoint.as_ref().unwrap(),
    )
    .await
    .unwrap();
    settings_set(
        &db.pool,
        "algorithm:price_volume_spike",
        &json!({"enabled":false}),
    )
    .await
    .unwrap();
    assert!(
        persist(&db.pool, &item, &mut pending, &stopped)
            .await
            .is_err()
    );
    settings_set(
        &db.pool,
        "algorithm:price_volume_spike",
        algorithms[0].stored.as_ref().unwrap(),
    )
    .await
    .unwrap();
    assert!(pending.setups.len() > 1);
    let reject = &pending.setups[1];
    sqlx::raw_sql(&format!("create function fixed_test_partial() returns trigger language plpgsql as $$begin if new.open_time=to_timestamp({}) and new.algorithm_id='{}' then raise exception 'fixed test partial failure';end if;return new;end$$;create trigger fixed_test_partial before insert on live_setups for each row execute function fixed_test_partial();",reject.open_time_unix,reject.algorithm_id))
        .execute(&db.pool).await.unwrap();
    assert!(
        persist(&db.pool, &item, &mut pending, &stopped)
            .await
            .is_err()
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("select count(*) from live_setups")
            .fetch_one(&db.pool)
            .await
            .unwrap(),
        1,
        "first setup commits atomically with its delivery job before the second setup fails"
    );
    assert_eq!(symbol.after, before);
    sqlx::raw_sql(
        "drop trigger fixed_test_partial on live_setups;drop function fixed_test_partial();",
    )
    .execute(&db.pool)
    .await
    .unwrap();
    persist(&db.pool, &item, &mut pending, &stopped)
        .await
        .unwrap();
    let count=sqlx::query_scalar::<_,i64>("select count(*) from live_setups where adaptive_slot_id is null and recommendation is null and extract(epoch from close_time-open_time)=60").fetch_one(&db.pool).await.unwrap();
    assert_eq!(count, pending.setups.len() as i64);
    assert_eq!(sqlx::query_scalar::<_,i64>("select count(*) from adaptive_notification_outbox where fixed_config is not null and payload->'recommendation'='null'::jsonb and extract(epoch from expires_at)::bigint=raw_close_unix+3600").fetch_one(&db.pool).await.unwrap(),count,"all fixed WATCH signals have one durable job with raw-close TTL; retry created no duplicate");
    symbol.accept(pending);
    let saved = settings_get(&db.pool, &key).await.unwrap().unwrap();
    assert_eq!(saved["nextUnix"], symbol.after.unwrap() + 60);
    let origin = symbol.read_from;
    let cutoff = symbol.observed_to;
    let mut seeds = Vec::new();
    for profile in &symbol.profiles {
        seeds.push(
            last_live_setup_open_time(
                &db.pool,
                &item.exchange,
                &item.category,
                &item.symbol,
                profile.algorithm.params.algorithm_id(),
                profile.algorithm.params.algorithm_version(),
                &serde_json::to_value(&profile.algorithm.params).unwrap(),
            )
            .await
            .unwrap(),
        );
    }
    assert!(seeds.iter().all(Option::is_some));
    symbol.invalidate();
    symbol.restart(&seeds).unwrap();
    assert_eq!(symbol.read_from, origin);
    assert!(symbol.profiles.iter().all(|p| p.emit_from >= cutoff));
    for _ in 0..100 {
        if symbol.reconstruction_to.is_none() {
            break;
        }
        let page = source.read(&symbol.query(now)).await.unwrap();
        let mut batch = symbol.prepare(&page, now, &stopped).unwrap();
        batch.read_token = Some(token(2));
        assert!(
            batch.setups.is_empty(),
            "repair reconstruction must not retroactively publish historical raws"
        );
        if batch.caught_up {
            persist(&db.pool, &item, &mut batch, &stopped)
                .await
                .unwrap();
        }
        symbol.accept(batch);
    }
    assert!(symbol.reconstruction_to.is_none());
    assert_eq!(
        sqlx::query_scalar::<_, i64>("select count(*) from live_setups")
            .fetch_one(&db.pool)
            .await
            .unwrap(),
        count
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("select count(*) from adaptive_notification_outbox")
            .fetch_one(&db.pool)
            .await
            .unwrap(),
        count
    );
    let reloaded = Symbol::load(&db.pool, &item, &algorithms, now, now)
        .await
        .unwrap();
    assert_eq!(
        reloaded.read_from, origin,
        "restart restores the original causal origin, never cursor-minus-baseline"
    );
    assert!(
        reloaded.read_token.is_none(),
        "reconstruction must capture a new token, not reuse an older proof"
    );
    for (profile, seed) in reloaded.profiles.iter().zip(&seeds) {
        assert_eq!(profile.previous_setup, *seed);
        assert!(profile.emit_from >= cutoff);
        assert_eq!(
            profile.checkpoint.as_ref().unwrap()["reconstructionSeedOpenUnix"],
            json!(seed)
        );
    }
    let expanded_key = checkpoint_key(item.id, algorithms.last().unwrap());
    let mut expanded = settings_get(&db.pool, &expanded_key)
        .await
        .unwrap()
        .unwrap();
    expanded["warmupFromUnix"] = json!(origin - 60);
    settings_set(&db.pool, &expanded_key, &expanded)
        .await
        .unwrap();
    let expanded = Symbol::load(&db.pool, &item, &algorithms, now, now)
        .await
        .unwrap();
    assert_eq!(expanded.read_from, origin - 60);
    assert!(
        expanded.read_token.is_none()
            && expanded.history.is_empty()
            && expanded.reconstruction_to.is_some(),
        "earlier causal origin always captures/reconstructs anew; a later-origin proof cannot be reused"
    );
    db.close().await;
}
