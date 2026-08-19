//! Database-backed tests for the permission checks.
//!
//! ```sh
//! docker compose up -d
//! export DATABASE_URL=postgres://mmuser:mmuser_password@localhost:5432/mattermost
//! MM_STORE_DB=1 cargo test -p mm-app --test db_authorization
//! ```
//!
//! Skipped unless `MM_STORE_DB=1`, like the store suite.
//!
//! The roles here are the ones the **Go server wrote at startup**, so a grant asserted below is a
//! grant the reference implementation would also make — and since [D-130] was paid, the container
//! and the ported source are the same minor, so that statement no longer carries an asterisk.
//!
//! Two roles are inserted: a soft-deleted one, because the Go server leaves none behind and the
//! `DeleteAt` skip is otherwise untested, and one granting `edit_other_users` without
//! `manage_system`, because no built-in role separates those two and the interesting branch of
//! `SessionHasPermissionToUser` needs them separated.

use mm_app::App;
use mm_model::permission::{
    PERMISSION_CREATE_TEAM, PERMISSION_EDIT_OTHER_USERS, PERMISSION_MANAGE_SYSTEM,
};
use mm_model::session::Session;
use mm_store::SqlStore;
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

/// One mutable database, shared with the `mm-store` suite. See that file's note.
static DB: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Two plain users and one admin, created by the tests rather than assumed.
///
/// An earlier version hardcoded ids the Go server happened to have minted. That is a landmine:
/// recreating the volume — which [D-130] required — mints new ones, and the tests then fail with
/// "permission denied" for a reason that has nothing to do with permissions. The **admin** is still
/// discovered from the database rather than inserted, because the assertion about it is precisely
/// that Go marked it `system_admin`.
const PLAIN_USER: &str = "mmrsauthplainuserxxxxxxxxx";
const OTHER_USER: &str = "mmrsauthotheruserxxxxxxxxx";

const DELETED_ROLE: &str = "mmrs_auth_deleted_role";
const EDITOR_ROLE: &str = "mmrs_auth_editor_role";

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

/// Purges at the **start**, because a failing assertion panics past any trailing cleanup.
async fn purge(pool: &PgPool) {
    sqlx::query("DELETE FROM roles WHERE name LIKE 'mmrs\\_%'")
        .execute(pool)
        .await
        .expect("purges leftover test roles");
    sqlx::query("DELETE FROM users WHERE id LIKE 'mmrsauth%'")
        .execute(pool)
        .await
        .expect("purges leftover test users");
    // Child tables first: channels reference teams, and both reference schemes.
    for statement in [
        "DELETE FROM channels WHERE id LIKE 'mmrsauth%'",
        "DELETE FROM teams WHERE id LIKE 'mmrsauth%'",
        "DELETE FROM schemes WHERE id LIKE 'mmrsauth%'",
    ] {
        sqlx::query(statement)
            .execute(pool)
            .await
            .expect("purges the leftover scheme graph");
    }
}

/// The `system_admin` the Go server created at first signup. Discovered, not hardcoded — its id
/// changes every time the volume is recreated, and what the test needs from it is Go's judgement
/// that it is an admin, not any particular id.
async fn admin_user_id(pool: &PgPool) -> String {
    sqlx::query_scalar(
        "SELECT id FROM users WHERE roles LIKE '%system_admin%' ORDER BY createat LIMIT 1",
    )
    .fetch_optional(pool)
    .await
    .expect("queries for an admin")
    .expect("the Go server creates a system admin at first signup; run the setup in the README")
}

/// A plain `system_user` row, inserted by the test. Only `id` and `lastlogin` are NOT NULL, so the
/// rest is what a real row would carry rather than what the schema demands.
async fn insert_user(pool: &PgPool, id: &str, username: &str, roles: &str) {
    sqlx::query(
        r#"
        INSERT INTO users (id, createat, updateat, deleteat, username, email, emailverified,
                           password, authdata, authservice, roles, allowmarketing, props,
                           notifyprops, lastpasswordupdate, failedattempts, locale, mfaactive,
                           mfasecret, position, timezone, remoteid, lastlogin)
        VALUES ($1, 1755000000000, 1755000000000, 0, $2, $2 || '@mmrs.invalid', true,
                '', NULL, '', $3, false, 'null'::jsonb, 'null'::jsonb, 1755000000000, 0, 'en',
                false, '', '', 'null'::jsonb, NULL, 0)
        "#,
    )
    .bind(id)
    .bind(username)
    .bind(roles)
    .execute(pool)
    .await
    .expect("inserts a test user");
}

async fn insert_role(pool: &PgPool, id: &str, name: &str, permissions: &str, delete_at: i64) {
    sqlx::query(
        r#"
        INSERT INTO roles (id, name, displayname, description, createat, updateat, deleteat,
                           permissions, schememanaged, builtin, schemeid)
        VALUES ($1, $2, 'MMRS Auth Role', 'inserted by mm-app tests', 1755000000000,
                1755000000000, $3, $4, false, false, NULL)
        "#,
    )
    .bind(id)
    .bind(name)
    .bind(delete_at)
    .bind(permissions)
    .execute(pool)
    .await
    .expect("inserts a test role");
}

fn session(user_id: &str, roles: &str) -> Session {
    Session {
        user_id: user_id.to_owned(),
        roles: roles.to_owned(),
        ..Default::default()
    }
}

#[tokio::test]
async fn roles_grant_permission_against_the_roles_go_wrote() {
    if !enabled() {
        return;
    }
    let _guard = DB.lock().await;
    let pool = pool().await;
    purge(&pool).await;
    let app = App::new(SqlStore::from_pool(pool.clone()));

    let grants = |names: &[&str], permission: &str| {
        let app = app.clone();
        let names: Vec<String> = names.iter().map(|n| (*n).to_owned()).collect();
        let permission = permission.to_owned();
        async move { app.roles_grant_permission(&names, &permission).await }
    };

    assert!(grants(&["system_user"], "create_team").await);
    assert!(!grants(&["system_user"], "manage_system").await);
    assert!(grants(&["system_admin"], "manage_system").await);
    // Any one role granting is enough.
    assert!(grants(&["system_user", "system_admin"], "manage_system").await);

    // No names, no roles, no grant — and the store short-circuits without a query.
    assert!(!grants(&[], "create_team").await);
    // A name that matches nothing is a denial, not an error.
    assert!(!grants(&["mmrs_no_such_role"], "create_team").await);
    // A permission that does not exist is a denial rather than a lookup failure.
    assert!(!grants(&["system_admin"], "mmrs_not_a_permission").await);
}

/// The store returns soft-deleted roles on purpose; this is the function that has to skip them.
/// Without an inserted row there is nothing in the database to skip, and dropping the `DeleteAt`
/// check passes the whole suite.
#[tokio::test]
async fn a_soft_deleted_role_grants_nothing() {
    if !enabled() {
        return;
    }
    let _guard = DB.lock().await;
    let pool = pool().await;
    purge(&pool).await;
    let app = App::new(SqlStore::from_pool(pool.clone()));

    let names = vec![DELETED_ROLE.to_owned()];

    // Alive first, to prove the permission really is on the row.
    insert_role(
        &pool,
        "mmrsauthdeletedrolexxxxxxx",
        DELETED_ROLE,
        " manage_system",
        0,
    )
    .await;
    assert!(
        app.roles_grant_permission(&names, "manage_system").await,
        "the row carries the permission while it is alive"
    );

    // Now soft-delete it. Same row, same permission, no grant.
    sqlx::query("UPDATE roles SET deleteat = 1755000000001 WHERE name = $1")
        .bind(DELETED_ROLE)
        .execute(&pool)
        .await
        .expect("soft-deletes the test role");

    assert!(
        !app.roles_grant_permission(&names, "manage_system").await,
        "a deleted role grants nothing"
    );

    purge(&pool).await;
}

#[tokio::test]
async fn session_checks_use_the_sessions_roles() {
    if !enabled() {
        return;
    }
    let _guard = DB.lock().await;
    let pool = pool().await;
    purge(&pool).await;
    let app = App::new(SqlStore::from_pool(pool.clone()));

    insert_user(&pool, PLAIN_USER, "mmrs_auth_plain", "system_user").await;
    let admin = session(&admin_user_id(&pool).await, "system_admin system_user");
    let plain = session(PLAIN_USER, "system_user");

    assert!(
        app.session_has_permission_to(&admin, &PERMISSION_MANAGE_SYSTEM)
            .await
    );
    assert!(
        !app.session_has_permission_to(&plain, &PERMISSION_MANAGE_SYSTEM)
            .await
    );
    assert!(
        app.session_has_permission_to(&plain, &PERMISSION_CREATE_TEAM)
            .await
    );

    // `Any` stops at the first grant, and denies when none of them lands.
    assert!(
        app.session_has_permission_to_any(
            &plain,
            &[&PERMISSION_MANAGE_SYSTEM, &PERMISSION_CREATE_TEAM]
        )
        .await
    );
    assert!(
        !app.session_has_permission_to_any(
            &plain,
            &[&PERMISSION_MANAGE_SYSTEM, &PERMISSION_EDIT_OTHER_USERS]
        )
        .await
    );

    // A team-scoped check with no membership falls back to the system roles, so the admin passes
    // for a team they are not a member of and the plain user does not.
    let team = "mmrsauthteamxxxxxxxxxxxxxx";
    assert!(
        app.session_has_permission_to_team(&admin, team, &PERMISSION_MANAGE_SYSTEM)
            .await
    );
    assert!(
        !app.session_has_permission_to_team(&plain, team, &PERMISSION_MANAGE_SYSTEM)
            .await
    );
}

/// `SessionHasPermissionToUser`'s five branches, in order. The fourth and fifth need a role that
/// grants `edit_other_users` **without** `manage_system` — no built-in role separates them, so the
/// test makes one.
#[tokio::test]
async fn session_has_permission_to_user_walks_its_branches_in_order() {
    if !enabled() {
        return;
    }
    let _guard = DB.lock().await;
    let pool = pool().await;
    purge(&pool).await;
    let app = App::new(SqlStore::from_pool(pool.clone()));

    insert_role(
        &pool,
        "mmrsautheditorrolexxxxxxxx",
        EDITOR_ROLE,
        " edit_other_users",
        0,
    )
    .await;

    insert_user(&pool, PLAIN_USER, "mmrs_auth_plain", "system_user").await;
    insert_user(&pool, OTHER_USER, "mmrs_auth_other", "system_user").await;
    let admin_id = admin_user_id(&pool).await;
    let admin = session(&admin_id, "system_admin system_user");
    let plain = session(PLAIN_USER, "system_user");
    let editor = session(PLAIN_USER, &format!("system_user {EDITOR_ROLE}"));

    // 1. An empty target denies.
    assert!(!app.session_has_permission_to_user(&admin, "").await);

    // 2. `manage_system` grants over anyone, including another system admin.
    assert!(app.session_has_permission_to_user(&admin, PLAIN_USER).await);
    assert!(app.session_has_permission_to_user(&admin, &admin_id).await);

    // 3. Acting on oneself grants without any permission at all — the self-shortcut the four
    //    migrated `me` routes rely on ([D-094]).
    assert!(app.session_has_permission_to_user(&plain, PLAIN_USER).await);

    // 4. Without `edit_other_users`, acting on someone else denies. The target has to be an
    //    **ordinary** user: against an admin, branch 5 denies anyway and masks this branch
    //    completely — a mutation deleting the `edit_other_users` requirement survived the whole
    //    suite until this assertion was added.
    assert!(!app.session_has_permission_to_user(&plain, OTHER_USER).await);
    assert!(!app.session_has_permission_to_user(&plain, &admin_id).await);

    // 5. With it, an ordinary target is allowed...
    assert!(
        app.session_has_permission_to_user(&editor, OTHER_USER)
            .await,
        "edit_other_users grants over a plain user"
    );
    //    ...but a **system admin** target still denies, which is the branch that exists to stop an
    //    editor from escalating.
    assert!(
        !app.session_has_permission_to_user(&editor, &admin_id).await,
        "a system admin target denies even with edit_other_users"
    );

    // A target that does not exist denies rather than erroring.
    assert!(
        !app.session_has_permission_to_user(&editor, "mmrsnosuchuserxxxxxxxxxxxx")
            .await
    );

    purge(&pool).await;
}

/// `GetRolesByNames` merges higher-scoped permissions into scheme-managed roles before anything
/// reads them. On Team Edition no scheme exists, so the merge finds nothing and must leave the
/// built-in roles **untouched** — a merge that fired unconditionally would replace every
/// scheme-managed role's permissions with the channel-scoped subset and silently drop the rest.
#[tokio::test]
async fn the_higher_scoped_merge_leaves_built_in_roles_alone_without_schemes() {
    if !enabled() {
        return;
    }
    let _guard = DB.lock().await;
    let pool = pool().await;
    purge(&pool).await;
    let app = App::new(SqlStore::from_pool(pool.clone()));

    let names = vec!["system_admin".to_owned(), "channel_user".to_owned()];
    let roles = app.get_roles_by_names(&names).await.expect("roles load");
    assert_eq!(roles.len(), 2);

    for role in &roles {
        assert!(role.scheme_managed, "both are scheme-managed built-ins");
        let permissions = role.permissions.as_deref().expect("never None from a read");
        assert!(!permissions.is_empty());
        if role.name == "system_admin" {
            // A merge would have cut this to channel scope only.
            assert!(
                permissions.iter().any(|p| p == "manage_system"),
                "system_admin keeps its system-scoped permissions"
            );
            assert!(
                permissions.len() > 100,
                "and all of them: {}",
                permissions.len()
            );
        }
    }
}

/// The higher-scoped merge, exercised **end to end through the app layer**.
///
/// `the_higher_scoped_merge_leaves_built_in_roles_alone_without_schemes` proves the merge does no
/// harm when there is no scheme; it cannot prove the merge happens, because with an empty `Schemes`
/// table merging and not merging are the same thing. A mutation that skipped the merge entirely
/// survived every other test here.
///
/// So this builds the graph — a channel scheme whose channel-user role is ours, a team with **no**
/// scheme, and a channel in it — which puts the built-in `channel_user` role in the higher scope.
/// The merge then has two visible effects, and the assertions are chosen so that skipping it fails
/// both ways: a permission the role holds but the channel scope does not is **dropped**, and one
/// the higher scope holds but the role does not is **added**.
#[tokio::test]
async fn get_roles_by_names_merges_a_scheme_managed_role_against_its_higher_scope() {
    if !enabled() {
        return;
    }
    let _guard = DB.lock().await;
    let pool = pool().await;
    purge(&pool).await;
    let app = App::new(SqlStore::from_pool(pool.clone()));

    let role = "mmrs_auth_merge_role";
    insert_scheme_managed_role(
        &pool,
        "mmrsauthmergerolexxxxxxxxx",
        role,
        " create_post manage_team",
    )
    .await;
    insert_channel_scheme(
        &pool,
        "mmrsauthmergeschemexxxxxxx",
        "mmrs_auth_merge_scheme",
        role,
    )
    .await;
    insert_team_no_scheme(&pool, "mmrsauthmergeteamxxxxxxxxx", "mmrs-auth-merge-team").await;
    insert_channel(
        &pool,
        "mmrsauthmergechannelxxxxxx",
        "mmrsauthmergeteamxxxxxxxxx",
        "mmrs-auth-merge-chan",
        "mmrsauthmergeschemexxxxxxx",
    )
    .await;

    let roles = app
        .get_roles_by_names(&[role.to_owned()])
        .await
        .expect("roles load");
    assert_eq!(roles.len(), 1);
    let merged = roles[0].permissions.as_deref().expect("never None");

    assert!(
        !merged.iter().any(|p| p == "manage_team"),
        "manage_team is not channel-scoped, so the merge drops it: {merged:?}"
    );
    assert!(
        merged.iter().any(|p| p == "read_channel"),
        "read_channel comes from the higher scope, which the role never held: {merged:?}"
    );
    assert!(
        merged.iter().any(|p| p == "create_post"),
        "create_post is moderated and held by both, so it survives: {merged:?}"
    );

    purge(&pool).await;
}

async fn insert_scheme_managed_role(pool: &PgPool, id: &str, name: &str, permissions: &str) {
    sqlx::query(
        r#"
        INSERT INTO roles (id, name, displayname, description, createat, updateat, deleteat,
                           permissions, schememanaged, builtin, schemeid)
        VALUES ($1, $2, 'MMRS Merge Role', 'inserted by mm-app tests', 1755000000000,
                1755000000000, 0, $3, true, false, NULL)
        "#,
    )
    .bind(id)
    .bind(name)
    .bind(permissions)
    .execute(pool)
    .await
    .expect("inserts a scheme-managed role");
}

async fn insert_channel_scheme(pool: &PgPool, id: &str, name: &str, channel_user_role: &str) {
    sqlx::query(
        r#"
        INSERT INTO schemes (id, name, displayname, description, createat, updateat, deleteat,
                             scope, defaultteamadminrole, defaultteamuserrole, defaultteamguestrole,
                             defaultchanneladminrole, defaultchanneluserrole, defaultchannelguestrole)
        VALUES ($1, $2, 'MMRS Merge Scheme', 'inserted by mm-app tests', 1755000000000,
                1755000000000, 0, 'channel', '', '', '', 'mmrs_auth_merge_admin', $3,
                'mmrs_auth_merge_guest')
        "#,
    )
    .bind(id)
    .bind(name)
    .bind(channel_user_role)
    .execute(pool)
    .await
    .expect("inserts a channel scheme");
}

/// A team with **no** scheme, which is what puts the built-in channel roles in the higher scope —
/// the third UNION branch of `channelHigherScopedPermissionsQuery`, and the only one that fires on
/// Team Edition.
async fn insert_team_no_scheme(pool: &PgPool, id: &str, name: &str) {
    sqlx::query(
        r#"
        INSERT INTO teams (id, createat, updateat, deleteat, displayname, name, type,
                           allowopeninvite, schemeid, cloudlimitsarchived)
        VALUES ($1, 1755000000000, 1755000000000, 0, 'MMRS Merge Team', $2, 'O', false, NULL, false)
        "#,
    )
    .bind(id)
    .bind(name)
    .execute(pool)
    .await
    .expect("inserts a team");
}

async fn insert_channel(pool: &PgPool, id: &str, team_id: &str, name: &str, scheme_id: &str) {
    sqlx::query(
        r#"
        INSERT INTO channels (id, createat, updateat, deleteat, teamid, type, displayname, name,
                              schemeid, defaultcategoryname, autotranslation, discoverable)
        VALUES ($1, 1755000000000, 1755000000000, 0, $2, 'O', 'MMRS Merge Channel', $3, $4, '',
                false, false)
        "#,
    )
    .bind(id)
    .bind(team_id)
    .bind(name)
    .bind(scheme_id)
    .execute(pool)
    .await
    .expect("inserts a channel");
}
