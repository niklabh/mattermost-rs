//! Cross-server parity for the three OAuth **app** reads — `getOAuthApps`, `getOAuthApp` and
//! `getOAuthAppInfo`.
//!
//! ```sh
//! scripts/parity.sh -p mm-api --test parity oauth_apps
//! ```
//!
//! # `client_secret` is on the wire for two of the three
//!
//! Only `/info` calls `Sanitize()`. The list hands every app's secret to anyone with
//! `manage_oauth`, and the single read hands it to the creator or a system-wide admin. Asserted
//! in both directions, because a port that sanitised "to be safe" would break the console page
//! the secret exists for, and one that stopped sanitising `/info` would leak it to every session.
//!
//! # The rows are planted
//!
//! `POST /oauth/apps` would work for the admin, but not for anyone else — `manage_oauth` is
//! granted to `system_admin` and nothing else — and the app whose creator is *not* the caller is
//! what makes the single read's second gate observable. Nothing in Go caches OAuth apps
//! (`localcachelayer` has no oauth layer), so a direct write is visible to both servers at once.

use crate::common;

use common::{
    RUST, assert_error_bodies_match_except_known_gaps, client, create_plain_user, create_team,
    fetch_both_raw, fetch_both_stable, go_minted_token, purge_api_fixtures, stack_enabled,
};

struct Fixture {
    /// Created by the fixture admin.
    mine: String,
    /// Created by the plain user — the app the admin reaches only through
    /// `manage_system_wide_oauth`.
    theirs: String,
    /// `CallbackUrls` is SQL NULL on this one.
    no_callbacks: String,
    /// A 26-character id no row has.
    absent: String,
    plain_token: String,
}

static FIXTURE: tokio::sync::OnceCell<Fixture> = tokio::sync::OnceCell::const_new();

async fn fixture(client: &reqwest::Client, token: &str) -> &'static Fixture {
    FIXTURE
        .get_or_init(|| async {
            purge_api_fixtures().await;

            let team = create_team(client, token, "oauthapps").await;
            let plain = create_plain_user(client, token, &team, "oauth").await;
            let admin = common::logged_in_user_id().to_owned();

            let id = |tag: &str| format!("mmrsoauth00000000000000{tag}");
            let ids: Vec<String> = ["001", "002", "003", "004"].iter().map(|t| id(t)).collect();
            for app_id in &ids {
                assert_eq!(app_id.len(), 26, "{app_id} must be a valid id");
            }

            plant(&[
                (
                    &ids[0],
                    &admin,
                    "mmrs app mine",
                    Some(r#"["http://example.invalid/a"]"#),
                ),
                (
                    &ids[1],
                    &plain.id,
                    "mmrs app theirs",
                    Some(r#"["http://example.invalid/b"]"#),
                ),
                (&ids[2], &admin, "mmrs app no callbacks", None),
            ])
            .await;

            Fixture {
                mine: ids[0].clone(),
                theirs: ids[1].clone(),
                no_callbacks: ids[2].clone(),
                absent: ids[3].clone(),
                plain_token: plain.token,
            }
        })
        .await
}

/// Clear this suite's rows and write the given ones.
///
/// **Every app in the table is in this route's answer** — `GetApps` has no predicate at all — so
/// the purge is what makes the list deterministic, and it runs on the way *in* because an
/// assertion panics past any teardown.
async fn plant(rows: &[(&str, &str, &str, Option<&str>)]) {
    let url = std::env::var("DATABASE_URL")
        .expect("DATABASE_URL must be set for the stack-backed suites; scripts/parity.sh sets it");
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(std::time::Duration::from_secs(5))
        .connect(&url)
        .await
        .expect("the shared database is reachable");

    sqlx::query("DELETE FROM oauthapps WHERE id LIKE 'mmrsoauth%'")
        .execute(&pool)
        .await
        .expect("earlier rows are cleared");

    for (id, creator, name, callbacks) in rows {
        sqlx::query(
            "INSERT INTO oauthapps
                (id, creatorid, createat, updateat, clientsecret, name, description,
                 callbackurls, homepage, istrusted, iconurl, mattermostappid,
                 isdynamicallyregistered)
             VALUES ($1, $2, 1788636490668, 1788636490669, 'mmrssecret000000000000001',
                     $3, 'planted by the parity suite', $4, 'http://example.invalid/',
                     true, 'http://example.invalid/i.png', '', false)",
        )
        .bind(id)
        .bind(creator)
        .bind(name)
        .bind(callbacks)
        .execute(&pool)
        .await
        .expect("the oauth app row is written");
    }
}

const LIST: &str = "/api/v4/oauth/apps";

fn single(app_id: &str) -> String {
    format!("/api/v4/oauth/apps/{app_id}")
}
fn info(app_id: &str) -> String {
    format!("/api/v4/oauth/apps/{app_id}/info")
}

fn ids_of(body: &[u8]) -> std::collections::BTreeSet<String> {
    serde_json::from_slice::<Vec<serde_json::Value>>(body)
        .unwrap_or_else(|e| panic!("decoding {}: {e}", String::from_utf8_lossy(body)))
        .into_iter()
        .map(|app| app["id"].as_str().expect("an id").to_owned())
        .collect()
}

/// The list, byte for byte — every app in the table, **with** its client secret, and **without** a
/// trailing newline.
#[tokio::test]
async fn the_list_is_byte_identical_and_carries_the_secret() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let fixture = fixture(&client, &token).await;

    let (go, rs) = fetch_both_stable(&client, &token, LIST).await;
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{LIST}"
    );
    assert!(
        !rs.ends_with(b"\n"),
        "`json.Marshal` + `w.Write` (oauth.go:173) — no encoder, no newline"
    );

    let listed = ids_of(&go);
    assert!(
        listed.contains(&fixture.mine) && listed.contains(&fixture.theirs),
        "a system-wide admin sees every app, including one it did not create: {listed:?}"
    );

    let apps: Vec<serde_json::Value> = serde_json::from_slice(&go).expect("decodes");
    assert!(
        apps.iter().all(|app| app["client_secret"] != ""),
        "the list is **not** sanitised: {apps:?}"
    );
}

/// The single read: a trailing newline, the secret present, and the `callback_urls` states
/// preserved — `null` is not `[]`.
#[tokio::test]
async fn a_single_app_is_byte_identical_and_keeps_its_callback_urls() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let fixture = fixture(&client, &token).await;

    let p = single(&fixture.mine);
    let (go, rs) = fetch_both_stable(&client, &token, &p).await;
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{p}"
    );
    assert!(
        rs.ends_with(b"\n"),
        "{p}: `json.NewEncoder`, unlike the list"
    );

    let app: serde_json::Value = serde_json::from_slice(&go).expect("decodes");
    assert_eq!(app["client_secret"], "mmrssecret000000000000001", "{p}");
    assert_eq!(
        app["callback_urls"],
        serde_json::json!(["http://example.invalid/a"]),
        "{p}"
    );

    let p = single(&fixture.no_callbacks);
    let (go, rs) = fetch_both_stable(&client, &token, &p).await;
    let app: serde_json::Value = serde_json::from_slice(&go).expect("decodes");
    assert!(
        app["callback_urls"].is_null(),
        "{p}: a NULL column is `null`, not `[]`: {app}"
    );
    assert_eq!(go, rs, "{p}");
}

/// `/info` **sanitises the secret and nothing else**, and needs no permission at all — the
/// consent screen a would-be authoriser sees.
#[tokio::test]
async fn the_info_route_blanks_the_secret_for_anyone() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let fixture = fixture(&client, &token).await;

    let p = info(&fixture.mine);
    for caller in [&token, &fixture.plain_token] {
        let ((go_status, go), (rs_status, rs)) = fetch_both_raw(&client, caller, &p).await;
        assert_eq!(
            go_status,
            200,
            "{p}: no permission check at all: {}",
            String::from_utf8_lossy(&go)
        );
        assert_eq!(rs_status, go_status, "{p}");
        assert_eq!(
            String::from_utf8_lossy(&go),
            String::from_utf8_lossy(&rs),
            "{p}"
        );

        let app: serde_json::Value = serde_json::from_slice(&go).expect("decodes");
        assert_eq!(app["client_secret"], "", "{p}: sanitised");
        assert_eq!(
            app["callback_urls"],
            serde_json::json!(["http://example.invalid/a"]),
            "{p}: and only the secret is sanitised"
        );
        assert_eq!(app["homepage"], "http://example.invalid/", "{p}");
        assert_eq!(app["is_trusted"], true, "{p}");
    }
}

/// **The list's refusal is a command error id**, not the permission one — Go builds it by hand
/// (oauth.go:147). A client branching on `api.context.permissions.app_error` will not match it.
#[tokio::test]
async fn the_lists_refusal_carries_the_command_error_id() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let fixture = fixture(&client, &token).await;

    let ((go_status, go), (rs_status, rs)) =
        fetch_both_raw(&client, &fixture.plain_token, LIST).await;
    assert_eq!(go_status, 403, "{LIST}");
    assert_eq!(rs_status, go_status, "{LIST}");
    let body = assert_error_bodies_match_except_known_gaps(&go, &rs, LIST);
    assert_eq!(
        body["id"], "api.command.admin_only.app_error",
        "{LIST}: a *command* id on an OAuth route"
    );

    // The single read, by contrast, uses the ordinary permission error — so the two refusals on
    // neighbouring routes carry different ids for the same missing permission.
    //
    // **Against an app the caller created**, deliberately: the route has two gates and both
    // answer 403 with the same body, because the permission name lives in the wiped
    // `detailed_error`. Only an app the caller owns passes the second gate, so only then is the
    // first gate the one refusing.
    let p = single(&fixture.theirs);
    let ((go_status, go), (rs_status, rs)) =
        fetch_both_raw(&client, &fixture.plain_token, &p).await;
    assert_eq!(go_status, 403, "{p}");
    assert_eq!(rs_status, go_status, "{p}");
    let body = assert_error_bodies_match_except_known_gaps(&go, &rs, &p);
    assert_eq!(body["id"], "api.context.permissions.app_error", "{p}");
}

/// An id no row has is a 404 on both single routes, and the id is the one `GetOAuthApp` uses for
/// not-found — one word away from the 500's.
#[tokio::test]
async fn an_unknown_app_is_a_404_on_both_single_routes() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let fixture = fixture(&client, &token).await;

    for p in [single(&fixture.absent), info(&fixture.absent)] {
        let ((go_status, go), (rs_status, rs)) = fetch_both_raw(&client, &token, &p).await;
        assert_eq!(go_status, 404, "{p}");
        assert_eq!(rs_status, go_status, "{p}");
        let body = assert_error_bodies_match_except_known_gaps(&go, &rs, &p);
        assert_eq!(body["id"], "app.oauth.get_app.find.app_error", "{p}");
    }
}

/// `page * per_page` is the offset, and the pages concatenate back into the whole table.
#[tokio::test]
async fn the_pages_split_the_list() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let _fixture = fixture(&client, &token).await;

    let (whole, _) = fetch_both_stable(&client, &token, LIST).await;
    let all = ids_of(&whole);

    // **`per_page = 2`, not 1.** At a page size of one the offset `page * per_page` is
    // numerically equal to `page`, so a store that used the page number as a raw offset passes.
    // Two pages of two over three rows is the smallest arrangement where the two differ — and the
    // assertion is **disjointness**, because the union is the same either way.
    let mut paged: Vec<String> = Vec::new();
    for page in 0..2 {
        let p = format!("{LIST}?page={page}&per_page=2");
        let (go, rs) = fetch_both_stable(&client, &token, &p).await;
        assert_eq!(
            String::from_utf8_lossy(&go),
            String::from_utf8_lossy(&rs),
            "{p}"
        );
        paged.extend(ids_of(&go));
    }

    let distinct: std::collections::BTreeSet<&String> = paged.iter().collect();
    assert_eq!(
        distinct.len(),
        paged.len(),
        "page 1 must start where page 0 stopped — the offset is page * per_page, not page: \
         {paged:?}"
    );
    // A **subset**, not an equality: `parity/authorized_oauth_apps.rs` plants its own apps in this
    // same table (under a different id prefix, so neither suite purges the other's), and this
    // route has no filter at all — so the table is larger than one suite's fixture. Disjointness
    // is what catches an offset of `page` rather than `page * per_page`; the union never could.
    assert!(
        distinct.iter().all(|id| all.contains(*id)),
        "the pages are drawn from the table: {distinct:?} vs {all:?}"
    );
}

/// A short id is `RequireAppId`'s 400 on both single routes; a non-mux segment is forwarded.
#[tokio::test]
async fn a_short_id_is_a_400_and_a_non_mux_segment_is_forwarded() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;

    for p in [single("abc"), info("abc")] {
        let ((go_status, go), (rs_status, rs)) = fetch_both_raw(&client, &token, &p).await;
        assert_eq!(go_status, 400, "{p}");
        assert_eq!(rs_status, go_status, "{p}");
        let body = assert_error_bodies_match_except_known_gaps(&go, &rs, &p);
        assert_eq!(body["id"], "api.context.invalid_url_param.app_error", "{p}");
    }

    for p in [single("not-an-id"), info("not-an-id")] {
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
            "{p}: outside the mux charset"
        );
        assert_eq!(ours.status().as_u16(), 404, "{p}");
    }
}

/// Registering the three `GET`s must not turn the writes beside them into our 405.
#[tokio::test]
async fn other_methods_are_forwarded() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let fixture = fixture(&client, &token).await;

    let ours = client
        .post(format!("{RUST}{LIST}"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!({}))
        .send()
        .await
        .expect("we answer");
    assert_eq!(
        ours.headers()
            .get("x-mmrs-served-by")
            .and_then(|v| v.to_str().ok()),
        Some("go"),
        "POST {LIST} must be forwarded"
    );

    // Deliberately against the **absent** id: a forwarded `DELETE` is a real delete, and pointing
    // one at a fixture row cost this session a whole suite once already.
    let p = single(&fixture.absent);
    let ours = client
        .delete(format!("{RUST}{p}"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("we answer");
    assert_eq!(
        ours.headers()
            .get("x-mmrs-served-by")
            .and_then(|v| v.to_str().ok()),
        Some("go"),
        "DELETE {p} must be forwarded"
    );
}
