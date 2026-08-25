//! Port of `model/websocket_message.go` — the event and response envelopes clients receive.
//!
//! # `ToJSON` has two encoders and they do not agree byte-for-byte
//!
//! The ordinary path marshals `webSocketEventJSON`, producing `{"event":…,"data":…}` with **no
//! space after the colons**. The precomputed path — used when one event is fanned out to many
//! connections — concatenates strings by hand and emits `{"event": …, "data": …}` **with** a
//! space after each colon. Both are valid JSON and every client parses both, but they are not the
//! same bytes, so anything hashing or signing the frame sees two different values depending on a
//! performance optimisation. [`WebSocketEvent::to_json`] and
//! [`WebSocketEvent::to_json_precomputed`] reproduce both exactly.
//!
//! # Three broadcast fields must never reach a client
//!
//! `ReliableClusterSend` carries `json:"-"`, and `BroadcastHooks`/`BroadcastHookArgs` carry
//! `omitempty` with a comment saying they "should never be sent to the client" — so the guard is
//! [`WebSocketEvent::without_broadcast_hooks`], called before send, not the tags. Reproduced.
//!
//! # `Copy` is shallow and `DeepCopy` is not
//!
//! Go's `Copy` shares the data map and the broadcast pointer with the original; only `DeepCopy`
//! clones them. Every setter (`SetEvent`, `SetData`, …) goes through the **shallow** one and
//! returns a new event, so two events can share one mutable data map. Rust's ownership makes that
//! sharing unrepresentable: the builders here clone, which is `DeepCopy`'s behaviour and the
//! safe direction.

use serde::{Deserialize, Serialize};

use crate::serde_helpers::{is_none, is_none_or_empty_map, is_none_or_empty_vec, is_zero_i64};
use crate::utils::{AppError, StringInterface};

/// `model.WebsocketEventTyping`.
pub const WEBSOCKET_EVENT_TYPING: &str = "typing";
/// `model.WebsocketEventPosted`.
pub const WEBSOCKET_EVENT_POSTED: &str = "posted";
/// `model.WebsocketEventPostEdited`.
pub const WEBSOCKET_EVENT_POST_EDITED: &str = "post_edited";
/// `model.WebsocketEventPostDeleted`.
pub const WEBSOCKET_EVENT_POST_DELETED: &str = "post_deleted";
/// `model.WebsocketEventPostUnread`.
pub const WEBSOCKET_EVENT_POST_UNREAD: &str = "post_unread";
/// `model.WebsocketEventChannelConverted`.
pub const WEBSOCKET_EVENT_CHANNEL_CONVERTED: &str = "channel_converted";
/// `model.WebsocketEventChannelCreated`.
pub const WEBSOCKET_EVENT_CHANNEL_CREATED: &str = "channel_created";
/// `model.WebsocketEventChannelDeleted`.
pub const WEBSOCKET_EVENT_CHANNEL_DELETED: &str = "channel_deleted";
/// `model.WebsocketEventChannelRestored`.
pub const WEBSOCKET_EVENT_CHANNEL_RESTORED: &str = "channel_restored";
/// `model.WebsocketEventChannelUpdated`.
pub const WEBSOCKET_EVENT_CHANNEL_UPDATED: &str = "channel_updated";
/// `model.WebsocketEventChannelMemberUpdated`.
pub const WEBSOCKET_EVENT_CHANNEL_MEMBER_UPDATED: &str = "channel_member_updated";
/// `model.WebsocketEventChannelSchemeUpdated`.
pub const WEBSOCKET_EVENT_CHANNEL_SCHEME_UPDATED: &str = "channel_scheme_updated";
/// `model.WebsocketEventDirectAdded`.
pub const WEBSOCKET_EVENT_DIRECT_ADDED: &str = "direct_added";
/// `model.WebsocketEventGroupAdded`.
pub const WEBSOCKET_EVENT_GROUP_ADDED: &str = "group_added";
/// `model.WebsocketEventNewUser`.
pub const WEBSOCKET_EVENT_NEW_USER: &str = "new_user";
/// `model.WebsocketEventAddedToTeam`.
pub const WEBSOCKET_EVENT_ADDED_TO_TEAM: &str = "added_to_team";
/// `model.WebsocketEventLeaveTeam`.
pub const WEBSOCKET_EVENT_LEAVE_TEAM: &str = "leave_team";
/// `model.WebsocketEventUpdateTeam`.
pub const WEBSOCKET_EVENT_UPDATE_TEAM: &str = "update_team";
/// `model.WebsocketEventDeleteTeam`.
pub const WEBSOCKET_EVENT_DELETE_TEAM: &str = "delete_team";
/// `model.WebsocketEventRestoreTeam`.
pub const WEBSOCKET_EVENT_RESTORE_TEAM: &str = "restore_team";
/// `model.WebsocketEventUpdateTeamScheme`.
pub const WEBSOCKET_EVENT_UPDATE_TEAM_SCHEME: &str = "update_team_scheme";
/// `model.WebsocketEventUserAdded`.
pub const WEBSOCKET_EVENT_USER_ADDED: &str = "user_added";
/// `model.WebsocketEventUserUpdated`.
pub const WEBSOCKET_EVENT_USER_UPDATED: &str = "user_updated";
/// `model.WebsocketEventUserRoleUpdated`.
pub const WEBSOCKET_EVENT_USER_ROLE_UPDATED: &str = "user_role_updated";
/// `model.WebsocketEventMemberroleUpdated`.
pub const WEBSOCKET_EVENT_MEMBERROLE_UPDATED: &str = "memberrole_updated";
/// `model.WebsocketEventUserRemoved`.
pub const WEBSOCKET_EVENT_USER_REMOVED: &str = "user_removed";
/// `model.WebsocketEventPreferenceChanged`.
pub const WEBSOCKET_EVENT_PREFERENCE_CHANGED: &str = "preference_changed";
/// `model.WebsocketEventPreferencesChanged`.
pub const WEBSOCKET_EVENT_PREFERENCES_CHANGED: &str = "preferences_changed";
/// `model.WebsocketEventPreferencesDeleted`.
pub const WEBSOCKET_EVENT_PREFERENCES_DELETED: &str = "preferences_deleted";
/// `model.WebsocketEventEphemeralMessage`.
pub const WEBSOCKET_EVENT_EPHEMERAL_MESSAGE: &str = "ephemeral_message";
/// `model.WebsocketEventStatusChange`.
pub const WEBSOCKET_EVENT_STATUS_CHANGE: &str = "status_change";
/// `model.WebsocketEventHello`.
pub const WEBSOCKET_EVENT_HELLO: &str = "hello";
/// `model.WebsocketAuthenticationChallenge`.
pub const WEBSOCKET_AUTHENTICATION_CHALLENGE: &str = "authentication_challenge";
/// `model.WebsocketEventReactionAdded`.
pub const WEBSOCKET_EVENT_REACTION_ADDED: &str = "reaction_added";
/// `model.WebsocketEventReactionRemoved`.
pub const WEBSOCKET_EVENT_REACTION_REMOVED: &str = "reaction_removed";
/// `model.WebsocketEventResponse`.
pub const WEBSOCKET_EVENT_RESPONSE: &str = "response";
/// `model.WebsocketEventEmojiAdded`.
pub const WEBSOCKET_EVENT_EMOJI_ADDED: &str = "emoji_added";
/// `model.WebsocketEventMultipleChannelsViewed`.
pub const WEBSOCKET_EVENT_MULTIPLE_CHANNELS_VIEWED: &str = "multiple_channels_viewed";
/// `model.WebsocketEventPluginStatusesChanged`.
pub const WEBSOCKET_EVENT_PLUGIN_STATUSES_CHANGED: &str = "plugin_statuses_changed";
/// `model.WebsocketEventPluginEnabled`.
pub const WEBSOCKET_EVENT_PLUGIN_ENABLED: &str = "plugin_enabled";
/// `model.WebsocketEventPluginDisabled`.
pub const WEBSOCKET_EVENT_PLUGIN_DISABLED: &str = "plugin_disabled";
/// `model.WebsocketEventRoleUpdated`.
pub const WEBSOCKET_EVENT_ROLE_UPDATED: &str = "role_updated";
/// `model.WebsocketEventLicenseChanged`.
pub const WEBSOCKET_EVENT_LICENSE_CHANGED: &str = "license_changed";
/// `model.WebsocketEventConfigChanged`.
pub const WEBSOCKET_EVENT_CONFIG_CHANGED: &str = "config_changed";
/// `model.WebsocketEventOpenDialog`.
pub const WEBSOCKET_EVENT_OPEN_DIALOG: &str = "open_dialog";
/// `model.WebsocketEventGuestsDeactivated`.
pub const WEBSOCKET_EVENT_GUESTS_DEACTIVATED: &str = "guests_deactivated";
/// `model.WebsocketEventUserActivationStatusChange`.
pub const WEBSOCKET_EVENT_USER_ACTIVATION_STATUS_CHANGE: &str = "user_activation_status_change";
/// `model.WebsocketEventReceivedGroup`.
pub const WEBSOCKET_EVENT_RECEIVED_GROUP: &str = "received_group";
/// `model.WebsocketEventReceivedGroupAssociatedToTeam`.
pub const WEBSOCKET_EVENT_RECEIVED_GROUP_ASSOCIATED_TO_TEAM: &str =
    "received_group_associated_to_team";
/// `model.WebsocketEventReceivedGroupNotAssociatedToTeam`.
pub const WEBSOCKET_EVENT_RECEIVED_GROUP_NOT_ASSOCIATED_TO_TEAM: &str =
    "received_group_not_associated_to_team";
/// `model.WebsocketEventReceivedGroupAssociatedToChannel`.
pub const WEBSOCKET_EVENT_RECEIVED_GROUP_ASSOCIATED_TO_CHANNEL: &str =
    "received_group_associated_to_channel";
/// `model.WebsocketEventReceivedGroupNotAssociatedToChannel`.
pub const WEBSOCKET_EVENT_RECEIVED_GROUP_NOT_ASSOCIATED_TO_CHANNEL: &str =
    "received_group_not_associated_to_channel";
/// `model.WebsocketEventGroupMemberDelete`.
pub const WEBSOCKET_EVENT_GROUP_MEMBER_DELETE: &str = "group_member_deleted";
/// `model.WebsocketEventGroupMemberAdd`.
pub const WEBSOCKET_EVENT_GROUP_MEMBER_ADD: &str = "group_member_add";
/// `model.WebsocketEventSidebarCategoryCreated`.
pub const WEBSOCKET_EVENT_SIDEBAR_CATEGORY_CREATED: &str = "sidebar_category_created";
/// `model.WebsocketEventSidebarCategoryUpdated`.
pub const WEBSOCKET_EVENT_SIDEBAR_CATEGORY_UPDATED: &str = "sidebar_category_updated";
/// `model.WebsocketEventSidebarCategoryDeleted`.
pub const WEBSOCKET_EVENT_SIDEBAR_CATEGORY_DELETED: &str = "sidebar_category_deleted";
/// `model.WebsocketEventSidebarCategoryOrderUpdated`.
pub const WEBSOCKET_EVENT_SIDEBAR_CATEGORY_ORDER_UPDATED: &str = "sidebar_category_order_updated";
/// `model.WebsocketEventCloudSubscriptionChanged`.
pub const WEBSOCKET_EVENT_CLOUD_SUBSCRIPTION_CHANGED: &str = "cloud_subscription_changed";
/// `model.WebsocketEventThreadUpdated`.
pub const WEBSOCKET_EVENT_THREAD_UPDATED: &str = "thread_updated";
/// `model.WebsocketEventThreadFollowChanged`.
pub const WEBSOCKET_EVENT_THREAD_FOLLOW_CHANGED: &str = "thread_follow_changed";
/// `model.WebsocketEventThreadReadChanged`.
pub const WEBSOCKET_EVENT_THREAD_READ_CHANGED: &str = "thread_read_changed";
/// `model.WebsocketFirstAdminVisitMarketplaceStatusReceived`.
pub const WEBSOCKET_FIRST_ADMIN_VISIT_MARKETPLACE_STATUS_RECEIVED: &str =
    "first_admin_visit_marketplace_status_received";
/// `model.WebsocketEventDraftCreated`.
pub const WEBSOCKET_EVENT_DRAFT_CREATED: &str = "draft_created";
/// `model.WebsocketEventDraftUpdated`.
pub const WEBSOCKET_EVENT_DRAFT_UPDATED: &str = "draft_updated";
/// `model.WebsocketEventDraftDeleted`.
pub const WEBSOCKET_EVENT_DRAFT_DELETED: &str = "draft_deleted";
/// `model.WebsocketEventAcknowledgementAdded`.
pub const WEBSOCKET_EVENT_ACKNOWLEDGEMENT_ADDED: &str = "post_acknowledgement_added";
/// `model.WebsocketEventAcknowledgementRemoved`.
pub const WEBSOCKET_EVENT_ACKNOWLEDGEMENT_REMOVED: &str = "post_acknowledgement_removed";
/// `model.WebsocketEventPersistentNotificationTriggered`.
pub const WEBSOCKET_EVENT_PERSISTENT_NOTIFICATION_TRIGGERED: &str =
    "persistent_notification_triggered";
/// `model.WebsocketEventHostedCustomerSignupProgressUpdated`.
pub const WEBSOCKET_EVENT_HOSTED_CUSTOMER_SIGNUP_PROGRESS_UPDATED: &str =
    "hosted_customer_signup_progress_updated";
/// `model.WebsocketEventChannelBookmarkCreated`.
pub const WEBSOCKET_EVENT_CHANNEL_BOOKMARK_CREATED: &str = "channel_bookmark_created";
/// `model.WebsocketEventChannelBookmarkUpdated`.
pub const WEBSOCKET_EVENT_CHANNEL_BOOKMARK_UPDATED: &str = "channel_bookmark_updated";
/// `model.WebsocketEventChannelBookmarkDeleted`.
pub const WEBSOCKET_EVENT_CHANNEL_BOOKMARK_DELETED: &str = "channel_bookmark_deleted";
/// `model.WebsocketEventChannelBookmarkSorted`.
pub const WEBSOCKET_EVENT_CHANNEL_BOOKMARK_SORTED: &str = "channel_bookmark_sorted";
/// `model.WebsocketEventChannelAccessControlUpdated`.
pub const WEBSOCKET_EVENT_CHANNEL_ACCESS_CONTROL_UPDATED: &str = "channel_access_control_updated";
/// `model.WebsocketEventTeamAccessControlUpdated`.
pub const WEBSOCKET_EVENT_TEAM_ACCESS_CONTROL_UPDATED: &str = "team_access_control_updated";
/// `model.WebsocketPresenceIndicator`.
pub const WEBSOCKET_PRESENCE_INDICATOR: &str = "presence";
/// `model.WebsocketPostedNotifyAck`.
pub const WEBSOCKET_POSTED_NOTIFY_ACK: &str = "posted_notify_ack";
/// `model.WebsocketScheduledPostCreated`.
pub const WEBSOCKET_SCHEDULED_POST_CREATED: &str = "scheduled_post_created";
/// `model.WebsocketScheduledPostUpdated`.
pub const WEBSOCKET_SCHEDULED_POST_UPDATED: &str = "scheduled_post_updated";
/// `model.WebsocketScheduledPostDeleted`.
pub const WEBSOCKET_SCHEDULED_POST_DELETED: &str = "scheduled_post_deleted";
/// `model.WebsocketEventCPAFieldCreated`.
pub const WEBSOCKET_EVENT_CPA_FIELD_CREATED: &str = "custom_profile_attributes_field_created";
/// `model.WebsocketEventCPAFieldUpdated`.
pub const WEBSOCKET_EVENT_CPA_FIELD_UPDATED: &str = "custom_profile_attributes_field_updated";
/// `model.WebsocketEventCPAFieldDeleted`.
pub const WEBSOCKET_EVENT_CPA_FIELD_DELETED: &str = "custom_profile_attributes_field_deleted";
/// `model.WebsocketEventCPAValuesUpdated`.
pub const WEBSOCKET_EVENT_CPA_VALUES_UPDATED: &str = "custom_profile_attributes_values_updated";
/// `model.WebsocketContentFlaggingReportValueUpdated`.
pub const WEBSOCKET_CONTENT_FLAGGING_REPORT_VALUE_UPDATED: &str =
    "content_flagging_report_value_updated";
/// `model.WebsocketEventJobUpdated`.
pub const WEBSOCKET_EVENT_JOB_UPDATED: &str = "job_updated";
/// `model.WebsocketEventRecapUpdated`.
pub const WEBSOCKET_EVENT_RECAP_UPDATED: &str = "recap_updated";
/// `model.WebsocketEventPostTranslationUpdated`.
pub const WEBSOCKET_EVENT_POST_TRANSLATION_UPDATED: &str = "post_translation_updated";
/// `model.WebsocketEventPostRevealed`.
pub const WEBSOCKET_EVENT_POST_REVEALED: &str = "post_revealed";
/// `model.WebsocketEventPostBurned`.
pub const WEBSOCKET_EVENT_POST_BURNED: &str = "post_burned";
/// `model.WebsocketEventBurnOnReadAllRevealed`.
pub const WEBSOCKET_EVENT_BURN_ON_READ_ALL_REVEALED: &str = "burn_on_read_all_revealed";
/// `model.WebsocketEventBoardCreated`.
pub const WEBSOCKET_EVENT_BOARD_CREATED: &str = "board_created";
/// `model.WebsocketEventViewCreated`.
pub const WEBSOCKET_EVENT_VIEW_CREATED: &str = "view_created";
/// `model.WebsocketEventViewUpdated`.
pub const WEBSOCKET_EVENT_VIEW_UPDATED: &str = "view_updated";
/// `model.WebsocketEventViewDeleted`.
pub const WEBSOCKET_EVENT_VIEW_DELETED: &str = "view_deleted";
/// `model.WebsocketEventViewSorted`.
pub const WEBSOCKET_EVENT_VIEW_SORTED: &str = "view_sorted";
/// `model.WebsocketEventPropertyFieldCreated`.
pub const WEBSOCKET_EVENT_PROPERTY_FIELD_CREATED: &str = "property_field_created";
/// `model.WebsocketEventPropertyFieldUpdated`.
pub const WEBSOCKET_EVENT_PROPERTY_FIELD_UPDATED: &str = "property_field_updated";
/// `model.WebsocketEventPropertyFieldDeleted`.
pub const WEBSOCKET_EVENT_PROPERTY_FIELD_DELETED: &str = "property_field_deleted";
/// `model.WebsocketEventPropertyValuesUpdated`.
pub const WEBSOCKET_EVENT_PROPERTY_VALUES_UPDATED: &str = "property_values_updated";
/// `model.WebsocketEventFileDownloadRejected`.
pub const WEBSOCKET_EVENT_FILE_DOWNLOAD_REJECTED: &str = "file_download_rejected";
/// `model.WebsocketEventFileUploadRejected`.
pub const WEBSOCKET_EVENT_FILE_UPLOAD_REJECTED: &str = "file_upload_rejected";
/// `model.WebsocketEventShowToast`.
pub const WEBSOCKET_EVENT_SHOW_TOAST: &str = "show_toast";
/// `model.WebsocketEventSharedChannelRemoteUpdated`.
pub const WEBSOCKET_EVENT_SHARED_CHANNEL_REMOTE_UPDATED: &str = "shared_channel_remote_updated";
/// `model.WebsocketEventChannelJoinRequestCreated`.
pub const WEBSOCKET_EVENT_CHANNEL_JOIN_REQUEST_CREATED: &str = "channel_join_request_created";
/// `model.WebsocketEventChannelJoinRequestUpdated`.
pub const WEBSOCKET_EVENT_CHANNEL_JOIN_REQUEST_UPDATED: &str = "channel_join_request_updated";

/// Go's `StatusFail`, which lives in `client4.go` — a file this project never reads (CLAUDE.md).
/// The value is repeated here rather than imported so nothing depends on that file.
pub const WEBSOCKET_STATUS_FAIL: &str = "FAIL";

/// Port of `model.WebSocketMsgTypeResponse` (websocket_message.go:126).
pub const WEB_SOCKET_MSG_TYPE_RESPONSE: &str = "response";
/// Port of `model.WebSocketMsgTypeEvent` (websocket_message.go:127).
pub const WEB_SOCKET_MSG_TYPE_EVENT: &str = "event";

/// Port of `model.ActiveQueueItem` (websocket_message.go:130).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ActiveQueueItem {
    /// [`WEB_SOCKET_MSG_TYPE_EVENT`] or [`WEB_SOCKET_MSG_TYPE_RESPONSE`] — which of the two
    /// shapes `buf` holds.
    #[serde(rename = "type")]
    pub type_: String,

    /// The already-encoded frame. A `json.RawMessage` in Go.
    #[serde(rename = "buf")]
    pub buf: serde_json::Value,
}

/// Port of `model.WSQueues` (websocket_message.go:135) — a connection's queued frames, used by
/// the reconnect path.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WSQueues {
    /// Events **and** responses.
    #[serde(rename = "active_queue")]
    pub active_q: Option<Vec<ActiveQueueItem>>,

    /// Events only — the dead queue holds frames that could not be delivered.
    #[serde(rename = "dead_queue")]
    pub dead_q: Option<Vec<serde_json::Value>>,

    #[serde(rename = "reuse_count")]
    pub reuse_count: i64,
}

/// Port of `model.WebsocketBroadcast` (websocket_message.go:147) — who an event goes to.
///
/// The five addressing fields are **filters, not a union**: an event with both `user_id` and
/// `channel_id` reaches only that user, and only if they are in that channel. `omit_users` and
/// `omit_connection_id` subtract from whatever the filters selected.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct WebsocketBroadcast {
    /// Users to skip. The bool is always `true` in practice — it is a set.
    #[serde(rename = "omit_users")]
    pub omit_users: Option<std::collections::BTreeMap<String, bool>>,

    #[serde(rename = "user_id")]
    pub user_id: String,

    #[serde(rename = "channel_id")]
    pub channel_id: String,

    #[serde(rename = "team_id")]
    pub team_id: String,

    #[serde(rename = "connection_id")]
    pub connection_id: String,

    #[serde(rename = "omit_connection_id")]
    pub omit_connection_id: String,

    /// Send **only** to non-sysadmins.
    #[serde(
        rename = "contains_sanitized_data",
        skip_serializing_if = "crate::serde_helpers::is_false"
    )]
    pub contains_sanitized_data: bool,

    /// Send **only** to sysadmins.
    #[serde(
        rename = "contains_sensitive_data",
        skip_serializing_if = "crate::serde_helpers::is_false"
    )]
    pub contains_sensitive_data: bool,

    /// `json:"-"` — whether to cross the cluster on the reliable, TCP-backed channel.
    #[serde(skip)]
    pub reliable_cluster_send: bool,

    /// Never sent to a client — see the module docs.
    #[serde(
        rename = "broadcast_hooks",
        skip_serializing_if = "is_none_or_empty_vec"
    )]
    pub broadcast_hooks: Option<Vec<String>>,

    /// Positionally paired with `broadcast_hooks`. Never sent to a client.
    #[serde(
        rename = "broadcast_hook_args",
        skip_serializing_if = "is_none_or_empty_vec"
    )]
    pub broadcast_hook_args: Option<Vec<StringInterface>>,
}

impl WebsocketBroadcast {
    /// Port of `(*WebsocketBroadcast).copy` (websocket_message.go:172).
    ///
    /// **Go's copy silently drops `ConnectionId` and `ReliableClusterSend`** — it assigns nine of
    /// the eleven fields by hand and omits those two. Reproduced: `without_broadcast_hooks`
    /// depends on it, so "fixing" it would change which connections an event reaches.
    pub fn copy_like_go(&self) -> WebsocketBroadcast {
        WebsocketBroadcast {
            omit_users: self.omit_users.clone(),
            user_id: self.user_id.clone(),
            channel_id: self.channel_id.clone(),
            team_id: self.team_id.clone(),
            // Not copied by Go:
            connection_id: String::new(),
            omit_connection_id: self.omit_connection_id.clone(),
            contains_sanitized_data: self.contains_sanitized_data,
            contains_sensitive_data: self.contains_sensitive_data,
            // Not copied by Go:
            reliable_cluster_send: false,
            broadcast_hooks: self.broadcast_hooks.clone(),
            broadcast_hook_args: self.broadcast_hook_args.clone(),
        }
    }

    /// Port of `(*WebsocketBroadcast).AddHook` (websocket_message.go:194) — appends to both
    /// slices so the indexes stay paired.
    pub fn add_hook(&mut self, hook_id: impl Into<String>, hook_args: StringInterface) {
        self.broadcast_hooks
            .get_or_insert_with(Vec::new)
            .push(hook_id.into());
        self.broadcast_hook_args
            .get_or_insert_with(Vec::new)
            .push(hook_args);
    }
}

/// Port of `model.WebSocketEvent` (websocket_message.go:239).
///
/// Go keeps every field unexported and marshals through `webSocketEventJSON`; the wire shape is
/// `event`, `data`, `broadcast`, **`seq`** — note the last key is not `sequence`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WebSocketEvent {
    #[serde(rename = "event")]
    pub event: String,

    #[serde(rename = "data")]
    pub data: Option<StringInterface>,

    #[serde(rename = "broadcast")]
    pub broadcast: Option<Box<WebsocketBroadcast>>,

    #[serde(rename = "seq")]
    pub sequence: i64,

    /// Go's `rejected` field. Unexported **and** absent from `webSocketEventJSON`, so it never
    /// crosses the wire and does not survive a round trip.
    #[serde(skip)]
    pub rejected: bool,
}

impl WebSocketEvent {
    /// Port of `model.NewWebSocketEvent` (websocket_message.go:290).
    pub fn new(
        event: impl Into<String>,
        team_id: &str,
        channel_id: &str,
        user_id: &str,
        omit_users: Option<std::collections::BTreeMap<String, bool>>,
        omit_connection_id: &str,
    ) -> Self {
        Self {
            event: event.into(),
            // Go allocates an empty map, so `data` is `{}` rather than `null` on the wire.
            data: Some(StringInterface::new()),
            broadcast: Some(Box::new(WebsocketBroadcast {
                team_id: team_id.to_string(),
                channel_id: channel_id.to_string(),
                user_id: user_id.to_string(),
                omit_users,
                omit_connection_id: omit_connection_id.to_string(),
                ..Default::default()
            })),
            sequence: 0,
            rejected: false,
        }
    }

    /// Port of `(*WebSocketEvent).Add` (websocket_message.go:287).
    pub fn add(&mut self, key: impl Into<String>, value: serde_json::Value) {
        self.data
            .get_or_insert_with(StringInterface::new)
            .insert(key.into(), value);
    }

    /// Port of `(*WebSocketEvent).GetData` (websocket_message.go:326).
    pub fn get_data(&self) -> Option<&StringInterface> {
        self.data.as_ref()
    }

    /// Port of `(*WebSocketEvent).GetBroadcast` (websocket_message.go:330).
    pub fn get_broadcast(&self) -> Option<&WebsocketBroadcast> {
        self.broadcast.as_deref()
    }

    /// Port of `(*WebSocketEvent).GetSequence` (websocket_message.go:334).
    pub fn get_sequence(&self) -> i64 {
        self.sequence
    }

    /// Port of `(*WebSocketEvent).SetEvent` (websocket_message.go:338) — returns a copy, as all
    /// four setters do.
    #[must_use]
    pub fn set_event(&self, event: impl Into<String>) -> WebSocketEvent {
        let mut copy = self.clone();
        copy.event = event.into();
        copy
    }

    /// Port of `(*WebSocketEvent).SetData` (websocket_message.go:344).
    #[must_use]
    pub fn set_data(&self, data: StringInterface) -> WebSocketEvent {
        let mut copy = self.clone();
        copy.data = Some(data);
        copy
    }

    /// Port of `(*WebSocketEvent).SetBroadcast` (websocket_message.go:350).
    #[must_use]
    pub fn set_broadcast(&self, broadcast: WebsocketBroadcast) -> WebSocketEvent {
        let mut copy = self.clone();
        copy.broadcast = Some(Box::new(broadcast));
        copy
    }

    /// Port of `(*WebSocketEvent).SetSequence` (websocket_message.go:356).
    #[must_use]
    pub fn set_sequence(&self, seq: i64) -> WebSocketEvent {
        let mut copy = self.clone();
        copy.sequence = seq;
        copy
    }

    /// Port of `(*WebSocketEvent).WithoutBroadcastHooks` (websocket_message.go:268).
    ///
    /// Returns the event with the hook fields cleared, plus the hooks that were removed. When
    /// there are none, Go returns the **original** event untouched; the same short-circuit is kept
    /// here so the common path does not clone.
    ///
    /// Note the copy goes through [`WebsocketBroadcast::copy_like_go`], which drops
    /// `connection_id` — so an event that had one loses it here.
    pub fn without_broadcast_hooks(&self) -> (WebSocketEvent, Vec<String>, Vec<StringInterface>) {
        let hooks = self
            .broadcast
            .as_ref()
            .and_then(|b| b.broadcast_hooks.clone())
            .unwrap_or_default();
        let hook_args = self
            .broadcast
            .as_ref()
            .and_then(|b| b.broadcast_hook_args.clone())
            .unwrap_or_default();

        if hooks.is_empty() && hook_args.is_empty() {
            return (self.clone(), hooks, hook_args);
        }

        let mut copy = self.clone();
        if let Some(broadcast) = &self.broadcast {
            let mut new_broadcast = broadcast.copy_like_go();
            new_broadcast.broadcast_hooks = None;
            new_broadcast.broadcast_hook_args = None;
            copy.broadcast = Some(Box::new(new_broadcast));
        }

        (copy, hooks, hook_args)
    }

    /// Port of `(*WebSocketEvent).IsValid` (websocket_message.go:362) — **only** that the event
    /// type is non-empty.
    pub fn is_valid(&self) -> bool {
        !self.event.is_empty()
    }

    /// Port of `(*WebSocketEvent).EventType` (websocket_message.go:366).
    pub fn event_type(&self) -> &str {
        &self.event
    }

    /// Port of `(*WebSocketEvent).ToJSON` (websocket_message.go:370), non-precomputed path.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        crate::utils::go_json_marshal(self)
    }

    /// Port of `precomputedJSONBuf` (websocket_message.go:397) — the hand-concatenated form,
    /// **with a space after each colon**. See the module docs.
    ///
    /// Go pre-marshals `event`, `data` and `broadcast` once and reuses them per connection; the
    /// only per-connection part is `seq`, which it renders with `strconv.Itoa(int(seq))` —
    /// a **32-bit truncation on a 32-bit platform**, though not on any this server runs on.
    pub fn to_json_precomputed(&self) -> Result<String, serde_json::Error> {
        let event = crate::utils::go_json_marshal(&self.event)?;
        let data = crate::utils::go_json_marshal(&self.data)?;
        let broadcast = crate::utils::go_json_marshal(&self.broadcast)?;
        Ok(format!(
            r#"{{"event": {event}, "data": {data}, "broadcast": {broadcast}, "seq": {}}}"#,
            self.sequence
        ))
    }

    /// Port of `(*WebSocketEvent).Reject` (websocket_message.go:471).
    pub fn reject(&mut self) {
        self.rejected = true;
    }

    /// Port of `(*WebSocketEvent).IsRejected` (websocket_message.go:475).
    pub fn is_rejected(&self) -> bool {
        self.rejected
    }

    /// Port of `model.WebSocketEventFromJSON` (websocket_message.go:409).
    ///
    /// Go re-marshals `data["user"]` and decodes it into a `*User`, replacing the map — so a
    /// decoded event's `user` entry is a typed value, not a map. Here `data` stays
    /// `serde_json::Value` throughout, and a caller that wants a `User` decodes that one key
    /// itself; the wire result of re-encoding is identical because `User` round-trips.
    pub fn from_json(data: &[u8]) -> Result<WebSocketEvent, serde_json::Error> {
        serde_json::from_slice(data)
    }
}

/// Port of `model.WebSocketResponse` (websocket_message.go:436) — the reply to a
/// [`crate::websocket_request::WebSocketRequest`].
///
/// **Neither `Clone` nor `PartialEq`**, unlike every other type in this crate: `AppError` owns a
/// boxed `dyn Error` for the wrapped cause, which is neither clonable nor comparable. Deriving
/// them would mean dropping the wrapped error, and the wrapped error is what
/// `AppError::to_json` folds into `detailed_error` — so a lossy `Clone` would produce a response
/// that serialises differently from the one it was cloned from.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct WebSocketResponse {
    /// `OK` or `FAIL`.
    #[serde(rename = "status")]
    pub status: String,

    /// Echoes the request's `seq`.
    #[serde(rename = "seq_reply", skip_serializing_if = "is_zero_i64")]
    pub seq_reply: i64,

    #[serde(rename = "data", skip_serializing_if = "is_none_or_empty_map")]
    pub data: Option<StringInterface>,

    #[serde(rename = "error", skip_serializing_if = "is_none")]
    pub error: Option<Box<AppError>>,
}

impl WebSocketResponse {
    /// Port of `model.NewWebSocketResponse` (websocket_message.go:447).
    pub fn new(status: impl Into<String>, seq_reply: i64, data: Option<StringInterface>) -> Self {
        Self {
            status: status.into(),
            seq_reply,
            data,
            error: None,
        }
    }

    /// Port of `model.NewWebSocketError` (websocket_message.go:451) — status is always `FAIL`,
    /// and `data` is left nil.
    pub fn new_error(seq_reply: i64, err: AppError) -> Self {
        Self {
            status: WEBSOCKET_STATUS_FAIL.to_string(),
            seq_reply,
            data: None,
            error: Some(Box::new(err)),
        }
    }

    /// Port of `(*WebSocketResponse).Add` (websocket_message.go:443).
    pub fn add(&mut self, key: impl Into<String>, value: serde_json::Value) {
        self.data
            .get_or_insert_with(StringInterface::new)
            .insert(key.into(), value);
    }

    /// Port of `(*WebSocketResponse).IsValid` (websocket_message.go:455).
    pub fn is_valid(&self) -> bool {
        !self.status.is_empty()
    }

    /// Port of `(*WebSocketResponse).EventType` (websocket_message.go:459) — always
    /// [`WEBSOCKET_EVENT_RESPONSE`].
    pub fn event_type(&self) -> &'static str {
        WEBSOCKET_EVENT_RESPONSE
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
    fn active_queue_item_round_trips_the_fixture() {
        assert_fixture_round_trips!(ActiveQueueItem, "active_queue_item");
    }
    #[test]
    fn ws_queues_round_trips_the_fixture() {
        assert_fixture_round_trips!(WSQueues, "ws_queues");
    }
    #[test]
    fn websocket_broadcast_round_trips_the_fixture() {
        assert_fixture_round_trips!(WebsocketBroadcast, "websocket_broadcast");
    }
    #[test]
    fn web_socket_response_round_trips_the_fixture() {
        assert_fixture_round_trips!(WebSocketResponse, "web_socket_response");
    }
}
