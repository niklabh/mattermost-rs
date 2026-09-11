//! The store-level oracle for `Channel.Save`, `SaveDirectChannel` and the per-team channel count.
//!
//! ```sh
//! docker compose up -d
//! export DATABASE_URL=postgres://mmuser:mmuser_password@localhost:5432/mattermost
//! MM_STORE_DB=1 cargo test -p mm-store --test db_channel_creates
//! ```
//!
//! # Three things here are invisible from the routes that cause them
//!
//! - **The `PublicChannels` row.** `POST /channels` answers the channel it wrote either way; the
//!   shadow table is what makes the new channel appear in "browse channels" and in search. A
//!   create that skipped the upsert passes every cross-server body comparison.
//! - **The self-DM's single membership.** `SaveDirectChannel` writes one `ChannelMembers` row
//!   when both participants are the same user and two otherwise. Writing two would violate the
//!   primary key and turn a legal self-DM into a 500 — but only for a user who has never opened
//!   one, which the parity suite's fixture user has.
//! - **The limit predicate.** `saveChannelT` counts *live* `O` and `P` channels while the app
//!   layer's own pre-check counts archived ones and `G` as well. Two different numbers against
//!   one setting, and only the store's half is reachable from here.
//!
//! Every id is `mmrs`-prefixed and every case cleans up after itself.

use mm_model::channel::{Channel, get_dm_name_from_ids};
use mm_model::channel_member::{ChannelMember, get_default_channel_notify_props};
use mm_store::channel_store::{ChannelSave, count_team_channels, save, save_direct_channel};
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

fn channel(team_id: &str, name: &str, channel_type: &str) -> Channel {
    Channel {
        team_id: team_id.to_owned(),
        name: name.to_owned(),
        display_name: "mmrs store create".to_owned(),
        channel_type: channel_type.to_owned(),
        ..Channel::default()
    }
}

async fn unplant_team(pool: &PgPool, team_id: &str) {
    let ids: Vec<String> = sqlx::query_scalar("SELECT id FROM channels WHERE teamid = $1")
        .bind(team_id)
        .fetch_all(pool)
        .await
        .unwrap_or_default();
    for channel_id in &ids {
        unplant_channel(pool, channel_id).await;
    }
}

async fn unplant_channel(pool: &PgPool, channel_id: &str) {
    for statement in [
        "DELETE FROM channelmemberhistory WHERE channelid = $1",
        "DELETE FROM channelmembers WHERE channelid = $1",
        "DELETE FROM sidebarchannels WHERE channelid = $1",
        "DELETE FROM publicchannels WHERE id = $1",
        "DELETE FROM channels WHERE id = $1",
    ] {
        let _ = sqlx::query(statement).bind(channel_id).execute(pool).await;
    }
}

async fn public_channel_row(pool: &PgPool, channel_id: &str) -> Option<(String, String, String)> {
    sqlx::query_as("SELECT teamid, name, displayname FROM publicchannels WHERE id = $1")
        .bind(channel_id)
        .fetch_optional(pool)
        .await
        .expect("the shadow table reads")
}

async fn member_ids(pool: &PgPool, channel_id: &str) -> Vec<String> {
    let mut ids: Vec<String> =
        sqlx::query_scalar("SELECT userid FROM channelmembers WHERE channelid = $1")
            .bind(channel_id)
            .fetch_all(pool)
            .await
            .expect("the memberships read");
    ids.sort();
    ids
}

fn direct_member(user_id: &str) -> ChannelMember {
    ChannelMember {
        user_id: user_id.to_owned(),
        notify_props: Some(get_default_channel_notify_props()),
        scheme_user: true,
        ..ChannelMember::default()
    }
}

/// A public channel reaches `PublicChannels`; a private one **deletes itself out of it**.
#[tokio::test]
async fn save_propagates_a_public_channel_to_the_shadow_table_and_a_private_one_out_of_it() {
    if !db_enabled() {
        return;
    }
    let pool = pool().await;
    let team = id("mmrsteam", "shad");
    unplant_team(&pool, &team).await;

    let mut open = channel(&team, "mmrs-store-open", "O");
    let outcome = save(&pool, &mut open, 2000)
        .await
        .expect("the save succeeds");
    assert!(
        matches!(outcome, ChannelSave::Saved),
        "a fresh name inserts"
    );
    // `PreSave` filled these in, in place — the caller's struct is the row that was written.
    assert_eq!(open.id.len(), 26, "PreSave mints an id: {}", open.id);
    assert!(open.create_at > 0 && open.update_at == open.create_at);

    let shadow = public_channel_row(&pool, &open.id).await;
    assert_eq!(
        shadow,
        Some((
            team.clone(),
            "mmrs-store-open".to_owned(),
            "mmrs store create".to_owned()
        )),
        "an open channel must appear in PublicChannels"
    );

    let mut private = channel(&team, "mmrs-store-private", "P");
    save(&pool, &mut private, 2000)
        .await
        .expect("the private save succeeds");
    assert_eq!(
        public_channel_row(&pool, &private.id).await,
        None,
        "a private channel must NOT appear in PublicChannels"
    );

    unplant_team(&pool, &team).await;
}

/// A taken `(name, teamid)` returns the row that holds it and writes nothing — **including when
/// that row is archived**, which is the case `POST /channels` turns into a 400.
#[tokio::test]
async fn save_returns_the_existing_channel_on_conflict_archived_or_not() {
    if !db_enabled() {
        return;
    }
    let pool = pool().await;
    let team = id("mmrsteam", "conf");
    unplant_team(&pool, &team).await;

    let mut first = channel(&team, "mmrs-store-taken", "O");
    save(&pool, &mut first, 2000).await.expect("the first save");

    let mut second = channel(&team, "mmrs-store-taken", "O");
    match save(&pool, &mut second, 2000).await.expect("no error") {
        ChannelSave::Existing(existing) => {
            assert_eq!(existing.id, first.id, "the holder of the name comes back");
            assert_eq!(existing.channel_type, "O");
        }
        ChannelSave::Saved => panic!("a duplicate name must not insert"),
    }
    // Nothing was written: the loser's minted id must not be in the table.
    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM channels WHERE id = $1")
        .bind(&second.id)
        .fetch_one(&pool)
        .await
        .expect("the count reads");
    assert_eq!(rows, 0, "the rolled-back insert left a row behind");

    // Archive the holder and try again. `tableSelectQuery` has no `DeleteAt` filter, so the name
    // is still taken — a port that filtered archived rows out here would insert a second channel
    // and violate the unique constraint instead.
    sqlx::query("UPDATE channels SET deleteat = 99 WHERE id = $1")
        .bind(&first.id)
        .execute(&pool)
        .await
        .expect("the archive writes");
    let mut third = channel(&team, "mmrs-store-taken", "O");
    match save(&pool, &mut third, 2000).await.expect("no error") {
        ChannelSave::Existing(existing) => {
            assert_eq!(existing.id, first.id);
            assert_eq!(existing.delete_at, 99, "and it comes back archived");
        }
        ChannelSave::Saved => panic!("an archived channel still holds its name"),
    }

    unplant_team(&pool, &team).await;
}

/// The three refusals that happen before the transaction opens, each with its own `field`.
#[tokio::test]
async fn save_refuses_an_archived_a_direct_and_a_pre_assigned_channel() {
    if !db_enabled() {
        return;
    }
    let pool = pool().await;
    let team = id("mmrsteam", "refu");
    unplant_team(&pool, &team).await;

    let mut archived = channel(&team, "mmrs-store-arch", "O");
    archived.delete_at = 1;
    match save(&pool, &mut archived, 2000).await {
        Err(StoreError::InvalidInput { entity, field, .. }) => {
            assert_eq!((entity, field), ("Channel", "DeleteAt"));
        }
        other => panic!("expected an InvalidInput on DeleteAt, got {other:?}"),
    }

    // Type `D` is refused here and type `G` is **not** — `createGroupChannel` reaches this same
    // function. Both halves asserted, because a guard widened to "is group or direct" would pass
    // every other test in this file.
    let mut direct = channel("", "mmrs-store-direct-guard", "D");
    match save(&pool, &mut direct, 2000).await {
        Err(StoreError::InvalidInput { entity, field, .. }) => {
            assert_eq!((entity, field), ("Channel", "Type"));
        }
        other => panic!("expected an InvalidInput on Type, got {other:?}"),
    }
    let mut group = channel("", &"a".repeat(40), "G");
    let outcome = save(&pool, &mut group, 2000)
        .await
        .expect("a group channel goes through Save");
    assert!(matches!(outcome, ChannelSave::Saved));
    unplant_channel(&pool, &group.id).await;

    let mut preset = channel(&team, "mmrs-store-preset", "O");
    preset.id = id("mmrschan", "pres");
    match save(&pool, &mut preset, 2000).await {
        Err(StoreError::InvalidInput {
            entity,
            field,
            value,
        }) => {
            assert_eq!((entity, field), ("Channel", "Id"));
            assert_eq!(value, preset.id, "the rejected id reaches detailed_error");
        }
        other => panic!("expected an InvalidInput on Id, got {other:?}"),
    }

    unplant_team(&pool, &team).await;
}

/// The per-team limit: `>=`, not `>`; live `O` and `P` only; and a negative setting disables it.
#[tokio::test]
async fn the_store_limit_counts_live_public_and_private_channels_only() {
    if !db_enabled() {
        return;
    }
    let pool = pool().await;
    let team = id("mmrsteam", "limi");
    unplant_team(&pool, &team).await;

    let mut first = channel(&team, "mmrs-store-limit-a", "O");
    save(&pool, &mut first, 2).await.expect("one of two");
    let mut second = channel(&team, "mmrs-store-limit-b", "P");
    save(&pool, &mut second, 2).await.expect("two of two");

    // The count is now 2 and the limit is 2: `count >= max` refuses. An off-by-one here (`>`)
    // would let a third channel in.
    let mut third = channel(&team, "mmrs-store-limit-c", "O");
    match save(&pool, &mut third, 2).await {
        Err(StoreError::LimitExceeded { what, count, .. }) => {
            assert_eq!(what, "channels_per_team");
            assert_eq!(count, 2, "the measured count, not the limit");
        }
        other => panic!("expected a LimitExceeded, got {other:?}"),
    }

    // Archiving one takes it out of the store's count — but not out of `count_team_channels`.
    sqlx::query("UPDATE channels SET deleteat = 42 WHERE id = $1")
        .bind(&first.id)
        .execute(&pool)
        .await
        .expect("the archive writes");
    let mut fourth = channel(&team, "mmrs-store-limit-d", "O");
    save(&pool, &mut fourth, 2)
        .await
        .expect("an archived channel does not count against the store's limit");
    assert_eq!(
        count_team_channels(&pool, &team)
            .await
            .expect("the count reads"),
        3,
        "the app layer's count *does* include the archived one"
    );

    // A negative limit switches the check off entirely.
    let mut fifth = channel(&team, "mmrs-store-limit-e", "O");
    save(&pool, &mut fifth, -1)
        .await
        .expect("a negative limit disables the check");

    // And zero refuses everything, because `0 >= 0`.
    let mut sixth = channel(&team, "mmrs-store-limit-f", "O");
    assert!(
        matches!(
            save(&pool, &mut sixth, 0).await,
            Err(StoreError::LimitExceeded { .. })
        ),
        "a limit of zero refuses every channel"
    );

    unplant_team(&pool, &team).await;
    assert_eq!(
        count_team_channels(&pool, &team)
            .await
            .expect("the count reads"),
        0,
        "an empty team counts zero — the app layer turns that into its 404"
    );
}

/// Two participants are two membership rows in one transaction; **one participant is one row**.
#[tokio::test]
async fn save_direct_channel_writes_both_memberships_and_one_for_a_self_dm() {
    if !db_enabled() {
        return;
    }
    let pool = pool().await;
    let alice = id("mmrsuser", "dma");
    let bob = id("mmrsuser", "dmb");

    let mut dm = Channel {
        // A team id the caller set is **discarded**: `SaveDirectChannel` blanks it before the
        // insert, which is what makes a DM's uniqueness `(name, '')` installation-wide.
        team_id: id("mmrsteam", "dmt"),
        name: get_dm_name_from_ids(&alice, &bob),
        channel_type: "D".to_owned(),
        shared: Some(false),
        creator_id: alice.clone(),
        ..Channel::default()
    };
    let outcome = save_direct_channel(&pool, &mut dm, direct_member(&alice), direct_member(&bob))
        .await
        .expect("the direct save succeeds");
    assert!(matches!(outcome, ChannelSave::Saved));
    assert_eq!(dm.team_id, "", "the team id is forced empty");
    let mut expected = vec![alice.clone(), bob.clone()];
    expected.sort();
    assert_eq!(member_ids(&pool, &dm.id).await, expected, "both members");
    assert_eq!(
        public_channel_row(&pool, &dm.id).await,
        None,
        "a DM never reaches PublicChannels"
    );

    // A second open of the same pair conflicts and writes nothing new.
    let mut again = Channel {
        name: get_dm_name_from_ids(&alice, &bob),
        channel_type: "D".to_owned(),
        shared: Some(false),
        ..Channel::default()
    };
    match save_direct_channel(
        &pool,
        &mut again,
        direct_member(&alice),
        direct_member(&bob),
    )
    .await
    .expect("no error")
    {
        ChannelSave::Existing(existing) => assert_eq!(existing.id, dm.id),
        ChannelSave::Saved => panic!("the pair already has a DM"),
    }

    // The self-DM: `GetMany` de-duplicates upstream, so both members carry the same id and only
    // `member2` is written. Two inserts would hit `channelmembers`' primary key.
    let mut selfdm = Channel {
        name: get_dm_name_from_ids(&alice, &alice),
        channel_type: "D".to_owned(),
        shared: Some(false),
        creator_id: alice.clone(),
        ..Channel::default()
    };
    save_direct_channel(
        &pool,
        &mut selfdm,
        direct_member(&alice),
        direct_member(&alice),
    )
    .await
    .expect("a self-DM is legal");
    assert_eq!(
        member_ids(&pool, &selfdm.id).await,
        vec![alice.clone()],
        "a self-DM has exactly one membership row"
    );

    // A non-`D` type is refused before anything is written.
    let mut wrong = Channel {
        name: "mmrs-store-not-direct".to_owned(),
        channel_type: "O".to_owned(),
        ..Channel::default()
    };
    match save_direct_channel(
        &pool,
        &mut wrong,
        direct_member(&alice),
        direct_member(&bob),
    )
    .await
    {
        Err(StoreError::InvalidInput { entity, field, .. }) => {
            assert_eq!((entity, field), ("Channel", "Type"));
        }
        other => panic!("expected an InvalidInput on Type, got {other:?}"),
    }

    unplant_channel(&pool, &dm.id).await;
    unplant_channel(&pool, &selfdm.id).await;
}

/// `count_team_channels` counts `O`, `P` and `G` — and `G` never has a team, so in practice it
/// counts the team's public and private channels, archived ones included.
#[tokio::test]
async fn count_team_channels_includes_archived_channels() {
    if !db_enabled() {
        return;
    }
    let pool = pool().await;
    let team = id("mmrsteam", "cnt");
    unplant_team(&pool, &team).await;

    assert_eq!(
        count_team_channels(&pool, &team).await.expect("reads"),
        0,
        "a team with no channels counts zero"
    );

    let mut open = channel(&team, "mmrs-store-count-a", "O");
    save(&pool, &mut open, 2000).await.expect("saved");
    let mut private = channel(&team, "mmrs-store-count-b", "P");
    save(&pool, &mut private, 2000).await.expect("saved");
    assert_eq!(count_team_channels(&pool, &team).await.expect("reads"), 2);

    sqlx::query("UPDATE channels SET deleteat = 7 WHERE id = $1")
        .bind(&open.id)
        .execute(&pool)
        .await
        .expect("the archive writes");
    assert_eq!(
        count_team_channels(&pool, &team).await.expect("reads"),
        2,
        "archiving does not change this count — Go's GetTeamChannels has no DeleteAt filter"
    );

    unplant_team(&pool, &team).await;
}
