//! Cross-server parity for
//! `GET /api/v4/teams/name/{team_name}/channels/name/{channel_name}`.
//!
//! ```sh
//! scripts/parity.sh -p mm-api --test parity channel_by_name_for_team_name
//! ```
//!
//! The by-**name** twin of `getChannelByName`: same permission block, same `FillInChannelProps`,
//! same trailing newline — all of which `channel_by_name` already pins. What is new here is the
//! team half, and that is where this suite spends itself:
//!
//! - a second mux class and a second validator, in Go's order (team first);
//! - a **team** 404 that is a different error id from the channel one, and comes first;
//! - both segments lower-cased before anything looks at them.

use crate::common;

use common::{
    GO, RUST, assert_error_bodies_match_except_known_gaps, client, create_channel,
    create_channel_typed, create_plain_user, create_team, delete_channel, fetch_both_raw,
    fetch_both_stable, go_minted_token, purge_api_fixtures, stack_enabled,
};

/// The team and channel names this suite builds, which are also the path segments it asks for.
const TEAM_NAME: &str = "mmrs-parity-bynameteam";
const OPEN_NAME: &str = "mmrs-parity-bynameopen";
const PRIVATE_NAME: &str = "mmrs-parity-bynameshut";
const ARCHIVED_NAME: &str = "mmrs-parity-bynamegone";

struct Fixture {
    plain_token: String,
}

static FIXTURE: tokio::sync::OnceCell<Fixture> = tokio::sync::OnceCell::const_new();

async fn fixture(client: &reqwest::Client, token: &str) -> &'static Fixture {
    FIXTURE
        .get_or_init(|| async {
            purge_api_fixtures().await;

            let team_id = create_team(client, token, "bynameteam").await;
            create_channel(client, token, &team_id, "bynameopen").await;
            create_channel_typed(client, token, &team_id, "bynameshut", "P").await;

            // Archived, so `?include_deleted` has something to reach and something to miss.
            let archived = create_channel(client, token, &team_id, "bynamegone").await;
            delete_channel(client, token, &archived).await;

            // A team member who is not in the private channel — the shared tail's 404 needs one.
            let plain = create_plain_user(client, token, &team_id, "byname").await;

            Fixture {
                plain_token: plain.token,
            }
        })
        .await
}

fn path(team: &str, channel: &str) -> String {
    format!("/api/v4/teams/name/{team}/channels/name/{channel}")
}

// ---------------------------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------------------------

/// The success, byte for byte — and the same channel through **mixed-case** segments, which Go
/// lower-cases before either validator sees them.
#[tokio::test]
async fn a_channel_reached_by_team_name_is_byte_identical() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let _f = fixture(&client, &token).await;

    let lower = path(TEAM_NAME, OPEN_NAME);
    let (go, rs) = fetch_both_stable(&client, &token, &lower).await;
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{lower} must be byte-identical"
    );
    assert_eq!(
        go.last(),
        Some(&b'\n'),
        "json.NewEncoder(w).Encode appends the newline json.Marshal does not"
    );
    let parsed: serde_json::Value = serde_json::from_slice(&go).expect("JSON");
    assert_eq!(parsed["name"], OPEN_NAME, "the right channel came back");

    // `params.TeamName` and `params.ChannelName` are both `strings.ToLower`ed (params.go:178),
    // so an upper-case segment is the same request — and `IsValidTeamName` never sees a capital.
    let upper = path(&TEAM_NAME.to_uppercase(), &OPEN_NAME.to_uppercase());
    let (go_upper, rs_upper) = fetch_both_stable(&client, &token, &upper).await;
    assert_eq!(go_upper, rs_upper, "{upper} must be byte-identical");
    assert_eq!(
        go_upper, go,
        "and it must be the same answer as the lower-case path"
    );
}

/// The team lookup runs first, so a request that names neither reports the **team**.
#[tokio::test]
async fn an_unknown_team_name_is_a_404_that_beats_an_unknown_channel() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let _f = fixture(&client, &token).await;

    for channel in [OPEN_NAME, "mmrs-parity-nosuchchannel"] {
        let p = path("mmrs-parity-nosuchteam", channel);
        let ((go_status, go_body), (rs_status, rs_body)) =
            fetch_both_raw(&client, &token, &p).await;
        assert_eq!(go_status, 404, "{p}");
        assert_eq!(rs_status, go_status, "{p}: statuses must match");
        let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &p);
        assert_eq!(
            go["id"], "app.team.get_by_name.missing.app_error",
            "{p}: the team is reported even when the channel is also wrong"
        );
    }
}

/// A known team with an unknown channel is the *other* 404.
#[tokio::test]
async fn an_unknown_channel_in_a_known_team_is_the_channel_404() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let _f = fixture(&client, &token).await;

    let p = path(TEAM_NAME, "mmrs-parity-nosuchchannel");
    let ((go_status, go_body), (rs_status, rs_body)) = fetch_both_raw(&client, &token, &p).await;
    assert_eq!(go_status, 404, "{p}");
    assert_eq!(rs_status, go_status, "{p}: statuses must match");
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &p);
    assert_eq!(go["id"], "app.channel.get_by_name.missing.app_error");
}

/// `IsValidTeamName` is `isValidAlphaNum` plus a **two-character minimum**, and the mux has
/// already refused everything else — so a one-character team name is the only reachable 400 on
/// this segment.
#[tokio::test]
async fn a_one_character_team_name_is_a_400_on_both() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let _f = fixture(&client, &token).await;

    let p = path("a", OPEN_NAME);
    let ((go_status, go_body), (rs_status, rs_body)) = fetch_both_raw(&client, &token, &p).await;
    assert_eq!(go_status, 400, "{p}");
    assert_eq!(rs_status, go_status, "{p}: statuses must match");
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &p);
    assert_eq!(go["id"], "api.context.invalid_url_param.app_error");
    assert!(
        go["message"]
            .as_str()
            .unwrap_or_default()
            .contains("team_name"),
        "the team validator runs first: {}",
        go["message"]
    );
}

/// `RequireChannelName` is `IsValidChannelIdentifier`, whose first character must be
/// **alphanumeric** — while the mux happily accepts a leading `-` or `_`. That gap is the only
/// reachable 400 on this segment: a one-character name passes the validator and 404s instead,
/// measured, so there is no minimum-length rule here to lean on.
#[tokio::test]
async fn a_channel_name_starting_with_a_hyphen_or_underscore_is_a_400_on_both() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let _f = fixture(&client, &token).await;

    for name in ["-ab", "_ab"] {
        let p = path(TEAM_NAME, name);
        let ((go_status, go_body), (rs_status, rs_body)) =
            fetch_both_raw(&client, &token, &p).await;
        assert_eq!(go_status, 400, "{p}");
        assert_eq!(rs_status, go_status, "{p}: statuses must match");
        let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &p);
        assert_eq!(go["id"], "api.context.invalid_url_param.app_error");
        assert!(
            go["message"]
                .as_str()
                .unwrap_or_default()
                .contains("channel_name"),
            "the channel validator is the one that refused: {}",
            go["message"]
        );
    }

    // A one-character name is *not* refused — it reaches the store and misses. Without this the
    // test above could pass against a validator that simply rejected everything short.
    let p = path(TEAM_NAME, "a");
    let ((go_status, go_body), (rs_status, rs_body)) = fetch_both_raw(&client, &token, &p).await;
    assert_eq!(
        go_status, 404,
        "{p}: no minimum-length rule on a channel name"
    );
    assert_eq!(rs_status, go_status);
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &p);
    assert_eq!(go["id"], "app.channel.get_by_name.missing.app_error");
}

/// A segment outside Go's `[A-Za-z0-9_-]+` never reaches a handler on either server: gorilla
/// 404s it, and this router forwards rather than guessing.
#[tokio::test]
async fn a_segment_outside_the_mux_is_forwarded() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let _f = fixture(&client, &token).await;

    for p in [
        path("mmrs.parity.team", OPEN_NAME),
        path(TEAM_NAME, "mmrs.parity.open"),
    ] {
        let rs = client
            .get(format!("{RUST}{p}"))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .expect("we answer");
        assert_eq!(
            rs.headers()
                .get("x-mmrs-served-by")
                .and_then(|v| v.to_str().ok()),
            Some("go"),
            "{p} must be forwarded, not served"
        );
        let go = client
            .get(format!("{GO}{p}"))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .expect("Go answers");
        assert_eq!(go.status(), rs.status(), "{p}: statuses must match");
    }
}

/// The shared tail, reached through this path: a private channel refuses a non-member with a
/// **404**, not the 403 an open channel would give.
#[tokio::test]
async fn a_private_channel_is_a_404_for_a_team_member_who_is_not_in_it() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let p = path(TEAM_NAME, PRIVATE_NAME);
    let ((go_status, go_body), (rs_status, rs_body)) =
        fetch_both_raw(&client, &f.plain_token, &p).await;
    assert_eq!(
        go_status, 404,
        "a private channel hides rather than refuses"
    );
    assert_eq!(rs_status, go_status, "{p}: statuses must match");
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &p);
    assert_eq!(go["id"], "app.channel.get_by_name.missing.app_error");

    // The admin, who holds `manage_team`, is admitted to the same channel — so the 404 above is
    // the permission block and not a missing fixture.
    let (go_ok, rs_ok) = fetch_both_stable(&client, &token, &p).await;
    assert_eq!(go_ok, rs_ok, "{p} must be byte-identical for the admin");
    let parsed: serde_json::Value = serde_json::from_slice(&go_ok).expect("JSON");
    assert_eq!(parsed["type"], "P");
}

/// `?include_deleted` picks the store variant, and an archived channel is reachable only with it.
#[tokio::test]
async fn include_deleted_chooses_whether_an_archived_channel_exists() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let _f = fixture(&client, &token).await;

    let bare = path(TEAM_NAME, ARCHIVED_NAME);
    let ((go_status, go_body), (rs_status, rs_body)) = fetch_both_raw(&client, &token, &bare).await;
    assert_eq!(go_status, 404, "archived channels are absent by default");
    assert_eq!(rs_status, go_status, "{bare}: statuses must match");
    assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &bare);

    let with = format!("{bare}?include_deleted=true");
    let (go_ok, rs_ok) = fetch_both_stable(&client, &token, &with).await;
    assert_eq!(go_ok, rs_ok, "{with} must be byte-identical");
    let parsed: serde_json::Value = serde_json::from_slice(&go_ok).expect("JSON");
    assert!(
        parsed["delete_at"].as_i64().unwrap_or(0) > 0,
        "and the channel it returns really is archived"
    );
}

/// Everything but `GET` on this path stays Go's.
#[tokio::test]
async fn other_methods_are_forwarded() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let _f = fixture(&client, &token).await;

    let p = path(TEAM_NAME, OPEN_NAME);
    for method in [reqwest::Method::POST, reqwest::Method::PUT] {
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

/// An unauthenticated request never reaches the handler.
#[tokio::test]
async fn no_session_is_a_401_on_both() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let p = path(TEAM_NAME, OPEN_NAME);

    let go = client
        .get(format!("{GO}{p}"))
        .send()
        .await
        .expect("Go answers");
    let rs = client
        .get(format!("{RUST}{p}"))
        .send()
        .await
        .expect("we answer");
    assert_eq!(go.status(), 401);
    assert_eq!(rs.status(), go.status(), "{p}: statuses must match");
    let go_body = go.bytes().await.expect("body").to_vec();
    let rs_body = rs.bytes().await.expect("body").to_vec();
    assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &p);
}
