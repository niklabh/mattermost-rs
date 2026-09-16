//! Cross-server parity for `ServiceSettings.PostEditTimeLimit` on the three edit routes and the
//! pin pair.
//!
//! ```sh
//! scripts/go-edit-limit.sh start && scripts/parity.sh --test parity post_edit_time_limit
//! ```
//!
//! # A pair of servers of its own
//!
//! `postEditTimeLimitExpired` (api4/post.go:1052) answers false on the stock `-1` before it reads
//! anything else, so on the stack server the 400 it gates is unreachable ([D-222], closed by this
//! file). `0` reaches it for every post: expired is `now > CreateAt + limit * 1000`. The setting
//! cannot be flipped on the shared servers while two dozen other suites edit posts, so this file
//! talks to `scripts/go-edit-limit.sh`'s Go (the stack's binary and database, started with
//! `MM_SERVICESETTINGS_POSTEDITTIMELIMIT=0`) and to an mm-api `SecondServer` given the same
//! variable, forwarding to that Go.
//!
//! # What each case separates
//!
//! Every refused request has an accepted twin that differs only in whether it changes anything —
//! Go checks the limit **after** its no-op short circuits, so an empty patch, an unchanged update
//! and an unpin of an unpinned post all pass on an expired post. A port that checked the limit
//! first would refuse the twins; one that skipped it would accept the refusals.

use crate::common;

use common::{
    GO, SecondServer, a_team_and_channel_the_user_is_in,
    assert_error_bodies_match_except_known_gaps, client, go_minted_token, post_message,
    stack_enabled,
};

/// `SecondServer` port for the edit-limit mm-api. Unique in the binary —
/// `parity::second_server_ports` checks.
const EDIT_LIMIT_RUST_PORT: u16 = 8072;

/// `MMRS_GO_PORT + 35`, the port `scripts/go-edit-limit.sh` binds — derived from [`GO`] so a
/// worktree on any stack finds its own.
fn edit_limit_go_base() -> Option<String> {
    let port: u16 = GO.rsplit(':').next()?.parse().ok()?;
    Some(format!("http://localhost:{}", port + 35))
}

const TIME_LIMIT: &str = "api.post.update_post.permissions_time_limit.app_error";

async fn send(
    http: &reqwest::Client,
    method: reqwest::Method,
    url: String,
    token: &str,
    body: Option<serde_json::Value>,
) -> (u16, Vec<u8>) {
    let mut request = http
        .request(method, url)
        .header("Authorization", format!("Bearer {token}"));
    if let Some(body) = body {
        request = request.json(&body);
    }
    let response = request.send().await.expect("the server answers");
    let status = response.status().as_u16();
    (status, response.bytes().await.expect("a body").to_vec())
}

#[tokio::test]
async fn every_edit_of_an_expired_post_is_refused_and_every_no_op_is_not() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let go = edit_limit_go_base().expect("GO carries a port");
    if !http
        .get(format!("{go}/api/v4/system/ping"))
        .send()
        .await
        .is_ok_and(|r| r.status().is_success())
    {
        // Panic rather than skip: `stack_enabled` is true, so a skip would pass asserting nothing.
        panic!(
            "post_edit_time_limit: no edit-limit oracle at {go}. Run `scripts/go-edit-limit.sh \
             start`."
        );
    }
    let server = SecondServer::start(
        EDIT_LIMIT_RUST_PORT,
        &[
            ("MM_SERVICESETTINGS_POSTEDITTIMELIMIT", "0"),
            ("MM_GO_UPSTREAM", &go),
        ],
    )
    .await
    .expect("the edit-limit mm-api starts");
    let rust = server.base.clone();

    let token = go_minted_token(&http).await;
    let (_team, channel) = a_team_and_channel_the_user_is_in(&http, &token).await;

    // One post per server, created through the stack's Go: a write cannot be replayed on the same
    // row, and a refused edit here must not be the reason the other server's edit is a no-op.
    let go_post = post_message(&http, &token, &channel, "mmrs edit limit go", None).await;
    let rs_post = post_message(&http, &token, &channel, "mmrs edit limit rs", None).await;

    struct Case {
        what: &'static str,
        method: reqwest::Method,
        suffix: &'static str,
        body: fn(&str, &str) -> Option<serde_json::Value>,
        refused: bool,
    }
    let cases = [
        Case {
            what: "a patch that changes the message",
            method: reqwest::Method::PUT,
            suffix: "/patch",
            body: |_, _| Some(serde_json::json!({ "message": "mmrs edited" })),
            refused: true,
        },
        Case {
            what: "an empty patch",
            method: reqwest::Method::PUT,
            suffix: "/patch",
            body: |_, _| Some(serde_json::json!({})),
            refused: false,
        },
        Case {
            what: "an update that changes the message",
            method: reqwest::Method::PUT,
            suffix: "",
            body: |id, channel| {
                Some(
                    serde_json::json!({ "id": id, "channel_id": channel, "message": "mmrs edited" }),
                )
            },
            refused: true,
        },
        Case {
            what: "a pin of an unpinned post",
            method: reqwest::Method::POST,
            suffix: "/pin",
            body: |_, _| None,
            refused: true,
        },
        Case {
            what: "an unpin of an unpinned post",
            method: reqwest::Method::POST,
            suffix: "/unpin",
            body: |_, _| None,
            refused: false,
        },
    ];

    for case in &cases {
        let (go_status, go_body) = send(
            &http,
            case.method.clone(),
            format!("{go}/api/v4/posts/{go_post}{}", case.suffix),
            &token,
            (case.body)(&go_post, &channel),
        )
        .await;
        let (rs_status, rs_body) = send(
            &http,
            case.method.clone(),
            format!("{rust}/api/v4/posts/{rs_post}{}", case.suffix),
            &token,
            (case.body)(&rs_post, &channel),
        )
        .await;
        let go_text = String::from_utf8_lossy(&go_body);
        assert_eq!(rs_status, go_status, "{}: go={go_text}", case.what);
        if case.refused {
            assert_eq!(go_status, 400, "{}: Go refuses: {go_text}", case.what);
            let go_error =
                assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, case.what);
            assert_eq!(go_error["id"], TIME_LIMIT, "{}", case.what);
        } else {
            assert_eq!(go_status, 200, "{}: Go accepts: {go_text}", case.what);
        }
    }

    for post in [&go_post, &rs_post] {
        common::delete_post(&http, &token, post).await;
    }
}
