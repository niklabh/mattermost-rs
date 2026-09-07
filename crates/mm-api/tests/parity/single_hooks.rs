//! Cross-server parity for `GET /api/v4/hooks/incoming/{hook_id}` and
//! `GET /api/v4/hooks/outgoing/{hook_id}` — `getIncomingHook` and `getOutgoingHook`.
//!
//! ```sh
//! scripts/parity.sh -p mm-api --test parity single_hooks
//! ```
//!
//! # The ids carry a timestamp, and that is not cosmetic
//!
//! `GetIncoming(id, true)` is the **only** webhook read Go serves from a cache
//! (localcachelayer/webhook_layer.go:41), and it holds a row for thirty minutes with no
//! invalidation a direct database write can reach. Fixed ids would let one run's rows answer the
//! next run's requests. Twelve digits of the clock make every run's ids new to that cache — the
//! same device `parity/roles.rs` uses, for the same reason.
//!
//! # The asymmetry these two routes exist to pin
//!
//! `getIncomingHook` looks the hook's **channel** up and refuses a caller who cannot read it.
//! `getOutgoingHook` does not look at the channel at all. So a team admin who is not a member of a
//! private channel is refused the incoming hook in it and served the outgoing hook in it —
//! [`the_channel_check_is_incoming_only`].

use crate::common;

use common::{
    GO, RUST, assert_error_bodies_match_except_known_gaps, client, create_channel,
    create_channel_typed, create_plain_user, create_team, fetch_both_raw, fetch_both_stable,
    go_minted_token, purge_api_fixtures, stack_enabled,
};

struct Fixture {
    /// Owned by the admin, in a public channel.
    incoming: String,
    /// Owned by the admin, in a **private** channel the team admin is not in.
    incoming_private: String,
    /// Owned by the plain user, in the public channel.
    incoming_theirs: String,
    /// Soft-deleted.
    incoming_deleted: String,
    outgoing: String,
    /// Owned by the plain user — the caller that **owns** a hook and still lacks
    /// `manage_own_outgoing_webhooks`, which is the only way the second gate's refusal is
    /// distinguishable from the third's.
    outgoing_theirs: String,
    /// In the same private channel as `incoming_private`.
    outgoing_private: String,
    outgoing_deleted: String,
    /// A 26-character id no row has.
    absent: String,
    plain_token: String,
    team_admin_token: String,
}

static FIXTURE: tokio::sync::OnceCell<Fixture> = tokio::sync::OnceCell::const_new();

async fn fixture(client: &reqwest::Client, token: &str) -> &'static Fixture {
    FIXTURE
        .get_or_init(|| async {
            purge_api_fixtures().await;

            let team = create_team(client, token, "onehook").await;
            let public = create_channel(client, token, &team, "onehook").await;
            let private = create_channel_typed(client, token, &team, "onehookpriv", "P").await;

            let plain = create_plain_user(client, token, &team, "onehook").await;
            let team_admin = create_plain_user(client, token, &team, "onehookadm").await;
            promote_to_team_admin(client, token, &team, &team_admin.id).await;
            let admin = common::logged_in_user_id().to_owned();

            // Twelve digits of the clock, so no id is one Go already holds. `mmrs1h` + 12 + 8 = 26.
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("after 1970")
                .as_millis()
                % 1_000_000_000_000;
            let id = |tag: &str| format!("mmrs1h{stamp:012}{tag}");
            let ids: Vec<String> = [
                "in000001", "in000002", "in000003", "in000004", "ou000001", "ou000002", "ou000003",
                "absent01", "ou000004",
            ]
            .iter()
            .map(|tag| id(tag))
            .collect();
            for hook_id in &ids {
                assert_eq!(hook_id.len(), 26, "{hook_id} must be a valid id");
            }

            plant_incoming(&[
                (&ids[0], &team, &public, &admin, 0),
                (&ids[1], &team, &private, &admin, 0),
                (&ids[2], &team, &public, &plain.id, 0),
                (&ids[3], &team, &public, &admin, 1_788_636_490_000),
            ])
            .await;
            plant_outgoing(&[
                (&ids[4], &team, &public, &admin, 0),
                (&ids[5], &team, &private, &admin, 0),
                (&ids[6], &team, &public, &admin, 1_788_636_490_000),
                (&ids[8], &team, &public, &plain.id, 0),
            ])
            .await;

            Fixture {
                incoming: ids[0].clone(),
                incoming_private: ids[1].clone(),
                incoming_theirs: ids[2].clone(),
                incoming_deleted: ids[3].clone(),
                outgoing: ids[4].clone(),
                outgoing_theirs: ids[8].clone(),
                outgoing_private: ids[5].clone(),
                outgoing_deleted: ids[6].clone(),
                absent: ids[7].clone(),
                plain_token: plain.token,
                team_admin_token: team_admin.token,
            }
        })
        .await
}

async fn promote_to_team_admin(
    client: &reqwest::Client,
    admin_token: &str,
    team_id: &str,
    user_id: &str,
) {
    let response = client
        .put(format!(
            "{GO}/api/v4/teams/{team_id}/members/{user_id}/schemeRoles"
        ))
        .header("Authorization", format!("Bearer {admin_token}"))
        .json(&serde_json::json!({ "scheme_admin": true, "scheme_user": true }))
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "promoting {user_id} to team admin failed: {}",
        response.text().await.unwrap_or_default()
    );
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

async fn plant_incoming(rows: &[(&str, &str, &str, &str, i64)]) {
    let pool = pool().await;
    sqlx::query("DELETE FROM incomingwebhooks WHERE id LIKE 'mmrs1h%'")
        .execute(&pool)
        .await
        .expect("earlier rows are cleared");

    for (id, team, channel, owner, delete_at) in rows {
        sqlx::query(
            "INSERT INTO incomingwebhooks
                (id, createat, updateat, deleteat, userid, channelid, teamid,
                 displayname, description, username, iconurl, channellocked, lastused)
             VALUES ($1, 1788636490668, 1788636490669, $5, $4, $3, $2,
                     'mmrs one hook', 'planted by the parity suite', 'hookbot',
                     'http://example.invalid/i.png', true, 1788636490670)",
        )
        .bind(id)
        .bind(team)
        .bind(channel)
        .bind(owner)
        .bind(delete_at)
        .execute(&pool)
        .await
        .expect("the incoming hook row is written");
    }
}

async fn plant_outgoing(rows: &[(&str, &str, &str, &str, i64)]) {
    let pool = pool().await;
    sqlx::query("DELETE FROM outgoingwebhooks WHERE id LIKE 'mmrs1h%'")
        .execute(&pool)
        .await
        .expect("earlier rows are cleared");

    for (id, team, channel, creator, delete_at) in rows {
        sqlx::query(
            "INSERT INTO outgoingwebhooks
                (id, token, createat, updateat, deleteat, creatorid, channelid, teamid,
                 triggerwords, triggerwhen, callbackurls, displayname, description,
                 contenttype, username, iconurl)
             VALUES ($1, 'mmrs1htoken000000000000001', 1788636490668, 1788636490669, $5,
                     $4, $3, $2, '[\"alpha\"]', 1, '[\"http://example.invalid/a\"]',
                     'mmrs one hook', 'planted by the parity suite',
                     'application/json', 'outbot', 'http://example.invalid/i.png')",
        )
        .bind(id)
        .bind(team)
        .bind(channel)
        .bind(creator)
        .bind(delete_at)
        .execute(&pool)
        .await
        .expect("the outgoing hook row is written");
    }
}

fn incoming(id: &str) -> String {
    format!("/api/v4/hooks/incoming/{id}")
}
fn outgoing(id: &str) -> String {
    format!("/api/v4/hooks/outgoing/{id}")
}

/// Both single reads, byte for byte — **with** the trailing newline the two list routes in the
/// same Go file do not have.
#[tokio::test]
async fn both_single_hooks_are_byte_identical_and_newline_terminated() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let fixture = fixture(&client, &token).await;

    for p in [incoming(&fixture.incoming), outgoing(&fixture.outgoing)] {
        let (go, rs) = fetch_both_stable(&client, &token, &p).await;
        assert_eq!(
            String::from_utf8_lossy(&go),
            String::from_utf8_lossy(&rs),
            "{p}"
        );
        assert!(
            rs.ends_with(b"\n"),
            "{p}: `json.NewEncoder(w).Encode`, unlike the lists"
        );
    }

    // The outgoing hook's `StringArray` columns survive the single read too.
    let (go, _) = fetch_both_stable(&client, &token, &outgoing(&fixture.outgoing)).await;
    let hook: serde_json::Value = serde_json::from_slice(&go).expect("decodes");
    assert_eq!(hook["trigger_words"], serde_json::json!(["alpha"]));
    assert_eq!(hook["channel_id"].as_str().map(str::len), Some(26));
}

/// A soft-deleted hook is a **404**, not a hook with `delete_at` set — the store's `DeleteAt = 0`
/// is in the `WHERE`, so the row is simply not found.
#[tokio::test]
async fn a_deleted_hook_is_a_404_on_both_routes() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let fixture = fixture(&client, &token).await;

    for (p, id) in [
        (
            incoming(&fixture.incoming_deleted),
            "app.webhooks.get_incoming.app_error",
        ),
        (
            outgoing(&fixture.outgoing_deleted),
            "app.webhooks.get_outgoing.app_error",
        ),
    ] {
        let ((go_status, go), (rs_status, rs)) = fetch_both_raw(&client, &token, &p).await;
        assert_eq!(go_status, 404, "{p}");
        assert_eq!(rs_status, go_status, "{p}");
        let body = assert_error_bodies_match_except_known_gaps(&go, &rs, &p);
        assert_eq!(body["id"], id, "{p}: each route has its own not-found id");
    }
}

/// An id no row has is the same 404, so "deleted" and "never existed" are indistinguishable — the
/// property that keeps a hook id from being an oracle for whether a hook once existed.
#[tokio::test]
async fn an_unknown_id_is_the_same_404() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let fixture = fixture(&client, &token).await;

    for p in [incoming(&fixture.absent), outgoing(&fixture.absent)] {
        let ((go_status, go), (rs_status, rs)) = fetch_both_raw(&client, &token, &p).await;
        assert_eq!(go_status, 404, "{p}");
        assert_eq!(rs_status, go_status, "{p}");
        assert_error_bodies_match_except_known_gaps(&go, &rs, &p);
    }
}

/// **The channel check is on the incoming route only.** A team admin who is not a member of a
/// private channel is refused the incoming hook in it — and served the outgoing hook in the very
/// same channel, because `getOutgoingHook` never looks the channel up.
#[tokio::test]
async fn the_channel_check_is_incoming_only() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let fixture = fixture(&client, &token).await;

    let p = incoming(&fixture.incoming_private);
    let ((go_status, go), (rs_status, rs)) =
        fetch_both_raw(&client, &fixture.team_admin_token, &p).await;
    assert_eq!(
        go_status,
        403,
        "{p}: the hook's channel is private and the caller is not in it: {}",
        String::from_utf8_lossy(&go)
    );
    assert_eq!(rs_status, go_status, "{p}");
    let body = assert_error_bodies_match_except_known_gaps(&go, &rs, &p);
    assert_eq!(
        body["id"], "api.context.permissions.app_error",
        "{p}: and it is reported as a *webhook* permission failure"
    );

    let p = outgoing(&fixture.outgoing_private);
    let ((go_status, go), (rs_status, rs)) =
        fetch_both_raw(&client, &fixture.team_admin_token, &p).await;
    assert_eq!(
        go_status,
        200,
        "{p}: the same caller, the same channel, and no channel check: {}",
        String::from_utf8_lossy(&go)
    );
    assert_eq!(rs_status, go_status, "{p}");
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{p}"
    );
}

/// A public channel is not restricted, so the same team admin is served the incoming hook there —
/// which is what makes the refusal above about the channel and not about the caller.
#[tokio::test]
async fn a_public_channels_hook_is_served_to_the_same_caller() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let fixture = fixture(&client, &token).await;

    let p = incoming(&fixture.incoming);
    let ((go_status, go), (rs_status, rs)) =
        fetch_both_raw(&client, &fixture.team_admin_token, &p).await;
    assert_eq!(go_status, 200, "{p}: {}", String::from_utf8_lossy(&go));
    assert_eq!(rs_status, go_status, "{p}");
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{p}"
    );
}

/// Someone else's hook needs `manage_others_incoming_webhooks`; the team admin has it, so this
/// exercises the *second* gate passing rather than refusing.
#[tokio::test]
async fn another_users_hook_needs_the_others_permission() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let fixture = fixture(&client, &token).await;

    let p = incoming(&fixture.incoming_theirs);
    let ((go_status, go), (rs_status, rs)) =
        fetch_both_raw(&client, &fixture.team_admin_token, &p).await;
    assert_eq!(
        go_status,
        200,
        "{p}: a team admin holds manage_others: {}",
        String::from_utf8_lossy(&go)
    );
    assert_eq!(rs_status, go_status, "{p}");
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{p}"
    );
}

/// A plain user holds neither permission and is refused on both routes, whoever owns the hook.
#[tokio::test]
async fn a_plain_user_is_refused_on_both_routes() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let fixture = fixture(&client, &token).await;

    // The last two are hooks the plain user **owns**. They matter: a caller who owns the hook
    // passes the third gate, so only the second gate can refuse them — which is what makes the
    // two gates distinguishable at all, given that the permission name is wiped from the body.
    for p in [
        incoming(&fixture.incoming),
        outgoing(&fixture.outgoing),
        incoming(&fixture.incoming_theirs),
        outgoing(&fixture.outgoing_theirs),
    ] {
        let ((go_status, go), (rs_status, rs)) =
            fetch_both_raw(&client, &fixture.plain_token, &p).await;
        assert_eq!(go_status, 403, "{p}");
        assert_eq!(rs_status, go_status, "{p}");
        assert_error_bodies_match_except_known_gaps(&go, &rs, &p);
    }
}

/// A short alphanumeric segment is `RequireHookId`'s 400; a segment outside gorilla's charset
/// never routes in Go and is forwarded so Go answers its own 404.
#[tokio::test]
async fn a_short_id_is_a_400_and_a_non_mux_segment_is_forwarded() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;

    for p in [incoming("abc"), outgoing("abc")] {
        let ((go_status, go), (rs_status, rs)) = fetch_both_raw(&client, &token, &p).await;
        assert_eq!(go_status, 400, "{p}");
        assert_eq!(rs_status, go_status, "{p}");
        let body = assert_error_bodies_match_except_known_gaps(&go, &rs, &p);
        assert_eq!(body["id"], "api.context.invalid_url_param.app_error", "{p}");
    }

    for p in [incoming("not-an-id"), outgoing("not-an-id")] {
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

/// Registering the two `GET`s must not turn `DELETE` on the same paths into our 405.
///
/// **Repointed when `DELETE` was migrated.** The probe is now `PATCH`, which Go registers on
/// neither hook path, so it still exercises the method fallback — a method this server does not
/// register must reach Go rather than meet axum's 405.
///
/// Against the absent id, deliberately: a forwarded write is one Go *performs*, and an earlier
/// version of this test pointed at the fixture's own hook and soft-deleted it, so whichever test
/// ran next found a 404 where it expected a 200.
#[tokio::test]
async fn other_methods_are_forwarded() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let fixture = fixture(&client, &token).await;

    for p in [incoming(&fixture.absent), outgoing(&fixture.absent)] {
        let ours = client
            .patch(format!("{RUST}{p}"))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .expect("we answer");
        assert_eq!(
            ours.headers()
                .get("x-mmrs-served-by")
                .and_then(|v| v.to_str().ok()),
            Some("go"),
            "PATCH {p} must be forwarded"
        );
    }
}
