//! Cross-server parity for `GET /api/v4/users/{user_id}/oauth/apps/authorized` —
//! `getAuthorizedOAuthApps`.
//!
//! ```sh
//! scripts/parity.sh -p mm-api --test parity authorized_oauth_apps
//! ```
//!
//! # This list **is** sanitised, and the admin list beside it is not
//!
//! `GetAuthorizedAppsForUser` calls `Sanitize()` on every app (app/oauth.go:642), so
//! `client_secret` is blank here and present in `GET /api/v4/oauth/apps`. Both are asserted, in
//! this file and in `parity/oauth_apps.rs`, because the pair is the whole point: the same rows,
//! two routes, two answers.
//!
//! # The join ignores the preference category
//!
//! `InnerJoin("Preferences AS p ON p.Name = o.Id AND p.UserId = ?")` (oauth_store.go:151) and
//! nothing else. Authorizing an app writes a preference in the `oauth_app` category, but the query
//! never says so — **any** preference row whose `Name` equals an app id authorises that app. The
//! fixture plants one in a different category to prove it, because narrowing the join to the
//! category is the obvious "fix" and it would answer differently from the server we forward to.

use crate::common;

use common::{
    RUST, assert_error_bodies_match_except_known_gaps, client, create_plain_user, create_team,
    fetch_both_raw, fetch_both_stable, go_minted_token, stack_enabled,
};

/// This suite's apps use their own id prefix: `parity/oauth_apps.rs` purges `mmrsoauth%` on the
/// way in, and two suites sharing a prefix would delete each other's rows mid-run.
const ID_PREFIX: &str = "mmrsauthzd";

struct Fixture {
    /// Authorised through a preference in the `oauth_app` category.
    authorized: String,
    /// Authorised through a preference in a **different** category — which the join does not look
    /// at, so it counts.
    authorized_oddly: String,
    /// Exists, and this user has not authorised it.
    unauthorized: String,
    /// A **third** authorised app, so the pagination test can use a page size of two — at a page
    /// size of one the offset `page * per_page` is numerically equal to `page`.
    also_authorized: String,
    user_id: String,
    user_token: String,
    /// A second user who has authorised nothing.
    stranger_token: String,
}

static FIXTURE: tokio::sync::OnceCell<Fixture> = tokio::sync::OnceCell::const_new();

async fn fixture(client: &reqwest::Client, token: &str) -> &'static Fixture {
    FIXTURE
        .get_or_init(|| async {
            let team = create_team(client, token, "authzdoauth").await;
            let user = create_plain_user(client, token, &team, "authzd").await;
            let stranger = create_plain_user(client, token, &team, "authzdother").await;
            let admin = common::logged_in_user_id().to_owned();

            let id = |tag: &str| format!("{ID_PREFIX}0000000000000{tag}");
            let ids: Vec<String> = ["001", "002", "003", "004"].iter().map(|t| id(t)).collect();
            for app_id in &ids {
                assert_eq!(app_id.len(), 26, "{app_id} must be a valid id");
            }

            plant_apps(&admin, &ids).await;
            authorize(&user.id, &ids[0], "oauth_app").await;
            authorize(&user.id, &ids[1], "mmrs_not_oauth").await;
            authorize(&user.id, &ids[3], "oauth_app").await;

            Fixture {
                authorized: ids[0].clone(),
                authorized_oddly: ids[1].clone(),
                unauthorized: ids[2].clone(),
                also_authorized: ids[3].clone(),
                user_id: user.id,
                user_token: user.token,
                stranger_token: stranger.token,
            }
        })
        .await
}

async fn pool() -> sqlx::PgPool {
    let url = std::env::var("DATABASE_URL")
        .expect("DATABASE_URL must be set for the stack-backed suites; scripts/parity.sh sets it");
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(std::time::Duration::from_secs(5))
        .connect(&url)
        .await
        .expect("the shared database is reachable")
}

async fn plant_apps(creator: &str, ids: &[String]) {
    let pool = pool().await;
    sqlx::query("DELETE FROM preferences WHERE name LIKE 'mmrsauthzd%'")
        .execute(&pool)
        .await
        .expect("earlier preferences are cleared");
    sqlx::query("DELETE FROM oauthapps WHERE id LIKE 'mmrsauthzd%'")
        .execute(&pool)
        .await
        .expect("earlier apps are cleared");

    for (n, id) in ids.iter().enumerate() {
        sqlx::query(
            "INSERT INTO oauthapps
                (id, creatorid, createat, updateat, clientsecret, name, description,
                 callbackurls, homepage, istrusted, iconurl, mattermostappid,
                 isdynamicallyregistered)
             VALUES ($1, $2, 1788636490668, 1788636490669, 'mmrssecret000000000000001',
                     $3, 'planted by the parity suite', '[\"http://example.invalid/a\"]',
                     'http://example.invalid/', true, 'http://example.invalid/i.png', '', false)",
        )
        .bind(id)
        .bind(creator)
        .bind(format!("mmrs authorized app {n}"))
        .execute(&pool)
        .await
        .expect("the oauth app row is written");
    }
}

/// Write the preference row that authorises `app_id` for `user_id`.
///
/// The category is a parameter because the join does not read it — see the module note.
async fn authorize(user_id: &str, app_id: &str, category: &str) {
    let pool = pool().await;
    sqlx::query(
        "INSERT INTO preferences (userid, category, name, value) VALUES ($1, $2, $3, 'true')",
    )
    .bind(user_id)
    .bind(category)
    .bind(app_id)
    .execute(&pool)
    .await
    .expect("the preference row is written");
}

fn path(user_id: &str) -> String {
    format!("/api/v4/users/{user_id}/oauth/apps/authorized")
}

fn ids_of(body: &[u8]) -> std::collections::BTreeSet<String> {
    serde_json::from_slice::<Vec<serde_json::Value>>(body)
        .unwrap_or_else(|e| panic!("decoding {}: {e}", String::from_utf8_lossy(body)))
        .into_iter()
        .map(|app| app["id"].as_str().expect("an id").to_owned())
        .collect()
}

/// The authorised list, byte for byte: both authorised apps, the unauthorised one absent, **no**
/// client secret, and no trailing newline.
#[tokio::test]
async fn the_authorized_list_is_byte_identical_and_sanitised() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let fixture = fixture(&client, &token).await;

    let p = path(&fixture.user_id);
    let (go, rs) = fetch_both_stable(&client, &fixture.user_token, &p).await;
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{p}"
    );
    assert!(
        !rs.ends_with(b"\n"),
        "`json.Marshal` + `w.Write` (oauth.go:336) — no encoder, no newline"
    );

    let listed = ids_of(&go);
    assert!(
        listed.contains(&fixture.authorized),
        "the authorised app is listed: {listed:?}"
    );
    assert!(
        !listed.contains(&fixture.unauthorized),
        "and an app this user never authorised is not: {listed:?}"
    );

    let apps: Vec<serde_json::Value> = serde_json::from_slice(&go).expect("decodes");
    assert!(
        apps.iter().all(|app| app["client_secret"] == ""),
        "**sanitised** — unlike `GET /api/v4/oauth/apps`: {apps:?}"
    );
    assert!(
        apps.iter().all(|app| app["homepage"] != ""),
        "and only the secret is sanitised: {apps:?}"
    );
}

/// **The join does not read the preference category.** An app authorised through a row in some
/// unrelated category is in the answer, which is Go's behaviour and the thing a narrowing "fix"
/// would change.
#[tokio::test]
async fn a_preference_in_another_category_still_authorises() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let fixture = fixture(&client, &token).await;

    let p = path(&fixture.user_id);
    let (go, rs) = fetch_both_stable(&client, &fixture.user_token, &p).await;
    let listed = ids_of(&go);
    assert!(
        listed.contains(&fixture.authorized_oddly),
        "the join is on `Name` and `UserId` only: {listed:?}"
    );
    assert_eq!(ids_of(&rs), listed, "{p}");
}

/// The list is per user: a second user who authorised nothing gets an empty array, not `null`
/// (the store starts from `[]*model.OAuthApp{}`).
#[tokio::test]
async fn another_users_list_is_empty_and_is_an_array() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let fixture = fixture(&client, &token).await;

    // The stranger's own id, read through `me`, which this route resolves before validating.
    let p = path("me");
    let (go, rs) = fetch_both_stable(&client, &fixture.stranger_token, &p).await;
    assert_eq!(go, b"[]", "an empty answer is an array, not null");
    assert_eq!(rs, go, "{p}");
}

/// `me` resolves to the caller — and the same request by id is the same answer.
#[tokio::test]
async fn me_resolves_to_the_caller() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let fixture = fixture(&client, &token).await;

    let (go_me, rs_me) = fetch_both_stable(&client, &fixture.user_token, &path("me")).await;
    assert_eq!(go_me, rs_me, "/users/me/oauth/apps/authorized");

    let (go_id, _) = fetch_both_stable(&client, &fixture.user_token, &path(&fixture.user_id)).await;
    assert_eq!(
        String::from_utf8_lossy(&go_me),
        String::from_utf8_lossy(&go_id),
        "`me` is the caller's own id"
    );
}

/// **The gate is `SessionHasPermissionToUser`, not `manage_oauth`.** A plain user reads their own
/// authorisations; another plain user is refused, and the refusal names `edit_other_users` — a
/// write permission on a read route, as `getUserAudits` does.
#[tokio::test]
async fn a_stranger_is_refused_and_an_admin_is_not() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let fixture = fixture(&client, &token).await;

    let p = path(&fixture.user_id);
    let ((go_status, go), (rs_status, rs)) =
        fetch_both_raw(&client, &fixture.stranger_token, &p).await;
    assert_eq!(go_status, 403, "{p}: another user's authorisations");
    assert_eq!(rs_status, go_status, "{p}");
    let body = assert_error_bodies_match_except_known_gaps(&go, &rs, &p);
    assert_eq!(body["id"], "api.context.permissions.app_error", "{p}");

    // The admin has `edit_other_users`, so the same request succeeds — and still sanitised.
    let ((go_status, go), (rs_status, rs)) = fetch_both_raw(&client, &token, &p).await;
    assert_eq!(go_status, 200, "{p}: as the admin");
    assert_eq!(rs_status, go_status, "{p}");
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{p}"
    );
    let apps: Vec<serde_json::Value> = serde_json::from_slice(&go).expect("decodes");
    assert!(
        apps.iter().all(|app| app["client_secret"] == ""),
        "an admin reading someone's authorisations is sanitised too: {apps:?}"
    );
}

/// `page * per_page` is the offset. Two authorised apps, one per page, and the pages are disjoint.
#[tokio::test]
async fn the_pages_split_the_list() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let fixture = fixture(&client, &token).await;

    // **Three** authorised apps and a page size of **two**: at a page size of one the offset
    // `page * per_page` equals `page`, and a store using the page number raw would pass.
    let mut paged: Vec<String> = Vec::new();
    for page in 0..2 {
        let p = format!("{}?page={page}&per_page=2", path(&fixture.user_id));
        let (go, rs) = fetch_both_stable(&client, &fixture.user_token, &p).await;
        assert_eq!(
            String::from_utf8_lossy(&go),
            String::from_utf8_lossy(&rs),
            "{p}"
        );
        paged.extend(ids_of(&go));
    }
    assert_eq!(paged.len(), 3, "two then one: {paged:?}");

    let distinct: std::collections::BTreeSet<&String> = paged.iter().collect();
    assert_eq!(
        distinct.len(),
        paged.len(),
        "page 1 must start where page 0 stopped: {paged:?}"
    );
    assert!(
        distinct.contains(&fixture.also_authorized),
        "and the third authorised app is in there: {paged:?}"
    );
}

/// A short id is `RequireUserId`'s 400; a non-mux segment is forwarded so Go answers its own 404.
#[tokio::test]
async fn a_short_id_is_a_400_and_a_non_mux_segment_is_forwarded() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;

    let p = path("abc");
    let ((go_status, go), (rs_status, rs)) = fetch_both_raw(&client, &token, &p).await;
    assert_eq!(go_status, 400, "{p}");
    assert_eq!(rs_status, go_status, "{p}");
    let body = assert_error_bodies_match_except_known_gaps(&go, &rs, &p);
    assert_eq!(body["id"], "api.context.invalid_url_param.app_error", "{p}");

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
        "{p}"
    );
    assert_eq!(ours.status().as_u16(), 404, "{p}");
}
