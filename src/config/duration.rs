//! Strict parsing of Go-style duration strings ("500ms", "5s", "1m30s", "1h").
//!
//! Durations in korgi.toml are passed to Docker (as nanoseconds) and to Traefik
//! (verbatim), both of which use Go duration semantics. Anything that does not
//! parse cleanly is rejected instead of silently falling back to a default.

use std::time::Duration;

/// Parse a duration such as `"500ms"`, `"5s"`, `"1m30s"`, `"1.5h"` or `"0"`.
///
/// Supported units: `ns`, `us` (or `µs`), `ms`, `s`, `m`, `h`. Every number must
/// carry a unit, except the bare value `"0"`.
pub fn parse_duration(input: &str) -> anyhow::Result<Duration> {
    let s = input.trim();
    if s.is_empty() {
        anyhow::bail!("empty duration");
    }
    if s == "0" {
        return Ok(Duration::ZERO);
    }

    let mut total_ns: u128 = 0;
    let mut rest = s;
    while !rest.is_empty() {
        let num_len = rest
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .unwrap_or(rest.len());
        if num_len == 0 {
            anyhow::bail!("invalid duration '{}': expected a number", input);
        }
        let (num_str, after_num) = rest.split_at(num_len);
        let unit_len = after_num
            .find(|c: char| c.is_ascii_digit() || c == '.')
            .unwrap_or(after_num.len());
        let (unit, after_unit) = after_num.split_at(unit_len);

        let unit_ns: u128 = match unit {
            "ns" => 1,
            "us" | "µs" => 1_000,
            "ms" => 1_000_000,
            "s" => 1_000_000_000,
            "m" => 60 * 1_000_000_000,
            "h" => 3_600 * 1_000_000_000,
            "" => anyhow::bail!(
                "invalid duration '{}': missing unit (use e.g. \"5s\", \"500ms\", \"1m30s\")",
                input
            ),
            other => anyhow::bail!("invalid duration '{}': unknown unit '{}'", input, other),
        };

        let (int_part, frac_part) = match num_str.split_once('.') {
            Some((i, f)) => (i, f),
            None => (num_str, ""),
        };
        if int_part.is_empty() && frac_part.is_empty() || frac_part.contains('.') {
            anyhow::bail!("invalid duration '{}': bad number '{}'", input, num_str);
        }
        let int_val: u128 = if int_part.is_empty() {
            0
        } else {
            int_part
                .parse()
                .map_err(|_| anyhow::anyhow!("invalid duration '{}': bad number", input))?
        };
        let mut component = int_val
            .checked_mul(unit_ns)
            .ok_or_else(|| anyhow::anyhow!("duration '{}' is too large", input))?;
        if !frac_part.is_empty() {
            // Fractional part, truncated to nanosecond precision.
            let digits: String = frac_part.chars().take(18).collect();
            let frac_val: u128 = digits
                .parse()
                .map_err(|_| anyhow::anyhow!("invalid duration '{}': bad number", input))?;
            let scale = 10u128.pow(digits.len() as u32);
            component += frac_val * unit_ns / scale;
        }
        total_ns = total_ns
            .checked_add(component)
            .ok_or_else(|| anyhow::anyhow!("duration '{}' is too large", input))?;
        rest = after_unit;
    }

    let secs = u64::try_from(total_ns / 1_000_000_000)
        .map_err(|_| anyhow::anyhow!("duration '{}' is too large", input))?;
    Ok(Duration::new(secs, (total_ns % 1_000_000_000) as u32))
}

/// Parse a duration and convert it to Docker's nanosecond representation.
pub fn parse_duration_ns(input: &str) -> anyhow::Result<i64> {
    let d = parse_duration(input)?;
    i64::try_from(d.as_nanos()).map_err(|_| anyhow::anyhow!("duration '{}' is too large", input))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_simple_units() {
        assert_eq!(parse_duration("5s").unwrap(), Duration::from_secs(5));
        assert_eq!(parse_duration("500ms").unwrap(), Duration::from_millis(500));
        assert_eq!(parse_duration("1m").unwrap(), Duration::from_secs(60));
        assert_eq!(parse_duration("1h").unwrap(), Duration::from_secs(3600));
        assert_eq!(parse_duration("250us").unwrap(), Duration::from_micros(250));
        assert_eq!(parse_duration("7ns").unwrap(), Duration::from_nanos(7));
    }

    #[test]
    fn test_compound_and_fractional() {
        assert_eq!(parse_duration("1m30s").unwrap(), Duration::from_secs(90));
        assert_eq!(parse_duration("1h2m3s").unwrap(), Duration::from_secs(3723));
        assert_eq!(parse_duration("1.5s").unwrap(), Duration::from_millis(1500));
        assert_eq!(parse_duration(".5s").unwrap(), Duration::from_millis(500));
        assert_eq!(
            parse_duration("2s500ms").unwrap(),
            Duration::from_millis(2500)
        );
    }

    #[test]
    fn test_zero_and_whitespace() {
        assert_eq!(parse_duration("0").unwrap(), Duration::ZERO);
        assert_eq!(parse_duration("0s").unwrap(), Duration::ZERO);
        assert_eq!(parse_duration(" 5s ").unwrap(), Duration::from_secs(5));
    }

    #[test]
    fn test_rejects_invalid() {
        for bad in [
            "", "5", "10", "s", "5x", "5 s", "abc", "1..5s", "-5s", "5s5", "1.2.3s",
        ] {
            assert!(parse_duration(bad).is_err(), "'{}' should be rejected", bad);
        }
    }

    #[test]
    fn test_parse_duration_ns() {
        assert_eq!(parse_duration_ns("5s").unwrap(), 5_000_000_000);
        assert_eq!(parse_duration_ns("500ms").unwrap(), 500_000_000);
        assert_eq!(parse_duration_ns("1m30s").unwrap(), 90_000_000_000);
        assert!(parse_duration_ns("500").is_err());
    }
}
