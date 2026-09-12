//! `SearchAll`, `SearchAllPaged`, `SearchOpen` and `SearchPrivate` — the four queries behind
//! `POST /api/v4/teams/search` — against a real Postgres.
//!
//! ```sh
//! docker compose up -d
//! export DATABASE_URL=postgres://mmuser:mmuser_password@localhost:5432/mattermost
//! MM_STORE_DB=1 cargo test -p mm-store --test db_team_search
//! ```
//!
//! # What this reaches that the parity suite cannot
//!
//! Three of the inputs cannot be produced over REST on a Team Edition deployment and are seeded
//! straight into the tables: a **NULL** `allowopeninvite`, a group-constrained team (LDAP group
//! sync is licensed), and a `RetentionPoliciesTeams` row. Each of them sits on a branch Go writes
//! with `IS NULL` / `NotEq` precisely because those rows exist, so a suite that could only create
//! teams through the API would leave every one of those branches unmeasured.
//!
//! # Every assertion is filtered to this file's own rows
//!
//! The search spans the whole `Teams` table, which three sibling worktrees also write to. Nothing
//! here asserts an absolute count.

use mm_model::team_search::TeamSearch;
use mm_store::team_store::{search_all, search_all_count, search_open_opts, search_private_opts};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

static FIXTURES: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// `~` is the last printable ASCII byte, so these display names sort after anything else in the
/// table and a leftover team from another suite cannot interleave with a positional assertion.
const PREFIX: &str = "mmrssrch";
const OPEN: &str = "mmrssrch000000000000000opn";
const PRIVATE: &str = "mmrssrch000000000000000prv";
const ARCHIVED: &str = "mmrssrch000000000000000arc";
const NULL_INVITE: &str = "mmrssrch00000000000000null";
const CONSTRAINED: &str = "mmrssrch000000000000000gcn";
const RETAINED: &str = "mmrssrch000000000000000ret";
const INVITE_TYPE: &str = "mmrssrch000000000000000inv";
const WILDCARD: &str = "mmrssrch000000000000000pct";
const POLICY: &str = "mmrssrch000000000000000pol";
/// NULL `name` **and** NULL `displayname` — the only row that can tell "the `ILIKE` clause was
/// built" from "no clause at all". Seeded by one test rather than by [`seed`], because every
/// other listing assertion in this file would have to carry it.
const NAMELESS: &str = "mmrssrch000000000000000nul";

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
        "DELETE FROM retentionpoliciesteams WHERE teamid LIKE 'mmrssrch%'",
        "DELETE FROM retentionpolicies WHERE id LIKE 'mmrssrch%'",
        "DELETE FROM accesscontrolpolicies WHERE id LIKE 'mmrssrch%'",
        "DELETE FROM teammembers WHERE teamid LIKE 'mmrssrch%'",
        "DELETE FROM teams WHERE id LIKE 'mmrssrch%'",
    ] {
        sqlx::query(statement)
            .execute(pool)
            .await
            .expect("purges leftover test rows");
    }
}

#[allow(clippy::too_many_arguments)]
async fn insert_team(
    pool: &PgPool,
    id: &str,
    display_name: &str,
    name: &str,
    team_type: &str,
    allow_open_invite: Option<bool>,
    group_constrained: Option<bool>,
    delete_at: i64,
) {
    sqlx::query(
        "INSERT INTO teams (id, createat, updateat, deleteat, displayname, name, description,
                            email, type, companyname, alloweddomains, inviteid, allowopeninvite,
                            lastteamiconupdate, schemeid, groupconstrained, cloudlimitsarchived)
         VALUES ($1, 1, 1, $7, $2, $3, '', $1 || '@mmrs.invalid', $4::text::team_type, '', '', $1, $5,
                 0, NULL, $6, false)",
    )
    .bind(id)
    .bind(display_name)
    .bind(name)
    .bind(team_type)
    .bind(allow_open_invite)
    .bind(group_constrained)
    .bind(delete_at)
    .execute(pool)
    .await
    .expect("inserts the team");
}

async fn seed(pool: &PgPool) {
    // display name                       name                type  aoi          gc      deleted
    insert_team(
        pool,
        ARCHIVED,
        "~mmrssrch a archived",
        "mmrssrch-arc",
        "O",
        Some(false),
        None,
        77,
    )
    .await;
    insert_team(
        pool,
        CONSTRAINED,
        "~mmrssrch b constrained",
        "mmrssrch-gcn",
        "O",
        Some(false),
        Some(true),
        0,
    )
    .await;
    insert_team(
        pool,
        INVITE_TYPE,
        "~mmrssrch c invitetype",
        "mmrssrch-inv",
        "I",
        Some(false),
        None,
        0,
    )
    .await;
    insert_team(
        pool,
        NULL_INVITE,
        "~mmrssrch d nullinvite",
        "mmrssrch-null",
        "O",
        None,
        None,
        0,
    )
    .await;
    insert_team(
        pool,
        OPEN,
        "~mmrssrch e open",
        "mmrssrch-opn",
        "O",
        Some(true),
        None,
        0,
    )
    .await;
    insert_team(
        pool,
        PRIVATE,
        "~mmrssrch f private",
        "mmrssrch-prv",
        "O",
        Some(false),
        None,
        0,
    )
    .await;
    insert_team(
        pool,
        RETAINED,
        "~mmrssrch g retained",
        "mmrssrch-ret",
        "O",
        Some(false),
        None,
        0,
    )
    .await;
    // The only team whose *name* carries a literal `%`, so an unescaped term would match every
    // other row and an escaped one matches only this.
    insert_team(
        pool,
        WILDCARD,
        "~mmrssrch h 100% Sure",
        "mmrssrch-pct",
        "O",
        Some(true),
        None,
        0,
    )
    .await;

    sqlx::query(
        "INSERT INTO retentionpolicies (id, displayname, postduration) VALUES ($1, $1, 30)",
    )
    .bind(POLICY)
    .execute(pool)
    .await
    .expect("inserts the retention policy");
    sqlx::query("INSERT INTO retentionpoliciesteams (policyid, teamid) VALUES ($1, $2)")
        .bind(POLICY)
        .bind(RETAINED)
        .execute(pool)
        .await
        .expect("attaches the team to the policy");
}

fn all_seeded() -> Vec<&'static str> {
    vec![
        ARCHIVED,
        CONSTRAINED,
        INVITE_TYPE,
        NULL_INVITE,
        OPEN,
        PRIVATE,
        RETAINED,
        WILDCARD,
    ]
}

async fn listed(pool: &PgPool, opts: &TeamSearch) -> Vec<String> {
    search_all(pool, opts)
        .await
        .expect("the search runs")
        .into_iter()
        .filter(|t| t.id.starts_with(PREFIX))
        .map(|t| t.id)
        .collect()
}

/// A term that only this file's rows can match, so the count query — which cannot be filtered
/// after the fact — is still safe to assert against a shared table.
fn term(value: &str) -> TeamSearch {
    TeamSearch {
        term: value.to_owned(),
        ..Default::default()
    }
}

async fn setup() -> (tokio::sync::MutexGuard<'static, ()>, PgPool) {
    let guard = FIXTURES.lock().await;
    let pool = pool().await;
    purge(&pool).await;
    seed(&pool).await;
    (guard, pool)
}

/// **No `DeleteAt` predicate.** An archived team that matches the term is returned, with its
/// `delete_at` intact. Adding the filter a reader expects would quietly shorten every team
/// directory search.
#[tokio::test]
async fn an_archived_team_is_searchable() {
    if !db_enabled() {
        return;
    }
    let (_guard, pool) = setup().await;

    let found = listed(&pool, &term("mmrssrch")).await;
    assert_eq!(found, all_seeded());

    let archived = search_all(&pool, &term("mmrssrch-arc"))
        .await
        .expect("the search runs");
    assert_eq!(archived.len(), 1);
    assert_eq!(archived[0].delete_at, 77, "returned, not repaired");

    purge(&pool).await;
}

/// The term matches `Name` **or** `DisplayName`, case-insensitively, and the wildcard characters
/// a caller sends are escaped rather than honoured. `100%` must find one team, not all of them.
#[tokio::test]
async fn the_term_matches_either_column_and_escapes_wildcards() {
    if !db_enabled() {
        return;
    }
    let (_guard, pool) = setup().await;

    // DisplayName only — `name` is `mmrssrch-opn`, which does not contain "open".
    assert_eq!(listed(&pool, &term("h e open")).await, vec![OPEN]);
    // Name only — no display name contains `-prv`.
    assert_eq!(listed(&pool, &term("-prv")).await, vec![PRIVATE]);
    // ILIKE, and `wildcardSearchTerm` lower-cases the needle as well.
    assert_eq!(listed(&pool, &term("MMRSSRCH-OPN")).await, vec![OPEN]);
    // The `%` is escaped, so it matches the literal one in "100% Sure" and nothing else.
    assert_eq!(listed(&pool, &term("100%")).await, vec![WILDCARD]);
    // `_` likewise: it would otherwise match any single character.
    assert!(listed(&pool, &term("mmrssrch_opn")).await.is_empty());

    purge(&pool).await;
}

/// **The empty-term guard reads the raw term, not the sanitised one.** A term of `\` sanitises to
/// `""` — `sanitizeSearchTerm` strips every occurrence of the escape character first — but
/// `term != ""` was already true, so the clause is built and renders as `ILIKE '%%'`. The channel
/// search guards on the *sanitised* value and drops the clause instead; the two routes genuinely
/// disagree here.
///
/// # The discriminator is a team with NULL name **and** NULL display name
///
/// `ILIKE '%%'` matches every row that has a value, and `NULL ILIKE '%%'` is NULL — not true — so
/// a clause that is *built* drops the NULL-column team while a clause that is *skipped* keeps it.
/// Without such a row the two behaviours are indistinguishable: both return every seeded team,
/// and the mutation that moves the guard onto the sanitised term **survived** the first run of
/// `scripts/mutations/team-write-family.plan` for exactly that reason. The right answer and the
/// wrong answer coincided.
///
/// Both columns are nullable in the schema and nothing over REST can produce such a row, so it is
/// inserted here and removed with the rest of the prefix.
#[tokio::test]
async fn a_term_of_nothing_but_escape_characters_matches_everything() {
    if !db_enabled() {
        return;
    }
    let (_guard, pool) = setup().await;

    // `ORDER BY displayname` puts NULL last in Postgres, so this team sorts after every seeded one.
    sqlx::query(
        "INSERT INTO teams (id, createat, updateat, deleteat, displayname, name, description,
                            email, type, companyname, alloweddomains, inviteid, allowopeninvite,
                            lastteamiconupdate, schemeid, groupconstrained, cloudlimitsarchived)
         VALUES ($1, 1, 1, 0, NULL, NULL, '', $1 || '@mmrs.invalid', 'O'::text::team_type, '', '',
                 $1, false, 0, NULL, NULL, false)",
    )
    .bind(NAMELESS)
    .execute(&pool)
    .await
    .expect("inserts the nameless team");

    // **Both reads happen before any assertion, and the row is deleted between them and the
    // asserts.** A NULL `name` breaks Go's own scan — `GET /api/v4/teams` and
    // `POST /api/v4/teams/search` both answer 500 `app.team.search_all_team.app_error` while this
    // row exists — so a failing assertion that skipped the trailing `purge` would take out every
    // other suite sharing this database. Measured, the hard way: it did.
    let backslash = listed(&pool, &term("\\")).await;
    let empty = listed(&pool, &TeamSearch::default()).await;
    purge(&pool).await;

    let mut with_nameless = all_seeded();
    with_nameless.push(NAMELESS);

    assert_eq!(
        backslash,
        all_seeded(),
        "the clause is built: `ILIKE '%%'` matches every row that has a value and drops the \
         NULL-column team"
    );
    assert_eq!(
        empty, with_nameless,
        "no clause at all is not the same as a clause that matches everything"
    );
}

/// `AllowOpenInvite = Some(false)` is **two** ANDed pairs: not-open *and* not-group-constrained.
/// Dropping the second pair widens a private-team search to teams an LDAP group owns. And both
/// halves use `IS DISTINCT FROM`, so the NULL-column team is excluded from the `true` listing and
/// present in the `false` one.
#[tokio::test]
async fn the_private_filter_also_excludes_group_constrained_teams() {
    if !db_enabled() {
        return;
    }
    let (_guard, pool) = setup().await;

    let open = TeamSearch {
        allow_open_invite: Some(true),
        ..term("mmrssrch")
    };
    assert_eq!(listed(&pool, &open).await, vec![OPEN, WILDCARD]);

    let private = TeamSearch {
        allow_open_invite: Some(false),
        ..term("mmrssrch")
    };
    assert_eq!(
        listed(&pool, &private).await,
        vec![ARCHIVED, INVITE_TYPE, NULL_INVITE, PRIVATE, RETAINED],
        "the group-constrained team is absent and the NULL-column team is present"
    );

    purge(&pool).await;
}

/// `GroupConstrained` on its own: `true` is `= true`, `false` is `IS DISTINCT FROM true` — so the
/// NULL rows, which are almost every team, count as not-constrained. `groupconstrained = false`
/// would return nothing at all.
#[tokio::test]
async fn the_group_constrained_filter_treats_null_as_not_constrained() {
    if !db_enabled() {
        return;
    }
    let (_guard, pool) = setup().await;

    let constrained = TeamSearch {
        group_constrained: Some(true),
        ..term("mmrssrch")
    };
    assert_eq!(listed(&pool, &constrained).await, vec![CONSTRAINED]);

    let unconstrained = TeamSearch {
        group_constrained: Some(false),
        ..term("mmrssrch")
    };
    let mut expected = all_seeded();
    expected.retain(|id| *id != CONSTRAINED);
    assert_eq!(listed(&pool, &unconstrained).await, expected);

    purge(&pool).await;
}

/// `SearchOpen` forces three fields, and the third is the one a reader drops: `GroupConstrained`
/// is reset to nil **whatever the caller sent**, so a caller-supplied `group_constrained: true`
/// cannot narrow — or a `false` widen — the mandatory public-only listing.
#[tokio::test]
async fn search_open_forces_its_three_fields_over_the_callers() {
    if !db_enabled() {
        return;
    }
    let (_guard, pool) = setup().await;

    let hostile = TeamSearch {
        allow_open_invite: Some(false),
        group_constrained: Some(true),
        ..term("mmrssrch")
    };
    let opts = search_open_opts(&hostile);
    assert_eq!(opts.team_type.as_deref(), Some("O"));
    assert_eq!(opts.allow_open_invite, Some(true));
    assert_eq!(opts.group_constrained, None);
    assert_eq!(listed(&pool, &opts).await, vec![OPEN, WILDCARD]);

    purge(&pool).await;
}

/// `SearchPrivate` sets `AllowOpenInvite = false` and clears `GroupConstrained`, and deliberately
/// sets **no `Type` filter** — privacy keys on `AllowOpenInvite` alone, so a closed team whose
/// `Type` is still `O` is private. Adding `Type = 'I'` to mirror the open case would drop every
/// one of them.
#[tokio::test]
async fn search_private_keys_on_open_invite_and_not_on_type() {
    if !db_enabled() {
        return;
    }
    let (_guard, pool) = setup().await;

    let hostile = TeamSearch {
        allow_open_invite: Some(true),
        group_constrained: Some(true),
        ..term("mmrssrch")
    };
    let opts = search_private_opts(&hostile);
    assert_eq!(opts.team_type, None, "no Type filter on the private search");
    assert_eq!(opts.allow_open_invite, Some(false));
    assert_eq!(opts.group_constrained, None);
    assert_eq!(
        listed(&pool, &opts).await,
        vec![ARCHIVED, INVITE_TYPE, NULL_INVITE, PRIVATE, RETAINED],
        "both the `O`-typed closed teams and the `I`-typed one"
    );

    purge(&pool).await;
}

/// Pagination is all-or-nothing: `IsPaginated()` needs **both** fields, so `page` alone returns
/// the whole result set unlimited. And the offset is `page * per_page`, not `page`.
#[tokio::test]
async fn only_both_pagination_fields_limit_the_result() {
    if !db_enabled() {
        return;
    }
    let (_guard, pool) = setup().await;

    let page_only = TeamSearch {
        page: Some(1),
        ..term("mmrssrch")
    };
    assert_eq!(
        listed(&pool, &page_only).await,
        all_seeded(),
        "page without per_page is not paginated"
    );

    let first = TeamSearch {
        page: Some(0),
        per_page: Some(3),
        ..term("mmrssrch")
    };
    assert_eq!(
        listed(&pool, &first).await,
        vec![ARCHIVED, CONSTRAINED, INVITE_TYPE]
    );

    let second = TeamSearch {
        page: Some(1),
        per_page: Some(3),
        ..term("mmrssrch")
    };
    assert_eq!(
        listed(&pool, &second).await,
        vec![NULL_INVITE, OPEN, PRIVATE]
    );

    // `per_page = 0` is a legitimate value, and squirrel renders `LIMIT 0` literally.
    let empty = TeamSearch {
        page: Some(0),
        per_page: Some(0),
        ..term("mmrssrch")
    };
    assert!(listed(&pool, &empty).await.is_empty());

    purge(&pool).await;
}

/// The count query carries **the same `WHERE` as the listing** — unlike `AnalyticsTeamCount`,
/// whose filters deliberately disagree with `GetAllPage`'s. So the count is the size of the whole
/// match, not of the page, and every filter moves it.
#[tokio::test]
async fn the_count_sees_the_same_rows_as_the_listing_and_ignores_the_page() {
    if !db_enabled() {
        return;
    }
    let (_guard, pool) = setup().await;

    let base = term("mmrssrch");
    assert_eq!(
        search_all_count(&pool, &base).await.expect("counts"),
        all_seeded().len() as i64
    );

    let paged = TeamSearch {
        page: Some(0),
        per_page: Some(2),
        ..term("mmrssrch")
    };
    assert_eq!(
        search_all_count(&pool, &paged).await.expect("counts"),
        all_seeded().len() as i64,
        "the LIMIT is dropped from the count query"
    );

    let private = TeamSearch {
        allow_open_invite: Some(false),
        ..term("mmrssrch")
    };
    assert_eq!(
        search_all_count(&pool, &private).await.expect("counts"),
        5,
        "every filter moves the count"
    );

    purge(&pool).await;
}

/// `exclude_policy_constrained` drops the team attached to a retention policy; `include_policy_id`
/// projects the policy id onto the rows instead of dropping them. Both are keyed off the *same*
/// left join, and Go's `else if` makes them mutually exclusive.
#[tokio::test]
async fn the_retention_policy_flags_drop_and_project() {
    if !db_enabled() {
        return;
    }
    let (_guard, pool) = setup().await;

    let excluded = TeamSearch {
        exclude_policy_constrained: Some(true),
        ..term("mmrssrch")
    };
    let mut expected = all_seeded();
    expected.retain(|id| *id != RETAINED);
    assert_eq!(listed(&pool, &excluded).await, expected);
    assert_eq!(
        search_all_count(&pool, &excluded).await.expect("counts"),
        expected.len() as i64,
        "the count honours it too — unlike AnalyticsTeamCount, which does not"
    );

    let projected = TeamSearch {
        include_policy_id: Some(true),
        ..term("mmrssrch")
    };
    let teams = search_all(&pool, &projected)
        .await
        .expect("the search runs");
    let retained = teams
        .iter()
        .find(|t| t.id == RETAINED)
        .expect("the retained team is still listed");
    assert_eq!(retained.policy_id.as_deref(), Some(POLICY));
    let open = teams.iter().find(|t| t.id == OPEN).expect("the open team");
    assert_eq!(open.policy_id, None);

    // Without the flag the column is not projected at all, even for the attached team.
    let teams = search_all(&pool, &term("mmrssrch"))
        .await
        .expect("the search runs");
    assert!(teams.iter().all(|t| t.policy_id.is_none()));

    purge(&pool).await;
}
