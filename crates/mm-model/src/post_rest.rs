//! The wire types of `api4/post.go`'s two remaining tails — the posts-for-reporting query
//! (`model.ReportPost*`, post.go:1458-1650) and the AI rewrite request (`model.Rewrite*`,
//! post.go:1478-1490). Both are declared in `model/post.go`; they live apart from `post.rs`
//! because that file is the `Post` struct's and this family is the only caller.
//!
//! # The cursor is opaque to the client and not to us
//!
//! `EncodeReportPostCursor` is `1:channel:time_field:include_deleted:exclude_system_posts:sort:
//! timestamp:post_id`, base64 with the **URL** alphabet and padding. A cursor carries every
//! query-affecting parameter, and when one is present the body's own `time_field`,
//! `sort_direction`, `include_deleted` and `exclude_system_posts` are **ignored** — only
//! `per_page` and `include_metadata` are read off the body on a later page. `channel_id` from
//! the body is read once more, after the query: see `getPostsForReporting`'s empty-page check.

use serde::{Deserialize, Serialize};

use crate::post::Post;
use crate::utils::{AppError, is_valid_id, parse_go_bool};

/// `model.MaxReportingPerPage` (post.go:80).
pub const MAX_REPORTING_PER_PAGE: i64 = 1000;
/// `model.ReportingTimeFieldCreateAt` (post.go:81).
pub const REPORTING_TIME_FIELD_CREATE_AT: &str = "create_at";
/// `model.ReportingTimeFieldUpdateAt` (post.go:82).
pub const REPORTING_TIME_FIELD_UPDATE_AT: &str = "update_at";
/// `model.ReportingSortDirectionAsc` (post.go:83).
pub const REPORTING_SORT_DIRECTION_ASC: &str = "asc";
/// `model.ReportingSortDirectionDesc` (post.go:84).
pub const REPORTING_SORT_DIRECTION_DESC: &str = "desc";

fn is_zero_i64(n: &i64) -> bool {
    *n == 0
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// Port of `model.ReportPostOptions` (post.go:1458). Every field but `channel_id` is
/// `omitempty`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReportPostOptions {
    #[serde(rename = "channel_id")]
    pub channel_id: String,

    /// Epoch milliseconds; the first page starts here when it is positive.
    #[serde(rename = "start_time", skip_serializing_if = "is_zero_i64")]
    pub start_time: i64,

    #[serde(rename = "time_field", skip_serializing_if = "String::is_empty")]
    pub time_field: String,

    #[serde(rename = "sort_direction", skip_serializing_if = "String::is_empty")]
    pub sort_direction: String,

    /// Go's `int`; `0` and negatives become 100 and anything past 1000 is 1000.
    #[serde(rename = "per_page", skip_serializing_if = "is_zero_i64")]
    pub per_page: i64,

    #[serde(rename = "include_deleted", skip_serializing_if = "is_false")]
    pub include_deleted: bool,

    #[serde(rename = "exclude_system_posts", skip_serializing_if = "is_false")]
    pub exclude_system_posts: bool,

    #[serde(rename = "include_metadata", skip_serializing_if = "is_false")]
    pub include_metadata: bool,
}

/// Port of `model.ReportPostOptionsCursor` (post.go:1524).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReportPostOptionsCursor {
    #[serde(rename = "cursor", skip_serializing_if = "String::is_empty")]
    pub cursor: String,
}

/// The body of `POST /api/v4/reports/posts`: Go decodes into an anonymous struct embedding
/// [`ReportPostOptions`] and [`ReportPostOptionsCursor`], so the nine keys sit flat.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct ReportPostRequest {
    #[serde(flatten)]
    pub options: ReportPostOptions,
    #[serde(flatten)]
    pub cursor: ReportPostOptionsCursor,
}

/// Port of `model.ReportPostListResponse` (post.go:1529).
///
/// `posts` has no `omitempty` and the store always builds it as an empty, non-nil slice, so an
/// empty page is `[]` and never `null`. `next_cursor` is `omitempty` on a pointer: absent on the
/// last page.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReportPostListResponse {
    #[serde(rename = "posts")]
    pub posts: Vec<Post>,

    #[serde(rename = "next_cursor", skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<ReportPostOptionsCursor>,
}

/// Port of `model.ReportPostQueryParams` (post.go:1537) — the resolved parameters the store
/// runs. No `json:` tags; it never crosses the wire.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReportPostQueryParams {
    pub channel_id: String,
    pub cursor_time: i64,
    pub cursor_id: String,
    pub time_field: String,
    pub sort_direction: String,
    pub include_deleted: bool,
    pub exclude_system_posts: bool,
    pub per_page: i64,
}

impl ReportPostQueryParams {
    /// Port of `(*ReportPostQueryParams).Validate` (post.go:1552). Four checks in this order,
    /// each a 400; `per_page` and `cursor_time` are not looked at.
    pub fn validate(&self) -> Result<(), Box<AppError>> {
        let invalid = |id: &str, details: String| {
            AppError::boxed("ReportPostQueryParams.Validate", id, None, details, 400)
        };
        if !is_valid_id(&self.channel_id) {
            return Err(invalid(
                "model.post.query_params.invalid_channel_id",
                "channel_id must be a valid 26-character ID".to_owned(),
            ));
        }
        if self.time_field != REPORTING_TIME_FIELD_CREATE_AT
            && self.time_field != REPORTING_TIME_FIELD_UPDATE_AT
        {
            return Err(invalid(
                "model.post.query_params.invalid_time_field",
                format!(
                    "time_field must be {REPORTING_TIME_FIELD_CREATE_AT:?} or {REPORTING_TIME_FIELD_UPDATE_AT:?}"
                ),
            ));
        }
        if self.sort_direction != REPORTING_SORT_DIRECTION_ASC
            && self.sort_direction != REPORTING_SORT_DIRECTION_DESC
        {
            return Err(invalid(
                "model.post.query_params.invalid_sort_direction",
                format!(
                    "sort_direction must be {REPORTING_SORT_DIRECTION_ASC:?} or {REPORTING_SORT_DIRECTION_DESC:?}"
                ),
            ));
        }
        if !self.cursor_id.is_empty() && !is_valid_id(&self.cursor_id) {
            return Err(invalid(
                "model.post.query_params.invalid_cursor_id",
                "cursor_id must be a valid 26-character ID".to_owned(),
            ));
        }
        Ok(())
    }
}

/// `base64.URLEncoding`: the URL-safe alphabet, padded on encode. On decode Go ignores `\r` and
/// `\n` anywhere in the input and does **not** reject non-zero trailing bits (that is
/// `Strict()`, which this codec does not use), so the engine here allows them too.
const URL_ENGINE: base64::engine::GeneralPurpose = base64::engine::GeneralPurpose::new(
    &base64::alphabet::URL_SAFE,
    base64::engine::GeneralPurposeConfig::new().with_decode_allow_trailing_bits(true),
);

/// Port of `model.EncodeReportPostCursor` (post.go:1584).
pub fn encode_report_post_cursor(
    channel_id: &str,
    time_field: &str,
    include_deleted: bool,
    exclude_system_posts: bool,
    sort_direction: &str,
    timestamp: i64,
    post_id: &str,
) -> String {
    use base64::Engine as _;
    let plain = format!(
        "1:{channel_id}:{time_field}:{include_deleted}:{exclude_system_posts}:{sort_direction}:{timestamp}:{post_id}"
    );
    URL_ENGINE.encode(plain.as_bytes())
}

/// Port of `model.DecodeReportPostCursorV1` (post.go:1597). Seven 400s, in this order: the
/// base64, the part count (exactly eight), the version as an integer, the version being 1,
/// `include_deleted` and `exclude_system_posts` as `strconv.ParseBool`, and the timestamp as an
/// `int64`. `channel_id`, `time_field`, `sort_direction` and `post_id` are **not** checked
/// here — `validate` does that on the resolved parameters. `per_page` is left at zero for the
/// handler to fill.
pub fn decode_report_post_cursor_v1(cursor: &str) -> Result<ReportPostQueryParams, Box<AppError>> {
    use base64::Engine as _;
    let invalid = |id: &str, details: String| {
        AppError::boxed("DecodeReportPostCursorV1", id, None, details, 400)
    };

    // `encoding/base64` skips CR and LF wherever they appear; nothing else is skipped.
    let stripped: String = cursor
        .chars()
        .filter(|c| *c != '\r' && *c != '\n')
        .collect();
    let decoded = URL_ENGINE
        .decode(stripped.as_bytes())
        .map_err(|err| invalid("model.post.decode_cursor.invalid_base64", err.to_string()))?;
    // Go's `string(decoded)` keeps invalid UTF-8 as-is and splits on the byte `:`; a lossy
    // conversion only alters bytes that could never equal `:` or a digit.
    let decoded = String::from_utf8_lossy(&decoded);
    let parts: Vec<&str> = decoded.split(':').collect();
    if parts.len() != 8 {
        return Err(invalid(
            "model.post.decode_cursor.invalid_format",
            format!("expected 8 parts, got {}", parts.len()),
        ));
    }

    let version: i64 = parts[0].parse().map_err(|err| {
        invalid(
            "model.post.decode_cursor.invalid_version",
            format!("version must be an integer: {err}"),
        )
    })?;
    if version != 1 {
        return Err(invalid(
            "model.post.decode_cursor.unsupported_version",
            format!("version {version}"),
        ));
    }

    let include_deleted = parse_go_bool(parts[3]).ok_or_else(|| {
        invalid(
            "model.post.decode_cursor.invalid_include_deleted",
            "include_deleted must be a boolean".to_owned(),
        )
    })?;
    let exclude_system_posts = parse_go_bool(parts[4]).ok_or_else(|| {
        invalid(
            "model.post.decode_cursor.invalid_exclude_system_posts",
            "exclude_system_posts must be a boolean".to_owned(),
        )
    })?;
    let timestamp: i64 = parts[6].parse().map_err(|err| {
        invalid(
            "model.post.decode_cursor.invalid_timestamp",
            format!("timestamp must be an integer: {err}"),
        )
    })?;

    Ok(ReportPostQueryParams {
        channel_id: parts[1].to_owned(),
        cursor_time: timestamp,
        cursor_id: parts[7].to_owned(),
        time_field: parts[2].to_owned(),
        sort_direction: parts[5].to_owned(),
        include_deleted,
        exclude_system_posts,
        per_page: 0,
    })
}

/// Port of `model.RewriteRequest` (post.go:1480), the body of `POST /api/v4/posts/rewrite`.
/// `action` is Go's `RewriteAction` string type; an unknown value is refused by the app layer,
/// not by the decoder.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RewriteRequest {
    #[serde(rename = "agent_id")]
    pub agent_id: String,

    #[serde(rename = "message")]
    pub message: String,

    #[serde(rename = "action")]
    pub action: String,

    #[serde(rename = "custom_prompt", skip_serializing_if = "String::is_empty")]
    pub custom_prompt: String,

    #[serde(rename = "root_id", skip_serializing_if = "String::is_empty")]
    pub root_id: String,
}

/// Port of `model.RewriteResponse` (post.go:1488).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RewriteResponse {
    #[serde(rename = "rewritten_text")]
    pub rewritten_text: String,
}

/// The seven `RewriteAction` constants (post.go:1468); `getRewritePromptForAction` answers an
/// empty prompt — the 400 `app.post.rewrite.invalid_action` — for anything else when the
/// message is non-empty.
pub const REWRITE_ACTIONS: [&str; 7] = [
    "custom",
    "shorten",
    "elaborate",
    "improve_writing",
    "fix_spelling",
    "simplify",
    "summarize",
];

/// Port of the empty-prompt branch of `getRewritePromptForAction` (app/post.go:3661): with an
/// **empty message** every action is accepted (the prompt is built from `custom_prompt` alone),
/// and with a message the action has to be one of the seven.
pub fn rewrite_action_is_accepted(action: &str, message: &str) -> bool {
    message.is_empty() || REWRITE_ACTIONS.contains(&action)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn value_of(raw: &str) -> serde_json::Value {
        serde_json::from_str(raw).unwrap()
    }

    #[test]
    fn report_post_options_round_trips_the_fixture() {
        let raw = include_str!("../../../fixtures/report_post_options.json");
        let parsed: ReportPostOptions = serde_json::from_str(raw).unwrap();
        assert_eq!(serde_json::to_value(&parsed).unwrap(), value_of(raw));
    }

    #[test]
    fn report_post_options_cursor_round_trips_the_fixture() {
        let raw = include_str!("../../../fixtures/report_post_options_cursor.json");
        let parsed: ReportPostOptionsCursor = serde_json::from_str(raw).unwrap();
        assert_eq!(serde_json::to_value(&parsed).unwrap(), value_of(raw));
    }

    #[test]
    fn report_post_list_response_round_trips_the_fixture() {
        let raw = include_str!("../../../fixtures/report_post_list_response.json");
        let parsed: ReportPostListResponse = serde_json::from_str(raw).unwrap();
        assert_eq!(serde_json::to_value(&parsed).unwrap(), value_of(raw));
    }

    #[test]
    fn rewrite_request_and_response_round_trip_the_fixtures() {
        let raw = include_str!("../../../fixtures/rewrite_request.json");
        let parsed: RewriteRequest = serde_json::from_str(raw).unwrap();
        assert_eq!(serde_json::to_value(&parsed).unwrap(), value_of(raw));
        let raw = include_str!("../../../fixtures/rewrite_response.json");
        let parsed: RewriteResponse = serde_json::from_str(raw).unwrap();
        assert_eq!(serde_json::to_value(&parsed).unwrap(), value_of(raw));
    }

    #[test]
    fn the_zero_options_marshal_to_channel_id_alone() {
        assert_eq!(
            serde_json::to_string(&ReportPostOptions::default()).unwrap(),
            r#"{"channel_id":""}"#
        );
        assert_eq!(
            serde_json::to_string(&ReportPostOptionsCursor::default()).unwrap(),
            "{}"
        );
        assert_eq!(
            serde_json::to_string(&ReportPostListResponse::default()).unwrap(),
            r#"{"posts":[]}"#
        );
    }

    #[test]
    fn the_request_body_reads_both_halves_flat() {
        let request: ReportPostRequest =
            serde_json::from_str(r#"{"channel_id":"c","per_page":5,"cursor":"x"}"#).unwrap();
        assert_eq!(request.options.channel_id, "c");
        assert_eq!(request.options.per_page, 5);
        assert_eq!(request.cursor.cursor, "x");
    }

    #[test]
    fn the_cursor_round_trips_and_decodes_every_part() {
        let cursor = encode_report_post_cursor(
            "abcdefghijklmnopqrstuvwxyz",
            "update_at",
            true,
            false,
            "desc",
            1_700_000_000_123,
            "zyxwvutsrqponmlkjihgfedcba",
        );
        let params = decode_report_post_cursor_v1(&cursor).unwrap();
        assert_eq!(
            params,
            ReportPostQueryParams {
                channel_id: "abcdefghijklmnopqrstuvwxyz".to_owned(),
                cursor_time: 1_700_000_000_123,
                cursor_id: "zyxwvutsrqponmlkjihgfedcba".to_owned(),
                time_field: "update_at".to_owned(),
                sort_direction: "desc".to_owned(),
                include_deleted: true,
                exclude_system_posts: false,
                per_page: 0,
            }
        );
        assert!(params.validate().is_ok());
    }

    fn cursor_of(plain: &str) -> String {
        use base64::Engine as _;
        URL_ENGINE.encode(plain.as_bytes())
    }

    #[test]
    fn the_cursor_decoder_refuses_in_gos_order() {
        let id = |c: &str| decode_report_post_cursor_v1(c).unwrap_err().id;
        assert_eq!(id("not*base64"), "model.post.decode_cursor.invalid_base64");
        assert_eq!(
            id(&cursor_of("1:a:b")),
            "model.post.decode_cursor.invalid_format"
        );
        assert_eq!(
            id(&cursor_of("x:c:create_at:true:true:asc:1:p")),
            "model.post.decode_cursor.invalid_version"
        );
        assert_eq!(
            id(&cursor_of("2:c:create_at:true:true:asc:1:p")),
            "model.post.decode_cursor.unsupported_version"
        );
        assert_eq!(
            id(&cursor_of("1:c:create_at:yes:true:asc:1:p")),
            "model.post.decode_cursor.invalid_include_deleted"
        );
        assert_eq!(
            id(&cursor_of("1:c:create_at:true:maybe:asc:1:p")),
            "model.post.decode_cursor.invalid_exclude_system_posts"
        );
        assert_eq!(
            id(&cursor_of("1:c:create_at:true:false:asc:soon:p")),
            "model.post.decode_cursor.invalid_timestamp"
        );
        // `ParseBool` takes `t`/`F`/`1`; a `+` sign is fine for `ParseInt`.
        let ok = decode_report_post_cursor_v1(&cursor_of("+1:c:create_at:t:F:asc:+7:p")).unwrap();
        assert!(ok.include_deleted);
        assert!(!ok.exclude_system_posts);
        assert_eq!(ok.cursor_time, 7);
    }

    #[test]
    fn validate_checks_the_four_fields_in_order() {
        let good = ReportPostQueryParams {
            channel_id: "abcdefghijklmnopqrstuvwxyz".to_owned(),
            time_field: "create_at".to_owned(),
            sort_direction: "asc".to_owned(),
            ..Default::default()
        };
        assert!(good.validate().is_ok());
        let id = |p: ReportPostQueryParams| p.validate().unwrap_err().id;
        assert_eq!(
            id(ReportPostQueryParams {
                channel_id: "short".to_owned(),
                time_field: "bad".to_owned(),
                ..good.clone()
            }),
            "model.post.query_params.invalid_channel_id"
        );
        assert_eq!(
            id(ReportPostQueryParams {
                time_field: "edit_at".to_owned(),
                sort_direction: "bad".to_owned(),
                ..good.clone()
            }),
            "model.post.query_params.invalid_time_field"
        );
        assert_eq!(
            id(ReportPostQueryParams {
                sort_direction: "ASC".to_owned(),
                ..good.clone()
            }),
            "model.post.query_params.invalid_sort_direction"
        );
        assert_eq!(
            id(ReportPostQueryParams {
                cursor_id: "not-an-id".to_owned(),
                ..good.clone()
            }),
            "model.post.query_params.invalid_cursor_id"
        );
    }

    #[test]
    fn an_unknown_rewrite_action_is_only_refused_with_a_message() {
        assert!(rewrite_action_is_accepted("shorten", "hi"));
        assert!(!rewrite_action_is_accepted("translate", "hi"));
        assert!(rewrite_action_is_accepted("translate", ""));
    }
}
