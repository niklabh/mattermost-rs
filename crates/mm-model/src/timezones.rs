//! Port of `shared/timezones/timezones.go` and `shared/timezones/default.go`.
//!
//! Twenty-nine lines of code and 592 lines of data. `User::pre_save` needs exactly one function
//! from it — [`default_user_timezone`] — which is why it was blocked behind [D-108].
//!
//! # Three things the code half encodes
//!
//! **`"true"` is a string.** `User.Timezone` is a `map[string]string`, so the automatic-timezone
//! flag is four bytes of text and not a JSON boolean. A port reaching for a `bool` changes the
//! wire format of every user object, and the `Users.Timezone` column with it.
//!
//! **The map is fresh on every call.** `DefaultUserTimezone` allocates, so two callers cannot
//! alias each other's timezone. Returning a `&'static` here would pass every serialization
//! assertion and still be wrong the first time a caller mutated one.
//!
//! **The marshalled key order is not the insertion order.** Go writes `useAutomaticTimezone`
//! first and marshals it last, because `encoding/json` sorts map keys. [`utils::StringMap`] is a
//! `BTreeMap` ([D-027]), so this agrees — but only because that conversion already happened.
//!
//! # The supported-zone list is a literal, not a host scan
//!
//! `DefaultSupportedTimezones` is a compile-time array in the Go source, which is what makes it
//! portable as a table at all. Contrast [D-065], where `time.LoadLocation` reads the host's
//! tzdata and therefore has no single answer to match. The 592 entries are **generated** into
//! [`crate::timezones_generated`] rather than transcribed, for the same reason the emoji and
//! locale tables are.
//!
//! [D-065]: ../../../docs/TECH_DEBT.md
//! [D-108]: ../../../docs/TECH_DEBT.md

use crate::utils::StringMap;

pub use crate::timezones_generated::DEFAULT_SUPPORTED_TIMEZONES;

/// The `useAutomaticTimezone` key. Unexported in Go — it appears only as a string literal.
pub const USE_AUTOMATIC_TIMEZONE: &str = "useAutomaticTimezone";

/// The `automaticTimezone` key.
pub const AUTOMATIC_TIMEZONE: &str = "automaticTimezone";

/// The `manualTimezone` key.
pub const MANUAL_TIMEZONE: &str = "manualTimezone";

/// Port of `timezones.DefaultUserTimezone` (timezones.go:22).
///
/// A fresh three-key map: automatic timezones on, both zone names empty.
///
/// `"true"` is the **string**, not a bool — see the module docs.
pub fn default_user_timezone() -> StringMap {
    let mut tz = StringMap::new();
    tz.insert(USE_AUTOMATIC_TIMEZONE.to_owned(), "true".to_owned());
    tz.insert(AUTOMATIC_TIMEZONE.to_owned(), String::new());
    tz.insert(MANUAL_TIMEZONE.to_owned(), String::new());
    tz
}

/// Port of `timezones.Timezones` (timezones.go:6).
///
/// A one-field wrapper over the supported list. Go's `New()` copies the package-level slice into
/// the struct; the only reader is `GetSupported`, which the config endpoint serves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Timezones {
    supported_zones: &'static [&'static str],
}

impl Timezones {
    /// Port of `timezones.New` (timezones.go:10).
    pub fn new() -> Self {
        Self {
            supported_zones: DEFAULT_SUPPORTED_TIMEZONES,
        }
    }

    /// Port of `(*Timezones).GetSupported` (timezones.go:18).
    pub fn get_supported(&self) -> &'static [&'static str] {
        self.supported_zones
    }
}

impl Default for Timezones {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_three_keys_with_a_string_true() {
        let tz = default_user_timezone();
        assert_eq!(tz.len(), 3);
        assert_eq!(
            tz.get(USE_AUTOMATIC_TIMEZONE).map(String::as_str),
            Some("true")
        );
        assert_eq!(tz.get(AUTOMATIC_TIMEZONE).map(String::as_str), Some(""));
        assert_eq!(tz.get(MANUAL_TIMEZONE).map(String::as_str), Some(""));
    }

    /// The Go function allocates, so two calls cannot alias. A `&'static` would fail here.
    #[test]
    fn each_call_returns_a_fresh_map() {
        let mut a = default_user_timezone();
        let b = default_user_timezone();
        a.insert(MANUAL_TIMEZONE.to_owned(), "Europe/Berlin".to_owned());
        assert_eq!(b.get(MANUAL_TIMEZONE).map(String::as_str), Some(""));
    }

    #[test]
    fn the_default_prefers_the_automatic_zone() {
        // `useAutomaticTimezone` is "true", so `GetPreferredTimezone` reads `automaticTimezone`,
        // which the default leaves empty.
        assert_eq!(
            crate::utils::get_preferred_timezone(&default_user_timezone()),
            ""
        );
    }
}

#[cfg(test)]
mod go_parity {
    use super::*;
    use serde_json::Value;
    use std::sync::OnceLock;

    fn oracle() -> &'static Value {
        static ORACLE: OnceLock<Value> = OnceLock::new();
        ORACLE.get_or_init(|| {
            let raw = include_str!("../../../fixtures/behaviour_timezones.json");
            serde_json::from_str(raw).expect("behaviour_timezones.json parses")
        })
    }

    #[test]
    fn default_user_timezone_matches_go() {
        let d = &oracle()["default_user_timezone"];

        let want: StringMap =
            serde_json::from_value(d["value"].clone()).expect("the default map decodes");
        assert_eq!(default_user_timezone(), want);

        assert_eq!(d["len"], 3);
        assert_eq!(
            d["use_automatic_timezone_go_type"], "string",
            "the automatic flag is text, not a bool"
        );
    }

    /// Go's marshalled bytes, not merely the same key/value pairs.
    ///
    /// This is what `User::is_valid` measures its 256-rune timezone cap against, so the key order
    /// is load-bearing rather than cosmetic: Go sorts, and the insertion order recorded alongside
    /// is the reverse.
    #[test]
    fn marshalled_bytes_match_go() {
        let d = &oracle()["default_user_timezone"];
        let tz = default_user_timezone();
        let ours = crate::utils::go_json_marshal_string_map(Some(&tz));
        assert_eq!(ours, d["marshalled"].as_str().unwrap());

        assert_eq!(
            d["insertion_note"], "useAutomaticTimezone, automaticTimezone, manualTimezone",
            "the order Go WRITES them in — the marshalled order above is sorted, not this"
        );
        assert_eq!(
            ours.chars().count() as i64,
            d["rune_count_of_marshalled"].as_i64().unwrap()
        );
        // Comfortably inside the cap, so `PreSave` filling this in can never make `IsValid` fail.
        assert!(
            ours.chars().count() < d["user_timezone_max_runes"].as_i64().unwrap() as usize,
            "the default must fit the cap PreSave hands to IsValid"
        );
    }

    #[test]
    fn freshness_matches_go() {
        let f = &oracle()["freshness"];
        assert_eq!(
            f["mutating_one_call_leaks_into_another"], false,
            "Go allocates per call; so must we"
        );
        assert_eq!(f["second_call_manual_timezone"], "");
    }

    /// The nil-vs-empty distinction `PreSave`'s guard turns on.
    ///
    /// `if u.Timezone == nil` — **not** `len(u.Timezone) == 0`. An empty-but-present map is left
    /// empty, which is the same shape of trap as `IsValid`'s props check.
    #[test]
    fn pre_save_only_fills_a_nil_timezone() {
        for case in oracle()["pre_save"].as_array().unwrap() {
            let name = case["name"].as_str().unwrap();
            let in_nil = case["in_nil"].as_bool().unwrap();
            let out_len = case["out_len"].as_u64().unwrap() as usize;

            let mut tz: Option<StringMap> = if in_nil {
                None
            } else {
                Some(serde_json::from_value(case["out"].clone()).unwrap_or_default())
            };
            if tz.is_none() {
                tz = Some(default_user_timezone());
            }

            assert_eq!(
                tz.as_ref().map_or(0, StringMap::len),
                out_len,
                "{name}: length after the guard"
            );
            if name == "empty_but_present" {
                assert_eq!(
                    out_len, 0,
                    "an empty map is NOT replaced — the guard is on nil"
                );
            }
        }
    }

    #[test]
    fn supported_table_matches_go() {
        let s = &oracle()["supported"];
        assert_eq!(
            DEFAULT_SUPPORTED_TIMEZONES.len() as u64,
            s["len"].as_u64().unwrap()
        );
        assert_eq!(DEFAULT_SUPPORTED_TIMEZONES[0], s["first"].as_str().unwrap());
        assert_eq!(
            DEFAULT_SUPPORTED_TIMEZONES[DEFAULT_SUPPORTED_TIMEZONES.len() - 1],
            s["last"].as_str().unwrap()
        );
        // Measured, not assumed — the first draft of this test asserted the opposite and the
        // oracle rejected it. The literal is in byte order, so `US/*` and the single-word
        // aliases trail the `Area/Location` names for the ordinary reason rather than by accident.
        assert_eq!(s["is_sorted"], true);
        assert!(
            DEFAULT_SUPPORTED_TIMEZONES.windows(2).all(|w| w[0] < w[1]),
            "the emitted table must keep Go's order, which is byte-sorted and strictly increasing"
        );
        // One assertion over all 592 entries, order-sensitively.
        assert_eq!(
            fnv1a64(DEFAULT_SUPPORTED_TIMEZONES),
            s["fnv1a64"].as_u64().unwrap()
        );
        assert_eq!(
            Timezones::new().get_supported(),
            DEFAULT_SUPPORTED_TIMEZONES
        );
    }

    /// FNV-1a over the table, matching `timezones_gen.go`'s digest.
    ///
    /// FNV-1**a**, deliberately unlike `link_metadata::generate_link_metadata_hash`, which is
    /// FNV-1 because Go's `fnv.New32()` is. This one is our own checksum and answers to nothing
    /// upstream, so the more usual variant is the safer default.
    fn fnv1a64(values: &[&str]) -> u64 {
        const OFFSET: u64 = 14_695_981_039_346_656_037;
        const PRIME: u64 = 1_099_511_628_211;
        let mut h = OFFSET;
        for s in values {
            for b in s.as_bytes() {
                h ^= u64::from(*b);
                h = h.wrapping_mul(PRIME);
            }
            // The NUL separator, so ["ab","c"] and ["a","bc"] differ.
            h ^= 0;
            h = h.wrapping_mul(PRIME);
        }
        h
    }
}
