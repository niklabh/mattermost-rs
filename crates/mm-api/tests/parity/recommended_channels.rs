//! Cross-server parity for `GET /api/v4/teams/{team_id}/channels/recommended` —
//! `getRecommendedChannelsForTeam`.
//!
//! ```sh
//! scripts/parity.sh -p mm-api --test parity recommended_channels
//! ```
//!
//! # The whole reachable answer is three bytes
//!
//! `GetRecommendedPublicChannelsForUser` (app/channel.go:4620) returns `model.ChannelList{}`
//! before doing anything at all unless the licence is **Enterprise Advanced** *and*
//! `AccessControlSettings.EnableAttributeBasedAccessControl` is on. On this unlicensed server
//! neither holds, so the attribute-based scan below that gate is unreachable and the answer is
//! `[]` — for a member, for an admin, for a team with a hundred channels.
//!
//! That makes the *permission check* the only thing this route decides, which is exactly what
//! this suite tests: who gets `[]` and who gets a 403, and that the empty list is an array rather
//! than `null`.

use crate::common;

use common::{
    ACTIVE_LICENCE_ROW, RUST, assert_error_bodies_match_except_known_gaps, client, create_channel,
    create_plain_user, create_team, fetch_both_raw, fetch_both_stable, go_minted_token,
    purge_api_fixtures, set_active_licence_id, stack_enabled,
};

struct Fixture {
    /// A team with a public channel in it, so an empty answer is not "there is nothing to list".
    team: String,
    /// A second team the plain user is **not** in.
    other_team: String,
    plain_token: String,
}

static FIXTURE: tokio::sync::OnceCell<Fixture> = tokio::sync::OnceCell::const_new();

async fn fixture(client: &reqwest::Client, token: &str) -> &'static Fixture {
    FIXTURE
        .get_or_init(|| async {
            purge_api_fixtures().await;

            let team = create_team(client, token, "recommended").await;
            let other_team = create_team(client, token, "recommendedother").await;
            create_channel(client, token, &team, "recommended").await;
            create_channel(client, token, &team, "recommended2").await;
            let plain = create_plain_user(client, token, &team, "recommended").await;

            Fixture {
                team,
                other_team,
                plain_token: plain.token,
            }
        })
        .await
}

fn path(team_id: &str) -> String {
    format!("/api/v4/teams/{team_id}/channels/recommended")
}

/// `[]` and a newline, for the admin and for a plain member of the team — and the team has public
/// channels, so this is the licence gate answering and not an empty table.
#[tokio::test]
async fn the_answer_is_an_empty_array_for_everyone_who_may_ask() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let fixture = fixture(&client, &token).await;

    let p = path(&fixture.team);
    let (go, rs) = fetch_both_stable(&client, &token, &p).await;
    assert_eq!(
        go, b"[]\n",
        "{p}: `model.ChannelList{{}}` plus the encoder's newline"
    );
    assert_eq!(rs, go, "{p}");

    let ((go_status, go), (rs_status, rs)) =
        fetch_both_raw(&client, &fixture.plain_token, &p).await;
    assert_eq!(go_status, 200, "{p}: a plain member may list team channels");
    assert_eq!(rs_status, go_status, "{p}");
    assert_eq!(go, b"[]\n", "{p}");
    assert_eq!(rs, go, "{p}");

    // And the team really does have channels the browse modal would otherwise show.
    let listed = client
        .get(format!(
            "{}/api/v4/teams/{}/channels",
            common::GO,
            fixture.team
        ))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("Go answers")
        .json::<Vec<serde_json::Value>>()
        .await
        .expect("decodes");
    assert!(
        listed.len() >= 2,
        "the fixture team has public channels, so `[]` is the gate and not the table: {listed:?}"
    );
}

/// **`[]`, never `null`.** The gate returns a composite literal, not a nil slice — the same
/// distinction `getUserAudits` gets wrong in the other direction.
#[tokio::test]
async fn the_empty_answer_is_an_array_and_not_null() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let fixture = fixture(&client, &token).await;

    let p = path(&fixture.team);
    let (go, _) = fetch_both_stable(&client, &token, &p).await;
    let value: serde_json::Value = serde_json::from_slice(&go).expect("decodes");
    assert!(value.is_array(), "{p}: {value}");
    assert_eq!(value.as_array().map(Vec::len), Some(0), "{p}");
}

/// The permission is `list_team_channels` **on that team**, so a member of one team is refused on
/// another — even though the answer would have been `[]` either way. The check runs first, and
/// that order is the only thing this route's 403 can be about.
#[tokio::test]
async fn a_non_member_is_refused_on_a_team_it_is_not_in() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let fixture = fixture(&client, &token).await;

    let p = path(&fixture.other_team);
    let ((go_status, go), (rs_status, rs)) =
        fetch_both_raw(&client, &fixture.plain_token, &p).await;
    assert_eq!(
        go_status,
        403,
        "{p}: the plain user is not in the other team: {}",
        String::from_utf8_lossy(&go)
    );
    assert_eq!(rs_status, go_status, "{p}");
    let body = assert_error_bodies_match_except_known_gaps(&go, &rs, &p);
    assert_eq!(body["id"], "api.context.permissions.app_error", "{p}");

    // The admin, who has the permission everywhere, gets the empty list on the same team — so the
    // 403 above is about the caller and not about the team.
    let ((go_status, go), (rs_status, rs)) = fetch_both_raw(&client, &token, &p).await;
    assert_eq!(go_status, 200, "{p}: as the admin");
    assert_eq!(rs_status, go_status, "{p}");
    assert_eq!(go, b"[]\n", "{p}");
    assert_eq!(rs, go, "{p}");
}

/// A team id that names nothing is also a 403 — `SessionHasPermissionToTeam` cannot find a
/// membership and there is no lookup before it, so this route never says which teams exist.
#[tokio::test]
async fn an_unknown_team_is_a_403_for_a_plain_user() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let fixture = fixture(&client, &token).await;

    let p = path("mmrsnosuchteam000000000001");
    let ((go_status, go), (rs_status, rs)) =
        fetch_both_raw(&client, &fixture.plain_token, &p).await;
    assert_eq!(go_status, 403, "{p}");
    assert_eq!(rs_status, go_status, "{p}");
    assert_error_bodies_match_except_known_gaps(&go, &rs, &p);
}

/// A short id is `RequireTeamId`'s 400; a non-mux segment is forwarded so Go answers its own 404.
#[tokio::test]
async fn a_short_id_is_a_400_and_a_non_mux_segment_is_forwarded() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;

    let p = path("abc");
    let ((go_status, go), (rs_status, rs)) = fetch_both_raw(&client, &token, &p).await;
    assert_eq!(go_status, 400, "{p}");
    assert_eq!(rs_status, go_status, "{p}");
    let body = assert_error_bodies_match_except_known_gaps(&go, &rs, &p);
    assert_eq!(body["id"], "api.context.invalid_url_param.app_error", "{p}");

    let p = path("not-an-id");
    let ours = client
        .get(format!("{RUST}{p}"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("we answer");
    assert_eq!(
        ours.headers()
            .get("x-mmrs-served-by")
            .and_then(|v| v.to_str().ok()),
        Some("go"),
        "{p}"
    );
    assert_eq!(ours.status().as_u16(), 404, "{p}");
}

/// Registering the `GET` must not turn another method on the path into our 405.
#[tokio::test]
async fn other_methods_are_forwarded() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let fixture = fixture(&client, &token).await;

    let p = path(&fixture.team);
    let ours = client
        .post(format!("{RUST}{p}"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!({}))
        .send()
        .await
        .expect("we answer");
    assert_eq!(
        ours.headers()
            .get("x-mmrs-served-by")
            .and_then(|v| v.to_str().ok()),
        Some("go"),
        "POST {p} must be forwarded"
    );
}

/// **The boundary.** This route answers only what it can see is unlicensed; the moment
/// `Systems.ActiveLicenseId` holds a valid id, the request goes back to the proxy — because behind
/// the licence gate is an attribute-based scan this port does not implement.
///
/// Go is unmoved by the row (it loaded its licence at startup), so the *body* stays `[]` and the
/// observable difference is `x-mmrs-served-by`. Holds the shared lock exclusively: the same row
/// decides `license_client`'s answers.
#[tokio::test]
async fn an_active_license_id_hands_the_route_back_to_go() {
    if !stack_enabled() {
        return;
    }
    let _exclusive = ACTIVE_LICENCE_ROW.write().await;

    let client = client();
    let token = go_minted_token(&client).await;
    let fixture = fixture(&client, &token).await;
    let p = path(&fixture.team);

    let served_by = async || {
        client
            .get(format!("{RUST}{p}"))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .expect("we answer")
            .headers()
            .get("x-mmrs-served-by")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    };

    set_active_licence_id(None).await;
    assert_eq!(
        served_by().await.as_deref(),
        Some("rust"),
        "with no licence row we answer the route ourselves"
    );

    set_active_licence_id(Some("mmrslicence000000000000001")).await;
    let forwarded = served_by().await;

    set_active_licence_id(None).await;
    let cleared = served_by().await;

    assert_eq!(
        forwarded.as_deref(),
        Some("go"),
        "a licensed installation means an ABAC scan we do not implement, so Go answers"
    );
    assert_eq!(
        cleared.as_deref(),
        Some("rust"),
        "and removing the row hands it back — the state is the row, not a startup decision"
    );
}
