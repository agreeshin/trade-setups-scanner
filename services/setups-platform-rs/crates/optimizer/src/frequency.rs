use crate::validation::invalid;
use crate::{StudyError, StudyRequest};

/// Fixed-scope frequency admission. Compute its exact integer requirement once
/// per detector packet; the displayed f64 average never decides admission.
pub(crate) struct FrequencyGate {
    duration_seconds: u64,
    pub required_setup_count: Option<usize>,
}

impl FrequencyGate {
    pub fn new(spec: &StudyRequest) -> Result<Self, StudyError> {
        if !spec.min_setups_per_day.is_finite() || spec.min_setups_per_day < 0.0 {
            return Err(invalid("minSetupsPerDay must be finite and nonnegative"));
        }
        let duration_seconds =
            spec.to_unix
                .checked_sub(spec.from_unix)
                .filter(|duration| *duration > 0)
                .ok_or_else(|| invalid("positive signal duration required"))? as u64;
        Ok(Self {
            duration_seconds,
            required_setup_count: required_count(spec.min_setups_per_day, duration_seconds),
        })
    }

    pub fn accepts(&self, selected_setup_count: usize) -> bool {
        self.required_setup_count
            .is_some_and(|minimum| selected_setup_count >= minimum)
    }

    pub fn average(&self, selected_setup_count: usize) -> f64 {
        selected_setup_count as f64 * 86400.0 / self.duration_seconds as f64
    }
}

/// ceil(threshold * seconds / 86400), using the shortest decimal spelling of
/// the normalized f64 threshold, NOT its binary product. This accepts exact
/// decimal boundaries such as 6 signals / 10 days >= 0.6 without an epsilon.
/// Finite f64 spelling bounds these temporary digit vectors (< 1 KiB each),
/// including extreme/subnormal thresholds; no big-int dependency or scale cap.
fn required_count(threshold: f64, duration_seconds: u64) -> Option<usize> {
    let text = threshold.to_string();
    let (mantissa, exponent) = match text.split_once(['e', 'E']) {
        Some((mantissa, exponent)) => (
            mantissa,
            exponent.parse::<i32>().expect("finite f64 exponent"),
        ),
        None => (text.as_str(), 0),
    };
    let fractional_digits = mantissa
        .split_once('.')
        .map_or(0, |(_, fraction)| fraction.len()) as i32;
    let scale = fractional_digits - exponent;
    let mut digits: Vec<u8> = mantissa
        .bytes()
        .filter(u8::is_ascii_digit)
        .map(|digit| digit - b'0')
        .collect();
    let mut carry = 0u128;
    for digit in digits.iter_mut().rev() {
        let product = u128::from(*digit) * u128::from(duration_seconds) + carry;
        *digit = (product % 10) as u8;
        carry = product / 10;
    }
    let mut prefix = Vec::new();
    while carry > 0 {
        prefix.push((carry % 10) as u8);
        carry /= 10;
    }
    prefix.reverse();
    prefix.extend(digits);
    if scale < 0 {
        prefix.extend(std::iter::repeat_n(0, (-scale) as usize));
    }
    // Decimal long division by the fixed seconds/day denominator.
    let mut remainder = 0u64;
    for digit in &mut prefix {
        let dividend = remainder * 10 + u64::from(*digit);
        *digit = (dividend / 86400) as u8;
        remainder = dividend % 86400;
    }
    let integer_len = prefix.len().saturating_sub(scale.max(0) as usize);
    let mut count = 0usize;
    for digit in &prefix[..integer_len] {
        count = count.checked_mul(10)?.checked_add(usize::from(*digit))?;
    }
    if remainder > 0 || prefix[integer_len..].iter().any(|digit| *digit != 0) {
        count = count.checked_add(1)?;
    }
    Some(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_decimal_and_fractional_day_boundaries_without_float_products() {
        assert_eq!(required_count(4.0, 864000), Some(40));
        assert_eq!(required_count(4.0, 864060), Some(41));
        assert_eq!(required_count(0.6, 864000), Some(6));
        assert_eq!(required_count(0.1, 864000), Some(1));
        assert_eq!(required_count(0.6000000000000001, 864000), Some(7));
        assert_eq!(required_count(4.0, 129600), Some(6));
        assert_eq!(required_count(4.0, 60), Some(1));
        assert_eq!(required_count(0.0, 864060), Some(0));
        assert_eq!(required_count(-0.0, 864060), Some(0));
    }

    #[test]
    fn all_finite_threshold_magnitudes_stay_bounded_and_fail_closed_on_count_overflow() {
        assert_eq!(required_count(f64::from_bits(1), 864000), Some(1));
        assert_eq!(required_count(1e-200, 864000), Some(1));
        assert_eq!(required_count(1e200, 864000), None);
        assert_eq!(required_count(f64::MAX, 864000), None);
    }
}
