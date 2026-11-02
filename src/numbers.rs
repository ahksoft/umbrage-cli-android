//! Parse user-supplied numeric literals, accepting hex (`0x...`) or decimal.

use anyhow::{bail, Result};

/// Parse a number that may be hex-prefixed or decimal, into `u64`.
///
/// Underscores are accepted and ignored, so datasheet-style literals like
/// `0x1000_0000` paste cleanly.
pub fn parse_u64(input: &str) -> Result<u64> {
    let s = input.trim();
    if s.is_empty() {
        bail!("empty number");
    }
    let cleaned: String = s.chars().filter(|c| *c != '_').collect();
    let parsed = if let Some(hex) = cleaned.strip_prefix("0x").or_else(|| cleaned.strip_prefix("0X"))
    {
        u64::from_str_radix(hex, 16)
            .map_err(|_| anyhow::anyhow!("{input:?} is not valid hexadecimal (expected 0x...)"))?
    } else {
        // No 0x prefix means decimal. Deliberately *not* auto-detected as hex:
        // silently reinterpreting an address would be dangerous on a tool that
        // writes to arbitrary memory.
        cleaned
            .parse::<u64>()
            .map_err(|_| anyhow::anyhow!("{input:?} is not a valid decimal number"))?
    };
    Ok(parsed)
}

/// Parse into `usize`, rejecting values that do not fit.
pub fn parse_usize(input: &str) -> Result<usize> {
    let v = parse_u64(input)?;
    usize::try_from(v).map_err(|_| {
        anyhow::anyhow!("value {input:?} ({v}) does not fit in usize on this platform")
    })
}

/// Parse into `u32`.
pub fn parse_u32(input: &str) -> Result<u32> {
    let v = parse_u64(input)?;
    u32::try_from(v).map_err(|_| anyhow::anyhow!("value {input:?} ({v}) does not fit in u32"))
}

/// Render a byte count in human-readable form.
pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} {}", UNITS[unit])
    } else {
        format!("{value:.2} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_and_decimal() {
        assert_eq!(parse_u64("0x1000").unwrap(), 4096);
        assert_eq!(parse_u64("1000").unwrap(), 1000);
        assert_eq!(parse_u64("0Xff").unwrap(), 255);
        // Leading zeros must not be read as octal.
        assert_eq!(parse_u64("0755").unwrap(), 755);
        // Bare digits are decimal, never hex.
        assert_eq!(parse_u64("20000000").unwrap(), 20000000);
        // 64-bit addresses must survive the round-trip.
        assert_eq!(parse_u64("0xffffffffffffffff").unwrap(), u64::MAX);
    }

    #[test]
    fn underscore_grouping() {
        // Datasheet-style literals paste cleanly.
        assert_eq!(parse_u64("0x1000_0000").unwrap(), 0x1000_0000);
        assert_eq!(parse_u64("0xdead_beef").unwrap(), 0xdead_beef);
        assert_eq!(parse_u64("1_000_000").unwrap(), 1_000_000);
        // Underscores are cosmetic, so they must not enable a shorter match
        // to sneak past the u32 bound.
        assert_eq!(parse_u32("0xffff_ffff").unwrap(), u32::MAX);
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse_u64("").is_err());
        assert!(parse_u64("xyz").is_err());
        // 0x100000000 exceeds u32 by one; this asserts the *overflow* check,
        // not the parse check.
        assert!(parse_u32("0x1_0000_0000").is_err());
        // No 0x prefix means decimal, so this overflows rather than parsing.
        assert!(parse_u32("4294967296").is_err());
        // A trailing underscore is unambiguous once stripped, so it is
        // deliberately tolerated rather than rejected.
        assert_eq!(parse_u64("0x1000_").unwrap(), 0x1000);
        // The dangerous case is a *malformed* hex digit, which must not be
        // silently truncated or reinterpreted.
        assert!(parse_u64("0x1000_g").is_err());
    }

    #[test]
    fn errors_name_the_input() {
        let msg = parse_u64("0xZZZZ").unwrap_err().to_string();
        assert!(msg.contains("0xZZZZ"), "message should quote input: {msg}");
    }

    #[test]
    fn sizes() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(4096), "4.00 KiB");
        assert_eq!(human_bytes(1024 * 1024 * 3), "3.00 MiB");
    }
}
