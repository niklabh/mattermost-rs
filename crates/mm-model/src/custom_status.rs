//! Port of `server/public/model/custom_status.go`.
//!
//! `CustomStatus` is the odd one out among the model types: its `ExpiresAt` is a real
//! `time.Time`, not the `int64` of epoch milliseconds every other timestamp in the package
//! uses. It therefore goes on the wire as RFC 3339, with Go's exact fractional-second and
//! zone-offset rules — see [`crate::utils::go_time`], which is what the field serialises
//! through and where those rules are documented and pinned.
//!
//! The value is not only an API response: `User.Props["customStatus"]` holds a marshalled
//! `CustomStatus` as a string, so these bytes round-trip through the database and are read back
//! by the Go server running alongside us.
//!
//! Everything here is pinned by `fixtures/behaviour_custom_status.json` and
//! `fixtures/custom_status.json`.

use std::ops::{Deref, DerefMut};

use chrono::{DateTime, FixedOffset, Utc};
use serde::{Deserialize, Serialize};

use crate::utils::go_time;

/// Port of `model.UserPropsKeyCustomStatus` (custom_status.go:14) — the `User.Props` key the
/// marshalled status is stored under.
pub const USER_PROPS_KEY_CUSTOM_STATUS: &str = "customStatus";

/// Port of `model.CustomStatusTextMaxRunes` (custom_status.go:16). Runes, not bytes.
pub const CUSTOM_STATUS_TEXT_MAX_RUNES: usize = 100;

/// Port of `model.MaxRecentCustomStatuses` (custom_status.go:17).
pub const MAX_RECENT_CUSTOM_STATUSES: usize = 5;

/// Port of `model.DefaultCustomStatusEmoji` (custom_status.go:18).
pub const DEFAULT_CUSTOM_STATUS_EMOJI: &str = "speech_balloon";

/// The duration [`CustomStatus::pre_save`] writes when an unexpired status arrives without one.
pub const DURATION_DATE_AND_TIME: &str = "date_and_time";

/// Port of `validCustomStatusDuration` (custom_status.go:21), which is unexported in Go.
///
/// Membership is observed through [`CustomStatus::are_duration_and_expiration_time_valid`], the
/// only function that reads the map. Note the empty string is **not** a member even though that
/// function returns true for it — an absent duration is handled by a separate earlier branch.
pub const VALID_CUSTOM_STATUS_DURATIONS: &[&str] = &[
    "thirty_minutes",
    "one_hour",
    "four_hours",
    "today",
    "this_week",
    "date_and_time",
];

/// Whether `duration` is a key of Go's `validCustomStatusDuration`. Exact match, so
/// `DATE_AND_TIME` and `date and time` are both rejected.
pub fn is_valid_custom_status_duration(duration: &str) -> bool {
    VALID_CUSTOM_STATUS_DURATIONS.contains(&duration)
}

/// Errors from the two `RecentCustomStatuses` methods Go declares as returning an `error`.
#[derive(Debug, thiserror::Error)]
pub enum CustomStatusError {
    /// Go's `json.Marshal` failure. Reachable only through
    /// [`crate::utils::go_time::format`]'s year bound — a status whose `expires_at` falls
    /// outside `[0, 9999]` cannot be marshalled, and both `Contains` and `Remove` marshal
    /// before they do anything else.
    #[error("marshalling a custom status failed")]
    Marshal(#[from] serde_json::Error),
}

/// Port of `model.CustomStatus` (custom_status.go:30).
///
/// No field carries `omitempty`, so all four keys are always present on the way **out** —
/// including `expires_at`, which renders Go's zero time as `"0001-01-01T00:00:00Z"` rather
/// than being dropped or written as `null`.
///
/// On the way **in** every field is optional: Go's `encoding/json` leaves an absent key at its
/// zero value, so `{}` and `{"emoji":"a"}` both decode. `#[serde(default)]` is what reproduces
/// that — without it serde rejects the partial object that Go accepts, which is reachable from
/// any client sending less than the full shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CustomStatus {
    #[serde(rename = "emoji")]
    pub emoji: String,

    #[serde(rename = "text")]
    pub text: String,

    #[serde(rename = "duration")]
    pub duration: String,

    #[serde(rename = "expires_at", with = "crate::utils::go_time")]
    pub expires_at: DateTime<FixedOffset>,
}

impl Default for CustomStatus {
    /// Go's zero `CustomStatus`: three empty strings and the zero `time.Time`.
    fn default() -> Self {
        Self {
            emoji: String::new(),
            text: String::new(),
            duration: String::new(),
            expires_at: *go_time::ZERO,
        }
    }
}

/// Equality that matches Go's, which compares **marshalled bytes** rather than fields.
///
/// This is not the same as chrono's `DateTime` equality, and the difference is reachable:
/// `12:00:00Z` and `17:30:00+05:30` are the same instant, so chrono calls them equal, but they
/// marshal to different strings and Go's `Contains` calls them different. Comparing the offset
/// explicitly reproduces the byte semantics for every value that can be marshalled at all.
impl PartialEq for CustomStatus {
    fn eq(&self, other: &Self) -> bool {
        self.emoji == other.emoji
            && self.text == other.text
            && self.duration == other.duration
            && self.expires_at == other.expires_at
            && self.expires_at.offset() == other.expires_at.offset()
    }
}

impl Eq for CustomStatus {}

impl CustomStatus {
    /// Port of `(*CustomStatus).PreSave` (custom_status.go:37).
    ///
    /// Two unrelated fixups. The duration one promotes an **empty** duration to
    /// `date_and_time` when the expiry has not already passed — and Go's zero time counts as
    /// passed, so `{duration: "", expires_at: zero}` is left exactly as it is. The text one
    /// truncates to 100 **runes**, which can split a grapheme cluster: 101 base-plus-combining
    /// pairs (202 runes) come back as 100 runes, ending on a bare combining mark.
    ///
    /// Note this is not a validator — `pre_save` will happily produce a status that
    /// [`Self::are_duration_and_expiration_time_valid`] rejects.
    pub fn pre_save(&mut self) {
        if self.duration.is_empty() && !self.is_expired(Utc::now()) {
            self.duration = DURATION_DATE_AND_TIME.to_string();
        }

        // Go slices `[]rune(cs.Text)`, so the limit counts code points. Only walk the string
        // when it could possibly be too long.
        if self.text.chars().count() > CUSTOM_STATUS_TEXT_MAX_RUNES {
            self.text = self
                .text
                .chars()
                .take(CUSTOM_STATUS_TEXT_MAX_RUNES)
                .collect();
        }
    }

    /// Port of `(*CustomStatus).AreDurationAndExpirationTimeValid` (custom_status.go:48).
    ///
    /// True in exactly two shapes:
    ///
    /// - no duration, and either no expiry at all (the zero time) or one that has not passed;
    /// - a duration in [`VALID_CUSTOM_STATUS_DURATIONS`] **and** an expiry that has not passed.
    ///
    /// The asymmetry is worth noticing: `{duration: "", expires_at: zero}` is valid, but
    /// `{duration: "date_and_time", expires_at: zero}` is not — a named duration always
    /// demands a future expiry, and the zero time is in the past.
    ///
    /// Go reads `time.Now()` once per branch; this reads it once for both. The two can only
    /// disagree for an expiry landing inside the nanoseconds between the calls.
    pub fn are_duration_and_expiration_time_valid(&self) -> bool {
        let now = Utc::now();
        let expired = self.is_expired(now);

        if self.duration.is_empty() && (go_time::is_zero(&self.expires_at) || !expired) {
            return true;
        }

        is_valid_custom_status_duration(&self.duration) && !expired
    }

    /// `cs.ExpiresAt.Before(now)`. The zero time is always in the past.
    fn is_expired(&self, now: DateTime<Utc>) -> bool {
        self.expires_at.with_timezone(&Utc) < now
    }

    /// The `json.Marshal(cs)` that `Contains` and `Remove` compare, and that
    /// `User::set_custom_status` stores.
    ///
    /// Goes through [`crate::utils::go_json_marshal`] rather than `serde_json::to_string` so
    /// the bytes are Go's, HTML escaping included. That matters twice over: these strings are
    /// persisted in `User.Props`, and the recent-statuses list compares them for equality one
    /// level up.
    pub fn marshal(&self) -> Result<String, CustomStatusError> {
        Ok(crate::utils::go_json_marshal(self)?)
    }
}

/// Port of `model.RuneToHexadecimalString` (custom_status.go:60) — `fmt.Sprintf("%04x", r)`.
///
/// Lower-case, zero-padded to at least four digits and never truncated above that, so
/// `U+1F600` renders as five digits (`1f600`). Go's parameter is a `rune`, i.e. an `int32`
/// that can hold negative and surrogate values a Rust `char` cannot; `%04x` would render those
/// with a leading `-`. No call site passes one — they all come from iterating a string.
pub fn rune_to_hexadecimal_string(r: char) -> String {
    format!("{:04x}", u32::from(r))
}

/// Port of `model.RecentCustomStatuses` (custom_status.go:64) — a bare `[]CustomStatus`.
///
/// `#[serde(transparent)]` so it is a JSON array, not an object wrapping one.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RecentCustomStatuses(pub Vec<CustomStatus>);

impl Deref for RecentCustomStatuses {
    type Target = Vec<CustomStatus>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for RecentCustomStatuses {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl From<Vec<CustomStatus>> for RecentCustomStatuses {
    fn from(v: Vec<CustomStatus>) -> Self {
        Self(v)
    }
}

impl RecentCustomStatuses {
    /// Port of `(RecentCustomStatuses).Contains` (custom_status.go:66).
    ///
    /// Byte equality over the whole status, so this is much stricter than it looks: a status
    /// matching on emoji and text but differing in `duration` or `expires_at` is **not**
    /// contained. Contrast [`Self::add`], which dedups on `text` alone.
    ///
    /// A status with neither an emoji nor a text is never contained, whatever the list holds —
    /// but note Go marshals *first* and returns the marshalling error even for that case, so
    /// the order of the two checks is load-bearing and reproduced.
    ///
    /// Go's nil-receiver-argument branch (`cs == nil` → `false`) is unrepresentable here and
    /// is not ported.
    pub fn contains(&self, cs: &CustomStatus) -> Result<bool, CustomStatusError> {
        let target = cs.marshal()?;
        if cs.emoji.is_empty() && cs.text.is_empty() {
            return Ok(false);
        }

        for status in &self.0 {
            if status.marshal()? == target {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Port of `(RecentCustomStatuses).Add` (custom_status.go:94).
    ///
    /// Prepends `cs`, drops every existing entry with the **same text** — regardless of emoji,
    /// duration or expiry — and truncates to [`MAX_RECENT_CUSTOM_STATUSES`]. There is no
    /// emptiness guard, so adding a fully zero status prepends it like any other.
    ///
    /// Go writes the filtered entries back through `rcs[:0]`, which rewrites the caller's
    /// backing array in place; this allocates instead. See D-024 — the aliasing is observable
    /// in Go only by a caller that keeps the old slice, which every call site treats as dead.
    pub fn add(&self, cs: &CustomStatus) -> Self {
        let mut out = Vec::with_capacity((self.0.len() + 1).min(MAX_RECENT_CUSTOM_STATUSES));
        // Go stores `*cs`, a copy of the pointed-to value; this is that copy, not a borrow
        // checker workaround.
        out.push(cs.clone());
        for status in &self.0 {
            if out.len() == MAX_RECENT_CUSTOM_STATUSES {
                break;
            }
            if status.text != cs.text {
                out.push(status.clone());
            }
        }
        Self(out)
    }

    /// Port of `(RecentCustomStatuses).Remove` (custom_status.go:110).
    ///
    /// Drops every byte-identical entry, so it is as strict as [`Self::contains`] and does not
    /// share [`Self::add`]'s text-only matching. A status with neither emoji nor text returns
    /// the list untouched, which means an empty status can be *added* but never removed.
    ///
    /// Unlike `add` this applies no cap: a list already over
    /// [`MAX_RECENT_CUSTOM_STATUSES`] stays over it.
    pub fn remove(&self, cs: &CustomStatus) -> Result<Self, CustomStatusError> {
        let target = cs.marshal()?;
        if cs.emoji.is_empty() && cs.text.is_empty() {
            return Ok(self.clone());
        }

        let mut out = Vec::with_capacity(self.0.len());
        for status in &self.0 {
            if status.marshal()? != target {
                out.push(status.clone());
            }
        }
        Ok(Self(out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use serde_json::Value;

    fn at(s: &str) -> DateTime<FixedOffset> {
        go_time::parse(s).unwrap()
    }

    fn status(emoji: &str, text: &str) -> CustomStatus {
        CustomStatus {
            emoji: emoji.to_string(),
            text: text.to_string(),
            duration: DURATION_DATE_AND_TIME.to_string(),
            expires_at: at("2026-08-14T12:00:00Z"),
        }
    }

    #[test]
    fn round_trips_the_generated_fixture() {
        let raw = include_str!("../../../fixtures/custom_status.json");
        let parsed: CustomStatus = serde_json::from_str(raw).unwrap();
        let reserialized = serde_json::to_value(&parsed).unwrap();
        let original: Value = serde_json::from_str(raw).unwrap();
        assert_eq!(reserialized, original);
    }

    #[test]
    fn the_zero_status_keeps_every_key() {
        let json = serde_json::to_value(CustomStatus::default()).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "emoji": "",
                "text": "",
                "duration": "",
                "expires_at": "0001-01-01T00:00:00Z",
            })
        );
    }

    #[test]
    fn equality_distinguishes_equal_instants_in_different_zones() {
        let mut utc = status("a", "one");
        utc.expires_at = at("2026-08-14T12:00:00Z");
        let mut ist = status("a", "one");
        ist.expires_at = at("2026-08-14T17:30:00+05:30");

        // The same instant, so chrono's own comparison agrees they are equal...
        assert_eq!(utc.expires_at, ist.expires_at);
        // ...but they marshal differently, and Go compares bytes.
        assert_ne!(utc, ist);
        assert_ne!(utc.marshal().unwrap(), ist.marshal().unwrap());
    }

    #[test]
    fn pre_save_leaves_an_empty_duration_alone_when_the_expiry_has_passed() {
        let mut cs = CustomStatus {
            expires_at: Utc
                .with_ymd_and_hms(2020, 1, 1, 0, 0, 0)
                .unwrap()
                .fixed_offset(),
            ..Default::default()
        };
        cs.pre_save();
        assert_eq!(cs.duration, "");

        // ...and the zero time counts as passed, which is the case that is easy to get wrong.
        let mut zero = CustomStatus::default();
        zero.pre_save();
        assert_eq!(zero.duration, "");
    }

    #[test]
    fn pre_save_truncates_by_runes_not_bytes() {
        let mut cs = CustomStatus {
            text: "\u{1F600}".repeat(101),
            ..Default::default()
        };
        cs.pre_save();
        assert_eq!(cs.text.chars().count(), 100);
        assert_eq!(cs.text.len(), 400);
    }

    #[test]
    fn an_empty_duration_with_no_expiry_is_valid_but_a_named_one_is_not() {
        let mut cs = CustomStatus::default();
        assert!(cs.are_duration_and_expiration_time_valid());

        cs.duration = DURATION_DATE_AND_TIME.to_string();
        assert!(!cs.are_duration_and_expiration_time_valid());
    }

    #[test]
    fn add_dedups_on_text_while_contains_compares_every_byte() {
        let list = RecentCustomStatuses(vec![status("a", "one"), status("b", "two")]);
        let same_text_other_emoji = status("z", "one");

        assert!(!list.contains(&same_text_other_emoji).unwrap());
        assert_eq!(
            list.add(&same_text_other_emoji).0,
            vec![status("z", "one"), status("b", "two")]
        );
    }

    #[test]
    fn add_caps_the_list_and_remove_does_not() {
        let six = RecentCustomStatuses(vec![
            status("a", "1"),
            status("b", "2"),
            status("c", "3"),
            status("d", "4"),
            status("e", "5"),
            status("f", "6"),
        ]);
        assert_eq!(six.add(&status("g", "7")).len(), MAX_RECENT_CUSTOM_STATUSES);
        assert_eq!(six.remove(&status("z", "9")).unwrap().len(), 6);
    }

    #[test]
    fn an_empty_status_can_be_added_but_never_removed() {
        let list = RecentCustomStatuses(vec![CustomStatus::default(), status("a", "one")]);
        let empty = CustomStatus::default();

        assert!(!list.contains(&empty).unwrap());
        assert_eq!(list.remove(&empty).unwrap(), list);
        assert_eq!(list.add(&empty).len(), 2);
    }

    #[test]
    fn rune_to_hexadecimal_string_pads_to_four_and_never_truncates() {
        assert_eq!(rune_to_hexadecimal_string('\u{0}'), "0000");
        assert_eq!(rune_to_hexadecimal_string('a'), "0061");
        assert_eq!(rune_to_hexadecimal_string('\u{2603}'), "2603");
        assert_eq!(rune_to_hexadecimal_string('\u{1F600}'), "1f600");
    }

    #[test]
    fn only_the_six_named_durations_are_valid() {
        for duration in VALID_CUSTOM_STATUS_DURATIONS {
            assert!(is_valid_custom_status_duration(duration));
        }
        assert!(!is_valid_custom_status_duration(""));
        assert!(!is_valid_custom_status_duration("DATE_AND_TIME"));
        assert!(!is_valid_custom_status_duration("forever"));
    }
}

/// Parity tests driven by `fixtures/behaviour_custom_status.json` — Go's own answers.
#[cfg(test)]
mod go_parity {
    use super::*;
    use chrono::{Duration, TimeZone};
    use serde_json::Value;

    fn oracle() -> Value {
        serde_json::from_str(include_str!(
            "../../../fixtures/behaviour_custom_status.json"
        ))
        .unwrap()
    }

    /// Rebuilds a case's `ExpiresAt` the way the Go generator did: either the zero time or an
    /// offset in whole hours from now.
    fn expires_at_for(case: &Value) -> DateTime<FixedOffset> {
        if case["expires_at_zero"].as_bool().unwrap() {
            return *go_time::ZERO;
        }
        let hours = case["expires_at_offset_hours"].as_i64().unwrap();
        (Utc::now() + Duration::hours(hours)).fixed_offset()
    }

    fn case_status(case: &Value) -> CustomStatus {
        CustomStatus {
            duration: case["duration"].as_str().unwrap().to_string(),
            expires_at: expires_at_for(case),
            ..Default::default()
        }
    }

    // --- crate::utils::go_time ------------------------------------------------

    #[test]
    fn time_marshal_matches_go() {
        let oracle = oracle();
        let cases = oracle["time_marshal"].as_array().unwrap();
        assert!(!cases.is_empty());
        for case in cases {
            let name = case["name"].as_str().unwrap();
            let seconds = case["unix_seconds"].as_i64().unwrap();
            let nanos = case["nanos"].as_u64().unwrap() as u32;
            let offset = case["offset_seconds"].as_i64().unwrap() as i32;

            let zone = FixedOffset::east_opt(offset).unwrap();
            let t = zone.timestamp_opt(seconds, nanos).unwrap();

            match go_time::format(&t) {
                Some(rendered) => {
                    let want = case["json"].as_str().unwrap();
                    // The oracle records the JSON token, quotes included.
                    assert_eq!(format!("\"{rendered}\""), want, "case {name}");
                    assert_eq!(go_time::is_zero(&t), case["is_zero"].as_bool().unwrap());
                }
                None => assert!(
                    !case["err"].as_str().unwrap().is_empty(),
                    "case {name}: we refused to format a value Go marshalled"
                ),
            }
        }
    }

    #[test]
    fn time_unmarshal_matches_go() {
        let oracle = oracle();
        let sentinel = oracle["time_unmarshal_sentinel"].as_str().unwrap();
        let cases = oracle["time_unmarshal"].as_array().unwrap();
        assert!(!cases.is_empty());

        for case in cases {
            let input = case["in"].as_str().unwrap();
            let want_ok = case["ok"].as_bool().unwrap();

            #[derive(Deserialize, Serialize)]
            struct Wrapper {
                #[serde(with = "crate::utils::go_time")]
                t: DateTime<FixedOffset>,
            }

            let wrapped = format!("{{\"t\":{input}}}");
            let got: Result<Wrapper, _> = serde_json::from_str(&wrapped);

            match (got, want_ok) {
                (Ok(w), true) => {
                    let want = case["out"].as_str().unwrap();
                    if want == sentinel {
                        // `null` is the only input Go accepts without writing anything. We
                        // cannot keep a previous value, so we write the zero time instead —
                        // D-023. Assert the divergence rather than pretending it is absent.
                        assert_eq!(input, "null");
                        assert!(go_time::is_zero(&w.t));
                        continue;
                    }
                    let rendered = go_time::format(&w.t).unwrap();
                    assert_eq!(format!("\"{rendered}\""), want, "case {input}");
                }
                (Err(_), false) => {}
                (Ok(_), false) => panic!("case {input}: accepted, Go rejected"),
                (Err(e), true) => panic!("case {input}: rejected ({e}), Go accepted"),
            }
        }
    }

    // --- custom_status.go ------------------------------------------------------

    #[test]
    fn valid_durations_match_go() {
        let oracle = oracle();
        let cases = oracle["valid_durations"].as_object().unwrap();
        assert!(!cases.is_empty());
        for (duration, want) in cases {
            // The oracle observed the whole validity function against a future expiry, so an
            // empty duration comes back true through the first branch rather than through the
            // map. Everything else is map membership.
            let cs = CustomStatus {
                duration: duration.clone(),
                expires_at: (Utc::now() + Duration::hours(24)).fixed_offset(),
                ..Default::default()
            };
            assert_eq!(
                cs.are_duration_and_expiration_time_valid(),
                want.as_bool().unwrap(),
                "duration {duration:?}"
            );
        }
    }

    #[test]
    fn pre_save_text_matches_go() {
        let oracle = oracle();
        let cases = oracle["pre_save_text"].as_array().unwrap();
        assert!(!cases.is_empty());
        for case in cases {
            let mut cs = CustomStatus {
                text: case["in"].as_str().unwrap().to_string(),
                ..Default::default()
            };
            cs.pre_save();
            assert_eq!(cs.text, case["out"].as_str().unwrap());
        }
    }

    #[test]
    fn pre_save_duration_matches_go() {
        let oracle = oracle();
        let cases = oracle["pre_save_duration"].as_array().unwrap();
        assert!(!cases.is_empty());
        for case in cases {
            let mut cs = case_status(case);
            cs.pre_save();
            assert_eq!(
                cs.duration,
                case["out"].as_str().unwrap(),
                "duration={:?} zero={} offset={}",
                case["duration"],
                case["expires_at_zero"],
                case["expires_at_offset_hours"]
            );
        }
    }

    #[test]
    fn duration_and_expiry_validity_matches_go() {
        let oracle = oracle();
        let cases = oracle["duration_and_expiry"].as_array().unwrap();
        assert!(!cases.is_empty());
        for case in cases {
            assert_eq!(
                case_status(case).are_duration_and_expiration_time_valid(),
                case["out"].as_bool().unwrap(),
                "duration={:?} zero={} offset={}",
                case["duration"],
                case["expires_at_zero"],
                case["expires_at_offset_hours"]
            );
        }
    }

    #[test]
    fn rune_to_hex_matches_go() {
        let oracle = oracle();
        let cases = oracle["rune_to_hex"].as_object().unwrap();
        assert!(!cases.is_empty());
        for (code_point, want) in cases {
            let r = char::from_u32(code_point.parse().unwrap()).unwrap();
            assert_eq!(rune_to_hexadecimal_string(r), want.as_str().unwrap());
        }
    }

    // --- RecentCustomStatuses ---------------------------------------------------

    fn case_list(case: &Value) -> RecentCustomStatuses {
        RecentCustomStatuses(
            case["list"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| serde_json::from_value(v.clone()).unwrap())
                .collect(),
        )
    }

    fn case_arg(case: &Value) -> CustomStatus {
        serde_json::from_value(case["arg"].clone()).unwrap()
    }

    #[test]
    fn contains_matches_go() {
        let oracle = oracle();
        let cases = oracle["contains"].as_array().unwrap();
        assert!(!cases.is_empty());
        for case in cases {
            let name = case["name"].as_str().unwrap();
            assert_eq!(
                case_list(case).contains(&case_arg(case)).unwrap(),
                case["found"].as_bool().unwrap(),
                "case {name}"
            );
        }
    }

    #[test]
    fn add_matches_go() {
        let oracle = oracle();
        let cases = oracle["add"].as_array().unwrap();
        assert!(!cases.is_empty());
        for case in cases {
            let name = case["name"].as_str().unwrap();
            let got = case_list(case).add(&case_arg(case));
            assert_eq!(
                serde_json::to_value(&got).unwrap(),
                case["out"],
                "case {name}"
            );
        }
    }

    #[test]
    fn remove_matches_go() {
        let oracle = oracle();
        let cases = oracle["remove"].as_array().unwrap();
        assert!(!cases.is_empty());
        for case in cases {
            let name = case["name"].as_str().unwrap();
            let got = case_list(case).remove(&case_arg(case)).unwrap();
            assert_eq!(
                serde_json::to_value(&got).unwrap(),
                case["out"],
                "case {name}"
            );
        }
    }
}
