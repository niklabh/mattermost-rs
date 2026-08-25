//! Port of `model/content_flagging_report.go` — the on-disk shape of a flagged-post report.
//!
//! # This file is YAML, and its layout is deliberately hand-written
//!
//! `FlaggedPostReportPost` embeds `*Post` but declares `MarshalYAML`, so **`Post`'s own field
//! tags do not decide the report layout** — the method does, and it emits a fixed set of keys.
//! That is the point: the report format must not drift when `Post` gains a field.
//!
//! There is no YAML codec in this crate, so the two tagged structs are ported as plain data and
//! the marshal method is ported as [`FlaggedPostReportPost::to_map`], which produces exactly the
//! map Go hands to the YAML encoder. A caller with a YAML encoder serialises that.

use crate::channel::Channel;
use crate::post::Post;
use crate::team::Team;
use crate::user::User;
use crate::utils::StringInterface;

/// Port of `model.FlaggedPostReportVersion` (content_flagging_report.go:5).
pub const FLAGGED_POST_REPORT_VERSION: &str = "1.0";

/// Port of `model.FlaggedPostReportContext` (content_flagging_report.go:7) — everything the
/// report generator needs, before projection.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FlaggedPostReportContext {
    pub post: Option<Box<Post>>,
    pub channel: Option<Box<Channel>>,
    pub team: Option<Box<Team>>,
    pub author: Option<Box<User>>,
    /// Prior revisions, oldest first.
    pub edit_history: Vec<Post>,
}

/// Port of `model.FlaggedPostReportPost` (content_flagging_report.go:18) — `post.yaml`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FlaggedPostReportPost {
    pub post: Option<Box<Post>>,

    pub author_name: String,
    pub author_email: String,
    pub channel_display_name: String,
    pub team_id: String,
    pub team_display_name: String,
    /// A `*int64`: the key is omitted entirely when absent, rather than written as zero.
    pub reply_count: Option<i64>,
    /// Omitted when empty.
    pub edit_history_order: Vec<String>,
}

impl FlaggedPostReportPost {
    /// Port of `(FlaggedPostReportPost).MarshalYAML` (content_flagging_report.go:30).
    ///
    /// Five keys are always written; the rest are conditional:
    ///
    /// - the eight post keys only when the post is present;
    /// - `props` only when non-empty, and `metadata` only when non-nil;
    /// - `reply_count` only when the pointer is set;
    /// - `edit_history_order` only when non-empty.
    ///
    /// Note what is **not** exported: `type`, `delete_at`, `edit_at`, `original_id`, `file_ids`,
    /// `hashtags`. A reader treating this as "the post" will find them missing.
    pub fn to_map(&self) -> StringInterface {
        let mut out = StringInterface::new();
        out.insert(
            "author_name".to_string(),
            serde_json::Value::String(self.author_name.clone()),
        );
        out.insert(
            "author_email".to_string(),
            serde_json::Value::String(self.author_email.clone()),
        );
        out.insert(
            "channel_display_name".to_string(),
            serde_json::Value::String(self.channel_display_name.clone()),
        );
        out.insert(
            "team_id".to_string(),
            serde_json::Value::String(self.team_id.clone()),
        );
        out.insert(
            "team_display_name".to_string(),
            serde_json::Value::String(self.team_display_name.clone()),
        );

        if let Some(post) = &self.post {
            out.insert("id".to_string(), serde_json::Value::String(post.id.clone()));
            // The post's `user_id` is exported as **`author_id`**.
            out.insert(
                "author_id".to_string(),
                serde_json::Value::String(post.user_id.clone()),
            );
            out.insert(
                "message".to_string(),
                serde_json::Value::String(post.message.clone()),
            );
            out.insert(
                "channel_id".to_string(),
                serde_json::Value::String(post.channel_id.clone()),
            );
            out.insert("create_at".to_string(), serde_json::json!(post.create_at));
            out.insert("update_at".to_string(), serde_json::json!(post.update_at));
            out.insert("is_pinned".to_string(), serde_json::json!(post.is_pinned));
            out.insert(
                "root_id".to_string(),
                serde_json::Value::String(post.root_id.clone()),
            );

            if let Some(props) = post.get_props() {
                if !props.is_empty() {
                    out.insert(
                        "props".to_string(),
                        serde_json::Value::Object(props.clone()),
                    );
                }
            }

            if let Some(metadata) = &post.metadata {
                if let Ok(value) = serde_json::to_value(metadata) {
                    out.insert("metadata".to_string(), value);
                }
            }
        }

        if let Some(reply_count) = self.reply_count {
            out.insert("reply_count".to_string(), serde_json::json!(reply_count));
        }

        if !self.edit_history_order.is_empty() {
            out.insert(
                "edit_history_order".to_string(),
                serde_json::Value::Array(
                    self.edit_history_order
                        .iter()
                        .map(|id| serde_json::Value::String(id.clone()))
                        .collect(),
                ),
            );
        }

        out
    }
}

/// Port of `model.FlaggedPostReportContentReview` (content_flagging_report.go:63) —
/// `content_review.yaml`.
///
/// The first six keys are always written; the seven reviewer/actor keys carry
/// `yaml:",omitempty"`, so a still-pending report contains only the reporter's half.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FlaggedPostReportContentReview {
    /// `yaml:"reporter_user_id"`.
    pub reporter_user_id: String,
    pub reporter_username: String,
    pub reporter_reason: String,
    pub reporter_comment: String,
    /// Epoch milliseconds.
    pub report_timestamp: i64,
    pub hidden: bool,

    /// `omitempty` from here down.
    pub reviewer_user_id: String,
    pub reviewer_username: String,
    pub reviewer_comment: String,
    pub action_time: i64,
    /// `keep` or `remove`.
    pub actor_decision: String,
    /// Note the casing: the Go field is `ActorUserId`, not `ActorUserID`, unlike the two above.
    pub actor_user_id: String,
    pub actor_username: String,
}

/// Port of `model.FlaggedPostReportMetadata` (content_flagging_report.go:79) —
/// `report_metadata.yaml`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FlaggedPostReportMetadata {
    pub generated_by_user_id: String,
    pub generated_by_username: String,
    /// Epoch milliseconds.
    pub timestamp: i64,
    /// [`FLAGGED_POST_REPORT_VERSION`].
    pub report_version: String,
}
