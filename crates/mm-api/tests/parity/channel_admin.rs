//! Cross-server parity for three channel-administration writes —
//! `PUT /api/v4/channels/{channel_id}/scheme` (`updateChannelScheme`),
//! `PUT /api/v4/channels/{channel_id}/moderations/patch` (`patchChannelModerations`) and
//! `PUT /api/v4/channels/{channel_id}/members/{user_id}/autotranslation`
//! (`updateChannelMemberAutotranslation`).
//!
//! ```sh
//! scripts/parity.sh -p mm-api --test parity channel_admin
//! ```
//!
//! # Two of the three are a gate and nothing else; the third has one step in front of its gate
//!
//! `patchChannelModerations` and `updateChannelMemberAutotranslation` return before validating an
//! id, before reading a body and before asking a permission, so on this stack the whole reachable
//! behaviour is one error each. `updateChannelScheme` validates the channel id and then the body
//! **first**, which puts a 400 in front of the 403 — measured, and the reason this suite exists
//! rather than one more row in `licence_gated_channels`.
//!
//! # The gate is not the same gate
//!
//! Two of them are `Channels().License() == nil`. The third is `AutoTranslation() == nil ||
//! !IsFeatureAvailable()` — an enterprise interface, not a licence field. They coincide on an
//! unlicensed OSS build and this suite cannot tell them apart; what it can pin is that the
//! licensed *branch* forwards, which is what [`a_license_row_hands_the_two_licence_routes_back`]
//! does for the two that are genuinely licence-gated.

use crate::common;

use common::{
    ACTIVE_LICENCE_ROW, GO, RUST, assert_error_bodies_match_except_known_gaps, client,
    create_channel, create_plain_user, create_team, go_minted_token, set_active_licence_id,
    stack_enabled,
};

/// A 26-character id that is a valid `IsValidId` but names nothing.
const ABSENT: &str = "mmrschadmin00000000000001x";
/// The scheme id the body gate accepts. Nothing reads it — the 403 lands first.
const SCHEME: &str = "mmrschadminscheme000000001";

struct Fixture {
    channel: String,
    admin: String,
    plain_token: String,
    plain_user: String,
}

static FIXTURE: tokio::sync::OnceCell<Fixture> = tokio::sync::OnceCell::const_new();

async fn fixture(client: &reqwest::Client, token: &str) -> &'static Fixture {
    FIXTURE
        .get_or_init(|| async {
            let team = create_team(client, token, "chadmin").await;
            let channel = create_channel(client, token, &team, "chadmin").await;
            let plain = create_plain_user(client, token, &team, "chadmin").await;
            let admin = client
                .get(format!("{GO}/api/v4/users/me"))
                .header("Authorization", format!("Bearer {token}"))
                .send()
                .await
                .expect("Go answers")
                .json::<serde_json::Value>()
                .await
                .expect("a user")["id"]
                .as_str()
                .expect("an id")
                .to_owned();
            Fixture {
                channel,
                admin,
                plain_token: plain.token,
                plain_user: plain.id,
            }
        })
        .await
}

/// `PUT` raw bytes to a path on both servers with the same token — the `PUT` counterpart of
/// `common::post_both_raw`, kept local because this is the only suite that needs it.
///
/// Raw bytes rather than a typed body on purpose: `updateChannelScheme`'s 400 branch is largely
/// about bodies that are not well-formed JSON, which a `serde_json::Value` cannot express.
async fn put_both_raw(
    client: &reqwest::Client,
    token: &str,
    path: &str,
    body: &[u8],
) -> ((u16, Vec<u8>), (u16, Vec<u8>)) {
    let put = async |base: &str| {
        let response = client
            .put(format!("{base}{path}"))
            .header("Authorization", format!("Bearer {token}"))
            .header("Content-Type", "application/json")
            .body(body.to_vec())
            .send()
            .await
            .unwrap_or_else(|e| panic!("{base}{path} is unreachable: {e}"));
        let status = response.status().as_u16();
        (status, response.bytes().await.expect("body reads").to_vec())
    };

    (put(GO).await, put(RUST).await)
}

fn scheme(channel_id: &str) -> String {
    format!("/api/v4/channels/{channel_id}/scheme")
}
fn moderations_patch(channel_id: &str) -> String {
    format!("/api/v4/channels/{channel_id}/moderations/patch")
}
fn autotranslation(channel_id: &str, user_id: &str) -> String {
    format!("/api/v4/channels/{channel_id}/members/{user_id}/autotranslation")
}

/// The three errors, byte-compatible with Go, at their own statuses and their own ids.
#[tokio::test]
async fn each_route_answers_its_own_gate_error() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let fixture = fixture(&client, &token).await;

    for (p, body, id) in [
        (
            scheme(&fixture.channel),
            format!(r#"{{"scheme_id":"{SCHEME}"}}"#),
            "api.channel.update_channel_scheme.license.error",
        ),
        (
            moderations_patch(&fixture.channel),
            "[]".to_owned(),
            "api.channel.patch_channel_moderations.license.error",
        ),
        (
            autotranslation(&fixture.channel, &fixture.admin),
            r#"{"autotranslation_disabled":true}"#.to_owned(),
            "api.channel.update_channel_member_autotranslation.feature_not_available.app_error",
        ),
    ] {
        let ((go_status, go), (rs_status, rs)) =
            put_both_raw(&client, &token, &p, body.as_bytes()).await;
        assert_eq!(
            go_status,
            403,
            "{p}: {}",
            String::from_utf8_lossy(&go)
                .chars()
                .take(200)
                .collect::<String>()
        );
        assert_eq!(rs_status, go_status, "{p}");
        let parsed = assert_error_bodies_match_except_known_gaps(&go, &rs, &p);
        assert_eq!(parsed["id"], id, "{p}");
        assert!(!rs.ends_with(b"\n"), "{p}: error bodies carry no newline");
    }
}

/// **Two of the three gates precede `RequireChannelId`.** `abc` is far too short to be a channel
/// id and is not a channel that exists, and `moderations/patch` and `autotranslation` still answer
/// their gate error rather than a 400 — the gate is each handler's first statement.
///
/// `scheme` is the exception, and the whole point of separating it: the same `abc` there is an
/// `invalid_url_param` 400, because `RequireChannelId` runs before the licence check.
#[tokio::test]
async fn the_gate_ordering_against_require_channel_id_is_visible() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;

    for (p, body) in [
        (moderations_patch("abc"), "[]"),
        (autotranslation("abc", "abc"), "{}"),
    ] {
        let ((go_status, go), (rs_status, rs)) =
            put_both_raw(&client, &token, &p, body.as_bytes()).await;
        assert_eq!(go_status, 403, "{p}: the gate precedes `RequireChannelId`");
        assert_eq!(rs_status, go_status, "{p}");
        let parsed = assert_error_bodies_match_except_known_gaps(&go, &rs, &p);
        assert_ne!(
            parsed["id"], "api.context.invalid_url_param.app_error",
            "{p}: and it is emphatically not the id error"
        );
    }

    let p = scheme("abc");
    let ((go_status, go), (rs_status, rs)) = put_both_raw(
        &client,
        &token,
        &p,
        format!(r#"{{"scheme_id":"{SCHEME}"}}"#).as_bytes(),
    )
    .await;
    assert_eq!(go_status, 400, "{p}: here the id check comes first");
    assert_eq!(rs_status, go_status, "{p}");
    let parsed = assert_error_bodies_match_except_known_gaps(&go, &rs, &p);
    assert_eq!(
        parsed["id"], "api.context.invalid_url_param.app_error",
        "{p}"
    );
}

/// **And a channel that does not exist is not a 404.** Nothing before any of the three gates reads
/// the `Channels` table, so a well-formed id naming nothing gets the same gate error as a real
/// channel — for `scheme` that means the 403 survives a `GetChannel` that never happens.
#[tokio::test]
async fn an_absent_channel_gets_the_gate_error_and_not_a_404() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;

    for (p, body) in [
        (scheme(ABSENT), format!(r#"{{"scheme_id":"{SCHEME}"}}"#)),
        (moderations_patch(ABSENT), "[]".to_owned()),
        (autotranslation(ABSENT, ABSENT), "{}".to_owned()),
    ] {
        let ((go_status, go), (rs_status, rs)) =
            put_both_raw(&client, &token, &p, body.as_bytes()).await;
        assert_eq!(go_status, 403, "{p}");
        assert_eq!(rs_status, go_status, "{p}");
        assert_error_bodies_match_except_known_gaps(&go, &rs, &p);
    }
}

/// **`updateChannelScheme`'s body gate, all of it.** Go collapses a decode error, a missing key,
/// an explicit null and an id that is not 26 letters-or-numbers into one 400 naming `scheme_id`;
/// anything that passes all four reaches the 403. The pairs below are the two answers either side
/// of that line, and a port that gated on the licence first would return 403 for every row.
#[tokio::test]
async fn the_scheme_body_gate_separates_400_from_403() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let fixture = fixture(&client, &token).await;
    let p = scheme(&fixture.channel);

    let four_hundred: &[&str] = &[
        "",
        "garbage",
        "[]",
        "null",
        "{}",
        r#"{"scheme_id":null}"#,
        r#"{"scheme_id":""}"#,
        r#"{"scheme_id":"nope"}"#,
        r#"{"scheme_id":"abcdefghijklmnopqrstuvwxy"}"#,
        r#"{"scheme_id":"abcdefghijklmnopqrstuvwxyza"}"#,
        r#"{"scheme_id":"ab-defghijklmnopqrstuvwxyz"}"#,
        r#"{"scheme_id":5}"#,
    ];
    for body in four_hundred {
        let ((go_status, go), (rs_status, rs)) =
            put_both_raw(&client, &token, &p, body.as_bytes()).await;
        assert_eq!(go_status, 400, "body {body:?} should not pass the gate");
        assert_eq!(rs_status, go_status, "body {body:?}");
        let parsed = assert_error_bodies_match_except_known_gaps(&go, &rs, &p);
        assert_eq!(
            parsed["id"], "api.context.invalid_body_param.app_error",
            "body {body:?}"
        );
        assert_eq!(
            parsed["message"],
            "Invalid or missing scheme_id in request body."
        );
    }

    // `IsValidId` is 26 bytes of Unicode letters-or-numbers, not base32 — so upper case and all
    // digits pass it — and `Decode` consumes one value and ignores the trailer.
    let four_oh_three: &[String] = &[
        format!(r#"{{"scheme_id":"{SCHEME}"}}"#),
        r#"{"scheme_id":"ABCDEFGHIJKLMNOPQRSTUVWXYZ"}"#.to_owned(),
        r#"{"scheme_id":"12345678901234567890123456"}"#.to_owned(),
        format!(r#"{{"scheme_id":"{SCHEME}"}} and then some"#),
        format!(r#"{{"scheme_id":"{SCHEME}","unknown":1}}"#),
    ];
    for body in four_oh_three {
        let ((go_status, go), (rs_status, rs)) =
            put_both_raw(&client, &token, &p, body.as_bytes()).await;
        assert_eq!(go_status, 403, "body {body:?} should have passed the gate");
        assert_eq!(rs_status, go_status, "body {body:?}");
        let parsed = assert_error_bodies_match_except_known_gaps(&go, &rs, &p);
        assert_eq!(
            parsed["id"], "api.channel.update_channel_scheme.license.error",
            "body {body:?}"
        );
    }
}

/// **Every gate precedes every permission question.** A plain user who is not in the channel gets
/// the same three errors the system admin does — for `scheme` that is `manage_system` never
/// asked, and for `autotranslation` it is `edit_other_users` never asked even though the target is
/// a *different* user.
#[tokio::test]
async fn a_plain_user_gets_the_gate_errors_and_not_permission_ones() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let fixture = fixture(&client, &token).await;

    for (p, body) in [
        (
            scheme(&fixture.channel),
            format!(r#"{{"scheme_id":"{SCHEME}"}}"#),
        ),
        (moderations_patch(&fixture.channel), "[]".to_owned()),
        // The plain user asking about the **admin's** membership, which `edit_other_users` would
        // refuse if the gate were not first.
        (
            autotranslation(&fixture.channel, &fixture.admin),
            r#"{"autotranslation_disabled":true}"#.to_owned(),
        ),
    ] {
        let ((go_status, go), (rs_status, rs)) =
            put_both_raw(&client, &fixture.plain_token, &p, body.as_bytes()).await;
        assert_eq!(go_status, 403, "{p}");
        assert_eq!(rs_status, go_status, "{p}");
        let parsed = assert_error_bodies_match_except_known_gaps(&go, &rs, &p);
        assert_ne!(
            parsed["id"], "api.context.permissions.app_error",
            "{p}: the gate's error, not a permission one"
        );
    }

    // And the plain user asking about themselves, where the permission would have been granted —
    // same error, which is what makes the previous row's `assert_ne` mean something.
    let p = autotranslation(&fixture.channel, &fixture.plain_user);
    let ((go_status, go), (rs_status, rs)) =
        put_both_raw(&client, &fixture.plain_token, &p, b"{}").await;
    assert_eq!(go_status, 403, "{p}");
    assert_eq!(rs_status, go_status, "{p}");
    assert_error_bodies_match_except_known_gaps(&go, &rs, &p);
}

/// **The boundary.** A valid `Systems.ActiveLicenseId` sends the two licence-gated routes back to
/// the proxy — everything behind those gates is enterprise and unported. Holds the shared lock
/// exclusively: the same row decides `licence_gated_channels` and `license_client`.
///
/// `autotranslation` is deliberately in this list too. Its gate is *not* the licence, but what a
/// licensed enterprise build answers there is not visible from this tree, so it forwards on the
/// same signal rather than guessing.
#[tokio::test]
async fn a_license_row_hands_the_two_licence_routes_back() {
    if !stack_enabled() {
        return;
    }
    let _exclusive = ACTIVE_LICENCE_ROW.write().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let fixture = fixture(&client, &token).await;

    let served_by = async |p: &str, body: &str| {
        client
            .put(format!("{RUST}{p}"))
            .header("Authorization", format!("Bearer {token}"))
            .header("Content-Type", "application/json")
            .body(body.to_owned())
            .send()
            .await
            .expect("we answer")
            .headers()
            .get("x-mmrs-served-by")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    };

    let valid_scheme = format!(r#"{{"scheme_id":"{SCHEME}"}}"#);
    let paths: [(String, &str); 3] = [
        (scheme(&fixture.channel), valid_scheme.as_str()),
        (moderations_patch(&fixture.channel), "[]"),
        (
            autotranslation(&fixture.channel, &fixture.admin),
            r#"{"autotranslation_disabled":true}"#,
        ),
    ];

    set_active_licence_id(None).await;
    for (p, body) in &paths {
        assert_eq!(
            served_by(p, body).await.as_deref(),
            Some("rust"),
            "{p}: unlicensed, so ours to answer"
        );
    }

    set_active_licence_id(Some("mmrslicence000000000000001")).await;
    let mut forwarded = Vec::new();
    for (p, body) in &paths {
        forwarded.push(served_by(p, body).await);
    }
    set_active_licence_id(None).await;

    for ((p, _), served) in paths.iter().zip(&forwarded) {
        assert_eq!(
            served.as_deref(),
            Some("go"),
            "{p}: a licence means work we have not ported"
        );
    }

    // And the 400 in front of `scheme`'s gate is **not** licence-dependent: a bad body is ours to
    // answer either way, which is the branch a "forward when licensed" shortcut would lose.
    set_active_licence_id(Some("mmrslicence000000000000001")).await;
    let bad = served_by(&scheme(&fixture.channel), r#"{"scheme_id":"nope"}"#).await;
    set_active_licence_id(None).await;
    assert_eq!(
        bad.as_deref(),
        Some("rust"),
        "the body gate runs before the licence check, licensed or not"
    );
}
