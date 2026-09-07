//! Cross-server parity for `GET /api/v4/channels/{channel_id}/timezones`.
//!
//! ```sh
//! docker compose up -d
//! scripts/parity.sh -p mm-api --test parity channel_timezones
//! ```
//!
//! # The route is three transformations over one unordered query
//!
//! The SQL has no `ORDER BY` and no filtering at all; the app layer drops members with no
//! timezone, resolves each survivor to *one* of its two fields, and then deduplicates through a
//! helper that **sorts**. So the response is alphabetical — and that sort is the only thing
//! making a byte comparison meaningful here.
//!
//! # The empty answer is `null`
//!
//! `var timezones []string` is never allocated when nothing survives the filter, and
//! `ArrayToJSON` of a nil slice is `null`. On a server where nobody has opened the timezone
//! setting that is every channel, so it is the common case rather than an edge one.

use crate::common;

use common::{
    GO, RUST, add_user_to_channel, assert_error_bodies_match_except_known_gaps, client,
    create_channel, create_channel_typed, create_plain_user, fetch_both, fetch_both_raw,
    go_minted_token, logged_in_user_id, patch_user_timezone, purge_api_fixtures, stack_enabled,
};

// ---------------------------------------------------------------------------------------------
// fixtures
// ---------------------------------------------------------------------------------------------

struct Fixture {
    /// Five members: two with distinct timezones, one whose timezone duplicates another's, one
    /// with only *half* its timezone set, and one with none at all.
    channel_id: String,
    /// A channel whose only member has no timezone set.
    empty_channel_id: String,
    /// A private channel the outsider is not in.
    private_channel_id: String,
    outsider_token: String,
    /// The timezones the fixture set, already sorted and deduplicated the way Go will return them.
    expected: Vec<String>,
}

static FIXTURE: tokio::sync::OnceCell<Fixture> = tokio::sync::OnceCell::const_new();

async fn fixture(client: &reqwest::Client, token: &str) -> &'static Fixture {
    FIXTURE
        .get_or_init(|| async {
            purge_api_fixtures().await;
            let (team_id, _) = common::a_team_and_channel_the_user_is_in(client, token).await;
            let channel_id = create_channel(client, token, &team_id, "tzmain").await;

            // The three plain users carry the timezones, so the shared fixture user's own
            // setting — which other suites may change — cannot move this answer.
            let automatic = create_plain_user(client, token, &team_id, "tzauto").await;
            let manual = create_plain_user(client, token, &team_id, "tzman").await;
            let duplicate = create_plain_user(client, token, &team_id, "tzdup").await;
            let half = create_plain_user(client, token, &team_id, "tzhalf").await;
            let none = create_plain_user(client, token, &team_id, "tznone").await;

            // `useAutomaticTimezone` is the string "true", so this user reports the *automatic*
            // field and the manual one beside it is a decoy: a port reading the wrong field
            // returns `Pacific/Auckland` here.
            patch_user_timezone(
                client,
                token,
                &automatic.id,
                "true",
                "Europe/Lisbon",
                "Pacific/Auckland",
            )
            .await;
            // …and this one reports the manual field, with the automatic one as the decoy.
            patch_user_timezone(
                client,
                token,
                &manual.id,
                "false",
                "America/Denver",
                "Asia/Kolkata",
            )
            .await;
            // A second member resolving to a timezone another member already has: without this
            // the deduplication is untested, because two distinct values dedupe to themselves.
            patch_user_timezone(client, token, &duplicate.id, "false", "", "Europe/Lisbon").await;

            // One field empty and the other set. The filter is `automatic == "" && manual == ""`
            // — an **and** — so this member survives it; a port that wrote `||` would drop it,
            // and its timezone is unique precisely so that dropping it is visible where the
            // duplicate user's would be masked by the deduplication.
            patch_user_timezone(client, token, &half.id, "false", "", "Pacific/Fiji").await;

            for user in [&automatic, &manual, &duplicate, &half, &none] {
                add_user_to_channel(client, token, &channel_id, &user.id).await;
            }

            let empty_channel_id = create_channel(client, token, &team_id, "tzempty").await;
            add_user_to_channel(client, token, &empty_channel_id, &none.id).await;

            let private_channel_id =
                create_channel_typed(client, token, &team_id, "tzpriv", "P").await;
            add_user_to_channel(client, token, &private_channel_id, logged_in_user_id()).await;

            let outsider = create_plain_user(client, token, &team_id, "tzout").await;

            // Sorted, because `RemoveDuplicateStrings` sorts. `Asia/Kolkata` is the manual
            // user's; `Europe/Lisbon` appears twice in the table and once here.
            let expected = vec![
                "Asia/Kolkata".to_owned(),
                "Europe/Lisbon".to_owned(),
                "Pacific/Fiji".to_owned(),
            ];

            Fixture {
                channel_id,
                empty_channel_id,
                private_channel_id,
                outsider_token: outsider.token,
                expected,
            }
        })
        .await
}

// ---------------------------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------------------------

/// The whole route: the filter, `GetPreferredTimezone`'s field choice, the deduplication and the
/// sort, in one byte comparison plus the four claims that make it non-vacuous.
#[tokio::test]
async fn the_timezone_list_is_byte_identical() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let path = format!("/api/v4/channels/{}/timezones", f.channel_id);
    let (go, rs) = fetch_both(&client, &token, &path).await;
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{path} must be byte-identical"
    );
    assert!(
        !go.ends_with(b"\n"),
        "w.Write of ArrayToJSON writes no trailing newline"
    );

    let parsed: Vec<String> = serde_json::from_slice(&go).expect("an array of strings");
    assert_eq!(
        parsed, f.expected,
        "sorted, deduplicated, and each member resolved through its own useAutomaticTimezone"
    );
    // Named individually so a failure says which of the four rules broke.
    assert!(
        parsed.contains(&"Europe/Lisbon".to_owned()),
        "useAutomaticTimezone=true reports the automatic field"
    );
    assert!(
        !parsed.contains(&"Pacific/Auckland".to_owned()),
        "…and not the manual decoy beside it"
    );
    assert!(
        parsed.contains(&"Asia/Kolkata".to_owned()),
        "useAutomaticTimezone=false reports the manual field"
    );
    assert!(
        !parsed.contains(&"America/Denver".to_owned()),
        "…and not the automatic decoy beside it"
    );
    assert_eq!(
        parsed.iter().filter(|tz| *tz == "Europe/Lisbon").count(),
        1,
        "two members share a timezone and it appears once"
    );
    assert!(
        parsed.contains(&"Pacific/Fiji".to_owned()),
        "the filter is `automatic == \"\" && manual == \"\"`, so half a timezone survives it"
    );
}

/// A member with no timezone at all contributes nothing — and a channel of nothing but such
/// members answers the four bytes `null`, not `[]`.
#[tokio::test]
async fn a_channel_with_no_timezones_answers_null() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let path = format!("/api/v4/channels/{}/timezones", f.empty_channel_id);
    let (go, rs) = fetch_both(&client, &token, &path).await;
    assert_eq!(
        go, b"null",
        "a nil slice marshals to null; an allocated empty one would be []"
    );
    assert_eq!(rs, go);
}

/// `read_channel` — not the `read_channel_content` the post routes on the same channel use, and
/// the difference is the whole test.
///
/// `SessionHasPermissionToChannel` falls back to the **team**, where `team_user` grants
/// `read_public_channel` and *not* `read_channel`. So this route refuses a non-member on a
/// **public** channel, where `getPinnedPosts` — which goes through `HasPermissionToReadChannel`
/// and its explicit open-channel fallback — serves the same user the same channel. Both halves
/// are asserted, because "refused" alone would pass on a port that refuses everybody.
#[tokio::test]
async fn a_non_member_is_refused_even_on_a_public_channel() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    for channel in [&f.private_channel_id, &f.channel_id] {
        let path = format!("/api/v4/channels/{channel}/timezones");
        let ((go_status, go_body), (rs_status, rs_body)) =
            fetch_both_raw(&client, &f.outsider_token, &path).await;
        assert_eq!(
            go_status, 403,
            "{path}: read_channel has no public fallback"
        );
        assert_eq!(rs_status, go_status);
        let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &path);
        assert_eq!(go["id"], "api.context.permissions.app_error");
    }

    // The same user, the same **public** channel, one route over: served. That is the
    // `read_channel_content` path's open-channel fallback, and it is what makes the refusals
    // above a statement about this route rather than about the user.
    let pinned = format!("/api/v4/channels/{}/pinned", f.channel_id);
    let (go, rs) = fetch_both(&client, &f.outsider_token, &pinned).await;
    assert_eq!(go, rs, "{pinned}");
}

/// The channel is **never fetched**, so an id naming nothing cannot 404 — the permission check
/// simply finds no membership and refuses. The opposite of `getPinnedPosts` on the same channel.
#[tokio::test]
async fn an_unknown_channel_is_a_403_not_a_404() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let _ = fixture(&client, &token).await;

    let path = "/api/v4/channels/zzzzzzzzzzzzzzzzzzzzzzzzzz/timezones";
    // The fixture user is a system admin, which passes the check on any channel — so the
    // question is only answerable with an actor that can be refused.
    let f = fixture(&client, &token).await;
    let ((go_status, go_body), (rs_status, rs_body)) =
        fetch_both_raw(&client, &f.outsider_token, path).await;
    assert_eq!(
        go_status, 403,
        "no GetChannel on this route, so nothing can raise a 404"
    );
    assert_eq!(rs_status, go_status);
    assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, path);

    // And the pinned route on the *same* id does 404, which is what makes the claim a
    // comparison rather than an assertion about one number.
    let pinned = "/api/v4/channels/zzzzzzzzzzzzzzzzzzzzzzzzzz/pinned";
    let ((pinned_status, _), _) = fetch_both_raw(&client, &f.outsider_token, pinned).await;
    assert_eq!(pinned_status, 404, "its sibling fetches the channel first");
}

#[tokio::test]
async fn a_malformed_channel_id_is_a_400_on_both() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let _ = fixture(&client, &token).await;

    let path = "/api/v4/channels/short/timezones";
    let ((go_status, go_body), (rs_status, rs_body)) = fetch_both_raw(&client, &token, path).await;
    assert_eq!(go_status, 400);
    assert_eq!(rs_status, go_status);
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, path);
    assert_eq!(go["id"], "api.context.invalid_url_param.app_error");
}
/// `/api/v4/system/timezones` is a **different route**, and since 2026-09-07 both are ours.
///
/// This test used to assert that the system list was still forwarded — a canary for the two paths
/// not colliding, which worked only while one of them was Go's. Now that both are answered here,
/// the collision it was guarding against is a live possibility rather than a hypothetical, so the
/// assertion is the stronger one: the two routes return **different** bodies, and the system one
/// returns the global table rather than this channel's members' zones.
#[tokio::test]
async fn the_system_timezone_route_is_a_different_answer() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let fixture = fixture(&client, &token).await;

    let get = async |base: &str, path: &str| {
        let response = client
            .get(format!("{base}{path}"))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .expect("reachable");
        assert_eq!(response.status(), 200, "{base}{path}");
        response.bytes().await.expect("reads").to_vec()
    };

    let system_path = "/api/v4/system/timezones";
    let channel_path = format!("/api/v4/channels/{}/timezones", fixture.channel_id);

    let system = get(RUST, system_path).await;
    assert_eq!(
        system,
        get(GO, system_path).await,
        "the system list must still match Go's"
    );

    let channel_zones = get(RUST, &channel_path).await;
    assert_ne!(
        String::from_utf8_lossy(&system),
        String::from_utf8_lossy(&channel_zones),
        "the global table and this channel's members' zones must not be the same answer"
    );

    let system: Vec<String> = serde_json::from_slice(&system).expect("an array of strings");
    let channel_zones: Vec<String> =
        serde_json::from_slice(&channel_zones).expect("an array of strings");
    assert!(
        system.len() > channel_zones.len(),
        "592 supported zones against a handful of members': {} vs {}",
        system.len(),
        channel_zones.len()
    );
}
