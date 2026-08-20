//! Cross-server parity for three `api4/team.go` routes:
//!
//! - `GET /api/v4/teams/name/{team_name}` (`getTeamByName`)
//! - `GET /api/v4/teams/{team_id}/members/{user_id}` (`getTeamMember`)
//! - `GET /api/v4/teams/{team_id}/members` (`getTeamMembers`)
//!
//! Every cell that has a different actor or a different shape is driven with Go as the oracle:
//! admin / plain member / non-member, public / non-public / archived / missing team, the
//! `SanitizeRoleData` split (`manage_team_roles` keeps roles; anyone else sees other rows blanked
//! with `delete_at: -1` and their own row intact), the three sorts, `exclude_deleted_users`,
//! `per_page=0`, the mux-charset forward and the three literals Go's `{team_id}` subrouter
//! shadows under `/teams/name/`.
//!
//! ```sh
//! scripts/parity.sh -p mm-api --test parity_team_name_members
//! ```

mod common;

use common::{
    GO, RUST, assert_error_bodies_match_except_known_gaps, client, create_plain_user,
    delete_plain_user, fetch_both_raw, fetch_both_stable, go_minted_token, purge_api_fixtures,
    stack_enabled,
};

/// Create a team through Go's API — type `O`, `AllowOpenInvite = false` (Go's default), so it
/// is **not** public until [`make_team_public`]. Returns `(id, name)`.
async fn create_team(client: &reqwest::Client, token: &str, tag: &str) -> (String, String) {
    let name = format!("mmrs-parity-{tag}");
    let response = client
        .post(format!("{GO}/api/v4/teams"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!({
            "name": name,
            "display_name": format!("mmrs parity {tag}"),
            "type": "O",
        }))
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "creating the fixture team failed: {}",
        response.text().await.unwrap_or_default()
    );
    let created: serde_json::Value = response.json().await.expect("the team decodes");
    (created["id"].as_str().expect("an id").to_owned(), name)
}

async fn make_team_public(client: &reqwest::Client, token: &str, team_id: &str) {
    let response = client
        .put(format!("{GO}/api/v4/teams/{team_id}/patch"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!({ "allow_open_invite": true }))
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "patching the team public failed: {}",
        response.text().await.unwrap_or_default()
    );
}

async fn archive_team(client: &reqwest::Client, token: &str, team_id: &str) {
    let response = client
        .delete(format!("{GO}/api/v4/teams/{team_id}"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "archiving the team failed: {}",
        response.text().await.unwrap_or_default()
    );
}

/// Soft-delete a membership — `DELETE /teams/{id}/members/{uid}` sets `TeamMembers.DeleteAt`.
async fn remove_from_team(client: &reqwest::Client, token: &str, team_id: &str, user_id: &str) {
    let response = client
        .delete(format!("{GO}/api/v4/teams/{team_id}/members/{user_id}"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "removing the member failed: {}",
        response.text().await.unwrap_or_default()
    );
}

/// Soft-delete a user — `DELETE /users/{id}` sets `Users.DeleteAt`, leaving the membership.
async fn deactivate_user(client: &reqwest::Client, admin_token: &str, user_id: &str) {
    let response = client
        .delete(format!("{GO}/api/v4/users/{user_id}"))
        .header("Authorization", format!("Bearer {admin_token}"))
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "deactivating {user_id} failed: {}",
        response.text().await.unwrap_or_default()
    );
}

fn assert_bytes_equal(go_body: &[u8], rs_body: &[u8], context: &str) {
    assert_eq!(
        String::from_utf8_lossy(rs_body),
        String::from_utf8_lossy(go_body),
        "{context}: the two servers must agree byte for byte"
    );
}

/// Both servers answer identically, and the Rust side **forwarded** — the proof that a router
/// decision was left to Go rather than reproduced.
async fn assert_forwarded_and_identical(client: &reqwest::Client, token: &str, path: &str) {
    let response = client
        .get(format!("{RUST}{path}"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("the Rust server answers");
    assert_eq!(
        response
            .headers()
            .get("x-mmrs-served-by")
            .and_then(|v| v.to_str().ok()),
        Some("go"),
        "{path}: Go's router owns this answer, so it must be forwarded"
    );
    let status = response.status().as_u16();
    let body = response.bytes().await.expect("body reads").to_vec();

    let direct = client
        .get(format!("{GO}{path}"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("Go answers");
    assert_eq!(direct.status().as_u16(), status, "{path}");
    let direct_body = direct.bytes().await.expect("body reads").to_vec();
    // Both answers are Go's, so even `message` agrees; only the per-request id may differ (and
    // Go's mux 404 carries none at all, so the bodies are then equal whole).
    let mut go: serde_json::Value =
        serde_json::from_slice(&direct_body).expect("Go's body is JSON");
    let mut rs: serde_json::Value =
        serde_json::from_slice(&body).expect("the forwarded body is JSON");
    for value in [&mut go, &mut rs] {
        if let Some(object) = value.as_object_mut() {
            object.remove("request_id");
        }
    }
    assert_eq!(go, rs, "{path}");
}

// ---------------------------------------------------------------------------------------------
// getTeamByName
// ---------------------------------------------------------------------------------------------

/// Admin by name: unsanitised, byte-identical, encoder newline — and the same bytes the by-id
/// route serves, since both go through `teamSliceColumns(true)`.
#[tokio::test]
async fn an_admin_reads_a_team_by_name_byte_identically() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let (team_id, name) = create_team(&client, &token, "tbnadmin").await;

    let path = format!("/api/v4/teams/name/{name}");
    let (go_body, rs_body) = fetch_both_stable(&client, &token, &path).await;
    assert_bytes_equal(&go_body, &rs_body, &path);
    assert_eq!(
        rs_body.last(),
        Some(&b'\n'),
        "the encoder's newline ([D-086])"
    );

    let parsed: serde_json::Value = serde_json::from_slice(&rs_body).expect("decodes");
    assert_eq!(parsed["id"].as_str(), Some(team_id.as_str()));
    assert_ne!(parsed["email"].as_str(), Some(""), "the admin keeps email");
    assert_ne!(parsed["invite_id"].as_str(), Some(""));

    let (_, by_id) = fetch_both_stable(&client, &token, &format!("/api/v4/teams/{team_id}")).await;
    assert_bytes_equal(&by_id, &rs_body, "by-name versus by-id");
}

/// A plain member lands in the sanitiser's mixed cell — `invite_id` kept, `email` stripped.
#[tokio::test]
async fn a_plain_member_by_name_keeps_invite_id_and_loses_email() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let (team_id, name) = create_team(&client, &token, "tbnmember").await;
    let member = create_plain_user(&client, &token, &team_id, "tbnmember").await;

    let path = format!("/api/v4/teams/name/{name}");
    let (go_body, rs_body) = fetch_both_stable(&client, &member.token, &path).await;
    delete_plain_user(&client, &token, &member.id).await;

    assert_bytes_equal(&go_body, &rs_body, &path);
    let parsed: serde_json::Value = serde_json::from_slice(&rs_body).expect("decodes");
    assert_eq!(parsed["email"].as_str(), Some(""));
    assert_ne!(parsed["invite_id"].as_str(), Some(""));
}

/// A non-member against a non-public team is refused with `view_team`; flip the team public and
/// the same caller is admitted **without** `list_public_teams` ever being consulted — the
/// by-name gate has no fallback, the public conjunct alone admits. Fully sanitised either way.
#[tokio::test]
async fn a_non_member_is_refused_until_the_team_is_public() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let (team_id, name) = create_team(&client, &token, "tbnoutsider").await;
    let (home_team, _) = create_team(&client, &token, "tbnoutsiderhome").await;
    let outsider = create_plain_user(&client, &token, &home_team, "tbnoutsider").await;

    let path = format!("/api/v4/teams/name/{name}");
    let ((go_status, go_body), (rs_status, rs_body)) =
        fetch_both_raw(&client, &outsider.token, &path).await;
    assert_eq!((go_status, rs_status), (403, 403));
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "non-public");
    assert_eq!(go["id"], "api.context.permissions.app_error");

    make_team_public(&client, &token, &team_id).await;
    let (go_body, rs_body) = fetch_both_stable(&client, &outsider.token, &path).await;
    delete_plain_user(&client, &token, &outsider.id).await;
    assert_bytes_equal(&go_body, &rs_body, "public");
    let parsed: serde_json::Value = serde_json::from_slice(&rs_body).expect("decodes");
    assert_eq!(parsed["email"].as_str(), Some(""));
    assert_eq!(
        parsed["invite_id"].as_str(),
        Some(""),
        "a non-member keeps neither"
    );
    assert_eq!(parsed["allow_open_invite"], true);
}

/// `GetByName` has no `DeleteAt` filter: an archived team still answers by name.
#[tokio::test]
async fn an_archived_team_still_answers_by_name() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let (team_id, name) = create_team(&client, &token, "tbnarchived").await;
    archive_team(&client, &token, &team_id).await;

    let path = format!("/api/v4/teams/name/{name}");
    let (go_body, rs_body) = fetch_both_stable(&client, &token, &path).await;
    assert_bytes_equal(&go_body, &rs_body, &path);
    let parsed: serde_json::Value = serde_json::from_slice(&rs_body).expect("decodes");
    assert_ne!(parsed["delete_at"].as_i64(), Some(0));
}

/// A valid name that matches nothing is the `missing.` 404; names the validator rejects
/// (uppercase, underscore, one character) are 400 `invalid_url_param`.
#[tokio::test]
async fn a_missing_name_is_404_and_an_invalid_one_is_400() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;
    let client = client();
    let token = go_minted_token(&client).await;

    let ((go_status, go_body), (rs_status, rs_body)) =
        fetch_both_raw(&client, &token, "/api/v4/teams/name/mmrs-parity-nosuchteam").await;
    assert_eq!((go_status, rs_status), (404, 404));
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "missing");
    assert_eq!(go["id"], "app.team.get_by_name.missing.app_error");

    for bad in ["Up_per", "a", "has_underscore", "-leading"] {
        let path = format!("/api/v4/teams/name/{bad}");
        let ((go_status, go_body), (rs_status, rs_body)) =
            fetch_both_raw(&client, &token, &path).await;
        assert_eq!((go_status, rs_status), (400, 400), "{path}");
        let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &path);
        assert_eq!(
            go["id"], "api.context.invalid_url_param.app_error",
            "{path}"
        );
    }
}

/// Two router decisions left to Go: a segment outside `[A-Za-z0-9_-]+` (the mux 404) and the
/// three literals the `{team_id}` subrouter shadows (a 400 naming `team_id`, from `getTeamStats`,
/// `getTeamMembers` and `getTeamImage` respectively). A literal that only a PUT/POST route
/// claims — `patch` — is a method mismatch mux skips, so it is a team name and a 404.
#[tokio::test]
async fn the_mux_charset_and_the_shadowed_literals_are_gos_answers() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;
    let client = client();
    let token = go_minted_token(&client).await;

    for path in [
        "/api/v4/teams/name/a.b",
        "/api/v4/teams/name/a%20b",
        "/api/v4/teams/name/stats",
        "/api/v4/teams/name/members",
        "/api/v4/teams/name/image",
    ] {
        assert_forwarded_and_identical(&client, &token, path).await;
    }

    let ((go_status, go_body), (rs_status, rs_body)) =
        fetch_both_raw(&client, &token, "/api/v4/teams/name/patch").await;
    assert_eq!((go_status, rs_status), (404, 404));
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "patch");
    assert_eq!(go["id"], "app.team.get_by_name.missing.app_error");
}

// ---------------------------------------------------------------------------------------------
// getTeamMember
// ---------------------------------------------------------------------------------------------

/// The `SanitizeRoleData` split, both directions: the admin (holding `manage_team_roles`) sees
/// the plain member's roles; the plain member sees the admin's row blanked with `delete_at: -1`
/// and its own row (via `me` and via its id) intact. All four byte-identical, newline included.
#[tokio::test]
async fn a_single_member_is_sanitised_by_manage_team_roles() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let (team_id, _) = create_team(&client, &token, "tmsplit").await;
    let member = create_plain_user(&client, &token, &team_id, "tmsplit").await;
    let admin_id = common::logged_in_user_id();

    // Admin → member: roles intact.
    let path = format!("/api/v4/teams/{team_id}/members/{}", member.id);
    let (go_body, rs_body) = fetch_both_stable(&client, &token, &path).await;
    assert_bytes_equal(&go_body, &rs_body, &path);
    assert_eq!(
        rs_body.last(),
        Some(&b'\n'),
        "the encoder's newline ([D-086])"
    );
    let parsed: serde_json::Value = serde_json::from_slice(&rs_body).expect("decodes");
    assert_eq!(parsed["roles"], "team_user");
    assert_eq!(parsed["scheme_user"], true);
    assert_eq!(parsed["delete_at"], 0);

    // Member → admin: blanked, -1 sentinel.
    let path = format!("/api/v4/teams/{team_id}/members/{admin_id}");
    let (go_body, rs_body) = fetch_both_stable(&client, &member.token, &path).await;
    assert_bytes_equal(&go_body, &rs_body, &path);
    let parsed: serde_json::Value = serde_json::from_slice(&rs_body).expect("decodes");
    assert_eq!(parsed["roles"], "");
    assert_eq!(parsed["scheme_admin"], false);
    assert_eq!(parsed["delete_at"], -1, "Go's sentinel reaches the wire");

    // Member → self, by alias and by id: intact.
    for target in ["me", member.id.as_str()] {
        let path = format!("/api/v4/teams/{team_id}/members/{target}");
        let (go_body, rs_body) = fetch_both_stable(&client, &member.token, &path).await;
        assert_bytes_equal(&go_body, &rs_body, &path);
        let parsed: serde_json::Value = serde_json::from_slice(&rs_body).expect("decodes");
        assert_eq!(parsed["roles"], "team_user", "{target}");
        assert_eq!(
            parsed["user_id"].as_str(),
            Some(member.id.as_str()),
            "{target}"
        );
    }

    delete_plain_user(&client, &token, &member.id).await;
}

/// A departed member's row still answers singly (no `DeleteAt` filter in `GetMember`), carrying
/// its non-zero `delete_at` — while the list route below drops it.
#[tokio::test]
async fn a_departed_member_still_answers_singly_with_its_delete_at() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let (team_id, _) = create_team(&client, &token, "tmdeparted").await;
    let member = create_plain_user(&client, &token, &team_id, "tmdeparted").await;
    remove_from_team(&client, &token, &team_id, &member.id).await;

    let path = format!("/api/v4/teams/{team_id}/members/{}", member.id);
    let (go_body, rs_body) = fetch_both_stable(&client, &token, &path).await;
    assert_bytes_equal(&go_body, &rs_body, &path);
    let parsed: serde_json::Value = serde_json::from_slice(&rs_body).expect("decodes");
    assert_ne!(
        parsed["delete_at"].as_i64(),
        Some(0),
        "the departure is on the wire"
    );

    let list_path = format!("/api/v4/teams/{team_id}/members");
    let (go_list, rs_list) = fetch_both_stable(&client, &token, &list_path).await;
    assert_bytes_equal(&go_list, &rs_list, &list_path);
    let list: Vec<serde_json::Value> = serde_json::from_slice(&rs_list).expect("decodes");
    assert!(
        list.iter().all(|m| m["user_id"] != member.id.as_str()),
        "the list filters DeleteAt = 0"
    );

    delete_plain_user(&client, &token, &member.id).await;
}

/// Refusals and misses on the single-member route: a non-member is 403 before anything is
/// fetched; a valid id that is not a member is the `missing.` 404; a well-formed team id that
/// matches nothing is a **404 for the admin** (system roles pass the gate, nothing fetches the
/// team) and a **403 for a plain user**; malformed ids are 400.
#[tokio::test]
async fn single_member_refusals_and_misses_match() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let (team_id, _) = create_team(&client, &token, "tmrefusals").await;
    let (home_team, _) = create_team(&client, &token, "tmrefusalshome").await;
    let outsider = create_plain_user(&client, &token, &home_team, "tmrefusals").await;
    let admin_id = common::logged_in_user_id();
    const NOBODY: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaa";
    const NO_TEAM: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbb";

    let cases: [(&str, String, u16, &str); 6] = [
        (
            "non-member",
            format!("/api/v4/teams/{team_id}/members/{admin_id}"),
            403,
            "api.context.permissions.app_error",
        ),
        (
            "not a member",
            format!("/api/v4/teams/{team_id}/members/{NOBODY}"),
            404,
            "app.team.get_member.missing.app_error",
        ),
        (
            "no such team, admin",
            format!("/api/v4/teams/{NO_TEAM}/members/{admin_id}"),
            404,
            "app.team.get_member.missing.app_error",
        ),
        (
            "no such team, plain",
            format!("/api/v4/teams/{NO_TEAM}/members/me"),
            403,
            "api.context.permissions.app_error",
        ),
        (
            "bad team id",
            format!("/api/v4/teams/notanid/members/{admin_id}"),
            400,
            "api.context.invalid_url_param.app_error",
        ),
        (
            "bad user id",
            format!("/api/v4/teams/{team_id}/members/notanid"),
            400,
            "api.context.invalid_url_param.app_error",
        ),
    ];
    for (label, path, status, id) in &cases {
        let actor = match *label {
            "non-member" | "no such team, plain" => outsider.token.as_str(),
            _ => token.as_str(),
        };
        let ((go_status, go_body), (rs_status, rs_body)) =
            fetch_both_raw(&client, actor, path).await;
        assert_eq!(
            (go_status, rs_status),
            (*status, *status),
            "{label}: {path}"
        );
        let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, label);
        assert_eq!(go["id"], *id, "{label}");
    }

    delete_plain_user(&client, &token, &outsider.id).await;
}

// ---------------------------------------------------------------------------------------------
// getTeamMembers
// ---------------------------------------------------------------------------------------------

/// The list with three members in default order: the admin sees every row whole; a plain
/// member sees the two other rows blanked with `delete_at: -1` and its own row intact, in the
/// middle of the list. No trailing newline — `json.Marshal`, not the encoder.
#[tokio::test]
async fn the_member_list_is_sanitised_per_row_by_manage_team_roles() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let (team_id, _) = create_team(&client, &token, "tmslist").await;
    let first = create_plain_user(&client, &token, &team_id, "tmslista").await;
    let second = create_plain_user(&client, &token, &team_id, "tmslistb").await;

    let path = format!("/api/v4/teams/{team_id}/members");
    let (go_body, rs_body) = fetch_both_stable(&client, &token, &path).await;
    assert_bytes_equal(&go_body, &rs_body, "admin");
    assert_ne!(
        rs_body.last(),
        Some(&b'\n'),
        "json.Marshal, no newline ([D-086])"
    );
    let list: Vec<serde_json::Value> = serde_json::from_slice(&rs_body).expect("decodes");
    assert_eq!(list.len(), 3);
    assert!(list.iter().all(|m| m["delete_at"] == 0 && m["roles"] != ""));
    let mut ids: Vec<&str> = list.iter().filter_map(|m| m["user_id"].as_str()).collect();
    let unsorted = ids.clone();
    ids.sort_unstable();
    assert_eq!(ids, unsorted, "the default sort is by UserId");

    let (go_body, rs_body) = fetch_both_stable(&client, &first.token, &path).await;
    assert_bytes_equal(&go_body, &rs_body, "plain member");
    let list: Vec<serde_json::Value> = serde_json::from_slice(&rs_body).expect("decodes");
    assert_eq!(list.len(), 3);
    for m in &list {
        if m["user_id"] == first.id.as_str() {
            assert_eq!(m["roles"], "team_user", "own row intact");
            assert_eq!(m["delete_at"], 0);
        } else {
            assert_eq!(m["roles"], "", "other rows blanked");
            assert_eq!(m["delete_at"], -1, "Go's sentinel");
            assert_eq!(m["scheme_user"], false);
        }
    }

    delete_plain_user(&client, &token, &first.id).await;
    delete_plain_user(&client, &token, &second.id).await;
}

/// The three sorts and the deactivated-user filter. Ids are random, so id order and username
/// order disagree by chance most runs but not all; the Username ordering is therefore asserted
/// against the usernames themselves, and the unordered `sort=bogus` answer is compared as a set.
#[tokio::test]
async fn sorting_and_exclude_deleted_users_match() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let (team_id, _) = create_team(&client, &token, "tmssort").await;
    let zulu = create_plain_user(&client, &token, &team_id, "tmssortzulu").await;
    let alpha = create_plain_user(&client, &token, &team_id, "tmssortalpha").await;
    let gone = create_plain_user(&client, &token, &team_id, "tmssortgone").await;
    deactivate_user(&client, &token, &gone.id).await;

    let base = format!("/api/v4/teams/{team_id}/members");

    // sort=Username: the four rows in username order, whatever the admin's username is.
    let path = format!("{base}?sort=Username");
    let (go_body, rs_body) = fetch_both_stable(&client, &token, &path).await;
    assert_bytes_equal(&go_body, &rs_body, &path);
    let list: Vec<serde_json::Value> = serde_json::from_slice(&rs_body).expect("decodes");
    let order: Vec<&str> = list.iter().filter_map(|m| m["user_id"].as_str()).collect();
    assert_eq!(
        order.len(),
        4,
        "the deactivated user's membership is still listed"
    );
    let admin_id = common::logged_in_user_id();
    let mut by_username: Vec<(String, &str)> = vec![
        (
            common::username_of(&client, &token, admin_id).await,
            admin_id,
        ),
        ("mmrsplaintmssortzulu".to_owned(), zulu.id.as_str()),
        ("mmrsplaintmssortalpha".to_owned(), alpha.id.as_str()),
        ("mmrsplaintmssortgone".to_owned(), gone.id.as_str()),
    ];
    by_username.sort();
    let expected: Vec<&str> = by_username.iter().map(|(_, id)| *id).collect();
    assert_eq!(order, expected, "ORDER BY Username");

    // exclude_deleted_users=1 drops the deactivated user's live membership.
    let path = format!("{base}?exclude_deleted_users=1");
    let (go_body, rs_body) = fetch_both_stable(&client, &token, &path).await;
    assert_bytes_equal(&go_body, &rs_body, &path);
    let list: Vec<serde_json::Value> = serde_json::from_slice(&rs_body).expect("decodes");
    assert_eq!(list.len(), 3);
    assert!(list.iter().all(|m| m["user_id"] != gone.id.as_str()));

    // Both, plus the `true` spelling; `yes` is a ParseBool error and therefore false.
    let path = format!("{base}?sort=Username&exclude_deleted_users=true");
    let (go_body, rs_body) = fetch_both_stable(&client, &token, &path).await;
    assert_bytes_equal(&go_body, &rs_body, &path);
    let list: Vec<serde_json::Value> = serde_json::from_slice(&rs_body).expect("decodes");
    assert_eq!(list.len(), 3);
    assert_eq!(list[0]["user_id"], alpha.id.as_str());
    let path = format!("{base}?exclude_deleted_users=yes");
    let (_, rs_body) = fetch_both_stable(&client, &token, &path).await;
    let list: Vec<serde_json::Value> = serde_json::from_slice(&rs_body).expect("decodes");
    assert_eq!(list.len(), 4, "`yes` is not a Go bool");

    // sort=bogus orders by nothing: same set, order not promised by either server.
    let path = format!("{base}?sort=bogus");
    let (go_body, rs_body) = fetch_both_stable(&client, &token, &path).await;
    let mut go_list: Vec<serde_json::Value> = serde_json::from_slice(&go_body).expect("decodes");
    let mut rs_list: Vec<serde_json::Value> = serde_json::from_slice(&rs_body).expect("decodes");
    let key = |m: &serde_json::Value| m["user_id"].as_str().unwrap_or_default().to_owned();
    go_list.sort_by_key(key);
    rs_list.sort_by_key(key);
    assert_eq!(go_list, rs_list);
    assert_eq!(rs_list.len(), 4);

    for user in [&zulu, &alpha, &gone] {
        delete_plain_user(&client, &token, &user.id).await;
    }
}

/// Pagination: `per_page=0` is an **empty list** on this route (the channel twin serves the
/// whole channel), `page=1&per_page=1` is the second row by id, garbage falls to defaults.
#[tokio::test]
async fn pagination_matches_including_the_empty_per_page_zero() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let (team_id, _) = create_team(&client, &token, "tmspage").await;
    let a = create_plain_user(&client, &token, &team_id, "tmspagea").await;
    let b = create_plain_user(&client, &token, &team_id, "tmspageb").await;

    let base = format!("/api/v4/teams/{team_id}/members");
    let (go_all, rs_all) = fetch_both_stable(&client, &token, &base).await;
    assert_bytes_equal(&go_all, &rs_all, &base);
    let all: Vec<serde_json::Value> = serde_json::from_slice(&rs_all).expect("decodes");
    assert_eq!(all.len(), 3);

    let path = format!("{base}?per_page=0");
    let (go_body, rs_body) = fetch_both_stable(&client, &token, &path).await;
    assert_bytes_equal(&go_body, &rs_body, &path);
    assert_eq!(
        rs_body, b"[]",
        "LIMIT 0 — the opposite of getChannelMembers"
    );

    let path = format!("{base}?page=1&per_page=1");
    let (go_body, rs_body) = fetch_both_stable(&client, &token, &path).await;
    assert_bytes_equal(&go_body, &rs_body, &path);
    let page: Vec<serde_json::Value> = serde_json::from_slice(&rs_body).expect("decodes");
    assert_eq!(page.len(), 1);
    assert_eq!(page[0], all[1], "offset 1, limit 1 is the second row");

    let path = format!("{base}?page=-3&per_page=abc");
    let (go_body, rs_body) = fetch_both_stable(&client, &token, &path).await;
    assert_bytes_equal(&go_body, &rs_body, &path);
    assert_eq!(rs_body, rs_all, "garbage falls to page 0, per_page 60");

    let path = format!("{base}?page=9");
    let (go_body, rs_body) = fetch_both_stable(&client, &token, &path).await;
    assert_bytes_equal(&go_body, &rs_body, &path);
    assert_eq!(rs_body, b"[]");

    delete_plain_user(&client, &token, &a.id).await;
    delete_plain_user(&client, &token, &b.id).await;
}

/// The list's refusals: a non-member is 403; a well-formed team id matching nothing is `[]`
/// for the admin and 403 for a plain user (nothing fetches the team — `getTeamStats`'s split).
#[tokio::test]
async fn list_refusals_and_the_missing_team_split_match() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let (team_id, _) = create_team(&client, &token, "tmsrefusals").await;
    let (home_team, _) = create_team(&client, &token, "tmsrefusalshome").await;
    let outsider = create_plain_user(&client, &token, &home_team, "tmsrefusals").await;
    const NO_TEAM: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbb";

    let path = format!("/api/v4/teams/{team_id}/members");
    let ((go_status, go_body), (rs_status, rs_body)) =
        fetch_both_raw(&client, &outsider.token, &path).await;
    assert_eq!((go_status, rs_status), (403, 403));
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "non-member");
    assert_eq!(go["id"], "api.context.permissions.app_error");

    let path = format!("/api/v4/teams/{NO_TEAM}/members");
    let (go_body, rs_body) = fetch_both_stable(&client, &token, &path).await;
    assert_bytes_equal(&go_body, &rs_body, "missing team, admin");
    assert_eq!(rs_body, b"[]");
    let ((go_status, _), (rs_status, _)) = fetch_both_raw(&client, &outsider.token, &path).await;
    assert_eq!((go_status, rs_status), (403, 403), "missing team, plain");

    let ((go_status, go_body), (rs_status, rs_body)) =
        fetch_both_raw(&client, &token, "/api/v4/teams/notanid/members").await;
    assert_eq!((go_status, rs_status), (400, 400));
    assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "bad id");

    delete_plain_user(&client, &token, &outsider.id).await;
}
