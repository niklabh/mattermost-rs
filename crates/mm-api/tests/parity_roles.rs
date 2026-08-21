//! Cross-server parity for `POST /api/v4/roles/names`, `GET /api/v4/roles/name/{role_name}` and
//! `GET /api/v4/roles/{role_id}`.
//!
//! ```sh
//! scripts/parity.sh -p mm-api --test parity_roles
//! ```
//!
//! # The one thing this suite exists to prove
//!
//! A role on the wire is the **database row**, not `model.MakeDefaultRoles()`. Every built-in
//! role's row starts life as a copy of the compiled default, so a port that answered from the
//! compiled table would pass every test written against a stock server and be catastrophically
//! wrong on any server whose permissions have ever been edited. So one test here patches a
//! built-in role away from its default through Go's own `PUT /roles/{id}/patch`, compares all
//! three routes against that, and asserts the answer is *not* the compiled default — then puts
//! the row back.
//!
//! # Go's role cache, and why the warm-up call is not superstition
//!
//! `LocalCacheRoleStore` (localcachelayer/role_layer.go) caches roles **by name** for 30 minutes.
//! `GetByNames` returns cache hits first, in requested order, then the queried misses — so Go's
//! response *order* for `POST /roles/names` depends on cache warmth, and the port matches the
//! warm case (see `roles.rs`). Every comparison here therefore calls Go once before comparing.
//! `Get` by **id** is not cached at all and needs no warm-up.
//!
//! That cache is also why the synthetic rows below get a **fresh name and id on every run**
//! rather than fixed ones: a name Go has never seen cannot be served from a stale cache entry,
//! and a fixed name is exactly what a previous run's edit would poison for the next 30 minutes.
//! Previous runs' rows are purged by id prefix on the way in.
//!
//! For the same reason nothing here writes through Go's API to a role it will then compare.
//! `PUT /roles/{id}/patch` with an **empty** body still calls `UpdateRole` → `Store().Save()`
//! (app/role.go:146-153), which stamps `UpdateAt` and de-duplicates `Permissions` — a "no-op"
//! request that silently rewrote a fixture row and produced a 200-line diff blamed on the port.

mod common;

use common::{
    GO, RUST, assert_error_bodies_match_except_known_gaps, client, fetch_both, fetch_both_raw,
    go_minted_token, post_both_raw, stack_enabled,
};

/// `strings.Fields` collapses every run of whitespace and keeps the duplicate: four entries, in
/// this order, with `create_post` appearing twice.
const SYNTHETIC_PERMISSIONS: &str = "  create_post \t edit_post  delete_post   create_post ";

/// The built-in role this suite is allowed to edit: granted to nobody on a stock server, so
/// patching it cannot change what any other test's actor may do.
const PATCHABLE_ROLE: &str = "system_post_all";

/// Two rows written straight into `Roles`, carrying everything the REST API cannot produce.
///
/// `messy` has a `Permissions` column with irregular spacing and a duplicate, a **non-zero
/// `DeleteAt`**, and a **NULL** `SchemeId`. `empty` is **scheme-managed** (so the higher-scoped
/// merge query actually runs for it) with an **empty** `Permissions` column.
struct Synthetic {
    messy_id: String,
    messy_name: String,
    empty_id: String,
    empty_name: String,
    /// A **channel scheme** and its three roles, plus a private channel pointing at the scheme —
    /// the only arrangement in which `ChannelHigherScopedPermissions` returns anything at all.
    /// See [`the_higher_scoped_merge_matches`].
    scheme_user_role: String,
    scheme_admin_role: String,
    scheme_guest_role: String,
    scheme_user_role_id: String,
}

/// The scheme roles' stored permissions, chosen so the merge visibly rewrites them:
/// `manage_system` is not channel-scoped and must vanish, `read_channel` is not moderated and
/// comes from the higher scope regardless, and `create_post` is moderated so it survives only
/// because the row lists it too.
const SCHEME_ROLE_PERMISSIONS: &str = " manage_system read_channel create_post";

/// Once per test binary — the harness runs tests concurrently and two of these racing would fight
/// over the primary key.
static SYNTHETIC: tokio::sync::OnceCell<Synthetic> = tokio::sync::OnceCell::const_new();

async fn synthetic_roles() -> &'static Synthetic {
    SYNTHETIC.get_or_init(write_synthetic_roles).await
}

async fn write_synthetic_roles() -> Synthetic {
    // Twelve digits of the clock, so the names and ids are new to Go's 30-minute role cache.
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("after 1970")
        .as_millis()
        % 1_000_000_000_000;
    let stamp = format!("{stamp:012}");
    let roles = Synthetic {
        // `mmrsparity` + 12 + 4 = 26 characters, all lower-case alphanumeric, so `IsValidId`
        // accepts them and gorilla's `[A-Za-z0-9]+` routes them.
        messy_id: format!("mmrsparity{stamp}0001"),
        messy_name: format!("mmrs_parity_role_{stamp}"),
        empty_id: format!("mmrsparity{stamp}0002"),
        empty_name: format!("mmrs_parity_empty_{stamp}"),
        scheme_user_role: format!("mmrs_scheme_user_{stamp}"),
        scheme_admin_role: format!("mmrs_scheme_admin_{stamp}"),
        scheme_guest_role: format!("mmrs_scheme_guest_{stamp}"),
        scheme_user_role_id: format!("mmrsparity{stamp}0003"),
    };
    assert_eq!(roles.messy_id.len(), 26);
    assert_eq!(roles.empty_id.len(), 26);
    assert_eq!(roles.scheme_user_role_id.len(), 26);
    let scheme_id = format!("mmrsscheme{stamp}0001");
    let channel_id = format!("mmrschan{stamp}000001");
    let scheme_admin_role_id = format!("mmrsparity{stamp}0004");
    let scheme_guest_role_id = format!("mmrsparity{stamp}0005");

    let url = std::env::var("DATABASE_URL")
        .expect("DATABASE_URL must be set for the stack-backed suites; scripts/parity.sh sets it");
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(std::time::Duration::from_secs(5))
        .connect(&url)
        .await
        .expect("the shared database is reachable");

    // Earlier runs' rows, by the id prefixes only this suite uses. Cleared on the way *in* rather
    // than at the end because an assertion panics past any trailing teardown — so a row does
    // survive between runs, and every one of them is written to be inert (see below). Children
    // first: the channel points at the scheme, the scheme names the roles, the team owns the
    // channel.
    for statement in [
        "DELETE FROM channels WHERE id LIKE 'mmrschan%'",
        "DELETE FROM schemes WHERE id LIKE 'mmrsscheme%'",
        "DELETE FROM roles WHERE id LIKE 'mmrsparity%'",
        "DELETE FROM teammembers WHERE teamid LIKE 'mmrsteam%'",
        "DELETE FROM teams WHERE id LIKE 'mmrsteam%'",
    ] {
        sqlx::query(statement)
            .execute(&pool)
            .await
            .expect("earlier synthetic rows are cleared");
    }

    sqlx::query(
        "INSERT INTO roles
            (id, name, displayname, description, createat, updateat, deleteat,
             permissions, schememanaged, builtin, schemeid)
         VALUES
            ($1, $2, 'mmrs parity role', 'written straight into the table',
             1701355039000, 1701355040000, 1701355041000, $3, false, false, NULL),
            ($4, $5, 'mmrs parity role (empty)', '', 1701355042000, 1701355043000, 0,
             '', true, false, '')",
    )
    .bind(&roles.messy_id)
    .bind(&roles.messy_name)
    .bind(SYNTHETIC_PERMISSIONS)
    .bind(&roles.empty_id)
    .bind(&roles.empty_name)
    .execute(&pool)
    .await
    .expect("the synthetic rows are written");

    // The three scheme roles. `SchemeManaged` is what puts them into the higher-scoped query, and
    // `SchemeId` is what ties them to the scheme.
    sqlx::query(
        "INSERT INTO roles
            (id, name, displayname, description, createat, updateat, deleteat,
             permissions, schememanaged, builtin, schemeid)
         VALUES
            ($1, $2, 'mmrs scheme user',  '', 1701355044000, 1701355044000, 0, $8, true, false, $7),
            ($3, $4, 'mmrs scheme admin', '', 1701355044000, 1701355044000, 0, $8, true, false, $7),
            ($5, $6, 'mmrs scheme guest', '', 1701355044000, 1701355044000, 0, $8, true, false, $7)",
    )
    .bind(&roles.scheme_user_role_id)
    .bind(&roles.scheme_user_role)
    .bind(&scheme_admin_role_id)
    .bind(&roles.scheme_admin_role)
    .bind(&scheme_guest_role_id)
    .bind(&roles.scheme_guest_role)
    .bind(&scheme_id)
    .bind(SCHEME_ROLE_PERMISSIONS)
    .execute(&pool)
    .await
    .expect("the scheme roles are written");

    sqlx::query(
        "INSERT INTO schemes
            (id, name, displayname, description, createat, updateat, deleteat, scope,
             defaultteamadminrole, defaultteamuserrole, defaultteamguestrole,
             defaultchanneladminrole, defaultchanneluserrole, defaultchannelguestrole)
         VALUES ($1, $2, 'mmrs parity scheme', '', 1701355045000, 1701355045000, 0, 'channel',
                 '', '', '', $3, $4, $5)",
    )
    .bind(&scheme_id)
    .bind(format!("mmrs-scheme-{stamp}"))
    .bind(&roles.scheme_admin_role)
    .bind(&roles.scheme_user_role)
    .bind(&roles.scheme_guest_role)
    .execute(&pool)
    .await
    .expect("the scheme is written");

    // # A team of our own, and every column spelled out
    //
    // The higher-scoped query's third branch needs a `Channels` row joined to a `Teams` row whose
    // own `SchemeId` is empty. The first version of this fixture borrowed the **fixture team** for
    // that, and both halves of the decision were wrong:
    //
    // 1. `GetTeamChannels` — which `GetNumberOfChannelsOnTeam` calls on **every**
    //    `POST /api/v4/channels` — selects whole rows for a team with no `DeleteAt` filter. Our
    //    row was therefore scanned by Go every time any suite created a channel on that team.
    // 2. That scan is into `model.Channel`, whose `TotalMsgCountRoot` and `LastRootPostAt` are
    //    plain `int64`. The insert omitted both, Postgres wrote NULL, and Go's scan failed — so
    //    channel creation on the shared team started answering 500 and took a sibling worktree's
    //    parity suite down with it.
    //
    // So: a team nothing else touches, and **every column Go's model persists gets a value**,
    // rather than trusting a database default for anything. The only columns left NULL are the
    // ones Go scans through a pointer (`SchemeId`, `GroupConstrained`, `Shared`, `BannerInfo`,
    // `PolicyId`), and [`assert_scannable_by_go`] below enforces exactly that split.
    let team_id = format!("mmrsteam{stamp}000001");
    sqlx::query(
        "INSERT INTO teams
            (id, createat, updateat, deleteat, displayname, name, description, email, type,
             companyname, alloweddomains, inviteid, schemeid, allowopeninvite,
             lastteamiconupdate, groupconstrained, cloudlimitsarchived)
         VALUES ($1, 1701355045000, 1701355045000, 0, 'mmrs parity scheme team', $2, '',
                 'mmrs-parity@mmrs.invalid', 'I', '', '', $3, NULL, false, 0, NULL, false)",
    )
    .bind(&team_id)
    .bind(format!("mmrs-scheme-team-{stamp}"))
    .bind(format!("mmrsinvite{stamp}0001"))
    .execute(&pool)
    .await
    .expect("the scheme's team is written");

    sqlx::query(
        "INSERT INTO channels
            (id, createat, updateat, deleteat, teamid, type, displayname, name,
             header, purpose, lastpostat, totalmsgcount, extraupdateat, creatorid, schemeid,
             groupconstrained, shared, totalmsgcountroot, lastrootpostat, bannerinfo,
             defaultcategoryname, autotranslation, discoverable)
         VALUES ($1, 1701355046000, 1701355046000, 0, $2, 'P', 'mmrs parity scheme channel', $3,
                 '', '', 0, 0, 0, '', $4,
                 NULL, NULL, 0, 0, NULL,
                 '', false, false)",
    )
    .bind(&channel_id)
    .bind(&team_id)
    .bind(format!("mmrs-scheme-channel-{stamp}"))
    .bind(&scheme_id)
    .execute(&pool)
    .await
    .expect("the scheme's channel is written");

    assert_scannable_by_go(&pool, &team_id, &channel_id).await;

    roles
}

/// Fail loudly if any column Go scans into a **non-pointer** field was left NULL.
///
/// A row this suite plants is read back by the *Go* server, through `sqlx`'s struct scan into
/// `model.Team` / `model.Channel`. A NULL in a column whose Go field is a plain `int64`, `string`
/// or `bool` is not a wrong value — it is a scan error that fails the whole query, for every
/// caller, until the row is removed. That is a shared-database outage caused by a test fixture,
/// and it has happened once already.
///
/// Listed positively rather than derived from `information_schema`: the database's own
/// nullability is *looser* than Go's (almost every column here is nullable in the schema), so it
/// cannot answer this question. The authority is the struct definition in `model/`.
async fn assert_scannable_by_go(pool: &sqlx::PgPool, team_id: &str, channel_id: &str) {
    let bad_teams: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM teams WHERE id = $1 AND (
             createat IS NULL OR updateat IS NULL OR deleteat IS NULL OR displayname IS NULL
          OR name IS NULL OR description IS NULL OR email IS NULL OR type IS NULL
          OR companyname IS NULL OR alloweddomains IS NULL OR inviteid IS NULL
          OR allowopeninvite IS NULL OR lastteamiconupdate IS NULL OR cloudlimitsarchived IS NULL)",
    )
    .bind(team_id)
    .fetch_one(pool)
    .await
    .expect("the guard query runs");
    assert_eq!(bad_teams, 0, "the fixture team has a column Go cannot scan");

    let bad_channels: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM channels WHERE id = $1 AND (
             createat IS NULL OR updateat IS NULL OR deleteat IS NULL OR teamid IS NULL
          OR type IS NULL OR displayname IS NULL OR name IS NULL OR header IS NULL
          OR purpose IS NULL OR lastpostat IS NULL OR totalmsgcount IS NULL
          OR extraupdateat IS NULL OR creatorid IS NULL OR totalmsgcountroot IS NULL
          OR lastrootpostat IS NULL OR defaultcategoryname IS NULL OR autotranslation IS NULL
          OR discoverable IS NULL)",
    )
    .bind(channel_id)
    .fetch_one(pool)
    .await
    .expect("the guard query runs");
    assert_eq!(
        bad_channels, 0,
        "the fixture channel has a column Go cannot scan — `GetTeamChannels` would 500 for the \
         whole team"
    );
}

/// Warm Go's `roleCache` for these names, so the `GetByNames` order it answers with is the
/// request order rather than the table's heap order. See the module note.
async fn warm_go_role_cache(client: &reqwest::Client, token: &str, names: &[&str]) {
    let response = client
        .post(format!("{GO}/api/v4/roles/names"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&names)
        .send()
        .await
        .expect("Go answers");
    assert_eq!(response.status(), 200, "warming the cache should succeed");
}

/// Read one role from Go by name, returning the parsed body.
async fn go_role(client: &reqwest::Client, token: &str, name: &str) -> serde_json::Value {
    client
        .get(format!("{GO}/api/v4/roles/name/{name}"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("Go answers")
        .json()
        .await
        .expect("the role decodes")
}

/// `PUT /api/v4/roles/{id}/patch` through Go — the only path that both writes the row *and*
/// invalidates Go's cache for its name.
async fn patch_role_permissions(
    client: &reqwest::Client,
    token: &str,
    role_id: &str,
    permissions: &[String],
) {
    let response = client
        .put(format!("{GO}/api/v4/roles/{role_id}/patch"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!({ "permissions": permissions }))
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "patching {role_id} failed: {}",
        response.text().await.unwrap_or_default()
    );
}

/// POST a JSON array of names to both servers and return `(go_body, rust_body)`, having warmed
/// Go's cache first. Asserts both answered 200 and that ours was not forwarded.
async fn post_names_both(
    client: &reqwest::Client,
    token: &str,
    names: &[&str],
) -> (Vec<u8>, Vec<u8>) {
    warm_go_role_cache(client, token, names).await;
    let body = serde_json::to_vec(names).expect("serialises");
    let ((go_status, go_body), (rs_status, rs_body)) =
        post_both_raw(client, token, "/api/v4/roles/names", &body).await;
    assert_eq!(go_status, 200, "Go should serve {names:?}");
    assert_eq!(rs_status, 200, "we should serve {names:?}");
    (go_body, rs_body)
}

/// A GET whose response we expect to be **forwarded**, so `fetch_both_raw`'s served-by-rust
/// assertion would be wrong. Returns Go's answer and ours plus our cutover marker.
async fn fetch_forwarded(
    client: &reqwest::Client,
    token: &str,
    path: &str,
) -> ((u16, Vec<u8>), (u16, Vec<u8>), String) {
    let get = async |base: &str| {
        let response = client
            .get(format!("{base}{path}"))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .unwrap_or_else(|e| panic!("{base}{path} is unreachable: {e}"));
        let status = response.status().as_u16();
        let served_by = response
            .headers()
            .get("x-mmrs-served-by")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        (
            status,
            response.bytes().await.expect("body reads").to_vec(),
            served_by,
        )
    };

    let (go_status, go_body, _) = get(GO).await;
    let (rs_status, rs_body, served_by) = get(RUST).await;
    ((go_status, go_body), (rs_status, rs_body), served_by)
}

// ---------------------------------------------------------------------------
// The three routes, on rows nobody has touched.
// ---------------------------------------------------------------------------

/// Every built-in role, by name, in one request — the shape the webapp sends on page load, plus
/// the whole table so that no single row's quirk goes unexercised.
#[tokio::test]
async fn roles_by_names_matches_for_every_builtin_role() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;

    // Deliberately *not* in sorted order: the request is what `SortedArrayFromJSON` sorts, and a
    // pre-sorted request could not tell a sort from a passthrough.
    let names = [
        "team_user",
        "channel_admin",
        "system_user",
        "system_admin",
        "channel_user",
        "system_guest",
        "team_admin",
        "team_guest",
        "channel_guest",
        "custom_group_user",
        "playbook_admin",
        "run_member",
        "system_manager",
        "system_read_only_admin",
        "system_user_manager",
        "system_post_all_public",
    ];

    let (go, rs) = post_names_both(&client, &token, &names).await;
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "POST /roles/names must be byte-identical"
    );

    // And the bytes actually carry sixteen roles — a suite comparing two empty arrays passes.
    let parsed: Vec<serde_json::Value> = serde_json::from_slice(&go).expect("an array");
    assert_eq!(parsed.len(), names.len());
}

/// The single-role routes on a built-in, both of which end in `Encode` and therefore a newline.
#[tokio::test]
async fn a_builtin_role_matches_by_name_and_by_id() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;

    for name in ["system_user", "channel_user", "system_admin", "run_member"] {
        let (go, rs) = fetch_both(&client, &token, &format!("/api/v4/roles/name/{name}")).await;
        assert_eq!(
            String::from_utf8_lossy(&go),
            String::from_utf8_lossy(&rs),
            "/roles/name/{name} must be byte-identical"
        );
        assert_eq!(
            go.last(),
            Some(&b'\n'),
            "Encode writes a trailing newline; if Go stops doing so, ours must too"
        );

        let role: serde_json::Value = serde_json::from_slice(&go).expect("a role");
        let id = role["id"].as_str().expect("an id");
        // `scheme_id` is **null** for a seeded built-in role — the column is NULL, and the Go
        // field is a `*string` with no `omitempty`, so the key is present and null rather than
        // absent or `""`. Read off Go's own answer, because the first guess here was `""`.
        assert_eq!(role["scheme_id"], serde_json::Value::Null, "{name}");

        let (go_by_id, rs_by_id) =
            fetch_both(&client, &token, &format!("/api/v4/roles/{id}")).await;
        assert_eq!(
            String::from_utf8_lossy(&go_by_id),
            String::from_utf8_lossy(&rs_by_id),
            "/roles/{id} must be byte-identical"
        );
        assert_eq!(
            String::from_utf8_lossy(&go_by_id),
            String::from_utf8_lossy(&go),
            "the two single-role routes must answer identically for the same role"
        );
    }
}

// ---------------------------------------------------------------------------
// The row wins over the compiled default.
// ---------------------------------------------------------------------------

/// Patch a built-in role away from `MakeDefaultRoles`, then read it back through all three
/// routes.
///
/// The restore happens **before** the assertions on purpose: a failing assertion panics past any
/// trailing cleanup, and leaving a built-in role permanently short of a permission because a test
/// failed is not an acceptable way to learn about a divergence.
///
/// Note that Go's handler runs `RemoveDuplicateStrings` over the patch (api4/role.go:181), which
/// **sorts**, so the restored row's permission order is alphabetical rather than whatever order
/// it originally held. The set is preserved and both servers read the one row, so parity is
/// unaffected; the ordering change is a recorded side effect of this suite on the dev database.
#[tokio::test]
async fn a_patched_row_beats_the_compiled_default() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;

    let before = go_role(&client, &token, PATCHABLE_ROLE).await;
    let role_id = before["id"].as_str().expect("an id").to_owned();
    let original: Vec<String> = before["permissions"]
        .as_array()
        .expect("a permission list")
        .iter()
        .filter_map(|p| p.as_str().map(str::to_owned))
        .collect();

    // Remove a permission the **compiled default also lists**, so that an answer sourced from
    // `MakeDefaultRoles` would provably still carry it. Picking the row's last entry instead was
    // not enough: the seeded row already holds two permissions the compiled default does not
    // (`upload_file`, `use_group_mentions`, added by the ancillary-permission migrations), so
    // dropping one of those would have proved nothing.
    let defaults = mm_model::role::make_default_roles();
    let compiled: Vec<String> = defaults
        .get(PATCHABLE_ROLE)
        .expect("a compiled default exists for this name")
        .permissions
        .clone()
        .unwrap_or_default();
    let removed = original
        .iter()
        .find(|permission| compiled.contains(permission))
        .unwrap_or_else(|| {
            panic!(
                "{PATCHABLE_ROLE}'s row {original:?} shares nothing with its default {compiled:?}"
            )
        })
        .clone();

    let patched: Vec<String> = original
        .iter()
        .filter(|permission| **permission != removed)
        .cloned()
        .collect();
    patch_role_permissions(&client, &token, &role_id, &patched).await;

    let by_name = fetch_both(
        &client,
        &token,
        &format!("/api/v4/roles/name/{PATCHABLE_ROLE}"),
    )
    .await;
    let by_id = fetch_both(&client, &token, &format!("/api/v4/roles/{role_id}")).await;
    let by_names = post_names_both(&client, &token, &[PATCHABLE_ROLE]).await;

    patch_role_permissions(&client, &token, &role_id, &original).await;

    for (label, (go, rs)) in [
        ("by name", &by_name),
        ("by id", &by_id),
        ("by names", &by_names),
    ] {
        assert_eq!(
            String::from_utf8_lossy(go),
            String::from_utf8_lossy(rs),
            "{label} on a patched role must be byte-identical"
        );
    }

    // The compiled default still lists the permission the row no longer has, so an answer that
    // came from `MakeDefaultRoles` would carry it. This is the assertion the whole test is for.
    let served: serde_json::Value = serde_json::from_slice(&by_name.1).expect("a role");
    let served_permissions: Vec<&str> = served["permissions"]
        .as_array()
        .expect("a list")
        .iter()
        .filter_map(|p| p.as_str())
        .collect();
    assert!(
        !served_permissions.contains(&removed.as_str()),
        "we answered with {removed}, which only the compiled default still has — the row was not \
         what reached the wire"
    );
    assert_eq!(served_permissions.len(), patched.len());
}

// ---------------------------------------------------------------------------
// Rows the REST API cannot produce.
// ---------------------------------------------------------------------------

/// The synthetic rows: irregular whitespace and a duplicate in `Permissions`, an empty
/// `Permissions`, a non-zero `DeleteAt`, and a NULL `SchemeId`.
#[tokio::test]
async fn a_hand_written_row_matches_on_all_three_routes() {
    if !stack_enabled() {
        return;
    }
    let synthetic = synthetic_roles().await;
    let client = client();
    let token = go_minted_token(&client).await;

    for (name, id) in [
        (&synthetic.messy_name, &synthetic.messy_id),
        (&synthetic.empty_name, &synthetic.empty_id),
    ] {
        let (go, rs) = fetch_both(&client, &token, &format!("/api/v4/roles/name/{name}")).await;
        assert_eq!(
            String::from_utf8_lossy(&go),
            String::from_utf8_lossy(&rs),
            "/roles/name/{name}"
        );

        let (go_by_id, rs_by_id) =
            fetch_both(&client, &token, &format!("/api/v4/roles/{id}")).await;
        assert_eq!(
            String::from_utf8_lossy(&go_by_id),
            String::from_utf8_lossy(&rs_by_id),
            "/roles/{id}"
        );

        let (go_names, rs_names) = post_names_both(&client, &token, &[name.as_str()]).await;
        assert_eq!(
            String::from_utf8_lossy(&go_names),
            String::from_utf8_lossy(&rs_names),
            "POST /roles/names for {name}"
        );
    }

    // And the specific properties the rows were written to carry, read off Go's own answer so
    // that they are claims about Mattermost rather than about this port.
    let messy = go_role(&client, &token, &synthetic.messy_name).await;
    assert_eq!(
        messy["permissions"],
        serde_json::json!(["create_post", "edit_post", "delete_post", "create_post"]),
        "strings.Fields collapses whitespace runs and keeps duplicates"
    );
    assert_eq!(
        messy["delete_at"],
        serde_json::json!(1701355041000i64),
        "a soft-deleted role is still returned — nothing filters DeleteAt"
    );
    assert_eq!(
        messy["scheme_id"],
        serde_json::Value::Null,
        "a NULL SchemeId is null on the wire"
    );

    let empty = go_role(&client, &token, &synthetic.empty_name).await;
    assert_eq!(
        empty["permissions"],
        serde_json::json!([]),
        "an empty Permissions column is [], never null"
    );
    assert_eq!(
        empty["scheme_managed"],
        serde_json::json!(true),
        "this row is the one that makes the higher-scoped merge query run"
    );
}

/// The higher-scoped merge — the one path on these routes that rewrites a role's permissions
/// between the row and the wire, and the only consumer of the three-branch UNION in
/// `role_store.rs`.
///
/// It cannot be reached on a stock server: it needs a **channel scheme**, and creating one needs
/// an enterprise licence. So the fixture plants the scheme, its three roles and a channel that
/// uses it directly in the database, which makes the query's third branch match — the branch Go's
/// own comment (role_store.go:407) says exists because no system scheme record ships with
/// Mattermost, so the built-in channel roles are matched by *name* there rather than by column.
///
/// What the merge then does is not subtle, which is the point:
///
/// - `manage_system` is on the row and is **not channel-scoped**, so it is gone.
/// - `read_channel` comes from the higher scope whether or not the row had it.
/// - `create_post` is **moderated**, so it survives only because both the row and the higher
///   scope list it.
/// - the **admin** role takes everything from the higher scope regardless of moderation — its
///   own branch in `MergeChannelHigherScopedPermissions` (role.go:599).
#[tokio::test]
async fn the_higher_scoped_merge_matches() {
    if !stack_enabled() {
        return;
    }
    let synthetic = synthetic_roles().await;
    let client = client();
    let token = go_minted_token(&client).await;

    let names = [
        synthetic.scheme_user_role.as_str(),
        synthetic.scheme_admin_role.as_str(),
        synthetic.scheme_guest_role.as_str(),
    ];

    let (go, rs) = post_names_both(&client, &token, &names).await;
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "the merged permissions must be byte-identical"
    );

    let (go_by_name, rs_by_name) = fetch_both(
        &client,
        &token,
        &format!("/api/v4/roles/name/{}", synthetic.scheme_user_role),
    )
    .await;
    assert_eq!(
        String::from_utf8_lossy(&go_by_name),
        String::from_utf8_lossy(&rs_by_name)
    );

    let (go_by_id, rs_by_id) = fetch_both(
        &client,
        &token,
        &format!("/api/v4/roles/{}", synthetic.scheme_user_role_id),
    )
    .await;
    assert_eq!(
        String::from_utf8_lossy(&go_by_id),
        String::from_utf8_lossy(&rs_by_id)
    );

    // And the merge actually fired, rather than both servers agreeing to do nothing. Read off
    // Go's answer, so this is a claim about Mattermost.
    let user_role: serde_json::Value = serde_json::from_slice(&go_by_name).expect("a role");
    let merged: Vec<&str> = user_role["permissions"]
        .as_array()
        .expect("a list")
        .iter()
        .filter_map(|p| p.as_str())
        .collect();
    assert!(
        !merged.contains(&"manage_system"),
        "the row lists manage_system; it is not channel-scoped, so the merge drops it — {merged:?}"
    );
    assert!(
        merged.contains(&"read_channel"),
        "unmoderated and on the higher scope — {merged:?}"
    );
    assert!(
        merged.contains(&"create_post"),
        "moderated, and on both the row and the higher scope — {merged:?}"
    );
    assert!(
        merged.len() > 3,
        "the merge rebuilds from the higher scope, so the list grows well past the row's three \
         entries — {merged:?}"
    );
}

/// An unknown name is absent from the array rather than an error — but when *every* name is
/// unknown the whole answer is `null`, not `[]`.
///
/// That is the cache layer, not the SQL store: `LocalCacheRoleStore.GetByNames` starts from a
/// **nil** slice and ends with `append(foundRoles, roles...)` (role_layer.go:70, :104), and
/// appending nothing to nil is still nil. So "empty" and "nil" coincide exactly, and `null` is
/// what every client receives. See `roles.rs`.
#[tokio::test]
async fn an_unknown_name_is_absent_and_an_all_unknown_request_is_null() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;

    let names = ["system_user", "no_such_role_at_all"];
    let (go, rs) = post_names_both(&client, &token, &names).await;
    assert_eq!(String::from_utf8_lossy(&go), String::from_utf8_lossy(&rs));

    let parsed: Vec<serde_json::Value> = serde_json::from_slice(&go).expect("an array");
    assert_eq!(parsed.len(), 1);
    assert_eq!(parsed[0]["name"], "system_user");

    // A request for nothing but unknown names is `null`, and no trailing newline.
    for request in [
        vec!["no_such_role_at_all"],
        vec!["no_such_role_at_all", "nor_this_one"],
    ] {
        let (go, rs) = post_names_both(&client, &token, &request).await;
        assert_eq!(go, b"null", "Go answers null for {request:?}");
        assert_eq!(rs, b"null", "and so must we");
    }
}

// ---------------------------------------------------------------------------
// Refusals. Error bodies are wire format too.
// ---------------------------------------------------------------------------

/// Every rejected body shape for `POST /roles/names`, in Go's branch order.
#[tokio::test]
async fn the_names_route_refuses_identically() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;

    let too_many: Vec<String> = (0..101).map(|i| format!("role_{i}")).collect();
    let too_many = serde_json::to_vec(&too_many).expect("serialises");
    let sixty_five = format!(r#"["{}"]"#, "a".repeat(65));

    let bodies: Vec<(&str, &[u8])> = vec![
        ("not json", b"not json at all"),
        ("an object", b"{}"),
        ("numbers", b"[1,2]"),
        ("null", b"null"),
        ("empty array", b"[]"),
        ("too many names", &too_many),
        ("upper case", br#"["System_User"]"#),
        ("a hyphen", br#"["system-user"]"#),
        ("untrimmed", br#"[" system_user "]"#),
        ("over 64 bytes", sixty_five.as_bytes()),
    ];

    for (label, body) in bodies {
        let ((go_status, go_body), (rs_status, rs_body)) =
            post_both_raw(&client, &token, "/api/v4/roles/names", body).await;
        assert_eq!(go_status, rs_status, "{label}: status");
        assert_eq!(go_status, 400, "{label}: Go should refuse this");
        assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, label);
    }

    // A body of nothing but blanks is *accepted* — `CleanRoleNames` drops them rather than
    // rejecting — and answers `null`, by the same nil-slice route as an all-unknown request.
    let ((go_status, go_body), (rs_status, rs_body)) =
        post_both_raw(&client, &token, "/api/v4/roles/names", br#"["", "  "]"#).await;
    assert_eq!((go_status, go_body.as_slice()), (200, b"null".as_slice()));
    assert_eq!((rs_status, rs_body.as_slice()), (200, b"null".as_slice()));
}

/// The two single-role routes' 400 and 404 branches.
#[tokio::test]
async fn the_single_role_routes_refuse_identically() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;

    // `role_name`: the mux class already ate every bad charset, so the only 400 left is length.
    let sixty_five = "a".repeat(65);
    for (label, path, expected) in [
        (
            "a 65-byte role name",
            format!("/api/v4/roles/name/{sixty_five}"),
            400,
        ),
        (
            "a well-formed name that is no role",
            "/api/v4/roles/name/no_such_role_at_all".to_owned(),
            404,
        ),
        (
            "a 25-character role id",
            "/api/v4/roles/zzzzzzzzzzzzzzzzzzzzzzzzz".to_owned(),
            400,
        ),
        (
            "a 27-character role id",
            "/api/v4/roles/zzzzzzzzzzzzzzzzzzzzzzzzzzz".to_owned(),
            400,
        ),
        (
            "a valid id that is no role",
            "/api/v4/roles/zzzzzzzzzzzzzzzzzzzzzzzzzz".to_owned(),
            404,
        ),
    ] {
        let ((go_status, go_body), (rs_status, rs_body)) =
            fetch_both_raw(&client, &token, &path).await;
        assert_eq!(go_status, rs_status, "{label}: status");
        assert_eq!(go_status, expected, "{label}: Go's status");
        assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, label);
    }
}

/// Segments outside Go's mux classes never reach a handler there, so they must not reach one
/// here either: the router forwards and Go answers its own 404.
#[tokio::test]
async fn segments_outside_gos_mux_classes_are_forwarded() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;

    for (label, path) in [
        // `{role_name:[a-z0-9_]+}` — upper case and hyphens are not in the class.
        (
            "upper case in a role name",
            "/api/v4/roles/name/System_User",
        ),
        ("a hyphen in a role name", "/api/v4/roles/name/system-user"),
        // `{role_id:[A-Za-z0-9]+}` — the shared id middleware forwards this one.
        (
            "a hyphen in a role id",
            "/api/v4/roles/aaaaaaaaaa-aaaaaaaaaaaaaaa",
        ),
    ] {
        let ((go_status, go_body), (rs_status, rs_body), served_by) =
            fetch_forwarded(&client, &token, path).await;
        assert_eq!(
            served_by, "go",
            "{label}: this must be forwarded, not answered locally"
        );
        assert_eq!(go_status, rs_status, "{label}: status");
        assert_eq!(go_status, 404, "{label}: gorilla's own mux 404");
        assert_eq!(
            without_request_id(&go_body),
            without_request_id(&rs_body),
            "{label}: a forwarded body is Go's own, bar the per-request id"
        );
    }
}

/// A parsed error body with `request_id` removed — the only key that can never match, and the
/// only difference expected between two bodies both written by Go.
fn without_request_id(body: &[u8]) -> serde_json::Value {
    let mut value: serde_json::Value = serde_json::from_slice(body).expect("an error body");
    if let Some(object) = value.as_object_mut() {
        object.remove("request_id");
    }
    value
}

/// `GET /api/v4/roles/names` is *not* this route. gorilla registered `{role_id}` before the
/// literal `names`, so Go treats it as `getRole("names")` and 400s; registering the literal as
/// POST-only here keeps that true, via the method fallback and the proxy.
#[tokio::test]
async fn a_get_to_the_names_path_is_still_gos_invalid_role_id() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;

    let ((go_status, go_body), (rs_status, rs_body), served_by) =
        fetch_forwarded(&client, &token, "/api/v4/roles/names").await;
    assert_eq!(served_by, "go", "a GET here must be forwarded");
    assert_eq!(go_status, 400);
    assert_eq!(rs_status, 400);

    let go: serde_json::Value = serde_json::from_slice(&go_body).expect("JSON");
    let rs: serde_json::Value = serde_json::from_slice(&rs_body).expect("JSON");
    assert_eq!(go["id"], "api.context.invalid_url_param.app_error");
    assert_eq!(rs["id"], go["id"]);
}

/// `GET /api/v4/roles` (`getAllRoles`) and `PUT /roles/{id}/patch` are not migrated, so they must
/// still be Go's — a route added carelessly beside them would 405 or 404 instead.
#[tokio::test]
async fn the_unmigrated_role_routes_are_still_forwarded() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;

    let (_, (rs_status, _), served_by) = fetch_forwarded(&client, &token, "/api/v4/roles").await;
    assert_eq!(served_by, "go", "getAllRoles is not migrated");
    assert_eq!(rs_status, 200, "and the fixture user is a system_admin");

    // Aimed at an id that is **no role**, so Go 404s before `PatchRole` can touch anything. A
    // patch at a real role — even with an empty body — calls `Store().Save()` and rewrites the
    // row's `UpdateAt` and `Permissions`; doing that to a fixture here cost a debugging session.
    let response = client
        .put(format!(
            "{RUST}/api/v4/roles/zzzzzzzzzzzzzzzzzzzzzzzzzz/patch"
        ))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!({}))
        .send()
        .await
        .expect("we answer");
    assert_eq!(response.status(), 404, "no such role");
    assert_eq!(
        response
            .headers()
            .get("x-mmrs-served-by")
            .and_then(|v| v.to_str().ok()),
        Some("go"),
        "patchRole is not migrated"
    );
}
