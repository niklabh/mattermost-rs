//! Cross-server parity for `GET /api/v4/websocket` — the handshake, the framing, and the two
//! actions the router itself owns.
//!
//! This is the first route in the suite whose answer is a *stream*, so the comparison is not one
//! body against another but frame against frame, in order, including the bytes around the JSON.
//!
//! ```sh
//! docker compose up -d && scripts/parity.sh --test parity websocket
//! ```

use std::time::Duration;

use crate::common;

use common::{GO, RUST, SocketProbe, client, go_minted_token, logged_in_user_id, stack_enabled};

/// Keys of the `hello` frame's `data` map that both servers must agree on.
///
/// `server_version` is deliberately absent. Go builds it from `model.CurrentVersion`, its own
/// build number, a hash of the client configuration and whether a licence manager exists — three
/// of the four are properties of the Go binary, so the values cannot agree and the port asserts
/// only that the key is present. `server_hostname` is the host, which differs the same way.
const HELLO_SHARED_KEYS: &[&str] = &["connection_id", "server_version", "server_hostname"];

#[tokio::test]
async fn hello_has_the_same_shape_on_both_servers() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;

    let go = raw_hello(GO, &token).await;
    let rust = raw_hello(RUST, &token).await;

    // The frame is compact and newline-terminated on both. That newline is `json.Encoder.Encode`
    // (web_conn.go:650) and it is the half of the framing a value comparison cannot see: `hello`
    // does **not** take Go's `PrecomputeJSON` path, so it is not the spaced form a broadcast uses.
    assert!(
        go.ends_with('\n'),
        "Go's hello is expected to be newline-terminated: {go:?}"
    );
    assert_eq!(
        go.ends_with('\n'),
        rust.ends_with('\n'),
        "the two servers disagree about the trailing newline on hello"
    );
    assert!(
        !go.contains("\"event\": "),
        "Go's hello is expected to be compact, not the precomputed spaced form: {go:?}"
    );
    assert!(
        !rust.contains("\"event\": "),
        "hello must not take the precompute path: {rust:?}"
    );

    let go: serde_json::Value = serde_json::from_str(&go).expect("Go's hello decodes");
    let rust: serde_json::Value = serde_json::from_str(&rust).expect("our hello decodes");

    assert_eq!(go["event"], "hello");
    assert_eq!(rust["event"], "hello");
    assert_eq!(go["seq"], 0, "hello is the connection's first frame");
    assert_eq!(rust["seq"], 0);

    // The broadcast block is addressed to the connecting user and is otherwise empty — including
    // `"omit_users": null`, which is a Go nil map and not `{}`.
    assert_eq!(go["broadcast"], rust["broadcast"]);
    assert_eq!(rust["broadcast"]["user_id"], logged_in_user_id());
    assert_eq!(rust["broadcast"]["omit_users"], serde_json::Value::Null);

    let go_keys: Vec<&str> = go["data"]
        .as_object()
        .expect("a data map")
        .keys()
        .map(|k| k.as_str())
        .collect();
    let rust_keys: Vec<&str> = rust["data"]
        .as_object()
        .expect("a data map")
        .keys()
        .map(|k| k.as_str())
        .collect();
    assert_eq!(go_keys, rust_keys, "hello's data keys must match");
    for key in HELLO_SHARED_KEYS {
        assert!(go_keys.contains(key), "Go's hello is missing {key}");
    }

    // `connection_id` is the only one whose *value* is comparable, and only structurally: it is a
    // freshly minted 26-character id on each server.
    let connection_id = rust["data"]["connection_id"].as_str().expect("a string");
    assert_eq!(connection_id.len(), 26, "connection_id is a Mattermost id");
    assert_eq!(
        connection_id,
        rust["data"]["connection_id"].as_str().expect("a string"),
    );
}

/// Connect and take the `hello` frame verbatim, then drop the socket.
///
/// It reads until it *finds* hello rather than taking the first frame: on a busy server the
/// admin's socket is already receiving other tests' broadcasts, and taking frame zero made this
/// test fail against a `new_user` event.
async fn raw_hello(base: &str, token: &str) -> String {
    let url = format!("{}/api/v4/websocket", base.replace("http://", "ws://"));
    let mut request =
        tokio_tungstenite::tungstenite::client::IntoClientRequest::into_client_request(
            url.as_str(),
        )
        .expect("a websocket request");
    request.headers_mut().insert(
        "Authorization",
        format!("Bearer {token}").parse().expect("a header value"),
    );
    let (mut socket, _) = tokio_tungstenite::connect_async(request)
        .await
        .unwrap_or_else(|e| panic!("{base} websocket: {e}"));
    loop {
        match tokio::time::timeout(
            Duration::from_secs(5),
            futures_util::StreamExt::next(&mut socket),
        )
        .await
        {
            Ok(Some(Ok(tokio_tungstenite::tungstenite::Message::Text(text)))) => {
                let text = text.to_string();
                let parsed: serde_json::Value =
                    serde_json::from_str(&text).expect("a frame decodes");
                if parsed.get("event").and_then(|e| e.as_str()) == Some("hello") {
                    return text;
                }
                continue;
            }
            Ok(Some(Ok(_))) => continue,
            other => panic!("{base} sent no hello frame: {other:?}"),
        }
    }
}

#[tokio::test]
async fn presence_and_an_unknown_action_answer_identically() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;

    let mut go = SocketProbe::connect(GO, &token).await;
    let mut rust = SocketProbe::connect(RUST, &token).await;

    for probe in [&mut go, &mut rust] {
        probe
            .send(serde_json::json!({
                "seq": 1,
                "action": "presence",
                "data": {"channel_id": "abcdefghijklmnopqrstuvwxyz"},
            }))
            .await;
        probe
            .send(serde_json::json!({"seq": 2, "action": "no_such_action", "data": {}}))
            .await;
        probe.collect_for(Duration::from_millis(600)).await;
    }

    // Responses only — see `SocketProbe::responses`. A socket is not isolated the way a request
    // is: anything else running against the same server broadcasts onto this connection, and a
    // whole-suite run put nine frames here where the test expected two. Counting frames was
    // really counting the rest of the suite.
    let go_responses = go.responses();
    let rust_responses = rust.responses();
    assert_eq!(
        go_responses.len(),
        2,
        "Go answered both requests: {:?}",
        go.raw
    );
    assert_eq!(
        rust_responses.len(),
        2,
        "we answered both requests: {:?}",
        rust.raw
    );

    // Responses take `json.Encoder.Encode` too, so they carry the same trailing newline.
    for ((go_raw, _), (rust_raw, _)) in go_responses.iter().zip(rust_responses.iter()) {
        assert_eq!(
            go_raw.ends_with('\n'),
            rust_raw.ends_with('\n'),
            "response framing differs:\n go: {go_raw:?}\nrust: {rust_raw:?}"
        );
    }

    let go_frames: Vec<serde_json::Value> =
        go_responses.into_iter().map(|(_, frame)| frame).collect();
    let rust_frames: Vec<serde_json::Value> =
        rust_responses.into_iter().map(|(_, frame)| frame).collect();

    assert_eq!(
        go_frames[0], rust_frames[0],
        "the presence response differs"
    );
    assert_eq!(
        rust_frames[0],
        serde_json::json!({"status": "OK", "seq_reply": 1}),
        "an accepted action answers OK with no data key at all"
    );

    // The unknown-action error. `message` is Go's translated string and ours is the id — the
    // project-wide i18n gap — so the two comparable fields are compared and `message` is not.
    for field in ["id", "detailed_error", "status_code"] {
        assert_eq!(
            go_frames[1]["error"][field], rust_frames[1]["error"][field],
            "the unknown-action error's {field} differs"
        );
    }
    assert_eq!(go_frames[1]["status"], rust_frames[1]["status"]);
    assert_eq!(go_frames[1]["seq_reply"], rust_frames[1]["seq_reply"]);
    assert_eq!(
        rust_frames[1]["error"]["id"],
        "api.web_socket_router.bad_action.app_error"
    );
    assert_eq!(rust_frames[1]["error"]["status_code"], 500);
}

#[tokio::test]
async fn a_request_with_no_seq_is_refused_the_same_way() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;

    let mut go = SocketProbe::connect(GO, &token).await;
    let mut rust = SocketProbe::connect(RUST, &token).await;

    // `seq <= 0` is rejected before the action is looked at, so a *valid* action with seq 0 is
    // still a `bad_seq`. That ordering is the point of the test.
    for probe in [&mut go, &mut rust] {
        probe
            .send(serde_json::json!({"seq": 0, "action": "presence", "data": {}}))
            .await;
        probe.collect_for(Duration::from_millis(600)).await;
    }

    // A `bad_seq` refusal carries no `seq_reply` — the field is `omitempty` and the seq was 0 —
    // so it is recognised by `status` instead. That is what `SocketProbe::responses` looks for.
    let go_frames: Vec<serde_json::Value> =
        go.responses().into_iter().map(|(_, frame)| frame).collect();
    let rust_frames: Vec<serde_json::Value> = rust
        .responses()
        .into_iter()
        .map(|(_, frame)| frame)
        .collect();
    assert_eq!(go_frames.len(), 1, "Go answered once: {:?}", go.raw);
    assert_eq!(rust_frames.len(), 1, "we answered once: {:?}", rust.raw);
    assert_eq!(
        go_frames[0]["error"]["id"], rust_frames[0]["error"]["id"],
        "the bad-seq error id differs"
    );
    assert_eq!(
        rust_frames[0]["error"]["id"],
        "api.web_socket_router.bad_seq.app_error"
    );
    assert_eq!(go_frames[0]["status"], rust_frames[0]["status"]);
}

#[tokio::test]
async fn a_query_string_token_is_refused_before_the_upgrade() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;

    // Not a websocket rule: `handlers.go:281` rejects a non-OAuth session presented in the query
    // string on *every* route. This route is where it shows, because it is the only one that
    // otherwise accepts an absent session.
    for base in [GO, RUST] {
        let response = http
            .get(format!("{base}/api/v4/websocket?access_token={token}"))
            .send()
            .await
            .unwrap_or_else(|e| panic!("{base} unreachable: {e}"));
        assert_eq!(
            response.status().as_u16(),
            401,
            "{base} should refuse a query-string token"
        );
        let body: serde_json::Value = response.json().await.expect("an AppError body");
        assert_eq!(
            body["id"], "api.context.token_provided.app_error",
            "{base} refused with the wrong id"
        );
    }
}
