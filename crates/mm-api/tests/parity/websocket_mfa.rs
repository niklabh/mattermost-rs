//! Cross-server parity for the MFA half of a websocket connection's authentication:
//! `IsAuthenticated` is `IsBasicAuthenticated && IsMFAAuthenticated` (web_conn.go:824), and the
//! second is `MFARequired`.
//!
//! ```sh
//! MMRS_LICENSED_VARIANT=mfa scripts/go-licensed.sh start
//! scripts/parity.sh --test parity websocket_mfa
//! ```
//!
//! Runs on the licensed **MFA** pair — MFA enabled and enforced. A user who has not set MFA up
//! keeps a registered socket and gets `hello`, because registration asks only the basic half, but
//! every action is refused and no event reaches it. REST is deliberately not exercised: this server
//! does not enforce MFA on REST routes yet (see `docs/TECH_DEBT.md`), and on the Go side of the
//! pair even the admin's REST calls would be refused.

use std::time::Duration;

use serde_json::{Value, json};

use crate::common;

use common::{
    BUSY_STATE, PlainUser, SocketProbe, add_user_to_channel, client, create_channel_typed,
    create_plain_user, create_team, go_minted_token, licensed_mfa, stack_enabled,
};

struct Fixture {
    channel_id: String,
    /// Has not set MFA up.
    owes: PlainUser,
    /// Enrolled; raises the typing.
    typist: PlainUser,
    /// Enrolled; proves the typing was raised.
    listener: PlainUser,
}

static FIXTURE: tokio::sync::OnceCell<Fixture> = tokio::sync::OnceCell::const_new();

async fn fixture(http: &reqwest::Client, admin: &str) -> &'static Fixture {
    FIXTURE
        .get_or_init(|| async {
            let team = create_team(http, admin, "wsm").await;
            let channel_id = create_channel_typed(http, admin, &team, "wsm", "O").await;
            let owes = create_plain_user(http, admin, &team, "wsmo").await;
            let typist = create_plain_user(http, admin, &team, "wsmt").await;
            let listener = create_plain_user(http, admin, &team, "wsml").await;
            for user in [&owes, &typist, &listener] {
                add_user_to_channel(http, admin, &channel_id, &user.id).await;
            }
            let pool = common::fixture_pool().await.expect("the fixture database");
            for user in [&typist, &listener] {
                sqlx::query(
                    "UPDATE users SET mfaactive = true, mfasecret = 'MMRSMFAFIXTURE' WHERE id = $1",
                )
                .bind(&user.id)
                .execute(&pool)
                .await
                .expect("the enrolment fixture is written");
            }
            Fixture {
                channel_id,
                owes,
                typist,
                listener,
            }
        })
        .await
}

async fn ask(probe: &mut SocketProbe, seq: i64, action: &str, data: Option<Value>) -> Value {
    let mut request = json!({"seq": seq, "action": action});
    if let Some(data) = data {
        request["data"] = data;
    }
    probe.send(request).await;
    assert!(
        probe
            .collect_until(Duration::from_secs(5), move |frames| frames
                .iter()
                .any(|f| f["seq_reply"] == seq))
            .await,
        "no answer to seq {seq}: {:?}",
        probe.raw
    );
    probe
        .frames()
        .into_iter()
        .find(|f| f["seq_reply"] == seq)
        .expect("found above")
}

#[tokio::test]
async fn a_user_who_owes_mfa_is_greeted_then_refused_and_hears_nothing() {
    if !stack_enabled() {
        return;
    }
    let _busy = BUSY_STATE.read().await;
    let http = client();
    let admin = go_minted_token(&http).await;
    let pair = licensed_mfa().await;
    let f = fixture(&http, &admin).await;

    let mut outcomes = Vec::new();
    for base in [pair.go.as_str(), pair.rust.as_str()] {
        // `connect` asserts exactly one `hello`: registration asks only the basic half.
        let mut owes = SocketProbe::connect(base, &f.owes.token).await;
        let mut listener = SocketProbe::connect(base, &f.listener.token).await;
        let mut typist = SocketProbe::connect(base, &f.typist.token).await;

        let refused = ask(&mut owes, 1, "ping", None).await;
        let answered = ask(&mut typist, 1, "ping", None).await;

        let typed = ask(
            &mut typist,
            2,
            "user_typing",
            Some(json!({"channel_id": f.channel_id})),
        )
        .await;
        assert_eq!(typed["status"], "OK", "{base}: {typed}");
        let channel_id = f.channel_id.clone();
        assert!(
            listener
                .collect_until(Duration::from_secs(3), move |frames| frames.iter().any(
                    |fr| { fr["event"] == "typing" && fr["broadcast"]["channel_id"] == channel_id }
                ))
                .await,
            "{base}: the enrolled listener heard no typing: {:?}",
            listener.raw
        );
        owes.collect_for(Duration::from_millis(800)).await;
        let owes_heard = owes
            .events_named("typing")
            .iter()
            .any(|fr| fr["broadcast"]["channel_id"] == f.channel_id);

        outcomes.push((base.to_owned(), refused, answered, owes_heard));
    }

    let (go_base, go_refused, go_answered, go_heard) = &outcomes[0];
    let (_, rust_refused, rust_answered, rust_heard) = &outcomes[1];

    assert_eq!(
        go_refused["error"]["id"], "api.web_socket_router.not_authenticated.app_error",
        "{go_base}: {go_refused}"
    );
    assert_eq!(go_answered["status"], "OK", "{go_base}: {go_answered}");
    assert!(
        !go_heard,
        "{go_base}: an unauthenticated socket heard typing"
    );

    for field in ["id", "status_code", "detailed_error"] {
        assert_eq!(
            go_refused["error"][field], rust_refused["error"][field],
            "the refusal's {field}\n go: {go_refused}\nrust: {rust_refused}"
        );
    }
    assert_eq!(go_refused["status"], rust_refused["status"]);
    // Not the whole answer: `server_time` is each server's own clock.
    for field in ["status", "seq_reply"] {
        assert_eq!(
            go_answered[field], rust_answered[field],
            "the enrolled ping's {field}"
        );
    }
    assert_eq!(go_answered["data"]["text"], rust_answered["data"]["text"]);
    assert_eq!(
        go_heard, rust_heard,
        "whether the socket that owes MFA heard typing"
    );
}
