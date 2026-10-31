//! Timestamps are stored and transmitted as fixed-width UTC strings with
//! millisecond precision, e.g. `2026-10-31T17:02:03.456Z`. Fixed width keeps
//! lexical order equal to chronological order, which the store relies on.

use time::format_description::BorrowedFormatItem;
use time::macros::format_description;
use time::{Duration, OffsetDateTime};

const FORMAT: &[BorrowedFormatItem<'static>] = format_description!(
    "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z"
);

pub fn now() -> String {
    format(OffsetDateTime::now_utc())
}

pub fn format(t: OffsetDateTime) -> String {
    t.format(FORMAT).expect("fixed format cannot fail")
}

pub fn parse(s: &str) -> Option<OffsetDateTime> {
    time::PrimitiveDateTime::parse(s, FORMAT)
        .ok()
        .map(|p| p.assume_utc())
}

/// Timestamp `secs` seconds before now, in the same format.
pub fn ago(secs: u64) -> String {
    format(OffsetDateTime::now_utc() - Duration::seconds(secs as i64))
}

/// Seconds between two timestamps, if both parse.
pub fn seconds_between(start: &str, end: &str) -> Option<f64> {
    let (a, b) = (parse(start)?, parse(end)?);
    Some((b - a).as_seconds_f64())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_sorts() {
        let a = now();
        assert_eq!(a.len(), 24);
        let t = parse(&a).unwrap();
        assert_eq!(format(t), a);
        let earlier = ago(10);
        assert!(earlier < a);
        assert!((seconds_between(&earlier, &a).unwrap() - 10.0).abs() < 0.1);
    }
}
