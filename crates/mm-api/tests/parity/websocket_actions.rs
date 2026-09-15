//! Cross-server parity for the six `wsapi` actions on `GET /api/v4/websocket`, and for the parts
//! of a socket's life they exposed: the five-second authentication window, the user id of a
//! connection that authenticates over the socket, how a frame is decoded, and the session re-read
//! in front of every action.
//!
//! ```sh
//! scripts/parity.sh --test parity websocket_actions
//! ```
//!
//! # `posted_notify_ack` with a non-string `status` is deliberately not sent
//!
//! Go's unchecked type assertion panics on it (`mm_api::wsapi::ack_type_assertion_panics`). The
//! panic is recovered on the handler goroutine, but provoking one on a Go server other checkouts
//! share is not a risk worth one branch; the decision is unit-tested instead.
//!
//! # Answers are found by `seq_reply`
//!
//! A socket is not isolated the way a request is (see `SocketProbe::responses`), so every answer
//! here is picked out by the `seq` its request carried, never by position.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use crate::common;

use common::{
    BROADCAST_STREAM, BUSY_STATE, GO, PlainUser, RUST, SocketProbe, add_user_to_channel, client,
    create_channel_typed, create_plain_user, create_team, go_minted_token, login_plain_user,
    purge_api_fixtures, stack_enabled,
};

/// A 26-character id that names nothing.
const NOBODY: &str = "zzzzzzzzzzzzzzzzzzzzzzzzzz";

struct Fixture {
    channel_id: String,
    /// A private channel no plain user is in.
    closed_id: String,
    typist: PlainUser,
    listener: PlainUser,
    /// Never connects and never has a status set.
    quiet: PlainUser,
    /// Set to `away` on both servers, for `get_statuses`.
    shown: PlainUser,
    /// Set to `offline` on both servers, for `get_statuses`.
    hidden: PlainUser,
    /// Set to `dnd` on both servers, for `get_statuses_by_ids`.
    dnd: PlainUser,
    /// Set to `away` through Go only, so the row is in the table and not in our cache.
    go_only: PlainUser,
    /// Set to `away` through us only — the mirror of `go_only`.
    rust_only: PlainUser,
    stater: PlainUser,
    challenger: PlainUser,
}

static FIXTURE: tokio::sync::OnceCell<Fixture> = tokio::sync::OnceCell::const_new();

/// Tag of the user whose extra sessions the revocation test logs in and out.
const REVOKED_TAG: &str = "wsar";

async fn fixture(client: &reqwest::Client, token: &str) -> &'static Fixture {
    FIXTURE
        .get_or_init(|| async {
            purge_api_fixtures().await;
            let team_id = create_team(client, token, "wsa").await;
            let channel_id = create_channel_typed(client, token, &team_id, "wsa", "O").await;
            let closed_id = create_channel_typed(client, token, &team_id, "wsashut", "P").await;
            let typist = create_plain_user(client, token, &team_id, "wsat").await;
            let listener = create_plain_user(client, token, &team_id, "wsal").await;
            add_user_to_channel(client, token, &channel_id, &typist.id).await;
            add_user_to_channel(client, token, &channel_id, &listener.id).await;
            let quiet = create_plain_user(client, token, &team_id, "wsaq").await;
            let shown = create_plain_user(client, token, &team_id, "wsas").await;
            let hidden = create_plain_user(client, token, &team_id, "wsah").await;
            let dnd = create_plain_user(client, token, &team_id, "wsad").await;
            let go_only = create_plain_user(client, token, &team_id, "wsag").await;
            let rust_only = create_plain_user(client, token, &team_id, "wsaw").await;
            let stater = create_plain_user(client, token, &team_id, "wsau").await;
            let challenger = create_plain_user(client, token, &team_id, "wsac").await;
            create_plain_user(client, token, &team_id, REVOKED_TAG).await;
            Fixture {
                channel_id,
                closed_id,
                typist,
                listener,
                quiet,
                shown,
                hidden,
                dnd,
                go_only,
                rust_only,
                stater,
                challenger,
            }
        })
        .await
}

/// Send a request and wait for the frame that answers it.
async fn ask(probe: &mut SocketProbe, seq: i64, action: &str, data: Option<Value>) -> Value {
    let mut request = json!({"seq": seq, "action": action});
    if let Some(data) = data {
        request["data"] = data;
    }
    probe.send(request).await;
    answer(probe, seq).await
}

async fn answer(probe: &mut SocketProbe, seq: i64) -> Value {
    let arrived = probe
        .collect_until(Duration::from_secs(5), move |frames| {
            frames.iter().any(|f| f["seq_reply"] == seq)
        })
        .await;
    assert!(arrived, "no answer to seq {seq}: {:?}", probe.raw);
    probe
        .frames()
        .into_iter()
        .find(|f| f["seq_reply"] == seq)
        .expect("found by the wait above")
}

/// The raw text of the answer to `seq`, for the framing assertions a value comparison cannot make.
fn raw_answer(probe: &SocketProbe, seq: i64) -> String {
    probe
        .raw
        .iter()
        .find(|raw| serde_json::from_str::<Value>(raw).is_ok_and(|frame| frame["seq_reply"] == seq))
        .cloned()
        .expect("the answer was collected")
}

/// Both servers refused, with `id`, and agree on everything but the translated message.
fn assert_same_refusal(what: &str, go: &Value, rust: &Value, id: &str, status_code: i64) {
    assert_eq!(go["status"], "FAIL", "{what}: Go did not refuse: {go}");
    assert_eq!(rust["status"], "FAIL", "{what}: we did not refuse: {rust}");
    assert_eq!(go["seq_reply"], rust["seq_reply"], "{what}: seq_reply");
    assert_eq!(
        go["error"]["id"], id,
        "{what}: Go refused differently: {go}"
    );
    for field in ["id", "detailed_error", "status_code", "request_id"] {
        assert_eq!(
            go["error"][field], rust["error"][field],
            "{what}: error.{field}\n go: {go}\nrust: {rust}"
        );
    }
    assert_eq!(rust["error"]["status_code"], status_code, "{what}");
    let keys = |frame: &Value| {
        let mut keys: Vec<String> = frame["error"]
            .as_object()
            .map(|o| o.keys().cloned().collect())
            .unwrap_or_default();
        keys.sort();
        keys
    };
    assert_eq!(
        keys(go),
        keys(rust),
        "{what}: the error objects' keys differ"
    );
    assert!(
        go.get("data").is_none() && rust.get("data").is_none(),
        "{what}: a refusal carries no data"
    );
}

async fn put_status(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    user_id: &str,
    status: &str,
) {
    let response = client
        .put(format!("{base}/api/v4/users/{user_id}/status"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&json!({"user_id": user_id, "status": status, "dnd_end_time": 0}))
        .send()
        .await
        .unwrap_or_else(|e| panic!("{base} is unreachable: {e}"));
    assert!(
        response.status().is_success(),
        "{base}: setting {user_id} to {status} failed: {}",
        response.text().await.unwrap_or_default()
    );
}

async fn get_status(client: &reqwest::Client, base: &str, token: &str, user_id: &str) -> Value {
    client
        .get(format!("{base}/api/v4/users/{user_id}/status"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .unwrap_or_else(|e| panic!("{base} is unreachable: {e}"))
        .json()
        .await
        .expect("a status body")
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("after the epoch")
        .as_millis() as i64
}

// ---------------------------------------------------------------------------------------------
// ping, posted_notify_ack
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn ping_answers_pong_with_the_same_four_keys() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let mut go = SocketProbe::connect(GO, &token).await;
    let mut rust = SocketProbe::connect(RUST, &token).await;

    let go_answer = ask(&mut go, 7, "ping", None).await;
    let rust_answer = ask(&mut rust, 7, "ping", None).await;

    for (name, frame) in [("Go", &go_answer), ("us", &rust_answer)] {
        assert_eq!(frame["status"], "OK", "{name}: {frame}");
        let keys: Vec<&str> = frame["data"]
            .as_object()
            .unwrap_or_else(|| panic!("{name} answered no data: {frame}"))
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            ["node_id", "server_time", "text", "version"],
            "{name}"
        );
        assert_eq!(frame["data"]["text"], "pong", "{name}");
        assert_eq!(
            frame["data"]["node_id"], "",
            "{name}: the literal, not a node id"
        );
        let server_time = frame["data"]["server_time"].as_i64().expect("an integer");
        assert!(
            (server_time - now_millis()).abs() < 60_000,
            "{name}: server_time is epoch milliseconds: {server_time}"
        );
    }
    assert_eq!(go_answer["data"]["version"], rust_answer["data"]["version"]);
    assert_eq!(go_answer["seq_reply"], rust_answer["seq_reply"]);

    // A response goes through `json.Encoder`, so both end in a newline.
    assert!(raw_answer(&go, 7).ends_with('\n'));
    assert!(raw_answer(&rust, 7).ends_with('\n'));
}

#[tokio::test]
async fn posted_notify_ack_answers_ok_with_no_data_for_every_well_typed_shape() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let mut go = SocketProbe::connect(GO, &token).await;
    let mut rust = SocketProbe::connect(RUST, &token).await;

    let shapes = [
        None,
        Some(json!({})),
        Some(json!({"status": "success"})),
        Some(json!({"status": "error"})),
        Some(json!({"status": "error", "reason": "fetch_error", "post_id": "x"})),
        // A null status is nil: the early return, so the non-string reason is never asserted.
        Some(json!({"status": null, "reason": 5})),
    ];
    for (i, data) in shapes.into_iter().enumerate() {
        let seq = i as i64 + 1;
        let go_answer = ask(&mut go, seq, "posted_notify_ack", data.clone()).await;
        let rust_answer = ask(&mut rust, seq, "posted_notify_ack", data.clone()).await;
        assert_eq!(
            go_answer,
            json!({"status": "OK", "seq_reply": seq}),
            "Go, {data:?}"
        );
        assert_eq!(rust_answer, go_answer, "{data:?}");
    }
}

// ---------------------------------------------------------------------------------------------
// get_statuses_by_ids, get_statuses
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn get_statuses_by_ids_refuses_anything_that_yields_no_string() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let mut go = SocketProbe::connect(GO, &token).await;
    let mut rust = SocketProbe::connect(RUST, &token).await;

    let shapes = [
        None,
        Some(json!({})),
        Some(json!({"user_ids": NOBODY})),
        Some(json!({"user_ids": [1, 2, null]})),
        Some(json!({"user_ids": []})),
        Some(json!({"user_ids": {"0": NOBODY}})),
    ];
    for (i, data) in shapes.into_iter().enumerate() {
        let seq = i as i64 + 1;
        let go_answer = ask(&mut go, seq, "get_statuses_by_ids", data.clone()).await;
        let rust_answer = ask(&mut rust, seq, "get_statuses_by_ids", data.clone()).await;
        assert_same_refusal(
            &format!("{data:?}"),
            &go_answer,
            &rust_answer,
            "api.websocket_handler.invalid_param.app_error",
            400,
        );
    }
}

#[tokio::test]
async fn get_statuses_by_ids_answers_a_map_with_offline_for_the_unknown() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let f = fixture(&http, &token).await;
    for base in [GO, RUST] {
        put_status(&http, base, &token, &f.dnd.id, "dnd").await;
    }

    let mut go = SocketProbe::connect(GO, &token).await;
    let mut rust = SocketProbe::connect(RUST, &token).await;

    // A set status, a user with no status at all, an id that names nobody, a repeat, and a
    // non-string that the array filter drops.
    let data = json!({"user_ids": [f.dnd.id, f.quiet.id, NOBODY, f.dnd.id, 5]});
    let go_answer = ask(&mut go, 1, "get_statuses_by_ids", Some(data.clone())).await;
    let rust_answer = ask(&mut rust, 1, "get_statuses_by_ids", Some(data)).await;

    assert_eq!(go_answer["status"], "OK", "Go: {go_answer}");
    assert_eq!(rust_answer, go_answer);
    let mut expected = serde_json::Map::new();
    expected.insert(f.dnd.id.clone(), json!("dnd"));
    expected.insert(f.quiet.id.clone(), json!("offline"));
    expected.insert(NOBODY.to_owned(), json!("offline"));
    assert_eq!(rust_answer["data"], Value::Object(expected));
}

#[tokio::test]
async fn get_statuses_holds_what_the_server_set_and_leaves_out_offline() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let f = fixture(&http, &token).await;

    // Each server's answer is its own cache, so each is primed through its own REST route —
    // exactly what a client of that server would have done.
    for base in [GO, RUST] {
        put_status(&http, base, &token, &f.shown.id, "away").await;
        put_status(&http, base, &token, &f.hidden.id, "offline").await;
    }

    let mut go = SocketProbe::connect(GO, &token).await;
    let mut rust = SocketProbe::connect(RUST, &token).await;
    let go_answer = ask(&mut go, 1, "get_statuses", None).await;
    let rust_answer = ask(&mut rust, 1, "get_statuses", None).await;

    for (name, frame) in [("Go", &go_answer), ("us", &rust_answer)] {
        assert_eq!(frame["status"], "OK", "{name}: {frame}");
        assert_eq!(frame["data"][&f.shown.id], "away", "{name}: {frame}");
        assert!(
            frame["data"].get(&f.hidden.id).is_none(),
            "{name}: an offline status is omitted: {frame}"
        );
        assert!(
            frame["data"]
                .as_object()
                .is_some_and(|o| o.values().all(|v| v != "offline")),
            "{name}: no offline value at all"
        );
    }
}

/// `GetStatusesByIds` puts every row it had to read from the table into the cache, so the next
/// `get_statuses` includes a user it did not before. Exercised in both directions: a status set
/// through one server is in the shared table and only in *that* server's cache, which is exactly
/// the miss the other server's call has to read.
#[tokio::test]
async fn get_statuses_by_ids_caches_the_rows_it_read_from_the_table() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let f = fixture(&http, &token).await;

    for (setter, asked, user) in [(GO, RUST, &f.go_only), (RUST, GO, &f.rust_only)] {
        put_status(&http, setter, &token, &user.id, "away").await;

        let mut probe = SocketProbe::connect(asked, &token).await;
        let before = ask(&mut probe, 1, "get_statuses", None).await;
        assert!(
            before["data"].get(&user.id).is_none(),
            "{asked}'s cache already held a status only {setter} set: {before}"
        );

        let by_ids = ask(
            &mut probe,
            2,
            "get_statuses_by_ids",
            Some(json!({"user_ids": [user.id]})),
        )
        .await;
        assert_eq!(
            by_ids["data"][&user.id], "away",
            "{asked} answers the miss from the table: {by_ids}"
        );

        let after = ask(&mut probe, 3, "get_statuses", None).await;
        assert_eq!(
            after["data"][&user.id], "away",
            "{asked} did not cache the row it read: {after}"
        );
    }
}

// ---------------------------------------------------------------------------------------------
// user_typing
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn user_typing_over_the_socket_reaches_members_but_not_the_typist() {
    if !stack_enabled() {
        return;
    }
    let _busy = BUSY_STATE.read().await;
    let _broadcast = BROADCAST_STREAM.lock().await;
    let http = client();
    let token = go_minted_token(&http).await;
    let f = fixture(&http, &token).await;

    let typing_here = |channel_id: String| {
        move |frames: &[Value]| {
            frames
                .iter()
                .any(|fr| fr["event"] == "typing" && fr["broadcast"]["channel_id"] == channel_id)
        }
    };

    let mut frames = Vec::new();
    for base in [GO, RUST] {
        let mut listener = SocketProbe::connect(base, &f.listener.token).await;
        let mut typist = SocketProbe::connect(base, &f.typist.token).await;

        // `parent_id` is any string, unchecked; a non-string is the empty string.
        let first = ask(
            &mut typist,
            1,
            "user_typing",
            Some(json!({"channel_id": f.channel_id, "parent_id": "not-an-id"})),
        )
        .await;
        assert_eq!(first, json!({"status": "OK", "seq_reply": 1}), "{base}");
        assert!(
            listener
                .collect_until(Duration::from_secs(3), typing_here(f.channel_id.clone()))
                .await,
            "{base} delivered no typing frame: {:?}",
            listener.raw
        );
        listener.raw.clear();

        let second = ask(
            &mut typist,
            2,
            "user_typing",
            Some(json!({"channel_id": f.channel_id, "parent_id": 5})),
        )
        .await;
        assert_eq!(second, json!({"status": "OK", "seq_reply": 2}), "{base}");
        let second_frame = listener
            .collect_until(Duration::from_secs(3), typing_here(f.channel_id.clone()))
            .await;
        assert!(second_frame, "{base}: no second frame: {:?}", listener.raw);
        let typing: Vec<Value> = listener.events_named("typing");

        typist.collect_for(Duration::from_millis(500)).await;
        assert!(
            typist.events_named("typing").is_empty(),
            "{base} sent the typist their own typing: {:?}",
            typist.raw
        );
        frames.push(typing[0].clone());
    }

    let (go, rust) = (&frames[0], &frames[1]);
    assert_eq!(go["data"], rust["data"], "the typing data differs");
    assert_eq!(go["broadcast"], rust["broadcast"], "the addressing differs");
    assert_eq!(rust["data"]["parent_id"], "");
    assert_eq!(rust["data"]["user_id"], f.typist.id.as_str());
}

#[tokio::test]
async fn user_typing_refuses_every_bad_channel_as_an_invalid_channel_id() {
    if !stack_enabled() {
        return;
    }
    let _busy = BUSY_STATE.read().await;
    let http = client();
    let token = go_minted_token(&http).await;
    let f = fixture(&http, &token).await;

    let mut go = SocketProbe::connect(GO, &f.typist.token).await;
    let mut rust = SocketProbe::connect(RUST, &f.typist.token).await;

    // Absent, the wrong type, not an id, a channel the typist cannot post in, a channel that does
    // not exist. The last two are a 403 and a 404 on the REST route; here all five are the same
    // 400.
    let shapes = [
        None,
        Some(json!({"channel_id": 5})),
        Some(json!({"channel_id": "short"})),
        Some(json!({"channel_id": f.closed_id})),
        Some(json!({"channel_id": NOBODY})),
    ];
    for (i, data) in shapes.into_iter().enumerate() {
        let seq = i as i64 + 1;
        let go_answer = ask(&mut go, seq, "user_typing", data.clone()).await;
        let rust_answer = ask(&mut rust, seq, "user_typing", data.clone()).await;
        assert_same_refusal(
            &format!("{data:?}"),
            &go_answer,
            &rust_answer,
            "api.websocket_handler.invalid_param.app_error",
            400,
        );
    }
}

// ---------------------------------------------------------------------------------------------
// user_update_active_status
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn user_update_active_status_needs_a_boolean_and_sets_the_status_it_names() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let f = fixture(&http, &token).await;

    let mut go = SocketProbe::connect(GO, &f.stater.token).await;
    let mut rust = SocketProbe::connect(RUST, &f.stater.token).await;

    for (seq, data) in [
        (1, None),
        (2, Some(json!({"user_is_active": "false"}))),
        (3, Some(json!({"user_is_active": 0, "manual": true}))),
    ] {
        let go_answer = ask(&mut go, seq, "user_update_active_status", data.clone()).await;
        let rust_answer = ask(&mut rust, seq, "user_update_active_status", data.clone()).await;
        assert_same_refusal(
            &format!("{data:?}"),
            &go_answer,
            &rust_answer,
            "api.websocket_handler.invalid_param.app_error",
            400,
        );
    }

    // Connecting marked the user online on both (`NewWebConn`), off the socket's own path — so
    // poll briefly rather than read once.
    for base in [GO, RUST] {
        let mut status = Value::Null;
        for _ in 0..30 {
            status = get_status(&http, base, &f.stater.token, &f.stater.id).await;
            if status["status"] == "online" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert_eq!(
            status["status"], "online",
            "{base}: connecting did not mark the user online: {status}"
        );
    }

    // A non-manual away is only "if needed", and a user active a moment ago does not need it.
    // This is also the only request here that leaves `manual` out, so it is what pins its default.
    for (base, probe) in [(GO, &mut go), (RUST, &mut rust)] {
        let answer = ask(
            probe,
            6,
            "user_update_active_status",
            Some(json!({"user_is_active": false})),
        )
        .await;
        assert_eq!(answer, json!({"status": "OK", "seq_reply": 6}), "{base}");
        let status = get_status(&http, base, &f.stater.token, &f.stater.id).await;
        assert_eq!(
            status["status"], "online",
            "{base}: a non-manual away went through: {status}"
        );
        assert_eq!(status["manual"], false, "{base}: {status}");
    }

    // A manual away is unconditional, and the user's own socket hears it.
    let away = |frames: &[Value]| {
        frames
            .iter()
            .any(|fr| fr["event"] == "status_change" && fr["data"]["status"] == "away")
    };
    let mut changes = Vec::new();
    for (base, probe) in [(GO, &mut go), (RUST, &mut rust)] {
        let answer = ask(
            probe,
            4,
            "user_update_active_status",
            Some(json!({"user_is_active": false, "manual": true})),
        )
        .await;
        assert_eq!(answer, json!({"status": "OK", "seq_reply": 4}), "{base}");
        assert!(
            probe.collect_until(Duration::from_secs(3), away).await,
            "{base}: no status_change to away: {:?}",
            probe.raw
        );
        let status = get_status(&http, base, &f.stater.token, &f.stater.id).await;
        assert_eq!(status["status"], "away", "{base}: {status}");
        assert_eq!(status["manual"], true, "{base}: {status}");
        changes.push(
            probe
                .events_named("status_change")
                .into_iter()
                .find(|fr| fr["data"]["status"] == "away")
                .expect("found above"),
        );
    }
    assert_eq!(changes[0]["data"], changes[1]["data"]);
    assert_eq!(changes[0]["broadcast"], changes[1]["broadcast"]);

    // Online clears `manual` even when asked for a manual online.
    for (base, probe) in [(GO, &mut go), (RUST, &mut rust)] {
        let answer = ask(
            probe,
            5,
            "user_update_active_status",
            Some(json!({"user_is_active": true, "manual": true})),
        )
        .await;
        assert_eq!(answer, json!({"status": "OK", "seq_reply": 5}), "{base}");
        let status = get_status(&http, base, &f.stater.token, &f.stater.id).await;
        assert_eq!(status["status"], "online", "{base}: {status}");
        assert_eq!(status["manual"], false, "{base}: {status}");
    }
}

// ---------------------------------------------------------------------------------------------
// the connection around the actions
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn an_anonymous_socket_is_answered_nothing_and_closed_after_five_seconds() {
    if !stack_enabled() {
        return;
    }

    let run = async |base: &'static str| {
        let opened = tokio::time::Instant::now();
        let mut probe = SocketProbe::connect_anonymous(base).await;
        // Refused by the router, a plugin action, and a presence hint: none is answered, because
        // the hub drops a response for a connection it does not hold.
        probe.send(json!({"seq": 1, "action": "ping"})).await;
        probe
            .send(json!({"seq": 2, "action": "custom_anything"}))
            .await;
        probe
            .send(json!({"seq": 3, "action": "presence", "data": {"channel_id": NOBODY}}))
            .await;
        let closed = probe.closed_within(Duration::from_secs(8)).await;
        (base, closed, opened.elapsed(), probe.raw)
    };

    let (go, rust) = tokio::join!(run(GO), run(RUST));
    for (base, closed, elapsed, raw) in [go, rust] {
        assert!(closed, "{base} kept an unauthenticated socket open past 8s");
        assert!(
            elapsed >= Duration::from_millis(4500),
            "{base} closed it too early, after {elapsed:?}"
        );
        assert!(
            raw.is_empty(),
            "{base} answered an anonymous socket: {raw:?}"
        );
    }
}

#[tokio::test]
async fn a_socket_that_authenticates_by_challenge_is_that_user_and_stays_open() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let f = fixture(&http, &token).await;

    let run = async |base: &'static str| {
        let opened = tokio::time::Instant::now();
        let mut probe = SocketProbe::connect_anonymous(base).await;
        let ok = ask(
            &mut probe,
            1,
            "authentication_challenge",
            Some(json!({"token": f.challenger.token})),
        )
        .await;
        assert_eq!(ok, json!({"status": "OK", "seq_reply": 1}), "{base}");

        // `hello` is addressed to the user — delivered only if the connection is indexed under
        // them, which is the point.
        assert!(
            probe
                .collect_until(Duration::from_secs(3), |frames| frames
                    .iter()
                    .any(|fr| fr["event"] == "hello"))
                .await,
            "{base}: no hello after the challenge: {:?}",
            probe.raw
        );
        let hello = probe.events_named("hello").remove(0);

        // A user-addressed event of the user's own making.
        let away = ask(
            &mut probe,
            2,
            "user_update_active_status",
            Some(json!({"user_is_active": false, "manual": true})),
        )
        .await;
        assert_eq!(away["status"], "OK", "{base}: {away}");
        assert!(
            probe
                .collect_until(Duration::from_secs(3), |frames| frames.iter().any(|fr| {
                    fr["event"] == "status_change" && fr["data"]["status"] == "away"
                }))
                .await,
            "{base}: the user's own status_change did not arrive: {:?}",
            probe.raw
        );

        // Past the five-second window, and still answered.
        tokio::time::sleep_until(opened + Duration::from_secs(6)).await;
        let pong = ask(&mut probe, 3, "ping", None).await;
        assert_eq!(pong["status"], "OK", "{base}: {pong}");
        hello
    };

    let (go_hello, rust_hello) = tokio::join!(run(GO), run(RUST));
    assert_eq!(go_hello["broadcast"], rust_hello["broadcast"]);
    assert_eq!(rust_hello["broadcast"]["user_id"], f.challenger.id.as_str());

    for base in [GO, RUST] {
        put_status(&http, base, &token, &f.challenger.id, "online").await;
    }
}

#[tokio::test]
async fn a_frame_is_decoded_as_one_json_value_and_an_undecodable_one_closes_the_socket() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;

    for base in [GO, RUST] {
        let mut probe = SocketProbe::connect(base, &token).await;
        // Whatever follows the first value is never read.
        probe
            .send_text(r#"{"seq": 1, "action": "ping"} this is never read"#)
            .await;
        let pong = answer(&mut probe, 1).await;
        assert_eq!(pong["status"], "OK", "{base}: {pong}");

        // A plugin action is not routed, so it is neither answered nor refused — the next request
        // is answered, and nothing for seq 2 ever was.
        probe
            .send(json!({"seq": 2, "action": "custom_anything"}))
            .await;
        let pong = ask(&mut probe, 3, "ping", None).await;
        assert_eq!(pong["status"], "OK", "{base}: {pong}");
        assert!(
            probe.frames().iter().all(|fr| fr["seq_reply"] != 2),
            "{base} answered a plugin action: {:?}",
            probe.raw
        );

        probe.send_text("not json").await;
        assert!(
            probe.closed_within(Duration::from_secs(3)).await,
            "{base} kept the socket open after an undecodable frame"
        );
    }
}

#[tokio::test]
async fn an_action_on_a_logged_out_session_is_refused() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    fixture(&http, &token).await;

    let mut answers = Vec::new();
    for base in [GO, RUST] {
        // A session of its own per server, logged out *through that server*, so each process has
        // done whatever its logout does to its own hub.
        let session_token = login_plain_user(&http, REVOKED_TAG).await;
        let mut probe = SocketProbe::connect(base, &session_token).await;
        let status = http
            .post(format!("{base}/api/v4/users/logout"))
            .header("Authorization", format!("Bearer {session_token}"))
            .send()
            .await
            .unwrap_or_else(|e| panic!("{base} is unreachable: {e}"))
            .status();
        assert!(status.is_success(), "{base}: logout answered {status}");
        answers.push(ask(&mut probe, 1, "ping", None).await);
    }

    let (go, rust) = (&answers[0], &answers[1]);
    assert_eq!(
        go["status"], "FAIL",
        "Go answered a logged-out socket: {go}"
    );
    assert_same_refusal(
        "ping after logout",
        go,
        rust,
        go["error"]["id"].as_str().unwrap_or_default(),
        go["error"]["status_code"].as_i64().unwrap_or_default(),
    );
}
