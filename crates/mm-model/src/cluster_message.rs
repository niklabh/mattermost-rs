//! Port of `model/cluster_message.go` — the intra-cluster message envelope and its event names.
//!
//! # `SendType` and `WaitForAllToSend` are `json:"-"`
//!
//! Only three of the five fields cross the wire, and the two that do not are exactly the two that
//! decide delivery semantics — so a message that round-trips through JSON loses its send type and
//! is re-read as best-effort-unset. That is Go's behaviour: the send type is chosen by the sender
//! at call time, never carried.
//!
//! # The gossip constants are untyped
//!
//! `ClusterEvent*` are typed `ClusterEvent`; the fourteen `ClusterGossipEvent*` names and the two
//! `ClusterSend*` names are **untyped string constants** in Go, i.e. a different type. They are
//! plain `&str` here for the same reason.

use serde::{Deserialize, Serialize};

use crate::serde_helpers::{is_empty_map, is_none};
use crate::utils::StringMap;

/// Port of `model.ClusterEvent` (cluster_message.go:12) — a `string` newtype, so the wire form is
/// the string itself.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ClusterEvent(pub String);

impl ClusterEvent {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for ClusterEvent {
    fn from(s: &str) -> Self {
        ClusterEvent(s.to_string())
    }
}

impl std::fmt::Display for ClusterEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The `ClusterEvent` constants (cluster_message.go:15). Grouped as `&str` so a caller can match
/// on `msg.event.as_str()`; wrap with `ClusterEvent::from` to construct.
///
/// A new event must also be added to `m.ClusterEventMap` in `metrics/metrics.go`, per the note in
/// the Go source.
pub mod events {
    pub const NONE: &str = "none";
    pub const PUBLISH: &str = "publish";
    pub const UPDATE_STATUS: &str = "update_status";
    pub const INVALIDATE_ALL_CACHES: &str = "inv_all_caches";
    pub const INVALIDATE_CACHE_FOR_REACTIONS: &str = "inv_reactions";
    pub const INVALIDATE_CACHE_FOR_CHANNEL_MEMBERS_NOTIFY_PROPS: &str =
        "inv_channel_members_notify_props";
    pub const INVALIDATE_CACHE_FOR_CHANNEL_BY_NAME: &str = "inv_channel_name";
    pub const INVALIDATE_CACHE_FOR_CHANNEL: &str = "inv_channel";
    pub const INVALIDATE_CACHE_FOR_CHANNEL_GUEST_COUNT: &str = "inv_channel_guest_count";
    pub const INVALIDATE_CACHE_FOR_USER: &str = "inv_user";
    /// Note the mismatch: the constant is `InvalidateWebConnCacheForUser`, the value is
    /// `inv_user_teams`.
    pub const INVALIDATE_WEB_CONN_CACHE_FOR_USER: &str = "inv_user_teams";
    pub const CLEAR_SESSION_CACHE_FOR_USER: &str = "clear_session_user";
    pub const INVALIDATE_CACHE_FOR_ROLES: &str = "inv_roles";
    pub const INVALIDATE_CACHE_FOR_ROLE_PERMISSIONS: &str = "inv_role_permissions";
    pub const INVALIDATE_CACHE_FOR_PROFILE_BY_IDS: &str = "inv_profile_ids";
    pub const INVALIDATE_CACHE_FOR_ALL_PROFILES: &str = "inv_all_profiles";
    pub const INVALIDATE_CACHE_FOR_PROFILE_IN_CHANNEL: &str = "inv_profile_in_channel";
    pub const INVALIDATE_CACHE_FOR_SCHEMES: &str = "inv_schemes";
    pub const INVALIDATE_CACHE_FOR_FILE_INFOS: &str = "inv_file_infos";
    pub const INVALIDATE_CACHE_FOR_WEBHOOKS: &str = "inv_webhooks";
    pub const INVALIDATE_CACHE_FOR_EMOJIS_BY_ID: &str = "inv_emojis_by_id";
    pub const INVALIDATE_CACHE_FOR_EMOJIS_ID_BY_NAME: &str = "inv_emojis_id_by_name";
    pub const INVALIDATE_CACHE_FOR_CHANNEL_FILE_COUNT: &str = "inv_channel_file_count";
    pub const INVALIDATE_CACHE_FOR_CHANNEL_PINNEDPOSTS_COUNTS: &str =
        "inv_channel_pinnedposts_counts";
    pub const INVALIDATE_CACHE_FOR_CHANNEL_MEMBER_COUNTS: &str = "inv_channel_member_counts";
    pub const INVALIDATE_CACHE_FOR_CHANNELS_MEMBER_COUNT: &str = "inv_channels_member_count";
    pub const INVALIDATE_CACHE_FOR_LAST_POSTS: &str = "inv_last_posts";
    pub const INVALIDATE_CACHE_FOR_LAST_POST_TIME: &str = "inv_last_post_time";
    pub const INVALIDATE_CACHE_FOR_POSTS_USAGE: &str = "inv_posts_usage";
    pub const INVALIDATE_CACHE_FOR_TEAMS: &str = "inv_teams";
    pub const INVALIDATE_CACHE_FOR_CONTENT_FLAGGING: &str = "inv_content_flagging";
    pub const INVALIDATE_CACHE_FOR_SESSION_ATTRIBUTES: &str = "inv_session_attributes";
    pub const UPDATE_SESSION_ATTRIBUTES: &str = "update_session_attributes";
    pub const INVALIDATE_CACHE_FOR_PROPERTY_FIELDS: &str = "inv_property_fields";
    pub const INVALIDATE_CACHE_FOR_AUTO_TRANSLATION: &str = "inv_autotranslation";
    pub const INVALIDATE_CACHE_FOR_READ_RECEIPTS: &str = "inv_read_receipts";
    pub const INVALIDATE_CACHE_FOR_TEMPORARY_POSTS: &str = "inv_temporary_posts";
    /// Again a mismatch: `ClearSessionCacheForAllUsers` is `inv_all_user_sessions`.
    pub const CLEAR_SESSION_CACHE_FOR_ALL_USERS: &str = "inv_all_user_sessions";
    pub const INSTALL_PLUGIN: &str = "install_plugin";
    pub const REMOVE_PLUGIN: &str = "remove_plugin";
    pub const PLUGIN_EVENT: &str = "plugin_event";
    pub const INVALIDATE_CACHE_FOR_TERMS_OF_SERVICE: &str = "inv_terms_of_service";
    pub const INVALIDATE_CACHE_FOR_USER_AUTO_TRANSLATION: &str = "inv_user_autotranslation";
    pub const INVALIDATE_CACHE_FOR_POST_TRANSLATION_ETAG: &str = "inv_post_translation_etag";
    pub const AUTO_TRANSLATION_TASK: &str = "autotranslation_task";
    pub const BUSY_STATE_CHANGED: &str = "busy_state_change";
}

/// The gossip request/response pairs (cluster_message.go:64). Untyped constants in Go.
pub mod gossip {
    pub const REQUEST_GET_LOGS: &str = "gossip_request_get_logs";
    pub const RESPONSE_GET_LOGS: &str = "gossip_response_get_logs";
    pub const REQUEST_GENERATE_SUPPORT_PACKET: &str = "gossip_request_generate_support_packet";
    pub const RESPONSE_GENERATE_SUPPORT_PACKET: &str = "gossip_response_generate_support_packet";
    /// The request/response names drop `get_` here: `gossip_request_cluster_stats`.
    pub const REQUEST_GET_CLUSTER_STATS: &str = "gossip_request_cluster_stats";
    pub const RESPONSE_GET_CLUSTER_STATS: &str = "gossip_response_cluster_stats";
    pub const REQUEST_GET_PLUGIN_STATUSES: &str = "gossip_request_plugin_statuses";
    pub const RESPONSE_GET_PLUGIN_STATUSES: &str = "gossip_response_plugin_statuses";
    pub const REQUEST_SAVE_CONFIG: &str = "gossip_request_save_config";
    pub const RESPONSE_SAVE_CONFIG: &str = "gossip_response_save_config";
    pub const REQUEST_WEB_CONN_COUNT: &str = "gossip_request_webconn_count";
    pub const RESPONSE_WEB_CONN_COUNT: &str = "gossip_response_webconn_count";
    pub const REQUEST_WS_QUEUES: &str = "gossip_request_ws_queues";
    pub const RESPONSE_WS_QUEUES: &str = "gossip_response_ws_queues";
}

/// Port of `model.ClusterSendBestEffort` (cluster_message.go:81).
pub const CLUSTER_SEND_BEST_EFFORT: &str = "best_effort";
/// Port of `model.ClusterSendReliable` (cluster_message.go:82).
pub const CLUSTER_SEND_RELIABLE: &str = "reliable";

/// Port of `model.ClusterMessage` (cluster_message.go:85).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ClusterMessage {
    #[serde(rename = "event")]
    pub event: ClusterEvent,

    /// `json:"-"` — [`CLUSTER_SEND_RELIABLE`] or [`CLUSTER_SEND_BEST_EFFORT`].
    #[serde(skip)]
    pub send_type: String,

    /// `json:"-"`.
    #[serde(skip)]
    pub wait_for_all_to_send: bool,

    /// base64 on the wire — see [`crate::go_bytes`].
    #[serde(
        rename = "data",
        with = "crate::go_bytes",
        skip_serializing_if = "is_none"
    )]
    pub data: Option<Vec<u8>>,

    #[serde(rename = "props", skip_serializing_if = "is_empty_map")]
    pub props: StringMap,
}

impl ClusterMessage {
    /// The `Props` keys `LogFields` reads for [`events::PLUGIN_EVENT`]. Capitalised, unlike every
    /// other map key in this crate.
    pub const PROP_PLUGIN_ID: &'static str = "PluginID";
    /// See [`Self::PROP_PLUGIN_ID`].
    pub const PROP_EVENT_ID: &'static str = "EventID";
}

/// The partial `Data` header `LogFields` decodes for a [`events::PUBLISH`] message
/// (cluster_message.go:100).
///
/// `LogFields` itself returns `[]mlog.Field`, which is the logging layer's type and does not
/// belong in `mm-model`; the part that *is* a wire format — the shape Go partially unmarshals —
/// is this. The caller in `mm-app` builds the tracing fields from it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct ClusterPublishHeader {
    #[serde(rename = "event")]
    pub event: String,

    #[serde(rename = "broadcast")]
    pub broadcast: ClusterPublishBroadcastHeader,
}

/// The `broadcast` half of [`ClusterPublishHeader`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct ClusterPublishBroadcastHeader {
    #[serde(rename = "channel_id")]
    pub channel_id: String,

    #[serde(rename = "team_id")]
    pub team_id: String,

    #[serde(rename = "omit_users")]
    pub omit_users: std::collections::BTreeMap<String, bool>,
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
    fn cluster_message_round_trips_the_fixture() {
        assert_fixture_round_trips!(ClusterMessage, "cluster_message");
    }
}
