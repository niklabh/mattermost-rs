//! Cross-server parity for the three job writes — `POST /api/v4/jobs`,
//! `POST /api/v4/jobs/{job_id}/cancel` and `PATCH /api/v4/jobs/{job_id}/status`.
//!
//! ```sh
//! scripts/parity.sh --test parity job_writes
//! ```
//!
//! # Planted rows, on purpose
//!
//! The Go server's workers poll the shared `Jobs` table for `pending` rows of every type they
//! know, so a row created through the API is picked up within seconds — which makes a created
//! job a poor subject for the cancel and status tests. Those plant rows of a type this build has
//! **no worker for** (`message_export`: the enterprise interface is nil) in the status they
//! need, and only the create test lets a real worker have its job. Not `data_retention`, which
//! is the type `parity::jobs` counts on being absent.
//!
//! `job_updated` carries `ContainsSensitiveData`, so the admin's own socket is where it lands.

use std::time::Duration;

use crate::common;

use common::{
    BROADCAST_STREAM, GO, RUST, SocketProbe, assert_error_bodies_match_except_known_gaps, client,
    create_plain_user, create_team, fixture_pool, go_minted_token, stack_enabled,
};

async fn post(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    path: &str,
    body: Option<&str>,
) -> (u16, bool, Vec<u8>) {
    let mut request = client
        .post(format!("{base}{path}"))
        .header("Authorization", format!("Bearer {token}"));
    if let Some(body) = body {
        request = request
            .header("Content-Type", "application/json")
            .body(body.to_owned());
    }
    finish(base, request).await
}

async fn patch(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    path: &str,
    body: &str,
) -> (u16, bool, Vec<u8>) {
    let request = client
        .patch(format!("{base}{path}"))
        .header("Authorization", format!("Bearer {token}"))
        .header("Content-Type", "application/json")
        .body(body.to_owned());
    finish(base, request).await
}

async fn finish(base: &str, request: reqwest::RequestBuilder) -> (u16, bool, Vec<u8>) {
    let response = request
        .send()
        .await
        .unwrap_or_else(|e| panic!("{base} is unreachable: {e}"));
    let status = response.status().as_u16();
    let served = response
        .headers()
        .get("x-mmrs-served-by")
        .and_then(|v| v.to_str().ok())
        == Some("rust");
    (
        status,
        served,
        response
            .bytes()
            .await
            .map(|b| b.to_vec())
            .unwrap_or_default(),
    )
}

fn json(body: &[u8]) -> serde_json::Value {
    serde_json::from_slice(body).unwrap_or(serde_json::Value::Null)
}

/// The two servers' error bodies, minus the fields that legitimately differ.
fn comparable_error(body: &[u8]) -> serde_json::Value {
    let mut value = json(body);
    if let Some(obj) = value.as_object_mut() {
        obj.remove("request_id");
        obj.remove("message");
        obj.remove("detailed_error");
    }
    value
}

/// Plant a `message_export` job — a type no worker on this build will touch — in `status`.
async fn plant_job(tag: &str, status: &str) -> String {
    plant_job_of(tag, "message_export", status).await
}

/// Plant a job of `job_type` in `status`. A registered type is only safe in a status its
/// worker does not poll for — anything but `pending`.
async fn plant_job_of(tag: &str, job_type: &str, status: &str) -> String {
    let pool = fixture_pool().await.expect("DATABASE_URL");
    let id = format!("mmrsjob{tag:0>19}");
    sqlx::query("DELETE FROM jobs WHERE id = $1")
        .bind(&id)
        .execute(&pool)
        .await
        .expect("swept");
    sqlx::query(
        "INSERT INTO jobs (id, type, priority, createat, startat, lastactivityat, status, progress, data)
         VALUES ($1, $3, 0, 1788600000000, 0, 1788600000000, $2, 0, '{}'::jsonb)",
    )
    .bind(&id)
    .bind(status)
    .bind(job_type)
    .execute(&pool)
    .await
    .expect("planted");
    id
}

/// `(status, lastactivityat, startat)` of a job row.
async fn job_row(id: &str) -> (String, i64, i64) {
    let pool = fixture_pool().await.expect("DATABASE_URL");
    sqlx::query_as("SELECT status, lastactivityat, startat FROM jobs WHERE id = $1")
        .bind(id)
        .fetch_one(&pool)
        .await
        .expect("the job row")
}

/// The `job_updated` frame naming `job_id`, or none within the window.
async fn job_updated_for(socket: &mut SocketProbe, job_id: &str) -> Option<serde_json::Value> {
    let wanted = job_id.to_owned();
    let found = move |frames: &[serde_json::Value]| {
        frames.iter().any(|f| {
            f["event"] == "job_updated"
                && f["data"]["job"]
                    .as_str()
                    .is_some_and(|j| j.contains(&wanted))
        })
    };
    if !socket
        .collect_until(Duration::from_millis(2500), found)
        .await
    {
        return None;
    }
    let frame = socket.events_named("job_updated").into_iter().find(|f| {
        f["data"]["job"]
            .as_str()
            .is_some_and(|j| j.contains(job_id))
    })?;
    serde_json::from_str(frame["data"]["job"].as_str()?).ok()
}

/// A registered type the admin may create (`active_users`, `manage_jobs`) is a 201 with a
/// `pending` job and a `null` data; the body is `json.NewEncoder`'s. A registered type nobody
/// may create is the 400 `incorrect_job_type`; a permitted type with no worker on this build is
/// the 400 `model.job.is_valid.type.app_error`; a plain user is the 403.
#[tokio::test]
async fn creating_a_job_follows_the_permission_matrix_and_the_worker_set() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let team = create_team(&client, &token, "jobw").await;
    let user = create_plain_user(&client, &token, &team, "jobw").await;

    let mut created = Vec::new();
    for base in [GO, RUST] {
        let (status, served, body) = post(
            &client,
            base,
            &token,
            "/api/v4/jobs",
            Some(r#"{"type":"active_users"}"#),
        )
        .await;
        assert_eq!(status, 201, "{base}: {}", String::from_utf8_lossy(&body));
        assert_eq!(served, base == RUST, "{base}");
        assert!(
            body.ends_with(b"\n"),
            "{base}: json.NewEncoder writes a newline"
        );
        let job = json(&body);
        assert_eq!(job["type"], "active_users", "{base}");
        assert_eq!(job["status"], "pending", "{base}: {job}");
        assert_eq!(
            job["data"],
            serde_json::Value::Null,
            "{base}: no data is null"
        );
        assert_eq!(job["progress"], 0, "{base}");
        assert_eq!(job["start_at"], 0, "{base}");
        assert!(job["create_at"].as_i64().unwrap_or(0) > 0, "{base}");
        let id = job["id"].as_str().expect("an id").to_owned();
        // The Go worker may already have it; only the type is stable.
        let (row_status, _, _) = job_row(&id).await;
        assert!(
            ["pending", "in_progress", "success"].contains(&row_status.as_str()),
            "{base}: {row_status}"
        );
        let mut normalised = job.clone();
        normalised["id"] = serde_json::json!("");
        normalised["create_at"] = serde_json::json!(0);
        created.push(normalised);
    }
    assert_eq!(created[0], created[1]);

    for (label, body, expected, id) in [
        (
            "no permission entry",
            r#"{"type":"cleanup_desktop_tokens"}"#,
            400,
            "api.job.unable_to_create_job.incorrect_job_type",
        ),
        (
            "no worker on this build",
            r#"{"type":"data_retention"}"#,
            400,
            "model.job.is_valid.type.app_error",
        ),
        (
            "unknown type",
            r#"{"type":"no_such_thing"}"#,
            400,
            "api.job.unable_to_create_job.incorrect_job_type",
        ),
    ] {
        let mut bodies = Vec::new();
        for base in [GO, RUST] {
            let (status, served, answer) =
                post(&client, base, &token, "/api/v4/jobs", Some(body)).await;
            assert_eq!(
                status,
                expected,
                "{base} {label}: {}",
                String::from_utf8_lossy(&answer)
            );
            assert_eq!(served, base == RUST, "{base} {label}");
            assert_eq!(json(&answer)["id"], id, "{base} {label}");
            bodies.push(comparable_error(&answer));
        }
        assert_eq!(bodies[0], bodies[1], "{label}");
    }

    let mut bodies = Vec::new();
    for base in [GO, RUST] {
        let (status, served, answer) = post(
            &client,
            base,
            &user.token,
            "/api/v4/jobs",
            Some(r#"{"type":"migrations"}"#),
        )
        .await;
        assert_eq!(status, 403, "{base}: {}", String::from_utf8_lossy(&answer));
        assert_eq!(served, base == RUST, "{base}");
        bodies.push(answer);
    }
    assert_error_bodies_match_except_known_gaps(&bodies[0], &bodies[1], "plain user");
}

/// Cancelling: a `pending` job is `canceled` outright and an `in_progress` one becomes
/// `cancel_requested`, each with a `job_updated` frame on the admin's socket carrying the new
/// status — and **not** on a plain user's, since the event carries `ContainsSensitiveData`;
/// a finished job is the 500 `jobs.request_cancellation.status.error`.
#[tokio::test]
async fn cancelling_moves_pending_to_canceled_and_in_progress_to_cancel_requested() {
    if !stack_enabled() {
        return;
    }
    let _broadcast = BROADCAST_STREAM.lock().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let team = create_team(&client, &token, "jobc").await;
    let bystander = create_plain_user(&client, &token, &team, "jobc").await;

    for (base, tag) in [(GO, "cg"), (RUST, "cr")] {
        for (from, to, tag_from) in [
            ("pending", "canceled", "pend"),
            ("in_progress", "cancel_requested", "prog"),
        ] {
            // The id has to pass the mux class `[A-Za-z0-9]+`: no underscore in the tag.
            let id = plant_job(&format!("{tag}{tag_from}"), from).await;
            let (_, before_activity, before_start) = job_row(&id).await;
            let mut socket = SocketProbe::connect(base, &token).await;
            let mut bystander_socket = SocketProbe::connect(base, &bystander.token).await;
            let (status, served, body) = post(
                &client,
                base,
                &token,
                &format!("/api/v4/jobs/{id}/cancel"),
                None,
            )
            .await;
            assert_eq!(
                status,
                200,
                "{base} {from}: {}",
                String::from_utf8_lossy(&body)
            );
            assert_eq!(served, base == RUST, "{base} {from}");
            assert_eq!(body, br#"{"status":"OK"}"#, "{base} {from}");
            let (row_status, activity, start) = job_row(&id).await;
            assert_eq!(row_status, to, "{base} {from}");
            assert!(
                activity > before_activity,
                "{base} {from}: LastActivityAt stamped"
            );
            assert_eq!(start, before_start, "{base} {from}: StartAt untouched");
            let event = job_updated_for(&mut socket, &id)
                .await
                .unwrap_or_else(|| panic!("{base} {from}: no job_updated frame: {:?}", socket.raw));
            assert_eq!(event["status"], to, "{base} {from}: {event}");
            assert_eq!(event["id"], id.as_str(), "{base} {from}");
            assert!(
                job_updated_for(&mut bystander_socket, &id).await.is_none(),
                "{base} {from}: a sensitive event stays with manage_system: {:?}",
                bystander_socket.raw
            );
        }

        let id = plant_job(&format!("{tag}done"), "success").await;
        let (status, served, body) = post(
            &client,
            base,
            &token,
            &format!("/api/v4/jobs/{id}/cancel"),
            None,
        )
        .await;
        assert_eq!(status, 500, "{base}: {}", String::from_utf8_lossy(&body));
        assert_eq!(served, base == RUST, "{base}");
        assert_eq!(
            json(&body)["id"],
            "jobs.request_cancellation.status.error",
            "{base}"
        );
        assert_eq!(job_row(&id).await.0, "success", "{base}: untouched");
    }
}

/// Status changes: `in_progress` → `pending` is valid and written with its frame; the same
/// target from `success` is the 400 `api.job.status.invalid` without `force`, and with `force`
/// it is written; a target outside the three settable statuses is the 500
/// `app.job.update_status.app_error` even when forced; a missing `status` is the 400 param.
#[tokio::test]
async fn updating_status_checks_the_transition_unless_forced() {
    if !stack_enabled() {
        return;
    }
    let _broadcast = BROADCAST_STREAM.lock().await;
    let client = client();
    let token = go_minted_token(&client).await;

    for (base, tag) in [(GO, "sg"), (RUST, "sr")] {
        let id = plant_job(&format!("{tag}prog"), "in_progress").await;
        let mut socket = SocketProbe::connect(base, &token).await;
        let (status, served, body) = patch(
            &client,
            base,
            &token,
            &format!("/api/v4/jobs/{id}/status"),
            r#"{"status":"pending"}"#,
        )
        .await;
        assert_eq!(status, 200, "{base}: {}", String::from_utf8_lossy(&body));
        assert_eq!(served, base == RUST, "{base}");
        assert_eq!(job_row(&id).await.0, "pending", "{base}");
        let event = job_updated_for(&mut socket, &id)
            .await
            .unwrap_or_else(|| panic!("{base}: no job_updated frame: {:?}", socket.raw));
        assert_eq!(event["status"], "pending", "{base}: {event}");

        let done = plant_job(&format!("{tag}done"), "success").await;
        let (status, served, body) = patch(
            &client,
            base,
            &token,
            &format!("/api/v4/jobs/{done}/status"),
            r#"{"status":"pending"}"#,
        )
        .await;
        assert_eq!(status, 400, "{base}: {}", String::from_utf8_lossy(&body));
        assert_eq!(served, base == RUST, "{base}");
        assert_eq!(json(&body)["id"], "api.job.status.invalid", "{base}");
        assert_eq!(job_row(&done).await.0, "success", "{base}");

        let (status, served, body) = patch(
            &client,
            base,
            &token,
            &format!("/api/v4/jobs/{done}/status"),
            r#"{"status":"pending","force":true}"#,
        )
        .await;
        assert_eq!(
            status,
            200,
            "{base} forced: {}",
            String::from_utf8_lossy(&body)
        );
        assert_eq!(served, base == RUST, "{base}");
        assert_eq!(job_row(&done).await.0, "pending", "{base}: forced through");

        let (status, served, body) = patch(
            &client,
            base,
            &token,
            &format!("/api/v4/jobs/{done}/status"),
            r#"{"status":"success","force":true}"#,
        )
        .await;
        assert_eq!(
            status,
            500,
            "{base} unsettable: {}",
            String::from_utf8_lossy(&body)
        );
        assert_eq!(served, base == RUST, "{base}");
        assert_eq!(
            json(&body)["id"],
            "app.job.update_status.app_error",
            "{base}"
        );

        let (status, served, body) = patch(
            &client,
            base,
            &token,
            &format!("/api/v4/jobs/{done}/status"),
            r#"{"force":true}"#,
        )
        .await;
        assert_eq!(
            status,
            400,
            "{base} no status: {}",
            String::from_utf8_lossy(&body)
        );
        assert_eq!(served, base == RUST, "{base}");
        assert_eq!(
            json(&body)["id"],
            "api.context.invalid_body_param.app_error",
            "{base}"
        );
    }
}

/// Cancel and status use different matrices: on a `message_export` job a plain user lacks
/// `create_compliance_export_job` (cancel) and `manage_compliance_export_job` (status). The
/// permission each names goes to the log, not the wire — `detailed_error` is blanked outside
/// developer mode — so the 403s are compared whole; an unknown job is the 404 on both routes.
#[tokio::test]
async fn the_two_routes_name_their_own_permissions_and_miss_alike() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let team = create_team(&client, &token, "jobp").await;
    let user = create_plain_user(&client, &token, &team, "jobp").await;
    let id = plant_job("perm", "in_progress").await;

    let mut bodies = Vec::new();
    for base in [GO, RUST] {
        let (status, served, body) = post(
            &client,
            base,
            &user.token,
            &format!("/api/v4/jobs/{id}/cancel"),
            None,
        )
        .await;
        assert_eq!(status, 403, "{base}: {}", String::from_utf8_lossy(&body));
        assert_eq!(served, base == RUST, "{base}");
        assert_eq!(
            json(&body)["id"],
            "api.context.permissions.app_error",
            "{base}"
        );
        bodies.push(body);
    }
    assert_error_bodies_match_except_known_gaps(&bodies[0], &bodies[1], "cancel 403");

    let mut bodies = Vec::new();
    for base in [GO, RUST] {
        let (status, served, body) = patch(
            &client,
            base,
            &user.token,
            &format!("/api/v4/jobs/{id}/status"),
            r#"{"status":"pending"}"#,
        )
        .await;
        assert_eq!(status, 403, "{base}: {}", String::from_utf8_lossy(&body));
        assert_eq!(served, base == RUST, "{base}");
        assert_eq!(
            json(&body)["id"],
            "api.context.permissions.app_error",
            "{base}"
        );
        bodies.push(body);
    }
    assert_error_bodies_match_except_known_gaps(&bodies[0], &bodies[1], "status 403");

    for (path, is_patch) in [
        ("/api/v4/jobs/zzzzzzzzzzzzzzzzzzzzzzzzzz/cancel", false),
        ("/api/v4/jobs/zzzzzzzzzzzzzzzzzzzzzzzzzz/status", true),
    ] {
        let mut bodies = Vec::new();
        for base in [GO, RUST] {
            let (status, served, body) = if is_patch {
                patch(&client, base, &token, path, r#"{"status":"pending"}"#).await
            } else {
                post(&client, base, &token, path, None).await
            };
            assert_eq!(
                status,
                404,
                "{base} {path}: {}",
                String::from_utf8_lossy(&body)
            );
            assert_eq!(served, base == RUST, "{base} {path}");
            bodies.push(body);
        }
        assert_error_bodies_match_except_known_gaps(&bodies[0], &bodies[1], path);
    }
}

/// A job created with `data` keeps it, key for key, on both.
#[tokio::test]
async fn data_is_stored_and_echoed() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;

    let mut created = Vec::new();
    for base in [GO, RUST] {
        let (status, served, body) = post(
            &client,
            base,
            &token,
            "/api/v4/jobs",
            Some(r#"{"type":"active_users","data":{"note":"mmrs","b":"2"}}"#),
        )
        .await;
        assert_eq!(status, 201, "{base}: {}", String::from_utf8_lossy(&body));
        assert_eq!(served, base == RUST, "{base}");
        let job = json(&body);
        assert_eq!(
            job["data"],
            serde_json::json!({ "note": "mmrs", "b": "2" }),
            "{base}"
        );
        let mut normalised = job.clone();
        normalised["id"] = serde_json::json!("");
        normalised["create_at"] = serde_json::json!(0);
        created.push(normalised);
    }
    assert_eq!(created[0], created[1]);
}

/// The two matrices differ on `notify_expiring_access_tokens`: the create matrix (which cancel
/// uses) grants it under `manage_jobs`, the manage matrix has no entry — so the same job can be
/// cancelled and not have its status set.
#[tokio::test]
async fn cancel_and_status_use_different_matrices() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;

    for (base, tag) in [(GO, "mg"), (RUST, "mr")] {
        // `in_progress`, which no worker polls for, so the registered type is safe to plant.
        let id = plant_job_of(
            &format!("{tag}notify"),
            "notify_expiring_access_tokens",
            "in_progress",
        )
        .await;

        let (status, served, body) = patch(
            &client,
            base,
            &token,
            &format!("/api/v4/jobs/{id}/status"),
            r#"{"status":"pending"}"#,
        )
        .await;
        assert_eq!(status, 400, "{base}: {}", String::from_utf8_lossy(&body));
        assert_eq!(served, base == RUST, "{base}");
        assert_eq!(
            json(&body)["id"],
            "api.job.unable_to_manage_job.incorrect_job_type",
            "{base}"
        );
        assert_eq!(job_row(&id).await.0, "in_progress", "{base}: untouched");

        let (status, served, body) = post(
            &client,
            base,
            &token,
            &format!("/api/v4/jobs/{id}/cancel"),
            None,
        )
        .await;
        assert_eq!(status, 200, "{base}: {}", String::from_utf8_lossy(&body));
        assert_eq!(served, base == RUST, "{base}");
        assert_eq!(job_row(&id).await.0, "cancel_requested", "{base}");
    }
}
