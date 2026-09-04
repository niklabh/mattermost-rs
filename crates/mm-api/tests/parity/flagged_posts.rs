//! Cross-server parity for `GET /api/v4/users/{user_id}/posts/flagged`
//! (`getFlaggedPostsForUser`) — the webapp's "Saved messages" panel.
//!
//! ```sh
//! docker compose up -d
//! scripts/parity.sh -p mm-api --test parity flagged_posts
//! ```
//!
//! # Two answers a reader would not predict
//!
//! **`page` is an offset.** The handler hands `c.Params.Page` straight to the store's `offset`
//! with no multiplication, so `?page=1&per_page=1` skips one *post*.
//! [`page_is_an_offset_not_a_page`] holds that.
//!
//! **The team filter lets every DM through.** Go's clause builder emits
//! `AND B.TeamId = ? OR B.TeamId = ''` without parentheses, so the second disjunct escapes the
//! membership check entirely and a flagged DM post answers for *any* team id.
//! [`the_team_filter_lets_every_dm_through`] holds that, and it is the reason the SQL in
//! `mm_store::post_store` is shaped the way it is.

use crate::common;

use common::{
    GO, RUST, add_user_to_channel, assert_error_bodies_match_except_known_gaps, client,
    create_channel, create_channel_typed, create_plain_user, create_team, fetch_both,
    fetch_both_raw, go_minted_token, logged_in_user_id, post_message, purge_api_fixtures,
    stack_enabled,
};

// ---------------------------------------------------------------------------------------------
// fixtures
// ---------------------------------------------------------------------------------------------

struct Fixture {
    /// Three posts the admin has flagged, oldest first — two in team A, one in team B.
    old_a: String,
    new_a: String,
    only_b: String,
    team_a: String,
    team_b: String,
    channel_a: String,
    /// A post in a DM, flagged by the admin. Its channel has no team.
    dm_post: String,
    /// A post the admin flagged and then deleted.
    deleted: String,
    /// A post in a private channel the plain user is **not** in, flagged *for* the plain user.
    /// Filtered out by the store's `ChannelMembers` subquery.
    hidden_from_plain: String,
    /// A post in a second private channel the plain user *is* a member of — with a membership
    /// row carrying no roles at all, so the handler's own read gate is what refuses it.
    roleless_member_post: String,
    plain_id: String,
    plain_token: String,
}

static FIXTURE: tokio::sync::OnceCell<Fixture> = tokio::sync::OnceCell::const_new();

async fn fixture(client: &reqwest::Client, token: &str) -> &'static Fixture {
    FIXTURE
        .get_or_init(|| async {
            purge_api_fixtures().await;

            let admin_id = logged_in_user_id();

            let team_a = create_team(client, token, "flaggeda").await;
            let team_b = create_team(client, token, "flaggedb").await;
            let channel_a = create_channel(client, token, &team_a, "flaggeda").await;
            let channel_b = create_channel(client, token, &team_b, "flaggedb").await;
            let closed = create_channel_typed(client, token, &team_a, "flaggedshut", "P").await;
            let roleless =
                create_channel_typed(client, token, &team_a, "flaggedroleless", "P").await;

            let plain = create_plain_user(client, token, &team_a, "flagged").await;
            add_user_to_channel(client, token, &channel_a, &plain.id).await;

            // A millisecond apart so `ORDER BY CreateAt DESC` has something to order.
            let old_a = post_message(client, token, &channel_a, "flag me first", None).await;
            wait_past_a_millisecond().await;
            let new_a = post_message(client, token, &channel_a, "flag me second", None).await;
            wait_past_a_millisecond().await;
            let only_b = post_message(client, token, &channel_b, "flag me in b", None).await;

            for post in [&old_a, &new_a, &only_b] {
                flag_post(client, token, admin_id, post).await;
            }

            // A DM: its channel's `TeamId` is `''`, which is what makes Go's unparenthesised
            // team filter observable.
            let dm = open_direct_channel(client, token, admin_id, &plain.id).await;
            let dm_post = post_message(client, token, &dm, "flagged in a dm", None).await;
            flag_post(client, token, admin_id, &dm_post).await;

            // Flagged, then deleted. The store filters `Posts.DeleteAt = 0` inside the
            // subquery — unlike `getPostsByIds`, which has no such filter.
            let deleted = post_message(client, token, &channel_a, "flagged then gone", None).await;
            flag_post(client, token, admin_id, &deleted).await;
            delete_post(client, token, &deleted).await;
            // Go's `DeletePost` deletes the post's flagged-post preferences along with it, so
            // over REST there is never a flag pointing at a deleted post and the subquery's
            // `DeleteAt = 0` is dead code to any test that goes through the API. Planted back
            // directly, which the API cannot do, so the predicate is a live branch.
            plant_flag(admin_id, &deleted).await;

            // The admin can write *another* user's preferences, and `updatePreferences` checks
            // the **session's** read permission on the channel, not the target user's. So this
            // plants a flag the plain user could never have created — and the store's
            // `ChannelMembers` subquery is the only thing that keeps it out of their list.
            let hidden_from_plain =
                post_message(client, token, &closed, "not for the plain user", None).await;
            flag_post(client, token, &plain.id, &hidden_from_plain).await;

            // The handler's *second* gate. The store only asks whether a `ChannelMembers` row
            // exists; the handler then asks whether the session can read the channel. A member
            // normally passes both, which is why that gate looked unreachable and a mutation
            // disabling it survived the first run. A membership row with **no roles** separates
            // them: the subquery matches, `read_channel` does not, and the post is dropped by
            // the handler. Measured against Go, which drops it too.
            let roleless_member_post =
                post_message(client, token, &roleless, "member without roles", None).await;
            flag_post(client, token, &plain.id, &roleless_member_post).await;
            plant_roleless_membership(&roleless, &plain.id).await;

            Fixture {
                old_a,
                new_a,
                only_b,
                team_a,
                team_b,
                channel_a,
                dm_post,
                deleted,
                hidden_from_plain,
                roleless_member_post,
                plain_id: plain.id,
                plain_token: plain.token,
            }
        })
        .await
}

/// Write a `flagged_post` preference straight into the table.
///
/// The REST API cannot leave a flag pointing at a deleted post — `DeletePost` removes both — so
/// this is the only way to reach the store's `Posts.DeleteAt = 0` predicate. Silent without a
/// `DATABASE_URL`; the test that needs it re-checks the row and says so if it is missing.
async fn plant_flag(user_id: &str, post_id: &str) {
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
    sqlx::query(
        "INSERT INTO preferences (userid, category, name, value) \
         VALUES ($1, 'flagged_post', $2, 'true') \
         ON CONFLICT (userid, category, name) DO UPDATE SET value = 'true'",
    )
    .bind(user_id)
    .bind(post_id)
    .execute(&pool)
    .await
    .expect("the insert runs");
}

/// Make `user_id` a member of `channel_id` with no roles whatsoever.
///
/// `POST /channels/{id}/members` always writes `channel_user`, so the REST API cannot produce
/// this row — and without it the handler's read gate is unreachable, because every ordinary
/// member of a channel can read it.
async fn plant_roleless_membership(channel_id: &str, user_id: &str) {
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
    sqlx::query(
        "INSERT INTO channelmembers (channelid, userid, roles, lastviewedat, msgcount, \
             mentioncount, notifyprops, lastupdateat, schemeuser, schemeadmin, schemeguest, \
             mentioncountroot, msgcountroot, urgentmentioncount) \
         VALUES ($1, $2, '', 0, 0, 0, '{}', 0, false, false, false, 0, 0, 0) \
         ON CONFLICT (channelid, userid) DO UPDATE \
             SET roles = '', schemeuser = false, schemeadmin = false, schemeguest = false",
    )
    .bind(channel_id)
    .bind(user_id)
    .execute(&pool)
    .await
    .expect("the insert runs");
}

/// `CreateAt` is milliseconds, so two writes inside one tick tie and an unordered port passes.
async fn wait_past_a_millisecond() {
    tokio::time::sleep(std::time::Duration::from_millis(3)).await;
}

/// Flagging is a preference: category `flagged_post`, name = the post id.
async fn flag_post(client: &reqwest::Client, token: &str, user_id: &str, post_id: &str) {
    let response = client
        .put(format!("{GO}/api/v4/users/{user_id}/preferences"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!([{
            "user_id": user_id,
            "category": "flagged_post",
            "name": post_id,
            "value": "true",
        }]))
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "flagging {post_id} for {user_id} failed: {}",
        response.text().await.unwrap_or_default()
    );
}

async fn open_direct_channel(client: &reqwest::Client, token: &str, a: &str, b: &str) -> String {
    let response = client
        .post(format!("{GO}/api/v4/channels/direct"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!([a, b]))
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "opening the DM failed: {}",
        response.text().await.unwrap_or_default()
    );
    let created: serde_json::Value = response.json().await.expect("the channel decodes");
    created["id"].as_str().expect("an id").to_owned()
}

async fn delete_post(client: &reqwest::Client, token: &str, post_id: &str) {
    let response = client
        .delete(format!("{GO}/api/v4/posts/{post_id}"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "deleting {post_id} failed: {}",
        response.text().await.unwrap_or_default()
    );
}

fn order_of(raw: &[u8]) -> Vec<String> {
    let parsed: serde_json::Value = serde_json::from_slice(raw).expect("the body is JSON");
    parsed["order"]
        .as_array()
        .expect("an order array")
        .iter()
        .map(|id| id.as_str().expect("an id").to_owned())
        .collect()
}

// ---------------------------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------------------------

/// The whole route in one assertion, plus the two claims a parsed comparison would not see.
#[tokio::test]
async fn a_flagged_list_is_byte_identical() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;
    let admin_id = logged_in_user_id();

    let path = format!("/api/v4/users/{admin_id}/posts/flagged");
    let (go, rs) = fetch_both(&client, &token, &path).await;
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{path} must be byte-identical"
    );

    let order = order_of(&go);
    assert_eq!(
        order,
        vec![
            f.dm_post.clone(),
            f.only_b.clone(),
            f.new_a.clone(),
            f.old_a.clone()
        ],
        "CreateAt DESC across every channel, and the deleted post is not in it"
    );
    assert_eq!(
        go.last(),
        Some(&b'\n'),
        "PostList.EncodeJSON appends the newline json.Marshal does not"
    );
}

/// `c.Params.Page` is handed to the store as `offset` and never multiplied by `per_page`.
#[tokio::test]
async fn page_is_an_offset_not_a_page() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;
    let admin_id = logged_in_user_id();

    // Four flagged posts, newest first: dm_post, only_b, new_a, old_a.
    for (page, expected) in [
        (0, vec![f.dm_post.clone()]),
        (1, vec![f.only_b.clone()]),
        (2, vec![f.new_a.clone()]),
        (3, vec![f.old_a.clone()]),
        (4, vec![]),
    ] {
        let path = format!("/api/v4/users/{admin_id}/posts/flagged?page={page}&per_page=1");
        let (go, rs) = fetch_both(&client, &token, &path).await;
        assert_eq!(go, rs, "{path} must be byte-identical");
        assert_eq!(
            order_of(&go),
            expected,
            "{path}: page {page} skips {page} posts, not {page} pages"
        );
    }

    // **`per_page` must not be 1 for this to mean anything.** With a page size of one,
    // `page` and `page * per_page` are the same number, and a mutation multiplying them
    // survives every case above — it did, on the first run. Two-at-a-time separates them:
    // `page=1` skips one post, where `page * per_page` would skip two.
    for (page, expected) in [
        (0, vec![f.dm_post.clone(), f.only_b.clone()]),
        (1, vec![f.only_b.clone(), f.new_a.clone()]),
        (2, vec![f.new_a.clone(), f.old_a.clone()]),
    ] {
        let path = format!("/api/v4/users/{admin_id}/posts/flagged?page={page}&per_page=2");
        let (go, rs) = fetch_both(&client, &token, &path).await;
        assert_eq!(go, rs, "{path} must be byte-identical");
        assert_eq!(
            order_of(&go),
            expected,
            "{path}: the window slides by one post per page, not by two"
        );
    }
}

/// Go's team clause is `AND B.TeamId = ? OR B.TeamId = ''` with no parentheses, so the second
/// disjunct answers for every DM regardless of the team asked for — and regardless of the
/// membership check it escapes.
#[tokio::test]
async fn the_team_filter_lets_every_dm_through() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;
    let admin_id = logged_in_user_id();

    // Team B holds exactly one flagged post — and the DM post, which belongs to no team at all.
    let path = format!(
        "/api/v4/users/{admin_id}/posts/flagged?team_id={}",
        f.team_b
    );
    let (go, rs) = fetch_both(&client, &token, &path).await;
    assert_eq!(go, rs, "{path} must be byte-identical");
    assert_eq!(
        order_of(&go),
        vec![f.dm_post.clone(), f.only_b.clone()],
        "the DM post has nothing to do with team B and is in the answer anyway"
    );

    // Team A, same story from the other side.
    let path = format!(
        "/api/v4/users/{admin_id}/posts/flagged?team_id={}",
        f.team_a
    );
    let (go, rs) = fetch_both(&client, &token, &path).await;
    assert_eq!(go, rs, "{path} must be byte-identical");
    assert_eq!(
        order_of(&go),
        vec![f.dm_post.clone(), f.new_a.clone(), f.old_a.clone()],
    );

    // And a team id that names nothing still answers the DM post — the clearest statement of
    // the bug, because nothing else can match.
    let path = format!("/api/v4/users/{admin_id}/posts/flagged?team_id=aaaaaaaaaaaaaaaaaaaaaaaaaa");
    let (go, rs) = fetch_both(&client, &token, &path).await;
    assert_eq!(go, rs, "{path} must be byte-identical");
    assert_eq!(order_of(&go), vec![f.dm_post.clone()]);
}

/// `channel_id` is tested first, so passing both filters by channel and ignores the team.
#[tokio::test]
async fn the_channel_filter_wins_over_the_team_filter() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;
    let admin_id = logged_in_user_id();

    let path = format!(
        "/api/v4/users/{admin_id}/posts/flagged?channel_id={}&team_id={}",
        f.channel_a, f.team_b
    );
    let (go, rs) = fetch_both(&client, &token, &path).await;
    assert_eq!(go, rs, "{path} must be byte-identical");
    assert_eq!(
        order_of(&go),
        vec![f.new_a.clone(), f.old_a.clone()],
        "channel A's flagged posts, and no DM — the team filter never ran"
    );

    // The channel filter alone, for the same answer, so the test above is about precedence and
    // not about the channel filter happening to be broken.
    let path = format!(
        "/api/v4/users/{admin_id}/posts/flagged?channel_id={}",
        f.channel_a
    );
    let (go, rs) = fetch_both(&client, &token, &path).await;
    assert_eq!(go, rs, "{path} must be byte-identical");
    assert_eq!(order_of(&go), vec![f.new_a.clone(), f.old_a.clone()]);
}

/// The `ChannelMembers` subquery, exercised by a flag the user could not have created.
#[tokio::test]
async fn a_flag_on_a_channel_the_user_is_not_in_is_filtered_out() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    // The preference row must exist, or this asserts nothing about the filter.
    if let Ok(url) = std::env::var("DATABASE_URL")
        && let Ok(pool) = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(std::time::Duration::from_secs(5))
            .connect(&url)
            .await
    {
        let flags: (i64,) = sqlx::query_as(
            "SELECT count(*) FROM preferences \
             WHERE userid = $1 AND category = 'flagged_post' AND name = $2",
        )
        .bind(&f.plain_id)
        .bind(&f.hidden_from_plain)
        .fetch_one(&pool)
        .await
        .expect("the count runs");
        assert_eq!(flags.0, 1, "the planted flag must be in the table");
    }

    let path = format!("/api/v4/users/{}/posts/flagged", f.plain_id);
    let (go, rs) = fetch_both(&client, &f.plain_token, &path).await;
    assert_eq!(go, rs, "{path} must be byte-identical");
    assert!(
        !order_of(&go).contains(&f.hidden_from_plain),
        "a flag on a private channel the user is not in must not surface it"
    );
}

/// `Posts.DeleteAt = 0` inside the subquery. The flag survives the delete, so the row is there
/// to be wrongly returned — and `getPostsByIds`, one route over, *would* return it.
#[tokio::test]
async fn a_flagged_post_that_was_deleted_is_excluded() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;
    let admin_id = logged_in_user_id();

    if let Ok(url) = std::env::var("DATABASE_URL")
        && let Ok(pool) = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(std::time::Duration::from_secs(5))
            .connect(&url)
            .await
    {
        let row: (i64, i64) = sqlx::query_as(
            "SELECT (SELECT deleteat FROM posts WHERE id = $1), \
                    (SELECT count(*) FROM preferences \
                      WHERE category = 'flagged_post' AND name = $1)",
        )
        .bind(&f.deleted)
        .fetch_one(&pool)
        .await
        .expect("the row is there");
        assert!(row.0 > 0, "Go soft-deletes; a hard delete proves nothing");
        assert_eq!(row.1, 1, "and the flag outlives the post it points at");
    }

    let path = format!("/api/v4/users/{admin_id}/posts/flagged");
    let (go, rs) = fetch_both(&client, &token, &path).await;
    assert_eq!(go, rs, "{path} must be byte-identical");
    assert!(
        !order_of(&go).contains(&f.deleted),
        "a deleted post must not come back through its surviving flag"
    );
}

/// The handler's own `SessionHasPermissionToReadChannel` gate, which the store's membership
/// subquery normally hides.
///
/// The fixture plants a `ChannelMembers` row with **no roles**, which the REST API cannot
/// create: the subquery then matches and the read check does not, so the post reaches the gate
/// and is dropped there. Without this the gate is dead code to the suite, and a mutation
/// disabling it survives — which is exactly what happened before this test existed.
#[tokio::test]
async fn a_member_who_cannot_read_the_channel_is_filtered_by_the_handler() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    // Both halves of the setup have to hold, or this proves nothing about the gate.
    if let Ok(url) = std::env::var("DATABASE_URL")
        && let Ok(pool) = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(std::time::Duration::from_secs(5))
            .connect(&url)
            .await
    {
        let row: (i64, String) = sqlx::query_as(
            "SELECT (SELECT count(*) FROM preferences \
                      WHERE userid = $1 AND category = 'flagged_post' AND name = $2), \
                    (SELECT cm.roles FROM channelmembers cm \
                       JOIN posts p ON p.channelid = cm.channelid \
                      WHERE cm.userid = $1 AND p.id = $2)",
        )
        .bind(&f.plain_id)
        .bind(&f.roleless_member_post)
        .fetch_one(&pool)
        .await
        .expect("the row is there");
        assert_eq!(
            row.0, 1,
            "the flag must exist for the store to return the post"
        );
        assert_eq!(
            row.1, "",
            "and the membership row must carry no roles, or the gate passes and this is vacuous"
        );
    }

    let path = format!("/api/v4/users/{}/posts/flagged", f.plain_id);
    let (go, rs) = fetch_both(&client, &f.plain_token, &path).await;
    assert_eq!(go, rs, "{path} must be byte-identical");
    assert!(
        !order_of(&go).contains(&f.roleless_member_post),
        "a member with no roles cannot read the channel, so the post is dropped"
    );
}

/// `NewPostList` gives the empty answer a non-nil order and map.
#[tokio::test]
async fn an_empty_answer_is_empty_collections_not_nulls() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    // The plain user's only flag is the one they cannot see.
    let path = format!("/api/v4/users/{}/posts/flagged", f.plain_id);
    let (go, rs) = fetch_both(&client, &f.plain_token, &path).await;
    assert_eq!(
        String::from_utf8_lossy(&go).trim_end(),
        r#"{"order":[],"posts":{},"next_post_id":"","prev_post_id":"","first_inaccessible_post_time":0}"#,
        "empty collections, not nulls"
    );
    assert_eq!(go, rs, "{path} must be byte-identical");
}

/// The gate is `SessionHasPermissionToUser`, and its refusal names a *write* permission.
#[tokio::test]
async fn another_users_flags_are_a_403() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;
    let admin_id = logged_in_user_id();

    let path = format!("/api/v4/users/{admin_id}/posts/flagged");
    let ((go_status, go_body), (rs_status, rs_body)) =
        fetch_both_raw(&client, &f.plain_token, &path).await;
    assert_eq!(go_status, 403, "a plain user cannot read the admin's flags");
    assert_eq!(rs_status, go_status, "{path}: statuses must match");
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &path);
    assert_eq!(go["id"], "api.context.permissions.app_error");

    // The admin, being a system admin, *can* read the plain user's — so the 403 above is about
    // the permission and not about a broken token.
    let path = format!("/api/v4/users/{}/posts/flagged", f.plain_id);
    let ((go_status, go_body), (_rs, rs_body)) = fetch_both_raw(&client, &token, &path).await;
    assert_eq!(go_status, 200);
    assert_eq!(go_body, rs_body, "{path} must be byte-identical");
}

/// `RequireUserId`: alphanumeric, so the router's charset lets it through, but the wrong length.
#[tokio::test]
async fn a_user_id_of_the_wrong_length_is_a_400_on_both() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;

    for id in ["short", "aaaaaaaaaaaaaaaaaaaaaaaaaaa"] {
        let path = format!("/api/v4/users/{id}/posts/flagged");
        let ((go_status, go_body), (rs_status, rs_body)) =
            fetch_both_raw(&client, &token, &path).await;
        assert_eq!(go_status, 400, "{path}");
        assert_eq!(rs_status, go_status, "{path}: statuses must match");
        let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &path);
        assert_eq!(go["id"], "api.context.invalid_url_param.app_error");
    }
}

/// Everything but `GET` on this path stays Go's.
#[tokio::test]
async fn other_methods_are_forwarded() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let admin_id = logged_in_user_id();

    let path = format!("/api/v4/users/{admin_id}/posts/flagged");
    for method in [reqwest::Method::POST, reqwest::Method::DELETE] {
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

/// An unauthenticated request never reaches the handler.
#[tokio::test]
async fn no_session_is_a_401_on_both() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let path = "/api/v4/users/aaaaaaaaaaaaaaaaaaaaaaaaaa/posts/flagged";

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
    assert_eq!(go.status(), 401);
    assert_eq!(rs.status(), go.status(), "{path}: statuses must match");
    let go_body = go.bytes().await.expect("body").to_vec();
    let rs_body = rs.bytes().await.expect("body").to_vec();
    assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, path);
}
