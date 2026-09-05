//! Cross-server parity for `GET /api/v4/users/{user_id}/audits` — `getUserAudits`.
//!
//! ```sh
//! scripts/parity.sh -p mm-api --test parity user_audits
//! ```
//!
//! # The subject is a plain user of this suite's own, not the fixture admin
//!
//! `Audits` is append-only and every **login** writes two rows to it. The fixture admin logs in
//! once per test binary, but a whole run's worth of other suites create plain users, and the
//! admin's page-0 is 200-odd rows deep in a table shared with them. A user this suite creates and
//! logs in exactly once has a page-0 that is *exactly* those two rows, for the whole run —
//! a deterministic oracle rather than a moving one.
//!
//! # `ORDER BY CreateAt DESC` has no tiebreak
//!
//! Go sorts on the millisecond alone (audit_store.go:59). The two rows a login writes are
//! milliseconds apart in practice but nothing guarantees it, and two rows sharing a `CreateAt`
//! have no defined order on either server. The byte comparisons here go through
//! [`common::fetch_both_stable`], and [`the_page_is_the_same_set_however_it_is_ordered`] is the
//! assertion that survives a tie.

use crate::common;

use common::{
    GO, RUST, assert_error_bodies_match_except_known_gaps, client, create_plain_user,
    fetch_both_raw, fetch_both_stable, go_minted_token, logged_in_user_id, stack_enabled,
};

/// The subject, its own token, and a second plain user who may not read it.
struct Fixture {
    subject: String,
    subject_token: String,
    stranger_token: String,
    /// A user with **no audit rows at all** — created, never logged in.
    silent: String,
}

static FIXTURE: tokio::sync::OnceCell<Fixture> = tokio::sync::OnceCell::const_new();

async fn fixture(client: &reqwest::Client, token: &str) -> &'static Fixture {
    FIXTURE
        .get_or_init(|| async {
            let team_id = common::a_team_and_channel_the_user_is_in(client, token)
                .await
                .0;

            let subject = create_plain_user(client, token, &team_id, "audits").await;
            let stranger = create_plain_user(client, token, &team_id, "auditsother").await;

            // **Two more logins, so the page size and the page number differ.** With the two rows
            // one login writes, the only page size that splits the list is 1 — and at
            // `per_page = 1` the offset `page * per_page` is numerically equal to `page`, so a
            // store using the page number as a raw offset answers identically. That mutation
            // survived until this loop existed. Six rows let the pagination test use
            // `per_page = 2`, where the two differ.
            log_in_again(client, "audits").await;
            log_in_again(client, "audits").await;

            // `create_plain_user` logs in, which is what writes the two rows. This one must not:
            // it is the fixture for the empty answer, and an empty answer is `null`, not `[]`.
            let silent = create_user_without_logging_in(client, token, "auditssilent").await;

            Fixture {
                subject: subject.id,
                subject_token: subject.token,
                stranger_token: stranger.token,
                silent,
            }
        })
        .await
}

/// Log in again as one of this suite's plain users, writing two more audit rows.
///
/// The token is discarded — the rows are the point.
async fn log_in_again(client: &reqwest::Client, tag: &str) {
    let username = format!("mmrsplain{tag}");
    let response = client
        .post(format!("{GO}/api/v4/users/login"))
        .json(&serde_json::json!({
            "login_id": format!("{username}@mmrs.invalid"),
            "password": "Mmrs-Plain-1234",
        }))
        .send()
        .await
        .expect("Go answers");
    assert_eq!(
        response.status(),
        200,
        "logging {username} in again failed: {}",
        response.text().await.unwrap_or_default()
    );
}

/// Create a user through Go and **do not log in as it**, so its `Audits` stay empty.
///
/// Deliberately not a variant of `create_plain_user`: that helper's login is the whole reason it
/// exists, and a flag turning it off would make every other suite's fixture one boolean away from
/// a user that cannot authenticate.
async fn create_user_without_logging_in(
    client: &reqwest::Client,
    admin_token: &str,
    tag: &str,
) -> String {
    let username = format!("mmrsplain{tag}");
    let response = client
        .post(format!("{GO}/api/v4/users"))
        .header("Authorization", format!("Bearer {admin_token}"))
        .json(&serde_json::json!({
            "email": format!("{username}@mmrs.invalid"),
            "username": username,
            "password": "Mmrs-Plain-1234",
        }))
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "creating the silent user failed: {}",
        response.text().await.unwrap_or_default()
    );
    let created: serde_json::Value = response.json().await.expect("the user decodes");
    created["id"].as_str().expect("an id").to_owned()
}

fn path(user_id: &str) -> String {
    format!("/api/v4/users/{user_id}/audits")
}

/// The whole page, byte for byte, read by the admin.
#[tokio::test]
async fn a_users_audit_page_is_byte_identical() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let fixture = fixture(&client, &token).await;

    let p = path(&fixture.subject);
    let (go, rs) = fetch_both_stable(&client, &token, &p).await;

    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{p}: the audit page must be byte-identical"
    );
    assert!(
        rs.ends_with(b"\n"),
        "`json.NewEncoder(w).Encode` writes a trailing newline"
    );

    let rows: Vec<serde_json::Value> = serde_json::from_slice(&go).expect("Go's audits decode");
    assert_eq!(
        rows.len(),
        6,
        "three logins write two rows each, and nothing else has touched this user: {}",
        String::from_utf8_lossy(&go)
    );
    // The *set* of keys, not their order: `serde_json::Value` stores an object in a `BTreeMap`,
    // so both sides come back alphabetical here whatever the bytes said. Field order is already
    // asserted, and asserted properly, by the byte comparison above.
    assert_eq!(
        rows[0]
            .as_object()
            .expect("an object")
            .keys()
            .map(String::as_str)
            .collect::<std::collections::BTreeSet<_>>(),
        std::collections::BTreeSet::from([
            "id",
            "create_at",
            "user_id",
            "action",
            "extra_info",
            "ip_address",
            "session_id"
        ]),
        "every key present, none omitted — `model.Audit` has no `omitempty`"
    );
    // The pre-login row carries an empty `session_id` and the key is still present — `model.Audit`
    // has no `omitempty` anywhere, which is the thing a port copying its neighbours would break.
    assert!(
        rows.iter()
            .any(|row| row["session_id"] == "" && row["extra_info"] == "authenticated"),
        "the `authenticated` row has an empty but present session_id: {rows:?}"
    );
}

/// **`null`, not `[]`.** `SqlAuditStore.Get` declares `var audits model.Audits` — a nil slice —
/// and sqlx appends into it, so a no-row query never allocates. The bot store beside it starts
/// from `[]*model.Bot{}` and answers `[]` for the same shape of query.
#[tokio::test]
async fn a_user_with_no_audits_is_null_and_not_an_empty_array() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let fixture = fixture(&client, &token).await;

    let p = path(&fixture.silent);
    let (go, rs) = fetch_both_stable(&client, &token, &p).await;

    assert_eq!(go, b"null\n", "Go's nil slice, plus the encoder's newline");
    assert_eq!(rs, go, "{p}: and ours is the same four bytes");
}

/// A page past the end is the same nil slice, reached a different way.
#[tokio::test]
async fn a_page_past_the_end_is_also_null() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let fixture = fixture(&client, &token).await;

    let p = format!("{}?page=50&per_page=60", path(&fixture.subject));
    let (go, rs) = fetch_both_stable(&client, &token, &p).await;
    assert_eq!(go, b"null\n");
    assert_eq!(rs, go, "{p}");
}

/// Pagination: `page * per_page` is the offset, and the two single-row pages together are the
/// whole list. A port that used `page` as a raw offset passes page 0 and fails page 1.
#[tokio::test]
async fn the_pages_split_the_list_and_agree() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let fixture = fixture(&client, &token).await;

    // `per_page = 2`, deliberately not 1. At a page size of one the offset `page * per_page` is
    // numerically equal to `page`, so a store that used the page number as a raw offset would
    // pass every assertion below. Two is the smallest size at which the two differ.
    const PER_PAGE: usize = 2;

    let mut paged: Vec<String> = Vec::new();
    for page in 0..3 {
        let p = format!("{}?page={page}&per_page={PER_PAGE}", path(&fixture.subject));
        let (go, rs) = fetch_both_stable(&client, &token, &p).await;
        assert_eq!(
            String::from_utf8_lossy(&go),
            String::from_utf8_lossy(&rs),
            "{p}"
        );
        let rows: Vec<serde_json::Value> = serde_json::from_slice(&go).expect("decodes");
        assert_eq!(rows.len(), PER_PAGE, "{p}: a full page");
        for row in rows {
            paged.push(row["id"].as_str().expect("an id").to_owned());
        }
    }

    let distinct: std::collections::BTreeSet<&String> = paged.iter().collect();
    assert_eq!(
        distinct.len(),
        paged.len(),
        "the three pages must not overlap — the offset is page * per_page, not page: {paged:?}"
    );

    // And the pages are that same list, cut into threes: page 1 starts where page 0 stopped.
    let whole = format!("{}?page=0&per_page=60", path(&fixture.subject));
    let (go_whole, _) = fetch_both_stable(&client, &token, &whole).await;
    let whole_ids: Vec<String> = serde_json::from_slice::<Vec<serde_json::Value>>(&go_whole)
        .expect("decodes")
        .into_iter()
        .map(|row| row["id"].as_str().expect("an id").to_owned())
        .collect();
    assert_eq!(
        paged, whole_ids,
        "the concatenated pages are the unpaged list, in the same order"
    );
}

/// The set is what survives a `CreateAt` tie, so it is asserted separately from the bytes.
#[tokio::test]
async fn the_page_is_the_same_set_however_it_is_ordered() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let fixture = fixture(&client, &token).await;

    let p = path(&fixture.subject);
    let (go, rs) = fetch_both_stable(&client, &token, &p).await;

    let ids = |body: &[u8]| -> std::collections::BTreeSet<String> {
        serde_json::from_slice::<Vec<serde_json::Value>>(body)
            .expect("decodes")
            .into_iter()
            .map(|row| row["id"].as_str().expect("an id").to_owned())
            .collect()
    };
    assert_eq!(ids(&go), ids(&rs), "{p}: the same rows, whatever the order");

    // And the order both servers did produce is newest-first, which is the only ordering claim
    // the SQL actually makes.
    let stamps: Vec<i64> = serde_json::from_slice::<Vec<serde_json::Value>>(&rs)
        .expect("decodes")
        .into_iter()
        .map(|row| row["create_at"].as_i64().expect("a timestamp"))
        .collect();
    let mut descending = stamps.clone();
    descending.sort_by(|a, b| b.cmp(a));
    assert_eq!(stamps, descending, "ORDER BY CreateAt DESC");
}

/// `per_page` above `PerPageMaximum` is clamped to 200 rather than refused, so the store's own
/// 1000-row bound is unreachable and its 400 never fires.
#[tokio::test]
async fn an_oversized_per_page_is_clamped_rather_than_refused() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let fixture = fixture(&client, &token).await;

    for per_page in ["5000", "201", "-1", "notanumber"] {
        let p = format!("{}?per_page={per_page}", path(&fixture.subject));
        let ((go_status, go), (rs_status, rs)) = fetch_both_raw(&client, &token, &p).await;
        assert_eq!(go_status, 200, "{p}: clamped, never a 400");
        assert_eq!(rs_status, go_status, "{p}");
        assert_eq!(
            String::from_utf8_lossy(&go),
            String::from_utf8_lossy(&rs),
            "{p}"
        );
    }
}

/// `me` is resolved before the id is validated, so `/users/me/audits` is the caller's own page.
#[tokio::test]
async fn me_resolves_to_the_caller() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let fixture = fixture(&client, &token).await;

    // As the subject itself: `me` and its own id must be the same request.
    let (go_me, rs_me) = fetch_both_stable(&client, &fixture.subject_token, &path("me")).await;
    assert_eq!(go_me, rs_me, "/users/me/audits");

    let (go_id, _) =
        fetch_both_stable(&client, &fixture.subject_token, &path(&fixture.subject)).await;
    assert_eq!(
        String::from_utf8_lossy(&go_me),
        String::from_utf8_lossy(&go_id),
        "`me` is the caller's own id"
    );

    // And as the admin, where `me` is a *different* user — so this cannot pass by the two ids
    // happening to coincide.
    let (go_admin, rs_admin) = fetch_both_stable(&client, &token, &path("me")).await;
    assert_eq!(go_admin, rs_admin, "the admin's own audits");
    assert_ne!(
        logged_in_user_id(),
        fixture.subject,
        "the admin and the subject are different users"
    );
}

/// The gate is `SessionHasPermissionToUser`, and its refusal names **`edit_other_users`** — a
/// write permission on a read route, which is Go's own choice.
#[tokio::test]
async fn a_stranger_is_refused_and_the_permission_named_is_edit_other_users() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let fixture = fixture(&client, &token).await;

    let p = path(&fixture.subject);
    let ((go_status, go), (rs_status, rs)) =
        fetch_both_raw(&client, &fixture.stranger_token, &p).await;

    assert_eq!(go_status, 403, "{p}: a plain user may not read another's");
    assert_eq!(rs_status, go_status, "{p}");
    let body = assert_error_bodies_match_except_known_gaps(&go, &rs, &p);
    assert_eq!(body["id"], "api.context.permissions.app_error");
    assert!(
        body["detailed_error"]
            .as_str()
            .unwrap_or_default()
            .is_empty(),
        "`detailed_error` is wiped on both sides: {body}"
    );
}

/// A caller reading its own audits is allowed with no permission at all — the self branch.
#[tokio::test]
async fn a_plain_user_may_read_its_own() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let fixture = fixture(&client, &token).await;

    let p = path(&fixture.subject);
    let ((go_status, go), (rs_status, rs)) =
        fetch_both_raw(&client, &fixture.subject_token, &p).await;
    assert_eq!(go_status, 200, "{p}: the self branch needs no permission");
    assert_eq!(rs_status, go_status);
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{p}"
    );
}

/// An alphanumeric segment that is not a 26-character id is a 400 from `RequireUserId`; a segment
/// outside `[A-Za-z0-9]+` never routes in Go at all and is forwarded so Go answers its own 404.
#[tokio::test]
async fn a_short_id_is_a_400_and_a_non_mux_segment_is_forwarded() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;

    let p = path("abc");
    let ((go_status, go), (rs_status, rs)) = fetch_both_raw(&client, &token, &p).await;
    assert_eq!(go_status, 400, "{p}: `IsValidId` refuses a short id");
    assert_eq!(rs_status, go_status, "{p}");
    let body = assert_error_bodies_match_except_known_gaps(&go, &rs, &p);
    assert_eq!(body["id"], "api.context.invalid_url_param.app_error");

    // A dash is outside gorilla's `[A-Za-z0-9]+`, so Go's mux never matches the route.
    let p = path("not-an-id");
    let ours = client
        .get(format!("{RUST}{p}"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("we answer");
    assert_eq!(
        ours.headers()
            .get("x-mmrs-served-by")
            .and_then(|v| v.to_str().ok()),
        Some("go"),
        "{p}: outside the mux charset, so Go answers its own 404"
    );
    assert_eq!(ours.status().as_u16(), 404, "{p}");
}

/// Every other method on this path stays forwarded — registering the `GET` must not turn a `POST`
/// into our 405.
#[tokio::test]
async fn other_methods_are_forwarded() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let fixture = fixture(&client, &token).await;

    let p = path(&fixture.subject);
    for method in [reqwest::Method::POST, reqwest::Method::DELETE] {
        let ours = client
            .request(method.clone(), format!("{RUST}{p}"))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .expect("we answer");
        assert_eq!(
            ours.headers()
                .get("x-mmrs-served-by")
                .and_then(|v| v.to_str().ok()),
            Some("go"),
            "{method} {p} must be forwarded"
        );
    }
}
