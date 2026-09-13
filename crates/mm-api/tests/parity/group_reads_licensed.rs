//! Store-versus-oracle parity for the three group list reads behind
//! `App.getGroupsAllowedForReferenceInChannel` (app/notification.go:1498), and for that function.
//!
//! ```sh
//! scripts/parity.sh -p mm-api --test parity group_reads_licensed
//! ```
//!
//! # What is compared
//!
//! `mm_store::GroupStore::{get_groups, get_groups_by_channel, get_groups_by_team}` are called on
//! the stack's Postgres and their results, serialised, are compared with what the **licensed Go
//! oracle** (`common::licensed`) answers for `GET /api/v4/groups`, `GET /channels/{id}/groups`
//! and `GET /teams/{id}/groups` — whose handlers (api4/group.go:1123, :900, :961) read exactly
//! these three store methods. Every `GroupSearchOpts` branch Go's builders have is put to both
//! with the query string that sets it, and the answer must be the same rows, in the same order,
//! with the same counts. Nothing here is read off the Go source and asserted on its own.
//!
//! `mm_app::App::get_groups_allowed_for_reference_in_channel` has no route of its own; its four
//! shapes are compared with the union the Go routes give for the same two store calls.
//!
//! # The comparison is over this suite's own groups
//!
//! Other suites in this binary create and delete groups while these run, so an unfiltered list
//! would differ by churn. Both sides are filtered to the fixture — by name prefix for the groups
//! Go created, by id prefix for the planted ones — which keeps the relative order and every count
//! and loses nothing the fixture asserts. The searches that page use `q` to pin the set instead.
//!
//! # Fixtures
//!
//! Custom groups are created through the licensed Go server (`POST /groups`), so each is Go's
//! own row; the LDAP and plugin-sourced ones are planted (no LDAP on this stack, as every
//! licensed group suite notes), and so are the `GroupTeams`/`GroupChannels` links. One planted
//! group has a **null name**, which is the branch the app function's `Name != nil` filter exists
//! for. Users are `create_plain_user`s; one is deactivated, one has a membership soft-deleted,
//! one lives in a second team, one is demoted to a guest on the licensed guest oracle.

use std::collections::BTreeMap;

use crate::common;
use common::{
    ACTIVE_LICENCE_ROW, GO, LicensedPair, add_user_to_channel, client, create_channel,
    create_plain_user, create_team, go_minted_token, invalidate_licensed_go_caches, licensed,
    licensed_guest, patch_user_timezone, stack_enabled,
};
use mm_app::App;
use mm_model::channel::Channel;
use mm_model::group::{GroupSearchOpts, GroupSource, PageOpts};
use mm_model::team::Team;
use mm_model::user::ViewUsersRestrictions;
use mm_store::{GroupStore, SqlStore};

const PREFIX: &str = "mmrsgrpread";

// Planted ids: the prefix plus fifteen characters, 26 in all — asserted at compile time, since a
// 25-character literal reads as one.
const ECHO: &str = "mmrsgrpreadecho00000000001";
const FOXTROT: &str = "mmrsgrpreadfoxtrot00000001";
const GOLF: &str = "mmrsgrpreadgolf00000000001";
const HOTEL: &str = "mmrsgrpreadhotel0000000001";
const INDIA: &str = "mmrsgrpreadindia0000000001";
const JULIET: &str = "mmrsgrpreadjuliet000000001";
const _: () = assert!(
    ECHO.len() == 26
        && FOXTROT.len() == 26
        && GOLF.len() == 26
        && HOTEL.len() == 26
        && INDIA.len() == 26
        && JULIET.len() == 26
);

struct Fixture {
    pair: LicensedPair,
    admin: String,
    /// The team the plain users live in, and its two channels: `channel` carries the group
    /// links, `counted` the memberships for the per-channel counts.
    team: String,
    channel: String,
    counted: String,
    /// A second team, `groupconstrained = true` by SQL, with one channel and one live link.
    constrained_team: String,
    constrained_channel: String,
    /// Go's ids for the custom groups, by tag.
    custom: BTreeMap<&'static str, String>,
    /// A plain user in `team`, in every custom group and in `counted`.
    u1: String,
    /// A plain user whose membership of `alpha` was deleted and whose only live one is `kilo`.
    u3: String,
    /// A guest — demoted on the licensed guest oracle — in `team` and `counted`, whose channel
    /// memberships are the `ViewUsersRestrictions` Go builds for it.
    guest_id: String,
    guest_token: String,
}

static FIXTURE: tokio::sync::OnceCell<Fixture> = tokio::sync::OnceCell::const_new();

/// A store on a pool of this test's own. **Not part of the fixture**: every `#[tokio::test]` is
/// its own runtime, and a pool opened in the first test's runtime is shut down with it — the
/// second test to use it got "a Tokio 1.x context was found, but it is being shutdown" from
/// the driver. Measured, in this suite's first green run.
async fn store() -> SqlStore {
    SqlStore::from_pool(pool().await)
}

async fn pool() -> sqlx::PgPool {
    let url = std::env::var("DATABASE_URL").expect("the parity stack exports DATABASE_URL");
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .acquire_timeout(std::time::Duration::from_secs(5))
        .connect(&url)
        .await
        .expect("the shared database is reachable")
}

/// Everything this suite planted or Go made for it, members and links first. Runs once, before
/// the fixture is built, so an aborted run leaves nothing a second run trips on.
async fn purge(pool: &sqlx::PgPool) {
    let like = format!("{PREFIX}%");
    for statement in [
        "DELETE FROM groupmembers WHERE groupid IN (SELECT id FROM usergroups WHERE id LIKE $1 OR name LIKE $1)",
        "DELETE FROM groupteams WHERE groupid IN (SELECT id FROM usergroups WHERE id LIKE $1 OR name LIKE $1)",
        "DELETE FROM groupchannels WHERE groupid IN (SELECT id FROM usergroups WHERE id LIKE $1 OR name LIKE $1)",
        "DELETE FROM usergroups WHERE id LIKE $1 OR name LIKE $1",
    ] {
        sqlx::query(statement)
            .bind(&like)
            .execute(pool)
            .await
            .expect("the sweep runs");
    }
}

async fn send(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    method: reqwest::Method,
    path: &str,
    body: Option<serde_json::Value>,
) -> (u16, serde_json::Value) {
    let mut request = client
        .request(method, format!("{base}{path}"))
        .header("Authorization", format!("Bearer {token}"));
    if let Some(body) = body {
        request = request.json(&body);
    }
    let response = request
        .send()
        .await
        .unwrap_or_else(|e| panic!("{base}{path} is unreachable: {e}"));
    let status = response.status().as_u16();
    let bytes = response.bytes().await.expect("a body");
    let value = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| serde_json::Value::String(String::from_utf8_lossy(&bytes).into()));
    (status, value)
}

/// A custom group through the licensed Go server: `name` is `mmrsgrpread-<tag>`, the display
/// name is what the ordering tests sort on, so it is chosen, not derived.
async fn create_custom(
    client: &reqwest::Client,
    pair: &LicensedPair,
    admin: &str,
    tag: &str,
    display: &str,
    allow_reference: bool,
    user_ids: &[&str],
) -> serde_json::Value {
    let (status, body) = send(
        client,
        &pair.go,
        admin,
        reqwest::Method::POST,
        "/api/v4/groups",
        Some(serde_json::json!({
            "name": format!("{PREFIX}-{tag}"),
            "display_name": display,
            "description": format!("group_reads_licensed {tag}"),
            "source": "custom",
            "allow_reference": allow_reference,
            "user_ids": user_ids,
        })),
    )
    .await;
    assert_eq!(status, 201, "creating {tag}: {body}");
    body
}

/// A group of a source the API cannot create here, straight into `UserGroups`. `name: None`
/// plants the null the app function filters on.
async fn plant_group(
    pool: &sqlx::PgPool,
    id: &str,
    name: Option<&str>,
    display: &str,
    source: &str,
    allow_reference: bool,
) {
    sqlx::query(
        "INSERT INTO usergroups
           (id, name, displayname, description, source, remoteid, createat, updateat, deleteat,
            allowreference)
         VALUES ($1, $2, $3, 'planted by group_reads_licensed', $4, $5, 1700000000000,
                 1700000000000, 0, $6)",
    )
    .bind(id)
    .bind(name.map(|n| format!("{PREFIX}-{n}")))
    .bind(display)
    .bind(source)
    .bind(format!("remote-{id}"))
    .bind(allow_reference)
    .execute(pool)
    .await
    .expect("the group is planted");
}

async fn plant_member(pool: &sqlx::PgPool, group: &str, user: &str) {
    sqlx::query(
        "INSERT INTO groupmembers (groupid, userid, createat, deleteat)
         VALUES ($1, $2, 1700000000000, 0)",
    )
    .bind(group)
    .bind(user)
    .execute(pool)
    .await
    .expect("the membership is planted");
}

/// A `GroupTeams` or `GroupChannels` row. `scheme_admin: None` leaves the column null, which
/// Go's `ToModel` turns into `false`; `delete_at` non-zero is an unlinked syncable.
async fn plant_link(
    pool: &sqlx::PgPool,
    table: &str,
    group: &str,
    syncable: &str,
    scheme_admin: Option<bool>,
    delete_at: i64,
) {
    let column = if table == "groupteams" {
        "teamid"
    } else {
        "channelid"
    };
    sqlx::query(&format!(
        "INSERT INTO {table} (groupid, {column}, autoadd, schemeadmin, createat, updateat, deleteat)
         VALUES ($1, $2, true, $3, 1700000000000, 1700000000000, $4)"
    ))
    .bind(group)
    .bind(syncable)
    .bind(scheme_admin)
    .bind(delete_at)
    .execute(pool)
    .await
    .expect("the link is planted");
}

async fn fixture() -> &'static Fixture {
    FIXTURE
        .get_or_init(|| async {
            let client = client();
            let pair = licensed().await;
            let guest_pair = licensed_guest().await;
            let admin = go_minted_token(&client).await;
            let pool = pool().await;
            purge(&pool).await;

            let team = create_team(&client, &admin, "grpread").await;
            let channel = create_channel(&client, &admin, &team, "grpread-links").await;
            let counted = create_channel(&client, &admin, &team, "grpread-counted").await;
            let constrained_team = create_team(&client, &admin, "grpread-ct").await;
            let constrained_channel =
                create_channel(&client, &admin, &constrained_team, "grpread-cc").await;
            // u1: in every custom group, in `counted`, automatic timezone (the default, whose
            // `automaticTimezone` is empty and so counts for nothing).
            // u2: a member of `alpha`, then deactivated — `Users.DeleteAt = 0` drops it.
            // u3: a member of `alpha` whose membership is then deleted — `GroupMembers.DeleteAt`
            //     — and a live member of `kilo`, so `filter_has_member` has one of each to tell
            //     apart.
            // u4: a member of `kilo`, in `counted`, with a manual timezone.
            // u5: in `counted` and in no group — `COUNT(ChannelMembers.UserId)` must not see it.
            // u7: a member of `alpha` in the *other* team, so the guest never shares a channel
            //     with it and the restricted member count is one less than the admin's.
            let u1 = create_plain_user(&client, &admin, &team, "grpread1")
                .await
                .id;
            let u2 = create_plain_user(&client, &admin, &team, "grpread2")
                .await
                .id;
            let u3 = create_plain_user(&client, &admin, &team, "grpread3")
                .await
                .id;
            let u4 = create_plain_user(&client, &admin, &team, "grpread4")
                .await
                .id;
            let u5 = create_plain_user(&client, &admin, &team, "grpread5")
                .await
                .id;
            let u7 = create_plain_user(&client, &admin, &constrained_team, "grpread7")
                .await
                .id;
            let guest = create_plain_user(&client, &admin, &team, "grpreadg").await;
            for user in [&u1, &u4, &u5, &guest.id] {
                add_user_to_channel(&client, &admin, &counted, user).await;
            }
            patch_user_timezone(&client, &admin, &u4, "false", "", "Asia/Kolkata").await;
            // After u7 joined it: Go refuses to add a user to a group-managed team.
            sqlx::query("UPDATE teams SET groupconstrained = true WHERE id = $1")
                .bind(&constrained_team)
                .execute(&pool)
                .await
                .expect("the team is constrained");

            // Display names decide the order and are chosen so that it differs from creation
            // order, from id order and from name order: `delta` (A) sorts first, `bravo` (A,
            // excluded by AllowReference) next, `alpha` (B) third.
            let mut custom = BTreeMap::new();
            for (tag, display, allow, members) in [
                (
                    "alpha",
                    "Grpread B alpha",
                    true,
                    vec![u1.as_str(), &u2, &u3, &u7],
                ),
                ("bravo", "Grpread A bravo", true, vec![u1.as_str()]),
                ("charlie", "Grpread C charlie", true, vec![u1.as_str()]),
                ("delta", "Grpread A delta", true, vec![]),
                ("kilo", "Grpread J kilo", true, vec![u1.as_str(), &u3, &u4]),
            ] {
                let created =
                    create_custom(&client, &pair, &admin, tag, display, allow, &members).await;
                custom.insert(tag, created["id"].as_str().expect("an id").to_owned());
            }
            // A custom group must be referenceable — `createGroup` and `patchGroup` both refuse
            // `allow_reference: false` (api4/group.go:187, :262) — so bravo's flag is flipped on
            // Go's own row; the store reads the column, not the API.
            sqlx::query("UPDATE usergroups SET allowreference = false WHERE id = $1")
                .bind(&custom["bravo"])
                .execute(&pool)
                .await
                .expect("bravo is unreferenceable");
            // The membership and user states above, through the same server that made them.
            let (status, body) = send(
                &client,
                &pair.go,
                &admin,
                reqwest::Method::DELETE,
                &format!("/api/v4/groups/{}/members", custom["alpha"]),
                Some(serde_json::json!({ "user_ids": [u3] })),
            )
            .await;
            assert_eq!(status, 200, "u3 leaves alpha: {body}");
            let (status, body) = send(
                &client,
                GO,
                &admin,
                reqwest::Method::DELETE,
                &format!("/api/v4/users/{u2}"),
                None,
            )
            .await;
            assert_eq!(status, 200, "u2 is deactivated: {body}");
            let (status, body) = send(
                &client,
                &pair.go,
                &admin,
                reqwest::Method::DELETE,
                &format!("/api/v4/groups/{}", custom["charlie"]),
                None,
            )
            .await;
            assert_eq!(status, 200, "charlie is archived: {body}");

            // The planted sources. `india`'s display name carries an underscore and `juliet`'s
            // an `X` in the same place, for the `q` escaping; `juliet`'s *source* has a `z`
            // where `plugin_` has its underscore, for the unescaped `LIKE 'plugin_%'`.
            plant_group(&pool, ECHO, Some("echo"), "Grpread D echo", "ldap", true).await;
            plant_group(&pool, FOXTROT, None, "Grpread E foxtrot", "ldap", true).await;
            plant_group(&pool, GOLF, Some("golf"), "Grpread F golf", "ldap", false).await;
            plant_group(&pool, HOTEL, Some("hotel"), "Grpread G hotel", "ldap", true).await;
            plant_group(
                &pool,
                INDIA,
                Some("india"),
                "Grpread H india_plug",
                "plugin_x",
                true,
            )
            .await;
            plant_group(
                &pool,
                JULIET,
                Some("juliet"),
                "Grpread I julietXplug",
                "pluginzz",
                true,
            )
            .await;
            plant_member(&pool, ECHO, &u1).await;
            plant_member(&pool, FOXTROT, &u4).await;

            // Links to `channel`: echo (admin), foxtrot, golf (null scheme admin), hotel
            // (unlinked). Links to `team`: echo, foxtrot, hotel live; golf unlinked. The
            // constrained team links india only.
            plant_link(&pool, "groupchannels", ECHO, &channel, Some(true), 0).await;
            plant_link(&pool, "groupchannels", FOXTROT, &channel, Some(false), 0).await;
            plant_link(&pool, "groupchannels", GOLF, &channel, None, 0).await;
            plant_link(
                &pool,
                "groupchannels",
                HOTEL,
                &channel,
                Some(true),
                1700000000001,
            )
            .await;
            plant_link(&pool, "groupteams", ECHO, &team, Some(false), 0).await;
            plant_link(&pool, "groupteams", FOXTROT, &team, Some(true), 0).await;
            plant_link(&pool, "groupteams", HOTEL, &team, None, 0).await;
            plant_link(&pool, "groupteams", GOLF, &team, Some(true), 1700000000001).await;
            plant_link(
                &pool,
                "groupteams",
                INDIA,
                &constrained_team,
                Some(false),
                0,
            )
            .await;

            // The guest: demoted on the oracle whose `GuestAccountsSettings.Enable` is on, and
            // logged in there, since a guest login is refused where the setting is off.
            let (status, body) = send(
                &client,
                &guest_pair.go,
                &admin,
                reqwest::Method::POST,
                &format!("/api/v4/users/{}/demote", guest.id),
                None,
            )
            .await;
            assert_eq!(status, 200, "the guest is demoted: {body}");
            let login = client
                .post(format!("{}/api/v4/users/login", guest_pair.go))
                .json(&serde_json::json!({
                    "login_id": common::plain_username("grpreadg"),
                    "password": common::PLAIN_USER_PASSWORD,
                }))
                .send()
                .await
                .expect("the guest oracle answers");
            assert_eq!(login.status(), 200, "the guest logs in");
            let guest_token = login
                .headers()
                .get("token")
                .expect("a token header")
                .to_str()
                .expect("ASCII")
                .to_owned();

            // The planted rows and the SQL flag bypassed both oracles' caches.
            invalidate_licensed_go_caches(&client, &pair, &admin).await;
            invalidate_licensed_go_caches(&client, &guest_pair, &admin).await;

            Fixture {
                pair,
                admin,
                team,
                channel,
                counted,
                constrained_team,
                constrained_channel,
                custom,
                u1,
                u3,
                guest_id: guest.id,
                guest_token,
            }
        })
        .await
}

fn is_fixture(group: &serde_json::Value) -> bool {
    let starts = |key: &str| group[key].as_str().is_some_and(|s| s.starts_with(PREFIX));
    starts("id") || starts("name")
}

fn fixture_only(groups: Vec<serde_json::Value>) -> Vec<serde_json::Value> {
    groups.into_iter().filter(is_fixture).collect()
}

fn tags(groups: &[serde_json::Value]) -> Vec<String> {
    groups
        .iter()
        .map(|g| {
            g["name"]
                .as_str()
                .map(|n| {
                    n.trim_start_matches(PREFIX)
                        .trim_start_matches('-')
                        .to_owned()
                })
                .unwrap_or_else(|| format!("<nameless {}>", g["id"].as_str().unwrap_or("")))
        })
        .collect()
}

/// `GET /api/v4/groups?<query>` from the licensed oracle, fixture rows only.
async fn go_groups(token: &str, base: &str, query: &str) -> Vec<serde_json::Value> {
    let client = client();
    let (status, body) = send(
        &client,
        base,
        token,
        reqwest::Method::GET,
        &format!("/api/v4/groups?{query}"),
        None,
    )
    .await;
    assert_eq!(status, 200, "GET /groups?{query}: {body}");
    fixture_only(body.as_array().expect("a list").clone())
}

/// The store's answer for the same options, as the wire would carry it.
async fn rs_groups(
    store: &SqlStore,
    page: i64,
    per_page: i64,
    opts: &GroupSearchOpts,
    restrictions: Option<&ViewUsersRestrictions>,
) -> Vec<serde_json::Value> {
    let groups = store
        .group()
        .get_groups(page, per_page, opts, restrictions)
        .await
        .expect("the store answers");
    fixture_only(
        groups
            .iter()
            .map(|g| serde_json::to_value(g).expect("a group serialises"))
            .collect(),
    )
}

/// One `get_groups` case: the Go query string and the options it sets must agree, row for row.
async fn same_groups(
    fx: &Fixture,
    store: &SqlStore,
    query: &str,
    page: i64,
    per_page: i64,
    opts: &GroupSearchOpts,
) {
    let go = go_groups(&fx.admin, &fx.pair.go, query).await;
    let rs = rs_groups(store, page, per_page, opts, None).await;
    assert_eq!(
        rs,
        go,
        "GET /groups?{query}: Go {:?}, store {:?}",
        tags(&go),
        tags(&rs)
    );
}

fn caller_opts() -> GroupSearchOpts {
    GroupSearchOpts {
        filter_allow_reference: true,
        include_member_count: true,
        ..GroupSearchOpts::default()
    }
}

// ---------------------------------------------------------------------------------------------
// GetGroups
// ---------------------------------------------------------------------------------------------

/// The two calls the mention engine makes — `FilterAllowReference` + `IncludeMemberCount`, then
/// the same with `Source: custom` — with the counts that exclude a deactivated user and a
/// deleted membership, and the archived group gone.
#[tokio::test]
async fn get_groups_matches_go_for_the_mention_engines_two_calls() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let fx = fixture().await;
    let store = store().await;

    let go = go_groups(
        &fx.admin,
        &fx.pair.go,
        "filter_allow_reference=true&include_member_count=true&per_page=200",
    )
    .await;
    // `per_page` 200 to Go and **0** to the store: the caller's "no page" must read the same
    // rows as the largest page the route allows.
    let rs = rs_groups(&store, 0, 0, &caller_opts(), None).await;
    assert_eq!(rs, go, "Go {:?}, store {:?}", tags(&go), tags(&rs));
    assert_eq!(
        tags(&go),
        [
            "delta",
            "alpha",
            "echo",
            "<nameless mmrsgrpreadfoxtrot00000001>",
            "hotel",
            "india",
            "juliet",
            "kilo"
        ],
        "the fixture must exercise the display-name order, the AllowReference filter and the \
         archive filter"
    );
    let alpha = go
        .iter()
        .find(|g| g["name"] == "mmrsgrpread-alpha")
        .unwrap();
    assert_eq!(alpha["id"], fx.custom["alpha"], "Go's own row for alpha");
    assert_eq!(
        alpha["member_count"], 2,
        "alpha counts u1 and u7 only: not the deactivated u2, not the departed u3"
    );
    assert_eq!(
        go.iter()
            .find(|g| g["name"] == "mmrsgrpread-delta")
            .unwrap()["member_count"],
        0,
        "a group with no members carries a zero, not an absent key"
    );

    let mut custom = caller_opts();
    custom.source = GroupSource::from(GroupSource::CUSTOM);
    same_groups(
        fx,
        &store,
        "filter_allow_reference=true&include_member_count=true&group_source=custom&per_page=200",
        0,
        200,
        &custom,
    )
    .await;
    let rs = rs_groups(&store, 0, 0, &custom, None).await;
    assert_eq!(tags(&rs), ["delta", "alpha", "kilo"]);
}

/// The three-way `DeleteAt` decision and the two orderings: `include_archived` appends the
/// archived groups after the live ones, `filter_archived` returns only them, and a positive
/// `since` alone lifts the `DeleteAt = 0` predicate (the mobile compatibility branch).
#[tokio::test]
async fn get_groups_archive_and_since_branches_match_go() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let fx = fixture().await;
    let store = store().await;

    let mut archived = caller_opts();
    archived.include_archived = true;
    same_groups(
        fx,
        &store,
        "filter_allow_reference=true&include_member_count=true&include_archived=true&per_page=200",
        0,
        200,
        &archived,
    )
    .await;
    let rs = rs_groups(&store, 0, 0, &archived, None).await;
    assert_eq!(
        tags(&rs).last().map(String::as_str),
        Some("charlie"),
        "the archived group sorts after every live one, not into them by display name"
    );

    let only_archived = GroupSearchOpts {
        filter_archived: true,
        include_member_count: true,
        ..GroupSearchOpts::default()
    };
    same_groups(
        fx,
        &store,
        "filter_archived=true&include_member_count=true&per_page=200",
        0,
        200,
        &only_archived,
    )
    .await;
    assert_eq!(
        tags(&rs_groups(&store, 0, 0, &only_archived, None).await),
        ["charlie"]
    );

    // A `since` that splits the fixture: the median `update_at` of the whole set, archived
    // included. `charlie` was deleted last, so its `UpdateAt` is the newest and it must appear
    // although `include_archived` is off.
    let mut stamps: Vec<i64> = rs.iter().filter_map(|g| g["update_at"].as_i64()).collect();
    stamps.sort_unstable();
    stamps.dedup();
    assert!(stamps.len() >= 3, "the fixture needs distinct timestamps");
    let since = stamps[stamps.len() / 2];
    let recent = GroupSearchOpts {
        since,
        include_member_count: true,
        ..GroupSearchOpts::default()
    };
    same_groups(
        fx,
        &store,
        &format!("since={since}&include_member_count=true&per_page=200"),
        0,
        200,
        &recent,
    )
    .await;
    let rs = rs_groups(&store, 0, 0, &recent, None).await;
    assert!(
        tags(&rs).contains(&"charlie".to_owned()),
        "since lifts the archive filter: {:?}",
        tags(&rs)
    );
    assert!(
        !tags(&rs).contains(&"delta".to_owned()) || stamps.len() < 4,
        "since must exclude something: {:?}",
        tags(&rs)
    );
}

/// `q` — `ILIKE` on name or display name, with `_` and `%` escaped — `filter_has_member`, and
/// a page in the middle of the search.
#[tokio::test]
async fn get_groups_search_and_member_filters_match_go() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let fx = fixture().await;
    let store = store().await;

    let search = |q: &str| GroupSearchOpts {
        q: q.to_owned(),
        include_member_count: true,
        ..GroupSearchOpts::default()
    };
    same_groups(
        fx,
        &store,
        "q=GRPREAD&include_member_count=true&per_page=200",
        0,
        200,
        &search("GRPREAD"),
    )
    .await;
    // `_plug` matches `india_plug` and not `julietXplug`: the underscore is escaped, not a
    // wildcard.
    same_groups(
        fx,
        &store,
        "q=_plug&include_member_count=true&per_page=200",
        0,
        200,
        &search("_plug"),
    )
    .await;
    assert_eq!(
        tags(&rs_groups(&store, 0, 0, &search("_plug"), None).await),
        ["india"]
    );
    // `%` likewise: nothing has a literal percent sign.
    same_groups(
        fx,
        &store,
        "q=Grpread%25&include_member_count=true&per_page=200",
        0,
        200,
        &search("Grpread%"),
    )
    .await;
    assert!(
        rs_groups(&store, 0, 0, &search("Grpread%"), None)
            .await
            .is_empty()
    );
    // A backslash is stripped before escaping, so `\_plug` is `_plug`.
    same_groups(
        fx,
        &store,
        "q=%5C_plug&include_member_count=true&per_page=200",
        0,
        200,
        &search("\\_plug"),
    )
    .await;

    // Pages over the search, which pins the set to the fixture so the pages are exact.
    for (page, per_page) in [(0, 3), (1, 3), (2, 3), (1, 2)] {
        let query = format!("q=grpread&include_member_count=true&page={page}&per_page={per_page}");
        same_groups(fx, &store, &query, page, per_page, &search("grpread")).await;
    }
    assert_eq!(
        tags(&rs_groups(&store, 1, 3, &search("grpread"), None).await),
        ["echo", "<nameless mmrsgrpreadfoxtrot00000001>", "golf"]
    );

    let member = GroupSearchOpts {
        filter_has_member: fx.u1.clone(),
        include_member_count: true,
        ..GroupSearchOpts::default()
    };
    same_groups(
        fx,
        &store,
        &format!(
            "filter_has_member={}&include_member_count=true&per_page=200",
            fx.u1
        ),
        0,
        200,
        &member,
    )
    .await;
    assert_eq!(
        tags(&rs_groups(&store, 0, 0, &member, None).await),
        ["bravo", "alpha", "echo", "kilo"],
        "u1's live memberships, the archived group excluded"
    );
    let departed = GroupSearchOpts {
        filter_has_member: fx.u3.clone(),
        include_member_count: true,
        ..GroupSearchOpts::default()
    };
    same_groups(
        fx,
        &store,
        &format!(
            "filter_has_member={}&include_member_count=true&per_page=200",
            fx.u3
        ),
        0,
        200,
        &departed,
    )
    .await;
    assert_eq!(
        tags(&rs_groups(&store, 0, 0, &departed, None).await),
        ["kilo"],
        "u3's membership of alpha is deleted and must not count"
    );
}

/// `group_source`, and `only_syncable_sources` — `ldap` plus the unescaped `LIKE 'plugin_%'`,
/// which is why `pluginzz` is in the fixture — and that an explicit source wins over it.
#[tokio::test]
async fn get_groups_source_filters_match_go() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let fx = fixture().await;
    let store = store().await;

    let ldap = GroupSearchOpts {
        source: GroupSource::from(GroupSource::LDAP),
        include_member_count: true,
        ..GroupSearchOpts::default()
    };
    same_groups(
        fx,
        &store,
        "group_source=ldap&include_member_count=true&per_page=200",
        0,
        200,
        &ldap,
    )
    .await;
    assert_eq!(
        tags(&rs_groups(&store, 0, 0, &ldap, None).await),
        [
            "echo",
            "<nameless mmrsgrpreadfoxtrot00000001>",
            "golf",
            "hotel"
        ]
    );

    let syncable = GroupSearchOpts {
        only_syncable_sources: true,
        include_member_count: true,
        ..GroupSearchOpts::default()
    };
    same_groups(
        fx,
        &store,
        "only_syncable_sources=true&include_member_count=true&per_page=200",
        0,
        200,
        &syncable,
    )
    .await;
    let rs = rs_groups(&store, 0, 0, &syncable, None).await;
    assert!(
        tags(&rs).contains(&"juliet".to_owned()),
        "`LIKE 'plugin_%'` matches `pluginzz` — the underscore is a wildcard there: {:?}",
        tags(&rs)
    );
    assert!(!tags(&rs).contains(&"alpha".to_owned()));

    let mut both = syncable.clone();
    both.source = GroupSource::from(GroupSource::CUSTOM);
    same_groups(
        fx,
        &store,
        "only_syncable_sources=true&group_source=custom&include_member_count=true&per_page=200",
        0,
        200,
        &both,
    )
    .await;
    assert_eq!(
        tags(&rs_groups(&store, 0, 0, &both, None).await),
        ["bravo", "delta", "alpha", "kilo"],
        "an explicit source is the whole filter; the syncable clause is its `else`"
    );
}

/// `not_associated_to_team`, `not_associated_to_channel` — each ignoring an unlinked syncable
/// — and `filter_parent_team_permitted`, which bites only for a channel in a group-constrained
/// team.
#[tokio::test]
async fn get_groups_association_filters_match_go() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let fx = fixture().await;
    let store = store().await;

    let not_team = GroupSearchOpts {
        not_associated_to_team: fx.team.clone(),
        include_member_count: true,
        ..GroupSearchOpts::default()
    };
    same_groups(
        fx,
        &store,
        &format!(
            "not_associated_to_team={}&include_member_count=true&per_page=200",
            fx.team
        ),
        0,
        200,
        &not_team,
    )
    .await;
    let rs = tags(&rs_groups(&store, 0, 0, &not_team, None).await);
    assert!(
        rs.contains(&"golf".to_owned()) && !rs.contains(&"echo".to_owned()),
        "golf's team link is deleted, echo's is live: {rs:?}"
    );

    let not_channel = GroupSearchOpts {
        not_associated_to_channel: fx.channel.clone(),
        include_member_count: true,
        ..GroupSearchOpts::default()
    };
    same_groups(
        fx,
        &store,
        &format!(
            "not_associated_to_channel={}&include_member_count=true&per_page=200",
            fx.channel
        ),
        0,
        200,
        &not_channel,
    )
    .await;
    let rs = tags(&rs_groups(&store, 0, 0, &not_channel, None).await);
    assert!(
        rs.contains(&"hotel".to_owned()) && !rs.contains(&"golf".to_owned()),
        "hotel's channel link is deleted, golf's is live: {rs:?}"
    );

    // The parent team is constrained: only its linked groups may be offered for the channel.
    let permitted = GroupSearchOpts {
        not_associated_to_channel: fx.constrained_channel.clone(),
        filter_parent_team_permitted: true,
        include_member_count: true,
        ..GroupSearchOpts::default()
    };
    same_groups(
        fx,
        &store,
        &format!(
            "not_associated_to_channel={}&filter_parent_team_permitted=true&include_member_count=true&per_page=200",
            fx.constrained_channel
        ),
        0,
        200,
        &permitted,
    )
    .await;
    assert_eq!(
        tags(&rs_groups(&store, 0, 0, &permitted, None).await),
        ["india"]
    );

    // The same flag for a channel whose team is not constrained changes nothing.
    let unconstrained = GroupSearchOpts {
        not_associated_to_channel: fx.counted.clone(),
        filter_parent_team_permitted: true,
        include_member_count: true,
        ..GroupSearchOpts::default()
    };
    same_groups(
        fx,
        &store,
        &format!(
            "not_associated_to_channel={}&filter_parent_team_permitted=true&include_member_count=true&per_page=200",
            fx.counted
        ),
        0,
        200,
        &unconstrained,
    )
    .await;
    assert_eq!(
        rs_groups(&store, 0, 0, &unconstrained, None).await.len(),
        10,
        "every live fixture group, since nothing is linked to `counted`"
    );
}

/// `include_channel_member_count` — the members of one channel per group — and
/// `include_timezones`, which counts distinct configured timezones among them.
#[tokio::test]
async fn get_groups_channel_member_counts_match_go() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let fx = fixture().await;
    let store = store().await;

    let counted = GroupSearchOpts {
        include_channel_member_count: fx.counted.clone(),
        include_member_count: true,
        ..GroupSearchOpts::default()
    };
    same_groups(
        fx,
        &store,
        &format!(
            "include_channel_member_count={}&include_member_count=true&per_page=200",
            fx.counted
        ),
        0,
        200,
        &counted,
    )
    .await;
    let rs = rs_groups(&store, 0, 0, &counted, None).await;
    let count = |tag: &str| {
        rs.iter()
            .find(|g| g["name"] == format!("{PREFIX}-{tag}"))
            .map(|g| g["channel_member_count"].clone())
            .unwrap()
    };
    assert_eq!(count("kilo"), 2, "u1 and u4 are in `counted`");
    assert_eq!(count("alpha"), 1, "u1 only; u7 is in the other team");
    assert_eq!(count("delta"), 0);
    assert!(
        rs.iter()
            .all(|g| g.get("channel_member_timezones_count").is_none()),
        "the timezone count is absent unless asked for"
    );

    let mut zoned = counted.clone();
    zoned.include_timezones = true;
    same_groups(
        fx,
        &store,
        &format!(
            "include_channel_member_count={}&include_timezones=true&include_member_count=true&per_page=200",
            fx.counted
        ),
        0,
        200,
        &zoned,
    )
    .await;
    let rs = rs_groups(&store, 0, 0, &zoned, None).await;
    let zones = |tag: &str| {
        rs.iter()
            .find(|g| g["name"] == format!("{PREFIX}-{tag}"))
            .map(|g| g["channel_member_timezones_count"].clone())
            .unwrap()
    };
    assert_eq!(
        zones("kilo"),
        1,
        "u4's manual timezone; u1's automatic one is empty"
    );
    assert_eq!(zones("alpha"), 0);
}

/// `viewRestrictions`, which Go builds for a caller without `view_members` — a guest — from the
/// channels they are in, and which narrows the **member count** only. The guest oracle answers
/// with its own restrictions; the store is handed the same two lists.
#[tokio::test]
async fn get_groups_view_restrictions_match_the_guest_oracle() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let fx = fixture().await;
    let store = store().await;
    let guest_go = common::licensed_guest_go();

    // `GetViewUsersRestrictions` (app/user.go:2756): no team grants a guest `view_members`, so
    // `Teams` is empty and `Channels` is every channel membership row the guest has.
    let pool = pool().await;
    let channels: Vec<String> =
        sqlx::query_scalar("SELECT channelid FROM channelmembers WHERE userid = $1")
            .bind(&fx.guest_id)
            .fetch_all(&pool)
            .await
            .expect("the guest's channels");
    assert!(channels.contains(&fx.counted), "the guest is in `counted`");
    let restrictions = ViewUsersRestrictions {
        teams: Vec::new(),
        channels,
    };

    let go = go_groups(
        &fx.guest_token,
        &guest_go,
        "include_member_count=true&per_page=200",
    )
    .await;
    let rs = rs_groups(&store, 0, 200, &caller_opts(), Some(&restrictions)).await;
    assert_eq!(rs, go, "Go {:?}, store {:?}", tags(&go), tags(&rs));
    let alpha = go
        .iter()
        .find(|g| g["name"] == "mmrsgrpread-alpha")
        .unwrap();
    assert_eq!(
        alpha["member_count"], 1,
        "the guest shares a channel with u1 but not with u7: one fewer than the administrator"
    );
    assert!(
        go.iter().all(|g| g["allow_reference"] == true),
        "a guest cannot read the sysconsole, so the reference filter is forced"
    );

    // The team list has no Go producer on this stack — nothing grants a guest `view_members`
    // on a team — so this half is checked against the channel half, not against Go: naming the
    // team u1 is in and no channel must count u1 again.
    let by_team = ViewUsersRestrictions {
        teams: vec![fx.team.clone()],
        channels: Vec::new(),
    };
    let rs = rs_groups(&store, 0, 200, &caller_opts(), Some(&by_team)).await;
    assert_eq!(
        rs.iter()
            .find(|g| g["name"] == "mmrsgrpread-alpha")
            .unwrap()["member_count"],
        1
    );
    // Two empty lists are Go's `1 = 0`: nobody is counted.
    let nobody = ViewUsersRestrictions::default();
    let rs = rs_groups(&store, 0, 200, &caller_opts(), Some(&nobody)).await;
    assert!(rs.iter().all(|g| g["member_count"] == 0), "{rs:?}");
}

// ---------------------------------------------------------------------------------------------
// GetGroupsByChannel / GetGroupsByTeam
// ---------------------------------------------------------------------------------------------

/// `GET /channels/{id}/groups?<query>` from the licensed oracle: the `groups` list, fixture rows
/// only, and the `total_group_count` beside it.
async fn go_linked(fx: &Fixture, kind: &str, id: &str, query: &str) -> Vec<serde_json::Value> {
    let client = client();
    let path = format!("/api/v4/{kind}/{id}/groups?{query}");
    let (status, body) = send(
        &client,
        &fx.pair.go,
        &fx.admin,
        reqwest::Method::GET,
        &path,
        None,
    )
    .await;
    assert_eq!(status, 200, "GET {path}: {body}");
    fixture_only(body["groups"].as_array().expect("a groups list").clone())
}

fn serialised(groups: &[mm_model::group::GroupWithSchemeAdmin]) -> Vec<serde_json::Value> {
    fixture_only(
        groups
            .iter()
            .map(|g| serde_json::to_value(g).expect("serialises"))
            .collect(),
    )
}

fn by_id(mut groups: Vec<serde_json::Value>) -> Vec<serde_json::Value> {
    groups.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
    groups
}

#[tokio::test]
async fn groups_by_channel_match_go() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let fx = fixture().await;
    let sql = store().await;
    let store = sql.group();

    // The mention engine's options: no page, member counts, references only. Hotel's link is
    // deleted and golf may not be referenced; the nameless group is present, with no `name`.
    let go = go_linked(
        fx,
        "channels",
        &fx.channel,
        "filter_allow_reference=true&include_member_count=true&paginate=false",
    )
    .await;
    let rs = serialised(
        &store
            .get_groups_by_channel(&fx.channel, &caller_opts())
            .await
            .unwrap(),
    );
    assert_eq!(rs, go, "Go {:?}, store {:?}", tags(&go), tags(&rs));
    assert_eq!(tags(&go), ["echo", "<nameless mmrsgrpreadfoxtrot00000001>"]);
    assert_eq!(go[0]["scheme_admin"], true);
    assert_eq!(go[1]["scheme_admin"], false);
    assert_eq!(go[0]["member_count"], 1);
    assert!(go[1].get("name").is_none(), "{:?}", go[1]);

    // No option at all — no ordering in Go either, so the sets are compared. Golf's null
    // `SchemeAdmin` comes back `false`, and `member_count` is absent.
    let go = go_linked(fx, "channels", &fx.channel, "paginate=false").await;
    let rs = serialised(
        &store
            .get_groups_by_channel(&fx.channel, &GroupSearchOpts::default())
            .await
            .unwrap(),
    );
    assert_eq!(by_id(rs), by_id(go.clone()));
    assert_eq!(go.len(), 3);
    assert!(go.iter().all(|g| g.get("member_count").is_none()), "{go:?}");
    assert_eq!(
        go.iter()
            .find(|g| g["id"] == GOLF)
            .map(|g| g["scheme_admin"].clone()),
        Some(serde_json::json!(false))
    );

    // `q`, and the default page (`paginate` unset is page 0 of 60).
    let go = go_linked(fx, "channels", &fx.channel, "q=GOLF").await;
    let rs = serialised(
        &store
            .get_groups_by_channel(
                &fx.channel,
                &GroupSearchOpts {
                    q: "GOLF".to_owned(),
                    page_opts: Some(PageOpts {
                        page: 0,
                        per_page: 60,
                    }),
                    ..GroupSearchOpts::default()
                },
            )
            .await
            .unwrap(),
    );
    assert_eq!(rs, go);
    assert_eq!(tags(&go), ["golf"]);

    // Pages of one, ordered by display name.
    for page in [0, 1, 2, 3] {
        let go = go_linked(
            fx,
            "channels",
            &fx.channel,
            &format!("page={page}&per_page=1"),
        )
        .await;
        let rs = serialised(
            &store
                .get_groups_by_channel(
                    &fx.channel,
                    &GroupSearchOpts {
                        page_opts: Some(PageOpts { page, per_page: 1 }),
                        ..GroupSearchOpts::default()
                    },
                )
                .await
                .unwrap(),
        );
        assert_eq!(rs, go, "page {page}");
        assert_eq!(go.len(), usize::from(page < 3), "page {page}");
    }
}

#[tokio::test]
async fn groups_by_team_match_go() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let fx = fixture().await;
    let sql = store().await;
    let store = sql.group();

    let go = go_linked(
        fx,
        "teams",
        &fx.team,
        "filter_allow_reference=true&include_member_count=true&paginate=false",
    )
    .await;
    let rs = serialised(
        &store
            .get_groups_by_team(&fx.team, &caller_opts())
            .await
            .unwrap(),
    );
    assert_eq!(rs, go, "Go {:?}, store {:?}", tags(&go), tags(&rs));
    assert_eq!(
        tags(&go),
        ["echo", "<nameless mmrsgrpreadfoxtrot00000001>", "hotel"],
        "golf's team link is deleted; the channel links are not consulted"
    );
    assert_eq!(
        go[0]["scheme_admin"], false,
        "echo's team link is not admin"
    );
    assert_eq!(go[1]["scheme_admin"], true);
    assert_eq!(go[2]["scheme_admin"], false, "hotel's null scheme admin");

    let go = go_linked(fx, "teams", &fx.team, "paginate=false").await;
    let rs = serialised(
        &store
            .get_groups_by_team(&fx.team, &GroupSearchOpts::default())
            .await
            .unwrap(),
    );
    assert_eq!(by_id(rs), by_id(go.clone()));
    assert_eq!(go.len(), 3);

    for (page, per_page) in [(0, 2), (1, 2), (2, 2)] {
        let go = go_linked(
            fx,
            "teams",
            &fx.team,
            &format!("page={page}&per_page={per_page}&include_member_count=true"),
        )
        .await;
        let rs = serialised(
            &store
                .get_groups_by_team(
                    &fx.team,
                    &GroupSearchOpts {
                        include_member_count: true,
                        page_opts: Some(PageOpts { page, per_page }),
                        ..GroupSearchOpts::default()
                    },
                )
                .await
                .unwrap(),
        );
        assert_eq!(rs, go, "page {page}");
    }
    let go = go_linked(fx, "teams", &fx.team, "page=1&per_page=2").await;
    assert_eq!(tags(&go), ["hotel"]);

    // The constrained team, whose channel and team links differ from `team`'s.
    let go = go_linked(fx, "teams", &fx.constrained_team, "paginate=false").await;
    let rs = serialised(
        &store
            .get_groups_by_team(&fx.constrained_team, &GroupSearchOpts::default())
            .await
            .unwrap(),
    );
    assert_eq!(rs, go);
    assert_eq!(tags(&go), ["india"]);
}

// ---------------------------------------------------------------------------------------------
// getGroupsAllowedForReferenceInChannel
// ---------------------------------------------------------------------------------------------

/// A `Group` map from a Go list, keyed by id, nameless groups dropped and the syncable's
/// `scheme_admin` removed — the app function's own shape, built from the oracle's rows.
fn expected_map(lists: Vec<Vec<serde_json::Value>>) -> BTreeMap<String, serde_json::Value> {
    let mut map = BTreeMap::new();
    for list in lists {
        for mut group in list {
            if group.get("name").is_none() {
                continue;
            }
            group.as_object_mut().unwrap().remove("scheme_admin");
            map.insert(group["id"].as_str().unwrap().to_owned(), group);
        }
    }
    map
}

fn app_map(
    groups: &BTreeMap<String, mm_model::group::Group>,
) -> BTreeMap<String, serde_json::Value> {
    groups
        .iter()
        .map(|(id, g)| (id.clone(), serde_json::to_value(g).unwrap()))
        .filter(|(_, g)| is_fixture(g))
        .collect()
}

#[tokio::test]
async fn allowed_for_reference_composes_the_three_reads_as_go_does() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let fx = fixture().await;
    let app = App::new(store().await);

    let constrained = |id: &str| Channel {
        id: id.to_owned(),
        group_constrained: Some(true),
        ..Channel::default()
    };
    let open = |id: &str| Channel {
        id: id.to_owned(),
        group_constrained: Some(false),
        ..Channel::default()
    };
    let constrained_team = Team {
        id: fx.team.clone(),
        group_constrained: Some(true),
        ..Team::default()
    };
    let open_team = Team {
        id: fx.team.clone(),
        group_constrained: None,
        ..Team::default()
    };

    let everything = go_groups(
        &fx.admin,
        &fx.pair.go,
        "filter_allow_reference=true&include_member_count=true&per_page=200",
    )
    .await;
    let custom = go_groups(
        &fx.admin,
        &fx.pair.go,
        "filter_allow_reference=true&include_member_count=true&group_source=custom&per_page=200",
    )
    .await;
    let linked_to_channel = go_linked(
        fx,
        "channels",
        &fx.channel,
        "filter_allow_reference=true&include_member_count=true&paginate=false",
    )
    .await;
    let linked_to_team = go_linked(
        fx,
        "teams",
        &fx.team,
        "filter_allow_reference=true&include_member_count=true&paginate=false",
    )
    .await;

    // Neither constrained: every referenceable group, whatever its source.
    let got = app
        .get_groups_allowed_for_reference_in_channel(&open(&fx.channel), Some(&open_team))
        .await
        .unwrap();
    assert_eq!(app_map(&got), expected_map(vec![everything.clone()]));
    let got = app
        .get_groups_allowed_for_reference_in_channel(&open(&fx.channel), None)
        .await
        .unwrap();
    assert_eq!(app_map(&got), expected_map(vec![everything]));

    // The channel is constrained: its links plus every custom group — and the team's links are
    // not consulted even though the team is constrained too (hotel is linked to the team only).
    let got = app
        .get_groups_allowed_for_reference_in_channel(
            &constrained(&fx.channel),
            Some(&constrained_team),
        )
        .await
        .unwrap();
    let expected = expected_map(vec![linked_to_channel.clone(), custom.clone()]);
    assert_eq!(app_map(&got), expected);
    assert!(expected.contains_key(ECHO) && !expected.contains_key(HOTEL));
    assert!(
        !expected.contains_key(FOXTROT),
        "the nameless group is linked but cannot be mentioned"
    );
    assert!(
        expected.values().any(|g| g["name"] == "mmrsgrpread-kilo"),
        "custom groups are offered whatever the links say"
    );
    let got = app
        .get_groups_allowed_for_reference_in_channel(&constrained(&fx.channel), None)
        .await
        .unwrap();
    assert_eq!(
        app_map(&got),
        expected_map(vec![linked_to_channel, custom.clone()])
    );

    // Only the team is constrained: the team's links plus every custom group.
    let got = app
        .get_groups_allowed_for_reference_in_channel(&open(&fx.channel), Some(&constrained_team))
        .await
        .unwrap();
    let expected = expected_map(vec![linked_to_team, custom]);
    assert_eq!(app_map(&got), expected);
    assert!(expected.contains_key(HOTEL) && expected.contains_key(ECHO));
    assert!(!expected.contains_key(GOLF), "golf's team link is deleted");
}
