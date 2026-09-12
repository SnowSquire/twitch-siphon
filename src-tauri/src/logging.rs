use std::time::{SystemTime, UNIX_EPOCH};

/// Backend log helper. Routes through the `log` crate so records fan out
/// to the tauri-plugin-log targets (stdout + webview console).
/// `module` becomes the log target, e.g. `log::info!(target: "hermes", …)`.
pub fn log(module: &str, message: impl std::fmt::Display) {
    log::info!(target: module, "{message}");
}

/// RFC 3339 UTC timestamp, matching the TS client's `toISOString()` output.
pub fn timestamp() -> String {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let (year, month, day) = civil_from_days((elapsed.as_secs() / 86_400) as i64);
    let rem = elapsed.as_secs() % 86_400;
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60,
        elapsed.subsec_millis()
    )
}

/// Howard Hinnant's `civil_from_days` (days since 1970-01-01 -> y/m/d).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// Howard Hinnant's `days_from_civil` (y/m/d -> days since 1970-01-01).
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let y = year - i64::from(month <= 2);
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * i64::from(mp) + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Milliseconds since the unix epoch for an ISO 8601 UTC timestamp like
/// `2026-09-10T18:35:06Z`; fractional seconds are accepted and truncated
/// to millisecond precision.
#[must_use]
pub fn parse_iso_ms(raw: &str) -> Option<i64> {
    let bytes = raw.as_bytes();
    if bytes.len() < 19
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || (bytes[10] != b'T' && bytes[10] != b't')
        || bytes[13] != b':'
        || bytes[16] != b':'
    {
        return None;
    }
    let num = |range: std::ops::Range<usize>| raw.get(range)?.parse::<i64>().ok();
    let year = num(0..4)?;
    let month = num(5..7)?;
    let day = num(8..10)?;
    let hour = num(11..13)?;
    let minute = num(14..16)?;
    let second = num(17..19)?;
    let millis = if bytes.len() > 19 && bytes[19] == b'.' {
        let end = bytes[20..]
            .iter()
            .position(|byte| !byte.is_ascii_digit())
            .map_or(bytes.len(), |offset| 20 + offset);
        let digits = raw.get(20..end)?;
        let leading = &digits[..digits.len().min(3)];
        let mut millis = leading.parse::<i64>().ok()?;
        for _ in leading.len()..3 {
            millis *= 10;
        }
        millis
    } else {
        0
    };
    Some(
        days_from_civil(year, month as u32, day as u32) * 86_400_000
            + hour * 3_600_000
            + minute * 60_000
            + second * 1000
            + millis,
    )
}
