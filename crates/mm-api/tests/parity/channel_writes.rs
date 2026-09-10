//! Cross-server parity for the five channel-lifecycle routes.
//!
//! ```sh
//! docker compose up -d && scripts/parity.sh -p mm-api --test parity channel_writes
//! ```
//!
//! Every assertion about a write this server made reads back **through this server**: Go's caches
//! do not see our writes ([D-190]), and its `channelByIdCache` is exactly the kind that would make
//! a passing test lie here.
//!
//! # Why nearly every test uses two channels
//!
//! These routes are destructive and not idempotent — a channel can only be archived once, and a
//! rename changes the value the next assertion reads. So a comparison against Go gets **a channel
//! each**, identical apart from the id and name, and the two bodies are normalised before they are
//! compared. A shared channel would let one server's write decide the other's answer.

use std::time::Duration;

use crate::common;

use common::{
    GO, RUST, SocketProbe, a_team_and_channel_the_user_is_in, client, create_channel_typed,
    create_plain_user, go_minted_token, logged_in_user_id, stack_enabled,
};

/// The channel-shaped keys that cannot agree across two different channels, blanked so the rest of
/// the body can be compared byte for byte.
///
/// `total_msg_count`, `last_post_at` and `last_root_post_at` are in the list for a reason worth
/// stating: Go's write paths create system posts and ours do not ([D-232]), so those three move on
/// Go's channel and stand still on ours. Everything *not* in this list is compared exactly, which
/// is where a wrong field name or a dropped column would show up.
fn normalise(channel: &serde_json::Value) -> serde_json::Value {
    let mut out = channel.clone();
    let object = out.as_object_mut().expect("a channel object");
    for key in ["id", "name", "display_name"] {
        object.insert(key.to_owned(), serde_json::json!("<per-channel>"));
    }
    for key in [
        "create_at",
        "update_at",
        "last_post_at",
        "last_root_post_at",
        "total_msg_count",
        "total_msg_count_root",
    ] {
        let value = object.get(key).and_then(|v| v.as_i64()).unwrap_or(0);
        object.insert(
            key.to_owned(),
            serde_json::json!(if value > 0 { "nonzero" } else { "zero" }),
        );
    }
    out
}

async fn send(
    http: &reqwest::Client,
    method: reqwest::Method,
    base: &str,
    token: &str,
    path: &str,
    body: Option<&serde_json::Value>,
) -> (u16, String, bool) {
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
    let by_rust = response
        .headers()
        .get("x-mmrs-served-by")
        .and_then(|value| value.to_str().ok())
        == Some("rust");
    (status, response.text().await.expect("a body"), by_rust)
}

async fn put_channel(
    http: &reqwest::Client,
    base: &str,
    token: &str,
    channel_id: &str,
    body: &serde_json::Value,
) -> (u16, String, bool) {
    send(
        http,
        reqwest::Method::PUT,
        base,
        token,
        &format!("/api/v4/channels/{channel_id}"),
        Some(body),
    )
    .await
}

async fn patch(
    http: &reqwest::Client,
    base: &str,
    token: &str,
    channel_id: &str,
    body: &serde_json::Value,
) -> (u16, String, bool) {
    send(
        http,
        reqwest::Method::PUT,
        base,
        token,
        &format!("/api/v4/channels/{channel_id}/patch"),
        Some(body),
    )
    .await
}

async fn set_privacy(
    http: &reqwest::Client,
    base: &str,
    token: &str,
    channel_id: &str,
    body: &serde_json::Value,
) -> (u16, String, bool) {
    send(
        http,
        reqwest::Method::PUT,
        base,
        token,
        &format!("/api/v4/channels/{channel_id}/privacy"),
        Some(body),
    )
    .await
}

async fn archive(
    http: &reqwest::Client,
    base: &str,
    token: &str,
    channel_id: &str,
    query: &str,
) -> (u16, String, bool) {
    send(
        http,
        reqwest::Method::DELETE,
        base,
        token,
        &format!("/api/v4/channels/{channel_id}{query}"),
        None,
    )
    .await
}

async fn restore(
    http: &reqwest::Client,
    base: &str,
    token: &str,
    channel_id: &str,
) -> (u16, String, bool) {
    send(
        http,
        reqwest::Method::POST,
        base,
        token,
        &format!("/api/v4/channels/{channel_id}/restore"),
        None,
    )
    .await
}

/// Read a channel back through **the server that wrote it** — see [D-190].
async fn read_channel(
    http: &reqwest::Client,
    base: &str,
    token: &str,
    channel_id: &str,
) -> serde_json::Value {
    let (status, body, _) = send(
        http,
        reqwest::Method::GET,
        base,
        token,
        &format!("/api/v4/channels/{channel_id}"),
        None,
    )
    .await;
    assert_eq!(
        status, 200,
        "reading {channel_id} back through {base}: {body}"
    );
    serde_json::from_str(&body).expect("a channel")
}

fn error_id(body: &str) -> String {
    let value: serde_json::Value =
        serde_json::from_str(body).unwrap_or_else(|e| panic!("not an AppError: {body} ({e})"));
    value["id"].as_str().unwrap_or_default().to_owned()
}

// -------------------------------------------------------------------------------------------
// PUT /channels/{id}
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn updating_a_channel_agrees_with_go_field_for_field() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let (team, _) = a_team_and_channel_the_user_is_in(&http, &token).await;
    let go_channel = create_channel_typed(&http, &token, &team, "cwupdg", "O").await;
    let rust_channel = create_channel_typed(&http, &token, &team, "cwupdr", "O").await;

    // Every field the route ignores is set to something absurd, so a port that honoured any of
    // them diverges immediately rather than at some later read.
    let body = |id: &str| {
        serde_json::json!({
            "id": id,
            "header": "mmrs header",
            "purpose": "mmrs purpose",
            "display_name": "mmrs renamed",
            "create_at": 1,
            "update_at": 1,
            "delete_at": 0,
            "total_msg_count": 9999,
            "total_msg_count_root": 9999,
            "last_post_at": 42,
            "last_root_post_at": 42,
            "extra_update_at": 7,
            "creator_id": "zzzzzzzzzzzzzzzzzzzzzzzzzz",
            "scheme_id": "yyyyyyyyyyyyyyyyyyyyyyyyyy",
            "default_category_name": "Nope",
            "discoverable": true,
            "autotranslation": true,
            "shared": true,
            "props": {"mmrs": "nope"},
        })
    };

    let (go_status, go_body, _) =
        put_channel(&http, GO, &token, &go_channel, &body(&go_channel)).await;
    let (rust_status, rust_body, by_rust) =
        put_channel(&http, RUST, &token, &rust_channel, &body(&rust_channel)).await;
    assert_eq!(go_status, 200, "Go's update: {go_body}");
    assert_eq!(
        rust_status, go_status,
        "the update status differs: {rust_body}"
    );
    assert!(by_rust, "this server should have served the update");
    assert!(
        go_body.ends_with('\n') && rust_body.ends_with('\n'),
        "both bodies are encoder-framed:\n go: {go_body:?}\nrust: {rust_body:?}"
    );

    let go: serde_json::Value = serde_json::from_str(&go_body).expect("a channel");
    let rust: serde_json::Value = serde_json::from_str(&rust_body).expect("a channel");
    assert_eq!(
        normalise(&go),
        normalise(&rust),
        "the updated channel differs:\n go: {go}\nrust: {rust}"
    );

    // The five honoured fields.
    assert_eq!(rust["header"], "mmrs header");
    assert_eq!(rust["purpose"], "mmrs purpose");
    assert_eq!(rust["display_name"], "mmrs renamed");
    // And the ignored ones, spelled out so a regression names the field.
    assert_eq!(
        rust["creator_id"], go["creator_id"],
        "creator_id is ignored"
    );
    assert!(rust["scheme_id"].is_null(), "scheme_id is ignored: {rust}");
    assert_eq!(
        rust["default_category_name"], "",
        "default_category_name is ignored by this route"
    );
    assert_eq!(rust["discoverable"], false, "discoverable is ignored");
    assert_eq!(rust["autotranslation"], false, "autotranslation is ignored");
    assert!(rust["shared"].is_null(), "shared is ignored: {rust}");
    assert!(
        rust["props"].is_null(),
        "updateChannel does not FillInChannelProps"
    );
    assert!(
        rust["create_at"].as_i64().unwrap_or(0) > 1,
        "create_at kept its real value: {rust}"
    );
    assert_eq!(rust["total_msg_count"], 0, "total_msg_count is ignored");

    // And the row really carries what the answer claimed, read back through this server.
    let stored = read_channel(&http, RUST, &token, &rust_channel).await;
    assert_eq!(stored["header"], "mmrs header");
    assert_eq!(stored["display_name"], "mmrs renamed");
    assert_eq!(stored["update_at"], rust["update_at"]);

    common::delete_channel(&http, &token, &go_channel).await;
    common::delete_channel(&http, &token, &rust_channel).await;
}

/// `header` and `purpose` are copied unconditionally; `display_name` and `name` only when
/// non-empty. So one request both clears the header and leaves the display name alone.
#[tokio::test]
async fn an_empty_header_clears_and_an_empty_display_name_does_not() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let (team, _) = a_team_and_channel_the_user_is_in(&http, &token).await;
    let go_channel = create_channel_typed(&http, &token, &team, "cwemptyg", "O").await;
    let rust_channel = create_channel_typed(&http, &token, &team, "cwemptyr", "O").await;

    for (base, channel) in [(GO, &go_channel), (RUST, &rust_channel)] {
        // Give it a header and a purpose first.
        let (status, body, _) = put_channel(
            &http,
            base,
            &token,
            channel,
            &serde_json::json!({"id": channel, "header": "h", "purpose": "p"}),
        )
        .await;
        assert_eq!(status, 200, "{base} first update: {body}");

        let (status, body, _) = put_channel(
            &http,
            base,
            &token,
            channel,
            &serde_json::json!({"id": channel, "header": "", "purpose": "", "display_name": "", "name": ""}),
        )
        .await;
        assert_eq!(status, 200, "{base} second update: {body}");
        let channel_body: serde_json::Value = serde_json::from_str(&body).expect("a channel");
        assert_eq!(channel_body["header"], "", "{base} cleared the header");
        assert_eq!(channel_body["purpose"], "", "{base} cleared the purpose");
        assert_ne!(
            channel_body["display_name"], "",
            "{base} must NOT clear the display name: {body}"
        );
        assert_ne!(
            channel_body["name"], "",
            "{base} must NOT clear the name: {body}"
        );
    }

    common::delete_channel(&http, &token, &go_channel).await;
    common::delete_channel(&http, &token, &rust_channel).await;
}

#[tokio::test]
async fn the_body_id_must_match_the_path_and_a_bad_body_names_channel() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let (team, _) = a_team_and_channel_the_user_is_in(&http, &token).await;
    let channel = create_channel_typed(&http, &token, &team, "cwbadbody", "O").await;

    let cases: [(&str, serde_json::Value); 2] = [
        // A mismatched id is `channel_id`, not `channel` — a different `Name` in the params, and
        // the same id and status, so only the translated message would tell them apart.
        (
            "mismatch",
            serde_json::json!({"id": "aaaaaaaaaaaaaaaaaaaaaaaaaa"}),
        ),
        // An omitted id lands on the same check, because `""` is not the path's id.
        ("no id", serde_json::json!({"header": "x"})),
    ];
    for (label, body) in cases {
        let (go_status, go_body, _) = put_channel(&http, GO, &token, &channel, &body).await;
        let (rust_status, rust_body, _) = put_channel(&http, RUST, &token, &channel, &body).await;
        assert_eq!(go_status, 400, "{label}: Go refuses: {go_body}");
        assert_eq!(
            rust_status, go_status,
            "{label}: status differs: {rust_body}"
        );
        common::assert_error_bodies_match_except_known_gaps(
            go_body.as_bytes(),
            rust_body.as_bytes(),
            label,
        );
    }

    // A body that is not an object at all, and the JSON literal `null`, both land on `channel`.
    for raw in ["not json", "null", "[]"] {
        let go = http
            .put(format!("{GO}/api/v4/channels/{channel}"))
            .header("Authorization", format!("Bearer {token}"))
            .header("Content-Type", "application/json")
            .body(raw)
            .send()
            .await
            .expect("Go answers");
        let go_status = go.status().as_u16();
        let go_body = go.text().await.expect("a body");
        let rust = http
            .put(format!("{RUST}/api/v4/channels/{channel}"))
            .header("Authorization", format!("Bearer {token}"))
            .header("Content-Type", "application/json")
            .body(raw)
            .send()
            .await
            .expect("we answer");
        let rust_status = rust.status().as_u16();
        let rust_body = rust.text().await.expect("a body");
        assert_eq!(go_status, 400, "{raw:?}: Go refuses: {go_body}");
        assert_eq!(
            rust_status, go_status,
            "{raw:?}: status differs: {rust_body}"
        );
        common::assert_error_bodies_match_except_known_gaps(
            go_body.as_bytes(),
            rust_body.as_bytes(),
            raw,
        );
    }

    common::delete_channel(&http, &token, &channel).await;
}

/// A name another channel in the team already holds is a **400** from both routes, carrying the
/// *save* id — `store.sql_channel.save_channel.exists.app_error`, which reads wrong for an update
/// and is what Go sends. It comes from the `channels_name_teamid_key` unique constraint, so it is
/// the one error on these paths that the database rather than a guard produces.
#[tokio::test]
async fn a_duplicate_channel_name_is_the_save_exists_error_from_both_routes() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let (team, _) = a_team_and_channel_the_user_is_in(&http, &token).await;
    let taken = create_channel_typed(&http, &token, &team, "cwduptaken", "O").await;
    let renaming = create_channel_typed(&http, &token, &team, "cwduprename", "O").await;
    let taken_name = read_channel(&http, RUST, &token, &taken).await["name"]
        .as_str()
        .expect("a name")
        .to_owned();

    let (go_status, go_body, _) = put_channel(
        &http,
        GO,
        &token,
        &renaming,
        &serde_json::json!({"id": renaming, "name": taken_name}),
    )
    .await;
    let (rust_status, rust_body, _) = put_channel(
        &http,
        RUST,
        &token,
        &renaming,
        &serde_json::json!({"id": renaming, "name": taken_name}),
    )
    .await;
    assert_eq!(go_status, 400, "Go refuses a duplicate name: {go_body}");
    assert_eq!(rust_status, go_status, "{rust_body}");
    assert_eq!(
        error_id(&rust_body),
        "store.sql_channel.save_channel.exists.app_error"
    );
    common::assert_error_bodies_match_except_known_gaps(
        go_body.as_bytes(),
        rust_body.as_bytes(),
        "duplicate name via update",
    );

    let (go_status, go_body, _) = patch(
        &http,
        GO,
        &token,
        &renaming,
        &serde_json::json!({"name": taken_name}),
    )
    .await;
    let (rust_status, rust_body, _) = patch(
        &http,
        RUST,
        &token,
        &renaming,
        &serde_json::json!({"name": taken_name}),
    )
    .await;
    assert_eq!(go_status, 400, "Go: {go_body}");
    assert_eq!(rust_status, go_status, "{rust_body}");
    assert_eq!(
        error_id(&rust_body),
        "store.sql_channel.save_channel.exists.app_error"
    );
    common::assert_error_bodies_match_except_known_gaps(
        go_body.as_bytes(),
        rust_body.as_bytes(),
        "duplicate name via patch",
    );

    common::delete_channel(&http, &token, &taken).await;
    common::delete_channel(&http, &token, &renaming).await;
}

#[tokio::test]
async fn a_type_change_through_update_is_refused_and_privacy_is_the_only_way() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let (team, _) = a_team_and_channel_the_user_is_in(&http, &token).await;
    let channel = create_channel_typed(&http, &token, &team, "cwtypech", "O").await;

    let body = serde_json::json!({"id": channel, "type": "P"});
    let (go_status, go_body, _) = put_channel(&http, GO, &token, &channel, &body).await;
    let (rust_status, rust_body, _) = put_channel(&http, RUST, &token, &channel, &body).await;
    assert_eq!(go_status, 400, "Go refuses a type change: {go_body}");
    assert_eq!(rust_status, go_status);
    assert_eq!(
        error_id(&rust_body),
        "api.channel.update_channel.typechange.app_error"
    );
    common::assert_error_bodies_match_except_known_gaps(
        go_body.as_bytes(),
        rust_body.as_bytes(),
        "typechange",
    );

    // The *same* type is not a change, so it is accepted and only `update_at` moves.
    let same = serde_json::json!({"id": channel, "type": "O"});
    let (status, body, _) = put_channel(&http, RUST, &token, &channel, &same).await;
    assert_eq!(status, 200, "the same type is not a change: {body}");

    common::delete_channel(&http, &token, &channel).await;
}

/// `json.Decoder.Decode` reads one value and **stops**, so trailing bytes are never looked at.
/// Three bodies Go accepts and a strict decoder would 400 — the difference between
/// `serde_json::from_slice` and `mm_model::utils::decode_one_from_json`, and the reason all three
/// handlers use the latter.
#[tokio::test]
async fn trailing_bytes_after_the_body_are_accepted_by_both_servers() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let (team, _) = a_team_and_channel_the_user_is_in(&http, &token).await;
    let go_channel = create_channel_typed(&http, &token, &team, "cwtrailg", "O").await;
    let rust_channel = create_channel_typed(&http, &token, &team, "cwtrailr", "O").await;

    let cases: [(&str, &str); 3] = [
        ("", "trailing"),
        ("/patch", "trailing"),
        ("/privacy", "{\"privacy\":\"O\"}"),
    ];
    for (base, channel) in [(GO, &go_channel), (RUST, &rust_channel)] {
        for (suffix, garbage) in cases {
            let body = match suffix {
                "/patch" => format!("{{\"header\":\"mmrs trailing\"}}{garbage}"),
                "/privacy" => format!("{{\"privacy\":\"P\"}}{garbage}"),
                _ => format!("{{\"id\":\"{channel}\",\"header\":\"mmrs trailing\"}}{garbage}"),
            };
            let response = http
                .put(format!("{base}/api/v4/channels/{channel}{suffix}"))
                .header("Authorization", format!("Bearer {token}"))
                .header("Content-Type", "application/json")
                .body(body.clone())
                .send()
                .await
                .expect("the server answers");
            let status = response.status().as_u16();
            let text = response.text().await.expect("a body");
            assert_eq!(
                status, 200,
                "{base}{suffix} must accept {body:?}, not 400 it: {text}"
            );
            // Put the type back so the next iteration starts from `O` again.
            if suffix == "/privacy" {
                let (status, body, _) = set_privacy(
                    &http,
                    base,
                    &token,
                    channel,
                    &serde_json::json!({"privacy": "O"}),
                )
                .await;
                assert_eq!(status, 200, "{base} converts back: {body}");
            }
        }
    }

    common::delete_channel(&http, &token, &go_channel).await;
    common::delete_channel(&http, &token, &rust_channel).await;
}

// -------------------------------------------------------------------------------------------
// town-square
// -------------------------------------------------------------------------------------------

/// `town-square` is refused a rename by both routes, an archive, and a conversion to private —
/// four guards, one name. **`off-topic` has none of them**, which is the half of this test that
/// pins a port that guessed at a second default channel.
///
/// # The team is this test's own, and that is a mutation-testing requirement
///
/// Every request here is expected to *fail*, and the only thing stopping it is the guard under
/// test. So a mutation that removes the guard makes the request **succeed** — and against the
/// shared fixture team that means archiving its `town-square` for good, or renaming its
/// `off-topic`, poisoning every later test in the batch. Both happened: the first mutation run
/// left the fixture team's town-square archived and its off-topic renamed and private, and the two
/// no-op controls at the end of the plan were then CAUGHT by the wreckage rather than by any
/// change. A disposable team makes the damage local to the mutation that caused it.
#[tokio::test]
async fn only_town_square_is_special_and_off_topic_is_not() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let team = common::create_team(&http, &token, "cwtsguards").await;

    let town_square = channel_named(&http, &token, &team, "town-square").await;

    let cases: Vec<(&str, u16, String, Option<serde_json::Value>, &str)> = vec![
        (
            "rename via update",
            400,
            format!("/api/v4/channels/{town_square}"),
            Some(serde_json::json!({"id": town_square, "name": "mmrsnottownsquare"})),
            "api.channel.update_channel.tried.app_error",
        ),
        (
            "rename via patch",
            400,
            format!("/api/v4/channels/{town_square}/patch"),
            Some(serde_json::json!({"name": "mmrsnottownsquare"})),
            "api.channel.update_channel.tried.app_error",
        ),
        (
            "convert to private",
            400,
            format!("/api/v4/channels/{town_square}/privacy"),
            Some(serde_json::json!({"privacy": "P"})),
            "api.channel.update_channel_privacy.default_channel_error",
        ),
    ];
    for (label, expected, path, body, id) in cases {
        let (go_status, go_body, _) = send(
            &http,
            reqwest::Method::PUT,
            GO,
            &token,
            &path,
            body.as_ref(),
        )
        .await;
        let (rust_status, rust_body, _) = send(
            &http,
            reqwest::Method::PUT,
            RUST,
            &token,
            &path,
            body.as_ref(),
        )
        .await;
        assert_eq!(go_status, expected, "{label}: Go: {go_body}");
        assert_eq!(
            rust_status, go_status,
            "{label}: status differs: {rust_body}"
        );
        assert_eq!(error_id(&rust_body), id, "{label}: our id is wrong");
        common::assert_error_bodies_match_except_known_gaps(
            go_body.as_bytes(),
            rust_body.as_bytes(),
            label,
        );
    }

    // The archive guard lives in `App.DeleteChannel`, not the handler, and it is *after* the
    // permission check — so it is the same 400 from both servers.
    let (go_status, go_body, _) = archive(&http, GO, &token, &town_square, "").await;
    let (rust_status, rust_body, _) = archive(&http, RUST, &token, &town_square, "").await;
    assert_eq!(
        go_status, 400,
        "Go refuses to archive town-square: {go_body}"
    );
    assert_eq!(rust_status, go_status, "{rust_body}");
    assert_eq!(
        error_id(&rust_body),
        "api.channel.delete_channel.cannot.app_error"
    );
    common::assert_error_bodies_match_except_known_gaps(
        go_body.as_bytes(),
        rust_body.as_bytes(),
        "archive town-square",
    );

    // And a rename of `town-square` to itself is not a rename, so it is a 200.
    let (status, body, _) = patch(
        &http,
        RUST,
        &token,
        &town_square,
        &serde_json::json!({"name": "town-square"}),
    )
    .await;
    assert_eq!(
        status, 200,
        "renaming town-square to itself is allowed: {body}"
    );
}

/// `off-topic` gets renamed and converted, on **one** server, because what is being tested is the
/// *absence* of a guard rather than an agreement between two answers.
///
/// On this test's own team, so nothing needs putting back: renaming a shared `off-topic` and
/// restoring it at the end works only while the test passes, and the whole point of a mutation run
/// is that it does not. See the note on the town-square test.
#[tokio::test]
async fn off_topic_can_be_renamed_archived_and_made_private() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let team = common::create_team(&http, &token, "cwotguards").await;
    let off_topic = channel_named(&http, &token, &team, "off-topic").await;

    let (status, body, by_rust) = patch(
        &http,
        RUST,
        &token,
        &off_topic,
        &serde_json::json!({"name": "mmrsrenamedofftopic"}),
    )
    .await;
    assert_eq!(status, 200, "off-topic has no rename guard: {body}");
    assert!(by_rust);

    let (status, body, _) = set_privacy(
        &http,
        RUST,
        &token,
        &off_topic,
        &serde_json::json!({"privacy": "P"}),
    )
    .await;
    assert_eq!(status, 200, "off-topic can be made private: {body}");
    let converted: serde_json::Value = serde_json::from_str(&body).expect("a channel");
    assert_eq!(converted["type"], "P");

    // Both round trips, which assert something the one-way conversion does not: that a default
    // channel's *name* can be taken back and its type flipped again. Not cleanup — the team is
    // disposable — so a failure here is a finding rather than a leak.
    let (status, _, _) = set_privacy(
        &http,
        RUST,
        &token,
        &off_topic,
        &serde_json::json!({"privacy": "O"}),
    )
    .await;
    assert_eq!(status, 200);
    let (status, _, _) = patch(
        &http,
        RUST,
        &token,
        &off_topic,
        &serde_json::json!({"name": "off-topic"}),
    )
    .await;
    assert_eq!(status, 200, "off-topic's name can be taken back");

    // And it archives, which `town-square` does not.
    let (status, body, by_rust) = archive(&http, RUST, &token, &off_topic, "").await;
    assert_eq!(status, 200, "off-topic has no archive guard: {body}");
    assert!(by_rust);
}

async fn channel_named(http: &reqwest::Client, token: &str, team: &str, name: &str) -> String {
    let (status, body, _) = send(
        http,
        reqwest::Method::GET,
        GO,
        token,
        &format!("/api/v4/teams/{team}/channels/name/{name}"),
        None,
    )
    .await;
    assert_eq!(status, 200, "{name} must exist in the fixture team: {body}");
    let channel: serde_json::Value = serde_json::from_str(&body).expect("a channel");
    channel["id"].as_str().expect("an id").to_owned()
}

// -------------------------------------------------------------------------------------------
// PUT /channels/{id}/patch
// -------------------------------------------------------------------------------------------

/// The four fields whose answer is decided without touching the channel, and the one that is
/// accepted and does nothing.
#[tokio::test]
async fn the_patch_gates_answer_the_same_five_ways() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let (team, _) = a_team_and_channel_the_user_is_in(&http, &token).await;
    let channel = create_channel_typed(&http, &token, &team, "cwgates", "O").await;

    let refusals: [(&str, serde_json::Value, u16, &str); 4] = [
        (
            "no changes",
            serde_json::json!({}),
            400,
            "api.channel.patch_update_channel.no_changes.app_error",
        ),
        (
            "autotranslation",
            serde_json::json!({"autotranslation": true}),
            403,
            "api.channel.patch_update_channel.feature_not_available.app_error",
        ),
        (
            "discoverable",
            serde_json::json!({"discoverable": true}),
            400,
            "api.channel.discoverable_join_request.feature_disabled.app_error",
        ),
        (
            "banner_info",
            serde_json::json!({"banner_info": {"enabled": true, "text": "hi", "background_color": "#ffffff"}}),
            403,
            "license_error.feature_unavailable.specific",
        ),
    ];
    for (label, body, expected, id) in refusals {
        let (go_status, go_body, _) = patch(&http, GO, &token, &channel, &body).await;
        let (rust_status, rust_body, _) = patch(&http, RUST, &token, &channel, &body).await;
        assert_eq!(go_status, expected, "{label}: Go: {go_body}");
        assert_eq!(
            rust_status, go_status,
            "{label}: status differs: {rust_body}"
        );
        assert_eq!(error_id(&rust_body), id, "{label}: our id is wrong");
        common::assert_error_bodies_match_except_known_gaps(
            go_body.as_bytes(),
            rust_body.as_bytes(),
            label,
        );
    }

    // `managed_category_name` is the silent one: a 200 that changes nothing but `update_at`. It is
    // enough on its own to get past the "no changes" gate, which is the part a port would drop.
    let before = read_channel(&http, RUST, &token, &channel).await;
    let (status, body, by_rust) = patch(
        &http,
        RUST,
        &token,
        &channel,
        &serde_json::json!({"managed_category_name": "mmrs ignored"}),
    )
    .await;
    assert_eq!(
        status, 200,
        "managed_category_name alone is accepted: {body}"
    );
    assert!(by_rust);
    let after: serde_json::Value = serde_json::from_str(&body).expect("a channel");
    assert_eq!(
        after["managed_category_name"], "",
        "the field is accepted and never applied: {body}"
    );
    assert!(
        after["update_at"].as_i64() > before["update_at"].as_i64(),
        "the row was rewritten even though nothing changed"
    );

    common::delete_channel(&http, &token, &channel).await;
}

/// `patchChannel` calls `FillInChannelProps` and `updateChannel` does not, so the same header
/// answers with and without `props.channel_mentions` depending on the route.
#[tokio::test]
async fn only_the_patch_route_fills_in_channel_mentions() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let (team, _) = a_team_and_channel_the_user_is_in(&http, &token).await;
    let go_channel = create_channel_typed(&http, &token, &team, "cwpropsg", "O").await;
    let rust_channel = create_channel_typed(&http, &token, &team, "cwpropsr", "O").await;

    let header = serde_json::json!({"header": "look at ~town-square"});
    let (go_status, go_body, _) = patch(&http, GO, &token, &go_channel, &header).await;
    let (rust_status, rust_body, _) = patch(&http, RUST, &token, &rust_channel, &header).await;
    assert_eq!(go_status, 200, "Go's patch: {go_body}");
    assert_eq!(rust_status, go_status, "{rust_body}");

    let go: serde_json::Value = serde_json::from_str(&go_body).expect("a channel");
    let rust: serde_json::Value = serde_json::from_str(&rust_body).expect("a channel");
    assert_eq!(
        go["props"]["channel_mentions"]["town-square"]["display_name"], "Town Square",
        "Go resolves the mention: {go_body}"
    );
    assert_eq!(
        normalise(&go),
        normalise(&rust),
        "the patched channel differs:\n go: {go}\nrust: {rust}"
    );

    // The same header through `updateChannel` answers `props: null` — measured on Go.
    let (status, body, _) = put_channel(
        &http,
        RUST,
        &token,
        &rust_channel,
        &serde_json::json!({"id": rust_channel, "header": "look at ~town-square"}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let updated: serde_json::Value = serde_json::from_str(&body).expect("a channel");
    assert!(
        updated["props"].is_null(),
        "updateChannel leaves props null even with a live mention: {body}"
    );

    common::delete_channel(&http, &token, &go_channel).await;
    common::delete_channel(&http, &token, &rust_channel).await;
}

/// The two branches this server hands to Go. Neither may answer with our header, and both must
/// still have taken effect — a forward that silently dropped the request would pass a status check.
#[tokio::test]
async fn the_two_unowned_patch_branches_are_forwarded_and_still_applied() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let (team, _) = a_team_and_channel_the_user_is_in(&http, &token).await;
    let category = create_channel_typed(&http, &token, &team, "cwfwdcat", "O").await;
    let constrained = create_channel_typed(&http, &token, &team, "cwfwdgc", "O").await;

    let (status, body, by_rust) = patch(
        &http,
        RUST,
        &token,
        &category,
        &serde_json::json!({"default_category_name": "Mmrs Forwarded"}),
    )
    .await;
    assert_eq!(status, 200, "the forwarded patch still succeeds: {body}");
    assert!(
        !by_rust,
        "a default_category_name patch must go to Go, not be served here: {body}"
    );
    let answered: serde_json::Value = serde_json::from_str(&body).expect("a channel");
    assert_eq!(answered["default_category_name"], "Mmrs Forwarded");

    let (status, body, by_rust) = patch(
        &http,
        RUST,
        &token,
        &constrained,
        &serde_json::json!({"group_constrained": true}),
    )
    .await;
    assert_eq!(status, 200, "the forwarded patch still succeeds: {body}");
    assert!(
        !by_rust,
        "turning group_constrained on must go to Go: {body}"
    );
    let answered: serde_json::Value = serde_json::from_str(&body).expect("a channel");
    assert_eq!(answered["group_constrained"], true);

    // Turning it **off** again is ours: only the off→on edge writes memberships.
    let (status, body, by_rust) = patch(
        &http,
        RUST,
        &token,
        &constrained,
        &serde_json::json!({"group_constrained": false}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(
        by_rust,
        "turning group_constrained off is served here: {body}"
    );

    common::delete_channel(&http, &token, &category).await;
    common::delete_channel(&http, &token, &constrained).await;
}

/// An archived channel is refused by both routes, with **different ids** — the handler's guard on
/// one and the store's on the other. This is the single most likely place for a port to
/// accidentally agree with itself and disagree with Go.
#[tokio::test]
async fn an_archived_channel_is_refused_by_update_and_patch_with_different_ids() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let (team, _) = a_team_and_channel_the_user_is_in(&http, &token).await;
    let go_channel = create_channel_typed(&http, &token, &team, "cwarchg", "O").await;
    let rust_channel = create_channel_typed(&http, &token, &team, "cwarchr", "O").await;
    for (base, channel) in [(GO, &go_channel), (RUST, &rust_channel)] {
        let (status, body, _) = archive(&http, base, &token, channel, "").await;
        assert_eq!(status, 200, "{base} archived the fixture: {body}");
    }

    let (go_status, go_body, _) = put_channel(
        &http,
        GO,
        &token,
        &go_channel,
        &serde_json::json!({"id": go_channel, "header": "x"}),
    )
    .await;
    let (rust_status, rust_body, _) = put_channel(
        &http,
        RUST,
        &token,
        &rust_channel,
        &serde_json::json!({"id": rust_channel, "header": "x"}),
    )
    .await;
    assert_eq!(go_status, 400, "Go: {go_body}");
    assert_eq!(rust_status, go_status, "{rust_body}");
    assert_eq!(
        error_id(&rust_body),
        "api.channel.update_channel.deleted.app_error",
        "updateChannel has its own guard"
    );
    assert_eq!(error_id(&go_body), error_id(&rust_body));

    let (go_status, go_body, _) = patch(
        &http,
        GO,
        &token,
        &go_channel,
        &serde_json::json!({"header": "x"}),
    )
    .await;
    let (rust_status, rust_body, _) = patch(
        &http,
        RUST,
        &token,
        &rust_channel,
        &serde_json::json!({"header": "x"}),
    )
    .await;
    assert_eq!(go_status, 400, "Go: {go_body}");
    assert_eq!(rust_status, go_status, "{rust_body}");
    assert_eq!(
        error_id(&rust_body),
        "app.channel.update.bad_id",
        "patchChannel has no guard of its own; the store's `DeleteAt != 0` fires"
    );
    assert_eq!(error_id(&go_body), error_id(&rust_body));
}

// -------------------------------------------------------------------------------------------
// DELETE /channels/{id} and POST /channels/{id}/restore
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn archiving_then_restoring_a_channel_agrees_and_is_refused_the_second_time() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let (team, _) = a_team_and_channel_the_user_is_in(&http, &token).await;
    let go_channel = create_channel_typed(&http, &token, &team, "cwlifeg", "O").await;
    let rust_channel = create_channel_typed(&http, &token, &team, "cwlifer", "O").await;

    for (base, channel) in [(GO, &go_channel), (RUST, &rust_channel)] {
        let (status, body, _) = archive(&http, base, &token, channel, "").await;
        assert_eq!(status, 200, "{base} archives: {body}");
        // `ReturnStatusOK`, written with `w.Write` — **no trailing newline**, unlike the four
        // routes that encode a channel.
        assert_eq!(body, r#"{"status":"OK"}"#, "{base}'s archive body");

        let (status, body, _) = archive(&http, base, &token, channel, "").await;
        assert_eq!(status, 400, "{base} refuses a second archive: {body}");
        assert_eq!(
            error_id(&body),
            "api.channel.delete_channel.deleted.app_error"
        );
    }

    // The archived channel carries a non-zero `delete_at`, read back through the writing server.
    let stored = read_channel(&http, RUST, &token, &rust_channel).await;
    assert!(
        stored["delete_at"].as_i64().unwrap_or(0) > 0,
        "our archive really wrote delete_at: {stored}"
    );
    assert_eq!(
        stored["delete_at"], stored["update_at"],
        "`Delete` is `SetDeleteAt(id, time, time)` — one millisecond in both columns"
    );

    // **`SetDeleteAt` writes both tables.** The team's public listing reads `PublicChannels`, so an
    // archive that touched only `Channels` would leave the channel in every "public channels in
    // this team" answer — invisible to any assertion about the channel itself. Measured on Go too:
    // it leaves the listing on archive and comes back on restore.
    assert!(
        !public_channel_ids(&http, RUST, &token, &team)
            .await
            .contains(&rust_channel),
        "an archived channel must leave the public listing"
    );

    for (base, channel) in [(GO, &go_channel), (RUST, &rust_channel)] {
        let (status, body, _) = restore(&http, base, &token, channel).await;
        assert_eq!(status, 200, "{base} restores: {body}");
        assert!(
            body.ends_with('\n'),
            "{base}'s restore body is encoder-framed"
        );
        let restored: serde_json::Value = serde_json::from_str(&body).expect("a channel");
        assert_eq!(restored["delete_at"], 0, "{base} zeroed delete_at");

        let (status, body, _) = restore(&http, base, &token, channel).await;
        assert_eq!(status, 400, "{base} refuses a second restore: {body}");
        assert_eq!(
            error_id(&body),
            "api.channel.restore_channel.restored.app_error"
        );
    }

    // And the restored row's `update_at` moved while `delete_at` went to zero — a port that wrote
    // `SetDeleteAt(id, 0, 0)` would pass every assertion above this one.
    let stored = read_channel(&http, RUST, &token, &rust_channel).await;
    assert_eq!(stored["delete_at"], 0);
    assert!(
        stored["update_at"].as_i64().unwrap_or(0) > 0,
        "restore sets UpdateAt to now, not to zero: {stored}"
    );
    assert!(
        public_channel_ids(&http, RUST, &token, &team)
            .await
            .contains(&rust_channel),
        "and a restored channel comes back to it"
    );

    common::delete_channel(&http, &token, &go_channel).await;
    common::delete_channel(&http, &token, &rust_channel).await;
}

/// Archiving takes the channel's live webhooks with it, at a timestamp **after** the channel's own
/// — two `GetMillis()` calls, not one.
#[tokio::test]
async fn archiving_a_channel_archives_its_webhooks_a_moment_later() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let (team, _) = a_team_and_channel_the_user_is_in(&http, &token).await;
    let channel = create_channel_typed(&http, &token, &team, "cwhooks", "O").await;

    let (status, body, _) = send(
        &http,
        reqwest::Method::POST,
        GO,
        &token,
        "/api/v4/hooks/incoming",
        Some(&serde_json::json!({"channel_id": channel, "display_name": "mmrs parity hook"})),
    )
    .await;
    assert_eq!(status, 201, "the incoming hook is created: {body}");
    let hook: serde_json::Value = serde_json::from_str(&body).expect("a hook");
    let hook_id = hook["id"].as_str().expect("an id").to_owned();

    // An outgoing hook as well, because the two loops are separate code and separate store methods
    // — a port that archived only the incoming ones passes every assertion about the other half.
    let (status, body, _) = send(
        &http,
        reqwest::Method::POST,
        GO,
        &token,
        "/api/v4/hooks/outgoing",
        Some(&serde_json::json!({
            "team_id": team,
            "channel_id": channel,
            "display_name": "mmrs parity outgoing",
            "trigger_words": ["mmrsparitytrigger"],
            "callback_urls": ["http://127.0.0.1:9/mmrs"],
        })),
    )
    .await;
    assert_eq!(status, 201, "the outgoing hook is created: {body}");
    let outgoing: serde_json::Value = serde_json::from_str(&body).expect("a hook");
    let outgoing_id = outgoing["id"].as_str().expect("an id").to_owned();

    let (status, body, by_rust) = archive(&http, RUST, &token, &channel, "").await;
    assert_eq!(status, 200, "our archive: {body}");
    assert!(by_rust);

    // Read the hook back through **Go**, which is the only server with a single-hook route — the
    // row is what is being asserted, and both servers share the table. Go's webhook cache is keyed
    // by id and the archive did not touch it, so this is a live read.
    let (status, body, _) = send(
        &http,
        reqwest::Method::GET,
        GO,
        &token,
        &format!("/api/v4/hooks/incoming/{hook_id}"),
        None,
    )
    .await;
    assert_eq!(
        status, 404,
        "archiving the channel must archive its incoming hook: {body}"
    );

    let (status, body, _) = send(
        &http,
        reqwest::Method::GET,
        GO,
        &token,
        &format!("/api/v4/hooks/outgoing/{outgoing_id}"),
        None,
    )
    .await;
    assert_eq!(status, 404, "and its outgoing hook: {body}");

    common::delete_channel(&http, &token, &channel).await;
}

/// `?permanent=true` with `EnableAPIChannelDeletion` off is a **401**, and the id is longer for a
/// system admin. `?permanent=yes` is not a permanent request at all and archives instead.
#[tokio::test]
async fn permanent_deletion_is_refused_with_the_admin_variant_of_the_id() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let (team, _) = a_team_and_channel_the_user_is_in(&http, &token).await;
    let channel = create_channel_typed(&http, &token, &team, "cwperm", "O").await;

    let (go_status, go_body, _) = archive(&http, GO, &token, &channel, "?permanent=true").await;
    let (rust_status, rust_body, _) =
        archive(&http, RUST, &token, &channel, "?permanent=true").await;
    assert_eq!(go_status, 401, "Go refuses with Unauthorized: {go_body}");
    assert_eq!(rust_status, go_status, "{rust_body}");
    assert_eq!(
        error_id(&rust_body),
        "api.user.delete_channel.not_enabled.for_admin.app_error",
        "the fixture user is a system admin, so it gets the verbose id"
    );
    common::assert_error_bodies_match_except_known_gaps(
        go_body.as_bytes(),
        rust_body.as_bytes(),
        "permanent=true",
    );

    // `strconv.ParseBool` refuses `yes` and Go **discards the error**, so this archives.
    let (status, body, by_rust) = archive(&http, RUST, &token, &channel, "?permanent=yes").await;
    assert_eq!(
        status, 200,
        "?permanent=yes archives rather than 400s: {body}"
    );
    assert!(by_rust);
    assert_eq!(body, r#"{"status":"OK"}"#);

    common::delete_channel(&http, &token, &channel).await;
}

// -------------------------------------------------------------------------------------------
// PUT /channels/{id}/privacy
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn converting_a_channel_moves_it_in_and_out_of_the_public_listing() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let (team, _) = a_team_and_channel_the_user_is_in(&http, &token).await;
    let go_channel = create_channel_typed(&http, &token, &team, "cwprivg", "O").await;
    let rust_channel = create_channel_typed(&http, &token, &team, "cwprivr", "O").await;

    let to_private = serde_json::json!({"privacy": "P"});
    let (go_status, go_body, _) = set_privacy(&http, GO, &token, &go_channel, &to_private).await;
    let (rust_status, rust_body, by_rust) =
        set_privacy(&http, RUST, &token, &rust_channel, &to_private).await;
    assert_eq!(go_status, 200, "Go converts: {go_body}");
    assert_eq!(rust_status, go_status, "{rust_body}");
    assert!(by_rust);
    let go: serde_json::Value = serde_json::from_str(&go_body).expect("a channel");
    let rust: serde_json::Value = serde_json::from_str(&rust_body).expect("a channel");
    assert_eq!(rust["type"], "P");
    assert_eq!(
        normalise(&go),
        normalise(&rust),
        "the converted channel differs:\n go: {go}\nrust: {rust}"
    );

    // The `PublicChannels` row is what "public" means to every listing and search, so the
    // conversion is asserted through one: a private channel is not in the team's public list.
    // Read through this server, which is the one that wrote it.
    assert!(
        !public_channel_ids(&http, RUST, &token, &team)
            .await
            .contains(&rust_channel),
        "a private channel must leave the public listing"
    );

    // Back to public, and the row comes back — `upsertPublicChannelT` is an upsert, not an insert,
    // so a port that only ever deleted would fail here and nowhere else.
    let (status, body, _) = set_privacy(
        &http,
        RUST,
        &token,
        &rust_channel,
        &serde_json::json!({"privacy": "O"}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(
        public_channel_ids(&http, RUST, &token, &team)
            .await
            .contains(&rust_channel),
        "converting back must restore the PublicChannels row"
    );

    // "Converting" a public channel to public is accepted and rewrites the row.
    let before = read_channel(&http, RUST, &token, &rust_channel).await;
    let (status, body, _) = set_privacy(
        &http,
        RUST,
        &token,
        &rust_channel,
        &serde_json::json!({"privacy": "O"}),
    )
    .await;
    assert_eq!(status, 200, "a no-op conversion is a 200: {body}");
    let after: serde_json::Value = serde_json::from_str(&body).expect("a channel");
    assert!(after["update_at"].as_i64() > before["update_at"].as_i64());

    common::delete_channel(&http, &token, &go_channel).await;
    common::delete_channel(&http, &token, &rust_channel).await;
}

async fn public_channel_ids(
    http: &reqwest::Client,
    base: &str,
    token: &str,
    team: &str,
) -> Vec<String> {
    let (status, body, _) = send(
        http,
        reqwest::Method::GET,
        base,
        token,
        &format!("/api/v4/teams/{team}/channels?per_page=200"),
        None,
    )
    .await;
    assert_eq!(status, 200, "the public channel listing: {body}");
    let channels: serde_json::Value = serde_json::from_str(&body).expect("a list");
    channels
        .as_array()
        .map(|list| {
            list.iter()
                .filter_map(|channel| channel["id"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// `StringInterfaceFromJSON` discards its decode error, so five different bad bodies are one
/// answer.
#[tokio::test]
async fn every_bad_privacy_body_is_the_same_400() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let (team, _) = a_team_and_channel_the_user_is_in(&http, &token).await;
    let channel = create_channel_typed(&http, &token, &team, "cwprivbad", "O").await;

    for raw in [
        "garbage",
        "{}",
        r#"{"privacy":"X"}"#,
        r#"{"privacy":7}"#,
        r#"{"privacy":null}"#,
        r#"{"privacy":"D"}"#,
        "[]",
    ] {
        let go = http
            .put(format!("{GO}/api/v4/channels/{channel}/privacy"))
            .header("Authorization", format!("Bearer {token}"))
            .header("Content-Type", "application/json")
            .body(raw)
            .send()
            .await
            .expect("Go answers");
        let go_status = go.status().as_u16();
        let go_body = go.text().await.expect("a body");
        let rust = http
            .put(format!("{RUST}/api/v4/channels/{channel}/privacy"))
            .header("Authorization", format!("Bearer {token}"))
            .header("Content-Type", "application/json")
            .body(raw)
            .send()
            .await
            .expect("we answer");
        let rust_status = rust.status().as_u16();
        let rust_body = rust.text().await.expect("a body");
        assert_eq!(go_status, 400, "{raw}: Go: {go_body}");
        assert_eq!(rust_status, go_status, "{raw}: {rust_body}");
        common::assert_error_bodies_match_except_known_gaps(
            go_body.as_bytes(),
            rust_body.as_bytes(),
            raw,
        );
    }

    common::delete_channel(&http, &token, &channel).await;
}

// -------------------------------------------------------------------------------------------
// DMs
// -------------------------------------------------------------------------------------------

/// A DM accepts a header edit from a member and refuses everything else — and the refusals differ
/// between the two routes in *which* fields they cover.
#[tokio::test]
async fn a_dm_accepts_a_header_and_refuses_the_rest() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let me = logged_in_user_id();
    let dm = common::create_direct_channel(&http, &token, me, me).await;

    // A member may edit the header, through both routes, with no channel permission at all.
    let (go_status, go_body, _) = put_channel(
        &http,
        GO,
        &token,
        &dm,
        &serde_json::json!({"id": dm, "header": "mmrs dm header go"}),
    )
    .await;
    assert_eq!(go_status, 200, "Go allows a DM header edit: {go_body}");
    let (rust_status, rust_body, by_rust) = put_channel(
        &http,
        RUST,
        &token,
        &dm,
        &serde_json::json!({"id": dm, "header": "mmrs dm header rust"}),
    )
    .await;
    assert_eq!(rust_status, 200, "we allow it too: {rust_body}");
    assert!(by_rust);
    let updated: serde_json::Value = serde_json::from_str(&rust_body).expect("a channel");
    assert_eq!(updated["header"], "mmrs dm header rust");
    assert_eq!(updated["team_id"], "", "a DM has no team");

    // A purpose change is refused *unconditionally* — the comparison is not guarded on non-empty,
    // so even setting it to the empty string it already holds is fine but changing it is not.
    let (go_status, go_body, _) = put_channel(
        &http,
        GO,
        &token,
        &dm,
        &serde_json::json!({"id": dm, "purpose": "mmrs purpose"}),
    )
    .await;
    let (rust_status, rust_body, _) = put_channel(
        &http,
        RUST,
        &token,
        &dm,
        &serde_json::json!({"id": dm, "purpose": "mmrs purpose"}),
    )
    .await;
    assert_eq!(go_status, 400, "Go: {go_body}");
    assert_eq!(rust_status, go_status, "{rust_body}");
    assert_eq!(
        error_id(&rust_body),
        "api.channel.update_channel.update_direct_or_group_messages_not_allowed.app_error"
    );
    common::assert_error_bodies_match_except_known_gaps(
        go_body.as_bytes(),
        rust_body.as_bytes(),
        "dm purpose",
    );

    // `group_constrained` on a DM is refused by its own guard, ahead of the type switch.
    let (go_status, go_body, _) = patch(
        &http,
        GO,
        &token,
        &dm,
        &serde_json::json!({"group_constrained": true}),
    )
    .await;
    let (rust_status, rust_body, _) = patch(
        &http,
        RUST,
        &token,
        &dm,
        &serde_json::json!({"group_constrained": true}),
    )
    .await;
    assert_eq!(go_status, 400, "Go: {go_body}");
    assert_eq!(rust_status, go_status, "{rust_body}");
    assert_eq!(
        error_id(&rust_body),
        "api.channel.patch_update_channel.group_constrained_not_allowed.app_error"
    );

    // `default_category_name` on a DM is refused whatever its value — the *presence* of the field
    // is the test, which is the disjunct `updateChannel` does not have.
    let (go_status, go_body, _) = patch(
        &http,
        GO,
        &token,
        &dm,
        &serde_json::json!({"default_category_name": ""}),
    )
    .await;
    let (rust_status, rust_body, _) = patch(
        &http,
        RUST,
        &token,
        &dm,
        &serde_json::json!({"default_category_name": ""}),
    )
    .await;
    assert_eq!(go_status, 400, "Go: {go_body}");
    assert_eq!(rust_status, go_status, "{rust_body}");
    assert_eq!(
        error_id(&rust_body),
        "api.channel.patch_update_channel.update_direct_or_group_messages_not_allowed.app_error"
    );

    // A DM cannot be archived, and the type check precedes both permission gates.
    let (go_status, go_body, _) = archive(&http, GO, &token, &dm, "").await;
    let (rust_status, rust_body, _) = archive(&http, RUST, &token, &dm, "").await;
    assert_eq!(go_status, 400, "Go: {go_body}");
    assert_eq!(rust_status, go_status, "{rust_body}");
    assert_eq!(
        error_id(&rust_body),
        "api.channel.delete_channel.type.invalid"
    );
    common::assert_error_bodies_match_except_known_gaps(
        go_body.as_bytes(),
        rust_body.as_bytes(),
        "archive a dm",
    );

    // And a DM's restore is the "not archived" 400, because a DM never is.
    let (go_status, go_body, _) = restore(&http, GO, &token, &dm).await;
    let (rust_status, rust_body, _) = restore(&http, RUST, &token, &dm).await;
    assert_eq!(go_status, 400, "Go: {go_body}");
    assert_eq!(rust_status, go_status, "{rust_body}");
    assert_eq!(
        error_id(&rust_body),
        "api.channel.restore_channel.restored.app_error"
    );
}

// -------------------------------------------------------------------------------------------
// Permissions
// -------------------------------------------------------------------------------------------

/// A private channel the caller is not in refuses all five routes, and each names its own
/// permission — five different 403s and one 400, all measured against Go.
#[tokio::test]
async fn a_non_member_of_a_private_channel_is_refused_by_every_route() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let admin = go_minted_token(&http).await;
    let (team, _) = a_team_and_channel_the_user_is_in(&http, &admin).await;
    let channel = create_channel_typed(&http, &admin, &team, "cwperms", "P").await;
    let plain = create_plain_user(&http, &admin, &team, "cwperms").await;

    let cases: Vec<(&str, reqwest::Method, String, Option<serde_json::Value>)> = vec![
        (
            "update",
            reqwest::Method::PUT,
            format!("/api/v4/channels/{channel}"),
            Some(serde_json::json!({"id": channel, "header": "x"})),
        ),
        (
            "patch",
            reqwest::Method::PUT,
            format!("/api/v4/channels/{channel}/patch"),
            Some(serde_json::json!({"header": "x"})),
        ),
        (
            "privacy",
            reqwest::Method::PUT,
            format!("/api/v4/channels/{channel}/privacy"),
            Some(serde_json::json!({"privacy": "O"})),
        ),
        (
            "delete",
            reqwest::Method::DELETE,
            format!("/api/v4/channels/{channel}"),
            None,
        ),
        (
            "restore",
            reqwest::Method::POST,
            format!("/api/v4/channels/{channel}/restore"),
            None,
        ),
    ];
    for (label, method, path, body) in cases {
        let (go_status, go_body, _) = send(
            &http,
            method.clone(),
            GO,
            &plain.token,
            &path,
            body.as_ref(),
        )
        .await;
        let (rust_status, rust_body, _) =
            send(&http, method, RUST, &plain.token, &path, body.as_ref()).await;
        assert_eq!(go_status, 403, "{label}: Go refuses: {go_body}");
        assert_eq!(
            rust_status, go_status,
            "{label}: status differs: {rust_body}"
        );
        common::assert_error_bodies_match_except_known_gaps(
            go_body.as_bytes(),
            rust_body.as_bytes(),
            label,
        );
    }

    // `canEditChannelBanner` is the one refusal whose id depends on *which* failure came last: Go
    // sets the licence error and then falls into the type switch, which overwrites it with the
    // permission error. So a non-member sees the **permission** 403 where an admin sees the
    // licence 403 — and a port that returned early on the licence check would answer the same
    // thing to both. The helper compares ids, which is what separates them.
    let banner = serde_json::json!({
        "banner_info": {"enabled": true, "text": "hi", "background_color": "#ffffff"}
    });
    let path = format!("/api/v4/channels/{channel}/patch");
    let (go_status, go_body, _) = send(
        &http,
        reqwest::Method::PUT,
        GO,
        &plain.token,
        &path,
        Some(&banner),
    )
    .await;
    let (rust_status, rust_body, _) = send(
        &http,
        reqwest::Method::PUT,
        RUST,
        &plain.token,
        &path,
        Some(&banner),
    )
    .await;
    assert_eq!(go_status, 403, "Go refuses the banner patch: {go_body}");
    assert_eq!(rust_status, go_status, "{rust_body}");
    common::assert_error_bodies_match_except_known_gaps(
        go_body.as_bytes(),
        rust_body.as_bytes(),
        "non-member banner patch",
    );
    assert_ne!(
        error_id(&rust_body),
        "license_error.feature_unavailable.specific",
        "the type switch overwrote the licence error with the permission one"
    );

    common::delete_plain_user(&http, &admin, &plain.id).await;
    common::delete_channel(&http, &admin, &channel).await;
}

/// A channel **member** without `manage_team` still cannot restore, because `restoreChannel`'s gate
/// is on the team and not on the channel. The archive is done by the admin so the fixture is
/// actually archived when the plain user tries.
#[tokio::test]
async fn restore_is_gated_on_the_team_not_on_the_channel() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let admin = go_minted_token(&http).await;
    let (team, _) = a_team_and_channel_the_user_is_in(&http, &admin).await;
    let channel = create_channel_typed(&http, &admin, &team, "cwrestperm", "P").await;
    let plain = create_plain_user(&http, &admin, &team, "cwrestperm").await;
    common::add_user_to_channel(&http, &admin, &channel, &plain.id).await;

    let (status, body, _) = archive(&http, RUST, &admin, &channel, "").await;
    assert_eq!(status, 200, "the admin archives it: {body}");

    let (go_status, go_body, _) = restore(&http, GO, &plain.token, &channel).await;
    let (rust_status, rust_body, _) = restore(&http, RUST, &plain.token, &channel).await;
    assert_eq!(
        go_status, 403,
        "a channel member without manage_team is refused: {go_body}"
    );
    assert_eq!(rust_status, go_status, "{rust_body}");
    common::assert_error_bodies_match_except_known_gaps(
        go_body.as_bytes(),
        rust_body.as_bytes(),
        "member restore",
    );

    // The admin can, and the answer is the restored channel.
    let (status, body, by_rust) = restore(&http, RUST, &admin, &channel).await;
    assert_eq!(status, 200, "{body}");
    assert!(by_rust);

    common::delete_plain_user(&http, &admin, &plain.id).await;
    common::delete_channel(&http, &admin, &channel).await;
}

/// A channel that does not exist is a **404** from every one of the five, with the same id — the
/// lookup precedes every permission check because the channel's type chooses the check.
#[tokio::test]
async fn a_missing_channel_is_a_404_from_all_five_routes() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let missing = "aaaaaaaaaaaaaaaaaaaaaaaaaa";

    let cases: Vec<(&str, reqwest::Method, String, Option<serde_json::Value>)> = vec![
        (
            "update",
            reqwest::Method::PUT,
            format!("/api/v4/channels/{missing}"),
            Some(serde_json::json!({"id": missing, "header": "x"})),
        ),
        (
            "patch",
            reqwest::Method::PUT,
            format!("/api/v4/channels/{missing}/patch"),
            Some(serde_json::json!({"header": "x"})),
        ),
        (
            "privacy",
            reqwest::Method::PUT,
            format!("/api/v4/channels/{missing}/privacy"),
            Some(serde_json::json!({"privacy": "P"})),
        ),
        (
            "delete",
            reqwest::Method::DELETE,
            format!("/api/v4/channels/{missing}"),
            None,
        ),
        (
            "restore",
            reqwest::Method::POST,
            format!("/api/v4/channels/{missing}/restore"),
            None,
        ),
    ];
    for (label, method, path, body) in cases {
        let (go_status, go_body, _) =
            send(&http, method.clone(), GO, &token, &path, body.as_ref()).await;
        let (rust_status, rust_body, _) =
            send(&http, method, RUST, &token, &path, body.as_ref()).await;
        assert_eq!(go_status, 404, "{label}: Go: {go_body}");
        assert_eq!(rust_status, go_status, "{label}: {rust_body}");
        assert_eq!(
            error_id(&rust_body),
            "app.channel.get.existing.app_error",
            "{label}: our id"
        );
        common::assert_error_bodies_match_except_known_gaps(
            go_body.as_bytes(),
            rust_body.as_bytes(),
            label,
        );
    }

    // A path segment of the wrong length is a **400** with a different id — `invalid_url_param`,
    // not `invalid_body_param`.
    let (go_status, go_body, _) = send(
        &http,
        reqwest::Method::PUT,
        GO,
        &token,
        "/api/v4/channels/short/patch",
        Some(&serde_json::json!({"header": "x"})),
    )
    .await;
    let (rust_status, rust_body, _) = send(
        &http,
        reqwest::Method::PUT,
        RUST,
        &token,
        "/api/v4/channels/short/patch",
        Some(&serde_json::json!({"header": "x"})),
    )
    .await;
    assert_eq!(go_status, 400, "Go: {go_body}");
    assert_eq!(rust_status, go_status, "{rust_body}");
    assert_eq!(
        error_id(&rust_body),
        "api.context.invalid_url_param.app_error"
    );
}

// -------------------------------------------------------------------------------------------
// Websocket events
// -------------------------------------------------------------------------------------------
//
// Four tests rather than one, and each opens its **own** probe for a single exchange. That is not
// tidiness: a `WebConn` whose send queue fills is *disconnected*, not throttled
// (`mm_app::hub`'s `try_send`), and the shared admin's stream carries every broadcast the rest of
// this ~1000-test binary produces. One probe held open across five sequential exchanges was
// dropped part-way through on a whole-suite run and passed in isolation — the classic shape of
// this bug. Each test also holds `common::BROADCAST_STREAM`, because each asserts a **count**.
//
// The addressing is the part no body comparison can reach: `channel_deleted` and
// `channel_restored` go to the **team** for a public channel and to the **channel** for a private
// one, and getting that backwards broadcasts a private channel's archive to everyone on the team.

/// `channel_updated`: addressed to the channel, the channel carried as a JSON **string**, and the
/// client's `Connection-Id` header deliberately *not* honoured — unlike `upsertDraft`, Go hands
/// this path an empty `omit_connection_id`.
#[tokio::test]
async fn channel_updated_is_addressed_to_the_channel_and_ignores_the_connection_id() {
    if !stack_enabled() {
        return;
    }
    let _broadcast = common::BROADCAST_STREAM.lock().await;

    let http = client();
    let token = go_minted_token(&http).await;
    let (team, _) = a_team_and_channel_the_user_is_in(&http, &token).await;
    let channel = create_channel_typed(&http, &token, &team, "cwevupd", "O").await;

    let mut probe = SocketProbe::connect(RUST, &token).await;
    let with_connection = http
        .put(format!("{RUST}/api/v4/channels/{channel}"))
        .header("Authorization", format!("Bearer {token}"))
        // Present, and ignored: the assertion below is that it does **not** reach the broadcast.
        .header("Connection-Id", "mmrschanlifeconnection12345")
        .json(&serde_json::json!({"id": channel, "header": "mmrs event header"}))
        .send()
        .await
        .expect("we answer");
    assert_eq!(with_connection.status().as_u16(), 200);

    assert!(
        probe
            .collect_until(Duration::from_secs(5), |frames| {
                frames
                    .iter()
                    .any(|frame| is_channel_event(frame, "channel_updated", &channel))
            })
            .await,
        "no channel_updated arrived: {:?}",
        probe.raw
    );
    let updated = channel_events(&probe, "channel_updated", &channel);
    assert_eq!(updated.len(), 1, "exactly one: {:?}", probe.raw);
    assert_eq!(updated[0]["broadcast"]["channel_id"], channel);
    assert_eq!(
        updated[0]["broadcast"]["team_id"], "",
        "channel_updated is addressed to the channel, not the team"
    );
    assert_eq!(updated[0]["broadcast"]["user_id"], "");
    assert_eq!(
        updated[0]["broadcast"]["omit_connection_id"], "",
        "Go passes an empty omit_connection_id on this path whatever the header says"
    );
    assert!(
        updated[0]["data"]["channel"].is_string(),
        "the channel travels as a JSON string, not an object: {}",
        updated[0]
    );

    common::delete_channel(&http, &token, &channel).await;
}

/// The privacy change publishes **two** events: `channel_updated` from `App.UpdateChannel`, then
/// `channel_converted` addressed to the team with `channel_type` as a plain string.
#[tokio::test]
async fn the_privacy_change_publishes_channel_updated_and_channel_converted() {
    if !stack_enabled() {
        return;
    }
    let _broadcast = common::BROADCAST_STREAM.lock().await;

    let http = client();
    let token = go_minted_token(&http).await;
    let (team, _) = a_team_and_channel_the_user_is_in(&http, &token).await;
    let channel = create_channel_typed(&http, &token, &team, "cwevconv", "O").await;

    let mut probe = SocketProbe::connect(RUST, &token).await;
    let (status, body, _) = set_privacy(
        &http,
        RUST,
        &token,
        &channel,
        &serde_json::json!({"privacy": "P"}),
    )
    .await;
    assert_eq!(status, 200, "{body}");

    assert!(
        probe
            .collect_until(Duration::from_secs(5), |frames| {
                frames.iter().any(|frame| {
                    frame["event"] == "channel_converted"
                        && frame["data"]["channel_id"] == channel.as_str()
                })
            })
            .await,
        "no channel_converted arrived: {:?}",
        probe.raw
    );
    let converted: Vec<_> = probe
        .events_named("channel_converted")
        .into_iter()
        .filter(|frame| frame["data"]["channel_id"] == channel.as_str())
        .collect();
    assert_eq!(converted.len(), 1, "exactly one: {:?}", probe.raw);
    assert_eq!(
        converted[0]["broadcast"]["team_id"], team,
        "channel_converted is addressed to the team"
    );
    assert_eq!(converted[0]["broadcast"]["channel_id"], "");
    assert_eq!(
        converted[0]["data"]["channel_type"], "P",
        "the new type travels as a plain string"
    );
    assert_eq!(
        channel_events(&probe, "channel_updated", &channel).len(),
        1,
        "the conversion publishes channel_updated too — two events, not one: {:?}",
        probe.raw
    );

    common::delete_channel(&http, &token, &channel).await;
}

/// A **private** channel's archive and restore are addressed to the channel, and only its members
/// hear them. `delete_at` is a JSON number; the restore event carries `channel_id` alone.
///
/// # One probe, and a warm-up event, because the membership cache excludes an archived channel
///
/// A channel-addressed event reaches a connection only if the channel is in that connection's
/// `allChannelMembers` snapshot, and the snapshot comes from
/// `get_all_channel_members_for_user(user, include_deleted = false)` — which **omits a member's
/// archived channels** — cached for thirty minutes (`mm_app::hub`, and Go's `WebConn` the same
/// way). So a connection whose snapshot happens to be taken while this channel is archived never
/// hears its restore, however correct the publish is.
///
/// Which is exactly what failed here: a second probe opened *after* the archive took its snapshot
/// then, and the restore event was filtered out. In isolation the restore's own event was the
/// first to populate the snapshot, by which time `DeleteAt` was already zero, and the test passed.
///
/// The fix is to make the snapshot deterministic rather than to wait longer: one probe, opened
/// while the channel is live, and a header patch first whose `channel_updated` forces the snapshot
/// to be taken **now**. Everything after that is a property of the publish and not of what else
/// the suite was doing.
#[tokio::test]
async fn a_private_channels_archive_and_restore_are_addressed_to_the_channel() {
    if !stack_enabled() {
        return;
    }
    let _broadcast = common::BROADCAST_STREAM.lock().await;

    let http = client();
    let token = go_minted_token(&http).await;
    let (team, _) = a_team_and_channel_the_user_is_in(&http, &token).await;
    let channel = create_channel_typed(&http, &token, &team, "cwevpriv", "P").await;

    let mut probe = SocketProbe::connect(RUST, &token).await;

    // The warm-up: a header patch while the channel is still live, so this connection's membership
    // snapshot is taken now and contains it. See the note above — without this the assertions below
    // depend on when unrelated traffic happened to populate the snapshot.
    let (status, body, _) = patch(
        &http,
        RUST,
        &token,
        &channel,
        &serde_json::json!({"header": "mmrs warm the member cache"}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(
        probe
            .collect_until(Duration::from_secs(5), |frames| {
                frames
                    .iter()
                    .any(|frame| is_channel_event(frame, "channel_updated", &channel))
            })
            .await,
        "the warm-up channel_updated never arrived: {:?}",
        probe.raw
    );

    let (status, body, _) = archive(&http, RUST, &token, &channel, "").await;
    assert_eq!(status, 200, "{body}");
    assert!(
        probe
            .collect_until(Duration::from_secs(5), |frames| {
                frames.iter().any(|frame| {
                    frame["event"] == "channel_deleted"
                        && frame["data"]["channel_id"] == channel.as_str()
                })
            })
            .await,
        "no channel_deleted arrived: {:?}",
        probe.raw
    );
    let deleted: Vec<_> = probe
        .events_named("channel_deleted")
        .into_iter()
        .filter(|frame| frame["data"]["channel_id"] == channel.as_str())
        .collect();
    assert_eq!(deleted.len(), 1, "exactly one: {:?}", probe.raw);
    assert_eq!(
        deleted[0]["broadcast"]["channel_id"], channel,
        "a private channel's archive is addressed to the channel, not the team"
    );
    assert_eq!(deleted[0]["broadcast"]["team_id"], "");
    assert!(
        deleted[0]["data"]["delete_at"].is_i64(),
        "delete_at is a number, not a string: {}",
        deleted[0]
    );

    // The restore, on the **same** probe: a fresh one would take its membership snapshot now, with
    // the channel archived, and never be told about the restore.
    let (status, body, _) = restore(&http, RUST, &token, &channel).await;
    assert_eq!(status, 200, "{body}");
    assert!(
        probe
            .collect_until(Duration::from_secs(5), |frames| {
                frames.iter().any(|frame| {
                    frame["event"] == "channel_restored"
                        && frame["data"]["channel_id"] == channel.as_str()
                })
            })
            .await,
        "no channel_restored arrived: {:?}",
        probe.raw
    );
    let restored: Vec<_> = probe
        .events_named("channel_restored")
        .into_iter()
        .filter(|frame| frame["data"]["channel_id"] == channel.as_str())
        .collect();
    assert_eq!(restored.len(), 1, "exactly one: {:?}", probe.raw);
    assert_eq!(restored[0]["broadcast"]["channel_id"], channel);
    assert_eq!(restored[0]["broadcast"]["team_id"], "");
    assert!(
        restored[0]["data"].get("delete_at").is_none(),
        "the restore event carries only channel_id: {}",
        restored[0]
    );

    common::delete_channel(&http, &token, &channel).await;
}

/// The other half of the archive addressing: a **public** channel's goes to the team, with an empty
/// `channel_id`. This and the test above are the pair; either alone passes with the branch
/// inverted.
#[tokio::test]
async fn a_public_channels_archive_is_addressed_to_the_team() {
    if !stack_enabled() {
        return;
    }
    let _broadcast = common::BROADCAST_STREAM.lock().await;

    let http = client();
    let token = go_minted_token(&http).await;
    let (team, _) = a_team_and_channel_the_user_is_in(&http, &token).await;
    let channel = create_channel_typed(&http, &token, &team, "cwevpub", "O").await;

    let mut probe = SocketProbe::connect(RUST, &token).await;
    let (status, body, _) = archive(&http, RUST, &token, &channel, "").await;
    assert_eq!(status, 200, "{body}");
    assert!(
        probe
            .collect_until(Duration::from_secs(5), |frames| {
                frames.iter().any(|frame| {
                    frame["event"] == "channel_deleted"
                        && frame["data"]["channel_id"] == channel.as_str()
                })
            })
            .await,
        "no channel_deleted arrived: {:?}",
        probe.raw
    );
    let deleted: Vec<_> = probe
        .events_named("channel_deleted")
        .into_iter()
        .filter(|frame| frame["data"]["channel_id"] == channel.as_str())
        .collect();
    assert_eq!(deleted.len(), 1, "exactly one: {:?}", probe.raw);
    assert_eq!(
        deleted[0]["broadcast"]["team_id"], team,
        "a public channel's archive is addressed to the team"
    );
    assert_eq!(deleted[0]["broadcast"]["channel_id"], "");

    common::delete_channel(&http, &token, &channel).await;
}

/// The event Go publishes and the event we publish, side by side — the addressing and the payload
/// keys, for the one route where both servers can act on their own channel.
#[tokio::test]
async fn gos_channel_updated_and_ours_carry_the_same_shape() {
    if !stack_enabled() {
        return;
    }
    // A count on both servers' shared-admin streams — see `common::BROADCAST_STREAM`.
    let _broadcast = common::BROADCAST_STREAM.lock().await;

    let http = client();
    let token = go_minted_token(&http).await;
    let (team, _) = a_team_and_channel_the_user_is_in(&http, &token).await;
    let go_channel = create_channel_typed(&http, &token, &team, "cwevcmpg", "O").await;
    let rust_channel = create_channel_typed(&http, &token, &team, "cwevcmpr", "O").await;

    let mut go_probe = SocketProbe::connect(GO, &token).await;
    let mut rust_probe = SocketProbe::connect(RUST, &token).await;

    put_channel(
        &http,
        GO,
        &token,
        &go_channel,
        &serde_json::json!({"id": go_channel, "header": "mmrs shape"}),
    )
    .await;
    put_channel(
        &http,
        RUST,
        &token,
        &rust_channel,
        &serde_json::json!({"id": rust_channel, "header": "mmrs shape"}),
    )
    .await;

    go_probe
        .collect_until(Duration::from_secs(3), |frames| {
            frames
                .iter()
                .any(|frame| is_channel_event(frame, "channel_updated", &go_channel))
        })
        .await;
    rust_probe
        .collect_until(Duration::from_secs(3), |frames| {
            frames
                .iter()
                .any(|frame| is_channel_event(frame, "channel_updated", &rust_channel))
        })
        .await;

    let go_events = channel_events(&go_probe, "channel_updated", &go_channel);
    let rust_events = channel_events(&rust_probe, "channel_updated", &rust_channel);
    assert_eq!(go_events.len(), 1, "Go published one: {:?}", go_probe.raw);
    assert_eq!(
        rust_events.len(),
        1,
        "we published one: {:?}",
        rust_probe.raw
    );

    // The broadcast blocks are identical apart from the channel id.
    let mut go_broadcast = go_events[0]["broadcast"].clone();
    let mut rust_broadcast = rust_events[0]["broadcast"].clone();
    go_broadcast["channel_id"] = serde_json::json!("<channel>");
    rust_broadcast["channel_id"] = serde_json::json!("<channel>");
    assert_eq!(
        go_broadcast, rust_broadcast,
        "the channel_updated addressing differs"
    );

    // And so are the channels inside the `channel` string, normalised.
    let go_inner: serde_json::Value =
        serde_json::from_str(go_events[0]["data"]["channel"].as_str().expect("a string"))
            .expect("a channel");
    let rust_inner: serde_json::Value = serde_json::from_str(
        rust_events[0]["data"]["channel"]
            .as_str()
            .expect("a string"),
    )
    .expect("a channel");
    assert_eq!(
        normalise(&go_inner),
        normalise(&rust_inner),
        "the channel inside the event differs:\n go: {go_inner}\nrust: {rust_inner}"
    );

    common::delete_channel(&http, &token, &go_channel).await;
    common::delete_channel(&http, &token, &rust_channel).await;
}

fn is_channel_event(frame: &serde_json::Value, event: &str, channel_id: &str) -> bool {
    frame["event"] == event
        && frame["data"]["channel"]
            .as_str()
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
            .and_then(|channel| channel["id"].as_str().map(|id| id == channel_id))
            .unwrap_or(false)
}

fn channel_events(probe: &SocketProbe, event: &str, channel_id: &str) -> Vec<serde_json::Value> {
    probe
        .frames()
        .into_iter()
        .filter(|frame| is_channel_event(frame, event, channel_id))
        .collect()
}
