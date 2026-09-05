//! Cross-server parity for `POST /api/v4/channels/stats/member_count` —
//! `getChannelsMemberCount`.
//!
//! ```sh
//! scripts/parity.sh -p mm-api --test parity channels_member_count
//! ```
//!
//! # A partially-resolvable list is Go's, not ours
//!
//! `GetChannels` reads through an in-memory cache and asks the database only for the misses, so
//! a list of one known and one unknown id is a 404 when the known channel is cached and a 200
//! when it is not. This port has no such cache; it forwards.
//! [`a_list_that_only_partly_resolves_is_forwarded`].

use crate::common;

use common::{
    GO, RUST, add_user_to_channel, assert_error_bodies_match_except_known_gaps, client,
    create_channel_typed, create_plain_user, create_team, go_minted_token, logged_in_user_id,
    post_both_raw, purge_api_fixtures, stack_enabled,
};

const PATH: &str = "/api/v4/channels/stats/member_count";
const UNKNOWN: &str = "zzzzzzzzzzzzzzzzzzzzzzzzzz";

struct Fixture {
    /// Open, and the plain user is in it.
    open: String,
    /// Open, and the plain user is **not** in it — reachable through `list_team_channels`.
    open_unjoined: String,
    /// Private, and the plain user is not in it — the refusal.
    private: String,
    /// Open, and **nobody** is in it: its creator left. The only way to reach a requested id
    /// the count query returns no row for, which is what the seeded `0` defaults exist for.
    deserted: String,
    plain_token: String,
}

static FIXTURE: tokio::sync::OnceCell<Fixture> = tokio::sync::OnceCell::const_new();

async fn fixture(client: &reqwest::Client, token: &str) -> &'static Fixture {
    FIXTURE
        .get_or_init(|| async {
            purge_api_fixtures().await;
            let team_id = create_team(client, token, "memcount").await;
            let open = create_channel_typed(client, token, &team_id, "memcount", "O").await;
            let open_unjoined =
                create_channel_typed(client, token, &team_id, "memcountopen", "O").await;
            let private = create_channel_typed(client, token, &team_id, "memcountpriv", "P").await;

            let plain = create_plain_user(client, token, &team_id, "memcount").await;
            add_user_to_channel(client, token, &open, &plain.id).await;

            // A deactivated member of `open`, which the count's `Users.DeleteAt = 0` join must
            // exclude — without it the join is dead weight and dropping it changes nothing.
            let doomed = create_plain_user(client, token, &team_id, "memcountdead").await;
            add_user_to_channel(client, token, &open, &doomed.id).await;
            common::delete_plain_user(client, token, &doomed.id).await;

            // The admin creates every channel and is therefore a member of it; leaving is the
            // only way to get a live channel with no members at all.
            let deserted = create_channel_typed(client, token, &team_id, "memcountgone", "O").await;
            leave_channel(client, token, &deserted, logged_in_user_id()).await;

            Fixture {
                open,
                open_unjoined,
                private,
                deserted,
                plain_token: plain.token,
            }
        })
        .await
}

/// `DELETE /channels/{channel_id}/members/{user_id}` — leaving a channel deletes the row
/// outright, unlike a team membership.
async fn leave_channel(client: &reqwest::Client, token: &str, channel_id: &str, user_id: &str) {
    let response = client
        .delete(format!(
            "{GO}/api/v4/channels/{channel_id}/members/{user_id}"
        ))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "leaving {channel_id} failed: {}",
        response.text().await.unwrap_or_default()
    );
}

fn body(ids: &[&str]) -> Vec<u8> {
    serde_json::to_vec(&ids).expect("a JSON array")
}

/// The counts are byte-identical, and the deactivated member is not among them.
#[tokio::test]
async fn the_counts_are_byte_identical_and_exclude_deactivated_members() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let ((go_status, go), (rs_status, rs)) = post_both_raw(
        &client,
        &token,
        PATH,
        &body(&[&f.open, &f.open_unjoined, &f.deserted]),
    )
    .await;
    assert_eq!(go_status, 200);
    assert_eq!(rs_status, go_status);
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{PATH} must be byte-identical"
    );
    assert_eq!(
        go.last(),
        Some(&b'\n'),
        "`json.NewEncoder(w).Encode` appends the newline"
    );

    let parsed: serde_json::Value = serde_json::from_slice(&go).expect("JSON");
    // The admin who created it, plus the plain user — the deactivated third member is excluded
    // by the `Users.DeleteAt = 0` join.
    assert_eq!(parsed[&f.open], 2, "{parsed}");
    assert_eq!(parsed[&f.open_unjoined], 1, "only its creator: {parsed}");
    // **The seeded default.** The query returns no row for a channel with no members, so this
    // key exists only because `GetChannelsMemberCount` starts from a map of zeros.
    assert_eq!(
        parsed[&f.deserted], 0,
        "a channel nobody is in is `0`, not an absent key: {parsed}"
    );

    // `encoding/json` sorts map keys, so the object is in id order whichever order was asked.
    let keys: Vec<&str> = parsed
        .as_object()
        .expect("an object")
        .keys()
        .map(String::as_str)
        .collect();
    let mut sorted = keys.clone();
    sorted.sort_unstable();
    assert_eq!(keys, sorted, "keys are bytewise sorted: {parsed}");
}

/// An empty list is `{}` — the cache layer returns before the query that would 404.
#[tokio::test]
async fn an_empty_list_is_an_empty_object() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let _ = fixture(&client, &token).await;

    for raw in [&b"[]"[..], &b"null"[..]] {
        let ((go_status, go), (rs_status, rs)) = post_both_raw(&client, &token, PATH, raw).await;
        let shown = String::from_utf8_lossy(raw).to_string();
        assert_eq!(go_status, 200, "[{shown}] is not the sqlstore's 404");
        assert_eq!(rs_status, go_status, "[{shown}]");
        assert_eq!(go, b"{}\n", "[{shown}]");
        assert_eq!(rs, go, "[{shown}]");
    }
}

/// A list where **nothing** resolves is a 404 on both — deterministic, because nothing can be
/// cached.
#[tokio::test]
async fn a_list_that_resolves_to_nothing_is_a_404() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let _ = fixture(&client, &token).await;

    let ((go_status, go_body), (rs_status, rs_body)) =
        post_both_raw(&client, &token, PATH, &body(&[UNKNOWN])).await;
    assert_eq!(go_status, 404);
    assert_eq!(rs_status, go_status, "{PATH}: statuses must match");
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, PATH);
    assert_eq!(go["id"], "app.channel.get.existing.app_error");
}

/// A list that only **partly** resolves is handed to Go, whose answer depends on its cache.
#[tokio::test]
async fn a_list_that_only_partly_resolves_is_forwarded() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let rs = client
        .post(format!("{RUST}{PATH}"))
        .header("Authorization", format!("Bearer {token}"))
        .body(body(&[&f.open, UNKNOWN]))
        .send()
        .await
        .expect("we answer");
    assert_eq!(
        rs.headers()
            .get("x-mmrs-served-by")
            .and_then(|v| v.to_str().ok()),
        Some("go"),
        "a partially-resolvable list must be forwarded, not guessed at"
    );
}

/// A private channel the caller is not in refuses the whole request, naming `list_team_channels`.
#[tokio::test]
async fn one_refusal_refuses_the_whole_request() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    // The plain user can count `open` (a member) and `open_unjoined` (list_team_channels), but
    // not `private` — and the one refusal takes the readable ones down with it.
    let ((go_status, go), (rs_status, rs)) = post_both_raw(
        &client,
        &f.plain_token,
        PATH,
        &body(&[&f.open, &f.open_unjoined]),
    )
    .await;
    assert_eq!(go_status, 200, "both of these are readable");
    assert_eq!(rs_status, go_status);
    assert_eq!(String::from_utf8_lossy(&go), String::from_utf8_lossy(&rs));

    let ((go_status, go_body), (rs_status, rs_body)) =
        post_both_raw(&client, &f.plain_token, PATH, &body(&[&f.open, &f.private])).await;
    assert_eq!(go_status, 403, "one unreadable channel refuses the list");
    assert_eq!(rs_status, go_status, "{PATH}: statuses must match");
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, PATH);
    assert_eq!(go["id"], "api.context.permissions.app_error");
}

/// A body that is not a JSON array of strings is the one honest 400.
#[tokio::test]
async fn a_bad_body_is_a_400_on_both() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let _ = fixture(&client, &token).await;

    for raw in [&b"not json"[..], &b"{\"a\":1}"[..], &b"[1,2]"[..]] {
        let ((go_status, go_body), (rs_status, rs_body)) =
            post_both_raw(&client, &token, PATH, raw).await;
        let shown = String::from_utf8_lossy(raw).to_string();
        assert_eq!(go_status, 400, "[{shown}] must be rejected by Go");
        assert_eq!(rs_status, go_status, "[{shown}]: statuses must match");
        let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &shown);
        assert_eq!(go["id"], "api.payload.parse.error", "for [{shown}]");
    }
}

/// No session is a 401 on both.
#[tokio::test]
async fn no_session_is_a_401_on_both() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    for base in [GO, RUST] {
        let response = client
            .post(format!("{base}{PATH}"))
            .body(body(&[UNKNOWN]))
            .send()
            .await
            .expect("reachable");
        assert_eq!(response.status().as_u16(), 401, "{base}{PATH}");
    }
}

/// Every other method on this path is Go's.
#[tokio::test]
async fn other_methods_are_forwarded() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    for method in [reqwest::Method::GET, reqwest::Method::DELETE] {
        let rs = client
            .request(method.clone(), format!("{RUST}{PATH}"))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .expect("we answer");
        assert_eq!(
            rs.headers()
                .get("x-mmrs-served-by")
                .and_then(|v| v.to_str().ok()),
            Some("go"),
            "{method} {PATH} must be forwarded"
        );
    }
}
