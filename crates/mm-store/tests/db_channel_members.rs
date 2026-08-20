//! The two-server oracle for `SqlChannelStore::get_member`.
//!
//! ```sh
//! docker compose up -d
//! export DATABASE_URL=postgres://mmuser:mmuser_password@localhost:5432/mattermost
//! MM_STORE_DB=1 MM_PARITY_STACK=1 cargo test -p mm-store --test db_channel_members
//! ```
//!
//! # Why this is not a `reference/dump` fixture
//!
//! `getChannelRoles` and `channelMemberWithSchemeRoles.ToModel` are **unexported**
//! (channel_store.go:248 and :313), so the oracle program cannot call them — the same wall
//! `getTeamRoles` hit in [D-077]. The alternative is the one used there and extended here: put a
//! role shape into the shared row, ask **both servers** what it means, and compare. That is a
//! stronger oracle than a fixture in one respect — it exercises Go's real query against Go's real
//! schema, not a hand-assembled struct — and weaker in another: it needs the stack up.
//!
//! The measured answers are transcribed into unit tests in `channel_store.rs` so a regression
//! still fails without Docker. This file is what makes those transcriptions honest.
//!
//! # It mutates rows the Go server owns, and restores them
//!
//! There is no other way to reach the scheme branches: `Schemes` is an enterprise table and is
//! **empty** on Team Edition, so a scheme has to be inserted. Every row this file creates is
//! `mmrs`-prefixed, the shared `ChannelMembers` row's original values are captured before the
//! first case and written back after the last, and both the purge and the restore run even when a
//! case fails — assertions are collected, not panicked on, precisely so the restore is reached.

use mm_store::channel_store::get_member;
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

const GO: &str = "http://localhost:8065";
const LOGIN_ID: &str = "slice@example.com";
const PASSWORD: &str = "Slice-Test-1234";

fn db_enabled() -> bool {
    std::env::var("MM_STORE_DB").is_ok_and(|v| v == "1")
}

fn stack_enabled() -> bool {
    std::env::var("MM_PARITY_STACK").is_ok_and(|v| v == "1")
}

/// `go_and_rust_agree_on_every_channel_role_shape` rewrites the shared membership row fourteen
/// times and restores it; `the_two_role_resolvers_agree_for_an_ordinary_member` reads the same
/// row twice and compares. Interleaved, the reader sees two different mid-mutation states and
/// reports a divergence that is neither server's. Serialised for the same reason
/// `db_channel_unread.rs` is.
static SHARED_ROW: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for MM_STORE_DB=1");
    PgPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .expect("connects to Postgres")
}

/// The channel scheme and the team scheme, and the six roles they name.
///
/// The roles are **copies of the real `channel_user`/`channel_admin`/`channel_guest` rows**,
/// permissions included. That is not cosmetic: the api4 handler runs
/// `SessionHasPermissionToChannel` before it will answer, and that check resolves the member's
/// effective role names against the `Roles` table. Invented names would resolve to nothing, Go
/// would return 403, and the oracle would measure the permission layer instead of the store.
const CHANNEL_SCHEME_ID: &str = "mmrsscheme000000000000chan";
const TEAM_SCHEME_ID: &str = "mmrsscheme000000000000team";

async fn purge(pool: &PgPool) {
    for statement in [
        "UPDATE channels SET schemeid = NULL WHERE schemeid LIKE 'mmrs%'",
        "UPDATE teams SET schemeid = NULL WHERE schemeid LIKE 'mmrs%'",
        "DELETE FROM schemes WHERE id LIKE 'mmrs%'",
        "DELETE FROM roles WHERE name LIKE 'mmrs\\_%'",
    ] {
        sqlx::query(statement)
            .execute(pool)
            .await
            .expect("purges leftover test rows");
    }
}

/// Copies the three built-in channel roles under a prefix and returns the new names.
async fn clone_channel_roles(pool: &PgPool, prefix: &str) -> (String, String, String) {
    sqlx::query(
        r#"
        INSERT INTO roles (id, name, displayname, description, createat, updateat, deleteat,
                           permissions, schememanaged, builtin)
        SELECT 'mmrs' || substr(md5($1 || r.name), 1, 22),
               $1 || r.name,
               r.displayname, r.description, r.createat, r.updateat, 0,
               r.permissions, true, false
          FROM roles r
         WHERE r.name IN ('channel_user', 'channel_admin', 'channel_guest')
        "#,
    )
    .bind(prefix)
    .execute(pool)
    .await
    .expect("clones the built-in channel roles");

    (
        format!("{prefix}channel_guest"),
        format!("{prefix}channel_user"),
        format!("{prefix}channel_admin"),
    )
}

#[allow(clippy::too_many_arguments)]
async fn insert_scheme(pool: &PgPool, id: &str, scope: &str, guest: &str, user: &str, admin: &str) {
    sqlx::query(
        r#"
        INSERT INTO schemes (id, name, displayname, description, createat, updateat, deleteat,
                             scope, defaultteamadminrole, defaultteamuserrole, defaultteamguestrole,
                             defaultchanneladminrole, defaultchanneluserrole, defaultchannelguestrole,
                             defaultplaybookadminrole, defaultplaybookmemberrole,
                             defaultrunadminrole, defaultrunmemberrole)
        VALUES ($1, $1, 'mmrs oracle scheme', '', 1, 1, 0, $2, '', '', '', $5, $4, $3, '', '', '', '')
        "#,
    )
    .bind(id)
    .bind(scope)
    .bind(guest)
    .bind(user)
    .bind(admin)
    .execute(pool)
    .await
    .expect("inserts the oracle scheme");
}

struct Case {
    name: &'static str,
    roles: &'static str,
    scheme_guest: bool,
    scheme_user: bool,
    scheme_admin: bool,
    channel_scheme: bool,
    team_scheme: bool,
}

/// The shared `ChannelMembers` row, its channel, and the token to ask Go with.
struct Target {
    channel_id: String,
    user_id: String,
    team_id: String,
    token: String,
}

/// Discovers the row rather than hardcoding ids.
///
/// Ids are minted per database, so a constant survives exactly until the volume is recreated —
/// which [D-130] required, and which turned hardcoded ids into "permission denied" rather than
/// into anything that named the real problem.
async fn discover(pool: &PgPool, client: &reqwest::Client) -> Target {
    let response = client
        .post(format!("{GO}/api/v4/users/login"))
        .json(&serde_json::json!({ "login_id": LOGIN_ID, "password": PASSWORD }))
        .send()
        .await
        .expect("the Go server is reachable — is `docker compose up -d` running?");
    assert_eq!(response.status(), 200, "login against Go failed");

    let token = response
        .headers()
        .get("token")
        .expect("Go returns the session token in a `Token` header")
        .to_str()
        .expect("the token is ASCII")
        .to_owned();
    let user: serde_json::Value = response.json().await.expect("login returns the user");
    let user_id = user["id"]
        .as_str()
        .expect("the user carries an id")
        .to_owned();

    // A public channel on a real team: the team join is what makes the team-scheme fallback
    // reachable at all, and a DM channel has an empty `TeamId`.
    let row = sqlx::query_as::<_, (String, String)>(
        r#"
        SELECT cm.channelid, c.teamid
          FROM channelmembers cm
          INNER JOIN channels c ON c.id = cm.channelid
         WHERE cm.userid = $1
           AND c.teamid <> ''
           AND c.deleteat = 0
         ORDER BY cm.channelid
         LIMIT 1
        "#,
    )
    .bind(&user_id)
    .fetch_one(pool)
    .await
    .expect("the fixture user is a member of at least one team channel");

    Target {
        channel_id: row.0,
        user_id,
        team_id: row.1,
        token,
    }
}

#[tokio::test]
async fn go_and_rust_agree_on_every_channel_role_shape() {
    if !db_enabled() || !stack_enabled() {
        eprintln!("skipped: needs MM_STORE_DB=1 and MM_PARITY_STACK=1");
        return;
    }
    let _shared_row = SHARED_ROW.lock().await;

    let pool = pool().await;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .expect("client builds");

    purge(&pool).await;
    let target = discover(&pool, &client).await;

    // Capture the row as the Go server left it, so the restore is to *its* values rather than to
    // anything this file believes they should be.
    let original = sqlx::query_as::<_, (Option<String>, Option<bool>, Option<bool>, Option<bool>)>(
        "SELECT roles, schemeguest, schemeuser, schemeadmin FROM channelmembers
          WHERE channelid = $1 AND userid = $2",
    )
    .bind(&target.channel_id)
    .bind(&target.user_id)
    .fetch_one(&pool)
    .await
    .expect("the target membership exists");

    let (cs_guest, cs_user, cs_admin) = clone_channel_roles(&pool, "mmrs_cs_").await;
    let (ts_guest, ts_user, ts_admin) = clone_channel_roles(&pool, "mmrs_ts_").await;
    insert_scheme(
        &pool,
        CHANNEL_SCHEME_ID,
        "channel",
        &cs_guest,
        &cs_user,
        &cs_admin,
    )
    .await;
    insert_scheme(
        &pool,
        TEAM_SCHEME_ID,
        "team",
        &ts_guest,
        &ts_user,
        &ts_admin,
    )
    .await;

    let cases = [
        Case {
            name: "plain member",
            roles: "",
            scheme_guest: false,
            scheme_user: true,
            scheme_admin: false,
            channel_scheme: false,
            team_scheme: false,
        },
        Case {
            name: "member and admin",
            roles: "",
            scheme_guest: false,
            scheme_user: true,
            scheme_admin: true,
            channel_scheme: false,
            team_scheme: false,
        },
        Case {
            name: "no flags at all",
            roles: "",
            scheme_guest: false,
            scheme_user: false,
            scheme_admin: false,
            channel_scheme: false,
            team_scheme: false,
        },
        Case {
            name: "guest only",
            roles: "",
            scheme_guest: true,
            scheme_user: false,
            scheme_admin: false,
            channel_scheme: false,
            team_scheme: false,
        },
        Case {
            name: "explicit roles keep their order",
            roles: "custom_one custom_two",
            scheme_guest: false,
            scheme_user: true,
            scheme_admin: false,
            channel_scheme: false,
            team_scheme: false,
        },
        Case {
            name: "un-migrated scheme role in the column",
            roles: "custom_one channel_guest custom_two",
            scheme_guest: false,
            scheme_user: true,
            scheme_admin: true,
            channel_scheme: false,
            team_scheme: false,
        },
        Case {
            name: "all three ids in the column, no flags",
            roles: "channel_guest channel_user channel_admin",
            scheme_guest: false,
            scheme_user: false,
            scheme_admin: false,
            channel_scheme: false,
            team_scheme: false,
        },
        Case {
            name: "a team role id is explicit to a channel member",
            roles: "team_admin team_user",
            scheme_guest: false,
            scheme_user: true,
            scheme_admin: false,
            channel_scheme: false,
            team_scheme: false,
        },
        Case {
            name: "whitespace runs in the column",
            roles: "  custom_one \t custom_two  ",
            scheme_guest: false,
            scheme_user: true,
            scheme_admin: false,
            channel_scheme: false,
            team_scheme: false,
        },
        Case {
            name: "channel scheme replaces the constants",
            roles: "",
            scheme_guest: false,
            scheme_user: true,
            scheme_admin: true,
            channel_scheme: true,
            team_scheme: false,
        },
        Case {
            name: "team scheme is the fallback",
            roles: "",
            scheme_guest: false,
            scheme_user: true,
            scheme_admin: true,
            channel_scheme: false,
            team_scheme: true,
        },
        Case {
            name: "channel scheme beats team scheme",
            roles: "",
            scheme_guest: false,
            scheme_user: true,
            scheme_admin: true,
            channel_scheme: true,
            team_scheme: true,
        },
        Case {
            name: "guest through the channel scheme",
            roles: "",
            scheme_guest: true,
            scheme_user: true,
            scheme_admin: true,
            channel_scheme: true,
            team_scheme: true,
        },
        Case {
            name: "un-migrated role plus a scheme",
            roles: "custom_one channel_admin",
            scheme_guest: false,
            scheme_user: true,
            scheme_admin: false,
            channel_scheme: true,
            team_scheme: false,
        },
    ];

    let mut failures: Vec<String> = Vec::new();

    for case in &cases {
        sqlx::query(
            "UPDATE channelmembers SET roles = $3, schemeguest = $4, schemeuser = $5,
                    schemeadmin = $6
              WHERE channelid = $1 AND userid = $2",
        )
        .bind(&target.channel_id)
        .bind(&target.user_id)
        .bind(case.roles)
        .bind(case.scheme_guest)
        .bind(case.scheme_user)
        .bind(case.scheme_admin)
        .execute(&pool)
        .await
        .expect("sets the case's role shape");

        set_scheme(
            &pool,
            "channels",
            &target.channel_id,
            case.channel_scheme.then_some(CHANNEL_SCHEME_ID),
        )
        .await;
        set_scheme(
            &pool,
            "teams",
            &target.team_id,
            case.team_scheme.then_some(TEAM_SCHEME_ID),
        )
        .await;

        let go_body: serde_json::Value = client
            .get(format!(
                "{GO}/api/v4/channels/{}/members/{}",
                target.channel_id, target.user_id
            ))
            .header("Authorization", format!("Bearer {}", target.token))
            .send()
            .await
            .expect("Go answers")
            .json()
            .await
            .expect("Go returns JSON");

        match get_member(&pool, &target.channel_id, &target.user_id).await {
            Ok(member) => {
                let ours = serde_json::to_value(&member).expect("the member serialises");
                if ours != go_body {
                    failures.push(format!(
                        "case {:?}\n  go:   {go_body}\n  rust: {ours}",
                        case.name
                    ));
                }
            }
            Err(error) => failures.push(format!("case {:?} errored: {error}", case.name)),
        }
    }

    // Restore before asserting: a panic here would unwind straight past any trailing cleanup and
    // leave the shared row holding a test's role shape, which the next run would then measure.
    set_scheme(&pool, "channels", &target.channel_id, None).await;
    set_scheme(&pool, "teams", &target.team_id, None).await;
    sqlx::query(
        "UPDATE channelmembers SET roles = $3, schemeguest = $4, schemeuser = $5, schemeadmin = $6
          WHERE channelid = $1 AND userid = $2",
    )
    .bind(&target.channel_id)
    .bind(&target.user_id)
    .bind(&original.0)
    .bind(original.1)
    .bind(original.2)
    .bind(original.3)
    .execute(&pool)
    .await
    .expect("restores the shared membership row");
    purge(&pool).await;

    assert!(
        failures.is_empty(),
        "{} of {} shapes disagree:\n{}",
        failures.len(),
        cases.len(),
        failures.join("\n")
    );
}

async fn set_scheme(pool: &PgPool, table: &str, id: &str, scheme: Option<&str>) {
    // `table` is one of two literals chosen above, never anything a caller supplies — the only
    // reason this is interpolated rather than bound is that Postgres cannot bind an identifier.
    let statement = match table {
        "channels" => "UPDATE channels SET schemeid = $2 WHERE id = $1",
        "teams" => "UPDATE teams SET schemeid = $2 WHERE id = $1",
        other => panic!("unexpected table {other}"),
    };
    sqlx::query(statement)
        .bind(id)
        .bind(scheme)
        .execute(pool)
        .await
        .expect("sets the scheme id");
}

/// A membership that does not exist is `NotFound`, not an empty member.
#[tokio::test]
async fn a_missing_membership_is_not_found() {
    if !db_enabled() {
        eprintln!("skipped: needs MM_STORE_DB=1");
        return;
    }
    let pool = pool().await;
    let error = get_member(
        &pool,
        "aaaaaaaaaaaaaaaaaaaaaaaaaa",
        "bbbbbbbbbbbbbbbbbbbbbbbbbb",
    )
    .await
    .expect_err("no such membership");
    assert!(error.is_not_found(), "{error}");
    assert_eq!(
        error.to_string(),
        "ChannelMember not found: channelId=aaaaaaaaaaaaaaaaaaaaaaaaaa, userId=bbbbbbbbbbbbbbbbbbbbbbbbbb"
    );
}

/// The `Channels` join is INNER, so a membership whose channel is gone is invisible.
///
/// Not reachable through the API — Go deletes memberships with the channel — so it is asserted
/// directly: insert a membership pointing at a channel id that does not exist, and confirm the
/// lookup misses rather than returning a member with empty scheme defaults.
#[tokio::test]
async fn an_orphaned_membership_does_not_resolve() {
    if !db_enabled() {
        eprintln!("skipped: needs MM_STORE_DB=1");
        return;
    }
    let pool = pool().await;
    let channel_id = "mmrsorphanchannel000000000";
    let user_id = "mmrsorphanuser000000000000";

    sqlx::query("DELETE FROM channelmembers WHERE channelid = $1")
        .bind(channel_id)
        .execute(&pool)
        .await
        .expect("clears any leftover");
    sqlx::query(
        "INSERT INTO channelmembers (channelid, userid, roles, notifyprops, schemeuser)
         VALUES ($1, $2, '', '{}'::jsonb, true)",
    )
    .bind(channel_id)
    .bind(user_id)
    .execute(&pool)
    .await
    .expect("inserts the orphan");

    let result = get_member(&pool, channel_id, user_id).await;

    sqlx::query("DELETE FROM channelmembers WHERE channelid = $1")
        .bind(channel_id)
        .execute(&pool)
        .await
        .expect("removes the orphan");

    assert!(
        result.as_ref().err().is_some_and(|e| e.is_not_found()),
        "an orphaned membership must not resolve, got {result:?}"
    );
}

/// `Get` is "the **message** channel with this id", not "the channel with this id".
///
/// `messageChannelTypes` (channel_store.go:39) is `O`, `P`, `D`, `G`. A board channel has a
/// `Channels` row and is deliberately invisible to this method — Go reaches it through
/// `GetBoardChannel` instead. Dropping the filter would make a permission check answer questions
/// about a channel Go considers missing, so it is asserted rather than assumed.
#[tokio::test]
async fn get_returns_message_channels_and_hides_the_others() {
    if !db_enabled() {
        eprintln!("skipped: needs MM_STORE_DB=1");
        return;
    }
    let pool = pool().await;

    let team = "mmrsgettteamxxxxxxxxxxxxxx";
    let private = "mmrsgetprivatexxxxxxxxxxxx";
    let board = "mmrsgetboardxxxxxxxxxxxxxx";

    for statement in [
        "DELETE FROM channels WHERE id LIKE 'mmrsget%'",
        "DELETE FROM teams WHERE id LIKE 'mmrsget%'",
    ] {
        sqlx::query(statement)
            .execute(&pool)
            .await
            .expect("clears leftovers");
    }

    sqlx::query(
        "INSERT INTO teams (id, createat, updateat, deleteat, displayname, name, description,
                            email, type, companyname, alloweddomains, inviteid, allowopeninvite,
                            groupconstrained, cloudlimitsarchived)
         VALUES ($1, 1, 1, 0, 'mmrs get team', 'mmrs-get-team', '', '', 'O', '', '', $1, false,
                 false, false)",
    )
    .bind(team)
    .execute(&pool)
    .await
    .expect("creates the team");

    for (id, channel_type, name) in [
        (private, "P", "mmrs-get-private"),
        (board, "BO", "mmrs-get-board"),
    ] {
        sqlx::query(
            "INSERT INTO channels (id, createat, updateat, deleteat, teamid, type, displayname,
                                   name, header, purpose, lastpostat, totalmsgcount, extraupdateat,
                                   creatorid, totalmsgcountroot, lastrootpostat,
                                   defaultcategoryname, autotranslation, discoverable)
             VALUES ($1, 1, 1, 0, $2, $3::channel_type, $4, $4, '', '', 0, 0, 0, '', 0, 0, '',
                     false, false)",
        )
        .bind(id)
        .bind(team)
        .bind(channel_type)
        .bind(name)
        .execute(&pool)
        .await
        .expect("creates the channel");
    }

    let found = mm_store::channel_store::get(&pool, private).await;
    let hidden = mm_store::channel_store::get(&pool, board).await;

    for statement in [
        "DELETE FROM channels WHERE id LIKE 'mmrsget%'",
        "DELETE FROM teams WHERE id LIKE 'mmrsget%'",
    ] {
        sqlx::query(statement)
            .execute(&pool)
            .await
            .expect("cleans up");
    }

    let channel = found.expect("a private channel is a message channel");
    assert_eq!(channel.channel_type, "P");
    assert_eq!(channel.team_id, team);
    assert!(
        !channel.policy_enforced && !channel.policy_is_active,
        "no access-control policy exists for this id, so both computed columns are false"
    );
    assert!(
        hidden.as_ref().err().is_some_and(|e| e.is_not_found()),
        "a board channel must be invisible to Get, got {hidden:?}"
    );
}

/// The two resolvers agree for a plain member, and the plural read is keyed by channel id.
///
/// This is the *agreement* half of [D-142] — worth pinning alongside the disagreement, because a
/// port that made them differ everywhere would be just as wrong as one that made them identical.
#[tokio::test]
async fn the_two_role_resolvers_agree_for_an_ordinary_member() {
    if !db_enabled() || !stack_enabled() {
        eprintln!("skipped: needs MM_STORE_DB=1 and MM_PARITY_STACK=1");
        return;
    }
    let _shared_row = SHARED_ROW.lock().await;
    let pool = pool().await;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .expect("client builds");
    let target = discover(&pool, &client).await;

    let member = get_member(&pool, &target.channel_id, &target.user_id)
        .await
        .expect("the shared membership resolves");
    let all =
        mm_store::channel_store::get_all_channel_members_for_user(&pool, &target.user_id, true)
            .await
            .expect("the plural read succeeds");

    assert_eq!(
        all.get(&target.channel_id).map(String::as_str),
        Some(member.roles.as_str()),
        "with no scheme and no literal scheme id in the column, both resolvers agree"
    );
    assert!(
        !all.is_empty(),
        "the plural read returns every membership, not just the one asked about"
    );
}
