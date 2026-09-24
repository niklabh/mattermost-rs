//! Cross-server parity for the two writes behind `PUT /api/v4/channels/{channel_id}/patch` that
//! were forwarded until D-234: turning `group_constrained` **on**, which removes every member no
//! linked group vouches for, and a non-empty `default_category_name`, which files the channel in
//! the patching user's sidebar.
//!
//! ```sh
//! scripts/parity.sh --test parity channel_patch_writes
//! ```
//!
//! Each server gets its own team or user and its own channel, because both writes are one-way. The
//! results are reduced to summaries with the ids replaced by their role, and the summaries are
//! compared, so a mismatch names the fact that differs.

use std::time::Duration;

use crate::common;

use common::{
    GO, RUST, SocketProbe, add_user_to_channel, client, create_channel_typed, create_plain_user,
    create_team, fixture_pool, go_minted_token, invalidate_go_caches, stack_enabled,
};

/// One request, returning `(status, served here, body)`.
async fn send(
    http: &reqwest::Client,
    base: &str,
    method: reqwest::Method,
    path: &str,
    token: &str,
    body: Option<&serde_json::Value>,
) -> (u16, bool, String) {
    let mut request = http
        .request(method, format!("{base}{path}"))
        .header("Authorization", format!("Bearer {token}"));
    if let Some(body) = body {
        request = request.json(body);
    }
    let response = request
        .send()
        .await
        .unwrap_or_else(|e| panic!("{base} unreachable: {e}"));
    let status = response.status().as_u16();
    let served = response
        .headers()
        .get("x-mmrs-served-by")
        .and_then(|v| v.to_str().ok())
        == Some("rust");
    (status, served, response.text().await.unwrap_or_default())
}

async fn patch(
    http: &reqwest::Client,
    base: &str,
    token: &str,
    channel: &str,
    body: serde_json::Value,
) -> String {
    let (status, served, answer) = send(
        http,
        base,
        reqwest::Method::PUT,
        &format!("/api/v4/channels/{channel}/patch"),
        token,
        Some(&body),
    )
    .await;
    assert_eq!(status, 200, "{base}: the patch {body}: {answer}");
    assert_eq!(
        served,
        base == RUST,
        "{base}: the patch {body} was answered by the other server"
    );
    answer
}

// -------------------------------------------------------------------------------------------
// group_constrained off → on
// -------------------------------------------------------------------------------------------

/// A group linked to `channel` holding `members`.
async fn plant_group(pool: &sqlx::PgPool, tag: &str, channel: &str, members: &[&str]) {
    let id = format!("mmrspwgrp{tag:0>17}");
    for statement in [
        "DELETE FROM groupmembers WHERE groupid = $1",
        "DELETE FROM groupchannels WHERE groupid = $1",
        "DELETE FROM usergroups WHERE id = $1",
    ] {
        sqlx::query(statement)
            .bind(&id)
            .execute(pool)
            .await
            .expect("the old group clears");
    }
    sqlx::query(
        "INSERT INTO usergroups
           (id, name, displayname, description, source, remoteid,
            createat, updateat, deleteat, allowreference)
         VALUES ($1, $2, $2, 'a group this suite made', 'custom', NULL,
                 1700000000000, 1700000000000, 0, TRUE)",
    )
    .bind(&id)
    .bind(format!("mmrs-pwgrp-{tag}"))
    .execute(pool)
    .await
    .expect("the group is written");
    sqlx::query(
        "INSERT INTO groupchannels (groupid, autoadd, schemeadmin, createat, deleteat, updateat, channelid)
         VALUES ($1, false, false, 1700000000000, 0, 1700000000000, $2)",
    )
    .bind(&id)
    .bind(channel)
    .execute(pool)
    .await
    .expect("the link is written");
    for member in members {
        sqlx::query(
            "INSERT INTO groupmembers (groupid, userid, createat, deleteat)
             VALUES ($1, $2, 1700000000000, 0)",
        )
        .bind(&id)
        .bind(member)
        .execute(pool)
        .await
        .expect("the membership is written");
    }
}

/// Which of `people` are still members of `channel`, by role name, sorted.
async fn remaining(pool: &sqlx::PgPool, channel: &str, people: &[(&str, &str)]) -> Vec<String> {
    let rows: Vec<(String,)> =
        sqlx::query_as("SELECT userid FROM channelmembers WHERE channelid = $1")
            .bind(channel)
            .fetch_all(pool)
            .await
            .expect("the members read");
    let mut names: Vec<String> = rows
        .iter()
        .map(|(id,)| {
            people.iter().find(|(person, _)| person == id).map_or_else(
                || format!("<stranger {id}>"),
                |(_, name)| (*name).to_owned(),
            )
        })
        .collect();
    names.sort();
    names
}

/// The channel's posts as `type`s, in order — the removal notices are what the sweep leaves behind.
async fn post_types(pool: &sqlx::PgPool, channel: &str) -> Vec<String> {
    let rows: Vec<(String,)> =
        sqlx::query_as("SELECT type FROM posts WHERE channelid = $1 ORDER BY createat, id")
            .bind(channel)
            .fetch_all(pool)
            .await
            .expect("the posts read");
    rows.into_iter().map(|(kind,)| kind).collect()
}

/// Turning `group_constrained` on removes every member no linked group vouches for — the admin
/// who made the patch included — and keeps the one the group holds. Go does it on a goroutine
/// after answering, and so does this server; the test therefore waits for the sweep to settle
/// on each side rather than reading the members straight after the `200`.
#[tokio::test]
async fn turning_group_constrained_on_sweeps_the_members_no_group_vouches_for() {
    if !stack_enabled() {
        return;
    }
    let Some(pool) = fixture_pool().await else {
        return;
    };
    let http = client();
    let admin = go_minted_token(&http).await;
    let admin_id = common::logged_in_user_id();

    let mut results = Vec::new();
    for (base, side) in [(GO, "go"), (RUST, "rs")] {
        let tag = format!("pwgc{side}");
        let team = create_team(&http, &admin, &tag).await;
        let channel = create_channel_typed(&http, &admin, &team, &tag, "O").await;
        let vouched = create_plain_user(&http, &admin, &team, &format!("{tag}v")).await;
        let outsider = create_plain_user(&http, &admin, &team, &format!("{tag}o")).await;
        for user in [&vouched.id, &outsider.id] {
            add_user_to_channel(&http, &admin, &channel, user).await;
        }
        plant_group(&pool, &tag, &channel, &[&vouched.id]).await;
        invalidate_go_caches(&http, &admin).await;
        let people = [
            (admin_id, "admin"),
            (vouched.id.as_str(), "vouched"),
            (outsider.id.as_str(), "outsider"),
        ];
        assert_eq!(
            remaining(&pool, &channel, &people).await,
            ["admin", "outsider", "vouched"],
            "{base}: the fixture"
        );

        let answer = patch(
            &http,
            base,
            &admin,
            &channel,
            serde_json::json!({"group_constrained": true}),
        )
        .await;
        let answered: serde_json::Value = serde_json::from_str(&answer).expect("a channel");
        assert_eq!(answered["group_constrained"], true, "{base}: {answer}");

        // The sweep is asynchronous on both servers: wait for it, then a little longer, so a
        // removal it should not have made has had time to happen too.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while remaining(&pool, &channel, &people).await != ["vouched"]
            && tokio::time::Instant::now() < deadline
        {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;

        results.push((
            remaining(&pool, &channel, &people).await,
            post_types(&pool, &channel).await,
        ));
        for user in [&vouched.id, &outsider.id] {
            common::delete_plain_user(&http, &admin, user).await;
        }
    }
    let rust = results.pop().expect("two results");
    let go = results.pop().expect("two results");
    assert_eq!(go.0, ["vouched"], "Go swept everyone else: {go:?}");
    assert_eq!(rust, go, "what the sweep left: members and posts");
}

// -------------------------------------------------------------------------------------------
// default_category_name
// -------------------------------------------------------------------------------------------

/// The user's sidebar on `team`, read through `base`, as `(type, display_name, sorting, muted,
/// channels)` with the patched channel written `<ch>` and the other channel `<other>`.
async fn sidebar(
    http: &reqwest::Client,
    base: &str,
    user: &common::PlainUser,
    team: &str,
    channel: &str,
    other: &str,
) -> Vec<serde_json::Value> {
    let (status, _, body) = send(
        http,
        base,
        reqwest::Method::GET,
        &format!("/api/v4/users/{}/teams/{team}/channels/categories", user.id),
        &user.token,
        None,
    )
    .await;
    assert_eq!(status, 200, "{base}: categories: {body}");
    let value: serde_json::Value = serde_json::from_str(&body).expect("categories");
    value["categories"]
        .as_array()
        .expect("a list")
        .iter()
        .map(|category| summarise(category, channel, other))
        .collect()
}

fn summarise(category: &serde_json::Value, channel: &str, other: &str) -> serde_json::Value {
    let channels: Vec<String> = category["channel_ids"]
        .as_array()
        .map(|ids| {
            ids.iter()
                .filter_map(|id| id.as_str())
                .map(|id| {
                    if id == channel {
                        "<ch>".to_owned()
                    } else if id == other {
                        "<other>".to_owned()
                    } else {
                        id.to_owned()
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    serde_json::json!([
        category["type"],
        category["display_name"],
        category["sorting"],
        category["muted"],
        channels
    ])
}

/// Every `sidebar_category_*` event on `socket`, in order: the event name, and for an update the
/// summarised categories it carried.
fn sidebar_events(socket: &SocketProbe, channel: &str, other: &str) -> Vec<serde_json::Value> {
    socket
        .frames()
        .into_iter()
        .filter(|frame| {
            frame["event"]
                .as_str()
                .is_some_and(|name| name.starts_with("sidebar_category_"))
        })
        .map(|frame| {
            let payload = frame["data"]["updatedCategories"]
                .as_str()
                .map(|raw| {
                    let categories: serde_json::Value =
                        serde_json::from_str(raw).expect("a JSON string");
                    serde_json::Value::Array(
                        categories
                            .as_array()
                            .expect("an array")
                            .iter()
                            .map(|category| summarise(category, channel, other))
                            .collect(),
                    )
                })
                .unwrap_or(serde_json::Value::Null);
            serde_json::json!([frame["event"], payload])
        })
        .collect()
}

/// Three patches in a row, each against the patching member's own sidebar:
///
/// 1. a new name → a custom category is **created** holding the channel, and **Channels** — where
///    the channel sat as an orphan — is written back without it: `created`, then `updated`;
/// 2. the same name in another case → the channel is already there (`EqualFold`): no event;
/// 3. the name of a custom category the user made, in another case → the channel is **prepended**
///    there and removed from the first: one `updated` carrying both.
///
/// Compared after every step: the sidebar as each server reads it back, and the events.
#[tokio::test]
async fn a_default_category_name_files_the_channel_in_the_patchers_sidebar() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let admin = go_minted_token(&http).await;
    let team = create_team(&http, &admin, "pwcat").await;
    let other = create_channel_typed(&http, &admin, &team, "pwcatother", "O").await;

    let mut sides = Vec::new();
    for (base, side) in [(GO, "go"), (RUST, "rs")] {
        let user = create_plain_user(&http, &admin, &team, &format!("pwcat{side}")).await;
        let channel =
            create_channel_typed(&http, &admin, &team, &format!("pwcat{side}"), "O").await;
        for id in [&channel, &other] {
            add_user_to_channel(&http, &admin, id, &user.id).await;
        }
        // The user's own custom category for step 3, holding the other channel.
        let (status, _, body) = send(
            &http,
            base,
            reqwest::Method::POST,
            &format!("/api/v4/users/{}/teams/{team}/channels/categories", user.id),
            &user.token,
            Some(&serde_json::json!({
                "user_id": user.id, "team_id": team,
                "display_name": "Mmrs Existing", "channel_ids": [other],
            })),
        )
        .await;
        assert_eq!(status, 200, "{base}: the custom category: {body}");
        sides.push((base, user, channel));
    }

    for (step, name) in [
        (1, "Mmrs Patched"),
        (2, "MMRS patched"),
        (3, "mmrs EXISTING"),
    ] {
        let mut observed = Vec::new();
        for (base, user, channel) in &sides {
            let mut socket = SocketProbe::connect(base, &user.token).await;
            patch(
                &http,
                base,
                &user.token,
                channel,
                serde_json::json!({"default_category_name": name}),
            )
            .await;
            let expected = [2, 0, 1][step - 1];
            socket
                .collect_until(Duration::from_secs(5), |frames| {
                    frames
                        .iter()
                        .filter(|f| {
                            f["event"]
                                .as_str()
                                .is_some_and(|n| n.starts_with("sidebar_category_"))
                        })
                        .count()
                        >= expected
                })
                .await;
            socket.collect_for(Duration::from_millis(600)).await;
            observed.push((
                sidebar(&http, base, user, &team, channel, &other).await,
                sidebar_events(&socket, channel, &other),
            ));
        }
        let rust = observed.pop().expect("two sides");
        let go = observed.pop().expect("two sides");
        assert_eq!(
            go.1.len(),
            [2, 0, 1][step - 1],
            "step {step}: Go's sidebar events: {:?}",
            go.1
        );
        assert_eq!(rust.1, go.1, "step {step}: the sidebar events differ");
        assert_eq!(rust.0, go.0, "step {step}: the sidebar read back differs");
    }

    let (_, user, _) = &sides[1];
    let final_sidebar = sidebar(&http, RUST, user, &team, &sides[1].2, &other).await;
    assert!(
        final_sidebar
            .iter()
            .any(|c| c[1] == "Mmrs Existing" && c[4] == serde_json::json!(["<ch>", "<other>"])),
        "the channel was prepended to the existing category: {final_sidebar:?}"
    );

    for (_, user, _) in &sides {
        common::delete_plain_user(&http, &admin, &user.id).await;
    }
}

/// The **create** path runs the same function, and the half an earlier port called dead is not:
/// the creator is already a member when it runs, so the new channel is an orphan in Channels and
/// Go writes Channels back without it — `sidebar_category_created`, then
/// `sidebar_category_updated`, both on the creator's socket.
#[tokio::test]
async fn a_default_category_name_on_create_files_the_channel_like_go() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let admin = go_minted_token(&http).await;
    let team = create_team(&http, &admin, "pwcreate").await;

    let mut observed = Vec::new();
    let mut users = Vec::new();
    for (base, side) in [(GO, "go"), (RUST, "rs")] {
        let user = create_plain_user(&http, &admin, &team, &format!("pwcreate{side}")).await;
        let mut socket = SocketProbe::connect(base, &user.token).await;
        let (status, served, body) = send(
            &http,
            base,
            reqwest::Method::POST,
            "/api/v4/channels",
            &user.token,
            Some(&serde_json::json!({
                "team_id": team, "name": format!("mmrs-parity-pwcreate{side}"),
                "display_name": "Mmrs Pw Create", "type": "O",
                "default_category_name": "Mmrs Created",
            })),
        )
        .await;
        assert_eq!(status, 201, "{base}: {body}");
        assert_eq!(served, base == RUST, "{base}: served where sent");
        let channel = serde_json::from_str::<serde_json::Value>(&body).expect("a channel")["id"]
            .as_str()
            .expect("an id")
            .to_owned();
        socket
            .collect_until(Duration::from_secs(5), |frames| {
                frames
                    .iter()
                    .any(|f| f["event"] == "sidebar_category_updated")
            })
            .await;
        socket.collect_for(Duration::from_millis(600)).await;
        observed.push((
            sidebar(&http, base, &user, &team, &channel, "").await,
            sidebar_events(&socket, &channel, ""),
        ));
        users.push(user);
    }
    let rust = observed.pop().expect("two sides");
    let go = observed.pop().expect("two sides");
    assert_eq!(
        go.1.len(),
        2,
        "Go: created, then Channels updated: {:?}",
        go.1
    );
    assert_eq!(rust.1, go.1, "the sidebar events differ");
    assert_eq!(rust.0, go.0, "the sidebar read back differs");

    for user in &users {
        common::delete_plain_user(&http, &admin, &user.id).await;
    }
}
