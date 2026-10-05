//! Numbers as people read them.

/// `x` with at most `digits` significant digits, thousands grouped with
/// commas, trailing zeros dropped, and very large or small numbers written
/// as `1.5 × 10²¹`. `None` for infinities and NaN.
pub fn format_number(x: f64, digits: usize) -> Option<String> {
    if !x.is_finite() {
        return None;
    }
    let digits = digits.clamp(1, 15);
    if x == 0.0 {
        return Some("0".to_string());
    }
    let magnitude = x.abs().log10().floor() as i32;
    if !(-6..15).contains(&magnitude) {
        // Rust's own exponent form: dividing by 10^magnitude overflows to
        // infinity for subnormals, and its rounding carries 9.99… to 1e+1.
        let sci = format!("{:.*e}", digits.saturating_sub(1).min(8), x);
        let (mantissa, exponent) = sci.split_once('e')?;
        let magnitude: i32 = exponent.parse().ok()?;
        let mantissa = trim(mantissa);
        return Some(format!("{mantissa} × 10{}", superscript(magnitude)));
    }
    let decimals = (digits as i32 - 1 - magnitude).clamp(0, 15) as usize;
    let text = trim(&format!("{x:.decimals$}"));
    let text = if text == "-0" { "0".to_string() } else { text };
    Some(group(&text))
}

/// `text`, a number, without trailing zeros after its point.
fn trim(text: &str) -> String {
    if text.contains('.') {
        text.trim_end_matches('0').trim_end_matches('.').to_string()
    } else {
        text.to_string()
    }
}

/// `text`, a number, with commas between thousands of its whole part.
fn group(text: &str) -> String {
    let (sign, rest) = match text.strip_prefix('-') {
        Some(rest) => ("-", rest),
        None => ("", text),
    };
    let (whole, fraction) = match rest.split_once('.') {
        Some((whole, fraction)) => (whole, Some(fraction)),
        None => (rest, None),
    };
    let mut grouped = String::new();
    for (i, c) in whole.chars().enumerate() {
        if i > 0 && (whole.len() - i) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(c);
    }
    match fraction {
        Some(fraction) => format!("{sign}{grouped}.{fraction}"),
        None => format!("{sign}{grouped}"),
    }
}

fn superscript(n: i32) -> String {
    n.to_string()
        .chars()
        .map(|c| match c {
            '-' => '⁻',
            '0' => '⁰',
            '1' => '¹',
            '2' => '²',
            '3' => '³',
            '4' => '⁴',
            '5' => '⁵',
            '6' => '⁶',
            '7' => '⁷',
            '8' => '⁸',
            _ => '⁹',
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::format_number as f;

    #[test]
    fn formats_numbers() {
        assert_eq!(f(84.0, 12).unwrap(), "84");
        assert_eq!(f(1234567.0, 12).unwrap(), "1,234,567");
        assert_eq!(f(-1234.5, 12).unwrap(), "-1,234.5");
        assert_eq!(f(0.1 + 0.2, 12).unwrap(), "0.3");
        assert_eq!(f(2f64.sqrt(), 12).unwrap(), "1.41421356237");
        assert_eq!(f(6.213711922, 6).unwrap(), "6.21371");
        assert_eq!(f(1.5e21, 12).unwrap(), "1.5 × 10²¹");
        assert_eq!(f(2.5e-9, 12).unwrap(), "2.5 × 10⁻⁹");
        assert_eq!(f(0.0, 12).unwrap(), "0");
        assert_eq!(f(f64::INFINITY, 12), None);
        assert_eq!(f(0.000123, 6).unwrap(), "0.000123");
        assert_eq!(f(9.9999999999e20, 6).unwrap(), "1 × 10²¹");
    }

    #[test]
    fn formats_subnormals() {
        // Subnormals keep few bits, so 1e-320 is really 9.99988867e-321.
        assert_eq!(f(1e-320, 12).unwrap(), "9.99988867 × 10⁻³²¹");
        assert_eq!(f(-5e-324, 3).unwrap(), "-4.94 × 10⁻³²⁴");
    }
}
