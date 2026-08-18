//! Port of `utils/timeutils/time.go` — 29 lines, and two traps.
//!
//! Ported alongside `job.go`, which is its only consumer in the model package: `Job`'s YAML codec
//! stores the three timestamps as formatted strings rather than as integers.
//!
//! # 1. It reads the server's timezone
//!
//! ```text
//! func FormatMillis(millis int64) string {
//!     return time.UnixMilli(millis).Format(RFC3339Milli)
//! }
//! ```
//!
//! `time.UnixMilli` attaches `time.Local`, and the layout ends in `Z07:00`, so the **offset in the
//! output is the server's**. Two Mattermost servers in different zones format the same instant
//! differently — this is [D-008] again, in a second place. Reproduced faithfully with
//! `chrono::Local`; the oracle records the zone it ran under so the test can rebuild the instant
//! in that zone rather than assuming one.
//!
//! # 2. Go's `.999` elides trailing zeros — and the decimal point with them
//!
//! Unlike `.000`, which pads. Measured:
//!
//! | millis | Go |
//! |---|---|
//! | `…000` | `2023-11-15T03:43:20+05:30` — **no fractional part at all** |
//! | `…100` | `2023-11-15T03:43:20.1+05:30` |
//! | `…010` | `2023-11-15T03:43:20.01+05:30` |
//! | `…123` | `2023-11-15T03:43:20.123+05:30` |
//!
//! chrono's `%.3f` always emits three digits and `%.f` emits nanoseconds, so neither is
//! substitutable and the fraction is assembled by hand.
//!
//! # And one asymmetry
//!
//! `FormatMillis` will happily render a five-digit year, which `ParseFormatedMillis` then cannot
//! read — so the round trip is **not** total. Measured at `253402300799999`.

use chrono::{DateTime, Datelike, Local, Offset, TimeZone, Timelike};

/// Port of `timeutils.RFC3339Milli` (time.go:11).
///
/// A **Go** layout string, kept for the doc comments and the oracle's constant check. It is not a
/// chrono format string and must not be handed to one.
pub const RFC3339_MILLI: &str = "2006-01-02T15:04:05.999Z07:00";

/// Port of `timeutils.FormatMillis` (time.go:14).
///
/// Renders in the **server's local timezone** — see the module docs.
///
/// Assembled by hand rather than with a chrono format string, for three reasons the corpus pins:
/// the fractional part elides trailing zeros, the offset is `Z` when zero, and Go prints a
/// five-digit year bare where chrono's `%Y` would prefix it with `+`.
pub fn format_millis(millis: i64) -> String {
    format_millis_in(millis, &Local)
}

/// [`format_millis`] against an explicit timezone.
///
/// Exists so the parity tests can drive the **production** code path in the zone the fixture was
/// generated under, and again at a zero offset. An earlier version of those tests reassembled the
/// string from the private helpers instead, and a mutation to `format_offset` slipped through both
/// of them — the branch that emits `Z`, which is the one a UTC server takes.
pub fn format_millis_in<Tz: TimeZone>(millis: i64, tz: &Tz) -> String {
    let Some(dt) = tz.timestamp_millis_opt(millis).single() else {
        // Unrepresentable instants. Go's `time.UnixMilli` saturates rather than failing, but no
        // caller can reach this: `i64` milliseconds span ±292 million years and chrono's range is
        // wider than the corpus needs.
        return String::new();
    };

    format!(
        "{}-{:02}-{:02}T{:02}:{:02}:{:02}{}{}",
        format_year(dt.year()),
        dt.month(),
        dt.day(),
        dt.hour(),
        dt.minute(),
        dt.second(),
        format_fraction(dt.timestamp_subsec_millis()),
        format_offset(&dt),
    )
}

/// Go's `2006` verb: at least four digits, zero-padded, with the sign outside the padding.
///
/// `{:04}` alone would render year −1 as `-001`; Go renders `-0001`.
fn format_year(year: i32) -> String {
    if year < 0 {
        format!("-{:04}", -(year as i64))
    } else {
        format!("{year:04}")
    }
}

/// Go's `.999`: up to three digits, trailing zeros removed, **and the point removed** when the
/// fraction is zero.
fn format_fraction(millis: u32) -> String {
    if millis == 0 {
        return String::new();
    }
    let digits = format!("{millis:03}");
    format!(".{}", digits.trim_end_matches('0'))
}

/// Go's `Z07:00`: the literal `Z` for a zero offset, otherwise `±hh:mm`.
fn format_offset<Tz: TimeZone>(dt: &DateTime<Tz>) -> String {
    let total = dt.offset().fix().local_minus_utc();
    if total == 0 {
        return "Z".to_owned();
    }
    let sign = if total < 0 { '-' } else { '+' };
    let abs = total.abs();
    format!("{sign}{:02}:{:02}", abs / 3600, (abs % 3600) / 60)
}

/// Port of `timeutils.ParseFormatedMillis` (time.go:18). Go's spelling of "formatted" is kept.
///
/// An **empty string is not an error** — it returns zero, by an explicit early return that
/// predates the layout. Everything else must carry a date, a `T`, a time and an offset; the
/// fractional part is optional and extra precision is **truncated**, not rounded (`.9999` is 999).
///
/// # Errors
///
/// Any input Go's `time.Parse` rejects. The error *text* is not reproduced — Go's is
/// `parsing time "…" as "…": cannot parse "…" as "…"`, describing its own layout machinery — so
/// this returns a typed error instead and the divergence is logged as [D-118]. The accept/reject
/// verdict and the parsed value are exact.
pub fn parse_formated_millis(s: &str) -> Result<i64, TimeParseError> {
    if s.is_empty() {
        return Ok(0);
    }

    // Go's `Z07:00` accepts a literal `Z`; chrono's `%:z` does not, so normalise first.
    let normalised = match s.strip_suffix(['Z', 'z']) {
        Some(head) => format!("{head}+00:00"),
        None => s.to_owned(),
    };

    let parsed = DateTime::parse_from_str(&normalised, "%Y-%m-%dT%H:%M:%S%.f%:z")
        .map_err(|_| TimeParseError(s.to_owned()))?;

    Ok(parsed.timestamp_millis())
}

/// A rejection from [`parse_formated_millis`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("parsing time {0:?} as {RFC3339_MILLI:?}")]
pub struct TimeParseError(String);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_fraction_elides_trailing_zeros() {
        assert_eq!(format_fraction(0), "");
        assert_eq!(format_fraction(1), ".001");
        assert_eq!(format_fraction(10), ".01");
        assert_eq!(format_fraction(100), ".1");
        assert_eq!(format_fraction(123), ".123");
        assert_eq!(format_fraction(120), ".12");
        assert_eq!(format_fraction(999), ".999");
    }

    #[test]
    fn the_year_is_at_least_four_digits_with_the_sign_outside() {
        assert_eq!(format_year(2023), "2023");
        assert_eq!(format_year(1), "0001");
        assert_eq!(format_year(10000), "10000");
        assert_eq!(format_year(-1), "-0001");
    }

    #[test]
    fn an_empty_string_parses_to_zero_rather_than_failing() {
        assert_eq!(parse_formated_millis(""), Ok(0));
    }

    #[test]
    fn extra_precision_is_truncated() {
        assert_eq!(
            parse_formated_millis("2023-11-14T16:43:20.9999Z"),
            Ok(1_699_980_200_999)
        );
    }

    #[test]
    fn an_offset_is_required() {
        assert!(parse_formated_millis("2023-11-14T16:43:20").is_err());
        assert!(parse_formated_millis("2023-11-14").is_err());
    }
}

#[cfg(test)]
mod go_parity {
    use super::*;
    use chrono::{FixedOffset, Utc};
    use serde_json::Value;
    use std::sync::OnceLock;

    fn oracle() -> &'static Value {
        static ORACLE: OnceLock<Value> = OnceLock::new();
        ORACLE.get_or_init(|| {
            let raw = include_str!("../../../fixtures/behaviour_job.json");
            serde_json::from_str(raw).expect("behaviour_job.json parses")
        })
    }

    #[test]
    fn the_layout_constant_matches_go() {
        assert_eq!(oracle()["constants"]["rfc3339_milli_layout"], RFC3339_MILLI);
    }

    /// `FormatMillis`, rebuilt in the zone the fixture was generated under.
    ///
    /// The output carries the **server's** offset, so asserting `format_millis` directly would
    /// only pass on a machine in the generator's timezone. Instead the instant is rebuilt in the
    /// recorded zone and formatted from there — the treatment [D-008] established.
    #[test]
    fn format_millis_matches_go() {
        let section = &oracle()["format_millis"];
        let offset_seconds = section["offset_seconds"].as_i64().unwrap() as i32;
        let zone = FixedOffset::east_opt(offset_seconds).expect("the recorded offset is valid");

        for case in section["cases"].as_array().unwrap() {
            let millis = case["millis"].as_i64().unwrap();
            let want = case["formatted"].as_str().unwrap();

            assert_eq!(format_millis_in(millis, &zone), want, "millis={millis}");
        }
    }

    /// The same instants at a **zero** offset, where the layout emits a literal `Z`.
    ///
    /// This section exists because a mutation exposed the gap: replacing the `Z` branch with
    /// `+00:00` passed the entire suite. The generator pins a +05:30 zone, so no case in
    /// `format_millis_matches_go` has a zero offset — and UTC is the common deployment, so the
    /// untested branch was the one most servers take.
    ///
    /// It also shows the year-10000 hole is an artefact of the offset: at UTC the same millis are
    /// `9999-12-31T23:59:59.999Z`, four digits, and round-trip cleanly.
    #[test]
    fn format_millis_at_a_zero_offset_emits_z() {
        let cases = oracle()["format_millis"]["utc_cases"].as_array().unwrap();
        assert_eq!(cases.len(), 14);

        for case in cases {
            let millis = case["millis"].as_i64().unwrap();
            let want = case["formatted"].as_str().unwrap();

            assert_eq!(format_millis_in(millis, &Utc), want, "millis={millis}");
            assert!(want.ends_with('Z'), "a zero offset is `Z`, never `+00:00`");
            // And every one of them parses back, unlike the +05:30 rendering of the last case.
            assert_eq!(parse_formated_millis(want), Ok(millis), "millis={millis}");
        }
    }

    /// The round trip is **not** total: a five-digit year formats and will not parse back.
    #[test]
    fn the_round_trip_has_a_hole_at_year_10000() {
        let mut total = 0;
        let mut broken = 0;

        for case in oracle()["format_millis"]["cases"].as_array().unwrap() {
            total += 1;
            let formatted = case["formatted"].as_str().unwrap();
            let round_trips = case["round_trips"].as_bool().unwrap();

            let ours = parse_formated_millis(formatted);
            assert_eq!(
                ours.is_ok()
                    && ours
                        .as_ref()
                        .is_ok_and(|m| *m == case["millis"].as_i64().unwrap()),
                round_trips,
                "millis={}: round trip",
                case["millis"]
            );

            if !round_trips {
                broken += 1;
                assert!(
                    formatted.starts_with("10000-"),
                    "the only non-round-tripping case should be the five-digit year, got {formatted}"
                );
            }
        }

        assert_eq!(total, 14);
        assert_eq!(broken, 1, "exactly one case fails to round trip");
    }

    /// `ParseFormatedMillis` — the verdict and the value, exactly.
    ///
    /// The error **text** is not asserted: Go's describes its own layout machinery
    /// (`cannot parse "" as "Z07:00"`), which would mean reproducing `time.Parse`'s internals for
    /// a string no client sees. See [D-118].
    #[test]
    fn parse_formated_millis_matches_go() {
        let mut rejected = 0;

        for case in oracle()["parse_formated_millis"].as_array().unwrap() {
            let input = case["input"].as_str().unwrap();
            let got = parse_formated_millis(input);

            assert_eq!(
                got.is_ok(),
                case["ok"].as_bool().unwrap(),
                "{input:?}: accept/reject"
            );

            match got {
                Ok(millis) => {
                    assert_eq!(millis, case["millis"].as_i64().unwrap(), "{input:?}: value")
                }
                Err(_) => {
                    rejected += 1;
                    assert!(
                        case["error"].as_str().is_some(),
                        "{input:?}: Go recorded an error"
                    );
                }
            }
        }

        assert_eq!(rejected, 4, "the corpus must exercise the reject path");
    }
}
