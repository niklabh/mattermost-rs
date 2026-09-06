//! Cross-server parity for the two licence-gated channel reads —
//! `GET /api/v4/channels/{channel_id}/moderations` (`getChannelModerations`) and
//! `GET /api/v4/channels/{channel_id}/bookmarks` (`listChannelBookmarksForChannel`).
//!
//! ```sh
//! scripts/parity.sh -p mm-api --test parity licence_gated_channels
//! ```
//!
//! # Both routes are one statement long on an unlicensed server
//!
//! Each opens with `if c.App.Channels().License() == nil` and returns before validating the
//! channel id or asking any permission (channel.go:2973, channel_bookmark.go:447). So the whole
//! reachable behaviour here is: which error, at which status, for **every** caller and every
//! id-shaped segment. The two differ — a **403** for moderations and a **501** for bookmarks —
//! and a client branching on the status sees them differently.
//!
//! A licensed installation is forwarded, and [`a_license_row_hands_both_routes_back_to_go`] pins
//! that boundary the way `license_client` and `recommended_channels` pin theirs.

use crate::common;

use common::{
    ACTIVE_LICENCE_ROW, RUST, assert_error_bodies_match_except_known_gaps, client, create_channel,
    create_plain_user, create_team, fetch_both_raw, go_minted_token, set_active_licence_id,
    stack_enabled,
};

struct Fixture {
    channel: String,
    plain_token: String,
}

static FIXTURE: tokio::sync::OnceCell<Fixture> = tokio::sync::OnceCell::const_new();

async fn fixture(client: &reqwest::Client, token: &str) -> &'static Fixture {
    FIXTURE
        .get_or_init(|| async {
            let team = create_team(client, token, "licencegated").await;
            let channel = create_channel(client, token, &team, "licencegated").await;
            let plain = create_plain_user(client, token, &team, "licencegated").await;
            Fixture {
                channel,
                plain_token: plain.token,
            }
        })
        .await
}

fn moderations(channel_id: &str) -> String {
    format!("/api/v4/channels/{channel_id}/moderations")
}
fn bookmarks(channel_id: &str) -> String {
    format!("/api/v4/channels/{channel_id}/bookmarks")
}

/// Each route's own error, at its own status, byte-compatible with Go.
#[tokio::test]
async fn each_route_answers_its_own_licence_error() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let fixture = fixture(&client, &token).await;

    for (p, status, id) in [
        (
            moderations(&fixture.channel),
            403,
            "api.channel.get_channel_moderations.license.error",
        ),
        (
            bookmarks(&fixture.channel),
            501,
            "api.channel.bookmark.channel_bookmark.license.error",
        ),
    ] {
        let ((go_status, go), (rs_status, rs)) = fetch_both_raw(&client, &token, &p).await;
        assert_eq!(go_status, status, "{p}");
        assert_eq!(rs_status, go_status, "{p}");
        let body = assert_error_bodies_match_except_known_gaps(&go, &rs, &p);
        assert_eq!(body["id"], id, "{p}");
        assert!(!rs.ends_with(b"\n"), "{p}: error bodies carry no newline");
    }
}

/// **The licence check runs before the id is validated.** `abc` is far too short to be a channel
/// id, and both routes still answer the licence error rather than a 400 — because the gate is the
/// first statement in each handler.
#[tokio::test]
async fn an_invalid_channel_id_still_gets_the_licence_error() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;

    for (p, status) in [(moderations("abc"), 403), (bookmarks("abc"), 501)] {
        let ((go_status, go), (rs_status, rs)) = fetch_both_raw(&client, &token, &p).await;
        assert_eq!(
            go_status,
            status,
            "{p}: the licence gate precedes `RequireChannelId`: {}",
            String::from_utf8_lossy(&go)
        );
        assert_eq!(rs_status, go_status, "{p}");
        let body = assert_error_bodies_match_except_known_gaps(&go, &rs, &p);
        assert_ne!(
            body["id"], "api.context.invalid_url_param.app_error",
            "{p}: and it is emphatically not the id error"
        );
    }
}

/// **And before any permission question.** A plain user who is not even in the channel gets the
/// same licence error, not a 403 about permissions — for moderations, where the permission gate
/// would otherwise refuse them, that is two different 403s and the ordering decides which.
#[tokio::test]
async fn a_plain_user_gets_the_licence_error_and_not_a_permission_one() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let fixture = fixture(&client, &token).await;

    let p = moderations(&fixture.channel);
    let ((go_status, go), (rs_status, rs)) =
        fetch_both_raw(&client, &fixture.plain_token, &p).await;
    assert_eq!(go_status, 403, "{p}");
    assert_eq!(rs_status, go_status, "{p}");
    let body = assert_error_bodies_match_except_known_gaps(&go, &rs, &p);
    assert_eq!(
        body["id"], "api.channel.get_channel_moderations.license.error",
        "{p}: the licence error, not `api.context.permissions.app_error`"
    );

    let p = bookmarks(&fixture.channel);
    let ((go_status, go), (rs_status, rs)) =
        fetch_both_raw(&client, &fixture.plain_token, &p).await;
    assert_eq!(go_status, 501, "{p}");
    assert_eq!(rs_status, go_status, "{p}");
    assert_error_bodies_match_except_known_gaps(&go, &rs, &p);
}

/// **The boundary.** A valid `Systems.ActiveLicenseId` sends both routes back to the proxy, since
/// everything behind the gate is unported. Holds the shared lock exclusively — the same row
/// decides `license_client` and `recommended_channels`.
#[tokio::test]
async fn a_license_row_hands_both_routes_back_to_go() {
    if !stack_enabled() {
        return;
    }
    let _exclusive = ACTIVE_LICENCE_ROW.write().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let fixture = fixture(&client, &token).await;

    let served_by = async |p: &str| {
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
    for p in [moderations(&fixture.channel), bookmarks(&fixture.channel)] {
        assert_eq!(
            served_by(&p).await.as_deref(),
            Some("rust"),
            "{p}: unlicensed, so ours to answer"
        );
    }

    set_active_licence_id(Some("mmrslicence000000000000001")).await;
    let forwarded: Vec<Option<String>> = vec![
        served_by(&moderations(&fixture.channel)).await,
        served_by(&bookmarks(&fixture.channel)).await,
    ];

    set_active_licence_id(None).await;

    for (p, served) in [
        (moderations(&fixture.channel), &forwarded[0]),
        (bookmarks(&fixture.channel), &forwarded[1]),
    ] {
        assert_eq!(
            served.as_deref(),
            Some("go"),
            "{p}: a licence means work we have not ported"
        );
    }
}

/// A segment outside gorilla's charset never routes in Go, so it is forwarded and Go answers its
/// own 404 — *before* the licence gate, because the gate is inside a handler that never runs.
#[tokio::test]
async fn a_non_mux_segment_is_forwarded_and_not_licence_gated() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;

    for p in [moderations("not-an-id"), bookmarks("not-an-id")] {
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
        assert_eq!(
            ours.status().as_u16(),
            404,
            "{p}: Go's router refuses it before the handler's licence check"
        );
    }
}

/// Registering the two `GET`s must not turn the `POST` beside them into our 405.
#[tokio::test]
async fn other_methods_are_forwarded() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let fixture = fixture(&client, &token).await;

    let p = bookmarks(&fixture.channel);
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
