//! Parsing `--since` and record timestamps.

use jiff::civil::Date;
use jiff::tz::TimeZone;
use jiff::{SignedDuration, Timestamp};

/// Parses `--since`: `7d`, `24h`, a date (local midnight) or an RFC 3339 time. `None` is a
/// local midnight past the last instant jiff holds (9999-12-30T22:00Z), which no record
/// can be at or after.
///
/// # Errors
/// Returns a message naming the accepted forms when `s` matches none of them.
pub fn parse_since(s: &str, now: Timestamp) -> Result<Option<Timestamp>, String> {
    let t = s.trim();
    let bad = || format!("--since: expected 7d, 24h, YYYY-MM-DD or an RFC 3339 time, got '{t}'");
    let before_1970 = || format!("--since: '{t}' is before 1970");
    let since = if let Some(delta) = relative(t) {
        now.checked_sub(delta).map_err(|_| before_1970())?
    } else if let Ok(date) = Date::strptime("%Y-%m-%d", t) {
        // A repeated midnight is the first; a skipped one is when the clocks resume.
        match date.to_zoned_datetime(TimeZone::system()) {
            Ok(midnight) => midnight.timestamp(),
            Err(_) if date.year() > 1970 => return Ok(None),
            Err(_) => return Err(before_1970()),
        }
    } else {
        parse_ts(t).ok_or_else(bad)?
    };
    if since < Timestamp::UNIX_EPOCH {
        return Err(before_1970());
    }
    Ok(Some(since))
}

fn relative(t: &str) -> Option<SignedDuration> {
    let (num, unit) = t.split_at(t.char_indices().last()?.0);
    if !num.starts_with(|c: char| c.is_ascii_digit()) {
        return None;
    }
    let hours = i64::from(num.parse::<u32>().ok()?);
    match unit {
        "d" => Some(SignedDuration::from_hours(hours * 24)),
        "h" => Some(SignedDuration::from_hours(hours)),
        _ => None,
    }
}

/// Parses an RFC 3339 timestamp, as both agents write them.
#[must_use]
pub fn parse_ts(s: &str) -> Option<Timestamp> {
    rfc3339(s.as_bytes()).then(|| s.parse().ok()).flatten()
}

/// Whether `b` has RFC 3339's layout: `YYYY-MM-DDTHH:MM:SS` (`T`, `t` or a space), an
/// optional fraction, then `Z`, `z` or an offset `+HH:MM` or `-HH:MM` under 24 hours.
/// jiff's parser alone also takes other ISO 8601 forms, such as no seconds or `+0000`.
fn rfc3339(b: &[u8]) -> bool {
    let digits =
        |r: std::ops::Range<usize>| b.get(r).is_some_and(|d| d.iter().all(u8::is_ascii_digit));
    let at = |i: usize, c: &[u8]| b.get(i).is_some_and(|x| c.contains(x));
    let layout = digits(0..4)
        && at(4, b"-")
        && digits(5..7)
        && at(7, b"-")
        && digits(8..10)
        && at(10, b"Tt ")
        && digits(11..13)
        && at(13, b":")
        && digits(14..16)
        && at(16, b":")
        && digits(17..19);
    if !layout {
        return false;
    }
    let mut rest = &b[19..];
    if let Some(fraction) = rest.strip_prefix(b".") {
        let n = fraction.iter().take_while(|c| c.is_ascii_digit()).count();
        if n == 0 {
            return false;
        }
        rest = &fraction[n..];
    }
    match rest {
        b"Z" | b"z" => true,
        [b'+' | b'-', h1, h2, b':', m1, m2] => {
            [h1, h2, m1, m2].iter().all(|c| c.is_ascii_digit()) && [*h1, *h2] < *b"24" && *m1 < b'6'
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> Timestamp {
        parse_ts("2026-11-08T00:00:00Z").unwrap()
    }

    #[test]
    fn relative_days_and_hours() {
        assert_eq!(
            parse_since("7d", now()),
            Ok(parse_ts("2026-11-01T00:00:00Z"))
        );
        assert_eq!(
            parse_since("24h", now()),
            Ok(parse_ts("2026-11-07T00:00:00Z"))
        );
    }

    #[test]
    fn the_last_instant_written_out_is_a_window_start() {
        assert_eq!(
            parse_since("9999-12-30T22:00:00.999999999Z", now()),
            Ok(Some(Timestamp::MAX))
        );
    }

    #[test]
    fn rfc3339_with_offset() {
        let t = parse_since("2026-11-08T10:00:00+11:00", now());
        assert_eq!(t, Ok(parse_ts("2026-11-07T23:00:00Z")));
    }

    #[test]
    fn date_is_local_midnight() {
        let t = parse_since("2026-11-01", now()).unwrap().unwrap();
        assert_eq!(
            t.to_zoned_datetime(TimeZone::system())
                .strftime("%Y-%m-%d %H:%M")
                .to_string(),
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
            "2026-11-08T00:00:00+0000",
            "2026-11-08T00:00:00+00",
            "2026-11-08T00:00:00+00:00:30",
            "+002026-11-08T00:00:00Z",
        ] {
            assert_eq!(parse_ts(s), None, "{s}");
        }
    }

    #[test]
    fn since_forms_and_messages() {
        assert_eq!(parse_since(" 7d ", now()), parse_since("7d", now()));
        assert_eq!(parse_since("07d", now()), parse_since("7d", now()));
        assert_eq!(parse_since("0h", now()), Ok(Some(now())));
        assert_eq!(
            parse_since("2026-11-08T00:00:00Z", now()),
            Ok(parse_ts("2026-11-08T00:00:00Z"))
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
            Ok(parse_ts("1970-01-01T00:00:00Z"))
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
