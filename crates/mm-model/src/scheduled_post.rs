//! Port of `server/public/model/scheduled_post.go` (204 lines) — **whole file except
//! `Auditable`** ([D-028]).
//!
//! A scheduled post is a [`Draft`] that has been given an id and a send time. Go expresses that
//! with an **anonymous struct field**, the first one in the ported tree, and it changes three
//! things at once.
//!
//! # 1. The wire form is one flat object, embedded half first
//!
//! Go inlines an embedded struct's keys into the parent, so the JSON is Draft's nine keys
//! followed by ScheduledPost's six. Field order is emission order, and
//! **`#[serde(flatten)]` does not reproduce it** — serde writes flattened fields *last*, and it
//! switches the whole struct to a map serializer. So [`Serialize`] is written by hand here and
//! only `Deserialize` is derived (where `flatten` is fine, because decoding is order-insensitive).
//!
//! `the_embedded_half_comes_first` guards the hazard directly: it asserts that a scheduled post's
//! JSON begins with its draft's JSON, so a field added to `Draft` and forgotten here fails a test
//! instead of silently vanishing from the wire.
//!
//! # 2. Draft's methods are promoted
//!
//! `s.Message`, `s.FileIds` and `s.GetProps()` all reach through the embed. [`Deref`] is the Rust
//! analogue and is why `self.message` works below. One consequence is worth stating because it
//! looks like a bug: `IsValid` calls `Draft::is_valid` **and** [`Self::base_is_valid`], which
//! calls `Draft::base_is_valid` — so the draft's base checks run **twice** per validation. The
//! answer is unchanged either way; the duplication is Go's.
//!
//! # 3. `repeat_timezone` is validated against the host filesystem
//!
//! `time.LoadLocation` reads `$ZONEINFO` and then the host's zoneinfo directory, so the accepted
//! set is a deployment artifact rather than a property of Go — the same shape of problem as
//! `mime.TypeByExtension` in [D-030]. The oracle proves it: on the macOS box that generated the
//! fixture, `america/new_york`, `AMERICA/NEW_YORK`, `utc` and `America//New_York` are all
//! **accepted**, because the filesystem is case-insensitive and normalises `//`. On a Linux
//! server they are rejected.
//!
//! This port uses `chrono_tz`, an embedded case-sensitive IANA table — which is what a Linux Go
//! server effectively answers. It agrees with the oracle on 44 of 50 probes; the six that differ
//! are exactly the host artifacts plus `""` and `"Local"`, both of which
//! [`Self::base_is_valid`] rejects *before* reaching the lookup. See [D-065].

use std::ops::{Deref, DerefMut};
use std::str::FromStr;

use serde::ser::SerializeStruct;
use serde::{Deserialize, Serialize, Serializer};

use crate::draft::Draft;
use crate::post::Post;
use crate::post_metadata::{PostMetadata, PostPriority};
use crate::utils::{AppError, AppResult, StringInterface, get_millis, go_format_v, new_id};

// --- constants ---------------------------------------------------------------------------

pub const SCHEDULED_POST_ERROR_UNKNOWN_ERROR: &str = "unknown";
pub const SCHEDULED_POST_ERROR_CODE_CHANNEL_ARCHIVED: &str = "channel_archived";
pub const SCHEDULED_POST_ERROR_CODE_RESTRICTED_DM: &str = "restricted_dm";
pub const SCHEDULED_POST_ERROR_CODE_CHANNEL_NOT_FOUND: &str = "channel_not_found";
pub const SCHEDULED_POST_ERROR_CODE_USER_DOES_NOT_EXIST: &str = "user_missing";
pub const SCHEDULED_POST_ERROR_CODE_USER_DELETED: &str = "user_deleted";
pub const SCHEDULED_POST_ERROR_CODE_NO_CHANNEL_PERMISSION: &str = "no_channel_permission";
pub const SCHEDULED_POST_ERROR_NO_CHANNEL_MEMBER: &str = "no_channel_member";
pub const SCHEDULED_POST_ERROR_THREAD_DELETED: &str = "thread_deleted";
pub const SCHEDULED_POST_ERROR_UNABLE_TO_SEND: &str = "unable_to_send";
pub const SCHEDULED_POST_ERROR_INVALID_POST: &str = "invalid_post";

/// Re-exported from [`crate::scheduled_post_recurrence`], their Go home — `IsValid`'s switch
/// needs both. There is one definition, not a copy, so the [D-005] borrow that used to sit here
/// is closed.
pub use crate::scheduled_post_recurrence::{
    SCHEDULED_POST_REPEAT_TYPE_NONE, SCHEDULED_POST_REPEAT_TYPE_WEEKLY,
};

/// Port of `model.scheduledPostMaxTimeGap` (scheduled_post.go:29). **Unexported in Go**, so the
/// oracle recovers it with `go/parser` rather than trusting this transcription — the technique
/// `version.go`'s release table needed ([D-021]).
///
/// Negative: a `scheduled_at` up to five seconds in the past is accepted, which Go's comment
/// attributes to slow client connections as much as to test determinism.
pub const SCHEDULED_POST_MAX_TIME_GAP: i64 = -5000;

// --- the wire type -----------------------------------------------------------------------

/// Port of `model.ScheduledPost` (scheduled_post.go:31).
///
/// `draft` stands in for Go's anonymous field. [`Deref`] reproduces the method and field
/// promotion; `Serialize` is hand-written so the embedded keys come first. See the module docs.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct ScheduledPost {
    /// Go's embedded `Draft`. Its keys are inlined into this object on the wire.
    #[serde(flatten)]
    pub draft: Draft,

    #[serde(rename = "id")]
    pub id: String,

    #[serde(rename = "scheduled_at")]
    pub scheduled_at: i64,

    #[serde(rename = "processed_at")]
    pub processed_at: i64,

    #[serde(rename = "error_code")]
    pub error_code: String,

    #[serde(rename = "repeat_type")]
    pub repeat_type: String,

    #[serde(rename = "repeat_timezone")]
    pub repeat_timezone: String,
}

impl Deref for ScheduledPost {
    type Target = Draft;
    fn deref(&self) -> &Self::Target {
        &self.draft
    }
}

impl DerefMut for ScheduledPost {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.draft
    }
}

/// Hand-written so the embedded fields are emitted **first** and in Draft's declaration order.
///
/// `#[serde(flatten)]` would compile and would put them last, which is a silent wire change.
/// The skip predicates are Draft's, restated: `props` has no `omitempty`, the other three do and
/// drop nil *and* empty. `the_embedded_half_comes_first` is the test that keeps this in step with
/// [`Draft`] if a field is ever added there.
impl Serialize for ScheduledPost {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let d = &self.draft;
        let file_ids = d.file_ids.as_ref().filter(|v| !v.is_empty());
        let priority = d.priority.as_ref().filter(|m| !m.is_empty());

        let len = 9 + usize::from(file_ids.is_some()) + usize::from(d.metadata.is_some());
        let len = len + usize::from(priority.is_some());

        let mut s = serializer.serialize_struct("ScheduledPost", len)?;
        s.serialize_field("create_at", &d.create_at)?;
        s.serialize_field("update_at", &d.update_at)?;
        s.serialize_field("delete_at", &d.delete_at)?;
        s.serialize_field("user_id", &d.user_id)?;
        s.serialize_field("channel_id", &d.channel_id)?;
        s.serialize_field("root_id", &d.root_id)?;
        s.serialize_field("message", &d.message)?;
        s.serialize_field("type", &d.draft_type)?;
        s.serialize_field("props", &d.props)?;
        if let Some(file_ids) = file_ids {
            s.serialize_field("file_ids", file_ids)?;
        }
        if let Some(metadata) = &d.metadata {
            s.serialize_field("metadata", metadata)?;
        }
        if let Some(priority) = priority {
            s.serialize_field("priority", priority)?;
        }

        s.serialize_field("id", &self.id)?;
        s.serialize_field("scheduled_at", &self.scheduled_at)?;
        s.serialize_field("processed_at", &self.processed_at)?;
        s.serialize_field("error_code", &self.error_code)?;
        s.serialize_field("repeat_type", &self.repeat_type)?;
        s.serialize_field("repeat_timezone", &self.repeat_timezone)?;
        s.end()
    }
}

// --- errors ------------------------------------------------------------------------------

/// The failure modes of `(*ScheduledPost).ToPost` (scheduled_post.go:116).
///
/// Go returns a bare `fmt.Errorf` for all three; the messages are reproduced verbatim, including
/// the `%v` rendering of the priority map, which sorts its keys and prints `map[k:v k2:v2]`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ToPostError {
    #[error("ScheduledPost.ToPost: priority is not a string. ScheduledPost.Priority: {0}")]
    PriorityNotAString(String),

    #[error("ScheduledPost.ToPost: requested_ack is not a bool. ScheduledPost.Priority: {0}")]
    RequestedAckNotABool(String),

    #[error(
        "ScheduledPost.ToPost: persistent_notifications is not a bool. ScheduledPost.Priority: {0}"
    )]
    PersistentNotificationsNotABool(String),
}

fn error(field: &str, details: String) -> Box<AppError> {
    Box::new(AppError::new(
        "ScheduledPost.IsValid",
        format!("model.scheduled_post.is_valid.{field}.app_error"),
        None,
        details,
        400,
    ))
}

/// Go's `fmt.Sprintf("%v", priority)` without cloning the map into a [`serde_json::Value`] just to
/// format it. Identical to [`go_format_v`]'s object arm — `StringInterface` is a `serde_json::Map`,
/// which is a `BTreeMap`, so iteration is already in Go's sorted-key order.
fn go_format_priority(priority: &StringInterface) -> String {
    let parts: Vec<String> = priority
        .iter()
        .map(|(k, v)| format!("{k}:{}", go_format_v(v)))
        .collect();
    format!("map[{}]", parts.join(" "))
}

impl ScheduledPost {
    /// Port of `(*ScheduledPost).IsValid` (scheduled_post.go:41).
    ///
    /// Runs the whole of `Draft::is_valid` — message length **and** the draft base checks — and
    /// then [`Self::base_is_valid`], which runs the draft base checks again. Go's duplication,
    /// reproduced; it cannot change an answer.
    pub fn is_valid(&self, max_message_size: i64) -> AppResult {
        self.draft.is_valid(max_message_size)?;
        self.base_is_valid()
    }

    /// Port of `(*ScheduledPost).BaseIsValid` (scheduled_post.go:50).
    pub fn base_is_valid(&self) -> AppResult {
        self.draft.base_is_valid()?;

        let id_detail = || format!("id={}", self.id);

        // Emptiness only — the id is never run through `is_valid_id`, unlike the draft's three.
        if self.id.is_empty() {
            return Err(error("id", id_detail()));
        }

        // A post needs *something* to send. Either a message or a file is enough, and a
        // whitespace-only message counts as a message.
        if self.message.is_empty() && self.file_ids.as_deref().unwrap_or_default().is_empty() {
            return Err(error("empty_post", id_detail()));
        }

        // The gap is negative, so this accepts a scheduled_at up to five seconds in the past.
        if (self.scheduled_at - get_millis()) < SCHEDULED_POST_MAX_TIME_GAP {
            return Err(error("scheduled_at", id_detail()));
        }

        if self.processed_at < 0 {
            return Err(error("processed_at", id_detail()));
        }

        // Exactly two accepted values, one of which is the empty string.
        if self.repeat_type != SCHEDULED_POST_REPEAT_TYPE_NONE
            && self.repeat_type != SCHEDULED_POST_REPEAT_TYPE_WEEKLY
        {
            return Err(error(
                "repeat_type",
                format!("id={}, repeat_type={}", self.id, self.repeat_type),
            ));
        }

        if self.repeat_type == SCHEDULED_POST_REPEAT_TYPE_WEEKLY {
            // Files bind to the first post they are attached to, so later occurrences would
            // silently send without them.
            if !self.file_ids.as_deref().unwrap_or_default().is_empty() {
                return Err(error("repeat_files", id_detail()));
            }

            if self.repeat_timezone.is_empty() {
                return Err(error("repeat_timezone", id_detail()));
            }

            // Go's `time.LoadLocation("Local")` succeeds and yields the *server's* zone, which a
            // persisted recurring schedule must not depend on. Rejected explicitly, before the
            // lookup — which is why `chrono_tz` not knowing `Local` cannot matter. See [D-065].
            if self.repeat_timezone == "Local" {
                return Err(error(
                    "repeat_timezone_invalid",
                    format!("id={}, repeat_timezone={}", self.id, self.repeat_timezone),
                ));
            }

            if chrono_tz::Tz::from_str(&self.repeat_timezone).is_err() {
                // Go appends `time.LoadLocation`'s own error text, which for an unknown name is
                // `unknown time zone <name>` and for a path-shaped one is `time: invalid location
                // name` or an OS error. Only the first is reproducible off Go's filesystem;
                // see [D-065].
                return Err(error(
                    "repeat_timezone_invalid",
                    format!(
                        "id={}, repeat_timezone={}, unknown time zone {}",
                        self.id, self.repeat_timezone, self.repeat_timezone
                    ),
                ));
            }
        }

        Ok(())
    }

    /// Port of `(*ScheduledPost).PreSave` (scheduled_post.go:99).
    ///
    /// Mints an id only when there is none, then **clears `processed_at` and `error_code`** — a
    /// re-saved post is un-processed and un-errored — and hands off to `Draft::pre_save`, which
    /// zeroes `delete_at` and materialises the collections.
    pub fn pre_save(&mut self) {
        if self.id.is_empty() {
            self.id = new_id();
        }

        self.processed_at = 0;
        self.error_code = String::new();

        self.draft.pre_save();
    }

    /// Port of `(*ScheduledPost).PreUpdate` (scheduled_post.go:110).
    ///
    /// **Not** `Draft::pre_save`: it sets `update_at` itself and calls `Draft::pre_commit`, so
    /// `create_at`, `delete_at`, `processed_at` and `error_code` all survive an update where
    /// [`Self::pre_save`] would have reset the last three.
    pub fn pre_update(&mut self) {
        self.draft.update_at = get_millis();
        self.draft.pre_commit();
    }

    /// Port of `(*ScheduledPost).ToPost` (scheduled_post.go:116).
    ///
    /// Carries seven fields and **not** `id`, `create_at`, `update_at`, `delete_at`,
    /// `scheduled_at`, `processed_at` or `error_code` — the new post is a fresh row.
    ///
    /// The priority conversion is all-or-nothing: all three keys must be present with the right
    /// types, because Go's type assertion on an absent key yields the zero value with `ok=false`.
    /// So `{"priority": "urgent"}` alone is an error, not a partial priority. An **empty** map is
    /// skipped entirely and is not an error.
    pub fn to_post(&self) -> Result<Post, ToPostError> {
        let mut post = Post {
            user_id: self.user_id.clone(),
            channel_id: self.channel_id.clone(),
            message: self.message.clone(),
            // Go assigns the slice and the pointer, so its Post *aliases* the scheduled post's
            // files and metadata. Ours owns them — same class as [D-015]; see [D-066].
            file_ids: self.file_ids.clone(),
            root_id: self.root_id.clone(),
            metadata: self.metadata.clone(),
            post_type: self.draft_type.clone(),
            ..Post::default()
        };

        // Go ranges `s.GetProps()` and calls `AddProp` per key, so an empty or nil props map
        // leaves `post.Props` **nil** rather than allocating an empty one.
        for (key, value) in self.get_props().into_iter().flatten() {
            post.add_prop(key.clone(), value.clone());
        }

        let Some(priority) = self.priority.as_ref().filter(|m| !m.is_empty()) else {
            return Ok(post);
        };

        let Some(priority_value) = priority.get("priority").and_then(|v| v.as_str()) else {
            return Err(ToPostError::PriorityNotAString(go_format_priority(
                priority,
            )));
        };
        let Some(requested_ack) = priority.get("requested_ack").and_then(|v| v.as_bool()) else {
            return Err(ToPostError::RequestedAckNotABool(go_format_priority(
                priority,
            )));
        };
        let Some(persistent) = priority
            .get("persistent_notifications")
            .and_then(|v| v.as_bool())
        else {
            return Err(ToPostError::PersistentNotificationsNotABool(
                go_format_priority(priority),
            ));
        };

        let metadata = post.metadata.get_or_insert_with(PostMetadata::default);
        metadata.priority = Some(PostPriority {
            priority: Some(priority_value.to_string()),
            requested_ack: Some(requested_ack),
            persistent_notifications: Some(persistent),
            ..PostPriority::default()
        });

        Ok(post)
    }

    /// Port of `(*ScheduledPost).RestoreNonUpdatableFields` (scheduled_post.go:182).
    ///
    /// Six fields, and `update_at` is **not** among them — nor is `message`, `props`, `file_ids`,
    /// `scheduled_at` or anything else a client is allowed to change.
    pub fn restore_non_updatable_fields(&mut self, original: &ScheduledPost) {
        self.id = original.id.clone();
        self.draft.create_at = original.create_at;
        self.draft.user_id = original.user_id.clone();
        self.draft.channel_id = original.channel_id.clone();
        self.draft.root_id = original.root_id.clone();
        self.draft.draft_type = original.draft_type.clone();
    }

    /// Port of `(*ScheduledPost).SanitizeInput` (scheduled_post.go:191).
    ///
    /// Zeroes `create_at` so the server assigns it, and drops the metadata embeds — which the
    /// server regenerates and a client must not be able to forge. Note it clears `embeds` on an
    /// **existing** metadata and never allocates one.
    pub fn sanitize_input(&mut self) {
        self.draft.create_at = 0;

        if let Some(metadata) = &mut self.draft.metadata {
            metadata.embeds = Vec::new();
        }
    }

    /// Port of `(*ScheduledPost).GetPriority` (scheduled_post.go:199).
    ///
    /// Reads `metadata.priority` — the typed [`PostPriority`] — **not** the draft's own untyped
    /// `priority` map, which is what a client sends. The two are different fields and only
    /// [`Self::to_post`] connects them.
    pub fn get_priority(&self) -> Option<&PostPriority> {
        self.metadata.as_ref()?.priority.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::go_json_marshal;

    fn valid() -> ScheduledPost {
        let mut s = ScheduledPost {
            id: "6bdz674pgq767e4jx75w4pf57a".to_string(),
            scheduled_at: get_millis() + 3_600_000,
            ..ScheduledPost::default()
        };
        s.draft.create_at = 1700000000000;
        s.draft.update_at = 1700000001000;
        s.draft.user_id = "6bdz674pgq767e4jx75w4pf57a".to_string();
        s.draft.channel_id = "qr6kf7ztp7yifxt4wm5xn51bke".to_string();
        s.draft.message = "hello".to_string();
        s
    }

    /// The hazard the hand-written `Serialize` exists for. If a field is added to [`Draft`] and
    /// not to that impl, this fails — which is the only thing standing between the embed and a
    /// silent wire change.
    #[test]
    fn the_embedded_half_comes_first() {
        let s = valid();
        let whole = go_json_marshal(&s).unwrap();
        let draft = go_json_marshal(&s.draft).unwrap();

        // The draft's object, minus its closing brace, must be a prefix of the whole.
        let prefix = &draft[..draft.len() - 1];
        assert!(
            whole.starts_with(prefix),
            "draft keys are not the leading run:\n  whole = {whole}\n  draft = {draft}"
        );
        assert!(whole[prefix.len()..].starts_with(",\"id\":"));
    }

    #[test]
    fn deref_reaches_the_draft() {
        let s = valid();
        assert_eq!(s.message, "hello");
        assert_eq!(s.create_at, 1700000000000);
    }

    #[test]
    fn a_post_needs_a_message_or_a_file() {
        let mut s = valid();
        s.draft.message = String::new();
        assert_eq!(
            s.base_is_valid().unwrap_err().id,
            "model.scheduled_post.is_valid.empty_post.app_error"
        );

        s.draft.file_ids = Some(vec!["f1".to_string()]);
        assert!(s.base_is_valid().is_ok());

        // Whitespace is a message.
        s.draft.file_ids = None;
        s.draft.message = " ".to_string();
        assert!(s.base_is_valid().is_ok());
    }

    #[test]
    fn the_five_second_grace_window_is_inclusive_of_the_recent_past() {
        let mut s = valid();
        s.scheduled_at = get_millis() - 1000;
        assert!(s.base_is_valid().is_ok());

        s.scheduled_at = get_millis() - 60_000;
        assert_eq!(
            s.base_is_valid().unwrap_err().id,
            "model.scheduled_post.is_valid.scheduled_at.app_error"
        );
    }

    #[test]
    fn pre_save_clears_the_processing_state_and_pre_update_does_not() {
        let mut s = valid();
        s.processed_at = 99;
        s.error_code = SCHEDULED_POST_ERROR_UNKNOWN_ERROR.to_string();
        s.pre_save();
        assert_eq!(s.processed_at, 0);
        assert!(s.error_code.is_empty());

        let mut s = valid();
        s.processed_at = 99;
        s.error_code = SCHEDULED_POST_ERROR_UNKNOWN_ERROR.to_string();
        s.pre_update();
        assert_eq!(s.processed_at, 99);
        assert_eq!(s.error_code, SCHEDULED_POST_ERROR_UNKNOWN_ERROR);
    }

    #[test]
    fn get_priority_reads_the_metadata_not_the_draft_field() {
        let mut s = valid();
        s.draft.priority = Some(
            [("priority".to_string(), serde_json::json!("urgent"))]
                .into_iter()
                .collect(),
        );
        assert!(s.get_priority().is_none());
    }
}

/// Parity tests driven by `fixtures/behaviour_scheduled_post.json` — Go's own answers.
#[cfg(test)]
mod go_parity {
    use super::*;
    use crate::utils::go_json_marshal;
    use serde_json::Value;

    fn oracle() -> Value {
        serde_json::from_str(include_str!(
            "../../../fixtures/behaviour_scheduled_post.json"
        ))
        .unwrap()
    }

    /// The one document Go accepts and we reject: `null` into a scalar. See [D-057].
    const NULL_SCALAR_ONLY: &str = "null_scalars";

    #[test]
    fn the_constants_match_go() {
        let oracle = oracle();
        let c = &oracle["constants"];

        for (key, ours) in [
            (
                "ScheduledPostErrorUnknownError",
                SCHEDULED_POST_ERROR_UNKNOWN_ERROR,
            ),
            (
                "ScheduledPostErrorCodeChannelArchived",
                SCHEDULED_POST_ERROR_CODE_CHANNEL_ARCHIVED,
            ),
            (
                "ScheduledPostErrorCodeRestrictedDM",
                SCHEDULED_POST_ERROR_CODE_RESTRICTED_DM,
            ),
            (
                "ScheduledPostErrorCodeChannelNotFound",
                SCHEDULED_POST_ERROR_CODE_CHANNEL_NOT_FOUND,
            ),
            (
                "ScheduledPostErrorCodeUserDoesNotExist",
                SCHEDULED_POST_ERROR_CODE_USER_DOES_NOT_EXIST,
            ),
            (
                "ScheduledPostErrorCodeUserDeleted",
                SCHEDULED_POST_ERROR_CODE_USER_DELETED,
            ),
            (
                "ScheduledPostErrorCodeNoChannelPermission",
                SCHEDULED_POST_ERROR_CODE_NO_CHANNEL_PERMISSION,
            ),
            (
                "ScheduledPostErrorNoChannelMember",
                SCHEDULED_POST_ERROR_NO_CHANNEL_MEMBER,
            ),
            (
                "ScheduledPostErrorThreadDeleted",
                SCHEDULED_POST_ERROR_THREAD_DELETED,
            ),
            (
                "ScheduledPostErrorUnableToSend",
                SCHEDULED_POST_ERROR_UNABLE_TO_SEND,
            ),
            (
                "ScheduledPostErrorInvalidPost",
                SCHEDULED_POST_ERROR_INVALID_POST,
            ),
            (
                "ScheduledPostRepeatTypeNone",
                SCHEDULED_POST_REPEAT_TYPE_NONE,
            ),
            (
                "ScheduledPostRepeatTypeWeekly",
                SCHEDULED_POST_REPEAT_TYPE_WEEKLY,
            ),
        ] {
            assert_eq!(c[key].as_str().unwrap(), ours, "{key}");
        }

        // Unexported in Go, read out of the source by the oracle.
        assert_eq!(
            c["scheduledPostMaxTimeGap"].as_i64().unwrap(),
            SCHEDULED_POST_MAX_TIME_GAP
        );
    }

    #[test]
    fn the_wire_format_matches_go() {
        let oracle = oracle();
        let cases = oracle["wire"].as_array().unwrap();
        assert!(!cases.is_empty());

        let mut checked = 0;
        for case in cases {
            let name = case["name"].as_str().unwrap();
            assert!(!case["panicked"].as_bool().unwrap(), "{name}: Go panicked");
            assert!(case["err"].is_null(), "{name}: Go failed to decode");

            if name == NULL_SCALAR_ONLY {
                continue;
            }

            let decoded: ScheduledPost = match case["in"].as_str().unwrap() {
                "" => ScheduledPost::default(),
                doc => serde_json::from_str(doc).unwrap_or_else(|e| panic!("{name}: {e}")),
            };

            // Byte-for-byte: this is what pins the embedded half coming first.
            assert_eq!(
                go_json_marshal(&decoded).unwrap(),
                case["out"].as_str().unwrap(),
                "{name}"
            );
            assert_eq!(
                decoded.props.is_none(),
                case["props_nil"].as_bool().unwrap(),
                "{name}: props nil"
            );
            assert_eq!(
                decoded.file_ids.is_none(),
                case["file_ids_nil"].as_bool().unwrap(),
                "{name}: file_ids nil"
            );
            assert_eq!(
                decoded.priority.is_none(),
                case["priority_nil"].as_bool().unwrap(),
                "{name}: priority nil"
            );
            assert_eq!(
                decoded.metadata.is_none(),
                case["metadata_nil"].as_bool().unwrap(),
                "{name}: metadata nil"
            );
            checked += 1;
        }
        assert_eq!(checked, cases.len() - 1, "every case but the null one");
    }

    /// Rebuilds a validation case, restoring the clock-relative `scheduled_at` the fixture
    /// records as an offset ([D-032]).
    fn valid_case_post(case: &Value) -> ScheduledPost {
        let mut post: ScheduledPost = serde_json::from_value(case["post"].clone()).unwrap();
        post.scheduled_at = get_millis() + case["scheduled_at_offset"].as_i64().unwrap();
        post
    }

    #[test]
    fn is_valid_matches_go() {
        let oracle = oracle();
        let cases = oracle["is_valid"].as_array().unwrap();
        assert!(cases.len() > 25, "corpus shrank: {}", cases.len());

        for case in cases {
            let name = case["name"].as_str().unwrap();
            let post = valid_case_post(case);
            let max = case["max_message_size"].as_i64().unwrap();

            match post.is_valid(max) {
                Ok(()) => assert_eq!(case["error_id"].as_str().unwrap(), "", "{name}: Go errored"),
                Err(err) => {
                    assert_eq!(err.id, case["error_id"].as_str().unwrap(), "{name}: id");
                    assert_eq!(
                        err.detailed_error,
                        case["detailed"].as_str().unwrap(),
                        "{name}: detail"
                    );
                    assert_eq!(err.where_, case["where"].as_str().unwrap(), "{name}: where");
                    assert_eq!(
                        i64::from(err.status_code),
                        case["status"].as_i64().unwrap(),
                        "{name}: status"
                    );
                }
            }
        }
    }

    #[test]
    fn base_is_valid_matches_go() {
        let oracle = oracle();
        for case in oracle["is_valid"].as_array().unwrap() {
            let name = case["name"].as_str().unwrap();
            let post = valid_case_post(case);

            match post.base_is_valid() {
                Ok(()) => assert_eq!(
                    case["base_error_id"].as_str().unwrap(),
                    "",
                    "{name}: Go errored"
                ),
                Err(err) => {
                    assert_eq!(
                        err.id,
                        case["base_error_id"].as_str().unwrap(),
                        "{name}: id"
                    );
                    assert_eq!(
                        err.detailed_error,
                        case["base_detailed"].as_str().unwrap(),
                        "{name}: detail"
                    );
                    assert_eq!(
                        err.where_,
                        case["base_where"].as_str().unwrap(),
                        "{name}: where"
                    );
                }
            }
        }
    }

    /// `time.LoadLocation` is a filesystem lookup, so Go's answers are the *generating host's*.
    /// The names that differ are listed explicitly rather than skipped by a predicate — each one
    /// is a claim about why it differs, and a new disagreement fails the test.
    ///
    /// See [D-065]. `""` and `"Local"` are Go special cases that `base_is_valid` rejects before
    /// the lookup runs, so `chrono_tz` not knowing them is unobservable.
    #[test]
    fn the_timezone_table_agrees_with_go_except_on_host_artifacts() {
        const HOST_ARTIFACTS: [&str; 6] = [
            "",                  // Go: LoadLocation("") is UTC. Rejected earlier by base_is_valid.
            "Local",             // Go: the server's own zone. Rejected earlier by base_is_valid.
            "america/new_york",  // accepted only on a case-insensitive filesystem
            "AMERICA/NEW_YORK",  // ditto
            "utc",               // ditto
            "America//New_York", // accepted only where the OS collapses `//` in a path
        ];

        let oracle = oracle();
        let rows = oracle["timezones"].as_array().unwrap();
        assert!(rows.len() > 40, "corpus shrank: {}", rows.len());

        let mut agreed = 0;
        let mut differed = Vec::new();
        for row in rows {
            let name = row["name"].as_str().unwrap();
            let go_ok = row["ok"].as_bool().unwrap();
            let ours = chrono_tz::Tz::from_str(name).is_ok();

            if go_ok == ours {
                agreed += 1;
                assert!(
                    !HOST_ARTIFACTS.contains(&name),
                    "{name}: listed as a host artifact but the two agree"
                );
            } else {
                differed.push(name);
            }
        }

        assert_eq!(differed, HOST_ARTIFACTS, "the disagreements moved");
        assert!(agreed >= 44, "only {agreed} names agreed");
    }

    fn assert_hook(
        case: &Value,
        post: &ScheduledPost,
        in_id: &str,
        in_create: i64,
        in_update: i64,
    ) {
        let name = case["name"].as_str().unwrap();
        let minted = post.id != in_id;
        assert_eq!(
            minted,
            case["id_was_minted"].as_bool().unwrap(),
            "{name}: id minted"
        );
        if !minted {
            assert_eq!(post.id, case["id_out"].as_str().unwrap(), "{name}: id");
        } else {
            assert_eq!(post.id.len(), 26, "{name}: minted id length");
        }

        assert_eq!(
            post.processed_at,
            case["processed_at_out"].as_i64().unwrap(),
            "{name}: processed_at"
        );
        assert_eq!(
            post.error_code,
            case["error_code_out"].as_str().unwrap(),
            "{name}: error_code"
        );
        assert_eq!(
            post.delete_at,
            case["delete_at_out"].as_i64().unwrap(),
            "{name}: delete_at"
        );
        assert_eq!(
            post.create_at == in_create,
            case["create_at_was_kept"].as_bool().unwrap(),
            "{name}: create_at kept"
        );
        assert_eq!(
            post.update_at != in_update,
            case["update_at_moved"].as_bool().unwrap(),
            "{name}: update_at moved"
        );
        assert_eq!(
            post.update_at == post.create_at,
            case["update_at_equals_create_at"].as_bool().unwrap(),
            "{name}: update_at == create_at"
        );
        assert_eq!(
            post.props.is_none(),
            case["props_nil_out"].as_bool().unwrap(),
            "{name}: props nil"
        );
        assert_eq!(
            serde_json::to_value(&post.draft.props).unwrap(),
            case["props_out"],
            "{name}: props"
        );
        assert_eq!(
            serde_json::to_value(&post.draft.file_ids).unwrap(),
            case["file_ids_out"],
            "{name}: file_ids"
        );
    }

    #[test]
    fn pre_save_matches_go() {
        let oracle = oracle();
        let cases = oracle["pre_save"].as_array().unwrap();
        assert!(!cases.is_empty());

        for case in cases {
            let mut post: ScheduledPost = serde_json::from_value(case["in"].clone()).unwrap();
            let (id, create, update) = (post.id.clone(), post.create_at, post.update_at);
            post.pre_save();
            assert_hook(case, &post, &id, create, update);
        }
    }

    #[test]
    fn pre_update_matches_go() {
        let oracle = oracle();
        let cases = oracle["pre_update"].as_array().unwrap();
        assert!(!cases.is_empty());

        for case in cases {
            let mut post: ScheduledPost = serde_json::from_value(case["in"].clone()).unwrap();
            let (id, create, update) = (post.id.clone(), post.create_at, post.update_at);
            post.pre_update();
            assert_hook(case, &post, &id, create, update);
        }
    }

    #[test]
    fn to_post_matches_go() {
        let oracle = oracle();
        let cases = oracle["to_post"].as_array().unwrap();
        assert!(cases.len() > 10, "corpus shrank: {}", cases.len());

        let mut errors = 0;
        for case in cases {
            let name = case["name"].as_str().unwrap();
            let scheduled: ScheduledPost = serde_json::from_value(case["in"].clone()).unwrap();

            match scheduled.to_post() {
                Ok(post) => {
                    assert_eq!(case["err"].as_str().unwrap(), "", "{name}: Go errored");
                    assert_eq!(
                        go_json_marshal(&post).unwrap(),
                        case["post"].as_str().unwrap(),
                        "{name}"
                    );
                }
                Err(err) => {
                    assert_eq!(err.to_string(), case["err"].as_str().unwrap(), "{name}");
                    assert_eq!(
                        case["post"].as_str().unwrap(),
                        "",
                        "{name}: Go returned a post"
                    );
                    errors += 1;
                }
            }
        }
        assert_eq!(errors, 4, "the priority error cases went missing");
    }

    #[test]
    fn restore_non_updatable_fields_matches_go() {
        let oracle = oracle();
        let cases = oracle["restore_non_updatable_fields"].as_array().unwrap();
        assert!(!cases.is_empty());

        for case in cases {
            let name = case["name"].as_str().unwrap();
            let mut target: ScheduledPost =
                serde_json::from_str(case["target"].as_str().unwrap()).unwrap();
            let original: ScheduledPost =
                serde_json::from_str(case["original"].as_str().unwrap()).unwrap();

            target.restore_non_updatable_fields(&original);
            assert_eq!(
                go_json_marshal(&target).unwrap(),
                case["out"].as_str().unwrap(),
                "{name}"
            );
        }
    }

    #[test]
    fn sanitize_input_matches_go() {
        let oracle = oracle();
        let cases = oracle["sanitize_input"].as_array().unwrap();
        assert!(!cases.is_empty());

        for case in cases {
            let name = case["name"].as_str().unwrap();
            let mut post: ScheduledPost =
                serde_json::from_str(case["in"].as_str().unwrap()).unwrap();
            post.sanitize_input();

            assert_eq!(
                go_json_marshal(&post).unwrap(),
                case["out"].as_str().unwrap(),
                "{name}"
            );
            assert_eq!(
                post.metadata.is_none(),
                case["metadata_nil"].as_bool().unwrap(),
                "{name}: metadata nil"
            );
            if let Some(metadata) = &post.metadata {
                // Go sets Embeds to nil; ours is an empty Vec, which `omitempty` drops alike.
                assert!(case["embeds_nil"].as_bool().unwrap(), "{name}");
                assert!(metadata.embeds.is_empty(), "{name}: embeds");
            }
        }
    }

    #[test]
    fn get_priority_matches_go() {
        let oracle = oracle();
        let cases = oracle["get_priority"].as_array().unwrap();
        assert!(!cases.is_empty());

        for case in cases {
            let name = case["name"].as_str().unwrap();
            let post: ScheduledPost = serde_json::from_str(case["in"].as_str().unwrap()).unwrap();

            match post.get_priority() {
                None => assert!(case["nil"].as_bool().unwrap(), "{name}: Go found one"),
                Some(priority) => {
                    assert!(!case["nil"].as_bool().unwrap(), "{name}: Go found none");
                    assert_eq!(
                        go_json_marshal(priority).unwrap(),
                        case["out"].as_str().unwrap(),
                        "{name}"
                    );
                }
            }
        }
    }
}

/// Serialization parity against `fixtures/scheduled_post.json` — every field non-zero.
#[cfg(test)]
mod fixture {
    use super::*;

    #[test]
    fn round_trips_the_generated_fixture() {
        let raw = include_str!("../../../fixtures/scheduled_post.json");
        let decoded: ScheduledPost = serde_json::from_str(raw).unwrap();

        // Both halves must be populated, or the embed proves nothing.
        assert!(!decoded.id.is_empty() && decoded.scheduled_at != 0 && decoded.processed_at != 0);
        assert!(!decoded.repeat_type.is_empty() && !decoded.repeat_timezone.is_empty());
        assert!(!decoded.user_id.is_empty() && !decoded.message.is_empty());
        assert!(decoded.metadata.is_some() && decoded.props.is_some());

        let ours: serde_json::Value = serde_json::to_value(&decoded).unwrap();
        let theirs: serde_json::Value = serde_json::from_str(raw).unwrap();
        assert_eq!(ours, theirs);
    }
}
