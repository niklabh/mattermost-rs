//! Cross-server parity for `POST /api/v4/channels/{channel_id}/move`.
//!
//! ```sh
//! scripts/parity.sh --test parity channel_move
//! ```
//!
//! A move is a one-way write, so every success case builds its own channel on each server and
//! the two are compared by shape, by the fields the move sets, and by what the database says
//! afterwards: the sidebar rows gone, the threads and webhooks re-homed, the members not on the
//! new team removed, and the `system_move_channel` post written by the mover.

use crate::common;

use common::{
    GO, RUST, add_user_to_channel, assert_error_bodies_match_except_known_gaps, client,
    create_channel_typed, create_plain_user, create_team, fixture_pool, go_minted_token,
    post_message, stack_enabled,
};

const MOVE_MESSAGE_PREFIX: &str = "This channel has been moved to this team from ";

async fn post(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    channel_id: &str,
    body: &str,
) -> (u16, bool, Vec<u8>) {
    let response = client
        .post(format!("{base}/api/v4/channels/{channel_id}/move"))
        .header("Authorization", format!("Bearer {token}"))
        .header("Content-Type", "application/json")
        .body(body.to_owned())
        .send()
        .await
        .unwrap_or_else(|e| panic!("{base} is unreachable: {e}"));
    let status = response.status().as_u16();
    let served = response
        .headers()
        .get("x-mmrs-served-by")
        .and_then(|v| v.to_str().ok())
        == Some("rust");
    (
        status,
        served,
        response
            .bytes()
            .await
            .map(|b| b.to_vec())
            .unwrap_or_default(),
    )
}

fn parsed(body: &[u8]) -> serde_json::Value {
    serde_json::from_slice(body).unwrap_or_default()
}

/// The same refusal from both servers on the same channel — nothing is written on a refusal, so
/// one channel serves both.
async fn both_refuse(
    client: &reqwest::Client,
    token: &str,
    channel_id: &str,
    body: &str,
    status: u16,
    id: &str,
) -> serde_json::Value {
    let (go_status, _, go) = post(client, GO, token, channel_id, body).await;
    let (rs_status, served, rs) = post(client, RUST, token, channel_id, body).await;
    assert_eq!(
        go_status,
        status,
        "Go {body}: {}",
        String::from_utf8_lossy(&go)
    );
    assert_eq!(
        rs_status,
        status,
        "Rust {body}: {}",
        String::from_utf8_lossy(&rs)
    );
    assert!(served, "{body}: served here");
    assert_eq!(parsed(&go)["id"], id, "{body}");
    assert_error_bodies_match_except_known_gaps(&go, &rs, "/api/v4/channels/{channel_id}/move");
    // Go's body: the parameter a 400 names lives only in Go's translated message, ours being
    // the untranslated id ([D-092]).
    parsed(&go)
}

async fn add_user_to_team(client: &reqwest::Client, admin: &str, team_id: &str, user_id: &str) {
    let response = client
        .post(format!("{GO}/api/v4/teams/{team_id}/members"))
        .header("Authorization", format!("Bearer {admin}"))
        .json(&serde_json::json!({ "team_id": team_id, "user_id": user_id }))
        .send()
        .await
        .expect("Go answers");
    assert!(response.status().is_success(), "adding to the team");
}

async fn reply(client: &reqwest::Client, token: &str, channel_id: &str, root_id: &str) -> String {
    let response = client
        .post(format!("{GO}/api/v4/posts"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!({ "channel_id": channel_id, "root_id": root_id, "message": "a reply" }))
        .send()
        .await
        .expect("Go answers");
    assert!(response.status().is_success(), "the reply is written");
    response.json::<serde_json::Value>().await.expect("JSON")["id"]
        .as_str()
        .expect("an id")
        .to_owned()
}

/// `GET /users/{id}/teams/{team}/channels/categories` makes Go create the user's default
/// categories; the channel is then filed into their `channels` category by hand, which is the
/// row the webapp would have written on first view and the one a move has to delete.
async fn plant_sidebar_row(
    client: &reqwest::Client,
    pool: &sqlx::PgPool,
    token: &str,
    user_id: &str,
    team_id: &str,
    channel_id: &str,
) {
    let response = client
        .get(format!(
            "{GO}/api/v4/users/{user_id}/teams/{team_id}/channels/categories"
        ))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("Go answers");
    assert!(response.status().is_success(), "the categories exist");
    let category_id: String = sqlx::query_scalar(
        "SELECT id FROM sidebarcategories WHERE userid = $1 AND teamid = $2 AND type = 'channels'",
    )
    .bind(user_id)
    .bind(team_id)
    .fetch_one(pool)
    .await
    .expect("the channels category");
    sqlx::query(
        "INSERT INTO sidebarchannels (channelid, userid, categoryid, sortorder) VALUES ($1, $2, $3, 0)",
    )
    .bind(channel_id)
    .bind(user_id)
    .bind(&category_id)
    .execute(pool)
    .await
    .expect("the sidebar row is planted");
}

async fn create_incoming_hook(client: &reqwest::Client, admin: &str, channel_id: &str) -> String {
    let response = client
        .post(format!("{GO}/api/v4/hooks/incoming"))
        .header("Authorization", format!("Bearer {admin}"))
        .json(&serde_json::json!({ "channel_id": channel_id, "display_name": "mmrs move" }))
        .send()
        .await
        .expect("Go answers");
    assert!(response.status().is_success(), "the incoming hook exists");
    response.json::<serde_json::Value>().await.expect("JSON")["id"]
        .as_str()
        .expect("an id")
        .to_owned()
}

async fn create_outgoing_hook(
    client: &reqwest::Client,
    admin: &str,
    team_id: &str,
    channel_id: &str,
) {
    let response = client
        .post(format!("{GO}/api/v4/hooks/outgoing"))
        .header("Authorization", format!("Bearer {admin}"))
        .json(&serde_json::json!({
            "team_id": team_id,
            "channel_id": channel_id,
            "display_name": "mmrs move out",
            "trigger_words": ["mmrsmove"],
            "callback_urls": ["http://127.0.0.1:1/mmrs"],
        }))
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "the outgoing hook exists: {}",
        response.text().await.unwrap_or_default()
    );
}

async fn scalar_i64(pool: &sqlx::PgPool, sql: &str, arg: &str) -> i64 {
    sqlx::query_scalar(sql)
        .bind(arg)
        .fetch_one(pool)
        .await
        .expect("the query answers")
}

async fn scalar_string(pool: &sqlx::PgPool, sql: &str, arg: &str) -> String {
    sqlx::query_scalar(sql)
        .bind(arg)
        .fetch_one(pool)
        .await
        .expect("the query answers")
}

/// Two teams, a channel on the first with a member who is on both, a thread, a sidebar row and
/// both kinds of webhook — the whole set a move has to carry across.
struct Moved {
    channel_id: String,
    root_id: String,
    hook_id: String,
    member: common::PlainUser,
}

async fn build(
    client: &reqwest::Client,
    pool: &sqlx::PgPool,
    admin: &str,
    team_a: &str,
    team_b: &str,
    tag: &str,
) -> Moved {
    let channel_id = create_channel_typed(client, admin, team_a, tag, "O").await;
    let member = create_plain_user(client, admin, team_a, tag).await;
    add_user_to_team(client, admin, team_b, &member.id).await;
    add_user_to_channel(client, admin, &channel_id, &member.id).await;
    plant_sidebar_row(client, pool, &member.token, &member.id, team_a, &channel_id).await;
    let root_id = post_message(client, &member.token, &channel_id, "a root", None).await;
    reply(client, &member.token, &channel_id, &root_id).await;
    let hook_id = create_incoming_hook(client, admin, &channel_id).await;
    create_outgoing_hook(client, admin, team_a, &channel_id).await;
    Moved {
        channel_id,
        root_id,
        hook_id,
        member,
    }
}

/// A move on each server: the same body shape, the new team on both, and the five writes each
/// server made to its own channel.
#[tokio::test]
async fn a_move_rehomes_the_channel_its_threads_its_hooks_and_writes_the_post() {
    if !stack_enabled() {
        return;
    }
    let Some(pool) = fixture_pool().await else {
        return;
    };
    let client = client();
    let admin = go_minted_token(&client).await;
    let team_a = create_team(&client, &admin, "mva").await;
    let team_b = create_team(&client, &admin, "mvb").await;
    let team_a_name: String =
        scalar_string(&pool, "SELECT name FROM teams WHERE id = $1", &team_a).await;
    let admin_username = client
        .get(format!("{GO}/api/v4/users/me"))
        .header("Authorization", format!("Bearer {admin}"))
        .send()
        .await
        .expect("Go answers")
        .json::<serde_json::Value>()
        .await
        .expect("JSON")["username"]
        .as_str()
        .expect("a username")
        .to_owned();
    let body = format!(r#"{{"team_id":"{team_b}","force":false}}"#);

    let mut bodies = Vec::new();
    for (base, tag) in [(GO, "mvgo"), (RUST, "mvrs")] {
        let built = build(&client, &pool, &admin, &team_a, &team_b, tag).await;
        assert!(
            scalar_i64(
                &pool,
                "SELECT COUNT(*) FROM sidebarchannels WHERE channelid = $1",
                &built.channel_id
            )
            .await
                >= 1,
            "{base}: a sidebar row exists before the move"
        );

        let (status, served, response) =
            post(&client, base, &admin, &built.channel_id, &body).await;
        assert_eq!(
            status,
            200,
            "{base}: {}",
            String::from_utf8_lossy(&response)
        );
        assert_eq!(served, base == RUST);
        assert_eq!(
            response.last().copied(),
            Some(b'\n'),
            "{base}: json.NewEncoder"
        );
        let channel = parsed(&response);
        assert_eq!(channel["team_id"], team_b, "{base}: the new team");
        assert_eq!(channel["id"], built.channel_id);
        assert!(
            channel["update_at"].as_i64().unwrap_or(0) > channel["create_at"].as_i64().unwrap_or(0),
            "{base}: the store stamped update_at"
        );

        assert_eq!(
            scalar_string(
                &pool,
                "SELECT teamid FROM channels WHERE id = $1",
                &built.channel_id
            )
            .await,
            team_b,
            "{base}: the row moved"
        );
        assert_eq!(
            scalar_i64(
                &pool,
                "SELECT COUNT(*) FROM sidebarchannels WHERE channelid = $1",
                &built.channel_id
            )
            .await,
            0,
            "{base}: every sidebar row for the channel is gone"
        );
        assert_eq!(
            scalar_string(
                &pool,
                "SELECT threadteamid FROM threads WHERE postid = $1",
                &built.root_id
            )
            .await,
            team_b,
            "{base}: the thread follows"
        );
        assert_eq!(
            scalar_string(
                &pool,
                "SELECT teamid FROM incomingwebhooks WHERE id = $1",
                &built.hook_id
            )
            .await,
            team_b,
            "{base}: the incoming hook follows"
        );
        assert_eq!(
            scalar_string(
                &pool,
                "SELECT teamid FROM outgoingwebhooks WHERE channelid = $1 AND deleteat = 0",
                &built.channel_id
            )
            .await,
            team_b,
            "{base}: the outgoing hook follows"
        );
        let (message, props): (String, serde_json::Value) = sqlx::query_as(
            "SELECT message, props FROM posts WHERE channelid = $1 AND type = 'system_move_channel'",
        )
        .bind(&built.channel_id)
        .fetch_one(&pool)
        .await
        .expect("one move post");
        assert_eq!(
            message,
            format!("{MOVE_MESSAGE_PREFIX}{team_a_name}."),
            "{base}"
        );
        assert_eq!(
            props["username"], admin_username,
            "{base}: the mover's username prop"
        );
        // The member on both teams is still in.
        assert_eq!(
            scalar_i64(
                &pool,
                "SELECT COUNT(*) FROM channelmembers WHERE channelid = $1",
                &built.channel_id
            )
            .await,
            2,
            "{base}: the admin and the member remain"
        );
        let _ = &built.member;
        bodies.push(channel);
    }

    let [go, rs] = [&bodies[0], &bodies[1]];
    assert_eq!(
        go.as_object()
            .expect("an object")
            .keys()
            .collect::<Vec<_>>(),
        rs.as_object()
            .expect("an object")
            .keys()
            .collect::<Vec<_>>(),
        "the moved channel carries the same keys.\n  go:   {go}\n  rust: {rs}"
    );
    for key in [
        "type",
        "team_id",
        "scheme_id",
        "props",
        "group_constrained",
        "shared",
    ] {
        assert_eq!(go[key], rs[key], "{key}");
    }
}

/// A member who is not on the new team: without `force` the 500; with it, swept off the channel
/// through the inner removal — no "removed" post — and the move succeeds. A deactivated member
/// is swept either way.
#[tokio::test]
async fn members_not_on_the_new_team_block_the_move_unless_forced() {
    if !stack_enabled() {
        return;
    }
    let Some(pool) = fixture_pool().await else {
        return;
    };
    let client = client();
    let admin = go_minted_token(&client).await;
    let team_a = create_team(&client, &admin, "mvfa").await;
    let team_b = create_team(&client, &admin, "mvfb").await;

    for (base, tag) in [(GO, "mvfgo"), (RUST, "mvfrs")] {
        let channel_id = create_channel_typed(&client, &admin, &team_a, tag, "P").await;
        let only_a = create_plain_user(&client, &admin, &team_a, tag).await;
        add_user_to_channel(&client, &admin, &channel_id, &only_a.id).await;
        let gone = create_plain_user(&client, &admin, &team_a, &format!("{tag}d")).await;
        add_user_to_channel(&client, &admin, &channel_id, &gone.id).await;
        let response = client
            .delete(format!("{GO}/api/v4/users/{}", gone.id))
            .header("Authorization", format!("Bearer {admin}"))
            .send()
            .await
            .expect("Go answers");
        assert!(response.status().is_success(), "deactivating");

        let members = |channel_id: String| {
            let pool = pool.clone();
            async move {
                scalar_i64(
                    &pool,
                    "SELECT COUNT(*) FROM channelmembers WHERE channelid = $1",
                    &channel_id,
                )
                .await
            }
        };
        assert_eq!(members(channel_id.clone()).await, 3);

        let (status, served, response) = post(
            &client,
            base,
            &admin,
            &channel_id,
            &format!(r#"{{"team_id":"{team_b}","force":false}}"#),
        )
        .await;
        assert_eq!(
            status,
            500,
            "{base}: {}",
            String::from_utf8_lossy(&response)
        );
        assert_eq!(served, base == RUST);
        assert_eq!(
            parsed(&response)["id"],
            "app.channel.move_channel.members_do_not_match.error"
        );
        // The deactivated sweep ran before the refusal; the live member stayed.
        assert_eq!(members(channel_id.clone()).await, 2, "{base}");
        assert_eq!(
            scalar_string(
                &pool,
                "SELECT teamid FROM channels WHERE id = $1",
                &channel_id
            )
            .await,
            team_a,
            "{base}: not moved"
        );

        let (status, _, response) = post(
            &client,
            base,
            &admin,
            &channel_id,
            &format!(r#"{{"team_id":"{team_b}","force":true}}"#),
        )
        .await;
        assert_eq!(
            status,
            200,
            "{base}: {}",
            String::from_utf8_lossy(&response)
        );
        assert_eq!(
            members(channel_id.clone()).await,
            1,
            "{base}: only the admin is left"
        );
        assert_eq!(
            scalar_i64(
                &pool,
                "SELECT COUNT(*) FROM posts WHERE channelid = $1 AND type LIKE 'system_remove%'",
                &channel_id
            )
            .await,
            0,
            "{base}: the sweep writes no removal post"
        );
        assert_eq!(
            scalar_i64(
                &pool,
                "SELECT COUNT(*) FROM posts WHERE channelid = $1 AND type = 'system_move_channel'",
                &channel_id
            )
            .await,
            1,
            "{base}: one move post"
        );
    }

    // The two refusals agree on the wire.
    let channel_go = create_channel_typed(&client, &admin, &team_a, "mvfx", "O").await;
    let stranded = create_plain_user(&client, &admin, &team_a, "mvfx").await;
    add_user_to_channel(&client, &admin, &channel_go, &stranded.id).await;
    let body = format!(r#"{{"team_id":"{team_b}","force":false}}"#);
    let (_, _, go) = post(&client, GO, &admin, &channel_go, &body).await;
    let (_, _, rs) = post(&client, RUST, &admin, &channel_go, &body).await;
    assert_error_bodies_match_except_known_gaps(&go, &rs, "/api/v4/channels/{channel_id}/move");
}

/// The checks in Go's order: the channel, the two body faults, the team, the DM/GM refusal, and
/// only then the permission — so a plain member's DM is refused for its type, not their rights.
#[tokio::test]
async fn the_refusals_come_in_gos_order() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let admin = go_minted_token(&client).await;
    let team_a = create_team(&client, &admin, "mvra").await;
    let team_b = create_team(&client, &admin, "mvrb").await;
    let channel_id = create_channel_typed(&client, &admin, &team_a, "mvr", "O").await;
    let a = create_plain_user(&client, &admin, &team_a, "mvra").await;
    let b = create_plain_user(&client, &admin, &team_a, "mvrb").await;
    add_user_to_channel(&client, &admin, &channel_id, &a.id).await;
    let dm = client
        .post(format!("{GO}/api/v4/channels/direct"))
        .header("Authorization", format!("Bearer {}", a.token))
        .json(&serde_json::json!([a.id, b.id]))
        .send()
        .await
        .expect("Go answers")
        .json::<serde_json::Value>()
        .await
        .expect("JSON")["id"]
        .as_str()
        .expect("an id")
        .to_owned();
    let ok = format!(r#"{{"team_id":"{team_b}","force":false}}"#);
    let nobody = mm_model::utils::new_id();

    both_refuse(
        &client,
        &a.token,
        "abc",
        &ok,
        400,
        "api.context.invalid_url_param.app_error",
    )
    .await;
    both_refuse(
        &client,
        &a.token,
        &nobody,
        &ok,
        404,
        "app.channel.get.existing.app_error",
    )
    .await;
    for (body, param) in [
        ("{}", "team_id"),
        ("[]", "team_id"),
        (r#"{"team_id":5,"force":true}"#, "team_id"),
        (&format!(r#"{{"team_id":"{team_b}"}}"#), "force"),
        (
            &format!(r#"{{"team_id":"{team_b}","force":"true"}}"#),
            "force",
        ),
    ] {
        let rs = both_refuse(
            &client,
            &a.token,
            &channel_id,
            body,
            400,
            "api.context.invalid_body_param.app_error",
        )
        .await;
        assert!(
            rs["message"].as_str().is_some_and(|m| m.contains(param)),
            "{body}: names {param}: {rs}"
        );
    }
    both_refuse(
        &client,
        &a.token,
        &channel_id,
        &format!(r#"{{"team_id":"{nobody}","force":false}}"#),
        404,
        "app.team.get.find.app_error",
    )
    .await;
    both_refuse(
        &client,
        &a.token,
        &dm,
        &ok,
        403,
        "api.channel.move_channel.type.invalid",
    )
    .await;
    both_refuse(
        &client,
        &admin,
        &dm,
        &ok,
        403,
        "api.channel.move_channel.type.invalid",
    )
    .await;
    both_refuse(
        &client,
        &a.token,
        &channel_id,
        &ok,
        403,
        "api.context.permissions.app_error",
    )
    .await;
}
