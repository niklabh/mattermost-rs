//! Cross-server parity for `GET /api/v4/hooks/outgoing` — `getOutgoingHooks`.
//!
//! ```sh
//! scripts/parity.sh -p mm-api --test parity outgoing_hooks
//! ```
//!
//! # Three scopes, and `channel_id` wins
//!
//! `channel_id` is checked first, then `team_id`, then neither — the branches are exclusive, so a
//! request carrying **both** is a channel request and the team is ignored. Each branch asks the
//! same pair of permissions at a different scope, and the fixture's **team admin** — who holds
//! them on one team and nowhere else — is the only caller that answers differently to the three.
//!
//! # `trigger_words` and `callback_urls` have three states
//!
//! They are `model.StringArray`, stored as a JSON array inside a `varchar`. `StringArray.Scan`
//! (model/utils.go:118) leaves the field **nil** for a SQL NULL and `json.Unmarshal`s anything
//! else, so `null`, `[]` and a populated array are three distinct answers. The fixture plants one
//! hook of each.

use crate::common;

use common::{
    RUST, assert_error_bodies_match_except_known_gaps, client, create_channel, create_plain_user,
    create_team, fetch_both_raw, fetch_both_stable, go_minted_token, purge_api_fixtures,
    stack_enabled,
};

const PATH: &str = "/api/v4/hooks/outgoing";

struct Fixture {
    team: String,
    other_team: String,
    channel: String,
    other_channel: String,
    /// The channel branch's answer, in order.
    in_channel: [String; 3],
    /// The team branch's answer, in order — the channel's three plus the other channel's one.
    in_team: [String; 4],
    deleted: String,
    other_team_hook: String,
    plain_token: String,
    team_admin_token: String,
}

static FIXTURE: tokio::sync::OnceCell<Fixture> = tokio::sync::OnceCell::const_new();

async fn fixture(client: &reqwest::Client, token: &str) -> &'static Fixture {
    FIXTURE
        .get_or_init(|| async {
            purge_api_fixtures().await;

            let team = create_team(client, token, "outhooks").await;
            let other_team = create_team(client, token, "outhooksother").await;
            let channel = create_channel(client, token, &team, "outhooks").await;
            let other_channel = create_channel(client, token, &team, "outhooks2").await;
            let far_channel = create_channel(client, token, &other_team, "outhooks3").await;

            let plain = create_plain_user(client, token, &team, "outhooks").await;
            let team_admin = create_plain_user(client, token, &team, "outhooksadm").await;
            promote_to_team_admin(client, token, &team, &team_admin.id).await;
            let admin = common::logged_in_user_id().to_owned();

            let id = |n: u32| format!("mmrsouth000000000000000{n:03}");
            let ids: Vec<String> = (1..=6).map(id).collect();
            for hook_id in &ids {
                assert_eq!(hook_id.len(), 26, "{hook_id} must be a valid id");
            }

            // `0002` is written **before** `0001` and the two share a display name, so a query
            // that dropped the `Id` half of `ORDER BY DisplayName, Id` falls back to the heap and
            // answers them the other way round.
            //
            // The three array states ride on the first three rows: NULL, `[]`, populated.
            plant(&[
                Row {
                    id: &ids[1],
                    team: &team,
                    channel: &channel,
                    creator: &admin,
                    display_name: "mmrs out alpha",
                    delete_at: 0,
                    trigger_words: Some("[]"),
                    callback_urls: Some("[]"),
                },
                Row {
                    id: &ids[0],
                    team: &team,
                    channel: &channel,
                    creator: &admin,
                    display_name: "mmrs out alpha",
                    delete_at: 0,
                    trigger_words: None,
                    callback_urls: None,
                },
                Row {
                    id: &ids[2],
                    team: &team,
                    channel: &channel,
                    creator: &plain.id,
                    display_name: "mmrs out beta",
                    delete_at: 0,
                    trigger_words: Some(r#"["alpha","beta"]"#),
                    callback_urls: Some(r#"["http://example.invalid/a"]"#),
                },
                Row {
                    id: &ids[3],
                    team: &team,
                    channel: &channel,
                    creator: &admin,
                    display_name: "mmrs out deleted",
                    delete_at: 1_788_636_490_000,
                    trigger_words: Some("[]"),
                    callback_urls: Some("[]"),
                },
                Row {
                    id: &ids[4],
                    team: &team,
                    channel: &other_channel,
                    creator: &admin,
                    display_name: "mmrs out otherchannel",
                    delete_at: 0,
                    trigger_words: Some("[]"),
                    callback_urls: Some("[]"),
                },
                Row {
                    id: &ids[5],
                    team: &other_team,
                    channel: &far_channel,
                    creator: &admin,
                    display_name: "mmrs out otherteam",
                    delete_at: 0,
                    trigger_words: Some("[]"),
                    callback_urls: Some("[]"),
                },
            ])
            .await;

            Fixture {
                team,
                other_team,
                channel,
                other_channel,
                in_channel: [ids[0].clone(), ids[1].clone(), ids[2].clone()],
                in_team: [
                    ids[0].clone(),
                    ids[1].clone(),
                    ids[2].clone(),
                    ids[4].clone(),
                ],
                deleted: ids[3].clone(),
                other_team_hook: ids[5].clone(),
                plain_token: plain.token,
                team_admin_token: team_admin.token,
            }
        })
        .await
}

struct Row<'a> {
    id: &'a str,
    team: &'a str,
    channel: &'a str,
    creator: &'a str,
    display_name: &'a str,
    delete_at: i64,
    /// `None` writes SQL NULL — the state that reaches the wire as `null`.
    trigger_words: Option<&'a str>,
    callback_urls: Option<&'a str>,
}

async fn promote_to_team_admin(
    client: &reqwest::Client,
    admin_token: &str,
    team_id: &str,
    user_id: &str,
) {
    let response = client
        .put(format!(
            "{}/api/v4/teams/{team_id}/members/{user_id}/schemeRoles",
            common::GO
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

/// Clear this suite's rows and write the given ones, in the order given.
async fn plant(rows: &[Row<'_>]) {
    let url = std::env::var("DATABASE_URL")
        .expect("DATABASE_URL must be set for the stack-backed suites; scripts/parity.sh sets it");
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(std::time::Duration::from_secs(5))
        .connect(&url)
        .await
        .expect("the shared database is reachable");

    sqlx::query("DELETE FROM outgoingwebhooks WHERE id LIKE 'mmrsouth%'")
        .execute(&pool)
        .await
        .expect("earlier rows are cleared");

    for row in rows {
        sqlx::query(
            "INSERT INTO outgoingwebhooks
                (id, token, createat, updateat, deleteat, creatorid, channelid, teamid,
                 triggerwords, triggerwhen, callbackurls, displayname, description,
                 contenttype, username, iconurl)
             VALUES ($1, 'mmrsouttoken00000000000001', 1788636490668, 1788636490669, $6,
                     $4, $3, $2, $7, 1, $8, $5, 'planted by the parity suite',
                     'application/json', 'outbot', 'http://example.invalid/i.png')",
        )
        .bind(row.id)
        .bind(row.team)
        .bind(row.channel)
        .bind(row.creator)
        .bind(row.display_name)
        .bind(row.delete_at)
        .bind(row.trigger_words)
        .bind(row.callback_urls)
        .execute(&pool)
        .await
        .expect("the hook row is written");
    }
}

fn ids_of(body: &[u8]) -> Vec<String> {
    serde_json::from_slice::<Vec<serde_json::Value>>(body)
        .unwrap_or_else(|e| panic!("decoding {}: {e}", String::from_utf8_lossy(body)))
        .into_iter()
        .map(|hook| hook["id"].as_str().expect("an id").to_owned())
        .collect()
}

/// The channel-scoped list, byte for byte, including the three `StringArray` states.
#[tokio::test]
async fn the_channel_scoped_list_is_byte_identical() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let fixture = fixture(&client, &token).await;

    let p = format!("{PATH}?channel_id={}", fixture.channel);
    let (go, rs) = fetch_both_stable(&client, &token, &p).await;

    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{p}: the list must be byte-identical"
    );
    assert!(
        !rs.ends_with(b"\n"),
        "`json.Marshal` + `w.Write` (webhook.go:572) — no encoder, no newline"
    );
    assert_eq!(ids_of(&go), fixture.in_channel.to_vec());
}

/// **`null`, `[]` and a populated array are three answers, not two.** A store that defaulted a
/// NULL column to `[]` would pass every other test in this file.
#[tokio::test]
async fn the_string_array_columns_keep_all_three_states() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let fixture = fixture(&client, &token).await;

    let p = format!("{PATH}?channel_id={}", fixture.channel);
    let (go, rs) = fetch_both_stable(&client, &token, &p).await;
    assert_eq!(go, rs, "{p}");

    let rows: Vec<serde_json::Value> = serde_json::from_slice(&go).expect("decodes");
    let by_id: std::collections::HashMap<&str, &serde_json::Value> = rows
        .iter()
        .map(|row| (row["id"].as_str().expect("an id"), row))
        .collect();

    let null_row = by_id[fixture.in_channel[0].as_str()];
    assert!(
        null_row["trigger_words"].is_null(),
        "a NULL column is `null`, not `[]`: {null_row}"
    );
    assert!(null_row["callback_urls"].is_null(), "{null_row}");

    let empty_row = by_id[fixture.in_channel[1].as_str()];
    assert_eq!(
        empty_row["trigger_words"],
        serde_json::json!([]),
        "an empty JSON array stays an empty array: {empty_row}"
    );

    let full_row = by_id[fixture.in_channel[2].as_str()];
    assert_eq!(
        full_row["trigger_words"],
        serde_json::json!(["alpha", "beta"]),
        "{full_row}"
    );
    assert_eq!(
        full_row["callback_urls"],
        serde_json::json!(["http://example.invalid/a"]),
        "{full_row}"
    );
    assert_eq!(full_row["trigger_when"], 1, "an `int` column, not a bool");
}

/// The team branch is wider than the channel branch by exactly the other channel's hook, and
/// still excludes the deleted one and the other team's.
#[tokio::test]
async fn the_team_scoped_list_spans_the_teams_channels() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let fixture = fixture(&client, &token).await;

    let p = format!("{PATH}?team_id={}", fixture.team);
    let (go, rs) = fetch_both_stable(&client, &token, &p).await;
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{p}"
    );
    assert_eq!(ids_of(&go), fixture.in_team.to_vec());

    let listed = ids_of(&go);
    assert!(
        !listed.contains(&fixture.deleted),
        "DeleteAt = 0: {listed:?}"
    );
    assert!(
        !listed.contains(&fixture.other_team_hook),
        "TeamId filter: {listed:?}"
    );
}

/// **`channel_id` wins.** A request carrying both is a channel request; the team is not consulted
/// at all, so naming a team the caller could not see changes nothing.
#[tokio::test]
async fn channel_id_takes_precedence_over_team_id() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let fixture = fixture(&client, &token).await;

    let both = format!(
        "{PATH}?channel_id={}&team_id={}",
        fixture.channel, fixture.other_team
    );
    let (go, rs) = fetch_both_stable(&client, &token, &both).await;
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{both}"
    );
    assert_eq!(
        ids_of(&go),
        fixture.in_channel.to_vec(),
        "the channel's list, not the other team's"
    );

    // And the other channel really is a different answer, so the assertion above is not vacuous.
    let other = format!("{PATH}?channel_id={}", fixture.other_channel);
    let (go_other, rs_other) = fetch_both_stable(&client, &token, &other).await;
    assert_eq!(ids_of(&go_other).len(), 1, "{other}");
    assert_eq!(ids_of(&rs_other), ids_of(&go_other), "{other}");
}

/// The unscoped list has no team or channel predicate, so it crosses both.
#[tokio::test]
async fn the_unscoped_list_crosses_teams() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let fixture = fixture(&client, &token).await;

    let (go, rs) = fetch_both_stable(&client, &token, PATH).await;
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{PATH}"
    );
    let listed = ids_of(&go);
    for planted in [&fixture.in_channel[0], &fixture.other_team_hook] {
        assert!(listed.contains(planted), "{planted} missing: {listed:?}");
    }
    assert!(!listed.contains(&fixture.deleted), "{listed:?}");
}

/// `page * per_page` is the offset, and the pages concatenate back into the team's list.
#[tokio::test]
async fn the_pages_split_the_list() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let fixture = fixture(&client, &token).await;

    let mut paged: Vec<String> = Vec::new();
    for page in 0..2 {
        let p = format!("{PATH}?team_id={}&page={page}&per_page=2", fixture.team);
        let (go, rs) = fetch_both_stable(&client, &token, &p).await;
        assert_eq!(
            String::from_utf8_lossy(&go),
            String::from_utf8_lossy(&rs),
            "{p}"
        );
        paged.extend(ids_of(&go));
    }
    assert_eq!(paged, fixture.in_team.to_vec());

    let p = format!("{PATH}?team_id={}&page=9&per_page=60", fixture.team);
    let (go, rs) = fetch_both_stable(&client, &token, &p).await;
    assert_eq!(go, b"[]", "an empty page is an empty array, not null");
    assert_eq!(rs, go, "{p}");
}

/// A user with neither permission is refused on **all three** branches, and the id names the
/// `manage_own` one.
#[tokio::test]
async fn a_user_without_the_permission_is_refused_on_every_branch() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let fixture = fixture(&client, &token).await;

    for p in [
        PATH.to_owned(),
        format!("{PATH}?team_id={}", fixture.team),
        format!("{PATH}?channel_id={}", fixture.channel),
    ] {
        let ((go_status, go), (rs_status, rs)) =
            fetch_both_raw(&client, &fixture.plain_token, &p).await;
        assert_eq!(go_status, 403, "{p}");
        assert_eq!(rs_status, go_status, "{p}");
        let body = assert_error_bodies_match_except_known_gaps(&go, &rs, &p);
        assert_eq!(body["id"], "api.context.permissions.app_error", "{p}");
    }
}

/// **The three scopes are three different questions.** A team admin passes the channel and team
/// branches on its own team and fails the system-scoped one — the same caller, three answers.
#[tokio::test]
async fn the_three_scopes_are_three_different_answers() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let fixture = fixture(&client, &token).await;

    for (p, expected) in [
        (format!("{PATH}?channel_id={}", fixture.channel), 200),
        (format!("{PATH}?team_id={}", fixture.team), 200),
        (PATH.to_owned(), 403),
        (format!("{PATH}?team_id={}", fixture.other_team), 403),
    ] {
        let ((go_status, go), (rs_status, rs)) =
            fetch_both_raw(&client, &fixture.team_admin_token, &p).await;
        assert_eq!(go_status, expected, "{p}: {}", String::from_utf8_lossy(&go));
        assert_eq!(rs_status, go_status, "{p}");
        if expected == 200 {
            assert_eq!(
                String::from_utf8_lossy(&go),
                String::from_utf8_lossy(&rs),
                "{p}"
            );
        } else {
            // An error body cannot be byte-compared: `message` is Go's translated prose against
            // our raw id, and `request_id` is per-request ([D-092]).
            assert_error_bodies_match_except_known_gaps(&go, &rs, &p);
        }
    }
}

/// Registering the `GET` must not turn the neighbouring `POST` into our 405.
/// **Repointed when `POST` was migrated.** The point of this test is that a method this server
/// does not register still reaches Go rather than meeting axum's 405 — not that any particular
/// method is unmigrated. `POST` is now served here, so the probe is `PATCH`, which Go does not
/// register on this path either (it answers its own 404).
#[tokio::test]
async fn other_methods_are_forwarded() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;

    let ours = client
        .patch(format!("{RUST}{PATH}"))
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
        "PATCH {PATH} must be forwarded"
    );
}
