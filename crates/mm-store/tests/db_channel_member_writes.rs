//! The store-level oracle for the six channel-member writes, and in particular for the two writes
//! that **no response body can show**: `ChannelMemberHistory`'s join and leave rows.
//!
//! ```sh
//! docker compose up -d
//! export DATABASE_URL=postgres://mmuser:mmuser_password@localhost:5432/mattermost
//! MM_STORE_DB=1 cargo test -p mm-store --test db_channel_member_writes
//! ```
//!
//! # Why the history rows need their own test
//!
//! `addUserToChannel` writes a `ChannelMemberHistory` row and `removeUserFromChannel` closes it.
//! Neither appears in `POST`/`DELETE …/members`'s answer, so the cross-server parity suite is
//! blind to both: it would pass with the two writes deleted. This file asserts the rows directly.
//!
//! # It writes rows the Go server owns, and cleans them up
//!
//! Every id here is `mmrs`-prefixed and every case deletes what it made, including on the failure
//! path — assertions are collected rather than panicked on where a leak would otherwise strand a
//! row the api parity suite's `purge_api_fixtures` does not know about.

use mm_model::channel_member::{ChannelMember, get_default_channel_notify_props};
use mm_store::channel_member_history_store::{
    ChannelMemberHistoryStore, SqlChannelMemberHistoryStore,
};
use mm_store::channel_store::{
    ChannelStore, SqlChannelStore, get_all_channel_member_ids_by_channel_id, remove_member,
    save_member, update_member, update_member_notify_props,
};
use mm_store::error::StoreError;
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

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

fn id(prefix: &str, tag: &str) -> String {
    let suffix = format!("{tag}{:0>26}", std::process::id());
    format!("{prefix}{}", &suffix[..26 - prefix.len()])
}

/// A channel row with no team and no scheme, which is the Team Edition shape: every scheme default
/// is NULL, so `get_channel_roles` falls through to the three constants.
async fn plant_channel(pool: &PgPool, channel_id: &str) {
    sqlx::query(
        "INSERT INTO channels (id, createat, updateat, deleteat, teamid, type, displayname, name, \
         header, purpose, lastpostat, totalmsgcount, extraupdateat, creatorid, schemeid, \
         groupconstrained, shared, totalmsgcountroot, lastrootpostat, defaultcategoryname, \
         discoverable, autotranslation) \
         VALUES ($1, 1, 1, 0, '', 'O', 'mmrs member writes', $2, '', '', 0, 0, 0, '', NULL, NULL, \
         NULL, 0, 0, '', false, false)",
    )
    .bind(channel_id)
    .bind(format!("mmrs-store-{channel_id}"))
    .execute(pool)
    .await
    .expect("the channel row is written");
}

async fn unplant(pool: &PgPool, channel_id: &str) {
    for statement in [
        "DELETE FROM channelmemberhistory WHERE channelid = $1",
        "DELETE FROM channelmembers WHERE channelid = $1",
        "DELETE FROM sidebarchannels WHERE channelid = $1",
        "DELETE FROM channels WHERE id = $1",
    ] {
        let _ = sqlx::query(statement).bind(channel_id).execute(pool).await;
    }
}

fn member(channel_id: &str, user_id: &str) -> ChannelMember {
    ChannelMember {
        channel_id: channel_id.to_owned(),
        user_id: user_id.to_owned(),
        notify_props: Some(get_default_channel_notify_props()),
        scheme_user: true,
        ..ChannelMember::default()
    }
}

#[tokio::test]
async fn a_join_writes_one_open_history_row_and_a_leave_closes_exactly_that_one() {
    if !db_enabled() {
        return;
    }
    let pool = pool().await;
    let channel = id("mmrschan", "hist");
    let user = id("mmrsuser", "hist");
    unplant(&pool, &channel).await;
    plant_channel(&pool, &channel).await;

    let history = SqlChannelMemberHistoryStore::new(pool.clone());

    history
        .log_join_event(&user, &channel, 1_000)
        .await
        .expect("the join is logged");
    let rows = history
        .get_history_for_member(&user, &channel)
        .await
        .expect("the history reads");
    assert_eq!(rows.len(), 1, "one row per join: {rows:?}");
    assert_eq!(rows[0].join_time, 1_000);
    // **`LeaveTime` is NULL while the member is in the channel**, not 0 — the column is nullable
    // and the leave query's `LeaveTime IS NULL` predicate is what finds the open stay.
    assert_eq!(
        rows[0].leave_time, None,
        "an open stay has a NULL leave time"
    );

    history
        .log_leave_event(&user, &channel, 2_000)
        .await
        .expect("the leave is logged");
    let rows = history
        .get_history_for_member(&user, &channel)
        .await
        .expect("the history reads");
    assert_eq!(rows[0].leave_time, Some(2_000), "the stay is closed");

    // A second join is a **second row**, and closing it must leave the first alone. This is the
    // assertion that catches an `UPDATE` without the `LeaveTime IS NULL` predicate: without it,
    // both rows would be rewritten and the audit trail would lose the first stay's end time.
    history
        .log_join_event(&user, &channel, 3_000)
        .await
        .expect("the rejoin is logged");
    history
        .log_leave_event(&user, &channel, 4_000)
        .await
        .expect("the second leave is logged");
    let rows = history
        .get_history_for_member(&user, &channel)
        .await
        .expect("the history reads");
    assert_eq!(rows.len(), 2, "two stays: {rows:?}");
    assert_eq!(
        (rows[0].join_time, rows[0].leave_time),
        (1_000, Some(2_000)),
        "the first stay must keep its own leave time"
    );
    assert_eq!(
        (rows[1].join_time, rows[1].leave_time),
        (3_000, Some(4_000))
    );

    // A leave with nothing open is **best effort**: Go logs a warning and returns nil, because a
    // member whose join was never recorded still has to be removable.
    history
        .log_leave_event(&user, &channel, 5_000)
        .await
        .expect("a leave with no open stay is not an error");

    unplant(&pool, &channel).await;
}

#[tokio::test]
async fn save_member_writes_explicit_roles_and_returns_the_effective_ones() {
    if !db_enabled() {
        return;
    }
    let pool = pool().await;
    let channel = id("mmrschan", "save");
    let user = id("mmrsuser", "save");
    unplant(&pool, &channel).await;
    plant_channel(&pool, &channel).await;

    let mut incoming = member(&channel, &user);
    incoming.explicit_roles = "mmrs_custom_role".to_owned();
    incoming.scheme_admin = true;
    let saved = save_member(&pool, incoming)
        .await
        .expect("the member saves");

    // The **return** carries the effective roles, resolved from the flags and the (absent) schemes:
    // the custom role first, in the order it appeared, then guest/user/admin from the constants.
    assert_eq!(saved.roles, "mmrs_custom_role channel_user channel_admin");
    assert_eq!(saved.explicit_roles, "mmrs_custom_role");
    assert!(saved.scheme_user && saved.scheme_admin && !saved.scheme_guest);
    // `PreSave` stamped it; nothing else could have.
    assert!(saved.last_update_at > 0, "PreSave sets LastUpdateAt");

    // The **column** holds only the explicit role. Writing the effective roles there would persist
    // `channel_user`/`channel_admin` as explicit grants that outlive a scheme change.
    let stored: Option<String> =
        sqlx::query_scalar("SELECT roles FROM channelmembers WHERE channelid = $1 AND userid = $2")
            .bind(&channel)
            .bind(&user)
            .fetch_one(&pool)
            .await
            .expect("the row is there");
    assert_eq!(
        stored.as_deref(),
        Some("mmrs_custom_role"),
        "the Roles column takes ExplicitRoles, not the effective roles"
    );

    // Reading it back through `get_member` must agree with what `save_member` answered — Go never
    // re-selects here, so a divergence between the two is invisible until something else reads the
    // row.
    let store = SqlChannelStore::new(pool.clone());
    let read = store
        .get_member(&channel, &user)
        .await
        .expect("the member reads back");
    assert_eq!(read.roles, saved.roles, "the answer and the row disagree");
    assert_eq!(read.explicit_roles, saved.explicit_roles);
    assert_eq!(read.scheme_admin, saved.scheme_admin);

    // A second save of the same pair is a **conflict**, typed rather than folded into `Db` — the
    // primary key is `(ChannelId, UserId)`.
    let again = save_member(&pool, member(&channel, &user)).await;
    assert!(
        matches!(
            again,
            Err(StoreError::Conflict {
                resource: "ChannelMembers",
                ..
            })
        ),
        "a duplicate member must be a typed conflict: {again:?}"
    );

    // A member with **no** notify props fails `IsValid` **inside the store**, before any SQL runs,
    // and the `AppError` is carried through rather than stringified.
    let mut invalid = member(&channel, &id("mmrsuser", "sav2"));
    invalid.notify_props = None;
    let refused = save_member(&pool, invalid).await;
    match refused {
        Err(StoreError::Invalid { app_error, .. }) => {
            assert_eq!(
                app_error.id, "model.channel_member.is_valid.notify_level.app_error",
                "a member with no `desktop` key fails on the notify level"
            );
            assert_eq!(app_error.status_code, 400);
        }
        other => panic!("expected a validation failure, got {other:?}"),
    }

    unplant(&pool, &channel).await;
}

#[tokio::test]
async fn update_member_rewrites_every_column_and_404s_a_member_that_is_not_there() {
    if !db_enabled() {
        return;
    }
    let pool = pool().await;
    let channel = id("mmrschan", "upd");
    let user = id("mmrsuser", "upd");
    unplant(&pool, &channel).await;
    plant_channel(&pool, &channel).await;

    let saved = save_member(&pool, member(&channel, &user))
        .await
        .expect("the member saves");
    let first_update_at = saved.last_update_at;

    // **A bystander in the same channel.** Without one, dropping `AND userid = $2` from the UPDATE
    // is invisible: the statement would rewrite every member of the channel and the only member is
    // the one being tested. Measured — that mutation survived until this row existed.
    let bystander = id("mmrsuser", "upb");
    let mut bystander_member = member(&channel, &bystander);
    bystander_member.mention_count = 3;
    let bystander_saved = save_member(&pool, bystander_member)
        .await
        .expect("the bystander saves");

    // **Backdate the column before the update.** `PreUpdate` and `PreSave` both stamp
    // `model.GetMillis()`, and the two writes here land in the same millisecond — so
    // `last_update_at >= first_update_at` is true whether `pre_update` ran or not, and a mutation
    // deleting the call survived on exactly that. Planting `1` makes the movement visible.
    sqlx::query("UPDATE channelmembers SET lastupdateat = 1 WHERE channelid = $1 AND userid = $2")
        .bind(&channel)
        .bind(&user)
        .execute(&pool)
        .await
        .expect("the timestamp is backdated");

    let mut changed = saved;
    // **And carry the backdated value into the struct**, so the only thing that can raise the
    // column is `PreUpdate` itself. Leaving the loaded timestamp here made a mutation deleting
    // `pre_update()` survive twice: the UPDATE wrote a real timestamp either way.
    changed.last_update_at = 1;
    changed.scheme_admin = true;
    changed.mention_count = 7;
    changed.explicit_roles = "mmrs_custom_role".to_owned();
    let updated = update_member(&pool, changed)
        .await
        .expect("the member updates");

    // The return comes from a **fresh SELECT** through the scheme joins, so `roles` is the
    // database's view rather than the caller's.
    assert_eq!(updated.roles, "mmrs_custom_role channel_user channel_admin");
    assert_eq!(updated.mention_count, 7, "the counters are rewritten too");
    assert!(
        updated.last_update_at > 1,
        "PreUpdate must stamp a fresh LastUpdateAt, not leave the backdated one: {}",
        updated.last_update_at
    );
    assert!(
        updated.last_update_at >= first_update_at,
        "and it moves forward, never back"
    );

    // The bystander is untouched: the UPDATE is scoped to one `(channelid, userid)` pair.
    let store = SqlChannelStore::new(pool.clone());
    let bystander_now = store
        .get_member(&channel, &bystander)
        .await
        .expect("the bystander is still there");
    assert_eq!(
        bystander_now.mention_count, 3,
        "the update reached another member's row"
    );
    assert_eq!(
        bystander_now.scheme_admin, bystander_saved.scheme_admin,
        "the update changed another member's roles"
    );
    assert_eq!(
        bystander_now.last_update_at, bystander_saved.last_update_at,
        "the update moved another member's LastUpdateAt"
    );

    // A member that is not there is **not-found**, and it comes from the re-select rather than
    // from `rows_affected` — a no-op update of a member that *does* exist must not 404.
    let missing = update_member(&pool, member(&channel, &id("mmrsuser", "up2"))).await;
    assert!(
        matches!(
            missing,
            Err(StoreError::NotFound {
                entity: "ChannelMember",
                ..
            })
        ),
        "expected not-found, got {missing:?}"
    );

    unplant(&pool, &channel).await;
}

#[tokio::test]
async fn notify_props_are_merged_and_the_rune_cap_is_checked_before_the_query() {
    if !db_enabled() {
        return;
    }
    let pool = pool().await;
    let channel = id("mmrschan", "prop");
    let user = id("mmrsuser", "prop");
    unplant(&pool, &channel).await;
    plant_channel(&pool, &channel).await;

    save_member(&pool, member(&channel, &user))
        .await
        .expect("the member saves");

    // Backdated for the same reason as the update test above: the `SET lastupdateat = $2` is
    // invisible against a value `PreSave` wrote in the same millisecond.
    sqlx::query("UPDATE channelmembers SET lastupdateat = 1 WHERE channelid = $1 AND userid = $2")
        .bind(&channel)
        .bind(&user)
        .execute(&pool)
        .await
        .expect("the timestamp is backdated");

    let mut patch = mm_model::utils::StringMap::new();
    patch.insert("desktop".to_owned(), "mention".to_owned());
    let updated = update_member_notify_props(&pool, &channel, &user, &patch)
        .await
        .expect("the props update");
    let props = updated.notify_props.expect("props");
    assert_eq!(props.get("desktop").map(String::as_str), Some("mention"));
    // The **merge**: the five keys the patch did not name survive. A replacing `SET notifyprops =`
    // would silently clear a member's mute.
    assert_eq!(props.get("mark_unread").map(String::as_str), Some("all"));
    assert_eq!(props.get("push").map(String::as_str), Some("default"));
    assert_eq!(props.len(), 6, "no key was dropped or added: {props:?}");

    // `LastUpdateAt` is set from `model.GetMillis()` in the same statement, so a client polling the
    // member sees the change. A mutation that drops that `SET` leaves the timestamp behind and no
    // response body notices.
    let stored_update_at: Option<i64> = sqlx::query_scalar(
        "SELECT lastupdateat FROM channelmembers WHERE channelid = $1 AND userid = $2",
    )
    .bind(&channel)
    .bind(&user)
    .fetch_one(&pool)
    .await
    .expect("the row is there");
    assert!(
        stored_update_at.is_some_and(|at| at > 1),
        "the notify-props write must move LastUpdateAt past the backdated 1: {stored_update_at:?}"
    );
    assert_eq!(
        updated.last_update_at,
        stored_update_at.unwrap_or_default(),
        "the answer and the row disagree about LastUpdateAt"
    );

    // **No validation on this path**: an invalid value is stored, and it is the app layer's filter
    // that decides which *keys* get through, not which values.
    let mut nonsense = mm_model::utils::StringMap::new();
    nonsense.insert("desktop".to_owned(), "banana".to_owned());
    let updated = update_member_notify_props(&pool, &channel, &user, &nonsense)
        .await
        .expect("an invalid value is accepted here");
    assert_eq!(
        updated
            .notify_props
            .as_ref()
            .and_then(|p| p.get("desktop"))
            .map(String::as_str),
        Some("banana")
    );

    // The rune cap is measured on Go's own encoding of the **submitted** map, before the UPDATE.
    let mut huge = mm_model::utils::StringMap::new();
    huge.insert("desktop".to_owned(), "a".repeat(800_001));
    let refused = update_member_notify_props(&pool, &channel, &user, &huge).await;
    assert!(
        matches!(
            refused,
            Err(StoreError::InvalidInput {
                entity: "ChannelMember",
                field: "NotifyProps",
                ..
            })
        ),
        "expected the notify-props cap, got {refused:?}"
    );

    // A member that is not there 404s from the re-select, exactly as `update_member` does.
    let missing =
        update_member_notify_props(&pool, &channel, &id("mmrsuser", "pro2"), &patch).await;
    assert!(
        matches!(missing, Err(StoreError::NotFound { .. })),
        "expected not-found, got {missing:?}"
    );

    unplant(&pool, &channel).await;
}

#[tokio::test]
async fn remove_member_also_clears_the_sidebar_rows() {
    if !db_enabled() {
        return;
    }
    let pool = pool().await;
    let channel = id("mmrschan", "rm");
    let user = id("mmrsuser", "rm");
    unplant(&pool, &channel).await;
    plant_channel(&pool, &channel).await;

    save_member(&pool, member(&channel, &user))
        .await
        .expect("the member saves");
    // A sidebar row for the same pair, which the removal has to take with it. The category id need
    // not exist: the DELETE joins nothing.
    sqlx::query(
        "INSERT INTO sidebarchannels (channelid, userid, categoryid, sortorder) \
         VALUES ($1, $2, 'mmrscategory', 0)",
    )
    .bind(&channel)
    .bind(&user)
    .execute(&pool)
    .await
    .expect("the sidebar row is written");

    // Another user's sidebar row in the same channel, which it must **not**.
    let bystander = id("mmrsuser", "rm2");
    sqlx::query(
        "INSERT INTO sidebarchannels (channelid, userid, categoryid, sortorder) \
         VALUES ($1, $2, 'mmrscategory', 0)",
    )
    .bind(&channel)
    .bind(&bystander)
    .execute(&pool)
    .await
    .expect("the bystander's sidebar row is written");

    remove_member(&pool, &channel, &user)
        .await
        .expect("the member is removed");

    let members = get_all_channel_member_ids_by_channel_id(&pool, &channel)
        .await
        .expect("the ids read");
    assert!(members.is_empty(), "the membership survived: {members:?}");

    let remaining: Vec<String> =
        sqlx::query_scalar("SELECT userid FROM sidebarchannels WHERE channelid = $1")
            .bind(&channel)
            .fetch_all(&pool)
            .await
            .expect("the sidebar rows read");
    assert_eq!(
        remaining,
        vec![bystander],
        "the removal must clear only the leaving user's sidebar rows"
    );

    // Removing again is **not** an error at the store level — the 404 a client sees comes from the
    // app layer's `GetChannelMember`, not from here.
    remove_member(&pool, &channel, &user)
        .await
        .expect("a second removal is a no-op");

    unplant(&pool, &channel).await;
}

#[tokio::test]
async fn get_all_channel_member_ids_is_every_member_and_an_empty_channel_is_not_a_miss() {
    if !db_enabled() {
        return;
    }
    let pool = pool().await;
    let channel = id("mmrschan", "ids");
    unplant(&pool, &channel).await;
    plant_channel(&pool, &channel).await;

    let empty = get_all_channel_member_ids_by_channel_id(&pool, &channel)
        .await
        .expect("an empty channel reads");
    assert!(
        empty.is_empty(),
        "an empty channel is an empty list, not a miss"
    );

    let a = id("mmrsuser", "idsa");
    let b = id("mmrsuser", "idsb");
    for user in [&a, &b] {
        save_member(&pool, member(&channel, user))
            .await
            .expect("the member saves");
    }
    let mut ids = get_all_channel_member_ids_by_channel_id(&pool, &channel)
        .await
        .expect("the ids read");
    // Go's query has **no `ORDER BY`** and its caller turns the result into a set, so nothing may
    // depend on the order — sorted here for the comparison, not because the store promises it.
    ids.sort();
    let mut expected = vec![a, b];
    expected.sort();
    assert_eq!(ids, expected);

    unplant(&pool, &channel).await;
}

#[tokio::test]
async fn get_channel_of_type_reaches_a_type_get_hides_and_misses_the_wrong_one() {
    if !db_enabled() {
        return;
    }
    let pool = pool().await;
    let channel = id("mmrschan", "type");
    unplant(&pool, &channel).await;
    // A **space** channel: `ChannelStore::get`'s `Type IN ('O','P','D','G')` filter hides it, which
    // is the whole reason `GetChannelOfType` exists.
    sqlx::query(
        "INSERT INTO channels (id, createat, updateat, deleteat, teamid, type, displayname, name, \
         header, purpose, lastpostat, totalmsgcount, extraupdateat, creatorid, schemeid, \
         groupconstrained, shared, totalmsgcountroot, lastrootpostat, defaultcategoryname, \
         discoverable, autotranslation) \
         VALUES ($1, 1, 1, 0, '', 'S', 'mmrs space', $2, '', '', 0, 0, 0, '', NULL, NULL, NULL, 0, \
         0, '', false, false)",
    )
    .bind(&channel)
    .bind(format!("mmrs-store-{channel}"))
    .execute(&pool)
    .await
    .expect("the space row is written");

    let store = SqlChannelStore::new(pool.clone());
    let found = store
        .get_channel_of_type(&channel, "S")
        .await
        .expect("a space channel is reachable by its exact type");
    assert_eq!(found.channel_type, "S");

    // Asking for the wrong type is a **miss**, not the row — which is what makes
    // `rejectSpaceChannelByID`'s 404 branch mean "not a space".
    let wrong = store.get_channel_of_type(&channel, "O").await;
    assert!(
        matches!(
            wrong,
            Err(StoreError::NotFound {
                entity: "Channel",
                ..
            })
        ),
        "expected not-found for the wrong type, got {wrong:?}"
    );

    // And `get` really does hide it, so the two are not interchangeable.
    let hidden = store.get(&channel).await;
    assert!(
        matches!(hidden, Err(StoreError::NotFound { .. })),
        "`get` must keep filtering the space type out, got {hidden:?}"
    );

    unplant(&pool, &channel).await;
}
