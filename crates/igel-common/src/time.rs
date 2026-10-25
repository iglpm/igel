//! RFC 3339 UTC timestamps without a date-time dependency.

pub fn format_rfc3339(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

pub fn parse_rfc3339(s: &str) -> Option<u64> {
    let s = s.trim();
    let (date, rest) = s.split_once(['T', 't', ' '])?;
    let mut d = date.split('-');
    let y: i64 = d.next()?.parse().ok()?;
    let m: u32 = d.next()?.parse().ok()?;
    let day: u32 = d.next()?.parse().ok()?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&day) || rest.len() < 8 {
        return None;
    }
    let (clock, zone) = rest.split_at(8);
    let mut c = clock.split(':');
    let hh: i64 = c.next()?.parse().ok()?;
    let mm: i64 = c.next()?.parse().ok()?;
    let ss: i64 = c.next()?.parse().ok()?;
    let zone = zone.trim_start_matches(|ch: char| ch == '.' || ch.is_ascii_digit());
    let offset = match zone {
        "Z" | "z" | "" => 0,
        z if z.len() == 6 && (z.starts_with('+') || z.starts_with('-')) => {
            let sign = if z.starts_with('-') { -1 } else { 1 };
            let oh: i64 = z[1..3].parse().ok()?;
            let om: i64 = z[4..6].parse().ok()?;
            sign * (oh * 3600 + om * 60)
        }
        _ => return None,
    };
    let days = days_from_civil(y, m, day);
    let secs = days * 86_400 + hh * 3600 + mm * 60 + ss - offset;
    u64::try_from(secs).ok()
}

pub fn parse_duration(s: &str) -> Option<u64> {
    let s = s.trim();
    let split = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let (n, unit) = s.split_at(split);
    let n: u64 = n.parse().ok()?;
    let factor = match unit.trim() {
        "" | "s" => 1,
        "m" | "min" => 60,
        "h" => 3600,
        "d" | "days" | "day" => 86_400,
        "w" | "weeks" | "week" => 7 * 86_400,
        _ => return None,
    };
    n.checked_mul(factor)
}

/// Howard Hinnant's `civil_from_days`.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Howard Hinnant's `days_from_civil`.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = if m > 2 { m - 3 } else { m + 9 } as i64;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_and_parses_round_trip() {
        for secs in [0, 951_782_400, 1_790_596_799, 4_102_444_800] {
            assert_eq!(parse_rfc3339(&format_rfc3339(secs)), Some(secs));
        }
        assert_eq!(format_rfc3339(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(parse_rfc3339("2026-03-11T00:20:04Z"), Some(1_773_188_404));
        assert_eq!(
            parse_rfc3339("2026-03-11T02:20:04.123+02:00"),
            Some(1_773_188_404)
        );
        assert_eq!(parse_rfc3339("yesterday"), None);
    }

    #[test]
    fn durations_take_common_units() {
        assert_eq!(parse_duration("14d"), Some(14 * 86_400));
        assert_eq!(parse_duration("72h"), Some(72 * 3600));
        assert_eq!(parse_duration("2w"), Some(14 * 86_400));
        assert_eq!(parse_duration("90"), Some(90));
        assert_eq!(parse_duration("soon"), None);
    }
}
