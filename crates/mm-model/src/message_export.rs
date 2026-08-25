//! Port of `model/message_export.go` — one flattened post row for a compliance export.
//!
//! **No `json:` tags anywhere**: the rows are written to XML/CSV by the export backends, never to
//! a client. Almost every field is a pointer because the row comes from a wide LEFT JOIN — a
//! deleted channel or a purged user leaves NULLs, and the exporter has to be able to tell those
//! apart from empty strings.

use crate::post::POST_PROPS_PREVIEWED_POST;
use crate::utils::StringArray;

/// Port of `model.MessageExport` (message_export.go:5).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MessageExport {
    pub team_id: Option<String>,
    pub team_name: Option<String>,
    pub team_display_name: Option<String>,

    pub channel_id: Option<String>,
    pub channel_name: Option<String>,
    pub channel_display_name: Option<String>,
    /// `*ChannelType` — `O`, `P`, `D` or `G`. A `String` here for the same reason `channel.rs`
    /// keeps it one: Go's `ChannelType` is a defined string type, not a closed enum.
    pub channel_type: Option<String>,

    pub user_id: Option<String>,
    pub user_email: Option<String>,
    pub username: Option<String>,
    /// The **only** non-pointer scalar in the struct.
    pub is_bot: bool,

    pub post_id: Option<String>,
    pub post_create_at: Option<i64>,
    pub post_update_at: Option<i64>,
    pub post_delete_at: Option<i64>,
    pub post_edit_at: Option<i64>,
    pub post_message: Option<String>,
    pub post_type: Option<String>,
    pub post_root_id: Option<String>,
    /// The post's props as **raw JSON text**, not a decoded map — it is selected straight out of
    /// the column. [`MessageExport::preview_id`] is what parses it.
    pub post_props: Option<String>,
    pub post_original_id: Option<String>,
    pub post_file_ids: StringArray,
}

impl MessageExport {
    /// Port of `(*MessageExport).PreviewID` (message_export.go:41).
    ///
    /// Returns the `previewed_post` prop, or `""` when the props are absent, unparseable, or the
    /// key is missing. **Go type-asserts `val.(string)` unchecked and panics** if the prop holds
    /// anything else; here a non-string is `""`, which is the safe direction and the only
    /// difference.
    pub fn preview_id(&self) -> String {
        let Some(raw) = &self.post_props else {
            return String::new();
        };
        let Ok(props) = serde_json::from_str::<serde_json::Value>(raw) else {
            return String::new();
        };
        props
            .get(POST_PROPS_PREVIEWED_POST)
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string()
    }
}

/// Port of `model.MessageExportCursor` (message_export.go:34).
///
/// The range is **inclusive** at both ends: `[last_post_update_at + last_post_id,
/// until_update_at]`. The id is part of the lower bound because `UpdateAt` is not unique.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MessageExportCursor {
    pub last_post_update_at: i64,
    pub last_post_id: String,
    pub until_update_at: i64,
}
