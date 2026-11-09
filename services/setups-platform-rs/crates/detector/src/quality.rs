use serde::{Deserialize, Serialize};
use setups_domain::{PushResult, PushStatus, SkipReason};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QualityReason {
    Evaluated,
    MissingCandle,
    InsufficientWarmup,
    RecentGap,
    PriceReferenceMissing,
    GapToleranceExceeded,
}

impl From<SkipReason> for QualityReason {
    fn from(reason: SkipReason) -> Self {
        match reason {
            SkipReason::InsufficientWarmup => QualityReason::InsufficientWarmup,
            SkipReason::RecentGap => QualityReason::RecentGap,
            SkipReason::PriceReferenceMissing => QualityReason::PriceReferenceMissing,
            SkipReason::GapToleranceExceeded => QualityReason::GapToleranceExceeded,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QualityState {
    InProgress,
    Complete,
    Partial,
    NoEligibleData,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ReasonCounts {
    pub missing_candle: u64,
    pub insufficient_warmup: u64,
    pub recent_gap: u64,
    pub price_reference_missing: u64,
    pub gap_tolerance_exceeded: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplayQuality {
    pub version: u32,
    pub state: QualityState,
    pub required_history_minutes: u64,
    pub requested_minutes: u64,
    pub evaluated_minutes: u64,
    pub evaluated_with_gaps_minutes: u64,
    pub skipped_minutes: u64,
    pub unassessed_minutes: u64,
    pub assessed_through_unix: i64,
    pub reason_counts: ReasonCounts,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QualityInterval {
    pub from_unix: i64,
    pub to_unix: i64,
    pub reason: QualityReason,
}

fn ceil_to_minute(unix: i64) -> i64 {
    (unix + 59).div_euclid(60) * 60
}

fn floor_to_minute(unix: i64) -> i64 {
    unix.div_euclid(60) * 60
}

pub struct ReplayQualityTracker {
    quality: ReplayQuality,
    end: i64,
    intervals: Vec<QualityInterval>,
}

impl ReplayQualityTracker {
    pub fn new(from_unix: i64, to_unix: i64, required_history_minutes: u64) -> Self {
        let start = ceil_to_minute(from_unix);
        let end = ceil_to_minute(to_unix);
        let requested_minutes = ((end - start) / 60) as u64;
        ReplayQualityTracker {
            quality: ReplayQuality {
                version: 1,
                state: QualityState::InProgress,
                required_history_minutes,
                requested_minutes,
                evaluated_minutes: 0,
                evaluated_with_gaps_minutes: 0,
                skipped_minutes: 0,
                unassessed_minutes: requested_minutes,
                assessed_through_unix: start,
                reason_counts: ReasonCounts::default(),
            },
            end,
            intervals: Vec::new(),
        }
    }

    pub fn quality(&self) -> &ReplayQuality {
        &self.quality
    }

    pub fn observe(&mut self, open_time_unix: i64, result: &PushResult) {
        let time = floor_to_minute(open_time_unix);
        if time < self.quality.assessed_through_unix || time >= self.end {
            return;
        }
        self.append(time, QualityReason::MissingCandle);
        let reason = result.skip_reason.map(QualityReason::from).unwrap_or(
            if result.status == PushStatus::Warmup {
                QualityReason::InsufficientWarmup
            } else {
                QualityReason::Evaluated
            },
        );
        self.append(time + 60, reason);
        if reason == QualityReason::Evaluated && result.missing_minutes.unwrap_or(0) > 0 {
            self.quality.evaluated_with_gaps_minutes += 1;
        }
    }

    pub fn finish(&mut self) {
        self.append(self.end, QualityReason::MissingCandle);
        self.quality.state = if self.quality.evaluated_minutes == 0 {
            QualityState::NoEligibleData
        } else if self.quality.skipped_minutes > 0 {
            QualityState::Partial
        } else {
            QualityState::Complete
        };
    }

    pub fn drain(&mut self) -> Vec<QualityInterval> {
        std::mem::take(&mut self.intervals)
    }

    fn append(&mut self, to_unix: i64, reason: QualityReason) {
        let from_unix = self.quality.assessed_through_unix;
        if to_unix <= from_unix {
            return;
        }
        let minutes = ((to_unix - from_unix) / 60) as u64;
        if reason == QualityReason::Evaluated {
            self.quality.evaluated_minutes += minutes;
        } else {
            self.quality.skipped_minutes += minutes;
            match reason {
                QualityReason::MissingCandle => {
                    self.quality.reason_counts.missing_candle += minutes;
                }
                QualityReason::InsufficientWarmup => {
                    self.quality.reason_counts.insufficient_warmup += minutes;
                }
                QualityReason::RecentGap => {
                    self.quality.reason_counts.recent_gap += minutes;
                }
                QualityReason::PriceReferenceMissing => {
                    self.quality.reason_counts.price_reference_missing += minutes;
                }
                QualityReason::GapToleranceExceeded => {
                    self.quality.reason_counts.gap_tolerance_exceeded += minutes;
                }
                QualityReason::Evaluated => unreachable!(),
            }
        }
        self.quality.unassessed_minutes -= minutes;
        self.quality.assessed_through_unix = to_unix;
        match self.intervals.last_mut() {
            Some(last) if last.reason == reason && last.to_unix == from_unix => {
                last.to_unix = to_unix;
            }
            _ => self.intervals.push(QualityInterval {
                from_unix,
                to_unix,
                reason,
            }),
        }
    }
}
