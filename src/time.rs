//! Parsing `--since` and record timestamps.

use chrono::{DateTime, Local, NaiveDate, TimeDelta, TimeZone, Utc};

/// Parses `--since`: `7d`, `24h`, a date (local midnight) or an RFC 3339 time.
///
/// # Errors
/// Returns a message naming the accepted forms when `s` matches none of them.
pub fn parse_since(s: &str, now: DateTime<Utc>) -> Result<DateTime<Utc>, String> {
    let t = s.trim();
    let bad = || format!("--since: expected 7d, 24h, YYYY-MM-DD or an RFC 3339 time, got '{t}'");
    let since = if let Some(delta) = relative(t) {
        now.checked_sub_signed(delta).ok_or_else(bad)?
    } else if let Ok(date) = NaiveDate::parse_from_str(t, "%Y-%m-%d") {
        let midnight = date.and_hms_opt(0, 0, 0).ok_or_else(bad)?;
        Local
            .from_local_datetime(&midnight)
            .earliest()
            .map(|d| d.with_timezone(&Utc))
            .ok_or_else(bad)?
    } else {
        parse_ts(t).ok_or_else(bad)?
    };
    if since < DateTime::UNIX_EPOCH {
        return Err(format!("--since: '{t}' is before 1970"));
    }
    Ok(since)
}

fn relative(t: &str) -> Option<TimeDelta> {
    let (num, unit) = t.split_at(t.char_indices().last()?.0);
    if !num.starts_with(|c: char| c.is_ascii_digit()) {
        return None;
    }
    let n: u32 = num.parse().ok()?;
    match unit {
        "d" => TimeDelta::try_days(i64::from(n)),
        "h" => TimeDelta::try_hours(i64::from(n)),
        _ => None,
    }
}

/// Parses an RFC 3339 timestamp, as both agents write them.
#[must_use]
pub fn parse_ts(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> DateTime<Utc> {
        parse_ts("2026-11-08T00:00:00Z").unwrap()
    }

    #[test]
    fn relative_days_and_hours() {
        assert_eq!(
            parse_since("7d", now()),
            parse_ts("2026-11-01T00:00:00Z").ok_or(String::new())
        );
        assert_eq!(
            parse_since("24h", now()),
            parse_ts("2026-11-07T00:00:00Z").ok_or(String::new())
        );
    }

    #[test]
    fn rfc3339_with_offset() {
        let t = parse_since("2026-11-08T10:00:00+11:00", now()).unwrap();
        assert_eq!(t, parse_ts("2026-11-07T23:00:00Z").unwrap());
    }

    #[test]
    fn date_is_local_midnight() {
        let t = parse_since("2026-11-01", now()).unwrap();
        assert_eq!(
            t.with_timezone(&Local).format("%Y-%m-%d %H:%M").to_string(),
            "2026-11-01 00:00"
        );
    }

    /// Which timestamp forms parse, and to what instant.
    #[test]
    fn timestamp_forms() {
        let z = parse_ts("2026-11-08T00:00:00Z");
        assert!(z.is_some());
        for s in [
            "2026-11-08T00:00:00z",
            "2026-11-08t00:00:00Z",
            "2026-11-08 00:00:00Z",
            "2026-11-08T11:00:00+11:00",
            "2026-11-07T14:00:00-10:00",
            "2026-11-08T00:00:00-00:00",
            "2026-11-08T00:00:00.000Z",
            "2026-11-08T00:00:00.000000000Z",
        ] {
            assert_eq!(parse_ts(s), z, "{s}");
        }
        assert_eq!(
            parse_ts("2026-11-08T00:00:00.123Z"),
            parse_ts("2026-11-08T00:00:00.123000000Z")
        );
        assert!(parse_ts("2026-11-08T00:00:00.1Z") > z);
        assert!(parse_ts("2026-11-08T00:00:00.000000001Z") > z);
        for s in [
            "",
            "2026-11-08",
            "2026-11-08T00:00:00",
            "2026-11-08T00:00Z",
            "2026-02-30T00:00:00Z",
            "2026-11-08T24:00:00Z",
            "2026-11-08T00:00:00+24:00",
            "20261108T000000Z",
            " 2026-11-08T00:00:00Z",
            "2026-11-08T00:00:00Z ",
            "2026-11-08T00:00:00Z[UTC]",
            "2026-11-08T00:00:00,5Z",
        ] {
            assert_eq!(parse_ts(s), None, "{s}");
        }
    }

    #[test]
    fn since_forms_and_messages() {
        assert_eq!(parse_since(" 7d ", now()), parse_since("7d", now()));
        assert_eq!(parse_since("07d", now()), parse_since("7d", now()));
        assert_eq!(parse_since("0h", now()), Ok(now()));
        assert_eq!(
            parse_since("2026-11-08T00:00:00Z", now()),
            parse_ts("2026-11-08T00:00:00Z").ok_or(String::new())
        );
        assert_eq!(
            parse_since("2026-1-1", now()),
            parse_since("2026-01-01", now())
        );
        assert_eq!(
            parse_since("200000d", now()),
            Err("--since: '200000d' is before 1970".to_owned())
        );
        assert_eq!(
            parse_since("1969-12-31T23:59:59Z", now()),
            Err("--since: '1969-12-31T23:59:59Z' is before 1970".to_owned())
        );
        assert_eq!(
            parse_since("1970-01-01T00:00:00Z", now()),
            parse_ts("1970-01-01T00:00:00Z").ok_or(String::new())
        );
        assert_eq!(
            parse_since("7w", now()),
            Err("--since: expected 7d, 24h, YYYY-MM-DD or an RFC 3339 time, got '7w'".to_owned())
        );
    }

    #[test]
    fn rejects_other_forms() {
        for s in [
            "",
            "d",
            "-3d",
            "+7d",
            "7w",
            "yesterday",
            "2026-13-01",
            "1500-01-01",
            "200000d",
        ] {
            assert!(parse_since(s, now()).is_err(), "{s}");
        }
    }
}
