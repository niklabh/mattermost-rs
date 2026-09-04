//! Cross-server parity for `GET /api/v4/posts/{post_id}/files/info` and
//! `GET /api/v4/files/{file_id}/info`.
//!
//! ```sh
//! docker compose up -d
//! scripts/parity.sh -p mm-api --test parity file_info
//! ```
//!
//! # One suite for two routes, because they disagree about the same row
//!
//! Both read `FileInfo`s and both select `FileInfo.Archived`. `getFileInfo` puts it on the wire;
//! `getFileInfosForPost` **drops it**, because Go's `GetByIds` scans into a store-private struct
//! whose `ToModel()` forgets that one field (file_info_store.go:75). Nothing but a fixture that
//! sets the column can tell the two apart, and no REST call sets it — hence
//! `common::set_fileinfo_column`.
//!
//! # The empty answer is `null`, and it is the common case
//!
//! A post with no attachments answers four bytes. That falls out of a nil slice surviving three
//! functions that all decline to materialise it; the assertion is here rather than in a comment
//! because a `[]` would be an equally plausible port.

use crate::common;

use common::{
    GO, RUST, TINY_PNG, add_user_to_channel, assert_error_bodies_match_except_known_gaps, client,
    create_channel, create_channel_typed, create_plain_user, fetch_both, fetch_both_raw,
    go_minted_token, logged_in_user_id, post_message, post_message_with_files, purge_api_fixtures,
    set_fileinfo_column, stack_enabled, upload_file,
};

// ---------------------------------------------------------------------------------------------
// fixtures
// ---------------------------------------------------------------------------------------------

struct Fixture {
    channel_id: String,
    /// A post carrying three attachments: one PNG and two text files.
    post_id: String,
    /// The three, in the order the post was created with — **not** the order the column holds.
    png_id: String,
    text_a_id: String,
    text_b_id: String,
    /// A post with no attachments at all.
    bare_post_id: String,
    /// Uploaded and never attached: `PostId` empty, `ChannelId` set by the upload.
    orphan_id: String,
    /// Uploaded, never attached, and then had its `ChannelId` nulled — a pre-migration row.
    channelless_id: String,
    /// An image whose `MiniPreview` was cleared, which is what sends both routes to Go.
    ///
    /// **There are two of them, and that is the finding.** Forwarding means Go serves the
    /// request — and `generateMiniPreview` *repairs the row* while doing so, upserting the
    /// thumbnail it just encoded. So a previewless file is previewless exactly once: the second
    /// read of the same row is served by us, because by then it has a preview. One fixture file
    /// used for both routes made the second assertion fail, and it failed on **every** mutation
    /// in the batch, which is how it was caught — as a control that should have survived and
    /// did not.
    previewless_id: String,
    previewless_post_id_file: String,
    previewless_post_id: String,
    /// A **private** channel's own attachment and post. A public channel cannot test a refusal:
    /// `HasPermissionToReadChannel` falls back to `read_public_channel` on the team, and every
    /// plain user this harness makes is a team member.
    private_post_id: String,
    private_file_id: String,
    /// A non-admin who is in the team and in neither channel.
    outsider_token: String,
    /// A non-admin who *is* in `channel_id` — it can read the post but holds no `manage_system`.
    member_token: String,
    /// A non-admin who joined the **private** channel, uploaded `uploader_file_id`, and left.
    uploader_token: String,
    uploader_file_id: String,
}

static FIXTURE: tokio::sync::OnceCell<Fixture> = tokio::sync::OnceCell::const_new();

async fn fixture(client: &reqwest::Client, token: &str) -> &'static Fixture {
    FIXTURE
        .get_or_init(|| async {
            purge_api_fixtures().await;
            let (team_id, _) = common::a_team_and_channel_the_user_is_in(client, token).await;
            let channel_id = create_channel(client, token, &team_id, "fileinfo").await;
            add_user_to_channel(client, token, &channel_id, logged_in_user_id()).await;

            let png_id =
                upload_file(client, token, &channel_id, "a.png", "image/png", TINY_PNG).await;
            let text_a_id = upload_file(
                client,
                token,
                &channel_id,
                "a.txt",
                "text/plain",
                b"first attachment",
            )
            .await;
            let text_b_id = upload_file(
                client,
                token,
                &channel_id,
                "b.txt",
                "text/plain",
                b"second attachment",
            )
            .await;

            let post_id = post_message_with_files(
                client,
                token,
                &channel_id,
                "three attachments",
                &[png_id.clone(), text_a_id.clone(), text_b_id.clone()],
            )
            .await;

            let bare_post_id = post_message(client, token, &channel_id, "no files", None).await;

            let orphan_id = upload_file(
                client,
                token,
                &channel_id,
                "orphan.txt",
                "text/plain",
                b"never attached",
            )
            .await;

            let channelless_id = upload_file(
                client,
                token,
                &channel_id,
                "old.txt",
                "text/plain",
                b"before the channelid column",
            )
            .await;
            set_fileinfo_column(&channelless_id, "channelid", "NULL").await;

            let previewless_id =
                upload_file(client, token, &channel_id, "np.png", "image/png", TINY_PNG).await;
            set_fileinfo_column(&previewless_id, "minipreview", "NULL").await;
            // The second one, for the post route — see the field's doc comment. **The column is
            // cleared after the post exists, not before**: creating a post runs Go's own
            // `PreparePostForClient`, which fetches the attachments' metadata and therefore
            // repairs the very preview this fixture is trying to remove.
            let previewless_post_id_file =
                upload_file(client, token, &channel_id, "np2.png", "image/png", TINY_PNG).await;
            let previewless_post_id = post_message_with_files(
                client,
                token,
                &channel_id,
                "an image with no stored preview",
                std::slice::from_ref(&previewless_post_id_file),
            )
            .await;
            set_fileinfo_column(&previewless_post_id_file, "minipreview", "NULL").await;

            // `text_b` carries the archived flag for the whole suite. It is on the post, so both
            // routes see it and can be made to disagree.
            set_fileinfo_column(&text_b_id, "archived", "TRUE").await;

            // Three non-admins, because three different questions need three different actors
            // and they must not move underneath each other: the harness runs the tests in this
            // binary **concurrently**, so a test that joins or leaves a channel mid-run would
            // change the answer another test is asserting. Every membership change this suite
            // needs happens here, once, before any test reads anything.
            let private_channel_id =
                create_channel_typed(client, token, &team_id, "filepriv", "P").await;
            add_user_to_channel(client, token, &private_channel_id, logged_in_user_id()).await;
            let private_file_id = upload_file(
                client,
                token,
                &private_channel_id,
                "secret.txt",
                "text/plain",
                b"members only",
            )
            .await;
            let private_post_id = post_message_with_files(
                client,
                token,
                &private_channel_id,
                "a private attachment",
                std::slice::from_ref(&private_file_id),
            )
            .await;

            let outsider = create_plain_user(client, token, &team_id, "fileout").await;

            let member = create_plain_user(client, token, &team_id, "filemem").await;
            add_user_to_channel(client, token, &channel_id, &member.id).await;

            let uploader = create_plain_user(client, token, &team_id, "fileup").await;
            add_user_to_channel(client, token, &private_channel_id, &uploader.id).await;
            let uploader_file_id = upload_file(
                client,
                &uploader.token,
                &private_channel_id,
                "mine.txt",
                "text/plain",
                b"my own file",
            )
            .await;
            let left = client
                .delete(format!(
                    "{GO}/api/v4/channels/{private_channel_id}/members/{}",
                    uploader.id
                ))
                .header("Authorization", format!("Bearer {token}"))
                .send()
                .await
                .expect("Go answers");
            assert!(
                left.status().is_success(),
                "the uploader leaves the channel"
            );

            Fixture {
                channel_id,
                post_id,
                png_id,
                text_a_id,
                text_b_id,
                bare_post_id,
                orphan_id,
                channelless_id,
                previewless_id,
                previewless_post_id_file,
                previewless_post_id,
                private_post_id,
                private_file_id,
                outsider_token: outsider.token,
                member_token: member.token,
                uploader_token: uploader.token,
                uploader_file_id,
            }
        })
        .await
}

// ---------------------------------------------------------------------------------------------
// getFileInfosForPost
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_posts_file_infos_are_byte_identical() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let path = format!("/api/v4/posts/{}/files/info", f.post_id);
    let (go, rs) = fetch_both(&client, &token, &path).await;
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{path} must be byte-identical"
    );
    assert!(
        !go.ends_with(b"\n"),
        "json.Marshal + w.Write writes no trailing newline"
    );

    let parsed: serde_json::Value = serde_json::from_slice(&go).expect("JSON");
    let infos = parsed.as_array().expect("an array");
    assert_eq!(infos.len(), 3, "three attachments");

    // Every wire key, and each one carrying something — a comparison of two sets of zeroes
    // proves nothing.
    for key in [
        "id",
        "user_id",
        "post_id",
        "channel_id",
        "create_at",
        "update_at",
        "delete_at",
        "name",
        "extension",
        "size",
        "mime_type",
        "mini_preview",
        "remote_id",
        "archived",
    ] {
        assert!(infos[0].get(key).is_some(), "{key} must be on the wire");
    }
    // `path`, `thumbnail_path`, `preview_path` and `content` are `json:"-"`.
    for key in ["path", "thumbnail_path", "preview_path", "content"] {
        assert!(infos[0].get(key).is_none(), "{key} must never be sent");
    }
    assert_eq!(infos[0]["post_id"], f.post_id.as_str());
    assert_eq!(infos[0]["channel_id"], f.channel_id.as_str());
}

/// The order follows `post.FileIds`, which `PreSave` **sorted** — so it is alphabetical by id,
/// not the order the fixture listed and not `CreateAt DESC`.
#[tokio::test]
async fn the_order_is_the_posts_sorted_file_ids() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let path = format!("/api/v4/posts/{}/files/info", f.post_id);
    let (go, rs) = fetch_both(&client, &token, &path).await;
    assert_eq!(go, rs);

    let parsed: serde_json::Value = serde_json::from_slice(&go).expect("JSON");
    let ids: Vec<&str> = parsed
        .as_array()
        .expect("an array")
        .iter()
        .map(|i| i["id"].as_str().expect("an id"))
        .collect();

    let mut expected = [
        f.png_id.as_str(),
        f.text_a_id.as_str(),
        f.text_b_id.as_str(),
    ];
    expected.sort_unstable();
    assert_eq!(
        ids, expected,
        "orderFileInfosByID follows the sorted column"
    );
}

/// `GetByIds` drops `Archived`. The row says `true` and both servers must say `false`.
#[tokio::test]
async fn archived_is_dropped_by_the_post_route() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let path = format!("/api/v4/posts/{}/files/info", f.post_id);
    let (go, rs) = fetch_both(&client, &token, &path).await;
    assert_eq!(go, rs);

    let parsed: serde_json::Value = serde_json::from_slice(&go).expect("JSON");
    let archived = parsed
        .as_array()
        .expect("an array")
        .iter()
        .find(|i| i["id"] == f.text_b_id.as_str())
        .expect("the archived file is on the post");
    assert_eq!(
        archived["archived"], false,
        "the column is TRUE and ToModel() forgets it — a port that reads the column diverges here"
    );
}

/// The sibling route reads the same row and keeps the flag. Without this the assertion above
/// would pass on a port that never selected the column at all.
#[tokio::test]
async fn archived_survives_the_single_file_route() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let path = format!("/api/v4/files/{}/info", f.text_b_id);
    let (go, rs) = fetch_both(&client, &token, &path).await;
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{path} must be byte-identical"
    );

    let parsed: serde_json::Value = serde_json::from_slice(&go).expect("JSON");
    assert_eq!(
        parsed["archived"], true,
        "`Get` scans into model.FileInfo, so the column reaches the wire"
    );
}

/// Four bytes, not two. Both servers.
#[tokio::test]
async fn a_post_with_no_files_answers_null() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let path = format!("/api/v4/posts/{}/files/info", f.bare_post_id);
    let (go, rs) = fetch_both(&client, &token, &path).await;
    assert_eq!(go, b"null", "a nil slice marshals to null, not []");
    assert_eq!(rs, go);
}

/// `GetEtagForFileInfos` on a non-empty list is `<version>.<postId>.<max updateAt>`, so the
/// header round-trips into a 304 — and the etag itself has to match across the two servers or
/// a client that cached against Go would never get one from us.
#[tokio::test]
async fn the_file_infos_etag_agrees_and_round_trips() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let path = format!("/api/v4/posts/{}/files/info", f.post_id);
    let mut etags = Vec::new();
    for base in [GO, RUST] {
        let response = client
            .get(format!("{base}{path}"))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .expect("reachable");
        assert_eq!(response.status(), 200);
        assert_eq!(
            response
                .headers()
                .get("Cache-Control")
                .and_then(|v| v.to_str().ok()),
            Some("max-age=2592000, private"),
            "{base} sets the private month-long cache header on the 200"
        );
        etags.push(
            response
                .headers()
                .get("ETag")
                .expect("an ETag")
                .to_str()
                .expect("ASCII")
                .to_owned(),
        );
    }
    assert_eq!(etags[0], etags[1], "the two servers compute the same etag");

    for base in [GO, RUST] {
        let response = client
            .get(format!("{base}{path}"))
            .header("Authorization", format!("Bearer {token}"))
            .header("If-None-Match", &etags[0])
            .send()
            .await
            .expect("reachable");
        assert_eq!(response.status(), 304, "{base} honours the etag");
        assert_eq!(
            response.headers().get("ETag").and_then(|v| v.to_str().ok()),
            Some(etags[0].as_str()),
            "{base} echoes the etag on the 304"
        );
        assert!(
            response.headers().get("Cache-Control").is_none(),
            "{base}: HandleEtag writes the 304 before the handler's own headers"
        );
    }
}

/// `GetEtagForFileInfos` on an empty list is a bare `model.Etag()` — **`CurrentVersion` and
/// nothing else**. So every post with no attachments on the whole server shares one etag, and a
/// client that cached post A's empty list gets a 304 for post B's. Not a guess: the first version
/// of this test asserted the opposite (a clock stamp, hence never a 304) and Go answered 304.
#[tokio::test]
async fn every_empty_file_list_shares_one_etag() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    // A second post with no files, so "the same etag" is a claim about two different posts.
    let other_bare = post_message(&client, &token, &f.channel_id, "also no files", None).await;

    let mut etags = Vec::new();
    for post in [f.bare_post_id.as_str(), other_bare.as_str()] {
        for base in [GO, RUST] {
            let path = format!("/api/v4/posts/{post}/files/info");
            let response = client
                .get(format!("{base}{path}"))
                .header("Authorization", format!("Bearer {token}"))
                .send()
                .await
                .expect("reachable");
            assert_eq!(response.status(), 200);
            etags.push(
                response
                    .headers()
                    .get("ETag")
                    .expect("an ETag")
                    .to_str()
                    .expect("ASCII")
                    .to_owned(),
            );
        }
    }
    assert!(
        etags.windows(2).all(|w| w[0] == w[1]),
        "two servers, two empty posts, one etag: {etags:?}"
    );
    assert_eq!(
        etags[0].matches('.').count(),
        2,
        "a bare Etag() is the version alone — three dotted components, no post id and no \
         timestamp: {}",
        etags[0]
    );

    // And it really does produce a 304 — on an etag minted from the *other* post.
    for base in [GO, RUST] {
        let path = format!("/api/v4/posts/{}/files/info", f.bare_post_id);
        let response = client
            .get(format!("{base}{path}"))
            .header("Authorization", format!("Bearer {token}"))
            .header("If-None-Match", etags[2].as_str())
            .send()
            .await
            .expect("reachable");
        assert_eq!(response.status(), 304, "{base}{path}");
    }
}

/// `include_deleted` is gated on `manage_system`, and the gate runs **after** the read
/// permission. Three actors pin all of it: a channel member is served without the flag and
/// refused with it, an outsider is refused either way, and the admin is served either way.
#[tokio::test]
async fn include_deleted_needs_manage_system() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let plain = format!("/api/v4/posts/{}/files/info", f.post_id);
    let deleted = format!(
        "/api/v4/posts/{}/files/info?include_deleted=true",
        f.post_id
    );

    // A member holds read_channel_content and not manage_system.
    let (go, rs) = fetch_both(&client, &f.member_token, &plain).await;
    assert_eq!(go, rs, "a member reads the files");

    let ((go_status, go_body), (rs_status, rs_body)) =
        fetch_both_raw(&client, &f.member_token, &deleted).await;
    assert_eq!(go_status, 403, "…and is refused the deleted ones");
    assert_eq!(rs_status, go_status);
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &deleted);
    assert_eq!(go["id"], "api.context.permissions.app_error");

    // The outsider fails the *first* gate, so the flag never gets a say — asserted against the
    // private post, since the public one is readable by any team member.
    let private_deleted = format!(
        "/api/v4/posts/{}/files/info?include_deleted=true",
        f.private_post_id
    );
    let ((go_status, go_body), (rs_status, rs_body)) =
        fetch_both_raw(&client, &f.outsider_token, &private_deleted).await;
    assert_eq!(go_status, 403);
    assert_eq!(rs_status, go_status);
    assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &private_deleted);

    // The admin holds both.
    let (go_ok, rs_ok) = fetch_both(&client, &token, &deleted).await;
    assert_eq!(go_ok, rs_ok);
}

/// A post the caller cannot read is a 403 before any file is fetched.
#[tokio::test]
async fn an_outsider_is_refused_the_posts_files() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let path = format!("/api/v4/posts/{}/files/info", f.private_post_id);
    let ((go_status, go_body), (rs_status, rs_body)) =
        fetch_both_raw(&client, &f.outsider_token, &path).await;
    assert_eq!(go_status, 403);
    assert_eq!(rs_status, go_status);
    assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &path);

    // And the *public* post is served to the same user, through the `read_public_channel`
    // fallback — without which this test would pass on a port that refused everybody.
    let public = format!("/api/v4/posts/{}/files/info", f.post_id);
    let (go, rs) = fetch_both(&client, &f.outsider_token, &public).await;
    assert_eq!(go, rs, "{public}");
}

#[tokio::test]
async fn a_malformed_post_id_is_a_400_on_both() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let _ = fixture(&client, &token).await;

    let path = "/api/v4/posts/short/files/info";
    let ((go_status, go_body), (rs_status, rs_body)) = fetch_both_raw(&client, &token, path).await;
    assert_eq!(go_status, 400);
    assert_eq!(rs_status, go_status);
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, path);
    assert_eq!(go["id"], "api.context.invalid_url_param.app_error");
}

/// A well-formed id naming nothing: the permission check cannot resolve a channel and falls back
/// to a bare system check, which the admin passes — so this is a 404 from `GetSingle`, not a 403.
#[tokio::test]
async fn an_unknown_post_is_a_404_for_an_admin() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let _ = fixture(&client, &token).await;

    let path = "/api/v4/posts/zzzzzzzzzzzzzzzzzzzzzzzzzz/files/info";
    let ((go_status, go_body), (rs_status, rs_body)) = fetch_both_raw(&client, &token, path).await;
    assert_eq!(go_status, 404);
    assert_eq!(rs_status, go_status);
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, path);
    assert_eq!(go["id"], "app.post.get.app_error");
}

// ---------------------------------------------------------------------------------------------
// getFileInfo
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_file_info_is_byte_identical() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let path = format!("/api/v4/files/{}/info", f.png_id);
    let (go, rs) = fetch_both(&client, &token, &path).await;
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{path} must be byte-identical"
    );
    assert!(
        go.ends_with(b"\n"),
        "json.NewEncoder().Encode adds a trailing newline — unlike its sibling route"
    );

    let parsed: serde_json::Value = serde_json::from_slice(&go).expect("JSON");
    assert_eq!(parsed["id"], f.png_id.as_str());
    assert_eq!(parsed["post_id"], f.post_id.as_str());
    assert_eq!(parsed["mime_type"], "image/png");
    assert!(
        parsed["mini_preview"].is_string(),
        "a []byte is base64 on Go's wire, not an array of numbers"
    );
    assert_eq!(parsed["width"], 1);
    assert_eq!(parsed["height"], 1);
}

/// No etag on this route at all, so `If-None-Match` is ignored and every request is a 200.
#[tokio::test]
async fn the_single_file_route_has_no_etag() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let path = format!("/api/v4/files/{}/info", f.text_a_id);
    for base in [GO, RUST] {
        let response = client
            .get(format!("{base}{path}"))
            .header("Authorization", format!("Bearer {token}"))
            .header("If-None-Match", "anything at all")
            .send()
            .await
            .expect("reachable");
        assert_eq!(
            response.status(),
            200,
            "{base} has no etag to match against"
        );
        assert!(response.headers().get("ETag").is_none(), "{base}");
        assert_eq!(
            response
                .headers()
                .get("Cache-Control")
                .and_then(|v| v.to_str().ok()),
            Some("max-age=2592000, private"),
            "{base}"
        );
    }
}

/// A file uploaded but never attached to a post: `PostId` is empty and the row is still readable,
/// because the upload set `ChannelId`.
#[tokio::test]
async fn an_unattached_upload_is_readable() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let path = format!("/api/v4/files/{}/info", f.orphan_id);
    let (go, rs) = fetch_both(&client, &token, &path).await;
    assert_eq!(go, rs);

    let parsed: serde_json::Value = serde_json::from_slice(&go).expect("JSON");
    assert_eq!(
        parsed.get("post_id"),
        None,
        "`post_id` is omitempty and the file is on no post"
    );
    assert_eq!(parsed["channel_id"], f.channel_id.as_str());
}

/// A pre-migration row with a NULL `ChannelId`: the `COALESCE` makes it the empty string, and
/// `GetChannel("")` 404s — *before* the `CreatorId == session.UserId` escape hatch is consulted,
/// so the uploader cannot read its own file.
#[tokio::test]
async fn a_null_channel_id_is_a_404_even_for_the_uploader() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let path = format!("/api/v4/files/{}/info", f.channelless_id);
    let ((go_status, go_body), (rs_status, rs_body)) = fetch_both_raw(&client, &token, &path).await;
    assert_eq!(go_status, 404, "the channel lookup runs first and misses");
    assert_eq!(rs_status, go_status);
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &path);
    assert_eq!(go["id"], "app.channel.get.existing.app_error");
}

/// An image with no stored preview takes Go's `generateMiniPreview` branch, which reads the file
/// backend and writes the row back. We forward it — so this asserts the response came from **Go**,
/// the one place in the suite where `x-mmrs-served-by: rust` must *not* hold.
#[tokio::test]
async fn an_image_with_no_mini_preview_is_forwarded() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    // Each path gets its **own** previewless file: Go repairs the row as it serves the forward,
    // so reading one file twice would be served by us the second time.
    assert_ne!(f.previewless_id, f.previewless_post_id_file);
    for path in [
        format!("/api/v4/files/{}/info", f.previewless_id),
        // One qualifying file forwards the whole list — the response is a single JSON array and
        // there is no way to serve half of it.
        format!("/api/v4/posts/{}/files/info", f.previewless_post_id),
    ] {
        let response = client
            .get(format!("{RUST}{path}"))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .expect("reachable");
        assert_eq!(response.status(), 200, "{path}");
        assert_eq!(
            response
                .headers()
                .get("x-mmrs-served-by")
                .and_then(|v| v.to_str().ok()),
            Some("go"),
            "{path}: the mini-preview repair is a write against a backend we do not have"
        );
    }
}

/// The outsider is not in the channel and did not upload the file, so the second branch of the
/// permission block refuses.
#[tokio::test]
async fn an_outsider_is_refused_a_file_it_did_not_upload() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let path = format!("/api/v4/files/{}/info", f.private_file_id);
    let ((go_status, go_body), (rs_status, rs_body)) =
        fetch_both_raw(&client, &f.outsider_token, &path).await;
    assert_eq!(go_status, 403);
    assert_eq!(rs_status, go_status);
    assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &path);
}

/// …but a file it uploaded itself, into a channel it has since left, it may read. This is the
/// `CreatorId == session.UserId` short-circuit, and it is the half of the permission block a
/// naive `!perm` port would delete. The join, the upload and the departure all happened in the
/// fixture, so nothing here moves under a concurrently running test.
#[tokio::test]
async fn a_user_may_read_its_own_upload_after_leaving_the_channel() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    // It really is an outsider now: a file it did *not* upload is refused.
    let others = format!("/api/v4/files/{}/info", f.private_file_id);
    let ((go_status, _), (rs_status, _)) =
        fetch_both_raw(&client, &f.uploader_token, &others).await;
    assert_eq!(go_status, 403, "no longer a member");
    assert_eq!(rs_status, go_status);

    let path = format!("/api/v4/files/{}/info", f.uploader_file_id);
    let (go, rs) = fetch_both(&client, &f.uploader_token, &path).await;
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "the uploader keeps access to its own file"
    );
}

#[tokio::test]
async fn a_malformed_file_id_is_a_400_on_both() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let _ = fixture(&client, &token).await;

    let path = "/api/v4/files/short/info";
    let ((go_status, go_body), (rs_status, rs_body)) = fetch_both_raw(&client, &token, path).await;
    assert_eq!(go_status, 400);
    assert_eq!(rs_status, go_status);
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, path);
    assert_eq!(go["id"], "api.context.invalid_url_param.app_error");
}

#[tokio::test]
async fn an_unknown_file_is_a_404_on_both() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let _ = fixture(&client, &token).await;

    let path = "/api/v4/files/zzzzzzzzzzzzzzzzzzzzzzzzzz/info";
    let ((go_status, go_body), (rs_status, rs_body)) = fetch_both_raw(&client, &token, path).await;
    assert_eq!(go_status, 404);
    assert_eq!(rs_status, go_status);
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, path);
    assert_eq!(go["id"], "app.file_info.get.app_error");
}

/// `Get` has no `includeDeleted` parameter, so a soft-deleted file is a 404 however it is asked
/// for — and the post route, with `include_deleted=true` and `manage_system`, still finds it.
#[tokio::test]
async fn a_soft_deleted_file_is_a_404_here_and_visible_there() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let gone_id = upload_file(
        &client,
        &token,
        &f.channel_id,
        "gone.txt",
        "text/plain",
        b"about to be deleted",
    )
    .await;
    let gone_post = post_message_with_files(
        &client,
        &token,
        &f.channel_id,
        "one doomed attachment",
        std::slice::from_ref(&gone_id),
    )
    .await;
    if !set_fileinfo_column(&gone_id, "deleteat", "1").await {
        return;
    }

    let path = format!("/api/v4/files/{gone_id}/info");
    let ((go_status, go_body), (rs_status, rs_body)) = fetch_both_raw(&client, &token, &path).await;
    assert_eq!(go_status, 404, "`Get` filters DeleteAt = 0 unconditionally");
    assert_eq!(rs_status, go_status);
    assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &path);

    let hidden = format!("/api/v4/posts/{gone_post}/files/info");
    let (go, rs) = fetch_both(&client, &token, &hidden).await;
    assert_eq!(go, b"null", "without include_deleted the file is gone");
    assert_eq!(rs, go);

    let shown = format!("/api/v4/posts/{gone_post}/files/info?include_deleted=true");
    let (go, rs) = fetch_both(&client, &token, &shown).await;
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "with it, and manage_system, the row comes back"
    );
    let parsed: serde_json::Value = serde_json::from_slice(&go).expect("JSON");
    assert_eq!(parsed.as_array().expect("an array").len(), 1);
    assert_eq!(parsed[0]["delete_at"], 1);
}
