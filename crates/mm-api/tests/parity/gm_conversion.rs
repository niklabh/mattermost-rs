//! Cross-server parity for `POST /api/v4/channels/{channel_id}/convert_to_channel`.
//!
//! ```sh
//! scripts/parity.sh --test parity gm_conversion
//! ```
//!
//! A conversion is a one-way write, so each server converts its own group message and the two
//! are compared by shape and by what the database says afterwards: the type, team and name; the
//! converter's `SchemeAdmin`; the sidebar row in each member's `channels` category; and the
//! `system_gm_to_channel` post with its two props.

use crate::common;

use common::{
    GO, RUST, assert_error_bodies_match_except_known_gaps, client, create_channel_typed,
    create_plain_user, create_team, fixture_pool, go_minted_token, plain_username, stack_enabled,
};

async fn post(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    channel_id: &str,
    body: &str,
) -> (u16, bool, Vec<u8>) {
    let response = client
        .post(format!(
            "{base}/api/v4/channels/{channel_id}/convert_to_channel"
        ))
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

/// The same refusal from both servers — nothing is written on a refusal.
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
    assert_error_bodies_match_except_known_gaps(
        &go,
        &rs,
        "/api/v4/channels/{channel_id}/convert_to_channel",
    );
    parsed(&go)
}

async fn create_group_message(client: &reqwest::Client, token: &str, user_ids: &[&str]) -> String {
    let response = client
        .post(format!("{GO}/api/v4/channels/group"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&user_ids)
        .send()
        .await
        .expect("Go answers");
    assert!(response.status().is_success(), "the group message exists");
    response.json::<serde_json::Value>().await.expect("JSON")["id"]
        .as_str()
        .expect("an id")
        .to_owned()
}

async fn create_direct_message(client: &reqwest::Client, token: &str, a: &str, b: &str) -> String {
    let response = client
        .post(format!("{GO}/api/v4/channels/direct"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&[a, b])
        .send()
        .await
        .expect("Go answers");
    assert!(response.status().is_success(), "the direct message exists");
    response.json::<serde_json::Value>().await.expect("JSON")["id"]
        .as_str()
        .expect("an id")
        .to_owned()
}

/// `GET /users/{id}/teams/{team}/channels/categories` makes Go create the member's default
/// categories on the team, which is what the conversion re-files the channel into.
async fn materialise_categories(
    client: &reqwest::Client,
    token: &str,
    user_id: &str,
    team_id: &str,
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
}

fn body(channel_id: &str, team_id: &str, name: &str) -> String {
    serde_json::json!({
        "channel_id": channel_id,
        "team_id": team_id,
        "name": name,
        "display_name": format!("Converted {name}"),
    })
    .to_string()
}

/// A conversion on each server: the private channel on the team, the converter as admin, the
/// sidebar rows, and the post with its props.
#[tokio::test]
async fn a_conversion_makes_a_private_channel_with_the_converter_as_admin() {
    if !stack_enabled() {
        return;
    }
    let Some(pool) = fixture_pool().await else {
        return;
    };
    let client = client();
    let admin = go_minted_token(&client).await;
    let team = create_team(&client, &admin, "gmc").await;

    let mut bodies = Vec::new();
    let mut filings = Vec::new();
    for (base, tag) in [(GO, "gmcgo"), (RUST, "gmcrs")] {
        let a = create_plain_user(&client, &admin, &team, &format!("{tag}a")).await;
        let b = create_plain_user(&client, &admin, &team, &format!("{tag}b")).await;
        let c = create_plain_user(&client, &admin, &team, &format!("{tag}c")).await;
        let gm = create_group_message(&client, &a.token, &[&a.id, &b.id, &c.id]).await;
        materialise_categories(&client, &b.token, &b.id, &team).await;
        let name = format!("mmrs-parity-{tag}");

        let (status, served, response) =
            post(&client, base, &a.token, &gm, &body(&gm, &team, &name)).await;
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
        assert_eq!(channel["id"], gm);
        assert_eq!(channel["type"], "P", "{base}");
        assert_eq!(channel["team_id"], team, "{base}");
        assert_eq!(channel["name"], name, "{base}");
        assert_eq!(
            channel["display_name"],
            format!("Converted {name}"),
            "{base}"
        );

        let (kind, team_id, scheme_admin): (String, String, bool) = sqlx::query_as(
            "SELECT c.type::text, c.teamid, cm.schemeadmin FROM channels c JOIN channelmembers cm ON cm.channelid = c.id WHERE c.id = $1 AND cm.userid = $2",
        )
        .bind(&gm)
        .bind(&a.id)
        .fetch_one(&pool)
        .await
        .expect("the row");
        assert_eq!(
            (kind.as_str(), team_id.as_str(), scheme_admin),
            ("P", team.as_str(), true),
            "{base}"
        );

        // The member with categories has the channel in their `channels` category; the ones
        // without have nothing, which is normal.
        let filed: Vec<(String, String)> = sqlx::query_as(
            "SELECT sc.userid, cat.type FROM sidebarchannels sc JOIN sidebarcategories cat ON cat.id = sc.categoryid WHERE sc.channelid = $1",
        )
        .bind(&gm)
        .fetch_all(&pool)
        .await
        .expect("the sidebar rows");
        // Measured: Go files nothing here — the member's categories exist, and the re-save of
        // the `channels` category still leaves the channel out of every sidebar. Kept as a
        // cross-server comparison rather than an expectation, since the two must agree.
        filings.push(filed);

        let (message, props): (String, serde_json::Value) = sqlx::query_as(
            "SELECT message, props FROM posts WHERE channelid = $1 AND type = 'system_gm_to_channel'",
        )
        .bind(&gm)
        .fetch_one(&pool)
        .await
        .expect("one conversion post");
        assert_eq!(
            message,
            format!(
                "{} created this channel from a group message with {}, {} and {}.",
                plain_username(&format!("{tag}a")),
                plain_username(&format!("{tag}a")),
                plain_username(&format!("{tag}b")),
                plain_username(&format!("{tag}c"))
            ),
            "{base}"
        );
        assert_eq!(props["convertedByUserId"], a.id, "{base}");
        assert_eq!(
            props["gmMembersDuringConversionIDs"],
            serde_json::json!([a.id, b.id, c.id]),
            "{base}: the member ids, in username order"
        );
        bodies.push(channel);
    }

    let [go, rs] = [&bodies[0], &bodies[1]];
    assert_eq!(
        filings[0].iter().map(|(_, kind)| kind).collect::<Vec<_>>(),
        filings[1].iter().map(|(_, kind)| kind).collect::<Vec<_>>(),
        "the sidebar filings agree in shape"
    );
    assert_eq!(
        go.as_object()
            .expect("an object")
            .keys()
            .collect::<Vec<_>>(),
        rs.as_object()
            .expect("an object")
            .keys()
            .collect::<Vec<_>>(),
        "the converted channel carries the same keys.\n  go:   {go}\n  rust: {rs}"
    );
    for key in [
        "type",
        "team_id",
        "header",
        "purpose",
        "scheme_id",
        "props",
        "group_constrained",
    ] {
        assert_eq!(go[key], rs[key], "{key}");
    }
}

/// The refusals in order: the body, the permission, the id mismatch, the team, the channel's
/// type, the converter's membership, and the private-channel shape.
#[tokio::test]
async fn the_refusals_come_in_gos_order() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let admin = go_minted_token(&client).await;
    let team = create_team(&client, &admin, "gmr").await;
    let elsewhere = create_team(&client, &admin, "gmre").await;
    let a = create_plain_user(&client, &admin, &team, "gmra").await;
    let b = create_plain_user(&client, &admin, &team, "gmrb").await;
    let c = create_plain_user(&client, &admin, &team, "gmrc").await;
    let d = create_plain_user(&client, &admin, &team, "gmrd").await;
    let gm = create_group_message(&client, &a.token, &[&a.id, &b.id, &c.id]).await;
    let dm = create_direct_message(&client, &a.token, &a.id, &b.id).await;
    let public = create_channel_typed(&client, &admin, &team, "gmr", "O").await;
    let nobody = mm_model::utils::new_id();

    both_refuse(
        &client,
        &a.token,
        "abc",
        &body(&gm, &team, "x"),
        400,
        "api.context.invalid_url_param.app_error",
    )
    .await;
    for bad in ["null", "", "[]", r#"{"channel_id":5}"#] {
        both_refuse(
            &client,
            &a.token,
            &gm,
            bad,
            400,
            "api.context.invalid_body_param.app_error",
        )
        .await;
    }
    // The permission is on the body's team, before the ids are compared: a team the caller is
    // not on refuses, whatever the channel id says.
    both_refuse(
        &client,
        &a.token,
        &gm,
        &body(&nobody, &elsewhere, "x"),
        403,
        "api.context.permissions.app_error",
    )
    .await;
    both_refuse(
        &client,
        &a.token,
        &gm,
        &body(&nobody, &team, "x"),
        400,
        "api.context.invalid_body_param.app_error",
    )
    .await;
    // The admin is on `elsewhere`, so the permission passes and the common-teams check refuses.
    both_refuse(
        &client,
        &admin,
        &gm,
        &body(&gm, &elsewhere, "x"),
        400,
        "app.channel.group_message_conversion.incorrect_team",
    )
    .await;
    // Not a DM or GM: the common-teams read's own 400, before the not-a-GM 404.
    both_refuse(
        &client,
        &a.token,
        &public,
        &body(&public, &team, "x"),
        400,
        "app.channel.get_common_teams.incorrect_channel_type",
    )
    .await;
    // A DM is the one non-GM that reaches the 404.
    both_refuse(
        &client,
        &a.token,
        &dm,
        &body(&dm, &team, "x"),
        404,
        "app.channel.group_message_conversion.original_channel_not_gm",
    )
    .await;
    // A converter who is not a member: the member read's 404.
    both_refuse(
        &client,
        &d.token,
        &gm,
        &body(&gm, &team, "x"),
        404,
        "app.channel.get_member.missing.app_error",
    )
    .await;
    // A name the private channel cannot carry.
    both_refuse(
        &client,
        &a.token,
        &gm,
        &body(&gm, &team, "Not A Name"),
        400,
        "model.channel.is_valid.1_or_more.app_error",
    )
    .await;
    both_refuse(
        &client,
        &a.token,
        &gm,
        &body(&gm, &team, ""),
        400,
        "model.channel.is_valid.1_or_more.app_error",
    )
    .await;
    let _ = (&b, &c);
}
