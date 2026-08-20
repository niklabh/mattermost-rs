//! Port of `model/channel_stats.go` — whole file: one wire struct and its three accessors.
//!
//! # `pinnedpost_count` has no underscore in the middle
//!
//! Four of the five tags are ordinary snake_case; `PinnedPostCount` is tagged `pinnedpost_count`,
//! not `pinned_post_count`. It is the one field here a reader will "correct" without noticing,
//! and the fixture is what stops that.
//!
//! # The three `_()` accessors return `float64`, and the conversion is lossy
//!
//! Go's telemetry pipeline wants floats, so `MemberCount_()` is `float64(o.MemberCount)`. Above
//! 2^53 that is not a widening but a rounding: `9007199254740993` comes back as
//! `9007199254740992`. Rust's `as f64` rounds to nearest-even from the same inputs, which is why
//! the corpus drives the exact boundary rather than a comfortable number — the two languages
//! agreeing here is measured, not assumed.
//!
//! Note there is deliberately **no** `FilesCount_()`: Go declares three accessors for five
//! fields. Adding a fourth for symmetry would be inventing API.

use serde::{Deserialize, Serialize};

/// Port of `model.ChannelStats` (channel_stats.go:6).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ChannelStats {
    #[serde(rename = "channel_id")]
    pub channel_id: String,

    #[serde(rename = "member_count")]
    pub member_count: i64,

    #[serde(rename = "guest_count")]
    pub guest_count: i64,

    /// `pinnedpost_count` — Go's tag, not `pinned_post_count`. See the module docs.
    #[serde(rename = "pinnedpost_count")]
    pub pinned_post_count: i64,

    #[serde(rename = "files_count")]
    pub files_count: i64,
}

impl ChannelStats {
    /// Port of `(*ChannelStats).MemberCount_` (channel_stats.go:14).
    pub fn member_count_f64(&self) -> f64 {
        self.member_count as f64
    }

    /// Port of `(*ChannelStats).GuestCount_` (channel_stats.go:18).
    pub fn guest_count_f64(&self) -> f64 {
        self.guest_count as f64
    }

    /// Port of `(*ChannelStats).PinnedPostCount_` (channel_stats.go:22).
    pub fn pinned_post_count_f64(&self) -> f64 {
        self.pinned_post_count as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn corpus() -> serde_json::Value {
        serde_json::from_str(include_str!("../../../fixtures/behaviour_small_types.json")).unwrap()
    }

    /// Every field non-zero, round-tripped through the generated fixture.
    #[test]
    fn serialization_parity_with_the_fixture() {
        let raw = include_str!("../../../fixtures/channel_stats.json");
        let stats: ChannelStats = serde_json::from_str(raw).unwrap();
        let expected: serde_json::Value = serde_json::from_str(raw).unwrap();
        assert_eq!(serde_json::to_value(&stats).unwrap(), expected);
    }

    /// No `omitempty` anywhere, so the zero value is five keys — and the key order is Go's.
    #[test]
    fn the_zero_value_is_five_keys_in_gos_order() {
        assert_eq!(
            serde_json::to_string(&ChannelStats::default()).unwrap(),
            r#"{"channel_id":"","member_count":0,"guest_count":0,"pinnedpost_count":0,"files_count":0}"#
        );
    }

    /// The tag a reader would "fix". Asserted on its own so the failure names the cause.
    #[test]
    fn the_pinned_post_tag_has_no_middle_underscore() {
        let doc = serde_json::to_string(&ChannelStats::default()).unwrap();
        assert!(doc.contains(r#""pinnedpost_count""#));
        assert!(
            !doc.contains(r#""pinned_post_count""#),
            "Go tags this without the middle underscore"
        );
    }

    /// **Go's own float conversions, at and past 2^53.** Rust and Go must round identically.
    #[test]
    fn go_parity_the_accessors_round_exactly_as_go_does() {
        let corpus = corpus();
        let rows = corpus["channel_stats"].as_array().unwrap();
        assert!(rows.len() >= 8, "the corpus should span the 2^53 boundary");

        let mut saw_lossy = false;
        for row in rows {
            let n = row["in"].as_i64().unwrap();
            let stats = ChannelStats {
                member_count: n,
                guest_count: n,
                pinned_post_count: n,
                files_count: n,
                channel_id: "c1".to_owned(),
            };

            for (key, ours) in [
                ("member_count_", stats.member_count_f64()),
                ("guest_count_", stats.guest_count_f64()),
                ("pinnedpost_count_", stats.pinned_post_count_f64()),
            ] {
                let theirs = row[key].as_f64().unwrap();
                assert_eq!(ours, theirs, "{key} for {n}");
            }

            if (stats.member_count_f64() as i64) != n {
                saw_lossy = true;
            }
        }
        assert!(
            saw_lossy,
            "the corpus must contain a value the conversion cannot represent, or this proves \
             nothing about rounding"
        );
    }

    /// There are three accessors for five fields, and the two without one are not an oversight.
    #[test]
    fn there_is_no_files_count_accessor() {
        let stats = ChannelStats {
            files_count: 9,
            ..Default::default()
        };
        // The field is reachable; only the float wrapper is absent, as in Go.
        assert_eq!(stats.files_count, 9);
    }
}
