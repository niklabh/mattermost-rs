//! Cross-server parity for `POST /api/v4/users/group_channels`
//! (`getUsersByGroupChannelIds`) — the member profiles behind a group message's avatar row.
//!
//! ```sh
//! scripts/parity.sh -p mm-api --test parity users_group_channels
//! ```
//!
//! # Two answers a reader would not predict
//!
//! **An empty list is a parse error.** Go's `if err != nil || len == 0` catches it, and the
//! `else if len == 0 { SetInvalidParam("channel_ids") }` beneath is unreachable — so `[]` and
//! `null` answer `api.payload.parse.error`, the opposite of every other by-ids route.
//! [`an_empty_list_is_a_parse_error_not_an_invalid_param`].
//!
//! **The access check is a subquery.** There is no permission gate in the handler or the app
//! layer; the store's `EXISTS` asserts the caller is a member of each channel it answers for, so
//! a channel you are not in is *absent from the map* rather than a 403.
//! [`a_group_channel_the_caller_is_not_in_is_absent`].

use crate::common;

use common::{
    GO, RUST, assert_error_bodies_match_except_known_gaps, client, create_channel_typed,
    create_plain_user, create_team, go_minted_token, logged_in_user_id, post_both_raw,
    purge_api_fixtures, stack_enabled,
};

struct Fixture {
    /// A group message with the admin and both plain users in it.
    shared_gm: String,
    /// A group message between the two plain users and a third — the admin is **not** in it.
    foreign_gm: String,
    /// The usernames of the two plain users, in ascending order.
    member_usernames: Vec<String>,
    /// An ordinary channel the admin is a member of, and a direct message they are in. Neither
    /// is a group channel, and this route must answer for neither — without them the `Type = 'G'`
    /// predicate is dead code, because every id the suite passes is already a group channel.
    open_channel: String,
    direct_channel: String,
    plain_token: String,
}

static FIXTURE: tokio::sync::OnceCell<Fixture> = tokio::sync::OnceCell::const_new();

async fn fixture(client: &reqwest::Client, token: &str) -> &'static Fixture {
    FIXTURE
        .get_or_init(|| async {
            purge_api_fixtures().await;

            let admin_id = logged_in_user_id();
            let team_id = create_team(client, token, "ugcteam").await;

            let a = create_plain_user(client, token, &team_id, "ugca").await;
            let b = create_plain_user(client, token, &team_id, "ugcb").await;
            let c = create_plain_user(client, token, &team_id, "ugcc").await;

            let shared_gm = open_group_channel(client, token, &[admin_id, &a.id, &b.id]).await;
            // Opened by a plain user, and the admin is not among the three.
            let foreign_gm = open_group_channel(client, &a.token, &[&a.id, &b.id, &c.id]).await;

            // Not group channels. The admin is a member of both.
            let open_channel = create_channel_typed(client, token, &team_id, "ugcopen", "O").await;
            let direct_channel = open_direct_channel(client, token, admin_id, &a.id).await;

            let mut member_usernames = vec![
                format!("mmrsplain{}", "ugca"),
                format!("mmrsplain{}", "ugcb"),
            ];
            member_usernames.sort();

            Fixture {
                shared_gm,
                foreign_gm,
                member_usernames,
                open_channel,
                direct_channel,
                plain_token: a.token,
            }
        })
        .await
}

async fn open_direct_channel(client: &reqwest::Client, token: &str, a: &str, b: &str) -> String {
    let response = client
        .post(format!("{GO}/api/v4/channels/direct"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!([a, b]))
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "opening the DM failed: {}",
        response.text().await.unwrap_or_default()
    );
    let created: serde_json::Value = response.json().await.expect("the channel decodes");
    created["id"].as_str().expect("an id").to_owned()
}

async fn open_group_channel(client: &reqwest::Client, token: &str, ids: &[&str]) -> String {
    let response = client
        .post(format!("{GO}/api/v4/channels/group"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!(ids))
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "opening the group channel failed: {}",
        response.text().await.unwrap_or_default()
    );
    let created: serde_json::Value = response.json().await.expect("the channel decodes");
    created["id"].as_str().expect("an id").to_owned()
}

const PATH: &str = "/api/v4/users/group_channels";

fn body(ids: &[&str]) -> Vec<u8> {
    serde_json::to_vec(&ids).expect("the id list serialises")
}

fn usernames_in(raw: &[u8], channel_id: &str) -> Vec<String> {
    let parsed: serde_json::Value = serde_json::from_slice(raw).expect("the body is JSON");
    parsed[channel_id]
        .as_array()
        .map(|users| {
            users
                .iter()
                .map(|u| u["username"].as_str().expect("a username").to_owned())
                .collect()
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------------------------

/// The whole route in one assertion: the caller's own group message, its other members in
/// username order, and the caller themselves absent.
#[tokio::test]
async fn a_group_channels_members_are_byte_identical() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;
    let admin_id = logged_in_user_id();

    let ids = [f.shared_gm.as_str()];
    let ((go_status, go_body), (rs_status, rs_body)) =
        post_both_raw(&client, &token, PATH, &body(&ids)).await;

    assert_eq!(go_status, 200, "{}", String::from_utf8_lossy(&go_body));
    assert_eq!(rs_status, go_status, "{PATH}: statuses must match");
    assert_eq!(
        String::from_utf8_lossy(&go_body),
        String::from_utf8_lossy(&rs_body),
        "{PATH} must be byte-identical"
    );
    assert_eq!(
        go_body.last(),
        Some(&b'\n'),
        "json.NewEncoder(w).Encode appends the newline json.Marshal does not"
    );

    let listed = usernames_in(&go_body, &f.shared_gm);
    assert_eq!(
        listed, f.member_usernames,
        "the other two members, in `ORDER BY Users.Username ASC`"
    );

    // `Users.Id <> ?`: the caller is never in their own avatar row.
    let parsed: serde_json::Value = serde_json::from_slice(&go_body).expect("JSON");
    assert!(
        !parsed[&f.shared_gm]
            .as_array()
            .expect("an array")
            .iter()
            .any(|u| u["id"] == admin_id),
        "the caller is excluded from their own group's list"
    );
}

/// The `EXISTS` subquery *is* the access check. A group channel the caller is not in comes back
/// absent — not a 403, and not an empty array either.
#[tokio::test]
async fn a_group_channel_the_caller_is_not_in_is_absent() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let ids = [f.shared_gm.as_str(), f.foreign_gm.as_str()];
    let ((go_status, go_body), (rs_status, rs_body)) =
        post_both_raw(&client, &token, PATH, &body(&ids)).await;

    assert_eq!(
        go_status, 200,
        "not a refusal — the row simply does not join"
    );
    assert_eq!(rs_status, go_status);
    assert_eq!(go_body, rs_body, "{PATH} must be byte-identical");

    let parsed: serde_json::Value = serde_json::from_slice(&go_body).expect("JSON");
    assert!(
        parsed.get(&f.shared_gm).is_some(),
        "the channel the admin is in is present"
    );
    assert!(
        parsed.get(&f.foreign_gm).is_none(),
        "and the one they are not in is absent, not empty: {parsed}"
    );

    // A member of that same channel does see it, so the absence above is the subquery and not a
    // channel that does not exist.
    let ((go_status, go_body), (_rs, rs_body)) =
        post_both_raw(&client, &f.plain_token, PATH, &body(&ids)).await;
    assert_eq!(go_status, 200);
    assert_eq!(go_body, rs_body, "{PATH} must be byte-identical");
    let parsed: serde_json::Value = serde_json::from_slice(&go_body).expect("JSON");
    assert!(
        parsed.get(&f.foreign_gm).is_some(),
        "its own member sees it: {parsed}"
    );
}

/// The dead branch. Go means to answer `invalid_body_param` for an empty list and can never
/// reach the line that would.
#[tokio::test]
async fn an_empty_list_is_a_parse_error_not_an_invalid_param() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;

    for raw in [&b"[]"[..], &b"null"[..], &b"{}"[..], &b"not json"[..]] {
        let ((go_status, go_body), (rs_status, rs_body)) =
            post_both_raw(&client, &token, PATH, raw).await;
        assert_eq!(
            go_status,
            400,
            "`{}` is refused",
            String::from_utf8_lossy(raw)
        );
        assert_eq!(rs_status, go_status, "{PATH}: statuses must match");
        let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, PATH);
        assert_eq!(
            go["id"],
            "api.payload.parse.error",
            "`{}`: never `api.context.invalid_body_param.app_error`",
            String::from_utf8_lossy(raw)
        );
    }
}

/// `MaxGroupChannelsForProfiles` **truncates** the list rather than refusing it — and it does so
/// after `SortedArrayFromJSON` has sorted, so it is the fifty lowest-sorting ids that survive.
///
/// Fifty ids that sort below the real one therefore push it out of the query entirely, and the
/// answer is an empty object with no error and nothing saying why.
#[tokio::test]
async fn the_fifty_channel_cap_truncates_the_sorted_list_silently() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    // `0` sorts below every character an id can start with, so these all precede the real id.
    let filler: Vec<String> = (0..50).map(|i| format!("0{i:025}")).collect();
    let mut ids: Vec<&str> = filler.iter().map(String::as_str).collect();
    ids.push(f.shared_gm.as_str());

    let ((go_status, go_body), (rs_status, rs_body)) =
        post_both_raw(&client, &token, PATH, &body(&ids)).await;
    assert_eq!(go_status, 200, "the cap is not a refusal");
    assert_eq!(rs_status, go_status);
    assert_eq!(go_body, rs_body, "{PATH} must be byte-identical");
    assert_eq!(
        String::from_utf8_lossy(&go_body).trim_end(),
        "{}",
        "the real channel was sorted into position 51 and dropped"
    );

    // One fewer filler and it survives, so the assertion above is the cap and not the fillers
    // breaking the query.
    let ids: Vec<&str> = filler[..49]
        .iter()
        .map(String::as_str)
        .chain(std::iter::once(f.shared_gm.as_str()))
        .collect();
    let ((go_status, go_body), (_rs, rs_body)) =
        post_both_raw(&client, &token, PATH, &body(&ids)).await;
    assert_eq!(go_status, 200);
    assert_eq!(go_body, rs_body, "{PATH} must be byte-identical");
    assert!(
        !usernames_in(&go_body, &f.shared_gm).is_empty(),
        "at fifty ids exactly, the real one is still inside the cap"
    );
}

/// Sanitisation, and the `asAdmin` bit that decides how much of it applies.
///
/// **Not the email.** This deployment has `ShowEmailAddress` on, so both callers see it — an
/// assertion that a plain caller does not was written here first and failed against *both*
/// servers, which is the fixture being wrong rather than the port. What `asAdmin` actually
/// changes on the wire is narrower and sharper: an admin's copy carries `notify_props` and a
/// plain caller's carries `auth_data` instead.
#[tokio::test]
async fn the_admin_bit_changes_which_fields_survive_sanitisation() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let ids = [f.shared_gm.as_str()];

    let ((go_admin_status, go_admin), (_rs, rs_admin)) =
        post_both_raw(&client, &token, PATH, &body(&ids)).await;
    assert_eq!(go_admin_status, 200);
    assert_eq!(
        go_admin, rs_admin,
        "{PATH} must be byte-identical for the admin"
    );

    let ((go_plain_status, go_plain), (_rs, rs_plain)) =
        post_both_raw(&client, &f.plain_token, PATH, &body(&ids)).await;
    assert_eq!(go_plain_status, 200);
    assert_eq!(
        go_plain, rs_plain,
        "{PATH} must be byte-identical for a plain caller"
    );

    let keys = |raw: &[u8], channel: &str| -> Vec<String> {
        let parsed: serde_json::Value = serde_json::from_slice(raw).expect("JSON");
        let user = parsed[channel]
            .as_array()
            .expect("an array")
            .first()
            .expect("at least one member")
            .clone();
        let mut k: Vec<String> = user
            .as_object()
            .expect("an object")
            .keys()
            .cloned()
            .collect();
        k.sort();
        k
    };

    let admin_keys = keys(&go_admin, &f.shared_gm);
    let plain_keys = keys(&go_plain, &f.shared_gm);

    assert!(
        admin_keys.contains(&"notify_props".to_owned()),
        "the admin's copy keeps notify_props: {admin_keys:?}"
    );
    assert!(
        !plain_keys.contains(&"notify_props".to_owned()),
        "a plain caller's does not: {plain_keys:?}"
    );
    assert!(
        plain_keys.contains(&"auth_data".to_owned())
            && !admin_keys.contains(&"auth_data".to_owned()),
        "and the pair swaps the other way for auth_data: admin {admin_keys:?}, plain {plain_keys:?}"
    );

    // Neither caller ever sees a credential, whatever the config says about email.
    for (who, k) in [("admin", &admin_keys), ("plain", &plain_keys)] {
        for secret in [
            "password",
            "mfa_secret",
            "last_password_update",
            "failed_attempts",
        ] {
            assert!(
                !k.contains(&secret.to_owned()),
                "{who} must never see {secret}: {k:?}"
            );
        }
    }
}

/// An unknown channel id is simply absent, like a channel the caller is not in.
#[tokio::test]
async fn an_unknown_channel_id_is_absent() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;

    let ids = ["aaaaaaaaaaaaaaaaaaaaaaaaaa"];
    let ((go_status, go_body), (rs_status, rs_body)) =
        post_both_raw(&client, &token, PATH, &body(&ids)).await;
    assert_eq!(go_status, 200);
    assert_eq!(rs_status, go_status);
    assert_eq!(
        String::from_utf8_lossy(&go_body).trim_end(),
        "{}",
        "an empty map, not a key with an empty list"
    );
    assert_eq!(go_body, rs_body, "{PATH} must be byte-identical");
}

/// Everything but `POST` on this path stays Go's.
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

/// An unauthenticated request never reaches the handler.
#[tokio::test]
async fn no_session_is_a_401_on_both() {
    if !stack_enabled() {
        return;
    }
    let client = client();

    let send = async |base: &str| {
        client
            .post(format!("{base}{PATH}"))
            .header("Content-Type", "application/json")
            .body(&b"[\"aaaaaaaaaaaaaaaaaaaaaaaaaa\"]"[..])
            .send()
            .await
            .expect("the server answers")
    };

    let go = send(GO).await;
    let rs = send(RUST).await;
    assert_eq!(go.status(), 401);
    assert_eq!(rs.status(), go.status(), "{PATH}: statuses must match");
    let go_body = go.bytes().await.expect("body").to_vec();
    let rs_body = rs.bytes().await.expect("body").to_vec();
    assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, PATH);
}

/// `c.Type = 'G'` — the route is group messages only. An ordinary channel and a direct message
/// the caller *is* a member of both come back absent.
///
/// Without this the type predicate is dead code: every other test passes group-channel ids, so
/// widening it to "any message channel" changed nothing and the mutation survived.
#[tokio::test]
async fn only_group_channels_are_answered_for() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let ids = [
        f.shared_gm.as_str(),
        f.open_channel.as_str(),
        f.direct_channel.as_str(),
    ];
    let ((go_status, go_body), (rs_status, rs_body)) =
        post_both_raw(&client, &token, PATH, &body(&ids)).await;

    assert_eq!(go_status, 200);
    assert_eq!(rs_status, go_status);
    assert_eq!(go_body, rs_body, "{PATH} must be byte-identical");

    let parsed: serde_json::Value = serde_json::from_slice(&go_body).expect("JSON");
    assert!(
        parsed.get(&f.shared_gm).is_some(),
        "the group channel answers: {parsed}"
    );
    assert!(
        parsed.get(&f.open_channel).is_none(),
        "an ordinary channel the caller is in does not: {parsed}"
    );
    assert!(
        parsed.get(&f.direct_channel).is_none(),
        "and neither does a direct message: {parsed}"
    );

    // The membership is real — the caller is in both — so the absences above are the type
    // predicate rather than the access subquery.
    if let Ok(url) = std::env::var("DATABASE_URL")
        && let Ok(pool) = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(std::time::Duration::from_secs(5))
            .connect(&url)
            .await
    {
        let rows: (i64,) = sqlx::query_as(
            "SELECT count(*) FROM channelmembers WHERE userid = $1 AND channelid = ANY($2)",
        )
        .bind(logged_in_user_id())
        .bind(vec![f.open_channel.clone(), f.direct_channel.clone()])
        .fetch_one(&pool)
        .await
        .expect("the count runs");
        assert_eq!(
            rows.0, 2,
            "the caller is a member of both non-group channels"
        );
    }
}
