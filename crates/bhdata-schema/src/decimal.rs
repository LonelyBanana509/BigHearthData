//! Exact decimal text, as whole numbers scaled by `10^scale` (an `i128` holds 38 digits).

/// Reads `text` (`-12.50`, no exponent, no spaces) as a value scaled by `10^scale`. Refuses more
/// digits after the point than `scale`, and more digits in all than `precision`.
pub fn parse_scaled(text: &str, precision: u8, scale: u8) -> Result<i128, String> {
    let (negative, rest) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text),
    };
    let (int, frac) = rest.split_once('.').unwrap_or((rest, ""));
    let plain = !int.is_empty()
        && int.bytes().all(|b| b.is_ascii_digit())
        && frac.bytes().all(|b| b.is_ascii_digit())
        && !(rest.contains('.') && frac.is_empty());
    if !plain {
        return Err(format!("{text:?} is not a plain decimal number"));
    }
    if frac.len() > usize::from(scale) {
        return Err(format!(
            "{text:?} has more than {scale} digits after the point"
        ));
    }
    let int = int.trim_start_matches('0');
    if int.len() + usize::from(scale) > usize::from(precision) {
        return Err(format!("{text:?} has more than {precision} digits in all"));
    }
    let mut digits = String::from(int);
    digits.push_str(frac);
    digits.extend(std::iter::repeat_n('0', usize::from(scale) - frac.len()));
    let value: i128 = if digits.is_empty() {
        0
    } else {
        digits
            .parse()
            .map_err(|_| format!("{text:?} is too large"))?
    };
    Ok(if negative { -value } else { value })
}

/// The text of a scaled value, always with exactly `scale` digits after the point.
pub fn format_scaled(value: i128, scale: u8) -> String {
    let scale = usize::from(scale);
    let mut digits = value.unsigned_abs().to_string();
    if digits.len() <= scale {
        digits = format!("{}{digits}", "0".repeat(scale + 1 - digits.len()));
    }
    let (int, frac) = digits.split_at(digits.len() - scale);
    let sign = if value < 0 { "-" } else { "" };
    if scale == 0 {
        format!("{sign}{int}")
    } else {
        format!("{sign}{int}.{frac}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_are_scaled_exactly_and_formatted_back() {
        for (text, scale, want) in [
            ("0", 2, 0),
            ("1.5", 2, 150),
            ("-12.50", 2, -1250),
            ("007", 0, 7),
            ("0.000001", 6, 1),
        ] {
            assert_eq!(parse_scaled(text, 38, scale).unwrap(), want, "{text}");
        }
        assert_eq!(format_scaled(150, 2), "1.50");
        assert_eq!(format_scaled(-1250, 2), "-12.50");
        assert_eq!(format_scaled(0, 2), "0.00");
        assert_eq!(format_scaled(7, 0), "7");
        assert_eq!(format_scaled(1, 6), "0.000001");
    }

    #[test]
    fn anything_that_is_not_a_plain_fitting_decimal_is_refused() {
        for (text, precision, scale) in [
            ("", 10, 2),
            ("-", 10, 2),
            ("1.", 10, 2),
            (".5", 10, 2),
            ("1e3", 10, 2),
            (" 1", 10, 2),
            ("+1", 10, 2),
            ("1.234", 10, 2),
            ("10000.00", 6, 2),
        ] {
            assert!(parse_scaled(text, precision, scale).is_err(), "{text:?}");
        }
        assert_eq!(parse_scaled("9999.99", 6, 2).unwrap(), 999_999);
    }

    #[test]
    fn the_widest_decimal_fits() {
        let nines = "9".repeat(38);
        assert!(parse_scaled(&nines, 38, 0).is_ok());
    }
}
