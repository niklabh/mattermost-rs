//! Cross-server parity for `GET /api/v4/emoji` — `getEmojiList`.
//!
//! ```sh
//! docker compose up -d
//! scripts/parity.sh -p mm-api --test parity emoji_list
//! ```
//!
//! # The default page has no `ORDER BY`
//!
//! Go emits one only for `?sort=name`. Two servers reading the same table with the same
//! `LIMIT`/`OFFSET` and no ordering are entitled to disagree about row order, so the unsorted
//! page is compared as a **set** and the sorted page byte for byte. Asserting bytes on the
//! unsorted one would be asserting something neither server promises — the mistake
//! `teams_for_user` made and had to have repaired.
//!
//! # The emoji table is shared with every other suite
//!
//! `emoji_get` and `post_get` both create custom emoji, and this route lists *all* of them. So
//! nothing here asserts a count or a page boundary against the whole table; the assertions are
//! about the fixture's own rows being present, in the right order relative to each other, and
//! about the two servers agreeing.

use crate::common;

use common::{
    GO, RUST, assert_error_bodies_match_except_known_gaps, client, create_custom_emoji,
    delete_custom_emoji, fetch_both_raw, fetch_both_stable, go_minted_token, logged_in_user_id,
    purge_api_fixtures, stack_enabled, unique_emoji_name,
};

// ---------------------------------------------------------------------------------------------
// fixtures
// ---------------------------------------------------------------------------------------------

struct Fixture {
    /// Three live emoji whose names sort in a known order, and one deleted.
    names: [String; 3],
    gone_name: String,
}

static FIXTURE: tokio::sync::OnceCell<Fixture> = tokio::sync::OnceCell::const_new();

async fn fixture(client: &reqwest::Client, token: &str) -> &'static Fixture {
    FIXTURE
        .get_or_init(|| async {
            purge_api_fixtures().await;
            let creator = logged_in_user_id();

            // **Created in reverse name order, and that is load-bearing.** With only this
            // suite's rows in the table — which is what a filtered mutation run leaves — an
            // unordered `SELECT` returns them in insertion order, so creating `lista` first
            // would make the unsorted page *already* sorted and `?sort=name` a no-op. Three
            // mutations (dropping the `ORDER BY`, swapping it for `createat`, and never passing
            // the flag) all survived against a fixture built in alphabetical order.
            let mut names = [
                unique_emoji_name("lista"),
                unique_emoji_name("listb"),
                unique_emoji_name("listc"),
            ];
            for name in names.iter().rev() {
                create_custom_emoji(client, token, creator, name).await;
            }
            names.sort();

            let gone_name = unique_emoji_name("listgone");
            let gone_id = create_custom_emoji(client, token, creator, &gone_name).await;
            delete_custom_emoji(client, token, &gone_id).await;

            Fixture { names, gone_name }
        })
        .await
}

/// Every emoji the two servers return for `path`, as `(names, bodies)`.
///
/// **Bracketed**, because this route lists the *whole* emoji table and the other emoji suites
/// create and soft-delete rows in it throughout the run. An unbracketed pair of reads straddles
/// one of those deletes often enough to fail — [D-160]'s shape, seen here as one side carrying a
/// `mmrsparitydoomed` row the other had already lost.
async fn names_from_both(
    client: &reqwest::Client,
    token: &str,
    path: &str,
) -> (Vec<String>, Vec<String>) {
    let (go, rs) = fetch_both_stable(client, token, path).await;
    let parse = |body: &[u8]| -> Vec<String> {
        serde_json::from_slice::<serde_json::Value>(body)
            .expect("JSON")
            .as_array()
            .expect("an array")
            .iter()
            .map(|e| e["name"].as_str().expect("a name").to_owned())
            .collect()
    };
    (parse(&go), parse(&rs))
}

// ---------------------------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------------------------

/// `?sort=name` is the only ordered page, so it is the only one that can be compared as bytes.
#[tokio::test]
async fn the_sorted_page_is_byte_identical() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let path = "/api/v4/emoji?sort=name&per_page=200";
    let (go, rs) = fetch_both_stable(&client, &token, path).await;
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{path} must be byte-identical"
    );
    assert!(
        go.ends_with(b"\n"),
        "json.NewEncoder().Encode adds a trailing newline"
    );

    let names: Vec<String> = serde_json::from_slice::<serde_json::Value>(&go)
        .expect("JSON")
        .as_array()
        .expect("an array")
        .iter()
        .map(|e| e["name"].as_str().expect("a name").to_owned())
        .collect();

    let mut sorted = names.clone();
    sorted.sort();
    assert_eq!(names, sorted, "ORDER BY Name, and it really is applied");

    // The fixture's three, in order, somewhere in the page.
    let positions: Vec<usize> = f
        .names
        .iter()
        .map(|n| {
            names
                .iter()
                .position(|got| got == n)
                .unwrap_or_else(|| panic!("{n} must be listed"))
        })
        .collect();
    assert!(
        positions[0] < positions[1] && positions[1] < positions[2],
        "the fixture's names must appear in sorted order: {positions:?}"
    );
}

/// No `sort` means no `ORDER BY`, so this compares the two answers as sets. A byte comparison
/// here would be asserting an ordering neither server promises.
#[tokio::test]
async fn the_unsorted_page_holds_the_same_emoji_on_both() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let path = "/api/v4/emoji?per_page=200";
    let (mut go, mut rs) = names_from_both(&client, &token, path).await;
    go.sort();
    rs.sort();
    assert_eq!(go, rs, "{path}: the same set, whatever the row order");

    for name in &f.names {
        assert!(go.contains(name), "{name} must be listed");
    }
    assert!(
        !go.contains(&f.gone_name),
        "the select builder's DeleteAt = 0 hides a soft-deleted emoji"
    );
}

/// `?sort=` is the same request as no `sort` at all — the guard is `sort != "" && sort != "name"`.
#[tokio::test]
async fn an_empty_sort_is_the_unsorted_page() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let _ = fixture(&client, &token).await;

    let path = "/api/v4/emoji?sort=&per_page=200";
    let (mut go, mut rs) = names_from_both(&client, &token, path).await;
    go.sort();
    rs.sort();
    assert_eq!(go, rs);
}

/// Anything else is a 400, case included — `Name` is not `name`.
#[tokio::test]
async fn an_unknown_sort_is_a_400_on_both() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let _ = fixture(&client, &token).await;

    for sort in ["Name", "NAME", "created_at", "id", "name%20"] {
        let path = format!("/api/v4/emoji?sort={sort}");
        let ((go_status, go_body), (rs_status, rs_body)) =
            fetch_both_raw(&client, &token, &path).await;
        assert_eq!(go_status, 400, "{path}");
        assert_eq!(rs_status, go_status, "{path}");
        let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &path);
        assert_eq!(go["id"], "api.context.invalid_url_param.app_error");
    }
}

/// `per_page=0` reaches the store as `LIMIT 0` and answers the empty list — *not* "no limit",
/// which is what a zero means to the channel and post pagination helpers.
#[tokio::test]
async fn per_page_zero_is_an_empty_page_not_an_unlimited_one() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let _ = fixture(&client, &token).await;

    let path = "/api/v4/emoji?per_page=0";
    let (go, rs) = fetch_both_stable(&client, &token, path).await;
    assert_eq!(go, b"[]\n", "LIMIT 0, and an initialised slice — not null");
    assert_eq!(rs, go);
}

/// Pagination is `web.ParamsFromRequest`'s: garbage and negatives fall to the defaults rather
/// than 400, and the offset is `page * per_page`.
#[tokio::test]
async fn pagination_clamps_rather_than_refusing() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let _ = fixture(&client, &token).await;

    // Garbage and negatives are the default page.
    let default_page = {
        let (mut go, mut rs) = names_from_both(&client, &token, "/api/v4/emoji?sort=name").await;
        go.sort();
        rs.sort();
        assert_eq!(go, rs);
        go
    };
    for query in ["page=-1", "page=abc", "per_page=-5", "per_page=notanumber"] {
        let path = format!("/api/v4/emoji?sort=name&{query}");
        let (go, rs) = fetch_both_stable(&client, &token, &path).await;
        assert_eq!(
            String::from_utf8_lossy(&go),
            String::from_utf8_lossy(&rs),
            "{path}"
        );
    }

    // The offset is `page * per_page`, and the page size has to be **more than one** to say so:
    // with `per_page=1` the product equals `page` and dropping the multiplication changes
    // nothing. That is precisely how `emojilist-offset` survived its first run.
    let (all, _) = names_from_both(&client, &token, "/api/v4/emoji?sort=name&per_page=200").await;
    assert!(
        all.len() >= 4,
        "the fixture leaves at least four emoji: {all:?}"
    );

    let (second_page, _) =
        names_from_both(&client, &token, "/api/v4/emoji?sort=name&per_page=2&page=1").await;
    assert_eq!(
        second_page,
        all[2..4].to_vec(),
        "page 1 of size 2 starts at offset 2, not at offset 1"
    );

    assert!(!default_page.is_empty(), "the fixture emoji are listed");
}

/// The bare collection is one segment shorter than `/emoji/{emoji_id}`, so the two never collide
/// — and only `GET` is migrated.
#[tokio::test]
async fn the_collection_does_not_shadow_the_single_reads() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let path = format!("/api/v4/emoji/name/{}", f.names[0]);
    let (go, rs) = fetch_both_stable(&client, &token, &path).await;
    assert_eq!(go, rs, "{path} is still getEmojiByName");

    // `POST /api/v4/emoji` is createEmoji and must still be Go's.
    let response = client
        .post(format!("{RUST}/api/v4/emoji"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("reachable");
    assert_eq!(
        response
            .headers()
            .get("x-mmrs-served-by")
            .and_then(|v| v.to_str().ok()),
        Some("go"),
        "only GET /emoji is migrated"
    );

    // `/emoji/autocomplete` used to be asserted here as forwarded. It is a route of its own
    // now — see `parity/emoji_autocomplete.rs`. `/emoji/names` and `/emoji/search` are POST in
    // Go and are still nobody's here, which the GET above already covers.

    let _ = GO;
}
