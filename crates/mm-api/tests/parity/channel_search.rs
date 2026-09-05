//! Cross-server parity for `POST /api/v4/teams/{team_id}/channels/search` —
//! `searchChannelsForTeam`.
//!
//! ```sh
//! scripts/parity.sh -p mm-api --test parity channel_search
//! ```
//!
//! # Private channels are never results
//!
//! Both branches join `PublicChannels`, Go's denormalised shadow table, so even the "not a team
//! lister" branch is *the public channels you are in*, not *your channels*.
//! [`a_private_channel_is_never_a_result`].
//!
//! # The second branch is unreachable for any account the API can create
//!
//! `list_team_channels` is granted by **`team_user`**, which every team membership carries, so a
//! team member is always a lister and `SearchChannelsForUser` never runs. Stripping the roles
//! behind a live session does not reach it either: Go's session cache still holds the
//! `TeamMembers` it was built with, so Go stays on the first branch while a port reading the
//! `Sessions` row alone falls to the second — a divergence the REST API cannot produce, and one
//! that belongs to the session model rather than to this route. Left untested for that reason.

use crate::common;

use common::{
    GO, RUST, add_user_to_channel, assert_error_bodies_match_except_known_gaps, client,
    create_channel_typed, create_plain_user, create_team, go_minted_token, post_both_raw,
    purge_api_fixtures, stack_enabled,
};

struct Fixture {
    team_id: String,
    /// A team the plain user is **not** in.
    other_team_id: String,
    /// The unique stem every fixture channel's display name shares.
    stem: String,
    /// Open, plain user is a member.
    joined: String,
    /// Open, plain user is not a member.
    unjoined: String,
    /// Private, plain user **is** a member — and still not a result.
    private: String,
    /// Open and archived, which `includeDeleted = true` keeps.
    archived: String,
    /// Its **display name** carries a word its `Name` does not, so the display-name arm of the
    /// search clause is the only thing that can find it.
    display_only: String,
    plain_token: String,
}

static FIXTURE: tokio::sync::OnceCell<Fixture> = tokio::sync::OnceCell::const_new();

async fn fixture(client: &reqwest::Client, token: &str) -> &'static Fixture {
    FIXTURE
        .get_or_init(|| async {
            purge_api_fixtures().await;
            let team_id = create_team(client, token, "chansearch").await;
            let other_team_id = create_team(client, token, "chansearchb").await;

            // `create_channel_typed` names channels `mmrs-parity-<tag>`; the display name is the
            // tag, so the stem is what the search term matches on.
            let joined = create_channel_typed(client, token, &team_id, "csjoined", "O").await;
            let unjoined = create_channel_typed(client, token, &team_id, "csunjoined", "O").await;
            let private = create_channel_typed(client, token, &team_id, "csprivate", "P").await;
            let archived = create_channel_typed(client, token, &team_id, "csarchived", "O").await;

            let plain = create_plain_user(client, token, &team_id, "chansearch").await;
            add_user_to_channel(client, token, &joined, &plain.id).await;
            add_user_to_channel(client, token, &private, &plain.id).await;

            // `create_channel_typed` derives the display name from the name, so every term that
            // matches one matches the other and the display-name arm is dead. This one is
            // created by hand with a word that appears in neither the name nor the purpose.
            let display_only =
                create_channel_named(client, token, &team_id, "csdisplay", "mmrsparityzebra").await;

            archive_channel(client, token, &archived).await;

            Fixture {
                team_id,
                other_team_id,
                stem: "cs".to_owned(),
                joined,
                unjoined,
                private,
                archived,
                display_only,
                plain_token: plain.token,
            }
        })
        .await
}

/// `POST /channels` with a display name unrelated to the channel's name.
async fn create_channel_named(
    client: &reqwest::Client,
    token: &str,
    team_id: &str,
    tag: &str,
    display_name: &str,
) -> String {
    let response = client
        .post(format!("{GO}/api/v4/channels"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!({
            "team_id": team_id,
            "name": format!("mmrs-parity-{tag}"),
            "display_name": display_name,
            "type": "O",
        }))
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "creating the fixture channel failed: {}",
        response.text().await.unwrap_or_default()
    );
    let created: serde_json::Value = response.json().await.expect("the channel decodes");
    created["id"].as_str().expect("an id").to_owned()
}

async fn archive_channel(client: &reqwest::Client, token: &str, channel_id: &str) {
    let response = client
        .delete(format!("{GO}/api/v4/channels/{channel_id}"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "archiving {channel_id} failed: {}",
        response.text().await.unwrap_or_default()
    );
}

fn path(team_id: &str) -> String {
    format!("/api/v4/teams/{team_id}/channels/search")
}

fn body(term: &str) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({"term": term})).expect("JSON")
}

fn ids(raw: &[u8]) -> Vec<String> {
    serde_json::from_slice::<serde_json::Value>(raw)
        .expect("JSON")
        .as_array()
        .expect("an array")
        .iter()
        .map(|c| c["id"].as_str().unwrap_or_default().to_owned())
        .collect()
}

/// An admin holds `list_team_channels`, so they search every public channel in the team.
#[tokio::test]
async fn a_lister_searches_every_public_channel_and_the_body_matches() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let p = path(&f.team_id);
    let ((go_status, go), (rs_status, rs)) =
        post_both_raw(&client, &token, &p, &body(&f.stem)).await;
    assert_eq!(go_status, 200);
    assert_eq!(rs_status, go_status);
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{p} must be byte-identical"
    );
    assert_eq!(
        go.last(),
        Some(&b'\n'),
        "`json.NewEncoder(w).Encode` appends the newline"
    );

    let found = ids(&go);
    assert!(found.contains(&f.joined), "{found:?}");
    assert!(
        found.contains(&f.unjoined),
        "a channel the admin has not joined"
    );
    assert!(
        found.contains(&f.archived),
        "`includeDeleted` is a literal true, so an archived channel is a result: {found:?}"
    );
    assert!(
        !found.contains(&f.private),
        "the join is on PublicChannels: {found:?}"
    );
}

/// The display-name arm of the search clause, which the name arm cannot answer for.
#[tokio::test]
async fn a_term_only_the_display_name_carries_still_matches() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let p = path(&f.team_id);
    let ((go_status, go), (rs_status, rs)) =
        post_both_raw(&client, &token, &p, &body("zebra")).await;
    assert_eq!(go_status, 200);
    assert_eq!(rs_status, go_status);
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{p} must be byte-identical"
    );
    assert_eq!(
        ids(&go),
        vec![f.display_only.clone()],
        "`zebra` is in the display name and in neither the name nor the purpose"
    );
}

/// A private channel the caller is in is still not a result — **every** `system_user` holds
/// `list_team_channels`, so an ordinary member takes the *first* branch.
#[tokio::test]
async fn a_private_channel_is_never_a_result() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let p = path(&f.team_id);
    let ((go_status, go), (rs_status, rs)) =
        post_both_raw(&client, &f.plain_token, &p, &body(&f.stem)).await;
    assert_eq!(go_status, 200);
    assert_eq!(rs_status, go_status);
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{p} must be byte-identical"
    );

    let found = ids(&go);
    assert!(
        !found.contains(&f.private),
        "the plain user is a member of it and it is still absent: {found:?}"
    );
    assert!(found.contains(&f.joined), "{found:?}");
    assert!(
        found.contains(&f.unjoined),
        "and a plain user is a *lister*, so this is not scoped to their memberships: {found:?}"
    );
}

/// An empty term returns the whole (ordered, limited) list rather than nothing.
#[tokio::test]
async fn an_empty_term_omits_the_search_clause() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let p = path(&f.team_id);
    for term in ["", "   ", "*"] {
        let ((go_status, go), (rs_status, rs)) =
            post_both_raw(&client, &token, &p, &body(term)).await;
        assert_eq!(go_status, 200, "[{term}]");
        assert_eq!(rs_status, go_status, "[{term}]");
        assert_eq!(
            String::from_utf8_lossy(&go),
            String::from_utf8_lossy(&rs),
            "[{term}] must be byte-identical"
        );
        assert!(
            ids(&go).contains(&f.joined),
            "a term that sanitises to nothing lists the team: [{term}]"
        );
    }
}

/// A term matching nothing is `[]`, not `null`.
#[tokio::test]
async fn no_matches_is_an_empty_array() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let p = path(&f.team_id);
    let ((go_status, go), (rs_status, rs)) =
        post_both_raw(&client, &token, &p, &body("mmrsparitynosuchchannelterm")).await;
    assert_eq!(go_status, 200);
    assert_eq!(rs_status, go_status);
    assert_eq!(go, b"[]\n");
    assert_eq!(rs, go);
}

/// A caller who is not a team member gets `GetTeamMember`'s **404**, not a 403.
#[tokio::test]
async fn a_non_member_without_the_list_permission_gets_a_404() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let p = path(&f.other_team_id);
    let ((go_status, go_body), (rs_status, rs_body)) =
        post_both_raw(&client, &f.plain_token, &p, &body(&f.stem)).await;
    assert_eq!(
        go_status, 404,
        "`GetTeamMember` is called for its error, so this is not a permission refusal"
    );
    assert_eq!(rs_status, go_status, "{p}: statuses must match");
    assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &p);
}

/// The body must decode to an object; `null` is rejected by the nil check after it.
#[tokio::test]
async fn a_bad_body_is_a_400_on_both() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let p = path(&f.team_id);
    for raw in [&b"not json"[..], &b"[]"[..], &b"null"[..], &b""[..]] {
        let ((go_status, go_body), (rs_status, rs_body)) =
            post_both_raw(&client, &token, &p, raw).await;
        let shown = String::from_utf8_lossy(raw).to_string();
        assert_eq!(go_status, 400, "[{shown}] must be rejected by Go");
        assert_eq!(rs_status, go_status, "[{shown}]: statuses must match");
        let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &shown);
        assert_eq!(
            go["id"], "api.context.invalid_body_param.app_error",
            "for [{shown}]"
        );
    }

    // `{}` decodes to a non-nil struct with an empty term, which is the *listing* case above.
    let ((go_status, _), (rs_status, _)) = post_both_raw(&client, &token, &p, b"{}").await;
    assert_eq!(go_status, 200, "an object with no term is not a bad body");
    assert_eq!(rs_status, go_status);
}

/// A malformed team id is a 400 before the body is read.
#[tokio::test]
async fn a_bad_team_id_is_a_400_on_both() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let _ = fixture(&client, &token).await;

    let p = path("short");
    let ((go_status, go_body), (rs_status, rs_body)) =
        post_both_raw(&client, &token, &p, &body("x")).await;
    assert_eq!(go_status, 400);
    assert_eq!(rs_status, go_status, "{p}: statuses must match");
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &p);
    assert_eq!(go["id"], "api.context.invalid_url_param.app_error");
}

/// No session is a 401 on both.
#[tokio::test]
async fn no_session_is_a_401_on_both() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let p = path("zzzzzzzzzzzzzzzzzzzzzzzzzz");
    for base in [GO, RUST] {
        let response = client
            .post(format!("{base}{p}"))
            .body(body("x"))
            .send()
            .await
            .expect("reachable");
        assert_eq!(response.status().as_u16(), 401, "{base}{p}");
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
    let p = path("zzzzzzzzzzzzzzzzzzzzzzzzzz");
    for method in [reqwest::Method::GET, reqwest::Method::DELETE] {
        let rs = client
            .request(method.clone(), format!("{RUST}{p}"))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .expect("we answer");
        assert_eq!(
            rs.headers()
                .get("x-mmrs-served-by")
                .and_then(|v| v.to_str().ok()),
            Some("go"),
            "{method} {p} must be forwarded"
        );
    }
}
