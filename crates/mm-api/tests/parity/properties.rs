//! Cross-server parity for the four read routes of `api4/properties.go`.
//!
//! ```sh
//! scripts/parity.sh -p mm-api --test parity properties
//! ```
//!
//! # The `boards` group is what makes this suite possible
//!
//! Its sibling, [`crate::parity::custom_profile_attributes`], can only ever compare *refusals*:
//! every call it makes runs into a licence hook. The hook is registered against one group id
//! (app/server.go:325), and `RegisterBuiltinGroups` writes `boards` and `post_attributes`
//! unconditionally beside it — so those two are PSAv2 groups with **no hook on the read path**,
//! readable on Team Edition, and the whole predicate set of `SearchPropertyFields` is
//! comparable against Go on this stack. That is why the fixture below lives in `boards`.
//!
//! # A fixture where the right answer and the wrong answer must not coincide
//!
//! Six field rows and three value rows, and the timestamps are chosen so that **`create_at`
//! order, `update_at` order and `id` order are three different orders**. Ordering by the wrong
//! column, or paging on the wrong cursor key, changes the answer — which is not true of any
//! fixture whose rows were inserted in one loop. The soft-deleted row is deliberately the
//! *earliest* by `create_at`, so a dropped `DeleteAt = 0` puts it first rather than last.
//!
//! # Every test holds [`common::PROPERTY_ROWS`]
//!
//! Two suites write these two tables. See that lock's docs.

use crate::common;

use common::{
    GO, RUST, assert_error_bodies_match_except_known_gaps, client, fetch_both_raw, go_minted_token,
    post_both_raw, stack_enabled,
};

/// `boards` — registered by `RegisterBuiltinGroups` on every edition, version 2, no hooks.
const GROUP: &str = "boards";

/// Field ids. **Exactly 26 characters** — `IsValidId` is what a cursor is checked against
/// (`PropertyFieldSearchCursor.IsValid`), so a 25-character id makes every cursor request a 400
/// and every paging test a comparison of two identical refusals. `mmrsprop`-prefixed so the purge
/// and a crashed run can find them.
const F_SYSTEM: &str = "mmrspropfieldsystem0000001";
const F_TEAM: &str = "mmrspropfieldteam000000001";
const F_CHANNEL: &str = "mmrspropfieldchannel000001";
const F_DELETED: &str = "mmrspropfielddeleted000001";
const F_POSTOBJ: &str = "mmrspropfieldpostobj000001";
const F_SYSOBJ: &str = "mmrspropfieldsysobj0000001";
const F_DM: &str = "mmrspropfielddm00000000001";

const V_ONE: &str = "mmrspropvalueone0000000001";
const V_TWO: &str = "mmrspropvaluetwo0000000001";
const V_DELETED: &str = "mmrspropvaluedeleted000001";
/// A value on the **DM** channel: the only evidence that `TargetID` is a real predicate rather
/// than decoration, since every other planted value sits on the one channel the tests ask about.
const V_OTHER_TARGET: &str = "mmrspropvalueotherchan0001";
/// A value at the **system** target, which is what the dedicated system-values route reads.
const V_SYSTEM: &str = "mmrspropvaluesystem0000001";

/// The `create_at` and `update_at` the fixture uses, laid out so no two orderings agree.
///
/// | row | object type | target | create_at | update_at |
/// |---|---|---|---|---|
/// | channel field | channel | this channel | 100 | 700 |
/// | deleted field | channel | this channel | 50 | 600 |
/// | team field | channel | this team | 200 | **950** |
/// | system-target field | channel | system | 300 | **800** |
/// | DM-channel field | channel | the DM | 150 | 850 |
/// | post-object field | post | this channel | 400 | 500 |
/// | system-object field | system | system | 350 | 1050 |
///
/// (offsets in units of 1000ms from [`EPOCH`].) The two bold entries are the ones that matter:
/// they put the team field **last** by `update_at` and **third** by `create_at`, so the four rows
/// a channel-scoped search returns come back in two different orders depending on the mode. An
/// earlier version of this fixture had `update_at` ascending in the same sequence as `create_at`,
/// and a mutation replacing the mode switch with a bare `ORDER BY createat` **survived** the whole
/// suite. Neither order matches the id order either.
const EPOCH: i64 = 1_788_600_000_000;

/// Plant the six fields and three values. Returns false when there is no database to plant into.
async fn plant() -> bool {
    // The same ordering rule as [`plant_delete_fixture`]: nothing may be written before
    // `go_minted_token` has run its purge. This one reaches it through
    // `fixture_team_and_channel` below, which is easy to reorder away by accident, so it is also
    // taken here explicitly.
    let _ = common::go_minted_token(&common::client()).await;
    let Some(pool) = common::fixture_pool().await else {
        return false;
    };
    let Ok(group) =
        sqlx::query_scalar::<_, String>("SELECT id FROM propertygroups WHERE name = 'boards'")
            .fetch_one(&pool)
            .await
    else {
        return false;
    };
    let (team, channel) = fixture_team_and_channel().await;

    // Cleared first, not `ON CONFLICT DO NOTHING`: a row left from an interrupted run would
    // otherwise keep whatever timestamps it had and quietly change every ordering below.
    unplant().await;

    // `attrs`, `protected`, the three `permission_level` columns, `linkedfieldid`, `createdby`
    // and `updatedby` are all set on at least one row and left NULL on at least one other, so the
    // `::text` casts and the `COALESCE`s in the store's SELECT are exercised rather than assumed.
    sqlx::query(
        "INSERT INTO propertyfields
            (id, groupid, name, type, attrs, targetid, targettype, objecttype, protected,
             permissionfield, permissionvalues, permissionoptions, linkedfieldid,
             createat, updateat, deleteat, createdby, updatedby)
         VALUES
           ($1, $7, 'mmrs-channel', 'select',
            '{\"mmrs\":\"channel-attrs\",\"n\":7}'::jsonb, $9, 'channel', 'channel', true,
            'member', 'admin', 'sysadmin', NULL,
            $10 + 100000, $10 + 700000, 0, $11, $11),
           ($2, $7, 'mmrs-team', 'text', NULL, $8, 'team', 'channel', false,
            NULL, NULL, NULL, NULL,
            $10 + 200000, $10 + 950000, 0, NULL, NULL),
           ($3, $7, 'mmrs-system', 'multiselect',
            '{\"mmrs\":\"system-attrs\"}'::jsonb, '', 'system', 'channel', false,
            'none', NULL, 'member', $1,
            $10 + 300000, $10 + 800000, 0, $11, NULL),
           ($4, $7, 'mmrs-deleted', 'text', '{}'::jsonb, $9, 'channel', 'channel', false,
            NULL, NULL, NULL, NULL,
            $10 + 50000, $10 + 600000, $10 + 999000, NULL, $11),
           ($5, $7, 'mmrs-postobj', 'date', '{\"mmrs\":\"post-attrs\"}'::jsonb, $9, 'channel',
            'post', false, NULL, NULL, NULL, NULL,
            $10 + 400000, $10 + 500000, 0, $11, $11),
           ($6, $7, 'mmrs-sysobj', 'user', '{\"mmrs\":\"sysobj-attrs\"}'::jsonb, '', 'system',
            'system', false, NULL, NULL, NULL, NULL,
            $10 + 350000, $10 + 1050000, 0, NULL, NULL),
           ($12, $7, 'mmrs-dm', 'text', '{}'::jsonb, $13, 'channel', 'channel', false,
            NULL, NULL, NULL, NULL,
            $10 + 150000, $10 + 850000, 0, NULL, NULL)",
    )
    .bind(F_CHANNEL)
    .bind(F_TEAM)
    .bind(F_SYSTEM)
    .bind(F_DELETED)
    .bind(F_POSTOBJ)
    .bind(F_SYSOBJ)
    .bind(&group)
    .bind(&team)
    .bind(&channel)
    .bind(EPOCH)
    .bind(common::logged_in_user_id())
    .bind(F_DM)
    .bind(fixture_dm_channel().await)
    .execute(&pool)
    .await
    .expect("the planted property fields are written");

    // Values on the same channel target. `create_at` and `update_at` disagree here too, and the
    // soft-deleted one is again the earliest by `create_at`.
    sqlx::query(
        "INSERT INTO propertyvalues
            (id, targetid, targettype, groupid, fieldid, value, createat, updateat, deleteat,
             createdby, updatedby)
         VALUES
           ($1, $8, 'channel', $7, $4, '\"mmrs-alpha\"'::jsonb,
            $9 + 100000, $9 + 700000, 0, $10, $10),
           ($2, $8, 'channel', $7, $5, '{\"n\":42}'::jsonb,
            $9 + 200000, $9 + 600000, 0, NULL, NULL),
           ($3, $8, 'channel', $7, $6, '\"mmrs-gone\"'::jsonb,
            $9 + 50000, $9 + 500000, $9 + 999000, $10, NULL),
           ($11, $12, 'channel', $7, $5, '\"mmrs-elsewhere\"'::jsonb,
            $9 + 150000, $9 + 650000, 0, NULL, NULL),
           ($13, 'system', 'system', $7, $14, '\"mmrs-system-value\"'::jsonb,
            $9 + 300000, $9 + 750000, 0, $10, $10)",
    )
    .bind(V_ONE)
    .bind(V_TWO)
    .bind(V_DELETED)
    .bind(F_CHANNEL)
    .bind(F_SYSTEM)
    .bind(F_DELETED)
    .bind(&group)
    .bind(&channel)
    .bind(EPOCH)
    .bind(common::logged_in_user_id())
    .bind(V_OTHER_TARGET)
    .bind(fixture_dm_channel().await)
    .bind(V_SYSTEM)
    .bind(F_SYSOBJ)
    .execute(&pool)
    .await
    .expect("the planted property values are written");

    true
}

async fn unplant() {
    let Some(pool) = common::fixture_pool().await else {
        return;
    };
    let _ = sqlx::query("DELETE FROM propertyvalues WHERE id LIKE 'mmrsprop%'")
        .execute(&pool)
        .await;
    let _ = sqlx::query("DELETE FROM propertyfields WHERE id LIKE 'mmrsprop%'")
        .execute(&pool)
        .await;
}

/// The fixture team and a channel **of that team**, created for this suite and cached.
///
/// Not `a_team_and_channel_the_user_is_in`: that returns the first channel of
/// `GET /users/me/teams/{team}/channels`, which on this database is a **direct message** — and a
/// DM has no parent team, so `resolveScopeAndCheckPermissions` takes the two-level
/// `system → channel` branch instead of the three-level one. The hierarchy tests would then have
/// been asserting the wrong branch and passing. [`the_channel_scope_on_a_dm_has_no_team_level`]
/// covers that branch deliberately.
async fn fixture_team_and_channel() -> (String, String) {
    static IDS: tokio::sync::OnceCell<(String, String)> = tokio::sync::OnceCell::const_new();
    IDS.get_or_init(async || {
        let client = client();
        let token = go_minted_token(&client).await;
        let (team, _) = common::a_team_and_channel_the_user_is_in(&client, &token).await;
        let channel = common::create_channel(&client, &token, &team, "props").await;
        (team, channel)
    })
    .await
    .clone()
}

/// A direct-message channel with the fixture user in it, which has **no team**.
async fn fixture_dm_channel() -> String {
    static DM: tokio::sync::OnceCell<String> = tokio::sync::OnceCell::const_new();
    DM.get_or_init(async || {
        let client = client();
        let token = go_minted_token(&client).await;
        let me = common::logged_in_user_id();
        common::create_direct_channel(&client, &token, me, me).await
    })
    .await
    .clone()
}

/// Both servers' answers to one GET, asserted byte-identical, decoded once.
async fn both_agree(
    client: &reqwest::Client,
    token: &str,
    path: &str,
    expected_status: u16,
) -> serde_json::Value {
    let ((go_status, go_body), (rs_status, rs_body)) = fetch_both_raw(client, token, path).await;
    assert_eq!(go_status, expected_status, "{path}: Go's status");
    assert_eq!(
        rs_status,
        go_status,
        "{path}: our status must be Go's\n  go: {}\n  rs: {}",
        String::from_utf8_lossy(&go_body),
        String::from_utf8_lossy(&rs_body)
    );
    assert_eq!(
        go_body,
        rs_body,
        "{path}: byte-identical bodies\n  go: {}\n  rs: {}",
        String::from_utf8_lossy(&go_body),
        String::from_utf8_lossy(&rs_body)
    );
    serde_json::from_slice(&go_body).unwrap_or_else(|e| panic!("{path}: body is not JSON: {e}"))
}

/// The `id` of every element of a JSON array, in wire order.
fn ids(value: &serde_json::Value) -> Vec<String> {
    value
        .as_array()
        .unwrap_or_else(|| panic!("expected an array, got {value}"))
        .iter()
        .map(|row| row["id"].as_str().expect("an id").to_owned())
        .collect()
}

/// **The hierarchy, in `create_at` order, and the tombstone is not in it.**
///
/// `channel_id` expands to `system OR team=… OR channel=…` (property_field_store.go:281), and the
/// field store orders by `CreateAt ASC, Id ASC`. So the answer is the channel field, the team
/// field and the system field — in an order that is neither their id order nor their `update_at`
/// order, which is what makes the assertion mean something.
#[tokio::test]
async fn the_channel_scope_returns_the_channel_its_team_and_the_system_rows() {
    if !stack_enabled() {
        return;
    }
    let _rows = common::PROPERTY_ROWS.lock().await;
    if !plant().await {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let (_, channel) = fixture_team_and_channel().await;

    let body = both_agree(
        &client,
        &token,
        &format!("/api/v4/properties/groups/{GROUP}/channel/fields?channel_id={channel}"),
        200,
    )
    .await;

    assert_eq!(
        ids(&body),
        vec![F_CHANNEL.to_owned(), F_TEAM.to_owned(), F_SYSTEM.to_owned()],
        "channel, then team, then system — `CreateAt ASC` and nothing else"
    );
    unplant().await;
}

/// **The team scope stops one level up.** Same request minus the channel: the channel field is
/// gone and the other two remain, so the hierarchy is a real expansion rather than "everything in
/// the group".
#[tokio::test]
async fn the_team_scope_omits_the_channels_own_field() {
    if !stack_enabled() {
        return;
    }
    let _rows = common::PROPERTY_ROWS.lock().await;
    if !plant().await {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let (team, _) = fixture_team_and_channel().await;

    let body = both_agree(
        &client,
        &token,
        &format!("/api/v4/properties/groups/{GROUP}/channel/fields?team_id={team}"),
        200,
    )
    .await;

    assert_eq!(
        ids(&body),
        vec![F_TEAM.to_owned(), F_SYSTEM.to_owned()],
        "team and system only, in `CreateAt` order"
    );
    unplant().await;
}

/// **A DM has no team, so its hierarchy has two levels and not three.**
///
/// `resolveScopeAndCheckPermissions` fills `opts.TeamID` from the channel it just read, and a
/// direct message's is empty — which drops the store into the `ChannelID != "" && TeamID == ""`
/// branch: `system OR channel`, with no team clause at all. Collapsing the two branches into one
/// three-way `OR` would leak every team-scoped field into every DM, and only a channel with no
/// team can tell.
#[tokio::test]
async fn the_channel_scope_on_a_dm_has_no_team_level() {
    if !stack_enabled() {
        return;
    }
    let _rows = common::PROPERTY_ROWS.lock().await;
    if !plant().await {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let dm = fixture_dm_channel().await;

    let body = both_agree(
        &client,
        &token,
        &format!("/api/v4/properties/groups/{GROUP}/channel/fields?channel_id={dm}"),
        200,
    )
    .await;

    assert_eq!(
        ids(&body),
        vec![F_DM.to_owned(), F_SYSTEM.to_owned()],
        "the DM's own field and the system rows — the team field is not in a DM's hierarchy"
    );
    unplant().await;
}

/// **A single-target scope is not a hierarchy.** `target_type=channel&target_id=…` returns the
/// channel's own field and *not* the system row that the `channel_id` form includes — the two
/// scope shapes are the branch `resolveScopeAndCheckPermissions` refuses to let a caller mix.
#[tokio::test]
async fn a_single_target_scope_excludes_the_ancestors() {
    if !stack_enabled() {
        return;
    }
    let _rows = common::PROPERTY_ROWS.lock().await;
    if !plant().await {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let (_, channel) = fixture_team_and_channel().await;

    let body = both_agree(
        &client,
        &token,
        &format!(
            "/api/v4/properties/groups/{GROUP}/channel/fields\
             ?target_type=channel&target_id={channel}"
        ),
        200,
    )
    .await;

    assert_eq!(ids(&body), vec![F_CHANNEL.to_owned()]);
    unplant().await;
}

/// **`since` changes three things at once**: the ordering column, the cursor key, and whether
/// tombstones are returned. The deleted field is the earliest row by `create_at` and the second
/// by `update_at`, so a delta read puts it in the middle — a position neither the directory order
/// nor "append the deleted rows" would produce.
#[tokio::test]
async fn delta_mode_orders_by_update_at_and_carries_the_tombstone() {
    if !stack_enabled() {
        return;
    }
    let _rows = common::PROPERTY_ROWS.lock().await;
    if !plant().await {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let (_, channel) = fixture_team_and_channel().await;

    let body = both_agree(
        &client,
        &token,
        &format!(
            "/api/v4/properties/groups/{GROUP}/channel/fields\
             ?channel_id={channel}&since={}",
            EPOCH + 1
        ),
        200,
    )
    .await;

    assert_eq!(
        ids(&body),
        vec![
            F_DELETED.to_owned(),
            F_CHANNEL.to_owned(),
            F_SYSTEM.to_owned(),
            F_TEAM.to_owned(),
        ],
        "`UpdateAt ASC`, tombstone included — and *not* the `CreateAt` order"
    );
    unplant().await;
}

/// **The `since` boundary is inclusive.** A row updated at exactly `since` is on the first page;
/// one millisecond later it is not. `>` instead of `>=` loses a row on every delta sync, and only
/// the row sitting exactly on the boundary can tell.
#[tokio::test]
async fn the_since_boundary_includes_the_row_it_names() {
    if !stack_enabled() {
        return;
    }
    let _rows = common::PROPERTY_ROWS.lock().await;
    if !plant().await {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let (_, channel) = fixture_team_and_channel().await;

    let on_the_boundary = both_agree(
        &client,
        &token,
        &format!(
            "/api/v4/properties/groups/{GROUP}/channel/fields?channel_id={channel}&since={}",
            EPOCH + 700_000
        ),
        200,
    )
    .await;
    assert_eq!(
        ids(&on_the_boundary),
        vec![F_CHANNEL.to_owned(), F_SYSTEM.to_owned(), F_TEAM.to_owned()],
        "the row updated at exactly `since` is returned"
    );

    let past_it = both_agree(
        &client,
        &token,
        &format!(
            "/api/v4/properties/groups/{GROUP}/channel/fields?channel_id={channel}&since={}",
            EPOCH + 700_001
        ),
        200,
    )
    .await;
    assert_eq!(
        ids(&past_it),
        vec![F_SYSTEM.to_owned(), F_TEAM.to_owned()],
        "one millisecond later it is not"
    );
    unplant().await;
}

/// **The directory cursor pages on `create_at`, and `per_page` really bounds the page.**
///
/// Three rows, one at a time, following the cursor. A cursor compared against `update_at` here
/// would return them in a different order or skip one outright, because the two columns disagree
/// for every row in the fixture.
#[tokio::test]
async fn the_directory_cursor_walks_the_pages_in_create_at_order() {
    if !stack_enabled() {
        return;
    }
    let _rows = common::PROPERTY_ROWS.lock().await;
    if !plant().await {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let (_, channel) = fixture_team_and_channel().await;
    let base =
        format!("/api/v4/properties/groups/{GROUP}/channel/fields?channel_id={channel}&per_page=1");

    let first = both_agree(&client, &token, &base, 200).await;
    assert_eq!(
        ids(&first),
        vec![F_CHANNEL.to_owned()],
        "one row, the first"
    );

    let second = both_agree(
        &client,
        &token,
        &format!(
            "{base}&cursor_id={F_CHANNEL}&cursor_create_at={}",
            EPOCH + 100_000
        ),
        200,
    )
    .await;
    assert_eq!(ids(&second), vec![F_TEAM.to_owned()]);

    let third = both_agree(
        &client,
        &token,
        &format!(
            "{base}&cursor_id={F_TEAM}&cursor_create_at={}",
            EPOCH + 200_000
        ),
        200,
    )
    .await;
    assert_eq!(ids(&third), vec![F_SYSTEM.to_owned()]);

    let past_the_end = both_agree(
        &client,
        &token,
        &format!(
            "{base}&cursor_id={F_SYSTEM}&cursor_create_at={}",
            EPOCH + 300_000
        ),
        200,
    )
    .await;
    assert_eq!(ids(&past_the_end), Vec::<String>::new(), "and then `[]`");
    unplant().await;
}

/// **The delta cursor pages on `update_at`**, which orders the same three rows differently — so
/// this test and the directory one cannot both pass on one implementation of the cursor.
#[tokio::test]
async fn the_delta_cursor_walks_the_pages_in_update_at_order() {
    if !stack_enabled() {
        return;
    }
    let _rows = common::PROPERTY_ROWS.lock().await;
    if !plant().await {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let (_, channel) = fixture_team_and_channel().await;
    let base = format!(
        "/api/v4/properties/groups/{GROUP}/channel/fields\
         ?channel_id={channel}&per_page=2&since={}",
        EPOCH + 1
    );

    let first = both_agree(&client, &token, &base, 200).await;
    assert_eq!(
        ids(&first),
        vec![F_DELETED.to_owned(), F_CHANNEL.to_owned()],
        "the tombstone leads, by `UpdateAt`"
    );

    let second = both_agree(
        &client,
        &token,
        &format!(
            "{base}&cursor_id={F_CHANNEL}&cursor_update_at={}",
            EPOCH + 700_000
        ),
        200,
    )
    .await;
    assert_eq!(
        ids(&second),
        vec![F_SYSTEM.to_owned(), F_TEAM.to_owned()],
        "system before team by `UpdateAt` — the reverse of how the directory cursor pages them"
    );
    unplant().await;
}

/// **`object_types == ["system"]` erases the scope the caller sent.**
///
/// `searchPropertyFieldsCore`'s shortcut (properties.go:310) clears `channel_id` and forces
/// `target_type=system`, so a request that would otherwise be `scope_conflict` succeeds and
/// returns exactly the system-object row. Without the shortcut the same URL is a 400.
#[tokio::test]
async fn the_system_object_type_erases_the_channel_filter() {
    if !stack_enabled() {
        return;
    }
    let _rows = common::PROPERTY_ROWS.lock().await;
    if !plant().await {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let (_, channel) = fixture_team_and_channel().await;

    let bare = both_agree(
        &client,
        &token,
        &format!("/api/v4/properties/groups/{GROUP}/system/fields"),
        200,
    )
    .await;
    assert_eq!(ids(&bare), vec![F_SYSOBJ.to_owned()]);

    let with_a_channel = both_agree(
        &client,
        &token,
        &format!("/api/v4/properties/groups/{GROUP}/system/fields?channel_id={channel}"),
        200,
    )
    .await;
    assert_eq!(
        bare, with_a_channel,
        "the channel filter is erased, not applied and not refused"
    );

    // And the shortcut does not extend to a second object type: `system` mixed with `channel` is
    // the ordinary path, which needs a scope and has none here.
    let ((go_status, go_body), (rs_status, rs_body)) = post_both_raw(
        &client,
        &token,
        &format!("/api/v4/properties/groups/{GROUP}/fields/search"),
        br#"{"object_types":["system","channel"],"per_page":50}"#,
    )
    .await;
    assert_eq!(go_status, 400, "mixing types loses the shortcut");
    assert_eq!(rs_status, go_status);
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "mixed object_types");
    assert_eq!(go["id"], "api.property_field.get.scope_required.app_error");
    unplant().await;
}

/// **The POST search reaches the same rows as the GET**, and its `object_types` is a list rather
/// than the URL's single segment — so one body can ask for two object types at once, which no GET
/// can express.
#[tokio::test]
async fn the_search_route_agrees_with_the_get_route_and_takes_several_types() {
    if !stack_enabled() {
        return;
    }
    let _rows = common::PROPERTY_ROWS.lock().await;
    if !plant().await {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let (_, channel) = fixture_team_and_channel().await;
    let path = format!("/api/v4/properties/groups/{GROUP}/fields/search");

    let body = format!(r#"{{"object_types":["channel"],"channel_id":"{channel}","per_page":50}}"#);
    let ((go_status, go_body), (rs_status, rs_body)) =
        post_both_raw(&client, &token, &path, body.as_bytes()).await;
    assert_eq!(go_status, 200);
    assert_eq!(rs_status, go_status);
    assert_eq!(go_body, rs_body, "byte-identical");
    let searched: serde_json::Value = serde_json::from_slice(&go_body).expect("JSON");

    let fetched = both_agree(
        &client,
        &token,
        &format!("/api/v4/properties/groups/{GROUP}/channel/fields?channel_id={channel}"),
        200,
    )
    .await;
    assert_eq!(searched, fetched, "same opts, same rows, same bytes");

    // Two types at once: the `post` row joins, and it sorts by `CreateAt` among the others rather
    // than being appended.
    // Scoped by the channel *target*, not by `system`: the `boards` group is seeded with two
    // `post`-object rows at the system level (`assignee` and `status`, written when the group was
    // registered), and an assertion that listed them would be an assertion about the seed.
    let both_types = format!(
        r#"{{"object_types":["channel","post"],"target_type":"channel","target_id":"{channel}","per_page":50}}"#
    );
    let ((go_status, go_body), (rs_status, rs_body)) =
        post_both_raw(&client, &token, &path, both_types.as_bytes()).await;
    assert_eq!(go_status, 200);
    assert_eq!(rs_status, go_status);
    assert_eq!(go_body, rs_body);
    let mixed: serde_json::Value = serde_json::from_slice(&go_body).expect("JSON");
    assert_eq!(
        ids(&mixed),
        vec![F_CHANNEL.to_owned(), F_POSTOBJ.to_owned()],
        "both object types at the channel target, in `CreateAt` order"
    );
    unplant().await;
}

/// **An empty value list is `null` and an empty field list is `[]`.**
///
/// The two sibling stores differ by one line — `fields := []*model.PropertyField{}` against
/// `var values []*model.PropertyValue` — and nothing downstream normalises either. A port that
/// returned a `Vec` from both would be wrong on exactly one of these, on every empty read.
#[tokio::test]
async fn the_two_empty_answers_are_not_the_same_shape() {
    if !stack_enabled() {
        return;
    }
    let _rows = common::PROPERTY_ROWS.lock().await;
    if !plant().await {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;

    let ((_, go_fields), (_, rs_fields)) = fetch_both_raw(
        &client,
        &token,
        &format!("/api/v4/properties/groups/{GROUP}/user/fields?target_type=system"),
    )
    .await;
    assert_eq!(go_fields, rs_fields);
    assert_eq!(String::from_utf8_lossy(&go_fields), "[]\n");

    // `post_attributes`, not `boards`: the fixture plants a system-target value in `boards` so
    // that the dedicated route has something to filter *for*, and an empty answer has to come
    // from a group that really is empty.
    let ((_, go_values), (_, rs_values)) = fetch_both_raw(
        &client,
        &token,
        "/api/v4/properties/groups/post_attributes/system/values",
    )
    .await;
    assert_eq!(go_values, rs_values);
    assert_eq!(String::from_utf8_lossy(&go_values), "null\n");
    unplant().await;
}

/// **The value reads, with rows in them.** Directory order, the tombstone skipped; then delta
/// mode, which flips the order and brings it back.
#[tokio::test]
async fn the_channel_values_page_in_create_at_order_and_delta_flips_them() {
    if !stack_enabled() {
        return;
    }
    let _rows = common::PROPERTY_ROWS.lock().await;
    if !plant().await {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let (_, channel) = fixture_team_and_channel().await;

    let directory = both_agree(
        &client,
        &token,
        &format!("/api/v4/properties/groups/{GROUP}/channel/values/{channel}"),
        200,
    )
    .await;
    assert_eq!(
        ids(&directory),
        vec![V_ONE.to_owned(), V_TWO.to_owned()],
        "`CreateAt ASC`, and the soft-deleted row is not here"
    );

    let delta = both_agree(
        &client,
        &token,
        &format!(
            "/api/v4/properties/groups/{GROUP}/channel/values/{channel}?since={}",
            EPOCH + 1
        ),
        200,
    )
    .await;
    assert_eq!(
        ids(&delta),
        vec![V_DELETED.to_owned(), V_TWO.to_owned(), V_ONE.to_owned()],
        "`UpdateAt ASC`, tombstone included — the reverse of the directory order"
    );
    unplant().await;
}

/// **The value search filters on the target, and the two value routes read different ones.**
///
/// Three values sit on the fixture channel, one on a DM and one at the system target — so the
/// channel route must return exactly the channel's, and `…/system/values` exactly the system one.
/// Without a value on a second target, dropping the `TargetID` predicate changes nothing and the
/// mutation that drops it survives; it did, on the first run of this plan.
#[tokio::test]
async fn each_value_route_reads_only_its_own_target() {
    if !stack_enabled() {
        return;
    }
    let _rows = common::PROPERTY_ROWS.lock().await;
    if !plant().await {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let (_, channel) = fixture_team_and_channel().await;
    let dm = fixture_dm_channel().await;

    let here = both_agree(
        &client,
        &token,
        &format!("/api/v4/properties/groups/{GROUP}/channel/values/{channel}"),
        200,
    )
    .await;
    assert_eq!(ids(&here), vec![V_ONE.to_owned(), V_TWO.to_owned()]);

    let elsewhere = both_agree(
        &client,
        &token,
        &format!("/api/v4/properties/groups/{GROUP}/channel/values/{dm}"),
        200,
    )
    .await;
    assert_eq!(
        ids(&elsewhere),
        vec![V_OTHER_TARGET.to_owned()],
        "the DM's own value, and none of the other channel's"
    );

    let system = both_agree(
        &client,
        &token,
        &format!("/api/v4/properties/groups/{GROUP}/system/values"),
        200,
    )
    .await;
    assert_eq!(
        ids(&system),
        vec![V_SYSTEM.to_owned()],
        "the dedicated route reads `TargetID = \"system\"`, which is a target id like any other"
    );

    // **A `user` target is an ordinary 200**, and this is the only probe of that object type in
    // the file. Without it, swapping the handler's `template` guard for `user` survives the whole
    // suite: the template 400 still comes back, one layer down, from `hasTargetAccess`'s own
    // template arm — same id, same status, a different function. Measured survivor, 2026-09-12.
    //
    // A sysadmin sees any user, so an id that names nobody still reaches the search and answers
    // `null` rather than a permission error.
    let ((go_status, go_body), (rs_status, rs_body)) = fetch_both_raw(
        &client,
        &token,
        &format!("/api/v4/properties/groups/{GROUP}/user/values/zzzzzzzzzzzzzzzzzzzzzzzzzz"),
    )
    .await;
    assert_eq!(go_status, 200, "a user target is not a refusal");
    assert_eq!(rs_status, go_status);
    assert_eq!(go_body, rs_body);
    assert_eq!(String::from_utf8_lossy(&go_body), "null\n");
    unplant().await;
}

/// **`per_page=0` is a 500 on the GET route and a 60-row default on the POST one.**
///
/// `web.ParamsFromRequest` clamps negatives and the maximum but lets a literal zero through, and
/// the store's `PerPage < 1` guard is what a GET then hits. `searchPropertyFields` clamps `<= 0`
/// itself before the store sees it. Same store, two endpoints, two answers.
#[tokio::test]
async fn per_page_zero_is_a_five_hundred_on_one_route_and_a_default_on_the_other() {
    if !stack_enabled() {
        return;
    }
    let _rows = common::PROPERTY_ROWS.lock().await;
    if !plant().await {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let (_, channel) = fixture_team_and_channel().await;

    let ((go_status, go_body), (rs_status, rs_body)) = fetch_both_raw(
        &client,
        &token,
        &format!(
            "/api/v4/properties/groups/{GROUP}/channel/fields?channel_id={channel}&per_page=0"
        ),
    )
    .await;
    assert_eq!(go_status, 500, "the GET route reaches the store's guard");
    assert_eq!(rs_status, go_status);
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "per_page=0");
    assert_eq!(go["id"], "app.property_field.search.app_error");

    let body = format!(r#"{{"object_types":["channel"],"channel_id":"{channel}","per_page":0}}"#);
    let ((go_status, go_body), (rs_status, rs_body)) = post_both_raw(
        &client,
        &token,
        &format!("/api/v4/properties/groups/{GROUP}/fields/search"),
        body.as_bytes(),
    )
    .await;
    assert_eq!(go_status, 200, "the POST route clamps it first");
    assert_eq!(rs_status, go_status);
    assert_eq!(go_body, rs_body);
    unplant().await;
}

/// Every refusal these four routes can mint, against Go, in one table.
///
/// The pairs that look redundant are not: `Boards` and `XY` are **mux** 404s, from a path segment
/// outside the gorilla pattern, while `xyz` reaches the handler for a 400 — three different
/// answers to what looks like one mistake. And a malformed cursor is `invalid_body_param` on the
/// field route and `api.property_value.get.invalid_opts.app_error` on the value route, because
/// only the first calls `cur.IsValid()`.
#[tokio::test]
async fn the_refusals_agree_including_the_ones_go_answers_from_its_mux() {
    if !stack_enabled() {
        return;
    }
    let _rows = common::PROPERTY_ROWS.lock().await;
    if !plant().await {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    const NOWHERE: &str = "zzzzzzzzzzzzzzzzzzzzzzzzzz";

    for (path, status, id) in [
        // getV2Group, in Go's order.
        (
            "/api/v4/properties/groups/nosuchgroup/system/fields".to_owned(),
            404,
            "app.property_group.get.app_error",
        ),
        (
            "/api/v4/properties/groups/content_flagging/system/fields".to_owned(),
            404,
            "api.property.v2_group_not_found.app_error",
        ),
        (
            "/api/v4/properties/groups/managed_channel_categories/system/fields".to_owned(),
            404,
            "api.property.v2_group_not_found.app_error",
        ),
        (
            "/api/v4/properties/groups/session_attributes/system/fields".to_owned(),
            501,
            "api.property.session_attributes.license.app_error",
        ),
        // The URL parameters.
        (
            format!("/api/v4/properties/groups/{GROUP}/xyz/fields?target_type=system"),
            400,
            "api.context.invalid_url_param.app_error",
        ),
        // Scope resolution.
        (
            format!("/api/v4/properties/groups/{GROUP}/channel/fields"),
            400,
            "api.property_field.get.scope_required.app_error",
        ),
        (
            format!(
                "/api/v4/properties/groups/{GROUP}/channel/fields\
                 ?channel_id={NOWHERE}&target_type=channel"
            ),
            400,
            "api.property_field.get.scope_conflict.app_error",
        ),
        (
            format!("/api/v4/properties/groups/{GROUP}/channel/fields?target_id={NOWHERE}"),
            400,
            "api.property_field.get.target_type_required.app_error",
        ),
        (
            format!("/api/v4/properties/groups/{GROUP}/channel/fields?target_type=bogus"),
            400,
            "api.property_field.get.invalid_target_type.app_error",
        ),
        (
            format!("/api/v4/properties/groups/{GROUP}/channel/fields?target_type=channel"),
            400,
            "api.property_field.get.target_id_required.app_error",
        ),
        (
            format!("/api/v4/properties/groups/{GROUP}/channel/fields?target_type=team"),
            400,
            "api.property_field.get.target_id_required.app_error",
        ),
        // A channel and a team the session cannot see.
        (
            format!("/api/v4/properties/groups/{GROUP}/channel/fields?channel_id={NOWHERE}"),
            403,
            "api.context.permissions.app_error",
        ),
        // Query-string parsing.
        (
            format!("/api/v4/properties/groups/{GROUP}/channel/fields?target_type=system&since=xx"),
            400,
            "api.context.invalid_body_param.app_error",
        ),
        (
            format!(
                "/api/v4/properties/groups/{GROUP}/channel/fields?target_type=system&cursor_id=zzz"
            ),
            400,
            "api.context.invalid_body_param.app_error",
        ),
        // The value routes' three pre-group refusals.
        (
            format!("/api/v4/properties/groups/{GROUP}/template/values/{NOWHERE}"),
            400,
            "api.property_value.template_no_values.app_error",
        ),
        // **The template guard runs before `RequireTargetId`**, so a target id that is not an id
        // at all still gets the template refusal rather than `invalid_url_param`. Reversing the
        // two is invisible with a well-formed id, which is what the first mutation run found.
        (
            format!("/api/v4/properties/groups/{GROUP}/template/values/notanid"),
            400,
            "api.property_value.template_no_values.app_error",
        ),
        // And the same guard runs before the group is read, so a group that does not exist is
        // still the template 400 and not a 404.
        (
            format!("/api/v4/properties/groups/nosuchgroup/template/values/{NOWHERE}"),
            400,
            "api.property_value.template_no_values.app_error",
        ),
        (
            format!("/api/v4/properties/groups/{GROUP}/system/values/{NOWHERE}"),
            400,
            "api.property_value.system_use_dedicated_route.app_error",
        ),
        (
            format!("/api/v4/properties/groups/{GROUP}/channel/values/{NOWHERE}"),
            403,
            "api.context.permissions.app_error",
        ),
        // The cursor the value route does not validate up front — same mistake, a different id.
        (
            format!("/api/v4/properties/groups/{GROUP}/system/values?cursor_id=zzz"),
            400,
            "api.property_value.get.invalid_opts.app_error",
        ),
    ] {
        let ((go_status, go_body), (rs_status, rs_body)) =
            fetch_both_raw(&client, &token, &path).await;
        assert_eq!(go_status, status, "{path}: Go's status");
        assert_eq!(rs_status, go_status, "{path}: our status");
        let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &path);
        assert_eq!(go["id"], id, "{path}: the error id");
    }
    unplant().await;
}

/// **A segment outside Go's mux pattern is answered by Go**, not by us: `{group_name}` is
/// `[a-z][a-z0-9_]*` and `{object_type}` is `[a-z]+`, so an upper-case segment never reaches a
/// handler and gorilla writes its own 404 with the request URL interpolated into
/// `detailed_error`. We forward so those bytes are Go's.
#[tokio::test]
async fn a_segment_outside_the_mux_pattern_is_forwarded() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;

    for path in [
        "/api/v4/properties/groups/Boards/system/fields",
        "/api/v4/properties/groups/_boards/system/fields",
        "/api/v4/properties/groups/9boards/system/fields",
        "/api/v4/properties/groups/boards/XY/fields",
        "/api/v4/properties/groups/boards/sys1/fields",
    ] {
        let go = client
            .get(format!("{GO}{path}"))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .expect("Go answers");
        let go_status = go.status().as_u16();
        let go_body = go.bytes().await.expect("body").to_vec();

        let rs = client
            .get(format!("{RUST}{path}"))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .expect("we answer");
        let rs_status = rs.status().as_u16();
        assert_eq!(
            rs.headers()
                .get("x-mmrs-served-by")
                .and_then(|v| v.to_str().ok()),
            Some("go"),
            "{path}: must be forwarded, not served"
        );
        let rs_body = rs.bytes().await.expect("body").to_vec();

        assert_eq!(go_status, 404, "{path}: Go's mux 404");
        assert_eq!(rs_status, go_status);
        let go: serde_json::Value = serde_json::from_slice(&go_body).expect("JSON");
        let rs: serde_json::Value = serde_json::from_slice(&rs_body).expect("JSON");
        assert_eq!(go["id"], "api.context.404.app_error", "{path}");
        assert_eq!(rs["id"], go["id"]);
        assert_eq!(
            rs["detailed_error"], go["detailed_error"],
            "{path}: the interpolated URL is Go's own"
        );
    }
}

/// **`object_types` is validated on the POST route and only there.** An empty list, a missing key
/// and a bogus entry are all `invalid_body_param` naming `object_types`; the GET route has no
/// equivalent because its one value comes from the URL and `RequireObjectType` has already
/// checked it.
#[tokio::test]
async fn the_search_route_validates_its_object_types() {
    if !stack_enabled() {
        return;
    }
    let _rows = common::PROPERTY_ROWS.lock().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let path = format!("/api/v4/properties/groups/{GROUP}/fields/search");

    for body in [
        &br#"{"per_page":50}"#[..],
        &br#"{"object_types":[],"per_page":50}"#[..],
        &br#"{"object_types":["bogus"],"per_page":50}"#[..],
        &br#"{"object_types":["channel","bogus"],"per_page":50}"#[..],
        &br#"null"#[..],
    ] {
        let ((go_status, go_body), (rs_status, rs_body)) =
            post_both_raw(&client, &token, &path, body).await;
        let context = format!("{path} {}", String::from_utf8_lossy(body));
        assert_eq!(go_status, 400, "{context}: Go's status");
        assert_eq!(rs_status, go_status, "{context}: our status");
        let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &context);
        assert_eq!(go["id"], "api.context.invalid_body_param.app_error");
    }
}

// ---------------------------------------------------------------------------------------------
// `deletePropertyField` — the first of the five writes in `api4/properties.go`.
//
// Destructive, so the two servers delete **different** rows of the same shape rather than racing
// for one. The refusals are non-destructive and share a row.
// ---------------------------------------------------------------------------------------------

/// Two equivalent live fields, one for each server to delete, each carrying a value.
///
/// **Every id here is exactly 26 characters.** A field id shorter than that is a `RequireFieldId`
/// 400 before any handler logic runs, which turns a test of the 409 into a test of the parser —
/// it did, once, and the failure named the wrong thing.
const D_GO: &str = "mmrsdelgo00000000000000001";
const D_RS: &str = "mmrsdelrs00000000000000001";
/// `Protected` — no session may delete it, not even an unrestricted one.
const D_PROTECTED: &str = "mmrsdelprot000000000000001";
/// `PermissionField IS NULL` — a legacy (PSAv1-shaped) row with no permission model.
const D_NO_PERMISSION: &str = "mmrsdelnoperm0000000000001";
/// The source of a link, and the dependent that makes deleting it a 409.
const D_LINK_SOURCE: &str = "mmrsdellink000000000000001";
const D_LINK_DEPENDENT: &str = "mmrsdeldep0000000000000001";
const D_VALUE_GO: &str = "mmrsdelvaluego000000000001";
const D_VALUE_RS: &str = "mmrsdelvaluers000000000001";
/// A second link pair, so **each** server can walk the whole unlink-then-delete sequence itself.
const D_LINK_SOURCE_GO: &str = "mmrsdellinkgo0000000000001";
const D_LINK_DEPENDENT_GO: &str = "mmrsdeldepgo00000000000001";
/// Two values the delete must **not** touch: one in this group on a field nobody deletes, and one
/// in another group entirely. Without them a cascade with no `FieldID` predicate and a cascade
/// with no `GroupID` predicate are both invisible — measured, 2026-09-12, two survivors.
const D_VALUE_BYSTANDER: &str = "mmrsdelvaluebystander00001";
const D_VALUE_OTHER_GROUP: &str = "mmrsdelvalueothergroup0001";
/// The field that other-group value hangs off, in `post_attributes`.
const D_FIELD_OTHER_GROUP: &str = "mmrsdelfieldothergroup0001";

/// Plant the delete fixture. Cleared first rather than `ON CONFLICT DO NOTHING`, because a row
/// left soft-deleted by an interrupted run would make every assertion below vacuous.
async fn plant_delete_fixture() -> bool {
    // **The token first, and it is not optional.** `purge_api_fixtures` runs inside
    // `go_minted_token`'s `OnceCell` — deliberately, so that no fixture is built before the sweep
    // — and it deletes every `mmrsdel%` row. A test that plants and *then* asks for a token
    // therefore wipes its own fixture whenever it is the first in the binary to need one, which
    // is order-dependent and so does not fail every run. It failed this one: `the_delete_refusals
    // _agree` saw **Go** answer 404 for a protected field, an answer no change to this port could
    // produce. Minting here forces the sweep to have happened before the first `INSERT`.
    let _ = common::go_minted_token(&common::client()).await;
    let Some(pool) = common::fixture_pool().await else {
        return false;
    };
    let Ok(group) =
        sqlx::query_scalar::<_, String>("SELECT id FROM propertygroups WHERE name = 'boards'")
            .fetch_one(&pool)
            .await
    else {
        return false;
    };
    let Ok(other_group) = sqlx::query_scalar::<_, String>(
        "SELECT id FROM propertygroups WHERE name = 'post_attributes'",
    )
    .fetch_one(&pool)
    .await
    else {
        return false;
    };
    unplant_delete_fixture().await;

    sqlx::query(
        "INSERT INTO propertyfields
            (id, groupid, name, type, attrs, targetid, targettype, objecttype, protected,
             permissionfield, permissionvalues, permissionoptions, linkedfieldid,
             createat, updateat, deleteat)
         VALUES
           ($1, $7, 'mmrs-del-go',     'text', '{}'::jsonb, '', 'system', 'channel', false,
            'member', 'member', 'member', NULL, $8, $8, 0),
           ($2, $7, 'mmrs-del-rs',     'text', '{}'::jsonb, '', 'system', 'channel', false,
            'member', 'member', 'member', NULL, $8, $8, 0),
           ($3, $7, 'mmrs-del-prot',   'text', '{}'::jsonb, '', 'system', 'channel', true,
            'member', 'member', 'member', NULL, $8, $8, 0),
           ($4, $7, 'mmrs-del-noperm', 'text', '{}'::jsonb, '', 'system', 'channel', false,
            NULL, NULL, NULL, NULL, $8, $8, 0),
           ($5, $7, 'mmrs-del-link',   'text', '{}'::jsonb, '', 'system', 'channel', false,
            'member', 'member', 'member', NULL, $8, $8, 0),
           ($6, $7, 'mmrs-del-dep',    'text', '{}'::jsonb, '', 'system', 'channel', false,
            'member', 'member', 'member', $5, $8, $8, 0),
           ($9, $7, 'mmrs-del-link-go', 'text', '{}'::jsonb, '', 'system', 'channel', false,
            'member', 'member', 'member', NULL, $8, $8, 0),
           ($10, $7, 'mmrs-del-dep-go', 'text', '{}'::jsonb, '', 'system', 'channel', false,
            'member', 'member', 'member', $9, $8, $8, 0),
           ($11, $12, 'mmrs-del-other', 'text', '{}'::jsonb, '', 'system', 'channel', false,
            'member', 'member', 'member', NULL, $8, $8, 0)",
    )
    .bind(D_GO)
    .bind(D_RS)
    .bind(D_PROTECTED)
    .bind(D_NO_PERMISSION)
    .bind(D_LINK_SOURCE)
    .bind(D_LINK_DEPENDENT)
    .bind(&group)
    .bind(EPOCH)
    .bind(D_LINK_SOURCE_GO)
    .bind(D_LINK_DEPENDENT_GO)
    .bind(D_FIELD_OTHER_GROUP)
    .bind(&other_group)
    .execute(&pool)
    .await
    .expect("the delete fixture's fields are written");

    sqlx::query(
        "INSERT INTO propertyvalues
            (id, targetid, targettype, groupid, fieldid, value, createat, updateat, deleteat)
         VALUES ($1, 'system', 'system', $3, $5, '\"go\"'::jsonb,        $6, $6, 0),
                ($2, 'system', 'system', $3, $4, '\"rust\"'::jsonb,      $6, $6, 0),
                ($7, 'system', 'system', $3, $8, '\"bystander\"'::jsonb, $6, $6, 0),
                ($9, 'system', 'system', $10, $11, '\"other\"'::jsonb,   $6, $6, 0)",
    )
    .bind(D_VALUE_GO)
    .bind(D_VALUE_RS)
    .bind(&group)
    .bind(D_RS)
    .bind(D_GO)
    .bind(EPOCH)
    .bind(D_VALUE_BYSTANDER)
    .bind(D_PROTECTED)
    .bind(D_VALUE_OTHER_GROUP)
    .bind(&other_group)
    .bind(D_FIELD_OTHER_GROUP)
    .execute(&pool)
    .await
    .expect("the delete fixture's values are written");

    true
}

async fn unplant_delete_fixture() {
    let Some(pool) = common::fixture_pool().await else {
        return;
    };
    let _ = sqlx::query("DELETE FROM propertyvalues WHERE id LIKE 'mmrsdel%'")
        .execute(&pool)
        .await;
    let _ = sqlx::query("DELETE FROM propertyfields WHERE id LIKE 'mmrsdel%'")
        .execute(&pool)
        .await;
}

/// `(delete_at > 0)` for one `PropertyFields` or `PropertyValues` row, or `None` if it is gone.
async fn is_soft_deleted(table: &str, id: &str) -> Option<bool> {
    let pool = common::fixture_pool().await?;
    let query = format!("SELECT deleteat > 0 FROM {table} WHERE id = $1");
    sqlx::query_scalar::<_, bool>(&query)
        .bind(id)
        .fetch_optional(&pool)
        .await
        .ok()
        .flatten()
}

/// One DELETE to one server.
async fn delete_one(
    client: &reqwest::Client,
    token: &str,
    base: &str,
    path: &str,
) -> (u16, Vec<u8>) {
    let response = client
        .delete(format!("{base}{path}"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .unwrap_or_else(|e| panic!("{base}{path} is unreachable: {e}"));
    let status = response.status().as_u16();
    if base == RUST {
        common::assert_served_by_rust(response.headers(), path);
    }
    (status, response.bytes().await.expect("body reads").to_vec())
}

/// **A delete is a soft delete, and it cascades to the field's values.**
///
/// Each server deletes its own row of an identical pair, so this is the same operation on
/// equivalent rows rather than a second write to one. What is asserted is the answer *and* the
/// two `DeleteAt` columns: a port that deleted the field and forgot `DeleteForField` would answer
/// identically and leave orphaned values that every delta read would keep returning.
///
/// The body is `{"status":"OK"}` with **no trailing newline** — `ReturnStatusOK` is a bare
/// `w.Write`, unlike every other route in this file. [D-086].
#[tokio::test]
async fn a_field_delete_soft_deletes_the_field_and_cascades_its_values() {
    if !stack_enabled() {
        return;
    }
    let _rows = common::PROPERTY_ROWS.lock().await;
    if !plant_delete_fixture().await {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;

    let (go_status, go_body) = delete_one(
        &client,
        &token,
        GO,
        &format!("/api/v4/properties/groups/{GROUP}/channel/fields/{D_GO}"),
    )
    .await;
    let (rs_status, rs_body) = delete_one(
        &client,
        &token,
        RUST,
        &format!("/api/v4/properties/groups/{GROUP}/channel/fields/{D_RS}"),
    )
    .await;

    assert_eq!(go_status, 200, "Go deletes its field");
    assert_eq!(rs_status, go_status);
    assert_eq!(go_body, rs_body, "byte-identical success bodies");
    assert_eq!(
        String::from_utf8_lossy(&go_body),
        r#"{"status":"OK"}"#,
        "ReturnStatusOK writes no trailing newline"
    );

    assert_eq!(
        is_soft_deleted("propertyfields", D_GO).await,
        Some(true),
        "Go soft-deletes rather than removing the row"
    );
    assert_eq!(
        is_soft_deleted("propertyfields", D_RS).await,
        Some(true),
        "and so do we"
    );
    assert_eq!(
        is_soft_deleted("propertyvalues", D_VALUE_GO).await,
        Some(true),
        "Go cascades to the field's values"
    );
    assert_eq!(
        is_soft_deleted("propertyvalues", D_VALUE_RS).await,
        Some(true),
        "and so do we — `DeleteForField` runs before the field delete"
    );

    // **And two values it must not touch.** `DeleteForField` filters on `FieldID` *and*
    // `GroupID`, and neither predicate is observable without a row on the other side of it: the
    // bystander shares this group and belongs to a field nobody deleted, the other-group value
    // shares nothing. Both mutations survived the whole suite before these rows existed.
    assert_eq!(
        is_soft_deleted("propertyvalues", D_VALUE_BYSTANDER).await,
        Some(false),
        "a value on another field in the same group is untouched"
    );
    assert_eq!(
        is_soft_deleted("propertyvalues", D_VALUE_OTHER_GROUP).await,
        Some(false),
        "and a value in another group is untouched"
    );

    // The deleted field is gone from the directory read on both servers.
    let listed = both_agree(
        &client,
        &token,
        &format!("/api/v4/properties/groups/{GROUP}/channel/fields?target_type=system"),
        200,
    )
    .await;
    let ids = ids(&listed);
    assert!(!ids.iter().any(|id| id == D_GO || id == D_RS));

    unplant_delete_fixture().await;
}

/// Every refusal the delete route can mint, in Go's order.
///
/// Three of the five are **404s that mean different things** — a group that does not exist, a
/// field that is not in this group, and a field whose object type does not match the URL — and
/// the last of those is a 404 rather than a 400 on purpose: Go's comment says it lets fields be
/// bucketed by URL without leaking cross-bucket existence.
///
/// The two 403s are the ones worth reading twice. A **protected** field and a field with a `NULL`
/// `PermissionField` both answer `api.property_field.delete.no_permission.app_error` — the
/// handler's own id, not the generic `api.context.permissions.app_error` every other route in
/// this file uses. And because `SessionHasPermissionToEditPropertyField` refuses a protected
/// field before the app layer is reached, `DeletePropertyField`'s own protected 403
/// (`app.property_field.delete.protected.app_error`) is **unreachable from this route** — it
/// exists for plugin and internal callers. Measured: a sysadmin deleting a protected field gets
/// the handler's id on both servers.
#[tokio::test]
async fn the_delete_refusals_agree() {
    if !stack_enabled() {
        return;
    }
    let _rows = common::PROPERTY_ROWS.lock().await;
    if !plant_delete_fixture().await {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    const NOWHERE: &str = "zzzzzzzzzzzzzzzzzzzzzzzzzz";

    for (path, status, id) in [
        (
            format!("/api/v4/properties/groups/{GROUP}/channel/fields/{D_PROTECTED}"),
            403,
            "api.property_field.delete.no_permission.app_error",
        ),
        (
            format!("/api/v4/properties/groups/{GROUP}/channel/fields/{D_NO_PERMISSION}"),
            403,
            "api.property_field.delete.no_permission.app_error",
        ),
        (
            format!("/api/v4/properties/groups/{GROUP}/channel/fields/{D_LINK_SOURCE}"),
            409,
            "app.property_field.delete.has_linked_dependents.app_error",
        ),
        (
            format!("/api/v4/properties/groups/{GROUP}/post/fields/{D_LINK_SOURCE}"),
            404,
            "api.property_field.object_type_mismatch.app_error",
        ),
        (
            format!("/api/v4/properties/groups/{GROUP}/channel/fields/{NOWHERE}"),
            404,
            "app.property.not_found.app_error",
        ),
        // The same field id, asked for through a **different** group: the group is part of the
        // key, so this is a 404 and not a delete.
        (
            format!("/api/v4/properties/groups/post_attributes/channel/fields/{D_LINK_SOURCE}"),
            404,
            "app.property.not_found.app_error",
        ),
        (
            format!("/api/v4/properties/groups/nosuchgroup/channel/fields/{D_LINK_SOURCE}"),
            404,
            "app.property_group.get.app_error",
        ),
        (
            format!("/api/v4/properties/groups/{GROUP}/channel/fields/notanid"),
            400,
            "api.context.invalid_url_param.app_error",
        ),
    ] {
        let (go_status, go_body) = delete_one(&client, &token, GO, &path).await;
        let (rs_status, rs_body) = delete_one(&client, &token, RUST, &path).await;
        assert_eq!(go_status, status, "{path}: Go's status");
        assert_eq!(rs_status, go_status, "{path}: our status");
        let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &path);
        assert_eq!(go["id"], id, "{path}: the error id");
    }

    // Nothing above wrote: every refusal fires before the delete.
    for id in [
        D_PROTECTED,
        D_NO_PERMISSION,
        D_LINK_SOURCE,
        D_LINK_DEPENDENT,
    ] {
        assert_eq!(
            is_soft_deleted("propertyfields", id).await,
            Some(false),
            "{id} must still be live after a refusal"
        );
    }

    unplant_delete_fixture().await;
}

/// **Deleting the dependent first makes the source deletable**, which is the whole point of the
/// 409 being a conflict rather than a flat refusal: it names a state the caller can change.
///
/// `CountLinkedFields` counts only rows with `DeleteAt = 0`, so a soft-deleted dependent stops
/// blocking. Counting every row would make the refusal permanent.
#[tokio::test]
async fn the_linked_dependent_refusal_lifts_once_the_dependent_is_gone() {
    if !stack_enabled() {
        return;
    }
    let _rows = common::PROPERTY_ROWS.lock().await;
    if !plant_delete_fixture().await {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;

    // **Each server walks the whole sequence on its own pair.** An earlier version deleted the
    // dependent through our route and then asked *Go* to delete the source, so our own
    // `DeleteAt = 0` count was never exercised on the lifted case and a mutation that counted
    // tombstones too survived the suite.
    for (base, source, dependent) in [
        (GO, D_LINK_SOURCE_GO, D_LINK_DEPENDENT_GO),
        (RUST, D_LINK_SOURCE, D_LINK_DEPENDENT),
    ] {
        let source_path = format!("/api/v4/properties/groups/{GROUP}/channel/fields/{source}");
        let (status, _) = delete_one(&client, &token, base, &source_path).await;
        assert_eq!(status, 409, "{base}: blocked while the dependent is live");

        let (status, _) = delete_one(
            &client,
            &token,
            base,
            &format!("/api/v4/properties/groups/{GROUP}/channel/fields/{dependent}"),
        )
        .await;
        assert_eq!(status, 200, "{base}: the dependent itself deletes");

        let (status, _) = delete_one(&client, &token, base, &source_path).await;
        assert_eq!(
            status, 200,
            "{base}: the source is deletable once the dependent is a tombstone — the count is \
             `DeleteAt = 0` only"
        );
        assert_eq!(is_soft_deleted("propertyfields", source).await, Some(true));
    }

    unplant_delete_fixture().await;
}

/// **The `property_field_deleted` event, on both servers' sockets.**
///
/// The payload is `field_id` and `object_type` — *not* the field itself, unlike the create and
/// update events, which carry the whole encoded row. The fixture's fields are **system**-target,
/// which `propertyFieldBroadcastParams` maps to a broadcast with neither a team nor a channel, so
/// a fresh connection with no presence set receives it.
#[tokio::test]
async fn the_delete_publishes_the_same_websocket_event_on_both_servers() {
    if !stack_enabled() {
        return;
    }
    let _rows = common::PROPERTY_ROWS.lock().await;
    let _stream = common::BROADCAST_STREAM.lock().await;
    if !plant_delete_fixture().await {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;

    let mut go_socket = common::SocketProbe::connect(GO, &token).await;
    let mut rust_socket = common::SocketProbe::connect(RUST, &token).await;

    let (go_status, _) = delete_one(
        &client,
        &token,
        GO,
        &format!("/api/v4/properties/groups/{GROUP}/channel/fields/{D_GO}"),
    )
    .await;
    let (rs_status, _) = delete_one(
        &client,
        &token,
        RUST,
        &format!("/api/v4/properties/groups/{GROUP}/channel/fields/{D_RS}"),
    )
    .await;
    assert_eq!(go_status, 200);
    assert_eq!(rs_status, 200);

    let window = std::time::Duration::from_secs(3);
    let carries = |id: &'static str| {
        move |frames: &[serde_json::Value]| {
            frames
                .iter()
                .any(|f| f["event"] == "property_field_deleted" && f["data"]["field_id"] == id)
        }
    };
    assert!(
        go_socket.collect_until(window, carries(D_GO)).await,
        "Go published no property_field_deleted within the window"
    );
    assert!(
        rust_socket.collect_until(window, carries(D_RS)).await,
        "we published no property_field_deleted within the window"
    );

    let of = |probe: &common::SocketProbe, id: &str| -> serde_json::Value {
        let mut found: Vec<serde_json::Value> = probe
            .events_named("property_field_deleted")
            .into_iter()
            .filter(|f| f["data"]["field_id"] == id)
            .collect();
        assert_eq!(
            found.len(),
            1,
            "exactly one property_field_deleted for {id}, got {found:?}"
        );
        found.remove(0)
    };
    let go_event = of(&go_socket, D_GO);
    let rust_event = of(&rust_socket, D_RS);

    assert_eq!(
        go_event["data"]["object_type"], "channel",
        "Go carries the field's object type"
    );
    assert_eq!(
        rust_event["data"]["object_type"], go_event["data"]["object_type"],
        "and so do we"
    );
    assert_eq!(
        go_event["data"].as_object().map(|o| {
            let mut keys: Vec<&String> = o.keys().collect();
            keys.sort();
            keys.into_iter().cloned().collect::<Vec<_>>()
        }),
        rust_event["data"].as_object().map(|o| {
            let mut keys: Vec<&String> = o.keys().collect();
            keys.sort();
            keys.into_iter().cloned().collect::<Vec<_>>()
        }),
        "the same payload keys — `field_id` and `object_type`, and not the field itself"
    );
    assert_eq!(
        go_event["broadcast"], rust_event["broadcast"],
        "a system-target field broadcasts unscoped on both sides"
    );

    unplant_delete_fixture().await;
}
