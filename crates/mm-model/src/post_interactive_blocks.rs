//! Port of `server/public/model/post_interactive_blocks.go` — **the walkers, not the action
//! ids**.
//!
//! The Go file is 41 functions over untyped JSON trees in three dialects — `props.mm_blocks`
//! (Mattermost's own), `props.blocks` (Block Kit) and `props.cards` (Adaptive Cards) — serving
//! two purposes: pulling human-readable text out of a post, and pulling image URLs out of it.
//! Both of those are ported here, and both are driven from `post.rs`
//! ([`Post::all_strings`](crate::post::Post::all_strings) and
//! [`Post::interactive_blocks_image_urls`](crate::post::Post::interactive_blocks_image_urls)).
//!
//! **Not ported: everything downstream of `appendMmactionIDsFromText`** — the whole
//! `Collect*ActionIDs` family, `RefreshInteractiveActionsOnPost`,
//! `ApplyMmBlocksWithActionsToProps`, `validateMmBlocksActionsPairing` and the two webhook
//! validators. That function calls `markdown.Inspect`, so it needs the 4,688-line
//! `shared/markdown` parser; see [D-044]. The id collectors read `mmaction://` links out of
//! *every* text node they walk, so porting them without the parser would silently under-report
//! ids, which is a validation failure in the permissive direction.
//!
//! Every function here is a faithful transliteration of an unexported Go one, so nothing is
//! `pub`: the surface is the two `Post` methods. Go declares several pairs of byte-identical
//! functions (`appendHumanStringsFromMmBlocks` / `…FromMmBlocksArray`, `appendMmBlockImageURLs` /
//! `appendMmBlocksArrayImageURLs`); each pair is one function here.
//!
//! Untyped tree walking is where a port drifts silently, because every type mismatch in the Go
//! source is a **no-op rather than an error** — a wrong key, a string where a map was expected,
//! an object where an array was expected, all just produce nothing. The oracle drives each of
//! those individually; see `fixtures/behaviour_post_interactive_blocks.json`.

use serde_json::{Map, Value};

use crate::message_attachment::MessageAttachment;
use crate::post::{
    POST_PROPS_ADAPTIVE_CARDS, POST_PROPS_BLOCK_KIT_BLOCKS, POST_PROPS_MM_BLOCKS, Post,
    append_non_whitespace_only_message,
};

/// Port of `interactivePropJSONArray` (post_interactive_blocks.go:31).
///
/// A missing key, a JSON `null`, an object and a string all fail alike — Go's type assertion
/// against `[]any` is the only test.
pub(crate) fn interactive_prop_json_array(raw: Option<&Value>) -> Option<&Vec<Value>> {
    match raw {
        Some(Value::Array(items)) => Some(items),
        _ => None,
    }
}

/// Go's `m["type"].(string)` discards the failure, leaving `""` — which matches no case in any
/// of the switches below. A missing type and a numeric one are therefore the same as an
/// unrecognised one.
fn block_type(m: &Map<String, Value>) -> &str {
    m.get("type").and_then(Value::as_str).unwrap_or("")
}

fn string_at<'a>(m: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    m.get(key).and_then(Value::as_str)
}

fn array_at<'a>(m: &'a Map<String, Value>, key: &str) -> Option<&'a Vec<Value>> {
    m.get(key).and_then(Value::as_array)
}

fn object_at<'a>(m: &'a Map<String, Value>, key: &str) -> Option<&'a Map<String, Value>> {
    m.get(key).and_then(Value::as_object)
}

// --- human-readable strings --------------------------------------------------------------------

/// Port of `appendHumanReadableInteractiveStrings` (post_interactive_blocks.go:14).
///
/// The order is fixed by the source and is wire-adjacent — it decides the order of the strings a
/// search index or a mention check sees: mm_blocks, then Block Kit, then Adaptive Cards.
pub(crate) fn append_human_readable_interactive_strings(post: &Post, out: &mut Vec<String>) {
    let Some(props) = post.get_props() else {
        return;
    };

    append_human_strings_from_mm_blocks(props.get(POST_PROPS_MM_BLOCKS), out);
    append_human_strings_from_block_kit_tree(props.get(POST_PROPS_BLOCK_KIT_BLOCKS), out);
    append_human_strings_from_adaptive_cards_tree(props.get(POST_PROPS_ADAPTIVE_CARDS), out);
}

/// Port of `appendHumanStringsFromMmBlocks` (post_interactive_blocks.go:45) and its byte-identical
/// twin `appendHumanStringsFromMmBlocksArray` (:90).
fn append_human_strings_from_mm_blocks(raw: Option<&Value>, out: &mut Vec<String>) {
    let Some(blocks) = interactive_prop_json_array(raw) else {
        return;
    };
    append_human_strings_from_mm_blocks_slice(blocks, out);
}

fn append_human_strings_from_mm_blocks_slice(blocks: &[Value], out: &mut Vec<String>) {
    for block in blocks {
        if let Some(m) = block.as_object() {
            append_human_strings_from_mm_block_map(m, out);
        }
    }
}

/// Port of `appendHumanStringsFromMmBlockMap` (post_interactive_blocks.go:60).
///
/// A `column_set` hands the whole `items` array to the block walker. Its image-URL counterpart
/// hands each **element** instead — see [`append_mm_block_map_image_urls`]. The two disagree, and
/// the oracle pins both.
fn append_human_strings_from_mm_block_map(m: &Map<String, Value>, out: &mut Vec<String>) {
    match block_type(m) {
        "text" => {
            if let Some(s) = string_at(m, "text") {
                append_non_whitespace_only_message(out, s);
            }
        }
        "container" => append_human_strings_from_mm_blocks(m.get("content"), out),
        "collapsible" => {
            append_human_strings_from_mm_blocks(m.get("header"), out);
            append_human_strings_from_mm_blocks(m.get("content"), out);
        }
        "column_set" => {
            let Some(columns) = array_at(m, "columns") else {
                return;
            };
            for column in columns {
                let Some(cm) = column.as_object() else {
                    continue;
                };
                let Some(items) = array_at(cm, "items") else {
                    continue;
                };
                append_human_strings_from_mm_blocks_slice(items, out);
            }
        }
        _ => {}
    }
}

/// Port of `appendHumanStringsFromBlockKitTree` (post_interactive_blocks.go:105).
///
/// Block Kit is flat — no recursion — and its two text shapes are not interchangeable. A
/// `markdown` block reads a bare string at `text`; a `section` and a `header` read `text.text`
/// off an object. Each dialect's mismatch is silent, so `{"type":"markdown","text":{"text":"x"}}`
/// contributes nothing, and neither does a `section` whose `text` is a bare string.
fn append_human_strings_from_block_kit_tree(raw: Option<&Value>, out: &mut Vec<String>) {
    let Some(blocks) = interactive_prop_json_array(raw) else {
        return;
    };

    for block in blocks {
        let Some(m) = block.as_object() else {
            continue;
        };
        match block_type(m) {
            "markdown" => {
                if let Some(s) = string_at(m, "text") {
                    append_non_whitespace_only_message(out, s);
                }
            }
            "section" => {
                if let Some(text_block) = object_at(m, "text") {
                    if let Some(s) = string_at(text_block, "text") {
                        append_non_whitespace_only_message(out, s);
                    }
                }
                if let Some(fields) = array_at(m, "fields") {
                    for field in fields {
                        let Some(fm) = field.as_object() else {
                            continue;
                        };
                        if let Some(s) = string_at(fm, "text") {
                            append_non_whitespace_only_message(out, s);
                        }
                    }
                }
            }
            "header" => {
                if let Some(text_block) = object_at(m, "text") {
                    if let Some(s) = string_at(text_block, "text") {
                        append_non_whitespace_only_message(out, s);
                    }
                }
            }
            _ => {}
        }
    }
}

/// Port of `appendHumanStringsFromAdaptiveCardsTree` (post_interactive_blocks.go:150).
///
/// Only `body` is walked. A card's top-level `actions` are not, so an `Action.Submit` title never
/// reaches the output — unlike `ActionSet`, which is walked and still contributes nothing because
/// the item walker has no case for it.
fn append_human_strings_from_adaptive_cards_tree(raw: Option<&Value>, out: &mut Vec<String>) {
    let Some(cards) = interactive_prop_json_array(raw) else {
        return;
    };

    for card in cards {
        let Some(cm) = card.as_object() else {
            continue;
        };
        let Some(body) = array_at(cm, "body") else {
            continue;
        };
        for item in body {
            append_human_strings_from_adaptive_cards_item(item, out);
        }
    }
}

/// Port of `appendHumanStringsFromAdaptiveCardsItem` (post_interactive_blocks.go:171).
fn append_human_strings_from_adaptive_cards_item(item: &Value, out: &mut Vec<String>) {
    let Some(m) = item.as_object() else {
        return;
    };

    match block_type(m) {
        "TextBlock" => {
            if let Some(s) = string_at(m, "text") {
                append_non_whitespace_only_message(out, s);
            }
        }
        "Container" => {
            if let Some(items) = array_at(m, "items") {
                for nested in items {
                    append_human_strings_from_adaptive_cards_item(nested, out);
                }
            }
        }
        "ColumnSet" => {
            if let Some(columns) = array_at(m, "columns") {
                for column in columns {
                    let Some(column_map) = column.as_object() else {
                        continue;
                    };
                    let Some(items) = array_at(column_map, "items") else {
                        continue;
                    };
                    for nested in items {
                        append_human_strings_from_adaptive_cards_item(nested, out);
                    }
                }
            }
        }
        _ => {}
    }
}

// --- image URLs ----------------------------------------------------------------------------------

/// Port of `appendMmBlockImageURLs` (post_interactive_blocks.go:208) and its byte-identical twin
/// `appendMmBlocksArrayImageURLs` (:255).
fn append_mm_block_image_urls(out: &mut Vec<String>, raw: Option<&Value>) {
    let Some(blocks) = interactive_prop_json_array(raw) else {
        return;
    };
    for block in blocks {
        if let Some(m) = block.as_object() {
            append_mm_block_map_image_urls(out, m);
        }
    }
}

/// Port of `appendMmBlockMapImageURLs` (post_interactive_blocks.go:223).
///
/// **The `column_set` case does not mirror its human-strings counterpart.** Go passes each
/// *element* of a column's `items` to the array walker, which re-tests it as an array — so an
/// image reaches the output only when `items` is an array **of arrays**. The ordinary shape
/// (`items: [{"type":"image",…}]`) yields nothing. Measured, not inferred; almost certainly an
/// upstream bug, and reproduced because a Rust server that found the image would attach a preview
/// the Go server does not.
///
/// An empty `url` is emitted as an empty string rather than skipped — unlike the attachment
/// walker, which tests for emptiness.
fn append_mm_block_map_image_urls(out: &mut Vec<String>, m: &Map<String, Value>) {
    match block_type(m) {
        "image" => {
            if let Some(u) = string_at(m, "url") {
                out.push(u.to_string());
            }
        }
        "container" => append_mm_block_image_urls(out, m.get("content")),
        "collapsible" => {
            append_mm_block_image_urls(out, m.get("header"));
            append_mm_block_image_urls(out, m.get("content"));
        }
        "column_set" => {
            let Some(columns) = array_at(m, "columns") else {
                return;
            };
            for column in columns {
                let Some(cm) = column.as_object() else {
                    continue;
                };
                let Some(items) = array_at(cm, "items") else {
                    continue;
                };
                for item in items {
                    append_mm_block_image_urls(out, Some(item));
                }
            }
        }
        _ => {}
    }
}

/// Port of `appendBlockKitImageURLs` (post_interactive_blocks.go:270).
///
/// A `section`'s accessory contributes only when its own type is `image`, and the key is
/// `image_url` — the mm_blocks dialect uses `url` for the same thing. Go's `continue` on a
/// non-image accessory skips the rest of *that block* only, so later blocks are still walked.
fn append_block_kit_image_urls(out: &mut Vec<String>, raw: Option<&Value>) {
    let Some(blocks) = interactive_prop_json_array(raw) else {
        return;
    };

    for block in blocks {
        let Some(m) = block.as_object() else {
            continue;
        };
        match block_type(m) {
            "section" => {
                if let Some(accessory) = object_at(m, "accessory") {
                    if block_type(accessory) != "image" {
                        continue;
                    }
                    if let Some(u) = string_at(accessory, "image_url") {
                        out.push(u.to_string());
                    }
                }
            }
            "image" => {
                if let Some(u) = string_at(m, "image_url") {
                    out.push(u.to_string());
                }
            }
            _ => {}
        }
    }
}

/// Port of `appendAdaptiveCardImageURLs` (post_interactive_blocks.go:302).
fn append_adaptive_card_image_urls(out: &mut Vec<String>, raw: Option<&Value>) {
    let Some(cards) = interactive_prop_json_array(raw) else {
        return;
    };

    for card in cards {
        let Some(cm) = card.as_object() else {
            continue;
        };
        let Some(body) = array_at(cm, "body") else {
            continue;
        };
        for item in body {
            append_adaptive_card_image_urls_from_item(out, item);
        }
    }
}

/// Port of `appendAdaptiveCardImageURLsFromItem` (post_interactive_blocks.go:323).
///
/// The key is `url`, not `image_url` — the third spelling in three dialects.
fn append_adaptive_card_image_urls_from_item(out: &mut Vec<String>, item: &Value) {
    let Some(m) = item.as_object() else {
        return;
    };

    match block_type(m) {
        "Container" => {
            if let Some(items) = array_at(m, "items") {
                for nested in items {
                    append_adaptive_card_image_urls_from_item(out, nested);
                }
            }
        }
        "ColumnSet" => {
            if let Some(columns) = array_at(m, "columns") {
                for column in columns {
                    let Some(column_map) = column.as_object() else {
                        continue;
                    };
                    let Some(items) = array_at(column_map, "items") else {
                        continue;
                    };
                    for nested in items {
                        append_adaptive_card_image_urls_from_item(out, nested);
                    }
                }
            }
        }
        "Image" => {
            if let Some(u) = string_at(m, "url") {
                out.push(u.to_string());
            }
        }
        _ => {}
    }
}

/// Port of `appendAttachmentsImageURLs` (post_interactive_blocks.go:360).
///
/// Four fields in a fixed order, each skipped when **empty** — the one walker in the file that
/// tests for an empty string rather than emitting it.
fn append_attachments_image_urls(out: &mut Vec<String>, attachments: &[MessageAttachment]) {
    for attachment in attachments {
        for url in [
            &attachment.image_url,
            &attachment.thumb_url,
            &attachment.author_icon,
            &attachment.footer_icon,
        ] {
            if !url.is_empty() {
                out.push(url.clone());
            }
        }
    }
}

/// Port of the body of `(*Post).InteractiveBlocksImageURLs` (post.go:846), which lives in post.go
/// but reads only this file's walkers.
///
/// `mm_blocks_enabled` gates **all three** block dialects, not just mm_blocks — the parameter
/// name is narrower than its effect. Attachment URLs are collected either way.
pub(crate) fn interactive_blocks_image_urls(post: &Post, mm_blocks_enabled: bool) -> Vec<String> {
    let Some(props) = post.get_props() else {
        return Vec::new();
    };

    let mut out = Vec::new();
    if mm_blocks_enabled {
        append_mm_block_image_urls(&mut out, props.get(POST_PROPS_MM_BLOCKS));
        append_block_kit_image_urls(&mut out, props.get(POST_PROPS_BLOCK_KIT_BLOCKS));
        append_adaptive_card_image_urls(&mut out, props.get(POST_PROPS_ADAPTIVE_CARDS));
    }
    append_attachments_image_urls(&mut out, &post.attachments());
    out
}

#[cfg(test)]
mod go_parity {
    use super::*;
    use crate::post::AllStringsOptions;

    fn oracle() -> Value {
        serde_json::from_str(include_str!(
            "../../../fixtures/behaviour_post_interactive_blocks.json"
        ))
        .unwrap()
    }

    fn cases(section: &str) -> Vec<Value> {
        oracle().get(section).unwrap().as_array().unwrap().to_vec()
    }

    fn s(v: &Value, key: &str) -> String {
        v.get(key).unwrap().as_str().unwrap().to_string()
    }

    /// Go's `[]string` is nil when nothing was appended and marshals as `null`; ours is an empty
    /// `Vec`.
    fn go_strings(v: &Value, key: &str) -> Vec<String> {
        match v.get(key).unwrap() {
            Value::Null => Vec::new(),
            Value::Array(items) => items
                .iter()
                .map(|i| i.as_str().unwrap().to_string())
                .collect(),
            other => panic!("not a string list: {other}"),
        }
    }

    fn post_from(v: &Value, key: &str) -> Post {
        serde_json::from_str(&s(v, key)).unwrap()
    }

    #[test]
    fn the_human_strings_walkers_match_go() {
        for case in cases("human_strings") {
            let name = s(&case, "name");
            let post = post_from(&case, "post");

            assert_eq!(
                post.all_strings(AllStringsOptions {
                    omit_interactive_blocks: false
                }),
                go_strings(&case, "full"),
                "{name}"
            );
            assert_eq!(
                post.all_strings(AllStringsOptions {
                    omit_interactive_blocks: true
                }),
                go_strings(&case, "omitting"),
                "{name}"
            );
        }
    }

    /// [D-041] is paid off: the corpus that used to differ under the two option values now agrees
    /// with Go under both. The four cases from `behaviour_post_attachments.json` that recorded
    /// the gap are re-asserted here against the **full** answer.
    #[test]
    fn the_interactive_half_of_all_strings_is_no_longer_a_gap() {
        let attachments_oracle: Value = serde_json::from_str(include_str!(
            "../../../fixtures/behaviour_post_attachments.json"
        ))
        .unwrap();

        let mut checked = 0;
        for case in attachments_oracle
            .get("all_strings")
            .unwrap()
            .as_array()
            .unwrap()
        {
            if !case.get("differs").unwrap().as_bool().unwrap() {
                continue;
            }
            checked += 1;
            let post = post_from(case, "post");
            assert_eq!(
                post.all_strings(AllStringsOptions {
                    omit_interactive_blocks: false
                }),
                go_strings(case, "full"),
                "{}",
                s(case, "name")
            );
        }
        assert_eq!(checked, 4);
    }

    #[test]
    fn the_image_url_walkers_match_go() {
        for case in cases("image_urls") {
            let name = s(&case, "name");
            let post = post_from(&case, "post");

            assert_eq!(
                post.interactive_blocks_image_urls(true),
                go_strings(&case, "enabled"),
                "{name}"
            );
            assert_eq!(
                post.interactive_blocks_image_urls(false),
                go_strings(&case, "disabled"),
                "{name}"
            );
        }
    }

    /// The two `column_set` walkers disagree, and the image one is almost certainly wrong: it
    /// re-tests each *item* as an array, so the ordinary shape yields nothing and an array of
    /// arrays is what works. Reproduced deliberately — see [D-045].
    #[test]
    fn the_two_column_set_walkers_disagree_the_way_go_does() {
        let flat = cases("image_urls")
            .into_iter()
            .find(|c| s(c, "name") == "mm_column_set_flat_items")
            .unwrap();
        assert!(go_strings(&flat, "enabled").is_empty());
        assert!(
            post_from(&flat, "post")
                .interactive_blocks_image_urls(true)
                .is_empty()
        );

        let nested = cases("image_urls")
            .into_iter()
            .find(|c| s(c, "name") == "mm_column_set_nested_items")
            .unwrap();
        assert_eq!(go_strings(&nested, "enabled"), ["https://x/a.png"]);
        assert_eq!(
            post_from(&nested, "post").interactive_blocks_image_urls(true),
            ["https://x/a.png"]
        );

        // The same flat shape is what the human-strings walker requires.
        let human = cases("human_strings")
            .into_iter()
            .find(|c| s(c, "name") == "mm_column_set")
            .unwrap();
        assert_eq!(go_strings(&human, "full"), ["c1", "c2"]);
    }

    /// `mm_blocks_enabled` gates Block Kit and Adaptive Cards too, and gates nothing about
    /// attachments.
    #[test]
    fn the_flag_gates_all_three_dialects_and_no_attachments() {
        let case = cases("image_urls")
            .into_iter()
            .find(|c| s(c, "name") == "blocks_then_attachments")
            .unwrap();

        assert_eq!(
            go_strings(&case, "enabled"),
            [
                "https://x/mm.png",
                "https://x/kit.png",
                "https://x/card.png",
                "https://x/att.png",
            ]
        );
        assert_eq!(go_strings(&case, "disabled"), ["https://x/att.png"]);

        let post = post_from(&case, "post");
        assert_eq!(
            post.interactive_blocks_image_urls(true),
            go_strings(&case, "enabled")
        );
        assert_eq!(
            post.interactive_blocks_image_urls(false),
            go_strings(&case, "disabled")
        );
    }

    /// An empty `url` on a block is emitted; an empty one on an attachment is not.
    #[test]
    fn an_empty_url_is_emitted_by_the_block_walker_and_skipped_by_the_attachment_one() {
        let block = cases("image_urls")
            .into_iter()
            .find(|c| s(c, "name") == "mm_image_url_empty")
            .unwrap();
        assert_eq!(go_strings(&block, "enabled"), [""]);
        assert_eq!(
            post_from(&block, "post").interactive_blocks_image_urls(true),
            [""]
        );

        let attachment = cases("image_urls")
            .into_iter()
            .find(|c| s(c, "name") == "attachment_empty_strings_skipped")
            .unwrap();
        assert_eq!(go_strings(&attachment, "enabled"), ["https://x/t.png"]);
        assert_eq!(
            post_from(&attachment, "post").interactive_blocks_image_urls(true),
            ["https://x/t.png"]
        );
    }
}
