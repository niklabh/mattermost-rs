//! Port of `model/post_list.go` (post_list.go:1–241).
//!
//! `PostList` is what every read endpoint returns, so its wire format is the most exposed in the
//! crate. Two things about it are easy to get wrong and neither is visible from the struct:
//!
//! # Nil and empty are different, and most methods convert one into the other
//!
//! `order` and `posts` carry **no** `omitempty`, so a nil slice or map reaches the client as
//! `null` rather than `[]`/`{}`. Both are therefore `Option`. Five methods materialise some
//! subset of the three collections and **no two do the same subset**:
//!
//! | | `order` | `posts` | `burn_on_read_posts` |
//! |---|---|---|---|
//! | [`PostList::new`] | ✓ | ✓ | ✓ |
//! | [`PostList::go_clone`] | ✓ | ✓ | ✓ |
//! | [`PostList::strip_action_integrations`] | — | ✓ | — |
//! | [`PostList::make_non_nil`] | ✓ | ✓ | — |
//! | [`PostList::unique_order`] | ✓ | — | — |
//!
//! Every row is measured in `fixtures/behaviour_post_list.json`, not read off the source.
//!
//! # `Etag` is order-independent here, unlike `ChannelList::etag`
//!
//! Go iterates a **map** to compute it, so an order-dependent answer would be nondeterministic
//! between runs of the same server. The `v.Id > id` tie-break is what saves it: the loop is a
//! maximum over the pair `(update_at, id)` seeded with `(0, "0")`. That seed is reachable — a
//! post with `update_at: 0` and an id above `"0"` beats it, one below does not.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::post::{POST_TYPE_BURN_ON_READ, Post};
use crate::utils::{StringArray, etag, go_json_marshal};
use crate::wrangler::WranglerPostList;

/// The post map. A `BTreeMap` rather than a `HashMap` because Go sorts map keys by byte value
/// when marshalling and `String`'s `Ord` is byte-wise — see [D-027].
pub type PostMap = BTreeMap<String, Post>;

/// Port of `model.PostList` (post_list.go:12).
///
/// `burn_on_read_posts` is `json:"-"` in Go, so it is **always nil on a decoded list** — which
/// is what makes [`PostList::add_post`]'s divergence reachable.
///
/// The container carries `#[serde(default)]` for the same reason [`Post`] does: Go leaves an
/// absent field at its zero value, and a client sending a partial list would otherwise be
/// rejected by serde where the Go server accepts it. See [D-043].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PostList {
    /// The display order, newest first. Nil and empty are distinguishable on the wire.
    #[serde(rename = "order")]
    pub order: Option<StringArray>,

    /// The posts, keyed by id. An order entry with no matching post here is legal and reachable
    /// — see [`PostList::to_slice`].
    #[serde(rename = "posts")]
    pub posts: Option<PostMap>,

    #[serde(rename = "next_post_id")]
    pub next_post_id: String,

    #[serde(rename = "prev_post_id")]
    pub prev_post_id: String,

    /// Whether more items can be fetched. A pointer with `omitempty` in Go, so `Some(false)`
    /// serialises as `false` and only `None` drops the key.
    #[serde(rename = "has_next", default, skip_serializing_if = "Option::is_none")]
    pub has_next: Option<bool>,

    /// The time of the latest inaccessible post, when the list was truncated by a retention
    /// policy. Epoch milliseconds.
    #[serde(rename = "first_inaccessible_post_time")]
    pub first_inaccessible_post_time: i64,

    /// `json:"-"` — never on the wire, populated only by [`PostList::add_post`].
    #[serde(skip)]
    pub burn_on_read_posts: Option<PostMap>,
}

/// The failure modes of [`PostList::encode_json`]. Go returns a bare `error` covering both.
#[derive(Debug, thiserror::Error)]
pub enum EncodeJsonError {
    #[error("serialize post list: {0}")]
    Serialize(#[from] serde_json::Error),
    #[error("write post list: {0}")]
    Write(#[from] std::io::Error),
}

impl PostList {
    /// Port of `model.NewPostList` (post_list.go:26).
    ///
    /// **Not the same as [`PostList::default`].** `Default` is Go's zero value, where all three
    /// collections are nil and serialise as `null`; this materialises them, so a list built here
    /// serialises with `[]` and `{}`. Both states are reachable and the difference reaches the
    /// client, so the two constructors are deliberately kept apart.
    pub fn new() -> Self {
        Self {
            order: Some(Vec::new()),
            posts: Some(PostMap::new()),
            next_post_id: String::new(),
            prev_post_id: String::new(),
            has_next: None,
            first_inaccessible_post_time: 0,
            burn_on_read_posts: Some(PostMap::new()),
        }
    }

    /// Port of `(*PostList).Clone` (post_list.go:36).
    ///
    /// Named `go_clone` rather than implementing `Clone`, because it is **not** a copy: it
    /// materialises all three nil collections into empty ones, so `go_clone` of a zero list is
    /// not equal to that list. Rust's `Clone` contract says a clone compares equal to its
    /// source, and `#[derive(Clone)]` on this type honours that — reach for `.clone()` when you
    /// want a copy and for `go_clone` when you are porting a Go call site.
    ///
    /// One divergence: Go copies `HasNext` as a **pointer**, so writing through the clone writes
    /// through to the original. `Option<bool>` is a value, so ours is independent. Same class as
    /// [D-036]; pinned by `clone_matches_go`.
    pub fn go_clone(&self) -> Self {
        Self {
            order: Some(self.order.clone().unwrap_or_default()),
            posts: Some(self.posts.clone().unwrap_or_default()),
            next_post_id: self.next_post_id.clone(),
            prev_post_id: self.prev_post_id.clone(),
            has_next: self.has_next,
            first_inaccessible_post_time: self.first_inaccessible_post_time,
            burn_on_read_posts: Some(self.burn_on_read_posts.clone().unwrap_or_default()),
        }
    }

    /// Port of `(*PostList).ForPlugin` (post_list.go:58).
    ///
    /// Clones (and therefore materialises the nil collections) and then runs every post through
    /// [`Post::for_plugin`], which strips the metadata.
    pub fn for_plugin(&self) -> Self {
        let mut copy = self.go_clone();
        if let Some(posts) = copy.posts.as_mut() {
            for post in posts.values_mut() {
                *post = post.for_plugin();
            }
        }
        copy
    }

    /// Port of `(*PostList).ToSlice` (post_list.go:66).
    ///
    /// Returns one entry per **order** id, so a post that is in `posts` but not in `order` does
    /// not appear, and an order id with no post yields `None` — Go's `[]*Post` holds a nil
    /// there. That is reachable through [`PostList::add_order`], which takes an id without
    /// requiring a post, so the `Option` is not defensive.
    ///
    /// One thing Go expresses that this does not: Go's result is a **nil** slice when `posts` is
    /// empty and a zero-length allocated slice when `posts` is non-empty but `order` is empty.
    /// No Go call site can observe the difference — all of them range over it or take its length
    /// — so both map to an empty `Vec` here. The oracle records the flag regardless.
    pub fn to_slice(&self) -> Vec<Option<&Post>> {
        let Some(order) = self.order.as_ref() else {
            return Vec::new();
        };
        order
            .iter()
            .map(|id| self.posts.as_ref().and_then(|posts| posts.get(id)))
            .collect()
    }

    /// Port of `(*PostList).StripActionIntegrations` (post_list.go:88).
    ///
    /// Rebuilds `posts` from scratch, so a nil map becomes an empty one — and `order` is left
    /// alone, so a list can come out of this holding `"order":null,"posts":{}`.
    ///
    /// Go clones each post before stripping it, which matters only because
    /// [`Self::to_json`] shares the map with its receiver. We own the map after cloning the
    /// whole list, so the clone is implicit.
    pub fn strip_action_integrations(&mut self) {
        let mut posts = self.posts.take().unwrap_or_default();
        for post in posts.values_mut() {
            post.strip_action_integrations();
        }
        self.posts = Some(posts);
    }

    /// Port of `(*PostList).ToJSON` (post_list.go:98).
    ///
    /// Strips a **copy**, so the receiver keeps its integrations — the opposite of
    /// [`Self::encode_json`]. Marshalled through [`go_json_marshal`] rather than
    /// `serde_json::to_string` because Go's `encoding/json` HTML-escapes; see [D-027].
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        let mut copy = self.clone();
        copy.strip_action_integrations();
        go_json_marshal(&copy)
    }

    /// Port of `(*PostList).EncodeJSON` (post_list.go:105).
    ///
    /// **Mutates the receiver**, and appends the newline Go's `json.Encoder.Encode` writes and
    /// `json.Marshal` does not.
    pub fn encode_json<W: std::io::Write>(&mut self, w: &mut W) -> Result<(), EncodeJsonError> {
        self.strip_action_integrations();
        let mut encoded = go_json_marshal(self)?;
        encoded.push('\n');
        w.write_all(encoded.as_bytes())?;
        Ok(())
    }

    /// Port of `(*PostList).MakeNonNil` (post_list.go:110).
    ///
    /// Materialises `order` and `posts` but **not** `burn_on_read_posts`, then recurses into
    /// every post.
    pub fn make_non_nil(&mut self) {
        self.order.get_or_insert_with(Vec::new);
        for post in self.posts.get_or_insert_with(PostMap::new).values_mut() {
            post.make_non_nil();
        }
    }

    /// Port of `(*PostList).AddOrder` (post_list.go:124).
    ///
    /// Does not deduplicate — that is [`Self::unique_order`]'s job — and does not require a
    /// matching post.
    pub fn add_order(&mut self, id: impl Into<String>) {
        self.order.get_or_insert_with(Vec::new).push(id.into());
    }

    /// Port of `(*PostList).AddPost` (post_list.go:132).
    ///
    /// A burn-on-read post is filed in `burn_on_read_posts` as well as `posts`. Go assigns into
    /// that map **without a nil check**, so it panics on any list that did not come from
    /// `NewPostList` — including every decoded one, since the field is `json:"-"`. We create the
    /// map instead; same call as [D-018], logged as [D-052].
    ///
    /// Go files the same pointer in both maps; ours are independent copies.
    pub fn add_post(&mut self, post: Post) {
        if post.post_type == POST_TYPE_BURN_ON_READ {
            self.burn_on_read_posts
                .get_or_insert_with(PostMap::new)
                .insert(post.id.clone(), post.clone());
        }
        self.posts
            .get_or_insert_with(PostMap::new)
            .insert(post.id.clone(), post);
    }

    /// Port of `(*PostList).UniqueOrder` (post_list.go:144).
    ///
    /// Keeps the **first** occurrence of each id, and always leaves `order` non-nil — Go builds
    /// a fresh `[]string{}` even when there was nothing to filter.
    pub fn unique_order(&mut self) {
        let mut seen = BTreeSet::new();
        let mut order = Vec::new();
        for id in self.order.iter().flatten() {
            if seen.insert(id.as_str()) {
                order.push(id.clone());
            }
        }
        self.order = Some(order);
    }

    /// Port of `(*PostList).Extend` (post_list.go:157).
    ///
    /// `other`'s posts win on an id collision, and its order entries are appended before the
    /// deduplication — so a colliding id keeps its **earlier** position.
    ///
    /// Go files `other`'s post pointers into the receiver, aliasing the two lists; we copy.
    /// Same class as [D-015].
    pub fn extend(&mut self, other: &PostList) {
        for post in other.posts.iter().flatten().map(|(_, post)| post) {
            self.add_post(post.clone());
        }
        for id in other.order.iter().flatten() {
            self.add_order(id.clone());
        }
        self.unique_order();
    }

    /// Port of `(*PostList).SortByCreateAt` (post_list.go:169).
    ///
    /// Sorts `order` by the referenced post's `create_at`, **descending**. An order id with no
    /// post sorts as `0` here and panics in Go, which dereferences a nil `*Post` in the
    /// comparator.
    ///
    /// Go uses `sort.Slice`, which is **not stable**, and `order` is on the wire — so two posts
    /// sharing a `create_at` can come out in a different order in the two servers once the list
    /// is long enough to leave Go's insertion-sort threshold. Measured and logged as [D-051];
    /// `sort_by_create_at_matches_go` asserts the agreement up to that point and
    /// `an_unstable_go_sort_scrambles_ties_above_twelve` asserts the divergence past it.
    pub fn sort_by_create_at(&mut self) {
        let Self { order, posts, .. } = self;
        let Some(order) = order.as_mut() else {
            return;
        };
        let create_at = |id: &String| {
            posts
                .as_ref()
                .and_then(|posts| posts.get(id))
                .map_or(0, |post| post.create_at)
        };
        order.sort_by_key(|id| std::cmp::Reverse(create_at(id)));
    }

    /// Port of `(*PostList).Etag` (post_list.go:175).
    ///
    /// Three components: the **first order entry** (which need not name a post, and is the empty
    /// string for an empty order), then the id and `update_at` of the post that wins a maximum
    /// over `(update_at, id)` seeded with `(0, "0")`.
    ///
    /// Unlike [`crate::channel_list::ChannelList::etag`] this does not depend on iteration
    /// order, which is what makes it safe over Go's randomised map iteration.
    pub fn etag(&self) -> String {
        let mut id = "0";
        let mut t = 0_i64;
        for post in self.posts.iter().flatten().map(|(_, post)| post) {
            if post.update_at > t || (post.update_at == t && post.id.as_str() > id) {
                t = post.update_at;
                id = &post.id;
            }
        }

        let order_id = self
            .order
            .as_ref()
            .and_then(|order| order.first())
            .map_or("", String::as_str);

        etag(&[&order_id, &id, &t])
    }

    /// Port of `(*PostList).IsChannelId` (post_list.go:197).
    ///
    /// Vacuously true for an empty list, including for a channel id no post could have.
    pub fn is_channel_id(&self, channel_id: &str) -> bool {
        self.posts
            .iter()
            .flatten()
            .all(|(_, post)| post.channel_id == channel_id)
    }

    /// Port of `(*PostList).BuildWranglerPostList` (post_list.go:207).
    ///
    /// **Mutates the receiver** before reading it — `UniqueOrder` then `SortByCreateAt` — so the
    /// caller's list is deduplicated and reordered as a side effect. That is why this takes
    /// `&mut self` rather than `&self`.
    ///
    /// The result is the posts **oldest first**, the distinct user ids in that same order, and
    /// the total file-id count. An order id with no post panics in Go (the nil `*Post` is
    /// dereferenced for `UserId`); we skip it. Logged with [D-052].
    pub fn build_wrangler_post_list(&mut self) -> WranglerPostList {
        self.unique_order();
        self.sort_by_create_at();

        let posts: Vec<&Post> = self.to_slice().into_iter().flatten().collect();
        let mut wpl = WranglerPostList::default();
        if posts.is_empty() {
            // "Something was sorted wrong or an empty PostList was provided."
            return wpl;
        }

        let mut seen = BTreeSet::new();
        let mut thread_user_ids = Vec::new();
        let mut ordered = Vec::with_capacity(posts.len());
        for post in posts.into_iter().rev() {
            if seen.insert(post.user_id.as_str()) {
                thread_user_ids.push(post.user_id.clone());
            }
            wpl.file_attachment_count += post.file_ids.as_ref().map_or(0, Vec::len) as i64;
            ordered.push(post.clone());
        }

        wpl.earlist_post_timestamp = ordered.first().map_or(0, |post| post.create_at);
        wpl.latest_post_timestamp = ordered.last().map_or(0, |post| post.create_at);
        wpl.posts = Some(ordered);
        wpl.thread_user_ids = Some(thread_user_ids);
        wpl
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn post(id: &str, create_at: i64) -> Post {
        Post {
            id: id.into(),
            create_at,
            update_at: create_at,
            channel_id: "c1".into(),
            user_id: "u1".into(),
            ..Default::default()
        }
    }

    #[test]
    fn new_is_not_default() {
        // The distinction reaches the client: `[]`/`{}` against `null`.
        assert_ne!(PostList::new(), PostList::default());
        assert_eq!(
            serde_json::to_string(&PostList::default()).unwrap(),
            r#"{"order":null,"posts":null,"next_post_id":"","prev_post_id":"","first_inaccessible_post_time":0}"#
        );
    }

    #[test]
    fn go_clone_materialises_the_nil_collections() {
        let cloned = PostList::default().go_clone();
        assert_eq!(cloned.order, Some(Vec::new()));
        assert_eq!(cloned.posts, Some(PostMap::new()));
        assert_eq!(cloned.burn_on_read_posts, Some(PostMap::new()));
        // Rust's own Clone does not, which is the whole reason the two are separate.
        assert_eq!(PostList::default().clone(), PostList::default());
    }

    #[test]
    fn to_slice_follows_order_and_reports_a_missing_post() {
        let list = PostList {
            order: Some(vec!["p1".into(), "ghost".into()]),
            posts: Some(PostMap::from([("p1".into(), post("p1", 100))])),
            ..Default::default()
        };
        let slice = list.to_slice();
        assert_eq!(slice.len(), 2);
        assert_eq!(slice[0].unwrap().id, "p1");
        assert!(slice[1].is_none());
    }

    #[test]
    fn a_post_outside_order_is_on_the_wire_and_out_of_the_slice() {
        let list = PostList {
            order: Some(Vec::new()),
            posts: Some(PostMap::from([("p1".into(), post("p1", 100))])),
            ..Default::default()
        };
        assert!(list.to_slice().is_empty());
        assert!(serde_json::to_string(&list).unwrap().contains(r#""p1""#));
    }

    #[test]
    fn unique_order_keeps_the_first_occurrence_and_never_leaves_nil() {
        let mut list = PostList {
            order: Some(vec![
                "a".into(),
                "b".into(),
                "a".into(),
                "c".into(),
                "b".into(),
            ]),
            ..Default::default()
        };
        list.unique_order();
        assert_eq!(list.order, Some(vec!["a".into(), "b".into(), "c".into()]));

        let mut empty = PostList::default();
        empty.unique_order();
        assert_eq!(empty.order, Some(Vec::new()));
    }

    #[test]
    fn add_post_files_a_burn_on_read_post_twice() {
        let mut list = PostList::default();
        let mut burn = post("b1", 5);
        burn.post_type = POST_TYPE_BURN_ON_READ.into();
        list.add_post(burn);
        list.add_post(post("p1", 100));

        assert_eq!(list.posts.as_ref().unwrap().len(), 2);
        let burned = list.burn_on_read_posts.as_ref().unwrap();
        assert_eq!(burned.len(), 1);
        assert!(burned.contains_key("b1"));
    }

    #[test]
    fn etag_is_a_maximum_over_update_at_then_id() {
        let list = PostList {
            order: Some(vec!["p1".into()]),
            posts: Some(PostMap::from([
                ("p1".into(), post("p1", 100)),
                ("p2".into(), post("p2", 100)),
            ])),
            ..Default::default()
        };
        // Equal update_at, so the larger id wins — regardless of which is first in `order`.
        assert!(list.etag().ends_with(".p1.p2.100"));
    }

    #[test]
    fn make_non_nil_leaves_the_burn_map_alone() {
        let mut list = PostList::default();
        list.make_non_nil();
        assert!(list.order.is_some());
        assert!(list.posts.is_some());
        assert!(list.burn_on_read_posts.is_none());
    }

    #[test]
    fn is_channel_id_is_vacuously_true_when_empty() {
        assert!(PostList::default().is_channel_id("anything"));
    }

    #[test]
    fn build_wrangler_post_list_reorders_its_receiver() {
        let mut list = PostList {
            order: Some(vec!["a".into(), "a".into(), "b".into()]),
            posts: Some(PostMap::from([
                ("a".into(), post("a", 100)),
                ("b".into(), post("b", 300)),
            ])),
            ..Default::default()
        };
        let wpl = list.build_wrangler_post_list();

        // The receiver is deduplicated and sorted newest-first...
        assert_eq!(list.order, Some(vec!["b".into(), "a".into()]));
        // ...while the wrangler list is oldest-first.
        assert_eq!(wpl.root_post().unwrap().id, "a");
        assert_eq!(wpl.earlist_post_timestamp, 100);
        assert_eq!(wpl.latest_post_timestamp, 300);
    }
}

/// Parity tests driven by `fixtures/behaviour_post_list.json` — Go's own answers.
#[cfg(test)]
mod go_parity {
    use super::*;
    use serde_json::Value;

    fn oracle() -> Value {
        serde_json::from_str(include_str!("../../../fixtures/behaviour_post_list.json")).unwrap()
    }

    fn section<'a>(oracle: &'a Value, key: &str) -> &'a [Value] {
        let cases = oracle[key].as_array().unwrap_or_else(|| panic!("{key}"));
        assert!(!cases.is_empty(), "{key} is empty");
        cases
    }

    fn name(case: &Value) -> &str {
        case["name"].as_str().unwrap()
    }

    fn panicked(case: &Value) -> bool {
        case["panicked"].as_bool().unwrap()
    }

    fn list_from(case: &Value, key: &str) -> PostList {
        serde_json::from_str(case[key].as_str().unwrap()).unwrap()
    }

    /// Byte-for-byte unless the answer carries a **rewritten** attachment list, where Go's struct
    /// field order and our sorted `serde_json::Map` disagree — see [D-048]. Same helper as
    /// `post::go_parity`, and the same carve-out.
    fn assert_json_matches_go(what: &str, ours: &str, want: &str) {
        if want.contains("\"attachments\":[") {
            let ours: Value = serde_json::from_str(ours).unwrap();
            let want: Value = serde_json::from_str(want).unwrap();
            assert_eq!(ours, want, "{what}");
        } else {
            assert_eq!(ours, want, "{what}");
        }
    }

    /// Asserts a whole list against a `dumpPostList` record: the exact bytes plus the nil-ness of
    /// all three collections, which the JSON alone cannot show for `burn_on_read_posts`.
    fn assert_dump(actual: &PostList, expected: &Value, what: &str) {
        assert_json_matches_go(
            &format!("{what}: bytes"),
            &go_json_marshal(actual).unwrap(),
            expected["json"].as_str().unwrap(),
        );
        assert_eq!(
            actual.order.is_none(),
            expected["order_nil"].as_bool().unwrap(),
            "{what}: order nil-ness"
        );
        assert_eq!(
            actual.posts.is_none(),
            expected["posts_nil"].as_bool().unwrap(),
            "{what}: posts nil-ness"
        );
        assert_eq!(
            actual.burn_on_read_posts.is_none(),
            expected["burn_nil"].as_bool().unwrap(),
            "{what}: burn_on_read_posts nil-ness"
        );
        assert_eq!(
            go_json_marshal(&actual.burn_on_read_posts).unwrap(),
            expected["burn"].as_str().unwrap(),
            "{what}: burn_on_read_posts"
        );
    }

    /// The one corpus document we cannot decode at all: Go's `map[string]*Post` accepts a nil
    /// value and our `BTreeMap<String, Post>` does not. Asserted once, in
    /// [`the_wire_format_matches_go`], and skipped everywhere else. See [D-033].
    const UNDECODABLE: &str = "nil_post_in_map";

    /// Runs the shared corpus through a mutating method, skipping the cases Go crashes on.
    fn assert_mutation(key: &str, apply: impl Fn(&mut PostList)) {
        let oracle = oracle();
        for case in section(&oracle, key) {
            let what = format!("{key}({})", name(case));
            if panicked(case) || name(case) == UNDECODABLE {
                continue;
            }
            let mut list = list_from(case, "in");
            apply(&mut list);
            assert_dump(&list, &case["out"], &what);
        }
    }

    #[test]
    fn the_fixture_round_trips() {
        let fixture: Value =
            serde_json::from_str(include_str!("../../../fixtures/post_list.json")).unwrap();
        let list: PostList = serde_json::from_value(fixture.clone()).unwrap();
        assert_eq!(serde_json::to_value(&list).unwrap(), fixture);
    }

    #[test]
    fn the_wire_format_matches_go() {
        let oracle = oracle();
        for case in section(&oracle, "wire") {
            let doc = case["in"].as_str().unwrap();
            let what = name(case);

            // Go's map[string]*Post accepts a nil value and we cannot — asserted rather than
            // skipped, so [D-033] cannot rot silently.
            if what == UNDECODABLE {
                assert!(
                    serde_json::from_str::<PostList>(doc).is_err(),
                    "a nil post is expected to fail the whole decode"
                );
                continue;
            }

            let list: PostList = serde_json::from_str(doc).unwrap();
            assert_eq!(
                go_json_marshal(&list).unwrap(),
                case["out"].as_str().unwrap(),
                "wire({what})"
            );
            assert_eq!(
                list.order.is_none(),
                case["order_nil"].as_bool().unwrap(),
                "wire({what}): order nil-ness"
            );
            assert_eq!(
                list.posts.is_none(),
                case["posts_nil"].as_bool().unwrap(),
                "wire({what}): posts nil-ness"
            );
            assert!(
                case["burn_nil"].as_bool().unwrap(),
                "burn_on_read_posts is json:\"-\" and must never survive a decode"
            );
            assert!(list.burn_on_read_posts.is_none(), "wire({what}): burn");
        }
    }

    #[test]
    fn new_matches_go() {
        assert_dump(&PostList::new(), &oracle()["new"], "NewPostList()");
    }

    #[test]
    fn clone_matches_go() {
        let oracle = oracle();
        for case in section(&oracle, "clone") {
            let what = format!("Clone({})", name(case));
            if panicked(case) {
                // Go's Clone dereferences a nil *Post; we never decode that document at all.
                assert!(
                    serde_json::from_str::<PostList>(case["in"].as_str().unwrap()).is_err(),
                    "{what}"
                );
                continue;
            }
            let list = list_from(case, "in");
            assert_dump(&list.go_clone(), &case["out"], &what);

            // The posts are deep copies in both languages...
            if let Some(aliased) = case["posts_aliased"].as_bool() {
                assert!(!aliased, "{what}: Go stopped deep-copying its posts");
            }
            // ...and HasNext is a shared pointer in Go and an owned value here.
            if let Some(aliased) = case["has_next_aliased"].as_bool() {
                assert!(aliased, "{what}: Go stopped aliasing HasNext");
                let mut copy = list.go_clone();
                copy.has_next = copy.has_next.map(|has_next| !has_next);
                assert_ne!(copy.has_next, list.has_next, "{what}: ours must not alias");
            }
        }
    }

    #[test]
    fn to_slice_matches_go() {
        let oracle = oracle();
        for case in section(&oracle, "to_slice") {
            let what = format!("ToSlice({})", name(case));
            if name(case) == UNDECODABLE {
                continue; // the document does not decode at all — see the wire test.
            }
            let list = list_from(case, "in");
            let slice = list.to_slice();
            assert_eq!(
                slice.len(),
                case["count"].as_u64().unwrap() as usize,
                "{what}: length"
            );

            // Go marshals the slice, so nil elements show up as `null`. Rebuild that.
            let rendered: Vec<Value> = slice
                .iter()
                .map(|post| match post {
                    Some(post) => serde_json::to_value(post).unwrap(),
                    None => Value::Null,
                })
                .collect();
            let expected: Value =
                serde_json::from_str(case["out"].as_str().unwrap()).unwrap_or(Value::Null);
            let expected = if expected.is_null() {
                Vec::new()
            } else {
                expected.as_array().unwrap().clone()
            };
            assert_eq!(rendered, expected, "{what}: elements");
        }
    }

    #[test]
    fn strip_action_integrations_matches_go() {
        assert_mutation(
            "strip_action_integrations",
            PostList::strip_action_integrations,
        );
    }

    #[test]
    fn make_non_nil_matches_go() {
        assert_mutation("make_non_nil", PostList::make_non_nil);
    }

    #[test]
    fn unique_order_matches_go() {
        assert_mutation("unique_order", PostList::unique_order);
    }

    #[test]
    fn add_order_matches_go() {
        let oracle = oracle();
        for case in section(&oracle, "add_order") {
            let what = format!("AddOrder({})", name(case));
            if panicked(case) || name(case) == UNDECODABLE {
                continue;
            }
            let mut list = list_from(case, "in");
            list.add_order(case["id"].as_str().unwrap());
            // The `twice` case adds the same id a second time; every other adds it once.
            if name(case) == "twice" {
                list.add_order(case["id"].as_str().unwrap());
            }
            assert_dump(&list, &case["out"], &what);
        }
    }

    #[test]
    fn to_json_matches_go() {
        let oracle = oracle();
        for case in section(&oracle, "to_json") {
            let what = format!("ToJSON({})", name(case));
            if panicked(case) || name(case) == UNDECODABLE {
                continue;
            }
            let list = list_from(case, "in");
            assert_json_matches_go(
                &what,
                &list.to_json().unwrap(),
                case["out"].as_str().unwrap(),
            );
            assert!(!case["err"].as_bool().unwrap(), "{what}: Go errored");
            // The receiver keeps its integrations — this is the half that separates ToJSON from
            // EncodeJSON, and getting it the wrong way round would destroy an integration.
            assert_dump(&list, &case["receiver_after"], &format!("{what}: receiver"));
        }
    }

    #[test]
    fn encode_json_matches_go() {
        let oracle = oracle();
        for case in section(&oracle, "encode_json") {
            let what = format!("EncodeJSON({})", name(case));
            if panicked(case) || name(case) == UNDECODABLE {
                continue;
            }
            let mut list = list_from(case, "in");
            let mut buf = Vec::new();
            list.encode_json(&mut buf).unwrap();
            assert_json_matches_go(
                &what,
                &String::from_utf8(buf).unwrap(),
                case["out"].as_str().unwrap(),
            );
            // ...and here the receiver *is* stripped.
            assert_dump(&list, &case["receiver_after"], &format!("{what}: receiver"));
        }
    }

    #[test]
    fn add_post_matches_go() {
        let oracle = oracle();
        for case in section(&oracle, "add_post") {
            let what = format!("AddPost({})", name(case));
            let post: Post = serde_json::from_str(case["post"].as_str().unwrap()).unwrap();

            let mut list = if case["in"].as_str().unwrap() == "NewPostList()" {
                PostList::new()
            } else {
                list_from(case, "in")
            };

            if panicked(case) {
                // Go assigns into a nil BurnOnReadPosts and crashes; we create the map. [D-052]
                assert!(
                    list.burn_on_read_posts.is_none(),
                    "{what}: Go only panics when the map is nil"
                );
                assert_eq!(post.post_type, POST_TYPE_BURN_ON_READ, "{what}");
                list.add_post(post);
                assert!(
                    list.burn_on_read_posts
                        .as_ref()
                        .is_some_and(|burn| burn.len() == 1),
                    "{what}: we file the post where Go crashes"
                );
                continue;
            }

            list.add_post(post);
            assert_dump(&list, &case["out"], &what);
        }
    }

    #[test]
    fn extend_matches_go() {
        let oracle = oracle();
        for case in section(&oracle, "extend") {
            let what = format!("Extend({})", name(case));
            if panicked(case) {
                continue;
            }
            let mut a = list_from(case, "a");
            let b = list_from(case, "b");
            a.extend(&b);
            assert_dump(&a, &case["out"], &what);
        }
    }

    /// The tie cases above twelve elements are excluded and asserted separately — see [D-051].
    const UNSTABLE_SORT_CASES: [&str; 1] = ["tie_interleaved_twenty"];

    #[test]
    fn sort_by_create_at_matches_go() {
        let oracle = oracle();
        for case in section(&oracle, "sort_by_create_at") {
            let what = format!("SortByCreateAt({})", name(case));
            if panicked(case) || UNSTABLE_SORT_CASES.contains(&name(case)) {
                continue;
            }
            let mut list = list_from(case, "in");
            list.sort_by_create_at();
            assert_dump(&list, &case["out"], &what);
        }
    }

    /// Go's `sort.Slice` is not stable, and `order` is on the wire. Below thirteen elements Go
    /// runs insertion sort and agrees with a stable sort on every measured input; above it, an
    /// interleaved tie pattern comes out scrambled. Asserted rather than skipped so the
    /// divergence cannot rot — see [D-051].
    #[test]
    fn an_unstable_go_sort_scrambles_ties_above_twelve() {
        let oracle = oracle();
        let case = section(&oracle, "sort_by_create_at")
            .iter()
            .find(|case| name(case) == "tie_interleaved_twenty")
            .expect("tie_interleaved_twenty");

        let mut list = list_from(case, "in");
        list.sort_by_create_at();
        let ours = list.order.clone().unwrap();
        let gos: Vec<String> = serde_json::from_value(case["out_order"].clone()).unwrap();

        // Both are correct sorts: the create_at sequence is identical...
        let create_at = |ids: &[String]| -> Vec<i64> {
            ids.iter()
                .map(|id| list.posts.as_ref().unwrap()[id].create_at)
                .collect()
        };
        assert_eq!(
            create_at(&ours),
            create_at(&gos),
            "the orderings must agree"
        );
        // ...and the permutation is not, because Go's sort is unstable and ours is stable.
        assert_ne!(
            ours, gos,
            "Go's sort.Slice became stable; [D-051] can be closed"
        );
    }

    #[test]
    fn etag_matches_go() {
        let oracle = oracle();
        for case in section(&oracle, "etag") {
            let what = format!("Etag({})", name(case));
            if panicked(case) {
                continue;
            }
            assert_eq!(
                list_from(case, "in").etag(),
                case["out"].as_str().unwrap(),
                "{what}"
            );
        }
    }

    #[test]
    fn is_channel_id_matches_go() {
        let oracle = oracle();
        for case in section(&oracle, "is_channel_id") {
            let channel_id = case["channel_id"].as_str().unwrap();
            assert_eq!(
                list_from(case, "in").is_channel_id(channel_id),
                case["out"].as_bool().unwrap(),
                "IsChannelId({}, {channel_id})",
                name(case)
            );
        }
    }

    #[test]
    fn build_wrangler_post_list_matches_go() {
        let oracle = oracle();
        for case in section(&oracle, "build_wrangler_post_list") {
            let what = format!("BuildWranglerPostList({})", name(case));
            if name(case) == UNDECODABLE {
                continue;
            }
            let mut list = list_from(case, "in");

            if panicked(case) {
                // An order id with no post: Go dereferences the nil element, we skip it. [D-052]
                let wpl = list.build_wrangler_post_list();
                assert_eq!(wpl.num_posts(), 0, "{what}: we return an empty list");
                continue;
            }

            let wpl = list.build_wrangler_post_list();
            assert_eq!(
                go_json_marshal(&wpl).unwrap(),
                case["out"].as_str().unwrap(),
                "{what}"
            );
            assert_eq!(
                wpl.num_posts(),
                case["num_posts"].as_u64().unwrap() as usize,
                "{what}: NumPosts"
            );
            assert_eq!(
                wpl.contains_file_attachments(),
                case["contains_file_attachments"].as_bool().unwrap(),
                "{what}: ContainsFileAttachments"
            );
            assert_eq!(
                wpl.root_post().is_none(),
                case["root_post_nil"].as_bool().unwrap(),
                "{what}: RootPost"
            );
            // The receiver is deduplicated and re-sorted as a side effect.
            assert_dump(&list, &case["list_after"], &format!("{what}: receiver"));
        }
    }

    #[test]
    fn for_plugin_matches_go() {
        let oracle = oracle();
        for case in section(&oracle, "for_plugin") {
            let what = format!("PostList::ForPlugin({})", name(case));
            if panicked(case) {
                continue;
            }
            assert_dump(&list_from(case, "in").for_plugin(), &case["out"], &what);
        }
    }

    #[test]
    fn post_for_plugin_matches_go() {
        let oracle = oracle();
        for case in section(&oracle, "post_for_plugin") {
            let what = format!("Post::ForPlugin({})", name(case));
            if panicked(case) {
                continue;
            }
            let post: Post = serde_json::from_str(case["in"].as_str().unwrap()).unwrap();
            assert_eq!(
                go_json_marshal(&post.for_plugin()).unwrap(),
                case["out"].as_str().unwrap(),
                "{what}"
            );
        }
    }
}
