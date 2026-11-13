use super::*;
use chrono::{DateTime, Utc};
use setups_domain::decimal::Decimal;

/// Plain text, not Telegram/HTML markup. Rendering a name cannot inject markup.
pub fn recommendation_message(
    instrument: &AdaptiveInstrument,
    raw: &SpikeSetup,
    value: &Value,
) -> Result<String, WorkerError> {
    let envelope: RecommendationEnvelope = serde_json::from_value(value.clone())?;
    let snapshot = &envelope.snapshot;
    let target = raw
        .close_time_unix
        .checked_add(
            snapshot
                .time_shift_minutes
                .checked_mul(60)
                .ok_or_else(|| invalid("entry time overflow"))?,
        )
        .ok_or_else(|| invalid("entry time overflow"))?;
    let mut lines = vec![
        format!(
            "{} · {} · {}",
            instrument.symbol,
            match snapshot.direction {
                RecommendationDirection::Short => "SHORT",
                RecommendationDirection::Long => "LONG",
            },
            if instrument.exchange == "bybit" {
                "Bybit"
            } else {
                &instrument.exchange
            }
        ),
        format!("Цена: {} USDT", display_decimal(&raw.close_price)?),
        String::new(),
        "Условия сделки:".into(),
    ];
    if snapshot.time_shift_minutes > 0 {
        let time = DateTime::<Utc>::from_timestamp(target, 0)
            .ok_or_else(|| invalid("invalid entry timestamp"))?;
        lines.push(format!(
            "Рекомендуемое время входа: {} UTC (через {} мин после сигнала)",
            time.format("%Y-%m-%d %H:%M:%S"),
            snapshot.time_shift_minutes
        ));
    }
    lines.push(format!(
        "TP = {}%, SL = {}% (от фактической цены входа)",
        snapshot.tp_pct.to_string().replace('.', ","),
        snapshot.sl_pct.to_string().replace('.', ",")
    ));
    if let Some(pct) = snapshot.price_change_pct {
        let threshold = price_threshold(&raw.close_price, pct, snapshot.direction)?;
        lines.push(format!(
            "Заходить в сделку, только если цена не {} {} USDT на момент входа",
            match snapshot.direction {
                RecommendationDirection::Short => "опустится ниже",
                RecommendationDirection::Long => "поднимется выше",
            },
            threshold
        ));
    }
    lines.push(String::new());
    lines.push(
        format!(
            "Планируемое матожидание торговли: {:+.2} R · матрица {:+.2} R",
            snapshot.raw_r, snapshot.mean_r
        )
        .replace('.', ","),
    );
    lines.push(String::new());
    lines.push(format!("Алгоритм: {}", envelope.external_name));
    lines.push(format!("Исследование №{}", envelope.study_id));
    Ok(lines.join("\n"))
}

fn display_decimal(raw: &str) -> Result<String, WorkerError> {
    let parsed = Decimal::parse(raw)?;
    if parsed.int <= 0 {
        return Err(invalid("nonpositive raw price"));
    }
    let digits = parsed.int.to_string();
    Ok(format_amount(&digits, parsed.scale as usize, false))
}

// Tiny bounded decimal-text multiplication is used only for presentation.
// Unlike binary float multiplication it preserves the source price and shortest
// JSON decimal price-change threshold, including scientific notation. Inputs
// are at most a source Decimal (38 digits) and an f64 (324 decimal places).
fn price_threshold(
    price: &str,
    pct: f64,
    direction: RecommendationDirection,
) -> Result<String, WorkerError> {
    let parsed = Decimal::parse(price)?;
    if parsed.int <= 0 || !pct.is_finite() || !(-100.0..=1000.0).contains(&pct) {
        return Err(invalid("invalid price threshold"));
    }
    let pct_negative = pct.is_sign_negative();
    let raw = pct.abs().to_string();
    let (coefficient, exponent) = raw
        .split_once('e')
        .map_or((raw.as_str(), 0), |(digits, exponent)| {
            (digits, exponent.parse::<i32>().unwrap_or(0))
        });
    let fraction = coefficient
        .split_once('.')
        .map_or(0, |(_, fraction)| fraction.len()) as i32;
    let mut digits: Vec<u8> = coefficient
        .bytes()
        .filter(|byte| *byte != b'.')
        .rev()
        .map(|byte| byte - b'0')
        .collect();
    let mut scale = fraction - exponent;
    if scale < 0 {
        digits.splice(0..0, std::iter::repeat_n(0, (-scale) as usize));
        scale = 0;
    }
    let scale = scale as usize;
    let mut hundred = vec![0; scale + 3];
    hundred[scale + 2] = 1;
    trim(&mut digits);
    let subtract = pct_negative ^ (direction == RecommendationDirection::Long);
    let (factor, negative) = if subtract {
        match compare(&hundred, &digits) {
            std::cmp::Ordering::Less => (subtract_digits(&digits, &hundred), true),
            _ => (subtract_digits(&hundred, &digits), false),
        }
    } else {
        (add_digits(&hundred, &digits), false)
    };
    let price: Vec<u8> = parsed
        .int
        .to_string()
        .bytes()
        .rev()
        .map(|byte| byte - b'0')
        .collect();
    let mut product = vec![0u32; price.len() + factor.len() + 1];
    for (i, &a) in price.iter().enumerate() {
        for (j, &b) in factor.iter().enumerate() {
            product[i + j] += u32::from(a) * u32::from(b);
        }
    }
    for i in 0..product.len() - 1 {
        let carry = product[i] / 10;
        product[i] %= 10;
        product[i + 1] += carry;
    }
    let mut product: Vec<u8> = product.into_iter().map(|digit| digit as u8).collect();
    trim(&mut product);
    let digits: String = product
        .iter()
        .rev()
        .map(|digit| char::from(b'0' + digit))
        .collect();
    Ok(format_amount(
        &digits,
        parsed.scale as usize + scale + 2,
        negative,
    ))
}

fn trim(digits: &mut Vec<u8>) {
    while digits.len() > 1 && digits.last() == Some(&0) {
        digits.pop();
    }
}
fn compare(a: &[u8], b: &[u8]) -> std::cmp::Ordering {
    a.len()
        .cmp(&b.len())
        .then_with(|| a.iter().rev().cmp(b.iter().rev()))
}
fn add_digits(a: &[u8], b: &[u8]) -> Vec<u8> {
    let mut result = Vec::with_capacity(a.len().max(b.len()) + 1);
    let mut carry = 0;
    for i in 0..a.len().max(b.len()) {
        let digit = a.get(i).copied().unwrap_or(0) + b.get(i).copied().unwrap_or(0) + carry;
        result.push(digit % 10);
        carry = digit / 10;
    }
    if carry > 0 {
        result.push(carry);
    }
    trim(&mut result);
    result
}
fn subtract_digits(a: &[u8], b: &[u8]) -> Vec<u8> {
    let mut result = Vec::with_capacity(a.len());
    let mut borrow = 0i16;
    for (i, &a) in a.iter().enumerate() {
        let digit = i16::from(a) - i16::from(b.get(i).copied().unwrap_or(0)) - borrow;
        result.push(digit.rem_euclid(10) as u8);
        borrow = i16::from(digit < 0);
    }
    trim(&mut result);
    result
}
fn format_amount(digits: &str, scale: usize, negative: bool) -> String {
    let digits = format!(
        "{}{}",
        "0".repeat(scale.saturating_add(1).saturating_sub(digits.len())),
        digits
    );
    let split = digits.len() - scale;
    let fraction = digits[split..].trim_end_matches('0');
    format!(
        "{}{},{}{}",
        if negative && digits.bytes().any(|byte| byte != b'0') {
            "-"
        } else {
            ""
        },
        &digits[..split],
        fraction,
        "0".repeat(2usize.saturating_sub(fraction.len()))
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use setups_domain::AlgorithmParams;

    #[test]
    fn adaptive_push_text_uses_saved_direction_risks_name_and_raw_time() {
        let mut raw = SpikeSetup {
            algorithm_id: "price_volume_spike".into(),
            algorithm_version: "1.1.1".into(),
            open_time_unix: 86340,
            close_time_unix: 86400,
            close_price: "100".into(),
            params: AlgorithmParams::default(),
            details: None,
        };
        let instrument = AdaptiveInstrument {
            exchange: "bybit".into(),
            category: "linear".into(),
            symbol: "BTCUSDT".into(),
        };
        let mut snapshot = json!({"direction":"short","tpPct":14,"slPct":4,"rawR":0.32,"meanR":0.21,"robustScore":0.29,
            "timeShiftMinutes":3,"priceChangePct":1,"clusterEnabled":true,"clusterGapMinutes":480,
            "provenance":{"algorithmId":"price_volume_spike","algorithmVersion":"1.1.1","optimizerVersion":"plan-a-continuous-1","seed":42,
                "snapshotHash":"fixture","capturedAt":"2026-11-12T00:00:00Z","fromUnix":0,"toUnix":86400,"observationCutoff":86400},
            "studyId":42,"generationId":3,"slotId":7,"ownerId":"admin","externalName":"<b>*plain name*","params":raw.params,
            "rawSignalAt":86400,"recommendedEntryAt":86580});
        let message = recommendation_message(&instrument, &raw, &snapshot).unwrap();
        assert!(message.contains("BTCUSDT · SHORT · Bybit"));
        assert!(message.contains("1970-01-02 00:03:00 UTC"));
        assert!(message.contains("не опустится ниже 101,00 USDT"));
        assert!(message.contains("TP = 14%, SL = 4% (от фактической цены входа)"));
        assert!(message.contains("+0,32 R · матрица +0,21 R"));
        assert!(message.contains("Алгоритм: <b>*plain name*"));
        assert!(message.contains("Исследование №42"));
        assert!(!message.contains("0,29"));

        // A chart candle labelled 17:24 closes at 17:25; shift 3 enters at 17:28.
        raw.open_time_unix = DateTime::parse_from_rfc3339("2026-11-13T17:24:00Z")
            .unwrap()
            .timestamp();
        raw.close_time_unix = raw.open_time_unix + 60;
        snapshot["rawSignalAt"] = json!(raw.close_time_unix);
        snapshot["recommendedEntryAt"] = json!(raw.close_time_unix + 3 * 60);
        let message = recommendation_message(&instrument, &raw, &snapshot).unwrap();
        assert!(message.contains("2026-11-13 17:28:00 UTC"));

        snapshot["direction"] = json!("long");
        snapshot["priceChangePct"] = json!(-1);
        snapshot["timeShiftMinutes"] = json!(0);
        let message = recommendation_message(&instrument, &raw, &snapshot).unwrap();
        assert!(message.contains("не поднимется выше 101,00 USDT"));
        assert!(!message.contains("Рекомендуемое время входа"));
        snapshot["priceChangePct"] = json!(0);
        assert!(
            recommendation_message(&instrument, &raw, &snapshot)
                .unwrap()
                .contains("не поднимется выше 100,00 USDT")
        );
        snapshot["priceChangePct"] = Value::Null;
        assert!(
            !recommendation_message(&instrument, &raw, &snapshot)
                .unwrap()
                .contains("только если")
        );
    }

    #[test]
    fn thresholds_keep_signed_zero_and_scientific_decimals_exact() {
        use RecommendationDirection::{Long, Short};
        assert_eq!(price_threshold("100", 1.0, Short).unwrap(), "101,00");
        assert_eq!(price_threshold("100", -1.0, Short).unwrap(), "99,00");
        assert_eq!(price_threshold("100", 1.0, Long).unwrap(), "99,00");
        assert_eq!(price_threshold("100", -1.0, Long).unwrap(), "101,00");
        assert_eq!(price_threshold("100", 0.0, Long).unwrap(), "100,00");
        assert_eq!(price_threshold("100", 1e-7, Short).unwrap(), "100,0000001");
        assert_eq!(
            price_threshold("123456789012345678.123456789012345678", 1.0, Short).unwrap(),
            "124691356902469134,90469135690246913478"
        );
    }
}
