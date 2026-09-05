//! Cross-server parity for `GET /api/v4/users/{user_id}/teams/{team_id}/drafts` — `getDrafts`.
//!
//! ```sh
//! scripts/parity.sh -p mm-api --test parity drafts
//! ```
//!
//! # The `{user_id}` segment does nothing
//!
//! The handler reads `c.AppContext.Session().UserId`, never `c.Params.UserId`, so the segment is
//! decorative: another user's id in the path returns **your** drafts, and a segment that is not
//! an id at all is a 200. [`the_path_user_is_ignored_entirely`].
//!
//! # `null`, not `[]`
//!
//! Go's slice is left nil by `SelectBuilder`, and `json.NewEncoder(w).Encode` gives it a trailing
//! newline, so a user with no drafts reads exactly `null\n`. [`no_drafts_is_null_with_a_newline`].

use crate::common;

use common::{
    GO, RUST, assert_error_bodies_match_except_known_gaps, client, create_channel_typed,
    create_plain_user, create_team, fetch_both_raw, go_minted_token, logged_in_user_id,
    purge_api_fixtures, stack_enabled,
};

struct Fixture {
    team_id: String,
    /// A team the plain user is **not** in, so `view_team` can refuse.
    other_team_id: String,
    channel_id: String,
    /// A DM, whose `Channels.TeamId` is empty — so it belongs to every team's draft list.
    dm_channel_id: String,
    plain_id: String,
    plain_token: String,
}

static FIXTURE: tokio::sync::OnceCell<Fixture> = tokio::sync::OnceCell::const_new();

async fn fixture(client: &reqwest::Client, token: &str) -> &'static Fixture {
    FIXTURE
        .get_or_init(|| async {
            purge_api_fixtures().await;

            let team_id = create_team(client, token, "drafts").await;
            let other_team_id = create_team(client, token, "draftsb").await;
            let channel_id = create_channel_typed(client, token, &team_id, "drafts", "O").await;
            let left_channel =
                create_channel_typed(client, token, &team_id, "draftsleft", "O").await;

            let plain = create_plain_user(client, token, &team_id, "drafts").await;

            // Three drafts for the admin: a plain one, one in a thread, and one in a channel the
            // admin is about to leave. The last is the fixture row the `ChannelMembers` join
            // exists to drop — without it that join is dead code.
            upsert(
                client,
                token,
                &channel_id,
                "",
                "first draft",
                serde_json::json!({}),
            )
            .await;
            // Non-empty `props` and `priority`, because both are `varchar` columns holding JSON
            // this port parses itself — an all-`{}` fixture cannot tell a working decoder from
            // one that returns the empty map for everything.
            upsert(
                client,
                token,
                &channel_id,
                ROOT_ID,
                "a reply draft",
                serde_json::json!({
                    "props": {"zz_keep": "kept"},
                    "priority": {"priority": "important", "requested_ack": false},
                }),
            )
            .await;
            upsert(
                client,
                token,
                &left_channel,
                "",
                "written then left",
                serde_json::json!({}),
            )
            .await;
            leave_channel(client, token, &left_channel, logged_in_user_id()).await;

            // A DM has no team, and the store's team filter has to admit it anyway.
            let dm_channel_id = open_dm(client, token, logged_in_user_id(), &plain.id).await;
            upsert(
                client,
                token,
                &dm_channel_id,
                "",
                "dm draft",
                serde_json::json!({}),
            )
            .await;

            // A **NULL** `Props` column, which `upsertDraft` never writes — it always stores at
            // least `{}`. Go's `StringInterface.Scan` is handed a freshly made empty map and
            // returns without touching it on NULL, so the wire shows `{}` and not `null`; the
            // difference is invisible until a row actually holds NULL. Probed against Go before
            // this was written.
            plant_null_props(&dm_channel_id).await;

            Fixture {
                team_id,
                other_team_id,
                channel_id,
                dm_channel_id,
                plain_id: plain.id,
                plain_token: plain.token,
            }
        })
        .await
}

/// A root id that names no post. `upsertDraft` never resolves it, so it needs no fixture post —
/// and using one would make the draft's `root_id` depend on another suite's rows.
const ROOT_ID: &str = "zzzzzzzzzzzzzzzzzzzzzzzzzz";

async fn upsert(
    client: &reqwest::Client,
    token: &str,
    channel_id: &str,
    root_id: &str,
    message: &str,
    extra: serde_json::Value,
) {
    let mut body = serde_json::json!({
        "channel_id": channel_id,
        "root_id": root_id,
        "message": message,
    });
    for (key, value) in extra.as_object().into_iter().flatten() {
        body[key] = value.clone();
    }
    let response = client
        .post(format!("{GO}/api/v4/drafts"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&body)
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "saving a draft in {channel_id} failed: {}",
        response.text().await.unwrap_or_default()
    );
}

/// Write a NULL into a `Props` column no API can leave NULL.
async fn plant_null_props(channel_id: &str) {
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
    let _ = sqlx::query("UPDATE drafts SET props = NULL WHERE channelid = $1")
        .bind(channel_id)
        .execute(&pool)
        .await;
}

/// `POST /channels/direct` — the DM's `TeamId` is `''`, which is the point.
async fn open_dm(client: &reqwest::Client, token: &str, a: &str, b: &str) -> String {
    let response = client
        .post(format!("{GO}/api/v4/channels/direct"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!([a, b]))
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "opening a DM failed: {}",
        response.text().await.unwrap_or_default()
    );
    let created: serde_json::Value = response.json().await.expect("the channel decodes");
    created["id"].as_str().expect("an id").to_owned()
}

async fn leave_channel(client: &reqwest::Client, token: &str, channel_id: &str, user_id: &str) {
    let response = client
        .delete(format!(
            "{GO}/api/v4/channels/{channel_id}/members/{user_id}"
        ))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "leaving {channel_id} failed: {}",
        response.text().await.unwrap_or_default()
    );
}

fn path(user_id: &str, team_id: &str) -> String {
    format!("/api/v4/users/{user_id}/teams/{team_id}/drafts")
}

/// The list is byte-identical, and it holds exactly the two drafts the join keeps.
#[tokio::test]
async fn the_draft_list_is_byte_identical() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let p = path(logged_in_user_id(), &f.team_id);
    let ((go_status, go), (rs_status, rs)) = fetch_both_raw(&client, &token, &p).await;
    assert_eq!(go_status, 200);
    assert_eq!(rs_status, go_status);
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{p} must be byte-identical"
    );
    assert_eq!(
        go.last(),
        Some(&b'\n'),
        "json.NewEncoder(w).Encode appends the newline json.Marshal does not"
    );

    let parsed: serde_json::Value = serde_json::from_slice(&go).expect("JSON");
    let drafts = parsed.as_array().expect("an array");
    // Four drafts were saved. The one in the channel the admin left is dropped by the store's
    // `ChannelMembers` join; the DM survives the team filter because its `TeamId` is empty.
    assert_eq!(drafts.len(), 3, "one draft in, one out: {parsed}");
    let channels: Vec<&str> = drafts
        .iter()
        .map(|d| d["channel_id"].as_str().unwrap_or_default())
        .collect();
    assert!(
        channels.contains(&f.dm_channel_id.as_str()),
        "a DM has no team and belongs to every team's list: {parsed}"
    );
    assert_eq!(
        channels.iter().filter(|c| **c == f.channel_id).count(),
        2,
        "and the other two are the ones in the channel the admin is still in: {parsed}"
    );

    // `ORDER BY UpdateAt DESC`, so the DM draft — saved last — is first.
    assert_eq!(drafts[0]["channel_id"], f.dm_channel_id.as_str());
    assert_eq!(drafts[1]["root_id"], ROOT_ID);
    assert_eq!(drafts[2]["root_id"], "");

    // `Metadata` is set on every success, files or none, because `omitempty` on a pointer tests
    // the pointer. `delete_at` is never selected, so it is Go's zero rather than the column's.
    for draft in drafts {
        assert_eq!(draft["metadata"], serde_json::json!({}));
        assert_eq!(draft["delete_at"], 0);
        assert!(
            draft.get("file_ids").is_none(),
            "empty file ids are omitted"
        );
    }

    // The two JSON-in-varchar columns, on the one draft that carries them. `Props` has no
    // `omitempty`, so an empty map is `{}`; `Priority` has one, so an empty map is absent.
    assert_eq!(drafts[1]["props"], serde_json::json!({"zz_keep": "kept"}));
    assert_eq!(
        drafts[1]["priority"],
        serde_json::json!({"priority": "important", "requested_ack": false})
    );
    // The DM draft's column is NULL, and NULL is `{}` on the wire — not `null`, which is what
    // the JSON text `null` in that column would give.
    assert_eq!(drafts[0]["props"], serde_json::json!({}));
    assert!(
        drafts[0].get("priority").is_none(),
        "an empty priority map is omitted: {parsed}"
    );
}

/// No drafts is `null`, not `[]` — and it still carries the newline.
#[tokio::test]
async fn no_drafts_is_null_with_a_newline() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let p = path(&f.plain_id, &f.team_id);
    let ((go_status, go), (rs_status, rs)) = fetch_both_raw(&client, &f.plain_token, &p).await;
    assert_eq!(go_status, 200);
    assert_eq!(rs_status, go_status);
    assert_eq!(go, b"null\n", "a nil slice marshals to null");
    assert_eq!(rs, go);
}

/// The path's user id is never read: another user's id, and a segment that is no id at all.
#[tokio::test]
async fn the_path_user_is_ignored_entirely() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let mine = path(logged_in_user_id(), &f.team_id);
    let ((_, mine_go), _) = fetch_both_raw(&client, &token, &mine).await;

    for segment in [f.plain_id.as_str(), "short", "me"] {
        let p = path(segment, &f.team_id);
        let ((go_status, go), (rs_status, rs)) = fetch_both_raw(&client, &token, &p).await;
        assert_eq!(go_status, 200, "{p}: no id in this handler is validated");
        assert_eq!(rs_status, go_status, "{p}");
        assert_eq!(
            String::from_utf8_lossy(&go),
            String::from_utf8_lossy(&rs),
            "{p} must be byte-identical"
        );
        assert_eq!(
            String::from_utf8_lossy(&go),
            String::from_utf8_lossy(&mine_go),
            "{p} answers the caller's own drafts, whoever the segment names"
        );
    }
}

/// `view_team` refuses a team the caller is not in.
#[tokio::test]
async fn a_team_the_caller_is_not_in_is_a_403() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let p = path(&f.plain_id, &f.other_team_id);
    let ((go_status, go_body), (rs_status, rs_body)) =
        fetch_both_raw(&client, &f.plain_token, &p).await;
    assert_eq!(go_status, 403, "the plain user is not in the other team");
    assert_eq!(rs_status, go_status, "{p}: statuses must match");
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &p);
    assert_eq!(go["id"], "api.context.permissions.app_error");
    // **Which permission Go names is not observable here.** `SetPermissionError` puts it in
    // `DetailedError`, and the api boundary wipes that unless `EnableDeveloper` is on — so the
    // `create_post`/`view_team` mismatch this handler carries is in the port and in the log, and
    // nowhere a client can read it.
    assert_eq!(go["detailed_error"], "");
}

/// An admin's system-wide `view_team` covers a team that does not exist, so a bogus team id is a
/// 200 for them and a 403 for everybody else — and the 200 is **not empty**, because a DM draft
/// has no team and belongs to every list.
#[tokio::test]
async fn a_team_that_does_not_exist_splits_by_caller() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let p = path(logged_in_user_id(), "zzzzzzzzzzzzzzzzzzzzzzzzzz");
    let ((go_status, go), (rs_status, rs)) = fetch_both_raw(&client, &token, &p).await;
    assert_eq!(go_status, 200, "the admin's system-wide view_team passes");
    assert_eq!(rs_status, go_status);
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{p} must be byte-identical"
    );
    // **Not empty**, and that is the team filter's second half doing its job: `TeamId = ''`
    // admits every DM draft to every team's list, including a team that does not exist.
    let parsed: serde_json::Value = serde_json::from_slice(&go).expect("JSON");
    let drafts = parsed.as_array().expect("an array");
    assert_eq!(drafts.len(), 1, "only the teamless one: {parsed}");
    assert_eq!(drafts[0]["channel_id"], f.dm_channel_id.as_str());

    let ((go_status, go_body), (rs_status, rs_body)) =
        fetch_both_raw(&client, &f.plain_token, &p).await;
    assert_eq!(go_status, 403, "the plain user has no such membership");
    assert_eq!(rs_status, go_status);
    assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &p);
}

/// No session is a 401 on both, before anything else runs.
#[tokio::test]
async fn no_session_is_a_401_on_both() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    // `logged_in_user_id` is only populated once something has logged in, and this test never
    // does — so the segment is a literal. It is ignored by the handler anyway.
    let p = path("zzzzzzzzzzzzzzzzzzzzzzzzzz", "zzzzzzzzzzzzzzzzzzzzzzzzzz");
    for base in [GO, RUST] {
        let response = client
            .get(format!("{base}{p}"))
            .send()
            .await
            .expect("reachable");
        assert_eq!(response.status().as_u16(), 401, "{base}{p}");
    }
}

/// `POST` and `DELETE` on this path are Go's.
#[tokio::test]
async fn other_methods_are_forwarded() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    // A team id nothing asserts on: these are writes as far as Go is concerned.
    let p = path(logged_in_user_id(), "zzzzzzzzzzzzzzzzzzzzzzzzzz");
    for method in [reqwest::Method::POST, reqwest::Method::DELETE] {
        let rs = client
            .request(method.clone(), format!("{RUST}{p}"))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .expect("we answer");
        assert_eq!(
            rs.headers()
                .get("x-mmrs-served-by")
                .and_then(|v| v.to_str().ok()),
            Some("go"),
            "{method} {p} must be forwarded"
        );
    }
}
