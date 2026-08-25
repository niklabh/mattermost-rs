//! Port of `model/notification.go` — the vocabulary of the notification-delivery log.
//!
//! Three `string` newtypes and 33 constants. `NotificationNoPlatform` is the odd one out: it is
//! **untyped** in Go, so it is not a `NotificationReason` despite sitting in the middle of the
//! block that defines them.

use serde::{Deserialize, Serialize};

macro_rules! string_newtype {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(
            Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(pub String);

        impl $name {
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl From<&str> for $name {
            fn from(s: &str) -> Self {
                $name(s.to_string())
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

string_newtype!(
    /// Port of `model.NotificationStatus` (notification.go:3).
    NotificationStatus
);
string_newtype!(
    /// Port of `model.NotificationType` (notification.go:4).
    NotificationType
);
string_newtype!(
    /// Port of `model.NotificationReason` (notification.go:5).
    NotificationReason
);

/// The `NotificationStatus` values (notification.go:8).
pub mod status {
    pub const SUCCESS: &str = "success";
    pub const ERROR: &str = "error";
    pub const NOT_SENT: &str = "not_sent";
    pub const UNSUPPORTED: &str = "unsupported";
}

/// The `NotificationType` values (notification.go:13).
pub mod type_ {
    pub const ALL: &str = "all";
    pub const EMAIL: &str = "email";
    pub const WEBSOCKET: &str = "websocket";
    pub const PUSH: &str = "push";
}

/// Port of `model.NotificationNoPlatform` (notification.go:18) — **untyped** in Go, unlike every
/// constant around it.
pub const NOTIFICATION_NO_PLATFORM: &str = "no_platform";

/// The `NotificationReason` values (notification.go:20).
pub mod reason {
    pub const FETCH_ERROR: &str = "fetch_error";
    /// The constant is `…ParseError`; the value spells out `json_parse_error`.
    pub const PARSE_ERROR: &str = "json_parse_error";
    pub const MARSHAL_ERROR: &str = "json_marshal_error";
    pub const PUSH_PROXY_ERROR: &str = "push_proxy_error";
    pub const PUSH_PROXY_SEND_ERROR: &str = "push_proxy_send_error";
    pub const PUSH_PROXY_REMOVE_DEVICE: &str = "push_proxy_remove_device";
    pub const REJECTED_BY_PLUGIN: &str = "rejected_by_plugin";
    pub const SESSION_EXPIRED: &str = "session_expired";
    pub const CHANNEL_MUTED: &str = "channel_muted";
    pub const SYSTEM_MESSAGE: &str = "system_message";
    /// The constant is `…Silent`; the value is `silent_notification`.
    pub const SILENT: &str = "silent_notification";
    /// The constant is `…LevelSetToNone`; the value is `notify_level_none`.
    pub const LEVEL_SET_TO_NONE: &str = "notify_level_none";
    pub const NOT_MENTIONED: &str = "not_mentioned";
    pub const USER_STATUS: &str = "user_status";
    pub const USER_IS_ACTIVE: &str = "user_is_active";
    pub const MISSING_PROFILE: &str = "missing_profile";
    pub const EMAIL_NOT_VERIFIED: &str = "email_not_verified";
    pub const EMAIL_SEND_ERROR: &str = "email_send_error";
    pub const TOO_MANY_USERS_IN_CHANNEL: &str = "too_many_users_in_channel";
    pub const RESOLVE_PERSISTENT_NOTIFICATION_ERROR: &str = "resolve_persistent_notification_error";
    pub const MISSING_THREAD_MEMBERSHIP: &str = "missing_thread_membership";
    pub const RECIPIENT_IS_BOT: &str = "recipient_is_bot";
}
