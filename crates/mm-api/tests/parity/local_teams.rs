//! Cross-server parity for the thirty local-mode pairs of `team_local.go`, `webhook_local.go`
//! and `command_local.go` — the same routes over two unix sockets.
//!
//! ```sh
//! scripts/parity.sh --test parity local_teams
//! ```
//!
//! See `local_mode.rs` for the transport and the authentication model. What is specific here:
//!
//! - **Every write lands in the shared database**, so each server writes its own rows — a team,
//!   a hook, a command created through Go's socket is compared, normalised, against the one
//!   created through ours — and every name carries a prefix `purge_api_fixtures` collects on
//!   (`mmrs-parity-` for teams, `mmrs` for hook display names) or is deleted by the test itself
//!   (commands, by team).
//! - **The six `local*` handlers are different functions from their HTTP namesakes**, and the
//!   tests are shaped around the differences: the invite id a socket create *keeps*, the
//!   `user_id`/`creator_id` a socket hook create *requires*, the permanent delete the socket
//!   *performs* with `EnableAPITeamDeletion` off.
//! - **The empty local user id** is what makes `POST /teams/{id}/members/ids` answer `[]` for a
//!   real member and `DELETE .../members/me` a 400, on both servers.

use crate::common;
use crate::common::local_socket::{
    assert_forwarded_body_is_gos, both, both_maybe_forwarded, go_socket, rust_socket,
    sockets_enabled,
};

/// One request over one socket, with an optional JSON body, for any method.
async fn send(
    socket: &std::path::Path,
    method: &str,
    path: &str,
    body: Option<&str>,
) -> (u16, axum::http::HeaderMap, Vec<u8>) {
    let mut request = axum::http::Request::builder()
        .method(method)
        .uri(path)
        .header("Host", "localhost");
    let body = match body {
        Some(body) => {
            request = request
                .header("Content-Type", "application/json")
                .header("Content-Length", body.len().to_string());
            axum::body::Body::from(body.to_owned())
        }
        None => axum::body::Body::empty(),
    };
    let response = mm_api::local::send_over_unix(socket, request.body(body).expect("builds"))
        .await
        .unwrap_or_else(|e| panic!("{method} {path} over {}: {e}", socket.display()));
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body reads")
        .to_vec();
    (status, headers, bytes)
}

fn served_here(headers: &axum::http::HeaderMap) -> bool {
    headers
        .get("x-mmrs-served-by")
        .and_then(|v| v.to_str().ok())
        == Some("rust")
}

/// The same request, with a body, to both sockets; asserts ours answered it.
async fn both_json(method: &str, path: &str, body: &str) -> ((u16, Vec<u8>), (u16, Vec<u8>)) {
    let (go_status, _, go_body) =
        send(&go_socket().expect("checked"), method, path, Some(body)).await;
    let (rs_status, rs_headers, rs_body) =
        send(&rust_socket().expect("checked"), method, path, Some(body)).await;
    assert!(
        served_here(&rs_headers),
        "{method} {path} was forwarded to Go over the socket, so this comparison proves nothing \
         about the Rust handler"
    );
    ((go_status, go_body), (rs_status, rs_body))
}

/// The same request, with a body, to both sockets, for a case that is *expected* to be forwarded.
async fn both_json_maybe_forwarded(
    method: &str,
    path: &str,
    body: &str,
) -> ((u16, Vec<u8>), (u16, Vec<u8>), bool) {
    let (go_status, _, go_body) =
        send(&go_socket().expect("checked"), method, path, Some(body)).await;
    let (rs_status, rs_headers, rs_body) =
        send(&rust_socket().expect("checked"), method, path, Some(body)).await;
    (
        (go_status, go_body),
        (rs_status, rs_body),
        served_here(&rs_headers),
    )
}

fn parse(raw: &[u8]) -> serde_json::Value {
    serde_json::from_slice(raw)
        .unwrap_or_else(|e| panic!("not JSON: {:?} ({e})", String::from_utf8_lossy(raw)))
}

/// Everything that is per-row or per-server about a JSON object, reduced to "present" (ids and
/// names) or "non-zero" (timestamps), so two rows created independently can be compared field
/// by field.
fn normalise(value: &serde_json::Value, id_keys: &[&str], time_keys: &[&str]) -> serde_json::Value {
    let mut out = value.clone();
    let Some(object) = out.as_object_mut() else {
        return out;
    };
    for key in id_keys {
        if let Some(v) = object.get(*key) {
            let present = v.as_str().is_some_and(|s| !s.is_empty());
            object.insert((*key).to_owned(), serde_json::json!(present));
        }
    }
    for key in time_keys {
        if let Some(v) = object.get(*key) {
            object.insert((*key).to_owned(), serde_json::json!(v.as_i64() > Some(0)));
        }
    }
    out
}

fn normalise_team(value: &serde_json::Value) -> serde_json::Value {
    normalise(
        value,
        &["id", "invite_id", "name", "display_name"],
        &["create_at", "update_at", "delete_at"],
    )
}

fn normalise_hook(value: &serde_json::Value) -> serde_json::Value {
    normalise(
        value,
        &["id", "token", "display_name", "trigger_words"],
        &["create_at", "update_at"],
    )
}

fn normalise_command(value: &serde_json::Value) -> serde_json::Value {
    normalise(
        value,
        &["id", "token", "team_id", "trigger"],
        &["create_at", "update_at"],
    )
}

/// The commands this file created, by team, plus any whose trigger carries this file's prefix.
async fn sweep_commands(teams: &[&str]) {
    let Some(pool) = common::fixture_pool().await else {
        return;
    };
    sqlx::query(r#"DELETE FROM commands WHERE "trigger" LIKE 'mmrsloc%' OR teamid = ANY($1)"#)
        .bind(teams)
        .execute(&pool)
        .await
        .expect("the commands are removed");
}

/// The same write on each server's own team; asserts ours served it and both agree on status.
#[allow(clippy::too_many_arguments)]
async fn write_both(
    go: &std::path::Path,
    rust: &std::path::Path,
    go_team: &str,
    rs_team: &str,
    method: &str,
    suffix: &str,
    body: &str,
    expect: u16,
    label: &str,
) -> (Vec<u8>, Vec<u8>) {
    let (go_status, _, go_body) = send(
        go,
        method,
        &format!("/api/v4/teams/{go_team}{suffix}"),
        Some(body),
    )
    .await;
    let (rs_status, rs_headers, rs_body) = send(
        rust,
        method,
        &format!("/api/v4/teams/{rs_team}{suffix}"),
        Some(body),
    )
    .await;
    assert!(served_here(&rs_headers), "{label} must be served here");
    assert_eq!(
        go_status,
        expect,
        "{label}: Go: {}",
        String::from_utf8_lossy(&go_body)
    );
    assert_eq!(
        rs_status,
        go_status,
        "{label}: {}",
        String::from_utf8_lossy(&rs_body)
    );
    (go_body, rs_body)
}

/// **The five team reads, and the one whose local answer is not the HTTP one.**
///
/// `GET /teams`, `GET /teams/{id}`, `GET /teams/name/{name}` (both casings) and the search are
/// the HTTP handlers under the local session, so a team the admin created through the port is
/// compared byte for byte — `email` and `invite_id` included, since nothing is sanitised for an
/// unrestricted session. `POST /teams/{id}/members/ids` asks about a real member and gets `[]`
/// from both, because the local session's view restrictions are those of user `""`. And the
/// three names the HTTP router forwards as `{team_id}` shadows are ordinary 404s here.
#[tokio::test]
async fn the_team_reads_match_over_the_socket() {
    if !sockets_enabled() {
        return;
    }
    let client = common::client();
    let token = common::go_minted_token(&client).await;
    let team = common::create_team(&client, &token, "loctread").await;
    let admin = common::logged_in_user_id();
    common::invalidate_go_caches(&client, &token).await;

    // The full list is racy — sibling tests create and delete teams while this one runs — so
    // the comparison is of this team's entry, plus the claim that both lists carry it. The page
    // is the maximum, 200: Go's default is 60, and a full run holds more teams than that at
    // once, so this team fell onto page two and "Go lists the fixture team" failed (2026-09-15).
    let ((go_status, go_body), (rs_status, rs_body)) =
        both("GET", "/api/v4/teams?per_page=200").await;
    assert_eq!((go_status, rs_status), (200, 200), "GET /teams");
    let mine = |body: &[u8]| -> Option<serde_json::Value> {
        parse(body)
            .as_array()
            .expect("an array")
            .iter()
            .find(|t| t["id"] == team)
            .cloned()
    };
    let go_entry = mine(&go_body).expect("Go lists the fixture team");
    assert_eq!(
        mine(&rs_body),
        Some(go_entry.clone()),
        "GET /teams: the fixture team's entry"
    );
    assert_eq!(
        go_entry["email"].as_str().map(|s| s.is_empty()),
        Some(false),
        "an unrestricted session is not sanitised"
    );

    let path = format!("/api/v4/teams/{team}");
    let ((go_status, go_body), (rs_status, rs_body)) = both("GET", &path).await;
    assert_eq!((go_status, rs_status), (200, 200), "{path}");
    assert_eq!(go_body, rs_body, "{path}: byte for byte, newline included");

    for name in ["mmrs-parity-loctread", "MMRS-PARITY-LOCTREAD"] {
        let path = format!("/api/v4/teams/name/{name}");
        let ((go_status, go_body), (rs_status, rs_body)) = both("GET", &path).await;
        assert_eq!((go_status, rs_status), (200, 200), "{path}");
        assert_eq!(go_body, rs_body, "{path}");
        assert_eq!(parse(&go_body)["id"], team, "{path} finds the fixture team");
    }

    // Shadowed on the HTTP router, plain names on this one: measured 404s on Go's socket.
    for name in ["stats", "image", "members"] {
        let path = format!("/api/v4/teams/name/{name}");
        let ((go_status, go_body), (rs_status, rs_body)) = both("GET", &path).await;
        assert_eq!((go_status, rs_status), (404, 404), "{path}");
        let go_err = common::assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &path);
        assert_eq!(
            go_err["id"], "app.team.get_by_name.missing.app_error",
            "{path}"
        );
    }

    let ((go_status, go_body), (rs_status, rs_body)) = both_json(
        "POST",
        "/api/v4/teams/search",
        r#"{"term":"mmrs-parity-loctread"}"#,
    )
    .await;
    assert_eq!((go_status, rs_status), (200, 200), "POST /teams/search");
    assert_eq!(
        mine(&rs_body),
        mine(&go_body),
        "the search finds the fixture team on both"
    );
    assert!(mine(&go_body).is_some(), "and Go's search does find it");

    // The admin created the team, so the admin is a member — and the socket still says `[]`.
    let path = format!("/api/v4/teams/{team}/members/ids");
    let ((go_status, go_body), (rs_status, rs_body)) =
        both_json("POST", &path, &format!(r#"["{admin}"]"#)).await;
    assert_eq!((go_status, rs_status), (200, 200), "{path}");
    assert_eq!(
        String::from_utf8_lossy(&go_body),
        "[]",
        "the local session's view restrictions are empty on Go"
    );
    assert_eq!(go_body, rs_body, "{path}: and on ours, no newline");

    for (body, id) in [
        ("[]", "api.context.invalid_body_param.app_error"),
        ("{}", "api.payload.parse.error"),
    ] {
        let ((go_status, go_body), (rs_status, rs_body)) = both_json("POST", &path, body).await;
        assert_eq!((go_status, rs_status), (400, 400), "{path} with {body}");
        let go_err = common::assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &path);
        assert_eq!(go_err["id"], id, "{path} with {body}");
    }

    // The content-reviewer flag is the HTTP handler's port forward; over the socket it must be
    // Go's own answer — on this unlicensed stack, content flagging's 501.
    let path = format!("/api/v4/teams/{team}?as_content_reviewer=true");
    let ((go_status, go_body), (rs_status, rs_body), served) =
        both_maybe_forwarded("GET", &path).await;
    assert!(!served, "{path} is forwarded over the socket");
    assert_eq!(go_status, 501, "{path}: content flagging is licensed");
    assert_eq!(rs_status, go_status, "{path}");
    assert_forwarded_body_is_gos(&go_body, &rs_body, &path);
}

/// **The team writes, each server on its own team.** `localCreateTeam` keeps the body's email
/// (lower-cased) and the invite id; the update, patch, privacy, archive and restore are the HTTP
/// handlers; the permanent delete is forwarded and *happens*, with `EnableAPITeamDeletion` off.
#[tokio::test]
async fn the_team_writes_match_over_the_socket() {
    if !sockets_enabled() {
        return;
    }
    common::purge_api_fixtures().await;
    let go = go_socket().expect("checked");
    let rust = rust_socket().expect("checked");

    let create = |tag: &str| {
        format!(
            r#"{{"name":"mmrs-parity-loctw{tag}","display_name":"mmrs parity loctw","type":"O","email":"MMRS-LocT@Example.com","description":"created over the socket","allow_open_invite":true,"allowed_domains":"example.com","invite_id":"chosenbytheclientxxxxxxxxx"}}"#
        )
    };
    let (go_status, _, go_body) = send(&go, "POST", "/api/v4/teams", Some(&create("go"))).await;
    let (rs_status, rs_headers, rs_body) =
        send(&rust, "POST", "/api/v4/teams", Some(&create("rs"))).await;
    assert!(served_here(&rs_headers), "POST /teams must be served here");
    assert_eq!(
        go_status,
        201,
        "Go creates a team over its socket: {}",
        String::from_utf8_lossy(&go_body)
    );
    assert_eq!(
        rs_status,
        go_status,
        "{}",
        String::from_utf8_lossy(&rs_body)
    );
    assert_eq!(rs_body.last(), Some(&b'\n'), "json.NewEncoder's newline");
    let (go_team, rs_team) = (parse(&go_body), parse(&rs_body));
    assert_eq!(
        normalise_team(&go_team),
        normalise_team(&rs_team),
        "POST /teams"
    );
    assert_eq!(
        go_team["email"], "mmrs-loct@example.com",
        "the body's email is kept, lower-cased — there is no creator to take it from"
    );
    for (who, team) in [("Go", &go_team), ("ours", &rs_team)] {
        let invite = team["invite_id"].as_str().unwrap_or_default();
        assert_eq!(
            invite.len(),
            26,
            "{who}: the invite id is minted and never blanked"
        );
        assert_ne!(
            invite, "chosenbytheclientxxxxxxxxx",
            "{who}: and not the client's"
        );
    }
    let go_id = go_team["id"].as_str().expect("an id").to_owned();
    let rs_id = rs_team["id"].as_str().expect("an id").to_owned();

    // The same malformed bodies refuse the same way on both.
    for (body, id, status) in [
        ("[1]", "api.context.invalid_body_param.app_error", 400),
        (
            r#"{"name":"mmrs-parity-loctwgo","display_name":"dup","type":"O"}"#,
            "store.sql_team.save_team.existing.app_error",
            400,
        ),
        (
            r#"{"name":"","display_name":"x","type":"O"}"#,
            "model.team.is_valid.characters.app_error",
            400,
        ),
    ] {
        let ((go_status, go_body), (rs_status, rs_body)) =
            both_json("POST", "/api/v4/teams", body).await;
        assert_eq!(
            (go_status, rs_status),
            (status, status),
            "POST /teams with {body}"
        );
        let go_err =
            common::assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "POST /teams");
        assert_eq!(go_err["id"], id, "POST /teams with {body}");
    }

    // A round of writes on each server's own team, compared normalised.
    let write = |method: &'static str,
                 suffix: &'static str,
                 body: &'static str,
                 expect: u16,
                 label: &'static str| {
        write_both(
            &go, &rust, &go_id, &rs_id, method, suffix, body, expect, label,
        )
    };

    // `updateTeam` demands the body's id equal the path's, so the bodies are built per server —
    // and the update **renames** (`App.UpdateTeam` copies `Name`, and a taken name is the 400
    // `rename_team.name_occupied` on both), so each server gets its own new name.
    for (socket, id, tag, label) in [(&go, &go_id, "go", "Go"), (&rust, &rs_id, "rs", "ours")] {
        let body = format!(
            r#"{{"id":"{id}","display_name":"mmrs parity renamed","description":"updated","company_name":"mmrs","allow_open_invite":false,"allowed_domains":"","type":"O","name":"mmrs-parity-loctwren{tag}"}}"#
        );
        let (status, headers, body) =
            send(socket, "PUT", &format!("/api/v4/teams/{id}"), Some(&body)).await;
        if label == "ours" {
            assert!(
                served_here(&headers),
                "PUT /teams/{{id}} must be served here"
            );
        }
        assert_eq!(
            status,
            200,
            "{label}: PUT /teams/{{id}}: {}",
            String::from_utf8_lossy(&body)
        );
        let team = parse(&body);
        assert_eq!(team["display_name"], "mmrs parity renamed", "{label}");
        assert_eq!(team["allow_open_invite"], false, "{label}");
        assert_eq!(
            team["name"],
            format!("mmrs-parity-loctwren{tag}"),
            "{label}: the update renames"
        );
    }
    let (go_body, rs_body) = write("GET", "", "", 200, "GET after the update").await;
    assert_eq!(
        normalise_team(&parse(&go_body)),
        normalise_team(&parse(&rs_body))
    );

    let (go_body, rs_body) = write(
        "PUT",
        "/patch",
        r#"{"description":"patched over the socket"}"#,
        200,
        "PUT /teams/{id}/patch",
    )
    .await;
    assert_eq!(
        normalise_team(&parse(&go_body)),
        normalise_team(&parse(&rs_body))
    );
    assert_eq!(parse(&rs_body)["description"], "patched over the socket");

    let (go_body, rs_body) = write(
        "PUT",
        "/privacy",
        // `model.TeamOpen` is the one-letter type, not the word.
        r#"{"privacy":"O"}"#,
        200,
        "PUT /teams/{id}/privacy",
    )
    .await;
    assert_eq!(
        normalise_team(&parse(&go_body)),
        normalise_team(&parse(&rs_body))
    );
    assert_eq!(parse(&rs_body)["allow_open_invite"], true);

    let (go_body, rs_body) = write("DELETE", "", "", 200, "DELETE /teams/{id}").await;
    assert_eq!(String::from_utf8_lossy(&rs_body), r#"{"status":"OK"}"#);
    assert_eq!(go_body, rs_body, "ReturnStatusOK on both, no newline");
    let (go_body, rs_body) = write("GET", "", "", 200, "GET after the archive").await;
    assert_ne!(parse(&go_body)["delete_at"], 0, "Go archived its team");
    assert_ne!(parse(&rs_body)["delete_at"], 0, "we archived ours");

    let (go_body, rs_body) = write("POST", "/restore", "", 200, "POST /teams/{id}/restore").await;
    assert_eq!(
        normalise_team(&parse(&go_body)),
        normalise_team(&parse(&rs_body))
    );
    assert_eq!(parse(&rs_body)["delete_at"], 0, "and restored");

    // A patch turning on group-constraint is forwarded over the socket, and Go applies it.
    for (socket, id, label) in [(&go, &go_id, "Go"), (&rust, &rs_id, "ours")] {
        let (status, headers, body) = send(
            socket,
            "PUT",
            &format!("/api/v4/teams/{id}/patch"),
            Some(r#"{"group_constrained":true}"#),
        )
        .await;
        assert!(
            !served_here(&headers),
            "{label}: the group-constraint patch is Go's to make"
        );
        assert_eq!(status, 200, "{label}: {}", String::from_utf8_lossy(&body));
        assert_eq!(parse(&body)["group_constrained"], true, "{label}");
    }

    // The permanent delete: forwarded, performed, and gone on both — with the API flag off.
    let (go_body, rs_body) = {
        let (go_status, _, go_body) = send(
            &go,
            "DELETE",
            &format!("/api/v4/teams/{go_id}?permanent=true"),
            None,
        )
        .await;
        let (rs_status, rs_headers, rs_body) = send(
            &rust,
            "DELETE",
            &format!("/api/v4/teams/{rs_id}?permanent=true"),
            None,
        )
        .await;
        assert!(
            !served_here(&rs_headers),
            "the permanent arm is forwarded over the socket"
        );
        assert_eq!(
            go_status,
            200,
            "Go deletes permanently over its socket: {}",
            String::from_utf8_lossy(&go_body)
        );
        assert_eq!(
            rs_status,
            go_status,
            "{}",
            String::from_utf8_lossy(&rs_body)
        );
        (go_body, rs_body)
    };
    assert_eq!(
        go_body, rs_body,
        "the permanent delete answers ReturnStatusOK"
    );
    let (go_body, rs_body) = write("GET", "", "", 404, "GET after the permanent delete").await;
    let go_err = common::assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "gone");
    assert_eq!(
        go_err["id"], "app.team.get.find.app_error",
        "the rows are gone on both"
    );
}

/// **`addTeamMember` and `removeTeamMember` with no session user.** A real user is added to each
/// server's own team and removed again; `me` is a 400 on both because the session's user id is
/// `""`; an empty `user_id` in the body is the same 400.
#[tokio::test]
async fn the_member_routes_match_over_the_socket() {
    if !sockets_enabled() {
        return;
    }
    let client = common::client();
    let token = common::go_minted_token(&client).await;
    let (shared_team, _) = common::a_team_and_channel_the_user_is_in(&client, &token).await;
    let plain = common::create_plain_user(&client, &token, &shared_team, "loctm").await;
    let go_team = common::create_team(&client, &token, "loctmgo").await;
    let rs_team = common::create_team(&client, &token, "loctmrs").await;
    common::invalidate_go_caches(&client, &token).await;
    let go = go_socket().expect("checked");
    let rust = rust_socket().expect("checked");

    let (go_status, _, go_body) = send(
        &go,
        "POST",
        &format!("/api/v4/teams/{go_team}/members"),
        Some(&format!(
            r#"{{"team_id":"{go_team}","user_id":"{}"}}"#,
            plain.id
        )),
    )
    .await;
    let (rs_status, rs_headers, rs_body) = send(
        &rust,
        "POST",
        &format!("/api/v4/teams/{rs_team}/members"),
        Some(&format!(
            r#"{{"team_id":"{rs_team}","user_id":"{}"}}"#,
            plain.id
        )),
    )
    .await;
    assert!(
        served_here(&rs_headers),
        "POST /teams/{{id}}/members must be served here"
    );
    assert_eq!(
        go_status,
        201,
        "Go adds a member over its socket: {}",
        String::from_utf8_lossy(&go_body)
    );
    assert_eq!(
        rs_status,
        go_status,
        "{}",
        String::from_utf8_lossy(&rs_body)
    );
    let strip_team = |body: &[u8]| normalise(&parse(body), &["team_id"], &[]);
    assert_eq!(
        strip_team(&go_body),
        strip_team(&rs_body),
        "POST /teams/{{id}}/members"
    );
    assert_eq!(parse(&rs_body)["user_id"], plain.id.as_str());

    for (body, label) in [
        (
            format!(r#"{{"team_id":"{go_team}","user_id":""}}"#),
            "an empty user_id",
        ),
        (
            format!(r#"{{"team_id":"{rs_team}","user_id":"{}"}}"#, plain.id),
            "a mismatched team_id",
        ),
    ] {
        let path = format!("/api/v4/teams/{go_team}/members");
        let ((go_status, go_body), (rs_status, rs_body)) = both_json("POST", &path, &body).await;
        assert_eq!((go_status, rs_status), (400, 400), "{label}");
        let go_err = common::assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, label);
        assert_eq!(
            go_err["id"], "api.context.invalid_body_param.app_error",
            "{label}"
        );
    }

    let path = format!("/api/v4/teams/{go_team}/members/me");
    let ((go_status, go_body), (rs_status, rs_body)) = both("DELETE", &path).await;
    assert_eq!(
        (go_status, rs_status),
        (400, 400),
        "{path}: `me` is nobody on the socket"
    );
    let go_err = common::assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &path);
    assert_eq!(go_err["id"], "api.context.invalid_url_param.app_error");

    let (go_status, _, go_body) = send(
        &go,
        "DELETE",
        &format!("/api/v4/teams/{go_team}/members/{}", plain.id),
        None,
    )
    .await;
    let (rs_status, rs_headers, rs_body) = send(
        &rust,
        "DELETE",
        &format!("/api/v4/teams/{rs_team}/members/{}", plain.id),
        None,
    )
    .await;
    assert!(served_here(&rs_headers), "the removal must be served here");
    assert_eq!(go_status, 200, "{}", String::from_utf8_lossy(&go_body));
    assert_eq!(
        rs_status,
        go_status,
        "{}",
        String::from_utf8_lossy(&rs_body)
    );
    assert_eq!(String::from_utf8_lossy(&rs_body), r#"{"status":"OK"}"#);
    assert_eq!(go_body, rs_body);

    common::delete_plain_user(&client, &token, &plain.id).await;
}

/// **`localInviteUsersToTeam` up to the send.** Its six refusals are served here and agree with
/// Go's socket; the send itself is forwarded and both answers are Go's — `{"status":"OK"}`, and
/// the graceful list.
#[tokio::test]
async fn the_invite_gates_match_and_the_send_is_forwarded() {
    if !sockets_enabled() {
        return;
    }
    let client = common::client();
    let token = common::go_minted_token(&client).await;
    let team = common::create_team(&client, &token, "loctinv").await;
    common::invalidate_go_caches(&client, &token).await;
    let path = format!("/api/v4/teams/{team}/invite/email");

    for (body, status, id) in [
        ("{}", 400, "api.context.invalid_body_param.app_error"),
        ("null", 400, "api.context.invalid_body_param.app_error"),
        (
            r#"["Not@An@Email"]"#,
            400,
            "api.team.invite_members.invalid_email.app_error",
        ),
        (
            r#"{"emails":["ok@example.com","nope"]}"#,
            400,
            "api.team.invite_members.invalid_email.app_error",
        ),
        (
            r#"{"emails":["a@example.com"],"profiles":[{"email":"a@example.com"}]}"#,
            400,
            "api.team.invite_members.profiles_graceful.app_error",
        ),
        (
            "[",
            400,
            "api.team.invite_members_to_team_and_channels.invalid_body.app_error",
        ),
    ] {
        let ((go_status, go_body), (rs_status, rs_body)) = both_json("POST", &path, body).await;
        assert_eq!(
            (go_status, rs_status),
            (status, status),
            "{path} with {body}"
        );
        let go_err = common::assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &path);
        assert_eq!(go_err["id"], id, "{path} with {body}");
    }

    let missing = "/api/v4/teams/zzzzzzzzzzzzzzzzzzzzzzzzzz/invite/email";
    let ((go_status, go_body), (rs_status, rs_body)) =
        both_json("POST", missing, r#"{"emails":["a@example.com"]}"#).await;
    assert_eq!((go_status, rs_status), (404, 404), "{missing}");
    let go_err = common::assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, missing);
    assert_eq!(go_err["id"], "app.team.get.find.app_error");

    // Past the gates: the send, which is Go's on both sides.
    let ((go_status, go_body), (rs_status, rs_body), served) = both_json_maybe_forwarded(
        "POST",
        &path,
        r#"{"emails":["mmrs-local-invite@example.com"]}"#,
    )
    .await;
    assert!(!served, "the send is forwarded over the socket");
    assert_eq!(
        (go_status, rs_status),
        (200, 200),
        "{}",
        String::from_utf8_lossy(&rs_body)
    );
    assert_eq!(String::from_utf8_lossy(&go_body), r#"{"status":"OK"}"#);
    assert_eq!(go_body, rs_body);

    let graceful = format!("{path}?graceful=true");
    let ((go_status, go_body), (rs_status, rs_body), served) = both_json_maybe_forwarded(
        "POST",
        &graceful,
        r#"{"emails":["mmrs-local-graceful@example.com"]}"#,
    )
    .await;
    assert!(!served, "the graceful send is forwarded too");
    assert_eq!((go_status, rs_status), (200, 200));
    assert_eq!(go_body, rs_body, "the per-address list is Go's on both");
    assert_eq!(
        parse(&rs_body)[0]["email"],
        "mmrs-local-graceful@example.com",
        "and it names the address"
    );
}

/// **The five incoming-hook pairs.** `localCreateIncomingHook` requires `user_id` and creates
/// the hook as posted — `channel_locked` is not forced; the four reads and writes are the HTTP
/// handlers, whose ownership checks the local session always passes.
#[tokio::test]
async fn the_incoming_hook_routes_match_over_the_socket() {
    if !sockets_enabled() {
        return;
    }
    let client = common::client();
    let token = common::go_minted_token(&client).await;
    let (team, _) = common::a_team_and_channel_the_user_is_in(&client, &token).await;
    let channel = common::create_channel(&client, &token, &team, "locthin").await;
    let admin = common::logged_in_user_id();
    common::invalidate_go_caches(&client, &token).await;
    let go = go_socket().expect("checked");
    let rust = rust_socket().expect("checked");

    let create = |tag: &str| {
        format!(
            r#"{{"channel_id":"{channel}","user_id":"{admin}","display_name":"mmrs local in {tag}","description":"planted over the socket","channel_locked":false}}"#
        )
    };
    let (go_status, _, go_body) =
        send(&go, "POST", "/api/v4/hooks/incoming", Some(&create("go"))).await;
    let (rs_status, rs_headers, rs_body) =
        send(&rust, "POST", "/api/v4/hooks/incoming", Some(&create("rs"))).await;
    assert!(
        served_here(&rs_headers),
        "POST /hooks/incoming must be served here"
    );
    assert_eq!(go_status, 201, "{}", String::from_utf8_lossy(&go_body));
    assert_eq!(
        rs_status,
        go_status,
        "{}",
        String::from_utf8_lossy(&rs_body)
    );
    assert_eq!(rs_body.last(), Some(&b'\n'));
    let (go_hook, rs_hook) = (parse(&go_body), parse(&rs_body));
    assert_eq!(
        normalise_hook(&go_hook),
        normalise_hook(&rs_hook),
        "POST /hooks/incoming"
    );
    assert_eq!(
        rs_hook["channel_locked"], false,
        "no session, no `bypass_incoming_webhook_channel_lock` forcing"
    );
    assert_eq!(rs_hook["user_id"], admin, "the body's user is the owner");
    let go_id = go_hook["id"].as_str().expect("an id").to_owned();
    let rs_id = rs_hook["id"].as_str().expect("an id").to_owned();

    for (body, status, id) in [
        (
            format!(r#"{{"channel_id":"{channel}","display_name":"mmrs no user"}}"#),
            400,
            "api.context.invalid_body_param.app_error",
        ),
        (
            format!(
                r#"{{"channel_id":"zzzzzzzzzzzzzzzzzzzzzzzzzz","user_id":"{admin}","display_name":"mmrs no channel"}}"#
            ),
            404,
            "app.channel.get.existing.app_error",
        ),
        (
            format!(
                r#"{{"channel_id":"{channel}","user_id":"zzzzzzzzzzzzzzzzzzzzzzzzzz","display_name":"mmrs no such user"}}"#
            ),
            404,
            "app.user.missing_account.const",
        ),
        (
            "null".to_owned(),
            400,
            "api.context.invalid_body_param.app_error",
        ),
    ] {
        let ((go_status, go_body), (rs_status, rs_body)) =
            both_json("POST", "/api/v4/hooks/incoming", &body).await;
        assert_eq!(
            (go_status, rs_status),
            (status, status),
            "POST /hooks/incoming with {body}"
        );
        let go_err = common::assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &body);
        assert_eq!(go_err["id"], id, "POST /hooks/incoming with {body}");
    }

    let ((go_status, go_body), (rs_status, rs_body)) = both("GET", "/api/v4/hooks/incoming").await;
    assert_eq!((go_status, rs_status), (200, 200));
    let entry = |body: &[u8], id: &str| {
        parse(body)
            .as_array()
            .expect("an array")
            .iter()
            .find(|h| h["id"] == id)
            .cloned()
    };
    for id in [&go_id, &rs_id] {
        let mine = entry(&go_body, id);
        assert!(mine.is_some(), "Go lists hook {id} with no user filter");
        assert_eq!(entry(&rs_body, id), mine, "GET /hooks/incoming: {id}");
    }

    let path = format!("/api/v4/hooks/incoming/{go_id}");
    let ((go_status, go_body), (rs_status, rs_body)) = both("GET", &path).await;
    assert_eq!((go_status, rs_status), (200, 200));
    assert_eq!(go_body, rs_body, "{path}");

    for (socket, id, label) in [(&go, &go_id, "Go"), (&rust, &rs_id, "ours")] {
        let body = format!(
            r#"{{"id":"{id}","channel_id":"{channel}","display_name":"mmrs local in updated","description":"updated over the socket"}}"#
        );
        let (status, headers, body) = send(
            socket,
            "PUT",
            &format!("/api/v4/hooks/incoming/{id}"),
            Some(&body),
        )
        .await;
        if label == "ours" {
            assert!(served_here(&headers));
        }
        assert_eq!(
            status,
            201,
            "{label}: PUT answers 201: {}",
            String::from_utf8_lossy(&body)
        );
        assert_eq!(
            parse(&body)["description"],
            "updated over the socket",
            "{label}"
        );
    }

    let (go_status, _, go_body) = send(
        &go,
        "DELETE",
        &format!("/api/v4/hooks/incoming/{go_id}"),
        None,
    )
    .await;
    let (rs_status, rs_headers, rs_body) = send(
        &rust,
        "DELETE",
        &format!("/api/v4/hooks/incoming/{rs_id}"),
        None,
    )
    .await;
    assert!(served_here(&rs_headers));
    assert_eq!((go_status, rs_status), (200, 200));
    assert_eq!(go_body, rs_body);
    assert_eq!(String::from_utf8_lossy(&rs_body), r#"{"status":"OK"}"#);

    common::delete_channel(&client, &token, &channel).await;
}

/// **The five outgoing-hook pairs**, with `creator_id` required by `localCreateOutgoingHook`.
#[tokio::test]
async fn the_outgoing_hook_routes_match_over_the_socket() {
    if !sockets_enabled() {
        return;
    }
    let client = common::client();
    let token = common::go_minted_token(&client).await;
    let (team, _) = common::a_team_and_channel_the_user_is_in(&client, &token).await;
    let channel = common::create_channel(&client, &token, &team, "locthout").await;
    let admin = common::logged_in_user_id();
    common::invalidate_go_caches(&client, &token).await;
    let go = go_socket().expect("checked");
    let rust = rust_socket().expect("checked");

    let create = |tag: &str| {
        format!(
            r#"{{"team_id":"{team}","channel_id":"{channel}","creator_id":"{admin}","display_name":"mmrs local out {tag}","trigger_words":["mmrsloc{tag}"],"callback_urls":["https://example.invalid/mmrs?a=1&b=2"]}}"#
        )
    };
    let (go_status, _, go_body) =
        send(&go, "POST", "/api/v4/hooks/outgoing", Some(&create("go"))).await;
    let (rs_status, rs_headers, rs_body) =
        send(&rust, "POST", "/api/v4/hooks/outgoing", Some(&create("rs"))).await;
    assert!(
        served_here(&rs_headers),
        "POST /hooks/outgoing must be served here"
    );
    assert_eq!(go_status, 201, "{}", String::from_utf8_lossy(&go_body));
    assert_eq!(
        rs_status,
        go_status,
        "{}",
        String::from_utf8_lossy(&rs_body)
    );
    let (go_hook, rs_hook) = (parse(&go_body), parse(&rs_body));
    assert_eq!(
        normalise_hook(&go_hook),
        normalise_hook(&rs_hook),
        "POST /hooks/outgoing"
    );
    assert!(
        rs_body.windows(6).any(|w| w == b"\\u0026"),
        "the callback URL's `&` is escaped the way encoding/json does"
    );
    let go_id = go_hook["id"].as_str().expect("an id").to_owned();
    let rs_id = rs_hook["id"].as_str().expect("an id").to_owned();

    for (body, status, id) in [
        (
            format!(
                r#"{{"team_id":"{team}","channel_id":"{channel}","display_name":"mmrs no creator","trigger_words":["mmrslocx"],"callback_urls":["https://example.invalid/"]}}"#
            ),
            400,
            "api.context.invalid_body_param.app_error",
        ),
        (
            format!(
                r#"{{"team_id":"{team}","channel_id":"{channel}","creator_id":"zzzzzzzzzzzzzzzzzzzzzzzzzz","display_name":"mmrs no such creator","trigger_words":["mmrslocy"],"callback_urls":["https://example.invalid/"]}}"#
            ),
            404,
            "app.user.missing_account.const",
        ),
    ] {
        let ((go_status, go_body), (rs_status, rs_body)) =
            both_json("POST", "/api/v4/hooks/outgoing", &body).await;
        assert_eq!(
            (go_status, rs_status),
            (status, status),
            "POST /hooks/outgoing with {body}"
        );
        let go_err = common::assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &body);
        assert_eq!(go_err["id"], id, "POST /hooks/outgoing with {body}");
    }

    let list = format!("/api/v4/hooks/outgoing?channel_id={channel}");
    let ((go_status, go_body), (rs_status, rs_body)) = both("GET", &list).await;
    assert_eq!((go_status, rs_status), (200, 200), "{list}");
    assert_eq!(
        go_body, rs_body,
        "{list}: this channel's two hooks, byte for byte"
    );
    assert_eq!(parse(&go_body).as_array().map(Vec::len), Some(2));

    let path = format!("/api/v4/hooks/outgoing/{go_id}");
    let ((go_status, go_body), (rs_status, rs_body)) = both("GET", &path).await;
    assert_eq!((go_status, rs_status), (200, 200));
    assert_eq!(go_body, rs_body, "{path}");

    for (socket, id, tag, label) in [(&go, &go_id, "go", "Go"), (&rust, &rs_id, "rs", "ours")] {
        let body = format!(
            r#"{{"id":"{id}","team_id":"{team}","channel_id":"{channel}","display_name":"mmrs local out updated","trigger_words":["mmrsloc{tag}"],"callback_urls":["https://example.invalid/updated"]}}"#
        );
        let (status, headers, body) = send(
            socket,
            "PUT",
            &format!("/api/v4/hooks/outgoing/{id}"),
            Some(&body),
        )
        .await;
        if label == "ours" {
            assert!(served_here(&headers));
        }
        assert_eq!(status, 200, "{label}: {}", String::from_utf8_lossy(&body));
        assert_eq!(
            parse(&body)["display_name"],
            "mmrs local out updated",
            "{label}"
        );
    }

    let (go_status, _, go_body) = send(
        &go,
        "DELETE",
        &format!("/api/v4/hooks/outgoing/{go_id}"),
        None,
    )
    .await;
    let (rs_status, rs_headers, rs_body) = send(
        &rust,
        "DELETE",
        &format!("/api/v4/hooks/outgoing/{rs_id}"),
        None,
    )
    .await;
    assert!(served_here(&rs_headers));
    assert_eq!((go_status, rs_status), (200, 200));
    assert_eq!(go_body, rs_body);
    assert_eq!(String::from_utf8_lossy(&rs_body), r#"{"status":"OK"}"#);

    common::delete_channel(&client, &token, &channel).await;
}

/// **The six command pairs.** `localCreateCommand` takes `creator_id` from the body without a
/// lookup — an empty one is `IsValid`'s 400, not a silently filled creator; the built-in list
/// (no `custom_only`) is forwarded; the rest are the HTTP handlers.
#[tokio::test]
async fn the_command_routes_match_over_the_socket() {
    if !sockets_enabled() {
        return;
    }
    let client = common::client();
    let token = common::go_minted_token(&client).await;
    let go_team = common::create_team(&client, &token, "loctcgo").await;
    let rs_team = common::create_team(&client, &token, "loctcrs").await;
    let admin = common::logged_in_user_id();
    sweep_commands(&[&go_team, &rs_team]).await;
    common::invalidate_go_caches(&client, &token).await;
    let go = go_socket().expect("checked");
    let rust = rust_socket().expect("checked");

    let create = |team: &str, tag: &str, creator: &str| {
        format!(
            r#"{{"team_id":"{team}","trigger":"mmrsloc{tag}","method":"P","url":"https://example.invalid/hook?a=1&b=2","display_name":"mmrs local <command>","creator_id":"{creator}","auto_complete":true}}"#
        )
    };
    let (go_status, _, go_body) = send(
        &go,
        "POST",
        "/api/v4/commands",
        Some(&create(&go_team, "go", admin)),
    )
    .await;
    let (rs_status, rs_headers, rs_body) = send(
        &rust,
        "POST",
        "/api/v4/commands",
        Some(&create(&rs_team, "rs", admin)),
    )
    .await;
    assert!(
        served_here(&rs_headers),
        "POST /commands must be served here"
    );
    assert_eq!(go_status, 201, "{}", String::from_utf8_lossy(&go_body));
    assert_eq!(
        rs_status,
        go_status,
        "{}",
        String::from_utf8_lossy(&rs_body)
    );
    let (go_cmd, rs_cmd) = (parse(&go_body), parse(&rs_body));
    assert_eq!(
        normalise_command(&go_cmd),
        normalise_command(&rs_cmd),
        "POST /commands"
    );
    assert_eq!(rs_cmd["creator_id"], admin, "the body's creator, unchanged");
    let go_id = go_cmd["id"].as_str().expect("an id").to_owned();
    let rs_id = rs_cmd["id"].as_str().expect("an id").to_owned();

    for (body, id) in [
        // An empty creator with no plugin is `IsValid`'s plugin-or-creator rule, not a filled
        // creator and not a 404: nothing looks the user up.
        (
            create(&go_team, "nocreator", ""),
            "model.command.is_valid.plugin_id.app_error",
        ),
        (
            create("", "noteam", admin),
            "model.command.is_valid.team_id.app_error",
        ),
        ("[1]".to_owned(), "api.context.invalid_body_param.app_error"),
    ] {
        let ((go_status, go_body), (rs_status, rs_body)) =
            both_json("POST", "/api/v4/commands", &body).await;
        assert_eq!(
            (go_status, rs_status),
            (400, 400),
            "POST /commands with {body}"
        );
        let go_err = common::assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &body);
        assert_eq!(go_err["id"], id, "POST /commands with {body}");
    }

    // A well-formed creator id that names nobody is accepted by both — `localCreateCommand`
    // does no lookup, where `createCommand` would have answered `GetUser`'s 404.
    let (go_status, _, go_body) = send(
        &go,
        "POST",
        "/api/v4/commands",
        Some(&create(&go_team, "fakego", "zzzzzzzzzzzzzzzzzzzzzzzzzz")),
    )
    .await;
    let (rs_status, rs_headers, rs_body) = send(
        &rust,
        "POST",
        "/api/v4/commands",
        Some(&create(&rs_team, "fakers", "zzzzzzzzzzzzzzzzzzzzzzzzzz")),
    )
    .await;
    assert!(served_here(&rs_headers));
    assert_eq!(
        go_status,
        201,
        "Go creates a command for a creator that does not exist: {}",
        String::from_utf8_lossy(&go_body)
    );
    assert_eq!(
        rs_status,
        go_status,
        "{}",
        String::from_utf8_lossy(&rs_body)
    );
    assert_eq!(
        normalise_command(&parse(&go_body)),
        normalise_command(&parse(&rs_body))
    );

    let list = format!("/api/v4/commands?team_id={go_team}&custom_only=true");
    let ((go_status, go_body), (rs_status, rs_body)) = both("GET", &list).await;
    assert_eq!((go_status, rs_status), (200, 200), "{list}");
    assert_eq!(
        go_body, rs_body,
        "{list}: Go's team's two commands, byte for byte"
    );
    assert!(
        parse(&go_body)
            .as_array()
            .is_some_and(|c| c.iter().any(|cmd| cmd["id"] == go_id.as_str())),
        "{list} carries the command Go created"
    );

    let builtins = format!("/api/v4/commands?team_id={go_team}");
    let ((go_status, _), (rs_status, _), served) = both_maybe_forwarded("GET", &builtins).await;
    assert!(
        !served,
        "the built-in registry is Go's; forwarded over the socket"
    );
    assert_eq!((go_status, rs_status), (200, 200), "{builtins}");

    let path = format!("/api/v4/commands/{go_id}");
    let ((go_status, go_body), (rs_status, rs_body)) = both("GET", &path).await;
    assert_eq!((go_status, rs_status), (200, 200), "{path}");
    assert_eq!(go_body, rs_body, "{path}");

    for (socket, id, team, tag, label) in [
        (&go, &go_id, &go_team, "go", "Go"),
        (&rust, &rs_id, &rs_team, "rs", "ours"),
    ] {
        let body = format!(
            r#"{{"id":"{id}","team_id":"{team}","trigger":"mmrsloc{tag}","method":"G","url":"https://example.invalid/updated","display_name":"mmrs local updated","creator_id":"{admin}"}}"#
        );
        let (status, headers, body) = send(
            socket,
            "PUT",
            &format!("/api/v4/commands/{id}"),
            Some(&body),
        )
        .await;
        if label == "ours" {
            assert!(served_here(&headers));
        }
        assert_eq!(status, 200, "{label}: {}", String::from_utf8_lossy(&body));
        assert_eq!(parse(&body)["method"], "G", "{label}");
    }

    // Each command moves to the *other* server's team, where its trigger is free.
    let (go_status, _, go_body) = send(
        &go,
        "PUT",
        &format!("/api/v4/commands/{go_id}/move"),
        Some(&format!(r#"{{"team_id":"{rs_team}"}}"#)),
    )
    .await;
    let (rs_status, rs_headers, rs_body) = send(
        &rust,
        "PUT",
        &format!("/api/v4/commands/{rs_id}/move"),
        Some(&format!(r#"{{"team_id":"{go_team}"}}"#)),
    )
    .await;
    assert!(served_here(&rs_headers), "the move must be served here");
    assert_eq!(go_status, 200, "{}", String::from_utf8_lossy(&go_body));
    assert_eq!(
        rs_status,
        go_status,
        "{}",
        String::from_utf8_lossy(&rs_body)
    );
    assert_eq!(go_body, rs_body);
    assert_eq!(String::from_utf8_lossy(&rs_body), r#"{"status":"OK"}"#);

    let (go_status, _, go_body) =
        send(&go, "DELETE", &format!("/api/v4/commands/{go_id}"), None).await;
    let (rs_status, rs_headers, rs_body) =
        send(&rust, "DELETE", &format!("/api/v4/commands/{rs_id}"), None).await;
    assert!(served_here(&rs_headers));
    assert_eq!((go_status, rs_status), (200, 200));
    assert_eq!(go_body, rs_body);

    sweep_commands(&[&go_team, &rs_team]).await;
}

/// **Thirty registrations beside the ones already there.** `/teams/search` and
/// `/teams/name/{x}` sit beside `/teams/{team_id}`, `/members/ids` beside `/members/{user_id}`;
/// the routes the socket served before this family landed must still answer here, and the
/// methods a literal does not carry must still reach Go and come back as its `{param}` answer.
#[tokio::test]
async fn the_previously_served_local_routes_still_answer_and_the_literals_resolve() {
    if !sockets_enabled() {
        return;
    }
    let client = common::client();
    let _token = common::go_minted_token(&client).await;
    let admin = common::logged_in_user_id();
    let _unlicensed = common::ACTIVE_LICENCE_ROW.read().await;

    for path in [
        "/api/v4/system/ping".to_owned(),
        "/api/v4/bots".to_owned(),
        "/api/v4/roles".to_owned(),
        format!("/api/v4/users/{admin}/status"),
    ] {
        let ((go_status, _), (rs_status, _)) = both("GET", &path).await;
        assert_eq!(
            (go_status, rs_status),
            (200, 200),
            "{path} still answers here"
        );
    }

    // A method the literal does not carry falls to the method fallback and is Go's own
    // `{param}` answer: `getTeam("search")` and `removeTeamMember(..., "ids")`, both 400.
    for (method, path) in [
        ("GET", "/api/v4/teams/search".to_owned()),
        (
            "DELETE",
            "/api/v4/teams/zzzzzzzzzzzzzzzzzzzzzzzzzz/members/ids".to_owned(),
        ),
    ] {
        let ((go_status, go_body), (rs_status, rs_body), served) =
            both_maybe_forwarded(method, &path).await;
        assert!(!served, "{method} {path} is forwarded, not answered here");
        assert_eq!((go_status, rs_status), (400, 400), "{method} {path}");
        assert_forwarded_body_is_gos(&go_body, &rs_body, &path);
    }

    // Segments outside Go's mux classes are its 404s, over the socket.
    for path in [
        "/api/v4/teams/bad-id",
        "/api/v4/teams/name/bad.name",
        "/api/v4/hooks/incoming/bad-id",
        "/api/v4/hooks/outgoing/bad-id",
        "/api/v4/commands/bad-id",
    ] {
        let ((go_status, go_body), (rs_status, rs_body), served) =
            both_maybe_forwarded("GET", path).await;
        assert!(!served, "{path} must be forwarded");
        assert_eq!((go_status, rs_status), (404, 404), "{path}");
        assert_forwarded_body_is_gos(&go_body, &rs_body, path);
    }

    // And the same family over TCP, with no token, is still refused: the unrestricted session
    // reaches only the socket.
    for (method, path) in [
        ("POST", "/api/v4/teams"),
        ("POST", "/api/v4/hooks/incoming"),
        ("POST", "/api/v4/commands"),
    ] {
        let response = client
            .request(
                method.parse().expect("a method"),
                format!("{}{path}", common::RUST),
            )
            .body("{}")
            .send()
            .await
            .expect("the TCP server answers");
        assert_eq!(
            response.status().as_u16(),
            401,
            "{method} {path} over the port"
        );
    }
}
