//! The outcome of ONE released recommendation. No cache keys, research inputs or I/O.
use crate::{
    AnalyticsSetup, EvaluateLimitOptions, LimitEvaluation, LimitStatus, OhlcCandle, TradeDirection,
    evaluate_limit_at_open, passes_price_change, risk_return,
};
use serde::{Deserialize, Serialize};

pub const HORIZON_MINUTES: i64 = 10_080;

#[derive(Clone, Copy)]
pub struct TradeTerms {
    pub direction: TradeDirection,
    pub tp_pct: f64,
    pub sl_pct: f64,
    pub time_shift_minutes: i64,
    pub price_change_pct: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TradeOutcome {
    pub status: String,
    pub entry_time_unix: Option<i64>,
    pub entry_price: Option<f64>,
    pub gate_threshold: Option<f64>,
    pub tp_price: Option<f64>,
    pub sl_price: Option<f64>,
    pub close_time_unix: Option<i64>,
    pub close_price: Option<f64>,
    pub risk_return: Option<f64>,
    pub evaluated_at_unix: i64,
    pub observed_through_unix: i64,
    pub is_final: bool,
}

pub fn gate_threshold(close: f64, pct: f64, direction: TradeDirection) -> f64 {
    match direction {
        TradeDirection::Short => close * (1.0 + pct / 100.0),
        TradeDirection::Long => close * (1.0 - pct / 100.0),
    }
}

pub use crate::trade_levels as display_levels;

pub fn classify(terms: TradeTerms, evaluation: LimitEvaluation, close: f64) -> TradeOutcome {
    let mut result = TradeOutcome {
        status: "no_data".into(),
        entry_time_unix: None,
        entry_price: None,
        gate_threshold: terms
            .price_change_pct
            .map(|pct| gate_threshold(close, pct, terms.direction)),
        tp_price: None,
        sl_price: None,
        close_time_unix: None,
        close_price: None,
        risk_return: None,
        evaluated_at_unix: 0,
        observed_through_unix: 0,
        is_final: false,
    };
    let Some(entry) = evaluation.entry_price else {
        return result;
    };
    result.entry_time_unix = evaluation.entry_time_unix;
    result.entry_price = Some(entry);
    if terms
        .price_change_pct
        .is_some_and(|pct| !passes_price_change(close, entry, terms.direction, pct))
    {
        result.status = "gate_failed".into();
        result.is_final = true;
        return result;
    }
    result.status = match evaluation.status {
        LimitStatus::Tp => "tp",
        LimitStatus::Sl => "sl",
        LimitStatus::Unclosed => "unclosed",
        LimitStatus::NoData => "no_data",
        LimitStatus::PriceChangeFiltered => "gate_failed",
    }
    .into();
    if matches!(
        evaluation.status,
        LimitStatus::Tp | LimitStatus::Sl | LimitStatus::Unclosed
    ) {
        let (tp, sl) = display_levels(entry, terms.tp_pct, terms.sl_pct, terms.direction);
        result.tp_price = Some(tp);
        result.sl_price = Some(sl);
        result.risk_return = risk_return(evaluation.status, terms.tp_pct, terms.sl_pct);
        result.close_time_unix = evaluation.close_time_unix;
        result.close_price = match evaluation.status {
            LimitStatus::Tp => Some(tp),
            LimitStatus::Sl => Some(sl),
            _ => None,
        };
    }
    result.is_final = matches!(result.status.as_str(), "tp" | "sl" | "gate_failed");
    result
}

/// Caller supplies only already-closed candles, bounded to this observation.
pub fn evaluate(
    setup: &AnalyticsSetup,
    terms: TradeTerms,
    candles: &[OhlcCandle],
    observed_through_unix: i64,
    evaluated_at_unix: i64,
) -> TradeOutcome {
    let evaluation = evaluate_limit_at_open(EvaluateLimitOptions {
        candles,
        setup,
        direction: terms.direction,
        tp_pct: terms.tp_pct,
        sl_pct: terms.sl_pct,
        time_shift_minutes: terms.time_shift_minutes,
        horizon_minutes: HORIZON_MINUTES,
    });
    let mut result = classify(
        terms,
        evaluation,
        setup.close_price.parse().unwrap_or(f64::NAN),
    );
    result.observed_through_unix = observed_through_unix;
    result.evaluated_at_unix = evaluated_at_unix;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    fn terms(direction: TradeDirection, gate: Option<f64>) -> TradeTerms {
        TradeTerms {
            direction,
            tp_pct: 20.0,
            sl_pct: 5.0,
            time_shift_minutes: 2,
            price_change_pct: gate,
        }
    }
    fn setup() -> AnalyticsSetup {
        AnalyticsSetup {
            exchange_code: "bybit".into(),
            category: "linear".into(),
            symbol: "TEST".into(),
            open_time_unix: 60,
            close_time_unix: 60,
            close_price: "100".into(),
        }
    }
    fn bar(at: i64, open: f64, high: f64, low: f64) -> OhlcCandle {
        OhlcCandle {
            open_time_unix: at,
            open,
            high,
            low,
            close: 999.0,
        }
    }
    #[test]
    fn entry_open_shift_first_touch_and_double_touch_sl_are_unchanged() {
        let out = evaluate(
            &setup(),
            terms(TradeDirection::Short, None),
            &[bar(180, 100.0, 106.0, 79.0)],
            240,
            250,
        );
        assert_eq!(out.status, "sl");
        assert_eq!(out.entry_price, Some(100.0));
        assert_eq!(out.entry_time_unix, Some(180));
        assert_eq!(out.close_time_unix, Some(180));
        assert_eq!(out.close_price, Some(105.0));
        assert_eq!(out.risk_return, Some(-1.0));
        assert!(out.is_final);
    }
    #[test]
    fn long_tp_and_signed_optional_zero_gate() {
        let out = evaluate(
            &setup(),
            terms(TradeDirection::Long, Some(-2.0)),
            &[bar(180, 101.0, 122.0, 100.0)],
            240,
            250,
        );
        assert_eq!(out.status, "tp");
        assert_eq!(out.risk_return, Some(4.0));
        assert_eq!(out.close_price, out.tp_price);
        let bars = [bar(180, 99.0, 100.0, 98.0)];
        assert_eq!(
            evaluate(
                &setup(),
                terms(TradeDirection::Short, None),
                &bars,
                240,
                250
            )
            .status,
            "unclosed"
        );
        let rejected = evaluate(
            &setup(),
            terms(TradeDirection::Short, Some(0.0)),
            &bars,
            240,
            250,
        );
        assert_eq!(rejected.status, "gate_failed");
        assert_eq!(rejected.close_price, None);
        assert_eq!(rejected.risk_return, None);
    }
    #[test]
    fn missing_entry_and_expired_horizon_never_make_a_synthetic_close() {
        let missing = evaluate(&setup(), terms(TradeDirection::Short, None), &[], 120, 150);
        assert_eq!(missing.status, "no_data");
        assert!(!missing.is_final);
        let out = evaluate(
            &setup(),
            terms(TradeDirection::Short, None),
            &[
                bar(180, 100.0, 101.0, 99.0),
                bar(180 + HORIZON_MINUTES * 60, 100.0, 110.0, 70.0),
            ],
            180 + (HORIZON_MINUTES + 1) * 60,
            999999,
        );
        assert_eq!(out.status, "unclosed");
        assert_eq!(out.close_time_unix, None);
        assert_eq!(out.close_price, None);
        assert_eq!(out.risk_return, None);
    }
}
