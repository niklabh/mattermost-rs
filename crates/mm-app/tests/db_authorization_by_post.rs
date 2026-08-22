//! Database-backed oracle for the by-post and read-channel checks, and for the two channel-store
//! queries under them.
//!
//! ```sh
//! docker compose up -d
//! export DATABASE_URL=postgres://mmuser:mmuser_password@localhost:5432/mattermost
//! MM_STORE_DB=1 cargo test -p mm-app --test db_authorization_by_post
//! ```
//!
//! # Why this one does not ask the Go server
//!
//! `db_channel_authorization.rs` puts every case to both servers because
//! `GET /channels/{id}/members/{id}` is gated by exactly the check under test, so 200-versus-403
//! is a real oracle. **No migrated route reaches the by-post checks**, and the Go routes that do
//! (`getPost`, `getFileInfosForPost`, …) sit behind a post store we have not ported, so there is
//! no equivalent single-gate endpoint to compare against. Asserting against the running server
//! here would mean asserting against a route whose *other* branches we have not reproduced, which
//! is a worse oracle than none.
//!
//! So these assert behaviour directly, and the protection against a stuck-closed port is
//! structural instead: **every test below contains a grant and a denial that differ by one
//! fixture fact**. A function that returned `false` unconditionally fails the grant half of each.
//!
//! # What is actually load-bearing here
//!
//! Four facts that no unit test against an unreachable store can reach:
//!
//! 1. `GetForPost` resolves a post in an **archived** channel, where `Get` would 404 it.
//! 2. `HasPermissionToTeam` ignores a **departed** member's roles (`DeleteAt != 0`) — the one
//!    predicate its session-scoped twin has no need of.
//! 3. The session- and user-scoped by-post checks **disagree for a DM post**, because only the
//!    session one guards its team fallback on a non-empty `TeamId`.
//! 4. The read-channel public fallback grants a non-member on an **open** channel and refuses the
//!    same user on a private one.

use mm_app::App;
use mm_model::permission::{PERMISSION_CREATE_POST, PERMISSION_READ_CHANNEL};
use mm_store::{ChannelStore, SqlStore};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

/// One set of `mmrsbp`-prefixed rows shared by every test, each purging before it seeds — so two
/// running concurrently delete each other's fixtures mid-assertion. Serialised for the same
/// reason `db_channel_authorization.rs` is.
static FIXTURES: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Ids are exactly 26 characters because that is what the column is; a shorter one inserts fine
/// and then fails to match a join on a padded comparison.
const TEAM: &str = "mmrsbpteamxxxxxxxxxxxxxxxx";
const OPEN_CHANNEL: &str = "mmrsbpopenxxxxxxxxxxxxxxxx";
const PRIVATE_CHANNEL: &str = "mmrsbpprivxxxxxxxxxxxxxxxx";
const ARCHIVED_CHANNEL: &str = "mmrsbparchxxxxxxxxxxxxxxxx";
/// A DM channel: `Type = 'D'` and, crucially, **`TeamId = ''`**. That empty team id is what makes
/// the two by-post twins disagree.
const DM_CHANNEL: &str = "mmrsbpdmxxxxxxxxxxxxxxxxxx";

const OPEN_POST: &str = "mmrsbppostopenxxxxxxxxxxxx";
const PRIVATE_POST: &str = "mmrsbppostprivxxxxxxxxxxxx";
const ARCHIVED_POST: &str = "mmrsbppostarchxxxxxxxxxxxx";
const DM_POST: &str = "mmrsbppostdmxxxxxxxxxxxxxx";
const ORPHAN_POST: &str = "mmrsbppostgonexxxxxxxxxxxx";

/// A role holding `read_public_channel` **and nothing else**, so the read-channel fallback is
/// separable from a blanket grant. `team_user` would also carry `read_channel`, which would make
/// the fallback indistinguishable from ordinary membership.
const TEAM_PUBLIC_ROLE: &str = "mmrs_bp_teampublic";
/// A role holding `create_post` and nothing else, for the by-post checks' system fallback.
const SYS_POST_ROLE: &str = "mmrs_bp_syspost";

fn enabled() -> bool {
    std::env::var("MM_STORE_DB").is_ok_and(|v| v == "1")
}

async fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set");
    PgPoolOptions::new()
        .max_connections(4)
        .acquire_timeout(std::time::Duration::from_secs(5))
        .connect(&url)
        .await
        .expect("connects to Postgres")
}

fn now() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("the clock is after 1970")
        .as_millis() as i64
}

async fn purge(pool: &PgPool) {
    for statement in [
        "DELETE FROM posts WHERE id LIKE 'mmrsbp%' OR channelid LIKE 'mmrsbp%'",
        "DELETE FROM channelmembers WHERE userid LIKE 'mmrsbp%' OR channelid LIKE 'mmrsbp%'",
        "DELETE FROM teammembers WHERE userid LIKE 'mmrsbp%' OR teamid LIKE 'mmrsbp%'",
        "DELETE FROM users WHERE id LIKE 'mmrsbp%'",
        "DELETE FROM channels WHERE id LIKE 'mmrsbp%'",
        "DELETE FROM teams WHERE id LIKE 'mmrsbp%'",
        "DELETE FROM roles WHERE name LIKE 'mmrs\\_bp\\_%'",
    ] {
        sqlx::query(statement)
            .execute(pool)
            .await
            .expect("purges leftover fixtures");
    }
}

async fn seed(pool: &PgPool) {
    purge(pool).await;

    sqlx::query(
        r#"
        INSERT INTO teams (id, createat, updateat, deleteat, displayname, name, description,
                           email, type, companyname, alloweddomains, inviteid, allowopeninvite,
                           schemeid, groupconstrained, cloudlimitsarchived)
        VALUES ($1, $2, $2, 0, 'mmrs bp team', 'mmrs-bp-team', '', '', 'O', '', '', $1, false,
                NULL, false, false)
        "#,
    )
    .bind(TEAM)
    .bind(now())
    .execute(pool)
    .await
    .expect("creates the fixture team");

    // (id, type, team, deleteat) — the DM carries an empty team id, which is the whole point of it.
    for (id, channel_type, team, delete_at) in [
        (OPEN_CHANNEL, "O", TEAM, 0),
        (PRIVATE_CHANNEL, "P", TEAM, 0),
        (ARCHIVED_CHANNEL, "P", TEAM, now()),
        (DM_CHANNEL, "D", "", 0),
    ] {
        sqlx::query(
            r#"
            INSERT INTO channels (id, createat, updateat, deleteat, teamid, type, displayname,
                                  name, header, purpose, lastpostat, totalmsgcount, extraupdateat,
                                  creatorid, schemeid, groupconstrained, shared, totalmsgcountroot,
                                  lastrootpostat, defaultcategoryname, autotranslation,
                                  discoverable)
            -- `channels.type` is a Postgres enum (`channel_type`), not text. A bound parameter
            -- arrives as text and has to be cast explicitly; the sibling suites get away without
            -- one only because they inline the type as a literal, which Postgres coerces.
            VALUES ($1, $2, $2, $3, $4, $5::channel_type, $1, $1, '', '', 0, 0, 0, '', NULL, NULL,
                    NULL, 0, 0, '', false, false)
            "#,
        )
        .bind(id)
        .bind(now())
        .bind(delete_at)
        .bind(team)
        .bind(channel_type)
        .execute(pool)
        .await
        .expect("creates a fixture channel");
    }

    for (id, channel) in [
        (OPEN_POST, OPEN_CHANNEL),
        (PRIVATE_POST, PRIVATE_CHANNEL),
        (ARCHIVED_POST, ARCHIVED_CHANNEL),
        (DM_POST, DM_CHANNEL),
    ] {
        sqlx::query(
            r#"
            INSERT INTO posts (id, createat, updateat, deleteat, userid, channelid, rootid,
                               originalid, message, type, props, hashtags, filenames, fileids,
                               hasreactions, editat, ispinned, remoteid)
            VALUES ($1, $2, $2, 0, '', $3, '', '', 'mmrs bp fixture', '', '{}'::jsonb, '', '[]',
                    '[]', false, 0, false, NULL)
            "#,
        )
        .bind(id)
        .bind(now())
        .bind(channel)
        .execute(pool)
        .await
        .expect("creates a fixture post");
    }

    for (id, name, permissions) in [
        (
            "mmrsbpteampublicrolexxxxxx",
            TEAM_PUBLIC_ROLE,
            "read_public_channel",
        ),
        ("mmrsbpsyspostrolexxxxxxxxx", SYS_POST_ROLE, "create_post"),
    ] {
        sqlx::query(
            "INSERT INTO roles (id, name, displayname, description, createat, updateat, deleteat,
                                permissions, schememanaged, builtin)
             VALUES ($1, $2, $2, '', 1, 1, 0, $3, false, false)",
        )
        .bind(id)
        .bind(name)
        .bind(permissions)
        .execute(pool)
        .await
        .expect("creates a probe role");
    }
}

/// A user with the given system roles, optionally a team membership (with its own roles and
/// `DeleteAt`) and optionally a channel membership.
async fn create_user(
    pool: &PgPool,
    tag: &str,
    system_roles: &str,
    team: Option<(&str, i64)>,
    channel: Option<&str>,
) -> String {
    let id = format!("mmrsbpu{tag:x<19}");
    sqlx::query(
        r#"
        INSERT INTO users (id, createat, updateat, deleteat, username, password, authdata,
                           authservice, email, emailverified, nickname, firstname, lastname,
                           position, roles, allowmarketing, props, notifyprops, lastpasswordupdate,
                           lastpictureupdate, failedattempts, locale, timezone, mfaactive,
                           mfasecret, remoteid)
        VALUES ($1, $2, $2, 0, $3, '', NULL, '', $4, true, '', '', '', '', $5, false,
                '{}'::jsonb, '{}'::jsonb, $2, 0, 0, 'en', '{}'::jsonb, false, '', NULL)
        "#,
    )
    .bind(&id)
    .bind(now())
    .bind(format!("mmrs-bp-{tag}"))
    .bind(format!("mmrs-bp-{tag}@example.com"))
    .bind(system_roles)
    .execute(pool)
    .await
    .expect("creates a fixture user");

    if let Some((team_roles, delete_at)) = team {
        sqlx::query(
            "INSERT INTO teammembers (teamid, userid, roles, deleteat, schemeuser, schemeadmin,
                                      schemeguest, createat)
             VALUES ($1, $2, $3, $4, false, false, false, $5)",
        )
        .bind(TEAM)
        .bind(&id)
        .bind(team_roles)
        .bind(delete_at)
        .bind(now())
        .execute(pool)
        .await
        .expect("creates a team membership");
    }

    if let Some(channel_id) = channel {
        sqlx::query(
            "INSERT INTO channelmembers (channelid, userid, roles, lastviewedat, msgcount,
                                         mentioncount, mentioncountroot, urgentmentioncount,
                                         msgcountroot, notifyprops, lastupdateat, schemeuser,
                                         schemeadmin, schemeguest)
             VALUES ($1, $2, '', 0, 0, 0, 0, 0, 0, '{}'::jsonb, $3, true, false, false)",
        )
        .bind(channel_id)
        .bind(&id)
        .bind(now())
        .execute(pool)
        .await
        .expect("creates a channel membership");
    }

    id
}

fn app(pool: &PgPool) -> App {
    App::new(SqlStore::from_pool(pool.clone()))
}

// ---------------------------------------------------------------------------------------------
// The two store queries.
// ---------------------------------------------------------------------------------------------

/// **`GetForPost` resolves a post in an archived channel; `Get` refuses the same channel by id.**
///
/// The two queries differ by exactly the predicates `get_for_post` omits — no `DeleteAt = 0` and
/// no `Type IN (...)` — and this asserts both halves so the omission is not mistaken for an
/// oversight. It is what lets the by-post checks answer at all for an archived channel.
#[tokio::test]
async fn get_for_post_sees_an_archived_channel_that_get_hides() {
    if !enabled() {
        return;
    }
    let _guard = FIXTURES.lock().await;
    let pool = pool().await;
    seed(&pool).await;
    let store = SqlStore::from_pool(pool.clone());

    let channel = store
        .channel()
        .get_for_post(ARCHIVED_POST)
        .await
        .expect("the archived channel resolves through its post");
    assert_eq!(channel.id, ARCHIVED_CHANNEL);
    assert_ne!(channel.delete_at, 0, "the fixture really is archived");

    // The living channel resolves too, so this is not "returns everything".
    let open = store
        .channel()
        .get_for_post(OPEN_POST)
        .await
        .expect("the open channel resolves");
    assert_eq!(open.id, OPEN_CHANNEL);

    // A post id nothing matches is a miss rather than a row.
    assert!(store.channel().get_for_post(ORPHAN_POST).await.is_err());

    purge(&pool).await;
}

/// `GetMemberForPost` finds the membership through the post's channel, and finds **nothing** for a
/// user who is not a member — the two answers the by-post checks branch on.
#[tokio::test]
async fn get_member_for_post_resolves_membership_through_the_post() {
    if !enabled() {
        return;
    }
    let _guard = FIXTURES.lock().await;
    let pool = pool().await;
    seed(&pool).await;
    let store = SqlStore::from_pool(pool.clone());

    let member_id = create_user(&pool, "mem", "system_user", None, Some(PRIVATE_CHANNEL)).await;
    let outsider_id = create_user(&pool, "out", "system_user", None, None).await;

    let member = store
        .channel()
        .get_member_for_post(PRIVATE_POST, &member_id)
        .await
        .expect("the member resolves through the post");
    assert_eq!(member.channel_id, PRIVATE_CHANNEL);
    assert_eq!(member.user_id, member_id);

    assert!(
        store
            .channel()
            .get_member_for_post(PRIVATE_POST, &outsider_id)
            .await
            .is_err(),
        "a non-member has no row to find"
    );

    purge(&pool).await;
}

// ---------------------------------------------------------------------------------------------
// The checks.
// ---------------------------------------------------------------------------------------------

/// **A departed team member's roles do not grant.**
///
/// `HasPermissionToTeam` filters on `DeleteAt == 0` (authorization.go:310) where its session-scoped
/// twin has no such check — the session's memberships were already filtered when it was built.
/// Two users identical but for that column, and the grant flips.
#[tokio::test]
async fn a_departed_team_member_loses_the_teams_permission() {
    if !enabled() {
        return;
    }
    let _guard = FIXTURES.lock().await;
    let pool = pool().await;
    seed(&pool).await;
    let app = app(&pool);

    let current = create_user(
        &pool,
        "cur",
        "system_user",
        Some((TEAM_PUBLIC_ROLE, 0)),
        None,
    )
    .await;
    let departed = create_user(
        &pool,
        "dep",
        "system_user",
        Some((TEAM_PUBLIC_ROLE, now())),
        None,
    )
    .await;

    assert!(
        app.has_permission_to_team(&current, TEAM, &PERMISSION_READ_PUBLIC_CHANNEL_CONST)
            .await,
        "a current member's team role grants"
    );
    assert!(
        !app.has_permission_to_team(&departed, TEAM, &PERMISSION_READ_PUBLIC_CHANNEL_CONST)
            .await,
        "the same role on a departed membership must not — authorization.go:310"
    );

    purge(&pool).await;
}

/// **The read-channel public fallback: an open channel grants a non-member, a private one does
/// not.**
///
/// The same user, the same team role, two channels differing only in `Type`. Without the fallback
/// both deny; without the type condition both grant. Also pins that the fallback reports
/// `is_member = false` (authorization.go:476), which it does by construction here since the user
/// really is not a member — the *interesting* half of that claim needs a member without
/// `read_channel_content` and is left to the unit suite's documentation.
#[tokio::test]
async fn the_public_channel_fallback_is_gated_on_the_channel_type() {
    if !enabled() {
        return;
    }
    let _guard = FIXTURES.lock().await;
    let pool = pool().await;
    seed(&pool).await;
    let app = app(&pool);

    let outsider = create_user(
        &pool,
        "pub",
        "system_user",
        Some((TEAM_PUBLIC_ROLE, 0)),
        None,
    )
    .await;

    let open = mm_model::channel::Channel {
        id: OPEN_CHANNEL.to_owned(),
        channel_type: mm_model::channel::CHANNEL_TYPE_OPEN.to_owned(),
        team_id: TEAM.to_owned(),
        ..Default::default()
    };
    let private = mm_model::channel::Channel {
        id: PRIVATE_CHANNEL.to_owned(),
        channel_type: mm_model::channel::CHANNEL_TYPE_PRIVATE.to_owned(),
        team_id: TEAM.to_owned(),
        ..Default::default()
    };

    assert_eq!(
        app.has_permission_to_read_channel(&outsider, &open).await,
        (true, false),
        "open channel: the team fallback grants, and reports non-membership"
    );
    assert_eq!(
        app.has_permission_to_read_channel(&outsider, &private)
            .await,
        (false, false),
        "private channel: no fallback exists"
    );

    // The mention check takes the same fallback, and the member-count check takes a *different*
    // team permission — `list_team_channels`, which this role does not hold.
    assert!(
        app.has_permission_to_resolve_channel_mention(&outsider, &open)
            .await
    );
    assert!(
        !app.has_permission_to_channel_member_count(&outsider, &open)
            .await,
        "member count wants list_team_channels, not read_public_channel — authorization.go:509"
    );

    purge(&pool).await;
}

/// **The two by-post twins disagree for a DM post, and this is the test that proves it.**
///
/// A DM channel has `TeamId = ''`. The session-scoped check guards its team fallback on a
/// non-empty team id and therefore falls through to the system check, which grants for a user
/// holding `create_post`. The user-scoped check calls `HasPermissionToTeam` unconditionally, that
/// function screens the empty id, and it denies — never reaching a system fallback.
///
/// Same user, same post, same permission, opposite answers. A port that "fixed" the asymmetry
/// would pass one half of this and fail the other.
#[tokio::test]
async fn the_by_post_twins_disagree_for_a_direct_message() {
    if !enabled() {
        return;
    }
    let _guard = FIXTURES.lock().await;
    let pool = pool().await;
    seed(&pool).await;
    let app = app(&pool);

    // Holds `create_post` system-wide, is a member of nothing.
    let user_id = create_user(&pool, "dm", SYS_POST_ROLE, None, None).await;
    let session = mm_model::session::Session {
        user_id: user_id.clone(),
        roles: SYS_POST_ROLE.to_owned(),
        ..Default::default()
    };

    assert!(
        app.session_has_permission_to_channel_by_post(&session, DM_POST, &PERMISSION_CREATE_POST)
            .await,
        "session variant: empty TeamId skips the team fallback and the system check grants"
    );
    assert!(
        !app.has_permission_to_channel_by_post(&user_id, DM_POST, &PERMISSION_CREATE_POST)
            .await,
        "user variant: calls HasPermissionToTeam with '' and is denied there — authorization.go:365"
    );

    // The control, and it sharpens the point rather than merely guarding it. Move the *same user*
    // and the *same permission* to a post in a **team** channel and the user-scoped check now
    // grants — because `HasPermissionToTeam` falls through to `HasPermissionTo`, which reads the
    // system role. So the DM denial above is not "this user has no permission"; it is precisely
    // the empty team id short-circuiting inside `HasPermissionToTeam` *before* that fallback can
    // run. One fixture fact — the channel's `TeamId` — flips the answer.
    assert!(
        app.has_permission_to_channel_by_post(&user_id, PRIVATE_POST, &PERMISSION_CREATE_POST)
            .await,
        "a team channel reaches HasPermissionToTeam's system fallback and grants"
    );

    purge(&pool).await;
}

/// A post that does not exist reaches the **system** check rather than denying, because Go guards
/// both store reads with `if err == nil` (authorization.go:211, :216). The user holding
/// `create_post` is granted for a post id that was never real.
///
/// This is the branch most likely to be "tidied" into a denial by a reader who assumes a missing
/// row is an error, so it is asserted in both directions.
#[tokio::test]
async fn a_missing_post_falls_through_to_the_system_check() {
    if !enabled() {
        return;
    }
    let _guard = FIXTURES.lock().await;
    let pool = pool().await;
    seed(&pool).await;
    let app = app(&pool);

    let granted = create_user(&pool, "gp", SYS_POST_ROLE, None, None).await;
    let denied = create_user(&pool, "dp", "system_user", None, None).await;

    let granted_session = mm_model::session::Session {
        user_id: granted.clone(),
        roles: SYS_POST_ROLE.to_owned(),
        ..Default::default()
    };
    let denied_session = mm_model::session::Session {
        user_id: denied.clone(),
        roles: "system_user".to_owned(),
        ..Default::default()
    };

    assert!(
        app.session_has_permission_to_channel_by_post(
            &granted_session,
            ORPHAN_POST,
            &PERMISSION_CREATE_POST
        )
        .await,
        "both reads miss, the system check grants"
    );
    assert!(
        !app.session_has_permission_to_channel_by_post(
            &denied_session,
            ORPHAN_POST,
            &PERMISSION_CREATE_POST
        )
        .await,
        "and denies for a user without the permission"
    );

    // `SessionHasPermissionToReadPost` has its own documented fallback for the same case, on
    // `read_channel_content` rather than the caller's permission (authorization.go:233).
    assert_eq!(
        app.session_has_permission_to_read_post(&granted_session, ORPHAN_POST)
            .await,
        (false, false),
        "SYS_POST_ROLE holds create_post, not read_channel_content"
    );

    purge(&pool).await;
}

/// Membership of the post's channel grants through the **channel** roles, without the team or
/// system branches being reached. The member holds no system permission at all, so a grant here
/// can only have come from the first branch.
#[tokio::test]
async fn channel_membership_grants_by_post_without_any_system_role() {
    if !enabled() {
        return;
    }
    let _guard = FIXTURES.lock().await;
    let pool = pool().await;
    seed(&pool).await;
    let app = app(&pool);

    // `schemeuser = true` on the membership resolves to `channel_user`, which grants
    // `read_channel`. The user's system role is `system_user`, which does not.
    let member = create_user(&pool, "cm", "system_user", None, Some(PRIVATE_CHANNEL)).await;
    let session = mm_model::session::Session {
        user_id: member.clone(),
        roles: "system_user".to_owned(),
        ..Default::default()
    };

    assert!(
        app.session_has_permission_to_channel_by_post(
            &session,
            PRIVATE_POST,
            &PERMISSION_READ_CHANNEL
        )
        .await,
        "channel_user grants read_channel through the membership branch"
    );

    // A non-member with the same system role is denied — the control that proves the grant above
    // came from the membership and not from `system_user`.
    let outsider = create_user(&pool, "co", "system_user", None, None).await;
    let outsider_session = mm_model::session::Session {
        user_id: outsider,
        roles: "system_user".to_owned(),
        ..Default::default()
    };
    assert!(
        !app.session_has_permission_to_channel_by_post(
            &outsider_session,
            PRIVATE_POST,
            &PERMISSION_READ_CHANNEL
        )
        .await
    );

    purge(&pool).await;
}

/// **`RestrictSystemAdmin` denies where the role check would have granted.**
///
/// This test exists because a mutation survived without it. The unit suite asserted the denial
/// against an *unreachable* store, so deleting the `restrict_system_admin` branch entirely still
/// produced `false` — the fallthrough reached the store, failed, and denied. Right answer, wrong
/// reason, indistinguishable.
///
/// Against a real database the two answers separate: the same user, the same permission, the same
/// store, and **only the config differs**. With the setting off the role check grants; with it on
/// the check must deny *before* reaching that role check. A port that drops the branch now fails
/// the second assertion.
#[tokio::test]
async fn the_restricted_admin_setting_denies_a_grant_the_roles_would_allow() {
    if !enabled() {
        return;
    }
    let _guard = FIXTURES.lock().await;
    let pool = pool().await;
    seed(&pool).await;

    // Holds `create_post` and nothing else, so the grant below can only come from the role check.
    let user_id = create_user(&pool, "ra", SYS_POST_ROLE, None, None).await;
    let session = mm_model::session::Session {
        user_id,
        roles: SYS_POST_ROLE.to_owned(),
        ..Default::default()
    };

    let permissive = App::with_config(
        SqlStore::from_pool(pool.clone()),
        mm_app::config::Config::default(),
    );
    assert!(
        permissive
            .session_has_permission_to_and_not_restricted_admin(&session, &PERMISSION_CREATE_POST)
            .await,
        "Go's default: the check is exactly SessionHasPermissionTo and the role grants"
    );

    let restricted = App::with_config(
        SqlStore::from_pool(pool.clone()),
        mm_app::config::Config {
            restrict_system_admin: true,
            ..mm_app::config::Config::default()
        },
    );
    assert!(
        !restricted
            .session_has_permission_to_and_not_restricted_admin(&session, &PERMISSION_CREATE_POST)
            .await,
        "restricted: denies outright rather than consulting the role — authorization.go:32"
    );

    purge(&pool).await;
}

/// `read_public_channel`, referenced by name so the fallback tests read as Go does. Declared here
/// rather than imported under an alias because the generated constant's name is long enough to
/// wrap every call site it appears in.
use mm_model::permission::PERMISSION_READ_PUBLIC_CHANNEL as PERMISSION_READ_PUBLIC_CHANNEL_CONST;
