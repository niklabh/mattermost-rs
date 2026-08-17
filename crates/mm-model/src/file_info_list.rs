//! Port of `model/file_info_list.go` (file_info_list.go:1–113).
//!
//! `PostList`'s twin, and the reason to read this header is that it is *not* a rename of
//! [`crate::post_list`]. Five things differ, all measured:
//!
//! | | `PostList` | `FileInfoList` |
//! |---|---|---|
//! | `ToSlice` on an empty order | empty allocated slice when the map is non-empty | always **nil** |
//! | `MakeNonNil` | recurses into every post | materialises the two collections and stops |
//! | the add-one method | no nil check, crashes writing `BurnOnReadPosts` ([D-052]) | checks its map, then dereferences its **argument** |
//! | `Clone` / `ForPlugin` / `StripActionIntegrations` / `ToJSON` | all present | none — this is a plain container |
//! | third collection | `burn_on_read_posts`, `json:"-"` | none |
//!
//! `Etag` is the one place a difference was expected and there is none: it is the same function
//! character for character, including the `Order[0]` prefix that makes it order-dependent.
//!
//! # Nil and empty are different, and each method materialises a different subset
//!
//! Neither `order` nor `file_infos` carries `omitempty`, so a nil collection reaches the client
//! as `null` rather than `[]`/`{}`. Both are therefore `Option`, and the subsets are measured in
//! `fixtures/behaviour_file_info_list.json`, not read off the source:
//!
//! | | `order` | `file_infos` |
//! |---|---|---|
//! | [`FileInfoList::new`] | ✓ | ✓ |
//! | [`FileInfoList::make_non_nil`] | ✓ | ✓ |
//! | [`FileInfoList::add_order`] | ✓ | — |
//! | [`FileInfoList::add_file_info`] | — | ✓ |
//! | [`FileInfoList::unique_order`] | ✓ | — |
//! | [`FileInfoList::extend`] | ✓ (through `unique_order`) | only if `other` had one |

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::file_info::FileInfo;
use crate::utils::{StringArray, etag};

/// The file map. A `BTreeMap` rather than a `HashMap` because Go sorts map keys by byte value
/// when marshalling and `String`'s `Ord` is byte-wise — see [D-027].
pub type FileInfoMap = BTreeMap<String, FileInfo>;

/// Port of `model.FileInfoList` (file_info_list.go:10).
///
/// The container carries `#[serde(default)]` for the same reason [`crate::post_list::PostList`]
/// does: Go leaves an absent field at its zero value, and a client sending a partial list would
/// otherwise be rejected. See [D-043].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FileInfoList {
    /// The display order. Nil and empty are distinguishable on the wire.
    #[serde(rename = "order")]
    pub order: Option<StringArray>,

    /// The files, keyed by id. An order entry with no matching file here is legal and reachable
    /// — see [`FileInfoList::to_slice`].
    #[serde(rename = "file_infos")]
    pub file_infos: Option<FileInfoMap>,

    #[serde(rename = "next_file_info_id")]
    pub next_file_info_id: String,

    #[serde(rename = "prev_file_info_id")]
    pub prev_file_info_id: String,

    /// The time of the latest inaccessible file, when the list was truncated by a retention
    /// policy. Epoch milliseconds.
    #[serde(rename = "first_inaccessible_file_time")]
    pub first_inaccessible_file_time: i64,
}

impl FileInfoList {
    /// Every JSON key this type claims, in declaration order.
    ///
    /// Exists for [`crate::file_info_search_results::FileInfoSearchResults`], which embeds a
    /// **nillable** `*FileInfoList` that Go allocates lazily — only once a decode walks into it
    /// for a key it recognises. Deciding that needs the key set.
    ///
    /// Unlike [`crate::post_list::PostList::WIRE_KEYS`] this is the *whole* struct: `PostList`
    /// has a `json:"-"` field that is promoted and yet an unknown key, and `FileInfoList` has no
    /// such field. Pinned against the Go struct tags by
    /// `file_info_search_results::go_parity::the_promoted_key_set_matches_go`, so a field added
    /// upstream fails a test rather than silently narrowing the set.
    pub const WIRE_KEYS: [&'static str; 5] = [
        "order",
        "file_infos",
        "next_file_info_id",
        "prev_file_info_id",
        "first_inaccessible_file_time",
    ];

    /// Port of `model.NewFileInfoList` (file_info_list.go:19).
    ///
    /// **Not the same as [`FileInfoList::default`].** `Default` is Go's zero value, where both
    /// collections are nil and serialise as `null`; this materialises them, so a list built here
    /// serialises with `[]` and `{}`. Both states are reachable and the difference reaches the
    /// client, so the two constructors are deliberately kept apart.
    pub fn new() -> Self {
        Self {
            order: Some(Vec::new()),
            file_infos: Some(FileInfoMap::new()),
            next_file_info_id: String::new(),
            prev_file_info_id: String::new(),
            first_inaccessible_file_time: 0,
        }
    }

    /// Port of `(*FileInfoList).ToSlice` (file_info_list.go:28).
    ///
    /// One entry per **order** id, so a file that is in `file_infos` but not in `order` does not
    /// appear, and an order id with no file yields `None` — Go's `[]*FileInfo` holds a nil there.
    /// [`Self::add_order`] takes an id without requiring a file, so the `Option` is not
    /// defensive.
    ///
    /// Unlike `PostList::to_slice`, Go's version here never pre-allocates: it declares a nil
    /// slice and appends, so an empty `order` returns **nil** even when the map is full. No Go
    /// call site can observe that (all of them range over the result or take its length), so it
    /// maps to an empty `Vec` — but the oracle records the flag, and
    /// `to_slice_matches_go` asserts it.
    pub fn to_slice(&self) -> Vec<Option<&FileInfo>> {
        let Some(order) = self.order.as_ref() else {
            return Vec::new();
        };
        order
            .iter()
            .map(|id| self.file_infos.as_ref().and_then(|infos| infos.get(id)))
            .collect()
    }

    /// Port of `(*FileInfoList).MakeNonNil` (file_info_list.go:36).
    ///
    /// Materialises both collections and **does not recurse** — `PostList::make_non_nil` walks
    /// into every post and calls its `MakeNonNil`; this one stops at the container.
    pub fn make_non_nil(&mut self) {
        self.order.get_or_insert_with(Vec::new);
        self.file_infos.get_or_insert_with(FileInfoMap::new);
    }

    /// Port of `(*FileInfoList).AddOrder` (file_info_list.go:46).
    ///
    /// Materialises `order` and leaves `file_infos` alone, so a list can come out of this holding
    /// `"order":["x"],"file_infos":null`. Go's `make([]string, 0, 128)` capacity hint has no
    /// observable effect.
    pub fn add_order(&mut self, id: impl Into<String>) {
        self.order.get_or_insert_with(Vec::new).push(id.into());
    }

    /// Port of `(*FileInfoList).AddFileInfo` (file_info_list.go:54).
    ///
    /// Keyed by the file's own `id`, so an id-less file is filed under `""` — Go does the same,
    /// and the oracle drives it.
    ///
    /// Go takes a `*FileInfo` and dereferences it for the key **after** nil-checking the map, so
    /// `AddFileInfo(nil)` panics. Taking the value by move makes that unrepresentable rather than
    /// merely unlikely; see [D-058]. Materialises `file_infos` and leaves `order` alone.
    pub fn add_file_info(&mut self, file_info: FileInfo) {
        self.file_infos
            .get_or_insert_with(FileInfoMap::new)
            .insert(file_info.id.clone(), file_info);
    }

    /// Port of `(*FileInfoList).UniqueOrder` (file_info_list.go:62).
    ///
    /// Keeps the **first** occurrence of each id and always leaves `order` non-nil, even when it
    /// started nil and stayed empty.
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

    /// Port of `(*FileInfoList).Extend` (file_info_list.go:75).
    ///
    /// Takes **every** file from `other`, not only the ones in its order, then appends its order
    /// and deduplicates. Go ranges over `other.FileInfos`, whose iteration order it randomises;
    /// the result is order-independent because the writes are keyed, which the oracle proves by
    /// running each pair twice rather than assuming it.
    ///
    /// Go shares the `*FileInfo` pointers with `other`; we clone. Same class as [D-055] — no Go
    /// call site mutates through either handle afterwards.
    pub fn extend(&mut self, other: &FileInfoList) {
        for info in other.file_infos.iter().flatten().map(|(_, info)| info) {
            self.add_file_info(info.clone());
        }
        for id in other.order.iter().flatten() {
            self.add_order(id.clone());
        }
        self.unique_order();
    }

    /// Port of `(*FileInfoList).SortByCreateAt` (file_info_list.go:87).
    ///
    /// Sorts `order` by the referenced file's `create_at`, **descending**. An order id with no
    /// file sorts as `0` here and panics in Go, which dereferences a nil `*FileInfo` in the
    /// comparator — [D-058].
    ///
    /// Go uses `sort.Slice`, which is **not stable**, and `order` is on the wire, so two files
    /// sharing a `create_at` can come out differently in the two servers once the list is long
    /// enough to leave Go's insertion-sort threshold. Identical to [D-051] on `PostList`, and
    /// measured again here rather than assumed: `an_unstable_go_sort_scrambles_ties_above_twelve`
    /// asserts the divergence and the create-at sequences still agree.
    pub fn sort_by_create_at(&mut self) {
        let Self {
            order, file_infos, ..
        } = self;
        let Some(order) = order.as_mut() else {
            return;
        };
        order.sort_by_key(|id| {
            std::cmp::Reverse(
                file_infos
                    .as_ref()
                    .and_then(|infos| infos.get(id))
                    .map_or(0, |info| info.create_at),
            )
        });
    }

    /// Port of `(*FileInfoList).Etag` (file_info_list.go:93).
    ///
    /// The same function as `PostList::etag`, character for character. Two halves worth keeping
    /// apart:
    ///
    /// - The **file** component is a maximum over the pair `(update_at, id)` seeded with
    ///   `(0, "0")`, which makes it independent of Go's randomised map iteration. The seed is
    ///   reachable: a file with `update_at: 0` and an id above `"0"` beats it, one below does
    ///   not — both cases are in the corpus.
    /// - The **first** component is `Order[0]`, so the etag *is* order-dependent. Reversing
    ///   `order` changes it, which `etag_matches_go` asserts against Go's own reversed answer.
    pub fn etag(&self) -> String {
        let mut id = "0";
        let mut t = 0_i64;
        for info in self.file_infos.iter().flatten().map(|(_, info)| info) {
            if info.update_at > t || (info.update_at == t && info.id.as_str() > id) {
                t = info.update_at;
                id = &info.id;
            }
        }

        let order_id = self
            .order
            .as_ref()
            .and_then(|order| order.first())
            .map_or("", String::as_str);

        etag(&[&order_id, &id, &t])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(id: &str, create_at: i64) -> FileInfo {
        FileInfo {
            id: id.into(),
            // The Go field is `CreatorId`; its JSON key is `user_id`.
            creator_id: "u1".into(),
            channel_id: "c1".into(),
            create_at,
            update_at: create_at,
            ..Default::default()
        }
    }

    #[test]
    fn new_is_not_default() {
        // The distinction reaches the client: `[]`/`{}` against `null`.
        assert_ne!(FileInfoList::new(), FileInfoList::default());
        assert_eq!(
            serde_json::to_string(&FileInfoList::default()).unwrap(),
            r#"{"order":null,"file_infos":null,"next_file_info_id":"","prev_file_info_id":"","first_inaccessible_file_time":0}"#
        );
    }

    #[test]
    fn to_slice_follows_order_and_reports_a_missing_file() {
        let list = FileInfoList {
            order: Some(vec!["f1".into(), "ghost".into()]),
            file_infos: Some(FileInfoMap::from([("f1".into(), info("f1", 100))])),
            ..Default::default()
        };
        let slice = list.to_slice();
        assert_eq!(slice.len(), 2);
        assert_eq!(slice[0].unwrap().id, "f1");
        assert!(slice[1].is_none());
    }

    #[test]
    fn a_file_outside_order_is_on_the_wire_and_out_of_the_slice() {
        let list = FileInfoList {
            order: Some(Vec::new()),
            file_infos: Some(FileInfoMap::from([("f1".into(), info("f1", 100))])),
            ..Default::default()
        };
        assert!(list.to_slice().is_empty());
        assert!(serde_json::to_string(&list).unwrap().contains(r#""f1""#));
    }

    #[test]
    fn make_non_nil_does_not_recurse() {
        // PostList::make_non_nil walks into every post; this one stops at the container.
        let mut list = FileInfoList::default();
        list.make_non_nil();
        assert_eq!(list.order, Some(Vec::new()));
        assert_eq!(list.file_infos, Some(FileInfoMap::new()));
    }

    #[test]
    fn add_order_and_add_file_info_materialise_different_halves() {
        let mut ordered = FileInfoList::default();
        ordered.add_order("x");
        assert!(ordered.order.is_some() && ordered.file_infos.is_none());

        let mut filed = FileInfoList::default();
        filed.add_file_info(info("f1", 1));
        assert!(filed.order.is_none() && filed.file_infos.is_some());
    }

    #[test]
    fn an_id_less_file_is_filed_under_the_empty_string() {
        let mut list = FileInfoList::default();
        list.add_file_info(FileInfo::default());
        assert!(list.file_infos.as_ref().unwrap().contains_key(""));
    }

    #[test]
    fn unique_order_keeps_the_first_occurrence_and_never_leaves_nil() {
        let mut list = FileInfoList {
            order: Some(vec!["a".into(), "b".into(), "a".into(), "c".into()]),
            ..Default::default()
        };
        list.unique_order();
        assert_eq!(list.order, Some(vec!["a".into(), "b".into(), "c".into()]));

        let mut empty = FileInfoList::default();
        empty.unique_order();
        assert_eq!(empty.order, Some(Vec::new()));
    }

    #[test]
    fn extend_takes_every_file_not_only_the_ordered_ones() {
        let mut list = FileInfoList::default();
        let other = FileInfoList {
            order: Some(vec!["f1".into()]),
            file_infos: Some(FileInfoMap::from([
                ("f1".into(), info("f1", 100)),
                ("unordered".into(), info("unordered", 200)),
            ])),
            ..Default::default()
        };
        list.extend(&other);
        assert_eq!(list.file_infos.as_ref().unwrap().len(), 2);
        assert_eq!(list.order, Some(vec!["f1".into()]));
    }

    #[test]
    fn etag_depends_on_both_the_newest_file_and_the_first_order_id() {
        let list = FileInfoList {
            order: Some(vec!["f1".into(), "f2".into()]),
            file_infos: Some(FileInfoMap::from([
                ("f1".into(), info("f1", 100)),
                ("f2".into(), info("f2", 100)),
            ])),
            ..Default::default()
        };
        // Equal update_at, so the larger id wins the file component...
        assert!(list.etag().ends_with(".f1.f2.100"));

        // ...and the first order id is the component before it.
        let reversed = FileInfoList {
            order: Some(vec!["f2".into(), "f1".into()]),
            ..list.clone()
        };
        assert_ne!(list.etag(), reversed.etag());
        assert!(reversed.etag().ends_with(".f2.f2.100"));
    }
}

/// Parity tests driven by `fixtures/behaviour_file_info_list.json` — Go's own answers.
#[cfg(test)]
mod go_parity {
    use super::*;
    use crate::utils::go_json_marshal;
    use serde_json::Value;

    fn oracle() -> Value {
        serde_json::from_str(include_str!(
            "../../../fixtures/behaviour_file_info_list.json"
        ))
        .unwrap()
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

    fn list_from(case: &Value, key: &str) -> FileInfoList {
        serde_json::from_str(case[key].as_str().unwrap()).unwrap()
    }

    /// The one corpus document we cannot decode at all: Go's `map[string]*FileInfo` accepts a nil
    /// value and our `BTreeMap<String, FileInfo>` does not. Asserted once, in
    /// [`the_wire_format_matches_go`], and skipped everywhere else. See [D-033].
    const UNDECODABLE: &str = "nil_info_in_map";

    /// Asserts a whole list against a `dumpFileInfoList` record: the exact bytes plus the
    /// nil-ness of both collections, which the JSON shows only as `null` against `[]`/`{}`.
    fn assert_dump(actual: &FileInfoList, expected: &Value, what: &str) {
        assert_eq!(
            go_json_marshal(actual).unwrap(),
            expected["json"].as_str().unwrap(),
            "{what}: bytes"
        );
        assert_eq!(
            actual.order.is_none(),
            expected["order_nil"].as_bool().unwrap(),
            "{what}: order nil-ness"
        );
        assert_eq!(
            actual.file_infos.is_none(),
            expected["infos_nil"].as_bool().unwrap(),
            "{what}: file_infos nil-ness"
        );
    }

    /// Runs the shared corpus through a mutating method, skipping the cases Go crashes on.
    fn assert_mutation(key: &str, apply: impl Fn(&mut FileInfoList)) {
        let oracle = oracle();
        let mut checked = 0;
        for case in section(&oracle, key) {
            let what = format!("{key}({})", name(case));
            if panicked(case) || name(case) == UNDECODABLE {
                continue;
            }
            let mut list = list_from(case, "in");
            apply(&mut list);
            assert_dump(&list, &case["out"], &what);
            checked += 1;
        }
        assert!(checked > 0, "{key}: nothing was checked");
    }

    #[test]
    fn new_matches_go() {
        let oracle = oracle();
        assert_dump(&FileInfoList::new(), &oracle["new"], "new");
    }

    #[test]
    fn the_wire_format_matches_go() {
        let oracle = oracle();
        for case in section(&oracle, "wire") {
            let what = name(case);
            assert!(!panicked(case), "{what}: Go panicked decoding");

            if what == UNDECODABLE {
                // [D-033]: Go re-emits the nil value as `null`; we cannot decode it at all.
                assert!(
                    serde_json::from_str::<FileInfoList>(case["in"].as_str().unwrap()).is_err(),
                    "{what}: we decoded it, so [D-033] can be narrowed"
                );
                assert!(
                    case["out"]["json"]
                        .as_str()
                        .unwrap()
                        .contains(r#""f1":null"#),
                    "{what}: Go kept the nil entry"
                );
                continue;
            }

            assert_dump(&list_from(case, "in"), &case["out"], what);
        }
    }

    #[test]
    fn to_slice_matches_go() {
        let oracle = oracle();
        for case in section(&oracle, "to_slice") {
            let what = name(case);
            if panicked(case) || what == UNDECODABLE {
                continue;
            }
            let list = list_from(case, "in");
            let slice = list.to_slice();
            assert_eq!(
                slice.len() as u64,
                case["count"].as_u64().unwrap(),
                "{what}: count"
            );

            if case["nil_slice"].as_bool().unwrap() {
                // Go returns a **nil** slice here, which marshals to `null`; ours is an empty
                // `Vec`, which marshals to `[]`. Flattened deliberately: `ToSlice`'s result is an
                // internal helper that every Go call site ranges over or takes the length of, so
                // no wire format depends on it. Asserted rather than skipped so a future change
                // that starts marshalling it fails here.
                assert!(slice.is_empty(), "{what}: Go's nil slice");
                assert_eq!(case["out"].as_str().unwrap(), "null", "{what}: Go's bytes");
                assert_eq!(go_json_marshal(&slice).unwrap(), "[]", "{what}: our bytes");
            } else {
                assert_eq!(
                    go_json_marshal(&slice).unwrap(),
                    case["out"].as_str().unwrap(),
                    "{what}: elements"
                );
            }
        }
    }

    #[test]
    fn make_non_nil_matches_go() {
        assert_mutation("make_non_nil", FileInfoList::make_non_nil);
    }

    #[test]
    fn add_order_matches_go() {
        assert_mutation("add_order", |list| {
            list.add_order("added");
            list.add_order("added");
        });
    }

    #[test]
    fn unique_order_matches_go() {
        assert_mutation("unique_order", FileInfoList::unique_order);
    }

    #[test]
    fn sort_by_create_at_matches_go() {
        assert_mutation("sort_by_create_at", FileInfoList::sort_by_create_at);
    }

    /// [D-058]: the corpus case Go crashes on is the one where an order id has no file, and ours
    /// treats the missing file as `create_at: 0`.
    #[test]
    fn sort_by_create_at_answers_where_go_panics() {
        let oracle = oracle();
        let mut crashes = 0;
        for case in section(&oracle, "sort_by_create_at") {
            if !panicked(case) || name(case) == UNDECODABLE {
                continue;
            }
            crashes += 1;
            let mut list = list_from(case, "in");
            list.sort_by_create_at();
            // A missing file sorts as 0, i.e. last in a descending sort.
            assert_eq!(
                list.order.as_ref().unwrap().last().map(String::as_str),
                Some("missing"),
                "{}",
                name(case)
            );
        }
        assert_eq!(crashes, 1, "the order-without-file case");
    }

    #[test]
    fn add_file_info_matches_go() {
        let oracle = oracle();
        let mut go_nil_crashes = 0;
        for case in section(&oracle, "add_file_info") {
            let what = name(case);
            let arg = case["arg"].as_str().unwrap();

            if arg == "nil" {
                // [D-058]: `AddFileInfo(nil)` dereferences the argument for its key. Our
                // signature takes the value by move, so there is no nil to pass.
                assert!(panicked(case), "{what}: Go survived a nil file");
                go_nil_crashes += 1;
                continue;
            }
            if panicked(case) || case["in"].as_str().unwrap().contains("null") {
                continue;
            }

            let mut list = list_from(case, "in");
            match arg {
                "real" => list.add_file_info(
                    serde_json::from_str(
                        r#"{"id":"f3","user_id":"u1","channel_id":"c2","create_at":50,
                           "update_at":50,"name":"three.pdf","extension":"pdf","size":33,
                           "mime_type":"application/pdf"}"#,
                    )
                    .unwrap(),
                ),
                "empty_id" => list.add_file_info(FileInfo::default()),
                other => panic!("unhandled arg {other}"),
            }
            assert_dump(&list, &case["out"], what);
        }
        assert!(go_nil_crashes > 0, "the nil-argument cases");
    }

    #[test]
    fn extend_matches_go() {
        let oracle = oracle();
        let mut checked = 0;
        for case in section(&oracle, "extend") {
            let what = name(case);
            if panicked(case) || what.contains(UNDECODABLE) {
                continue;
            }
            // Go randomises its map iteration and still answers the same twice, because the
            // writes are keyed. Ours is a BTreeMap, so this is free — assert Go's flag anyway.
            assert!(
                case["deterministic"].as_bool().unwrap(),
                "{what}: Go was not deterministic"
            );

            let mut list = list_from(case, "in");
            list.extend(&list_from(case, "other"));
            assert_dump(&list, &case["out"], what);
            checked += 1;
        }
        assert!(checked > 100, "the corpus is crossed with itself");
    }

    #[test]
    fn etag_matches_go() {
        let oracle = oracle();
        for case in section(&oracle, "etag") {
            let what = name(case);
            if panicked(case) || what == UNDECODABLE {
                continue;
            }
            let list = list_from(case, "in");
            assert_eq!(list.etag(), case["etag"].as_str().unwrap(), "{what}");

            // The first component is Order[0], so reversing the order changes the answer.
            let mut reversed = list.clone();
            if let Some(order) = reversed.order.as_mut() {
                order.reverse();
            }
            assert_eq!(
                reversed.etag(),
                case["etag_reversed"].as_str().unwrap(),
                "{what}: reversed"
            );
        }
    }

    /// [D-051] again, on this type: Go's `sort.Slice` is unstable and `order` is on the wire.
    /// Below thirteen elements it runs insertion sort and agrees with a stable sort; above it,
    /// pdqsort's partitioning scrambles ties.
    #[test]
    fn an_unstable_go_sort_scrambles_ties_above_twelve() {
        let oracle = oracle();
        let mut diverged = 0;
        for case in section(&oracle, "sort_ties") {
            let what = name(case);
            assert!(!panicked(case), "{what}");

            let mut list = FileInfoList::new();
            let in_order: Vec<String> = serde_json::from_value(case["in_order"].clone()).unwrap();
            let stable_order: Vec<String> =
                serde_json::from_value(case["stable_order"].clone()).unwrap();
            let go_order: Vec<String> = serde_json::from_value(case["out_order"].clone()).unwrap();
            let create_at: Vec<i64> =
                serde_json::from_value(case["create_at_sequence"].clone()).unwrap();

            // Rebuild Go's input: create_at is recoverable from Go's own answer.
            let mut by_id: BTreeMap<&str, i64> = BTreeMap::new();
            for (id, at) in go_order.iter().zip(&create_at) {
                by_id.insert(id, *at);
            }
            for id in &in_order {
                list.add_order(id.clone());
                list.add_file_info(FileInfo {
                    id: id.clone(),
                    create_at: by_id[id.as_str()],
                    ..Default::default()
                });
            }
            list.sort_by_create_at();
            let ours = list.order.clone().unwrap();

            // Ours is stable, so it must equal Go's stable-sort control every time...
            assert_eq!(ours, stable_order, "{what}: ours is not a stable sort");
            // ...and Go's own answer agrees only below the threshold.
            assert_eq!(
                go_order == stable_order,
                case["stable_agrees"].as_bool().unwrap(),
                "{what}: Go's stability flag"
            );
            if !case["stable_agrees"].as_bool().unwrap() {
                diverged += 1;
                assert_ne!(ours, go_order, "{what}: the divergence vanished");
            }

            // Both are correct sorts: the create_at sequence is identical either way.
            let ours_sequence: Vec<i64> = ours.iter().map(|id| by_id[id.as_str()]).collect();
            assert_eq!(ours_sequence, create_at, "{what}: create_at sequence");
        }
        assert_eq!(diverged, 1, "the twenty-element interleaved case");
    }
}

/// Serialization parity against `fixtures/file_info_list.json` — the reflection-populated oracle,
/// every field non-zero.
#[cfg(test)]
mod fixture {
    use super::*;

    #[test]
    fn round_trips_the_generated_fixture() {
        let raw = include_str!("../../../fixtures/file_info_list.json");
        let decoded: FileInfoList = serde_json::from_str(raw).unwrap();

        assert!(decoded.order.as_ref().is_some_and(|o| !o.is_empty()));
        assert!(decoded.file_infos.as_ref().is_some_and(|m| !m.is_empty()));
        assert_ne!(decoded.first_inaccessible_file_time, 0);

        let ours: serde_json::Value = serde_json::to_value(&decoded).unwrap();
        let theirs: serde_json::Value = serde_json::from_str(raw).unwrap();
        assert_eq!(ours, theirs);
    }
}
