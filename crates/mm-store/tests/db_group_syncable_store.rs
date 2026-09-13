//! The syncable half of the group store against a real Postgres — the four membership queries
//! whose SQL was translated from squirrel, the two `UpdateMembersRole` statements, the write
//! rules on a `GroupTeams`/`GroupChannels` row, and the newest-job read.
//!
//! ```sh
//! docker compose up -d
//! export DATABASE_URL=postgres://mmuser:mmuser_password@localhost:5432/mattermost
//! MM_STORE_DB=1 cargo test -p mm-store --test db_group_syncable_store
//! ```
//!
//! # Why these are here and not only in the parity suite
//!
//! Over HTTP the sync is a background task on both servers writing one database, so a parity
//! test sees only that *a* member arrived; the predicates that decide *which* — the `LEFT JOIN`
//! switched by `reAddRemovedMembers`, the `since` cut-off, the bot exemption, the channel types
//! `ChannelMembersToRemove` will touch — are visible only where the query is called directly,
//! with rows built to sit on each side of each one.
//!
//! Every row this file plants carries the `mmrsgsync` prefix and is swept first; the users
//! referenced by memberships do not exist, since none of the queries join `Users`.

use mm_model::group_syncable::{GroupSyncable, GroupSyncableType};
use mm_store::group_syncable_store::GroupSyncableStore;
use mm_store::{JobStore, SqlChannelStore, SqlGroupStore, SqlJobStore, SqlTeamStore, StoreError};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

static FIXTURES: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const GROUP: &str = "mmrsgsyncgroup000000000001";
const OTHER_GROUP: &str = "mmrsgsyncgroup000000000002";
const DELETED_GROUP: &str = "mmrsgsyncgroup000000000003";
const TEAM: &str = "mmrsgsyncteam0000000000001";
const OTHER_TEAM: &str = "mmrsgsyncteam0000000000002";
const OPEN_CHANNEL: &str = "mmrsgsyncchan0000000000001";
const DM_CHANNEL: &str = "mmrsgsyncchan0000000000002";
const OTHER_CHANNEL: &str = "mmrsgsyncchan0000000000003";
const NEW_MEMBER: &str = "mmrsgsyncuser0000000000001";
const OLD_MEMBER: &str = "mmrsgsyncuser0000000000002";
const ALREADY_IN: &str = "mmrsgsyncuser0000000000003";
const BOT: &str = "mmrsgsyncuser0000000000004";
const STRANGER: &str = "mmrsgsyncuser0000000000005";
const GUEST: &str = "mmrsgsyncuser0000000000006";
const LEFT_BEFORE: &str = "mmrsgsyncuser0000000000007";

fn db_enabled() -> bool {
    std::env::var("MM_STORE_DB").is_ok_and(|v| v == "1")
}

async fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for MM_STORE_DB=1");
    PgPoolOptions::new()
        .max_connections(2)
        .acquire_timeout(std::time::Duration::from_secs(5))
        .connect(&url)
        .await
        .expect("connects to Postgres")
}

fn team_type() -> GroupSyncableType {
    GroupSyncableType::from(GroupSyncableType::TEAM)
}

fn channel_type() -> GroupSyncableType {
    GroupSyncableType::from(GroupSyncableType::CHANNEL)
}

async fn sweep(pool: &PgPool) {
    for statement in [
        "DELETE FROM groupmembers WHERE groupid LIKE 'mmrsgsync%'",
        "DELETE FROM groupteams WHERE groupid LIKE 'mmrsgsync%'",
        "DELETE FROM groupchannels WHERE groupid LIKE 'mmrsgsync%'",
        "DELETE FROM usergroups WHERE id LIKE 'mmrsgsync%'",
        "DELETE FROM teammembers WHERE teamid LIKE 'mmrsgsync%'",
        "DELETE FROM channelmembers WHERE channelid LIKE 'mmrsgsync%'",
        "DELETE FROM channelmemberhistory WHERE channelid LIKE 'mmrsgsync%'",
        "DELETE FROM bots WHERE userid LIKE 'mmrsgsync%'",
        "DELETE FROM channels WHERE id LIKE 'mmrsgsync%'",
        "DELETE FROM teams WHERE id LIKE 'mmrsgsync%'",
        "DELETE FROM jobs WHERE id LIKE 'mmrsgsync%'",
    ] {
        sqlx::query(statement)
            .execute(pool)
            .await
            .expect("the sweep runs");
    }
}

/// Two teams and three channels, a constrained team and a constrained open channel among them;
/// three groups (one deleted); memberships arranged so every predicate has a row on each side.
async fn plant(pool: &PgPool) {
    sweep(pool).await;
    for (team, constrained) in [(TEAM, true), (OTHER_TEAM, false)] {
        sqlx::query(
            // Every string column Go scans is set: a NULL in `description`, `email`,
            // `companyname`, `alloweddomains` or `inviteid` is a 500 on `GET /teams` for as
            // long as the row exists, and a store test's rows exist until its sweep.
            "INSERT INTO teams (id, createat, updateat, deleteat, displayname, name, description,
                                email, type, companyname, alloweddomains, inviteid,
                                allowopeninvite, groupconstrained, lastteamiconupdate)
             VALUES ($1, 1, 1, 0, 'mmrs gsync', $1, '', '', 'O', '', '', $1, false, $2, 0)",
        )
        .bind(team)
        .bind(constrained)
        .execute(pool)
        .await
        .expect("inserts the team");
    }
    for (channel, kind, constrained) in [
        (OPEN_CHANNEL, "O", true),
        (DM_CHANNEL, "D", true),
        (OTHER_CHANNEL, "P", false),
    ] {
        sqlx::query(
            "INSERT INTO channels (id, createat, updateat, deleteat, teamid, type, displayname,
                                   name, totalmsgcount, totalmsgcountroot, groupconstrained)
             VALUES ($1, 1, 1, 0, $2, $3::channel_type, $1, $1, 0, 0, $4)",
        )
        .bind(channel)
        .bind(TEAM)
        .bind(kind)
        .bind(constrained)
        .execute(pool)
        .await
        .expect("inserts the channel");
    }
    for (group, delete_at) in [(GROUP, 0i64), (OTHER_GROUP, 0), (DELETED_GROUP, 5)] {
        sqlx::query(
            "INSERT INTO usergroups (id, name, displayname, description, source, remoteid,
                                     createat, updateat, deleteat, allowreference)
             VALUES ($1, $1, $1, '', 'ldap', $1, 1, 1, $2, true)",
        )
        .bind(group)
        .bind(delete_at)
        .execute(pool)
        .await
        .expect("inserts the group");
    }
    // Memberships: `NEW_MEMBER` joined the group at 1000, `OLD_MEMBER` at 600; `ALREADY_IN` and
    // `LEFT_BEFORE` are in the group too; `STRANGER` is in a deleted group only, `BOT` in none.
    for (group, user, create_at, delete_at) in [
        (GROUP, NEW_MEMBER, 1000i64, 0i64),
        // Exactly at the `since` the test below uses, so `>=` and `>` give different answers.
        (GROUP, OLD_MEMBER, 600, 0),
        (GROUP, ALREADY_IN, 100, 0),
        (GROUP, LEFT_BEFORE, 100, 0),
        // BOT is deliberately in **no** group: a bot in a linked group is never removable
        // anyway, and the bot exemption in `*MembersToRemove` was unobservable until this
        // changed — the mutation dropping it survived.
        (DELETED_GROUP, STRANGER, 100, 0),
        (GROUP, GUEST, 100, 7), // a deleted membership
    ] {
        sqlx::query(
            "INSERT INTO groupmembers (groupid, userid, createat, deleteat) VALUES ($1, $2, $3, $4)",
        )
        .bind(group)
        .bind(user)
        .bind(create_at)
        .bind(delete_at)
        .execute(pool)
        .await
        .expect("inserts the membership");
    }
    // Links: GROUP → TEAM (auto-add, admin), GROUP → OPEN_CHANNEL (auto-add, admin),
    // GROUP → DM_CHANNEL (auto-add), DELETED_GROUP → TEAM, OTHER_GROUP → OTHER_TEAM (soft-deleted).
    for (group, team, auto_add, admin, delete_at) in [
        (GROUP, TEAM, true, true, 0i64),
        (DELETED_GROUP, TEAM, true, true, 0),
        (OTHER_GROUP, OTHER_TEAM, true, true, 9),
    ] {
        sqlx::query(
            "INSERT INTO groupteams (groupid, teamid, autoadd, schemeadmin, createat, updateat, deleteat)
             VALUES ($1, $2, $3, $4, 500, 500, $5)",
        )
        .bind(group)
        .bind(team)
        .bind(auto_add)
        .bind(admin)
        .bind(delete_at)
        .execute(pool)
        .await
        .expect("inserts the team link");
    }
    for (group, channel) in [(GROUP, OPEN_CHANNEL), (GROUP, DM_CHANNEL)] {
        sqlx::query(
            "INSERT INTO groupchannels (groupid, channelid, autoadd, schemeadmin, createat, updateat, deleteat)
             VALUES ($1, $2, true, true, 500, 500, 0)",
        )
        .bind(group)
        .bind(channel)
        .execute(pool)
        .await
        .expect("inserts the channel link");
    }
    // Existing team members of TEAM: ALREADY_IN (in the group), STRANGER (in no live group),
    // BOT (a bot), GUEST (a guest, in the group only through a deleted membership).
    for (user, admin, guest) in [
        (ALREADY_IN, false, false),
        (STRANGER, true, false),
        (BOT, false, false),
        (GUEST, false, true),
    ] {
        sqlx::query(
            "INSERT INTO teammembers (teamid, userid, roles, deleteat, schemeuser, schemeadmin,
                                      schemeguest, createat)
             VALUES ($1, $2, '', 0, true, $3, $4, 1)",
        )
        .bind(TEAM)
        .bind(user)
        .bind(admin)
        .bind(guest)
        .execute(pool)
        .await
        .expect("inserts the team member");
    }
    // Existing channel members of the open and the DM channel: the same four in each.
    for channel in [OPEN_CHANNEL, DM_CHANNEL] {
        for (user, admin, guest) in [
            (ALREADY_IN, false, false),
            (STRANGER, true, false),
            (BOT, false, false),
            (GUEST, false, true),
        ] {
            sqlx::query(
                "INSERT INTO channelmembers (channelid, userid, roles, lastviewedat, msgcount,
                     mentioncount, mentioncountroot, urgentmentioncount, notifyprops, lastupdateat,
                     schemeuser, schemeadmin, schemeguest, msgcountroot)
                 VALUES ($1, $2, '', 0, 0, 0, 0, 0, '{}'::jsonb, 0, true, $3, $4, 0)",
            )
            .bind(channel)
            .bind(user)
            .bind(admin)
            .bind(guest)
            .execute(pool)
            .await
            .expect("inserts the channel member");
        }
    }
    // Channel history: ALREADY_IN joined and stayed; LEFT_BEFORE joined and left.
    for (user, leave) in [(ALREADY_IN, None::<i64>), (LEFT_BEFORE, Some(50))] {
        sqlx::query(
            "INSERT INTO channelmemberhistory (channelid, userid, jointime, leavetime)
             VALUES ($1, $2, 10, $3)",
        )
        .bind(OPEN_CHANNEL)
        .bind(user)
        .bind(leave)
        .execute(pool)
        .await
        .expect("inserts the history");
    }
    sqlx::query(
        "INSERT INTO bots (userid, description, ownerid, createat, updateat, deleteat, lasticonupdate)
         VALUES ($1, '', $1, 1, 1, 0, 0)",
    )
    .bind(BOT)
    .execute(pool)
    .await
    .expect("inserts the bot");
}

fn ids<T, F: Fn(&T) -> String>(rows: &[T], f: F) -> Vec<String> {
    let mut v: Vec<String> = rows.iter().map(f).collect();
    v.sort();
    v
}

/// `TeamMembersToAdd` with the join switched on and off, the `since` cut-off, and the scope.
#[tokio::test]
async fn team_members_to_add_switches_on_re_add_since_and_scope() {
    if !db_enabled() {
        return;
    }
    let _guard = FIXTURES.lock().await;
    let pool = pool().await;
    plant(&pool).await;
    let store = SqlGroupStore::new(pool.clone());

    // Re-adding: everyone in a live group linked auto-add to a live team, members already in the
    // team included; the deleted group and the deleted link contribute nothing.
    let all = store.team_members_to_add(0, None, true).await.unwrap();
    assert_eq!(
        ids(&all, |p| format!("{}:{}", p.team_id, p.user_id)),
        [NEW_MEMBER, OLD_MEMBER, ALREADY_IN, LEFT_BEFORE]
            .iter()
            .map(|u| format!("{TEAM}:{u}"))
            .collect::<Vec<_>>()
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>()
    );

    // Not re-adding: only users with no TeamMembers row at all, and only where the membership
    // or the link is at least `since` old. With since=600 the link (500) is too old; NEW_MEMBER
    // (1000) qualifies and so does OLD_MEMBER (exactly 600 — the comparison is `>=`); ALREADY_IN
    // has a row and is excluded either way.
    let fresh = store.team_members_to_add(600, None, false).await.unwrap();
    assert_eq!(ids(&fresh, |p| p.user_id.clone()), [NEW_MEMBER, OLD_MEMBER]);
    // since=0: the link's UpdateAt (500) qualifies everyone without a row.
    let since_zero = store.team_members_to_add(0, None, false).await.unwrap();
    assert_eq!(
        ids(&since_zero, |p| p.user_id.clone()),
        [NEW_MEMBER, OLD_MEMBER, LEFT_BEFORE]
            .iter()
            .map(|s| s.to_string())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>()
    );
    // Scoped to a team that has no live link.
    let scoped = store
        .team_members_to_add(0, Some(OTHER_TEAM), true)
        .await
        .unwrap();
    assert!(scoped.is_empty());
    let scoped = store
        .team_members_to_add(0, Some(TEAM), true)
        .await
        .unwrap();
    assert_eq!(scoped.len(), all.len());
    sweep(&pool).await;
}

/// `ChannelMembersToAdd`: the history join excludes anyone with **any** history row — even one
/// whose `LeaveTime` says they left — when not re-adding.
#[tokio::test]
async fn channel_members_to_add_reads_the_history_not_the_membership() {
    if !db_enabled() {
        return;
    }
    let _guard = FIXTURES.lock().await;
    let pool = pool().await;
    plant(&pool).await;
    let store = SqlGroupStore::new(pool.clone());

    let all = store
        .channel_members_to_add(0, Some(OPEN_CHANNEL), true)
        .await
        .unwrap();
    assert_eq!(
        ids(&all, |p| p.user_id.clone()),
        [NEW_MEMBER, OLD_MEMBER, ALREADY_IN, LEFT_BEFORE]
            .iter()
            .map(|s| s.to_string())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>()
    );
    let fresh = store
        .channel_members_to_add(0, Some(OPEN_CHANNEL), false)
        .await
        .unwrap();
    // ALREADY_IN has a history row; LEFT_BEFORE has one too, with a leave time — both excluded.
    assert_eq!(
        ids(&fresh, |p| p.user_id.clone()),
        [NEW_MEMBER, OLD_MEMBER]
            .iter()
            .map(|s| s.to_string())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>()
    );
    // The DM channel is linked too, and nothing about `*ToAdd` cares about channel type.
    let dm = store
        .channel_members_to_add(0, Some(DM_CHANNEL), true)
        .await
        .unwrap();
    assert_eq!(dm.len(), 4);
    // Unscoped: both channels' pairs.
    let unscoped = store.channel_members_to_add(0, None, true).await.unwrap();
    assert_eq!(unscoped.len(), 8);
    sweep(&pool).await;
}

/// `TeamMembersToRemove` and `ChannelMembersToRemove`: members of a constrained syncable in
/// none of its live groups, bots exempt, and — for channels — only open and private ones.
#[tokio::test]
async fn members_to_remove_exempt_bots_and_non_syncable_channels() {
    if !db_enabled() {
        return;
    }
    let _guard = FIXTURES.lock().await;
    let pool = pool().await;
    plant(&pool).await;
    let store = SqlGroupStore::new(pool.clone());

    // STRANGER is in a deleted group only; GUEST's membership is deleted; BOT is in no group
    // at all and would be removed but for being a bot.
    let team = store.team_members_to_remove(Some(TEAM)).await.unwrap();
    assert_eq!(
        ids(&team, |p| p.user_id.clone()),
        [GUEST, STRANGER]
            .iter()
            .map(|s| s.to_string())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>()
    );
    let other = store
        .team_members_to_remove(Some(OTHER_TEAM))
        .await
        .unwrap();
    assert!(other.is_empty(), "an unconstrained team sheds nobody");
    let unscoped = store.team_members_to_remove(None).await.unwrap();
    assert_eq!(
        unscoped.len(),
        team.len(),
        "only the constrained team contributes"
    );

    let open = store
        .channel_members_to_remove(Some(OPEN_CHANNEL))
        .await
        .unwrap();
    assert_eq!(
        ids(&open, |p| p.user_id.clone()),
        [GUEST, STRANGER]
            .iter()
            .map(|s| s.to_string())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>()
    );
    let dm = store
        .channel_members_to_remove(Some(DM_CHANNEL))
        .await
        .unwrap();
    assert!(
        dm.is_empty(),
        "a DM channel is never group-synced, constrained or not"
    );
    sweep(&pool).await;
}

/// `PermittedSyncableAdmins`: the members of the groups linked with `SchemeAdmin`, both the link
/// and the membership live.
#[tokio::test]
async fn permitted_syncable_admins_need_a_live_admin_link_and_a_live_membership() {
    if !db_enabled() {
        return;
    }
    let _guard = FIXTURES.lock().await;
    let pool = pool().await;
    plant(&pool).await;
    let store = SqlGroupStore::new(pool.clone());

    let mut admins = store
        .permitted_syncable_admins(TEAM, &team_type())
        .await
        .unwrap();
    admins.sort();
    // DELETED_GROUP's link is live and admin, so STRANGER counts here: the query never looks at
    // `UserGroups.DeleteAt` — and neither does Go's.
    let mut expected: Vec<String> = [NEW_MEMBER, OLD_MEMBER, ALREADY_IN, LEFT_BEFORE, STRANGER]
        .iter()
        .map(|s| s.to_string())
        .collect();
    expected.sort();
    assert_eq!(admins, expected);

    sqlx::query("UPDATE groupteams SET schemeadmin = false WHERE groupid = $1")
        .bind(GROUP)
        .execute(&pool)
        .await
        .unwrap();
    let admins = store
        .permitted_syncable_admins(TEAM, &team_type())
        .await
        .unwrap();
    assert_eq!(admins, [STRANGER]);
    let channel_admins = store
        .permitted_syncable_admins(OPEN_CHANNEL, &channel_type())
        .await
        .unwrap();
    assert_eq!(channel_admins.len(), 4);
    sweep(&pool).await;
}

/// `UpdateMembersRole` on both tables: only the rows whose flag changes come back, guests are
/// never touched, and an empty list demotes every admin.
#[tokio::test]
async fn update_members_role_touches_only_changed_non_guest_rows() {
    if !db_enabled() {
        return;
    }
    let _guard = FIXTURES.lock().await;
    let pool = pool().await;
    plant(&pool).await;
    let teams = SqlTeamStore::new(pool.clone());
    let channels = SqlChannelStore::new(pool.clone());

    // STRANGER is admin; make ALREADY_IN and GUEST admins: ALREADY_IN promoted, STRANGER demoted,
    // GUEST untouched (a guest), BOT untouched (not named and not admin).
    let updated = teams
        .update_members_role(TEAM, &[ALREADY_IN.to_owned(), GUEST.to_owned()])
        .await
        .unwrap();
    let mut changed: Vec<(String, bool)> = updated
        .iter()
        .map(|m| (m.user_id.clone(), m.scheme_admin))
        .collect();
    changed.sort();
    assert_eq!(
        changed,
        [(ALREADY_IN.to_owned(), true), (STRANGER.to_owned(), false)]
    );
    // The returned row is the raw one: `roles` is the stored column, `explicit_roles` empty.
    assert!(updated.iter().all(|m| m.explicit_roles.is_empty()));

    let none = teams.update_members_role(TEAM, &[]).await.unwrap();
    assert_eq!(ids(&none, |m| m.user_id.clone()), [ALREADY_IN]);

    let updated = channels
        .update_members_role(OPEN_CHANNEL, &[ALREADY_IN.to_owned(), GUEST.to_owned()])
        .await
        .unwrap();
    let mut changed: Vec<(String, bool)> = updated
        .iter()
        .map(|m| (m.user_id.clone(), m.scheme_admin))
        .collect();
    changed.sort();
    assert_eq!(
        changed,
        [(ALREADY_IN.to_owned(), true), (STRANGER.to_owned(), false)]
    );
    assert!(updated.iter().all(|m| m.notify_props.is_some()));
    sweep(&pool).await;
}

/// The row writes: create resets the clock and refuses a missing team; update refuses a
/// `DeleteAt` change to anything but zero and keeps `CreateAt`; delete twice is invalid input;
/// the list of live syncables carries the display fields and skips the deleted.
#[tokio::test]
async fn syncable_writes_follow_the_store_rules() {
    if !db_enabled() {
        return;
    }
    let _guard = FIXTURES.lock().await;
    let pool = pool().await;
    plant(&pool).await;
    let store = SqlGroupStore::new(pool.clone());

    let missing = store
        .create_group_syncable(GroupSyncable::new_group_team(
            OTHER_GROUP,
            "mmrsgsyncteam0000000000009",
            true,
        ))
        .await
        .unwrap_err();
    assert!(missing.is_not_found(), "{missing}");

    let mut fresh = GroupSyncable::new_group_team(OTHER_GROUP, TEAM, true);
    fresh.create_at = 1;
    fresh.update_at = 2;
    fresh.delete_at = 3;
    let created = store.create_group_syncable(fresh).await.unwrap();
    assert_eq!(created.delete_at, 0, "DeleteAt is reset");
    assert!(created.create_at > 3 && created.update_at == created.create_at);

    let mut channel_link = GroupSyncable::new_group_channel(OTHER_GROUP, OTHER_CHANNEL, false);
    channel_link.scheme_admin = true;
    let created_channel = store.create_group_syncable(channel_link).await.unwrap();
    assert_eq!(
        created_channel.team_id, TEAM,
        "a channel link carries its team"
    );

    let mut resurrect = store
        .get_group_syncable(OTHER_GROUP, OTHER_TEAM, &team_type())
        .await
        .unwrap();
    assert_eq!(resurrect.delete_at, 9, "a soft-deleted row is still found");
    resurrect.delete_at = 4;
    let refused = store
        .update_group_syncable(resurrect.clone())
        .await
        .unwrap_err();
    assert!(matches!(refused, StoreError::Argument { .. }), "{refused}");
    resurrect.delete_at = 0;
    resurrect.create_at = 77;
    let restored = store.update_group_syncable(resurrect).await.unwrap();
    assert_eq!(restored.delete_at, 0);
    assert_eq!(
        restored.create_at, 500,
        "CreateAt comes from the stored row"
    );

    let deleted = store
        .delete_group_syncable(OTHER_GROUP, OTHER_TEAM, &team_type())
        .await
        .unwrap();
    assert!(deleted.delete_at > 0 && deleted.update_at == deleted.delete_at);
    let again = store
        .delete_group_syncable(OTHER_GROUP, OTHER_TEAM, &team_type())
        .await
        .unwrap_err();
    assert!(again.is_invalid_input(), "{again}");
    let nowhere = store
        .delete_group_syncable(OTHER_GROUP, "mmrsgsyncteam0000000000009", &team_type())
        .await
        .unwrap_err();
    assert!(nowhere.is_not_found());

    let live_teams = store
        .get_all_group_syncables_by_group_id(OTHER_GROUP, &team_type())
        .await
        .unwrap();
    assert_eq!(ids(&live_teams, |s| s.syncable_id.clone()), [TEAM]);
    assert_eq!(live_teams[0].team_display_name, "mmrs gsync");
    assert_eq!(live_teams[0].team_type, "O");
    let live_channels = store
        .get_all_group_syncables_by_group_id(OTHER_GROUP, &channel_type())
        .await
        .unwrap();
    assert_eq!(live_channels.len(), 1);
    assert_eq!(live_channels[0].channel_type, "P");
    assert_eq!(live_channels[0].team_id, TEAM);
    assert!(live_channels[0].scheme_admin);

    let synced = store.group_ids_synced_to_team(TEAM).await.unwrap();
    assert!(synced.contains(&GROUP.to_owned()) && synced.contains(&OTHER_GROUP.to_owned()));
    assert!(
        !synced.contains(&DELETED_GROUP.to_owned()),
        "a deleted group is not among a team's groups"
    );
    sweep(&pool).await;
}

/// `GetNewestJobByStatusAndType` orders by `CreateAt`, not `StartAt`.
#[tokio::test]
async fn the_newest_job_is_by_create_at() {
    if !db_enabled() {
        return;
    }
    let _guard = FIXTURES.lock().await;
    let pool = pool().await;
    plant(&pool).await;
    for (id, create_at, start_at) in [
        ("mmrsgsyncjob00000000000001", 100i64, 900i64),
        ("mmrsgsyncjob00000000000002", 200, 100),
    ] {
        sqlx::query(
            "INSERT INTO jobs (id, type, priority, createat, startat, lastactivityat, status,
                               progress, data)
             VALUES ($1, 'ldap_sync', 0, $2, $3, 0, 'success', 0, NULL)",
        )
        .bind(id)
        .bind(create_at)
        .bind(start_at)
        .execute(&pool)
        .await
        .unwrap();
    }
    let jobs = SqlJobStore::new(pool.clone());
    let newest = jobs
        .get_newest_job_by_status_and_type("success", "ldap_sync")
        .await
        .unwrap();
    assert_eq!(newest.id, "mmrsgsyncjob00000000000002");
    assert_eq!(newest.start_at, 100);
    let none = jobs
        .get_newest_job_by_status_and_type("pending", "ldap_sync")
        .await
        .unwrap_err();
    assert!(none.is_not_found());
    sweep(&pool).await;
}
