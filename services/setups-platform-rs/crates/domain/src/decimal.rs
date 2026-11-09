use std::cmp::Ordering;
use std::fmt;

pub const MAX_SCALE: u32 = 18;

const POW10: [i128; 19] = [
    1,
    10,
    100,
    1_000,
    10_000,
    100_000,
    1_000_000,
    10_000_000,
    100_000_000,
    1_000_000_000,
    10_000_000_000,
    100_000_000_000,
    1_000_000_000_000,
    10_000_000_000_000,
    100_000_000_000_000,
    1_000_000_000_000_000,
    10_000_000_000_000_000,
    100_000_000_000_000_000,
    1_000_000_000_000_000_000,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Decimal {
    pub int: i128,
    pub scale: u32,
}

pub const ZERO: Decimal = Decimal { int: 0, scale: 0 };

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecimalError {
    Invalid(String),
    ScaleTooLarge(u32),
    Overflow(&'static str),
}

impl fmt::Display for DecimalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DecimalError::Invalid(value) => write!(f, "invalid decimal string: {value}"),
            DecimalError::ScaleTooLarge(scale) => {
                write!(f, "decimal scale {scale} exceeds maximum {MAX_SCALE}")
            }
            DecimalError::Overflow(op) => write!(f, "decimal overflow in {op}"),
        }
    }
}

impl std::error::Error for DecimalError {}

fn overflow(op: &'static str) -> DecimalError {
    DecimalError::Overflow(op)
}

impl Decimal {
    pub fn parse(value: &str) -> Result<Decimal, DecimalError> {
        let trimmed = value.trim();
        match trimmed.find('.') {
            None => {
                let int: i128 = trimmed
                    .parse()
                    .map_err(|_| DecimalError::Invalid(value.to_string()))?;
                Ok(Decimal { int, scale: 0 })
            }
            Some(point) => {
                let (head, tail) = trimmed.split_at(point);
                let frac = &tail[1..];
                let scale = frac.len() as u32;
                if scale > MAX_SCALE {
                    return Err(DecimalError::ScaleTooLarge(scale));
                }
                let mut digits = String::with_capacity(head.len() + frac.len());
                digits.push_str(head);
                digits.push_str(frac);
                let int: i128 = digits
                    .parse()
                    .map_err(|_| DecimalError::Invalid(value.to_string()))?;
                Ok(Decimal { int, scale })
            }
        }
    }
}

fn scale_up(int: i128, diff: u32) -> Result<i128, DecimalError> {
    int.checked_mul(POW10[diff as usize])
        .ok_or_else(|| overflow("scale alignment"))
}

fn align(a: Decimal, b: Decimal) -> Result<(i128, i128), DecimalError> {
    if a.scale == b.scale {
        Ok((a.int, b.int))
    } else if a.scale < b.scale {
        Ok((scale_up(a.int, b.scale - a.scale)?, b.int))
    } else {
        Ok((a.int, scale_up(b.int, a.scale - b.scale)?))
    }
}

pub fn add(a: Decimal, b: Decimal) -> Result<Decimal, DecimalError> {
    if a.scale == b.scale {
        Ok(Decimal {
            int: a.int.checked_add(b.int).ok_or_else(|| overflow("add"))?,
            scale: a.scale,
        })
    } else {
        let (ai, bi) = align(a, b)?;
        Ok(Decimal {
            int: ai.checked_add(bi).ok_or_else(|| overflow("add"))?,
            scale: a.scale.max(b.scale),
        })
    }
}

pub fn sub(a: Decimal, b: Decimal) -> Result<Decimal, DecimalError> {
    if a.scale == b.scale {
        Ok(Decimal {
            int: a.int.checked_sub(b.int).ok_or_else(|| overflow("sub"))?,
            scale: a.scale,
        })
    } else {
        let (ai, bi) = align(a, b)?;
        Ok(Decimal {
            int: ai.checked_sub(bi).ok_or_else(|| overflow("sub"))?,
            scale: a.scale.max(b.scale),
        })
    }
}

pub fn mul_by_i128(a: Decimal, factor: i128) -> Result<Decimal, DecimalError> {
    Ok(Decimal {
        int: a.int.checked_mul(factor).ok_or_else(|| overflow("mul"))?,
        scale: a.scale,
    })
}

pub fn compare_scaled(
    a: Decimal,
    a_mul: i128,
    b: Decimal,
    b_mul: i128,
) -> Result<Ordering, DecimalError> {
    let (ai, bi) = align(a, b)?;
    let left = ai.checked_mul(a_mul).ok_or_else(|| overflow("compare"))?;
    let right = bi.checked_mul(b_mul).ok_or_else(|| overflow("compare"))?;
    Ok(left.cmp(&right))
}

pub fn ratio(a: Decimal, b: Decimal) -> Result<Option<String>, DecimalError> {
    let (ai, bi) = align(a, b)?;
    if bi == 0 {
        return Ok(None);
    }
    let scaled = ai.checked_mul(10_000).ok_or_else(|| overflow("ratio"))? / bi;
    let negative = scaled < 0;
    let magnitude = scaled.unsigned_abs();
    let int_part = magnitude / 10_000;
    let frac_part = magnitude % 10_000;
    Ok(Some(if negative {
        format!("-{int_part}.{frac_part:04}")
    } else {
        format!("{int_part}.{frac_part:04}")
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_integers_and_fractions() {
        assert_eq!(Decimal::parse("12").unwrap(), Decimal { int: 12, scale: 0 });
        assert_eq!(
            Decimal::parse("1.500").unwrap(),
            Decimal {
                int: 1_500,
                scale: 3
            }
        );
        assert_eq!(
            Decimal::parse(" 2.5 ").unwrap(),
            Decimal { int: 25, scale: 1 }
        );
        assert_eq!(
            Decimal::parse("-0.00000001").unwrap(),
            Decimal { int: -1, scale: 8 }
        );
        assert_eq!(Decimal::parse("5.").unwrap(), Decimal { int: 5, scale: 0 });
        assert!(Decimal::parse("abc").is_err());
        assert!(Decimal::parse("").is_err());
        assert!(Decimal::parse("1e8").is_err());
    }

    #[test]
    fn add_sub_align_scales() {
        let a = Decimal { int: 1, scale: 0 };
        let b = Decimal { int: 25, scale: 2 };
        assert_eq!(add(a, b).unwrap(), Decimal { int: 125, scale: 2 });
        assert_eq!(sub(b, a).unwrap(), Decimal { int: -75, scale: 2 });
    }

    #[test]
    fn compare_scaled_keeps_exactness() {
        let sum = Decimal::parse("0.3").unwrap();
        let base = Decimal::parse("0.1").unwrap();
        assert_eq!(
            compare_scaled(sum, 1, base, 3).unwrap(),
            std::cmp::Ordering::Equal
        );
        assert_eq!(
            compare_scaled(sum, 4, base, 1).unwrap(),
            std::cmp::Ordering::Greater
        );
        assert_eq!(
            compare_scaled(base, 1, sum, 1).unwrap(),
            std::cmp::Ordering::Less
        );
    }

    #[test]
    fn ratio_truncates_toward_zero_with_four_decimals() {
        let a = Decimal { int: 1, scale: 0 };
        let b = Decimal { int: 3, scale: 0 };
        assert_eq!(ratio(a, b).unwrap().unwrap(), "0.3333");
        let neg = Decimal { int: -2, scale: 0 };
        let pos = Decimal { int: 3, scale: 0 };
        assert_eq!(ratio(neg, pos).unwrap().unwrap(), "-0.6666");
        assert_eq!(ratio(a, ZERO).unwrap(), None);
        let x = Decimal {
            int: 100_045,
            scale: 5,
        };
        let y = Decimal {
            int: 100_000,
            scale: 5,
        };
        assert_eq!(ratio(x, y).unwrap().unwrap(), "1.0004");
    }
}
