//! Bounded, caller-owned per-instrument parsed history and independent rolling
//! profiles. The reference detector remains the replay/optimizer engine.
//!
//! Positions count calendar minutes from the first admitted bar and never shift
//! on eviction. A profile consumes every real bar in order (holes are implicit,
//! NOT zero-volume candles). Warm profiles once by iterating `positions_from`;
//! keep only a cheap profile clone across speculative persistence/CAS. Callers
//! own source identity, frozen-prefix validation, publication and causal clusters.
use std::{collections::VecDeque, fmt, mem::size_of, sync::Arc};

use setups_domain::decimal::{Decimal, DecimalError, ZERO, add, compare_scaled, mul_by_i128, ratio, sub};
use setups_domain::{AlgorithmParams, DetectorBar, PushResult, PushStatus, SetupDetails, SkipReason, SpikeSetup};

const MINUTE: i64 = 60;
const HUNDRED: Decimal = Decimal { int: 100, scale: 0 };

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HistoryIdentity(pub [u8; 32]);

/// Absolute calendar offset, not a shifting deque index. Only meaningful with
/// the matching HistoryIdentity. `end_position()` is an exclusive boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct HistoryPosition(pub u64);

#[derive(Debug, Clone, Copy)]
pub struct HistoryLimits {
    /// Calendar retention, INCLUDING baseline departure anchors and catch-up
    /// headroom. A full N-minute profile needs N+1 slots to ingest its next bar
    /// before advancing. Batch ingestion requires correspondingly more headroom.
    pub capacity_minutes: usize,
    /// Total UTF-8 bytes for ORIGINAL close strings, not an assumed mean size.
    pub max_close_bytes: usize,
    /// Hard allocation ceiling, including column capacities and this container.
    pub max_bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SharedDetectorError {
    Decimal(DecimalError),
    Invalid(&'static str),
    Capacity { required: usize, limit: usize },
    Evicted { requested: HistoryPosition, oldest: HistoryPosition },
    Future(HistoryPosition),
    WrongHistory,
    Sequence { expected_previous: Option<HistoryPosition>, actual_previous: Option<HistoryPosition> },
}
impl From<DecimalError> for SharedDetectorError { fn from(error: DecimalError) -> Self { Self::Decimal(error) } }
impl fmt::Display for SharedDetectorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { write!(f, "shared detector: {self:?}") }
}
impl std::error::Error for SharedDetectorError {}

/// Struct-of-arrays avoids i128/Option<Decimal> padding and per-bar String/Arc
/// allocations. Only real bars occupy columns; calendar holes remain implicit.
/// All columns and the close byte ring have fixed, accounted allocation caps.
#[derive(Debug)]
pub struct SharedHistory {
    identity: HistoryIdentity,
    limits: HistoryLimits,
    origin: Option<i64>,
    oldest: HistoryPosition,
    end: HistoryPosition,
    last_real: Option<HistoryPosition>,
    evicted_real: Option<HistoryPosition>,
    positions: VecDeque<u64>,
    volumes: VecDeque<i128>,
    volume_scales: VecDeque<u8>,
    closes: VecDeque<i128>,
    close_scales: VecDeque<u8>,
    close_lengths: VecDeque<usize>,
    close_starts: VecDeque<u64>,
    close_bytes: VecDeque<u8>,
    bytes_start: u64,
    bytes_end: u64,
}

/// Borrowed, already parsed real bar. Reading/iterating frames never clones the
/// history or reparses input. Original close bytes are copied only on request or
/// when an actual setup is emitted.
#[derive(Debug, Clone, Copy)]
pub struct HistoryFrame<'a> {
    history: &'a SharedHistory,
    index: usize,
}
impl HistoryFrame<'_> {
    pub fn position(&self) -> HistoryPosition { HistoryPosition(self.history.positions[self.index]) }
    pub fn open_time_unix(&self) -> i64 { self.history.open_time(self.position()).expect("admitted frame time") }
    pub fn close(&self) -> Decimal { Decimal { int:self.history.closes[self.index], scale:self.history.close_scales[self.index] as u32 } }
    pub fn quote_turnover(&self) -> Decimal { Decimal { int:self.history.volumes[self.index], scale:self.history.volume_scales[self.index] as u32 } }
    pub fn original_close(&self) -> String { self.history.original_close(self.index) }
}

fn deque<T>(capacity: usize) -> Result<VecDeque<T>, SharedDetectorError> {
    let mut values = VecDeque::new();
    values.try_reserve_exact(capacity).map_err(|_| SharedDetectorError::Invalid("history allocation failed"))?;
    Ok(values)
}

impl SharedHistory {
    pub fn new(identity: HistoryIdentity, limits: HistoryLimits) -> Result<Self, SharedDetectorError> {
        if limits.capacity_minutes == 0 || limits.max_close_bytes == 0 { return Err(SharedDetectorError::Invalid("empty history budget")); }
        let required = Self::required_bytes(limits.capacity_minutes, limits.max_close_bytes)?;
        if required > limits.max_bytes { return Err(SharedDetectorError::Capacity { required, limit: limits.max_bytes }); }
        let history = Self {
            identity, limits, origin: None, oldest: HistoryPosition(0), end: HistoryPosition(0),
            last_real: None, evicted_real: None,
            positions: deque(limits.capacity_minutes)?, volumes: deque(limits.capacity_minutes)?,
            volume_scales: deque(limits.capacity_minutes)?, closes: deque(limits.capacity_minutes)?,
            close_scales: deque(limits.capacity_minutes)?, close_lengths: deque(limits.capacity_minutes)?,
            close_starts: deque(limits.capacity_minutes)?, close_bytes: deque(limits.max_close_bytes)?,
            bytes_start: 0, bytes_end: 0,
        };
        if history.retained_bytes() > limits.max_bytes {
            return Err(SharedDetectorError::Capacity { required: history.retained_bytes(), limit: limits.max_bytes });
        }
        Ok(history)
    }

    /// Exact requested allocation on this target (verified against real VecDeque
    /// capacities after allocation). Includes inline container, excludes allocator
    /// metadata/RSS; those are separately measured by the opt-in benchmark.
    pub fn required_bytes(minutes: usize, close_bytes: usize) -> Result<usize, SharedDetectorError> {
        let per_bar = size_of::<u64>() * 2 + size_of::<i128>() * 2 + size_of::<u8>() * 2 + size_of::<usize>();
        minutes.checked_mul(per_bar).and_then(|n| n.checked_add(close_bytes))
            .and_then(|n| n.checked_add(size_of::<Self>())).ok_or(SharedDetectorError::Invalid("history budget overflow"))
    }

    pub fn retained_bytes(&self) -> usize {
        size_of::<Self>() + self.positions.capacity()*size_of::<u64>()
            + self.volumes.capacity()*size_of::<i128>() + self.volume_scales.capacity()
            + self.closes.capacity()*size_of::<i128>() + self.close_scales.capacity()
            + self.close_lengths.capacity()*size_of::<usize>() + self.close_starts.capacity()*size_of::<u64>()
            + self.close_bytes.capacity()
    }
    pub fn identity(&self) -> HistoryIdentity { self.identity }
    pub fn oldest_position(&self) -> HistoryPosition { self.oldest }
    pub fn end_position(&self) -> HistoryPosition { self.end }
    pub fn latest_position(&self) -> Option<HistoryPosition> { self.positions.back().copied().map(HistoryPosition) }
    pub fn len(&self) -> usize { self.positions.len() }
    pub fn is_empty(&self) -> bool { self.positions.is_empty() }

    /// Map an open time to a stable position, including a future cursor. Reading
    /// that cursor still returns Future until the corresponding minute is ingested.
    pub fn position_for_open(&self, open: i64) -> Result<HistoryPosition, SharedDetectorError> {
        let delta=open.checked_sub(self.origin.ok_or(SharedDetectorError::Invalid("uninitialized history"))?)
            .ok_or(SharedDetectorError::Invalid("history time overflow"))?;
        if delta<0 || delta%MINUTE!=0 { return Err(SharedDetectorError::Invalid("open is outside history grid")); }
        Ok(HistoryPosition((delta/MINUTE) as u64))
    }

    /// None denotes an actual calendar hole INSIDE the retained range. Outside
    /// that range is always an explicit Evicted/Future error, never a fake hole.
    pub fn frame(&self, position: HistoryPosition) -> Result<Option<HistoryFrame<'_>>, SharedDetectorError> {
        Ok(self.index(position)?.map(|index|HistoryFrame{history:self,index}))
    }

    pub fn frames_from(&self, from: HistoryPosition) -> Result<impl Iterator<Item=HistoryFrame<'_>> + '_, SharedDetectorError> {
        if from<self.oldest {return Err(SharedDetectorError::Evicted{requested:from,oldest:self.oldest});}
        if from>self.end {return Err(SharedDetectorError::Future(from));}
        let start=self.positions.partition_point(|p|*p<from.0);
        Ok((start..self.positions.len()).map(|index|HistoryFrame{history:self,index}))
    }

    pub fn positions_from(&self, from: HistoryPosition) -> Result<impl Iterator<Item=HistoryPosition> + '_, SharedDetectorError> {
        if from < self.oldest { return Err(SharedDetectorError::Evicted { requested: from, oldest: self.oldest }); }
        if from > self.end { return Err(SharedDetectorError::Future(from)); }
        let start = self.positions.partition_point(|p| *p < from.0);
        Ok(self.positions.iter().skip(start).copied().map(HistoryPosition))
    }

    pub fn open_time(&self, position: HistoryPosition) -> Result<i64, SharedDetectorError> {
        let offset = i64::try_from(position.0).ok().and_then(|p| p.checked_mul(MINUTE))
            .ok_or(SharedDetectorError::Invalid("history time overflow"))?;
        self.origin.and_then(|origin| origin.checked_add(offset)).ok_or(SharedDetectorError::Invalid("uninitialized history"))
    }

    pub fn push(&mut self, bar: &DetectorBar, observation_cutoff: i64) -> Result<HistoryPosition, SharedDetectorError> {
        self.push_impl(bar, observation_cutoff, None)
    }

    /// Refuse ingestion atomically if it would evict any required profile anchor.
    /// Worker must budget headroom, advance/drain profiles or explicitly rebuild;
    /// this API never substitutes fabricated zeroes for an exhausted prefix.
    pub fn push_retaining(&mut self, bar: &DetectorBar, observation_cutoff: i64, anchor: HistoryPosition) -> Result<HistoryPosition, SharedDetectorError> {
        self.push_impl(bar, observation_cutoff, Some(anchor))
    }

    fn push_impl(&mut self, bar: &DetectorBar, observation_cutoff: i64, anchor: Option<HistoryPosition>) -> Result<HistoryPosition, SharedDetectorError> {
        if bar.open_time_unix.checked_add(MINUTE).is_none_or(|close| close > observation_cutoff) {
            return Err(SharedDetectorError::Invalid("bar is not closed at observation cutoff"));
        }
        let origin = self.origin.unwrap_or(bar.open_time_unix);
        let delta = bar.open_time_unix.checked_sub(origin).ok_or(SharedDetectorError::Invalid("history time overflow"))?;
        if delta < 0 || delta%MINUTE!=0 { return Err(SharedDetectorError::Invalid("bar is outside history minute grid")); }
        let position = HistoryPosition((delta/MINUTE) as u64);
        if self.last_real.is_some_and(|last| position <= last) {
            // Never repair a previously consumed implicit None in-place: future
            // rolling subtraction would remove a value that was never added.
            let index=self.index(position)?.ok_or(SharedDetectorError::Invalid("older gap repair requires history rebuild"))?;
            let close=Decimal::parse(&bar.close)?;
            let volume=Decimal::parse(&bar.quote_turnover)?;
            if close== (Decimal{int:self.closes[index],scale:self.close_scales[index] as u32})
                && volume== (Decimal{int:self.volumes[index],scale:self.volume_scales[index] as u32})
                && self.original_close(index)==bar.close {
                return Ok(position); // identical retained duplicate: no mutation
            }
            return Err(SharedDetectorError::Invalid("conflicting history bar requires history rebuild"));
        }
        let end = HistoryPosition(position.0.checked_add(1).ok_or(SharedDetectorError::Invalid("history position overflow"))?);
        let oldest = HistoryPosition(end.0.saturating_sub(self.limits.capacity_minutes as u64).max(self.oldest.0));
        if let Some(anchor) = anchor {
            if anchor < oldest { return Err(SharedDetectorError::Evicted { requested: anchor, oldest }); }
        }
        // Parse and validate all input BEFORE mutation; one parse serves all profiles.
        let close = Decimal::parse(&bar.close)?;
        let volume = Decimal::parse(&bar.quote_turnover)?;
        let remove = self.positions.partition_point(|p| *p < oldest.0);
        let remove_bytes = self.close_lengths.iter().take(remove).sum::<usize>();
        let wanted_bytes = self.close_bytes.len().checked_sub(remove_bytes).and_then(|n| n.checked_add(bar.close.len()))
            .ok_or(SharedDetectorError::Invalid("history close bytes overflow"))?;
        if wanted_bytes > self.limits.max_close_bytes {
            return Err(SharedDetectorError::Capacity { required: wanted_bytes, limit: self.limits.max_close_bytes });
        }
        let bytes_end = self.bytes_end.checked_add(bar.close.len() as u64).ok_or(SharedDetectorError::Invalid("history byte cursor overflow"))?;
        for _ in 0..remove {
            self.evicted_real = self.positions.pop_front().map(HistoryPosition);
            self.volumes.pop_front(); self.volume_scales.pop_front(); self.closes.pop_front(); self.close_scales.pop_front();
            let bytes = self.close_lengths.pop_front().expect("parallel history columns");
            self.close_starts.pop_front();
            self.close_bytes.drain(..bytes);
            self.bytes_start += bytes as u64;
        }
        self.positions.push_back(position.0); self.volumes.push_back(volume.int); self.volume_scales.push_back(volume.scale as u8);
        self.closes.push_back(close.int); self.close_scales.push_back(close.scale as u8);
        self.close_lengths.push_back(bar.close.len()); self.close_starts.push_back(self.bytes_end);
        self.close_bytes.extend(bar.close.bytes()); self.bytes_end = bytes_end;
        self.origin = Some(origin); self.oldest = oldest; self.end = end; self.last_real = Some(position);
        Ok(position)
    }

    fn check(&self, position: HistoryPosition) -> Result<(), SharedDetectorError> {
        if position < self.oldest { return Err(SharedDetectorError::Evicted { requested: position, oldest: self.oldest }); }
        if position >= self.end { return Err(SharedDetectorError::Future(position)); }
        Ok(())
    }
    fn index(&self, position: HistoryPosition) -> Result<Option<usize>, SharedDetectorError> {
        self.check(position)?;
        Ok(self.positions.binary_search(&position.0).ok())
    }
    fn volume(&self, position: HistoryPosition) -> Result<Option<Decimal>, SharedDetectorError> {
        Ok(self.index(position)?.map(|i| Decimal { int: self.volumes[i], scale: self.volume_scales[i] as u32 }))
    }
    fn close(&self, position: HistoryPosition) -> Result<Option<Decimal>, SharedDetectorError> {
        Ok(self.index(position)?.map(|i| Decimal { int: self.closes[i], scale: self.close_scales[i] as u32 }))
    }
    fn original_close(&self, index: usize) -> String {
        let start = (self.close_starts[index] - self.bytes_start) as usize;
        let bytes: Vec<u8> = self.close_bytes.iter().skip(start).take(self.close_lengths[index]).copied().collect();
        String::from_utf8(bytes).expect("original UTF-8 close")
    }
    fn previous_real(&self, index: usize) -> Option<HistoryPosition> {
        if index == 0 { self.evicted_real } else { Some(HistoryPosition(self.positions[index-1])) }
    }
}

#[derive(Debug)]
struct ProfileConfig {
    params: AlgorithmParams,
    long: usize,
    short: usize,
    window: usize,
    recent: usize,
    max_gap: u64,
    max_missing: usize,
    cooldown_seconds: i64,
    price_numerator: i128,
    price_denominator: i128,
    multiplier: i128,
    multiplier_denominator: i128,
    price_drop: bool,
}

/// Clone is O(1): immutable parsed parameter configuration is Arc-shared, and
/// mutable state contains only scalars. There is NO baseline/window collection.
#[derive(Debug, Clone)]
pub struct SharedProfile {
    config: Arc<ProfileConfig>,
    identity: HistoryIdentity,
    start: HistoryPosition,
    previous: Option<HistoryPosition>,
    series_start: Option<HistoryPosition>,
    length: usize,
    missing: usize,
    last_missing: Option<HistoryPosition>,
    sum_window: Decimal,
    sum_short: Decimal,
    sum_long: Decimal,
    last_setup_time: Option<i64>,
}

impl SharedProfile {
    pub fn new(params: AlgorithmParams, identity: HistoryIdentity, start: HistoryPosition, last_raw_open_unix: Option<i64>) -> Result<Self, SharedDetectorError> {
        let long = params.baseline_long_days() as usize * 1440;
        let short = params.baseline_short_days() as usize * 1440;
        let window = params.window_minutes() as usize;
        if !(1440..=90*1440).contains(&long) || short == 0 || short > long || !(1..=1440).contains(&window)
            || params.max_consecutive_gap_minutes()>1440 || params.required_recent_minutes()>1440 {
            return Err(SharedDetectorError::Invalid("profile exceeds supported detector parameter bounds"));
        }
        let pct = Decimal::parse(params.price_pct())?;
        let gap = Decimal::parse(params.max_gap_percent())?;
        let multiplier = Decimal::parse(params.volume_multiplier())?;
        let price_drop = matches!(params, AlgorithmParams::SpikeDrop(_));
        let bound = if price_drop { sub(HUNDRED,pct)? } else { add(HUNDRED,pct)? };
        let gap_divisor = 100i128.checked_mul(10i128.pow(gap.scale)).ok_or(DecimalError::Overflow("gap percent divisor"))?;
        if gap.int < 0 || compare_scaled(gap,1,HUNDRED,1)?.is_gt() || pct.int < 0 || multiplier.int <= 0 {
            return Err(SharedDetectorError::Invalid("invalid profile decimal parameters"));
        }
        let max_missing = (long as i128).checked_mul(gap.int).ok_or(DecimalError::Overflow("gap percent"))? / gap_divisor;
        let config = ProfileConfig {
            long, short, window, recent: params.required_recent_minutes() as usize,
            max_gap: u64::from(params.max_consecutive_gap_minutes()), max_missing: max_missing as usize,
            cooldown_seconds: i64::from(params.cooldown_minutes())*MINUTE,
            price_numerator: bound.int,
            price_denominator: 100i128.checked_mul(10i128.pow(bound.scale)).ok_or(DecimalError::Overflow("price denominator"))?,
            multiplier: multiplier.int, multiplier_denominator: 10i128.pow(multiplier.scale), price_drop, params,
        };
        Ok(Self { config: Arc::new(config), identity, start, previous: None, series_start: None,
            length: 0, missing: 0, last_missing: None, sum_window: ZERO, sum_short: ZERO,
            sum_long: ZERO, last_setup_time: last_raw_open_unix })
    }

    pub fn last_position(&self) -> Option<HistoryPosition> { self.previous }
    pub fn last_raw_open_unix(&self) -> Option<i64> { self.last_setup_time }
    /// Earliest calendar position needed BEFORE ingestion of the next real bar.
    /// Keep this across speculative clones/CAS until the committed state advances.
    pub fn retention_anchor(&self) -> HistoryPosition {
        match (self.previous,self.series_start) {
            (Some(previous),Some(start)) => HistoryPosition(previous.0.saturating_add(1).saturating_sub(self.config.long.max(self.config.window+1) as u64).max(start.0)),
            _ => self.start,
        }
    }
    /// Conservative per-profile count: includes the shared config and its strings
    /// for EACH profile (thus overcounts clones rather than undercounting them).
    pub fn retained_bytes(&self) -> usize {
        let strings = match &self.config.params {
            AlgorithmParams::Spike(p) => p.price_growth_pct.capacity()+p.volume_multiplier.capacity()+p.max_gap_percent.capacity(),
            AlgorithmParams::SpikeDrop(p) => p.price_drop_pct.capacity()+p.volume_multiplier.capacity()+p.max_gap_percent.capacity(),
        };
        size_of::<Self>() + size_of::<ProfileConfig>() + 2*size_of::<usize>() + strings
    }

    pub fn push(&mut self, history: &SharedHistory, position: HistoryPosition) -> Result<PushResult, SharedDetectorError> {
        if history.identity != self.identity { return Err(SharedDetectorError::WrongHistory); }
        if self.previous.is_none() && self.start<history.oldest {
            return Err(SharedDetectorError::Evicted{requested:self.start,oldest:history.oldest});
        }
        if position < self.start { return Err(SharedDetectorError::Invalid("profile bar precedes bootstrap origin")); }
        let index = history.index(position)?.ok_or(SharedDetectorError::Invalid("profile push requires a real bar"))?;
        let actual_previous = history.previous_real(index);
        if let Some(previous) = self.previous {
            if actual_previous != Some(previous) { return Err(SharedDetectorError::Sequence { expected_previous: Some(previous), actual_previous }); }
        } else if actual_previous.is_some_and(|previous| previous >= self.start) {
            return Err(SharedDetectorError::Sequence { expected_previous: None, actual_previous });
        }
        // Stage only scalar state. An exhausted anchor/arithmetic error must not
        // corrupt even a caller that forgets its own speculative clone boundary.
        let mut next = self.clone();
        let result = next.advance(history,position,index)?;
        *self = next;
        Ok(result)
    }

    fn advance(&mut self, history: &SharedHistory, position: HistoryPosition, index: usize) -> Result<PushResult, SharedDetectorError> {
        if let Some(previous) = self.previous {
            let gap = position.0.checked_sub(previous.0+1).ok_or(SharedDetectorError::Invalid("profile position did not advance"))?;
            if gap > 0 {
                if gap > self.config.max_gap { self.reset(); }
                else {
                    for p in previous.0+1..position.0 { self.push_entry(history,HistoryPosition(p),None)?; }
                    if self.missing > self.config.max_missing { self.reset(); }
                }
            }
        }
        self.previous = Some(position);
        self.push_entry(history,position,Some(Decimal { int: history.volumes[index], scale: history.volume_scales[index] as u32 }))?;
        if self.length < self.config.long {
            return Ok(PushResult { status:PushStatus::Warmup,continuous_minutes:self.length,setup:None,
                skip_reason:Some(SkipReason::InsufficientWarmup),missing_minutes:None });
        }
        if self.config.recent > 0 && self.last_missing.is_some_and(|missing| missing.0 >= position.0+1-self.config.recent.min(self.length) as u64) {
            return Ok(PushResult { status:PushStatus::Warmup,continuous_minutes:self.length,setup:None,
                skip_reason:Some(SkipReason::RecentGap),missing_minutes:None });
        }
        let (setup,skip_reason) = self.evaluate(history,position,index)?;
        Ok(PushResult { status:PushStatus::Evaluated,continuous_minutes:self.length,setup,skip_reason,missing_minutes:Some(self.missing as u32) })
    }

    fn push_entry(&mut self, history: &SharedHistory, position: HistoryPosition, volume: Option<Decimal>) -> Result<(),SharedDetectorError> {
        let start = *self.series_start.get_or_insert(position);
        let len = self.length+1;
        let volume = match volume { Some(v)=>v, None=>{self.missing+=1;self.last_missing=Some(position);ZERO} };
        self.sum_window=add(self.sum_window,volume)?;
        self.sum_short=add(self.sum_short,volume)?;
        self.sum_long=add(self.sum_long,volume)?;
        if len>self.config.short {
            self.sum_short=sub(self.sum_short,history.volume(HistoryPosition(position.0-self.config.short as u64))?.unwrap_or(ZERO))?;
        }
        if len>self.config.window {
            self.sum_window=sub(self.sum_window,history.volume(HistoryPosition(position.0-self.config.window as u64))?.unwrap_or(ZERO))?;
        }
        if len>self.config.long {
            let leaving=history.volume(HistoryPosition(position.0-self.config.long as u64))?;
            if leaving.is_none() {self.missing-=1;}
            self.sum_long=sub(self.sum_long,leaving.unwrap_or(ZERO))?;
        }
        debug_assert!(position>=start);
        self.length=len.min(self.config.long);
        Ok(())
    }

    fn reset(&mut self) {
        self.series_start=None;self.length=0;self.missing=0;self.last_missing=None;
        self.sum_window=ZERO;self.sum_short=ZERO;self.sum_long=ZERO;
    }

    fn evaluate(&mut self, history:&SharedHistory, position:HistoryPosition,index:usize) -> Result<(Option<SpikeSetup>,Option<SkipReason>),SharedDetectorError> {
        if self.missing>self.config.max_missing {return Ok((None,Some(SkipReason::GapToleranceExceeded)));}
        let Some(target) = position.0.checked_sub(self.config.window as u64).map(HistoryPosition) else {return Ok((None,Some(SkipReason::PriceReferenceMissing)));};
        // Reference closes are reset locally, never by another profile's gaps.
        if self.series_start.is_none_or(|start|target<start) {return Ok((None,Some(SkipReason::PriceReferenceMissing)));}
        let Some(previous_close)=history.close(target)? else {return Ok((None,Some(SkipReason::PriceReferenceMissing)));};
        let current_close=Decimal {int:history.closes[index],scale:history.close_scales[index] as u32};
        let ordering=compare_scaled(current_close,self.config.price_denominator,previous_close,self.config.price_numerator)?;
        if if self.config.price_drop {ordering.is_gt()} else {ordering.is_lt()} {return Ok((None,None));}
        let window=self.config.window as i128;
        let long=self.config.long as i128;
        let short=self.config.short as i128;
        let over_long=compare_scaled(self.sum_window,long.checked_mul(self.config.multiplier_denominator).ok_or(DecimalError::Overflow("long factor"))?,
            mul_by_i128(self.sum_long,self.config.multiplier.checked_mul(window).ok_or(DecimalError::Overflow("window factor"))?)?,1)?.is_gt();
        let over_short=compare_scaled(self.sum_window,short.checked_mul(self.config.multiplier_denominator).ok_or(DecimalError::Overflow("short factor"))?,
            mul_by_i128(self.sum_short,self.config.multiplier.checked_mul(window).ok_or(DecimalError::Overflow("window factor"))?)?,1)?.is_gt();
        if !over_long && !over_short {return Ok((None,None));}
        let open=history.open_time(position)?;
        if self.last_setup_time.is_some_and(|last| open.saturating_sub(last)<self.config.cooldown_seconds) {return Ok((None,None));}
        self.last_setup_time=Some(open);
        let growth_pct=ratio(mul_by_i128(sub(current_close,previous_close)?,100)?,previous_close)?;
        let volume_long_ratio=ratio(mul_by_i128(self.sum_window,long)?,mul_by_i128(self.sum_long,window)?)?;
        let volume_short_ratio=ratio(mul_by_i128(self.sum_window,short)?,mul_by_i128(self.sum_short,window)?)?;
        Ok((Some(SpikeSetup {algorithm_id:self.config.params.algorithm_id().into(),algorithm_version:self.config.params.algorithm_version().into(),
            open_time_unix:open,close_time_unix:open+MINUTE,close_price:history.original_close(index),params:self.config.params.clone(),
            details:Some(SetupDetails {growth_pct,volume_long_ratio,volume_short_ratio,over_long,over_short,missing_minutes:self.missing as u32})}),None))
    }
}
