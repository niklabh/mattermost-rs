//! Cross-server parity for the **system-operations family** — `api4/system.go`'s analytics,
//! cache, pool, log, restart and enterprise-upgrade routes, `api4/elasticsearch.go`'s two, and
//! the two `system_local.go` pairs on the unix socket. See `mm_api::sysops`.
//!
//! ```sh
//! scripts/parity.sh --test parity sysops
//! ```
//!
//! # What can be compared byte for byte, and what is a process's own
//!
//! - The five analytics reports are figures over the shared database and compare exactly —
//!   except three rows of `standard` (`total_websocket_connections`, the two `db_connections`)
//!   which each server reports about itself and which are compared by **name and position**
//!   only. Every report is read Go-us-Go, since other suites write posts and sessions under it.
//! - The log routes read the file `LogSettings.FileLocation` names, found from the working
//!   directory, and on a checkout where `reference/.build` is a symlink both servers refuse
//!   with the same 403 (see `mm_app::logs`); the tests assert the *pair* agrees and compare
//!   bodies where the answer is an error, so a stack that can read the file passes too.
//! - `POST /restart` is a one-second 200 that restarts nothing on either server; the test
//!   measures the second and pings both afterwards.
//! - The upgrade routes refuse on the architecture; the Elasticsearch routes end at the 501
//!   both servers give without an engine, after the body and password checks that precede it.
//! - `POST /caches/invalidate` is called under `GO_CACHE`: the Rust handler purges Go's caches
//!   too, and one credential test elsewhere asserts Go is stale.

use std::time::{Duration, Instant};

use crate::common;
use crate::common::local_socket::{both, both_with_body, sockets_enabled};
use common::{
    GO, GO_CACHE, RUST, assert_error_bodies_match_except_known_gaps, client, create_plain_user,
    delete_plain_user, go_minted_token, request_raw, stack_enabled,
};
use reqwest::Method;

/// The fixture user's first team.
async fn a_team(client: &reqwest::Client, token: &str) -> String {
    let teams: serde_json::Value = client
        .get(format!("{GO}/api/v4/users/me/teams"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("Go answers")
        .json()
        .await
        .expect("a team list");
    teams[0]["id"].as_str().expect("a team id").to_owned()
}

fn json(bytes: &[u8]) -> serde_json::Value {
    serde_json::from_slice(bytes)
        .unwrap_or_else(|e| panic!("not JSON: {e}: {}", String::from_utf8_lossy(bytes)))
}

/// Both servers, the same request, and the pair `(go, rust)` of `(status, body, served_by)`.
async fn pair(
    client: &reqwest::Client,
    method: Method,
    token: Option<&str>,
    path: &str,
    body: Option<&[u8]>,
) -> ((u16, Vec<u8>), (u16, Vec<u8>)) {
    let (go_status, go_body, _) = request_raw(client, GO, method.clone(), token, path, body).await;
    let (rs_status, rs_body, served) = request_raw(client, RUST, method, token, path, body).await;
    assert_eq!(
        served.as_deref(),
        Some("rust"),
        "{path} was forwarded, so this comparison proves nothing about the Rust handler"
    );
    ((go_status, go_body), (rs_status, rs_body))
}

/// A pair that must be the same error on both sides.
async fn assert_same_error(
    client: &reqwest::Client,
    method: Method,
    token: Option<&str>,
    path: &str,
    body: Option<&[u8]>,
    status: u16,
    id: &str,
) {
    let ((go_status, go_body), (rs_status, rs_body)) =
        pair(client, method, token, path, body).await;
    assert_eq!(
        go_status,
        status,
        "{path}: Go's status: {}",
        String::from_utf8_lossy(&go_body)
    );
    assert_eq!(
        rs_status,
        status,
        "{path}: our status: {}",
        String::from_utf8_lossy(&rs_body)
    );
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, path);
    assert_eq!(go["id"], id, "{path}");
}

/// The `standard` rows each process reports about itself, by position.
const PROCESS_ROWS: [usize; 3] = [5, 6, 7];

/// Read a report Go-us-Go and return the masked pair that agreed, or the last disagreement.
async fn analytics_report(
    client: &reqwest::Client,
    token: &str,
    path: &str,
    masked: &[usize],
) -> (serde_json::Value, serde_json::Value) {
    let get = async |base: &str| {
        let (status, body, served) =
            request_raw(client, base, Method::GET, Some(token), path, None).await;
        assert_eq!(
            status,
            200,
            "{base}{path}: {}",
            String::from_utf8_lossy(&body)
        );
        if base == RUST {
            assert_eq!(served.as_deref(), Some("rust"), "{path} was forwarded");
        }
        // `json.NewEncoder(w).Encode`: the body ends in a newline on both sides.
        assert!(body.ends_with(b"\n"), "{base}{path}: no trailing newline");
        let mut rows = json(&body);
        if let Some(list) = rows.as_array_mut() {
            for &i in masked {
                if let Some(row) = list.get_mut(i) {
                    row["value"] = serde_json::Value::Null;
                }
            }
        }
        rows
    };
    let mut last = None;
    for _ in 0..8 {
        let before = get(GO).await;
        let ours = get(RUST).await;
        let after = get(GO).await;
        if ours == before || ours == after || before == after {
            return (if ours == after { after } else { before }, ours);
        }
        last = Some((before, ours));
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    last.expect("the loop runs")
}

#[tokio::test]
async fn the_analytics_reports_match_except_the_process_rows() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let admin = go_minted_token(&client).await;
    let team = a_team(&client, &admin).await;

    // An archived team, so `team_count`'s `DeleteAt = 0` is a filter and not a no-op.
    let archived = common::create_team(&client, &admin, "sysopsarchived").await;
    let deleted = client
        .delete(format!("{GO}/api/v4/teams/{archived}"))
        .header("Authorization", format!("Bearer {admin}"))
        .send()
        .await
        .expect("Go answers");
    assert_eq!(deleted.status(), 200, "archiving the planted team");

    // `standard`, with and without a team, and under its default name.
    for path in [
        "/api/v4/analytics/old",
        "/api/v4/analytics/old?name=standard",
        "/api/v4/analytics/old?name=standard&team_id=",
    ] {
        let (go, rs) = analytics_report(&client, &admin, path, &PROCESS_ROWS).await;
        assert_eq!(go, rs, "{path}");
        let names: Vec<&str> = rs
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["name"].as_str().unwrap())
            .collect();
        assert_eq!(names[5], "total_websocket_connections");
        assert_eq!(names[6], "total_master_db_connections");
        assert_eq!(names[7], "total_read_db_connections");
        assert_eq!(rs[10]["name"], "inactive_user_count");
        assert_ne!(
            rs[10]["value"], -1,
            "{path}: no team, so the inactive count is real"
        );
    }
    let team_path = format!("/api/v4/analytics/old?name=standard&team_id={team}");
    let (go, rs) = analytics_report(&client, &admin, &team_path, &PROCESS_ROWS).await;
    assert_eq!(go, rs, "{team_path}");
    assert_eq!(
        rs[10]["value"], -1,
        "with a team the inactive count is the literal -1"
    );
    assert_eq!(rs[3]["name"], "unique_user_count");

    // The four other reports, byte-comparable (modulo churn) with and without a team.
    for name in [
        "post_counts_day",
        "bot_post_counts_day",
        "user_counts_with_posts_day",
    ] {
        for path in [
            format!("/api/v4/analytics/old?name={name}"),
            format!("/api/v4/analytics/old?name={name}&team_id={team}"),
        ] {
            let (go, rs) = analytics_report(&client, &admin, &path, &[]).await;
            assert_eq!(go, rs, "{path}");
        }
    }
    let (go, rs) = analytics_report(
        &client,
        &admin,
        "/api/v4/analytics/old?name=extra_counts",
        &[],
    )
    .await;
    assert_eq!(go, rs, "extra_counts");
    assert_eq!(rs[3]["name"], "session_count");

    // Go's `Where("TeamId", teamId)` is a broken statement: a 500 on both, for any team.
    for team_id in [team.as_str(), "zzz"] {
        assert_same_error(
            &client,
            Method::GET,
            Some(&admin),
            &format!("/api/v4/analytics/old?name=extra_counts&team_id={team_id}"),
            None,
            500,
            "app.webhooks.analytics_outgoing_count.app_error",
        )
        .await;
    }

    // An unknown report is a 400 blaming the *body* parameter `name`.
    assert_same_error(
        &client,
        Method::GET,
        Some(&admin),
        "/api/v4/analytics/old?name=bogus",
        None,
        400,
        "api.context.invalid_body_param.app_error",
    )
    .await;

    // The gate is `get_analytics`, which a plain member lacks — checked after the default name.
    let plain = create_plain_user(&client, &admin, &team, "sysopsan").await;
    assert_same_error(
        &client,
        Method::GET,
        Some(&plain.token),
        "/api/v4/analytics/old?name=bogus",
        None,
        403,
        "api.context.permissions.app_error",
    )
    .await;
    delete_plain_user(&client, &admin, &plain.id).await;
}

#[tokio::test]
async fn the_cache_and_pool_resets_answer_ok_and_refuse_a_member() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let admin = go_minted_token(&client).await;
    let team = a_team(&client, &admin).await;
    let plain = create_plain_user(&client, &admin, &team, "sysopsrs").await;

    for path in ["/api/v4/caches/invalidate", "/api/v4/database/recycle"] {
        assert_same_error(
            &client,
            Method::POST,
            Some(&plain.token),
            path,
            None,
            403,
            "api.context.permissions.app_error",
        )
        .await;
    }

    {
        // The Rust handler purges Go's caches as a side effect; `auth_writes` asserts Go is
        // stale, under this lock.
        let _go_cache = GO_CACHE.lock().await;
        let go = client
            .post(format!("{GO}/api/v4/caches/invalidate"))
            .header("Authorization", format!("Bearer {admin}"))
            .send()
            .await
            .expect("Go answers");
        let rs = client
            .post(format!("{RUST}/api/v4/caches/invalidate"))
            .header("Authorization", format!("Bearer {admin}"))
            .send()
            .await
            .expect("we answer");
        assert_eq!(
            rs.headers()
                .get("x-mmrs-served-by")
                .and_then(|v| v.to_str().ok()),
            Some("rust")
        );
        assert_eq!(
            (go.status(), rs.status()),
            (reqwest::StatusCode::OK, reqwest::StatusCode::OK)
        );
        assert_eq!(
            go.headers().get("cache-control"),
            rs.headers().get("cache-control"),
            "the no-store Cache-Control is part of the answer"
        );
        assert_eq!(
            rs.headers()
                .get("cache-control")
                .and_then(|v| v.to_str().ok()),
            Some("no-cache, no-store, must-revalidate")
        );
        assert_eq!(go.bytes().await.unwrap(), rs.bytes().await.unwrap());
    }

    let ((go_status, go_body), (rs_status, rs_body)) = pair(
        &client,
        Method::POST,
        Some(&admin),
        "/api/v4/database/recycle",
        None,
    )
    .await;
    assert_eq!((go_status, rs_status), (200, 200));
    assert_eq!(go_body, rs_body);
    assert_eq!(rs_body, br#"{"status":"OK"}"#);

    delete_plain_user(&client, &admin, &plain.id).await;
}

#[tokio::test]
async fn the_log_routes_agree_on_the_file_they_are_pointed_at() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let admin = go_minted_token(&client).await;
    let team = a_team(&client, &admin).await;
    let plain = create_plain_user(&client, &admin, &team, "sysopslg").await;

    // The gate on all three, for a member.
    for (method, path, body) in [
        (Method::GET, "/api/v4/logs?logs_per_page=2", None),
        (Method::GET, "/api/v4/logs/download", None),
        (Method::POST, "/api/v4/logs/query", Some(&b"[]"[..])),
    ] {
        assert_same_error(
            &client,
            method,
            Some(&plain.token),
            path,
            body,
            403,
            "api.context.permissions.app_error",
        )
        .await;
    }

    // The page read: the same verdict, and where it is a refusal the same body.
    for path in [
        "/api/v4/logs?logs_per_page=2",
        "/api/v4/logs?logs_per_page=0",
        "/api/v4/logs?page=100000&logs_per_page=3",
    ] {
        let ((go_status, go_body), (rs_status, rs_body)) =
            pair(&client, Method::GET, Some(&admin), path, None).await;
        assert_eq!(
            go_status,
            rs_status,
            "{path}: Go {} / us {}",
            String::from_utf8_lossy(&go_body),
            String::from_utf8_lossy(&rs_body)
        );
        if go_status == 200 {
            // A stack that can read the file: the shape is an array (or `null` for a zero
            // page) on both sides; the lines themselves move between the two reads.
            let go = json(&go_body);
            let rs = json(&rs_body);
            assert_eq!(go.is_array(), rs.is_array(), "{path}");
            assert_eq!(go.is_null(), rs.is_null(), "{path}");
        } else {
            let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, path);
            assert_eq!(go["id"], "api.admin.file_read_error", "{path}");
        }
    }

    // The query: a bad filter is a 500, a filter for other nodes is an empty map without a
    // read, and a real filter is the page read's verdict.
    for body in [&b"[]"[..], b"\"x\"", b"null", b""] {
        assert_same_error(
            &client,
            Method::POST,
            Some(&admin),
            "/api/v4/logs/query",
            Some(body),
            500,
            "api.system.logs.invalidFilter",
        )
        .await;
    }
    let ((go_status, go_body), (rs_status, rs_body)) = pair(
        &client,
        Method::POST,
        Some(&admin),
        "/api/v4/logs/query",
        Some(br#"{"server_names":["other"]}"#),
    )
    .await;
    assert_eq!((go_status, rs_status), (200, 200));
    assert_eq!(go_body, rs_body);
    assert_eq!(rs_body, b"{}");
    for body in [
        &b"{}"[..],
        br#"{"log_levels":["error"],"server_names":["default"]}"#,
    ] {
        let ((go_status, go_body), (rs_status, rs_body)) = pair(
            &client,
            Method::POST,
            Some(&admin),
            "/api/v4/logs/query?logs_per_page=2",
            Some(body),
        )
        .await;
        assert_eq!(
            go_status,
            rs_status,
            "query {}",
            String::from_utf8_lossy(body)
        );
        if go_status == 200 {
            let go = json(&go_body);
            let rs = json(&rs_body);
            assert_eq!(
                go.as_object().map(|o| o.keys().collect::<Vec<_>>()),
                rs.as_object().map(|o| o.keys().collect::<Vec<_>>())
            );
        } else {
            let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "logs/query");
            assert_eq!(go["id"], "api.admin.file_read_error");
        }
    }

    // The download: one 500 for every refusal; a served file is an attachment named for the
    // log file, whose Last-Modified is the instant of each request.
    let ((go_status, go_body), (rs_status, rs_body)) = pair(
        &client,
        Method::GET,
        Some(&admin),
        "/api/v4/logs/download",
        None,
    )
    .await;
    assert_eq!(
        go_status,
        rs_status,
        "download: Go {} / us {}",
        String::from_utf8_lossy(&go_body),
        String::from_utf8_lossy(&rs_body)
    );
    if go_status != 200 {
        let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "logs/download");
        assert_eq!(go["id"], "api.system.logs.download_bytes_buffer.app_error");
    } else {
        let rs = client
            .get(format!("{RUST}/api/v4/logs/download"))
            .header("Authorization", format!("Bearer {admin}"))
            .send()
            .await
            .expect("we answer");
        assert_eq!(
            rs.headers()
                .get("content-disposition")
                .and_then(|v| v.to_str().ok()),
            Some("attachment;filename=\"mattermost.log\"; filename*=UTF-8''mattermost.log")
        );
        assert_eq!(
            rs.headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok()),
            Some("text/plain")
        );
    }

    delete_plain_user(&client, &admin, &plain.id).await;
}

#[tokio::test]
async fn the_restart_is_a_one_second_ok_that_restarts_nothing() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let admin = go_minted_token(&client).await;
    let team = a_team(&client, &admin).await;
    let plain = create_plain_user(&client, &admin, &team, "sysopsrt").await;

    assert_same_error(
        &client,
        Method::POST,
        Some(&plain.token),
        "/api/v4/restart",
        None,
        403,
        "api.context.permissions.app_error",
    )
    .await;

    for base in [GO, RUST] {
        let started = Instant::now();
        let (status, body, served) = request_raw(
            &client,
            base,
            Method::POST,
            Some(&admin),
            "/api/v4/restart",
            None,
        )
        .await;
        let elapsed = started.elapsed();
        assert_eq!(status, 200, "{base}: {}", String::from_utf8_lossy(&body));
        assert_eq!(body, br#"{"status":"OK"}"#, "{base}");
        if base == RUST {
            assert_eq!(served.as_deref(), Some("rust"));
        }
        assert!(
            elapsed >= Duration::from_secs(1) && elapsed < Duration::from_secs(5),
            "{base}: the 200 arrives after the one-second sleep, took {elapsed:?}"
        );
        // Nothing restarted: the next request is answered at once.
        let started = Instant::now();
        let (status, _, _) = request_raw(
            &client,
            base,
            Method::GET,
            None,
            "/api/v4/system/ping",
            None,
        )
        .await;
        assert_eq!(status, 200, "{base} is still up");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{base} did not restart"
        );
    }

    delete_plain_user(&client, &admin, &plain.id).await;
}

#[tokio::test]
async fn the_upgrade_routes_refuse_on_this_host_as_go_does() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let admin = go_minted_token(&client).await;
    let team = a_team(&client, &admin).await;
    let plain = create_plain_user(&client, &admin, &team, "sysopsup").await;

    for (method, path) in [
        (Method::POST, "/api/v4/upgrade_to_enterprise"),
        (Method::GET, "/api/v4/upgrade_to_enterprise/status"),
        (Method::GET, "/api/v4/upgrade_to_enterprise/allowed"),
    ] {
        assert_same_error(
            &client,
            method,
            Some(&plain.token),
            path,
            None,
            403,
            "api.context.permissions.app_error",
        )
        .await;
    }

    let ((go_status, go_body), (rs_status, rs_body)) = pair(
        &client,
        Method::GET,
        Some(&admin),
        "/api/v4/upgrade_to_enterprise/status",
        None,
    )
    .await;
    assert_eq!((go_status, rs_status), (200, 200));
    assert_eq!(go_body, rs_body);
    assert_eq!(rs_body, br#"{"error":null,"percentage":0}"#);

    // This host is not Linux amd64, so both `allowed` and the upgrade itself refuse on the
    // architecture; on an amd64 host both would go on to the executable's directory, and
    // the assertion below names that with the same id either way it goes.
    let expected = if std::env::consts::ARCH == "x86_64" && std::env::consts::OS == "linux" {
        None
    } else {
        Some("api.upgrade_to_enterprise.system_not_supported.app_error")
    };
    for (method, path) in [
        (Method::GET, "/api/v4/upgrade_to_enterprise/allowed"),
        (Method::POST, "/api/v4/upgrade_to_enterprise"),
    ] {
        let ((go_status, go_body), (rs_status, rs_body)) =
            pair(&client, method, Some(&admin), path, None).await;
        assert_eq!(
            go_status,
            rs_status,
            "{path}: Go {} / us {}",
            String::from_utf8_lossy(&go_body),
            String::from_utf8_lossy(&rs_body)
        );
        if go_status == 403 {
            let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, path);
            if let Some(id) = expected {
                assert_eq!(go["id"], id, "{path}");
            }
        }
    }

    delete_plain_user(&client, &admin, &plain.id).await;
}

#[tokio::test]
async fn the_elasticsearch_routes_check_the_body_before_the_gate() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let admin = go_minted_token(&client).await;
    let team = a_team(&client, &admin).await;
    let plain = create_plain_user(&client, &admin, &team, "sysopses").await;

    // A member with an incomplete body learns that before the 403; with no body, the 403.
    for body in [&b"{}"[..], b"[]", b"5", br#"{"ElasticsearchSettings":{}}"#] {
        assert_same_error(
            &client,
            Method::POST,
            Some(&plain.token),
            "/api/v4/elasticsearch/test",
            Some(body),
            400,
            "api.elasticsearch.test_elasticsearch_settings_nil.app_error",
        )
        .await;
    }
    for body in [None, Some(&b"null"[..]), Some(b"{bad")] {
        assert_same_error(
            &client,
            Method::POST,
            Some(&plain.token),
            "/api/v4/elasticsearch/test",
            body,
            403,
            "api.context.permissions.app_error",
        )
        .await;
    }
    assert_same_error(
        &client,
        Method::POST,
        Some(&plain.token),
        "/api/v4/elasticsearch/purge_indexes",
        None,
        403,
        "api.context.permissions.app_error",
    )
    .await;

    // The administrator: no engine on either side, after the password check.
    for body in [None, Some(&b"null"[..])] {
        assert_same_error(
            &client,
            Method::POST,
            Some(&admin),
            "/api/v4/elasticsearch/test",
            body,
            501,
            "ent.elasticsearch.test_config.license.error",
        )
        .await;
    }
    assert_same_error(
        &client,
        Method::POST,
        Some(&admin),
        "/api/v4/elasticsearch/purge_indexes?index=posts&index=users",
        None,
        501,
        "ent.elasticsearch.test_config.license.error",
    )
    .await;

    // The configuration as `GET /config` masks it: the fake password with the running URL and
    // username is substituted and reaches the 501; with the username moved it is the 400.
    let masked: serde_json::Value = client
        .get(format!("{GO}/api/v4/config"))
        .header("Authorization", format!("Bearer {admin}"))
        .send()
        .await
        .expect("Go answers")
        .json()
        .await
        .expect("the config decodes");
    assert_eq!(
        masked["ElasticsearchSettings"]["Password"],
        "********************************"
    );
    let same = serde_json::to_vec(&masked).unwrap();
    assert_same_error(
        &client,
        Method::POST,
        Some(&admin),
        "/api/v4/elasticsearch/test",
        Some(&same),
        501,
        "ent.elasticsearch.test_config.license.error",
    )
    .await;
    let mut moved = masked.clone();
    moved["ElasticsearchSettings"]["Username"] = serde_json::Value::String("someone-else".into());
    let moved = serde_json::to_vec(&moved).unwrap();
    assert_same_error(
        &client,
        Method::POST,
        Some(&admin),
        "/api/v4/elasticsearch/test",
        Some(&moved),
        400,
        "ent.elasticsearch.test_config.reenter_password",
    )
    .await;
    // A wrong-typed field is allocated, not nil: the URL reads as "" and has therefore moved.
    let mut typed = masked.clone();
    typed["ElasticsearchSettings"]["ConnectionURL"] = serde_json::Value::from(5);
    let typed = serde_json::to_vec(&typed).unwrap();
    assert_same_error(
        &client,
        Method::POST,
        Some(&admin),
        "/api/v4/elasticsearch/test",
        Some(&typed),
        400,
        "ent.elasticsearch.test_config.reenter_password",
    )
    .await;
    // One field absent is nil, whatever the rest.
    let mut missing = masked.clone();
    missing["ElasticsearchSettings"]
        .as_object_mut()
        .unwrap()
        .remove("Sniff");
    let missing = serde_json::to_vec(&missing).unwrap();
    assert_same_error(
        &client,
        Method::POST,
        Some(&admin),
        "/api/v4/elasticsearch/test",
        Some(&missing),
        400,
        "api.elasticsearch.test_elasticsearch_settings_nil.app_error",
    )
    .await;

    delete_plain_user(&client, &admin, &plain.id).await;
}

#[tokio::test]
async fn the_two_forwarded_routes_are_still_gos() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let admin = go_minted_token(&client).await;
    let team = a_team(&client, &admin).await;

    let (status, body, served) = request_raw(
        &client,
        RUST,
        Method::GET,
        Some(&admin),
        &format!("/api/v4/system/notices/{team}?client=web&clientVersion=11.0.0"),
        None,
    )
    .await;
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    assert_eq!(
        served.as_deref(),
        Some("go"),
        "the notices read forwards on its cache ([D-681])"
    );

    let (status, _, served) = request_raw(
        &client,
        RUST,
        Method::POST,
        Some(&admin),
        "/api/v4/notifications/test",
        None,
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(
        served.as_deref(),
        Some("go"),
        "the test notification forwards on ForceNotification ([D-680])"
    );
}

/// Sort each check's records so a tie in `ORDER BY parent id` cannot fail the comparison.
fn normalise_integrity(mut results: serde_json::Value) -> serde_json::Value {
    if let Some(list) = results.as_array_mut() {
        for result in list {
            if let Some(records) = result["data"]["records"].as_array_mut() {
                records.sort_by_key(|r| {
                    (
                        r["parent_id"].as_str().unwrap_or("").to_owned(),
                        r["child_id"].as_str().unwrap_or("").to_owned(),
                    )
                });
            }
        }
    }
    results
}

#[tokio::test]
async fn the_local_pairs_match_over_the_socket() {
    if !stack_enabled() || !sockets_enabled() {
        return;
    }

    let ((go_status, go_body), (rs_status, rs_body)) = both("POST", "/api/v4/integrity").await;
    assert_eq!(go_status, 200, "{}", String::from_utf8_lossy(&go_body));
    assert_eq!(rs_status, 200, "{}", String::from_utf8_lossy(&rs_body));
    assert!(
        !rs_body.ends_with(b"\n"),
        "json.Marshal writes no trailing newline"
    );
    let go = json(&go_body);
    let rs = json(&rs_body);
    assert_eq!(go.as_array().map(Vec::len), Some(41));
    assert_eq!(rs.as_array().map(Vec::len), Some(41));
    // Headers and order are exact; records are compared sorted within each check.
    for (i, (g, r)) in go
        .as_array()
        .unwrap()
        .iter()
        .zip(rs.as_array().unwrap())
        .enumerate()
    {
        assert_eq!(g["err"], r["err"], "check {i}");
        for key in [
            "parent_name",
            "child_name",
            "parent_id_attr",
            "child_id_attr",
        ] {
            assert_eq!(g["data"][key], r["data"][key], "check {i} {key}");
        }
    }
    assert_eq!(normalise_integrity(go), normalise_integrity(rs));
    // Byte for byte where the planner agreed on the order, which it does on this stack; a
    // difference here with equal sorted records is a tie, not a divergence.
    if go_body != rs_body {
        eprintln!("integrity bodies differ only in record order within a check");
    }

    // `GET /logs` under the local session passes the gate; the verdict is the stack's.
    let ((go_status, go_body), (rs_status, rs_body), _) =
        common::local_socket::both_maybe_forwarded("GET", "/api/v4/logs?logs_per_page=1").await;
    assert_eq!(
        go_status,
        rs_status,
        "socket logs: Go {} / us {}",
        String::from_utf8_lossy(&go_body),
        String::from_utf8_lossy(&rs_body)
    );
    if go_status != 200 {
        let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "socket /logs");
        assert_eq!(go["id"], "api.admin.file_read_error");
    }
    let _ = both_with_body;
}
