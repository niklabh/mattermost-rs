//! Cross-server parity for `GET /api/v4/posts/{post_id}/edit_history`.
//!
//! ```sh
//! docker compose up -d
//! scripts/parity.sh -p mm-api --test parity post_edit_history
//! ```
//!
//! # Almost every refusal on this route is the same 403
//!
//! An unknown post, a caller without `edit_post` on the channel, and a caller who is not the
//! post's author all produce `SetPermissionError(PermissionEditPost)`. Go **discards**
//! `GetSinglePost`'s 404 to make the first of those a 403, which is a deliberate refusal to
//! disclose whether the post exists — so a port that let the 404 through would leak exactly what
//! the handler is hiding. The one thing that *is* a 404 is a post that exists, is yours, and has
//! never been edited.
//!
//! # The metadata is one field and no pipeline
//!
//! This is the only post read in the port that does not go through `PreparePostForClient`. The
//! app layer sets `metadata.files` and nothing else, so a history entry's `metadata` is `{}` or
//! `{"files":[…]}` — never `emojis`, `reactions`, `embeds`, `priority` or `acknowledgements`.

use crate::common;

use common::{
    GO, TINY_PNG, add_user_to_channel, assert_error_bodies_match_except_known_gaps, client,
    create_channel, create_plain_user, fetch_both, fetch_both_raw, go_minted_token,
    logged_in_user_id, post_message, post_message_with_files, purge_api_fixtures, stack_enabled,
    update_post, upload_file,
};

// ---------------------------------------------------------------------------------------------
// fixtures
// ---------------------------------------------------------------------------------------------

struct Fixture {
    channel_id: String,
    /// Edited twice, so `ORDER BY EditAt DESC` has something to order.
    edited_id: String,
    /// Edited once and carrying an attachment, so `metadata.files` is populated.
    edited_with_file_id: String,
    /// Never edited: the one 404 on this route.
    untouched_id: String,
    /// Authored by a channel member other than the caller.
    other_authors_id: String,
    /// A channel member who is not the author of `edited_id`.
    member_token: String,
    /// Not in the channel at all.
    outsider_token: String,
    /// Authored a post, edited it, and then **left the channel**. The only actor that can
    /// separate the channel gate from the authorship one: it passes the second and fails the
    /// first, so a port that dropped the channel check would serve it.
    departed_token: String,
    departed_post_id: String,
}

static FIXTURE: tokio::sync::OnceCell<Fixture> = tokio::sync::OnceCell::const_new();

async fn fixture(client: &reqwest::Client, token: &str) -> &'static Fixture {
    FIXTURE
        .get_or_init(|| async {
            purge_api_fixtures().await;
            let (team_id, _) = common::a_team_and_channel_the_user_is_in(client, token).await;
            let channel_id = create_channel(client, token, &team_id, "edithist").await;
            add_user_to_channel(client, token, &channel_id, logged_in_user_id()).await;

            let edited_id = post_message(client, token, &channel_id, "first version", None).await;
            update_post(client, token, &edited_id, "second version").await;
            update_post(client, token, &edited_id, "third version").await;

            let file_id = upload_file(
                client,
                token,
                &channel_id,
                "hist.png",
                "image/png",
                TINY_PNG,
            )
            .await;
            let edited_with_file_id = post_message_with_files(
                client,
                token,
                &channel_id,
                "with an attachment",
                std::slice::from_ref(&file_id),
            )
            .await;
            update_post(
                client,
                token,
                &edited_with_file_id,
                "edited, still attached",
            )
            .await;

            let untouched_id = post_message(client, token, &channel_id, "never edited", None).await;

            let member = create_plain_user(client, token, &team_id, "edithist").await;
            add_user_to_channel(client, token, &channel_id, &member.id).await;
            let other_authors_id =
                post_message(client, &member.token, &channel_id, "not yours", None).await;
            update_post(
                client,
                &member.token,
                &other_authors_id,
                "edited by its author",
            )
            .await;

            let outsider = create_plain_user(client, token, &team_id, "edithistout").await;

            let departed = create_plain_user(client, token, &team_id, "edithistgone").await;
            add_user_to_channel(client, token, &channel_id, &departed.id).await;
            let departed_post_id =
                post_message(client, &departed.token, &channel_id, "mine, once", None).await;
            update_post(client, &departed.token, &departed_post_id, "mine, edited").await;
            let left = client
                .delete(format!(
                    "{GO}/api/v4/channels/{channel_id}/members/{}",
                    departed.id
                ))
                .header("Authorization", format!("Bearer {token}"))
                .send()
                .await
                .expect("Go answers");
            assert!(left.status().is_success(), "the author leaves the channel");

            Fixture {
                channel_id,
                edited_id,
                edited_with_file_id,
                untouched_id,
                other_authors_id,
                member_token: member.token,
                outsider_token: outsider.token,
                departed_token: departed.token,
                departed_post_id,
            }
        })
        .await
}

// ---------------------------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_edit_history_is_byte_identical() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let path = format!("/api/v4/posts/{}/edit_history", f.edited_id);
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

    let parsed: serde_json::Value = serde_json::from_slice(&go).expect("JSON");
    let entries = parsed.as_array().expect("a bare array, not a PostList");
    assert_eq!(entries.len(), 2, "two edits, two history rows");

    // The history rows are *old versions*, so each carries the message it used to hold and an
    // `original_id` naming the live post.
    for entry in entries {
        assert_eq!(entry["original_id"], f.edited_id.as_str());
        assert_eq!(entry["channel_id"], f.channel_id.as_str());
        assert_ne!(entry["id"], f.edited_id.as_str(), "a copy, not the post");
    }
    let messages: Vec<&str> = entries
        .iter()
        .map(|e| e["message"].as_str().expect("a message"))
        .collect();
    assert_eq!(
        messages,
        vec!["second version", "first version"],
        "ORDER BY EditAt DESC — most recent edit first"
    );
}

/// `postsQuery` selects no reply-count subquery, so every history row reports `0` — even for a
/// post that has replies.
#[tokio::test]
async fn a_history_entry_reports_no_replies() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    // Give the live post a reply *now*, so the assertion is about the query and not about the
    // fixture happening to have none.
    post_message(
        &client,
        &token,
        &f.channel_id,
        "a reply to the edited post",
        Some(&f.edited_id),
    )
    .await;

    let path = format!("/api/v4/posts/{}/edit_history", f.edited_id);
    let (go, rs) = fetch_both(&client, &token, &path).await;
    assert_eq!(String::from_utf8_lossy(&go), String::from_utf8_lossy(&rs));

    let parsed: serde_json::Value = serde_json::from_slice(&go).expect("JSON");
    for entry in parsed.as_array().expect("an array") {
        assert_eq!(entry["reply_count"], 0);
    }
}

/// `metadata` is set by hand and holds `files` alone — no `emojis`, no `reactions`, no `embeds`.
#[tokio::test]
async fn the_metadata_is_files_and_nothing_else() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let path = format!("/api/v4/posts/{}/edit_history", f.edited_with_file_id);
    let (go, rs) = fetch_both(&client, &token, &path).await;
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{path} must be byte-identical"
    );

    let parsed: serde_json::Value = serde_json::from_slice(&go).expect("JSON");
    let entry = &parsed.as_array().expect("an array")[0];
    let metadata = entry["metadata"].as_object().expect("a metadata object");
    assert_eq!(
        metadata.keys().collect::<Vec<_>>(),
        vec!["files"],
        "one key, set by populateEditHistoryFileMetadata and nothing else"
    );
    assert_eq!(metadata["files"].as_array().expect("an array").len(), 1);

    // And an entry with no attachments carries an **empty** metadata object rather than none:
    // Go allocates the struct unconditionally and `Files` is omitempty.
    let plain = format!("/api/v4/posts/{}/edit_history", f.edited_id);
    let (go, _) = fetch_both(&client, &token, &plain).await;
    let parsed: serde_json::Value = serde_json::from_slice(&go).expect("JSON");
    assert_eq!(
        parsed[0]["metadata"],
        serde_json::json!({}),
        "an allocated but empty PostMetadata"
    );
}

/// A post that exists, is the caller's own, and has never been edited: the store's
/// `ErrNotFound`, and the **only** 404 this route can produce.
#[tokio::test]
async fn a_post_that_was_never_edited_is_a_404() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let path = format!("/api/v4/posts/{}/edit_history", f.untouched_id);
    let ((go_status, go_body), (rs_status, rs_body)) = fetch_both_raw(&client, &token, &path).await;
    assert_eq!(go_status, 404);
    assert_eq!(rs_status, go_status);
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &path);
    assert_eq!(go["id"], "app.post.get.app_error");
}

/// An unknown post is a **403**, not a 404: Go discards `GetSinglePost`'s error and raises a
/// permission one instead. Asserted for a system admin, who passes every permission check there
/// is — so the 403 can only have come from that discarded error.
#[tokio::test]
async fn an_unknown_post_is_a_403_even_for_an_admin() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let _ = fixture(&client, &token).await;

    let path = "/api/v4/posts/zzzzzzzzzzzzzzzzzzzzzzzzzz/edit_history";
    let ((go_status, go_body), (rs_status, rs_body)) = fetch_both_raw(&client, &token, path).await;
    assert_eq!(
        go_status, 403,
        "the missing-post 404 is swallowed and re-raised as a permission error"
    );
    assert_eq!(rs_status, go_status);
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, path);
    assert_eq!(go["id"], "api.context.permissions.app_error");
}

/// The authorship check: a channel member with `edit_post` still cannot read someone else's
/// history. Both directions asserted, so the test cannot pass on a port that refuses everybody.
#[tokio::test]
async fn only_the_author_may_read_a_history() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    // The member reading its own post's history: served.
    let own = format!("/api/v4/posts/{}/edit_history", f.other_authors_id);
    let (go, rs) = fetch_both(&client, &f.member_token, &own).await;
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{own}"
    );

    // The same member reading the admin's post's history: refused, despite holding edit_post on
    // the channel.
    let theirs = format!("/api/v4/posts/{}/edit_history", f.edited_id);
    let ((go_status, go_body), (rs_status, rs_body)) =
        fetch_both_raw(&client, &f.member_token, &theirs).await;
    assert_eq!(go_status, 403);
    assert_eq!(rs_status, go_status);
    assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &theirs);

    // And the admin reading the member's post: also refused. `manage_system` passes the channel
    // gate and does nothing for the authorship one, which is what makes these two separate
    // checks rather than one.
    let ((go_status, go_body), (rs_status, rs_body)) = fetch_both_raw(&client, &token, &own).await;
    assert_eq!(
        go_status, 403,
        "a system admin is still not the author, and the check is on identity"
    );
    assert_eq!(rs_status, go_status);
    assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &own);
}

/// The channel gate on its own, separated from the authorship one.
///
/// `edit_post` is a **channel** permission: `channel_user` grants it, `team_user` does not. So an
/// author who has left the channel fails the first check and passes the second — the only shape
/// that can tell the two apart, and the reason a mutation disabling the channel gate survived
/// until this fixture existed. Every other refusal on this route fails both.
#[tokio::test]
async fn the_author_still_needs_edit_post_on_the_channel() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let path = format!("/api/v4/posts/{}/edit_history", f.departed_post_id);
    let ((go_status, go_body), (rs_status, rs_body)) =
        fetch_both_raw(&client, &f.departed_token, &path).await;
    assert_eq!(
        go_status, 403,
        "the author of this post, refused because it no longer holds edit_post on the channel"
    );
    assert_eq!(rs_status, go_status);
    assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &path);

    // The same user reading the same post through `getPost` is served — the post is in a public
    // channel and `read_channel_content` does have a team-level fallback. So the 403 above is
    // about `edit_post` specifically, not about the user having lost all access.
    let readable = format!("/api/v4/posts/{}", f.departed_post_id);
    let (go, rs) = fetch_both(&client, &f.departed_token, &readable).await;
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{readable}"
    );
}

/// The channel gate, which runs before the authorship one and refuses with the same body.
#[tokio::test]
async fn an_outsider_is_refused() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let path = format!("/api/v4/posts/{}/edit_history", f.edited_id);
    let ((go_status, go_body), (rs_status, rs_body)) =
        fetch_both_raw(&client, &f.outsider_token, &path).await;
    assert_eq!(go_status, 403);
    assert_eq!(rs_status, go_status);
    assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &path);
}

#[tokio::test]
async fn a_malformed_post_id_is_a_400_on_both() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let _ = fixture(&client, &token).await;

    let path = "/api/v4/posts/short/edit_history";
    let ((go_status, go_body), (rs_status, rs_body)) = fetch_both_raw(&client, &token, path).await;
    assert_eq!(go_status, 400);
    assert_eq!(rs_status, go_status);
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, path);
    assert_eq!(go["id"], "api.context.invalid_url_param.app_error");
}
