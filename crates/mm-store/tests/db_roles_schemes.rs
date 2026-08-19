//! Database-backed tests for `SqlRoleStore` and `SqlSchemeStore`.
//!
//! ```sh
//! docker compose up -d
//! export DATABASE_URL=postgres://mmuser:mmuser_password@localhost:5432/mattermost
//! MM_STORE_DB=1 cargo test -p mm-store --test db_roles_schemes
//! ```
//!
//! Skipped unless `MM_STORE_DB=1`, so `cargo test` stays green with no Docker — the same
//! arrangement as `MM_PARITY_STACK` for the cross-server tests.
//!
//! # What these can and cannot check
//!
//! `Roles` is populated by the Go server at startup, so it is real data written by the reference
//! implementation — the best possible input for a read path. It is **not** an oracle for
//! `MakeDefaultRoles`, and the attempt to use it as one is what found [D-130]: the container runs
//! `mattermost-team-edition:latest`, which is **11.10.0**, while the reference tree is pinned at
//! **11.11.0**. Thirteen of the twenty-four roles differ in both directions — the server lacks the
//! permissions 11.11.0 added, and carries deprecated ones 11.11.0 dropped — plus the app layer
//! augments the defaults before writing them. So these assert what a *store* should: that rows
//! come back, parse, and round-trip.
//!
//! `Schemes` is empty on Team Edition, so the scheme tests insert their own row and delete it
//! again. That is a write to a table the Go server owns; the id is prefixed so it is obviously
//! test-owned, and the cleanup runs even when an assertion fails.

use mm_model::scheme::{SCHEME_SCOPE_CHANNEL, SCHEME_SCOPE_TEAM};
use mm_store::{RoleStore, SchemeStore, SqlRoleStore, SqlSchemeStore, SqlUserStore, UserStore};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

/// These tests share one mutable database: they insert rows into `Schemes`, `Teams` and
/// `Channels`, and two of them assert on *counts* and *listings*, which another test's rows would
/// change. The harness runs tests in parallel by default, so they take this lock — the alternative
/// is a suite whose failures look exactly like a store bug and are not one.
static DB: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Deletes every row these tests could have created, by id or name prefix.
///
/// Called at the **start** of each test rather than only at the end, because a failing assertion
/// panics and unwinds straight past any trailing cleanup — which is not hypothetical: it is how a
/// mutation-testing run left three schemes behind and made the next test fail for a reason that had
/// nothing to do with the code under test. Cleanup that only runs on the happy path is not cleanup.
async fn purge_test_rows(pool: &PgPool) {
    // Child tables first: channels reference teams, and both reference schemes.
    for statement in [
        "DELETE FROM channels WHERE id LIKE 'mmrs%'",
        "DELETE FROM teams WHERE id LIKE 'mmrs%'",
        "DELETE FROM schemes WHERE id LIKE 'mmrs%'",
        "DELETE FROM roles WHERE id LIKE 'mmrs%' OR name LIKE 'mmrs\\_%'",
    ] {
        sqlx::query(statement)
            .execute(pool)
            .await
            .expect("purges leftover test rows");
    }
}

fn enabled() -> bool {
    std::env::var("MM_STORE_DB").is_ok_and(|v| v == "1")
}

async fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for MM_STORE_DB=1");
    PgPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .expect("connects to Postgres")
}

#[tokio::test]
async fn roles_read_back_from_what_the_go_server_wrote() {
    if !enabled() {
        return;
    }
    let _guard = DB.lock().await;
    purge_test_rows(&pool().await).await;
    let store = SqlRoleStore::new(pool().await);

    let all = store.get_all().await.expect("get_all succeeds");
    assert!(
        all.len() >= 24,
        "the Go server writes 24 built-in roles at startup; found {}",
        all.len()
    );

    for role in &all {
        assert!(!role.id.is_empty(), "every row has an id");
        assert!(!role.name.is_empty(), "every row has a name");
        // The column is written with a leading space per entry. If `strings.Fields`' semantics
        // were not reproduced, an empty first element would appear here on real data.
        let permissions = role.permissions.as_deref().expect("never None from a read");
        assert!(
            permissions.iter().all(|p| !p.is_empty()),
            "{}: an empty permission means the split kept the leading space",
            role.name
        );
        assert!(
            permissions.iter().all(|p| !p.contains(char::is_whitespace)),
            "{}: a permission containing whitespace means the split did not happen",
            role.name
        );
    }

    // Every built-in role the *port* knows about should exist in a database the Go server
    // migrated — this direction holds across the version skew, since 11.11.0 adds permissions to
    // existing roles rather than adding roles.
    let by_name: std::collections::BTreeMap<&str, &mm_model::role::Role> =
        all.iter().map(|r| (r.name.as_str(), r)).collect();
    for id in mm_model::role::BUILT_IN_SCHEME_MANAGED_ROLE_IDS {
        assert!(
            by_name.contains_key(id),
            "{id} is missing from the database"
        );
    }
}

/// None of the four read paths filters `DeleteAt`, which matters because `Delete` only stamps it:
/// a permission check still has to resolve the role a member's `Roles` column names. The Go server
/// leaves no deleted roles behind, so this test makes one — without it, adding a `WHERE deleteat =
/// 0` to `get_all` passes the entire suite.
#[tokio::test]
async fn a_deleted_role_is_still_returned_by_every_read_path() {
    if !enabled() {
        return;
    }
    let _guard = DB.lock().await;
    purge_test_rows(&pool().await).await;
    let pool = pool().await;
    let store = SqlRoleStore::new(pool.clone());

    let id = "mmrstestroledeletedxxxxxxx";
    let name = "mmrs_test_deleted_role";
    delete_role(&pool, id).await;

    let result = async {
        sqlx::query(
            r#"
            INSERT INTO roles (id, name, displayname, description, createat, updateat, deleteat,
                               permissions, schememanaged, builtin, schemeid)
            VALUES ($1, $2, 'MMRS Deleted Role', 'inserted by mm-store tests',
                    1755000000000, 1755000000000, 1755000000001,
                    ' create_post edit_post', false, false, NULL)
            "#,
        )
        .bind(id)
        .bind(name)
        .execute(&pool)
        .await
        .expect("inserts a deleted test role");

        let by_id = store.get(id).await?.expect("get returns a deleted role");
        assert_eq!(by_id.delete_at, 1_755_000_000_001);
        assert_eq!(
            by_id.permissions.as_deref().expect("never None"),
            ["create_post".to_owned(), "edit_post".to_owned()]
        );

        assert!(store.get_by_name(name).await?.is_some(), "get_by_name too");
        assert!(
            store.get_by_names(&[name.to_owned()]).await?.len() == 1,
            "get_by_names too"
        );
        assert!(
            store.get_all().await?.iter().any(|r| r.name == name),
            "get_all has no DeleteAt filter at all"
        );

        Ok::<(), mm_store::StoreError>(())
    }
    .await;

    delete_role(&pool, id).await;
    result.expect("deleted roles resolve");
}

async fn delete_role(pool: &PgPool, id: &str) {
    sqlx::query("DELETE FROM roles WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await
        .expect("removes the test role");
}

#[tokio::test]
async fn role_get_by_name_and_get_agree() {
    if !enabled() {
        return;
    }
    let _guard = DB.lock().await;
    purge_test_rows(&pool().await).await;
    let store = SqlRoleStore::new(pool().await);

    let by_name = store
        .get_by_name("system_admin")
        .await
        .expect("get_by_name succeeds")
        .expect("system_admin exists");
    assert!(by_name.built_in && by_name.scheme_managed);
    assert!(
        by_name
            .permissions
            .as_deref()
            .expect("never None")
            .iter()
            .any(|p| p == "manage_system"),
        "system_admin holds manage_system in every version"
    );

    let by_id = store
        .get(&by_name.id)
        .await
        .expect("get succeeds")
        .expect("the id just read back exists");
    assert_eq!(by_id, by_name, "the two read paths must agree exactly");
}

#[tokio::test]
async fn role_lookups_that_find_nothing_are_not_errors() {
    if !enabled() {
        return;
    }
    let _guard = DB.lock().await;
    purge_test_rows(&pool().await).await;
    let store = SqlRoleStore::new(pool().await);

    // Go returns `store.NewErrNotFound`; the port answers `None` and lets the caller choose.
    assert!(
        store
            .get("nosuchrole1jbyqbtxbtqcgy")
            .await
            .expect("no error")
            .is_none()
    );
    assert!(
        store
            .get_by_name("no_such_role")
            .await
            .expect("no error")
            .is_none()
    );

    // The empty-input short circuit never reaches the database.
    assert!(store.get_by_names(&[]).await.expect("no error").is_empty());

    let requested = vec!["system_admin".to_owned(), "no_such_role".to_owned()];
    let found = store.get_by_names(&requested).await.expect("no error");
    assert_eq!(found.len(), 1, "a missing name is skipped, not an error");
    assert_eq!(found[0].name, "system_admin");
}

#[tokio::test]
async fn scheme_read_paths_disagree_about_deleted_rows() {
    if !enabled() {
        return;
    }
    let _guard = DB.lock().await;
    purge_test_rows(&pool().await).await;
    let pool = pool().await;
    let store = SqlSchemeStore::new(pool.clone());

    // Test-owned ids: 26 characters, the shape `IsValidId` demands, and obviously not the Go
    // server's. Schemes is empty on Team Edition, so nothing else is looking at this table.
    let live_id = "mmrstestschemealivexxxxxxx";
    let dead_id = "mmrstestschemebdeadxxxxxxx";
    // A second live row, created LATER, so the DESC ordering and the paging window have something
    // to be wrong about. With one row, `ORDER BY createat ASC` and a swapped LIMIT/OFFSET both
    // pass everything.
    let newer_id = "mmrstestschemecnewerxxxxxx";
    cleanup(&pool, &[live_id, dead_id, newer_id]).await;

    let result = async {
        insert_scheme(
            &pool,
            live_id,
            "mmrs_test_scheme_live",
            SCHEME_SCOPE_TEAM,
            0,
        )
        .await;
        insert_scheme(
            &pool,
            dead_id,
            "mmrs_test_scheme_dead",
            SCHEME_SCOPE_TEAM,
            1_755_000_000_000,
        )
        .await;
        insert_scheme_at(
            &pool,
            newer_id,
            "mmrs_test_scheme_newer",
            SCHEME_SCOPE_TEAM,
            0,
            1_755_000_000_001,
        )
        .await;

        // `Get` and `GetByName` do not filter DeleteAt — a deleted scheme still resolves.
        let dead = store
            .get(dead_id)
            .await?
            .expect("the deleted scheme resolves by id");
        assert_eq!(dead.delete_at, 1_755_000_000_000);
        assert!(store.get_by_name("mmrs_test_scheme_dead").await?.is_some());

        let live = store.get(live_id).await?.expect("the live scheme resolves");
        assert_eq!(live.name, "mmrs_test_scheme_live");
        assert_eq!(live.scope, SCHEME_SCOPE_TEAM);
        assert_eq!(live.default_team_admin_role, "custom_team_admin");
        // The four playbook/run columns default to '' at the database level, not NULL.
        assert_eq!(live.default_playbook_admin_role, "");

        // `GetAllPage` and `CountByScope` do filter it.
        let page = store.get_all_page(SCHEME_SCOPE_TEAM, 0, 100).await?;
        let names: Vec<&str> = page.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"mmrs_test_scheme_live"));
        assert!(
            !names.contains(&"mmrs_test_scheme_dead"),
            "GetAllPage filters DeleteAt = 0"
        );

        assert_eq!(store.count_by_scope(SCHEME_SCOPE_TEAM).await?, 2);
        assert_eq!(
            store.count_by_scope(SCHEME_SCOPE_CHANNEL).await?,
            0,
            "the test rows are team-scoped"
        );
        // An empty scope counts nothing — Go's SQL has a bare `Scope = ?` with no wildcard.
        assert_eq!(store.count_by_scope("").await?, 0);

        // ...but an empty scope is a wildcard for GetAllPage, which adds the predicate only when
        // the argument is non-empty. The two are asymmetric and this is where that shows.
        let all_scopes = store.get_all_page("", 0, 100).await?;
        assert!(all_scopes.iter().any(|s| s.name == "mmrs_test_scheme_live"));

        // Ordering is `CreateAt DESC`, so the newer row comes first — and paging applies after
        // it, which is what makes a swapped LIMIT/OFFSET visible.
        let ordered = store.get_all_page(SCHEME_SCOPE_TEAM, 0, 100).await?;
        assert_eq!(
            ordered.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            vec!["mmrs_test_scheme_newer", "mmrs_test_scheme_live"],
            "newest first"
        );
        let first_page = store.get_all_page(SCHEME_SCOPE_TEAM, 0, 1).await?;
        assert_eq!(first_page.len(), 1);
        assert_eq!(first_page[0].name, "mmrs_test_scheme_newer");
        let second_page = store.get_all_page(SCHEME_SCOPE_TEAM, 1, 1).await?;
        assert_eq!(second_page.len(), 1);
        assert_eq!(
            second_page[0].name, "mmrs_test_scheme_live",
            "offset skips the newest"
        );
        assert!(store.get_all_page("", 1000, 100).await?.is_empty());

        Ok::<(), mm_store::StoreError>(())
    }
    .await;

    cleanup(&pool, &[live_id, dead_id, newer_id]).await;
    result.expect("the scheme read paths behave");
}

async fn insert_scheme(pool: &PgPool, id: &str, name: &str, scope: &str, delete_at: i64) {
    insert_scheme_at(pool, id, name, scope, delete_at, 1_755_000_000_000).await;
}

async fn insert_scheme_at(
    pool: &PgPool,
    id: &str,
    name: &str,
    scope: &str,
    delete_at: i64,
    create_at: i64,
) {
    sqlx::query(
        r#"
        INSERT INTO schemes (id, name, displayname, description, createat, updateat, deleteat,
                             scope, defaultteamadminrole, defaultteamuserrole, defaultteamguestrole,
                             defaultchanneladminrole, defaultchanneluserrole, defaultchannelguestrole)
        VALUES ($1, $2, 'MMRS Test Scheme', 'inserted by mm-store tests', $5,
                $5, $3, $4, 'custom_team_admin', 'custom_team_user', 'custom_team_guest',
                'custom_channel_admin', 'custom_channel_user', 'custom_channel_guest')
        "#,
    )
    .bind(id)
    .bind(name)
    .bind(delete_at)
    .bind(scope)
    .bind(create_at)
    .execute(pool)
    .await
    .expect("inserts a test scheme");
}

/// Runs before and after, so a previous failed run cannot poison the next one.
async fn cleanup(pool: &PgPool, ids: &[&str]) {
    for id in ids {
        sqlx::query("DELETE FROM schemes WHERE id = $1")
            .bind(id)
            .execute(pool)
            .await
            .expect("removes the test scheme");
    }
}

/// `ChannelHigherScopedPermissions` — the query behind channel moderation, and the only one here
/// with more than one shape. Three UNION branches, and none of them fires on an empty `Schemes`
/// table, so this builds the whole graph: a team scheme, two channel schemes, two teams (one with
/// a scheme and one without) and a channel in each.
///
/// Branch 3 — the "team has no scheme, so the higher scope is the system default" case — is the
/// one that fires on Team Edition, and it is the one whose role names are matched by *name* rather
/// than by column, because no system scheme record ships with Mattermost.
#[tokio::test]
async fn channel_higher_scoped_permissions_resolves_all_three_branches() {
    if !enabled() {
        return;
    }
    let _guard = DB.lock().await;
    purge_test_rows(&pool().await).await;
    let pool = pool().await;
    let store = SqlRoleStore::new(pool.clone());

    // Every id is 26 characters and `mmrs`-prefixed, so a stray row is obviously test-owned.
    let team_scheme = "mmrshspteamschemexxxxxxxxx";
    let chan_scheme_scoped = "mmrshspchanschemeaxxxxxxxx";
    let chan_scheme_system = "mmrshspchanschemebxxxxxxxx";
    let team_with_scheme = "mmrshspteamwithschemexxxxx";
    let team_no_scheme = "mmrshspteamnoschemexxxxxxx";
    let channel_scoped = "mmrshspchannelaxxxxxxxxxxx";
    let channel_system = "mmrshspchannelbxxxxxxxxxxx";
    let roles = [
        "mmrs_hsp_ts_channel_guest",
        "mmrs_hsp_ts_channel_user",
        "mmrs_hsp_ts_channel_admin",
    ];

    let cleanup_all = || async {
        cleanup(
            &pool,
            &[team_scheme, chan_scheme_scoped, chan_scheme_system],
        )
        .await;
        for id in [channel_scoped, channel_system] {
            sqlx::query("DELETE FROM channels WHERE id = $1")
                .bind(id)
                .execute(&pool)
                .await
                .expect("clean channel");
        }
        for id in [team_with_scheme, team_no_scheme] {
            sqlx::query("DELETE FROM teams WHERE id = $1")
                .bind(id)
                .execute(&pool)
                .await
                .expect("clean team");
        }
        for name in roles {
            sqlx::query("DELETE FROM roles WHERE name = $1")
                .bind(name)
                .execute(&pool)
                .await
                .expect("clean role");
        }
    };
    cleanup_all().await;

    let result = async {
        // The team scheme's three channel roles, each with a permission set of its own so the
        // three columns of the result cannot be confused with one another.
        for (i, name) in roles.iter().enumerate() {
            sqlx::query(
                r#"
                INSERT INTO roles (id, name, displayname, description, createat, updateat, deleteat,
                                   permissions, schememanaged, builtin, schemeid)
                VALUES ($1, $2, 'MMRS HSP Role', 'inserted by mm-store tests',
                        1755000000000, 1755000000000, 0, $3, true, false, NULL)
                "#,
            )
            .bind(format!("mmrshsprole{i}xxxxxxxxxxxxxxx")[..26].to_owned())
            .bind(*name)
            .bind(format!(
                " {}",
                ["read_channel", "create_post", "manage_channel_roles"][i]
            ))
            .execute(&pool)
            .await
            .expect("inserts an hsp role");
        }

        insert_scheme_roles(
            &pool,
            team_scheme,
            "mmrs_hsp_team_scheme",
            SCHEME_SCOPE_TEAM,
            roles,
        )
        .await;
        insert_scheme_roles(
            &pool,
            chan_scheme_scoped,
            "mmrs_hsp_chan_scheme_scoped",
            SCHEME_SCOPE_CHANNEL,
            ["mmrs_hsp_cs_guest", "mmrs_hsp_cs_user", "mmrs_hsp_cs_admin"],
        )
        .await;
        insert_scheme_roles(
            &pool,
            chan_scheme_system,
            "mmrs_hsp_chan_scheme_system",
            SCHEME_SCOPE_CHANNEL,
            [
                "mmrs_hsp_sys_guest",
                "mmrs_hsp_sys_user",
                "mmrs_hsp_sys_admin",
            ],
        )
        .await;

        insert_team(
            &pool,
            team_with_scheme,
            "mmrs-hsp-team-a",
            Some(team_scheme),
        )
        .await;
        insert_team(&pool, team_no_scheme, "mmrs-hsp-team-b", None).await;
        insert_channel(
            &pool,
            channel_scoped,
            team_with_scheme,
            "mmrs-hsp-chan-a",
            chan_scheme_scoped,
        )
        .await;
        insert_channel(
            &pool,
            channel_system,
            team_no_scheme,
            "mmrs-hsp-chan-b",
            chan_scheme_system,
        )
        .await;

        // --- branches 1 and 2: the team has a scheme, so that scheme's channel roles are the
        // higher scope.
        let scoped = store
            .channel_higher_scoped_permissions(&["mmrs_hsp_cs_user".to_owned()])
            .await?;
        let user = scoped
            .get("mmrs_hsp_cs_user")
            .expect("the requested role resolves");
        assert_eq!(user.role_id, "channel_user");
        assert!(
            user.permissions.iter().any(|p| p == "create_post"),
            "the higher scope is the TEAM scheme's channel_user role: {:?}",
            user.permissions
        );
        // The admin column comes back on the same row even though only the user role was asked for.
        let admin = scoped.get("mmrs_hsp_cs_admin").expect("the admin role too");
        assert_eq!(admin.role_id, "channel_admin");
        assert!(
            admin
                .permissions
                .iter()
                .any(|p| p == "manage_channel_roles")
        );

        // Guest is branch 2, a separate SELECT keyed on the guest role name alone.
        let guest_only = store
            .channel_higher_scoped_permissions(&["mmrs_hsp_cs_guest".to_owned()])
            .await?;
        let guest = guest_only.get("mmrs_hsp_cs_guest").expect("the guest role");
        assert_eq!(guest.role_id, "channel_guest");
        assert!(guest.permissions.iter().any(|p| p == "read_channel"));

        // --- branch 3: the team has NO scheme, so the higher scope is the built-in channel roles,
        // matched by name.
        let system = store
            .channel_higher_scoped_permissions(&["mmrs_hsp_sys_user".to_owned()])
            .await?;
        for (name, role_id) in [
            ("mmrs_hsp_sys_guest", "channel_guest"),
            ("mmrs_hsp_sys_user", "channel_user"),
            ("mmrs_hsp_sys_admin", "channel_admin"),
        ] {
            let entry = system
                .get(name)
                .unwrap_or_else(|| panic!("{name} resolves through branch 3"));
            assert_eq!(entry.role_id, role_id);
            assert!(
                !entry.permissions.is_empty(),
                "{name} takes the built-in role's permissions"
            );
        }
        // The built-in channel_user role really is the source, not the team scheme's.
        let sys_user = &system["mmrs_hsp_sys_user"].permissions;
        assert!(
            sys_user.iter().any(|p| p == "read_channel"),
            "built-in channel_user holds read_channel: {sys_user:?}"
        );
        assert!(
            sys_user.len() > 3,
            "the built-in role has many permissions, the test role had one: {sys_user:?}"
        );

        // --- the two upstream quirks this port reproduces rather than tidies ([D-132]).
        //
        // 1. The empty-string key exists, because Go writes all three names unconditionally and
        //    the first two branches select `''` for the ones they do not carry.
        assert!(
            scoped.contains_key(""),
            "the '' key is Go's, and a port that skipped empty names would not have it"
        );
        // 2. The permission lists are split on a single space, so the column's leading space
        //    survives as an empty first element — unlike every other read of this column.
        assert_eq!(
            user.permissions.first().map(String::as_str),
            Some(""),
            "strings.Split, not strings.Fields: {:?}",
            user.permissions
        );

        // A name nobody uses resolves to nothing but the '' key.
        let none = store
            .channel_higher_scoped_permissions(&["mmrs_hsp_no_such_role".to_owned()])
            .await?;
        assert!(!none.contains_key("mmrs_hsp_no_such_role"));

        Ok::<(), mm_store::StoreError>(())
    }
    .await;

    cleanup_all().await;
    result.expect("the higher-scoped query resolves");
}

async fn insert_scheme_roles(
    pool: &PgPool,
    id: &str,
    name: &str,
    scope: &str,
    channel_roles: [&str; 3],
) {
    sqlx::query(
        r#"
        INSERT INTO schemes (id, name, displayname, description, createat, updateat, deleteat,
                             scope, defaultteamadminrole, defaultteamuserrole, defaultteamguestrole,
                             defaultchanneladminrole, defaultchanneluserrole, defaultchannelguestrole)
        VALUES ($1, $2, 'MMRS HSP Scheme', 'inserted by mm-store tests', 1755000000000,
                1755000000000, 0, $3, '', '', '', $4, $5, $6)
        "#,
    )
    .bind(id)
    .bind(name)
    .bind(scope)
    .bind(channel_roles[2])
    .bind(channel_roles[1])
    .bind(channel_roles[0])
    .execute(pool)
    .await
    .expect("inserts an hsp scheme");
}

async fn insert_team(pool: &PgPool, id: &str, name: &str, scheme_id: Option<&str>) {
    sqlx::query(
        r#"
        INSERT INTO teams (id, createat, updateat, deleteat, displayname, name, type,
                           allowopeninvite, schemeid, cloudlimitsarchived)
        VALUES ($1, 1755000000000, 1755000000000, 0, 'MMRS HSP Team', $2, 'O', false, $3, false)
        "#,
    )
    .bind(id)
    .bind(name)
    .bind(scheme_id)
    .execute(pool)
    .await
    .expect("inserts an hsp team");
}

async fn insert_channel(pool: &PgPool, id: &str, team_id: &str, name: &str, scheme_id: &str) {
    sqlx::query(
        r#"
        INSERT INTO channels (id, createat, updateat, deleteat, teamid, type, displayname, name,
                              schemeid, defaultcategoryname, autotranslation, discoverable)
        VALUES ($1, 1755000000000, 1755000000000, 0, $2, 'O', 'MMRS HSP Channel', $3, $4, '',
                false, false)
        "#,
    )
    .bind(id)
    .bind(team_id)
    .bind(name)
    .bind(scheme_id)
    .execute(pool)
    .await
    .expect("inserts an hsp channel");
}

/// Regression test for [D-135]: a `jsonb` column can hold the JSON value `null`, which is not SQL
/// NULL and is not malformed. The Go server writes exactly that — four of the five users in the
/// development database have `mfausedtimestamps = 'null'::jsonb` — and treating it as a decode
/// failure made `GET /users/me` a 500 for every one of them.
///
/// Reading **every** user is the point. The bug survived because the parity suite logs in as the
/// one user whose column holds `[]`.
#[tokio::test]
async fn every_user_in_the_database_decodes() {
    if !enabled() {
        return;
    }
    let _guard = DB.lock().await;
    let pool = pool().await;
    let store = SqlUserStore::new(pool.clone());

    let ids: Vec<String> = sqlx::query_scalar("SELECT id FROM users")
        .fetch_all(&pool)
        .await
        .expect("lists users");
    assert!(
        !ids.is_empty(),
        "the Go server creates several users at startup"
    );

    let mut json_nulls = 0;
    for id in &ids {
        let user = store
            .get(id)
            .await
            .unwrap_or_else(|e| panic!("user {id} must decode: {e}"));
        assert_eq!(&user.id, id);

        let raw: Option<serde_json::Value> =
            sqlx::query_scalar("SELECT mfausedtimestamps FROM users WHERE id = $1")
                .bind(id)
                .fetch_one(&pool)
                .await
                .expect("reads the raw column");
        if raw == Some(serde_json::Value::Null) {
            json_nulls += 1;
            assert!(
                user.mfa_used_timestamps.is_none(),
                "a JSON null column is an absent value, not an empty list"
            );
        }
    }
    assert!(
        json_nulls > 0,
        "no user held a JSON null, so this test proves nothing about the bug it exists for"
    );
}
