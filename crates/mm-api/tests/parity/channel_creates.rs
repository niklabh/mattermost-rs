//! Cross-server parity for the three channel-creation routes.
//!
//! ```sh
//! docker compose up -d && scripts/parity.sh -p mm-api --test parity channel_creates
//! ```
//!
//! Every assertion about a write this server made reads back **through this server** ([D-190]).
//!
//! # Why every success test uses a different channel on each side
//!
//! A create is not idempotent in the way a read is, and the *name* of the thing created is
//! derived from its inputs: a public channel's name is the client's, a DM's is the two sorted
//! user ids, and a GM's is a SHA-1 of the sorted member list. So the two servers cannot be asked
//! to create the same channel — one of them would get the other's 400 or the other's row. Each
//! side gets its own inputs and the bodies are normalised before they are compared, which leaves
//! every field that is *not* derived from the inputs under byte comparison.
//!
//! The exception is the self-DM, which already exists for the fixture user: both servers return
//! the identical stored row, so that one is compared with nothing masked at all. It is the
//! strongest assertion in the file for exactly that reason.
//!
//! # The system post is missing here too
//!
//! `POST /channels` ends in `postJoinChannelMessage` on Go's side and does not here ([D-231]),
//! so Go's channel gets a post and ours does not. It is invisible in the **response**, which is
//! marshalled before the post exists — and very visible in a later `GET /channels/{id}`, whose
//! `total_msg_count` and `last_post_at` move on Go's row only. Nothing here re-reads a created
//! channel's row for that reason.

use std::time::Duration;

use crate::common;

use common::{
    GO, RUST, SocketProbe, a_team_and_channel_the_user_is_in,
    assert_error_bodies_match_except_known_gaps, client, create_plain_user, create_team,
    delete_plain_user, go_minted_token, logged_in_user_id, stack_enabled,
};

async fn post(
    http: &reqwest::Client,
    base: &str,
    token: &str,
    path: &str,
    body: &serde_json::Value,
) -> (u16, String, bool) {
    let response = http
        .post(format!("{base}{path}"))
        .header("Authorization", format!("Bearer {token}"))
        .json(body)
        .send()
        .await
        .unwrap_or_else(|e| panic!("{base} unreachable: {e}"));
    let status = response.status().as_u16();
    let by_rust = response
        .headers()
        .get("x-mmrs-served-by")
        .and_then(|value| value.to_str().ok())
        == Some("rust");
    (status, response.text().await.expect("a body"), by_rust)
}

/// A raw body, for the bodies `reqwest::json` cannot express (`{`, a bare `null`, trailing junk).
async fn post_raw(
    http: &reqwest::Client,
    base: &str,
    token: &str,
    path: &str,
    body: &'static str,
) -> (u16, String) {
    let response = http
        .post(format!("{base}{path}"))
        .header("Authorization", format!("Bearer {token}"))
        .header("Content-Type", "application/json")
        .body(body)
        .send()
        .await
        .unwrap_or_else(|e| panic!("{base} unreachable: {e}"));
    let status = response.status().as_u16();
    (status, response.text().await.expect("a body"))
}

/// The keys that cannot agree across two channels built from two different inputs, blanked so
/// everything else is compared byte for byte.
///
/// Deliberately **not** in the list: `type`, `team_id`, `creator_id`, `shared`, `scheme_id`,
/// `props`, `group_constrained`, `policy_enforced`, `policy_is_active`, `default_category_name`,
/// `managed_category_name`, `discoverable`, `header`, `purpose` and the four counters. Those are
/// where a create path's divergence would actually live — a GM whose `creator_id` is filled in,
/// a DM whose `shared` is `null` rather than `false`, a public channel whose `managed_category_name`
/// survived.
fn normalise(channel: &serde_json::Value) -> serde_json::Value {
    let mut out = channel.clone();
    let object = out.as_object_mut().expect("a channel object");
    for key in ["id", "name", "display_name"] {
        object.insert(key.to_owned(), serde_json::json!("<per-channel>"));
    }
    for key in ["create_at", "update_at"] {
        let value = object
            .get(key)
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0);
        object.insert(
            key.to_owned(),
            serde_json::json!(if value > 0 { "nonzero" } else { "zero" }),
        );
    }
    out
}

fn body(raw: &str) -> serde_json::Value {
    serde_json::from_str(raw).expect("a JSON body")
}

// ---------------------------------------------------------------------------------------------
// POST /api/v4/channels
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn creating_a_channel_answers_the_same_body_for_both_types() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let (team, _) = a_team_and_channel_the_user_is_in(&http, &token).await;

    for channel_type in ["O", "P"] {
        let tag = format!("{}{}", channel_type.to_lowercase(), std::process::id());
        let go_body = serde_json::json!({
            "team_id": team,
            "name": format!("mmrs-create-go-{tag}"),
            "display_name": format!("mmrs create go {tag}"),
            "type": channel_type,
            "header": "a header",
            "purpose": "a purpose",
        });
        let rust_body = serde_json::json!({
            "team_id": team,
            "name": format!("mmrs-create-rs-{tag}"),
            "display_name": format!("mmrs create rs {tag}"),
            "type": channel_type,
            "header": "a header",
            "purpose": "a purpose",
        });

        let (go_status, go_text, _) = post(&http, GO, &token, "/api/v4/channels", &go_body).await;
        let (rust_status, rust_text, by_rust) =
            post(&http, RUST, &token, "/api/v4/channels", &rust_body).await;

        assert!(by_rust, "{channel_type}: this server must serve the create");
        assert_eq!(go_status, 201, "{channel_type}: Go answers 201");
        assert_eq!(rust_status, 201, "{channel_type}: so must we");
        // The trailing newline `json.NewEncoder` leaves ([D-086]) is part of the wire format.
        assert!(
            rust_text.ends_with('\n') && go_text.ends_with('\n'),
            "{channel_type}: both bodies end in a newline"
        );
        assert_eq!(
            normalise(&body(&go_text)),
            normalise(&body(&rust_text)),
            "{channel_type}: the created channels differ"
        );

        // `header` and `purpose` are honoured on a create, unlike half the fields `PUT
        // /channels/{id}` accepts — pinned because they are *not* masked above but are easy to
        // drop from an INSERT column list without any other test noticing.
        assert_eq!(body(&rust_text)["header"], "a header");
        assert_eq!(body(&rust_text)["purpose"], "a purpose");

        common::delete_channel(&http, &token, body(&go_text)["id"].as_str().expect("an id")).await;
        common::delete_channel(
            &http,
            &token,
            body(&rust_text)["id"].as_str().expect("an id"),
        )
        .await;
    }
}

/// The creator is a channel **admin**, which the create response cannot show.
///
/// `CreateChannel` builds the membership with `SchemeAdmin: true` and every other membership
/// write on every other create path leaves it false. Nothing in the 201 body depends on it, so
/// this is the only test that can tell `channel_user channel_admin` from `channel_user`.
#[tokio::test]
async fn the_creator_is_a_channel_admin_on_both_servers() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let (team, _) = a_team_and_channel_the_user_is_in(&http, &token).await;
    let me = logged_in_user_id();
    let tag = std::process::id();

    let (_, go_text, _) = post(
        &http,
        GO,
        &token,
        "/api/v4/channels",
        &serde_json::json!({
            "team_id": team,
            "name": format!("mmrs-admin-go-{tag}"),
            "display_name": "mmrs admin go",
            "type": "O",
        }),
    )
    .await;
    let (_, rust_text, _) = post(
        &http,
        RUST,
        &token,
        "/api/v4/channels",
        &serde_json::json!({
            "team_id": team,
            "name": format!("mmrs-admin-rs-{tag}"),
            "display_name": "mmrs admin rs",
            "type": "O",
        }),
    )
    .await;
    let go_channel = body(&go_text)["id"].as_str().expect("an id").to_owned();
    let rust_channel = body(&rust_text)["id"].as_str().expect("an id").to_owned();

    // Each side reads back through the server that wrote it ([D-190]).
    let go_member = http
        .get(format!("{GO}/api/v4/channels/{go_channel}/members/{me}"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("Go answers")
        .json::<serde_json::Value>()
        .await
        .expect("a member");
    let rust_member = http
        .get(format!(
            "{RUST}/api/v4/channels/{rust_channel}/members/{me}"
        ))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("mm-api answers")
        .json::<serde_json::Value>()
        .await
        .expect("a member");

    assert_eq!(
        go_member["roles"], "channel_user channel_admin",
        "Go makes the creator a channel admin"
    );
    assert_eq!(
        rust_member["roles"], go_member["roles"],
        "so must we: {rust_member}"
    );
    for key in [
        "scheme_user",
        "scheme_admin",
        "scheme_guest",
        "explicit_roles",
    ] {
        assert_eq!(
            rust_member[key], go_member[key],
            "the member's {key} differs"
        );
    }
    assert_eq!(
        rust_member["notify_props"], go_member["notify_props"],
        "the default notify props differ"
    );

    // The `ChannelMemberHistory` row no route reads. `parity/channel_member_writes.rs` checks it
    // the same way and for the same reason: dropping the write is invisible over HTTP.
    if let Some(history) = common::channel_member_history(&rust_channel, me).await {
        assert_eq!(
            history.len(),
            1,
            "the create logs exactly one join event: {history:?}"
        );
        assert_eq!(history[0].1, None, "the join is still open");
    }

    common::delete_channel(&http, &token, &go_channel).await;
    common::delete_channel(&http, &token, &rust_channel).await;
}

/// A taken name is a 400 — and **an archived channel still holds its name**.
#[tokio::test]
async fn a_taken_name_is_the_same_400_even_when_the_holder_is_archived() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let (team, _) = a_team_and_channel_the_user_is_in(&http, &token).await;
    let tag = std::process::id();

    for (base, side) in [(GO, "go"), (RUST, "rs")] {
        let name = format!("mmrs-dup-{side}-{tag}");
        let create = serde_json::json!({
            "team_id": team,
            "name": name,
            "display_name": "mmrs dup",
            "type": "O",
        });
        let (status, text, _) = post(&http, base, &token, "/api/v4/channels", &create).await;
        assert_eq!(status, 201, "{side}: the first create succeeds");
        let id = body(&text)["id"].as_str().expect("an id").to_owned();

        let (again, again_text, _) = post(&http, base, &token, "/api/v4/channels", &create).await;
        assert_eq!(again, 400, "{side}: a live duplicate is a 400");
        assert_eq!(
            body(&again_text)["id"],
            "store.sql_channel.save_channel.exists.app_error",
            "{side}: the duplicate id"
        );

        common::delete_channel(&http, &token, &id).await;
        let (archived, archived_text, _) =
            post(&http, base, &token, "/api/v4/channels", &create).await;
        assert_eq!(
            archived, 400,
            "{side}: an archived channel still holds the name — {archived_text}"
        );
        assert_eq!(
            body(&archived_text)["id"],
            "store.sql_channel.save_channel.exists.app_error",
            "{side}: and it is the same id"
        );
    }
}

#[tokio::test]
async fn the_create_channel_error_branches_match() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let (team, _) = a_team_and_channel_the_user_is_in(&http, &token).await;
    let tag = std::process::id();

    // (label, body-for-Go, body-for-us). Two bodies because a *successful* branch would collide
    // on the name; every case here is an error on both, but the pair keeps that from being an
    // assumption.
    let cases: Vec<(&str, serde_json::Value, serde_json::Value)> = vec![
        (
            "no team_id",
            serde_json::json!({"name": format!("mmrs-e1-go-{tag}"), "display_name": "e", "type": "O"}),
            serde_json::json!({"name": format!("mmrs-e1-rs-{tag}"), "display_name": "e", "type": "O"}),
        ),
        (
            "no display_name",
            serde_json::json!({"team_id": team, "name": format!("mmrs-e2-go-{tag}"), "type": "O"}),
            serde_json::json!({"team_id": team, "name": format!("mmrs-e2-rs-{tag}"), "type": "O"}),
        ),
        (
            "type D",
            serde_json::json!({"team_id": team, "name": format!("mmrs-e3-go-{tag}"), "display_name": "e", "type": "D"}),
            serde_json::json!({"team_id": team, "name": format!("mmrs-e3-rs-{tag}"), "display_name": "e", "type": "D"}),
        ),
        (
            "type G",
            serde_json::json!({"team_id": team, "name": format!("mmrs-e4-go-{tag}"), "display_name": "e", "type": "G"}),
            serde_json::json!({"team_id": team, "name": format!("mmrs-e4-rs-{tag}"), "display_name": "e", "type": "G"}),
        ),
        (
            "a board type",
            serde_json::json!({"team_id": team, "name": format!("mmrs-e5-go-{tag}"), "display_name": "e", "type": "BO"}),
            serde_json::json!({"team_id": team, "name": format!("mmrs-e5-rs-{tag}"), "display_name": "e", "type": "BO"}),
        ),
        (
            "a space type",
            serde_json::json!({"team_id": team, "name": format!("mmrs-e6-go-{tag}"), "display_name": "e", "type": "S"}),
            serde_json::json!({"team_id": team, "name": format!("mmrs-e6-rs-{tag}"), "display_name": "e", "type": "S"}),
        ),
        (
            "an unknown type",
            serde_json::json!({"team_id": team, "name": format!("mmrs-e7-go-{tag}"), "display_name": "e", "type": "Z"}),
            serde_json::json!({"team_id": team, "name": format!("mmrs-e7-rs-{tag}"), "display_name": "e", "type": "Z"}),
        ),
        (
            "an empty name",
            serde_json::json!({"team_id": team, "name": "", "display_name": "e", "type": "O"}),
            serde_json::json!({"team_id": team, "name": "", "display_name": "e", "type": "O"}),
        ),
        (
            "an over-long display name",
            serde_json::json!({"team_id": team, "name": format!("mmrs-e9-go-{tag}"), "display_name": "d".repeat(65), "type": "O"}),
            serde_json::json!({"team_id": team, "name": format!("mmrs-e9-rs-{tag}"), "display_name": "d".repeat(65), "type": "O"}),
        ),
        (
            "discoverable and private",
            serde_json::json!({"team_id": team, "name": format!("mmrs-ea-go-{tag}"), "display_name": "e", "type": "P", "discoverable": true}),
            serde_json::json!({"team_id": team, "name": format!("mmrs-ea-rs-{tag}"), "display_name": "e", "type": "P", "discoverable": true}),
        ),
        (
            "discoverable and public",
            serde_json::json!({"team_id": team, "name": format!("mmrs-eb-go-{tag}"), "display_name": "e", "type": "O", "discoverable": true}),
            serde_json::json!({"team_id": team, "name": format!("mmrs-eb-rs-{tag}"), "display_name": "e", "type": "O", "discoverable": true}),
        ),
        (
            "a name that looks like a DM",
            serde_json::json!({"team_id": team, "name": format!("{}__{}", logged_in_user_id(), logged_in_user_id()), "display_name": "e", "type": "O"}),
            serde_json::json!({"team_id": team, "name": format!("{}__{}", logged_in_user_id(), logged_in_user_id()), "display_name": "e", "type": "O"}),
        ),
        (
            "a team that does not exist",
            serde_json::json!({"team_id": "aaaaaaaaaaaaaaaaaaaaaaaaaa", "name": format!("mmrs-ed-go-{tag}"), "display_name": "e", "type": "O"}),
            serde_json::json!({"team_id": "aaaaaaaaaaaaaaaaaaaaaaaaaa", "name": format!("mmrs-ed-rs-{tag}"), "display_name": "e", "type": "O"}),
        ),
    ];

    for (label, go_body, rust_body) in cases {
        let (go_status, go_text, _) = post(&http, GO, &token, "/api/v4/channels", &go_body).await;
        let (rust_status, rust_text, by_rust) =
            post(&http, RUST, &token, "/api/v4/channels", &rust_body).await;
        assert!(by_rust, "{label}: this server must answer it");
        assert_eq!(
            go_status, rust_status,
            "{label}: status differs\n  go: {go_text}\n  rs: {rust_text}"
        );
        assert_ne!(
            go_status, 201,
            "{label}: the case must be an error on Go too"
        );
        assert_error_bodies_match_except_known_gaps(
            go_text.as_bytes(),
            rust_text.as_bytes(),
            label,
        );
    }

    // The malformed-body cases, which need a raw payload.
    for (label, raw) in [
        ("a truncated object", "{"),
        ("a bare null", "null"),
        ("an array", "[]"),
        ("a bare string", "\"abc\""),
    ] {
        let (go_status, go_text) = post_raw(&http, GO, &token, "/api/v4/channels", raw).await;
        let (rust_status, rust_text) = post_raw(&http, RUST, &token, "/api/v4/channels", raw).await;
        assert_eq!(go_status, rust_status, "{label}: status differs");
        assert_error_bodies_match_except_known_gaps(
            go_text.as_bytes(),
            rust_text.as_bytes(),
            label,
        );
    }
}

/// A team the actor is not a member of, which is the only permission branch reachable with the
/// fixture user's own token gone.
///
/// The assertion is deliberately "both servers agree", not a hardcoded status: `system_user`
/// carries `create_public_channel` system-wide on a stock installation, and which way
/// `SessionHasPermissionToTeam` falls for a non-member is exactly the thing being compared.
#[tokio::test]
async fn a_team_the_actor_is_not_in_is_answered_identically() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let admin = go_minted_token(&http).await;
    let (home_team, _) = a_team_and_channel_the_user_is_in(&http, &admin).await;
    let outsider = create_plain_user(&http, &admin, &home_team, "cnotmem").await;
    let other_team = create_team(&http, &admin, "cnotmem").await;
    let tag = std::process::id();

    let go_body = serde_json::json!({
        "team_id": other_team, "name": format!("mmrs-perm-go-{tag}"),
        "display_name": "mmrs perm", "type": "P",
    });
    let rust_body = serde_json::json!({
        "team_id": other_team, "name": format!("mmrs-perm-rs-{tag}"),
        "display_name": "mmrs perm", "type": "P",
    });

    let (go_status, go_text, _) =
        post(&http, GO, &outsider.token, "/api/v4/channels", &go_body).await;
    let (rust_status, rust_text, by_rust) =
        post(&http, RUST, &outsider.token, "/api/v4/channels", &rust_body).await;
    assert!(by_rust, "this server must answer it");
    assert_eq!(
        go_status, rust_status,
        "a non-member's create differs\n  go: {go_text}\n  rs: {rust_text}"
    );
    if go_status == 201 {
        assert_eq!(normalise(&body(&go_text)), normalise(&body(&rust_text)));
        common::delete_channel(&http, &admin, body(&go_text)["id"].as_str().expect("an id")).await;
        common::delete_channel(
            &http,
            &admin,
            body(&rust_text)["id"].as_str().expect("an id"),
        )
        .await;
    } else {
        assert_error_bodies_match_except_known_gaps(
            go_text.as_bytes(),
            rust_text.as_bytes(),
            "non-member create",
        );
    }

    delete_plain_user(&http, &admin, &outsider.id).await;
}

/// `channel_created` is addressed to the **user**, and carries the channel and team ids as data.
#[tokio::test]
async fn creating_a_channel_publishes_channel_created_to_the_creator() {
    if !stack_enabled() {
        return;
    }
    let _broadcast = common::BROADCAST_STREAM.lock().await;
    let http = client();
    let token = go_minted_token(&http).await;
    let (team, _) = a_team_and_channel_the_user_is_in(&http, &token).await;
    let tag = std::process::id();

    let mut go_socket = SocketProbe::connect(GO, &token).await;
    let mut rust_socket = SocketProbe::connect(RUST, &token).await;

    let (_, go_text, _) = post(
        &http,
        GO,
        &token,
        "/api/v4/channels",
        &serde_json::json!({"team_id": team, "name": format!("mmrs-ws-go-{tag}"), "display_name": "ws go", "type": "O"}),
    )
    .await;
    let (_, rust_text, _) = post(
        &http,
        RUST,
        &token,
        "/api/v4/channels",
        &serde_json::json!({"team_id": team, "name": format!("mmrs-ws-rs-{tag}"), "display_name": "ws rs", "type": "O"}),
    )
    .await;
    let go_id = body(&go_text)["id"].as_str().expect("an id").to_owned();
    let rust_id = body(&rust_text)["id"].as_str().expect("an id").to_owned();

    let named = |id: String| {
        move |frames: &[serde_json::Value]| {
            frames.iter().any(|frame| {
                frame["event"] == "channel_created" && frame["data"]["channel_id"] == id.as_str()
            })
        }
    };
    go_socket
        .collect_until(Duration::from_millis(2000), named(go_id.clone()))
        .await;
    rust_socket
        .collect_until(Duration::from_millis(2000), named(rust_id.clone()))
        .await;

    let go_events: Vec<_> = go_socket
        .events_named("channel_created")
        .into_iter()
        .filter(|frame| frame["data"]["channel_id"] == go_id.as_str())
        .collect();
    let rust_events: Vec<_> = rust_socket
        .events_named("channel_created")
        .into_iter()
        .filter(|frame| frame["data"]["channel_id"] == rust_id.as_str())
        .collect();

    assert_eq!(go_events.len(), 1, "Go published one: {:?}", go_socket.raw);
    assert_eq!(
        rust_events.len(),
        1,
        "we published one: {:?}",
        rust_socket.raw
    );
    assert_eq!(go_events[0]["data"]["team_id"], team);
    assert_eq!(rust_events[0]["data"]["team_id"], team);
    // Addressed to the user, with an **empty** channel_id and team_id on the broadcast — so it
    // reaches the creator's other sessions and nobody else's. Getting this backwards would
    // announce a private channel to the whole team.
    assert_eq!(
        go_events[0]["broadcast"], rust_events[0]["broadcast"],
        "the addressing differs"
    );
    assert_eq!(rust_events[0]["broadcast"]["user_id"], logged_in_user_id());
    assert_eq!(rust_events[0]["broadcast"]["channel_id"], "");
    assert_eq!(rust_events[0]["broadcast"]["team_id"], "");
    assert_eq!(rust_events[0]["broadcast"]["omit_connection_id"], "");

    common::delete_channel(&http, &token, &go_id).await;
    common::delete_channel(&http, &token, &rust_id).await;
}

// ---------------------------------------------------------------------------------------------
// POST /api/v4/channels/direct
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn opening_a_direct_channel_answers_the_same_body() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let (team, _) = a_team_and_channel_the_user_is_in(&http, &token).await;
    let me = logged_in_user_id();
    let go_other = create_plain_user(&http, &token, &team, "dmgo").await;
    let rust_other = create_plain_user(&http, &token, &team, "dmrs").await;

    let (go_status, go_text, _) = post(
        &http,
        GO,
        &token,
        "/api/v4/channels/direct",
        &serde_json::json!([me, go_other.id]),
    )
    .await;
    let (rust_status, rust_text, by_rust) = post(
        &http,
        RUST,
        &token,
        "/api/v4/channels/direct",
        &serde_json::json!([me, rust_other.id]),
    )
    .await;

    assert!(by_rust, "this server must serve the DM create");
    assert_eq!(go_status, 201, "Go answers 201");
    assert_eq!(rust_status, 201, "so must we");
    assert!(rust_text.ends_with('\n') && go_text.ends_with('\n'));
    assert_eq!(
        normalise(&body(&go_text)),
        normalise(&body(&rust_text)),
        "the direct channels differ"
    );
    // `shared` is `false`, not `null`: `CreateDirectChannel` assigns `new(bool)` where a public
    // channel leaves the pointer nil. That is a genuine wire difference between the two routes.
    assert_eq!(body(&rust_text)["shared"], false);
    assert_eq!(body(&rust_text)["team_id"], "");
    assert_eq!(body(&rust_text)["display_name"], "");
    assert_eq!(body(&rust_text)["creator_id"], me);
    // The name is the two ids sorted bytewise, joined by `__`.
    let mut pair = [me.to_owned(), rust_other.id.clone()];
    pair.sort();
    assert_eq!(
        body(&rust_text)["name"],
        format!("{}__{}", pair[0], pair[1])
    );

    // Both members are in it, read back through the server that wrote it.
    for user in [me, rust_other.id.as_str()] {
        let channel_id = body(&rust_text)["id"].as_str().expect("an id").to_owned();
        let member = http
            .get(format!(
                "{RUST}/api/v4/channels/{channel_id}/members/{user}"
            ))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .expect("mm-api answers");
        assert_eq!(member.status().as_u16(), 200, "{user} must be a member");
        let member: serde_json::Value = member.json().await.expect("a member");
        assert_eq!(
            member["scheme_admin"], false,
            "nobody is a channel admin in a DM"
        );
    }

    delete_plain_user(&http, &token, &go_other.id).await;
    delete_plain_user(&http, &token, &rust_other.id).await;
}

/// The self-DM is the one channel both servers can be asked to open *and* both already have, so
/// the two bodies are compared with nothing masked.
#[tokio::test]
async fn the_self_direct_channel_is_byte_identical_on_both() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let me = logged_in_user_id();

    // A one-element list naming yourself is duplicated by the handler; the two-element form is
    // the same channel by a different route into it. Both must agree.
    for list in [serde_json::json!([me]), serde_json::json!([me, me])] {
        let (go_status, go_text, _) =
            post(&http, GO, &token, "/api/v4/channels/direct", &list).await;
        let (rust_status, rust_text, by_rust) =
            post(&http, RUST, &token, "/api/v4/channels/direct", &list).await;
        assert!(by_rust, "{list}: this server must answer it");
        assert_eq!(go_status, 201, "{list}: Go answers 201");
        assert_eq!(rust_status, 201, "{list}: so must we");
        assert_eq!(
            go_text, rust_text,
            "{list}: the self-DM body must be byte-identical"
        );
    }
}

/// Re-opening an existing DM is a **201** carrying the existing channel, not a 200 and not a 400.
#[tokio::test]
async fn re_opening_a_direct_channel_is_201_with_the_same_channel() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let (team, _) = a_team_and_channel_the_user_is_in(&http, &token).await;
    let me = logged_in_user_id();
    let other = create_plain_user(&http, &token, &team, "dmidem").await;
    let list = serde_json::json!([me, other.id]);

    for (base, side) in [(GO, "go"), (RUST, "rs")] {
        let (first, first_text, _) =
            post(&http, base, &token, "/api/v4/channels/direct", &list).await;
        let (second, second_text, _) =
            post(&http, base, &token, "/api/v4/channels/direct", &list).await;
        assert_eq!(first, 201, "{side}: the first open is 201");
        assert_eq!(second, 201, "{side}: and so is the second");
        assert_eq!(
            first_text, second_text,
            "{side}: the second open returns the same channel byte for byte"
        );
    }

    delete_plain_user(&http, &token, &other.id).await;
}

#[tokio::test]
async fn the_direct_channel_error_branches_match() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let (team, _) = a_team_and_channel_the_user_is_in(&http, &token).await;
    let me = logged_in_user_id();
    let other = create_plain_user(&http, &token, &team, "dmerr").await;
    let third = create_plain_user(&http, &token, &team, "dmerr2").await;

    let cases: Vec<(&str, serde_json::Value)> = vec![
        ("three ids", serde_json::json!([me, other.id, third.id])),
        ("an empty list", serde_json::json!([])),
        (
            "one id that is not mine",
            serde_json::json!([other.id.clone()]),
        ),
        ("a malformed id", serde_json::json!([me, "nope"])),
        ("two malformed ids", serde_json::json!(["nope", "alsonope"])),
        (
            "an id that is valid but does not exist",
            serde_json::json!([me, "aaaaaaaaaaaaaaaaaaaaaaaaaa"]),
        ),
        (
            "two other users",
            serde_json::json!([other.id.clone(), third.id.clone()]),
        ),
    ];

    for (label, list) in cases {
        let (go_status, go_text, _) =
            post(&http, GO, &token, "/api/v4/channels/direct", &list).await;
        let (rust_status, rust_text, by_rust) =
            post(&http, RUST, &token, "/api/v4/channels/direct", &list).await;
        assert!(by_rust, "{label}: this server must answer it");
        assert_eq!(
            go_status, rust_status,
            "{label}: status differs\n  go: {go_text}\n  rs: {rust_text}"
        );
        if go_status == 201 {
            // "two other users" succeeds for a system admin (`manage_system`), and the channel is
            // the same one on both servers — so it is a byte comparison, not a normalised one.
            assert_eq!(go_text, rust_text, "{label}: the bodies differ");
        } else {
            assert_error_bodies_match_except_known_gaps(
                go_text.as_bytes(),
                rust_text.as_bytes(),
                label,
            );
        }
    }

    for (label, raw) in [
        ("a truncated array", "["),
        ("a bare null", "null"),
        ("an object", "{}"),
        ("a number", "123"),
        ("a list of numbers", "[1,2]"),
    ] {
        let (go_status, go_text) =
            post_raw(&http, GO, &token, "/api/v4/channels/direct", raw).await;
        let (rust_status, rust_text) =
            post_raw(&http, RUST, &token, "/api/v4/channels/direct", raw).await;
        assert_eq!(
            go_status, rust_status,
            "{label}: status differs\n  go: {go_text}\n  rs: {rust_text}"
        );
        assert_error_bodies_match_except_known_gaps(
            go_text.as_bytes(),
            rust_text.as_bytes(),
            label,
        );
    }

    delete_plain_user(&http, &token, &other.id).await;
    delete_plain_user(&http, &token, &third.id).await;
}

/// `direct_added` is addressed to the **channel**, and `creator_id` is the first id the *body*
/// listed — not the session's user.
#[tokio::test]
async fn opening_a_direct_channel_publishes_direct_added_naming_the_first_id() {
    if !stack_enabled() {
        return;
    }
    let _broadcast = common::BROADCAST_STREAM.lock().await;
    let http = client();
    let token = go_minted_token(&http).await;
    let (team, _) = a_team_and_channel_the_user_is_in(&http, &token).await;
    let me = logged_in_user_id();
    let go_other = create_plain_user(&http, &token, &team, "dmwsgo").await;
    let rust_other = create_plain_user(&http, &token, &team, "dmwsrs").await;

    let mut go_socket = SocketProbe::connect(GO, &token).await;
    let mut rust_socket = SocketProbe::connect(RUST, &token).await;

    // The list is sent **largest id first**, which is two assertions in one: `creator_id` is
    // taken from the body rather than from the session (a port using the session would put `me`
    // here), *and* the body's order survives (`NonSortedArrayFromJSON`, not the sorting one — a
    // sorted parse would put the smaller id first and name the wrong creator). Choosing the
    // order by comparison rather than by luck is what makes the second half deterministic.
    let go_pair = descending(me, &go_other.id);
    let rust_pair = descending(me, &rust_other.id);
    let (_, go_text, _) = post(
        &http,
        GO,
        &token,
        "/api/v4/channels/direct",
        &serde_json::json!(go_pair),
    )
    .await;
    let (_, rust_text, _) = post(
        &http,
        RUST,
        &token,
        "/api/v4/channels/direct",
        &serde_json::json!(rust_pair),
    )
    .await;
    let go_id = body(&go_text)["id"].as_str().expect("an id").to_owned();
    let rust_id = body(&rust_text)["id"].as_str().expect("an id").to_owned();

    let named = |id: String| {
        move |frames: &[serde_json::Value]| {
            frames.iter().any(|frame| {
                frame["event"] == "direct_added" && frame["broadcast"]["channel_id"] == id.as_str()
            })
        }
    };
    go_socket
        .collect_until(Duration::from_millis(2000), named(go_id.clone()))
        .await;
    rust_socket
        .collect_until(Duration::from_millis(2000), named(rust_id.clone()))
        .await;

    let go_events: Vec<_> = go_socket
        .events_named("direct_added")
        .into_iter()
        .filter(|frame| frame["broadcast"]["channel_id"] == go_id.as_str())
        .collect();
    let rust_events: Vec<_> = rust_socket
        .events_named("direct_added")
        .into_iter()
        .filter(|frame| frame["broadcast"]["channel_id"] == rust_id.as_str())
        .collect();

    assert_eq!(go_events.len(), 1, "Go published one: {:?}", go_socket.raw);
    assert_eq!(
        rust_events.len(),
        1,
        "we published one: {:?}",
        rust_socket.raw
    );
    assert_eq!(
        go_events[0]["data"]["creator_id"], go_pair[0],
        "Go names the body's first id as the creator"
    );
    assert_eq!(
        rust_events[0]["data"]["creator_id"], rust_pair[0],
        "so must we"
    );
    assert_eq!(go_events[0]["data"]["teammate_id"], go_pair[1]);
    assert_eq!(rust_events[0]["data"]["teammate_id"], rust_pair[1]);
    assert_eq!(rust_events[0]["broadcast"]["user_id"], "");
    assert_eq!(rust_events[0]["broadcast"]["team_id"], "");
    assert_eq!(rust_events[0]["broadcast"]["omit_connection_id"], "");

    delete_plain_user(&http, &token, &go_other.id).await;
    delete_plain_user(&http, &token, &rust_other.id).await;
}

/// The two ids with the **larger** first, so a test can tell a body-ordered parse from a sorted
/// one without depending on which random id happened to sort first.
fn descending(a: &str, b: &str) -> [String; 2] {
    if a > b {
        [a.to_owned(), b.to_owned()]
    } else {
        [b.to_owned(), a.to_owned()]
    }
}

// ---------------------------------------------------------------------------------------------
// POST /api/v4/channels/group
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn creating_a_group_channel_answers_the_same_body() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let (team, _) = a_team_and_channel_the_user_is_in(&http, &token).await;
    let me = logged_in_user_id();
    let go_a = create_plain_user(&http, &token, &team, "gmgoa").await;
    let go_b = create_plain_user(&http, &token, &team, "gmgob").await;
    let rs_a = create_plain_user(&http, &token, &team, "gmrsa").await;
    let rs_b = create_plain_user(&http, &token, &team, "gmrsb").await;

    let (go_status, go_text, _) = post(
        &http,
        GO,
        &token,
        "/api/v4/channels/group",
        &serde_json::json!([me, go_a.id, go_b.id]),
    )
    .await;
    let (rust_status, rust_text, by_rust) = post(
        &http,
        RUST,
        &token,
        "/api/v4/channels/group",
        &serde_json::json!([me, rs_a.id, rs_b.id]),
    )
    .await;

    assert!(by_rust, "this server must serve the GM create");
    assert_eq!(go_status, 201, "Go answers 201");
    assert_eq!(rust_status, 201, "so must we");
    assert!(rust_text.ends_with('\n') && go_text.ends_with('\n'));
    assert_eq!(
        normalise(&body(&go_text)),
        normalise(&body(&rust_text)),
        "the group channels differ"
    );
    // **`creator_id` is empty** on a group channel — `createGroupChannel` never sets it, unlike
    // every other create on this file's three routes.
    assert_eq!(body(&rust_text)["creator_id"], "");
    assert_eq!(body(&rust_text)["shared"], false);
    assert_eq!(body(&rust_text)["team_id"], "");
    // The name is the SHA-1 of the sorted ids: 40 lowercase hex characters.
    let name = body(&rust_text)["name"]
        .as_str()
        .expect("a name")
        .to_owned();
    assert_eq!(name.len(), 40, "the GM name is a sha1 hex digest: {name}");
    assert!(
        name.chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
    );
    // The display name is the members' usernames, sorted, comma-separated.
    let display = body(&rust_text)["display_name"]
        .as_str()
        .expect("a display name")
        .to_owned();
    let mut expected: Vec<String> = vec![
        common::username_of(&http, &token, me).await,
        common::username_of(&http, &token, &rs_a.id).await,
        common::username_of(&http, &token, &rs_b.id).await,
    ];
    expected.sort();
    assert_eq!(display, expected.join(", "), "the GM display name differs");

    // Every member is in it, nobody is a channel admin.
    let channel_id = body(&rust_text)["id"].as_str().expect("an id").to_owned();
    for user in [me, rs_a.id.as_str(), rs_b.id.as_str()] {
        let member = http
            .get(format!(
                "{RUST}/api/v4/channels/{channel_id}/members/{user}"
            ))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .expect("mm-api answers");
        assert_eq!(member.status().as_u16(), 200, "{user} must be a member");
        let member: serde_json::Value = member.json().await.expect("a member");
        assert_eq!(member["scheme_admin"], false, "{user} must not be an admin");
        assert_eq!(member["scheme_user"], true);
    }

    for user in [&go_a, &go_b, &rs_a, &rs_b] {
        delete_plain_user(&http, &token, &user.id).await;
    }
}

/// Re-creating a group channel with the same membership is a 201 carrying the existing channel.
///
/// The name is a hash of the sorted ids, so the second create collides in the store and Go
/// swallows the conflict. A caller that omits their own id gets it appended, so the **same**
/// group channel is reachable by two different bodies — asserted here, because the append happens
/// after the sort and a port that sorted afterwards would hash a different list.
#[tokio::test]
async fn re_creating_a_group_channel_is_201_with_the_same_channel() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let (team, _) = a_team_and_channel_the_user_is_in(&http, &token).await;
    let me = logged_in_user_id();
    let a = create_plain_user(&http, &token, &team, "gmidema").await;
    let b = create_plain_user(&http, &token, &team, "gmidemb").await;

    for (base, side) in [(GO, "go"), (RUST, "rs")] {
        let full = serde_json::json!([me, a.id, b.id]);
        let without_me = serde_json::json!([a.id, b.id]);
        let (first, first_text, _) =
            post(&http, base, &token, "/api/v4/channels/group", &full).await;
        assert_eq!(first, 201, "{side}: the first create is 201 — {first_text}");
        let (second, second_text, _) =
            post(&http, base, &token, "/api/v4/channels/group", &full).await;
        assert_eq!(second, 201, "{side}: so is the second");
        assert_eq!(
            first_text, second_text,
            "{side}: the same channel comes back"
        );
        let (third, third_text, _) =
            post(&http, base, &token, "/api/v4/channels/group", &without_me).await;
        assert_eq!(third, 201, "{side}: omitting yourself still reaches it");
        assert_eq!(
            first_text, third_text,
            "{side}: the appended id must hash to the same name"
        );
    }

    delete_plain_user(&http, &token, &a.id).await;
    delete_plain_user(&http, &token, &b.id).await;
}

#[tokio::test]
async fn the_group_channel_error_branches_match() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let (team, _) = a_team_and_channel_the_user_is_in(&http, &token).await;
    let me = logged_in_user_id();
    let a = create_plain_user(&http, &token, &team, "gmerra").await;

    // Eight other ids plus the caller is nine — one over `ChannelGroupMaxUsers`.
    let too_many: Vec<String> = (0..8)
        .map(|n| format!("{n}aaaaaaaaaaaaaaaaaaaaaaaaa"))
        .collect();

    let cases: Vec<(&str, serde_json::Value)> = vec![
        ("an empty list", serde_json::json!([])),
        ("just me", serde_json::json!([me])),
        ("me and one other", serde_json::json!([me, a.id])),
        ("one other", serde_json::json!([a.id.clone()])),
        ("a malformed id", serde_json::json!([me, a.id, "nope"])),
        ("nine ids", serde_json::json!(too_many)),
        (
            "three valid ids that do not exist",
            serde_json::json!([
                "aaaaaaaaaaaaaaaaaaaaaaaaaa",
                "bbbbbbbbbbbbbbbbbbbbbbbbbb",
                "cccccccccccccccccccccccccc"
            ]),
        ),
    ];

    for (label, list) in cases {
        let (go_status, go_text, _) =
            post(&http, GO, &token, "/api/v4/channels/group", &list).await;
        let (rust_status, rust_text, by_rust) =
            post(&http, RUST, &token, "/api/v4/channels/group", &list).await;
        assert!(by_rust, "{label}: this server must answer it");
        assert_eq!(
            go_status, rust_status,
            "{label}: status differs\n  go: {go_text}\n  rs: {rust_text}"
        );
        assert_ne!(go_status, 201, "{label}: the case must be an error on Go");
        assert_error_bodies_match_except_known_gaps(
            go_text.as_bytes(),
            rust_text.as_bytes(),
            label,
        );
    }

    for (label, raw) in [
        ("a truncated array", "["),
        ("a bare null", "null"),
        ("an object", "{}"),
        ("a list of numbers", "[1,2,3]"),
    ] {
        let (go_status, go_text) = post_raw(&http, GO, &token, "/api/v4/channels/group", raw).await;
        let (rust_status, rust_text) =
            post_raw(&http, RUST, &token, "/api/v4/channels/group", raw).await;
        assert_eq!(
            go_status, rust_status,
            "{label}: status differs\n  go: {go_text}\n  rs: {rust_text}"
        );
        assert_error_bodies_match_except_known_gaps(
            go_text.as_bytes(),
            rust_text.as_bytes(),
            label,
        );
    }

    delete_plain_user(&http, &token, &a.id).await;
}

/// `group_added` is published **once per member**, each addressed to that member, and carries the
/// sorted id list as a JSON array inside a JSON string.
#[tokio::test]
async fn creating_a_group_channel_publishes_group_added_to_every_member() {
    if !stack_enabled() {
        return;
    }
    let _broadcast = common::BROADCAST_STREAM.lock().await;
    let http = client();
    let token = go_minted_token(&http).await;
    let (team, _) = a_team_and_channel_the_user_is_in(&http, &token).await;
    let me = logged_in_user_id();
    let go_a = create_plain_user(&http, &token, &team, "gmwsgoa").await;
    let go_b = create_plain_user(&http, &token, &team, "gmwsgob").await;
    let rs_a = create_plain_user(&http, &token, &team, "gmwsrsa").await;
    let rs_b = create_plain_user(&http, &token, &team, "gmwsrsb").await;

    let mut go_socket = SocketProbe::connect(GO, &token).await;
    let mut rust_socket = SocketProbe::connect(RUST, &token).await;

    // Deliberately unsorted, and without the caller, so the sort *and* the append are both
    // exercised: `teammate_ids` must still come out sorted and must include `me`.
    let go_list = serde_json::json!([go_b.id, go_a.id]);
    let rust_list = serde_json::json!([rs_b.id, rs_a.id]);
    let (_, go_text, _) = post(&http, GO, &token, "/api/v4/channels/group", &go_list).await;
    let (_, rust_text, _) = post(&http, RUST, &token, "/api/v4/channels/group", &rust_list).await;
    let go_id = body(&go_text)["id"].as_str().expect("an id").to_owned();
    let rust_id = body(&rust_text)["id"].as_str().expect("an id").to_owned();

    let named = |id: String| {
        move |frames: &[serde_json::Value]| {
            frames.iter().any(|frame| {
                frame["event"] == "group_added" && frame["broadcast"]["channel_id"] == id.as_str()
            })
        }
    };
    go_socket
        .collect_until(Duration::from_millis(2000), named(go_id.clone()))
        .await;
    rust_socket
        .collect_until(Duration::from_millis(2000), named(rust_id.clone()))
        .await;

    // This socket belongs to `me`, and the event is addressed per-member, so exactly one of the
    // three publishes reaches it. A port that addressed the event to the channel instead would
    // still deliver one frame here — which is why the `broadcast` is compared too.
    let go_events: Vec<_> = go_socket
        .events_named("group_added")
        .into_iter()
        .filter(|frame| frame["broadcast"]["channel_id"] == go_id.as_str())
        .collect();
    let rust_events: Vec<_> = rust_socket
        .events_named("group_added")
        .into_iter()
        .filter(|frame| frame["broadcast"]["channel_id"] == rust_id.as_str())
        .collect();
    assert_eq!(
        go_events.len(),
        1,
        "Go published one to me: {:?}",
        go_socket.raw
    );
    assert_eq!(
        rust_events.len(),
        1,
        "we published one to me: {:?}",
        rust_socket.raw
    );
    assert_eq!(rust_events[0]["broadcast"]["user_id"], me);
    assert_eq!(go_events[0]["broadcast"]["user_id"], me);
    assert_eq!(rust_events[0]["broadcast"]["team_id"], "");
    assert_eq!(rust_events[0]["broadcast"]["omit_connection_id"], "");

    // `teammate_ids` is a JSON array inside a JSON string, sorted, and includes the caller.
    let decode = |frame: &serde_json::Value| -> Vec<String> {
        let raw = frame["data"]["teammate_ids"]
            .as_str()
            .expect("teammate_ids is a JSON string");
        serde_json::from_str(raw).expect("and it decodes to an array")
    };
    let go_ids = decode(&go_events[0]);
    let rust_ids = decode(&rust_events[0]);
    let mut go_sorted = vec![me.to_owned(), go_a.id.clone(), go_b.id.clone()];
    go_sorted.sort();
    let mut rust_sorted = vec![me.to_owned(), rs_a.id.clone(), rs_b.id.clone()];
    rust_sorted.sort();
    assert_eq!(go_ids, go_sorted, "Go sorts the ids and appends the caller");
    assert_eq!(rust_ids, rust_sorted, "so must we");

    for user in [&go_a, &go_b, &rs_a, &rs_b] {
        delete_plain_user(&http, &token, &user.id).await;
    }
}

/// `CreateChannel` trims three fields and blanks a fourth, and none of it is visible unless a
/// test sends padding.
///
/// - `display_name` is trimmed; `name`, `header` and `purpose` are **not**.
/// - `managed_category_name` comes back **empty** whatever was sent, because the feature needs an
///   enterprise licence and Go's unlicensed branch blanks the field on the answer.
#[tokio::test]
async fn the_create_trims_the_display_name_and_blanks_the_managed_category() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let (team, _) = a_team_and_channel_the_user_is_in(&http, &token).await;
    let tag = std::process::id();

    for (base, side) in [(GO, "go"), (RUST, "rs")] {
        let (status, text, _) = post(
            &http,
            base,
            &token,
            "/api/v4/channels",
            &serde_json::json!({
                "team_id": team,
                "name": format!("mmrs-trim-{side}-{tag}"),
                "display_name": "   mmrs trimmed   ",
                "header": "  padded header  ",
                "purpose": "  padded purpose  ",
                "managed_category_name": "  Zed  ",
                "type": "O",
            }),
        )
        .await;
        assert_eq!(status, 201, "{side}: the create succeeds — {text}");
        let created = body(&text);
        assert_eq!(
            created["display_name"], "mmrs trimmed",
            "{side}: display_name is trimmed"
        );
        assert_eq!(
            created["header"], "  padded header  ",
            "{side}: header is NOT trimmed"
        );
        assert_eq!(
            created["purpose"], "  padded purpose  ",
            "{side}: purpose is NOT trimmed"
        );
        assert_eq!(
            created["managed_category_name"], "",
            "{side}: an unlicensed server blanks the managed category"
        );
        common::delete_channel(&http, &token, created["id"].as_str().expect("an id")).await;
    }
}

/// A team whose members may create a **public** channel and not a private one, which is the only
/// fixture that can tell `create_public_channel` from `create_private_channel`.
///
/// # Why this needs a whole team scheme
///
/// `SessionHasPermissionToTeam` consults the team membership's roles first and falls back to the
/// session's *system* roles. On a stock installation `team_user` grants **both** create
/// permissions and `system_user` grants neither, so every ordinary member passes either check and
/// swapping the two permissions in the handler is invisible. Measured against the roles table, not
/// assumed. Narrowing the system roles would change a row every other suite reads; narrowing the
/// *team* role through a scheme attached to a throwaway team changes nothing outside it.
///
/// The mutation this exists for — `create-channel-private-gate-is-the-public-permission` —
/// SURVIVED the first run of `scripts/mutations/channel-creates.plan` for exactly that reason.
#[tokio::test]
async fn the_private_create_is_gated_on_its_own_permission() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let admin = go_minted_token(&http).await;

    let Some(role) = common::plant_role("cpubonly", "create_public_channel").await else {
        return; // no DATABASE_URL — the planted fixtures cannot be built
    };
    let Some(scheme) = common::plant_scheme("team", "cpubonly").await else {
        return;
    };
    let Some(pool) = common::fixture_pool().await else {
        return;
    };
    // `plant_scheme` writes every default role as the empty string; this is the one that decides
    // what an ordinary member of the team may do.
    sqlx::query("UPDATE schemes SET defaultteamuserrole = $2 WHERE id = $1")
        .bind(&scheme)
        .bind(&role)
        .execute(&pool)
        .await
        .expect("the scheme's team-user role is written");

    let team = create_team(&http, &admin, "cpubonly").await;
    common::set_team_scheme(&team, Some(&scheme)).await;
    // The rows were planted underneath Go, whose role and scheme caches predate them. The user is
    // created *after* the invalidation so its login hydrates the membership through the scheme.
    common::invalidate_go_caches(&http, &admin).await;
    let member = create_plain_user(&http, &admin, &team, "cpubonly").await;

    let tag = std::process::id();
    for (channel_type, expected) in [("O", 201), ("P", 403)] {
        let go_body = serde_json::json!({
            "team_id": team,
            "name": format!("mmrs-gate-go-{}-{tag}", channel_type.to_lowercase()),
            "display_name": "mmrs gate",
            "type": channel_type,
        });
        let rust_body = serde_json::json!({
            "team_id": team,
            "name": format!("mmrs-gate-rs-{}-{tag}", channel_type.to_lowercase()),
            "display_name": "mmrs gate",
            "type": channel_type,
        });
        let (go_status, go_text, _) =
            post(&http, GO, &member.token, "/api/v4/channels", &go_body).await;
        let (rust_status, rust_text, by_rust) =
            post(&http, RUST, &member.token, "/api/v4/channels", &rust_body).await;

        assert!(by_rust, "{channel_type}: this server must answer it");
        assert_eq!(
            go_status, expected,
            "{channel_type}: Go answers {expected} for a create_public_channel-only member — \
             {go_text}"
        );
        assert_eq!(
            rust_status, go_status,
            "{channel_type}: status differs\n  go: {go_text}\n  rs: {rust_text}"
        );
        if expected == 201 {
            assert_eq!(normalise(&body(&go_text)), normalise(&body(&rust_text)));
            common::delete_channel(&http, &admin, body(&go_text)["id"].as_str().expect("an id"))
                .await;
            common::delete_channel(
                &http,
                &admin,
                body(&rust_text)["id"].as_str().expect("an id"),
            )
            .await;
        } else {
            assert_error_bodies_match_except_known_gaps(
                go_text.as_bytes(),
                rust_text.as_bytes(),
                channel_type,
            );
        }
    }

    common::set_team_scheme(&team, None).await;
    delete_plain_user(&http, &admin, &member.id).await;
}
