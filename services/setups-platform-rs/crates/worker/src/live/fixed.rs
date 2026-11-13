//! Fixed live owns one bounded parsed history per instrument, and two scalar
//! rolling profiles. Source and DB work are page-sized; cold symbols and current
//! symbols have separate round-robin queues and independently reserved workers.
use super::*;
use setups_detector::shared::{HistoryIdentity, HistoryLimits, SharedHistory, SharedProfile};
use setups_domain::DetectorBar;
use setups_mdclient::{
    CandlesQuery, TAIL_MAX_INSTRUMENTS, TailBatchQuery, TailReadGuard, TailReadGuardQuery,
    TailReadToken,
};
use setups_replay::{CandleBarDto, CandlePage, CandleSourceQuery};
use sqlx::{Postgres, Transaction};
use std::collections::{HashMap, VecDeque};
use std::time::Instant;

const PAGE_BARS: usize = 1000;
const MAX_SETUP_COMMITS: usize = 32;
const MAX_SYMBOLS: usize = 1000;
const IO_WORKERS: usize = 8;
const COLD_WORKERS: usize = 2;
const READ_TIMEOUT: Duration = Duration::from_secs(2);
const JOB_TIMEOUT: Duration = Duration::from_secs(8);
const SLICE_TIME: Duration = Duration::from_millis(100);
const POLL: Duration = Duration::from_secs(2);
const DEFAULT_HISTORY_BYTES: usize = 4 * 1024 * 1024 * 1024;

mod guard;

fn invalid(message: impl Into<String>) -> WorkerError {
    WorkerError::Source(message.into())
}
fn detector(error: impl std::fmt::Display) -> WorkerError {
    invalid(error.to_string())
}

/// All fixed instruments use one HTTP connection pool, not one client per bar,
/// profile or instrument. The source adapter itself retains no history.
struct Source<'a> {
    client: &'a MarketDataClient,
    instrument_id: i64,
}
impl CandleSource for Source<'_> {
    async fn read(&self, query: &CandleSourceQuery) -> Result<CandlePage, String> {
        let response = self
            .client
            .candles(
                &CandlesQuery {
                    instrument_id: self.instrument_id,
                    from_unix: query.from_unix,
                    to_unix: query.to_unix,
                    after_unix: query.after_unix,
                    limit: query.limit,
                },
                READ_TIMEOUT,
            )
            .await
            .map_err(|e| e.to_string())?;
        Ok(CandlePage {
            bars: response
                .candles
                .into_iter()
                .map(|b| CandleBarDto {
                    open_time_unix: b.open_time_unix,
                    close: b.close,
                    volume: b.volume,
                    quote_turnover: b.quote_turnover,
                })
                .collect(),
            next_cursor_unix: response.next_cursor_unix,
        })
    }
}

fn limits(algorithms: &[LiveAlgorithm]) -> Result<HistoryLimits, WorkerError> {
    let minutes = algorithms
        .iter()
        .map(|a| {
            (a.params.baseline_long_days() as usize * 1440)
                .max(a.params.window_minutes() as usize + 1)
        })
        .max()
        .unwrap_or(1440)
        + PAGE_BARS
        + 1;
    // Original close bytes have a separate real-byte cap. Overlong source data
    // fails explicitly; prices are never truncated to make admission fit.
    let close_bytes = minutes
        .checked_mul(24)
        .ok_or_else(|| invalid("fixed history budget overflow"))?;
    Ok(HistoryLimits {
        capacity_minutes: minutes,
        max_close_bytes: close_bytes,
        max_bytes: SharedHistory::required_bytes(minutes, close_bytes).map_err(detector)?,
    })
}

#[derive(Clone)]
struct Profile {
    algorithm: LiveAlgorithm,
    checkpoint: Option<Value>,
    emit_from: i64,
    warmup_from: i64,
    previous_setup: Option<i64>,
    rolling: Option<SharedProfile>,
    ready: bool,
}

struct Symbol {
    history: SharedHistory,
    capacity_minutes: usize,
    profiles: Vec<Profile>,
    // Only committed detector progress advances this source cursor. History may
    // contain a tentative page after DB failure; identical retries reuse it.
    after: Option<i64>,
    read_from: i64,
    caught_up: bool,
    last_closed: Option<i64>,
    // read_from is the stable INITIALIZATION/RECONSTRUCTION causal origin.
    // It never follows history eviction: cooldown can carry its influence.
    read_token: Option<TailReadToken>,
    // Query observation only. An absent trailing minute has not been consumed
    // by the detector and must remain eligible when its delayed final arrives.
    observed_to: i64,
    reconstruction_to: Option<i64>,
    needs_rebuild: bool,
}

struct Prepared {
    profiles: Vec<Profile>,
    setups: Vec<SpikeSetup>,
    after: Option<i64>,
    caught_up: bool,
    observed_to: i64,
    read_token: Option<TailReadToken>,
}

impl Symbol {
    async fn load(
        pool: &PgPool,
        item: &CatalogInstrument,
        algorithms: &[LiveAlgorithm],
        now: i64,
        initial_close: i64,
    ) -> Result<Self, WorkerError> {
        let mut profiles = Vec::with_capacity(algorithms.len());
        for algorithm in algorithms {
            let checkpoint = settings_get(pool, &checkpoint_key(item.id, algorithm)).await?;
            let matching = checkpoint
                .as_ref()
                .filter(|v| v["configHash"].as_str() == Some(&algorithm.config_hash));
            // Legacy consumedThroughUnix could describe queried trailing
            // absence. Only nextUnix is durable detector/publication progress.
            let emit_from = matching
                .and_then(|v| v["nextUnix"].as_i64())
                .unwrap_or(initial_close - 600);
            if emit_from % 60 != 0 || emit_from > now {
                return Err(invalid("invalid fixed live checkpoint minute"));
            }
            let default_warmup = emit_from
                .checked_sub(i64::from(algorithm.params.baseline_long_days()) * 86400)
                .ok_or_else(|| invalid("fixed live warmup overflow"))?;
            let warmup_from = match matching.and_then(|v| v.get("warmupFromUnix")) {
                Some(value) => value
                    .as_i64()
                    .filter(|v| *v >= 0 && *v % 60 == 0 && *v <= emit_from)
                    .ok_or_else(|| invalid("invalid fixed causal reconstruction origin"))?,
                None => default_warmup,
            };
            let previous_setup = last_live_setup_open_time(
                pool,
                &item.exchange,
                &item.category,
                &item.symbol,
                algorithm.params.algorithm_id(),
                algorithm.params.algorithm_version(),
                &serde_json::to_value(&algorithm.params)?,
            )
            .await?;
            profiles.push(Profile {
                algorithm: algorithm.clone(),
                checkpoint,
                emit_from,
                warmup_from,
                previous_setup,
                rolling: None,
                ready: false,
            });
        }
        let read_from = profiles.iter().map(|p| p.warmup_from).min().unwrap_or(now);
        let reconstruction_to = profiles.iter().map(|p| p.emit_from).min();
        let identity = HistoryIdentity(
            Sha256::digest(format!(
                "fixed/{}/{}/{}/{}",
                item.id, item.exchange, item.category, item.symbol
            ))
            .into(),
        );
        let history_limits = limits(algorithms)?;
        Ok(Self {
            capacity_minutes: history_limits.capacity_minutes,
            history: SharedHistory::new(identity, history_limits).map_err(detector)?,
            profiles,
            after: None,
            read_from,
            caught_up: false,
            last_closed: None,
            read_token: None,
            observed_to: read_from,
            reconstruction_to,
            needs_rebuild: false,
        })
    }

    fn query(&self, now: i64) -> CandleSourceQuery {
        CandleSourceQuery {
            from_unix: self.read_from,
            to_unix: self.reconstruction_to.unwrap_or(now),
            after_unix: self.after,
            limit: Some(PAGE_BARS as u32),
        }
    }

    fn prepare(
        &mut self,
        page: &CandlePage,
        now: i64,
        stopped: &AtomicBool,
    ) -> Result<Prepared, WorkerError> {
        validate_page(page, &self.query(now))?;
        let mut profiles = self.profiles.clone();
        let mut setups = Vec::new();
        let mut after = self.after;
        let started = Instant::now();
        let mut processed = 0;
        let committed_anchor = self
            .profiles
            .iter()
            .filter_map(|p| p.rolling.as_ref().map(SharedProfile::retention_anchor))
            .min();
        let mut committed_anchors_released = false;
        for bar in &page.bars {
            if stopped.load(Ordering::SeqCst) {
                return Err(invalid("fixed live stopped"));
            }
            if processed > 0 && started.elapsed() >= SLICE_TIME {
                break;
            }
            if !setups.is_empty() && setups.len() + profiles.len() > MAX_SETUP_COMMITS {
                break;
            }
            let staged_anchor = profiles
                .iter()
                .filter_map(|p| p.rolling.as_ref().map(SharedProfile::retention_anchor))
                .min();
            let bar = DetectorBar {
                open_time_unix: bar.open_time_unix,
                close: bar.close.clone(),
                quote_turnover: bar.quote_turnover.clone(),
            };
            let resets_all = self
                .history
                .latest_position()
                .and_then(|p| self.history.open_time(p).ok())
                .is_some_and(|last| {
                    bar.open_time_unix - last - 60
                        > profiles
                            .iter()
                            .map(|p| {
                                i64::from(p.algorithm.params.max_consecutive_gap_minutes()) * 60
                            })
                            .max()
                            .unwrap_or(0)
                });
            committed_anchors_released |= resets_all;
            let anchor = staged_anchor
                .into_iter()
                .chain(committed_anchor.filter(|_| !committed_anchors_released))
                .min();
            // A sparse page can span more calendar minutes than PAGE_BARS.
            // Stop the slice BEFORE evicting the committed rollback anchor;
            // resume at this real bar after the smaller slice commits.
            if !resets_all
                && processed > 0
                && anchor.is_some_and(|anchor| {
                    self.history
                        .position_for_open(bar.open_time_unix)
                        .is_ok_and(|position| {
                            anchor.0 < (position.0 + 1).saturating_sub(self.capacity_minutes as u64)
                        })
                })
            {
                break;
            }
            let position = if let Some(anchor) = anchor.filter(|_| !resets_all) {
                self.history.push_retaining(&bar, now, anchor)
            } else {
                self.history.push(&bar, now)
            }
            .map_err(detector)?;
            for profile in &mut profiles {
                if bar.open_time_unix < profile.warmup_from {
                    continue;
                }
                if profile.rolling.is_none() {
                    profile.rolling = Some(
                        SharedProfile::new(
                            profile.algorithm.params.clone(),
                            self.history.identity(),
                            position,
                            profile.previous_setup,
                        )
                        .map_err(detector)?,
                    );
                }
                let result = profile
                    .rolling
                    .as_mut()
                    .expect("initialized profile")
                    .push(&self.history, position)
                    .map_err(detector)?;
                profile.ready = result.status == PushStatus::Evaluated;
                if let Some(setup) = result.setup
                    && bar.open_time_unix >= profile.emit_from
                {
                    setups.push(setup);
                }
            }
            after = Some(bar.open_time_unix);
            processed += 1;
            if setups.len() >= MAX_SETUP_COMMITS {
                break;
            }
        }
        let complete_page = processed == page.bars.len();
        Ok(Prepared {
            profiles,
            setups,
            after,
            caught_up: complete_page && page.next_cursor_unix.is_none(),
            observed_to: if complete_page && page.next_cursor_unix.is_none() {
                self.query(now).to_unix
            } else {
                after.map_or(self.observed_to, |v| v + 60)
            }
            .max(self.observed_to),
            read_token: None,
        })
    }

    fn accept(&mut self, prepared: Prepared) {
        let was_reconstructing = self.reconstruction_to.is_some();
        let finished_reconstruction = was_reconstructing && prepared.caught_up;
        if (!was_reconstructing || finished_reconstruction)
            && let Some(token) = prepared.read_token
        {
            self.read_token = Some(token);
        }
        if finished_reconstruction {
            self.reconstruction_to = None;
        }
        self.observed_to = prepared.observed_to;
        self.profiles = prepared.profiles;
        self.after = prepared.after;
        self.caught_up = prepared.caught_up && !was_reconstructing;
        self.last_closed = self.after.map(|v| v + 60);
    }
}

fn validate_page(page: &CandlePage, query: &CandleSourceQuery) -> Result<(), WorkerError> {
    if page.bars.len() > query.limit.unwrap_or(PAGE_BARS as u32) as usize {
        return Err(WorkerError::PageLimitExceeded(page.bars.len()));
    }
    let mut previous = query.after_unix;
    for bar in &page.bars {
        if bar.open_time_unix % 60 != 0
            || bar.open_time_unix < query.from_unix
            || bar
                .open_time_unix
                .checked_add(60)
                .is_none_or(|v| v > query.to_unix)
            || previous.is_some_and(|p| bar.open_time_unix <= p)
            || bar.close.len() > 128
            || bar.quote_turnover.len() > 128
            || bar.volume.len() > 128
        {
            return Err(invalid(
                "invalid fixed live source page ordering/range/decimal byte budget",
            ));
        }
        previous = Some(bar.open_time_unix);
    }
    if page.next_cursor_unix.is_some()
        && page.next_cursor_unix != page.bars.last().map(|b| b.open_time_unix)
    {
        return Err(invalid("fixed live source cursor did not match page tail"));
    }
    Ok(())
}

fn checkpoint_key(id: i64, algorithm: &LiveAlgorithm) -> String {
    format!("live-checkpoint:{id}{}", algorithm.checkpoint_suffix)
}

fn checkpoint_value(profile: &Profile, after: Option<i64>) -> Value {
    let next = after
        .and_then(|v| v.checked_add(60))
        .unwrap_or(profile.emit_from)
        .max(
            profile
                .checkpoint
                .as_ref()
                .filter(|v| v["configHash"].as_str() == Some(&profile.algorithm.config_hash))
                .and_then(|v| v["nextUnix"].as_i64())
                .unwrap_or(profile.emit_from),
        );
    json!({"configHash":profile.algorithm.config_hash,"nextUnix":next,
        "warmupFromUnix":profile.warmup_from,"consumedThroughUnix":next,
        "reconstructionSeedOpenUnix":profile.previous_setup})
}

/// Setup+outbox writes own their DB transactions. Checkpoint/config guards are
/// checked before and after them, without holding a pool connection across a
/// nested pool call. Partial writes are retried idempotently; no failed/ambiguous
/// batch consumes source progress or cooldown in memory.
async fn persist(
    pool: &PgPool,
    item: &CatalogInstrument,
    batch: &mut Prepared,
    stopped: &AtomicBool,
) -> Result<(), WorkerError> {
    if batch.read_token.is_none() {
        return Err(invalid(
            "fixed persistence requires a validated source observation",
        ));
    }
    if stopped.load(Ordering::SeqCst) {
        return Err(invalid("fixed live stopped before persistence"));
    }
    let mut tx = pool.begin().await?;
    fence(&mut tx, item, batch).await?;
    tx.commit().await?;
    for setup in &batch.setups {
        if stopped.load(Ordering::SeqCst) {
            return Err(invalid("fixed live stopped before persistence"));
        }
        // None is deliberately NOT treated as proof of current permission: the
        // post-write settings/checkpoint fence below still has to succeed.
        setups_db::continuous::outbox::commit_fixed_setup(
            pool,
            &item.exchange,
            &item.category,
            &item.symbol,
            setup,
        )
        .await?;
    }
    let mut tx = pool.begin().await?;
    fence(&mut tx, item, batch).await?;
    for profile in &mut batch.profiles {
        {
            let value = checkpoint_value(profile, batch.after);
            let key = checkpoint_key(item.id, &profile.algorithm);
            if profile.checkpoint.is_none() {
                if sqlx::query("insert into settings(key,value,updated_at) values($1,$2,now()) on conflict(key) do nothing")
                    .bind(&key).bind(&value).execute(&mut *tx).await?.rows_affected() != 1 {
                    return Err(invalid("fixed live checkpoint creation CAS lost"));
                }
            } else {
                sqlx::query("update settings set value=$2,updated_at=now() where key=$1")
                    .bind(&key)
                    .bind(&value)
                    .execute(&mut *tx)
                    .await?;
            }
            profile.checkpoint = Some(value);
        }
    }
    if stopped.load(Ordering::SeqCst) {
        return Err(invalid("fixed live stopped before commit"));
    }
    tx.commit().await?;
    Ok(())
}

async fn fence(
    tx: &mut Transaction<'_, Postgres>,
    item: &CatalogInstrument,
    batch: &Prepared,
) -> Result<(), WorkerError> {
    sqlx::query("set local statement_timeout='2000ms'")
        .execute(&mut **tx)
        .await?;
    let enabled: Option<Value> =
        sqlx::query_scalar("select value from settings where key='live' for share")
            .fetch_optional(&mut **tx)
            .await?;
    if enabled.as_ref().and_then(|v| v.get("enabled")) != Some(&Value::Bool(true)) {
        return Err(invalid("fixed live disabled before commit"));
    }
    let selection: Option<Value> =
        sqlx::query_scalar("select value from settings where key='instrument-list:live' for share")
            .fetch_optional(&mut **tx)
            .await?;
    if selection.as_ref().is_some_and(|v| v["configured"] == true) {
        let selected: Option<i32> = sqlx::query_scalar("select id from instrument_lists where list_type='live' and exchange_code=$1 and category=$2 and symbol=$3 for share")
            .bind(&item.exchange).bind(&item.category).bind(&item.symbol).fetch_optional(&mut **tx).await?;
        if selected.is_none() {
            return Err(invalid("fixed live instrument deselected before commit"));
        }
    }
    for profile in &batch.profiles {
        let key = format!("algorithm:{}", profile.algorithm.params.algorithm_id());
        let current: Option<Value> =
            sqlx::query_scalar("select value from settings where key=$1 for share")
                .bind(key)
                .fetch_optional(&mut **tx)
                .await?;
        if current.as_ref().unwrap_or(&json!({}))
            != profile.algorithm.stored.as_deref().unwrap_or(&json!({}))
        {
            return Err(invalid("fixed live configuration changed before commit"));
        }
        let current: Option<Value> =
            sqlx::query_scalar("select value from settings where key=$1 for update")
                .bind(checkpoint_key(item.id, &profile.algorithm))
                .fetch_optional(&mut **tx)
                .await?;
        if current != profile.checkpoint {
            return Err(invalid("fixed live checkpoint CAS lost"));
        }
    }
    Ok(())
}

struct Entry {
    item: CatalogInstrument,
    symbol: Option<Symbol>,
    error: Option<String>,
    next_poll: Instant,
    // At most 30 raw bars/symbol, fetched once for BOTH fixed profiles. Cleared
    // on consumption/removal/Stop; network response bytes are capped by mdclient.
    prefetched: Option<Prefetched>,
}

struct Prefetched {
    page: CandlePage,
    token: TailReadToken,
    observed_to: i64,
}

#[derive(Default)]
struct Runtime {
    entries: HashMap<i64, Entry>,
    ready: VecDeque<i64>,
    cold: VecDeque<i64>,
    signature: Option<String>,
    admitted_symbols: usize,
    admitted_history_bytes: usize,
}

impl Runtime {
    fn clear(&mut self) {
        *self = Self::default();
    }

    fn sync(
        &mut self,
        instruments: Vec<CatalogInstrument>,
        algorithms: &[LiveAlgorithm],
        max_bytes: usize,
    ) -> Result<(), WorkerError> {
        if instruments.len() > MAX_SYMBOLS {
            return Err(invalid("select at most 1000 fixed live instruments"));
        }
        let budget = limits(algorithms)?
            .max_bytes
            .checked_mul(instruments.len())
            .ok_or_else(|| invalid("fixed history admission overflow"))?;
        if budget > max_bytes {
            return Err(invalid(format!(
                "fixed live history resource admission: required {budget} bytes, limit {max_bytes}; universe/baselines were not truncated"
            )));
        }
        let signature = algorithms
            .iter()
            .map(|a| a.config_hash.as_str())
            .collect::<Vec<_>>()
            .join("/");
        if self.signature.as_deref() != Some(&signature) {
            self.clear();
            self.signature = Some(signature);
        }
        let ids: HashSet<i64> = instruments.iter().map(|item| item.id).collect();
        if ids.len() != instruments.len()
            || instruments.iter().any(|v| {
                v.id <= 0
                    || v.id > i64::from(i32::MAX)
                    || [&v.exchange, &v.category, &v.symbol]
                        .iter()
                        .any(|s| s.is_empty() || s.len() > 128 || s.chars().any(char::is_control))
            })
        {
            return Err(invalid("invalid/duplicate fixed live instrument identity"));
        }
        self.entries.retain(|id, _| ids.contains(id));
        self.ready.retain(|id| ids.contains(id));
        self.cold.retain(|id| ids.contains(id));
        for item in instruments {
            if self
                .entries
                .get(&item.id)
                .is_some_and(|entry| entry.item != item)
            {
                self.entries.remove(&item.id);
                self.ready.retain(|id| *id != item.id);
                self.cold.retain(|id| *id != item.id);
            }
            if !self.entries.contains_key(&item.id) {
                self.cold.push_back(item.id);
                self.entries.insert(
                    item.id,
                    Entry {
                        item,
                        symbol: None,
                        error: None,
                        next_poll: Instant::now(),
                        prefetched: None,
                    },
                );
            }
        }
        // Names/metadata do not alter rolling arithmetic and must not cause an
        // 820-symbol baseline rewarm. Refresh their cheap persistence fences.
        for profile in self
            .entries
            .values_mut()
            .filter_map(|e| e.symbol.as_mut())
            .flat_map(|s| &mut s.profiles)
        {
            if let Some(algorithm) = algorithms
                .iter()
                .find(|a| a.config_hash == profile.algorithm.config_hash)
            {
                profile.algorithm = algorithm.clone();
            }
        }
        self.admitted_symbols = ids.len();
        self.admitted_history_bytes = budget;
        Ok(())
    }

    fn take(&mut self, cold: bool) -> Option<(i64, Entry)> {
        let queue = if cold {
            &mut self.cold
        } else {
            &mut self.ready
        };
        for _ in 0..queue.len() {
            let id = queue.pop_front()?;
            if self
                .entries
                .get(&id)
                .is_some_and(|e| e.next_poll > Instant::now() || (!cold && e.prefetched.is_none()))
            {
                queue.push_back(id);
                continue;
            }
            if let Some(entry) = self.entries.remove(&id) {
                return Some((id, entry));
            }
        }
        None
    }

    fn put(&mut self, id: i64, entry: Entry) {
        if entry.symbol.as_ref().is_some_and(|s| s.caught_up) {
            self.ready.push_back(id);
        } else {
            self.cold.push_back(id);
        }
        self.entries.insert(id, entry);
    }

    fn status(&self) -> Value {
        let states: Vec<Value> = self.entries.values().map(|entry| {
            let symbol = entry.symbol.as_ref();
            let stale = symbol.and_then(|s| s.last_closed).is_some_and(|v| chrono::Utc::now().timestamp()-v>120);
            let state = if entry.error.is_some() { "error" } else if symbol.is_none_or(|s| !s.caught_up) { "warming" }
                else if stale { "stale" } else if symbol.is_some_and(|s| s.profiles.iter().all(|p| p.ready)) { "ready" } else { "insufficient_history" };
            json!({"symbol":entry.item.symbol,"state":state,"lastClosedUnix":symbol.and_then(|s| s.last_closed),"error":entry.error})
        }).collect();
        let failed = states.iter().filter(|v| v["state"] == "error").count();
        let stale = states.iter().filter(|v| v["state"] == "stale").count();
        json!({"running":false,"lastScanAt":chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis,true),
            "lastError":if failed>0 {json!(format!("Не обработано инструментов: {failed}"))} else {Value::Null},
            "scanned":states.len(),"stale":stale,"instruments":states,
            "selected":self.admitted_symbols,"inFlight":self.admitted_symbols.saturating_sub(self.entries.len()),
            "admittedHistoryBytes":self.admitted_history_bytes,
            "historyBytesIdle":self.entries.values().filter_map(|e|e.symbol.as_ref()).map(|s|s.history.retained_bytes()).sum::<usize>()})
    }
}

async fn prefetch(
    runtime: &mut Runtime,
    client: &Arc<MarketDataClient>,
    stopped: &AtomicBool,
) -> Result<(), WorkerError> {
    if stopped.load(Ordering::SeqCst) {
        return Err(invalid("fixed live stopped"));
    }
    let target = chrono::Utc::now().timestamp().div_euclid(60) * 60;
    let mut ids = Vec::new();
    let mut cold = HashSet::new();
    for id in &runtime.ready {
        let Some(entry) = runtime.entries.get(id) else {
            continue;
        };
        if entry.prefetched.is_some() || entry.next_poll > Instant::now() {
            continue;
        }
        match entry.symbol.as_ref().and_then(|s| s.after) {
            Some(after) if after + 60 >= target - 1800 => ids.push(*id),
            _ => {
                cold.insert(*id);
            }
        }
    }
    runtime.ready.retain(|id| !cold.contains(id));
    for id in cold.iter().copied() {
        if let Some(entry) = runtime.entries.get_mut(&id) {
            if let Some(symbol) = &mut entry.symbol {
                symbol.caught_up = false;
            }
            runtime.cold.push_back(id);
        }
    }
    let mut reads = tokio::task::JoinSet::new();
    for ids in ids.chunks(TAIL_MAX_INSTRUMENTS) {
        let ids = ids.to_vec();
        let client = client.clone();
        let from = ids
            .iter()
            .filter_map(|id| {
                runtime
                    .entries
                    .get(id)?
                    .symbol
                    .as_ref()?
                    .after
                    .map(|v| v + 60)
            })
            .min()
            .unwrap_or(target - 60)
            .min(target - 60);
        let guards = ids
            .iter()
            .map(|id| {
                let symbol = runtime
                    .entries
                    .get(id)
                    .and_then(|e| e.symbol.as_ref())
                    .ok_or_else(|| invalid("fixed ready symbol missing"))?;
                Ok(TailReadGuardQuery {
                    instrument_id: *id,
                    from_unix: symbol.read_from,
                    to_unix: symbol.guarded_to(),
                    previous: Some(symbol.read_token.clone().ok_or_else(|| {
                        invalid("fixed ready symbol has no validated read token")
                    })?),
                })
            })
            .collect::<Result<Vec<_>, WorkerError>>()?;
        reads.spawn(async move {
            let result = tokio::time::timeout(
                READ_TIMEOUT,
                client.tail_batch_guarded(
                    &TailBatchQuery {
                        instrument_ids: ids.clone(),
                        from_unix: from,
                        to_unix: target,
                    },
                    &guards,
                ),
            )
            .await
            .map_err(|_| invalid("fixed live tail batch timed out"))
            .and_then(|v| v.map_err(detector));
            (ids, result)
        });
    }
    while let Some(result) = reads.join_next().await {
        if stopped.load(Ordering::SeqCst) {
            reads.abort_all();
            return Err(invalid("fixed live stopped"));
        }
        let (ids, result) = result.map_err(detector)?;
        match result {
            Ok(response) => {
                for item in response.instruments {
                    if let Some(entry) = runtime.entries.get_mut(&item.instrument_id) {
                        if item.status != "known" {
                            entry.error = Some("fixed live instrument missing from source".into());
                            continue;
                        }
                        let symbol = entry
                            .symbol
                            .as_mut()
                            .ok_or_else(|| invalid("fixed prefetched symbol disappeared"))?;
                        let previous = symbol
                            .read_token
                            .as_ref()
                            .ok_or_else(|| invalid("fixed prefetched read token disappeared"))?;
                        let proof = item
                            .read_guard
                            .as_ref()
                            .and_then(|response| guard::proof(previous, response));
                        let Some(token) = proof else {
                            symbol.invalidate();
                            entry.prefetched = None;
                            entry.error = None;
                            entry.next_poll = Instant::now();
                            cold.insert(item.instrument_id);
                            continue;
                        };
                        let after = symbol.after;
                        let bars = item
                            .candles
                            .into_iter()
                            .filter(|b| after.is_none_or(|after| b.candle.open_time_unix > after))
                            .map(|row| {
                                let b = row.candle;
                                CandleBarDto {
                                    open_time_unix: b.open_time_unix,
                                    close: b.close,
                                    volume: b.volume,
                                    quote_turnover: b.quote_turnover,
                                }
                            })
                            .collect::<Vec<_>>();
                        entry.error = None;
                        if bars.is_empty() {
                            // Same atomic observation certifies old state; no
                            // detector/checkpoint work is needed to acknowledge
                            // an irrelevant older backfill revision.
                            symbol.read_token = Some(token);
                            symbol.observed_to = symbol.observed_to.max(target);
                            entry.next_poll = Instant::now() + POLL;
                        } else {
                            entry.prefetched = Some(Prefetched {
                                page: CandlePage {
                                    bars,
                                    next_cursor_unix: None,
                                },
                                token,
                                observed_to: target,
                            });
                        }
                    }
                }
            }
            Err(error) => {
                for id in ids {
                    if let Some(entry) = runtime.entries.get_mut(&id) {
                        entry.error = Some(error.to_string().chars().take(240).collect());
                        entry.next_poll = Instant::now() + POLL;
                    }
                }
            }
        }
    }
    runtime.ready.retain(|id| !cold.contains(id));
    for id in cold {
        if runtime
            .entries
            .get(&id)
            .is_some_and(|e| e.symbol.as_ref().is_some_and(|s| s.needs_rebuild))
            && !runtime.cold.contains(&id)
        {
            runtime.cold.push_back(id);
        }
    }
    Ok(())
}

async fn work(
    pool: &PgPool,
    client: &MarketDataClient,
    entry: &mut Entry,
    algorithms: &[LiveAlgorithm],
    stopped: &AtomicBool,
) -> Result<(), WorkerError> {
    let now = chrono::Utc::now().timestamp().div_euclid(60) * 60;
    if stopped.load(Ordering::SeqCst) {
        return Err(invalid("fixed live stopped"));
    }
    let source = Source {
        client,
        instrument_id: entry.item.id,
    };
    if entry.symbol.is_none() {
        // Preserve the legacy initial/config-change ten-minute tail relative to
        // the latest CLOSED source bar, including a stalled source. This probe
        // occurs once per bootstrap, shared by BOTH algorithms, never per tick.
        let query = CandleSourceQuery {
            from_unix: now - 86400,
            to_unix: now,
            after_unix: None,
            limit: Some(1440),
        };
        let recent = source.read(&query).await.map_err(invalid)?;
        validate_page(&recent, &query)?;
        if recent.next_cursor_unix.is_some() {
            return Err(invalid(
                "fixed initial day probe was unexpectedly truncated",
            ));
        }
        let frontier = recent.bars.last().map_or(now, |b| b.open_time_unix + 60);
        entry.symbol = Some(Symbol::load(pool, &entry.item, algorithms, now, frontier).await?);
    }
    let symbol = entry.symbol.as_mut().expect("loaded symbol");
    if symbol.needs_rebuild {
        let mut seeds = Vec::with_capacity(symbol.profiles.len());
        for profile in &symbol.profiles {
            seeds.push(
                last_live_setup_open_time(
                    pool,
                    &entry.item.exchange,
                    &entry.item.category,
                    &entry.item.symbol,
                    profile.algorithm.params.algorithm_id(),
                    profile.algorithm.params.algorithm_version(),
                    &serde_json::to_value(&profile.algorithm.params)?,
                )
                .await?,
            );
        }
        symbol.restart(&seeds)?;
        entry.prefetched = None;
    }
    let reconstructing = symbol.reconstruction_to.is_some();
    let prefetched = if let Some(prefetched) = entry.prefetched.take() {
        if reconstructing {
            return Err(invalid("fixed reconstruction received a hot tail page"));
        }
        prefetched
    } else {
        let Some(prefetched) =
            guard::historical_page(client, entry.item.id, symbol, &source, now, stopped).await?
        else {
            entry.next_poll = Instant::now();
            return Ok(());
        };
        prefetched
    };
    let mut batch = symbol.prepare(&prefetched.page, prefetched.observed_to, stopped)?;
    batch.read_token = Some(prefetched.token.clone());
    // This certifies cached input bytes, not tentative detector/DB progress.
    // Retaining it on a DB failure avoids mistaking a previously proved normal
    // append in the cached overhang for a new repair on retry.
    if !reconstructing {
        symbol.read_token = Some(prefetched.token);
    }
    if stopped.load(Ordering::SeqCst) {
        return Err(invalid("fixed live stopped"));
    }
    // Empty current pages need no DB work, but must not advance to a wall-clock
    // cursor: a closed bar accepted later in the SAME minute remains readable.
    if (!reconstructing || batch.caught_up)
        && (symbol.checkpoint_changed(&batch) || !batch.setups.is_empty())
    {
        persist(pool, &entry.item, &mut batch, stopped).await?;
    }
    symbol.accept(batch);
    let caught_up = symbol.caught_up;
    entry.next_poll = Instant::now() + if caught_up { POLL } else { Duration::ZERO };
    Ok(())
}

pub(super) async fn run(
    pool: PgPool,
    url: String,
    key: String,
    stopped: Arc<AtomicBool>,
    notify: Arc<Notify>,
) {
    let max_bytes = match std::env::var("FIXED_LIVE_MAX_HISTORY_BYTES") {
        Ok(value) => {
            match value.parse::<usize>() {
                Ok(v) if (64 * 1024 * 1024..=64 * 1024 * 1024 * 1024).contains(&v) => v,
                _ => {
                    let _ = settings_set(&pool,"live-status",&json!({"running":false,"lastError":"invalid FIXED_LIVE_MAX_HISTORY_BYTES"})).await;
                    return;
                }
            }
        }
        Err(_) => DEFAULT_HISTORY_BYTES,
    };
    let mut runtime = Runtime::default();
    let client = Arc::new(MarketDataClient::new(&url, &key));
    let mut catalog = Vec::new();
    let mut catalog_at = Instant::now() - Duration::from_secs(60);
    let mut tasks = tokio::task::JoinSet::new();
    let mut active_cold = 0;
    let mut algorithms = Vec::new();
    let mut next_refresh = Instant::now();
    let mut plan_signature = String::new();
    while !stopped.load(Ordering::SeqCst) {
        if Instant::now() >= next_refresh {
            let mut global_error: Option<String> = None;
            let refresh = async {
                let enabled = settings_get(&pool, "live")
                    .await?
                    .as_ref()
                    .and_then(|v| v.get("enabled"))
                    == Some(&Value::Bool(true));
                if !enabled {
                    return Ok::<_, WorkerError>((Vec::new(), Vec::new()));
                }
                let algorithms = active_live_algorithms(&pool).await?;
                if catalog_at.elapsed() >= Duration::from_secs(30) {
                    catalog = client.instruments().await.map_err(detector)?;
                    if catalog.len() > 10000 {
                        return Err(invalid("fixed instrument catalog exceeds budget"));
                    }
                    catalog_at = Instant::now();
                }
                let configured = settings_get(&pool, "instrument-list:live")
                    .await?
                    .as_ref()
                    .and_then(|v| v.get("configured"))
                    == Some(&Value::Bool(true));
                let selected: HashSet<_> = instrument_list_by_kind(&pool, "live")
                    .await?
                    .into_iter()
                    .map(|v| (v.exchange_code, v.category, v.symbol))
                    .collect();
                let instruments: Vec<_> = catalog
                    .iter()
                    .filter(|v| {
                        v.status == "active"
                            && (!configured
                                || selected.contains(&(
                                    v.exchange.clone(),
                                    v.category.clone(),
                                    v.symbol.clone(),
                                )))
                    })
                    .cloned()
                    .collect();
                Ok((algorithms, instruments))
            }
            .await;
            match refresh {
                Ok((next_algorithms, instruments)) => {
                    let next_plan = json!([
                        next_algorithms
                            .iter()
                            .map(|a| json!([a.config_hash, a.stored.as_deref()]))
                            .collect::<Vec<_>>(),
                        instruments
                    ])
                    .to_string();
                    // Ordinary polling NEVER drains cold work: a slow bootstrap
                    // cannot hold fresh symbols behind its I/O timeout. Drain
                    // only for a real settings/universe ownership transition.
                    if next_plan != plan_signature {
                        while let Some(result) = tasks.join_next().await {
                            if let Ok((id, entry, _)) = result {
                                runtime.put(id, entry);
                            }
                        }
                        active_cold = 0;
                        if next_algorithms.is_empty() {
                            runtime.clear();
                            plan_signature = next_plan;
                        } else {
                            match runtime.sync(instruments, &next_algorithms, max_bytes) {
                                Ok(()) => plan_signature = next_plan,
                                Err(error) => {
                                    runtime.clear();
                                    global_error = Some(error.to_string());
                                }
                            }
                        }
                    }
                    algorithms = next_algorithms;
                    if let Err(error) = prefetch(&mut runtime, &client, &stopped).await {
                        global_error = Some(error.to_string());
                    }
                }
                Err(error) => {
                    global_error = Some(error.to_string().chars().take(300).collect::<String>());
                }
            }
            let mut status = runtime.status();
            if global_error.is_some() {
                status["lastError"] = serde_json::to_value(&global_error).unwrap_or(Value::Null);
            }
            let _ = settings_set(&pool, "live-status", &status).await;
            next_refresh = Instant::now() + POLL;
        }
        while tasks.len() < IO_WORKERS {
            let job = runtime.take(false).map(|v| (v, false)).or_else(|| {
                if active_cold < COLD_WORKERS {
                    runtime.take(true).map(|v| (v, true))
                } else {
                    None
                }
            });
            let Some(((id, mut entry), cold)) = job else {
                break;
            };
            if cold {
                active_cold += 1;
            }
            let pool = pool.clone();
            let client = client.clone();
            let algorithms = algorithms.clone();
            let stopped = stopped.clone();
            tasks.spawn(async move {
                let result = tokio::time::timeout(
                    JOB_TIMEOUT,
                    work(&pool, &client, &mut entry, &algorithms, &stopped),
                )
                .await
                .unwrap_or_else(|_| Err(invalid("fixed live bounded job timed out")));
                if result.as_ref().err().is_some_and(|e| {
                    let text = e.to_string();
                    text.contains("checkpoint CAS") || text.contains("creation CAS")
                }) {
                    entry.symbol = None;
                }
                entry.error = result
                    .err()
                    .map(|e| e.to_string().chars().take(240).collect());
                if entry.error.is_some() {
                    entry.next_poll = Instant::now() + POLL;
                }
                (id, entry, cold)
            });
        }
        tokio::select! {
            result=tasks.join_next(), if !tasks.is_empty() => {
                match result { Some(Ok((id,entry,cold)))=>{if cold {active_cold-=1;}runtime.put(id,entry);},
                    Some(Err(error))=>{
                        eprintln!("[fixed-live] bounded worker failed: {error}");
                        // Reconcile lost ownership explicitly on the next poll;
                        // never silently drop a symbol after a task panic.
                        plan_signature.clear();
                    }, _=>{} }
            },
            _=notify.notified()=>{},
            _=tokio::time::sleep(Duration::from_millis(20))=>{},
        }
    }
    // Abort/dropped transactions roll back; then release every retained symbol.
    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
    runtime.clear();
    let _ = settings_set(
        &pool,
        "live-status",
        &json!({"running":false,"lastScanAt":Value::Null,"lastError":Value::Null}),
    )
    .await;
}

#[cfg(test)]
mod tests;
