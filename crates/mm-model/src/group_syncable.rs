//! Port of `model/group_syncable.go` — the link between a group and a team or channel.
//!
//! # One struct, two wire shapes, and neither matches the struct
//!
//! `SyncableId` and `Type` both carry `json:"-"`, so the field that says *what* the group is
//! linked to is never written under its own name. Hand-written `MarshalJSON` renames it per type:
//! a team syncable writes `team_id`, a channel syncable writes `channel_id` **plus** `team_id`
//! for the parent team. A syncable whose `Type` is neither is an **error to marshal** — there is
//! no default shape.
//!
//! `UnmarshalJSON` is the mirror image and is deliberately lossy: it reads **only** `team_id`,
//! `channel_id`, `group_id` and `auto_add`, and silently drops everything else. So
//! `scheme_admin`, the three timestamps and the joined display names do not survive a round trip
//! — a fact worth knowing before using a decoded syncable as an update body.
//!
//! Presence of `channel_id` is what picks the type: a body with both ids is a **channel**
//! syncable whose `team_id` becomes the parent team.

use serde::de::{Deserializer, Error as DeError};
use serde::ser::{SerializeMap, Serializer};
use serde::{Deserialize, Serialize};

use crate::utils::{AppError, AppResult, is_valid_id};

/// Port of `model.GroupSyncableType` (group_syncable.go:12) — a `string` newtype whose two values
/// are **capitalised**: `Team` and `Channel`, not `team`/`channel`.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct GroupSyncableType(pub String);

impl GroupSyncableType {
    pub const TEAM: &'static str = "Team";
    pub const CHANNEL: &'static str = "Channel";

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for GroupSyncableType {
    fn from(s: &str) -> Self {
        GroupSyncableType(s.to_string())
    }
}

/// Port of `(GroupSyncableType).String` (group_syncable.go:19).
impl std::fmt::Display for GroupSyncableType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Port of `model.GroupSyncable` (group_syncable.go:23).
///
/// Seven of the twelve fields are `db:"-"` **and** `json:"-"`: they are joined in from the team or
/// channel for display and are neither stored on the row nor sent under those names.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GroupSyncable {
    pub group_id: String,

    /// The id of the team or channel being synced. `db:"-"` and `json:"-"` — it reaches the wire
    /// as `team_id` or `channel_id`; see the module docs.
    pub syncable_id: String,

    pub auto_add: bool,
    pub scheme_admin: bool,
    /// Epoch milliseconds.
    pub create_at: i64,
    pub delete_at: i64,
    pub update_at: i64,
    /// `db:"-"`, `json:"-"` — decides which wire shape is used.
    pub type_: GroupSyncableType,

    /// Joined in from the associated channel.
    pub channel_display_name: String,
    /// Joined in from the associated team.
    pub team_display_name: String,
    pub team_type: String,
    pub channel_type: String,
    /// The parent team of a channel syncable.
    pub team_id: String,
}

impl GroupSyncable {
    /// Port of `model.NewGroupTeam` (group_syncable.go:176).
    pub fn new_group_team(group_id: &str, team_id: &str, auto_add: bool) -> Self {
        Self {
            group_id: group_id.to_string(),
            syncable_id: team_id.to_string(),
            type_: GroupSyncableType::TEAM.into(),
            auto_add,
            ..Default::default()
        }
    }

    /// Port of `model.NewGroupChannel` (group_syncable.go:185).
    ///
    /// Note it does **not** set `team_id`: a channel syncable built this way has no parent team
    /// until the store fills one in.
    pub fn new_group_channel(group_id: &str, channel_id: &str, auto_add: bool) -> Self {
        Self {
            group_id: group_id.to_string(),
            syncable_id: channel_id.to_string(),
            type_: GroupSyncableType::CHANNEL.into(),
            auto_add,
            ..Default::default()
        }
    }

    /// Port of `(*GroupSyncable).IsValid` (group_syncable.go:63).
    ///
    /// The `Where` is **`GroupSyncable.SyncableIsValid`**, not `GroupSyncable.IsValid`. `Type` is
    /// not checked, so an unmarshalled syncable can be valid and still fail to marshal.
    pub fn is_valid(&self) -> AppResult {
        if !is_valid_id(&self.group_id) {
            return Err(err("group_id"));
        }
        if !is_valid_id(&self.syncable_id) {
            return Err(err("syncable_id"));
        }
        Ok(())
    }

    /// Port of `(*GroupSyncable).Patch` (group_syncable.go:159).
    pub fn patch(&mut self, patch: &GroupSyncablePatch) {
        if let Some(auto_add) = patch.auto_add {
            self.auto_add = auto_add;
        }
        if let Some(scheme_admin) = patch.scheme_admin {
            self.scheme_admin = scheme_admin;
        }
    }
}

fn err(field: &str) -> Box<AppError> {
    Box::new(AppError::new(
        "GroupSyncable.SyncableIsValid",
        format!("model.group_syncable.{field}.app_error"),
        None,
        "",
        400,
    ))
}

impl Serialize for GroupSyncable {
    /// Port of `(*GroupSyncable).MarshalJSON` (group_syncable.go:104).
    ///
    /// Key order is Go's: the renamed id and its display fields first, then `type`, then the
    /// embedded alias's own fields. The `*_display_name`, `*_type` and `type` keys carry
    /// `omitempty`; `team_id`/`channel_id` do not, except that a channel syncable's `team_id`
    /// does.
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut map = s.serialize_map(None)?;

        match self.type_.as_str() {
            GroupSyncableType::TEAM => {
                map.serialize_entry("team_id", &self.syncable_id)?;
                if !self.team_display_name.is_empty() {
                    map.serialize_entry("team_display_name", &self.team_display_name)?;
                }
                if !self.team_type.is_empty() {
                    map.serialize_entry("team_type", &self.team_type)?;
                }
                map.serialize_entry("type", &self.type_)?;
            }
            GroupSyncableType::CHANNEL => {
                map.serialize_entry("channel_id", &self.syncable_id)?;
                if !self.channel_display_name.is_empty() {
                    map.serialize_entry("channel_display_name", &self.channel_display_name)?;
                }
                if !self.channel_type.is_empty() {
                    map.serialize_entry("channel_type", &self.channel_type)?;
                }
                map.serialize_entry("type", &self.type_)?;

                if !self.team_id.is_empty() {
                    map.serialize_entry("team_id", &self.team_id)?;
                }
                if !self.team_display_name.is_empty() {
                    map.serialize_entry("team_display_name", &self.team_display_name)?;
                }
                if !self.team_type.is_empty() {
                    map.serialize_entry("team_type", &self.team_type)?;
                }
            }
            other => {
                return Err(serde::ser::Error::custom(format!(
                    "unknown syncable type: {other}"
                )));
            }
        }

        // The embedded `*Alias` — the fields that keep their own tags.
        map.serialize_entry("group_id", &self.group_id)?;
        map.serialize_entry("auto_add", &self.auto_add)?;
        map.serialize_entry("scheme_admin", &self.scheme_admin)?;
        map.serialize_entry("create_at", &self.create_at)?;
        map.serialize_entry("delete_at", &self.delete_at)?;
        map.serialize_entry("update_at", &self.update_at)?;

        map.end()
    }
}

/// The four keys `UnmarshalJSON` reads. Everything else in the body is discarded.
#[derive(Deserialize, Default)]
#[serde(default)]
struct GroupSyncableWire {
    team_id: Option<String>,
    channel_id: Option<String>,
    group_id: Option<String>,
    auto_add: Option<bool>,
}

impl<'de> Deserialize<'de> for GroupSyncable {
    /// Port of `(*GroupSyncable).UnmarshalJSON` (group_syncable.go:72).
    ///
    /// Go's `value.(string)` / `value.(bool)` assertions are unchecked and **panic** on a
    /// wrong-typed key; here they are decode errors, which is the safe direction and the only
    /// difference. A `null` for any of the four is treated as absent, matching Go's `nil`
    /// assertion panic being unreachable for a well-formed client.
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let wire = GroupSyncableWire::deserialize(d).map_err(DeError::custom)?;

        let channel_id = wire.channel_id.unwrap_or_default();
        let team_id = wire.team_id.unwrap_or_default();

        let mut syncable = GroupSyncable {
            group_id: wire.group_id.unwrap_or_default(),
            auto_add: wire.auto_add.unwrap_or(false),
            ..Default::default()
        };

        if !channel_id.is_empty() {
            syncable.team_id = team_id;
            syncable.syncable_id = channel_id;
            syncable.type_ = GroupSyncableType::CHANNEL.into();
        } else {
            syncable.syncable_id = team_id;
            syncable.type_ = GroupSyncableType::TEAM.into();
        }

        Ok(syncable)
    }
}

/// Port of `model.GroupSyncablePatch` (group_syncable.go:145). Neither field carries
/// `omitempty`, so both keys are always written, as `null` when unset.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct GroupSyncablePatch {
    #[serde(rename = "auto_add")]
    pub auto_add: Option<bool>,

    #[serde(rename = "scheme_admin")]
    pub scheme_admin: Option<bool>,
}

/// Port of `model.UserTeamIDPair` (group_syncable.go:166). No tags.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct UserTeamIDPair {
    pub user_id: String,
    pub team_id: String,
}

/// Port of `model.UserChannelIDPair` (group_syncable.go:171).
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct UserChannelIDPair {
    pub user_id: String,
    pub channel_id: String,
}

#[cfg(test)]
mod wire_parity {
    use super::*;

    /// Round-trips the Go-generated fixture: decode into the port's type, re-encode, and compare
    /// the value graphs. The fixture is produced by `reference/dump`, whose reflective filler
    /// gives **every** field a distinctive non-zero value — so a dropped key, a renamed tag or a
    /// mis-typed field cannot pass. This is the parity oracle, not a smoke test.
    macro_rules! assert_fixture_round_trips {
        ($ty:ty, $fixture:literal) => {{
            let raw = include_str!(concat!("../../../fixtures/", $fixture, ".json"));
            let decoded: $ty =
                serde_json::from_str(raw).unwrap_or_else(|e| panic!("decoding {}: {e}", $fixture));
            let expected: serde_json::Value = serde_json::from_str(raw).unwrap();
            assert_eq!(
                serde_json::to_value(&decoded).unwrap(),
                expected,
                "re-encoding {} does not match Go",
                $fixture
            );
        }};
    }

    #[test]
    fn group_syncable_patch_round_trips_the_fixture() {
        assert_fixture_round_trips!(GroupSyncablePatch, "group_syncable_patch");
    }
}

#[cfg(test)]
mod go_parity {
    use super::*;

    fn oracle() -> serde_json::Value {
        serde_json::from_str(include_str!(
            "../../../fixtures/behaviour_sweep_models.json"
        ))
        .expect("behaviour_sweep_models.json is generated by reference/dump")
    }

    /// Rebuilds the Go corpus's inputs. The oracle records only the *output*, because the input
    /// side is a Go struct literal; keeping the two lists in the same order is what ties them
    /// together, and the name assertion is what catches a drift in that order.
    fn full(type_: &str) -> GroupSyncable {
        GroupSyncable {
            group_id: "g5m1zdc3jpn19eh4h6c9j3xyzz".to_string(),
            syncable_id: "s7k2waa4hqm28fj5j7d8k4abcd".to_string(),
            auto_add: true,
            scheme_admin: true,
            create_at: 1_700_000_000_000,
            delete_at: 1_700_000_000_001,
            update_at: 1_700_000_000_002,
            type_: type_.into(),
            channel_display_name: "Channel Display".to_string(),
            team_display_name: "Team Display".to_string(),
            team_type: "O".to_string(),
            channel_type: "P".to_string(),
            team_id: "t9j3xbb5irn39gk6k8e9l5efgh".to_string(),
        }
    }

    fn minimal(type_: &str) -> GroupSyncable {
        GroupSyncable {
            group_id: "g5m1zdc3jpn19eh4h6c9j3xyzz".to_string(),
            syncable_id: "s7k2waa4hqm28fj5j7d8k4abcd".to_string(),
            type_: type_.into(),
            ..Default::default()
        }
    }

    /// Asserts the **text**, not the value graph: `MarshalJSON` writes its keys in a fixed order
    /// that a `serde_json::Value` comparison would erase, and that order is part of the wire
    /// format a Go client's streaming decoder sees.
    #[test]
    fn marshal_matches_go() {
        let oracle = oracle();
        let cases = oracle["group_syncable_marshal"].as_array().unwrap();
        assert_eq!(
            cases.len(),
            6,
            "corpus should cover both shapes and both failures"
        );

        let inputs: Vec<GroupSyncable> = vec![
            full(GroupSyncableType::TEAM),
            full(GroupSyncableType::CHANNEL),
            minimal(GroupSyncableType::TEAM),
            minimal(GroupSyncableType::CHANNEL),
            GroupSyncable {
                group_id: "g5m1zdc3jpn19eh4h6c9j3xyzz".to_string(),
                ..Default::default()
            },
            GroupSyncable {
                group_id: "g5m1zdc3jpn19eh4h6c9j3xyzz".to_string(),
                type_: "Nope".into(),
                ..Default::default()
            },
        ];
        let names = [
            "team_full",
            "channel_full",
            "team_minimal",
            "channel_minimal",
            "unset_type",
            "unknown_type",
        ];

        for ((case, input), name) in cases.iter().zip(inputs.iter()).zip(names.iter()) {
            assert_eq!(
                case["name"].as_str().unwrap(),
                *name,
                "corpus order drifted"
            );
            match case.get("out").and_then(|v| v.as_str()) {
                Some(expected) => assert_eq!(
                    serde_json::to_string(input).unwrap(),
                    expected,
                    "marshalling {name}"
                ),
                None => {
                    // Go wraps the message: `json: error calling MarshalJSON for type
                    // *model.GroupSyncable: <ours>`. Only the tail is the port's to reproduce.
                    let go_err = case["error"].as_str().unwrap();
                    let tail = go_err
                        .rsplit_once("*model.GroupSyncable: ")
                        .expect("Go error should be a MarshalJSON wrapper")
                        .1;
                    let err = serde_json::to_string(input).unwrap_err().to_string();
                    assert!(
                        err.contains(tail),
                        "marshalling {name}: {err:?} lacks {tail:?}"
                    );
                }
            }
        }
    }

    /// The deliberately lossy read. Both what lands in the struct and what a re-marshal produces
    /// are asserted, because the second is what a proxy forwarding a decoded body would send.
    #[test]
    fn unmarshal_matches_go() {
        let oracle = oracle();
        let cases = oracle["group_syncable_unmarshal"].as_array().unwrap();
        assert_eq!(cases.len(), 6);

        for case in cases {
            let input = case["in"].as_str().unwrap();
            let decoded: GroupSyncable =
                serde_json::from_str(input).unwrap_or_else(|e| panic!("decoding {input}: {e}"));

            assert_eq!(
                decoded.group_id,
                case["group_id"].as_str().unwrap(),
                "{input}"
            );
            assert_eq!(
                decoded.syncable_id,
                case["syncable_id"].as_str().unwrap(),
                "{input}"
            );
            assert_eq!(
                decoded.type_.as_str(),
                case["type"].as_str().unwrap(),
                "{input}"
            );
            assert_eq!(
                decoded.auto_add,
                case["auto_add"].as_bool().unwrap(),
                "{input}"
            );
            // Dropped by Go's reader even when the body carries them.
            assert_eq!(
                decoded.scheme_admin,
                case["scheme_admin"].as_bool().unwrap(),
                "{input}"
            );
            assert_eq!(
                decoded.create_at,
                case["create_at"].as_i64().unwrap(),
                "{input}"
            );
            assert_eq!(
                decoded.team_id,
                case["team_id"].as_str().unwrap(),
                "{input}"
            );

            assert_eq!(
                serde_json::to_string(&decoded).unwrap(),
                case["remarshalled"].as_str().unwrap(),
                "re-marshalling {input}"
            );
        }
    }
}
