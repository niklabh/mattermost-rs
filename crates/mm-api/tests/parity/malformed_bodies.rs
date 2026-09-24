//! A JSON array, a bare `null` and trailing bytes as request bodies, across route families
//! ([D-941]).
//!
//! `encoding/json` refuses an array for a struct, takes `null` as the zero value for a value
//! target (and as nil for a pointer, which the handler refuses by the body's name), and ignores
//! whatever follows the first value when the handler uses `Decoder.Decode`. serde's derive
//! differs on all three, and the difference only shows in which *parameter* the 400 names — so
//! every case here compares the whole error document, translated message included.
//!
//! Every case is an error on Go by construction, so nothing is written by either server.

use crate::common::{
    GO, RUST, a_team_and_channel_the_user_is_in, assert_error_bodies_match_except_known_gaps,
    assert_served_by_rust, client, go_minted_token, logged_in_user_id, post_message, stack_enabled,
};

async fn send(
    http: &reqwest::Client,
    base: &str,
    token: &str,
    method: &str,
    path: &str,
    body: &str,
) -> (u16, Vec<u8>) {
    let method = reqwest::Method::from_bytes(method.as_bytes()).expect("a method");
    let response = http
        .request(method, format!("{base}{path}"))
        .header("Authorization", format!("Bearer {token}"))
        .header("Content-Type", "application/json")
        .body(body.to_owned())
        .send()
        .await
        .unwrap_or_else(|e| panic!("{base}{path} is unreachable: {e}"));
    let status = response.status().as_u16();
    // A folded forwarded field is expected to reach Go; every other case must be served here.
    if base == RUST && !body.contains("NOT_IN_CHANNEL_ID") {
        assert_served_by_rust(response.headers(), path);
    }
    if base == RUST && body.contains("NOT_IN_CHANNEL_ID") {
        assert_eq!(
            response
                .headers()
                .get("x-mmrs-served-by")
                .and_then(|v| v.to_str().ok()),
            Some("go"),
            "a folded forwarded field must be handed to Go"
        );
    }
    (status, response.bytes().await.expect("body reads").to_vec())
}

#[tokio::test]
async fn an_array_a_null_and_trailing_bytes_are_answered_as_go_answers_them() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let me = logged_in_user_id().to_owned();
    let (team, channel) = a_team_and_channel_the_user_is_in(&http, &token).await;
    let post = post_message(&http, &token, &channel, "malformed-bodies target", None).await;
    // A well-formed id no row has: the reaction's post check fails after the decode.
    let absent = "zzzzzzzzzzzzzzzzzzzzzzzzzz";
    let reaction_trailing =
        format!(r#"{{"user_id":"{me}","post_id":"{absent}","emoji_name":"smile"}} trailing"#);

    let cases: Vec<(&str, String, &str)> = vec![
        // Posts: `var post model.Post` — a value.
        ("POST", "/api/v4/posts".into(), "[]"),
        ("POST", "/api/v4/posts".into(), "null"),
        ("PUT", format!("/api/v4/posts/{post}/patch"), "[]"),
        ("POST", "/api/v4/posts/ephemeral".into(), "[]"),
        ("POST", "/api/v4/posts/ephemeral".into(), "null"),
        // Reactions: a value, and a body with bytes after it.
        ("POST", "/api/v4/reactions".into(), "[]"),
        ("POST", "/api/v4/reactions".into(), "null"),
        ("POST", "/api/v4/reactions".into(), &reaction_trailing),
        // Status: a value.
        ("PUT", format!("/api/v4/users/{me}/status"), "[]"),
        ("PUT", format!("/api/v4/users/{me}/status"), "null"),
        // Teams: a value.
        ("POST", "/api/v4/teams".into(), "[]"),
        ("PUT", format!("/api/v4/teams/{team}/patch"), "[]"),
        // Channels: a pointer, so `null` is the body's own refusal.
        ("PUT", format!("/api/v4/channels/{channel}/patch"), "[]"),
        ("PUT", format!("/api/v4/channels/{channel}/patch"), "null"),
        // Webhooks: a value.
        ("POST", "/api/v4/hooks/incoming".into(), "[]"),
        ("POST", "/api/v4/hooks/outgoing".into(), "[]"),
        // Bots: a pointer.
        ("POST", "/api/v4/bots".into(), "[]"),
        ("POST", "/api/v4/bots".into(), "null"),
        // Preferences: a slice, which `null` leaves nil and `[]` leaves empty — both the
        // length check's 400.
        ("PUT", format!("/api/v4/users/{me}/preferences"), "null"),
        ("PUT", format!("/api/v4/users/{me}/preferences"), "[]"),
        ("PUT", format!("/api/v4/users/{me}/preferences"), "[[]]"),
        // Drafts: a value.
        ("POST", "/api/v4/drafts".into(), "[]"),
        // Scheme roles on a channel member: a value.
        (
            "PUT",
            format!("/api/v4/channels/{channel}/members/{me}/schemeRoles"),
            "[]",
        ),
        // Commands: a value.
        ("POST", "/api/v4/commands".into(), "[]"),
    ];

    let mut failures = Vec::new();
    for (method, path, body) in &cases {
        let label = format!("{method} {path} {body}");
        let (go_status, go_body) = send(&http, GO, &token, method, path, body).await;
        let (rust_status, rust_body) = send(&http, RUST, &token, method, path, body).await;
        assert!(
            go_status >= 400,
            "{label}: the case must be an error on Go, got {go_status}: {}",
            String::from_utf8_lossy(&go_body)
        );
        if go_status != rust_status {
            failures.push(format!(
                "{label}: status go {go_status} rust {rust_status}\n  go: {}\n  rs: {}",
                String::from_utf8_lossy(&go_body),
                String::from_utf8_lossy(&rust_body)
            ));
            continue;
        }
        let outcome = std::panic::catch_unwind(|| {
            assert_error_bodies_match_except_known_gaps(&go_body, &rust_body, &label);
        });
        if outcome.is_err() {
            failures.push(format!(
                "{label}: bodies differ\n  go: {}\n  rs: {}",
                String::from_utf8_lossy(&go_body),
                String::from_utf8_lossy(&rust_body)
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} cases differ:\n{}",
        failures.len(),
        cases.len(),
        failures.join("\n")
    );
}

/// Go's other decoding rules, now in the shared body decoder: a `null` member ([D-057]), a `null`
/// slice element ([D-075]), a key in another case — including U+212A KELVIN SIGN, which folds to
/// `K` ([D-040], [D-460]) — and a repeated key ([D-071]). serde's derive alone answers each of
/// these differently from Go; every case below was a divergence before.
///
/// Success answers are compared byte for byte (each is a search that matches nothing, or a
/// focus-loss view), errors as whole documents.
#[tokio::test]
async fn null_members_folded_keys_and_repeated_keys_are_answered_as_go_answers_them() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let me = logged_in_user_id().to_owned();
    let (team, _channel) = a_team_and_channel_the_user_is_in(&http, &token).await;
    let absent = "zzzzzzzzzzzzzzzzzzzzzzzzzz";
    let none = "mmrs-go-json-nomatch-zq";

    let cases: Vec<(&str, String, String)> = vec![
        // Emoji search: a folded `term`, a Kelvin-sign `prefix_only`'s cousin, a `null` bool.
        (
            "POST",
            "/api/v4/emoji/search".into(),
            format!(r#"{{"TERM":"{none}"}}"#),
        ),
        (
            "POST",
            "/api/v4/emoji/search".into(),
            format!(r#"{{"term":"{none}","prefix_only":null}}"#),
        ),
        (
            "POST",
            "/api/v4/emoji/search".into(),
            format!(r#"{{"term":"x","term":"{none}"}}"#),
        ),
        // A folded forwarded field still hands the search to Go, whose answer for a
        // `not_in_channel_id` without a `team_id` is a 400.
        (
            "POST",
            "/api/v4/users/search".into(),
            format!(r#"{{"term":"{none}","NOT_IN_CHANNEL_ID":"{absent}"}}"#),
        ),
        // User and team searches: folded keys.
        (
            "POST",
            "/api/v4/users/search".into(),
            format!(r#"{{"Term":"{none}"}}"#),
        ),
        (
            "POST",
            "/api/v4/teams/search".into(),
            format!(r#"{{"TERM":"{none}"}}"#),
        ),
        (
            "POST",
            format!("/api/v4/teams/{team}/channels/search"),
            format!(r#"{{"tErM":"{none}","team_ids":[null]}}"#),
        ),
        // Status: a folded `user_id` reaches the status check rather than the id check.
        (
            "PUT",
            format!("/api/v4/users/{me}/status"),
            format!(r#"{{"USER_ID":"{me}","status":"bogus"}}"#),
        ),
        // Reactions: folded keys, a Kelvin sign, and a repeated `user_id` whose last value wins.
        (
            "POST",
            "/api/v4/reactions".into(),
            format!(r#"{{"USER_ID":"{me}","POST_ID":"{absent}","EMOJI_NAME":"smile"}}"#),
        ),
        (
            "POST",
            "/api/v4/reactions".into(),
            format!(
                "{{\"user_id\":\"{me}\",\"post_id\":\"{absent}\",\"emoji_name\":\"smile\",\"create_at\":null}}"
            ),
        ),
        (
            "POST",
            "/api/v4/reactions".into(),
            format!(
                r#"{{"user_id":"{absent}","user_id":"{me}","post_id":"{absent}","emoji_name":"smile"}}"#
            ),
        ),
        // Types Go embeds another in ([D-1240]): the embedded half's keys fold and take `null`.
        (
            "POST",
            "/api/v4/reports/posts".into(),
            format!(r#"{{"CHANNEL_ID":"{absent}","per_page":null,"cursor":null}}"#),
        ),
        (
            "POST",
            "/api/v4/reports/posts".into(),
            r#"{"channel_id":"","Cursor":"not-a-cursor"}"#.into(),
        ),
        // A focus-loss view with `null` ids.
        (
            "POST",
            "/api/v4/channels/members/me/view".into(),
            r#"{"channel_id":null,"prev_channel_id":null,"collapsed_threads_supported":null}"#
                .into(),
        ),
        // Preferences: a `null` value and a folded key in an element.
        (
            "PUT",
            format!("/api/v4/users/{me}/preferences"),
            format!(r#"[{{"user_id":"{me}","category":"mmrs_go_json","name":"n","value":null}}]"#),
        ),
        (
            "PUT",
            format!("/api/v4/users/{me}/preferences"),
            format!(
                "[{{\"USER_ID\":\"{me}\",\"category\":\"mmrs_go_json\",\"NAME\":\"\u{212a}\",\"Value\":\"v\"}}]"
            ),
        ),
    ];

    let mut failures = Vec::new();
    for (method, path, body) in &cases {
        let label = format!("{method} {path} {body}");
        let (go_status, go_body) = send(&http, GO, &token, method, path, body).await;
        let (rust_status, rust_body) = send(&http, RUST, &token, method, path, body).await;
        if go_status != rust_status {
            failures.push(format!(
                "{label}: status go {go_status} rust {rust_status}\n  go: {}\n  rs: {}",
                String::from_utf8_lossy(&go_body),
                String::from_utf8_lossy(&rust_body)
            ));
            continue;
        }
        let same = if go_status < 400 {
            go_body == rust_body
        } else {
            std::panic::catch_unwind(|| {
                assert_error_bodies_match_except_known_gaps(&go_body, &rust_body, &label);
            })
            .is_ok()
        };
        if !same {
            failures.push(format!(
                "{label}: bodies differ\n  go: {}\n  rs: {}",
                String::from_utf8_lossy(&go_body),
                String::from_utf8_lossy(&rust_body)
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} cases differ:\n{}",
        failures.len(),
        cases.len(),
        failures.join("\n")
    );
}

/// `SidebarCategoryWithChannels` embeds `SidebarCategory` ([D-1240]): a folded `user_id` and
/// `team_id` in the embedded half, and a `null` `sorting`, reach the create as Go reads them.
/// A success on both sides, so the two created categories are compared without their ids and
/// then deleted.
#[tokio::test]
async fn an_embedded_body_type_takes_go_s_rules_in_its_embedded_half() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let me = logged_in_user_id().to_owned();
    let (team, channel) = a_team_and_channel_the_user_is_in(&http, &token).await;
    let path = format!("/api/v4/users/{me}/teams/{team}/channels/categories");
    let body = format!(
        r#"{{"USER_ID":"{me}","Team_Id":"{team}","display_name":"mmrs-embed","type":"custom","channel_ids":["{channel}",null],"sorting":null}}"#
    );
    let (go_status, go_body) = send(&http, GO, &token, "POST", &path, &body).await;
    let (rust_status, rust_body) = send(&http, RUST, &token, "POST", &path, &body).await;
    let parse = |bytes: &[u8]| -> serde_json::Value { serde_json::from_slice(bytes).unwrap() };
    let (mut go, mut rust) = (parse(&go_body), parse(&rust_body));
    for created in [&go, &rust] {
        if let Some(id) = created["id"].as_str() {
            let _ = http
                .delete(format!("{GO}{path}/{id}"))
                .header("Authorization", format!("Bearer {token}"))
                .send()
                .await;
        }
    }
    assert_eq!(go_status, 200, "Go refused it: {go}");
    assert_eq!(rust_status, go_status, "{rust}");
    for created in [&mut go, &mut rust] {
        created.as_object_mut().unwrap().remove("id");
    }
    assert_eq!(rust, go);
}
