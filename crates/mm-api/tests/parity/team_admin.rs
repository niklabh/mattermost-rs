//! Cross-server parity for the five team-administration routes —
//! `GET /api/v4/teams/{team_id}/members_minus_group_members`, `PUT …/scheme`,
//! `POST …/invite/email`, `POST …/invite-guests/email` and `POST …/import`.
//!
//! ```sh
//! scripts/parity.sh -p mm-api --test parity team_admin
//! ```
//!
//! # Every fixture here is this suite's own
//!
//! `importTeam` and both invite routes mutate team membership, and the seeded team is read by two
//! dozen suites in the same binary. So this suite creates its own teams (`tadmgrp`, `tadmout`,
//! `tadmdoom`), its own users (`tga`…`tgg`) and its own groups (`mmrstadmgrp…`), and touches
//! nothing it did not write.
//!
//! # What this suite can and cannot prove
//!
//! Three of the five routes hand over: `scheme` and `invite-guests` on a licensed server,
//! `invite/email` for every request that would send mail, and `import` for `importFrom=slack`.
//! [`every_answer_this_server_gives_on_the_two_write_routes_is_a_refusal`] is the proof that the
//! hand-over precedes any write — not by watching for a write, but by showing that **no input
//! reaches a 2xx from Rust at all** on either route. A handler that cannot answer success cannot
//! have written anything.

use crate::common;

use common::{
    ACTIVE_LICENCE_ROW, GO, RUST, assert_error_bodies_match_except_known_gaps, client,
    create_plain_user, create_team, go_minted_token, set_active_licence_id, stack_enabled,
};

/// A 26-character id that is a valid `IsValidId` but names no team.
const ABSENT: &str = "mmrstadmin0000000000000001";
/// The scheme id the body gate accepts. Nothing reads it — the 501 lands first.
const SCHEME: &str = "mmrstadminscheme0000000001";

const GROUP_ONE: &str = "mmrstadmgrp000000000000001";
const GROUP_TWO: &str = "mmrstadmgrp000000000000002";
const GROUP_THREE: &str = "mmrstadmgrp000000000000003";

/// A team whose membership makes every predicate of `teamMembersMinusGroupMembersQuery` matter.
///
/// | member | groups | why it is here |
/// |---|---|---|
/// | admin | — | the creator, and the only member with no `GroupMembers` row at all |
/// | `tga` | one | the user `minus GROUP_ONE` must **exclude** |
/// | `tgb` | two | excluded only when `GROUP_TWO` is asked for |
/// | `tgc` | two, three | two joined rows for one user — what `count(DISTINCT)` is for |
/// | `tgd` | one, **deleted** | the subquery's `deleteat = 0`: not excluded, yet still reports the group |
/// | `tge` | — | deactivated, so `Users.DeleteAt = 0` drops them |
/// | `tgf` | — | `TeamMembers.DeleteAt != 0` — **the predicate the channel twin does not have** |
/// | a bot | — | a team member that `Bots.UserId IS NULL` drops |
struct Fixture {
    team: String,
    outsider_team: String,
    admin: String,
    users: std::collections::HashMap<&'static str, String>,
    /// A plain user **in** the fixture team, so `team_user` grants `invite_user` and
    /// `add_user_to_team` — the token that reaches `invite/email`'s body gates.
    member_token: String,
    /// A plain user in a different team, so nothing but `system_user` applies to the fixture team.
    outsider_token: String,
}

/// **The shared admin is a member of every team this suite makes, and creating one moves its
/// `Users.UpdateAt`** — which is a field of this route's body, for a user who appears on every
/// page. Five byte comparisons failed on a timestamp neither server got wrong before this existed,
/// each of them passing on its own.
///
/// Read/write rather than a mutex, for the reason `common::ACTIVE_LICENCE_ROW` gives: the byte
/// comparisons hold it **shared** and still run in parallel; the one test that creates a team after
/// the fixture is built holds it **exclusively**.
static TEAM_WRITES: tokio::sync::RwLock<()> = tokio::sync::RwLock::const_new(());

static FIXTURE: tokio::sync::OnceCell<Fixture> = tokio::sync::OnceCell::const_new();

async fn pool() -> sqlx::PgPool {
    let url = std::env::var("DATABASE_URL").expect("the parity stack exports DATABASE_URL");
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(std::time::Duration::from_secs(5))
        .connect(&url)
        .await
        .expect("the shared database is reachable")
}

async fn fixture(client: &reqwest::Client, token: &str) -> &'static Fixture {
    FIXTURE
        .get_or_init(|| async {
            let team = create_team(client, token, "tadmgrp").await;
            let outsider_team = create_team(client, token, "tadmout").await;

            let admin = client
                .get(format!("{GO}/api/v4/users/me"))
                .header("Authorization", format!("Bearer {token}"))
                .send()
                .await
                .expect("Go answers")
                .json::<serde_json::Value>()
                .await
                .expect("a user")["id"]
                .as_str()
                .expect("an id")
                .to_owned();

            let mut users = std::collections::HashMap::new();
            let mut member_token = String::new();
            for tag in ["tga", "tgb", "tgc", "tgd", "tge", "tgf"] {
                let user = create_plain_user(client, token, &team, tag).await;
                if tag == "tga" {
                    member_token = user.token.clone();
                }
                users.insert(tag, user.id);
            }
            let outsider = create_plain_user(client, token, &outsider_team, "tgg").await;
            users.insert("tgg", outsider.id);

            // A bot in the team, for `Bots.UserId IS NULL`. Planted rather than created: `POST
            // /bots` is refused on this deployment (`EnableBotAccountCreation` is false), and an
            // assertion nested inside an `if let Some(bot)` that never matches is a test that
            // cannot fail — see `parity::channel_admin`, where exactly that let a mutation live.
            let bot_id = common::plant_bot("tadmgrp", &admin, 0)
                .await
                .expect("the parity stack exports DATABASE_URL");
            let pool = pool().await;
            sqlx::query(
                "INSERT INTO teammembers
                    (teamid, userid, roles, deleteat, schemeuser, schemeadmin, schemeguest, createat)
                 VALUES ($1, $2, '', 0, TRUE, FALSE, FALSE, 1788600000000)
                 ON CONFLICT (teamid, userid) DO NOTHING",
            )
            .bind(&team)
            .bind(&bot_id)
            .execute(&pool)
            .await
            .expect("the bot joins the team");
            users.insert("bot", bot_id);

            // **`TeamMembers.DeleteAt` is planted, because no route writes it.**
            // `SqlTeamStore.RemoveMember` issues a `DELETE`, not a stamp (team_store.go:1273), so
            // `DELETE /teams/{id}/members/{user}` leaves no row at all and cannot exercise the
            // predicate. The column exists, the query filters on it, and an UPDATE here is the
            // only way to put a row on the wrong side of it.
            sqlx::query("UPDATE teammembers SET deleteat = 1700000002000 WHERE teamid = $1 AND userid = $2")
                .bind(&team)
                .bind(&users["tgf"])
                .execute(&pool)
                .await
                .expect("tgf's membership is stamped deleted");

            // `tge` is deactivated **last**, so its `Users.UpdateAt` settles before any read.
            let deactivated = client
                .delete(format!("{GO}/api/v4/users/{}", users["tge"]))
                .header("Authorization", format!("Bearer {token}"))
                .send()
                .await
                .expect("Go answers");
            assert!(deactivated.status().is_success(), "tge is deactivated");

            // The group rows, by SQL: every route that would write them is licence-gated to a 501.
            sqlx::query("DELETE FROM groupmembers WHERE groupid LIKE 'mmrstadmgrp%'")
                .execute(&pool)
                .await
                .expect("the old memberships clear");
            sqlx::query("DELETE FROM usergroups WHERE id LIKE 'mmrstadmgrp%'")
                .execute(&pool)
                .await
                .expect("the old groups clear");

            for (id, name) in [
                (GROUP_ONE, "mmrs-tadmgrp-one"),
                (GROUP_TWO, "mmrs-tadmgrp-two"),
                (GROUP_THREE, "mmrs-tadmgrp-three"),
            ] {
                sqlx::query(
                    "INSERT INTO usergroups
                       (id, name, displayname, description, source, remoteid,
                        createat, updateat, deleteat, allowreference)
                     VALUES ($1, $2, $3, 'a group this suite made', 'custom', NULL,
                             1700000000000, 1700000000000, 0, TRUE)",
                )
                .bind(id)
                .bind(name)
                .bind(format!("Display {name}"))
                .execute(&pool)
                .await
                .expect("the group is written");
            }

            for (group, user, delete_at) in [
                (GROUP_ONE, users["tga"].as_str(), 0i64),
                (GROUP_TWO, users["tgb"].as_str(), 0),
                (GROUP_TWO, users["tgc"].as_str(), 0),
                (GROUP_THREE, users["tgc"].as_str(), 0),
                // The deleted membership: `tgd` is still reported as being in `GROUP_ONE` by the
                // outer `string_agg`, which has no `DeleteAt` filter, and is **not** excluded by
                // the subquery, which has one.
                (GROUP_ONE, users["tgd"].as_str(), 1700000001000),
            ] {
                sqlx::query(
                    "INSERT INTO groupmembers (groupid, userid, createat, deleteat)
                     VALUES ($1, $2, 1700000000000, $3)",
                )
                .bind(group)
                .bind(user)
                .bind(delete_at)
                .execute(&pool)
                .await
                .expect("the membership is written");
            }

            Fixture {
                team,
                outsider_team,
                admin,
                users,
                member_token,
                outsider_token: outsider.token,
            }
        })
        .await
}

fn minus(team_id: &str, query: &str) -> String {
    format!("/api/v4/teams/{team_id}/members_minus_group_members?{query}")
}
fn scheme_path(team_id: &str) -> String {
    format!("/api/v4/teams/{team_id}/scheme")
}
fn invite_path(team_id: &str) -> String {
    format!("/api/v4/teams/{team_id}/invite/email")
}
fn invite_guests_path(team_id: &str) -> String {
    format!("/api/v4/teams/{team_id}/invite-guests/email")
}
fn import_path(team_id: &str) -> String {
    format!("/api/v4/teams/{team_id}/import")
}

/// `PUT`/`POST` raw bytes to a path on both servers. Local rather than `common::post_both_raw`
/// because that helper asserts the Rust answer was served locally, and three of these five routes
/// deliberately forward.
async fn send_both_raw(
    client: &reqwest::Client,
    method: reqwest::Method,
    token: &str,
    path: &str,
    content_type: &str,
    body: &[u8],
) -> ((u16, Vec<u8>), (u16, Vec<u8>)) {
    let send = async |base: &str| {
        let response = client
            .request(method.clone(), format!("{base}{path}"))
            .header("Authorization", format!("Bearer {token}"))
            .header("Content-Type", content_type)
            .body(body.to_vec())
            .send()
            .await
            .unwrap_or_else(|e| panic!("{base}{path} is unreachable: {e}"));
        let status = response.status().as_u16();
        (status, response.bytes().await.expect("body reads").to_vec())
    };

    (send(GO).await, send(RUST).await)
}

async fn put_both(
    client: &reqwest::Client,
    token: &str,
    path: &str,
    body: &[u8],
) -> ((u16, Vec<u8>), (u16, Vec<u8>)) {
    send_both_raw(
        client,
        reqwest::Method::PUT,
        token,
        path,
        "application/json",
        body,
    )
    .await
}

async fn post_both(
    client: &reqwest::Client,
    token: &str,
    path: &str,
    body: &[u8],
) -> ((u16, Vec<u8>), (u16, Vec<u8>)) {
    send_both_raw(
        client,
        reqwest::Method::POST,
        token,
        path,
        "application/json",
        body,
    )
    .await
}

/// Which server answered — the header every locally served response carries and no proxied one
/// does.
async fn served_by(
    client: &reqwest::Client,
    method: reqwest::Method,
    token: &str,
    path: &str,
    content_type: &str,
    body: &[u8],
) -> Option<String> {
    client
        .request(method, format!("{RUST}{path}"))
        .header("Authorization", format!("Bearer {token}"))
        .header("Content-Type", content_type)
        .body(body.to_vec())
        .send()
        .await
        .expect("we answer")
        .headers()
        .get("x-mmrs-served-by")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
}

// ---------------------------------------------------------------------------
// GET /api/v4/teams/{team_id}/members_minus_group_members
// ---------------------------------------------------------------------------

/// The answer itself, byte for byte, and then the eight facts about the fixture that make the byte
/// comparison mean something.
#[tokio::test]
async fn the_team_page_matches_go_byte_for_byte_and_every_predicate_bites() {
    if !stack_enabled() {
        return;
    }
    let _stable = TEAM_WRITES.read().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let p = minus(&f.team, &format!("group_ids={GROUP_ONE}"));
    let (go, rs) = common::fetch_both(&client, &token, &p).await;
    assert_eq!(go, rs, "{p}: {}", String::from_utf8_lossy(&rs));
    assert!(
        !rs.ends_with(b"\n"),
        "`json.Marshal` then `w.Write`, so no trailing newline"
    );

    let body: serde_json::Value = serde_json::from_slice(&go).expect("the body decodes");
    let users = body["users"].as_array().expect("a users array");
    let ids: Vec<&str> = users
        .iter()
        .map(|u| u["id"].as_str().expect("an id"))
        .collect();

    // 1. The member in the asked-for group is gone; 2. the members in other groups are not.
    assert!(
        !ids.contains(&f.users["tga"].as_str()),
        "tga is in GROUP_ONE"
    );
    assert!(
        ids.contains(&f.users["tgb"].as_str()),
        "tgb is in GROUP_TWO"
    );
    assert!(
        ids.contains(&f.users["tgc"].as_str()),
        "tgc is in two others"
    );
    // 3. A **deleted** group membership does not exclude: the subquery filters `DeleteAt = 0`.
    assert!(
        ids.contains(&f.users["tgd"].as_str()),
        "tgd's GROUP_ONE membership is deleted"
    );
    // 4. `Users.DeleteAt = 0` drops the deactivated member.
    assert!(
        !ids.contains(&f.users["tge"].as_str()),
        "tge is deactivated"
    );
    // 5. **`TeamMembers.DeleteAt = 0`** — the predicate with no channel counterpart. `tgf` is an
    //    active user whose membership row is stamped deleted, so nothing else in the query
    //    excludes them.
    assert!(
        !ids.contains(&f.users["tgf"].as_str()),
        "tgf's team membership is stamped deleted"
    );
    // 6. `Bots.UserId IS NULL` drops the bot — a `TeamMembers` row like any other, in no group.
    assert!(
        !ids.contains(&f.users["bot"].as_str()),
        "a bot is a team member here and must not be in the answer"
    );
    // 7. The member with no group row at all is present, with `groups: []` and not `null`.
    assert!(ids.contains(&f.admin.as_str()), "the admin is a member");
    let admin = users
        .iter()
        .find(|u| u["id"] == f.admin.as_str())
        .expect("the admin row");
    assert_eq!(admin["groups"], serde_json::json!([]), "empty, not null");

    // 8. `total_count` counts **users**, not joined rows: `tgc` is in two groups and contributes
    //    one. A `count(*)` would answer one more than the page holds.
    assert_eq!(
        body["total_count"].as_u64().expect("a count"),
        ids.len() as u64,
        "one row per user, whatever their group count"
    );
    assert_eq!(
        ids.len(),
        4,
        "the admin, tgb, tgc and tgd — and if this number moves, a predicate stopped biting"
    );
}

/// The `groups` array is **every** group the member is in, not only the ones asked about — and it
/// ignores `GroupMembers.DeleteAt`, unlike the subquery three lines above it in the same SQL.
#[tokio::test]
async fn the_groups_array_is_hydrated_from_every_membership_row() {
    if !stack_enabled() {
        return;
    }
    let _stable = TEAM_WRITES.read().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let p = minus(&f.team, &format!("group_ids={GROUP_ONE}"));
    let (go, rs) = common::fetch_both(&client, &token, &p).await;
    assert_eq!(go, rs, "{p}");
    let body: serde_json::Value = serde_json::from_slice(&go).expect("the body decodes");
    let users = body["users"].as_array().expect("a users array");

    let groups_of = |user_id: &str| -> Vec<String> {
        users
            .iter()
            .find(|u| u["id"] == user_id)
            .and_then(|u| u["groups"].as_array())
            .expect("a groups array")
            .iter()
            .map(|g| g["id"].as_str().expect("a group id").to_owned())
            .collect()
    };

    let mut tgc = groups_of(&f.users["tgc"]);
    tgc.sort();
    assert_eq!(
        tgc,
        vec![GROUP_TWO.to_owned(), GROUP_THREE.to_owned()],
        "two memberships, two groups"
    );

    // And the hydrated group carries the stored columns, not an id-only stub.
    let group = users
        .iter()
        .find(|u| u["id"] == f.users["tgb"].as_str())
        .and_then(|u| u["groups"].as_array())
        .and_then(|g| g.first())
        .expect("tgb's group");
    assert_eq!(group["display_name"], "Display mmrs-tadmgrp-two");
    assert_eq!(group["source"], "custom");
    assert_eq!(group["allow_reference"], true);
    assert_eq!(group["has_syncables"], false);
    assert_eq!(group["member_ids"], serde_json::Value::Null);
    assert!(
        group.get("member_count").is_none(),
        "`omitempty` and not computed"
    );
}

/// Asking for two groups removes the members of both, and `total_count` follows the page.
#[tokio::test]
async fn a_second_group_id_removes_a_second_member() {
    if !stack_enabled() {
        return;
    }
    let _stable = TEAM_WRITES.read().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let p = minus(&f.team, &format!("group_ids={GROUP_ONE},{GROUP_TWO}"));
    let (go, rs) = common::fetch_both(&client, &token, &p).await;
    assert_eq!(go, rs, "{p}: {}", String::from_utf8_lossy(&rs));

    let body: serde_json::Value = serde_json::from_slice(&go).expect("the body decodes");
    let ids: Vec<&str> = body["users"]
        .as_array()
        .expect("a users array")
        .iter()
        .map(|u| u["id"].as_str().expect("an id"))
        .collect();
    for tag in ["tga", "tgb", "tgc"] {
        assert!(!ids.contains(&f.users[tag].as_str()), "{tag} is excluded");
    }
    assert!(ids.contains(&f.users["tgd"].as_str()), "tgd is not");
    assert_eq!(
        body["total_count"].as_u64().expect("a count"),
        ids.len() as u64
    );
}

/// Paging is `LIMIT per_page OFFSET page * per_page`, and `total_count` is the **whole** set.
#[tokio::test]
async fn paging_slices_the_page_and_leaves_the_total_alone() {
    if !stack_enabled() {
        return;
    }
    let _stable = TEAM_WRITES.read().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let whole = minus(&f.team, &format!("group_ids={GROUP_ONE}"));
    let (go_whole, rs_whole) = common::fetch_both(&client, &token, &whole).await;
    assert_eq!(go_whole, rs_whole, "{whole}");
    let whole: serde_json::Value = serde_json::from_slice(&go_whole).expect("the body decodes");
    let total = whole["total_count"].as_u64().expect("a count");
    assert!(total >= 3, "the fixture must have something to page");

    let mut seen = Vec::new();
    for page in 0..total {
        let p = minus(
            &f.team,
            &format!("group_ids={GROUP_ONE}&page={page}&per_page=1"),
        );
        let (go, rs) = common::fetch_both(&client, &token, &p).await;
        assert_eq!(go, rs, "{p}: {}", String::from_utf8_lossy(&rs));
        let body: serde_json::Value = serde_json::from_slice(&go).expect("the body decodes");
        assert_eq!(
            body["total_count"].as_u64().expect("a count"),
            total,
            "page {page}: the total is the whole set"
        );
        let users = body["users"].as_array().expect("a users array");
        assert_eq!(users.len(), 1, "page {page}: one per page");
        seen.push(users[0]["id"].as_str().expect("an id").to_owned());
    }

    let expected: Vec<String> = whole["users"]
        .as_array()
        .expect("a users array")
        .iter()
        .map(|u| u["id"].as_str().expect("an id").to_owned())
        .collect();
    assert_eq!(
        seen, expected,
        "`ORDER BY Users.Username ASC`, one at a time"
    );

    // **A page size other than one.** With `per_page=1` the offset is `page * 1`, so a port that
    // dropped the multiplication would agree on every page above.
    let p = minus(&f.team, &format!("group_ids={GROUP_ONE}&page=0&per_page=2"));
    let (go, rs) = common::fetch_both(&client, &token, &p).await;
    assert_eq!(go, rs, "{p}: {}", String::from_utf8_lossy(&rs));
    let first: serde_json::Value = serde_json::from_slice(&go).expect("the body decodes");
    let p = minus(&f.team, &format!("group_ids={GROUP_ONE}&page=1&per_page=2"));
    let (go, rs) = common::fetch_both(&client, &token, &p).await;
    assert_eq!(go, rs, "{p}: {}", String::from_utf8_lossy(&rs));
    let second: serde_json::Value = serde_json::from_slice(&go).expect("the body decodes");

    let page_ids = |v: &serde_json::Value| -> Vec<String> {
        v["users"]
            .as_array()
            .expect("a users array")
            .iter()
            .map(|u| u["id"].as_str().expect("an id").to_owned())
            .collect()
    };
    let (a, b) = (page_ids(&first), page_ids(&second));
    assert_eq!(a, expected[..2], "page 0 of 2 is the first two");
    assert_eq!(
        b,
        expected[2..(4.min(expected.len()))],
        "page 1 of 2 starts at offset 2, not at offset 1"
    );

    // One page past the end is an empty list and the same total, not a 404.
    let p = minus(
        &f.team,
        &format!("group_ids={GROUP_ONE}&page={total}&per_page=1"),
    );
    let (go, rs) = common::fetch_both(&client, &token, &p).await;
    assert_eq!(go, rs, "{p}");
    let body: serde_json::Value = serde_json::from_slice(&go).expect("the body decodes");
    assert_eq!(body["users"], serde_json::json!([]));
    assert_eq!(body["total_count"].as_u64().expect("a count"), total);
}

/// `ORDER BY Users.Username ASC`, asserted against the usernames the body itself carries.
#[tokio::test]
async fn the_page_is_ordered_by_username() {
    if !stack_enabled() {
        return;
    }
    let _stable = TEAM_WRITES.read().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let p = minus(&f.team, &format!("group_ids={GROUP_ONE}"));
    let (go, rs) = common::fetch_both(&client, &token, &p).await;
    assert_eq!(go, rs, "{p}");
    let body: serde_json::Value = serde_json::from_slice(&go).expect("the body decodes");
    let names: Vec<&str> = body["users"]
        .as_array()
        .expect("a users array")
        .iter()
        .map(|u| u["username"].as_str().expect("a username"))
        .collect();
    let mut sorted = names.clone();
    sorted.sort();
    assert_eq!(names, sorted, "ascending, and by username and not by id");
    assert!(names.len() > 1, "one element is sorted by accident");
}

/// The two `group_ids` gates, over HTTP — the same parser the channel route uses, reached from the
/// team path, so a regression in either handler's wiring shows here.
#[tokio::test]
async fn the_group_ids_gates_answer_the_same_400_go_does() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    for query in [
        "".to_owned(),
        "group_ids=".to_owned(),
        "group_ids=abcdefghijklmnopqrstuvwxy".to_owned(),
        format!("group_ids={}", "a!".repeat(20)),
        format!("group_ids={}", "a".repeat(30)),
        format!("group_ids={},{}", "a".repeat(13), "b".repeat(13)),
        format!("group_ids={GROUP_ONE},"),
        // The two gates run on two different strings: stripped this is a valid id, raw it is 27
        // characters and is not.
        format!("group_ids={}!{}", &GROUP_ONE[..3], &GROUP_ONE[3..]),
    ] {
        let p = minus(&f.team, &query);
        let ((go_status, go), (rs_status, rs)) = common::fetch_both_raw(&client, &token, &p).await;
        assert_eq!(go_status, 400, "{p}: {}", String::from_utf8_lossy(&go));
        assert_eq!(rs_status, go_status, "{p}");
        let parsed = assert_error_bodies_match_except_known_gaps(&go, &rs, &p);
        assert_eq!(
            parsed["id"], "api.context.invalid_body_param.app_error",
            "{p}"
        );
        assert_eq!(
            parsed["message"],
            "Invalid or missing group_ids in request body."
        );
    }
}

/// **The gates are in this order**: the team id, then `group_ids`, then the permission — and the
/// team is never fetched at all.
#[tokio::test]
async fn the_id_gate_precedes_the_group_gate_and_neither_reads_the_team() {
    if !stack_enabled() {
        return;
    }
    let _stable = TEAM_WRITES.read().await;
    let client = client();
    let token = go_minted_token(&client).await;

    let p = minus("abc", "group_ids=short");
    let ((go_status, go), (rs_status, rs)) = common::fetch_both_raw(&client, &token, &p).await;
    assert_eq!(go_status, 400, "{p}");
    assert_eq!(rs_status, go_status, "{p}");
    let parsed = assert_error_bodies_match_except_known_gaps(&go, &rs, &p);
    assert_eq!(
        parsed["id"], "api.context.invalid_url_param.app_error",
        "{p}"
    );

    // A well-formed team id naming nothing: **200 with an empty page**, not a 404.
    let p = minus(ABSENT, &format!("group_ids={GROUP_ONE}"));
    let (go, rs) = common::fetch_both(&client, &token, &p).await;
    assert_eq!(go, rs, "{p}: {}", String::from_utf8_lossy(&rs));
    let body: serde_json::Value = serde_json::from_slice(&go).expect("the body decodes");
    assert_eq!(body["users"], serde_json::json!([]));
    assert_eq!(body["total_count"], 0);
}

/// The permission is **`sysconsole_read_user_management_groups`**, not the channel route's
/// `…_channels`, and it is asked **after** both `group_ids` gates.
#[tokio::test]
async fn a_plain_user_is_refused_but_only_after_the_group_gates() {
    if !stack_enabled() {
        return;
    }
    let _stable = TEAM_WRITES.read().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let p = minus(&f.team, &format!("group_ids={GROUP_ONE}"));
    let ((go_status, go), (rs_status, rs)) =
        common::fetch_both_raw(&client, &f.member_token, &p).await;
    assert_eq!(go_status, 403, "{p}: {}", String::from_utf8_lossy(&go));
    assert_eq!(rs_status, go_status, "{p}");
    let parsed = assert_error_bodies_match_except_known_gaps(&go, &rs, &p);
    assert_eq!(parsed["id"], "api.context.permissions.app_error", "{p}");

    let p = minus(&f.team, "group_ids=short");
    let ((go_status, go), (rs_status, rs)) =
        common::fetch_both_raw(&client, &f.member_token, &p).await;
    assert_eq!(go_status, 400, "{p}: the group gate runs first");
    assert_eq!(rs_status, go_status, "{p}");
    let parsed = assert_error_bodies_match_except_known_gaps(&go, &rs, &p);
    assert_eq!(
        parsed["id"], "api.context.invalid_body_param.app_error",
        "{p}"
    );
}

/// **The permission is the *groups* one, and no stock role separates it from the channels one.**
///
/// `system_admin`, `system_manager`, `system_read_only_admin` and `system_user_manager` all hold
/// both `sysconsole_read_user_management_groups` and `…_channels`, so a fixture built from stock
/// roles cannot tell the team route's gate from the channel route's — the mutation that swaps them
/// would survive against any session this suite otherwise has. Two planted roles, one permission
/// each, are what make it bite.
///
/// The two readers are created in the **outsider** team on purpose: this gate is a system
/// permission and asks nothing about membership, and adding two members to the fixture team would
/// change every page assertion above.
#[tokio::test]
async fn the_permission_is_the_groups_one_and_not_the_channels_one() {
    if !stack_enabled() {
        return;
    }
    let _exclusive = TEAM_WRITES.write().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let reader = async |tag: &str, permission: &str| -> Option<String> {
        let role = common::plant_role(tag, permission).await?;
        let user = create_plain_user(&client, &token, &f.outsider_team, tag).await;
        // The roles are copied onto the session row at login and never re-read, so the grant has
        // to precede the login that mints the token used below.
        common::set_user_roles(&user.id, &format!("system_user {role}")).await;
        Some(common::login_plain_user(&client, tag).await)
    };

    let Some(groups_reader) = reader("tadmgrpr", "sysconsole_read_user_management_groups").await
    else {
        return; // no DATABASE_URL to plant into
    };
    let Some(channels_reader) = reader("tadmchr", "sysconsole_read_user_management_channels").await
    else {
        return;
    };

    let p = minus(&f.team, &format!("group_ids={GROUP_ONE}"));

    // The groups permission admits, and the body is the same one the admin gets.
    let ((go_status, go), (rs_status, rs)) =
        common::fetch_both_raw(&client, &groups_reader, &p).await;
    assert_eq!(go_status, 200, "{p}: {}", String::from_utf8_lossy(&go));
    assert_eq!(rs_status, go_status, "{p}");
    assert_eq!(go, rs, "{p}: {}", String::from_utf8_lossy(&rs));

    // The channels permission does **not** — it is the gate on the *channel* route.
    let ((go_status, go), (rs_status, rs)) =
        common::fetch_both_raw(&client, &channels_reader, &p).await;
    assert_eq!(
        go_status,
        403,
        "{p}: `…_channels` is not this route's permission: {}",
        String::from_utf8_lossy(&go)
    );
    assert_eq!(rs_status, go_status, "{p}");
    let parsed = assert_error_bodies_match_except_known_gaps(&go, &rs, &p);
    assert_eq!(parsed["id"], "api.context.permissions.app_error", "{p}");

    // ...and the same two tokens the other way round on the channel route, so the pair is a
    // genuine separation rather than "one of these roles works and the other does not".
    let channel_path = format!(
        "/api/v4/channels/{}/members_minus_group_members?group_ids={GROUP_ONE}",
        ABSENT
    );
    let ((go_status, _), (rs_status, _)) =
        common::fetch_both_raw(&client, &channels_reader, &channel_path).await;
    assert_eq!(
        go_status, 200,
        "the channels permission opens the channel route"
    );
    assert_eq!(rs_status, go_status);
    let ((go_status, go), (rs_status, rs)) =
        common::fetch_both_raw(&client, &groups_reader, &channel_path).await;
    assert_eq!(
        go_status,
        403,
        "and the groups permission does not: {}",
        String::from_utf8_lossy(&go)
    );
    assert_eq!(rs_status, go_status);
    assert_error_bodies_match_except_known_gaps(&go, &rs, &channel_path);
}

/// **`Teams.DeleteAt = 0`.** A soft-deleted team keeps its `TeamMembers` rows, and this route
/// still answers 200 — with an empty page, because the join to `Teams` filters the team itself out.
#[tokio::test]
async fn a_deleted_team_answers_an_empty_page() {
    if !stack_enabled() {
        return;
    }
    let _exclusive = TEAM_WRITES.write().await;
    let client = client();
    let token = go_minted_token(&client).await;
    // **Await the fixture even though nothing here reads it.** Building it creates six accounts and
    // two teams, each of which moves the shared admin's `Users.UpdateAt` — which is in this route's
    // body. Without the barrier the Go call and the Rust call below straddle one of those writes
    // and the byte comparison fails on a timestamp neither server got wrong.
    let _barrier = fixture(&client, &token).await;

    let doomed = create_team(&client, &token, "tadmdoom").await;

    // Live, it answers its one member — the creator.
    let p = minus(&doomed, &format!("group_ids={GROUP_ONE}"));
    let (go, rs) = common::fetch_both(&client, &token, &p).await;
    assert_eq!(go, rs, "{p}: {}", String::from_utf8_lossy(&rs));
    let body: serde_json::Value = serde_json::from_slice(&go).expect("the body decodes");
    assert_eq!(body["total_count"], 1, "the creator, before the delete");

    let deleted = client
        .delete(format!("{GO}/api/v4/teams/{doomed}"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("Go answers");
    assert!(deleted.status().is_success(), "the team is soft-deleted");

    let (go, rs) = common::fetch_both(&client, &token, &p).await;
    assert_eq!(go, rs, "{p}: {}", String::from_utf8_lossy(&rs));
    let body: serde_json::Value = serde_json::from_slice(&go).expect("the body decodes");
    assert_eq!(body["users"], serde_json::json!([]), "and after, nothing");
    assert_eq!(body["total_count"], 0);
}

// ---------------------------------------------------------------------------
// PUT /api/v4/teams/{team_id}/scheme
// ---------------------------------------------------------------------------

/// **The team gate and the channel gate disagree about `""`.**
///
/// `updateTeamScheme`'s condition carries `&& *p.SchemeID != ""`, which `updateChannelScheme`'s
/// does not, so `{"scheme_id":""}` — the way a client detaches a team from its scheme — is a 501
/// here and a 400 there. Everything else collapses into one 400 naming `scheme_id`.
#[tokio::test]
async fn the_team_scheme_body_gate_separates_400_from_501() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;
    let p = scheme_path(&f.team);

    for body in [
        "",
        "garbage",
        "[]",
        "null",
        "{}",
        r#"{"scheme_id":null}"#,
        r#"{"scheme_id":"nope"}"#,
        r#"{"scheme_id":"abcdefghijklmnopqrstuvwxy"}"#,
        r#"{"scheme_id":"abcdefghijklmnopqrstuvwxyza"}"#,
        r#"{"scheme_id":"ab-defghijklmnopqrstuvwxyz"}"#,
        r#"{"scheme_id":5}"#,
    ] {
        let ((go_status, go), (rs_status, rs)) =
            put_both(&client, &token, &p, body.as_bytes()).await;
        assert_eq!(go_status, 400, "body {body:?} should not pass the gate");
        assert_eq!(rs_status, go_status, "body {body:?}");
        let parsed = assert_error_bodies_match_except_known_gaps(&go, &rs, &p);
        assert_eq!(
            parsed["id"], "api.context.invalid_body_param.app_error",
            "body {body:?}"
        );
        assert_eq!(
            parsed["message"],
            "Invalid or missing scheme_id in request body."
        );
    }

    for body in [
        // **The empty string passes**, and this is the only row that separates the two handlers.
        r#"{"scheme_id":""}"#.to_owned(),
        format!(r#"{{"scheme_id":"{SCHEME}"}}"#),
        r#"{"scheme_id":"ABCDEFGHIJKLMNOPQRSTUVWXYZ"}"#.to_owned(),
        r#"{"scheme_id":"12345678901234567890123456"}"#.to_owned(),
        format!(r#"{{"scheme_id":"{SCHEME}"}} and then some"#),
        format!(r#"{{"scheme_id":"{SCHEME}","unknown":1}}"#),
    ] {
        let ((go_status, go), (rs_status, rs)) =
            put_both(&client, &token, &p, body.as_bytes()).await;
        assert_eq!(
            go_status, 501,
            "body {body:?} should have reached the licence gate"
        );
        assert_eq!(rs_status, go_status, "body {body:?}");
        let parsed = assert_error_bodies_match_except_known_gaps(&go, &rs, &p);
        assert_eq!(
            parsed["id"], "api.team.update_team_scheme.license.error",
            "body {body:?}"
        );
    }
}

/// `RequireTeamId` precedes the body gate, and neither reads the `Teams` table: a well-formed id
/// naming nothing still reaches the 501.
#[tokio::test]
async fn the_team_scheme_id_gate_precedes_the_body_gate() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;

    let p = scheme_path("abc");
    let ((go_status, go), (rs_status, rs)) = put_both(&client, &token, &p, b"garbage").await;
    assert_eq!(go_status, 400, "{p}: the id check comes first");
    assert_eq!(rs_status, go_status, "{p}");
    let parsed = assert_error_bodies_match_except_known_gaps(&go, &rs, &p);
    assert_eq!(
        parsed["id"], "api.context.invalid_url_param.app_error",
        "{p}"
    );

    let p = scheme_path(ABSENT);
    let ((go_status, go), (rs_status, rs)) = put_both(
        &client,
        &token,
        &p,
        format!(r#"{{"scheme_id":"{SCHEME}"}}"#).as_bytes(),
    )
    .await;
    assert_eq!(go_status, 501, "{p}: no 404, the licence lands first");
    assert_eq!(rs_status, go_status, "{p}");
    assert_error_bodies_match_except_known_gaps(&go, &rs, &p);
}

/// A plain user reaches the same 400 and the same 501: neither gate is a permission question, and
/// `sysconsole_write_user_management_permissions` is asked **after** the licence.
#[tokio::test]
async fn a_plain_user_gets_the_scheme_gates_and_not_a_permission_error() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;
    let p = scheme_path(&f.team);

    for (body, status) in [
        (r#"{"scheme_id":"nope"}"#, 400u16),
        (r#"{"scheme_id":""}"#, 501),
    ] {
        let ((go_status, go), (rs_status, rs)) =
            put_both(&client, &f.outsider_token, &p, body.as_bytes()).await;
        assert_eq!(
            go_status,
            status,
            "{body:?}: {}",
            String::from_utf8_lossy(&go)
        );
        assert_eq!(rs_status, go_status, "{body:?}");
        let parsed = assert_error_bodies_match_except_known_gaps(&go, &rs, &p);
        assert_ne!(
            parsed["id"], "api.context.permissions.app_error",
            "{body:?}: the permission is never asked unlicensed"
        );
    }
}

// ---------------------------------------------------------------------------
// POST /api/v4/teams/{team_id}/invite-guests/email
// ---------------------------------------------------------------------------

/// **The licence check is this handler's first statement**, ahead of `RequireTeamId`, the
/// permission and the body — so every input on an unlicensed server is the same 501.
#[tokio::test]
async fn invite_guests_answers_the_licence_error_before_anything_else() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let cases: [(String, &str, &str); 4] = [
        // A real team and a well-formed body.
        (
            invite_guests_path(&f.team),
            r#"{"emails":["g@mmrs.invalid"],"channels":["abcdefghijklmnopqrstuvwxyz"],"message":"hi"}"#,
            &token,
        ),
        // A team id that is not id-shaped: `RequireTeamId` never runs.
        (invite_guests_path("zzz"), "{}", &token),
        // A team that does not exist, and a body that is not JSON.
        (invite_guests_path(ABSENT), "garbage", &token),
        // A user with no permission on the team at all.
        (invite_guests_path(&f.team), "{}", &f.outsider_token),
    ];
    for (p, body, tok) in cases {
        let ((go_status, go), (rs_status, rs)) = post_both(&client, tok, &p, body.as_bytes()).await;
        assert_eq!(go_status, 501, "{p}: {}", String::from_utf8_lossy(&go));
        assert_eq!(rs_status, go_status, "{p}");
        let parsed = assert_error_bodies_match_except_known_gaps(&go, &rs, &p);
        assert_eq!(
            parsed["id"], "api.team.invite_guests_to_channels.license.error",
            "{p}"
        );
        assert!(!rs.ends_with(b"\n"), "{p}: error bodies carry no newline");
    }
}

/// A licence row hands both licence-gated routes back to Go — and the 400 in front of `scheme`'s
/// gate is **not** licence-dependent, which is the branch a "forward when licensed" shortcut loses.
#[tokio::test]
async fn a_license_row_hands_the_two_licence_routes_back() {
    if !stack_enabled() {
        return;
    }
    let _exclusive = ACTIVE_LICENCE_ROW.write().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let valid = format!(r#"{{"scheme_id":"{SCHEME}"}}"#);
    let cases: [(reqwest::Method, String, &str); 2] = [
        (reqwest::Method::PUT, scheme_path(&f.team), valid.as_str()),
        (reqwest::Method::POST, invite_guests_path(&f.team), "{}"),
    ];

    set_active_licence_id(None).await;
    for (method, p, body) in &cases {
        assert_eq!(
            served_by(
                &client,
                method.clone(),
                &token,
                p,
                "application/json",
                body.as_bytes()
            )
            .await
            .as_deref(),
            Some("rust"),
            "{p}: unlicensed, so ours to answer"
        );
    }

    set_active_licence_id(Some("mmrslicence000000000000001")).await;
    let mut forwarded = Vec::new();
    for (method, p, body) in &cases {
        forwarded.push(
            served_by(
                &client,
                method.clone(),
                &token,
                p,
                "application/json",
                body.as_bytes(),
            )
            .await,
        );
    }
    let bad_body = served_by(
        &client,
        reqwest::Method::PUT,
        &token,
        &scheme_path(&f.team),
        "application/json",
        br#"{"scheme_id":"nope"}"#,
    )
    .await;
    set_active_licence_id(None).await;

    for ((_, p, _), served) in cases.iter().zip(&forwarded) {
        assert_eq!(
            served.as_deref(),
            Some("go"),
            "{p}: a licence means work we have not ported"
        );
    }
    assert_eq!(
        bad_body.as_deref(),
        Some("rust"),
        "the body gate runs before the licence check, licensed or not"
    );
}

// ---------------------------------------------------------------------------
// POST /api/v4/teams/{team_id}/invite/email
// ---------------------------------------------------------------------------

/// The six refusals, in Go's order.
#[tokio::test]
async fn the_invite_gates_answer_what_go_answers() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    // 1. `team_id` is not id-shaped.
    let p = invite_path("abc");
    let ((go_status, go), (rs_status, rs)) = post_both(&client, &token, &p, b"[]").await;
    assert_eq!(go_status, 400, "{p}");
    assert_eq!(rs_status, go_status, "{p}");
    let parsed = assert_error_bodies_match_except_known_gaps(&go, &rs, &p);
    assert_eq!(
        parsed["id"], "api.context.invalid_url_param.app_error",
        "{p}"
    );

    // 2/3. A user with no membership of this team falls back to `system_user`, which grants
    //      neither `invite_user` nor `add_user_to_team`. The permission precedes the body, so a
    //      body that would be a 400 is still a 403 here.
    let p = invite_path(&f.team);
    let ((go_status, go), (rs_status, rs)) =
        post_both(&client, &f.outsider_token, &p, b"garbage").await;
    assert_eq!(go_status, 403, "{p}: {}", String::from_utf8_lossy(&go));
    assert_eq!(rs_status, go_status, "{p}");
    let parsed = assert_error_bodies_match_except_known_gaps(&go, &rs, &p);
    assert_eq!(parsed["id"], "api.context.permissions.app_error", "{p}");

    // 4. The body does not decode. A `team_user` is past the permissions, so this is reachable.
    for body in ["garbage", r#"{"emails":5}"#, ""] {
        let ((go_status, go), (rs_status, rs)) =
            post_both(&client, &f.member_token, &p, body.as_bytes()).await;
        assert_eq!(go_status, 400, "{body:?}: {}", String::from_utf8_lossy(&go));
        assert_eq!(rs_status, go_status, "{body:?}");
        let parsed = assert_error_bodies_match_except_known_gaps(&go, &rs, &p);
        assert_eq!(
            parsed["id"], "api.team.invite_members_to_team_and_channels.invalid_body.app_error",
            "{body:?}"
        );
    }

    // 5. `emails` is empty — and **a bare array is a well-formed body**, so `[]` is this refusal
    //    and not the one above it.
    // `null` is never a decode error in Go, so every row here is row 5 and not row 4 — see the
    // `behaviour_member_invite` corpus.
    for body in [
        "[]",
        "{}",
        "null",
        r#"{"emails":[]}"#,
        r#"{"emails":null}"#,
        r#"{"emails":null,"message":null,"channelIds":null,"profiles":null}"#,
    ] {
        let ((go_status, go), (rs_status, rs)) =
            post_both(&client, &f.member_token, &p, body.as_bytes()).await;
        assert_eq!(go_status, 400, "{body:?}: {}", String::from_utf8_lossy(&go));
        assert_eq!(rs_status, go_status, "{body:?}");
        let parsed = assert_error_bodies_match_except_known_gaps(&go, &rs, &p);
        assert_eq!(
            parsed["id"], "api.context.invalid_body_param.app_error",
            "{body:?}"
        );
        assert_eq!(
            parsed["message"],
            "Invalid or missing user_email in request body."
        );
    }

    // 6. `profiles` without `?graceful=`.
    for with_profiles in [
        r#"{"emails":["mmrstadm@mmrs.invalid"],"profiles":[{"email":"mmrstadm@mmrs.invalid","username":"mmrstadmprofile"}]}"#,
        // **A `null` element counts.** Go's `[]*MemberInviteProfile` holds one nil pointer here,
        // `len(Profiles) > 0` is true, and the gate fires — where a port that refused the `null`
        // would answer row 4 instead.
        r#"{"emails":["mmrstadm@mmrs.invalid"],"profiles":[null]}"#,
        // And a profile whose every field is `null` is a profile, not a decode error.
        r#"{"emails":["mmrstadm@mmrs.invalid"],"profiles":[{"email":null,"username":null}]}"#,
    ] {
        let ((go_status, go), (rs_status, rs)) =
            post_both(&client, &f.member_token, &p, with_profiles.as_bytes()).await;
        assert_eq!(
            go_status,
            400,
            "{with_profiles}: {}",
            String::from_utf8_lossy(&go)
        );
        assert_eq!(rs_status, go_status, "{with_profiles}");
        let parsed = assert_error_bodies_match_except_known_gaps(&go, &rs, &p);
        assert_eq!(
            parsed["id"], "api.team.invite_members.profiles_graceful.app_error",
            "{with_profiles}"
        );
    }
}

/// **The route asks for two permissions, and `team_user` holds both** — so no ordinary session can
/// tell them apart, and the mutation collapsing the pair to one `invite_user` survived the first
/// run for exactly that reason.
///
/// A planted system role with `invite_user` **alone**, on a user who is not in the team, separates
/// them: `SessionHasPermissionToTeam` finds no membership and falls back to the system roles, which
/// grant the first check and not the second. The answer is the 403; a handler that checked
/// `invite_user` twice would reach the body and answer a 400.
#[tokio::test]
async fn invite_asks_for_add_user_to_team_as_well_as_invite_user() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let reader = async |tag: &str, permissions: &str| -> Option<String> {
        let role = common::plant_role(tag, permissions).await?;
        let user = create_plain_user(&client, &token, &f.outsider_team, tag).await;
        common::set_user_roles(&user.id, &format!("system_user {role}")).await;
        Some(common::login_plain_user(&client, tag).await)
    };

    let Some(invite_only) = reader("tadminv1", "invite_user").await else {
        return; // no DATABASE_URL to plant into
    };
    let Some(invite_and_add) = reader("tadminv2", "invite_user add_user_to_team").await else {
        return;
    };

    let p = invite_path(&f.team);

    // `invite_user` alone: the **second** check refuses, and the body is never read.
    let ((go_status, go), (rs_status, rs)) = post_both(&client, &invite_only, &p, b"[]").await;
    assert_eq!(
        go_status,
        403,
        "`add_user_to_team` is a second, separate check: {}",
        String::from_utf8_lossy(&go)
    );
    assert_eq!(rs_status, go_status, "{p}");
    let parsed = assert_error_bodies_match_except_known_gaps(&go, &rs, &p);
    assert_eq!(parsed["id"], "api.context.permissions.app_error", "{p}");

    // Both permissions: past the gate, so the empty `emails` 400 lands instead.
    let ((go_status, go), (rs_status, rs)) = post_both(&client, &invite_and_add, &p, b"[]").await;
    assert_eq!(
        go_status,
        400,
        "both permissions reach the body: {}",
        String::from_utf8_lossy(&go)
    );
    assert_eq!(rs_status, go_status, "{p}");
    let parsed = assert_error_bodies_match_except_known_gaps(&go, &rs, &p);
    assert_eq!(
        parsed["message"],
        "Invalid or missing user_email in request body."
    );

    // **What no HTTP test can see**: which of the two permissions the 403 *names*. Go passes
    // `PermissionInviteUser` to `SetPermissionError` under either check (api4/team.go:1760, :1765),
    // and the name reaches only `detailed_error`, which `WipeDetailed` blanks on both servers when
    // `EnableDeveloper` is false — the default, and not ported. The two bodies are identical.
    assert_eq!(parsed["detailed_error"], "", "both servers wipe it");
}

/// `graceful` is presence-**and-non-empty**: `?graceful` bare is false, `?graceful=0` is true. The
/// difference is visible exactly once, on the `profiles` refusal.
#[tokio::test]
async fn graceful_is_presence_and_non_empty() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let body = r#"{"emails":["mmrstadmg@mmrs.invalid"],"profiles":[{"email":"mmrstadmg@mmrs.invalid","username":"mmrstadmgrace"}]}"#;

    // Bare `?graceful` — Go's `Get` returns "", so this is **not** graceful and the 400 fires.
    let p = format!("{}?graceful", invite_path(&f.team));
    let ((go_status, go), (rs_status, rs)) =
        post_both(&client, &f.member_token, &p, body.as_bytes()).await;
    assert_eq!(go_status, 400, "{p}: {}", String::from_utf8_lossy(&go));
    assert_eq!(rs_status, go_status, "{p}");
    let parsed = assert_error_bodies_match_except_known_gaps(&go, &rs, &p);
    assert_eq!(
        parsed["id"], "api.team.invite_members.profiles_graceful.app_error",
        "{p}"
    );

    // `?graceful=0` is a non-empty value, so the 400 does not fire and the request forwards.
    let p = format!("{}?graceful=0", invite_path(&f.team));
    assert_eq!(
        served_by(
            &client,
            reqwest::Method::POST,
            &f.member_token,
            &p,
            "application/json",
            body.as_bytes()
        )
        .await
        .as_deref(),
        Some("go"),
        "{p}: past every gate, so Go sends the mail"
    );
}

/// **The proof that the hand-over precedes any write.**
///
/// Not by watching for a write, but by showing that nothing this server answers on either write
/// route is a success: every input either produces a 4xx from Rust or is forwarded whole. A
/// handler with no 2xx of its own cannot have written anything before handing over — and the two
/// rows that *do* forward are the two that would have written.
#[tokio::test]
async fn every_answer_this_server_gives_on_the_two_write_routes_is_a_refusal() {
    if !stack_enabled() {
        return;
    }
    // **`import`'s first gate is the licence**, so three of the six refusals below become
    // forwards the moment a sibling test plants a licence row — and the assertion then fails
    // naming a route the writer never touched. This lock was missing on the first mutation run
    // and the no-op control `control-rename-the-team-offset-binding` was reported CAUGHT by
    // *this* test, which the control cannot affect.
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    // Refusals: ours, and never a success.
    let refusals: [(&str, String, &str, &str); 6] = [
        ("invite", invite_path("abc"), "application/json", "[]"),
        (
            "invite",
            invite_path(&f.team),
            "application/json",
            "garbage",
        ),
        ("invite", invite_path(&f.team), "application/json", "[]"),
        ("import", import_path("abc"), "application/json", "{}"),
        ("import", import_path(&f.team), "application/json", "{}"),
        (
            "import",
            import_path(&f.team),
            "multipart/form-data; boundary=X",
            "--X\r\nContent-Disposition: form-data; name=\"importFrom\"\r\n\r\nnotslack\r\n--X--\r\n",
        ),
    ];
    for (label, p, content_type, body) in refusals {
        let response = client
            .post(format!("{RUST}{p}"))
            .header("Authorization", format!("Bearer {}", f.member_token))
            .header("Content-Type", content_type)
            .body(body.to_owned())
            .send()
            .await
            .expect("we answer");
        let served = response
            .headers()
            .get("x-mmrs-served-by")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let status = response.status().as_u16();
        assert_eq!(
            served.as_deref(),
            Some("rust"),
            "{label} {p}: ours to refuse"
        );
        assert!(
            (400..600).contains(&status),
            "{label} {p}: a served answer must be a refusal, got {status}"
        );
    }

    // And the two that would write are forwarded. `member_token` is a `team_user`, which grants
    // `invite_user`; `import_team` needs the admin.
    assert_eq!(
        served_by(
            &client,
            reqwest::Method::POST,
            &f.member_token,
            &invite_path(&f.team),
            "application/json",
            br#"["mmrstadmfwd@mmrs.invalid"]"#
        )
        .await
        .as_deref(),
        Some("go"),
        "an invite that would send mail is Go's"
    );
    let slack = concat!(
        "--X\r\nContent-Disposition: form-data; name=\"importFrom\"\r\n\r\nslack\r\n",
        "--X\r\nContent-Disposition: form-data; name=\"filesize\"\r\n\r\n6\r\n",
        "--X\r\nContent-Disposition: form-data; name=\"file\"; filename=\"e.zip\"\r\n\r\nhello\n\r\n",
        "--X--\r\n"
    );
    assert_eq!(
        served_by(
            &client,
            reqwest::Method::POST,
            &token,
            &import_path(&f.team),
            "multipart/form-data; boundary=X",
            slack.as_bytes()
        )
        .await
        .as_deref(),
        Some("go"),
        "`importFrom=slack` is Go's"
    );
}

// ---------------------------------------------------------------------------
// POST /api/v4/teams/{team_id}/import
// ---------------------------------------------------------------------------

/// One `multipart/form-data` part: the field name, an optional filename, and the value.
type Part<'a> = (&'a str, Option<&'a str>, &'a str);

/// A multipart body with the parts named, in Go's wire order.
fn multipart(parts: &[Part<'_>]) -> Vec<u8> {
    let mut body = String::new();
    for (name, filename, value) in parts {
        body.push_str("--MMRSB\r\nContent-Disposition: form-data; name=\"");
        body.push_str(name);
        body.push('"');
        if let Some(filename) = filename {
            body.push_str("; filename=\"");
            body.push_str(filename);
            body.push('"');
        }
        body.push_str("\r\n\r\n");
        body.push_str(value);
        body.push_str("\r\n");
    }
    body.push_str("--MMRSB--\r\n");
    body.into_bytes()
}

/// The seven refusals, each with its own error id, and one of them is a 500.
#[tokio::test]
async fn the_import_gates_answer_what_go_answers() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    // 1. `team_id` is not id-shaped — and this precedes the multipart parse, so a JSON body here
    //    is a 400 and not the 500 below.
    let p = import_path("abc");
    let ((go_status, go), (rs_status, rs)) = post_both(&client, &token, &p, b"{}").await;
    assert_eq!(go_status, 400, "{p}: {}", String::from_utf8_lossy(&go));
    assert_eq!(rs_status, go_status, "{p}");
    let parsed = assert_error_bodies_match_except_known_gaps(&go, &rs, &p);
    assert_eq!(
        parsed["id"], "api.context.invalid_url_param.app_error",
        "{p}"
    );

    // 2. `import_team` is not in `team_user`, so a plain member is refused before the parse.
    let p = import_path(&f.team);
    let ((go_status, go), (rs_status, rs)) = post_both(&client, &f.member_token, &p, b"{}").await;
    assert_eq!(go_status, 403, "{p}: {}", String::from_utf8_lossy(&go));
    assert_eq!(rs_status, go_status, "{p}");
    let parsed = assert_error_bodies_match_except_known_gaps(&go, &rs, &p);
    assert_eq!(parsed["id"], "api.context.permissions.app_error", "{p}");

    // 3. Not multipart at all — a **500** for a malformed client body, which is Go's answer.
    let ((go_status, go), (rs_status, rs)) = post_both(&client, &token, &p, b"{}").await;
    assert_eq!(go_status, 500, "{p}: {}", String::from_utf8_lossy(&go));
    assert_eq!(rs_status, go_status, "{p}");
    let parsed = assert_error_bodies_match_except_known_gaps(&go, &rs, &p);
    assert_eq!(parsed["id"], "api.team.import_team.parse.app_error", "{p}");

    // 4-8. The field gates, each with its own id.
    let zip = ("file", Some("export.zip"), "hello");
    let cases: [(Vec<Part<'_>>, u16, &str); 5] = [
        (
            vec![("filesize", None, "6"), zip],
            400,
            "api.team.import_team.no_import_from.app_error",
        ),
        (
            vec![("importFrom", None, "slack"), zip],
            400,
            "api.team.import_team.unavailable.app_error",
        ),
        (
            vec![
                ("importFrom", None, "slack"),
                ("filesize", None, "abc"),
                zip,
            ],
            400,
            "api.team.import_team.integer.app_error",
        ),
        (
            vec![("importFrom", None, "slack"), ("filesize", None, "6")],
            400,
            "api.team.import_team.no_file.app_error",
        ),
        (
            vec![
                ("importFrom", None, "elsewhere"),
                ("filesize", None, "6"),
                zip,
            ],
            400,
            "api.team.import_team.unknown_import_from.app_error",
        ),
    ];
    for (parts, status, id) in cases {
        let body = multipart(&parts);
        let ((go_status, go), (rs_status, rs)) = send_both_raw(
            &client,
            reqwest::Method::POST,
            &token,
            &p,
            "multipart/form-data; boundary=MMRSB",
            &body,
        )
        .await;
        assert_eq!(go_status, status, "{id}: {}", String::from_utf8_lossy(&go));
        assert_eq!(rs_status, go_status, "{id}");
        let parsed = assert_error_bodies_match_except_known_gaps(&go, &rs, &p);
        assert_eq!(parsed["id"], id, "{id}");
    }
}

/// `importFrom=slack` is handed over, and Go's own answer for a body that is not a zip comes back
/// through the proxy unchanged — the evidence that the hand-over carries the multipart body intact.
#[tokio::test]
async fn slack_forwards_with_the_body_intact() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let body = multipart(&[
        ("importFrom", None, "slack"),
        ("filesize", None, "6"),
        ("file", Some("export.zip"), "hello"),
    ]);
    let p = import_path(&f.team);
    let ((go_status, go), (rs_status, rs)) = send_both_raw(
        &client,
        reqwest::Method::POST,
        &token,
        &p,
        "multipart/form-data; boundary=MMRSB",
        &body,
    )
    .await;
    assert_eq!(go_status, 400, "{p}: {}", String::from_utf8_lossy(&go));
    assert_eq!(rs_status, go_status, "{p}");
    // **Not `assert_error_bodies_match_except_known_gaps`.** This answer is Go's own, proxied
    // through us, so its `message` is Go's translated string rather than the raw id that helper
    // expects of a locally served error ([D-092]).
    let go_body: serde_json::Value = serde_json::from_slice(&go).expect("Go's body is JSON");
    let rs_body: serde_json::Value = serde_json::from_slice(&rs).expect("our body is JSON");
    assert_eq!(
        go_body["id"], "api.slackimport.slack_import.zip.app_error",
        "{p}: the import got as far as opening the zip"
    );
    assert_eq!(
        rs_body["id"], go_body["id"],
        "{p}: the same answer came back through the proxy, so the body survived it"
    );
    assert_eq!(rs_body["message"], go_body["message"], "{p}");
    // `outsider_team` is only here to keep the fixture field used from this module.
    assert!(!f.outsider_team.is_empty());
}
