//! Port of `model/channel_list.go` (channel_list.go:1–53).
//!
//! The whole file is two named slice types and their `Etag`. Both are newtypes rather than
//! aliases because Go attaches methods to the named slice, and both are `#[serde(transparent)]`
//! so they still serialise as bare JSON arrays.
//!
//! # The `Etag` algorithm is not "the newest channel"
//!
//! It looks like a max over `LastPostAt`, but each comparison is against a single running `t`
//! that the previous comparison may already have raised. So `t` ends up the maximum over *all*
//! the compared fields of *all* the channels, and `id` is whichever channel last raised it —
//! which can be a different channel from the one holding the largest `LastPostAt`. Every case
//! below is pinned in `fixtures/behaviour_channel_list.json`.

use std::ops::{Deref, DerefMut};

use serde::{Deserialize, Serialize};

use crate::channel::{Channel, ChannelWithTeamData};
use crate::utils::etag;

/// Port of `model.ChannelList` (channel_list.go:6).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ChannelList(pub Vec<Channel>);

impl ChannelList {
    /// Port of `(*ChannelList).Etag` (channel_list.go:8).
    ///
    /// Three things worth knowing before reading a value:
    ///
    /// - **`delta` is always zero.** Go declares `var delta int64` and never assigns it, so the
    ///   third component of every list etag is a literal `0`.
    /// - **An empty or nil list yields `<version>.0.0.0.0`** — `id` starts as the string `"0"`.
    /// - **Comparisons are strictly greater-than**, so ties keep the earlier channel, and a
    ///   list whose timestamps are all negative keeps `id = "0"` and `t = 0`.
    pub fn etag(&self) -> String {
        let (id, t) = max_timestamp(
            self.0
                .iter()
                .map(|c| (c.id.as_str(), [c.last_post_at, c.update_at])),
        );
        etag(&[&id, &t, &0_i64, &self.0.len()])
    }
}

/// Port of `model.ChannelListWithTeamData` (channel_list.go:28).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ChannelListWithTeamData(pub Vec<ChannelWithTeamData>);

impl ChannelListWithTeamData {
    /// Port of `(*ChannelListWithTeamData).Etag` (channel_list.go:30).
    ///
    /// Identical to [`ChannelList::etag`] plus a third comparison on `team_update_at`, so a
    /// team rename invalidates every channel list for that team.
    pub fn etag(&self) -> String {
        let (id, t) = max_timestamp(self.0.iter().map(|c| {
            (
                c.channel.id.as_str(),
                [
                    c.channel.last_post_at,
                    c.channel.update_at,
                    c.team_update_at,
                ],
            )
        }));
        etag(&[&id, &t, &0_i64, &self.0.len()])
    }
}

/// The shared body of both `Etag`s: walk the items in order, and for each candidate timestamp
/// in turn raise `t` and adopt that item's id if the timestamp is strictly greater.
///
/// Taking the candidates as an array keeps the two field orders explicit at the call sites —
/// the order matters only for which comparison raises `t` first, but reordering would change
/// nothing observable and is exactly the kind of "harmless" edit that breaks parity later.
fn max_timestamp<'a, const N: usize>(
    items: impl Iterator<Item = (&'a str, [i64; N])>,
) -> (&'a str, i64) {
    let mut id = "0";
    let mut t = 0_i64;
    for (item_id, candidates) in items {
        for candidate in candidates {
            if candidate > t {
                t = candidate;
                id = item_id;
            }
        }
    }
    (id, t)
}

impl From<Vec<Channel>> for ChannelList {
    fn from(channels: Vec<Channel>) -> Self {
        Self(channels)
    }
}

impl From<Vec<ChannelWithTeamData>> for ChannelListWithTeamData {
    fn from(channels: Vec<ChannelWithTeamData>) -> Self {
        Self(channels)
    }
}

impl Deref for ChannelList {
    type Target = Vec<Channel>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for ChannelList {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl Deref for ChannelListWithTeamData {
    type Target = Vec<ChannelWithTeamData>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for ChannelListWithTeamData {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::CURRENT_VERSION;

    fn channel(id: &str, last_post_at: i64, update_at: i64) -> Channel {
        Channel {
            id: id.into(),
            last_post_at,
            update_at,
            ..Default::default()
        }
    }

    #[test]
    fn lists_serialise_as_bare_arrays() {
        let list = ChannelList(vec![channel("a", 1, 2)]);
        let value = serde_json::to_value(&list).unwrap();
        assert!(value.is_array(), "must be a bare array, not an object");
        assert_eq!(value[0]["id"], "a");

        let round_tripped: ChannelList = serde_json::from_value(value).unwrap();
        assert_eq!(round_tripped, list);
    }

    #[test]
    fn empty_list_etag_is_all_zeroes() {
        assert_eq!(
            ChannelList::default().etag(),
            format!("{CURRENT_VERSION}.0.0.0.0")
        );
        assert_eq!(
            ChannelListWithTeamData::default().etag(),
            format!("{CURRENT_VERSION}.0.0.0.0")
        );
    }

    #[test]
    fn delta_is_always_zero() {
        // Go declares `var delta int64` and never assigns it.
        let list = ChannelList(vec![channel("a", 100, 200)]);
        let value = list.etag();
        let parts: Vec<&str> = value.rsplit('.').collect();
        // Reversed: count, delta, t, id, then the version's own components.
        assert_eq!(parts[0], "1", "count");
        assert_eq!(parts[1], "0", "delta");
    }

    #[test]
    fn ties_keep_the_earlier_channel() {
        let list = ChannelList(vec![
            channel("first", 100, 100),
            channel("second", 100, 100),
        ]);
        assert!(list.etag().contains(".first."));
    }

    #[test]
    fn the_id_can_come_from_a_different_channel_than_the_newest_post() {
        // "a" holds the largest last_post_at, but "b"'s update_at raises t last.
        let list = ChannelList(vec![channel("a", 300, 0), channel("b", 100, 400)]);
        assert_eq!(list.etag(), format!("{CURRENT_VERSION}.b.400.0.2"));
    }

    #[test]
    fn negative_timestamps_never_beat_the_initial_zero() {
        let list = ChannelList(vec![channel("a", -5, -5)]);
        assert_eq!(list.etag(), format!("{CURRENT_VERSION}.0.0.0.1"));
    }

    #[test]
    fn deref_exposes_the_underlying_vec() {
        let mut list = ChannelList::default();
        list.push(channel("a", 1, 1));
        assert_eq!(list.len(), 1);
        assert_eq!(list.first().unwrap().id, "a");
    }
}

/// Parity tests driven by `fixtures/behaviour_channel_list.json` — Go's own answers.
#[cfg(test)]
mod go_parity {
    use super::*;
    use crate::team::Team;
    use crate::user::User;
    use crate::utils::{CURRENT_VERSION, etag};
    use serde_json::Value;

    fn oracle() -> Value {
        serde_json::from_str(include_str!(
            "../../../fixtures/behaviour_channel_list.json"
        ))
        .unwrap()
    }

    /// The drift detector for [`CURRENT_VERSION`]. When the pinned Go SHA moves to a release
    /// with a new `versions[0]`, this fails — instead of every etag in the tree silently
    /// changing.
    #[test]
    fn current_version_matches_go() {
        assert_eq!(
            CURRENT_VERSION,
            oracle()["current_version"].as_str().unwrap(),
            "version.go's versions[0] moved; update CURRENT_VERSION in utils.rs"
        );
    }

    #[test]
    fn etag_parts_match_go() {
        let oracle = oracle();
        let cases = oracle["etag_parts"].as_array().unwrap();
        assert!(!cases.is_empty());

        for case in cases {
            let name = case["name"].as_str().unwrap();
            // The oracle stores each part as JSON, so rebuild it as the Display it came from.
            let owned: Vec<String> = case["parts"]
                .as_array()
                .unwrap()
                .iter()
                .map(|part| match part {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                })
                .collect();
            let parts: Vec<&dyn std::fmt::Display> =
                owned.iter().map(|s| s as &dyn std::fmt::Display).collect();
            assert_eq!(etag(&parts), case["out"].as_str().unwrap(), "Etag({name})");
        }
    }

    #[test]
    fn channel_list_etag_matches_go() {
        let oracle = oracle();
        let cases = oracle["channel_list_etag"].as_array().unwrap();
        assert!(!cases.is_empty());

        for case in cases {
            let name = case["name"].as_str().unwrap();
            // A Go nil slice marshals to `null`; both it and `[]` mean an empty list here.
            let list: ChannelList = if case["items"].is_null() {
                ChannelList::default()
            } else {
                serde_json::from_value(case["items"].clone()).unwrap()
            };
            assert_eq!(list.etag(), case["out"].as_str().unwrap(), "Etag({name})");
        }
    }

    #[test]
    fn channel_list_with_team_data_etag_matches_go() {
        let oracle = oracle();
        let cases = oracle["channel_list_with_team_data_etag"]
            .as_array()
            .unwrap();
        assert!(!cases.is_empty());

        for case in cases {
            let name = case["name"].as_str().unwrap();
            let list: ChannelListWithTeamData = if case["items"].is_null() {
                ChannelListWithTeamData::default()
            } else {
                serde_json::from_value(case["items"].clone()).unwrap()
            };
            assert_eq!(list.etag(), case["out"].as_str().unwrap(), "Etag({name})");
        }
    }

    #[test]
    fn team_etag_matches_go() {
        let oracle = oracle();
        let cases = oracle["team_etag"].as_array().unwrap();
        assert!(!cases.is_empty());

        for case in cases {
            let name = case["name"].as_str().unwrap();
            let team: Team = serde_json::from_value(case["items"].clone()).unwrap();
            assert_eq!(
                team.etag(),
                case["out"].as_str().unwrap(),
                "Team::Etag({name})"
            );
        }
    }

    #[test]
    fn user_etag_matches_go() {
        let oracle = oracle();
        let cases = oracle["user_etag"].as_array().unwrap();
        assert!(!cases.is_empty());

        for case in cases {
            let name = case["name"].as_str().unwrap();
            let user: User = serde_json::from_value(case["user"].clone()).unwrap();
            let show_full_name = case["show_full_name"].as_bool().unwrap();
            let show_email = case["show_email"].as_bool().unwrap();
            assert_eq!(
                user.etag(show_full_name, show_email),
                case["out"].as_str().unwrap(),
                "User::Etag({name}, {show_full_name}, {show_email})"
            );
        }
    }
}
