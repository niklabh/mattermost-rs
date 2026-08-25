//! Port of `model/thread.go` — the metadata for a root post and its replies.
//!
//! **Thread metadata does not exist until the first reply.** A root post with no replies has no
//! `Threads` row at all, which is why every count here is safe to treat as ≥ 1 when present.
//!
//! # Two columns are named differently in the database
//!
//! `DeleteAt` is stored as **`ThreadDeleteAt`** and `TeamId` as **`ThreadTeamId`**, both
//! denormalised copies. Go's comment explains why: the plain names would collide with queries
//! from older server versions running against the same database — exactly the Strangler Fig
//! situation this project is in, so the aliases are load-bearing here too.

use serde::{Deserialize, Serialize};

use crate::post::Post;
use crate::user::User;
use crate::utils::{AppError, AppResult, StringArray, etag, is_valid_id};

/// Port of `model.Thread` (thread.go:11).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Thread {
    /// The root post. Tagged **`id`**, not `post_id` — unlike [`ThreadMembership::post_id`].
    #[serde(rename = "id")]
    pub post_id: String,

    #[serde(rename = "channel_id")]
    pub channel_id: String,

    /// Excludes deleted posts.
    #[serde(rename = "reply_count")]
    pub reply_count: i64,

    /// Epoch milliseconds.
    #[serde(rename = "last_reply_at")]
    pub last_reply_at: i64,

    /// Oldest to newest. **The root author is not in this list until they reply.**
    #[serde(rename = "participants")]
    pub participants: Option<StringArray>,

    /// Denormalised from the root post. Column `ThreadDeleteAt`.
    #[serde(rename = "delete_at")]
    pub delete_at: i64,

    /// Denormalised from the channel. Column `ThreadTeamId`.
    #[serde(rename = "team_id")]
    pub team_id: String,
}

impl Thread {
    /// Port of `(*Thread).Etag` (thread.go:93) — the root id and the last reply time.
    pub fn etag(&self) -> String {
        etag(&[&self.post_id, &self.last_reply_at])
    }
}

/// Port of `model.ThreadResponse` (thread.go:44) — a thread as sent to a client.
///
/// A different shape from [`Thread`]: it carries the resolved participants and root post, plus
/// the caller's own unread counts.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ThreadResponse {
    #[serde(rename = "id")]
    pub post_id: String,

    #[serde(rename = "reply_count")]
    pub reply_count: i64,

    #[serde(rename = "last_reply_at")]
    pub last_reply_at: i64,

    #[serde(rename = "last_viewed_at")]
    pub last_viewed_at: i64,

    #[serde(rename = "participants")]
    pub participants: Option<Vec<User>>,

    #[serde(rename = "post")]
    pub post: Option<Box<Post>>,

    #[serde(rename = "unread_replies")]
    pub unread_replies: i64,

    #[serde(rename = "unread_mentions")]
    pub unread_mentions: i64,

    #[serde(rename = "is_urgent")]
    pub is_urgent: bool,

    #[serde(rename = "delete_at")]
    pub delete_at: i64,
}

/// Port of `model.Threads` (thread.go:57) — a page of threads plus the caller's totals.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Threads {
    #[serde(rename = "total")]
    pub total: i64,

    #[serde(rename = "total_unread_threads")]
    pub total_unread_threads: i64,

    #[serde(rename = "total_unread_mentions")]
    pub total_unread_mentions: i64,

    #[serde(rename = "total_unread_urgent_mentions")]
    pub total_unread_urgent_mentions: i64,

    #[serde(rename = "threads")]
    pub threads: Option<Vec<ThreadResponse>>,
}

/// Port of `model.GetUserThreadsOpts` (thread.go:65). No `json:` tags — assembled from query
/// parameters.
///
/// `PageSize` and `Since` are **`uint64`** in Go, the only unsigned fields in the package; they
/// stay unsigned here so a negative page size is unrepresentable rather than silently huge.
///
/// `TotalsOnly` and `ThreadsOnly` are opposite shortcuts and Go does not reject setting both.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GetUserThreadsOpts {
    /// Default 30 — applied by the caller, not here.
    pub page_size: u64,
    /// Enrich the response with participant details.
    pub extended: bool,
    /// Include deleted threads, for mobile sync.
    pub deleted: bool,
    /// Filters on `LastUpdateAt`.
    pub since: u64,
    /// A thread id cursor; returns the page **before** it.
    pub before: String,
    /// A thread id cursor; returns the page **after** it.
    pub after: String,
    pub unread: bool,
    /// Fetch no threads, only the counts.
    pub totals_only: bool,
    /// Fetch threads and return **zero** totals.
    pub threads_only: bool,
    /// Restrict to one team, excluding DMs and GMs.
    pub team_only: bool,
    pub include_is_urgent: bool,
    pub exclude_direct: bool,
}

/// Port of `model.ThreadMembership` (thread.go:100) — the thread analogue of a channel
/// membership.
///
/// **The three timestamp-ish tags do not match their field names**: `LastUpdated` is
/// `last_update_at` and `LastViewed` is `last_view_at` — singular `update`/`view`, unlike
/// `ChannelMember.last_viewed_at`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ThreadMembership {
    #[serde(rename = "post_id")]
    pub post_id: String,

    #[serde(rename = "user_id")]
    pub user_id: String,

    /// Defaults to true when the record is created — a record does not exist until the user first
    /// follows the thread — but the user may stop and resume at will.
    #[serde(rename = "following")]
    pub following: bool,

    /// Creation or last change. Used to constrain queries on websocket reconnect **and** as the
    /// retention policy's time column.
    #[serde(rename = "last_update_at")]
    pub last_updated: i64,

    /// Where the user should start reading.
    #[serde(rename = "last_view_at")]
    pub last_viewed: i64,

    #[serde(rename = "unread_mentions")]
    pub unread_mentions: i64,
}

impl ThreadMembership {
    /// Port of `(*ThreadMembership).IsValid` (thread.go:134). Two branches; neither timestamp is
    /// checked. The error ids say **`model.thread.`**, not `model.thread_membership.`.
    pub fn is_valid(&self) -> AppResult {
        if !is_valid_id(&self.post_id) {
            return Err(err("post_id"));
        }

        if !is_valid_id(&self.user_id) {
            return Err(err("user_id"));
        }

        Ok(())
    }
}

fn err(field: &str) -> Box<AppError> {
    Box::new(AppError::new(
        "ThreadMembership.IsValid",
        format!("model.thread.is_valid.{field}.app_error"),
        None,
        "",
        400,
    ))
}

/// Port of `model.ThreadMembershipForExport` (thread.go:146).
///
/// `Username` is tagged **`user_name`** with an underscore, and `LastViewed` here is
/// `last_viewed` — a *third* spelling of the same concept.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ThreadMembershipForExport {
    #[serde(rename = "user_name")]
    pub username: String,

    #[serde(rename = "last_viewed")]
    pub last_viewed: i64,

    #[serde(rename = "unread_mentions")]
    pub unread_mentions: i64,
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
    fn thread_round_trips_the_fixture() {
        assert_fixture_round_trips!(Thread, "thread");
    }
    #[test]
    fn thread_response_round_trips_the_fixture() {
        assert_fixture_round_trips!(ThreadResponse, "thread_response");
    }
    #[test]
    fn threads_round_trips_the_fixture() {
        assert_fixture_round_trips!(Threads, "threads");
    }
    #[test]
    fn thread_membership_round_trips_the_fixture() {
        assert_fixture_round_trips!(ThreadMembership, "thread_membership");
    }
    #[test]
    fn thread_membership_for_export_round_trips_the_fixture() {
        assert_fixture_round_trips!(ThreadMembershipForExport, "thread_membership_for_export");
    }
}
