//! The cross-server oracle for `App::session_has_permission_to_channel`.
//!
//! ```sh
//! docker compose up -d
//! export DATABASE_URL=postgres://mmuser:mmuser_password@localhost:5432/mattermost
//! MM_STORE_DB=1 MM_PARITY_STACK=1 cargo test -p mm-app --test db_channel_authorization
//! ```
//!
//! # Why this asks the Go server rather than asserting a boolean
//!
//! `SessionHasPermissionToChannel` is six branches over three stores, and every one of them denies
//! on failure. A test that only asserted our own answer would pass just as happily against a
//! function that returned `false` unconditionally. So each case is put to **both** servers: Go
//! answers through `GET /api/v4/channels/{id}/members/{id}`, whose only gate is this exact check
//! (api4/channel.go), and 200-versus-403 is compared against our boolean.
//!
//! The suite therefore contains grants *and* denials by construction, and a stuck-closed port
//! fails on the grants.
//!
//! # Sessions are injected, and Go accepts them
//!
//! Go authenticates a token by reading the shared `Sessions` table, so a row inserted here logs in
//! against the Go server exactly as a real one does — measured, not assumed. That is what makes it
//! possible to ask Go the same question as a **non-admin**: the fixture user `sliceuser` is a
//! `system_admin`, and `manage_system` grants at step 5, so every case would pass for it and the
//! oracle would be vacuous.
//!
//! # Go caches; the fixtures work around it rather than fighting it
//!
//! `GetAllChannelMembersForUser` is cached per user and the `Roles` table is cached by name. Two
//! consequences shape everything below: **each case gets its own fresh user**, so no membership is
//! ever read twice, and **no role's permissions are ever mutated** — the cases lean only on the
//! fixed facts that `channel_user` grants `read_channel` while `channel_admin`, `team_user` and
//! `system_user` do not. Mutating a role mid-run produced a stale answer and cost an hour; it is
//! recorded here so it costs nobody else one.

use mm_app::App;
use mm_model::permission::PERMISSION_READ_CHANNEL;
use mm_store::{SessionStore, SqlStore};

/// The three tests share one set of `mmrsca`-prefixed fixture rows and each purges before
/// seeding, so two running interleaved delete each other's fixtures mid-assertion — measured as
/// `teams_pkey` duplicate inserts under load. Serialised for the same reason
/// `db_channel_unread.rs` in `mm-store` is.
static FIXTURES: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

const GO: &str = "http://localhost:8065";

/// Ids must be exactly 26 characters or `RequireUserId` rejects the request with a 400 before the
/// permission check runs — which looks like a denial and is not one.
const TEAM: &str = "mmrscateamxxxxxxxxxxxxxxxx";
const CHANNEL: &str = "mmrscachanxxxxxxxxxxxxxxxx";
const MISSING_CHANNEL: &str = "mmrscagonexxxxxxxxxxxxxxxx";
/// An **archived** channel — `DeleteAt != 0`. Go passes `includeDeleted = true` to the membership
/// read, so a member of an archived channel still holds their channel roles.
const ARCHIVED_CHANNEL: &str = "mmrscaarchxxxxxxxxxxxxxxxx";

fn enabled() -> bool {
    std::env::var("MM_STORE_DB").is_ok_and(|v| v == "1")
        && std::env::var("MM_PARITY_STACK").is_ok_and(|v| v == "1")
}

async fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set");
    PgPoolOptions::new()
        .max_connections(4)
        .connect(&url)
        .await
        .expect("connects to Postgres")
}

async fn purge(pool: &PgPool) {
    for statement in [
        "DELETE FROM sessions WHERE id LIKE 'mmrsca%'",
        "DELETE FROM channelmembers WHERE userid LIKE 'mmrsca%' OR channelid LIKE 'mmrsca%'",
        "DELETE FROM teammembers WHERE userid LIKE 'mmrsca%' OR teamid LIKE 'mmrsca%'",
        "DELETE FROM users WHERE id LIKE 'mmrsca%'",
        "DELETE FROM channels WHERE id LIKE 'mmrsca%'",
        "DELETE FROM teams WHERE id LIKE 'mmrsca%'",
        "DELETE FROM roles WHERE name LIKE 'mmrs\\_ca\\_%'",
    ] {
        sqlx::query(statement)
            .execute(pool)
            .await
            .expect("purges leftover fixtures");
    }
}

fn now() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("the clock is after 1970")
        .as_millis() as i64
}

async fn create_team_and_channel(pool: &PgPool) {
    sqlx::query(
        r#"
        INSERT INTO teams (id, createat, updateat, deleteat, displayname, name, description,
                           email, type, companyname, alloweddomains, inviteid, allowopeninvite,
                           schemeid, groupconstrained, cloudlimitsarchived)
        VALUES ($1, $2, $2, 0, 'mmrs ca team', 'mmrs-ca-team', '', '', 'O', '', '', $1, false,
                NULL, false, false)
        "#,
    )
    .bind(TEAM)
    .bind(now())
    .execute(pool)
    .await
    .expect("creates the fixture team");

    // Private, so nothing in the api4 handler can take a public-channel shortcut around the check.
    sqlx::query(
        r#"
        INSERT INTO channels (id, createat, updateat, deleteat, teamid, type, displayname, name,
                              header, purpose, lastpostat, totalmsgcount, extraupdateat, creatorid,
                              schemeid, groupconstrained, shared, totalmsgcountroot,
                              lastrootpostat, defaultcategoryname, autotranslation, discoverable)
        VALUES ($1, $2, $2, 0, $3, 'P', 'mmrs ca channel', 'mmrs-ca-channel', '', '', 0, 0, 0, '',
                NULL, NULL, NULL, 0, 0, '', false, false)
        "#,
    )
    .bind(CHANNEL)
    .bind(now())
    .bind(TEAM)
    .execute(pool)
    .await
    .expect("creates the fixture channel");

    sqlx::query(
        r#"
        INSERT INTO channels (id, createat, updateat, deleteat, teamid, type, displayname, name,
                              header, purpose, lastpostat, totalmsgcount, extraupdateat, creatorid,
                              schemeid, groupconstrained, shared, totalmsgcountroot,
                              lastrootpostat, defaultcategoryname, autotranslation, discoverable)
        VALUES ($1, $2, $2, $2, $3, 'P', 'mmrs ca archived', 'mmrs-ca-archived', '', '', 0, 0, 0,
                '', NULL, NULL, NULL, 0, 0, '', false, false)
        "#,
    )
    .bind(ARCHIVED_CHANNEL)
    .bind(now())
    .bind(TEAM)
    .execute(pool)
    .await
    .expect("creates the archived fixture channel");
}

/// Two roles that exist only to make branches 5 and 6 observable.
///
/// Without them the suite has a hole that a mutation run found: dropping the `manage_system`
/// branch entirely left every case unchanged, because the only actor holding `manage_system` was a
/// `system_admin`, and `system_admin` also grants `read_channel` outright — so branch 6 granted
/// whatever branch 5 would have. A role holding **`manage_system` and nothing else** separates
/// them, and a team role holding **`read_channel` and nothing else** separates branch 6 from a
/// blanket denial.
const SYS_ROLE: &str = "mmrs_ca_sysrole";
const TEAM_ROLE: &str = "mmrs_ca_teamrole";

async fn create_probe_roles(pool: &PgPool) {
    for (id, name, permissions) in [
        ("mmrscasysrolexxxxxxxxxxxxx", SYS_ROLE, "manage_system"),
        ("mmrscateamrolexxxxxxxxxxxx", TEAM_ROLE, "read_channel"),
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

/// `(Roles column, SchemeUser, SchemeAdmin, SchemeGuest)` for a channel membership.
type ChannelRoles<'a> = (&'a str, bool, bool, bool);

/// `(tag, system roles, in team, channel membership, what the case probes)`.
type Case<'a> = (&'a str, &'a str, bool, Option<ChannelRoles<'a>>, &'a str);

/// A user, a session for it, and optionally a team membership and a channel membership.
struct Actor {
    id: String,
    token: String,
}

#[allow(clippy::too_many_arguments)]
async fn create_actor(
    pool: &PgPool,
    tag: &str,
    system_roles: &str,
    in_team: bool,
    channel_roles: Option<ChannelRoles<'_>>,
) -> Actor {
    create_actor_with_team_roles(
        pool,
        tag,
        system_roles,
        in_team.then_some(""),
        channel_roles,
    )
    .await
}

/// `team_roles` is `None` for "not in the team" and `Some(roles)` for a membership whose `Roles`
/// column holds exactly that.
async fn create_actor_with_team_roles(
    pool: &PgPool,
    tag: &str,
    system_roles: &str,
    team_roles: Option<&str>,
    channel_roles: Option<ChannelRoles<'_>>,
) -> Actor {
    // `mmrsca` + a 2-char tag + 18 filler = 26.
    let id = format!("mmrsca{tag}{}", "u".repeat(18));
    let token = format!("mmrsca{tag}{}", "t".repeat(18));
    let session_id = format!("mmrsca{tag}{}", "s".repeat(18));
    assert_eq!(id.len(), 26, "ids must be 26 chars or api4 returns 400");

    sqlx::query(
        r#"
        INSERT INTO users (id, createat, updateat, deleteat, username, email, emailverified,
                           password, authservice, nickname, firstname, lastname, position, roles,
                           allowmarketing, props, notifyprops, lastpasswordupdate,
                           lastpictureupdate, failedattempts, locale, timezone, mfaactive,
                           mfasecret, lastlogin)
        VALUES ($1, $2, $2, 0, $3, $3 || '@mmrs.invalid', true, '', '', '', '', '', '', $4, false,
                '{}'::jsonb, '{}'::jsonb, $2, 0, 0, 'en', '{}'::jsonb, false, '', $2)
        "#,
    )
    .bind(&id)
    .bind(now())
    .bind(format!("mmrs_ca_{tag}"))
    .bind(system_roles)
    .execute(pool)
    .await
    .expect("creates the actor");

    if let Some(roles) = team_roles {
        sqlx::query(
            "INSERT INTO teammembers (teamid, userid, roles, deleteat, schemeuser, schemeadmin,
                                      schemeguest, createat)
             VALUES ($1, $2, $4, 0, true, false, false, $3)",
        )
        .bind(TEAM)
        .bind(&id)
        .bind(now())
        .bind(roles)
        .execute(pool)
        .await
        .expect("adds the actor to the team");
    }

    if let Some((roles, scheme_user, scheme_admin, scheme_guest)) = channel_roles {
        sqlx::query(
            "INSERT INTO channelmembers (channelid, userid, roles, notifyprops, schemeuser,
                                         schemeadmin, schemeguest, lastviewedat, msgcount,
                                         mentioncount, mentioncountroot, msgcountroot,
                                         urgentmentioncount, lastupdateat)
             VALUES ($1, $2, $3, '{}'::jsonb, $4, $5, $6, 0, 0, 0, 0, 0, 0, $7)",
        )
        .bind(CHANNEL)
        .bind(&id)
        .bind(roles)
        .bind(scheme_user)
        .bind(scheme_admin)
        .bind(scheme_guest)
        .bind(now())
        .execute(pool)
        .await
        .expect("adds the actor to the channel");
    }

    sqlx::query(
        "INSERT INTO sessions (id, token, createat, expiresat, lastactivityat, userid, deviceid,
                               roles, isoauth, props, expirednotify, voipdeviceid)
         VALUES ($1, $2, $3, $4, $3, $5, '', $6, false, '{}'::jsonb, false, '')",
    )
    .bind(&session_id)
    .bind(&token)
    .bind(now())
    .bind(now() + 30 * 86_400_000)
    .bind(&id)
    .bind(system_roles)
    .execute(pool)
    .await
    .expect("creates the session");

    Actor { id, token }
}

/// Go's answer: does `GET /channels/{channel}/members/{user}` get past the permission gate?
async fn join_channel(pool: &PgPool, actor: &Actor, channel_id: &str, roles: &str) {
    sqlx::query(
        "INSERT INTO channelmembers (channelid, userid, roles, notifyprops, schemeuser,
                                     schemeadmin, schemeguest, lastviewedat, msgcount,
                                     mentioncount, mentioncountroot, msgcountroot,
                                     urgentmentioncount, lastupdateat)
         VALUES ($1, $2, $3, '{}'::jsonb, false, false, false, 0, 0, 0, 0, 0, 0, $4)",
    )
    .bind(channel_id)
    .bind(&actor.id)
    .bind(roles)
    .bind(now())
    .execute(pool)
    .await
    .expect("adds the actor to the channel");
}

async fn go_grants(client: &reqwest::Client, actor: &Actor, channel_id: &str) -> bool {
    let response = client
        .get(format!(
            "{GO}/api/v4/channels/{channel_id}/members/{}",
            actor.id
        ))
        .header("Authorization", format!("Bearer {}", actor.token))
        .send()
        .await
        .expect("the Go server is reachable");

    let status = response.status().as_u16();
    assert!(
        status == 200 || status == 403 || status == 404,
        "unexpected status {status} from Go — the gate was not what was measured: {}",
        response.text().await.unwrap_or_default()
    );
    // 404 is the member lookup failing *after* the gate passed, so it still counts as a grant.
    status != 403
}

#[tokio::test]
async fn go_and_rust_agree_on_channel_permission_checks() {
    if !enabled() {
        eprintln!("skipped: needs MM_STORE_DB=1 and MM_PARITY_STACK=1");
        return;
    }

    let pool = pool().await;
    let _fixtures = FIXTURES.lock().await;
    purge(&pool).await;
    create_team_and_channel(&pool).await;
    create_probe_roles(&pool).await;

    let store = SqlStore::from_pool(pool.clone());
    let app = App::new(store.clone());
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .expect("client builds");

    let specs: Vec<Case> = vec![
        (
            "aa",
            "system_user",
            true,
            Some(("channel_user", false, false, false)),
            "a literal channel_user in the Roles column grants",
        ),
        (
            "bb",
            "system_user",
            true,
            Some(("channel_admin", false, false, false)),
            "channel_admin does not carry read_channel, so it denies",
        ),
        (
            "cc",
            "system_user",
            true,
            Some(("", true, false, false)),
            "SchemeUser implies channel_user with no scheme, and grants",
        ),
        (
            "dd",
            "system_user",
            true,
            Some(("", false, true, false)),
            "SchemeAdmin alone implies only channel_admin, and denies",
        ),
        (
            "ee",
            "system_user",
            true,
            None,
            "a team member who is not in the channel falls back to team_user, which denies",
        ),
        (
            "ff",
            "system_user",
            false,
            None,
            "a stranger to both team and channel denies",
        ),
        (
            "gg",
            "system_admin system_user",
            false,
            None,
            "manage_system grants at step 5 without any membership",
        ),
        (
            "hh",
            "system_user",
            true,
            Some(("channel_user channel_admin", false, false, false)),
            "one granting role among several is enough",
        ),
    ];

    let mut failures: Vec<String> = Vec::new();

    for (tag, system_roles, in_team, channel_roles, description) in &specs {
        let actor = create_actor(&pool, tag, system_roles, *in_team, *channel_roles).await;

        // Read the session back through the store, so both servers answer from the same row
        // rather than from a struct this test assembled.
        let session = store
            .session()
            .get(&actor.token)
            .await
            .expect("the injected session loads");

        let (ours, _is_member) = app
            .session_has_permission_to_channel(&session, CHANNEL, &PERMISSION_READ_CHANNEL)
            .await;
        let theirs = go_grants(&client, &actor, CHANNEL).await;

        if ours != theirs {
            failures.push(format!(
                "case {tag:?} ({description}): go={theirs}, rust={ours}"
            ));
        }
    }

    // ---- branch 5, isolated: `manage_system` and nothing else --------------------------------
    //
    // The actor holds a role granting only `manage_system`, is in neither the team nor the
    // channel, and asks for `read_channel`. Branch 5 grants on the strength of `manage_system`
    // alone; every other branch has nothing to grant on. Deleting branch 5 turns this into a
    // denial, which is what makes it a test rather than a restatement.
    let sysrole = create_actor(&pool, "sy", SYS_ROLE, false, None).await;
    let session = store
        .session()
        .get(&sysrole.token)
        .await
        .expect("the injected session loads");
    let (ours, _) = app
        .session_has_permission_to_channel(&session, CHANNEL, &PERMISSION_READ_CHANNEL)
        .await;
    let theirs = go_grants(&client, &sysrole, CHANNEL).await;
    if !ours || ours != theirs {
        failures.push(format!(
            "manage_system alone must grant at branch 5: go={theirs}, rust={ours}"
        ));
    }

    // ---- branch 6, isolated: the team fallback -----------------------------------------------
    //
    // In the team with a team role granting `read_channel`, not in the channel, no
    // `manage_system`. Only branch 6 can grant. This one also proves the session's `TeamMembers`
    // are populated by the store — `SessionHasPermissionToTeam` reads the roles off the session,
    // not the database, so an unhydrated session would deny here and nowhere else.
    let teamrole =
        create_actor_with_team_roles(&pool, "tr", "system_user", Some(TEAM_ROLE), None).await;
    let session = store
        .session()
        .get(&teamrole.token)
        .await
        .expect("the injected session loads");
    let (ours, _) = app
        .session_has_permission_to_channel(&session, CHANNEL, &PERMISSION_READ_CHANNEL)
        .await;
    let theirs = go_grants(&client, &teamrole, CHANNEL).await;
    if !ours || ours != theirs {
        failures.push(format!(
            "a team role granting read_channel must grant at branch 6: go={theirs}, rust={ours}"
        ));
    }

    // ---- the archived channel: `includeDeleted = true` is not cosmetic ------------------------
    //
    // Archiving a channel makes it read-only, not invisible to its members. Go asks for the
    // membership with `includeDeleted = true`, so a member of an archived channel still holds
    // their channel roles and still passes a `read_channel` check. Passing `false` — the reading
    // that "deleted means gone" invites — denies every member of every archived channel, and no
    // case on a live channel can tell the difference.
    let archived = create_actor(&pool, "ar", "system_user", true, None).await;
    join_channel(&pool, &archived, ARCHIVED_CHANNEL, "channel_user").await;
    let session = store
        .session()
        .get(&archived.token)
        .await
        .expect("the injected session loads");
    let (ours, _) = app
        .session_has_permission_to_channel(&session, ARCHIVED_CHANNEL, &PERMISSION_READ_CHANNEL)
        .await;
    let theirs = go_grants(&client, &archived, ARCHIVED_CHANNEL).await;
    if !ours || ours != theirs {
        failures.push(format!(
            "a member of an archived channel must still be granted: go={theirs}, rust={ours}"
        ));
    }

    // A channel that does not exist denies on both sides — Go's `GetChannel` 404s and the check
    // returns false rather than falling through to the team or system branches.
    let prober = create_actor(&pool, "zz", "system_admin system_user", true, None).await;
    let session = store
        .session()
        .get(&prober.token)
        .await
        .expect("the injected session loads");
    let (ours, _) = app
        .session_has_permission_to_channel(&session, MISSING_CHANNEL, &PERMISSION_READ_CHANNEL)
        .await;
    let theirs = go_grants(&client, &prober, MISSING_CHANNEL).await;
    if ours || ours != theirs {
        failures.push(format!(
            "a missing channel must deny even for a system admin: go={theirs}, rust={ours}"
        ));
    }

    // An empty channel id denies before anything is looked up. Go cannot be asked — the router
    // does not match the path at all — so this half is ours alone.
    let (empty, _) = app
        .session_has_permission_to_channel(&session, "", &PERMISSION_READ_CHANNEL)
        .await;
    if empty {
        failures.push("an empty channel id must deny, even for a system admin".to_owned());
    }

    purge(&pool).await;

    assert!(
        failures.is_empty(),
        "{} of {} cases disagree:\n{}",
        failures.len(),
        specs.len() + 5,
        failures.join("\n")
    );
}

/// `is_member` is the second return value and is not a by-product: Go's comment says it exists so
/// a caller can audit access *without* membership. A system admin granted at step 5 must report
/// `false`, and a member granted at step 4 must report `true`.
#[tokio::test]
async fn is_member_distinguishes_membership_from_privilege() {
    if !enabled() {
        eprintln!("skipped: needs MM_STORE_DB=1 and MM_PARITY_STACK=1");
        return;
    }

    let pool = pool().await;
    let _fixtures = FIXTURES.lock().await;
    purge(&pool).await;
    create_team_and_channel(&pool).await;

    let store = SqlStore::from_pool(pool.clone());
    let app = App::new(store.clone());

    let member = create_actor(
        &pool,
        "mm",
        "system_user",
        true,
        Some(("channel_user", false, false, false)),
    )
    .await;
    let admin = create_actor(&pool, "ad", "system_admin system_user", false, None).await;
    // A member whose channel roles do **not** grant: still a member, still denied by step 4, and
    // denied overall. This is the case that separates `is_member` from `has_permission`.
    let weak = create_actor(
        &pool,
        "wk",
        "system_user",
        true,
        Some(("channel_admin", false, false, false)),
    )
    .await;

    let mut results = Vec::new();
    for actor in [&member, &admin, &weak] {
        let session = store
            .session()
            .get(&actor.token)
            .await
            .expect("the injected session loads");
        results.push(
            app.session_has_permission_to_channel(&session, CHANNEL, &PERMISSION_READ_CHANNEL)
                .await,
        );
    }

    purge(&pool).await;

    assert_eq!(results[0], (true, true), "a granting member");
    assert_eq!(
        results[1],
        (true, false),
        "a system admin grants without being a member"
    );
    assert_eq!(
        results[2],
        (false, true),
        "a member whose roles do not grant is still a member"
    );
}

/// The unrestricted branch, which the cross-server oracle **cannot** reach.
///
/// `Session.IsUnrestricted` returns `Session.Local` (session.go:103), and `Local` is not a column —
/// it is set by the local-mode socket handler on a session it constructs in memory. So no injected
/// row can be unrestricted and no HTTP request can carry one. This half is asserted against our
/// implementation alone, and says so.
///
/// What it pins is the **order**, which is the part a reading gets wrong: Go fetches the channel
/// *before* it checks `IsUnrestricted` (authorization.go:106 versus :114), so an unrestricted
/// session asking about a channel that does not exist is **denied**. Hoisting the unrestricted
/// check to the top — the obvious "privileged callers skip the lookup" optimisation — grants
/// access to a nonexistent channel, and every other case in this file still passes.
#[tokio::test]
async fn an_unrestricted_session_still_needs_the_channel_to_exist() {
    if !enabled() {
        eprintln!("skipped: needs MM_STORE_DB=1 and MM_PARITY_STACK=1");
        return;
    }

    let pool = pool().await;
    let _fixtures = FIXTURES.lock().await;
    purge(&pool).await;
    create_team_and_channel(&pool).await;

    let app = App::new(SqlStore::from_pool(pool.clone()));
    let local = mm_model::session::Session {
        user_id: "mmrscalocalxxxxxxxxxxxxxxx".to_owned(),
        roles: String::new(),
        local: true,
        ..Default::default()
    };
    assert!(local.is_unrestricted(), "the fixture is what it claims");

    let existing = app
        .session_has_permission_to_channel(&local, CHANNEL, &PERMISSION_READ_CHANNEL)
        .await;
    let missing = app
        .session_has_permission_to_channel(&local, MISSING_CHANNEL, &PERMISSION_READ_CHANNEL)
        .await;
    let empty = app
        .session_has_permission_to_channel(&local, "", &PERMISSION_READ_CHANNEL)
        .await;

    purge(&pool).await;

    assert_eq!(
        existing,
        (true, false),
        "an unrestricted session grants, and is never reported as a member"
    );
    assert_eq!(
        missing,
        (false, false),
        "the channel is fetched before the unrestricted check, so a missing channel denies"
    );
    assert_eq!(empty, (false, false), "an empty id denies first of all");
}
