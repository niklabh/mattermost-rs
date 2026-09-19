//! Cross-server parity for the **miscellaneous local-mode families** — the thirty-one
//! `*_local.go` pairs `mm_api::local_misc` registers — over the two unix sockets.
//!
//! See `local_mode.rs` for the transport and the authentication model. What this file adds:
//!
//! - **Writes land in the shared database, so each server writes its own row.** A preference
//!   is written for one synthetic user through Go's socket and for another through ours, and
//!   the two answers are compared with the user id normalised — the same operation on
//!   equivalent rows, never a second write to one row. Jobs and bots are planted the same way,
//!   one per server. Everything is prefixed `mmrs…` and swept at both ends of its test.
//! - **`me` is nobody**, on five pairs, and the test shows Go's socket says so too.
//! - **The forwards go over the socket**, and the assertion is the *status* as well as the
//!   body: a forward over the port would come back 401 where Go's local mux answers 404.
//! - **`localGetConfig` shows the secrets.** The body is compared to Go's *unmasked*, which is
//!   the one thing reusing the HTTP handler would have got wrong.

use super::super::common;
use super::super::common::local_socket::{
    assert_forwarded_body_is_gos, both, both_maybe_forwarded, both_with_body, go_socket,
    rust_socket, sockets_enabled,
};

/// The two synthetic users the preference writes target: a valid id, no `Users` row (Go needs
/// none for a preference), one per server. 26 characters, in Go's id alphabet.
const GO_PREF_USER: &str = "mmrsmisclocalgo00000000000";
const RUST_PREF_USER: &str = "mmrsmisclocalrs00000000000";
/// A category no other suite writes.
const PREF_CATEGORY: &str = "mmrs_misc_local";

/// One request with an optional body over one socket, returning `(status, headers, body)`.
async fn send(
    socket: &std::path::Path,
    method: &str,
    path: &str,
    body: Option<&str>,
) -> (u16, axum::http::HeaderMap, Vec<u8>) {
    let mut builder = axum::http::Request::builder()
        .method(method)
        .uri(path)
        .header("Host", "localhost");
    let body = match body {
        Some(body) => {
            builder = builder
                .header("Content-Type", "application/json")
                .header("Content-Length", body.len().to_string());
            axum::body::Body::from(body.to_owned())
        }
        None => axum::body::Body::empty(),
    };
    let request = builder.body(body).expect("request builds");
    let response = mm_api::local::send_over_unix(socket, request)
        .await
        .unwrap_or_else(|e| panic!("{method} {path} over {}: {e}", socket.display()));
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body reads")
        .to_vec();
    (status, headers, body)
}

fn served_here(headers: &axum::http::HeaderMap) -> bool {
    headers
        .get("x-mmrs-served-by")
        .and_then(|v| v.to_str().ok())
        == Some("rust")
}

/// A **different** request to each socket — Go's row through Go, ours through ours — asserting
/// ours was answered here. Returns `((go_status, go_body), (rust_status, rust_body))`.
async fn each(
    method: &str,
    go_path: &str,
    rust_path: &str,
    go_body: Option<&str>,
    rust_body: Option<&str>,
) -> ((u16, Vec<u8>), (u16, Vec<u8>)) {
    let go = go_socket().expect("checked by sockets_enabled");
    let rust = rust_socket().expect("checked by sockets_enabled");
    let (go_status, _, go_bytes) = send(&go, method, go_path, go_body).await;
    let (rs_status, rs_headers, rs_bytes) = send(&rust, method, rust_path, rust_body).await;
    assert!(
        served_here(&rs_headers),
        "{method} {rust_path} was forwarded to Go over the socket"
    );
    ((go_status, go_bytes), (rs_status, rs_bytes))
}

fn json(bytes: &[u8]) -> serde_json::Value {
    serde_json::from_slice(bytes)
        .unwrap_or_else(|e| panic!("not JSON: {e}: {}", String::from_utf8_lossy(bytes)))
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

async fn sweep_preferences() {
    let Some(pool) = common::fixture_pool().await else {
        return;
    };
    let _ = sqlx::query("DELETE FROM preferences WHERE category = $1")
        .bind(PREF_CATEGORY)
        .execute(&pool)
        .await;
}

// ---------------------------------------------------------------------------------------------
// config_local.go — the two handlers that are not the HTTP ones
// ---------------------------------------------------------------------------------------------

/// `localGetConfig` is `c.App.Config()` unsanitized: the socket sees the database password
/// where the HTTP route writes asterisks. Byte-for-byte parity apart from the one per-checkout
/// key, the encoder's newline, `Cache-Control`, and `?remove_masked=` forwarded over the socket.
#[tokio::test]
async fn the_local_config_matches_and_is_not_sanitized() {
    if !sockets_enabled() {
        return;
    }
    // Shared: `parity::configlic` patches the document under the exclusive half.
    let _document = common::CONFIG_DOCUMENT.read().await;
    let ((go_status, go_body), (rs_status, rs_body)) = both("GET", "/api/v4/config").await;
    assert_eq!(go_status, 200);
    assert_eq!(rs_status, 200);
    assert!(
        go_body.ends_with(b"\n") && rs_body.ends_with(b"\n"),
        "json.NewEncoder"
    );

    let mut go = json(&go_body);
    let mut rust = json(&rs_body);
    // Two keys are paths rooted at whichever checkout launched each server —
    // `FileSettings.Directory`, the exemption `config_reads` makes for the HTTP route, and
    // `ServiceSettings.LocalModeSocketLocation`, which the HTTP route never compares because the
    // value only differs between a worktree's mm-api and the main checkout's Go. Same socket,
    // through a symlink; a different string.
    for (section, key) in [
        ("FileSettings", "Directory"),
        ("ServiceSettings", "LocalModeSocketLocation"),
    ] {
        for document in [&mut go, &mut rust] {
            document[section]
                .as_object_mut()
                .expect(section)
                .remove(key)
                .unwrap_or_else(|| panic!("{section}.{key} is in the document"));
        }
    }
    assert_eq!(go, rust, "the unsanitized configuration");

    for (section, key) in [
        ("SqlSettings", "DataSource"),
        ("SqlSettings", "AtRestEncryptKey"),
        ("FileSettings", "PublicLinkSalt"),
    ] {
        let value = rust[section][key].as_str().expect("a string");
        assert_ne!(
            value, "********************************",
            "{section}.{key}: localGetConfig does not sanitize"
        );
    }
    assert!(
        text(&rs_body).contains("mmuser_password"),
        "the socket sees the database password, as Go's does"
    );

    let rust = rust_socket().expect("checked");
    let (_, headers, _) = send(&rust, "GET", "/api/v4/config", None).await;
    assert_eq!(
        headers.get("Cache-Control").and_then(|v| v.to_str().ok()),
        Some("no-cache, no-store, must-revalidate")
    );

    // `?remove_masked=` is `FilterConfig` proper, forwarded — over the socket, so Go's own
    // local handler answers it rather than `APISessionRequired` refusing it.
    for query in ["remove_masked=true", "remove_defaults=1"] {
        let path = format!("/api/v4/config?{query}");
        let ((go_status, go_body), (rs_status, rs_body), served) =
            both_maybe_forwarded("GET", &path).await;
        assert!(!served, "{path} must be forwarded");
        assert_eq!(go_status, 200, "{path}");
        assert_eq!(rs_status, go_status, "{path}");
        assert_eq!(json(&go_body), json(&rs_body), "{path}: Go's own answer");
    }
    // An unparseable value is `false` to `strconv.ParseBool` and is served.
    let ((_, _), (_, _), served) =
        both_maybe_forwarded("GET", "/api/v4/config?remove_masked=maybe").await;
    assert!(served, "an unparseable flag is not a filter");
}

/// `localGetClientConfig` is the **full** map with no session test and `w.Write` rather than an
/// encoder — no trailing newline, where the HTTP route's body ends in one.
#[tokio::test]
async fn the_local_client_config_is_the_full_map_without_a_newline() {
    if !sockets_enabled() {
        return;
    }
    let ((go_status, go_body), (rs_status, rs_body)) = both("GET", "/api/v4/config/client").await;
    assert_eq!(go_status, 200);
    assert_eq!(rs_status, 200);
    assert_eq!(text(&go_body), text(&rs_body));
    assert!(!go_body.ends_with(b"\n"), "MapToJSON + Write: no newline");
    let map = json(&rs_body);
    // A key the limited map does not carry: the full map is what the socket gets.
    assert!(
        map.get("SiteURL").is_some() && map.get("EnableCustomEmoji").is_some(),
        "the full client configuration, not the anonymous one"
    );
    let rust = rust_socket().expect("checked");
    let (_, headers, _) = send(&rust, "GET", "/api/v4/config/client", None).await;
    assert!(
        headers.get("Cache-Control").is_none(),
        "no Cache-Control here"
    );
}

// ---------------------------------------------------------------------------------------------
// preference_local.go — five pairs, one write per server
// ---------------------------------------------------------------------------------------------

/// The preference round trip — write, three reads, delete, read again — for a synthetic user
/// per server, plus the refusals, `me`, a `flagged_post` batch, and the one forward (a category
/// outside the mux class).
#[tokio::test]
async fn the_preference_pairs_match_over_the_socket() {
    if !sockets_enabled() {
        return;
    }
    sweep_preferences().await;

    let go_batch = format!(
        r#"[{{"user_id":"{GO_PREF_USER}","category":"{PREF_CATEGORY}","name":"name_one","value":"b"}}]"#
    );
    let rs_batch = format!(
        r#"[{{"user_id":"{RUST_PREF_USER}","category":"{PREF_CATEGORY}","name":"name_one","value":"b"}}]"#
    );
    let go_path = format!("/api/v4/users/{GO_PREF_USER}/preferences");
    let rs_path = format!("/api/v4/users/{RUST_PREF_USER}/preferences");

    // The write: a `PUT` the HTTP router serves only as `/users/me/preferences`.
    let ((go_status, go_body), (rs_status, rs_body)) =
        each("PUT", &go_path, &rs_path, Some(&go_batch), Some(&rs_batch)).await;
    assert_eq!(go_status, 200, "{}", text(&go_body));
    assert_eq!(rs_status, 200, "{}", text(&rs_body));
    assert_eq!(
        text(&go_body),
        text(&rs_body),
        "status OK, without a newline"
    );

    // The three reads, with the user id normalised out.
    let normalise = |bytes: &[u8]| {
        text(bytes)
            .replace(GO_PREF_USER, "USER")
            .replace(RUST_PREF_USER, "USER")
    };
    for suffix in [
        "",
        &format!("/{PREF_CATEGORY}"),
        &format!("/{PREF_CATEGORY}/name/name_one"),
    ] {
        let ((go_status, go_body), (rs_status, rs_body)) = each(
            "GET",
            &format!("{go_path}{suffix}"),
            &format!("{rs_path}{suffix}"),
            None,
            None,
        )
        .await;
        assert_eq!(go_status, 200, "GET …{suffix}: {}", text(&go_body));
        assert_eq!(rs_status, 200, "GET …{suffix}");
        assert_eq!(normalise(&go_body), normalise(&rs_body), "GET …{suffix}");
        assert!(go_body.ends_with(b"\n"), "GET …{suffix}: json.NewEncoder");
    }

    // A preference naming another user is `App.UpdatePreferences`'s 403 — the same request to
    // both, so nothing is written.
    let foreign = format!(
        r#"[{{"user_id":"zzzzzzzzzzzzzzzzzzzzzzzzzz","category":"{PREF_CATEGORY}","name":"c","value":"d"}}]"#
    );
    let ((go_status, go_body), (rs_status, rs_body)) =
        both_with_body("PUT", &rs_path, Box::leak(foreign.into_boxed_str())).await;
    assert_eq!(go_status, 403);
    assert_eq!(rs_status, 403);
    let body = common::assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "foreign");
    assert_eq!(
        body["id"],
        "api.preference.update_preferences.set.app_error"
    );

    // An empty batch is a 400, not a no-op.
    let ((go_status, go_body), (rs_status, rs_body)) = both_with_body("PUT", &rs_path, "[]").await;
    assert_eq!(go_status, 400);
    assert_eq!(rs_status, 400);
    common::assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "empty batch");

    // `me` is nobody on this transport: a 400 naming `user_id`, on the read and on the write.
    for (method, body) in [
        ("GET", None),
        ("PUT", Some(r#"[{"category":"x","name":"y","value":"z"}]"#)),
    ] {
        let path = "/api/v4/users/me/preferences";
        let ((go_status, go_body), (rs_status, rs_body)) = match body {
            Some(body) => both_with_body(method, path, body).await,
            None => both(method, path).await,
        };
        assert_eq!(go_status, 400, "{method} me");
        assert_eq!(rs_status, 400, "{method} me");
        let body = common::assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, path);
        assert_eq!(
            body["id"], "api.context.invalid_url_param.app_error",
            "{method} me"
        );
    }

    // A `flagged_post` batch is served here since 2026-09-19: the post does not exist, so both
    // answer the 400 naming `preference.name` before writing anything.
    let flagged = format!(
        r#"[{{"user_id":"{RUST_PREF_USER}","category":"flagged_post","name":"zzzzzzzzzzzzzzzzzzzzzzzzzz","value":"true"}}]"#
    );
    let go = go_socket().expect("checked");
    let rust = rust_socket().expect("checked");
    let (go_status, _, go_body) = send(&go, "PUT", &rs_path, Some(&flagged)).await;
    let (rs_status, rs_headers, rs_body) = send(&rust, "PUT", &rs_path, Some(&flagged)).await;
    assert!(served_here(&rs_headers), "a flagged_post batch is ours");
    assert_eq!(go_status, 400);
    assert_eq!(rs_status, go_status);
    common::assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "flagged_post");

    // A category outside `[A-Za-z0-9_]+` is Go's mux 404, over the socket (a port forward would
    // be a 401 here); one inside it but not lower-case is our 400 naming `category`.
    let path = format!("{rs_path}/display-settings");
    let ((go_status, go_body), (rs_status, rs_body), served) =
        both_maybe_forwarded("GET", &path).await;
    assert!(!served, "{path} is forwarded");
    assert_eq!(go_status, 404, "{path}");
    assert_eq!(rs_status, go_status, "{path}");
    assert_forwarded_body_is_gos(&go_body, &rs_body, &path);
    let path = format!("{rs_path}/Display_Settings");
    let ((go_status, go_body), (rs_status, rs_body)) = both("GET", &path).await;
    assert_eq!(go_status, 400, "{path}");
    assert_eq!(rs_status, 400, "{path}");
    common::assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &path);

    // `RequirePreferenceName` is the format-strict pattern: a one-character name is inside the
    // mux class and still a 400 naming `preference_name`. (The first run of this file used `a`
    // as the name and learned this from Go.)
    let path = format!("{rs_path}/{PREF_CATEGORY}/name/a");
    let ((go_status, go_body), (rs_status, rs_body)) = both("GET", &path).await;
    assert_eq!(go_status, 400, "{path}");
    assert_eq!(rs_status, 400, "{path}");
    let body = common::assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &path);
    assert_eq!(body["id"], "api.context.invalid_url_param.app_error");

    // `GET …/preferences/delete` is `getPreferencesByCategory("delete")` — a 404, served here.
    let path = format!("{rs_path}/delete");
    let ((go_status, go_body), (rs_status, rs_body)) = both("GET", &path).await;
    assert_eq!(go_status, 404, "{path}");
    assert_eq!(rs_status, 404, "{path}");
    let body = common::assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &path);
    assert_eq!(
        body["id"],
        "api.preference.preferences_category.get.app_error"
    );

    // The delete, then the category read is a 404 on both.
    let ((go_status, go_body), (rs_status, rs_body)) = each(
        "POST",
        &format!("{go_path}/delete"),
        &format!("{rs_path}/delete"),
        Some(&go_batch),
        Some(&rs_batch),
    )
    .await;
    assert_eq!(go_status, 200, "{}", text(&go_body));
    assert_eq!(rs_status, 200, "{}", text(&rs_body));
    assert_eq!(text(&go_body), text(&rs_body));
    let ((go_status, _), (rs_status, _)) = each(
        "GET",
        &format!("{go_path}/{PREF_CATEGORY}"),
        &format!("{rs_path}/{PREF_CATEGORY}"),
        None,
        None,
    )
    .await;
    assert_eq!(go_status, 404, "deleted on Go's side");
    assert_eq!(rs_status, 404, "deleted on ours");

    sweep_preferences().await;
}

// ---------------------------------------------------------------------------------------------
// job_local.go — seven pairs
// ---------------------------------------------------------------------------------------------

/// Plant a `message_export` job — no worker on this build touches the type — in `status`.
async fn plant_job(tag: &str, status: &str) -> Option<String> {
    let pool = common::fixture_pool().await?;
    let id = format!("mmrsjob{tag:0>19}");
    sqlx::query("DELETE FROM jobs WHERE id = $1")
        .bind(&id)
        .execute(&pool)
        .await
        .ok()?;
    sqlx::query(
        "INSERT INTO jobs (id, type, priority, createat, startat, lastactivityat, status, progress, data)
         VALUES ($1, 'message_export', 0, 1788600000000, 0, 1788600000000, $2, 0, '{}'::jsonb)",
    )
    .bind(&id)
    .bind(status)
    .execute(&pool)
    .await
    .ok()?;
    Some(id)
}

async fn unplant_jobs(ids: &[&str]) {
    let Some(pool) = common::fixture_pool().await else {
        return;
    };
    for id in ids {
        let _ = sqlx::query("DELETE FROM jobs WHERE id = $1")
            .bind(id)
            .execute(&pool)
            .await;
    }
}

/// The job routes: a create per server, a read, a cancel, a status write and its refusal, the
/// download gate, and the three refusals — with the two mux forwards over the socket.
#[tokio::test]
async fn the_job_pairs_match_over_the_socket() {
    if !sockets_enabled() {
        return;
    }
    let _unlicensed = common::ACTIVE_LICENCE_ROW.read().await;

    // `createJob`: `active_users` is a type the admin may create, and the local session may
    // create anything — a 201 with the job, newline-terminated, ids and times normalised.
    let mut created = Vec::new();
    let mut created_ids = Vec::new();
    for socket in [
        go_socket().expect("checked"),
        rust_socket().expect("checked"),
    ] {
        let (status, _, body) = send(
            &socket,
            "POST",
            "/api/v4/jobs",
            Some(r#"{"type":"active_users"}"#),
        )
        .await;
        assert_eq!(status, 201, "{}", text(&body));
        assert!(body.ends_with(b"\n"), "json.NewEncoder");
        let mut job = json(&body);
        created_ids.push(job["id"].as_str().expect("an id").to_owned());
        job["id"] = serde_json::json!("");
        job["create_at"] = serde_json::json!(0);
        created.push(job);
    }
    assert_eq!(created[0], created[1], "the created job, normalised");
    assert_eq!(created[1]["status"], "pending");
    let owned: Vec<&str> = created_ids.iter().map(String::as_str).collect();
    unplant_jobs(&owned).await;

    for (body, expected, id) in [
        (
            r#"{"type":"no_such_thing"}"#,
            400,
            "api.job.unable_to_create_job.incorrect_job_type",
        ),
        (
            r#"{"type":"data_retention"}"#,
            400,
            "model.job.is_valid.type.app_error",
        ),
        ("[]", 400, "api.context.invalid_body_param.app_error"),
        (
            "null",
            400,
            "api.job.unable_to_create_job.incorrect_job_type",
        ),
    ] {
        let ((go_status, go_body), (rs_status, rs_body)) =
            both_with_body("POST", "/api/v4/jobs", body).await;
        assert_eq!(go_status, expected, "{body}: {}", text(&go_body));
        assert_eq!(rs_status, expected, "{body}: {}", text(&rs_body));
        let answer = common::assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, body);
        assert_eq!(answer["id"], id, "{body}");
    }

    // One planted pending job per server for the cancel, and one for the status write.
    let (Some(go_cancel), Some(rs_cancel), Some(go_status_job), Some(rs_status_job)) = (
        plant_job("misccango", "pending").await,
        plant_job("misccanrs", "pending").await,
        plant_job("miscstago", "pending").await,
        plant_job("miscstars", "pending").await,
    ) else {
        return;
    };
    let normalise = |bytes: &[u8]| {
        let mut job = json(bytes);
        job["id"] = serde_json::json!("");
        job["last_activity_at"] = serde_json::json!(0);
        job
    };

    // `getJob`, and the download gate, on the planted rows.
    let ((go_status, go_body), (rs_status, rs_body)) = each(
        "GET",
        &format!("/api/v4/jobs/{go_cancel}"),
        &format!("/api/v4/jobs/{rs_cancel}"),
        None,
        None,
    )
    .await;
    assert_eq!(go_status, 200, "{}", text(&go_body));
    assert_eq!(rs_status, 200, "{}", text(&rs_body));
    assert_eq!(normalise(&go_body), normalise(&rs_body), "getJob");
    let path = format!("/api/v4/jobs/{rs_cancel}/download");
    let ((go_status, go_body), (rs_status, rs_body)) = both("GET", &path).await;
    assert_eq!(go_status, 501, "{path}");
    assert_eq!(rs_status, 501, "{path}");
    let body = common::assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &path);
    assert_eq!(body["id"], "app.job.download_export_results_not_enabled");

    // `cancelJob` on a pending job, then the row as each server reports it.
    let ((go_status, go_body), (rs_status, rs_body)) = each(
        "POST",
        &format!("/api/v4/jobs/{go_cancel}/cancel"),
        &format!("/api/v4/jobs/{rs_cancel}/cancel"),
        None,
        None,
    )
    .await;
    assert_eq!(go_status, 200, "{}", text(&go_body));
    assert_eq!(rs_status, 200, "{}", text(&rs_body));
    assert_eq!(text(&go_body), text(&rs_body));
    let ((_, go_body), (_, rs_body)) = each(
        "GET",
        &format!("/api/v4/jobs/{go_cancel}"),
        &format!("/api/v4/jobs/{rs_cancel}"),
        None,
        None,
    )
    .await;
    assert_eq!(normalise(&go_body), normalise(&rs_body), "after the cancel");
    assert_eq!(json(&rs_body)["status"], "canceled");

    // `updateJobStatus`: `pending → canceled` is not a valid change (400, nothing written);
    // `pending → cancel_requested` is.
    let path = format!("/api/v4/jobs/{rs_status_job}/status");
    let ((go_status, go_body), (rs_status, rs_body)) =
        both_with_body("PATCH", &path, r#"{"status":"canceled"}"#).await;
    assert_eq!(go_status, 400, "{path}: {}", text(&go_body));
    assert_eq!(rs_status, 400, "{path}: {}", text(&rs_body));
    let body = common::assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &path);
    assert_eq!(body["id"], "api.job.status.invalid");
    let ((go_status, go_body), (rs_status, rs_body)) = each(
        "PATCH",
        &format!("/api/v4/jobs/{go_status_job}/status"),
        &path,
        Some(r#"{"status":"cancel_requested"}"#),
        Some(r#"{"status":"cancel_requested"}"#),
    )
    .await;
    assert_eq!(go_status, 200, "{}", text(&go_body));
    assert_eq!(rs_status, 200, "{}", text(&rs_body));
    assert_eq!(text(&go_body), text(&rs_body));
    let ((_, go_body), (_, rs_body)) = each(
        "GET",
        &format!("/api/v4/jobs/{go_status_job}"),
        &format!("/api/v4/jobs/{rs_status_job}"),
        None,
        None,
    )
    .await;
    assert_eq!(
        normalise(&go_body),
        normalise(&rs_body),
        "after the status write"
    );

    unplant_jobs(&[&go_cancel, &rs_cancel, &go_status_job, &rs_status_job]).await;

    // The misses: a well-formed id nobody has, and the two mux forwards over the socket.
    for (method, path, expected, id) in [
        (
            "GET",
            "/api/v4/jobs/zzzzzzzzzzzzzzzzzzzzzzzzzz",
            404,
            "app.job.get.app_error",
        ),
        (
            "POST",
            "/api/v4/jobs/zzzzzzzzzzzzzzzzzzzzzzzzzz/cancel",
            404,
            "app.job.get.app_error",
        ),
    ] {
        let ((go_status, go_body), (rs_status, rs_body)) = both(method, path).await;
        assert_eq!(go_status, expected, "{method} {path}");
        assert_eq!(rs_status, expected, "{method} {path}");
        let body = common::assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, path);
        assert_eq!(body["id"], id, "{method} {path}");
    }
    for path in ["/api/v4/jobs/bad-id", "/api/v4/jobs/type/bad.type"] {
        let ((go_status, go_body), (rs_status, rs_body), served) =
            both_maybe_forwarded("GET", path).await;
        assert!(!served, "{path} is forwarded");
        assert_eq!(go_status, 404, "{path}: Go's mux 404");
        assert_eq!(rs_status, go_status, "{path}");
        assert_forwarded_body_is_gos(&go_body, &rs_body, path);
    }

    // `getJobsByType` for a type with no rows is `[]` and `getJobs?job_type=` for the same is
    // `null` — two spellings of nothing. `ldap_sync` has a read-permission entry (the local
    // session passes it) and no scheduler on this build, so it stays empty; a type with **no**
    // entry is the 400 `api.job.retrieve.nopermissions` even for root — measured, on the first
    // run of this file, with `cleanup_desktop_tokens`.
    for (path, expected) in [
        ("/api/v4/jobs/type/ldap_sync?per_page=1", "[]"),
        ("/api/v4/jobs?job_type=ldap_sync&per_page=1", "null"),
    ] {
        let ((go_status, go_body), (rs_status, rs_body)) = both("GET", path).await;
        assert_eq!(go_status, 200, "{path}: {}", text(&go_body));
        assert_eq!(rs_status, 200, "{path}: {}", text(&rs_body));
        assert_eq!(text(&go_body), text(&rs_body), "{path}");
        assert_eq!(text(&go_body).trim_end(), expected, "{path}");
    }
    let path = "/api/v4/jobs/type/cleanup_desktop_tokens";
    let ((go_status, go_body), (rs_status, rs_body)) = both("GET", path).await;
    assert_eq!(go_status, 400, "{path}");
    assert_eq!(rs_status, 400, "{path}");
    let body = common::assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, path);
    assert_eq!(body["id"], "api.job.retrieve.nopermissions");
}

// ---------------------------------------------------------------------------------------------
// custom_profile_attributes_local.go — seven pairs, unlicensed
// ---------------------------------------------------------------------------------------------

/// The seven CPA pairs on an unlicensed server: the two reads that answer, the five refusals,
/// and `me` — which on `PATCH /custom_profile_attributes/values` is an **empty target** that
/// Go never reaches because the fields are read first.
#[tokio::test]
async fn the_cpa_pairs_match_over_the_socket() {
    if !sockets_enabled() {
        return;
    }
    let _rows = common::PROPERTY_ROWS.lock().await;
    let _unlicensed = common::ACTIVE_LICENCE_ROW.read().await;
    let client = common::client();
    let _token = common::go_minted_token(&client).await;
    let admin = common::logged_in_user_id();

    for (method, path, body, expected) in [
        (
            "GET",
            "/api/v4/custom_profile_attributes/fields".to_owned(),
            None,
            200,
        ),
        (
            "POST",
            "/api/v4/custom_profile_attributes/fields".to_owned(),
            Some(r#"{"name":"mmrsmisclocal","type":"text"}"#),
            403,
        ),
        (
            "PATCH",
            "/api/v4/custom_profile_attributes/fields/zzzzzzzzzzzzzzzzzzzzzzzzzz".to_owned(),
            Some(r#"{"name":"x"}"#),
            404,
        ),
        (
            "DELETE",
            "/api/v4/custom_profile_attributes/fields/zzzzzzzzzzzzzzzzzzzzzzzzzz".to_owned(),
            None,
            404,
        ),
        (
            "GET",
            format!("/api/v4/users/{admin}/custom_profile_attributes"),
            None,
            200,
        ),
        (
            "GET",
            "/api/v4/users/me/custom_profile_attributes".to_owned(),
            None,
            400,
        ),
        (
            "PATCH",
            "/api/v4/custom_profile_attributes/values".to_owned(),
            Some("{}"),
            400,
        ),
        (
            "PATCH",
            "/api/v4/custom_profile_attributes/values".to_owned(),
            Some(r#"{"zzzzzzzzzzzzzzzzzzzzzzzzzz":"v"}"#),
            404,
        ),
        (
            "PATCH",
            format!("/api/v4/users/{admin}/custom_profile_attributes"),
            Some(r#"{"zzzzzzzzzzzzzzzzzzzzzzzzzz":"v"}"#),
            404,
        ),
        (
            "PATCH",
            format!("/api/v4/users/{admin}/custom_profile_attributes"),
            Some("[]"),
            400,
        ),
        (
            "PATCH",
            "/api/v4/users/me/custom_profile_attributes".to_owned(),
            Some(r#"{"zzzzzzzzzzzzzzzzzzzzzzzzzz":"v"}"#),
            400,
        ),
    ] {
        let ((go_status, go_body), (rs_status, rs_body)) = match body {
            Some(body) => both_with_body(method, &path, body).await,
            None => both(method, &path).await,
        };
        assert_eq!(go_status, expected, "{method} {path}: {}", text(&go_body));
        assert_eq!(rs_status, expected, "{method} {path}: {}", text(&rs_body));
        if expected == 200 {
            assert_eq!(text(&go_body), text(&rs_body), "{method} {path}");
        } else {
            common::assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &path);
        }
    }

    let path = "/api/v4/users/bad-id/custom_profile_attributes";
    let ((go_status, go_body), (rs_status, rs_body), served) =
        both_maybe_forwarded("GET", path).await;
    assert!(!served, "{path} is forwarded");
    assert_eq!(go_status, 404);
    assert_eq!(rs_status, go_status);
    assert_forwarded_body_is_gos(&go_body, &rs_body, path);
}

// ---------------------------------------------------------------------------------------------
// export_local.go, import_local.go, upload_local.go
// ---------------------------------------------------------------------------------------------

/// The six archive pairs, and `getUpload` on a real session and on two misses.
#[tokio::test]
async fn the_export_import_and_upload_pairs_match_over_the_socket() {
    if !sockets_enabled() {
        return;
    }
    let ((go_status, go_body), (rs_status, rs_body)) = both("GET", "/api/v4/exports").await;
    assert_eq!(go_status, 200);
    assert_eq!(rs_status, 200);
    assert_eq!(text(&go_body), text(&rs_body));
    assert!(!go_body.ends_with(b"\n"), "listExports is Marshal + Write");
    let ((go_status, go_body), (rs_status, rs_body)) = both("GET", "/api/v4/imports").await;
    assert_eq!(go_status, 200);
    assert_eq!(rs_status, 200);
    assert_eq!(text(&go_body), text(&rs_body));
    assert!(go_body.ends_with(b"\n"), "listImports is Encoder.Encode");

    for (method, path, expected, id) in [
        (
            "GET",
            "/api/v4/exports/mmrs-not-here.zip",
            404,
            "api.export.export_not_found.app_error",
        ),
        (
            "POST",
            "/api/v4/exports/mmrs-not-here.zip/presign-url",
            500,
            "app.eport.generate_presigned_url.featureflag.app_error",
        ),
        (
            "GET",
            "/api/v4/uploads/mmrsupload0000000000000abc",
            404,
            "app.upload.get.app_error",
        ),
        (
            "GET",
            "/api/v4/uploads/short",
            400,
            "api.context.invalid_url_param.app_error",
        ),
    ] {
        let ((go_status, go_body), (rs_status, rs_body)) = both(method, path).await;
        assert_eq!(go_status, expected, "{method} {path}: {}", text(&go_body));
        assert_eq!(rs_status, expected, "{method} {path}: {}", text(&rs_body));
        let body = common::assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, path);
        assert_eq!(body["id"], id, "{method} {path}");
    }
    // Deleting an archive that is not there is a 200 on both — `RemoveFile` of nothing is fine.
    for path in [
        "/api/v4/exports/mmrs-not-here.zip",
        "/api/v4/imports/mmrs-not-here.zip",
    ] {
        let ((go_status, go_body), (rs_status, rs_body)) = both("DELETE", path).await;
        assert_eq!(go_status, 200, "{path}: {}", text(&go_body));
        assert_eq!(rs_status, 200, "{path}: {}", text(&rs_body));
        assert_eq!(text(&go_body), text(&rs_body), "{path}");
    }
    // A name outside `.+\.zip` is Go's mux 404, over the socket.
    let path = "/api/v4/exports/.zip";
    let ((go_status, go_body), (rs_status, rs_body), served) =
        both_maybe_forwarded("GET", path).await;
    assert!(!served, "{path} is forwarded");
    assert_eq!(go_status, 404);
    assert_eq!(rs_status, go_status);
    assert_forwarded_body_is_gos(&go_body, &rs_body, path);

    // A real upload session, created through Go's HTTP API (the `POST` is another family's on
    // the socket), read through both sockets: the local session owns nothing, and reads it
    // anyway.
    let client = common::client();
    let token = common::go_minted_token(&client).await;
    let channel = common::a_channel_the_user_is_in(&client, &token).await;
    let response = client
        .post(format!("{}/api/v4/uploads", common::GO))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!({
            "channel_id": channel,
            "filename": "mmrs-misc-local.txt",
            "file_size": 12,
        }))
        .send()
        .await
        .expect("Go answers");
    assert_eq!(response.status(), 201, "creating the upload session");
    let upload: serde_json::Value = response.json().await.expect("the session decodes");
    let upload_id = upload["id"].as_str().expect("an id").to_owned();

    let path = format!("/api/v4/uploads/{upload_id}");
    let ((go_status, go_body), (rs_status, rs_body)) = both("GET", &path).await;
    assert_eq!(go_status, 200, "{}", text(&go_body));
    assert_eq!(rs_status, 200, "{}", text(&rs_body));
    assert_eq!(text(&go_body), text(&rs_body), "getUpload");
    assert!(go_body.ends_with(b"\n"), "getUpload is Encoder.Encode");

    if let Some(pool) = common::fixture_pool().await {
        let _ = sqlx::query("DELETE FROM uploadsessions WHERE id = $1")
            .bind(&upload_id)
            .execute(&pool)
            .await;
    }
}

// ---------------------------------------------------------------------------------------------
// the singletons: convert_to_user, support_packet, ldap/groups
// ---------------------------------------------------------------------------------------------

/// `convertBotToUser` on a planted bot per server — the answer is the user, with the tag and
/// `update_at` normalised — and on a bot nobody has; `generateSupportPacket` and
/// `getLdapGroups` up to their licence gates.
#[tokio::test]
async fn the_singleton_pairs_match_over_the_socket() {
    if !sockets_enabled() {
        return;
    }
    let _unlicensed = common::ACTIVE_LICENCE_ROW.read().await;

    for (path, expected, id) in [
        ("/api/v4/system/support_packet", 403, "api.no_license"),
        ("/api/v4/ldap/groups", 501, "api.ldap_groups.license_error"),
        (
            "/api/v4/bots/zzzzzzzzzzzzzzzzzzzzzzzzzz/convert_to_user",
            404,
            "store.sql_bot.get.missing.app_error",
        ),
    ] {
        let method = if path.ends_with("convert_to_user") {
            "POST"
        } else {
            "GET"
        };
        let ((go_status, go_body), (rs_status, rs_body)) = both(method, path).await;
        assert_eq!(go_status, expected, "{method} {path}: {}", text(&go_body));
        assert_eq!(rs_status, expected, "{method} {path}: {}", text(&rs_body));
        let body = common::assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, path);
        assert_eq!(body["id"], id, "{method} {path}");
    }
    let path = "/api/v4/bots/bad-id/convert_to_user";
    let ((go_status, go_body), (rs_status, rs_body), served) =
        both_maybe_forwarded("POST", path).await;
    assert!(!served, "{path} is forwarded");
    assert_eq!(go_status, 404);
    assert_eq!(rs_status, go_status);
    assert_forwarded_body_is_gos(&go_body, &rs_body, path);

    // The conversion proper. `mmrsbot%` is one fixture and `unplant_bots` sweeps all of it, so
    // this shares `parity/bots`' lock rather than racing it.
    let _bots = common::BOT_FIXTURES.lock().await;
    let client = common::client();
    let token = common::go_minted_token(&client).await;
    let owner = common::logged_in_user_id();
    let (Some(theirs), Some(mine)) = (
        common::plant_bot("misccvgo", owner, 0).await,
        common::plant_bot("misccvrs", owner, 0).await,
    ) else {
        return;
    };
    common::invalidate_go_caches(&client, &token).await;

    // The body is a `UserPatch` whose `password` must be set — `{}` and no body alike are the
    // 400 naming `userPatch`, after the bot lookup.
    let patch = r#"{"password":"Mmrs-Convert-1234"}"#;
    let ((go_status, go_body), (rs_status, rs_body)) = each(
        "POST",
        &format!("/api/v4/bots/{theirs}/convert_to_user"),
        &format!("/api/v4/bots/{mine}/convert_to_user"),
        Some(patch),
        Some(patch),
    )
    .await;
    assert_eq!(go_status, 200, "{}", text(&go_body));
    assert_eq!(rs_status, 200, "{}", text(&rs_body));
    let normalise = |bytes: &[u8]| {
        let mut user = json(bytes);
        user["update_at"] = serde_json::json!(0);
        serde_json::to_string(&user)
            .expect("re-encodes")
            .replace("misccvgo", "TAG")
            .replace("misccvrs", "TAG")
    };
    assert_eq!(
        normalise(&go_body),
        normalise(&rs_body),
        "the converted user"
    );
    assert!(go_body.ends_with(b"\n"), "json.NewEncoder");
    assert!(rs_body.ends_with(b"\n"), "json.NewEncoder");

    // Converted, so no longer a bot: a second conversion is the 404 on both.
    let ((go_status, go_body), (rs_status, rs_body)) = each(
        "POST",
        &format!("/api/v4/bots/{theirs}/convert_to_user"),
        &format!("/api/v4/bots/{mine}/convert_to_user"),
        None,
        None,
    )
    .await;
    assert_eq!(go_status, 404);
    assert_eq!(rs_status, 404);
    common::assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "second conversion");

    common::unplant_bots().await;
}

// ---------------------------------------------------------------------------------------------
// the neighbours
// ---------------------------------------------------------------------------------------------

/// Every route the local router served before this family still answers here: the new
/// registrations sit beside `/users/{user_id}/status`, `/bots/{bot_user_id}/…`, `/system/…`
/// and the roles, and a literal segment beside a `{param}` can un-serve a route.
#[tokio::test]
async fn the_neighbouring_local_routes_still_answer() {
    if !sockets_enabled() {
        return;
    }
    let _unlicensed = common::ACTIVE_LICENCE_ROW.read().await;
    let _busy = common::BUSY_STATE.read().await;
    let client = common::client();
    let _token = common::go_minted_token(&client).await;
    let admin = common::logged_in_user_id();

    for path in [
        "/api/v4/system/ping".to_owned(),
        "/api/v4/system/schema/version".to_owned(),
        "/api/v4/server_busy".to_owned(),
        "/api/v4/license/client?format=old".to_owned(),
        "/api/v4/bots".to_owned(),
        format!("/api/v4/users/{admin}/status"),
        "/api/v4/roles".to_owned(),
        "/api/v4/roles/name/system_user".to_owned(),
    ] {
        let ((go_status, _), (rs_status, _), served) = both_maybe_forwarded("GET", &path).await;
        assert!(served, "{path} must still be answered here");
        assert_eq!(rs_status, go_status, "{path}");
        assert_eq!(go_status, 200, "{path}");
    }
}
