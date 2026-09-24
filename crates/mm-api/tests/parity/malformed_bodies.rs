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
    if base == RUST {
        assert_served_by_rust(response.headers(), path);
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
