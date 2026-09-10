//! Cross-server parity for the three job reads: `GET /api/v4/jobs`,
//! `GET /api/v4/jobs/{job_id}` and `GET /api/v4/jobs/type/{job_type}`.
//!
//! ```sh
//! scripts/parity.sh -p mm-api --test parity jobs
//! ```
//!
//! # What this suite is really pinning
//!
//! `null` versus `[]` on an empty page. The two list routes go through *different* store methods
//! whose only difference is a slice initialiser, and the difference survives to the wire. Nothing
//! in the type system can catch it, so it is asserted directly, byte for byte, on the same
//! database with the same zero rows.
//!
//! # The `Jobs` table is written by the Go server while this runs
//!
//! The extract-content and product-notices workers insert a row every few minutes, so the
//! *unfiltered* page moves under both servers. Reads that can churn go through
//! [`common::fetch_both_stable`]; reads pinned to a type no worker touches (`data_retention`,
//! `access_control_sync`) are compared directly, because they are empty on this deployment and
//! stay that way.

use crate::common;

use common::{
    ACTIVE_LICENCE_ROW, RUST, client, create_plain_user, create_team, fetch_both_raw,
    fetch_both_stable, go_minted_token, stack_enabled,
};

/// An id in the right charset that no `Jobs` row has.
const NOWHERE: &str = "zzzzzzzzzzzzzzzzzzzzzzzzzz";

/// A job type with **no** permission case in `SessionHasPermissionToReadJob` — and therefore the
/// 400, not the 403. In `AllJobTypes`? No; that is the point of the second one below.
const UNDEFINED_TYPE: &str = "bogus_type";

/// A type that *is* in `AllJobTypes` yet has no permission arm: `scheduled_recap`. Being in the
/// validator's list gets it past `IsValidJobType` and straight into the same 400.
const VALID_BUT_UNREADABLE_TYPE: &str = "scheduled_recap";

async fn assert_same(client: &reqwest::Client, token: &str, path: &str) -> Vec<u8> {
    let ((go_status, go), (rs_status, rs)) = fetch_both_raw(client, token, path).await;
    assert_eq!(rs_status, go_status, "{path}");
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{path}"
    );
    go
}

/// Every response we serve carries the marker; a forwarded one does not. Without this a suite can
/// pass while the proxy answers every request.
async fn assert_served_by_rust(client: &reqwest::Client, token: &str, path: &str) {
    let response = client
        .get(format!("{RUST}{path}"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("reachable");
    assert_eq!(
        response
            .headers()
            .get("x-mmrs-served-by")
            .and_then(|v| v.to_str().ok()),
        Some("rust"),
        "{path} was forwarded, so this suite would be comparing Go with Go"
    );
}

/// **The finding this suite exists for.** An empty `getJobs` page is the four bytes `null`; an
/// empty `getJobsByType` page is `[]`. Same admin, same database, same zero `data_retention` rows.
#[tokio::test]
async fn an_empty_page_is_null_on_one_route_and_empty_on_the_other() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;

    let list = assert_same(&client, &token, "/api/v4/jobs?job_type=data_retention").await;
    assert_eq!(list, b"null", "`var jobs []*model.Job` marshals as null");

    let by_type = assert_same(&client, &token, "/api/v4/jobs/type/data_retention").await;
    assert_eq!(
        by_type, b"[]",
        "`statuses := []*model.Job{{}}` marshals as []"
    );

    assert_served_by_rust(&client, &token, "/api/v4/jobs?job_type=data_retention").await;
    assert_served_by_rust(&client, &token, "/api/v4/jobs/type/data_retention").await;
}

/// The **third** initialiser: adding `status` switches `getJobs` to `GetAllByTypesAndStatusesPage`,
/// whose slice is non-nil, so the *same route* answers `[]` where it answered `null` a line ago.
#[tokio::test]
async fn a_status_filter_turns_getjobs_null_into_empty() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;

    let without = assert_same(&client, &token, "/api/v4/jobs?job_type=data_retention").await;
    let with = assert_same(
        &client,
        &token,
        "/api/v4/jobs?job_type=data_retention&status=success",
    )
    .await;

    assert_eq!(without, b"null");
    assert_eq!(with, b"[]");
}

/// The populated page, and the one route whose body ends in a newline.
#[tokio::test]
async fn a_populated_page_matches_byte_for_byte() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;

    // `migrations` jobs are written once, at first boot, and never again — a stable page.
    let body = assert_same(&client, &token, "/api/v4/jobs/type/migrations").await;
    let jobs: serde_json::Value = serde_json::from_slice(&body).expect("an array");
    let jobs = jobs.as_array().expect("an array");
    assert!(!jobs.is_empty(), "the migrations job runs at first boot");
    assert!(
        !body.ends_with(b"\n"),
        "`json.Marshal` + `w.Write` — no newline on the list routes"
    );

    // Nine fields, none omitted, and `data` present even when it is null.
    let first = jobs[0].as_object().expect("an object");
    assert_eq!(first.len(), 9, "model.Job has no omitempty: {jobs:?}");
    for key in [
        "id",
        "type",
        "priority",
        "create_at",
        "start_at",
        "last_activity_at",
        "status",
        "progress",
        "data",
    ] {
        assert!(first.contains_key(key), "{key} is on the wire");
    }

    // The single-job route, on an id we just learned, *does* end in a newline.
    let id = first["id"].as_str().expect("an id");
    let one = assert_same(&client, &token, &format!("/api/v4/jobs/{id}")).await;
    assert!(
        one.ends_with(b"\n"),
        "`json.NewEncoder(w).Encode` writes a newline"
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&one).expect("decodes"),
        jobs[0],
        "the same row through both routes"
    );
    assert_served_by_rust(&client, &token, &format!("/api/v4/jobs/{id}")).await;
}

/// A `data` column holding a JSON object and one holding SQL/JSON `null` are both on the wire, and
/// they are different documents. The product-notices worker writes the null.
#[tokio::test]
async fn the_data_column_round_trips_as_an_object_and_as_null() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;
    // The null shape is planted rather than waited for: see `plant_null_data_job`. Without it this
    // test asserts nothing on any database younger than the product-notices worker's first tick,
    // which is every freshly created stack.
    common::plant_null_data_job("nulldata").await;

    let (go, rs) = fetch_both_stable(&client, &token, "/api/v4/jobs?per_page=200").await;
    assert_eq!(String::from_utf8_lossy(&go), String::from_utf8_lossy(&rs));

    let jobs: serde_json::Value = serde_json::from_slice(&rs).expect("decodes");
    let jobs = jobs.as_array().cloned().unwrap_or_default();
    assert!(!jobs.is_empty(), "this deployment has run jobs");

    let has_null = jobs.iter().any(|j| j["data"].is_null());
    let has_object = jobs.iter().any(|j| j["data"].is_object());
    common::unplant_jobs().await;

    assert!(
        has_null && has_object,
        "both shapes must appear or this test proves nothing: {jobs:?}"
    );
}

/// An unknown job type is a **400**, not a 403 — Go's `permissionRequired == nil` branch. Asserted
/// on both spellings, because one is rejected by `IsValidJobType` on `getJobs` and the other is
/// not, and they still converge on the same answer through `getJobsByType`.
#[tokio::test]
async fn a_type_with_no_permission_arm_is_a_400() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;

    for job_type in [UNDEFINED_TYPE, VALID_BUT_UNREADABLE_TYPE] {
        let path = format!("/api/v4/jobs/type/{job_type}");
        let ((go_status, go), (rs_status, rs)) = fetch_both_raw(&client, &token, &path).await;
        assert_eq!(rs_status, go_status, "{path}");
        assert_eq!(go_status, 400, "{path}");
        let parsed = common::assert_error_bodies_match_except_known_gaps(&go, &rs, &path);
        assert_eq!(parsed["id"], "api.job.retrieve.nopermissions", "{path}");
        assert_served_by_rust(&client, &token, &path).await;
    }

    // On `getJobs` the *invalid* type is caught a step earlier, by `IsValidJobType`, and gets the
    // URL-param 400 instead — a different id for the same string.
    let path = format!("/api/v4/jobs?job_type={UNDEFINED_TYPE}");
    let ((go_status, go), (rs_status, rs)) = fetch_both_raw(&client, &token, &path).await;
    assert_eq!(rs_status, go_status);
    assert_eq!(go_status, 400);
    let parsed = common::assert_error_bodies_match_except_known_gaps(&go, &rs, &path);
    assert_eq!(parsed["id"], "api.context.invalid_url_param.app_error");

    // …while the *valid but unreadable* one gets past the validator and lands on the other 400.
    let path = format!("/api/v4/jobs?job_type={VALID_BUT_UNREADABLE_TYPE}");
    let ((go_status, go), (rs_status, rs)) = fetch_both_raw(&client, &token, &path).await;
    assert_eq!(rs_status, go_status);
    assert_eq!(go_status, 400);
    let parsed = common::assert_error_bodies_match_except_known_gaps(&go, &rs, &path);
    assert_eq!(parsed["id"], "api.job.retrieve.nopermissions");
}

/// A bad `status` is its own 400 with its own id, and it is checked **after** the job-type
/// permission block — so a caller refused on type never sees it.
#[tokio::test]
async fn an_invalid_status_is_its_own_error_and_comes_second() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;

    let path = "/api/v4/jobs?status=bogus";
    let ((go_status, go), (rs_status, rs)) = fetch_both_raw(&client, &token, path).await;
    assert_eq!(rs_status, go_status);
    assert_eq!(go_status, 400);
    let parsed = common::assert_error_bodies_match_except_known_gaps(&go, &rs, path);
    assert_eq!(parsed["id"], "api.job.status.invalid");

    // Both wrong: the *type* error wins, which pins the order.
    let path = format!("/api/v4/jobs?job_type={UNDEFINED_TYPE}&status=bogus");
    let ((_, go), (_, rs)) = fetch_both_raw(&client, &token, &path).await;
    let parsed = common::assert_error_bodies_match_except_known_gaps(&go, &rs, &path);
    assert_eq!(
        parsed["id"], "api.context.invalid_url_param.app_error",
        "the job_type check runs before the status check"
    );
}

/// `getJob` on an id that does not exist: 404, and the id is the one `GetJob` builds — not a
/// store-shaped one.
#[tokio::test]
async fn a_missing_job_is_a_404() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;

    let path = format!("/api/v4/jobs/{NOWHERE}");
    let ((go_status, go), (rs_status, rs)) = fetch_both_raw(&client, &token, &path).await;
    assert_eq!(rs_status, go_status);
    assert_eq!(go_status, 404);
    let parsed = common::assert_error_bodies_match_except_known_gaps(&go, &rs, &path);
    assert_eq!(parsed["id"], "app.job.get.app_error");

    // A segment that is in the mux charset but is not an id: `RequireJobId`'s 400, and it is the
    // **URL**-param id, not the body one.
    let path = "/api/v4/jobs/short";
    let ((go_status, go), (rs_status, rs)) = fetch_both_raw(&client, &token, path).await;
    assert_eq!(rs_status, go_status);
    assert_eq!(go_status, 400);
    let parsed = common::assert_error_bodies_match_except_known_gaps(&go, &rs, path);
    assert_eq!(parsed["id"], "api.context.invalid_url_param.app_error");
}

/// A segment outside `{job_type:[A-Za-z0-9_-]+}` never matched Go's route, so it must be
/// forwarded and answered by Go's own mux 404 — not by us.
///
/// Sent by hand rather than through [`common::fetch_both_raw`], which asserts the opposite: that
/// the Rust server answered. Here being forwarded *is* the property under test.
#[tokio::test]
async fn a_job_type_outside_the_mux_charset_is_forwarded() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;

    let get = async |base: &str, path: &str| {
        let response = client
            .get(format!("{base}{path}"))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .expect("reachable");
        let status = response.status().as_u16();
        let served_by = response
            .headers()
            .get("x-mmrs-served-by")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        (
            status,
            served_by,
            response.bytes().await.expect("reads").to_vec(),
        )
    };

    // A dot is in the *username* class and not in the job-type one — the near-miss that would
    // pass a charset copied from the wrong neighbour.
    for segment in ["data.retention", "data~retention"] {
        let path = format!("/api/v4/jobs/type/{segment}");
        let (go_status, _, go) = get(common::GO, &path).await;
        let (rs_status, served_by, rs) = get(RUST, &path).await;

        assert_eq!(
            served_by.as_deref(),
            Some("go"),
            "{path}: the charset miss must reach Go, not our 400"
        );
        assert_eq!(go_status, 404, "gorilla's NotFoundHandler");
        assert_eq!(rs_status, go_status, "{path}");
        // Byte-identical, not "identical except request_id": a forwarded response *is* Go's,
        // handed back unaltered, and gorilla's mux 404 carries no request id to differ on.
        assert_eq!(
            String::from_utf8_lossy(&go),
            String::from_utf8_lossy(&rs),
            "{path}"
        );
    }
}

/// A plain user may read **no** job type, so `validJobTypes` is empty and Go answers a 403 that
/// names no permission at all — a body no other branch on this route produces.
#[tokio::test]
async fn a_plain_user_gets_a_403_naming_no_permission() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let admin = go_minted_token(&client).await;
    let team = create_team(&client, &admin, "jobs").await;
    let user = create_plain_user(&client, &admin, &team, "jobs").await;

    let path = "/api/v4/jobs";
    let ((go_status, go), (rs_status, rs)) = fetch_both_raw(&client, &user.token, path).await;
    assert_eq!(rs_status, go_status, "{path}");
    assert_eq!(go_status, 403, "{path}");
    let parsed = common::assert_error_bodies_match_except_known_gaps(&go, &rs, path);
    assert_eq!(parsed["id"], "api.context.permissions.app_error");
    assert_served_by_rust(&client, &user.token, path).await;

    // The per-type 403 is the *same* id from a different branch — reached only for a type that
    // has a permission arm, which `data_retention` does.
    let path = "/api/v4/jobs/type/data_retention";
    let ((go_status, go), (rs_status, rs)) = fetch_both_raw(&client, &user.token, path).await;
    assert_eq!(rs_status, go_status, "{path}");
    assert_eq!(go_status, 403, "{path}");
    common::assert_error_bodies_match_except_known_gaps(&go, &rs, path);

    common::delete_plain_user(&client, &admin, &user.id).await;
}

/// Pagination, on the one type whose rows are stable. A page past the end is `[]` on
/// `getJobsByType` and `null` on `getJobs` — the initialiser distinction again, at the far end.
#[tokio::test]
async fn pagination_runs_out_the_way_each_route_does() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;

    let first = assert_same(&client, &token, "/api/v4/jobs/type/migrations?per_page=1").await;
    let decoded: serde_json::Value = serde_json::from_slice(&first).expect("decodes");
    assert_eq!(decoded.as_array().map(Vec::len), Some(1), "one per page");

    let past = assert_same(
        &client,
        &token,
        "/api/v4/jobs/type/migrations?page=99&per_page=1",
    )
    .await;
    assert_eq!(past, b"[]");

    let past_list = assert_same(&client, &token, "/api/v4/jobs?page=99").await;
    assert_eq!(past_list, b"null");
}

/// `access_control_sync` is readable only by `manage_system`, so an admin gets an empty `[]` and
/// a plain user gets the 403 — the two access-control arms of the permission table, exercised
/// without a licence.
#[tokio::test]
async fn the_access_control_types_use_manage_system() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;

    let body = assert_same(&client, &token, "/api/v4/jobs/type/access_control_sync").await;
    assert_eq!(body, b"[]");

    // `policy_id` on a sync type takes the system-admin branch, which the admin passes.
    let scoped = assert_same(
        &client,
        &token,
        &format!("/api/v4/jobs/type/access_control_sync?policy_id={NOWHERE}"),
    )
    .await;
    assert_eq!(scoped, b"[]", "the in-memory page of no rows is []");

    // A `team_id` that is not an id is the URL-param 400, checked before any grant.
    let path = "/api/v4/jobs/type/access_control_sync?team_id=nope";
    let ((go_status, go), (rs_status, rs)) = fetch_both_raw(&client, &token, path).await;
    assert_eq!(rs_status, go_status, "{path}");
    assert_eq!(go_status, 400);
    let parsed = common::assert_error_bodies_match_except_known_gaps(&go, &rs, path);
    assert_eq!(parsed["id"], "api.context.invalid_url_param.app_error");
}

/// **A survivor's fixture.** `data_retention` maps to `read_data_retention_job`, *not* to the
/// generic `read_jobs` — and on a stock server every role holding one holds the other, so the
/// admin and the plain user both give the same answer either way. A mutation swapping the two
/// permissions therefore survived the whole suite.
///
/// The branch is not unreachable, only unreachable from a *stock* role: a planted role granting
/// `read_jobs` alone separates them. It reads `migrations` and is refused `data_retention`, which
/// is exactly the pair the mutation would collapse.
#[tokio::test]
async fn read_jobs_does_not_open_the_types_with_their_own_permission() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let admin = go_minted_token(&client).await;
    let team = create_team(&client, &admin, "jobsperm").await;

    let Some(role) = common::plant_role("jobsperm", "read_jobs").await else {
        return; // no DATABASE_URL — the planted-role fixtures cannot be built
    };
    let user = create_plain_user(&client, &admin, &team, "jobsperm").await;
    common::set_user_roles(&user.id, &format!("system_user {role}")).await;
    let token = common::login_plain_user(&client, "jobsperm").await;

    // `read_jobs` opens the thirteen types that share it.
    let body = assert_same(&client, &token, "/api/v4/jobs/type/migrations").await;
    let jobs: serde_json::Value = serde_json::from_slice(&body).expect("an array");
    assert!(
        !jobs.as_array().expect("an array").is_empty(),
        "`read_jobs` admits `migrations`"
    );

    // …and closes the five that have a permission of their own.
    for job_type in ["data_retention", "message_export", "ldap_sync"] {
        let path = format!("/api/v4/jobs/type/{job_type}");
        let ((go_status, go), (rs_status, rs)) = fetch_both_raw(&client, &token, &path).await;
        assert_eq!(rs_status, go_status, "{path}");
        assert_eq!(
            go_status, 403,
            "{path}: `read_jobs` is not `read_{job_type}_job`"
        );
        common::assert_error_bodies_match_except_known_gaps(&go, &rs, &path);
    }

    // The same split through `getJobs`: the unfiltered page is built from the readable types
    // only, so this caller gets a page and is still refused the narrowed one.
    let path = "/api/v4/jobs?job_type=data_retention";
    let ((go_status, go), (rs_status, rs)) = fetch_both_raw(&client, &token, path).await;
    assert_eq!(rs_status, go_status, "{path}");
    assert_eq!(go_status, 403, "{path}");
    common::assert_error_bodies_match_except_known_gaps(&go, &rs, path);

    common::delete_plain_user(&client, &admin, &user.id).await;
}
