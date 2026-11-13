//! Journal observations are source snapshot boundaries, not distributed locks.
//! Keep the causal origin stable even when parsed baseline frames are evicted.
use super::*;

pub(super) fn proof(previous: &TailReadToken, response: &TailReadGuard) -> Option<TailReadToken> {
    response
        .validates(previous)
        .then(|| response.token.clone())
        .flatten()
}

pub(super) async fn observe(
    client: &MarketDataClient,
    id: i64,
    from: i64,
    to: i64,
    previous: Option<&TailReadToken>,
    stopped: &AtomicBool,
) -> Result<TailReadGuard, WorkerError> {
    if stopped.load(Ordering::SeqCst) {
        return Err(invalid("fixed live stopped"));
    }
    let now = chrono::Utc::now().timestamp().div_euclid(60) * 60;
    let response = tokio::time::timeout(
        READ_TIMEOUT,
        client.tail_batch_guarded(
            &TailBatchQuery {
                instrument_ids: vec![id],
                from_unix: now - 60,
                to_unix: now,
            },
            &[TailReadGuardQuery {
                instrument_id: id,
                from_unix: from,
                to_unix: to,
                previous: previous.cloned(),
            }],
        ),
    )
    .await
    .map_err(|_| invalid("fixed read-guard observation timed out"))?
    .map_err(detector)?;
    if stopped.load(Ordering::SeqCst) {
        return Err(invalid("fixed live stopped"));
    }
    let item = response
        .instruments
        .into_iter()
        .next()
        .ok_or_else(|| invalid("fixed read-guard instrument missing"))?;
    if item.status != "known" {
        return Err(invalid("fixed read-guard instrument not found"));
    }
    item.read_guard
        .ok_or_else(|| invalid("fixed source omitted requested read guard"))
}

/// The initial capture is intentionally retained across ALL reconstruction
/// pages. Forward inserts beyond the frozen reconstruction frontier do not
/// restart warmup; an overlap anywhere in the full causal prefix does.
pub(super) async fn historical_page<S: CandleSource>(
    client: &MarketDataClient,
    id: i64,
    symbol: &mut Symbol,
    source: &S,
    now: i64,
    stopped: &AtomicBool,
) -> Result<Option<Prefetched>, WorkerError> {
    if symbol.read_token.is_none() {
        let captured = observe(
            client,
            id,
            symbol.read_from,
            symbol.read_from,
            None,
            stopped,
        )
        .await?;
        let token = captured
            .token
            .ok_or_else(|| invalid("fixed source cannot capture repair journal token"))?;
        token.validate().map_err(detector)?;
        // previous:null capture is not a validity proof.
        symbol.read_token = Some(token);
    }
    let original = symbol
        .read_token
        .clone()
        .ok_or_else(|| invalid("fixed repair token missing"))?;
    let before = observe(
        client,
        id,
        symbol.read_from,
        symbol.guarded_to(),
        Some(&original),
        stopped,
    )
    .await?;
    let Some(before_token) = proof(&original, &before) else {
        symbol.invalidate();
        return Ok(None);
    };
    let base = if symbol.reconstruction_to.is_some() {
        original
    } else {
        before_token
    };
    let query = symbol.query(now);
    let page = tokio::time::timeout(READ_TIMEOUT, source.read(&query))
        .await
        .map_err(|_| invalid("fixed live page read timed out"))?
        .map_err(invalid)?;
    validate_page(&page, &query)?;
    // Only a later real input turns preceding missing minutes into consumed
    // interior gaps. EOF/trailing absence is still pending, not detector state.
    let through = page
        .bars
        .last()
        .map_or(symbol.consumed_to(), |b| b.open_time_unix + 60);
    let after = observe(
        client,
        id,
        symbol.read_from,
        through.max(symbol.guarded_to()),
        Some(&base),
        stopped,
    )
    .await?;
    let Some(token) = proof(&base, &after) else {
        symbol.invalidate();
        return Ok(None);
    };
    Ok(Some(Prefetched {
        page,
        token,
        observed_to: query.to_unix,
    }))
}

impl Symbol {
    pub(super) fn consumed_to(&self) -> i64 {
        self.after.map_or(self.read_from, |v| v + 60)
    }

    pub(super) fn guarded_to(&self) -> i64 {
        self.history
            .latest_position()
            .and_then(|p| self.history.open_time(p).ok())
            .map_or(self.consumed_to(), |v| (v + 60).max(self.consumed_to()))
    }

    pub(super) fn invalidate(&mut self) {
        self.needs_rebuild = true;
        self.caught_up = false;
    }

    /// Reset only after durable seeds were reloaded. Previously committed rows
    /// stay authoritative; all reconstructed raw results before this frozen
    /// detector frontier are warmup-only, never retroactive publications.
    pub(super) fn restart(&mut self, seeds: &[Option<i64>]) -> Result<(), WorkerError> {
        if seeds.len() != self.profiles.len() {
            return Err(invalid("fixed reconstruction seed count mismatch"));
        }
        let algorithms = self
            .profiles
            .iter()
            .map(|p| p.algorithm.clone())
            .collect::<Vec<_>>();
        let history =
            SharedHistory::new(self.history.identity(), limits(&algorithms)?).map_err(detector)?;
        let publication_from = self.consumed_to();
        for (profile, seed) in self.profiles.iter_mut().zip(seeds) {
            profile.emit_from = profile.emit_from.max(publication_from);
            profile.previous_setup = *seed;
            profile.rolling = None;
            profile.ready = false;
        }
        self.history = history;
        self.after = None;
        self.last_closed = None;
        self.read_token = None;
        self.reconstruction_to = self.profiles.iter().map(|p| p.emit_from).min();
        self.observed_to = self.read_from;
        self.caught_up = false;
        self.needs_rebuild = false;
        Ok(())
    }

    pub(super) fn checkpoint_changed(&self, batch: &Prepared) -> bool {
        batch.profiles.iter().any(|profile| {
            profile.checkpoint.as_ref() != Some(&checkpoint_value(profile, batch.after))
        })
    }
}
