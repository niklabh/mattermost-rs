//! Cross-server parity for `GET /api/v4/emoji/{emoji_id}` and
//! `GET /api/v4/emoji/name/{emoji_name}`.
//!
//! ```sh
//! docker compose up -d
//! scripts/parity.sh -p mm-api --test parity emoji_get
//! ```
//!
//! # The routing question is the whole route
//!
//! Both handlers are three lines around one store read. What is not obvious is which requests
//! reach them: gorilla registers the `/emoji` subrouter **before** `/emoji/{emoji_id}`, so its
//! literals win, while axum only prefers literals it has been given. So the suite spends most of
//! its assertions on the siblings — `autocomplete` is served by its own route, `names` and `search` must
//! *not* be (they are POST-only in Go and fall through to `getEmoji`, which 400s), and
//! `/emoji/name/x` must not be read as an emoji with the id `name`.
//!
//! # Go's cache is the reason every emoji here is named with a timestamp
//!
//! `LocalCacheEmojiStore` memoises `GetByName` for thirty minutes and the SQL purge below
//! deletes rows Go never hears about. See `common::create_custom_emoji`.

use crate::common;

use common::{
    GO, RUST, assert_error_bodies_match_except_known_gaps, client, create_custom_emoji,
    delete_custom_emoji, fetch_both, fetch_both_raw, go_minted_token, logged_in_user_id,
    purge_api_fixtures, stack_enabled, unique_emoji_name,
};

// ---------------------------------------------------------------------------------------------
// fixtures
// ---------------------------------------------------------------------------------------------

struct Fixture {
    /// A live custom emoji.
    live_id: String,
    live_name: String,
    /// One that has been deleted through Go's API — the row survives with `DeleteAt` set.
    gone_id: String,
    gone_name: String,
}

static FIXTURE: tokio::sync::OnceCell<Fixture> = tokio::sync::OnceCell::const_new();

async fn fixture(client: &reqwest::Client, token: &str) -> &'static Fixture {
    FIXTURE
        .get_or_init(|| async {
            purge_api_fixtures().await;
            let creator = logged_in_user_id();

            let live_name = unique_emoji_name("live");
            let live_id = create_custom_emoji(client, token, creator, &live_name).await;

            let gone_name = unique_emoji_name("gone");
            let gone_id = create_custom_emoji(client, token, creator, &gone_name).await;
            delete_custom_emoji(client, token, &gone_id).await;

            Fixture {
                live_id,
                live_name,
                gone_id,
                gone_name,
            }
        })
        .await
}

// ---------------------------------------------------------------------------------------------
// getEmoji
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn an_emoji_by_id_is_byte_identical() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let path = format!("/api/v4/emoji/{}", f.live_id);
    let (go, rs) = fetch_both(&client, &token, &path).await;
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{path} must be byte-identical"
    );
    assert!(
        go.ends_with(b"\n"),
        "json.NewEncoder().Encode adds a trailing newline"
    );

    // Every field must carry something, or the comparison is between two sets of zeroes.
    let parsed: serde_json::Value = serde_json::from_slice(&go).expect("JSON");
    for key in [
        "id",
        "create_at",
        "update_at",
        "delete_at",
        "creator_id",
        "name",
    ] {
        assert!(parsed.get(key).is_some(), "{key} must be on the wire");
    }
    assert_eq!(parsed["name"], f.live_name.as_str());
    assert_eq!(parsed["delete_at"], 0);
    assert_ne!(parsed["create_at"], 0);
}

/// The shared select builder's `DeleteAt = 0`, which lives nowhere near either method's body.
#[tokio::test]
async fn a_deleted_emoji_is_a_404_by_id_on_both() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    // The row is still there — otherwise this asserts nothing about the predicate.
    if let Ok(url) = std::env::var("DATABASE_URL")
        && let Ok(pool) = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(std::time::Duration::from_secs(5))
            .connect(&url)
            .await
    {
        let row: (i64,) = sqlx::query_as("SELECT deleteat FROM emoji WHERE id = $1")
            .bind(&f.gone_id)
            .fetch_one(&pool)
            .await
            .expect("the soft-deleted row is still there");
        assert_ne!(row.0, 0, "Go soft-deletes an emoji; the row must survive");
    }

    let path = format!("/api/v4/emoji/{}", f.gone_id);
    let ((go_status, go_body), (rs_status, rs_body)) = fetch_both_raw(&client, &token, &path).await;
    assert_eq!(go_status, 404, "{path}");
    assert_eq!(rs_status, go_status);
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &path);
    assert_eq!(go["id"], "app.emoji.get.no_result");
}

#[tokio::test]
async fn an_unknown_id_is_a_404_on_both() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;

    let path = "/api/v4/emoji/aaaaaaaaaaaaaaaaaaaaaaaaaa";
    let ((go_status, go_body), (rs_status, rs_body)) = fetch_both_raw(&client, &token, path).await;
    assert_eq!(go_status, 404);
    assert_eq!(rs_status, go_status);
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, path);
    assert_eq!(go["id"], "app.emoji.get.no_result");
}

/// `RequireEmojiId` — and, for `names`/`search`, the proof that gorilla really does fall through
/// a POST-only literal into `getEmoji` rather than answering 405.
#[tokio::test]
async fn a_bad_id_is_the_same_400_on_both_including_the_post_only_literals() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;

    for segment in [
        "short",
        "aaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "names",
        "search",
        "name",
    ] {
        let path = format!("/api/v4/emoji/{segment}");
        let ((go_status, go_body), (rs_status, rs_body)) =
            fetch_both_raw(&client, &token, &path).await;
        assert_eq!(go_status, 400, "{path} should be an invalid emoji_id");
        assert_eq!(rs_status, go_status, "{path}: statuses must match");
        let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &path);
        assert_eq!(
            go["id"], "api.context.invalid_url_param.app_error",
            "{path}"
        );
    }
}

/// The one literal gorilla's registration order takes away from `{emoji_id}` — now registered
/// here too, so axum takes it away for the same reason and answers it from Rust.
///
/// Kept in *this* file because what it guards is `getEmoji`'s routing, not autocomplete's
/// behaviour: if `/emoji/autocomplete` ever landed on `{emoji_id}` again it would 400 where Go
/// returns a list. The route's own contract lives in `parity/emoji_autocomplete.rs`.
#[tokio::test]
async fn the_autocomplete_literal_does_not_land_on_get_emoji() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;

    // A filter no fixture emoji can match. `?name=mmrs` matched whatever another
    // suite had just created and the two servers were asked seconds apart, so the bodies
    // differed for a reason that had nothing to do with routing.
    let path = "/api/v4/emoji/autocomplete?name=zzznosuchemojiprefix";
    let go = client
        .get(format!("{GO}{path}"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("Go answers");
    let rs = client
        .get(format!("{RUST}{path}"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("we answer");

    assert_eq!(
        rs.headers()
            .get("x-mmrs-served-by")
            .and_then(|v| v.to_str().ok()),
        Some("rust"),
        "{path} must reach the autocomplete handler, not `getEmoji` with emoji_id=autocomplete"
    );
    assert_eq!(
        go.status(),
        200,
        "if Go stops serving autocomplete, this test is wrong for a new reason"
    );
    assert_eq!(go.status(), rs.status());
    assert_eq!(
        go.bytes().await.expect("body"),
        rs.bytes().await.expect("body")
    );
}

/// One segment deeper, and now served here — `mm_api::images::get_emoji_image` reads the bytes
/// out of the local file backend. Kept in this suite because it is the routing question the rest
/// of the module is about: `/emoji/{id}/image` must reach its own handler and not `getEmoji`.
#[tokio::test]
async fn the_image_subroute_is_served_here() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let path = format!("/api/v4/emoji/{}/image", f.live_id);
    let go = client
        .get(format!("{GO}{path}"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("Go answers");
    let rs = client
        .get(format!("{RUST}{path}"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("we answer");
    assert_eq!(
        rs.headers()
            .get("x-mmrs-served-by")
            .and_then(|v| v.to_str().ok()),
        Some("rust"),
        "{path} is served here now — the file backend landed with it"
    );
    assert_eq!(go.status(), rs.status(), "{path}: statuses must match");
}

// ---------------------------------------------------------------------------------------------
// getEmojiByName
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn an_emoji_by_name_is_byte_identical() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let path = format!("/api/v4/emoji/name/{}", f.live_name);
    let (go, rs) = fetch_both(&client, &token, &path).await;
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{path} must be byte-identical"
    );

    // The two routes answer the *same* emoji, which is what makes reading the wrong column
    // visible: a `get_by_name` that queried `id` would 404 here rather than returning a
    // different row.
    let by_id = format!("/api/v4/emoji/{}", f.live_id);
    let (go_by_id, _) = fetch_both(&client, &token, &by_id).await;
    assert_eq!(go, go_by_id, "both routes name one row");
}

#[tokio::test]
async fn a_deleted_emoji_is_a_404_by_name_on_both() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let path = format!("/api/v4/emoji/name/{}", f.gone_name);
    let ((go_status, go_body), (rs_status, rs_body)) = fetch_both_raw(&client, &token, &path).await;
    assert_eq!(go_status, 404, "{path}");
    assert_eq!(rs_status, go_status);
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &path);
    assert_eq!(
        go["id"], "app.emoji.get_by_name.no_result",
        "the by-name route has its own error id — a shared helper would emit the by-id one"
    );
}

/// A **system** emoji name is not in the table at all: `getEmojiByName` reads `Emoji` and
/// nothing else, so `:grinning:` is a 404 even though every client renders it.
#[tokio::test]
async fn a_system_emoji_name_is_a_404_on_both() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;

    let path = "/api/v4/emoji/name/grinning";
    let ((go_status, go_body), (rs_status, rs_body)) = fetch_both_raw(&client, &token, path).await;
    assert_eq!(go_status, 404, "system emoji are not rows");
    assert_eq!(rs_status, go_status);
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, path);
    assert_eq!(go["id"], "app.emoji.get_by_name.no_result");
}

/// Inside the mux charset, past the handler's 64-byte limit: a 400 from both, not a forward.
#[tokio::test]
async fn a_name_over_sixty_four_bytes_is_a_400_on_both() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;

    let sixty_five = "a".repeat(65);
    let path = format!("/api/v4/emoji/name/{sixty_five}");
    let ((go_status, go_body), (rs_status, rs_body)) = fetch_both_raw(&client, &token, &path).await;
    assert_eq!(go_status, 400, "{path}");
    assert_eq!(rs_status, go_status);
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &path);
    assert_eq!(go["id"], "api.context.invalid_url_param.app_error");

    // And exactly 64 is *not* a 400 — it is a 404, because no such emoji exists. The boundary
    // has to be checked from both sides or an off-by-one passes.
    let sixty_four = "a".repeat(64);
    let path = format!("/api/v4/emoji/name/{sixty_four}");
    let ((go_status, _), (rs_status, _)) = fetch_both_raw(&client, &token, &path).await;
    assert_eq!(go_status, 404, "64 bytes is inside the limit");
    assert_eq!(rs_status, go_status);
}

/// The `+` in `+1` is a legal emoji-name character and a legal path character, and it must not
/// be read as a space. Go's mux class allows it explicitly.
#[tokio::test]
async fn a_plus_in_the_name_survives_the_path() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;

    let path = "/api/v4/emoji/name/+1";
    let ((go_status, go_body), (rs_status, rs_body)) = fetch_both_raw(&client, &token, path).await;
    assert_eq!(
        go_status, 404,
        "`+1` is a system emoji, so it is not a row — but it is a *valid name*, which is the point"
    );
    assert_eq!(rs_status, go_status);
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, path);
    assert_eq!(
        go["id"], "app.emoji.get_by_name.no_result",
        "a 400 here would mean the `+` was rejected by the name check"
    );
}

/// Outside gorilla's `[A-Za-z0-9\\_\\-\\+]+`, so Go's mux never matched the route: forwarded.
#[tokio::test]
async fn a_name_outside_the_mux_charset_is_forwarded() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;

    let path = "/api/v4/emoji/name/has.dot";
    let go = client
        .get(format!("{GO}{path}"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("Go answers");
    let rs = client
        .get(format!("{RUST}{path}"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("we answer");

    assert_eq!(
        rs.headers()
            .get("x-mmrs-served-by")
            .and_then(|v| v.to_str().ok()),
        Some("go"),
        "{path} was a mux 404 in Go, so it must be forwarded rather than 400ed"
    );
    assert_eq!(go.status(), rs.status(), "{path}: statuses must match");
    assert_eq!(
        go.bytes().await.expect("body"),
        rs.bytes().await.expect("body")
    );
}

/// The methods these two paths do **not** claim stay Go's, and the one this server now claims —
/// `DELETE /emoji/{emoji_id}` — is proved on a **throwaway** emoji.
///
/// The obvious version of this test is destructive and quietly so: the delete really does run,
/// and the fixture emoji the rest of the module reads is soft-deleted out from under it. That is
/// what happened on the first run — every other test kept passing because Go's by-name cache
/// still answered, and only the by-id read noticed. So the delete gets its own emoji, and "the
/// emoji is really gone" is what turns a 200 into evidence.
///
/// The delete's own cross-server assertions live in `parity::emoji_writes`; what is checked here
/// is that claiming it did not disturb the two reads this module owns.
#[tokio::test]
async fn other_methods_are_forwarded() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    // Non-destructive on every path: Go registers no `PUT` under `/emoji`, so both are mux 404s
    // there and forwarding is all that is being checked.
    for path in [
        format!("/api/v4/emoji/{}", f.live_id),
        format!("/api/v4/emoji/name/{}", f.live_name),
    ] {
        for method in [reqwest::Method::PUT, reqwest::Method::POST] {
            let rs = client
                .request(method.clone(), format!("{RUST}{path}"))
                .header("Authorization", format!("Bearer {token}"))
                .send()
                .await
                .expect("we answer");
            assert_eq!(
                rs.headers()
                    .get("x-mmrs-served-by")
                    .and_then(|v| v.to_str().ok()),
                Some("go"),
                "{method} {path} must be forwarded"
            );
        }
    }

    // The destructive one, on an emoji created for it.
    let doomed_name = unique_emoji_name("doomed");
    let doomed_id = create_custom_emoji(&client, &token, logged_in_user_id(), &doomed_name).await;
    let path = format!("/api/v4/emoji/{doomed_id}");
    let rs = client
        .delete(format!("{RUST}{path}"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("we answer");
    assert_eq!(
        rs.headers()
            .get("x-mmrs-served-by")
            .and_then(|v| v.to_str().ok()),
        Some("rust"),
        "DELETE {path} is served here now"
    );
    assert_eq!(rs.status(), 200, "deleteEmoji answers OK");

    // And it really deleted, rather than answering a 200-shaped thing.
    let ((go_status, _), (rs_status, _)) = fetch_both_raw(&client, &token, &path).await;
    assert_eq!(go_status, 404, "the emoji is gone from Go's own read");
    assert_eq!(rs_status, 404, "and from ours");
}

/// An unauthenticated request. Both routes are `APISessionRequired`, so neither may leak a row
/// to a caller with no token.
#[tokio::test]
async fn no_token_is_a_401_on_both() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    for path in [
        format!("/api/v4/emoji/{}", f.live_id),
        format!("/api/v4/emoji/name/{}", f.live_name),
    ] {
        let go = client
            .get(format!("{GO}{path}"))
            .send()
            .await
            .expect("Go answers");
        let rs = client
            .get(format!("{RUST}{path}"))
            .send()
            .await
            .expect("we answer");
        assert_eq!(go.status(), 401, "{path}");
        assert_eq!(rs.status(), go.status(), "{path}: statuses must match");
    }
}
