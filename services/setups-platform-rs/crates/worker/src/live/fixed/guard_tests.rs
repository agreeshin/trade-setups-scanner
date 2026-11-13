//! Exercises the real fixed guard/page pipeline against a bounded HTTP journal
//! fixture. Persistence atomicity is covered separately by the PostgreSQL test.
use super::*;
use axum::{
    Json, Router,
    extract::Query,
    routing::{get, post},
};
use serde::Deserialize;
use setups_mdclient::{TailReadGuardQuery, TailReadGuardStatus};
use std::sync::Mutex;

struct Feed {
    bars: Vec<CandleBarDto>,
    revision: u64,
    pruned_through: u64,
    journal: VecDeque<(u64, i64, i64)>,
    guards: VecDeque<TailReadGuardQuery>,
    page_calls: usize,
    mutate_on_page: Option<(usize, CandleBarDto)>,
}
impl Feed {
    fn insert(&mut self, bar: CandleBarDto) {
        assert!(self.bars.len() < 20_000);
        assert!(
            !self
                .bars
                .iter()
                .any(|b| b.open_time_unix == bar.open_time_unix)
        );
        self.revision += 1;
        self.journal
            .push_back((self.revision, bar.open_time_unix, bar.open_time_unix + 60));
        if self.journal.len() > 1024 {
            self.pruned_through = self.journal.pop_front().unwrap().0;
        }
        self.bars.push(bar);
        self.bars.sort_by_key(|b| b.open_time_unix);
    }
    fn guard(&mut self, query: &TailReadGuardQuery) -> TailReadGuard {
        self.guards.push_back(query.clone());
        if self.guards.len() > 512 {
            self.guards.pop_front();
        }
        let status = match &query.previous {
            None => TailReadGuardStatus::Unknown, // capture is deliberately NOT proof
            Some(previous)
                if previous.epoch != "fixture"
                    || previous.revision.parse::<u64>().unwrap() < self.pruned_through =>
            {
                TailReadGuardStatus::Unknown
            }
            Some(previous) => {
                let revision = previous.revision.parse::<u64>().unwrap();
                if self.journal.iter().any(|(r, from, to)| {
                    *r > revision && *from < query.to_unix && *to > query.from_unix
                }) {
                    TailReadGuardStatus::Invalid
                } else {
                    TailReadGuardStatus::Valid
                }
            }
        };
        TailReadGuard {
            status,
            token: Some(token(self.revision)),
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PageQuery {
    from_unix: i64,
    to_unix: i64,
    after_unix: Option<i64>,
    limit: Option<usize>,
}

fn candle_json(bar: &CandleBarDto) -> Value {
    json!({"instrumentId":1,"openTimeUnix":bar.open_time_unix,"open":"100","high":"100","low":"100",
        "close":bar.close,"volume":bar.volume,"quoteTurnover":bar.quote_turnover,"source":"bybit"})
}

struct Fixture {
    client: Arc<MarketDataClient>,
    feed: Arc<Mutex<Feed>>,
    server: tokio::task::JoinHandle<()>,
}
impl Fixture {
    async fn new(bars: Vec<CandleBarDto>) -> Self {
        let feed = Arc::new(Mutex::new(Feed {
            bars,
            revision: 1,
            pruned_through: 0,
            journal: VecDeque::new(),
            guards: VecDeque::new(),
            page_calls: 0,
            mutate_on_page: None,
        }));
        let pages = feed.clone();
        let tails = feed.clone();
        let app=Router::new().route("/v1/candles",get(move |Query(query):Query<PageQuery>| {
            let feed=pages.clone();
            async move {
                let mut feed=feed.lock().unwrap();
                feed.page_calls+=1;
                let limit=query.limit.unwrap_or(PAGE_BARS);
                let rows=feed.bars.iter().filter(|b|b.open_time_unix>=query.from_unix && b.open_time_unix<query.to_unix
                    && query.after_unix.is_none_or(|p|b.open_time_unix>p)).take(limit+1).cloned().collect::<Vec<_>>();
                let has_more=rows.len()>limit;
                let rows=rows.into_iter().take(limit).collect::<Vec<_>>();
                let cursor=has_more.then(||rows.last().unwrap().open_time_unix);
                // Return the preceding page snapshot, then commit the repair.
                // The consumer's post-read guard must reject this candidate.
                if feed.mutate_on_page.as_ref().is_some_and(|(n,_)|*n==feed.page_calls) {
                    let (_,bar)=feed.mutate_on_page.take().unwrap();feed.insert(bar);
                }
                Json(json!({"candles":rows.iter().map(candle_json).collect::<Vec<_>>(),"nextCursorUnix":cursor}))
            }
        })).route("/v1/candles/tail-batch",post(move |Json(query):Json<Value>| {
            let feed=tails.clone();
            async move {
                let mut feed=feed.lock().unwrap();
                let guards=serde_json::from_value::<Vec<TailReadGuardQuery>>(query["readGuards"].clone()).unwrap();
                let from=query["fromUnix"].as_i64().unwrap();let to=query["toUnix"].as_i64().unwrap();
                let rows=feed.bars.iter().filter(|b|b.open_time_unix>=from && b.open_time_unix<to).map(candle_json).collect::<Vec<_>>();
                let mut missing=Vec::new();let mut cursor=from;let mut contiguous=None;
                for row in &rows {
                    let open=row["openTimeUnix"].as_i64().unwrap();
                    if cursor<open {missing.push(json!({"fromUnix":cursor,"toUnix":open}));}
                    if missing.is_empty() {contiguous=Some(open);}
                    cursor=open+60;
                }
                if cursor<to {missing.push(json!({"fromUnix":cursor,"toUnix":to}));}
                let result=feed.guard(&guards[0]);
                Json(json!({"checkedAt":"fixture","instruments":[{"instrumentId":1,"status":"known",
                    "latestStoredOpenTimeUnix":feed.bars.last().map(|b|b.open_time_unix),
                    "contiguousThroughOpenTimeUnix":contiguous,"missingRanges":missing,"candles":rows,"readGuard":result}]}))
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = Arc::new(MarketDataClient::new(
            format!("http://{}", listener.local_addr().unwrap()),
            "fixture",
        ));
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            client,
            feed,
            server,
        }
    }
    fn insert(&self, open: i64) {
        self.feed.lock().unwrap().insert(bars(open, 1).remove(0));
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}

fn reconstructing(origin: i64, cutoff: i64) -> Symbol {
    let one = vec![algorithms().remove(0)];
    let mut state = symbol(&one, cutoff);
    state.read_from = origin;
    state.observed_to = origin;
    state.profiles[0].warmup_from = origin;
    state.reconstruction_to = Some(cutoff);
    state
}

async fn step(fixture: &Fixture, state: &mut Symbol, now: i64) -> Option<Vec<SpikeSetup>> {
    assert!(
        !state.needs_rebuild,
        "caller must reconstruct after invalidation"
    );
    let stopped = AtomicBool::new(false);
    let source = Source {
        client: &fixture.client,
        instrument_id: 1,
    };
    let page = guard::historical_page(&fixture.client, 1, state, &source, now, &stopped)
        .await
        .unwrap()?;
    let mut batch = state
        .prepare(&page.page, page.observed_to, &stopped)
        .unwrap();
    batch.read_token = Some(page.token);
    let setups = batch.setups.clone();
    state.accept(batch);
    Some(setups)
}

async fn finish_reconstruction(fixture: &Fixture, state: &mut Symbol, now: i64) {
    for _ in 0..100 {
        if state.reconstruction_to.is_none() {
            return;
        }
        assert!(
            step(fixture, state, now).await.unwrap().is_empty(),
            "reconstruction never publishes historical raw signals"
        );
    }
    panic!("bounded fixture reconstruction did not finish");
}

#[tokio::test]
async fn evicted_consumed_repair_rebuilds_original_cooldown_phase_before_publication() {
    let origin = 10_000 * 60;
    let cutoff = origin + 6000 * 60;
    let input = bars(origin, 6020).into_iter().skip(1).collect(); // initial missing origin changes cooldown phase
    let fixture = Fixture::new(input).await;
    let mut state = reconstructing(origin, cutoff);
    finish_reconstruction(&fixture, &mut state, cutoff + 20 * 60).await;
    assert!(state.history.oldest_position().0 > 0);
    let previous = state.profiles[0]
        .rolling
        .as_ref()
        .unwrap()
        .last_raw_open_unix()
        .unwrap();
    fixture.insert(origin); // repaired bar is well outside the parsed ring
    let after = state.after;
    assert!(step(&fixture, &mut state, cutoff + 20 * 60).await.is_none());
    assert!(state.needs_rebuild);
    assert_eq!(
        state.after, after,
        "invalid response cannot consume detector progress"
    );
    assert_eq!(
        state.profiles[0]
            .rolling
            .as_ref()
            .unwrap()
            .last_raw_open_unix(),
        Some(previous)
    );
    state.restart(&[None]).unwrap();
    assert_eq!(state.read_from, origin);
    assert_eq!(state.reconstruction_to, Some(cutoff));
    finish_reconstruction(&fixture, &mut state, cutoff + 20 * 60).await;
    let mut actual = Vec::new();
    for _ in 0..10 {
        actual.extend(step(&fixture, &mut state, cutoff + 20 * 60).await.unwrap());
        if state.caught_up {
            break;
        }
    }
    let source = ArrayCandleSource::new(fixture.feed.lock().unwrap().bars.clone());
    let expected = replay_run(ReplayRunOptions {
        source: &source,
        from_unix: cutoff,
        to_unix: cutoff + 20 * 60,
        params: Some(state.profiles[0].algorithm.params.clone()),
        last_setup_unix: None,
        warmup_unix: Some(origin),
        cancel: None,
    })
    .await
    .unwrap();
    assert!(!expected.setups.is_empty());
    assert_eq!(
        serde_json::to_value(actual).unwrap(),
        serde_json::to_value(expected.setups).unwrap()
    );
    assert_eq!(
        state.read_from, origin,
        "eviction and rebuild never rebase causality"
    );
}

#[tokio::test]
async fn warmup_keeps_capture_token_and_full_prefix_check_rejects_mid_page_mutation() {
    let origin = 10_000 * 60;
    let cutoff = origin + 4000 * 60;
    let fixture = Fixture::new(bars(origin, 4000).into_iter().skip(1).collect()).await;
    let mut state = reconstructing(origin, cutoff);
    step(&fixture, &mut state, cutoff).await.unwrap();
    assert_eq!(state.read_token, Some(token(1)));
    let page_calls = fixture.feed.lock().unwrap().page_calls;
    fixture.feed.lock().unwrap().mutate_on_page = Some((page_calls + 1, bars(origin, 1).remove(0)));
    let after = state.after;
    assert!(step(&fixture, &mut state, cutoff).await.is_none());
    assert_eq!(state.after, after, "candidate page rejected before prepare");
    assert!(state.needs_rebuild);
    let feed = fixture.feed.lock().unwrap();
    assert!(feed.guards.back().unwrap().to_unix > after.unwrap() + 60);
    assert_eq!(feed.guards.back().unwrap().from_unix, origin);
    assert_eq!(feed.guards.back().unwrap().previous, Some(token(1)));
}

#[tokio::test]
async fn irrelevant_old_and_normal_forward_inserts_do_not_restart_warmup_or_rebase_token() {
    let origin = 10_000 * 60;
    let cutoff = origin + 4000 * 60;
    let fixture = Fixture::new(bars(origin, 4000)).await;
    let mut state = reconstructing(origin, cutoff);
    step(&fixture, &mut state, cutoff).await.unwrap();
    fixture.insert(origin - 60);
    fixture.insert(cutoff); // normal forward WS insert: exactly outside frozen warmup
    step(&fixture, &mut state, cutoff + 60).await.unwrap();
    assert_eq!(
        state.read_token,
        Some(token(1)),
        "intermediate valid response must not replace original warmup capture"
    );
    assert!(!state.needs_rebuild);
    finish_reconstruction(&fixture, &mut state, cutoff + 60).await;
    assert_eq!(
        state.read_token,
        Some(token(3)),
        "only complete full-prefix validation adopts newer token"
    );
    assert_eq!(state.read_from, origin);
    let before = state.after;
    fixture.insert(origin - 120);
    step(&fixture, &mut state, cutoff).await.unwrap();
    assert_eq!(state.after, before);
    assert_eq!(state.read_token, Some(token(4)));
    assert!(!state.needs_rebuild);
}

#[tokio::test]
async fn trailing_absence_is_pending_until_a_later_processed_bar_makes_the_gap_interior() {
    let origin = 10_000 * 60;
    let cutoff = origin + 2000 * 60;
    let observed = cutoff + 120;
    let fixture = Fixture::new(bars(origin, 2000)).await;
    let mut state = reconstructing(origin, cutoff);
    finish_reconstruction(&fixture, &mut state, observed).await;
    assert!(
        step(&fixture, &mut state, observed)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(state.observed_to, observed);
    assert_eq!(state.after, Some(cutoff - 60));
    assert_eq!(
        state.guarded_to(),
        cutoff,
        "query EOF does not consume missing trailing minutes"
    );
    fixture.insert(cutoff + 60); // ordinary forward input; cutoff is still missing
    step(&fixture, &mut state, observed).await.unwrap();
    assert!(!state.needs_rebuild);
    assert_eq!(
        state.guarded_to(),
        observed,
        "processing the later real bar consumes the interior gap"
    );
    fixture.insert(cutoff); // now this repairs an actually consumed INTERIOR gap
    assert!(step(&fixture, &mut state, observed).await.is_none());
    state.restart(&[None]).unwrap();
    assert_eq!(state.profiles[0].emit_from, observed);
    finish_reconstruction(&fixture, &mut state, observed).await;
    assert!(
        step(&fixture, &mut state, observed)
            .await
            .unwrap()
            .is_empty()
    );
    fixture.insert(observed); // half-open endpoint is an ordinary next live input
    step(&fixture, &mut state, observed + 60).await.unwrap();
    assert!(!state.needs_rebuild);
}

fn publication_algorithm() -> LiveAlgorithm {
    let mut algorithm = algorithms().remove(0);
    if let AlgorithmParams::Spike(params) = &mut algorithm.params {
        params.volume_multiplier = "3".into();
        params.required_recent_minutes = 0;
    }
    algorithm.config_hash = live_config_hash(
        algorithm.params.algorithm_version(),
        &serde_json::to_value(&algorithm.params).unwrap(),
    );
    algorithm.stored = Some(Arc::new(json!({"params":algorithm.params})));
    algorithm
}

async fn enable_publication(db: &Database, algorithm: &LiveAlgorithm) {
    settings_set(&db.pool, "live", &json!({"enabled":true}))
        .await
        .unwrap();
    settings_set(&db.pool, "notifications", &json!({"maxAgeSeconds":3600}))
        .await
        .unwrap();
    settings_set(
        &db.pool,
        &format!("algorithm:{}", algorithm.params.algorithm_id()),
        algorithm.stored.as_ref().unwrap(),
    )
    .await
    .unwrap();
}

fn comparable_setup(setup: &SpikeSetup) -> Value {
    json!({"algorithmId":setup.algorithm_id,"algorithmVersion":setup.algorithm_version,
        "openTimeUnix":setup.open_time_unix,"closeTimeUnix":setup.close_time_unix,
        "closePrice":setup.close_price,"params":setup.params,"details":setup.details})
}

async fn saved_setups(pool: &PgPool, symbol: &str, from: i64) -> Vec<Value> {
    use sqlx::Row;
    sqlx::query("select algorithm_id,algorithm_version,extract(epoch from open_time)::bigint as open_unix,extract(epoch from close_time)::bigint as close_unix,close_price,params,details from live_setups where symbol=$1 and open_time>=to_timestamp($2) order by open_time,algorithm_id")
        .bind(symbol).bind(from).fetch_all(pool).await.unwrap().into_iter().map(|row|json!({
            "algorithmId":row.get::<String,_>("algorithm_id"),"algorithmVersion":row.get::<String,_>("algorithm_version"),
            "openTimeUnix":row.get::<i64,_>("open_unix"),"closeTimeUnix":row.get::<i64,_>("close_unix"),
            "closePrice":row.get::<String,_>("close_price"),"params":row.get::<Value,_>("params"),"details":row.get::<Option<Value>,_>("details")
        })).collect()
}

async fn finish_work(
    db: &Database,
    fixture: &Fixture,
    entry: &mut Entry,
    algorithms: &[LiveAlgorithm],
) {
    for _ in 0..32 {
        work(
            &db.pool,
            &fixture.client,
            entry,
            algorithms,
            &AtomicBool::new(false),
        )
        .await
        .unwrap();
        if entry.symbol.as_ref().is_some_and(|s| s.caught_up) && entry.prefetched.is_none() {
            return;
        }
    }
    panic!("bounded publication fixture did not catch up");
}

async fn delayed_final_publication_case(restart: bool) {
    let Some(db) = Database::new().await else {
        return;
    };
    let algorithm = publication_algorithm();
    let algorithms = vec![algorithm.clone()];
    enable_publication(&db, &algorithm).await;
    let close = chrono::Utc::now().timestamp().div_euclid(60) * 60;
    let pending = close - 60;
    let origin = close - 1441 * 60;
    let fixture = Fixture::new(bars(origin, 1440)).await; // last stored open = close-120
    let item = item(1);
    let key = checkpoint_key(item.id, &algorithm);
    let checkpoint = json!({"configHash":algorithm.config_hash,"nextUnix":pending,"warmupFromUnix":origin,
        "consumedThroughUnix":pending,"reconstructionSeedOpenUnix":null});
    settings_set(&db.pool, &key, &checkpoint).await.unwrap();
    let mut runtime = Runtime::default();
    runtime
        .sync(vec![item.clone()], &algorithms, usize::MAX)
        .unwrap();
    let (_, mut entry) = runtime.take(true).unwrap();
    finish_work(&db, &fixture, &mut entry, &algorithms).await; // warmup then EOF while the final is absent
    let guarded_after_empty = entry.symbol.as_ref().unwrap().guarded_to();
    let observed_after_empty = entry.symbol.as_ref().unwrap().observed_to;
    entry.next_poll = Instant::now();
    runtime.put(1, entry);
    prefetch(&mut runtime, &fixture.client, &AtomicBool::new(false))
        .await
        .unwrap(); // hot empty path too
    if let Some((id, mut entry)) = runtime.take(false) {
        finish_work(&db, &fixture, &mut entry, &algorithms).await;
        runtime.put(id, entry);
    }
    let empty_checkpoint = settings_get(&db.pool, &key).await.unwrap().unwrap();
    if restart {
        runtime.clear();
        // Compatibility with checkpoints written by the regression: the old
        // observation-only evidence must not override actual nextUnix.
        let mut legacy = empty_checkpoint.clone();
        legacy["consumedThroughUnix"] = json!(close);
        settings_set(&db.pool, &key, &legacy).await.unwrap();
        runtime
            .sync(vec![item.clone()], &algorithms, usize::MAX)
            .unwrap();
        let (_, mut entry) = runtime.take(true).unwrap();
        finish_work(&db, &fixture, &mut entry, &algorithms).await;
        entry.next_poll = Instant::now();
        runtime.put(1, entry);
    }
    let mut late = bars(pending, 1).remove(0);
    late.quote_turnover = "1000".into();
    fixture.feed.lock().unwrap().insert(late); // ordinary delayed final, same closed minute
    runtime.entries.get_mut(&1).unwrap().next_poll = Instant::now();
    prefetch(&mut runtime, &fixture.client, &AtomicBool::new(false))
        .await
        .unwrap();
    let (id, mut entry) = runtime
        .take(false)
        .or_else(|| runtime.take(true))
        .expect("arrival schedules real work");
    finish_work(&db, &fixture, &mut entry, &algorithms).await;
    runtime.put(id, entry);
    runtime.entries.get_mut(&1).unwrap().next_poll = Instant::now();
    prefetch(&mut runtime, &fixture.client, &AtomicBool::new(false))
        .await
        .unwrap();
    if let Some((id, mut entry)) = runtime.take(false) {
        finish_work(&db, &fixture, &mut entry, &algorithms).await;
        runtime.put(id, entry);
    }
    let source = ArrayCandleSource::new(fixture.feed.lock().unwrap().bars.clone());
    let expected = replay_run(ReplayRunOptions {
        source: &source,
        from_unix: pending,
        to_unix: close,
        params: Some(algorithm.params.clone()),
        last_setup_unix: None,
        warmup_unix: Some(origin),
        cancel: None,
    })
    .await
    .unwrap();
    let expected = expected
        .setups
        .iter()
        .map(comparable_setup)
        .collect::<Vec<_>>();
    let actual = saved_setups(&db.pool, &item.symbol, pending).await;
    let outbox = sqlx::query_scalar::<_, i64>("select count(*) from adaptive_notification_outbox")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    runtime.clear();
    db.close().await;
    assert_eq!(
        expected.len(),
        1,
        "fixture has exactly one eligible delayed final"
    );
    assert_eq!(
        actual, expected,
        "restart={restart}: delayed final must not be mistaken for an old repair; guarded_after_empty={guarded_after_empty}, observed_after_empty={observed_after_empty}, empty_checkpoint={empty_checkpoint}"
    );
    assert_eq!(
        outbox, 1,
        "exactly one durable delivery job, including repeated polling"
    );
    assert_eq!(
        guarded_after_empty, pending,
        "unprocessed trailing absence is not in the consumed guard"
    );
    assert!(observed_after_empty >= close);
    assert_eq!(
        empty_checkpoint, checkpoint,
        "empty responses do not move durable publication progress"
    );
}

#[tokio::test]
async fn delayed_final_is_published_exactly_once_through_hot_prefetch_and_work() {
    delayed_final_publication_case(false).await;
}

#[tokio::test]
async fn delayed_final_is_published_exactly_once_after_restart_between_empty_and_arrival() {
    delayed_final_publication_case(true).await;
}

#[tokio::test]
async fn durable_seeded_continuation_matches_reference_and_ignores_json_seed_evidence() {
    let Some(db) = Database::new().await else {
        return;
    };
    let algorithm = publication_algorithm();
    let algorithms = vec![algorithm.clone()];
    enable_publication(&db, &algorithm).await;
    let now = chrono::Utc::now().timestamp().div_euclid(60) * 60;
    let from = now - 6 * 60;
    let seed = from - 60;
    let origin = from - 1440 * 60;
    let mut history = bars(origin, 1440);
    history.last_mut().unwrap().quote_turnover = "1000".into();
    // A historical hole was absent when this real durable setup was emitted.
    // Its later large-volume repair removes the old raw predicate, but must not
    // retract the published row or erase its authoritative cooldown anchor.
    let before_repair = ArrayCandleSource::new(
        history
            .iter()
            .enumerate()
            .filter(|(n, _)| *n != 500)
            .map(|(_, b)| b.clone())
            .collect(),
    );
    let published = replay_run(ReplayRunOptions {
        source: &before_repair,
        from_unix: seed,
        to_unix: from,
        params: Some(algorithm.params.clone()),
        last_setup_unix: None,
        warmup_unix: Some(origin),
        cancel: None,
    })
    .await
    .unwrap();
    assert_eq!(published.setups.len(), 1);
    let item = item(1);
    setups_db::continuous::outbox::commit_fixed_setup(
        &db.pool,
        &item.exchange,
        &item.category,
        &item.symbol,
        &published.setups[0],
    )
    .await
    .unwrap();
    let original_row = saved_setups(&db.pool, &item.symbol, seed).await;
    history[500].quote_turnover = "1000000".into();
    let mut continuation = bars(from, 6);
    for bar in &mut continuation {
        bar.quote_turnover = "100000".into();
    }
    history.extend(continuation);
    let fixture = Fixture::new(history).await;
    let key = checkpoint_key(item.id, &algorithm);
    settings_set(
        &db.pool,
        &key,
        &json!({"configHash":algorithm.config_hash,"nextUnix":from,
        "warmupFromUnix":origin,"consumedThroughUnix":from,
        "reconstructionSeedOpenUnix":now+86400}),
    )
    .await
    .unwrap(); // deliberately false evidence
    let mut entry = Entry {
        item: item.clone(),
        symbol: None,
        error: None,
        next_poll: Instant::now(),
        prefetched: None,
    };
    finish_work(&db, &fixture, &mut entry, &algorithms).await;
    let restored_seed = entry.symbol.as_ref().unwrap().profiles[0].previous_setup;
    let evidence = settings_get(&db.pool, &key).await.unwrap().unwrap();
    let source = ArrayCandleSource::new(fixture.feed.lock().unwrap().bars.clone());
    let expected = replay_run(ReplayRunOptions {
        source: &source,
        from_unix: from,
        to_unix: now,
        params: Some(algorithm.params.clone()),
        last_setup_unix: Some(seed),
        warmup_unix: Some(origin),
        cancel: None,
    })
    .await
    .unwrap();
    let expected = expected
        .setups
        .iter()
        .map(comparable_setup)
        .collect::<Vec<_>>();
    let actual = saved_setups(&db.pool, &item.symbol, from).await;
    let all = saved_setups(&db.pool, &item.symbol, seed).await;
    let outbox = sqlx::query_scalar::<_, i64>("select count(*) from adaptive_notification_outbox")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    drop(entry);
    db.close().await;
    assert_eq!(
        restored_seed,
        Some(seed),
        "seed is restored from live_setups, not checkpoint JSON evidence"
    );
    assert_eq!(evidence["reconstructionSeedOpenUnix"], json!(seed));
    assert_eq!(
        expected.len(),
        2,
        "short continuation exercises cooldown and its boundary"
    );
    assert_eq!(
        actual, expected,
        "durable-seeded continuation matches conditional reference exactly"
    );
    assert_eq!(
        &all[..1],
        original_row.as_slice(),
        "published historical setup is unchanged"
    );
    assert_eq!(all.len(), 3);
    assert_eq!(outbox, 3);
}

#[tokio::test]
async fn pruned_unknown_with_current_token_is_not_proof_and_stop_releases_owned_state() {
    let origin = 10_000 * 60;
    let cutoff = origin + 2000 * 60;
    let fixture = Fixture::new(bars(origin, 2000)).await;
    let mut state = reconstructing(origin, cutoff);
    finish_reconstruction(&fixture, &mut state, cutoff).await;
    for n in 1..=1030 {
        fixture.insert(origin - n * 60);
    }
    let response = guard::observe(
        &fixture.client,
        1,
        origin,
        cutoff,
        state.read_token.as_ref(),
        &AtomicBool::new(false),
    )
    .await
    .unwrap();
    assert_eq!(response.status, TailReadGuardStatus::Unknown);
    assert!(response.token.is_some());
    assert!(guard::proof(state.read_token.as_ref().unwrap(), &response).is_none());
    assert!(step(&fixture, &mut state, cutoff).await.is_none());
    assert!(state.needs_rebuild);
    let stopped = AtomicBool::new(true);
    let calls = fixture.feed.lock().unwrap().guards.len();
    let source = Source {
        client: &fixture.client,
        instrument_id: 1,
    };
    assert!(
        guard::historical_page(&fixture.client, 1, &mut state, &source, cutoff, &stopped)
            .await
            .is_err()
    );
    assert_eq!(
        fixture.feed.lock().unwrap().guards.len(),
        calls,
        "Stop must not initiate guard I/O"
    );
    state.restart(&[None]).unwrap();
    assert!(state.history.is_empty() && state.read_token.is_none());
    assert!(state.profiles.iter().all(|p| p.rolling.is_none()));
    assert_eq!(state.read_from, origin);
}

#[tokio::test]
async fn hot_prefetch_acknowledges_irrelevant_revision_and_routes_repair_to_cold_without_computation()
 {
    let now = chrono::Utc::now().timestamp().div_euclid(60) * 60;
    let origin = now - 2000 * 60;
    let cutoff = now - 600;
    let fixture = Fixture::new(bars(origin, 2000).into_iter().skip(1).collect()).await;
    let mut state = reconstructing(origin, cutoff);
    finish_reconstruction(&fixture, &mut state, now).await;
    step(&fixture, &mut state, now).await.unwrap();
    assert!(state.caught_up);
    let after = state.after;
    let algorithms = state
        .profiles
        .iter()
        .map(|p| p.algorithm.clone())
        .collect::<Vec<_>>();
    let mut runtime = Runtime::default();
    runtime
        .sync(vec![item(1)], &algorithms, usize::MAX)
        .unwrap();
    let (_, mut entry) = runtime.take(true).unwrap();
    entry.symbol = Some(state);
    runtime.put(1, entry);
    let stopped = AtomicBool::new(false);
    fixture.insert(origin - 60);
    prefetch(&mut runtime, &fixture.client, &stopped)
        .await
        .unwrap();
    let entry = runtime.entries.get_mut(&1).unwrap();
    assert_eq!(entry.symbol.as_ref().unwrap().read_token, Some(token(2)));
    assert_eq!(entry.symbol.as_ref().unwrap().after, after);
    assert!(entry.prefetched.is_none());
    assert!(!entry.symbol.as_ref().unwrap().needs_rebuild);
    entry.next_poll = Instant::now();
    fixture.insert(origin);
    prefetch(&mut runtime, &fixture.client, &stopped)
        .await
        .unwrap();
    let entry = runtime.entries.get(&1).unwrap();
    assert!(entry.symbol.as_ref().unwrap().needs_rebuild);
    assert_eq!(entry.symbol.as_ref().unwrap().after, after);
    assert!(entry.prefetched.is_none());
    assert!(runtime.take(false).is_none());
    assert_eq!(runtime.cold.iter().copied().collect::<Vec<_>>(), vec![1]);
    stopped.store(true, Ordering::SeqCst);
    assert!(
        prefetch(&mut runtime, &fixture.client, &stopped)
            .await
            .is_err()
    );
    runtime.clear();
    assert!(runtime.entries.is_empty() && runtime.ready.is_empty() && runtime.cold.is_empty());
}
