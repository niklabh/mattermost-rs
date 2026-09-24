//! Cross-server parity for **what a NULL column reads as** — [D-331] and [D-158], which are one
//! question: for every nullable column Go scans into a map, a slice or a pointer, what do SQL
//! `NULL` and a JSON `null` each become on the wire?
//!
//! Go's answer depends on *how* the store scans, not on the column type, which is why reading the
//! scanner alone produced wrong ports twice:
//!
//! | how Go scans | SQL `NULL` | JSON `null` |
//! |---|---|---|
//! | sqlx into a struct's **map** field (`StringMap`, `StringInterface`) | `{}` — reflectx allocates the map, `Scan` returns early | `null` — `json.Unmarshal` zeroes it |
//! | sqlx into a **pointer** field (`*ChannelBannerInfo`) | `null` — `database/sql` sets `**T` back to nil | the struct, every field `null` |
//! | sqlx into a **slice** field (`StringArray`) | `null` | `null` |
//! | a scalar `Select(&[]model.StringMap)` | nil map | nil map |
//! | a manual `row.Scan` into `[]byte`, then `json.Unmarshal` (`User.Get`) | a failed read, 500 | `null` |
//!
//! Every row here is planted with SQL on a fixture of this module's own, and Go's caches are
//! invalidated between the write and the read (holding [`GO_CACHE`]) so Go reads the row rather
//! than a copy it made earlier: its user cache re-encodes a nil map as `{}`, which is Go process
//! state this server does not reproduce. Go is always read first.

use crate::common;

use common::{
    GO, GO_CACHE, add_user_to_channel, assert_error_bodies_match_except_known_gaps, client,
    create_channel, create_plain_user, delete_channel, delete_plain_user, fetch_both_raw,
    fixture_pool, go_minted_token, invalidate_go_caches_locked, post_both_raw, post_message,
    stack_enabled,
};

/// Run one statement with `$1` bound to `id`, asserting it touched exactly `rows` rows.
async fn plant(pool: &sqlx::PgPool, sql: &str, id: &str, rows: u64) {
    let done = sqlx::query(sql)
        .bind(id)
        .execute(pool)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
    assert_eq!(done.rows_affected(), rows, "{sql} for {id}");
}

/// GET `path` on both after invalidating Go's caches; the two bodies must be byte-identical, and
/// the parsed Go body is returned for the assertion that says *which* shape it was.
async fn same_get(
    client: &reqwest::Client,
    admin: &str,
    token: &str,
    path: &str,
) -> serde_json::Value {
    let _lock = GO_CACHE.lock().await;
    invalidate_go_caches_locked(client, admin).await;
    let ((go_status, go), (rs_status, rs)) = fetch_both_raw(client, token, path).await;
    assert_eq!(go_status, 200, "{path}: {}", String::from_utf8_lossy(&go));
    assert_eq!(
        rs_status,
        go_status,
        "{path}: {}",
        String::from_utf8_lossy(&rs)
    );
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{path}"
    );
    serde_json::from_slice(&go).expect("a JSON body")
}

/// A fresh 26-character id for a row no API writes.
fn fresh_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("after 1970")
        .as_nanos();
    let mut n = nanos;
    let mut id = String::new();
    while id.len() < 26 {
        let digit = u32::try_from(n % 36).expect("under 36");
        id.push(char::from_digit(digit, 36).expect("a base-36 digit"));
        n = n / 36 + 7_919 * u128::from(digit + 1);
    }
    id
}

/// `users.props`, `notifyprops` and `timezone` — the one entity read both ways.
///
/// `GET /users/{id}` is `SqlUserStore.Get`, a manual scan into `[]byte`: any of the three NULL
/// is `unexpected end of JSON input` and a 500. `POST /users/ids` is `GetProfileByIds`, sqlx: a
/// NULL timezone is `{}` (the other two are `omitempty`). A jsonb `null` is `null` on both.
#[tokio::test]
async fn a_null_user_column_is_a_failed_get_and_an_empty_map_elsewhere() {
    if !stack_enabled() {
        return;
    }
    let Some(pool) = fixture_pool().await else {
        return;
    };
    let client = client();
    let admin = go_minted_token(&client).await;
    let (team, _) = common::a_team_and_channel_the_user_is_in(&client, &admin).await;
    let user = create_plain_user(&client, &admin, &team, "nullcols").await;
    let single = format!("/api/v4/users/{}", user.id);
    let restore = "UPDATE users SET props = '{}', notifyprops = '{}', \
                   timezone = '{\"automaticTimezone\":\"\",\"manualTimezone\":\"\",\"useAutomaticTimezone\":\"true\"}' \
                   WHERE id = $1";

    for column in ["props", "notifyprops", "timezone"] {
        plant(&pool, restore, &user.id, 1).await;
        plant(
            &pool,
            &format!("UPDATE users SET {column} = NULL WHERE id = $1"),
            &user.id,
            1,
        )
        .await;
        let _lock = GO_CACHE.lock().await;
        invalidate_go_caches_locked(&client, &admin).await;
        let ((go_status, go), (rs_status, rs)) = fetch_both_raw(&client, &admin, &single).await;
        assert_eq!(go_status, 500, "Go's manual scan fails on a NULL {column}");
        assert_eq!(rs_status, go_status, "{column}");
        assert_error_bodies_match_except_known_gaps(&go, &rs, &format!("NULL {column}"));
    }

    // The sqlx read of the same row: an empty map.
    plant(&pool, restore, &user.id, 1).await;
    plant(
        &pool,
        "UPDATE users SET timezone = NULL WHERE id = $1",
        &user.id,
        1,
    )
    .await;
    let ids = serde_json::to_vec(&[&user.id]).expect("encodes");
    {
        let _lock = GO_CACHE.lock().await;
        invalidate_go_caches_locked(&client, &admin).await;
        let ((go_status, go), (rs_status, rs)) =
            post_both_raw(&client, &admin, "/api/v4/users/ids", &ids).await;
        assert_eq!((go_status, rs_status), (200, 200));
        let parsed: serde_json::Value = serde_json::from_slice(&go).expect("JSON");
        assert_eq!(
            parsed[0]["timezone"],
            serde_json::json!({}),
            "sqlx allocates"
        );
        assert_eq!(
            String::from_utf8_lossy(&go),
            String::from_utf8_lossy(&rs),
            "POST /users/ids"
        );
    }

    // A jsonb `null` decodes on both paths, to nil.
    plant(
        &pool,
        "UPDATE users SET timezone = 'null'::jsonb WHERE id = $1",
        &user.id,
        1,
    )
    .await;
    let got = same_get(&client, &admin, &admin, &single).await;
    assert_eq!(got["timezone"], serde_json::Value::Null);
    {
        let _lock = GO_CACHE.lock().await;
        invalidate_go_caches_locked(&client, &admin).await;
        let ((_, go), (_, rs)) = post_both_raw(&client, &admin, "/api/v4/users/ids", &ids).await;
        let parsed: serde_json::Value = serde_json::from_slice(&go).expect("JSON");
        assert_eq!(parsed[0]["timezone"], serde_json::Value::Null);
        assert_eq!(String::from_utf8_lossy(&go), String::from_utf8_lossy(&rs));
    }

    // `GetChannelMembersTimezones` scans a bare `StringMap`: both shapes are a nil map, which
    // the app layer skips, so the channel's list is the other member's timezone alone.
    let channel = create_channel(&client, &admin, &team, "nullcols-tz").await;
    add_user_to_channel(&client, &admin, &channel, &user.id).await;
    let timezones = format!("/api/v4/channels/{channel}/timezones");
    same_get(&client, &admin, &admin, &timezones).await;
    plant(
        &pool,
        "UPDATE users SET timezone = NULL WHERE id = $1",
        &user.id,
        1,
    )
    .await;
    same_get(&client, &admin, &admin, &timezones).await;

    plant(&pool, restore, &user.id, 1).await;
    delete_channel(&client, &admin, &channel).await;
    delete_plain_user(&client, &admin, &user.id).await;
}

/// `sessions.props` (sqlx, `StringMap`) and `channelmembers.notifyprops` (sqlx, `StringMap`):
/// NULL is `{}`, jsonb `null` is `null`.
#[tokio::test]
async fn a_null_string_map_column_is_an_empty_object_and_a_json_null_is_null() {
    if !stack_enabled() {
        return;
    }
    let Some(pool) = fixture_pool().await else {
        return;
    };
    let client = client();
    let admin = go_minted_token(&client).await;
    let (team, _) = common::a_team_and_channel_the_user_is_in(&client, &admin).await;
    let user = create_plain_user(&client, &admin, &team, "nullmaps").await;
    let channel = create_channel(&client, &admin, &team, "nullmaps").await;
    add_user_to_channel(&client, &admin, &channel, &user.id).await;

    let sessions = format!("/api/v4/users/{}/sessions", user.id);
    let member = format!("/api/v4/channels/{channel}/members/{}", user.id);
    for (value, expected) in [
        ("NULL", serde_json::json!({})),
        ("'null'::jsonb", serde_json::Value::Null),
    ] {
        plant(
            &pool,
            &format!("UPDATE sessions SET props = {value} WHERE userid = $1"),
            &user.id,
            1,
        )
        .await;
        let got = same_get(&client, &admin, &admin, &sessions).await;
        assert_eq!(got[0]["props"], expected, "sessions.props = {value}");

        plant(
            &pool,
            &format!(
                "UPDATE channelmembers SET notifyprops = {value} \
                 WHERE userid = $1 AND channelid = '{channel}'"
            ),
            &user.id,
            1,
        )
        .await;
        let got = same_get(&client, &admin, &admin, &member).await;
        assert_eq!(got["notify_props"], expected, "notifyprops = {value}");
    }

    delete_channel(&client, &admin, &channel).await;
    delete_plain_user(&client, &admin, &user.id).await;
}

/// `jobs.data` (sqlx, `StringMap`), on rows planted whole — no route creates a job with a chosen
/// `data`.
#[tokio::test]
async fn a_null_job_data_column_is_an_empty_object_and_a_json_null_is_null() {
    if !stack_enabled() {
        return;
    }
    let Some(pool) = fixture_pool().await else {
        return;
    };
    let client = client();
    let admin = go_minted_token(&client).await;
    for (value, expected) in [
        ("NULL", serde_json::json!({})),
        ("'null'::jsonb", serde_json::Value::Null),
    ] {
        let id = fresh_id();
        plant(
            &pool,
            &format!(
                "INSERT INTO jobs (id, type, priority, createat, startat, lastactivityat, status, \
                 progress, data) VALUES ($1, 'data_retention', 0, 1700000000000, 0, 0, \
                 'success', 100, {value})"
            ),
            &id,
            1,
        )
        .await;
        let got = same_get(&client, &admin, &admin, &format!("/api/v4/jobs/{id}")).await;
        assert_eq!(got["data"], expected, "jobs.data = {value}");
        plant(&pool, "DELETE FROM jobs WHERE id = $1", &id, 1).await;
    }
}

/// `channels.bannerinfo` — the **pointer** shape. NULL is `null`; a jsonb `null` is a
/// `ChannelBannerInfo` with every field `null`, because `database/sql` allocates a fresh struct
/// for any non-NULL value and `json.Unmarshal("null")` leaves it alone.
#[tokio::test]
async fn a_null_banner_is_null_and_a_json_null_banner_is_an_empty_struct() {
    if !stack_enabled() {
        return;
    }
    let Some(pool) = fixture_pool().await else {
        return;
    };
    let client = client();
    let admin = go_minted_token(&client).await;
    let (team, _) = common::a_team_and_channel_the_user_is_in(&client, &admin).await;
    let channel = create_channel(&client, &admin, &team, "nullbanner").await;
    let path = format!("/api/v4/channels/{channel}");
    for (value, expected) in [
        ("NULL", serde_json::Value::Null),
        (
            "'null'::jsonb",
            serde_json::json!({"enabled": null, "text": null, "background_color": null}),
        ),
    ] {
        plant(
            &pool,
            &format!("UPDATE channels SET bannerinfo = {value} WHERE id = $1"),
            &channel,
            1,
        )
        .await;
        let got = same_get(&client, &admin, &admin, &path).await;
        assert_eq!(got["banner_info"], expected, "bannerinfo = {value}");
    }
    delete_channel(&client, &admin, &channel).await;
}

/// The **slice** shape over `varchar` columns — `outgoingwebhooks.triggerwords`/`callbackurls`
/// and `oauthapps.callbackurls`. NULL and the text `null` are both `null`; the text `null` is what
/// `StringArray.Value` itself writes for a nil slice, and it used to fail the read here.
#[tokio::test]
async fn a_null_string_array_column_is_null_either_way() {
    if !stack_enabled() {
        return;
    }
    let Some(pool) = fixture_pool().await else {
        return;
    };
    let client = client();
    let admin = go_minted_token(&client).await;
    let (team, _) = common::a_team_and_channel_the_user_is_in(&client, &admin).await;
    // A channel of its own: with its trigger words nulled the hook fires on **every** post in its
    // channel, and in the shared channel that forwarded every other suite's served post there
    // (`licensed_sweep`'s priority post, 2026-09-25). A run that panics before the cleanup below
    // leaves the hook live, so the previous run's leftovers go first.
    sqlx::query("DELETE FROM outgoingwebhooks WHERE displayname = 'mmrs-parity-nullcols'")
        .execute(&pool)
        .await
        .expect("stale hooks are removed");
    let channel = create_channel(&client, &admin, &team, "nullhooks").await;

    let created = |path: &'static str, body: serde_json::Value| {
        let client = &client;
        let admin = &admin;
        async move {
            let response = client
                .post(format!("{GO}{path}"))
                .header("Authorization", format!("Bearer {admin}"))
                .json(&body)
                .send()
                .await
                .expect("Go answers");
            assert_eq!(response.status(), 201, "{path}");
            let value: serde_json::Value = response.json().await.expect("JSON");
            value["id"].as_str().expect("an id").to_owned()
        }
    };
    let hook = created(
        "/api/v4/hooks/outgoing",
        serde_json::json!({
            "team_id": team, "channel_id": channel, "display_name": "mmrs-parity-nullcols",
            "trigger_words": ["nullcols"], "callback_urls": ["http://localhost:9/x"],
        }),
    )
    .await;
    let app = created(
        "/api/v4/oauth/apps",
        serde_json::json!({
            "name": "nullcols", "description": "nullcols", "homepage": "http://localhost:9",
            "callback_urls": ["http://localhost:9/cb"],
        }),
    )
    .await;

    for value in ["NULL", "'null'"] {
        plant(
            &pool,
            &format!(
                "UPDATE outgoingwebhooks SET triggerwords = {value}, callbackurls = {value} \
                 WHERE id = $1"
            ),
            &hook,
            1,
        )
        .await;
        let got = same_get(
            &client,
            &admin,
            &admin,
            &format!("/api/v4/hooks/outgoing/{hook}"),
        )
        .await;
        assert_eq!(got["trigger_words"], serde_json::Value::Null, "{value}");
        assert_eq!(got["callback_urls"], serde_json::Value::Null, "{value}");

        plant(
            &pool,
            &format!("UPDATE oauthapps SET callbackurls = {value} WHERE id = $1"),
            &app,
            1,
        )
        .await;
        let got = same_get(
            &client,
            &admin,
            &admin,
            &format!("/api/v4/oauth/apps/{app}"),
        )
        .await;
        assert_eq!(got["callback_urls"], serde_json::Value::Null, "{value}");
    }

    let _ = client
        .delete(format!("{GO}/api/v4/hooks/outgoing/{hook}"))
        .header("Authorization", format!("Bearer {admin}"))
        .send()
        .await;
    let _ = client
        .delete(format!("{GO}/api/v4/oauth/apps/{app}"))
        .header("Authorization", format!("Bearer {admin}"))
        .send()
        .await;
    delete_channel(&client, &admin, &channel).await;
}

/// `drafts.props` (sqlx, `StringInterface` over `varchar`) and `drafts.fileids` (`StringArray`,
/// `omitempty`): NULL props is `{}`, text `null` props is `null`, and a text `null` file list is
/// omitted rather than a failed read.
#[tokio::test]
async fn a_null_draft_column_reads_as_go_reads_it() {
    if !stack_enabled() {
        return;
    }
    let Some(pool) = fixture_pool().await else {
        return;
    };
    let client = client();
    let admin = go_minted_token(&client).await;
    let (team, _) = common::a_team_and_channel_the_user_is_in(&client, &admin).await;
    let user = create_plain_user(&client, &admin, &team, "nulldraft").await;
    let channel = create_channel(&client, &admin, &team, "nulldraft").await;
    add_user_to_channel(&client, &admin, &channel, &user.id).await;
    let response = client
        .post(format!("{GO}/api/v4/drafts"))
        .header("Authorization", format!("Bearer {}", user.token))
        .json(&serde_json::json!({
            "user_id": user.id, "channel_id": channel, "message": "nullcols",
        }))
        .send()
        .await
        .expect("Go answers");
    assert!(response.status().is_success(), "the draft is saved");

    let path = format!("/api/v4/users/{}/teams/{team}/drafts", user.id);
    for (props, expected) in [
        ("NULL", serde_json::json!({})),
        ("'null'", serde_json::Value::Null),
    ] {
        plant(
            &pool,
            &format!("UPDATE drafts SET props = {props}, fileids = 'null' WHERE userid = $1"),
            &user.id,
            1,
        )
        .await;
        let got = same_get(&client, &admin, &user.token, &path).await;
        assert_eq!(got[0]["props"], expected, "drafts.props = {props}");
        assert!(got[0].get("file_ids").is_none(), "omitempty");
    }

    delete_channel(&client, &admin, &channel).await;
    delete_plain_user(&client, &admin, &user.id).await;
}

/// `threads.participants` read through `COALESCE(Threads.Participants, '[]')` on the collapsed
/// page: the coalesce catches a SQL NULL and not a jsonb `null`, which `StringArray.Scan` turns
/// into a nil slice — the same empty participant list, not a failed read.
#[tokio::test]
async fn a_json_null_participant_list_is_an_empty_one() {
    if !stack_enabled() {
        return;
    }
    let Some(pool) = fixture_pool().await else {
        return;
    };
    let client = client();
    let admin = go_minted_token(&client).await;
    let (team, _) = common::a_team_and_channel_the_user_is_in(&client, &admin).await;
    let channel = create_channel(&client, &admin, &team, "nullthread").await;
    let root = post_message(&client, &admin, &channel, "root", None).await;
    post_message(&client, &admin, &channel, "reply", Some(&root)).await;

    let path = format!("/api/v4/channels/{channel}/posts?collapsedThreads=true");
    for value in ["NULL", "'null'::jsonb"] {
        plant(
            &pool,
            &format!("UPDATE threads SET participants = {value} WHERE postid = $1"),
            &root,
            1,
        )
        .await;
        let got = same_get(&client, &admin, &admin, &path).await;
        assert_eq!(
            got["posts"][&root]["participants"],
            serde_json::Value::Null,
            "participants = {value}"
        );
    }
    delete_channel(&client, &admin, &channel).await;
}

/// A NULL scanned into a plain Go **`string`** — `posts.hashtags` — is a scan error, not `""`:
/// `database/sql` cannot convert NULL to a string, and `GET /posts/{id}` is a 500 on both.
#[tokio::test]
async fn a_null_into_a_plain_string_is_a_failed_read() {
    if !stack_enabled() {
        return;
    }
    let Some(pool) = fixture_pool().await else {
        return;
    };
    let client = client();
    let admin = go_minted_token(&client).await;
    let (team, _) = common::a_team_and_channel_the_user_is_in(&client, &admin).await;
    let channel = create_channel(&client, &admin, &team, "nullhashtags").await;
    let post = post_message(&client, &admin, &channel, "hashtags", None).await;
    plant(
        &pool,
        "UPDATE posts SET hashtags = NULL WHERE id = $1",
        &post,
        1,
    )
    .await;
    {
        let _lock = GO_CACHE.lock().await;
        invalidate_go_caches_locked(&client, &admin).await;
        let path = format!("/api/v4/posts/{post}");
        let ((go_status, go), (rs_status, rs)) = fetch_both_raw(&client, &admin, &path).await;
        assert_eq!(go_status, 500, "{}", String::from_utf8_lossy(&go));
        assert_eq!(rs_status, go_status);
        assert_error_bodies_match_except_known_gaps(&go, &rs, "NULL hashtags");
    }
    plant(
        &pool,
        "UPDATE posts SET hashtags = '' WHERE id = $1",
        &post,
        1,
    )
    .await;
    delete_channel(&client, &admin, &channel).await;
}
