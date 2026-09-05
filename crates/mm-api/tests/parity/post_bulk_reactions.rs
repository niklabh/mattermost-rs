//! Cross-server parity for `POST /api/v4/posts/ids/reactions` (`getBulkReactions`).
//!
//! ```sh
//! docker compose up -d
//! scripts/parity.sh -p mm-api --test parity post_bulk_reactions
//! ```
//!
//! # This route is the opposite of the one beside it
//!
//! `GET /posts/{id}/reactions` answers `null` for a post with no reactions, because Go's slice
//! is nil. This one answers `[]`, because `populateEmptyReactions` (app/reaction.go:148) writes
//! a literal empty slice into the map for every requested id. Same absence, different bytes, and
//! only the running server settles it — [`an_empty_value_is_an_empty_array_not_null`] is the
//! test that does.
//!
//! # And it validates nothing
//!
//! No `RequirePostId`, no `IsValidId`, and — alone among api4's by-ids handlers — no empty-list
//! check. `["abc"]` is a legal request. `[]` and `null` are **500s**, because Go's store builds
//! `PostId IN ()` and Postgres will not parse it. Both are ported deliberately; see the handler.

use crate::common;

use common::{
    GO, RUST, add_user_to_channel, assert_error_bodies_match_except_known_gaps, client,
    create_channel, create_channel_typed, create_plain_user, create_team, go_minted_token,
    logged_in_user_id, post_both_raw, post_message, purge_api_fixtures, stack_enabled,
};

// ---------------------------------------------------------------------------------------------
// fixtures
// ---------------------------------------------------------------------------------------------

struct Fixture {
    /// A post carrying three reactions from two users, in reverse emoji-name order.
    reacted: String,
    /// A second reacted post, so the grouping is doing real work rather than passing through.
    also_reacted: String,
    /// A post nobody has reacted to.
    bare: String,
    /// A post whose only reaction has been withdrawn — Go soft-deletes, so the row survives.
    withdrawn: String,
    /// A post whose one reaction has `UpdateAt` and `DeleteAt` set to **NULL** in the table.
    nulled: String,
    /// A post in a **private** channel the plain user is not in.
    unreadable: String,
    plain_token: String,
}

static FIXTURE: tokio::sync::OnceCell<Fixture> = tokio::sync::OnceCell::const_new();

async fn fixture(client: &reqwest::Client, token: &str) -> &'static Fixture {
    FIXTURE
        .get_or_init(|| async {
            purge_api_fixtures().await;

            let team_id = create_team(client, token, "bulkreactions").await;
            let channel_id = create_channel(client, token, &team_id, "bulkreactions").await;
            // Private: a public channel is readable by any team member through
            // `read_public_channel`, and the 403 this fixture exists to produce would be a 200.
            let closed_id =
                create_channel_typed(client, token, &team_id, "bulkreactshut", "P").await;

            let plain = create_plain_user(client, token, &team_id, "bulkreact").await;
            add_user_to_channel(client, token, &channel_id, &plain.id).await;

            let admin_id = logged_in_user_id();

            let reacted = post_message(client, token, &channel_id, "react to me", None).await;
            // Creation order is reverse-alphabetical by emoji name, so `ORDER BY CreateAt` and
            // `ORDER BY EmojiName` disagree and the assertion can tell them apart. The middle
            // reaction comes from the other user so `user_id` is not a proxy for the order
            // either.
            add_reaction(client, token, &reacted, admin_id, "grinning").await;
            wait_past_a_millisecond().await;
            add_reaction(client, &plain.token, &reacted, &plain.id, "eyes").await;
            wait_past_a_millisecond().await;
            add_reaction(client, token, &reacted, admin_id, "+1").await;

            // A different count in a different post: a port that returned every reaction under
            // one key, or the first post's list under both keys, fails on this.
            let also_reacted =
                post_message(client, token, &channel_id, "react to me too", None).await;
            add_reaction(client, token, &also_reacted, admin_id, "wave").await;

            let bare = post_message(client, token, &channel_id, "nobody reacted", None).await;

            let withdrawn = post_message(client, token, &channel_id, "reaction undone", None).await;
            add_reaction(client, token, &withdrawn, admin_id, "tada").await;
            remove_reaction(client, token, &withdrawn, admin_id, "tada").await;

            // The two `COALESCE`s in the query exist for rows written before the migration
            // that backfilled these columns, and **nothing reachable over REST produces one** —
            // Go's `SaveReaction` always writes both. So the coalesces are dead weight to any
            // test that goes through the API, and a mutation removing them survives. That was a
            // recorded survivor on the single-post route; here the row is planted directly, so
            // the branch is real and a mutation dies on it.
            let nulled = post_message(client, token, &channel_id, "nulls in the row", None).await;
            add_reaction(client, token, &nulled, admin_id, "ghost").await;
            plant_nulls(&nulled).await;

            let unreadable = post_message(client, token, &closed_id, "not for you", None).await;
            add_reaction(client, token, &unreadable, admin_id, "lock").await;

            Fixture {
                reacted,
                also_reacted,
                bare,
                withdrawn,
                nulled,
                unreadable,
                plain_token: plain.token,
            }
        })
        .await
}

/// Set `UpdateAt` and `DeleteAt` to NULL on every reaction of a post, which the REST API cannot
/// do. Silent when there is no `DATABASE_URL`, like every other direct-database step in the
/// suite; the test that depends on it re-checks the row and says so if the planting did not
/// happen.
async fn plant_nulls(post_id: &str) {
    let Ok(url) = std::env::var("DATABASE_URL") else {
        return;
    };
    let Ok(pool) = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(std::time::Duration::from_secs(5))
        .connect(&url)
        .await
    else {
        return;
    };
    sqlx::query("UPDATE reactions SET updateat = NULL, deleteat = NULL WHERE postid = $1")
        .bind(post_id)
        .execute(&pool)
        .await
        .expect("the update runs");
}

/// `CreateAt` is milliseconds, so two writes inside one tick tie and an unordered port passes.
async fn wait_past_a_millisecond() {
    tokio::time::sleep(std::time::Duration::from_millis(3)).await;
}

async fn add_reaction(
    client: &reqwest::Client,
    token: &str,
    post_id: &str,
    user_id: &str,
    emoji_name: &str,
) {
    let response = client
        .post(format!("{GO}/api/v4/reactions"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!({
            "user_id": user_id,
            "post_id": post_id,
            "emoji_name": emoji_name,
        }))
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "reacting with {emoji_name} failed: {}",
        response.text().await.unwrap_or_default()
    );
}

async fn remove_reaction(
    client: &reqwest::Client,
    token: &str,
    post_id: &str,
    user_id: &str,
    emoji_name: &str,
) {
    let response = client
        .delete(format!(
            "{GO}/api/v4/users/{user_id}/posts/{post_id}/reactions/{emoji_name}"
        ))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "withdrawing {emoji_name} failed: {}",
        response.text().await.unwrap_or_default()
    );
}

const PATH: &str = "/api/v4/posts/ids/reactions";

fn body(ids: &[&str]) -> Vec<u8> {
    serde_json::to_vec(&ids).expect("the id list serialises")
}

/// The byte offset of each key in the raw response, in the order they appear.
///
/// `serde_json::Value` is a map and forgets the order, so the sorted-keys claim cannot be made
/// against a parsed body. Go sorts map keys when it marshals; a `HashMap` on our side would
/// serialise in an arbitrary order and still parse equal.
fn key_order(raw: &[u8], ids: &[&str]) -> Vec<String> {
    let text = String::from_utf8_lossy(raw).into_owned();
    let mut seen: Vec<(usize, String)> = ids
        .iter()
        .filter_map(|id| {
            text.find(&format!("\"{id}\""))
                .map(|at| (at, (*id).to_owned()))
        })
        .collect();
    seen.sort_by_key(|(at, _)| *at);
    seen.into_iter().map(|(_, id)| id).collect()
}

// ---------------------------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------------------------

/// The whole route in one assertion: two reacted posts with different counts, one bare post,
/// requested in an order that is not the answer's order.
#[tokio::test]
async fn a_bulk_read_is_byte_identical() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    // Descending, so neither server can be answering in request order by accident.
    let mut ids = vec![f.reacted.as_str(), f.also_reacted.as_str(), f.bare.as_str()];
    ids.sort_by(|a, b| b.cmp(a));

    let ((go_status, go_body), (rs_status, rs_body)) =
        post_both_raw(&client, &token, PATH, &body(&ids)).await;

    assert_eq!(go_status, 200, "{}", String::from_utf8_lossy(&go_body));
    assert_eq!(rs_status, go_status, "{PATH}: statuses must match");
    assert_eq!(
        String::from_utf8_lossy(&go_body),
        String::from_utf8_lossy(&rs_body),
        "{PATH} must be byte-identical"
    );

    // The fixture is the one the claims need.
    let parsed: serde_json::Value = serde_json::from_slice(&go_body).expect("Go's body is JSON");
    let names: Vec<&str> = parsed[&f.reacted]
        .as_array()
        .expect("an array")
        .iter()
        .filter_map(|r| r["emoji_name"].as_str())
        .collect();
    assert_eq!(
        names,
        vec!["grinning", "eyes", "+1"],
        "reactions come back in creation order, which here is the reverse of emoji-name order"
    );
    assert_eq!(
        parsed[&f.also_reacted].as_array().expect("an array").len(),
        1,
        "the second post's reactions must not have been pooled with the first's"
    );

    // And the keys are sorted, not in request order.
    let mut expected: Vec<String> = ids.iter().map(|id| (*id).to_owned()).collect();
    expected.sort();
    assert_eq!(
        key_order(&go_body, &ids),
        expected,
        "Go marshals map keys in bytewise order"
    );
    assert_eq!(
        key_order(&rs_body, &ids),
        expected,
        "so the port must too — a HashMap would parse equal and serialise differently"
    );
}

/// The wire quirk this route exists to get right, and the one that differs from its neighbour.
#[tokio::test]
async fn an_empty_value_is_an_empty_array_not_null() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let ids = [f.bare.as_str()];
    let ((go_status, go_body), (_rs_status, rs_body)) =
        post_both_raw(&client, &token, PATH, &body(&ids)).await;

    assert_eq!(go_status, 200);
    assert_eq!(
        String::from_utf8_lossy(&go_body),
        format!("{{\"{}\":[]}}", f.bare),
        "populateEmptyReactions writes `[]`, not `null` and not a missing key"
    );
    assert_eq!(go_body, rs_body, "{PATH} must be byte-identical");

    // The contrast, on the same post, through the route next door.
    let single = client
        .get(format!("{GO}/api/v4/posts/{}/reactions", f.bare))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("Go answers")
        .bytes()
        .await
        .expect("body");
    assert_eq!(
        single.as_ref(),
        b"null",
        "the single-post route answers `null` for the same post — the two must not be unified"
    );
}

/// No id validation at all: a 26-character id that names nothing, and a three-character one
/// that could never be an id, both come back as keys.
#[tokio::test]
async fn every_requested_id_gets_a_key_even_when_it_names_nothing() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;

    let ids = ["aaaaaaaaaaaaaaaaaaaaaaaaaa", "abc"];
    let ((go_status, go_body), (rs_status, rs_body)) =
        post_both_raw(&client, &token, PATH, &body(&ids)).await;

    assert_eq!(
        go_status,
        200,
        "there is no RequirePostId on this route: {}",
        String::from_utf8_lossy(&go_body)
    );
    assert_eq!(rs_status, go_status);
    assert_eq!(
        String::from_utf8_lossy(&go_body),
        "{\"aaaaaaaaaaaaaaaaaaaaaaaaaa\":[],\"abc\":[]}",
    );
    assert_eq!(go_body, rs_body, "{PATH} must be byte-identical");
}

/// `COALESCE(DeleteAt, 0) = 0` in the bulk query. The row is still in the table to be wrongly
/// returned, which is what makes this an exclusion rather than an empty read.
#[tokio::test]
async fn a_withdrawn_reaction_is_excluded_by_both() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    if let Ok(url) = std::env::var("DATABASE_URL")
        && let Ok(pool) = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(std::time::Duration::from_secs(5))
            .connect(&url)
            .await
    {
        let rows: (i64,) = sqlx::query_as("SELECT count(*) FROM reactions WHERE postid = $1")
            .bind(&f.withdrawn)
            .fetch_one(&pool)
            .await
            .expect("the count runs");
        assert_eq!(
            rows.0, 1,
            "Go soft-deletes a reaction; if the row is gone this test proves nothing"
        );
    }

    let ids = [f.withdrawn.as_str()];
    let ((go_status, go_body), (_rs, rs_body)) =
        post_both_raw(&client, &token, PATH, &body(&ids)).await;
    assert_eq!(go_status, 200);
    assert_eq!(
        String::from_utf8_lossy(&go_body),
        format!("{{\"{}\":[]}}", f.withdrawn),
        "the withdrawn reaction must not come back"
    );
    assert_eq!(go_body, rs_body, "{PATH} must be byte-identical");
}

/// `SortedArrayFromJSON` de-duplicates before the handler sees the list, so twenty copies of one
/// id is one key and one row's worth of work.
#[tokio::test]
async fn duplicate_ids_collapse_to_one_key() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let ids = [f.reacted.as_str(), f.reacted.as_str(), f.reacted.as_str()];
    let ((go_status, go_body), (_rs, rs_body)) =
        post_both_raw(&client, &token, PATH, &body(&ids)).await;

    assert_eq!(go_status, 200);
    assert_eq!(go_body, rs_body, "{PATH} must be byte-identical");
    let parsed: serde_json::Value = serde_json::from_slice(&go_body).expect("JSON");
    assert_eq!(
        parsed.as_object().expect("an object").len(),
        1,
        "three copies of one id is one key"
    );
    assert_eq!(
        parsed[&f.reacted].as_array().expect("an array").len(),
        3,
        "and the reactions are not tripled either"
    );
}

/// The ported bug. `SortedArrayFromJSON` accepts both bodies, the store builds `PostId IN ()`,
/// and Postgres refuses to parse it — so an empty request is a **500**, not the 400 every other
/// by-ids route answers.
#[tokio::test]
async fn an_empty_id_list_is_a_500_on_both() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;

    for raw in [&b"[]"[..], &b"null"[..]] {
        let ((go_status, go_body), (rs_status, rs_body)) =
            post_both_raw(&client, &token, PATH, raw).await;

        assert_eq!(
            go_status,
            500,
            "`{}` reaches the store as zero ids",
            String::from_utf8_lossy(raw)
        );
        assert_eq!(rs_status, go_status, "{PATH}: statuses must match");
        let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, PATH);
        assert_eq!(go["id"], "app.reaction.bulk_get_for_post_ids.app_error");
    }
}

/// A body that is not a JSON array of strings — the one refusal this handler makes on purpose.
#[tokio::test]
async fn a_non_array_body_is_a_400_on_both() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;

    for raw in [&b"{}"[..], &b"[1,2]"[..], &b"not json"[..]] {
        let ((go_status, go_body), (rs_status, rs_body)) =
            post_both_raw(&client, &token, PATH, raw).await;

        assert_eq!(
            go_status,
            400,
            "`{}` must not decode",
            String::from_utf8_lossy(raw)
        );
        assert_eq!(rs_status, go_status, "{PATH}: statuses must match");
        let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, PATH);
        assert_eq!(go["id"], "api.payload.parse.error");
    }
}

/// The permission gate runs over **every** id before any read, so one unreadable post in a list
/// of readable ones refuses the whole request.
#[tokio::test]
async fn one_unreadable_post_refuses_the_whole_request() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let ids = [f.reacted.as_str(), f.unreadable.as_str()];
    let ((go_status, go_body), (rs_status, rs_body)) =
        post_both_raw(&client, &f.plain_token, PATH, &body(&ids)).await;

    assert_eq!(
        go_status, 403,
        "the plain user is not in the private channel"
    );
    assert_eq!(rs_status, go_status, "{PATH}: statuses must match");
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, PATH);
    assert_eq!(go["id"], "api.context.permissions.app_error");

    // The same actor reading only what it can see is served — otherwise the 403 above would
    // pass for a user whose token was simply broken.
    let ok = [f.reacted.as_str()];
    let ((go_status, go_body), (_rs, rs_body)) =
        post_both_raw(&client, &f.plain_token, PATH, &body(&ok)).await;
    assert_eq!(go_status, 200);
    assert_eq!(go_body, rs_body, "{PATH} must be byte-identical");
}

/// Order of operations: the empty-list failure happens in the **store**, past a permission loop
/// that had no ids to check — so a plain user posting `[]` sees the 500, not a 403.
#[tokio::test]
async fn the_empty_list_500_is_reached_before_any_permission_check() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let ((go_status, go_body), (rs_status, rs_body)) =
        post_both_raw(&client, &f.plain_token, PATH, b"[]").await;

    assert_eq!(go_status, 500, "not a 403 — there was nothing to refuse");
    assert_eq!(rs_status, go_status);
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, PATH);
    assert_eq!(go["id"], "app.reaction.bulk_get_for_post_ids.app_error");
}

/// Go registers only `POST` on this path. Everything else is Go's to answer.
#[tokio::test]
async fn other_methods_are_forwarded() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;

    for method in [
        reqwest::Method::GET,
        reqwest::Method::PUT,
        reqwest::Method::DELETE,
    ] {
        let rs = client
            .request(method.clone(), format!("{RUST}{PATH}"))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .expect("we answer");
        assert_eq!(
            rs.headers()
                .get("x-mmrs-served-by")
                .and_then(|v| v.to_str().ok()),
            Some("go"),
            "{method} {PATH} must be forwarded"
        );
    }
}

/// An unauthenticated request never reaches the handler.
#[tokio::test]
async fn no_session_is_a_401_on_both() {
    if !stack_enabled() {
        return;
    }
    let client = client();

    let go = client
        .post(format!("{GO}{PATH}"))
        .header("Content-Type", "application/json")
        .body(&b"[\"aaaaaaaaaaaaaaaaaaaaaaaaaa\"]"[..])
        .send()
        .await
        .expect("Go answers");
    let rs = client
        .post(format!("{RUST}{PATH}"))
        .header("Content-Type", "application/json")
        .body(&b"[\"aaaaaaaaaaaaaaaaaaaaaaaaaa\"]"[..])
        .send()
        .await
        .expect("we answer");

    assert_eq!(go.status(), 401);
    assert_eq!(rs.status(), go.status(), "{PATH}: statuses must match");
    let go_body = go.bytes().await.expect("body").to_vec();
    let rs_body = rs.bytes().await.expect("body").to_vec();
    assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, PATH);
}

/// The two `COALESCE`s, against a row that actually holds NULL.
///
/// `UpdateAt` falls back to **`CreateAt`**, not to zero, and `DeleteAt` falls back to `0` — in
/// the select list *and* in the predicate, where a NULL would otherwise compare as unknown and
/// drop the row from a result Go includes. Neither branch is reachable through the REST API, so
/// the fixture writes the NULLs itself.
#[tokio::test]
async fn a_row_holding_nulls_coalesces_identically_on_both() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    // Without the planted NULLs this test asserts nothing the ordinary fixtures do not.
    if let Ok(url) = std::env::var("DATABASE_URL")
        && let Ok(pool) = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(std::time::Duration::from_secs(5))
            .connect(&url)
            .await
    {
        let nulls: (i64,) = sqlx::query_as(
            "SELECT count(*) FROM reactions \
             WHERE postid = $1 AND updateat IS NULL AND deleteat IS NULL",
        )
        .bind(&f.nulled)
        .fetch_one(&pool)
        .await
        .expect("the count runs");
        assert_eq!(
            nulls.0, 1,
            "the NULLs must be in the table for this to mean anything"
        );
    }

    let ids = [f.nulled.as_str()];
    let ((go_status, go_body), (rs_status, rs_body)) =
        post_both_raw(&client, &token, PATH, &body(&ids)).await;

    assert_eq!(go_status, 200);
    assert_eq!(rs_status, go_status);
    assert_eq!(go_body, rs_body, "{PATH} must be byte-identical");

    let parsed: serde_json::Value = serde_json::from_slice(&go_body).expect("JSON");
    let row = &parsed[&f.nulled][0];
    assert_eq!(
        row["emoji_name"], "ghost",
        "the NULL `DeleteAt` must not have filtered the row out"
    );
    assert_eq!(
        row["update_at"], row["create_at"],
        "`UpdateAt` coalesces to `CreateAt`, not to zero"
    );
    assert_ne!(row["update_at"], 0, "and `CreateAt` is not zero either");
    assert_eq!(row["delete_at"], 0, "`DeleteAt` coalesces to 0");
}
