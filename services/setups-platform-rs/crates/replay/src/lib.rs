use serde::{Deserialize, Serialize};
use setups_detector::PriceVolumeSpikeDetector;
use setups_detector::quality::{ReplayQuality, ReplayQualityTracker};
use setups_domain::decimal::DecimalError;
use setups_domain::{AlgorithmParams, MINUTE_SECONDS, PushStatus, SpikeSetup};

pub const PAGE_LIMIT: usize = 5000;

const DEFAULT_PAGE_LIMIT: u32 = 1000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CandleBarDto {
    pub open_time_unix: i64,
    pub close: String,
    pub volume: String,
    pub quote_turnover: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CandlePage {
    pub bars: Vec<CandleBarDto>,
    pub next_cursor_unix: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CandleSourceQuery {
    pub from_unix: i64,
    pub to_unix: i64,
    pub after_unix: Option<i64>,
    pub limit: Option<u32>,
}

pub type SourceResult = Result<CandlePage, String>;

#[allow(async_fn_in_trait)]
pub trait CandleSource: Send + Sync {
    async fn read(&self, query: &CandleSourceQuery) -> SourceResult;
}

pub struct ArrayCandleSource {
    bars: Vec<CandleBarDto>,
}

impl ArrayCandleSource {
    pub fn new(bars: Vec<CandleBarDto>) -> Self {
        ArrayCandleSource { bars }
    }
}

impl CandleSource for ArrayCandleSource {
    async fn read(&self, query: &CandleSourceQuery) -> SourceResult {
        let limit = query
            .limit
            .unwrap_or(DEFAULT_PAGE_LIMIT)
            .min(PAGE_LIMIT as u32) as usize;
        let empty_page = || CandlePage {
            bars: Vec::new(),
            next_cursor_unix: None,
        };
        let from_index = self.bars.iter().position(|bar| {
            bar.open_time_unix >= query.from_unix
                && query
                    .after_unix
                    .is_none_or(|after| bar.open_time_unix > after)
        });
        let Some(from_index) = from_index else {
            return Ok(empty_page());
        };
        let mut page: Vec<CandleBarDto> = Vec::new();
        for bar in &self.bars[from_index..] {
            if bar.open_time_unix >= query.to_unix {
                break;
            }
            if query
                .after_unix
                .is_some_and(|after| bar.open_time_unix <= after)
            {
                continue;
            }
            if page.len() > limit {
                break;
            }
            page.push(bar.clone());
        }
        let has_more = page.len() > limit;
        if has_more {
            page.truncate(limit);
        }
        Ok(CandlePage {
            next_cursor_unix: if has_more {
                page.last().map(|bar| bar.open_time_unix)
            } else {
                None
            },
            bars: page,
        })
    }
}

#[derive(Default)]
pub struct CancelToken {
    cancelled: std::sync::atomic::AtomicBool,
}

impl CancelToken {
    pub fn new() -> Self {
        CancelToken::default()
    }

    pub fn cancel(&self) {
        self.cancelled
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[derive(Debug)]
pub enum ReplayRunError {
    Source(String),
    PageLimitExceeded(usize),
    Detector(DecimalError),
}

impl std::fmt::Display for ReplayRunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReplayRunError::Source(message) => write!(f, "replay source error: {message}"),
            ReplayRunError::PageLimitExceeded(size) => {
                write!(f, "replay source exceeded page limit: {size}")
            }
            ReplayRunError::Detector(error) => write!(f, "detector error: {error}"),
        }
    }
}

impl std::error::Error for ReplayRunError {}

pub struct ReplayPageCheckpoint {
    pub setups: Vec<SpikeSetup>,
    pub warmup_minutes: u64,
    pub processed_minutes: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplayRunResult {
    pub quality: ReplayQuality,
    pub setups: Vec<SpikeSetup>,
    pub processed_minutes: u64,
    pub warmup_minutes: u64,
    pub last_processed_unix: Option<i64>,
    pub cancelled: bool,
    pub last_status: Option<PushStatus>,
}

pub struct ReplayRunOptions<'a, S> {
    pub source: &'a S,
    pub from_unix: i64,
    pub to_unix: i64,
    pub params: Option<AlgorithmParams>,
    pub last_setup_unix: Option<i64>,
    pub warmup_unix: Option<i64>,
    pub cancel: Option<&'a CancelToken>,
}

pub async fn replay_run<S: CandleSource>(
    options: ReplayRunOptions<'_, S>,
) -> Result<ReplayRunResult, ReplayRunError> {
    let params = options.params.unwrap_or_default();
    let required_minutes = params.baseline_long_days() as u64 * 1440;
    let mut detector = PriceVolumeSpikeDetector::with_params(params, options.last_setup_unix)
        .map_err(ReplayRunError::Detector)?;
    let mut tracker =
        ReplayQualityTracker::new(options.from_unix, options.to_unix, required_minutes);
    let mut last_status: Option<PushStatus> = None;
    let warmup_unix = options
        .warmup_unix
        .unwrap_or(options.from_unix - required_minutes as i64 * MINUTE_SECONDS);

    let mut setups: Vec<SpikeSetup> = Vec::new();
    let mut processed_minutes: u64 = 0;
    let mut warmup_minutes: u64 = 0;
    let mut last_processed_unix: Option<i64> = None;
    let mut cursor: Option<i64> = None;

    let cancelled = || options.cancel.is_some_and(|token| token.is_cancelled());

    macro_rules! flush {
        () => {
            tracker.drain();
        };
    }

    flush!();
    'outer: loop {
        if cancelled() {
            break;
        }
        let page = options
            .source
            .read(&CandleSourceQuery {
                from_unix: warmup_unix,
                to_unix: options.to_unix,
                after_unix: cursor,
                limit: Some(PAGE_LIMIT as u32),
            })
            .await
            .map_err(ReplayRunError::Source)?;
        if page.bars.is_empty() {
            break;
        }
        if page.bars.len() > PAGE_LIMIT {
            return Err(ReplayRunError::PageLimitExceeded(page.bars.len()));
        }
        for bar in &page.bars {
            if cancelled() {
                break 'outer;
            }
            let detector_bar = setups_domain::DetectorBar {
                open_time_unix: bar.open_time_unix,
                close: bar.close.clone(),
                quote_turnover: bar.quote_turnover.clone(),
            };
            let result = detector
                .push(&detector_bar)
                .map_err(ReplayRunError::Detector)?;
            tracker.observe(bar.open_time_unix, &result);
            last_status = Some(result.status);
            match result.status {
                PushStatus::Warmup => warmup_minutes += 1,
                PushStatus::Evaluated => processed_minutes += 1,
            }
            if let Some(setup) = &result.setup
                && bar.open_time_unix >= options.from_unix
            {
                setups.push(setup.clone());
            }
            last_processed_unix = Some(bar.open_time_unix);
        }
        flush!();
        let Some(next_cursor) = page.next_cursor_unix else {
            break;
        };
        cursor = Some(next_cursor);
    }

    if !cancelled() {
        tracker.finish();
    }
    flush!();

    Ok(ReplayRunResult {
        quality: *tracker.quality(),
        setups,
        processed_minutes,
        warmup_minutes,
        last_processed_unix,
        cancelled: cancelled(),
        last_status,
    })
}
