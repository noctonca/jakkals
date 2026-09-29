//! Money: US dollars as integer nano-dollars (billionths), read from
//! decimal digits and never through a float.

/// A cost has a handful of significant digits; more than fit a u128
/// with room to spare is not a cost.
const COST_DIGITS_MAX: usize = 30;

/// A decimal number of US dollars as nano-dollars, read from its decimal
/// digits as written, never through a float. Rounded to the nearest
/// nano-dollar, halves up. `None` for anything but a non-negative
/// number that fits u64 nano-dollars.
pub fn nano_usd(literal: &str) -> Option<u64> {
    let (mantissa, exponent) = match literal.find(['e', 'E']) {
        Some(at) => (&literal[..at], literal[at + 1..].parse::<i32>().ok()?),
        None => (literal, 0),
    };
    let (whole, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    if whole.is_empty()
        || !whole
            .bytes()
            .chain(fraction.bytes())
            .all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    let digits = format!("{whole}{fraction}");
    let digits = digits.trim_start_matches('0');
    if digits.is_empty() {
        return Some(0);
    }
    if digits.len() > COST_DIGITS_MAX {
        return None;
    }
    let value: u128 = digits.parse().expect("at most 30 ASCII digits fit u128");
    // value × 10^(exponent − fraction digits) dollars, and a dollar is
    // 10^9 nano-dollars.
    let fraction_digits = i64::try_from(fraction.len()).ok()?;
    let shift = i64::from(exponent) + 9 - fraction_digits;
    let nano = if shift >= 0 {
        value.checked_mul(10u128.checked_pow(u32::try_from(shift).ok()?)?)?
    } else {
        match 10u128.checked_pow(u32::try_from(-shift).ok()?) {
            Some(divisor) => (value + divisor / 2) / divisor,
            // A divisor past u128 is more than twice any value.
            None => 0,
        }
    };
    u64::try_from(nano).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cost_is_read_from_its_digits() {
        let cases = [
            ("0", Some(0)),
            ("0.0", Some(0)),
            ("1", Some(1_000_000_000)),
            ("0.000123456", Some(123_456)),
            ("12.3456789012", Some(12_345_678_901)),
            ("1.5e-6", Some(1_500)),
            ("1.5E-6", Some(1_500)),
            ("2e+3", Some(2_000_000_000_000)),
            // Halves round up, less rounds down.
            ("0.0000000005", Some(1)),
            ("0.00000000049", Some(0)),
            ("1e-400", Some(0)),
            // The most u64 nano-dollars hold, and one past it.
            ("18446744073.709551615", Some(u64::MAX)),
            ("18446744073.709551616", None),
            ("1e30", None),
            ("-0.1", None),
            (".5", None),
            ("\"0.1\"", None),
            ("null", None),
            ("1234567890123456789012345678901", None),
        ];
        for (literal, expected) in cases {
            assert_eq!(nano_usd(literal), expected, "{literal}");
        }
    }
}
