use super::model::{Source, chart, first_signal_open};
use crate::replay::ReplayState;
use futures::FutureExt;
use serde_json::{Value, json};
use setups_analytics::{
    AnalyticsSetup, CausalCluster, OhlcCandle,
    live_outcomes::{TradeOutcome, TradeTerms, evaluate_with_horizon},
};
use setups_db::study_actual::{self as db, Job};
use setups_mdclient::{CandleDto, CandlesQuery, CandlesResponse, MarketDataClient, MdClientError};
use setups_replay::{ArrayCandleSource, CandleBarDto, ReplayRunOptions, replay_run_bounded};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    future::Future,
    panic::AssertUnwindSafe,
    sync::Arc,
    time::Duration,
};
use tokio_util::sync::CancellationToken;

const MAX_INPUT_BYTES: usize = 64 * 1024 * 1024;
const MAX_INSTRUMENT_OUTPUT: usize = 8 * 1024 * 1024;
const JOB_SECONDS: u64 = 600;
const MARKET_REQUEST_ATTEMPTS: usize = 4;
pub struct ActualHandle {
    stop: CancellationToken,
    task: Option<tokio::task::JoinHandle<()>>,
}
impl Drop for ActualHandle {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}
impl ActualHandle {
    pub async fn stop(mut self) {
        self.stop.cancel();
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

async fn connect_owner(
    state: &ReplayState,
) -> Result<Option<sqlx::PgConnection>, setups_db::DbError> {
    let mut owner = state.pool.acquire().await?.detach();
    sqlx::query("select set_config('application_name','study_actual_runtime',false),set_config('statement_timeout','5000',false)").execute(&mut owner).await?;
    let owns: bool = sqlx::query_scalar("select pg_try_advisory_lock($1)")
        .bind(db::RUNTIME_LOCK)
        .fetch_one(&mut owner)
        .await?;
    if !owns {
        return Ok(None);
    }
    // Recovery writes and lock ownership use the SAME session too. Losing it
    // rolls back its current mutation; no pool-based recovery can outlive it.
    db::recover_owned(&mut owner).await?;
    Ok(Some(owner))
}

async fn settle_until<F, Fut>(job: &Job, stop: &CancellationToken, mut attempt: F) -> bool
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<(), setups_db::DbError>>,
{
    let mut retries = 0u32;
    loop {
        match tokio::time::timeout(Duration::from_secs(5), attempt()).await {
            Ok(Ok(())) => {
                if retries > 0 {
                    eprintln!(
                        "study_actual event=settled study_id={} revision={} retries={retries}",
                        job.study, job.revision
                    );
                }
                return true;
            }
            _ => {
                if retries == 0 {
                    eprintln!(
                        "study_actual event=settlement_pending study_id={} revision={}",
                        job.study, job.revision
                    );
                }
            }
        }
        if stop.is_cancelled() {
            eprintln!(
                "study_actual event=settlement_deferred_to_restart study_id={} revision={}",
                job.study, job.revision
            );
            return false;
        }
        let backoff =
            Duration::from_millis(200u64.saturating_mul(1u64 << retries.min(5)).min(5000));
        retries = retries.saturating_add(1);
        tokio::select! {_=stop.cancelled()=>{},_=tokio::time::sleep(backoff)=>{}}
    }
}

async fn run_job(
    state: Arc<ReplayState>,
    job: Job,
    root: CancellationToken,
    epoch: CancellationToken,
) {
    let started = std::time::Instant::now();
    let cancel = setups_replay::CancelToken::new();
    eprintln!(
        "study_actual event=started study_id={} revision={}",
        job.study, job.revision
    );
    let outcome = {
        let compute = AssertUnwindSafe(compute(&state, &job, &cancel)).catch_unwind();
        tokio::select! {
            biased;
            _=root.cancelled()=>Err("study_actual_interrupted_by_shutdown"),
            _=epoch.cancelled()=>Err("study_actual_runtime_ownership_lost"),
            result=tokio::time::timeout(Duration::from_secs(JOB_SECONDS),compute)=>match result {
                Ok(Ok(result))=>result,Ok(Err(_))=>Err("study_actual_compute_panicked"),Err(_)=>Err("study_actual_deadline_exceeded"),
            }
        }
    }; // Dropped compute has no spawned CPU tasks; settlement cannot race it.
    cancel.cancel();
    if let Err(code) = outcome {
        let confirmed = settle_until(&job, &root, || db::fail(&state.pool, &job, code)).await;
        // A lost acknowledgement of completed publication remains completed;
        // settlement is confirmed without falsely logging that job as failed.
        eprintln!(
            "study_actual event={} study_id={} revision={} reason={code} duration_ms={}",
            if confirmed {
                "settlement_finished"
            } else {
                "settlement_deferred"
            },
            job.study,
            job.revision,
            started.elapsed().as_millis()
        );
    } else {
        eprintln!(
            "study_actual event=completed study_id={} revision={} duration_ms={}",
            job.study,
            job.revision,
            started.elapsed().as_millis()
        );
    }
}

async fn coordinator(
    state: Arc<ReplayState>,
    workers: usize,
    root: CancellationToken,
    mut owner: sqlx::PgConnection,
) {
    let mut bootstrap_at = tokio::time::Instant::now();
    let market = MarketDataClient::new(&state.market_url, &state.market_key);
    let mut market_ready = false;
    let mut market_retry_at = tokio::time::Instant::now();
    loop {
        let epoch = CancellationToken::new();
        let mut tasks = tokio::task::JoinSet::new();
        let mut preparations = tokio::task::JoinSet::new();
        // At most two retained jobs. Removed on join; never a persistent registry.
        let mut jobs: HashMap<tokio::task::Id, Job> = HashMap::new();
        let mut poll = tokio::time::interval(Duration::from_millis(500));
        poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                biased;
                _=root.cancelled()=>break,
                Some(result)=tasks.join_next_with_id(),if !tasks.is_empty()=>{
                    match result {Ok((id,()))=>{jobs.remove(&id);},Err(_)=>break}
                },
                _=preparations.join_next(),if !preparations.is_empty()=>{},
                _=poll.tick()=>{
                    // Check even when every local slot is occupied. A pool ping
                    // would not establish ownership of this advisory-lock session.
                    if !matches!(tokio::time::timeout(Duration::from_secs(2),db::owns_runtime(&mut owner)).await,Ok(Ok(true))) {break;}
                    if tasks.len()<workers.min(2) {
                        // Docker restart policies do not preserve Compose startup
                        // ordering. Do not claim a durable job until market-data
                        // actually serves requests after a host restart.
                        if !market_ready {
                            if tokio::time::Instant::now()<market_retry_at {continue;}
                            match tokio::time::timeout(Duration::from_secs(6),market.ready()).await {
                                Ok(Ok(true))=>{market_ready=true;eprintln!("study_actual event=market_data_ready");},
                                _=>{market_retry_at=tokio::time::Instant::now()+Duration::from_secs(1);continue;}
                            }
                        }
                        // Durable requests always claim before speculative source
                        // preparation. Slow DB/source reads cannot hold this loop.
                        match tokio::time::timeout(Duration::from_secs(5),db::claim_owned(&mut owner,workers as i64)).await {
                            Ok(Ok(Some(job)))=>{let task=tasks.spawn(run_job(state.clone(),job.clone(),root.clone(),epoch.clone()));jobs.insert(task.id(),job);},
                            Ok(Ok(None))=>{
                                if preparations.is_empty() {
                                    let bootstrap=tokio::time::Instant::now()>=bootstrap_at;
                                    if bootstrap {bootstrap_at=tokio::time::Instant::now()+Duration::from_secs(30);}
                                    let state=state.clone();let root=root.clone();let epoch=epoch.clone();
                                    preparations.spawn(async move {
                                        let prepare=async {
                                            if bootstrap {db::continuous::bootstrap(&state.pool).await?;}
                                            schedule_automatic(&state).await
                                        };
                                        tokio::select! {
                                            biased;
                                            _=root.cancelled()=>{},_=epoch.cancelled()=>{},
                                            result=tokio::time::timeout(Duration::from_secs(4),prepare)=>{
                                                if let Ok(Err(e))=result {eprintln!("study_actual event=automatic_admission_failed cause={e}");}
                                            }
                                        }
                                    });
                                }
                            },_=>break,
                        }
                    }
                }
            }
        }
        epoch.cancel();
        while preparations.join_next().await.is_some() {}
        while let Some(result) = tasks.join_next_with_id().await {
            if let Ok((id, ())) = result {
                jobs.remove(&id);
            }
        }
        // A panic outside the guarded compute still has an explicit job owner.
        for job in jobs.into_values() {
            settle_until(&job, &root, || {
                db::fail(&state.pool, &job, "study_actual_runtime_task_failed")
            })
            .await;
        }
        drop(owner);
        if root.is_cancelled() {
            return;
        }
        eprintln!("study_actual event=runtime_session_lost action=reacquire");
        // Same singleton lock, bounded attempts/backoff; never recover another
        // owner's work. No fresh claims before acquisition AND recovery succeed.
        let mut retries = 0u32;
        loop {
            let backoff =
                Duration::from_millis(200u64.saturating_mul(1u64 << retries.min(5)).min(5000));
            tokio::select! {_=root.cancelled()=>return,_=tokio::time::sleep(backoff)=>{}}
            let result = tokio::select! {_=root.cancelled()=>return,result=tokio::time::timeout(Duration::from_secs(10),connect_owner(&state))=>result};
            if let Ok(Ok(Some(connection))) = result {
                owner = connection;
                break;
            }
            retries = retries.saturating_add(1);
        }
        eprintln!("study_actual event=runtime_session_restored");
    }
}

/// Same source loader, immutable request and admission as manual. At most one
/// 16-candidate page is inspected per tick; never hold the market or a full circle.
async fn schedule_automatic(state: &ReplayState) -> Result<(), setups_db::DbError> {
    let Some(ticket) = db::continuous::next(&state.pool).await? else {
        return Ok(());
    };
    let mut reason = "no_eligible_studies";
    for candidate in &ticket.candidates {
        let source = match super::model::source(
            &state.pool,
            candidate.study,
            &ticket.settings.owner_id,
            ticket.cutoff,
        )
        .await
        {
            Ok(source) if source.execution_started_at == candidate.start => source,
            Ok(_) => {
                reason = "execution_start_changed";
                continue;
            }
            Err("study_actual_storage_unavailable") => {
                return Err(setups_db::DbError::Json(
                    "study_actual_storage_unavailable".into(),
                ));
            }
            Err(code) => {
                reason = code;
                continue;
            }
        };
        if source.work_minutes(ticket.cutoff) > state.limits.max_work_minutes {
            reason = "study_actual_candidate_work_limit";
            continue;
        }
        let value = json!(source);
        let job = format!("{:032x}", rand::random::<u128>());
        match db::continuous::admit(
            &state.pool,
            &ticket,
            db::Request {
                owner: &ticket.settings.owner_id,
                study: candidate.study,
                job: &job,
                start: source.execution_started_at,
                cutoff: ticket.cutoff,
                source: &value,
                identity: &source.source_identity,
                work: source.work_minutes(ticket.cutoff),
                limits: &state.limits,
            },
        )
        .await?
        {
            db::Admission::SourceChanged => {
                reason = "source_changed";
                continue;
            }
            db::Admission::Resource => {
                db::continuous::backoff(&state.pool, &ticket, "resource_limit").await?;
                return Ok(());
            }
            _ => return Ok(()),
        }
    }
    db::continuous::skipped(&state.pool, &ticket, reason).await
}

pub async fn start_actual_runtime(
    state: Arc<ReplayState>,
    workers: usize,
) -> Result<ActualHandle, String> {
    let stop = CancellationToken::new();
    let mut handle = ActualHandle {
        stop: stop.clone(),
        task: None,
    };
    if workers == 0 {
        return Ok(handle);
    }
    let owner = tokio::time::timeout(Duration::from_secs(10), connect_owner(&state))
        .await
        .map_err(|_| "actual_runtime_storage_timeout")?
        .map_err(|_| "actual_runtime_storage")?
        .ok_or("actual_runtime_already_owned")?;
    handle.task = Some(tokio::spawn(coordinator(state, workers, stop, owner)));
    Ok(handle)
}

async fn candles(
    client: &MarketDataClient,
    instrument: i64,
    from: i64,
    cutoff: i64,
) -> Result<(Vec<CandleDto>, String), &'static str> {
    let mut rows = Vec::new();
    let mut after = None;
    let mut bytes = 0;
    let mut hash = Sha256::new();
    for _ in 0..512 {
        let page = candle_page(
            client,
            &CandlesQuery {
                instrument_id: instrument,
                from_unix: from,
                to_unix: cutoff,
                after_unix: after,
                limit: Some(5000),
            },
        )
        .await?;
        if page.candles.is_empty() && page.next_cursor_unix.is_some() {
            return Err("study_actual_pagination_incomplete");
        }
        for row in page.candles {
            if row.open_time_unix < from
                || row.open_time_unix + 60 > cutoff
                || row.open_time_unix % 60 != 0
                || after.is_some_and(|last| row.open_time_unix <= last)
            {
                return Err("study_actual_pagination_nonforward");
            }
            let data = serde_json::to_vec(&row).map_err(|_| "study_actual_input_invalid")?;
            bytes += data.len() + std::mem::size_of::<CandleDto>();
            if bytes > MAX_INPUT_BYTES || rows.len() >= super::model::MAX_CANDLES as usize {
                return Err("study_actual_input_budget_exceeded");
            }
            hash.update(data);
            after = Some(row.open_time_unix);
            rows.push(row);
        }
        match page.next_cursor_unix {
            None => return Ok((rows, format!("{:x}", hash.finalize()))),
            Some(cursor) if Some(cursor) == after => {}
            _ => return Err("study_actual_pagination_incomplete"),
        }
    }
    Err("study_actual_read_budget_exceeded")
}

fn transient_market_error(error: &MdClientError) -> bool {
    match error {
        MdClientError::Network(_) => true,
        MdClientError::Http { status, .. } => matches!(*status, 408 | 425 | 429 | 500..=599),
        MdClientError::Decode(_) => false,
    }
}

fn market_retry_delay(attempt: usize) -> Duration {
    #[cfg(test)]
    {
        let _ = attempt;
        Duration::from_millis(1)
    }
    #[cfg(not(test))]
    {
        Duration::from_millis(250 * (1u64 << attempt.min(3)))
    }
}

async fn candle_page(
    client: &MarketDataClient,
    query: &CandlesQuery,
) -> Result<CandlesResponse, &'static str> {
    for attempt in 0..MARKET_REQUEST_ATTEMPTS {
        match client.candles(query, Duration::from_secs(30)).await {
            Ok(page) => {
                if attempt > 0 {
                    eprintln!("study_actual event=market_data_request_recovered retries={attempt}");
                }
                return Ok(page);
            }
            Err(error)
                if transient_market_error(&error) && attempt + 1 < MARKET_REQUEST_ATTEMPTS =>
            {
                tokio::time::sleep(market_retry_delay(attempt)).await;
            }
            Err(error) => {
                eprintln!(
                    "study_actual event=market_data_request_failed transient={} attempts={}",
                    transient_market_error(&error),
                    attempt + 1
                );
                return Err("study_actual_market_data_unavailable");
            }
        }
    }
    Err("study_actual_market_data_unavailable")
}

#[cfg(test)]
mod request_tests {
    use super::*;
    use axum::{Json, Router, http::StatusCode, response::IntoResponse, routing::get};
    use serde_json::json;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    async fn fixture(
        failures: usize,
    ) -> (
        MarketDataClient,
        Arc<AtomicUsize>,
        tokio::task::JoinHandle<()>,
    ) {
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        let app = Router::new().route("/v1/candles", get(move || {
            let count = count.clone();
            async move {
                let call = count.fetch_add(1, Ordering::SeqCst);
                if call < failures {
                    return (StatusCode::SERVICE_UNAVAILABLE, "not ready").into_response();
                }
                Json(json!({"candles":[{"openTimeUnix":60,"open":"1","high":"1","low":"1","close":"1","volume":"0","quoteTurnover":"0"}],"nextCursorUnix":null})).into_response()
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (
            MarketDataClient::new(format!("http://{address}"), "fixture"),
            calls,
            server,
        )
    }

    fn query() -> CandlesQuery {
        CandlesQuery {
            instrument_id: 1,
            from_unix: 60,
            to_unix: 120,
            after_unix: None,
            limit: Some(5000),
        }
    }

    #[tokio::test]
    async fn transient_market_failure_retries_then_returns_the_same_page() {
        let (client, calls, server) = fixture(1).await;
        let page = candle_page(&client, &query()).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(page.candles.len(), 1);
        assert_eq!(page.candles[0].open_time_unix, 60);
        server.abort();
    }

    #[tokio::test]
    async fn persistent_market_failure_exhausts_the_bounded_budget() {
        let (client, calls, server) = fixture(MARKET_REQUEST_ATTEMPTS).await;
        assert_eq!(
            candle_page(&client, &query()).await,
            Err("study_actual_market_data_unavailable")
        );
        assert_eq!(calls.load(Ordering::SeqCst), MARKET_REQUEST_ATTEMPTS);
        server.abort();
    }
}

fn ohlc(rows: &[CandleDto]) -> Result<Vec<OhlcCandle>, &'static str> {
    fn finite(s: &str) -> Result<f64, &'static str> {
        s.parse::<f64>()
            .ok()
            .filter(|v| v.is_finite() && *v > 0.0)
            .ok_or("study_actual_candle_invalid")
    }
    rows.iter()
        .map(|c| {
            let bar = OhlcCandle {
                open_time_unix: c.open_time_unix,
                open: finite(&c.open)?,
                high: finite(&c.high)?,
                low: finite(&c.low)?,
                close: finite(&c.close)?,
            };
            if bar.high < bar.open.max(bar.close)
                || bar.low > bar.open.min(bar.close)
                || bar.low > bar.high
            {
                return Err("study_actual_candle_invalid");
            }
            Ok(bar)
        })
        .collect()
}

pub(crate) fn emitted(
    source: &Source,
    all: &[AnalyticsSetup],
    cutoff: i64,
) -> Result<(Vec<AnalyticsSetup>, u64, u64), &'static str> {
    let in_scope = |s: &AnalyticsSetup| {
        s.close_time_unix <= cutoff
            && (s.close_time_unix > source.execution_started_at.timestamp()
                || s.close_time_unix == source.execution_started_at.timestamp()
                    && source.execution_started_at.timestamp_subsec_nanos() == 0)
    };
    let raw = all.iter().filter(|s| in_scope(s)).count() as u64;
    let selected = if !source.analytics.cluster_enabled {
        all.to_vec()
    } else if source.continuous || source.analytics.setup_maturity == Some(0.0) {
        let mut cluster = CausalCluster::new(
            source
                .analytics
                .cluster_gap_minutes
                .ok_or("selection_recipe_unavailable")?
                * 60,
            None,
        )
        .map_err(|_| "selection_recipe_invalid")?;
        let mut selected = Vec::new();
        // Pre-S raw context advances the SAME cluster seed; never restart at export boundary.
        for s in all {
            if cluster
                .observe_raw(s.close_time_unix)
                .map_err(|_| "selection_recipe_invalid")?
            {
                selected.push(s.clone());
            }
        }
        selected
    } else {
        setups_analytics::select_by_maturity(
            all,
            source
                .analytics
                .setup_maturity
                .ok_or("selection_recipe_unavailable")?,
            source
                .analytics
                .cluster_gap_minutes
                .ok_or("selection_recipe_unavailable")?
                * 60,
        )
        .rows
    };
    let selected: Vec<_> = selected.into_iter().filter(in_scope).collect();
    let excluded = raw
        .checked_sub(selected.len() as u64)
        .ok_or("study_actual_selection_counts_invalid")?;
    Ok((selected, raw, excluded))
}

pub(crate) fn evaluate(
    setup: &AnalyticsSetup,
    source: &Source,
    candles: &[OhlcCandle],
    cutoff: i64,
    at: i64,
) -> TradeOutcome {
    let mut trade = setup.clone();
    // Trade evaluator's open axis means entry anchor, not detector candle open.
    trade.open_time_unix = setup.close_time_unix;
    let closed = candles.partition_point(|c| c.open_time_unix.saturating_add(60) <= cutoff);
    evaluate_with_horizon(
        &trade,
        TradeTerms {
            direction: source.direction,
            tp_pct: source.tp_pct,
            sl_pct: source.sl_pct,
            time_shift_minutes: source.analytics.time_shift_minutes,
            price_change_pct: source.analytics.price_change_pct,
        },
        &candles[..closed],
        cutoff,
        at,
        source.analytics.horizon_minutes,
    )
}

#[derive(Default)]
struct Aggregate {
    total: u64,
    tp: u64,
    sl: u64,
    gate: u64,
    unclosed: u64,
    no_data: u64,
    sum: f64,
}
impl Aggregate {
    fn observe(&mut self, out: &TradeOutcome) -> Result<(), &'static str> {
        self.total += 1;
        match out.status.as_str() {
            "tp" | "sl" => {
                let r = out
                    .risk_return
                    .filter(|v| v.is_finite())
                    .ok_or("study_actual_closed_r_invariant")?;
                self.sum += r;
                if !self.sum.is_finite() {
                    return Err("study_actual_closed_r_invariant");
                }
                if out.status == "tp" {
                    self.tp += 1
                } else {
                    self.sl += 1
                }
            }
            "gate_failed" => self.gate += 1,
            "unclosed" => self.unclosed += 1,
            "no_data" => self.no_data += 1,
            _ => return Err("study_actual_status_invariant"),
        }
        Ok(())
    }
    fn value(&self) -> Value {
        let closed = self.tp + self.sl;
        json!({"total":self.total,"closed":closed,"tp":self.tp,"sl":self.sl,"tpCount":self.tp,"slCount":self.sl,"gateFailed":self.gate,"gateFailedCount":self.gate,"unclosed":self.unclosed,"unclosedCount":self.unclosed,"noData":self.no_data,"noDataCount":self.no_data,"gatePassed":closed+self.unclosed,"meanR":if closed>0{Some(self.sum/closed as f64)}else{None}})
    }
}

#[allow(clippy::too_many_arguments)]
async fn evaluate_selected(
    selected: Vec<AnalyticsSetup>,
    source: &Source,
    bars: &[OhlcCandle],
    job: &Job,
    aggregate: &mut Aggregate,
    days: &mut BTreeMap<i64, Aggregate>,
    cancel: &setups_replay::CancelToken,
) -> Result<Vec<Value>, &'static str> {
    let mut rows = Vec::with_capacity(selected.len());
    for (ordinal, s) in selected.into_iter().enumerate() {
        // A single scan is bounded by MAX_CANDLES. Yield between small groups
        // so deadline/ownership cancellation is polled during a large cohort.
        if ordinal % 16 == 0 {
            tokio::task::yield_now().await;
        }
        if cancel.is_cancelled() {
            return Err("study_actual_cancelled");
        }
        let outcome = evaluate(&s, source, bars, job.cutoff, chrono::Utc::now().timestamp());
        aggregate.observe(&outcome)?;
        let day = s.close_time_unix.div_euclid(86400) * 86400;
        days.entry(day).or_default().observe(&outcome)?;
        if days.len() > 3661 {
            return Err("study_actual_daily_budget_exceeded");
        }
        let mut row = serde_json::to_value(&outcome).map_err(|_| "study_actual_output_invalid")?;
        let object = row.as_object_mut().unwrap();
        for (key, value) in [
            ("symbol", json!(s.symbol)),
            ("exchange", json!(s.exchange_code)),
            ("category", json!(s.category)),
            ("studyId", json!(job.study)),
            ("slotId", Value::Null),
            ("direction", json!(source.direction)),
            ("setupTimeUnix", json!(s.close_time_unix)),
        ] {
            object.insert(key.into(), value);
        }
        rows.push(row);
    }
    Ok(rows)
}

async fn compute(
    state: &ReplayState,
    job: &Job,
    cancel: &setups_replay::CancelToken,
) -> Result<(), &'static str> {
    let source: Source =
        serde_json::from_value(job.source.clone()).map_err(|_| "study_actual_source_invalid")?;
    if source.source_identity != job.identity {
        return Err("study_actual_source_identity_invalid");
    }
    if source.params.algorithm_id() != source.algorithm_id
        || source.params.algorithm_version() != source.algorithm_version
        || crate::replay::validate_algorithm_params(&source.params).is_some()
    {
        return Err("study_actual_algorithm_version_unavailable");
    }
    let client = MarketDataClient::new(&state.market_url, &state.market_key);
    let catalog = client
        .instruments()
        .await
        .map_err(|_| "study_actual_market_data_unavailable")?;
    let batch = db::create_batch(&state.pool, job)
        .await
        .map_err(|_| "study_actual_storage_unavailable")?
        .ok_or("study_actual_fence_lost")?;
    let mut aggregate = Aggregate::default();
    let mut days: BTreeMap<i64, Aggregate> = BTreeMap::new();
    let mut manifest = Vec::new();
    let (mut raw_total, mut excluded_total) = (0, 0);
    let mut present_minutes = 0u64;
    let mut manifest_bytes = 0usize;
    for (index, instrument) in source.instruments.iter().enumerate() {
        let matches: Vec<_> = catalog
            .iter()
            .filter(|c| {
                c.exchange == instrument.exchange
                    && c.category == instrument.category
                    && c.symbol == instrument.symbol
            })
            .collect();
        if matches.len() != 1 {
            return Err("study_actual_instrument_unavailable");
        }
        let context_open = source.selection_from_open_unix;
        let warmup = context_open - i64::from(source.params.baseline_long_days()) * 86400;
        let read_from = warmup.min(
            first_signal_open(source.execution_started_at)
                + source.analytics.time_shift_minutes.min(0) * 60,
        );
        if !db::progress(&state.pool,job,"acquiring",&json!({"instrumentsTotal":source.instruments.len(),"instrumentsDone":index,"rawDetectedCount":raw_total,"clusterExcludedCount":excluded_total,"evaluatedSetups":aggregate.total})).await.map_err(|_|"study_actual_storage_unavailable")? {return Err("study_actual_fence_lost");}
        let (input, fingerprint) = if first_signal_open(source.execution_started_at) >= job.cutoff {
            (Vec::new(), format!("{:x}", Sha256::digest([])))
        } else {
            candles(&client, matches[0].id, read_from, job.cutoff).await?
        };
        let bars = ohlc(&input)?;
        present_minutes += input
            .iter()
            .filter(|b| {
                b.open_time_unix >= first_signal_open(source.execution_started_at)
                    && b.open_time_unix + 60 <= job.cutoff
            })
            .count() as u64;
        let data = ArrayCandleSource::new(
            input
                .iter()
                .map(|c| CandleBarDto {
                    open_time_unix: c.open_time_unix,
                    close: c.close.clone(),
                    volume: c.volume.clone(),
                    quote_turnover: c.quote_turnover.clone(),
                })
                .collect(),
        );
        let replay = replay_run_bounded(
            ReplayRunOptions {
                source: &data,
                from_unix: context_open,
                to_unix: job.cutoff,
                params: Some(source.params.clone()),
                last_setup_unix: None,
                warmup_unix: Some(warmup),
                cancel: Some(cancel),
            },
            50_000,
            MAX_INSTRUMENT_OUTPUT,
        )
        .await
        .map_err(|_| "study_actual_replay_failed")?;
        if replay.cancelled {
            return Err("study_actual_cancelled");
        }
        drop(data);
        let raw: Vec<_> = replay
            .setups
            .iter()
            .map(|s| AnalyticsSetup {
                exchange_code: instrument.exchange.clone(),
                category: instrument.category.clone(),
                symbol: instrument.symbol.clone(),
                open_time_unix: s.open_time_unix,
                close_time_unix: s.close_time_unix,
                close_price: s.close_price.clone(),
            })
            .collect();
        let (selected, raw_count, excluded) = emitted(&source, &raw, job.cutoff)?;
        raw_total += raw_count;
        excluded_total += excluded;
        let selected_times: BTreeSet<_> = selected.iter().map(|s| s.close_time_unix).collect();
        let exported: Vec<_> = replay
            .setups
            .iter()
            .filter(|s| selected_times.contains(&s.close_time_unix))
            .cloned()
            .collect();
        let rows = evaluate_selected(
            selected,
            &source,
            &bars,
            job,
            &mut aggregate,
            &mut days,
            cancel,
        )
        .await?;
        let emitted = json!(exported);
        let quality = json!(replay.quality);
        if serde_json::to_vec(&emitted)
            .map_err(|_| "study_actual_output_invalid")?
            .len()
            + serde_json::to_vec(&rows)
                .map_err(|_| "study_actual_output_invalid")?
                .len()
            > MAX_INSTRUMENT_OUTPUT
        {
            return Err("study_actual_output_budget_exceeded");
        }
        if !db::append(
            &state.pool,
            job,
            batch,
            db::InstrumentOutput {
                exchange: &instrument.exchange,
                category: &instrument.category,
                symbol: &instrument.symbol,
                emitted: &emitted,
                rows: &rows,
                quality: &quality,
            },
        )
        .await
        .map_err(|_| "study_actual_storage_or_output_budget")?
        {
            return Err("study_actual_fence_lost");
        }
        let item = json!({"exchange":instrument.exchange,"category":instrument.category,"symbol":instrument.symbol,"instrumentId":matches[0].id,"readFromUnix":read_from,"observedThroughUnix":job.cutoff,"capturedAt":chrono::Utc::now(),"inputSha256":fingerprint,"candles":input.len(),"quality":quality,"rawDetectedCount":raw_count,"clusterExcludedCount":excluded,"artifactSetupCount":rows.len()});
        manifest_bytes = manifest_bytes.saturating_add(
            serde_json::to_vec(&item)
                .map_err(|_| "study_actual_manifest_invalid")?
                .len(),
        );
        if manifest_bytes > 1024 * 1024 {
            return Err("study_actual_manifest_budget_exceeded");
        }
        manifest.push(item);
        tokio::task::yield_now().await;
    }
    let daily=days.into_iter().map(|(day,a)|json!({"dayUnix":day,"plotAtUnix":(day as f64).max(source.start_unix).min(job.cutoff as f64),"meanR":a.value()["meanR"],"closedCount":a.tp+a.sl,"tpCount":a.tp,"slCount":a.sl})).collect();
    let mut report = chart(&source, job.cutoff, daily);
    let expected_minutes = ((job.cutoff - first_signal_open(source.execution_started_at)).max(0)
        / 60) as u64
        * source.instruments.len() as u64;
    report["coverage"] = json!({"status":if present_minutes==0{"empty"}else if present_minutes<expected_minutes{"partial"}else{"complete"},"instrumentsExpected":source.instruments.len(),"instrumentsRead":source.instruments.len(),"expectedMinutes":expected_minutes,"presentMinutes":present_minutes,"missingMinutes":expected_minutes.saturating_sub(present_minutes),"pointInTimeMarketSnapshot":false});
    report["provenance"] = json!({"manifest":manifest,"rawDetectedCount":raw_total,"clusterExcludedCount":excluded_total,"artifactSetupCount":aggregate.total,"retrospective":true,"pointInTimeMarketSnapshot":false});
    let mut totals = aggregate.value();
    totals["rawDetectedCount"] = json!(raw_total);
    totals["clusterExcludedCount"] = json!(excluded_total);
    if !db::progress(&state.pool,job,"publishing",&json!({"instrumentsTotal":source.instruments.len(),"instrumentsDone":source.instruments.len(),"rawDetectedCount":raw_total,"clusterExcludedCount":excluded_total,"evaluatedSetups":aggregate.total})).await.map_err(|_|"study_actual_storage_unavailable")? {return Err("study_actual_fence_lost");}
    if !db::finish(&state.pool, job, &report, &totals)
        .await
        .map_err(|_| "study_actual_storage_unavailable")?
    {
        return Err("study_actual_fence_lost");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    pub(crate) fn source() -> Source {
        serde_json::from_value(json!({"algorithmId":"price_volume_spike","algorithmVersion":"1.1.1","params":setups_domain::AlgorithmParams::default(),
            "instruments":[{"exchange":"fixture","category":"linear","symbol":"FIX"}],"startUnix":0.0,"executionStartedAt":"1970-01-01T00:00:00Z","selectionFromOpenUnix":-60,
            "direction":"short","analytics":{"clusterEnabled":true,"clusterGapMinutes":1,"timeShiftMinutes":1,"priceChangePct":null,"setupMaturity":0.0,"horizonMinutes":10},
            "tpPct":10.0,"slPct":10.0,"continuous":true,"objective":{"version":1,"trendMode":"mean","trendBendDays":"linear","trendBlendLambda":0.5},
            "originalEndpoint":{},"sourceArtifactId":null,"artifact":null,"sourceIdentity":"fixture"})).unwrap()
    }
    fn setup(close: i64) -> AnalyticsSetup {
        AnalyticsSetup {
            exchange_code: "fixture".into(),
            category: "linear".into(),
            symbol: "FIX".into(),
            open_time_unix: close - 60,
            close_time_unix: close,
            close_price: "100".into(),
        }
    }
    #[test]
    fn trade_adapter_anchors_to_signal_close_and_does_not_mutate_detector_fields() {
        let raw = setup(120);
        let source = source();
        let bars = [OhlcCandle {
            open_time_unix: 180,
            open: 100.0,
            high: 101.0,
            low: 99.0,
            close: 100.0,
        }];
        let out = evaluate(&raw, &source, &bars, 240, 241);
        assert_eq!(out.entry_time_unix, Some(180));
        assert_eq!(out.entry_price, Some(100.0));
        assert_eq!(raw.open_time_unix, 60);
        assert_eq!(out.status, "unclosed");
        assert_eq!(
            evaluate(&raw, &source, &bars, 180, 181).entry_price,
            None,
            "entry candle open at cutoff is not closed"
        );
    }
    #[test]
    fn causal_cluster_keeps_pre_start_seed_and_filters_close_interval() {
        let mut source = source();
        source.start_unix = 150.5;
        source.execution_started_at = chrono::DateTime::from_timestamp(150, 500_000_000).unwrap();
        let (selected, raw, excluded) = emitted(
            &source,
            &[setup(120), setup(180), setup(360), setup(420)],
            400,
        )
        .unwrap();
        assert_eq!((raw, excluded), (2, 1));
        assert_eq!(
            selected
                .iter()
                .map(|s| s.close_time_unix)
                .collect::<Vec<_>>(),
            vec![360]
        );
    }
    #[test]
    fn manual_maturity_is_preserved_and_gate_is_not_a_selection_stage() {
        let mut source = source();
        source.continuous = false;
        source.analytics.setup_maturity = Some(0.5);
        source.start_unix = 150.5;
        source.execution_started_at = chrono::DateTime::from_timestamp(150, 500_000_000).unwrap();
        source.analytics.price_change_pct = Some(-1.0);
        let (selected, raw, excluded) =
            emitted(&source, &[setup(120), setup(180), setup(240)], 240).unwrap();
        assert_eq!((raw, excluded), (2, 1));
        assert_eq!(selected[0].close_time_unix, 180);
    }
    #[test]
    fn denominator_includes_every_emitted_status_without_a_fake_mean() {
        let source = source();
        let mut out = evaluate(&setup(120), &source, &[], 240, 241);
        let mut a = Aggregate::default();
        out.status = "gate_failed".into();
        for _ in 0..20 {
            a.observe(&out).unwrap();
        }
        out.status = "unclosed".into();
        for _ in 0..3 {
            a.observe(&out).unwrap();
        }
        assert_eq!(a.value()["total"], 23);
        assert_eq!(a.value()["closed"], 0);
        assert_eq!(a.value()["meanR"], Value::Null);
        out.status = "tp".into();
        assert!(
            a.observe(&out).is_err(),
            "closed without finite R must fail fast"
        );
    }
    #[test]
    fn no_original_chart_does_not_remove_factual_points() {
        let source = source();
        let report = chart(
            &source,
            240,
            vec![json!({"dayUnix":0,"plotAtUnix":source.start_unix,"meanR":1.0})],
        );
        assert_eq!(
            report["historical"]["availability"]["status"],
            "unavailable"
        );
        assert_eq!(
            report["projection"]["availability"]["status"],
            "unavailable"
        );
        assert_eq!(report["sourceArtifactId"], Value::Null);
        assert_eq!(report["actual"]["daily"][0]["meanR"], 1.0);
    }
    #[tokio::test(flavor = "current_thread")]
    async fn long_outcome_cohort_yields_to_cancellation_between_bounded_scans() {
        let mut source = source();
        source.analytics.horizon_minutes = 200_000;
        let bars: Vec<_> = (0..super::super::model::MAX_CANDLES)
            .map(|i| OhlcCandle {
                open_time_unix: 180 + i * 60,
                open: 100.0,
                high: 101.0,
                low: 99.0,
                close: 100.0,
            })
            .collect();
        let selected = (0..1000).map(|i| setup(120 + i * 60)).collect();
        let cancel = Arc::new(setups_replay::CancelToken::new());
        let trigger = cancel.clone();
        let task = tokio::spawn(async move {
            tokio::task::yield_now().await;
            trigger.cancel();
        });
        let job = Job {
            owner: "fixture".into(),
            study: 1,
            revision: 1,
            token: 1,
            source: Value::Null,
            identity: "fixture".into(),
            cutoff: 180 + 200_000 * 60,
            job_id: "fixture".into(),
        };
        let mut aggregate = Aggregate::default();
        let mut days = BTreeMap::new();
        let started = std::time::Instant::now();
        let result = evaluate_selected(
            selected,
            &source,
            &bars,
            &job,
            &mut aggregate,
            &mut days,
            &cancel,
        )
        .await;
        task.await.unwrap();
        assert_eq!(result.unwrap_err(), "study_actual_cancelled");
        assert!(aggregate.total <= 32);
        assert!(started.elapsed() < Duration::from_secs(2));
    }
}
