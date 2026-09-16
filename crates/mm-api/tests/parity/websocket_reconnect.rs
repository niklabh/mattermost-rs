//! Cross-server parity for resuming a dropped websocket: `connection_id` and `sequence_number` on
//! `GET /api/v4/websocket`, the dead queue behind them, and what a disconnect does to the user's
//! status.
//!
//! ```sh
//! scripts/parity.sh --test parity websocket_reconnect
//! ```
//!
//! # How a test knows the old connection has been unregistered
//!
//! A resume finds only a connection the hub has already marked inactive, and that happens a
//! moment after the client closes — on both servers, off the request path. Reconnecting too soon is
//! a fresh connection, not a resume. Each test's user holds no other socket, so the unregister arm
//! sets them `offline`, and waiting for `offline` over REST is waiting for that arm. The short
//! pause after it covers the broadcast of that same status, which Go publishes just after the cache
//! write REST reads.
//!
//! # What is compared
//!
//! Sequence numbers depend on how many events each server happened to send, so each server's
//! resumed frames are compared with **what that server sent before the drop** — byte for byte,
//! which is what shows the replay keeps each frame's encoding — and the two servers are compared
//! on the shape: which frames came back, whether `hello` did, and where the counter resumed.

use std::time::Duration;

use serde_json::{Value, json};

use crate::common;

use common::{
    GO, PlainUser, RUST, SocketProbe, client, create_plain_user, create_team, go_minted_token,
    purge_api_fixtures, stack_enabled,
};

/// A 26-character id that names no connection.
const NOBODY: &str = "zzzzzzzzzzzzzzzzzzzzzzzzzz";

struct Fixture {
    replay: PlainUser,
    lossless: PlainUser,
    loss: PlainUser,
    unknown: PlainUser,
    malformed: PlainUser,
}

static FIXTURE: tokio::sync::OnceCell<Fixture> = tokio::sync::OnceCell::const_new();

async fn fixture(client: &reqwest::Client, token: &str) -> &'static Fixture {
    FIXTURE
        .get_or_init(|| async {
            purge_api_fixtures().await;
            let team_id = create_team(client, token, "wsr").await;
            Fixture {
                replay: create_plain_user(client, token, &team_id, "wsrp").await,
                lossless: create_plain_user(client, token, &team_id, "wsrl").await,
                loss: create_plain_user(client, token, &team_id, "wsrx").await,
                unknown: create_plain_user(client, token, &team_id, "wsru").await,
                malformed: create_plain_user(client, token, &team_id, "wsrm").await,
            }
        })
        .await
}

/// `(seq, raw)` for every event frame collected, in arrival order.
fn events(probe: &SocketProbe) -> Vec<(i64, String)> {
    probe
        .raw
        .iter()
        .filter_map(|raw| {
            let frame: Value = serde_json::from_str(raw).ok()?;
            frame.get("event")?;
            Some((frame["seq"].as_i64()?, raw.clone()))
        })
        .collect()
}

fn parsed(raw: &str) -> Value {
    serde_json::from_str(raw).expect("a frame decodes")
}

async fn status_of(http: &reqwest::Client, base: &str, token: &str, user_id: &str) -> String {
    let body: Value = http
        .get(format!("{base}/api/v4/users/{user_id}/status"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .unwrap_or_else(|e| panic!("{base} is unreachable: {e}"))
        .json()
        .await
        .expect("a status body");
    body["status"].as_str().unwrap_or_default().to_owned()
}

async fn wait_for_status(
    http: &reqwest::Client,
    base: &str,
    token: &str,
    user_id: &str,
    want: &str,
) {
    let mut last = String::new();
    for _ in 0..50 {
        last = status_of(http, base, token, user_id).await;
        if last == want {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("{base}: {user_id} never became {want}; last {last}");
}

async fn put_status(http: &reqwest::Client, base: &str, token: &str, user_id: &str, status: &str) {
    let response = http
        .put(format!("{base}/api/v4/users/{user_id}/status"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&json!({"user_id": user_id, "status": status, "dnd_end_time": 0}))
        .send()
        .await
        .unwrap_or_else(|e| panic!("{base} is unreachable: {e}"));
    assert!(
        response.status().is_success(),
        "{base}: setting {status} failed"
    );
}

/// Connect as `user`, let the connect-time `online` settle, and raise four `status_change`
/// events — three manual aways and a manual online. Returns the probe, the connection id from `hello`, and every event
/// frame the connection has been sent.
async fn a_connection_with_history(
    http: &reqwest::Client,
    base: &str,
    user: &PlainUser,
) -> (SocketProbe, String, Vec<(i64, String)>) {
    let mut probe = SocketProbe::connect_raw_with_query(base, &user.token, "").await;
    assert!(
        probe
            .collect_until(Duration::from_secs(3), |frames| frames
                .iter()
                .any(|f| f["event"] == "hello"))
            .await,
        "{base}: no hello"
    );
    let hello = probe.events_named("hello").remove(0);
    let connection_id = hello["data"]["connection_id"]
        .as_str()
        .expect("an id")
        .to_owned();
    wait_for_status(http, base, &user.token, &user.id, "online").await;

    for seq in 1..=3 {
        probe
            .send(json!({
                "seq": seq,
                "action": "user_update_active_status",
                "data": {"user_is_active": false, "manual": true},
            }))
            .await;
    }
    // Manual aways always broadcast. The last request is a manual *online*, which clears the
    // manual flag again: `QueueSetStatusOffline` refuses to override a manual status, so without
    // it the disconnect below would leave the user away on both servers and never offline.
    probe
        .send(json!({
            "seq": 4,
            "action": "user_update_active_status",
            "data": {"user_is_active": true, "manual": true},
        }))
        .await;
    let aways_then_online = |frames: &[Value]| {
        let changes: Vec<&Value> = frames
            .iter()
            .filter(|f| f["event"] == "status_change")
            .collect();
        changes
            .iter()
            .filter(|f| f["data"]["status"] == "away")
            .count()
            >= 3
            && changes
                .last()
                .is_some_and(|f| f["data"]["status"] == "online")
    };
    assert!(
        probe
            .collect_until(Duration::from_secs(5), aways_then_online)
            .await,
        "{base}: the aways and the online did not arrive: {:?}",
        probe.raw
    );
    // Anything the connect itself set off lands before the snapshot.
    probe.collect_for(Duration::from_millis(300)).await;
    let history = events(&probe);
    (probe, connection_id, history)
}

/// Close the probe and wait until the server has unregistered the connection.
async fn drop_and_wait(http: &reqwest::Client, base: &str, probe: SocketProbe, user: &PlainUser) {
    probe.close().await;
    wait_for_status(http, base, &user.token, &user.id, "offline").await;
    tokio::time::sleep(Duration::from_millis(300)).await;
}

fn last_seq(history: &[(i64, String)]) -> i64 {
    history.iter().map(|(seq, _)| *seq).max().expect("events")
}

#[tokio::test]
async fn a_resume_inside_the_dead_queue_replays_the_missed_frames_byte_for_byte() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let user = &fixture(&http, &token).await.replay;

    let mut shapes = Vec::new();
    for base in [GO, RUST] {
        let (probe, connection_id, history) = a_connection_with_history(&http, base, user).await;
        let last = last_seq(&history);
        let from = history
            .iter()
            .find(|(_, raw)| parsed(raw)["data"]["status"] == "away")
            .map(|(seq, _)| *seq)
            .expect("an away");
        drop_and_wait(&http, base, probe, user).await;

        let mut resumed = SocketProbe::connect_raw_with_query(
            base,
            &user.token,
            &format!("connection_id={connection_id}&sequence_number={from}"),
        )
        .await;
        assert!(
            resumed
                .collect_until(Duration::from_secs(5), move |frames| frames.iter().any(
                    |f| f["event"] == "status_change" && f["seq"].as_i64() > Some(last)
                ))
                .await,
            "{base}: nothing past the replay arrived: {:?}",
            resumed.raw
        );

        // The opposite assertion needs a window: a wrongly queued `hello` would sit behind the
        // frames that waited while the client was away, so it arrives after them.
        resumed.collect_for(Duration::from_millis(700)).await;
        let got = events(&resumed);
        assert!(
            resumed.events_named("hello").is_empty(),
            "{base}: a resumed connection is not greeted: {:?}",
            resumed.raw
        );
        let replayed: Vec<(i64, String)> = got
            .iter()
            .take_while(|(seq, _)| *seq <= last)
            .cloned()
            .collect();
        let expected: Vec<(i64, String)> = history
            .iter()
            .filter(|(seq, _)| *seq >= from)
            .cloned()
            .collect();
        assert_eq!(
            replayed, expected,
            "{base}: the replay is the missed frames, byte for byte"
        );

        // Then the queue that waited while the client was away, numbered straight on from the
        // replay. A socket is not isolated — another suite's `new_user` can share that queue — so
        // the frame after the replay is checked for its number, and the offline this client's own
        // disconnect raised is looked for among the status changes.
        let next = &got[replayed.len()];
        assert_eq!(next.0, last + 1, "{base}");
        let offline = got[replayed.len()..]
            .iter()
            .find(|(_, raw)| parsed(raw)["event"] == "status_change")
            .expect("a status change after the replay");
        assert_eq!(parsed(&offline.1)["data"]["status"], "offline", "{base}");

        shapes.push(
            expected
                .iter()
                .filter(|(_, raw)| parsed(raw)["event"] == "status_change")
                .map(|(_, raw)| parsed(raw)["data"]["status"].clone())
                .collect::<Vec<_>>(),
        );
        resumed.close().await;
        wait_for_status(&http, base, &user.token, &user.id, "offline").await;
    }
    assert_eq!(
        shapes[0], shapes[1],
        "the servers replayed different frames"
    );
}

#[tokio::test]
async fn a_resume_right_after_the_newest_frame_says_nothing_and_delivers_what_waited() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let user = &fixture(&http, &token).await.lossless;

    let mut shapes = Vec::new();
    for base in [GO, RUST] {
        let (probe, connection_id, history) = a_connection_with_history(&http, base, user).await;
        let last = last_seq(&history);
        drop_and_wait(&http, base, probe, user).await;

        // Raised while nobody is connected: it waits in the parked queue.
        put_status(&http, base, &token, &user.id, "dnd").await;
        wait_for_status(&http, base, &user.token, &user.id, "dnd").await;
        tokio::time::sleep(Duration::from_millis(300)).await;

        let mut resumed = SocketProbe::connect_raw_with_query(
            base,
            &user.token,
            &format!("connection_id={connection_id}&sequence_number={}", last + 1),
        )
        .await;
        assert!(
            resumed
                .collect_until(Duration::from_secs(5), |frames| frames
                    .iter()
                    .any(|f| f["data"]["status"] == "dnd"))
                .await,
            "{base}: the status raised while away never arrived: {:?}",
            resumed.raw
        );

        resumed.collect_for(Duration::from_millis(700)).await;
        let got = events(&resumed);
        assert!(
            resumed.events_named("hello").is_empty(),
            "{base}: lossless means no hello: {:?}",
            resumed.raw
        );
        // Nothing replayed: the first frame is numbered straight on from the client's count.
        // Another suite's broadcasts can share the queue, so the two statuses are looked for among
        // the status changes, in order.
        assert_eq!(
            got.first().map(|(seq, _)| *seq),
            Some(last + 1),
            "{base}: {got:?}"
        );
        assert!(
            got.iter().all(|(seq, _)| *seq > last),
            "{base}: something was replayed: {got:?}"
        );
        let statuses: Vec<Value> = got
            .iter()
            .map(|(_, raw)| parsed(raw))
            .filter(|f| f["event"] == "status_change")
            .map(|f| f["data"]["status"].clone())
            .take(2)
            .collect();
        assert_eq!(statuses, [json!("offline"), json!("dnd")], "{base}");
        shapes.push(statuses);

        // Back to a non-manual status before leaving: a REST write is always manual, and a manual
        // status would block the next server's connect-time online.
        resumed
            .send(json!({
                "seq": 1,
                "action": "user_update_active_status",
                "data": {"user_is_active": true, "manual": true},
            }))
            .await;
        wait_for_status(&http, base, &user.token, &user.id, "online").await;
        resumed.close().await;
        wait_for_status(&http, base, &user.token, &user.id, "offline").await;
    }
    assert_eq!(shapes[0], shapes[1]);
}

#[tokio::test]
async fn a_resume_past_the_dead_queue_is_told_with_a_new_hello_under_a_new_id() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let user = &fixture(&http, &token).await.loss;

    for base in [GO, RUST] {
        let (probe, connection_id, history) = a_connection_with_history(&http, base, user).await;
        let last = last_seq(&history);
        drop_and_wait(&http, base, probe, user).await;

        let mut resumed = SocketProbe::connect_raw_with_query(
            base,
            &user.token,
            &format!(
                "connection_id={connection_id}&sequence_number={}",
                last + 20
            ),
        )
        .await;
        assert!(
            resumed
                .collect_until(Duration::from_secs(5), |frames| {
                    frames.iter().any(|f| f["event"] == "hello")
                        && frames.iter().any(|f| f["event"] == "status_change")
                })
                .await,
            "{base}: no hello and follow-up: {:?}",
            resumed.raw
        );

        let got = events(&resumed);
        let (hello_seq, hello_raw) = &got[0];
        let hello = parsed(hello_raw);
        assert_eq!(
            hello["event"], "hello",
            "{base}: hello comes first: {got:?}"
        );
        assert_eq!(*hello_seq, 0, "{base}: the counter restarts");
        assert!(
            hello_raw.ends_with('\n'),
            "{base}: hello is never precomputed"
        );
        let new_id = hello["data"]["connection_id"].as_str().expect("an id");
        assert_ne!(
            new_id, connection_id,
            "{base}: loss means a new connection id"
        );
        assert_eq!(new_id.len(), 26);
        assert_eq!(got[1].0, 1, "{base}: the waiting frames follow at 1");

        resumed.close().await;
        wait_for_status(&http, base, &user.token, &user.id, "offline").await;
    }
}

#[tokio::test]
async fn an_unknown_or_still_open_connection_id_is_a_fresh_connection() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let user = &fixture(&http, &token).await.unknown;

    for base in [GO, RUST] {
        let mut fresh = SocketProbe::connect_raw_with_query(
            base,
            &user.token,
            &format!("connection_id={NOBODY}&sequence_number=5"),
        )
        .await;
        assert!(
            fresh
                .collect_until(Duration::from_secs(3), |frames| frames
                    .iter()
                    .any(|f| f["event"] == "hello"))
                .await,
            "{base}: an unknown id is greeted as fresh: {:?}",
            fresh.raw
        );
        let hello = fresh.events_named("hello").remove(0);
        assert_eq!(hello["seq"], 0, "{base}: the sequence sent is ignored");
        let open_id = hello["data"]["connection_id"]
            .as_str()
            .expect("an id")
            .to_owned();
        assert_ne!(open_id, NOBODY, "{base}");

        // The id of a connection whose client never left is not resumable either.
        let mut second = SocketProbe::connect_raw_with_query(
            base,
            &user.token,
            &format!("connection_id={open_id}&sequence_number=1"),
        )
        .await;
        assert!(
            second
                .collect_until(Duration::from_secs(3), |frames| frames
                    .iter()
                    .any(|f| f["event"] == "hello"))
                .await,
            "{base}: {:?}",
            second.raw
        );
        let second_id = second.events_named("hello").remove(0)["data"]["connection_id"].clone();
        assert_ne!(second_id, json!(open_id), "{base}: not taken over");

        // And the first connection is untouched by the attempt.
        fresh.send(json!({"seq": 1, "action": "ping"})).await;
        assert!(
            fresh
                .collect_until(Duration::from_secs(3), |frames| frames
                    .iter()
                    .any(|f| f["seq_reply"] == 1))
                .await,
            "{base}: the open connection stopped answering"
        );

        fresh.close().await;
        second.close().await;
        wait_for_status(&http, base, &user.token, &user.id, "offline").await;
    }
}

#[tokio::test]
async fn a_malformed_resumption_opens_the_socket_and_closes_it() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let user = &fixture(&http, &token).await.malformed;

    let queries = [
        "connection_id=abc&sequence_number=1".to_owned(),
        format!("connection_id={NOBODY}"),
        format!("connection_id={NOBODY}&sequence_number="),
        format!("connection_id={NOBODY}&sequence_number=x"),
        format!("connection_id={NOBODY}&sequence_number=1.5"),
    ];
    for base in [GO, RUST] {
        for query in &queries {
            let mut probe = SocketProbe::connect_raw_with_query(base, &user.token, query).await;
            assert!(
                probe.closed_within(Duration::from_secs(3)).await,
                "{base}: `{query}` kept the socket open"
            );
            assert!(
                probe.events_named("hello").is_empty(),
                "{base}: `{query}` was greeted: {:?}",
                probe.raw
            );
        }

        // Without a user the resumption is never parsed: the socket stays up (until the
        // five-second authentication window, which this does not wait for).
        let mut anonymous =
            SocketProbe::connect_anonymous_with_query(base, "connection_id=abc&sequence_number=x")
                .await;
        assert!(
            !anonymous.closed_within(Duration::from_secs(2)).await,
            "{base}: an anonymous socket's resumption was parsed"
        );
    }
}
