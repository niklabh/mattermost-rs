//! Cross-server parity for the six channel-membership writes.
//!
//! Every assertion about a write this server made reads back **through this server** ([D-190]):
//! Go's in-process caches do not see our rows and ours do not see Go's.
//!
//! # Each test gives the two servers their own channel
//!
//! These routes *mutate membership*, so a shared fixture channel would let one server's `PUT
//! …/members` remove the member the other server's test is about. Every test here creates two
//! channels from one template and points one server at each — which is also what makes a byte
//! comparison meaningful, since the only field that differs is `channel_id`.
//!
//! ```sh
//! docker compose up -d && scripts/parity.sh -p mm-api --test parity channel_member_write
//! ```

use std::time::Duration;

use crate::common;

use common::{
    GO, RUST, SocketProbe, a_team_and_channel_the_user_is_in, client, create_channel_typed,
    create_plain_user, delete_channel, go_minted_token, logged_in_user_id, stack_enabled,
};

/// The one field two otherwise-identical answers must differ in, replaced so the rest can be
/// compared byte for byte.
fn normalise(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (key, inner) in map {
                if key == "channel_id" {
                    out.insert(key.clone(), serde_json::json!("<channel>"));
                } else {
                    out.insert(key.clone(), normalise(inner));
                }
            }
            serde_json::Value::Object(out)
        }
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(normalise).collect())
        }
        other => other.clone(),
    }
}

/// One request against one server, returning `(status, body)` and asserting that a `RUST` answer
/// really came from this port rather than the proxy.
async fn call(
    http: &reqwest::Client,
    base: &str,
    method: reqwest::Method,
    path: &str,
    token: &str,
    body: Option<&serde_json::Value>,
) -> (u16, String) {
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
    if base == RUST {
        common::assert_served_by_rust(response.headers(), path);
    }
    (status, response.text().await.expect("a body"))
}

/// A raw request with an explicit body string, for the malformed bodies `serde_json` would refuse
/// to build.
async fn call_raw(
    http: &reqwest::Client,
    base: &str,
    method: reqwest::Method,
    path: &str,
    token: &str,
    body: &'static str,
) -> (u16, String) {
    let response = http
        .request(method, format!("{base}{path}"))
        .header("Authorization", format!("Bearer {token}"))
        .header("Content-Type", "application/json")
        .body(body)
        .send()
        .await
        .unwrap_or_else(|e| panic!("{base} unreachable: {e}"));
    let status = response.status().as_u16();
    if base == RUST {
        common::assert_served_by_rust(response.headers(), path);
    }
    (status, response.text().await.expect("a body"))
}

/// The same request against both servers, each against its own channel, with the two statuses and
/// the two normalised bodies compared. Returns the Rust body so a caller can go on asserting.
async fn both(
    http: &reqwest::Client,
    token: &str,
    go_path: &str,
    rust_path: &str,
    method: reqwest::Method,
    body: Option<&serde_json::Value>,
    what: &str,
) -> String {
    let (go_status, go_raw) = call(http, GO, method.clone(), go_path, token, body).await;
    let (rust_status, rust_raw) = call(http, RUST, method, rust_path, token, body).await;
    assert_eq!(
        rust_status, go_status,
        "{what}: the status differs\n go: {go_raw}\nrust: {rust_raw}"
    );
    assert_eq!(
        go_raw.ends_with('\n'),
        rust_raw.ends_with('\n'),
        "{what}: the body's framing differs\n go: {go_raw:?}\nrust: {rust_raw:?}"
    );

    if go_status >= 400 {
        common::assert_error_bodies_match_except_known_gaps(
            go_raw.trim().as_bytes(),
            rust_raw.trim().as_bytes(),
            what,
        );
    } else {
        let go_json: serde_json::Value = serde_json::from_str(go_raw.trim()).unwrap_or_default();
        let rust_json: serde_json::Value =
            serde_json::from_str(rust_raw.trim()).unwrap_or_default();
        assert_eq!(
            normalise(&go_json),
            normalise(&rust_json),
            "{what}: the body differs\n go: {go_raw}\nrust: {rust_raw}"
        );
    }
    rust_raw
}

/// [`both`] with named fields blanked on **both** sides before the comparison.
///
/// The only use is the pair of mention counters, which Go's deferred join/add system post moves and
/// this server's does not — D-231. Masking is preferred to skipping the comparison outright: every
/// other field on the row is still asserted byte for byte.
#[allow(clippy::too_many_arguments)] // `both` plus the mask; splitting it would hide the mask.
async fn both_masked(
    http: &reqwest::Client,
    token: &str,
    go_path: &str,
    rust_path: &str,
    method: reqwest::Method,
    body: Option<&serde_json::Value>,
    mask: &[&str],
    what: &str,
) -> String {
    let (go_status, go_raw) = call(http, GO, method.clone(), go_path, token, body).await;
    let (rust_status, rust_raw) = call(http, RUST, method, rust_path, token, body).await;
    assert_eq!(
        rust_status, go_status,
        "{what}: the status differs\n go: {go_raw}\nrust: {rust_raw}"
    );
    let blank = |raw: &str| -> serde_json::Value {
        let mut value: serde_json::Value =
            serde_json::from_str(raw.trim()).unwrap_or_else(|e| panic!("{what}: not JSON: {e}"));
        let members: Vec<&mut serde_json::Value> = match &mut value {
            serde_json::Value::Array(items) => items.iter_mut().collect(),
            other => vec![other],
        };
        for member in members {
            if let Some(object) = member.as_object_mut() {
                for key in mask {
                    object.insert((*key).to_owned(), serde_json::json!("<masked>"));
                }
            }
        }
        normalise(&value)
    };
    assert_eq!(
        blank(&go_raw),
        blank(&rust_raw),
        "{what}: the body differs beyond {mask:?}\n go: {go_raw}\nrust: {rust_raw}"
    );
    rust_raw
}

/// Two channels of one type, one per server, plus a plain user who is on the team and in neither.
struct Pair {
    team: String,
    go_channel: String,
    rust_channel: String,
    other_user: String,
}

async fn pair(http: &reqwest::Client, token: &str, tag: &str, channel_type: &str) -> Pair {
    let (team, _) = a_team_and_channel_the_user_is_in(http, token).await;
    let go_channel =
        create_channel_typed(http, token, &team, &format!("{tag}g"), channel_type).await;
    let rust_channel =
        create_channel_typed(http, token, &team, &format!("{tag}r"), channel_type).await;
    let other = create_plain_user(http, token, &team, tag).await;
    Pair {
        team,
        go_channel,
        rust_channel,
        other_user: other.id,
    }
}

async fn cleanup(http: &reqwest::Client, token: &str, fixture: &Pair) {
    delete_channel(http, token, &fixture.go_channel).await;
    delete_channel(http, token, &fixture.rust_channel).await;
    common::delete_plain_user(http, token, &fixture.other_user).await;
}

#[tokio::test]
async fn adding_a_member_agrees_and_the_body_shape_follows_the_user_id_key() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let fixture = pair(&http, &token, "cmwadd", "O").await;
    let user = &fixture.other_user;

    // `{"user_id": …}` ⇒ a bare object, `201`, encoder-framed.
    let rust_raw = both(
        &http,
        &token,
        &format!("/api/v4/channels/{}/members", fixture.go_channel),
        &format!("/api/v4/channels/{}/members", fixture.rust_channel),
        reqwest::Method::POST,
        Some(&serde_json::json!({"user_id": user})),
        "addChannelMember with user_id",
    )
    .await;
    let member: serde_json::Value = serde_json::from_str(rust_raw.trim()).expect("a member");
    assert!(
        member.is_object(),
        "a `user_id` body answers a bare object: {rust_raw}"
    );
    assert!(
        rust_raw.ends_with('\n'),
        "`json.NewEncoder(w).Encode` frames with a newline: {rust_raw:?}"
    );
    // `SanitizeForCurrentUser` blanks both timestamps to **-1**, not 0, for anybody but the
    // member — and the caller here is not the member.
    assert_eq!(member["last_viewed_at"], -1);
    assert_eq!(member["last_update_at"], -1);
    // `GetDefaultChannelNotifyProps` is six keys, and the member is invalid without `desktop`.
    assert_eq!(member["notify_props"]["desktop"], "default");
    assert_eq!(member["notify_props"]["mark_unread"], "all");
    assert_eq!(member["roles"], "channel_user");
    assert_eq!(member["scheme_user"], true);
    assert_eq!(member["scheme_admin"], false);

    // A second add of the same member is still a `201`, with the *stored* member — no write, no
    // event, no history row.
    //
    // **The mention counters cannot match here, and the reason is D-231.** Go's first add posted an
    // `add_to_channel` system message that @-mentions the added user, so by the time the stored
    // member is read back Go's `mention_count` is 1 and ours is 0. That is the deferred join/leave
    // post leaking into a *response body*, which is the one place it was expected not to. Everything
    // else on the row is compared.
    let (go_status, go_raw) = call(
        &http,
        GO,
        reqwest::Method::POST,
        &format!("/api/v4/channels/{}/members", fixture.go_channel),
        &token,
        Some(&serde_json::json!({"user_id": user})),
    )
    .await;
    let (rust_status, rust_raw) = call(
        &http,
        RUST,
        reqwest::Method::POST,
        &format!("/api/v4/channels/{}/members", fixture.rust_channel),
        &token,
        Some(&serde_json::json!({"user_id": user})),
    )
    .await;
    assert_eq!(go_status, 201, "a re-add is still Created: {go_raw}");
    assert_eq!(rust_status, go_status, "the re-add status differs");
    let mut go_again: serde_json::Value = serde_json::from_str(go_raw.trim()).expect("a member");
    let mut rust_again: serde_json::Value =
        serde_json::from_str(rust_raw.trim()).expect("a member");
    for value in [&mut go_again, &mut rust_again] {
        let object = value.as_object_mut().expect("a member object");
        for key in ["mention_count", "mention_count_root"] {
            object.insert(key.to_owned(), serde_json::json!("<D-231>"));
        }
    }
    assert_eq!(
        normalise(&go_again),
        normalise(&rust_again),
        "the re-add answer differs beyond the mention counters\n go: {go_raw}\nrust: {rust_raw}"
    );
    // And Go's counter really is the one D-231 predicts, so the exclusion above is not hiding
    // something else.
    let go_mentions: serde_json::Value = serde_json::from_str(go_raw.trim()).expect("a member");
    assert_eq!(
        go_mentions["mention_count"], 1,
        "Go's add-to-channel system post is what raises this: {go_raw}"
    );

    // `{"user_ids": [...]}` ⇒ an **array**, even for one id. Masked for the same D-231 reason as
    // the re-add above: this too returns the stored member.
    let rust_raw = both_masked(
        &http,
        &token,
        &format!("/api/v4/channels/{}/members", fixture.go_channel),
        &format!("/api/v4/channels/{}/members", fixture.rust_channel),
        reqwest::Method::POST,
        Some(&serde_json::json!({"user_ids": [user]})),
        &["mention_count", "mention_count_root"],
        "addChannelMember with user_ids",
    )
    .await;
    assert!(
        serde_json::from_str::<serde_json::Value>(rust_raw.trim())
            .expect("a list")
            .is_array(),
        "a `user_ids` body answers an array: {rust_raw}"
    );

    // **Both keys**: `user_ids` decides who is added, `user_id`'s mere presence decides the shape.
    let rust_raw = both_masked(
        &http,
        &token,
        &format!("/api/v4/channels/{}/members", fixture.go_channel),
        &format!("/api/v4/channels/{}/members", fixture.rust_channel),
        reqwest::Method::POST,
        Some(&serde_json::json!({"user_id": user, "user_ids": [user]})),
        &["mention_count", "mention_count_root"],
        "addChannelMember with both keys",
    )
    .await;
    assert!(
        serde_json::from_str::<serde_json::Value>(rust_raw.trim())
            .expect("a member")
            .is_object(),
        "both keys plus one member ⇒ an object: {rust_raw}"
    );

    // **The history row, which no response body shows.** Both servers must have written one open
    // stay for this membership; a port that dropped `LogJoinEvent` would pass every assertion above.
    for (base, channel) in [(GO, &fixture.go_channel), (RUST, &fixture.rust_channel)] {
        let Some(history) = common::channel_member_history(channel, user).await else {
            break;
        };
        assert_eq!(
            history.len(),
            1,
            "{base} wrote {} history rows for one join",
            history.len()
        );
        assert!(history[0].0 > 0, "{base}'s join time is unset");
        assert_eq!(
            history[0].1, None,
            "{base} closed the stay of a member who is still there"
        );
    }

    // Read back through the writing server — Go's caches cannot see our row.
    let (status, listed) = call(
        &http,
        RUST,
        reqwest::Method::GET,
        &format!("/api/v4/channels/{}/members", fixture.rust_channel),
        &token,
        None,
    )
    .await;
    assert_eq!(status, 200);
    assert!(
        listed.contains(user.as_str()),
        "our own listing does not have the member we added: {listed}"
    );

    cleanup(&http, &token, &fixture).await;
}

#[tokio::test]
async fn the_add_bodys_four_refusals_agree() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let fixture = pair(&http, &token, "cmwaddbad", "O").await;
    let go = format!("/api/v4/channels/{}/members", fixture.go_channel);
    let rust = format!("/api/v4/channels/{}/members", fixture.rust_channel);

    // A well-formed id nobody holds: **404** from `GetUser`, not a 400.
    both(
        &http,
        &token,
        &go,
        &rust,
        reqwest::Method::POST,
        Some(&serde_json::json!({"user_id": "aaaaaaaaaaaaaaaaaaaaaaaaaa"})),
        "a user id that matches nothing",
    )
    .await;

    // A malformed id — `invalid_body_param` naming `user_id or user_ids`.
    both(
        &http,
        &token,
        &go,
        &rust,
        reqwest::Method::POST,
        Some(&serde_json::json!({"user_id": "short"})),
        "a malformed user_id",
    )
    .await;

    // A `user_ids` that is not an array falls through to the single-id shape, so it reports the
    // *other* parameter name. Invisible in the body (params are not on the wire) and asserted in
    // `channel_member_writes`'s unit tests; here it is the status and id that must agree.
    both(
        &http,
        &token,
        &go,
        &rust,
        reqwest::Method::POST,
        Some(&serde_json::json!({"user_ids": "notanarray"})),
        "a user_ids that is not an array",
    )
    .await;

    // A malformed id inside the array.
    both(
        &http,
        &token,
        &go,
        &rust,
        reqwest::Method::POST,
        Some(&serde_json::json!({"user_ids": ["short"]})),
        "a malformed id inside user_ids",
    )
    .await;

    // An empty body — `StringInterfaceFromJSON` gives `{}`, so neither key is present.
    both(
        &http,
        &token,
        &go,
        &rust,
        reqwest::Method::POST,
        Some(&serde_json::json!({})),
        "an empty add body",
    )
    .await;

    // An empty `user_ids` array **is** an array, so it takes that branch and adds nobody: `201`
    // with `null`, because the member slice is never appended to.
    let rust_raw = both(
        &http,
        &token,
        &go,
        &rust,
        reqwest::Method::POST,
        Some(&serde_json::json!({"user_ids": []})),
        "an empty user_ids array",
    )
    .await;
    assert_eq!(
        rust_raw.trim(),
        "null",
        "an empty list adds nobody and encodes the nil slice: {rust_raw:?}"
    );

    cleanup(&http, &token, &fixture).await;
}

#[tokio::test]
async fn a_direct_channel_refuses_both_the_add_and_the_remove_by_type() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let (team, _) = a_team_and_channel_the_user_is_in(&http, &token).await;
    let other = create_plain_user(&http, &token, &team, "cmwdm").await;
    let me = logged_in_user_id();
    // One DM channel: it is the *type* that is refused, so both servers can share it — neither
    // reaches a write.
    let dm = common::create_direct_channel(&http, &token, me, &other.id).await;

    let path = format!("/api/v4/channels/{dm}/members");
    both(
        &http,
        &token,
        &path,
        &path,
        reqwest::Method::POST,
        Some(&serde_json::json!({"user_id": other.id})),
        "adding to a DM",
    )
    .await;

    let path = format!("/api/v4/channels/{dm}/members/{me}");
    both(
        &http,
        &token,
        &path,
        &path,
        reqwest::Method::DELETE,
        None,
        "removing from a DM",
    )
    .await;

    let path = format!("/api/v4/channels/{dm}/members");
    both(
        &http,
        &token,
        &path,
        &path,
        reqwest::Method::PUT,
        Some(&serde_json::json!({"members": [me]})),
        "setting a DM's members",
    )
    .await;

    common::delete_plain_user(&http, &token, &other.id).await;
}

#[tokio::test]
async fn removing_a_member_agrees_and_the_second_removal_is_a_404() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let fixture = pair(&http, &token, "cmwrm", "O").await;
    let user = &fixture.other_user;

    for (base, channel) in [(GO, &fixture.go_channel), (RUST, &fixture.rust_channel)] {
        let (status, body) = call(
            &http,
            base,
            reqwest::Method::POST,
            &format!("/api/v4/channels/{channel}/members"),
            &token,
            Some(&serde_json::json!({"user_id": user})),
        )
        .await;
        assert_eq!(status, 201, "{base} seeded the member: {body}");
    }

    let go = format!("/api/v4/channels/{}/members/{user}", fixture.go_channel);
    let rust = format!("/api/v4/channels/{}/members/{user}", fixture.rust_channel);

    both(
        &http,
        &token,
        &go,
        &rust,
        reqwest::Method::DELETE,
        None,
        "removeChannelMember",
    )
    .await;

    // **Not idempotent**, unlike `deleteDraft`: the member lookup inside
    // `removeUserFromChannel` 404s.
    both(
        &http,
        &token,
        &go,
        &rust,
        reqwest::Method::DELETE,
        None,
        "removeChannelMember twice",
    )
    .await;

    // **The leave row.** The stay opened by the add is closed, and it is the same row — not a
    // second one — so `LogLeaveEvent`'s `LeaveTime IS NULL` predicate did its job.
    for (base, channel) in [(GO, &fixture.go_channel), (RUST, &fixture.rust_channel)] {
        let Some(history) = common::channel_member_history(channel, user).await else {
            break;
        };
        assert_eq!(
            history.len(),
            1,
            "{base} wrote a second history row on leaving"
        );
        assert!(
            history[0].1.is_some_and(|leave| leave >= history[0].0),
            "{base} left the stay open after a removal: {history:?}"
        );
    }

    // The row is gone as far as the writing server is concerned.
    let (status, _) = call(
        &http,
        RUST,
        reqwest::Method::GET,
        &format!("/api/v4/channels/{}/members/{user}", fixture.rust_channel),
        &token,
        None,
    )
    .await;
    assert_eq!(status, 404, "our own read still finds the removed member");

    // A member id that is not an id at all is a **400** before any lookup.
    let go = format!("/api/v4/channels/{}/members/short", fixture.go_channel);
    let rust = format!("/api/v4/channels/{}/members/short", fixture.rust_channel);
    both(
        &http,
        &token,
        &go,
        &rust,
        reqwest::Method::DELETE,
        None,
        "removeChannelMember with a malformed user id",
    )
    .await;

    cleanup(&http, &token, &fixture).await;
}

#[tokio::test]
async fn town_square_refuses_the_removal_for_a_non_guest() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let (team, _) = a_team_and_channel_the_user_is_in(&http, &token).await;
    let me = logged_in_user_id();

    // The default channel is `town-square` and it exists per team; both servers can be pointed at
    // the same one, because the refusal happens before any write.
    let (status, body) = call(
        &http,
        RUST,
        reqwest::Method::GET,
        &format!("/api/v4/teams/{team}/channels/name/town-square"),
        &token,
        None,
    )
    .await;
    assert_eq!(status, 200, "the default channel is there: {body}");
    let town_square: serde_json::Value = serde_json::from_str(&body).expect("a channel");
    let town_square = town_square["id"].as_str().expect("an id");

    let path = format!("/api/v4/channels/{town_square}/members/{me}");
    both(
        &http,
        &token,
        &path,
        &path,
        reqwest::Method::DELETE,
        None,
        "leaving town-square",
    )
    .await;

    // And the caller is still a member on both.
    for base in [GO, RUST] {
        let (status, _) = call(&http, base, reqwest::Method::GET, &path, &token, None).await;
        assert_eq!(status, 200, "{base} removed the member anyway");
    }
}

#[tokio::test]
async fn the_roles_update_agrees_across_every_refusal() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let fixture = pair(&http, &token, "cmwroles", "O").await;
    let user = &fixture.other_user;

    for (base, channel) in [(GO, &fixture.go_channel), (RUST, &fixture.rust_channel)] {
        let (status, body) = call(
            &http,
            base,
            reqwest::Method::POST,
            &format!("/api/v4/channels/{channel}/members"),
            &token,
            Some(&serde_json::json!({"user_id": user})),
        )
        .await;
        assert_eq!(status, 201, "{base} seeded the member: {body}");
    }

    let go = format!(
        "/api/v4/channels/{}/members/{user}/roles",
        fixture.go_channel
    );
    let rust = format!(
        "/api/v4/channels/{}/members/{user}/roles",
        fixture.rust_channel
    );

    // The success: `{"status":"OK"}`, encoder-framed.
    let rust_raw = both(
        &http,
        &token,
        &go,
        &rust,
        reqwest::Method::PUT,
        Some(&serde_json::json!({"roles": "channel_user channel_admin"})),
        "granting channel_admin",
    )
    .await;
    // `ReturnStatusOK` is `w.Write(MapToJSON(...))` — **no** trailing newline, unlike
    // `addChannelMember`'s encoder-framed body.
    assert_eq!(rust_raw, r#"{"status":"OK"}"#);

    // And it landed, read back through our own server.
    let (_, body) = call(
        &http,
        RUST,
        reqwest::Method::GET,
        &format!("/api/v4/channels/{}/members/{user}", fixture.rust_channel),
        &token,
        None,
    )
    .await;
    let member: serde_json::Value = serde_json::from_str(&body).expect("a member");
    assert_eq!(
        member["scheme_admin"], true,
        "the grant did not land: {body}"
    );
    assert_eq!(
        member["roles"], "channel_user channel_admin",
        "the effective roles are rebuilt from the flags: {body}"
    );

    // A **system** role is refused by `IsValidChannelMemberRoles` — before the permission check,
    // so this is a 400 for an admin and a 400 for anybody.
    both(
        &http,
        &token,
        &go,
        &rust,
        reqwest::Method::PUT,
        Some(&serde_json::json!({"roles": "system_admin"})),
        "a system role",
    )
    .await;

    // A role name that does not exist: `GetRoleByName`'s 404 id, forced to a **400**.
    both(
        &http,
        &token,
        &go,
        &rust,
        reqwest::Method::PUT,
        Some(&serde_json::json!({"roles": "mmrs_no_such_role"})),
        "a role that does not exist",
    )
    .await;

    // No `roles` key at all: valid at the handler, refused four layers down for having no base
    // scheme role.
    both(
        &http,
        &token,
        &go,
        &rust,
        reqwest::Method::PUT,
        Some(&serde_json::json!({})),
        "an empty roles body",
    )
    .await;

    // `channel_admin` alone: same refusal, because the flags are *set*, not patched.
    both(
        &http,
        &token,
        &go,
        &rust,
        reqwest::Method::PUT,
        Some(&serde_json::json!({"roles": "channel_admin"})),
        "channel_admin without channel_user",
    )
    .await;

    // Promoting to guest: `changing_guest_role`, which is what stops this route being a
    // guest-management API.
    both(
        &http,
        &token,
        &go,
        &rust,
        reqwest::Method::PUT,
        Some(&serde_json::json!({"roles": "channel_guest"})),
        "making a member a guest",
    )
    .await;

    // `MapFromJSON` swallows a non-string value, so this is an *empty map* — the same answer as
    // an empty body, not a 400.
    let (go_status, go_raw) = call_raw(
        &http,
        GO,
        reqwest::Method::PUT,
        &go,
        &token,
        "{\"roles\": 5}",
    )
    .await;
    let (rust_status, rust_raw) = call_raw(
        &http,
        RUST,
        reqwest::Method::PUT,
        &rust,
        &token,
        "{\"roles\": 5}",
    )
    .await;
    assert_eq!(go_status, 400, "Go refuses it downstream: {go_raw}");
    assert_eq!(rust_status, go_status, "a non-string roles value differs");
    let go_json: serde_json::Value = serde_json::from_str(&go_raw).expect("an error");
    assert_eq!(
        go_json["id"], "api.channel.update_channel_member_roles.unset_user_scheme.app_error",
        "the value was dropped, not rejected: {go_raw}"
    );
    common::assert_error_bodies_match_except_known_gaps(
        go_raw.as_bytes(),
        rust_raw.as_bytes(),
        "a non-string roles value",
    );

    cleanup(&http, &token, &fixture).await;
}

#[tokio::test]
async fn the_scheme_roles_update_agrees_and_takes_only_three_bodies() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let fixture = pair(&http, &token, "cmwscheme", "O").await;
    let user = &fixture.other_user;

    for (base, channel) in [(GO, &fixture.go_channel), (RUST, &fixture.rust_channel)] {
        let (status, body) = call(
            &http,
            base,
            reqwest::Method::POST,
            &format!("/api/v4/channels/{channel}/members"),
            &token,
            Some(&serde_json::json!({"user_id": user})),
        )
        .await;
        assert_eq!(status, 201, "{base} seeded the member: {body}");
    }

    let go = format!(
        "/api/v4/channels/{}/members/{user}/schemeRoles",
        fixture.go_channel
    );
    let rust = format!(
        "/api/v4/channels/{}/members/{user}/schemeRoles",
        fixture.rust_channel
    );

    both(
        &http,
        &token,
        &go,
        &rust,
        reqwest::Method::PUT,
        Some(&serde_json::json!({"scheme_user": true, "scheme_admin": true})),
        "scheme admin on",
    )
    .await;

    let (_, body) = call(
        &http,
        RUST,
        reqwest::Method::GET,
        &format!("/api/v4/channels/{}/members/{user}", fixture.rust_channel),
        &token,
        None,
    )
    .await;
    let member: serde_json::Value = serde_json::from_str(&body).expect("a member");
    assert_eq!(member["scheme_admin"], true, "the promotion did not land");
    assert_eq!(member["scheme_user"], true);

    both(
        &http,
        &token,
        &go,
        &rust,
        reqwest::Method::PUT,
        Some(&serde_json::json!({"scheme_user": true, "scheme_admin": false})),
        "scheme admin off",
    )
    .await;

    // `{}` decodes cleanly to three `false`s and is then refused for `scheme_user: false`.
    both(
        &http,
        &token,
        &go,
        &rust,
        reqwest::Method::PUT,
        Some(&serde_json::json!({})),
        "an empty schemeRoles body",
    )
    .await;

    // `scheme_guest: true` is refused outright — this route cannot make a guest.
    both(
        &http,
        &token,
        &go,
        &rust,
        reqwest::Method::PUT,
        Some(&serde_json::json!({"scheme_user": true, "scheme_guest": true})),
        "asking for guest",
    )
    .await;

    // Unlike `/roles`, this route **does** 400 on a body that will not decode.
    let (go_status, go_raw) = call_raw(&http, GO, reqwest::Method::PUT, &go, &token, "[").await;
    let (rust_status, rust_raw) =
        call_raw(&http, RUST, reqwest::Method::PUT, &rust, &token, "[").await;
    assert_eq!(go_status, 400, "Go refuses a broken body here: {go_raw}");
    assert_eq!(rust_status, go_status);
    common::assert_error_bodies_match_except_known_gaps(
        go_raw.as_bytes(),
        rust_raw.as_bytes(),
        "a broken schemeRoles body",
    );

    // The camelCase path is the only spelling either router matches; the lower-case one is
    // forwarded, and Go 404s it. Asserted through Go's own answer via the proxy, so this is a
    // router test rather than a handler one.
    let lower = format!(
        "/api/v4/channels/{}/members/{user}/schemeroles",
        fixture.rust_channel
    );
    // No `assert_served_by_rust` here: being **forwarded** is the assertion, and the guard would
    // fail on exactly the answer this test wants.
    let response = http
        .put(format!("{RUST}{lower}"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!({"scheme_user": true}))
        .send()
        .await
        .expect("our server answers");
    assert_eq!(
        response
            .headers()
            .get("x-mmrs-served-by")
            .and_then(|v| v.to_str().ok()),
        Some("go"),
        "the lower-case spelling must not reach the handler"
    );
    assert_eq!(response.status().as_u16(), 404, "and Go 404s it");

    cleanup(&http, &token, &fixture).await;
}

#[tokio::test]
async fn notify_props_merges_the_known_keys_and_validates_none_of_them() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let fixture = pair(&http, &token, "cmwnotify", "O").await;
    let me = logged_in_user_id();

    let go = format!(
        "/api/v4/channels/{}/members/{me}/notify_props",
        fixture.go_channel
    );
    let rust = format!(
        "/api/v4/channels/{}/members/{me}/notify_props",
        fixture.rust_channel
    );

    // **A value no validator would accept, and a key that is not in the filter.** Go stores the
    // first and drops the second, with a 200.
    both(
        &http,
        &token,
        &go,
        &rust,
        reqwest::Method::PUT,
        Some(&serde_json::json!({"desktop": "banana", "mmrs_bogus": "x"})),
        "an invalid value and an unknown key",
    )
    .await;

    for (base, channel) in [(GO, &fixture.go_channel), (RUST, &fixture.rust_channel)] {
        let (_, body) = call(
            &http,
            base,
            reqwest::Method::GET,
            &format!("/api/v4/channels/{channel}/members/{me}"),
            &token,
            None,
        )
        .await;
        let member: serde_json::Value = serde_json::from_str(&body).expect("a member");
        assert_eq!(
            member["notify_props"]["desktop"], "banana",
            "{base} did not store the invalid value: {body}"
        );
        // The **merge**: the five keys the caller did not name survive.
        assert_eq!(
            member["notify_props"]["mark_unread"], "all",
            "{base} replaced the props instead of merging: {body}"
        );
        assert_eq!(
            member["notify_props"]["push"], "default",
            "{base} lost `push`: {body}"
        );
        assert!(
            member["notify_props"]["mmrs_bogus"].is_null(),
            "{base} stored a key outside the filter: {body}"
        );
    }

    // Put `desktop` back, so the row is valid for the tests that call `UpdateMember` — which
    // *does* run `IsValid` and would refuse a member holding `banana`.
    for (base, path) in [(GO, &go), (RUST, &rust)] {
        let (status, _) = call(
            &http,
            base,
            reqwest::Method::PUT,
            path,
            &token,
            Some(&serde_json::json!({"desktop": "default"})),
        )
        .await;
        assert_eq!(status, 200, "{base} would not reset the prop");
    }

    // A body that is not an object at all: `MapFromJSON` gives `{}` and the answer is a **200**,
    // not the 400 the dead `props == nil` branch suggests.
    let (go_status, go_raw) = call_raw(&http, GO, reqwest::Method::PUT, &go, &token, "[]").await;
    let (rust_status, rust_raw) =
        call_raw(&http, RUST, reqwest::Method::PUT, &rust, &token, "[]").await;
    assert_eq!(go_status, 200, "Go accepts a non-object body: {go_raw}");
    assert_eq!(rust_status, go_status);
    assert_eq!(rust_raw, go_raw, "the empty-merge answer differs");

    // Somebody else's props need `edit_other_users`; the caller here is an admin, so this is the
    // *granted* side of the check — the refusal is covered by the plain-user test below.
    let go_other = format!(
        "/api/v4/channels/{}/members/{}/notify_props",
        fixture.go_channel, fixture.other_user
    );
    let rust_other = format!(
        "/api/v4/channels/{}/members/{}/notify_props",
        fixture.rust_channel, fixture.other_user
    );
    // Not a member of either channel: the permission passes and the **store's re-select 404s**.
    both(
        &http,
        &token,
        &go_other,
        &rust_other,
        reqwest::Method::PUT,
        Some(&serde_json::json!({"desktop": "all"})),
        "somebody else's props in a channel they are not in",
    )
    .await;

    cleanup(&http, &token, &fixture).await;
}

#[tokio::test]
async fn a_plain_user_is_refused_the_three_privileged_writes() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    // A **private** channel: a public one lets `read_public_channel` on the team stand in for
    // membership, so a "non-member" is served rather than refused.
    let fixture = pair(&http, &token, "cmwperm", "P").await;
    let plain = create_plain_user(&http, &token, &fixture.team, "cmwperm2").await;
    let me = logged_in_user_id();

    // `manage_channel_roles` on the channel.
    both(
        &http,
        &plain.token,
        &format!("/api/v4/channels/{}/members/{me}/roles", fixture.go_channel),
        &format!(
            "/api/v4/channels/{}/members/{me}/roles",
            fixture.rust_channel
        ),
        reqwest::Method::PUT,
        Some(&serde_json::json!({"roles": "channel_user"})),
        "a plain user updating roles",
    )
    .await;

    both(
        &http,
        &plain.token,
        &format!(
            "/api/v4/channels/{}/members/{me}/schemeRoles",
            fixture.go_channel
        ),
        &format!(
            "/api/v4/channels/{}/members/{me}/schemeRoles",
            fixture.rust_channel
        ),
        reqwest::Method::PUT,
        Some(&serde_json::json!({"scheme_user": true})),
        "a plain user updating scheme roles",
    )
    .await;

    // `edit_other_users` — a *user* permission, not a channel one, so the channel type does not
    // matter here.
    both(
        &http,
        &plain.token,
        &format!(
            "/api/v4/channels/{}/members/{me}/notify_props",
            fixture.go_channel
        ),
        &format!(
            "/api/v4/channels/{}/members/{me}/notify_props",
            fixture.rust_channel
        ),
        reqwest::Method::PUT,
        Some(&serde_json::json!({"desktop": "all"})),
        "a plain user updating somebody else's notify props",
    )
    .await;

    // `manage_private_channel_members` for the add, and
    // `manage_private_channel_members`/`manage_public_channel_members` for the remove.
    both(
        &http,
        &plain.token,
        &format!("/api/v4/channels/{}/members", fixture.go_channel),
        &format!("/api/v4/channels/{}/members", fixture.rust_channel),
        reqwest::Method::POST,
        Some(&serde_json::json!({"user_id": plain.id})),
        "a plain user adding themselves to a private channel",
    )
    .await;

    both(
        &http,
        &plain.token,
        &format!("/api/v4/channels/{}/members/{me}", fixture.go_channel),
        &format!("/api/v4/channels/{}/members/{me}", fixture.rust_channel),
        reqwest::Method::DELETE,
        None,
        "a plain user removing somebody else",
    )
    .await;

    // `manage_system` for the bulk set.
    both(
        &http,
        &plain.token,
        &format!("/api/v4/channels/{}/members", fixture.go_channel),
        &format!("/api/v4/channels/{}/members", fixture.rust_channel),
        reqwest::Method::PUT,
        Some(&serde_json::json!({"members": [me]})),
        "a plain user setting the whole membership",
    )
    .await;

    common::delete_plain_user(&http, &token, &plain.id).await;
    cleanup(&http, &token, &fixture).await;
}

#[tokio::test]
async fn set_channel_members_reconciles_and_frames_ndjson() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let fixture = pair(&http, &token, "cmwset", "O").await;
    let me = logged_in_user_id();
    let user = &fixture.other_user;

    let go = format!(
        "/api/v4/channels/{}/members?batch_delay_ms=0",
        fixture.go_channel
    );
    let rust = format!(
        "/api/v4/channels/{}/members?batch_delay_ms=0",
        fixture.rust_channel
    );

    // The creator is already a member and already a channel admin, so `{me, user}` with no
    // `channel_admins` adds one and touches nothing else.
    let rust_raw = both(
        &http,
        &token,
        &go,
        &rust,
        reqwest::Method::PUT,
        Some(&serde_json::json!({"members": [me, user]})),
        "adding one member",
    )
    .await;
    assert!(
        rust_raw.ends_with('\n'),
        "every NDJSON line is newline-terminated: {rust_raw:?}"
    );
    let line: serde_json::Value = serde_json::from_str(rust_raw.trim()).expect("one line");
    // `added` and `removed` are forced from nil to `[]` in the callback, so both keys are always
    // present as arrays; the other three are `omitempty`.
    assert_eq!(line["added"], serde_json::json!([user]));
    assert_eq!(line["removed"], serde_json::json!([]));
    assert!(line.get("promoted").is_none(), "promoted is omitempty");
    assert!(line.get("errors").is_none(), "errors is omitempty");

    // The **no-op**: still exactly one line, and it is the empty response.
    let rust_raw = both(
        &http,
        &token,
        &go,
        &rust,
        reqwest::Method::PUT,
        Some(&serde_json::json!({"members": [me, user]})),
        "a no-op reconcile",
    )
    .await;
    assert_eq!(
        rust_raw, "{\"added\":[],\"removed\":[]}\n",
        "a no-op must still emit one line"
    );

    // A removal plus a demotion, which is **two** lines — the phases are separate batches.
    let rust_raw = both(
        &http,
        &token,
        &go,
        &rust,
        reqwest::Method::PUT,
        Some(&serde_json::json!({"members": [me], "channel_admins": []})),
        "removing one and demoting the admin",
    )
    .await;
    let lines: Vec<&str> = rust_raw.trim().lines().collect();
    assert_eq!(
        lines.len(),
        2,
        "removals and demotions are separate lines: {rust_raw:?}"
    );
    let removal: serde_json::Value = serde_json::from_str(lines[0]).expect("a line");
    assert_eq!(removal["removed"], serde_json::json!([user]));
    let demotion: serde_json::Value = serde_json::from_str(lines[1]).expect("a line");
    assert_eq!(demotion["demoted"], serde_json::json!([me]));

    // And the promotion back.
    let rust_raw = both(
        &http,
        &token,
        &go,
        &rust,
        reqwest::Method::PUT,
        Some(&serde_json::json!({"members": [me], "channel_admins": [me]})),
        "promoting",
    )
    .await;
    let promotion: serde_json::Value = serde_json::from_str(rust_raw.trim()).expect("one line");
    assert_eq!(promotion["promoted"], serde_json::json!([me]));

    // An id nobody holds becomes an **error line**, not a failed request — and the line carries
    // the *unwiped* detail, because it never goes through `handleContextError`.
    //
    // **`errors[].error` cannot match, and it is the i18n gap (D-092), not the route.** Go's
    // `appErr.Error()` is `where: <translated message>, <detailed_error>`; this server has no
    // message catalogue, so it writes `where: <id>`. `user_id` and `id` — the two fields a client
    // can act on — are compared, and the string is only asserted to name the failing function.
    let body = serde_json::json!({
        "members": [me, "aaaaaaaaaaaaaaaaaaaaaaaaaa"], "channel_admins": [me],
    });
    let (go_status, go_raw) = call(&http, GO, reqwest::Method::PUT, &go, &token, Some(&body)).await;
    let (rust_status, rust_raw) = call(
        &http,
        RUST,
        reqwest::Method::PUT,
        &rust,
        &token,
        Some(&body),
    )
    .await;
    assert_eq!(
        go_status, 200,
        "an unknown id is a line, not a failure: {go_raw}"
    );
    assert_eq!(rust_status, go_status);
    let go_line: serde_json::Value =
        serde_json::from_str(go_raw.trim().lines().next().expect("a line")).expect("a line");
    let rust_line: serde_json::Value =
        serde_json::from_str(rust_raw.trim().lines().next().expect("a line")).expect("a line");
    assert_eq!(
        go_line["errors"][0]["user_id"], rust_line["errors"][0]["user_id"],
        "the error line names a different user"
    );
    assert_eq!(
        go_line["errors"][0]["id"], rust_line["errors"][0]["id"],
        "the error line carries a different id"
    );
    assert_eq!(
        rust_line["errors"][0]["id"],
        "app.user.missing_account.const"
    );
    assert!(
        rust_line["errors"][0]["error"]
            .as_str()
            .is_some_and(|s| s.starts_with("GetUser:")),
        "the error string names the failing function: {rust_raw}"
    );
    assert_eq!(
        go_line["added"], rust_line["added"],
        "the added list differs on a partly-failing batch"
    );
    assert_eq!(go_line["removed"], rust_line["removed"]);

    cleanup(&http, &token, &fixture).await;
}

#[tokio::test]
async fn set_channel_members_refusals_agree() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let fixture = pair(&http, &token, "cmwsetbad", "O").await;
    let private = pair(&http, &token, "cmwsetpriv", "P").await;
    let me = logged_in_user_id();

    let go = format!("/api/v4/channels/{}/members", fixture.go_channel);
    let rust = format!("/api/v4/channels/{}/members", fixture.rust_channel);

    // No `members` key: `null` is not an empty list, and the `nil` check is a 400.
    both(
        &http,
        &token,
        &go,
        &rust,
        reqwest::Method::PUT,
        Some(&serde_json::json!({})),
        "no members key",
    )
    .await;

    // An explicit `null` is the same thing.
    both(
        &http,
        &token,
        &go,
        &rust,
        reqwest::Method::PUT,
        Some(&serde_json::json!({"members": serde_json::Value::Null})),
        "an explicitly null members",
    )
    .await;

    // A malformed id in either list.
    both(
        &http,
        &token,
        &go,
        &rust,
        reqwest::Method::PUT,
        Some(&serde_json::json!({"members": ["short"]})),
        "a malformed member id",
    )
    .await;
    both(
        &http,
        &token,
        &go,
        &rust,
        reqwest::Method::PUT,
        Some(&serde_json::json!({"members": [me], "channel_admins": ["short"]})),
        "a malformed admin id",
    )
    .await;

    // The two query bounds, both **inclusive**, and an empty value that means "default".
    for (query, what) in [
        ("batch_size=0", "batch_size below the minimum"),
        ("batch_size=1001", "batch_size above the maximum"),
        ("batch_size=x", "a non-numeric batch_size"),
        ("batch_delay_ms=10001", "batch_delay_ms above the maximum"),
        ("batch_delay_ms=-1", "a negative batch_delay_ms"),
    ] {
        both(
            &http,
            &token,
            &format!("{go}?{query}"),
            &format!("{rust}?{query}"),
            reqwest::Method::PUT,
            Some(&serde_json::json!({"members": [me]})),
            what,
        )
        .await;
    }

    // An **empty private channel** is refused, and the guard reads the submitted list rather than
    // the result.
    both(
        &http,
        &token,
        &format!(
            "/api/v4/channels/{}/members?batch_delay_ms=0",
            private.go_channel
        ),
        &format!(
            "/api/v4/channels/{}/members?batch_delay_ms=0",
            private.rust_channel
        ),
        reqwest::Method::PUT,
        Some(&serde_json::json!({"members": []})),
        "emptying a private channel",
    )
    .await;

    // An empty **public** channel is allowed, which is the other half of that guard.
    both(
        &http,
        &token,
        &format!("{go}?batch_delay_ms=0"),
        &format!("{rust}?batch_delay_ms=0"),
        reqwest::Method::PUT,
        Some(&serde_json::json!({"members": []})),
        "emptying a public channel",
    )
    .await;

    cleanup(&http, &token, &fixture).await;
    cleanup(&http, &token, &private).await;
}

#[tokio::test]
async fn a_board_and_a_space_channel_are_refused_on_the_three_put_routes() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let (team, _) = a_team_and_channel_the_user_is_in(&http, &token).await;
    let me = logged_in_user_id();

    let Some(board) = common::plant_channel_of_type(&team, "BO", "cmwboard").await else {
        return;
    };
    let Some(space) = common::plant_channel_of_type(&team, "S", "cmwspace").await else {
        return;
    };

    for (channel, what) in [(&board, "a board"), (&space, "a space")] {
        for (suffix, body) in [
            ("roles", serde_json::json!({"roles": "channel_user"})),
            ("schemeRoles", serde_json::json!({"scheme_user": true})),
            ("notify_props", serde_json::json!({"desktop": "all"})),
        ] {
            let path = format!("/api/v4/channels/{channel}/members/{me}/{suffix}");
            both(
                &http,
                &token,
                &path,
                &path,
                reqwest::Method::PUT,
                Some(&body),
                &format!("{what} channel on /{suffix}"),
            )
            .await;
        }

        // And the routes **without** the guards answer something else entirely — a 404 from
        // `GetChannel`, whose query filters both types out.
        let path = format!("/api/v4/channels/{channel}/members");
        both(
            &http,
            &token,
            &path,
            &path,
            reqwest::Method::POST,
            Some(&serde_json::json!({"user_id": me})),
            &format!("{what} channel on the add route"),
        )
        .await;
    }
}

#[tokio::test]
async fn the_membership_writes_publish_the_events_go_publishes() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let fixture = pair(&http, &token, "cmwevents", "O").await;
    let user = &fixture.other_user;
    let me = logged_in_user_id();

    let mut go_socket = SocketProbe::connect(GO, &token).await;
    let mut rust_socket = SocketProbe::connect(RUST, &token).await;

    // --- the add: **two** `user_added` events, one per broadcast target ---
    for (base, channel) in [(GO, &fixture.go_channel), (RUST, &fixture.rust_channel)] {
        let (status, body) = call(
            &http,
            base,
            reqwest::Method::POST,
            &format!("/api/v4/channels/{channel}/members"),
            &token,
            Some(&serde_json::json!({"user_id": user})),
        )
        .await;
        assert_eq!(status, 201, "{base} added the member: {body}");
    }
    go_socket.collect_for(Duration::from_millis(900)).await;
    rust_socket.collect_for(Duration::from_millis(900)).await;

    let go_added = events_for_channel(&go_socket, "user_added", &fixture.go_channel);
    let rust_added = events_for_channel(&rust_socket, "user_added", &fixture.rust_channel);
    // This socket belongs to the *adder*, not the added user, so it sees only the
    // channel-addressed event — the second one is addressed to the added user's sessions.
    assert_eq!(
        go_added.len(),
        1,
        "Go published one channel-addressed user_added: {:?}",
        go_socket.raw
    );
    assert_eq!(
        rust_added.len(),
        go_added.len(),
        "the user_added count differs: {:?}",
        rust_socket.raw
    );
    assert_eq!(
        go_added[0]["data"], rust_added[0]["data"],
        "the user_added payload differs"
    );
    assert_eq!(rust_added[0]["data"]["user_id"], user.as_str());
    assert_eq!(rust_added[0]["data"]["team_id"], fixture.team.as_str());
    // The added user is in `omit_users`, which is what stops them getting it twice.
    assert_eq!(
        go_added[0]["broadcast"]["omit_users"], rust_added[0]["broadcast"]["omit_users"],
        "the user_added broadcast differs"
    );
    assert_eq!(
        rust_added[0]["broadcast"]["omit_users"][user.as_str()],
        true
    );
    assert_eq!(rust_added[0]["broadcast"]["user_id"], "");

    // --- the roles update: `channel_member_updated`, addressed to the **member** ---
    for (base, channel) in [(GO, &fixture.go_channel), (RUST, &fixture.rust_channel)] {
        let (status, body) = call(
            &http,
            base,
            reqwest::Method::PUT,
            &format!("/api/v4/channels/{channel}/members/{me}/notify_props"),
            &token,
            Some(&serde_json::json!({"desktop": "all"})),
        )
        .await;
        assert_eq!(status, 200, "{base} updated the props: {body}");
    }
    go_socket.collect_for(Duration::from_millis(900)).await;
    rust_socket.collect_for(Duration::from_millis(900)).await;

    let go_updated = member_events(&go_socket, &fixture.go_channel);
    let rust_updated = member_events(&rust_socket, &fixture.rust_channel);
    assert_eq!(
        go_updated.len(),
        1,
        "Go published one channel_member_updated: {:?}",
        go_socket.raw
    );
    assert_eq!(
        rust_updated.len(),
        go_updated.len(),
        "the channel_member_updated count differs: {:?}",
        rust_socket.raw
    );
    // Addressed to the member's user id and to **no channel** — a port that added the channel
    // here would broadcast one member's settings to everybody in it.
    assert_eq!(rust_updated[0]["broadcast"]["user_id"], me);
    assert_eq!(rust_updated[0]["broadcast"]["channel_id"], "");
    assert_eq!(
        go_updated[0]["broadcast"], rust_updated[0]["broadcast"],
        "the channel_member_updated broadcast differs"
    );
    // The payload is the member **as a JSON string**, not as an object.
    assert!(
        rust_updated[0]["data"]["channelMember"].is_string(),
        "the member is a JSON string in `data`: {}",
        rust_updated[0]
    );

    // --- the removal: two `user_removed` events with **different** payload keys ---
    for (base, channel) in [(GO, &fixture.go_channel), (RUST, &fixture.rust_channel)] {
        let (status, body) = call(
            &http,
            base,
            reqwest::Method::DELETE,
            &format!("/api/v4/channels/{channel}/members/{user}"),
            &token,
            None,
        )
        .await;
        assert_eq!(status, 200, "{base} removed the member: {body}");
    }
    go_socket.collect_for(Duration::from_millis(900)).await;
    rust_socket.collect_for(Duration::from_millis(900)).await;

    let go_removed = events_for_channel(&go_socket, "user_removed", &fixture.go_channel);
    let rust_removed = events_for_channel(&rust_socket, "user_removed", &fixture.rust_channel);
    assert_eq!(
        go_removed.len(),
        1,
        "Go published one channel-addressed user_removed: {:?}",
        go_socket.raw
    );
    assert_eq!(
        rust_removed.len(),
        go_removed.len(),
        "the user_removed count differs: {:?}",
        rust_socket.raw
    );
    assert_eq!(
        go_removed[0]["data"], rust_removed[0]["data"],
        "the user_removed payload differs"
    );
    // The channel-addressed event carries `user_id` and `remover_id` — **not** `channel_id`,
    // which is what its user-addressed sibling carries instead.
    assert_eq!(rust_removed[0]["data"]["user_id"], user.as_str());
    assert_eq!(rust_removed[0]["data"]["remover_id"], me);
    assert!(
        rust_removed[0]["data"]["channel_id"].is_null(),
        "the channel-addressed removal must not carry channel_id: {}",
        rust_removed[0]
    );

    cleanup(&http, &token, &fixture).await;
}

/// The events addressed to one channel, so two servers' fixtures never see each other's.
fn events_for_channel(
    probe: &SocketProbe,
    event_type: &str,
    channel_id: &str,
) -> Vec<serde_json::Value> {
    probe
        .events_named(event_type)
        .into_iter()
        .filter(|frame| frame["broadcast"]["channel_id"] == channel_id)
        .collect()
}

/// `channel_member_updated` carries no channel in its broadcast, so it is filtered by the
/// `channel_id` inside the encoded member instead.
fn member_events(probe: &SocketProbe, channel_id: &str) -> Vec<serde_json::Value> {
    probe
        .events_named("channel_member_updated")
        .into_iter()
        .filter(|frame| {
            frame["data"]["channelMember"]
                .as_str()
                .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
                .and_then(|member| member["channel_id"].as_str().map(|id| id == channel_id))
                .unwrap_or(false)
        })
        .collect()
}

/// A multi-id add where one id succeeds and another is refused answers **two JSON documents**:
/// the member array, then the error object.
///
/// The per-id loop calls `c.SetPermissionError`, which sets `c.Err` and is never cleared by a later
/// success; `handleContextError` runs after the handler and writes the error body on top of the
/// already-committed `201`. A reader would guess either a 403 or a clean 201 — it is both.
///
/// The fixture is a plain user who holds `join_public_channels` on the team (so they may add
/// **themselves** to a public channel) and not `manage_public_channel_members` (so they may not add
/// anybody else).
#[tokio::test]
async fn a_partly_refused_multi_add_answers_a_member_array_and_then_an_error() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let fixture = pair(&http, &token, "cmwpartial", "O").await;
    let joiner = create_plain_user(&http, &token, &fixture.team, "cmwpartial2").await;

    let body = serde_json::json!({"user_ids": [joiner.id, fixture.other_user]});
    let (go_status, go_raw) = call(
        &http,
        GO,
        reqwest::Method::POST,
        &format!("/api/v4/channels/{}/members", fixture.go_channel),
        &joiner.token,
        Some(&body),
    )
    .await;
    let (rust_status, rust_raw) = call(
        &http,
        RUST,
        reqwest::Method::POST,
        &format!("/api/v4/channels/{}/members", fixture.rust_channel),
        &joiner.token,
        Some(&body),
    )
    .await;

    // The status is the **success** one: it was written before the refusal was known.
    assert_eq!(
        go_status, 201,
        "Go commits the 201 before the error is written: {go_raw}"
    );
    assert_eq!(rust_status, go_status, "the partial-add status differs");

    // Two documents, concatenated. `serde_json::StreamDeserializer` is what reads them apart, the
    // same way an NDJSON client would.
    let split = |raw: &str| -> Vec<serde_json::Value> {
        serde_json::Deserializer::from_str(raw)
            .into_iter::<serde_json::Value>()
            .map(|value| value.expect("each document decodes"))
            .collect()
    };
    let go_docs = split(&go_raw);
    let rust_docs = split(&rust_raw);
    assert_eq!(
        go_docs.len(),
        2,
        "Go writes the members and then the error: {go_raw:?}"
    );
    assert_eq!(
        rust_docs.len(),
        go_docs.len(),
        "the number of documents differs\n go: {go_raw:?}\nrust: {rust_raw:?}"
    );

    // The first document is the one member that *was* added — the array shape, because the body
    // carried `user_ids` and not `user_id`.
    assert!(
        go_docs[0].is_array(),
        "the first document is the member array"
    );
    assert_eq!(
        go_docs[0].as_array().map(Vec::len),
        rust_docs[0].as_array().map(Vec::len),
        "a different number of members was added"
    );
    assert_eq!(rust_docs[0][0]["user_id"], joiner.id.as_str());

    // The second is an ordinary `AppError` envelope, with `detailed_error` wiped and a fresh
    // `request_id`, exactly as if it had been the whole response.
    assert_eq!(go_docs[1]["id"], "api.context.permissions.app_error");
    assert_eq!(
        rust_docs[1]["id"], go_docs[1]["id"],
        "the appended error differs"
    );
    assert_eq!(rust_docs[1]["status_code"], 403);
    assert_eq!(
        rust_docs[1]["detailed_error"], "",
        "the appended error must still have its detail wiped"
    );

    common::delete_plain_user(&http, &token, &joiner.id).await;
    cleanup(&http, &token, &fixture).await;
}
