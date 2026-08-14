//! Port of `server/public/model/status.go`.
//!
//! A small file with two traps worth stating up front.
//!
//! **`dnd_end_time` is in seconds.** Every other timestamp in the model package is epoch
//! milliseconds; this one is not, and Go says so in a comment rather than in the type. Nothing
//! in either language will catch a caller that treats it as milliseconds.
//!
//! **`active_channel` is declared on the wire and then stripped from it.** It has a `json:` tag
//! *and* `omitempty`, but both marshallers in the file blank it on a copy first, so the key
//! never leaves the server. Serialising a `Status` directly is therefore **not** the same as
//! [`Status::to_json`] — see the note there.
//!
//! Pinned by `fixtures/status.json` and `fixtures/behaviour_status.json`.

use std::collections::HashMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::utils::{StringMap, go_json_marshal};

/// Port of `model.StatusOutOfOffice` (status.go:12). Note the value is `"ooo"`, not
/// `"out_of_office"`.
pub const STATUS_OUT_OF_OFFICE: &str = "ooo";

/// Port of `model.StatusOffline` (status.go:13). The only status
/// [`status_map_to_interface_map`] filters out.
pub const STATUS_OFFLINE: &str = "offline";

/// Port of `model.StatusAway` (status.go:14).
pub const STATUS_AWAY: &str = "away";

/// Port of `model.StatusDnd` (status.go:15).
pub const STATUS_DND: &str = "dnd";

/// Port of `model.StatusOnline` (status.go:16).
pub const STATUS_ONLINE: &str = "online";

/// Port of `model.StatusCacheSize` (status.go:17), which is defined as `SessionCacheSize`.
///
/// Aliased rather than re-transcribed, so the two cannot drift apart the way they could in Go.
pub const STATUS_CACHE_SIZE: usize = crate::session::SESSION_CACHE_SIZE;

/// Port of `model.StatusChannelTimeout` (status.go:18) — 20 seconds, in milliseconds.
pub const STATUS_CHANNEL_TIMEOUT: i64 = 20_000;

/// Port of `model.StatusMinUpdateTime` (status.go:19) — 2 minutes, in milliseconds.
pub const STATUS_MIN_UPDATE_TIME: i64 = 120_000;

/// Port of `model.DNDExpiryInterval` (status.go:22) — how often the job expiring temporary DND
/// statuses runs.
///
/// Go's `time.Duration` is an `int64` of **nanoseconds**, the only nanosecond quantity in the
/// model package; the oracle records it as `60000000000`.
pub const DND_EXPIRY_INTERVAL: Duration = Duration::from_secs(60);

/// Port of `model.Status` (status.go:25).
///
/// `#[serde(default)]` because Go's `encoding/json` zero-fills absent keys — and here that is
/// not merely defensive: `active_channel` carries `omitempty`, so a `Status` produced by Go is
/// routinely missing the key and must still decode.
///
/// The Go struct also carries `xml:` tags. Nothing in the migration targets the XML encoder, so
/// they are not reproduced; if an XML path ever appears, note that its names are the Go field
/// names (`UserId`, `DNDEndTime`), not the snake_case JSON ones.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Status {
    #[serde(rename = "user_id")]
    pub user_id: String,

    #[serde(rename = "status")]
    pub status: String,

    #[serde(rename = "manual")]
    pub manual: bool,

    /// Epoch **milliseconds**, unlike [`Self::dnd_end_time`].
    #[serde(rename = "last_activity_at")]
    pub last_activity_at: i64,

    /// Carries `db:"-"` as well as `omitempty` in Go: neither persisted nor, in practice, sent
    /// — [`Status::to_json`] and [`status_list_to_json`] both blank it first. A non-pointer
    /// `string` with `omitempty` is a `String` plus a skip predicate, never an `Option`.
    #[serde(rename = "active_channel", skip_serializing_if = "String::is_empty")]
    pub active_channel: String,

    /// When the user's DND status expires, in **seconds** — not milliseconds like every other
    /// timestamp in the package (status.go:32-33).
    #[serde(rename = "dnd_end_time")]
    pub dnd_end_time: i64,

    /// `json:"-"` in Go: server-side only, never on the wire.
    #[serde(skip)]
    pub prev_status: String,
}

impl Status {
    /// Port of `(*Status).ToJSON` (status.go:39).
    ///
    /// Blanks `active_channel` **on a copy**, so the receiver keeps its value and the key is
    /// dropped from the output by `omitempty`. Serialising a `Status` with serde directly does
    /// *not* do this and will leak the field — use this method on any path that sends a status
    /// to a client.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        // Go's `sCopy := *s`. A value copy, not a borrow-checker workaround.
        let mut copy = self.clone();
        copy.active_channel.clear();
        go_json_marshal(&copy)
    }
}

/// Port of `model.StatusListToJSON` (status.go:45).
///
/// Blanks `active_channel` on every element, again on copies. Go takes `[]*Status` and
/// dereferences without a nil check, so a nil element panics; a Rust slice cannot hold one.
///
/// **Never emits `null`.** Go builds `make([]Status, len(u))`, which is empty-but-non-nil even
/// for a nil input, so an absent list serialises as `[]`. A port that passed a nil Go slice
/// straight to the encoder would write `null` and break clients that index the result.
pub fn status_list_to_json(statuses: &[Status]) -> Result<String, serde_json::Error> {
    let list: Vec<Status> = statuses
        .iter()
        .map(|s| Status {
            active_channel: String::new(),
            ..s.clone()
        })
        .collect();
    go_json_marshal(&list)
}

/// Port of `model.StatusMapToInterfaceMap` (status.go:54).
///
/// Two things it does that are easy to get wrong:
///
/// - **The result is keyed by `status.user_id`, not by the input map's key.** They agree at
///   every Go call site, which is precisely why using the wrong one would go unnoticed.
/// - **Offline statuses are dropped**, on the documented convention that an omitted user is
///   offline. Only the exact string `"offline"` is filtered, so an *empty* status survives.
///
/// Go's return type is `map[string]any`, but the only value it ever inserts is `s.Status`, so
/// the shape is proven and this returns a [`StringMap`].
///
/// The input key is unused; it stays in the signature so call-site ports are mechanical. Note
/// that two entries sharing a `user_id` would collide, and Go's map iteration order makes the
/// winner nondeterministic — no call site can produce that.
pub fn status_map_to_interface_map(status_map: &HashMap<String, Status>) -> StringMap {
    let mut out = StringMap::new();
    for status in status_map.values() {
        if status.status != STATUS_OFFLINE {
            out.insert(status.user_id.clone(), status.status.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn online() -> Status {
        Status {
            user_id: "6bdz674pgq767e4jx75w4pf57a".into(),
            status: STATUS_ONLINE.into(),
            manual: true,
            last_activity_at: 1_700_000_000_000,
            active_channel: "qr6kf7ztp7yifxt4wm5xn51bke".into(),
            dnd_end_time: 1_700_000_060,
            prev_status: STATUS_AWAY.into(),
        }
    }

    #[test]
    fn round_trips_the_generated_fixture() {
        let raw = include_str!("../../../fixtures/status.json");
        let parsed: Status = serde_json::from_str(raw).unwrap();
        let original: Value = serde_json::from_str(raw).unwrap();
        assert_eq!(serde_json::to_value(&parsed).unwrap(), original);
    }

    #[test]
    fn prev_status_never_reaches_the_wire() {
        let json = serde_json::to_value(online()).unwrap();
        assert!(json.get("prev_status").is_none());
        // ...and an inbound one is ignored rather than rejected.
        let parsed: Status =
            serde_json::from_str(r#"{"user_id":"a","prev_status":"online"}"#).unwrap();
        assert_eq!(parsed.prev_status, "");
    }

    #[test]
    fn a_status_missing_every_optional_key_still_decodes() {
        // Go zero-fills absent keys, and `active_channel` is routinely absent because of
        // omitempty — so this is the shape Go's own output round-trips through.
        let parsed: Status = serde_json::from_str("{}").unwrap();
        assert_eq!(parsed, Status::default());
    }

    #[test]
    fn to_json_strips_active_channel_without_touching_the_receiver() {
        let status = online();
        let encoded = status.to_json().unwrap();

        assert!(!encoded.contains("active_channel"));
        assert_eq!(status.active_channel, "qr6kf7ztp7yifxt4wm5xn51bke");
        // Serialising directly does leak it — the difference this method exists for.
        assert!(
            serde_json::to_string(&status)
                .unwrap()
                .contains("active_channel")
        );
    }

    #[test]
    fn an_empty_list_is_an_array_not_null() {
        assert_eq!(status_list_to_json(&[]).unwrap(), "[]");
    }

    #[test]
    fn only_the_exact_offline_string_is_filtered() {
        let mut map = HashMap::new();
        map.insert(
            "k1".to_string(),
            Status {
                user_id: "u1".into(),
                status: STATUS_OFFLINE.into(),
                ..Default::default()
            },
        );
        map.insert(
            "k2".to_string(),
            Status {
                user_id: "u2".into(),
                status: String::new(),
                ..Default::default()
            },
        );
        map.insert(
            "k3".to_string(),
            Status {
                user_id: "u3".into(),
                status: "OFFLINE".into(),
                ..Default::default()
            },
        );

        let out = status_map_to_interface_map(&map);
        assert_eq!(out.get("u1"), None);
        assert_eq!(out.get("u2").map(String::as_str), Some(""));
        assert_eq!(out.get("u3").map(String::as_str), Some("OFFLINE"));
    }

    #[test]
    fn the_result_is_keyed_by_user_id_not_by_the_map_key() {
        let mut map = HashMap::new();
        map.insert(
            "not-a-user-id".to_string(),
            Status {
                user_id: "u1".into(),
                status: STATUS_AWAY.into(),
                ..Default::default()
            },
        );

        let out = status_map_to_interface_map(&map);
        assert_eq!(out.get("u1").map(String::as_str), Some(STATUS_AWAY));
        assert!(!out.contains_key("not-a-user-id"));
    }

    #[test]
    fn dnd_end_time_is_seconds_while_last_activity_is_millis() {
        // Not a behaviour test — a tripwire for anyone "unifying" the two units. Both fields
        // are i64 and nothing else distinguishes them.
        let status = online();
        assert_eq!(status.dnd_end_time, 1_700_000_060);
        assert_eq!(status.last_activity_at, 1_700_000_000_000);
        assert_eq!(status.last_activity_at / 1000, status.dnd_end_time - 60);
    }
}

/// Parity tests driven by `fixtures/behaviour_status.json` — Go's own answers.
#[cfg(test)]
mod go_parity {
    use super::*;
    use serde_json::Value;

    fn oracle() -> Value {
        serde_json::from_str(include_str!("../../../fixtures/behaviour_status.json")).unwrap()
    }

    #[test]
    fn constants_match_go() {
        let oracle = oracle();
        let c = &oracle["constants"];
        assert_eq!(STATUS_OUT_OF_OFFICE, c["out_of_office"].as_str().unwrap());
        assert_eq!(STATUS_OFFLINE, c["offline"].as_str().unwrap());
        assert_eq!(STATUS_AWAY, c["away"].as_str().unwrap());
        assert_eq!(STATUS_DND, c["dnd"].as_str().unwrap());
        assert_eq!(STATUS_ONLINE, c["online"].as_str().unwrap());
        assert_eq!(
            STATUS_CHANNEL_TIMEOUT,
            c["channel_timeout"].as_i64().unwrap()
        );
        assert_eq!(
            STATUS_MIN_UPDATE_TIME,
            c["min_update_time"].as_i64().unwrap()
        );
        assert_eq!(
            DND_EXPIRY_INTERVAL.as_nanos() as u64,
            c["dnd_expiry_interval_nanos"].as_u64().unwrap()
        );
        // The cross-file borrow from session.go. Recorded here so it cannot drift unnoticed,
        // which is the pattern D-005 asks the remaining borrows to adopt.
        assert_eq!(STATUS_CACHE_SIZE as u64, c["cache_size"].as_u64().unwrap());
    }

    /// Byte-for-byte, not `Value`-for-`Value`: field order and the *absence* of the stripped key
    /// are both part of what is being asserted.
    #[test]
    fn to_json_matches_go() {
        let oracle = oracle();
        let cases = oracle["to_json"].as_array().unwrap();
        assert!(!cases.is_empty());
        for case in cases {
            let name = case["name"].as_str().unwrap();
            // `plain` is what a naive serialize produces; rebuilding from it proves the input
            // came from Go rather than from our own idea of the struct.
            let status: Status = serde_json::from_value(case["plain"].clone()).unwrap();
            assert_eq!(
                status.to_json().unwrap(),
                case["out_bytes"].as_str().unwrap(),
                "case {name}"
            );
        }
    }

    #[test]
    fn to_json_leaves_the_receiver_alone() {
        let oracle = oracle();
        for case in oracle["to_json"].as_array().unwrap() {
            let name = case["name"].as_str().unwrap();
            let status: Status = serde_json::from_value(case["plain"].clone()).unwrap();
            let before = status.active_channel.clone();
            let _ = status.to_json().unwrap();
            assert_eq!(status.active_channel, before, "case {name}");
            assert_eq!(
                status.active_channel,
                case["active_channel_after"].as_str().unwrap(),
                "case {name}"
            );
        }
    }

    #[test]
    fn status_list_to_json_matches_go() {
        let oracle = oracle();
        let cases = oracle["status_list_to_json"].as_array().unwrap();
        assert!(!cases.is_empty());
        for case in cases {
            let name = case["name"].as_str().unwrap();
            // Go's nil and empty inputs both serialise to `null`/`[]` on the way into the
            // fixture; a Rust slice cannot be nil, so both arrive here as an empty slice —
            // which is the point, since Go's output is `[]` for both.
            let input: Vec<Status> = if case["in"].is_null() {
                Vec::new()
            } else {
                serde_json::from_value(case["in"].clone()).unwrap()
            };
            assert_eq!(
                status_list_to_json(&input).unwrap(),
                case["out_bytes"].as_str().unwrap(),
                "case {name}"
            );
        }
    }

    #[test]
    fn status_map_to_interface_map_matches_go() {
        let oracle = oracle();
        let cases = oracle["status_map_to_interface_map"].as_array().unwrap();
        assert!(!cases.is_empty());
        for case in cases {
            let name = case["name"].as_str().unwrap();
            let input: HashMap<String, Status> =
                serde_json::from_value(case["in"].clone()).unwrap();
            let got = status_map_to_interface_map(&input);
            assert_eq!(
                serde_json::to_value(&got).unwrap(),
                case["out"],
                "case {name}"
            );
        }
    }
}
