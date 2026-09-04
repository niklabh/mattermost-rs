//! Cross-server parity for `GET /api/v4/posts/{post_id}/reactions`.
//!
//! ```sh
//! docker compose up -d
//! scripts/parity.sh -p mm-api --test parity post_reactions
//! ```
//!
//! # The empty answer is the interesting one
//!
//! Go's store leaves `[]*model.Reaction` nil when nothing matches and the handler marshals it
//! straight through, so a post with no reactions answers the literal `null`. Every other list
//! route in this port answers `[]`, so this is the one place where "serialise the `Vec`" is the
//! *wrong* translation — and the only way to know is to ask the running server, which
//! [`empty_is_null_not_an_empty_array`] does.
//!
//! # Two reactors, and one of them is not an admin
//!
//! `ORDER BY CreateAt` cannot be tested with a single reaction, and `CreateAt` has millisecond
//! resolution — two reactions written in the same millisecond tie, and a tie is exactly the
//! fixture that makes an unordered port look ordered. The fixture therefore reacts, waits past
//! a millisecond boundary, and reacts again, from two different users so `user_id` also varies.

use crate::common;

use common::{
    GO, RUST, add_user_to_channel, assert_error_bodies_match_except_known_gaps, client,
    create_channel, create_plain_user, fetch_both, fetch_both_raw, go_minted_token,
    logged_in_user_id, post_message, purge_api_fixtures, stack_enabled,
};

// ---------------------------------------------------------------------------------------------
// fixtures
// ---------------------------------------------------------------------------------------------

/// One team, one channel, and the four posts the module needs, built once per binary.
struct Fixture {
    /// A post carrying three reactions from two users.
    reacted: String,
    /// A post nobody has reacted to.
    bare: String,
    /// A post whose only reaction has been withdrawn — Go soft-deletes, so the row survives.
    withdrawn: String,
    /// A post in a channel the plain user is **not** in.
    unreadable: String,
    /// A non-admin who can be refused.
    plain_id: String,
    plain_token: String,
}

static FIXTURE: tokio::sync::OnceCell<Fixture> = tokio::sync::OnceCell::const_new();

async fn fixture(client: &reqwest::Client, token: &str) -> &'static Fixture {
    FIXTURE
        .get_or_init(|| async {
            purge_api_fixtures().await;

            let team_id = create_team(client, token, "reactions").await;
            let channel_id = create_channel(client, token, &team_id, "reactions").await;
            // **Private**, not public. A public channel would be readable by any team member
            // through `HasPermissionToReadChannel`'s `read_public_channel` fallback, and the
            // 403 this fixture exists to produce would be a 200 — measured on the first run.
            let closed_id = create_private_channel(client, token, &team_id, "reactionsshut").await;

            let plain = create_plain_user(client, token, &team_id, "react").await;
            add_user_to_channel(client, token, &channel_id, &plain.id).await;

            let admin_id = logged_in_user_id();

            let reacted = post_message(client, token, &channel_id, "react to me", None).await;
            // Creation order is **reverse alphabetical**: `+` is 0x2B and sorts before every
            // letter, so reacting `+1` → `eyes` → `grinning` would make `ORDER BY CreateAt` and
            // `ORDER BY EmojiName` produce the same list and the ordering assertion would hold
            // against either query. Going the other way separates them.
            add_reaction(client, token, &reacted, admin_id, "grinning").await;
            wait_past_a_millisecond().await;
            // The middle one comes from the *other* user, so `user_id` is not a proxy for the
            // order either.
            add_reaction(client, &plain.token, &reacted, &plain.id, "eyes").await;
            wait_past_a_millisecond().await;
            add_reaction(client, token, &reacted, admin_id, "+1").await;

            let bare = post_message(client, token, &channel_id, "nobody reacted", None).await;

            let withdrawn = post_message(client, token, &channel_id, "reaction undone", None).await;
            add_reaction(client, token, &withdrawn, admin_id, "tada").await;
            remove_reaction(client, token, &withdrawn, admin_id, "tada").await;

            let unreadable = post_message(client, token, &closed_id, "not for you", None).await;
            add_reaction(client, token, &unreadable, admin_id, "lock").await;

            Fixture {
                reacted,
                bare,
                withdrawn,
                unreadable,
                plain_id: plain.id,
                plain_token: plain.token,
            }
        })
        .await
}

/// `CreateAt` is milliseconds, so two writes inside one tick are indistinguishable to an
/// `ORDER BY` and the fixture would pass against an unordered query by accident.
async fn wait_past_a_millisecond() {
    tokio::time::sleep(std::time::Duration::from_millis(3)).await;
}

/// `common::create_channel` hardcodes type `O`; this route's refusal needs a `P`.
async fn create_private_channel(
    client: &reqwest::Client,
    admin_token: &str,
    team_id: &str,
    tag: &str,
) -> String {
    let response = client
        .post(format!("{GO}/api/v4/channels"))
        .header("Authorization", format!("Bearer {admin_token}"))
        .json(&serde_json::json!({
            "team_id": team_id,
            "name": format!("mmrs-parity-{tag}"),
            "display_name": format!("mmrs parity {tag}"),
            "type": "P",
        }))
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "creating the private fixture channel failed: {}",
        response.text().await.unwrap_or_default()
    );
    let created: serde_json::Value = response.json().await.expect("the channel decodes");
    created["id"].as_str().expect("an id").to_owned()
}

async fn create_team(client: &reqwest::Client, token: &str, tag: &str) -> String {
    let response = client
        .post(format!("{GO}/api/v4/teams"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!({
            "name": format!("mmrs-parity-{tag}"),
            "display_name": format!("mmrs parity {tag}"),
            "type": "O",
        }))
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "creating the fixture team failed: {}",
        response.text().await.unwrap_or_default()
    );
    let created: serde_json::Value = response.json().await.expect("the team decodes");
    created["id"].as_str().expect("an id").to_owned()
}

/// `POST /api/v4/reactions` — always against **Go**, so the row exists before either server is
/// asked to read it.
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

// ---------------------------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_posts_reactions_are_byte_identical() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let path = format!("/api/v4/posts/{}/reactions", f.reacted);
    let (go, rs) = fetch_both(&client, &token, &path).await;
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{path} must be byte-identical"
    );

    // And the fixture is actually the one the ordering claim needs.
    let parsed: serde_json::Value = serde_json::from_slice(&go).expect("Go's body is JSON");
    let names: Vec<&str> = parsed
        .as_array()
        .expect("an array")
        .iter()
        .filter_map(|r| r["emoji_name"].as_str())
        .collect();
    assert_eq!(
        names,
        vec!["grinning", "eyes", "+1"],
        "the fixture must be in creation order — which here is the *reverse* of emoji-name \
         order, so `ORDER BY EmojiName` cannot pass this"
    );
}

/// The wire quirk this route exists to get right.
#[tokio::test]
async fn empty_is_null_not_an_empty_array() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let path = format!("/api/v4/posts/{}/reactions", f.bare);
    let (go, rs) = fetch_both(&client, &token, &path).await;
    assert_eq!(go, b"null", "Go marshals a nil slice as `null`");
    assert_eq!(rs, go, "{path} must be byte-identical");
}

/// `COALESCE(DeleteAt, 0) = 0` in the store, and the row is still there to be wrongly returned.
#[tokio::test]
async fn a_withdrawn_reaction_is_excluded_by_both() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    // The row survives the withdrawal, so this is a real exclusion rather than an empty table.
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

    let path = format!("/api/v4/posts/{}/reactions", f.withdrawn);
    let (go, rs) = fetch_both(&client, &token, &path).await;
    assert_eq!(go, b"null", "the only reaction was withdrawn");
    assert_eq!(rs, go, "{path} must be byte-identical");
}

/// The permission gate, exercised by somebody who can actually be refused.
#[tokio::test]
async fn a_non_member_is_refused_identically() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let path = format!("/api/v4/posts/{}/reactions", f.unreadable);
    let ((go_status, go_body), (rs_status, rs_body)) =
        fetch_both_raw(&client, &f.plain_token, &path).await;

    assert_eq!(go_status, 403, "a non-member cannot read the post");
    assert_eq!(rs_status, go_status, "{path}: statuses must match");
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &path);
    assert_eq!(
        go["id"], "api.context.permissions.app_error",
        "SetPermissionError's id"
    );

    // And the same actor *can* read a post in a channel it is in — otherwise the 403 above
    // would pass for a user whose token was simply broken.
    let readable = format!("/api/v4/posts/{}/reactions", f.reacted);
    let (go_ok, rs_ok) = fetch_both(&client, &f.plain_token, &readable).await;
    assert_eq!(go_ok, rs_ok, "{readable} must be byte-identical");
    assert!(!go_ok.is_empty() && go_ok != b"null");
    let _ = &f.plain_id;
}

/// A well-formed id that names nothing. `SessionHasPermissionToReadPost` cannot resolve the
/// channel and falls back to a bare system check, so an ordinary user gets a **403** rather
/// than the 404 every other post route answers.
#[tokio::test]
async fn an_unknown_post_is_a_403_for_a_plain_user_and_null_for_an_admin() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let path = "/api/v4/posts/aaaaaaaaaaaaaaaaaaaaaaaaaa/reactions";

    let ((go_status, go_body), (rs_status, rs_body)) =
        fetch_both_raw(&client, &f.plain_token, path).await;
    assert_eq!(go_status, 403, "the fallback check refuses a plain user");
    assert_eq!(rs_status, go_status);
    assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, path);

    // The admin holds `read_channel_content` at the system level, so the fallback *passes* and
    // the read of a post that does not exist succeeds with nothing in it.
    let ((go_status, go_body), (rs_status, rs_body)) = fetch_both_raw(&client, &token, path).await;
    assert_eq!(go_status, 200, "there is no not-found branch on this route");
    assert_eq!(rs_status, go_status);
    assert_eq!(go_body, b"null");
    assert_eq!(rs_body, go_body);
}

/// `RequirePostId`: alphanumeric, so the router's charset lets it through, but the wrong length.
#[tokio::test]
async fn a_post_id_of_the_wrong_length_is_a_400_on_both() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;

    for id in ["short", "aaaaaaaaaaaaaaaaaaaaaaaaaaa"] {
        let path = format!("/api/v4/posts/{id}/reactions");
        let ((go_status, go_body), (rs_status, rs_body)) =
            fetch_both_raw(&client, &token, &path).await;
        assert_eq!(go_status, 400, "{path}");
        assert_eq!(rs_status, go_status, "{path}: statuses must match");
        let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &path);
        assert_eq!(go["id"], "api.context.invalid_url_param.app_error");
    }
}

/// A segment gorilla's `[A-Za-z0-9]+` refuses. Neither server may serve it from Rust: ours
/// forwards on the charset middleware and Go answers its own mux 404.
#[tokio::test]
async fn a_non_id_shaped_segment_is_forwarded() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;

    let path = "/api/v4/posts/not-an-id/reactions";
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
        "{path} must be forwarded, not served"
    );
    assert_eq!(go.status(), rs.status(), "{path}: statuses must match");
    assert_eq!(
        go.bytes().await.expect("body"),
        rs.bytes().await.expect("body"),
        "{path}: bodies must match"
    );
}

/// Everything but `GET` on this exact path stays Go's.
#[tokio::test]
async fn other_methods_are_forwarded() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let path = format!("/api/v4/posts/{}/reactions", f.reacted);
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

// No teardown test. Deleting the plain user from a `#[tokio::test]` races every other test in
// the binary — `zz_teardown` ran first on the very first pass and turned two 403s into 401s.
// `create_plain_user` names the user deterministically, so `common::purge_api_fixtures` collects
// last run's leftovers at the top of [`fixture`]; that is the whole cleanup story here.
