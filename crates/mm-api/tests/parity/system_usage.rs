//! Cross-server parity for the ten routes migrated on 2026-09-07: five under `/system` and
//! `/cluster`, one audit listing, three usage counters and the ancillary-permission expansion.
//!
//! ```sh
//! scripts/parity.sh -p mm-api --test parity system_usage
//! ```
//!
//! # Why these are one module
//!
//! They are one commit and they share exactly one property worth testing together: none of them
//! takes a path parameter, so the whole class of `{user_id}`-shadowing questions that dominates
//! the rest of this suite does not arise. What is left is the wire format — and these ten routes
//! use **three different writers** between them, which is the thing a reader gets wrong.

use crate::common;

use common::{
    ACTIVE_LICENCE_ROW, GO, RUST, client, fetch_both, fetch_both_raw, fetch_both_stable,
    go_minted_token, post_both_raw, stack_enabled,
};

/// `GET /api/v4/system/ping`, with no credentials at all.
///
/// The one route this server answers anonymously — `api.APIHandler`, not
/// `APISessionRequired` — so the test sends no `Authorization` header and expects 200 from both.
#[tokio::test]
async fn the_ping_answers_without_a_session() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    const PATH: &str = "/api/v4/system/ping";

    let get = async |base: &str| {
        let response = client
            .get(format!("{base}{PATH}"))
            .send()
            .await
            .unwrap_or_else(|e| panic!("{base}{PATH} is unreachable: {e}"));
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

    let (go_status, _, go) = get(GO).await;
    let (rs_status, served_by, rs) = get(RUST).await;

    assert_eq!(go_status, 200, "an anonymous ping is a 200 on Go");
    assert_eq!(rs_status, go_status);
    assert_eq!(
        served_by.as_deref(),
        Some("rust"),
        "and it is answered here, not forwarded"
    );
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{PATH}"
    );

    assert!(
        !rs.ends_with(b"\n"),
        "`model.ToJSON` is a bare `json.Marshal`; every other route in this module that uses \
         `Encode` does end in a newline"
    );

    // The key order is the assertion that a `serde_json::Value` comparison would lose: Go
    // marshals a `map[string]any` with **byte-sorted** keys, so the lower-case `status` is last.
    let body = String::from_utf8(rs.clone()).expect("utf-8");
    assert!(
        body.ends_with(r#""status":"OK"}"#),
        "`status` sorts after every capitalised key: {body}"
    );
    assert!(
        body.starts_with(r#"{"ActiveSearchBackend":"database","#),
        "no search engine is registered on Team Edition: {body}"
    );
    assert!(
        !body.contains("TestFeatureFlag"),
        "the key is absent unless the flag is set: {body}"
    );
}

/// Each of the three query parameters that hands the ping back to Go.
///
/// This is the test the handler's boundary table exists for: none of the three is something the
/// parity suite can arrange the *effect* of — there is no push proxy, no goroutine threshold —
/// but the routing decision is observable in the header, and that is what has to be right.
#[tokio::test]
async fn the_extended_pings_are_forwarded() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();

    let served_by = async |query: &str| {
        client
            .get(format!("{RUST}/api/v4/system/ping{query}"))
            .send()
            .await
            .expect("we answer")
            .headers()
            .get("x-mmrs-served-by")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    };

    assert_eq!(served_by("").await.as_deref(), Some("rust"));
    assert_eq!(
        served_by("?get_server_status=true").await.as_deref(),
        Some("go"),
        "the database and filestore checks are Go's to run"
    );
    assert_eq!(
        served_by("?device_id=abc").await.as_deref(),
        Some("go"),
        "there is no push client here"
    );

    // The near misses. Go compares `get_server_status` against the literal `"true"` and
    // `device_id` against emptiness, so neither of these is an extended ping.
    assert_eq!(
        served_by("?get_server_status=1").await.as_deref(),
        Some("rust"),
        "`1` is not `true`"
    );
    assert_eq!(
        served_by("?device_id=").await.as_deref(),
        Some("rust"),
        "an empty device id is not a device id"
    );
    assert_eq!(
        served_by("?use_rest_semantics=true").await.as_deref(),
        Some("rust"),
        "`use_rest_semantics` only matters on an unhealthy answer, which we never produce"
    );
}

/// `GET /api/v4/system/timezones` — 592 strings from a compile-time table on both servers.
#[tokio::test]
async fn the_timezone_list_matches_byte_for_byte() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;
    const PATH: &str = "/api/v4/system/timezones";

    let (go, rs) = fetch_both(&client, &token, PATH).await;
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{PATH}"
    );
    assert!(!rs.ends_with(b"\n"), "`json.Marshal` and a bare `Write`");

    let zones: Vec<String> = serde_json::from_slice(&go).expect("an array of strings");
    assert!(
        zones.len() > 400,
        "the table is the Go source's literal, not the host's tzdata: {}",
        zones.len()
    );
    assert!(zones.contains(&"Asia/Kolkata".to_owned()));
    assert_eq!(
        zones.first().map(String::as_str),
        Some("Africa/Abidjan"),
        "and the order is the table's, not sorted by us"
    );
}

/// `GET /api/v4/system/schema/version` — every applied migration, newest first.
#[tokio::test]
async fn the_applied_migrations_match_and_are_newest_first() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;
    const PATH: &str = "/api/v4/system/schema/version";

    let (go, rs) = fetch_both(&client, &token, PATH).await;
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{PATH}"
    );
    assert!(!rs.ends_with(b"\n"), "`json.Marshal` and a bare `Write`");

    let migrations: Vec<serde_json::Value> = serde_json::from_slice(&go).expect("an array");
    assert!(
        migrations.len() > 50,
        "a migrated schema has run many: {}",
        migrations.len()
    );
    let versions: Vec<i64> = migrations
        .iter()
        .map(|m| m["version"].as_i64().expect("an integer version"))
        .collect();
    let mut descending = versions.clone();
    descending.sort_unstable_by(|a, b| b.cmp(a));
    assert_eq!(versions, descending, "`ORDER BY Version DESC`");
    assert_eq!(
        migrations[0].as_object().expect("an object").len(),
        2,
        "two fields, `version` and `name`: {}",
        migrations[0]
    );
}

/// **The schema-migration permission is "any sysconsole read", not `manage_system`.**
///
/// A system admin holds both, so swapping the check for `manage_system` survived the first run.
/// `system_read_only_admin` is the discriminating role: 53 sysconsole read permissions and no
/// `manage_system`, so it is admitted by Go's rule and refused by the wrong one.
#[tokio::test]
async fn a_read_only_admin_may_list_the_schema_migrations() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let admin = go_minted_token(&client).await;
    let team = common::create_team(&client, &admin, "schemaperm").await;
    let reader = common::create_plain_user(&client, &admin, &team, "schemaperm").await;
    const PATH: &str = "/api/v4/system/schema/version";

    // A plain user is refused by both rules, so this half proves the token is not simply
    // all-powerful.
    let ((go_status, go), (rs_status, rs)) = fetch_both_raw(&client, &reader.token, PATH).await;
    assert_eq!(go_status, 403, "a plain user holds no sysconsole read");
    assert_eq!(rs_status, go_status, "{PATH}");
    common::assert_error_bodies_match_except_known_gaps(&go, &rs, PATH);

    if !common::set_user_roles(&reader.id, "system_user system_read_only_admin").await {
        common::delete_plain_user(&client, &admin, &reader.id).await;
        return; // no DATABASE_URL
    }
    // **A fresh token, not the one above.** `SessionHasPermissionTo` reads `session.Roles`, which
    // is copied at login and never refreshed — so the old token still holds `system_user` alone
    // and Go answers 403 to a user who now has the role. See [`common::login_plain_user`].
    let reader_token = common::login_plain_user(&client, "schemaperm").await;

    let ((go_status, go), (rs_status, rs)) = fetch_both_raw(&client, &reader_token, PATH).await;
    assert_eq!(
        go_status,
        200,
        "`SessionHasPermissionToAny(SysconsoleReadPermissions)` admits a read-only admin: {}",
        String::from_utf8_lossy(&go)
    );
    assert_eq!(rs_status, go_status, "{PATH}");
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{PATH}"
    );

    common::delete_plain_user(&client, &admin, &reader.id).await;
}

/// `GET /api/v4/system/onboarding/complete`, and the sibling POST that must stay Go's.
///
/// # The row is planted, and that is the whole test
///
/// A real server already holds `FirstAdminSetupComplete = "false"`, which is **also** what the
/// handler synthesises when the row is missing — so the stored branch and the synthesised branch
/// produce identical bytes and two mutations of that decision survived the first run. The fixture
/// therefore drives all three states: a distinctive stored value, no row at all, and whatever was
/// there to begin with, restored at the end.
#[tokio::test]
async fn the_onboarding_flag_matches_in_every_state() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;
    const PATH: &str = "/api/v4/system/onboarding/complete";
    const KEY: &str = "FirstAdminSetupComplete";

    let Some(original) = common::system_value(KEY).await else {
        return; // no DATABASE_URL: the planting half of this test cannot run
    };

    let compare = async |expected_value: &str| {
        let (go, rs) = fetch_both(&client, &token, PATH).await;
        assert_eq!(
            String::from_utf8_lossy(&go),
            String::from_utf8_lossy(&rs),
            "{PATH}"
        );
        assert!(
            rs.ends_with(b"\n"),
            "this one *is* `json.NewEncoder(w).Encode`, unlike the two routes above it"
        );
        let row: serde_json::Value = serde_json::from_slice(&go).expect("an object");
        assert_eq!(row["name"], KEY);
        assert_eq!(
            row["value"], expected_value,
            "the `Systems` table is map[string]string, so this is text and never a bool: {row}"
        );
        assert_eq!(row.as_object().expect("an object").len(), 2);
    };

    // A stored value neither server would ever synthesise, so "read the row" and "make one up"
    // are finally different answers.
    common::set_system_value(KEY, Some("true")).await;
    compare("true").await;

    // And with no row at all, both servers must synthesise the *string* `"false"` — not a 404,
    // not `null`, not a bool.
    common::set_system_value(KEY, None).await;
    compare("false").await;

    match original {
        Some(Some(value)) => {
            common::set_system_value(KEY, Some(&value)).await;
        }
        Some(None) => {
            // The row existed with a SQL NULL value. Nothing writes that, and restoring it would
            // need a third helper; the server treats NULL and absent alike, so absent it stays.
            common::set_system_value(KEY, None).await;
        }
        None => {
            common::set_system_value(KEY, None).await;
        }
    }

    // The POST on the same path installs marketplace plugins; there is no plugin host here.
    let ours = client
        .post(format!("{RUST}{PATH}"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!({"organization": "parity-must-not-write"}))
        .send()
        .await
        .expect("we answer");
    assert_eq!(
        ours.headers()
            .get("x-mmrs-served-by")
            .and_then(|v| v.to_str().ok()),
        Some("go"),
        "POST {PATH} must be forwarded"
    );
}

/// `GET /api/v4/cluster/status` — `[]` on an unlicensed server, and Go's when licensed.
#[tokio::test]
async fn the_cluster_roster_is_an_empty_array() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;
    const PATH: &str = "/api/v4/cluster/status";

    let (go, rs) = fetch_both(&client, &token, PATH).await;
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{PATH}"
    );
    assert_eq!(
        rs, b"[]",
        "`make([]*model.ClusterInfo, 0)` — an empty array, never `null`"
    );
    assert!(!rs.ends_with(b"\n"), "`json.Marshal` and a bare `Write`");
}

/// `GET /api/v4/audits` — the server-wide log, which is the same store call with an empty user id.
///
/// # Byte-comparing page 0 of this list is the wrong oracle, and that took three attempts to see
///
/// `Audits` is append-only and sorted newest-first, so page 0 shifts every time **anything** on
/// either server writes a row — a login, or `getOnboarding`, which calls `c.LogAudit("attempt")`.
/// This binary does both, concurrently, from other tests. A bracketed byte comparison
/// ([`common::fetch_both_stable`]) therefore depends on catching the server in a quiet moment; at
/// twelve windows it reported a **no-op control mutation as CAUGHT**, and at forty it still
/// failed once the onboarding test started writing two rows per state.
///
/// The fix is to compare what is actually stable. New rows only ever *prepend*, so two page-0
/// reads taken k writes apart overlap in a contiguous run: a suffix of the earlier read is a
/// prefix of the later one. That overlap is exact — same rows, same order, same bytes — and a
/// mutation of the query, the ordering or the filter breaks it. See [`overlapping_run`].
#[tokio::test]
async fn the_server_wide_audit_page_matches() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;
    const PATH: &str = "/api/v4/audits";

    // **Both requests at once, and retried on a burst.** Fifteen other tests in this binary are
    // hitting Go concurrently and almost every call they make writes an audit row — a login writes
    // two, `getOnboarding` writes one per read. Issued sequentially, the two pages could be more
    // than sixty rows apart and share nothing at all; that happened in two runs out of five.
    // `join!` puts the two reads in the same instant, and the retry rides out the case where a
    // burst still lands between them.
    let mut found = None;
    for attempt in 1..=25u64 {
        let (go, rs) = tokio::join!(
            fetch_raw(&client, GO, &token, PATH),
            fetch_raw(&client, RUST, &token, PATH)
        );
        let (go_status, go) = go;
        let (rs_status, rs) = rs;
        assert_eq!(go_status, 200);
        assert_eq!(rs_status, go_status, "{PATH}");
        assert!(rs.ends_with(b"\n"), "`json.NewEncoder(w).Encode`");

        let go_rows: Vec<serde_json::Value> = serde_json::from_slice(&go).expect("an array");
        let rs_rows: Vec<serde_json::Value> = serde_json::from_slice(&rs).expect("an array");
        assert_eq!(
            go_rows.len(),
            rs_rows.len(),
            "both pages are `LIMIT 60`, whatever moved between them"
        );
        assert!(
            !go_rows.is_empty(),
            "the empty-user-id branch must not be reading `WHERE userid = ''`"
        );

        if let Some(overlap) = overlapping_run(&go_rows, &rs_rows)
            && overlap >= 20
        {
            found = Some((go_rows, overlap));
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(40 * attempt)).await;
    }

    let (go_rows, overlap) = found.expect(
        "twenty-five paired reads and never a twenty-row contiguous run: that is a divergence in \
         the query, not the audit churn this test rides out",
    );
    assert!(overlap >= 20, "{overlap} rows overlap");

    // **The point of the route.** An earlier port made the empty id match nothing; a page holding
    // more than one distinct user id is what says that branch is now the unfiltered one.
    let mut users: Vec<&str> = go_rows
        .iter()
        .filter_map(|row| row["user_id"].as_str())
        .collect();
    users.sort_unstable();
    users.dedup();
    assert!(
        !users.is_empty(),
        "every audit row carries a user id: {go_rows:?}"
    );

    // Every field, on a row both servers returned — so the overlap above is not agreement on a
    // truncated shape.
    let sample = &go_rows[0];
    for field in [
        "id",
        "create_at",
        "user_id",
        "action",
        "extra_info",
        "ip_address",
        "session_id",
    ] {
        assert!(
            sample.get(field).is_some(),
            "`model.Audit` has no `omitempty`, so {field} is always on the wire: {sample}"
        );
    }
    assert_eq!(sample.as_object().expect("an object").len(), 7);
}

/// One GET against one server, returning `(status, body)` — the halves [`fetch_both_raw`] runs
/// sequentially, so a caller can `join!` them instead.
async fn fetch_raw(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    path: &str,
) -> (u16, Vec<u8>) {
    let response = client
        .get(format!("{base}{path}"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .unwrap_or_else(|e| panic!("{base}{path} is unreachable: {e}"));
    let status = response.status().as_u16();
    (status, response.bytes().await.expect("body reads").to_vec())
}

/// The length of the longest contiguous run shared by two newest-first pages of the same list.
///
/// Rows are only ever prepended, so if `b` was read after `a` and k rows arrived in between, then
/// `a[..len-k] == b[k..]`. Returns the overlap length for the first shift that matches in either
/// direction, or [`None`] when no shift explains the difference — which is a real divergence.
///
/// Compares whole rows, so an ordering change, a dropped field or a different `WHERE` all break
/// it. A shift of 0 is checked first, so a quiescent server still gives the full-page answer.
fn overlapping_run(a: &[serde_json::Value], b: &[serde_json::Value]) -> Option<usize> {
    let len = a.len().min(b.len());
    for shift in 0..len {
        if a[..len - shift] == b[shift..len] {
            return Some(len - shift);
        }
        if b[..len - shift] == a[shift..len] {
            return Some(len - shift);
        }
    }
    None
}

/// The audit page is genuinely *not* the per-user page — otherwise the test above could pass
/// against the old, wrong store branch on a single-user server.
#[tokio::test]
async fn the_server_wide_page_is_wider_than_one_user_s() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let me = common::logged_in_user_id();

    let ((all_status, all), _) =
        fetch_both_raw(&client, &token, "/api/v4/audits?per_page=200").await;
    let ((mine_status, mine), _) = fetch_both_raw(
        &client,
        &token,
        &format!("/api/v4/users/{me}/audits?per_page=200"),
    )
    .await;
    assert_eq!(all_status, 200);
    assert_eq!(mine_status, 200);

    let count = |body: &[u8]| -> usize {
        serde_json::from_slice::<serde_json::Value>(body)
            .ok()
            .and_then(|v| v.as_array().map(Vec::len))
            .unwrap_or(0)
    };
    assert!(
        count(&all) >= count(&mine),
        "the unfiltered page cannot be smaller than one user's slice of it"
    );
}

/// **A plain user is refused the server-wide audit log**, with Go's own body.
///
/// Added because a mutation deleting the `read_audits` check **survived** the first run of
/// `scripts/mutations/system-usage.plan`: every test here used an admin token, so nothing could
/// tell a permission check from no permission check. `read_audits` is one of the original coarse
/// permissions — a role holding sysconsole compliance reads does not get in — and without this
/// the route would have leaked every user's IP addresses and session ids to anyone signed in.
#[tokio::test]
async fn a_plain_user_is_refused_the_audit_log() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let admin = go_minted_token(&client).await;
    let team = common::create_team(&client, &admin, "auditperm").await;
    let plain = common::create_plain_user(&client, &admin, &team, "auditperm").await;

    const PATH: &str = "/api/v4/audits";
    let ((go_status, go), (rs_status, rs)) = fetch_both_raw(&client, &plain.token, PATH).await;
    assert_eq!(go_status, 403, "`read_audits` is a system-admin permission");
    assert_eq!(rs_status, go_status, "{PATH}");
    let body = common::assert_error_bodies_match_except_known_gaps(&go, &rs, PATH);
    assert_eq!(body["id"], "api.context.permissions.app_error");

    // And the same user *is* allowed their own audit page — so the refusal above is about the
    // route's permission and not about the token being unusable.
    let own = format!("/api/v4/users/{}/audits", plain.id);
    let ((go_status, _), (rs_status, _)) = fetch_both_raw(&client, &plain.token, &own).await;
    assert_eq!(go_status, 200, "a user may read their own access history");
    assert_eq!(rs_status, go_status, "{own}");

    common::delete_plain_user(&client, &admin, &plain.id).await;
}

/// The three usage counters. One test, because they differ only in which number they round.
#[tokio::test]
async fn the_usage_counters_match() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;

    // **Go's posts count lives in a thirty-minute cache that no write invalidates.** Without this
    // the comparison is against whatever Go last computed — measured at `400` against a table
    // holding `18`. See [`common::invalidate_go_caches`], and [D-087] for why we are the current
    // one of the two.
    common::invalidate_go_caches(&client, &token).await;

    for path in [
        "/api/v4/usage/posts",
        "/api/v4/usage/storage",
        "/api/v4/usage/teams",
    ] {
        let (go, rs) = fetch_both_stable(&client, &token, path).await;
        assert_eq!(
            String::from_utf8_lossy(&go),
            String::from_utf8_lossy(&rs),
            "{path}"
        );
        assert!(
            !rs.ends_with(b"\n"),
            "{path} is `json.Marshal` and a bare `Write`"
        );
    }

    // The shapes, so a byte-identical pair of empty objects could not pass.
    common::invalidate_go_caches(&client, &token).await;
    let (posts, _) = fetch_both_stable(&client, &token, "/api/v4/usage/posts").await;
    let posts: serde_json::Value = serde_json::from_slice(&posts).expect("json");
    assert!(posts["count"].is_i64(), "{posts}");
    assert_eq!(posts.as_object().expect("an object").len(), 1);

    let (storage, _) = fetch_both_stable(&client, &token, "/api/v4/usage/storage").await;
    let storage: serde_json::Value = serde_json::from_slice(&storage).expect("json");
    assert!(storage["bytes"].is_i64(), "{storage}");

    let (teams, _) = fetch_both_stable(&client, &token, "/api/v4/usage/teams").await;
    let teams: serde_json::Value = serde_json::from_slice(&teams).expect("json");
    assert!(
        teams["active"].as_i64().unwrap_or(0) > 0,
        "this deployment has teams: {teams}"
    );
}

/// **The archived-team counter, with a team that is actually archived.**
///
/// `CloudLimitsArchived` is written by the cloud billing job and by no REST route, so on this
/// deployment the counter is permanently zero — and three mutations of the predicate behind it
/// survived the first run because zero is zero however you compute it. The fixture plants one:
/// a team, deleted through the API, then flagged directly in the column.
///
/// Both halves are then observable. `active` — `AnalyticsTeamCount{IncludeDeleted: false}` — must
/// **not** count it, and `cloud_archived` must, which is what makes the two queries' disagreement
/// about deleted teams visible rather than theoretical.
#[tokio::test]
async fn an_archived_team_is_counted_in_one_column_and_not_the_other() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;
    const PATH: &str = "/api/v4/usage/teams";

    let read = async || -> serde_json::Value {
        let (go, rs) = fetch_both_stable(&client, &token, PATH).await;
        assert_eq!(
            String::from_utf8_lossy(&go),
            String::from_utf8_lossy(&rs),
            "{PATH}"
        );
        serde_json::from_slice(&go).expect("json")
    };

    // **Only `cloud_archived` is compared as a delta.** `active` moves under this test — other
    // tests in this binary create and delete teams concurrently — so an exact `before + 1` on it
    // is a flake, measured as one. Nothing else in the suite writes `CloudLimitsArchived`, so that
    // column is stable, and `active` is covered by the byte comparison against Go inside `read`:
    // a port that set `IncludeDeleted: true` would disagree with Go the moment a deleted team
    // exists, which is exactly the state this fixture creates.
    let before = read().await;

    let team = common::create_team(&client, &token, "archived").await;

    // Soft-delete it through Go, then flag it the way the billing job would.
    let deleted = client
        .delete(format!("{GO}/api/v4/teams/{team}"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("Go answers");
    assert!(deleted.status().is_success(), "the team is soft-deleted");
    if !common::set_team_cloud_limits_archived(&team, true).await {
        return; // no DATABASE_URL
    }

    let archived = read().await;
    assert_eq!(
        archived["cloud_archived"].as_i64().unwrap_or(-1),
        before["cloud_archived"].as_i64().unwrap_or(0) + 1,
        "and the archived counter must pick it up — both halves of the predicate: {archived}"
    );

    // Clear the flag but leave the team deleted: the counter must fall back, which is what says
    // `cloud_limits_archived` is doing the work and not `delete_at` alone.
    common::set_team_cloud_limits_archived(&team, false).await;
    let cleared = read().await;
    assert_eq!(
        cleared["cloud_archived"].as_i64().unwrap_or(-1),
        before["cloud_archived"].as_i64().unwrap_or(0),
        "a deleted team that is not flagged does not count: {cleared}"
    );
}

/// **A post whose `Type` is set but is not a `system_` type.**
///
/// `UsersPostsOnly` is `Type = ''` *and* the bot exclusion; the neighbouring `ExcludeSystemPosts`
/// option — which this route does **not** set — is `Type NOT LIKE 'system_%'`. Every post on this
/// deployment is either untyped or `system_*`, so the two predicates return the same count and a
/// mutation swapping one for the other survived the first run.
///
/// The row is planted directly because the REST API will not create a post with an arbitrary
/// type. It is deleted again at the end: a stray post changes the count every other suite reads.
#[tokio::test]
async fn a_custom_typed_post_is_not_counted() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;
    const PATH: &str = "/api/v4/usage/posts";

    let channel = common::a_channel_the_user_is_in(&client, &token).await;
    let me = common::logged_in_user_id();

    let read = async || -> i64 {
        common::invalidate_go_caches(&client, &token).await;
        let (go, rs) = fetch_both_stable(&client, &token, PATH).await;
        assert_eq!(
            String::from_utf8_lossy(&go),
            String::from_utf8_lossy(&rs),
            "{PATH}"
        );
        serde_json::from_slice::<serde_json::Value>(&go).expect("json")["count"]
            .as_i64()
            .expect("an integer count")
    };

    let before = read().await;

    // **Twenty-five of them, not one.** The route reports a *rounded* count — three significant
    // trailing zeroes — so on a server holding eighteen user posts, one extra post rounds to the
    // same number and the whole comparison is blind to it. That is not hypothetical: the first
    // version of this test planted a single post and the mutation swapping `Type = ''` for
    // `Type NOT LIKE 'system_%'` survived it. Twenty-five moves the count across at least two
    // rounding buckets for any base this deployment can have.
    let mut planted = Vec::new();
    for _ in 0..25 {
        let Some(id) = common::plant_custom_typed_post(&channel, me, "custom_parity_type").await
        else {
            break; // no DATABASE_URL
        };
        planted.push(id);
    }
    if planted.is_empty() {
        return;
    }

    // Both servers must ignore them. The assertion is byte parity *plus* the invariant that the
    // number did not move — a port using `NOT LIKE 'system_%'` would have counted all twenty-five,
    // and so would one that dropped the type predicate entirely.
    let after = read().await;
    for id in &planted {
        common::delete_planted_post(id).await;
    }

    assert_eq!(
        after, before,
        "posts with a non-empty, non-`system_` type are not user posts: {before} -> {after}"
    );

    let restored = read().await;
    assert_eq!(restored, before, "and the fixture cleaned up after itself");
}

/// A **plain user** gets the same usage numbers as an admin — there is no permission check on any
/// of the three, which is worth pinning because it looks like an oversight.
#[tokio::test]
async fn the_usage_counters_need_no_permission() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let admin = go_minted_token(&client).await;
    common::invalidate_go_caches(&client, &admin).await;
    let team = common::create_team(&client, &admin, "usageperm").await;
    let plain = common::create_plain_user(&client, &admin, &team, "usageperm").await;

    for path in [
        "/api/v4/usage/posts",
        "/api/v4/usage/storage",
        "/api/v4/usage/teams",
    ] {
        // The status check is a single unretried pair — it is about the *permission*, and 200 is
        // 200 however many teams exist. The **body** comparison goes through the bracketed
        // fetch, because `active` is a live `COUNT(*)` and other suites create and delete teams
        // throughout the run: an unretried byte comparison of it fails on churn alone, which it
        // did once the schemes fixture started creating five teams of its own.
        let ((go_status, _), (rs_status, _)) = fetch_both_raw(&client, &plain.token, path).await;
        assert_eq!(go_status, 200, "{path} refuses nobody");
        assert_eq!(rs_status, go_status, "{path}");

        let (go, rs) = fetch_both_stable(&client, &plain.token, path).await;
        assert_eq!(
            String::from_utf8_lossy(&go),
            String::from_utf8_lossy(&rs),
            "{path}"
        );
    }

    common::delete_plain_user(&client, &admin, &plain.id).await;
}

/// `POST /api/v4/permissions/ancillary` — the expansion, and the three bodies that are one 400.
#[tokio::test]
async fn the_ancillary_expansion_matches() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;
    const PATH: &str = "/api/v4/permissions/ancillary";

    for body in [
        br#"["sysconsole_write_user_management_channels"]"#.as_slice(),
        br#"["sysconsole_read_user_management_users"]"#.as_slice(),
        // **The one overlapping pair in the table.** `sysconsole_read_compliance_compliance_export`
        // and its `write` sibling both imply `download_compliance_export_result`, so the response
        // contains it **twice** — nothing deduplicates the output, only the input. Chosen by
        // searching the generated table rather than guessed: the first pair tried here overlapped
        // in nothing and the mutation that added an output dedup survived.
        br#"["sysconsole_read_compliance_compliance_export","sysconsole_write_compliance_compliance_export"]"#,
        // Deduplicated on the way in, in arrival order.
        br#"["b","sysconsole_write_user_management_channels","b"]"#,
        br#"["not_a_real_permission"]"#,
    ] {
        let ((go_status, go), (rs_status, rs)) = post_both_raw(&client, &token, PATH, body).await;
        assert_eq!(rs_status, go_status, "{}", String::from_utf8_lossy(body));
        assert_eq!(
            String::from_utf8_lossy(&go),
            String::from_utf8_lossy(&rs),
            "{}",
            String::from_utf8_lossy(body)
        );
        assert!(
            !rs.ends_with(b"\n"),
            "`json.Marshal` and a bare `Write`: {}",
            String::from_utf8_lossy(body)
        );
    }

    // The output really does repeat, so the assertion above is about a body that a dedup would
    // change. Without this, "no dedup" is untested: every other input produces a duplicate-free
    // answer by accident.
    let overlapping =
        br#"["sysconsole_read_compliance_compliance_export","sysconsole_write_compliance_compliance_export"]"#;
    let ((_, go), _) = post_both_raw(&client, &token, PATH, overlapping).await;
    let expanded: Vec<String> = serde_json::from_slice(&go).expect("an array of strings");
    let mut unique = expanded.clone();
    unique.sort();
    unique.dedup();
    assert!(
        unique.len() < expanded.len(),
        "the two inputs share an ancillary permission, so it must appear twice: {expanded:?}"
    );

    // `err != nil || len(permissions) < 1` — one 400 for three different failures.
    for body in [
        b"null".as_slice(),
        b"[]".as_slice(),
        b"{".as_slice(),
        br#"{"a":1}"#,
        b"",
    ] {
        let ((go_status, go), (rs_status, rs)) = post_both_raw(&client, &token, PATH, body).await;
        assert_eq!(
            go_status,
            400,
            "Go refuses {}",
            String::from_utf8_lossy(body)
        );
        assert_eq!(rs_status, go_status, "{}", String::from_utf8_lossy(body));
        common::assert_error_bodies_match_except_known_gaps(
            &go,
            &rs,
            &format!("{PATH} {}", String::from_utf8_lossy(body)),
        );
    }
}

/// Registering these ten must not turn another method on any of their paths into our 405.
#[tokio::test]
async fn other_methods_are_forwarded() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;

    for path in [
        "/api/v4/system/timezones",
        "/api/v4/system/schema/version",
        "/api/v4/cluster/status",
        "/api/v4/audits",
        "/api/v4/usage/posts",
        "/api/v4/usage/storage",
        "/api/v4/usage/teams",
    ] {
        let ours = client
            .delete(format!("{RUST}{path}"))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .expect("we answer");
        assert_eq!(
            ours.headers()
                .get("x-mmrs-served-by")
                .and_then(|v| v.to_str().ok()),
            Some("go"),
            "DELETE {path} must be forwarded"
        );
    }

    // The GET on the ancillary path, which is POST-only in Go.
    let ours = client
        .get(format!("{RUST}/api/v4/permissions/ancillary"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("we answer");
    assert_eq!(
        ours.headers()
            .get("x-mmrs-served-by")
            .and_then(|v| v.to_str().ok()),
        Some("go"),
        "GET /api/v4/permissions/ancillary must be forwarded"
    );
}
