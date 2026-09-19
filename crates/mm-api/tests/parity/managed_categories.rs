//! Cross-server parity for `GET /api/v4/teams/{team_id}/channels/managed_categories` —
//! `getManagedCategories` (api4/channel.go:3302) and `App.GetVisibleManagedCategoryMappings`
//! (app/channel_category.go:342).
//!
//! ```sh
//! scripts/parity.sh -p mm-api --test parity managed_categories
//! ```
//!
//! # Three pairs, because the route exists only behind an environment-only flag
//!
//! Go registers the route only while `FeatureFlags.ManagedChannelCategories` is on, and the flag
//! is off everywhere on the stack but the two oracles `scripts/go-licensed.sh` starts as
//! `managedcat` (licensed) and `managedcat-unlicensed` (the stock binary): see
//! [`common::managed_categories`]. The stack's own pair is the flag-off answer — the mux 404,
//! forwarded — and `channel_search_all::the_managed_categories_route_is_a_404_on_both` already
//! pins it for an authenticated caller.
//!
//! # What the planted values have to discriminate
//!
//! The values are planted by SQL (no REST route writes this group), each on a channel chosen so
//! that one predicate or branch decides whether it appears:
//!
//! | channel | value | appears? | because |
//! |---|---|---|---|
//! | `a` (public, member) | `"Zeta <&> Team"` | yes | and the encoder HTML-escapes it |
//! | `b` (private, member) | `"alpha"` | yes | |
//! | `c` (member) | `42` | **no** | `json.Unmarshal` into a string fails → `continue` |
//! | `d` (member) | `null` | **yes, as `""`** | Unmarshal of `null` is not an error |
//! | `e` (archived, member) | `"archived"` | no | default `ChannelSearchOpts` drop archived |
//! | `n` (private, **not** a member) | `"hidden"` | no | the target ids are the caller's channels |
//! | the DM | `"direct"` | yes — **on any team** | `TeamId = ''` is in the channel query |
//! | town square | `"deleted"`, `DeleteAt > 0` | no | `DeleteAt = 0` |
//! | town square | `"wrongfield"` in another field | no | the `FieldID` filter |
//! | off-topic | `"wronggroup"` in the boards group | no | the `GroupID` filter |
//!
//! Four values survive for one user, so a `PerPage` of anything under four truncates.

use crate::common;

use common::{
    ACTIVE_LICENCE_ROW, GO, LicensedPair, RUST, add_user_to_channel,
    assert_error_bodies_match_except_known_gaps, client, create_channel, create_channel_typed,
    create_direct_channel, create_plain_user, create_team, delete_channel, fetch_licensed_pair,
    fixture_pool, go_minted_token, logged_in_user_id, managed_categories, stack_enabled,
};

/// Every value this suite plants has an id under this prefix.
const PREFIX: &str = "mmrsmanagedcat";

fn value_id(suffix: &str) -> String {
    let id = format!("{PREFIX}{suffix:0>12}");
    assert_eq!(id.len(), 26, "{id}");
    id
}

fn path(team_id: &str) -> String {
    format!("/api/v4/teams/{team_id}/channels/managed_categories")
}

struct Fixture {
    team: String,
    /// A second team only `plain2` is in.
    team2: String,
    plain_token: String,
    /// In `team2` only, with no DM: in no channel at all on `team`.
    plain2_token: String,
    a: String,
    b: String,
    d: String,
    dm: String,
}

static FIXTURE: tokio::sync::OnceCell<Fixture> = tokio::sync::OnceCell::const_new();

async fn purge_values(pool: &sqlx::PgPool) {
    sqlx::query("DELETE FROM propertyvalues WHERE id LIKE $1")
        .bind(format!("{PREFIX}%"))
        .execute(pool)
        .await
        .expect("the planted values are purged");
}

async fn fixture(client: &reqwest::Client, token: &str) -> &'static Fixture {
    FIXTURE
        .get_or_init(|| async {
            let team = create_team(client, token, "managedcat").await;
            let team2 = create_team(client, token, "managedcat2").await;
            let plain = create_plain_user(client, token, &team, "managedcat").await;
            let plain2 = create_plain_user(client, token, &team2, "managedcat2").await;

            let a = create_channel(client, token, &team, "managedcat-a").await;
            let b = create_channel_typed(client, token, &team, "managedcat-b", "P").await;
            let c = create_channel(client, token, &team, "managedcat-c").await;
            let d = create_channel(client, token, &team, "managedcat-d").await;
            let e = create_channel(client, token, &team, "managedcat-e").await;
            let n = create_channel_typed(client, token, &team, "managedcat-n", "P").await;
            for channel in [&a, &b, &c, &d, &e] {
                add_user_to_channel(client, token, channel, &plain.id).await;
            }
            delete_channel(client, token, &e).await;
            let dm = create_direct_channel(client, token, &plain.id, logged_in_user_id()).await;

            let pool = fixture_pool()
                .await
                .expect("the shared database is reachable");
            let (town_square, off_topic): (String, String) = sqlx::query_as(
                "SELECT (SELECT id FROM channels WHERE teamid = $1 AND name = 'town-square'),
                        (SELECT id FROM channels WHERE teamid = $1 AND name = 'off-topic')",
            )
            .bind(&team)
            .fetch_one(&pool)
            .await
            .expect("the default channels exist");
            let (group, field, boards): (String, String, String) = sqlx::query_as(
                "SELECT g.id, f.id, (SELECT id FROM propertygroups WHERE name = 'boards')
                   FROM propertygroups g
                   JOIN propertyfields f ON f.groupid = g.id
                  WHERE g.name = 'managed_channel_categories'
                    AND f.name = 'category_name' AND f.objecttype = 'channel'
                    AND f.targetid = '' AND f.deleteat = 0",
            )
            .fetch_one(&pool)
            .await
            .expect("Go's migration created the managed-category group and field");

            purge_values(&pool).await;
            let plants: [(&str, &str, &str, &str, serde_json::Value, i64); 10] = [
                ("a", &a, &group, &field, "Zeta <&> Team".into(), 0),
                ("b", &b, &group, &field, "alpha".into(), 0),
                ("c", &c, &group, &field, 42.into(), 0),
                ("d", &d, &group, &field, serde_json::Value::Null, 0),
                ("e", &e, &group, &field, "archived".into(), 0),
                ("n", &n, &group, &field, "hidden".into(), 0),
                ("dm", &dm, &group, &field, "direct".into(), 0),
                ("ts", &town_square, &group, &field, "deleted".into(), 1),
                (
                    "tsfield",
                    &town_square,
                    &group,
                    "mmrsmanagedcatnotafield000",
                    "wrongfield".into(),
                    0,
                ),
                ("ot", &off_topic, &boards, &field, "wronggroup".into(), 0),
            ];
            for (i, (suffix, target, group_id, field_id, value, delete_at)) in
                plants.into_iter().enumerate()
            {
                let at = 1_767_225_600_000_i64 + i64::try_from(i).unwrap();
                sqlx::query(
                    "INSERT INTO propertyvalues
                         (id, targetid, targettype, groupid, fieldid, value,
                          createat, updateat, deleteat, createdby, updatedby)
                     VALUES ($1, $2, 'channel', $3, $4, $5, $6, $6, $7, '', '')",
                )
                .bind(value_id(suffix))
                .bind(target)
                .bind(group_id)
                .bind(field_id)
                .bind(value)
                .bind(at)
                .bind(delete_at)
                .execute(&pool)
                .await
                .expect("the value is planted");
            }

            Fixture {
                team,
                team2,
                plain_token: plain.token,
                plain2_token: plain2.token,
                a,
                b,
                d,
                dm,
            }
        })
        .await
}

/// Both servers of `pair` answer `path` with the same status and **the same bytes**.
async fn same_bytes(
    client: &reqwest::Client,
    pair: &LicensedPair,
    token: Option<&str>,
    path: &str,
) -> (u16, Vec<u8>) {
    let ((go_status, go), (rs_status, rs)) = fetch_licensed_pair(client, pair, token, path).await;
    assert_eq!(
        rs_status,
        go_status,
        "{path}: Go said {}, we said {}",
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs)
    );
    if go_status == 200 {
        assert_eq!(
            String::from_utf8_lossy(&rs),
            String::from_utf8_lossy(&go),
            "{path}"
        );
    } else {
        assert_error_bodies_match_except_known_gaps(&go, &rs, path);
    }
    (go_status, go)
}

/// The mapping for a member: the four survivors of the table in the module docs, keys sorted,
/// `<&>` escaped, `null` read as `""`, a trailing newline — and the same bytes from both servers.
#[tokio::test]
async fn the_mapping_holds_exactly_the_members_decodable_live_values() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;
    let (licensed, _) = managed_categories().await;

    let p = path(&f.team);
    let (status, body) = same_bytes(&client, &licensed, Some(&f.plain_token), &p).await;
    assert_eq!(status, 200, "{p}: {}", String::from_utf8_lossy(&body));

    let mut expected = std::collections::BTreeMap::new();
    expected.insert(f.a.as_str(), "Zeta \\u003c\\u0026\\u003e Team");
    expected.insert(f.b.as_str(), "alpha");
    expected.insert(f.d.as_str(), "");
    expected.insert(f.dm.as_str(), "direct");
    let entries: Vec<String> = expected
        .iter()
        .map(|(k, v)| format!("\"{k}\":\"{v}\""))
        .collect();
    assert_eq!(
        String::from_utf8_lossy(&body),
        format!("{{{}}}\n", entries.join(",")),
        "{p}"
    );
}

/// No team check at all: on a team the caller is not in — here one that does not exist — the
/// answer is the caller's DMs and GMs, because the channel query keeps `TeamId = ''`.
#[tokio::test]
async fn a_team_the_caller_is_not_in_still_answers_its_direct_channels() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;
    let (licensed, _) = managed_categories().await;

    for team in [f.team2.as_str(), "zzzzzzzzzzzzzzzzzzzzzzzzzz"] {
        let p = path(team);
        let (status, body) = same_bytes(&client, &licensed, Some(&f.plain_token), &p).await;
        assert_eq!(status, 200, "{p}");
        assert_eq!(
            String::from_utf8_lossy(&body),
            format!("{{\"{}\":\"direct\"}}\n", f.dm),
            "{p}"
        );
    }
}

/// Both empty answers are `{}` and a newline: a caller in **no** channel on the team (the store's
/// 404, swallowed) and a caller whose channels carry no value (the search finding nothing).
#[tokio::test]
async fn no_channels_and_no_values_are_both_an_empty_object() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;
    let (licensed, _) = managed_categories().await;

    for team in [f.team.as_str(), f.team2.as_str()] {
        let p = path(team);
        let (status, body) = same_bytes(&client, &licensed, Some(&f.plain2_token), &p).await;
        assert_eq!(status, 200, "{p}: {}", String::from_utf8_lossy(&body));
        assert_eq!(body, b"{}\n", "{p}");
    }
}

/// The gates, in Go's order, on both flag-on pairs: the session, then `RequireTeamId`, then —
/// unlicensed only — the licence, at **501**.
#[tokio::test]
async fn the_session_then_the_team_id_then_the_licence() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;
    let (licensed, unlicensed) = managed_categories().await;

    for (name, pair) in [("licensed", &licensed), ("unlicensed", &unlicensed)] {
        // No session: 401, whatever the id and whatever the licence.
        for team in ["tooshort", f.team.as_str()] {
            let p = path(team);
            let (status, body) = same_bytes(&client, pair, None, &p).await;
            assert_eq!(status, 401, "{name} {p}");
            let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(
                body["id"], "api.context.session_expired.app_error",
                "{name}"
            );
        }
        // A mux-shaped id that is not 26 characters: 400 `team_id`, ahead of the licence.
        let p = path("tooshort");
        let (status, body) = same_bytes(&client, pair, Some(&f.plain_token), &p).await;
        assert_eq!(status, 400, "{name} {p}");
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            body["id"], "api.context.invalid_url_param.app_error",
            "{name}"
        );
    }

    let p = path(&f.team);
    let (status, body) = same_bytes(&client, &unlicensed, Some(&f.plain_token), &p).await;
    assert_eq!(status, 501, "{p}: {}", String::from_utf8_lossy(&body));
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["id"], "api.license_error");
}

/// With the flag off — the stack's own pair — the route does not exist and the request is
/// forwarded **before** the session is checked, so an anonymous caller gets gorilla's 404 and not
/// our 401.
#[tokio::test]
async fn with_the_flag_off_even_an_anonymous_caller_gets_the_mux_404() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let p = path(&f.team);
    for base in [GO, RUST] {
        let response = client
            .get(format!("{base}{p}"))
            .send()
            .await
            .expect("reachable");
        let served_by = response
            .headers()
            .get("x-mmrs-served-by")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        assert_eq!(response.status().as_u16(), 404, "{base}{p}");
        if base == RUST {
            assert_eq!(served_by.as_deref(), Some("go"), "{p} must be forwarded");
        }
        let body: serde_json::Value = response.json().await.expect("JSON");
        assert_eq!(body["id"], "api.context.404.app_error", "{base}{p}");
    }
}
