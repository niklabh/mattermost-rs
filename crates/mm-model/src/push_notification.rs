//! Port of `model/push_notification.go` — the payload sent to the push proxy.

use serde::{Deserialize, Serialize};

use crate::serde_helpers::{is_empty_str, is_zero_i64};

pub const PUSH_NOTIFY_APPLE: &str = "apple";
pub const PUSH_NOTIFY_ANDROID: &str = "android";
pub const PUSH_NOTIFY_APPLE_REACT_NATIVE: &str = "apple_rn";
pub const PUSH_NOTIFY_ANDROID_REACT_NATIVE: &str = "android_rn";

pub const PUSH_TYPE_MESSAGE: &str = "message";
pub const PUSH_TYPE_CLEAR: &str = "clear";
pub const PUSH_TYPE_UPDATE_BADGE: &str = "update_badge";
pub const PUSH_TYPE_SESSION: &str = "session";
pub const PUSH_TYPE_TEST: &str = "test";
pub const PUSH_MESSAGE_V2: &str = "v2";

pub const PUSH_SOUND_NONE: &str = "none";

/// The APNs category that enables the inline reply action.
pub const CATEGORY_CAN_REPLY: &str = "CAN_REPLY";

/// The legacy US endpoint. A DNS alias that routes to the regional one.
pub const MHPNS_LEGACY_US: &str = "https://push.mattermost.com";
/// The legacy German endpoint.
pub const MHPNS_LEGACY_DE: &str = "https://hpns-de.mattermost.com";
pub const MHPNS_GLOBAL: &str = "https://global.push.mattermost.com";
pub const MHPNS_US: &str = "https://us.push.mattermost.com";
pub const MHPNS_EU: &str = "https://eu.push.mattermost.com";
pub const MHPNS_AP: &str = "https://ap.push.mattermost.com";
/// Port of `model.MHPNS` — an alias of [`MHPNS_US`], kept for backwards compatibility. **Not**
/// the global endpoint, despite the bare name.
pub const MHPNS: &str = MHPNS_US;

/// The four delivery-status strings. They are **human sentences**, not identifiers, and they are
/// stored in the notification log — so "Not Sent due to preferences" is a value, not a comment.
pub const PUSH_SEND_PREPARE: &str = "Prepared to send";
pub const PUSH_SEND_SUCCESS: &str = "Successful";
pub const PUSH_NOT_SENT: &str = "Not Sent due to preferences";
pub const PUSH_RECEIVED: &str = "Received by device";

/// Port of `model.PushSubType` (push_notification.go:45) — extra message-type information passed
/// to mobile clients in a backwards-compatible way.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PushSubType(pub String);

impl PushSubType {
    /// Used by the Calls plugin.
    pub const CALLS: &'static str = "calls";

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for PushSubType {
    fn from(s: &str) -> Self {
        PushSubType(s.to_string())
    }
}

/// Port of `model.PushTransport` (push_notification.go:51) — which delivery path the proxy uses.
///
/// The standard transport is the **empty string**, so an unset `transport` is not a missing value
/// but the default one — and with `omitempty` it never reaches the wire.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PushTransport(pub String);

impl PushTransport {
    pub const STANDARD: &'static str = "";
    pub const VOIP: &'static str = "voip";

    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn is_standard(&self) -> bool {
        self.0.is_empty()
    }
}

impl From<&str> for PushTransport {
    fn from(s: &str) -> Self {
        PushTransport(s.to_string())
    }
}

/// Port of `model.PushNotificationAck` (push_notification.go:59) — the device's confirmation.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PushNotificationAck {
    #[serde(rename = "id")]
    pub id: String,

    /// Tagged **`received_at`**, without the `client_` the field name carries.
    #[serde(rename = "received_at")]
    pub client_received_at: i64,

    #[serde(rename = "platform")]
    pub client_platform: String,

    #[serde(rename = "type")]
    pub notification_type: String,

    #[serde(rename = "post_id", skip_serializing_if = "is_empty_str")]
    pub post_id: String,

    #[serde(rename = "is_id_loaded")]
    pub is_id_loaded: bool,
}

/// Port of `model.PushNotification` (push_notification.go:68).
///
/// Almost every optional field carries `omitempty`, but **`ack_id`, `platform`, `server_id`,
/// `device_id`, `post_id`, `is_crt_enabled`, `is_id_loaded` and `signature` do not** — they are
/// always written, empty or not. `post_type` and `channel_type` are `json:"-"`: the server uses
/// them to decide what to send and never forwards them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PushNotification {
    #[serde(rename = "ack_id")]
    pub ack_id: String,

    #[serde(rename = "platform")]
    pub platform: String,

    #[serde(rename = "server_id")]
    pub server_id: String,

    #[serde(rename = "device_id")]
    pub device_id: String,

    #[serde(rename = "post_id")]
    pub post_id: String,

    #[serde(rename = "category", skip_serializing_if = "is_empty_str")]
    pub category: String,

    #[serde(rename = "sound", skip_serializing_if = "is_empty_str")]
    pub sound: String,

    #[serde(rename = "message", skip_serializing_if = "is_empty_str")]
    pub message: String,

    #[serde(rename = "badge", skip_serializing_if = "is_zero_i64")]
    pub badge: i64,

    /// Tagged **`cont_ava`** — APNs' `content-available`, abbreviated.
    #[serde(rename = "cont_ava", skip_serializing_if = "is_zero_i64")]
    pub content_available: i64,

    #[serde(rename = "team_id", skip_serializing_if = "is_empty_str")]
    pub team_id: String,

    #[serde(rename = "channel_id", skip_serializing_if = "is_empty_str")]
    pub channel_id: String,

    #[serde(rename = "root_id", skip_serializing_if = "is_empty_str")]
    pub root_id: String,

    #[serde(rename = "channel_name", skip_serializing_if = "is_empty_str")]
    pub channel_name: String,

    #[serde(rename = "type", skip_serializing_if = "is_empty_str")]
    pub type_: String,

    #[serde(rename = "sub_type", skip_serializing_if = "PushSubType::is_empty")]
    pub sub_type: PushSubType,

    #[serde(
        rename = "transport",
        skip_serializing_if = "PushTransport::is_standard"
    )]
    pub transport: PushTransport,

    #[serde(rename = "sender_id", skip_serializing_if = "is_empty_str")]
    pub sender_id: String,

    #[serde(rename = "sender_name", skip_serializing_if = "is_empty_str")]
    pub sender_name: String,

    #[serde(rename = "override_username", skip_serializing_if = "is_empty_str")]
    pub override_username: String,

    #[serde(rename = "override_icon_url", skip_serializing_if = "is_empty_str")]
    pub override_icon_url: String,

    #[serde(rename = "from_webhook", skip_serializing_if = "is_empty_str")]
    pub from_webhook: String,

    #[serde(rename = "version", skip_serializing_if = "is_empty_str")]
    pub version: String,

    #[serde(rename = "is_crt_enabled")]
    pub is_crt_enabled: bool,

    #[serde(rename = "is_id_loaded")]
    pub is_id_loaded: bool,

    /// `json:"-"`.
    #[serde(skip)]
    pub post_type: String,

    /// `json:"-"`. A `ChannelType` in Go, which is a `String` in this crate.
    #[serde(skip)]
    pub channel_type: String,

    #[serde(rename = "signature")]
    pub signature: String,
}

impl PushSubType {
    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl PushNotification {
    /// Port of `(*PushNotification).DeepCopy` (push_notification.go:105).
    ///
    /// Go's "deep" copy is a **struct copy**, which for this type is genuinely deep only because
    /// every field is a scalar. `Clone` is the same thing; the method is kept so a mechanical
    /// translation of a Go call site lands somewhere.
    pub fn deep_copy(&self) -> PushNotification {
        self.clone()
    }

    /// Port of `(*PushNotification).SetDeviceIdAndPlatform` (push_notification.go:110).
    ///
    /// Splits on the **first** `:`. A device id with no colon leaves **both** fields untouched —
    /// it is not treated as a bare device id — because Go's `strings.Cut` reports `ok == false`
    /// and the whole assignment is skipped.
    pub fn set_device_id_and_platform(&mut self, device_id: &str) {
        if let Some((platform, id)) = device_id.split_once(':') {
            self.platform = platform.to_string();
            self.device_id = id.to_string();
        }
    }
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
    fn push_notification_ack_round_trips_the_fixture() {
        assert_fixture_round_trips!(PushNotificationAck, "push_notification_ack");
    }
    #[test]
    fn push_notification_round_trips_the_fixture() {
        assert_fixture_round_trips!(PushNotification, "push_notification");
    }
}
