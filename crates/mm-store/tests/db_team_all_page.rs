//! `GetAllPage` and `AnalyticsTeamCount` — the two queries behind `GET /api/v4/teams` — against
//! a real Postgres.
//!
//! ```sh
//! docker compose up -d
//! export DATABASE_URL=postgres://mmuser:mmuser_password@localhost:5432/mattermost
//! MM_STORE_DB=1 cargo test -p mm-store --test db_team_all_page
//! ```
//!
//! # Why the counts are asserted as deltas
//!
//! Both queries span the **whole** `Teams` table, which three sibling worktrees are also writing
//! to. An absolute `total_count` would be a fixture-pollution flake wearing a green tick. Every
//! count here is therefore measured before seeding and again after, and only the difference is
//! asserted; every listing assertion is filtered to the ids this file created.
//!
//! # What this file reaches that the parity suite cannot
//!
//! Three of `GetAllPage`'s inputs are unreachable over REST on a Team Edition deployment, so they
//! are seeded straight into the tables here:
//!
//! - a **NULL** `allowopeninvite` — the REST create path always writes a boolean, and the whole
//!   point is that Go filters with `=`, so a NULL team is in neither single-permission listing;
//! - a `RetentionPoliciesTeams` row, which needs an enterprise data-retention policy to exist;
//! - an `AccessControlPolicies` row of type `team`, which needs Enterprise Advanced ABAC.

use mm_model::team_search::TeamSearch;
use mm_store::team_store::{analytics_team_count, get_all_page};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

/// Same file-local serialisation as every other DB suite: these tests seed shared tables.
static FIXTURES: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Display names are the sort key, so they are chosen to order deterministically **and** to sort
/// after anything else the database holds: `~` is the last printable ASCII byte, so no leftover
/// team from another suite can interleave with these and break a positional assertion.
const PREFIX: &str = "mmrsallt";
const TEAM_OPEN: &str = "mmrsallt000000000000000opn";
const TEAM_PRIVATE: &str = "mmrsallt000000000000000prv";
const TEAM_ARCHIVED: &str = "mmrsallt000000000000000arc";
const TEAM_NULL_INVITE: &str = "mmrsallt00000000000000null";
const TEAM_RETAINED: &str = "mmrsallt000000000000000ret";
const TEAM_GOVERNED: &str = "mmrsallt000000000000000gov";
const POLICY: &str = "mmrsallt000000000000000pol";

fn db_enabled() -> bool {
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

async fn purge(pool: &PgPool) {
    for statement in [
        "DELETE FROM retentionpoliciesteams WHERE teamid LIKE 'mmrsallt%'",
        "DELETE FROM retentionpolicies WHERE id LIKE 'mmrsallt%'",
        "DELETE FROM accesscontrolpolicies WHERE id LIKE 'mmrsallt%'",
        "DELETE FROM teammembers WHERE teamid LIKE 'mmrsallt%'",
        "DELETE FROM teams WHERE id LIKE 'mmrsallt%'",
    ] {
        sqlx::query(statement)
            .execute(pool)
            .await
            .expect("purges leftover test rows");
    }
}

/// `display_name` doubles as the sort key; `allow_open_invite` is `None` for the NULL column.
async fn insert_team(
    pool: &PgPool,
    id: &str,
    display_name: &str,
    allow_open_invite: Option<bool>,
    delete_at: i64,
) {
    sqlx::query(
        "INSERT INTO teams (id, createat, updateat, deleteat, displayname, name, description,
                            email, type, companyname, alloweddomains, inviteid, allowopeninvite,
                            lastteamiconupdate, schemeid, groupconstrained, cloudlimitsarchived)
         VALUES ($1, 1, 1, $4, $2, $1, '', $1 || '@mmrs.invalid', 'O', '', '', $1, $3,
                 0, NULL, NULL, false)",
    )
    .bind(id)
    .bind(display_name)
    .bind(allow_open_invite)
    .bind(delete_at)
    .execute(pool)
    .await
    .expect("inserts the team");
}

/// Every team this file needs, sorted by display name as `~mmrsallt <letter>`.
async fn seed(pool: &PgPool) {
    insert_team(pool, TEAM_ARCHIVED, "~mmrsallt a archived", Some(false), 77).await;
    insert_team(pool, TEAM_GOVERNED, "~mmrsallt b governed", Some(false), 0).await;
    insert_team(pool, TEAM_NULL_INVITE, "~mmrsallt c null", None, 0).await;
    insert_team(pool, TEAM_OPEN, "~mmrsallt d open", Some(true), 0).await;
    insert_team(pool, TEAM_PRIVATE, "~mmrsallt e private", Some(false), 0).await;
    insert_team(pool, TEAM_RETAINED, "~mmrsallt f retained", Some(false), 0).await;

    sqlx::query(
        "INSERT INTO retentionpolicies (id, displayname, postduration) VALUES ($1, $1, 30)",
    )
    .bind(POLICY)
    .execute(pool)
    .await
    .expect("inserts the retention policy");
    sqlx::query("INSERT INTO retentionpoliciesteams (policyid, teamid) VALUES ($1, $2)")
        .bind(POLICY)
        .bind(TEAM_RETAINED)
        .execute(pool)
        .await
        .expect("attaches the team to the policy");

    sqlx::query(
        "INSERT INTO accesscontrolpolicies (id, name, type, active, createat, revision, version)
         VALUES ($1, $1, 'team', true, 1, 1, 'v0.1')",
    )
    .bind(TEAM_GOVERNED)
    .execute(pool)
    .await
    .expect("inserts the access control policy");
}

/// Every seeded team, in display-name order — the full expected listing when nothing filters.
fn all_seeded() -> Vec<&'static str> {
    vec![
        TEAM_ARCHIVED,
        TEAM_GOVERNED,
        TEAM_NULL_INVITE,
        TEAM_OPEN,
        TEAM_PRIVATE,
        TEAM_RETAINED,
    ]
}

/// The listing restricted to this file's rows, so the other worktrees' teams cannot move it.
///
/// `limit` is deliberately large: paging is asserted separately, on a query that *is* positional.
async fn listed(pool: &PgPool, opts: &TeamSearch) -> Vec<String> {
    get_all_page(pool, 0, 1000, opts)
        .await
        .expect("the listing runs")
        .into_iter()
        .filter(|t| t.id.starts_with(PREFIX))
        .map(|t| t.id)
        .collect()
}

async fn setup() -> (tokio::sync::MutexGuard<'static, ()>, PgPool) {
    let guard = FIXTURES.lock().await;
    let pool = pool().await;
    purge(&pool).await;
    seed(&pool).await;
    (guard, pool)
}

/// No options at all is the both-permissions branch: every team, **archived included**, because
/// `GetAllPage` has no `DeleteAt` predicate. Dropping the archived team here is the single most
/// plausible "fix" a reader could make to this query, and it would shorten every System Console
/// team page.
#[tokio::test]
async fn the_unfiltered_listing_includes_archived_teams() {
    if !db_enabled() {
        return;
    }
    let (_guard, pool) = setup().await;

    assert_eq!(listed(&pool, &TeamSearch::default()).await, all_seeded());

    purge(&pool).await;
}

/// `AllowOpenInvite` is compared with `=` against a **nullable** column, so the NULL team is
/// absent from both single-value listings while both-permissions still sees it. Writing the
/// private filter as `IS NOT TRUE` — the natural-looking Rust — would put it in the private list
/// and diverge from Go.
#[tokio::test]
async fn the_open_invite_filter_excludes_the_null_column_from_both_sides() {
    if !db_enabled() {
        return;
    }
    let (_guard, pool) = setup().await;

    let public_only = TeamSearch {
        allow_open_invite: Some(true),
        ..Default::default()
    };
    assert_eq!(listed(&pool, &public_only).await, vec![TEAM_OPEN]);

    let private_only = TeamSearch {
        allow_open_invite: Some(false),
        ..Default::default()
    };
    assert_eq!(
        listed(&pool, &private_only).await,
        vec![TEAM_ARCHIVED, TEAM_GOVERNED, TEAM_PRIVATE, TEAM_RETAINED],
        "archived and retained are private teams too; the NULL one is in neither list"
    );

    purge(&pool).await;
}

/// The ABAC widening: a governed team enters the public-only listing even though its
/// `allowopeninvite` is false, so the directory filter can evaluate it. Unreachable over REST
/// here — `AccessControlPolicies` is an Enterprise Advanced table and holds no rows — which is
/// exactly why the row is seeded.
#[tokio::test]
async fn include_policy_enforced_widens_the_public_listing_to_governed_teams() {
    if !db_enabled() {
        return;
    }
    let (_guard, pool) = setup().await;

    let widened = TeamSearch {
        allow_open_invite: Some(true),
        include_policy_enforced: Some(true),
        ..Default::default()
    };
    assert_eq!(
        listed(&pool, &widened).await,
        vec![TEAM_GOVERNED, TEAM_OPEN],
        "the governed private team joins the open one"
    );

    // And the flag never widens on its own: without an AllowOpenInvite value Go adds no
    // predicate at all, so this must not *narrow* to the governed team either.
    let unfiltered_but_flagged = TeamSearch {
        include_policy_enforced: Some(true),
        ..Default::default()
    };
    assert_eq!(listed(&pool, &unfiltered_but_flagged).await, all_seeded());

    // The computed column the widening keys on is hydrated on the way out.
    let governed = get_all_page(&pool, 0, 1000, &TeamSearch::default())
        .await
        .expect("the listing runs")
        .into_iter()
        .find(|t| t.id == TEAM_GOVERNED)
        .expect("the governed team is listed");
    assert!(governed.policy_enforced);
    assert!(governed.policy_is_active);

    purge(&pool).await;
}

/// `IncludePolicyID` projects `RetentionPoliciesTeams.PolicyId`; without it the column is not
/// selected and `Team.PolicyID` stays nil for every team, retained or not.
#[tokio::test]
async fn include_policy_id_is_the_only_thing_that_puts_a_policy_id_on_a_team() {
    if !db_enabled() {
        return;
    }
    let (_guard, pool) = setup().await;

    let with_id = TeamSearch {
        include_policy_id: Some(true),
        ..Default::default()
    };
    let teams = get_all_page(&pool, 0, 1000, &with_id)
        .await
        .expect("the listing runs");
    let retained = teams
        .iter()
        .find(|t| t.id == TEAM_RETAINED)
        .expect("listed");
    assert_eq!(retained.policy_id.as_deref(), Some(POLICY));
    for team in teams.iter().filter(|t| t.id.starts_with(PREFIX)) {
        if team.id != TEAM_RETAINED {
            assert_eq!(team.policy_id, None, "{} is in no policy", team.id);
        }
    }

    let without = get_all_page(&pool, 0, 1000, &TeamSearch::default())
        .await
        .expect("the listing runs");
    assert_eq!(
        without
            .iter()
            .find(|t| t.id == TEAM_RETAINED)
            .expect("listed")
            .policy_id,
        None,
        "the column is not selected without the flag"
    );

    purge(&pool).await;
}

/// `ExcludePolicyConstrained` drops teams that a retention policy governs — and it does so
/// independently of `IncludePolicyID`, which shares the same join.
#[tokio::test]
async fn exclude_policy_constrained_drops_the_retained_team() {
    if !db_enabled() {
        return;
    }
    let (_guard, pool) = setup().await;

    let excluded = TeamSearch {
        exclude_policy_constrained: Some(true),
        ..Default::default()
    };
    let mut expected = all_seeded();
    expected.retain(|id| *id != TEAM_RETAINED);
    assert_eq!(listed(&pool, &excluded).await, expected);

    let excluded_with_id = TeamSearch {
        exclude_policy_constrained: Some(true),
        include_policy_id: Some(true),
        ..Default::default()
    };
    assert_eq!(listed(&pool, &excluded_with_id).await, expected);

    purge(&pool).await;
}

/// `LIMIT 0` is an empty page, not "no limit" — squirrel renders `Limit(uint64(0))` literally.
/// The sibling `getChannelMembers` route means the opposite by the same parameter.
#[tokio::test]
async fn a_limit_of_zero_returns_nothing() {
    if !db_enabled() {
        return;
    }
    let (_guard, pool) = setup().await;

    let page = get_all_page(&pool, 0, 0, &TeamSearch::default())
        .await
        .expect("the listing runs");
    assert!(page.is_empty(), "LIMIT 0 selects no rows");

    purge(&pool).await;
}

/// Paging walks the `ORDER BY DisplayName` sequence. Asserted on the seeded window only: the
/// offsets here are relative to a listing the other worktrees share, so the test filters to a
/// contiguous run it owns rather than counting from row zero of the table.
#[tokio::test]
async fn offset_and_limit_walk_the_display_name_order() {
    if !db_enabled() {
        return;
    }
    let (_guard, pool) = setup().await;

    // The seeded names all begin with `~`, the last printable ASCII byte, so they are the tail
    // of the table's display-name order and their offset is (total - 6).
    let total = analytics_team_count(&pool, &TeamSearch::default())
        .await
        .expect("counts");
    let base = total - 6;

    let first_two: Vec<String> = get_all_page(&pool, base, 2, &TeamSearch::default())
        .await
        .expect("the listing runs")
        .into_iter()
        .map(|t| t.id)
        .collect();
    assert_eq!(first_two, vec![TEAM_ARCHIVED, TEAM_GOVERNED]);

    let next_two: Vec<String> = get_all_page(&pool, base + 2, 2, &TeamSearch::default())
        .await
        .expect("the listing runs")
        .into_iter()
        .map(|t| t.id)
        .collect();
    assert_eq!(next_two, vec![TEAM_NULL_INVITE, TEAM_OPEN]);

    purge(&pool).await;
}

/// The count's `DeleteAt` condition reads backwards: a **nil** `IncludeDeleted` takes the
/// permissive branch, so archived teams are counted — which is what makes `total_count` agree
/// with the listing. Only an explicit `Some(false)` filters them out, and no route sets it.
#[tokio::test]
async fn the_count_includes_archived_teams_unless_include_deleted_is_explicitly_false() {
    if !db_enabled() {
        return;
    }
    let guard = FIXTURES.lock().await;
    let pool = pool().await;
    purge(&pool).await;

    let default_before = analytics_team_count(&pool, &TeamSearch::default())
        .await
        .expect("counts");
    let excluded = TeamSearch {
        include_deleted: Some(false),
        ..Default::default()
    };
    let excluded_before = analytics_team_count(&pool, &excluded)
        .await
        .expect("counts");

    seed(&pool).await;

    assert_eq!(
        analytics_team_count(&pool, &TeamSearch::default())
            .await
            .expect("counts")
            - default_before,
        6,
        "all six seeded teams count, the archived one included"
    );
    assert_eq!(
        analytics_team_count(&pool, &excluded)
            .await
            .expect("counts")
            - excluded_before,
        5,
        "include_deleted = Some(false) is the only thing that drops it"
    );

    purge(&pool).await;
    drop(guard);
}

/// The count applies the open-invite filter — so `total_count` shrinks with the listing for a
/// single-permission caller — but it **ignores `ExcludePolicyConstrained` entirely**. A System
/// Console page asking for both therefore gets a total that counts teams its own list omits.
/// That is Go's answer; making the two agree would be the divergence.
#[tokio::test]
async fn the_count_honours_open_invite_but_not_the_policy_exclusion() {
    if !db_enabled() {
        return;
    }
    let guard = FIXTURES.lock().await;
    let pool = pool().await;
    purge(&pool).await;

    let public = TeamSearch {
        allow_open_invite: Some(true),
        ..Default::default()
    };
    let private = TeamSearch {
        allow_open_invite: Some(false),
        ..Default::default()
    };
    let excluded = TeamSearch {
        exclude_policy_constrained: Some(true),
        ..Default::default()
    };

    let public_before = analytics_team_count(&pool, &public).await.expect("counts");
    let private_before = analytics_team_count(&pool, &private).await.expect("counts");
    let excluded_before = analytics_team_count(&pool, &excluded)
        .await
        .expect("counts");

    seed(&pool).await;

    assert_eq!(
        analytics_team_count(&pool, &public).await.expect("counts") - public_before,
        1,
        "only the open-invite team"
    );
    assert_eq!(
        analytics_team_count(&pool, &private).await.expect("counts") - private_before,
        4,
        "the NULL-column team is in neither count"
    );
    assert_eq!(
        analytics_team_count(&pool, &excluded)
            .await
            .expect("counts")
            - excluded_before,
        6,
        "the retained team is still counted — the exclusion never reaches this query"
    );

    purge(&pool).await;
    drop(guard);
}

/// The ABAC widening reaches the count too, so the reported total matches the widened listing.
#[tokio::test]
async fn the_count_widens_with_include_policy_enforced() {
    if !db_enabled() {
        return;
    }
    let guard = FIXTURES.lock().await;
    let pool = pool().await;
    purge(&pool).await;

    let widened = TeamSearch {
        allow_open_invite: Some(true),
        include_policy_enforced: Some(true),
        ..Default::default()
    };
    let before = analytics_team_count(&pool, &widened).await.expect("counts");

    seed(&pool).await;

    assert_eq!(
        analytics_team_count(&pool, &widened).await.expect("counts") - before,
        2,
        "the open team and the governed private one"
    );

    purge(&pool).await;
    drop(guard);
}
