//! Port of `model/channel_sidebar.go`.
//!
//! Ported for the read side of `BaseRoutes.ChannelCategories` (api4/api.go:231): the three
//! `GET`s under `/api/v4/users/{user_id}/teams/{team_id}/channels/categories`. The webapp calls
//! the collection one on every team load, so this is the sidebar a real client renders.
//!
//! # `null` is a reachable value for three of these arrays, and it is not `[]`
//!
//! `SidebarCategoryWithChannels.Channels`, `OrderedSidebarCategories.Categories` and `.Order`
//! all carry a plain `json:` tag with **no `omitempty`**, so a nil Go slice marshals as `null`
//! while an empty one marshals as `[]`. The distinction is on the wire and a client can see it,
//! so all three are `Option<Vec<_>>` here rather than `Vec<_>` — the same reasoning as
//! `ChannelsWithCount.channels`. `fixtures/behaviour_sidebar_category.json` § `nil_shapes`
//! records both marshallings side by side.
//!
//! The three ported read routes never produce a `null`: Go's store builds `make([]string, 0)`
//! and `make(SidebarCategoriesWithChannels, 0)` before it fills them, so `[]` is what
//! `mm-store`'s port emits too. Modelling it as `Vec` would have been correct for those routes
//! and silently wrong for the write routes that are still forwarded.

use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::utils::is_valid_id;

// ---------------------------------------------------------------------------
// Category types and sort modes.
//
// Go declares `SidebarCategoryType` and `SidebarCategorySorting` as named string types with a
// `MarshalJSON` that does nothing but `json.Marshal(string(t))` — so they are plain strings on
// the wire, and modelled as `String` here the way `Channel.Type` is. Values emitted from Go in
// `fixtures/behaviour_sidebar_category.json` § `constants`, not transcribed.
// ---------------------------------------------------------------------------

/// `model.SidebarCategoryChannels` (channel_sidebar.go:17).
pub const SIDEBAR_CATEGORY_CHANNELS: &str = "channels";
/// `model.SidebarCategoryDirectMessages` (channel_sidebar.go:18).
pub const SIDEBAR_CATEGORY_DIRECT_MESSAGES: &str = "direct_messages";
/// `model.SidebarCategoryFavorites` (channel_sidebar.go:19).
pub const SIDEBAR_CATEGORY_FAVORITES: &str = "favorites";
/// `model.SidebarCategoryCustom` (channel_sidebar.go:20).
pub const SIDEBAR_CATEGORY_CUSTOM: &str = "custom";
/// `model.SidebarCategoryManaged` (channel_sidebar.go:21).
pub const SIDEBAR_CATEGORY_MANAGED: &str = "managed";

/// `model.MinimalSidebarSortDistance` (channel_sidebar.go:23).
pub const MINIMAL_SIDEBAR_SORT_DISTANCE: i64 = 10;
/// `model.DefaultSidebarSortOrderFavorites` (channel_sidebar.go:25).
pub const DEFAULT_SIDEBAR_SORT_ORDER_FAVORITES: i64 = 0;
/// `model.DefaultSidebarSortOrderChannels` (channel_sidebar.go:26).
pub const DEFAULT_SIDEBAR_SORT_ORDER_CHANNELS: i64 =
    DEFAULT_SIDEBAR_SORT_ORDER_FAVORITES + MINIMAL_SIDEBAR_SORT_DISTANCE;
/// `model.DefaultSidebarSortOrderDMs` (channel_sidebar.go:27).
pub const DEFAULT_SIDEBAR_SORT_ORDER_DMS: i64 =
    DEFAULT_SIDEBAR_SORT_ORDER_CHANNELS + MINIMAL_SIDEBAR_SORT_DISTANCE;

/// `model.SidebarCategorySortDefault` (channel_sidebar.go:30) — **the empty string**, which is
/// a valid `sorting` value and not a missing one. Every category Go creates except Direct
/// Messages carries it.
pub const SIDEBAR_CATEGORY_SORT_DEFAULT: &str = "";
/// `model.SidebarCategorySortManual` (channel_sidebar.go:32).
pub const SIDEBAR_CATEGORY_SORT_MANUAL: &str = "manual";
/// `model.SidebarCategorySortRecent` (channel_sidebar.go:34) — the default for DMs.
pub const SIDEBAR_CATEGORY_SORT_RECENT: &str = "recent";
/// `model.SidebarCategorySortAlphabetical` (channel_sidebar.go:36) — note the wire value is
/// `alpha`, not `alphabetical`.
pub const SIDEBAR_CATEGORY_SORT_ALPHABETICAL: &str = "alpha";

/// `model.ManagedCategoryPropertyGroupName` (channel_sidebar.go:38).
pub const MANAGED_CATEGORY_PROPERTY_GROUP_NAME: &str = "managed_channel_categories";
/// `model.ManagedCategoryPropertyFieldName` (channel_sidebar.go:39).
pub const MANAGED_CATEGORY_PROPERTY_FIELD_NAME: &str = "category_name";

// ---------------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------------

/// Port of `model.SidebarCategory` (channel_sidebar.go:43) — one row of `SidebarCategories`.
///
/// Every column but `Id` is nullable in the schema Go migrates, and Go scans them into plain
/// `string`/`int64`/`bool`, so a NULL fails the whole query rather than defaulting. The store
/// port reproduces that; see `mm_store::sidebar_category_store`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SidebarCategory {
    #[serde(rename = "id")]
    pub id: String,

    #[serde(rename = "user_id")]
    pub user_id: String,

    #[serde(rename = "team_id")]
    pub team_id: String,

    #[serde(rename = "sort_order")]
    pub sort_order: i64,

    /// One of the `SIDEBAR_CATEGORY_SORT_*` constants. `""` is the default and is not "unset".
    #[serde(rename = "sorting")]
    pub sorting: String,

    /// One of the `SIDEBAR_CATEGORY_*` type constants.
    #[serde(rename = "type")]
    pub category_type: String,

    #[serde(rename = "display_name")]
    pub display_name: String,

    #[serde(rename = "muted")]
    pub muted: bool,

    #[serde(rename = "collapsed")]
    pub collapsed: bool,
}

/// Port of `model.SidebarCategoryWithChannels` (channel_sidebar.go:56).
///
/// Go **embeds** `SidebarCategory`, so its nine keys are inlined ahead of `channel_ids` rather
/// than nested — `#[serde(flatten)]` on a field declared first reproduces both the inlining and
/// the key order.
///
/// The Go field is named `Channels` but its tag is `channel_ids`; the name here follows the
/// wire, because that is the half a client sees.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SidebarCategoryWithChannels {
    #[serde(flatten)]
    pub category: SidebarCategory,

    /// `null` when Go's slice is nil, `[]` when it is empty — see the module docs.
    #[serde(rename = "channel_ids")]
    pub channel_ids: Option<Vec<String>>,
}

impl SidebarCategoryWithChannels {
    /// Port of `(SidebarCategoryWithChannels).ChannelIds` (channel_sidebar.go:61).
    ///
    /// Returns the field itself, nil-ness included — it is not a copy, not a sorted view, and
    /// not an empty-slice normalisation. Pinned in the corpus because a port that "helpfully"
    /// returned `&[]` for a nil slice would erase the `null` the wire distinguishes.
    #[must_use]
    pub fn channel_ids(&self) -> Option<&[String]> {
        self.channel_ids.as_deref()
    }
}

/// Port of `model.OrderedSidebarCategories` (channel_sidebar.go:69) — the body of
/// `GET .../channels/categories`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrderedSidebarCategories {
    /// Go's `SidebarCategoriesWithChannels`, a named `[]*SidebarCategoryWithChannels` with no
    /// methods of its own.
    #[serde(rename = "categories")]
    pub categories: Option<Vec<SidebarCategoryWithChannels>>,

    /// Go's `SidebarCategoryOrder`, a named `[]string` with no methods of its own. The category
    /// ids again, in the same order as `categories` — Go appends to both in one pass.
    #[serde(rename = "order")]
    pub order: Option<Vec<String>>,
}

/// Port of `model.SidebarChannel` (channel_sidebar.go:74) — one row of `SidebarChannels`.
///
/// `SortOrder` is tagged `json:"-"`, so it is **absent** from the wire rather than emitted as
/// zero; `#[serde(skip)]` matches, and `fixtures/sidebar_channel.json` has only three keys.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SidebarChannel {
    #[serde(rename = "channel_id")]
    pub channel_id: String,

    #[serde(rename = "user_id")]
    pub user_id: String,

    #[serde(rename = "category_id")]
    pub category_id: String,

    #[serde(skip)]
    pub sort_order: i64,
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

/// `categoryIdPattern` (channel_sidebar.go:82).
///
/// Compiled from the identical source text. Go's `MatchString` and Rust's `is_match` are both
/// **unanchored**, which is the whole subtlety — see [`is_valid_category_id`].
static CATEGORY_ID_PATTERN: LazyLock<Option<Regex>> = LazyLock::new(|| {
    Regex::new("(favorites|channels|direct_messages)_[a-z0-9]{26}_[a-z0-9]{26}").ok()
});

/// Port of `model.IsValidCategoryId` (channel_sidebar.go:84), the check behind
/// `Context.RequireCategoryId` (web/context.go:333).
///
/// Two branches that disagree with each other, both pinned against Go in
/// `fixtures/behaviour_sidebar_category.json` § `category_ids`:
///
/// 1. **A real 26-character id.** [`is_valid_id`] accepts Go's unicode letter and number
///    classes, so `ABCDEFGHIJKLMNOPQRSTUVWXYZ` passes here.
/// 2. **A default category's deterministic id**, `{type}_{userId}_{teamId}`, built by
///    `createInitialSidebarCategoriesT`. Its two halves are `[a-z0-9]{26}` — *lower case only*,
///    unlike branch 1 — so the same upper-case characters inside this shape are rejected.
///
/// **The pattern is not anchored.** `regexp.MatchString` asks whether the pattern occurs
/// anywhere, so `zzfavorites_<26>_<26>!!` is accepted, and so is a *27*-character second half
/// (the pattern takes its 26 and stops). Wrapping the translation in `^…$` is the obvious edit
/// and it rejects inputs Go admits; the corpus fails immediately if anyone makes it.
///
/// `custom` and `managed` are category types but are deliberately absent from the alternation —
/// a custom category always carries a real id, so it takes branch 1.
#[must_use]
pub fn is_valid_category_id(value: &str) -> bool {
    // Category IDs can either be regular IDs
    if is_valid_id(value) {
        return true;
    }

    // Or default categories can follow the pattern {type}_{userID}_{teamID}
    CATEGORY_ID_PATTERN
        .as_ref()
        .is_some_and(|pattern| pattern.is_match(value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn oracle() -> Value {
        serde_json::from_str(include_str!(
            "../../../fixtures/behaviour_sidebar_category.json"
        ))
        .expect("behaviour_sidebar_category.json parses")
    }

    /// Round-trip every registry fixture: deserialize, re-serialize, compare the value graphs.
    #[test]
    fn sidebar_category_json_round_trip() {
        let raw = include_str!("../../../fixtures/sidebar_category.json");
        let parsed: SidebarCategory = serde_json::from_str(raw).expect("parses");
        let before: Value = serde_json::from_str(raw).expect("parses as value");
        assert_eq!(serde_json::to_value(&parsed).expect("serialises"), before);

        // And the two fields whose Rust names differ from their JSON keys.
        assert_eq!(parsed.category_type, before["type"]);
        assert_eq!(
            parsed.sort_order,
            before["sort_order"].as_i64().expect("i64")
        );
    }

    #[test]
    fn sidebar_category_with_channels_json_round_trip() {
        let raw = include_str!("../../../fixtures/sidebar_category_with_channels.json");
        let parsed: SidebarCategoryWithChannels = serde_json::from_str(raw).expect("parses");
        let before: Value = serde_json::from_str(raw).expect("parses as value");
        assert_eq!(serde_json::to_value(&parsed).expect("serialises"), before);

        // The embedded struct's keys are inlined, not nested under a `category` object.
        assert!(before.get("category").is_none());
        assert_eq!(parsed.category.id, before["id"]);
    }

    #[test]
    fn ordered_sidebar_categories_json_round_trip() {
        let raw = include_str!("../../../fixtures/ordered_sidebar_categories.json");
        let parsed: OrderedSidebarCategories = serde_json::from_str(raw).expect("parses");
        let before: Value = serde_json::from_str(raw).expect("parses as value");
        assert_eq!(serde_json::to_value(&parsed).expect("serialises"), before);
    }

    #[test]
    fn sidebar_channel_json_round_trip() {
        let raw = include_str!("../../../fixtures/sidebar_channel.json");
        let parsed: SidebarChannel = serde_json::from_str(raw).expect("parses");
        let before: Value = serde_json::from_str(raw).expect("parses as value");
        assert_eq!(serde_json::to_value(&parsed).expect("serialises"), before);
    }

    /// `SortOrder` is `json:"-"`: absent, not zero. The registry fixture is fully populated by
    /// reflection, so a fourth key here would mean the tag was dropped.
    #[test]
    fn sidebar_channel_omits_sort_order_entirely() {
        let raw = include_str!("../../../fixtures/sidebar_channel.json");
        let before: Value = serde_json::from_str(raw).expect("parses as value");
        // `serde_json::Value`'s map is a `BTreeMap` without the `preserve_order` feature, so this
        // is a *set* comparison. Wire order is asserted on the serialised text, below and in
        // `embedded_keys_precede_channel_ids_in_gos_order`.
        let mut keys: Vec<&str> = before
            .as_object()
            .expect("object")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(keys, vec!["category_id", "channel_id", "user_id"]);

        let channel = SidebarChannel {
            sort_order: 42,
            ..SidebarChannel::default()
        };
        let wire = serde_json::to_value(&channel).expect("serialises");
        assert!(wire.get("sort_order").is_none(), "sort_order is json:\"-\"");
    }

    /// The embedded fields come first, in Go's declaration order, and `channel_ids` last. Key
    /// order is wire format here: the parity suite compares bytes.
    #[test]
    fn embedded_keys_precede_channel_ids_in_gos_order() {
        let raw = include_str!("../../../fixtures/sidebar_category_with_channels.json");
        let parsed: SidebarCategoryWithChannels = serde_json::from_str(raw).expect("parses");
        let text = serde_json::to_string(&parsed).expect("serialises");

        let keys: Vec<&str> = [
            "\"id\"",
            "\"user_id\"",
            "\"team_id\"",
            "\"sort_order\"",
            "\"sorting\"",
            "\"type\"",
            "\"display_name\"",
            "\"muted\"",
            "\"collapsed\"",
            "\"channel_ids\"",
        ]
        .to_vec();

        let mut cursor = 0usize;
        for key in keys {
            let at = text[cursor..]
                .find(key)
                .unwrap_or_else(|| panic!("{key} missing from {text}"));
            cursor += at + key.len();
        }
    }

    // -----------------------------------------------------------------------
    // go_parity: every assertion below is against a value Go computed.
    // -----------------------------------------------------------------------

    mod go_parity {
        use super::*;

        #[test]
        fn constants_match_go() {
            let c = &oracle()["constants"];
            assert_eq!(c["SidebarCategoryChannels"], SIDEBAR_CATEGORY_CHANNELS);
            assert_eq!(
                c["SidebarCategoryDirectMessages"],
                SIDEBAR_CATEGORY_DIRECT_MESSAGES
            );
            assert_eq!(c["SidebarCategoryFavorites"], SIDEBAR_CATEGORY_FAVORITES);
            assert_eq!(c["SidebarCategoryCustom"], SIDEBAR_CATEGORY_CUSTOM);
            assert_eq!(c["SidebarCategoryManaged"], SIDEBAR_CATEGORY_MANAGED);
            assert_eq!(
                c["MinimalSidebarSortDistance"].as_i64(),
                Some(MINIMAL_SIDEBAR_SORT_DISTANCE)
            );
            assert_eq!(
                c["DefaultSidebarSortOrderFavorites"].as_i64(),
                Some(DEFAULT_SIDEBAR_SORT_ORDER_FAVORITES)
            );
            assert_eq!(
                c["DefaultSidebarSortOrderChannels"].as_i64(),
                Some(DEFAULT_SIDEBAR_SORT_ORDER_CHANNELS)
            );
            assert_eq!(
                c["DefaultSidebarSortOrderDMs"].as_i64(),
                Some(DEFAULT_SIDEBAR_SORT_ORDER_DMS)
            );
            assert_eq!(
                c["SidebarCategorySortDefault"],
                SIDEBAR_CATEGORY_SORT_DEFAULT
            );
            assert_eq!(c["SidebarCategorySortManual"], SIDEBAR_CATEGORY_SORT_MANUAL);
            assert_eq!(c["SidebarCategorySortRecent"], SIDEBAR_CATEGORY_SORT_RECENT);
            assert_eq!(
                c["SidebarCategorySortAlphabetical"],
                SIDEBAR_CATEGORY_SORT_ALPHABETICAL
            );
            assert_eq!(
                c["ManagedCategoryPropertyGroupName"],
                MANAGED_CATEGORY_PROPERTY_GROUP_NAME
            );
            assert_eq!(
                c["ManagedCategoryPropertyFieldName"],
                MANAGED_CATEGORY_PROPERTY_FIELD_NAME
            );
        }

        /// Every input Go was asked about, with Go's answer. Includes the unanchored cases and
        /// the case-class disagreement between the two branches.
        #[test]
        fn is_valid_category_id_matches_go_on_every_case() {
            let oracle = oracle();
            let cases = oracle["category_ids"].as_array().expect("an array");
            assert!(cases.len() >= 20, "the corpus must not shrink silently");

            for case in cases {
                let input = case["input"].as_str().expect("a string");
                let want = case["is_valid_category_id"].as_bool().expect("a bool");
                assert_eq!(
                    is_valid_category_id(input),
                    want,
                    "is_valid_category_id({input:?})"
                );

                // The first branch, separately: a difference here localises a regression to
                // `is_valid_id` rather than to the pattern.
                let want_id = case["is_valid_id"].as_bool().expect("a bool");
                assert_eq!(is_valid_id(input), want_id, "is_valid_id({input:?})");
            }
        }

        /// The unanchored cases called out by name, so a reader sees them without reading JSON.
        #[test]
        fn the_pattern_is_unanchored_like_gos() {
            let oracle = oracle();
            let cases = oracle["category_ids"].as_array().expect("an array");
            let answer = |needle: &str| {
                cases
                    .iter()
                    .find(|c| c["input"] == needle)
                    .and_then(|c| c["is_valid_category_id"].as_bool())
                    .unwrap_or_else(|| panic!("{needle} is not in the corpus"))
            };

            let a = "abcdefghijklmnopqrstuvwxyz";
            let b = "0123456789abcdefghijklmnop";

            assert!(answer(&format!("zzfavorites_{a}_{b}")), "leading junk");
            assert!(answer(&format!("favorites_{a}_{b}zz")), "trailing junk");
            // A 27-character second half: the pattern consumes 26 and is satisfied.
            assert!(answer(&format!("favorites_{a}_{a}a")), "long second half");
            // A 27-character FIRST half is not, because the `_` that follows it is displaced.
            assert!(!answer(&format!("favorites_{a}a_{b}")), "long first half");
        }

        /// `null` and `[]` are different bytes, and Go produces both.
        #[test]
        fn nil_and_empty_slices_marshal_differently() {
            let oracle = oracle();
            let shapes = &oracle["nil_shapes"];

            let nil_channels = SidebarCategoryWithChannels {
                category: SidebarCategory {
                    id: "y9i4er48tt8bukijy7i3u5y9ar".to_owned(),
                    ..SidebarCategory::default()
                },
                channel_ids: None,
            };
            assert_eq!(
                serde_json::to_value(&nil_channels).expect("serialises"),
                shapes["category_nil_channels"]
            );

            let empty_channels = SidebarCategoryWithChannels {
                channel_ids: Some(Vec::new()),
                ..nil_channels.clone()
            };
            assert_eq!(
                serde_json::to_value(&empty_channels).expect("serialises"),
                shapes["category_empty_channels"]
            );

            assert_eq!(
                serde_json::to_value(OrderedSidebarCategories::default()).expect("serialises"),
                shapes["ordered_nil"]
            );
            assert_eq!(
                serde_json::to_value(OrderedSidebarCategories {
                    categories: Some(Vec::new()),
                    order: Some(Vec::new()),
                })
                .expect("serialises"),
                shapes["ordered_empty"]
            );

            // And the accessor carries the nil-ness through rather than normalising it.
            assert_eq!(
                serde_json::to_value(nil_channels.channel_ids()).expect("serialises"),
                shapes["channel_ids_of_nil"]
            );
            assert_eq!(
                serde_json::to_value(empty_channels.channel_ids()).expect("serialises"),
                shapes["channel_ids_of_empty"]
            );
        }
    }
}
