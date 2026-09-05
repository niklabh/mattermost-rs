//! Cross-server parity for `POST /api/v4/posts/ids` (`getPostsByIds`).
//!
//! ```sh
//! docker compose up -d
//! scripts/parity.sh -p mm-api --test parity posts_by_ids
//! ```
//!
//! # The two answers a reader would not predict
//!
//! **A soft-deleted post comes back.** The store's only predicate is `p.Id IN (…)`; every other
//! multi-post read in the tree filters `DeleteAt = 0` and this one does not, so a deleted post is
//! served with its `delete_at` set. [`a_deleted_post_is_still_returned`] holds that.
//!
//! **A post the caller cannot read is dropped silently, not refused.** The handler `continue`s
//! past it, so the answer is a 200 that is simply shorter — where the neighbouring
//! `POST /posts/ids/reactions` answers 403 for the same list. [`an_unreadable_post_is_dropped_not_refused`].

use crate::common;

use common::{
    GO, RUST, add_user_to_channel, assert_error_bodies_match_except_known_gaps, client,
    create_channel, create_channel_typed, create_plain_user, create_team, go_minted_token,
    post_both_raw, post_message, purge_api_fixtures, stack_enabled,
};

// ---------------------------------------------------------------------------------------------
// fixtures
// ---------------------------------------------------------------------------------------------

struct Fixture {
    /// Three posts in the readable channel, oldest first in creation order.
    early: String,
    middle: String,
    late: String,
    /// A thread root with two replies, and one of those replies.
    root: String,
    reply: String,
    /// A post whose message carries `&` and U+2028 — both escaped by Go, neither by serde.
    escaped: String,
    /// A post carrying an interactive attachment action with an `integration` block.
    actions: String,
    /// A post that has been deleted through Go's API — soft-deleted, so the row survives.
    deleted: String,
    /// A post in a **private** channel the plain user is not in.
    unreadable: String,
    plain_token: String,
}

static FIXTURE: tokio::sync::OnceCell<Fixture> = tokio::sync::OnceCell::const_new();

async fn fixture(client: &reqwest::Client, token: &str) -> &'static Fixture {
    FIXTURE
        .get_or_init(|| async {
            purge_api_fixtures().await;

            let team_id = create_team(client, token, "postsbyids").await;
            let channel_id = create_channel(client, token, &team_id, "postsbyids").await;
            let closed_id =
                create_channel_typed(client, token, &team_id, "postsbyidsshut", "P").await;

            let plain = create_plain_user(client, token, &team_id, "byids").await;
            add_user_to_channel(client, token, &channel_id, &plain.id).await;

            // Three posts, a millisecond apart, so `ORDER BY CreateAt DESC` has something to
            // order. No URLs in any message: a link sends `PreparePostForClient` down the embed
            // branch this port forwards rather than serves, and the comparison would then be
            // Go against Go.
            let early = post_message(client, token, &channel_id, "first one", None).await;
            wait_past_a_millisecond().await;
            let middle = post_message(client, token, &channel_id, "second one", None).await;
            wait_past_a_millisecond().await;
            let late = post_message(client, token, &channel_id, "third one", None).await;

            // A thread: the root's `ReplyCount` subquery counts its replies, and each *reply*
            // resolves the root first and reports the same number. A port that counted
            // `RootId = p.Id` without the `CASE` gives a reply 0.
            let root = post_message(client, token, &channel_id, "thread root", None).await;
            let reply = post_message(client, token, &channel_id, "reply one", Some(&root)).await;
            post_message(client, token, &channel_id, "reply two", Some(&root)).await;
            // A third reply, deleted. The `ReplyCount` subquery filters `DeleteAt = 0` even
            // though the outer query does not, and without this row that predicate is dead code
            // to the suite — a mutation loosening it would survive.
            let gone = post_message(client, token, &channel_id, "reply three", Some(&root)).await;
            delete_post(client, token, &gone).await;

            // `encoding/json` escapes `&` and U+2028; `serde_json` does not. A fixture whose
            // messages are all alphanumeric cannot tell the two apart, and the byte comparison
            // would pass against the wrong serialiser.
            //
            // **No `<`.** That needle is in `message_may_contain_a_link`, so a message carrying
            // one is *forwarded* — and a forwarded response compares Go against Go.
            let escaped = post_message(client, token, &channel_id, "a & b \u{2028}end", None).await;

            // `StripActionIntegrations` removes the `integration` block from every attachment
            // action on the way out — the post is stored with it and served without it.
            let actions = post_with_action_integration(client, token, &channel_id).await;

            let deleted = post_message(client, token, &channel_id, "about to go", None).await;
            delete_post(client, token, &deleted).await;

            let unreadable = post_message(client, token, &closed_id, "not for you", None).await;

            Fixture {
                early,
                middle,
                late,
                root,
                reply,
                escaped,
                actions,
                deleted,
                unreadable,
                plain_token: plain.token,
            }
        })
        .await
}

/// `CreateAt` is milliseconds, so two writes inside one tick tie and an unordered port passes.
async fn wait_past_a_millisecond() {
    tokio::time::sleep(std::time::Duration::from_millis(3)).await;
}

/// Go's `DELETE /posts/{id}` is a **soft** delete: the row stays, `DeleteAt` is set and the
/// message is blanked.
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

/// A post carrying one attachment action with an `integration` block. Go stores the whole
/// structure — verified in the table — and strips only `integration` when it serves the post.
async fn post_with_action_integration(
    client: &reqwest::Client,
    token: &str,
    channel_id: &str,
) -> String {
    let response = client
        .post(format!("{GO}/api/v4/posts"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!({
            "channel_id": channel_id,
            "message": "pick one",
            "props": {
                "attachments": [{
                    "text": "pick",
                    "actions": [{
                        "id": "act1",
                        "name": "Click",
                        "integration": {
                            "url": "http://example.invalid/hook",
                            "context": {"k": "v"},
                        },
                    }],
                }],
            },
        }))
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "posting the action fixture failed: {}",
        response.text().await.unwrap_or_default()
    );
    let created: serde_json::Value = response.json().await.expect("the post decodes");
    created["id"].as_str().expect("an id").to_owned()
}

const PATH: &str = "/api/v4/posts/ids";

fn body(ids: &[&str]) -> Vec<u8> {
    serde_json::to_vec(&ids).expect("the id list serialises")
}

fn ids_of(raw: &[u8]) -> Vec<String> {
    let parsed: serde_json::Value = serde_json::from_slice(raw).expect("the body is JSON");
    parsed
        .as_array()
        .expect("an array")
        .iter()
        .map(|p| p["id"].as_str().expect("an id").to_owned())
        .collect()
}

// ---------------------------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------------------------

/// The whole route in one assertion, plus the two claims about shape that a parsed comparison
/// would not see: the ordering and the trailing newline.
#[tokio::test]
async fn a_multi_post_read_is_byte_identical() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    // Requested oldest-first; the answer must come back newest-first.
    let ids = [f.early.as_str(), f.middle.as_str(), f.late.as_str()];
    let ((go_status, go_body), (rs_status, rs_body)) =
        post_both_raw(&client, &token, PATH, &body(&ids)).await;

    assert_eq!(go_status, 200, "{}", String::from_utf8_lossy(&go_body));
    assert_eq!(rs_status, go_status, "{PATH}: statuses must match");
    assert_eq!(
        String::from_utf8_lossy(&go_body),
        String::from_utf8_lossy(&rs_body),
        "{PATH} must be byte-identical"
    );

    assert_eq!(
        ids_of(&go_body),
        vec![f.late.clone(), f.middle.clone(), f.early.clone()],
        "ORDER BY CreateAt DESC — the reverse of the request, which was sorted by id"
    );
    assert_eq!(
        go_body.last(),
        Some(&b'\n'),
        "json.NewEncoder(w).Encode appends a newline that json.Marshal does not"
    );
}

/// Every 200 carries `First-Inaccessible-Post-Time`, and on a deployment with no Cloud
/// `PostHistory` limit it is `0`. `post_both_raw` does not surface headers, so this asks both
/// servers directly.
#[tokio::test]
async fn both_servers_set_the_first_inaccessible_post_time_header() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    for base in [GO, RUST] {
        let response = client
            .post(format!("{base}{PATH}"))
            .header("Authorization", format!("Bearer {token}"))
            .header("Content-Type", "application/json")
            .body(body(&[f.late.as_str()]))
            .send()
            .await
            .expect("the server answers");
        assert_eq!(response.status(), 200, "{base}{PATH}");
        assert_eq!(
            response
                .headers()
                .get("First-Inaccessible-Post-Time")
                .and_then(|v| v.to_str().ok()),
            Some("0"),
            "{base}{PATH} must carry the header, and it is 0 without a Cloud licence"
        );
    }
}

/// The store has no `DeleteAt` filter. Every sibling read does.
#[tokio::test]
async fn a_deleted_post_is_still_returned() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    // The row must still be there, or this asserts nothing.
    if let Ok(url) = std::env::var("DATABASE_URL")
        && let Ok(pool) = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(std::time::Duration::from_secs(5))
            .connect(&url)
            .await
    {
        let row: (i64,) = sqlx::query_as("SELECT deleteat FROM posts WHERE id = $1")
            .bind(&f.deleted)
            .fetch_one(&pool)
            .await
            .expect("the row is still there");
        assert!(
            row.0 > 0,
            "Go soft-deletes; a hard delete proves nothing here"
        );
    }

    let ids = [f.deleted.as_str()];
    let ((go_status, go_body), (rs_status, rs_body)) =
        post_both_raw(&client, &token, PATH, &body(&ids)).await;

    assert_eq!(go_status, 200, "a deleted post is not a 404 on this route");
    assert_eq!(rs_status, go_status);
    assert_eq!(go_body, rs_body, "{PATH} must be byte-identical");
    assert_eq!(ids_of(&go_body), vec![f.deleted.clone()]);

    let parsed: serde_json::Value = serde_json::from_slice(&go_body).expect("JSON");
    assert!(
        parsed[0]["delete_at"].as_i64().unwrap_or(0) > 0,
        "and it is served with its delete_at intact"
    );
}

/// `ReplyCount` resolves each post's own thread root before counting, so a reply reports its
/// parent's count and not zero.
#[tokio::test]
async fn reply_count_resolves_the_thread_root_for_every_row() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let ids = [f.root.as_str(), f.reply.as_str()];
    let ((go_status, go_body), (_rs, rs_body)) =
        post_both_raw(&client, &token, PATH, &body(&ids)).await;

    assert_eq!(go_status, 200);
    assert_eq!(go_body, rs_body, "{PATH} must be byte-identical");

    let parsed: serde_json::Value = serde_json::from_slice(&go_body).expect("JSON");
    for post in parsed.as_array().expect("an array") {
        assert_eq!(
            post["reply_count"], 2,
            "both the root and the reply report the thread's two replies: {post}"
        );
    }
}

/// The silent filter. Compare with `POST /posts/ids/reactions`, which 403s on the same list.
#[tokio::test]
async fn an_unreadable_post_is_dropped_not_refused() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let ids = [f.late.as_str(), f.unreadable.as_str()];
    let ((go_status, go_body), (rs_status, rs_body)) =
        post_both_raw(&client, &f.plain_token, PATH, &body(&ids)).await;

    assert_eq!(go_status, 200, "not a 403 — the post is simply absent");
    assert_eq!(rs_status, go_status);
    assert_eq!(go_body, rs_body, "{PATH} must be byte-identical");
    assert_eq!(
        ids_of(&go_body),
        vec![f.late.clone()],
        "the private-channel post must not be in the answer"
    );

    // And the admin, who can read both, gets both — otherwise the filter above would pass for a
    // fixture whose second post simply did not exist.
    let ((go_status, go_body), (_rs, rs_body)) =
        post_both_raw(&client, &token, PATH, &body(&ids)).await;
    assert_eq!(go_status, 200);
    assert_eq!(go_body, rs_body, "{PATH} must be byte-identical");
    assert_eq!(ids_of(&go_body).len(), 2, "the admin sees both");
}

/// Every id unknown is a 404; one known id among them is a 200 that mentions the miss nowhere.
#[tokio::test]
async fn all_unknown_is_a_404_but_partly_unknown_is_a_200() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let unknown = ["aaaaaaaaaaaaaaaaaaaaaaaaaa", "bbbbbbbbbbbbbbbbbbbbbbbbbb"];
    let ((go_status, go_body), (rs_status, rs_body)) =
        post_both_raw(&client, &token, PATH, &body(&unknown)).await;
    assert_eq!(go_status, 404, "zero rows is ErrNotFound in the store");
    assert_eq!(rs_status, go_status, "{PATH}: statuses must match");
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, PATH);
    assert_eq!(go["id"], "app.post.get.app_error");

    let mixed = [f.late.as_str(), "aaaaaaaaaaaaaaaaaaaaaaaaaa"];
    let ((go_status, go_body), (rs_status, rs_body)) =
        post_both_raw(&client, &token, PATH, &body(&mixed)).await;
    assert_eq!(go_status, 200, "one match is enough to avoid the 404");
    assert_eq!(rs_status, go_status);
    assert_eq!(go_body, rs_body, "{PATH} must be byte-identical");
    assert_eq!(ids_of(&go_body), vec![f.late.clone()]);
}

/// The length check this route has and `getBulkReactions` does not.
#[tokio::test]
async fn an_empty_id_list_is_a_400_naming_post_ids() {
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
            400,
            "`{}` is an empty list, not a parse failure",
            String::from_utf8_lossy(raw)
        );
        assert_eq!(rs_status, go_status, "{PATH}: statuses must match");
        let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, PATH);
        assert_eq!(go["id"], "api.context.invalid_body_param.app_error");
        assert!(
            go["message"]
                .as_str()
                .unwrap_or_default()
                .contains("post_ids"),
            "the parameter is named in the message: {}",
            go["message"]
        );
    }
}

/// A body that is not a JSON array of strings.
#[tokio::test]
async fn a_non_array_body_is_a_400() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;

    for raw in [&b"{}"[..], &b"[1,2]"[..], &b"not json"[..]] {
        let ((go_status, go_body), (rs_status, rs_body)) =
            post_both_raw(&client, &token, PATH, raw).await;
        assert_eq!(go_status, 400, "`{}`", String::from_utf8_lossy(raw));
        assert_eq!(rs_status, go_status, "{PATH}: statuses must match");
        let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, PATH);
        assert_eq!(go["id"], "api.payload.parse.error");
    }
}

/// The cap is 1000 — and it is applied **after** de-duplication, so 1500 copies of one id is a
/// legal request for one post.
#[tokio::test]
async fn the_thousand_id_cap_is_counted_after_de_duplication() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let distinct: Vec<String> = (0..1001).map(|i| format!("{i:026}")).collect();
    let refs: Vec<&str> = distinct.iter().map(String::as_str).collect();
    let ((go_status, go_body), (rs_status, rs_body)) =
        post_both_raw(&client, &token, PATH, &body(&refs)).await;
    assert_eq!(go_status, 400, "1001 distinct ids is over the cap");
    assert_eq!(rs_status, go_status, "{PATH}: statuses must match");
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, PATH);
    assert_eq!(go["id"], "api.post.posts_by_ids.invalid_body.request_error");

    // Exactly 1000 distinct ids is under it — and none of them exist, so the answer is the 404,
    // which is how we know the cap did not fire.
    let under: Vec<&str> = refs[..1000].to_vec();
    let ((go_status, _go_body), (rs_status, _rs_body)) =
        post_both_raw(&client, &token, PATH, &body(&under)).await;
    assert_eq!(go_status, 404, "1000 is not over the cap");
    assert_eq!(rs_status, go_status);

    let duplicated: Vec<&str> = vec![f.late.as_str(); 1500];
    let ((go_status, go_body), (rs_status, rs_body)) =
        post_both_raw(&client, &token, PATH, &body(&duplicated)).await;
    assert_eq!(
        go_status, 200,
        "SortedArrayFromJSON collapses them before the count"
    );
    assert_eq!(rs_status, go_status);
    assert_eq!(go_body, rs_body, "{PATH} must be byte-identical");
    assert_eq!(ids_of(&go_body), vec![f.late.clone()]);
}

/// Go registers only `POST` on this path.
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

    let send = async |base: &str| {
        client
            .post(format!("{base}{PATH}"))
            .header("Content-Type", "application/json")
            .body(&b"[\"aaaaaaaaaaaaaaaaaaaaaaaaaa\"]"[..])
            .send()
            .await
            .expect("the server answers")
    };

    let go = send(GO).await;
    let rs = send(RUST).await;
    assert_eq!(go.status(), 401);
    assert_eq!(rs.status(), go.status(), "{PATH}: statuses must match");
    let go_body = go.bytes().await.expect("body").to_vec();
    let rs_body = rs.bytes().await.expect("body").to_vec();
    assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, PATH);
}

/// Go's `json.Encoder` escapes `&` (and `<`, `>`, U+2028, U+2029); `serde_json` leaves them
/// alone. A post message is exactly where those characters turn up, and a fixture of plain
/// alphanumerics cannot tell the two serialisers apart.
#[tokio::test]
async fn html_and_line_separators_are_escaped_the_way_go_escapes_them() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let ids = [f.escaped.as_str()];
    let ((go_status, go_body), (_rs, rs_body)) =
        post_both_raw(&client, &token, PATH, &body(&ids)).await;

    assert_eq!(go_status, 200);
    let go_text = String::from_utf8_lossy(&go_body);
    assert!(
        go_text.contains(r"\u0026") && go_text.contains(r"\u2028"),
        "Go must actually have escaped something here, or the comparison is vacuous: {go_text}"
    );
    assert_eq!(
        go_text,
        String::from_utf8_lossy(&rs_body),
        "{PATH} must be byte-identical, escaping included"
    );
}

/// A post carrying `attachments` props is **forwarded**, not served.
///
/// `attachments` is in [`mm_app::post::REFUSED_PROPS`], so `SanitizePostMetadataForUser`'s port
/// refuses it and the handler hands the whole request to Go. Two consequences worth stating:
/// one unreadable-to-this-server post in a list forwards the *entire* request, and the
/// handler's own `StripActionIntegrations` call is therefore unreachable today. It is kept
/// because it is Go's, and because narrowing `REFUSED_PROPS` would otherwise silently start
/// leaking `integration` blocks.
#[tokio::test]
async fn a_post_with_attachment_props_is_forwarded() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    // The stored row carries the integration block; Go strips it on the way out. That is the
    // behaviour being deferred to, so assert it is really there to be stripped.
    if let Ok(url) = std::env::var("DATABASE_URL")
        && let Ok(pool) = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(std::time::Duration::from_secs(5))
            .connect(&url)
            .await
    {
        let stored: (String,) = sqlx::query_as("SELECT props::text FROM posts WHERE id = $1")
            .bind(&f.actions)
            .fetch_one(&pool)
            .await
            .expect("the row is there");
        assert!(
            stored.0.contains("integration"),
            "the fixture post must be stored *with* the integration block: {}",
            stored.0
        );
    }

    let response = client
        .post(format!("{RUST}{PATH}"))
        .header("Authorization", format!("Bearer {token}"))
        .header("Content-Type", "application/json")
        .body(body(&[f.actions.as_str()]))
        .send()
        .await
        .expect("we answer");

    assert_eq!(
        response
            .headers()
            .get("x-mmrs-served-by")
            .and_then(|v| v.to_str().ok()),
        Some("go"),
        "an attachments post must be forwarded, not served"
    );
    assert_eq!(response.status(), 200);
    let text = response.text().await.expect("body");
    assert!(
        !text.contains("integration") && text.contains("\"act1\""),
        "and Go's own strip still applies to the forwarded answer: {text}"
    );
}

/// A forward is all-or-nothing: one refused post takes the whole request to Go, including the
/// posts this server could have served itself.
#[tokio::test]
async fn one_refused_post_forwards_the_whole_request() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let response = client
        .post(format!("{RUST}{PATH}"))
        .header("Authorization", format!("Bearer {token}"))
        .header("Content-Type", "application/json")
        .body(body(&[f.late.as_str(), f.actions.as_str()]))
        .send()
        .await
        .expect("we answer");

    assert_eq!(
        response
            .headers()
            .get("x-mmrs-served-by")
            .and_then(|v| v.to_str().ok()),
        Some("go"),
        "the servable post does not rescue the request"
    );
    assert_eq!(response.status(), 200);

    // And the forwarded body is the one Go would have written unprompted.
    let rs_body = response.text().await.expect("body");
    let go_body = client
        .post(format!("{GO}{PATH}"))
        .header("Authorization", format!("Bearer {token}"))
        .header("Content-Type", "application/json")
        .body(body(&[f.late.as_str(), f.actions.as_str()]))
        .send()
        .await
        .expect("Go answers")
        .text()
        .await
        .expect("body");
    assert_eq!(rs_body, go_body, "{PATH}: the forwarded body must match");
}
