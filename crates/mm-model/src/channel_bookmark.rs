//! Port of `model/channel_bookmark.go` — the links, files and boards pinned to a channel's header.
//!
//! # `IsValid` is a matrix, not a list
//!
//! The rules depend on `Type`, and the interesting ones are the **negative** cases: a `link` may
//! not carry a `file_id`, a `file` may not carry a `link_url`, and a `board` may carry neither a
//! `file_id` nor an `image_url` and its `link_url` must be **relative** — `/…`, not `//…` and
//! with no `://` anywhere. That last rule is an open-redirect guard: a board bookmark's URL is
//! server-generated and must never point off-site.
//!
//! # Two names that mislead
//!
//! `DisplayNameMaxRunes` and `LinkMaxRunes` are declared in this file with no `ChannelBookmark`
//! prefix, so they read like package-wide limits. They are not — they bound this type only.

use serde::{Deserialize, Serialize};

use crate::channel::{Channel, ChannelWithTeamData};
use crate::file_info::FileInfo;
use crate::serde_helpers::{is_empty_str, is_none, is_none_or_empty_vec};
use crate::utils::{
    AppError, AppResult, get_millis, is_valid_http_url, is_valid_id, new_id, sanitize_unicode,
};

/// Port of `model.ChannelBookmarkType` (channel_bookmark.go:12).
pub const CHANNEL_BOOKMARK_LINK: &str = "link";
pub const CHANNEL_BOOKMARK_FILE: &str = "file";
pub const CHANNEL_BOOKMARK_BOARD: &str = "board";

/// Port of `model.BookmarkFileOwner` (channel_bookmark.go:18) — the `FileInfo.CreatorId` written
/// for a bookmark's file, in place of a user id.
pub const BOOKMARK_FILE_OWNER: &str = "bookmark";
pub const MAX_BOOKMARKS_PER_CHANNEL: i64 = 50;
/// Bounds `ChannelBookmark.display_name` only — see the module docs.
pub const DISPLAY_NAME_MAX_RUNES: usize = 64;
/// Bounds `link_url` and `image_url` only.
pub const LINK_MAX_RUNES: usize = 1024;

/// Port of `isValidChannelBookmarkType` (channel_bookmark.go:37).
pub fn is_valid_channel_bookmark_type(t: &str) -> bool {
    matches!(
        t,
        CHANNEL_BOOKMARK_LINK | CHANNEL_BOOKMARK_FILE | CHANNEL_BOOKMARK_BOARD
    )
}

/// Port of `model.IsExternallyManagedChannelBookmarkType` (channel_bookmark.go:47).
///
/// A `board` bookmark's lifecycle is owned by the boards subsystem, **not** by the bookmarks API —
/// so the create/update/delete routes must refuse it even though it is a valid type.
pub fn is_externally_managed_channel_bookmark_type(t: &str) -> bool {
    t == CHANNEL_BOOKMARK_BOARD
}

/// Port of `model.ChannelBookmark` (channel_bookmark.go:51).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ChannelBookmark {
    #[serde(rename = "id")]
    pub id: String,

    #[serde(rename = "create_at")]
    pub create_at: i64,

    #[serde(rename = "update_at")]
    pub update_at: i64,

    #[serde(rename = "delete_at")]
    pub delete_at: i64,

    #[serde(rename = "channel_id")]
    pub channel_id: String,

    #[serde(rename = "owner_id")]
    pub owner_id: String,

    /// Required for a `file` bookmark, forbidden for the other two.
    #[serde(rename = "file_id")]
    pub file_id: String,

    #[serde(rename = "display_name")]
    pub display_name: String,

    #[serde(rename = "sort_order")]
    pub sort_order: i64,

    #[serde(rename = "link_url", skip_serializing_if = "is_empty_str")]
    pub link_url: String,

    #[serde(rename = "image_url", skip_serializing_if = "is_empty_str")]
    pub image_url: String,

    /// Stored **without** the surrounding colons; `PreSave` trims them.
    #[serde(rename = "emoji", skip_serializing_if = "is_empty_str")]
    pub emoji: String,

    #[serde(rename = "type")]
    pub type_: String,

    /// The board id for a `board` bookmark; must be empty otherwise.
    #[serde(rename = "target_id", skip_serializing_if = "is_empty_str")]
    pub target_id: String,

    /// Set when this bookmark was copied from another — see
    /// [`ChannelBookmark::set_original`].
    #[serde(rename = "original_id", skip_serializing_if = "is_empty_str")]
    pub original_id: String,

    #[serde(rename = "parent_id", skip_serializing_if = "is_empty_str")]
    pub parent_id: String,
}

impl ChannelBookmark {
    /// Port of `(*ChannelBookmark).SetOriginal` (channel_bookmark.go:92).
    ///
    /// Produces a **new, unsaved** bookmark: the id and all three timestamps are cleared,
    /// `original_id` points at the source, and the owner is replaced. `parent_id` and
    /// `target_id` are carried over unchanged.
    pub fn set_original(&self, new_owner_id: &str) -> ChannelBookmark {
        let mut copy = self.clone();
        copy.id = String::new();
        copy.create_at = 0;
        copy.delete_at = 0;
        copy.update_at = 0;
        copy.original_id = self.id.clone();
        copy.owner_id = new_owner_id.to_string();
        copy
    }

    /// Port of `(*ChannelBookmark).IsValid` (channel_bookmark.go:104). See the module docs for
    /// the per-type matrix.
    ///
    /// Note the two error ids that are **shared across different failures**:
    /// `file_id.missing_or_invalid` covers both "a link has a file" and "a file has no file", and
    /// `link_url.missing_or_invalid` likewise. A caller cannot tell them apart from the id.
    pub fn is_valid(&self) -> AppResult {
        let details = || format!("id={}", self.id);

        if !is_valid_id(&self.id) {
            return Err(err("id", String::new()));
        }

        if self.create_at == 0 {
            return Err(err("create_at", details()));
        }

        if self.update_at == 0 {
            return Err(err("update_at", details()));
        }

        if !is_valid_id(&self.channel_id) {
            return Err(err("channel_id", String::new()));
        }

        if !is_valid_id(&self.owner_id) {
            return Err(err("owner_id", String::new()));
        }

        if self.display_name.is_empty()
            || self.display_name.chars().count() > DISPLAY_NAME_MAX_RUNES
        {
            return Err(err("display_name", String::new()));
        }

        if !is_valid_channel_bookmark_type(&self.type_) {
            return Err(err("type", details()));
        }

        // `link` and `file` must leave `target_id` empty.
        if matches!(
            self.type_.as_str(),
            CHANNEL_BOOKMARK_LINK | CHANNEL_BOOKMARK_FILE
        ) && !self.target_id.is_empty()
        {
            return Err(err("target_id", details()));
        }

        if self.type_ == CHANNEL_BOOKMARK_BOARD {
            if self.target_id.is_empty() || !is_valid_id(&self.target_id) {
                return Err(err("board.target_id", details()));
            }
            // Relative, single-slash, and no scheme anywhere — the open-redirect guard.
            if self.link_url.is_empty()
                || !self.link_url.starts_with('/')
                || self.link_url.starts_with("//")
                || self.link_url.contains("://")
                || self.link_url.chars().count() > LINK_MAX_RUNES
            {
                return Err(err("board.link_url", details()));
            }
            if !self.file_id.is_empty() {
                return Err(err("board.file_id", details()));
            }
            if !self.image_url.is_empty() {
                return Err(err("board.image_url", details()));
            }
        }

        if self.type_ == CHANNEL_BOOKMARK_LINK && !self.file_id.is_empty() {
            return Err(err("file_id.missing_or_invalid", details()));
        }

        if self.type_ == CHANNEL_BOOKMARK_FILE && !self.link_url.is_empty() {
            return Err(err("link_url.missing_or_invalid", details()));
        }

        if self.type_ == CHANNEL_BOOKMARK_LINK
            && (self.link_url.is_empty()
                || !is_valid_http_url(&self.link_url)
                || self.link_url.chars().count() > LINK_MAX_RUNES)
        {
            return Err(err("link_url.missing_or_invalid", details()));
        }

        if self.type_ == CHANNEL_BOOKMARK_LINK
            && !self.image_url.is_empty()
            && (!is_valid_http_url(&self.image_url)
                || self.image_url.chars().count() > LINK_MAX_RUNES)
        {
            return Err(err("image_url", details()));
        }

        if self.type_ == CHANNEL_BOOKMARK_FILE
            && (self.file_id.is_empty() || !is_valid_id(&self.file_id))
        {
            return Err(err("file_id.missing_or_invalid", details()));
        }

        // Applies to **every** type: a bookmark cannot have both an image URL and a file.
        if !self.image_url.is_empty() && !self.file_id.is_empty() {
            return Err(err("link_file", details()));
        }

        if !self.original_id.is_empty() && !is_valid_id(&self.original_id) {
            return Err(err("original_id", String::new()));
        }

        if !self.parent_id.is_empty() && !is_valid_id(&self.parent_id) {
            return Err(err("parent_id", String::new()));
        }

        Ok(())
    }

    /// Port of `(*ChannelBookmark).PreSave` (channel_bookmark.go:190).
    ///
    /// `strings.Trim(emoji, ":")` strips **every** leading and trailing colon, not just one pair.
    pub fn pre_save(&mut self) {
        if self.id.is_empty() {
            self.id = new_id();
        }

        self.display_name = sanitize_unicode(&self.display_name);
        self.emoji = self.emoji.trim_matches(':').to_string();
        if self.create_at == 0 {
            self.create_at = get_millis();
        }
        self.update_at = self.create_at;
    }

    /// Port of `(*ChannelBookmark).PreUpdate` (channel_bookmark.go:203).
    pub fn pre_update(&mut self) {
        self.update_at = get_millis();
        self.display_name = sanitize_unicode(&self.display_name);
        self.emoji = self.emoji.trim_matches(':').to_string();
    }

    /// Port of `(*ChannelBookmark).ToBookmarkWithFileInfo` (channel_bookmark.go:209).
    ///
    /// Copies every field and re-trims the emoji — so a bookmark that never went through
    /// `PreSave` still comes out trimmed here. A `FileInfo` with an **empty id** is dropped
    /// rather than attached.
    pub fn to_bookmark_with_file_info(
        &self,
        file: Option<&FileInfo>,
    ) -> ChannelBookmarkWithFileInfo {
        let mut bookmark = self.clone();
        bookmark.emoji = self.emoji.trim_matches(':').to_string();

        ChannelBookmarkWithFileInfo {
            bookmark,
            file_info: file.filter(|f| !f.id.is_empty()).cloned().map(Box::new),
        }
    }

    /// Port of `(*ChannelBookmark).Patch` (channel_bookmark.go:252).
    pub fn patch(&mut self, patch: &ChannelBookmarkPatch) {
        if let Some(file_id) = &patch.file_id {
            self.file_id = file_id.clone();
        }
        if let Some(display_name) = &patch.display_name {
            self.display_name = display_name.clone();
        }
        if let Some(sort_order) = patch.sort_order {
            self.sort_order = sort_order;
        }
        if let Some(link_url) = &patch.link_url {
            self.link_url = link_url.clone();
        }
        if let Some(image_url) = &patch.image_url {
            self.image_url = image_url.clone();
        }
        if let Some(emoji) = &patch.emoji {
            self.emoji = emoji.clone();
        }
    }
}

fn err(suffix: &str, details: String) -> Box<AppError> {
    Box::new(AppError::new(
        "ChannelBookmark.IsValid",
        format!("model.channel_bookmark.is_valid.{suffix}.app_error"),
        None,
        details,
        400,
    ))
}

/// Port of `model.ChannelBookmarkPatch` (channel_bookmark.go:237).
///
/// **`type` and `target_id` are absent on purpose**: a bookmark cannot change kind, and a board
/// bookmark cannot be repointed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ChannelBookmarkPatch {
    #[serde(rename = "file_id")]
    pub file_id: Option<String>,

    #[serde(rename = "display_name")]
    pub display_name: Option<String>,

    #[serde(rename = "sort_order")]
    pub sort_order: Option<i64>,

    #[serde(rename = "link_url", skip_serializing_if = "is_none")]
    pub link_url: Option<String>,

    #[serde(rename = "image_url", skip_serializing_if = "is_none")]
    pub image_url: Option<String>,

    #[serde(rename = "emoji", skip_serializing_if = "is_none")]
    pub emoji: Option<String>,
}

/// Port of `model.ChannelBookmarkWithFileInfo` (channel_bookmark.go:273).
///
/// The embedded pointer is **inlined**, so the file sits under `file` beside the bookmark's own
/// keys rather than nested under a bookmark object.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ChannelBookmarkWithFileInfo {
    #[serde(flatten)]
    pub bookmark: ChannelBookmark,

    #[serde(rename = "file", skip_serializing_if = "is_none")]
    pub file_info: Option<Box<FileInfo>>,
}

/// Port of `model.ChannelWithBookmarks` (channel_bookmark.go:294).
// `Channel` derives `PartialEq` but not `Eq`, so this cannot either.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ChannelWithBookmarks {
    #[serde(flatten)]
    pub channel: Channel,

    #[serde(rename = "bookmarks", skip_serializing_if = "is_none_or_empty_vec")]
    pub bookmarks: Option<Vec<ChannelBookmarkWithFileInfo>>,
}

/// Port of `model.ChannelWithTeamDataAndBookmarks` (channel_bookmark.go:299).
// As above: `ChannelWithTeamData` is `PartialEq` only.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ChannelWithTeamDataAndBookmarks {
    #[serde(flatten)]
    pub channel: ChannelWithTeamData,

    #[serde(rename = "bookmarks", skip_serializing_if = "is_none_or_empty_vec")]
    pub bookmarks: Option<Vec<ChannelBookmarkWithFileInfo>>,
}

/// Port of `model.UpdateChannelBookmarkResponse` (channel_bookmark.go:304).
///
/// `Deleted` is populated when an update **replaced** a bookmark rather than editing it in place —
/// which happens when the file behind a `file` bookmark changes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct UpdateChannelBookmarkResponse {
    #[serde(rename = "updated", skip_serializing_if = "is_none")]
    pub updated: Option<ChannelBookmarkWithFileInfo>,

    #[serde(rename = "deleted", skip_serializing_if = "is_none")]
    pub deleted: Option<ChannelBookmarkWithFileInfo>,
}

/// Port of `model.ChannelBookmarkAndFileInfo` (channel_bookmark.go:319) — the flattened join row.
///
/// No tags: it exists to be scanned from a query and immediately converted. Note the row has both
/// a `FileInfoId` (the bookmark's column) and a `FileId` (the joined file's own id); they are the
/// same value when the join matched and the conversion checks **both**.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChannelBookmarkAndFileInfo {
    pub id: String,
    pub create_at: i64,
    pub update_at: i64,
    pub delete_at: i64,
    pub channel_id: String,
    pub owner_id: String,
    /// The bookmark's `FileId` column.
    pub file_info_id: String,
    pub display_name: String,
    pub sort_order: i64,
    pub link_url: String,
    pub image_url: String,
    pub emoji: String,
    pub type_: String,
    pub target_id: String,
    pub original_id: String,
    pub parent_id: String,

    /// The joined `FileInfo.Id`.
    pub file_id: String,
    pub file_name: String,
    pub extension: String,
    pub size: i64,
    pub mime_type: String,
    pub width: i64,
    pub height: i64,
    pub has_preview_image: bool,
    pub mini_preview: Option<Vec<u8>>,
}

impl ChannelBookmarkAndFileInfo {
    /// Port of `(*ChannelBookmarkAndFileInfo).ToChannelBookmarkWithFileInfo`
    /// (channel_bookmark.go:348).
    ///
    /// Two details: the emoji is **not** re-trimmed here (unlike
    /// `ChannelBookmark::to_bookmark_with_file_info`), and an **empty** mini-preview is collapsed
    /// to nil so it serialises as `null` rather than `""`.
    ///
    /// **Go dereferences `o.MiniPreview` unguarded** while checking its length, so a row with a
    /// nil pointer and a non-empty file id panics. Here `None` simply stays `None`.
    pub fn to_channel_bookmark_with_file_info(&self) -> ChannelBookmarkWithFileInfo {
        let bookmark = ChannelBookmark {
            id: self.id.clone(),
            create_at: self.create_at,
            update_at: self.update_at,
            delete_at: self.delete_at,
            channel_id: self.channel_id.clone(),
            owner_id: self.owner_id.clone(),
            file_id: self.file_info_id.clone(),
            display_name: self.display_name.clone(),
            sort_order: self.sort_order,
            link_url: self.link_url.clone(),
            image_url: self.image_url.clone(),
            emoji: self.emoji.clone(),
            type_: self.type_.clone(),
            target_id: self.target_id.clone(),
            original_id: self.original_id.clone(),
            parent_id: self.parent_id.clone(),
        };

        let file_info = if !self.file_info_id.is_empty() && !self.file_id.is_empty() {
            let mini_preview = self
                .mini_preview
                .as_ref()
                .filter(|preview| !preview.is_empty())
                .cloned();

            Some(Box::new(FileInfo {
                id: self.file_id.clone(),
                name: self.file_name.clone(),
                extension: self.extension.clone(),
                size: self.size,
                mime_type: self.mime_type.clone(),
                width: self.width,
                height: self.height,
                has_preview_image: self.has_preview_image,
                mini_preview,
                ..Default::default()
            }))
        } else {
            None
        };

        ChannelBookmarkWithFileInfo {
            bookmark,
            file_info,
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
    fn channel_bookmark_round_trips_the_fixture() {
        assert_fixture_round_trips!(ChannelBookmark, "channel_bookmark");
    }
    #[test]
    fn channel_bookmark_patch_round_trips_the_fixture() {
        assert_fixture_round_trips!(ChannelBookmarkPatch, "channel_bookmark_patch");
    }
    #[test]
    fn channel_bookmark_with_file_info_round_trips_the_fixture() {
        assert_fixture_round_trips!(
            ChannelBookmarkWithFileInfo,
            "channel_bookmark_with_file_info"
        );
    }
    #[test]
    fn channel_with_bookmarks_round_trips_the_fixture() {
        assert_fixture_round_trips!(ChannelWithBookmarks, "channel_with_bookmarks");
    }
    #[test]
    fn channel_with_team_data_and_bookmarks_round_trips_the_fixture() {
        assert_fixture_round_trips!(
            ChannelWithTeamDataAndBookmarks,
            "channel_with_team_data_and_bookmarks"
        );
    }
    #[test]
    fn update_channel_bookmark_response_round_trips_the_fixture() {
        assert_fixture_round_trips!(
            UpdateChannelBookmarkResponse,
            "update_channel_bookmark_response"
        );
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

    fn bookmark(mut mutate: impl FnMut(&mut ChannelBookmark)) -> ChannelBookmark {
        let mut b = ChannelBookmark {
            id: "b1a2b3c4d5e6f7g8h9i0j1k2l3".to_string(),
            create_at: 1_700_000_000_000,
            update_at: 1_700_000_000_000,
            channel_id: "c1a2b3c4d5e6f7g8h9i0j1k2l3".to_string(),
            owner_id: "o1a2b3c4d5e6f7g8h9i0j1k2l3".to_string(),
            display_name: "Runbook".to_string(),
            type_: CHANNEL_BOOKMARK_LINK.to_string(),
            link_url: "https://example.com/runbook".to_string(),
            ..Default::default()
        };
        mutate(&mut b);
        b
    }

    fn repeat(c: char, n: usize) -> String {
        std::iter::repeat_n(c, n).collect()
    }

    /// `IsValid` is a per-type matrix: each of the three types requires one field and **forbids**
    /// the other two's. The negative rows are the point — a validator written as a flat field
    /// list passes every positive case and none of these.
    ///
    /// The three `board_*` rows are the security-relevant ones: a board bookmark's `link_url` is
    /// required to be relative, and Go rejects `//host` and an embedded scheme separately.
    #[test]
    fn is_valid_and_presave_match_go() {
        let oracle = oracle();
        let cases = oracle["channel_bookmark_is_valid"].as_array().unwrap();

        let inputs: Vec<(&str, ChannelBookmark)> = vec![
            ("link_ok", bookmark(|_| {})),
            (
                "link_with_file",
                bookmark(|b| b.file_id = "f1a2b3c4d5e6f7g8h9i0j1k2l3".to_string()),
            ),
            ("link_no_url", bookmark(|b| b.link_url = String::new())),
            (
                "link_relative_url",
                bookmark(|b| b.link_url = "/relative".to_string()),
            ),
            (
                "link_with_image",
                bookmark(|b| b.image_url = "https://example.com/i.png".to_string()),
            ),
            (
                "link_with_bad_image",
                bookmark(|b| b.image_url = "not a url".to_string()),
            ),
            (
                "link_with_target",
                bookmark(|b| b.target_id = "t1a2b3c4d5e6f7g8h9i0j1k2l3".to_string()),
            ),
            (
                "file_ok",
                bookmark(|b| {
                    b.type_ = CHANNEL_BOOKMARK_FILE.to_string();
                    b.link_url = String::new();
                    b.file_id = "f1a2b3c4d5e6f7g8h9i0j1k2l3".to_string();
                }),
            ),
            (
                "file_with_link",
                bookmark(|b| {
                    b.type_ = CHANNEL_BOOKMARK_FILE.to_string();
                    b.file_id = "f1a2b3c4d5e6f7g8h9i0j1k2l3".to_string();
                }),
            ),
            (
                "file_no_file",
                bookmark(|b| {
                    b.type_ = CHANNEL_BOOKMARK_FILE.to_string();
                    b.link_url = String::new();
                }),
            ),
            (
                "board_ok",
                bookmark(|b| {
                    b.type_ = CHANNEL_BOOKMARK_BOARD.to_string();
                    b.target_id = "t1a2b3c4d5e6f7g8h9i0j1k2l3".to_string();
                    b.link_url = "/boards/team/1/2".to_string();
                }),
            ),
            (
                "board_absolute_url",
                bookmark(|b| {
                    b.type_ = CHANNEL_BOOKMARK_BOARD.to_string();
                    b.target_id = "t1a2b3c4d5e6f7g8h9i0j1k2l3".to_string();
                }),
            ),
            (
                "board_protocol_relative",
                bookmark(|b| {
                    b.type_ = CHANNEL_BOOKMARK_BOARD.to_string();
                    b.target_id = "t1a2b3c4d5e6f7g8h9i0j1k2l3".to_string();
                    b.link_url = "//evil.example.com/x".to_string();
                }),
            ),
            (
                "board_scheme_inside",
                bookmark(|b| {
                    b.type_ = CHANNEL_BOOKMARK_BOARD.to_string();
                    b.target_id = "t1a2b3c4d5e6f7g8h9i0j1k2l3".to_string();
                    b.link_url = "/redirect?to=https://evil.example.com".to_string();
                }),
            ),
            (
                "board_no_target",
                bookmark(|b| {
                    b.type_ = CHANNEL_BOOKMARK_BOARD.to_string();
                    b.link_url = "/boards/team/1/2".to_string();
                }),
            ),
            (
                "board_with_file",
                bookmark(|b| {
                    b.type_ = CHANNEL_BOOKMARK_BOARD.to_string();
                    b.target_id = "t1a2b3c4d5e6f7g8h9i0j1k2l3".to_string();
                    b.link_url = "/boards/team/1/2".to_string();
                    b.file_id = "f1a2b3c4d5e6f7g8h9i0j1k2l3".to_string();
                }),
            ),
            (
                "image_and_file",
                bookmark(|b| {
                    b.type_ = CHANNEL_BOOKMARK_FILE.to_string();
                    b.link_url = String::new();
                    b.file_id = "f1a2b3c4d5e6f7g8h9i0j1k2l3".to_string();
                    b.image_url = "https://example.com/i.png".to_string();
                }),
            ),
            (
                "unknown_type",
                bookmark(|b| b.type_ = "sticker".to_string()),
            ),
            (
                "empty_display_name",
                bookmark(|b| b.display_name = String::new()),
            ),
            (
                "long_display_name",
                bookmark(|b| b.display_name = repeat('é', 65)),
            ),
            (
                "display_name_at_cap",
                bookmark(|b| b.display_name = repeat('é', 64)),
            ),
            (
                "bad_original_id",
                bookmark(|b| b.original_id = "short".to_string()),
            ),
            (
                "bad_parent_id",
                bookmark(|b| b.parent_id = "short".to_string()),
            ),
        ];
        assert_eq!(
            cases.len(),
            inputs.len() + 6,
            "the corpus is the IsValid cases plus six PreSave rows"
        );

        for (case, (name, b)) in cases.iter().zip(inputs.iter()) {
            assert_eq!(
                case["name"].as_str().unwrap(),
                *name,
                "corpus order drifted"
            );
            match (b.is_valid(), case.get("error_id").and_then(|v| v.as_str())) {
                (Ok(()), None) => {}
                (Ok(()), Some(id)) => panic!("{name}: Go rejected with {id}, the port accepted"),
                (Err(e), None) => panic!("{name}: Go accepted, the port rejected with {}", e.id),
                (Err(e), Some(id)) => {
                    assert_eq!(e.id, id, "{name}");
                    assert_eq!(
                        e.detailed_error,
                        case["error_details"].as_str().unwrap(),
                        "{name}"
                    );
                }
            }
        }

        // PreSave trims the emoji's wrapping colons — but only one pair, and only a matched pair.
        for case in &cases[inputs.len()..] {
            let name = case["name"].as_str().unwrap();
            let emoji = name.strip_prefix("presave_emoji:").expect("presave row");
            let mut b = bookmark(|b| b.emoji = emoji.to_string());
            let before = b.create_at;
            b.pre_save();
            assert_eq!(b.emoji, case["emoji_after"].as_str().unwrap(), "{name}");
            assert_eq!(
                b.create_at == before,
                case["create_at_retained"].as_bool().unwrap(),
                "{name}"
            );
            assert_eq!(
                b.update_at == b.create_at,
                case["update_at_equals"].as_bool().unwrap(),
                "{name}"
            );
        }
    }
}
