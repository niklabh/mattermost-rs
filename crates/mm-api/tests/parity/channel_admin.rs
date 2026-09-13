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

// ---------------------------------------------------------------------------
// `GET /api/v4/channels/{channel_id}/members_minus_group_members`
//
// The one route in `api4/channel.go` that reads the group tables with no licence gate — so it
// answers 200 on this stack, and this half of the suite is about the rows rather than an error.
// ---------------------------------------------------------------------------

/// The three groups this suite inserts. Ids are `mmrschadmgrp…` so the purge below can find them
/// and so nothing another suite writes can collide.
const GROUP_ONE: &str = "mmrschadmgrp0000000000001x";
const GROUP_TWO: &str = "mmrschadmgrp0000000000002x";
const GROUP_THREE: &str = "mmrschadmgrp0000000000003x";

/// A channel whose membership is built to make every predicate in the query *matter*.
///
/// | member | groups | why it is here |
/// |---|---|---|
/// | admin | — | the creator, and the only member with no `GroupMembers` row at all |
/// | `chga` | one | the user `minus GROUP_ONE` must **exclude** |
/// | `chgb` | two | excluded only when `GROUP_TWO` is asked for |
/// | `chgc` | two, three | two joined rows for one user — what `count(DISTINCT)` is for |
/// | `chgd` | one, **deleted** | the subquery's `deleteat = 0`: not excluded, yet still reports the group |
/// | `chge` | — | deactivated, so `Users.DeleteAt = 0` drops them |
/// | a bot | — | a channel member that `Bots.UserId IS NULL` drops |
struct GroupFixture {
    channel: String,
    admin: String,
    users: std::collections::HashMap<&'static str, String>,
    plain_token: String,
}

static GROUP_FIXTURE: tokio::sync::OnceCell<GroupFixture> = tokio::sync::OnceCell::const_new();

async fn group_pool() -> sqlx::PgPool {
    let url = std::env::var("DATABASE_URL").expect("the parity stack exports DATABASE_URL");
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(std::time::Duration::from_secs(5))
        .connect(&url)
        .await
        .expect("the shared database is reachable")
}

async fn group_fixture(client: &reqwest::Client, token: &str) -> &'static GroupFixture {
    GROUP_FIXTURE
        .get_or_init(|| async {
            let team = create_team(client, token, "chadmgrp").await;
            let channel = create_channel(client, token, &team, "chadmgrp").await;

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

            let mut users = std::collections::HashMap::new();
            let mut plain_token = String::new();
            for tag in ["chga", "chgb", "chgc", "chgd", "chge"] {
                let user = create_plain_user(client, token, &team, tag).await;
                let joined = client
                    .post(format!("{GO}/api/v4/channels/{channel}/members"))
                    .header("Authorization", format!("Bearer {token}"))
                    .json(&serde_json::json!({ "user_id": user.id }))
                    .send()
                    .await
                    .expect("Go answers");
                assert!(joined.status().is_success(), "{tag} joins the channel");
                if tag == "chga" {
                    plain_token = user.token.clone();
                }
                users.insert(tag, user.id);
            }

            // A bot in the channel, for `Bots.UserId IS NULL`.
            //
            // **Planted, not created.** `POST /bots` is refused on this deployment —
            // `ServiceSettings.EnableBotAccountCreation` is false, which `bot_writes` asserts —
            // so the first version of this fixture swallowed a 403 and left no bot at all. The
            // mutation that drops `Bots.UserId IS NULL` then **survived**, and the survivor was
            // the only thing that said so: every assertion about the bot was inside
            // `if let Some(bot)`. `common::plant_bot` writes the same two rows `scripts/stack.sh`
            // writes for its own seeded bot.
            let bot_id = common::plant_bot("chadmgrp", &admin, 0)
                .await
                .expect("the parity stack exports DATABASE_URL");
            // Straight into `ChannelMembers` for the same reason: `POST /channels/{id}/members`
            // would need the bot in the team, and nothing here reads a bot's team membership.
            let pool = group_pool().await;
            sqlx::query(
                "INSERT INTO channelmembers
                    (channelid, userid, roles, lastviewedat, msgcount, mentioncount, notifyprops,
                     lastupdateat, schemeuser, schemeadmin, schemeguest, mentioncountroot,
                     msgcountroot, urgentmentioncount)
                 VALUES ($1, $2, '', 0, 0, 0, '{}'::jsonb, 1788600000000, TRUE, FALSE, FALSE, 0, 0, 0)
                 ON CONFLICT (channelid, userid) DO NOTHING",
            )
            .bind(&channel)
            .bind(&bot_id)
            .execute(&pool)
            .await
            .expect("the bot joins the channel");
            users.insert("bot", bot_id);

            // `chge` is deactivated **last**, so its `Users.UpdateAt` settles before any read.
            let deactivated = client
                .delete(format!("{GO}/api/v4/users/{}", users["chge"]))
                .header("Authorization", format!("Bearer {token}"))
                .send()
                .await
                .expect("Go answers");
            assert!(deactivated.status().is_success(), "chge is deactivated");

            // The group rows. Written by SQL because every route that would write them is
            // licence-gated to a 501 — there is no way to create a group through this server.
            let pool = group_pool().await;
            sqlx::query("DELETE FROM groupmembers WHERE groupid LIKE 'mmrschadmgrp%'")
                .execute(&pool)
                .await
                .expect("the old memberships clear");
            sqlx::query("DELETE FROM usergroups WHERE id LIKE 'mmrschadmgrp%'")
                .execute(&pool)
                .await
                .expect("the old groups clear");

            for (id, name) in [
                (GROUP_ONE, "mmrs-chadmgrp-one"),
                (GROUP_TWO, "mmrs-chadmgrp-two"),
                (GROUP_THREE, "mmrs-chadmgrp-three"),
            ] {
                sqlx::query(
                    "INSERT INTO usergroups
                       (id, name, displayname, description, source, remoteid,
                        createat, updateat, deleteat, allowreference)
                     VALUES ($1, $2, $3, 'a group this suite made', 'custom', NULL,
                             1700000000000, 1700000000000, 0, TRUE)",
                )
                .bind(id)
                .bind(name)
                .bind(format!("Display {name}"))
                .execute(&pool)
                .await
                .expect("the group is written");
            }

            for (group, user, delete_at) in [
                (GROUP_ONE, users["chga"].as_str(), 0i64),
                (GROUP_TWO, users["chgb"].as_str(), 0),
                (GROUP_TWO, users["chgc"].as_str(), 0),
                (GROUP_THREE, users["chgc"].as_str(), 0),
                // The deleted membership: `chgd` is still reported as being in `GROUP_ONE` by the
                // outer `string_agg`, which has no `DeleteAt` filter, and is **not** excluded by
                // the subquery, which has one.
                (GROUP_ONE, users["chgd"].as_str(), 1700000001000),
            ] {
                sqlx::query(
                    "INSERT INTO groupmembers (groupid, userid, createat, deleteat)
                     VALUES ($1, $2, 1700000000000, $3)",
                )
                .bind(group)
                .bind(user)
                .bind(delete_at)
                .execute(&pool)
                .await
                .expect("the membership is written");
            }

            GroupFixture {
                channel,
                admin,
                users,
                plain_token,
            }
        })
        .await
}

fn minus(channel_id: &str, query: &str) -> String {
    format!("/api/v4/channels/{channel_id}/members_minus_group_members?{query}")
}

/// The answer itself, byte for byte, and then the seven facts about the fixture that make the
/// byte comparison mean something.
#[tokio::test]
async fn the_page_matches_go_byte_for_byte_and_every_predicate_bites() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = group_fixture(&client, &token).await;

    let p = minus(&f.channel, &format!("group_ids={GROUP_ONE}"));
    let (go, rs) = common::fetch_both(&client, &token, &p).await;
    assert_eq!(go, rs, "{p}: {}", String::from_utf8_lossy(&rs));
    assert!(
        !rs.ends_with(b"\n"),
        "`json.Marshal` then `w.Write`, so no trailing newline"
    );

    let body: serde_json::Value = serde_json::from_slice(&go).expect("the body decodes");
    let users = body["users"].as_array().expect("a users array");
    let ids: Vec<&str> = users
        .iter()
        .map(|u| u["id"].as_str().expect("an id"))
        .collect();

    // 1. The member in the asked-for group is gone; 2. the members in other groups are not.
    assert!(
        !ids.contains(&f.users["chga"].as_str()),
        "chga is in GROUP_ONE"
    );
    assert!(
        ids.contains(&f.users["chgb"].as_str()),
        "chgb is in GROUP_TWO"
    );
    assert!(
        ids.contains(&f.users["chgc"].as_str()),
        "chgc is in two others"
    );
    // 3. A **deleted** group membership does not exclude: the subquery filters `DeleteAt = 0`.
    assert!(
        ids.contains(&f.users["chgd"].as_str()),
        "chgd's GROUP_ONE membership is deleted"
    );
    // 4. `Users.DeleteAt = 0` drops the deactivated member.
    assert!(
        !ids.contains(&f.users["chge"].as_str()),
        "chge is deactivated"
    );
    // 5. `Bots.UserId IS NULL` drops the bot — which is a `ChannelMembers` row like any other,
    //    and is in none of the groups, so nothing but that predicate keeps it out.
    assert!(
        !ids.contains(&f.users["bot"].as_str()),
        "a bot is a channel member here and must not be in the answer"
    );
    // 6. The member with no group row at all is present, with `groups: []` and not `null` —
    //    the app layer's `user.Groups = []*model.Group{}`.
    assert!(ids.contains(&f.admin.as_str()), "the admin is a member");
    let admin = users
        .iter()
        .find(|u| u["id"] == f.admin.as_str())
        .expect("the admin row");
    assert_eq!(admin["groups"], serde_json::json!([]), "empty, not null");

    // 7. `total_count` counts **users**, not joined rows: `chgc` is in two groups and contributes
    //    one. A `count(*)` in place of `count(DISTINCT Users.Id)` would answer one more than the
    //    page holds.
    assert_eq!(
        body["total_count"].as_u64().expect("a count"),
        ids.len() as u64,
        "one row per user, whatever their group count"
    );
}

/// The `groups` array is **every** group the member is in, not only the ones asked about — and it
/// ignores `GroupMembers.DeleteAt`, unlike the subquery three lines above it in the same SQL.
#[tokio::test]
async fn the_groups_array_is_hydrated_from_every_membership_row() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = group_fixture(&client, &token).await;

    let p = minus(&f.channel, &format!("group_ids={GROUP_ONE}"));
    let (go, rs) = common::fetch_both(&client, &token, &p).await;
    assert_eq!(go, rs, "{p}");
    let body: serde_json::Value = serde_json::from_slice(&go).expect("the body decodes");
    let users = body["users"].as_array().expect("a users array");

    let groups_of = |user_id: &str| -> Vec<String> {
        users
            .iter()
            .find(|u| u["id"] == user_id)
            .and_then(|u| u["groups"].as_array())
            .expect("a groups array")
            .iter()
            .map(|g| g["id"].as_str().expect("a group id").to_owned())
            .collect()
    };

    let mut chgc = groups_of(&f.users["chgc"]);
    chgc.sort();
    assert_eq!(
        chgc,
        vec![GROUP_TWO.to_owned(), GROUP_THREE.to_owned()],
        "two memberships, two groups"
    );

    assert_eq!(
        groups_of(&f.users["chgd"]),
        vec![GROUP_ONE.to_owned()],
        "the outer join has no `DeleteAt` filter, so a deleted membership still shows"
    );

    // And the hydrated group carries the stored columns, not an id-only stub.
    let group = users
        .iter()
        .find(|u| u["id"] == f.users["chgb"].as_str())
        .and_then(|u| u["groups"].as_array())
        .and_then(|g| g.first())
        .expect("chgb's group");
    assert_eq!(group["display_name"], "Display mmrs-chadmgrp-two");
    assert_eq!(group["source"], "custom");
    assert_eq!(group["allow_reference"], true);
    // The five `db:"-"` fields that `GetByIDs` does not compute.
    assert_eq!(group["has_syncables"], false);
    assert_eq!(group["member_ids"], serde_json::Value::Null);
    assert!(
        group.get("member_count").is_none(),
        "`omitempty` and not computed"
    );
}

/// Asking for two groups removes the members of both, and `total_count` follows the page.
#[tokio::test]
async fn a_second_group_id_removes_a_second_member() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = group_fixture(&client, &token).await;

    let p = minus(&f.channel, &format!("group_ids={GROUP_ONE},{GROUP_TWO}"));
    let (go, rs) = common::fetch_both(&client, &token, &p).await;
    assert_eq!(go, rs, "{p}: {}", String::from_utf8_lossy(&rs));

    let body: serde_json::Value = serde_json::from_slice(&go).expect("the body decodes");
    let ids: Vec<&str> = body["users"]
        .as_array()
        .expect("a users array")
        .iter()
        .map(|u| u["id"].as_str().expect("an id"))
        .collect();
    for tag in ["chga", "chgb", "chgc"] {
        assert!(!ids.contains(&f.users[tag].as_str()), "{tag} is excluded");
    }
    assert!(ids.contains(&f.users["chgd"].as_str()), "chgd is not");
    assert_eq!(
        body["total_count"].as_u64().expect("a count"),
        ids.len() as u64
    );
}

/// Paging is `LIMIT per_page OFFSET page * per_page`, and `total_count` is the **whole** set —
/// it does not shrink with the page. A port that counted the page would agree on page 0 and
/// diverge on every other one.
#[tokio::test]
async fn paging_slices_the_page_and_leaves_the_total_alone() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = group_fixture(&client, &token).await;

    let whole = minus(&f.channel, &format!("group_ids={GROUP_ONE}"));
    let (go_whole, rs_whole) = common::fetch_both(&client, &token, &whole).await;
    assert_eq!(go_whole, rs_whole, "{whole}");
    let whole: serde_json::Value = serde_json::from_slice(&go_whole).expect("the body decodes");
    let total = whole["total_count"].as_u64().expect("a count");
    assert!(total >= 3, "the fixture must have something to page");

    let mut seen = Vec::new();
    for page in 0..total {
        let p = minus(
            &f.channel,
            &format!("group_ids={GROUP_ONE}&page={page}&per_page=1"),
        );
        let (go, rs) = common::fetch_both(&client, &token, &p).await;
        assert_eq!(go, rs, "{p}: {}", String::from_utf8_lossy(&rs));
        let body: serde_json::Value = serde_json::from_slice(&go).expect("the body decodes");
        assert_eq!(
            body["total_count"].as_u64().expect("a count"),
            total,
            "page {page}: the total is the whole set"
        );
        let users = body["users"].as_array().expect("a users array");
        assert_eq!(users.len(), 1, "page {page}: one per page");
        seen.push(users[0]["id"].as_str().expect("an id").to_owned());
    }

    // Every page is a different user, and together they are the whole answer in username order.
    let expected: Vec<String> = whole["users"]
        .as_array()
        .expect("a users array")
        .iter()
        .map(|u| u["id"].as_str().expect("an id").to_owned())
        .collect();
    assert_eq!(
        seen, expected,
        "`ORDER BY Users.Username ASC`, one at a time"
    );

    // **A page size other than one.** With `per_page=1` the offset is `page * 1`, so a port that
    // dropped the multiplication entirely would agree on every page — the mutation
    // `page-offset-ignores-the-page-number` survived the loop above for exactly that reason.
    // At `per_page=2`, page 1 must start where page 0 stopped.
    let p = minus(
        &f.channel,
        &format!("group_ids={GROUP_ONE}&page=0&per_page=2"),
    );
    let (go, rs) = common::fetch_both(&client, &token, &p).await;
    assert_eq!(go, rs, "{p}: {}", String::from_utf8_lossy(&rs));
    let first: serde_json::Value = serde_json::from_slice(&go).expect("the body decodes");
    let p = minus(
        &f.channel,
        &format!("group_ids={GROUP_ONE}&page=1&per_page=2"),
    );
    let (go, rs) = common::fetch_both(&client, &token, &p).await;
    assert_eq!(go, rs, "{p}: {}", String::from_utf8_lossy(&rs));
    let second: serde_json::Value = serde_json::from_slice(&go).expect("the body decodes");

    let page_ids = |v: &serde_json::Value| -> Vec<String> {
        v["users"]
            .as_array()
            .expect("a users array")
            .iter()
            .map(|u| u["id"].as_str().expect("an id").to_owned())
            .collect()
    };
    let (a, b) = (page_ids(&first), page_ids(&second));
    assert_eq!(a, expected[..2], "page 0 of 2 is the first two");
    assert_eq!(a.len(), 2, "and it is two, not one");
    assert_eq!(
        b,
        expected[2..(4.min(expected.len()))],
        "page 1 of 2 starts at offset 2, not at offset 1"
    );

    // One page past the end is an empty list and the same total, not a 404.
    let p = minus(
        &f.channel,
        &format!("group_ids={GROUP_ONE}&page={total}&per_page=1"),
    );
    let (go, rs) = common::fetch_both(&client, &token, &p).await;
    assert_eq!(go, rs, "{p}");
    let body: serde_json::Value = serde_json::from_slice(&go).expect("the body decodes");
    assert_eq!(body["users"], serde_json::json!([]));
    assert_eq!(body["total_count"].as_u64().expect("a count"), total);
}

/// `ORDER BY Users.Username ASC` is asserted against the usernames the body itself carries, so it
/// cannot pass on a coincidence of insertion order.
#[tokio::test]
async fn the_page_is_ordered_by_username() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = group_fixture(&client, &token).await;

    let p = minus(&f.channel, &format!("group_ids={GROUP_ONE}"));
    let (go, rs) = common::fetch_both(&client, &token, &p).await;
    assert_eq!(go, rs, "{p}");
    let body: serde_json::Value = serde_json::from_slice(&go).expect("the body decodes");
    let names: Vec<&str> = body["users"]
        .as_array()
        .expect("a users array")
        .iter()
        .map(|u| u["username"].as_str().expect("a username"))
        .collect();
    let mut sorted = names.clone();
    sorted.sort();
    assert_eq!(names, sorted, "ascending, and by username and not by id");
    assert!(names.len() > 1, "one element is sorted by accident");
}

/// The two `group_ids` gates, over HTTP. Each row is a 400 naming `group_ids`, and the pairs that
/// look alike are the point: `a!b…` is stripped to `ab…` for the **length** test and split raw for
/// the **id** test, so the two strings differ and so do the clauses they fail.
#[tokio::test]
async fn the_group_ids_gates_answer_the_same_400_go_does() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = group_fixture(&client, &token).await;

    for query in [
        // No parameter at all.
        "".to_owned(),
        // Present and empty.
        "group_ids=".to_owned(),
        // Too short after the strip.
        "group_ids=abcdefghijklmnopqrstuvwxy".to_owned(),
        // Long enough only before the strip.
        format!("group_ids={}", "a!".repeat(20)),
        // Past the length gate, not an id.
        format!("group_ids={}", "a".repeat(30)),
        // Two ids of 13 characters each: the comma keeps the length gate happy.
        format!("group_ids={},{}", "a".repeat(13), "b".repeat(13)),
        // A trailing comma is an empty element, and "" is not a valid id.
        format!("group_ids={GROUP_ONE},"),
        // **The two gates run on two different strings.** Stripped, this is `GROUP_ONE` and a
        // valid id; raw, it is 27 characters and is not. Go splits the **raw** parameter, so it
        // is a 400 — and a port that validated the stripped copy would answer 200 here. The only
        // row in this list that separates the two, and the mutation
        // `group-ids-split-runs-on-the-stripped-string` survived until it existed.
        format!("group_ids={}!{}", &GROUP_ONE[..3], &GROUP_ONE[3..]),
    ] {
        let p = minus(&f.channel, &query);
        let ((go_status, go), (rs_status, rs)) = common::fetch_both_raw(&client, &token, &p).await;
        assert_eq!(go_status, 400, "{p}: {}", String::from_utf8_lossy(&go));
        assert_eq!(rs_status, go_status, "{p}");
        let parsed = assert_error_bodies_match_except_known_gaps(&go, &rs, &p);
        assert_eq!(
            parsed["id"], "api.context.invalid_body_param.app_error",
            "{p}"
        );
        assert_eq!(
            parsed["message"],
            "Invalid or missing group_ids in request body."
        );
    }
}

/// **The gates are in this order**: the channel id, then `group_ids`, then the permission — and
/// the channel is never fetched at all.
#[tokio::test]
async fn the_id_gate_precedes_the_group_gate_and_neither_reads_the_channel() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;

    // A malformed channel id with a malformed `group_ids`: the id error wins.
    let p = minus("abc", "group_ids=short");
    let ((go_status, go), (rs_status, rs)) = common::fetch_both_raw(&client, &token, &p).await;
    assert_eq!(go_status, 400, "{p}");
    assert_eq!(rs_status, go_status, "{p}");
    let parsed = assert_error_bodies_match_except_known_gaps(&go, &rs, &p);
    assert_eq!(
        parsed["id"], "api.context.invalid_url_param.app_error",
        "{p}"
    );

    // A well-formed channel id naming nothing: **200 with an empty page**, not a 404. Nothing on
    // this path calls `GetChannel` — the store's `Channels.Id = ?` simply matches no row.
    let p = minus(ABSENT, &format!("group_ids={GROUP_ONE}"));
    let (go, rs) = common::fetch_both(&client, &token, &p).await;
    assert_eq!(go, rs, "{p}: {}", String::from_utf8_lossy(&rs));
    let body: serde_json::Value = serde_json::from_slice(&go).expect("the body decodes");
    assert_eq!(body["users"], serde_json::json!([]));
    assert_eq!(body["total_count"], 0);
}

/// The permission is `sysconsole_read_user_management_channels`, and it is asked **after** both
/// `group_ids` gates — so a plain user with a bad `group_ids` gets the 400, not the 403.
#[tokio::test]
async fn a_plain_user_is_refused_but_only_after_the_group_gates() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = group_fixture(&client, &token).await;

    let p = minus(&f.channel, &format!("group_ids={GROUP_ONE}"));
    let ((go_status, go), (rs_status, rs)) =
        common::fetch_both_raw(&client, &f.plain_token, &p).await;
    assert_eq!(go_status, 403, "{p}: {}", String::from_utf8_lossy(&go));
    assert_eq!(rs_status, go_status, "{p}");
    let parsed = assert_error_bodies_match_except_known_gaps(&go, &rs, &p);
    assert_eq!(parsed["id"], "api.context.permissions.app_error", "{p}");

    let p = minus(&f.channel, "group_ids=short");
    let ((go_status, go), (rs_status, rs)) =
        common::fetch_both_raw(&client, &f.plain_token, &p).await;
    assert_eq!(go_status, 400, "{p}: the group gate runs first");
    assert_eq!(rs_status, go_status, "{p}");
    let parsed = assert_error_bodies_match_except_known_gaps(&go, &rs, &p);
    assert_eq!(
        parsed["id"], "api.context.invalid_body_param.app_error",
        "{p}"
    );
}

/// **`Channels.DeleteAt = 0`.** An archived channel keeps its `ChannelMembers` rows, and this
/// route still answers 200 — with an empty page, because the join to `Channels` filters the
/// channel itself out. Not a 404 and not the membership list: the predicate is on the *channel*
/// row, which is the one a reader is most likely to drop as redundant.
#[tokio::test]
async fn an_archived_channel_answers_an_empty_page() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = group_fixture(&client, &token).await;

    // Its own channel, in the fixture's team, so archiving it disturbs nothing else.
    let team = client
        .get(format!("{GO}/api/v4/channels/{}", f.channel))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("Go answers")
        .json::<serde_json::Value>()
        .await
        .expect("a channel")["team_id"]
        .as_str()
        .expect("a team id")
        .to_owned();
    let doomed = create_channel(&client, &token, &team, "chadmgrparch").await;
    let joined = client
        .post(format!("{GO}/api/v4/channels/{doomed}/members"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!({ "user_id": f.users["chgb"] }))
        .send()
        .await
        .expect("Go answers");
    assert!(
        joined.status().is_success(),
        "chgb joins the doomed channel"
    );

    // Live, it answers the two members.
    let p = minus(&doomed, &format!("group_ids={GROUP_ONE}"));
    let (go, rs) = common::fetch_both(&client, &token, &p).await;
    assert_eq!(go, rs, "{p}: {}", String::from_utf8_lossy(&rs));
    let body: serde_json::Value = serde_json::from_slice(&go).expect("the body decodes");
    assert_eq!(
        body["total_count"], 2,
        "the creator and chgb, before archiving"
    );

    let archived = client
        .delete(format!("{GO}/api/v4/channels/{doomed}"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("Go answers");
    assert!(archived.status().is_success(), "the channel is archived");

    let (go, rs) = common::fetch_both(&client, &token, &p).await;
    assert_eq!(go, rs, "{p}: {}", String::from_utf8_lossy(&rs));
    let body: serde_json::Value = serde_json::from_slice(&go).expect("the body decodes");
    assert_eq!(body["users"], serde_json::json!([]), "and after, nothing");
    assert_eq!(body["total_count"], 0);
}
