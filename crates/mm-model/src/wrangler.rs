//! Port of `model/wrangler.go` (wrangler.go:1–33).
//!
//! The whole file is one struct and three accessors. It is here rather than deferred because
//! `(*PostList).BuildWranglerPostList` (post_list.go:207) returns it, and that is the last
//! function in `post_list.go` — porting the list without it would leave the file PARTIAL for a
//! 33-line dependency with no logic of its own.
//!
//! **None of the five fields carries a `json:` tag**, so the wire keys are the Go field names
//! verbatim — capitalisation, the `IDs` initialism and the `EarlistPostTimestamp` typo included.
//! Nothing in the model package marshals one today, but `fixtures/wrangler_post_list.json` pins
//! the shape so a future caller cannot silently invent snake_case keys.

use serde::{Deserialize, Serialize};

use crate::post::Post;

/// Port of `model.WranglerPostList` (wrangler.go:8).
///
/// Both slices are `Option` because Go builds them with `append` onto a nil slice and never
/// materialises them: an empty list marshals as `null`, not `[]`. See
/// [`crate::post_list::PostList::build_wrangler_post_list`], which returns a fully zero-valued
/// instance for an empty input.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WranglerPostList {
    #[serde(rename = "Posts")]
    pub posts: Option<Vec<Post>>,

    #[serde(rename = "ThreadUserIDs")]
    pub thread_user_ids: Option<Vec<String>>,

    /// `EarlistPostTimestamp` — the typo is upstream's (wrangler.go:12) and is wire surface,
    /// because the field has no `json:` tag to hide it behind.
    #[serde(rename = "EarlistPostTimestamp")]
    pub earlist_post_timestamp: i64,

    #[serde(rename = "LatestPostTimestamp")]
    pub latest_post_timestamp: i64,

    #[serde(rename = "FileAttachmentCount")]
    pub file_attachment_count: i64,
}

impl WranglerPostList {
    /// Port of `(*WranglerPostList).NumPosts` (wrangler.go:17).
    pub fn num_posts(&self) -> usize {
        self.posts.as_ref().map_or(0, Vec::len)
    }

    /// Port of `(*WranglerPostList).RootPost` (wrangler.go:22).
    ///
    /// Go returns nil for an empty list, which is `None` here — and which
    /// `BuildWranglerPostList` immediately dereferences without checking, having already
    /// returned early for that case.
    pub fn root_post(&self) -> Option<&Post> {
        self.posts.as_ref().and_then(|posts| posts.first())
    }

    /// Port of `(*WranglerPostList).ContainsFileAttachments` (wrangler.go:31).
    ///
    /// Tests `!= 0`, not `> 0`. The count is only ever incremented, so the distinction is
    /// unreachable — reproduced anyway.
    pub fn contains_file_attachments(&self) -> bool {
        self.file_attachment_count != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_fixture_round_trips() {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../../../fixtures/wrangler_post_list.json"))
                .unwrap();
        let wpl: WranglerPostList = serde_json::from_value(fixture.clone()).unwrap();
        assert_eq!(serde_json::to_value(&wpl).unwrap(), fixture);
    }

    /// Asserted against the serialised **string**, not a `serde_json::Value` — a `Value` is a
    /// `BTreeMap` and would sort the keys, hiding both the field order and the typo.
    #[test]
    fn the_wire_keys_are_the_go_field_names_in_declaration_order() {
        assert_eq!(
            serde_json::to_string(&WranglerPostList::default()).unwrap(),
            r#"{"Posts":null,"ThreadUserIDs":null,"EarlistPostTimestamp":0,"LatestPostTimestamp":0,"FileAttachmentCount":0}"#
        );
    }

    #[test]
    fn the_accessors_handle_an_empty_list() {
        let empty = WranglerPostList::default();
        assert_eq!(empty.num_posts(), 0);
        assert!(empty.root_post().is_none());
        assert!(!empty.contains_file_attachments());

        let one = WranglerPostList {
            posts: Some(vec![Post {
                id: "p1".into(),
                ..Default::default()
            }]),
            file_attachment_count: 2,
            ..Default::default()
        };
        assert_eq!(one.num_posts(), 1);
        assert_eq!(one.root_post().unwrap().id, "p1");
        assert!(one.contains_file_attachments());
    }
}
